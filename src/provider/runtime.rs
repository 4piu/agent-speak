use std::{
    path::PathBuf,
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, Sender, select};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::{
    config::TtsConfig,
    playback::{
        AudioAdapter, CompletionNotifier, OutputTarget, PlaybackError, PreparedAudio, RodioAudio,
        TtsAdapter, TtsCapabilities,
    },
};

use super::{
    CANCELLATION_GRACE_MS, MAX_AUDIO_BYTES, ProviderError, SessionKind,
    client::{
        Client, Frame, ensure_provider_directories, provider_directories, remote_error,
        write_request,
    },
    decoder::{DecodedMessage, EncodedDecoder, EncodedFormat},
    discover_provider,
};

const PREBUFFER_MS: u64 = 200;
const MAX_QUEUED_MS: u64 = 2_000;
const QUEUED_SEGMENT_MS: u64 = 100;

enum EncodedEvent {
    Provider(Frame),
    Decoder(DecodedMessage),
}

#[derive(Default)]
struct EncodedPlaybackState {
    sample_rate_hz: Option<u32>,
    channels: Option<u16>,
    pending: Vec<Vec<u8>>,
    pending_frames: u64,
    started: bool,
}

enum WorkerCommand {
    Speak {
        text: String,
        utterance_options: Map<String, Value>,
        gain: f32,
        target: OutputTarget,
        completion: CompletionNotifier,
    },
    Stop {
        playback_id: Uuid,
        response: Sender<Result<(), PlaybackError>>,
    },
    Finished(Uuid),
    Shutdown(Sender<Result<(), PlaybackError>>),
}

pub struct UtterPipeTts {
    commands: Sender<WorkerCommand>,
    join: Mutex<Option<thread::JoinHandle<()>>>,
    initialization: super::client::RuntimeInitialization,
}

impl UtterPipeTts {
    pub fn new(tts: TtsConfig, maximum_audio_seconds: u64) -> Result<Self, PlaybackError> {
        Self::new_with_audio(tts, maximum_audio_seconds, RodioAudio::new()?)
    }

    pub(crate) fn new_with_audio(
        tts: TtsConfig,
        maximum_audio_seconds: u64,
        audio: RodioAudio,
    ) -> Result<Self, PlaybackError> {
        let provider = tts
            .utterpipe()
            .ok_or_else(|| PlaybackError::Backend("UtterPipe provider is missing".into()))?;
        let slug = provider.provider.clone();
        let executable = discover_provider(&slug).map_err(playback_error)?;
        let (data, cache) = provider_directories(&slug).map_err(playback_error)?;
        ensure_provider_directories(&data, &cache).map_err(playback_error)?;
        let (commands, receiver) = crossbeam_channel::bounded(4);
        let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);
        let join = thread::Builder::new()
            .name(format!("utterpipe-{slug}-runtime"))
            .spawn(move || {
                let mut worker = Worker::new(
                    WorkerLocation {
                        slug,
                        executable,
                        data,
                        cache,
                    },
                    tts,
                    maximum_audio_seconds,
                    receiver,
                    audio,
                );
                match worker.start_client() {
                    Ok(initialization) => {
                        let _ = ready_tx.send(Ok(initialization));
                        worker.run();
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(playback_error(error)));
                    }
                }
            })
            .map_err(|error| PlaybackError::Backend(error.to_string()))?;
        match ready_rx.recv_timeout(Duration::from_secs(125)) {
            Ok(Ok(initialization)) => Ok(Self {
                commands,
                join: Mutex::new(Some(join)),
                initialization,
            }),
            Ok(Err(error)) => {
                let _ = join.join();
                Err(error)
            }
            Err(_) => {
                let _ = join.join();
                Err(PlaybackError::Backend(
                    "UtterPipe runtime initialization timed out".into(),
                ))
            }
        }
    }

    pub(crate) fn initialization(&self) -> &super::client::RuntimeInitialization {
        &self.initialization
    }
}

impl TtsAdapter for UtterPipeTts {
    fn capabilities(&self) -> TtsCapabilities {
        TtsCapabilities {
            voice_id: None,
            completion_observable: true,
            stoppable: true,
            volume_controllable: true,
        }
    }

    fn speak(
        &mut self,
        text: String,
        gain: f32,
        completion: CompletionNotifier,
    ) -> Result<(), PlaybackError> {
        self.speak_with_options_to(
            text,
            Map::new(),
            gain,
            &OutputTarget::SystemDefault,
            completion,
        )
    }

    fn speak_to(
        &mut self,
        text: String,
        gain: f32,
        target: &OutputTarget,
        completion: CompletionNotifier,
    ) -> Result<(), PlaybackError> {
        self.speak_with_options_to(text, Map::new(), gain, target, completion)
    }

    fn speak_with_options_to(
        &mut self,
        text: String,
        utterance_options: Map<String, Value>,
        gain: f32,
        target: &OutputTarget,
        completion: CompletionNotifier,
    ) -> Result<(), PlaybackError> {
        self.commands
            .try_send(WorkerCommand::Speak {
                text,
                utterance_options,
                gain,
                target: target.clone(),
                completion,
            })
            .map_err(|_| {
                PlaybackError::Backend("UtterPipe runtime worker is busy or unavailable".into())
            })
    }

    fn stop(&mut self, playback_id: Uuid) -> Result<(), PlaybackError> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.commands
            .send(WorkerCommand::Stop {
                playback_id,
                response: tx,
            })
            .map_err(|_| PlaybackError::Backend("UtterPipe runtime worker stopped".into()))?;
        rx.recv_timeout(Duration::from_millis(CANCELLATION_GRACE_MS + 500))
            .map_err(|_| {
                PlaybackError::Backend("UtterPipe stop acknowledgement timed out".into())
            })?
    }

    fn finished(&mut self, playback_id: Uuid) {
        let _ = self.commands.try_send(WorkerCommand::Finished(playback_id));
    }
}

impl Drop for UtterPipeTts {
    fn drop(&mut self) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let _ = self.commands.send(WorkerCommand::Shutdown(tx));
        let _ = rx.recv_timeout(Duration::from_secs(4));
        if let Some(join) = self
            .join
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let _ = join.join();
        }
    }
}

struct Worker {
    slug: String,
    executable: PathBuf,
    data: PathBuf,
    cache: PathBuf,
    tts: TtsConfig,
    commands: Receiver<WorkerCommand>,
    client: Option<Client>,
    audio: RodioAudio,
    restart_used: bool,
    active_request_id: Option<String>,
    active_playback_id: Option<Uuid>,
    shutdown_requested: Option<Sender<Result<(), PlaybackError>>>,
    maximum_audio_seconds: u64,
    expected_initialization: Option<super::client::RuntimeInitialization>,
}

struct WorkerLocation {
    slug: String,
    executable: PathBuf,
    data: PathBuf,
    cache: PathBuf,
}

impl Worker {
    fn new(
        location: WorkerLocation,
        tts: TtsConfig,
        maximum_audio_seconds: u64,
        commands: Receiver<WorkerCommand>,
        audio: RodioAudio,
    ) -> Self {
        Self {
            slug: location.slug,
            executable: location.executable,
            data: location.data,
            cache: location.cache,
            tts,
            commands,
            client: None,
            audio,
            restart_used: false,
            active_request_id: None,
            active_playback_id: None,
            shutdown_requested: None,
            maximum_audio_seconds,
            expected_initialization: None,
        }
    }

    fn start_client(&mut self) -> Result<super::client::RuntimeInitialization, ProviderError> {
        let mut client = Client::spawn(
            &self.executable,
            &self.slug,
            SessionKind::Runtime,
            &self
                .tts
                .utterpipe()
                .expect("validated provider")
                .provider_environment,
        )?;
        let initialization = client
            .initialize(&self.tts, &self.data, &self.cache)?
            .ok_or_else(|| {
                ProviderError::Protocol("runtime initialization returned no schema".into())
            })?;
        if self
            .expected_initialization
            .as_ref()
            .is_some_and(|expected| expected != &initialization)
        {
            return Err(ProviderError::Protocol(
                "restarted provider changed its audio deliveries or utterance schema".into(),
            ));
        }
        self.expected_initialization = Some(initialization.clone());
        self.client = Some(client);
        Ok(initialization)
    }

