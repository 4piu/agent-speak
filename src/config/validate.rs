use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use super::{
    MAXIMUM_PRESETS, MAXIMUM_QUEUE_ITEMS, MAXIMUM_TEXT_CHARACTERS, OutputTargetKind, PresetKind,
    ProfileConfig, SCHEMA_VERSION, TtsBackend,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationIssue {
    pub field: String,
    pub message: String,
}

impl ValidationIssue {
    fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            message: message.into(),
        }
    }
}

/// Resolve configuration paths and validate all invariants that do not require
/// an initialized decoder or platform audio backend.
pub fn resolve_and_validate(
    mut profile: ProfileConfig,
    configuration_directory: &Path,
) -> Result<ProfileConfig, Vec<ValidationIssue>> {
    let mut issues = Vec::new();

    validate_general(&profile, &mut issues);
    validate_playback(&profile, &mut issues);
    validate_tts(&profile, &mut issues);
    validate_outputs(&profile, &mut issues);
    resolve_logging(&mut profile, configuration_directory, &mut issues);
    resolve_and_validate_presets(&mut profile, configuration_directory, &mut issues);

    if issues.is_empty() {
        Ok(profile)
    } else {
        Err(issues)
    }
}

fn validate_outputs(profile: &ProfileConfig, issues: &mut Vec<ValidationIssue>) {
    if profile.outputs.targets.is_empty() {
        issues.push(ValidationIssue::new(
            "outputs.targets",
            "must contain at least one output target",
        ));
    }

    let mut identifiers = HashSet::new();
    for (index, target) in profile.outputs.targets.iter().enumerate() {
        let base = format!("outputs.targets[{index}]");
        if !valid_preset_id(&target.id) {
            issues.push(ValidationIssue::new(
                format!("{base}.id"),
                "must match [a-zA-Z0-9][a-zA-Z0-9._-]{0,63}",
            ));
        }
        if !identifiers.insert(target.id.as_str()) {
            issues.push(ValidationIssue::new(
                format!("{base}.id"),
                "must be unique within the output target allowlist",
            ));
        }
        if target.description.chars().count() > 1_000 {
            issues.push(ValidationIssue::new(
                format!("{base}.description"),
                "must contain no more than 1000 Unicode characters",
            ));
        }

        let mut categories = HashSet::new();
        for category in &target.allow {
            if !categories.insert(*category) {
                issues.push(ValidationIssue::new(
                    format!("{base}.allow"),
                    "must not contain duplicate categories",
                ));
                break;
            }
        }

        match target.kind {
            OutputTargetKind::SystemDefault if target.device_id.is_some() => {
                issues.push(ValidationIssue::new(
                    format!("{base}.device_id"),
                    "is not allowed for a system_default target",
                ));
            }
            OutputTargetKind::Device
                if target
                    .device_id
                    .as_deref()
                    .is_none_or(|device_id| device_id.trim().is_empty()) =>
            {
                issues.push(ValidationIssue::new(
                    format!("{base}.device_id"),
                    "is required and must not be empty for a device target",
                ));
            }
            _ => {}
        }
    }

    if !identifiers.contains(profile.outputs.default_target.as_str()) {
        issues.push(ValidationIssue::new(
            "outputs.default_target",
            "must identify one of outputs.targets",
        ));
    }
}

fn validate_general(profile: &ProfileConfig, issues: &mut Vec<ValidationIssue>) {
    if !matches!(profile.schema_version, 1 | SCHEMA_VERSION) {
        issues.push(ValidationIssue::new(
            "schema_version",
            format!("must equal 1 or {SCHEMA_VERSION}"),
        ));
    }

    let profile_name_length = profile.profile_name.chars().count();
    if profile_name_length == 0 || profile_name_length > 80 {
        issues.push(ValidationIssue::new(
            "profile_name",
            "must contain between 1 and 80 Unicode characters",
        ));
    }

    if profile.presets.len() > MAXIMUM_PRESETS {
        issues.push(ValidationIssue::new(
            "presets",
            format!("must contain no more than {MAXIMUM_PRESETS} entries"),
        ));
    }
}

