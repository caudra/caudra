use std::fmt;

use async_trait::async_trait;
use futures_lite::io::AsyncRead;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    ByteRange, MutationCondition, OperationHandle, OperationId, OperationStatus, ReleaseResult,
    ResourceId, ResourceRevision, SessionWorkspaceBinding, WorkspaceCursor, WorkspaceError,
    WorkspacePath,
};

const SHA256_HEX_LENGTH: usize = 64;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TransferDigest(String);

impl TransferDigest {
    pub fn new(value: impl Into<String>) -> Result<Self, WorkspaceError> {
        let value = value.into();
        let valid = value.strip_prefix("sha256:").is_some_and(|hex| {
            hex.len() == SHA256_HEX_LENGTH
                && hex
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        });
        if !valid {
            return Err(WorkspaceError::TransferIntegrity);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for TransferDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("TransferDigest(<redacted>)")
    }
}

impl TryFrom<String> for TransferDigest {
    type Error = WorkspaceError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<TransferDigest> for String {
    fn from(value: TransferDigest) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransferMode {
    Regular,
    Executable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferContent {
    pub digest: TransferDigest,
    pub size_bytes: u64,
    pub mode: TransferMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferLimits {
    pub max_file_bytes: u64,
    pub max_stages: u32,
    pub max_reserved_bytes: u64,
    pub max_concurrent_io: u32,
    pub stream_buffer_bytes: u32,
    pub atomic_replace_against_external_writers: bool,
}

/// A client-owned stream, never a workspace path or a remotely supplied host filename.
pub struct LocalTransferSource(Box<dyn AsyncRead + Unpin + Send>);

impl LocalTransferSource {
    pub fn new(reader: impl AsyncRead + Unpin + Send + 'static) -> Self {
        Self(Box::new(reader))
    }

    pub fn into_reader(self) -> Box<dyn AsyncRead + Unpin + Send> {
        self.0
    }
}

impl fmt::Debug for LocalTransferSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LocalTransferSource(<private stream>)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteTransferStage {
    pub id: OperationId,
    pub binding: SessionWorkspaceBinding,
    pub cursor: WorkspaceCursor,
    pub content: TransferContent,
    pub expires_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedTransfer {
    pub stage: RemoteTransferStage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteTransferFile {
    pub binding: SessionWorkspaceBinding,
    pub cursor: WorkspaceCursor,
    /// Relative to the captured remote cursor, not to the client's working directory.
    pub path: WorkspacePath,
    pub resource_id: ResourceId,
    pub revision: ResourceRevision,
    pub content: TransferContent,
}

#[derive(Debug)]
pub struct DownloadedTransfer {
    pub source: LocalTransferSource,
    pub content: TransferContent,
    pub range: ByteRange,
    /// A partial range has its own digest, but cannot verify the whole remote file digest.
    pub whole_file_verified: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferPublicationRequest {
    pub publication_id: OperationId,
    pub path: WorkspacePath,
    pub condition: MutationCondition,
    pub create_directories: Vec<WorkspacePath>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct PreparedTransferPublication {
    pub operation: OperationHandle,
    /// Remote cwd retained to rebind process-local handles for durable status queries.
    pub cwd_path: WorkspacePath,
    pub request_digest: TransferDigest,
    pub sealed: SealedTransfer,
    pub request: TransferPublicationRequest,
    pub review: Value,
}

impl fmt::Debug for PreparedTransferPublication {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedTransferPublication")
            .field("operation", &self.operation)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransferPublicationState {
    Prepared,
    Publishing,
    Completed,
    Failed,
    Cancelled,
    Indeterminate,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferPublicationStatus {
    pub publication_id: OperationId,
    pub state: TransferPublicationState,
    pub file: Option<RemoteTransferFile>,
    pub created_directories: Vec<(WorkspacePath, ResourceId)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryPublicationRequest {
    pub publication_id: OperationId,
    pub path: WorkspacePath,
    pub create_directories: Vec<WorkspacePath>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PreparedDirectoryPublication {
    pub operation: OperationHandle,
    pub binding: SessionWorkspaceBinding,
    pub cursor: WorkspaceCursor,
    pub cwd_path: WorkspacePath,
    pub request_digest: TransferDigest,
    pub request: DirectoryPublicationRequest,
    pub review: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedLocalDirectory {
    pub request: DirectoryPublicationRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishedTransferDirectory {
    pub path: WorkspacePath,
    pub resource_id: ResourceId,
    pub created_directories: Vec<(WorkspacePath, ResourceId)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryPublicationStatus {
    pub publication_id: OperationId,
    pub state: TransferPublicationState,
    pub directory: Option<PublishedTransferDirectory>,
}

#[async_trait]
pub trait WorkspaceTransferService: Send + Sync {
    fn supports_directory_publication(&self) -> bool {
        false
    }
    async fn prepare_directory(
        &self,
        _binding: &SessionWorkspaceBinding,
        _cursor: &WorkspaceCursor,
        _request: &DirectoryPublicationRequest,
    ) -> Result<PreparedDirectoryPublication, WorkspaceError> {
        Err(WorkspaceError::UnsupportedEntry)
    }
    async fn execute_directory(
        &self,
        _prepared: &PreparedDirectoryPublication,
    ) -> Result<OperationStatus<DirectoryPublicationStatus>, WorkspaceError> {
        Err(WorkspaceError::UnsupportedEntry)
    }
    async fn directory_status(
        &self,
        _prepared: &PreparedDirectoryPublication,
    ) -> Result<DirectoryPublicationStatus, WorkspaceError> {
        Err(WorkspaceError::UnsupportedEntry)
    }
    async fn release_directory(
        &self,
        _prepared: &PreparedDirectoryPublication,
    ) -> Result<ReleaseResult, WorkspaceError> {
        Err(WorkspaceError::UnsupportedEntry)
    }

    fn limits(&self) -> Result<TransferLimits, WorkspaceError>;

    async fn stage(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        source: LocalTransferSource,
        expected: &TransferContent,
    ) -> Result<RemoteTransferStage, WorkspaceError>;
    async fn seal(&self, stage: &RemoteTransferStage) -> Result<SealedTransfer, WorkspaceError>;
    async fn release_stage(&self, stage: &RemoteTransferStage) -> Result<bool, WorkspaceError>;
    async fn stat(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        path: &WorkspacePath,
    ) -> Result<RemoteTransferFile, WorkspaceError>;
    async fn download(
        &self,
        file: &RemoteTransferFile,
        range: Option<ByteRange>,
    ) -> Result<DownloadedTransfer, WorkspaceError>;
    async fn prepare_publication(
        &self,
        sealed: &SealedTransfer,
        request: &TransferPublicationRequest,
    ) -> Result<PreparedTransferPublication, WorkspaceError>;
    async fn execute_publication(
        &self,
        prepared: &PreparedTransferPublication,
    ) -> Result<OperationStatus<TransferPublicationStatus>, WorkspaceError>;
    async fn publication_status(
        &self,
        prepared: &PreparedTransferPublication,
    ) -> Result<TransferPublicationStatus, WorkspaceError>;
    async fn release_publication(
        &self,
        prepared: &PreparedTransferPublication,
    ) -> Result<ReleaseResult, WorkspaceError>;
}

/// An explicitly selected path under a local publisher's root. No conversion from WorkspacePath.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct LocalTransferPath(String);

impl LocalTransferPath {
    pub fn new(path: impl Into<String>) -> Result<Self, WorkspaceError> {
        let path = path.into();
        if path.is_empty()
            || path.contains(['\\', '\0', ':'])
            || path
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
        {
            return Err(WorkspaceError::PermissionDenied);
        }
        Ok(Self(path))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for LocalTransferPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LocalTransferPath(<private>)")
    }
}

impl TryFrom<String> for LocalTransferPath {
    type Error = WorkspaceError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<LocalTransferPath> for String {
    fn from(value: LocalTransferPath) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalTransferRevision(pub ResourceRevision);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LocalTransferCondition {
    MustNotExist,
    Matches(LocalTransferRevision),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalTransferDestination {
    pub path: LocalTransferPath,
    pub condition: LocalTransferCondition,
    pub create_directories: Vec<WorkspacePath>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalTransferReview {
    pub destination: LocalTransferDestination,
    pub content: TransferContent,
    pub atomic_replace_against_external_writers: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedLocalTransfer {
    pub id: OperationId,
    pub review: LocalTransferReview,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LocalPublicationState {
    Prepared,
    Publishing,
    /// Durable publication evidence reconciled with current bytes and executable bit. An
    /// external writer producing identical bytes cannot be distinguished from our publication.
    Completed(LocalTransferRevision),
    NotPublished,
    Indeterminate,
    Unknown,
}

#[async_trait]
pub trait LocalTransferAuthorization: Send + Sync {
    async fn authorize_directory(
        &self,
        _request: &DirectoryPublicationRequest,
    ) -> Result<(), WorkspaceError> {
        Err(WorkspaceError::PermissionDenied)
    }
    /// Independent host approval; remote approval must never authorize local publication.
    async fn authorize(&self, review: &LocalTransferReview) -> Result<(), WorkspaceError>;
}

#[async_trait]
pub trait LocalTransferService: Send + Sync {
    fn supports_directory_publication(&self) -> bool {
        false
    }
    async fn prepare_directory(
        &self,
        _request: &DirectoryPublicationRequest,
    ) -> Result<PreparedLocalDirectory, WorkspaceError> {
        Err(WorkspaceError::UnsupportedEntry)
    }
    async fn execute_directory(
        &self,
        _prepared: &PreparedLocalDirectory,
    ) -> Result<DirectoryPublicationStatus, WorkspaceError> {
        Err(WorkspaceError::UnsupportedEntry)
    }
    async fn directory_status(
        &self,
        _prepared: &PreparedLocalDirectory,
    ) -> Result<DirectoryPublicationStatus, WorkspaceError> {
        Err(WorkspaceError::UnsupportedEntry)
    }
    async fn release_directory(
        &self,
        _prepared: &PreparedLocalDirectory,
    ) -> Result<(), WorkspaceError> {
        Err(WorkspaceError::UnsupportedEntry)
    }

    async fn created_directories(
        &self,
        _prepared: &PreparedLocalTransfer,
    ) -> Result<Vec<(WorkspacePath, ResourceId)>, WorkspaceError> {
        Ok(Vec::new())
    }

    /// Status never replays publication. Completed requires durable evidence and a current
    /// matching destination; ambiguous mutation is not a no-side-effects proof.
    async fn publication_status(
        &self,
        _prepared: &PreparedLocalTransfer,
    ) -> Result<LocalPublicationState, WorkspaceError> {
        Ok(LocalPublicationState::Unknown)
    }
    async fn stat(
        &self,
        path: &LocalTransferPath,
    ) -> Result<(LocalTransferRevision, TransferContent), WorkspaceError>;
    async fn prepare(
        &self,
        source: LocalTransferSource,
        destination: LocalTransferDestination,
        expected: TransferContent,
    ) -> Result<PreparedLocalTransfer, WorkspaceError>;
    /// An indeterminate error means publication may have happened. Do not retry blindly.
    async fn execute(
        &self,
        prepared: &PreparedLocalTransfer,
    ) -> Result<LocalTransferRevision, WorkspaceError>;
    async fn release(&self, prepared: &PreparedLocalTransfer) -> Result<(), WorkspaceError>;
}

#[cfg(test)]
mod tests {
    use super::{
        LocalTransferDestination, LocalTransferPath, TransferDigest, TransferPublicationRequest,
        TransferPublicationStatus,
    };
    use serde_json::{Value, json};
    use test_case::test_case;

    #[test_case("request", "create_directories"; "remote_publication")]
    #[test_case("status", "created_directories"; "remote_recovery")]
    #[test_case("local", "create_directories"; "local_publication")]
    fn directory_metadata_is_required(kind: &str, field: &str) {
        let mut value = match kind {
            "request" => {
                json!({"publication_id":"publication","path":"file","condition":{"kind":"must_not_exist"},"create_directories":[]})
            }
            "status" => {
                json!({"publication_id":"publication","state":"Unknown","file":null,"created_directories":[]})
            }
            _ => {
                json!({"path":"file","condition":"MustNotExist","create_directories":[]})
            }
        };
        let valid = |value: Value| match kind {
            "request" => serde_json::from_value::<TransferPublicationRequest>(value).is_ok(),
            "status" => serde_json::from_value::<TransferPublicationStatus>(value).is_ok(),
            _ => serde_json::from_value::<LocalTransferDestination>(value).is_ok(),
        };
        assert!(valid(value.clone()));
        value.as_object_mut().unwrap().remove(field);
        assert!(!valid(value));
    }

    #[test_case("/tmp/local-canary"; "absolute_host_path")]
    #[test_case("../canary"; "parent_escape")]
    #[test_case("a/../canary"; "embedded_parent_escape")]
    #[test_case("a//canary"; "empty_component")]
    #[test_case("C:\\canary"; "drive_prefix")]
    #[test_case("canary\0"; "nul")]
    fn local_destinations_require_an_explicit_relative_path(path: &str) {
        assert!(LocalTransferPath::new(path).is_err());
    }

    #[test_case("sha256:short"; "short_digest")]
    #[test_case("sha256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"; "uppercase_digest")]
    #[test_case("sha256:000000000000000000000000000000000000000000000000000000000000000g"; "invalid_hex")]
    fn transfer_digests_fail_closed_even_on_deserialization(digest: &str) {
        assert!(TransferDigest::new(digest).is_err());
        assert!(serde_json::from_value::<TransferDigest>(serde_json::json!(digest)).is_err());
    }
}
