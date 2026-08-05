use std::{
    collections::VecDeque,
    fmt,
    sync::{Arc, Mutex},
    thread,
};

use crossbeam_channel::{Receiver, Sender, TrySendError, select};
use thiserror::Error;
use tokio::sync::{broadcast, oneshot};
use uuid::Uuid;

use super::{OutputTarget, PreparedAudio};

/// How a newly accepted item interacts with current playback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConcurrencyMode {
    /// Play immediately when idle, otherwise join the tail of the FIFO.
    Enqueue,
    /// Stop the active item and play this item next, retaining the FIFO.
    Interrupt,
}

/// The already-validated material that a backend will render.
#[derive(Debug)]
pub enum PlaybackSource {
    Audio(PreparedAudio),
    Speech(String),
}

/// A validated request submitted to the single playback actor.
#[derive(Debug)]
pub struct PlaybackJob {
    pub id: Uuid,
    pub source: PlaybackSource,
    pub gain: f32,
    pub output_target: OutputTarget,
}

impl PlaybackJob {
    pub fn audio(id: Uuid, source: PreparedAudio, gain: f32) -> Self {
        Self::audio_to(id, source, gain, OutputTarget::SystemDefault)
    }

    pub fn audio_to(
        id: Uuid,
        source: PreparedAudio,
        gain: f32,
        output_target: OutputTarget,
    ) -> Self {
        Self {
            id,
            source: PlaybackSource::Audio(source),
            gain,
            output_target,
        }
    }

    pub fn speech(id: Uuid, text: impl Into<String>, gain: f32) -> Self {
        Self::speech_to(id, text, gain, OutputTarget::SystemDefault)
    }

    pub fn speech_to(
        id: Uuid,
        text: impl Into<String>,
        gain: f32,
        output_target: OutputTarget,
    ) -> Self {
        Self {
            id,
            source: PlaybackSource::Speech(text.into()),
            gain,
            output_target,
        }
    }
}

/// A successful actor acceptance. It does not claim completion or audibility.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Acceptance {
    pub playback_id: Uuid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaybackState {
    Accepted,
    Playing,
    Completed,
    Interrupted,
    Failed,
}

/// Best-effort internal lifecycle feed for diagnostics and future history.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LifecycleEvent {
    pub playback_id: Uuid,
    pub state: PlaybackState,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum PlaybackError {
    #[error("the playback queue is full")]
    QueueFull,
    #[error("the playback actor command channel is busy")]
    ActorBusy,
    #[error("the playback actor is not available")]
    ActorClosed,
    #[error("playback backend error: {0}")]
    Backend(String),
    #[error("output target is unavailable: {0}")]
    OutputUnavailable(String),
    #[error("audio file could not be opened: {0}")]
    OpenFile(String),
    #[error("audio source is not a regular file")]
    NotRegularFile,
    #[error("audio format is not supported")]
    UnsupportedAudio,
    #[error("audio duration could not be determined")]
    DurationUnknown,
    #[error("audio duration exceeds the built-in maximum")]
    AudioTooLong,
}

/// Terminal information supplied by a backend after a successful start.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BackendCompletion {
    Completed,
    Failed(String),
}

/// A one-shot completion token handed to the active backend.
///
/// Sending runs on a backend callback/watcher thread, never an MCP or Tokio
/// worker thread. Completions use a separate channel so an inline stop callback
/// cannot deadlock behind the bounded command mailbox.
pub struct CompletionNotifier {
    playback_id: Uuid,
    tx: Option<Sender<CompletionMessage>>,
}

impl fmt::Debug for CompletionNotifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompletionNotifier")
            .field("playback_id", &self.playback_id)
            .finish_non_exhaustive()
    }
}

impl CompletionNotifier {
    pub fn playback_id(&self) -> Uuid {
        self.playback_id
    }

    pub fn complete(self) {
        self.send(BackendCompletion::Completed);
    }

    pub fn fail(self, error: impl Into<String>) {
        self.send(BackendCompletion::Failed(error.into()));
    }

    fn send(mut self, completion: BackendCompletion) {
        let Some(tx) = self.tx.take() else {
            return;
        };
        let _ = tx.send(CompletionMessage {
            playback_id: self.playback_id,
            completion,
        });
    }
}

