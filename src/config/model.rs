use std::path::PathBuf;

use clap::ValueEnum;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

pub const SCHEMA_VERSION: u32 = 1;
pub const MAXIMUM_AUDIO_CUES: usize = 256;
pub const MAXIMUM_QUEUE_ITEMS: usize = 1_024;
pub const MAXIMUM_TEXT_CHARACTERS: usize = 10_000;

/// An Agent Speak profile as represented in TOML.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileConfig {
    pub schema_version: u32,
    pub profile_name: String,
    pub permissions: PermissionsConfig,
    pub playback: PlaybackConfig,
    pub outputs: OutputsConfig,
    pub tts: TtsConfig,
    pub logging: LoggingConfig,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audio_cues: Vec<AudioCueConfig>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OutputsConfig {
    pub default_target: String,
    pub targets: Vec<OutputTargetConfig>,
}

impl Default for OutputsConfig {
    fn default() -> Self {
        Self {
            default_target: "system".to_owned(),
            targets: vec![OutputTargetConfig {
                id: "system".to_owned(),
                description: "Current system default audio device".to_owned(),
                kind: OutputTargetKind::SystemDefault,
                device_id: None,
                allow: vec![OutputCategory::Audio, OutputCategory::Speech],
            }],
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OutputTargetConfig {
    pub id: String,
    pub description: String,
    pub kind: OutputTargetKind,
    #[serde(default)]
    pub device_id: Option<String>,
    pub allow: Vec<OutputCategory>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutputTargetKind {
    SystemDefault,
    Device,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum OutputCategory {
    Audio,
    Speech,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionsConfig {
    pub arbitrary_text: bool,
    pub arbitrary_local_audio: bool,
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
    /// Zero disables the decoded-duration limit.
    #[serde(default)]
    pub maximum_audio_seconds: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ConcurrencyMode {
    Enqueue,
    Interrupt,
}

#[derive(Clone, Debug, Serialize)]
pub struct TtsConfig {
    pub enabled: bool,
    #[serde(flatten)]
    pub backend: TtsBackend,
    pub maximum_characters: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TtsBackend {
    System(SystemTtsConfig),
    Utterpipe(UtterPipeTtsConfig),
}

impl Serialize for TtsBackend {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Serialize)]
        struct SystemBackend<'a> {
            backend: &'static str,
            #[serde(flatten)]
            config: &'a SystemTtsConfig,
        }

        #[derive(Serialize)]
        struct UtterPipeBackend<'a> {
            backend: String,
            model_id: &'a str,
            voice_id: &'a str,
            #[serde(skip_serializing_if = "Vec::is_empty")]
            provider_environment: &'a Vec<String>,
            #[serde(skip_serializing_if = "toml::Table::is_empty")]
            provider_options: &'a toml::Table,
        }

        match self {
            Self::System(config) => SystemBackend {
                backend: "system",
                config,
            }
            .serialize(serializer),
            Self::Utterpipe(config) => UtterPipeBackend {
                backend: format!("utterpipe-{}", config.provider),
                model_id: &config.model_id,
                voice_id: &config.voice_id,
                provider_environment: &config.provider_environment,
                provider_options: &config.provider_options,
            }
            .serialize(serializer),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SystemTtsConfig {
    #[serde(default)]
    pub voice_id: String,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct UtterPipeTtsConfig {
    pub provider: String,
    pub model_id: String,
    pub voice_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_environment: Vec<String>,
    #[serde(default, skip_serializing_if = "toml::Table::is_empty")]
    pub provider_options: toml::Table,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTtsConfig {
    enabled: bool,
    maximum_characters: usize,
    backend: String,
    #[serde(default)]
    voice_id: Option<String>,
    #[serde(default)]
    model_id: Option<String>,
    #[serde(default)]
    provider_environment: Option<Vec<String>>,
    #[serde(default)]
    provider_options: Option<toml::Table>,
}

impl<'de> Deserialize<'de> for TtsConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawTtsConfig::deserialize(deserializer)?;
        let backend = match raw.backend.as_str() {
            "system" => {
                if raw.model_id.is_some()
                    || raw.provider_environment.is_some()
                    || raw.provider_options.is_some()
                {
                    return Err(D::Error::custom(
                        "provider fields are not allowed for the system TTS backend",
                    ));
                }
                TtsBackend::System(SystemTtsConfig {
                    voice_id: raw.voice_id.unwrap_or_default(),
                })
            }
            name if name.starts_with("utterpipe-") => TtsBackend::Utterpipe(UtterPipeTtsConfig {
                provider: name["utterpipe-".len()..].to_owned(),
                model_id: raw
                    .model_id
                    .ok_or_else(|| D::Error::missing_field("model_id"))?,
                voice_id: raw
                    .voice_id
                    .ok_or_else(|| D::Error::missing_field("voice_id"))?,
                provider_environment: raw.provider_environment.unwrap_or_default(),
                provider_options: raw.provider_options.unwrap_or_default(),
            }),
            _ => {
                return Err(D::Error::custom(
                    "backend must be 'system' or an 'utterpipe-<provider>' executable name",
                ));
            }
        };
        Ok(Self {
            enabled: raw.enabled,
            backend,
            maximum_characters: raw.maximum_characters,
        })
    }
}

impl TtsConfig {
    pub fn voice_id(&self) -> &str {
        match &self.backend {
            TtsBackend::System(config) => &config.voice_id,
            TtsBackend::Utterpipe(config) => &config.voice_id,
        }
    }

    pub fn utterpipe(&self) -> Option<&UtterPipeTtsConfig> {
        match &self.backend {
            TtsBackend::System(_) => None,
            TtsBackend::Utterpipe(config) => Some(config),
        }
    }
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
pub struct AudioCueConfig {
    pub id: String,
    pub kind: AudioCueKind,
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
pub enum AudioCueKind {
    AudioFile,
    Speech,
}

/// Sanitized, model-visible capability projection.
#[derive(Clone, Debug, Serialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EffectiveCapabilities {
    pub schema_version: u32,
    pub profile_name: String,
    pub tools: Vec<String>,
    pub permissions: EffectivePermissions,
    pub audio_cues_available: bool,
    pub audio: AudioCapabilities,
    pub outputs: OutputCapabilities,
    pub playback: PlaybackCapabilities,
    pub tts: TtsCapabilities,
    pub history_enabled: bool,
}

#[derive(Clone, Debug, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OutputCapabilities {
    pub default_target: String,
    pub targets: Vec<OutputTargetSummary>,
}

#[derive(Clone, Debug, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OutputTargetSummary {
    pub id: String,
    pub description: String,
    pub allow: Vec<OutputCategory>,
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
    pub maximum_audio_seconds: u64,
}

#[derive(Clone, Debug, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TtsCapabilities {
    pub enabled: bool,
    pub maximum_characters: usize,
}

/// Sanitized entry returned by the audio-cue-list tool.
#[derive(Clone, Debug, Serialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AudioCueSummary {
    pub id: String,
    pub kind: AudioCueKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub default_gain: f64,
}

impl ProfileConfig {
    pub fn derive_capabilities(&self) -> EffectiveCapabilities {
        let has_audio_cues = !self.audio_cues.is_empty();
        let arbitrary_text = self.permissions.arbitrary_text && self.tts.enabled;
        let arbitrary_local_audio = self.permissions.arbitrary_local_audio;

        let mut tools = vec!["get_audio_capabilities".to_owned()];
        if has_audio_cues {
            tools.extend(["list_audio_cues".to_owned(), "play_audio_cue".to_owned()]);
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
            audio_cues_available: has_audio_cues,
            audio: AudioCapabilities {
                formats: ["wav", "mp3", "flac", "ogg_vorbis"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
            },
            outputs: OutputCapabilities {
                default_target: self.outputs.default_target.clone(),
                targets: self
                    .outputs
                    .targets
                    .iter()
                    .map(|target| OutputTargetSummary {
                        id: target.id.clone(),
                        description: target.description.clone(),
                        allow: target.allow.clone(),
                    })
                    .collect(),
            },
            playback: PlaybackCapabilities {
                minimum_gain: self.playback.minimum_gain,
                maximum_gain: self.playback.maximum_gain,
                default_gain: self.playback.default_gain,
                default_concurrency: self.playback.default_concurrency,
                allowed_concurrency: self.playback.allowed_concurrency.clone(),
                maximum_queue_items: self.playback.maximum_queue_items,
                maximum_audio_seconds: self.playback.maximum_audio_seconds,
            },
            tts: TtsCapabilities {
                enabled: self.tts.enabled,
                maximum_characters: self.tts.maximum_characters,
            },
            history_enabled: self.logging.history_enabled,
        }
    }

    pub fn audio_cue_summaries(&self) -> Vec<AudioCueSummary> {
        self.audio_cues
            .iter()
            .map(|cue| AudioCueSummary {
                id: cue.id.clone(),
                kind: cue.kind,
                description: (!cue.description.is_empty()).then(|| cue.description.clone()),
                default_gain: cue.default_gain,
            })
            .collect()
    }
}
