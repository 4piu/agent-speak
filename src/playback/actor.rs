use std::{
    collections::{HashMap, VecDeque},
    fmt,
    sync::{Arc, Mutex},
    thread,
};

use crossbeam_channel::{Receiver, Sender, TrySendError, select};
use serde_json::{Map, Value};
use thiserror::Error;
use tokio::sync::{broadcast, oneshot};
use uuid::Uuid;

use super::{OutputTarget, PreparedAudio};

/// Most-recent terminal playback results retained for status inspection.
pub const PLAYBACK_STATUS_RETENTION_ITEMS: usize = 256;

/// Default capacity for internal/test callers that do not supply policy.
/// MCP startup passes the validated profile limit explicitly.
const DEFAULT_INTERNAL_MIX_STREAM_CAPACITY: usize = 2;

/// How a newly accepted item interacts with current playback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConcurrencyMode {
    /// Play immediately when idle, otherwise join the tail of the FIFO.
    Enqueue,
    /// Stop every active item and play this item next, retaining the FIFO.
    Interrupt,
    /// Start beside active playback when a configured stream slot is available.
    Mix,
}

/// The already-validated material that a backend will render.
#[derive(Debug)]
pub enum PlaybackSource {
    Audio(PreparedAudio),
    Speech {
        text: String,
        utterance_options: Map<String, Value>,
    },
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
        Self::speech_with_options_to(id, text, Map::new(), gain, output_target)
    }

    pub fn speech_with_options_to(
        id: Uuid,
        text: impl Into<String>,
        utterance_options: Map<String, Value>,
        gain: f32,
        output_target: OutputTarget,
    ) -> Self {
        Self {
            id,
            source: PlaybackSource::Speech {
                text: text.into(),
                utterance_options,
            },
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

/// Result of asking the actor to cancel one accepted playback ID.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cancellation {
    pub playback_id: Uuid,
    pub state: PlaybackState,
    pub cancelled: bool,
}

/// Result of stopping every active or queued item owned by this actor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EmergencyStop {
    pub interrupted_items: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaybackState {
    Accepted,
    Playing,
    Completed,
    Interrupted,
    Failed,
}

impl PlaybackState {
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Interrupted | Self::Failed)
    }
}

/// Current or retained terminal state for one actor-accepted playback ID.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlaybackStatus {
    pub playback_id: Uuid,
    pub state: PlaybackState,
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

    /// Retire this callback without manufacturing another terminal event.
    /// Backends use this only after establishing a synchronous terminal path.
    pub(crate) fn discard(mut self) {
        self.tx.take();
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

    /// Return only once this active output has been asked to stop.
    fn stop(&mut self, playback_id: Uuid) -> Result<(), PlaybackError>;

    /// Release per-item backend state after a terminal callback.
    fn finished(&mut self, _playback_id: Uuid) {}

    fn shutdown(&mut self) -> Result<(), PlaybackError> {
        Ok(())
    }
}

enum ActorMessage {
    Submit {
        job: PlaybackJob,
        mode: ConcurrencyMode,
        response: oneshot::Sender<Result<Acceptance, PlaybackError>>,
    },
    GetStatus {
        playback_id: Uuid,
        response: oneshot::Sender<Option<PlaybackStatus>>,
    },
    Cancel {
        playback_id: Uuid,
        response: oneshot::Sender<Result<Option<Cancellation>, PlaybackError>>,
    },
    GetSnapshot {
        response: oneshot::Sender<Vec<PlaybackStatus>>,
    },
    EmergencyStop {
        response: oneshot::Sender<Result<EmergencyStop, PlaybackError>>,
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
        Self::spawn_with_active_capacity(
            maximum_queue_items,
            DEFAULT_INTERNAL_MIX_STREAM_CAPACITY,
            backend_factory,
        )
    }

    pub(crate) fn spawn_with_active_capacity<F, B>(
        maximum_queue_items: usize,
        maximum_active_items: usize,
        backend_factory: F,
    ) -> Result<Self, PlaybackError>
    where
        F: FnOnce() -> Result<B, PlaybackError> + Send + 'static,
        B: PlaybackBackend,
    {
        Self::spawn_with_metadata_and_active_capacity(
            maximum_queue_items,
            maximum_active_items,
            move || backend_factory().map(|backend| (backend, ())),
        )
        .map(|(handle, ())| handle)
    }

