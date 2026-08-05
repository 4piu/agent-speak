//! Policy-shaped MCP tools and request validation.

use std::{
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

use crate::{
    config::{
        ConcurrencyMode as PolicyConcurrency, EffectiveCapabilities, PresetConfig, PresetKind,
        ProfileConfig, ValidatedConfig,
    },
    history::{HistoryMetadata, HistoryRecorder},
    playback::{
        ConcurrencyMode, NativeSystemBackend, PlaybackError, PlaybackHandle, PlaybackJob,
        PreparedAudio,
    },
};

const TOOL_NAMES: [&str; 5] = [
    "get_audio_capabilities",
    "list_audio_presets",
    "play_audio_preset",
    "speak_text",
    "play_audio_source",
];

#[derive(Debug, Error)]
pub enum ServerStartupError {
    #[error("audio preset '{preset_id}' failed decoder preflight: {source}")]
    PresetPreflight {
        preset_id: String,
        #[source]
        source: PlaybackError,
    },
    #[error("audio preset '{preset_id}' exceeds playback.maximum_file_bytes")]
    PresetFileTooLarge { preset_id: String },
    #[error("playback history could not be initialized: {0}")]
    History(#[source] std::io::Error),
    #[error(transparent)]
    Playback(#[from] PlaybackError),
}

#[derive(Clone)]
pub struct AgentSpeakServer {
    profile: Arc<ProfileConfig>,
    capabilities: Arc<EffectiveCapabilities>,
    playback: PlaybackHandle,
    history: Option<HistoryRecorder>,
    rate_limiter: Arc<RateLimiter>,
    tool_router: ToolRouter<Self>,
}

impl AgentSpeakServer {
    /// Preflight configured media, initialize only the required native
    /// backends, and freeze the policy-shaped MCP surface.
    pub fn new(config: ValidatedConfig) -> Result<Self, ServerStartupError> {
        preflight_config_media(config.profile())?;

        let profile = config.profile();
        let audio_enabled = profile.permissions.arbitrary_local_audio
            || profile
                .presets
                .iter()
                .any(|preset| preset.kind == PresetKind::AudioFile);
        let tts_enabled = (profile.permissions.arbitrary_text && profile.tts.enabled)
            || profile
                .presets
                .iter()
                .any(|preset| preset.kind == PresetKind::Text);
        let voice_id = (!profile.tts.voice_id.is_empty()).then(|| profile.tts.voice_id.clone());
        let maximum_queue_items = profile.playback.maximum_queue_items;
        let playback = PlaybackHandle::spawn(maximum_queue_items, move || {
            NativeSystemBackend::initialize(audio_enabled, tts_enabled, voice_id.as_deref())
        })?;

        let history = if profile.logging.history_enabled {
            let path = profile.logging.history_path.as_deref().ok_or_else(|| {
                ServerStartupError::History(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "history path is missing",
                ))
            })?;
            Some(
                HistoryRecorder::start(path, playback.subscribe())
                    .map_err(ServerStartupError::History)?,
            )
        } else {
            None
        };

        let mut server = Self::from_parts(config, playback);
        server.history = history;
        Ok(server)
    }

    fn from_parts(config: ValidatedConfig, playback: PlaybackHandle) -> Self {
        let mut tool_router = Self::tool_router();
        for name in TOOL_NAMES {
            if !config.capabilities().tools.iter().any(|tool| tool == name) {
                tool_router.remove_route(name);
            }
        }

        Self {
            profile: Arc::new(config.profile().clone()),
            capabilities: Arc::new(config.capabilities().clone()),
            playback,
            history: None,
            rate_limiter: Arc::new(RateLimiter::new(
                config.profile().playback.maximum_plays_per_minute,
            )),
            tool_router,
        }
    }

    pub async fn shutdown(&self) -> Result<(), PlaybackError> {
        let result = self.playback.shutdown().await;
        if let Some(history) = &self.history {
            history.shutdown().await;
        }
        result
    }

    /// Visible tool names, primarily useful for startup diagnostics and
    /// contract tests. This never includes policy-disabled routes.
    pub fn registered_tool_names(&self) -> Vec<String> {
        self.tool_router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect()
    }

    fn resolve_playback_options(
        &self,
        gain: Option<f64>,
        concurrency: Option<PolicyConcurrency>,
        default_gain: f64,
    ) -> Result<(f64, ConcurrencyMode), ToolFailure> {
        let gain = gain.unwrap_or(default_gain);
        let policy = &self.profile.playback;
        if !gain.is_finite() || gain < policy.minimum_gain || gain > policy.maximum_gain {
            return Err(ToolFailure::new(
                "invalid_gain",
                format!(
                    "gain must be between {:.2} and {:.2}",
                    policy.minimum_gain, policy.maximum_gain
                ),
                false,
            ));
        }

        let concurrency = concurrency.unwrap_or(policy.default_concurrency);
        if !policy.allowed_concurrency.contains(&concurrency) {
            return Err(ToolFailure::new(
                "concurrency_not_allowed",
                "the requested concurrency mode is not allowed by startup policy",
                false,
            ));
        }

        Ok((
            gain,
            match concurrency {
                PolicyConcurrency::Enqueue => ConcurrencyMode::Enqueue,
                PolicyConcurrency::Interrupt => ConcurrencyMode::Interrupt,
            },
        ))
    }

    async fn accept_job(
        &self,
        job: PlaybackJob,
        mode: ConcurrencyMode,
        history_metadata: HistoryMetadata,
    ) -> Result<AcceptanceOutput, ToolFailure> {
        let playback_id = job.id;
        self.rate_limiter.reserve(playback_id, Instant::now())?;
        if let Some(history) = &self.history {
            history.track(playback_id, history_metadata);
        }

        match self.playback.submit(job, mode).await {
            Ok(accepted) => Ok(AcceptanceOutput {
                playback_id: accepted.playback_id.to_string(),
                status: "accepted",
                accepted_at: OffsetDateTime::now_utc()
                    .format(&Rfc3339)
                    .unwrap_or_else(|_| "unknown".to_owned()),
            }),
            Err(error) => {
                self.rate_limiter.release(playback_id);
                if let Some(history) = &self.history {
                    history.forget(playback_id);
                }
                Err(ToolFailure::from_playback(error))
            }
        }
    }

    fn prepare_audio(&self, path: &Path) -> Result<PreparedAudio, ToolFailure> {
        PreparedAudio::open_with_limits(
            path,
            self.profile.playback.maximum_file_bytes,
            Duration::from_secs(self.profile.playback.maximum_audio_seconds),
        )
        .map_err(ToolFailure::from_playback)
    }

    fn authorize_arbitrary_path(&self, requested: &Path) -> Result<PathBuf, ToolFailure> {
        if !is_local_absolute_input(requested) {
            return Err(ToolFailure::new(
                "invalid_path",
                "path must be an absolute local file path, not a network or device path",
                false,
            ));
        }
        let canonical = fs::canonicalize(requested).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ToolFailure::new("file_not_found", "audio file was not found", false)
            } else {
                ToolFailure::new("invalid_path", "audio path could not be opened", false)
            }
        })?;
        if !self
            .profile
            .permissions
            .approved_directories
            .iter()
            .any(|root| canonical.starts_with(root))
        {
            return Err(ToolFailure::new(
                "path_outside_approved_directories",
                "audio path is outside the directories allowed by startup policy",
                false,
            ));
        }
        if !fs::metadata(&canonical)
            .map(|metadata| metadata.is_file())
            .unwrap_or(false)
        {
            return Err(ToolFailure::new(
                "invalid_path",
                "audio path must identify a regular file",
                false,
            ));
        }
        Ok(canonical)
    }

    fn result<T: Serialize>(value: &T) -> CallToolResult {
        match serde_json::to_value(value) {
            Ok(value) => CallToolResult::structured(value),
            Err(_) => ToolFailure::new(
                "request_not_accepted",
                "the result could not be serialized",
                true,
            )
            .into_result(),
        }
    }
}

