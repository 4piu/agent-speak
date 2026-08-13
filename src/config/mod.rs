//! Strict, versioned startup configuration and sanitized effective policy.

mod load;
mod model;
mod validate;

pub use load::{
    BuiltInConfigOverrides, ConfigError, ConfigOrigin, ValidatedConfig, ValidationErrors,
    built_in_config, load_config, load_discovered_config, parse_config,
};
pub use model::{
    AudioCapabilities, AudioCueConfig, AudioCueKind, AudioCueSummary, ConcurrencyMode,
    DEFAULT_MAXIMUM_MIX_STREAMS, EffectiveCapabilities, EffectivePermissions, LogLevel,
    LoggingConfig, MAXIMUM_AUDIO_CUES, MAXIMUM_MIX_STREAMS, MAXIMUM_QUEUE_ITEMS,
    MAXIMUM_TEXT_CHARACTERS, OutputCapabilities, OutputCategory, OutputTargetConfig,
    OutputTargetKind, OutputTargetSummary, OutputsConfig, PermissionsConfig, PlaybackCapabilities,
    PlaybackConfig, ProfileConfig, SCHEMA_VERSION, SystemTtsConfig, TtsCapabilities, TtsConfig,
    TtsProvider, UtterPipeTtsConfig,
};
pub use validate::{ValidationIssue, resolve_and_validate};
