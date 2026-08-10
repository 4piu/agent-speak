//! Policy-shaped MCP tools and request validation.

use std::{
    collections::BTreeMap,
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
        AudioCueConfig, AudioCueKind, ConcurrencyMode as PolicyConcurrency, EffectiveCapabilities,
        OutputCategory, OutputTargetKind, ProfileConfig, TtsBackend, ValidatedConfig,
    },
    history::{HistoryMetadata, HistoryRecorder},
    playback::{
        ConcurrencyMode, OutputTarget, PlaybackError, PlaybackHandle, PlaybackJob, PlaybackState,
        PreparedAudio, RodioAudio, SystemBackend, SystemTts, TtsAdapter, TtsCapabilities,
    },
    provider::{UtterPipeTts, projected_utterance_options_schema, validate_utterance_options},
};

const TOOL_NAMES: [&str; 7] = [
    "cancel_playback",
    "get_audio_capabilities",
    "get_playback_status",
    "list_audio_cues",
    "play_audio_cue",
    "speak_text",
    "play_audio_source",
];

const SERVER_INSTRUCTIONS: &str = "Agent Speak creates audible, non-idempotent side effects. Use it only when the user asks for audible output or a startup-approved audio cue description clearly applies. Before the first playback action in a session, call get_audio_capabilities. Call list_audio_cues before selecting an audio cue unless its catalog was already retrieved in this session. Omit gain, concurrency, and output_target to use the user's configured defaults. enqueue waits behind active playback; interrupt stops every active item, starts the replacement, and retains already queued items; mix starts alongside active items up to the advertised stream limit and otherwise waits in the FIFO. A successful playback call means accepted into the queue, not completed or audible. Use get_playback_status with its playback ID when terminal confirmation matters; do not repeat playback merely because completion is unconfirmed. Use cancel_playback only when the user asks to stop an accepted item or when stale playback must be stopped.";

#[derive(Debug, Error)]
pub enum ServerStartupError {
    #[error("audio cue '{cue_id}' failed decoder preflight: {source}")]
    AudioCuePreflight {
        cue_id: String,
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

    fn speak_with_options_to(
        &mut self,
        text: String,
        utterance_options: serde_json::Map<String, serde_json::Value>,
        gain: f32,
        target: &OutputTarget,
        completion: crate::playback::CompletionNotifier,
    ) -> Result<(), PlaybackError> {
        match self {
            Self::System(tts) => {
                tts.speak_with_options_to(text, utterance_options, gain, target, completion)
            }
            Self::Utterpipe(tts) => {
                tts.speak_with_options_to(text, utterance_options, gain, target, completion)
            }
        }
    }

    fn stop(&mut self, playback_id: Uuid) -> Result<(), PlaybackError> {
        match self {
            Self::System(tts) => tts.stop(playback_id),
            Self::Utterpipe(tts) => tts.stop(playback_id),
        }
    }

    fn finished(&mut self, playback_id: Uuid) {
        match self {
            Self::System(tts) => tts.finished(playback_id),
            Self::Utterpipe(tts) => tts.finished(playback_id),
        }
    }
}

#[derive(Clone)]
pub struct AgentSpeakServer {
    profile: Arc<ProfileConfig>,
    capabilities: Arc<EffectiveCapabilities>,
    playback: PlaybackHandle,
    history: Option<HistoryRecorder>,
    utterance_options_schema: Arc<Option<serde_json::Value>>,
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
                .audio_cues
                .iter()
                .any(|cue| cue.kind == AudioCueKind::AudioFile);
        let tts_enabled = (profile.permissions.arbitrary_text && profile.tts.enabled)
            || profile
                .audio_cues
                .iter()
                .any(|cue| cue.kind == AudioCueKind::Speech);
        let tts_config = profile.tts.clone();
        let maximum_audio_seconds = profile.playback.maximum_audio_seconds;
        let maximum_queue_items = profile.playback.maximum_queue_items;
        let maximum_mix_streams = profile.playback.maximum_mix_streams;
        let (playback, utterance_options_schema) =
            PlaybackHandle::spawn_with_metadata_and_active_capacity(
                maximum_queue_items,
                maximum_mix_streams,
                move || {
                    let output = (audio_enabled || tts_enabled)
                        .then(RodioAudio::new)
                        .transpose()?;
                    let audio = audio_enabled.then(|| {
                        output
                            .as_ref()
                            .expect("enabled output service")
                            .shared_client()
                    });
                    let (tts, utterance_options_schema) = if tts_enabled {
                        Some(match &tts_config.backend {
                            TtsBackend::System(system) => (
                                ConfiguredTts::System(SystemTts::new_with_audio(
                                    (!system.voice_id.is_empty())
                                        .then_some(system.voice_id.as_str()),
                                    output
                                        .as_ref()
                                        .expect("enabled output service")
                                        .shared_client(),
                                )?),
                                None,
                            ),
                            TtsBackend::Utterpipe(provider) => {
                                let allowed = provider.agent_utterance_options.clone();
                                let tts = UtterPipeTts::new_with_audio(
                                    tts_config.clone(),
                                    maximum_audio_seconds,
                                    output
                                        .as_ref()
                                        .expect("enabled output service")
                                        .shared_client(),
                                )?;
                                let schema = projected_utterance_options_schema(
                                    tts.initialization(),
                                    &allowed,
                                )
                                .map_err(|error| PlaybackError::Backend(error.to_string()))?;
                                (ConfiguredTts::Utterpipe(tts), Some(schema))
                            }
                        })
                        .map_or((None, None), |(tts, schema)| (Some(tts), schema))
                    } else {
                        (None, None)
                    };
                    Ok((SystemBackend::new(audio, tts), utterance_options_schema))
                },
            )?;

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

