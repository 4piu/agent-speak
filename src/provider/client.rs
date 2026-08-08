use std::{
    collections::{BTreeMap, HashSet},
    env,
    ffi::OsString,
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{Arc, Mutex, RwLock},
    thread,
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, RecvTimeoutError};
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{self, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Value, json};
use thiserror::Error;

#[cfg(windows)]
use std::os::windows::io::AsRawHandle;

#[cfg(windows)]
use windows::{
    Win32::{
        Foundation::HANDLE,
        System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject, TerminateJobObject,
        },
    },
    core::{Owned, PCWSTR},
};

use crate::config::TtsConfig;

use super::{
    MAX_AUDIO_BYTES, MAX_CONTROL_BYTES, MAX_INCREMENTAL_AUDIO_FRAME_BYTES, MAX_STDERR_BYTES,
    SHUTDOWN_GRACE_MS,
};

const MAGIC: &[u8; 4] = b"UTP1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionKind {
    Inspect,
    Runtime,
    Management,
}

impl SessionKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Inspect => "inspect",
            Self::Runtime => "runtime",
            Self::Management => "management",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeliveryMode {
    Complete,
    Incremental,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AudioDelivery {
    pub mode: DeliveryMode,
    pub format: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ProviderIdentity {
    pub slug: String,
    pub name: String,
    pub vendor: String,
    pub version: String,
}

#[derive(Clone, Debug)]
pub struct Capabilities {
    pub synthesis: bool,
    pub cancellation: bool,
    pub catalog: bool,
    pub prepare: bool,
    pub remove: bool,
    pub asset_import: bool,
}

impl Capabilities {
    fn from_names(names: &[String]) -> Self {
        let has = |name: &str| names.iter().any(|item| item == name);
        Self {
            synthesis: has("synthesis"),
            cancellation: has("synthesis.cancel"),
            catalog: has("catalog"),
            prepare: has("prepare"),
            remove: has("remove"),
            asset_import: has("asset.import"),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct CatalogDescriptor {
    pub id: String,
    pub name: String,
    pub description: String,
    pub item_kind: String,
    pub patchable_provider_options: Vec<String>,
    pub patchable_utterance_options: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ImportKindDescriptor {
    pub id: String,
    pub name: String,
    pub media_types: Vec<String>,
    pub max_source_bytes: u64,
    pub patchable_provider_options: Vec<String>,
    pub patchable_utterance_options: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ProviderInfo {
    pub executable: PathBuf,
    pub protocol_version: u64,
    pub provider: ProviderIdentity,
    pub capabilities: Capabilities,
    pub audio_deliveries: Vec<AudioDelivery>,
    pub provider_options_schema: Value,
    pub catalogs: Vec<CatalogDescriptor>,
    pub import_kinds: Vec<ImportKindDescriptor>,
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("provider discovery failed: {0}")]
    Discovery(String),
    #[error("provider process failed: {0}")]
    Process(String),
    #[error("provider protocol error: {0}")]
    Protocol(String),
    #[error("provider request failed ({code}): {message}")]
    Remote { code: String, message: String },
    #[error("provider operation timed out")]
    Timeout,
    #[error("provider configuration is invalid: {0}")]
    Configuration(String),
}

impl From<io::Error> for ProviderError {
    fn from(value: io::Error) -> Self {
        Self::Process(value.to_string())
    }
}

#[derive(Debug)]
pub(crate) enum Frame {
    Control(Value),
    Audio(Vec<u8>),
}

#[derive(Deserialize)]
struct HelloResult {
    protocol: String,
    version: u64,
    framing: String,
    provider: ProviderIdentity,
    capabilities: Vec<String>,
    audio_deliveries: Vec<Value>,
    utterance_schema_profile: String,
    provider_options_schema: Value,
    management_options_schema: Value,
    catalogs: Vec<CatalogDescriptor>,
    import_kinds: Vec<ImportKindDescriptor>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RuntimeInitialization {
    pub audio_deliveries: Vec<AudioDelivery>,
    pub utterance_options_schema: Value,
    pub utterance_options_schema_digest: String,
    pub configured_utterance_options: Map<String, Value>,
}

pub(crate) struct Client {
    pub info: ProviderInfo,
    pub session: SessionKind,
    writer: Option<Arc<Mutex<ChildStdin>>>,
    pub frames: Receiver<Result<Frame, ProviderError>>,
    pub runtime_initialization: Option<RuntimeInitialization>,
    audio_frame_limit: Arc<RwLock<usize>>,
    child: Child,
    #[cfg(windows)]
    job: WindowsJobObject,
    next_id: u64,
    _stderr: Arc<Mutex<Vec<u8>>>,
}

impl Client {
    pub fn spawn(
        executable: &Path,
        slug: &str,
        session: SessionKind,
        provider_environment: &[String],
    ) -> Result<Self, ProviderError> {
        let mut command = Command::new(executable);
        command
            .args(["protocol", "--stdio"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear();
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // A dedicated process group lets cancellation clean up provider-created
            // descendants without involving a shell.
            command.process_group(0);
        }
        for (key, value) in baseline_environment(provider_environment)? {
            command.env(key, value);
        }
        #[cfg(windows)]
        let job = WindowsJobObject::new().map_err(|error| {
            ProviderError::Process(format!(
                "provider kill-on-close Windows Job Object could not be created; refusing to spawn without descendant cleanup: {error}"
            ))
        })?;
        let child = command.spawn().map_err(|source| {
            ProviderError::Process(format!(
                "could not start '{}': {source}",
                executable.display()
            ))
        })?;
        #[cfg(windows)]
        let mut child = child;
        #[cfg(windows)]
        job.assign(&mut child).map_err(|error| {
            let _ = child.kill();
            let _ = child.wait();
            ProviderError::Process(format!(
                "provider process could not be assigned to a kill-on-close Windows Job Object; refusing to start without descendant cleanup: {error}"
            ))
        })?;
        #[cfg(windows)]
        let mut guard = SpawnGuard::new(child, job);
        #[cfg(not(windows))]
        let mut guard = SpawnGuard::new(child);

        let stdin = guard
            .child_mut()
            .stdin
            .take()
            .ok_or_else(|| ProviderError::Process("provider stdin was not piped".into()))?;
        let mut stdout = guard
            .child_mut()
            .stdout
            .take()
            .ok_or_else(|| ProviderError::Process("provider stdout was not piped".into()))?;
        let mut child_stderr = guard
            .child_mut()
            .stderr
            .take()
            .ok_or_else(|| ProviderError::Process("provider stderr was not piped".into()))?;

        let (frame_tx, frame_rx) = crossbeam_channel::bounded(8);
        let audio_frame_limit = Arc::new(RwLock::new(MAX_AUDIO_BYTES as usize));
        let reader_audio_frame_limit = audio_frame_limit.clone();
        thread::Builder::new()
            .name(format!("utterpipe-{slug}-stdout"))
            .spawn(move || {
                loop {
                    match read_frame(&mut stdout, &reader_audio_frame_limit) {
                        Ok(Some(frame)) => {
                            if frame_tx.send(Ok(frame)).is_err() {
                                break;
                            }
                        }
                        Ok(None) => {
                            let _ = frame_tx
                                .send(Err(ProviderError::Process("provider closed stdout".into())));
                            break;
                        }
                        Err(error) => {
                            let _ = frame_tx.send(Err(error));
                            break;
                        }
                    }
                }
            })
            .map_err(|error| ProviderError::Process(error.to_string()))?;

        let stderr = Arc::new(Mutex::new(Vec::new()));
        let retained = stderr.clone();
        thread::Builder::new()
            .name(format!("utterpipe-{slug}-stderr"))
            .spawn(move || {
                let mut buffer = [0_u8; 8192];
                while let Ok(read) = child_stderr.read(&mut buffer) {
                    if read == 0 {
                        break;
                    }
                    let mut output = retained
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    let available = MAX_STDERR_BYTES.saturating_sub(output.len());
                    output.extend_from_slice(&buffer[..read.min(available)]);
                }
            })
            .map_err(|error| ProviderError::Process(error.to_string()))?;

        let writer = Arc::new(Mutex::new(stdin));
        let hello_id = "hello";
        write_request(
            &writer,
            hello_id,
            "protocol.hello",
            json!({
                "protocol": "utterpipe.tts",
                "versions": [1],
                "expected_provider": slug,
                "session": session.as_str(),
                "utterance_schema_profiles": ["utterpipe.utterance-options/1"],
                "host": {"name": "agent-speak", "version": env!("CARGO_PKG_VERSION")}
            }),
        )?;
        let result = wait_response(&frame_rx, hello_id, Duration::from_secs(5), None)?.0;
        let hello: HelloResult = serde_json::from_value(result)
            .map_err(|error| ProviderError::Protocol(format!("invalid hello result: {error}")))?;
        let audio_deliveries = validate_hello(&hello, slug)?;
        let info = ProviderInfo {
            executable: executable.to_owned(),
            protocol_version: hello.version,
            provider: hello.provider,
            capabilities: Capabilities::from_names(&hello.capabilities),
            audio_deliveries,
            provider_options_schema: hello.provider_options_schema,
            catalogs: hello.catalogs,
            import_kinds: hello.import_kinds,
        };
        let spawned = guard.disarm();
        Ok(Self {
            info,
            session,
            writer: Some(writer),
            frames: frame_rx,
            runtime_initialization: None,
            audio_frame_limit,
            child: spawned.child,
            #[cfg(windows)]
            job: spawned.job,
            next_id: 1,
            _stderr: stderr,
        })
    }

    pub fn initialize(
        &mut self,
        tts: &TtsConfig,
        data: &Path,
        cache: &Path,
    ) -> Result<Option<RuntimeInitialization>, ProviderError> {
        let provider = tts.utterpipe().ok_or_else(|| {
            ProviderError::Configuration(
                "UtterPipe initialization requires an external backend".into(),
            )
        })?;
        let provider_options = toml_table_to_json(&provider.provider_options)?;
        let id = self.next_request_id();
        let mut params = json!({
            "data_dir": unicode_path(data, "provider data directory")?,
            "cache_dir": unicode_path(cache, "provider cache directory")?,
            "provider_options": provider_options,
        });
        let preferred = if provider.audio_deliveries.is_empty() {
            host_audio_deliveries()
        } else {
            provider
                .audio_deliveries
                .iter()
                .map(|delivery| AudioDelivery {
                    mode: match delivery.mode.as_str() {
                        "complete" => DeliveryMode::Complete,
                        "incremental" => DeliveryMode::Incremental,
                        _ => unreachable!("configuration was validated"),
                    },
                    format: delivery.format.clone(),
                })
                .collect()
        };
        let offered: Vec<_> = preferred
            .into_iter()
            .filter(|delivery| self.info.audio_deliveries.contains(delivery))
            .collect();
        if self.session == SessionKind::Runtime {
            if offered.is_empty() {
                return Err(ProviderError::Configuration(
                    "provider and Agent Speak have no compatible audio delivery".into(),
                ));
            }
            params["limits"] = json!({
                "max_text_code_points": tts.maximum_characters,
                "max_audio_bytes": MAX_AUDIO_BYTES,
                "synthesis_timeout_ms": super::SYNTHESIS_TIMEOUT_MS,
            });
            params["accepted_audio_deliveries"] = json!(offered);
        }
        let result =
            self.call_with_id(&id, "session.initialize", params, Duration::from_secs(120))?;
        if result.get("ready").and_then(Value::as_bool) != Some(true) {
            return Err(ProviderError::Protocol(
                "initialize did not return ready=true".into(),
            ));
        }
        if self.session == SessionKind::Management {
            return Ok(None);
        }
        let audio_deliveries: Vec<AudioDelivery> =
            serde_json::from_value(result.get("audio_deliveries").cloned().ok_or_else(|| {
                ProviderError::Protocol("initialize omitted audio_deliveries".into())
            })?)
            .map_err(|_| {
                ProviderError::Protocol("initialize returned invalid audio_deliveries".into())
            })?;
        if audio_deliveries.is_empty()
            || audio_deliveries.len() > offered.len()
            || has_duplicates(&audio_deliveries)
            || !order_preserving_subset(&audio_deliveries, &offered)
        {
            return Err(ProviderError::Protocol(
                "initialize returned an invalid or reordered audio delivery set".into(),
            ));
        }
        let schema = result
            .get("utterance_options_schema")
            .cloned()
            .ok_or_else(|| {
                ProviderError::Protocol("initialize omitted utterance_options_schema".into())
            })?;
        let digest = result
            .get("utterance_options_schema_digest")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ProviderError::Protocol("initialize omitted utterance_options_schema_digest".into())
            })?
            .to_owned();
        super::schema::validate_schema_and_digest(&schema, &digest)
            .map_err(ProviderError::Protocol)?;
        let provider_properties = self.info.provider_options_schema["properties"]
            .as_object()
            .expect("provider schema was validated");
        let utterance_properties = schema["properties"]
            .as_object()
            .expect("utterance schema was validated");
        if utterance_properties
            .keys()
            .any(|name| provider_properties.contains_key(name))
        {
            return Err(ProviderError::Protocol(
                "provider and utterance option schemas contain overlapping top-level names".into(),
            ));
        }
        let configured_utterance_options = toml_table_to_json(&provider.utterance_options)?
            .as_object()
            .cloned()
            .expect("a TOML table converts to a JSON object");
        super::schema::validate_request(&configured_utterance_options, Some(&schema))
            .map_err(ProviderError::Configuration)?;
        super::schema::project_allowed_properties(&schema, &provider.agent_utterance_options)
            .map_err(ProviderError::Configuration)?;

        let initialized = RuntimeInitialization {
            audio_deliveries,
            utterance_options_schema: schema,
            utterance_options_schema_digest: digest,
            configured_utterance_options,
        };
        self.runtime_initialization = Some(initialized.clone());
        Ok(Some(initialized))
    }

    pub(crate) fn select_audio_delivery(&self, delivery: &AudioDelivery) {
        *self
            .audio_frame_limit
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            audio_frame_limit_for(delivery.mode);
    }

    pub fn call(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, ProviderError> {
        let id = self.next_request_id();
        self.call_with_id(&id, method, params, timeout)
    }

    pub fn call_with_id(
        &mut self,
        id: &str,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, ProviderError> {
        write_request(self.writer()?, id, method, params)?;
        wait_response(&self.frames, id, timeout, None).map(|result| result.0)
    }

    pub(crate) fn writer(&self) -> Result<&Arc<Mutex<ChildStdin>>, ProviderError> {
        self.writer
            .as_ref()
            .ok_or_else(|| ProviderError::Process("provider stdin is already closed".into()))
    }

    pub fn call_with_events(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, ProviderError> {
        let id = self.next_request_id();
        let operation_id = params
            .get("operation_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ProviderError::Configuration(
                    "event-producing provider call requires operation_id".into(),
                )
            })?
            .to_owned();
        write_request(self.writer()?, &id, method, params)?;
        wait_response(&self.frames, &id, timeout, Some(&operation_id)).map(|result| result.0)
    }

    pub fn next_request_id(&mut self) -> String {
        let id = format!("req-{}", self.next_id);
        self.next_id += 1;
        id
    }

    pub fn shutdown(mut self) -> Result<(), ProviderError> {
        let result = self
            .call(
                "session.shutdown",
                json!({}),
                Duration::from_millis(SHUTDOWN_GRACE_MS),
            )
            .and_then(validate_shutdown_result);
        // The provider may use an uncancellable blocking stdin reader. Closing the
        // host's last pipe handle after the shutdown response lets that reader see
        // EOF so the provider runtime can exit cleanly.
        self.writer.take();
        let deadline = Instant::now() + Duration::from_millis(SHUTDOWN_GRACE_MS);
        loop {
            match self.child.try_wait()? {
                Some(status) if status.success() => break,
                Some(status) => {
                    return Err(ProviderError::Process(format!(
                        "provider exited unsuccessfully after shutdown: {status}"
                    )));
                }
                None if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                None => {
                    self.terminate_process_tree();
                    let _ = self.child.wait();
                    return Err(ProviderError::Process(
                        "provider did not exit after shutdown".into(),
                    ));
                }
            }
        }
        result.map(|_| ())
    }

    pub fn terminate(&mut self) {
        self.terminate_process_tree();
        let _ = self.child.wait();
    }

    fn terminate_process_tree(&mut self) {
        #[cfg(windows)]
        self.job.terminate();
        terminate_process_tree(&mut self.child);
    }
}

struct SpawnGuard {
    child: Option<Child>,
    #[cfg(windows)]
    job: Option<WindowsJobObject>,
}

struct SpawnedProvider {
    child: Child,
    #[cfg(windows)]
    job: WindowsJobObject,
}

impl SpawnGuard {
    #[cfg(not(windows))]
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    #[cfg(windows)]
    fn new(child: Child, job: WindowsJobObject) -> Self {
        Self {
            child: Some(child),
            job: Some(job),
        }
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child
            .as_mut()
            .expect("spawn guard always owns the provider child while armed")
    }

    fn disarm(mut self) -> SpawnedProvider {
        SpawnedProvider {
            child: self
                .child
                .take()
                .expect("spawn guard always owns the provider child while armed"),
            #[cfg(windows)]
            job: self
                .job
                .take()
                .expect("spawn guard always owns the provider job while armed"),
        }
    }
}

impl Drop for SpawnGuard {
    fn drop(&mut self) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        #[cfg(windows)]
        if let Some(job) = &self.job {
            job.terminate();
        }
        terminate_process_tree(child);
        let _ = child.wait();
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            self.terminate_process_tree();
            let _ = self.child.wait();
        }
    }
}

fn terminate_process_tree(child: &mut Child) {
    #[cfg(unix)]
    {
        let process_group = -(child.id() as i32);
        // SAFETY: The provider is spawned as leader of this dedicated process group.
        // A negative PID targets exactly that group; failure is handled by the direct
        // child kill fallback below.
        unsafe {
            libc::kill(process_group, libc::SIGKILL);
        }
    }
    // This is a final fallback for the direct child. On Windows the retained Job
    // Object is terminated before this call; on Unix the process group is killed.
    let _ = child.kill();
}

#[cfg(windows)]
struct WindowsJobObject {
    handle: Owned<HANDLE>,
}

#[cfg(windows)]
impl WindowsJobObject {
    fn assign(&self, child: &mut Child) -> windows::core::Result<()> {
        let process = HANDLE(child.as_raw_handle());
        // SAFETY: `process` is the live provider process handle owned by `child`,
        // and `self.handle` remains owned by Client for at least as long as child.
        unsafe {
            AssignProcessToJobObject(*self.handle, process)?;
        }
        Ok(())
    }

    fn new() -> windows::core::Result<Self> {
        // SAFETY: Null security attributes and name request a private job with
        // default security. Owned takes responsibility for the returned handle.
        let handle = unsafe { Owned::new(CreateJobObjectW(None, PCWSTR::null())?) };
        let limits = kill_on_close_job_limits();
        // SAFETY: The pointer and size describe `limits` exactly for the requested
        // JobObjectExtendedLimitInformation information class.
        unsafe {
            SetInformationJobObject(
                *handle,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )?;
        }
        Ok(Self { handle })
    }

    fn terminate(&self) {
        // SAFETY: The handle is a live Job Object owned by this value. Failure is
        // followed by the direct-child fallback and wait in the caller.
        let _ = unsafe { TerminateJobObject(*self.handle, 1) };
    }
}

#[cfg(windows)]
fn kill_on_close_job_limits() -> JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    limits
}

fn validate_hello(hello: &HelloResult, slug: &str) -> Result<Vec<AudioDelivery>, ProviderError> {
    if hello.protocol != "utterpipe.tts"
        || hello.version != 1
        || hello.framing != "UTP1"
        || hello.provider.slug != slug
        || hello.utterance_schema_profile != "utterpipe.utterance-options/1"
    {
        return Err(ProviderError::Protocol(
            "provider identity or protocol version does not match configuration".into(),
        ));
    }
    if hello.capabilities.len() > 64
        || !hello.capabilities.iter().any(|name| name == "synthesis")
        || has_duplicates(&hello.capabilities)
        || hello
            .capabilities
            .iter()
            .any(|name| !valid_capability_name(name))
    {
        return Err(ProviderError::Protocol(
            "provider advertised invalid capabilities".into(),
        ));
    }
    let audio_deliveries = known_audio_deliveries(&hello.audio_deliveries)?;
    if !audio_deliveries
        .iter()
        .any(|delivery| delivery.mode == DeliveryMode::Complete)
    {
        return Err(ProviderError::Protocol(
            "provider did not advertise a registered complete audio delivery".into(),
        ));
    }
    if [&hello.provider.name, &hello.provider.vendor]
        .into_iter()
        .any(|value| {
            value.is_empty() || value.chars().count() > 256 || value.chars().any(char::is_control)
        })
        || !valid_semver(&hello.provider.version)
    {
        return Err(ProviderError::Protocol(
            "provider identity contains an invalid name, vendor, or SemVer product version".into(),
        ));
    }
    validate_fixed_options_schema(&hello.provider_options_schema, false)?;
    validate_fixed_options_schema(&hello.management_options_schema, true)?;
    let has_catalog = hello.capabilities.iter().any(|name| name == "catalog");
    let has_import = hello.capabilities.iter().any(|name| name == "asset.import");
    let provider_properties = hello.provider_options_schema["properties"]
        .as_object()
        .expect("fixed options schema was validated");
    if hello.catalogs.len() > 32
        || hello.import_kinds.len() > 16
        || has_catalog != !hello.catalogs.is_empty()
        || has_import != !hello.import_kinds.is_empty()
        || has_duplicates_by(&hello.catalogs, |catalog| &catalog.id)
        || has_duplicates_by(&hello.import_kinds, |kind| &kind.id)
        || hello.catalogs.iter().any(|catalog| {
            !valid_descriptor_id(&catalog.id)
                || !valid_descriptor_id(&catalog.item_kind)
                || !valid_display_text(&catalog.name, 80)
                || !valid_display_text(&catalog.description, 512)
                || !valid_patchable_options(
                    &catalog.patchable_provider_options,
                    provider_properties,
                )
                || !valid_patchable_option_names(&catalog.patchable_utterance_options)
        })
        || hello.import_kinds.iter().any(|kind| {
            !valid_descriptor_id(&kind.id)
                || !valid_display_text(&kind.name, 80)
                || kind.media_types.is_empty()
                || kind.max_source_bytes == 0
                || has_duplicates(&kind.media_types)
                || kind.media_types.iter().any(|media_type| {
                    media_type.len() > 128
                        || !media_type.contains('/')
                        || media_type.chars().any(char::is_control)
                })
                || !valid_patchable_options(&kind.patchable_provider_options, provider_properties)
                || !valid_patchable_option_names(&kind.patchable_utterance_options)
        })
    {
        return Err(ProviderError::Protocol(
            "provider advertised invalid catalog or import descriptors".into(),
        ));
    }
    Ok(audio_deliveries)
}

fn known_audio_deliveries(values: &[Value]) -> Result<Vec<AudioDelivery>, ProviderError> {
    if values.is_empty() || values.len() > 32 {
        return Err(ProviderError::Protocol(
            "provider advertised an invalid number of audio deliveries".into(),
        ));
    }
    let mut seen = HashSet::new();
    let mut known = Vec::new();
    for value in values {
        let object = value.as_object().ok_or_else(|| {
            ProviderError::Protocol("audio delivery descriptor is not an object".into())
        })?;
        let mode = object
            .get("mode")
            .and_then(Value::as_str)
            .ok_or_else(|| ProviderError::Protocol("audio delivery mode is not a string".into()))?;
        let format = object
            .get("format")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ProviderError::Protocol("audio delivery format is not a string".into())
            })?;
        if mode.is_empty()
            || mode.len() > 32
            || mode.chars().any(char::is_control)
            || format.is_empty()
            || format.len() > 256
            || format.chars().any(char::is_control)
            || !seen.insert((mode.to_owned(), format.to_owned()))
        {
            return Err(ProviderError::Protocol(
                "provider advertised a malformed or duplicate audio delivery".into(),
            ));
        }
        let mode = match mode {
            "complete" => DeliveryMode::Complete,
            "incremental" => DeliveryMode::Incremental,
            _ => continue,
        };
        let delivery = AudioDelivery {
            mode,
            format: format.to_owned(),
        };
        if registered_audio_delivery(&delivery) {
            known.push(delivery);
        }
    }
    Ok(known)
}

fn host_audio_deliveries() -> Vec<AudioDelivery> {
    [
        (DeliveryMode::Incremental, "audio/ogg;codecs=opus"),
        (DeliveryMode::Incremental, "audio/mpeg"),
        (DeliveryMode::Incremental, "audio/pcm;codec=pcm_s16le"),
        (DeliveryMode::Complete, "audio/ogg;codecs=opus"),
        (DeliveryMode::Complete, "audio/mpeg"),
        (DeliveryMode::Complete, "audio/wav;codec=pcm_s16le"),
    ]
    .into_iter()
    .map(|(mode, format)| AudioDelivery {
        mode,
        format: format.into(),
    })
    .collect()
}

fn audio_frame_limit_for(mode: DeliveryMode) -> usize {
    match mode {
        DeliveryMode::Complete => MAX_AUDIO_BYTES as usize,
        DeliveryMode::Incremental => MAX_INCREMENTAL_AUDIO_FRAME_BYTES,
    }
}

fn registered_audio_delivery(delivery: &AudioDelivery) -> bool {
    matches!(
        (delivery.mode, delivery.format.as_str()),
        (DeliveryMode::Complete, "audio/wav;codec=pcm_s16le")
            | (DeliveryMode::Incremental, "audio/pcm;codec=pcm_s16le")
            | (
                DeliveryMode::Complete | DeliveryMode::Incremental,
                "audio/mpeg" | "audio/ogg;codecs=opus" | "audio/aac" | "audio/flac",
            )
    )
}

fn validate_fixed_options_schema(schema: &Value, management: bool) -> Result<(), ProviderError> {
    if serde_json::to_vec(schema).map_or(true, |encoded| encoded.len() > 262_144) {
        return Err(ProviderError::Protocol(
            "provider options schema exceeds its byte limit".into(),
        ));
    }
    let root = schema.as_object().ok_or_else(|| {
        ProviderError::Protocol("provider options schema root is not an object".into())
    })?;
    if root.get("$schema").and_then(Value::as_str)
        != Some("https://json-schema.org/draft/2020-12/schema")
        || root.get("type").and_then(Value::as_str) != Some("object")
        || root.get("additionalProperties").and_then(Value::as_bool) != Some(false)
        || !root.get("properties").is_some_and(Value::is_object)
        || management
            && (root
                .get("required")
                .and_then(Value::as_array)
                .is_some_and(|required| !required.is_empty())
                || root
                    .get("minProperties")
                    .and_then(Value::as_u64)
                    .is_some_and(|minimum| minimum > 0))
    {
        return Err(ProviderError::Protocol(
            "provider options schema does not have a valid closed object root".into(),
        ));
    }
    validate_schema_references(schema, 0)
}

fn validate_schema_references(value: &Value, depth: usize) -> Result<(), ProviderError> {
    if depth > 16 {
        return Err(ProviderError::Protocol(
            "provider options schema nesting exceeds sixteen".into(),
        ));
    }
    match value {
        Value::Object(object) => {
            if object
                .get("$ref")
                .and_then(Value::as_str)
                .is_some_and(|reference| !reference.starts_with('#'))
            {
                return Err(ProviderError::Protocol(
                    "provider options schema contains a remote reference".into(),
                ));
            }
            for member in object.values() {
                validate_schema_references(member, depth + 1)?;
            }
        }
        Value::Array(items) => {
            for item in items {
                validate_schema_references(item, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn valid_descriptor_id(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
        && value.len() <= 64
        && bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

fn valid_capability_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
        && value.len() <= 64
        && bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

fn valid_patchable_options(names: &[String], properties: &serde_json::Map<String, Value>) -> bool {
    names.len() <= 64
        && !has_duplicates(names)
        && names.iter().all(|name| {
            !name.is_empty()
                && name.len() <= 256
                && !name.chars().any(char::is_control)
                && properties.get(name).is_some_and(|property| {
                    property.get("writeOnly").and_then(Value::as_bool) != Some(true)
                })
        })
}

fn valid_patchable_option_names(names: &[String]) -> bool {
    names.len() <= 64
        && !has_duplicates(names)
        && names.iter().all(|name| {
            let mut bytes = name.bytes();
            bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
                && name.len() <= 64
                && bytes
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        })
}

fn order_preserving_subset<T: PartialEq>(subset: &[T], values: &[T]) -> bool {
    let mut remaining = values;
    for item in subset {
        let Some(index) = remaining.iter().position(|candidate| candidate == item) else {
            return false;
        };
        remaining = &remaining[index + 1..];
    }
    true
}

fn valid_display_text(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.chars().count() <= maximum
        && !value
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\t' | '\n' | '\r'))
}

fn has_duplicates<T: PartialEq>(items: &[T]) -> bool {
    items
        .iter()
        .enumerate()
        .any(|(index, item)| items[..index].contains(item))
}

fn has_duplicates_by<T, K: PartialEq + ?Sized>(items: &[T], key: impl Fn(&T) -> &K) -> bool {
    items
        .iter()
        .enumerate()
        .any(|(index, item)| items[..index].iter().any(|prior| key(prior) == key(item)))
}

fn validate_shutdown_result(result: Value) -> Result<(), ProviderError> {
    if result.get("accepted").and_then(Value::as_bool) != Some(true) {
        return Err(ProviderError::Protocol(
            "session.shutdown did not return accepted=true".into(),
        ));
    }
    Ok(())
}

fn valid_semver(value: &str) -> bool {
    let (version, build) = value
        .split_once('+')
        .map_or((value, None), |(left, right)| (left, Some(right)));
    if build.is_some_and(|build| !valid_semver_identifiers(build, false)) {
        return false;
    }
    let (core, prerelease) = version
        .split_once('-')
        .map_or((version, None), |(left, right)| (left, Some(right)));
    if prerelease.is_some_and(|prerelease| !valid_semver_identifiers(prerelease, true)) {
        return false;
    }
    let parts: Vec<_> = core.split('.').collect();
    parts.len() == 3
        && parts.into_iter().all(|part| {
            !part.is_empty()
                && part.bytes().all(|byte| byte.is_ascii_digit())
                && (part == "0" || !part.starts_with('0'))
        })
}

fn valid_semver_identifiers(value: &str, reject_numeric_leading_zero: bool) -> bool {
    !value.is_empty()
        && value.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && !(reject_numeric_leading_zero
                    && part.bytes().all(|byte| byte.is_ascii_digit())
                    && part.len() > 1
                    && part.starts_with('0'))
        })
}

pub(crate) fn write_request(
    writer: &Arc<Mutex<ChildStdin>>,
    id: &str,
    method: &str,
    params: Value,
) -> Result<(), ProviderError> {
    let payload =
        serde_json::to_vec(&json!({"kind":"request", "id":id, "method":method, "params":params}))
            .map_err(|error| ProviderError::Protocol(error.to_string()))?;
    write_frame(
        &mut *writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
        1,
        &payload,
    )
}

pub(crate) fn wait_response(
    frames: &Receiver<Result<Frame, ProviderError>>,
    id: &str,
    timeout: Duration,
    operation_id: Option<&str>,
) -> Result<(Value, Vec<Value>), ProviderError> {
    let deadline = Instant::now() + timeout;
    let events = Vec::new();
    let mut progress_times = std::collections::VecDeque::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let frame = frames
            .recv_timeout(remaining)
            .map_err(|error| match error {
                RecvTimeoutError::Timeout => ProviderError::Timeout,
                RecvTimeoutError::Disconnected => {
                    ProviderError::Process("provider frame reader stopped".into())
                }
            })??;
        match frame {
            Frame::Audio(_) => {
                return Err(ProviderError::Protocol("unexpected audio frame".into()));
            }
            Frame::Control(value) => match value.get("kind").and_then(Value::as_str) {
                Some("event") => {
                    let name = validate_event_envelope(&value)?;
                    if name == "operation.progress" {
                        let expected = operation_id.ok_or_else(|| {
                            ProviderError::Protocol("unexpected operation.progress event".into())
                        })?;
                        validate_progress_event(&value["params"], expected)?;
                        let now = Instant::now();
                        while progress_times
                            .front()
                            .is_some_and(|time| now.duration_since(*time) >= Duration::from_secs(1))
                        {
                            progress_times.pop_front();
                        }
                        if progress_times.len() >= 10 {
                            return Err(ProviderError::Protocol(
                                "provider exceeded the progress event rate limit".into(),
                            ));
                        }
                        progress_times.push_back(now);
                    }
                    // Unknown advisory events and validated progress are intentionally not
                    // retained: volume and operation duration are provider-controlled.
                }
                Some("response") => {
                    if value.get("id").and_then(Value::as_str) != Some(id) {
                        return Err(ProviderError::Protocol(
                            "response request ID mismatch".into(),
                        ));
                    }
                    let result = value.get("result");
                    let error = value.get("error");
                    match (result, error) {
                        (Some(result), None) if result.is_object() => {
                            return Ok((result.clone(), events));
                        }
                        (Some(_), None) => {
                            return Err(ProviderError::Protocol(
                                "response result must be an object".into(),
                            ));
                        }
                        (None, Some(error)) => return Err(remote_error(error)),
                        _ => {
                            return Err(ProviderError::Protocol(
                                "response must contain exactly one of result or error".into(),
                            ));
                        }
                    }
                }
                _ => return Err(ProviderError::Protocol("invalid control envelope".into())),
            },
        }
    }
}

pub(crate) fn remote_error(value: &Value) -> ProviderError {
    let Some(code) = value.get("code").and_then(Value::as_str) else {
        return ProviderError::Protocol("provider error omitted a string code".into());
    };
    let Some(message) = value.get("message").and_then(Value::as_str) else {
        return ProviderError::Protocol("provider error omitted a string message".into());
    };
    if code.is_empty()
        || code.len() > 64
        || !code
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        || message.is_empty()
        || message.chars().count() > 512
        || message.chars().any(char::is_control)
    {
        return ProviderError::Protocol("provider error code or message is malformed".into());
    }
    ProviderError::Remote {
        code: code.to_owned(),
        message: message.to_owned(),
    }
}

fn validate_event_envelope(value: &Value) -> Result<&str, ProviderError> {
    let event = value.get("event").and_then(Value::as_str).ok_or_else(|| {
        ProviderError::Protocol("provider event omitted a string event name".into())
    })?;
    if event.is_empty()
        || event.len() > 128
        || event.chars().any(char::is_control)
        || !value.get("params").is_some_and(Value::is_object)
    {
        return Err(ProviderError::Protocol(
            "provider event envelope is malformed".into(),
        ));
    }
    Ok(event)
}

fn validate_progress_event(params: &Value, operation_id: &str) -> Result<(), ProviderError> {
    let phase = params.get("phase").and_then(Value::as_str);
    let completed = params.get("completed").and_then(Value::as_u64);
    let total = params.get("total");
    let unit = params.get("unit").and_then(Value::as_str);
    let message = params.get("message");
    if params.get("operation_id").and_then(Value::as_str) != Some(operation_id)
        || phase.is_none_or(|phase| !valid_descriptor_id(phase))
        || completed.is_none()
        || total.is_some_and(|total| {
            total
                .as_u64()
                .is_none_or(|total| completed.is_none_or(|completed| total < completed))
        })
        || !matches!(unit, Some("bytes" | "items" | "unknown"))
        || message.is_some_and(|message| {
            message
                .as_str()
                .is_none_or(|message| !valid_display_text(message, 512))
        })
    {
        return Err(ProviderError::Protocol(
            "provider returned an invalid operation.progress event".into(),
        ));
    }
    Ok(())
}

fn read_frame(
    reader: &mut impl Read,
    max_audio_frame_bytes: &RwLock<usize>,
) -> Result<Option<Frame>, ProviderError> {
    let mut header = [0_u8; 12];
    let mut first = [0_u8; 1];
    match reader.read(&mut first) {
        Ok(0) => return Ok(None),
        Ok(1) => header[0] = first[0],
        Ok(_) => unreachable!(),
        Err(error) => return Err(error.into()),
    }
    reader
        .read_exact(&mut header[1..])
        .map_err(|error| ProviderError::Protocol(format!("truncated frame header: {error}")))?;
    if &header[..4] != MAGIC || header[5] != 0 || header[6] != 0 || header[7] != 0 {
        return Err(ProviderError::Protocol("invalid frame header".into()));
    }
    let length = u32::from_be_bytes(header[8..12].try_into().unwrap()) as usize;
    let max_audio_frame_bytes = *max_audio_frame_bytes
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let limit = match header[4] {
        1 => MAX_CONTROL_BYTES,
        2 => max_audio_frame_bytes.min(MAX_AUDIO_BYTES as usize),
        _ => return Err(ProviderError::Protocol("unknown frame kind".into())),
    };
    if length == 0 || length > limit || header[4] == 1 && length < 2 {
        return Err(ProviderError::Protocol(
            "frame payload length is outside protocol bounds".into(),
        ));
    }
    let mut payload = vec![0_u8; length];
    reader
        .read_exact(&mut payload)
        .map_err(|error| ProviderError::Protocol(format!("truncated frame payload: {error}")))?;
    if header[4] == 2 {
        return Ok(Some(Frame::Audio(payload)));
    }
    let value = strict_json(&payload)?;
    if !value.is_object() {
        return Err(ProviderError::Protocol(
            "control payload is not a JSON object".into(),
        ));
    }
    Ok(Some(Frame::Control(value)))
}

fn strict_json(payload: &[u8]) -> Result<Value, ProviderError> {
    struct StrictValue(Value);

    impl<'de> Deserialize<'de> for StrictValue {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct StrictVisitor;

            impl<'de> Visitor<'de> for StrictVisitor {
                type Value = StrictValue;

                fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    formatter.write_str("a JSON value without duplicate object keys")
                }

                fn visit_bool<E: de::Error>(self, value: bool) -> Result<Self::Value, E> {
                    Ok(StrictValue(Value::Bool(value)))
                }

                fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                    if value.unsigned_abs() > 9_007_199_254_740_991 {
                        return Err(E::custom("integer is outside the exact JSON range"));
                    }
                    Ok(StrictValue(Value::Number(value.into())))
                }

                fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                    if value > 9_007_199_254_740_991 {
                        return Err(E::custom("integer is outside the exact JSON range"));
                    }
                    Ok(StrictValue(Value::Number(value.into())))
                }

                fn visit_f64<E: de::Error>(self, value: f64) -> Result<Self::Value, E> {
                    serde_json::Number::from_f64(value)
                        .map(|number| StrictValue(Value::Number(number)))
                        .ok_or_else(|| E::custom("non-finite JSON number"))
                }

                fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                    Ok(StrictValue(Value::String(value.to_owned())))
                }

                fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
                    Ok(StrictValue(Value::String(value)))
                }

                fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
                    Ok(StrictValue(Value::Null))
                }

                fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                    Ok(StrictValue(Value::Null))
                }

                fn visit_seq<A: SeqAccess<'de>>(
                    self,
                    mut sequence: A,
                ) -> Result<Self::Value, A::Error> {
                    let mut values = Vec::new();
                    while let Some(StrictValue(value)) = sequence.next_element::<StrictValue>()? {
                        values.push(value);
                    }
                    Ok(StrictValue(Value::Array(values)))
                }

                fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                    let mut values = serde_json::Map::new();
                    let mut keys = HashSet::new();
                    while let Some(key) = map.next_key::<String>()? {
                        if !keys.insert(key.clone()) {
                            return Err(de::Error::custom(format!("duplicate object key '{key}'")));
                        }
                        let StrictValue(value) = map.next_value::<StrictValue>()?;
                        values.insert(key, value);
                    }
                    Ok(StrictValue(Value::Object(values)))
                }
            }

            deserializer.deserialize_any(StrictVisitor)
        }
    }

    let mut deserializer = serde_json::Deserializer::from_slice(payload);
    let StrictValue(value) = StrictValue::deserialize(&mut deserializer)
        .map_err(|error| ProviderError::Protocol(format!("invalid control JSON: {error}")))?;
    deserializer.end().map_err(|error| {
        ProviderError::Protocol(format!("invalid trailing control JSON: {error}"))
    })?;
    Ok(value)
}

