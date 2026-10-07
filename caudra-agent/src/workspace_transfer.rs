//! Explicit, reviewed file transfers. Compare never stages, journals, or publishes.
//!
//! Inventory adapters must provide no-follow, no-mount traversal and revision-bound local
//! streams. The byte services alone cannot attest those properties. Missing remote parents are
//! explicit conditional creations in each reviewed publication. No delete or automatic rollback
//! is part of this API.

mod directories;
mod execution;
mod journal;
mod manifest;

pub use journal::{BaseEntry, JournalAudit, JournalEntry, JournalState, TransferJournal};
pub use manifest::{
    Comparison, ComparisonKind, ComparisonRow, ExclusionReason, ScanLimit, ScanState,
    TransferFilters,
};

use async_trait::async_trait;
use caudra_storage::{id::CaudraId, private_file::PrivateFileError};
use caudra_workspace::{
    ByteRange, CollectionRevision, ContinuationToken, LocalTransferPath, LocalTransferService,
    LocalTransferSource, OperationId, PreparedDirectoryPublication, PreparedTransferPublication,
    RemoteTransferFile, ResourceId, ResourceRevision, SessionWorkspaceBinding, TransferContent,
    TransferDigest, WorkspaceCursor, WorkspaceError, WorkspacePath, WorkspacePathError,
    WorkspaceTransferService,
};
use futures_lite::{future, io::AsyncReadExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use smol::Timer;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
#[cfg(unix)]
use std::{fs, os::unix::fs::MetadataExt};
use thiserror::Error;

use crate::CancelToken;

const MAX_ENTRIES: usize = 4096;
const PAGE_SIZE: u32 = 128;
/// Listing a folder costs a page even when it is empty, so the budget covers the root, every
/// folder the entry budget admits, and the extra pages of full folders. Only entries bound a scan.
const MAX_PAGES: usize = MAX_ENTRIES + 1 + MAX_ENTRIES / PAGE_SIZE as usize;
const MAX_DEPTH: usize = 32;
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
const MAX_SELECTED: usize = 128;
const PREVIEW_BYTES: usize = 4096;
const INSPECT_BYTES: usize = 64 * 1024;
const MAX_REVIEW_BYTES: usize = 32 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(30);
const PLAN_DOMAIN: &str = "caudra-reviewed-transfer-v2";
const DIGEST_PREFIX: &str = "sha256:";

#[derive(Debug, Error)]
pub enum TransferError {
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error(transparent)]
    Storage(#[from] PrivateFileError),
    #[error("transfer root, filter, resource, or prepared review changed; compare again")]
    Stale,
    #[error("transfer requires explicit, unique, supported file selections")]
    Selection,
    #[error("every destination parent must already exist and be explicitly approved")]
    Parent,
    #[error("transfer traversal, byte, review, or journal quota exceeded")]
    Quota,
    #[error(
        "transfer journal is full of unresolved or cleanup-pending records; reconcile them before another transfer"
    )]
    JournalQuota,
    #[error("invalid transfer filter or unsupported deletion policy")]
    Filter,
    #[error("safe traversal or ignore evaluation is unavailable")]
    UnsafeInventory,
    #[error(
        "previews and reviews need a complete scan of both sides; compare a smaller folder or exclude large folders"
    )]
    PartialInventory,
    #[error("transfer cancelled")]
    Cancelled,
    #[error("transfer service timed out")]
    Timeout,
    #[error("an unresolved transfer blocks this file; reconcile its operation ID, never replay")]
    RecoveryRequired,
    #[error("this reviewed file was already attempted; compare and review a new plan")]
    ReviewConsumed,
    #[error("dirty editor buffer blocks Pull publication")]
    DirtyBuffer,
    #[error("transfer journal is invalid")]
    Journal,
    #[error("local root must be an existing folder outside protected paths")]
    LocalRoot,
    #[error("publication was rejected without applying the file")]
    PublicationRejected,
    #[error("publication was cancelled without applying the file")]
    PublicationCancelled,
}

