use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::Arc,
    time::Duration,
};

use axum::{
    Router,
    extract::{Request, State},
    http::{StatusCode, header::AUTHORIZATION},
    middleware,
    middleware::Next,
    response::Response,
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::{
    net::TcpListener,
    sync::{Mutex, watch},
    task::{AbortHandle, JoinHandle},
};
use tokio_util::sync::CancellationToken;

use super::{TransportError, TransportOutcome, shutdown_signal};
use crate::{
    cli::HttpBindMode,
    http_policy::{self, HttpPolicy},
    server::WorkcellServer,
};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const SSE_KEEP_ALIVE: Duration = Duration::from_secs(15);
const SSE_RETRY: Duration = Duration::from_secs(3);
const MIN_TOKEN_BYTES: usize = 32;
const MAX_TOKEN_BYTES: usize = 4_096;

#[derive(Clone)]
pub struct HttpAuthentication(Arc<[u8; 32]>);

impl HttpAuthentication {
    pub fn new(token: &str) -> Result<Self, TransportError> {
        if token.len() < MIN_TOKEN_BYTES
            || token.len() > MAX_TOKEN_BYTES
            || token.trim() != token
            || token.chars().any(char::is_control)
        {
            return Err(TransportError::HttpAuthentication);
        }
        Ok(Self(Arc::new(Sha256::digest(token.as_bytes()).into())))
    }

    fn accepts(&self, candidate: &str) -> bool {
        let digest: [u8; 32] = Sha256::digest(candidate.as_bytes()).into();
        self.0.as_ref().ct_eq(&digest).into()
    }
}

#[derive(Clone)]
pub struct HttpConfiguration {
    pub bind_mode: HttpBindMode,
    pub allowed_hosts: Vec<String>,
    pub authentication: Option<HttpAuthentication>,
}

impl HttpConfiguration {
    pub fn validate(&self) -> Result<(), TransportError> {
        if self.bind_mode == HttpBindMode::Container && self.authentication.is_none() {
            return Err(TransportError::HttpAuthenticationRequired);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ServeState {
    Running,
    Completed,
    Failed,
    Forced,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShutdownOutcome {
    Completed,
    Failed,
    TimedOut,
    AlreadyStopped,
}

struct HttpServerInner {
    shutdown: CancellationToken,
    task: Mutex<Option<JoinHandle<Result<(), std::io::Error>>>>,
    abort: AbortHandle,
    state_tx: watch::Sender<ServeState>,
}

#[derive(Clone)]
pub struct HttpServer {
    address: SocketAddr,
    bind_mode: HttpBindMode,
    inner: Arc<HttpServerInner>,
}

impl HttpServer {
    pub async fn start(
        server: WorkcellServer,
        port: u16,
        configuration: HttpConfiguration,
    ) -> Result<Self, TransportError> {
        configuration.validate()?;
        let modern_only = server.modern_only();
        let requested = SocketAddrV4::new(
            match configuration.bind_mode {
                HttpBindMode::Loopback => Ipv4Addr::LOCALHOST,
                HttpBindMode::Container => Ipv4Addr::UNSPECIFIED,
            },
            port,
        );
        let listener = TcpListener::bind(requested)
            .await
            .map_err(|_| TransportError::HttpBind)?;
        let address = listener
            .local_addr()
            .map_err(|_| TransportError::HttpBind)?;
        let shutdown = CancellationToken::new();
        let transport_config = StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(false)
            .with_sse_keep_alive(Some(SSE_KEEP_ALIVE))
            .with_sse_retry(Some(SSE_RETRY))
            .with_cancellation_token(shutdown.child_token())
            .with_allowed_hosts(configuration.allowed_hosts.clone());
        let service: StreamableHttpService<WorkcellServer, NeverSessionManager> =
            StreamableHttpService::new(
                move || Ok(server.clone()),
                Arc::new(NeverSessionManager::default()),
                transport_config,
            );
        let router = Router::new()
            .nest_service(http_policy::ENDPOINT_PATH, service)
            .layer(middleware::from_fn_with_state(
                configuration.authentication,
                authenticate,
            ))
            .layer(middleware::from_fn_with_state(
                HttpPolicy::new(configuration.allowed_hosts, modern_only),
                http_policy::enforce,
            ));
        let (state_tx, _state_rx) = watch::channel(ServeState::Running);
        let task_shutdown = shutdown.clone();
        let task_state = state_tx.clone();
        let task = tokio::spawn(async move {
            let result = axum::serve(listener, router)
                .with_graceful_shutdown(task_shutdown.cancelled_owned())
                .await;
            task_state.send_replace(if result.is_ok() {
                ServeState::Completed
            } else {
                ServeState::Failed
            });
            result
        });
        let abort = task.abort_handle();
        Ok(Self {
            address,
            bind_mode: configuration.bind_mode,
            inner: Arc::new(HttpServerInner {
                shutdown,
                task: Mutex::new(Some(task)),
                abort,
                state_tx,
            }),
        })
    }

    #[must_use]
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    #[must_use]
    pub const fn listen_host(&self) -> &'static str {
        match self.bind_mode {
            HttpBindMode::Loopback => "127.0.0.1",
            HttpBindMode::Container => "0.0.0.0",
        }
    }

    pub async fn wait(&self) -> Result<(), TransportError> {
        let mut state = self.inner.state_tx.subscribe();
        loop {
            match *state.borrow_and_update() {
                ServeState::Running => {}
                ServeState::Completed | ServeState::Forced => return Ok(()),
                ServeState::Failed => return Err(TransportError::HttpService),
            }
            state
                .changed()
                .await
                .map_err(|_| TransportError::HttpService)?;
        }
    }

    pub async fn shutdown(&self) -> ShutdownOutcome {
        self.inner.shutdown.cancel();
        let task = self.inner.task.lock().await.take();
        let Some(mut task) = task else {
            let mut state = self.inner.state_tx.subscribe();
            return match tokio::time::timeout(SHUTDOWN_TIMEOUT, async {
                loop {
                    let current = *state.borrow_and_update();
                    match current {
                        ServeState::Running => {
                            state.changed().await.map_err(|_| ShutdownOutcome::Failed)?
                        }
                        ServeState::Failed => return Err(ShutdownOutcome::Failed),
                        ServeState::Completed | ServeState::Forced => return Ok(()),
                    }
                }
            })
            .await
            {
                Ok(Ok(())) => ShutdownOutcome::AlreadyStopped,
                Ok(Err(outcome)) => outcome,
                Err(_) => ShutdownOutcome::TimedOut,
            };
        };
        match tokio::time::timeout(SHUTDOWN_TIMEOUT, &mut task).await {
            Ok(Ok(Ok(()))) => ShutdownOutcome::Completed,
            Ok(Ok(Err(_))) | Ok(Err(_)) => ShutdownOutcome::Failed,
            Err(_) => {
                task.abort();
                let _ = task.await;
                self.inner.state_tx.send_replace(ServeState::Forced);
                ShutdownOutcome::TimedOut
            }
        }
    }

    pub async fn force(&self) {
        self.inner.shutdown.cancel();
        self.inner.abort.abort();
        self.inner.state_tx.send_replace(ServeState::Forced);
    }
}

async fn authenticate(
    State(authentication): State<Option<HttpAuthentication>>,
    mut request: Request,
    next: Next,
) -> Response {
    let authorization = request.headers().get(AUTHORIZATION);
    match (authentication, authorization) {
        (None, None) => next.run(request).await,
        (None, Some(_)) => http_policy::policy_error(
            StatusCode::BAD_REQUEST,
            -32_000,
            "Authorization is not configured.",
        ),
        (Some(_), None) => http_policy::policy_error(
            StatusCode::UNAUTHORIZED,
            -32_000,
            "Bearer authentication is required.",
        ),
        (Some(authentication), Some(value)) => {
            let accepted = value
                .to_str()
                .ok()
                .and_then(|value| value.strip_prefix("Bearer "))
                .is_some_and(|token| authentication.accepts(token));
            if accepted {
                request.headers_mut().remove(AUTHORIZATION);
                next.run(request).await
            } else {
                http_policy::policy_error(
                    StatusCode::UNAUTHORIZED,
                    -32_000,
                    "Bearer authentication failed.",
                )
            }
        }
    }
}

pub async fn run(
    server: WorkcellServer,
    port: u16,
    configuration: HttpConfiguration,
) -> Result<TransportOutcome, TransportError> {
    let authenticated = configuration.authentication.is_some();
    let service = HttpServer::start(server, port, configuration).await?;
    tracing::info!(
        operation = "mcp.started",
        transport = "http",
        authenticated,
        listen_host = service.listen_host(),
        port = service.address().port(),
        "MCP server listening"
    );
    println!(
        "{}",
        serde_json::json!({
            "kind": "workcell.mcp.ready",
            "version": 1,
            "listenHost": service.listen_host(),
            "port": service.address().port(),
            "mcpPath": http_policy::ENDPOINT_PATH,
            "authenticated": authenticated,
        })
    );
    wait_for_shutdown(service).await
}

async fn wait_for_shutdown(service: HttpServer) -> Result<TransportOutcome, TransportError> {
    tokio::select! {
        result = service.wait() => {
            let shutdown = service.shutdown().await;
            result?;
            if shutdown == ShutdownOutcome::Failed {
                return Err(TransportError::HttpService);
            }
            Ok(TransportOutcome::PeerClosed)
        }
        signal = shutdown_signal() => {
            signal?;
            tracing::info!(operation = "mcp.shutdown.started", "MCP server shutting down");
            let shutdown = service.shutdown();
            tokio::pin!(shutdown);
            tokio::select! {
                outcome = &mut shutdown => match outcome {
                    ShutdownOutcome::Completed | ShutdownOutcome::AlreadyStopped => {
                        Ok(TransportOutcome::ShutdownCompleted)
                    }
                    ShutdownOutcome::TimedOut => Ok(TransportOutcome::ShutdownTimedOut),
                    ShutdownOutcome::Failed => Err(TransportError::HttpService),
                },
                second = shutdown_signal() => {
                    second?;
                    service.force().await;
                    Ok(TransportOutcome::ShutdownForced)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_validation_is_bounded_and_redacted() {
        assert!(HttpAuthentication::new("short").is_err());
        let authentication = HttpAuthentication::new(&"a".repeat(32)).unwrap();
        assert!(authentication.accepts(&"a".repeat(32)));
        assert!(!authentication.accepts(&"b".repeat(32)));
    }

    #[test]
    fn container_bind_requires_authentication() {
        let configuration = HttpConfiguration {
            bind_mode: HttpBindMode::Container,
            allowed_hosts: vec!["127.0.0.1".into()],
            authentication: None,
        };
        assert_eq!(
            configuration.validate().unwrap_err(),
            TransportError::HttpAuthenticationRequired
        );
    }
}
