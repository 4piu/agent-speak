use std::{
    fs::File,
    io::{Cursor, Read, Seek, SeekFrom},
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
    str::FromStr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use rodio::cpal::{
    self,
    traits::{DeviceTrait, HostTrait},
};
use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Player, Source};

use super::{CompletionNotifier, PlaybackError};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioFormat {
    Wav,
    Mp3,
    Flac,
    OggVorbis,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioInfo {
    pub format: AudioFormat,
    pub byte_length: u64,
    pub duration: Duration,
}

/// A concrete output-routing request passed to a playback adapter.
///
/// Device IDs are CPAL's stable, serializable identifiers. Display names are
/// intentionally not accepted because they are neither unique nor stable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OutputTarget {
    /// Resolve the current system default immediately before playback starts.
    SystemDefault,
    /// Open exactly this CPAL output device, without falling back to another.
    DeviceId(String),
}

/// Read-only metadata for an available CPAL output device.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutputDevice {
    /// Stable, serializable CPAL identity suitable for configuration.
    pub device_id: String,
    /// Display-only name; never use this field to select a device.
    pub name: String,
    /// Whether this device was the system default during enumeration.
    pub is_default: bool,
}

/// Enumerate available outputs without creating or starting an audio stream.
pub fn list_output_devices() -> Result<Vec<OutputDevice>, PlaybackError> {
    let host = cpal::default_host();
    let default_id = host
        .default_output_device()
        .and_then(|device| device.id().ok())
        .map(|id| id.to_string());
    let devices = host.output_devices().map_err(|error| {
        PlaybackError::OutputUnavailable(format!("output devices could not be enumerated: {error}"))
    })?;
    let mut outputs = Vec::new();
    for device in devices {
        let device_id = device.id().map_err(|error| {
            PlaybackError::OutputUnavailable(format!(
                "an output device identity is unavailable: {error}"
            ))
        })?;
        let device_id = device_id.to_string();
        let name = device
            .description()
            .map(|description| {
                // Some backends expose a richer endpoint-friendly name as an
                // extended description when it differs from the generic
                // device class (for example, "Speakers (USB Audio)").
                description
                    .extended()
                    .first()
                    .cloned()
                    .unwrap_or_else(|| description.name().to_owned())
            })
            .unwrap_or_else(|_| "<name unavailable>".into());
        outputs.push(OutputDevice {
            is_default: default_id.as_deref() == Some(device_id.as_str()),
            device_id,
            name,
        });
    }
    outputs.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.device_id.cmp(&right.device_id))
    });
    Ok(outputs)
}

/// A decoder-preflighted file handle, ready to move to the actor.
#[derive(Debug)]
pub struct PreparedAudio {
    source: PreparedAudioSource,
    info: AudioInfo,
    runtime_limit: Option<Duration>,
}

#[derive(Debug)]
enum PreparedAudioSource {
    File(File),
    Memory(Cursor<Vec<u8>>),
}

impl PreparedAudio {
    /// Open, sniff, and decoder-preflight an audio file.
    ///
    /// This does not initialize the output device and is therefore safe to use
    /// from configuration validation and MCP request handling.
    pub fn open(
        path: impl AsRef<Path>,
        maximum_duration: Option<Duration>,
    ) -> Result<Self, PlaybackError> {
        let file = File::open(path.as_ref())
            .map_err(|error| PlaybackError::OpenFile(error.to_string()))?;
        Self::from_file(file, maximum_duration)
    }