    fn run(&mut self) {
        while let Ok(command) = self.commands.recv() {
            match command {
                WorkerCommand::Speak {
                    text,
                    utterance_options,
                    gain,
                    target,
                    completion,
                } => {
                    if let Err(error) = self.ensure_client_for_speech() {
                        completion.fail(error);
                        continue;
                    }
                    if let Err(error) =
                        self.synthesize(text, utterance_options, gain, target, completion)
                    {
                        tracing::warn!(provider = %self.slug, %error, "UtterPipe synthesis failed");
                    }
                    if let Some(response) = self.shutdown_requested.take() {
                        let audio_result = self.audio.shutdown();
                        let provider_result = self
                            .client
                            .take()
                            .map(Client::shutdown)
                            .transpose()
                            .map_err(playback_error);
                        let _ = response.send(audio_result.and(provider_result.map(|_| ())));
                        break;
                    }
                }
                WorkerCommand::Stop {
                    playback_id,
                    response,
                } => {
                    let _ = response.send(self.audio.stop(playback_id));
                }
                WorkerCommand::Finished(playback_id) => self.audio.finished(playback_id),
                WorkerCommand::Shutdown(response) => {
                    let audio_result = self.audio.shutdown();
                    let provider_result = self
                        .client
                        .take()
                        .map(Client::shutdown)
                        .transpose()
                        .map_err(playback_error);
                    let _ = response.send(audio_result.and(provider_result.map(|_| ())));
                    break;
                }
            }
        }
    }

    fn ensure_client_for_speech(&mut self) -> Result<(), String> {
        if self.client.is_some() {
            return Ok(());
        }
        if self.restart_used {
            return Err("UtterPipe runtime is unavailable after its one restart".into());
        }
        self.restart_used = true;
        self.start_client()
            .map(|_| ())
            .map_err(|error| playback_error(error).to_string())
    }

    fn synthesize(
        &mut self,
        text: String,
        utterance_options: Map<String, Value>,
        gain: f32,
        target: OutputTarget,
        completion: CompletionNotifier,
    ) -> Result<(), ProviderError> {
        let playback_id = completion.playback_id();
        let timeout_seconds = (15 + text.chars().count() as u64 / 10).min(120);
        let client = self.client.as_mut().expect("client checked");
        let initialization = client.runtime_initialization.as_ref().ok_or_else(|| {
            ProviderError::Protocol("runtime has no initialized delivery set".into())
        })?;
        let delivery = initialization
            .audio_deliveries
            .first()
            .cloned()
            .ok_or_else(|| ProviderError::Protocol("runtime has no usable delivery".into()))?;
        let effective_options = merge_utterance_options(
            &initialization.configured_utterance_options,
            utterance_options,
        );
        super::schema::validate_request(
            &effective_options,
            Some(&initialization.utterance_options_schema),
        )
        .map_err(ProviderError::Configuration)?;
        client.select_audio_delivery(&delivery);
        let request_id = client.next_request_id();
        write_request(
            client.writer()?,
            &request_id,
            "synthesis.start",
            json!({
                "text": text,
                "audio_delivery": delivery,
                "utterance_options": effective_options,
            }),
        )?;
        let mode = delivery.mode;
        let format = delivery.format;
        self.active_request_id = Some(request_id.clone());
        self.active_playback_id = Some(playback_id);
        let mut completion = Some(completion);
        let deadline = Instant::now() + Duration::from_secs(timeout_seconds);
        let result = match mode {
            super::DeliveryMode::Complete => self.receive_complete(
                &request_id,
                &format,
                deadline,
                gain,
                &target,
                &mut completion,
            ),
            super::DeliveryMode::Incremental => self.receive_incremental(
                &request_id,
                &format,
                deadline,
                gain,
                &target,
                &mut completion,
            ),
        };
        self.active_request_id = None;
        self.active_playback_id = None;
        if let Err(error) = &result {
            // A terminal incremental failure must immediately silence and discard
            // samples already queued locally. This is harmless before playback starts.
            let _ = self.audio.stop(playback_id);
            if !matches!(error, ProviderError::Remote { code, .. } if code == "cancelled")
                && let Some(completion) = completion.take()
            {
                completion.fail(playback_error_string(error));
            }
            if matches!(
                error,
                ProviderError::Process(_) | ProviderError::Protocol(_) | ProviderError::Timeout
            ) {
                if let Some(client) = self.client.as_mut() {
                    client.terminate();
                }
                self.client = None;
            }
        }
        result
    }

    fn receive_complete(
        &mut self,
        request_id: &str,
        expected_format: &str,
        deadline: Instant,
        gain: f32,
        target: &OutputTarget,
        completion: &mut Option<CompletionNotifier>,
    ) -> Result<(), ProviderError> {
        let value = self.wait_synthesis_control(request_id, deadline)?;
        let audio = response_result_for(&value, request_id)?
            .get("audio")
            .cloned()
            .ok_or_else(|| {
                ProviderError::Protocol("synthesis response omitted audio metadata".into())
            })?;
        validate_audio_metadata(&audio, expected_format)?;
        if audio.get("frame_count").is_some() {
            return Err(ProviderError::Protocol(
                "complete audio metadata must omit frame_count".into(),
            ));
        }
        let declared = audio
            .get("byte_length")
            .and_then(Value::as_u64)
            .ok_or_else(|| ProviderError::Protocol("audio byte_length is invalid".into()))?;
        if declared == 0 || declared > MAX_AUDIO_BYTES {
            return Err(ProviderError::Protocol(
                "complete audio length exceeds bounds".into(),
            ));
        }
        let frame = self.wait_frame_or_control(deadline)?;
        let Frame::Audio(bytes) = frame else {
            return Err(ProviderError::Protocol(
                "complete synthesis response was not immediately followed by audio".into(),
            ));
        };
        if bytes.len() as u64 != declared {
            return Err(ProviderError::Protocol(
                "complete audio length does not match metadata".into(),
            ));
        }
        if expected_format == "audio/wav;codec=pcm_s16le" {
            validate_pcm_wav(&bytes, &audio)?;
            let prepared = PreparedAudio::from_memory(bytes).map_err(|error| {
                ProviderError::Protocol(format!("provider returned invalid PCM WAV: {error}"))
            })?;
            if self.maximum_audio_seconds != 0
                && prepared.info().duration > Duration::from_secs(self.maximum_audio_seconds)
            {
                return Err(ProviderError::Protocol(
                    "complete audio exceeds the configured duration limit".into(),
                ));
            }
            let owned = completion.take().expect("completion is installed once");
            return self
                .audio
                .play_to(prepared, gain, target, owned)
                .map_err(|error| ProviderError::Process(error.to_string()));
        }

        let encoded_format = EncodedFormat::parse(expected_format).ok_or_else(|| {
            ProviderError::Protocol("complete audio format is unsupported".into())
        })?;
        let (sample_rate, channels, chunks) =
            decode_complete(encoded_format, bytes, self.maximum_audio_seconds, deadline)?;
        let owned = completion.take().expect("completion is installed once");
        self.audio
            .start_incremental(sample_rate, channels, gain, target, owned)
            .map_err(|error| ProviderError::Process(error.to_string()))?;
        for chunk in chunks {
            self.append_bounded_pcm(request_id, sample_rate, channels, &chunk, deadline)?;
        }
        let playback_id = self.current_playback_id()?;
        self.audio
            .finish_incremental(playback_id)
            .map_err(|error| ProviderError::Process(error.to_string()))
    }

