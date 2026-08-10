//! Strict, versioned startup configuration and sanitized effective policy.

mod load;
mod model;
mod validate;

pub use load::{
    ConfigError, ConfigOrigin, QuickProfileOverrides, ValidatedConfig, ValidationErrors,
    load_config, load_discovered_config, parse_config, quick_profile,
};
pub use model::{
    AudioCapabilities, AudioCueConfig, AudioCueKind, AudioCueSummary, ConcurrencyMode,
    DEFAULT_MAXIMUM_MIX_STREAMS, EffectiveCapabilities, EffectivePermissions, LogLevel,
    LoggingConfig, MAXIMUM_AUDIO_CUES, MAXIMUM_MIX_STREAMS, MAXIMUM_QUEUE_ITEMS,
    MAXIMUM_TEXT_CHARACTERS, OutputCapabilities, OutputCategory, OutputTargetConfig,
    OutputTargetKind, OutputTargetSummary, OutputsConfig, PermissionsConfig, PlaybackCapabilities,
    PlaybackConfig, ProfileConfig, SCHEMA_VERSION, SystemTtsConfig, TtsBackend, TtsCapabilities,
    TtsConfig, UtterPipeTtsConfig,
};
pub use validate::{ValidationIssue, resolve_and_validate};
