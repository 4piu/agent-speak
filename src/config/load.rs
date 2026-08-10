use std::{
    env, fmt, fs, io,
    path::{Path, PathBuf},
};

use thiserror::Error;

use super::{
    ConcurrencyMode, EffectiveCapabilities, LogLevel, LoggingConfig, OutputsConfig,
    PermissionsConfig, PlaybackConfig, ProfileConfig, SCHEMA_VERSION, TtsConfig, ValidationIssue,
    resolve_and_validate,
};

#[derive(Clone, Debug, Default)]
pub struct QuickProfileOverrides {
    pub voice_id: Option<String>,
    pub minimum_gain: Option<f64>,
    pub maximum_gain: Option<f64>,
    pub default_gain: Option<f64>,
    pub maximum_text_characters: Option<usize>,
    pub log_level: Option<LogLevel>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigOrigin {
    QuickProfile,
    File(PathBuf),
    Layered(Vec<PathBuf>),
}

impl fmt::Display for ConfigOrigin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::QuickProfile => formatter.write_str("built-in quick profile"),
            Self::File(path) => write!(formatter, "explicit file {}", path.display()),
            Self::Layered(paths) => {
                formatter.write_str("discovered layers (low to high): ")?;
                for (index, path) in paths.iter().enumerate() {
                    if index != 0 {
                        formatter.write_str(", ")?;
                    }
                    write!(formatter, "{}", path.display())?;
                }
                Ok(())
            }
        }
    }
}

/// A profile whose static invariants and configured filesystem paths have been
/// checked. Decoder and platform-backend checks still belong to server startup.
#[derive(Clone, Debug)]
pub struct ValidatedConfig {
    profile: ProfileConfig,
    capabilities: EffectiveCapabilities,
    origin: ConfigOrigin,
}

impl ValidatedConfig {
    pub fn profile(&self) -> &ProfileConfig {
        &self.profile
    }

    pub fn into_profile(self) -> ProfileConfig {
        self.profile
    }

    pub fn capabilities(&self) -> &EffectiveCapabilities {
        &self.capabilities
    }

    pub fn origin(&self) -> &ConfigOrigin {
        &self.origin
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not read the configuration file: {0}")]
    Read(#[source] io::Error),
    #[error("configuration is not valid TOML: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("could not determine the working directory for configuration discovery: {0}")]
    CurrentDirectory(#[source] io::Error),
    #[error("could not read discovered configuration layer {path}: {source}")]
    LayerRead {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("discovered configuration layer {path} is not valid TOML: {source}")]
    LayerParse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("built-in configuration defaults could not be represented as TOML: {0}")]
    DefaultSerialize(#[source] toml::ser::Error),
    #[error("configuration path has no parent directory")]
    MissingParent,
    #[error("configuration validation failed:\n{0}")]
    Validation(ValidationErrors),
}

#[derive(Clone, Debug)]
pub struct ValidationErrors(pub Vec<ValidationIssue>);

impl fmt::Display for ValidationErrors {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, issue) in self.0.iter().enumerate() {
            if index != 0 {
                formatter.write_str("\n")?;
            }
            write!(formatter, "- {}: {}", issue.field, issue.message)?;
        }
        Ok(())
    }
}

pub fn load_config(path: impl AsRef<Path>) -> Result<ValidatedConfig, ConfigError> {
    let canonical_path = fs::canonicalize(path.as_ref()).map_err(ConfigError::Read)?;
    let configuration_directory = canonical_path
        .parent()
        .ok_or(ConfigError::MissingParent)?
        .to_owned();
    let source = fs::read_to_string(&canonical_path).map_err(ConfigError::Read)?;
    parse_config(
        &source,
        &configuration_directory,
        ConfigOrigin::File(canonical_path),
    )
}

/// Load and merge the platform system, user, and working-directory layers.
/// Missing layers are skipped; `None` means no layer exists.
pub fn load_discovered_config() -> Result<Option<ValidatedConfig>, ConfigError> {
    let working_directory = env::current_dir().map_err(ConfigError::CurrentDirectory)?;
    let candidates = discovery_candidates(&working_directory);
    load_discovered_config_from(&candidates)
}

fn discovery_candidates(working_directory: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::with_capacity(3);
    #[cfg(not(windows))]
    paths.push(PathBuf::from("/etc/agent-speak.toml"));
    #[cfg(windows)]
    if let Some(program_data) = env::var_os("ProgramData") {
        let program_data = PathBuf::from(program_data);
        if program_data.is_absolute() {
            paths.push(program_data.join("Agent Speak").join("agent-speak.toml"));
        }
    }

    #[cfg(windows)]
    let home = ["USERPROFILE", "HOME"]
        .into_iter()
        .filter_map(env::var_os)
        .map(PathBuf::from)
        .find(|path| path.is_absolute());
    #[cfg(not(windows))]
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute());
    if let Some(home) = home {
        paths.push(home.join(".agent-speak.toml"));
    }
    paths.push(working_directory.join(".agent-speak.toml"));
    paths
}

