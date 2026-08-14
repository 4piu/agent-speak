//! Authenticated loopback Streamable HTTP transport for desktop-owned MCP use.

use std::{
    future::Future,
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use axum::{
    Router,
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use rmcp::transport::{
    StreamableHttpServerConfig, StreamableHttpService,
    streamable_http_server::session::local::LocalSessionManager,
};
use serde::{Deserialize, Serialize};
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::{
    mcp::AgentSpeakServer,
    private_file::{constant_time_equal, random_token, write_private_json},
};

const DESCRIPTOR_SCHEMA_VERSION: u32 = 1;
const MCP_PATH: &str = "/mcp";
const MAXIMUM_REQUEST_BYTES: usize = 128 * 1024;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpMcpDescriptor {
    pub schema_version: u32,
    pub transport: String,
    pub url: String,
    pub authorization: HttpAuthorizationDescriptor,
}

impl std::fmt::Debug for HttpMcpDescriptor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HttpMcpDescriptor")
            .field("schema_version", &self.schema_version)
            .field("transport", &self.transport)
            .field("url", &self.url)
            .field("authorization", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpAuthorizationDescriptor {
    pub scheme: String,
    pub token: String,
}

#[derive(Clone)]
struct BearerToken(Arc<str>);

/// A running authenticated MCP listener bound to IPv4 loopback.
pub struct HttpMcpServer {
    descriptor_path: PathBuf,
    cancellation: CancellationToken,
    task: Option<JoinHandle<io::Result<()>>>,
}

impl HttpMcpServer {
    pub async fn start(
        descriptor_path: impl AsRef<Path>,
        server: AgentSpeakServer,
    ) -> io::Result<Self> {
        let descriptor_path = descriptor_path.as_ref().to_owned();
        validate_descriptor_path(&descriptor_path)?;

        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let token = random_token()?;
        let descriptor = HttpMcpDescriptor {
            schema_version: DESCRIPTOR_SCHEMA_VERSION,
            transport: "streamable_http".to_owned(),
            url: format!("http://{address}{MCP_PATH}"),
            authorization: HttpAuthorizationDescriptor {
                scheme: "Bearer".to_owned(),
                token: token.clone(),
            },
        };

        let cancellation = CancellationToken::new();
        let config = StreamableHttpServerConfig::default()
            .with_json_response(true)
            .with_max_request_body_bytes(MAXIMUM_REQUEST_BYTES)
            .with_cancellation_token(cancellation.child_token());
        let service: StreamableHttpService<AgentSpeakServer, LocalSessionManager> =
            StreamableHttpService::new(
                move || Ok(server.clone()),
                Arc::new(LocalSessionManager::default()),
                config,
            );
        let router =
            Router::new()
                .nest_service(MCP_PATH, service)
                .layer(middleware::from_fn_with_state(
                    BearerToken(Arc::from(token)),
                    require_bearer,
                ));

        write_private_json(&descriptor_path, &descriptor)?;
        let graceful_cancellation = cancellation.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    graceful_cancellation.cancelled_owned().await;
                })
                .await
                .map_err(io::Error::other)
        });

        Ok(Self {
            descriptor_path,
            cancellation,
            task: Some(task),
        })
    }

    /// Run until the listener fails or the caller requests an orderly shutdown.
    pub async fn run_until<F>(mut self, shutdown: F) -> io::Result<()>
    where
        F: Future<Output = io::Result<()>>,
    {
        let mut task = self.task.take().expect("HTTP server task is present");
        tokio::pin!(shutdown);
        let result = tokio::select! {
            result = &mut task => join_result(result),
            signal = &mut shutdown => {
                self.cancellation.cancel();
                let task_result = join_result(task.await);
                signal.and(task_result)
            }
        };
        self.cancellation.cancel();
        let cleanup = remove_descriptor(&self.descriptor_path);
        result.and(cleanup)
    }

    pub async fn shutdown(self) -> io::Result<()> {
        self.run_until(async { Ok(()) }).await
    }
}

impl Drop for HttpMcpServer {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
        }
        let _ = remove_descriptor(&self.descriptor_path);
    }
}

fn validate_descriptor_path(path: &Path) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "HTTP descriptor path must be absolute",
        ));
    }
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "HTTP descriptor path has no parent directory",
        )
    })?;
    if !parent.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "HTTP descriptor parent directory does not exist",
        ));
    }
    Ok(())
}

