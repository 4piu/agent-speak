use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
    sync::Arc,
    thread,
    time::Duration,
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

/// A decoder-preflighted file handle, ready to move to the actor.
#[derive(Debug)]
pub struct PreparedAudio {
    file: File,
    info: AudioInfo,
    runtime_limit: Duration,
}

impl PreparedAudio {
    /// Open, sniff, and decoder-preflight an audio file.
    ///
    /// This does not initialize the output device and is therefore safe to use
    /// from configuration validation and MCP request handling.
    pub fn open(path: impl AsRef<Path>, maximum_duration: Duration) -> Result<Self, PlaybackError> {
        Self::open_with_limits(path, u64::MAX, maximum_duration)
    }

    /// Open and preflight a file while applying its byte limit to the same
    /// retained handle before reading headers or initializing the decoder.
    pub fn open_with_limits(
        path: impl AsRef<Path>,
        maximum_bytes: u64,
        maximum_duration: Duration,
    ) -> Result<Self, PlaybackError> {
        let mut file = File::open(path.as_ref())
            .map_err(|error| PlaybackError::OpenFile(error.to_string()))?;
        let metadata = file
            .metadata()
            .map_err(|error| PlaybackError::OpenFile(error.to_string()))?;
        if !metadata.is_file() {
            return Err(PlaybackError::NotRegularFile);
        }
        let byte_length = metadata.len();
        if byte_length > maximum_bytes {
            return Err(PlaybackError::FileTooLarge);
        }
        let format = sniff_format(&mut file)?;
        file.seek(SeekFrom::Start(0))
            .map_err(|error| PlaybackError::OpenFile(error.to_string()))?;

        let decoder = Decoder::try_from(
            file.try_clone()
                .map_err(|error| PlaybackError::OpenFile(error.to_string()))?,
        )
        .map_err(|_| PlaybackError::UnsupportedAudio)?;
        let duration = decoder
            .total_duration()
            .ok_or(PlaybackError::DurationUnknown)?;
        if duration > maximum_duration {
            return Err(PlaybackError::AudioTooLong);
        }

        Ok(Self {
            file,
            info: AudioInfo {
                format,
                byte_length,
                duration,
            },
            runtime_limit: maximum_duration,
        })
    }

    pub fn info(&self) -> AudioInfo {
        self.info
    }

    fn into_parts(self) -> (File, Duration) {
        (self.file, self.runtime_limit)
    }
}

fn sniff_format(file: &mut File) -> Result<AudioFormat, PlaybackError> {
    let mut header = vec![0_u8; 64 * 1024];
    let read = file
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
            .windows(2)
            .any(|bytes| bytes[0] == 0xff && bytes[1] & 0xe0 == 0xe0)
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

    fn stop(&mut self) -> Result<(), PlaybackError>;

    fn finished(&mut self) {}
}

/// Rodio/CPAL playback through the current default system output.
pub struct RodioAudio {
    _output: MixerDeviceSink,
    active: Option<Arc<Player>>,
}

impl RodioAudio {
    pub fn new() -> Result<Self, PlaybackError> {
        let mut output = DeviceSinkBuilder::open_default_sink()
            .map_err(|error| PlaybackError::Backend(error.to_string()))?;
        output.log_on_drop(false);
        Ok(Self {
            _output: output,
            active: None,
        })
    }
}

impl AudioAdapter for RodioAudio {
    fn play(
        &mut self,
        source: PreparedAudio,
        gain: f32,
        completion: CompletionNotifier,
    ) -> Result<(), PlaybackError> {
        if self.active.as_ref().is_some_and(|player| !player.empty()) {
            return Err(PlaybackError::Backend(
                "audio backend already has an active item".into(),
            ));
        }
        self.active = None;
        let (file, runtime_limit) = source.into_parts();
        let decoder = Decoder::try_from(file).map_err(|_| PlaybackError::UnsupportedAudio)?;
        let player = Arc::new(Player::connect_new(self._output.mixer()));
        player.set_volume(gain);
        player.append(decoder.take_duration(runtime_limit));
        self.active = Some(player.clone());

        thread::Builder::new()
            .name("agent-speak-audio-completion".into())
            .spawn(move || {
                while !player.empty() {
                    thread::sleep(Duration::from_millis(5));
                }
                completion.complete();
            })
            .map_err(|error| PlaybackError::Backend(error.to_string()))?;
        Ok(())
    }

    fn stop(&mut self) -> Result<(), PlaybackError> {
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
    }
}

#[cfg(test)]
mod tests {
    use std::{io::Write, process::Command};

    use tempfile::NamedTempFile;

    use super::*;

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

        let prepared = PreparedAudio::open(file.path(), Duration::from_secs(1)).unwrap();
        assert_eq!(prepared.info().format, AudioFormat::Wav);
        assert_eq!(prepared.info().byte_length, 46);
        assert!(prepared.info().duration <= Duration::from_millis(1));
    }

    #[test]
    fn rejects_content_that_only_has_a_supported_extension() {
        let mut file = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
        file.write_all(b"not audio").unwrap();

        assert!(matches!(
            PreparedAudio::open(file.path(), Duration::from_secs(1)),
            Err(PlaybackError::UnsupportedAudio)
        ));
    }

    #[test]
    fn rejects_audio_over_runtime_limit() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(&silent_wav()).unwrap();

        assert_eq!(
            PreparedAudio::open(file.path(), Duration::ZERO).unwrap_err(),
            PlaybackError::AudioTooLong
        );
    }

    #[test]
    fn rejects_byte_limit_before_decoder_preflight() {
        let file = NamedTempFile::new().unwrap();
        file.as_file().set_len(2).unwrap();

        assert!(matches!(
            PreparedAudio::open_with_limits(file.path(), 1, Duration::from_secs(1)),
            Err(PlaybackError::FileTooLarge)
        ));
    }

    #[test]
    fn recognizes_supported_container_signatures() {
        let cases = [
            (&b"RIFF\x00\x00\x00\x00WAVE"[..], AudioFormat::Wav),
            (&b"fLaC\x00\x00\x00\x00"[..], AudioFormat::Flac),
            (&b"OggSxxxx\x01vorbis"[..], AudioFormat::OggVorbis),
            (&b"ID3\x04\x00\x00"[..], AudioFormat::Mp3),
        ];
        for (bytes, expected) in cases {
            let mut file = NamedTempFile::new().unwrap();
            file.write_all(bytes).unwrap();
            file.as_file_mut().seek(SeekFrom::Start(0)).unwrap();
            assert_eq!(sniff_format(file.as_file_mut()).unwrap(), expected);
        }
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
            let status = Command::new(&ffmpeg)
                .args([
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
                ])
                .arg(&output)
                .status()
                .unwrap();
            assert!(status.success(), "ffmpeg could not create {extension}");

            let prepared = PreparedAudio::open(&output, Duration::from_secs(1)).unwrap();
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
