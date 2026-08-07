use std::{
    io::{self, Read, Seek, SeekFrom},
    panic::{AssertUnwindSafe, catch_unwind},
    thread,
};

use crossbeam_channel::{Receiver, Sender};
use symphonia::{
    core::{
        audio::SampleBuffer,
        codecs::{CODEC_TYPE_MP3, CODEC_TYPE_OPUS, CodecRegistry, DecoderOptions},
        errors::Error as SymphoniaError,
        formats::FormatOptions,
        io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions},
        meta::MetadataOptions,
        probe::Hint,
    },
    default,
};
use symphonia_adapter_libopus::OpusDecoder;

const CHANNEL_CAPACITY: usize = 4;
const MAX_DECODED_AUDIO_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EncodedFormat {
    Mp3,
    OggOpus,
}

impl EncodedFormat {
    pub(super) fn parse(value: &str) -> Option<Self> {
        match value {
            "audio/mpeg" => Some(Self::Mp3),
            "audio/ogg;codecs=opus" => Some(Self::OggOpus),
            _ => None,
        }
    }

    fn mime(self) -> &'static str {
        match self {
            Self::Mp3 => "audio/mpeg",
            Self::OggOpus => "audio/ogg",
        }
    }
}

#[derive(Debug)]
pub(super) enum DecodedMessage {
    Pcm {
        sample_rate_hz: u32,
        channels: u16,
        bytes: Vec<u8>,
    },
    Complete,
    Failed(String),
}

pub(super) struct EncodedDecoder {
    pub(super) input: Option<Sender<Vec<u8>>>,
    pub(super) output: Receiver<DecodedMessage>,
    join: Option<thread::JoinHandle<()>>,
}

impl EncodedDecoder {
    pub(super) fn spawn(format: EncodedFormat, maximum_audio_seconds: u64) -> Result<Self, String> {
        let (input, input_rx) = crossbeam_channel::bounded(CHANNEL_CAPACITY);
        let (output_tx, output) = crossbeam_channel::bounded(CHANNEL_CAPACITY);
        let join = thread::Builder::new()
            .name("agent-speak-encoded-decoder".into())
            .spawn(move || {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    decode_stream(format, maximum_audio_seconds, input_rx, &output_tx)
                }))
                .unwrap_or_else(|_| Err("audio decoder panicked".into()));
                let message = match result {
                    Ok(()) => DecodedMessage::Complete,
                    Err(error) => DecodedMessage::Failed(error),
                };
                let _ = output_tx.send(message);
            })
            .map_err(|error| error.to_string())?;
        Ok(Self {
            input: Some(input),
            output,
            join: Some(join),
        })
    }

    pub(super) fn finish_input(&mut self) {
        self.input = None;
    }
}

impl Drop for EncodedDecoder {
    fn drop(&mut self) {
        self.input = None;
        if let Some(join) = self.join.take() {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
            while !join.is_finished() && std::time::Instant::now() < deadline {
                let _ = self
                    .output
                    .recv_timeout(std::time::Duration::from_millis(5));
            }
            if join.is_finished() {
                let _ = join.join();
            }
        }
    }
}

struct ChannelSource {
    receiver: Receiver<Vec<u8>>,
    current: Vec<u8>,
    offset: usize,
    eof: bool,
}

impl ChannelSource {
    fn new(receiver: Receiver<Vec<u8>>) -> Self {
        Self {
            receiver,
            current: Vec::new(),
            offset: 0,
            eof: false,
        }
    }
}

impl Read for ChannelSource {
    fn read(&mut self, destination: &mut [u8]) -> io::Result<usize> {
        if destination.is_empty() {
            return Ok(0);
        }
        while self.offset == self.current.len() {
            if self.eof {
                return Ok(0);
            }
            match self.receiver.recv() {
                Ok(chunk) if !chunk.is_empty() => {
                    self.current = chunk;
                    self.offset = 0;
                }
                Ok(_) => continue,
                Err(_) => {
                    self.eof = true;
                    return Ok(0);
                }
            }
        }
        let length = destination.len().min(self.current.len() - self.offset);
        destination[..length].copy_from_slice(&self.current[self.offset..self.offset + length]);
        self.offset += length;
        Ok(length)
    }
}