async fn require_bearer(
    State(expected): State<BearerToken>,
    request: Request,
    next: Next,
) -> Response {
    if authorized(request.headers(), &expected.0) {
        next.run(request).await
    } else {
        unauthorized_response()
    }
}

fn authorized(headers: &axum::http::HeaderMap, expected: &str) -> bool {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let Some(value) = values.next() else {
        return false;
    };
    if values.next().is_some() {
        return false;
    }
    let Ok(value) = value.to_str() else {
        return false;
    };
    let Some((scheme, token)) = value.split_once(' ') else {
        return false;
    };
    scheme.eq_ignore_ascii_case("bearer") && constant_time_equal(token, expected)
}

fn unauthorized_response() -> Response {
    let mut response = (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        axum::http::HeaderValue::from_static("Bearer"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}

fn join_result(result: Result<io::Result<()>, tokio::task::JoinError>) -> io::Result<()> {
    result.map_err(io::Error::other)?
}

fn remove_descriptor(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{ConfigOrigin, parse_config},
        mcp::AgentSpeakServer,
    };
    use rmcp::{
        ServiceExt,
        transport::{
            StreamableHttpClientTransport,
            streamable_http_client::StreamableHttpClientTransportConfig,
        },
    };
    use std::time::Duration;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    const INERT_PROFILE: &str = include_str!("../tests/fixtures/inert-profile.toml");

    fn inert_server() -> AgentSpeakServer {
        let config =
            parse_config(INERT_PROFILE, Path::new("."), ConfigOrigin::BuiltInDefaults).unwrap();
        AgentSpeakServer::new(config).unwrap()
    }

    #[test]
    fn bearer_auth_accepts_one_exact_credential_and_rejects_ambiguous_headers() {
        let mut headers = axum::http::HeaderMap::new();
        assert!(!authorized(&headers, "secret"));

        headers.insert(header::AUTHORIZATION, "Bearer wrong".parse().unwrap());
        assert!(!authorized(&headers, "secret"));

        headers.insert(header::AUTHORIZATION, "bearer secret".parse().unwrap());
        assert!(authorized(&headers, "secret"));

        headers.append(header::AUTHORIZATION, "Bearer secret".parse().unwrap());
        assert!(!authorized(&headers, "secret"));
    }

    #[tokio::test]
    async fn streamable_http_requires_bearer_and_serves_the_policy_shaped_tools() {
        let directory = tempfile::tempdir().unwrap();
        let descriptor_path = directory.path().join("http.json");
        let agent = inert_server();
        let http = HttpMcpServer::start(&descriptor_path, agent.clone())
            .await
            .unwrap();
        let descriptor: HttpMcpDescriptor =
            serde_json::from_slice(&std::fs::read(&descriptor_path).unwrap()).unwrap();

        assert_eq!(descriptor.schema_version, DESCRIPTOR_SCHEMA_VERSION);
        assert_eq!(descriptor.transport, "streamable_http");
        assert!(descriptor.url.starts_with("http://127.0.0.1:"));
        assert!(descriptor.url.ends_with(MCP_PATH));
        assert_eq!(descriptor.authorization.scheme, "Bearer");
        assert_eq!(descriptor.authorization.token.len(), 64);
        assert!(!format!("{descriptor:?}").contains(&descriptor.authorization.token));
        #[cfg(unix)]
        assert_eq!(
            std::fs::metadata(&descriptor_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        let unauthorized = StreamableHttpClientTransport::from_uri(descriptor.url.clone());
        assert!(
            tokio::time::timeout(Duration::from_secs(2), ().serve(unauthorized))
                .await
                .unwrap()
                .is_err()
        );

        let authorized = StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(descriptor.url.clone())
                .auth_header(descriptor.authorization.token.clone()),
        );
        let client = ().serve(authorized).await.unwrap();
        let tools = client.list_all_tools().await.unwrap();
        assert_eq!(
            tools
                .into_iter()
                .map(|tool| tool.name.into_owned())
                .collect::<Vec<_>>(),
            [
                "cancel_playback",
                "get_audio_capabilities",
                "get_playback_status"
            ]
        );
        client.cancel().await.unwrap();

        http.shutdown().await.unwrap();
        assert!(!descriptor_path.exists());
        agent.shutdown().await.unwrap();
    }

    #[test]
    fn descriptor_path_must_be_absolute() {
        assert!(validate_descriptor_path(Path::new("http.json")).is_err());
    }
}
