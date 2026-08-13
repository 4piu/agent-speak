use std::collections::HashMap;

use serde_json::{Map, Value};
use uuid::Uuid;

use super::{
    AudioAdapter, CompletionNotifier, OutputTarget, PlaybackBackend, PlaybackError, PlaybackJob,
    PlaybackSource, RodioAudio,
};

#[cfg(any(windows, target_os = "macos"))]
use super::PreparedAudio;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TtsCapabilities {
    pub voice_id: Option<String>,
    pub completion_observable: bool,
    pub stoppable: bool,
    pub volume_controllable: bool,
}

/// Read-only metadata for a system voice exposed to applications.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SystemVoice {
    pub id: String,
    pub display_name: String,
    pub language: String,
    pub description: String,
    pub gender: String,
    pub is_default: bool,
}

pub trait TtsAdapter: 'static {
    fn capabilities(&self) -> TtsCapabilities;

    fn speak(
        &mut self,
        text: String,
        gain: f32,
        completion: CompletionNotifier,
    ) -> Result<(), PlaybackError>;

    fn speak_to(
        &mut self,
        text: String,
        gain: f32,
        target: &OutputTarget,
        completion: CompletionNotifier,
    ) -> Result<(), PlaybackError> {
        match target {
            OutputTarget::SystemDefault => self.speak(text, gain, completion),
            OutputTarget::DeviceId(_) => Err(PlaybackError::OutputUnavailable(
                "system TTS does not support explicit output routing".into(),
            )),
        }
    }

    fn speak_with_options_to(
        &mut self,
        text: String,
        utterance_options: Map<String, Value>,
        gain: f32,
        target: &OutputTarget,
        completion: CompletionNotifier,
    ) -> Result<(), PlaybackError> {
        if !utterance_options.is_empty() {
            return Err(PlaybackError::Backend(
                "utterance options are unavailable for this TTS provider".into(),
            ));
        }
        self.speak_to(text, gain, target, completion)
    }

    fn stop(&mut self, playback_id: Uuid) -> Result<(), PlaybackError>;

    fn finished(&mut self, _playback_id: Uuid) {}
}

/// Dispatches audio files and speech while retaining per-playback ownership.
pub struct SystemBackend<A, T> {
    audio: Option<A>,
    tts: Option<T>,
    active_kinds: HashMap<Uuid, ActiveKind>,
}

#[derive(Clone, Copy)]
enum ActiveKind {
    Audio,
    Speech,
}

impl<A, T> SystemBackend<A, T> {
    pub fn new(audio: Option<A>, tts: Option<T>) -> Self {
        Self {
            audio,
            tts,
            active_kinds: HashMap::new(),
        }
    }

    pub fn tts_capabilities(&self) -> Option<TtsCapabilities>
    where
        T: TtsAdapter,
    {
        self.tts.as_ref().map(TtsAdapter::capabilities)
    }
}