impl Drop for CompletionNotifier {
    fn drop(&mut self) {
        let Some(tx) = self.tx.take() else {
            return;
        };
        let _ = tx.send(CompletionMessage {
            playback_id: self.playback_id,
            completion: BackendCompletion::Failed(
                "playback backend dropped its completion callback".into(),
            ),
        });
    }
}

/// Narrow seam around the platform-specific playback implementation.
///
/// The backend is constructed and retained on the actor thread, so this trait
/// intentionally does not require `Send`.
pub trait PlaybackBackend: 'static {
    fn start(
        &mut self,
        job: PlaybackJob,
        completion: CompletionNotifier,
    ) -> Result<(), PlaybackError>;

    /// Return only once active output has been asked to stop.
    fn stop(&mut self) -> Result<(), PlaybackError>;

    /// Release per-item backend state after a terminal callback.
    fn finished(&mut self) {}

    fn shutdown(&mut self) -> Result<(), PlaybackError> {
        self.stop()
    }
}

enum ActorMessage {
    Submit {
        job: PlaybackJob,
        mode: ConcurrencyMode,
        response: oneshot::Sender<Result<Acceptance, PlaybackError>>,
    },
    Shutdown {
        response: oneshot::Sender<Result<(), PlaybackError>>,
    },
}

struct CompletionMessage {
    playback_id: Uuid,
    completion: BackendCompletion,
}

struct HandleInner {
    tx: Sender<ActorMessage>,
    join: Mutex<Option<thread::JoinHandle<()>>>,
    events: broadcast::Sender<LifecycleEvent>,
}

impl Drop for HandleInner {
    fn drop(&mut self) {
        let (response, _ignored) = oneshot::channel();
        let _ = self.tx.try_send(ActorMessage::Shutdown { response });
    }
}

/// Cloneable async handle to a dedicated playback actor thread.
#[derive(Clone)]
pub struct PlaybackHandle {
    inner: Arc<HandleInner>,
}

impl PlaybackHandle {
    /// Start a playback actor and initialize its backend on that actor's thread.
    pub fn spawn<F, B>(
        maximum_queue_items: usize,
        backend_factory: F,
    ) -> Result<Self, PlaybackError>
    where
        F: FnOnce() -> Result<B, PlaybackError> + Send + 'static,
        B: PlaybackBackend,
    {
        // Queue commands, completions, and shutdown all share one bounded and
        // therefore globally ordered mailbox. Completion and control headroom
        // is deliberately separate from the user-visible pending FIFO bound.
        let mailbox_capacity = maximum_queue_items.saturating_add(4).max(4);
        let (tx, rx) = crossbeam_channel::bounded(mailbox_capacity);
        let (completion_tx, completion_rx) = crossbeam_channel::unbounded();
        let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);
        let (events, _) = broadcast::channel(mailbox_capacity.saturating_mul(4).max(16));
        let actor_events = events.clone();

        let join = thread::Builder::new()
            .name("agent-speak-playback".into())
            .spawn(move || match backend_factory() {
                Ok(backend) => {
                    let _ = ready_tx.send(Ok(()));
                    Actor::new(backend, maximum_queue_items, completion_tx, actor_events)
                        .run(rx, completion_rx);
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                }
            })
            .map_err(|error| PlaybackError::Backend(error.to_string()))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                inner: Arc::new(HandleInner {
                    tx,
                    join: Mutex::new(Some(join)),
                    events,
                }),
            }),
            Ok(Err(error)) => {
                let _ = join.join();
                Err(error)
            }
            Err(_) => {
                let _ = join.join();
                Err(PlaybackError::ActorClosed)
            }
        }
    }

    /// Submit a validated job and wait only for actor acceptance.
    pub async fn submit(
        &self,
        job: PlaybackJob,
        mode: ConcurrencyMode,
    ) -> Result<Acceptance, PlaybackError> {
        let (response, response_rx) = oneshot::channel();
        self.inner
            .tx
            .try_send(ActorMessage::Submit {
                job,
                mode,
                response,
            })
            .map_err(map_try_send_error)?;
        response_rx.await.map_err(|_| PlaybackError::ActorClosed)?
    }

    /// Subscribe to best-effort lifecycle records. Slow receivers may lag.
    pub fn subscribe(&self) -> broadcast::Receiver<LifecycleEvent> {
        self.inner.events.subscribe()
    }

    /// Stop active playback, discard pending jobs, and release backend state.
    pub async fn shutdown(&self) -> Result<(), PlaybackError> {
        let (response, response_rx) = oneshot::channel();
        self.inner
            .tx
            .try_send(ActorMessage::Shutdown { response })
            .map_err(map_try_send_error)?;
        let result = response_rx.await.map_err(|_| PlaybackError::ActorClosed)?;

        if let Some(join) = self
            .inner
            .join
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let _ = join.join();
        }
        result
    }
}

