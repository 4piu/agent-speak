//! Policy-shaped MCP tools and request validation.

use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

#[cfg(windows)]
use std::{
    ffi::OsString,
    os::windows::{ffi::OsStringExt, io::AsRawHandle},
};

#[cfg(target_os = "macos")]
use std::{
    fs::OpenOptions,
    mem::MaybeUninit,
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
};

#[cfg(windows)]
use windows::Win32::{
    Foundation::HANDLE,
    Storage::FileSystem::{FILE_NAME_NORMALIZED, GetFinalPathNameByHandleW},
};

use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

use crate::{
    config::{
        ConcurrencyMode as PolicyConcurrency, EffectiveCapabilities, OutputCategory,
        OutputTargetKind, PresetConfig, PresetKind, ProfileConfig, TtsBackend, ValidatedConfig,
    },
    history::{HistoryMetadata, HistoryRecorder},
    playback::{
        ConcurrencyMode, OutputTarget, PlaybackError, PlaybackHandle, PlaybackJob, PreparedAudio,
        RodioAudio, SystemBackend, SystemTts, TtsAdapter, TtsCapabilities,
    },
    provider::UtterPipeTts,
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
    #[error("playback history could not be initialized: {0}")]
    History(#[source] std::io::Error),
    #[error(transparent)]
    Playback(#[from] PlaybackError),
}

enum ConfiguredTts {
    System(SystemTts),
    Utterpipe(UtterPipeTts),
}

impl TtsAdapter for ConfiguredTts {
    fn capabilities(&self) -> TtsCapabilities {
        match self {
            Self::System(tts) => tts.capabilities(),
            Self::Utterpipe(tts) => tts.capabilities(),
        }
    }

    fn speak(
        &mut self,
        text: String,
        gain: f32,
        completion: crate::playback::CompletionNotifier,
    ) -> Result<(), PlaybackError> {
        match self {
            Self::System(tts) => tts.speak(text, gain, completion),
            Self::Utterpipe(tts) => tts.speak(text, gain, completion),
        }
    }

    fn speak_to(
        &mut self,
        text: String,
        gain: f32,
        target: &OutputTarget,
        completion: crate::playback::CompletionNotifier,
    ) -> Result<(), PlaybackError> {
        match self {
            Self::System(tts) => tts.speak_to(text, gain, target, completion),
            Self::Utterpipe(tts) => tts.speak_to(text, gain, target, completion),
        }
    }

    fn stop(&mut self) -> Result<(), PlaybackError> {
        match self {
            Self::System(tts) => tts.stop(),
            Self::Utterpipe(tts) => tts.stop(),
        }
    }

    fn finished(&mut self) {
        match self {
            Self::System(tts) => tts.finished(),
            Self::Utterpipe(tts) => tts.finished(),
        }
    }
}

#[derive(Clone)]
pub struct AgentSpeakServer {
    profile: Arc<ProfileConfig>,
    capabilities: Arc<EffectiveCapabilities>,
    playback: PlaybackHandle,
    history: Option<HistoryRecorder>,
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
        let tts_config = profile.tts.clone();
        let maximum_audio_seconds = profile.playback.maximum_audio_seconds;
        let maximum_queue_items = profile.playback.maximum_queue_items;
        let playback = PlaybackHandle::spawn(maximum_queue_items, move || {
            let audio = audio_enabled.then(RodioAudio::new).transpose()?;
            let tts = if tts_enabled {
                Some(match &tts_config.backend {
                    TtsBackend::System(system) => ConfiguredTts::System(SystemTts::new(
                        (!system.voice_id.is_empty()).then_some(system.voice_id.as_str()),
                    )?),
                    TtsBackend::Utterpipe(_) => ConfiguredTts::Utterpipe(UtterPipeTts::new(
                        tts_config,
                        maximum_audio_seconds,
                    )?),
                })
            } else {
                None
            };
            Ok(SystemBackend::new(audio, tts))
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

    fn resolve_output_target(
        &self,
        requested: Option<&str>,
        category: OutputCategory,
    ) -> Result<(String, OutputTarget), ToolFailure> {
        let target_id = requested.unwrap_or(&self.profile.outputs.default_target);
        let target = self
            .profile
            .outputs
            .targets
            .iter()
            .find(|target| target.id == target_id)
            .ok_or_else(|| {
                ToolFailure::new(
                    "unknown_output_target",
                    "output target is not present in the startup allowlist",
                    false,
                )
            })?;
        if !target.allow.contains(&category) {
            return Err(ToolFailure::new(
                "output_not_allowed",
                "output target does not allow this kind of playback",
                false,
            ));
        }

        let output = match target.kind {
            OutputTargetKind::SystemDefault => OutputTarget::SystemDefault,
            OutputTargetKind::Device => target
                .device_id
                .clone()
                .map(OutputTarget::DeviceId)
                .ok_or_else(|| {
                    ToolFailure::new(
                        "invalid_output_target",
                        "configured device output has no endpoint identity",
                        false,
                    )
                })?,
        };
        Ok((target.id.clone(), output))
    }

    async fn accept_job(
        &self,
        job: PlaybackJob,
        mode: ConcurrencyMode,
        history_metadata: HistoryMetadata,
    ) -> Result<AcceptanceOutput, ToolFailure> {
        let playback_id = job.id;
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
                if let Some(history) = &self.history {
                    history.forget(playback_id);
                }
                Err(ToolFailure::from_playback(error))
            }
        }
    }

    fn prepare_audio(&self, path: &Path) -> Result<PreparedAudio, ToolFailure> {
        prepare_audio_path(path, duration_limit(&self.profile)).map_err(ToolFailure::from_playback)
    }

    fn prepare_arbitrary_audio(&self, requested: &Path) -> Result<PreparedAudio, ToolFailure> {
        if !is_local_absolute_input(requested) {
            return Err(ToolFailure::new(
                "invalid_path",
                "path must be an absolute local file path, not a network or device path",
                false,
            ));
        }

        // Open once, reject non-local targets behind links, then decoder-
        // preflight and retain the same handle for playback. Replacing the path
        // cannot redirect a later step to a different file.
        let file = open_audio_file(requested).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ToolFailure::new("file_not_found", "audio file was not found", false)
            } else {
                ToolFailure::new("invalid_path", "audio path could not be opened", false)
            }
        })?;
        prepare_opened_audio(file, requested, duration_limit(&self.profile))
            .map_err(ToolFailure::from_playback)
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
        let category = match preset.kind {
            PresetKind::AudioFile => OutputCategory::Audio,
            PresetKind::Text => OutputCategory::Speech,
        };
        let (output_target_id, output_target) =
            match self.resolve_output_target(input.output_target.as_deref(), category) {
                Ok(target) => target,
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
            output_target: output_target_id,
            spoken_text: None,
        };
        let job = match self.job_for_preset(playback_id, preset, gain, output_target) {
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
        let (output_target_id, output_target) = match self
            .resolve_output_target(input.output_target.as_deref(), OutputCategory::Speech)
        {
            Ok(target) => target,
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
            output_target: output_target_id,
            spoken_text,
        };
        let job = PlaybackJob::speech_to(Uuid::new_v4(), input.text, gain as f32, output_target);

        match self.accept_job(job, concurrency, history_metadata).await {
            Ok(output) => Self::result(&output),
            Err(error) => error.into_result(),
        }
    }

    #[tool(
        name = "play_audio_source",
        description = "Audibly play any absolute local regular file that passes media safety limits. Returns after queue acceptance, not after playback completes.",
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
        let (output_target_id, output_target) = match self
            .resolve_output_target(input.output_target.as_deref(), OutputCategory::Audio)
        {
            Ok(target) => target,
            Err(error) => return error.into_result(),
        };
        let prepared = match self.prepare_arbitrary_audio(&input.path) {
            Ok(prepared) => prepared,
            Err(error) => return error.into_result(),
        };
        let history_metadata = HistoryMetadata {
            tool: "play_audio_source",
            source_kind: "arbitrary_local_audio",
            preset_id: None,
            gain,
            concurrency: concurrency_name(concurrency),
            output_target: output_target_id,
            spoken_text: None,
        };
        let job = PlaybackJob::audio_to(Uuid::new_v4(), prepared, gain as f32, output_target);

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
        output_target: OutputTarget,
    ) -> Result<PlaybackJob, ToolFailure> {
        match preset.kind {
            PresetKind::Text => preset
                .text
                .map(|text| {
                    PlaybackJob::speech_to(playback_id, text, gain as f32, output_target.clone())
                })
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
                self.prepare_audio(&path).map(|audio| {
                    PlaybackJob::audio_to(playback_id, audio, gain as f32, output_target)
                })
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
    #[schemars(description = "Optional startup-approved output target alias")]
    output_target: Option<String>,
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
    #[schemars(description = "Optional startup-approved output target alias")]
    output_target: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct PlaySourceInput {
    #[schemars(description = "Absolute path to any local regular audio file")]
    path: PathBuf,
    #[schemars(description = "Optional normalized gain within the advertised policy range")]
    gain: Option<f64>,
    #[schemars(description = "Optional enqueue or interrupt behavior")]
    concurrency: Option<PolicyConcurrency>,
    #[schemars(description = "Optional startup-approved output target alias")]
    output_target: Option<String>,
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
}

impl ToolFailure {
    fn new(code: &'static str, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code,
            message: message.into(),
            retryable,
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
            PlaybackError::OutputUnavailable(_) => Self::new(
                "output_unavailable",
                "the selected output target is unavailable",
                true,
            ),
            PlaybackError::OpenFile(_) | PlaybackError::NotRegularFile => {
                Self::new("invalid_path", "audio file could not be opened", false)
            }
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
                "audio exceeds playback.maximum_audio_seconds",
                false,
            ),
        }
    }

    fn into_result(self) -> CallToolResult {
        let detail = json!({
            "code": self.code,
            "message": self.message,
            "retryable": self.retryable,
        });
        CallToolResult::structured_error(json!({ "error": detail }))
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
        prepare_audio_path(path, duration_limit(profile)).map_err(|source| {
            ServerStartupError::PresetPreflight {
                preset_id: preset.id.clone(),
                source,
            }
        })?;
    }
    Ok(())
}

