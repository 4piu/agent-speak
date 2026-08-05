use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

use thiserror::Error;

use super::{
    ConcurrencyMode, EffectiveCapabilities, LogLevel, LoggingConfig, PermissionsConfig,
    PlaybackConfig, ProfileConfig, SCHEMA_VERSION, TtsConfig, ValidationIssue,
    resolve_and_validate,
};

const QUICK_MAXIMUM_FILE_BYTES: u64 = 52_428_800;
const QUICK_MAXIMUM_AUDIO_SECONDS: u64 = 300;

#[derive(Clone, Debug, Default)]
pub struct QuickProfileOverrides {
    pub voice_id: Option<String>,
    pub minimum_gain: Option<f64>,
    pub maximum_gain: Option<f64>,
    pub default_gain: Option<f64>,
    pub maximum_text_characters: Option<usize>,
    pub maximum_plays_per_minute: Option<u32>,
    pub log_level: Option<LogLevel>,
}

#[derive(Clone, Debug)]
pub enum ConfigOrigin {
    QuickProfile,
    File(PathBuf),
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
            approved_directories: Vec::new(),
        },
        playback: PlaybackConfig {
            minimum_gain: overrides.minimum_gain.unwrap_or(0.0),
            maximum_gain: overrides.maximum_gain.unwrap_or(0.7),
            default_gain: overrides.default_gain.unwrap_or(0.4),
            default_concurrency: ConcurrencyMode::Enqueue,
            allowed_concurrency: vec![ConcurrencyMode::Enqueue, ConcurrencyMode::Interrupt],
            maximum_queue_items: 16,
            maximum_file_bytes: QUICK_MAXIMUM_FILE_BYTES,
            maximum_audio_seconds: QUICK_MAXIMUM_AUDIO_SECONDS,
            maximum_plays_per_minute: overrides.maximum_plays_per_minute.unwrap_or(10),
        },
        tts: TtsConfig {
            enabled: true,
            voice_id: overrides.voice_id.unwrap_or_default(),
            maximum_characters: overrides.maximum_text_characters.unwrap_or(300),
        },
        logging: LoggingConfig {
            level: overrides.log_level.unwrap_or(LogLevel::Warning),
            history_enabled: false,
            history_path: None,
            history_include_spoken_text: false,
        },
        presets: Vec::new(),
    };

    // Quick profile contains no paths, so its base is deliberately irrelevant.
    finish_validation(profile, Path::new("."), ConfigOrigin::QuickProfile)
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
approved_directories = []

[playback]
minimum_gain = 0.2
maximum_gain = 0.7
default_gain = 0.4
default_concurrency = "enqueue"
allowed_concurrency = ["enqueue", "interrupt"]
maximum_queue_items = 16
maximum_file_bytes = 52428800
maximum_audio_seconds = 300
maximum_plays_per_minute = 10

[tts]
enabled = true
voice_id = ""
maximum_characters = 300

[logging]
level = "warning"
history_enabled = false
history_include_spoken_text = false
"#;

    #[test]
    fn strict_parser_accepts_version_one_profile() {
        let config = parse_config(VALID, Path::new("."), ConfigOrigin::QuickProfile).unwrap();
        assert_eq!(config.profile().profile_name, "default");
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
            "default_concurrency = \"mix\"",
        );
        assert!(matches!(
            parse_config(&source, Path::new("."), ConfigOrigin::QuickProfile),
            Err(ConfigError::Parse(_))
        ));
    }

    #[test]
    fn quick_profile_matches_normative_defaults() {
        let config = quick_profile(QuickProfileOverrides::default()).unwrap();
        let profile = config.profile();
        assert_eq!(profile.schema_version, 1);
        assert_eq!(profile.profile_name, "quickstart");
        assert!(profile.permissions.arbitrary_text);
        assert!(!profile.permissions.arbitrary_local_audio);
        assert_eq!(profile.playback.minimum_gain, 0.0);
        assert_eq!(profile.playback.maximum_gain, 0.7);
        assert_eq!(profile.playback.default_gain, 0.4);
        assert_eq!(profile.playback.maximum_queue_items, 16);
        assert_eq!(profile.playback.maximum_plays_per_minute, 10);
        assert_eq!(profile.tts.maximum_characters, 300);
        assert_eq!(profile.logging.level, LogLevel::Warning);
        assert_eq!(
            config.capabilities().tools,
            ["get_audio_capabilities", "speak_text"]
        );

        assert_eq!(
            serde_json::to_value(config.capabilities()).unwrap(),
            serde_json::json!({
                "schema_version": 1,
                "profile_name": "quickstart",
                "tools": ["get_audio_capabilities", "speak_text"],
                "permissions": {
                    "arbitrary_text": true,
                    "arbitrary_local_audio": false
                },
                "presets_available": false,
                "audio": {
                    "formats": ["wav", "mp3", "flac", "ogg_vorbis"]
                },
                "playback": {
                    "minimum_gain": 0.0,
                    "maximum_gain": 0.7,
                    "default_gain": 0.4,
                    "default_concurrency": "enqueue",
                    "allowed_concurrency": ["enqueue", "interrupt"],
                    "maximum_queue_items": 16,
                    "maximum_file_bytes": 52428800,
                    "maximum_audio_seconds": 300,
                    "maximum_plays_per_minute": 10
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
        assert_eq!(config.capabilities().tools, ["get_audio_capabilities"]);

        let preset = format!(
            "{VALID}\n[[presets]]\nid = \"ready\"\nkind = \"text\"\ntext = \"Ready\"\ndefault_gain = 0.4\n"
        );
        let config = parse_config(&preset, Path::new("."), ConfigOrigin::QuickProfile).unwrap();
        assert!(config.capabilities().presets_available);
        assert_eq!(
            config.capabilities().tools,
            [
                "get_audio_capabilities",
                "list_audio_presets",
                "play_audio_preset"
            ]
        );
    }

    #[test]
    fn capabilities_and_preset_summaries_are_sanitized() {
        let profile_source = format!(
            "{VALID}\n[[presets]]\nid = \"say-secret\"\nkind = \"text\"\ntext = \"never expose this phrase\"\ndescription = \"\"\ndefault_gain = 0.4\n"
        );
        let config =
            parse_config(&profile_source, Path::new("."), ConfigOrigin::QuickProfile).unwrap();
        let json = serde_json::to_string(config.capabilities()).unwrap();
        for forbidden in [
            "approved_directories",
            "history_path",
            "voice_id",
            "never expose this phrase",
        ] {
            assert!(!json.contains(forbidden), "leaked {forbidden}");
        }

        let summaries = config.profile().preset_summaries();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].description, None);
        let summary_json = serde_json::to_string(&summaries).unwrap();
        assert!(!summary_json.contains("never expose this phrase"));
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
            "{VALID}\n[[presets]]\nid = \"chime\"\nkind = \"audio_file\"\nsource = \"sounds/chime.wav\"\ndefault_gain = 0.4\n"
        );
        fs::write(&config_path, source).unwrap();

        let config = load_config(&config_path).unwrap();
        let expected_media_path = fs::canonicalize(&media_path).unwrap();
        assert_eq!(
            config.profile().presets[0].source.as_deref(),
            Some(expected_media_path.as_path())
        );

        fs::remove_dir_all(&directory).unwrap();
    }
}
