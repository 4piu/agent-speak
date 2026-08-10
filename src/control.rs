//! Optional authenticated loopback control channel for local human-facing UIs.

use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::{Semaphore, oneshot},
    task::{JoinHandle, JoinSet},
    time::timeout,
};
use uuid::Uuid;

use crate::playback::{PlaybackError, PlaybackHandle, PlaybackState};

const CONTROL_SCHEMA_VERSION: u32 = 1;
const MAXIMUM_REQUEST_BYTES: u64 = 8 * 1024;
const MAXIMUM_CONNECTIONS: usize = 16;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlDescriptor {
    pub schema_version: u32,
    pub session_id: String,
    pub host: String,
    pub port: u16,
    pub token: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlRequest {
    token: String,
    method: String,
    #[serde(default)]
    playback_id: Option<String>,
}

/// Running loopback listener and its private descriptor file.
#[derive(Debug)]
pub struct ControlServer {
    descriptor_path: PathBuf,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl ControlServer {
    pub async fn start(
        descriptor_path: impl AsRef<Path>,
        playback: PlaybackHandle,
    ) -> io::Result<Self> {
        let descriptor_path = descriptor_path.as_ref().to_owned();
        if !descriptor_path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "control descriptor path must be absolute",
            ));
        }
        let parent = descriptor_path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "control descriptor path has no parent directory",
            )
        })?;
        if !parent.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "control descriptor parent directory does not exist",
            ));
        }

        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let session_id = Uuid::new_v4().to_string();
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let descriptor = ControlDescriptor {
            schema_version: CONTROL_SCHEMA_VERSION,
            session_id: session_id.clone(),
            host: address.ip().to_string(),
            port: address.port(),
            token: token.clone(),
        };
        write_private_descriptor(&descriptor_path, &descriptor)?;

        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let permits = Arc::new(Semaphore::new(MAXIMUM_CONNECTIONS));
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    _ = connections.join_next(), if !connections.is_empty() => {}
                    accepted = listener.accept() => {
                        let Ok((stream, _peer)) = accepted else {
                            break;
                        };
                        let Ok(permit) = permits.clone().try_acquire_owned() else {
                            continue;
                        };
                        let playback = playback.clone();
                        let token = token.clone();
                        let session_id = session_id.clone();
                        connections.spawn(async move {
                            let _permit = permit;
                            let _ = handle_connection(stream, &token, &session_id, playback).await;
                        });
                    }
                }
            }
            connections.abort_all();
            while connections.join_next().await.is_some() {}
        });

        Ok(Self {
            descriptor_path,
            shutdown: Some(shutdown_tx),
            task,
        })
    }

    pub async fn shutdown(mut self) -> io::Result<()> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = self.task.await;
        match fs::remove_file(&self.descriptor_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

async fn handle_connection(
    stream: TcpStream,
    expected_token: &str,
    session_id: &str,
    playback: PlaybackHandle,
) -> io::Result<()> {
    let mut reader = BufReader::new(stream);
    let mut bytes = Vec::new();
    let read = timeout(
        REQUEST_TIMEOUT,
        (&mut reader)
            .take(MAXIMUM_REQUEST_BYTES + 1)
            .read_until(b'\n', &mut bytes),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "control request timed out"))??;
    let response = if read == 0 || bytes.len() as u64 > MAXIMUM_REQUEST_BYTES {
        error_response("invalid_request", "control request is invalid", false)
    } else {
        match serde_json::from_slice::<ControlRequest>(&bytes) {
            Ok(request) if constant_time_equal(&request.token, expected_token) => {
                dispatch(request, session_id, playback).await
            }
            Ok(_) => error_response("permission_denied", "control token is invalid", false),
            Err(_) => error_response("invalid_request", "control request is invalid", false),
        }
    };
    let mut response = serde_json::to_vec(&response).map_err(io::Error::other)?;
    response.push(b'\n');
    reader.get_mut().write_all(&response).await
}

async fn dispatch(request: ControlRequest, session_id: &str, playback: PlaybackHandle) -> Value {
    match (request.method.as_str(), request.playback_id) {
        ("snapshot", None) => match playback.snapshot().await {
            Ok(items) => json!({
                "ok": true,
                "snapshot": {
                    "session_id": session_id,
                    "items": items.into_iter().map(|item| json!({
                        "playback_id": item.playback_id.to_string(),
                        "status": state_name(item.state),
                        "terminal": item.state.is_terminal(),
                        "error_code": (item.state == PlaybackState::Failed)
                            .then_some("playback_unavailable"),
                    })).collect::<Vec<_>>()
                }
            }),
            Err(error) => playback_error_response(error),
        },
        ("cancel", Some(playback_id)) => {
            let playback_id = match Uuid::parse_str(&playback_id) {
                Ok(playback_id) => playback_id,
                Err(_) => {
                    return error_response(
                        "invalid_playback_id",
                        "playback_id must be a valid UUID",
                        false,
                    );
                }
            };
            match playback.cancel(playback_id).await {
                Ok(Some(result)) => json!({
                    "ok": true,
                    "cancellation": {
                        "playback_id": result.playback_id.to_string(),
                        "status": state_name(result.state),
                        "terminal": result.state.is_terminal(),
                        "cancelled": result.cancelled,
                    }
                }),
                Ok(None) => error_response(
                    "unknown_playback_id",
                    "playback ID is not known or is no longer retained",
                    false,
                ),
                Err(error) => playback_error_response(error),
            }
        }
        ("emergency_stop", None) => match playback.emergency_stop().await {
            Ok(result) => json!({
                "ok": true,
                "emergency_stop": {
                    "interrupted_items": result.interrupted_items,
                }
            }),
            Err(error) => playback_error_response(error),
        },
        _ => error_response("invalid_request", "control request is invalid", false),
    }
}