    fn receive_incremental(
        &mut self,
        request_id: &str,
        expected_format: &str,
        deadline: Instant,
        gain: f32,
        target: &OutputTarget,
        completion: &mut Option<CompletionNotifier>,
    ) -> Result<(), ProviderError> {
        let begin = self.wait_synthesis_control(request_id, deadline)?;
        if begin.get("kind").and_then(Value::as_str) != Some("event")
            || begin.get("event").and_then(Value::as_str) != Some("synthesis.audio_begin")
        {
            if begin.get("kind").and_then(Value::as_str) == Some("response") {
                // Preserve a well-formed engine/cancellation error. A successful
                // complete response still violates the negotiated delivery mode.
                response_result_for(&begin, request_id)?;
                return Err(ProviderError::Protocol(
                    "runtime delivery selection did not match provider output".into(),
                ));
            }
            return Err(ProviderError::Protocol(
                "incremental synthesis omitted audio_begin".into(),
            ));
        }
        let params = begin
            .get("params")
            .ok_or_else(|| ProviderError::Protocol("audio_begin omitted params".into()))?;
        if params.get("request_id").and_then(Value::as_str) != Some(request_id) {
            return Err(ProviderError::Protocol(
                "audio_begin request ID mismatch".into(),
            ));
        }
        if params.get("format").and_then(Value::as_str) != Some(expected_format) {
            return Err(ProviderError::Protocol(
                "audio_begin format does not match negotiated audio format".into(),
            ));
        }
        if let Some(encoded_format) = EncodedFormat::parse(expected_format) {
            validate_encoded_sample_metadata(params)?;
            return self.receive_incremental_encoded(
                request_id,
                expected_format,
                encoded_format,
                deadline,
                gain,
                target,
                completion,
            );
        }
        if expected_format != "audio/pcm;codec=pcm_s16le" {
            return Err(ProviderError::Protocol(
                "negotiated incremental audio format is unsupported".into(),
            ));
        }
        let sample_rate = params
            .get("sample_rate_hz")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .ok_or_else(|| ProviderError::Protocol("audio_begin sample rate is invalid".into()))?;
        let channels = params
            .get("channels")
            .and_then(Value::as_u64)
            .and_then(|v| u16::try_from(v).ok())
            .ok_or_else(|| ProviderError::Protocol("audio_begin channels are invalid".into()))?;
        validate_pcm(sample_rate, channels)?;

        let mut pending = Vec::<Vec<u8>>::new();
        let mut total = 0_u64;
        let mut frame_count = 0_u64;
        let prebuffer_bytes =
            u64::from(sample_rate) * u64::from(channels) * 2 * PREBUFFER_MS / 1000;
        let mut started = false;
        loop {
            match self.wait_frame_or_control(deadline)? {
                Frame::Audio(bytes) => {
                    let alignment = usize::from(channels) * 2;
                    if bytes.is_empty() || bytes.len() > 1024 * 1024 || bytes.len() % alignment != 0
                    {
                        return Err(ProviderError::Protocol(
                            "incremental PCM frame is misaligned or oversized".into(),
                        ));
                    }
                    total = total.checked_add(bytes.len() as u64).ok_or_else(|| {
                        ProviderError::Protocol("incremental byte count overflow".into())
                    })?;
                    if total > MAX_AUDIO_BYTES {
                        return Err(ProviderError::Protocol(
                            "incremental audio exceeds negotiated bound".into(),
                        ));
                    }
                    let bytes_per_second = u128::from(sample_rate) * u128::from(channels) * 2;
                    if self.maximum_audio_seconds != 0
                        && u128::from(total)
                            > bytes_per_second * u128::from(self.maximum_audio_seconds)
                    {
                        return Err(ProviderError::Protocol(
                            "incremental audio exceeds the configured duration limit".into(),
                        ));
                    }
                    frame_count += 1;
                    if started {
                        self.append_bounded_pcm(
                            request_id,
                            sample_rate,
                            channels,
                            &bytes,
                            deadline,
                        )?;
                    } else {
                        pending.push(bytes);
                        if total >= prebuffer_bytes {
                            let owned = completion.take().expect("completion is installed once");
                            self.audio
                                .start_incremental(sample_rate, channels, gain, target, owned)
                                .map_err(|error| ProviderError::Process(error.to_string()))?;
                            for chunk in pending.drain(..) {
                                self.append_bounded_pcm(
                                    request_id,
                                    sample_rate,
                                    channels,
                                    &chunk,
                                    deadline,
                                )?;
                            }
                            started = true;
                        }
                    }
                }
                Frame::Control(value) => {
                    if frame_count > 0 && ignorable_runtime_event(&value)? {
                        continue;
                    }
                    let result = response_result_for(&value, request_id)?;
                    let audio = result.get("audio").ok_or_else(|| {
                        ProviderError::Protocol(
                            "terminal synthesis response omitted audio metadata".into(),
                        )
                    })?;
                    validate_audio_metadata(audio, "audio/pcm;codec=pcm_s16le")?;
                    if audio.get("sample_rate_hz").and_then(Value::as_u64)
                        != Some(u64::from(sample_rate))
                        || audio.get("channels").and_then(Value::as_u64)
                            != Some(u64::from(channels))
                    {
                        return Err(ProviderError::Protocol(
                            "incremental terminal format differs from audio_begin".into(),
                        ));
                    }
                    if audio.get("byte_length").and_then(Value::as_u64) != Some(total)
                        || audio.get("frame_count").and_then(Value::as_u64) != Some(frame_count)
                    {
                        return Err(ProviderError::Protocol(
                            "incremental terminal counts do not match received audio".into(),
                        ));
                    }
                    if !started {
                        if pending.is_empty() {
                            return Err(ProviderError::Protocol(
                                "incremental synthesis produced no audio".into(),
                            ));
                        }
                        let owned = completion.take().expect("completion is installed once");
                        self.audio
                            .start_incremental(sample_rate, channels, gain, target, owned)
                            .map_err(|error| ProviderError::Process(error.to_string()))?;
                        for chunk in pending.drain(..) {
                            self.append_bounded_pcm(
                                request_id,
                                sample_rate,
                                channels,
                                &chunk,
                                deadline,
                            )?;
                        }
                    }
                    let playback_id = self.current_playback_id()?;
                    self.audio
                        .finish_incremental(playback_id)
                        .map_err(|error| ProviderError::Process(error.to_string()))?;
                    return Ok(());
                }
            }
        }
    }

