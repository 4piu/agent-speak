use super::{
    AudioAdapter, CompletionNotifier, PlaybackBackend, PlaybackError, PlaybackJob, PlaybackSource,
    RodioAudio,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TtsCapabilities {
    pub voice_id: Option<String>,
    pub completion_observable: bool,
    pub stoppable: bool,
    pub volume_controllable: bool,
}

pub trait TtsAdapter: 'static {
    fn capabilities(&self) -> TtsCapabilities;

    fn speak(
        &mut self,
        text: String,
        gain: f32,
        completion: CompletionNotifier,
    ) -> Result<(), PlaybackError>;

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
        let result = match job.source {
            PlaybackSource::Audio(source) => self
                .audio
                .as_mut()
                .ok_or_else(|| PlaybackError::Backend("audio playback is disabled".into()))?
                .play(source, job.gain, completion)
                .map(|()| ActiveKind::Audio),
            PlaybackSource::Speech(text) => self
                .tts
                .as_mut()
                .ok_or_else(|| PlaybackError::Backend("system TTS is disabled".into()))?
                .speak(text, job.gain, completion)
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
    use std::sync::{Arc, Mutex};

    use tts::Tts;

    use super::*;

    pub struct SystemTts {
        engine: Tts,
        completion: Arc<Mutex<Option<CompletionNotifier>>>,
        capabilities: TtsCapabilities,
    }

    impl SystemTts {
        pub fn new(voice_id: Option<&str>) -> Result<Self, PlaybackError> {
            let mut engine = Tts::default().map_err(tts_error)?;
            let features = engine.supported_features();
            if !(features.stop && features.volume && features.utterance_callbacks) {
                return Err(PlaybackError::Backend(
                    "system TTS lacks required stop, volume, or completion support".into(),
                ));
            }

            if let Some(requested_id) = voice_id.filter(|id| !id.is_empty()) {
                let voice = engine
                    .voices()
                    .map_err(tts_error)?
                    .into_iter()
                    .find(|voice| voice.id() == requested_id)
                    .ok_or_else(|| {
                        PlaybackError::Backend(format!(
                            "configured system TTS voice was not found: {requested_id}"
                        ))
                    })?;
                engine.set_voice(&voice).map_err(tts_error)?;
            }

            let selected_voice = engine
                .voice()
                .ok()
                .flatten()
                .map(|voice| voice.id().to_owned());
            let completion: Arc<Mutex<Option<CompletionNotifier>>> = Arc::new(Mutex::new(None));
            let on_end = completion.clone();
            engine
                .on_utterance_end(Some(Box::new(move |_| {
                    if let Some(completion) =
                        on_end.lock().expect("TTS callback mutex poisoned").take()
                    {
                        completion.complete();
                    }
                })))
                .map_err(tts_error)?;
            let on_stop = completion.clone();
            engine
                .on_utterance_stop(Some(Box::new(move |_| {
                    if let Some(completion) =
                        on_stop.lock().expect("TTS callback mutex poisoned").take()
                    {
                        // Actor-side ID matching treats this completion as stale
                        // after a confirmed interrupt.
                        completion.complete();
                    }
                })))
                .map_err(tts_error)?;

            Ok(Self {
                engine,
                completion,
                capabilities: TtsCapabilities {
                    voice_id: selected_voice,
                    completion_observable: true,
                    stoppable: true,
                    volume_controllable: true,
                },
            })
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
            let min = self.engine.min_volume();
            let max = self.engine.max_volume();
            let volume = min + gain * (max - min);
            self.engine.set_volume(volume).map_err(tts_error)?;
            *self.completion.lock().expect("TTS callback mutex poisoned") = Some(completion);
            if let Err(error) = self.engine.speak(text, false) {
                self.completion
                    .lock()
                    .expect("TTS callback mutex poisoned")
                    .take();
                return Err(tts_error(error));
            }
            Ok(())
        }

        fn stop(&mut self) -> Result<(), PlaybackError> {
            self.engine.stop().map_err(tts_error)?;
            self.completion
                .lock()
                .expect("TTS callback mutex poisoned")
                .take();
            Ok(())
        }
    }

    fn tts_error(error: tts::Error) -> PlaybackError {
        PlaybackError::Backend(error.to_string())
    }
}

#[cfg(windows)]
pub use native::SystemTts;

/// Compiling on non-Windows keeps the backend seam visible without claiming a
/// supported system TTS implementation for the Windows-first MVP.
#[cfg(not(windows))]
pub struct SystemTts;

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
