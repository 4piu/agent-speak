//! Strict, versioned startup configuration and sanitized effective policy.

mod load;
mod model;
mod validate;

pub use load::{
    ConfigError, ConfigOrigin, QuickProfileOverrides, ValidatedConfig, ValidationErrors,
    load_config, parse_config, quick_profile,
};
pub use model::{
    AudioCapabilities, ConcurrencyMode, EffectiveCapabilities, EffectivePermissions, LogLevel,
    LoggingConfig, MAXIMUM_AUDIO_SECONDS, MAXIMUM_FILE_BYTES, MAXIMUM_PLAYS_PER_MINUTE,
    MAXIMUM_PRESETS, MAXIMUM_QUEUE_ITEMS, MAXIMUM_TEXT_CHARACTERS, PermissionsConfig,
    PlaybackCapabilities, PlaybackConfig, PresetConfig, PresetKind, PresetSummary, ProfileConfig,
    SCHEMA_VERSION, TtsCapabilities, TtsConfig,
};
pub use validate::{ValidationIssue, resolve_and_validate};
