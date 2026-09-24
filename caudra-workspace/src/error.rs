use serde::{Deserialize, Serialize};

use crate::{ResourceId, WorkspaceCapability};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvalidResponseKind {
    Malformed,
    MissingField,
    InvalidIdentity,
    InvalidPath,
    InvalidCursor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportErrorKind {
    Disconnected,
    Timeout,
    Protocol,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "error", rename_all = "snake_case")]
/// Stable errors safe to serialize and display across the workspace protocol boundary.
///
/// Adapters should log private diagnostics at the transport boundary rather than embedding
/// backend messages, credentials, or host paths in these variants.
pub enum WorkspaceError {
    #[error("workspace authority is unavailable")]
    Unavailable,
    #[error("workspace capability {capability:?} is unsupported")]
    UnsupportedCapability { capability: WorkspaceCapability },
    #[error("workspace capability {capability:?} is inconsistent with its service or dependencies")]
    CapabilityMismatch { capability: WorkspaceCapability },
    #[error("workspace resource is stale")]
    StaleResource { resource_id: ResourceId },
    #[error("workspace cursor is stale")]
    StaleCursor,
    #[error("workspace identity does not match the authority or binding")]
    IdentityMismatch,
    #[error("workspace operation is not permitted")]
    PermissionDenied,
    #[error("workspace operation was denied by policy")]
    PolicyDenied,
    #[error("workspace operation conflicts with current state")]
    Conflict,
    #[error("transfer content does not match its expected digest or size")]
    TransferIntegrity,
    #[error("transfer staging or I/O quota is exhausted")]
    TransferQuota,
    #[error("remote operation {operation_id} is still awaiting reconciliation")]
    PendingOperation { operation_id: String },
    #[error("workspace operation was cancelled")]
    Cancelled,
    #[error("workspace operation outcome is indeterminate")]
    IndeterminateOutcome,
    #[error("workspace response exceeds the {limit_bytes}-byte limit")]
    ResponseTooLarge { limit_bytes: u64 },
    #[error("workspace response is invalid: {violation:?}")]
    InvalidResponse { violation: InvalidResponseKind },
    /// A well-formed refusal whose reason this client has no mapping for.
    ///
    /// Distinct from `InvalidResponse`: the authority answered correctly and
    /// said why, and reporting that as malformed would blame the wire for a
    /// decision the authority made deliberately.
    #[error("workspace authority refused the request with code {code}, reason {symbolic}")]
    Refused { code: i64, symbolic: String },
    #[error("workspace transport failed: {kind:?}")]
    Transport { kind: TransportErrorKind },
}