#[tool_router]
impl AgentSpeakServer {
    #[tool(
        name = "get_audio_capabilities",
        description = "Return the immutable Agent Speak startup policy and visible audio tools. This does not play audio.",
        output_schema = rmcp::handler::server::tool::schema_for_type::<EffectiveCapabilities>()
    )]
    async fn get_audio_capabilities(&self) -> CallToolResult {
        Self::result(self.capabilities.as_ref())
    }

    #[tool(
        name = "list_audio_presets",
        description = "List the safe preset IDs the user allowed at startup without revealing source paths or speech text.",
        output_schema = rmcp::handler::server::tool::schema_for_type::<PresetListOutput>()
    )]
    async fn list_audio_presets(&self) -> CallToolResult {
        if self.profile.presets.is_empty() {
            return ToolFailure::new(
                "permission_denied",
                "preset playback is not enabled by startup policy",
                false,
            )
            .into_result();
        }
        Self::result(&PresetListOutput {
            presets: self.profile.preset_summaries(),
        })
    }

    #[tool(
        name = "play_audio_preset",
        description = "Audibly play a startup-approved preset. Returns after queue acceptance, not after playback completion.",
        output_schema = rmcp::handler::server::tool::schema_for_type::<AcceptanceOutput>()
    )]
    async fn play_audio_preset(
        &self,
        Parameters(input): Parameters<PlayPresetInput>,
    ) -> CallToolResult {
        let Some(preset) = self
            .profile
            .presets
            .iter()
            .find(|preset| preset.id == input.preset_id)
            .cloned()
        else {
            return ToolFailure::new(
                "unknown_preset",
                "preset ID is not present in the startup catalog",
                false,
            )
            .into_result();
        };

        let (gain, concurrency) =
            match self.resolve_playback_options(input.gain, input.concurrency, preset.default_gain)
            {
                Ok(options) => options,
                Err(error) => return error.into_result(),
            };
        let playback_id = Uuid::new_v4();
        let history_metadata = HistoryMetadata {
            tool: "play_audio_preset",
            source_kind: match preset.kind {
                PresetKind::AudioFile => "preset_audio",
                PresetKind::Text => "preset_text",
            },
            preset_id: Some(preset.id.clone()),
            gain,
            concurrency: concurrency_name(concurrency),
            spoken_text: None,
        };
        let job = match self.job_for_preset(playback_id, preset, gain) {
            Ok(job) => job,
            Err(error) => return error.into_result(),
        };

        match self.accept_job(job, concurrency, history_metadata).await {
            Ok(output) => Self::result(&output),
            Err(error) => error.into_result(),
        }
    }

    #[tool(
        name = "speak_text",
        description = "Audibly speak arbitrary plain text through system TTS. Returns after queue acceptance, not after speech completes.",
        output_schema = rmcp::handler::server::tool::schema_for_type::<AcceptanceOutput>()
    )]
    async fn speak_text(&self, Parameters(input): Parameters<SpeakTextInput>) -> CallToolResult {
        if !self.profile.permissions.arbitrary_text || !self.profile.tts.enabled {
            return ToolFailure::new(
                "permission_denied",
                "arbitrary speech is not enabled by startup policy",
                false,
            )
            .into_result();
        }
        if input.text.trim().is_empty() {
            return ToolFailure::new("text_empty", "text must not be empty", false).into_result();
        }
        if input.text.chars().count() > self.profile.tts.maximum_characters {
            return ToolFailure::new(
                "text_too_long",
                format!(
                    "text must contain no more than {} characters",
                    self.profile.tts.maximum_characters
                ),
                false,
            )
            .into_result();
        }
        let (gain, concurrency) = match self.resolve_playback_options(
            input.gain,
            input.concurrency,
            self.profile.playback.default_gain,
        ) {
            Ok(options) => options,
            Err(error) => return error.into_result(),
        };
        let spoken_text = self
            .profile
            .logging
            .history_include_spoken_text
            .then(|| input.text.clone());
        let history_metadata = HistoryMetadata {
            tool: "speak_text",
            source_kind: "arbitrary_text",
            preset_id: None,
            gain,
            concurrency: concurrency_name(concurrency),
            spoken_text,
        };
        let job = PlaybackJob::speech(Uuid::new_v4(), input.text, gain as f32);

        match self.accept_job(job, concurrency, history_metadata).await {
            Ok(output) => Self::result(&output),
            Err(error) => error.into_result(),
        }
    }

    #[tool(
        name = "play_audio_source",
        description = "Audibly play an absolute local audio path within a startup-approved directory. Returns after queue acceptance, not after playback completes.",
        output_schema = rmcp::handler::server::tool::schema_for_type::<AcceptanceOutput>()
    )]
    async fn play_audio_source(
        &self,
        Parameters(input): Parameters<PlaySourceInput>,
    ) -> CallToolResult {
        if !self.profile.permissions.arbitrary_local_audio {
            return ToolFailure::new(
                "permission_denied",
                "arbitrary local audio is not enabled by startup policy",
                false,
            )
            .into_result();
        }
        let (gain, concurrency) = match self.resolve_playback_options(
            input.gain,
            input.concurrency,
            self.profile.playback.default_gain,
        ) {
            Ok(options) => options,
            Err(error) => return error.into_result(),
        };
        let canonical = match self.authorize_arbitrary_path(&input.path) {
            Ok(path) => path,
            Err(error) => return error.into_result(),
        };
        let prepared = match self.prepare_audio(&canonical) {
            Ok(prepared) => prepared,
            Err(error) => return error.into_result(),
        };
        let history_metadata = HistoryMetadata {
            tool: "play_audio_source",
            source_kind: "arbitrary_local_audio",
            preset_id: None,
            gain,
            concurrency: concurrency_name(concurrency),
            spoken_text: None,
        };
        let job = PlaybackJob::audio(Uuid::new_v4(), prepared, gain as f32);

        match self.accept_job(job, concurrency, history_metadata).await {
            Ok(output) => Self::result(&output),
            Err(error) => error.into_result(),
        }
    }
}

