use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::{Cursor, Read, Seek, SeekFrom},
    num::NonZeroU16,
    num::NonZeroU32,
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

use crossbeam_channel::{Receiver, Sender};
use rodio::buffer::SamplesBuffer;
use rodio::cpal::{
    self,
    traits::{DeviceTrait, HostTrait},
};
use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Player, Source};
use uuid::Uuid;

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

    fn stop(&mut self, playback_id: Uuid) -> Result<(), PlaybackError>;

    fn finished(&mut self, _playback_id: Uuid) {}

    fn shutdown(&mut self) -> Result<(), PlaybackError> {
        Ok(())
    }
}

struct ActiveOutput {
    completion: CompletionNotifier,
    player: Arc<Player>,
}

#[derive(Default)]
struct StreamState {
    failed: AtomicBool,
    active: Mutex<HashMap<Uuid, ActiveOutput>>,
}

impl StreamState {
    fn install(
        &self,
        completion: CompletionNotifier,
        player: Arc<Player>,
    ) -> Result<(), CompletionNotifier> {
        let playback_id = completion.playback_id();
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.failed.load(Ordering::SeqCst) || active.contains_key(&playback_id) {
            Err(completion)
        } else {
            active.insert(playback_id, ActiveOutput { completion, player });
            Ok(())
        }
    }

    fn complete(&self, playback_id: Uuid) {
        if let Some(active) = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&playback_id)
        {
            active.completion.complete();
        }
    }

    fn cancel(&self, playback_id: Uuid) {
        if let Some(active) = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&playback_id)
        {
            active.completion.discard();
        }
    }

    fn fail(&self) {
        self.failed.store(true, Ordering::SeqCst);
        let active = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain()
            .map(|(_, active)| active)
            .collect::<Vec<_>>();
        for active in active {
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
    sink: MixerDeviceSink,
    state: Arc<StreamState>,
}

struct ActivePlayer {
    device_id: String,
    player: Arc<Player>,
    requested_gain: f32,
}

trait AudioEngine: 'static {
    fn play(
        &mut self,
        source: PreparedAudio,
        gain: f32,
        target: &OutputTarget,
        completion: CompletionNotifier,
    ) -> Result<(), PlaybackError>;

    fn start_incremental(
        &mut self,
        sample_rate_hz: u32,
        channels: u16,
        gain: f32,
        target: &OutputTarget,
        completion: CompletionNotifier,
    ) -> Result<(), PlaybackError>;

    fn append_incremental(
        &mut self,
        playback_id: Uuid,
        sample_rate_hz: u32,
        channels: u16,
        pcm: &[u8],
    ) -> Result<(), PlaybackError>;

    fn incremental_queue_len(&self, playback_id: Uuid) -> Result<usize, PlaybackError>;
    fn finish_incremental(&mut self, playback_id: Uuid) -> Result<(), PlaybackError>;
    fn stop(&mut self, playback_id: Uuid) -> Result<(), PlaybackError>;
    fn finished(&mut self, playback_id: Uuid);
    fn shutdown(&mut self) -> Result<(), PlaybackError>;
}

/// Thread-confined Rodio/CPAL state for every output used by one server.
struct RodioEngine {
    outputs: HashMap<String, OpenedOutput>,
    active: HashMap<Uuid, ActivePlayer>,
}

impl RodioEngine {
    fn new() -> Self {
        Self {
            outputs: HashMap::new(),
            active: HashMap::new(),
        }
    }

    fn ensure_output(&mut self, target: &OutputTarget) -> Result<String, PlaybackError> {
        let (device, device_id) = resolve_output_device(target)?;
        let can_reuse = self
            .outputs
            .get(&device_id)
            .is_some_and(|output| !output.state.has_failed());
        if can_reuse {
            return Ok(device_id);
        }

        self.outputs.remove(&device_id);
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
        self.outputs
            .insert(device_id.clone(), OpenedOutput { sink, state });
        Ok(device_id)
    }