impl From<WorkspacePathError> for TransferError {
    fn from(_: WorkspacePathError) -> Self {
        Self::Selection
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalRootIdentity {
    canonical_path: PathBuf,
    device: u64,
    inode: u64,
}

impl LocalRootIdentity {
    pub fn capture(path: &Path) -> Result<Self, TransferError> {
        #[cfg(unix)]
        {
            let canonical_path = fs::canonicalize(path).map_err(|_| TransferError::LocalRoot)?;
            if canonical_path
                .iter()
                .any(|component| component.to_str().is_none_or(manifest::protected_component))
            {
                return Err(TransferError::LocalRoot);
            }
            let metadata =
                fs::symlink_metadata(&canonical_path).map_err(|_| TransferError::LocalRoot)?;
            if !metadata.is_dir() {
                return Err(TransferError::LocalRoot);
            }
            Ok(Self {
                canonical_path,
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            Err(TransferError::LocalRoot)
        }
    }

    pub fn canonical_path(&self) -> &Path {
        &self.canonical_path
    }

    fn validate(&self) -> Result<(), TransferError> {
        if Self::capture(&self.canonical_path)? != *self {
            return Err(TransferError::Stale);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteRootIdentity {
    pub binding: SessionWorkspaceBinding,
    pub cursor: WorkspaceCursor,
    pub cwd: WorkspacePath,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferRoots {
    pub local: LocalRootIdentity,
    pub remote: RemoteRootIdentity,
}

impl TransferRoots {
    fn validate(&self) -> Result<(), TransferError> {
        self.local.validate()?;
        self.remote.binding.validate_cursor(
            &self.remote.cursor,
            self.remote.cursor.generation(),
            self.remote.cursor.cwd_handle(),
        )?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Local,
    Remote,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeKind {
    File,
    Directory,
    Symlink,
    Mount,
    Special,
    NestedRepository,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryNode {
    pub path: WorkspacePath,
    pub identity: ResourceId,
    pub revision: ResourceRevision,
    pub kind: NodeKind,
    pub size_bytes: Option<u64>,
    /// None means the adapter could not evaluate the effective ignore rules.
    pub ignored: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryContext {
    pub roots: TransferRoots,
    pub local_ignore_digest: TransferDigest,
    pub remote_ignore_digest: TransferDigest,
    /// Attests no symlink following, mount crossing (including bind mounts), or special-file I/O.
    pub safe_local_traversal: bool,
    pub safe_remote_traversal: bool,
}

pub struct InventoryPage {
    pub revision: CollectionRevision,
    pub entries: Vec<InventoryNode>,
    pub next: Option<ContinuationToken>,
    pub incomplete: bool,
}

pub struct Inspection {
    /// None is a positively established absence, never a permission or transport failure.
    pub node: Option<InventoryNode>,
    pub ignored: Option<bool>,
}

/// Read-only, rooted inventory, independent of the publication services. Listings must include
/// hidden/protected names and return direct children only, without following links or mounts.
/// Inspect must check ancestors for new mounts, links and nested repositories too. Adapter
/// failures are errors, not absence. Ignore digests bind all effective ignore inputs on each side.
#[async_trait]
pub trait TransferInventory: Send + Sync {
    async fn context(&self) -> Result<InventoryContext, TransferError>;
    /// May page a snapshot pinned by an earlier listing of that side, so one scan reads every
    /// directory at one revision. `context` and `inspect` unpin it: a check that needs the
    /// current tree, such as an emptiness check, lists right after inspecting that side.
    async fn list(
        &self,
        side: &Side,
        directory: &WorkspacePath,
        continuation: Option<ContinuationToken>,
        limit: u32,
    ) -> Result<InventoryPage, TransferError>;
    /// Reads the current tree, and fails while that side's inventory is incomplete.
    async fn inspect(&self, side: &Side, path: &WorkspacePath)
    -> Result<Inspection, TransferError>;
    /// Open under the captured canonical root, no-follow/no-mount, and bind the descriptor to
    /// the expected revision. The receiving transfer service verifies the entire stream digest.
    async fn open_local(
        &self,
        root: &LocalRootIdentity,
        path: &WorkspacePath,
        revision: &ResourceRevision,
    ) -> Result<LocalTransferSource, TransferError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalAccess {
    Read,
    Export,
    Write,
}

#[async_trait]
pub trait TransferAuthorization: Send + Sync {
    async fn review_remote_directory(
        &self,
        _plan: &TransferPlan,
        _directory: &PlannedDirectory,
        _prepared: &PreparedDirectoryPublication,
    ) -> Result<(), TransferError> {
        Err(WorkspaceError::PermissionDenied.into())
    }
    async fn roots(&self, roots: &TransferRoots) -> Result<(), TransferError>;
    async fn local(
        &self,
        roots: &TransferRoots,
        path: &WorkspacePath,
        access: LocalAccess,
    ) -> Result<(), TransferError>;
    async fn review_plan(&self, plan: &TransferPlan) -> Result<(), TransferError>;
    async fn review_remote_publication(
        &self,
        plan: &TransferPlan,
        file: &PlannedFile,
        prepared: &PreparedTransferPublication,
    ) -> Result<(), TransferError>;
}

/// Hold this lease until publication settles so a clean buffer cannot become dirty in the gap.
pub trait CleanBufferLease: Send + Sync {}

#[async_trait]
pub trait PullBufferGuard: Send + Sync {
    async fn lock_clean(
        &self,
        root: &LocalRootIdentity,
        path: &WorkspacePath,
    ) -> Result<Box<dyn CleanBufferLease>, TransferError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransferAction {
    Seed,
    Push,
    Pull,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStamp {
    pub node: InventoryNode,
    pub revision: ResourceRevision,
    pub content: TransferContent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum FilePreview {
    TextPrefix { text: String, truncated: bool },
    BinarySummary { content: TransferContent },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TransferPreview {
    pub path: WorkspacePath,
    pub local: Option<FileStamp>,
    pub remote: Option<FileStamp>,
    pub local_preview: Option<FilePreview>,
    pub remote_preview: Option<FilePreview>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovedParent {
    pub side: Side,
    pub path: WorkspacePath,
    pub identity: ResourceId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MetadataPolicy {
    ContentAndExecutableBitOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RollbackCoverage {
    /// No checkpoint, backup, deletion of newly created files, or project-wide undo is implied.
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedFile {
    pub operation_id: OperationId,
    pub path: WorkspacePath,
    pub local: Option<FileStamp>,
    pub remote: Option<FileStamp>,
    pub local_preview: Option<FilePreview>,
    pub remote_preview: Option<FilePreview>,
    pub parents: Vec<ApprovedParent>,
    pub create_directories: Vec<WorkspacePath>,
    pub directory_side: Side,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedDirectory {
    pub operation_id: OperationId,
    pub path: WorkspacePath,
    pub source: InventoryNode,
    pub parents: Vec<ApprovedParent>,
    pub create_directories: Vec<WorkspacePath>,
    pub directory_side: Side,
}

impl PlannedFile {
    fn source(&self, action: &TransferAction) -> Result<&FileStamp, TransferError> {
        match action {
            TransferAction::Seed | TransferAction::Push => self.local.as_ref(),
            TransferAction::Pull => self.remote.as_ref(),
        }
        .ok_or(TransferError::Selection)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanReview {
    pub context: InventoryContext,
    pub filter_digest: TransferDigest,
    pub action: TransferAction,
    pub files: Vec<PlannedFile>,
    pub directories: Vec<PlannedDirectory>,
    pub skipped: Vec<WorkspacePath>,
    pub metadata: MetadataPolicy,
    pub rollback: RollbackCoverage,
    pub atomic_across_files: bool,
    pub atomic_replace_against_external_writers: bool,
}

/// Only the planner can construct this value. UI code receives immutable views; no deserialize
/// or public constructor can turn a modified preview into an executable approval.
#[derive(Debug)]
pub struct TransferPlan {
    digest: TransferDigest,
    review: PlanReview,
}

impl TransferPlan {
    pub fn digest(&self) -> &TransferDigest {
        &self.digest
    }

    pub fn review(&self) -> &PlanReview {
        &self.review
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum TransferPhase {
    Scanning,
    Staging,
    Sealing,
    Preparing,
    Reviewing,
    Publishing,
    Reconciling,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileOutcome {
    Confirmed,
    Failed,
    Cancelled,
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind")]
pub enum TransferEvent {
    Phase {
        side: Option<Side>,
        path: WorkspacePath,
        phase: TransferPhase,
    },
    Planned {
        digest: TransferDigest,
        files: usize,
    },
    Settled {
        operation_id: OperationId,
        path: WorkspacePath,
        outcome: FileOutcome,
    },
    CleanupDeferred {
        operation_id: OperationId,
    },
}

pub trait TransferEvents: Send + Sync {
    /// Must be nonblocking; the receiver owns UI rendering and terminal escaping of previews.
    fn emit(&self, event: TransferEvent);
}

/// All adapters must address the same roots returned by inventory.context(), including the
/// local publisher's pinned directory. A remote workspace's approval is never a local grant.
pub struct TransferServices {
    pub inventory: Arc<dyn TransferInventory>,
    pub local: Arc<dyn LocalTransferService>,
    pub remote: Arc<dyn WorkspaceTransferService>,
    pub authorization: Arc<dyn TransferAuthorization>,
    pub buffers: Arc<dyn PullBufferGuard>,
    pub events: Arc<dyn TransferEvents>,
}

#[derive(Debug, Clone)]
pub struct OrchestrationLimits {
    pub max_entries: usize,
    pub max_pages: usize,
    pub max_depth: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    pub max_selected: usize,
    /// Per-side prefix embedded in a reviewed plan.
    pub preview_bytes: usize,
    /// Per-side prefix read for a read-only comparison.
    pub inspect_bytes: usize,
    pub io_timeout: Duration,
}

impl Default for OrchestrationLimits {
    fn default() -> Self {
        Self {
            max_entries: MAX_ENTRIES,
            max_pages: MAX_PAGES,
            max_depth: MAX_DEPTH,
            max_file_bytes: MAX_FILE_BYTES,
            max_total_bytes: MAX_TOTAL_BYTES,
            max_selected: MAX_SELECTED,
            preview_bytes: PREVIEW_BYTES,
            inspect_bytes: INSPECT_BYTES,
            io_timeout: IO_TIMEOUT,
        }
    }
}

impl OrchestrationLimits {
    fn validate(&self) -> Result<(), TransferError> {
        if self.max_entries == 0
            || self.max_entries > MAX_ENTRIES
            || self.max_pages == 0
            || self.max_pages > MAX_PAGES
            || self.max_depth == 0
            || self.max_depth > MAX_DEPTH
            || self.max_file_bytes == 0
            || self.max_file_bytes > MAX_FILE_BYTES
            || self.max_total_bytes == 0
            || self.max_total_bytes > MAX_TOTAL_BYTES
            || self.max_selected == 0
            || self.max_selected > MAX_SELECTED
            || self.preview_bytes == 0
            || self.preview_bytes > PREVIEW_BYTES
            || self.inspect_bytes == 0
            || self.inspect_bytes > INSPECT_BYTES
            || self.io_timeout.is_zero()
            || self.io_timeout > IO_TIMEOUT
        {
            return Err(TransferError::Quota);
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct TransferRun {
    pub outcomes: BTreeMap<OperationId, FileOutcome>,
    pub stopped: Option<TransferError>,
    pub cleanup_deferred: BTreeSet<OperationId>,
}

pub struct WorkspaceTransfer {
    services: TransferServices,
    filters: TransferFilters,
    limits: OrchestrationLimits,
}

impl WorkspaceTransfer {
    pub fn new(
        services: TransferServices,
        filters: TransferFilters,
        limits: OrchestrationLimits,
    ) -> Result<Self, TransferError> {
        limits.validate()?;
        Ok(Self {
            services,
            filters,
            limits,
        })
    }

    async fn bounded<T>(
        &self,
        cancel: &CancelToken,
        work: impl Future<Output = Result<T, TransferError>>,
    ) -> Result<T, TransferError> {
        if cancel.is_cancelled() {
            return Err(TransferError::Cancelled);
        }
        future::race(work, async {
            Err(future::race(
                async {
                    cancel.cancelled().await;
                    TransferError::Cancelled
                },
                async {
                    Timer::after(self.limits.io_timeout).await;
                    TransferError::Timeout
                },
            )
            .await)
        })
        .await
    }

    async fn workspace<T>(
        &self,
        cancel: &CancelToken,
        work: impl Future<Output = Result<T, WorkspaceError>>,
    ) -> Result<T, TransferError> {
        self.bounded(cancel, async { Ok(work.await?) }).await
    }

    fn phase(&self, path: &WorkspacePath, phase: TransferPhase) {
        self.services.events.emit(TransferEvent::Phase {
            side: None,
            path: path.clone(),
            phase,
        });
    }

    async fn context(&self, cancel: &CancelToken) -> Result<InventoryContext, TransferError> {
        let context = self
            .bounded(cancel, self.services.inventory.context())
            .await?;
        context.roots.validate()?;
        Ok(context)
    }

    async fn validate_context(
        &self,
        context: &InventoryContext,
        filter_digest: &TransferDigest,
        cancel: &CancelToken,
    ) -> Result<(), TransferError> {
        if self.context(cancel).await? != *context || self.filters.digest() != filter_digest {
            return Err(TransferError::Stale);
        }
        Ok(())
    }

    async fn stamp(
        &self,
        context: &InventoryContext,
        side: &Side,
        node: &InventoryNode,
        cancel: &CancelToken,
    ) -> Result<FileStamp, TransferError> {
        let (revision, content) = match side {
            Side::Local => {
                self.bounded(
                    cancel,
                    self.services.authorization.local(
                        &context.roots,
                        &node.path,
                        LocalAccess::Read,
                    ),
                )
                .await?;
                let (revision, content) = self
                    .workspace(
                        cancel,
                        self.services
                            .local
                            .stat(&LocalTransferPath::new(node.path.as_str())?),
                    )
                    .await?;
                (revision.0, content)
            }
            Side::Remote => {
                let root = &context.roots.remote;
                let file = self
                    .workspace(
                        cancel,
                        self.services
                            .remote
                            .stat(&root.binding, &root.cursor, &node.path),
                    )
                    .await?;
                if file.binding != root.binding
                    || file.cursor != root.cursor
                    || file.path != node.path
                    || file.resource_id != node.identity
                {
                    return Err(TransferError::Stale);
                }
                (file.revision, file.content)
            }
        };
        if Some(content.size_bytes) != node.size_bytes
            || content.size_bytes > self.limits.max_file_bytes
        {
            return Err(TransferError::Stale);
        }
        Ok(FileStamp {
            node: node.clone(),
            revision,
            content,
        })
    }

    fn remote_file(context: &InventoryContext, stamp: &FileStamp) -> RemoteTransferFile {
        RemoteTransferFile {
            binding: context.roots.remote.binding.clone(),
            cursor: context.roots.remote.cursor.clone(),
            path: stamp.node.path.clone(),
            resource_id: stamp.node.identity.clone(),
            revision: stamp.revision.clone(),
            content: stamp.content.clone(),
        }
    }

    /// Selection and parent approval are explicit. Neither direction nor a timestamp chooses
    /// winners. Equal, unsupported and incomplete entries cannot be turned into mutations.
    pub async fn plan(
        &self,
        comparison: &Comparison,
        action: TransferAction,
        selected: &[WorkspacePath],
        approved_parents: &BTreeSet<WorkspacePath>,
        cancel: &CancelToken,
    ) -> Result<TransferPlan, TransferError> {
        if selected.is_empty()
            || selected.len() > self.limits.max_selected
            || selected.iter().collect::<BTreeSet<_>>().len() != selected.len()
        {
            return Err(TransferError::Selection);
        }
        self.validate_context(&comparison.context, &comparison.filter_digest, cancel)
            .await?;
        let mut files = Vec::with_capacity(selected.len());
        let mut required_parents = BTreeSet::new();
        let mut bytes = 0_u64;
        for path in selected {
            let row = comparison
                .rows
                .iter()
                .find(|row| &row.path == path)
                .ok_or(TransferError::Selection)?;
            let valid = match action {
                TransferAction::Seed => row.kind == ComparisonKind::LocalOnly,
                TransferAction::Push => matches!(
                    row.kind,
                    ComparisonKind::LocalOnly | ComparisonKind::Conflict
                ),
                TransferAction::Pull => matches!(
                    row.kind,
                    ComparisonKind::RemoteOnly | ComparisonKind::Conflict
                ),
            };
            if !valid
                || path.is_root()
                || row
                    .local_kind
                    .as_ref()
                    .is_some_and(|kind| *kind != NodeKind::File)
                || row
                    .remote_kind
                    .as_ref()
                    .is_some_and(|kind| *kind != NodeKind::File)
            {
                return Err(TransferError::Selection);
            }
            comparison.ensure_inspectable()?;
            let mut file = PlannedFile {
                operation_id: operation_id()?,
                path: path.clone(),
                local: row.local.clone(),
                remote: row.remote.clone(),
                local_preview: None,
                remote_preview: None,
                parents: Vec::new(),
                create_directories: Vec::new(),
                directory_side: if action == TransferAction::Pull {
                    Side::Local
                } else {
                    Side::Remote
                },
            };
            let source = file.source(&action)?;
            bytes = bytes
                .checked_add(source.content.size_bytes)
                .ok_or(TransferError::Quota)?;
            if source.content.size_bytes > self.limits.max_file_bytes
                || bytes > self.limits.max_total_bytes
            {
                return Err(TransferError::Quota);
            }
            let mut parent = path.parent();
            while let Some(path) = parent.filter(|path| !path.is_root()) {
                required_parents.insert(path.clone());
                if !approved_parents.contains(&path) {
                    return Err(TransferError::Parent);
                }
                for (side, manifest) in [
                    (Side::Local, &comparison.local),
                    (Side::Remote, &comparison.remote),
                ] {
                    let Some(entry) = manifest.entries.get(&path) else {
                        if side == file.directory_side && manifest.complete() {
                            if self.inspect_allowed(&side, &path, cancel).await?.is_some() {
                                return Err(TransferError::Parent);
                            }
                            file.create_directories.push(path.clone());
                            continue;
                        }
                        return Err(TransferError::Parent);
                    };
                    let node = &entry.node;
                    if node.kind != NodeKind::Directory || entry.blocked.is_some() {
                        return Err(TransferError::Parent);
                    }
                    file.parents.push(ApprovedParent {
                        side,
                        path: path.clone(),
                        identity: node.identity.clone(),
                    });
                }
                parent = path.parent();
            }
            file.create_directories.reverse();
            self.validate_file(&comparison.context, &file, cancel)
                .await?;
            let limit = self.limits.preview_bytes;
            if let Some(stamp) = &file.local {
                file.local_preview = Some(
                    self.preview(&comparison.context, &Side::Local, stamp, limit, cancel)
                        .await?,
                );
            }
            if let Some(stamp) = &file.remote {
                file.remote_preview = Some(
                    self.preview(&comparison.context, &Side::Remote, stamp, limit, cancel)
                        .await?,
                );
            }
            self.validate_file(&comparison.context, &file, cancel)
                .await?;
            files.push(file);
        }
        if &required_parents != approved_parents {
            return Err(TransferError::Parent);
        }
        self.validate_context(&comparison.context, &comparison.filter_digest, cancel)
            .await?;
        let remote_limits = self.services.remote.limits()?;
        let review = PlanReview {
            context: comparison.context.clone(),
            filter_digest: comparison.filter_digest.clone(),
            atomic_replace_against_external_writers: action != TransferAction::Pull
                && remote_limits.atomic_replace_against_external_writers,
            action,
            files,
            directories: Vec::new(),
            skipped: Vec::new(),
            metadata: MetadataPolicy::ContentAndExecutableBitOnly,
            rollback: RollbackCoverage::None,
            atomic_across_files: false,
        };
        let plan = TransferPlan {
            digest: digest(&(PLAN_DOMAIN, &review))?,
            review,
        };
        self.services.events.emit(TransferEvent::Planned {
            digest: plan.digest.clone(),
            files: plan.review.files.len(),
        });
        Ok(plan)
    }

    async fn inspect_allowed(
        &self,
        side: &Side,
        path: &WorkspacePath,
        cancel: &CancelToken,
    ) -> Result<Option<InventoryNode>, TransferError> {
        if self.filters.excludes(path) {
            return Err(TransferError::Stale);
        }
        let inspected = self
            .bounded(cancel, self.services.inventory.inspect(side, path))
            .await?;
        if self.filters.ignored(inspected.ignored)? {
            return Err(TransferError::Stale);
        }
        if let Some(node) = &inspected.node
            && (node.path != *path
                || self.filters.ignored(node.ignored)?
                || !matches!(node.kind, NodeKind::File | NodeKind::Directory))
        {
            return Err(TransferError::Stale);
        }
        Ok(inspected.node)
    }

    async fn validate_file(
        &self,
        context: &InventoryContext,
        file: &PlannedFile,
        cancel: &CancelToken,
    ) -> Result<(), TransferError> {
        if !context.safe_local_traversal || !context.safe_remote_traversal {
            return Err(TransferError::UnsafeInventory);
        }
        for parent in &file.parents {
            let node = self
                .inspect_allowed(&parent.side, &parent.path, cancel)
                .await?
                .ok_or(TransferError::Stale)?;
            // Directory membership/mtime changes after our own publications; identity must not.
            if node.kind != NodeKind::Directory || node.identity != parent.identity {
                return Err(TransferError::Stale);
            }
        }
        for path in &file.create_directories {
            if self
                .inspect_allowed(&file.directory_side, path, cancel)
                .await?
                .is_some()
            {
                return Err(TransferError::Stale);
            }
        }
        for (side, expected) in [(Side::Local, &file.local), (Side::Remote, &file.remote)] {
            let inspected = self.inspect_allowed(&side, &file.path, cancel).await?;
            match (inspected, expected) {
                (None, None) => {}
                (Some(node), Some(expected)) if node == expected.node => {
                    if self.stamp(context, &side, &node, cancel).await? != *expected {
                        return Err(TransferError::Stale);
                    }
                }
                _ => return Err(TransferError::Stale),
            }
        }
        Ok(())
    }

    pub async fn inspect_preview(
        &self,
        comparison: &Comparison,
        path: &WorkspacePath,
        cancel: &CancelToken,
    ) -> Result<TransferPreview, TransferError> {
        let row = comparison
            .rows
            .iter()
            .find(|row| &row.path == path)
            .ok_or(TransferError::Selection)?;
        if matches!(
            row.kind,
            ComparisonKind::Excluded | ComparisonKind::Unsupported | ComparisonKind::Incomplete
        ) || row
            .local_kind
            .as_ref()
            .is_some_and(|kind| *kind != NodeKind::File)
            || row
                .remote_kind
                .as_ref()
                .is_some_and(|kind| *kind != NodeKind::File)
            || (row.local.is_none() && row.remote.is_none())
        {
            return Err(TransferError::Selection);
        }
        comparison.ensure_inspectable()?;
        self.validate_context(&comparison.context, &comparison.filter_digest, cancel)
            .await?;
        self.bounded(
            cancel,
            self.services.authorization.roots(&comparison.context.roots),
        )
        .await?;
        let mut result = TransferPreview {
            path: path.clone(),
            local: row.local.clone(),
            remote: row.remote.clone(),
            local_preview: None,
            remote_preview: None,
        };
        for (side, expected, preview) in [
            (Side::Local, &result.local, &mut result.local_preview),
            (Side::Remote, &result.remote, &mut result.remote_preview),
        ] {
            let node = self.inspect_allowed(&side, path, cancel).await?;
            match (node, expected) {
                (None, None) => {}
                (Some(node), Some(stamp)) if node == stamp.node => {
                    if self
                        .stamp(&comparison.context, &side, &node, cancel)
                        .await?
                        != *stamp
                    {
                        return Err(TransferError::Stale);
                    }
                    *preview = Some(
                        self.preview(
                            &comparison.context,
                            &side,
                            stamp,
                            self.limits.inspect_bytes,
                            cancel,
                        )
                        .await?,
                    );
                }
                _ => return Err(TransferError::Stale),
            }
        }
        for (side, expected) in [(Side::Local, &result.local), (Side::Remote, &result.remote)] {
            let node = self.inspect_allowed(&side, path, cancel).await?;
            match (node, expected) {
                (None, None) => {}
                (Some(node), Some(stamp)) if node == stamp.node => {
                    if self
                        .stamp(&comparison.context, &side, &node, cancel)
                        .await?
                        != *stamp
                    {
                        return Err(TransferError::Stale);
                    }
                }
                _ => return Err(TransferError::Stale),
            }
        }
        self.validate_context(&comparison.context, &comparison.filter_digest, cancel)
            .await?;
        Ok(result)
    }

    async fn preview(
        &self,
        context: &InventoryContext,
        side: &Side,
        stamp: &FileStamp,
        limit: usize,
        cancel: &CancelToken,
    ) -> Result<FilePreview, TransferError> {
        let length = stamp.content.size_bytes.min(limit as u64);
        let (source, expected_digest) = match side {
            Side::Local => {
                self.bounded(
                    cancel,
                    self.services.authorization.local(
                        &context.roots,
                        &stamp.node.path,
                        LocalAccess::Read,
                    ),
                )
                .await?;
                (
                    self.bounded(
                        cancel,
                        self.services.inventory.open_local(
                            &context.roots.local,
                            &stamp.node.path,
                            &stamp.revision,
                        ),
                    )
                    .await?,
                    (length == stamp.content.size_bytes).then_some(stamp.content.digest.clone()),
                )
            }
            Side::Remote => {
                let file = Self::remote_file(context, stamp);
                let range = (length < stamp.content.size_bytes).then_some(ByteRange {
                    start: 0,
                    end_exclusive: length,
                });
                let downloaded = self
                    .workspace(cancel, self.services.remote.download(&file, range))
                    .await?;
                if downloaded.range
                    != (ByteRange {
                        start: 0,
                        end_exclusive: length,
                    })
                    || downloaded.content.size_bytes != length
                {
                    return Err(TransferError::Stale);
                }
                (downloaded.source, Some(downloaded.content.digest))
            }
        };
        let mut bytes = Vec::with_capacity(length as usize);
        self.bounded(cancel, async {
            source
                .into_reader()
                .take(length)
                .read_to_end(&mut bytes)
                .await
                .map_err(|_| WorkspaceError::TransferIntegrity)?;
            Ok(())
        })
        .await?;
        if bytes.len() as u64 != length
            || expected_digest
                .is_some_and(|expected| byte_digest(&bytes).ok().as_ref() != Some(&expected))
        {
            return Err(WorkspaceError::TransferIntegrity.into());
        }
        let truncated = length < stamp.content.size_bytes;
        let text = match String::from_utf8(bytes) {
            Ok(text) => Some(text),
            Err(error) if truncated && error.utf8_error().error_len().is_none() => {
                let valid = error.utf8_error().valid_up_to();
                let mut bytes = error.into_bytes();
                bytes.truncate(valid);
                String::from_utf8(bytes).ok()
            }
            Err(_) => None,
        };
        Ok(match text.filter(|text| !text.contains('\0')) {
            Some(text) => FilePreview::TextPrefix { text, truncated },
            None => FilePreview::BinarySummary {
                content: stamp.content.clone(),
            },
        })
    }
}

fn digest(value: &impl Serialize) -> Result<TransferDigest, TransferError> {
    let encoded = serde_json::to_vec(value).map_err(|_| TransferError::Journal)?;
    byte_digest(&encoded)
}

fn byte_digest(bytes: &[u8]) -> Result<TransferDigest, TransferError> {
    let mut encoded = String::from(DIGEST_PREFIX);
    for byte in Sha256::digest(bytes) {
        write!(encoded, "{byte:02x}").map_err(|_| TransferError::Journal)?;
    }
    Ok(TransferDigest::new(encoded)?)
}

fn operation_id() -> Result<OperationId, TransferError> {
    OperationId::new(format!("transfer-{}", CaudraId::generate()))
        .map_err(|_| TransferError::Journal)
}

#[cfg(all(test, unix))]
mod tests {
    use super::journal::{ROTATE_RECORDS, tests::remote_root};
    use super::{
        CleanBufferLease, ComparisonKind, ExclusionReason, FileOutcome, FilePreview, INSPECT_BYTES,
        Inspection, InventoryContext, InventoryNode, InventoryPage, JournalState, LocalAccess,
        LocalRootIdentity, NodeKind, OrchestrationLimits, PlannedDirectory, PlannedFile,
        PullBufferGuard, RollbackCoverage, ScanLimit, Side, TransferAction, TransferAuthorization,
        TransferError, TransferEvent, TransferEvents, TransferFilters, TransferInventory,
        TransferJournal, TransferPhase, TransferPlan, TransferRoots, TransferServices,
        WorkspaceTransfer, byte_digest, digest, operation_id,
    };
    use crate::{CancelToken, CancelTrigger};
    use async_trait::async_trait;
    use caudra_config::sandbox::TransferPolicy;
    use caudra_storage::private_file::PrivateFileError;
    use caudra_workspace::{
        ByteRange, CollectionRevision, ContinuationToken, DirectoryPublicationRequest,
        DirectoryPublicationStatus, DownloadedTransfer, LocalPublicationState,
        LocalTransferCondition, LocalTransferDestination, LocalTransferPath, LocalTransferReview,
        LocalTransferRevision, LocalTransferService, LocalTransferSource, MutationCondition,
        OperationError, OperationHandle, OperationId, OperationPhase, OperationState,
        OperationStatus, PreparedDirectoryPublication, PreparedLocalDirectory,
        PreparedLocalTransfer, PreparedTransferPublication, PublishedTransferDirectory,
        ReleaseResult, RemoteTransferFile, RemoteTransferStage, ResourceId, ResourceRevision,
        SealedTransfer, SequenceMetadata, SessionWorkspaceBinding, TransferContent, TransferLimits,
        TransferMode, TransferPublicationRequest, TransferPublicationState,
        TransferPublicationStatus, WorkspaceCursor, WorkspaceError, WorkspacePath,
        WorkspaceTransferService,
    };
    use futures_lite::{
        future,
        io::{AsyncReadExt, Cursor},
    };
    use serde_json::{Value, json};
    use std::{
        collections::{BTreeMap, BTreeSet},
        fs::{self, Permissions},
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        process::Command,
        sync::{Arc, Mutex},
    };
    use tempfile::{Builder, TempDir};
    use test_case::test_case;

    const FILE: &str = "one.bin";
    const SECOND: &str = "two.bin";
    const THIRD: &str = "three.bin";
    const BEFORE: &[u8] = b"original bytes";
    const AFTER: &[u8] = b"reviewed bytes, not journal data";
    const EXTERNAL: &[u8] = b"external writer";
    const BINARY_SIZE: usize = 6 * 1024 * 1024;
    const JOURNAL_FILE: &str = "transfers.json";
    const TEST_ROTATION_RECORDS: usize = 3;
    const ROTATION_PASSES: usize = 2;
    const EMPTY_DIRECTORY: &str = "folder/empty";
    const FOLDER: &str = "folder";
    const NESTED_SAME: &str = "folder/same";
    const NESTED_CHANGED: &str = "folder/changed";
    const NESTED_LOCAL: &str = "folder/local";
    const NESTED_REMOTE: &str = "folder/remote";
    const PROTECTED: &str = ".env";
    const GENERATED: &str = "generated";
    const IGNORED: &str = "ignored";
    const DOTFILE: &str = ".tool-versions";
    const MULTIBYTE: &str = "aé";
    const MULTIBYTE_CUT: usize = 2;
    const MULTIBYTE_PREFIX: &str = "a";
    const PRECONDITION_FAILED: &str = "precondition failed";
    const SUBJECT: &str = "subject";
    const VERSION: &str = "generation-a";
    const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
    const JOURNAL_PROCESS_PATH: &str = "CAUDRA_TEST_JOURNAL_PROCESS_PATH";
    const JOURNAL_PROCESS_ID: &str = "CAUDRA_TEST_JOURNAL_PROCESS_ID";
    const JOURNAL_PROCESS_TEST: &str =
        "workspace_transfer::tests::journal_compaction_process_writer";

    #[test_case(".ssh")]
    #[test_case(".git")]
    #[test_case("secrets")]
    #[test_case(".env.private")]
    fn explicit_local_roots_cannot_bypass_protected_ancestors(name: &str) {
        let directory = tempfile::tempdir().unwrap();
        let selected = directory.path().join(name).join("nested");
        fs::create_dir_all(&selected).unwrap();
        assert!(matches!(
            LocalRootIdentity::capture(&selected),
            Err(TransferError::LocalRoot)
        ));
    }

    fn path(value: &str) -> WorkspacePath {
        WorkspacePath::new(value).unwrap()
    }

    #[derive(Clone)]
    struct TestFile {
        node: InventoryNode,
        bytes: Vec<u8>,
        mode: TransferMode,
    }

    impl TestFile {
        fn content(&self) -> TransferContent {
            TransferContent {
                digest: byte_digest(&self.bytes).unwrap(),
                size_bytes: self.bytes.len() as u64,
                mode: self.mode.clone(),
            }
        }
    }

    #[derive(Default)]
    struct Counts {
        lists: usize,
        stats: usize,
        opens: usize,
        downloads: usize,
        stages: usize,
        local_prepares: usize,
        remote_prepares: usize,
        publishes: usize,
        status: usize,
        releases: usize,
        exports: usize,
        writes: usize,
        remote_reviews: usize,
    }

    struct FakeState {
        cancel_directory_preparation: Option<PathBuf>,
        defer_directory_release: bool,
        directory_capability: bool,
        directory_publications: BTreeMap<OperationId, DirectoryPublicationStatus>,
        context: InventoryContext,
        local: BTreeMap<WorkspacePath, TestFile>,
        remote: BTreeMap<WorkspacePath, TestFile>,
        stages: BTreeMap<OperationId, Vec<u8>>,
        local_preparations: BTreeMap<OperationId, (PreparedLocalTransfer, Vec<u8>)>,
        publications: BTreeMap<OperationId, TransferPublicationStatus>,
        local_outcomes: BTreeMap<OperationId, LocalPublicationState>,
        local_status_available: bool,
        counts: Counts,
        revision: usize,
        incomplete: Option<Side>,
        ignored_absence: Option<Side>,
        repeated_cursor: bool,
        bad_page: bool,
        deny_roots: bool,
        deny_access: Option<LocalAccess>,
        mutate_review: Option<Side>,
        tamper_prepared: bool,
        corrupt_stream: bool,
        fail_publish: Option<usize>,
        unknown_publish: Option<usize>,
        race_create: bool,
        dirty: bool,
        cancel_phase: Option<TransferPhase>,
        cancel_after_confirm: bool,
        cancel_in_publish: bool,
        cancel: Option<CancelTrigger>,
        events: Vec<TransferEvent>,
    }

    impl FakeState {
        fn cancel_preparation(&self, id: &OperationId) {
            if let Some(path) = &self.cancel_directory_preparation {
                let mut journal = TransferJournal::new(path.clone()).unwrap();
                journal
                    .update(id, |entry| {
                        entry.state = JournalState::Cancelled;
                        entry.cleanup_pending = true;
                        Ok(())
                    })
                    .unwrap();
            }
        }

        fn publish_directory(
            &mut self,
            request: &DirectoryPublicationRequest,
            side: Side,
        ) -> Result<DirectoryPublicationStatus, WorkspaceError> {
            self.counts.publishes += 1;
            if self.fail_publish == Some(self.counts.publishes) {
                return Err(WorkspaceError::Conflict);
            }
            let mut created_directories = Vec::new();
            for path in request.create_directories.iter().chain([&request.path]) {
                if self.files(&side).contains_key(path) {
                    return Err(WorkspaceError::Conflict);
                }
                self.put(&side, path.as_str(), b"", NodeKind::Directory);
                if *path != request.path {
                    created_directories
                        .push((path.clone(), self.files(&side)[path].node.identity.clone()));
                }
            }
            let status = DirectoryPublicationStatus {
                publication_id: request.publication_id.clone(),
                state: TransferPublicationState::Completed,
                directory: Some(PublishedTransferDirectory {
                    path: request.path.clone(),
                    resource_id: self.files(&side)[&request.path].node.identity.clone(),
                    created_directories,
                }),
            };
            self.directory_publications
                .insert(request.publication_id.clone(), status.clone());
            if self.unknown_publish == Some(self.counts.publishes) {
                return Err(WorkspaceError::IndeterminateOutcome);
            }
            Ok(status)
        }

        fn directory_status(
            &mut self,
            id: &OperationId,
        ) -> Result<DirectoryPublicationStatus, WorkspaceError> {
            self.counts.status += 1;
            Ok(self
                .directory_publications
                .get(id)
                .cloned()
                .unwrap_or(DirectoryPublicationStatus {
                    publication_id: id.clone(),
                    state: TransferPublicationState::Unknown,
                    directory: None,
                }))
        }

        fn files(&self, side: &Side) -> &BTreeMap<WorkspacePath, TestFile> {
            match side {
                Side::Local => &self.local,
                Side::Remote => &self.remote,
            }
        }

        fn files_mut(&mut self, side: &Side) -> &mut BTreeMap<WorkspacePath, TestFile> {
            match side {
                Side::Local => &mut self.local,
                Side::Remote => &mut self.remote,
            }
        }

        fn put(&mut self, side: &Side, name: &str, bytes: &[u8], kind: NodeKind) {
            self.revision += 1;
            let node = InventoryNode {
                path: path(name),
                identity: ResourceId::new(name).unwrap(),
                revision: ResourceRevision::new(format!("revision-{}", self.revision)).unwrap(),
                kind,
                size_bytes: Some(bytes.len() as u64),
                ignored: Some(false),
            };
            self.files_mut(side).insert(
                path(name),
                TestFile {
                    node,
                    bytes: bytes.to_vec(),
                    mode: TransferMode::Regular,
                },
            );
        }

        fn remote_file(&self, name: &WorkspacePath) -> Result<RemoteTransferFile, WorkspaceError> {
            let file = self.remote.get(name).ok_or(WorkspaceError::Conflict)?;
            let root = &self.context.roots.remote;
            Ok(RemoteTransferFile {
                binding: root.binding.clone(),
                cursor: root.cursor.clone(),
                path: name.clone(),
                resource_id: file.node.identity.clone(),
                revision: file.node.revision.clone(),
                content: file.content(),
            })
        }
    }

    struct Fake(Mutex<FakeState>);

    struct Fixture {
        root: TempDir,
        state: TempDir,
        fake: Arc<Fake>,
        engine: WorkspaceTransfer,
        journal: TransferJournal,
    }

    impl Fixture {
        fn rotating() -> Self {
            let mut fixture = Self::new();
            fixture.journal.rotation_records = TEST_ROTATION_RECORDS;
            fixture
        }

        fn fill_journal(&self, count: usize, state: &str, cleanup_pending: bool) {
            let journal_path = self.state.path().join(JOURNAL_FILE);
            let mut data: Value =
                serde_json::from_slice(&fs::read(&journal_path).unwrap()).unwrap();
            let template = data["entries"]
                .as_object()
                .unwrap()
                .values()
                .next()
                .unwrap()
                .clone();
            let mut entries = BTreeMap::new();
            for index in 0..count {
                let id = format!("retained-{index}");
                let mut entry = template.clone();
                entry["operation_id"] = json!(id);
                entry["state"] = json!(state);
                entry["path"] = json!(id);
                entry["local"]["node"]["path"] = json!(id);
                entry["cleanup_pending"] = json!(cleanup_pending);
                entries.insert(id, entry);
            }
            data["entries"] = serde_json::to_value(entries).unwrap();
            fs::write(&journal_path, serde_json::to_vec(&data).unwrap()).unwrap();
        }

        async fn directory_plan(&self, action: TransferAction) -> TransferPlan {
            let source = if action == TransferAction::Pull {
                Side::Remote
            } else {
                Side::Local
            };
            {
                let mut state = self.fake.0.lock().unwrap();
                for name in [FOLDER, EMPTY_DIRECTORY] {
                    state.put(&source, name, b"", NodeKind::Directory);
                }
            }
            let comparison = self.engine.compare(&CancelToken::none()).await.unwrap();
            self.engine
                .plan_selection(&comparison, action, &[path(FOLDER)], &CancelToken::none())
                .await
                .unwrap()
        }

        fn new() -> Self {
            let root = TempDir::new().unwrap();
            let state = Builder::new()
                .permissions(Permissions::from_mode(PRIVATE_DIRECTORY_MODE))
                .tempdir()
                .unwrap();
            let context = InventoryContext {
                roots: TransferRoots {
                    local: LocalRootIdentity::capture(root.path()).unwrap(),
                    remote: remote_root(VERSION, SUBJECT),
                },
                local_ignore_digest: byte_digest(b"local ignores").unwrap(),
                remote_ignore_digest: byte_digest(b"remote ignores").unwrap(),
                safe_local_traversal: true,
                safe_remote_traversal: true,
            };
            let fake = Arc::new(Fake(Mutex::new(FakeState {
                cancel_directory_preparation: None,
                defer_directory_release: false,
                directory_capability: true,
                directory_publications: BTreeMap::new(),
                context,
                local: BTreeMap::new(),
                remote: BTreeMap::new(),
                stages: BTreeMap::new(),
                local_preparations: BTreeMap::new(),
                publications: BTreeMap::new(),
                local_outcomes: BTreeMap::new(),
                local_status_available: false,
                counts: Counts::default(),
                revision: 0,
                incomplete: None,
                ignored_absence: None,
                repeated_cursor: false,
                bad_page: false,
                deny_roots: false,
                deny_access: None,
                mutate_review: None,
                tamper_prepared: false,
                corrupt_stream: false,
                fail_publish: None,
                unknown_publish: None,
                race_create: false,
                dirty: false,
                cancel_phase: None,
                cancel_after_confirm: false,
                cancel_in_publish: false,
                cancel: None,
                events: Vec::new(),
            })));
            let services = TransferServices {
                inventory: fake.clone(),
                local: fake.clone(),
                remote: fake.clone(),
                authorization: fake.clone(),
                buffers: fake.clone(),
                events: fake.clone(),
            };
            let engine = WorkspaceTransfer::new(
                services,
                TransferFilters::new(&TransferPolicy::default(), &[], false).unwrap(),
                OrchestrationLimits::default(),
            )
            .unwrap();
            let journal = TransferJournal::new(state.path().join(JOURNAL_FILE)).unwrap();
            Self {
                root,
                state,
                fake,
                engine,
                journal,
            }
        }

        fn put(&self, side: Side, name: &str, bytes: &[u8]) {
            self.fake
                .0
                .lock()
                .unwrap()
                .put(&side, name, bytes, NodeKind::File);
        }

        async fn plan(&self, action: TransferAction, names: &[&str]) -> TransferPlan {
            let comparison = self.engine.compare(&CancelToken::none()).await.unwrap();
            self.engine
                .plan(
                    &comparison,
                    action,
                    &names.iter().map(|name| path(name)).collect::<Vec<_>>(),
                    &BTreeSet::new(),
                    &CancelToken::none(),
                )
                .await
                .unwrap()
        }
    }

    #[async_trait]
    impl TransferInventory for Fake {
        async fn context(&self) -> Result<InventoryContext, TransferError> {
            Ok(self.0.lock().unwrap().context.clone())
        }

        async fn list(
            &self,
            side: &Side,
            directory: &WorkspacePath,
            continuation: Option<ContinuationToken>,
            limit: u32,
        ) -> Result<InventoryPage, TransferError> {
            let mut state = self.0.lock().unwrap();
            state.counts.lists += 1;
            let offset = continuation
                .as_ref()
                .map_or(0, |cursor| cursor.as_str().parse::<usize>().unwrap());
            let all = state
                .files(side)
                .values()
                .filter(|file| file.node.path.parent().as_ref() == Some(directory))
                .map(|file| file.node.clone())
                .collect::<Vec<_>>();
            let mut entries = all
                .iter()
                .skip(offset)
                .take(limit as usize)
                .cloned()
                .collect::<Vec<_>>();
            if state.bad_page
                && let Some(first) = entries.first().cloned()
            {
                entries = vec![first; limit as usize + 1];
            }
            let next = if state.repeated_cursor {
                Some(ContinuationToken::new("0").unwrap())
            } else if offset + entries.len() < all.len() {
                Some(ContinuationToken::new((offset + entries.len()).to_string()).unwrap())
            } else {
                None
            };
            Ok(InventoryPage {
                revision: CollectionRevision::new("collection").unwrap(),
                entries,
                next,
                incomplete: state.incomplete.as_ref() == Some(side),
            })
        }

        async fn inspect(
            &self,
            side: &Side,
            path: &WorkspacePath,
        ) -> Result<Inspection, TransferError> {
            let state = self.0.lock().unwrap();
            let node = state.files(side).get(path).map(|file| file.node.clone());
            let ignored = node
                .as_ref()
                .map_or(Some(state.ignored_absence.as_ref() == Some(side)), |node| {
                    node.ignored
                });
            Ok(Inspection { node, ignored })
        }

        async fn open_local(
            &self,
            root: &LocalRootIdentity,
            path: &WorkspacePath,
            revision: &ResourceRevision,
        ) -> Result<LocalTransferSource, TransferError> {
            let mut state = self.0.lock().unwrap();
            state.counts.opens += 1;
            let file = state.local.get(path).ok_or(TransferError::Stale)?;
            if &state.context.roots.local != root || &file.node.revision != revision {
                return Err(TransferError::Stale);
            }
            let mut bytes = file.bytes.clone();
            if state.corrupt_stream {
                bytes.push(0);
            }
            Ok(LocalTransferSource::new(Cursor::new(bytes)))
        }
    }

    #[async_trait]
    impl TransferAuthorization for Fake {
        async fn review_remote_directory(
            &self,
            _: &TransferPlan,
            directory: &PlannedDirectory,
            _: &PreparedDirectoryPublication,
        ) -> Result<(), TransferError> {
            let mut state = self.0.lock().unwrap();
            state.counts.remote_reviews += 1;
            if let Some(side) = state.mutate_review.take() {
                state.put(
                    &side,
                    &format!("{}/new", directory.path),
                    EXTERNAL,
                    NodeKind::File,
                );
            }
            Ok(())
        }

        async fn roots(&self, _: &TransferRoots) -> Result<(), TransferError> {
            if self.0.lock().unwrap().deny_roots {
                return Err(WorkspaceError::PermissionDenied.into());
            }
            Ok(())
        }

        async fn local(
            &self,
            _: &TransferRoots,
            _: &WorkspacePath,
            access: LocalAccess,
        ) -> Result<(), TransferError> {
            let mut state = self.0.lock().unwrap();
            match access {
                LocalAccess::Export => state.counts.exports += 1,
                LocalAccess::Write => state.counts.writes += 1,
                LocalAccess::Read => {}
            }
            if state.deny_access.as_ref() == Some(&access) {
                return Err(WorkspaceError::PermissionDenied.into());
            }
            Ok(())
        }

        async fn review_plan(&self, _: &TransferPlan) -> Result<(), TransferError> {
            Ok(())
        }

        async fn review_remote_publication(
            &self,
            _: &TransferPlan,
            file: &PlannedFile,
            _: &PreparedTransferPublication,
        ) -> Result<(), TransferError> {
            let mut state = self.0.lock().unwrap();
            state.counts.remote_reviews += 1;
            if let Some(side) = state.mutate_review.take() {
                state.put(&side, file.path.as_str(), EXTERNAL, NodeKind::File);
            }
            Ok(())
        }
    }

    struct Clean;
    impl CleanBufferLease for Clean {}

    #[async_trait]
    impl PullBufferGuard for Fake {
        async fn lock_clean(
            &self,
            _: &LocalRootIdentity,
            path: &WorkspacePath,
        ) -> Result<Box<dyn CleanBufferLease>, TransferError> {
            let mut state = self.0.lock().unwrap();
            if state.dirty {
                return Err(TransferError::DirtyBuffer);
            }
            if let Some(side) = state.mutate_review.take() {
                state.put(&side, path.as_str(), EXTERNAL, NodeKind::File);
            }
            Ok(Box::new(Clean))
        }
    }

    impl TransferEvents for Fake {
        fn emit(&self, event: TransferEvent) {
            let mut state = self.0.lock().unwrap();
            let cancel = match &event {
                TransferEvent::Phase { phase, .. } => state.cancel_phase.as_ref() == Some(phase),
                TransferEvent::Settled {
                    outcome: FileOutcome::Confirmed,
                    ..
                } => state.cancel_after_confirm,
                _ => false,
            };
            state.events.push(event);
            if cancel && let Some(trigger) = state.cancel.take() {
                trigger.cancel();
            }
        }
    }

    async fn receive(
        source: LocalTransferSource,
        expected: &TransferContent,
    ) -> Result<Vec<u8>, WorkspaceError> {
        let mut bytes = Vec::new();
        source
            .into_reader()
            .take(expected.size_bytes + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| WorkspaceError::TransferIntegrity)?;
        if bytes.len() as u64 != expected.size_bytes
            || byte_digest(&bytes).unwrap() != expected.digest
        {
            return Err(WorkspaceError::TransferIntegrity);
        }
        Ok(bytes)
    }

    #[async_trait]
    impl LocalTransferService for Fake {
        fn supports_directory_publication(&self) -> bool {
            self.0.lock().unwrap().directory_capability
        }
        async fn prepare_directory(
            &self,
            request: &DirectoryPublicationRequest,
        ) -> Result<PreparedLocalDirectory, WorkspaceError> {
            let mut state = self.0.lock().unwrap();
            state.counts.local_prepares += 1;
            state.cancel_preparation(&request.publication_id);
            Ok(PreparedLocalDirectory {
                request: request.clone(),
            })
        }
        async fn execute_directory(
            &self,
            prepared: &PreparedLocalDirectory,
        ) -> Result<DirectoryPublicationStatus, WorkspaceError> {
            self.0
                .lock()
                .unwrap()
                .publish_directory(&prepared.request, Side::Local)
        }
        async fn directory_status(
            &self,
            prepared: &PreparedLocalDirectory,
        ) -> Result<DirectoryPublicationStatus, WorkspaceError> {
            self.0
                .lock()
                .unwrap()
                .directory_status(&prepared.request.publication_id)
        }
        async fn release_directory(
            &self,
            _: &PreparedLocalDirectory,
        ) -> Result<(), WorkspaceError> {
            Ok(())
        }

        async fn created_directories(
            &self,
            prepared: &PreparedLocalTransfer,
        ) -> Result<Vec<(WorkspacePath, ResourceId)>, WorkspaceError> {
            Ok(prepared
                .review
                .destination
                .create_directories
                .iter()
                .map(|path| (path.clone(), ResourceId::new(path.as_str()).unwrap()))
                .collect())
        }
        async fn publication_status(
            &self,
            prepared: &PreparedLocalTransfer,
        ) -> Result<LocalPublicationState, WorkspaceError> {
            let state = self.0.lock().unwrap();
            Ok(if state.local_status_available {
                state
                    .local_outcomes
                    .get(&prepared.id)
                    .cloned()
                    .unwrap_or(LocalPublicationState::Unknown)
            } else {
                LocalPublicationState::Unknown
            })
        }
        async fn stat(
            &self,
            local_path: &LocalTransferPath,
        ) -> Result<(LocalTransferRevision, TransferContent), WorkspaceError> {
            let mut state = self.0.lock().unwrap();
            state.counts.stats += 1;
            let file = state
                .local
                .get(&path(local_path.as_str()))
                .ok_or(WorkspaceError::Conflict)?;
            Ok((
                LocalTransferRevision(file.node.revision.clone()),
                file.content(),
            ))
        }

        async fn prepare(
            &self,
            source: LocalTransferSource,
            destination: LocalTransferDestination,
            expected: TransferContent,
        ) -> Result<PreparedLocalTransfer, WorkspaceError> {
            let bytes = receive(source, &expected).await?;
            let prepared = PreparedLocalTransfer {
                id: operation_id().unwrap(),
                review: LocalTransferReview {
                    destination,
                    content: expected,
                    atomic_replace_against_external_writers: false,
                },
            };
            let mut state = self.0.lock().unwrap();
            state.counts.local_prepares += 1;
            state
                .local_preparations
                .insert(prepared.id.clone(), (prepared.clone(), bytes));
            Ok(prepared)
        }

        async fn execute(
            &self,
            prepared: &PreparedLocalTransfer,
        ) -> Result<LocalTransferRevision, WorkspaceError> {
            let mut state = self.0.lock().unwrap();
            state.counts.publishes += 1;
            let name = path(prepared.review.destination.path.as_str());
            if state.race_create {
                state.put(&Side::Local, name.as_str(), EXTERNAL, NodeKind::File);
            }
            let current = state.local.get(&name);
            let matches = match &prepared.review.destination.condition {
                LocalTransferCondition::MustNotExist => current.is_none(),
                LocalTransferCondition::Matches(expected) => {
                    current.is_some_and(|file| file.node.revision == expected.0)
                }
            };
            if !matches {
                return Err(WorkspaceError::Conflict);
            }
            for directory in &prepared.review.destination.create_directories {
                if state.local.contains_key(directory) {
                    return Err(WorkspaceError::Conflict);
                }
                state.put(&Side::Local, directory.as_str(), b"", NodeKind::Directory);
            }
            let (_, bytes) = state
                .local_preparations
                .remove(&prepared.id)
                .ok_or(WorkspaceError::Conflict)?;
            state.put(&Side::Local, name.as_str(), &bytes, NodeKind::File);
            state.local.get_mut(&name).unwrap().mode = prepared.review.content.mode.clone();
            let completed = LocalPublicationState::Completed(LocalTransferRevision(
                state.local[&name].node.revision.clone(),
            ));
            state.local_outcomes.insert(prepared.id.clone(), completed);
            if state.unknown_publish == Some(state.counts.publishes) {
                return Err(WorkspaceError::IndeterminateOutcome);
            }
            Ok(LocalTransferRevision(
                state.local[&name].node.revision.clone(),
            ))
        }

        async fn release(&self, prepared: &PreparedLocalTransfer) -> Result<(), WorkspaceError> {
            let mut state = self.0.lock().unwrap();
            state.counts.releases += 1;
            state.local_preparations.remove(&prepared.id);
            Ok(())
        }
    }

    #[async_trait]
    impl WorkspaceTransferService for Fake {
        fn supports_directory_publication(&self) -> bool {
            self.0.lock().unwrap().directory_capability
        }
        async fn prepare_directory(
            &self,
            binding: &SessionWorkspaceBinding,
            cursor: &WorkspaceCursor,
            request: &DirectoryPublicationRequest,
        ) -> Result<PreparedDirectoryPublication, WorkspaceError> {
            let mut state = self.0.lock().unwrap();
            state.counts.remote_prepares += 1;
            state.cancel_preparation(&request.publication_id);
            Ok(PreparedDirectoryPublication {
                operation: OperationHandle {
                    preparation_id: operation_id().unwrap(),
                    invocation_id: Some(operation_id().unwrap()),
                    execution_id: None,
                    expires_at_unix_ms: Some(u64::MAX),
                },
                binding: binding.clone(),
                cursor: cursor.clone(),
                cwd_path: state.context.roots.remote.cwd.clone(),
                request_digest: digest(request).unwrap(),
                request: request.clone(),
                review: Value::Null,
            })
        }
        async fn execute_directory(
            &self,
            prepared: &PreparedDirectoryPublication,
        ) -> Result<OperationStatus<DirectoryPublicationStatus>, WorkspaceError> {
            let result = self
                .0
                .lock()
                .unwrap()
                .publish_directory(&prepared.request, Side::Remote)?;
            Ok(OperationStatus {
                handle: prepared.operation.clone(),
                state: OperationState::Completed {
                    result,
                    side_effects_possible: true,
                },
                progress: Vec::new(),
                progress_metadata: SequenceMetadata {
                    first_retained_sequence: None,
                    next_sequence: 0,
                    gap_before_first: false,
                },
            })
        }
        async fn directory_status(
            &self,
            prepared: &PreparedDirectoryPublication,
        ) -> Result<DirectoryPublicationStatus, WorkspaceError> {
            self.0
                .lock()
                .unwrap()
                .directory_status(&prepared.request.publication_id)
        }
        async fn release_directory(
            &self,
            _: &PreparedDirectoryPublication,
        ) -> Result<ReleaseResult, WorkspaceError> {
            let mut state = self.0.lock().unwrap();
            let released = !state.defer_directory_release;
            state.defer_directory_release = false;
            Ok(ReleaseResult {
                state: OperationPhase::Completed,
                released,
            })
        }

        fn limits(&self) -> Result<TransferLimits, WorkspaceError> {
            Ok(TransferLimits {
                max_file_bytes: super::MAX_FILE_BYTES,
                max_stages: 1,
                max_reserved_bytes: super::MAX_TOTAL_BYTES,
                max_concurrent_io: 1,
                stream_buffer_bytes: 64 * 1024,
                atomic_replace_against_external_writers: false,
            })
        }

        async fn stage(
            &self,
            binding: &SessionWorkspaceBinding,
            cursor: &WorkspaceCursor,
            source: LocalTransferSource,
            expected: &TransferContent,
        ) -> Result<RemoteTransferStage, WorkspaceError> {
            let bytes = receive(source, expected).await?;
            let stage = RemoteTransferStage {
                id: operation_id().unwrap(),
                binding: binding.clone(),
                cursor: cursor.clone(),
                content: expected.clone(),
                expires_at_unix_ms: u64::MAX,
            };
            let mut state = self.0.lock().unwrap();
            state.counts.stages += 1;
            state.stages.insert(stage.id.clone(), bytes);
            Ok(stage)
        }

        async fn seal(
            &self,
            stage: &RemoteTransferStage,
        ) -> Result<SealedTransfer, WorkspaceError> {
            Ok(SealedTransfer {
                stage: stage.clone(),
            })
        }

        async fn release_stage(&self, stage: &RemoteTransferStage) -> Result<bool, WorkspaceError> {
            let mut state = self.0.lock().unwrap();
            state.counts.releases += 1;
            Ok(state.stages.remove(&stage.id).is_some())
        }

        async fn stat(
            &self,
            _: &SessionWorkspaceBinding,
            _: &WorkspaceCursor,
            path: &WorkspacePath,
        ) -> Result<RemoteTransferFile, WorkspaceError> {
            let mut state = self.0.lock().unwrap();
            state.counts.stats += 1;
            state.remote_file(path)
        }

        async fn download(
            &self,
            file: &RemoteTransferFile,
            range: Option<ByteRange>,
        ) -> Result<DownloadedTransfer, WorkspaceError> {
            let mut state = self.0.lock().unwrap();
            state.counts.downloads += 1;
            if state.remote_file(&file.path)? != *file {
                return Err(WorkspaceError::Conflict);
            }
            let full = &state.remote[&file.path].bytes;
            let range = range.unwrap_or(ByteRange {
                start: 0,
                end_exclusive: full.len() as u64,
            });
            let bytes = full[range.start as usize..range.end_exclusive as usize].to_vec();
            Ok(DownloadedTransfer {
                content: TransferContent {
                    digest: byte_digest(&bytes).unwrap(),
                    size_bytes: bytes.len() as u64,
                    mode: file.content.mode.clone(),
                },
                source: LocalTransferSource::new(Cursor::new(bytes)),
                whole_file_verified: range.start == 0 && range.end_exclusive == full.len() as u64,
                range,
            })
        }

        async fn prepare_publication(
            &self,
            sealed: &SealedTransfer,
            request: &TransferPublicationRequest,
        ) -> Result<PreparedTransferPublication, WorkspaceError> {
            let mut state = self.0.lock().unwrap();
            state.counts.remote_prepares += 1;
            let mut prepared = PreparedTransferPublication {
                operation: OperationHandle {
                    preparation_id: operation_id().unwrap(),
                    invocation_id: Some(operation_id().unwrap()),
                    execution_id: None,
                    expires_at_unix_ms: Some(u64::MAX),
                },
                cwd_path: state.context.roots.remote.cwd.clone(),
                request_digest: digest(&(sealed, request)).unwrap(),
                sealed: sealed.clone(),
                request: request.clone(),
                review: json!({"mutating": true}),
            };
            if state.tamper_prepared {
                prepared.request.path = path(SECOND);
            }
            Ok(prepared)
        }

        async fn execute_publication(
            &self,
            prepared: &PreparedTransferPublication,
        ) -> Result<OperationStatus<TransferPublicationStatus>, WorkspaceError> {
            let (result, cancel) = {
                let mut state = self.0.lock().unwrap();
                state.counts.publishes += 1;
                let name = &prepared.request.path;
                if state.race_create {
                    state.put(&Side::Remote, name.as_str(), EXTERNAL, NodeKind::File);
                }
                let current = state.remote.get(name);
                let matches = match &prepared.request.condition {
                    MutationCondition::MustNotExist => current.is_none(),
                    MutationCondition::Matches(revision) => {
                        current.is_some_and(|file| &file.node.revision == revision)
                    }
                };
                let result = if !matches || state.fail_publish == Some(state.counts.publishes) {
                    OperationState::Failed {
                        error: OperationError {
                            code: OperationId::new("conflict").unwrap(),
                            message: PRECONDITION_FAILED.into(),
                        },
                        side_effects_possible: false,
                    }
                } else {
                    let bytes = state.stages[&prepared.sealed.stage.id].clone();
                    let mut created_directories = Vec::new();
                    for directory in &prepared.request.create_directories {
                        if state.remote.contains_key(directory) {
                            return Err(WorkspaceError::Conflict);
                        }
                        state.put(&Side::Remote, directory.as_str(), b"", NodeKind::Directory);
                        created_directories.push((
                            directory.clone(),
                            state.remote[directory].node.identity.clone(),
                        ));
                    }
                    state.put(&Side::Remote, name.as_str(), &bytes, NodeKind::File);
                    state.remote.get_mut(name).unwrap().mode =
                        prepared.sealed.stage.content.mode.clone();
                    let status = TransferPublicationStatus {
                        publication_id: prepared.request.publication_id.clone(),
                        state: TransferPublicationState::Completed,
                        file: Some(state.remote_file(name)?),
                        created_directories,
                    };
                    state
                        .publications
                        .insert(prepared.request.publication_id.clone(), status.clone());
                    OperationState::Completed {
                        result: status,
                        side_effects_possible: true,
                    }
                };
                if state.unknown_publish == Some(state.counts.publishes) {
                    return Err(WorkspaceError::IndeterminateOutcome);
                }
                let cancel = if state.cancel_in_publish {
                    state.cancel.take()
                } else {
                    None
                };
                (result, cancel)
            };
            if let Some(trigger) = cancel {
                trigger.cancel();
                return future::pending().await;
            }
            Ok(OperationStatus {
                handle: prepared.operation.clone(),
                state: result,
                progress: Vec::new(),
                progress_metadata: SequenceMetadata {
                    first_retained_sequence: None,
                    next_sequence: 0,
                    gap_before_first: false,
                },
            })
        }

        async fn publication_status(
            &self,
            prepared: &PreparedTransferPublication,
        ) -> Result<TransferPublicationStatus, WorkspaceError> {
            let mut state = self.0.lock().unwrap();
            state.counts.status += 1;
            Ok(state
                .publications
                .get(&prepared.request.publication_id)
                .cloned()
                .unwrap_or(TransferPublicationStatus {
                    publication_id: prepared.request.publication_id.clone(),
                    state: TransferPublicationState::Unknown,
                    file: None,
                    created_directories: Vec::new(),
                }))
        }

        async fn release_publication(
            &self,
            _: &PreparedTransferPublication,
        ) -> Result<ReleaseResult, WorkspaceError> {
            self.0.lock().unwrap().counts.releases += 1;
            Ok(ReleaseResult {
                state: OperationPhase::Completed,
                released: true,
            })
        }
    }

    #[test]
    fn terminal_directory_cleanup_is_retryable_and_history_can_compact() {
        smol::block_on(async {
            let mut fixture = Fixture::rotating();
            for index in 0..=TEST_ROTATION_RECORDS {
                let name = format!("directory-{index}");
                fixture
                    .fake
                    .0
                    .lock()
                    .unwrap()
                    .put(&Side::Local, &name, b"", NodeKind::Directory);
                let comparison = fixture.engine.compare(&CancelToken::none()).await.unwrap();
                let plan = fixture
                    .engine
                    .plan_selection(
                        &comparison,
                        TransferAction::Push,
                        &[path(&name)],
                        &CancelToken::none(),
                    )
                    .await
                    .unwrap();
                fixture.fake.0.lock().unwrap().defer_directory_release = true;
                let run = fixture
                    .engine
                    .execute(&plan, &mut fixture.journal, &CancelToken::none())
                    .await;
                assert!(run.stopped.is_none(), "{:?}", run.stopped);
                assert_eq!(run.cleanup_deferred.len(), 1);
                let run = fixture
                    .engine
                    .reconcile(&mut fixture.journal, &CancelToken::none())
                    .await;
                assert!(run.stopped.is_none(), "{:?}", run.stopped);
                assert_eq!(
                    run.outcomes.values().collect::<Vec<_>>(),
                    vec![&FileOutcome::Confirmed]
                );
                assert!(run.cleanup_deferred.is_empty());
            }
            let reopened = TransferJournal::new(fixture.state.path().join(JOURNAL_FILE)).unwrap();
            assert!(reopened.entries().unwrap().len() < TEST_ROTATION_RECORDS);
            assert!(
                reopened
                    .audit()
                    .unwrap()
                    .values()
                    .any(|audit| audit.confirmed > 0)
            );
            assert_eq!(
                fixture.fake.0.lock().unwrap().counts.publishes,
                TEST_ROTATION_RECORDS + 1
            );
        });
    }

    #[test_case(TransferAction::Push; "push")]
    #[test_case(TransferAction::Pull; "pull")]
    #[test_case(TransferAction::Seed; "seed")]
    fn empty_directory_effects_publish_and_reconcile_without_replay(action: TransferAction) {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            let plan = fixture.directory_plan(action).await;
            assert!(plan.review.files.is_empty());
            assert_eq!(plan.review.directories.len(), 1);
            let id = plan.review.directories[0].operation_id.clone();
            fixture.fake.0.lock().unwrap().unknown_publish = Some(1);
            let run = fixture
                .engine
                .execute(&plan, &mut fixture.journal, &CancelToken::none())
                .await;
            assert_eq!(run.outcomes[&id], FileOutcome::Unknown);
            let mut reopened =
                TransferJournal::new(fixture.state.path().join(JOURNAL_FILE)).unwrap();
            let run = fixture
                .engine
                .reconcile(&mut reopened, &CancelToken::none())
                .await;
            assert!(run.stopped.is_none(), "{:?}", run.stopped);
            assert_eq!(run.outcomes[&id], FileOutcome::Confirmed);
            assert!(run.cleanup_deferred.is_empty());
            assert!(reopened.base().unwrap().is_empty());
            let replay = fixture
                .engine
                .execute(&plan, &mut reopened, &CancelToken::none())
                .await;
            assert!(replay.stopped.is_none());
            assert_eq!(fixture.fake.0.lock().unwrap().counts.publishes, 1);
            assert_eq!(reopened.entries().unwrap()[0].created_directories.len(), 2);
        });
    }

    #[test]
    fn unknown_directory_effect_blocks_descendant_file_publication() {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            let plan = fixture.directory_plan(TransferAction::Push).await;
            fixture.fake.0.lock().unwrap().unknown_publish = Some(1);
            fixture
                .engine
                .execute(&plan, &mut fixture.journal, &CancelToken::none())
                .await;
            let child = format!("{EMPTY_DIRECTORY}/{FILE}");
            fixture.put(Side::Local, &child, BEFORE);
            let comparison = fixture.engine.compare(&CancelToken::none()).await.unwrap();
            let plan = fixture
                .engine
                .plan_selection(
                    &comparison,
                    TransferAction::Push,
                    &[path(&child)],
                    &CancelToken::none(),
                )
                .await
                .unwrap();
            let run = fixture
                .engine
                .execute(&plan, &mut fixture.journal, &CancelToken::none())
                .await;
            assert!(matches!(run.stopped, Some(TransferError::RecoveryRequired)));
            assert_eq!(fixture.fake.0.lock().unwrap().counts.publishes, 1);
        });
    }

    #[test]
    fn version_two_file_journals_load_without_rewriting_and_upgrade_on_change() {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            fixture.put(Side::Local, FILE, BEFORE);
            let plan = fixture.plan(TransferAction::Push, &[FILE]).await;
            fixture
                .journal
                .reserve(&plan, &plan.review.files[0])
                .unwrap();
            let path = fixture.state.path().join(JOURNAL_FILE);
            let mut data: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            data["version"] = json!(2);
            for record in data["entries"].as_object_mut().unwrap().values_mut() {
                for key in ["directory", "remote_directory", "local_directory"] {
                    record.as_object_mut().unwrap().remove(key);
                }
            }
            let before = serde_json::to_vec(&data).unwrap();
            fs::write(&path, &before).unwrap();
            assert_eq!(fixture.journal.entries().unwrap().len(), 1);
            assert_eq!(fs::read(&path).unwrap(), before);
            let run = fixture
                .engine
                .reconcile(&mut fixture.journal, &CancelToken::none())
                .await;
            assert!(run.stopped.is_none());
            let data: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            assert_eq!(data["version"], json!(3));
            assert_eq!(fixture.fake.0.lock().unwrap().counts.publishes, 0);
        });
    }

    #[test_case(TransferAction::Push; "push")]
    #[test_case(TransferAction::Pull; "pull")]
    fn newly_nonempty_source_directories_invalidate_review(action: TransferAction) {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            let source = if action == TransferAction::Pull {
                Side::Remote
            } else {
                Side::Local
            };
            let plan = fixture.directory_plan(action).await;
            fixture.put(source, &format!("{EMPTY_DIRECTORY}/{FILE}"), BEFORE);
            let run = fixture
                .engine
                .execute(&plan, &mut fixture.journal, &CancelToken::none())
                .await;
            assert!(matches!(run.stopped, Some(TransferError::Stale)));
            assert_eq!(fixture.fake.0.lock().unwrap().counts.publishes, 0);
        });
    }

    #[test_case(TransferAction::Push; "push")]
    #[test_case(TransferAction::Pull; "pull")]
    fn mixed_folder_selection_deduplicates_shared_ancestors_and_reports_skips(
        action: TransferAction,
    ) {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            let source = if action == TransferAction::Pull {
                Side::Remote
            } else {
                Side::Local
            };
            fixture.directory_plan(action.clone()).await;
            let file = format!("{FOLDER}/{FILE}");
            let excluded = format!("{FOLDER}/.env");
            fixture.put(source.clone(), &file, BEFORE);
            fixture.put(source.clone(), &excluded, BEFORE);
            fixture.put(source, &format!("{FOLDER}-other"), BEFORE);
            let comparison = fixture.engine.compare(&CancelToken::none()).await.unwrap();
            let plan = fixture
                .engine
                .plan_selection(
                    &comparison,
                    action,
                    &[path(FOLDER), path(&file)],
                    &CancelToken::none(),
                )
                .await
                .unwrap();
            assert_eq!(plan.review.files.len(), 1);
            assert_eq!(plan.review.directories.len(), 1);
            assert_eq!(plan.review.skipped, vec![path(&excluded)]);
            let run = fixture
                .engine
                .execute(&plan, &mut fixture.journal, &CancelToken::none())
                .await;
            assert!(run.stopped.is_none(), "{:?}", run.stopped);
            assert_eq!(run.outcomes.len(), 2);
            assert!(run.cleanup_deferred.is_empty());
        });
    }

    #[test_case(TransferAction::Push; "push")]
    #[test_case(TransferAction::Pull; "pull")]
    fn unsupported_directory_publication_fails_closed(action: TransferAction) {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.directory_plan(action.clone()).await;
            fixture.fake.0.lock().unwrap().directory_capability = false;
            assert!(!fixture.engine.supports_directory_publication());
            let comparison = fixture.engine.compare(&CancelToken::none()).await.unwrap();
            assert!(matches!(
                fixture
                    .engine
                    .plan_selection(&comparison, action, &[path(FOLDER)], &CancelToken::none())
                    .await,
                Err(TransferError::Workspace(WorkspaceError::UnsupportedEntry))
            ));
            assert_eq!(fixture.fake.0.lock().unwrap().counts.publishes, 0);
        });
    }

    #[test_case(TransferAction::Push; "push")]
    #[test_case(TransferAction::Pull; "pull")]
    fn directory_preparation_cannot_resurrect_cancelled_reservation(action: TransferAction) {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            let plan = fixture.directory_plan(action).await;
            fixture.fake.0.lock().unwrap().cancel_directory_preparation =
                Some(fixture.state.path().join(JOURNAL_FILE));
            let run = fixture
                .engine
                .execute(&plan, &mut fixture.journal, &CancelToken::none())
                .await;
            assert!(matches!(run.stopped, Some(TransferError::RecoveryRequired)));
            assert_eq!(fixture.fake.0.lock().unwrap().counts.publishes, 0);
            assert!(!fixture.journal.entries().unwrap()[0].state.blocks());
        });
    }

    #[test_case(JournalState::Reserved; "reserved")]
    #[test_case(JournalState::Prepared; "prepared")]
    fn stale_directory_reconciliation_cannot_clear_dispatched_state(snapshot_state: JournalState) {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            let plan = fixture.directory_plan(TransferAction::Push).await;
            let directory = &plan.review.directories[0];
            fixture.journal.reserve_directory(&plan, directory).unwrap();
            fixture
                .journal
                .update(&directory.operation_id, |entry| {
                    entry.state = snapshot_state;
                    Ok(())
                })
                .unwrap();
            let snapshot = fixture.journal.entries().unwrap().remove(0);
            let mut concurrent =
                TransferJournal::new(fixture.state.path().join(JOURNAL_FILE)).unwrap();
            concurrent
                .update(&directory.operation_id, |entry| {
                    entry.state = JournalState::Dispatched;
                    entry.cleanup_pending = true;
                    Ok(())
                })
                .unwrap();
            assert!(matches!(
                fixture
                    .engine
                    .reconcile_directory(&snapshot, &mut fixture.journal, &CancelToken::none())
                    .await,
                Err(TransferError::RecoveryRequired)
            ));
            let current = concurrent.entries().unwrap().remove(0);
            assert_eq!(current.state, JournalState::Dispatched);
            assert!(current.cleanup_pending);
        });
    }

    #[test_case(Side::Local; "local")]
    #[test_case(Side::Remote; "remote")]
    fn inspection_is_bounded_revision_bound_and_never_prepares_publication(side: Side) {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            fixture.engine.limits.inspect_bytes = BEFORE.len() - 1;
            fixture.put(side.clone(), FILE, BEFORE);
            let comparison = fixture.engine.compare(&CancelToken::none()).await.unwrap();
            let preview = fixture
                .engine
                .inspect_preview(&comparison, &path(FILE), &CancelToken::none())
                .await
                .unwrap();
            let preview = if side == Side::Local {
                preview.local_preview
            } else {
                preview.remote_preview
            };
            assert!(
                matches!(preview, Some(FilePreview::TextPrefix { truncated: true, text }) if text.len() == BEFORE.len() - 1)
            );
            fixture.put(side, FILE, AFTER);
            assert!(matches!(
                fixture
                    .engine
                    .inspect_preview(&comparison, &path(FILE), &CancelToken::none())
                    .await,
                Err(TransferError::Stale)
            ));
            let state = fixture.fake.0.lock().unwrap();
            assert_eq!(
                state.counts.local_prepares
                    + state.counts.remote_prepares
                    + state.counts.publishes
                    + state.counts.stages,
                0
            );
            assert!(fixture.journal.entries().unwrap().is_empty());
        });
    }

    #[test]
    fn compare_is_read_only_and_classifies_without_timestamp_winners() {
        smol::block_on(async {
            let fixture = Fixture::new();
            for side in [Side::Local, Side::Remote] {
                fixture.put(side, FILE, BEFORE);
            }
            fixture.put(Side::Local, SECOND, AFTER);
            fixture.put(Side::Remote, THIRD, BEFORE);
            fixture.put(Side::Local, "conflict", BEFORE);
            fixture.put(Side::Remote, "conflict", AFTER);
            let comparison = fixture.engine.compare(&CancelToken::none()).await.unwrap();
            let kinds = comparison
                .rows()
                .iter()
                .map(|row| (row.path.clone(), row.kind.clone()))
                .collect::<BTreeMap<_, _>>();
            assert_eq!(kinds[&path(FILE)], ComparisonKind::Equal);
            assert_eq!(kinds[&path(SECOND)], ComparisonKind::LocalOnly);
            assert_eq!(kinds[&path(THIRD)], ComparisonKind::RemoteOnly);
            assert_eq!(kinds[&path("conflict")], ComparisonKind::Conflict);
            let state = fixture.fake.0.lock().unwrap();
            assert_eq!(
                state.counts.stages
                    + state.counts.publishes
                    + state.counts.local_prepares
                    + state.counts.remote_prepares
                    + state.counts.downloads
                    + state.counts.opens,
                0
            );
            assert!(!fixture.state.path().join(JOURNAL_FILE).exists());
            assert_eq!(fs::read_dir(fixture.root.path()).unwrap().count(), 0);
        });
    }

    #[test_case(TransferAction::Seed; "seed")]
    #[test_case(TransferAction::Push; "push")]
    #[test_case(TransferAction::Pull; "pull")]
    fn binary_six_mib_explicit_selection_and_no_deletion(action: TransferAction) {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            let (source, destination) = if action == TransferAction::Pull {
                (Side::Remote, Side::Local)
            } else {
                (Side::Local, Side::Remote)
            };
            let bytes = vec![0_u8; BINARY_SIZE];
            fixture.put(source.clone(), FILE, &bytes);
            fixture.put(source.clone(), SECOND, AFTER);
            fixture.put(destination.clone(), THIRD, BEFORE);
            fixture
                .fake
                .0
                .lock()
                .unwrap()
                .files_mut(&source)
                .get_mut(&path(FILE))
                .unwrap()
                .mode = TransferMode::Executable;
            let plan = fixture.plan(action, &[FILE]).await;
            assert_eq!(plan.review.rollback, RollbackCoverage::None);
            assert!(!plan.review.atomic_across_files);
            let preview = if source == Side::Local {
                &plan.review.files[0].local_preview
            } else {
                &plan.review.files[0].remote_preview
            };
            assert!(
                matches!(preview, Some(FilePreview::BinarySummary { content }) if content.size_bytes == BINARY_SIZE as u64)
            );
            let run = fixture
                .engine
                .execute(&plan, &mut fixture.journal, &CancelToken::none())
                .await;
            assert!(run.stopped.is_none(), "{:?}", run.stopped);
            let state = fixture.fake.0.lock().unwrap();
            assert_eq!(state.files(&destination)[&path(FILE)].bytes, bytes);
            assert_eq!(
                state.files(&destination)[&path(FILE)].mode,
                TransferMode::Executable
            );
            assert_eq!(state.files(&destination)[&path(THIRD)].bytes, BEFORE);
            assert!(!state.files(&destination).contains_key(&path(SECOND)));
            assert_eq!(state.counts.publishes, 1);
            assert_eq!(fixture.journal.base().unwrap().len(), 1);
        });
    }

    #[test_case(Side::Local; "local")]
    #[test_case(Side::Remote; "remote")]
    fn protected_ignored_and_unsupported_entries_never_read(side: Side) {
        smol::block_on(async {
            let fixture = Fixture::new();
            {
                let mut state = fixture.fake.0.lock().unwrap();
                for name in [
                    ".env",
                    ".env.production",
                    ".git",
                    ".ssh",
                    "secret.key",
                    "credentials.json",
                    "target",
                ] {
                    state.put(&side, name, AFTER, NodeKind::File);
                }
                for (name, kind) in [
                    ("link", NodeKind::Symlink),
                    ("mount", NodeKind::Mount),
                    ("fifo", NodeKind::Special),
                    ("repo", NodeKind::NestedRepository),
                ] {
                    state.put(&side, name, AFTER, kind);
                }
                state.put(&side, "ignored", AFTER, NodeKind::File);
                state
                    .files_mut(&side)
                    .get_mut(&path("ignored"))
                    .unwrap()
                    .node
                    .ignored = Some(true);
            }
            let comparison = fixture.engine.compare(&CancelToken::none()).await.unwrap();
            assert!(comparison.rows().iter().all(|row| matches!(
                row.kind,
                ComparisonKind::Excluded | ComparisonKind::Unsupported
            )));
            let state = fixture.fake.0.lock().unwrap();
            assert_eq!(
                state.counts.stats + state.counts.opens + state.counts.downloads,
                0
            );
        });
    }

    #[test]
    fn nested_repository_is_discovered_before_hashing_children() {
        smol::block_on(async {
            let fixture = Fixture::new();
            {
                let mut state = fixture.fake.0.lock().unwrap();
                state.put(&Side::Remote, "nested", b"", NodeKind::Directory);
                state.put(&Side::Remote, "nested/.git", b"", NodeKind::File);
                state.put(&Side::Remote, "nested/code", AFTER, NodeKind::File);
            }
            let comparison = fixture.engine.compare(&CancelToken::none()).await.unwrap();
            assert_eq!(comparison.rows()[0].kind, ComparisonKind::Unsupported);
            assert_eq!(
                comparison.rows()[0].remote_kind,
                Some(NodeKind::NestedRepository)
            );
            assert!(comparison.complete());
            assert_eq!(fixture.fake.0.lock().unwrap().counts.stats, 0);
        });
    }

    #[test_case(Side::Local; "local_incomplete")]
    #[test_case(Side::Remote; "remote_incomplete")]
    fn incomplete_never_means_absent_or_seedable(side: Side) {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.put(Side::Local, FILE, AFTER);
            fixture.fake.0.lock().unwrap().incomplete = Some(side);
            let comparison = fixture.engine.compare(&CancelToken::none()).await.unwrap();
            assert!(!comparison.complete());
            assert!(
                comparison
                    .rows()
                    .iter()
                    .all(|row| row.kind == ComparisonKind::Incomplete)
            );
            assert!(matches!(
                fixture
                    .engine
                    .plan(
                        &comparison,
                        TransferAction::Seed,
                        &[path(FILE)],
                        &BTreeSet::new(),
                        &CancelToken::none()
                    )
                    .await,
                Err(TransferError::Selection)
            ));
        });
    }

    #[test_case(Side::Local; "local_revision")]
    #[test_case(Side::Remote; "remote_revision")]
    fn stale_either_side_after_remote_review_releases_stage(side: Side) {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            fixture.put(Side::Local, FILE, AFTER);
            fixture.put(Side::Remote, FILE, BEFORE);
            let plan = fixture.plan(TransferAction::Push, &[FILE]).await;
            fixture.fake.0.lock().unwrap().mutate_review = Some(side);
            let run = fixture
                .engine
                .execute(&plan, &mut fixture.journal, &CancelToken::none())
                .await;
            assert!(matches!(run.stopped, Some(TransferError::Stale)));
            let state = fixture.fake.0.lock().unwrap();
            assert_eq!(state.counts.publishes, 0);
            assert!(state.stages.is_empty());
            assert!(fixture.journal.base().unwrap().is_empty());
        });
    }

    #[test_case(TransferAction::Seed; "seed_no_replace")]
    #[test_case(TransferAction::Pull; "pull_no_replace")]
    fn publication_race_never_replaces_new_destination(action: TransferAction) {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            let source = if action == TransferAction::Pull {
                Side::Remote
            } else {
                Side::Local
            };
            fixture.put(source, FILE, AFTER);
            let plan = fixture.plan(action.clone(), &[FILE]).await;
            fixture.fake.0.lock().unwrap().race_create = true;
            let run = fixture
                .engine
                .execute(&plan, &mut fixture.journal, &CancelToken::none())
                .await;
            assert!(matches!(
                run.stopped,
                Some(TransferError::PublicationRejected)
            ));
            let side = if action == TransferAction::Pull {
                Side::Local
            } else {
                Side::Remote
            };
            assert_eq!(
                fixture.fake.0.lock().unwrap().files(&side)[&path(FILE)].bytes,
                EXTERNAL
            );
            assert!(fixture.journal.base().unwrap().is_empty());
        });
    }

    #[test]
    fn partial_unknown_reopens_and_reconciles_ids_without_replaying_post() {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            for name in [FILE, SECOND, THIRD] {
                fixture.put(Side::Local, name, AFTER);
            }
            let plan = fixture
                .plan(TransferAction::Push, &[FILE, SECOND, THIRD])
                .await;
            fixture.fake.0.lock().unwrap().unknown_publish = Some(2);
            let run = fixture
                .engine
                .execute(&plan, &mut fixture.journal, &CancelToken::none())
                .await;
            assert_eq!(
                run.outcomes[&plan.review.files[0].operation_id],
                FileOutcome::Confirmed
            );
            assert_eq!(
                run.outcomes[&plan.review.files[1].operation_id],
                FileOutcome::Unknown
            );
            assert_eq!(fixture.journal.base().unwrap().len(), 1);
            fixture.journal =
                TransferJournal::new(fixture.state.path().join(JOURNAL_FILE)).unwrap();
            let blocked = fixture
                .engine
                .execute(&plan, &mut fixture.journal, &CancelToken::none())
                .await;
            assert!(matches!(
                blocked.stopped,
                Some(TransferError::RecoveryRequired)
            ));
            assert_eq!(fixture.fake.0.lock().unwrap().counts.publishes, 2);
            let recovered = fixture
                .engine
                .reconcile(&mut fixture.journal, &CancelToken::none())
                .await;
            assert!(recovered.stopped.is_none(), "{:?}", recovered.stopped);
            assert_eq!(fixture.journal.base().unwrap().len(), 2);
            assert_eq!(fixture.fake.0.lock().unwrap().counts.status, 1);
            let resumed = fixture
                .engine
                .execute(&plan, &mut fixture.journal, &CancelToken::none())
                .await;
            assert!(resumed.stopped.is_none(), "{:?}", resumed.stopped);
            assert_eq!(fixture.fake.0.lock().unwrap().counts.publishes, 3);
            assert_eq!(fixture.journal.base().unwrap().len(), 3);
            let journal = fs::read_to_string(fixture.state.path().join(JOURNAL_FILE)).unwrap();
            assert!(!journal.contains(str::from_utf8(AFTER).unwrap()));
        });
    }

    #[test]
    fn partial_failure_advances_only_confirmed_files() {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            for name in [FILE, SECOND, THIRD] {
                fixture.put(Side::Local, name, AFTER);
            }
            let plan = fixture
                .plan(TransferAction::Push, &[FILE, SECOND, THIRD])
                .await;
            fixture.fake.0.lock().unwrap().fail_publish = Some(2);
            let run = fixture
                .engine
                .execute(&plan, &mut fixture.journal, &CancelToken::none())
                .await;
            assert_eq!(
                run.outcomes[&plan.review.files[0].operation_id],
                FileOutcome::Confirmed
            );
            assert_eq!(
                run.outcomes[&plan.review.files[1].operation_id],
                FileOutcome::Failed
            );
            assert_eq!(fixture.journal.base().unwrap().len(), 1);
            assert!(
                !fixture
                    .fake
                    .0
                    .lock()
                    .unwrap()
                    .remote
                    .contains_key(&path(THIRD))
            );
        });
    }

    #[test_case(false, false; "unknown_is_not_digest_only_success")]
    #[test_case(true, false; "durable_status_reconciles_without_replay")]
    #[test_case(true, true; "changed_destination_rejects_stale_completed_status")]
    fn local_recovery_requires_durable_status_and_current_bytes(durable: bool, changed: bool) {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            fixture.put(Side::Remote, FILE, AFTER);
            let plan = fixture.plan(TransferAction::Pull, &[FILE]).await;
            fixture.fake.0.lock().unwrap().unknown_publish = Some(1);
            let run = fixture
                .engine
                .execute(&plan, &mut fixture.journal, &CancelToken::none())
                .await;
            assert_eq!(run.outcomes.values().next(), Some(&FileOutcome::Unknown));
            fixture.fake.0.lock().unwrap().local_status_available = durable;
            if changed {
                fixture.put(Side::Local, FILE, EXTERNAL);
            }
            fixture.journal =
                TransferJournal::new(fixture.state.path().join(JOURNAL_FILE)).unwrap();
            let recovered = fixture
                .engine
                .reconcile(&mut fixture.journal, &CancelToken::none())
                .await;
            assert_eq!(
                recovered.outcomes.values().next(),
                Some(&if durable && !changed {
                    FileOutcome::Confirmed
                } else {
                    FileOutcome::Unknown
                })
            );
            assert_eq!(
                fixture.journal.base().unwrap().len(),
                usize::from(durable && !changed)
            );
            assert_eq!(
                fixture.journal.entries().unwrap()[0].state,
                if durable && !changed {
                    JournalState::Confirmed
                } else {
                    JournalState::Unknown
                }
            );
            assert_eq!(fixture.fake.0.lock().unwrap().counts.publishes, 1);
        });
    }

    #[test_case(false; "stop_after_partial_success")]
    #[test_case(true; "cancel_inflight_publication")]
    fn cancellation_preserves_applied_outcomes(inflight: bool) {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            for name in [FILE, SECOND] {
                fixture.put(Side::Local, name, AFTER);
            }
            let plan = fixture.plan(TransferAction::Push, &[FILE, SECOND]).await;
            let (trigger, token) = CancelToken::new();
            {
                let mut state = fixture.fake.0.lock().unwrap();
                state.cancel = Some(trigger);
                state.cancel_after_confirm = !inflight;
                state.cancel_in_publish = inflight;
            }
            let run = fixture
                .engine
                .execute(&plan, &mut fixture.journal, &token)
                .await;
            assert!(matches!(run.stopped, Some(TransferError::Cancelled)));
            let state = fixture.fake.0.lock().unwrap();
            assert_eq!(state.counts.publishes, 1);
            assert_eq!(state.remote[&path(FILE)].bytes, AFTER);
            assert!(!state.remote.contains_key(&path(SECOND)));
            assert_eq!(
                fixture.journal.base().unwrap().len(),
                usize::from(!inflight)
            );
            assert_eq!(
                run.outcomes[&plan.review.files[0].operation_id],
                if inflight {
                    FileOutcome::Unknown
                } else {
                    FileOutcome::Confirmed
                }
            );
        });
    }

    #[test]
    fn cancelling_after_staging_releases_known_leases_without_publication() {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            fixture.put(Side::Local, FILE, AFTER);
            let plan = fixture.plan(TransferAction::Push, &[FILE]).await;
            let (trigger, token) = CancelToken::new();
            {
                let mut state = fixture.fake.0.lock().unwrap();
                state.cancel = Some(trigger);
                state.cancel_phase = Some(TransferPhase::Sealing);
            }
            let run = fixture
                .engine
                .execute(&plan, &mut fixture.journal, &token)
                .await;
            assert!(matches!(run.stopped, Some(TransferError::Cancelled)));
            assert!(run.cleanup_deferred.is_empty());
            let state = fixture.fake.0.lock().unwrap();
            assert_eq!(state.counts.stages, 1);
            assert_eq!(state.counts.publishes, 0);
            assert!(state.stages.is_empty());
        });
    }

    #[test]
    fn dirty_buffer_blocks_pull_and_releases_private_preparation() {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            fixture.put(Side::Remote, FILE, AFTER);
            fixture.put(Side::Local, FILE, BEFORE);
            let plan = fixture.plan(TransferAction::Pull, &[FILE]).await;
            fixture.fake.0.lock().unwrap().dirty = true;
            let run = fixture
                .engine
                .execute(&plan, &mut fixture.journal, &CancelToken::none())
                .await;
            assert!(matches!(run.stopped, Some(TransferError::DirtyBuffer)));
            let state = fixture.fake.0.lock().unwrap();
            assert_eq!(state.local[&path(FILE)].bytes, BEFORE);
            assert!(state.local_preparations.is_empty());
            assert_eq!(state.counts.publishes, 0);
        });
    }

    #[test_case(0; "filter_configuration")]
    #[test_case(1; "effective_ignores")]
    #[test_case(2; "remote_generation")]
    #[test_case(3; "remote_principal")]
    #[test_case(4; "remote_cwd")]
    #[test_case(5; "local_root_replacement")]
    fn changed_root_or_digest_invalidates_review(change: usize) {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            fixture.put(Side::Local, FILE, AFTER);
            let plan = fixture.plan(TransferAction::Push, &[FILE]).await;
            match change {
                0 => {
                    fixture.engine.filters =
                        TransferFilters::new(&TransferPolicy::default(), &["new/**".into()], false)
                            .unwrap()
                }
                1 => {
                    fixture.fake.0.lock().unwrap().context.local_ignore_digest =
                        byte_digest(b"changed ignores").unwrap()
                }
                2 => {
                    fixture.fake.0.lock().unwrap().context.roots.remote =
                        remote_root("generation-b", SUBJECT)
                }
                3 => {
                    fixture.fake.0.lock().unwrap().context.roots.remote =
                        remote_root(VERSION, "other")
                }
                4 => fixture.fake.0.lock().unwrap().context.roots.remote.cwd = path("other"),
                _ => {
                    let old = fixture.root.path().with_extension("old");
                    fs::rename(fixture.root.path(), &old).unwrap();
                    fs::create_dir(fixture.root.path()).unwrap();
                    fs::remove_dir(old).unwrap();
                }
            }
            let run = fixture
                .engine
                .execute(&plan, &mut fixture.journal, &CancelToken::none())
                .await;
            assert!(matches!(run.stopped, Some(TransferError::Stale)));
            assert_eq!(fixture.fake.0.lock().unwrap().counts.stages, 0);
        });
    }

    #[test_case(false; "bad_digest")]
    #[test_case(true; "tampered_prepared_path")]
    fn byte_and_prepared_integrity_are_not_approval(tamper: bool) {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            fixture.put(Side::Local, FILE, AFTER);
            let plan = fixture.plan(TransferAction::Push, &[FILE]).await;
            {
                let mut state = fixture.fake.0.lock().unwrap();
                state.corrupt_stream = !tamper;
                state.tamper_prepared = tamper;
            }
            let run = fixture
                .engine
                .execute(&plan, &mut fixture.journal, &CancelToken::none())
                .await;
            assert!(run.stopped.is_some());
            assert_eq!(fixture.fake.0.lock().unwrap().counts.publishes, 0);
            assert!(fixture.journal.base().unwrap().is_empty());
        });
    }

    #[test_case(LocalAccess::Export, TransferAction::Push; "export")]
    #[test_case(LocalAccess::Write, TransferAction::Pull; "write")]
    fn remote_or_plan_approval_never_grants_local_access(
        access: LocalAccess,
        action: TransferAction,
    ) {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            let side = if action == TransferAction::Pull {
                Side::Remote
            } else {
                Side::Local
            };
            fixture.put(side, FILE, AFTER);
            let plan = fixture.plan(action, &[FILE]).await;
            fixture.fake.0.lock().unwrap().deny_access = Some(access);
            let run = fixture
                .engine
                .execute(&plan, &mut fixture.journal, &CancelToken::none())
                .await;
            assert!(matches!(
                run.stopped,
                Some(TransferError::Workspace(WorkspaceError::PermissionDenied))
            ));
            let state = fixture.fake.0.lock().unwrap();
            assert_eq!(
                state.counts.publishes + state.counts.stages + state.counts.local_prepares,
                0
            );
        });
    }

    #[test]
    fn explicit_root_approval_precedes_any_traversal() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.fake.0.lock().unwrap().deny_roots = true;
            assert!(fixture.engine.compare(&CancelToken::none()).await.is_err());
            assert_eq!(fixture.fake.0.lock().unwrap().counts.lists, 0);
        });
    }

    #[test]
    fn parents_are_explicit_and_identity_bound_or_reviewed_creations() {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            {
                let mut state = fixture.fake.0.lock().unwrap();
                for side in [Side::Local, Side::Remote] {
                    state.put(&side, "src", b"", NodeKind::Directory);
                }
                state.put(&Side::Local, "src/file", AFTER, NodeKind::File);
            }
            let comparison = fixture.engine.compare(&CancelToken::none()).await.unwrap();
            assert!(matches!(
                fixture
                    .engine
                    .plan(
                        &comparison,
                        TransferAction::Push,
                        &[path("src/file")],
                        &BTreeSet::new(),
                        &CancelToken::none()
                    )
                    .await,
                Err(TransferError::Parent)
            ));
            let plan = fixture
                .engine
                .plan(
                    &comparison,
                    TransferAction::Push,
                    &[path("src/file")],
                    &BTreeSet::from([path("src")]),
                    &CancelToken::none(),
                )
                .await
                .unwrap();
            fixture
                .fake
                .0
                .lock()
                .unwrap()
                .remote
                .get_mut(&path("src"))
                .unwrap()
                .node
                .identity = ResourceId::new("replacement-directory").unwrap();
            let run = fixture
                .engine
                .execute(&plan, &mut fixture.journal, &CancelToken::none())
                .await;
            assert!(matches!(run.stopped, Some(TransferError::Stale)));
            fixture.fake.0.lock().unwrap().remote.remove(&path("src"));
            let comparison = fixture.engine.compare(&CancelToken::none()).await.unwrap();
            let seed = fixture
                .engine
                .plan(
                    &comparison,
                    TransferAction::Seed,
                    &[path("src/file")],
                    &BTreeSet::from([path("src")]),
                    &CancelToken::none(),
                )
                .await
                .unwrap();
            assert_eq!(seed.review.files[0].create_directories, [path("src")]);
        });
    }

    #[test_case(false, TransferAction::Seed; "seed_resumes_with_recorded_directory_identity")]
    #[test_case(true, TransferAction::Seed; "seed_replacement_parent_invalidates_remainder")]
    #[test_case(false, TransferAction::Pull; "pull_resumes_with_recorded_directory_identity")]
    #[test_case(true, TransferAction::Pull; "pull_replacement_parent_invalidates_remainder")]
    fn directory_outcomes_survive_per_file_reconciliation(replace: bool, action: TransferAction) {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            let source = if action == TransferAction::Pull {
                Side::Remote
            } else {
                Side::Local
            };
            let destination = if action == TransferAction::Pull {
                Side::Local
            } else {
                Side::Remote
            };
            {
                let mut state = fixture.fake.0.lock().unwrap();
                for parent in ["src", "src/deep"] {
                    state.put(&source, parent, b"", NodeKind::Directory);
                }
                for file in ["src/deep/one", "src/deep/two"] {
                    state.put(&source, file, AFTER, NodeKind::File);
                }
                state.unknown_publish = Some(1);
                state.local_status_available = true;
            }
            let comparison = fixture.engine.compare(&CancelToken::none()).await.unwrap();
            let plan = fixture
                .engine
                .plan(
                    &comparison,
                    action,
                    &[path("src/deep/one"), path("src/deep/two")],
                    &BTreeSet::from([path("src"), path("src/deep")]),
                    &CancelToken::none(),
                )
                .await
                .unwrap();
            let run = fixture
                .engine
                .execute(&plan, &mut fixture.journal, &CancelToken::none())
                .await;
            assert_eq!(
                run.outcomes[&plan.review.files[0].operation_id],
                FileOutcome::Unknown
            );
            fixture.journal =
                TransferJournal::new(fixture.state.path().join(JOURNAL_FILE)).unwrap();
            let recovered = fixture
                .engine
                .reconcile(&mut fixture.journal, &CancelToken::none())
                .await;
            assert!(recovered.stopped.is_none());
            assert_eq!(
                fixture.journal.entries().unwrap()[0]
                    .created_directories
                    .len(),
                2
            );
            if replace {
                fixture
                    .fake
                    .0
                    .lock()
                    .unwrap()
                    .files_mut(&destination)
                    .get_mut(&path("src/deep"))
                    .unwrap()
                    .node
                    .identity = ResourceId::new("rebound").unwrap();
            }
            let run = fixture
                .engine
                .execute(&plan, &mut fixture.journal, &CancelToken::none())
                .await;
            assert_eq!(run.stopped.is_some(), replace);
            assert_eq!(
                fixture.fake.0.lock().unwrap().counts.publishes,
                if replace { 1 } else { 2 }
            );
        });
    }

    #[test_case(0; "entry_budget")]
    #[test_case(1; "byte_budget")]
    #[test_case(2; "repeated_cursor")]
    #[test_case(3; "oversized_page")]
    #[test_case(4; "unattested_traversal")]
    fn quotas_and_missing_safety_are_fail_closed(case: usize) {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            for name in [FILE, SECOND] {
                fixture.put(Side::Local, name, AFTER);
            }
            match case {
                0 => fixture.engine.limits.max_entries = 1,
                1 => fixture.engine.limits.max_total_bytes = 1,
                2 => fixture.fake.0.lock().unwrap().repeated_cursor = true,
                3 => fixture.fake.0.lock().unwrap().bad_page = true,
                _ => fixture.fake.0.lock().unwrap().context.safe_local_traversal = false,
            }
            let comparison = fixture.engine.compare(&CancelToken::none()).await.unwrap();
            assert!(!comparison.complete());
            assert!(
                !comparison
                    .rows()
                    .iter()
                    .any(|row| row.kind == ComparisonKind::LocalOnly || row.path.is_root())
            );
            assert!(fixture.fake.0.lock().unwrap().counts.lists <= 4);
        });
    }

    #[test_case(Side::Local; "local_partial")]
    #[test_case(Side::Remote; "remote_partial")]
    fn partial_listing_keeps_nested_rows_without_a_root_row(partial: Side) {
        smol::block_on(async {
            let fixture = Fixture::new();
            {
                let mut state = fixture.fake.0.lock().unwrap();
                for side in [Side::Local, Side::Remote] {
                    state.put(&side, FOLDER, b"", NodeKind::Directory);
                    state.put(&side, NESTED_SAME, BEFORE, NodeKind::File);
                }
                state.put(&Side::Local, NESTED_CHANGED, BEFORE, NodeKind::File);
                state.put(&Side::Remote, NESTED_CHANGED, AFTER, NodeKind::File);
                state.put(&Side::Local, NESTED_LOCAL, AFTER, NodeKind::File);
                state.put(&Side::Remote, NESTED_REMOTE, AFTER, NodeKind::File);
                state.incomplete = Some(partial.clone());
            }
            let comparison = fixture.engine.compare(&CancelToken::none()).await.unwrap();
            let kinds = comparison
                .rows()
                .iter()
                .map(|row| (row.path.as_str(), row.kind.clone()))
                .collect::<BTreeMap<_, _>>();
            assert_eq!(
                kinds,
                BTreeMap::from([
                    (FOLDER, ComparisonKind::Equal),
                    (NESTED_CHANGED, ComparisonKind::Conflict),
                    (NESTED_LOCAL, ComparisonKind::Incomplete),
                    (NESTED_REMOTE, ComparisonKind::Incomplete),
                    (NESTED_SAME, ComparisonKind::Equal),
                ])
            );
            assert!(!comparison.complete());
            assert_eq!(
                comparison.scan(&partial).limits,
                BTreeSet::from([ScanLimit::WorkcellIncomplete])
            );
            let other = if partial == Side::Local {
                Side::Remote
            } else {
                Side::Local
            };
            assert!(comparison.scan(&other).complete());
            assert!(comparison.rows().iter().all(|row| !row.unlisted));
            assert!(matches!(
                fixture
                    .engine
                    .inspect_preview(&comparison, &path(NESTED_CHANGED), &CancelToken::none())
                    .await,
                Err(TransferError::PartialInventory)
            ));
            assert!(matches!(
                fixture
                    .engine
                    .plan(
                        &comparison,
                        TransferAction::Push,
                        &[path(NESTED_CHANGED)],
                        &BTreeSet::from([path(FOLDER)]),
                        &CancelToken::none()
                    )
                    .await,
                Err(TransferError::PartialInventory)
            ));
        });
    }

    #[test]
    fn unattested_traversal_marks_every_row_unsupported_without_a_root_row() {
        smol::block_on(async {
            let fixture = Fixture::new();
            {
                let mut state = fixture.fake.0.lock().unwrap();
                state.put(&Side::Remote, FOLDER, b"", NodeKind::Directory);
                state.put(&Side::Remote, NESTED_SAME, AFTER, NodeKind::File);
                state.context.safe_local_traversal = false;
            }
            let comparison = fixture.engine.compare(&CancelToken::none()).await.unwrap();
            assert!(comparison.scan(&Side::Local).unsupported);
            assert_eq!(comparison.rows().len(), 2);
            assert!(
                comparison
                    .rows()
                    .iter()
                    .all(|row| row.kind == ComparisonKind::Unsupported && !row.path.is_root())
            );
        });
    }

    #[test]
    fn every_folder_within_the_entry_budget_is_listed() {
        smol::block_on(async {
            let fixture = Fixture::new();
            let folders = OrchestrationLimits::default().max_entries - 1;
            {
                let mut state = fixture.fake.0.lock().unwrap();
                for index in 0..folders {
                    for side in [Side::Local, Side::Remote] {
                        state.put(&side, &index.to_string(), b"", NodeKind::Directory);
                    }
                }
            }
            let comparison = fixture.engine.compare(&CancelToken::none()).await.unwrap();
            for side in [Side::Local, Side::Remote] {
                assert!(comparison.scan(&side).complete(), "{side:?}");
            }
            assert_eq!(comparison.rows().len(), folders);
        });
    }

    #[test_case(ScanLimit::Depth; "depth")]
    #[test_case(ScanLimit::Entries; "entries")]
    fn exhausted_limits_mark_directories_unlisted(limit: ScanLimit) {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            {
                let mut state = fixture.fake.0.lock().unwrap();
                for side in [Side::Local, Side::Remote] {
                    state.put(&side, FOLDER, b"", NodeKind::Directory);
                    state.put(&side, NESTED_SAME, BEFORE, NodeKind::File);
                }
            }
            match limit {
                ScanLimit::Depth => fixture.engine.limits.max_depth = 1,
                _ => fixture.engine.limits.max_entries = 1,
            }
            let comparison = fixture.engine.compare(&CancelToken::none()).await.unwrap();
            let [folder] = comparison.rows() else {
                panic!("only the unlisted folder is compared");
            };
            assert_eq!(folder.path, path(FOLDER));
            assert_eq!(folder.kind, ComparisonKind::Equal);
            assert!(folder.unlisted);
            for side in [Side::Local, Side::Remote] {
                assert_eq!(comparison.scan(&side).limits, BTreeSet::from([limit]));
            }
        });
    }

    #[test]
    fn a_folder_without_an_ignore_decision_is_unlisted() {
        smol::block_on(async {
            let fixture = Fixture::new();
            {
                let mut state = fixture.fake.0.lock().unwrap();
                for side in [Side::Local, Side::Remote] {
                    state.put(&side, FOLDER, b"", NodeKind::Directory);
                    state.put(&side, NESTED_SAME, BEFORE, NodeKind::File);
                    state
                        .files_mut(&side)
                        .get_mut(&path(FOLDER))
                        .unwrap()
                        .node
                        .ignored = None;
                }
            }
            let comparison = fixture.engine.compare(&CancelToken::none()).await.unwrap();
            let [folder] = comparison.rows() else {
                panic!("the folder is never listed");
            };
            assert_eq!(folder.kind, ComparisonKind::Incomplete);
            assert!(folder.unlisted);
            for side in [Side::Local, Side::Remote] {
                assert_eq!(
                    comparison.scan(&side).limits,
                    BTreeSet::from([ScanLimit::ListingFailed])
                );
            }
        });
    }

    #[test]
    fn exclusion_reasons_are_reported_per_row() {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            fixture.engine.filters = TransferFilters::new(
                &TransferPolicy {
                    exclude: vec![GENERATED.into()],
                    ..TransferPolicy::default()
                },
                &[],
                true,
            )
            .unwrap();
            {
                let mut state = fixture.fake.0.lock().unwrap();
                for name in [PROTECTED, GENERATED, IGNORED, FILE, DOTFILE] {
                    state.put(&Side::Local, name, AFTER, NodeKind::File);
                }
                state
                    .files_mut(&Side::Local)
                    .get_mut(&path(IGNORED))
                    .unwrap()
                    .node
                    .ignored = Some(true);
                state.ignored_absence = Some(Side::Remote);
            }
            let comparison = fixture.engine.compare(&CancelToken::none()).await.unwrap();
            let reasons = comparison
                .rows()
                .iter()
                .map(|row| (row.path.as_str(), (row.kind.clone(), row.excluded)))
                .collect::<BTreeMap<_, _>>();
            let excluded = |reason| (ComparisonKind::Excluded, Some(reason));
            assert_eq!(
                reasons,
                BTreeMap::from([
                    (PROTECTED, excluded(ExclusionReason::Protected)),
                    (GENERATED, excluded(ExclusionReason::Pattern)),
                    (IGNORED, excluded(ExclusionReason::Gitignore)),
                    (FILE, excluded(ExclusionReason::Gitignore)),
                    (DOTFILE, excluded(ExclusionReason::Dotfile)),
                ])
            );
        });
    }

    #[test]
    fn inspection_reads_a_larger_utf8_safe_prefix_than_review() {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            fixture.put(Side::Local, FILE, &vec![b'x'; INSPECT_BYTES + 1]);
            fixture.put(Side::Remote, FILE, AFTER);
            fixture.put(Side::Local, SECOND, MULTIBYTE.as_bytes());
            fixture.put(Side::Remote, SECOND, BEFORE);
            let comparison = fixture.engine.compare(&CancelToken::none()).await.unwrap();
            let inspected = fixture
                .engine
                .inspect_preview(&comparison, &path(FILE), &CancelToken::none())
                .await
                .unwrap();
            assert!(
                matches!(&inspected.local_preview, Some(FilePreview::TextPrefix { text, truncated: true }) if text.len() == INSPECT_BYTES)
            );
            let plan = fixture.plan(TransferAction::Push, &[FILE]).await;
            assert!(
                matches!(&plan.review.files[0].local_preview, Some(FilePreview::TextPrefix { text, truncated: true }) if text.len() == fixture.engine.limits.preview_bytes)
            );
            fixture.engine.limits.inspect_bytes = MULTIBYTE_CUT;
            let inspected = fixture
                .engine
                .inspect_preview(&comparison, &path(SECOND), &CancelToken::none())
                .await
                .unwrap();
            assert_eq!(
                inspected.local_preview,
                Some(FilePreview::TextPrefix {
                    text: MULTIBYTE_PREFIX.into(),
                    truncated: true
                })
            );
        });
    }

    #[test]
    fn including_ignored_files_compares_them_and_invalidates_earlier_reviews() {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            {
                let mut state = fixture.fake.0.lock().unwrap();
                for name in [FILE, PROTECTED, IGNORED] {
                    state.put(&Side::Local, name, AFTER, NodeKind::File);
                }
                state
                    .files_mut(&Side::Local)
                    .get_mut(&path(IGNORED))
                    .unwrap()
                    .node
                    .ignored = Some(true);
            }
            let plan = fixture.plan(TransferAction::Push, &[FILE]).await;
            fixture.engine.filters = TransferFilters::new(
                &TransferPolicy {
                    respect_gitignore: false,
                    ..TransferPolicy::default()
                },
                &[],
                false,
            )
            .unwrap();
            let comparison = fixture.engine.compare(&CancelToken::none()).await.unwrap();
            let kinds = comparison
                .rows()
                .iter()
                .map(|row| (row.path.as_str(), (row.kind.clone(), row.excluded)))
                .collect::<BTreeMap<_, _>>();
            assert_eq!(kinds[IGNORED], (ComparisonKind::LocalOnly, None));
            assert_eq!(
                kinds[PROTECTED],
                (ComparisonKind::Excluded, Some(ExclusionReason::Protected))
            );
            let run = fixture
                .engine
                .execute(&plan, &mut fixture.journal, &CancelToken::none())
                .await;
            assert!(matches!(run.stopped, Some(TransferError::Stale)));
            assert_eq!(fixture.fake.0.lock().unwrap().counts.publishes, 0);
        });
    }

    #[test]
    fn text_preview_and_selection_limits_are_bounded() {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            fixture.put(Side::Local, FILE, &vec![b'x'; BINARY_SIZE]);
            let plan = fixture.plan(TransferAction::Push, &[FILE]).await;
            assert!(
                matches!(&plan.review.files[0].local_preview, Some(FilePreview::TextPrefix { text, truncated: true }) if text.len() == fixture.engine.limits.preview_bytes)
            );
            fixture.put(Side::Local, SECOND, AFTER);
            let comparison = fixture.engine.compare(&CancelToken::none()).await.unwrap();
            for selected in [
                vec![],
                vec![path(FILE), path(FILE)],
                vec![path("not-compared")],
            ] {
                assert!(matches!(
                    fixture
                        .engine
                        .plan(
                            &comparison,
                            TransferAction::Push,
                            &selected,
                            &BTreeSet::new(),
                            &CancelToken::none()
                        )
                        .await,
                    Err(TransferError::Selection)
                ));
            }
            fixture.engine.limits.max_selected = 1;
            assert!(matches!(
                fixture
                    .engine
                    .plan(
                        &comparison,
                        TransferAction::Push,
                        &[path(FILE), path(SECOND)],
                        &BTreeSet::new(),
                        &CancelToken::none()
                    )
                    .await,
                Err(TransferError::Selection)
            ));
        });
    }

    #[test_case(Side::Local; "local_destination")]
    #[test_case(Side::Remote; "remote_source")]
    fn pull_revalidates_both_sides_after_preparing(side: Side) {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            fixture.put(Side::Local, FILE, BEFORE);
            fixture.put(Side::Remote, FILE, AFTER);
            let plan = fixture.plan(TransferAction::Pull, &[FILE]).await;
            fixture.fake.0.lock().unwrap().mutate_review = Some(side);
            let run = fixture
                .engine
                .execute(&plan, &mut fixture.journal, &CancelToken::none())
                .await;
            assert!(
                matches!(run.stopped, Some(TransferError::Stale)),
                "{:?}",
                run.stopped
            );
            assert_eq!(fixture.fake.0.lock().unwrap().counts.publishes, 0);
            assert!(fixture.fake.0.lock().unwrap().local_preparations.is_empty());
        });
    }

    #[test_case(false; "changed_hash_unchanged_revision")]
    #[test_case(true; "changed_mode_unchanged_revision")]
    fn revision_alone_cannot_authorize_changed_content_or_metadata(mode: bool) {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            fixture.put(Side::Local, FILE, AFTER);
            let plan = fixture.plan(TransferAction::Push, &[FILE]).await;
            {
                let mut state = fixture.fake.0.lock().unwrap();
                let file = state.local.get_mut(&path(FILE)).unwrap();
                if mode {
                    file.mode = TransferMode::Executable;
                } else {
                    file.bytes.fill(b'x');
                }
            }
            let run = fixture
                .engine
                .execute(&plan, &mut fixture.journal, &CancelToken::none())
                .await;
            assert!(
                matches!(run.stopped, Some(TransferError::Stale)),
                "{:?}",
                run.stopped
            );
            assert_eq!(fixture.fake.0.lock().unwrap().counts.stages, 0);
        });
    }

    #[test]
    fn unknown_remote_status_does_not_unlock_or_advance_base() {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            fixture.put(Side::Local, FILE, AFTER);
            let plan = fixture.plan(TransferAction::Push, &[FILE]).await;
            fixture.fake.0.lock().unwrap().unknown_publish = Some(1);
            fixture
                .engine
                .execute(&plan, &mut fixture.journal, &CancelToken::none())
                .await;
            fixture.fake.0.lock().unwrap().publications.clear();
            let run = fixture
                .engine
                .reconcile(&mut fixture.journal, &CancelToken::none())
                .await;
            assert_eq!(
                run.outcomes[&plan.review.files[0].operation_id],
                FileOutcome::Unknown
            );
            assert!(fixture.journal.base().unwrap().is_empty());
            assert!(fixture.journal.entries().unwrap()[0].state.blocks());
            assert_eq!(fixture.fake.0.lock().unwrap().counts.publishes, 1);
        });
    }

    #[test]
    fn journal_reservation_blocks_other_handles_until_pre_dispatch_recovery() {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            fixture.put(Side::Local, FILE, AFTER);
            let old = fixture.plan(TransferAction::Push, &[FILE]).await;
            assert!(fixture.journal.reserve(&old, &old.review.files[0]).unwrap());
            let plan = fixture.plan(TransferAction::Push, &[FILE]).await;
            let mut reopened =
                TransferJournal::new(fixture.state.path().join(JOURNAL_FILE)).unwrap();
            let blocked = fixture
                .engine
                .execute(&plan, &mut reopened, &CancelToken::none())
                .await;
            assert!(matches!(
                blocked.stopped,
                Some(TransferError::RecoveryRequired)
            ));
            fixture
                .engine
                .reconcile(&mut reopened, &CancelToken::none())
                .await;
            assert_eq!(
                reopened.entries().unwrap()[0].state,
                JournalState::Cancelled
            );
            let run = fixture
                .engine
                .execute(&plan, &mut reopened, &CancelToken::none())
                .await;
            assert!(run.stopped.is_none(), "{:?}", run.stopped);
        });
    }

    #[test_case(ROTATE_RECORDS - 1, false; "below_threshold")]
    #[test_case(ROTATE_RECORDS, true; "at_threshold")]
    fn journal_default_rotation_threshold(count: usize, rotates: bool) {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            fixture.put(Side::Local, FILE, AFTER);
            let first = fixture.plan(TransferAction::Push, &[FILE]).await;
            assert!(
                fixture
                    .journal
                    .reserve(&first, &first.review.files[0])
                    .unwrap()
            );
            fixture.fill_journal(count, "Failed", false);
            let plan = fixture.plan(TransferAction::Push, &[FILE]).await;
            assert!(
                fixture
                    .journal
                    .reserve(&plan, &plan.review.files[0])
                    .unwrap()
            );
            let reopened = TransferJournal::new(fixture.state.path().join(JOURNAL_FILE)).unwrap();
            assert_eq!(
                reopened.entries().unwrap().len(),
                if rotates { 1 } else { count + 1 }
            );
            let audit = reopened.audit().unwrap();
            if rotates {
                assert_eq!(audit.len(), 1);
                let audit = audit.values().next().unwrap();
                assert_eq!(audit.pages, 1);
                assert_eq!(audit.failed as usize, count);
            } else {
                assert!(audit.is_empty());
            }
        });
    }

    #[test_case("Reserved", false; "reserved")]
    #[test_case("Prepared", false; "prepared")]
    #[test_case("Dispatched", false; "dispatched")]
    #[test_case("Unknown", false; "unknown")]
    #[test_case("Confirmed", true; "cleanup_pending")]
    #[test_case("Failed", true; "failed_cleanup_pending")]
    #[test_case("Cancelled", true; "cancelled_cleanup_pending")]
    #[test_case("Failed", false; "failed_rotates")]
    #[test_case("Cancelled", false; "cancelled_rotates")]
    fn journal_capacity_rotates_only_terminal_records(state: &str, cleanup_pending: bool) {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            fixture.put(Side::Local, FILE, AFTER);
            let old = fixture.plan(TransferAction::Push, &[FILE]).await;
            fixture.journal.reserve(&old, &old.review.files[0]).unwrap();
            fixture.fill_journal(super::journal::MAX_RECORDS, state, cleanup_pending);
            let plan = fixture.plan(TransferAction::Push, &[FILE]).await;
            let run = fixture
                .engine
                .execute(&plan, &mut fixture.journal, &CancelToken::none())
                .await;
            if cleanup_pending || !matches!(state, "Failed" | "Cancelled") {
                assert!(matches!(run.stopped, Some(TransferError::JournalQuota)));
                assert_eq!(fixture.fake.0.lock().unwrap().counts.stages, 0);
                assert_eq!(
                    fixture.journal.entries().unwrap().len(),
                    super::journal::MAX_RECORDS
                );
            } else {
                assert!(run.stopped.is_none(), "{:?}", run.stopped);
                assert_eq!(fixture.journal.entries().unwrap().len(), 1);
                assert_eq!(fixture.journal.base().unwrap().len(), 1);
                let audit = fixture.journal.audit().unwrap();
                let audit = audit.values().next().unwrap();
                assert_eq!(
                    (audit.failed + audit.cancelled) as usize,
                    super::journal::MAX_RECORDS
                );
            }
        });
    }

    #[test_case("v1"; "old_version")]
    #[test_case("future"; "future_version")]
    #[test_case("archives"; "missing_archives")]
    #[test_case("created_directories"; "missing_directory_recovery")]
    #[test_case("local_review"; "missing_local_recovery")]
    fn unsupported_journals_are_rejected_without_rewriting(shape: &str) {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            fixture.put(Side::Local, FILE, AFTER);
            let plan = fixture.plan(TransferAction::Push, &[FILE]).await;
            fixture
                .journal
                .reserve(&plan, &plan.review.files[0])
                .unwrap();
            let path = fixture.state.path().join(JOURNAL_FILE);
            let mut data: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            match shape {
                "v1" => data["version"] = json!(1),
                "future" => data["version"] = json!(4),
                "archives" => {
                    data.as_object_mut().unwrap().remove(shape);
                }
                _ => {
                    data["entries"]
                        .as_object_mut()
                        .unwrap()
                        .values_mut()
                        .next()
                        .unwrap()
                        .as_object_mut()
                        .unwrap()
                        .remove(shape);
                }
            }
            let before = serde_json::to_vec(&data).unwrap();
            fs::write(&path, &before).unwrap();
            assert!(matches!(
                fixture.journal.entries(),
                Err(TransferError::Journal)
            ));
            assert!(matches!(
                fixture.journal.reserve(&plan, &plan.review.files[0]),
                Err(TransferError::Journal)
            ));
            assert_eq!(fs::read(&path).unwrap(), before);
            assert_eq!(fixture.fake.0.lock().unwrap().counts.stages, 0);
        });
    }

    #[test]
    fn journal_compaction_process_writer() {
        let Some(path) = std::env::var_os(JOURNAL_PROCESS_PATH) else {
            return;
        };
        let id = OperationId::new(std::env::var(JOURNAL_PROCESS_ID).unwrap()).unwrap();
        let mut journal = TransferJournal::new(path.into()).unwrap();
        journal
            .update(&id, |entry| {
                entry.state = JournalState::Unknown;
                entry.cleanup_pending = true;
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn journal_compaction_cas_cannot_erase_another_process_update() {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            fixture.put(Side::Local, FILE, AFTER);
            fixture.put(Side::Local, SECOND, AFTER);
            let unresolved = fixture.plan(TransferAction::Push, &[FILE]).await;
            let id = &unresolved.review.files[0].operation_id;
            fixture
                .journal
                .reserve(&unresolved, &unresolved.review.files[0])
                .unwrap();
            let retired = fixture.plan(TransferAction::Push, &[SECOND]).await;
            fixture
                .journal
                .reserve(&retired, &retired.review.files[0])
                .unwrap();
            fixture
                .journal
                .update(&retired.review.files[0].operation_id, |entry| {
                    entry.state = JournalState::Failed;
                    Ok(())
                })
                .unwrap();
            let result = fixture.journal.compact_before_commit(|| {
                let status = Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", JOURNAL_PROCESS_TEST, "--nocapture"])
                    .env(
                        JOURNAL_PROCESS_PATH,
                        fixture.state.path().join(JOURNAL_FILE),
                    )
                    .env(JOURNAL_PROCESS_ID, id.as_str())
                    .status()
                    .unwrap();
                assert!(status.success());
            });
            assert!(matches!(
                result,
                Err(TransferError::Storage(PrivateFileError::Conflict))
            ));
            let mut reopened =
                TransferJournal::new(fixture.state.path().join(JOURNAL_FILE)).unwrap();
            assert!(reopened.audit().unwrap().is_empty());
            assert_eq!(reopened.entries().unwrap().len(), 2);
            reopened.compact_before_commit(|| {}).unwrap();
            let entries = reopened.entries().unwrap();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].operation_id, *id);
            assert_eq!(entries[0].state, JournalState::Unknown);
            assert!(entries[0].cleanup_pending);
            assert!(matches!(
                reopened.reserve(&retired, &retired.review.files[0]),
                Err(TransferError::ReviewConsumed)
            ));
            assert_eq!(reopened.audit().unwrap().values().next().unwrap().failed, 1);
        });
    }

    #[test_case(false; "archive_root")]
    #[test_case(true; "archive_subtree")]
    fn journal_archive_cannot_be_transferred(subtree: bool) {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            let archive = fixture
                .state
                .path()
                .join(JOURNAL_FILE)
                .with_extension("archive");
            let selected = if subtree {
                archive.join(SUBJECT)
            } else {
                archive
            };
            fs::create_dir_all(&selected).unwrap();
            fixture.fake.0.lock().unwrap().context.roots.local =
                LocalRootIdentity::capture(&selected).unwrap();
            fixture.put(Side::Local, FILE, AFTER);
            let plan = fixture.plan(TransferAction::Push, &[FILE]).await;
            assert!(matches!(
                fixture.journal.reserve(&plan, &plan.review.files[0]),
                Err(TransferError::Journal)
            ));
            assert!(!fixture.state.path().join(JOURNAL_FILE).exists());
        });
    }

    #[test_case(TransferAction::Push; "push")]
    #[test_case(TransferAction::Pull; "pull")]
    fn journal_rotates_repeated_successes_and_reopens_without_replay(action: TransferAction) {
        smol::block_on(async {
            let mut fixture = Fixture::rotating();
            let source = if action == TransferAction::Push {
                Side::Local
            } else {
                Side::Remote
            };
            fixture.put(source.clone(), FILE, AFTER);
            let first = fixture.plan(action.clone(), &[FILE]).await;
            let total = TEST_ROTATION_RECORDS * ROTATION_PASSES + 1;
            let mut last = first.review.files[0].operation_id.clone();
            for index in 0..total {
                let plan = if index == 0 {
                    &first
                } else {
                    fixture.put(source.clone(), FILE, &index.to_le_bytes());
                    &fixture.plan(action.clone(), &[FILE]).await
                };
                let run = fixture
                    .engine
                    .execute(plan, &mut fixture.journal, &CancelToken::none())
                    .await;
                assert!(run.stopped.is_none(), "{index}: {:?}", run.stopped);
                last = plan.review.files[0].operation_id.clone();
                assert_eq!(run.outcomes[&last], FileOutcome::Confirmed);
            }
            let mut reopened =
                TransferJournal::new(fixture.state.path().join(JOURNAL_FILE)).unwrap();
            let entries = reopened.entries().unwrap();
            assert!(entries.len() < TEST_ROTATION_RECORDS);
            assert!(
                !entries
                    .iter()
                    .any(|entry| entry.operation_id == first.review.files[0].operation_id)
            );
            let base = reopened.base().unwrap();
            assert_eq!(base.len(), 1);
            assert_eq!(base[0].operation_id, last);
            let audit = reopened.audit().unwrap();
            assert_eq!(audit.len(), 1);
            assert_eq!(
                audit.values().next().unwrap().pages as usize,
                ROTATION_PASSES
            );
            assert_eq!(
                audit.values().next().unwrap().confirmed as usize + entries.len(),
                total
            );
            let publishes = fixture.fake.0.lock().unwrap().counts.publishes;
            let writes = fixture.fake.0.lock().unwrap().counts.writes;
            let replay = fixture
                .engine
                .execute(&first, &mut reopened, &CancelToken::none())
                .await;
            assert!(replay.stopped.is_none(), "{:?}", replay.stopped);
            assert_eq!(fixture.fake.0.lock().unwrap().counts.publishes, publishes);
            assert_eq!(fixture.fake.0.lock().unwrap().counts.writes, writes);
        });
    }

    #[test_case(JournalState::Reserved, false; "reserved")]
    #[test_case(JournalState::Prepared, false; "prepared")]
    #[test_case(JournalState::Dispatched, false; "dispatched")]
    #[test_case(JournalState::Unknown, false; "unknown")]
    #[test_case(JournalState::Confirmed, true; "pending_cleanup")]
    #[test_case(JournalState::Failed, false; "failed_replay_tombstone")]
    #[test_case(JournalState::Cancelled, false; "cancelled_replay_tombstone")]
    fn journal_rotation_retains_unresolved_and_terminal_replay_ids(
        state: JournalState,
        pending: bool,
    ) {
        smol::block_on(async {
            let mut fixture = Fixture::rotating();
            fixture.put(Side::Local, FILE, AFTER);
            let first = fixture.plan(TransferAction::Push, &[FILE]).await;
            let id = &first.review.files[0].operation_id;
            fixture
                .journal
                .reserve(&first, &first.review.files[0])
                .unwrap();
            fixture
                .journal
                .update(id, |entry| {
                    entry.state = state.clone();
                    entry.cleanup_pending = pending;
                    Ok(())
                })
                .unwrap();
            for index in 0..=TEST_ROTATION_RECORDS {
                fixture.put(Side::Local, SECOND, &index.to_le_bytes());
                let plan = fixture.plan(TransferAction::Push, &[SECOND]).await;
                let run = fixture
                    .engine
                    .execute(&plan, &mut fixture.journal, &CancelToken::none())
                    .await;
                assert!(run.stopped.is_none(), "{:?}", run.stopped);
            }
            let mut reopened =
                TransferJournal::new(fixture.state.path().join(JOURNAL_FILE)).unwrap();
            assert!(
                reopened
                    .audit()
                    .unwrap()
                    .values()
                    .any(|audit| audit.pages > 0)
            );
            let retained = reopened
                .entries()
                .unwrap()
                .into_iter()
                .find(|entry| entry.operation_id == *id);
            assert_eq!(retained.is_some(), state.blocks() || pending);
            if let Some(retained) = retained {
                assert_eq!(retained.state, state);
                assert_eq!(retained.cleanup_pending, pending);
            }
            let replay = reopened.reserve(&first, &first.review.files[0]);
            if pending {
                assert!(!replay.unwrap());
            } else if state.blocks() {
                assert!(matches!(replay, Err(TransferError::RecoveryRequired)));
            } else {
                assert!(matches!(replay, Err(TransferError::ReviewConsumed)));
            }
            assert_eq!(reopened.base().unwrap().len(), 1);
        });
    }

    #[test_case(false; "missing_archive")]
    #[test_case(true; "corrupt_archive")]
    fn removing_replay_archive_fails_closed(corrupt: bool) {
        smol::block_on(async {
            let mut fixture = Fixture::rotating();
            fixture.put(Side::Local, FILE, AFTER);
            let first = fixture.plan(TransferAction::Push, &[FILE]).await;
            for index in 0..=TEST_ROTATION_RECORDS {
                let plan = if index == 0 {
                    &first
                } else {
                    &fixture.plan(TransferAction::Push, &[FILE]).await
                };
                fixture
                    .journal
                    .reserve(plan, &plan.review.files[0])
                    .unwrap();
                fixture
                    .journal
                    .update(&plan.review.files[0].operation_id, |entry| {
                        entry.state = JournalState::Failed;
                        Ok(())
                    })
                    .unwrap();
            }
            let archive = fixture
                .state
                .path()
                .join(JOURNAL_FILE)
                .with_extension("archive");
            let namespace = fs::read_dir(archive)
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            let page = fs::read_dir(namespace)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|path| path.extension().is_none())
                .unwrap();
            if corrupt {
                fs::write(page, b"{}").unwrap();
            } else {
                fs::remove_file(page).unwrap();
            }
            let mut reopened =
                TransferJournal::new(fixture.state.path().join(JOURNAL_FILE)).unwrap();
            assert!(matches!(
                reopened.reserve(&first, &first.review.files[0]),
                Err(TransferError::Journal)
            ));
            assert_eq!(fixture.fake.0.lock().unwrap().counts.publishes, 0);
        });
    }

    #[test]
    fn completed_history_is_namespaced_and_cannot_starve_another_project() {
        smol::block_on(async {
            let mut first = Fixture::rotating();
            let mut second = Fixture::new();
            second.journal = TransferJournal::new(first.state.path().join(JOURNAL_FILE)).unwrap();
            second.journal.rotation_records = TEST_ROTATION_RECORDS;
            for fixture in [&mut first, &mut second] {
                for _ in 0..TEST_ROTATION_RECORDS * ROTATION_PASSES {
                    fixture.put(Side::Local, FILE, AFTER);
                    let plan = fixture.plan(TransferAction::Push, &[FILE]).await;
                    fixture
                        .journal
                        .reserve(&plan, &plan.review.files[0])
                        .unwrap();
                    fixture
                        .journal
                        .update(&plan.review.files[0].operation_id, |entry| {
                            entry.state = JournalState::Failed;
                            Ok(())
                        })
                        .unwrap();
                }
            }
            let audit = second.journal.audit().unwrap();
            assert_eq!(audit.len(), 2);
            let plan = second.plan(TransferAction::Push, &[FILE]).await;
            let run = second
                .engine
                .execute(&plan, &mut second.journal, &CancelToken::none())
                .await;
            assert!(run.stopped.is_none(), "{:?}", run.stopped);
            assert_eq!(second.journal.base().unwrap().len(), 1);
            assert!(
                second
                    .journal
                    .audit()
                    .unwrap()
                    .values()
                    .all(|audit| audit.pages as usize >= ROTATION_PASSES)
            );
        });
    }

    #[test]
    fn seed_refuses_existing_targets_and_filters_on_absent_destination() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.put(Side::Local, FILE, AFTER);
            fixture.put(Side::Remote, FILE, BEFORE);
            let comparison = fixture.engine.compare(&CancelToken::none()).await.unwrap();
            assert!(matches!(
                fixture
                    .engine
                    .plan(
                        &comparison,
                        TransferAction::Seed,
                        &[path(FILE)],
                        &BTreeSet::new(),
                        &CancelToken::none()
                    )
                    .await,
                Err(TransferError::Selection)
            ));
            fixture.fake.0.lock().unwrap().remote.clear();
            fixture.fake.0.lock().unwrap().ignored_absence = Some(Side::Remote);
            let comparison = fixture.engine.compare(&CancelToken::none()).await.unwrap();
            assert_eq!(comparison.rows()[0].kind, ComparisonKind::Excluded);
            assert!(matches!(
                fixture
                    .engine
                    .plan(
                        &comparison,
                        TransferAction::Push,
                        &[path(FILE)],
                        &BTreeSet::new(),
                        &CancelToken::none()
                    )
                    .await,
                Err(TransferError::Selection)
            ));
        });
    }

    #[test]
    fn lower_execution_quotas_cannot_be_bypassed_with_an_older_plan() {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            fixture.put(Side::Local, FILE, AFTER);
            let plan = fixture.plan(TransferAction::Push, &[FILE]).await;
            fixture.engine.limits.max_total_bytes = 1;
            let run = fixture
                .engine
                .execute(&plan, &mut fixture.journal, &CancelToken::none())
                .await;
            assert!(matches!(run.stopped, Some(TransferError::Quota)));
            assert!(!fixture.state.path().join(JOURNAL_FILE).exists());
            assert_eq!(fixture.fake.0.lock().unwrap().counts.stages, 0);
        });
    }

    #[test]
    fn rejected_publication_requires_new_review_not_status_replay() {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            fixture.put(Side::Local, FILE, AFTER);
            let plan = fixture.plan(TransferAction::Push, &[FILE]).await;
            fixture.fake.0.lock().unwrap().fail_publish = Some(1);
            let failed = fixture
                .engine
                .execute(&plan, &mut fixture.journal, &CancelToken::none())
                .await;
            assert!(matches!(
                failed.stopped,
                Some(TransferError::PublicationRejected)
            ));
            let replay = fixture
                .engine
                .execute(&plan, &mut fixture.journal, &CancelToken::none())
                .await;
            assert!(matches!(
                replay.stopped,
                Some(TransferError::ReviewConsumed)
            ));
            assert_eq!(fixture.fake.0.lock().unwrap().counts.publishes, 1);
            let fresh = fixture.plan(TransferAction::Push, &[FILE]).await;
            let run = fixture
                .engine
                .execute(&fresh, &mut fixture.journal, &CancelToken::none())
                .await;
            assert!(run.stopped.is_none(), "{:?}", run.stopped);
            assert_eq!(fixture.journal.base().unwrap().len(), 1);
        });
    }
}
