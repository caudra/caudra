use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::hash::Hash;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::SHELL_EXECUTION_TIMEOUT;
use crate::transfer::PrivateStaging;
use async_trait::async_trait;
use caudra_config::workcell::{RemoteWorkcellSelection, WorkcellEndpoint};
use caudra_storage::auth::{WorkcellCredential, WorkcellCredentialName};
use caudra_storage::id::CaudraId;
use caudra_storage::remote_operation_journal::{
    RemoteOperationJournal, RemoteOperationRecord, RemoteOperationReservation,
    RemoteOperationState, RequestDigest,
};
use caudra_storage::workspace_binding::StoredWorkspaceBinding;
use caudra_workspace::{
    AuthenticatedPrincipalId, AuthorityIdentity, ByteContent, CancellationResult, CheckpointId,
    CollectionRevision, ContinuationToken, CwdHandle, DirectoryNavigation, ExecRequest, ListPage,
    ListRequest, Mutation, MutationCondition, MutationEntryResult, MutationKind, MutationRequest,
    MutationResult, OperationError, OperationHandle, OperationId, OperationPhase,
    OperationProgress, OperationProgressKind, OperationState, OperationStatus, PreparedScmMutation,
    PreparedSnapshotOperation, PreparedToolCall, ProjectAsset, ProjectAssetContent,
    ProjectAssetKind, ProjectAssetManifest, ProjectAssetTrust, ProjectIdentity, ProjectKey,
    ReadBytesRequest, ReadTextRequest, ReleaseResult, ResolvedWorkspaceDirectory, ResourceId,
    ResourceKind, ResourceRevision, ResourceScope, ResourceSelector, RestoreId, ScmChangeKind,
    ScmCommit, ScmDiffLine, ScmDiffLineKind, ScmDiffPage, ScmDiffRequest, ScmDiffTarget,
    ScmDiscoverRequest, ScmDiscoverResult, ScmLogPage, ScmLogRequest, ScmMutation,
    ScmMutationPreview, ScmMutationResult, ScmReadSidePage, ScmReadSideRequest, ScmRepository,
    ScmRepositoryRevisions, ScmRevision, ScmSide, ScmStatusEntry, ScmStatusPage, ScmStatusRequest,
    SearchHit, SearchPage, SearchRequest, SearchScanCounts, SequenceMetadata, SessionBindingId,
    SessionWorkspaceBinding, SnapshotCaptureLimits, SnapshotCaptureRequest, SnapshotCaptureResult,
    SnapshotChange, SnapshotChangeCounts, SnapshotChangeKind, SnapshotCleanupPreview,
    SnapshotCleanupResult, SnapshotEntryKind, SnapshotFile, SnapshotId, SnapshotInspectPage,
    SnapshotInspectRequest, SnapshotOperationPreview, SnapshotOperationResult,
    SnapshotRestorePreview, SnapshotRestoreState, SnapshotRestoreStatus, SnapshotSkipReason,
    SnapshotSkipped, SnapshotSkippedEntry, SnapshotState, SnapshotSummary, SnapshotUnrevertPreview,
    SourceTrustAnchor, TextContent, ToolPrepareRequest, WatchCloseResult, WatchCursor,
    WatchEventPage, WatchOpenRequest, WatchPollRequest, WatchPollState, WatchResyncReason,
    WatchSubscription, WatchSubscriptionId, WorkspaceAssetService, WorkspaceCapabilities,
    WorkspaceCapability, WorkspaceControlCommand, WorkspaceControlService, WorkspaceCursor,
    WorkspaceError, WorkspaceEvent, WorkspaceEventKind, WorkspaceExecService, WorkspaceHandle,
    WorkspaceMutationService, WorkspacePath, WorkspaceReadService, WorkspaceResource,
    WorkspaceScmMutationService, WorkspaceScmReadService, WorkspaceSearchService,
    WorkspaceServices, WorkspaceSnapshotMutationService, WorkspaceSnapshotReadService,
    WorkspaceToolService, WorkspaceWatchService, WriteContent,
};
use caudra_workspace::{PreparedTransferPublication, WorkspaceTransferService};
use event_listener::{Event, EventListener};
use flume::{Receiver, Sender};
use futures_lite::future;
use futures_lite::io::{AsyncReadExt, Cursor};
use isahc::config::{Configurable, RedirectPolicy, VersionNegotiation};
use isahc::http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use isahc::http::{Method, Request, StatusCode};
use isahc::{AsyncBody, HttpClient, ResponseExt};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;
use tracing::warn;
use url::{Host, Url};
use workcell::host_contract as contract;
use workcell::{CatalogRevision, OwnedToolSpec, ToolAnnotations, ToolManifest};

const PROTOCOL_VERSION: &str = "2026-07-28";
const PROTOCOL_HEADER: &str = "mcp-protocol-version";
const JSON_CONTENT_TYPE: &str = "application/json";
const SSE_CONTENT_TYPE: &str = "text/event-stream";
const ACCEPT_VALUE: &str = "application/json, text/event-stream";
const OCTET_STREAM: &str = "application/octet-stream";
const MAX_HTTP_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_SSE_EVENT_BYTES: usize = 2 * 1024 * 1024;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const EVENT_QUEUE_CAPACITY: usize = 256;
const CONNECTION_CONNECTED: u8 = 0;
const CONNECTION_DISCONNECTED: u8 = 1;
const CONNECTION_RECONNECTING: u8 = 2;
const RESOURCE_NAMESPACE_VERSION: &str = "v1";
const NO_SYMBOLIC_REASON: &str = "<none>";
const MAX_RPC_DIAGNOSTIC_BYTES: usize = 128;
/// A limit name travels into user-facing text, so anything longer or not a
/// plain identifier is host prose and stays at the transport boundary.
const MAX_LIMIT_NAME_BYTES: usize = 32;
/// The host's token for a resource that changed after it was prepared. On a
/// failed file mutation it also means nothing was published.
pub(crate) const STALE_RESOURCE_CODE: &str = "stale_resource";
const TOOL_ERROR_CODE: &str = "tool_error";
const RESERVATION_WAIT: Duration = Duration::from_secs(10);
/// Caps a single park so a slot freed by the clock rather than by a release is
/// still noticed: expiry notifies nobody.
const RESERVATION_POLL: Duration = Duration::from_millis(250);
const PATH_STYLE: &str = "root-relative-posix";
const TOOL_MANIFEST_VERSION: &str = "v2";
const JSON_SCHEMA_VERSION: &str = "http://json-schema.org/draft-07/schema#";
const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(600);
const WATCH_INITIAL_SEQUENCE: u64 = 1;
const CANONICAL_OPERATION_PREFIX: &str = "canonical:";
const WORKSPACE_MUTATION_KIND: &str = "workspace_mutation";
const DIRECT_EXEC_KIND: &str = "direct_exec";
const SCM_MUTATION_KIND: &str = "scm_mutation";
const SNAPSHOT_RESTORE_KIND: &str = "snapshot_restore";
const SNAPSHOT_UNREVERT_KIND: &str = "snapshot_unrevert";
const SNAPSHOT_CLEANUP_KIND: &str = "snapshot_cleanup";
const MAX_CONTROL_OPERATIONS: usize = 32;
const UNREACHABLE_OPERATIONS_HEADING: &str = "Operations from an earlier workspace generation";
const UNREACHABLE_OPERATIONS_REMEDY: &str = "The host that ran them is gone, so they cannot be reconciled; acknowledge each once you have checked its effects.";
const SHELL_CONTRACT_ID: &str = "shell.execution.v1";

mod snapshot;
mod transfer;

#[derive(Clone)]
pub struct NamedBearerCredential {
    name: WorkcellCredentialName,
    bearer: Arc<str>,
}

impl NamedBearerCredential {
    pub fn new(name: WorkcellCredentialName, credential: WorkcellCredential) -> Self {
        Self {
            name,
            bearer: Arc::from(credential.bearer_token()),
        }
    }

    pub fn name(&self) -> &WorkcellCredentialName {
        &self.name
    }
}

impl fmt::Debug for NamedBearerCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NamedBearerCredential")
            .field("name", &self.name)
            .field("bearer", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteConnectionStatus {
    Connected,
    Disconnected,
    Reconnecting,
}

#[derive(Clone, PartialEq)]
pub struct RemoteEvent {
    pub method: String,
    pub params: Option<Value>,
}

impl fmt::Debug for RemoteEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteEvent")
            .field("method", &self.method)
            .field("params", &self.params.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RemoteWorkcellError {
    #[error("remote Workcell transport is unavailable")]
    Transport,
    #[error("remote Workcell request timed out")]
    Timeout,
    #[error("remote Workcell request was cancelled")]
    Cancelled,
    #[error("remote Workcell authentication was rejected")]
    Authentication,
    #[error("remote Workcell credentials require HTTPS")]
    InsecureTransport,
    #[error("remote Workcell endpoint origin changed")]
    OriginMismatch,
    #[error("remote Workcell protocol response is invalid")]
    InvalidProtocol,
    #[error("remote Workcell identity does not match the configured identity")]
    IdentityMismatch,
    #[error("remote Workcell does not provide a compatible control plane")]
    CapabilityMismatch,
    #[error("remote Workcell canonical tool catalog is incompatible")]
    CatalogMismatch,
    #[error("remote Workcell resource is stale")]
    StaleResource,
    #[error("remote Workcell cursor is stale")]
    StaleCursor,
    #[error("remote Workcell operation conflicts with current state")]
    Conflict,
    #[error("remote Workcell is busy with another operation")]
    Busy,
    #[error("remote Workcell directory is not a repository")]
    NotRepository,
    #[error("remote Workcell watch backend is unavailable")]
    WatchUnavailable,
    #[error("remote Workcell operation was denied by policy")]
    PolicyDenied,
    #[error("remote Workcell binding does not match the requested authority")]
    BindingMismatch,
    #[error("remote Workcell {} limit was exceeded", .limit.as_deref().unwrap_or("request"))]
    LimitExceeded {
        limit: Option<String>,
        maximum: Option<u64>,
    },
    /// Snapshot refusals name their limit. The operation ledger's is the one
    /// quota that arrives unnamed.
    #[error("remote Workcell {} quota is exhausted", .limit.as_deref().unwrap_or("operation"))]
    QuotaExceeded {
        limit: Option<String>,
        maximum: Option<u64>,
    },
    #[error("remote Workcell entry is not a plain file, directory or symlink")]
    UnsupportedEntry,
    #[error("remote transfer digest or size does not match")]
    TransferIntegrity,
    #[error("remote transfer quota is exhausted")]
    TransferQuota,
    #[error("remote Workcell operation outcome is indeterminate")]
    Indeterminate,
    #[error("remote Workcell durable operation journal is unavailable")]
    JournalUnavailable,
    #[error("pending remote operation {operation_id} is recorded against a different project")]
    RecoveryBindingMismatch { operation_id: String },
    /// A refusal that arrived well formed and carried a reason this client has
    /// no mapping for. The host's prose stays at the transport boundary; the
    /// numeric and symbolic codes travel so the caller learns what was refused.
    #[error("remote Workcell refused the request with code {code}, reason {symbolic}")]
    UnmappedRefusal { code: i64, symbolic: String },
}

#[derive(Clone)]
pub struct RemotePreparedToolCall {
    pub prepared: PreparedToolCall,
    pub intent: contract::OperationIntent,
    pub binding: SessionWorkspaceBinding,
    pub cursor: WorkspaceCursor,
}

impl RemotePreparedToolCall {
    pub fn expires_within(&self, margin: Duration) -> bool {
        let margin = u64::try_from(margin.as_millis()).unwrap_or(u64::MAX);
        self.prepared
            .operation
            .expires_at_unix_ms
            .is_some_and(|expires| expires <= unix_millis().saturating_add(margin))
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum RemoteToolExecutionError {
    #[error("{0}")]
    BeforeDispatch(#[from] WorkspaceError),
    #[error("{0}")]
    PossiblyDispatched(WorkspaceError),
}

#[derive(Clone, Debug, PartialEq)]
pub struct RemoteToolResultEnvelope {
    pub model_output: String,
    pub structured_content: Value,
    pub is_error: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingRemoteOperation {
    pub operation_id: OperationId,
    pub operation_kind: String,
    pub state: RemoteOperationState,
    /// False when an earlier generation of the workspace recorded it: its host
    /// is gone, so only acknowledging it after inspecting the files remains.
    pub reachable: bool,
}

impl From<RemoteWorkcellError> for WorkspaceError {
    fn from(error: RemoteWorkcellError) -> Self {
        match error {
            RemoteWorkcellError::Timeout => Self::Transport {
                kind: caudra_workspace::TransportErrorKind::Timeout,
            },
            RemoteWorkcellError::Cancelled => Self::Cancelled,
            RemoteWorkcellError::Authentication | RemoteWorkcellError::InsecureTransport => {
                Self::PermissionDenied
            }
            RemoteWorkcellError::StaleResource => Self::StaleCursor,
            RemoteWorkcellError::StaleCursor => Self::StaleCursor,
            RemoteWorkcellError::Conflict => Self::Conflict,
            RemoteWorkcellError::Busy => Self::Busy,
            RemoteWorkcellError::NotRepository => Self::NotRepository,
            RemoteWorkcellError::WatchUnavailable => Self::WatchUnavailable,
            RemoteWorkcellError::PolicyDenied => Self::PolicyDenied,
            RemoteWorkcellError::BindingMismatch => Self::IdentityMismatch,
            RemoteWorkcellError::LimitExceeded { limit, maximum } => {
                Self::LimitExceeded { limit, maximum }
            }
            RemoteWorkcellError::QuotaExceeded { limit, maximum } => {
                Self::QuotaExceeded { limit, maximum }
            }
            RemoteWorkcellError::UnsupportedEntry => Self::UnsupportedEntry,
            RemoteWorkcellError::TransferIntegrity => Self::TransferIntegrity,
            RemoteWorkcellError::TransferQuota => Self::TransferQuota,
            RemoteWorkcellError::Indeterminate => Self::IndeterminateOutcome,
            RemoteWorkcellError::JournalUnavailable => Self::Unavailable,
            RemoteWorkcellError::RecoveryBindingMismatch { .. } => Self::IdentityMismatch,
            RemoteWorkcellError::IdentityMismatch | RemoteWorkcellError::OriginMismatch => {
                Self::IdentityMismatch
            }
            RemoteWorkcellError::InvalidProtocol
            | RemoteWorkcellError::CapabilityMismatch
            | RemoteWorkcellError::CatalogMismatch => Self::InvalidResponse {
                violation: caudra_workspace::InvalidResponseKind::Malformed,
            },
            RemoteWorkcellError::UnmappedRefusal { code, symbolic } => {
                Self::Refused { code, symbolic }
            }
            RemoteWorkcellError::Transport => Self::Transport {
                kind: caudra_workspace::TransportErrorKind::Disconnected,
            },
        }
    }
}

#[derive(Serialize)]
struct JsonRpcRequest<'a> {
    jsonrpc: &'static str,
    id: u64,
    method: &'a str,
    params: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonRpcError {
    code: i64,
    message: String,
    #[serde(default)]
    data: Option<Value>,
}

struct RpcCallContext<'a> {
    id: u64,
    method: &'a str,
    started: Instant,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ToolAnnotationsWire {
    read_only_hint: Option<bool>,
    destructive_hint: Option<bool>,
    idempotent_hint: Option<bool>,
    open_world_hint: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolWire {
    name: String,
    title: Option<String>,
    description: String,
    input_schema: Map<String, Value>,
    output_schema: Option<Map<String, Value>>,
    annotations: ToolAnnotationsWire,
    #[serde(default)]
    icons: Option<Value>,
    #[serde(rename = "_meta")]
    meta: Map<String, Value>,
    #[serde(flatten)]
    _extras: Map<String, Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolListWire {
    result_type: String,
    #[serde(rename = "_meta", default)]
    meta: Option<Map<String, Value>>,
    next_cursor: Option<String>,
    ttl_ms: u64,
    cache_scope: String,
    tools: Vec<ToolWire>,
    #[serde(flatten)]
    _extras: Map<String, Value>,
}

#[derive(Debug)]
struct RequestFailure {
    error: RemoteWorkcellError,
    dispatched: bool,
}

struct RemoteTransport {
    endpoint: Url,
    client: HttpClient,
    bearer: Option<Arc<str>>,
    next_id: AtomicU64,
    status: AtomicU8,
    events: Sender<RemoteEvent>,
}

impl RemoteTransport {
    fn new(
        endpoint: &WorkcellEndpoint,
        bearer: Option<Arc<str>>,
    ) -> Result<(Self, Receiver<RemoteEvent>), RemoteWorkcellError> {
        let endpoint_url = endpoint.as_url();
        if endpoint.is_loopback() && !numeric_loopback(endpoint_url) {
            return Err(RemoteWorkcellError::InsecureTransport);
        }
        // A bearer may ride plaintext only to a numeric loopback literal. That
        // request never reaches a network interface, and the proxy is bypassed
        // below, which closes the one path that could divert it off the host. A
        // name like `localhost` does not qualify, because resolution can point
        // it somewhere else.
        if endpoint_url.scheme() == "http" && !numeric_loopback(endpoint_url) {
            return Err(RemoteWorkcellError::InsecureTransport);
        }
        let mut builder = HttpClient::builder()
            .redirect_policy(RedirectPolicy::None)
            .version_negotiation(VersionNegotiation::http11())
            .timeout(DEFAULT_TIMEOUT);
        if numeric_loopback(endpoint_url) {
            builder = builder.proxy(None);
        }
        let client = builder
            .build()
            .map_err(|_| RemoteWorkcellError::Transport)?;
        let (events, receiver) = flume::bounded(EVENT_QUEUE_CAPACITY);
        Ok((
            Self {
                endpoint: endpoint_url.clone(),
                client,
                bearer,
                next_id: AtomicU64::new(1),
                status: AtomicU8::new(CONNECTION_CONNECTED),
                events,
            },
            receiver,
        ))
    }

    fn status(&self) -> RemoteConnectionStatus {
        match self.status.load(Ordering::Acquire) {
            CONNECTION_CONNECTED => RemoteConnectionStatus::Connected,
            CONNECTION_RECONNECTING => RemoteConnectionStatus::Reconnecting,
            _ => RemoteConnectionStatus::Disconnected,
        }
    }

    async fn request(
        &self,
        method: &str,
        params: Value,
        max_request_bytes: u64,
        cancellation: &CancellationToken,
    ) -> Result<Value, RemoteWorkcellError> {
        self.request_tracked(method, params, max_request_bytes, cancellation)
            .await
            .map_err(|failure| failure.error)
    }

    async fn request_tracked(
        &self,
        method: &str,
        params: Value,
        max_request_bytes: u64,
        cancellation: &CancellationToken,
    ) -> Result<Value, RequestFailure> {
        self.request_tracked_with_dispatch(
            method,
            params,
            max_request_bytes,
            cancellation,
            None,
            || Ok(()),
        )
        .await
    }

    async fn request_tracked_with_dispatch<F>(
        &self,
        method: &str,
        mut params: Value,
        max_request_bytes: u64,
        cancellation: &CancellationToken,
        timeout_override: Option<Duration>,
        before_dispatch: F,
    ) -> Result<Value, RequestFailure>
    where
        F: FnOnce() -> Result<(), RemoteWorkcellError>,
    {
        if cancellation.is_cancelled() {
            return Err(RequestFailure {
                error: RemoteWorkcellError::Cancelled,
                dispatched: false,
            });
        }
        let Some(params) = params.as_object_mut() else {
            return Err(RequestFailure {
                error: RemoteWorkcellError::InvalidProtocol,
                dispatched: false,
            });
        };
        params.insert("_meta".to_owned(), request_metadata());
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = serde_json::to_vec(&JsonRpcRequest {
            jsonrpc: "2.0",
            id,
            method,
            params: Value::Object(params.clone()),
        })
        .map_err(|_| RequestFailure {
            error: RemoteWorkcellError::InvalidProtocol,
            dispatched: false,
        })?;
        let limit = usize::try_from(max_request_bytes)
            .unwrap_or(usize::MAX)
            .min(MAX_HTTP_RESPONSE_BYTES);
        if limit == 0 || body.len() > limit {
            return Err(RequestFailure {
                error: RemoteWorkcellError::InvalidProtocol,
                dispatched: false,
            });
        }
        let dispatched = AtomicBool::new(false);
        let operation = self.send(
            method,
            body,
            id,
            &dispatched,
            timeout_override,
            before_dispatch,
        );
        let cancelled = async {
            cancellation.cancelled().await;
            Err(RemoteWorkcellError::Cancelled)
        };
        future::race(operation, cancelled)
            .await
            .map_err(|error| RequestFailure {
                error,
                dispatched: dispatched.load(Ordering::Acquire),
            })
    }

    async fn send<F>(
        &self,
        method: &str,
        body: Vec<u8>,
        id: u64,
        dispatched: &AtomicBool,
        timeout_override: Option<Duration>,
        before_dispatch: F,
    ) -> Result<Value, RemoteWorkcellError>
    where
        F: FnOnce() -> Result<(), RemoteWorkcellError>,
    {
        let context = RpcCallContext {
            id,
            method,
            started: Instant::now(),
        };
        let mut builder = Request::builder()
            .method(Method::POST)
            .uri(self.endpoint.as_str())
            .header(CONTENT_TYPE, JSON_CONTENT_TYPE)
            .header(ACCEPT, ACCEPT_VALUE)
            .header("mcp-method", method)
            .header(PROTOCOL_HEADER, PROTOCOL_VERSION);
        // The server answers an execution request only once the command has
        // finished, and dropping the request cancels it, so the client's own
        // deadline has to outlast what it asked the command to be allowed.
        if let Some(timeout) = timeout_override {
            builder = builder.timeout(timeout);
        }
        if let Some(bearer) = &self.bearer {
            builder = builder.header(AUTHORIZATION, format!("Bearer {bearer}"));
        }
        // A reused connection can die after dispatch. Libcurl retries rewindable
        // bodies even for POST; recovery must use operation status, not replay.
        let length = body.len() as u64;
        let request = builder
            .body(AsyncBody::from_reader_sized(Cursor::new(body), length))
            .map_err(|_| RemoteWorkcellError::InvalidProtocol)?;
        before_dispatch()?;
        dispatched.store(true, Ordering::Release);
        let mut response = self.client.send_async(request).await.map_err(|error| {
            self.status
                .store(CONNECTION_DISCONNECTED, Ordering::Release);
            if error.is_timeout() {
                RemoteWorkcellError::Timeout
            } else {
                RemoteWorkcellError::Transport
            }
        })?;
        if !same_origin(&self.endpoint, response.effective_uri()) {
            self.status
                .store(CONNECTION_DISCONNECTED, Ordering::Release);
            return Err(RemoteWorkcellError::OriginMismatch);
        }
        if matches!(
            response.status(),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ) {
            return Err(RemoteWorkcellError::Authentication);
        }
        if !response.status().is_success() && response.status() != StatusCode::BAD_REQUEST {
            return Err(RemoteWorkcellError::Transport);
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next());
        let result = match content_type {
            Some(SSE_CONTENT_TYPE) => self.read_sse(&mut response, &context).await,
            Some(JSON_CONTENT_TYPE) => self.read_json(&mut response, &context).await,
            _ => Err(RemoteWorkcellError::InvalidProtocol),
        };
        match result {
            Ok(result) => {
                self.status.store(CONNECTION_CONNECTED, Ordering::Release);
                Ok(result)
            }
            Err(error) => {
                if matches!(
                    error,
                    RemoteWorkcellError::Transport | RemoteWorkcellError::Timeout
                ) {
                    self.status
                        .store(CONNECTION_DISCONNECTED, Ordering::Release);
                }
                Err(error)
            }
        }
    }

    async fn read_json(
        &self,
        response: &mut isahc::Response<isahc::AsyncBody>,
        context: &RpcCallContext<'_>,
    ) -> Result<Value, RemoteWorkcellError> {
        let bytes = read_bounded(response.body_mut(), MAX_HTTP_RESPONSE_BYTES).await?;
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|_| RemoteWorkcellError::InvalidProtocol)?;
        self.find_response(value, context)
    }

    async fn read_sse(
        &self,
        response: &mut isahc::Response<isahc::AsyncBody>,
        context: &RpcCallContext<'_>,
    ) -> Result<Value, RemoteWorkcellError> {
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 8192];
        let mut total = 0_usize;
        loop {
            let read = response
                .body_mut()
                .read(&mut chunk)
                .await
                .map_err(|_| RemoteWorkcellError::Transport)?;
            if read == 0 {
                break;
            }
            total = total.saturating_add(read);
            if total > MAX_HTTP_RESPONSE_BYTES {
                return Err(RemoteWorkcellError::InvalidProtocol);
            }
            buffer.extend_from_slice(&chunk[..read]);
            while let Some(end) = sse_event_end(&buffer) {
                if end > MAX_SSE_EVENT_BYTES {
                    return Err(RemoteWorkcellError::InvalidProtocol);
                }
                let result = self.parse_sse_event(&buffer[..end], context)?;
                buffer.drain(..end);
                if let Some(result) = result {
                    return Ok(result);
                }
            }
            if buffer.len() > MAX_SSE_EVENT_BYTES {
                return Err(RemoteWorkcellError::InvalidProtocol);
            }
        }
        if buffer.len() > MAX_SSE_EVENT_BYTES {
            return Err(RemoteWorkcellError::InvalidProtocol);
        }
        if !buffer.is_empty()
            && let Some(result) = self.parse_sse_event(&buffer, context)?
        {
            return Ok(result);
        }
        Err(RemoteWorkcellError::InvalidProtocol)
    }

    fn parse_sse_event(
        &self,
        event: &[u8],
        context: &RpcCallContext<'_>,
    ) -> Result<Option<Value>, RemoteWorkcellError> {
        let text = std::str::from_utf8(event).map_err(|_| RemoteWorkcellError::InvalidProtocol)?;
        let data = text
            .lines()
            .filter_map(|line| {
                line.strip_prefix("data:")
                    .map(|data| data.strip_prefix(' ').unwrap_or(data))
            })
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() {
            return Ok(None);
        }
        let value =
            serde_json::from_str(&data).map_err(|_| RemoteWorkcellError::InvalidProtocol)?;
        self.handle_message(value, context)
    }

    fn find_response(
        &self,
        value: Value,
        context: &RpcCallContext<'_>,
    ) -> Result<Value, RemoteWorkcellError> {
        let messages = match value {
            Value::Array(messages) if !messages.is_empty() => messages,
            Value::Array(_) => return Err(RemoteWorkcellError::InvalidProtocol),
            message => vec![message],
        };
        let mut response = None;
        for message in messages {
            if let Some(value) = self.handle_message(message, context)?
                && response.replace(value).is_some()
            {
                return Err(RemoteWorkcellError::InvalidProtocol);
            }
        }
        response.ok_or(RemoteWorkcellError::InvalidProtocol)
    }

    fn handle_message(
        &self,
        message: Value,
        context: &RpcCallContext<'_>,
    ) -> Result<Option<Value>, RemoteWorkcellError> {
        let object = message
            .as_object()
            .ok_or(RemoteWorkcellError::InvalidProtocol)?;
        if object.get("jsonrpc") != Some(&Value::String("2.0".to_owned())) {
            return Err(RemoteWorkcellError::InvalidProtocol);
        }
        let has_method = object.contains_key("method");
        let has_result = object.contains_key("result");
        let has_error = object.contains_key("error");
        let has_id = object.contains_key("id");
        if has_method {
            if has_id
                || has_result
                || has_error
                || object
                    .keys()
                    .any(|key| !matches!(key.as_str(), "jsonrpc" | "method" | "params"))
            {
                return Err(RemoteWorkcellError::InvalidProtocol);
            }
            let method = object
                .get("method")
                .and_then(Value::as_str)
                .filter(|method| !method.is_empty())
                .ok_or(RemoteWorkcellError::InvalidProtocol)?;
            if object
                .get("params")
                .is_some_and(|params| !params.is_object() && !params.is_array())
            {
                return Err(RemoteWorkcellError::InvalidProtocol);
            }
            let _ = self.events.try_send(RemoteEvent {
                method: method.to_owned(),
                params: object.get("params").cloned(),
            });
            return Ok(None);
        }
        if !has_id
            || has_result == has_error
            || object.contains_key("params")
            || object
                .keys()
                .any(|key| !matches!(key.as_str(), "jsonrpc" | "id" | "result" | "error"))
        {
            return Err(RemoteWorkcellError::InvalidProtocol);
        }
        if object.get("id").and_then(Value::as_u64) != Some(context.id) {
            return Err(RemoteWorkcellError::InvalidProtocol);
        }
        if has_error {
            let error: JsonRpcError = serde_json::from_value(
                object
                    .get("error")
                    .cloned()
                    .ok_or(RemoteWorkcellError::InvalidProtocol)?,
            )
            .map_err(|_| RemoteWorkcellError::InvalidProtocol)?;
            if error.message.is_empty() {
                return Err(RemoteWorkcellError::InvalidProtocol);
            }
            let mapped = map_rpc_error(&error);
            let symbolic =
                rpc_diagnostic_token(error.data.as_ref().and_then(|data| data.get("code")));
            if matches!(
                mapped,
                RemoteWorkcellError::UnmappedRefusal { .. } | RemoteWorkcellError::WatchUnavailable
            ) && symbolic != Some(snapshot::NOT_FOUND)
            {
                let phase = error
                    .data
                    .as_ref()
                    .and_then(|data| data.get("phase"))
                    .and_then(Value::as_str)
                    .filter(|phase| matches!(*phase, "initialize" | "register"));
                let errno = error
                    .data
                    .as_ref()
                    .and_then(|data| data.get("rawOsError"))
                    .and_then(Value::as_i64);
                warn!(
                    method = context.method,
                    elapsed_ms = context.started.elapsed().as_millis() as u64,
                    code = error.code,
                    symbolic = symbolic.unwrap_or(NO_SYMBOLIC_REASON),
                    phase,
                    errno,
                    "remote Workcell request refused"
                );
            }
            return Err(mapped);
        }
        Ok(object.get("result").cloned())
    }
}

struct CacheEntry<V> {
    value: V,
    touched: Instant,
}

struct BoundedMap<K, V> {
    entries: HashMap<K, CacheEntry<V>>,
    limit: usize,
    ttl: Duration,
}

struct ResourceCache {
    by_id: HashMap<ResourceId, CacheEntry<WorkspacePath>>,
    by_path: HashMap<WorkspacePath, ResourceId>,
    by_touch: BTreeSet<(Instant, ResourceId)>,
    limit: usize,
    ttl: Duration,
    #[cfg(test)]
    maintenance_probes: usize,
}

impl ResourceCache {
    fn new(limit: usize, ttl: Duration) -> Self {
        Self {
            by_id: HashMap::new(),
            by_path: HashMap::new(),
            by_touch: BTreeSet::new(),
            limit: limit.max(1),
            ttl,
            #[cfg(test)]
            maintenance_probes: 0,
        }
    }

    fn oldest(&mut self) -> Option<&(Instant, ResourceId)> {
        #[cfg(test)]
        {
            self.maintenance_probes += 1;
        }
        self.by_touch.first()
    }

    fn expire(&mut self, now: Instant) {
        let ttl = self.ttl;
        while let Some((touched, id)) = self.oldest() {
            if now.duration_since(*touched) <= ttl {
                break;
            }
            let id = id.clone();
            self.remove_id(&id);
        }
    }

    fn insert(&mut self, id: ResourceId, path: WorkspacePath) {
        self.insert_at(id, path, Instant::now());
    }

    fn insert_at(&mut self, id: ResourceId, path: WorkspacePath, now: Instant) {
        self.expire(now);
        self.remove_id(&id);
        if let Some(previous) = self.by_path.get(&path).cloned() {
            self.remove_id(&previous);
        }
        while self.by_id.len() >= self.limit {
            let Some((_, oldest)) = self.oldest().cloned() else {
                break;
            };
            self.remove_id(&oldest);
        }
        self.by_touch.insert((now, id.clone()));
        self.by_path.insert(path.clone(), id.clone());
        self.by_id.insert(
            id,
            CacheEntry {
                value: path,
                touched: now,
            },
        );
    }

    fn get_path(&mut self, id: &ResourceId) -> Option<&WorkspacePath> {
        self.get_path_at(id, Instant::now())
    }

    fn get_path_at(&mut self, id: &ResourceId, now: Instant) -> Option<&WorkspacePath> {
        self.expire(now);
        let entry = self.by_id.get_mut(id)?;
        self.by_touch.remove(&(entry.touched, id.clone()));
        entry.touched = now;
        self.by_touch.insert((now, id.clone()));
        Some(&entry.value)
    }

    fn remove_id(&mut self, id: &ResourceId) {
        if let Some(entry) = self.by_id.remove(id) {
            self.by_path.remove(&entry.value);
            self.by_touch.remove(&(entry.touched, id.clone()));
        }
    }

    fn remove_path(&mut self, path: &WorkspacePath) {
        if let Some(id) = self.by_path.get(path).cloned() {
            self.remove_id(&id);
        }
    }

    #[cfg(test)]
    fn id_for_path(&mut self, path: &WorkspacePath) -> Option<&ResourceId> {
        self.expire(Instant::now());
        self.by_path.get(path)
    }
}

impl<K, V> BoundedMap<K, V>
where
    K: Clone + Eq + Hash,
{
    fn new(limit: usize, ttl: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            limit: limit.max(1),
            ttl,
        }
    }

    fn expire(&mut self) {
        let now = Instant::now();
        self.entries
            .retain(|_, entry| now.duration_since(entry.touched) <= self.ttl);
    }

    fn make_room(&mut self) {
        self.expire();
        while self.entries.len() >= self.limit {
            let Some(key) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.touched)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.entries.remove(&key);
        }
    }

    fn insert(&mut self, key: K, value: V) {
        if !self.entries.contains_key(&key) {
            self.make_room();
        }
        self.entries.insert(
            key,
            CacheEntry {
                value,
                touched: Instant::now(),
            },
        );
    }

    fn get(&mut self, key: &K) -> Option<&V> {
        self.expire();
        let entry = self.entries.get_mut(key)?;
        entry.touched = Instant::now();
        Some(&entry.value)
    }

    #[cfg(test)]
    fn len(&mut self) -> usize {
        self.expire();
        self.entries.len()
    }
}

#[derive(Clone)]
struct CursorRecord {
    cursor: WorkspaceCursor,
    path: WorkspacePath,
}

struct CursorRegistry {
    records: HashMap<CwdHandle, CursorRecord>,
    limit: usize,
}

impl CursorRegistry {
    fn insert(&mut self, handle: CwdHandle, record: CursorRecord) -> Result<(), WorkspaceError> {
        if let Some(existing) = self.records.get(&handle) {
            return if existing.cursor == record.cursor && existing.path == record.path {
                Ok(())
            } else {
                Err(WorkspaceError::StaleCursor)
            };
        }
        if self.records.len() >= self.limit {
            return Err(WorkspaceError::Unavailable);
        }
        self.records.insert(handle, record);
        Ok(())
    }
}

#[derive(Clone)]
struct WatchRecord {
    workspace_cursor: WorkspaceCursor,
    cursor: WatchCursor,
    root: WorkspacePath,
    recursive: bool,
}

struct WatchRegistry {
    records: HashMap<WatchSubscriptionId, CacheEntry<WatchRecord>>,
    limit: usize,
    ttl: Duration,
}

impl WatchRegistry {
    fn new(limit: usize, ttl: Duration) -> Self {
        Self {
            records: HashMap::new(),
            limit: limit.max(1),
            ttl,
        }
    }

    fn insert(&mut self, id: WatchSubscriptionId, record: WatchRecord) {
        if !self.records.contains_key(&id)
            && self.records.len() >= self.limit
            && let Some(oldest) = self
                .records
                .iter()
                .min_by_key(|(_, entry)| entry.touched)
                .map(|(id, _)| id.clone())
        {
            self.records.remove(&oldest);
        }
        self.records.insert(
            id,
            CacheEntry {
                value: record,
                touched: Instant::now(),
            },
        );
    }
}

#[derive(Clone)]
struct StoredOperation {
    transfer: Option<PreparedTransferPublication>,
    binding: contract::OperationBinding,
    context: Option<PreparedWorkspaceContext>,
    invocation_id: OperationId,
    expires_at_unix_ms: u64,
    journal: Option<JournalOperation>,
    persisted: bool,
}

#[derive(Clone)]
struct PreparedWorkspaceContext {
    binding: SessionWorkspaceBinding,
    cursor: WorkspaceCursor,
}

impl PreparedWorkspaceContext {
    fn matches(&self, binding: &SessionWorkspaceBinding, cursor: &WorkspaceCursor) -> bool {
        &self.binding == binding && &self.cursor == cursor
    }
}

#[derive(Clone)]
struct JournalOperation {
    publication_cwd: Option<WorkspacePath>,
    publication_id: Option<OperationId>,
    host_instance_id: String,
    operation_id: OperationId,
    invocation_id: OperationId,
    preparation_id: OperationId,
    operation_kind: String,
    request_digest: RequestDigest,
}

struct PendingJournalOperation {
    publication_cwd: Option<WorkspacePath>,
    publication_id: Option<OperationId>,
    host_instance_id: String,
    invocation_id: OperationId,
    preparation_id: OperationId,
    operation_kind: String,
    request_digest: RequestDigest,
    state: RemoteOperationState,
    dispatched_at: Option<u64>,
    cursor: WorkspaceCursor,
    reachable: bool,
}

impl PendingJournalOperation {
    fn from_record(
        record: RemoteOperationRecord,
        current: &StoredWorkspaceBinding,
    ) -> Option<(OperationId, Self)> {
        let reachable = record.reachable_from(current);
        let cursor = record.binding.cursor().cloned()?;
        Some((
            record.operation_id,
            Self {
                publication_cwd: record.publication_cwd,
                publication_id: record.publication_id,
                host_instance_id: record.host_instance_id,
                invocation_id: record.invocation_id,
                preparation_id: record.preparation_id,
                operation_kind: record.operation_kind,
                request_digest: record.request_digest,
                state: record.state,
                dispatched_at: record.dispatched_at,
                cursor,
                reachable,
            },
        ))
    }
}

struct RemoteJournalState {
    journal: RemoteOperationJournal,
    pending: HashMap<OperationId, PendingJournalOperation>,
}

#[derive(Clone)]
struct RecoveryOperation {
    operation_kind: String,
    publication_cwd: Option<WorkspacePath>,
    publication_id: Option<OperationId>,
    host_instance_id: String,
    operation_id: OperationId,
    invocation_id: OperationId,
    preparation_id: OperationId,
    request_digest: RequestDigest,
    dispatched_at: Option<u64>,
    cursor: WorkspaceCursor,
}

struct RemoteMutationJournal {
    binding: StoredWorkspaceBinding,
    state: Mutex<RemoteJournalState>,
}

impl RemoteMutationJournal {
    fn new(
        journal: RemoteOperationJournal,
        binding: StoredWorkspaceBinding,
    ) -> Result<Self, RemoteWorkcellError> {
        let pending = journal
            .list_pending(&binding)
            .map_err(|_| RemoteWorkcellError::JournalUnavailable)?
            .into_iter()
            .map(|record| {
                PendingJournalOperation::from_record(record, &binding)
                    .ok_or(RemoteWorkcellError::JournalUnavailable)
            })
            .collect::<Result<HashMap<_, _>, _>>()?;
        Ok(Self {
            binding,
            state: Mutex::new(RemoteJournalState { journal, pending }),
        })
    }

    fn reserve_at(
        &self,
        operation: &JournalOperation,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
    ) -> Result<(), WorkspaceError> {
        if binding != self.binding.binding() {
            return Err(WorkspaceError::IdentityMismatch);
        }
        let stored_binding = self
            .binding
            .with_cursor(cursor.clone())
            .map_err(|_| WorkspaceError::IdentityMismatch)?;
        self.reserve_stored(operation, stored_binding)
    }

    fn reserve_stored(
        &self,
        operation: &JournalOperation,
        stored_binding: StoredWorkspaceBinding,
    ) -> Result<(), WorkspaceError> {
        let mut state = self.state.lock().map_err(|_| WorkspaceError::Unavailable)?;
        if state.pending.contains_key(&operation.operation_id) {
            return Err(WorkspaceError::PendingOperation {
                operation_id: operation.operation_id.as_str().to_owned(),
            });
        }
        let cursor = stored_binding
            .cursor()
            .cloned()
            .ok_or(WorkspaceError::IdentityMismatch)?;
        state
            .journal
            .reserve_before_send(&RemoteOperationReservation {
                publication_cwd: operation.publication_cwd.clone(),
                host_instance_id: operation.host_instance_id.clone(),
                publication_id: operation.publication_id.clone(),
                operation_id: operation.operation_id.clone(),
                invocation_id: operation.invocation_id.clone(),
                preparation_id: operation.preparation_id.clone(),
                binding: stored_binding,
                operation_kind: operation.operation_kind.clone(),
                request_digest: operation.request_digest.clone(),
                created_at: unix_millis(),
            })
            .map_err(|_| WorkspaceError::Unavailable)?;
        state.pending.insert(
            operation.operation_id.clone(),
            PendingJournalOperation {
                publication_cwd: operation.publication_cwd.clone(),
                publication_id: operation.publication_id.clone(),
                host_instance_id: operation.host_instance_id.clone(),
                invocation_id: operation.invocation_id.clone(),
                preparation_id: operation.preparation_id.clone(),
                operation_kind: operation.operation_kind.clone(),
                request_digest: operation.request_digest.clone(),
                state: RemoteOperationState::Reserved,
                dispatched_at: None,
                cursor,
                reachable: true,
            },
        );
        Ok(())
    }

    #[cfg(test)]
    fn reserve(&self, operation: &JournalOperation) -> Result<(), WorkspaceError> {
        let cursor = WorkspaceCursor::new(
            self.binding.binding(),
            ResourceScope::root(
                ResourceId::new(self.binding.cwd_handle().as_str())
                    .map_err(|_| WorkspaceError::IdentityMismatch)?,
            ),
            0,
            self.binding.cwd_handle().clone(),
        );
        self.reserve_at(operation, self.binding.binding(), &cursor)
    }

    fn mark_dispatched(&self, operation_id: &OperationId) -> Result<(), RemoteWorkcellError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| RemoteWorkcellError::JournalUnavailable)?;
        let dispatched_at = unix_millis();
        state
            .journal
            .mark_dispatched(operation_id, dispatched_at)
            .map_err(|_| RemoteWorkcellError::JournalUnavailable)?;
        let pending = state
            .pending
            .get_mut(operation_id)
            .ok_or(RemoteWorkcellError::JournalUnavailable)?;
        pending.state = RemoteOperationState::Dispatched;
        pending.dispatched_at = Some(dispatched_at);
        Ok(())
    }

    fn mark_indeterminate(&self, operation_id: &OperationId) -> Result<(), WorkspaceError> {
        let mut state = self.state.lock().map_err(|_| WorkspaceError::Unavailable)?;
        let current = state
            .pending
            .get(operation_id)
            .map(|pending| pending.state)
            .ok_or(WorkspaceError::Unavailable)?;
        if current != RemoteOperationState::Indeterminate {
            state
                .journal
                .mark_indeterminate(operation_id, unix_millis())
                .map_err(|_| WorkspaceError::Unavailable)?;
            state
                .pending
                .get_mut(operation_id)
                .ok_or(WorkspaceError::Unavailable)?
                .state = RemoteOperationState::Indeterminate;
        }
        Ok(())
    }

    fn commit_terminal<T>(
        &self,
        operation_id: &OperationId,
        status: &OperationState<T>,
    ) -> Result<bool, WorkspaceError> {
        let Some((terminal, side_effects_possible)) = journal_terminal_state(status) else {
            if matches!(
                status,
                OperationState::NeverSeen
                    | OperationState::Forgotten
                    | OperationState::Indeterminate { .. }
            ) {
                self.mark_indeterminate(operation_id)?;
            }
            return Ok(false);
        };
        let mut state = self.state.lock().map_err(|_| WorkspaceError::Unavailable)?;
        state
            .journal
            .commit_terminal(operation_id, terminal, side_effects_possible, unix_millis())
            .map_err(|_| WorkspaceError::Unavailable)?;
        state.pending.remove(operation_id);
        Ok(true)
    }

    fn release_before_dispatch(&self, operation_id: &OperationId) -> Result<(), WorkspaceError> {
        let mut state = self.state.lock().map_err(|_| WorkspaceError::Unavailable)?;
        let at = unix_millis();
        state
            .journal
            .commit_terminal(operation_id, RemoteOperationState::Cancelled, false, at)
            .and_then(|()| state.journal.acknowledge(operation_id, at))
            .map_err(|_| WorkspaceError::Unavailable)?;
        state.pending.remove(operation_id);
        Ok(())
    }

    fn abandon(&self, operation_id: &OperationId) -> Result<(), WorkspaceError> {
        let state = self
            .state
            .lock()
            .map_err(|_| WorkspaceError::Unavailable)?
            .pending
            .get(operation_id)
            .map(|pending| pending.state)
            .ok_or(WorkspaceError::Unavailable)?;
        match state {
            RemoteOperationState::Reserved => self.release_before_dispatch(operation_id),
            RemoteOperationState::Dispatched => self.mark_indeterminate(operation_id),
            RemoteOperationState::Indeterminate => Ok(()),
            _ => Err(WorkspaceError::Unavailable),
        }
    }

    fn acknowledge(&self, operation_id: &OperationId) -> Result<(), RemoteWorkcellError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| RemoteWorkcellError::JournalUnavailable)?;
        if !state.pending.contains_key(operation_id) {
            return Err(RemoteWorkcellError::Conflict);
        }
        state
            .journal
            .acknowledge(operation_id, unix_millis())
            .map_err(|_| RemoteWorkcellError::JournalUnavailable)?;
        state.pending.remove(operation_id);
        Ok(())
    }

    fn contains(&self, operation_id: &OperationId) -> Result<bool, WorkspaceError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| WorkspaceError::Unavailable)?
            .pending
            .contains_key(operation_id))
    }

    fn pending(&self) -> Vec<PendingRemoteOperation> {
        let Ok(state) = self.state.lock() else {
            return Vec::new();
        };
        let mut pending = state
            .pending
            .iter()
            .map(|(operation_id, pending)| PendingRemoteOperation {
                operation_id: operation_id.clone(),
                operation_kind: pending.operation_kind.clone(),
                state: pending.state,
                reachable: pending.reachable,
            })
            .collect::<Vec<_>>();
        pending.sort_unstable_by(|left, right| {
            left.operation_id.as_str().cmp(right.operation_id.as_str())
        });
        pending
    }

    fn recovery_operations(&self) -> Result<Vec<RecoveryOperation>, RemoteWorkcellError> {
        let state = self
            .state
            .lock()
            .map_err(|_| RemoteWorkcellError::JournalUnavailable)?;
        Ok(state
            .pending
            .iter()
            .filter(|(_, pending)| pending.reachable)
            .map(|(operation_id, pending)| RecoveryOperation {
                operation_kind: pending.operation_kind.clone(),
                publication_cwd: pending.publication_cwd.clone(),
                publication_id: pending.publication_id.clone(),
                host_instance_id: pending.host_instance_id.clone(),
                operation_id: operation_id.clone(),
                invocation_id: pending.invocation_id.clone(),
                preparation_id: pending.preparation_id.clone(),
                request_digest: pending.request_digest.clone(),
                dispatched_at: pending.dispatched_at,
                cursor: pending.cursor.clone(),
            })
            .collect())
    }

    fn reconcile_recovery<T>(
        &self,
        operation_id: &OperationId,
        status: &OperationState<T>,
        absence_is_proof: bool,
    ) -> Result<bool, RemoteWorkcellError> {
        if absence_is_proof
            && matches!(status, OperationState::NeverSeen | OperationState::Prepared)
        {
            return self.reconcile_host_absent(operation_id);
        }
        let Some((terminal, side_effects_possible)) = journal_terminal_state(status) else {
            return Ok(false);
        };
        let mut state = self
            .state
            .lock()
            .map_err(|_| RemoteWorkcellError::JournalUnavailable)?;
        state
            .journal
            .reconcile(operation_id, terminal, side_effects_possible, unix_millis())
            .map_err(|_| RemoteWorkcellError::JournalUnavailable)?;
        state.pending.remove(operation_id);
        Ok(true)
    }

    /// The host reports NeverSeen or Prepared, and its absence is proof: the
    /// query reached the same instance the client dispatched to, and that
    /// instance has not evicted any tombstone recent enough to have covered
    /// this operation. The local dispatch fence (mark_dispatched fires before
    /// the send) proves intent, not delivery, so the host's assertion is
    /// strictly stronger, and the operation is safe to cancel regardless of
    /// the local fence state.
    fn reconcile_host_absent(
        &self,
        operation_id: &OperationId,
    ) -> Result<bool, RemoteWorkcellError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| RemoteWorkcellError::JournalUnavailable)?;
        if !state.pending.contains_key(operation_id) {
            return Err(RemoteWorkcellError::JournalUnavailable);
        }
        state
            .journal
            .reconcile(
                operation_id,
                RemoteOperationState::Cancelled,
                false,
                unix_millis(),
            )
            .map_err(|_| RemoteWorkcellError::JournalUnavailable)?;
        state.pending.remove(operation_id);
        Ok(true)
    }

    fn recovery_indeterminate(
        &self,
        operation_id: &OperationId,
    ) -> Result<(), RemoteWorkcellError> {
        self.mark_indeterminate(operation_id)
            .map_err(|_| RemoteWorkcellError::JournalUnavailable)
    }
}

struct OperationRegistry {
    entries: HashMap<OperationId, StoredOperation>,
    pending: usize,
    limit: usize,
}

impl OperationRegistry {
    fn new(limit: usize) -> Self {
        Self {
            entries: HashMap::new(),
            pending: 0,
            limit: limit.max(1),
        }
    }

    fn expire_confirmed(&mut self) {
        let now = unix_millis();
        self.entries
            .retain(|_, operation| operation.persisted || operation.expires_at_unix_ms > now);
    }

    fn reserve(&mut self) -> bool {
        self.expire_confirmed();
        if self.entries.len().saturating_add(self.pending) >= self.limit {
            return false;
        }
        self.pending = self.pending.saturating_add(1);
        true
    }

    /// Journal-backed entries a sweep may be able to reclaim. Expiry exempts
    /// them because the journal, not the clock, decides when their operation
    /// is over, so nothing else ever drops one.
    fn settled_candidates(&self) -> Vec<OperationId> {
        self.entries
            .values()
            .filter(|operation| operation.persisted)
            .filter_map(|operation| {
                operation
                    .journal
                    .as_ref()
                    .map(|journal| journal.operation_id.clone())
            })
            .collect()
    }

    fn finish_reservation(&mut self) {
        self.pending = self.pending.saturating_sub(1);
    }

    fn insert(
        &mut self,
        preparation_id: OperationId,
        operation: StoredOperation,
    ) -> Result<(), WorkspaceError> {
        self.expire_confirmed();
        if self.entries.contains_key(&preparation_id) {
            return Err(WorkspaceError::InvalidResponse {
                violation: caudra_workspace::InvalidResponseKind::InvalidIdentity,
            });
        }
        self.entries.insert(preparation_id, operation);
        Ok(())
    }

    fn get(&mut self, operation: &OperationHandle) -> Result<&StoredOperation, WorkspaceError> {
        self.expire_confirmed();
        let stored = self
            .entries
            .get(&operation.preparation_id)
            .ok_or_else(|| stale_preparation(&operation.preparation_id))?;
        if operation.invocation_id.as_ref() != Some(&stored.invocation_id)
            || operation.expires_at_unix_ms != Some(stored.expires_at_unix_ms)
        {
            return Err(WorkspaceError::IdentityMismatch);
        }
        Ok(stored)
    }

    fn mark_persisted(&mut self, preparation_id: &OperationId) -> Result<(), WorkspaceError> {
        self.entries
            .get_mut(preparation_id)
            .ok_or_else(|| stale_preparation(preparation_id))?
            .persisted = true;
        Ok(())
    }

    fn remove(&mut self, preparation_id: &OperationId) {
        self.entries.remove(preparation_id);
    }

    fn remove_journal_operation(&mut self, operation_id: &OperationId) {
        self.entries.retain(|_, operation| {
            operation
                .journal
                .as_ref()
                .is_none_or(|journal| &journal.operation_id != operation_id)
        });
    }

    #[cfg(test)]
    fn len(&mut self) -> usize {
        self.expire_confirmed();
        self.entries.len()
    }
}

struct PreparationPermit(Arc<RemoteInner>);

impl Drop for PreparationPermit {
    fn drop(&mut self) {
        if let Ok(mut operations) = self.0.operations.lock() {
            operations.finish_reservation();
        }
        self.0.operation_slots.notify(1);
    }
}

struct RemoteInner {
    staging: PrivateStaging,
    transport: RemoteTransport,
    descriptor: contract::RemoteHostDescriptor,
    host_binding: Mutex<contract::HostBinding>,
    authority: AuthorityIdentity,
    project: ProjectIdentity,
    session_binding: SessionWorkspaceBinding,
    stored_binding: StoredWorkspaceBinding,
    root_cursor: WorkspaceCursor,
    capabilities: WorkspaceCapabilities,
    manifest: ToolManifest,
    catalog: HashMap<String, OwnedToolSpec>,
    events: Receiver<RemoteEvent>,
    cancellation: CancellationToken,
    paths: Mutex<ResourceCache>,
    repositories: Mutex<BoundedMap<ResourceId, WorkspacePath>>,
    cursors: Mutex<CursorRegistry>,
    watches: Mutex<WatchRegistry>,
    operations: Mutex<OperationRegistry>,
    captures: snapshot::CaptureRegistry,
    operation_slots: Event,
    mutation_journal: RemoteMutationJournal,
}

#[derive(Clone)]
pub struct RemoteWorkcellClient(Arc<RemoteInner>);

impl fmt::Debug for RemoteWorkcellClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteWorkcellClient")
            .field("status", &self.connection_status())
            .field("authority", &self.0.authority)
            .finish_non_exhaustive()
    }
}

impl RemoteWorkcellClient {
    pub async fn connect(
        selection: &RemoteWorkcellSelection,
        credential: Option<NamedBearerCredential>,
        session_binding_id: SessionBindingId,
        journal: RemoteOperationJournal,
        cancellation: CancellationToken,
    ) -> Result<Self, RemoteWorkcellError> {
        validate_credential_selection(selection, credential.as_ref())?;
        let bearer = credential
            .as_ref()
            .map(|credential| credential.bearer.clone());
        let (transport, events) = RemoteTransport::new(&selection.endpoint, bearer)?;
        let discover = transport
            .request(
                "server/discover",
                json!({}),
                MAX_HTTP_RESPONSE_BYTES as u64,
                &cancellation,
            )
            .await?;
        let descriptor = parse_descriptor(&discover)?;
        validate_descriptor(&descriptor, selection)?;
        let host_binding = host_binding(&descriptor);
        let manifest = fetch_and_validate_catalog(&transport, &descriptor, &cancellation).await?;
        let catalog: HashMap<String, OwnedToolSpec> = manifest
            .tools
            .iter()
            .cloned()
            .map(|tool| (tool.name.clone(), tool))
            .collect();
        let trust_anchor = source_trust_anchor(selection)?;
        let authority = AuthorityIdentity::new(
            trust_anchor,
            descriptor.server_id.as_str(),
            descriptor.workspace_id.as_str(),
            descriptor.workspace_generation.as_str(),
            descriptor.resource_namespace_version.as_str(),
        )
        .map_err(|_| RemoteWorkcellError::IdentityMismatch)?;
        let principal =
            AuthenticatedPrincipalId::new(authority.clone(), descriptor.principal_id.as_str())
                .map_err(|_| RemoteWorkcellError::IdentityMismatch)?;
        let project = ProjectIdentity::new(
            authority.clone(),
            ProjectKey::new(descriptor.root_project_id.as_str())
                .map_err(|_| RemoteWorkcellError::IdentityMismatch)?,
        );
        let session_binding = SessionWorkspaceBinding::new(
            session_binding_id,
            authority.clone(),
            principal.clone(),
            project.clone(),
        )
        .map_err(|_| RemoteWorkcellError::IdentityMismatch)?;
        let resolved = transport
            .request(
                contract::RESOLVE_DIRECTORY_METHOD,
                serde_json::to_value(contract::ResolveDirectoryRequest {
                    version: contract::ContractVersion::V1,
                    binding: contract::WorkspaceRequestBinding {
                        host: host_binding.clone(),
                        cwd_handle: descriptor.cwd.handle.clone(),
                    },
                    path: contract::DirectoryNavigation::new(".")
                        .map_err(|_| RemoteWorkcellError::InvalidProtocol)?,
                })
                .map_err(|_| RemoteWorkcellError::InvalidProtocol)?,
                descriptor
                    .capabilities
                    .tool_execution
                    .limits
                    .max_request_bytes,
                &cancellation,
            )
            .await?;
        let resolved: contract::ResolveDirectoryResponse =
            serde_json::from_value(resolved).map_err(|_| RemoteWorkcellError::InvalidProtocol)?;
        if resolved.version != contract::ContractVersion::V1
            || resolved.directory.display_path.as_str() != descriptor.cwd.display_path.as_str()
        {
            return Err(RemoteWorkcellError::IdentityMismatch);
        }
        let root_id = resource_id(&resolved.directory.resource_id)
            .map_err(|_| RemoteWorkcellError::IdentityMismatch)?;
        let cwd_handle = cwd_handle(&descriptor.cwd.handle)
            .map_err(|_| RemoteWorkcellError::IdentityMismatch)?;
        let root_cursor = WorkspaceCursor::new(
            &session_binding,
            ResourceScope::root(root_id.clone()),
            0,
            cwd_handle.clone(),
        );
        let stored_binding = StoredWorkspaceBinding::new_with_cursor(
            session_binding.clone(),
            root_cursor.clone(),
            Some(descriptor.workspace_generation.as_str().to_owned()),
        )
        .map_err(|_| RemoteWorkcellError::IdentityMismatch)?;
        let mutation_journal = RemoteMutationJournal::new(journal, stored_binding.clone())?;
        let capabilities = workspace_capabilities(&descriptor.capabilities);
        capabilities
            .validate()
            .map_err(|_| RemoteWorkcellError::CapabilityMismatch)?;
        let path_limit = descriptor
            .capabilities
            .workspace
            .as_ref()
            .map_or(1, |workspace| workspace.limits.max_list_entries as usize)
            .max(1);
        let cursor_limit = path_limit.min(contract::MAX_WORKSPACE_LIST_ENTRIES as usize);
        let watch_limit = descriptor
            .capabilities
            .watch
            .as_ref()
            .map_or(1, |watch| watch.limits.max_subscriptions as usize);
        let watch_ttl = descriptor
            .capabilities
            .watch
            .as_ref()
            .map_or(DEFAULT_CACHE_TTL, |watch| {
                Duration::from_millis(watch.limits.subscription_ttl_ms)
            });
        // Defaulting to one operation made a host that never declared the
        // capability look like a host that allows a single call at a time, so
        // the second concurrent tool call conflicted for no stated reason.
        let operations = descriptor
            .capabilities
            .operations
            .as_ref()
            .ok_or(RemoteWorkcellError::CapabilityMismatch)?;
        let operation_limit = operations
            .limits
            .max_operations
            .min(operations.limits.max_preparations) as usize;
        let mut paths = ResourceCache::new(path_limit, DEFAULT_CACHE_TTL);
        paths.insert(root_id, selection.cwd.clone());
        let mut cursors = HashMap::new();
        cursors.insert(
            cwd_handle,
            CursorRecord {
                cursor: root_cursor.clone(),
                path: selection.cwd.clone(),
            },
        );
        let client = Self(Arc::new(RemoteInner {
            staging: PrivateStaging::new(transfer::negotiated_limits(
                descriptor.capabilities.reviewed_transfer.as_ref(),
            )),
            transport,
            descriptor,
            host_binding: Mutex::new(host_binding),
            authority,
            project,
            session_binding,
            stored_binding,
            root_cursor,
            capabilities,
            manifest,
            catalog,
            events,
            cancellation,
            paths: Mutex::new(paths),
            repositories: Mutex::new(BoundedMap::new(path_limit, DEFAULT_CACHE_TTL)),
            cursors: Mutex::new(CursorRegistry {
                records: cursors,
                limit: cursor_limit,
            }),
            watches: Mutex::new(WatchRegistry::new(watch_limit, watch_ttl)),
            operations: Mutex::new(OperationRegistry::new(operation_limit)),
            captures: snapshot::CaptureRegistry::default(),
            operation_slots: Event::new(),
            mutation_journal,
        }));
        client.recover_pending_operations().await?;
        Ok(client)
    }

    pub fn descriptor(&self) -> &contract::RemoteHostDescriptor {
        &self.0.descriptor
    }

    pub fn host_binding(&self) -> contract::HostBinding {
        self.0
            .host_binding
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn manifest(&self) -> &ToolManifest {
        &self.0.manifest
    }

    pub fn canonical_catalog(&self) -> &HashMap<String, OwnedToolSpec> {
        &self.0.catalog
    }

    pub fn endpoint_authority(&self) -> String {
        self.0.transport.endpoint.origin().ascii_serialization()
    }

    pub fn endpoint(&self) -> &Url {
        &self.0.transport.endpoint
    }

    pub async fn prepare_canonical_tool(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &ToolPrepareRequest,
    ) -> Result<RemotePreparedToolCall, WorkspaceError> {
        let prepared = WorkspaceToolService::prepare(self, binding, cursor, request).await?;
        let intent =
            serde_json::from_value(prepared.review.clone()).map_err(|_| invalid_response())?;
        Ok(RemotePreparedToolCall {
            prepared,
            intent,
            binding: binding.clone(),
            cursor: cursor.clone(),
        })
    }

    pub async fn execute_canonical_tool(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        prepared: &PreparedToolCall,
    ) -> Result<OperationStatus<RemoteToolResultEnvelope>, WorkspaceError> {
        self.execute_canonical_tool_tracked(binding, cursor, prepared)
            .await
            .map_err(|error| match error {
                RemoteToolExecutionError::BeforeDispatch(error)
                | RemoteToolExecutionError::PossiblyDispatched(error) => error,
            })
    }

    pub(crate) async fn execute_canonical_tool_tracked(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        prepared: &PreparedToolCall,
    ) -> Result<OperationStatus<RemoteToolResultEnvelope>, RemoteToolExecutionError> {
        self.require_capability(WorkspaceCapability::ToolExecute)?;
        self.execute_tool_operation(binding, cursor, &prepared.operation)
            .await
    }

    pub async fn canonical_tool_status(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        operation: &OperationHandle,
    ) -> Result<OperationStatus<RemoteToolResultEnvelope>, WorkspaceError> {
        self.canonical_tool_status_after(binding, cursor, operation, None)
            .await
    }

    pub async fn canonical_tool_status_after(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        operation: &OperationHandle,
        after_sequence: Option<u64>,
    ) -> Result<OperationStatus<RemoteToolResultEnvelope>, WorkspaceError> {
        self.require_capability(WorkspaceCapability::ToolStatus)?;
        self.validate_prepared_context(binding, cursor, operation)?;
        self.0
            .operations
            .lock()
            .map_err(|_| WorkspaceError::Unavailable)?
            .get(operation)?;
        let mut request = status_request(operation, &self.host_binding())?;
        request.after_sequence = after_sequence;
        let response: contract::StatusResponse = self
            .call(
                contract::STATUS_METHOD,
                &request,
                &self.0.cancellation.child_token(),
            )
            .await?;
        self.convert_remote_tool_status(&response, operation).await
    }

    pub async fn cancel_canonical_tool(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        operation: &OperationHandle,
    ) -> Result<CancellationResult, WorkspaceError> {
        self.require_capability(WorkspaceCapability::ToolCancel)?;
        self.cancel_operation(binding, cursor, operation).await
    }

    pub async fn release_canonical_tool(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        prepared: &PreparedToolCall,
    ) -> Result<ReleaseResult, WorkspaceError> {
        self.require_capability(WorkspaceCapability::ToolRelease)?;
        self.release_operation(binding, cursor, &prepared.operation)
            .await
    }

    pub fn connection_status(&self) -> RemoteConnectionStatus {
        self.0.transport.status()
    }

    pub fn events(&self) -> Receiver<RemoteEvent> {
        self.0.events.clone()
    }

    pub fn session_binding(&self) -> &SessionWorkspaceBinding {
        &self.0.session_binding
    }

    pub fn stored_binding(&self) -> &StoredWorkspaceBinding {
        &self.0.stored_binding
    }

    pub fn pending_remote_operations(&self) -> Vec<PendingRemoteOperation> {
        self.0.mutation_journal.pending()
    }

    pub fn acknowledge_pending_operation(
        &self,
        operation_id: &OperationId,
    ) -> Result<(), RemoteWorkcellError> {
        self.0.mutation_journal.acknowledge(operation_id)?;
        self.0
            .operations
            .lock()
            .map_err(|_| RemoteWorkcellError::JournalUnavailable)?
            .remove_journal_operation(operation_id);
        self.0.operation_slots.notify(1);
        Ok(())
    }

    pub fn abandon_operation(&self, operation: &OperationHandle) -> Result<(), WorkspaceError> {
        let journal = self
            .0
            .operations
            .lock()
            .map_err(|_| WorkspaceError::Unavailable)?
            .get(operation)?
            .journal
            .clone();
        if let Some(journal) = journal {
            self.0.mutation_journal.abandon(&journal.operation_id)?;
        }
        Ok(())
    }

    pub fn root_cursor(&self) -> &WorkspaceCursor {
        &self.0.root_cursor
    }

    /// The workspace path of `cursor`'s directory: the base the host resolves
    /// a relative path against.
    pub fn cursor_path(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
    ) -> Result<WorkspacePath, WorkspaceError> {
        self.validate_context(binding, cursor)
            .map(|record| record.path)
    }

    pub fn workspace_handle(&self) -> Result<WorkspaceHandle, WorkspaceError> {
        let service = Arc::new(self.clone());
        let capabilities = &self.0.descriptor.capabilities;
        let workspace = capabilities.workspace.as_ref();
        let has_any = |candidates: &[WorkspaceCapability]| {
            candidates
                .iter()
                .any(|capability| self.0.capabilities.supports(*capability))
        };
        let services = WorkspaceServices {
            transfer: self
                .0
                .capabilities
                .supports(WorkspaceCapability::ReviewedTransfer)
                .then(|| service.clone() as Arc<dyn WorkspaceTransferService>),
            control: Some(service.clone()),
            read: workspace
                .is_some()
                .then(|| service.clone() as Arc<dyn WorkspaceReadService>),
            mutation: has_any(&[
                WorkspaceCapability::MutationExecute,
                WorkspaceCapability::MutationStatus,
                WorkspaceCapability::MutationCancel,
            ])
            .then(|| service.clone() as Arc<dyn WorkspaceMutationService>),
            search: workspace
                .is_some_and(|workspace| workspace.methods.search_text)
                .then(|| service.clone() as Arc<dyn WorkspaceSearchService>),
            watch: has_any(&[
                WorkspaceCapability::WatchOpen,
                WorkspaceCapability::WatchPoll,
                WorkspaceCapability::WatchClose,
            ])
            .then(|| service.clone() as Arc<dyn WorkspaceWatchService>),
            exec: has_any(&[
                WorkspaceCapability::ExecExecute,
                WorkspaceCapability::ExecStatus,
                WorkspaceCapability::ExecCancel,
            ])
            .then(|| service.clone() as Arc<dyn WorkspaceExecService>),
            scm_read: capabilities
                .scm
                .as_ref()
                .is_some_and(|scm| {
                    scm.methods.discover
                        || scm.methods.status
                        || scm.methods.log
                        || scm.methods.diff
                        || scm.methods.read_side
                })
                .then(|| service.clone() as Arc<dyn WorkspaceScmReadService>),
            scm_mutation: has_any(&[
                WorkspaceCapability::ScmStage,
                WorkspaceCapability::ScmUnstage,
                WorkspaceCapability::ScmDiscard,
                WorkspaceCapability::ScmMutationStatus,
                WorkspaceCapability::ScmMutationCancel,
                WorkspaceCapability::ScmMutationRelease,
            ])
            .then(|| service.clone() as Arc<dyn WorkspaceScmMutationService>),
            snapshot_read: capabilities
                .snapshots
                .as_ref()
                .is_some_and(|snapshots| {
                    snapshots.methods.prepare_capture && snapshots.methods.checkpoint
                        || snapshots.methods.inspect
                        || snapshots.methods.status
                })
                .then(|| service.clone() as Arc<dyn WorkspaceSnapshotReadService>),
            snapshot_mutation: has_any(&[
                WorkspaceCapability::SnapshotPrepareRestore,
                WorkspaceCapability::SnapshotPrepareUnrevert,
                WorkspaceCapability::SnapshotPrepareCleanup,
                WorkspaceCapability::SnapshotExecute,
                WorkspaceCapability::SnapshotOperationStatus,
                WorkspaceCapability::SnapshotCancel,
                WorkspaceCapability::SnapshotAcknowledge,
                WorkspaceCapability::SnapshotRelease,
            ])
            .then(|| service.clone() as Arc<dyn WorkspaceSnapshotMutationService>),
            assets: capabilities
                .project_assets
                .as_ref()
                .is_some_and(|assets| assets.methods.discover || assets.methods.read)
                .then(|| service.clone() as Arc<dyn WorkspaceAssetService>),
            tools: has_any(&[
                WorkspaceCapability::ToolPrepare,
                WorkspaceCapability::ToolExecute,
                WorkspaceCapability::ToolStatus,
                WorkspaceCapability::ToolCancel,
                WorkspaceCapability::ToolRelease,
            ])
            .then_some(service as Arc<dyn WorkspaceToolService>),
        };
        WorkspaceHandle::new(
            self.0.authority.clone(),
            self.0.capabilities.clone(),
            services,
        )
    }

    pub async fn reconnect(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<(), RemoteWorkcellError> {
        self.0
            .transport
            .status
            .store(CONNECTION_RECONNECTING, Ordering::Release);
        let result = async {
            let discover = self
                .0
                .transport
                .request(
                    "server/discover",
                    json!({}),
                    MAX_HTTP_RESPONSE_BYTES as u64,
                    cancellation,
                )
                .await?;
            let descriptor = parse_descriptor(&discover)?;
            if !same_descriptor_except_instance(&descriptor, &self.0.descriptor) {
                return Err(RemoteWorkcellError::IdentityMismatch);
            }
            let manifest =
                fetch_and_validate_catalog(&self.0.transport, &descriptor, cancellation).await?;
            if manifest != self.0.manifest {
                return Err(RemoteWorkcellError::CatalogMismatch);
            }
            *self
                .0
                .host_binding
                .lock()
                .map_err(|_| RemoteWorkcellError::Transport)? = host_binding(&descriptor);
            self.recover_pending_operations().await?;
            Ok(())
        }
        .await;
        self.0.transport.status.store(
            if result.is_ok() {
                CONNECTION_CONNECTED
            } else {
                CONNECTION_DISCONNECTED
            },
            Ordering::Release,
        );
        result
    }

    async fn recover_pending_operations(&self) -> Result<(), RemoteWorkcellError> {
        for operation in self.0.mutation_journal.recovery_operations()? {
            if operation.cursor.project() != &self.0.project {
                return Err(RemoteWorkcellError::RecoveryBindingMismatch {
                    operation_id: operation.operation_id.as_str().to_owned(),
                });
            }
            if operation.publication_id.is_some() {
                self.recover_transfer(&operation).await?;
                continue;
            }
            let handle = OperationHandle {
                preparation_id: operation.preparation_id.clone(),
                invocation_id: Some(operation.invocation_id.clone()),
                execution_id: None,
                expires_at_unix_ms: None,
            };
            let host = self.host_binding();
            let same_instance = operation.host_instance_id == host.instance_id.as_str();
            let response = self
                .call::<_, contract::StatusResponse>(
                    contract::STATUS_METHOD,
                    &status_request(&handle, &host)
                        .map_err(|_| RemoteWorkcellError::InvalidProtocol)?,
                    &CancellationToken::new(),
                )
                .await;
            let status = response.and_then(|response| {
                let evicted_through = response.tombstones_evicted_through_unix_ms;
                recovery_status(&response, &operation, &host)
                    .map(|status| (status, evicted_through))
            });
            match status {
                Ok((status, evicted_through))
                    if self.0.mutation_journal.reconcile_recovery(
                        &operation.operation_id,
                        &status.state,
                        same_instance && absence_is_proof(&operation, evicted_through),
                    )? =>
                {
                    self.release_confirmed_operation(
                        &operation.preparation_id,
                        &operation.invocation_id,
                    )
                    .await;
                }
                Ok(_) | Err(_) => {
                    self.0
                        .mutation_journal
                        .recovery_indeterminate(&operation.operation_id)?;
                }
            }
        }
        Ok(())
    }

    async fn release_confirmed_operation(
        &self,
        preparation_id: &OperationId,
        invocation_id: &OperationId,
    ) {
        let request = contract::ReleaseRequest {
            version: contract::ContractVersion::V1,
            selector: contract::OperationSelector {
                preparation_id: match contract_identifier(preparation_id) {
                    Ok(id) => id,
                    Err(_) => return,
                },
                invocation_id: match contract_identifier(invocation_id) {
                    Ok(id) => Some(id),
                    Err(_) => return,
                },
                host: self.host_binding(),
            },
        };
        let _: Result<contract::ReleaseResponse, WorkspaceError> = self
            .call(
                contract::RELEASE_METHOD,
                &request,
                &CancellationToken::new(),
            )
            .await;
    }

    pub async fn recover_persisted_operation_status(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        operation: &OperationHandle,
    ) -> Result<OperationStatus<Value>, WorkspaceError> {
        self.validate_context(binding, cursor)?;
        {
            let mut operations = self
                .0
                .operations
                .lock()
                .map_err(|_| WorkspaceError::Unavailable)?;
            operations.expire_confirmed();
            if operations.entries.contains_key(&operation.preparation_id) {
                return Err(WorkspaceError::Conflict);
            }
        }
        let response: contract::StatusResponse = self
            .call(
                contract::STATUS_METHOD,
                &status_request(operation, &self.host_binding())?,
                &self.0.cancellation.child_token(),
            )
            .await?;
        self.validate_status_limits(&response)?;
        convert_status(&response, operation, &self.host_binding(), None, |value| {
            Ok(value.clone())
        })
    }

    pub async fn resolve_directory_cursor(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        path: &WorkspacePath,
    ) -> Result<ResolvedWorkspaceDirectory, WorkspaceError> {
        self.navigate_directory(
            binding,
            cursor,
            &DirectoryNavigation::new(path.as_str()).map_err(|_| invalid_path())?,
        )
        .await
    }

    async fn navigate_directory_cursor(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        path: &DirectoryNavigation,
    ) -> Result<ResolvedWorkspaceDirectory, WorkspaceError> {
        self.require_capability(WorkspaceCapability::Resolve)?;
        let cancellation = self.0.cancellation.child_token();
        let requested =
            contract::DirectoryNavigation::new(path.as_str()).map_err(|_| invalid_path())?;
        let expected_path = path
            .resolve(&self.validate_context(binding, cursor)?.path)
            .map_err(|_| invalid_path())?;
        let response: contract::ResolveDirectoryResponse = self
            .call(
                contract::RESOLVE_DIRECTORY_METHOD,
                &contract::ResolveDirectoryRequest {
                    version: contract::ContractVersion::V1,
                    binding: self.bind_workspace_request(binding, cursor)?,
                    path: requested,
                },
                &cancellation,
            )
            .await?;
        validate_v1(response.version)?;
        let returned_path = workspace_path(&response.directory.display_path)?;
        if returned_path != expected_path {
            return Err(invalid_path());
        }
        let id = self.remember_resource_id(&response.directory.resource_id, &returned_path)?;
        let records = self
            .0
            .cursors
            .lock()
            .map_err(|_| WorkspaceError::Unavailable)?;
        let scope = if let Some(record) = records.records.values().find(|record| {
            record.path == returned_path && record.cursor.scope().resource_id() == &id
        }) {
            record.cursor.scope().clone()
        } else if let Some(parent) = records
            .records
            .values()
            .filter(|record| {
                record.path != returned_path && path_within(&record.path, &returned_path, true)
            })
            .max_by_key(|record| record.path.as_str().len())
        {
            child_scope(parent.cursor.scope(), id.clone())?
        } else {
            ResourceScope::new(Vec::new(), id.clone()).map_err(|_| invalid_path())?
        };
        drop(records);
        let resource = WorkspaceResource {
            project: self.0.project.clone(),
            scope,
            path: Some(returned_path.clone()),
            kind: ResourceKind::Directory,
            revision: Some(resource_revision(&response.directory.revision)?),
            size_bytes: None,
        };
        let handle = cwd_handle(&response.directory.handle)?;
        let resolved_cursor = WorkspaceCursor::new(
            binding,
            resource.scope.clone(),
            cursor.generation(),
            handle.clone(),
        );
        let mut cursors = self
            .0
            .cursors
            .lock()
            .map_err(|_| WorkspaceError::Unavailable)?;
        cursors.insert(
            handle,
            CursorRecord {
                cursor: resolved_cursor.clone(),
                path: returned_path,
            },
        )?;
        Ok(ResolvedWorkspaceDirectory {
            resource,
            cursor: resolved_cursor,
        })
    }

    fn require_capability(&self, capability: WorkspaceCapability) -> Result<(), WorkspaceError> {
        self.0.capabilities.require(capability)
    }

    fn validate_context(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
    ) -> Result<CursorRecord, WorkspaceError> {
        if binding != &self.0.session_binding {
            return Err(WorkspaceError::IdentityMismatch);
        }
        if self.0.cancellation.is_cancelled() {
            return Err(WorkspaceError::Cancelled);
        }
        let record = self
            .0
            .cursors
            .lock()
            .map_err(|_| WorkspaceError::Unavailable)?
            .records
            .get(cursor.cwd_handle())
            .cloned()
            .ok_or(WorkspaceError::StaleCursor)?;
        if &record.cursor != cursor
            || cursor.project() != &self.0.project
            || cursor.generation() != self.0.root_cursor.generation()
        {
            return Err(WorkspaceError::StaleCursor);
        }
        cursor.validate(
            binding,
            record.cursor.generation(),
            record.cursor.cwd_handle(),
        )?;
        Ok(record)
    }

    fn validate_prepared_context(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        operation: &OperationHandle,
    ) -> Result<(), WorkspaceError> {
        self.validate_context(binding, cursor)?;
        let operations = self
            .0
            .operations
            .lock()
            .map_err(|_| WorkspaceError::Unavailable)?;
        let stored = operations
            .entries
            .get(&operation.preparation_id)
            .ok_or_else(|| stale_preparation(&operation.preparation_id))?;
        if stored
            .context
            .as_ref()
            .is_some_and(|prepared| !prepared.matches(binding, cursor))
        {
            return Err(WorkspaceError::IdentityMismatch);
        }
        Ok(())
    }

    fn bind_workspace_request(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
    ) -> Result<contract::WorkspaceRequestBinding, WorkspaceError> {
        self.validate_context(binding, cursor)?;
        Ok(contract::WorkspaceRequestBinding {
            host: self.host_binding(),
            cwd_handle: contract::ResourceId::new(cursor.cwd_handle().as_str())
                .map_err(|_| invalid_response())?,
        })
    }

    fn snapshot_limits(&self) -> Result<&contract::WorkspaceSnapshotLimits, WorkspaceError> {
        self.0
            .descriptor
            .capabilities
            .snapshots
            .as_ref()
            .map(|snapshots| &snapshots.limits)
            .ok_or_else(invalid_response)
    }

    fn expected_response_path(
        &self,
        cursor: &WorkspaceCursor,
        requested: &WorkspacePath,
    ) -> Result<WorkspacePath, WorkspaceError> {
        let record = self.validate_context(&self.0.session_binding, cursor)?;
        if requested.is_root() {
            return Ok(record.path);
        }
        if record.path.is_root() {
            return Ok(requested.clone());
        }
        WorkspacePath::new(format!("{}/{}", record.path.as_str(), requested.as_str()))
            .map_err(|_| invalid_path())
    }

    fn next_identifier(&self, prefix: &str) -> Result<contract::Identifier, WorkspaceError> {
        contract::Identifier::new(format!("caudra-{prefix}-{}", CaudraId::generate()))
            .map_err(|_| invalid_response())
    }

    fn try_reserve_preparation(&self) -> Result<Option<PreparationPermit>, WorkspaceError> {
        let reserved = self
            .0
            .operations
            .lock()
            .map_err(|_| WorkspaceError::Unavailable)?
            .reserve();
        Ok(reserved.then(|| PreparationPermit(self.0.clone())))
    }

    /// A persisted entry is exempt from expiry, and nothing reclaimed it when
    /// its journal operation settled, so a slot could stay held for the life of
    /// the process. Swept only under capacity pressure: while slots are free
    /// the journal read costs more than the slot is worth.
    ///
    /// The operations lock is released before each journal read, because the
    /// journal takes its own lock and the two must never nest.
    fn reclaim_settled_operations(&self) -> Result<(), WorkspaceError> {
        let candidates = self
            .0
            .operations
            .lock()
            .map_err(|_| WorkspaceError::Unavailable)?
            .settled_candidates();
        for operation_id in candidates {
            if self.0.mutation_journal.contains(&operation_id)? {
                continue;
            }
            self.0
                .operations
                .lock()
                .map_err(|_| WorkspaceError::Unavailable)?
                .remove_journal_operation(&operation_id);
            self.0.operation_slots.notify(1);
        }
        Ok(())
    }

    /// Waits for a slot rather than refusing the moment the host's operation
    /// limit is reached: a second concurrent tool call is normal, and failing
    /// it outright made a limit the user never chose look like a conflict they
    /// caused.
    ///
    /// The listener is registered before each retry, so a slot released between
    /// the check and the park wakes this caller instead of being missed, and
    /// waiters are woken in the order they arrived.
    async fn reserve_preparation(&self) -> Result<PreparationPermit, WorkspaceError> {
        if let Some(permit) = self.try_reserve_preparation()? {
            return Ok(permit);
        }
        self.reclaim_settled_operations()?;
        let deadline = Instant::now() + RESERVATION_WAIT;
        loop {
            let listener = self.0.operation_slots.listen();
            if let Some(permit) = self.try_reserve_preparation()? {
                return Ok(permit);
            }
            match park_for_slot(listener, &self.0.cancellation, deadline).await {
                SlotWait::Retry => {}
                SlotWait::Cancelled => return Err(WorkspaceError::Cancelled),
                SlotWait::Exhausted => {
                    warn!(
                        limit = self.operation_limit(),
                        waited_ms = RESERVATION_WAIT.as_millis(),
                        "remote Workcell operation slots stayed exhausted for the whole wait"
                    );
                    return Err(WorkspaceError::Conflict);
                }
            }
        }
    }

    fn operation_limit(&self) -> usize {
        self.0
            .operations
            .lock()
            .map_or(0, |operations| operations.limit)
    }

    fn request_limit(&self, method: &str) -> u64 {
        if method == "tools/list" {
            self.0
                .descriptor
                .capabilities
                .tool_catalog
                .limits
                .max_request_bytes
        } else if matches!(
            method,
            contract::PREPARE_METHOD
                | contract::PREPARE_MUTATION_METHOD
                | contract::PREPARE_EXEC_METHOD
                | contract::SCM_PREPARE_MUTATION_METHOD
                | contract::SNAPSHOT_PREPARE_CAPTURE_METHOD
                | contract::SNAPSHOT_PREPARE_RESTORE_METHOD
                | contract::SNAPSHOT_PREPARE_UNREVERT_METHOD
                | contract::SNAPSHOT_PREPARE_CLEANUP_METHOD
                | contract::EXECUTE_METHOD
                | contract::STATUS_METHOD
                | contract::CANCEL_METHOD
                | contract::RELEASE_METHOD
        ) {
            self.0
                .descriptor
                .capabilities
                .tool_execution
                .limits
                .max_request_bytes
        } else {
            MAX_HTTP_RESPONSE_BYTES as u64
        }
    }

    async fn call<Req, Resp>(
        &self,
        method: &str,
        request: &Req,
        cancellation: &CancellationToken,
    ) -> Result<Resp, WorkspaceError>
    where
        Req: Serialize,
        Resp: DeserializeOwned,
    {
        let params = serde_json::to_value(request).map_err(|_| invalid_response())?;
        self.validate_outgoing_limits(&params)?;
        let value = self
            .0
            .transport
            .request(method, params, self.request_limit(method), cancellation)
            .await
            .map_err(|error| self.workspace_error(error))?;
        validate_response_binding(&value, &self.host_binding())?;
        serde_json::from_value(value).map_err(|_| invalid_response())
    }

    async fn call_execute(
        &self,
        request: &contract::ExecuteRequest,
        cancellation: &CancellationToken,
        stored: &StoredOperation,
    ) -> Result<contract::StatusResponse, RequestFailure> {
        let journal_operation_id = stored.journal.as_ref().map(|journal| &journal.operation_id);
        let params = serde_json::to_value(request).map_err(|_| RequestFailure {
            error: RemoteWorkcellError::InvalidProtocol,
            dispatched: false,
        })?;
        self.validate_outgoing_limits(&params)
            .map_err(|_| RequestFailure {
                error: RemoteWorkcellError::InvalidProtocol,
                dispatched: false,
            })?;
        let value = self
            .0
            .transport
            .request_tracked_with_dispatch(
                contract::EXECUTE_METHOD,
                params,
                self.request_limit(contract::EXECUTE_METHOD),
                cancellation,
                execution_timeout(&stored.binding),
                || {
                    journal_operation_id.map_or(Ok(()), |operation_id| {
                        self.0.mutation_journal.mark_dispatched(operation_id)
                    })
                },
            )
            .await?;
        validate_response_binding(&value, &self.host_binding()).map_err(|_| RequestFailure {
            error: RemoteWorkcellError::InvalidProtocol,
            dispatched: true,
        })?;
        serde_json::from_value(value).map_err(|_| RequestFailure {
            error: RemoteWorkcellError::InvalidProtocol,
            dispatched: true,
        })
    }

    fn validate_outgoing_limits(&self, value: &Value) -> Result<(), WorkspaceError> {
        let Some(workspace) = &self.0.descriptor.capabilities.workspace else {
            return Ok(());
        };
        validate_string_fields(
            value,
            &["path", "paths", "from", "to"],
            workspace.limits.max_path_bytes as usize,
        )?;
        validate_string_fields(
            value,
            &["cursor"],
            workspace.limits.max_cursor_bytes as usize,
        )
    }

    fn workspace_error(&self, error: RemoteWorkcellError) -> WorkspaceError {
        if error == RemoteWorkcellError::StaleResource {
            return resource_id(&self.host_binding().cwd_handle)
                .map_or(WorkspaceError::StaleCursor, |resource_id| {
                    WorkspaceError::StaleResource { resource_id }
                });
        }
        WorkspaceError::from(error)
    }

    fn remember_resource_id(
        &self,
        contract_id: &contract::ResourceId,
        path: &WorkspacePath,
    ) -> Result<ResourceId, WorkspaceError> {
        let id = resource_id(contract_id)?;
        let mut paths = self
            .0
            .paths
            .lock()
            .map_err(|_| WorkspaceError::Unavailable)?;
        paths.insert(id.clone(), path.clone());
        Ok(id)
    }

    fn remember_entry(
        &self,
        entry: &contract::WorkspaceEntry,
    ) -> Result<WorkspaceResource, WorkspaceError> {
        let path = workspace_path(&entry.path)?;
        let id = self.remember_resource_id(&entry.resource_id, &path)?;
        Ok(WorkspaceResource {
            project: self.0.project.clone(),
            scope: child_scope(self.0.root_cursor.scope(), id)?,
            path: Some(path),
            kind: match entry.kind {
                contract::WorkspaceEntryKind::File => ResourceKind::File,
                contract::WorkspaceEntryKind::Directory => ResourceKind::Directory,
            },
            revision: entry.revision.as_ref().map(resource_revision).transpose()?,
            size_bytes: entry.size_bytes,
        })
    }

    fn selector_path(
        &self,
        cursor: &WorkspaceCursor,
        selector: &ResourceSelector,
    ) -> Result<WorkspacePath, WorkspaceError> {
        let cursor_record = self.validate_context(&self.0.session_binding, cursor)?;
        match selector {
            ResourceSelector::Current => Ok(cursor_record.path),
            ResourceSelector::Path(path) => self.expected_response_path(cursor, path),
            ResourceSelector::Id(id) => self
                .0
                .paths
                .lock()
                .map_err(|_| WorkspaceError::Unavailable)?
                .get_path(id)
                .cloned()
                .ok_or_else(|| WorkspaceError::StaleResource {
                    resource_id: id.clone(),
                }),
        }
    }

    fn selector_contract_path(
        &self,
        cursor: &WorkspaceCursor,
        selector: &ResourceSelector,
    ) -> Result<contract::WorkspacePath, WorkspaceError> {
        let absolute = self.selector_path(cursor, selector)?;
        let current = self.validate_context(&self.0.session_binding, cursor)?.path;
        let relative = relative_path(&current, &absolute)?;
        contract_path(&relative)
    }

    fn prepared_handle(
        &self,
        response: &contract::PrepareResponse,
        prefix: &str,
        journal_policy: Option<String>,
        context: Option<PreparedWorkspaceContext>,
    ) -> Result<OperationHandle, WorkspaceError> {
        validate_prepare(response, &self.host_binding())?;
        let preparation_id = operation_id(&response.preparation_id)?;
        let invocation_id = operation_id(&self.next_identifier(prefix)?)?;
        let journal = journal_policy
            .map(|operation_kind| {
                Ok::<JournalOperation, WorkspaceError>(JournalOperation {
                    publication_cwd: None,
                    publication_id: None,
                    host_instance_id: self.host_binding().instance_id.as_str().to_owned(),
                    operation_id: OperationId::new(format!(
                        "caudra-journal-{}",
                        CaudraId::generate()
                    ))
                    .map_err(|_| invalid_response())?,
                    invocation_id: invocation_id.clone(),
                    preparation_id: preparation_id.clone(),
                    operation_kind,
                    request_digest: RequestDigest::sha256(
                        response.binding.argument_digest.as_str().to_owned(),
                    )
                    .map_err(|_| invalid_response())?,
                })
            })
            .transpose()?;
        let handle = OperationHandle {
            preparation_id: preparation_id.clone(),
            invocation_id: Some(invocation_id.clone()),
            execution_id: None,
            expires_at_unix_ms: Some(response.expires_at_unix_ms),
        };
        let mut operations = self
            .0
            .operations
            .lock()
            .map_err(|_| WorkspaceError::Unavailable)?;
        operations.insert(
            preparation_id,
            StoredOperation {
                transfer: None,
                binding: response.binding.clone(),
                context,
                invocation_id,
                expires_at_unix_ms: response.expires_at_unix_ms,
                journal,
                persisted: false,
            },
        )?;
        Ok(handle)
    }

    async fn execute_operation<T, F>(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        operation: &OperationHandle,
        parse: F,
    ) -> Result<OperationStatus<T>, WorkspaceError>
    where
        F: Fn(&Value) -> Result<T, WorkspaceError>,
    {
        self.validate_prepared_context(binding, cursor, operation)?;
        let stored = self
            .0
            .operations
            .lock()
            .map_err(|_| WorkspaceError::Unavailable)?
            .get(operation)?
            .clone();
        if let Some(journal) = &stored.journal {
            self.0
                .mutation_journal
                .reserve_at(journal, binding, cursor)?;
            self.0
                .operations
                .lock()
                .map_err(|_| WorkspaceError::Unavailable)?
                .mark_persisted(&operation.preparation_id)?;
        }
        let invocation = operation
            .invocation_id
            .as_ref()
            .ok_or_else(invalid_response)?;
        let request = contract::ExecuteRequest {
            version: contract::ContractVersion::V1,
            preparation_id: contract_identifier(&operation.preparation_id)?,
            invocation_id: contract_identifier(invocation)?,
            host: self.host_binding(),
        };
        match self
            .call_execute(&request, &self.0.cancellation.child_token(), &stored)
            .await
        {
            Ok(status) => {
                let converted = self.convert_remote_status(&status, operation, parse).await;
                if converted.is_err()
                    && let Some(journal) = &stored.journal
                {
                    self.0
                        .mutation_journal
                        .mark_indeterminate(&journal.operation_id)?;
                }
                converted
            }
            Err(failure) if failure.dispatched => {
                if let Some(journal) = &stored.journal {
                    self.0
                        .mutation_journal
                        .mark_indeterminate(&journal.operation_id)?;
                }
                let status_request = status_request(operation, &self.host_binding())?;
                let recovery = self
                    .call::<_, contract::StatusResponse>(
                        contract::STATUS_METHOD,
                        &status_request,
                        &CancellationToken::new(),
                    )
                    .await;
                match recovery {
                    Ok(status) => self.convert_remote_status(&status, operation, parse).await,
                    Err(_) => Ok(indeterminate_status(operation.clone())),
                }
            }
            Err(failure) => {
                if let Some(journal) = &stored.journal {
                    self.0
                        .mutation_journal
                        .release_before_dispatch(&journal.operation_id)?;
                }
                Err(self.workspace_error(failure.error))
            }
        }
    }

    async fn execute_tool_operation(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        operation: &OperationHandle,
    ) -> Result<OperationStatus<RemoteToolResultEnvelope>, RemoteToolExecutionError> {
        self.validate_prepared_context(binding, cursor, operation)?;
        let stored = self
            .0
            .operations
            .lock()
            .map_err(|_| WorkspaceError::Unavailable)?
            .get(operation)?
            .clone();
        if let Some(journal) = &stored.journal {
            self.0
                .mutation_journal
                .reserve_at(journal, binding, cursor)?;
            self.0
                .operations
                .lock()
                .map_err(|_| WorkspaceError::Unavailable)?
                .mark_persisted(&operation.preparation_id)?;
        }
        let invocation = operation
            .invocation_id
            .as_ref()
            .ok_or_else(invalid_response)?;
        let request = contract::ExecuteRequest {
            version: contract::ContractVersion::V1,
            preparation_id: contract_identifier(&operation.preparation_id)?,
            invocation_id: contract_identifier(invocation)?,
            host: self.host_binding(),
        };
        match self
            .call_execute(&request, &self.0.cancellation.child_token(), &stored)
            .await
        {
            Ok(status) => {
                let converted = self.convert_remote_tool_status(&status, operation).await;
                if converted.is_err()
                    && let Some(journal) = &stored.journal
                {
                    self.0
                        .mutation_journal
                        .mark_indeterminate(&journal.operation_id)
                        .map_err(RemoteToolExecutionError::PossiblyDispatched)?;
                }
                converted.map_err(RemoteToolExecutionError::PossiblyDispatched)
            }
            Err(failure) if failure.dispatched => {
                if let Some(journal) = &stored.journal {
                    self.0
                        .mutation_journal
                        .mark_indeterminate(&journal.operation_id)
                        .map_err(RemoteToolExecutionError::PossiblyDispatched)?;
                }
                let recovery = self
                    .call::<_, contract::StatusResponse>(
                        contract::STATUS_METHOD,
                        &status_request(operation, &self.host_binding())
                            .map_err(RemoteToolExecutionError::PossiblyDispatched)?,
                        &CancellationToken::new(),
                    )
                    .await;
                match recovery {
                    Ok(status) => self
                        .convert_remote_tool_status(&status, operation)
                        .await
                        .map_err(RemoteToolExecutionError::PossiblyDispatched),
                    Err(_) => Ok(indeterminate_status(operation.clone())),
                }
            }
            Err(failure) => {
                if let Some(journal) = &stored.journal {
                    self.0
                        .mutation_journal
                        .release_before_dispatch(&journal.operation_id)?;
                }
                Err(RemoteToolExecutionError::BeforeDispatch(
                    self.workspace_error(failure.error),
                ))
            }
        }
    }

    async fn convert_remote_tool_status(
        &self,
        response: &contract::StatusResponse,
        operation: &OperationHandle,
    ) -> Result<OperationStatus<RemoteToolResultEnvelope>, WorkspaceError> {
        self.validate_status_limits(response)?;
        let expected_operation = self
            .0
            .operations
            .lock()
            .map_err(|_| WorkspaceError::Unavailable)?
            .get(operation)?
            .clone();
        let envelope = response
            .outcome
            .as_ref()
            .filter(|outcome| outcome.kind == contract::OutcomeKind::Completed)
            .and_then(|outcome| outcome.result.as_ref())
            .map(remote_tool_result_envelope)
            .transpose()?;
        let status = convert_status(
            response,
            operation,
            &self.host_binding(),
            Some(&expected_operation.binding),
            |_| envelope.clone().ok_or_else(invalid_response),
        )?;
        let remove = if let Some(journal) = &expected_operation.journal {
            self.0
                .mutation_journal
                .commit_terminal(&journal.operation_id, &status.state)?
        } else {
            operation_is_terminal(&status.state)
        };
        if remove {
            self.0
                .operations
                .lock()
                .map_err(|_| WorkspaceError::Unavailable)?
                .remove(&operation.preparation_id);
            self.0.operation_slots.notify(1);
            self.release_confirmed_operation(
                &operation.preparation_id,
                &expected_operation.invocation_id,
            )
            .await;
        }
        Ok(status)
    }

    async fn operation_status<T, F>(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        operation: &OperationHandle,
        parse: F,
    ) -> Result<OperationStatus<T>, WorkspaceError>
    where
        F: Fn(&Value) -> Result<T, WorkspaceError>,
    {
        self.validate_prepared_context(binding, cursor, operation)?;
        self.0
            .operations
            .lock()
            .map_err(|_| WorkspaceError::Unavailable)?
            .get(operation)?;
        let response: contract::StatusResponse = self
            .call(
                contract::STATUS_METHOD,
                &status_request(operation, &self.host_binding())?,
                &self.0.cancellation.child_token(),
            )
            .await?;
        self.convert_remote_status(&response, operation, parse)
            .await
    }

    async fn convert_remote_status<T, F>(
        &self,
        response: &contract::StatusResponse,
        operation: &OperationHandle,
        parse: F,
    ) -> Result<OperationStatus<T>, WorkspaceError>
    where
        F: Fn(&Value) -> Result<T, WorkspaceError>,
    {
        self.validate_status_limits(response)?;
        let expected_operation = self
            .0
            .operations
            .lock()
            .map_err(|_| WorkspaceError::Unavailable)?
            .get(operation)?
            .clone();
        let status = convert_status(
            response,
            operation,
            &self.host_binding(),
            Some(&expected_operation.binding),
            parse,
        )?;
        let remove = if let Some(journal) = &expected_operation.journal {
            self.0
                .mutation_journal
                .commit_terminal(&journal.operation_id, &status.state)?
        } else {
            operation_is_terminal(&status.state)
        };
        if remove
            && !expected_operation
                .journal
                .as_ref()
                .is_some_and(|journal| transfer::is_directory_publication(&journal.operation_kind))
        {
            self.0
                .operations
                .lock()
                .map_err(|_| WorkspaceError::Unavailable)?
                .remove(&operation.preparation_id);
            self.0.operation_slots.notify(1);
            self.release_confirmed_operation(
                &operation.preparation_id,
                &expected_operation.invocation_id,
            )
            .await;
        }
        Ok(status)
    }

    fn validate_status_limits(
        &self,
        response: &contract::StatusResponse,
    ) -> Result<(), WorkspaceError> {
        let limits = &self
            .0
            .descriptor
            .capabilities
            .operations
            .as_ref()
            .ok_or_else(invalid_response)?
            .limits;
        let progress_bytes = serialized_items_bytes(&response.progress)?;
        if response.progress.len() > limits.max_progress_events as usize
            || progress_bytes as u64 > limits.max_progress_bytes
        {
            return Err(invalid_response());
        }
        Ok(())
    }

    async fn cancel_operation(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        operation: &OperationHandle,
    ) -> Result<CancellationResult, WorkspaceError> {
        self.validate_prepared_context(binding, cursor, operation)?;
        let stored = self
            .0
            .operations
            .lock()
            .map_err(|_| WorkspaceError::Unavailable)?
            .get(operation)?
            .clone();
        let invocation = operation
            .invocation_id
            .as_ref()
            .ok_or_else(invalid_response)?;
        let response: contract::CancelResponse = self
            .call(
                contract::CANCEL_METHOD,
                &contract::CancelRequest {
                    version: contract::ContractVersion::V1,
                    preparation_id: contract_identifier(&operation.preparation_id)?,
                    invocation_id: contract_identifier(invocation)?,
                    host: self.host_binding(),
                },
                &self.0.cancellation.child_token(),
            )
            .await?;
        validate_v1(response.version)?;
        let state = operation_phase(response.state);
        if !matches!(state, OperationPhase::Prepared | OperationPhase::Running)
            && stored.journal.is_none()
        {
            self.0
                .operations
                .lock()
                .map_err(|_| WorkspaceError::Unavailable)?
                .remove(&operation.preparation_id);
            self.0.operation_slots.notify(1);
        }
        Ok(CancellationResult {
            state,
            cancellation_requested: response.cancellation_requested,
        })
    }

    async fn release_operation(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        operation: &OperationHandle,
    ) -> Result<ReleaseResult, WorkspaceError> {
        self.validate_prepared_context(binding, cursor, operation)?;
        let stored = self
            .0
            .operations
            .lock()
            .map_err(|_| WorkspaceError::Unavailable)?
            .get(operation)?
            .clone();
        if let Some(journal) = &stored.journal
            && self.0.mutation_journal.contains(&journal.operation_id)?
        {
            return Err(WorkspaceError::PendingOperation {
                operation_id: journal.operation_id.as_str().to_owned(),
            });
        }
        let response: contract::ReleaseResponse = self
            .call(
                contract::RELEASE_METHOD,
                &contract::ReleaseRequest {
                    version: contract::ContractVersion::V1,
                    selector: contract::OperationSelector {
                        preparation_id: contract_identifier(&operation.preparation_id)?,
                        invocation_id: None,
                        host: self.host_binding(),
                    },
                },
                &self.0.cancellation.child_token(),
            )
            .await?;
        validate_v1(response.version)?;
        if response.released {
            self.0
                .operations
                .lock()
                .map_err(|_| WorkspaceError::Unavailable)?
                .remove(&operation.preparation_id);
            self.0.operation_slots.notify(1);
        }
        Ok(ReleaseResult {
            state: operation_phase(response.state),
            released: response.released,
        })
    }
}

#[async_trait]
impl WorkspaceControlService for RemoteWorkcellClient {
    async fn execute(&self, command: WorkspaceControlCommand) -> Result<String, WorkspaceError> {
        match command {
            WorkspaceControlCommand::Reconnect => self.reconnect(&self.0.cancellation).await?,
            WorkspaceControlCommand::Reconcile => self.recover_pending_operations().await?,
            WorkspaceControlCommand::Acknowledge(id) => {
                self.acknowledge_pending_operation(&id)?;
            }
            WorkspaceControlCommand::Status | WorkspaceControlCommand::Pending => {}
        }
        let (reachable, unreachable): (Vec<_>, Vec<_>) = self
            .pending_remote_operations()
            .into_iter()
            .partition(|operation| operation.reachable);
        let mut output = format!(
            "Remote: {:?}; pending operations: {}. No mutation resent.",
            self.connection_status(),
            reachable.len()
        );
        push_control_operations(&mut output, &reachable)?;
        if !unreachable.is_empty() {
            output.push_str(&format!(
                "\n{UNREACHABLE_OPERATIONS_HEADING}: {}. {UNREACHABLE_OPERATIONS_REMEDY}",
                unreachable.len()
            ));
            push_control_operations(&mut output, &unreachable)?;
        }
        Ok(output)
    }
}

fn push_control_operations(
    output: &mut String,
    operations: &[PendingRemoteOperation],
) -> Result<(), WorkspaceError> {
    for operation in operations.iter().take(MAX_CONTROL_OPERATIONS) {
        let id = serde_json::to_string(operation.operation_id.as_str())
            .map_err(|_| invalid_response())?;
        output.push_str(&format!(
            "\n{id}: {:?} {}",
            operation.state, operation.operation_kind
        ));
    }
    if operations.len() > MAX_CONTROL_OPERATIONS {
        output.push_str(&format!(
            "\nShowing the first {MAX_CONTROL_OPERATIONS}; resolve these to see the rest."
        ));
    }
    Ok(())
}

#[async_trait]
impl WorkspaceReadService for RemoteWorkcellClient {
    async fn navigate_directory(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        navigation: &DirectoryNavigation,
    ) -> Result<ResolvedWorkspaceDirectory, WorkspaceError> {
        self.navigate_directory_cursor(binding, cursor, navigation)
            .await
    }

    async fn resolve(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        path: &WorkspacePath,
    ) -> Result<WorkspaceResource, WorkspaceError> {
        self.require_capability(WorkspaceCapability::Resolve)?;
        let expected = self.expected_response_path(cursor, path)?;
        let response: contract::StatResponse = self
            .call(
                contract::STAT_METHOD,
                &contract::StatRequest {
                    version: contract::ContractVersion::V1,
                    binding: self.bind_workspace_request(binding, cursor)?,
                    path: contract_path(path)?,
                },
                &self.0.cancellation.child_token(),
            )
            .await?;
        validate_v1(response.version)?;
        if workspace_path(&response.entry.path)? != expected {
            return Err(invalid_path());
        }
        self.remember_entry(&response.entry)
    }

    async fn resolve_directory(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        path: &WorkspacePath,
    ) -> Result<ResolvedWorkspaceDirectory, WorkspaceError> {
        self.resolve_directory_cursor(binding, cursor, path).await
    }

    async fn stat(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        resource: &ResourceSelector,
    ) -> Result<WorkspaceResource, WorkspaceError> {
        self.require_capability(WorkspaceCapability::Stat)?;
        let absolute = self.selector_path(cursor, resource)?;
        let relative = relative_path(&self.validate_context(binding, cursor)?.path, &absolute)?;
        let response: contract::StatResponse = self
            .call(
                contract::STAT_METHOD,
                &contract::StatRequest {
                    version: contract::ContractVersion::V1,
                    binding: self.bind_workspace_request(binding, cursor)?,
                    path: contract_path(&relative)?,
                },
                &self.0.cancellation.child_token(),
            )
            .await?;
        validate_v1(response.version)?;
        if workspace_path(&response.entry.path)? != absolute {
            return Err(invalid_path());
        }
        validate_selector_id(resource, &resource_id(&response.entry.resource_id)?)?;
        let returned = self.remember_entry(&response.entry)?;
        Ok(returned)
    }

    async fn list(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &ListRequest,
    ) -> Result<ListPage, WorkspaceError> {
        self.require_capability(WorkspaceCapability::List)?;
        let limits = &self
            .0
            .descriptor
            .capabilities
            .workspace
            .as_ref()
            .ok_or(WorkspaceError::UnsupportedCapability {
                capability: WorkspaceCapability::List,
            })?
            .limits;
        require_nonzero_within(request.limit, limits.max_page_size)?;
        let parent = self.selector_path(cursor, &request.parent)?;
        let response: contract::ListResponse = self
            .call(
                contract::LIST_METHOD,
                &contract::ListRequest {
                    version: contract::ContractVersion::V1,
                    binding: self.bind_workspace_request(binding, cursor)?,
                    path: self.selector_contract_path(cursor, &request.parent)?,
                    recursive: request.recursive,
                    page_size: request.limit,
                    cursor: request
                        .continuation
                        .as_ref()
                        .map(continuation_contract)
                        .transpose()?,
                },
                &self.0.cancellation.child_token(),
            )
            .await?;
        validate_v1(response.version)?;
        if response.entries.len() > request.limit as usize
            || response
                .next_cursor
                .as_ref()
                .is_some_and(|cursor| cursor.as_str().len() > limits.max_cursor_bytes as usize)
        {
            return Err(invalid_response());
        }
        let resources = response
            .entries
            .iter()
            .map(|entry| {
                let path = workspace_path(&entry.path)?;
                if !path_within(&parent, &path, request.recursive) {
                    return Err(invalid_path());
                }
                self.remember_entry(entry)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let (truncated, incomplete) = pagination_flags(
            response.truncated || response.incomplete,
            response.next_cursor.is_some(),
        );
        Ok(ListPage {
            revision: collection_revision(&response.revision)?,
            resources,
            truncated,
            incomplete,
            continuation: response
                .next_cursor
                .as_ref()
                .map(continuation)
                .transpose()?,
        })
    }

    async fn read_text(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &ReadTextRequest,
    ) -> Result<TextContent, WorkspaceError> {
        self.require_capability(WorkspaceCapability::ReadText)?;
        let limit = self
            .0
            .descriptor
            .capabilities
            .workspace
            .as_ref()
            .ok_or(WorkspaceError::UnsupportedCapability {
                capability: WorkspaceCapability::ReadText,
            })?
            .limits
            .max_text_read_bytes;
        require_nonzero_within(request.max_bytes, limit)?;
        if let Some(range) = request.range
            && (range.start_line == 0 || range.end_line.is_some_and(|end| end < range.start_line))
        {
            return Err(invalid_response());
        }
        let expected_path = self.selector_path(cursor, &request.resource)?;
        let response: contract::ReadTextResponse = self
            .call(
                contract::READ_TEXT_METHOD,
                &contract::ReadTextRequest {
                    version: contract::ContractVersion::V1,
                    binding: self.bind_workspace_request(binding, cursor)?,
                    path: self.selector_contract_path(cursor, &request.resource)?,
                    range: request.range.map(|range| contract::TextRange {
                        start_line: range.start_line,
                        end_line: range.end_line,
                    }),
                    byte_offset: request.byte_offset,
                    max_bytes: request.max_bytes,
                },
                &self.0.cancellation.child_token(),
            )
            .await?;
        validate_v1(response.version)?;
        let path = workspace_path(&response.path)?;
        let start_matches = request
            .range
            .map_or(response.start_byte == request.byte_offset, |range| {
                response.start_line == range.start_line
            });
        if path != expected_path
            || !start_matches
            || response.end_byte < response.start_byte
            || response.end_byte - response.start_byte != response.text.len() as u64
            || response.text.len() > request.max_bytes as usize
            || response.end_line < response.start_line
            || response.end_line > response.total_lines
            || request
                .range
                .and_then(|range| range.end_line)
                .is_some_and(|requested_end| response.end_line > requested_end)
            || response.truncated != response.next_byte_offset.is_some()
            || response
                .next_byte_offset
                .is_some_and(|next| next != response.end_byte)
        {
            return Err(invalid_response());
        }
        validate_selector_id(&request.resource, &resource_id(&response.resource_id)?)?;
        let id = self.remember_resource_id(&response.resource_id, &path)?;
        Ok(TextContent {
            text: response.text,
            resource_id: id,
            revision: resource_revision(&response.revision)?,
            path,
            start_line: response.start_line,
            end_line: response.end_line,
            total_lines: response.total_lines,
            start_byte: response.start_byte,
            end_byte: response.end_byte,
            truncated: response.truncated,
            next_byte_offset: response.next_byte_offset,
        })
    }

    async fn read_bytes(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &ReadBytesRequest,
    ) -> Result<ByteContent, WorkspaceError> {
        self.require_capability(WorkspaceCapability::ReadBytes)?;
        self.read_reviewed_bytes(binding, cursor, request).await
    }
}

#[async_trait]
impl WorkspaceSearchService for RemoteWorkcellClient {
    async fn search(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &SearchRequest,
    ) -> Result<SearchPage, WorkspaceError> {
        self.require_capability(WorkspaceCapability::Search)?;
        let workspace = self.0.descriptor.capabilities.workspace.as_ref().ok_or(
            WorkspaceError::UnsupportedCapability {
                capability: WorkspaceCapability::Search,
            },
        )?;
        require_nonzero_within(request.max_results, workspace.limits.max_page_size)?;
        if request.query.is_empty()
            || request.query.len() > workspace.limits.max_search_pattern_bytes as usize
            || request.include.as_ref().is_some_and(String::is_empty)
        {
            return Err(invalid_response());
        }
        let root = self.selector_path(cursor, &request.root)?;
        let response: contract::SearchTextResponse = self
            .call(
                contract::SEARCH_TEXT_METHOD,
                &contract::SearchTextRequest {
                    version: contract::ContractVersion::V1,
                    binding: self.bind_workspace_request(binding, cursor)?,
                    path: self.selector_contract_path(cursor, &request.root)?,
                    pattern: contract::SearchPattern::new(request.query.clone())
                        .map_err(|_| invalid_response())?,
                    include: request
                        .include
                        .clone()
                        .map(contract::IncludePattern::new)
                        .transpose()
                        .map_err(|_| invalid_response())?,
                    page_size: request.max_results,
                    cursor: request
                        .continuation
                        .as_ref()
                        .map(continuation_contract)
                        .transpose()?,
                },
                &self.0.cancellation.child_token(),
            )
            .await?;
        validate_v1(response.version)?;
        if response.matches.len() > request.max_results as usize
            || response.next_cursor.as_ref().is_some_and(|cursor| {
                cursor.as_str().len() > workspace.limits.max_cursor_bytes as usize
            })
        {
            return Err(invalid_response());
        }
        let hits = response
            .matches
            .iter()
            .map(|hit| {
                let path = workspace_path(&hit.path)?;
                if !path_within(&root, &path, true) || hit.line == 0 {
                    return Err(invalid_response());
                }
                let resource_id = self.remember_resource_id(&hit.resource_id, &path)?;
                Ok(SearchHit {
                    resource: WorkspaceResource {
                        project: self.0.project.clone(),
                        scope: child_scope(self.0.root_cursor.scope(), resource_id)?,
                        path: Some(path),
                        kind: ResourceKind::File,
                        revision: Some(resource_revision(&hit.revision)?),
                        size_bytes: None,
                    },
                    line: hit.line,
                    text: hit.text.clone(),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let (truncated, incomplete) =
            pagination_flags(response.truncated, response.next_cursor.is_some());
        Ok(SearchPage {
            revision: collection_revision(&response.revision)?,
            hits,
            scan_counts: SearchScanCounts {
                files_scanned: response.files_scanned,
                files_listed: response.files_listed,
            },
            truncated,
            incomplete,
            continuation: response
                .next_cursor
                .as_ref()
                .map(continuation)
                .transpose()?,
        })
    }
}

#[async_trait]
impl WorkspaceWatchService for RemoteWorkcellClient {
    async fn open(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &WatchOpenRequest,
    ) -> Result<WatchSubscription, WorkspaceError> {
        self.require_capability(WorkspaceCapability::WatchOpen)?;
        if request.recursive {
            self.require_capability(WorkspaceCapability::WatchRecursive)?;
        }
        let root = self.selector_path(cursor, &request.root)?;
        let response: contract::WatchOpenResponse = self
            .call(
                contract::WATCH_OPEN_METHOD,
                &contract::WatchOpenRequest {
                    version: contract::ContractVersion::V1,
                    binding: self.bind_workspace_request(binding, cursor)?,
                    path: self.selector_contract_path(cursor, &request.root)?,
                    recursive: request.recursive,
                },
                &self.0.cancellation.child_token(),
            )
            .await?;
        validate_v1(response.version)?;
        if response.state != contract::WatchState::Current {
            return Err(invalid_response());
        }
        let subscription_id = watch_subscription_id(&response.subscription_id)?;
        let watch_cursor = watch_cursor(&response.cursor)?;
        self.0
            .watches
            .lock()
            .map_err(|_| WorkspaceError::Unavailable)?
            .insert(
                subscription_id.clone(),
                WatchRecord {
                    workspace_cursor: cursor.clone(),
                    cursor: watch_cursor.clone(),
                    root,
                    recursive: request.recursive,
                },
            );
        Ok(WatchSubscription {
            subscription_id,
            cursor: watch_cursor,
            expires_at_unix_ms: response.expires_at_unix_ms,
        })
    }

    async fn poll(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &WatchPollRequest,
    ) -> Result<WatchEventPage, WorkspaceError> {
        self.require_capability(WorkspaceCapability::WatchPoll)?;
        let limits = &self
            .0
            .descriptor
            .capabilities
            .watch
            .as_ref()
            .ok_or(WorkspaceError::UnsupportedCapability {
                capability: WorkspaceCapability::WatchPoll,
            })?
            .limits;
        require_nonzero_within(request.max_events, u32::MAX)?;
        require_nonzero_within(request.max_bytes, u32::MAX)?;
        let max_events = request.max_events.min(limits.max_poll_events);
        let max_bytes = request.max_bytes.min(limits.max_poll_bytes);
        let wait_ms = request.wait_ms.min(limits.max_wait_ms);
        let wire_binding = self.bind_workspace_request(binding, cursor)?;
        let known = {
            let mut watches = self
                .0
                .watches
                .lock()
                .map_err(|_| WorkspaceError::Unavailable)?;
            let ttl = watches.ttl;
            let entry = watches
                .records
                .get_mut(&request.subscription_id)
                .ok_or(WorkspaceError::StaleCursor)?;
            if entry.value.workspace_cursor != *cursor || entry.value.cursor != request.cursor {
                return Err(WorkspaceError::StaleCursor);
            }
            if entry.touched.elapsed() > ttl {
                watches.records.remove(&request.subscription_id);
                return Ok(WatchEventPage {
                    subscription_id: request.subscription_id.clone(),
                    state: WatchPollState::FullResync {
                        reason: WatchResyncReason::SubscriptionExpired,
                    },
                    sequence: SequenceMetadata {
                        first_retained_sequence: None,
                        next_sequence: WATCH_INITIAL_SEQUENCE,
                        gap_before_first: true,
                    },
                    events: Vec::new(),
                });
            }
            entry.touched = Instant::now();
            entry.value.clone()
        };
        let response: contract::WatchPollResponse = self
            .call(
                contract::WATCH_POLL_METHOD,
                &contract::WatchPollRequest {
                    version: contract::ContractVersion::V1,
                    binding: wire_binding,
                    subscription_id: contract_identifier(&request.subscription_id)?,
                    cursor: contract::Cursor::new(request.cursor.as_str())
                        .map_err(|_| WorkspaceError::StaleCursor)?,
                    max_events,
                    max_bytes,
                    wait_ms,
                },
                &self.0.cancellation.child_token(),
            )
            .await?;
        validate_v1(response.version)?;
        response.validate().map_err(|_| invalid_response())?;
        if response.subscription_id.as_str() != request.subscription_id.as_str() {
            return Err(WorkspaceError::IdentityMismatch);
        }
        if response.events.len() > max_events as usize
            || serialized_items_bytes(&response.events)? > max_bytes as usize
        {
            return Err(invalid_response());
        }
        let events = response
            .events
            .iter()
            .map(|event| {
                let path = workspace_path(&event.path)?;
                if !watch_path_within(&known.root, &path, known.recursive) {
                    return Err(invalid_path());
                }
                Ok(WorkspaceEvent {
                    sequence: event.sequence,
                    kind: match event.kind {
                        contract::WatchEventKind::Create => WorkspaceEventKind::Created,
                        contract::WatchEventKind::Modify => WorkspaceEventKind::Changed,
                        contract::WatchEventKind::Remove => WorkspaceEventKind::Removed,
                        contract::WatchEventKind::Rescan => WorkspaceEventKind::Rescan,
                    },
                    path,
                    previous_path: None,
                })
            })
            .collect::<Result<Vec<_>, WorkspaceError>>()?;
        let gap_before_first = response.state == contract::WatchState::FullResync;
        let state = match response.state {
            contract::WatchState::Current => WatchPollState::Current {
                next_cursor: watch_cursor(
                    response.next_cursor.as_ref().ok_or_else(invalid_response)?,
                )?,
                expires_at_unix_ms: response.expires_at_unix_ms.ok_or_else(invalid_response)?,
            },
            contract::WatchState::FullResync => WatchPollState::FullResync {
                reason: watch_resync_reason(response.resync_reason.ok_or_else(invalid_response)?),
            },
        };
        if let WatchPollState::Current { next_cursor, .. } = &state {
            let mut next = known;
            next.cursor = next_cursor.clone();
            self.0
                .watches
                .lock()
                .map_err(|_| WorkspaceError::Unavailable)?
                .insert(request.subscription_id.clone(), next);
        } else {
            self.0
                .watches
                .lock()
                .map_err(|_| WorkspaceError::Unavailable)?
                .records
                .remove(&request.subscription_id);
        }
        Ok(WatchEventPage {
            subscription_id: request.subscription_id.clone(),
            state,
            sequence: SequenceMetadata {
                first_retained_sequence: response.first_retained_sequence,
                next_sequence: response.next_sequence,
                gap_before_first,
            },
            events,
        })
    }

    async fn close(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        subscription_id: &WatchSubscriptionId,
    ) -> Result<WatchCloseResult, WorkspaceError> {
        self.require_capability(WorkspaceCapability::WatchClose)?;
        let wire_binding = self.bind_workspace_request(binding, cursor)?;
        if self
            .0
            .watches
            .lock()
            .map_err(|_| WorkspaceError::Unavailable)?
            .records
            .get(subscription_id)
            .is_none_or(|entry| entry.value.workspace_cursor != *cursor)
        {
            return Err(WorkspaceError::StaleCursor);
        }
        let response: contract::WatchCloseResponse = self
            .call(
                contract::WATCH_CLOSE_METHOD,
                &contract::WatchCloseRequest {
                    version: contract::ContractVersion::V1,
                    binding: wire_binding,
                    subscription_id: contract_identifier(subscription_id)?,
                },
                &self.0.cancellation.child_token(),
            )
            .await?;
        validate_v1(response.version)?;
        if response.subscription_id.as_str() != subscription_id.as_str() {
            return Err(invalid_response());
        }
        self.0
            .watches
            .lock()
            .map_err(|_| WorkspaceError::Unavailable)?
            .records
            .remove(subscription_id);
        Ok(WatchCloseResult {
            subscription_id: subscription_id.clone(),
            closed: response.closed,
        })
    }
}

#[async_trait]
impl WorkspaceMutationService for RemoteWorkcellClient {
    async fn execute(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &MutationRequest,
    ) -> Result<OperationStatus<MutationResult>, WorkspaceError> {
        self.require_capability(WorkspaceCapability::MutationExecute)?;
        if request.mutations.iter().any(|mutation| {
            matches!(
                mutation,
                Mutation::Write {
                    content: WriteContent::Bytes(_),
                    ..
                }
            )
        }) {
            return self.execute_binary_mutation(binding, cursor, request).await;
        }
        let capability = self
            .0
            .descriptor
            .capabilities
            .workspace_mutation
            .as_ref()
            .ok_or(WorkspaceError::UnsupportedCapability {
                capability: WorkspaceCapability::MutationExecute,
            })?;
        if request.mutations.is_empty()
            || request.mutations.len() > capability.max_mutations as usize
        {
            return Err(invalid_response());
        }
        let mutations = request
            .mutations
            .iter()
            .map(workspace_mutation)
            .collect::<Result<Vec<_>, _>>()?;
        let expected_results = request
            .mutations
            .iter()
            .map(|mutation| {
                expected_mutation_result(mutation, |path| self.expected_response_path(cursor, path))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let content_bytes = request
            .mutations
            .iter()
            .map(mutation_content_bytes)
            .sum::<usize>();
        if content_bytes > capability.max_content_bytes as usize {
            return Err(invalid_response());
        }
        let preparation_permit = self.reserve_preparation().await?;
        let response: contract::PrepareResponse = self
            .call(
                contract::PREPARE_MUTATION_METHOD,
                &contract::PrepareMutationRequest {
                    version: contract::ContractVersion::V1,
                    binding: self.bind_workspace_request(binding, cursor)?,
                    mutations,
                },
                &self.0.cancellation.child_token(),
            )
            .await?;
        validate_fixed_contract(
            &response.binding.contract,
            contract::WORKSPACE_MUTATION_CONTRACT_ID,
        )?;
        let operation = self.prepared_handle(
            &response,
            "mutation",
            Some(WORKSPACE_MUTATION_KIND.to_owned()),
            Some(PreparedWorkspaceContext {
                binding: binding.clone(),
                cursor: cursor.clone(),
            }),
        )?;
        drop(preparation_permit);
        let status = self
            .execute_operation(binding, cursor, &operation, |value| {
                parse_mutation_result(value, &expected_results)
            })
            .await?;
        if let OperationState::Completed { result, .. } = &status.state {
            let mut paths = self
                .0
                .paths
                .lock()
                .map_err(|_| WorkspaceError::Unavailable)?;
            invalidate_mutation_aliases(&mut paths, result);
        }
        Ok(status)
    }

    async fn status(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        operation: &OperationHandle,
    ) -> Result<OperationStatus<MutationResult>, WorkspaceError> {
        self.require_capability(WorkspaceCapability::MutationStatus)?;
        let transfer = self
            .0
            .operations
            .lock()
            .map_err(|_| WorkspaceError::Unavailable)?
            .get(operation)?
            .transfer
            .clone();
        if let Some(prepared) = transfer {
            return self
                .binary_mutation_status(binding, cursor, &prepared)
                .await;
        }
        self.operation_status(binding, cursor, operation, parse_mutation_result_unbound)
            .await
    }

    async fn cancel(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        operation: &OperationHandle,
    ) -> Result<CancellationResult, WorkspaceError> {
        self.require_capability(WorkspaceCapability::MutationCancel)?;
        self.cancel_operation(binding, cursor, operation).await
    }
}

#[async_trait]
impl WorkspaceExecService for RemoteWorkcellClient {
    async fn execute(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &ExecRequest,
    ) -> Result<OperationStatus<Value>, WorkspaceError> {
        self.require_capability(WorkspaceCapability::ExecExecute)?;
        let capability = self.0.descriptor.capabilities.direct_exec.as_ref().ok_or(
            WorkspaceError::UnsupportedCapability {
                capability: WorkspaceCapability::ExecExecute,
            },
        )?;
        // Zero is the server's own way of asking for its maximum, so it passes
        // through rather than being second-guessed here.
        if request.command.as_str().len() > capability.max_command_bytes as usize
            || request
                .timeout_ms
                .is_some_and(|timeout| timeout > capability.max_timeout_ms)
        {
            return Err(invalid_response());
        }
        let preparation_permit = self.reserve_preparation().await?;
        let response: contract::PrepareResponse = self
            .call(
                contract::PREPARE_EXEC_METHOD,
                &contract::PrepareExecRequest {
                    version: contract::ContractVersion::V1,
                    binding: self.bind_workspace_request(binding, cursor)?,
                    options: contract::DirectExecOptions {
                        command: contract::CommandText::new(request.command.as_str())
                            .map_err(|_| invalid_response())?,
                        timeout_ms: request.timeout_ms,
                    },
                },
                &self.0.cancellation.child_token(),
            )
            .await?;
        validate_fixed_contract(
            &response.binding.contract,
            contract::DIRECT_EXEC_CONTRACT_ID,
        )?;
        let operation = self.prepared_handle(
            &response,
            "exec",
            Some(DIRECT_EXEC_KIND.to_owned()),
            Some(PreparedWorkspaceContext {
                binding: binding.clone(),
                cursor: cursor.clone(),
            }),
        )?;
        drop(preparation_permit);
        self.execute_operation(binding, cursor, &operation, |value| Ok(value.clone()))
            .await
    }

    async fn status(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        operation: &OperationHandle,
    ) -> Result<OperationStatus<Value>, WorkspaceError> {
        self.require_capability(WorkspaceCapability::ExecStatus)?;
        self.operation_status(binding, cursor, operation, |value| Ok(value.clone()))
            .await
    }

    async fn cancel(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        operation: &OperationHandle,
    ) -> Result<CancellationResult, WorkspaceError> {
        self.require_capability(WorkspaceCapability::ExecCancel)?;
        self.cancel_operation(binding, cursor, operation).await
    }
}

#[async_trait]
impl WorkspaceAssetService for RemoteWorkcellClient {
    async fn discover(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
    ) -> Result<ProjectAssetManifest, WorkspaceError> {
        self.require_capability(WorkspaceCapability::ProjectAssetsDiscover)?;
        let response: contract::DiscoverProjectAssetsResponse = self
            .call(
                contract::DISCOVER_PROJECT_ASSETS_METHOD,
                &contract::DiscoverProjectAssetsRequest {
                    version: contract::ContractVersion::V1,
                    binding: self.bind_workspace_request(binding, cursor)?,
                },
                &self.0.cancellation.child_token(),
            )
            .await?;
        validate_v1(response.version)?;
        let capability = self
            .0
            .descriptor
            .capabilities
            .project_assets
            .as_ref()
            .ok_or_else(invalid_response)?;
        if response.manifest.version != capability.manifest_version
            || response.manifest.assets.len() > capability.limits.max_assets as usize
        {
            return Err(invalid_response());
        }
        Ok(ProjectAssetManifest {
            version: operation_id(&response.manifest.version)?,
            revision: collection_revision(&response.manifest.revision)?,
            assets: response
                .manifest
                .assets
                .iter()
                .map(|asset| self.project_asset(asset))
                .collect::<Result<Vec<_>, _>>()?,
            unreadable: response
                .manifest
                .unreadable
                .iter()
                .map(workspace_path)
                .collect::<Result<Vec<_>, _>>()?,
        })
    }

    async fn read(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        asset: &ProjectAsset,
        max_bytes: u32,
    ) -> Result<ProjectAssetContent, WorkspaceError> {
        self.require_capability(WorkspaceCapability::ProjectAssetsRead)?;
        let limit = self
            .0
            .descriptor
            .capabilities
            .project_assets
            .as_ref()
            .ok_or_else(invalid_response)?
            .limits
            .max_read_bytes;
        let max_bytes = max_bytes.min(limit);
        require_nonzero_within(max_bytes, limit)?;
        let response: contract::ReadProjectAssetResponse = self
            .call(
                contract::READ_PROJECT_ASSET_METHOD,
                &contract::ReadProjectAssetRequest {
                    version: contract::ContractVersion::V1,
                    binding: self.bind_workspace_request(binding, cursor)?,
                    path: contract_path(&asset.path)?,
                    expected_revision: contract_revision(&asset.revision)?,
                    max_bytes,
                },
                &self.0.cancellation.child_token(),
            )
            .await?;
        validate_v1(response.version)?;
        if response.encoding != contract::ProjectAssetEncoding::Utf8 {
            return Err(invalid_response());
        }
        let returned = self.project_asset(&response.asset)?;
        if &returned != asset || response.content.len() > max_bytes as usize {
            return Err(invalid_response());
        }
        Ok(ProjectAssetContent {
            asset: returned,
            content: response.content.as_str().to_owned(),
            truncated: response.truncated,
        })
    }
}

impl RemoteWorkcellClient {
    fn project_asset(
        &self,
        asset: &contract::ProjectAsset,
    ) -> Result<ProjectAsset, WorkspaceError> {
        let path = workspace_path(&asset.path)?;
        let resource_id = self.remember_resource_id(&asset.resource_id, &path)?;
        Ok(ProjectAsset {
            path,
            resource_id,
            revision: resource_revision(&asset.revision)?,
            kind: project_asset_kind(asset.kind),
            trust: project_asset_trust(asset.trust),
            size_bytes: asset.size_bytes,
        })
    }
}

fn project_asset_kind(kind: contract::ProjectAssetKind) -> ProjectAssetKind {
    match kind {
        contract::ProjectAssetKind::Instructions => ProjectAssetKind::Instructions,
        contract::ProjectAssetKind::Skill => ProjectAssetKind::Skill,
        contract::ProjectAssetKind::Command => ProjectAssetKind::Command,
        contract::ProjectAssetKind::Workflow => ProjectAssetKind::Workflow,
        contract::ProjectAssetKind::Permissions => ProjectAssetKind::Permissions,
    }
}

fn project_asset_trust(trust: contract::ProjectAssetTrust) -> ProjectAssetTrust {
    match trust {
        contract::ProjectAssetTrust::Declarative => ProjectAssetTrust::Declarative,
        contract::ProjectAssetTrust::ClientApprovalRequired => {
            ProjectAssetTrust::ClientApprovalRequired
        }
        contract::ProjectAssetTrust::MixedReviewRequired => ProjectAssetTrust::MixedReviewRequired,
    }
}

#[async_trait]
impl WorkspaceToolService for RemoteWorkcellClient {
    async fn prepare(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &ToolPrepareRequest,
    ) -> Result<PreparedToolCall, WorkspaceError> {
        self.require_capability(WorkspaceCapability::ToolPrepare)?;
        self.validate_context(binding, cursor)?;
        let spec =
            self.0
                .catalog
                .get(&request.name)
                .ok_or(WorkspaceError::UnsupportedCapability {
                    capability: WorkspaceCapability::ToolPrepare,
                })?;
        let max_arguments = self
            .0
            .descriptor
            .capabilities
            .operations
            .as_ref()
            .ok_or_else(invalid_response)?
            .limits
            .max_argument_bytes;
        let wire = contract::PrepareRequest {
            cwd_handle: Some(
                contract::ResourceId::new(cursor.cwd_handle().as_str())
                    .map_err(|_| invalid_response())?,
            ),
            version: contract::ContractVersion::V1,
            host: self.host_binding(),
            tool: contract::ToolName::new(spec.name.clone()).map_err(|_| invalid_response())?,
            contract: contract_binding(spec)?,
            arguments: request.input.clone(),
        };
        wire.validate(max_arguments)
            .map_err(|_| invalid_response())?;
        let _permit = self.reserve_preparation().await?;
        let response: contract::PrepareResponse = self
            .call(
                contract::PREPARE_METHOD,
                &wire,
                &self.0.cancellation.child_token(),
            )
            .await?;
        validate_contract_binding(&response.binding.contract, spec)?;
        Ok(PreparedToolCall {
            operation: self.prepared_handle(
                &response,
                "tool",
                canonical_journal_policy(&request.name, &response.intent),
                Some(PreparedWorkspaceContext {
                    binding: binding.clone(),
                    cursor: cursor.clone(),
                }),
            )?,
            canonical_input: request.input.clone(),
            review: serde_json::to_value(response.intent).map_err(|_| invalid_response())?,
        })
    }

    async fn execute(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        prepared: &PreparedToolCall,
    ) -> Result<OperationStatus<Value>, WorkspaceError> {
        self.require_capability(WorkspaceCapability::ToolExecute)?;
        self.execute_operation(binding, cursor, &prepared.operation, |value| {
            Ok(value.clone())
        })
        .await
    }

    async fn status(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        operation: &OperationHandle,
    ) -> Result<OperationStatus<Value>, WorkspaceError> {
        self.require_capability(WorkspaceCapability::ToolStatus)?;
        self.operation_status(binding, cursor, operation, |value| Ok(value.clone()))
            .await
    }

    async fn cancel(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        operation: &OperationHandle,
    ) -> Result<CancellationResult, WorkspaceError> {
        self.require_capability(WorkspaceCapability::ToolCancel)?;
        self.cancel_operation(binding, cursor, operation).await
    }

    async fn release(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        prepared: &PreparedToolCall,
    ) -> Result<ReleaseResult, WorkspaceError> {
        self.require_capability(WorkspaceCapability::ToolRelease)?;
        self.release_operation(binding, cursor, &prepared.operation)
            .await
    }
}

#[async_trait]
impl WorkspaceScmReadService for RemoteWorkcellClient {
    async fn discover(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &ScmDiscoverRequest,
    ) -> Result<ScmDiscoverResult, WorkspaceError> {
        self.require_capability(WorkspaceCapability::ScmDiscover)?;
        let response: contract::ScmDiscoverResponse = self
            .call(
                contract::SCM_DISCOVER_METHOD,
                &contract::ScmDiscoverRequest {
                    version: contract::ContractVersion::V1,
                    binding: self.bind_workspace_request(binding, cursor)?,
                    path: contract_path(&request.path)?,
                },
                &self.0.cancellation.child_token(),
            )
            .await?;
        validate_v1(response.version)?;
        let repository = scm_repository(&response.repository)?;
        self.0
            .repositories
            .lock()
            .map_err(|_| WorkspaceError::Unavailable)?
            .insert(repository.handle.clone(), repository.root.clone());
        Ok(ScmDiscoverResult { repository })
    }

    async fn status(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &ScmStatusRequest,
    ) -> Result<ScmStatusPage, WorkspaceError> {
        self.require_capability(WorkspaceCapability::ScmStatus)?;
        let limits = &self
            .0
            .descriptor
            .capabilities
            .scm
            .as_ref()
            .ok_or_else(invalid_response)?
            .limits;
        require_nonzero_within(request.page_size, limits.max_status_entries)?;
        if request
            .continuation
            .as_ref()
            .is_some_and(|cursor| cursor.as_str().len() > limits.max_cursor_bytes as usize)
        {
            return Err(WorkspaceError::StaleCursor);
        }
        let response: contract::ScmStatusResponse = self
            .call(
                contract::SCM_STATUS_METHOD,
                &contract::ScmStatusRequest {
                    version: contract::ContractVersion::V1,
                    binding: self.bind_workspace_request(binding, cursor)?,
                    repository_handle: contract_resource_id(&request.repository_handle)?,
                    page_size: request.page_size,
                    cursor: request
                        .continuation
                        .as_ref()
                        .map(continuation_contract)
                        .transpose()?,
                },
                &self.0.cancellation.child_token(),
            )
            .await?;
        validate_v1(response.version)?;
        if response.entries.len() > request.page_size as usize
            || response.entries.len() > limits.max_status_paths as usize
            || response
                .next_cursor
                .as_ref()
                .is_some_and(|cursor| cursor.as_str().len() > limits.max_cursor_bytes as usize)
        {
            return Err(invalid_response());
        }
        Ok(ScmStatusPage {
            revisions: scm_revisions(&response.revisions)?,
            collection_revision: collection_revision(&response.revision)?,
            entries: response
                .entries
                .iter()
                .map(scm_status_entry)
                .collect::<Result<_, _>>()?,
            truncated: response.next_cursor.is_some(),
            incomplete: false,
            continuation: response
                .next_cursor
                .as_ref()
                .map(continuation)
                .transpose()?,
        })
    }

    async fn log(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &ScmLogRequest,
    ) -> Result<ScmLogPage, WorkspaceError> {
        self.require_capability(WorkspaceCapability::ScmLog)?;
        let limits = &self
            .0
            .descriptor
            .capabilities
            .scm
            .as_ref()
            .ok_or_else(invalid_response)?
            .limits;
        require_nonzero_within(request.page_size, limits.max_log_entries)?;
        if request
            .continuation
            .as_ref()
            .is_some_and(|cursor| cursor.as_str().len() > limits.max_cursor_bytes as usize)
        {
            return Err(WorkspaceError::StaleCursor);
        }
        let response: contract::ScmLogResponse = self
            .call(
                contract::SCM_LOG_METHOD,
                &contract::ScmLogRequest {
                    version: contract::ContractVersion::V1,
                    binding: self.bind_workspace_request(binding, cursor)?,
                    repository_handle: contract_resource_id(&request.repository_handle)?,
                    page_size: request.page_size,
                    cursor: request
                        .continuation
                        .as_ref()
                        .map(continuation_contract)
                        .transpose()?,
                },
                &self.0.cancellation.child_token(),
            )
            .await?;
        validate_v1(response.version)?;
        if response.commits.len() > request.page_size as usize
            || response
                .next_cursor
                .as_ref()
                .is_some_and(|cursor| cursor.as_str().len() > limits.max_cursor_bytes as usize)
        {
            return Err(invalid_response());
        }
        let (truncated, incomplete) =
            pagination_flags(response.truncated, response.next_cursor.is_some());
        Ok(ScmLogPage {
            head_revision: scm_revision(&response.head_revision)?,
            collection_revision: collection_revision(&response.revision)?,
            commits: response
                .commits
                .iter()
                .map(scm_commit)
                .collect::<Result<_, _>>()?,
            truncated,
            incomplete,
            continuation: response
                .next_cursor
                .as_ref()
                .map(continuation)
                .transpose()?,
        })
    }

    async fn diff(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &ScmDiffRequest,
    ) -> Result<ScmDiffPage, WorkspaceError> {
        self.require_capability(WorkspaceCapability::ScmDiff)?;
        let limits = &self
            .0
            .descriptor
            .capabilities
            .scm
            .as_ref()
            .ok_or_else(invalid_response)?
            .limits;
        require_nonzero_within(request.max_lines, limits.max_diff_lines)?;
        require_nonzero_within(request.max_bytes, limits.max_diff_bytes)?;
        if request
            .continuation
            .as_ref()
            .is_some_and(|cursor| cursor.as_str().len() > limits.max_cursor_bytes as usize)
        {
            return Err(WorkspaceError::StaleCursor);
        }
        let target = scm_diff_target_contract(&request.target)?;
        let response: contract::ScmDiffResponse = self
            .call(
                contract::SCM_DIFF_METHOD,
                &contract::ScmDiffRequest {
                    version: contract::ContractVersion::V1,
                    binding: self.bind_workspace_request(binding, cursor)?,
                    repository_handle: contract_resource_id(&request.repository_handle)?,
                    target: target.clone(),
                    path: request.path.as_ref().map(contract_path).transpose()?,
                    max_lines: request.max_lines,
                    max_bytes: request.max_bytes,
                    cursor: request
                        .continuation
                        .as_ref()
                        .map(continuation_contract)
                        .transpose()?,
                },
                &self.0.cancellation.child_token(),
            )
            .await?;
        validate_v1(response.version)?;
        let response_bytes = serialized_items_bytes(&response.lines)?;
        let response_files = response
            .lines
            .iter()
            .map(|line| line.path.as_str())
            .collect::<HashSet<_>>()
            .len();
        if response.lines.len() > request.max_lines as usize
            || response_bytes > request.max_bytes as usize
            || response_files > limits.max_diff_files as usize
            || response
                .next_cursor
                .as_ref()
                .is_some_and(|cursor| cursor.as_str().len() > limits.max_cursor_bytes as usize)
        {
            return Err(invalid_response());
        }
        let (truncated, incomplete) =
            pagination_flags(response.truncated, response.next_cursor.is_some());
        Ok(ScmDiffPage {
            repository_revision: scm_revision(&response.repository_revision)?,
            collection_revision: collection_revision(&response.revision)?,
            lines: response
                .lines
                .iter()
                .map(scm_diff_line)
                .collect::<Result<_, _>>()?,
            truncated,
            incomplete,
            continuation: response
                .next_cursor
                .as_ref()
                .map(continuation)
                .transpose()?,
        })
    }

    async fn read_side(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &ScmReadSideRequest,
    ) -> Result<ScmReadSidePage, WorkspaceError> {
        self.require_capability(WorkspaceCapability::ScmReadSide)?;
        let limits = &self
            .0
            .descriptor
            .capabilities
            .scm
            .as_ref()
            .ok_or_else(invalid_response)?
            .limits;
        require_nonzero_within(request.max_lines, limits.max_side_lines)?;
        require_nonzero_within(request.max_bytes, limits.max_side_bytes)?;
        if request.start_line == 0 {
            return Err(invalid_response());
        }
        let side = scm_side_contract(&request.side)?;
        let response: contract::ScmReadSideResponse = self
            .call(
                contract::SCM_READ_SIDE_METHOD,
                &contract::ScmReadSideRequest {
                    version: contract::ContractVersion::V1,
                    binding: self.bind_workspace_request(binding, cursor)?,
                    repository_handle: contract_resource_id(&request.repository_handle)?,
                    path: contract_path(&request.path)?,
                    side: side.clone(),
                    start_line: request.start_line,
                    max_lines: request.max_lines,
                    max_bytes: request.max_bytes,
                },
                &self.0.cancellation.child_token(),
            )
            .await?;
        validate_v1(response.version)?;
        if workspace_path(&response.path)? != request.path
            || response.side != side
            || response.start_line != request.start_line
            || response.end_line < response.start_line
            || response.end_line > response.total_lines
            || response.truncated != response.next_start_line.is_some()
            || response.content.as_str().len() > request.max_bytes as usize
            || response.end_line - response.start_line + 1 > request.max_lines
            || response
                .next_start_line
                .is_some_and(|next| response.end_line.checked_add(1) != Some(next))
        {
            return Err(invalid_response());
        }
        let path = workspace_path(&response.path)?;
        let repository_root = self
            .0
            .repositories
            .lock()
            .map_err(|_| WorkspaceError::Unavailable)?
            .get(&request.repository_handle)
            .cloned()
            .ok_or_else(|| WorkspaceError::StaleResource {
                resource_id: request.repository_handle.clone(),
            })?;
        let cached_path = join_workspace_path(&repository_root, &path)?;
        let resource_id = self.remember_resource_id(&response.resource_id, &cached_path)?;
        Ok(ScmReadSidePage {
            repository_revision: scm_revision(&response.repository_revision)?,
            resource_id,
            revision: resource_revision(&response.revision)?,
            path,
            side: request.side.clone(),
            content: response.content.as_str().to_owned(),
            start_line: response.start_line,
            end_line: response.end_line,
            total_lines: response.total_lines,
            truncated: response.truncated,
            incomplete: false,
            next_start_line: response.next_start_line,
        })
    }
}

#[async_trait]
impl WorkspaceScmMutationService for RemoteWorkcellClient {
    async fn prepare(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        repository_handle: &ResourceId,
        mutation: &ScmMutation,
    ) -> Result<PreparedScmMutation, WorkspaceError> {
        let capability = match mutation {
            ScmMutation::Stage { .. } => WorkspaceCapability::ScmStage,
            ScmMutation::Unstage { .. } => WorkspaceCapability::ScmUnstage,
            ScmMutation::Discard { .. } => WorkspaceCapability::ScmDiscard,
        };
        self.require_capability(capability)?;
        let path_count = match mutation {
            ScmMutation::Stage { paths }
            | ScmMutation::Unstage { paths }
            | ScmMutation::Discard { paths } => paths.len(),
        };
        let path_limit = self
            .0
            .descriptor
            .capabilities
            .scm
            .as_ref()
            .ok_or_else(invalid_response)?
            .limits
            .max_paths as usize;
        if path_count == 0 || path_count > path_limit {
            return Err(invalid_response());
        }
        let wire_mutation = scm_mutation_contract(mutation)?;
        let _permit = self.reserve_preparation().await?;
        let response: contract::ScmPrepareMutationResponse = self
            .call(
                contract::SCM_PREPARE_MUTATION_METHOD,
                &contract::ScmPrepareMutationRequest {
                    version: contract::ContractVersion::V1,
                    binding: self.bind_workspace_request(binding, cursor)?,
                    repository_handle: contract_resource_id(repository_handle)?,
                    mutation: wire_mutation.clone(),
                },
                &self.0.cancellation.child_token(),
            )
            .await?;
        validate_v1(response.version)?;
        if response.preview.mutation != wire_mutation {
            return Err(invalid_response());
        }
        validate_fixed_contract(
            &response.operation.binding.contract,
            contract::SCM_MUTATION_CONTRACT_ID,
        )?;
        Ok(PreparedScmMutation {
            operation: self.prepared_handle(
                &response.operation,
                "scm",
                Some(SCM_MUTATION_KIND.to_owned()),
                Some(PreparedWorkspaceContext {
                    binding: binding.clone(),
                    cursor: cursor.clone(),
                }),
            )?,
            preview: ScmMutationPreview {
                mutation: mutation.clone(),
                repository_identity: scm_revision(&response.preview.repository_identity)?,
                revisions: scm_revisions(&response.preview.revisions)?,
                entries: response
                    .preview
                    .entries
                    .iter()
                    .map(scm_status_entry)
                    .collect::<Result<_, _>>()?,
            },
        })
    }

    async fn execute(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        prepared: &PreparedScmMutation,
    ) -> Result<OperationStatus<ScmMutationResult>, WorkspaceError> {
        let expected = prepared.preview.mutation.clone();
        self.execute_operation(binding, cursor, &prepared.operation, move |value| {
            let result: contract::ScmMutationResponse =
                serde_json::from_value(value.clone()).map_err(|_| invalid_response())?;
            validate_v1(result.version)?;
            let mutation = scm_mutation(&result.mutation)?;
            if mutation != expected {
                return Err(invalid_response());
            }
            Ok(ScmMutationResult {
                mutation,
                revisions: scm_revisions(&result.revisions)?,
            })
        })
        .await
    }

    async fn status(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        operation: &OperationHandle,
    ) -> Result<OperationStatus<ScmMutationResult>, WorkspaceError> {
        self.require_capability(WorkspaceCapability::ScmMutationStatus)?;
        self.operation_status(binding, cursor, operation, parse_scm_mutation_result)
            .await
    }

    async fn cancel(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        operation: &OperationHandle,
    ) -> Result<CancellationResult, WorkspaceError> {
        self.require_capability(WorkspaceCapability::ScmMutationCancel)?;
        self.cancel_operation(binding, cursor, operation).await
    }

    async fn release(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        prepared: &PreparedScmMutation,
    ) -> Result<ReleaseResult, WorkspaceError> {
        self.require_capability(WorkspaceCapability::ScmMutationRelease)?;
        self.release_operation(binding, cursor, &prepared.operation)
            .await
    }
}

#[async_trait]
impl WorkspaceSnapshotReadService for RemoteWorkcellClient {
    async fn capture(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &SnapshotCaptureRequest,
    ) -> Result<SnapshotCaptureResult, WorkspaceError> {
        self.require_capability(WorkspaceCapability::SnapshotCapture)?;
        if request.label.is_some() {
            self.require_capability(WorkspaceCapability::SnapshotCaptureLabels)?;
        }
        self.capture_snapshot(binding, cursor, request, |delay| async move {
            smol::Timer::after(delay).await;
        })
        .await
    }

    async fn inspect(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &SnapshotInspectRequest,
    ) -> Result<SnapshotInspectPage, WorkspaceError> {
        self.require_capability(WorkspaceCapability::SnapshotInspect)?;
        let limits = self.snapshot_limits()?;
        require_nonzero_within(request.page_size, limits.max_files)?;
        let response: contract::SnapshotInspectResponse = self
            .call(
                contract::SNAPSHOT_INSPECT_METHOD,
                &contract::SnapshotInspectRequest {
                    version: contract::ContractVersion::V1,
                    binding: self.bind_workspace_request(binding, cursor)?,
                    snapshot_id: contract_identifier(&request.snapshot_id)?,
                    page_size: request.page_size,
                    cursor: request
                        .continuation
                        .as_ref()
                        .map(continuation_contract)
                        .transpose()?,
                },
                &self.0.cancellation.child_token(),
            )
            .await?;
        validate_v1(response.version)?;
        let snapshot = snapshot_summary(&response.snapshot)?;
        validate_snapshot_summary(&snapshot, limits)?;
        if snapshot.snapshot_id != request.snapshot_id
            || response.files.len() > request.page_size as usize
            || response
                .files
                .iter()
                .any(|file| file.size_bytes > limits.max_file_bytes)
        {
            return Err(invalid_response());
        }
        Ok(SnapshotInspectPage {
            snapshot,
            files: response
                .files
                .iter()
                .map(snapshot_file)
                .collect::<Result<_, _>>()?,
            exclusions: response
                .exclusions
                .iter()
                .map(workspace_path)
                .collect::<Result<_, _>>()?,
            truncated: response.next_cursor.is_some(),
            incomplete: false,
            continuation: response
                .next_cursor
                .as_ref()
                .map(continuation)
                .transpose()?,
        })
    }

    async fn restore_status(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        restore_id: &RestoreId,
    ) -> Result<SnapshotRestoreStatus, WorkspaceError> {
        self.require_capability(WorkspaceCapability::SnapshotStatus)?;
        let response: contract::SnapshotStatusResponse = self
            .call(
                contract::SNAPSHOT_STATUS_METHOD,
                &contract::SnapshotStatusRequest {
                    version: contract::ContractVersion::V1,
                    binding: self.bind_workspace_request(binding, cursor)?,
                    restore_id: contract_identifier(restore_id)?,
                },
                &self.0.cancellation.child_token(),
            )
            .await?;
        validate_v1(response.version)?;
        let status = snapshot_restore_status(&response.restore)?;
        if &status.restore_id != restore_id {
            return Err(WorkspaceError::IdentityMismatch);
        }
        Ok(status)
    }
}

#[async_trait]
impl WorkspaceSnapshotMutationService for RemoteWorkcellClient {
    fn max_cleanup_checkpoints(&self) -> usize {
        self.snapshot_limits()
            .map_or(0, |limits| limits.max_cleanup_checkpoints as usize)
    }

    async fn prepare_restore(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        target: &SnapshotId,
        source: &SnapshotId,
    ) -> Result<PreparedSnapshotOperation, WorkspaceError> {
        self.require_capability(WorkspaceCapability::SnapshotPrepareRestore)?;
        let _permit = self.reserve_preparation().await?;
        let response: contract::SnapshotPrepareRestoreResponse = self
            .call(
                contract::SNAPSHOT_PREPARE_RESTORE_METHOD,
                &contract::SnapshotPrepareRestoreRequest {
                    version: contract::ContractVersion::V1,
                    binding: self.bind_workspace_request(binding, cursor)?,
                    snapshot_id: contract_identifier(target)?,
                    source_snapshot_id: contract_identifier(source)?,
                },
                &self.0.cancellation.child_token(),
            )
            .await?;
        validate_v1(response.version)?;
        let preview = snapshot_restore_preview(&response.preview)?;
        if &preview.target_snapshot_id != target || &preview.source_snapshot_id != source {
            return Err(WorkspaceError::IdentityMismatch);
        }
        validate_fixed_contract(
            &response.operation.binding.contract,
            contract::SNAPSHOT_RESTORE_CONTRACT_ID,
        )?;
        Ok(PreparedSnapshotOperation {
            operation: self.prepared_handle(
                &response.operation,
                "snapshot",
                Some(SNAPSHOT_RESTORE_KIND.to_owned()),
                Some(PreparedWorkspaceContext {
                    binding: binding.clone(),
                    cursor: cursor.clone(),
                }),
            )?,
            preview: SnapshotOperationPreview::Restore(preview),
        })
    }

    async fn prepare_unrevert(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        restore_id: &RestoreId,
    ) -> Result<PreparedSnapshotOperation, WorkspaceError> {
        self.require_capability(WorkspaceCapability::SnapshotPrepareUnrevert)?;
        let _permit = self.reserve_preparation().await?;
        let response: contract::SnapshotPrepareRestoreResponse = self
            .call(
                contract::SNAPSHOT_PREPARE_UNREVERT_METHOD,
                &contract::SnapshotPrepareUnrevertRequest {
                    version: contract::ContractVersion::V1,
                    binding: self.bind_workspace_request(binding, cursor)?,
                    restore_id: contract_identifier(restore_id)?,
                },
                &self.0.cancellation.child_token(),
            )
            .await?;
        validate_v1(response.version)?;
        validate_fixed_contract(
            &response.operation.binding.contract,
            contract::SNAPSHOT_UNREVERT_CONTRACT_ID,
        )?;
        Ok(PreparedSnapshotOperation {
            operation: self.prepared_handle(
                &response.operation,
                "unrevert",
                Some(SNAPSHOT_UNREVERT_KIND.to_owned()),
                Some(PreparedWorkspaceContext {
                    binding: binding.clone(),
                    cursor: cursor.clone(),
                }),
            )?,
            preview: SnapshotOperationPreview::Unrevert(SnapshotUnrevertPreview {
                source_restore_id: restore_id.clone(),
                restore: snapshot_restore_preview(&response.preview)?,
            }),
        })
    }

    async fn prepare_cleanup(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        checkpoint_ids: &[CheckpointId],
    ) -> Result<PreparedSnapshotOperation, WorkspaceError> {
        self.require_capability(WorkspaceCapability::SnapshotPrepareCleanup)?;
        if checkpoint_ids.is_empty() || checkpoint_ids.len() > self.max_cleanup_checkpoints() {
            return Err(invalid_response());
        }
        let _permit = self.reserve_preparation().await?;
        let response: contract::SnapshotPrepareCleanupResponse = self
            .call(
                contract::SNAPSHOT_PREPARE_CLEANUP_METHOD,
                &contract::SnapshotPrepareCleanupRequest {
                    version: contract::ContractVersion::V1,
                    binding: self.bind_workspace_request(binding, cursor)?,
                    checkpoint_ids: checkpoint_ids
                        .iter()
                        .map(contract_identifier)
                        .collect::<Result<_, _>>()?,
                },
                &self.0.cancellation.child_token(),
            )
            .await?;
        validate_v1(response.version)?;
        let preview = snapshot_cleanup_preview(&response.preview)?;
        if !cleanup_preview_partitions(checkpoint_ids, &preview) {
            return Err(WorkspaceError::IdentityMismatch);
        }
        validate_fixed_contract(
            &response.operation.binding.contract,
            contract::SNAPSHOT_CLEANUP_CONTRACT_ID,
        )?;
        Ok(PreparedSnapshotOperation {
            operation: self.prepared_handle(
                &response.operation,
                "cleanup",
                Some(SNAPSHOT_CLEANUP_KIND.to_owned()),
                Some(PreparedWorkspaceContext {
                    binding: binding.clone(),
                    cursor: cursor.clone(),
                }),
            )?,
            preview: SnapshotOperationPreview::Cleanup(preview),
        })
    }

    async fn execute(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        prepared: &PreparedSnapshotOperation,
    ) -> Result<OperationStatus<SnapshotOperationResult>, WorkspaceError> {
        self.require_capability(WorkspaceCapability::SnapshotExecute)?;
        let preview = prepared.preview.clone();
        self.execute_operation(binding, cursor, &prepared.operation, move |value| {
            parse_snapshot_result(value, &preview)
        })
        .await
    }

    async fn operation_status(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        operation: &OperationHandle,
    ) -> Result<OperationStatus<SnapshotOperationResult>, WorkspaceError> {
        self.require_capability(WorkspaceCapability::SnapshotOperationStatus)?;
        self.operation_status(binding, cursor, operation, parse_snapshot_result_unbound)
            .await
    }

    async fn cancel(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        operation: &OperationHandle,
    ) -> Result<CancellationResult, WorkspaceError> {
        self.require_capability(WorkspaceCapability::SnapshotCancel)?;
        self.cancel_operation(binding, cursor, operation).await
    }

    async fn acknowledge(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        restore_id: &RestoreId,
    ) -> Result<SnapshotRestoreStatus, WorkspaceError> {
        self.require_capability(WorkspaceCapability::SnapshotAcknowledge)?;
        let response: contract::SnapshotAcknowledgeResponse = self
            .call(
                contract::SNAPSHOT_ACKNOWLEDGE_METHOD,
                &contract::SnapshotAcknowledgeRequest {
                    version: contract::ContractVersion::V1,
                    binding: self.bind_workspace_request(binding, cursor)?,
                    restore_id: contract_identifier(restore_id)?,
                },
                &self.0.cancellation.child_token(),
            )
            .await?;
        validate_v1(response.version)?;
        let status = snapshot_restore_status(&response.restore)?;
        if &status.restore_id != restore_id || status.state != SnapshotRestoreState::Acknowledged {
            return Err(invalid_response());
        }
        Ok(status)
    }

    async fn release(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        prepared: &PreparedSnapshotOperation,
    ) -> Result<ReleaseResult, WorkspaceError> {
        self.require_capability(WorkspaceCapability::SnapshotRelease)?;
        self.release_operation(binding, cursor, &prepared.operation)
            .await
    }
}

fn request_metadata() -> Value {
    json!({
        "io.modelcontextprotocol/clientInfo": {
            "name": "caudra",
            "version": env!("CARGO_PKG_VERSION")
        },
        "io.modelcontextprotocol/protocolVersion": PROTOCOL_VERSION,
        "io.modelcontextprotocol/clientCapabilities": {
            "extensions": {"ai.workcell/remote-host": {"versions": ["v1"]}}
        },
        "ai.workcell/remote-host": {"versions": ["v1"]}
    })
}

async fn read_bounded(
    body: &mut isahc::AsyncBody,
    limit: usize,
) -> Result<Vec<u8>, RemoteWorkcellError> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let read = body
            .read(&mut chunk)
            .await
            .map_err(|_| RemoteWorkcellError::Transport)?;
        if read == 0 {
            return Ok(bytes);
        }
        if bytes.len().saturating_add(read) > limit {
            return Err(RemoteWorkcellError::InvalidProtocol);
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
}

fn numeric_loopback(url: &Url) -> bool {
    matches!(url.host(), Some(Host::Ipv4(address)) if address.is_loopback())
        || matches!(url.host(), Some(Host::Ipv6(address)) if address.is_loopback())
}

fn validate_credential_selection(
    selection: &RemoteWorkcellSelection,
    credential: Option<&NamedBearerCredential>,
) -> Result<(), RemoteWorkcellError> {
    // Matches `RemoteTransport::new`: plaintext is confined to a numeric
    // loopback literal, and a bearer is permitted there because the request
    // never leaves the host.
    if selection.endpoint.as_url().scheme() == "http"
        && !numeric_loopback(selection.endpoint.as_url())
    {
        return Err(RemoteWorkcellError::InsecureTransport);
    }
    match (&selection.credential_ref, credential) {
        (None, None) => Ok(()),
        (Some(reference), Some(credential)) if reference.name() == credential.name() => Ok(()),
        // A process-scoped bearer names no stored credential, so there is no
        // reference for it to agree with. Selection already refuses to omit the
        // reference for anything but a numeric loopback literal, and that is
        // re-established here rather than assumed.
        (None, Some(_)) if numeric_loopback(selection.endpoint.as_url()) => Ok(()),
        _ => Err(RemoteWorkcellError::Authentication),
    }
}

fn source_trust_anchor(
    selection: &RemoteWorkcellSelection,
) -> Result<SourceTrustAnchor, RemoteWorkcellError> {
    let mut origin = selection.endpoint.as_url().clone();
    origin.set_path("/");
    origin.set_query(None);
    origin.set_fragment(None);
    SourceTrustAnchor::new(origin.origin().ascii_serialization())
        .map_err(|_| RemoteWorkcellError::IdentityMismatch)
}

fn same_origin(endpoint: &Url, effective: Option<&isahc::http::Uri>) -> bool {
    let Some(effective) = effective.and_then(|uri| Url::parse(&uri.to_string()).ok()) else {
        return false;
    };
    same_url_origin(endpoint, &effective)
}

fn same_url_origin(left: &Url, right: &Url) -> bool {
    left.scheme() == right.scheme()
        && left.host_str().map(str::to_ascii_lowercase)
            == right.host_str().map(str::to_ascii_lowercase)
        && left.port_or_known_default() == right.port_or_known_default()
}

fn sse_event_end(buffer: &[u8]) -> Option<usize> {
    let lf = buffer
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|position| (position, position + 2));
    let crlf = buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| (position, position + 4));
    match (lf, crlf) {
        (Some(left), Some(right)) => Some(if left.0 <= right.0 { left.1 } else { right.1 }),
        (Some((_, end)), None) | (None, Some((_, end))) => Some(end),
        (None, None) => None,
    }
}

fn parse_content_range(value: &str) -> Result<(u64, u64, Option<u64>), RemoteWorkcellError> {
    let range = value
        .strip_prefix("bytes ")
        .ok_or(RemoteWorkcellError::InvalidProtocol)?;
    let (bounds, total) = range
        .split_once('/')
        .ok_or(RemoteWorkcellError::InvalidProtocol)?;
    let (start, end) = bounds
        .split_once('-')
        .ok_or(RemoteWorkcellError::InvalidProtocol)?;
    let start = start
        .parse::<u64>()
        .map_err(|_| RemoteWorkcellError::InvalidProtocol)?;
    let end = end
        .parse::<u64>()
        .map_err(|_| RemoteWorkcellError::InvalidProtocol)?;
    let end_exclusive = end
        .checked_add(1)
        .ok_or(RemoteWorkcellError::InvalidProtocol)?;
    if end_exclusive <= start {
        return Err(RemoteWorkcellError::InvalidProtocol);
    }
    let total = if total == "*" {
        None
    } else {
        Some(
            total
                .parse::<u64>()
                .map_err(|_| RemoteWorkcellError::InvalidProtocol)?,
        )
    };
    if total.is_some_and(|total| end_exclusive > total) {
        return Err(RemoteWorkcellError::InvalidProtocol);
    }
    Ok((start, end_exclusive, total))
}

fn map_rpc_error(error: &JsonRpcError) -> RemoteWorkcellError {
    let symbolic = rpc_diagnostic_token(error.data.as_ref().and_then(|data| data.get("code")));
    match symbolic {
        Some("transferIntegrityFailure") => RemoteWorkcellError::TransferIntegrity,
        Some("transferLimitExceeded") => RemoteWorkcellError::TransferQuota,
        Some(
            "transferConflict"
            | "transferMissing"
            | "transferInvalidState"
            | "transferPublicationReserved",
        ) => RemoteWorkcellError::Conflict,
        Some("transferBindingMismatch") => RemoteWorkcellError::BindingMismatch,
        Some("transferCancelled") => RemoteWorkcellError::Cancelled,
        Some("transferIndeterminate") => RemoteWorkcellError::Indeterminate,
        Some("transferStorageUnavailable") => RemoteWorkcellError::Transport,
        Some("authentication") | Some("permission_denied") => RemoteWorkcellError::Authentication,
        Some(STALE_RESOURCE_CODE) | Some("stale_repository") | Some("stale_prepared_operation") => {
            RemoteWorkcellError::StaleResource
        }
        Some("stale_cwd") | Some("invalid_cursor") | Some("stale_cursor") => {
            RemoteWorkcellError::StaleCursor
        }
        Some("binding_mismatch") | Some("instance_mismatch") => {
            RemoteWorkcellError::BindingMismatch
        }
        Some("conflict")
        | Some("invocation_mismatch")
        | Some("running")
        | Some("workspace_changed")
        | Some("repository_locked")
        | Some("acknowledgement_required") => RemoteWorkcellError::Conflict,
        Some("busy") => RemoteWorkcellError::Busy,
        Some("not_repository") => RemoteWorkcellError::NotRepository,
        Some("watch_unavailable") => RemoteWorkcellError::WatchUnavailable,
        Some("quota_exceeded") => {
            let (limit, maximum) = exceeded_limit(error.data.as_ref());
            RemoteWorkcellError::QuotaExceeded { limit, maximum }
        }
        Some("limit_exceeded") | Some("resource_limit") => {
            let (limit, maximum) = exceeded_limit(error.data.as_ref());
            RemoteWorkcellError::LimitExceeded { limit, maximum }
        }
        Some("unsupported_file") => RemoteWorkcellError::UnsupportedEntry,
        Some("policy_denied") => RemoteWorkcellError::PolicyDenied,
        Some("indeterminate") => RemoteWorkcellError::Indeterminate,
        Some("cancelled") => RemoteWorkcellError::Cancelled,
        Some("timed_out") => RemoteWorkcellError::Timeout,
        Some(snapshot::NOT_FOUND) => RemoteWorkcellError::UnmappedRefusal {
            code: error.code,
            symbolic: snapshot::NOT_FOUND.to_owned(),
        },
        _ if matches!(error.code, 401 | 403) => RemoteWorkcellError::Authentication,
        _ => RemoteWorkcellError::UnmappedRefusal {
            code: error.code,
            symbolic: symbolic.unwrap_or(NO_SYMBOLIC_REASON).to_owned(),
        },
    }
}

fn rpc_diagnostic_token(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str).filter(|token| {
        (1..=MAX_RPC_DIAGNOSTIC_BYTES).contains(&token.len())
            && token
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    })
}

fn exceeded_limit(data: Option<&Value>) -> (Option<String>, Option<u64>) {
    let limit = data
        .and_then(|data| data.get("limit"))
        .and_then(Value::as_str)
        .filter(|limit| {
            (1..=MAX_LIMIT_NAME_BYTES).contains(&limit.len())
                && limit.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
        .map(str::to_owned);
    let maximum = data
        .and_then(|data| data.get("maximum"))
        .and_then(Value::as_u64);
    (limit, maximum)
}

fn parse_descriptor(
    discover: &Value,
) -> Result<contract::RemoteHostDescriptor, RemoteWorkcellError> {
    let object = discover
        .as_object()
        .ok_or(RemoteWorkcellError::InvalidProtocol)?;
    if object.get("resultType").and_then(Value::as_str) != Some("complete")
        || object.get("ttlMs").and_then(Value::as_u64) != Some(0)
        || object.get("cacheScope").and_then(Value::as_str) != Some("private")
    {
        return Err(RemoteWorkcellError::InvalidProtocol);
    }
    let supported = object
        .get("supportedVersions")
        .and_then(Value::as_array)
        .ok_or(RemoteWorkcellError::InvalidProtocol)?;
    if !supported.iter().any(|version| version == PROTOCOL_VERSION) {
        return Err(RemoteWorkcellError::CapabilityMismatch);
    }
    let descriptor = object
        .get("capabilities")
        .and_then(|capabilities| capabilities.get("extensions"))
        .and_then(|extensions| extensions.get(contract::EXTENSION_ID))
        .cloned()
        .ok_or(RemoteWorkcellError::CapabilityMismatch)?;
    serde_json::from_value(descriptor).map_err(|_| RemoteWorkcellError::InvalidProtocol)
}

fn validate_descriptor(
    descriptor: &contract::RemoteHostDescriptor,
    selection: &RemoteWorkcellSelection,
) -> Result<(), RemoteWorkcellError> {
    if descriptor.version != contract::ContractVersion::V1
        || descriptor.resource_namespace_version.as_str() != RESOURCE_NAMESPACE_VERSION
        || descriptor.path_style.as_str() != PATH_STYLE
        || descriptor.cwd.display_path.as_str() != selection.cwd.as_str()
    {
        return Err(RemoteWorkcellError::IdentityMismatch);
    }
    if selection
        .expected_server_id
        .as_ref()
        .is_some_and(|expected| expected.as_str() != descriptor.server_id.as_str())
        || selection
            .expected_workspace_id
            .as_ref()
            .is_some_and(|expected| expected.as_str() != descriptor.workspace_id.as_str())
    {
        return Err(RemoteWorkcellError::IdentityMismatch);
    }
    require_full_remote_parity(&descriptor.capabilities)
}

fn same_descriptor_except_instance(
    left: &contract::RemoteHostDescriptor,
    right: &contract::RemoteHostDescriptor,
) -> bool {
    left.version == right.version
        && left.server_id == right.server_id
        && left.workspace_id == right.workspace_id
        && left.workspace_generation == right.workspace_generation
        && left.root_project_id == right.root_project_id
        && left.principal_id == right.principal_id
        && left.resource_namespace_version == right.resource_namespace_version
        && left.path_style == right.path_style
        && left.revisions == right.revisions
        && left.cwd == right.cwd
        && left.capabilities == right.capabilities
}

fn validate_capabilities(
    capabilities: &contract::RemoteHostCapabilities,
) -> Result<(), RemoteWorkcellError> {
    if !capabilities.control_plane
        || !capabilities.control_plane_missing.is_empty()
        || capabilities.tool_catalog.version != contract::ContractVersion::V1
        || capabilities.tool_execution.version != contract::ContractVersion::V1
        || capabilities.tool_catalog.limits.max_request_bytes == 0
        || capabilities.tool_execution.limits.max_request_bytes == 0
        || capabilities.tool_catalog.limits.max_request_bytes > MAX_HTTP_RESPONSE_BYTES as u64
        || capabilities.tool_execution.limits.max_request_bytes > MAX_HTTP_RESPONSE_BYTES as u64
    {
        return Err(RemoteWorkcellError::CapabilityMismatch);
    }
    if let Some(operations) = &capabilities.operations {
        let limits = &operations.limits;
        if operations.version != contract::ContractVersion::V1
            || !operations.exact_preparation
            || limits.preparation_ttl_ms == 0
            || limits.max_preparations == 0
            || limits.max_operations == 0
            || limits.max_ledger_bytes == 0
            || limits.max_argument_bytes == 0
            || limits.max_argument_bytes > contract::MAX_ARGUMENT_BYTES as u64
            || limits.max_resource_intents == 0
            || limits.max_resource_intents > contract::MAX_RESOURCE_INTENTS as u32
            || limits.max_progress_events == 0
            || limits.max_progress_events > contract::MAX_PROGRESS_EVENTS as u32
            || limits.max_progress_bytes == 0
            || limits.max_progress_bytes > capabilities.tool_execution.limits.max_request_bytes
        {
            return Err(RemoteWorkcellError::CapabilityMismatch);
        }
    }
    if let Some(workspace) = &capabilities.workspace {
        let limits = &workspace.limits;
        if workspace.version != contract::ContractVersion::V1
            || limits.max_path_bytes == 0
            || limits.max_path_bytes > contract::MAX_WORKSPACE_PATH_BYTES as u32
            || limits.max_page_size == 0
            || limits.max_page_size > contract::MAX_PAGE_SIZE
            || limits.max_text_read_bytes == 0
            || limits.max_text_read_bytes > contract::MAX_TEXT_READ_BYTES
            || limits.max_search_pattern_bytes == 0
            || limits.max_search_pattern_bytes > contract::MAX_SEARCH_PATTERN_BYTES as u32
            || limits.max_cursor_bytes == 0
            || limits.max_cursor_bytes > contract::MAX_CURSOR_BYTES as u32
            || limits.max_list_entries == 0
            || limits.max_list_entries > contract::MAX_WORKSPACE_LIST_ENTRIES
            || limits.max_list_retained_bytes == 0
            || limits.max_list_retained_bytes > contract::MAX_WORKSPACE_LIST_RETAINED_BYTES
        {
            return Err(RemoteWorkcellError::CapabilityMismatch);
        }
    }
    if capabilities.reviewed_transfer.is_some()
        && !transfer::compatible(&capabilities.reviewed_transfer, &capabilities.operations)
    {
        return Err(RemoteWorkcellError::CapabilityMismatch);
    }
    if let Some(watch) = &capabilities.watch {
        let limits = &watch.limits;
        if watch.version != contract::ContractVersion::V1
            || limits.max_subscriptions == 0
            || limits.max_subscriptions > contract::MAX_WATCH_SUBSCRIPTIONS as u32
            || limits.max_retained_events == 0
            || limits.max_retained_events > contract::MAX_WATCH_RETAINED_EVENTS as u32
            || limits.max_retained_bytes == 0
            || limits.max_retained_bytes > contract::MAX_WATCH_RETAINED_BYTES as u64
            || limits.max_lifetime_events == 0
            || limits.max_lifetime_events > contract::MAX_WATCH_LIFETIME_EVENTS as u64
            || limits.max_poll_events == 0
            || limits.max_poll_events > contract::MAX_WATCH_POLL_EVENTS
            || limits.max_poll_bytes == 0
            || limits.max_poll_bytes > contract::MAX_WATCH_POLL_BYTES
            || limits.max_wait_ms == 0
            || limits.max_wait_ms > contract::MAX_WATCH_WAIT_MS
            || limits.subscription_ttl_ms == 0
            || limits.subscription_ttl_ms > contract::WATCH_SUBSCRIPTION_TTL_MS
        {
            return Err(RemoteWorkcellError::CapabilityMismatch);
        }
    }
    if let Some(assets) = &capabilities.project_assets
        && (assets.version != contract::ContractVersion::V1
            || assets.manifest_version.as_str() != contract::PROJECT_ASSET_MANIFEST_VERSION
            || assets.limits.max_assets == 0
            || assets.limits.max_assets > contract::MAX_PROJECT_ASSETS as u32
            || assets.limits.max_read_bytes == 0
            || assets.limits.max_read_bytes > contract::MAX_PROJECT_ASSET_READ_BYTES)
    {
        return Err(RemoteWorkcellError::CapabilityMismatch);
    }
    if let Some(mutation) = &capabilities.workspace_mutation
        && (mutation.version != contract::ContractVersion::V1
            || !mutation.prepared
            || mutation.max_mutations == 0
            || mutation.max_mutations > contract::MAX_MUTATIONS as u32
            || mutation.max_content_bytes == 0
            || mutation.max_content_bytes > contract::MAX_MUTATION_CONTENT_BYTES as u64)
    {
        return Err(RemoteWorkcellError::CapabilityMismatch);
    }
    if let Some(exec) = &capabilities.direct_exec
        && (exec.version != contract::ContractVersion::V1
            || !exec.prepared
            || exec.interactive
            || exec.max_command_bytes == 0
            || exec.max_command_bytes > contract::MAX_COMMAND_BYTES as u32
            || exec.max_timeout_ms == 0)
    {
        return Err(RemoteWorkcellError::CapabilityMismatch);
    }
    if let Some(scm) = &capabilities.scm {
        let limits = &scm.limits;
        if scm.version != contract::ContractVersion::V1
            || (scm.methods.stage || scm.methods.unstage || scm.methods.discard)
                && !scm.prepared_mutations
            || limits.max_paths == 0
            || limits.max_paths > contract::MAX_SCM_PATHS as u32
            || limits.max_status_entries == 0
            || limits.max_status_entries > contract::MAX_SCM_STATUS_ENTRIES
            || limits.max_status_paths == 0
            || limits.max_status_paths > contract::MAX_SCM_STATUS_PATHS
            || limits.max_config_bytes == 0
            || limits.max_config_bytes > contract::MAX_SCM_CONFIG_BYTES
            || limits.max_log_entries == 0
            || limits.max_log_entries > contract::MAX_SCM_LOG_ENTRIES
            || limits.max_log_commits == 0
            || limits.max_log_commits > contract::MAX_SCM_LOG_COMMITS
            || limits.max_commit_bytes == 0
            || limits.max_commit_bytes > contract::MAX_SCM_COMMIT_BYTES
            || limits.max_log_scan_bytes == 0
            || limits.max_log_scan_bytes > contract::MAX_SCM_LOG_SCAN_BYTES
            || limits.max_diff_lines == 0
            || limits.max_diff_lines > contract::MAX_SCM_DIFF_LINES
            || limits.max_diff_bytes == 0
            || limits.max_diff_bytes > contract::MAX_SCM_DIFF_BYTES
            || limits.max_diff_files == 0
            || limits.max_diff_files > contract::MAX_SCM_DIFF_FILES
            || limits.max_diff_scan_bytes == 0
            || limits.max_diff_scan_bytes > contract::MAX_SCM_DIFF_SCAN_BYTES
            || limits.max_diff_parsed_lines == 0
            || limits.max_diff_parsed_lines > contract::MAX_SCM_DIFF_PARSED_LINES
            || limits.max_side_lines == 0
            || limits.max_side_lines > contract::MAX_SCM_SIDE_LINES
            || limits.max_side_bytes == 0
            || limits.max_side_bytes > contract::MAX_SCM_SIDE_BYTES
            || limits.max_cursor_bytes == 0
            || limits.max_cursor_bytes > contract::MAX_CURSOR_BYTES as u32
        {
            return Err(RemoteWorkcellError::CapabilityMismatch);
        }
    }
    if let Some(snapshots) = &capabilities.snapshots {
        let limits = &snapshots.limits;
        if snapshots.version != contract::ContractVersion::V1
            || limits.max_files == 0
            || limits.max_files > contract::MAX_SNAPSHOT_FILES as u32
            || limits.max_file_bytes == 0
            || limits.max_file_bytes > contract::MAX_SNAPSHOT_FILE_BYTES
            || limits.max_total_bytes == 0
            || limits.max_total_bytes > contract::MAX_SNAPSHOT_TOTAL_BYTES
            || limits.max_capture_entries == 0
            || limits.max_capture_entries > contract::MAX_SNAPSHOT_CAPTURE_ENTRIES as u32
            || limits.max_capture_path_bytes == 0
            || limits.max_capture_path_bytes > contract::MAX_SNAPSHOT_CAPTURE_PATH_BYTES
            || limits.max_snapshots == 0
            || limits.max_snapshots > contract::MAX_SNAPSHOT_COUNT as u32
            || limits.max_storage_bytes == 0
            || limits.max_storage_bytes > contract::MAX_SNAPSHOT_STORAGE_BYTES
            || limits.max_concurrent_captures == 0
            || limits.max_cleanup_checkpoints == 0
            || limits.max_cleanup_checkpoints > contract::MAX_SNAPSHOT_CLEANUP as u32
        {
            return Err(RemoteWorkcellError::CapabilityMismatch);
        }
    }
    Ok(())
}

fn require_full_remote_parity(
    capabilities: &contract::RemoteHostCapabilities,
) -> Result<(), RemoteWorkcellError> {
    validate_capabilities(capabilities)?;
    let Some(operations) = capabilities.operations.as_ref() else {
        return Err(RemoteWorkcellError::CapabilityMismatch);
    };
    let operation_methods = &operations.methods;
    let Some(workspace) = capabilities.workspace.as_ref() else {
        return Err(RemoteWorkcellError::CapabilityMismatch);
    };
    let Some(watch) = capabilities.watch.as_ref() else {
        return Err(RemoteWorkcellError::CapabilityMismatch);
    };
    let Some(assets) = capabilities.project_assets.as_ref() else {
        return Err(RemoteWorkcellError::CapabilityMismatch);
    };
    let Some(mutation) = capabilities.workspace_mutation.as_ref() else {
        return Err(RemoteWorkcellError::CapabilityMismatch);
    };
    let Some(exec) = capabilities.direct_exec.as_ref() else {
        return Err(RemoteWorkcellError::CapabilityMismatch);
    };
    let Some(scm) = capabilities.scm.as_ref() else {
        return Err(RemoteWorkcellError::CapabilityMismatch);
    };
    let Some(snapshots) = capabilities.snapshots.as_ref() else {
        return Err(RemoteWorkcellError::CapabilityMismatch);
    };
    let complete = capabilities.control_plane
        && capabilities.control_plane_missing.is_empty()
        && capabilities.execution_environment.is_some()
        && transfer::compatible(&capabilities.reviewed_transfer, &capabilities.operations)
        && capabilities
            .reviewed_transfer
            .as_ref()
            .is_some_and(|transfer| transfer.creates_directories && transfer.safe_inventory)
        && operations.exact_preparation
        && operation_methods.prepare
        && operation_methods.execute
        && operation_methods.release
        && operation_methods.status
        && operation_methods.cancel
        && operations.limits.max_ledger_bytes > 0
        && operations.limits.max_progress_events > 0
        && operations.limits.max_progress_bytes > 0
        && workspace.methods.resolve_directory
        && workspace.methods.stat
        && workspace.methods.list
        && workspace.methods.read_text
        && workspace.methods.search_text
        && mutation.prepared
        && mutation.rollback_on_failure
        && watch.methods.open
        && watch.methods.poll
        && watch.methods.close
        && watch.recursive
        && exec.prepared
        && assets.methods.discover
        && assets.methods.read
        && scm.methods.discover
        && scm.methods.status
        && scm.methods.log
        && scm.methods.diff
        && scm.methods.read_side
        && scm.methods.stage
        && scm.methods.unstage
        && scm.methods.discard
        && scm.prepared_mutations
        && snapshots.methods.prepare_capture
        && snapshots.methods.checkpoint
        && snapshots.methods.inspect
        && snapshots.methods.status
        && snapshots.methods.prepare_restore
        && snapshots.methods.prepare_unrevert
        && snapshots.methods.acknowledge
        && snapshots.methods.prepare_cleanup;
    complete
        .then_some(())
        .ok_or(RemoteWorkcellError::CapabilityMismatch)
}

fn host_binding(descriptor: &contract::RemoteHostDescriptor) -> contract::HostBinding {
    contract::HostBinding {
        server_id: descriptor.server_id.clone(),
        instance_id: descriptor.instance_id.clone(),
        workspace_id: descriptor.workspace_id.clone(),
        workspace_generation: descriptor.workspace_generation.clone(),
        root_project_id: descriptor.root_project_id.clone(),
        principal_id: descriptor.principal_id.clone(),
        cwd_handle: descriptor.cwd.handle.clone(),
        catalog_revision: descriptor.revisions.catalog.clone(),
        policy_revision: descriptor.revisions.policy.clone(),
    }
}

async fn fetch_and_validate_catalog(
    transport: &RemoteTransport,
    descriptor: &contract::RemoteHostDescriptor,
    cancellation: &CancellationToken,
) -> Result<ToolManifest, RemoteWorkcellError> {
    let value = transport
        .request(
            "tools/list",
            json!({}),
            descriptor
                .capabilities
                .tool_catalog
                .limits
                .max_request_bytes,
            cancellation,
        )
        .await?;
    let list: ToolListWire =
        serde_json::from_value(value).map_err(|_| RemoteWorkcellError::InvalidProtocol)?;
    let manifest = freeze_catalog(list)?;
    if !super::canonical_remote_catalog(&manifest.tools) {
        return Err(RemoteWorkcellError::CatalogMismatch);
    }
    if manifest.revision.as_str() != descriptor.revisions.catalog.as_str() {
        return Err(RemoteWorkcellError::CatalogMismatch);
    }
    Ok(manifest)
}

fn freeze_catalog(list: ToolListWire) -> Result<ToolManifest, RemoteWorkcellError> {
    if list.result_type != "complete"
        || list.next_cursor.is_some()
        || list.ttl_ms != 0
        || list.cache_scope != "private"
        || list.meta.as_ref().is_some_and(|meta| !meta.is_empty())
    {
        return Err(RemoteWorkcellError::CatalogMismatch);
    }
    let mut seen = HashSet::new();
    let mut tools = Vec::with_capacity(list.tools.len());
    for tool in list.tools {
        if tool.name.is_empty()
            || tool.description.is_empty()
            || !seen.insert(tool.name.clone())
            || tool.icons.as_ref().is_some_and(|icons| !icons.is_array())
        {
            return Err(RemoteWorkcellError::CatalogMismatch);
        }
        validate_schema(&tool.input_schema)?;
        if tool.input_schema.get("type").and_then(Value::as_str) != Some("object") {
            return Err(RemoteWorkcellError::CatalogMismatch);
        }
        if let Some(schema) = &tool.output_schema {
            validate_schema(schema)?;
        }
        let presentation = tool
            .meta
            .get("ai.workcell/presentation-profile")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or(RemoteWorkcellError::CatalogMismatch)?;
        let contract = tool
            .meta
            .get("ai.workcell/contract")
            .and_then(Value::as_object)
            .ok_or(RemoteWorkcellError::CatalogMismatch)?;
        if contract.len() != 3 {
            return Err(RemoteWorkcellError::CatalogMismatch);
        }
        let contract_id = contract_string(contract, "id")?;
        let contract_version = contract_string(contract, "version")?;
        let result_version = contract_string(contract, "resultVersion")?;
        if contract_version != "v1" || result_version != "v1" {
            return Err(RemoteWorkcellError::CatalogMismatch);
        }
        tools.push(OwnedToolSpec {
            name: tool.name,
            title: tool.title,
            description: tool.description,
            input_schema: tool.input_schema,
            output_schema: tool.output_schema,
            annotations: ToolAnnotations {
                read_only_hint: tool.annotations.read_only_hint,
                destructive_hint: tool.annotations.destructive_hint,
                idempotent_hint: tool.annotations.idempotent_hint,
                open_world_hint: tool.annotations.open_world_hint,
            },
            presentation: presentation.to_owned(),
            contract_id: contract_id.to_owned(),
            contract_version: contract_version.to_owned(),
            result_version: result_version.to_owned(),
        });
    }
    let version = TOOL_MANIFEST_VERSION.to_owned();
    let revision = CatalogRevision::for_serializable(&(&version, &tools))
        .map_err(|_| RemoteWorkcellError::CatalogMismatch)?;
    Ok(ToolManifest {
        version,
        revision,
        tools,
    })
}

fn validate_schema(schema: &Map<String, Value>) -> Result<(), RemoteWorkcellError> {
    if schema
        .get("$schema")
        .is_some_and(|version| version.as_str() != Some(JSON_SCHEMA_VERSION))
    {
        return Err(RemoteWorkcellError::CatalogMismatch);
    }
    Ok(())
}

fn contract_string<'a>(
    contract: &'a Map<String, Value>,
    key: &str,
) -> Result<&'a str, RemoteWorkcellError> {
    contract
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(RemoteWorkcellError::CatalogMismatch)
}

fn workspace_capabilities(
    capabilities: &contract::RemoteHostCapabilities,
) -> WorkspaceCapabilities {
    let mut result = Vec::new();
    push_if(
        &mut result,
        transfer::compatible(&capabilities.reviewed_transfer, &capabilities.operations),
        WorkspaceCapability::ReviewedTransfer,
    );
    if let Some(workspace) = &capabilities.workspace {
        push_if(
            &mut result,
            workspace.methods.resolve_directory,
            WorkspaceCapability::Resolve,
        );
        push_if(
            &mut result,
            workspace.methods.stat,
            WorkspaceCapability::Stat,
        );
        push_if(
            &mut result,
            workspace.methods.list,
            WorkspaceCapability::List,
        );
        push_if(
            &mut result,
            workspace.methods.read_text,
            WorkspaceCapability::ReadText,
        );
        push_if(
            &mut result,
            workspace.methods.search_text,
            WorkspaceCapability::Search,
        );
    }
    push_if(
        &mut result,
        transfer::compatible(&capabilities.reviewed_transfer, &capabilities.operations),
        WorkspaceCapability::ReadBytes,
    );
    if let Some(operations) = &capabilities.operations {
        let executable = operations.methods.prepare && operations.methods.execute;
        if capabilities.workspace_mutation.is_some() {
            push_if(
                &mut result,
                executable,
                WorkspaceCapability::MutationExecute,
            );
            push_if(
                &mut result,
                executable && operations.methods.status,
                WorkspaceCapability::MutationStatus,
            );
            push_if(
                &mut result,
                executable && operations.methods.cancel,
                WorkspaceCapability::MutationCancel,
            );
            if let Some(mutation) = &capabilities.workspace_mutation {
                push_if(
                    &mut result,
                    mutation.atomic_across_files,
                    WorkspaceCapability::MutationAtomic,
                );
                push_if(
                    &mut result,
                    mutation.rollback_on_failure,
                    WorkspaceCapability::MutationRollback,
                );
            }
        }
        if capabilities.direct_exec.is_some() {
            push_if(&mut result, executable, WorkspaceCapability::ExecExecute);
            push_if(
                &mut result,
                executable && operations.methods.status,
                WorkspaceCapability::ExecStatus,
            );
            push_if(
                &mut result,
                executable && operations.methods.cancel,
                WorkspaceCapability::ExecCancel,
            );
            push_if(&mut result, executable, WorkspaceCapability::ExecTimeout);
        }
        push_if(
            &mut result,
            operations.methods.prepare,
            WorkspaceCapability::ToolPrepare,
        );
        push_if(&mut result, executable, WorkspaceCapability::ToolExecute);
        push_if(
            &mut result,
            executable && operations.methods.status,
            WorkspaceCapability::ToolStatus,
        );
        push_if(
            &mut result,
            executable && operations.methods.cancel,
            WorkspaceCapability::ToolCancel,
        );
        push_if(
            &mut result,
            operations.methods.prepare && operations.methods.release,
            WorkspaceCapability::ToolRelease,
        );
    }
    if let Some(watch) = &capabilities.watch {
        push_if(
            &mut result,
            watch.methods.open,
            WorkspaceCapability::WatchOpen,
        );
        push_if(
            &mut result,
            watch.methods.poll,
            WorkspaceCapability::WatchPoll,
        );
        push_if(
            &mut result,
            watch.methods.close,
            WorkspaceCapability::WatchClose,
        );
        push_if(
            &mut result,
            watch.recursive,
            WorkspaceCapability::WatchRecursive,
        );
        push_if(
            &mut result,
            watch.exact_rename_pairing,
            WorkspaceCapability::WatchExactRenamePairing,
        );
    }
    if let Some(scm) = &capabilities.scm {
        let has_mutation = scm.methods.stage || scm.methods.unstage || scm.methods.discard;
        push_if(
            &mut result,
            scm.methods.discover,
            WorkspaceCapability::ScmDiscover,
        );
        push_if(
            &mut result,
            scm.methods.status,
            WorkspaceCapability::ScmStatus,
        );
        push_if(&mut result, scm.methods.log, WorkspaceCapability::ScmLog);
        push_if(&mut result, scm.methods.diff, WorkspaceCapability::ScmDiff);
        push_if(
            &mut result,
            scm.methods.read_side,
            WorkspaceCapability::ScmReadSide,
        );
        push_if(
            &mut result,
            scm.methods.stage,
            WorkspaceCapability::ScmStage,
        );
        push_if(
            &mut result,
            scm.methods.unstage,
            WorkspaceCapability::ScmUnstage,
        );
        push_if(
            &mut result,
            scm.methods.discard,
            WorkspaceCapability::ScmDiscard,
        );
        if let Some(operations) = &capabilities.operations {
            push_if(
                &mut result,
                has_mutation && operations.methods.status,
                WorkspaceCapability::ScmMutationStatus,
            );
            push_if(
                &mut result,
                has_mutation && operations.methods.cancel,
                WorkspaceCapability::ScmMutationCancel,
            );
            push_if(
                &mut result,
                has_mutation && operations.methods.release,
                WorkspaceCapability::ScmMutationRelease,
            );
        }
    }
    if let Some(snapshots) = &capabilities.snapshots {
        let has_mutation = snapshots.methods.prepare_restore
            || snapshots.methods.prepare_unrevert
            || snapshots.methods.prepare_cleanup;
        push_if(
            &mut result,
            snapshot::compatible(capabilities),
            WorkspaceCapability::SnapshotCapture,
        );
        push_if(
            &mut result,
            false,
            WorkspaceCapability::SnapshotCaptureLabels,
        );
        push_if(
            &mut result,
            snapshots.methods.inspect,
            WorkspaceCapability::SnapshotInspect,
        );
        push_if(
            &mut result,
            snapshots.methods.status,
            WorkspaceCapability::SnapshotStatus,
        );
        push_if(
            &mut result,
            snapshots.methods.prepare_restore,
            WorkspaceCapability::SnapshotPrepareRestore,
        );
        push_if(
            &mut result,
            snapshots.methods.prepare_unrevert,
            WorkspaceCapability::SnapshotPrepareUnrevert,
        );
        push_if(
            &mut result,
            snapshots.methods.acknowledge,
            WorkspaceCapability::SnapshotAcknowledge,
        );
        push_if(
            &mut result,
            snapshots.methods.prepare_cleanup,
            WorkspaceCapability::SnapshotPrepareCleanup,
        );
        if let Some(operations) = &capabilities.operations {
            let executable = has_mutation && operations.methods.execute;
            push_if(
                &mut result,
                executable,
                WorkspaceCapability::SnapshotExecute,
            );
            push_if(
                &mut result,
                executable && operations.methods.status,
                WorkspaceCapability::SnapshotOperationStatus,
            );
            push_if(
                &mut result,
                executable && operations.methods.cancel,
                WorkspaceCapability::SnapshotCancel,
            );
            push_if(
                &mut result,
                has_mutation && operations.methods.release,
                WorkspaceCapability::SnapshotRelease,
            );
        }
        push_if(
            &mut result,
            snapshots.atomic_across_files,
            WorkspaceCapability::SnapshotAtomicAcrossFiles,
        );
        push_if(
            &mut result,
            snapshots.durable_per_file_journal,
            WorkspaceCapability::SnapshotDurablePerFileJournal,
        );
    }
    if let Some(assets) = &capabilities.project_assets {
        push_if(
            &mut result,
            assets.methods.discover,
            WorkspaceCapability::ProjectAssetsDiscover,
        );
        push_if(
            &mut result,
            assets.methods.read,
            WorkspaceCapability::ProjectAssetsRead,
        );
    }
    WorkspaceCapabilities::new(result)
}

fn push_if(
    capabilities: &mut Vec<WorkspaceCapability>,
    enabled: bool,
    capability: WorkspaceCapability,
) {
    if enabled {
        capabilities.push(capability);
    }
}

fn validate_response_binding(
    value: &Value,
    expected: &contract::HostBinding,
) -> Result<(), WorkspaceError> {
    let binding = value.get("binding").or_else(|| {
        value
            .get("operation")
            .and_then(|operation| operation.get("binding"))
    });
    let Some(host) = binding.and_then(|binding| binding.get("host")) else {
        return Ok(());
    };
    let actual: contract::HostBinding =
        serde_json::from_value(host.clone()).map_err(|_| invalid_response())?;
    if &actual == expected {
        Ok(())
    } else {
        Err(WorkspaceError::IdentityMismatch)
    }
}

fn validate_v1(version: contract::ContractVersion) -> Result<(), WorkspaceError> {
    (version == contract::ContractVersion::V1)
        .then_some(())
        .ok_or_else(invalid_response)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlotWait {
    Retry,
    Cancelled,
    Exhausted,
}

/// Parks a caller waiting for an operation slot without holding the registry
/// lock, so a release can actually happen while it waits. Returns as soon as a
/// slot is released, the poll interval lapses, or the caller is cancelled.
async fn park_for_slot(
    listener: EventListener,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> SlotWait {
    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
        return SlotWait::Exhausted;
    };
    future::or(
        async {
            listener.await;
            SlotWait::Retry
        },
        future::or(
            async {
                smol::Timer::after(remaining.min(RESERVATION_POLL)).await;
                SlotWait::Retry
            },
            async {
                cancellation.cancelled().await;
                SlotWait::Cancelled
            },
        ),
    )
    .await
}

fn invalid_response() -> WorkspaceError {
    WorkspaceError::InvalidResponse {
        violation: caudra_workspace::InvalidResponseKind::Malformed,
    }
}

fn stale_preparation(preparation_id: &OperationId) -> WorkspaceError {
    ResourceId::new(preparation_id.as_str()).map_or(WorkspaceError::Conflict, |resource_id| {
        WorkspaceError::StaleResource { resource_id }
    })
}

fn invalid_path() -> WorkspaceError {
    WorkspaceError::InvalidResponse {
        violation: caudra_workspace::InvalidResponseKind::InvalidPath,
    }
}

fn contract_path(path: &WorkspacePath) -> Result<contract::WorkspacePath, WorkspaceError> {
    contract::WorkspacePath::new(path.as_str()).map_err(|_| invalid_path())
}

fn workspace_path(path: &contract::WorkspacePath) -> Result<WorkspacePath, WorkspaceError> {
    WorkspacePath::new(path.as_str()).map_err(|_| invalid_path())
}

fn resource_id(id: &contract::ResourceId) -> Result<ResourceId, WorkspaceError> {
    ResourceId::new(id.as_str()).map_err(|_| invalid_response())
}

fn contract_resource_id(id: &ResourceId) -> Result<contract::ResourceId, WorkspaceError> {
    contract::ResourceId::new(id.as_str()).map_err(|_| invalid_response())
}

fn cwd_handle(id: &contract::ResourceId) -> Result<CwdHandle, WorkspaceError> {
    CwdHandle::new(id.as_str()).map_err(|_| invalid_response())
}

fn resource_revision(revision: &contract::Revision) -> Result<ResourceRevision, WorkspaceError> {
    ResourceRevision::new(revision.as_str()).map_err(|_| invalid_response())
}

fn contract_revision(revision: &ResourceRevision) -> Result<contract::Revision, WorkspaceError> {
    contract::Revision::new(revision.as_str()).map_err(|_| invalid_response())
}

fn collection_revision(
    revision: &contract::Revision,
) -> Result<CollectionRevision, WorkspaceError> {
    CollectionRevision::new(revision.as_str()).map_err(|_| invalid_response())
}

fn operation_id(id: &contract::Identifier) -> Result<OperationId, WorkspaceError> {
    OperationId::new(id.as_str()).map_err(|_| invalid_response())
}

fn contract_identifier<T>(id: &T) -> Result<contract::Identifier, WorkspaceError>
where
    T: AsRefOpaqueId,
{
    contract::Identifier::new(id.opaque_id()).map_err(|_| invalid_response())
}

trait AsRefOpaqueId {
    fn opaque_id(&self) -> &str;
}

macro_rules! opaque_id_ref {
    ($($type:ty),+ $(,)?) => {
        $(impl AsRefOpaqueId for $type {
            fn opaque_id(&self) -> &str {
                self.as_str()
            }
        })+
    };
}

opaque_id_ref!(
    OperationId,
    WatchSubscriptionId,
    SnapshotId,
    CheckpointId,
    RestoreId,
);

fn continuation(cursor: &contract::Cursor) -> Result<ContinuationToken, WorkspaceError> {
    ContinuationToken::new(cursor.as_str()).map_err(|_| invalid_response())
}

fn continuation_contract(cursor: &ContinuationToken) -> Result<contract::Cursor, WorkspaceError> {
    contract::Cursor::new(cursor.as_str()).map_err(|_| WorkspaceError::StaleCursor)
}

fn watch_cursor(cursor: &contract::Cursor) -> Result<WatchCursor, WorkspaceError> {
    WatchCursor::new(cursor.as_str()).map_err(|_| WorkspaceError::StaleCursor)
}

fn watch_subscription_id(id: &contract::Identifier) -> Result<WatchSubscriptionId, WorkspaceError> {
    WatchSubscriptionId::new(id.as_str()).map_err(|_| invalid_response())
}

fn child_scope(scope: &ResourceScope, id: ResourceId) -> Result<ResourceScope, WorkspaceError> {
    if scope.resource_id() == &id {
        return Ok(scope.clone());
    }
    let mut ancestors = scope.ancestors().to_vec();
    ancestors.push(scope.resource_id().clone());
    ResourceScope::new(ancestors, id).map_err(|_| invalid_response())
}

fn relative_path(
    base: &WorkspacePath,
    path: &WorkspacePath,
) -> Result<WorkspacePath, WorkspaceError> {
    if base == path {
        return Ok(WorkspacePath::root());
    }
    if base.is_root() {
        return Ok(path.clone());
    }
    let prefix = format!("{}/", base.as_str());
    let relative = path
        .as_str()
        .strip_prefix(&prefix)
        .ok_or_else(invalid_path)?;
    WorkspacePath::new(relative).map_err(|_| invalid_path())
}

fn join_workspace_path(
    base: &WorkspacePath,
    path: &WorkspacePath,
) -> Result<WorkspacePath, WorkspaceError> {
    if path.is_root() {
        return Ok(base.clone());
    }
    if base.is_root() {
        return Ok(path.clone());
    }
    WorkspacePath::new(format!("{}/{}", base.as_str(), path.as_str())).map_err(|_| invalid_path())
}

fn validate_selector_id(
    selector: &ResourceSelector,
    returned: &ResourceId,
) -> Result<(), WorkspaceError> {
    if let ResourceSelector::Id(expected) = selector
        && expected != returned
    {
        return Err(WorkspaceError::IdentityMismatch);
    }
    Ok(())
}

fn invalidate_mutation_aliases(paths: &mut ResourceCache, result: &MutationResult) {
    for entry in &result.results {
        paths.remove_path(&entry.path);
        if let Some(destination) = &entry.destination {
            paths.remove_path(destination);
        }
    }
}

fn path_within(parent: &WorkspacePath, path: &WorkspacePath, recursive: bool) -> bool {
    if parent == path {
        return false;
    }
    if parent.is_root() {
        return recursive || !path.as_str().contains('/');
    }
    let prefix = format!("{}/", parent.as_str());
    path.as_str()
        .strip_prefix(&prefix)
        .is_some_and(|relative| recursive || !relative.contains('/'))
}

fn watch_path_within(root: &WorkspacePath, path: &WorkspacePath, recursive: bool) -> bool {
    root == path || path_within(root, path, recursive)
}

fn require_nonzero_within<T>(value: T, maximum: T) -> Result<(), WorkspaceError>
where
    T: Copy + Ord + From<u8>,
{
    (value > T::from(0) && value <= maximum)
        .then_some(())
        .ok_or_else(invalid_response)
}

fn validate_string_fields(
    value: &Value,
    names: &[&str],
    maximum_bytes: usize,
) -> Result<(), WorkspaceError> {
    match value {
        Value::Array(values) => {
            for value in values {
                validate_string_fields(value, names, maximum_bytes)?;
            }
        }
        Value::Object(values) => {
            for (name, value) in values {
                if names.contains(&name.as_str()) {
                    match value {
                        Value::String(value) if value.len() <= maximum_bytes => {}
                        Value::Array(values)
                            if values.iter().all(|value| {
                                value
                                    .as_str()
                                    .is_some_and(|value| value.len() <= maximum_bytes)
                            }) => {}
                        Value::Null => {}
                        _ => return Err(invalid_response()),
                    }
                }
                validate_string_fields(value, names, maximum_bytes)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn serialized_items_bytes<T: Serialize>(items: &[T]) -> Result<usize, WorkspaceError> {
    items.iter().try_fold(0_usize, |total, item| {
        let item_bytes = serde_json::to_vec(item)
            .map_err(|_| invalid_response())?
            .len();
        total.checked_add(item_bytes).ok_or_else(invalid_response)
    })
}

fn pagination_flags(underlying_truncated: bool, has_cursor: bool) -> (bool, bool) {
    (has_cursor, underlying_truncated)
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(u64::MAX)
}

/// Only the two contracts that run a command need the longer client deadline.
/// Both are validated against the catalogue before an operation is stored, so a
/// server cannot widen its own budget by naming a contract it does not own.
fn execution_timeout(binding: &contract::OperationBinding) -> Option<Duration> {
    matches!(
        binding.contract.id.as_str(),
        SHELL_CONTRACT_ID | contract::DIRECT_EXEC_CONTRACT_ID
    )
    .then_some(SHELL_EXECUTION_TIMEOUT)
}

fn contract_binding(spec: &OwnedToolSpec) -> Result<contract::ContractBinding, WorkspaceError> {
    Ok(contract::ContractBinding {
        id: contract::Identifier::new(spec.contract_id.clone()).map_err(|_| invalid_response())?,
        version: contract::Identifier::new(spec.contract_version.clone())
            .map_err(|_| invalid_response())?,
        result_version: contract::Identifier::new(spec.result_version.clone())
            .map_err(|_| invalid_response())?,
    })
}

fn validate_contract_binding(
    binding: &contract::ContractBinding,
    spec: &OwnedToolSpec,
) -> Result<(), WorkspaceError> {
    if binding.id.as_str() == spec.contract_id
        && binding.version.as_str() == spec.contract_version
        && binding.result_version.as_str() == spec.result_version
    {
        Ok(())
    } else {
        Err(invalid_response())
    }
}

fn validate_fixed_contract(
    binding: &contract::ContractBinding,
    expected_id: &str,
) -> Result<(), WorkspaceError> {
    if binding.id.as_str() == expected_id
        && binding.version.as_str() == "v1"
        && binding.result_version.as_str() == "v1"
    {
        Ok(())
    } else {
        Err(invalid_response())
    }
}

fn canonical_journal_policy(tool_name: &str, intent: &contract::OperationIntent) -> Option<String> {
    let known_mutation = matches!(tool_name, "file_write" | "file_edit" | "file_apply_patch");
    let isolated = tool_name == "python_execution" && !intent.mutating;
    (known_mutation
        || intent.mutating
        || (intent.kind == contract::OperationKind::Execute && !isolated))
        .then(|| format!("{CANONICAL_OPERATION_PREFIX}{tool_name}"))
}

/// Whether a host's "no record" answer proves the operation never ran.
///
/// The host evicts tombstones oldest first and cannot forget an operation
/// before it was dispatched, so an operation dispatched after the reported
/// eviction boundary would still be retained if it had ever existed. An
/// operation that was never dispatched trivially qualifies. Without a boundary
/// the ledger has dropped nothing and every absence is authoritative.
fn absence_is_proof(operation: &RecoveryOperation, evicted_through: Option<u64>) -> bool {
    operation
        .dispatched_at
        .is_none_or(|dispatched| evicted_through.is_none_or(|through| dispatched > through))
}

fn journal_terminal_state<T>(state: &OperationState<T>) -> Option<(RemoteOperationState, bool)> {
    match state {
        OperationState::Completed {
            side_effects_possible,
            ..
        } => Some((RemoteOperationState::Succeeded, *side_effects_possible)),
        OperationState::Failed {
            side_effects_possible,
            ..
        } => Some((RemoteOperationState::Failed, *side_effects_possible)),
        OperationState::Cancelled {
            side_effects_possible,
        } => Some((RemoteOperationState::Cancelled, *side_effects_possible)),
        OperationState::NeverSeen
        | OperationState::Prepared
        | OperationState::Running
        | OperationState::Forgotten
        | OperationState::Indeterminate { .. } => None,
    }
}

fn recovery_status(
    response: &contract::StatusResponse,
    operation: &RecoveryOperation,
    host: &contract::HostBinding,
) -> Result<OperationStatus<Value>, WorkspaceError> {
    if response.binding.as_ref().is_some_and(|binding| {
        binding.argument_digest.as_str() != operation.request_digest.as_str()
    }) {
        return Err(WorkspaceError::IdentityMismatch);
    }
    let expected = OperationHandle {
        preparation_id: operation.preparation_id.clone(),
        invocation_id: Some(operation.invocation_id.clone()),
        execution_id: None,
        expires_at_unix_ms: response.expires_at_unix_ms,
    };
    convert_status(response, &expected, host, None, |value| Ok(value.clone()))
}

fn validate_prepare(
    response: &contract::PrepareResponse,
    host: &contract::HostBinding,
) -> Result<(), WorkspaceError> {
    validate_v1(response.version)?;
    response.intent.validate().map_err(|_| invalid_response())?;
    if &response.binding.host != host {
        return Err(WorkspaceError::IdentityMismatch);
    }
    Ok(())
}

fn status_request(
    operation: &OperationHandle,
    host: &contract::HostBinding,
) -> Result<contract::StatusRequest, WorkspaceError> {
    Ok(contract::StatusRequest {
        version: contract::ContractVersion::V1,
        after_sequence: None,
        selector: contract::OperationSelector {
            preparation_id: contract_identifier(&operation.preparation_id)?,
            invocation_id: operation
                .invocation_id
                .as_ref()
                .map(contract_identifier)
                .transpose()?,
            host: host.clone(),
        },
    })
}

fn convert_status<T, F>(
    response: &contract::StatusResponse,
    expected: &OperationHandle,
    host: &contract::HostBinding,
    expected_binding: Option<&contract::OperationBinding>,
    parse: F,
) -> Result<OperationStatus<T>, WorkspaceError>
where
    F: Fn(&Value) -> Result<T, WorkspaceError>,
{
    validate_v1(response.version)?;
    response.validate().map_err(|_| invalid_response())?;
    if response.preparation_id.as_str() != expected.preparation_id.as_str()
        || response.invocation_id.as_ref().map(|id| id.as_str())
            != expected.invocation_id.as_ref().map(OperationId::as_str)
        || expected.execution_id.as_ref().is_some_and(|expected_id| {
            response.execution_id.as_ref().map(|id| id.as_str()) != Some(expected_id.as_str())
        })
        || response.binding.is_some() && response.expires_at_unix_ms != expected.expires_at_unix_ms
    {
        return Err(WorkspaceError::IdentityMismatch);
    }
    if response
        .binding
        .as_ref()
        .is_some_and(|binding| &binding.host != host)
    {
        return Err(WorkspaceError::IdentityMismatch);
    }
    if let Some(expected_binding) = expected_binding
        && response
            .binding
            .as_ref()
            .is_some_and(|binding| binding != expected_binding)
    {
        return Err(WorkspaceError::IdentityMismatch);
    }
    let handle = OperationHandle {
        preparation_id: operation_id(&response.preparation_id)?,
        invocation_id: response
            .invocation_id
            .as_ref()
            .map(operation_id)
            .transpose()?,
        execution_id: response
            .execution_id
            .as_ref()
            .map(operation_id)
            .transpose()?,
        expires_at_unix_ms: response.expires_at_unix_ms,
    };
    let progress = response
        .progress
        .iter()
        .map(|progress| {
            if response.execution_id.as_ref() != Some(&progress.execution_id) {
                return Err(WorkspaceError::IdentityMismatch);
            }
            Ok(OperationProgress {
                execution_id: operation_id(&progress.execution_id)?,
                sequence: progress.sequence,
                kind: match progress.kind.as_str() {
                    "started" => OperationProgressKind::Started,
                    "stdout" => OperationProgressKind::Stdout,
                    "stderr" => OperationProgressKind::Stderr,
                    "exited" => OperationProgressKind::Exited,
                    other => OperationProgressKind::Unknown(
                        OperationId::new(other).map_err(|_| invalid_response())?,
                    ),
                },
                chunk: progress.chunk.as_str().to_owned(),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let outcome = response.outcome.as_ref();
    let state = match response.state {
        contract::OperationState::NeverSeen => {
            require_status_shape(response, false, false, false)?;
            OperationState::NeverSeen
        }
        contract::OperationState::Prepared => {
            require_status_shape(response, true, false, false)?;
            OperationState::Prepared
        }
        contract::OperationState::Running => {
            require_status_shape(response, true, true, false)?;
            OperationState::Running
        }
        contract::OperationState::Completed => {
            require_status_shape(response, true, true, true)?;
            let outcome = outcome.ok_or_else(invalid_response)?;
            if outcome.kind != contract::OutcomeKind::Completed
                || outcome.side_effects_possible
                || outcome.error.is_some()
                || outcome.result.as_ref().is_none_or(|result| result.is_error)
            {
                return Err(invalid_response());
            }
            let value = outcome
                .result
                .as_ref()
                .and_then(|result| result.structured_content.as_ref())
                .ok_or_else(invalid_response)?;
            OperationState::Completed {
                result: parse(value)?,
                side_effects_possible: outcome.side_effects_possible,
            }
        }
        contract::OperationState::Failed => {
            require_status_shape(response, true, true, true)?;
            let outcome = outcome.ok_or_else(invalid_response)?;
            if outcome.kind != contract::OutcomeKind::Failed || outcome.side_effects_possible {
                return Err(invalid_response());
            }
            let error = failed_operation_error(outcome)?;
            OperationState::Failed {
                error,
                side_effects_possible: outcome.side_effects_possible,
            }
        }
        contract::OperationState::Cancelled => {
            require_status_shape(response, true, true, true)?;
            let outcome = outcome.ok_or_else(invalid_response)?;
            if outcome.kind != contract::OutcomeKind::Cancelled
                || outcome.side_effects_possible
                || outcome.error.is_some()
            {
                return Err(invalid_response());
            }
            OperationState::Cancelled {
                side_effects_possible: outcome.side_effects_possible,
            }
        }
        contract::OperationState::Forgotten => {
            require_status_shape(response, false, false, false)?;
            OperationState::Forgotten
        }
        contract::OperationState::Indeterminate => {
            let retained = response.binding.is_some() && response.execution_id.is_some();
            let tombstone = response.binding.is_none()
                && response.execution_id.is_none()
                && response.outcome.is_none();
            if !retained && !tombstone {
                return Err(invalid_response());
            }
            if let Some(outcome) = outcome {
                if !outcome.side_effects_possible {
                    return Err(invalid_response());
                }
                validate_outcome_shape(outcome)?;
            }
            OperationState::Indeterminate {
                side_effects_possible: outcome.is_none_or(|outcome| outcome.side_effects_possible),
            }
        }
    };
    Ok(OperationStatus {
        handle,
        state,
        progress,
        progress_metadata: SequenceMetadata {
            first_retained_sequence: response.progress_metadata.first_retained_sequence,
            next_sequence: response.progress_metadata.next_sequence,
            gap_before_first: response.progress_metadata.gap_before_first,
        },
    })
}

fn remote_tool_result_envelope(
    envelope: &contract::ToolResultEnvelope,
) -> Result<RemoteToolResultEnvelope, WorkspaceError> {
    let structured_content = envelope
        .structured_content
        .clone()
        .ok_or_else(invalid_response)?;
    let model_output = envelope
        .content
        .iter()
        .map(|content| match content {
            contract::ToolResultContent::Text { text } => text.as_str(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(RemoteToolResultEnvelope {
        model_output,
        structured_content,
        is_error: envelope.is_error,
    })
}

fn failed_operation_error(
    outcome: &contract::StructuredOutcome,
) -> Result<OperationError, WorkspaceError> {
    match (&outcome.result, &outcome.error) {
        (Some(result), None) if result.is_error => Ok(OperationError {
            code: OperationId::new(
                result
                    .structured_content
                    .as_ref()
                    .and_then(|content| content["error"]["code"].as_str())
                    .unwrap_or(TOOL_ERROR_CODE),
            )
            .map_err(|_| invalid_response())?,
            message: result
                .content
                .iter()
                .map(|content| match content {
                    contract::ToolResultContent::Text { text } => text.as_str(),
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }),
        (None, Some(error)) => Ok(OperationError {
            code: operation_id(&error.code)?,
            message: error.message.as_str().to_owned(),
        }),
        _ => Err(invalid_response()),
    }
}

fn validate_outcome_shape(outcome: &contract::StructuredOutcome) -> Result<(), WorkspaceError> {
    match outcome.kind {
        contract::OutcomeKind::Completed
            if outcome.error.is_none()
                && outcome
                    .result
                    .as_ref()
                    .is_some_and(|result| !result.is_error) =>
        {
            Ok(())
        }
        contract::OutcomeKind::Failed => failed_operation_error(outcome).map(|_| ()),
        contract::OutcomeKind::Cancelled if outcome.error.is_none() => Ok(()),
        _ => Err(invalid_response()),
    }
}

fn operation_is_terminal<T>(state: &OperationState<T>) -> bool {
    !matches!(state, OperationState::Prepared | OperationState::Running)
}

fn require_status_shape(
    response: &contract::StatusResponse,
    binding: bool,
    execution: bool,
    outcome: bool,
) -> Result<(), WorkspaceError> {
    if response.binding.is_some() != binding
        || response.execution_id.is_some() != execution
        || response.outcome.is_some() != outcome
    {
        return Err(invalid_response());
    }
    Ok(())
}

fn indeterminate_status<T>(handle: OperationHandle) -> OperationStatus<T> {
    OperationStatus {
        handle,
        state: OperationState::Indeterminate {
            side_effects_possible: true,
        },
        progress: Vec::new(),
        progress_metadata: SequenceMetadata {
            first_retained_sequence: None,
            next_sequence: 1,
            gap_before_first: true,
        },
    }
}

fn operation_phase(state: contract::OperationState) -> OperationPhase {
    match state {
        contract::OperationState::NeverSeen => OperationPhase::NeverSeen,
        contract::OperationState::Prepared => OperationPhase::Prepared,
        contract::OperationState::Running => OperationPhase::Running,
        contract::OperationState::Completed => OperationPhase::Completed,
        contract::OperationState::Failed => OperationPhase::Failed,
        contract::OperationState::Cancelled => OperationPhase::Cancelled,
        contract::OperationState::Forgotten => OperationPhase::Forgotten,
        contract::OperationState::Indeterminate => OperationPhase::Indeterminate,
    }
}

fn workspace_mutation(mutation: &Mutation) -> Result<contract::WorkspaceMutation, WorkspaceError> {
    match mutation {
        Mutation::Write {
            path,
            content,
            condition,
        } => {
            let content = match content {
                WriteContent::Text(content) => content.clone(),
                WriteContent::Bytes(content) => {
                    String::from_utf8(content.clone()).map_err(|_| {
                        WorkspaceError::UnsupportedCapability {
                            capability: WorkspaceCapability::MutationExecute,
                        }
                    })?
                }
            };
            let content =
                contract::MutationContent::new(content).map_err(|_| invalid_response())?;
            match condition {
                MutationCondition::MustNotExist => Ok(contract::WorkspaceMutation::Create {
                    path: contract_path(path)?,
                    content,
                }),
                MutationCondition::Matches(revision) => Ok(contract::WorkspaceMutation::Write {
                    path: contract_path(path)?,
                    content,
                    expected_revision: contract_revision(revision)?,
                }),
            }
        }
        Mutation::CreateDirectory { path } => Ok(contract::WorkspaceMutation::Mkdir {
            path: contract_path(path)?,
        }),
        Mutation::Remove {
            path,
            expected_revision,
        } => Ok(contract::WorkspaceMutation::Delete {
            path: contract_path(path)?,
            expected_revision: contract_revision(expected_revision)?,
        }),
        Mutation::Move {
            source,
            destination,
            expected_revision,
        } => Ok(contract::WorkspaceMutation::Rename {
            from: contract_path(source)?,
            to: contract_path(destination)?,
            expected_revision: contract_revision(expected_revision)?,
        }),
    }
}

fn mutation_content_bytes(mutation: &Mutation) -> usize {
    match mutation {
        Mutation::Write { content, .. } => match content {
            WriteContent::Text(content) => content.len(),
            WriteContent::Bytes(content) => content.len(),
        },
        Mutation::CreateDirectory { .. } | Mutation::Remove { .. } | Mutation::Move { .. } => 0,
    }
}

fn parse_mutation_result(
    value: &Value,
    expected: &[MutationEntryResult],
) -> Result<MutationResult, WorkspaceError> {
    let result = parse_mutation_result_unbound(value)?;
    if !result.committed
        || result.rolled_back
        || result.results.len() != expected.len()
        || !result
            .results
            .iter()
            .zip(expected)
            .all(|(result, expected)| {
                result.kind == expected.kind
                    && result.path == expected.path
                    && result.destination == expected.destination
            })
    {
        return Err(invalid_response());
    }
    Ok(result)
}

fn parse_mutation_result_unbound(value: &Value) -> Result<MutationResult, WorkspaceError> {
    let response: contract::WorkspaceMutationResponse =
        serde_json::from_value(value.clone()).map_err(|_| invalid_response())?;
    validate_v1(response.version)?;
    if !response.committed || response.rolled_back {
        return Err(invalid_response());
    }
    Ok(MutationResult {
        committed: response.committed,
        rolled_back: response.rolled_back,
        atomic_across_files: response.atomic_across_files,
        results: response
            .results
            .iter()
            .map(|result| {
                Ok(MutationEntryResult {
                    kind: match result.kind {
                        contract::WorkspaceMutationKind::Create => MutationKind::Create,
                        contract::WorkspaceMutationKind::Write => MutationKind::Write,
                        contract::WorkspaceMutationKind::Mkdir => MutationKind::CreateDirectory,
                        contract::WorkspaceMutationKind::Rename => MutationKind::Move,
                        contract::WorkspaceMutationKind::Delete => MutationKind::Remove,
                    },
                    path: workspace_path(&result.path)?,
                    destination: result
                        .destination
                        .as_ref()
                        .map(workspace_path)
                        .transpose()?,
                    revision: result
                        .revision
                        .as_ref()
                        .map(resource_revision)
                        .transpose()?,
                })
            })
            .collect::<Result<_, WorkspaceError>>()?,
    })
}

fn expected_mutation_result(
    mutation: &Mutation,
    resolve_path: impl Fn(&WorkspacePath) -> Result<WorkspacePath, WorkspaceError>,
) -> Result<MutationEntryResult, WorkspaceError> {
    let (kind, path, destination) = match mutation {
        Mutation::Write {
            path, condition, ..
        } => (
            match condition {
                MutationCondition::MustNotExist => MutationKind::Create,
                MutationCondition::Matches(_) => MutationKind::Write,
            },
            path,
            None,
        ),
        Mutation::CreateDirectory { path } => (MutationKind::CreateDirectory, path, None),
        Mutation::Remove { path, .. } => (MutationKind::Remove, path, None),
        Mutation::Move {
            source,
            destination,
            ..
        } => (MutationKind::Move, source, Some(destination)),
    };
    Ok(MutationEntryResult {
        kind,
        path: resolve_path(path)?,
        destination: destination.map(resolve_path).transpose()?,
        revision: None,
    })
}

fn watch_resync_reason(reason: contract::WatchResyncReason) -> WatchResyncReason {
    match reason {
        contract::WatchResyncReason::CursorInvalid => WatchResyncReason::CursorInvalid,
        contract::WatchResyncReason::InstanceChanged => WatchResyncReason::InstanceChanged,
        contract::WatchResyncReason::Overflow => WatchResyncReason::Overflow,
        contract::WatchResyncReason::BackendError => WatchResyncReason::BackendError,
        contract::WatchResyncReason::RetentionLost => WatchResyncReason::RetentionLost,
        contract::WatchResyncReason::SubscriptionExpired => WatchResyncReason::SubscriptionExpired,
        contract::WatchResyncReason::SubscriptionClosed => WatchResyncReason::SubscriptionClosed,
    }
}

fn scm_revision(revision: &contract::Revision) -> Result<ScmRevision, WorkspaceError> {
    ScmRevision::new(revision.as_str()).map_err(|_| invalid_response())
}

fn scm_revisions(
    revisions: &contract::ScmRepositoryRevisions,
) -> Result<ScmRepositoryRevisions, WorkspaceError> {
    Ok(ScmRepositoryRevisions {
        repository: scm_revision(&revisions.repository)?,
        head: scm_revision(&revisions.head)?,
        index: scm_revision(&revisions.index)?,
        worktree: scm_revision(&revisions.worktree)?,
    })
}

fn scm_repository(repository: &contract::ScmRepository) -> Result<ScmRepository, WorkspaceError> {
    Ok(ScmRepository {
        handle: resource_id(&repository.handle)?,
        resource_id: resource_id(&repository.resource_id)?,
        root: workspace_path(&repository.root)?,
        identity: scm_revision(&repository.identity)?,
        revisions: scm_revisions(&repository.revisions)?,
    })
}

fn scm_change_kind(kind: contract::ScmChangeKind) -> ScmChangeKind {
    match kind {
        contract::ScmChangeKind::Added => ScmChangeKind::Added,
        contract::ScmChangeKind::Modified => ScmChangeKind::Modified,
        contract::ScmChangeKind::Deleted => ScmChangeKind::Deleted,
        contract::ScmChangeKind::Renamed => ScmChangeKind::Renamed,
        contract::ScmChangeKind::Copied => ScmChangeKind::Copied,
        contract::ScmChangeKind::TypeChanged => ScmChangeKind::TypeChanged,
        contract::ScmChangeKind::Unmerged => ScmChangeKind::Unmerged,
    }
}

fn scm_status_entry(entry: &contract::ScmStatusEntry) -> Result<ScmStatusEntry, WorkspaceError> {
    Ok(ScmStatusEntry {
        path: workspace_path(&entry.path)?,
        staged: entry.staged.map(scm_change_kind),
        unstaged: entry.unstaged.map(scm_change_kind),
        untracked: entry.untracked,
        conflicted: entry.conflicted,
    })
}

fn scm_commit(commit: &contract::ScmCommit) -> Result<ScmCommit, WorkspaceError> {
    Ok(ScmCommit {
        id: scm_revision(&commit.id)?,
        parents: commit
            .parents
            .iter()
            .map(scm_revision)
            .collect::<Result<_, _>>()?,
        author_name: commit.author_name.as_str().to_owned(),
        author_email: commit.author_email.as_str().to_owned(),
        committed_unix_seconds: commit.committed_unix_seconds,
        summary: commit.summary.as_str().to_owned(),
        body: commit.body.as_ref().map(|body| body.as_str().to_owned()),
    })
}

fn scm_diff_target_contract(
    target: &ScmDiffTarget,
) -> Result<contract::ScmDiffTarget, WorkspaceError> {
    Ok(match target {
        ScmDiffTarget::Staged => contract::ScmDiffTarget::Staged,
        ScmDiffTarget::Unstaged => contract::ScmDiffTarget::Unstaged,
        ScmDiffTarget::Tree { base, target } => contract::ScmDiffTarget::Tree {
            base: contract::Revision::new(base.as_str()).map_err(|_| invalid_response())?,
            target: contract::Revision::new(target.as_str()).map_err(|_| invalid_response())?,
        },
    })
}

fn scm_diff_line(line: &contract::ScmDiffLine) -> Result<ScmDiffLine, WorkspaceError> {
    Ok(ScmDiffLine {
        path: workspace_path(&line.path)?,
        kind: match line.kind {
            contract::ScmDiffLineKind::File => ScmDiffLineKind::File,
            contract::ScmDiffLineKind::Context => ScmDiffLineKind::Context,
            contract::ScmDiffLineKind::Addition => ScmDiffLineKind::Addition,
            contract::ScmDiffLineKind::Deletion => ScmDiffLineKind::Deletion,
            contract::ScmDiffLineKind::Binary => ScmDiffLineKind::Binary,
        },
        change: line.change.map(scm_change_kind),
        old_line: line.old_line,
        new_line: line.new_line,
        text: line.text.as_str().to_owned(),
    })
}

fn scm_side_contract(side: &ScmSide) -> Result<contract::ScmSide, WorkspaceError> {
    Ok(match side {
        ScmSide::Head => contract::ScmSide::Head,
        ScmSide::Index => contract::ScmSide::Index,
        ScmSide::Worktree => contract::ScmSide::Worktree,
        ScmSide::Commit { revision } => contract::ScmSide::Commit {
            revision: contract::Revision::new(revision.as_str()).map_err(|_| invalid_response())?,
        },
    })
}

fn scm_mutation_contract(mutation: &ScmMutation) -> Result<contract::ScmMutation, WorkspaceError> {
    Ok(match mutation {
        ScmMutation::Stage { paths } => contract::ScmMutation::Stage {
            paths: paths.iter().map(contract_path).collect::<Result<_, _>>()?,
        },
        ScmMutation::Unstage { paths } => contract::ScmMutation::Unstage {
            paths: paths.iter().map(contract_path).collect::<Result<_, _>>()?,
        },
        ScmMutation::Discard { paths } => contract::ScmMutation::Discard {
            paths: paths.iter().map(contract_path).collect::<Result<_, _>>()?,
        },
    })
}

fn scm_mutation(mutation: &contract::ScmMutation) -> Result<ScmMutation, WorkspaceError> {
    Ok(match mutation {
        contract::ScmMutation::Stage { paths } => ScmMutation::Stage {
            paths: paths.iter().map(workspace_path).collect::<Result<_, _>>()?,
        },
        contract::ScmMutation::Unstage { paths } => ScmMutation::Unstage {
            paths: paths.iter().map(workspace_path).collect::<Result<_, _>>()?,
        },
        contract::ScmMutation::Discard { paths } => ScmMutation::Discard {
            paths: paths.iter().map(workspace_path).collect::<Result<_, _>>()?,
        },
    })
}

fn parse_scm_mutation_result(value: &Value) -> Result<ScmMutationResult, WorkspaceError> {
    let response: contract::ScmMutationResponse =
        serde_json::from_value(value.clone()).map_err(|_| invalid_response())?;
    validate_v1(response.version)?;
    Ok(ScmMutationResult {
        mutation: scm_mutation(&response.mutation)?,
        revisions: scm_revisions(&response.revisions)?,
    })
}

fn snapshot_summary(
    summary: &contract::SnapshotSummary,
) -> Result<SnapshotSummary, WorkspaceError> {
    Ok(SnapshotSummary {
        snapshot_id: SnapshotId::new(summary.snapshot_id.as_str())
            .map_err(|_| invalid_response())?,
        checkpoint_id: summary
            .checkpoint_id
            .as_ref()
            .map(|id| CheckpointId::new(id.as_str()))
            .transpose()
            .map_err(|_| invalid_response())?,
        label: None,
        state: match summary.state {
            contract::SnapshotState::Complete => SnapshotState::Complete,
            contract::SnapshotState::Corrupt => SnapshotState::Corrupt,
        },
        manifest_revision: resource_revision(&summary.manifest_revision)?,
        scope: workspace_path(&summary.scope)?,
        file_count: summary.file_count,
        total_bytes: summary.total_bytes,
        skipped: snapshot_skipped(&summary.skipped),
        created_at_unix_ms: summary.created_at_unix_ms,
    })
}

fn snapshot_skipped(skipped: &contract::SnapshotSkipped) -> SnapshotSkipped {
    SnapshotSkipped {
        nested_repositories: skipped.nested_repositories,
        mounts: skipped.mounts,
        special_files: skipped.special_files,
        oversized_files: skipped.oversized_files,
        unreadable_entries: skipped.unreadable_entries,
        unstable_files: skipped.unstable_files,
        unrepresentable_names: skipped.unrepresentable_names,
        samples: skipped
            .samples
            .iter()
            .map(|sample| SnapshotSkippedEntry {
                path: sample.path.as_str().to_owned(),
                reason: match sample.reason {
                    contract::SnapshotSkipReason::NestedRepository => {
                        SnapshotSkipReason::NestedRepository
                    }
                    contract::SnapshotSkipReason::Mount => SnapshotSkipReason::Mount,
                    contract::SnapshotSkipReason::Special => SnapshotSkipReason::Special,
                    contract::SnapshotSkipReason::Oversized => SnapshotSkipReason::Oversized,
                    contract::SnapshotSkipReason::Unreadable => SnapshotSkipReason::Unreadable,
                    contract::SnapshotSkipReason::Unstable => SnapshotSkipReason::Unstable,
                    contract::SnapshotSkipReason::Unrepresentable => {
                        SnapshotSkipReason::Unrepresentable
                    }
                },
            })
            .collect(),
    }
}

/// A caller's ceilings, clamped to the host's. The host refuses a zero ceiling
/// as malformed, so the smallest a caller gets is one.
fn capture_limits(
    requested: &SnapshotCaptureLimits,
    host: &contract::WorkspaceSnapshotLimits,
) -> contract::SnapshotCaptureLimits {
    contract::SnapshotCaptureLimits {
        max_files: u32::try_from(requested.max_files)
            .unwrap_or(u32::MAX)
            .clamp(1, host.max_files),
        max_file_bytes: requested.max_file_bytes.clamp(1, host.max_file_bytes),
        max_total_bytes: requested.max_total_bytes.clamp(1, host.max_total_bytes),
    }
}

fn snapshot_file(file: &contract::SnapshotFile) -> Result<SnapshotFile, WorkspaceError> {
    Ok(SnapshotFile {
        path: workspace_path(&file.path)?,
        resource_id: resource_id(&file.resource_id)?,
        kind: match file.kind {
            contract::SnapshotEntryKind::File => SnapshotEntryKind::File,
            contract::SnapshotEntryKind::Symlink => SnapshotEntryKind::Symlink,
        },
        digest: resource_revision(&file.digest)?,
        mode: file.mode,
        size_bytes: file.size_bytes,
    })
}

fn validate_snapshot_summary(
    summary: &SnapshotSummary,
    limits: &contract::WorkspaceSnapshotLimits,
) -> Result<(), WorkspaceError> {
    if summary.file_count > limits.max_capture_entries
        || summary.total_bytes > limits.max_total_bytes
    {
        return Err(invalid_response());
    }
    Ok(())
}

fn snapshot_restore_preview(
    preview: &contract::SnapshotRestorePreview,
) -> Result<SnapshotRestorePreview, WorkspaceError> {
    Ok(SnapshotRestorePreview {
        restore_id: RestoreId::new(preview.restore_id.as_str()).map_err(|_| invalid_response())?,
        target_snapshot_id: SnapshotId::new(preview.target_snapshot_id.as_str())
            .map_err(|_| invalid_response())?,
        source_snapshot_id: SnapshotId::new(preview.source_snapshot_id.as_str())
            .map_err(|_| invalid_response())?,
        counts: SnapshotChangeCounts {
            create: preview.counts.create,
            replace: preview.counts.replace,
            delete: preview.counts.delete,
            conflict: preview.counts.conflict,
            unchanged: preview.counts.unchanged,
            created_directories: preview.counts.created_directories,
        },
        changes: preview
            .changes
            .iter()
            .map(|change| {
                Ok(SnapshotChange {
                    path: workspace_path(&change.path)?,
                    resource_id: resource_id(&change.resource_id)?,
                    kind: match change.kind {
                        contract::SnapshotChangeKind::Create => SnapshotChangeKind::Create,
                        contract::SnapshotChangeKind::Replace => SnapshotChangeKind::Replace,
                        contract::SnapshotChangeKind::Delete => SnapshotChangeKind::Delete,
                        contract::SnapshotChangeKind::Conflict => SnapshotChangeKind::Conflict,
                    },
                    current_revision: change
                        .current_revision
                        .as_ref()
                        .map(resource_revision)
                        .transpose()?,
                    target_revision: change
                        .target_revision
                        .as_ref()
                        .map(resource_revision)
                        .transpose()?,
                })
            })
            .collect::<Result<_, WorkspaceError>>()?,
        created_directories: preview
            .created_directories
            .iter()
            .map(workspace_path)
            .collect::<Result<_, _>>()?,
    })
}

fn checkpoint_ids(ids: &[contract::Identifier]) -> Result<Vec<CheckpointId>, WorkspaceError> {
    ids.iter()
        .map(|id| CheckpointId::new(id.as_str()).map_err(|_| invalid_response()))
        .collect()
}

fn snapshot_cleanup_preview(
    preview: &contract::SnapshotCleanupPreview,
) -> Result<SnapshotCleanupPreview, WorkspaceError> {
    Ok(SnapshotCleanupPreview {
        checkpoint_ids: checkpoint_ids(&preview.checkpoint_ids)?,
        missing_checkpoint_ids: checkpoint_ids(&preview.missing_checkpoint_ids)?,
        reclaimable_bytes: preview.reclaimable_bytes,
    })
}

/// Every requested checkpoint is either deleted or already gone, and nothing else is named.
fn cleanup_preview_partitions(
    requested: &[CheckpointId],
    preview: &SnapshotCleanupPreview,
) -> bool {
    let requested_len = requested.len();
    let requested = requested.iter().collect::<HashSet<_>>();
    let deletable = preview.checkpoint_ids.iter().collect::<HashSet<_>>();
    let missing = preview
        .missing_checkpoint_ids
        .iter()
        .collect::<HashSet<_>>();
    requested.len() == requested_len
        && requested.len() == preview.checkpoint_ids.len() + preview.missing_checkpoint_ids.len()
        && deletable.len() == preview.checkpoint_ids.len()
        && missing.len() == preview.missing_checkpoint_ids.len()
        && deletable.union(&missing).copied().collect::<HashSet<_>>() == requested
}

fn same_unique_ids(left: &[CheckpointId], right: &[CheckpointId]) -> bool {
    let left_ids = left.iter().collect::<HashSet<_>>();
    let right_ids = right.iter().collect::<HashSet<_>>();
    left_ids.len() == left.len() && right_ids.len() == right.len() && left_ids == right_ids
}

fn snapshot_restore_status(
    status: &contract::SnapshotRestoreStatus,
) -> Result<SnapshotRestoreStatus, WorkspaceError> {
    if status.applied_files > status.total_files {
        return Err(invalid_response());
    }
    Ok(SnapshotRestoreStatus {
        restore_id: RestoreId::new(status.restore_id.as_str()).map_err(|_| invalid_response())?,
        state: match status.state {
            contract::SnapshotRestoreState::Publishing => SnapshotRestoreState::Publishing,
            contract::SnapshotRestoreState::Completed => SnapshotRestoreState::Completed,
            contract::SnapshotRestoreState::Partial => SnapshotRestoreState::Partial,
            contract::SnapshotRestoreState::Indeterminate => SnapshotRestoreState::Indeterminate,
            contract::SnapshotRestoreState::Acknowledged => SnapshotRestoreState::Acknowledged,
            contract::SnapshotRestoreState::Reverted => SnapshotRestoreState::Reverted,
        },
        target_snapshot_id: SnapshotId::new(status.target_snapshot_id.as_str())
            .map_err(|_| invalid_response())?,
        source_snapshot_id: SnapshotId::new(status.source_snapshot_id.as_str())
            .map_err(|_| invalid_response())?,
        applied_files: status.applied_files,
        total_files: status.total_files,
        acknowledgement_required: status.acknowledgement_required,
        reconciliation_required: status.reconciliation_required,
        unrevert_of: status
            .unrevert_of
            .as_ref()
            .map(|id| RestoreId::new(id.as_str()))
            .transpose()
            .map_err(|_| invalid_response())?,
    })
}

fn parse_snapshot_result(
    value: &Value,
    preview: &SnapshotOperationPreview,
) -> Result<SnapshotOperationResult, WorkspaceError> {
    let result = parse_snapshot_result_unbound(value)?;
    let valid = match (preview, &result) {
        (SnapshotOperationPreview::Restore(preview), SnapshotOperationResult::Restore(status)) => {
            preview.restore_id == status.restore_id
                && preview.target_snapshot_id == status.target_snapshot_id
                && preview.source_snapshot_id == status.source_snapshot_id
        }
        (SnapshotOperationPreview::Unrevert(preview), SnapshotOperationResult::Restore(status)) => {
            preview.restore.restore_id == status.restore_id
                && preview.restore.target_snapshot_id == status.target_snapshot_id
                && preview.restore.source_snapshot_id == status.source_snapshot_id
                && status.unrevert_of.as_ref() == Some(&preview.source_restore_id)
        }
        (SnapshotOperationPreview::Cleanup(preview), SnapshotOperationResult::Cleanup(result)) => {
            same_unique_ids(&result.deleted_checkpoint_ids, &preview.checkpoint_ids)
        }
        _ => false,
    };
    valid.then_some(result).ok_or_else(invalid_response)
}

fn parse_snapshot_result_unbound(value: &Value) -> Result<SnapshotOperationResult, WorkspaceError> {
    if let Ok(status) = serde_json::from_value::<contract::SnapshotRestoreStatus>(value.clone()) {
        return Ok(SnapshotOperationResult::Restore(snapshot_restore_status(
            &status,
        )?));
    }
    if let Ok(response) = serde_json::from_value::<contract::SnapshotStatusResponse>(value.clone())
    {
        validate_v1(response.version)?;
        return Ok(SnapshotOperationResult::Restore(snapshot_restore_status(
            &response.restore,
        )?));
    }
    let response: contract::SnapshotCleanupResponse =
        serde_json::from_value(value.clone()).map_err(|_| invalid_response())?;
    validate_v1(response.version)?;
    Ok(SnapshotOperationResult::Cleanup(SnapshotCleanupResult {
        deleted_checkpoint_ids: checkpoint_ids(&response.deleted_checkpoint_ids)?,
        deleted_snapshots: response.deleted_snapshots,
        deleted_blobs: response.deleted_blobs,
        reclaimed_bytes: response.reclaimed_bytes,
    }))
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::fmt::Debug;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::str::FromStr;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use serde_json::{Value, json};
    use test_case::test_case;
    use tokio::sync::Mutex as AsyncMutex;
    use tokio_util::sync::CancellationToken;
    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event as TracingEvent, Metadata, Subscriber};

    use super::{
        BoundedMap, CatalogRevision, CursorRecord, CursorRegistry, DIRECT_EXEC_KIND,
        JSON_SCHEMA_VERSION, JournalOperation, JsonRpcError, MAX_SSE_EVENT_BYTES,
        OperationRegistry, PendingRemoteOperation, PreparedWorkspaceContext, RecoveryOperation,
        RemoteEvent, RemoteInner, RemoteMutationJournal, RemotePreparedToolCall, RemoteTransport,
        RemoteWorkcellClient, RemoteWorkcellError, ResourceCache, SHELL_CONTRACT_ID,
        SHELL_EXECUTION_TIMEOUT, StoredOperation, ToolListWire, WORKSPACE_MUTATION_KIND,
        WatchRegistry, canonical_journal_policy, cleanup_preview_partitions, convert_status,
        execution_timeout, freeze_catalog, join_workspace_path, map_rpc_error, numeric_loopback,
        pagination_flags, parse_content_range, parse_snapshot_result, project_asset_kind,
        project_asset_trust, recovery_status, require_full_remote_parity, require_nonzero_within,
        same_descriptor_except_instance, same_unique_ids, serialized_items_bytes,
        source_trust_anchor, unix_millis, validate_capabilities, validate_selector_id,
        watch_path_within, workspace_capabilities,
    };
    use crate::transfer::PrivateStaging;
    use crate::{
        Input, REMOTE_DEADLINE_BEFORE_DISPATCH, REMOTE_PREPARATION_RENEWAL, RemoteExecutionCleanup,
        RemotePreparedState, RemoteWorkcellInvocation, RemoteWorkcellTool,
        SHELL_DESCRIPTION_REPLACEMENTS, ToolKind, WorkcellHost, WorkcellTool,
    };
    use caudra_agent::cancel::CancelToken;
    use caudra_agent::tools::{
        Deadline, DescriptionContext, Tool, ToolAudience, ToolFilter, ToolRegistry,
    };
    use caudra_config::workcell::{
        RemoteWorkcellSelection, WorkcellEndpoint, WorkcellProfileName, WorkcellSourceRef,
    };
    use caudra_storage::StateDir;
    use caudra_storage::auth::WorkcellCredentialRef;
    use caudra_storage::remote_operation_journal::{
        RemoteOperationJournal, RemoteOperationState, RequestDigest,
    };
    use caudra_storage::workspace_binding::StoredWorkspaceBinding;
    use caudra_workspace::{
        AuthenticatedPrincipalId, AuthorityIdentity, CheckpointId, CwdHandle, ListRequest,
        Mutation, MutationCondition, MutationKind, MutationRequest, OperationHandle, OperationId,
        OperationState, PreparedToolCall, ProjectAssetKind, ProjectAssetTrust, ProjectIdentity,
        ProjectKey, ResourceId, ResourceRevision, ResourceScope, ResourceSelector, RestoreId,
        SessionBindingId, SessionWorkspaceBinding, SnapshotCaptureLimits, SnapshotCaptureRequest,
        SnapshotCaptureResult, SnapshotChangeCounts, SnapshotCleanupPreview, SnapshotId,
        SnapshotOperationPreview, SnapshotRestorePreview, SnapshotUnrevertPreview,
        SourceTrustAnchor, ToolPrepareRequest, WatchCursor, WatchOpenRequest, WatchPollRequest,
        WatchPollState, WatchResyncReason, WatchSubscription, WatchSubscriptionId,
        WorkspaceCapability, WorkspaceCursor, WorkspaceError, WorkspaceMutationService,
        WorkspacePath, WorkspaceReadService, WorkspaceSession, WorkspaceWatchService, WriteContent,
    };
    use workcell::shell::{DEFAULT_TIMEOUT_MS, MAX_TIMEOUT_SECS};
    use workcell::{OwnedToolSpec, ToolManifest, host_contract as contract};

    const HOST_SNAPSHOT_CEILING: u64 = 100;
    const PLAINTEXT_BEARER_BOUNDARY: &str =
        "a bearer may ride plaintext only to a numeric loopback literal";
    const TEST_REQUEST_DIGEST: &str =
        "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const UNMAPPED_RPC_CODE: i64 = -32602;
    const UNMAPPED_RPC_MESSAGE: &str = "path escapes the workspace root";
    const SHELL_KIND: &str = "canonical:shell";
    const WRITE_KIND: &str = "canonical:file_write";
    const CANCELLED: &str = "cancelled";
    /// Long past, so dispatch would renew the preparation on the host first.
    const LAPSED_EXPIRY_UNIX_MS: u64 = 0;
    const SNAPSHOT_CHECKPOINT: &str = "baseline-checkpoint";
    const SNAPSHOT_RPC_MESSAGE: &str = "snapshot request refused";
    const SNAPSHOT_PREPARATION: &str = "capture-prepared";
    const SNAPSHOT_POLLS: usize = 24;
    const SNAPSHOT_REFUSAL: i64 = -32602;
    const RPC_REFUSAL_LOG: &str = "remote Workcell request refused";
    const WATCH_ERRNO: i64 = 28;
    const WATCH_PHASE: &str = "register";
    const MUTATION_CWD: &str = "sub";
    const MUTATION_TARGET: &str = "sub/a.rs";
    const MUTATION_DESTINATION: &str = "sub/b.rs";
    const MUTATION_SHADOW: &str = "sub/sub/a.rs";
    const MUTATION_SHADOW_DESTINATION: &str = "sub/sub/b.rs";
    const MUTATION_ORIGINAL: &str = "original";
    const MUTATION_UPDATED: &str = "updated";
    const MUTATION_SHADOW_CONTENT: &str = "untouched";
    const MUTATION_PREPARATION: &str = "mutation-prepared";
    const CACHE_REGRESSION_ENTRIES: usize = 25_000;
    const CACHE_PRESSURE_LIMIT: usize = 1024;
    const CACHE_CLOCK_STEP: Duration = Duration::from_secs(1);
    const TIMEOUT_COMMAND: &str = "cargo test";

    #[test_case(true; "pinned_legacy_catalog")]
    #[test_case(false; "neutral_catalog")]
    fn embedded_and_remote_shell_descriptions_are_delivery_neutral(legacy: bool) {
        let root = tempfile::tempdir().unwrap();
        let host = WorkcellHost::new(root.path(), None).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint =
            WorkcellEndpoint::parse(&format!("http://{}/mcp", listener.local_addr().unwrap()))
                .unwrap();
        let client = snapshot_client(&endpoint, &StateDir::from_path(root.path().join("state")));
        let ctx = DescriptionContext {
            filter: &ToolFilter::All,
            audience: ToolAudience::MAIN,
            workflows_available: false,
        };
        for mut spec in crate::canonical_remote_specs() {
            let kind = ToolKind::from_name(spec.name).unwrap();
            let mut expected = spec.description.clone();
            if kind == ToolKind::Shell {
                for &(previous, current) in SHELL_DESCRIPTION_REPLACEMENTS {
                    expected = expected.replace(previous, current);
                    spec.description = if legacy {
                        spec.description.replace(current, previous)
                    } else {
                        spec.description.replace(previous, current)
                    };
                }
            }
            let remote = RemoteWorkcellTool {
                client: client.clone(),
                kind,
                spec: OwnedToolSpec::from(&spec),
            };
            let local = WorkcellTool {
                kind,
                spec,
                host: Arc::clone(&host.inner),
            };
            let local_description = local.description(&ctx);
            assert_eq!(local_description, expected);
            assert_eq!(remote.description(&ctx), expected);
            if kind == ToolKind::Shell {
                assert!(!local_description.to_lowercase().contains("background"));
                assert!(!local_description.contains("holds the call"));
            }
            assert_eq!(local.schema(), remote.schema());
            assert_eq!(local.spec.contract_id, remote.spec.contract_id);
            assert_eq!(local.spec.presentation, remote.spec.presentation);
        }
    }

    #[test_case("shell", json!({"command": TIMEOUT_COMMAND}), Some(Duration::from_millis(DEFAULT_TIMEOUT_MS)); "omitted_default")]
    #[test_case("shell", json!({"command": TIMEOUT_COMMAND, "timeoutSec": null}), Some(Duration::from_millis(DEFAULT_TIMEOUT_MS)); "null_default")]
    #[test_case("shell", json!({"command": TIMEOUT_COMMAND, "timeoutSec": 1}), Some(Duration::from_secs(1)); "minimum")]
    #[test_case("shell", json!({"command": TIMEOUT_COMMAND, "timeoutSec": MAX_TIMEOUT_SECS}), Some(Duration::from_secs(MAX_TIMEOUT_SECS)); "maximum")]
    #[test_case("shell", json!({"command": TIMEOUT_COMMAND, "timeoutSec": 0}), None; "zero_is_not_a_default")]
    #[test_case("shell", json!({"command": TIMEOUT_COMMAND, "timeoutSec": MAX_TIMEOUT_SECS + 1}), None; "overflow_is_not_clamped")]
    #[test_case("shell", json!({"command": TIMEOUT_COMMAND, "timeoutSec": -1}), None; "negative_is_rejected")]
    #[test_case("shell", json!({"command": TIMEOUT_COMMAND, "timeoutSec": "600"}), None; "string_is_rejected")]
    #[test_case("shell", json!({"command": TIMEOUT_COMMAND, "background": true}), None; "no_delivery_argument")]
    #[test_case("file_read", json!({"filePath": "Cargo.toml"}), None; "non_shell")]
    fn shell_timeout_metadata_matches_embedded_and_remote_invocations(
        name: &str,
        input: Value,
        expected: Option<Duration>,
    ) {
        let root = tempfile::tempdir().unwrap();
        let host = WorkcellHost::new(root.path(), None).unwrap();
        let registry = ToolRegistry::new();
        host.register(&registry).unwrap();
        let local = registry.get(name).unwrap().tool.parse(&input);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint =
            WorkcellEndpoint::parse(&format!("http://{}/mcp", listener.local_addr().unwrap()))
                .unwrap();
        let spec = crate::canonical_remote_specs()
            .into_iter()
            .find(|spec| spec.name == name)
            .unwrap();
        let remote = RemoteWorkcellTool {
            client: snapshot_client(&endpoint, &StateDir::from_path(root.path().join("state"))),
            kind: ToolKind::from_name(name).unwrap(),
            spec: OwnedToolSpec::from(&spec),
        }
        .parse(&input);
        assert_eq!(local.is_ok(), remote.is_ok());
        assert_eq!(
            local.as_ref().ok().and_then(|call| call.shell_timeout()),
            expected
        );
        assert_eq!(
            remote.as_ref().ok().and_then(|call| call.shell_timeout()),
            expected
        );
        if let Ok(call) = remote {
            assert_eq!(call.permission_input(), Some(&input));
            assert_eq!(call.shell_timeout(), expected);
            assert_eq!(call.permission_input(), Some(&input));
        }
        listener.set_nonblocking(true).unwrap();
        assert!(listener.accept().is_err());
    }

    #[test]
    fn authoritative_cursors_survive_capacity_pressure_and_reject_rebinding() {
        const CAPACITY: usize = 2;
        let seed = StoredWorkspaceBinding::local_from_cwd("opaque-workspace");
        let mut cursors = CursorRegistry {
            records: HashMap::new(),
            limit: CAPACITY,
        };
        for name in ["root", "nested", "overflow"] {
            let handle = CwdHandle::new(name).unwrap();
            let cursor = WorkspaceCursor::new(
                seed.binding(),
                ResourceScope::root(ResourceId::new(name).unwrap()),
                0,
                handle.clone(),
            );
            let result = cursors.insert(
                handle,
                CursorRecord {
                    cursor,
                    path: WorkspacePath::new(name).unwrap(),
                },
            );
            assert_eq!(result.is_ok(), name != "overflow");
        }
        let nested_handle = CwdHandle::new("nested").unwrap();
        let retained = cursors.records.get(&nested_handle).unwrap().clone();
        cursors
            .insert(nested_handle.clone(), retained.clone())
            .unwrap();
        let mut rebound = retained.clone();
        rebound.path = WorkspacePath::root();
        assert_eq!(
            cursors.insert(nested_handle.clone(), rebound),
            Err(WorkspaceError::StaleCursor)
        );
        assert_eq!(
            cursors.records.get(&nested_handle).unwrap().path,
            retained.path
        );
        assert_eq!(cursors.records.len(), CAPACITY);
    }

    #[test]
    fn modern_metadata_negotiates_the_remote_extension() {
        let metadata = super::request_metadata();
        assert_eq!(
            metadata["io.modelcontextprotocol/protocolVersion"],
            super::PROTOCOL_VERSION
        );
        assert_eq!(
            metadata["io.modelcontextprotocol/clientCapabilities"]["extensions"]
                [contract::EXTENSION_ID]["versions"],
            json!(["v1"])
        );
        assert!(metadata.get("protocolVersion").is_none());
    }

    /// A restore is recovered from its two captures and the live tree, so the
    /// host needs no per-file journal. It does need to report a restore's
    /// state, or an interrupted one could never be reconciled.
    #[test]
    fn full_parity_accepts_bounded_transfers_and_nonatomic_snapshots_without_a_per_file_journal() {
        let mut capabilities = full_capabilities();
        capabilities
            .reviewed_transfer
            .as_mut()
            .unwrap()
            .limits
            .max_file_bytes = 64 * 1024 * 1024;
        let snapshots = capabilities.snapshots.as_mut().unwrap();
        snapshots.atomic_across_files = false;
        snapshots.durable_per_file_journal = false;
        validate_capabilities(&capabilities).unwrap();
        require_full_remote_parity(&capabilities).unwrap();
        capabilities.snapshots.as_mut().unwrap().methods.status = false;
        assert_eq!(
            require_full_remote_parity(&capabilities),
            Err(RemoteWorkcellError::CapabilityMismatch)
        );
    }

    #[test]
    fn catalog_preserves_optional_annotations_and_union_output_schemas() {
        let mut list = tool_list("optional annotations and union output");
        list.tools[0].annotations.destructive_hint = None;
        list.tools[0].input_schema.remove("$schema");
        list.tools[0].output_schema = Some(
            serde_json::from_value(json!({"oneOf":[{"type":"object"},{"type":"array"}]})).unwrap(),
        );
        let manifest = freeze_catalog(list).unwrap();
        assert_eq!(manifest.tools[0].annotations.destructive_hint, None);
        assert!(
            manifest.tools[0]
                .output_schema
                .as_ref()
                .unwrap()
                .contains_key("oneOf")
        );
    }

    fn full_capabilities_value() -> Value {
        json!({
            "toolCatalog":{"version":"v1","limits":{"maxRequestBytes":1}},
            "toolExecution":{"version":"v1","limits":{"maxRequestBytes":1}},
            "executionEnvironment":{"version":"v1","limits":{"maxRequestBytes":1}},
            "reviewedTransfer":{
                "version":"v1","privateStaging":true,"sealedPublication":true,
                "conditionalDownload":true,"singleRange":true,"durableOutcomes":true,
                "createsDirectories":true,"safeInventory":true,
                "atomicReplaceAgainstExternalWriters":false,
                "limits":{"maxFileBytes":1,"maxStages":1,"maxReservedBytes":1,
                    "maxConcurrentIo":1,"stageTtlMs":1,"ioTimeoutMs":1,"maxJournals":1,
                    "maxJournalBytes":1,"maxJournalStorageBytes":1,"outcomeRetentionMs":1,
                    "streamBufferBytes":1}
            },
            "operations":{
                "version":"v1","exactPreparation":true,
                "methods":{"prepare":true,"execute":true,"release":true,"status":true,"cancel":true},
                "limits":{"preparationTtlMs":1,"maxPreparations":1,"maxOperations":1,
                    "maxLedgerBytes":1,"maxArgumentBytes":1,"maxResourceIntents":1,
                    "maxProgressEvents":1,"maxProgressBytes":1}
            },
            "workspace":{
                "version":"v1",
                "methods":{"resolveDirectory":true,"stat":true,"list":true,"readText":true,"searchText":true},
                "limits":{"maxPathBytes":1,"maxPageSize":1,"maxTextReadBytes":1,
                    "maxSearchPatternBytes":1,"maxCursorBytes":1,"maxListEntries":1,
                    "maxListRetainedBytes":1}
            },
            "watch":{
                "version":"v1","methods":{"open":true,"poll":true,"close":true},
                "limits":{"maxSubscriptions":1,"maxRetainedEvents":1,"maxRetainedBytes":1,
                    "maxLifetimeEvents":1,"maxPollEvents":1,"maxPollBytes":1,"maxWaitMs":1,
                    "subscriptionTtlMs":1},"recursive":true,"exactRenamePairing":false
            },
            "projectAssets":{
                "version":"v1","manifestVersion":"project-assets.v1",
                "methods":{"discover":true,"read":true},
                "limits":{"maxAssets":1,"maxReadBytes":1,"maxPathBytes":1,
                    "maxDiscoveryEntries":1,"maxDiscoveryRetainedBytes":1,"maxDiscoveryHashBytes":1}
            },
            "workspaceMutation":{"version":"v1","prepared":true,"maxMutations":1,
                "maxContentBytes":1,"atomicAcrossFiles":false,"rollbackOnFailure":true},
            "directExec":{"version":"v1","prepared":true,"interactive":false,
                "maxCommandBytes":1,"maxTimeoutMs":1},
            "scm":{
                "version":"v1","methods":{"discover":true,"status":true,"log":true,"diff":true,
                    "readSide":true,"stage":true,"unstage":true,"discard":true},
                "limits":{"maxConcurrentOperations":1,"maxPaths":1,"maxStatusEntries":1,
                    "maxStatusPaths":1,"maxConfigBytes":1,"maxLogEntries":1,"maxLogCommits":1,
                    "maxCommitBytes":1,"maxLogScanBytes":1,"maxDiffLines":1,"maxDiffBytes":1,
                    "maxDiffFiles":1,"maxDiffScanBytes":1,"maxDiffParsedLines":1,
                    "maxSideLines":1,"maxSideBytes":1,"maxCursorBytes":1},
                "preparedMutations":true,"discardUntracked":false
            },
            "snapshots":{
                "version":"v1","methods":{"capture":true,"prepareCapture":true,"checkpoint":true,"inspect":true,"status":true,
                    "prepareRestore":true,"prepareUnrevert":true,"acknowledge":true,"prepareCleanup":true},
                "limits":{"maxFiles":1,"maxFileBytes":1,"maxTotalBytes":1,"maxCaptureEntries":1,
                    "maxCapturePathBytes":1,"maxSnapshots":1,"maxStorageBytes":1,
                    "maxConcurrentCaptures":1,"maxCleanupCheckpoints":1},
                "atomicAcrossFiles":true,"durablePerFileJournal":true
            },
            "controlPlane":true,"controlPlaneMissing":[]
        })
    }

    fn full_capabilities() -> contract::RemoteHostCapabilities {
        serde_json::from_value(full_capabilities_value()).unwrap()
    }

    fn descriptor(instance: &str, generation: &str) -> contract::RemoteHostDescriptor {
        let mut value = json!({
            "version":"v1","serverId":"server","workspaceId":"workspace",
            "workspaceGeneration":generation,"rootProjectId":"project","principalId":"principal",
            "instanceId":instance,"resourceNamespaceVersion":"v1",
            "pathStyle":"root-relative-posix",
            "revisions":{"executionEnvironment":"environment","catalog":"catalog","policy":"policy"},
            "cwd":{"handle":"cwd","displayPath":"workspace"},
            "capabilities":{}
        });
        value["capabilities"] = full_capabilities_value();
        serde_json::from_value(value).unwrap()
    }

    fn selection(
        endpoint: &str,
        source: WorkcellSourceRef,
        credential_ref: Option<&str>,
    ) -> RemoteWorkcellSelection {
        RemoteWorkcellSelection {
            source,
            endpoint: WorkcellEndpoint::parse(endpoint).unwrap(),
            cwd: WorkspacePath::new("workspace").unwrap(),
            credential_ref: credential_ref
                .map(|reference| WorkcellCredentialRef::from_str(reference).unwrap()),
            expected_server_id: None,
            expected_workspace_id: None,
        }
    }

    #[test]
    fn static_origin_alone_defines_remote_source_trust() {
        let direct = selection(
            "https://WORKCELL.example:443/first/mcp",
            WorkcellSourceRef::Direct,
            Some("credential:first"),
        );
        let profile = selection(
            "https://workcell.example/other/mcp",
            WorkcellSourceRef::Profile(WorkcellProfileName::new("profile").unwrap()),
            Some("credential:other"),
        );
        let other_origin = selection(
            "https://workcell.example:8443/mcp",
            WorkcellSourceRef::Direct,
            Some("credential:first"),
        );

        assert_eq!(
            source_trust_anchor(&direct).unwrap().as_str(),
            "https://workcell.example"
        );
        assert_eq!(
            source_trust_anchor(&direct).unwrap(),
            source_trust_anchor(&profile).unwrap()
        );
        assert_ne!(
            source_trust_anchor(&direct).unwrap(),
            source_trust_anchor(&other_origin).unwrap()
        );
    }

    #[test]
    fn restart_identity_ignores_only_instance_and_rejects_generation_changes() {
        let initial = descriptor("instance-one", "generation-one");
        let restarted = descriptor("instance-two", "generation-one");
        let replaced = descriptor("instance-two", "generation-two");

        assert!(same_descriptor_except_instance(&initial, &restarted));
        assert!(!same_descriptor_except_instance(&initial, &replaced));
    }

    #[test]
    fn full_remote_parity_rejects_every_missing_capability_family() {
        let capabilities = full_capabilities();
        assert!(validate_capabilities(&capabilities).is_ok());
        assert!(require_full_remote_parity(&capabilities).is_ok());

        for family in [
            "executionEnvironment",
            "reviewedTransfer",
            "operations",
            "workspace",
            "watch",
            "projectAssets",
            "workspaceMutation",
            "directExec",
            "scm",
            "snapshots",
        ] {
            let mut value = full_capabilities_value();
            value[family] = Value::Null;
            let partial: contract::RemoteHostCapabilities = serde_json::from_value(value).unwrap();
            assert_eq!(
                require_full_remote_parity(&partial),
                Err(RemoteWorkcellError::CapabilityMismatch),
                "missing {family} was accepted"
            );
        }
        let mut no_replay = full_capabilities_value();
        no_replay["operations"]["exactPreparation"] = Value::Bool(false);
        let no_replay = serde_json::from_value(no_replay).unwrap();
        assert_eq!(
            require_full_remote_parity(&no_replay),
            Err(RemoteWorkcellError::CapabilityMismatch)
        );
        let mut no_progress = full_capabilities_value();
        no_progress["operations"]["limits"]["maxProgressEvents"] = json!(0);
        let no_progress = serde_json::from_value(no_progress).unwrap();
        assert_eq!(
            validate_capabilities(&no_progress),
            Err(RemoteWorkcellError::CapabilityMismatch)
        );
    }

    #[test_case("privateStaging")]
    #[test_case("sealedPublication")]
    #[test_case("conditionalDownload")]
    #[test_case("singleRange")]
    #[test_case("durableOutcomes")]
    #[test_case("createsDirectories")]
    #[test_case("safeInventory")]
    fn full_remote_parity_requires_current_reviewed_transfer_guarantees(field: &str) {
        let mut value = full_capabilities_value();
        value["reviewedTransfer"][field] = json!(false);
        let capabilities = serde_json::from_value(value).unwrap();
        assert_eq!(
            require_full_remote_parity(&capabilities),
            Err(RemoteWorkcellError::CapabilityMismatch)
        );
    }

    #[test_case("prepare")]
    #[test_case("execute")]
    #[test_case("status")]
    #[test_case("release")]
    #[test_case("cancel")]
    fn binary_read_and_transfer_require_the_operation_lifecycle(method: &str) {
        let mut value = full_capabilities_value();
        value["operations"]["methods"][method] = json!(false);
        let capabilities = serde_json::from_value(value).unwrap();
        assert_eq!(
            require_full_remote_parity(&capabilities),
            Err(RemoteWorkcellError::CapabilityMismatch)
        );
        let supported = super::workspace_capabilities(&capabilities);
        assert!(!supported.supports(WorkspaceCapability::ReadBytes));
        assert!(!supported.supports(WorkspaceCapability::ReviewedTransfer));
    }

    #[test_case(false; "missing_reviewed_transfer")]
    #[test_case(true; "obsolete_transfer_descriptor")]
    fn unsupported_servers_fail_discovery_without_catalog_or_raw_transfer_downgrade(
        obsolete: bool,
    ) {
        let mut descriptor = serde_json::to_value(descriptor("instance", "generation")).unwrap();
        descriptor["capabilities"]
            .as_object_mut()
            .unwrap()
            .remove("reviewedTransfer");
        if obsolete {
            descriptor["capabilities"]["fileTransfer"] =
                json!({"version":"v1","limits":{"maxBytes":1}});
        }
        let body = json!({"jsonrpc":"2.0","id":"$ID","result":{
            "resultType":"complete","ttlMs":0,"cacheScope":"private",
            "supportedVersions":[super::PROTOCOL_VERSION],
            "capabilities":{"extensions":{contract::EXTENSION_ID:descriptor}}
        }})
        .to_string()
        .replace("\"$ID\"", "$ID");
        let (endpoint, server) = serve_once(body, super::JSON_CONTENT_TYPE);
        let selection = selection(endpoint.as_url().as_str(), WorkcellSourceRef::Direct, None);
        let temp = tempfile::tempdir().unwrap();
        let state = StateDir::from_path(temp.path().join("state"));
        let result = smol::block_on(RemoteWorkcellClient::connect(
            &selection,
            None,
            SessionBindingId::new("test").unwrap(),
            RemoteOperationJournal::open(&state).unwrap(),
            CancellationToken::new(),
        ));
        assert_eq!(
            result.unwrap_err(),
            if obsolete {
                RemoteWorkcellError::InvalidProtocol
            } else {
                RemoteWorkcellError::CapabilityMismatch
            }
        );
        server.join().unwrap();
    }

    #[test]
    fn real_rpc_error_response_uses_code_without_kind_alias() {
        let response: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/remote_rpc_stale_resource.json"
        ))
        .unwrap();
        let error: JsonRpcError = serde_json::from_value(response["error"].clone()).unwrap();
        assert_eq!(map_rpc_error(&error), RemoteWorkcellError::StaleResource);
        assert_eq!(
            map_rpc_error(&rpc_error(json!({"kind":"policy_denied"}))),
            RemoteWorkcellError::UnmappedRefusal {
                code: UNMAPPED_RPC_CODE,
                symbolic: super::NO_SYMBOLIC_REASON.to_owned(),
            }
        );
    }

    /// A snapshot refusal says which ceiling it reached, and the caller decides
    /// on that: a full store can be pruned, a workspace over a limit cannot.
    #[test_case(
        json!({"code":"limit_exceeded","limit":"files","maximum":1}),
        WorkspaceError::LimitExceeded { limit: Some("files".into()), maximum: Some(1) }
        ; "limit_with_maximum"
    )]
    #[test_case(
        json!({"code":"limit_exceeded","limit":"ignoreRules"}),
        WorkspaceError::LimitExceeded { limit: Some("ignoreRules".into()), maximum: None }
        ; "limit_without_maximum"
    )]
    #[test_case(
        json!({"code":"quota_exceeded","limit":"storageBytes","maximum":2}),
        WorkspaceError::QuotaExceeded { limit: Some("storageBytes".into()), maximum: Some(2) }
        ; "quota_with_maximum"
    )]
    #[test_case(
        json!({"code":"quota_exceeded"}),
        WorkspaceError::QuotaExceeded { limit: None, maximum: None }
        ; "ledger_quota"
    )]
    #[test_case(
        json!({"code":"resource_limit"}),
        WorkspaceError::LimitExceeded { limit: None, maximum: None }
        ; "operation_intent_limit"
    )]
    #[test_case(
        json!({"code":"limit_exceeded","limit":"the files limit, see /etc/secret","maximum":"many"}),
        WorkspaceError::LimitExceeded { limit: None, maximum: None }
        ; "prose_never_travels_as_a_limit_name"
    )]
    #[test_case(json!({"code":"busy"}), WorkspaceError::Busy ; "busy")]
    #[test_case(json!({"code":"unsupported_file"}), WorkspaceError::UnsupportedEntry ; "unsupported_entry")]
    #[test_case(json!({"code":"not_repository"}), WorkspaceError::NotRepository ; "not_repository")]
    #[test_case(json!({"code":"watch_unavailable"}), WorkspaceError::WatchUnavailable ; "watch_unavailable")]
    #[test_case(json!({"code":"repository_locked"}), WorkspaceError::Conflict ; "repository_conflict")]
    #[test_case(json!({"code":"transferConflict"}), WorkspaceError::Conflict ; "transfer_conflict")]
    fn a_refusal_keeps_the_reason_the_host_gave(data: Value, expected: WorkspaceError) {
        assert_eq!(
            WorkspaceError::from(map_rpc_error(&rpc_error(data))),
            expected
        );
    }

    /// The host answered, and said why. Reporting that as a malformed response
    /// blames the wire and leaves the caller with nothing to act on, which is
    /// how a correct `file_read` surfaced as "invalid response".
    #[test_case(json!({"code":"capability_unavailable"}), "capability_unavailable" ; "absent_group")]
    #[test_case(json!({"code":"not_found"}), "not_found" ; "missing_path")]
    #[test_case(json!({"code":"repository_unavailable"}), "repository_unavailable" ; "repository_failure")]
    #[test_case(json!({"code":"unsupported_repository"}), "unsupported_repository" ; "unsupported_repository")]
    #[test_case(json!({"code":"unknown"}), "unknown" ; "unknown_code")]
    #[test_case(json!({"code":"not_repository_extra"}), "not_repository_extra" ; "not_repository_prefix")]
    #[test_case(json!({"code":"watch_unavailable_extra"}), "watch_unavailable_extra" ; "watch_unavailable_prefix")]
    #[test_case(json!({"kind":"not_repository"}), crate::remote::NO_SYMBOLIC_REASON ; "not_repository_kind_alias")]
    #[test_case(json!({"kind":"watch_unavailable"}), crate::remote::NO_SYMBOLIC_REASON ; "watch_unavailable_kind_alias")]
    #[test_case(json!({"code":"/private/workspace"}), crate::remote::NO_SYMBOLIC_REASON ; "path_is_not_a_symbolic_code")]
    #[test_case(json!({"code":"Bearer secret"}), crate::remote::NO_SYMBOLIC_REASON ; "prose_is_not_a_symbolic_code")]
    #[test_case(json!({"code":"x".repeat(crate::remote::MAX_RPC_DIAGNOSTIC_BYTES + 1)}), crate::remote::NO_SYMBOLIC_REASON ; "oversized_symbolic_code")]
    #[test_case(json!({}), crate::remote::NO_SYMBOLIC_REASON ; "no_symbolic_code")]
    fn an_unmapped_refusal_carries_its_reason_instead_of_claiming_malformed(
        data: Value,
        expected: &str,
    ) {
        let error = map_rpc_error(&rpc_error(data));

        assert_eq!(
            error,
            RemoteWorkcellError::UnmappedRefusal {
                code: UNMAPPED_RPC_CODE,
                symbolic: expected.to_owned(),
            }
        );
        assert_eq!(
            WorkspaceError::from(error),
            WorkspaceError::Refused {
                code: UNMAPPED_RPC_CODE,
                symbolic: expected.to_owned(),
            }
        );
    }

    fn rpc_error(data: Value) -> JsonRpcError {
        JsonRpcError {
            code: UNMAPPED_RPC_CODE,
            message: UNMAPPED_RPC_MESSAGE.to_owned(),
            data: Some(data),
        }
    }

    #[derive(Clone, Default)]
    struct RpcDiagnostics(Arc<Mutex<Vec<(String, String)>>>);

    impl Visit for RpcDiagnostics {
        fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
            self.0
                .lock()
                .unwrap()
                .push((field.name().to_owned(), format!("{value:?}")));
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            self.0
                .lock()
                .unwrap()
                .push((field.name().to_owned(), value.to_owned()));
        }
    }

    impl Subscriber for RpcDiagnostics {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            metadata.target() == "caudra_workcell::remote"
        }

        fn new_span(&self, _: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }

        fn record(&self, _: &Id, _: &Record<'_>) {}

        fn record_follows_from(&self, _: &Id, _: &Id) {}

        fn event(&self, event: &TracingEvent<'_>) {
            event.record(&mut self.clone());
        }

        fn enter(&self, _: &Id) {}

        fn exit(&self, _: &Id) {}
    }

    #[test_case(crate::remote::JSON_CONTENT_TYPE, contract::SCM_DISCOVER_METHOD, "repository_unavailable"; "json_unmapped")]
    #[test_case(crate::remote::SSE_CONTENT_TYPE, contract::SCM_DISCOVER_METHOD, "repository_unavailable"; "sse_unmapped")]
    #[test_case(crate::remote::JSON_CONTENT_TYPE, contract::WATCH_OPEN_METHOD, "watch_unavailable"; "json_watch")]
    #[test_case(crate::remote::SSE_CONTENT_TYPE, contract::WATCH_OPEN_METHOD, "watch_unavailable"; "sse_watch")]
    fn rpc_refusal_diagnostics_keep_call_context_without_payloads(
        content_type: &str,
        method: &str,
        symbolic: &str,
    ) {
        let body = json!({"jsonrpc":"2.0", "id":"$ID", "error": {
            "code":UNMAPPED_RPC_CODE, "message":"private host path /private/workspace",
            "data":{"code":symbolic, "phase":WATCH_PHASE, "rawOsError":WATCH_ERRNO,
                "token":"private-response-token", "path":"/private/workspace"}
        }})
        .to_string()
        .replace("\"$ID\"", "$ID");
        let body = if content_type == super::SSE_CONTENT_TYPE {
            format!("data: {body}\n\n")
        } else {
            body
        };
        let (endpoint, server) = serve_once(body, content_type);
        let (transport, _) = RemoteTransport::new(&endpoint, None).unwrap();
        let diagnostics = RpcDiagnostics::default();
        let error = tracing::subscriber::with_default(diagnostics.clone(), || {
            smol::block_on(transport.request(
                method,
                json!({"path":"/private/workspace", "token":"private-request-token"}),
                super::MAX_HTTP_RESPONSE_BYTES as u64,
                &CancellationToken::new(),
            ))
            .unwrap_err()
        });
        server.join().unwrap();
        assert_eq!(error, map_rpc_error(&rpc_error(json!({"code":symbolic}))));
        let fields = diagnostics.0.lock().unwrap();
        let mut recorded = fields.iter().cloned().collect::<HashMap<_, _>>();
        assert_eq!(recorded.len(), fields.len());
        recorded
            .remove("elapsed_ms")
            .unwrap()
            .parse::<u64>()
            .unwrap();
        assert_eq!(
            recorded,
            HashMap::from([
                ("message".to_owned(), RPC_REFUSAL_LOG.to_owned()),
                ("method".to_owned(), method.to_owned()),
                ("code".to_owned(), UNMAPPED_RPC_CODE.to_string()),
                ("symbolic".to_owned(), symbolic.to_owned()),
                ("phase".to_owned(), WATCH_PHASE.to_owned()),
                ("errno".to_owned(), WATCH_ERRNO.to_string()),
            ])
        );
    }

    fn serve_once(body: String, content_type: &str) -> (WorkcellEndpoint, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint =
            WorkcellEndpoint::parse(&format!("http://{}/mcp", listener.local_addr().unwrap()))
                .unwrap();
        let content_type = content_type.to_owned();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length: ") {
                    length = value.trim().parse().unwrap();
                }
            }
            let mut request = vec![0; length];
            reader.read_exact(&mut request).unwrap();
            let request: Value = serde_json::from_slice(&request).unwrap();
            let body = body.replace("$ID", &request["id"].to_string());
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        (endpoint, server)
    }

    fn host_binding() -> contract::HostBinding {
        serde_json::from_value(json!({
            "serverId":"server",
            "instanceId":"instance",
            "workspaceId":"workspace",
            "workspaceGeneration":"generation",
            "rootProjectId":"project",
            "principalId":"principal",
            "cwdHandle":"cwd",
            "catalogRevision":"catalog",
            "policyRevision":"policy"
        }))
        .unwrap()
    }

    fn operation_handle() -> OperationHandle {
        OperationHandle {
            preparation_id: OperationId::new("prepared").unwrap(),
            invocation_id: Some(OperationId::new("invocation").unwrap()),
            execution_id: None,
            expires_at_unix_ms: Some(42),
        }
    }

    fn completed_status() -> contract::StatusResponse {
        let host = host_binding();
        serde_json::from_value(json!({
            "version":"v1",
            "state":"completed",
            "preparationId":"prepared",
            "invocationId":"invocation",
            "executionId":"execution",
            "expiresAtUnixMs":42,
            "binding":{
                "host":host,
                "contract":{"id":"test.v1","version":"v1","resultVersion":"v1"},
                "argumentDigest":TEST_REQUEST_DIGEST
            },
            "outcome":{
                "kind":"completed",
                "sideEffectsPossible":false,
                "result":{
                    "version":"v1",
                    "content":[],
                    "structuredContent":{"ok":true},
                    "isError":false
                },
                "error":null
            },
            "progressMetadata":{
                "firstRetainedSequence":null,
                "nextSequence":1,
                "gapBeforeFirst":false
            },
            "progress":[]
        }))
        .unwrap()
    }

    fn tool_list(description: &str) -> ToolListWire {
        serde_json::from_value(json!({
            "resultType":"complete",
            "ttlMs":0,
            "cacheScope":"private",
            "tools":[{
                "name":"example",
                "title":"Example",
                "description":description,
                "inputSchema":{
                    "$schema":"http://json-schema.org/draft-07/schema#",
                    "type":"object"
                },
                "outputSchema":{
                    "$schema":"http://json-schema.org/draft-07/schema#",
                    "type":"object"
                },
                "annotations":{
                    "readOnlyHint":true,
                    "destructiveHint":false,
                    "idempotentHint":true,
                    "openWorldHint":false
                },
                "_meta":{
                    "ai.workcell/presentation-profile":"example.result.v1",
                    "ai.workcell/contract":{
                        "id":"example.v1",
                        "version":"v1",
                        "resultVersion":"v1"
                    },
                    "ignoredExtension":true
                },
                "ignoredExtension":true
            }],
            "ignoredExtension":true
        }))
        .unwrap()
    }

    /// A sandbox manager on the same host mints a bearer per sandbox and serves
    /// it over plaintext loopback. That request never reaches an interface, so
    /// the bearer is allowed; anything the host cannot vouch for is not.
    #[test_case("http://127.0.0.1:1234/mcp" ; "numeric_ipv4_loopback")]
    #[test_case("http://[::1]:1234/mcp" ; "numeric_ipv6_loopback")]
    #[test_case("https://workcell.example/mcp" ; "tls_anywhere")]
    fn bearer_is_carried_to_every_endpoint_selection_allows(endpoint: &str) {
        let endpoint = WorkcellEndpoint::parse(endpoint).unwrap();

        let result = RemoteTransport::new(&endpoint, Some(Arc::from("secret")));

        assert!(result.is_ok(), "{PLAINTEXT_BEARER_BOUNDARY}");
    }

    /// Plaintext off the host is refused a layer earlier, so the transport is
    /// never the only thing standing between a bearer and the network.
    #[test_case("http://10.0.2.100:1234/mcp" ; "plaintext_private_address")]
    #[test_case("http://workcell.example/mcp" ; "plaintext_public_name")]
    #[test_case("http://localhost:1234/mcp" ; "plaintext_resolvable_name")]
    fn plaintext_off_host_never_parses(endpoint: &str) {
        assert!(
            WorkcellEndpoint::parse(endpoint).is_err(),
            "{PLAINTEXT_BEARER_BOUNDARY}"
        );
    }

    #[test_case(contract::ProjectAssetKind::Instructions, ProjectAssetKind::Instructions ; "instructions")]
    #[test_case(contract::ProjectAssetKind::Skill, ProjectAssetKind::Skill ; "skill")]
    #[test_case(contract::ProjectAssetKind::Command, ProjectAssetKind::Command ; "command")]
    #[test_case(contract::ProjectAssetKind::Workflow, ProjectAssetKind::Workflow ; "workflow")]
    #[test_case(contract::ProjectAssetKind::Permissions, ProjectAssetKind::Permissions ; "permissions")]
    fn project_asset_kinds_are_mapped(
        contract_kind: contract::ProjectAssetKind,
        expected: ProjectAssetKind,
    ) {
        assert_eq!(project_asset_kind(contract_kind), expected);
    }

    #[test_case(contract::ProjectAssetTrust::Declarative, ProjectAssetTrust::Declarative ; "declarative")]
    #[test_case(contract::ProjectAssetTrust::ClientApprovalRequired, ProjectAssetTrust::ClientApprovalRequired ; "approval")]
    #[test_case(contract::ProjectAssetTrust::MixedReviewRequired, ProjectAssetTrust::MixedReviewRequired ; "mixed_review")]
    fn project_asset_trust_is_mapped(
        contract_trust: contract::ProjectAssetTrust,
        expected: ProjectAssetTrust,
    ) {
        assert_eq!(project_asset_trust(contract_trust), expected);
    }

    #[test]
    fn localhost_name_is_not_treated_as_proven_loopback() {
        let endpoint = url::Url::parse("http://localhost:1234/mcp").unwrap();
        assert!(!numeric_loopback(&endpoint));
    }

    #[test_case("bytes 0-9/10", Ok((0, 10, Some(10))); "complete")]
    #[test_case("bytes 5-9/*", Ok((5, 10, None)); "unknown_total")]
    #[test_case("bytes 5-4/10", Err(RemoteWorkcellError::InvalidProtocol); "reversed")]
    #[test_case("bytes 0-10/10", Err(RemoteWorkcellError::InvalidProtocol); "past_total")]
    fn content_range_is_strict(
        value: &str,
        expected: Result<(u64, u64, Option<u64>), RemoteWorkcellError>,
    ) {
        assert_eq!(parse_content_range(value), expected);
    }

    #[test]
    fn watch_events_must_remain_in_the_subscribed_scope() {
        let root = WorkspacePath::new("src").unwrap();
        assert!(watch_path_within(&root, &root, false));
        assert!(watch_path_within(
            &root,
            &WorkspacePath::new("src/lib.rs").unwrap(),
            false
        ));
        assert!(!watch_path_within(
            &root,
            &WorkspacePath::new("src/nested/lib.rs").unwrap(),
            false
        ));
        assert!(watch_path_within(
            &root,
            &WorkspacePath::new("src/nested/lib.rs").unwrap(),
            true
        ));
        assert!(!watch_path_within(
            &root,
            &WorkspacePath::new("secrets.txt").unwrap(),
            true
        ));
    }

    #[test]
    fn malformed_json_rpc_envelopes_are_rejected() {
        for body in [
            r#"{"id":$ID,"result":{}}"#,
            r#"{"jsonrpc":"2.0","result":{},"error":{"code":1,"message":"bad"},"id":$ID}"#,
            r#"{"jsonrpc":"2.0","error":{"code":1,"message":"bad"}}"#,
            r#"{"jsonrpc":"2.0","id":999,"result":{}}"#,
            r#"{"jsonrpc":"2.0","method":"notice","id":$ID,"params":{}}"#,
        ] {
            let (endpoint, server) = serve_once(body.to_owned(), "application/json");
            let (transport, _) = RemoteTransport::new(&endpoint, None).unwrap();
            let result = smol::block_on(transport.request(
                "test",
                json!({}),
                1024,
                &CancellationToken::new(),
            ));
            assert_eq!(result, Err(RemoteWorkcellError::InvalidProtocol));
            server.join().unwrap();
        }
    }

    #[test]
    fn disconnect_after_dispatch_is_distinguished_from_pre_send_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint =
            WorkcellEndpoint::parse(&format!("http://{}/mcp", listener.local_addr().unwrap()))
                .unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length: ") {
                    length = value.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
        });
        let (transport, _) = RemoteTransport::new(&endpoint, None).unwrap();
        let failure = smol::block_on(transport.request_tracked(
            "test",
            json!({}),
            1024,
            &CancellationToken::new(),
        ))
        .unwrap_err();
        assert_eq!(failure.error, RemoteWorkcellError::Transport);
        assert!(failure.dispatched);
        server.join().unwrap();
    }

    #[test]
    fn cancellation_after_dispatch_retains_indeterminate_classification() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint =
            WorkcellEndpoint::parse(&format!("http://{}/mcp", listener.local_addr().unwrap()))
                .unwrap();
        let cancellation = CancellationToken::new();
        let server_cancellation = cancellation.clone();
        let (release, hold) = mpsc::channel();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length: ") {
                    length = value.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            server_cancellation.cancel();
            hold.recv().unwrap();
        });
        let (transport, _) = RemoteTransport::new(&endpoint, None).unwrap();
        let failure =
            smol::block_on(transport.request_tracked("test", json!({}), 1024, &cancellation))
                .unwrap_err();
        assert_eq!(failure.error, RemoteWorkcellError::Cancelled);
        assert!(failure.dispatched);
        release.send(()).unwrap();
        server.join().unwrap();
    }

    fn binding_for(contract_id: &str) -> contract::OperationBinding {
        let mut binding = completed_status().binding.unwrap();
        binding.contract.id = contract::Identifier::new(contract_id).unwrap();
        binding
    }

    /// The server answers an execution request only when the command is done, so
    /// the two contracts that run one need a request deadline longer than the
    /// transport default. Nothing else does, and a contract the catalogue never
    /// validated cannot claim it.
    #[test_case(SHELL_CONTRACT_ID => Some(SHELL_EXECUTION_TIMEOUT) ; "the shell waits for its command")]
    #[test_case(contract::DIRECT_EXEC_CONTRACT_ID => Some(SHELL_EXECUTION_TIMEOUT) ; "so does a direct host command")]
    #[test_case("file.read.v1" => None ; "an ordinary tool keeps the transport default")]
    #[test_case("test.v1" => None ; "an unknown contract cannot widen its own budget")]
    fn only_command_contracts_extend_the_request_deadline(contract_id: &str) -> Option<Duration> {
        execution_timeout(&binding_for(contract_id))
    }

    #[test]
    fn terminal_status_requires_matching_shape_and_ids() {
        let status = completed_status();
        let converted = convert_status(
            &status,
            &operation_handle(),
            &host_binding(),
            None,
            |value| Ok(value.clone()),
        )
        .unwrap();
        assert!(matches!(converted.state, OperationState::Completed { .. }));

        let mut malformed = serde_json::to_value(status).unwrap();
        malformed["outcome"]["result"] = Value::Null;
        let malformed: contract::StatusResponse = serde_json::from_value(malformed).unwrap();
        assert!(matches!(
            convert_status(
                &malformed,
                &operation_handle(),
                &host_binding(),
                None,
                |value| Ok(value.clone())
            ),
            Err(WorkspaceError::InvalidResponse { .. })
        ));

        let mut wrong_id = completed_status();
        wrong_id.execution_id = Some(contract::Identifier::new("foreign").unwrap());
        let mut expected = operation_handle();
        expected.execution_id = Some(OperationId::new("execution").unwrap());
        assert_eq!(
            convert_status(&wrong_id, &expected, &host_binding(), None, |value| Ok(
                value.clone()
            )),
            Err(WorkspaceError::IdentityMismatch)
        );

        let expected_binding = completed_status().binding.unwrap();
        let mut wrong_binding = completed_status();
        wrong_binding.binding.as_mut().unwrap().argument_digest =
            contract::Revision::new("foreign").unwrap();
        assert_eq!(
            convert_status(
                &wrong_binding,
                &operation_handle(),
                &host_binding(),
                Some(&expected_binding),
                |value| Ok(value.clone())
            ),
            Err(WorkspaceError::IdentityMismatch)
        );
    }

    #[test]
    fn failed_result_envelopes_and_cancelled_results_are_valid_terminal_shapes() {
        let mut failed = completed_status();
        failed.state = contract::OperationState::Failed;
        let outcome = failed.outcome.as_mut().unwrap();
        outcome.kind = contract::OutcomeKind::Failed;
        outcome.result.as_mut().unwrap().is_error = true;
        assert!(matches!(
            convert_status(
                &failed,
                &operation_handle(),
                &host_binding(),
                None,
                |value| Ok(value.clone())
            )
            .unwrap()
            .state,
            OperationState::Failed { .. }
        ));
        failed
            .outcome
            .as_mut()
            .unwrap()
            .result
            .as_mut()
            .unwrap()
            .is_error = false;
        assert!(
            convert_status(
                &failed,
                &operation_handle(),
                &host_binding(),
                None,
                |value| Ok(value.clone())
            )
            .is_err()
        );

        let mut cancelled = completed_status();
        cancelled.state = contract::OperationState::Cancelled;
        cancelled.outcome.as_mut().unwrap().kind = contract::OutcomeKind::Cancelled;
        assert!(matches!(
            convert_status(
                &cancelled,
                &operation_handle(),
                &host_binding(),
                None,
                |value| Ok(value.clone())
            )
            .unwrap()
            .state,
            OperationState::Cancelled { .. }
        ));
    }

    #[test_case(
        Some(serde_json::json!({"error": {"code": crate::remote::STALE_RESOURCE_CODE}})),
        crate::remote::STALE_RESOURCE_CODE;
        "a_failed_file_mutation_names_its_cause"
    )]
    #[test_case(None, crate::remote::TOOL_ERROR_CODE; "a_text_only_failure_stays_generic")]
    fn a_failed_tool_result_keeps_the_code_the_host_gave_it(structured: Option<Value>, code: &str) {
        let mut failed = completed_status();
        failed.state = contract::OperationState::Failed;
        let outcome = failed.outcome.as_mut().unwrap();
        outcome.kind = contract::OutcomeKind::Failed;
        let result = outcome.result.as_mut().unwrap();
        result.is_error = true;
        result.structured_content = structured;

        let state = convert_status(
            &failed,
            &operation_handle(),
            &host_binding(),
            None,
            |value| Ok(value.clone()),
        )
        .unwrap()
        .state;

        let OperationState::Failed { error, .. } = state else {
            panic!("a failed status must convert to a failed state: {state:?}");
        };
        assert_eq!(error.code.as_str(), code);
    }

    #[test]
    fn operation_registry_refuses_capacity_without_evicting_persisted_operations() {
        let preparation_id = OperationId::new("prepared").unwrap();
        let invocation_id = OperationId::new("invocation").unwrap();
        let expires_at_unix_ms = 0;
        let mut registry = OperationRegistry::new(1);
        assert!(registry.reserve());
        assert!(!registry.reserve());
        registry.finish_reservation();
        registry
            .insert(
                preparation_id.clone(),
                StoredOperation {
                    transfer: None,
                    binding: completed_status().binding.unwrap(),
                    context: None,
                    invocation_id: invocation_id.clone(),
                    expires_at_unix_ms,
                    journal: None,
                    persisted: true,
                },
            )
            .unwrap();
        assert!(!registry.reserve());
        assert_eq!(registry.len(), 1);
        let handle = OperationHandle {
            preparation_id: preparation_id.clone(),
            invocation_id: Some(invocation_id),
            execution_id: None,
            expires_at_unix_ms: Some(expires_at_unix_ms),
        };
        assert!(registry.get(&handle).is_ok());
        registry.remove(&preparation_id);
        assert!(registry.reserve());
    }

    /// Refusing the moment the host's limit is reached made a second
    /// concurrent tool call look like a conflict the user caused, so a waiting
    /// caller has to be woken by the release rather than left to time out.
    #[test]
    fn a_parked_reservation_wakes_when_a_slot_is_released() {
        smol::block_on(async {
            let slots = super::Event::new();
            let listener = slots.listen();
            slots.notify(1);

            assert_eq!(
                super::park_for_slot(listener, &CancellationToken::new(), far_deadline()).await,
                super::SlotWait::Retry
            );
        });
    }

    #[test]
    fn a_parked_reservation_gives_up_when_the_caller_is_cancelled() {
        smol::block_on(async {
            let slots = super::Event::new();
            let listener = slots.listen();
            let cancellation = CancellationToken::new();
            cancellation.cancel();

            assert_eq!(
                super::park_for_slot(listener, &cancellation, far_deadline()).await,
                super::SlotWait::Cancelled
            );
        });
    }

    #[test]
    fn a_parked_reservation_reports_exhaustion_past_its_deadline() {
        smol::block_on(async {
            let slots = super::Event::new();
            let listener = slots.listen();

            assert_eq!(
                super::park_for_slot(listener, &CancellationToken::new(), super::Instant::now())
                    .await,
                super::SlotWait::Exhausted
            );
        });
    }

    fn far_deadline() -> super::Instant {
        super::Instant::now() + super::RESERVATION_WAIT
    }

    fn journal_operation(id: &str, operation_kind: &str) -> JournalOperation {
        JournalOperation {
            publication_cwd: None,
            publication_id: None,
            host_instance_id: "instance".to_owned(),
            operation_id: OperationId::new(format!("operation-{id}")).unwrap(),
            invocation_id: OperationId::new(format!("invocation-{id}")).unwrap(),
            preparation_id: OperationId::new(format!("preparation-{id}")).unwrap(),
            operation_kind: operation_kind.to_owned(),
            request_digest: RequestDigest::sha256(TEST_REQUEST_DIGEST).unwrap(),
        }
    }

    #[test]
    fn canonical_mutations_and_unknown_effect_execution_are_journaled() {
        let read: contract::OperationIntent = serde_json::from_value(json!({
            "kind":"read",
            "mutating":false,
            "resources":[{
                "resourceId":"resource-a", "scope":["resource-a"],
                "display":"redacted",
                "access":"read",
                "revision":null
            }]
        }))
        .unwrap();
        let execute: contract::OperationIntent = serde_json::from_value(json!({
            "kind":"execute",
            "mutating":false,
            "resources":[{
                "resourceId":"workspace", "scope":["workspace"],
                "display":"redacted",
                "access":"execute",
                "revision":null
            }]
        }))
        .unwrap();

        assert!(canonical_journal_policy("file_read", &read).is_none());
        assert!(canonical_journal_policy("file_index", &read).is_none());
        assert!(canonical_journal_policy("python_execution", &execute).is_none());
        assert_eq!(
            canonical_journal_policy("file_write", &read),
            Some(WRITE_KIND.to_owned())
        );
        assert_eq!(
            canonical_journal_policy("shell", &execute),
            Some(SHELL_KIND.to_owned())
        );
    }

    #[test]
    fn journal_survives_before_send_after_send_and_before_terminal_commit() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let binding = StoredWorkspaceBinding::local_from_cwd("opaque-workspace");
        let operation = journal_operation("crash", "workspace_mutation");
        {
            let journal = RemoteOperationJournal::open(&state_dir).unwrap();
            let coordinator = RemoteMutationJournal::new(journal, binding.clone()).unwrap();
            coordinator.reserve(&operation).unwrap();
            assert_eq!(
                coordinator.pending()[0].state,
                RemoteOperationState::Reserved
            );
        }
        {
            let journal = RemoteOperationJournal::open(&state_dir).unwrap();
            let coordinator = RemoteMutationJournal::new(journal, binding.clone()).unwrap();
            assert_eq!(
                coordinator.pending()[0].state,
                RemoteOperationState::Reserved
            );
            coordinator
                .mark_dispatched(&operation.operation_id)
                .unwrap();
        }
        let journal = RemoteOperationJournal::open(&state_dir).unwrap();
        let coordinator = RemoteMutationJournal::new(journal, binding).unwrap();
        assert_eq!(
            coordinator.pending()[0].state,
            RemoteOperationState::Dispatched
        );
        assert!(
            coordinator
                .commit_terminal(
                    &operation.operation_id,
                    &OperationState::Completed {
                        result: json!({"ok": true}),
                        side_effects_possible: false,
                    },
                )
                .unwrap()
        );
        assert!(coordinator.pending().is_empty());
    }

    #[test]
    fn cancellation_before_dispatch_is_cleanly_acknowledged() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let binding = StoredWorkspaceBinding::local_from_cwd("opaque-workspace");
        let journal = RemoteOperationJournal::open(&state_dir).unwrap();
        let coordinator = RemoteMutationJournal::new(journal, binding.clone()).unwrap();
        let operation = journal_operation("cancelled", "workspace_mutation");
        coordinator.reserve(&operation).unwrap();

        coordinator.abandon(&operation.operation_id).unwrap();

        assert!(coordinator.pending().is_empty());
        drop(coordinator);
        let journal = RemoteOperationJournal::open(&state_dir).unwrap();
        assert!(journal.list_pending(&binding).unwrap().is_empty());
    }

    /// The journal records operations so recovery can find them; it never
    /// decides what may run. An operation whose outcome is still open, in any
    /// state, holds back neither a command nor a write.
    #[test_case(SHELL_KIND, WRITE_KIND, RemoteOperationState::Reserved; "reserved_shell_then_write")]
    #[test_case(SHELL_KIND, WRITE_KIND, RemoteOperationState::Dispatched; "dispatched_shell_then_write")]
    #[test_case(SHELL_KIND, WRITE_KIND, RemoteOperationState::Indeterminate; "indeterminate_shell_then_write")]
    #[test_case(WRITE_KIND, SHELL_KIND, RemoteOperationState::Dispatched; "dispatched_write_then_shell")]
    #[test_case(WRITE_KIND, WRITE_KIND, RemoteOperationState::Dispatched; "dispatched_write_then_write")]
    #[test_case(DIRECT_EXEC_KIND, DIRECT_EXEC_KIND, RemoteOperationState::Dispatched; "dispatched_exec_then_exec")]
    fn open_operations_never_block_new_reservations(
        first_kind: &str,
        second_kind: &str,
        first_state: RemoteOperationState,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let binding = StoredWorkspaceBinding::local_from_cwd("opaque-workspace");
        let journal = RemoteOperationJournal::open(&state_dir).unwrap();
        let coordinator = RemoteMutationJournal::new(journal, binding).unwrap();
        let first = journal_operation("first", first_kind);
        let second = journal_operation("second", second_kind);
        reserve_in_state(&coordinator, &first, first_state);

        coordinator.reserve(&second).unwrap();

        let states = coordinator
            .pending()
            .into_iter()
            .map(|pending| (pending.operation_id, pending.state))
            .collect::<Vec<_>>();
        assert_eq!(
            states,
            vec![
                (first.operation_id, first_state),
                (second.operation_id, RemoteOperationState::Reserved),
            ]
        );
    }

    fn reserve_in_state(
        coordinator: &RemoteMutationJournal,
        operation: &JournalOperation,
        state: RemoteOperationState,
    ) {
        coordinator.reserve(operation).unwrap();
        if state != RemoteOperationState::Reserved {
            coordinator
                .mark_dispatched(&operation.operation_id)
                .unwrap();
        }
        if state == RemoteOperationState::Indeterminate {
            coordinator
                .mark_indeterminate(&operation.operation_id)
                .unwrap();
        }
    }

    /// No-replay rests on identity, not on blocking: an operation whose outcome
    /// is still open is refused if it is sent again, and its record is kept.
    #[test_case(RemoteOperationState::Reserved; "reserved")]
    #[test_case(RemoteOperationState::Dispatched; "dispatched")]
    #[test_case(RemoteOperationState::Indeterminate; "indeterminate")]
    fn an_open_operation_is_never_reserved_twice(state: RemoteOperationState) {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let binding = StoredWorkspaceBinding::local_from_cwd("opaque-workspace");
        let journal = RemoteOperationJournal::open(&state_dir).unwrap();
        let coordinator = RemoteMutationJournal::new(journal, binding).unwrap();
        let operation = journal_operation("replayed", WRITE_KIND);
        reserve_in_state(&coordinator, &operation, state);

        assert_eq!(
            coordinator.reserve(&operation),
            Err(WorkspaceError::PendingOperation {
                operation_id: operation.operation_id.as_str().to_owned(),
            })
        );
        let states = coordinator
            .pending()
            .into_iter()
            .map(|pending| (pending.operation_id, pending.state))
            .collect::<Vec<_>>();
        assert_eq!(states, vec![(operation.operation_id, state)]);
    }

    fn binding_in_generation(generation: &str) -> StoredWorkspaceBinding {
        let authority = AuthorityIdentity::new(
            SourceTrustAnchor::new("source").unwrap(),
            "server",
            "workspace",
            generation,
            "namespace",
        )
        .unwrap();
        let principal = AuthenticatedPrincipalId::new(authority.clone(), "principal").unwrap();
        let project = ProjectIdentity::new(authority.clone(), ProjectKey::new("project").unwrap());
        let session = SessionWorkspaceBinding::new(
            SessionBindingId::new("session").unwrap(),
            authority,
            principal,
            project,
        )
        .unwrap();
        StoredWorkspaceBinding::new(session, CwdHandle::new("cwd").unwrap(), None).unwrap()
    }

    /// The host that ran an operation of an earlier workspace generation is
    /// gone, so that operation can never be reconciled. It must not stop the
    /// client from starting or hold anything back; recovery leaves it alone,
    /// and only an explicit acknowledgement clears it.
    #[test_case(RemoteOperationState::Reserved; "reserved")]
    #[test_case(RemoteOperationState::Dispatched; "dispatched")]
    #[test_case(RemoteOperationState::Indeterminate; "indeterminate")]
    fn an_earlier_generation_operation_is_listed_skipped_by_recovery_and_acknowledgeable(
        state: RemoteOperationState,
    ) {
        const PREVIOUS_GENERATION: &str = "previous-generation";
        const CURRENT_GENERATION: &str = "current-generation";
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let earlier = journal_operation("earlier", WRITE_KIND);
        {
            let previous = RemoteMutationJournal::new(
                RemoteOperationJournal::open(&state_dir).unwrap(),
                binding_in_generation(PREVIOUS_GENERATION),
            )
            .unwrap();
            reserve_in_state(&previous, &earlier, state);
        }
        let current = RemoteMutationJournal::new(
            RemoteOperationJournal::open(&state_dir).unwrap(),
            binding_in_generation(CURRENT_GENERATION),
        )
        .unwrap();
        let later = journal_operation("later", SHELL_KIND);

        current.reserve(&later).unwrap();

        assert_eq!(
            current.pending(),
            vec![
                PendingRemoteOperation {
                    operation_id: earlier.operation_id.clone(),
                    operation_kind: WRITE_KIND.to_owned(),
                    state,
                    reachable: false,
                },
                PendingRemoteOperation {
                    operation_id: later.operation_id.clone(),
                    operation_kind: SHELL_KIND.to_owned(),
                    state: RemoteOperationState::Reserved,
                    reachable: true,
                },
            ]
        );
        let recoverable = current
            .recovery_operations()
            .unwrap()
            .into_iter()
            .map(|operation| operation.operation_id)
            .collect::<Vec<_>>();
        assert_eq!(recoverable, vec![later.operation_id.clone()]);
        current.acknowledge(&earlier.operation_id).unwrap();
        let remaining = current
            .pending()
            .into_iter()
            .map(|pending| pending.operation_id)
            .collect::<Vec<_>>();
        assert_eq!(remaining, vec![later.operation_id]);
    }

    /// Cancellation or an expired deadline before dispatch ends the call
    /// without contacting the host or leaving anything to reconcile, not even
    /// to renew a preparation that has lapsed.
    #[test_case(true, u64::MAX, CANCELLED; "cancelled")]
    #[test_case(false, u64::MAX, REMOTE_DEADLINE_BEFORE_DISPATCH; "deadline_expired")]
    #[test_case(true, LAPSED_EXPIRY_UNIX_MS, CANCELLED; "cancelled_after_lapse")]
    #[test_case(false, LAPSED_EXPIRY_UNIX_MS, REMOTE_DEADLINE_BEFORE_DISPATCH; "deadline_expired_after_lapse")]
    fn a_call_stopped_before_dispatch_never_reaches_the_host(
        cancelled: bool,
        expires_at_unix_ms: u64,
        expected: &str,
    ) {
        const CAPACITY: usize = 8;
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let seed = StoredWorkspaceBinding::local_from_cwd("opaque-workspace");
        let cursor = WorkspaceCursor::new(
            seed.binding(),
            ResourceScope::root(ResourceId::new("root").unwrap()),
            0,
            CwdHandle::new("cwd").unwrap(),
        );
        let stored_binding = seed.with_cursor(cursor.clone()).unwrap();
        let binding = stored_binding.binding().clone();
        let journal = RemoteOperationJournal::open(&state_dir).unwrap();
        let coordinator = RemoteMutationJournal::new(journal, stored_binding.clone()).unwrap();
        let stopped = journal_operation("stopped", WRITE_KIND);
        let operation = OperationHandle {
            preparation_id: stopped.preparation_id.clone(),
            invocation_id: Some(stopped.invocation_id.clone()),
            execution_id: None,
            expires_at_unix_ms: Some(expires_at_unix_ms),
        };
        let mut operations = OperationRegistry::new(CAPACITY);
        operations
            .insert(
                operation.preparation_id.clone(),
                StoredOperation {
                    transfer: None,
                    binding: binding_for("test.v1"),
                    context: None,
                    invocation_id: stopped.invocation_id.clone(),
                    expires_at_unix_ms,
                    journal: Some(stopped.clone()),
                    persisted: false,
                },
            )
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint =
            WorkcellEndpoint::parse(&format!("http://{}/mcp", listener.local_addr().unwrap()))
                .unwrap();
        let (transport, events) = RemoteTransport::new(&endpoint, None).unwrap();
        let descriptor = descriptor("instance", "generation");
        let client = RemoteWorkcellClient(Arc::new(RemoteInner {
            staging: PrivateStaging::new(super::transfer::negotiated_limits(
                descriptor.capabilities.reviewed_transfer.as_ref(),
            )),
            transport,
            capabilities: workspace_capabilities(&descriptor.capabilities),
            descriptor,
            host_binding: Mutex::new(host_binding()),
            authority: binding.authority().clone(),
            project: binding.project().clone(),
            session_binding: binding.clone(),
            stored_binding: stored_binding.clone(),
            root_cursor: cursor.clone(),
            manifest: freeze_catalog(tool_list("test")).unwrap(),
            catalog: HashMap::new(),
            events,
            cancellation: CancellationToken::new(),
            paths: Mutex::new(ResourceCache::new(CAPACITY, Duration::MAX)),
            repositories: Mutex::new(BoundedMap::new(CAPACITY, Duration::MAX)),
            cursors: Mutex::new(CursorRegistry {
                records: HashMap::from([(
                    cursor.cwd_handle().clone(),
                    CursorRecord {
                        cursor: cursor.clone(),
                        path: WorkspacePath::root(),
                    },
                )]),
                limit: CAPACITY,
            }),
            watches: Mutex::new(WatchRegistry::new(CAPACITY, Duration::MAX)),
            operations: Mutex::new(operations),
            captures: super::snapshot::CaptureRegistry::default(),
            operation_slots: super::Event::new(),
            mutation_journal: coordinator,
        }));
        let raw_input = json!({"filePath":"test", "content":"test"});
        let call = RemotePreparedToolCall {
            prepared: PreparedToolCall {
                operation,
                canonical_input: raw_input.clone(),
                review: Value::Null,
            },
            intent: serde_json::from_value(
                json!({"kind":"execute", "mutating":true, "resources":[]}),
            )
            .unwrap(),
            binding: binding.clone(),
            cursor: cursor.clone(),
        };
        let (trigger, cancel) = CancelToken::new();
        let mut ctx = crate::tests::context(temp.path(), Arc::new(ToolRegistry::new()), cancel);
        ctx.workspace_session = Some(
            WorkspaceSession::new(client.workspace_handle().unwrap(), binding, cursor).unwrap(),
        );
        if cancelled {
            trigger.cancel();
        } else {
            ctx.deadline = Deadline::after(Duration::ZERO);
        }
        let invocation = RemoteWorkcellInvocation {
            client: client.clone(),
            kind: ToolKind::FileWrite,
            input: Input::parse(ToolKind::FileWrite, raw_input.clone()).unwrap(),
            raw_input,
            prepared: AsyncMutex::new(RemotePreparedState::default()),
        };
        let mut cleanup = RemoteExecutionCleanup {
            client: client.clone(),
            call: None,
            execution_started: false,
        };

        let result = smol::block_on(invocation.execute_remote(&ctx, call, &mut cleanup));

        assert_eq!(result.output.unwrap_err(), expected);
        assert!(!cleanup.execution_started);
        assert!(client.pending_remote_operations().is_empty());
        let journal = RemoteOperationJournal::open(&state_dir).unwrap();
        assert!(journal.list_pending(&stored_binding).unwrap().is_empty());
        listener.set_nonblocking(true).unwrap();
        assert!(listener.accept().is_err());
    }

    pub(super) struct ScriptedHost {
        pub(super) endpoint: WorkcellEndpoint,
        calls: Arc<Mutex<Vec<(String, Value)>>>,
        stop: Arc<AtomicBool>,
        server: Option<thread::JoinHandle<()>>,
    }

    impl Drop for ScriptedHost {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(server) = self.server.take() {
                server.join().unwrap();
            }
        }
    }

    impl ScriptedHost {
        fn new(respond: impl Fn(&str, &Value) -> Value + Send + 'static) -> Self {
            Self::rpc(move |method, params| Ok(respond(method, params)))
        }

        fn rpc(respond: impl Fn(&str, &Value) -> Result<Value, Value> + Send + 'static) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let endpoint =
                WorkcellEndpoint::parse(&format!("http://{}/mcp", listener.local_addr().unwrap()))
                    .unwrap();
            listener.set_nonblocking(true).unwrap();
            let calls = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let recorded = calls.clone();
            let halt = stop.clone();
            let server = thread::spawn(move || {
                while !halt.load(Ordering::Acquire) {
                    let Ok((mut stream, _)) = listener.accept() else {
                        thread::yield_now();
                        continue;
                    };
                    stream.set_nonblocking(false).unwrap();
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut length = 0;
                    loop {
                        let mut line = String::new();
                        reader.read_line(&mut line).unwrap();
                        if line == "\r\n" {
                            break;
                        }
                        if let Some(value) =
                            line.to_ascii_lowercase().strip_prefix("content-length: ")
                        {
                            length = value.trim().parse().unwrap();
                        }
                    }
                    let mut body = vec![0; length];
                    reader.read_exact(&mut body).unwrap();
                    let request: Value = serde_json::from_slice(&body).unwrap();
                    let method = request["method"].as_str().unwrap().to_owned();
                    let params = request["params"].clone();
                    let result = respond(&method, &params);
                    recorded.lock().unwrap().push((method, params));
                    let body = match result {
                        Ok(result) => json!({"jsonrpc":"2.0","id":request["id"],"result":result}),
                        Err(error) => json!({"jsonrpc":"2.0","id":request["id"],"error":error}),
                    }
                    .to_string();
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                }
            });
            Self {
                endpoint,
                calls,
                stop,
                server: Some(server),
            }
        }

        fn methods(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|(method, _)| method.clone())
                .collect()
        }

        pub(super) fn params_for(&self, method: &str) -> Vec<Value> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(recorded, _)| recorded == method)
                .map(|(_, params)| params.clone())
                .collect()
        }
    }

    fn scripted_descriptor() -> contract::RemoteHostDescriptor {
        const BYTES: u64 = 64 * 1024;
        const COUNT: u32 = 64;
        let mut descriptor = descriptor("instance", "generation");
        descriptor
            .capabilities
            .tool_execution
            .limits
            .max_request_bytes = BYTES;
        descriptor
            .capabilities
            .tool_catalog
            .limits
            .max_request_bytes = BYTES;
        let operations = descriptor.capabilities.operations.as_mut().unwrap();
        operations.limits.max_argument_bytes = BYTES;
        operations.limits.max_preparations = COUNT;
        operations.limits.max_operations = COUNT;
        operations.limits.max_ledger_bytes = BYTES;
        operations.limits.max_resource_intents = COUNT;
        operations.limits.max_progress_events = COUNT;
        operations.limits.max_progress_bytes = BYTES;
        descriptor
    }

    fn watch_host() -> ScriptedHost {
        watch_host_with_response(|_, response| response)
    }

    struct MutationHost {
        host: ScriptedHost,
        files: Arc<Mutex<HashMap<String, String>>>,
    }

    fn mutation_revision(content: &str) -> ResourceRevision {
        ResourceRevision::new(
            CatalogRevision::for_serializable(&content)
                .unwrap()
                .as_str(),
        )
        .unwrap()
    }

    fn mutation_host(result_patch: Value) -> MutationHost {
        let files = Arc::new(Mutex::new(HashMap::from([
            (MUTATION_TARGET.to_owned(), MUTATION_ORIGINAL.to_owned()),
            (
                MUTATION_SHADOW.to_owned(),
                MUTATION_SHADOW_CONTENT.to_owned(),
            ),
            (
                MUTATION_SHADOW_DESTINATION.to_owned(),
                MUTATION_SHADOW_CONTENT.to_owned(),
            ),
        ])));
        let host_files = files.clone();
        let prepared = Mutex::new(None::<(Value, Value)>);
        let host = ScriptedHost::rpc(move |method, params| match method {
            contract::PREPARE_MUTATION_METHOD => {
                assert_eq!(params["cwdHandle"], MUTATION_CWD);
                let mutation = params["mutations"][0].clone();
                let kind = mutation["kind"].as_str().unwrap();
                assert_eq!(
                    mutation
                        .get("path")
                        .or_else(|| mutation.get("from"))
                        .unwrap(),
                    "a.rs"
                );
                if kind == "rename" {
                    assert_eq!(mutation["to"], "b.rs");
                }
                let files = host_files.lock().unwrap();
                let current = files.get(MUTATION_TARGET);
                if (matches!(kind, "create" | "mkdir") && current.is_some())
                    || mutation.get("expectedRevision").is_some_and(|expected| {
                        current
                            .is_none_or(|content| expected != mutation_revision(content).as_str())
                    })
                {
                    return Err(snapshot_refusal(super::STALE_RESOURCE_CODE));
                }
                let response = json!({
                    "version":"v1", "preparationId":MUTATION_PREPARATION,
                    "expiresAtUnixMs":u64::MAX,
                    "binding":{"host":params["host"], "argumentDigest":TEST_REQUEST_DIGEST,
                        "contract":{"id":contract::WORKSPACE_MUTATION_CONTRACT_ID,"version":"v1","resultVersion":"v1"}},
                    "intent":{"kind":"mutate","mutating":true,"resources":[{
                        "display":MUTATION_TARGET,"access":"write","resourceId":"resource",
                        "scope":["resource"],"revision":null
                    }]}
                });
                *prepared.lock().unwrap() = Some((response.clone(), mutation));
                Ok(response)
            }
            contract::EXECUTE_METHOD => {
                let prepared = prepared.lock().unwrap();
                let (prepared, mutation) = prepared.as_ref().unwrap();
                let kind = mutation["kind"].as_str().unwrap();
                let mut files = host_files.lock().unwrap();
                let destination = match kind {
                    "create" | "write" => {
                        files.insert(
                            MUTATION_TARGET.into(),
                            mutation["content"].as_str().unwrap().into(),
                        );
                        None
                    }
                    "mkdir" => {
                        files.insert(MUTATION_TARGET.into(), String::new());
                        None
                    }
                    "delete" => {
                        files.remove(MUTATION_TARGET).unwrap();
                        None
                    }
                    "rename" => {
                        let content = files.remove(MUTATION_TARGET).unwrap();
                        files.insert(MUTATION_DESTINATION.into(), content);
                        Some(MUTATION_DESTINATION)
                    }
                    _ => panic!("unexpected mutation kind {kind}"),
                };
                let revision = files
                    .get(destination.unwrap_or(MUTATION_TARGET))
                    .map(|content| mutation_revision(content));
                let mut entry = json!({"kind":kind,"path":MUTATION_TARGET,"destination":destination,"revision":revision});
                entry
                    .as_object_mut()
                    .unwrap()
                    .extend(result_patch.as_object().unwrap().clone());
                let mut status = serde_json::to_value(completed_status()).unwrap();
                status["preparationId"] = params["preparationId"].clone();
                status["invocationId"] = params["invocationId"].clone();
                status["binding"] = prepared["binding"].clone();
                status["expiresAtUnixMs"] = prepared["expiresAtUnixMs"].clone();
                status["outcome"]["result"]["structuredContent"] = json!({
                    "version":"v1","committed":true,"rolledBack":false,"atomicAcrossFiles":false,"results":[entry]
                });
                Ok(status)
            }
            contract::RELEASE_METHOD => {
                Ok(json!({"version":"v1","state":"forgotten","released":true}))
            }
            _ => panic!("unexpected mutation method {method}"),
        });
        MutationHost { host, files }
    }

    #[test_case(false; "normal_release")]
    #[test_case(true; "retry_deferred_release")]
    fn completed_directory_publications_retain_exact_cleanup_until_release(defer: bool) {
        use caudra_workspace::{DirectoryPublicationRequest, WorkspaceTransferService};
        use workcell::CatalogRevision;

        const PUBLICATIONS: usize = 16;
        let prepared = Mutex::new(None::<(Value, Value, bool)>);
        let host = ScriptedHost::rpc(move |method, params| match method {
            contract::TRANSFER_DIRECTORY_PREPARE_METHOD => {
                let mut request = params.clone();
                request.as_object_mut().unwrap().remove("_meta");
                let wire: contract::TransferDirectoryPrepareRequest =
                    serde_json::from_value(request).unwrap();
                let digest = CatalogRevision::for_serializable(&wire).unwrap();
                let operation = json!({
                    "version":"v1", "preparationId":wire.publication_id, "expiresAtUnixMs":u64::MAX,
                    "binding":{"host":params["host"], "argumentDigest":digest.as_str(),
                        "contract":{"id":contract::TRANSFER_DIRECTORY_PUBLICATION_CONTRACT_ID,"version":"v1","resultVersion":"v1"}},
                    "intent":{"kind":"transfer","mutating":true,"resources":[{"display":wire.path,"access":"write","resourceId":"directory","scope":["directory"],"revision":null}]}
                });
                *prepared.lock().unwrap() = Some((operation.clone(), params.clone(), defer));
                Ok(
                    json!({"version":"v1", "publicationId":wire.publication_id, "operation":operation}),
                )
            }
            contract::EXECUTE_METHOD => {
                let prepared = prepared.lock().unwrap();
                let (operation, request, _) = prepared.as_ref().unwrap();
                let mut status = serde_json::to_value(completed_status()).unwrap();
                status["preparationId"] = params["preparationId"].clone();
                status["invocationId"] = params["invocationId"].clone();
                status["binding"] = operation["binding"].clone();
                status["expiresAtUnixMs"] = operation["expiresAtUnixMs"].clone();
                status["outcome"]["result"]["structuredContent"] = json!({
                    "version":"v1", "publicationId":request["publicationId"], "state":"completed",
                    "preparationId":params["preparationId"], "invocationId":params["invocationId"],
                    "requestDigest":operation["binding"]["argumentDigest"],
                    "directory":{"path":request["path"],"resourceId":"directory","createdDirectories":[]}
                });
                Ok(status)
            }
            contract::RELEASE_METHOD => {
                let mut prepared = prepared.lock().unwrap();
                let (_, _, deferred) = prepared.as_mut().unwrap();
                let released = !*deferred;
                *deferred = false;
                Ok(json!({"version":"v1","state":"completed","released":released}))
            }
            _ => panic!("unexpected directory method {method}"),
        });
        let temp = tempfile::tempdir().unwrap();
        let mut client = snapshot_client(
            &host.endpoint,
            &StateDir::from_path(temp.path().join("state")),
        );
        let capabilities = &mut Arc::get_mut(&mut client.0).unwrap().descriptor.capabilities;
        capabilities
            .reviewed_transfer
            .as_mut()
            .unwrap()
            .directory_publication = true;
        capabilities
            .workspace
            .as_mut()
            .unwrap()
            .limits
            .max_path_bytes = contract::MAX_WORKSPACE_PATH_BYTES as u32;
        smol::block_on(async {
            for index in 0..PUBLICATIONS {
                let request = DirectoryPublicationRequest {
                    publication_id: OperationId::new(format!("directory-{index}")).unwrap(),
                    path: WorkspacePath::new(format!("directory-{index}")).unwrap(),
                    create_directories: Vec::new(),
                };
                let prepared = client
                    .prepare_directory(client.session_binding(), client.root_cursor(), &request)
                    .await
                    .unwrap();
                let result = client.execute_directory(&prepared).await.unwrap();
                assert!(matches!(result.state, OperationState::Completed { .. }));
                assert!(client.pending_remote_operations().is_empty());
                assert!(
                    client
                        .0
                        .operations
                        .lock()
                        .unwrap()
                        .entries
                        .contains_key(&prepared.operation.preparation_id)
                );
                let release = client.release_directory(&prepared).await.unwrap();
                assert_eq!(release.released, !defer);
                if defer {
                    assert!(client.release_directory(&prepared).await.unwrap().released);
                }
                assert!(client.0.operations.lock().unwrap().entries.is_empty());
            }
        });
    }

    fn nonroot_mutation_client(host: &MutationHost, state: &StateDir) -> RemoteWorkcellClient {
        let mut client = snapshot_client(&host.host.endpoint, state);
        let inner = Arc::get_mut(&mut client.0).unwrap();
        inner
            .descriptor
            .capabilities
            .workspace
            .as_mut()
            .unwrap()
            .limits
            .max_path_bytes = contract::MAX_WORKSPACE_PATH_BYTES as u32;
        inner
            .descriptor
            .capabilities
            .workspace_mutation
            .as_mut()
            .unwrap()
            .max_content_bytes = contract::MAX_ARGUMENT_BYTES as u64;
        let cursor = WorkspaceCursor::new(
            &inner.session_binding,
            inner.root_cursor.scope().clone(),
            inner.root_cursor.generation(),
            CwdHandle::new(MUTATION_CWD).unwrap(),
        );
        inner
            .cursors
            .lock()
            .unwrap()
            .insert(
                cursor.cwd_handle().clone(),
                CursorRecord {
                    cursor: cursor.clone(),
                    path: WorkspacePath::new(MUTATION_CWD).unwrap(),
                },
            )
            .unwrap();
        inner.root_cursor = cursor;
        for (index, path) in [
            MUTATION_TARGET,
            MUTATION_DESTINATION,
            MUTATION_SHADOW,
            MUTATION_SHADOW_DESTINATION,
        ]
        .into_iter()
        .enumerate()
        {
            inner.paths.lock().unwrap().insert(
                ResourceId::new(format!("resource-{index}")).unwrap(),
                WorkspacePath::new(path).unwrap(),
            );
        }
        client
    }

    fn mutation_request(kind: MutationKind, revision: ResourceRevision) -> MutationRequest {
        let path = WorkspacePath::new("a.rs").unwrap();
        MutationRequest {
            mutations: vec![match kind {
                MutationKind::Create | MutationKind::Write => Mutation::Write {
                    path,
                    content: WriteContent::Text(MUTATION_UPDATED.into()),
                    condition: if kind == MutationKind::Create {
                        MutationCondition::MustNotExist
                    } else {
                        MutationCondition::Matches(revision)
                    },
                },
                MutationKind::CreateDirectory => Mutation::CreateDirectory { path },
                MutationKind::Move => Mutation::Move {
                    source: path,
                    destination: WorkspacePath::new("b.rs").unwrap(),
                    expected_revision: revision,
                },
                MutationKind::Remove => Mutation::Remove {
                    path,
                    expected_revision: revision,
                },
            }],
        }
    }

    #[test_case(MutationKind::Write; "write")]
    #[test_case(MutationKind::Create; "create")]
    #[test_case(MutationKind::CreateDirectory; "mkdir")]
    #[test_case(MutationKind::Move; "rename")]
    #[test_case(MutationKind::Remove; "delete")]
    fn nonroot_mutations_validate_project_paths_without_redispatch(kind: MutationKind) {
        let host = mutation_host(json!({}));
        let temp = tempfile::tempdir().unwrap();
        let client =
            nonroot_mutation_client(&host, &StateDir::from_path(temp.path().join("state")));
        if matches!(kind, MutationKind::Create | MutationKind::CreateDirectory) {
            host.files.lock().unwrap().remove(MUTATION_TARGET);
        } else {
            let stale = mutation_request(kind, mutation_revision(MUTATION_UPDATED));
            assert_eq!(
                smol::block_on(client.execute(
                    client.session_binding(),
                    client.root_cursor(),
                    &stale
                )),
                Err(WorkspaceError::StaleResource {
                    resource_id: ResourceId::new(client.host_binding().cwd_handle.as_str())
                        .unwrap(),
                })
            );
            assert_eq!(
                host.files.lock().unwrap().get(MUTATION_TARGET).unwrap(),
                MUTATION_ORIGINAL
            );
            assert!(host.host.params_for(contract::EXECUTE_METHOD).is_empty());
        }
        let request = mutation_request(kind, mutation_revision(MUTATION_ORIGINAL));
        let status = smol::block_on(client.execute(
            client.session_binding(),
            client.root_cursor(),
            &request,
        ))
        .unwrap();
        let OperationState::Completed { result, .. } = status.state else {
            panic!("mutation did not complete")
        };
        assert!(result.committed);
        assert_eq!(result.results.len(), 1);
        assert_eq!(result.results[0].kind, kind);
        assert_eq!(result.results[0].path.as_str(), MUTATION_TARGET);
        assert_eq!(
            result.results[0]
                .destination
                .as_ref()
                .map(|path| path.as_str()),
            (kind == MutationKind::Move).then_some(MUTATION_DESTINATION)
        );
        let files = host.files.lock().unwrap();
        match kind {
            MutationKind::Create | MutationKind::Write => {
                assert_eq!(files.get(MUTATION_TARGET).unwrap(), MUTATION_UPDATED)
            }
            MutationKind::CreateDirectory => {
                assert!(files.get(MUTATION_TARGET).unwrap().is_empty())
            }
            MutationKind::Move => {
                assert!(!files.contains_key(MUTATION_TARGET));
                assert_eq!(files.get(MUTATION_DESTINATION).unwrap(), MUTATION_ORIGINAL);
            }
            MutationKind::Remove => assert!(!files.contains_key(MUTATION_TARGET)),
        }
        for shadow in [MUTATION_SHADOW, MUTATION_SHADOW_DESTINATION] {
            assert_eq!(files.get(shadow).unwrap(), MUTATION_SHADOW_CONTENT);
            assert!(
                client
                    .0
                    .paths
                    .lock()
                    .unwrap()
                    .id_for_path(&WorkspacePath::new(shadow).unwrap())
                    .is_some()
            );
        }
        assert!(
            client
                .0
                .paths
                .lock()
                .unwrap()
                .id_for_path(&WorkspacePath::new(MUTATION_TARGET).unwrap())
                .is_none()
        );
        assert_eq!(
            client
                .0
                .paths
                .lock()
                .unwrap()
                .id_for_path(&WorkspacePath::new(MUTATION_DESTINATION).unwrap())
                .is_none(),
            kind == MutationKind::Move
        );
        assert_eq!(host.host.params_for(contract::EXECUTE_METHOD).len(), 1);
        assert_eq!(host.host.params_for(contract::RELEASE_METHOD).len(), 1);
    }

    #[test_case(MutationKind::Write, json!({"path":"a.rs"}); "cursor_relative_response_is_invalid")]
    #[test_case(MutationKind::Write, json!({"path":"sub/sub/a.rs"}); "double_prefixed_response_is_invalid")]
    #[test_case(MutationKind::Move, json!({"destination":"sub/sub/b.rs"}); "wrong_rename_destination")]
    #[test_case(MutationKind::Write, json!({"kind":"delete"}); "wrong_mutation_kind")]
    fn nonroot_mutations_reject_bad_outcomes_without_reexecution(kind: MutationKind, patch: Value) {
        let host = mutation_host(patch);
        let temp = tempfile::tempdir().unwrap();
        let client =
            nonroot_mutation_client(&host, &StateDir::from_path(temp.path().join("state")));
        let request = mutation_request(kind, mutation_revision(MUTATION_ORIGINAL));
        assert_eq!(
            smol::block_on(client.execute(
                client.session_binding(),
                client.root_cursor(),
                &request
            )),
            Err(super::invalid_response())
        );
        assert_eq!(host.host.params_for(contract::EXECUTE_METHOD).len(), 1);
        assert!(host.host.params_for(contract::STATUS_METHOD).is_empty());
        assert!(host.host.params_for(contract::RELEASE_METHOD).is_empty());
        assert_eq!(
            host.files.lock().unwrap().get(MUTATION_SHADOW).unwrap(),
            MUTATION_SHADOW_CONTENT
        );
    }

    fn watch_host_with_response(
        respond: impl Fn(&str, Value) -> Value + Send + 'static,
    ) -> ScriptedHost {
        let next_id = AtomicU64::new(0);
        ScriptedHost::new(move |method, params| {
            let response = match method {
                contract::WATCH_OPEN_METHOD => json!({
                    "version":"v1", "subscriptionId":format!("watch-{}", next_id.fetch_add(1, Ordering::Relaxed)),
                    "state":"current", "cursor":"a", "expiresAtUnixMs":u64::MAX
                }),
                contract::WATCH_POLL_METHOD => json!({
                    "version":"v1", "subscriptionId":params["subscriptionId"],
                    "state":"current", "resyncReason":null, "firstRetainedSequence":null,
                    "nextSequence":crate::remote::WATCH_INITIAL_SEQUENCE, "events":[],
                    "nextCursor":"b", "expiresAtUnixMs":u64::MAX
                }),
                contract::WATCH_CLOSE_METHOD => json!({
                    "version":"v1", "subscriptionId":params["subscriptionId"], "closed":true
                }),
                _ => panic!("unexpected watch method {method}"),
            };
            respond(method, response)
        })
    }

    fn open_watch(client: &RemoteWorkcellClient) -> WatchSubscription {
        smol::block_on(client.open(
            client.session_binding(),
            client.root_cursor(),
            &WatchOpenRequest {
                root: ResourceSelector::Current,
                recursive: false,
            },
        ))
        .unwrap()
    }

    fn watch_poll_request(subscription: &WatchSubscription) -> WatchPollRequest {
        WatchPollRequest {
            subscription_id: subscription.subscription_id.clone(),
            cursor: subscription.cursor.clone(),
            max_events: 1,
            max_bytes: 1,
            wait_ms: 0,
        }
    }

    fn expire_watch(client: &RemoteWorkcellClient, id: &WatchSubscriptionId) {
        let mut watches = client.0.watches.lock().unwrap();
        watches.ttl = super::DEFAULT_CACHE_TTL;
        watches.records.get_mut(id).unwrap().touched = Instant::now() - watches.ttl * 2;
    }

    #[test_case((256, 65_536, 30_000), (128, 4096, 1000), (128, 4096, 1000); "negotiated_host_ceiling")]
    #[test_case((1, 1, 0), (128, 4096, 1000), (1, 1, 0); "smaller_nonblocking_request")]
    #[test_case((u32::MAX, u32::MAX, u64::MAX), (1, 1, 1), (1, 1, 1); "maximum_request")]
    fn watch_poll_clamps_to_negotiated_limits(
        requested: (u32, u32, u64),
        limits: (u32, u32, u64),
        expected: (u32, u32, u64),
    ) {
        let host = watch_host();
        let temp = tempfile::tempdir().unwrap();
        let mut client = snapshot_client(
            &host.endpoint,
            &StateDir::from_path(temp.path().join("state")),
        );
        let advertised = &mut Arc::get_mut(&mut client.0)
            .unwrap()
            .descriptor
            .capabilities
            .watch
            .as_mut()
            .unwrap()
            .limits;
        advertised.max_poll_events = limits.0;
        advertised.max_poll_bytes = limits.1;
        advertised.max_wait_ms = limits.2;
        let subscription = open_watch(&client);
        let mut request = watch_poll_request(&subscription);
        request.max_events = requested.0;
        request.max_bytes = requested.1;
        request.wait_ms = requested.2;
        let page =
            smol::block_on(client.poll(client.session_binding(), client.root_cursor(), &request))
                .unwrap();
        assert!(matches!(page.state, WatchPollState::Current { .. }));
        let calls = host.params_for(contract::WATCH_POLL_METHOD);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["maxEvents"], expected.0);
        assert_eq!(calls[0]["maxBytes"], expected.1);
        assert_eq!(calls[0]["waitMs"], expected.2);
    }

    #[test_case(0, 1; "zero_events")]
    #[test_case(1, 0; "zero_bytes")]
    fn watch_poll_rejects_zero_before_dispatch(max_events: u32, max_bytes: u32) {
        let host = watch_host();
        let temp = tempfile::tempdir().unwrap();
        let client = snapshot_client(
            &host.endpoint,
            &StateDir::from_path(temp.path().join("state")),
        );
        let subscription = open_watch(&client);
        let mut request = watch_poll_request(&subscription);
        request.max_events = max_events;
        request.max_bytes = max_bytes;
        request.wait_ms = u64::MAX;
        assert_eq!(
            smol::block_on(client.poll(client.session_binding(), client.root_cursor(), &request)),
            Err(super::invalid_response())
        );
        assert!(host.params_for(contract::WATCH_POLL_METHOD).is_empty());
    }

    #[test_case(json!({"events":[{"sequence":1,"kind":"modify","path":"f"},{"sequence":2,"kind":"modify","path":"f"}],"nextSequence":3}), 1024, crate::remote::invalid_response(); "too_many_events_for_clamped_limit")]
    #[test_case(json!({"events":[{"sequence":1,"kind":"modify","path":"f"}],"nextSequence":2}), 1, crate::remote::invalid_response(); "too_many_bytes_for_clamped_limit")]
    #[test_case(json!({"subscriptionId":"wrong"}), 1024, WorkspaceError::IdentityMismatch; "wrong_subscription")]
    #[test_case(json!({"nextCursor":null}), 1024, crate::remote::invalid_response(); "malformed_current_state")]
    fn clamped_watch_poll_still_rejects_bad_responses(
        patch: Value,
        max_bytes: u32,
        expected: WorkspaceError,
    ) {
        let host = watch_host_with_response(move |method, mut response| {
            if method == contract::WATCH_POLL_METHOD {
                response
                    .as_object_mut()
                    .unwrap()
                    .extend(patch.as_object().unwrap().clone());
            }
            response
        });
        let temp = tempfile::tempdir().unwrap();
        let mut client = snapshot_client(
            &host.endpoint,
            &StateDir::from_path(temp.path().join("state")),
        );
        Arc::get_mut(&mut client.0)
            .unwrap()
            .descriptor
            .capabilities
            .watch
            .as_mut()
            .unwrap()
            .limits
            .max_poll_bytes = max_bytes;
        let subscription = open_watch(&client);
        let mut request = watch_poll_request(&subscription);
        request.max_events = u32::MAX;
        request.max_bytes = u32::MAX;
        request.wait_ms = u64::MAX;
        assert_eq!(
            smol::block_on(client.poll(client.session_binding(), client.root_cursor(), &request)),
            Err(expected)
        );
        assert_eq!(
            client
                .0
                .watches
                .lock()
                .unwrap()
                .records
                .get(&subscription.subscription_id)
                .unwrap()
                .value
                .cursor,
            subscription.cursor
        );
        assert_eq!(host.params_for(contract::WATCH_POLL_METHOD).len(), 1);
    }

    #[test_case(false; "other_watch_is_current")]
    #[test_case(true; "other_watch_is_also_expired")]
    fn local_watch_expiry_resyncs_only_the_matching_subscription(other_expired: bool) {
        let host = watch_host();
        let temp = tempfile::tempdir().unwrap();
        let client = snapshot_client(
            &host.endpoint,
            &StateDir::from_path(temp.path().join("state")),
        );
        let expired = open_watch(&client);
        let other = open_watch(&client);
        expire_watch(&client, &expired.subscription_id);
        if other_expired {
            expire_watch(&client, &other.subscription_id);
        }
        let transient = open_watch(&client);
        smol::block_on(client.close(
            client.session_binding(),
            client.root_cursor(),
            &transient.subscription_id,
        ))
        .unwrap();
        let request = watch_poll_request(&expired);
        let page =
            smol::block_on(client.poll(client.session_binding(), client.root_cursor(), &request))
                .unwrap();
        assert_eq!(page.subscription_id, expired.subscription_id);
        assert_eq!(
            page.state,
            WatchPollState::FullResync {
                reason: WatchResyncReason::SubscriptionExpired
            }
        );
        assert!(page.events.is_empty());
        assert_eq!(page.sequence.first_retained_sequence, None);
        assert_eq!(page.sequence.next_sequence, super::WATCH_INITIAL_SEQUENCE);
        assert!(page.sequence.gap_before_first);
        assert!(
            client
                .0
                .watches
                .lock()
                .unwrap()
                .records
                .contains_key(&other.subscription_id)
        );
        assert_eq!(
            smol::block_on(client.poll(client.session_binding(), client.root_cursor(), &request)),
            Err(WorkspaceError::StaleCursor)
        );
        let reopened = open_watch(&client);
        let page = smol::block_on(client.poll(
            client.session_binding(),
            client.root_cursor(),
            &watch_poll_request(&reopened),
        ))
        .unwrap();
        assert!(matches!(page.state, WatchPollState::Current { .. }));
        let page = smol::block_on(client.poll(
            client.session_binding(),
            client.root_cursor(),
            &watch_poll_request(&other),
        ))
        .unwrap();
        assert_eq!(
            matches!(
                page.state,
                WatchPollState::FullResync {
                    reason: WatchResyncReason::SubscriptionExpired
                }
            ),
            other_expired
        );
        let calls = host.params_for(contract::WATCH_POLL_METHOD);
        assert!(
            calls
                .iter()
                .all(|params| params["subscriptionId"] != expired.subscription_id.as_str())
        );
        assert_eq!(calls.len(), if other_expired { 1 } else { 2 });
    }

    enum WatchMismatch {
        UnknownSubscription,
        WatchCursor,
        StaleWorkspaceCursor,
        OtherWorkspaceCursor,
        Binding,
    }

    #[test_case(WatchMismatch::UnknownSubscription, WorkspaceError::StaleCursor; "unknown_subscription")]
    #[test_case(WatchMismatch::WatchCursor, WorkspaceError::StaleCursor; "wrong_watch_cursor")]
    #[test_case(WatchMismatch::StaleWorkspaceCursor, WorkspaceError::StaleCursor; "stale_workspace_cursor")]
    #[test_case(WatchMismatch::OtherWorkspaceCursor, WorkspaceError::StaleCursor; "other_valid_workspace_cursor")]
    #[test_case(WatchMismatch::Binding, WorkspaceError::IdentityMismatch; "wrong_session_binding")]
    fn local_watch_expiry_never_masks_invalid_context(
        mismatch: WatchMismatch,
        expected: WorkspaceError,
    ) {
        let host = watch_host();
        let temp = tempfile::tempdir().unwrap();
        let client = snapshot_client(
            &host.endpoint,
            &StateDir::from_path(temp.path().join("state")),
        );
        let subscription = open_watch(&client);
        expire_watch(&client, &subscription.subscription_id);
        let mut request = watch_poll_request(&subscription);
        let mut binding = client.session_binding().clone();
        request.max_events = u32::MAX;
        request.max_bytes = u32::MAX;
        request.wait_ms = u64::MAX;
        let mut cursor = client.root_cursor().clone();
        match mismatch {
            WatchMismatch::UnknownSubscription => {
                request.subscription_id = WatchSubscriptionId::new("unknown").unwrap()
            }
            WatchMismatch::WatchCursor => request.cursor = WatchCursor::new("wrong").unwrap(),
            WatchMismatch::StaleWorkspaceCursor | WatchMismatch::OtherWorkspaceCursor => {
                cursor = WorkspaceCursor::new(
                    &binding,
                    cursor.scope().clone(),
                    cursor.generation(),
                    CwdHandle::new("other").unwrap(),
                );
                if matches!(mismatch, WatchMismatch::OtherWorkspaceCursor) {
                    client
                        .0
                        .cursors
                        .lock()
                        .unwrap()
                        .insert(
                            cursor.cwd_handle().clone(),
                            CursorRecord {
                                cursor: cursor.clone(),
                                path: WorkspacePath::root(),
                            },
                        )
                        .unwrap();
                }
            }
            WatchMismatch::Binding => {
                binding = StoredWorkspaceBinding::local_from_cwd("another-workspace")
                    .binding()
                    .clone()
            }
        }
        assert_eq!(
            smol::block_on(client.poll(&binding, &cursor, &request)),
            Err(expected)
        );
        assert!(
            client
                .0
                .watches
                .lock()
                .unwrap()
                .records
                .contains_key(&subscription.subscription_id)
        );
        let page = smol::block_on(client.poll(
            client.session_binding(),
            client.root_cursor(),
            &watch_poll_request(&subscription),
        ))
        .unwrap();
        assert_eq!(
            page.state,
            WatchPollState::FullResync {
                reason: WatchResyncReason::SubscriptionExpired
            }
        );
        assert!(host.params_for(contract::WATCH_POLL_METHOD).is_empty());
    }

    #[test_case(1; "single_slot")]
    #[test_case(2; "multiple_slots")]
    fn watch_registry_capacity_remains_bounded(limit: usize) {
        let host = watch_host();
        let temp = tempfile::tempdir().unwrap();
        let client = snapshot_client(
            &host.endpoint,
            &StateDir::from_path(temp.path().join("state")),
        );
        client.0.watches.lock().unwrap().limit = limit;
        let oldest = open_watch(&client);
        expire_watch(&client, &oldest.subscription_id);
        for _ in 0..limit {
            open_watch(&client);
        }
        assert_eq!(client.0.watches.lock().unwrap().records.len(), limit);
        assert_eq!(
            smol::block_on(client.poll(
                client.session_binding(),
                client.root_cursor(),
                &watch_poll_request(&oldest)
            )),
            Err(WorkspaceError::StaleCursor)
        );
        assert!(host.params_for(contract::WATCH_POLL_METHOD).is_empty());
    }

    #[test_case(None, "file"; "metadata_only_file")]
    #[test_case(None, "directory"; "metadata_only_directory")]
    #[test_case(Some(crate::remote::tests::TEST_REQUEST_DIGEST), "file"; "verified_file")]
    fn remembered_entries_preserve_optional_revisions(revision: Option<&str>, kind: &str) {
        let endpoint = WorkcellEndpoint::parse("http://127.0.0.1:1/mcp").unwrap();
        let temp = tempfile::tempdir().unwrap();
        let client = snapshot_client(&endpoint, &StateDir::from_path(temp.path().join("state")));
        let entry: contract::WorkspaceEntry = serde_json::from_value(json!({
            "path":"f", "resourceId":"resource", "revision":revision,
            "kind":kind, "sizeBytes":1
        }))
        .unwrap();
        let resource = client.remember_entry(&entry).unwrap();
        assert_eq!(
            resource.revision.as_ref().map(|revision| revision.as_str()),
            revision
        );
        assert_eq!(
            resource.path.as_ref().unwrap().as_str(),
            entry.path.as_str()
        );
        assert_eq!(
            resource.scope.resource_id().as_str(),
            entry.resource_id.as_str()
        );
        assert_eq!(resource.project, *client.session_binding().project());
        assert_eq!(resource.size_bytes, entry.size_bytes);
        assert_eq!(
            client
                .selector_path(
                    client.root_cursor(),
                    &ResourceSelector::Id(resource.scope.resource_id().clone())
                )
                .unwrap(),
            resource.path.unwrap()
        );
    }

    #[test_case(false, false, false, (false, false); "complete")]
    #[test_case(false, false, true, (true, false); "continuation")]
    #[test_case(false, true, false, (false, true); "incomplete")]
    #[test_case(false, true, true, (true, true); "incomplete_with_continuation")]
    #[test_case(true, false, false, (false, true); "bounded_scan")]
    #[test_case(true, false, true, (true, true); "bounded_scan_with_continuation")]
    #[test_case(true, true, false, (false, true); "bounded_incomplete_scan")]
    #[test_case(true, true, true, (true, true); "bounded_incomplete_scan_with_continuation")]
    fn metadata_list_pages_propagate_incompleteness(
        truncated: bool,
        incomplete: bool,
        has_cursor: bool,
        expected: (bool, bool),
    ) {
        let host = ScriptedHost::new(move |method, _| {
            assert_eq!(method, contract::LIST_METHOD);
            json!({
                "version":"v1", "revision":"collection",
                "entries":[{"path":"f", "resourceId":"resource", "revision":null,
                    "kind":"file", "sizeBytes":1}],
                "truncated":truncated, "incomplete":incomplete,
                "nextCursor":has_cursor.then_some("n")
            })
        });
        let temp = tempfile::tempdir().unwrap();
        let client = snapshot_client(
            &host.endpoint,
            &StateDir::from_path(temp.path().join("state")),
        );
        let page = smol::block_on(client.list(
            client.session_binding(),
            client.root_cursor(),
            &ListRequest {
                parent: ResourceSelector::Current,
                recursive: false,
                continuation: None,
                limit: 1,
            },
        ))
        .unwrap();
        assert_eq!((page.truncated, page.incomplete), expected);
        assert_eq!(page.continuation.is_some(), has_cursor);
        assert_eq!(page.revision.as_str(), "collection");
        assert_eq!(page.resources.len(), 1);
        let resource = &page.resources[0];
        assert_eq!(resource.revision, None);
        assert_eq!(resource.path.as_ref().unwrap().as_str(), "f");
        assert_eq!(resource.scope.resource_id().as_str(), "resource");
        assert_eq!(
            client
                .selector_path(
                    client.root_cursor(),
                    &ResourceSelector::Id(resource.scope.resource_id().clone())
                )
                .unwrap(),
            *resource.path.as_ref().unwrap()
        );
    }

    pub(super) fn snapshot_client(
        endpoint: &WorkcellEndpoint,
        state: &StateDir,
    ) -> RemoteWorkcellClient {
        const CAPACITY: usize = 8;
        let seed = StoredWorkspaceBinding::local_from_cwd("snapshot-workspace");
        let cursor = WorkspaceCursor::new(
            seed.binding(),
            ResourceScope::root(ResourceId::new("root").unwrap()),
            0,
            CwdHandle::new("cwd").unwrap(),
        );
        let stored = seed.with_cursor(cursor.clone()).unwrap();
        let binding = stored.binding().clone();
        let (transport, events) = RemoteTransport::new(endpoint, None).unwrap();
        let descriptor = scripted_descriptor();
        RemoteWorkcellClient(Arc::new(RemoteInner {
            staging: PrivateStaging::new(super::transfer::negotiated_limits(
                descriptor.capabilities.reviewed_transfer.as_ref(),
            )),
            transport,
            capabilities: workspace_capabilities(&descriptor.capabilities),
            descriptor,
            host_binding: Mutex::new(host_binding()),
            authority: binding.authority().clone(),
            project: binding.project().clone(),
            session_binding: binding,
            stored_binding: stored.clone(),
            root_cursor: cursor.clone(),
            manifest: shell_manifest(),
            catalog: HashMap::new(),
            events,
            cancellation: CancellationToken::new(),
            paths: Mutex::new(ResourceCache::new(CAPACITY, Duration::MAX)),
            repositories: Mutex::new(BoundedMap::new(CAPACITY, Duration::MAX)),
            cursors: Mutex::new(CursorRegistry {
                records: HashMap::from([(
                    cursor.cwd_handle().clone(),
                    CursorRecord {
                        cursor,
                        path: WorkspacePath::root(),
                    },
                )]),
                limit: CAPACITY,
            }),
            watches: Mutex::new(WatchRegistry::new(CAPACITY, Duration::MAX)),
            operations: Mutex::new(OperationRegistry::new(CAPACITY)),
            captures: super::snapshot::CaptureRegistry::default(),
            operation_slots: super::Event::new(),
            mutation_journal: RemoteMutationJournal::new(
                RemoteOperationJournal::open(state).unwrap(),
                stored,
            )
            .unwrap(),
        }))
    }

    pub(super) fn snapshot_request() -> SnapshotCaptureRequest {
        SnapshotCaptureRequest {
            checkpoint_id: CheckpointId::new(SNAPSHOT_CHECKPOINT).unwrap(),
            label: None,
            limits: SnapshotCaptureLimits {
                max_files: 1,
                max_file_bytes: 1,
                max_total_bytes: 1,
            },
        }
    }

    pub(super) fn snapshot_checkpoint() -> Value {
        json!({"version":"v1", "reusedCheckpoint":true, "snapshot": {
            "snapshotId":"snapshot", "checkpointId":SNAPSHOT_CHECKPOINT,
            "state":"complete", "manifestRevision":"manifest", "scope":".",
            "fileCount":1, "totalBytes":1, "createdAtUnixMs":1,
            "skipped":{"nestedRepositories":0,"mounts":0,"specialFiles":0,
                "oversizedFiles":0,"unreadableEntries":0,"unstableFiles":0,
                "unrepresentableNames":0,"samples":[]}
        }})
    }

    pub(super) fn snapshot_refusal(code: &str) -> Value {
        json!({"code":SNAPSHOT_REFUSAL, "message":SNAPSHOT_RPC_MESSAGE, "data":{"code":code}})
    }

    pub(super) fn snapshot_host(
        respond: impl Fn(&str, &Value, Result<Value, Value>) -> Result<Value, Value> + Send + 'static,
    ) -> ScriptedHost {
        let prepared = Mutex::new(HashMap::<String, (Value, Value)>::new());
        ScriptedHost::rpc(move |method, params| {
            let reply = match method {
                contract::SNAPSHOT_CHECKPOINT_METHOD => {
                    Err(snapshot_refusal(super::snapshot::NOT_FOUND))
                }
                contract::SNAPSHOT_PREPARE_CAPTURE_METHOD => {
                    let mut request = params.clone();
                    request.as_object_mut().unwrap().remove("_meta");
                    let digest = super::CatalogRevision::for_serializable(&request).unwrap();
                    let resources = [
                        ("file:.", "read"),
                        ("snapshot-store:captures", "read"),
                        ("snapshot-store:captures", "write"),
                        ("snapshot-store:captures", "delete"),
                    ]
                    .into_iter()
                    .map(|(display, access)| {
                        json!({
                            "display":display, "access":access, "resourceId":"resource",
                            "scope":["resource"], "revision":null
                        })
                    })
                    .collect::<Vec<_>>();
                    let mut prepared = prepared.lock().unwrap();
                    let preparation_id = format!("{SNAPSHOT_PREPARATION}-{}", prepared.len());
                    let response = json!({
                        "version":"v1", "preparationId":preparation_id,
                        "expiresAtUnixMs":u64::MAX,
                        "binding":{"host":params["host"], "argumentDigest":digest.as_str(),
                            "contract":{"id":contract::SNAPSHOT_CAPTURE_CONTRACT_ID,"version":"v1","resultVersion":"v1"}},
                        "intent":{"kind":"mutate","mutating":true,"resources":resources}
                    });
                    prepared.insert(
                        preparation_id,
                        (response.clone(), params["checkpointId"].clone()),
                    );
                    Ok(response)
                }
                contract::EXECUTE_METHOD | contract::STATUS_METHOD => {
                    let prepared = prepared.lock().unwrap();
                    let (prepared, checkpoint) =
                        &prepared[params["preparationId"].as_str().unwrap()];
                    let mut status = serde_json::to_value(completed_status()).unwrap();
                    status["preparationId"] = params["preparationId"].clone();
                    status["invocationId"] = params["invocationId"].clone();
                    status["binding"] = prepared["binding"].clone();
                    status["expiresAtUnixMs"] = prepared["expiresAtUnixMs"].clone();
                    status["outcome"]["result"]["structuredContent"] = snapshot_checkpoint();
                    status["outcome"]["result"]["structuredContent"]["snapshot"]["checkpointId"] =
                        checkpoint.clone();
                    Ok(status)
                }
                contract::CANCEL_METHOD => {
                    Ok(json!({"version":"v1","state":"running","cancellationRequested":true}))
                }
                contract::RELEASE_METHOD => {
                    Ok(json!({"version":"v1","state":"forgotten","released":true}))
                }
                other => panic!("unexpected snapshot method {other}"),
            };
            respond(method, params, reply)
        })
    }

    fn capture_without_waiting(
        client: &RemoteWorkcellClient,
    ) -> Result<SnapshotCaptureResult, WorkspaceError> {
        smol::block_on(client.capture_snapshot(
            client.session_binding(),
            client.root_cursor(),
            &snapshot_request(),
            |_| async {},
        ))
    }

    #[test]
    fn snapshot_running_outlasts_rpc_budget_without_reexecuting() {
        let remaining = Mutex::new(SNAPSHOT_POLLS);
        let host = snapshot_host(move |method, _, reply| {
            let mut remaining = remaining.lock().unwrap();
            if matches!(method, contract::EXECUTE_METHOD | contract::STATUS_METHOD)
                && *remaining > 0
            {
                *remaining -= 1;
                let mut status = reply.unwrap();
                status["state"] = json!("running");
                status["outcome"] = Value::Null;
                Ok(status)
            } else {
                reply
            }
        });
        let temp = tempfile::tempdir().unwrap();
        let state = StateDir::from_path(temp.path().join("state"));
        let client = snapshot_client(&host.endpoint, &state);
        let waited = Mutex::new(Duration::ZERO);
        let result = smol::block_on(client.capture_snapshot(
            client.session_binding(),
            client.root_cursor(),
            &snapshot_request(),
            |delay| {
                *waited.lock().unwrap() += delay;
                async {}
            },
        ))
        .unwrap();
        assert_eq!(
            result.snapshot.checkpoint_id,
            Some(snapshot_request().checkpoint_id)
        );
        assert!(*waited.lock().unwrap() > super::DEFAULT_TIMEOUT);
        assert_eq!(host.params_for(contract::EXECUTE_METHOD).len(), 1);
        assert_eq!(
            host.params_for(contract::STATUS_METHOD).len(),
            SNAPSHOT_POLLS
        );
        assert_eq!(host.params_for(contract::RELEASE_METHOD).len(), 1);
        assert!(
            host.params_for(contract::SNAPSHOT_CAPTURE_METHOD)
                .is_empty()
        );
        assert_eq!(client.0.captures.len().unwrap(), 0);
        assert!(client.pending_remote_operations().is_empty());
        assert!(
            RemoteOperationJournal::open(&state)
                .unwrap()
                .list_pending(client.stored_binding())
                .unwrap()
                .is_empty()
        );
    }

    #[test_case(false; "lost_execute_reply")]
    #[test_case(true; "client_restart")]
    fn snapshot_commit_is_recovered_by_checkpoint(restarted: bool) {
        let published = AtomicBool::new(restarted);
        let host = snapshot_host(move |method, _, reply| match method {
            contract::EXECUTE_METHOD => {
                published.store(true, Ordering::Release);
                Err(snapshot_refusal("timed_out"))
            }
            contract::SNAPSHOT_CHECKPOINT_METHOD if published.load(Ordering::Acquire) => {
                Ok(snapshot_checkpoint())
            }
            _ => reply,
        });
        let temp = tempfile::tempdir().unwrap();
        let client = snapshot_client(
            &host.endpoint,
            &StateDir::from_path(temp.path().join("state")),
        );
        assert!(capture_without_waiting(&client).unwrap().reused_checkpoint);
        assert_eq!(
            host.params_for(contract::EXECUTE_METHOD).len(),
            usize::from(!restarted)
        );
        assert!(
            host.params_for(contract::SNAPSHOT_CAPTURE_METHOD)
                .is_empty()
        );
    }

    #[test_case("checkpointId", json!("foreign"), true; "lookup_checkpoint_id")]
    #[test_case("scope", json!("foreign"), true; "lookup_scope")]
    #[test_case("checkpointId", json!("foreign"), false; "completed_checkpoint_id")]
    #[test_case("scope", json!("foreign"), false; "completed_scope")]
    #[test_case("state", json!("corrupt"), false; "corrupt_result")]
    #[test_case("fileCount", json!(2), false; "summary_limit")]
    fn snapshot_result_identity_and_summary_are_validated(
        field: &'static str,
        value: Value,
        lookup: bool,
    ) {
        let host = snapshot_host(move |method, _, reply| {
            if lookup && method == contract::SNAPSHOT_CHECKPOINT_METHOD {
                let mut checkpoint = snapshot_checkpoint();
                checkpoint["snapshot"][field] = value.clone();
                Ok(checkpoint)
            } else if !lookup
                && matches!(method, contract::EXECUTE_METHOD | contract::STATUS_METHOD)
            {
                let mut status = reply.unwrap();
                status["outcome"]["result"]["structuredContent"]["snapshot"][field] = value.clone();
                Ok(status)
            } else {
                reply
            }
        });
        let temp = tempfile::tempdir().unwrap();
        let client = snapshot_client(
            &host.endpoint,
            &StateDir::from_path(temp.path().join("state")),
        );
        assert!(matches!(
            capture_without_waiting(&client),
            Err(WorkspaceError::IdentityMismatch | WorkspaceError::InvalidResponse { .. })
        ));
        assert_eq!(
            host.params_for(contract::EXECUTE_METHOD).len(),
            usize::from(!lookup)
        );
        assert!(host.params_for(contract::RELEASE_METHOD).is_empty());
    }

    #[test_case("contract"; "wrong_contract")]
    #[test_case("binding"; "wrong_binding")]
    #[test_case("digest"; "wrong_arguments")]
    #[test_case("intent"; "workspace_write_not_capture")]
    #[test_case("scope"; "wrong_read_scope")]
    fn snapshot_preparation_is_checked_before_execute(mismatch: &'static str) {
        let host = snapshot_host(move |method, _, reply| {
            if method != contract::SNAPSHOT_PREPARE_CAPTURE_METHOD {
                return reply;
            }
            let mut prepared = reply.unwrap();
            match mismatch {
                "contract" => {
                    prepared["binding"]["contract"]["id"] =
                        json!(contract::SNAPSHOT_RESTORE_CONTRACT_ID)
                }
                "binding" => prepared["binding"]["host"]["workspaceId"] = json!("foreign"),
                "digest" => prepared["binding"]["argumentDigest"] = json!(TEST_REQUEST_DIGEST),
                "intent" => prepared["intent"]["resources"][0]["access"] = json!("write"),
                "scope" => prepared["intent"]["resources"][0]["display"] = json!("file:foreign"),
                _ => unreachable!(),
            }
            Ok(prepared)
        });
        let temp = tempfile::tempdir().unwrap();
        let client = snapshot_client(
            &host.endpoint,
            &StateDir::from_path(temp.path().join("state")),
        );
        assert!(capture_without_waiting(&client).is_err());
        assert!(host.params_for(contract::EXECUTE_METHOD).is_empty());
        assert!(client.pending_remote_operations().is_empty());
    }

    #[test_case("indeterminate", "not_found"; "unknown_absent")]
    #[test_case("forgotten", "not_found"; "forgotten_absent")]
    #[test_case("neverSeen", "not_found"; "never_seen_absent")]
    #[test_case("indeterminate", "busy"; "unknown_busy")]
    fn snapshot_unresolved_capture_is_never_replayed(terminal: &'static str, lookup: &'static str) {
        let executed = AtomicBool::new(false);
        let host = snapshot_host(move |method, _, reply| {
            if method == contract::SNAPSHOT_CHECKPOINT_METHOD && executed.load(Ordering::Acquire) {
                return Err(snapshot_refusal(lookup));
            }
            if matches!(method, contract::EXECUTE_METHOD | contract::STATUS_METHOD) {
                executed.store(true, Ordering::Release);
                let mut status = reply.unwrap();
                status["state"] = json!(terminal);
                if terminal == "cancelled" {
                    status["outcome"] = json!({"kind":"cancelled","sideEffectsPossible":false,"result":null,"error":null});
                } else {
                    for field in ["binding", "executionId", "expiresAtUnixMs", "outcome"] {
                        status[field] = Value::Null;
                    }
                }
                return Ok(status);
            }
            reply
        });
        let temp = tempfile::tempdir().unwrap();
        let client = snapshot_client(
            &host.endpoint,
            &StateDir::from_path(temp.path().join("state")),
        );
        for _ in 0..2 {
            assert_eq!(
                capture_without_waiting(&client).unwrap_err(),
                if terminal == "cancelled" {
                    WorkspaceError::Cancelled
                } else {
                    WorkspaceError::IndeterminateOutcome
                }
            );
        }
        assert_eq!(host.params_for(contract::EXECUTE_METHOD).len(), 1);
        assert_eq!(
            host.params_for(contract::SNAPSHOT_PREPARE_CAPTURE_METHOD)
                .len(),
            1
        );
        assert_eq!(client.0.captures.len().unwrap(), 1);
        assert!(client.pending_remote_operations().is_empty());
    }

    #[test_case(false; "cancel_remains_running")]
    #[test_case(true; "cancel_rpc_failed")]
    fn snapshot_client_cancellation_uses_independent_control(fail_cancel: bool) {
        let host = snapshot_host(move |method, _, reply| {
            if method == contract::CANCEL_METHOD && fail_cancel {
                return Err(snapshot_refusal("timed_out"));
            }
            if matches!(method, contract::EXECUTE_METHOD | contract::STATUS_METHOD) {
                let mut status = reply.unwrap();
                status["state"] = json!("running");
                status["outcome"] = Value::Null;
                Ok(status)
            } else {
                reply
            }
        });
        let temp = tempfile::tempdir().unwrap();
        let client = snapshot_client(
            &host.endpoint,
            &StateDir::from_path(temp.path().join("state")),
        );
        let result = smol::block_on(client.capture_snapshot(
            client.session_binding(),
            client.root_cursor(),
            &snapshot_request(),
            |_| {
                client.0.cancellation.cancel();
                async { futures_lite::future::pending::<()>().await }
            },
        ));
        assert_eq!(result.unwrap_err(), WorkspaceError::Cancelled);
        assert_eq!(host.params_for(contract::CANCEL_METHOD).len(), 1);
        assert_eq!(client.0.captures.len().unwrap(), 1);
        assert!(host.params_for(contract::RELEASE_METHOD).is_empty());
    }

    #[test]
    fn snapshot_named_refusal_is_preserved_from_failed_outcome() {
        let host = snapshot_host(move |method, _, reply| {
            if method == contract::EXECUTE_METHOD {
                let mut status = reply.unwrap();
                status["state"] = json!("failed");
                status["outcome"]["kind"] = json!("failed");
                status["outcome"]["result"]["isError"] = json!(true);
                status["outcome"]["result"]["structuredContent"] = json!({"error":{
                    "code":"limit_exceeded", "limit":"captureEntries", "maximum":HOST_SNAPSHOT_CEILING,
                    "message":SNAPSHOT_RPC_MESSAGE
                }});
                Ok(status)
            } else {
                reply
            }
        });
        let temp = tempfile::tempdir().unwrap();
        let client = snapshot_client(
            &host.endpoint,
            &StateDir::from_path(temp.path().join("state")),
        );
        assert_eq!(
            capture_without_waiting(&client).unwrap_err(),
            WorkspaceError::LimitExceeded {
                limit: Some("captureEntries".into()),
                maximum: Some(HOST_SNAPSHOT_CEILING),
            }
        );
        assert_eq!(host.params_for(contract::RELEASE_METHOD).len(), 1);
    }

    #[test_case("not_found"; "absent")]
    #[test_case("busy"; "publication_contention")]
    fn snapshot_timeout_stays_failed_and_same_host_retry_only_polls(lookup: &'static str) {
        let executed = AtomicBool::new(false);
        let host = snapshot_host(move |method, _, reply| match method {
            contract::EXECUTE_METHOD => {
                executed.store(true, Ordering::Release);
                Err(snapshot_refusal("timed_out"))
            }
            contract::SNAPSHOT_CHECKPOINT_METHOD if executed.load(Ordering::Acquire) => {
                Err(snapshot_refusal(lookup))
            }
            contract::CANCEL_METHOD => Err(snapshot_refusal("timed_out")),
            _ => reply,
        });
        let temp = tempfile::tempdir().unwrap();
        let client = snapshot_client(
            &host.endpoint,
            &StateDir::from_path(temp.path().join("state")),
        );
        assert_eq!(
            capture_without_waiting(&client).unwrap_err(),
            RemoteWorkcellError::Timeout.into()
        );
        assert_eq!(client.0.captures.len().unwrap(), 1);
        assert!(capture_without_waiting(&client).is_ok());
        assert_eq!(host.params_for(contract::EXECUTE_METHOD).len(), 1);
        assert_eq!(host.params_for(contract::STATUS_METHOD).len(), 1);
        assert_eq!(
            host.params_for(contract::SNAPSHOT_PREPARE_CAPTURE_METHOD)
                .len(),
            1
        );
    }

    #[test]
    fn snapshot_absent_after_verified_host_restart_can_be_recaptured() {
        let host = snapshot_host(move |method, params, reply| match method {
            contract::EXECUTE_METHOD if params["host"]["instanceId"] == "instance" => {
                Err(snapshot_refusal("timed_out"))
            }
            contract::CANCEL_METHOD => Err(snapshot_refusal("timed_out")),
            _ => reply,
        });
        let temp = tempfile::tempdir().unwrap();
        let client = snapshot_client(
            &host.endpoint,
            &StateDir::from_path(temp.path().join("state")),
        );
        assert_eq!(
            capture_without_waiting(&client).unwrap_err(),
            RemoteWorkcellError::Timeout.into()
        );
        client.0.host_binding.lock().unwrap().instance_id =
            contract::Identifier::new("restarted").unwrap();
        assert!(capture_without_waiting(&client).is_ok());
        assert_eq!(host.params_for(contract::EXECUTE_METHOD).len(), 2);
        assert_eq!(client.0.captures.len().unwrap(), 0);
    }

    #[test]
    fn snapshot_dropped_waiter_explicitly_cancels_without_forgetting() {
        let settled = CancellationToken::new();
        let signal = settled.clone();
        let host = snapshot_host(move |method, _, reply| {
            if method == contract::STATUS_METHOD {
                signal.cancel();
            }
            if matches!(method, contract::EXECUTE_METHOD | contract::STATUS_METHOD) {
                let mut status = reply.unwrap();
                status["state"] = json!("running");
                status["outcome"] = Value::Null;
                Ok(status)
            } else {
                reply
            }
        });
        let temp = tempfile::tempdir().unwrap();
        let client = snapshot_client(
            &host.endpoint,
            &StateDir::from_path(temp.path().join("state")),
        );
        smol::block_on(async {
            let waiting = CancellationToken::new();
            let request = snapshot_request();
            futures_lite::future::race(
                async {
                    let _ = client
                        .capture_snapshot(
                            client.session_binding(),
                            client.root_cursor(),
                            &request,
                            |_| {
                                waiting.cancel();
                                futures_lite::future::pending::<()>()
                            },
                        )
                        .await;
                    panic!("capture unexpectedly settled before waiter drop");
                },
                waiting.cancelled(),
            )
            .await;
            settled.cancelled().await;
            assert_eq!(client.0.captures.len().unwrap(), 1);
            assert_eq!(host.params_for(contract::CANCEL_METHOD).len(), 1);
            assert!(host.params_for(contract::RELEASE_METHOD).is_empty());
        });
    }

    #[test]
    fn snapshot_publication_winning_cancellation_is_recovered_on_next_call() {
        let published = AtomicBool::new(false);
        let host = snapshot_host(move |method, _, reply| match method {
            contract::EXECUTE_METHOD => {
                let mut status = reply.unwrap();
                status["state"] = json!("running");
                status["outcome"] = Value::Null;
                Ok(status)
            }
            contract::CANCEL_METHOD => {
                published.store(true, Ordering::Release);
                Ok(json!({"version":"v1","state":"completed","cancellationRequested":false}))
            }
            contract::SNAPSHOT_CHECKPOINT_METHOD if published.load(Ordering::Acquire) => {
                Ok(snapshot_checkpoint())
            }
            _ => reply,
        });
        let temp = tempfile::tempdir().unwrap();
        let state = StateDir::from_path(temp.path().join("state"));
        let client = snapshot_client(&host.endpoint, &state);
        let result = smol::block_on(client.capture_snapshot(
            client.session_binding(),
            client.root_cursor(),
            &snapshot_request(),
            |_| {
                client.0.cancellation.cancel();
                futures_lite::future::pending::<()>()
            },
        ));
        assert_eq!(result.unwrap_err(), WorkspaceError::Cancelled);
        assert_eq!(client.0.captures.len().unwrap(), 0);
        let reconnected = snapshot_client(&host.endpoint, &state);
        assert!(
            capture_without_waiting(&reconnected)
                .unwrap()
                .reused_checkpoint
        );
        assert_eq!(host.params_for(contract::EXECUTE_METHOD).len(), 1);
    }

    #[test]
    fn snapshot_busy_without_a_handle_does_not_start_work() {
        let host = snapshot_host(move |_, _, _| Err(snapshot_refusal("busy")));
        let temp = tempfile::tempdir().unwrap();
        let client = snapshot_client(
            &host.endpoint,
            &StateDir::from_path(temp.path().join("state")),
        );
        assert_eq!(
            capture_without_waiting(&client).unwrap_err(),
            WorkspaceError::Busy
        );
        assert_eq!(host.methods(), [contract::SNAPSHOT_CHECKPOINT_METHOD]);
    }

    #[test_case("prepareCapture")]
    #[test_case("checkpoint")]
    fn snapshot_old_only_host_fails_discovery(method: &'static str) {
        let mut descriptor = serde_json::to_value(scripted_descriptor()).unwrap();
        descriptor["capabilities"]["snapshots"]["methods"]
            .as_object_mut()
            .unwrap()
            .remove(method);
        let capabilities = serde_json::from_value(descriptor["capabilities"].clone()).unwrap();
        assert!(
            !workspace_capabilities(&capabilities).supports(WorkspaceCapability::SnapshotCapture)
        );
        let host = ScriptedHost::new(move |method, _| {
            assert_eq!(method, "server/discover");
            json!({"resultType":"complete","ttlMs":0,"cacheScope":"private",
                "supportedVersions":[super::PROTOCOL_VERSION],
                "capabilities":{"extensions":{contract::EXTENSION_ID:descriptor}}})
        });
        let selection = selection(
            host.endpoint.as_url().as_str(),
            WorkcellSourceRef::Direct,
            None,
        );
        let temp = tempfile::tempdir().unwrap();
        let state = StateDir::from_path(temp.path().join("state"));
        let result = smol::block_on(RemoteWorkcellClient::connect(
            &selection,
            None,
            SessionBindingId::new("test").unwrap(),
            RemoteOperationJournal::open(&state).unwrap(),
            CancellationToken::new(),
        ));
        assert_eq!(result.unwrap_err(), RemoteWorkcellError::CapabilityMismatch);
        assert_eq!(host.methods(), ["server/discover"]);
    }

    fn shell_manifest() -> ToolManifest {
        freeze_catalog(
            serde_json::from_value(json!({
                "resultType":"complete",
                "ttlMs":0,
                "cacheScope":"private",
                "tools":[{
                    "name":"shell",
                    "title":"Shell",
                    "description":"Run a command",
                    "inputSchema":{"$schema":JSON_SCHEMA_VERSION,"type":"object"},
                    "outputSchema":{"$schema":JSON_SCHEMA_VERSION,"type":"object"},
                    "annotations":{
                        "readOnlyHint":false,
                        "destructiveHint":true,
                        "idempotentHint":false,
                        "openWorldHint":true
                    },
                    "_meta":{
                        "ai.workcell/presentation-profile":"shell.result.v1",
                        "ai.workcell/contract":{
                            "id":SHELL_CONTRACT_ID,
                            "version":"v1",
                            "resultVersion":"v1"
                        }
                    }
                }]
            }))
            .unwrap(),
        )
        .unwrap()
    }

    fn shell_intent() -> Value {
        json!({
            "kind":"execute",
            "mutating":true,
            "resources":[{
                "resourceId":"resource-command",
                "scope":["resource-command"],
                "display":"true",
                "access":"execute",
                "revision":null
            }]
        })
    }

    /// A command can wait on review past the moment the server expires its
    /// preparation on its own timer. The wait is legitimate, so the call has
    /// to be renewed and run, never reported as a conflict.
    #[test]
    fn a_command_reviewed_past_its_preparation_renews_instead_of_conflicting() {
        const LAPSING_MS: u64 = 200;
        const LAPSE_POLL: Duration = Duration::from_millis(5);
        const HEALTHY_MS: u64 = 600_000;
        const RENEWED_PREPARATION: &str = "prepared-2";
        const COMMAND_OUTPUT: &str = "renewed";
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let seed = StoredWorkspaceBinding::local_from_cwd("opaque-workspace");
        let cursor = WorkspaceCursor::new(
            seed.binding(),
            ResourceScope::root(ResourceId::new("root").unwrap()),
            0,
            CwdHandle::new("cwd").unwrap(),
        );
        let stored_binding = seed.with_cursor(cursor.clone()).unwrap();
        let binding = stored_binding.binding().clone();
        let coordinator = RemoteMutationJournal::new(
            RemoteOperationJournal::open(&state_dir).unwrap(),
            stored_binding.clone(),
        )
        .unwrap();
        let manifest = shell_manifest();
        let issued: Arc<Mutex<Vec<(String, u64)>>> = Arc::new(Mutex::new(Vec::new()));
        let prepared = issued.clone();
        let host = ScriptedHost::new(move |method, params| {
            let host = host_binding();
            match method {
                contract::PREPARE_METHOD => {
                    let mut prepared = prepared.lock().unwrap();
                    let count = prepared.len() + 1;
                    let expires = unix_millis() + if count < 2 { LAPSING_MS } else { HEALTHY_MS };
                    let preparation_id = format!("prepared-{count}");
                    prepared.push((preparation_id.clone(), expires));
                    json!({
                        "version":"v1",
                        "preparationId":preparation_id,
                        "expiresAtUnixMs":expires,
                        "binding":{
                            "host":host,
                            "contract":{
                                "id":SHELL_CONTRACT_ID,
                                "version":"v1",
                                "resultVersion":"v1"
                            },
                            "argumentDigest":TEST_REQUEST_DIGEST
                        },
                        "intent":shell_intent()
                    })
                }
                contract::EXECUTE_METHOD => json!({
                    "version":"v1",
                    "state":"completed",
                    "preparationId":params["preparationId"],
                    "invocationId":params["invocationId"],
                    "executionId":params["invocationId"],
                    "expiresAtUnixMs":issued
                        .lock()
                        .unwrap()
                        .iter()
                        .find(|(id, _)| Some(id.as_str()) == params["preparationId"].as_str())
                        .map(|(_, expires)| *expires)
                        .unwrap(),
                    "binding":{
                        "host":host,
                        "contract":{
                            "id":SHELL_CONTRACT_ID,
                            "version":"v1",
                            "resultVersion":"v1"
                        },
                        "argumentDigest":TEST_REQUEST_DIGEST
                    },
                    "outcome":{
                        "kind":"completed",
                        "sideEffectsPossible":false,
                        "result":{
                            "version":"v1",
                            "content":[{"type":"text","text":COMMAND_OUTPUT}],
                            "structuredContent":{
                                "version":1,
                                "kind":"shell",
                                "relativeWorkdir":"",
                                "timeoutMs":1_000,
                                "durationMs":1,
                                "exitCode":0,
                                "signal":null,
                                "timedOut":false,
                                "outputLimitExceeded":false,
                                "finalSequence":0,
                                "stdoutUtf8Bytes":COMMAND_OUTPUT.len(),
                                "stderrUtf8Bytes":0,
                                "stdout":COMMAND_OUTPUT,
                                "stderr":"",
                                "stdoutCaptureTruncated":false,
                                "stderrCaptureTruncated":false,
                                "stdoutPreviewTruncated":false,
                                "stderrPreviewTruncated":false,
                                "stdoutRedrawsCollapsed":0,
                                "stderrRedrawsCollapsed":0
                            },
                            "isError":false
                        },
                        "error":null
                    },
                    "progressMetadata":{
                        "firstRetainedSequence":null,
                        "nextSequence":1,
                        "gapBeforeFirst":false
                    },
                    "progress":[]
                }),
                contract::RELEASE_METHOD => {
                    json!({"version":"v1","state":"forgotten","released":true})
                }
                other => panic!("unexpected method {other}"),
            }
        });
        let (transport, events) = RemoteTransport::new(&host.endpoint, None).unwrap();
        let descriptor = scripted_descriptor();
        let client = RemoteWorkcellClient(Arc::new(RemoteInner {
            staging: PrivateStaging::new(super::transfer::negotiated_limits(
                descriptor.capabilities.reviewed_transfer.as_ref(),
            )),
            transport,
            capabilities: workspace_capabilities(&descriptor.capabilities),
            descriptor,
            host_binding: Mutex::new(host_binding()),
            authority: binding.authority().clone(),
            project: binding.project().clone(),
            session_binding: binding.clone(),
            stored_binding,
            root_cursor: cursor.clone(),
            catalog: manifest
                .tools
                .iter()
                .cloned()
                .map(|tool| (tool.name.clone(), tool))
                .collect(),
            manifest,
            events,
            cancellation: CancellationToken::new(),
            paths: Mutex::new(ResourceCache::new(8, Duration::MAX)),
            repositories: Mutex::new(BoundedMap::new(8, Duration::MAX)),
            cursors: Mutex::new(CursorRegistry {
                records: HashMap::from([(
                    cursor.cwd_handle().clone(),
                    CursorRecord {
                        cursor: cursor.clone(),
                        path: WorkspacePath::root(),
                    },
                )]),
                limit: 8,
            }),
            watches: Mutex::new(WatchRegistry::new(8, Duration::MAX)),
            operations: Mutex::new(OperationRegistry::new(8)),
            captures: super::snapshot::CaptureRegistry::default(),
            operation_slots: super::Event::new(),
            mutation_journal: coordinator,
        }));
        let raw_input = json!({"command":"true"});
        let request = ToolPrepareRequest {
            name: "shell".into(),
            input: raw_input.clone(),
        };
        let call =
            smol::block_on(client.prepare_canonical_tool(&binding, &cursor, &request)).unwrap();
        assert!(call.expires_within(REMOTE_PREPARATION_RENEWAL));
        let mut ctx = crate::tests::context(
            temp.path(),
            Arc::new(ToolRegistry::new()),
            CancelToken::none(),
        );
        ctx.workspace_session = Some(
            WorkspaceSession::new(
                client.workspace_handle().unwrap(),
                binding.clone(),
                cursor.clone(),
            )
            .unwrap(),
        );
        let invocation = RemoteWorkcellInvocation {
            client: client.clone(),
            kind: ToolKind::Shell,
            input: Input::parse(ToolKind::Shell, raw_input.clone()).unwrap(),
            raw_input,
            prepared: AsyncMutex::new(RemotePreparedState::default()),
        };
        let mut cleanup = RemoteExecutionCleanup {
            client: client.clone(),
            call: Some(call.clone()),
            execution_started: false,
        };
        let expiry = call.prepared.operation.expires_at_unix_ms.unwrap();
        let result = smol::block_on(async {
            while unix_millis() <= expiry {
                smol::Timer::after(LAPSE_POLL).await;
            }
            invocation.execute_remote(&ctx, call, &mut cleanup).await
        });

        let output = result.output.expect("reviewed command reported an error");
        assert!(!result.is_error);
        assert!(output.as_text().contains(COMMAND_OUTPUT), "{output:?}");
        let executed = host.params_for(contract::EXECUTE_METHOD);
        assert_eq!(executed.len(), 1);
        assert_eq!(executed[0]["preparationId"], RENEWED_PREPARATION);
        assert_eq!(
            host.methods()
                .iter()
                .filter(|method| method.as_str() == contract::PREPARE_METHOD)
                .count(),
            2
        );
        assert!(client.pending_remote_operations().is_empty());
        assert_eq!(client.0.operations.lock().unwrap().len(), 0);
    }

    /// A pending operation from a nested cursor is recovered at the root
    /// scope. When the host says NeverSeen or Prepared (same instance), the
    /// local dispatch fence is weaker than the host's assertion, so the
    /// operation resolves to cancelled and leaves the pending list.
    #[test_case(RemoteOperationState::Dispatched, OperationState::NeverSeen; "dispatched_never_seen")]
    #[test_case(RemoteOperationState::Indeterminate, OperationState::NeverSeen; "indeterminate_never_seen")]
    #[test_case(RemoteOperationState::Dispatched, OperationState::Prepared; "dispatched_prepared")]
    fn nested_cursor_operation_the_host_never_saw_resolves_at_root_scope(
        local_state: RemoteOperationState,
        server_state: OperationState<Value>,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let seed = StoredWorkspaceBinding::local_from_cwd("opaque-workspace");
        let root_id = ResourceId::new("root").unwrap();
        let root = seed
            .with_cursor(WorkspaceCursor::new(
                seed.binding(),
                ResourceScope::root(root_id.clone()),
                0,
                CwdHandle::new("root-cwd").unwrap(),
            ))
            .unwrap();
        let nested_cursor = WorkspaceCursor::new(
            root.binding(),
            ResourceScope::new(vec![root_id], ResourceId::new("nested").unwrap()).unwrap(),
            1,
            CwdHandle::new("nested-cwd").unwrap(),
        );
        let pending = journal_operation("pending", DIRECT_EXEC_KIND);
        {
            let journal = RemoteOperationJournal::open(&state_dir).unwrap();
            let coordinator = RemoteMutationJournal::new(journal, root.clone()).unwrap();
            coordinator
                .reserve_at(&pending, root.binding(), &nested_cursor)
                .unwrap();
            coordinator.mark_dispatched(&pending.operation_id).unwrap();
            if local_state == RemoteOperationState::Indeterminate {
                coordinator
                    .mark_indeterminate(&pending.operation_id)
                    .unwrap();
            }
        }

        let journal = RemoteOperationJournal::open(&state_dir).unwrap();
        let coordinator = RemoteMutationJournal::new(journal, root.clone()).unwrap();
        assert!(
            coordinator
                .reconcile_recovery(&pending.operation_id, &server_state, true)
                .unwrap()
        );
        assert!(coordinator.pending().is_empty());
    }

    #[test_case(OperationState::NeverSeen; "never_seen")]
    #[test_case(OperationState::Prepared; "prepared")]
    fn a_reserved_operation_the_host_never_saw_resolves_to_cancelled(
        server_state: OperationState<Value>,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let binding = StoredWorkspaceBinding::local_from_cwd("opaque-workspace");
        let operation = journal_operation("reserved", WORKSPACE_MUTATION_KIND);
        let journal = RemoteOperationJournal::open(&state_dir).unwrap();
        let coordinator = RemoteMutationJournal::new(journal, binding.clone()).unwrap();
        coordinator.reserve(&operation).unwrap();
        drop(coordinator);
        let journal = RemoteOperationJournal::open(&state_dir).unwrap();
        let coordinator = RemoteMutationJournal::new(journal, binding.clone()).unwrap();
        assert!(
            coordinator
                .reconcile_recovery(&operation.operation_id, &server_state, true)
                .unwrap()
        );
        assert!(coordinator.pending().is_empty());
        drop(coordinator);
        assert!(
            RemoteOperationJournal::open(&state_dir)
                .unwrap()
                .list_pending(&binding)
                .unwrap()
                .is_empty()
        );
    }

    /// The local dispatch fence fires before the send, so `dispatched` proves
    /// intent, not delivery. When the same host instance reports `NeverSeen`,
    /// the host's assertion is strictly stronger: the request never arrived.
    /// Refusing to resolve here is what wedged a cancelled shell command into
    /// an indeterminate state that blocked every subsequent mutation.
    #[test_case(OperationState::NeverSeen; "never_seen")]
    #[test_case(OperationState::Prepared; "prepared")]
    fn a_dispatched_operation_the_host_never_saw_resolves_to_cancelled(
        server_state: OperationState<Value>,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let binding = StoredWorkspaceBinding::local_from_cwd("opaque-workspace");
        let operation = journal_operation("dispatched", WORKSPACE_MUTATION_KIND);
        let journal = RemoteOperationJournal::open(&state_dir).unwrap();
        let coordinator = RemoteMutationJournal::new(journal, binding.clone()).unwrap();
        coordinator.reserve(&operation).unwrap();
        coordinator
            .mark_dispatched(&operation.operation_id)
            .unwrap();
        drop(coordinator);
        let journal = RemoteOperationJournal::open(&state_dir).unwrap();
        let coordinator = RemoteMutationJournal::new(journal, binding.clone()).unwrap();
        assert!(
            coordinator
                .reconcile_recovery(&operation.operation_id, &server_state, true)
                .unwrap()
        );
        assert!(coordinator.pending().is_empty());
    }

    #[test_case(false; "dispatched")]
    #[test_case(true; "already_indeterminate")]
    fn live_never_seen_status_cannot_commit_a_clean_terminal(already_indeterminate: bool) {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let binding = StoredWorkspaceBinding::local_from_cwd("opaque-workspace");
        let journal = RemoteOperationJournal::open(&state_dir).unwrap();
        let coordinator = RemoteMutationJournal::new(journal, binding.clone()).unwrap();
        let operation = journal_operation("live", WORKSPACE_MUTATION_KIND);
        coordinator.reserve(&operation).unwrap();
        coordinator
            .mark_dispatched(&operation.operation_id)
            .unwrap();
        if already_indeterminate {
            coordinator
                .mark_indeterminate(&operation.operation_id)
                .unwrap();
        }
        assert!(
            !coordinator
                .commit_terminal::<Value>(&operation.operation_id, &OperationState::NeverSeen)
                .unwrap()
        );
        drop(coordinator);
        let records = RemoteOperationJournal::open(&state_dir)
            .unwrap()
            .list_pending(&binding)
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].state, RemoteOperationState::Indeterminate);
        assert!(records[0].side_effects_possible);
        assert!(records[0].acknowledged_at.is_none());
    }

    #[test]
    fn nested_cursor_canonical_file_and_shell_preparation_retain_exact_context() {
        let seed = StoredWorkspaceBinding::local_from_cwd("opaque-workspace");
        let root_id = ResourceId::new("root").unwrap();
        let root_cursor = WorkspaceCursor::new(
            seed.binding(),
            ResourceScope::root(root_id.clone()),
            0,
            CwdHandle::new("root-cwd").unwrap(),
        );
        let nested_cursor = WorkspaceCursor::new(
            seed.binding(),
            ResourceScope::new(vec![root_id], ResourceId::new("nested").unwrap()).unwrap(),
            0,
            CwdHandle::new("nested-cwd").unwrap(),
        );
        let prepared = PreparedWorkspaceContext {
            binding: seed.binding().clone(),
            cursor: nested_cursor.clone(),
        };

        assert!(prepared.matches(seed.binding(), &nested_cursor));
        assert!(!prepared.matches(seed.binding(), &root_cursor));
        let read = contract::OperationIntent {
            kind: contract::OperationKind::Read,
            mutating: false,
            resources: vec![contract::ResourceIntent {
                scope: vec![contract::ResourceId::new("nested-file").unwrap()],
                resource_id: contract::ResourceId::new("nested-file").unwrap(),
                access: contract::ResourceAccess::Read,
                revision: None,
                display: contract::DisplayText::new("nested/file").unwrap(),
            }],
        };
        let shell = contract::OperationIntent {
            kind: contract::OperationKind::Execute,
            mutating: true,
            resources: read.resources.clone(),
        };
        assert_eq!(
            canonical_journal_policy("file_write", &read),
            Some(WRITE_KIND.to_owned())
        );
        assert_eq!(
            canonical_journal_policy("shell", &shell),
            Some(SHELL_KIND.to_owned())
        );
    }

    #[test]
    fn lost_execute_response_reconciles_by_persisted_status_identity() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let binding = StoredWorkspaceBinding::local_from_cwd("opaque-workspace");
        let journal = RemoteOperationJournal::open(&state_dir).unwrap();
        let coordinator = RemoteMutationJournal::new(journal, binding).unwrap();
        let mut operation = journal_operation("recovery", "workspace_mutation");
        operation.invocation_id = OperationId::new("invocation").unwrap();
        operation.preparation_id = OperationId::new("prepared").unwrap();
        coordinator.reserve(&operation).unwrap();
        coordinator
            .mark_dispatched(&operation.operation_id)
            .unwrap();
        let (dispatched_at, cursor) = {
            let state = coordinator.state.lock().unwrap();
            let pending = state.pending.get(&operation.operation_id).unwrap();
            (pending.dispatched_at, pending.cursor.clone())
        };
        let recovery = RecoveryOperation {
            operation_kind: operation.operation_kind.clone(),
            publication_cwd: None,
            publication_id: None,
            host_instance_id: "instance".to_owned(),
            operation_id: operation.operation_id.clone(),
            invocation_id: OperationId::new("invocation").unwrap(),
            preparation_id: OperationId::new("prepared").unwrap(),
            request_digest: RequestDigest::sha256(TEST_REQUEST_DIGEST).unwrap(),
            dispatched_at,
            cursor,
        };
        let status = recovery_status(&completed_status(), &recovery, &host_binding()).unwrap();
        assert!(
            coordinator
                .reconcile_recovery(&operation.operation_id, &status.state, true)
                .unwrap()
        );
        assert!(coordinator.pending().is_empty());
    }

    #[test]
    fn forgotten_server_state_remains_indeterminate_and_requires_acknowledgement() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let binding = StoredWorkspaceBinding::local_from_cwd("opaque-workspace");
        let journal = RemoteOperationJournal::open(&state_dir).unwrap();
        let coordinator = RemoteMutationJournal::new(journal, binding).unwrap();
        let mut operation = journal_operation("forgotten", "workspace_mutation");
        operation.invocation_id = OperationId::new("invocation").unwrap();
        operation.preparation_id = OperationId::new("prepared").unwrap();
        coordinator.reserve(&operation).unwrap();
        coordinator
            .mark_dispatched(&operation.operation_id)
            .unwrap();
        let mut response = completed_status();
        response.state = contract::OperationState::Forgotten;
        response.execution_id = None;
        response.expires_at_unix_ms = None;
        response.binding = None;
        response.outcome = None;
        let (dispatched_at, cursor) = {
            let state = coordinator.state.lock().unwrap();
            let pending = state.pending.get(&operation.operation_id).unwrap();
            (pending.dispatched_at, pending.cursor.clone())
        };
        let recovery = RecoveryOperation {
            operation_kind: operation.operation_kind.clone(),
            publication_cwd: None,
            publication_id: None,
            host_instance_id: "instance".to_owned(),
            operation_id: operation.operation_id.clone(),
            invocation_id: OperationId::new("invocation").unwrap(),
            preparation_id: OperationId::new("prepared").unwrap(),
            request_digest: RequestDigest::sha256(TEST_REQUEST_DIGEST).unwrap(),
            dispatched_at,
            cursor,
        };
        let status = recovery_status(&response, &recovery, &host_binding()).unwrap();
        assert!(matches!(status.state, OperationState::Forgotten));
        coordinator
            .recovery_indeterminate(&operation.operation_id)
            .unwrap();
        assert_eq!(
            coordinator.pending()[0].state,
            RemoteOperationState::Indeterminate
        );
        coordinator.acknowledge(&operation.operation_id).unwrap();
        assert!(coordinator.pending().is_empty());
    }

    #[test_case(false, false, (false, false); "complete_final_page")]
    #[test_case(false, true, (true, false); "normal_continuation")]
    #[test_case(true, false, (false, true); "underlying_truncation_without_cursor")]
    #[test_case(true, true, (true, true); "continuation_and_underlying_truncation")]
    fn pagination_and_incompleteness_are_independent(
        underlying_truncated: bool,
        has_cursor: bool,
        expected: (bool, bool),
    ) {
        assert_eq!(pagination_flags(underlying_truncated, has_cursor), expected);
    }

    #[test]
    fn serialized_item_accounting_includes_non_payload_fields() {
        let event: contract::ProgressEvent = serde_json::from_value(json!({
            "executionId":"execution",
            "sequence":1,
            "kind":"stdout",
            "chunk":"x"
        }))
        .unwrap();
        assert_eq!(
            serialized_items_bytes(std::slice::from_ref(&event)).unwrap(),
            serde_json::to_vec(&event).unwrap().len()
        );
        let line: contract::ScmDiffLine = serde_json::from_value(json!({
            "path":"src/lib.rs",
            "kind":"addition",
            "change":null,
            "oldLine":null,
            "newLine":1,
            "text":"x"
        }))
        .unwrap();
        assert!(serialized_items_bytes(&[line]).unwrap() > 1);
        let watch: contract::WatchEvent = serde_json::from_value(json!({
            "sequence":1,
            "kind":"modify",
            "path":"src/lib.rs"
        }))
        .unwrap();
        assert!(serialized_items_bytes(&[watch]).unwrap() > "src/lib.rs".len());
    }

    #[test]
    fn cleanup_preview_is_a_disjoint_duplicate_free_partition() {
        let first = CheckpointId::new("first").unwrap();
        let second = CheckpointId::new("second").unwrap();
        let preview = SnapshotCleanupPreview {
            checkpoint_ids: vec![second.clone()],
            missing_checkpoint_ids: vec![first.clone()],
            reclaimable_bytes: 1,
        };
        assert!(cleanup_preview_partitions(
            &[first.clone(), second.clone()],
            &preview
        ));
        assert!(!cleanup_preview_partitions(
            &[first.clone(), first.clone()],
            &preview
        ));
        let overlap = SnapshotCleanupPreview {
            checkpoint_ids: vec![first.clone()],
            missing_checkpoint_ids: vec![first.clone()],
            reclaimable_bytes: 1,
        };
        assert!(!cleanup_preview_partitions(&[first, second], &overlap));
        assert!(same_unique_ids(
            &preview.checkpoint_ids,
            &preview.checkpoint_ids
        ));
        assert!(!same_unique_ids(
            &[
                preview.checkpoint_ids[0].clone(),
                preview.checkpoint_ids[0].clone()
            ],
            &preview.checkpoint_ids
        ));
    }

    #[test_case(0, 1 ; "zero_becomes_the_smallest_ceiling_the_host_accepts")]
    #[test_case(10, 10 ; "a_lower_ceiling_is_kept")]
    #[test_case(u64::MAX, HOST_SNAPSHOT_CEILING ; "a_higher_ceiling_is_clamped_to_the_host")]
    fn capture_limits_are_clamped_to_what_the_host_accepts(requested: u64, expected: u64) {
        let host = contract::WorkspaceSnapshotLimits {
            max_files: HOST_SNAPSHOT_CEILING as u32,
            max_file_bytes: HOST_SNAPSHOT_CEILING,
            max_total_bytes: HOST_SNAPSHOT_CEILING,
            max_capture_entries: 1,
            max_capture_path_bytes: 1,
            max_snapshots: 1,
            max_storage_bytes: 1,
            max_concurrent_captures: 1,
            max_cleanup_checkpoints: 1,
        };
        let limits = super::capture_limits(
            &SnapshotCaptureLimits {
                max_files: requested,
                max_file_bytes: requested,
                max_total_bytes: requested,
            },
            &host,
        );

        assert_eq!(
            (
                u64::from(limits.max_files),
                limits.max_file_bytes,
                limits.max_total_bytes
            ),
            (expected, expected, expected)
        );
    }

    #[test]
    fn unrevert_terminal_status_must_name_the_requested_source_restore() {
        let preview = SnapshotOperationPreview::Unrevert(SnapshotUnrevertPreview {
            source_restore_id: RestoreId::new("source-restore").unwrap(),
            restore: SnapshotRestorePreview {
                restore_id: RestoreId::new("new-restore").unwrap(),
                target_snapshot_id: SnapshotId::new("target").unwrap(),
                source_snapshot_id: SnapshotId::new("source").unwrap(),
                counts: SnapshotChangeCounts::default(),
                changes: Vec::new(),
                created_directories: Vec::new(),
            },
        });
        let response = json!({
            "version":"v1",
            "restore":{
                "restoreId":"new-restore",
                "state":"completed",
                "targetSnapshotId":"target",
                "sourceSnapshotId":"source",
                "appliedFiles":0,
                "totalFiles":0,
                "acknowledgementRequired":false,
                "reconciliationRequired":false,
                "unrevertOf":"source-restore"
            }
        });
        assert!(parse_snapshot_result(&response, &preview).is_ok());
        let mut wrong_source = response.clone();
        wrong_source["restore"]["unrevertOf"] = json!("foreign-restore");
        assert!(parse_snapshot_result(&wrong_source, &preview).is_err());
        let mut wrong_capture = response;
        wrong_capture["restore"]["sourceSnapshotId"] = json!("foreign-capture");
        assert!(parse_snapshot_result(&wrong_capture, &preview).is_err());
    }

    #[test]
    fn server_smaller_limits_and_zero_are_rejected() {
        assert!(require_nonzero_within(1_u32, 2).is_ok());
        assert!(require_nonzero_within(0_u32, 2).is_err());
        assert!(require_nonzero_within(3_u32, 2).is_err());
    }

    #[test]
    fn catalog_revision_is_computed_from_the_complete_received_manifest() {
        let first = freeze_catalog(tool_list("first")).unwrap();
        let same = freeze_catalog(tool_list("first")).unwrap();
        let changed = freeze_catalog(tool_list("changed")).unwrap();
        assert_eq!(first, same);
        assert_ne!(first.revision, changed.revision);
        assert_eq!(first.tools[0].description, "first");
        let mut cacheable = tool_list("first");
        cacheable.ttl_ms = 1;
        assert!(matches!(
            freeze_catalog(cacheable),
            Err(RemoteWorkcellError::CatalogMismatch)
        ));
    }

    #[test]
    fn resource_cache_invalidates_both_sides_of_recreated_and_moved_aliases() {
        let mut paths = ResourceCache::new(4, std::time::Duration::from_secs(60));
        let old_id = ResourceId::new("old-resource").unwrap();
        let new_id = ResourceId::new("new-resource").unwrap();
        let first = WorkspacePath::new("first").unwrap();
        let second = WorkspacePath::new("second").unwrap();
        paths.insert(old_id.clone(), first.clone());
        paths.insert(new_id.clone(), first.clone());
        assert!(paths.get_path(&old_id).is_none());
        assert_eq!(paths.id_for_path(&first), Some(&new_id));

        paths.insert(new_id.clone(), second.clone());
        assert!(paths.id_for_path(&first).is_none());
        assert_eq!(paths.get_path(&new_id), Some(&second));
        assert_resource_cache_consistent(&paths);
    }

    fn assert_resource_cache_consistent(cache: &ResourceCache) {
        assert!(cache.by_id.len() <= cache.limit);
        assert_eq!(cache.by_id.len(), cache.by_path.len());
        assert_eq!(cache.by_id.len(), cache.by_touch.len());
        for (id, entry) in &cache.by_id {
            assert_eq!(cache.by_path.get(&entry.value), Some(id));
            assert!(cache.by_touch.contains(&(entry.touched, id.clone())));
        }
    }

    #[test_case(Duration::ZERO; "zero_ttl")]
    #[test_case(Duration::from_secs(10); "sliding_ttl")]
    fn resource_cache_preserves_exact_expiry_and_refresh(ttl: Duration) {
        let mut cache = ResourceCache::new(1, ttl);
        let id = ResourceId::new("resource").unwrap();
        let path = WorkspacePath::new("file").unwrap();
        let start = Instant::now();
        cache.insert_at(id.clone(), path.clone(), start);
        let refreshed = start + ttl;
        assert_eq!(cache.get_path_at(&id, refreshed), Some(&path));
        assert_resource_cache_consistent(&cache);
        let expiry = refreshed + ttl;
        cache.expire(expiry);
        assert_eq!(cache.by_path.get(&path), Some(&id));
        assert_eq!(
            cache.get_path_at(&id, expiry + Duration::from_nanos(1)),
            None
        );
        assert!(cache.by_id.is_empty());
        assert_resource_cache_consistent(&cache);
    }

    #[test_case(false; "oldest_expires")]
    #[test_case(true; "lookup_refresh_reorders_expiry")]
    fn resource_cache_expires_only_due_entries(refresh: bool) {
        let mut cache = ResourceCache::new(2, CACHE_CLOCK_STEP * 3);
        let first_id = ResourceId::new("first").unwrap();
        let second_id = ResourceId::new("second").unwrap();
        let first_path = WorkspacePath::new("first").unwrap();
        let second_path = WorkspacePath::new("second").unwrap();
        let start = Instant::now();
        cache.insert_at(first_id.clone(), first_path.clone(), start);
        cache.insert_at(
            second_id.clone(),
            second_path.clone(),
            start + CACHE_CLOCK_STEP,
        );
        if refresh {
            assert_eq!(
                cache.get_path_at(&first_id, start + CACHE_CLOCK_STEP * 2),
                Some(&first_path)
            );
        }
        let now = start + CACHE_CLOCK_STEP * 4 + Duration::from_nanos(1);
        cache.expire(now);
        assert_eq!(cache.get_path_at(&second_id, now), None);
        assert!(!cache.by_path.contains_key(&second_path));
        assert_eq!(
            cache.get_path_at(&first_id, now),
            refresh.then_some(&first_path)
        );
        assert_resource_cache_consistent(&cache);
    }

    #[test_case(false; "evicts_oldest_insert")]
    #[test_case(true; "lookup_protects_recent_resource")]
    fn resource_cache_eviction_uses_last_touch_without_alias_leaks(refresh: bool) {
        let mut cache = ResourceCache::new(2, Duration::MAX);
        let first_id = ResourceId::new("first").unwrap();
        let second_id = ResourceId::new("second").unwrap();
        let third_id = ResourceId::new("third").unwrap();
        let first_path = WorkspacePath::new("first").unwrap();
        let second_path = WorkspacePath::new("second").unwrap();
        let third_path = WorkspacePath::new("third").unwrap();
        let start = Instant::now();
        cache.insert_at(first_id.clone(), first_path.clone(), start);
        cache.insert_at(
            second_id.clone(),
            second_path.clone(),
            start + CACHE_CLOCK_STEP,
        );
        if refresh {
            assert_eq!(
                cache.get_path_at(&first_id, start + CACHE_CLOCK_STEP * 2),
                Some(&first_path)
            );
        }
        let now = start + CACHE_CLOCK_STEP * 3;
        assert_eq!(cache.get_path_at(&third_id, now), None);
        cache.insert_at(third_id.clone(), third_path.clone(), now);
        assert_eq!(
            cache.get_path_at(&first_id, now),
            refresh.then_some(&first_path)
        );
        assert_eq!(
            cache.get_path_at(&second_id, now),
            (!refresh).then_some(&second_path)
        );
        assert_eq!(cache.get_path_at(&third_id, now), Some(&third_path));
        assert_resource_cache_consistent(&cache);
    }

    #[test_case(0; "zero_capacity_clamps_to_one")]
    #[test_case(2; "replacement_removes_both_previous_aliases")]
    fn resource_cache_replacement_and_invalidation_retire_touch_records(limit: usize) {
        let mut cache = ResourceCache::new(limit, Duration::MAX);
        let old_id = ResourceId::new("old").unwrap();
        let id = ResourceId::new("resource").unwrap();
        let old_path = WorkspacePath::new("old").unwrap();
        let path = WorkspacePath::new("new").unwrap();
        cache.insert(old_id.clone(), path.clone());
        cache.insert(id.clone(), old_path.clone());
        cache.insert(id.clone(), path.clone());
        assert_eq!(cache.by_id.len(), 1);
        assert_eq!(cache.get_path(&old_id), None);
        assert_eq!(cache.id_for_path(&old_path), None);
        cache.remove_path(&old_path);
        cache.remove_id(&old_id);
        assert_eq!(cache.get_path(&id), Some(&path));
        assert_resource_cache_consistent(&cache);
        cache.remove_path(&path);
        assert_eq!(cache.get_path(&id), None);
        assert!(cache.by_id.is_empty());
        assert_resource_cache_consistent(&cache);
    }

    #[test_case(false; "repeated_insert")]
    #[test_case(true; "repeated_lookup")]
    fn resource_cache_refresh_index_stays_bounded(lookup: bool) {
        let mut cache = ResourceCache::new(1, Duration::MAX);
        let id = ResourceId::new("resource").unwrap();
        let path = WorkspacePath::new("file").unwrap();
        let start = Instant::now();
        cache.insert_at(id.clone(), path.clone(), start);
        for offset in 1..=CACHE_REGRESSION_ENTRIES {
            let now = start + Duration::from_nanos(offset as u64);
            if lookup {
                assert_eq!(cache.get_path_at(&id, now), Some(&path));
            } else {
                cache.insert_at(id.clone(), path.clone(), now);
            }
        }
        assert_eq!(cache.maintenance_probes, CACHE_REGRESSION_ENTRIES + 1);
        assert_eq!(cache.by_id.len(), 1);
        assert_resource_cache_consistent(&cache);
    }

    #[test_case(1; "single_expiry")]
    #[test_case(crate::remote::tests::CACHE_REGRESSION_ENTRIES; "bulk_expiry")]
    fn resource_cache_expiry_visits_each_retired_entry_once(count: usize) {
        let mut cache = ResourceCache::new(count, CACHE_CLOCK_STEP);
        let start = Instant::now();
        for index in 0..count {
            cache.insert_at(
                ResourceId::new(format!("resource-{index}")).unwrap(),
                WorkspacePath::new(format!("file-{index}")).unwrap(),
                start,
            );
        }
        assert_eq!(cache.maintenance_probes, count);
        cache.expire(start + CACHE_CLOCK_STEP);
        assert_eq!(cache.by_id.len(), count);
        assert_eq!(cache.maintenance_probes, count + 1);
        let now = start + CACHE_CLOCK_STEP + Duration::from_nanos(1);
        cache.expire(now);
        assert_eq!(cache.maintenance_probes, count * 2 + 2);
        assert!(cache.by_id.is_empty());
        assert_resource_cache_consistent(&cache);
        cache.expire(now);
        assert_eq!(cache.maintenance_probes, count * 2 + 3);
    }

    #[test_case(crate::remote::tests::CACHE_REGRESSION_ENTRIES; "all_entries_retained")]
    #[test_case(crate::remote::tests::CACHE_PRESSURE_LIMIT; "bounded_capacity_pressure")]
    fn metadata_cache_registration_and_lookup_have_linear_maintenance(limit: usize) {
        let endpoint = WorkcellEndpoint::parse("http://127.0.0.1:1/mcp").unwrap();
        let temp = tempfile::tempdir().unwrap();
        let client = snapshot_client(&endpoint, &StateDir::from_path(temp.path().join("state")));
        *client.0.paths.lock().unwrap() = ResourceCache::new(limit, Duration::MAX);
        let entries = (0..CACHE_REGRESSION_ENTRIES)
            .map(|index| contract::WorkspaceEntry {
                path: contract::WorkspacePath::new(format!("file-{index}")).unwrap(),
                resource_id: contract::ResourceId::new(format!("resource-{index}")).unwrap(),
                revision: None,
                kind: contract::WorkspaceEntryKind::File,
                size_bytes: Some(1),
            })
            .collect::<Vec<_>>();
        let started = Instant::now();
        for entry in &entries {
            assert!(client.remember_entry(entry).unwrap().revision.is_none());
        }
        let registration_time = started.elapsed();
        let retained = limit.min(CACHE_REGRESSION_ENTRIES);
        let evicted = CACHE_REGRESSION_ENTRIES - retained;
        let registration_probes = CACHE_REGRESSION_ENTRIES + evicted;
        let retained_ids = {
            let cache = client.0.paths.lock().unwrap();
            assert_eq!(cache.by_id.len(), retained);
            assert_eq!(cache.maintenance_probes, registration_probes);
            assert_resource_cache_consistent(&cache);
            cache.by_id.keys().cloned().collect::<HashSet<_>>()
        };
        let started = Instant::now();
        for entry in &entries {
            let id = ResourceId::new(entry.resource_id.as_str()).unwrap();
            let selected =
                client.selector_path(client.root_cursor(), &ResourceSelector::Id(id.clone()));
            if !retained_ids.contains(&id) {
                assert_eq!(
                    selected,
                    Err(WorkspaceError::StaleResource { resource_id: id })
                );
            } else {
                assert_eq!(selected.unwrap().as_str(), entry.path.as_str());
            }
        }
        let lookup_time = started.elapsed();
        let mut cache = client.0.paths.lock().unwrap();
        assert_eq!(
            cache.maintenance_probes,
            registration_probes + CACHE_REGRESSION_ENTRIES
        );
        assert_resource_cache_consistent(&cache);
        let started = Instant::now();
        for entry in &entries {
            cache.remove_path(&WorkspacePath::new(entry.path.as_str()).unwrap());
        }
        let invalidation_time = started.elapsed();
        assert!(cache.by_id.is_empty());
        assert_eq!(
            cache.maintenance_probes,
            registration_probes + CACHE_REGRESSION_ENTRIES
        );
        assert_resource_cache_consistent(&cache);
        eprintln!(
            "resource_cache entries={CACHE_REGRESSION_ENTRIES} limit={limit} registration={registration_time:?} lookup={lookup_time:?} invalidation={invalidation_time:?} maintenance_probes={}",
            cache.maintenance_probes
        );
    }

    #[test]
    fn id_selectors_require_exact_returned_identity_before_caching() {
        let expected = ResourceId::new("expected").unwrap();
        let returned = ResourceId::new("returned").unwrap();
        assert_eq!(
            validate_selector_id(&ResourceSelector::Id(expected), &returned),
            Err(WorkspaceError::IdentityMismatch)
        );
    }

    #[test]
    fn repository_relative_paths_are_joined_before_resource_caching() {
        assert_eq!(
            join_workspace_path(
                &WorkspacePath::new("project/repository").unwrap(),
                &WorkspacePath::new("src/lib.rs").unwrap()
            )
            .unwrap(),
            WorkspacePath::new("project/repository/src/lib.rs").unwrap()
        );
    }

    #[test]
    fn oversized_complete_sse_event_is_rejected_before_parsing() {
        let body = format!("data: {}\n\n", "x".repeat(MAX_SSE_EVENT_BYTES));
        let (endpoint, server) = serve_once(body, "text/event-stream");
        let (transport, _) = RemoteTransport::new(&endpoint, None).unwrap();
        let result =
            smol::block_on(transport.request("test", json!({}), 1024, &CancellationToken::new()));
        assert_eq!(result, Err(RemoteWorkcellError::InvalidProtocol));
        server.join().unwrap();
    }

    #[test]
    fn persistent_crlf_sse_stream_processes_events_in_wire_order() {
        let body = concat!(
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notice\",\"params\":{\"sequence\":1}}\r\n\r\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":$ID,\"result\":{\"ok\":true}}\r\n\r\n"
        )
        .to_owned();
        let (endpoint, server) = serve_once(body, "text/event-stream");
        let (transport, events) = RemoteTransport::new(&endpoint, None).unwrap();
        let result =
            smol::block_on(transport.request("test", json!({}), 1024, &CancellationToken::new()))
                .unwrap();
        assert_eq!(result, json!({"ok":true}));
        assert_eq!(events.recv().unwrap().method, "notice");
        server.join().unwrap();
    }

    #[test]
    fn mixed_sse_delimiters_do_not_skip_an_earlier_crlf_event() {
        let body = concat!(
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notice\"}\r\n\r\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":$ID,\"result\":{}}\n\n"
        )
        .to_owned();
        let (endpoint, server) = serve_once(body, "text/event-stream");
        let (transport, events) = RemoteTransport::new(&endpoint, None).unwrap();
        let result =
            smol::block_on(transport.request("test", json!({}), 1024, &CancellationToken::new()));
        assert_eq!(result, Ok(json!({})));
        assert_eq!(events.recv().unwrap().method, "notice");
        server.join().unwrap();
    }

    #[test]
    fn event_debug_redacts_params() {
        let event = RemoteEvent {
            method: "notice".to_owned(),
            params: Some(json!({"token":"secret"})),
        };
        let debug = format!("{event:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("secret"));
    }

    #[test]
    fn bounded_map_evicts_lru_entries() {
        let mut map = BoundedMap::new(2, std::time::Duration::from_secs(60));
        map.insert(1, "first");
        map.insert(2, "second");
        assert_eq!(map.get(&1), Some(&"first"));
        map.insert(3, "third");
        assert_eq!(map.len(), 2);
        assert!(map.get(&2).is_none());
        assert_eq!(map.get(&1), Some(&"first"));
        assert_eq!(map.get(&3), Some(&"third"));
    }
}