    fn append_bounded_pcm(
        &mut self,
        request_id: &str,
        sample_rate: u32,
        channels: u16,
        bytes: &[u8],
        deadline: Instant,
    ) -> Result<(), ProviderError> {
        let playback_id = self.current_playback_id()?;
        let bytes_per_second = u64::from(sample_rate) * u64::from(channels) * 2;
        let alignment = usize::from(channels) * 2;
        let segment_bytes = ((bytes_per_second / 10) as usize / alignment).max(1) * alignment;
        for segment in bytes.chunks(segment_bytes) {
            while self
                .audio
                .incremental_queue_len(playback_id)
                .map_err(|error| ProviderError::Process(error.to_string()))?
                >= (MAX_QUEUED_MS / QUEUED_SEGMENT_MS) as usize
            {
                if Instant::now() >= deadline {
                    return Err(ProviderError::Timeout);
                }
                if self.handle_waiting_command(request_id)? {
                    return Err(cancelled_error());
                }
                thread::sleep(Duration::from_millis(5));
            }
            self.audio
                .append_incremental(playback_id, sample_rate, channels, segment)
                .map_err(|error| ProviderError::Process(error.to_string()))?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn receive_incremental_encoded(
        &mut self,
        request_id: &str,
        expected_format: &str,
        encoded_format: EncodedFormat,
        deadline: Instant,
        gain: f32,
        target: &OutputTarget,
        completion: &mut Option<CompletionNotifier>,
    ) -> Result<(), ProviderError> {
        let mut decoder = EncodedDecoder::spawn(encoded_format, self.maximum_audio_seconds)
            .map_err(ProviderError::Process)?;
        let mut playback = EncodedPlaybackState::default();
        let mut total = 0_u64;
        let mut frame_count = 0_u64;

        loop {
            match self.wait_encoded_event(deadline, &decoder.output)? {
                EncodedEvent::Decoder(message) => {
                    if self.consume_decoded_message(
                        request_id,
                        message,
                        gain,
                        target,
                        completion,
                        &mut playback,
                        deadline,
                    )? {
                        return Err(ProviderError::Protocol(
                            "encoded decoder ended before terminal metadata".into(),
                        ));
                    }
                }
                EncodedEvent::Provider(Frame::Audio(bytes)) => {
                    if bytes.is_empty() || bytes.len() > 1024 * 1024 {
                        return Err(ProviderError::Protocol(
                            "incremental encoded frame is empty or oversized".into(),
                        ));
                    }
                    total = total.checked_add(bytes.len() as u64).ok_or_else(|| {
                        ProviderError::Protocol("incremental byte count overflow".into())
                    })?;
                    if total > MAX_AUDIO_BYTES {
                        return Err(ProviderError::Protocol(
                            "incremental audio exceeds negotiated bound".into(),
                        ));
                    }
                    frame_count += 1;
                    let sender = decoder
                        .input
                        .as_ref()
                        .expect("decoder input is open")
                        .clone();
                    let mut pending = bytes;
                    loop {
                        match sender.send_timeout(pending, Duration::from_millis(5)) {
                            Ok(()) => break,
                            Err(crossbeam_channel::SendTimeoutError::Timeout(bytes)) => {
                                pending = bytes;
                                while let Ok(message) = decoder.output.try_recv() {
                                    if self.consume_decoded_message(
                                        request_id,
                                        message,
                                        gain,
                                        target,
                                        completion,
                                        &mut playback,
                                        deadline,
                                    )? {
                                        return Err(ProviderError::Protocol(
                                            "encoded decoder ended before terminal metadata".into(),
                                        ));
                                    }
                                }
                                if Instant::now() >= deadline {
                                    return Err(ProviderError::Timeout);
                                }
                                if self.handle_waiting_command(request_id)? {
                                    return Err(cancelled_error());
                                }
                            }
                            Err(crossbeam_channel::SendTimeoutError::Disconnected(_)) => {
                                return Err(ProviderError::Protocol(
                                    "encoded decoder stopped before consuming the stream".into(),
                                ));
                            }
                        }
                    }
                }
                EncodedEvent::Provider(Frame::Control(value)) => {
                    if frame_count > 0 && ignorable_runtime_event(&value)? {
                        continue;
                    }
                    let result = response_result_for(&value, request_id)?;
                    let audio = result.get("audio").ok_or_else(|| {
                        ProviderError::Protocol(
                            "terminal synthesis response omitted audio metadata".into(),
                        )
                    })?;
                    validate_audio_metadata(audio, expected_format)?;
                    if audio.get("byte_length").and_then(Value::as_u64) != Some(total)
                        || audio.get("frame_count").and_then(Value::as_u64) != Some(frame_count)
                    {
                        return Err(ProviderError::Protocol(
                            "incremental terminal counts do not match received audio".into(),
                        ));
                    }
                    if total == 0 || frame_count == 0 {
                        return Err(ProviderError::Protocol(
                            "incremental synthesis produced no audio".into(),
                        ));
                    }
                    decoder.finish_input();
                    loop {
                        match self.wait_encoded_event(deadline, &decoder.output)? {
                            EncodedEvent::Decoder(message) => {
                                if self.consume_decoded_message(
                                    request_id,
                                    message,
                                    gain,
                                    target,
                                    completion,
                                    &mut playback,
                                    deadline,
                                )? {
                                    return Ok(());
                                }
                            }
                            EncodedEvent::Provider(Frame::Control(value))
                                if ignorable_runtime_event(&value)? => {}
                            EncodedEvent::Provider(_) => {
                                return Err(ProviderError::Protocol(
                                    "provider sent a frame after terminal synthesis metadata"
                                        .into(),
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn consume_decoded_message(
        &mut self,
        request_id: &str,
        message: DecodedMessage,
        gain: f32,
        target: &OutputTarget,
        completion: &mut Option<CompletionNotifier>,
        state: &mut EncodedPlaybackState,
        deadline: Instant,
    ) -> Result<bool, ProviderError> {
        match message {
            DecodedMessage::Pcm {
                sample_rate_hz,
                channels,
                bytes,
            } => {
                validate_pcm(sample_rate_hz, channels)?;
                if state
                    .sample_rate_hz
                    .is_some_and(|value| value != sample_rate_hz)
                    || state.channels.is_some_and(|value| value != channels)
                {
                    return Err(ProviderError::Protocol(
                        "decoded audio changes sample format mid-stream".into(),
                    ));
                }
                state.sample_rate_hz = Some(sample_rate_hz);
                state.channels = Some(channels);
                let alignment = usize::from(channels) * 2;
                if bytes.is_empty() || !bytes.len().is_multiple_of(alignment) {
                    return Err(ProviderError::Protocol(
                        "decoder returned invalid PCM samples".into(),
                    ));
                }
                if state.started {
                    self.append_bounded_pcm(
                        request_id,
                        sample_rate_hz,
                        channels,
                        &bytes,
                        deadline,
                    )?;
                } else {
                    state.pending_frames += (bytes.len() / alignment) as u64;
                    state.pending.push(bytes);
                    if state.pending_frames * 1_000 >= u64::from(sample_rate_hz) * PREBUFFER_MS {
                        self.start_decoded_playback(
                            request_id, gain, target, completion, state, deadline,
                        )?;
                    }
                }
                Ok(false)
            }
            DecodedMessage::Complete => {
                if state.sample_rate_hz.is_none() || state.pending.is_empty() && !state.started {
                    return Err(ProviderError::Protocol(
                        "encoded audio contains no decodable samples".into(),
                    ));
                }
                if !state.started {
                    self.start_decoded_playback(
                        request_id, gain, target, completion, state, deadline,
                    )?;
                }
                let playback_id = self.current_playback_id()?;
                self.audio
                    .finish_incremental(playback_id)
                    .map_err(|error| ProviderError::Process(error.to_string()))?;
                Ok(true)
            }
            DecodedMessage::Failed(error) => Err(ProviderError::Protocol(format!(
                "provider returned invalid encoded audio: {error}"
            ))),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn start_decoded_playback(
        &mut self,
        request_id: &str,
        gain: f32,
        target: &OutputTarget,
        completion: &mut Option<CompletionNotifier>,
        state: &mut EncodedPlaybackState,
        deadline: Instant,
    ) -> Result<(), ProviderError> {
        let sample_rate = state.sample_rate_hz.expect("decoded format exists");
        let channels = state.channels.expect("decoded format exists");
        let owned = completion.take().expect("completion is installed once");
        self.audio
            .start_incremental(sample_rate, channels, gain, target, owned)
            .map_err(|error| ProviderError::Process(error.to_string()))?;
        for chunk in state.pending.drain(..) {
            self.append_bounded_pcm(request_id, sample_rate, channels, &chunk, deadline)?;
        }
        state.started = true;
        Ok(())
    }

    fn wait_encoded_event(
        &mut self,
        deadline: Instant,
        decoded: &Receiver<DecodedMessage>,
    ) -> Result<EncodedEvent, ProviderError> {
        loop {
            let frames = self.client.as_ref().expect("client present").frames.clone();
            let timeout =
                crossbeam_channel::after(deadline.saturating_duration_since(Instant::now()));
            select! {
                recv(self.commands) -> command => match command {
                    Ok(WorkerCommand::Stop { playback_id, response }) => {
                        let result = self.cancel_active(playback_id);
                        if result.is_err()
                            && let Some(client) = self.client.as_mut()
                        {
                            client.terminate();
                            self.client = None;
                        }
                        let _ = response.send(result.as_ref().map(|_| ()).map_err(|error| PlaybackError::Backend(playback_error_string(error))));
                        return Err(cancelled_error());
                    }
                    Ok(WorkerCommand::Shutdown(response)) => {
                        let result = self.cancel_current();
                        self.shutdown_requested = Some(response);
                        result?;
                        return Err(cancelled_error());
                    }
                    Ok(WorkerCommand::Finished(playback_id)) => self.audio.finished(playback_id),
                    Ok(WorkerCommand::Speak { completion, .. }) => completion.fail("UtterPipe provider is busy"),
                    Err(_) => return Err(ProviderError::Process("runtime worker command channel closed".into())),
                },
                recv(frames) -> frame => {
                    let frame = frame.map_err(|_| ProviderError::Process("provider frame reader stopped".into()))??;
                    return Ok(EncodedEvent::Provider(frame));
                },
                recv(decoded) -> message => {
                    return message.map(EncodedEvent::Decoder).map_err(|_| ProviderError::Process("encoded decoder stopped without a terminal result".into()));
                },
                recv(timeout) -> _ => return Err(ProviderError::Timeout),
            }
        }
    }

    fn wait_synthesis_control(
        &mut self,
        request_id: &str,
        deadline: Instant,
    ) -> Result<Value, ProviderError> {
        loop {
            match self.wait_frame_or_control(deadline)? {
                Frame::Control(value) if ignorable_runtime_event(&value)? => {}
                Frame::Control(value) => return Ok(value),
                Frame::Audio(_) => {
                    return Err(ProviderError::Protocol(format!(
                        "audio arrived before metadata for {request_id}"
                    )));
                }
            }
        }
    }

    fn wait_frame_or_control(&mut self, deadline: Instant) -> Result<Frame, ProviderError> {
        loop {
            let frames = self.client.as_ref().expect("client present").frames.clone();
            let timeout =
                crossbeam_channel::after(deadline.saturating_duration_since(Instant::now()));
            select! {
                recv(self.commands) -> command => match command {
                Ok(WorkerCommand::Stop { playback_id, response }) => {
                    let result = self.cancel_active(playback_id);
                    if result.is_err()
                        && let Some(client) = self.client.as_mut()
                    {
                        client.terminate();
                        self.client = None;
                    }
                        let _ = response.send(result.as_ref().map(|_| ()).map_err(|error| PlaybackError::Backend(playback_error_string(error))));
                        return Err(cancelled_error());
                    }
                    Ok(WorkerCommand::Shutdown(response)) => {
                        let result = self.cancel_current();
                        self.shutdown_requested = Some(response);
                        result?;
                        return Err(cancelled_error());
                    }
                    Ok(WorkerCommand::Finished(playback_id)) => self.audio.finished(playback_id),
                    Ok(WorkerCommand::Speak { completion, .. }) => completion.fail("UtterPipe provider is busy"),
                    Err(_) => return Err(ProviderError::Process("runtime worker command channel closed".into())),
                },
                recv(frames) -> frame => return frame.map_err(|_| ProviderError::Process("provider frame reader stopped".into()))?,
                recv(timeout) -> _ => return Err(ProviderError::Timeout),
            }
        }
    }

    fn handle_waiting_command(&mut self, request_id: &str) -> Result<bool, ProviderError> {
        match self.commands.try_recv() {
            Ok(WorkerCommand::Stop {
                playback_id,
                response,
            }) => {
                let result = self.cancel_active(playback_id);
                if result.is_err()
                    && let Some(client) = self.client.as_mut()
                {
                    client.terminate();
                    self.client = None;
                }
                let _ = response.send(
                    result
                        .as_ref()
                        .map(|_| ())
                        .map_err(|error| PlaybackError::Backend(playback_error_string(error))),
                );
                Ok(true)
            }
            Ok(WorkerCommand::Shutdown(response)) => {
                let result = self.cancel_current();
                self.shutdown_requested = Some(response);
                result.map(|_| true)
            }
            Ok(WorkerCommand::Finished(playback_id)) => {
                self.audio.finished(playback_id);
                Ok(false)
            }
            Ok(WorkerCommand::Speak { completion, .. }) => {
                completion.fail("UtterPipe provider is busy");
                Ok(false)
            }
            Err(crossbeam_channel::TryRecvError::Empty) => Ok(false),
            Err(_) => Err(ProviderError::Process(format!(
                "runtime worker command channel closed during {request_id}"
            ))),
        }
    }

    fn cancel_current(&mut self) -> Result<(), ProviderError> {
        let playback_id = self.current_playback_id()?;
        self.cancel_active(playback_id)
    }

    fn current_playback_id(&self) -> Result<Uuid, ProviderError> {
        self.active_playback_id
            .ok_or_else(|| ProviderError::Protocol("active playback ID is unavailable".into()))
    }

    fn cancel_active(&mut self, playback_id: Uuid) -> Result<(), ProviderError> {
        if self.active_playback_id != Some(playback_id) {
            return Err(ProviderError::Protocol(
                "requested playback ID is not the active synthesis".into(),
            ));
        }
        let _ = self.audio.stop(playback_id);
        let client = self.client.as_mut().expect("client present");
        if !client.info.capabilities.cancellation {
            client.terminate();
            self.client = None;
            return Ok(());
        }
        let request_id = self
            .active_request_id
            .as_deref()
            .ok_or_else(|| ProviderError::Protocol("active synthesis ID is unavailable".into()))?;
        let cancel_id = client.next_request_id();
        write_request(
            client.writer()?,
            &cancel_id,
            "synthesis.cancel",
            json!({"request_id":request_id}),
        )?;
        let deadline = Instant::now() + Duration::from_millis(CANCELLATION_GRACE_MS);
        let mut cancel_result = None;
        let mut original_terminal = None;
        while Instant::now() < deadline {
            match client
                .frames
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(Ok(Frame::Audio(_))) if cancel_result == Some(true) => {
                    return Err(ProviderError::Protocol(
                        "provider sent audio after accepting cancellation".into(),
                    ));
                }
                Ok(Ok(Frame::Audio(_))) => {}
                Ok(Ok(Frame::Control(value))) => {
                    if ignorable_runtime_event(&value)? {
                        continue;
                    }
                    let id = value.get("id").and_then(Value::as_str);
                    if id == Some(cancel_id.as_str()) {
                        let result = response_result(&value)?;
                        let accepted =
                            result
                                .get("accepted")
                                .and_then(Value::as_bool)
                                .ok_or_else(|| {
                                    ProviderError::Protocol(
                                        "cancellation response omitted accepted".into(),
                                    )
                                })?;
                        if cancel_result.replace(accepted).is_some() {
                            return Err(ProviderError::Protocol(
                                "provider sent duplicate cancellation responses".into(),
                            ));
                        }
                        match (accepted, original_terminal) {
                            (true, Some(_)) => {
                                return Err(ProviderError::Protocol(
                                    "provider accepted cancellation after the synthesis terminal response"
                                        .into(),
                                ));
                            }
                            (false, Some(_)) => return Ok(()),
                            (false, None) => {
                                return Err(ProviderError::Protocol(
                                    "provider rejected cancellation before the synthesis terminal response"
                                        .into(),
                                ));
                            }
                            (true, None) => {}
                        }
                    } else if id == Some(request_id) {
                        let cancelled = match (value.get("result"), value.get("error")) {
                            (Some(result), None) if result.is_object() => false,
                            (None, Some(error)) => match remote_error(error) {
                                ProviderError::Remote { code, .. } => code == "cancelled",
                                error => return Err(error),
                            },
                            (Some(_), None) => {
                                return Err(ProviderError::Protocol(
                                    "synthesis terminal result must be an object".into(),
                                ));
                            }
                            _ => {
                                return Err(ProviderError::Protocol(
                                    "synthesis terminal response is malformed".into(),
                                ));
                            }
                        };
                        if original_terminal.replace(cancelled).is_some() {
                            return Err(ProviderError::Protocol(
                                "provider sent duplicate synthesis terminal responses".into(),
                            ));
                        }
                        match cancel_result {
                            Some(true) if cancelled => return Ok(()),
                            Some(true) => {
                                return Err(ProviderError::Protocol(
                                    "accepted cancellation did not end with a cancelled error"
                                        .into(),
                                ));
                            }
                            Some(false) => {
                                return Err(ProviderError::Protocol(
                                    "provider rejected cancellation after its response".into(),
                                ));
                            }
                            None => {}
                        }
                    } else {
                        return Err(ProviderError::Protocol(
                            "unexpected response while cancelling synthesis".into(),
                        ));
                    }
                }
                Ok(Err(error)) => return Err(error),
                Err(_) => break,
            }
        }
        client.terminate();
        self.client = None;
        Ok(())
    }
}

fn merge_utterance_options(
    configured: &Map<String, Value>,
    request: Map<String, Value>,
) -> Map<String, Value> {
    let mut effective = configured.clone();
    effective.extend(request);
    effective
}

fn validate_pcm(sample_rate: u32, channels: u16) -> Result<(), ProviderError> {
    if !(8_000..=96_000).contains(&sample_rate) || !(1..=2).contains(&channels) {
        return Err(ProviderError::Protocol(
            "audio sample rate or channel count is outside bounds".into(),
        ));
    }
    Ok(())
}

fn validate_audio_metadata(audio: &Value, expected_format: &str) -> Result<(), ProviderError> {
    if audio.get("format").and_then(Value::as_str) != Some(expected_format) {
        return Err(ProviderError::Protocol(
            "audio format does not match negotiated format".into(),
        ));
    }
    if EncodedFormat::parse(expected_format).is_some() {
        return validate_encoded_sample_metadata(audio);
    }
    let sample_rate = audio
        .get("sample_rate_hz")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| ProviderError::Protocol("audio sample rate is invalid".into()))?;
    let channels = audio
        .get("channels")
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
        .ok_or_else(|| ProviderError::Protocol("audio channels are invalid".into()))?;
    validate_pcm(sample_rate, channels)
}

fn validate_encoded_sample_metadata(metadata: &Value) -> Result<(), ProviderError> {
    if metadata.get("sample_rate_hz").is_some() || metadata.get("channels").is_some() {
        return Err(ProviderError::Protocol(
            "self-describing encoded audio must omit sample metadata".into(),
        ));
    }
    Ok(())
}

fn decode_complete(
    format: EncodedFormat,
    bytes: Vec<u8>,
    maximum_audio_seconds: u64,
    deadline: Instant,
) -> Result<(u32, u16, Vec<Vec<u8>>), ProviderError> {
    let mut decoder =
        EncodedDecoder::spawn(format, maximum_audio_seconds).map_err(ProviderError::Process)?;
    decoder
        .input
        .as_ref()
        .expect("decoder input is open")
        .send(bytes)
        .map_err(|_| ProviderError::Process("encoded decoder stopped before input".into()))?;
    decoder.finish_input();
    let mut sample_rate = None;
    let mut channels = None;
    let mut chunks = Vec::new();
    loop {
        let message = decoder
            .output
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|error| match error {
                crossbeam_channel::RecvTimeoutError::Timeout => ProviderError::Timeout,
                crossbeam_channel::RecvTimeoutError::Disconnected => ProviderError::Process(
                    "encoded decoder stopped without a terminal result".into(),
                ),
            })?;
        match message {
            DecodedMessage::Pcm {
                sample_rate_hz,
                channels: decoded_channels,
                bytes,
            } => {
                if sample_rate.is_some_and(|value| value != sample_rate_hz)
                    || channels.is_some_and(|value| value != decoded_channels)
                {
                    return Err(ProviderError::Protocol(
                        "decoded audio changes sample format mid-stream".into(),
                    ));
                }
                sample_rate = Some(sample_rate_hz);
                channels = Some(decoded_channels);
                chunks.push(bytes);
            }
            DecodedMessage::Complete => {
                return match (sample_rate, channels, chunks.is_empty()) {
                    (Some(sample_rate), Some(channels), false) => {
                        Ok((sample_rate, channels, chunks))
                    }
                    _ => Err(ProviderError::Protocol(
                        "encoded audio contains no decodable samples".into(),
                    )),
                };
            }
            DecodedMessage::Failed(error) => {
                return Err(ProviderError::Protocol(format!(
                    "provider returned invalid encoded audio: {error}"
                )));
            }
        }
    }
}

fn validate_pcm_wav(bytes: &[u8], metadata: &Value) -> Result<(), ProviderError> {
    if bytes.len() < 44 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(ProviderError::Protocol(
            "complete audio is not RIFF/WAVE".into(),
        ));
    }
    let riff_size = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    if riff_size.checked_add(8) != Some(bytes.len()) {
        return Err(ProviderError::Protocol(
            "WAV RIFF size is inconsistent".into(),
        ));
    }
    let mut position: usize = 12;
    let mut format = None;
    let mut data_length = None;
    while position
        .checked_add(8)
        .is_some_and(|end| end <= bytes.len())
    {
        let chunk_id = &bytes[position..position + 4];
        let chunk_length =
            u32::from_le_bytes(bytes[position + 4..position + 8].try_into().unwrap()) as usize;
        let start = position + 8;
        let end = start
            .checked_add(chunk_length)
            .ok_or_else(|| ProviderError::Protocol("WAV chunk size overflow".into()))?;
        if end > bytes.len() {
            return Err(ProviderError::Protocol(
                "WAV chunk exceeds container".into(),
            ));
        }
        if chunk_id == b"fmt " {
            if format.is_some() || chunk_length < 16 {
                return Err(ProviderError::Protocol(
                    "WAV fmt chunk is missing, duplicate, or short".into(),
                ));
            }
            let encoding = u16::from_le_bytes(bytes[start..start + 2].try_into().unwrap());
            let channels = u16::from_le_bytes(bytes[start + 2..start + 4].try_into().unwrap());
            let sample_rate = u32::from_le_bytes(bytes[start + 4..start + 8].try_into().unwrap());
            let byte_rate = u32::from_le_bytes(bytes[start + 8..start + 12].try_into().unwrap());
            let block_align = u16::from_le_bytes(bytes[start + 12..start + 14].try_into().unwrap());
            let bits = u16::from_le_bytes(bytes[start + 14..start + 16].try_into().unwrap());
            let expected_align = channels.checked_mul(2);
            let expected_rate =
                expected_align.and_then(|align| sample_rate.checked_mul(u32::from(align)));
            if encoding != 1
                || bits != 16
                || Some(block_align) != expected_align
                || Some(byte_rate) != expected_rate
            {
                return Err(ProviderError::Protocol(
                    "WAV is not internally consistent PCM16".into(),
                ));
            }
            validate_pcm(sample_rate, channels)?;
            format = Some((sample_rate, channels, block_align));
        } else if chunk_id == b"data" && data_length.replace(chunk_length).is_some() {
            return Err(ProviderError::Protocol(
                "WAV contains duplicate data chunks".into(),
            ));
        }
        if chunk_length & 1 == 1 && end == bytes.len() {
            return Err(ProviderError::Protocol(
                "odd WAV chunk is missing its padding byte".into(),
            ));
        }
        position = end
            .checked_add(chunk_length & 1)
            .ok_or_else(|| ProviderError::Protocol("WAV chunk offset overflow".into()))?;
    }
    if position != bytes.len() {
        return Err(ProviderError::Protocol(
            "WAV contains a truncated trailing chunk header".into(),
        ));
    }
    let (sample_rate, channels, block_align) =
        format.ok_or_else(|| ProviderError::Protocol("WAV omitted fmt chunk".into()))?;
    let data_length =
        data_length.ok_or_else(|| ProviderError::Protocol("WAV omitted data chunk".into()))?;
    if data_length == 0 || data_length % usize::from(block_align) != 0 {
        return Err(ProviderError::Protocol(
            "WAV data is empty or sample-misaligned".into(),
        ));
    }
    if metadata.get("sample_rate_hz").and_then(Value::as_u64) != Some(u64::from(sample_rate))
        || metadata.get("channels").and_then(Value::as_u64) != Some(u64::from(channels))
    {
        return Err(ProviderError::Protocol(
            "WAV format disagrees with response metadata".into(),
        ));
    }
    Ok(())
}

fn response_result(value: &Value) -> Result<&Value, ProviderError> {
    if value.get("kind").and_then(Value::as_str) != Some("response") {
        return Err(ProviderError::Protocol(
            "expected synthesis response".into(),
        ));
    }
    match (value.get("result"), value.get("error")) {
        (Some(result), None) if result.is_object() => Ok(result),
        (Some(_), None) => Err(ProviderError::Protocol(
            "response result must be an object".into(),
        )),
        (None, Some(error)) => Err(remote_error(error)),
        _ => Err(ProviderError::Protocol(
            "response must contain exactly one of result or error".into(),
        )),
    }
}

fn ignorable_runtime_event(value: &Value) -> Result<bool, ProviderError> {
    if value.get("kind").and_then(Value::as_str) != Some("event") {
        return Ok(false);
    }
    let event = value.get("event").and_then(Value::as_str).ok_or_else(|| {
        ProviderError::Protocol("provider event omitted a string event name".into())
    })?;
    if event.is_empty()
        || event.len() > 128
        || event.chars().any(char::is_control)
        || !value.get("params").is_some_and(Value::is_object)
    {
        return Err(ProviderError::Protocol(
            "provider event envelope is malformed".into(),
        ));
    }
    Ok(!matches!(
        event,
        "synthesis.audio_begin" | "operation.progress"
    ))
}

fn response_result_for<'a>(value: &'a Value, id: &str) -> Result<&'a Value, ProviderError> {
    if value.get("id").and_then(Value::as_str) != Some(id) {
        return Err(ProviderError::Protocol(
            "synthesis response ID mismatch".into(),
        ));
    }
    response_result(value)
}

fn cancelled_error() -> ProviderError {
    ProviderError::Remote {
        code: "cancelled".into(),
        message: "synthesis was cancelled".into(),
    }
}
fn playback_error(error: ProviderError) -> PlaybackError {
    PlaybackError::Backend(playback_error_string(&error))
}
fn playback_error_string(error: &ProviderError) -> String {
    error
        .to_string()
        .replace(['\r', '\n'], " ")
        .chars()
        .take(1024)
        .collect()
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::{fs, os::unix::fs::PermissionsExt, path::Path};

    use super::*;

    #[test]
    fn request_options_overlay_configured_defaults_without_mutating_them() {
        let configured = Map::from_iter([
            ("voice".into(), json!("coral")),
            ("speed".into(), json!(1.0)),
        ]);
        let effective =
            merge_utterance_options(&configured, Map::from_iter([("speed".into(), json!(1.25))]));
        assert_eq!(effective["voice"], "coral");
        assert_eq!(effective["speed"], 1.25);
        assert_eq!(configured["speed"], 1.0);
    }

    #[cfg(unix)]
    const FAKE_RUNTIME_PROVIDER: &str = r#"#!__PYTHON__
import hashlib, json, os, struct, sys

MODE = "__MODE__"

with open(os.path.join(os.path.dirname(__file__), 'starts'), 'a') as output:
    output.write(str(os.getpid()) + '\n')

def read_frame():
    header = sys.stdin.buffer.read(12)
    if not header:
        return None
    assert len(header) == 12 and header[:4] == b'UTP1' and header[4] == 1
    size = struct.unpack('>I', header[8:12])[0]
    return json.loads(sys.stdin.buffer.read(size))

def write_control(value):
    payload = json.dumps(value, separators=(',', ':')).encode()
    sys.stdout.buffer.write(b'UTP1' + bytes([1, 0, 0, 0]) + struct.pack('>I', len(payload)) + payload)
    sys.stdout.buffer.flush()

def write_audio(payload):
    sys.stdout.buffer.write(b'UTP1' + bytes([2, 0, 0, 0]) + struct.pack('>I', len(payload)) + payload)
    sys.stdout.buffer.flush()

pcm = b'\0\0' * 80
wav = (b'RIFF' + struct.pack('<I', 36 + len(pcm)) + b'WAVEfmt ' +
       struct.pack('<IHHIIHH', 16, 1, 1, 8000, 16000, 2, 16) +
       b'data' + struct.pack('<I', len(pcm)) + pcm)

while True:
    request = read_frame()
    if request is None:
        break
    method = request['method']
    request_id = request['id']
    if method == 'protocol.hello':
        fixed_schema = {'$schema':'https://json-schema.org/draft/2020-12/schema','type':'object','additionalProperties':False,'properties':{}}
        audio_deliveries = [{'mode':'complete','format':'audio/wav;codec=pcm_s16le'}]
        if MODE == 'incremental':
            audio_deliveries.append({'mode':'incremental','format':'audio/pcm;codec=pcm_s16le'})
        result = {
            'protocol': 'utterpipe.tts', 'version': 1, 'framing': 'UTP1',
            'provider': {'slug': 'fake', 'name': 'Runtime Fake', 'vendor': 'Tests', 'version': '0.1.0'},
            'capabilities': ['synthesis', 'synthesis.cancel'],
            'audio_deliveries': audio_deliveries,
            'utterance_schema_profile':'utterpipe.utterance-options/1',
            'provider_options_schema':fixed_schema,
            'management_options_schema':fixed_schema,
            'catalogs':[], 'import_kinds':[]
        }
        write_control({'kind': 'response', 'id': request_id, 'result': result})
    elif method == 'session.initialize':
        audio_format = ('audio/wav;codec=pcm_s16le' if MODE == 'complete'
                        else 'audio/pcm;codec=pcm_s16le')
        utterance_schema = {'$schema':'https://json-schema.org/draft/2020-12/schema',
                            'type':'object','additionalProperties':False,
                            'maxProperties':64,'properties':{}}
        canonical = json.dumps(utterance_schema, sort_keys=True, separators=(',', ':')).encode()
        write_control({'kind': 'response', 'id': request_id, 'result': {
            'ready': True, 'audio_deliveries': [{'mode':MODE,'format':audio_format}],
            'utterance_options_schema':utterance_schema,
            'utterance_options_schema_digest':'sha256:' + hashlib.sha256(canonical).hexdigest()
        }})
    elif method == 'synthesis.start':
        text = request['params']['text']
        if text == 'wrong-correlation':
            if MODE == 'complete':
                write_control({'kind': 'response', 'id': 'wrong-id', 'result': {}})
            else:
                write_control({'kind': 'event', 'event': 'synthesis.audio_begin', 'params': {
                    'request_id': 'wrong-id', 'format': 'audio/pcm;codec=pcm_s16le',
                    'sample_rate_hz': 8000, 'channels': 1
                }})
            continue
        if MODE == 'complete':
            write_control({'kind': 'response', 'id': request_id, 'result': {'audio': {
                'format': 'audio/wav;codec=pcm_s16le', 'sample_rate_hz': 8000,
                'channels': 1, 'byte_length': len(wav)
            }}})
            write_audio(wav)
        else:
            write_control({'kind': 'event', 'event': 'synthesis.audio_begin', 'params': {
                'request_id': request_id, 'format': 'audio/pcm;codec=pcm_s16le',
                'sample_rate_hz': 8000, 'channels': 1
            }})
            if text == 'cancel-terminal-first':
                write_control({'kind': 'response', 'id': request_id,
                               'error': {'code': 'upstream_failed', 'message': 'upstream failed'}})
                cancel = read_frame()
                assert cancel['method'] == 'synthesis.cancel'
                write_control({'kind': 'response', 'id': cancel['id'], 'result': {'accepted': False}})
            elif text.startswith('cancel'):
                cancel = read_frame()
                assert cancel['method'] == 'synthesis.cancel'
                assert cancel['params']['request_id'] == request_id
                if text == 'cancel-original-first':
                    write_control({'kind': 'response', 'id': request_id,
                                   'error': {'code': 'cancelled', 'message': 'synthesis was cancelled'}})
                    write_control({'kind': 'response', 'id': cancel['id'], 'result': {'accepted': True}})
                else:
                    write_control({'kind': 'response', 'id': cancel['id'], 'result': {'accepted': True}})
                    if text == 'cancel-audio-after':
                        write_audio(pcm)
                    write_control({'kind': 'response', 'id': request_id,
                                   'error': {'code': 'cancelled', 'message': 'synthesis was cancelled'}})
            else:
                write_audio(pcm)
                write_audio(pcm)
                write_control({'kind': 'response', 'id': request_id, 'result': {'audio': {
                    'format': 'audio/pcm;codec=pcm_s16le', 'sample_rate_hz': 8000,
                    'channels': 1, 'byte_length': len(pcm) * 2, 'frame_count': 2
                }}})
    elif method == 'session.shutdown':
        write_control({'kind': 'response', 'id': request_id, 'result': {'accepted': True}})
        assert read_frame() is None
        break
    else:
        raise AssertionError('unexpected method: ' + method)
"#;

    fn pcm_wav() -> Vec<u8> {
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&38_u32.to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&8_000_u32.to_le_bytes());
        wav.extend_from_slice(&16_000_u32.to_le_bytes());
        wav.extend_from_slice(&2_u16.to_le_bytes());
        wav.extend_from_slice(&16_u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&2_u32.to_le_bytes());
        wav.extend_from_slice(&0_i16.to_le_bytes());
        wav
    }

    #[test]
    fn complete_wav_must_be_consistent_pcm16_and_match_metadata() {
        let wav = pcm_wav();
        let metadata = json!({"sample_rate_hz":8000, "channels":1});
        validate_pcm_wav(&wav, &metadata).unwrap();

        let mut compressed = wav.clone();
        compressed[20..22].copy_from_slice(&3_u16.to_le_bytes());
        assert!(validate_pcm_wav(&compressed, &metadata).is_err());
        assert!(validate_pcm_wav(&wav, &json!({"sample_rate_hz":24000, "channels":1})).is_err());

        let mut trailing = wav.clone();
        trailing.extend_from_slice(&[0]);
        trailing[4..8].copy_from_slice(&39_u32.to_le_bytes());
        assert!(validate_pcm_wav(&trailing, &metadata).is_err());

        let mut missing_padding = wav;
        missing_padding[40..44].copy_from_slice(&1_u32.to_le_bytes());
        missing_padding.truncate(45);
        missing_padding[4..8].copy_from_slice(&37_u32.to_le_bytes());
        assert!(validate_pcm_wav(&missing_padding, &metadata).is_err());
    }

    #[test]
    fn encoded_metadata_is_self_describing_and_strict() {
        validate_audio_metadata(&json!({"format":"audio/mpeg"}), "audio/mpeg").unwrap();
        validate_audio_metadata(
            &json!({"format":"audio/ogg;codecs=opus"}),
            "audio/ogg;codecs=opus",
        )
        .unwrap();
        assert!(
            validate_audio_metadata(
                &json!({"format":"audio/mpeg","sample_rate_hz":24000}),
                "audio/mpeg"
            )
            .is_err()
        );
        assert!(
            validate_audio_metadata(
                &json!({"format":"audio/ogg;codecs=opus","channels":null}),
                "audio/ogg;codecs=opus"
            )
            .is_err()
        );
    }

    #[test]
    fn incremental_start_preserves_remote_errors() {
        let error = json!({
            "kind": "response",
            "id": "synth",
            "error": {"code": "assets_missing", "message": "prepare the selected model"}
        });
        assert!(matches!(
            response_result_for(&error, "synth"),
            Err(ProviderError::Remote { code, .. }) if code == "assets_missing"
        ));

        let both = json!({
            "kind": "response", "id": "synth", "result": {},
            "error": {"code": "internal", "message": "bad"}
        });
        assert!(matches!(
            response_result_for(&both, "synth"),
            Err(ProviderError::Protocol(_))
        ));
        let scalar = json!({"kind": "response", "id": "synth", "result": true});
        assert!(matches!(
            response_result_for(&scalar, "synth"),
            Err(ProviderError::Protocol(_))
        ));
    }

    #[cfg(unix)]
    fn python_interpreter() -> Option<&'static str> {
        [
            "/usr/bin/python3",
            "/usr/local/bin/python3",
            "/opt/homebrew/bin/python3",
        ]
        .into_iter()
        .find(|path| Path::new(path).is_file())
    }

    #[cfg(unix)]
    fn write_runtime_provider(directory: &Path, mode: &str) -> Option<PathBuf> {
        let interpreter = python_interpreter()?;
        let executable = directory.join(format!("utterpipe-runtime-{mode}"));
        let source = FAKE_RUNTIME_PROVIDER
            .replace("__PYTHON__", interpreter)
            .replace("__MODE__", mode);
        fs::write(&executable, source).unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&executable, permissions).unwrap();
        Some(executable)
    }

    #[cfg(unix)]
    fn runtime_tts() -> TtsConfig {
        TtsConfig {
            enabled: true,
            backend: crate::config::TtsBackend::Utterpipe(crate::config::UtterPipeTtsConfig {
                provider: "fake".into(),
                audio_deliveries: Vec::new(),
                provider_environment: Vec::new(),
                provider_options: toml::Table::new(),
                utterance_options: toml::Table::new(),
                agent_utterance_options: Vec::new(),
            }),
            maximum_characters: 1_000,
        }
    }

    #[cfg(unix)]
    fn runtime_client(
        executable: &Path,
        directory: &Path,
        mode: super::super::DeliveryMode,
    ) -> Client {
        let mut client = Client::spawn(executable, "fake", SessionKind::Runtime, &[]).unwrap();
        let selected = client
            .initialize(
                &runtime_tts(),
                &directory.join("data"),
                &directory.join("cache"),
            )
            .unwrap()
            .unwrap();
        assert_eq!(selected.audio_deliveries[0].mode, mode);
        client
    }

    #[cfg(unix)]
    fn next_provider_frame(client: &Client) -> Frame {
        client
            .frames
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap()
    }

    #[cfg(unix)]
    fn begin_synthesis(client: &mut Client, text: &str) -> String {
        let audio_delivery = client
            .runtime_initialization
            .as_ref()
            .unwrap()
            .audio_deliveries[0]
            .clone();
        let request_id = client.next_request_id();
        write_request(
            client.writer().unwrap(),
            &request_id,
            "synthesis.start",
            json!({
                "text": text,
                "audio_delivery": audio_delivery,
                "utterance_options": {},
            }),
        )
        .unwrap();
        request_id
    }

    #[cfg(unix)]
    fn runtime_worker(executable: PathBuf, directory: &Path) -> (Worker, Sender<WorkerCommand>) {
        let (commands, receiver) = crossbeam_channel::bounded(4);
        (
            Worker::new(
                WorkerLocation {
                    slug: "fake".into(),
                    executable,
                    data: directory.join("data"),
                    cache: directory.join("cache"),
                },
                runtime_tts(),
                30,
                receiver,
                RodioAudio::new().unwrap(),
            ),
            commands,
        )
    }

    #[cfg(unix)]
    #[test]
    fn fake_runtime_complete_frames_are_response_then_correlated_wav() {
        let directory = tempfile::tempdir().unwrap();
        let Some(executable) = write_runtime_provider(directory.path(), "complete") else {
            return;
        };
        let mut client = runtime_client(
            &executable,
            directory.path(),
            super::super::DeliveryMode::Complete,
        );
        let request_id = begin_synthesis(&mut client, "complete");

        let Frame::Control(response) = next_provider_frame(&client) else {
            panic!("complete audio arrived before its response metadata");
        };
        let audio = response_result_for(&response, &request_id).unwrap()["audio"].clone();
        validate_audio_metadata(&audio, "audio/wav;codec=pcm_s16le").unwrap();
        let Frame::Audio(wav) = next_provider_frame(&client) else {
            panic!("complete response was not followed by audio");
        };
        assert_eq!(audio["byte_length"].as_u64(), Some(wav.len() as u64));
        validate_pcm_wav(&wav, &audio).unwrap();
        client.shutdown().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn fake_runtime_incremental_frames_are_begin_pcm_then_correlated_terminal() {
        let directory = tempfile::tempdir().unwrap();
        let Some(executable) = write_runtime_provider(directory.path(), "incremental") else {
            return;
        };
        let mut client = runtime_client(
            &executable,
            directory.path(),
            super::super::DeliveryMode::Incremental,
        );
        let request_id = begin_synthesis(&mut client, "incremental");

        let Frame::Control(begin) = next_provider_frame(&client) else {
            panic!("incremental audio arrived before audio_begin");
        };
        assert_eq!(begin["kind"], "event");
        assert_eq!(begin["event"], "synthesis.audio_begin");
        assert_eq!(begin["params"]["request_id"], request_id);
        assert_eq!(begin["params"]["sample_rate_hz"], 8_000);
        assert_eq!(begin["params"]["channels"], 1);

        let mut bytes = 0_u64;
        for _ in 0..2 {
            let Frame::Audio(pcm) = next_provider_frame(&client) else {
                panic!("incremental PCM frame was replaced by control data");
            };
            assert!(!pcm.is_empty() && pcm.len().is_multiple_of(2));
            bytes += pcm.len() as u64;
        }
        let Frame::Control(terminal) = next_provider_frame(&client) else {
            panic!("incremental stream omitted its terminal response");
        };
        let audio = &response_result_for(&terminal, &request_id).unwrap()["audio"];
        validate_audio_metadata(audio, "audio/pcm;codec=pcm_s16le").unwrap();
        assert_eq!(audio["byte_length"].as_u64(), Some(bytes));
        assert_eq!(audio["frame_count"].as_u64(), Some(2));
        client.shutdown().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn fake_runtime_rejects_incremental_request_correlation_mismatch() {
        let directory = tempfile::tempdir().unwrap();
        let Some(executable) = write_runtime_provider(directory.path(), "incremental") else {
            return;
        };
        let (mut worker, _commands) = runtime_worker(executable, directory.path());
        worker.start_client().unwrap();
        let request_id = begin_synthesis(worker.client.as_mut().unwrap(), "wrong-correlation");
        let mut completion = None;
        let error = worker
            .receive_incremental(
                &request_id,
                "audio/pcm;codec=pcm_s16le",
                Instant::now() + Duration::from_secs(2),
                1.0,
                &OutputTarget::SystemDefault,
                &mut completion,
            )
            .unwrap_err();
        assert!(
            matches!(error, ProviderError::Protocol(message) if message.contains("request ID mismatch"))
        );
        let mut client = worker.client.take().unwrap();
        client.terminate();
    }

    #[cfg(unix)]
    #[test]
    fn fake_runtime_cancellation_correlates_both_terminal_responses() {
        let directory = tempfile::tempdir().unwrap();
        let Some(executable) = write_runtime_provider(directory.path(), "incremental") else {
            return;
        };
        let (mut worker, _commands) = runtime_worker(executable, directory.path());
        worker.start_client().unwrap();
        let request_id = begin_synthesis(worker.client.as_mut().unwrap(), "cancel");
        worker.active_request_id = Some(request_id.clone());
        worker.active_playback_id = Some(Uuid::new_v4());
        let Frame::Control(begin) = next_provider_frame(worker.client.as_ref().unwrap()) else {
            panic!("cancellable synthesis omitted audio_begin");
        };
        assert_eq!(begin["params"]["request_id"], request_id);

        worker.cancel_current().unwrap();
        assert!(
            worker.client.is_some(),
            "successful cancellation killed the client"
        );
        worker.active_request_id = None;
        worker.active_playback_id = None;
        worker.client.take().unwrap().shutdown().unwrap();
    }

    #[cfg(unix)]
    fn cancellation_worker(text: &str) -> Option<(Worker, String)> {
        let directory = tempfile::tempdir().unwrap();
        let executable = write_runtime_provider(directory.path(), "incremental")?;
        let (mut worker, _commands) = runtime_worker(executable, directory.path());
        worker.start_client().unwrap();
        let request_id = begin_synthesis(worker.client.as_mut().unwrap(), text);
        worker.active_request_id = Some(request_id.clone());
        worker.active_playback_id = Some(Uuid::new_v4());
        let Frame::Control(begin) = next_provider_frame(worker.client.as_ref().unwrap()) else {
            panic!("cancellable synthesis omitted audio_begin");
        };
        assert_eq!(begin["params"]["request_id"], request_id);
        Some((worker, request_id))
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_lost_race_requires_terminal_before_false_response() {
        let Some((mut worker, _request_id)) = cancellation_worker("cancel-terminal-first") else {
            return;
        };
        worker.cancel_current().unwrap();
        worker.active_request_id = None;
        worker.active_playback_id = None;
        worker.client.take().unwrap().shutdown().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_rejects_audio_after_acceptance() {
        let Some((mut worker, _request_id)) = cancellation_worker("cancel-audio-after") else {
            return;
        };
        let error = worker.cancel_current().unwrap_err();
        assert!(
            matches!(error, ProviderError::Protocol(message) if message.contains("audio after accepting"))
        );
        worker.client.as_mut().unwrap().terminate();
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_rejects_acceptance_after_original_terminal() {
        let Some((mut worker, _request_id)) = cancellation_worker("cancel-original-first") else {
            return;
        };
        let error = worker.cancel_current().unwrap_err();
        assert!(
            matches!(error, ProviderError::Protocol(message) if message.contains("after the synthesis terminal"))
        );
        worker.client.as_mut().unwrap().terminate();
    }

    #[cfg(unix)]
    #[test]
    fn fake_runtime_allows_exactly_one_process_restart() {
        let directory = tempfile::tempdir().unwrap();
        let Some(executable) = write_runtime_provider(directory.path(), "complete") else {
            return;
        };
        let (mut worker, _commands) = runtime_worker(executable, directory.path());
        worker.start_client().unwrap();
        assert!(!worker.restart_used);

        let mut first = worker.client.take().unwrap();
        first.terminate();
        worker.ensure_client_for_speech().unwrap();
        assert!(worker.restart_used);
        assert_eq!(
            fs::read_to_string(directory.path().join("starts"))
                .unwrap()
                .lines()
                .count(),
            2
        );

        let mut second = worker.client.take().unwrap();
        second.terminate();
        assert!(worker.ensure_client_for_speech().is_err());
        assert_eq!(
            fs::read_to_string(directory.path().join("starts"))
                .unwrap()
                .lines()
                .count(),
            2,
            "restart budget spawned a third provider"
        );
    }
}