fn validate_tts(profile: &ProfileConfig, issues: &mut Vec<ValidationIssue>) {
    let tts = &profile.tts;
    if profile.schema_version == 1 {
        if tts.backend_explicit {
            issues.push(ValidationIssue::new(
                "tts.backend",
                "is available only in a schema-2 profile",
            ));
        }
        if matches!(tts.backend, TtsBackend::Utterpipe(_)) {
            issues.push(ValidationIssue::new(
                "tts",
                "schema 1 supports only the built-in system TTS fields",
            ));
        }
        return;
    }
    if !tts.backend_explicit {
        issues.push(ValidationIssue::new(
            "tts.backend",
            "is required in a schema-2 profile",
        ));
    }

    match &tts.backend {
        TtsBackend::System(_) => {}
        TtsBackend::Utterpipe(provider) => {
            if !valid_provider_slug(&provider.provider) {
                issues.push(ValidationIssue::new(
                    "tts.provider",
                    "is required and must match [a-z0-9][a-z0-9-]{0,62}[a-z0-9] (or one lowercase letter/digit)",
                ));
            }
            validate_provider_id("tts.model_id", Some(&provider.model_id), issues);
            validate_provider_id("tts.voice_id", Some(&provider.voice_id), issues);

            if provider.provider_environment.len() > 32 {
                issues.push(ValidationIssue::new(
                    "tts.provider_environment",
                    "must contain no more than 32 names",
                ));
            }
            let mut names = HashSet::new();
            for name in &provider.provider_environment {
                if !valid_environment_name(name) {
                    issues.push(ValidationIssue::new(
                        "tts.provider_environment",
                        "names must match [A-Za-z_][A-Za-z0-9_]{0,127}",
                    ));
                    break;
                }
                if !names.insert(name) {
                    issues.push(ValidationIssue::new(
                        "tts.provider_environment",
                        "must not contain duplicate names",
                    ));
                    break;
                }
            }
            validate_provider_options(&provider.provider_options, "tts.provider_options", issues);
        }
    }
}