    /// Preflight an already-opened file and retain that exact handle for
    /// playback. Callers may inspect the opened object before decoder input is
    /// read, avoiding a path replacement race.
    pub(crate) fn from_file(
        mut file: File,
        maximum_duration: Option<Duration>,
    ) -> Result<Self, PlaybackError> {
        let metadata = file
            .metadata()
            .map_err(|error| PlaybackError::OpenFile(error.to_string()))?;
        if !metadata.is_file() {
            return Err(PlaybackError::NotRegularFile);
        }
        let byte_length = metadata.len();
        let format = sniff_format(&mut file)?;
        file.seek(SeekFrom::Start(0))
            .map_err(|error| PlaybackError::OpenFile(error.to_string()))?;

        let decoder_input = file
            .try_clone()
            .map_err(|error| PlaybackError::OpenFile(error.to_string()))?;
        let duration = contain_decoder_preflight(|| {
            let decoder =
                Decoder::try_from(decoder_input).map_err(|_| PlaybackError::UnsupportedAudio)?;
            decoder
                .total_duration()
                .ok_or(PlaybackError::DurationUnknown)
        })?;
        if maximum_duration.is_some_and(|limit| duration > limit) {
            return Err(PlaybackError::AudioTooLong);
        }
        // On Windows, `File::try_clone` duplicates the handle while retaining
        // a shared file pointer. Decoder preflight can therefore advance the
        // source we intend to keep. Rewind the retained handle after every
        // decoder read so runtime playback always starts at byte zero.
        file.seek(SeekFrom::Start(0))
            .map_err(|error| PlaybackError::OpenFile(error.to_string()))?;

        Ok(Self {
            source: PreparedAudioSource::File(file),
            info: AudioInfo {
                format,
                byte_length,
                duration,
            },
            runtime_limit: maximum_duration,
        })
    }

    /// Decoder-preflight synthesized audio held entirely in memory.
    ///
    /// The caller must apply a byte ceiling before allocation. This constructor
    /// performs the same signature, decoder, and duration checks as file input.
    pub(crate) fn from_memory(bytes: Vec<u8>) -> Result<Self, PlaybackError> {
        let byte_length = bytes.len() as u64;
        let mut cursor = Cursor::new(bytes);
        let format = sniff_format(&mut cursor)?;
        cursor
            .seek(SeekFrom::Start(0))
            .map_err(|error| PlaybackError::OpenFile(error.to_string()))?;
        let decoder_input = cursor.clone();
        let duration = contain_decoder_preflight(|| {
            let decoder =
                Decoder::try_from(decoder_input).map_err(|_| PlaybackError::UnsupportedAudio)?;
            decoder
                .total_duration()
                .ok_or(PlaybackError::DurationUnknown)
        })?;
        cursor
            .seek(SeekFrom::Start(0))
            .map_err(|error| PlaybackError::OpenFile(error.to_string()))?;

        Ok(Self {
            source: PreparedAudioSource::Memory(cursor),
            info: AudioInfo {
                format,
                byte_length,
                duration,
            },
            runtime_limit: Some(duration),
        })
    }

    pub fn info(&self) -> AudioInfo {
        self.info
    }

    fn into_parts(self) -> (PreparedAudioSource, Option<Duration>) {
        (self.source, self.runtime_limit)
    }
}

fn contain_decoder_preflight(
    preflight: impl FnOnce() -> Result<Duration, PlaybackError>,
) -> Result<Duration, PlaybackError> {
    catch_unwind(AssertUnwindSafe(preflight)).unwrap_or(Err(PlaybackError::UnsupportedAudio))
}

fn sniff_format(source: &mut impl Read) -> Result<AudioFormat, PlaybackError> {
    let mut header = vec![0_u8; 64 * 1024];
    let read = source
        .read(&mut header)
        .map_err(|error| PlaybackError::OpenFile(error.to_string()))?;
    header.truncate(read);

    if header.starts_with(b"RIFF") && header.get(8..12) == Some(b"WAVE") {
        return Ok(AudioFormat::Wav);
    }
    if header.starts_with(b"fLaC") {
        return Ok(AudioFormat::Flac);
    }
    if header.starts_with(b"OggS") && header.windows(7).any(|bytes| bytes == b"\x01vorbis") {
        return Ok(AudioFormat::OggVorbis);
    }
    if header.starts_with(b"ID3")
        || header
            .get(..2)
            .is_some_and(|bytes| bytes[0] == 0xff && bytes[1] & 0xe0 == 0xe0)
    {
        return Ok(AudioFormat::Mp3);
    }
    Err(PlaybackError::UnsupportedAudio)
}

pub trait AudioAdapter: 'static {
    fn play(
        &mut self,
        source: PreparedAudio,
        gain: f32,
        completion: CompletionNotifier,
    ) -> Result<(), PlaybackError>;

    /// Play through a selected output target.
    ///
    /// Adapters that do not implement explicit device routing retain their
    /// existing system-default behavior and reject fixed device IDs.
    fn play_to(
        &mut self,
        source: PreparedAudio,
        gain: f32,
        target: &OutputTarget,
        completion: CompletionNotifier,
    ) -> Result<(), PlaybackError> {
        match target {
            OutputTarget::SystemDefault => self.play(source, gain, completion),
            OutputTarget::DeviceId(device_id) => Err(PlaybackError::OutputUnavailable(format!(
                "audio adapter does not support output device `{device_id}`"
            ))),
        }
    }

    fn stop(&mut self) -> Result<(), PlaybackError>;

    fn finished(&mut self) {}
}