#[derive(Default)]
struct PathOrigins {
    history: Option<PathBuf>,
    audio_cues: Option<PathBuf>,
}

fn load_discovered_config_from(
    candidates: &[PathBuf],
) -> Result<Option<ValidatedConfig>, ConfigError> {
    let defaults = quick_profile(QuickProfileOverrides::default())?.into_profile();
    let mut effective =
        match toml::Value::try_from(defaults).map_err(ConfigError::DefaultSerialize)? {
            toml::Value::Table(table) => table,
            _ => unreachable!("ProfileConfig always serializes as a TOML table"),
        };
    let mut sources = Vec::new();
    let mut path_origins = PathOrigins::default();

    for candidate in candidates {
        let canonical_path = match fs::canonicalize(candidate) {
            Ok(path) => path,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(ConfigError::LayerRead {
                    path: candidate.clone(),
                    source,
                });
            }
        };
        if sources.contains(&canonical_path) {
            continue;
        }
        let source =
            fs::read_to_string(&canonical_path).map_err(|source| ConfigError::LayerRead {
                path: canonical_path.clone(),
                source,
            })?;
        let layer: toml::Table =
            toml::from_str(&source).map_err(|source| ConfigError::LayerParse {
                path: canonical_path.clone(),
                source,
            })?;
        let directory = canonical_path
            .parent()
            .ok_or(ConfigError::MissingParent)?
            .to_owned();

        if layer
            .get("logging")
            .and_then(toml::Value::as_table)
            .is_some_and(|logging| logging.contains_key("history_path"))
        {
            path_origins.history = Some(directory.clone());
        }
        if layer.contains_key("audio_cues") {
            path_origins.audio_cues = Some(directory);
        }

        reset_tts_variant_on_backend_change(&mut effective, &layer);
        merge_tables(&mut effective, layer);
        sources.push(canonical_path);
    }

    if sources.is_empty() {
        return Ok(None);
    }

    let mut profile: ProfileConfig = toml::Value::Table(effective).try_into()?;
    if let (Some(path), Some(directory)) = (
        &mut profile.logging.history_path,
        path_origins.history.as_deref(),
    ) && path.is_relative()
    {
        *path = directory.join(&*path);
    }
    if let Some(directory) = path_origins.audio_cues.as_deref() {
        for cue in &mut profile.audio_cues {
            if let Some(path) = &mut cue.source
                && path.is_relative()
            {
                *path = directory.join(&*path);
            }
        }
    }

    finish_validation(profile, Path::new("."), ConfigOrigin::Layered(sources)).map(Some)
}

fn merge_tables(base: &mut toml::Table, incoming: toml::Table) {
    for (key, value) in incoming {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(base)), toml::Value::Table(incoming)) => {
                merge_tables(base, incoming);
            }
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

fn reset_tts_variant_on_backend_change(base: &mut toml::Table, incoming: &toml::Table) {
    let Some(incoming_tts) = incoming.get("tts").and_then(toml::Value::as_table) else {
        return;
    };
    let Some(incoming_backend) = incoming_tts.get("backend") else {
        return;
    };
    let Some(base_tts) = base.get_mut("tts").and_then(toml::Value::as_table_mut) else {
        return;
    };
    if base_tts.get("backend") == Some(incoming_backend) {
        return;
    }
    for key in [
        "voice_id",
        "audio_deliveries",
        "provider_environment",
        "provider_options",
        "utterance_options",
        "agent_utterance_options",
    ] {
        base_tts.remove(key);
    }
}

/// Parse profile text with an explicit base directory. This is primarily useful
/// for embedding and tests; `load_config` is the normal file-based entry point.
pub fn parse_config(
    source: &str,
    configuration_directory: &Path,
    origin: ConfigOrigin,
) -> Result<ValidatedConfig, ConfigError> {
    let profile: ProfileConfig = toml::from_str(source)?;
    finish_validation(profile, configuration_directory, origin)
}