fn map_try_send_error(error: TrySendError<ActorMessage>) -> PlaybackError {
    match error {
        TrySendError::Full(_) => PlaybackError::ActorBusy,
        TrySendError::Disconnected(_) => PlaybackError::ActorClosed,
    }
}

struct Active {
    playback_id: Uuid,
}

struct Actor<B> {
    backend: B,
    maximum_queue_items: usize,
    completion_tx: Sender<CompletionMessage>,
    events: broadcast::Sender<LifecycleEvent>,
    active: Option<Active>,
    pending: VecDeque<PlaybackJob>,
    unhealthy: bool,
}

impl<B: PlaybackBackend> Actor<B> {
    fn new(
        backend: B,
        maximum_queue_items: usize,
        completion_tx: Sender<CompletionMessage>,
        events: broadcast::Sender<LifecycleEvent>,
    ) -> Self {
        Self {
            backend,
            maximum_queue_items,
            completion_tx,
            events,
            active: None,
            pending: VecDeque::new(),
            unhealthy: false,
        }
    }

    fn run(mut self, rx: Receiver<ActorMessage>, completions: Receiver<CompletionMessage>) {
        let mut shutdown_requested = false;
        loop {
            select! {
                recv(rx) -> message => match message {
                    Ok(ActorMessage::Submit { job, mode, response }) => {
                        self.submit(job, mode, response);
                    }
                    Ok(ActorMessage::Shutdown { response }) => {
                        let result = self.do_shutdown();
                        let _ = response.send(result);
                        shutdown_requested = true;
                        break;
                    }
                    Err(_) => break,
                },
                recv(completions) -> completion => if let Ok(completion) = completion {
                    self.finish(completion.playback_id, completion.completion);
                }
            }
        }
        if !shutdown_requested {
            let _ = self.do_shutdown();
        }
    }

    fn submit(
        &mut self,
        job: PlaybackJob,
        mode: ConcurrencyMode,
        response: oneshot::Sender<Result<Acceptance, PlaybackError>>,
    ) {
        let playback_id = job.id;
        if self.unhealthy {
            let _ = response.send(Err(PlaybackError::Backend(
                "playback backend is unhealthy after a failed stop".into(),
            )));
            return;
        }
        let result = match mode {
            ConcurrencyMode::Enqueue if self.active.is_some() => {
                if self.pending.len() >= self.maximum_queue_items {
                    Err(PlaybackError::QueueFull)
                } else {
                    self.pending.push_back(job);
                    self.emit(playback_id, PlaybackState::Accepted, None);
                    Ok(Acceptance { playback_id })
                }
            }
            ConcurrencyMode::Interrupt if self.active.is_some() => {
                if let Err(error) = self.backend.stop() {
                    // Starting anything else could overlap output whose stop
                    // was never confirmed. Keep the FIFO paused until restart.
                    self.unhealthy = true;
                    Err(error)
                } else {
                    let interrupted = self.active.take().expect("active checked above");
                    self.emit(interrupted.playback_id, PlaybackState::Interrupted, None);
                    let result = self.start_now(job, false);
                    if result.is_err() {
                        self.advance_queue();
                    }
                    result
                }
            }
            ConcurrencyMode::Enqueue | ConcurrencyMode::Interrupt => self.start_now(job, false),
        };
        let _ = response.send(result);
    }

    fn start_now(
        &mut self,
        job: PlaybackJob,
        previously_accepted: bool,
    ) -> Result<Acceptance, PlaybackError> {
        let playback_id = job.id;
        let completion = CompletionNotifier {
            playback_id,
            tx: Some(self.completion_tx.clone()),
        };
        match self.backend.start(job, completion) {
            Ok(()) => {
                self.active = Some(Active { playback_id });
                if !previously_accepted {
                    self.emit(playback_id, PlaybackState::Accepted, None);
                }
                self.emit(playback_id, PlaybackState::Playing, None);
                Ok(Acceptance { playback_id })
            }
            Err(error) => {
                if previously_accepted {
                    self.emit(playback_id, PlaybackState::Failed, Some(error.to_string()));
                }
                Err(error)
            }
        }
    }