fn valid_provider_slug(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 64
        || !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit()
    {
        return false;
    }
    if bytes.len() > 1
        && (!bytes[bytes.len() - 1].is_ascii_lowercase()
            && !bytes[bytes.len() - 1].is_ascii_digit())
    {
        return false;
    }
    bytes
        .iter()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

fn validate_provider_id(field: &str, value: Option<&str>, issues: &mut Vec<ValidationIssue>) {
    if value.is_none_or(|value| {
        value.is_empty()
            || value.chars().count() > 256
            || value
                .chars()
                .any(|character| matches!(character, '\r' | '\n' | '\0'))
    }) {
        issues.push(ValidationIssue::new(
            field,
            "is required, must contain 1 to 256 Unicode characters, and must not contain CR, LF, or NUL",
        ));
    }
}

fn valid_environment_name(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}

fn validate_provider_options(table: &toml::Table, field: &str, issues: &mut Vec<ValidationIssue>) {
    fn valid(value: &toml::Value) -> bool {
        match value {
            toml::Value::String(_) | toml::Value::Boolean(_) => true,
            toml::Value::Integer(value) => value.unsigned_abs() <= 9_007_199_254_740_991,
            toml::Value::Float(value) => value.is_finite(),
            toml::Value::Array(values) => values.iter().all(valid),
            toml::Value::Table(table) => table.values().all(valid),
            toml::Value::Datetime(_) => false,
        }
    }
    if !table.values().all(valid) {
        issues.push(ValidationIssue::new(
            field,
            "must contain only JSON-compatible finite values and exactly representable integers",
        ));
    }
}

fn validate_playback(profile: &ProfileConfig, issues: &mut Vec<ValidationIssue>) {
    let playback = &profile.playback;
    validate_gain("playback.minimum_gain", playback.minimum_gain, issues);
    validate_gain("playback.maximum_gain", playback.maximum_gain, issues);
    validate_gain("playback.default_gain", playback.default_gain, issues);

    if playback.minimum_gain.is_finite()
        && playback.maximum_gain.is_finite()
        && playback.minimum_gain > playback.maximum_gain
    {
        issues.push(ValidationIssue::new(
            "playback.minimum_gain",
            "must be less than or equal to playback.maximum_gain",
        ));
    }
    if playback.default_gain.is_finite()
        && playback.minimum_gain.is_finite()
        && playback.maximum_gain.is_finite()
        && (playback.default_gain < playback.minimum_gain
            || playback.default_gain > playback.maximum_gain)
    {
        issues.push(ValidationIssue::new(
            "playback.default_gain",
            "must be between playback.minimum_gain and playback.maximum_gain",
        ));
    }

    let mut modes = HashSet::new();
    for mode in &playback.allowed_concurrency {
        if !modes.insert(*mode) {
            issues.push(ValidationIssue::new(
                "playback.allowed_concurrency",
                "must not contain duplicate modes",
            ));
            break;
        }
    }
    if !playback
        .allowed_concurrency
        .contains(&playback.default_concurrency)
    {
        issues.push(ValidationIssue::new(
            "playback.default_concurrency",
            "must be included in playback.allowed_concurrency",
        ));
    }

    validate_positive_limit(
        "playback.maximum_queue_items",
        playback.maximum_queue_items,
        MAXIMUM_QUEUE_ITEMS,
        issues,
    );
    validate_positive_limit(
        "tts.maximum_characters",
        profile.tts.maximum_characters,
        MAXIMUM_TEXT_CHARACTERS,
        issues,
    );
}

fn validate_gain(field: &str, gain: f64, issues: &mut Vec<ValidationIssue>) {
    if !gain.is_finite() || !(0.0..=1.0).contains(&gain) {
        issues.push(ValidationIssue::new(
            field,
            "must be a finite number between 0.0 and 1.0 inclusive",
        ));
    }
}

fn validate_positive_limit<T>(field: &str, value: T, ceiling: T, issues: &mut Vec<ValidationIssue>)
where
    T: Copy + PartialEq + PartialOrd + Default + std::fmt::Display,
{
    if value == T::default() || value > ceiling {
        issues.push(ValidationIssue::new(
            field,
            format!("must be positive and no greater than {ceiling}"),
        ));
    }
}

fn resolve_logging(
    profile: &mut ProfileConfig,
    configuration_directory: &Path,
    issues: &mut Vec<ValidationIssue>,
) {
    if profile.logging.history_enabled && profile.logging.history_path.is_none() {
        issues.push(ValidationIssue::new(
            "logging.history_path",
            "is required when logging.history_enabled is true",
        ));
    }
    if let Some(path) = &mut profile.logging.history_path {
        if path.as_os_str().is_empty() {
            issues.push(ValidationIssue::new(
                "logging.history_path",
                "must identify a file when provided",
            ));
            return;
        }
        *path = resolve_path(configuration_directory, path);
        if is_nonlocal_windows_path(path) {
            issues.push(ValidationIssue::new(
                "logging.history_path",
                "network and device paths are not supported",
            ));
        }
        if path.is_dir() {
            issues.push(ValidationIssue::new(
                "logging.history_path",
                "must identify a file, not a directory",
            ));
        }
        if !path.parent().is_some_and(Path::is_dir) {
            issues.push(ValidationIssue::new(
                "logging.history_path",
                "parent directory does not exist",
            ));
        }
    }
}

fn resolve_and_validate_presets(
    profile: &mut ProfileConfig,
    configuration_directory: &Path,
    issues: &mut Vec<ValidationIssue>,
) {
    let mut identifiers = HashSet::new();

    for (index, preset) in profile.presets.iter_mut().enumerate() {
        let base = format!("presets[{index}]");
        if !valid_preset_id(&preset.id) {
            issues.push(ValidationIssue::new(
                format!("{base}.id"),
                "must match [a-zA-Z0-9][a-zA-Z0-9._-]{0,63}",
            ));
        }
        if !identifiers.insert(preset.id.clone()) {
            issues.push(ValidationIssue::new(
                format!("{base}.id"),
                "must be unique within the profile",
            ));
        }
        if preset.description.chars().count() > 1_000 {
            issues.push(ValidationIssue::new(
                format!("{base}.description"),
                "must contain no more than 1000 Unicode characters",
            ));
        }
        if !preset.default_gain.is_finite()
            || preset.default_gain < profile.playback.minimum_gain
            || preset.default_gain > profile.playback.maximum_gain
        {
            issues.push(ValidationIssue::new(
                format!("{base}.default_gain"),
                "must be finite and within the configured playback gain range",
            ));
        }

        match preset.kind {
            PresetKind::AudioFile => {
                if preset.text.is_some() {
                    issues.push(ValidationIssue::new(
                        format!("{base}.text"),
                        "is not allowed for an audio_file preset",
                    ));
                }
                let Some(source) = &mut preset.source else {
                    issues.push(ValidationIssue::new(
                        format!("{base}.source"),
                        "is required for an audio_file preset",
                    ));
                    continue;
                };
                let candidate = resolve_path(configuration_directory, source);
                match fs::canonicalize(candidate) {
                    Ok(canonical) if is_nonlocal_windows_path(&canonical) => {
                        issues.push(ValidationIssue::new(
                            format!("{base}.source"),
                            "network and device paths are not supported",
                        ));
                    }
                    Ok(canonical) => match fs::metadata(&canonical) {
                        Ok(metadata) if !metadata.is_file() => issues.push(ValidationIssue::new(
                            format!("{base}.source"),
                            "must identify a regular file",
                        )),
                        Ok(_) => *source = canonical,
                        Err(_) => issues.push(ValidationIssue::new(
                            format!("{base}.source"),
                            "could not be inspected",
                        )),
                    },
                    Err(_) => issues.push(ValidationIssue::new(
                        format!("{base}.source"),
                        "file does not exist",
                    )),
                }
            }
            PresetKind::Text => {
                if preset.source.is_some() {
                    issues.push(ValidationIssue::new(
                        format!("{base}.source"),
                        "is not allowed for a text preset",
                    ));
                }
                let Some(text) = &preset.text else {
                    issues.push(ValidationIssue::new(
                        format!("{base}.text"),
                        "is required for a text preset",
                    ));
                    continue;
                };
                if text.trim().is_empty() {
                    issues.push(ValidationIssue::new(
                        format!("{base}.text"),
                        "must not be empty or whitespace-only",
                    ));
                }
                if text.chars().count() > profile.tts.maximum_characters {
                    issues.push(ValidationIssue::new(
                        format!("{base}.text"),
                        "exceeds tts.maximum_characters",
                    ));
                }
                if !profile.tts.enabled {
                    issues.push(ValidationIssue::new(
                        format!("{base}.kind"),
                        "requires tts.enabled to be true",
                    ));
                }
            }
        }
    }
}

fn resolve_path(configuration_directory: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        configuration_directory.join(path)
    }
}