struct OneShotSlot<T>(Mutex<Option<T>>);

impl<T> Default for OneShotSlot<T> {
    fn default() -> Self {
        Self(Mutex::new(None))
    }
}

impl<T> OneShotSlot<T> {
    fn take(&self) -> Option<T> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }
}

struct ActiveOutput {
    completion: CompletionNotifier,
    player: Arc<Player>,
}

#[derive(Default)]
struct StreamState {
    failed: AtomicBool,
    active: OneShotSlot<ActiveOutput>,
}

impl StreamState {
    fn install(
        &self,
        completion: CompletionNotifier,
        player: Arc<Player>,
    ) -> Result<(), CompletionNotifier> {
        let mut slot = self
            .active
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.failed.load(Ordering::SeqCst) {
            Err(completion)
        } else {
            *slot = Some(ActiveOutput { completion, player });
            Ok(())
        }
    }

    fn complete(&self) {
        if let Some(active) = self.active.take() {
            active.completion.complete();
        }
    }

    fn cancel(&self) {
        let _ = self.active.take();
    }

    fn fail(&self) {
        self.failed.store(true, Ordering::SeqCst);
        if let Some(active) = self.active.take() {
            // Make the polling watcher terminate even when a disconnected
            // stream has stopped pulling samples from Rodio's mixer.
            active.player.stop();
            active.completion.fail("selected output stream failed");
        }
    }

    fn has_failed(&self) -> bool {
        self.failed.load(Ordering::SeqCst)
    }
}

struct OpenedOutput {
    device_id: String,
    sink: MixerDeviceSink,
    state: Arc<StreamState>,
}

/// Rodio/CPAL playback through an exact, stable output endpoint.
pub struct RodioAudio {
    output: Option<OpenedOutput>,
    active: Option<Arc<Player>>,
}

impl RodioAudio {
    pub fn new() -> Result<Self, PlaybackError> {
        Ok(Self {
            output: None,
            active: None,
        })
    }

    fn ensure_output(&mut self, target: &OutputTarget) -> Result<(), PlaybackError> {
        let (device, device_id) = resolve_output_device(target)?;
        let can_reuse = self
            .output
            .as_ref()
            .is_some_and(|output| output.device_id == device_id && !output.state.has_failed());
        if can_reuse {
            return Ok(());
        }

        // Dropping the previous sink before opening the requested one prevents
        // a late callback from that stream from observing the new completion.
        self.output = None;
        let state = Arc::new(StreamState::default());
        let callback_state = state.clone();
        let callback_device_id = device_id.clone();
        let builder = DeviceSinkBuilder::from_device(device).map_err(|error| {
            PlaybackError::OutputUnavailable(format!(
                "selected output device `{device_id}` could not be configured: {error}"
            ))
        })?;
        let mut sink = builder
            .with_error_callback(move |error| {
                tracing::warn!(
                    output_device_id = %callback_device_id,
                    %error,
                    "selected audio output stream failed"
                );
                callback_state.fail();
            })
            // This may try other stream configurations, but never another
            // device. `open_default_sink` is deliberately not used here.
            .open_sink_or_fallback()
            .map_err(|error| {
                PlaybackError::OutputUnavailable(format!(
                    "selected output device `{device_id}` could not be opened: {error}"
                ))
            })?;
        sink.log_on_drop(false);
        if state.has_failed() {
            return Err(PlaybackError::OutputUnavailable(format!(
                "selected output device `{device_id}` failed while opening"
            )));
        }
        self.output = Some(OpenedOutput {
            device_id,
            sink,
            state,
        });
        Ok(())
    }