    fn finish(&mut self, playback_id: Uuid, completion: BackendCompletion) {
        if self.active.as_ref().map(|active| active.playback_id) != Some(playback_id) {
            // A completion that races with a confirmed interrupt belongs to the
            // old item and must not advance the replacement.
            return;
        }

        self.active = None;
        self.backend.finished();
        match completion {
            BackendCompletion::Completed => {
                self.emit(playback_id, PlaybackState::Completed, None);
            }
            BackendCompletion::Failed(error) => {
                self.emit(playback_id, PlaybackState::Failed, Some(error));
            }
        }
        if !self.unhealthy {
            self.advance_queue();
        }
    }

    fn advance_queue(&mut self) {
        while self.active.is_none() {
            let Some(job) = self.pending.pop_front() else {
                return;
            };
            if self.start_now(job, true).is_ok() {
                return;
            }
        }
    }

    fn do_shutdown(&mut self) -> Result<(), PlaybackError> {
        let discarded: Vec<_> = self.pending.drain(..).map(|job| job.id).collect();
        for playback_id in discarded {
            self.emit(playback_id, PlaybackState::Interrupted, None);
        }
        let mut first_error = None;
        if let Some(active) = self.active.take() {
            if let Err(error) = self.backend.stop() {
                first_error = Some(error);
            }
            self.emit(active.playback_id, PlaybackState::Interrupted, None);
        }
        if let Err(error) = self.backend.shutdown()
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn emit(&self, playback_id: Uuid, state: PlaybackState, error: Option<String>) {
        let _ = self.events.send(LifecycleEvent {
            playback_id,
            state,
            error,
        });
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet},
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };

    use tokio::sync::Barrier;

    use super::*;

    #[derive(Default)]
    struct FakeState {
        started: Vec<Uuid>,
        stopped: Vec<Uuid>,
        active: Option<Uuid>,
        completions: HashMap<Uuid, CompletionNotifier>,
        fail_on_start: HashSet<Uuid>,
        fail_stop: bool,
        shutdowns: usize,
    }

    #[derive(Clone, Default)]
    struct FakeControl(Arc<Mutex<FakeState>>);

    impl FakeControl {
        fn complete(&self, id: Uuid) {
            let completion = self
                .0
                .lock()
                .unwrap()
                .completions
                .remove(&id)
                .expect("missing completion");
            completion.complete();
        }

        fn fail(&self, id: Uuid) {
            let completion = self
                .0
                .lock()
                .unwrap()
                .completions
                .remove(&id)
                .expect("missing completion");
            completion.fail("simulated device loss");
        }

        fn lose_callback(&self, id: Uuid) {
            let completion = self
                .0
                .lock()
                .unwrap()
                .completions
                .remove(&id)
                .expect("missing completion");
            drop(completion);
        }

        fn started(&self) -> Vec<Uuid> {
            self.0.lock().unwrap().started.clone()
        }