pub fn quick_profile(overrides: QuickProfileOverrides) -> Result<ValidatedConfig, ConfigError> {
    let profile = ProfileConfig {
        schema_version: SCHEMA_VERSION,
        profile_name: "quickstart".to_owned(),
        permissions: PermissionsConfig {
            arbitrary_text: true,
            arbitrary_local_audio: false,
        },
        playback: PlaybackConfig {
            minimum_gain: overrides.minimum_gain.unwrap_or(0.0),
            maximum_gain: overrides.maximum_gain.unwrap_or(0.7),
            default_gain: overrides.default_gain.unwrap_or(0.4),
            default_concurrency: ConcurrencyMode::Enqueue,
            allowed_concurrency: vec![ConcurrencyMode::Enqueue, ConcurrencyMode::Interrupt],
            maximum_queue_items: 16,
            maximum_mix_streams: super::DEFAULT_MAXIMUM_MIX_STREAMS,
            maximum_audio_seconds: 0,
        },
        outputs: OutputsConfig::default(),
        tts: TtsConfig {
            enabled: true,
            backend: quick_tts_backend(overrides.voice_id),
            maximum_characters: overrides.maximum_text_characters.unwrap_or(300),
        },
        logging: LoggingConfig {
            level: overrides.log_level.unwrap_or(LogLevel::Warning),
            history_enabled: false,
            history_path: None,
            history_include_spoken_text: false,
        },
        audio_cues: Vec::new(),
    };

    // Quick profile contains no paths, so its base is deliberately irrelevant.
    finish_validation(profile, Path::new("."), ConfigOrigin::QuickProfile)
}