    fn play_inner(
        &mut self,
        source: PreparedAudio,
        gain: f32,
        target: &OutputTarget,
        completion: CompletionNotifier,
    ) -> Result<(), PlaybackError> {
        if self.active.as_ref().is_some_and(|player| !player.empty()) {
            return Err(PlaybackError::Backend(
                "audio backend already has an active item".into(),
            ));
        }
        self.active = None;
        self.ensure_output(target)?;
        let output = self.output.as_ref().ok_or_else(|| {
            PlaybackError::Backend("audio output initialization returned no sink".into())
        })?;
        let player = Arc::new(Player::connect_new(output.sink.mixer()));
        player.set_volume(gain);
        let (source, runtime_limit) = source.into_parts();
        match source {
            PreparedAudioSource::File(file) => {
                let decoder =
                    Decoder::try_from(file).map_err(|_| PlaybackError::UnsupportedAudio)?;
                if let Some(limit) = runtime_limit {
                    player.append(decoder.take_duration(limit));
                } else {
                    player.append(decoder);
                }
            }
            PreparedAudioSource::Memory(cursor) => {
                let decoder =
                    Decoder::try_from(cursor).map_err(|_| PlaybackError::UnsupportedAudio)?;
                if let Some(limit) = runtime_limit {
                    player.append(decoder.take_duration(limit));
                } else {
                    player.append(decoder);
                }
            }
        }

        let state = output.state.clone();
        if state.install(completion, player.clone()).is_err() {
            player.stop();
            return Err(PlaybackError::OutputUnavailable(format!(
                "selected output device `{}` is unavailable",
                output.device_id
            )));
        }
        let watcher_player = player.clone();
        let watcher_state = state.clone();
        if let Err(error) = thread::Builder::new()
            .name("agent-speak-audio-completion".into())
            .spawn(move || {
                while !watcher_player.empty() {
                    thread::sleep(Duration::from_millis(5));
                }
                watcher_state.complete();
            })
        {
            state.cancel();
            player.stop();
            return Err(PlaybackError::Backend(error.to_string()));
        }
        self.active = Some(player);
        Ok(())
    }
}

fn resolve_output_device(target: &OutputTarget) -> Result<(cpal::Device, String), PlaybackError> {
    let host = cpal::default_host();
    let (device, verify_output_direction) = match target {
        OutputTarget::SystemDefault => host
            .default_output_device()
            .ok_or_else(|| {
                PlaybackError::OutputUnavailable("no default output device is available".into())
            })
            .map(|device| (device, false))?,
        OutputTarget::DeviceId(device_id) => {
            let parsed = cpal::DeviceId::from_str(device_id).map_err(|_| {
                PlaybackError::OutputUnavailable(format!("invalid output device id `{device_id}`"))
            })?;
            host.device_by_id(&parsed)
                .ok_or_else(|| {
                    PlaybackError::OutputUnavailable(format!(
                        "configured output device `{device_id}` is unavailable"
                    ))
                })
                .map(|device| (device, true))?
        }
    };
    // CPAL's ALSA backend deliberately reports the virtual `default` device
    // with an unknown direction because its capabilities are knowable only by
    // opening it. `default_output_device()` is already the directional API;
    // let the stream builder perform that probe instead of rejecting it here.
    if verify_output_direction && !device.supports_output() {
        return Err(PlaybackError::OutputUnavailable(
            "selected device does not support audio output".into(),
        ));
    }
    let device_id = device.id().map_err(|error| {
        PlaybackError::OutputUnavailable(format!(
            "selected output device identity is unavailable: {error}"
        ))
    })?;
    Ok((device, device_id.to_string()))
}

impl AudioAdapter for RodioAudio {
    fn play(
        &mut self,
        source: PreparedAudio,
        gain: f32,
        completion: CompletionNotifier,
    ) -> Result<(), PlaybackError> {
        self.play_inner(source, gain, &OutputTarget::SystemDefault, completion)
    }

    fn play_to(
        &mut self,
        source: PreparedAudio,
        gain: f32,
        target: &OutputTarget,
        completion: CompletionNotifier,
    ) -> Result<(), PlaybackError> {
        self.play_inner(source, gain, target, completion)
    }

    fn stop(&mut self) -> Result<(), PlaybackError> {
        if let Some(output) = &self.output {
            output.state.cancel();
        }
        if let Some(player) = self.active.take() {
            player.stop();
            let deadline = std::time::Instant::now() + Duration::from_secs(1);
            while !player.empty() {
                if std::time::Instant::now() >= deadline {
                    return Err(PlaybackError::Backend(
                        "audio backend did not confirm stop before timeout".into(),
                    ));
                }
                thread::sleep(Duration::from_millis(2));
            }
        }
        Ok(())
    }