fn duration_limit(profile: &ProfileConfig) -> Option<Duration> {
    (profile.playback.maximum_audio_seconds != 0)
        .then(|| Duration::from_secs(profile.playback.maximum_audio_seconds))
}

fn prepare_audio_path(
    path: &Path,
    maximum_duration: Option<Duration>,
) -> Result<PreparedAudio, PlaybackError> {
    let file = open_audio_file(path).map_err(|error| PlaybackError::OpenFile(error.to_string()))?;
    prepare_opened_audio(file, path, maximum_duration)
}

#[cfg(target_os = "macos")]
fn open_audio_file(path: &Path) -> std::io::Result<File> {
    // A read-only open of a FIFO can otherwise block before the retained
    // descriptor's regular-file type can be checked.
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
}

#[cfg(not(target_os = "macos"))]
fn open_audio_file(path: &Path) -> std::io::Result<File> {
    File::open(path)
}

fn prepare_opened_audio(
    file: File,
    requested: &Path,
    maximum_duration: Option<Duration>,
) -> Result<PreparedAudio, PlaybackError> {
    let metadata = file
        .metadata()
        .map_err(|error| PlaybackError::OpenFile(error.to_string()))?;
    if !metadata.is_file() {
        return Err(PlaybackError::NotRegularFile);
    }
    let final_path = opened_file_path(&file, requested)
        .map_err(|error| PlaybackError::OpenFile(error.to_string()))?;
    if !is_local_absolute_input(&final_path) {
        return Err(PlaybackError::OpenFile(
            "network and device paths are not supported".into(),
        ));
    }
    if !opened_file_is_local(&file).map_err(|error| PlaybackError::OpenFile(error.to_string()))? {
        return Err(PlaybackError::OpenFile(
            "network filesystems are not supported".into(),
        ));
    }
    set_opened_file_blocking(&file).map_err(|error| PlaybackError::OpenFile(error.to_string()))?;
    PreparedAudio::from_file(file, maximum_duration)
}