fn write_frame(writer: &mut impl Write, kind: u8, payload: &[u8]) -> Result<(), ProviderError> {
    let maximum = if kind == 1 {
        MAX_CONTROL_BYTES
    } else {
        u32::MAX as usize
    };
    if payload.is_empty() || payload.len() > maximum {
        return Err(ProviderError::Protocol(
            "outbound frame length is invalid".into(),
        ));
    }
    let mut header = [0_u8; 12];
    header[..4].copy_from_slice(MAGIC);
    header[4] = kind;
    header[8..12].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    writer.write_all(&header)?;
    writer.write_all(payload)?;
    writer.flush()?;
    Ok(())
}

fn baseline_environment(
    requested: &[String],
) -> Result<BTreeMap<OsString, OsString>, ProviderError> {
    let baseline = [
        "HOME",
        "USERPROFILE",
        "LOCALAPPDATA",
        "TMPDIR",
        "TEMP",
        "TMP",
        "LANG",
        "LC_ALL",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "SYSTEMROOT",
        "WINDIR",
    ];
    let mut result = BTreeMap::new();
    for name in baseline {
        if let Some(value) = env::var_os(name) {
            result.insert(name.into(), value);
        }
    }
    for name in requested {
        let value = env::var_os(name).ok_or_else(|| {
            ProviderError::Configuration(format!(
                "required provider environment variable '{name}' is not set"
            ))
        })?;
        result.insert(name.into(), value);
    }
    Ok(result)
}