fn quick_tts_backend(voice_id: Option<String>) -> super::TtsBackend {
    #[cfg(target_os = "linux")]
    {
        super::TtsBackend::Utterpipe(super::UtterPipeTtsConfig {
            provider: "espeak-ng".to_owned(),
            audio_deliveries: Vec::new(),
            provider_environment: Vec::new(),
            provider_options: toml::Table::new(),
            utterance_options: toml::Table::from_iter([(
                "voice".into(),
                toml::Value::String(voice_id.unwrap_or_else(|| "default".to_owned())),
            )]),
            agent_utterance_options: Vec::new(),
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        super::TtsBackend::System(super::SystemTtsConfig {
            voice_id: voice_id.unwrap_or_default(),
        })
    }
}

fn finish_validation(
    profile: ProfileConfig,
    configuration_directory: &Path,
    origin: ConfigOrigin,
) -> Result<ValidatedConfig, ConfigError> {
    let profile = resolve_and_validate(profile, configuration_directory)
        .map_err(|issues| ConfigError::Validation(ValidationErrors(issues)))?;
    let capabilities = profile.derive_capabilities();
    Ok(ValidatedConfig {
        profile,
        capabilities,
        origin,
    })
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    const VALID: &str = r#"
schema_version = 1
profile_name = "default"

[permissions]
arbitrary_text = false
arbitrary_local_audio = false

[playback]
minimum_gain = 0.2
maximum_gain = 0.7
default_gain = 0.4
default_concurrency = "enqueue"
allowed_concurrency = ["enqueue", "interrupt"]
maximum_queue_items = 16
maximum_mix_streams = 2
maximum_audio_seconds = 0

[outputs]
default_target = "system"

[[outputs.targets]]
id = "system"
description = "Current system default audio device"
kind = "system_default"
allow = ["audio", "speech"]

[tts]
enabled = true
backend = "system"
voice_id = ""
maximum_characters = 300

[logging]
level = "warning"
history_enabled = false
history_include_spoken_text = false
"#;

    const DEFAULT_OUTPUTS: &str = r#"[outputs]
default_target = "system"

[[outputs.targets]]
id = "system"
description = "Current system default audio device"
kind = "system_default"
allow = ["audio", "speech"]
"#;

    #[test]
    fn strict_parser_accepts_current_profile() {
        let config = parse_config(VALID, Path::new("."), ConfigOrigin::QuickProfile).unwrap();
        assert_eq!(config.profile().profile_name, "default");
    }

    #[test]
    fn published_examples_are_valid_profiles() {
        for (name, source) in [
            (
                "text-profile.toml",
                include_str!("../../examples/text-profile.toml"),
            ),
            (
                "espeak-provider.toml",
                include_str!("../../examples/espeak-provider.toml"),
            ),
            (
                "openai-http-provider.toml",
                include_str!("../../examples/openai-http-provider.toml"),
            ),
            (
                "pocket-provider.toml",
                include_str!("../../examples/pocket-provider.toml"),
            ),
        ] {
            parse_config(source, Path::new("."), ConfigOrigin::QuickProfile)
                .unwrap_or_else(|error| panic!("{name} is invalid: {error}"));
        }
    }

    #[test]
    fn utterpipe_executable_name_is_the_backend() {
        let source = VALID.replace(
                "[tts]\nenabled = true\nbackend = \"system\"\nvoice_id = \"\"\nmaximum_characters = 300",
                r#"[tts]
enabled = true
backend = "utterpipe-pocket-tts"
maximum_characters = 300
provider_environment = ["POCKET_TOKEN"]
agent_utterance_options = ["speed"]
audio_deliveries = [{ mode = "incremental", format = "audio/pcm;codec=pcm_s16le" }]

[tts.provider_options]
model = "english"
sample_rate_hz = 24000

[tts.utterance_options]
voice = "my-voice"
speed = 1.1"#,
            );
        let config = parse_config(&source, Path::new("."), ConfigOrigin::QuickProfile).unwrap();
        let provider = config.profile().tts.utterpipe().unwrap();
        assert_eq!(provider.provider, "pocket-tts");
        assert_eq!(provider.provider_environment, ["POCKET_TOKEN"]);
        assert_eq!(provider.agent_utterance_options, ["speed"]);
        assert_eq!(provider.audio_deliveries[0].mode, "incremental");
        assert_eq!(provider.provider_options["model"].as_str(), Some("english"));
        assert_eq!(
            provider.utterance_options["voice"].as_str(),
            Some("my-voice")
        );
        assert_eq!(
            provider.provider_options["sample_rate_hz"].as_integer(),
            Some(24000)
        );

        let rendered = toml::to_string_pretty(config.profile()).unwrap();
        assert!(rendered.contains("backend = \"utterpipe-pocket-tts\""));
        assert!(!rendered.contains("provider ="));
    }

    #[test]
    fn system_backend_rejects_external_fields_during_parse() {
        let source = VALID.replace("voice_id = \"\"", "provider_options = {}\nvoice_id = \"\"");
        assert!(matches!(
            parse_config(&source, Path::new("."), ConfigOrigin::QuickProfile),
            Err(ConfigError::Parse(_))
        ));
    }

    #[test]
    fn profile_requires_an_explicit_backend() {
        let source = VALID.replace("backend = \"system\"\n", "");
        assert!(matches!(
            parse_config(&source, Path::new("."), ConfigOrigin::QuickProfile),
            Err(ConfigError::Parse(_))
        ));
    }

    #[test]
    fn omitted_audio_duration_limit_defaults_to_unlimited() {
        let source = VALID.replace("maximum_audio_seconds = 0\n", "");
        let config = parse_config(&source, Path::new("."), ConfigOrigin::QuickProfile).unwrap();

        assert_eq!(config.profile().playback.maximum_audio_seconds, 0);
        assert_eq!(config.capabilities().playback.maximum_audio_seconds, 0);
    }

    #[test]
    fn omitted_mix_stream_limit_defaults_to_two() {
        let source = VALID.replace("maximum_mix_streams = 2\n", "");
        let config = parse_config(&source, Path::new("."), ConfigOrigin::QuickProfile).unwrap();

        assert_eq!(config.profile().playback.maximum_mix_streams, 2);
        assert_eq!(config.capabilities().playback.maximum_mix_streams, 2);
    }

    #[test]
    fn file_profile_requires_outputs() {
        let source = VALID.replace(DEFAULT_OUTPUTS, "");
        assert!(matches!(
            parse_config(&source, Path::new("."), ConfigOrigin::QuickProfile),
            Err(ConfigError::Parse(_))
        ));
    }

    #[test]
    fn strict_parser_rejects_unknown_top_level_and_nested_fields() {
        let top = VALID.replace(
            "profile_name = \"default\"",
            "profile_name = \"default\"\nunexpected = true",
        );
        assert!(matches!(
            parse_config(&top, Path::new("."), ConfigOrigin::QuickProfile),
            Err(ConfigError::Parse(_))
        ));

        let nested = VALID.replace(
            "arbitrary_text = false",
            "arbitrary_text = false\ntypo = true",
        );
        assert!(matches!(
            parse_config(&nested, Path::new("."), ConfigOrigin::QuickProfile),
            Err(ConfigError::Parse(_))
        ));
    }

    #[test]
    fn rejects_unknown_enum_variants() {
        let source = VALID.replace(
            "default_concurrency = \"enqueue\"",
            "default_concurrency = \"overlap\"",
        );
        assert!(matches!(
            parse_config(&source, Path::new("."), ConfigOrigin::QuickProfile),
            Err(ConfigError::Parse(_))
        ));
    }

    #[test]
    fn parser_handles_generated_untrusted_text_without_panicking() {
        for end in 0..=VALID.len() {
            let _ = parse_config(&VALID[..end], Path::new("."), ConfigOrigin::QuickProfile);
        }

        let alphabet = b"[]{}=,.#\"'\\/\r\n\t abcdefghijklmnopqrstuvwxyz0123456789_-";
        let mut state = 0x6a09_e667_f3bc_c909_u64;
        for case in 0..512 {
            let length = case % 384;
            let mut source = String::with_capacity(length);
            for _ in 0..length {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                source.push(alphabet[state as usize % alphabet.len()] as char);
            }
            let _ = parse_config(&source, Path::new("."), ConfigOrigin::QuickProfile);
        }
    }

    #[test]
    fn quick_profile_matches_normative_defaults() {
        let config = quick_profile(QuickProfileOverrides::default()).unwrap();
        let profile = config.profile();
        assert_eq!(profile.schema_version, SCHEMA_VERSION);
        assert_eq!(profile.profile_name, "quickstart");
        assert!(profile.permissions.arbitrary_text);
        assert!(!profile.permissions.arbitrary_local_audio);
        assert_eq!(profile.playback.minimum_gain, 0.0);
        assert_eq!(profile.playback.maximum_gain, 0.7);
        assert_eq!(profile.playback.default_gain, 0.4);
        assert_eq!(profile.playback.maximum_queue_items, 16);
        assert_eq!(profile.playback.maximum_mix_streams, 2);
        assert_eq!(profile.playback.maximum_audio_seconds, 0);
        assert_eq!(profile.outputs.default_target, "system");
        assert_eq!(profile.outputs.targets.len(), 1);
        assert_eq!(profile.outputs.targets[0].id, "system");
        assert_eq!(profile.tts.maximum_characters, 300);
        #[cfg(target_os = "linux")]
        assert!(matches!(
            &profile.tts.backend,
            super::super::TtsBackend::Utterpipe(provider)
                if provider.provider == "espeak-ng"
                    && provider.utterance_options["voice"].as_str() == Some("default")
        ));
        #[cfg(not(target_os = "linux"))]
        assert!(matches!(
            profile.tts.backend,
            super::super::TtsBackend::System(_)
        ));
        assert_eq!(profile.logging.level, LogLevel::Warning);
        assert_eq!(
            config.capabilities().tools,
            [
                "cancel_playback",
                "get_audio_capabilities",
                "get_playback_status",
                "speak_text"
            ]
        );

        assert_eq!(
            serde_json::to_value(config.capabilities()).unwrap(),
            serde_json::json!({
                "schema_version": SCHEMA_VERSION,
                "profile_name": "quickstart",
                "tools": ["cancel_playback", "get_audio_capabilities", "get_playback_status", "speak_text"],
                "permissions": {
                    "arbitrary_text": true,
                    "arbitrary_local_audio": false
                },
                "audio_cues_available": false,
                "audio": {
                    "formats": ["wav", "mp3", "flac", "ogg_vorbis"]
                },
                "outputs": {
                    "default_target": "system",
                    "targets": [{
                        "id": "system",
                        "description": "Current system default audio device",
                        "allow": ["audio", "speech"]
                    }]
                },
                "playback": {
                    "minimum_gain": 0.0,
                    "maximum_gain": 0.7,
                    "default_gain": 0.4,
                    "default_concurrency": "enqueue",
                    "allowed_concurrency": ["enqueue", "interrupt"],
                    "maximum_queue_items": 16,
                    "maximum_mix_streams": 2,
                    "maximum_audio_seconds": 0,
                    "status_retention_items": 256
                },
                "tts": {
                    "enabled": true,
                    "maximum_characters": 300
                },
                "history_enabled": false
            })
        );
    }

    #[test]
    fn invalid_quick_override_uses_normal_validation() {
        let error = quick_profile(QuickProfileOverrides {
            minimum_gain: Some(0.8),
            maximum_gain: Some(0.2),
            ..QuickProfileOverrides::default()
        })
        .unwrap_err();
        assert!(matches!(error, ConfigError::Validation(_)));
    }

    #[test]
    fn capability_tools_are_derived_from_effective_policy() {
        let text_without_tts = VALID
            .replace("arbitrary_text = false", "arbitrary_text = true")
            .replace("enabled = true", "enabled = false");
        let config = parse_config(
            &text_without_tts,
            Path::new("."),
            ConfigOrigin::QuickProfile,
        )
        .unwrap();
        assert!(!config.capabilities().permissions.arbitrary_text);
        assert_eq!(
            config.capabilities().tools,
            [
                "cancel_playback",
                "get_audio_capabilities",
                "get_playback_status"
            ]
        );

        let cue = format!(
            "{VALID}\n[[audio_cues]]\nid = \"ready\"\nkind = \"speech\"\ntext = \"Ready\"\ndefault_gain = 0.4\n"
        );
        let config = parse_config(&cue, Path::new("."), ConfigOrigin::QuickProfile).unwrap();
        assert!(config.capabilities().audio_cues_available);
        assert_eq!(
            config.capabilities().tools,
            [
                "cancel_playback",
                "get_audio_capabilities",
                "get_playback_status",
                "list_audio_cues",
                "play_audio_cue"
            ]
        );
    }

    #[test]
    fn capabilities_and_audio_cue_summaries_are_sanitized() {
        let source = VALID.replace(
            DEFAULT_OUTPUTS,
            r#"[outputs]
default_target = "private-headset"

[[outputs.targets]]
id = "private-headset"
description = "Private headset"
kind = "device"
device_id = "secret-stable-device-id"
allow = ["speech"]
"#,
        );
        let profile_source = format!(
            "{source}\n[[audio_cues]]\nid = \"say-secret\"\nkind = \"speech\"\ntext = \"never expose this phrase\"\ndescription = \"\"\ndefault_gain = 0.4\n"
        );
        let config =
            parse_config(&profile_source, Path::new("."), ConfigOrigin::QuickProfile).unwrap();
        let json = serde_json::to_string(config.capabilities()).unwrap();
        for forbidden in [
            "history_path",
            "voice_id",
            "never expose this phrase",
            "secret-stable-device-id",
            "device_id",
            "kind",
        ] {
            assert!(!json.contains(forbidden), "leaked {forbidden}");
        }

        assert_eq!(
            serde_json::to_value(&config.capabilities().outputs).unwrap(),
            serde_json::json!({
                "default_target": "private-headset",
                "targets": [{
                    "id": "private-headset",
                    "description": "Private headset",
                    "allow": ["speech"]
                }]
            })
        );

        let summaries = config.profile().audio_cue_summaries();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].description, None);
        let summary_json = serde_json::to_string(&summaries).unwrap();
        assert!(!summary_json.contains("never expose this phrase"));
    }

    #[test]
    fn parser_rejects_unknown_output_kinds_and_categories() {
        let unknown_kind = VALID.replace(
            DEFAULT_OUTPUTS,
            r#"[outputs]
default_target = "system"

[[outputs.targets]]
id = "system"
description = "System"
kind = "automatic"
allow = ["audio"]
"#,
        );
        assert!(matches!(
            parse_config(&unknown_kind, Path::new("."), ConfigOrigin::QuickProfile),
            Err(ConfigError::Parse(_))
        ));

        let unknown_category = VALID.replace(
            DEFAULT_OUTPUTS,
            r#"[outputs]
default_target = "system"

[[outputs.targets]]
id = "system"
description = "System"
kind = "system_default"
allow = ["music"]
"#,
        );
        assert!(matches!(
            parse_config(
                &unknown_category,
                Path::new("."),
                ConfigOrigin::QuickProfile
            ),
            Err(ConfigError::Parse(_))
        ));
    }

    #[test]
    fn resolves_relative_history_path_against_configuration_directory() {
        let source = VALID.replace(
            "history_enabled = false",
            "history_enabled = true\nhistory_path = \"history/events.jsonl\"",
        );
        let base = tempfile::tempdir().unwrap();
        fs::create_dir(base.path().join("history")).unwrap();
        let config = parse_config(&source, base.path(), ConfigOrigin::QuickProfile).unwrap();
        let expected = base.path().join("history/events.jsonl");
        assert_eq!(
            config.profile().logging.history_path.as_deref(),
            Some(expected.as_path())
        );
    }

    #[test]
    fn load_resolves_paths_from_file_directory_not_current_directory() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "agent-speak-config-test-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(directory.join("sounds")).unwrap();
        let media_path = directory.join("sounds/chime.wav");
        fs::write(&media_path, b"static validation does not decode").unwrap();
        let config_path = directory.join("profile.toml");
        let source = format!(
            "{VALID}\n[[audio_cues]]\nid = \"chime\"\nkind = \"audio_file\"\nsource = \"sounds/chime.wav\"\ndefault_gain = 0.4\n"
        );
        fs::write(&config_path, source).unwrap();

        let config = load_config(&config_path).unwrap();
        let expected_media_path = fs::canonicalize(&media_path).unwrap();
        assert_eq!(
            config.profile().audio_cues[0].source.as_deref(),
            Some(expected_media_path.as_path())
        );

        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn discovery_returns_none_when_every_candidate_is_missing() {
        let directory = tempfile::tempdir().unwrap();
        let candidates = [
            directory.path().join("system.toml"),
            directory.path().join("user.toml"),
            directory.path().join("project.toml"),
        ];
        assert!(load_discovered_config_from(&candidates).unwrap().is_none());
    }

    #[test]
    fn discovery_candidates_use_platform_and_working_directory_locations() {
        let working_directory = Path::new("configured-working-directory");
        let candidates = discovery_candidates(working_directory);
        assert_eq!(
            candidates.last(),
            Some(&working_directory.join(".agent-speak.toml"))
        );
        #[cfg(not(windows))]
        {
            assert_eq!(
                candidates.first(),
                Some(&PathBuf::from("/etc/agent-speak.toml"))
            );
            if let Some(home) = env::var_os("HOME").map(PathBuf::from)
                && home.is_absolute()
            {
                assert!(candidates.contains(&home.join(".agent-speak.toml")));
            }
        }
        #[cfg(windows)]
        if let Some(program_data) = env::var_os("ProgramData") {
            let program_data = PathBuf::from(program_data);
            if program_data.is_absolute() {
                assert_eq!(
                    candidates.first(),
                    Some(&program_data.join("Agent Speak").join("agent-speak.toml"))
                );
            }
        }
    }

    #[test]
    fn discovered_layers_merge_provider_options_and_project_permissions() {
        let directory = tempfile::tempdir().unwrap();
        let system = directory.path().join("system/agent-speak.toml");
        let user = directory.path().join("user/.agent-speak.toml");
        let project = directory.path().join("project/.agent-speak.toml");
        for path in [&system, &user, &project] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
        }
        fs::write(&system, VALID).unwrap();
        fs::write(
            &user,
            r#"
[tts]
backend = "utterpipe-pocket-tts"
provider_environment = ["POCKET_TOKEN"]

[tts.provider_options]
voice = "alba"
temperature = 0.1

[tts.provider_options.network]
timeout_seconds = 10
"#,
        )
        .unwrap();
        fs::write(
            &project,
            r#"
[permissions]
arbitrary_text = true

[playback]
allowed_concurrency = ["enqueue"]

[tts]
provider_environment = ["PROJECT_TOKEN"]

[tts.provider_options]
temperature = 0.2

[tts.provider_options.network]
retries = 2
"#,
        )
        .unwrap();

        let config = load_discovered_config_from(&[system.clone(), user.clone(), project.clone()])
            .unwrap()
            .unwrap();
        let profile = config.profile();
        assert!(profile.permissions.arbitrary_text);
        assert!(!profile.permissions.arbitrary_local_audio);
        assert_eq!(
            profile.playback.allowed_concurrency,
            [ConcurrencyMode::Enqueue]
        );
        let provider = profile.tts.utterpipe().unwrap();
        assert_eq!(provider.provider, "pocket-tts");
        assert_eq!(provider.provider_environment, ["PROJECT_TOKEN"]);
        assert_eq!(provider.provider_options["voice"].as_str(), Some("alba"));
        assert_eq!(
            provider.provider_options["temperature"].as_float(),
            Some(0.2)
        );
        assert_eq!(
            provider.provider_options["network"]["timeout_seconds"].as_integer(),
            Some(10)
        );
        assert_eq!(
            provider.provider_options["network"]["retries"].as_integer(),
            Some(2)
        );
        assert_eq!(
            config.origin(),
            &ConfigOrigin::Layered(vec![
                fs::canonicalize(system).unwrap(),
                fs::canonicalize(user).unwrap(),
                fs::canonicalize(project).unwrap(),
            ])
        );
    }

    #[test]
    fn partial_user_and_project_layers_extend_built_in_defaults() {
        let directory = tempfile::tempdir().unwrap();
        let user = directory.path().join("user.toml");
        let project = directory.path().join("project.toml");
        fs::write(
            &user,
            r#"
[tts]
backend = "utterpipe-pocket-tts"

[tts.provider_options]
voice = "alba"
"#,
        )
        .unwrap();
        fs::write(
            &project,
            r#"
[permissions]
arbitrary_text = false
arbitrary_local_audio = true
"#,
        )
        .unwrap();

        let config = load_discovered_config_from(&[user, project])
            .unwrap()
            .unwrap();
        let profile = config.profile();
        assert_eq!(profile.profile_name, "quickstart");
        assert_eq!(profile.playback.default_gain, 0.4);
        assert_eq!(profile.outputs.default_target, "system");
        assert!(!profile.permissions.arbitrary_text);
        assert!(profile.permissions.arbitrary_local_audio);
        let provider = profile.tts.utterpipe().unwrap();
        assert_eq!(provider.provider, "pocket-tts");
        assert_eq!(provider.provider_options["voice"].as_str(), Some("alba"));
    }

    #[test]
    fn backend_change_discards_incompatible_lower_layer_fields() {
        let directory = tempfile::tempdir().unwrap();
        let system = directory.path().join("system.toml");
        let user = directory.path().join("user.toml");
        let project = directory.path().join("project.toml");
        fs::write(&system, VALID).unwrap();
        fs::write(
            &user,
            r#"
[tts]
backend = "utterpipe-pocket-tts"
provider_environment = ["SECRET_TOKEN"]

[tts.provider_options]
api_key = "sentinel-secret"
"#,
        )
        .unwrap();
        fs::write(
            &project,
            r#"
[tts]
backend = "system"
voice_id = "project-voice"
"#,
        )
        .unwrap();

        let config = load_discovered_config_from(&[system, user, project])
            .unwrap()
            .unwrap();
        assert!(matches!(
            config.profile().tts.backend,
            super::super::TtsBackend::System(_)
        ));
        assert_eq!(
            config.profile().tts.system_voice_id(),
            Some("project-voice")
        );
    }

    #[test]
    fn discovered_relative_paths_follow_the_layer_that_declared_them() {
        let directory = tempfile::tempdir().unwrap();
        let system_directory = directory.path().join("system");
        let user_directory = directory.path().join("user");
        let project_directory = directory.path().join("project");
        for path in [&system_directory, &user_directory, &project_directory] {
            fs::create_dir_all(path).unwrap();
        }
        fs::create_dir(system_directory.join("history")).unwrap();
        fs::create_dir(project_directory.join("sounds")).unwrap();
        let sound = project_directory.join("sounds/attention.wav");
        fs::write(&sound, b"static validation does not decode").unwrap();
        let system = system_directory.join("agent-speak.toml");
        let user = user_directory.join(".agent-speak.toml");
        let project = project_directory.join(".agent-speak.toml");
        fs::write(
            &system,
            VALID.replace(
                "history_enabled = false",
                "history_enabled = true\nhistory_path = \"history/events.jsonl\"",
            ),
        )
        .unwrap();
        fs::write(&user, "[logging]\nlevel = \"info\"\n").unwrap();
        fs::write(
            &project,
            r#"
[[audio_cues]]
id = "attention"
kind = "audio_file"
source = "sounds/attention.wav"
default_gain = 0.4
"#,
        )
        .unwrap();

        let config = load_discovered_config_from(&[system, user, project])
            .unwrap()
            .unwrap();
        let expected_history = fs::canonicalize(&system_directory)
            .unwrap()
            .join("history/events.jsonl");
        assert_eq!(
            config.profile().logging.history_path.as_deref(),
            Some(expected_history.as_path())
        );
        assert_eq!(
            config.profile().audio_cues[0].source.as_deref(),
            Some(fs::canonicalize(sound).unwrap().as_path())
        );
    }

    #[test]
    fn malformed_discovered_layer_fails_with_its_path() {
        let directory = tempfile::tempdir().unwrap();
        let system = directory.path().join("system.toml");
        let project = directory.path().join("project.toml");
        fs::write(&system, VALID).unwrap();
        fs::write(&project, "[permissions\narbitrary_text = true").unwrap();

        let error = load_discovered_config_from(&[system, project.clone()]).unwrap_err();
        assert!(matches!(
            error,
            ConfigError::LayerParse { path, .. }
                if path == fs::canonicalize(project).unwrap()
        ));
    }

    #[test]
    fn duplicate_canonical_discovery_paths_are_loaded_once() {
        let directory = tempfile::tempdir().unwrap();
        let profile = directory.path().join(".agent-speak.toml");
        fs::write(&profile, VALID).unwrap();
        let config = load_discovered_config_from(&[profile.clone(), profile.clone()])
            .unwrap()
            .unwrap();
        assert_eq!(
            config.origin(),
            &ConfigOrigin::Layered(vec![fs::canonicalize(profile).unwrap()])
        );
    }
}