fn playback_error_response(error: PlaybackError) -> Value {
    match error {
        PlaybackError::ActorBusy => error_response(
            "request_not_accepted",
            "playback service is busy; retry later",
            true,
        ),
        PlaybackError::ActorClosed | PlaybackError::Backend(_) => error_response(
            "playback_unavailable",
            "playback backend is unavailable",
            true,
        ),
        _ => error_response(
            "request_not_accepted",
            "playback request was not accepted",
            false,
        ),
    }
}

fn error_response(code: &str, message: &str, retryable: bool) -> Value {
    json!({
        "ok": false,
        "error": {
            "code": code,
            "message": message,
            "retryable": retryable,
        }
    })
}

fn state_name(state: PlaybackState) -> &'static str {
    match state {
        PlaybackState::Accepted => "accepted",
        PlaybackState::Playing => "playing",
        PlaybackState::Completed => "completed",
        PlaybackState::Interrupted => "interrupted",
        PlaybackState::Failed => "failed",
    }
}

fn constant_time_equal(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.bytes()
        .zip(right.bytes())
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn write_private_descriptor(path: &Path, descriptor: &ControlDescriptor) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    // Do not include `open` in the cleanup scope: a failed `create_new` means
    // the path belongs to somebody else and must remain untouched.
    let mut file = options.open(path)?;
    let result = (|| {
        let bytes = serde_json::to_vec(descriptor).map_err(io::Error::other)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        #[cfg(unix)]
        {
            let mode = file.metadata()?.permissions().mode() & 0o777;
            if mode != 0o600 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "control descriptor permissions are not private",
                ));
            }
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(path);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playback::{
        CompletionNotifier, ConcurrencyMode, PlaybackBackend, PlaybackJob, PlaybackState,
    };
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

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

    async fn request(descriptor: &ControlDescriptor, value: Value) -> Value {
        let mut stream = TcpStream::connect((descriptor.host.as_str(), descriptor.port))
            .await
            .unwrap();
        stream
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
        let mut response = String::new();
        BufReader::new(stream)
            .read_line(&mut response)
            .await
            .unwrap();
        serde_json::from_str(&response).unwrap()
    }

    #[tokio::test]
    async fn control_channel_is_private_sanitized_and_controls_the_actor() {
        let directory = tempfile::tempdir().unwrap();
        let descriptor_path = directory.path().join("control.json");
        let playback = PlaybackHandle::spawn(2, || Ok(HoldingBackend::default())).unwrap();
        let server = ControlServer::start(&descriptor_path, playback.clone())
            .await
            .unwrap();
        let descriptor: ControlDescriptor =
            serde_json::from_slice(&fs::read(&descriptor_path).unwrap()).unwrap();
        assert_eq!(descriptor.schema_version, CONTROL_SCHEMA_VERSION);
        assert_eq!(descriptor.host, "127.0.0.1");
        assert!(descriptor.token.len() >= 64);
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&descriptor_path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let unauthorized =
            request(&descriptor, json!({"token": "wrong", "method": "snapshot"})).await;
        assert_eq!(unauthorized["error"]["code"], "permission_denied");

        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        playback
            .submit(
                PlaybackJob::speech(first, "never expose this text", 0.4),
                ConcurrencyMode::Enqueue,
            )
            .await
            .unwrap();
        playback
            .submit(
                PlaybackJob::speech(second, "nor this queued text", 0.4),
                ConcurrencyMode::Enqueue,
            )
            .await
            .unwrap();

        let snapshot = request(
            &descriptor,
            json!({"token": descriptor.token, "method": "snapshot"}),
        )
        .await;
        assert_eq!(
            snapshot["snapshot"]["items"][0]["playback_id"],
            second.to_string()
        );
        assert_eq!(snapshot["snapshot"]["items"][0]["status"], "accepted");
        assert_eq!(snapshot["snapshot"]["items"][1]["status"], "playing");
        let serialized = snapshot.to_string();
        assert!(!serialized.contains("never expose"));
        assert!(!serialized.contains("queued text"));

        let cancelled = request(
            &descriptor,
            json!({
                "token": descriptor.token,
                "method": "cancel",
                "playback_id": second,
            }),
        )
        .await;
        assert_eq!(cancelled["cancellation"]["status"], "interrupted");
        assert_eq!(cancelled["cancellation"]["cancelled"], true);

        let stopped = request(
            &descriptor,
            json!({"token": descriptor.token, "method": "emergency_stop"}),
        )
        .await;
        assert_eq!(stopped["emergency_stop"]["interrupted_items"], 1);
        assert_eq!(
            playback.status(first).await.unwrap().unwrap().state,
            PlaybackState::Interrupted
        );

        server.shutdown().await.unwrap();
        assert!(!descriptor_path.exists());
        playback.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn control_descriptor_requires_an_absolute_fresh_path() {
        let playback = PlaybackHandle::spawn(1, || Ok(HoldingBackend::default())).unwrap();
        assert_eq!(
            ControlServer::start("relative-control.json", playback.clone())
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control.json");
        fs::write(&path, b"user-owned").unwrap();
        assert_eq!(
            ControlServer::start(&path, playback.clone())
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(path).unwrap(), b"user-owned");
        playback.shutdown().await.unwrap();
    }
}