pub(crate) fn provider_directories(slug: &str) -> Result<(PathBuf, PathBuf), ProviderError> {
    #[cfg(windows)]
    {
        let base = env::var_os("LOCALAPPDATA")
            .ok_or_else(|| ProviderError::Configuration("LOCALAPPDATA is not set".into()))?;
        let root = PathBuf::from(base)
            .join("UtterPipe")
            .join("providers")
            .join(slug);
        Ok((root.join("data"), root.join("cache")))
    }
    #[cfg(target_os = "macos")]
    {
        let home = env::var_os("HOME")
            .ok_or_else(|| ProviderError::Configuration("HOME is not set".into()))?;
        let home = PathBuf::from(home);
        Ok((
            home.join("Library/Application Support/UtterPipe/providers")
                .join(slug)
                .join("data"),
            home.join("Library/Caches/UtterPipe/providers").join(slug),
        ))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let home = env::var_os("HOME")
            .ok_or_else(|| ProviderError::Configuration("HOME is not set".into()))?;
        let data = env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(&home).join(".local/share"));
        let cache = env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(&home).join(".cache"));
        Ok((
            data.join("utterpipe/providers").join(slug),
            cache.join("utterpipe/providers").join(slug),
        ))
    }
}

pub(crate) fn ensure_provider_directories(data: &Path, cache: &Path) -> Result<(), ProviderError> {
    fs::create_dir_all(data)?;
    fs::create_dir_all(cache)?;
    #[cfg(unix)]
    for path in [data, cache] {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    let data = fs::canonicalize(data)?;
    let cache = fs::canonicalize(cache)?;
    if data == cache || data.starts_with(&cache) || cache.starts_with(&data) {
        return Err(ProviderError::Configuration(
            "provider data and cache directories must be distinct and non-nested".into(),
        ));
    }
    Ok(())
}

fn toml_table_to_json(table: &toml::Table) -> Result<Value, ProviderError> {
    serde_json::to_value(table).map_err(|error| {
        ProviderError::Configuration(format!("provider options could not be encoded: {error}"))
    })
}

pub(crate) fn unicode_path<'a>(
    path: &'a Path,
    description: &str,
) -> Result<&'a str, ProviderError> {
    path.to_str().ok_or_else(|| {
        ProviderError::Configuration(format!(
            "{description} is not valid Unicode and cannot be sent to an UtterPipe provider"
        ))
    })
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::{fs, os::unix::fs::PermissionsExt};

    use super::*;

    #[test]
    fn audio_frame_limit_tracks_each_requested_delivery_mode() {
        assert_eq!(
            audio_frame_limit_for(DeliveryMode::Complete),
            MAX_AUDIO_BYTES as usize
        );
        assert_eq!(
            audio_frame_limit_for(DeliveryMode::Incremental),
            MAX_INCREMENTAL_AUDIO_FRAME_BYTES
        );
    }

    #[test]
    fn strict_json_rejects_duplicate_keys_at_every_depth() {
        assert!(strict_json(br#"{"kind":"event","kind":"response"}"#).is_err());
        assert!(strict_json(br#"{"outer":{"id":1,"id":2}}"#).is_err());
        assert!(strict_json(br#"{"too_large":9007199254740992}"#).is_err());
        assert_eq!(strict_json(br#"{"ok":[true,null,1]}"#).unwrap()["ok"][2], 1);
    }

    #[test]
    fn response_success_requires_one_object_result() {
        for response in [
            json!({"kind":"response", "id":"r", "result":true}),
            json!({"kind":"response", "id":"r", "result":{}, "error":{"code":"internal", "message":"bad"}}),
        ] {
            let (sender, receiver) = crossbeam_channel::bounded(1);
            sender.send(Ok(Frame::Control(response))).unwrap();
            assert!(matches!(
                wait_response(&receiver, "r", Duration::from_millis(10), None),
                Err(ProviderError::Protocol(_))
            ));
        }
    }

    #[test]
    fn response_wait_ignores_future_events_and_validates_progress() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        sender
            .send(Ok(Frame::Control(json!({
                "kind":"event","event":"future.advisory","params":{"value":1}
            }))))
            .unwrap();
        sender
            .send(Ok(Frame::Control(json!({
                "kind":"event","event":"operation.progress","params":{
                    "operation_id":"prepare-1","phase":"download","completed":5,
                    "total":10,"unit":"bytes","message":"Downloading"
                }
            }))))
            .unwrap();
        sender
            .send(Ok(Frame::Control(json!({
                "kind":"response","id":"r","result":{"status":"ready"}
            }))))
            .unwrap();
        assert!(
            wait_response(&receiver, "r", Duration::from_millis(10), Some("prepare-1")).is_ok()
        );

        let (sender, receiver) = crossbeam_channel::bounded(1);
        sender
            .send(Ok(Frame::Control(json!({
                "kind":"event","event":"operation.progress","params":{
                    "operation_id":"wrong","phase":"download","completed":5,
                    "total":4,"unit":"bytes"
                }
            }))))
            .unwrap();
        assert!(matches!(
            wait_response(&receiver, "r", Duration::from_millis(10), Some("prepare-1")),
            Err(ProviderError::Protocol(_))
        ));
    }

    #[test]
    fn hello_rejects_terminal_control_characters() {
        let mut hello: HelloResult = serde_json::from_value(json!({
            "protocol":"utterpipe.tts", "version":1,"framing":"UTP1",
            "provider":{"slug":"fake", "name":"Fake", "vendor":"Tests", "version":"0.1.0"},
            "capabilities":["synthesis"],
            "audio_deliveries":[{"mode":"complete","format":"audio/wav;codec=pcm_s16le"}],
            "utterance_schema_profile":"utterpipe.utterance-options/1",
            "provider_options_schema":{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","additionalProperties":false,"properties":{}},
            "management_options_schema":{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","additionalProperties":false,"properties":{}},
            "catalogs":[],"import_kinds":[]
        })).unwrap();
        hello.provider.name = "Fake\u{1b}[31m".into();
        assert!(validate_hello(&hello, "fake").is_err());
        hello.provider.name = "Fake".into();
        hello.audio_deliveries.push(json!({
            "mode":"complete",
            "format":"future\tformat"
        }));
        assert!(validate_hello(&hello, "fake").is_err());
    }

    #[test]
    fn hello_accepts_compressed_incremental_delivery() {
        let hello: HelloResult = serde_json::from_value(json!({
            "protocol":"utterpipe.tts", "version":1,"framing":"UTP1",
            "provider":{"slug":"fake", "name":"Fake", "vendor":"Tests", "version":"0.1.0"},
            "capabilities":["synthesis"],
            "audio_deliveries":[
                {"mode":"complete","format":"audio/wav;codec=pcm_s16le"},
                {"mode":"incremental","format":"audio/ogg;codecs=opus"}
            ],
            "utterance_schema_profile":"utterpipe.utterance-options/1",
            "provider_options_schema":{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","additionalProperties":false,"properties":{}},
            "management_options_schema":{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","additionalProperties":false,"properties":{}},
            "catalogs":[],"import_kinds":[]
        })).unwrap();
        validate_hello(&hello, "fake").unwrap();
    }

    #[test]
    fn hello_ignores_well_formed_future_audio_pairs() {
        let hello: HelloResult = serde_json::from_value(json!({
            "protocol":"utterpipe.tts", "version":1,"framing":"UTP1",
            "provider":{"slug":"fake", "name":"Fake", "vendor":"Tests", "version":"0.1.0"},
            "capabilities":["synthesis","future.capability"],
            "audio_deliveries":[
                {"mode":"future","format":"audio/future"},
                {"mode":"complete","format":"audio/future"},
                {"mode":"complete","format":"audio/wav;codec=pcm_s16le"}
            ],
            "utterance_schema_profile":"utterpipe.utterance-options/1",
            "provider_options_schema":{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","additionalProperties":false,"properties":{}},
            "management_options_schema":{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","additionalProperties":false,"properties":{}},
            "catalogs":[],"import_kinds":[]
        })).unwrap();
        assert_eq!(validate_hello(&hello, "fake").unwrap().len(), 1);
    }

    #[test]
    fn product_versions_use_semver_two_syntax() {
        for valid in ["0.1.0", "1.2.3-alpha.1+build-7", "10.0.0+abc"] {
            assert!(valid_semver(valid), "rejected {valid}");
        }
        for invalid in ["1", "1.2", "01.2.3", "1.2.3-01", "1.2.3+", "v1.2.3"] {
            assert!(!valid_semver(invalid), "accepted {invalid}");
        }
    }

    #[test]
    fn shutdown_result_requires_positive_acceptance() {
        assert!(validate_shutdown_result(json!({"accepted": true})).is_ok());
        assert!(validate_shutdown_result(json!({"accepted": false})).is_err());
        assert!(validate_shutdown_result(json!({})).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_job_is_created_with_kill_on_close() {
        let limits = kill_on_close_job_limits();
        assert_eq!(
            limits.BasicLimitInformation.LimitFlags,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        );
        let job = WindowsJobObject::new().unwrap();
        assert!(!job.handle.is_invalid());
    }

    #[test]
    fn frame_reader_rejects_bounds_and_reserved_bits_before_payload() {
        let bad_reserved = *b"UTP1\x01\x00\x00\x01\x00\x00\x00\x02{}";
        let complete_limit = RwLock::new(MAX_AUDIO_BYTES as usize);
        assert!(read_frame(&mut bad_reserved.as_slice(), &complete_limit).is_err());
        let oversized = [b'U', b'T', b'P', b'1', 1, 0, 0, 0, 0xff, 0xff, 0xff, 0xff];
        assert!(read_frame(&mut oversized.as_slice(), &complete_limit).is_err());
        let incremental_oversized = [b'U', b'T', b'P', b'1', 2, 0, 0, 0, 0, 0x10, 0x00, 0x01];
        assert!(
            read_frame(
                &mut incremental_oversized.as_slice(),
                &RwLock::new(MAX_INCREMENTAL_AUDIO_FRAME_BYTES)
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn failed_handshake_terminates_and_reaps_provider() {
        let interpreter = [
            "/usr/bin/python3",
            "/usr/local/bin/python3",
            "/opt/homebrew/bin/python3",
        ]
        .into_iter()
        .find(|path| Path::new(path).is_file());
        let Some(interpreter) = interpreter else {
            return;
        };
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("utterpipe-invalid");
        let source = format!(
            r#"#!{interpreter}
import json, os, struct, sys, time
with open(os.path.join(os.path.dirname(__file__), 'pid'), 'w') as output:
    output.write(str(os.getpid()))
header = sys.stdin.buffer.read(12)
size = struct.unpack('>I', header[8:12])[0]
sys.stdin.buffer.read(size)
payload = json.dumps({{'kind':'response','id':'hello','result':{{}}}}, separators=(',', ':')).encode()
sys.stdout.buffer.write(b'UTP1' + bytes([1,0,0,0]) + struct.pack('>I', len(payload)) + payload)
sys.stdout.buffer.flush()
time.sleep(60)
"#
        );
        fs::write(&executable, source).unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&executable, permissions).unwrap();

        assert!(
            Client::spawn(&executable, "invalid", SessionKind::Inspect, &[]).is_err(),
            "invalid hello unexpectedly completed the handshake"
        );
        let pid: i32 = fs::read_to_string(directory.path().join("pid"))
            .unwrap()
            .parse()
            .unwrap();
        // SAFETY: Signal zero only queries whether this recorded child PID exists.
        let child_still_exists = unsafe { libc::kill(pid, 0) } == 0;
        assert!(
            !child_still_exists,
            "provider remained alive or unreaped after handshake failure"
        );
    }

    #[cfg(unix)]
    #[test]
    fn shutdown_rejects_a_nonzero_provider_exit() {
        let interpreter = [
            "/usr/bin/python3",
            "/usr/local/bin/python3",
            "/opt/homebrew/bin/python3",
        ]
        .into_iter()
        .find(|path| Path::new(path).is_file());
        let Some(interpreter) = interpreter else {
            return;
        };
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("utterpipe-nonzero");
        let source = format!(
            r#"#!{interpreter}
import json, struct, sys
def read_frame():
    header = sys.stdin.buffer.read(12)
    if not header: return None
    size = struct.unpack('>I', header[8:12])[0]
    return json.loads(sys.stdin.buffer.read(size))
def write(value):
    payload = json.dumps(value, separators=(',', ':')).encode()
    sys.stdout.buffer.write(b'UTP1' + bytes([1,0,0,0]) + struct.pack('>I', len(payload)) + payload)
    sys.stdout.buffer.flush()
hello = read_frame()
schema = {{'$schema':'https://json-schema.org/draft/2020-12/schema','type':'object','additionalProperties':False,'properties':{{}}}}
write({{'kind':'response','id':hello['id'],'result':{{'protocol':'utterpipe.tts','version':1,'framing':'UTP1','provider':{{'slug':'nonzero','name':'Nonzero','vendor':'Tests','version':'0.1.0'}},'capabilities':['synthesis'],'audio_deliveries':[{{'mode':'complete','format':'audio/wav;codec=pcm_s16le'}}],'utterance_schema_profile':'utterpipe.utterance-options/1','provider_options_schema':schema,'management_options_schema':schema,'catalogs':[],'import_kinds':[]}}}})
shutdown = read_frame()
write({{'kind':'response','id':shutdown['id'],'result':{{'accepted':True}}}})
assert read_frame() is None
sys.exit(7)
"#
        );
        fs::write(&executable, source).unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&executable, permissions).unwrap();

        let client = Client::spawn(&executable, "nonzero", SessionKind::Inspect, &[]).unwrap();
        assert!(matches!(client.shutdown(), Err(ProviderError::Process(_))));
    }

    #[cfg(unix)]
    #[test]
    fn fake_provider_completes_management_handshake_initialize_validate_and_shutdown() {
        let interpreter = [
            "/usr/bin/python3",
            "/usr/local/bin/python3",
            "/opt/homebrew/bin/python3",
        ]
        .into_iter()
        .find(|path| Path::new(path).is_file());
        let Some(interpreter) = interpreter else {
            return;
        };
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("utterpipe-fake");
        let source = format!(
            r#"#!{interpreter}
import json, struct, sys
def read_frame():
    header = sys.stdin.buffer.read(12)
    if not header: return None
    assert len(header) == 12 and header[:4] == b'UTP1' and header[4] == 1
    size = struct.unpack('>I', header[8:12])[0]
    return json.loads(sys.stdin.buffer.read(size))
def write(value):
    payload = json.dumps(value, separators=(',', ':')).encode()
    sys.stdout.buffer.write(b'UTP1' + bytes([1,0,0,0]) + struct.pack('>I', len(payload)) + payload)
    sys.stdout.buffer.flush()
while True:
    request = read_frame()
    if request is None: break
    method = request['method']
    if method == 'protocol.hello':
        schema = {{'$schema':'https://json-schema.org/draft/2020-12/schema','type':'object','additionalProperties':False,'properties':{{}}}}
        result = {{'protocol':'utterpipe.tts','version':1,'framing':'UTP1','provider':{{'slug':'fake','name':'Fake','vendor':'Tests','version':'0.1.0'}},'capabilities':['synthesis'],'audio_deliveries':[{{'mode':'complete','format':'audio/wav;codec=pcm_s16le'}}],'utterance_schema_profile':'utterpipe.utterance-options/1','provider_options_schema':schema,'management_options_schema':schema,'catalogs':[],'import_kinds':[]}}
    elif method == 'session.initialize': result = {{'ready':True}}
    elif method == 'provider.validate': result = {{'status':'ready','issues':[]}}
    elif method == 'session.shutdown':
        write({{'kind':'response','id':request['id'],'result':{{'accepted':True}}}})
        assert read_frame() is None
        break
    else: result = {{'status':'ready'}}
    write({{'kind':'response','id':request['id'],'result':result}})
"#
        );
        fs::write(&executable, source).unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&executable, permissions).unwrap();

        let tts = TtsConfig {
            enabled: true,
            backend: crate::config::TtsBackend::Utterpipe(crate::config::UtterPipeTtsConfig {
                provider: "fake".into(),
                audio_deliveries: Vec::new(),
                provider_environment: Vec::new(),
                provider_options: toml::Table::new(),
                utterance_options: toml::Table::new(),
                agent_utterance_options: Vec::new(),
            }),
            maximum_characters: 300,
        };
        let mut client = Client::spawn(&executable, "fake", SessionKind::Management, &[]).unwrap();
        client
            .initialize(
                &tts,
                &directory.path().join("data"),
                &directory.path().join("cache"),
            )
            .unwrap();
        let validation = client
            .call("provider.validate", json!({}), Duration::from_secs(1))
            .unwrap();
        assert_eq!(validation["status"], "ready");
        client.shutdown().unwrap();
    }
}