impl<A, T> PlaybackBackend for SystemBackend<A, T>
where
    A: AudioAdapter,
    T: TtsAdapter,
{
    fn start(
        &mut self,
        job: PlaybackJob,
        completion: CompletionNotifier,
    ) -> Result<(), PlaybackError> {
        let playback_id = job.id;
        let output_target = job.output_target;
        let result = match job.source {
            PlaybackSource::Audio(source) => self
                .audio
                .as_mut()
                .ok_or_else(|| PlaybackError::Backend("audio playback is disabled".into()))?
                .play_to(source, job.gain, &output_target, completion)
                .map(|()| ActiveKind::Audio),
            PlaybackSource::Speech {
                text,
                utterance_options,
            } => self
                .tts
                .as_mut()
                .ok_or_else(|| PlaybackError::Backend("system TTS is disabled".into()))?
                .speak_with_options_to(
                    text,
                    utterance_options,
                    job.gain,
                    &output_target,
                    completion,
                )
                .map(|()| ActiveKind::Speech),
        };
        match result {
            Ok(kind) => {
                self.active_kinds.insert(playback_id, kind);
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    fn stop(&mut self, playback_id: Uuid) -> Result<(), PlaybackError> {
        let result = match self.active_kinds.get(&playback_id).copied() {
            Some(ActiveKind::Audio) => self
                .audio
                .as_mut()
                .ok_or_else(|| PlaybackError::Backend("audio playback is disabled".into()))?
                .stop(playback_id),
            Some(ActiveKind::Speech) => self
                .tts
                .as_mut()
                .ok_or_else(|| PlaybackError::Backend("system TTS is disabled".into()))?
                .stop(playback_id),
            None => Ok(()),
        };
        if result.is_ok() {
            self.active_kinds.remove(&playback_id);
        }
        result
    }

    fn finished(&mut self, playback_id: Uuid) {
        match self.active_kinds.remove(&playback_id) {
            Some(ActiveKind::Audio) => {
                if let Some(audio) = self.audio.as_mut() {
                    audio.finished(playback_id);
                }
            }
            Some(ActiveKind::Speech) => {
                if let Some(tts) = self.tts.as_mut() {
                    tts.finished(playback_id);
                }
            }
            None => {}
        }
    }

    fn shutdown(&mut self) -> Result<(), PlaybackError> {
        let active = self.active_kinds.keys().copied().collect::<Vec<_>>();
        let mut first_error = None;
        for playback_id in active {
            if let Err(error) = self.stop(playback_id)
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

pub type NativeSystemBackend = SystemBackend<RodioAudio, SystemTts>;

impl NativeSystemBackend {
    /// Initialize default-device audio and optional system TTS.
    ///
    /// Call this from the `PlaybackHandle::spawn` factory. Windows media/TTS
    /// objects remain actor-owned; macOS bridges AVSpeech work to the serviced
    /// process main thread and returns bounded WAV bytes to this actor. All
    /// rendered audio is handed to the server's shared output service.
    pub fn initialize(
        audio_enabled: bool,
        tts_enabled: bool,
        voice_id: Option<&str>,
    ) -> Result<Self, PlaybackError> {
        let output = (audio_enabled || tts_enabled)
            .then(RodioAudio::new)
            .transpose()?;
        let audio = audio_enabled.then(|| {
            output
                .as_ref()
                .expect("enabled output service")
                .shared_client()
        });
        let tts = if tts_enabled {
            Some(SystemTts::new_with_audio(
                voice_id,
                output
                    .as_ref()
                    .expect("enabled output service")
                    .shared_client(),
            )?)
        } else {
            None
        };
        Ok(Self::new(audio, tts))
    }
}

#[cfg(windows)]
mod native {
    use windows::{
        Media::SpeechSynthesis::{SpeechSynthesizer, VoiceGender},
        Storage::Streams::DataReader,
        core::{Error as WinRtError, HSTRING},
    };

    use super::*;

    /// Bounds memory committed before handing synthesized WAV data to Rodio.
    /// The public text limit provides a much tighter practical bound; this is
    /// a final defense against an unexpectedly large platform stream.
    const MAX_SYNTHESIZED_AUDIO_BYTES: u64 = 256 * 1024 * 1024;

    /// Enumerate voices available through the same WinRT API used for speech.
    pub fn list_system_voices() -> Result<Vec<SystemVoice>, PlaybackError> {
        let installed = SpeechSynthesizer::AllVoices().map_err(|error| {
            winrt_error("installed system TTS voices could not be enumerated", error)
        })?;
        let mut voices = Vec::new();
        for voice in installed {
            let id = voice
                .Id()
                .map(|value| value.to_string())
                .map_err(|error| winrt_error("an installed voice has no readable id", error))?;
            let display_name = voice
                .DisplayName()
                .map(|value| value.to_string())
                .map_err(|error| winrt_error("an installed voice has no readable name", error))?;
            let language = voice
                .Language()
                .map(|value| value.to_string())
                .map_err(|error| {
                    winrt_error("an installed voice has no readable language", error)
                })?;
            let description =
                voice
                    .Description()
                    .map(|value| value.to_string())
                    .map_err(|error| {
                        winrt_error("an installed voice has no readable description", error)
                    })?;
            let gender = match voice
                .Gender()
                .map_err(|error| winrt_error("an installed voice has no readable gender", error))?
            {
                VoiceGender::Male => "male",
                VoiceGender::Female => "female",
                _ => "unknown",
            }
            .to_owned();
            voices.push(SystemVoice {
                is_default: false,
                id,
                display_name,
                language,
                description,
                gender,
            });
        }
        if voices.is_empty() {
            return Ok(voices);
        }
        let default_id = SpeechSynthesizer::DefaultVoice()
            .and_then(|voice| voice.Id())
            .map(|id| id.to_string())
            .map_err(|error| {
                winrt_error("default system TTS voice could not be inspected", error)
            })?;
        for voice in &mut voices {
            voice.is_default = voice.id == default_id;
        }
        voices.sort_by(|left, right| {
            left.language
                .cmp(&right.language)
                .then_with(|| left.display_name.cmp(&right.display_name))
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(voices)
    }

    pub struct SystemTts {
        engine: SpeechSynthesizer,
        audio: RodioAudio,
        capabilities: TtsCapabilities,
    }

    impl SystemTts {
        pub fn new(voice_id: Option<&str>) -> Result<Self, PlaybackError> {
            Self::new_with_audio(voice_id, RodioAudio::new()?)
        }

        pub(crate) fn new_with_audio(
            voice_id: Option<&str>,
            audio: RodioAudio,
        ) -> Result<Self, PlaybackError> {
            let engine = SpeechSynthesizer::new()
                .map_err(|error| winrt_error("system TTS could not be initialized", error))?;

            if let Some(requested_id) = voice_id.filter(|id| !id.is_empty()) {
                let voices = SpeechSynthesizer::AllVoices().map_err(|error| {
                    winrt_error("installed system TTS voices could not be enumerated", error)
                })?;
                let mut requested_voice = None;
                for voice in voices {
                    let id = voice.Id().map_err(|error| {
                        winrt_error("an installed system TTS voice has no readable id", error)
                    })?;
                    if id == requested_id {
                        requested_voice = Some(voice);
                        break;
                    }
                }
                let requested_voice = requested_voice.ok_or_else(|| {
                    PlaybackError::Backend(format!(
                        "configured system TTS voice was not found: {requested_id}"
                    ))
                })?;
                engine.SetVoice(&requested_voice).map_err(|error| {
                    winrt_error("configured system TTS voice could not be selected", error)
                })?;
            }

            let selected_voice = engine
                .Voice()
                .and_then(|voice| voice.Id())
                .map(|id| id.to_string())
                .map_err(|error| {
                    winrt_error("selected system TTS voice could not be inspected", error)
                })?;

            Ok(Self {
                engine,
                // The shared output service opens endpoints lazily on first use.
                audio,
                capabilities: TtsCapabilities {
                    voice_id: Some(selected_voice),
                    completion_observable: true,
                    stoppable: true,
                    volume_controllable: true,
                },
            })
        }

        fn synthesize(&self, text: String) -> Result<PreparedAudio, PlaybackError> {
            let stream = self
                .engine
                .SynthesizeTextToStreamAsync(&HSTRING::from(text))
                .map_err(|error| winrt_error("system TTS synthesis could not be started", error))?
                .get()
                .map_err(|error| winrt_error("system TTS synthesis failed", error))?;
            let byte_length = stream.Size().map_err(|error| {
                winrt_error("synthesized speech stream size is unavailable", error)
            })?;
            let byte_length = checked_stream_length(byte_length)?;
            let input = stream.GetInputStreamAt(0).map_err(|error| {
                winrt_error("synthesized speech stream could not be read", error)
            })?;
            let reader = DataReader::CreateDataReader(&input).map_err(|error| {
                winrt_error("synthesized speech reader could not be created", error)
            })?;
            let loaded = reader
                .LoadAsync(byte_length)
                .map_err(|error| {
                    winrt_error("synthesized speech stream load could not be started", error)
                })?
                .get()
                .map_err(|error| winrt_error("synthesized speech stream load failed", error))?;
            if loaded != byte_length {
                return Err(PlaybackError::Backend(format!(
                    "system TTS produced an incomplete audio stream: expected {byte_length} bytes, received {loaded}"
                )));
            }
            let mut wav = vec![0; loaded as usize];
            reader.ReadBytes(&mut wav).map_err(|error| {
                winrt_error("synthesized speech bytes could not be read", error)
            })?;
            PreparedAudio::from_memory(wav)
        }

        fn speak_inner(
            &mut self,
            text: String,
            gain: f32,
            target: &OutputTarget,
            completion: CompletionNotifier,
        ) -> Result<(), PlaybackError> {
            let source = self.synthesize(text)?;
            self.audio.play_to(source, gain, target, completion)
        }
    }

    impl TtsAdapter for SystemTts {
        fn capabilities(&self) -> TtsCapabilities {
            self.capabilities.clone()
        }

        fn speak(
            &mut self,
            text: String,
            gain: f32,
            completion: CompletionNotifier,
        ) -> Result<(), PlaybackError> {
            self.speak_inner(text, gain, &OutputTarget::SystemDefault, completion)
        }

        fn speak_to(
            &mut self,
            text: String,
            gain: f32,
            target: &OutputTarget,
            completion: CompletionNotifier,
        ) -> Result<(), PlaybackError> {
            self.speak_inner(text, gain, target, completion)
        }

        fn stop(&mut self, playback_id: Uuid) -> Result<(), PlaybackError> {
            self.audio.stop(playback_id)
        }

        fn finished(&mut self, playback_id: Uuid) {
            self.audio.finished(playback_id);
        }
    }

    fn checked_stream_length(byte_length: u64) -> Result<u32, PlaybackError> {
        let byte_length = u32::try_from(byte_length).map_err(|_| {
            PlaybackError::Backend("system TTS produced an audio stream larger than 4 GiB".into())
        })?;
        if u64::from(byte_length) > MAX_SYNTHESIZED_AUDIO_BYTES {
            return Err(PlaybackError::Backend(format!(
                "system TTS produced an audio stream larger than the {} MiB internal safety limit",
                MAX_SYNTHESIZED_AUDIO_BYTES / (1024 * 1024)
            )));
        }
        Ok(byte_length)
    }

    fn winrt_error(context: &str, error: WinRtError) -> PlaybackError {
        PlaybackError::Backend(format!("{context}: {error}"))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn synthesized_stream_length_is_bounded_before_allocation() {
            assert_eq!(
                checked_stream_length(MAX_SYNTHESIZED_AUDIO_BYTES).unwrap(),
                MAX_SYNTHESIZED_AUDIO_BYTES as u32
            );
            assert!(checked_stream_length(MAX_SYNTHESIZED_AUDIO_BYTES + 1).is_err());
            assert!(checked_stream_length(u64::from(u32::MAX) + 1).is_err());
        }
    }
}

#[cfg(windows)]
pub use native::{SystemTts, list_system_voices};

#[cfg(target_os = "macos")]
mod macos {
    use std::{
        cell::RefCell,
        collections::HashMap,
        ptr::NonNull,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
            mpsc,
        },
        time::Duration,
    };

    use block2::RcBlock;
    use dispatch2::DispatchQueue;
    use objc2::rc::{Retained, autoreleasepool};
    use objc2_avf_audio::{
        AVAudioBuffer, AVAudioCommonFormat, AVAudioPCMBuffer, AVSpeechBoundary,
        AVSpeechSynthesisVoice, AVSpeechSynthesisVoiceGender, AVSpeechSynthesisVoiceQuality,
        AVSpeechSynthesizer, AVSpeechUtterance,
    };
    use objc2_foundation::NSString;

    use super::*;

    /// Bounds all synthesized PCM and its WAV header before Rodio receives it.
    /// The public text ceiling is much tighter in normal use; this remains the
    /// final defense against unexpectedly large output from the platform API.
    const MAX_SYNTHESIZED_AUDIO_BYTES: usize = 256 * 1024 * 1024;
    const WAV_HEADER_BYTES: usize = 44;
    const MAX_SYNTHESIS_SECONDS: u64 = 120;
    const MAX_SYNTHESIS_CHANNELS: usize = 32;
    const MAIN_THREAD_RESPONSE_SECONDS: u64 = 2;
    const MAIN_THREAD_STARTUP_SECONDS: u64 = 10;

    static NEXT_SYNTHESIS_ID: AtomicU64 = AtomicU64::new(1);

    thread_local! {
        static MAIN_SYNTHESIS: RefCell<HashMap<u64, MainSynthesis>> = RefCell::new(HashMap::new());
    }

    /// Enumerate the same AVSpeech voices used by the synthesis backend.
    pub fn list_system_voices() -> Result<Vec<SystemVoice>, PlaybackError> {
        run_on_main(
            list_system_voices_on_main,
            Duration::from_secs(MAIN_THREAD_STARTUP_SECONDS),
            "macOS voice enumeration did not run on the main thread",
        )?
    }

    fn list_system_voices_on_main() -> Result<Vec<SystemVoice>, PlaybackError> {
        autoreleasepool(|_| list_system_voices_in_pool())
    }

    fn list_system_voices_in_pool() -> Result<Vec<SystemVoice>, PlaybackError> {
        // SAFETY: All AVSpeech objects and their non-atomic accessors remain on
        // the process main thread for the duration of enumeration.
        unsafe {
            let installed = AVSpeechSynthesisVoice::speechVoices();
            let default_id = AVSpeechSynthesisVoice::voiceWithLanguage(None)
                .map(|voice| voice.identifier().to_string());
            let mut voices = Vec::with_capacity(installed.count());
            for index in 0..installed.count() {
                let voice = installed.objectAtIndex(index);
                let quality = quality_name(voice.quality());
                voices.push(SystemVoice {
                    id: voice.identifier().to_string(),
                    display_name: voice.name().to_string(),
                    language: voice.language().to_string(),
                    description: format!("{quality} quality macOS voice"),
                    gender: gender_name(voice.gender()).to_owned(),
                    is_default: default_id
                        .as_deref()
                        .is_some_and(|id| id == voice.identifier().to_string()),
                });
            }
            voices.sort_by(|left, right| {
                left.language
                    .cmp(&right.language)
                    .then_with(|| left.display_name.cmp(&right.display_name))
                    .then_with(|| left.id.cmp(&right.id))
            });
            Ok(voices)
        }
    }

    pub struct SystemTts {
        voice_id: String,
        audio: RodioAudio,
        capabilities: TtsCapabilities,
        synthesis_healthy: bool,
    }

    impl SystemTts {
        pub fn new(voice_id: Option<&str>) -> Result<Self, PlaybackError> {
            Self::new_with_audio(voice_id, RodioAudio::new()?)
        }

        pub(crate) fn new_with_audio(
            voice_id: Option<&str>,
            audio: RodioAudio,
        ) -> Result<Self, PlaybackError> {
            let requested_voice = voice_id.filter(|id| !id.is_empty()).map(str::to_owned);
            let selected_voice = run_on_main(
                move || resolve_voice_on_main(requested_voice.as_deref()),
                Duration::from_secs(MAIN_THREAD_STARTUP_SECONDS),
                "macOS voice selection did not run on the main thread",
            )??;

            Ok(Self {
                voice_id: selected_voice.clone(),
                // The shared output service opens the endpoint only after synthesis.
                audio,
                capabilities: TtsCapabilities {
                    voice_id: Some(selected_voice),
                    completion_observable: true,
                    stoppable: true,
                    volume_controllable: true,
                },
                synthesis_healthy: true,
            })
        }

        fn synthesize(&mut self, text: String) -> Result<PreparedAudio, PlaybackError> {
            if !self.synthesis_healthy {
                return Err(PlaybackError::Backend(
                    "macOS system TTS is unavailable after an incomplete synthesis".into(),
                ));
            }
            let text_characters = text.chars().count() as u64;
            let synthesis_id = NEXT_SYNTHESIS_ID.fetch_add(1, Ordering::Relaxed);
            let collector = Arc::new(Mutex::new(WavCollector::new()));
            let (result_tx, result_rx) = mpsc::sync_channel(1);
            let callback_collector = collector.clone();
            let voice_id = self.voice_id.clone();
            DispatchQueue::main().exec_async(move || {
                start_main_synthesis(synthesis_id, text, voice_id, callback_collector, result_tx);
            });

            let timeout_seconds = (15 + text_characters / 10).min(MAX_SYNTHESIS_SECONDS);
            let result = result_rx.recv_timeout(Duration::from_secs(timeout_seconds));
            match result {
                Ok(Ok(wav)) => PreparedAudio::from_memory(wav),
                Ok(Err(error)) => {
                    self.synthesis_healthy = false;
                    Err(error)
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    collector
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .finish();
                    self.cancel_incomplete_synthesis(synthesis_id);
                    Err(PlaybackError::Backend(
                        "macOS system TTS synthesis timed out".into(),
                    ))
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.cancel_incomplete_synthesis(synthesis_id);
                    Err(PlaybackError::Backend(
                        "macOS system TTS ended without an audio result".into(),
                    ))
                }
            }
        }

        fn cancel_incomplete_synthesis(&mut self, synthesis_id: u64) {
            self.synthesis_healthy = false;
            let _ = cancel_main_synthesis(synthesis_id);
        }

        fn speak_inner(
            &mut self,
            text: String,
            gain: f32,
            target: &OutputTarget,
            completion: CompletionNotifier,
        ) -> Result<(), PlaybackError> {
            let source = self.synthesize(text)?;
            self.audio.play_to(source, gain, target, completion)
        }
    }

    struct MainSynthesis {
        engine: Retained<AVSpeechSynthesizer>,
        utterance: Retained<AVSpeechUtterance>,
        callback: RcBlock<dyn Fn(NonNull<AVAudioBuffer>)>,
    }

    fn resolve_voice_on_main(voice_id: Option<&str>) -> Result<String, PlaybackError> {
        autoreleasepool(|_| {
            // SAFETY: Voice selection and inspection occur on the process main
            // thread inside a bounded autorelease pool.
            unsafe {
                let voice = match voice_id {
                    Some(requested_id) => {
                        let requested = NSString::from_str(requested_id);
                        AVSpeechSynthesisVoice::voiceWithIdentifier(&requested).ok_or_else(
                            || {
                                PlaybackError::Backend(format!(
                                    "configured system TTS voice was not found: {requested_id}"
                                ))
                            },
                        )?
                    }
                    None => AVSpeechSynthesisVoice::voiceWithLanguage(None).ok_or_else(|| {
                        PlaybackError::Backend(
                            "no default macOS system TTS voice is available".into(),
                        )
                    })?,
                };
                Ok(voice.identifier().to_string())
            }
        })
    }

    fn run_on_main<T, F>(
        work: F,
        timeout: Duration,
        timeout_message: &'static str,
    ) -> Result<T, PlaybackError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        // SAFETY: This call only reads pthread identity state.
        if unsafe { libc::pthread_main_np() } == 1 {
            return Ok(work());
        }
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        DispatchQueue::main().exec_async(move || {
            let _ = result_tx.try_send(work());
        });
        result_rx
            .recv_timeout(timeout)
            .map_err(|_| PlaybackError::Backend(timeout_message.into()))
    }

    fn start_main_synthesis(
        synthesis_id: u64,
        text: String,
        voice_id: String,
        collector: Arc<Mutex<WavCollector>>,
        result_tx: mpsc::SyncSender<Result<Vec<u8>, PlaybackError>>,
    ) {
        autoreleasepool(|_| {
            let voice_id = NSString::from_str(&voice_id);
            let utterance_text = NSString::from_str(&text);
            // SAFETY: This closure and all retained AVSpeech state execute on
            // the serviced process main thread.
            let Some(voice) = (unsafe { AVSpeechSynthesisVoice::voiceWithIdentifier(&voice_id) })
            else {
                let _ = result_tx.try_send(Err(PlaybackError::Backend(
                    "the selected macOS system TTS voice became unavailable".into(),
                )));
                return;
            };
            // SAFETY: Construction occurs on the serviced process main thread.
            let engine = unsafe { AVSpeechSynthesizer::new() };
            let utterance = unsafe {
                let utterance = AVSpeechUtterance::speechUtteranceWithString(&utterance_text);
                utterance.setVoice(Some(&voice));
                // Gain is applied exactly once by Rodio on the selected target.
                utterance.setVolume(1.0);
                utterance
            };
            let callback_collector = collector.clone();
            let callback_tx = result_tx.clone();
            let callback: RcBlock<dyn Fn(NonNull<AVAudioBuffer>)> =
                RcBlock::new(move |buffer: NonNull<AVAudioBuffer>| {
                    let terminal = {
                        let mut collector = callback_collector
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        // SAFETY: AVSpeech guarantees the callback's buffer is
                        // valid for this invocation. Samples are copied before
                        // the callback returns.
                        unsafe { collector.consume(buffer.as_ref()) }
                    };
                    if let Some(result) = terminal {
                        let cancel = result.is_err();
                        let result_tx = callback_tx.clone();
                        DispatchQueue::main().exec_async(move || {
                            finish_main_synthesis(synthesis_id, cancel);
                            let _ = result_tx.try_send(result);
                        });
                    }
                });

            MAIN_SYNTHESIS.with(|active| {
                active.borrow_mut().insert(
                    synthesis_id,
                    MainSynthesis {
                        engine,
                        utterance,
                        callback,
                    },
                );
            });
            MAIN_SYNTHESIS.with(|active| {
                let active = active.borrow();
                let synthesis = active
                    .get(&synthesis_id)
                    .expect("macOS synthesis disappeared before it started");
                // SAFETY: The callback block and utterance are retained in the
                // main-thread registry until terminal cleanup.
                unsafe {
                    synthesis.engine.writeUtterance_toBufferCallback(
                        &synthesis.utterance,
                        RcBlock::as_ptr(&synthesis.callback),
                    );
                }
            });
        });
    }

    fn finish_main_synthesis(synthesis_id: u64, cancel: bool) {
        let synthesis = MAIN_SYNTHESIS.with(|active| active.borrow_mut().remove(&synthesis_id));
        if cancel && let Some(synthesis) = synthesis.as_ref() {
            // SAFETY: Main-thread-owned synthesizer; immediate stop occurs
            // before the terminal result reaches the playback actor.
            unsafe {
                synthesis
                    .engine
                    .stopSpeakingAtBoundary(AVSpeechBoundary::Immediate);
            }
        }
    }

    fn cancel_main_synthesis(synthesis_id: u64) -> Result<(), PlaybackError> {
        let (cancel_tx, cancel_rx) = mpsc::sync_channel(1);
        DispatchQueue::main().exec_async(move || {
            finish_main_synthesis(synthesis_id, true);
            let _ = cancel_tx.try_send(());
        });
        cancel_rx
            .recv_timeout(Duration::from_secs(MAIN_THREAD_RESPONSE_SECONDS))
            .map_err(|_| {
                PlaybackError::Backend(
                    "macOS system TTS cancellation was not confirmed on the main thread".into(),
                )
            })
    }

    impl TtsAdapter for SystemTts {
        fn capabilities(&self) -> TtsCapabilities {
            self.capabilities.clone()
        }

        fn speak(
            &mut self,
            text: String,
            gain: f32,
            completion: CompletionNotifier,
        ) -> Result<(), PlaybackError> {
            self.speak_inner(text, gain, &OutputTarget::SystemDefault, completion)
        }

        fn speak_to(
            &mut self,
            text: String,
            gain: f32,
            target: &OutputTarget,
            completion: CompletionNotifier,
        ) -> Result<(), PlaybackError> {
            self.speak_inner(text, gain, target, completion)
        }

        fn stop(&mut self, playback_id: Uuid) -> Result<(), PlaybackError> {
            self.audio.stop(playback_id)
        }

        fn finished(&mut self, playback_id: Uuid) {
            self.audio.finished(playback_id);
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum PcmEncoding {
        Float32,
        Int16,
        Int32,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct PcmFormat {
        sample_rate: u32,
        channels: u16,
        encoding: PcmEncoding,
        interleaved: bool,
    }

    struct WavCollector {
        bytes: Vec<u8>,
        format: Option<PcmFormat>,
        finished: bool,
    }

    impl WavCollector {
        fn new() -> Self {
            Self {
                bytes: vec![0; WAV_HEADER_BYTES],
                format: None,
                finished: false,
            }
        }

        fn finish(&mut self) {
            self.finished = true;
        }

        /// Copy one AVSpeech buffer and return a terminal result when the
        /// documented zero-frame sentinel arrives or validation fails.
        unsafe fn consume(
            &mut self,
            buffer: &AVAudioBuffer,
        ) -> Option<Result<Vec<u8>, PlaybackError>> {
            if self.finished {
                return None;
            }
            let Some(buffer) = buffer.downcast_ref::<AVAudioPCMBuffer>() else {
                return self.fail("macOS system TTS produced a non-PCM audio buffer");
            };
            // SAFETY: The buffer is live for this callback and all access is
            // confined to this callback invocation.
            let frame_length = unsafe { buffer.frameLength() } as usize;
            if frame_length == 0 {
                self.finished = true;
                return Some(self.finalize());
            }

            // SAFETY: AVAudioPCMBuffer owns this immutable format for the
            // duration of the callback.
            let audio_format = unsafe { buffer.format() };
            let sample_rate = unsafe { audio_format.sampleRate() };
            let channels = unsafe { audio_format.channelCount() } as usize;
            let interleaved = unsafe { audio_format.isInterleaved() };
            let encoding = match unsafe { audio_format.commonFormat() } {
                AVAudioCommonFormat::PCMFormatFloat32 => PcmEncoding::Float32,
                AVAudioCommonFormat::PCMFormatInt16 => PcmEncoding::Int16,
                AVAudioCommonFormat::PCMFormatInt32 => PcmEncoding::Int32,
                _ => {
                    return self.fail("macOS system TTS produced an unsupported PCM sample format");
                }
            };
            let Ok(sample_rate) = validate_sample_rate(sample_rate) else {
                return self.fail("macOS system TTS produced an invalid sample rate");
            };
            if channels == 0 || channels > MAX_SYNTHESIS_CHANNELS {
                return self.fail("macOS system TTS produced an invalid channel count");
            }
            let format = PcmFormat {
                sample_rate,
                channels: channels as u16,
                encoding,
                interleaved,
            };
            if self.format.is_some_and(|current| current != format) {
                return self.fail("macOS system TTS changed audio format during synthesis");
            }
            self.format = Some(format);

            let Some(samples) = frame_length.checked_mul(channels) else {
                return self.fail("macOS system TTS produced an oversized audio buffer");
            };
            let Some(additional_bytes) = samples.checked_mul(size_of::<i16>()) else {
                return self.fail("macOS system TTS produced an oversized audio buffer");
            };
            if self.bytes.len().saturating_add(additional_bytes) > MAX_SYNTHESIZED_AUDIO_BYTES {
                return self.fail("macOS system TTS exceeded the 256 MiB internal audio limit");
            }
            let stride = unsafe { buffer.stride() };
            let trailing_channel = if interleaved { channels - 1 } else { 0 };
            let last_sample_offset = frame_length
                .saturating_sub(1)
                .checked_mul(stride)
                .and_then(|offset| offset.checked_add(trailing_channel));
            if stride == 0 || (interleaved && stride < channels) || last_sample_offset.is_none() {
                return self.fail("macOS system TTS produced an invalid sample stride");
            }
            self.bytes.reserve(additional_bytes);

            for frame in 0..frame_length {
                for channel in 0..channels {
                    // SAFETY: AVAudioPCMBuffer documents `frameLength` samples
                    // separated by `stride`. Interleaved channels share the
                    // first sample block; planar channels use separate pointers.
                    // The common format selects the matching typed accessor.
                    let sample = unsafe {
                        pcm_sample(buffer, encoding, channel, frame, stride, interleaved)
                    };
                    let Some(sample) = sample else {
                        return self
                            .fail("macOS system TTS returned no data for its declared PCM format");
                    };
                    self.bytes.extend_from_slice(&sample.to_le_bytes());
                }
            }
            None
        }

        fn fail(&mut self, message: &str) -> Option<Result<Vec<u8>, PlaybackError>> {
            self.finished = true;
            Some(Err(PlaybackError::Backend(message.into())))
        }

        fn finalize(&mut self) -> Result<Vec<u8>, PlaybackError> {
            let format = self.format.ok_or_else(|| {
                PlaybackError::Backend("macOS system TTS produced no audio samples".into())
            })?;
            let data_length = u32::try_from(self.bytes.len() - WAV_HEADER_BYTES).map_err(|_| {
                PlaybackError::Backend("macOS system TTS WAV data is too large".into())
            })?;
            let riff_length = data_length.checked_add(36).ok_or_else(|| {
                PlaybackError::Backend("macOS system TTS WAV data is too large".into())
            })?;
            let block_align = format.channels.checked_mul(2).ok_or_else(|| {
                PlaybackError::Backend("macOS system TTS WAV format is invalid".into())
            })?;
            let byte_rate = format
                .sample_rate
                .checked_mul(u32::from(block_align))
                .ok_or_else(|| {
                    PlaybackError::Backend("macOS system TTS WAV format is invalid".into())
                })?;

            self.bytes[0..4].copy_from_slice(b"RIFF");
            self.bytes[4..8].copy_from_slice(&riff_length.to_le_bytes());
            self.bytes[8..12].copy_from_slice(b"WAVE");
            self.bytes[12..16].copy_from_slice(b"fmt ");
            self.bytes[16..20].copy_from_slice(&16_u32.to_le_bytes());
            self.bytes[20..22].copy_from_slice(&1_u16.to_le_bytes());
            self.bytes[22..24].copy_from_slice(&format.channels.to_le_bytes());
            self.bytes[24..28].copy_from_slice(&format.sample_rate.to_le_bytes());
            self.bytes[28..32].copy_from_slice(&byte_rate.to_le_bytes());
            self.bytes[32..34].copy_from_slice(&block_align.to_le_bytes());
            self.bytes[34..36].copy_from_slice(&16_u16.to_le_bytes());
            self.bytes[36..40].copy_from_slice(b"data");
            self.bytes[40..44].copy_from_slice(&data_length.to_le_bytes());
            Ok(std::mem::take(&mut self.bytes))
        }
    }

    unsafe fn pcm_sample(
        buffer: &AVAudioPCMBuffer,
        encoding: PcmEncoding,
        channel: usize,
        frame: usize,
        stride: usize,
        interleaved: bool,
    ) -> Option<i16> {
        let pointer_channel = if interleaved { 0 } else { channel };
        let offset =
            frame
                .checked_mul(stride)?
                .checked_add(if interleaved { channel } else { 0 })?;
        match encoding {
            PcmEncoding::Float32 => {
                let channels = unsafe { buffer.floatChannelData() };
                let pointer = unsafe { channel_pointer(channels, pointer_channel)? };
                let value = unsafe { *pointer.as_ptr().add(offset) };
                let value = if value.is_finite() {
                    value.clamp(-1.0, 1.0)
                } else {
                    0.0
                };
                Some((value * f32::from(i16::MAX)).round() as i16)
            }
            PcmEncoding::Int16 => {
                let channels = unsafe { buffer.int16ChannelData() };
                let pointer = unsafe { channel_pointer(channels, pointer_channel)? };
                Some(unsafe { *pointer.as_ptr().add(offset) })
            }
            PcmEncoding::Int32 => {
                let channels = unsafe { buffer.int32ChannelData() };
                let pointer = unsafe { channel_pointer(channels, pointer_channel)? };
                Some((unsafe { *pointer.as_ptr().add(offset) } >> 16) as i16)
            }
        }
    }

    unsafe fn channel_pointer<T>(channels: *mut NonNull<T>, channel: usize) -> Option<NonNull<T>> {
        if channels.is_null() {
            None
        } else {
            Some(unsafe { *channels.add(channel) })
        }
    }

    fn validate_sample_rate(sample_rate: f64) -> Result<u32, ()> {
        if !sample_rate.is_finite() || sample_rate <= 0.0 || sample_rate > f64::from(u32::MAX) {
            return Err(());
        }
        Ok(sample_rate.round() as u32)
    }

    fn quality_name(quality: AVSpeechSynthesisVoiceQuality) -> &'static str {
        match quality {
            AVSpeechSynthesisVoiceQuality::Enhanced => "enhanced",
            AVSpeechSynthesisVoiceQuality::Premium => "premium",
            AVSpeechSynthesisVoiceQuality::Default => "default",
            _ => "unknown",
        }
    }

    fn gender_name(gender: AVSpeechSynthesisVoiceGender) -> &'static str {
        match gender {
            AVSpeechSynthesisVoiceGender::Male => "male",
            AVSpeechSynthesisVoiceGender::Female => "female",
            _ => "unspecified",
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn wav_collector_builds_a_bounded_pcm_header() {
            let mut collector = WavCollector::new();
            collector.format = Some(PcmFormat {
                sample_rate: 22_050,
                channels: 1,
                encoding: PcmEncoding::Float32,
                interleaved: false,
            });
            collector.bytes.extend_from_slice(&123_i16.to_le_bytes());
            let wav = collector.finalize().unwrap();

            assert_eq!(&wav[0..4], b"RIFF");
            assert_eq!(&wav[8..12], b"WAVE");
            assert_eq!(&wav[22..24], &1_u16.to_le_bytes());
            assert_eq!(&wav[24..28], &22_050_u32.to_le_bytes());
            assert_eq!(&wav[40..44], &2_u32.to_le_bytes());
            assert_eq!(&wav[44..], &123_i16.to_le_bytes());
        }

        #[test]
        fn sample_rate_validation_rejects_non_finite_and_non_positive_values() {
            assert_eq!(validate_sample_rate(22_050.0), Ok(22_050));
            assert_eq!(validate_sample_rate(f64::NAN), Err(()));
            assert_eq!(validate_sample_rate(f64::INFINITY), Err(()));
            assert_eq!(validate_sample_rate(0.0), Err(()));
        }
    }
}

#[cfg(target_os = "macos")]
pub use macos::{SystemTts, list_system_voices};

/// Other platforms retain the backend seam without claiming a supported
/// native TTS implementation.
#[cfg(not(any(windows, target_os = "macos")))]
pub struct SystemTts;

#[cfg(not(any(windows, target_os = "macos")))]
pub fn list_system_voices() -> Result<Vec<SystemVoice>, PlaybackError> {
    Err(PlaybackError::Backend(
        if cfg!(target_os = "linux") {
            "Linux has no supported native system TTS; use a configured UtterPipe provider such as utterpipe-espeak-ng"
        } else {
            "system TTS voice discovery is not implemented on this platform"
        }
        .into(),
    ))
}

#[cfg(not(any(windows, target_os = "macos")))]
impl SystemTts {
    pub fn new(_voice_id: Option<&str>) -> Result<Self, PlaybackError> {
        Err(PlaybackError::Backend(
            if cfg!(target_os = "linux") {
                "Linux system TTS was removed from Agent Speak; install utterpipe-espeak-ng beside Agent Speak or on PATH and select provider = \"utterpipe-espeak-ng\""
            } else {
                "system TTS is not implemented on this platform"
            }
            .into(),
        ))
    }

    pub(crate) fn new_with_audio(
        voice_id: Option<&str>,
        _audio: RodioAudio,
    ) -> Result<Self, PlaybackError> {
        Self::new(voice_id)
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
impl TtsAdapter for SystemTts {
    fn capabilities(&self) -> TtsCapabilities {
        TtsCapabilities {
            voice_id: None,
            completion_observable: false,
            stoppable: false,
            volume_controllable: false,
        }
    }

    fn speak(
        &mut self,
        _text: String,
        _gain: f32,
        _completion: CompletionNotifier,
    ) -> Result<(), PlaybackError> {
        Err(PlaybackError::Backend(
            "system TTS is not implemented on this platform".into(),
        ))
    }

    fn stop(&mut self, _playback_id: Uuid) -> Result<(), PlaybackError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FailingTts;

    impl TtsAdapter for FailingTts {
        fn capabilities(&self) -> TtsCapabilities {
            TtsCapabilities {
                voice_id: Some("test-voice".into()),
                completion_observable: true,
                stoppable: true,
                volume_controllable: true,
            }
        }

        fn speak(
            &mut self,
            _text: String,
            _gain: f32,
            _completion: CompletionNotifier,
        ) -> Result<(), PlaybackError> {
            Err(PlaybackError::Backend(
                "simulated TTS synthesis failure".into(),
            ))
        }

        fn stop(&mut self, _playback_id: Uuid) -> Result<(), PlaybackError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn tts_start_failure_is_typed_and_does_not_close_actor() {
        let handle = crate::playback::PlaybackHandle::spawn(1, || {
            Ok(SystemBackend::<RodioAudio, FailingTts>::new(
                None,
                Some(FailingTts),
            ))
        })
        .unwrap();

        for _ in 0..2 {
            let result = handle
                .submit(
                    PlaybackJob::speech(uuid::Uuid::new_v4(), "test", 0.4),
                    crate::playback::ConcurrencyMode::Enqueue,
                )
                .await;
            assert_eq!(
                result,
                Err(PlaybackError::Backend(
                    "simulated TTS synthesis failure".into()
                ))
            );
        }
        handle.shutdown().await.unwrap();
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "manual spike: requires native system speech services"]
    fn spike_initializes_native_system_tts() {
        let tts = SystemTts::new(None).unwrap();
        let capabilities = tts.capabilities();
        assert!(capabilities.completion_observable);
        assert!(capabilities.stoppable);
        assert!(capabilities.volume_controllable);
    }
}