fn valid_preset_id(identifier: &str) -> bool {
    let bytes = identifier.as_bytes();
    (1..=64).contains(&bytes.len())
        && bytes[0].is_ascii_alphanumeric()
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'.' | b'_' | b'-'))
}

#[cfg(windows)]
fn is_nonlocal_windows_path(path: &Path) -> bool {
    use std::path::{Component, Prefix};

    matches!(
        path.components().next(),
        Some(Component::Prefix(prefix))
            if !matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
    )
}

#[cfg(not(windows))]
fn is_nonlocal_windows_path(_path: &Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        ConcurrencyMode, LogLevel, LoggingConfig, OutputCategory, OutputTargetConfig,
        OutputsConfig, PermissionsConfig, PlaybackConfig, PresetConfig, TtsConfig,
    };

    fn valid_profile() -> ProfileConfig {
        ProfileConfig {
            schema_version: SCHEMA_VERSION,
            profile_name: "test".into(),
            permissions: PermissionsConfig {
                arbitrary_text: false,
                arbitrary_local_audio: false,
            },
            playback: PlaybackConfig {
                minimum_gain: 0.0,
                maximum_gain: 1.0,
                default_gain: 0.5,
                default_concurrency: ConcurrencyMode::Enqueue,
                allowed_concurrency: vec![ConcurrencyMode::Enqueue, ConcurrencyMode::Interrupt],
                maximum_queue_items: 16,
                maximum_audio_seconds: 0,
            },
            outputs: OutputsConfig::default(),
            tts: TtsConfig {
                enabled: true,
                backend: TtsBackend::System(crate::config::SystemTtsConfig::default()),
                maximum_characters: 300,
                backend_explicit: true,
            },
            logging: LoggingConfig {
                level: LogLevel::Warning,
                history_enabled: false,
                history_path: None,
                history_include_spoken_text: false,
            },
            presets: vec![],
        }
    }

    fn issue_fields(profile: ProfileConfig) -> Vec<String> {
        resolve_and_validate(profile, Path::new("."))
            .unwrap_err()
            .into_iter()
            .map(|issue| issue.field)
            .collect()
    }

    #[test]
    fn accepts_valid_profile() {
        assert!(resolve_and_validate(valid_profile(), Path::new(".")).is_ok());
    }

    #[test]
    fn validates_schema_and_profile_name() {
        let mut profile = valid_profile();
        profile.schema_version = 3;
        profile.profile_name = String::new();
        let fields = issue_fields(profile);
        assert!(fields.contains(&"schema_version".to_owned()));
        assert!(fields.contains(&"profile_name".to_owned()));

        let mut profile = valid_profile();
        profile.profile_name = "🦀".repeat(81);
        assert!(issue_fields(profile).contains(&"profile_name".to_owned()));
    }

    #[test]
    fn validates_gain_values_and_ordering() {
        for bad in [f64::NAN, f64::INFINITY, -0.1, 1.1] {
            let mut profile = valid_profile();
            profile.playback.default_gain = bad;
            assert!(issue_fields(profile).contains(&"playback.default_gain".to_owned()));
        }

        let mut profile = valid_profile();
        profile.playback.minimum_gain = 0.7;
        profile.playback.maximum_gain = 0.6;
        let fields = issue_fields(profile);
        assert!(fields.contains(&"playback.minimum_gain".to_owned()));
    }

    #[test]
    fn validates_configurable_positive_limits_and_ceilings() {
        let mut profile = valid_profile();
        profile.playback.maximum_queue_items = 0;
        profile.tts.maximum_characters = 0;
        let fields = issue_fields(profile);
        for expected in ["playback.maximum_queue_items", "tts.maximum_characters"] {
            assert!(fields.contains(&expected.to_owned()), "missing {expected}");
        }
    }

    #[test]
    fn validates_concurrency_allowlist() {
        let mut duplicate = valid_profile();
        duplicate.playback.allowed_concurrency =
            vec![ConcurrencyMode::Enqueue, ConcurrencyMode::Enqueue];
        assert!(issue_fields(duplicate).contains(&"playback.allowed_concurrency".to_owned()));

        let mut missing_default = valid_profile();
        missing_default.playback.allowed_concurrency = vec![ConcurrencyMode::Interrupt];
        assert!(issue_fields(missing_default).contains(&"playback.default_concurrency".to_owned()));
    }

    #[test]
    fn accepts_an_explicit_device_output_target() {
        let mut profile = valid_profile();
        profile.outputs = OutputsConfig {
            default_target: "private-headset".into(),
            targets: vec![OutputTargetConfig {
                id: "private-headset".into(),
                description: "Private headset".into(),
                kind: OutputTargetKind::Device,
                device_id: Some("stable-private-endpoint-id".into()),
                allow: vec![OutputCategory::Audio, OutputCategory::Speech],
            }],
        };

        assert!(resolve_and_validate(profile, Path::new(".")).is_ok());
    }

    #[test]
    fn output_targets_must_be_nonempty_and_include_the_default() {
        let mut profile = valid_profile();
        profile.outputs.targets.clear();
        let fields = issue_fields(profile);
        assert!(fields.contains(&"outputs.targets".to_owned()));
        assert!(fields.contains(&"outputs.default_target".to_owned()));

        let mut missing_default = valid_profile();
        missing_default.outputs.default_target = "not-allowed".into();
        assert!(issue_fields(missing_default).contains(&"outputs.default_target".to_owned()));
    }

    #[test]
    fn validates_output_target_ids_descriptions_and_allow_categories() {
        let mut profile = valid_profile();
        profile.outputs.default_target = "bad id".into();
        profile.outputs.targets = vec![
            OutputTargetConfig {
                id: "bad id".into(),
                description: "🎧".repeat(1_001),
                kind: OutputTargetKind::SystemDefault,
                device_id: None,
                allow: vec![OutputCategory::Audio, OutputCategory::Audio],
            },
            OutputTargetConfig {
                id: "bad id".into(),
                description: String::new(),
                kind: OutputTargetKind::SystemDefault,
                device_id: None,
                allow: vec![],
            },
        ];

        let fields = issue_fields(profile);
        for expected in [
            "outputs.targets[0].id",
            "outputs.targets[0].description",
            "outputs.targets[0].allow",
            "outputs.targets[1].id",
        ] {
            assert!(fields.contains(&expected.to_owned()), "missing {expected}");
        }
    }

    #[test]
    fn output_target_id_uses_the_same_boundaries_as_preset_ids() {
        let valid_id = format!("a{}", "_".repeat(63));
        let invalid_id = format!("a{}", "_".repeat(64));
        let mut profile = valid_profile();
        profile.outputs.default_target = valid_id.clone();
        profile.outputs.targets = vec![
            OutputTargetConfig {
                id: valid_id,
                description: String::new(),
                kind: OutputTargetKind::SystemDefault,
                device_id: None,
                allow: vec![OutputCategory::Audio],
            },
            OutputTargetConfig {
                id: invalid_id,
                description: String::new(),
                kind: OutputTargetKind::SystemDefault,
                device_id: None,
                allow: vec![OutputCategory::Speech],
            },
        ];

        let fields = issue_fields(profile);
        assert!(!fields.contains(&"outputs.targets[0].id".to_owned()));
        assert!(fields.contains(&"outputs.targets[1].id".to_owned()));
    }

    #[test]
    fn validates_output_kind_and_device_id_consistency() {
        let mut profile = valid_profile();
        profile.outputs.targets = vec![
            OutputTargetConfig {
                id: "system".into(),
                description: String::new(),
                kind: OutputTargetKind::SystemDefault,
                device_id: Some("must-not-be-present".into()),
                allow: vec![OutputCategory::Audio],
            },
            OutputTargetConfig {
                id: "missing-device-id".into(),
                description: String::new(),
                kind: OutputTargetKind::Device,
                device_id: None,
                allow: vec![OutputCategory::Audio],
            },
            OutputTargetConfig {
                id: "blank-device-id".into(),
                description: String::new(),
                kind: OutputTargetKind::Device,
                device_id: Some(" \t".into()),
                allow: vec![OutputCategory::Speech],
            },
        ];

        let fields = issue_fields(profile);
        for expected in [
            "outputs.targets[0].device_id",
            "outputs.targets[1].device_id",
            "outputs.targets[2].device_id",
        ] {
            assert!(fields.contains(&expected.to_owned()), "missing {expected}");
        }
    }

    #[test]
    fn validates_preset_ids_duplicates_and_descriptions() {
        let mut profile = valid_profile();
        profile.presets = vec![
            PresetConfig {
                id: "bad id".into(),
                kind: PresetKind::Text,
                source: None,
                text: Some("one".into()),
                description: "x".repeat(1001),
                default_gain: 0.5,
            },
            PresetConfig {
                id: "bad id".into(),
                kind: PresetKind::Text,
                source: None,
                text: Some("two".into()),
                description: String::new(),
                default_gain: 0.5,
            },
        ];
        let fields = issue_fields(profile);
        assert!(fields.contains(&"presets[0].id".to_owned()));
        assert!(fields.contains(&"presets[0].description".to_owned()));
        assert!(fields.contains(&"presets[1].id".to_owned()));
    }

    #[test]
    fn validates_preset_count_id_boundaries_and_gain() {
        let text_preset = |id: String| PresetConfig {
            id,
            kind: PresetKind::Text,
            source: None,
            text: Some("sound".into()),
            description: String::new(),
            default_gain: 0.5,
        };

        let mut too_many = valid_profile();
        too_many.presets = (0..=MAXIMUM_PRESETS)
            .map(|index| text_preset(format!("preset-{index}")))
            .collect();
        assert!(issue_fields(too_many).contains(&"presets".to_owned()));

        let mut boundaries = valid_profile();
        boundaries.presets = vec![
            text_preset(format!("a{}", "_".repeat(63))),
            text_preset(format!("a{}", "_".repeat(64))),
        ];
        boundaries.presets[0].default_gain = 1.1;
        let fields = issue_fields(boundaries);
        assert!(fields.contains(&"presets[0].default_gain".to_owned()));
        assert!(!fields.contains(&"presets[0].id".to_owned()));
        assert!(fields.contains(&"presets[1].id".to_owned()));
    }

    #[test]
    fn validates_typed_preset_fields_and_text_policy() {
        let mut audio = valid_profile();
        audio.presets.push(PresetConfig {
            id: "audio".into(),
            kind: PresetKind::AudioFile,
            source: None,
            text: Some("forbidden".into()),
            description: String::new(),
            default_gain: 0.5,
        });
        let fields = issue_fields(audio);
        assert!(fields.contains(&"presets[0].source".to_owned()));
        assert!(fields.contains(&"presets[0].text".to_owned()));

        let mut text = valid_profile();
        text.tts.enabled = false;
        text.tts.maximum_characters = 3;
        text.presets.push(PresetConfig {
            id: "text".into(),
            kind: PresetKind::Text,
            source: Some(PathBuf::from("forbidden")),
            text: Some("long".into()),
            description: String::new(),
            default_gain: 0.5,
        });
        let fields = issue_fields(text);
        assert!(fields.contains(&"presets[0].source".to_owned()));
        assert!(fields.contains(&"presets[0].text".to_owned()));
        assert!(fields.contains(&"presets[0].kind".to_owned()));
    }

    #[test]
    fn enabled_history_requires_a_destination() {
        let mut profile = valid_profile();
        profile.logging.history_enabled = true;
        assert!(issue_fields(profile).contains(&"logging.history_path".to_owned()));
    }

    #[test]
    fn history_destination_must_not_be_a_directory() {
        let mut profile = valid_profile();
        profile.logging.history_enabled = true;
        profile.logging.history_path = Some(std::env::temp_dir());
        assert!(issue_fields(profile).contains(&"logging.history_path".to_owned()));
    }

    #[test]
    fn history_destination_parent_must_exist() {
        let mut profile = valid_profile();
        profile.logging.history_enabled = true;
        profile.logging.history_path = Some(
            std::env::temp_dir()
                .join("agent-speak-missing-history-parent")
                .join("history.jsonl"),
        );
        assert!(issue_fields(profile).contains(&"logging.history_path".to_owned()));
    }

    #[test]
    fn validates_utterpipe_identity_environment_and_json_options() {
        let mut profile = valid_profile();
        profile.tts.backend = TtsBackend::Utterpipe(crate::config::UtterPipeTtsConfig {
            provider: "Bad-Provider".into(),
            model_id: "bad\nmodel".into(),
            voice_id: String::new(),
            provider_environment: vec!["TOKEN".into(), "TOKEN".into()],
            provider_options: toml::Table::from_iter([(
                "when".into(),
                toml::Value::Datetime("1979-05-27T07:32:00Z".parse().unwrap()),
            )]),
        });
        let fields = issue_fields(profile);
        for expected in [
            "tts.provider",
            "tts.model_id",
            "tts.voice_id",
            "tts.provider_environment",
            "tts.provider_options",
        ] {
            assert!(fields.contains(&expected.to_owned()), "missing {expected}");
        }
    }

    #[test]
    fn schema_one_cannot_select_an_external_backend() {
        let mut profile = valid_profile();
        profile.schema_version = 1;
        profile.tts.backend = TtsBackend::Utterpipe(crate::config::UtterPipeTtsConfig {
            provider: "pocket-tts".into(),
            model_id: "english".into(),
            voice_id: "voice".into(),
            provider_environment: Vec::new(),
            provider_options: toml::Table::new(),
        });
        assert!(issue_fields(profile).contains(&"tts".to_owned()));
    }

    #[test]
    fn schema_one_rejects_the_schema_two_backend_tag() {
        let mut profile = valid_profile();
        profile.schema_version = 1;
        profile.tts.backend_explicit = true;
        assert!(issue_fields(profile).contains(&"tts.backend".to_owned()));
    }
}