fn concurrency_name(mode: ConcurrencyMode) -> &'static str {
    match mode {
        ConcurrencyMode::Enqueue => "enqueue",
        ConcurrencyMode::Interrupt => "interrupt",
    }
}

#[cfg(windows)]
fn opened_file_path(file: &File, _requested: &Path) -> std::io::Result<PathBuf> {
    let handle = HANDLE(file.as_raw_handle());
    let mut buffer = vec![0_u16; 260];
    loop {
        // SAFETY: `handle` is borrowed from a live `File`, and the Windows API
        // writes at most the provided slice length. The file remains owned by
        // the caller for the duration of this call.
        let length =
            unsafe { GetFinalPathNameByHandleW(handle, &mut buffer, FILE_NAME_NORMALIZED) };
        if length == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let length = length as usize;
        if length < buffer.len() {
            return Ok(PathBuf::from(OsString::from_wide(&buffer[..length])));
        }
        buffer.resize(length, 0);
    }
}

#[cfg(target_os = "macos")]
fn opened_file_path(_file: &File, requested: &Path) -> std::io::Result<PathBuf> {
    // Locality is inspected from the retained descriptor below. Returning the
    // already-validated absolute input avoids reopening or resolving a mutable
    // path after the target object has been opened.
    Ok(requested.to_owned())
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn opened_file_path(_file: &File, requested: &Path) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(requested)
}