        async fn wait_started(&self, count: usize) {
            let deadline = Instant::now() + Duration::from_secs(1);
            while self.started().len() < count {
                assert!(Instant::now() < deadline, "playback actor timed out");
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
    }

    struct FakeBackend(FakeControl);

    impl PlaybackBackend for FakeBackend {
        fn start(
            &mut self,
            job: PlaybackJob,
            completion: CompletionNotifier,
        ) -> Result<(), PlaybackError> {
            let mut state = self.0.0.lock().unwrap();
            if state.fail_on_start.remove(&job.id) {
                return Err(PlaybackError::Backend("simulated start failure".into()));
            }
            assert!(state.active.is_none(), "fake backend observed overlap");
            state.active = Some(job.id);
            state.started.push(job.id);
            state.completions.insert(job.id, completion);
            Ok(())
        }

        fn stop(&mut self) -> Result<(), PlaybackError> {
            let mut state = self.0.0.lock().unwrap();
            if state.fail_stop {
                return Err(PlaybackError::Backend("simulated stop failure".into()));
            }
            if let Some(id) = state.active.take() {
                state.stopped.push(id);
                state.completions.remove(&id);
            }
            Ok(())
        }

        fn finished(&mut self) {
            self.0.0.lock().unwrap().active = None;
        }

        fn shutdown(&mut self) -> Result<(), PlaybackError> {
            self.stop()?;
            self.0.0.lock().unwrap().shutdowns += 1;
            Ok(())
        }
    }

    fn setup(maximum_queue_items: usize) -> (PlaybackHandle, FakeControl) {
        let control = FakeControl::default();
        let backend_control = control.clone();
        let handle = PlaybackHandle::spawn(maximum_queue_items, move || {
            Ok(FakeBackend(backend_control))
        })
        .unwrap();
        (handle, control)
    }

    fn job(id: Uuid) -> PlaybackJob {
        PlaybackJob::speech(id, "test", 0.4)
    }

    #[tokio::test]
    async fn starts_immediately_when_idle() {
        let (handle, control) = setup(2);
        let id = Uuid::new_v4();

        assert_eq!(
            handle.submit(job(id), ConcurrencyMode::Enqueue).await,
            Ok(Acceptance { playback_id: id })
        );
        assert_eq!(control.started(), vec![id]);
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn enqueued_items_play_in_fifo_order() {
        let (handle, control) = setup(3);
        let ids: Vec<_> = (0..3).map(|_| Uuid::new_v4()).collect();
        for id in &ids {
            handle
                .submit(job(*id), ConcurrencyMode::Enqueue)
                .await
                .unwrap();
        }

        assert_eq!(control.started(), ids[..1]);
        control.complete(ids[0]);
        control.wait_started(2).await;
        control.complete(ids[1]);
        control.wait_started(3).await;
        assert_eq!(control.started(), ids);
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn rejects_when_pending_fifo_is_full() {
        let (handle, _control) = setup(1);
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        handle
            .submit(job(a), ConcurrencyMode::Enqueue)
            .await
            .unwrap();
        handle
            .submit(job(b), ConcurrencyMode::Enqueue)
            .await
            .unwrap();

        assert_eq!(
            handle.submit(job(c), ConcurrencyMode::Enqueue).await,
            Err(PlaybackError::QueueFull)
        );
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn interrupt_stops_active_and_preserves_pending_fifo() {
        let (handle, control) = setup(3);
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        let interrupt = Uuid::new_v4();
        for id in [a, b, c] {
            handle
                .submit(job(id), ConcurrencyMode::Enqueue)
                .await
                .unwrap();
        }

        handle
            .submit(job(interrupt), ConcurrencyMode::Interrupt)
            .await
            .unwrap();
        assert_eq!(control.started(), vec![a, interrupt]);
        assert_eq!(control.0.lock().unwrap().stopped, vec![a]);

        control.complete(interrupt);
        control.wait_started(3).await;
        control.complete(b);
        control.wait_started(4).await;
        assert_eq!(control.started(), vec![a, interrupt, b, c]);
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn backend_failure_advances_pending_fifo() {
        let (handle, control) = setup(2);
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        handle
            .submit(job(a), ConcurrencyMode::Enqueue)
            .await
            .unwrap();
        handle
            .submit(job(b), ConcurrencyMode::Enqueue)
            .await
            .unwrap();

        control.fail(a);
        control.wait_started(2).await;
        assert_eq!(control.started(), vec![a, b]);
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn dropped_completion_callback_fails_item_and_advances_fifo() {
        let (handle, control) = setup(2);
        let mut events = handle.subscribe();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        handle
            .submit(job(first), ConcurrencyMode::Enqueue)
            .await
            .unwrap();
        handle
            .submit(job(second), ConcurrencyMode::Enqueue)
            .await
            .unwrap();

        control.lose_callback(first);
        control.wait_started(2).await;
        assert_eq!(control.started(), vec![first, second]);

        let failed = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let event = events.recv().await.unwrap();
                if event.playback_id == first && event.state == PlaybackState::Failed {
                    break event;
                }
            }
        })
        .await
        .expect("callback-loss lifecycle event timed out");
        assert_eq!(
            failed.error.as_deref(),
            Some("playback backend dropped its completion callback")
        );
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn backend_panic_is_contained_to_actor_thread() {
        struct PanicBackend;

        impl PlaybackBackend for PanicBackend {
            fn start(
                &mut self,
                _job: PlaybackJob,
                _completion: CompletionNotifier,
            ) -> Result<(), PlaybackError> {
                panic!("simulated backend panic");
            }

            fn stop(&mut self) -> Result<(), PlaybackError> {
                Ok(())
            }
        }

        let handle = PlaybackHandle::spawn(1, || Ok(PanicBackend)).unwrap();
        assert_eq!(
            handle
                .submit(job(Uuid::new_v4()), ConcurrencyMode::Enqueue)
                .await,
            Err(PlaybackError::ActorClosed)
        );
        assert!(matches!(
            handle
                .submit(job(Uuid::new_v4()), ConcurrencyMode::Enqueue)
                .await,
            Err(PlaybackError::ActorClosed)
        ));
    }

    #[test]
    fn backend_factory_panic_is_reported_as_actor_closed() {
        let result = PlaybackHandle::spawn::<_, FakeBackend>(1, || {
            panic!("simulated backend initialization panic")
        });
        assert!(matches!(result, Err(PlaybackError::ActorClosed)));
    }

    #[tokio::test]
    async fn failed_interrupt_start_restores_pending_fifo_progress() {
        let (handle, control) = setup(2);
        let active = Uuid::new_v4();
        let queued = Uuid::new_v4();
        let interrupt = Uuid::new_v4();
        handle
            .submit(job(active), ConcurrencyMode::Enqueue)
            .await
            .unwrap();
        handle
            .submit(job(queued), ConcurrencyMode::Enqueue)
            .await
            .unwrap();
        control.0.lock().unwrap().fail_on_start.insert(interrupt);

        assert!(matches!(
            handle
                .submit(job(interrupt), ConcurrencyMode::Interrupt)
                .await,
            Err(PlaybackError::Backend(_))
        ));
        control.wait_started(2).await;
        assert_eq!(control.started(), vec![active, queued]);
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn failed_stop_marks_backend_unhealthy_and_pauses_fifo() {
        let (handle, control) = setup(2);
        let active = Uuid::new_v4();
        let queued = Uuid::new_v4();
        handle
            .submit(job(active), ConcurrencyMode::Enqueue)
            .await
            .unwrap();
        handle
            .submit(job(queued), ConcurrencyMode::Enqueue)
            .await
            .unwrap();
        control.0.lock().unwrap().fail_stop = true;

        assert!(matches!(
            handle
                .submit(job(Uuid::new_v4()), ConcurrencyMode::Interrupt)
                .await,
            Err(PlaybackError::Backend(_))
        ));
        control.complete(active);
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(control.started(), vec![active]);
        assert!(matches!(
            handle
                .submit(job(Uuid::new_v4()), ConcurrencyMode::Enqueue)
                .await,
            Err(PlaybackError::Backend(_))
        ));
        assert!(handle.shutdown().await.is_err());
    }

    #[tokio::test]
    async fn concurrent_calls_follow_actor_acceptance_order() {
        let (handle, control) = setup(16);
        let mut events = handle.subscribe();
        let barrier = Arc::new(Barrier::new(9));
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let handle = handle.clone();
            let barrier = barrier.clone();
            tasks.push(tokio::spawn(async move {
                let id = Uuid::new_v4();
                barrier.wait().await;
                handle
                    .submit(job(id), ConcurrencyMode::Enqueue)
                    .await
                    .unwrap();
                id
            }));
        }
        barrier.wait().await;
        for task in tasks {
            task.await.unwrap();
        }

        let mut accepted = Vec::new();
        while accepted.len() < 8 {
            let event = events.recv().await.unwrap();
            if event.state == PlaybackState::Accepted {
                accepted.push(event.playback_id);
            }
        }
        for index in 0..7 {
            control.wait_started(index + 1).await;
            let active = control.started()[index];
            control.complete(active);
        }
        control.wait_started(8).await;
        assert_eq!(control.started(), accepted);
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_stops_active_and_discards_pending() {
        let (handle, control) = setup(2);
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        handle
            .submit(job(a), ConcurrencyMode::Enqueue)
            .await
            .unwrap();
        handle
            .submit(job(b), ConcurrencyMode::Enqueue)
            .await
            .unwrap();

        handle.shutdown().await.unwrap();
        let state = control.0.lock().unwrap();
        assert_eq!(state.started, vec![a]);
        assert_eq!(state.stopped, vec![a]);
        assert_eq!(state.shutdowns, 1);
    }
}
