use super::{
    AudioAdapter, CompletionNotifier, OutputTarget, PlaybackBackend, PlaybackError, PlaybackJob,
    PlaybackSource, PreparedAudio, RodioAudio,
};

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

    fn stop(&mut self) -> Result<(), PlaybackError>;

    fn finished(&mut self) {}
}

/// Dispatches audio files and speech through one logical non-mixing backend.
pub struct SystemBackend<A, T> {
    audio: Option<A>,
    tts: Option<T>,
    active_kind: Option<ActiveKind>,
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
            active_kind: None,
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
        let output_target = job.output_target;
        let result = match job.source {
            PlaybackSource::Audio(source) => self
                .audio
                .as_mut()
                .ok_or_else(|| PlaybackError::Backend("audio playback is disabled".into()))?
                .play_to(source, job.gain, &output_target, completion)
                .map(|()| ActiveKind::Audio),
            PlaybackSource::Speech(text) => self
                .tts
                .as_mut()
                .ok_or_else(|| PlaybackError::Backend("system TTS is disabled".into()))?
                .speak_to(text, job.gain, &output_target, completion)
                .map(|()| ActiveKind::Speech),
        };
        match result {
            Ok(kind) => {
                self.active_kind = Some(kind);
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    fn stop(&mut self) -> Result<(), PlaybackError> {
        match self.active_kind.take() {
            Some(ActiveKind::Audio) => self
                .audio
                .as_mut()
                .ok_or_else(|| PlaybackError::Backend("audio playback is disabled".into()))?
                .stop(),
            Some(ActiveKind::Speech) => self
                .tts
                .as_mut()
                .ok_or_else(|| PlaybackError::Backend("system TTS is disabled".into()))?
                .stop(),
            None => Ok(()),
        }
    }

    fn finished(&mut self) {
        match self.active_kind.take() {
            Some(ActiveKind::Audio) => {
                if let Some(audio) = self.audio.as_mut() {
                    audio.finished();
                }
            }
            Some(ActiveKind::Speech) => {
                if let Some(tts) = self.tts.as_mut() {
                    tts.finished();
                }
            }
            None => {}
        }
    }

    fn shutdown(&mut self) -> Result<(), PlaybackError> {
        self.stop()
    }
}

pub type NativeSystemBackend = SystemBackend<RodioAudio, SystemTts>;

impl NativeSystemBackend {
    /// Initialize default-device audio and optional system TTS.
    ///
    /// Call this from the `PlaybackHandle::spawn` factory so Windows media and
    /// TTS objects remain owned by the actor thread.
    pub fn initialize(
        audio_enabled: bool,
        tts_enabled: bool,
        voice_id: Option<&str>,
    ) -> Result<Self, PlaybackError> {
        let audio = if audio_enabled {
            Some(RodioAudio::new()?)
        } else {
            None
        };
        let tts = if tts_enabled {
            Some(SystemTts::new(voice_id)?)
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
                // RodioAudio opens its selected endpoint lazily on first use.
                // Constructing TTS therefore does not touch audio hardware.
                audio: RodioAudio::new()?,
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

        fn stop(&mut self) -> Result<(), PlaybackError> {
            self.audio.stop()
        }

        fn finished(&mut self) {
            self.audio.finished();
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

/// Compiling on non-Windows keeps the backend seam visible without claiming a
/// supported system TTS implementation for the Windows-first MVP.
#[cfg(not(windows))]
pub struct SystemTts;

#[cfg(not(windows))]
pub fn list_system_voices() -> Result<Vec<SystemVoice>, PlaybackError> {
    Err(PlaybackError::Backend(
        "system TTS voice discovery is not implemented on this platform".into(),
    ))
}

#[cfg(not(windows))]
impl SystemTts {
    pub fn new(_voice_id: Option<&str>) -> Result<Self, PlaybackError> {
        Err(PlaybackError::Backend(
            "system TTS is not implemented on this platform".into(),
        ))
    }
}

#[cfg(not(windows))]
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

    fn stop(&mut self) -> Result<(), PlaybackError> {
        Ok(())
    }
}

#[cfg(all(test, windows))]
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

        fn stop(&mut self) -> Result<(), PlaybackError> {
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

    #[test]
    #[ignore = "manual spike: requires Windows system speech services"]
    fn spike_initializes_windows_system_tts() {
        let tts = SystemTts::new(None).unwrap();
        let capabilities = tts.capabilities();
        assert!(capabilities.completion_observable);
        assert!(capabilities.stoppable);
        assert!(capabilities.volume_controllable);
    }
}
