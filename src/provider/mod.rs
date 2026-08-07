//! Host-side UtterPipe provider discovery, protocol, management, and runtime TTS.

mod client;
mod decoder;
mod discovery;
mod management;
mod runtime;

pub use client::{Capabilities, DeliveryMode, ProviderError, ProviderInfo, SessionKind};
pub use discovery::discover_provider;
pub use management::{
    ModelScope, PrepareOptions, import_voice, inspect_provider, list_models, list_voices,
    prepare_provider, remove_provider, render_json, validate_provider,
};
pub use runtime::UtterPipeTts;

pub(crate) const MAX_CONTROL_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_AUDIO_BYTES: u64 = 256 * 1024 * 1024;
pub(crate) const MAX_INCREMENTAL_AUDIO_FRAME_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_STDERR_BYTES: usize = 1024 * 1024;
pub(crate) const SYNTHESIS_TIMEOUT_MS: u64 = 120_000;
pub(crate) const CANCELLATION_GRACE_MS: u64 = 1_000;
pub(crate) const SHUTDOWN_GRACE_MS: u64 = 3_000;