    /// Start an actor while returning immutable metadata produced alongside
    /// its backend initialization.
    pub fn spawn_with_metadata<F, B, M>(
        maximum_queue_items: usize,
        backend_factory: F,
    ) -> Result<(Self, M), PlaybackError>
    where
        F: FnOnce() -> Result<(B, M), PlaybackError> + Send + 'static,
        B: PlaybackBackend,
        M: Send + 'static,
    {
        Self::spawn_with_metadata_and_active_capacity(
            maximum_queue_items,
            DEFAULT_INTERNAL_MIX_STREAM_CAPACITY,
            backend_factory,
        )
    }

    pub(crate) fn spawn_with_metadata_and_active_capacity<F, B, M>(
        maximum_queue_items: usize,
        maximum_active_items: usize,
        backend_factory: F,
    ) -> Result<(Self, M), PlaybackError>
    where
        F: FnOnce() -> Result<(B, M), PlaybackError> + Send + 'static,
        B: PlaybackBackend,
        M: Send + 'static,
    {
        if maximum_active_items == 0 {
            return Err(PlaybackError::Backend(
                "playback active-stream capacity must be positive".into(),
            ));
        }
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
                Ok((backend, metadata)) => {
                    let _ = ready_tx.send(Ok(metadata));
                    Actor::new(
                        backend,
                        maximum_queue_items,
                        maximum_active_items,
                        completion_tx,
                        actor_events,
                    )
                    .run(rx, completion_rx);
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                }
            })
            .map_err(|error| PlaybackError::Backend(error.to_string()))?;

        match ready_rx.recv() {
            Ok(Ok(metadata)) => Ok((
                Self {
                    inner: Arc::new(HandleInner {
                        tx,
                        join: Mutex::new(Some(join)),
                        events,
                    }),
                },
                metadata,
            )),
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

    /// Return the current or retained terminal state for an accepted ID.
    pub async fn status(&self, playback_id: Uuid) -> Result<Option<PlaybackStatus>, PlaybackError> {
        let (response, response_rx) = oneshot::channel();
        self.inner
            .tx
            .try_send(ActorMessage::GetStatus {
                playback_id,
                response,
            })
            .map_err(map_try_send_error)?;
        response_rx.await.map_err(|_| PlaybackError::ActorClosed)
    }

    /// Stop an active item or remove a queued item by accepted playback ID.
    pub async fn cancel(&self, playback_id: Uuid) -> Result<Option<Cancellation>, PlaybackError> {
        let (response, response_rx) = oneshot::channel();
        self.inner
            .tx
            .try_send(ActorMessage::Cancel {
                playback_id,
                response,
            })
            .map_err(map_try_send_error)?;
        response_rx.await.map_err(|_| PlaybackError::ActorClosed)?
    }

    /// Return every in-flight and retained terminal status, newest first.
    pub async fn snapshot(&self) -> Result<Vec<PlaybackStatus>, PlaybackError> {
        let (response, response_rx) = oneshot::channel();
        self.inner
            .tx
            .try_send(ActorMessage::GetSnapshot { response })
            .map_err(map_try_send_error)?;
        response_rx.await.map_err(|_| PlaybackError::ActorClosed)
    }

    /// Stop active playback and discard every queued item.
    pub async fn emergency_stop(&self) -> Result<EmergencyStop, PlaybackError> {
        let (response, response_rx) = oneshot::channel();
        self.inner
            .tx
            .try_send(ActorMessage::EmergencyStop { response })
            .map_err(map_try_send_error)?;
        response_rx.await.map_err(|_| PlaybackError::ActorClosed)?
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

struct Active;

struct Pending {
    job: PlaybackJob,
    mode: ConcurrencyMode,
}

struct Actor<B> {
    backend: B,
    maximum_queue_items: usize,
    maximum_active_items: usize,
    completion_tx: Sender<CompletionMessage>,
    events: broadcast::Sender<LifecycleEvent>,
    active: HashMap<Uuid, Active>,
    pending: VecDeque<Pending>,
    statuses: HashMap<Uuid, PlaybackStatus>,
    terminal_statuses: VecDeque<Uuid>,
    status_order: VecDeque<Uuid>,
    unhealthy: bool,
}

impl<B: PlaybackBackend> Actor<B> {
    fn new(
        backend: B,
        maximum_queue_items: usize,
        maximum_active_items: usize,
        completion_tx: Sender<CompletionMessage>,
        events: broadcast::Sender<LifecycleEvent>,
    ) -> Self {
        Self {
            backend,
            maximum_queue_items,
            maximum_active_items,
            completion_tx,
            events,
            active: HashMap::new(),
            pending: VecDeque::new(),
            statuses: HashMap::new(),
            terminal_statuses: VecDeque::new(),
            status_order: VecDeque::new(),
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
                    Ok(ActorMessage::GetStatus { playback_id, response }) => {
                        let _ = response.send(self.statuses.get(&playback_id).copied());
                    }
                    Ok(ActorMessage::Cancel { playback_id, response }) => {
                        let _ = response.send(self.cancel(playback_id));
                    }
                    Ok(ActorMessage::GetSnapshot { response }) => {
                        let statuses = self
                            .status_order
                            .iter()
                            .rev()
                            .filter_map(|playback_id| self.statuses.get(playback_id).copied())
                            .collect();
                        let _ = response.send(statuses);
                    }
                    Ok(ActorMessage::EmergencyStop { response }) => {
                        let _ = response.send(self.emergency_stop());
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
            ConcurrencyMode::Enqueue if !self.active.is_empty() => {
                if self.pending.len() >= self.maximum_queue_items {
                    Err(PlaybackError::QueueFull)
                } else {
                    self.pending.push_back(Pending {
                        job,
                        mode: ConcurrencyMode::Enqueue,
                    });
                    self.emit(playback_id, PlaybackState::Accepted, None);
                    Ok(Acceptance { playback_id })
                }
            }
            ConcurrencyMode::Interrupt if !self.active.is_empty() => {
                let active = self.active.keys().copied().collect::<Vec<_>>();
                let mut stop_error = None;
                for playback_id in active {
                    if let Err(error) = self.backend.stop(playback_id) {
                        stop_error.get_or_insert(error);
                    } else {
                        self.active.remove(&playback_id);
                        self.emit(playback_id, PlaybackState::Interrupted, None);
                    }
                }
                if let Some(error) = stop_error {
                    // Starting anything else could overlap output whose stop
                    // was never confirmed. Keep the FIFO paused until restart.
                    self.unhealthy = true;
                    Err(error)
                } else {
                    let result = self.start_now(job, false);
                    if result.is_err() {
                        self.advance_queue();
                    }
                    result
                }
            }
            ConcurrencyMode::Mix
                if self.active.len() >= self.maximum_active_items || !self.pending.is_empty() =>
            {
                if self.pending.len() >= self.maximum_queue_items {
                    Err(PlaybackError::QueueFull)
                } else {
                    self.pending.push_back(Pending {
                        job,
                        mode: ConcurrencyMode::Mix,
                    });
                    self.emit(playback_id, PlaybackState::Accepted, None);
                    Ok(Acceptance { playback_id })
                }
            }
            ConcurrencyMode::Enqueue | ConcurrencyMode::Interrupt | ConcurrencyMode::Mix => {
                self.start_now(job, false)
            }
        };
        let _ = response.send(result);
    }

    fn cancel(&mut self, playback_id: Uuid) -> Result<Option<Cancellation>, PlaybackError> {
        if self.active.contains_key(&playback_id) {
            if let Err(error) = self.backend.stop(playback_id) {
                self.unhealthy = true;
                return Err(error);
            }
            self.active.remove(&playback_id);
            self.emit(playback_id, PlaybackState::Interrupted, None);
            if !self.unhealthy {
                self.advance_queue();
            }
            return Ok(Some(Cancellation {
                playback_id,
                state: PlaybackState::Interrupted,
                cancelled: true,
            }));
        }

        if let Some(position) = self
            .pending
            .iter()
            .position(|pending| pending.job.id == playback_id)
        {
            self.pending.remove(position);
            self.emit(playback_id, PlaybackState::Interrupted, None);
            return Ok(Some(Cancellation {
                playback_id,
                state: PlaybackState::Interrupted,
                cancelled: true,
            }));
        }

        match self.statuses.get(&playback_id).copied() {
            Some(status) if status.state.is_terminal() => Ok(Some(Cancellation {
                playback_id,
                state: status.state,
                cancelled: false,
            })),
            Some(_) => Err(PlaybackError::Backend(
                "playback status is inconsistent with actor state".into(),
            )),
            None => Ok(None),
        }
    }

    fn emergency_stop(&mut self) -> Result<EmergencyStop, PlaybackError> {
        let queued: Vec<_> = self
            .pending
            .drain(..)
            .map(|pending| pending.job.id)
            .collect();
        let mut interrupted_items = queued.len();
        for playback_id in queued {
            self.emit(playback_id, PlaybackState::Interrupted, None);
        }

        let active = self.active.keys().copied().collect::<Vec<_>>();
        let mut first_error = None;
        for playback_id in active {
            if let Err(error) = self.backend.stop(playback_id) {
                first_error.get_or_insert(error);
            } else {
                self.active.remove(&playback_id);
                self.emit(playback_id, PlaybackState::Interrupted, None);
                interrupted_items += 1;
            }
        }
        if let Some(error) = first_error {
            self.unhealthy = true;
            return Err(error);
        }

        Ok(EmergencyStop { interrupted_items })
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
                self.active.insert(playback_id, Active);
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
        if !self.active.contains_key(&playback_id) {
            // A completion that races with a confirmed interrupt belongs to the
            // old item and must not advance the replacement.
            return;
        }

        self.active.remove(&playback_id);
        self.backend.finished(playback_id);
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
        loop {
            let Some(next) = self.pending.front() else {
                return;
            };
            let can_start = if self.active.is_empty() {
                true
            } else {
                next.mode == ConcurrencyMode::Mix && self.active.len() < self.maximum_active_items
            };
            if !can_start {
                return;
            }
            let Pending { job, mode } = self.pending.pop_front().expect("front checked");
            let started = self.start_now(job, true).is_ok();
            if started && mode != ConcurrencyMode::Mix {
                return;
            }
        }
    }

    fn do_shutdown(&mut self) -> Result<(), PlaybackError> {
        let discarded: Vec<_> = self
            .pending
            .drain(..)
            .map(|pending| pending.job.id)
            .collect();
        for playback_id in discarded {
            self.emit(playback_id, PlaybackState::Interrupted, None);
        }
        let mut first_error = None;
        let active = self.active.keys().copied().collect::<Vec<_>>();
        for playback_id in active {
            if let Err(error) = self.backend.stop(playback_id)
                && first_error.is_none()
            {
                first_error = Some(error);
            }
            self.active.remove(&playback_id);
            self.emit(playback_id, PlaybackState::Interrupted, None);
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

    fn emit(&mut self, playback_id: Uuid, state: PlaybackState, error: Option<String>) {
        if !self.statuses.contains_key(&playback_id) {
            self.status_order.push_back(playback_id);
        }
        let was_terminal = self
            .statuses
            .get(&playback_id)
            .is_some_and(|status| status.state.is_terminal());
        self.statuses
            .insert(playback_id, PlaybackStatus { playback_id, state });
        if state.is_terminal() && !was_terminal {
            self.terminal_statuses.push_back(playback_id);
            while self.terminal_statuses.len() > PLAYBACK_STATUS_RETENTION_ITEMS {
                if let Some(expired) = self.terminal_statuses.pop_front() {
                    self.statuses.remove(&expired);
                    self.status_order
                        .retain(|playback_id| *playback_id != expired);
                }
            }
        }
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
        fn take_completion(&self, id: Uuid) -> CompletionNotifier {
            self.0
                .lock()
                .unwrap()
                .completions
                .remove(&id)
                .expect("missing completion")
        }

        fn complete(&self, id: Uuid) {
            self.take_completion(id).complete();
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

        fn stop(&mut self, playback_id: Uuid) -> Result<(), PlaybackError> {
            let mut state = self.0.0.lock().unwrap();
            if state.fail_stop {
                return Err(PlaybackError::Backend("simulated stop failure".into()));
            }
            if state.active == Some(playback_id) {
                state.active = None;
                state.stopped.push(playback_id);
                if let Some(completion) = state.completions.remove(&playback_id) {
                    completion.discard();
                }
            }
            Ok(())
        }

        fn finished(&mut self, playback_id: Uuid) {
            let mut state = self.0.0.lock().unwrap();
            if state.active == Some(playback_id) {
                state.active = None;
            }
        }

        fn shutdown(&mut self) -> Result<(), PlaybackError> {
            let mut state = self.0.0.lock().unwrap();
            state.shutdowns += 1;
            if state.fail_stop {
                return Err(PlaybackError::Backend("simulated stop failure".into()));
            }
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

    #[derive(Default)]
    struct MultiState {
        started: Vec<Uuid>,
        stopped: Vec<Uuid>,
        active: HashSet<Uuid>,
        completions: HashMap<Uuid, CompletionNotifier>,
    }

    #[derive(Clone, Default)]
    struct MultiControl(Arc<Mutex<MultiState>>);

    impl MultiControl {
        fn complete(&self, playback_id: Uuid) {
            self.0
                .lock()
                .unwrap()
                .completions
                .remove(&playback_id)
                .expect("missing completion")
                .complete();
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

    struct MultiBackend(MultiControl);

    impl PlaybackBackend for MultiBackend {
        fn start(
            &mut self,
            job: PlaybackJob,
            completion: CompletionNotifier,
        ) -> Result<(), PlaybackError> {
            let mut state = self.0.0.lock().unwrap();
            assert!(state.active.insert(job.id), "duplicate active playback ID");
            state.started.push(job.id);
            state.completions.insert(job.id, completion);
            Ok(())
        }

        fn stop(&mut self, playback_id: Uuid) -> Result<(), PlaybackError> {
            let mut state = self.0.0.lock().unwrap();
            if state.active.remove(&playback_id) {
                state.stopped.push(playback_id);
                if let Some(completion) = state.completions.remove(&playback_id) {
                    completion.discard();
                }
            }
            Ok(())
        }

        fn finished(&mut self, playback_id: Uuid) {
            self.0.0.lock().unwrap().active.remove(&playback_id);
        }
    }

    fn setup_multi(maximum_queue_items: usize) -> (PlaybackHandle, MultiControl) {
        setup_multi_with_capacity(maximum_queue_items, DEFAULT_INTERNAL_MIX_STREAM_CAPACITY)
    }

    fn setup_multi_with_capacity(
        maximum_queue_items: usize,
        maximum_active_items: usize,
    ) -> (PlaybackHandle, MultiControl) {
        let control = MultiControl::default();
        let backend_control = control.clone();
        let handle = PlaybackHandle::spawn_with_active_capacity(
            maximum_queue_items,
            maximum_active_items,
            move || Ok(MultiBackend(backend_control)),
        )
        .unwrap();
        (handle, control)
    }

    fn job(id: Uuid) -> PlaybackJob {
        PlaybackJob::speech(id, "test", 0.4)
    }

    async fn wait_status(
        handle: &PlaybackHandle,
        playback_id: Uuid,
        expected: PlaybackState,
    ) -> PlaybackStatus {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let Some(status) = handle.status(playback_id).await.unwrap()
                && status.state == expected
            {
                return status;
            }
            assert!(Instant::now() < deadline, "playback status timed out");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    #[tokio::test]
    async fn reports_accepted_playing_and_terminal_states() {
        let (handle, control) = setup(2);
        let active = Uuid::new_v4();
        let queued = Uuid::new_v4();
        let interrupt = Uuid::new_v4();

        assert_eq!(handle.status(Uuid::new_v4()).await.unwrap(), None);
        handle
            .submit(job(active), ConcurrencyMode::Enqueue)
            .await
            .unwrap();
        handle
            .submit(job(queued), ConcurrencyMode::Enqueue)
            .await
            .unwrap();
        assert_eq!(
            handle.status(active).await.unwrap(),
            Some(PlaybackStatus {
                playback_id: active,
                state: PlaybackState::Playing,
            })
        );
        assert_eq!(
            handle.status(queued).await.unwrap(),
            Some(PlaybackStatus {
                playback_id: queued,
                state: PlaybackState::Accepted,
            })
        );

        control.complete(active);
        wait_status(&handle, active, PlaybackState::Completed).await;
        wait_status(&handle, queued, PlaybackState::Playing).await;

        handle
            .submit(job(interrupt), ConcurrencyMode::Interrupt)
            .await
            .unwrap();
        wait_status(&handle, queued, PlaybackState::Interrupted).await;
        wait_status(&handle, interrupt, PlaybackState::Playing).await;

        control.fail(interrupt);
        wait_status(&handle, interrupt, PlaybackState::Failed).await;
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancels_queued_and_active_items_then_advances_fifo() {
        let (handle, control) = setup(3);
        let active = Uuid::new_v4();
        let removed = Uuid::new_v4();
        let next = Uuid::new_v4();
        for playback_id in [active, removed, next] {
            handle
                .submit(job(playback_id), ConcurrencyMode::Enqueue)
                .await
                .unwrap();
        }

        assert_eq!(
            handle.cancel(removed).await.unwrap(),
            Some(Cancellation {
                playback_id: removed,
                state: PlaybackState::Interrupted,
                cancelled: true,
            })
        );
        assert_eq!(
            handle.status(removed).await.unwrap().unwrap().state,
            PlaybackState::Interrupted
        );
        assert_eq!(control.started(), vec![active]);

        assert_eq!(
            handle.cancel(active).await.unwrap(),
            Some(Cancellation {
                playback_id: active,
                state: PlaybackState::Interrupted,
                cancelled: true,
            })
        );
        control.wait_started(2).await;
        assert_eq!(control.started(), vec![active, next]);
        assert_eq!(control.0.lock().unwrap().stopped, vec![active]);
        assert_eq!(
            handle.status(active).await.unwrap().unwrap().state,
            PlaybackState::Interrupted
        );
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_is_a_noop_for_terminal_and_unknown_ids() {
        let (handle, control) = setup(1);
        let completed = Uuid::new_v4();
        handle
            .submit(job(completed), ConcurrencyMode::Enqueue)
            .await
            .unwrap();
        control.complete(completed);
        wait_status(&handle, completed, PlaybackState::Completed).await;

        assert_eq!(
            handle.cancel(completed).await.unwrap(),
            Some(Cancellation {
                playback_id: completed,
                state: PlaybackState::Completed,
                cancelled: false,
            })
        );
        assert_eq!(handle.cancel(Uuid::new_v4()).await.unwrap(), None);
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_stop_failure_preserves_state_and_pauses_fifo() {
        let (handle, control) = setup(1);
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
            handle.cancel(active).await,
            Err(PlaybackError::Backend(_))
        ));
        assert_eq!(
            handle.status(active).await.unwrap().unwrap().state,
            PlaybackState::Playing
        );
        control.complete(active);
        wait_status(&handle, active, PlaybackState::Completed).await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(control.started(), vec![active]);
        assert_eq!(
            handle.status(queued).await.unwrap().unwrap().state,
            PlaybackState::Accepted
        );
        assert!(handle.shutdown().await.is_err());
    }

    #[tokio::test]
    async fn snapshot_is_newest_first_without_playback_content() {
        let (handle, control) = setup(2);
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

        assert_eq!(
            handle.snapshot().await.unwrap(),
            vec![
                PlaybackStatus {
                    playback_id: second,
                    state: PlaybackState::Accepted,
                },
                PlaybackStatus {
                    playback_id: first,
                    state: PlaybackState::Playing,
                },
            ]
        );
        control.complete(first);
        wait_status(&handle, second, PlaybackState::Playing).await;
        assert_eq!(
            handle.snapshot().await.unwrap()[0].state,
            PlaybackState::Playing
        );
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn emergency_stop_interrupts_active_and_every_queued_item() {
        let (handle, control) = setup(3);
        let ids = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
        for playback_id in ids {
            handle
                .submit(job(playback_id), ConcurrencyMode::Enqueue)
                .await
                .unwrap();
        }

        assert_eq!(
            handle.emergency_stop().await.unwrap(),
            EmergencyStop {
                interrupted_items: 3,
            }
        );
        assert_eq!(control.0.lock().unwrap().stopped, vec![ids[0]]);
        for playback_id in ids {
            assert_eq!(
                handle.status(playback_id).await.unwrap().unwrap().state,
                PlaybackState::Interrupted
            );
        }
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn emergency_stop_failure_discards_queue_but_preserves_active_state() {
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
            handle.emergency_stop().await,
            Err(PlaybackError::Backend(_))
        ));
        assert_eq!(
            handle.status(active).await.unwrap().unwrap().state,
            PlaybackState::Playing
        );
        assert_eq!(
            handle.status(queued).await.unwrap().unwrap().state,
            PlaybackState::Interrupted
        );
        assert!(handle.shutdown().await.is_err());
    }

    #[tokio::test]
    async fn completion_and_cancellation_race_to_one_terminal_state() {
        for _ in 0..32 {
            let (handle, control) = setup(1);
            let playback_id = Uuid::new_v4();
            handle
                .submit(job(playback_id), ConcurrencyMode::Enqueue)
                .await
                .unwrap();
            let completion = control.take_completion(playback_id);
            let cancel_handle = handle.clone();
            let cancel = tokio::spawn(async move { cancel_handle.cancel(playback_id).await });
            completion.complete();

            let cancellation = cancel.await.unwrap().unwrap().unwrap();
            let status = handle.status(playback_id).await.unwrap().unwrap();
            if cancellation.cancelled {
                assert_eq!(cancellation.state, PlaybackState::Interrupted);
                assert_eq!(status.state, PlaybackState::Interrupted);
            } else {
                assert_eq!(cancellation.state, PlaybackState::Completed);
                assert_eq!(status.state, PlaybackState::Completed);
            }
            handle.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn retains_only_the_newest_bounded_terminal_statuses() {
        let (handle, control) = setup(1);
        let mut playback_ids = Vec::new();
        for _ in 0..=PLAYBACK_STATUS_RETENTION_ITEMS {
            let playback_id = Uuid::new_v4();
            playback_ids.push(playback_id);
            handle
                .submit(job(playback_id), ConcurrencyMode::Enqueue)
                .await
                .unwrap();
            control.complete(playback_id);
            wait_status(&handle, playback_id, PlaybackState::Completed).await;
        }

        assert_eq!(handle.status(playback_ids[0]).await.unwrap(), None);
        assert_eq!(
            handle.status(playback_ids[1]).await.unwrap().unwrap().state,
            PlaybackState::Completed
        );
        assert_eq!(
            handle
                .status(*playback_ids.last().unwrap())
                .await
                .unwrap()
                .unwrap()
                .state,
            PlaybackState::Completed
        );
        handle.shutdown().await.unwrap();
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
    async fn mix_uses_two_internal_slots_and_advances_each_id_independently() {
        let (handle, control) = setup_multi(3);
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let third = Uuid::new_v4();

        for playback_id in [first, second, third] {
            handle
                .submit(job(playback_id), ConcurrencyMode::Mix)
                .await
                .unwrap();
        }

        assert_eq!(control.started(), vec![first, second]);
        assert_eq!(
            handle.status(third).await.unwrap().unwrap().state,
            PlaybackState::Accepted
        );

        control.complete(second);
        wait_status(&handle, second, PlaybackState::Completed).await;
        control.wait_started(3).await;
        assert_eq!(control.started(), vec![first, second, third]);
        assert_eq!(
            handle.status(first).await.unwrap().unwrap().state,
            PlaybackState::Playing
        );
        assert_eq!(
            handle.status(third).await.unwrap().unwrap().state,
            PlaybackState::Playing
        );

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn internal_mix_capacity_is_configurable_and_not_a_two_stream_ceiling() {
        let (handle, control) = setup_multi_with_capacity(2, 3);
        let ids = [
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        ];
        for playback_id in ids {
            handle
                .submit(job(playback_id), ConcurrencyMode::Mix)
                .await
                .unwrap();
        }

        assert_eq!(control.started(), ids[..3]);
        control.complete(ids[1]);
        control.wait_started(4).await;
        assert_eq!(control.started(), ids);
        handle.shutdown().await.unwrap();
    }

    #[test]
    fn zero_internal_mix_capacity_is_rejected() {
        let control = FakeControl::default();
        let result =
            PlaybackHandle::spawn_with_active_capacity(1, 0, move || Ok(FakeBackend(control)));
        assert!(
            matches!(result, Err(PlaybackError::Backend(message)) if message.contains("capacity"))
        );
    }

    #[tokio::test]
    async fn cancelling_one_mixed_item_preserves_its_peer_and_fills_the_slot() {
        let (handle, control) = setup_multi(2);
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let third = Uuid::new_v4();
        for playback_id in [first, second, third] {
            handle
                .submit(job(playback_id), ConcurrencyMode::Mix)
                .await
                .unwrap();
        }

        assert_eq!(
            handle.cancel(first).await.unwrap(),
            Some(Cancellation {
                playback_id: first,
                state: PlaybackState::Interrupted,
                cancelled: true,
            })
        );
        control.wait_started(3).await;
        {
            let state = control.0.lock().unwrap();
            assert_eq!(state.started, vec![first, second, third]);
            assert_eq!(state.stopped, vec![first]);
            assert_eq!(state.active, HashSet::from([second, third]));
        }
        assert_eq!(
            handle.status(second).await.unwrap().unwrap().state,
            PlaybackState::Playing
        );
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn enqueue_at_the_fifo_head_waits_for_every_mixed_item() {
        let (handle, control) = setup_multi(3);
        let mixed = [Uuid::new_v4(), Uuid::new_v4()];
        let enqueued = Uuid::new_v4();
        let later_mix = Uuid::new_v4();

        for playback_id in mixed {
            handle
                .submit(job(playback_id), ConcurrencyMode::Mix)
                .await
                .unwrap();
        }
        handle
            .submit(job(enqueued), ConcurrencyMode::Enqueue)
            .await
            .unwrap();
        handle
            .submit(job(later_mix), ConcurrencyMode::Mix)
            .await
            .unwrap();

        control.complete(mixed[0]);
        wait_status(&handle, mixed[0], PlaybackState::Completed).await;
        assert_eq!(control.started(), mixed);

        control.complete(mixed[1]);
        control.wait_started(3).await;
        assert_eq!(control.started(), vec![mixed[0], mixed[1], enqueued]);
        control.complete(enqueued);
        control.wait_started(4).await;
        assert_eq!(
            control.started(),
            vec![mixed[0], mixed[1], enqueued, later_mix]
        );

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn interrupt_stops_every_mixed_item_before_replacement() {
        let (handle, control) = setup_multi(2);
        let mixed = [Uuid::new_v4(), Uuid::new_v4()];
        let replacement = Uuid::new_v4();
        for playback_id in mixed {
            handle
                .submit(job(playback_id), ConcurrencyMode::Mix)
                .await
                .unwrap();
        }

        handle
            .submit(job(replacement), ConcurrencyMode::Interrupt)
            .await
            .unwrap();

        {
            let state = control.0.lock().unwrap();
            assert_eq!(state.started, vec![mixed[0], mixed[1], replacement]);
            assert_eq!(
                state.stopped.iter().copied().collect::<HashSet<_>>(),
                mixed.into()
            );
            assert_eq!(state.active, HashSet::from([replacement]));
        }
        for playback_id in mixed {
            assert_eq!(
                handle.status(playback_id).await.unwrap().unwrap().state,
                PlaybackState::Interrupted
            );
        }
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
                // Exercise thread unwinding without invoking the process-global
                // panic hook. The Rust test harness captures that hook's output,
                // which can block a deliberately panicking background thread on
                // some Linux runners before its response sender is dropped.
                std::panic::resume_unwind(Box::new("simulated backend panic"));
            }

            fn stop(&mut self, _playback_id: Uuid) -> Result<(), PlaybackError> {
                Ok(())
            }
        }

        let handle = PlaybackHandle::spawn(1, || Ok(PanicBackend)).unwrap();
        let first = tokio::time::timeout(
            Duration::from_secs(2),
            handle.submit(job(Uuid::new_v4()), ConcurrencyMode::Enqueue),
        )
        .await
        .expect("panicking backend did not close the actor promptly");
        assert_eq!(first, Err(PlaybackError::ActorClosed));

        // The response sender is dropped while the actor thread is unwinding,
        // just before its command receiver is dropped. Join that deliberately
        // panicked thread so this assertion cannot race those two events on a
        // heavily loaded test runner.
        let join = handle
            .inner
            .join
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .expect("actor thread join handle was missing");
        assert!(join.join().is_err());

        let second = handle
            .submit(job(Uuid::new_v4()), ConcurrencyMode::Enqueue)
            .await;
        assert!(matches!(second, Err(PlaybackError::ActorClosed)));
    }

    #[test]
    fn backend_factory_panic_is_reported_as_actor_closed() {
        let result = PlaybackHandle::spawn::<_, FakeBackend>(1, || {
            std::panic::resume_unwind(Box::new("simulated backend initialization panic"))
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