        let mut server = Self::from_parts(config, playback, utterance_options_schema);
        server.history = history;
        Ok(server)
    }

    fn from_parts(
        config: ValidatedConfig,
        playback: PlaybackHandle,
        utterance_options_schema: Option<serde_json::Value>,
    ) -> Self {
        let mut tool_router = Self::tool_router();
        for name in TOOL_NAMES {
            if !config.capabilities().tools.iter().any(|tool| tool == name) {
                tool_router.remove_route(name);
            }
        }
        if let Some(route) = tool_router.map.get_mut("speak_text") {
            let input_schema = Arc::make_mut(&mut route.attr.input_schema);
            let properties = input_schema
                .get_mut("properties")
                .and_then(serde_json::Value::as_object_mut)
                .expect("generated speak_text schema has object properties");
            match utterance_options_schema.as_ref() {
                Some(schema) => {
                    properties.insert("utterance_options".into(), schema.clone());
                }
                None => {
                    properties.remove("utterance_options");
                }
            }
        }

        Self {
            profile: Arc::new(config.profile().clone()),
            capabilities: Arc::new(config.capabilities().clone()),
            playback,
            history: None,
            utterance_options_schema: Arc::new(utterance_options_schema),
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

    /// Actor handle used by the optional local human-control channel.
    pub fn playback_handle(&self) -> PlaybackHandle {
        self.playback.clone()
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
                PolicyConcurrency::Mix => ConcurrencyMode::Mix,
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
        description = "Call this once before the first audible action in a session. Returns the immutable startup policy, visible tools, output aliases, defaults, and limits without playing audio.",
        annotations(title = "Get Audio Capabilities", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false),
        output_schema = rmcp::handler::server::tool::schema_for_type::<EffectiveCapabilities>()
    )]
    async fn get_audio_capabilities(&self) -> CallToolResult {
        Self::result(self.capabilities.as_ref())
    }

    #[tool(
        name = "cancel_playback",
        description = "Stop active playback or remove a queued item by an ID previously accepted by this server. Cancelling an already-terminal item is an idempotent no-op. This affects audible output and should be used only when the user asks to stop an item or stale playback must be stopped.",
        annotations(title = "Cancel Playback", read_only_hint = false, destructive_hint = true, idempotent_hint = true, open_world_hint = false),
        output_schema = rmcp::handler::server::tool::schema_for_type::<CancellationOutput>()
    )]
    async fn cancel_playback(
        &self,
        Parameters(input): Parameters<PlaybackIdInput>,
    ) -> CallToolResult {
        let playback_id = match parse_playback_id(&input.playback_id) {
            Ok(playback_id) => playback_id,
            Err(error) => return error.into_result(),
        };
        match self.playback.cancel(playback_id).await {
            Ok(Some(cancellation)) => Self::result(&CancellationOutput {
                playback_id: cancellation.playback_id.to_string(),
                status: playback_state_name(cancellation.state),
                terminal: cancellation.state.is_terminal(),
                cancelled: cancellation.cancelled,
            }),
            Ok(None) => unknown_playback_id().into_result(),
            Err(error) => ToolFailure::from_playback(error).into_result(),
        }
    }

    #[tool(
        name = "get_playback_status",
        description = "Return the current or retained terminal lifecycle state for a playback ID previously accepted by this server. This does not play audio and never returns spoken text, cue text, or source paths. Recent terminal results are retained only up to the advertised item limit.",
        annotations(title = "Get Playback Status", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false),
        output_schema = rmcp::handler::server::tool::schema_for_type::<PlaybackStatusOutput>()
    )]
    async fn get_playback_status(
        &self,
        Parameters(input): Parameters<PlaybackIdInput>,
    ) -> CallToolResult {
        let playback_id = match parse_playback_id(&input.playback_id) {
            Ok(playback_id) => playback_id,
            Err(error) => return error.into_result(),
        };
        match self.playback.status(playback_id).await {
            Ok(Some(status)) => Self::result(&PlaybackStatusOutput {
                playback_id: status.playback_id.to_string(),
                status: playback_state_name(status.state),
                terminal: status.state.is_terminal(),
                error_code: (status.state == PlaybackState::Failed)
                    .then_some("playback_unavailable"),
            }),
            Ok(None) => unknown_playback_id().into_result(),
            Err(error) => ToolFailure::from_playback(error).into_result(),
        }
    }

    #[tool(
        name = "list_audio_cues",
        description = "Call before choosing an audio cue unless this session already retrieved the catalog. Returns startup-approved audio cue IDs and descriptions explaining when they apply; does not play audio or reveal source paths or speech text.",
        annotations(title = "List Audio Cues", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false),
        output_schema = rmcp::handler::server::tool::schema_for_type::<AudioCueListOutput>()
    )]
    async fn list_audio_cues(&self) -> CallToolResult {
        if self.profile.audio_cues.is_empty() {
            return ToolFailure::new(
                "permission_denied",
                "audio cue playback is not enabled by startup policy",
                false,
            )
            .into_result();
        }
        Self::result(&AudioCueListOutput {
            audio_cues: self.profile.audio_cue_summaries(),
        })
    }

    #[tool(
        name = "play_audio_cue",
        description = "Audibly play a startup-approved audio cue returned by list_audio_cues. This is a non-idempotent side effect: success means queue acceptance, not completed or audible playback, so do not repeat it merely because completion is unconfirmed. Omit optional fields to use configured defaults.",
        annotations(title = "Play Audio Cue", read_only_hint = false, destructive_hint = false, idempotent_hint = false, open_world_hint = true),
        output_schema = rmcp::handler::server::tool::schema_for_type::<AcceptanceOutput>()
    )]
    async fn play_audio_cue(
        &self,
        Parameters(input): Parameters<PlayAudioCueInput>,
    ) -> CallToolResult {
        let Some(cue) = self
            .profile
            .audio_cues
            .iter()
            .find(|cue| cue.id == input.cue_id)
            .cloned()
        else {
            return ToolFailure::new(
                "unknown_audio_cue",
                "audio cue ID is not present in the startup catalog",
                false,
            )
            .into_result();
        };

        let (gain, concurrency) =
            match self.resolve_playback_options(input.gain, input.concurrency, cue.default_gain) {
                Ok(options) => options,
                Err(error) => return error.into_result(),
            };
        let category = match cue.kind {
            AudioCueKind::AudioFile => OutputCategory::Audio,
            AudioCueKind::Speech => OutputCategory::Speech,
        };
        let (output_target_id, output_target) =
            match self.resolve_output_target(input.output_target.as_deref(), category) {
                Ok(target) => target,
                Err(error) => return error.into_result(),
            };
        let playback_id = Uuid::new_v4();
        let history_metadata = HistoryMetadata {
            tool: "play_audio_cue",
            source_kind: match cue.kind {
                AudioCueKind::AudioFile => "cue_audio_file",
                AudioCueKind::Speech => "cue_speech",
            },
            cue_id: Some(cue.id.clone()),
            gain,
            concurrency: concurrency_name(concurrency),
            output_target: output_target_id,
            spoken_text: None,
        };
        let job = match self.job_for_audio_cue(playback_id, cue, gain, output_target) {
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
        description = "Audibly speak arbitrary plain text when the user requests spoken output; do not use it merely because the tool is available. This is a non-idempotent side effect: success means queue acceptance, not completed or audible speech, so do not repeat it merely because completion is unconfirmed. Omit optional fields to use configured defaults.",
        annotations(title = "Speak Text Audibly", read_only_hint = false, destructive_hint = false, idempotent_hint = false, open_world_hint = true),
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
        let utterance_options: serde_json::Map<String, serde_json::Value> = input
            .utterance_options
            .unwrap_or_default()
            .into_iter()
            .collect();
        if let Err(message) = validate_utterance_options(
            &utterance_options,
            self.utterance_options_schema.as_ref().as_ref(),
        ) {
            return ToolFailure::new("invalid_utterance_options", message, false).into_result();
        }
        let spoken_text = self
            .profile
            .logging
            .history_include_spoken_text
            .then(|| input.text.clone());
        let history_metadata = HistoryMetadata {
            tool: "speak_text",
            source_kind: "arbitrary_text",
            cue_id: None,
            gain,
            concurrency: concurrency_name(concurrency),
            output_target: output_target_id,
            spoken_text,
        };
        let job = PlaybackJob::speech_with_options_to(
            Uuid::new_v4(),
            input.text,
            utterance_options,
            gain as f32,
            output_target,
        );

        match self.accept_job(job, concurrency, history_metadata).await {
            Ok(output) => Self::result(&output),
            Err(error) => error.into_result(),
        }
    }

    #[tool(
        name = "play_audio_source",
        description = "Audibly play an absolute local regular audio file when the user requests it. This is a non-idempotent side effect: success means queue acceptance, not completed or audible playback, so do not repeat it merely because completion is unconfirmed. Omit optional fields to use configured defaults.",
        annotations(title = "Play Local Audio File", read_only_hint = false, destructive_hint = false, idempotent_hint = false, open_world_hint = false),
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
            cue_id: None,
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
    fn job_for_audio_cue(
        &self,
        playback_id: Uuid,
        cue: AudioCueConfig,
        gain: f64,
        output_target: OutputTarget,
    ) -> Result<PlaybackJob, ToolFailure> {
        match cue.kind {
            AudioCueKind::Speech => cue
                .text
                .map(|text| {
                    PlaybackJob::speech_to(playback_id, text, gain as f32, output_target.clone())
                })
                .ok_or_else(|| {
                    ToolFailure::new(
                        "playback_unavailable",
                        "configured speech cue is unavailable",
                        true,
                    )
                }),
            AudioCueKind::AudioFile => {
                let path = cue.source.ok_or_else(|| {
                    ToolFailure::new(
                        "playback_unavailable",
                        "configured audio-file cue is unavailable",
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
            .with_instructions(SERVER_INSTRUCTIONS)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct PlaybackIdInput {
    #[schemars(description = "Playback identifier returned by an accepted playback tool call")]
    playback_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct PlayAudioCueInput {
    #[schemars(description = "Startup-approved audio cue identifier returned by list_audio_cues")]
    cue_id: String,
    #[schemars(
        description = "Omit for the cue default; otherwise use a normalized gain within the advertised policy range"
    )]
    gain: Option<f64>,
    #[schemars(
        description = "Omit for the configured default; enqueue waits behind active playback, interrupt stops all active items before starting this one, and mix overlaps up to the advertised stream limit"
    )]
    concurrency: Option<PolicyConcurrency>,
    #[schemars(
        description = "Omit for the configured default; otherwise use a startup-approved output alias from get_audio_capabilities"
    )]
    output_target: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SpeakTextInput {
    #[schemars(
        description = "Exact plain text to speak audibly within the advertised character limit; SSML is not interpreted"
    )]
    text: String,
    #[schemars(
        description = "Provider-defined per-utterance controls explicitly granted at startup"
    )]
    utterance_options: Option<BTreeMap<String, serde_json::Value>>,
    #[schemars(
        description = "Omit for the configured default; otherwise use a normalized gain within the advertised policy range"
    )]
    gain: Option<f64>,
    #[schemars(
        description = "Omit for the configured default; enqueue waits behind active playback, interrupt stops all active items before starting this one, and mix overlaps up to the advertised stream limit"
    )]
    concurrency: Option<PolicyConcurrency>,
    #[schemars(
        description = "Omit for the configured default; otherwise use a startup-approved output alias from get_audio_capabilities"
    )]
    output_target: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct PlaySourceInput {
    #[schemars(description = "Absolute path to the local regular audio file the user requested")]
    path: PathBuf,
    #[schemars(
        description = "Omit for the configured default; otherwise use a normalized gain within the advertised policy range"
    )]
    gain: Option<f64>,
    #[schemars(
        description = "Omit for the configured default; enqueue waits behind active playback, interrupt stops all active items before starting this one, and mix overlaps up to the advertised stream limit"
    )]
    concurrency: Option<PolicyConcurrency>,
    #[schemars(
        description = "Omit for the configured default; otherwise use a startup-approved output alias from get_audio_capabilities"
    )]
    output_target: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct AcceptanceOutput {
    playback_id: String,
    status: &'static str,
    accepted_at: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct PlaybackStatusOutput {
    playback_id: String,
    status: &'static str,
    terminal: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<&'static str>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct CancellationOutput {
    playback_id: String,
    status: &'static str,
    terminal: bool,
    cancelled: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
struct AudioCueListOutput {
    audio_cues: Vec<crate::config::AudioCueSummary>,
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

fn parse_playback_id(value: &str) -> Result<Uuid, ToolFailure> {
    Uuid::parse_str(value).map_err(|_| {
        ToolFailure::new(
            "invalid_playback_id",
            "playback_id must be a valid UUID returned by a playback tool",
            false,
        )
    })
}

fn unknown_playback_id() -> ToolFailure {
    ToolFailure::new(
        "unknown_playback_id",
        "playback ID is not known to this server or is no longer retained",
        false,
    )
}

/// Perform decoder and duration checks shared by `validate` and server startup
/// without opening an output device.
pub fn preflight_config_media(profile: &ProfileConfig) -> Result<(), ServerStartupError> {
    for cue in &profile.audio_cues {
        if cue.kind != AudioCueKind::AudioFile {
            continue;
        }
        let Some(path) = cue.source.as_ref() else {
            continue;
        };
        prepare_audio_path(path, duration_limit(profile)).map_err(|source| {
            ServerStartupError::AudioCuePreflight {
                cue_id: cue.id.clone(),
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
        ConcurrencyMode::Mix => "mix",
    }
}

fn playback_state_name(state: PlaybackState) -> &'static str {
    match state {
        PlaybackState::Accepted => "accepted",
        PlaybackState::Playing => "playing",
        PlaybackState::Completed => "completed",
        PlaybackState::Interrupted => "interrupted",
        PlaybackState::Failed => "failed",
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

        fn stop(&mut self, _playback_id: Uuid) -> Result<(), PlaybackError> {
            Ok(())
        }
    }

    struct FailingBackend;

    impl PlaybackBackend for FailingBackend {
        fn start(
            &mut self,
            _job: PlaybackJob,
            completion: CompletionNotifier,
        ) -> Result<(), PlaybackError> {
            completion.fail("provider secret diagnostic");
            Ok(())
        }

        fn stop(&mut self, _playback_id: Uuid) -> Result<(), PlaybackError> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct HoldingBackend {
        completion: Option<CompletionNotifier>,
    }

    impl PlaybackBackend for HoldingBackend {
        fn start(
            &mut self,
            _job: PlaybackJob,
            completion: CompletionNotifier,
        ) -> Result<(), PlaybackError> {
            self.completion = Some(completion);
            Ok(())
        }

        fn stop(&mut self, _playback_id: Uuid) -> Result<(), PlaybackError> {
            self.completion.take();
            Ok(())
        }
    }

    #[derive(Default)]
    struct StopFailingBackend {
        completion: Option<CompletionNotifier>,
    }

    impl PlaybackBackend for StopFailingBackend {
        fn start(
            &mut self,
            _job: PlaybackJob,
            completion: CompletionNotifier,
        ) -> Result<(), PlaybackError> {
            self.completion = Some(completion);
            Ok(())
        }

        fn stop(&mut self, _playback_id: Uuid) -> Result<(), PlaybackError> {
            Err(PlaybackError::Backend(
                "private backend cancellation diagnostic".to_owned(),
            ))
        }
    }

    fn quick_server() -> AgentSpeakServer {
        let config = quick_profile(QuickProfileOverrides::default()).unwrap();
        let playback = PlaybackHandle::spawn(16, || Ok(NoopBackend)).unwrap();
        AgentSpeakServer::from_parts(config, playback, None)
    }

    fn profile_server(source: &str, base: &Path) -> AgentSpeakServer {
        let config = parse_config(source, base, ConfigOrigin::QuickProfile).unwrap();
        let playback = PlaybackHandle::spawn(16, || Ok(NoopBackend)).unwrap();
        AgentSpeakServer::from_parts(config, playback, None)
    }

    fn profile_source(arbitrary_text: bool, arbitrary_audio: bool, cue: bool) -> String {
        let cue = if cue {
            r#"
[[audio_cues]]
id = "attention"
kind = "speech"
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
maximum_mix_streams = 2
maximum_audio_seconds = 0

{SYSTEM_OUTPUTS}

[tts]
enabled = true
backend = "system"
voice_id = ""
maximum_characters = 300

[logging]
level = "warning"
history_enabled = false
history_include_spoken_text = false
{cue}
"#
        )
    }

    fn utterance_schema() -> serde_json::Value {
        json!({
            "$schema":"https://json-schema.org/draft/2020-12/schema",
            "type":"object",
            "additionalProperties":false,
            "maxProperties":1,
            "properties":{
                "speed":{
                    "type":"number",
                    "minimum":0.5,
                    "maximum":2.0,
                    "title":"Speaking speed",
                    "description":"Relative speaking speed for this utterance.",
                    "x-utterpipe":{
                        "default_behavior":"Omission uses configured speed.",
                        "use_when":"Use for deliberately faster or slower delivery.",
                        "omit_when":"Omit when configured speed is suitable.",
                        "unit":"ratio"
                    }
                }
            }
        })
    }

    #[test]
    fn quick_profile_exposes_only_capabilities_and_speech() {
        assert_eq!(
            quick_server().registered_tool_names(),
            vec![
                "cancel_playback",
                "get_audio_capabilities",
                "get_playback_status",
                "speak_text"
            ]
        );
    }

    #[test]
    fn profile_matrix_shapes_the_complete_tool_surface() {
        let directory = tempfile::tempdir().unwrap();
        let all = profile_server(&profile_source(true, true, true), directory.path());
        assert_eq!(
            all.registered_tool_names(),
            vec![
                "cancel_playback",
                "get_audio_capabilities",
                "get_playback_status",
                "list_audio_cues",
                "play_audio_cue",
                "play_audio_source",
                "speak_text",
            ]
        );

        let inspection_only =
            profile_server(&profile_source(false, false, false), directory.path());
        assert_eq!(
            inspection_only.registered_tool_names(),
            vec![
                "cancel_playback",
                "get_audio_capabilities",
                "get_playback_status"
            ]
        );
    }

    #[test]
    fn mix_policy_maps_to_the_actor_and_advertises_its_stream_limit() {
        let directory = tempfile::tempdir().unwrap();
        let source = profile_source(true, false, false)
            .replace(
                "default_concurrency = \"enqueue\"",
                "default_concurrency = \"mix\"",
            )
            .replace(
                "allowed_concurrency = [\"enqueue\", \"interrupt\"]",
                "allowed_concurrency = [\"enqueue\", \"interrupt\", \"mix\"]",
            )
            .replace("maximum_mix_streams = 2", "maximum_mix_streams = 3");
        let server = profile_server(&source, directory.path());

        assert_eq!(server.capabilities.playback.maximum_mix_streams, 3);
        assert_eq!(
            server.resolve_playback_options(None, None, 0.4).unwrap().1,
            ConcurrencyMode::Mix
        );
        assert_eq!(
            server
                .resolve_playback_options(Some(0.3), Some(PolicyConcurrency::Mix), 0.4)
                .unwrap(),
            (0.3, ConcurrencyMode::Mix)
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
    async fn playback_status_is_read_only_bounded_and_sanitized() {
        let server = quick_server();
        let accepted = server
            .speak_text(Parameters(SpeakTextInput {
                text: "status secret marker".to_owned(),
                utterance_options: None,
                gain: None,
                concurrency: None,
                output_target: None,
            }))
            .await;
        let playback_id = accepted.structured_content.unwrap()["playback_id"]
            .as_str()
            .unwrap()
            .to_owned();

        let terminal = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let status = server
                    .get_playback_status(Parameters(PlaybackIdInput {
                        playback_id: playback_id.clone(),
                    }))
                    .await;
                let output = status.structured_content.unwrap();
                if output["terminal"] == true {
                    break output;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("playback status did not reach a terminal state");
        assert_eq!(terminal["playback_id"], playback_id);
        assert_eq!(terminal["status"], "completed");
        assert_eq!(terminal["terminal"], true);
        assert!(terminal.get("error_code").is_none());
        let serialized = terminal.to_string();
        assert!(!serialized.contains("status secret marker"));
        assert!(!serialized.contains("path"));

        let invalid = server
            .get_playback_status(Parameters(PlaybackIdInput {
                playback_id: "not-a-uuid".to_owned(),
            }))
            .await;
        assert_eq!(invalid.is_error, Some(true));
        assert_eq!(
            invalid.structured_content.unwrap()["error"]["code"],
            "invalid_playback_id"
        );

        let unknown = server
            .get_playback_status(Parameters(PlaybackIdInput {
                playback_id: Uuid::new_v4().to_string(),
            }))
            .await;
        assert_eq!(unknown.is_error, Some(true));
        assert_eq!(
            unknown.structured_content.unwrap()["error"]["code"],
            "unknown_playback_id"
        );
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_stops_active_playback_and_is_idempotent() {
        let config = quick_profile(QuickProfileOverrides::default()).unwrap();
        let playback = PlaybackHandle::spawn(16, || Ok(HoldingBackend::default())).unwrap();
        let server = AgentSpeakServer::from_parts(config, playback, None);
        let accepted = server
            .speak_text(Parameters(SpeakTextInput {
                text: "cancellation privacy marker".to_owned(),
                utterance_options: None,
                gain: None,
                concurrency: None,
                output_target: None,
            }))
            .await;
        let playback_id = accepted.structured_content.unwrap()["playback_id"]
            .as_str()
            .unwrap()
            .to_owned();

        let cancelled = server
            .cancel_playback(Parameters(PlaybackIdInput {
                playback_id: playback_id.clone(),
            }))
            .await;
        assert_eq!(cancelled.is_error, Some(false));
        let output = cancelled.structured_content.unwrap();
        assert_eq!(output["playback_id"], playback_id);
        assert_eq!(output["status"], "interrupted");
        assert_eq!(output["terminal"], true);
        assert_eq!(output["cancelled"], true);
        assert!(!output.to_string().contains("cancellation privacy marker"));

        let repeated = server
            .cancel_playback(Parameters(PlaybackIdInput {
                playback_id: playback_id.clone(),
            }))
            .await
            .structured_content
            .unwrap();
        assert_eq!(repeated["status"], "interrupted");
        assert_eq!(repeated["cancelled"], false);

        let status = server
            .get_playback_status(Parameters(PlaybackIdInput { playback_id }))
            .await
            .structured_content
            .unwrap();
        assert_eq!(status["status"], "interrupted");

        for invalid_id in ["not-a-uuid".to_owned(), Uuid::new_v4().to_string()] {
            let rejected = server
                .cancel_playback(Parameters(PlaybackIdInput {
                    playback_id: invalid_id,
                }))
                .await;
            assert_eq!(rejected.is_error, Some(true));
            let code = rejected.structured_content.unwrap()["error"]["code"]
                .as_str()
                .unwrap()
                .to_owned();
            assert!(matches!(
                code.as_str(),
                "invalid_playback_id" | "unknown_playback_id"
            ));
        }
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_failure_is_sanitized_and_preserves_observed_state() {
        let config = quick_profile(QuickProfileOverrides::default()).unwrap();
        let playback = PlaybackHandle::spawn(16, || Ok(StopFailingBackend::default())).unwrap();
        let server = AgentSpeakServer::from_parts(config, playback, None);
        let accepted = server
            .speak_text(Parameters(SpeakTextInput {
                text: "private spoken text".to_owned(),
                utterance_options: None,
                gain: None,
                concurrency: None,
                output_target: None,
            }))
            .await;
        let playback_id = accepted.structured_content.unwrap()["playback_id"]
            .as_str()
            .unwrap()
            .to_owned();

        let rejected = server
            .cancel_playback(Parameters(PlaybackIdInput {
                playback_id: playback_id.clone(),
            }))
            .await;
        assert_eq!(rejected.is_error, Some(true));
        let output = rejected.structured_content.unwrap();
        assert_eq!(output["error"]["code"], "playback_unavailable");
        let serialized = output.to_string();
        assert!(!serialized.contains("private backend cancellation diagnostic"));
        assert!(!serialized.contains("private spoken text"));

        let status = server
            .get_playback_status(Parameters(PlaybackIdInput { playback_id }))
            .await
            .structured_content
            .unwrap();
        assert_eq!(status["status"], "playing");
        assert_eq!(status["terminal"], false);
        assert!(server.shutdown().await.is_err());
    }

    #[tokio::test]
    async fn failed_playback_status_exposes_only_a_stable_error_code() {
        let config = quick_profile(QuickProfileOverrides::default()).unwrap();
        let playback = PlaybackHandle::spawn(16, || Ok(FailingBackend)).unwrap();
        let server = AgentSpeakServer::from_parts(config, playback, None);
        let accepted = server
            .speak_text(Parameters(SpeakTextInput {
                text: "private spoken text".to_owned(),
                utterance_options: None,
                gain: None,
                concurrency: None,
                output_target: None,
            }))
            .await;
        let playback_id = accepted.structured_content.unwrap()["playback_id"]
            .as_str()
            .unwrap()
            .to_owned();

        let terminal = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let status = server
                    .get_playback_status(Parameters(PlaybackIdInput {
                        playback_id: playback_id.clone(),
                    }))
                    .await
                    .structured_content
                    .unwrap();
                if status["terminal"] == true {
                    break status;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("failed playback status did not become terminal");
        assert_eq!(terminal["status"], "failed");
        assert_eq!(terminal["error_code"], "playback_unavailable");
        let serialized = terminal.to_string();
        assert!(!serialized.contains("provider secret diagnostic"));
        assert!(!serialized.contains("private spoken text"));
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cue_audio_bytes_are_repreflighted_on_every_call() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mutable.wav");
        fs::write(&path, silent_wav()).unwrap();
        let source = format!(
            "{}\n[[audio_cues]]\nid = \"mutable\"\nkind = \"audio_file\"\nsource = \"mutable.wav\"\ndescription = \"\"\ndefault_gain = 0.4\n",
            profile_source(false, false, false)
        );
        let server = profile_server(&source, directory.path());

        fs::write(&path, b"MZ untrusted non-audio payload").unwrap();
        let rejected = server
            .play_audio_cue(Parameters(PlayAudioCueInput {
                cue_id: "mutable".into(),
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
            .play_audio_cue(Parameters(PlayAudioCueInput {
                cue_id: "mutable".into(),
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
        let mut server = AgentSpeakServer::from_parts(config, playback, None);
        server.history = Some(history);

        let result = server
            .speak_text(Parameters(SpeakTextInput {
                text: "integration secret marker".to_owned(),
                utterance_options: None,
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
        assert_eq!(
            client.peer_info().unwrap().instructions.as_deref(),
            Some(SERVER_INSTRUCTIONS)
        );
        assert!(SERVER_INSTRUCTIONS.contains("Use get_playback_status"));
        assert!(SERVER_INSTRUCTIONS.contains("Use cancel_playback"));

        let listed = client.list_tools(None).await.unwrap();
        let names: Vec<_> = listed.tools.iter().map(|tool| tool.name.as_ref()).collect();
        assert_eq!(
            names,
            [
                "cancel_playback",
                "get_audio_capabilities",
                "get_playback_status",
                "speak_text"
            ]
        );
        assert!(listed.tools.iter().all(|tool| tool.output_schema.is_some()));
        let capability_schema = listed
            .tools
            .iter()
            .find(|tool| tool.name == "get_audio_capabilities")
            .unwrap();
        let capability_annotations = capability_schema.annotations.as_ref().unwrap();
        assert_eq!(capability_annotations.read_only_hint, Some(true));
        assert_eq!(capability_annotations.idempotent_hint, Some(true));
        assert_eq!(capability_annotations.open_world_hint, Some(false));
        let status_schema = listed
            .tools
            .iter()
            .find(|tool| tool.name == "get_playback_status")
            .unwrap();
        let status_annotations = status_schema.annotations.as_ref().unwrap();
        assert_eq!(status_annotations.read_only_hint, Some(true));
        assert_eq!(status_annotations.idempotent_hint, Some(true));
        assert_eq!(status_annotations.open_world_hint, Some(false));
        assert!(status_schema.input_schema["properties"]["playback_id"].is_object());
        let cancel_schema = listed
            .tools
            .iter()
            .find(|tool| tool.name == "cancel_playback")
            .unwrap();
        let cancel_annotations = cancel_schema.annotations.as_ref().unwrap();
        assert_eq!(cancel_annotations.read_only_hint, Some(false));
        assert_eq!(cancel_annotations.destructive_hint, Some(true));
        assert_eq!(cancel_annotations.idempotent_hint, Some(true));
        assert_eq!(cancel_annotations.open_world_hint, Some(false));
        assert!(cancel_schema.input_schema["properties"]["playback_id"].is_object());
        let speak_schema = listed
            .tools
            .iter()
            .find(|tool| tool.name == "speak_text")
            .unwrap();
        assert!(
            speak_schema
                .description
                .as_deref()
                .unwrap()
                .contains("non-idempotent")
        );
        let speak_annotations = speak_schema.annotations.as_ref().unwrap();
        assert_eq!(speak_annotations.read_only_hint, Some(false));
        assert_eq!(speak_annotations.destructive_hint, Some(false));
        assert_eq!(speak_annotations.idempotent_hint, Some(false));
        assert_eq!(speak_annotations.open_world_hint, Some(true));
        assert!(speak_schema.input_schema["properties"]["output_target"].is_object());
        assert!(
            speak_schema.input_schema["properties"]["concurrency"]["description"]
                .as_str()
                .unwrap()
                .contains("mix overlaps up to the advertised stream limit")
        );

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

        let accepted = client
            .call_tool(
                CallToolRequestParams::new("speak_text").with_arguments(
                    json!({ "text": "Status route check." })
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await
            .unwrap();
        let playback_id = accepted.structured_content.unwrap()["playback_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let terminal = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let arguments = json!({ "playback_id": playback_id })
                    .as_object()
                    .unwrap()
                    .clone();
                let status = client
                    .call_tool(
                        CallToolRequestParams::new("get_playback_status").with_arguments(arguments),
                    )
                    .await
                    .unwrap()
                    .structured_content
                    .unwrap();
                if status["terminal"] == true {
                    break status;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("MCP playback status route did not become terminal");
        assert_eq!(terminal["status"], "completed");

        let arguments = json!({ "playback_id": playback_id })
            .as_object()
            .unwrap()
            .clone();
        let cancellation = client
            .call_tool(CallToolRequestParams::new("cancel_playback").with_arguments(arguments))
            .await
            .unwrap()
            .structured_content
            .unwrap();
        assert_eq!(cancellation["status"], "completed");
        assert_eq!(cancellation["terminal"], true);
        assert_eq!(cancellation["cancelled"], false);

        client.cancel().await.unwrap();
        server_task.await.unwrap();
        control.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn mcp_projects_and_enforces_the_startup_utterance_schema() {
        let directory = tempfile::tempdir().unwrap();
        let source = profile_source(true, false, false).replace(
            "backend = \"system\"\nvoice_id = \"\"\nmaximum_characters = 300",
            "backend = \"utterpipe-fake\"\nmaximum_characters = 300\nagent_utterance_options = [\"speed\"]",
        );
        let config = parse_config(&source, directory.path(), ConfigOrigin::QuickProfile).unwrap();
        let playback = PlaybackHandle::spawn(16, || Ok(NoopBackend)).unwrap();
        let server = AgentSpeakServer::from_parts(config, playback, Some(utterance_schema()));
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
        let speak = listed
            .tools
            .iter()
            .find(|tool| tool.name == "speak_text")
            .unwrap();
        let options = &speak.input_schema["properties"]["utterance_options"];
        assert_eq!(options["additionalProperties"], false);
        assert_eq!(options["properties"]["speed"]["maximum"], 2.0);
        assert!(options["properties"].get("voice").is_none());

        let invalid = json!({
            "text":"Too fast.",
            "utterance_options":{"speed":3.0}
        })
        .as_object()
        .unwrap()
        .clone();
        let rejected = client
            .call_tool(CallToolRequestParams::new("speak_text").with_arguments(invalid))
            .await
            .unwrap();
        assert_eq!(rejected.is_error, Some(true));
        assert_eq!(
            rejected.structured_content.unwrap()["error"]["code"],
            "invalid_utterance_options"
        );

        let valid = json!({
            "text":"A little faster.",
            "utterance_options":{"speed":1.25}
        })
        .as_object()
        .unwrap()
        .clone();
        let accepted = client
            .call_tool(CallToolRequestParams::new("speak_text").with_arguments(valid))
            .await
            .unwrap();
        assert_eq!(accepted.is_error, Some(false));

        client.cancel().await.unwrap();
        server_task.await.unwrap();
        control.shutdown().await.unwrap();
    }
}