#[cfg(target_os = "macos")]
fn opened_file_is_local(file: &File) -> std::io::Result<bool> {
    let mut status = MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `file` owns a live descriptor, and `fstatfs` initializes the
    // complete `statfs` value on success before `assume_init` is reached.
    if unsafe { libc::fstatfs(file.as_raw_fd(), status.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let status = unsafe { status.assume_init() };
    Ok(macos_mount_flags_are_local(status.f_flags))
}

#[cfg(target_os = "macos")]
fn macos_mount_flags_are_local(flags: u32) -> bool {
    flags & libc::MNT_LOCAL as u32 != 0
}

#[cfg(target_os = "macos")]
fn set_opened_file_blocking(file: &File) -> std::io::Result<()> {
    let descriptor = file.as_raw_fd();
    // SAFETY: `descriptor` is borrowed from the live retained file. F_GETFL
    // reads its status flags and F_SETFL changes only O_NONBLOCK on that same
    // descriptor after the regular-file check has succeeded.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    if flags & libc::O_NONBLOCK != 0
        && unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags & !libc::O_NONBLOCK) } == -1
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn set_opened_file_blocking(_file: &File) -> std::io::Result<()> {
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn opened_file_is_local(_file: &File) -> std::io::Result<bool> {
    Ok(true)
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
    use std::fs;

    const SYSTEM_OUTPUTS: &str = r#"[outputs]
default_target = "system"

[[outputs.targets]]
id = "system"
description = "Current system default audio device"
kind = "system_default"
allow = ["audio", "speech"]
"#;

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

[playback]
minimum_gain = 0.0
maximum_gain = 0.7
default_gain = 0.4
default_concurrency = "enqueue"
allowed_concurrency = ["enqueue", "interrupt"]
maximum_queue_items = 16
maximum_audio_seconds = 0

{SYSTEM_OUTPUTS}

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

    #[test]
    fn resolves_only_allowed_output_aliases_without_exposing_device_ids() {
        let directory = tempfile::tempdir().unwrap();
        let source = profile_source(true, false, false).replace(
            SYSTEM_OUTPUTS,
            r#"[outputs]
default_target = "system"

[[outputs.targets]]
id = "system"
description = "Current default"
kind = "system_default"
allow = ["audio", "speech"]

[[outputs.targets]]
id = "desk"
description = "Desk speakers"
kind = "device"
device_id = "wasapi:private-endpoint-id"
allow = ["audio"]
"#,
        );
        let server = profile_server(&source, directory.path());

        assert_eq!(
            server
                .resolve_output_target(None, OutputCategory::Speech)
                .unwrap(),
            ("system".to_owned(), OutputTarget::SystemDefault)
        );
        assert_eq!(
            server
                .resolve_output_target(Some("desk"), OutputCategory::Audio)
                .unwrap(),
            (
                "desk".to_owned(),
                OutputTarget::DeviceId("wasapi:private-endpoint-id".to_owned())
            )
        );
        assert_eq!(
            server
                .resolve_output_target(Some("desk"), OutputCategory::Speech)
                .unwrap_err()
                .code,
            "output_not_allowed"
        );
        assert_eq!(
            server
                .resolve_output_target(Some("not-approved"), OutputCategory::Audio)
                .unwrap_err()
                .code,
            "unknown_output_target"
        );
    }

    #[tokio::test]
    async fn arbitrary_audio_accepts_any_local_regular_file() {
        let config_directory = tempfile::tempdir().unwrap();
        let server = profile_server(&profile_source(false, true, false), config_directory.path());
        let first_directory = tempfile::tempdir().unwrap();
        let second_directory = tempfile::tempdir().unwrap();
        let paths = [
            first_directory.path().join("first.wav"),
            second_directory.path().join("second.wav"),
        ];
        for path in &paths {
            fs::write(path, silent_wav()).unwrap();
            let accepted = server
                .play_audio_source(Parameters(PlaySourceInput {
                    path: path.clone(),
                    gain: None,
                    concurrency: None,
                    output_target: None,
                }))
                .await;
            assert_eq!(accepted.is_error, Some(false));
            assert_eq!(accepted.structured_content.unwrap()["status"], "accepted");
        }

        let missing = server
            .play_audio_source(Parameters(PlaySourceInput {
                path: config_directory.path().join("missing.wav"),
                gain: None,
                concurrency: None,
                output_target: None,
            }))
            .await;
        assert_eq!(missing.is_error, Some(true));
        assert_eq!(
            missing.structured_content.unwrap()["error"]["code"],
            "file_not_found"
        );
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn preset_audio_bytes_are_repreflighted_on_every_call() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mutable.wav");
        fs::write(&path, silent_wav()).unwrap();
        let source = format!(
            "{}\n[[presets]]\nid = \"mutable\"\nkind = \"audio_file\"\nsource = \"mutable.wav\"\ndescription = \"\"\ndefault_gain = 0.4\n",
            profile_source(false, false, false)
        );
        let server = profile_server(&source, directory.path());

        fs::write(&path, b"MZ untrusted non-audio payload").unwrap();
        let rejected = server
            .play_audio_preset(Parameters(PlayPresetInput {
                preset_id: "mutable".into(),
                gain: None,
                concurrency: None,
                output_target: None,
            }))
            .await;
        assert_eq!(rejected.is_error, Some(true));
        assert_eq!(
            rejected.structured_content.unwrap()["error"]["code"],
            "unsupported_audio"
        );

        fs::write(&path, silent_wav()).unwrap();
        let accepted = server
            .play_audio_preset(Parameters(PlayPresetInput {
                preset_id: "mutable".into(),
                gain: None,
                concurrency: None,
                output_target: None,
            }))
            .await;
        assert_eq!(accepted.is_error, Some(false));
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
                output_target: None,
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

    #[cfg(windows)]
    #[test]
    fn rejects_unc_and_device_paths_before_filesystem_access() {
        assert!(!is_local_absolute_input(Path::new(
            r"\\server\share\sound.wav"
        )));
        assert!(!is_local_absolute_input(Path::new(r"\\.\C:\sound.wav")));
        assert!(is_local_absolute_input(Path::new(r"C:\sound.wav")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_checks_locality_from_the_opened_filesystem() {
        assert!(macos_mount_flags_are_local(libc::MNT_LOCAL as u32));
        assert!(!macos_mount_flags_are_local(0));

        let file = tempfile::NamedTempFile::new().unwrap();
        assert!(opened_file_is_local(file.as_file()).unwrap());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_retained_handle_does_not_depend_on_the_path_after_open() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("retained.wav");
        fs::write(&path, silent_wav()).unwrap();
        let file = File::open(&path).unwrap();
        fs::remove_file(&path).unwrap();

        let prepared = prepare_opened_audio(file, &path, Some(Duration::from_secs(1))).unwrap();
        assert_eq!(prepared.info().format, crate::playback::AudioFormat::Wav);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_rejects_directories_and_special_files() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(
            prepare_audio_path(directory.path(), None).unwrap_err(),
            PlaybackError::NotRegularFile
        );
        assert_eq!(
            prepare_audio_path(Path::new("/dev/null"), None).unwrap_err(),
            PlaybackError::NotRegularFile
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_rejects_a_fifo_without_blocking_during_open() {
        use std::{ffi::CString, os::unix::ffi::OsStrExt, time::Instant};

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("audio.fifo");
        let native_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `native_path` is a NUL-terminated path owned for this call.
        assert_eq!(unsafe { libc::mkfifo(native_path.as_ptr(), 0o600) }, 0);

        let started = Instant::now();
        assert_eq!(
            prepare_audio_path(&path, None).unwrap_err(),
            PlaybackError::NotRegularFile
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_follows_a_symlink_once_and_retains_the_opened_file() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.wav");
        let link = directory.path().join("link.wav");
        fs::write(&target, silent_wav()).unwrap();
        symlink(&target, &link).unwrap();

        let prepared = prepare_audio_path(&link, Some(Duration::from_secs(1))).unwrap();
        assert_eq!(prepared.info().format, crate::playback::AudioFormat::Wav);
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
        let speak_schema = listed
            .tools
            .iter()
            .find(|tool| tool.name == "speak_text")
            .unwrap();
        assert!(speak_schema.input_schema["properties"]["output_target"].is_object());

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

        let arguments = json!({
            "text": "This request must be rejected before playback.",
            "output_target": "not-approved"
        })
        .as_object()
        .unwrap()
        .clone();
        let rejected = client
            .call_tool(CallToolRequestParams::new("speak_text").with_arguments(arguments))
            .await
            .unwrap();
        assert_eq!(rejected.is_error, Some(true));
        assert_eq!(
            rejected.structured_content.unwrap()["error"]["code"],
            "unknown_output_target"
        );

        client.cancel().await.unwrap();
        server_task.await.unwrap();
        control.shutdown().await.unwrap();
    }
}