    fn play_inner(
        &mut self,
        source: PreparedAudio,
        gain: f32,
        target: &OutputTarget,
        completion: CompletionNotifier,
    ) -> Result<(), PlaybackError> {
        let playback_id = completion.playback_id();
        if self.active.contains_key(&playback_id) {
            return Err(PlaybackError::Backend(
                "audio backend already has this active playback ID".into(),
            ));
        }
        let device_id = self.ensure_output(target)?;
        let output = self.outputs.get(&device_id).ok_or_else(|| {
            PlaybackError::Backend("audio output initialization returned no sink".into())
        })?;
        let player = Arc::new(Player::connect_new(output.sink.mixer()));
        player.pause();
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
        if let Err(completion) = state.install(completion, player.clone()) {
            completion.discard();
            player.stop();
            return Err(PlaybackError::OutputUnavailable(format!(
                "selected output device `{}` is unavailable",
                device_id
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
                watcher_state.complete(playback_id);
            })
        {
            state.cancel(playback_id);
            player.stop();
            return Err(PlaybackError::Backend(error.to_string()));
        }
        self.active.insert(
            playback_id,
            ActivePlayer {
                device_id: device_id.clone(),
                player: player.clone(),
                requested_gain: gain,
            },
        );
        self.rebalance_output_gain(&device_id);
        player.play();
        Ok(())
    }

    /// Begin a provider-owned incremental PCM stream. Completion is deliberately
    /// armed only by `finish_incremental`; a temporarily empty queue is not a
    /// terminal condition while synthesis is still producing chunks.
    fn start_incremental(
        &mut self,
        sample_rate_hz: u32,
        channels: u16,
        gain: f32,
        target: &OutputTarget,
        completion: CompletionNotifier,
    ) -> Result<(), PlaybackError> {
        let playback_id = completion.playback_id();
        if self.active.contains_key(&playback_id) {
            return Err(PlaybackError::Backend(
                "audio backend already has this active playback ID".into(),
            ));
        }
        if !(8_000..=96_000).contains(&sample_rate_hz) || !(1..=2).contains(&channels) {
            return Err(PlaybackError::UnsupportedAudio);
        }
        let device_id = self.ensure_output(target)?;
        let output = self.outputs.get(&device_id).ok_or_else(|| {
            PlaybackError::Backend("audio output initialization returned no sink".into())
        })?;
        let player = Arc::new(Player::connect_new(output.sink.mixer()));
        player.pause();
        player.set_volume(gain);
        if let Err(completion) = output.state.install(completion, player.clone()) {
            completion.discard();
            return Err(PlaybackError::OutputUnavailable(format!(
                "selected output device `{}` is unavailable",
                device_id
            )));
        }
        self.active.insert(
            playback_id,
            ActivePlayer {
                device_id: device_id.clone(),
                player: player.clone(),
                requested_gain: gain,
            },
        );
        self.rebalance_output_gain(&device_id);
        player.play();
        Ok(())
    }

    fn append_incremental(
        &mut self,
        playback_id: Uuid,
        sample_rate_hz: u32,
        channels: u16,
        pcm: &[u8],
    ) -> Result<(), PlaybackError> {
        let player = &self
            .active
            .get(&playback_id)
            .ok_or_else(|| {
                PlaybackError::Backend("incremental audio stream has not started".into())
            })?
            .player;
        let alignment = usize::from(channels) * 2;
        if alignment == 0 || pcm.is_empty() || !pcm.len().is_multiple_of(alignment) {
            return Err(PlaybackError::UnsupportedAudio);
        }
        let samples = pcm
            .chunks_exact(2)
            .map(|bytes| f32::from(i16::from_le_bytes([bytes[0], bytes[1]])) / 32768.0)
            .collect::<Vec<_>>();
        let channels = NonZeroU16::new(channels).ok_or(PlaybackError::UnsupportedAudio)?;
        let sample_rate = NonZeroU32::new(sample_rate_hz).ok_or(PlaybackError::UnsupportedAudio)?;
        player.append(SamplesBuffer::new(channels, sample_rate, samples));
        Ok(())
    }

    fn incremental_queue_len(&self, playback_id: Uuid) -> Result<usize, PlaybackError> {
        self.active
            .get(&playback_id)
            .map(|active| active.player.len())
            .ok_or_else(|| {
                PlaybackError::Backend("incremental audio stream has not started".into())
            })
    }

    fn finish_incremental(&mut self, playback_id: Uuid) -> Result<(), PlaybackError> {
        let active = self.active.get(&playback_id).ok_or_else(|| {
            PlaybackError::Backend("incremental audio stream has not started".into())
        })?;
        let player = active.player.clone();
        let output = self
            .outputs
            .get(&active.device_id)
            .ok_or_else(|| PlaybackError::Backend("audio output is unavailable".into()))?;
        let state = output.state.clone();
        thread::Builder::new()
            .name("agent-speak-incremental-completion".into())
            .spawn(move || {
                while !player.empty() {
                    thread::sleep(Duration::from_millis(5));
                }
                state.complete(playback_id);
            })
            .map_err(|error| PlaybackError::Backend(error.to_string()))?;
        Ok(())
    }

    fn stop(&mut self, playback_id: Uuid) -> Result<(), PlaybackError> {
        let Some(active) = self.active.get(&playback_id) else {
            return Ok(());
        };
        let device_id = active.device_id.clone();
        let player = active.player.clone();
        if let Some(output) = self.outputs.get(&active.device_id) {
            output.state.cancel(playback_id);
        }
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
        self.active.remove(&playback_id);
        self.rebalance_output_gain(&device_id);
        if self
            .outputs
            .get(&device_id)
            .is_some_and(|output| output.state.has_failed())
        {
            self.outputs.remove(&device_id);
        }
        Ok(())
    }

    fn finished(&mut self, playback_id: Uuid) {
        if let Some(active) = self.active.remove(&playback_id) {
            if let Some(output) = self.outputs.get(&active.device_id) {
                output.state.cancel(playback_id);
            }
            self.rebalance_output_gain(&active.device_id);
        }
    }

    fn rebalance_output_gain(&self, device_id: &str) {
        let total_requested = self
            .active
            .values()
            .filter(|active| active.device_id == device_id)
            .map(|active| active.requested_gain)
            .sum::<f32>();
        let scale = gain_normalization_scale(total_requested);
        for active in self
            .active
            .values()
            .filter(|active| active.device_id == device_id)
        {
            active.player.set_volume(active.requested_gain * scale);
        }
    }

    fn shutdown(&mut self) -> Result<(), PlaybackError> {
        let active = self.active.keys().copied().collect::<Vec<_>>();
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

fn gain_normalization_scale(total_requested_gain: f32) -> f32 {
    if total_requested_gain > 1.0 {
        total_requested_gain.recip()
    } else {
        1.0
    }
}

impl AudioEngine for RodioEngine {
    fn play(
        &mut self,
        source: PreparedAudio,
        gain: f32,
        target: &OutputTarget,
        completion: CompletionNotifier,
    ) -> Result<(), PlaybackError> {
        self.play_inner(source, gain, target, completion)
    }

    fn start_incremental(
        &mut self,
        sample_rate_hz: u32,
        channels: u16,
        gain: f32,
        target: &OutputTarget,
        completion: CompletionNotifier,
    ) -> Result<(), PlaybackError> {
        RodioEngine::start_incremental(self, sample_rate_hz, channels, gain, target, completion)
    }

    fn append_incremental(
        &mut self,
        playback_id: Uuid,
        sample_rate_hz: u32,
        channels: u16,
        pcm: &[u8],
    ) -> Result<(), PlaybackError> {
        RodioEngine::append_incremental(self, playback_id, sample_rate_hz, channels, pcm)
    }

    fn incremental_queue_len(&self, playback_id: Uuid) -> Result<usize, PlaybackError> {
        RodioEngine::incremental_queue_len(self, playback_id)
    }

    fn finish_incremental(&mut self, playback_id: Uuid) -> Result<(), PlaybackError> {
        RodioEngine::finish_incremental(self, playback_id)
    }

    fn stop(&mut self, playback_id: Uuid) -> Result<(), PlaybackError> {
        RodioEngine::stop(self, playback_id)
    }

    fn finished(&mut self, playback_id: Uuid) {
        RodioEngine::finished(self, playback_id);
    }

    fn shutdown(&mut self) -> Result<(), PlaybackError> {
        RodioEngine::shutdown(self)
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

enum AudioCommand {
    Play {
        source: PreparedAudio,
        gain: f32,
        target: OutputTarget,
        completion: CompletionNotifier,
        response: Sender<Result<(), PlaybackError>>,
    },
    StartIncremental {
        sample_rate_hz: u32,
        channels: u16,
        gain: f32,
        target: OutputTarget,
        completion: CompletionNotifier,
        response: Sender<Result<(), PlaybackError>>,
    },
    AppendIncremental {
        playback_id: Uuid,
        sample_rate_hz: u32,
        channels: u16,
        pcm: Vec<u8>,
        response: Sender<Result<(), PlaybackError>>,
    },
    IncrementalQueueLen {
        playback_id: Uuid,
        response: Sender<Result<usize, PlaybackError>>,
    },
    FinishIncremental {
        playback_id: Uuid,
        response: Sender<Result<(), PlaybackError>>,
    },
    Stop {
        playback_id: Uuid,
        response: Sender<Result<(), PlaybackError>>,
    },
    Finished(Uuid),
    Shutdown(Sender<Result<(), PlaybackError>>),
}

struct AudioService {
    commands: Sender<AudioCommand>,
    join: Mutex<Option<thread::JoinHandle<()>>>,
}

impl Drop for AudioService {
    fn drop(&mut self) {
        let (response, receiver) = crossbeam_channel::bounded(1);
        let _ = self.commands.send(AudioCommand::Shutdown(response));
        let _ = receiver.recv();
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

/// A lightweight client for the server-owned Rodio output thread.
///
/// Every client created with [`Self::shared_client`] routes to the same
/// per-device mixer registry while retaining ownership of only the playback
/// IDs it started.
pub struct RodioAudio {
    service: Arc<AudioService>,
    owned: HashSet<Uuid>,
}

impl RodioAudio {
    pub fn new() -> Result<Self, PlaybackError> {
        Self::with_engine(RodioEngine::new)
    }

    fn with_engine<E>(factory: impl FnOnce() -> E + Send + 'static) -> Result<Self, PlaybackError>
    where
        E: AudioEngine,
    {
        let (commands, receiver) = crossbeam_channel::unbounded();
        let (ready, readiness) = crossbeam_channel::bounded(1);
        let join = thread::Builder::new()
            .name("agent-speak-audio-output".into())
            .spawn(move || {
                let mut engine = factory();
                let _ = ready.send(());
                run_audio_service(&mut engine, receiver);
            })
            .map_err(|error| PlaybackError::Backend(error.to_string()))?;
        readiness
            .recv()
            .map_err(|_| PlaybackError::Backend("audio output thread did not start".into()))?;
        Ok(Self {
            service: Arc::new(AudioService {
                commands,
                join: Mutex::new(Some(join)),
            }),
            owned: HashSet::new(),
        })
    }

    /// Create an independently-owned route to this server's output service.
    pub(crate) fn shared_client(&self) -> Self {
        Self {
            service: self.service.clone(),
            owned: HashSet::new(),
        }
    }

    fn call<T>(
        &self,
        command: impl FnOnce(Sender<Result<T, PlaybackError>>) -> AudioCommand,
    ) -> Result<T, PlaybackError> {
        let (response, receiver) = crossbeam_channel::bounded(1);
        self.service
            .commands
            .send(command(response))
            .map_err(|_| PlaybackError::Backend("audio output thread stopped".into()))?;
        receiver
            .recv()
            .map_err(|_| PlaybackError::Backend("audio output thread stopped".into()))?
    }

    pub(crate) fn start_incremental(
        &mut self,
        sample_rate_hz: u32,
        channels: u16,
        gain: f32,
        target: &OutputTarget,
        completion: CompletionNotifier,
    ) -> Result<(), PlaybackError> {
        let playback_id = completion.playback_id();
        self.call(|response| AudioCommand::StartIncremental {
            sample_rate_hz,
            channels,
            gain,
            target: target.clone(),
            completion,
            response,
        })?;
        self.owned.insert(playback_id);
        Ok(())
    }

    pub(crate) fn append_incremental(
        &mut self,
        playback_id: Uuid,
        sample_rate_hz: u32,
        channels: u16,
        pcm: &[u8],
    ) -> Result<(), PlaybackError> {
        self.call(|response| AudioCommand::AppendIncremental {
            playback_id,
            sample_rate_hz,
            channels,
            pcm: pcm.to_vec(),
            response,
        })
    }

    pub(crate) fn incremental_queue_len(&self, playback_id: Uuid) -> Result<usize, PlaybackError> {
        self.call(|response| AudioCommand::IncrementalQueueLen {
            playback_id,
            response,
        })
    }

    pub(crate) fn finish_incremental(&mut self, playback_id: Uuid) -> Result<(), PlaybackError> {
        self.call(|response| AudioCommand::FinishIncremental {
            playback_id,
            response,
        })
    }
}

fn run_audio_service(engine: &mut impl AudioEngine, commands: Receiver<AudioCommand>) {
    while let Ok(command) = commands.recv() {
        match command {
            AudioCommand::Play {
                source,
                gain,
                target,
                completion,
                response,
            } => {
                let _ = response.send(engine.play(source, gain, &target, completion));
            }
            AudioCommand::StartIncremental {
                sample_rate_hz,
                channels,
                gain,
                target,
                completion,
                response,
            } => {
                let _ = response.send(engine.start_incremental(
                    sample_rate_hz,
                    channels,
                    gain,
                    &target,
                    completion,
                ));
            }
            AudioCommand::AppendIncremental {
                playback_id,
                sample_rate_hz,
                channels,
                pcm,
                response,
            } => {
                let _ = response.send(engine.append_incremental(
                    playback_id,
                    sample_rate_hz,
                    channels,
                    &pcm,
                ));
            }
            AudioCommand::IncrementalQueueLen {
                playback_id,
                response,
            } => {
                let _ = response.send(engine.incremental_queue_len(playback_id));
            }
            AudioCommand::FinishIncremental {
                playback_id,
                response,
            } => {
                let _ = response.send(engine.finish_incremental(playback_id));
            }
            AudioCommand::Stop {
                playback_id,
                response,
            } => {
                let _ = response.send(engine.stop(playback_id));
            }
            AudioCommand::Finished(playback_id) => engine.finished(playback_id),
            AudioCommand::Shutdown(response) => {
                let _ = response.send(engine.shutdown());
                break;
            }
        }
    }
}

impl AudioAdapter for RodioAudio {
    fn play(
        &mut self,
        source: PreparedAudio,
        gain: f32,
        completion: CompletionNotifier,
    ) -> Result<(), PlaybackError> {
        self.play_to(source, gain, &OutputTarget::SystemDefault, completion)
    }

    fn play_to(
        &mut self,
        source: PreparedAudio,
        gain: f32,
        target: &OutputTarget,
        completion: CompletionNotifier,
    ) -> Result<(), PlaybackError> {
        let playback_id = completion.playback_id();
        self.call(|response| AudioCommand::Play {
            source,
            gain,
            target: target.clone(),
            completion,
            response,
        })?;
        self.owned.insert(playback_id);
        Ok(())
    }

    fn stop(&mut self, playback_id: Uuid) -> Result<(), PlaybackError> {
        if !self.owned.contains(&playback_id) {
            return Ok(());
        }
        self.call(|response| AudioCommand::Stop {
            playback_id,
            response,
        })?;
        self.owned.remove(&playback_id);
        Ok(())
    }

    fn finished(&mut self, playback_id: Uuid) {
        if self.owned.remove(&playback_id) {
            let _ = self
                .service
                .commands
                .send(AudioCommand::Finished(playback_id));
        }
    }

    fn shutdown(&mut self) -> Result<(), PlaybackError> {
        let active = self.owned.iter().copied().collect::<Vec<_>>();
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

#[cfg(test)]
mod tests {
    use std::{io::Write, process::Command, time::Instant};

    use tempfile::NamedTempFile;

    use super::*;
    use crate::playback::{
        ConcurrencyMode, PlaybackHandle, PlaybackJob, SystemBackend, SystemTts, TtsAdapter,
        TtsCapabilities,
    };

    #[derive(Default)]
    struct TestEngineState {
        factories: usize,
        started: Vec<Uuid>,
        stopped: Vec<Uuid>,
        incremental_starts: Vec<Uuid>,
        incremental_appends: Vec<Uuid>,
        incremental_finishes: Vec<Uuid>,
        completions: HashMap<Uuid, CompletionNotifier>,
    }

    struct TestAudioEngine(Arc<Mutex<TestEngineState>>);

    impl AudioEngine for TestAudioEngine {
        fn play(
            &mut self,
            _source: PreparedAudio,
            _gain: f32,
            _target: &OutputTarget,
            completion: CompletionNotifier,
        ) -> Result<(), PlaybackError> {
            self.install(completion)
        }

        fn start_incremental(
            &mut self,
            _sample_rate_hz: u32,
            _channels: u16,
            _gain: f32,
            _target: &OutputTarget,
            completion: CompletionNotifier,
        ) -> Result<(), PlaybackError> {
            let playback_id = completion.playback_id();
            self.install(completion)?;
            self.0.lock().unwrap().incremental_starts.push(playback_id);
            Ok(())
        }

        fn append_incremental(
            &mut self,
            playback_id: Uuid,
            _sample_rate_hz: u32,
            _channels: u16,
            _pcm: &[u8],
        ) -> Result<(), PlaybackError> {
            let mut state = self.0.lock().unwrap();
            if !state.completions.contains_key(&playback_id) {
                return Err(PlaybackError::Backend("test stream is unavailable".into()));
            }
            state.incremental_appends.push(playback_id);
            Ok(())
        }

        fn incremental_queue_len(&self, playback_id: Uuid) -> Result<usize, PlaybackError> {
            self.0
                .lock()
                .unwrap()
                .completions
                .contains_key(&playback_id)
                .then_some(0)
                .ok_or_else(|| PlaybackError::Backend("test stream is unavailable".into()))
        }

        fn finish_incremental(&mut self, playback_id: Uuid) -> Result<(), PlaybackError> {
            self.incremental_queue_len(playback_id)?;
            self.0
                .lock()
                .unwrap()
                .incremental_finishes
                .push(playback_id);
            Ok(())
        }

        fn stop(&mut self, playback_id: Uuid) -> Result<(), PlaybackError> {
            let mut state = self.0.lock().unwrap();
            if let Some(completion) = state.completions.remove(&playback_id) {
                completion.discard();
                state.stopped.push(playback_id);
            }
            Ok(())
        }

        fn finished(&mut self, playback_id: Uuid) {
            if let Some(completion) = self.0.lock().unwrap().completions.remove(&playback_id) {
                completion.discard();
            }
        }

        fn shutdown(&mut self) -> Result<(), PlaybackError> {
            let active = self
                .0
                .lock()
                .unwrap()
                .completions
                .keys()
                .copied()
                .collect::<Vec<_>>();
            for playback_id in active {
                self.stop(playback_id)?;
            }
            Ok(())
        }
    }

    impl TestAudioEngine {
        fn install(&self, completion: CompletionNotifier) -> Result<(), PlaybackError> {
            let playback_id = completion.playback_id();
            let mut state = self.0.lock().unwrap();
            if state.completions.insert(playback_id, completion).is_some() {
                return Err(PlaybackError::Backend("duplicate test playback ID".into()));
            }
            state.started.push(playback_id);
            Ok(())
        }
    }

    struct SharedTestTts {
        audio: RodioAudio,
    }

    impl TtsAdapter for SharedTestTts {
        fn capabilities(&self) -> TtsCapabilities {
            TtsCapabilities {
                voice_id: Some("shared-test".into()),
                completion_observable: true,
                stoppable: true,
                volume_controllable: true,
            }
        }

        fn speak(
            &mut self,
            _text: String,
            gain: f32,
            completion: CompletionNotifier,
        ) -> Result<(), PlaybackError> {
            let playback_id = completion.playback_id();
            self.audio.start_incremental(
                8_000,
                1,
                gain,
                &OutputTarget::SystemDefault,
                completion,
            )?;
            self.audio
                .append_incremental(playback_id, 8_000, 1, &[0, 0])?;
            self.audio.finish_incremental(playback_id)
        }

        fn stop(&mut self, playback_id: Uuid) -> Result<(), PlaybackError> {
            self.audio.stop(playback_id)
        }

        fn finished(&mut self, playback_id: Uuid) {
            self.audio.finished(playback_id);
        }
    }

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

    #[tokio::test]
    async fn file_and_tts_routes_share_one_output_service_with_keyed_ownership() {
        let state = Arc::new(Mutex::new(TestEngineState::default()));
        let actor_state = state.clone();
        let handle = PlaybackHandle::spawn(2, move || {
            let engine_state = actor_state.clone();
            let output = RodioAudio::with_engine(move || {
                engine_state.lock().unwrap().factories += 1;
                TestAudioEngine(engine_state)
            })?;
            let audio = output.shared_client();
            let tts = SharedTestTts {
                audio: output.shared_client(),
            };
            Ok(SystemBackend::new(Some(audio), Some(tts)))
        })
        .unwrap();
        let file_id = Uuid::new_v4();
        let speech_id = Uuid::new_v4();

        handle
            .submit(
                PlaybackJob::audio(
                    file_id,
                    PreparedAudio::from_memory(silent_wav()).unwrap(),
                    0.3,
                ),
                ConcurrencyMode::Mix,
            )
            .await
            .unwrap();
        handle
            .submit(
                PlaybackJob::speech(speech_id, "shared output", 0.3),
                ConcurrencyMode::Mix,
            )
            .await
            .unwrap();

        {
            let state = state.lock().unwrap();
            assert_eq!(state.factories, 1);
            assert_eq!(state.started, vec![file_id, speech_id]);
            assert_eq!(state.incremental_starts, vec![speech_id]);
            assert_eq!(state.incremental_appends, vec![speech_id]);
            assert_eq!(state.incremental_finishes, vec![speech_id]);
        }

        handle.cancel(file_id).await.unwrap();
        {
            let state = state.lock().unwrap();
            assert_eq!(state.stopped, vec![file_id]);
            assert!(state.completions.contains_key(&speech_id));
        }
        assert_eq!(
            handle.status(speech_id).await.unwrap().unwrap().state,
            crate::playback::PlaybackState::Playing
        );

        state
            .lock()
            .unwrap()
            .completions
            .remove(&speech_id)
            .unwrap()
            .complete();
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if handle.status(speech_id).await.unwrap().unwrap().state
                == crate::playback::PlaybackState::Completed
            {
                break;
            }
            assert!(Instant::now() < deadline, "completion status timed out");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        handle.shutdown().await.unwrap();
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

    #[test]
    fn gain_normalization_preserves_headroom_without_attenuating_safe_sums() {
        assert_eq!(gain_normalization_scale(0.0), 1.0);
        assert_eq!(gain_normalization_scale(0.8), 1.0);
        assert_eq!(gain_normalization_scale(1.0), 1.0);

        let requested = [0.7_f32, 0.7, 0.2];
        let scale = gain_normalization_scale(requested.iter().sum());
        let normalized_sum = requested.iter().map(|gain| gain * scale).sum::<f32>();
        assert!((normalized_sum - 1.0).abs() < f32::EPSILON * 4.0);
        assert!(scale < 1.0);
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
