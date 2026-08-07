use std::{
    fmt, fs, io,
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
        },
        playback: PlaybackConfig {
            minimum_gain: overrides.minimum_gain.unwrap_or(0.0),
            maximum_gain: overrides.maximum_gain.unwrap_or(0.7),
            default_gain: overrides.default_gain.unwrap_or(0.4),
            default_concurrency: ConcurrencyMode::Enqueue,
            allowed_concurrency: vec![ConcurrencyMode::Enqueue, ConcurrencyMode::Interrupt],
            maximum_queue_items: 16,
            maximum_audio_seconds: 0,
        },
        outputs: OutputsConfig::default(),
        tts: TtsConfig {
            enabled: true,
            backend: quick_tts_backend(overrides.voice_id),
            maximum_characters: overrides.maximum_text_characters.unwrap_or(300),
            backend_explicit: true,
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

fn quick_tts_backend(voice_id: Option<String>) -> super::TtsBackend {
    #[cfg(target_os = "linux")]
    {
        super::TtsBackend::Utterpipe(super::UtterPipeTtsConfig {
            provider: "espeak-ng".to_owned(),
            model_id: "espeak-ng".to_owned(),
            voice_id: voice_id.unwrap_or_else(|| "default".to_owned()),
            provider_environment: Vec::new(),
            provider_options: toml::Table::new(),
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
    fn strict_parser_accepts_version_one_profile() {
        let config = parse_config(VALID, Path::new("."), ConfigOrigin::QuickProfile).unwrap();
        assert_eq!(config.profile().profile_name, "default");
    }

    #[test]
    fn schema_two_uses_a_strict_tagged_utterpipe_backend() {
        let source = VALID
            .replace("schema_version = 1", "schema_version = 2")
            .replace(
                "[tts]\nenabled = true\nvoice_id = \"\"\nmaximum_characters = 300",
                r#"[tts]
enabled = true
backend = "utterpipe"
provider = "pocket-tts"
model_id = "english"
voice_id = "my-voice"
maximum_characters = 300
provider_environment = ["POCKET_TOKEN"]

[tts.provider_options]
speed = 1.1
sample_rate_hz = 24000"#,
            );
        let config = parse_config(&source, Path::new("."), ConfigOrigin::QuickProfile).unwrap();
        let provider = config.profile().tts.utterpipe().unwrap();
        assert_eq!(provider.provider, "pocket-tts");
        assert_eq!(provider.model_id, "english");
        assert_eq!(provider.voice_id, "my-voice");
        assert_eq!(provider.provider_environment, ["POCKET_TOKEN"]);
        assert_eq!(
            provider.provider_options["sample_rate_hz"].as_integer(),
            Some(24000)
        );
    }

    #[test]
    fn tagged_system_backend_rejects_cross_backend_fields_during_parse() {
        let source = VALID
            .replace("schema_version = 1", "schema_version = 2")
            .replace(
                "voice_id = \"\"",
                "backend = \"system\"\nvoice_id = \"\"\nprovider = \"pocket-tts\"",
            );
        assert!(matches!(
            parse_config(&source, Path::new("."), ConfigOrigin::QuickProfile),
            Err(ConfigError::Parse(_))
        ));
    }

    #[test]
    fn schema_two_requires_an_explicit_backend_tag() {
        let source = VALID.replace("schema_version = 1", "schema_version = 2");
        let Err(ConfigError::Validation(errors)) =
            parse_config(&source, Path::new("."), ConfigOrigin::QuickProfile)
        else {
            panic!("schema 2 without tts.backend was accepted");
        };
        assert!(errors.0.iter().any(|issue| issue.field == "tts.backend"));
    }

    #[test]
    fn omitted_audio_duration_limit_defaults_to_unlimited() {
        let source = VALID.replace("maximum_audio_seconds = 0\n", "");
        let config = parse_config(&source, Path::new("."), ConfigOrigin::QuickProfile).unwrap();

        assert_eq!(config.profile().playback.maximum_audio_seconds, 0);
        assert_eq!(config.capabilities().playback.maximum_audio_seconds, 0);
    }

    #[test]
    fn file_profile_requires_outputs_without_a_compatibility_fallback() {
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
    fn removed_directory_allowlist_is_rejected_instead_of_silently_ignored() {
        let source = VALID.replace(
            "arbitrary_local_audio = false",
            "arbitrary_local_audio = false\napproved_directories = [\".\"]",
        );
        assert!(matches!(
            parse_config(&source, Path::new("."), ConfigOrigin::QuickProfile),
            Err(ConfigError::Parse(_))
        ));
    }

    #[test]
    fn removed_playback_limit_fields_are_rejected_without_fallbacks() {
        for field in ["maximum_file_bytes = 1", "maximum_plays_per_minute = 1"] {
            let source = VALID.replace(
                "maximum_queue_items = 16",
                &format!("maximum_queue_items = 16\n{field}"),
            );
            assert!(matches!(
                parse_config(&source, Path::new("."), ConfigOrigin::QuickProfile),
                Err(ConfigError::Parse(_))
            ));
        }
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
        assert_eq!(profile.schema_version, 2);
        assert_eq!(profile.profile_name, "quickstart");
        assert!(profile.permissions.arbitrary_text);
        assert!(!profile.permissions.arbitrary_local_audio);
        assert_eq!(profile.playback.minimum_gain, 0.0);
        assert_eq!(profile.playback.maximum_gain, 0.7);
        assert_eq!(profile.playback.default_gain, 0.4);
        assert_eq!(profile.playback.maximum_queue_items, 16);
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
                    && provider.model_id == "espeak-ng"
                    && provider.voice_id == "default"
        ));
        #[cfg(not(target_os = "linux"))]
        assert!(matches!(
            profile.tts.backend,
            super::super::TtsBackend::System(_)
        ));
        assert_eq!(profile.logging.level, LogLevel::Warning);
        assert_eq!(
            config.capabilities().tools,
            ["get_audio_capabilities", "speak_text"]
        );

        assert_eq!(
            serde_json::to_value(config.capabilities()).unwrap(),
            serde_json::json!({
                "schema_version": 2,
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
                    "maximum_audio_seconds": 0
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
            "{source}\n[[presets]]\nid = \"say-secret\"\nkind = \"text\"\ntext = \"never expose this phrase\"\ndescription = \"\"\ndefault_gain = 0.4\n"
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

        let summaries = config.profile().preset_summaries();
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
