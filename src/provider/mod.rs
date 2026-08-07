//! Host-side UtterPipe provider discovery, protocol, management, and runtime TTS.

use serde_json::{Map, Value};

mod client;
mod decoder;
mod discovery;
mod management;
mod runtime;
mod schema;

pub use client::{
    AudioDelivery, Capabilities, DeliveryMode, ProviderError, ProviderInfo, SessionKind,
};
pub use discovery::discover_provider;
pub use management::{
    CatalogScope, PrepareOptions, import_asset, inspect_provider, list_catalog, prepare_provider,
    remove_provider, render_json, validate_provider,
};
pub use runtime::UtterPipeTts;

pub(crate) const MAX_CONTROL_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_AUDIO_BYTES: u64 = 256 * 1024 * 1024;
pub(crate) const MAX_INCREMENTAL_AUDIO_FRAME_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_STDERR_BYTES: usize = 1024 * 1024;
pub(crate) const SYNTHESIS_TIMEOUT_MS: u64 = 120_000;
pub(crate) const CANCELLATION_GRACE_MS: u64 = 1_000;
pub(crate) const SHUTDOWN_GRACE_MS: u64 = 3_000;

pub(crate) fn projected_utterance_options_schema(
    initialization: &client::RuntimeInitialization,
    allowed: &[String],
) -> Result<Value, ProviderError> {
    schema::project_allowed_properties(&initialization.utterance_options_schema, allowed)
        .map(schema::projected_object_schema)
        .map_err(ProviderError::Configuration)
}

pub(crate) fn validate_utterance_options(
    options: &Map<String, Value>,
    projected_schema: Option<&Value>,
) -> Result<(), String> {
    schema::validate_request(options, projected_schema)
}