impl Seek for ChannelSource {
    fn seek(&mut self, _position: SeekFrom) -> io::Result<u64> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "encoded provider streams are not seekable",
        ))
    }
}

impl MediaSource for ChannelSource {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}

fn decode_stream(
    expected: EncodedFormat,
    maximum_audio_seconds: u64,
    input: Receiver<Vec<u8>>,
    output: &Sender<DecodedMessage>,
) -> Result<(), String> {
    let source = MediaSourceStream::new(
        Box::new(ChannelSource::new(input)),
        MediaSourceStreamOptions::default(),
    );
    let mut hint = Hint::new();
    hint.mime_type(expected.mime());
    let probed = default::get_probe()
        .format(
            &hint,
            source,
            &FormatOptions {
                enable_gapless: true,
                ..FormatOptions::default()
            },
            &MetadataOptions::default(),
        )
        .map_err(|_| "encoded audio has an invalid or unsupported container".to_owned())?;
    let mut reader = probed.format;
    let expected_codec = match expected {
        EncodedFormat::Mp3 => CODEC_TYPE_MP3,
        EncodedFormat::OggOpus => CODEC_TYPE_OPUS,
    };
    let track = reader
        .tracks()
        .iter()
        .find(|track| track.codec_params.codec == expected_codec)
        .ok_or_else(|| {
            "encoded audio container does not contain the negotiated codec".to_owned()
        })?;
    let track_id = track.id;
    let codec_params = track.codec_params.clone();
    let mut codecs = CodecRegistry::new();
    default::register_enabled_codecs(&mut codecs);
    codecs.register_all::<OpusDecoder>();
    let mut decoder = codecs
        .make(&codec_params, &DecoderOptions::default())
        .map_err(|_| "encoded audio codec parameters are unsupported".to_owned())?;

    let mut stream_format = None;
    let mut decoded_frames = 0_u64;
    let mut decoded_bytes = 0_u64;
    let mut produced_audio = false;
    loop {
        let packet = match reader.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(error)) if error.kind() == io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(_) => return Err("encoded audio container is malformed or truncated".into()),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = decoder
            .decode(&packet)
            .map_err(|_| "encoded audio payload is malformed or truncated".to_owned())?;
        if decoded.frames() == 0 {
            continue;
        }
        let spec = *decoded.spec();
        let channels = u16::try_from(spec.channels.count())
            .map_err(|_| "decoded channel count is unsupported".to_owned())?;
        if !(8_000..=96_000).contains(&spec.rate) || !(1..=2).contains(&channels) {
            return Err("decoded sample rate or channel count is unsupported".into());
        }
        if stream_format.get_or_insert((spec.rate, channels)) != &(spec.rate, channels) {
            return Err("decoded audio changes sample format mid-stream".into());
        }
        decoded_frames = decoded_frames
            .checked_add(decoded.frames() as u64)
            .ok_or_else(|| "decoded duration overflow".to_owned())?;
        if maximum_audio_seconds != 0
            && u128::from(decoded_frames)
                > u128::from(spec.rate) * u128::from(maximum_audio_seconds)
        {
            return Err("decoded audio exceeds the configured duration limit".into());
        }
        decoded_bytes = decoded_bytes
            .checked_add(decoded.frames() as u64 * u64::from(channels) * 2)
            .ok_or_else(|| "decoded audio size overflow".to_owned())?;
        if decoded_bytes > MAX_DECODED_AUDIO_BYTES {
            return Err("decoded audio exceeds the absolute decoded-size limit".into());
        }

        let mut samples = SampleBuffer::<i16>::new(decoded.capacity() as u64, spec);
        samples.copy_interleaved_ref(decoded);
        let mut bytes = Vec::with_capacity(samples.len() * 2);
        for sample in samples.samples() {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        if !bytes.is_empty() {
            output
                .send(DecodedMessage::Pcm {
                    sample_rate_hz: spec.rate,
                    channels,
                    bytes,
                })
                .map_err(|_| "decoded audio consumer stopped".to_owned())?;
            produced_audio = true;
        }
    }
    if !produced_audio {
        return Err("encoded audio contains no decodable samples".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MP3: &str = concat!(
        "SUQzBAAAAAAAI1RTU0UAAAAPAAADTGF2ZjYyLjEyLjEwMgAAAAAAAAAAAAAA//OEwAAAAAAAAAAAAElu",
        "Zm8AAAAPAAAABwAAA2AAVVVVVVVVVVVVVVVVVVVVVVVxcXFxcXFxcXFxcXFxcY6Ojo6Ojo6Ojo6Oj",
        "o6Oqqqqqqqqqqqqqqqqqqqqx8fHx8fHx8fHx8fHx8fj4+Pj4+Pj4+Pj4+Pj4///////////////",
        "////AAAAAExhdmM2Mi4yOAAAAAAAAAAAAAAAACQCQAAAAAAAAANg+cNTFgAAAAAAAAAAAAAAAAAAAAAA",
        "AAAAAAAA//NExAASUG54H1gYAFZJbgF3rvWHVOmOoO09uhbA1tNrTWkzlAR0Aa63fjcYlkojEspKSxg",
        "D4Pg+D9QIOy5/gg6c6fOcv5zp5cEHSgDD+TBDl/d/hjpVMAhQwuHqsc1B//NExAkUSTJsAZyAAGY7a",
        "x7I/AxBnyxgYVQB8JAmcSAYbCiWNGmzOBZKDbULCfxSQlIQsQH/GZGVJoixAv/MSdIqZF4vf/mJdLq",
        "QNN/KhIGhKEv+DSoOhJWhwn6h8EAD//NExAoTYFosVd4AAJa0BAGmB6FkYVA+RgnBomEqIkeTr3Bnv",
        "CHmFKFQYKoJxgtgTGAAAeWRL0q3ONIsfWh2pqvjf+j7v/Z6Ef2b/d9n3/6aLc1wlBnSQ21hL8AAgx",
        "CT//NExA8SaFIoVue0QM6H+wUrTBPFHPCL6ExtQqTkKjIiS0CY67Gdw/LD+Nayr7fdveruT/MIxYqq",
        "2n2/9L1FDG6709NmvGilVYpmAQAsHAAxgEABwYDoBzGCZAVxg+gO//NExBgVoFogAV8AAMGCFBBRhx",
        "4XaZe2wCGILCfhhsQM8YGMBDAABtMBxAOzAHwBkiAA1tobrro6/br/Vc/60lv//R8n2627/G/+tf//",
        "xtjSIGcSsgMcBpGAMO/reyFp//NExBQW8nKUAZpoAP+EgEwHh/iZjnJccn/j0KA5yXN//x6Fw0Jcvm",
        "//+ShcNC+Xzcuf//lxAvl9MuGiBf////TTLiCBummaIIG///h8EAGHwQAYfQIEIMMMIAByQvj4//NEx",
        "AsVOqqMVZNQABs6QZfCyULY+FEAWArfhQgFQGRBf4NwiiERIs/+RCKFohESS//kQiiYqPSVP/8fOo9M",
        "Q5f//5qTlyI2Qnf///yJZCccRGmkJxxETEFNRTMuMTAw"
    );
    const OPUS: &str = concat!(
        "T2dnUwACAAAAAAAAAABZgMGhAAAAAAcJQ48BE09wdXNIZWFkAQE4AYC7AAAAAABPZ2dTAAAAAAAAAAAA",
        "AFmAwaEBAAAAplB7mAE+T3B1c1RhZ3MNAAAATGF2ZjYyLjEyLjEwMgEAAAAdAAAAZW5jb2Rlcj1MYXZj",
        "NjIuMjguMTAyIGxpYm9wdXNPZ2dTAAS4FwAAAAAAAFmAwaECAAAAzy9YGAdfP0A+RTwveIGnXWyemawA",
        "AAgK4Fc5eAj+oI/KPI7NLS+BNjWbAb32eUeWk0FM13r1/3aC8RChw0WgDZIlr/LHwem56mZ43FdT",
        "6X566lHTS6DV6p8HnADD9vrQYnFlmls0gNiGLEV4n2cB5/yVTUqet0Pvc7OsvZHNzeFLWeT9qcy5yl",
        "ArqEl6pfrSdiZWmmmdcU/nHndIEB70c0W2zW4WRtcwa9B4mrLfdZz8Szo43erLRkivqUvrGS4jgNrE",
        "Af1SHkC09xd9XBxxfMIDrrQo8r1iJF9hYmjMZLugGP7jDOJjS2NfeJqy33Wc/ElJEDj0PaJIxfIvwAu",
        "IlopLeZHJjQYhj0rvVVzQRWLyjDoOBGpglpoYoCMKXSQ1R0FlTnkrvU94mrLfdZz8RdCz8q3ABmnX",
        "OkSf7egiPeLYMQryoAC6ghQbFnbx+tPv5rL8THJXoPK+eLoQ+/hMaGLD4UrfCmbGJjPzcE14mrLfdZ",
        "z8STLIzwacz6hpyyekMUOEw0On/LtlvECczQNaGXKUrHOf7f5V0ipB2/GfgsgY6Cwpv+M6Q2B4BlcY",
        "h/MnKVgqDor3gvaJJud1TcdPX1D9ktv8E/8fEkc4nnx5V9DWpESZHuBmjg=="
    );

    fn base64(value: &str) -> Vec<u8> {
        fn sextet(byte: u8) -> u8 {
            match byte {
                b'A'..=b'Z' => byte - b'A',
                b'a'..=b'z' => byte - b'a' + 26,
                b'0'..=b'9' => byte - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                _ => 0,
            }
        }
        let mut output = Vec::new();
        for group in value.as_bytes().chunks_exact(4) {
            let bits = u32::from(sextet(group[0])) << 18
                | u32::from(sextet(group[1])) << 12
                | u32::from(sextet(group[2])) << 6
                | u32::from(sextet(group[3]));
            output.push((bits >> 16) as u8);
            if group[2] != b'=' {
                output.push((bits >> 8) as u8);
            }
            if group[3] != b'=' {
                output.push(bits as u8);
            }
        }
        output
    }

    fn decode(format: EncodedFormat, bytes: Vec<u8>) -> Result<usize, String> {
        let mut decoder = EncodedDecoder::spawn(format, 1)?;
        let split = bytes.len() / 3;
        for chunk in [
            bytes[..split].to_vec(),
            bytes[split..split * 2].to_vec(),
            bytes[split * 2..].to_vec(),
        ] {
            decoder.input.as_ref().unwrap().send(chunk).unwrap();
        }
        decoder.finish_input();
        let mut decoded = 0;
        loop {
            match decoder.output.recv().unwrap() {
                DecodedMessage::Pcm { bytes, .. } => decoded += bytes.len(),
                DecodedMessage::Complete => return Ok(decoded),
                DecodedMessage::Failed(error) => return Err(error),
            }
        }
    }

    #[test]
    fn decodes_mp3_across_transport_boundaries() {
        assert!(decode(EncodedFormat::Mp3, base64(MP3)).unwrap() > 0);
    }

    #[test]
    fn decodes_ogg_opus_across_transport_boundaries() {
        assert!(decode(EncodedFormat::OggOpus, base64(OPUS)).unwrap() > 0);
    }

    #[test]
    fn rejects_truncated_ogg_opus() {
        let mut bytes = base64(OPUS);
        bytes.truncate(bytes.len() - 20);
        assert!(decode(EncodedFormat::OggOpus, bytes).is_err());
    }
}
