use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    AuthorityIdentity, CheckpointId, CollectionRevision, ContinuationToken, DirectoryNavigation,
    OperationId, ResourceId, ResourceRevision, RestoreId, ScmRevision, SessionWorkspaceBinding,
    SnapshotId, WatchCursor, WatchSubscriptionId, WorkspaceCapabilities, WorkspaceCapability,
    WorkspaceCursor, WorkspaceError, WorkspacePath, WorkspaceResource, WorkspaceTransferService,
};

const MAX_COMMAND_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum ResourceSelector {
    Current,
    Id(ResourceId),
    Path(WorkspacePath),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListRequest {
    pub parent: ResourceSelector,
    pub recursive: bool,
    pub continuation: Option<ContinuationToken>,
    pub limit: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListPage {
    pub revision: CollectionRevision,
    pub resources: Vec<WorkspaceResource>,
    pub truncated: bool,
    pub incomplete: bool,
    pub continuation: Option<ContinuationToken>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextRange {
    pub start_line: u32,
    pub end_line: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadTextRequest {
    pub resource: ResourceSelector,
    pub range: Option<TextRange>,
    pub byte_offset: u64,
    pub max_bytes: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextContent {
    pub text: String,
    pub resource_id: ResourceId,
    pub revision: ResourceRevision,
    pub path: WorkspacePath,
    pub start_line: u32,
    pub end_line: u32,
    pub total_lines: u32,
    pub start_byte: u64,
    pub end_byte: u64,
    pub truncated: bool,
    pub next_byte_offset: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadBytesRequest {
    pub resource: ResourceSelector,
    pub byte_offset: u64,
    pub max_bytes: u64,
    pub if_revision: Option<ResourceRevision>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ByteRange {
    pub start: u64,
    pub end_exclusive: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ByteContent {
    pub bytes: Vec<u8>,
    pub resource_id: ResourceId,
    pub revision: ResourceRevision,
    pub range: ByteRange,
    pub total_bytes: Option<u64>,
    pub truncated: bool,
    pub next_byte_offset: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedWorkspaceDirectory {
    pub resource: WorkspaceResource,
    pub cursor: WorkspaceCursor,
}

#[async_trait]
pub trait WorkspaceReadService: Send + Sync {
    async fn resolve(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        path: &WorkspacePath,
    ) -> Result<WorkspaceResource, WorkspaceError>;

    async fn resolve_directory(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        path: &WorkspacePath,
    ) -> Result<ResolvedWorkspaceDirectory, WorkspaceError>;

    async fn navigate_directory(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        navigation: &DirectoryNavigation,
    ) -> Result<ResolvedWorkspaceDirectory, WorkspaceError> {
        let path =
            WorkspacePath::new(navigation.as_str()).map_err(|_| WorkspaceError::Unavailable)?;
        self.resolve_directory(binding, cursor, &path).await
    }

    async fn stat(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        resource: &ResourceSelector,
    ) -> Result<WorkspaceResource, WorkspaceError>;

    async fn list(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &ListRequest,
    ) -> Result<ListPage, WorkspaceError>;

    async fn read_text(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &ReadTextRequest,
    ) -> Result<TextContent, WorkspaceError>;

    async fn read_bytes(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &ReadBytesRequest,
    ) -> Result<ByteContent, WorkspaceError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "revision")]
pub enum MutationCondition {
    MustNotExist,
    Matches(ResourceRevision),
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "content")]
pub enum WriteContent {
    Text(String),
    Bytes(Vec<u8>),
}

impl fmt::Debug for WriteContent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (kind, size) = match self {
            Self::Text(text) => ("text", text.len()),
            Self::Bytes(bytes) => ("bytes", bytes.len()),
        };
        formatter
            .debug_struct("WriteContent")
            .field("kind", &kind)
            .field("size_bytes", &size)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Mutation {
    Write {
        path: WorkspacePath,
        content: WriteContent,
        condition: MutationCondition,
    },
    CreateDirectory {
        path: WorkspacePath,
    },
    Remove {
        path: WorkspacePath,
        expected_revision: ResourceRevision,
    },
    Move {
        source: WorkspacePath,
        destination: WorkspacePath,
        expected_revision: ResourceRevision,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutationRequest {
    pub mutations: Vec<Mutation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationKind {
    Create,
    Write,
    CreateDirectory,
    Move,
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutationEntryResult {
    pub kind: MutationKind,
    pub path: WorkspacePath,
    pub destination: Option<WorkspacePath>,
    pub revision: Option<ResourceRevision>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutationResult {
    pub committed: bool,
    pub rolled_back: bool,
    pub atomic_across_files: bool,
    pub results: Vec<MutationEntryResult>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    pub include: Option<String>,
    pub root: ResourceSelector,
    pub max_results: u32,
    pub continuation: Option<ContinuationToken>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
    pub resource: WorkspaceResource,
    pub line: u32,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchScanCounts {
    pub files_scanned: u32,
    pub files_listed: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchPage {
    pub revision: CollectionRevision,
    pub hits: Vec<SearchHit>,
    pub scan_counts: SearchScanCounts,
    pub truncated: bool,
    pub incomplete: bool,
    pub continuation: Option<ContinuationToken>,
}

#[async_trait]
pub trait WorkspaceSearchService: Send + Sync {
    async fn search(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &SearchRequest,
    ) -> Result<SearchPage, WorkspaceError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchOpenRequest {
    pub root: ResourceSelector,
    pub recursive: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchResyncReason {
    CursorInvalid,
    InstanceChanged,
    Overflow,
    BackendError,
    RetentionLost,
    SubscriptionExpired,
    SubscriptionClosed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchSubscription {
    pub subscription_id: WatchSubscriptionId,
    pub cursor: WatchCursor,
    pub expires_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchPollRequest {
    pub subscription_id: WatchSubscriptionId,
    pub cursor: WatchCursor,
    pub max_events: u32,
    pub max_bytes: u32,
    pub wait_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceEventKind {
    Created,
    Changed,
    Removed,
    Rescan,
    Renamed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceEvent {
    pub sequence: u64,
    pub kind: WorkspaceEventKind,
    pub path: WorkspacePath,
    pub previous_path: Option<WorkspacePath>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SequenceMetadata {
    pub first_retained_sequence: Option<u64>,
    pub next_sequence: u64,
    pub gap_before_first: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum WatchPollState {
    Current {
        next_cursor: WatchCursor,
        expires_at_unix_ms: u64,
    },
    FullResync {
        reason: WatchResyncReason,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchEventPage {
    pub subscription_id: WatchSubscriptionId,
    pub state: WatchPollState,
    pub sequence: SequenceMetadata,
    pub events: Vec<WorkspaceEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchCloseResult {
    pub subscription_id: WatchSubscriptionId,
    pub closed: bool,
}

#[async_trait]
pub trait WorkspaceWatchService: Send + Sync {
    async fn open(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &WatchOpenRequest,
    ) -> Result<WatchSubscription, WorkspaceError>;

    async fn poll(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &WatchPollRequest,
    ) -> Result<WatchEventPage, WorkspaceError>;

    async fn close(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        subscription_id: &WatchSubscriptionId,
    ) -> Result<WatchCloseResult, WorkspaceError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationHandle {
    pub preparation_id: OperationId,
    pub invocation_id: Option<OperationId>,
    pub execution_id: Option<OperationId>,
    pub expires_at_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationError {
    pub code: OperationId,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "name")]
pub enum OperationProgressKind {
    Started,
    Stdout,
    Stderr,
    Exited,
    Unknown(OperationId),
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationProgress {
    pub execution_id: OperationId,
    pub sequence: u64,
    pub kind: OperationProgressKind,
    pub chunk: String,
}

impl fmt::Debug for OperationProgress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OperationProgress")
            .field("execution_id", &self.execution_id)
            .field("sequence", &self.sequence)
            .field("kind", &self.kind)
            .field("chunk_bytes", &self.chunk.len())
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum OperationState<T> {
    NeverSeen,
    Prepared,
    Running,
    Completed {
        result: T,
        side_effects_possible: bool,
    },
    Failed {
        error: OperationError,
        side_effects_possible: bool,
    },
    Cancelled {
        side_effects_possible: bool,
    },
    Forgotten,
    Indeterminate {
        side_effects_possible: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperationStatus<T> {
    pub handle: OperationHandle,
    pub state: OperationState<T>,
    pub progress: Vec<OperationProgress>,
    pub progress_metadata: SequenceMetadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancellationResult {
    pub state: OperationPhase,
    pub cancellation_requested: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseResult {
    pub state: OperationPhase,
    pub released: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationPhase {
    NeverSeen,
    Prepared,
    Running,
    Completed,
    Failed,
    Cancelled,
    Forgotten,
    Indeterminate,
}

#[async_trait]
pub trait WorkspaceMutationService: Send + Sync {
    async fn execute(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &MutationRequest,
    ) -> Result<OperationStatus<MutationResult>, WorkspaceError>;

    async fn status(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        operation: &OperationHandle,
    ) -> Result<OperationStatus<MutationResult>, WorkspaceError>;

    async fn cancel(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        operation: &OperationHandle,
    ) -> Result<CancellationResult, WorkspaceError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CommandTextError {
    #[error("command text must not be empty")]
    Empty,
    #[error("command text exceeds {MAX_COMMAND_BYTES} bytes")]
    TooLong,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CommandText(String);

impl CommandText {
    pub fn new(command: impl Into<String>) -> Result<Self, CommandTextError> {
        let command = command.into();
        if command.is_empty() {
            return Err(CommandTextError::Empty);
        }
        if command.len() > MAX_COMMAND_BYTES {
            return Err(CommandTextError::TooLong);
        }
        Ok(Self(command))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for CommandText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CommandText")
            .field("value", &"<redacted>")
            .field("size_bytes", &self.0.len())
            .finish()
    }
}

impl TryFrom<String> for CommandText {
    type Error = CommandTextError;

    fn try_from(command: String) -> Result<Self, Self::Error> {
        Self::new(command)
    }
}

impl From<CommandText> for String {
    fn from(command: CommandText) -> Self {
        command.0
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecRequest {
    pub command: CommandText,
    pub timeout_ms: Option<u64>,
}

impl fmt::Debug for ExecRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExecRequest")
            .field("command", &"<redacted>")
            .field("command_bytes", &self.command.as_str().len())
            .field("timeout_ms", &self.timeout_ms)
            .finish()
    }
}

#[async_trait]
pub trait WorkspaceExecService: Send + Sync {
    async fn execute(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &ExecRequest,
    ) -> Result<OperationStatus<Value>, WorkspaceError>;

    async fn status(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        operation: &OperationHandle,
    ) -> Result<OperationStatus<Value>, WorkspaceError>;

    async fn cancel(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        operation: &OperationHandle,
    ) -> Result<CancellationResult, WorkspaceError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScmRepositoryRevisions {
    pub repository: ScmRevision,
    pub head: ScmRevision,
    pub index: ScmRevision,
    pub worktree: ScmRevision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScmRepository {
    pub handle: ResourceId,
    pub resource_id: ResourceId,
    pub root: WorkspacePath,
    pub identity: ScmRevision,
    pub revisions: ScmRepositoryRevisions,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScmDiscoverRequest {
    pub path: WorkspacePath,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScmDiscoverResult {
    pub repository: ScmRepository,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScmChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Unmerged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScmStatusEntry {
    pub path: WorkspacePath,
    pub staged: Option<ScmChangeKind>,
    pub unstaged: Option<ScmChangeKind>,
    pub untracked: bool,
    pub conflicted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScmStatusRequest {
    pub repository_handle: ResourceId,
    pub page_size: u32,
    pub continuation: Option<ContinuationToken>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScmStatusPage {
    pub revisions: ScmRepositoryRevisions,
    pub collection_revision: CollectionRevision,
    pub entries: Vec<ScmStatusEntry>,
    pub truncated: bool,
    pub incomplete: bool,
    pub continuation: Option<ContinuationToken>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScmCommit {
    pub id: ScmRevision,
    pub parents: Vec<ScmRevision>,
    pub author_name: String,
    pub author_email: String,
    pub committed_unix_seconds: i64,
    pub summary: String,
    /// The message past its subject line. `None` means the workspace did not
    /// report one, which is not the same as a commit whose message is a
    /// subject and nothing else.
    pub body: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScmLogRequest {
    pub repository_handle: ResourceId,
    pub page_size: u32,
    pub continuation: Option<ContinuationToken>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScmLogPage {
    pub head_revision: ScmRevision,
    pub collection_revision: CollectionRevision,
    pub commits: Vec<ScmCommit>,
    pub truncated: bool,
    pub incomplete: bool,
    pub continuation: Option<ContinuationToken>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ScmDiffTarget {
    Staged,
    Unstaged,
    Tree {
        base: ScmRevision,
        target: ScmRevision,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScmDiffLineKind {
    File,
    Context,
    Addition,
    Deletion,
    Binary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScmDiffLine {
    pub path: WorkspacePath,
    pub kind: ScmDiffLineKind,
    pub change: Option<ScmChangeKind>,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScmDiffRequest {
    pub repository_handle: ResourceId,
    pub target: ScmDiffTarget,
    pub path: Option<WorkspacePath>,
    pub max_lines: u32,
    pub max_bytes: u32,
    pub continuation: Option<ContinuationToken>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScmDiffPage {
    pub repository_revision: ScmRevision,
    pub collection_revision: CollectionRevision,
    pub lines: Vec<ScmDiffLine>,
    pub truncated: bool,
    pub incomplete: bool,
    pub continuation: Option<ContinuationToken>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ScmSide {
    Head,
    Index,
    Worktree,
    Commit { revision: ScmRevision },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScmReadSideRequest {
    pub repository_handle: ResourceId,
    pub path: WorkspacePath,
    pub side: ScmSide,
    pub start_line: u32,
    pub max_lines: u32,
    pub max_bytes: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScmReadSidePage {
    pub repository_revision: ScmRevision,
    pub resource_id: ResourceId,
    pub revision: ResourceRevision,
    pub path: WorkspacePath,
    pub side: ScmSide,
    pub content: String,
    pub start_line: u32,
    pub end_line: u32,
    pub total_lines: u32,
    pub truncated: bool,
    pub incomplete: bool,
    pub next_start_line: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ScmMutation {
    Stage { paths: Vec<WorkspacePath> },
    Unstage { paths: Vec<WorkspacePath> },
    Discard { paths: Vec<WorkspacePath> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScmMutationPreview {
    pub mutation: ScmMutation,
    pub repository_identity: ScmRevision,
    pub revisions: ScmRepositoryRevisions,
    pub entries: Vec<ScmStatusEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedScmMutation {
    pub operation: OperationHandle,
    pub preview: ScmMutationPreview,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScmMutationResult {
    pub mutation: ScmMutation,
    pub revisions: ScmRepositoryRevisions,
}

#[async_trait]
pub trait WorkspaceScmReadService: Send + Sync {
    async fn discover(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &ScmDiscoverRequest,
    ) -> Result<ScmDiscoverResult, WorkspaceError>;

    async fn status(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &ScmStatusRequest,
    ) -> Result<ScmStatusPage, WorkspaceError>;

    async fn log(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &ScmLogRequest,
    ) -> Result<ScmLogPage, WorkspaceError>;

    async fn diff(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &ScmDiffRequest,
    ) -> Result<ScmDiffPage, WorkspaceError>;

    async fn read_side(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &ScmReadSideRequest,
    ) -> Result<ScmReadSidePage, WorkspaceError>;
}

#[async_trait]
pub trait WorkspaceScmMutationService: Send + Sync {
    async fn prepare(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        repository_handle: &ResourceId,
        mutation: &ScmMutation,
    ) -> Result<PreparedScmMutation, WorkspaceError>;

    async fn execute(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        prepared: &PreparedScmMutation,
    ) -> Result<OperationStatus<ScmMutationResult>, WorkspaceError>;

    async fn status(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        operation: &OperationHandle,
    ) -> Result<OperationStatus<ScmMutationResult>, WorkspaceError>;

    async fn cancel(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        operation: &OperationHandle,
    ) -> Result<CancellationResult, WorkspaceError>;

    async fn release(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        prepared: &PreparedScmMutation,
    ) -> Result<ReleaseResult, WorkspaceError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotState {
    Complete,
    Corrupt,
}

/// Why a capture left an entry out. A restore never touches an entry left out of either side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotSkipReason {
    NestedRepository,
    Mount,
    Special,
    Oversized,
    Unreadable,
    Unstable,
    Unrepresentable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotSkippedEntry {
    /// Lossy for an unrepresentable name, so it is for display only.
    pub path: String,
    pub reason: SnapshotSkipReason,
}

/// Complete counts of what a capture left out, beside a bounded sample of the paths.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotSkipped {
    pub nested_repositories: u32,
    pub mounts: u32,
    pub special_files: u32,
    pub oversized_files: u32,
    pub unreadable_entries: u32,
    pub unstable_files: u32,
    pub unrepresentable_names: u32,
    pub samples: Vec<SnapshotSkippedEntry>,
}

impl SnapshotSkipped {
    pub fn total(&self) -> u64 {
        [
            self.nested_repositories,
            self.mounts,
            self.special_files,
            self.oversized_files,
            self.unreadable_entries,
            self.unstable_files,
            self.unrepresentable_names,
        ]
        .into_iter()
        .map(u64::from)
        .sum()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotSummary {
    pub snapshot_id: SnapshotId,
    pub checkpoint_id: Option<CheckpointId>,
    pub label: Option<String>,
    pub state: SnapshotState,
    pub manifest_revision: ResourceRevision,
    /// The directory the capture covers; a restore never reaches outside it.
    pub scope: WorkspacePath,
    pub file_count: u32,
    pub total_bytes: u64,
    pub skipped: SnapshotSkipped,
    pub created_at_unix_ms: u64,
}

/// Ceilings for one capture. An authority clamps them to its own, so a caller
/// states what it is willing to pay rather than what the authority allows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotCaptureLimits {
    /// Above it the workspace is refused rather than captured.
    pub max_files: u64,
    /// A larger file is left out of the capture and never restored over.
    pub max_file_bytes: u64,
    /// Above it the workspace is refused rather than captured.
    pub max_total_bytes: u64,
}

/// Captures the directory the cursor names, the session's working directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotCaptureRequest {
    pub checkpoint_id: CheckpointId,
    pub label: Option<String>,
    pub limits: SnapshotCaptureLimits,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotCaptureResult {
    pub snapshot: SnapshotSummary,
    pub reused_checkpoint: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotInspectRequest {
    pub snapshot_id: SnapshotId,
    pub page_size: u32,
    pub continuation: Option<ContinuationToken>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotEntryKind {
    File,
    /// Captured as the link itself: `digest` covers the raw target and it is never followed.
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotFile {
    pub path: WorkspacePath,
    pub resource_id: ResourceId,
    pub kind: SnapshotEntryKind,
    pub digest: ResourceRevision,
    pub mode: u32,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotInspectPage {
    pub snapshot: SnapshotSummary,
    pub files: Vec<SnapshotFile>,
    pub exclusions: Vec<WorkspacePath>,
    pub truncated: bool,
    pub incomplete: bool,
    pub continuation: Option<ContinuationToken>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotChangeKind {
    Create,
    Replace,
    Delete,
    Conflict,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotChange {
    pub path: WorkspacePath,
    pub resource_id: ResourceId,
    pub kind: SnapshotChangeKind,
    pub current_revision: Option<ResourceRevision>,
    pub target_revision: Option<ResourceRevision>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotChangeCounts {
    pub create: u32,
    pub replace: u32,
    pub delete: u32,
    pub conflict: u32,
    /// Paths that differ between the two captures but already match the target.
    pub unchanged: u32,
    pub created_directories: u32,
}

impl SnapshotChangeCounts {
    /// Paths a restore would write or remove. Conflicts are not among them: any
    /// conflict stops the restore before it writes anything.
    pub fn applied(&self) -> u64 {
        u64::from(self.create) + u64::from(self.replace) + u64::from(self.delete)
    }
}

/// `changes` and `created_directories` are bounded samples, conflicts first;
/// `counts` is complete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotRestorePreview {
    pub restore_id: RestoreId,
    pub target_snapshot_id: SnapshotId,
    /// The capture the workspace is believed to match. Only paths that differ
    /// between it and the target are restored.
    pub source_snapshot_id: SnapshotId,
    pub counts: SnapshotChangeCounts,
    pub changes: Vec<SnapshotChange>,
    pub created_directories: Vec<WorkspacePath>,
}

/// Cleanup deletes checkpoints, never snapshots: one content-addressed snapshot
/// may back checkpoints of other sessions, and the authority collects snapshots
/// nothing references any more on its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotCleanupPreview {
    pub checkpoint_ids: Vec<CheckpointId>,
    /// Requested checkpoints the authority no longer holds.
    pub missing_checkpoint_ids: Vec<CheckpointId>,
    pub reclaimable_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotUnrevertPreview {
    pub source_restore_id: RestoreId,
    pub restore: SnapshotRestorePreview,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "preview")]
pub enum SnapshotOperationPreview {
    Restore(SnapshotRestorePreview),
    Unrevert(SnapshotUnrevertPreview),
    Cleanup(SnapshotCleanupPreview),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedSnapshotOperation {
    pub operation: OperationHandle,
    pub preview: SnapshotOperationPreview,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotRestoreState {
    Publishing,
    Completed,
    Partial,
    Indeterminate,
    Acknowledged,
    Reverted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotRestoreStatus {
    pub restore_id: RestoreId,
    pub state: SnapshotRestoreState,
    pub target_snapshot_id: SnapshotId,
    pub source_snapshot_id: SnapshotId,
    pub applied_files: u32,
    pub total_files: u32,
    pub acknowledgement_required: bool,
    pub reconciliation_required: bool,
    pub unrevert_of: Option<RestoreId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotCleanupResult {
    pub deleted_checkpoint_ids: Vec<CheckpointId>,
    pub deleted_snapshots: u32,
    pub deleted_objects: u32,
    pub reclaimed_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "result")]
pub enum SnapshotOperationResult {
    Restore(SnapshotRestoreStatus),
    Cleanup(SnapshotCleanupResult),
}

#[async_trait]
pub trait WorkspaceSnapshotReadService: Send + Sync {
    async fn capture(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &SnapshotCaptureRequest,
    ) -> Result<SnapshotCaptureResult, WorkspaceError>;

    async fn inspect(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &SnapshotInspectRequest,
    ) -> Result<SnapshotInspectPage, WorkspaceError>;

    async fn restore_status(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        restore_id: &RestoreId,
    ) -> Result<SnapshotRestoreStatus, WorkspaceError>;
}

#[async_trait]
pub trait WorkspaceSnapshotMutationService: Send + Sync {
    /// Most checkpoints one cleanup may name; a caller deletes more in chunks.
    fn max_cleanup_checkpoints(&self) -> usize;

    /// Restores `target` over paths that differ between it and `source`, the
    /// capture the workspace is believed to match. A path that matches neither
    /// is a conflict.
    async fn prepare_restore(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        target: &SnapshotId,
        source: &SnapshotId,
    ) -> Result<PreparedSnapshotOperation, WorkspaceError>;

    async fn prepare_unrevert(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        restore_id: &RestoreId,
    ) -> Result<PreparedSnapshotOperation, WorkspaceError>;

    async fn prepare_cleanup(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        checkpoint_ids: &[CheckpointId],
    ) -> Result<PreparedSnapshotOperation, WorkspaceError>;

    async fn execute(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        prepared: &PreparedSnapshotOperation,
    ) -> Result<OperationStatus<SnapshotOperationResult>, WorkspaceError>;

    async fn operation_status(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        operation: &OperationHandle,
    ) -> Result<OperationStatus<SnapshotOperationResult>, WorkspaceError>;

    async fn cancel(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        operation: &OperationHandle,
    ) -> Result<CancellationResult, WorkspaceError>;

    async fn acknowledge(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        restore_id: &RestoreId,
    ) -> Result<SnapshotRestoreStatus, WorkspaceError>;

    async fn release(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        prepared: &PreparedSnapshotOperation,
    ) -> Result<ReleaseResult, WorkspaceError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectAssetKind {
    Instructions,
    Skill,
    Command,
    Workflow,
    Permissions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectAssetTrust {
    Declarative,
    ClientApprovalRequired,
    MixedReviewRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectAsset {
    pub path: WorkspacePath,
    pub resource_id: ResourceId,
    pub revision: ResourceRevision,
    pub kind: ProjectAssetKind,
    pub trust: ProjectAssetTrust,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProjectAssetTrustKey {
    authority: AuthorityIdentity,
    principal: crate::AuthenticatedPrincipalId,
    project: crate::ProjectIdentity,
    path: WorkspacePath,
    resource_id: ResourceId,
    revision: ResourceRevision,
}

impl ProjectAssetTrustKey {
    pub fn new(binding: &SessionWorkspaceBinding, asset: &ProjectAsset) -> Self {
        Self {
            authority: binding.authority().clone(),
            principal: binding.principal().clone(),
            project: binding.project().clone(),
            path: asset.path.clone(),
            resource_id: asset.resource_id.clone(),
            revision: asset.revision.clone(),
        }
    }

    pub fn from_parts(
        authority: AuthorityIdentity,
        principal: crate::AuthenticatedPrincipalId,
        project: crate::ProjectIdentity,
        path: WorkspacePath,
        resource_id: ResourceId,
        revision: ResourceRevision,
    ) -> Result<Self, WorkspaceError> {
        if principal.authority() != &authority || project.authority() != &authority {
            return Err(WorkspaceError::IdentityMismatch);
        }
        Ok(Self {
            authority,
            principal,
            project,
            path,
            resource_id,
            revision,
        })
    }

    pub fn authority(&self) -> &AuthorityIdentity {
        &self.authority
    }

    pub fn principal(&self) -> &crate::AuthenticatedPrincipalId {
        &self.principal
    }

    pub fn project(&self) -> &crate::ProjectIdentity {
        &self.project
    }

    pub fn path(&self) -> &WorkspacePath {
        &self.path
    }

    pub fn resource_id(&self) -> &ResourceId {
        &self.resource_id
    }

    pub fn revision(&self) -> &ResourceRevision {
        &self.revision
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectAssetManifest {
    pub version: OperationId,
    pub revision: CollectionRevision,
    pub assets: Vec<ProjectAsset>,
    /// Paths the host could not read while discovering, so nothing beneath
    /// them is in `assets`.
    pub unreadable: Vec<WorkspacePath>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectAssetContent {
    pub asset: ProjectAsset,
    pub content: String,
    pub truncated: bool,
}

#[async_trait]
pub trait WorkspaceAssetService: Send + Sync {
    async fn discover(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
    ) -> Result<ProjectAssetManifest, WorkspaceError>;

    async fn read(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        asset: &ProjectAsset,
        max_bytes: u32,
    ) -> Result<ProjectAssetContent, WorkspaceError>;
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolPrepareRequest {
    pub name: String,
    pub input: Value,
}

impl fmt::Debug for ToolPrepareRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolPrepareRequest")
            .field("name", &self.name)
            .field("input", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct PreparedToolCall {
    pub operation: OperationHandle,
    pub canonical_input: Value,
    pub review: Value,
}

impl fmt::Debug for PreparedToolCall {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedToolCall")
            .field("operation", &self.operation)
            .field("canonical_input", &"<redacted>")
            .field("review", &"<redacted>")
            .finish()
    }
}

#[async_trait]
pub trait WorkspaceToolService: Send + Sync {
    async fn prepare(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &ToolPrepareRequest,
    ) -> Result<PreparedToolCall, WorkspaceError>;

    async fn execute(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        prepared: &PreparedToolCall,
    ) -> Result<OperationStatus<Value>, WorkspaceError>;

    async fn status(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        operation: &OperationHandle,
    ) -> Result<OperationStatus<Value>, WorkspaceError>;

    async fn cancel(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        operation: &OperationHandle,
    ) -> Result<CancellationResult, WorkspaceError>;

    async fn release(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        prepared: &PreparedToolCall,
    ) -> Result<ReleaseResult, WorkspaceError>;
}

#[derive(Default)]
pub struct WorkspaceServices {
    pub transfer: Option<Arc<dyn WorkspaceTransferService>>,
    pub control: Option<Arc<dyn WorkspaceControlService>>,
    pub read: Option<Arc<dyn WorkspaceReadService>>,
    pub mutation: Option<Arc<dyn WorkspaceMutationService>>,
    pub search: Option<Arc<dyn WorkspaceSearchService>>,
    pub watch: Option<Arc<dyn WorkspaceWatchService>>,
    pub exec: Option<Arc<dyn WorkspaceExecService>>,
    pub scm_read: Option<Arc<dyn WorkspaceScmReadService>>,
    pub scm_mutation: Option<Arc<dyn WorkspaceScmMutationService>>,
    pub snapshot_read: Option<Arc<dyn WorkspaceSnapshotReadService>>,
    pub snapshot_mutation: Option<Arc<dyn WorkspaceSnapshotMutationService>>,
    pub assets: Option<Arc<dyn WorkspaceAssetService>>,
    pub tools: Option<Arc<dyn WorkspaceToolService>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceControlCommand {
    Status,
    Pending,
    Reconnect,
    Reconcile,
    Acknowledge(OperationId),
}

impl WorkspaceControlCommand {
    pub fn parse(args: &str) -> Result<Self, &'static str> {
        let words: Vec<_> = args.split_whitespace().collect();
        match words.as_slice() {
            [] | ["status"] => Ok(Self::Status),
            ["pending"] => Ok(Self::Pending),
            ["reconnect"] => Ok(Self::Reconnect),
            ["reconcile"] => Ok(Self::Reconcile),
            ["acknowledge", id, "--accept-possible-effects"] => OperationId::new(*id)
                .map(Self::Acknowledge)
                .map_err(|_| "Invalid operation identifier"),
            ["acknowledge", ..] => Err(
                "Acknowledgement clears the operation from the pending report. The remote operation may already have had effects or may still be running. It does not cancel, undo, or resend it. Confirm with: /remote acknowledge <operation-id> --accept-possible-effects",
            ),
            _ => Err(
                "Usage: /remote status|pending|reconnect|reconcile|acknowledge <operation-id> --accept-possible-effects",
            ),
        }
    }
}

#[async_trait]
pub trait WorkspaceControlService: Send + Sync {
    async fn execute(&self, command: WorkspaceControlCommand) -> Result<String, WorkspaceError>;
}

pub async fn execute_workspace_control(
    workspace: &WorkspaceSession,
    args: &str,
) -> Result<String, String> {
    let command = WorkspaceControlCommand::parse(args).map_err(str::to_owned)?;
    workspace
        .workspace()
        .services()
        .control
        .as_ref()
        .ok_or_else(|| "Remote workspace control is unavailable".to_owned())?
        .execute(command)
        .await
        .map_err(|_| {
            "Remote control failed; no mutation was resent. Check status and pending operations."
                .to_owned()
        })
}

impl WorkspaceServices {
    fn supports(&self, capability: WorkspaceCapability) -> bool {
        use WorkspaceCapability as Capability;

        match capability {
            Capability::ReviewedTransfer => self.transfer.is_some(),
            Capability::Resolve
            | Capability::Stat
            | Capability::List
            | Capability::ReadText
            | Capability::ReadBytes => self.read.is_some(),
            Capability::MutationExecute
            | Capability::MutationStatus
            | Capability::MutationCancel
            | Capability::MutationAtomic
            | Capability::MutationRollback => self.mutation.is_some(),
            Capability::Search => self.search.is_some(),
            Capability::WatchOpen
            | Capability::WatchPoll
            | Capability::WatchClose
            | Capability::WatchRecursive
            | Capability::WatchExactRenamePairing => self.watch.is_some(),
            Capability::ExecExecute
            | Capability::ExecStatus
            | Capability::ExecCancel
            | Capability::ExecTimeout => self.exec.is_some(),
            Capability::ScmDiscover
            | Capability::ScmStatus
            | Capability::ScmLog
            | Capability::ScmDiff
            | Capability::ScmReadSide => self.scm_read.is_some(),
            Capability::ScmStage
            | Capability::ScmUnstage
            | Capability::ScmDiscard
            | Capability::ScmMutationStatus
            | Capability::ScmMutationCancel
            | Capability::ScmMutationRelease => self.scm_mutation.is_some(),
            Capability::SnapshotCapture
            | Capability::SnapshotCaptureLabels
            | Capability::SnapshotInspect
            | Capability::SnapshotStatus => self.snapshot_read.is_some(),
            Capability::SnapshotPrepareRestore
            | Capability::SnapshotPrepareUnrevert
            | Capability::SnapshotAcknowledge
            | Capability::SnapshotPrepareCleanup
            | Capability::SnapshotExecute
            | Capability::SnapshotOperationStatus
            | Capability::SnapshotCancel
            | Capability::SnapshotRelease
            | Capability::SnapshotAtomicAcrossFiles
            | Capability::SnapshotDurablePerFileJournal => self.snapshot_mutation.is_some(),
            Capability::ProjectAssetsDiscover | Capability::ProjectAssetsRead => {
                self.assets.is_some()
            }
            Capability::ToolPrepare
            | Capability::ToolExecute
            | Capability::ToolStatus
            | Capability::ToolCancel
            | Capability::ToolRelease => self.tools.is_some(),
        }
    }
}

struct WorkspaceHandleInner {
    authority: AuthorityIdentity,
    capabilities: WorkspaceCapabilities,
    services: WorkspaceServices,
}

#[derive(Clone)]
pub struct WorkspaceHandle(Arc<WorkspaceHandleInner>);

struct WorkspaceSessionInner {
    workspace: WorkspaceHandle,
    binding: SessionWorkspaceBinding,
    cursor: WorkspaceCursor,
}

/// Exact workspace authority, identity, and cursor captured for one client session.
#[derive(Clone)]
pub struct WorkspaceSession(Arc<WorkspaceSessionInner>);

impl WorkspaceHandle {
    pub fn new(
        authority: AuthorityIdentity,
        capabilities: WorkspaceCapabilities,
        services: WorkspaceServices,
    ) -> Result<Self, WorkspaceError> {
        capabilities.validate()?;
        for capability in capabilities.iter() {
            if !services.supports(capability) {
                return Err(WorkspaceError::CapabilityMismatch { capability });
            }
        }
        Ok(Self(Arc::new(WorkspaceHandleInner {
            authority,
            capabilities,
            services,
        })))
    }

    pub fn authority(&self) -> &AuthorityIdentity {
        &self.0.authority
    }

    pub fn capabilities(&self) -> &WorkspaceCapabilities {
        &self.0.capabilities
    }

    pub fn services(&self) -> &WorkspaceServices {
        &self.0.services
    }

    pub fn validate_context(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        expected_generation: u64,
        expected_handle: &crate::CwdHandle,
    ) -> Result<(), WorkspaceError> {
        if binding.authority() != &self.0.authority {
            return Err(WorkspaceError::IdentityMismatch);
        }
        cursor.validate(binding, expected_generation, expected_handle)
    }
}

impl WorkspaceSession {
    pub fn new(
        workspace: WorkspaceHandle,
        binding: SessionWorkspaceBinding,
        cursor: WorkspaceCursor,
    ) -> Result<Self, WorkspaceError> {
        if workspace.authority() != binding.authority()
            || cursor.binding_id() != binding.binding_id()
            || cursor.project() != binding.project()
        {
            return Err(WorkspaceError::IdentityMismatch);
        }
        Ok(Self(Arc::new(WorkspaceSessionInner {
            workspace,
            binding,
            cursor,
        })))
    }

    pub fn workspace(&self) -> &WorkspaceHandle {
        &self.0.workspace
    }

    pub fn binding(&self) -> &SessionWorkspaceBinding {
        &self.0.binding
    }

    pub fn cursor(&self) -> &WorkspaceCursor {
        &self.0.cursor
    }

    pub fn with_cursor(
        &self,
        resolved: ResolvedWorkspaceDirectory,
    ) -> Result<Self, WorkspaceError> {
        if resolved.resource.project != *self.0.binding.project()
            || resolved.resource.scope != *resolved.cursor.scope()
            || !matches!(
                resolved.resource.kind,
                crate::ResourceKind::ProjectRoot | crate::ResourceKind::Directory
            )
            || resolved.resource.path.is_none()
        {
            return Err(WorkspaceError::IdentityMismatch);
        }
        if resolved.cursor.binding_id() != self.0.binding.binding_id()
            || resolved.cursor.project() != self.0.binding.project()
        {
            return Err(WorkspaceError::IdentityMismatch);
        }
        if resolved.cursor.generation() != self.0.cursor.generation() {
            return Err(WorkspaceError::StaleCursor);
        }
        Self::new(
            self.0.workspace.clone(),
            self.0.binding.clone(),
            resolved.cursor,
        )
    }
}

impl fmt::Debug for WorkspaceSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceSession")
            .field("workspace", &self.0.workspace)
            .field("binding", &self.0.binding)
            .field("cursor", &self.0.cursor)
            .finish()
    }
}

impl fmt::Debug for WorkspaceHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceHandle")
            .field("authority", &self.0.authority)
            .field("capabilities", &self.0.capabilities)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::WorkspaceControlCommand;
    use serde_json::{Value, json};
    use test_case::test_case;

    use crate::{
        AuthenticatedPrincipalId, AuthorityIdentity, CollectionRevision, CommandText,
        ContinuationToken, CwdHandle, ListPage, OperationError, OperationHandle, OperationId,
        OperationProgress, OperationProgressKind, OperationState, OperationStatus, ProjectIdentity,
        ProjectKey, ResolvedWorkspaceDirectory, ResourceId, ResourceKind, ResourceRevision,
        ResourceScope, ScmChangeKind, ScmStatusEntry, SearchPage, SearchScanCounts,
        SequenceMetadata, SessionBindingId, SessionWorkspaceBinding, SourceTrustAnchor,
        TextContent, WatchCursor, WatchEventPage, WatchPollState, WatchResyncReason,
        WatchSubscriptionId, WorkspaceCapabilities, WorkspaceCapability, WorkspaceCursor,
        WorkspaceError, WorkspaceHandle, WorkspacePath, WorkspaceResource, WorkspaceServices,
        WorkspaceSession,
    };

    #[test_case("acknowledge operation"; "missing_confirmation")]
    #[test_case("acknowledge operation --yes"; "wrong_confirmation")]
    #[test_case("acknowledge operation --accept-possible-effects extra"; "extra_argument")]
    #[test_case("execute operation"; "no_execute")]
    fn control_rejects_unconfirmed_acknowledgement_and_execution(args: &str) {
        assert!(WorkspaceControlCommand::parse(args).is_err());
    }

    #[test_case("status", WorkspaceControlCommand::Status; "status")]
    #[test_case("pending", WorkspaceControlCommand::Pending; "pending")]
    #[test_case("reconnect", WorkspaceControlCommand::Reconnect; "reconnect")]
    #[test_case("reconcile", WorkspaceControlCommand::Reconcile; "reconcile")]
    fn control_parses_recovery_commands(args: &str, expected: WorkspaceControlCommand) {
        assert_eq!(WorkspaceControlCommand::parse(args).unwrap(), expected);
    }

    fn operation_handle() -> OperationHandle {
        OperationHandle {
            preparation_id: OperationId::new("prepared").expect("valid operation id"),
            invocation_id: Some(OperationId::new("invocation").expect("valid operation id")),
            execution_id: Some(OperationId::new("execution").expect("valid operation id")),
            expires_at_unix_ms: Some(42),
        }
    }

    #[test]
    fn paged_search_metadata_round_trips_without_losing_consistency() {
        let page = SearchPage {
            revision: CollectionRevision::new("collection-r1").expect("valid revision"),
            hits: Vec::new(),
            scan_counts: SearchScanCounts {
                files_scanned: 7,
                files_listed: 11,
            },
            truncated: true,
            incomplete: true,
            continuation: Some(ContinuationToken::new("next").expect("valid continuation")),
        };

        let encoded = serde_json::to_value(&page).expect("serialize search page");
        let decoded: SearchPage = serde_json::from_value(encoded).expect("deserialize search page");

        assert_eq!(decoded, page);
        assert_eq!(decoded.scan_counts.files_scanned, 7);
        assert!(decoded.truncated);
        assert!(decoded.incomplete);

        let list = ListPage {
            revision: CollectionRevision::new("list-r1").expect("valid revision"),
            resources: Vec::new(),
            truncated: true,
            incomplete: false,
            continuation: Some(ContinuationToken::new("list-next").expect("valid continuation")),
        };
        assert_eq!(
            serde_json::from_value::<ListPage>(
                serde_json::to_value(&list).expect("serialize list page")
            )
            .expect("deserialize list page"),
            list
        );
    }

    #[test]
    fn operation_status_keeps_terminal_failure_distinct_from_indeterminate() {
        let progress = vec![OperationProgress {
            execution_id: OperationId::new("execution").expect("valid operation id"),
            sequence: 9,
            kind: OperationProgressKind::Unknown(
                OperationId::new("backend-phase").expect("valid progress kind"),
            ),
            chunk: "details".to_owned(),
        }];
        let failed: OperationStatus<Value> = OperationStatus {
            handle: operation_handle(),
            state: OperationState::Failed {
                error: OperationError {
                    code: OperationId::new("command-failed").expect("valid error code"),
                    message: "command failed".to_owned(),
                },
                side_effects_possible: true,
            },
            progress: progress.clone(),
            progress_metadata: SequenceMetadata {
                first_retained_sequence: Some(9),
                next_sequence: 10,
                gap_before_first: true,
            },
        };
        let indeterminate = OperationStatus::<Value> {
            handle: operation_handle(),
            state: OperationState::Indeterminate {
                side_effects_possible: true,
            },
            progress,
            progress_metadata: failed.progress_metadata,
        };

        assert!(matches!(failed.state, OperationState::Failed { .. }));
        assert!(matches!(
            indeterminate.state,
            OperationState::Indeterminate { .. }
        ));
        assert!(matches!(
            failed.progress[0].kind,
            OperationProgressKind::Unknown(_)
        ));
    }

    #[test]
    fn text_reads_preserve_line_and_byte_continuations() {
        let content = TextContent {
            text: "second\n".to_owned(),
            resource_id: ResourceId::new("file").expect("valid resource id"),
            revision: ResourceRevision::new("file-r1").expect("valid revision"),
            path: WorkspacePath::new("src/lib.rs").expect("valid path"),
            start_line: 2,
            end_line: 2,
            total_lines: 10,
            start_byte: 6,
            end_byte: 13,
            truncated: true,
            next_byte_offset: Some(13),
        };
        let decoded: TextContent =
            serde_json::from_value(serde_json::to_value(&content).expect("serialize text content"))
                .expect("deserialize text content");

        assert_eq!(decoded, content);
        assert_eq!(decoded.start_line, 2);
        assert_eq!(decoded.start_byte, 6);
        assert_eq!(decoded.next_byte_offset, Some(13));
    }

    #[test]
    fn full_resync_carries_reason_and_sequence_gap_metadata() {
        let page = WatchEventPage {
            subscription_id: WatchSubscriptionId::new("watch").expect("valid subscription id"),
            state: WatchPollState::FullResync {
                reason: WatchResyncReason::RetentionLost,
            },
            sequence: SequenceMetadata {
                first_retained_sequence: None,
                next_sequence: 81,
                gap_before_first: true,
            },
            events: Vec::new(),
        };

        assert!(matches!(
            page.state,
            WatchPollState::FullResync {
                reason: WatchResyncReason::RetentionLost
            }
        ));
        assert!(page.sequence.gap_before_first);
        assert!(page.events.is_empty());
    }

    #[test]
    fn request_debug_output_redacts_commands_and_progress_chunks() {
        const SECRET: &str = "token=private";
        let request = super::ExecRequest {
            command: CommandText::new(format!("run {SECRET}")).expect("valid command"),
            timeout_ms: None,
        };
        let progress = OperationProgress {
            execution_id: OperationId::new("execution").expect("valid operation id"),
            sequence: 1,
            kind: OperationProgressKind::Stdout,
            chunk: SECRET.to_owned(),
        };

        assert!(!format!("{request:?}").contains(SECRET));
        assert!(!format!("{progress:?}").contains(SECRET));
        assert!(serde_json::from_value::<CommandText>(json!("x".repeat(64 * 1024 + 1))).is_err());
    }

    #[test]
    fn current_watch_state_preserves_the_server_cursor() {
        let cursor = WatchCursor::new("server-position").expect("valid watch cursor");
        let state = WatchPollState::Current {
            next_cursor: cursor.clone(),
            expires_at_unix_ms: 100,
        };

        assert_eq!(
            serde_json::from_value::<WatchPollState>(
                serde_json::to_value(&state).expect("serialize watch state")
            )
            .expect("deserialize watch state"),
            state
        );
        assert_eq!(json!(cursor), json!("server-position"));
    }

    #[test]
    fn scm_status_keeps_staged_unstaged_and_conflict_states_separate() {
        let entry = ScmStatusEntry {
            path: WorkspacePath::new("src/lib.rs").expect("valid path"),
            staged: Some(ScmChangeKind::Added),
            unstaged: Some(ScmChangeKind::Modified),
            untracked: false,
            conflicted: true,
        };

        assert_eq!(entry.staged, Some(ScmChangeKind::Added));
        assert_eq!(entry.unstaged, Some(ScmChangeKind::Modified));
        assert!(entry.conflicted);
    }

    #[test]
    fn advertised_methods_require_an_installed_service() {
        let authority = AuthorityIdentity::new(
            SourceTrustAnchor::new("test-source").expect("valid trust anchor"),
            "authority",
            "workspace",
            "generation",
            "namespace",
        )
        .expect("valid authority");
        let capabilities = WorkspaceCapabilities::from([WorkspaceCapability::Resolve]);

        assert!(matches!(
            WorkspaceHandle::new(authority, capabilities, WorkspaceServices::default()),
            Err(WorkspaceError::CapabilityMismatch {
                capability: WorkspaceCapability::Resolve
            })
        ));
    }

    fn workspace_session(tab: &str) -> WorkspaceSession {
        let authority = AuthorityIdentity::new(
            SourceTrustAnchor::new("test-source").expect("valid trust anchor"),
            "authority",
            "workspace",
            "generation",
            "namespace",
        )
        .expect("valid authority");
        let binding = SessionWorkspaceBinding::new(
            SessionBindingId::new(tab).expect("valid binding id"),
            authority.clone(),
            AuthenticatedPrincipalId::new(authority.clone(), "principal").expect("valid principal"),
            ProjectIdentity::new(
                authority.clone(),
                ProjectKey::new("project").expect("valid project key"),
            ),
        )
        .expect("valid binding");
        let cursor = WorkspaceCursor::new(
            &binding,
            ResourceScope::root(ResourceId::new("root").expect("valid root")),
            7,
            CwdHandle::new(format!("{tab}-root")).expect("valid cwd handle"),
        );
        WorkspaceSession::new(
            WorkspaceHandle::new(
                authority,
                WorkspaceCapabilities::default(),
                Default::default(),
            )
            .expect("valid workspace"),
            binding,
            cursor,
        )
        .expect("valid session")
    }

    fn resolved_directory(
        session: &WorkspaceSession,
        id: &str,
        generation: u64,
    ) -> ResolvedWorkspaceDirectory {
        let resource_id = ResourceId::new(id).expect("valid resource id");
        let scope = ResourceScope::new(
            vec![session.cursor().scope().resource_id().clone()],
            resource_id,
        )
        .expect("valid scope");
        ResolvedWorkspaceDirectory {
            resource: WorkspaceResource {
                project: session.binding().project().clone(),
                scope: scope.clone(),
                path: Some(WorkspacePath::new(id).expect("valid path")),
                kind: ResourceKind::Directory,
                revision: Some(ResourceRevision::new("revision").expect("valid revision")),
                size_bytes: None,
            },
            cursor: WorkspaceCursor::new(
                session.binding(),
                scope,
                generation,
                CwdHandle::new(format!("{id}-cwd")).expect("valid cwd handle"),
            ),
        }
    }

    #[test]
    fn replacing_a_cursor_is_immutable_and_tab_scoped() {
        let first = workspace_session("tab-a");
        let second = workspace_session("tab-b");
        let changed = first
            .with_cursor(resolved_directory(&first, "nested", 7))
            .expect("valid cursor replacement");

        assert_eq!(first.cursor().cwd_handle().as_str(), "tab-a-root");
        assert_eq!(second.cursor().cwd_handle().as_str(), "tab-b-root");
        assert_eq!(changed.cursor().cwd_handle().as_str(), "nested-cwd");
    }

    #[test]
    fn cursor_replacement_rejects_stale_or_foreign_resolutions() {
        let session = workspace_session("tab-a");
        let stale = resolved_directory(&session, "stale", 8);
        assert!(matches!(
            session.with_cursor(stale),
            Err(WorkspaceError::StaleCursor)
        ));

        let foreign = workspace_session("tab-b");
        assert!(matches!(
            session.with_cursor(resolved_directory(&foreign, "foreign", 7)),
            Err(WorkspaceError::IdentityMismatch)
        ));
    }
}