    fn finished(&mut self) {
        self.active = None;
        if let Some(output) = &self.output {
            output.state.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{io::Write, process::Command};

    use tempfile::NamedTempFile;

    use super::*;
    use crate::playback::{ConcurrencyMode, PlaybackHandle, PlaybackJob, SystemBackend, SystemTts};

    fn silent_wav() -> Vec<u8> {
        // PCM, mono, 8 kHz, one 16-bit sample.
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
    fn preflights_wav_and_reports_metadata() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(&silent_wav()).unwrap();

        let prepared = PreparedAudio::open(file.path(), Some(Duration::from_secs(1))).unwrap();
        assert_eq!(prepared.info().format, AudioFormat::Wav);
        assert_eq!(prepared.info().byte_length, 46);
        assert!(prepared.info().duration <= Duration::from_millis(1));
    }

    #[tokio::test]
    async fn unavailable_output_is_typed_on_first_use() {
        let handle = PlaybackHandle::spawn(1, || {
            Ok(SystemBackend::<RodioAudio, SystemTts>::new(
                Some(RodioAudio::new()?),
                None,
            ))
        })
        .unwrap();
        let source = PreparedAudio::from_memory(silent_wav()).unwrap();
        let result = handle
            .submit(
                PlaybackJob::audio_to(
                    uuid::Uuid::new_v4(),
                    source,
                    0.4,
                    OutputTarget::DeviceId("not-a-valid-cpal-device-id".into()),
                ),
                ConcurrencyMode::Enqueue,
            )
            .await;

        assert!(matches!(result, Err(PlaybackError::OutputUnavailable(_))));
        handle.shutdown().await.unwrap();
    }

    #[test]
    fn retained_handle_is_rewound_after_decoder_preflight() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(&silent_wav()).unwrap();

        let prepared = PreparedAudio::open(file.path(), Some(Duration::from_secs(1))).unwrap();
        let (PreparedAudioSource::File(mut retained), _) = prepared.into_parts() else {
            panic!("file input changed storage kind");
        };
        assert_eq!(retained.stream_position().unwrap(), 0);
    }

    #[test]
    fn preflights_synthesized_audio_in_memory() {
        let prepared = PreparedAudio::from_memory(silent_wav()).unwrap();
        assert_eq!(prepared.info().format, AudioFormat::Wav);
        assert_eq!(prepared.info().byte_length, 46);
        assert!(prepared.info().duration <= Duration::from_millis(1));

        let (PreparedAudioSource::Memory(retained), _) = prepared.into_parts() else {
            panic!("memory input changed storage kind");
        };
        assert_eq!(retained.position(), 0);
    }

    #[test]
    fn media_preflight_handles_generated_bytes_without_panicking() {
        let valid = silent_wav();
        for end in 0..=valid.len() {
            let _ = PreparedAudio::from_memory(valid[..end].to_vec());
        }
        for byte in 0..valid.len() {
            for bit in 0..8 {
                let mut mutated = valid.clone();
                mutated[byte] ^= 1 << bit;
                let _ = PreparedAudio::from_memory(mutated);
            }
        }

        let signatures: [&[u8]; 4] = [b"RIFF....WAVE", b"fLaC", b"OggS\x01vorbis", b"ID3"];
        let mut state = 0xbb67_ae85_84ca_a73b_u64;
        for case in 0..512 {
            let signature = signatures[case % signatures.len()];
            let length = signature.len() + case % 512;
            let mut bytes = Vec::with_capacity(length);
            bytes.extend_from_slice(signature);
            while bytes.len() < length {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                bytes.push(state as u8);
            }
            let _ = PreparedAudio::from_memory(bytes);
        }
    }

    #[test]
    fn decoder_preflight_panic_becomes_an_unsupported_media_error() {
        assert_eq!(
            contain_decoder_preflight(|| panic!("simulated decoder panic")),
            Err(PlaybackError::UnsupportedAudio)
        );
    }

    #[test]
    fn rejects_content_that_only_has_a_supported_extension() {
        let mut file = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
        file.write_all(b"not audio").unwrap();

        assert!(matches!(
            PreparedAudio::open(file.path(), Some(Duration::from_secs(1))),
            Err(PlaybackError::UnsupportedAudio)
        ));
    }

    #[test]
    fn rejects_audio_over_runtime_limit() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(&silent_wav()).unwrap();

        assert_eq!(
            PreparedAudio::open(file.path(), Some(Duration::ZERO)).unwrap_err(),
            PlaybackError::AudioTooLong
        );
    }

