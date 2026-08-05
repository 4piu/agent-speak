use std::path::PathBuf;

use clap::ValueEnum;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 1;
pub const MAXIMUM_PRESETS: usize = 256;
pub const MAXIMUM_QUEUE_ITEMS: usize = 1_024;
pub const MAXIMUM_FILE_BYTES: u64 = 1_073_741_824;
pub const MAXIMUM_AUDIO_SECONDS: u64 = 86_400;
pub const MAXIMUM_TEXT_CHARACTERS: usize = 10_000;
pub const MAXIMUM_PLAYS_PER_MINUTE: u32 = 10_000;

/// A version-one profile as represented in TOML.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileConfig {
    pub schema_version: u32,
    pub profile_name: String,
    pub permissions: PermissionsConfig,
    pub playback: PlaybackConfig,
    pub tts: TtsConfig,
    pub logging: LoggingConfig,
    #[serde(default)]
    pub presets: Vec<PresetConfig>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionsConfig {
    pub arbitrary_text: bool,
    pub arbitrary_local_audio: bool,
    #[serde(default)]
    pub approved_directories: Vec<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlaybackConfig {
    pub minimum_gain: f64,
    pub maximum_gain: f64,
    pub default_gain: f64,
    pub default_concurrency: ConcurrencyMode,
    pub allowed_concurrency: Vec<ConcurrencyMode>,
    pub maximum_queue_items: usize,
    pub maximum_file_bytes: u64,
    pub maximum_audio_seconds: u64,
    pub maximum_plays_per_minute: u32,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ConcurrencyMode {
    Enqueue,
    Interrupt,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TtsConfig {
    pub enabled: bool,
    #[serde(default)]
    pub voice_id: String,
    pub maximum_characters: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingConfig {
    pub level: LogLevel,
    pub history_enabled: bool,
    #[serde(default)]
    pub history_path: Option<PathBuf>,
    pub history_include_spoken_text: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[value(rename_all = "lower")]
pub enum LogLevel {
    Error,
    Warning,
    Info,
    Debug,
    Trace,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PresetConfig {
    pub id: String,
    pub kind: PresetKind,
    #[serde(default)]
    pub source: Option<PathBuf>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub description: String,
    pub default_gain: f64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PresetKind {
    AudioFile,
    Text,
}

/// Sanitized, model-visible capability projection.
#[derive(Clone, Debug, Serialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EffectiveCapabilities {
    pub schema_version: u32,
    pub profile_name: String,
    pub tools: Vec<String>,
    pub permissions: EffectivePermissions,
    pub presets_available: bool,
    pub audio: AudioCapabilities,
    pub playback: PlaybackCapabilities,
    pub tts: TtsCapabilities,
    pub history_enabled: bool,
}

#[derive(Clone, Debug, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EffectivePermissions {
    pub arbitrary_text: bool,
    pub arbitrary_local_audio: bool,
}

#[derive(Clone, Debug, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AudioCapabilities {
    pub formats: Vec<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PlaybackCapabilities {
    pub minimum_gain: f64,
    pub maximum_gain: f64,
    pub default_gain: f64,
    pub default_concurrency: ConcurrencyMode,
    pub allowed_concurrency: Vec<ConcurrencyMode>,
    pub maximum_queue_items: usize,
    pub maximum_file_bytes: u64,
    pub maximum_audio_seconds: u64,
    pub maximum_plays_per_minute: u32,
}

#[derive(Clone, Debug, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TtsCapabilities {
    pub enabled: bool,
    pub maximum_characters: usize,
}

/// Sanitized entry returned by the preset-list tool.
#[derive(Clone, Debug, Serialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PresetSummary {
    pub id: String,
    pub kind: PresetKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub default_gain: f64,
}

impl ProfileConfig {
    pub fn derive_capabilities(&self) -> EffectiveCapabilities {
        let has_presets = !self.presets.is_empty();
        let arbitrary_text = self.permissions.arbitrary_text && self.tts.enabled;
        let arbitrary_local_audio = self.permissions.arbitrary_local_audio;

        let mut tools = vec!["get_audio_capabilities".to_owned()];
        if has_presets {
            tools.extend([
                "list_audio_presets".to_owned(),
                "play_audio_preset".to_owned(),
            ]);
        }
        if arbitrary_text {
            tools.push("speak_text".to_owned());
        }
        if arbitrary_local_audio {
            tools.push("play_audio_source".to_owned());
        }

        EffectiveCapabilities {
            schema_version: self.schema_version,
            profile_name: self.profile_name.clone(),
            tools,
            permissions: EffectivePermissions {
                arbitrary_text,
                arbitrary_local_audio,
            },
            presets_available: has_presets,
            audio: AudioCapabilities {
                formats: ["wav", "mp3", "flac", "ogg_vorbis"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
            },
            playback: PlaybackCapabilities {
                minimum_gain: self.playback.minimum_gain,
                maximum_gain: self.playback.maximum_gain,
                default_gain: self.playback.default_gain,
                default_concurrency: self.playback.default_concurrency,
                allowed_concurrency: self.playback.allowed_concurrency.clone(),
                maximum_queue_items: self.playback.maximum_queue_items,
                maximum_file_bytes: self.playback.maximum_file_bytes,
                maximum_audio_seconds: self.playback.maximum_audio_seconds,
                maximum_plays_per_minute: self.playback.maximum_plays_per_minute,
            },
            tts: TtsCapabilities {
                enabled: self.tts.enabled,
                maximum_characters: self.tts.maximum_characters,
            },
            history_enabled: self.logging.history_enabled,
        }
    }

    pub fn preset_summaries(&self) -> Vec<PresetSummary> {
        self.presets
            .iter()
            .map(|preset| PresetSummary {
                id: preset.id.clone(),
                kind: preset.kind,
                description: (!preset.description.is_empty()).then(|| preset.description.clone()),
                default_gain: preset.default_gain,
            })
            .collect()
    }
}