impl AgentSpeakServer {
    fn job_for_preset(
        &self,
        playback_id: Uuid,
        preset: PresetConfig,
        gain: f64,
    ) -> Result<PlaybackJob, ToolFailure> {
        match preset.kind {
            PresetKind::Text => preset
                .text
                .map(|text| PlaybackJob::speech(playback_id, text, gain as f32))
                .ok_or_else(|| {
                    ToolFailure::new(
                        "playback_unavailable",
                        "configured text preset is unavailable",
                        true,
                    )
                }),
            PresetKind::AudioFile => {
                let path = preset.source.ok_or_else(|| {
                    ToolFailure::new(
                        "playback_unavailable",
                        "configured audio preset is unavailable",
                        true,
                    )
                })?;
                self.prepare_audio(&path)
                    .map(|audio| PlaybackJob::audio(playback_id, audio, gain as f32))
            }
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for AgentSpeakServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "agent-speak",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Discover the startup policy first, then use only listed tools. Playback is audible and fire-and-forget.",
            )
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct PlayPresetInput {
    #[schemars(description = "Startup-approved preset identifier")]
    preset_id: String,
    #[schemars(description = "Optional normalized gain within the advertised policy range")]
    gain: Option<f64>,
    #[schemars(description = "Optional enqueue or interrupt behavior")]
    concurrency: Option<PolicyConcurrency>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SpeakTextInput {
    #[schemars(description = "Plain text to speak audibly; SSML is not interpreted")]
    text: String,
    #[schemars(description = "Optional normalized gain within the advertised policy range")]
    gain: Option<f64>,
    #[schemars(description = "Optional enqueue or interrupt behavior")]
    concurrency: Option<PolicyConcurrency>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct PlaySourceInput {
    #[schemars(description = "Absolute local path within a startup-approved directory")]
    path: PathBuf,
    #[schemars(description = "Optional normalized gain within the advertised policy range")]
    gain: Option<f64>,
    #[schemars(description = "Optional enqueue or interrupt behavior")]
    concurrency: Option<PolicyConcurrency>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct AcceptanceOutput {
    playback_id: String,
    status: &'static str,
    accepted_at: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct PresetListOutput {
    presets: Vec<crate::config::PresetSummary>,
}

#[derive(Debug)]
struct ToolFailure {
    code: &'static str,
    message: String,
    retryable: bool,
    retry_after_seconds: Option<u64>,
}

impl ToolFailure {
    fn new(code: &'static str, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code,
            message: message.into(),
            retryable,
            retry_after_seconds: None,
        }
    }

    fn rate_limited(retry_after_seconds: u64) -> Self {
        Self {
            code: "rate_limited",
            message: "playback rate limit reached; retry later".to_owned(),
            retryable: true,
            retry_after_seconds: Some(retry_after_seconds),
        }
    }

    fn from_playback(error: PlaybackError) -> Self {
        match error {
            PlaybackError::QueueFull => Self::new("queue_full", "the playback queue is full", true),
            PlaybackError::ActorBusy => Self::new(
                "request_not_accepted",
                "the playback service is busy; retry later",
                true,
            ),
            PlaybackError::ActorClosed | PlaybackError::Backend(_) => Self::new(
                "playback_unavailable",
                "the playback backend is unavailable",
                true,
            ),
            PlaybackError::OpenFile(_) | PlaybackError::NotRegularFile => {
                Self::new("invalid_path", "audio file could not be opened", false)
            }
            PlaybackError::FileTooLarge => Self::new(
                "file_too_large",
                "audio file exceeds the configured byte limit",
                false,
            ),
            PlaybackError::UnsupportedAudio => {
                Self::new("unsupported_audio", "audio format is not supported", false)
            }
            PlaybackError::DurationUnknown => Self::new(
                "duration_unknown",
                "audio duration could not be determined",
                false,
            ),
            PlaybackError::AudioTooLong => Self::new(
                "audio_too_long",
                "audio exceeds the configured duration limit",
                false,
            ),
        }
    }

    fn into_result(self) -> CallToolResult {
        let mut detail = json!({
            "code": self.code,
            "message": self.message,
            "retryable": self.retryable,
        });
        if let Some(seconds) = self.retry_after_seconds
            && let Value::Object(fields) = &mut detail
        {
            fields.insert("retry_after_seconds".to_owned(), json!(seconds));
        }
        CallToolResult::structured_error(json!({ "error": detail }))
    }
}

struct RateLimiter {
    maximum_per_minute: u32,
    accepted: Mutex<VecDeque<(Instant, Uuid)>>,
}

impl RateLimiter {
    fn new(maximum_per_minute: u32) -> Self {
        Self {
            maximum_per_minute,
            accepted: Mutex::new(VecDeque::new()),
        }
    }

    fn reserve(&self, playback_id: Uuid, now: Instant) -> Result<(), ToolFailure> {
        let window = Duration::from_secs(60);
        let mut accepted = self.accepted.lock().expect("rate limiter mutex poisoned");
        while accepted
            .front()
            .is_some_and(|(timestamp, _)| now.saturating_duration_since(*timestamp) >= window)
        {
            accepted.pop_front();
        }
        if accepted.len() >= self.maximum_per_minute as usize {
            let retry_after = accepted
                .front()
                .map(|(timestamp, _)| {
                    window
                        .saturating_sub(now.saturating_duration_since(*timestamp))
                        .as_secs()
                        .max(1)
                })
                .unwrap_or(1);
            return Err(ToolFailure::rate_limited(retry_after));
        }
        accepted.push_back((now, playback_id));
        Ok(())
    }

    fn release(&self, playback_id: Uuid) {
        self.accepted
            .lock()
            .expect("rate limiter mutex poisoned")
            .retain(|(_, id)| *id != playback_id);
    }
}

/// Perform decoder and duration checks shared by `validate` and server startup
/// without opening an output device.
pub fn preflight_config_media(profile: &ProfileConfig) -> Result<(), ServerStartupError> {
    for preset in &profile.presets {
        if preset.kind != PresetKind::AudioFile {
            continue;
        }
        let Some(path) = preset.source.as_ref() else {
            continue;
        };
        PreparedAudio::open_with_limits(
            path,
            profile.playback.maximum_file_bytes,
            Duration::from_secs(profile.playback.maximum_audio_seconds),
        )
        .map_err(|source| match source {
            PlaybackError::FileTooLarge => ServerStartupError::PresetFileTooLarge {
                preset_id: preset.id.clone(),
            },
            source => ServerStartupError::PresetPreflight {
                preset_id: preset.id.clone(),
                source,
            },
        })?;
    }
    Ok(())
}

fn concurrency_name(mode: ConcurrencyMode) -> &'static str {
    match mode {
        ConcurrencyMode::Enqueue => "enqueue",
        ConcurrencyMode::Interrupt => "interrupt",
    }
}

#[cfg(windows)]
fn is_local_absolute_input(path: &Path) -> bool {
    use std::path::{Component, Prefix};

    path.is_absolute()
        && matches!(
            path.components().next(),
            Some(Component::Prefix(prefix))
                if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
        )
}

#[cfg(not(windows))]
fn is_local_absolute_input(path: &Path) -> bool {
    path.is_absolute()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{ConfigOrigin, QuickProfileOverrides, parse_config, quick_profile},
        playback::{CompletionNotifier, PlaybackBackend},
    };
    use rmcp::{ClientHandler, ServiceExt, model::CallToolRequestParams};
    use std::io::Write;

    struct NoopBackend;

    impl PlaybackBackend for NoopBackend {
        fn start(
            &mut self,
            _job: PlaybackJob,
            completion: CompletionNotifier,
        ) -> Result<(), PlaybackError> {
            completion.complete();
            Ok(())
        }

        fn stop(&mut self) -> Result<(), PlaybackError> {
            Ok(())
        }
    }

    fn quick_server() -> AgentSpeakServer {
        let config = quick_profile(QuickProfileOverrides::default()).unwrap();
        let playback = PlaybackHandle::spawn(16, || Ok(NoopBackend)).unwrap();
        AgentSpeakServer::from_parts(config, playback)
    }

    fn profile_server(source: &str, base: &Path) -> AgentSpeakServer {
        let config = parse_config(source, base, ConfigOrigin::QuickProfile).unwrap();
        let playback = PlaybackHandle::spawn(16, || Ok(NoopBackend)).unwrap();
        AgentSpeakServer::from_parts(config, playback)
    }

    fn profile_source(arbitrary_text: bool, arbitrary_audio: bool, preset: bool) -> String {
        let preset = if preset {
            r#"
[[presets]]
id = "attention"
kind = "text"
text = "Attention is needed."
description = ""
default_gain = 0.4
"#
        } else {
            ""
        };
        format!(
            r#"
schema_version = 1
profile_name = "contract-test"

[permissions]
arbitrary_text = {arbitrary_text}
arbitrary_local_audio = {arbitrary_audio}
approved_directories = ["."]

[playback]
minimum_gain = 0.0
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
{preset}
"#
        )
    }

    #[test]
    fn quick_profile_exposes_only_capabilities_and_speech() {
        assert_eq!(
            quick_server().registered_tool_names(),
            vec!["get_audio_capabilities", "speak_text"]
        );
    }

    #[test]
    fn profile_matrix_shapes_the_complete_tool_surface() {
        let directory = tempfile::tempdir().unwrap();
        let all = profile_server(&profile_source(true, true, true), directory.path());
        assert_eq!(
            all.registered_tool_names(),
            vec![
                "get_audio_capabilities",
                "list_audio_presets",
                "play_audio_preset",
                "play_audio_source",
                "speak_text",
            ]
        );

        let inspection_only =
            profile_server(&profile_source(false, false, false), directory.path());
        assert_eq!(
            inspection_only.registered_tool_names(),
            vec!["get_audio_capabilities"]
        );
    }

    #[tokio::test]
    async fn arbitrary_audio_enforces_root_and_accepts_preflighted_handle() {
        let directory = tempfile::tempdir().unwrap();
        let server = profile_server(&profile_source(false, true, false), directory.path());
        let mut audio = tempfile::Builder::new()
            .suffix(".wav")
            .tempfile_in(directory.path())
            .unwrap();
        audio.write_all(&silent_wav()).unwrap();

        let accepted = server
            .play_audio_source(Parameters(PlaySourceInput {
                path: audio.path().to_owned(),
                gain: None,
                concurrency: None,
            }))
            .await;
        assert_eq!(accepted.is_error, Some(false));
        assert_eq!(accepted.structured_content.unwrap()["status"], "accepted");

        let outside = tempfile::NamedTempFile::new().unwrap();
        let rejected = server
            .play_audio_source(Parameters(PlaySourceInput {
                path: outside.path().to_owned(),
                gain: None,
                concurrency: None,
            }))
            .await;
        assert_eq!(rejected.is_error, Some(true));
        assert_eq!(
            rejected.structured_content.unwrap()["error"]["code"],
            "path_outside_approved_directories"
        );
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn opted_in_history_records_arbitrary_speech_without_blocking_acceptance() {
        let directory = tempfile::tempdir().unwrap();
        let source = profile_source(true, false, false).replace(
            "history_enabled = false\nhistory_include_spoken_text = false",
            "history_enabled = true\nhistory_path = \"history.jsonl\"\nhistory_include_spoken_text = true",
        );
        let config = parse_config(&source, directory.path(), ConfigOrigin::QuickProfile).unwrap();
        let history_path = config.profile().logging.history_path.clone().unwrap();
        let playback = PlaybackHandle::spawn(16, || Ok(NoopBackend)).unwrap();
        let history = HistoryRecorder::start(&history_path, playback.subscribe()).unwrap();
        let mut server = AgentSpeakServer::from_parts(config, playback);
        server.history = Some(history);

        let result = server
            .speak_text(Parameters(SpeakTextInput {
                text: "integration secret marker".to_owned(),
                gain: None,
                concurrency: None,
            }))
            .await;
        assert_eq!(result.is_error, Some(false));
        server.shutdown().await.unwrap();

        let records = fs::read_to_string(history_path).unwrap();
        assert!(records.contains("\"tool\":\"speak_text\""));
        assert!(records.contains("integration secret marker"));
    }

    fn silent_wav() -> Vec<u8> {
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&38_u32.to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&8_000_u32.to_le_bytes());
        wav.extend_from_slice(&16_000_u32.to_le_bytes());
        wav.extend_from_slice(&2_u16.to_le_bytes());
        wav.extend_from_slice(&16_u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&2_u32.to_le_bytes());
        wav.extend_from_slice(&0_i16.to_le_bytes());
        wav
    }

    #[test]
    fn sliding_window_releases_rejected_reservations() {
        let limiter = RateLimiter::new(1);
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let now = Instant::now();
        limiter.reserve(first, now).unwrap();
        assert!(limiter.reserve(second, now).is_err());
        limiter.release(first);
        limiter.reserve(second, now).unwrap();
    }

    #[test]
    fn sliding_window_expires_old_acceptances() {
        let limiter = RateLimiter::new(1);
        let now = Instant::now();
        limiter.reserve(Uuid::new_v4(), now).unwrap();
        limiter
            .reserve(Uuid::new_v4(), now + Duration::from_secs(60))
            .unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn rejects_unc_and_device_paths_before_filesystem_access() {
        assert!(!is_local_absolute_input(Path::new(
            r"\\server\share\sound.wav"
        )));
        assert!(!is_local_absolute_input(Path::new(r"\\.\C:\sound.wav")));
        assert!(is_local_absolute_input(Path::new(r"C:\sound.wav")));
    }

    #[derive(Clone, Default)]
    struct TestClient;

    impl ClientHandler for TestClient {}

    #[tokio::test]
    async fn mcp_lists_policy_shaped_tools_and_returns_structured_results() {
        let server = quick_server();
        let control = server.clone();
        let (server_transport, client_transport) = tokio::io::duplex(16 * 1024);
        let server_task = tokio::spawn(async move {
            server
                .serve(server_transport)
                .await
                .unwrap()
                .waiting()
                .await
                .unwrap();
        });
        let client = TestClient.serve(client_transport).await.unwrap();

        let listed = client.list_tools(None).await.unwrap();
        let names: Vec<_> = listed.tools.iter().map(|tool| tool.name.as_ref()).collect();
        assert_eq!(names, ["get_audio_capabilities", "speak_text"]);
        assert!(listed.tools.iter().all(|tool| tool.output_schema.is_some()));

        let capabilities = client
            .call_tool(CallToolRequestParams::new("get_audio_capabilities"))
            .await
            .unwrap();
        assert_eq!(capabilities.is_error, Some(false));
        assert!(capabilities.structured_content.is_some());
        assert!(!capabilities.content.is_empty());

        let arguments = json!({ "text": "  " }).as_object().unwrap().clone();
        let rejected = client
            .call_tool(CallToolRequestParams::new("speak_text").with_arguments(arguments))
            .await
            .unwrap();
        assert_eq!(rejected.is_error, Some(true));
        assert_eq!(
            rejected.structured_content.unwrap()["error"]["code"],
            "text_empty"
        );

        client.cancel().await.unwrap();
        server_task.await.unwrap();
        control.shutdown().await.unwrap();
    }
}