    #[test]
    fn unlimited_duration_accepts_valid_audio() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(&silent_wav()).unwrap();

        assert!(PreparedAudio::open(file.path(), None).is_ok());
    }

    #[test]
    fn file_size_is_not_capped() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(&silent_wav()).unwrap();
        let length = 52_428_801;
        file.as_file().set_len(length).unwrap();

        let prepared = PreparedAudio::open(file.path(), None).unwrap();
        assert_eq!(prepared.info().byte_length, length);
    }

    #[test]
    fn recognizes_supported_container_signatures() {
        let cases = [
            (&b"RIFF\x00\x00\x00\x00WAVE"[..], AudioFormat::Wav),
            (&b"fLaC\x00\x00\x00\x00"[..], AudioFormat::Flac),
            (&b"OggSxxxx\x01vorbis"[..], AudioFormat::OggVorbis),
            (&b"ID3\x04\x00\x00"[..], AudioFormat::Mp3),
            (&b"\xff\xfb\x90\x64"[..], AudioFormat::Mp3),
        ];
        for (bytes, expected) in cases {
            let mut file = NamedTempFile::new().unwrap();
            file.write_all(bytes).unwrap();
            file.as_file_mut().seek(SeekFrom::Start(0)).unwrap();
            assert_eq!(sniff_format(file.as_file_mut()).unwrap(), expected);
        }

        let mut unsupported_ogg = &b"OggSxxxx\x7fFLACxxxx\xff\xfb"[..];
        assert_eq!(
            sniff_format(&mut unsupported_ogg),
            Err(PlaybackError::UnsupportedAudio)
        );
    }

    #[test]
    fn output_targets_compare_by_stable_id_not_display_name() {
        assert_eq!(
            OutputTarget::DeviceId("wasapi:endpoint-a".into()),
            OutputTarget::DeviceId("wasapi:endpoint-a".into())
        );
        assert_ne!(
            OutputTarget::DeviceId("wasapi:endpoint-a".into()),
            OutputTarget::DeviceId("wasapi:endpoint-b".into())
        );
        assert_ne!(
            OutputTarget::SystemDefault,
            OutputTarget::DeviceId("wasapi:endpoint-a".into())
        );
    }

    #[test]
    fn one_shot_slot_can_only_yield_a_completion_once() {
        let slot = OneShotSlot(Mutex::new(Some(42)));
        assert_eq!(slot.take(), Some(42));
        assert_eq!(slot.take(), None);
    }

    #[test]
    #[ignore = "manual spike: requires ffmpeg on PATH"]
    fn spike_preflights_all_selected_decoders() {
        let Some(ffmpeg) = std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .flat_map(|directory| [directory.join("ffmpeg.exe"), directory.join("ffmpeg")])
                .find(|candidate| candidate.is_file())
        }) else {
            panic!("ffmpeg is required for this manual decoder spike");
        };
        let directory = tempfile::tempdir().unwrap();
        let cases = [
            ("wav", AudioFormat::Wav),
            ("mp3", AudioFormat::Mp3),
            ("flac", AudioFormat::Flac),
            ("ogg", AudioFormat::OggVorbis),
        ];

        for (extension, expected) in cases {
            let output = directory.path().join(format!("silence.{extension}"));
            let mut command = Command::new(&ffmpeg);
            command.args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=8000:cl=mono",
                "-t",
                "0.1",
                "-y",
            ]);
            if extension == "ogg" {
                command.args(["-ac", "2", "-c:a", "vorbis", "-strict", "experimental"]);
            }
            let status = command.arg(&output).status().unwrap();
            assert!(status.success(), "ffmpeg could not create {extension}");

            let prepared = PreparedAudio::open(&output, Some(Duration::from_secs(1))).unwrap();
            assert_eq!(prepared.info().format, expected);
            assert!(prepared.info().duration <= Duration::from_secs(1));
        }
    }

    #[test]
    #[ignore = "manual spike: requires a default system audio output"]
    fn spike_initializes_default_output() {
        RodioAudio::new().unwrap();
    }
}
