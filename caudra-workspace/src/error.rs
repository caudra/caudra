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
    #[error("workspace authority is busy with another operation")]
    Busy,
    #[error("workspace directory is not a repository")]
    NotRepository,
    #[error("workspace watch backend is unavailable")]
    WatchUnavailable,
    /// A ceiling the request would pass, which no retry of the same request fixes.
    #[error("{}", exceeded("limit", "was exceeded", .limit, .maximum))]
    LimitExceeded {
        limit: Option<String>,
        maximum: Option<u64>,
    },
    /// Storage the authority keeps is full until something in it is released.
    #[error("{}", exceeded("quota", "is exhausted", .limit, .maximum))]
    QuotaExceeded {
        limit: Option<String>,
        maximum: Option<u64>,
    },
    #[error("workspace entry is not a plain file, directory or symlink the authority supports")]
    UnsupportedEntry,
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

/// Names the limit when the authority said which, and its maximum when it has one.
fn exceeded(kind: &str, verb: &str, limit: &Option<String>, maximum: &Option<u64>) -> String {
    match (limit, maximum) {
        (Some(limit), Some(maximum)) => format!("the workspace {limit} {kind} of {maximum} {verb}"),
        (Some(limit), None) => format!("the workspace {limit} {kind} {verb}"),
        (None, _) => format!("a workspace {kind} {verb}"),
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::WorkspaceError;

    #[test_case(WorkspaceError::LimitExceeded { limit: Some("files".into()), maximum: Some(1) }, "the workspace files limit of 1 was exceeded" ; "limit_with_maximum")]
    #[test_case(WorkspaceError::LimitExceeded { limit: Some("ignoreRules".into()), maximum: None }, "the workspace ignoreRules limit was exceeded" ; "limit_without_maximum")]
    #[test_case(WorkspaceError::LimitExceeded { limit: None, maximum: None }, "a workspace limit was exceeded" ; "unnamed_limit")]
    #[test_case(WorkspaceError::QuotaExceeded { limit: Some("storageBytes".into()), maximum: Some(2) }, "the workspace storageBytes quota of 2 is exhausted" ; "quota_with_maximum")]
    #[test_case(WorkspaceError::QuotaExceeded { limit: None, maximum: None }, "a workspace quota is exhausted" ; "unnamed_quota")]
    fn a_refused_ceiling_names_itself_and_its_maximum(error: WorkspaceError, expected: &str) {
        assert_eq!(error.to_string(), expected);
    }

    #[test_case(WorkspaceError::NotRepository, "not_repository"; "not_repository")]
    #[test_case(WorkspaceError::WatchUnavailable, "watch_unavailable"; "watch_unavailable")]
    fn availability_errors_round_trip_without_private_details(error: WorkspaceError, code: &str) {
        let value = serde_json::json!({"error": code});
        assert_eq!(serde_json::to_value(&error).unwrap(), value);
        assert_eq!(
            serde_json::from_value::<WorkspaceError>(value).unwrap(),
            error
        );
    }
}
