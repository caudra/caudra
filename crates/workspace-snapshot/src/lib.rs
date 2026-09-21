#![forbid(unsafe_code)]

#[cfg(test)]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt::Write as _,
    mem::size_of,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{Mutex as AsyncMutex, MutexGuard, OwnedMutexGuard},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workcell_host_contract::{
    ContractVersion, Cursor, Identifier, MAX_PAGE_SIZE, MAX_RESOURCE_INTENTS,
    MAX_SNAPSHOT_CAPTURE_ENTRIES, MAX_SNAPSHOT_CAPTURE_PATH_BYTES, MAX_SNAPSHOT_CLEANUP,
    MAX_SNAPSHOT_COUNT, MAX_SNAPSHOT_FILE_BYTES, MAX_SNAPSHOT_FILES, MAX_SNAPSHOT_JOURNALS,
    MAX_SNAPSHOT_STORAGE_BYTES, MAX_SNAPSHOT_TOTAL_BYTES, ResourceId, Revision,
    SnapshotAcknowledgeResponse, SnapshotCaptureResponse, SnapshotChange, SnapshotChangeKind,
    SnapshotCleanupPreview, SnapshotCleanupResponse, SnapshotFile, SnapshotInspectResponse,
    SnapshotRestorePreview, SnapshotRestoreState, SnapshotRestoreStatus, SnapshotState,
    SnapshotStatusResponse, SnapshotSummary, WorkspacePath, WorkspaceSnapshotCapability,
    WorkspaceSnapshotLimits, WorkspaceSnapshotMethods,
};
use workcell_mcp_files::{
    RootResourceKind, WorkspaceError, WorkspaceSnapshotAccess, root_relative_resource_id,
};

const MANIFEST_VERSION: &str = "workspace-snapshot.v1";
const JOURNAL_VERSION: &str = "workspace-restore-journal.v1";
const CHECKPOINT_VERSION: &str = "workspace-snapshot-checkpoint.v1";
const CAPTURE_ADMISSION_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_PRIVATE_METADATA_BYTES: u64 = 2 * 1_024 * 1_024;
const MAX_JOURNAL_STORAGE_BYTES: u64 = 64 * 1_024 * 1_024;
const MAX_EXCLUSIONS: usize = 32;
const MAX_PRIVATE_ENTRIES: usize =
    MAX_SNAPSHOT_COUNT * MAX_SNAPSHOT_FILES + MAX_SNAPSHOT_COUNT + MAX_SNAPSHOT_JOURNALS + 64;
const DEFAULT_EXCLUSIONS: &[&str] = &[
    ".git",
    ".ssh",
    ".workcell",
    ".env",
    ".npmrc",
    ".pypirc",
    ".netrc",
];

#[derive(Clone)]
pub struct SnapshotManager {
    inner: Arc<SnapshotInner>,
}

struct SnapshotInner {
    workspace: WorkspaceSnapshotAccess,
    root: PathBuf,
    exclusions: Vec<String>,
    capture: AsyncMutex<()>,
    publication: AsyncMutex<()>,
    state: Mutex<SnapshotRuntimeState>,
    #[cfg(test)]
    fail_after_file: AtomicUsize,
    #[cfg(test)]
    fail_before_journal: AtomicBool,
    #[cfg(test)]
    fail_cleanup_after_phase: AtomicUsize,
}

#[derive(Default)]
struct SnapshotRuntimeState {
    journals: HashMap<String, StoredJournal>,
    pending: HashMap<String, PendingReference>,
}

struct PendingReference {
    paths: BTreeSet<String>,
    snapshots: BTreeSet<String>,
}

#[derive(Default)]
struct CaptureTraversalBudget {
    entries: usize,
    path_bytes: usize,
}

impl CaptureTraversalBudget {
    fn retain(
        &mut self,
        path_bytes: usize,
        maximum_entries: usize,
        maximum_path_bytes: usize,
    ) -> Result<(), SnapshotError> {
        let retained_path_bytes = self.path_bytes.saturating_add(path_bytes);
        if self.entries >= maximum_entries || retained_path_bytes > maximum_path_bytes {
            return Err(SnapshotError::LimitExceeded);
        }
        self.entries = self.entries.saturating_add(1);
        self.path_bytes = retained_path_bytes;
        Ok(())
    }
}

impl PendingReference {
    fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(
                self.paths
                    .len()
                    .saturating_mul(size_of::<String>().saturating_mul(2)),
            )
            .saturating_add(
                self.paths
                    .iter()
                    .map(String::capacity)
                    .fold(0, usize::saturating_add),
            )
            .saturating_add(
                self.snapshots
                    .len()
                    .saturating_mul(size_of::<String>().saturating_mul(2)),
            )
            .saturating_add(
                self.snapshots
                    .iter()
                    .map(String::capacity)
                    .fold(0, usize::saturating_add),
            )
    }
}

pub struct PreparedSnapshotRestore {
    manager: SnapshotManager,
    lease_id: String,
    restore_id: String,
    target: StoredManifest,
    baseline_revision: String,
    pre_restore_snapshot_id: String,
    changes: Vec<StoredChange>,
    created_directories: Vec<String>,
    unrevert_of: Option<String>,
    pending_bytes: usize,
}

pub struct PreparedSnapshotCleanup {
    manager: SnapshotManager,
    lease_id: String,
    requested_snapshot_ids: Vec<String>,
    plan: CleanupPlan,
    resource_scope: String,
    preview: SnapshotCleanupPreview,
    pending_bytes: usize,
}

impl PreparedSnapshotRestore {
    /// Conservative retained bytes, excluding the snapshot manager shared with the host.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.lease_id.capacity())
            .saturating_add(self.restore_id.capacity())
            .saturating_add(self.target.retained_bytes())
            .saturating_add(self.baseline_revision.capacity())
            .saturating_add(self.pre_restore_snapshot_id.capacity())
            .saturating_add(retained_vec(&self.changes, StoredChange::retained_bytes))
            .saturating_add(retained_strings(&self.created_directories))
            .saturating_add(self.unrevert_of.as_ref().map_or(0, String::capacity))
            .saturating_add(self.pending_bytes)
    }
}

impl PreparedSnapshotCleanup {
    /// Conservative retained bytes, excluding the snapshot manager shared with the host.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.lease_id.capacity())
            .saturating_add(retained_strings(&self.requested_snapshot_ids))
            .saturating_add(self.plan.retained_bytes())
            .saturating_add(self.resource_scope.capacity())
            .saturating_add(snapshot_cleanup_preview_bytes(&self.preview))
            .saturating_add(self.pending_bytes)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct CleanupPlan {
    checkpoints: Vec<StoredCheckpoint>,
    journals: Vec<StoredJournal>,
    manifests: Vec<String>,
    blobs: Vec<String>,
    reclaimed_bytes: u64,
}

impl CleanupPlan {
    fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(retained_vec(
                &self.checkpoints,
                StoredCheckpoint::retained_bytes,
            ))
            .saturating_add(retained_vec(&self.journals, StoredJournal::retained_bytes))
            .saturating_add(retained_strings(&self.manifests))
            .saturating_add(retained_strings(&self.blobs))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestContent {
    version: String,
    files: Vec<StoredFile>,
    exclusions: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredManifest {
    snapshot_id: String,
    created_at_unix_ms: u64,
    content: ManifestContent,
}

impl StoredManifest {
    fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.snapshot_id.capacity())
            .saturating_add(self.content.retained_bytes())
    }
}

impl ManifestContent {
    fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.version.capacity())
            .saturating_add(retained_vec(&self.files, StoredFile::retained_bytes))
            .saturating_add(retained_strings(&self.exclusions))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredFile {
    path: String,
    resource_id: String,
    identity: String,
    revision: String,
    digest: String,
    mode: u32,
    size_bytes: u64,
}

impl StoredFile {
    fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.path.capacity())
            .saturating_add(self.resource_id.capacity())
            .saturating_add(self.identity.capacity())
            .saturating_add(self.revision.capacity())
            .saturating_add(self.digest.capacity())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredCheckpoint {
    version: String,
    checkpoint_id: String,
    snapshot_id: String,
}

impl StoredCheckpoint {
    fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.version.capacity())
            .saturating_add(self.checkpoint_id.capacity())
            .saturating_add(self.snapshot_id.capacity())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredJournal {
    version: String,
    restore_id: String,
    state: StoredRestoreState,
    target_snapshot_id: String,
    pre_restore_snapshot_id: String,
    changes: Vec<StoredChange>,
    #[serde(default)]
    created_directories: Vec<String>,
    applied_files: usize,
    #[serde(default)]
    applied_directories: usize,
    reconciliation_required: bool,
    acknowledgement_required: bool,
    unrevert_of: Option<String>,
}

impl StoredJournal {
    fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.version.capacity())
            .saturating_add(self.restore_id.capacity())
            .saturating_add(self.target_snapshot_id.capacity())
            .saturating_add(self.pre_restore_snapshot_id.capacity())
            .saturating_add(retained_vec(&self.changes, StoredChange::retained_bytes))
            .saturating_add(retained_strings(&self.created_directories))
            .saturating_add(self.unrevert_of.as_ref().map_or(0, String::capacity))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredRestoreState {
    Publishing,
    Completed,
    Partial,
    Indeterminate,
    Acknowledged,
    Reverted,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredChange {
    path: String,
    resource_id: String,
    kind: StoredChangeKind,
    current_revision: Option<String>,
    target_revision: Option<String>,
    target_digest: Option<String>,
    target_mode: Option<u32>,
}

impl StoredChange {
    fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.path.capacity())
            .saturating_add(self.resource_id.capacity())
            .saturating_add(self.current_revision.as_ref().map_or(0, String::capacity))
            .saturating_add(self.target_revision.as_ref().map_or(0, String::capacity))
            .saturating_add(self.target_digest.as_ref().map_or(0, String::capacity))
    }
}

fn retained_vec<T>(values: &Vec<T>, nested: impl Fn(&T) -> usize) -> usize {
    values
        .capacity()
        .saturating_mul(size_of::<T>())
        .saturating_add(values.iter().map(nested).fold(0, usize::saturating_add))
}

fn retained_strings(values: &Vec<String>) -> usize {
    values
        .capacity()
        .saturating_mul(size_of::<String>())
        .saturating_add(
            values
                .iter()
                .map(String::capacity)
                .fold(0, usize::saturating_add),
        )
}

fn pending_entry_retained_bytes(lease_id: &str, pending: &PendingReference) -> usize {
    size_of::<(String, PendingReference)>()
        .saturating_mul(2)
        .saturating_add(lease_id.len())
        .saturating_add(pending.retained_bytes())
}

fn snapshot_cleanup_preview_bytes(preview: &SnapshotCleanupPreview) -> usize {
    size_of::<SnapshotCleanupPreview>()
        .saturating_add(
            preview
                .snapshot_ids
                .capacity()
                .saturating_mul(size_of::<Identifier>()),
        )
        .saturating_add(
            preview
                .snapshot_ids
                .iter()
                .map(Identifier::retained_bytes)
                .fold(0, usize::saturating_add),
        )
        .saturating_add(
            preview
                .retained_snapshot_ids
                .capacity()
                .saturating_mul(size_of::<Identifier>()),
        )
        .saturating_add(
            preview
                .retained_snapshot_ids
                .iter()
                .map(Identifier::retained_bytes)
                .fold(0, usize::saturating_add),
        )
}

fn enforce_snapshot_preparation_bytes(bytes: usize, maximum: usize) -> Result<(), SnapshotError> {
    if bytes > maximum {
        return Err(SnapshotError::LimitExceeded);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredChangeKind {
    Create,
    Replace,
    Delete,
    Conflict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SnapshotError {
    #[error("snapshot configuration is invalid")]
    InvalidConfiguration,
    #[error("snapshot storage is unavailable or unhealthy")]
    UnhealthyStorage,
    #[error("snapshot request is invalid")]
    InvalidRequest,
    #[error("snapshot was not found")]
    NotFound,
    #[error("snapshot data failed integrity verification")]
    IntegrityFailure,
    #[error("workspace contains an unsupported file type or path")]
    UnsupportedFile,
    #[error("workspace changed while it was being captured")]
    WorkspaceChanged,
    #[error("snapshot limit was exceeded")]
    LimitExceeded,
    #[error("snapshot storage quota was exceeded")]
    QuotaExceeded,
    #[error("snapshot capture admission timed out")]
    Busy,
    #[error("prepared snapshot restore is stale or conflicts with later edits")]
    Conflict,
    #[error("an overlapping restore requires acknowledgement")]
    AcknowledgementRequired,
    #[error("snapshot operation was cancelled")]
    Cancelled,
    #[error("snapshot operation failed")]
    OperationFailed,
}

impl SnapshotError {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::InvalidConfiguration => "invalid_configuration",
            Self::UnhealthyStorage => "unhealthy_storage",
            Self::InvalidRequest => "invalid_request",
            Self::NotFound => "not_found",
            Self::IntegrityFailure => "integrity_failure",
            Self::UnsupportedFile => "unsupported_file",
            Self::WorkspaceChanged => "workspace_changed",
            Self::LimitExceeded => "limit_exceeded",
            Self::QuotaExceeded => "quota_exceeded",
            Self::Busy => "busy",
            Self::Conflict => "conflict",
            Self::AcknowledgementRequired => "acknowledgement_required",
            Self::Cancelled => "cancelled",
            Self::OperationFailed => "operation_failed",
        }
    }
}

impl SnapshotManager {
    pub async fn open(
        workspace: WorkspaceSnapshotAccess,
        private_root: impl AsRef<Path>,
        excluded_paths: &[PathBuf],
    ) -> Result<Self, SnapshotError> {
        Self::open_validated(workspace, private_root.as_ref(), excluded_paths, None).await
    }

    pub async fn open_bound(
        workspace: WorkspaceSnapshotAccess,
        private_root: impl AsRef<Path>,
        excluded_paths: &[PathBuf],
        workspace_binding: &Identifier,
    ) -> Result<Self, SnapshotError> {
        Self::open_validated(
            workspace,
            private_root.as_ref(),
            excluded_paths,
            Some(workspace_binding),
        )
        .await
    }

    async fn open_validated(
        workspace: WorkspaceSnapshotAccess,
        private_root: &Path,
        excluded_paths: &[PathBuf],
        workspace_binding: Option<&Identifier>,
    ) -> Result<Self, SnapshotError> {
        if !workspace.allow_write() {
            return Err(SnapshotError::InvalidConfiguration);
        }
        let mut private_root = validate_private_root(private_root, workspace.root())?;
        if let Some(workspace_binding) = workspace_binding {
            validate_store_binding(workspace_binding.as_str())?;
            private_root.push(workspace_binding.as_str());
            create_private_directory(&private_root).await?;
        }
        for directory in ["blobs", "manifests", "checkpoints", "journals"] {
            create_private_directory(&private_root.join(directory)).await?;
        }
        let exclusions = configured_exclusions(workspace.root(), excluded_paths)?;
        let manager = Self {
            inner: Arc::new(SnapshotInner {
                workspace,
                root: private_root,
                exclusions,
                capture: AsyncMutex::new(()),
                publication: AsyncMutex::new(()),
                state: Mutex::new(SnapshotRuntimeState::default()),
                #[cfg(test)]
                fail_after_file: AtomicUsize::new(usize::MAX),
                #[cfg(test)]
                fail_before_journal: AtomicBool::new(false),
                #[cfg(test)]
                fail_cleanup_after_phase: AtomicUsize::new(usize::MAX),
            }),
        };
        manager.cleanup_private_temps().await?;
        manager.validate_existing_store().await?;
        manager.gc_blobs(None, &CancellationToken::new()).await?;
        manager.load_and_recover_journals().await?;
        manager.reclaim_startup_journals_to_quota().await?;
        manager.recover_loaded_journals().await?;
        let recovery_token = CancellationToken::new();
        manager.gc_orphan_manifests(&recovery_token).await?;
        manager.gc_blobs(None, &recovery_token).await?;
        manager.validate_manifests().await?;
        manager.validate_blobs().await?;
        Ok(manager)
    }

    #[must_use]
    pub fn capability() -> WorkspaceSnapshotCapability {
        WorkspaceSnapshotCapability {
            version: ContractVersion::V1,
            methods: WorkspaceSnapshotMethods {
                capture: true,
                inspect: true,
                status: true,
                prepare_restore: true,
                prepare_unrevert: true,
                acknowledge: true,
                prepare_cleanup: true,
            },
            limits: WorkspaceSnapshotLimits {
                max_files: u32::try_from(MAX_SNAPSHOT_FILES).unwrap_or(u32::MAX),
                max_file_bytes: MAX_SNAPSHOT_FILE_BYTES,
                max_total_bytes: MAX_SNAPSHOT_TOTAL_BYTES,
                max_capture_entries: u32::try_from(MAX_SNAPSHOT_CAPTURE_ENTRIES)
                    .unwrap_or(u32::MAX),
                max_capture_path_bytes: MAX_SNAPSHOT_CAPTURE_PATH_BYTES,
                max_snapshots: u32::try_from(MAX_SNAPSHOT_COUNT).unwrap_or(u32::MAX),
                max_storage_bytes: MAX_SNAPSHOT_STORAGE_BYTES,
                max_concurrent_captures: 1,
                max_cleanup_snapshots: u32::try_from(MAX_SNAPSHOT_CLEANUP).unwrap_or(u32::MAX),
            },
            atomic_across_files: false,
            durable_per_file_journal: true,
        }
    }

    pub async fn capture(
        &self,
        checkpoint_id: &Identifier,
        token: &CancellationToken,
    ) -> Result<SnapshotCaptureResponse, SnapshotError> {
        let (_capture, _publication, _workspace) = self.capture_guards(token).await?;
        check_cancelled(token)?;
        if let Some(manifest) = self.load_checkpoint(checkpoint_id.as_str()).await? {
            return Ok(SnapshotCaptureResponse {
                version: ContractVersion::V1,
                snapshot: manifest.summary(Some(checkpoint_id.as_str()))?,
                reused_checkpoint: true,
            });
        }
        if directory_count(&self.inner.root.join("checkpoints"), MAX_SNAPSHOT_COUNT).await?
            >= MAX_SNAPSHOT_COUNT
        {
            return Err(SnapshotError::QuotaExceeded);
        }
        if self.manifest_count().await? >= MAX_SNAPSHOT_COUNT {
            return Err(SnapshotError::QuotaExceeded);
        }
        let result = async {
            let manifest = self.capture_manifest(token).await?;
            self.persist_manifest(&manifest).await?;
            let manifest = self.load_manifest(&manifest.snapshot_id, false).await?;
            self.persist_checkpoint(checkpoint_id.as_str(), &manifest.snapshot_id)
                .await?;
            Ok::<_, SnapshotError>(manifest)
        }
        .await;
        let manifest = match result {
            Ok(manifest) => manifest,
            Err(error) => {
                self.recover_orphans().await?;
                return Err(error);
            }
        };
        Ok(SnapshotCaptureResponse {
            version: ContractVersion::V1,
            snapshot: manifest.summary(Some(checkpoint_id.as_str()))?,
            reused_checkpoint: false,
        })
    }

    pub async fn inspect(
        &self,
        snapshot_id: &Identifier,
        page_size: u32,
        cursor: Option<&Cursor>,
    ) -> Result<SnapshotInspectResponse, SnapshotError> {
        if page_size == 0 || page_size > MAX_PAGE_SIZE {
            return Err(SnapshotError::InvalidRequest);
        }
        let manifest = self.load_manifest(snapshot_id.as_str(), true).await?;
        let offset = parse_cursor(cursor, snapshot_id.as_str(), manifest.content.files.len())?;
        let end = offset
            .saturating_add(page_size as usize)
            .min(manifest.content.files.len());
        let next_cursor = (end < manifest.content.files.len())
            .then(|| Cursor::new(format!("{}:{end}", snapshot_id.as_str())))
            .transpose()
            .map_err(|_| SnapshotError::OperationFailed)?;
        Ok(SnapshotInspectResponse {
            version: ContractVersion::V1,
            snapshot: manifest.summary(None)?,
            files: manifest.content.files[offset..end]
                .iter()
                .map(StoredFile::contract)
                .collect::<Result<_, _>>()?,
            exclusions: manifest
                .content
                .exclusions
                .iter()
                .map(|path| WorkspacePath::new(path.clone()))
                .collect::<Result<_, _>>()
                .map_err(|_| SnapshotError::IntegrityFailure)?,
            next_cursor,
        })
    }

    pub fn status(&self, restore_id: &Identifier) -> Result<SnapshotStatusResponse, SnapshotError> {
        let state = lock(&self.inner.state);
        let journal = state
            .journals
            .get(restore_id.as_str())
            .ok_or(SnapshotError::NotFound)?;
        Ok(SnapshotStatusResponse {
            version: ContractVersion::V1,
            restore: journal.contract_status()?,
        })
    }

    pub async fn prepare_restore(
        &self,
        snapshot_id: &Identifier,
        token: &CancellationToken,
    ) -> Result<(PreparedSnapshotRestore, SnapshotRestorePreview), SnapshotError> {
        self.prepare_restore_bounded(snapshot_id, usize::MAX, token)
            .await
    }

    pub async fn prepare_restore_bounded(
        &self,
        snapshot_id: &Identifier,
        maximum_retained_bytes: usize,
        token: &CancellationToken,
    ) -> Result<(PreparedSnapshotRestore, SnapshotRestorePreview), SnapshotError> {
        let result = self
            .prepare_restore_inner(snapshot_id.as_str(), None, token)
            .await?;
        enforce_snapshot_preparation_bytes(result.0.retained_bytes(), maximum_retained_bytes)?;
        Ok(result)
    }

    pub async fn prepare_unrevert(
        &self,
        restore_id: &Identifier,
        token: &CancellationToken,
    ) -> Result<(PreparedSnapshotRestore, SnapshotRestorePreview), SnapshotError> {
        self.prepare_unrevert_bounded(restore_id, usize::MAX, token)
            .await
    }

    pub async fn prepare_unrevert_bounded(
        &self,
        restore_id: &Identifier,
        maximum_retained_bytes: usize,
        token: &CancellationToken,
    ) -> Result<(PreparedSnapshotRestore, SnapshotRestorePreview), SnapshotError> {
        let target = {
            let state = lock(&self.inner.state);
            let journal = state
                .journals
                .get(restore_id.as_str())
                .ok_or(SnapshotError::NotFound)?;
            if !journal.acknowledgement_required
                || !matches!(journal.state, StoredRestoreState::Completed)
            {
                return Err(SnapshotError::Conflict);
            }
            journal.pre_restore_snapshot_id.clone()
        };
        let result = self
            .prepare_restore_inner(&target, Some(restore_id.as_str()), token)
            .await?;
        enforce_snapshot_preparation_bytes(result.0.retained_bytes(), maximum_retained_bytes)?;
        Ok(result)
    }

    pub async fn acknowledge(
        &self,
        restore_id: &Identifier,
    ) -> Result<SnapshotAcknowledgeResponse, SnapshotError> {
        let _publication = self.inner.publication.lock().await;
        let mut journal = {
            let state = lock(&self.inner.state);
            state
                .journals
                .get(restore_id.as_str())
                .cloned()
                .ok_or(SnapshotError::NotFound)?
        };
        if journal.reconciliation_required
            || !matches!(journal.state, StoredRestoreState::Completed)
        {
            return Err(SnapshotError::Conflict);
        }
        journal.state = StoredRestoreState::Acknowledged;
        journal.acknowledgement_required = false;
        self.persist_journal(&journal).await?;
        lock(&self.inner.state)
            .journals
            .insert(journal.restore_id.clone(), journal.clone());
        Ok(SnapshotAcknowledgeResponse {
            version: ContractVersion::V1,
            restore: journal.contract_status()?,
        })
    }

    pub async fn prepare_cleanup(
        &self,
        snapshot_ids: &[Identifier],
    ) -> Result<(PreparedSnapshotCleanup, SnapshotCleanupPreview), SnapshotError> {
        self.prepare_cleanup_bounded(snapshot_ids, usize::MAX).await
    }

    pub async fn prepare_cleanup_bounded(
        &self,
        snapshot_ids: &[Identifier],
        maximum_retained_bytes: usize,
    ) -> Result<(PreparedSnapshotCleanup, SnapshotCleanupPreview), SnapshotError> {
        let _publication = self.inner.publication.lock().await;
        if snapshot_ids.len() > MAX_SNAPSHOT_CLEANUP {
            return Err(SnapshotError::InvalidRequest);
        }
        let requested = snapshot_ids
            .iter()
            .map(|id| id.as_str().to_owned())
            .collect::<BTreeSet<_>>();
        if requested.len() != snapshot_ids.len() {
            return Err(SnapshotError::InvalidRequest);
        }
        for id in &requested {
            self.load_manifest(id, false).await?;
        }
        let protected = self.protected_snapshots(None);
        let retained = requested
            .intersection(&protected)
            .cloned()
            .collect::<Vec<_>>();
        let eligible = requested
            .difference(&protected)
            .cloned()
            .collect::<Vec<_>>();
        let plan = self
            .cleanup_plan_bounded(&requested, None, maximum_retained_bytes)
            .await?;
        let preview = SnapshotCleanupPreview {
            snapshot_ids: identifiers(&eligible)?,
            retained_snapshot_ids: identifiers(&retained)?,
            reclaimable_bytes: plan.reclaimed_bytes,
        };
        let lease_id = format!("lease_{}", Uuid::new_v4());
        let pending = PendingReference {
            paths: BTreeSet::new(),
            snapshots: requested.clone(),
        };
        let pending_bytes = pending_entry_retained_bytes(&lease_id, &pending);
        lock(&self.inner.state)
            .pending
            .insert(lease_id.clone(), pending);
        let prepared = PreparedSnapshotCleanup {
            manager: self.clone(),
            lease_id,
            requested_snapshot_ids: requested.into_iter().collect(),
            resource_scope: format!("snapshot-store:cleanup:{}", digest_serializable(&plan)?),
            plan,
            preview: preview.clone(),
            pending_bytes,
        };
        enforce_snapshot_preparation_bytes(prepared.retained_bytes(), maximum_retained_bytes)?;
        Ok((prepared, preview))
    }

    pub async fn execute_restore(
        &self,
        prepared: &PreparedSnapshotRestore,
        token: &CancellationToken,
    ) -> Result<SnapshotRestoreStatus, SnapshotError> {
        if !Arc::ptr_eq(&self.inner, &prepared.manager.inner) {
            return Err(SnapshotError::InvalidRequest);
        }
        let _publication = self.inner.publication.lock().await;
        let _workspace = self
            .inner
            .workspace
            .mutation_guard()
            .await
            .map_err(map_workspace)?;
        let current = self.consistent_manifest(false, token).await?;
        if manifest_revision(&current.content)? != prepared.baseline_revision
            || current.snapshot_id != prepared.pre_restore_snapshot_id
        {
            return Err(SnapshotError::Conflict);
        }
        if prepared
            .changes
            .iter()
            .any(|change| change.kind == StoredChangeKind::Conflict)
        {
            return Err(SnapshotError::Conflict);
        }
        self.ensure_no_overlap(
            &prepared.lease_id,
            &prepared.changes,
            prepared.unrevert_of.as_deref(),
        )?;
        self.make_journal_room(&prepared.restore_id, prepared.maximum_journal_bytes()?)
            .await?;
        let pre_restore_result = async {
            let manifest = self.capture_manifest(token).await?;
            self.persist_manifest(&manifest).await?;
            Ok::<_, SnapshotError>(manifest)
        }
        .await;
        let pre_restore = match pre_restore_result {
            Ok(manifest) => manifest,
            Err(error) => {
                self.recover_orphans().await?;
                return Err(error);
            }
        };
        if pre_restore.snapshot_id != prepared.pre_restore_snapshot_id {
            self.recover_orphans().await?;
            return Err(SnapshotError::Conflict);
        }
        let journal_path = self.journal_path(&prepared.restore_id)?;
        let maximum_journal_bytes = prepared.maximum_journal_bytes()?;
        let capacity = async {
            self.ensure_journal_capacity(&journal_path, maximum_journal_bytes)
                .await?;
            self.ensure_storage_capacity(&journal_path, maximum_journal_bytes)
                .await
        }
        .await;
        if let Err(error) = capacity {
            self.recover_orphans().await?;
            return Err(error);
        }
        let mut journal = StoredJournal {
            version: JOURNAL_VERSION.to_owned(),
            restore_id: prepared.restore_id.clone(),
            state: if prepared.changes.is_empty() && prepared.created_directories.is_empty() {
                StoredRestoreState::Completed
            } else {
                StoredRestoreState::Publishing
            },
            target_snapshot_id: prepared.target.snapshot_id.clone(),
            pre_restore_snapshot_id: prepared.pre_restore_snapshot_id.clone(),
            changes: prepared.changes.clone(),
            created_directories: prepared.created_directories.clone(),
            applied_files: 0,
            applied_directories: 0,
            reconciliation_required: false,
            acknowledgement_required: !prepared.changes.is_empty()
                || !prepared.created_directories.is_empty(),
            unrevert_of: prepared.unrevert_of.clone(),
        };
        #[cfg(test)]
        if self.inner.fail_before_journal.load(Ordering::SeqCst) {
            self.recover_orphans().await?;
            return Err(SnapshotError::OperationFailed);
        }
        if let Err(error) = self.persist_journal(&journal).await {
            self.recover_orphans().await?;
            return Err(error);
        }
        lock(&self.inner.state).pending.remove(&prepared.lease_id);
        lock(&self.inner.state)
            .journals
            .insert(journal.restore_id.clone(), journal.clone());

        for index in 0..journal.created_directories.len() {
            if token.is_cancelled() {
                self.mark_interrupted(&mut journal).await?;
                return Err(SnapshotError::Cancelled);
            }
            if let Err(error) = self
                .publish_directory(&journal.created_directories[index])
                .await
            {
                self.mark_interrupted(&mut journal).await?;
                return Err(error);
            }
            journal.applied_directories = index + 1;
            if let Err(error) = self.persist_journal(&journal).await {
                let _ = self.mark_interrupted(&mut journal).await;
                return Err(error);
            }
            lock(&self.inner.state)
                .journals
                .insert(journal.restore_id.clone(), journal.clone());
        }

        for index in 0..journal.changes.len() {
            if token.is_cancelled() {
                self.mark_interrupted(&mut journal).await?;
                return Err(SnapshotError::Cancelled);
            }
            let change = &journal.changes[index];
            if let Err(error) = self.publish_change(change, token).await {
                self.mark_interrupted(&mut journal).await?;
                return Err(error);
            }
            #[cfg(test)]
            if self.inner.fail_after_file.load(Ordering::SeqCst) == index {
                let _ = self.mark_interrupted(&mut journal).await;
                return Err(SnapshotError::OperationFailed);
            }
            journal.applied_files = index + 1;
            if let Err(error) = self.persist_journal(&journal).await {
                journal.state = StoredRestoreState::Indeterminate;
                journal.reconciliation_required = true;
                lock(&self.inner.state)
                    .journals
                    .insert(journal.restore_id.clone(), journal.clone());
                return Err(error);
            }
            lock(&self.inner.state)
                .journals
                .insert(journal.restore_id.clone(), journal.clone());
        }
        if journal.state == StoredRestoreState::Publishing {
            journal.state = StoredRestoreState::Completed;
            if let Err(error) = self.persist_journal(&journal).await {
                let _ = self.mark_interrupted(&mut journal).await;
                return Err(error);
            }
        }
        {
            let mut state = lock(&self.inner.state);
            state
                .journals
                .insert(journal.restore_id.clone(), journal.clone());
        }
        if let Some(original_id) = &journal.unrevert_of
            && let Err(error) = self.mark_reverted(original_id).await
        {
            let _ = self.mark_interrupted(&mut journal).await;
            return Err(error);
        }
        journal.contract_status()
    }

    pub async fn execute_cleanup(
        &self,
        prepared: &PreparedSnapshotCleanup,
        token: &CancellationToken,
    ) -> Result<SnapshotCleanupResponse, SnapshotError> {
        if !Arc::ptr_eq(&self.inner, &prepared.manager.inner) {
            return Err(SnapshotError::InvalidRequest);
        }
        let _publication = self.inner.publication.lock().await;
        let requested = prepared
            .requested_snapshot_ids
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let current_plan = self
            .cleanup_plan(&requested, Some(&prepared.lease_id))
            .await?;
        if current_plan != prepared.plan {
            return Err(SnapshotError::Conflict);
        }
        lock(&self.inner.state).pending.remove(&prepared.lease_id);
        for checkpoint in &prepared.plan.checkpoints {
            check_cancelled(token)?;
            fs::remove_file(self.checkpoint_path(&checkpoint.checkpoint_id))
                .await
                .map_err(|_| SnapshotError::OperationFailed)?;
        }
        for journal in &prepared.plan.journals {
            check_cancelled(token)?;
            fs::remove_file(self.journal_path(&journal.restore_id)?)
                .await
                .map_err(|_| SnapshotError::OperationFailed)?;
            lock(&self.inner.state).journals.remove(&journal.restore_id);
        }
        sync_directory(&self.inner.root.join("checkpoints")).await?;
        sync_directory(&self.inner.root.join("journals")).await?;
        #[cfg(test)]
        if self.inner.fail_cleanup_after_phase.load(Ordering::SeqCst) == 0 {
            return Err(SnapshotError::OperationFailed);
        }
        for snapshot_id in &prepared.plan.manifests {
            check_cancelled(token)?;
            fs::remove_file(self.manifest_path(snapshot_id)?)
                .await
                .map_err(|_| SnapshotError::OperationFailed)?;
        }
        sync_directory(&self.inner.root.join("manifests")).await?;
        #[cfg(test)]
        if self.inner.fail_cleanup_after_phase.load(Ordering::SeqCst) == 1 {
            return Err(SnapshotError::OperationFailed);
        }
        for digest in &prepared.plan.blobs {
            check_cancelled(token)?;
            fs::remove_file(self.blob_path(digest)?)
                .await
                .map_err(|_| SnapshotError::OperationFailed)?;
        }
        sync_directory(&self.inner.root.join("blobs")).await?;
        #[cfg(test)]
        if self.inner.fail_cleanup_after_phase.load(Ordering::SeqCst) == 2 {
            return Err(SnapshotError::OperationFailed);
        }
        Ok(SnapshotCleanupResponse {
            version: ContractVersion::V1,
            deleted_snapshot_ids: prepared.preview.snapshot_ids.clone(),
            deleted_blobs: u32::try_from(prepared.plan.blobs.len()).unwrap_or(u32::MAX),
            reclaimed_bytes: prepared.plan.reclaimed_bytes,
        })
    }

    // One admission deadline covers all locks, without cancelling publication once it starts.
    async fn capture_guards(
        &self,
        token: &CancellationToken,
    ) -> Result<(MutexGuard<'_, ()>, MutexGuard<'_, ()>, OwnedMutexGuard<()>), SnapshotError> {
        let acquire = async {
            let capture = self.inner.capture.lock().await;
            let publication = self.inner.publication.lock().await;
            let workspace = self.inner.workspace.capture_guard().await;
            (capture, publication, workspace)
        };
        tokio::select! {
            biased;
            () = token.cancelled() => Err(SnapshotError::Cancelled),
            result = tokio::time::timeout(CAPTURE_ADMISSION_TIMEOUT, acquire) => {
                result.map_err(|_| SnapshotError::Busy)
            }
        }
    }

    async fn prepare_restore_inner(
        &self,
        snapshot_id: &str,
        unrevert_of: Option<&str>,
        token: &CancellationToken,
    ) -> Result<(PreparedSnapshotRestore, SnapshotRestorePreview), SnapshotError> {
        let (_capture, _publication, _workspace) = self.capture_guards(token).await?;
        let target = self.load_manifest(snapshot_id, true).await?;
        let current = self.consistent_manifest(false, token).await?;
        let changes = compare_manifests(&current, &target)?;
        let created_directories = self.missing_ancestors(&changes).await?;
        if changes
            .len()
            .saturating_add(created_directories.len())
            .saturating_add(3)
            > MAX_RESOURCE_INTENTS
        {
            return Err(SnapshotError::LimitExceeded);
        }
        self.ensure_no_overlap("", &changes, unrevert_of)?;
        let lease_id = format!("lease_{}", Uuid::new_v4());
        let restore_id = format!("restore_{}", Uuid::new_v4());
        let paths = changes.iter().map(|change| change.path.clone()).collect();
        let snapshots = [target.snapshot_id.clone(), current.snapshot_id.clone()]
            .into_iter()
            .collect();
        let pending = PendingReference { paths, snapshots };
        let pending_bytes = pending_entry_retained_bytes(&lease_id, &pending);
        lock(&self.inner.state)
            .pending
            .insert(lease_id.clone(), pending);
        let baseline_revision = manifest_revision(&current.content)?;
        let preview = SnapshotRestorePreview {
            restore_id: identifier(&restore_id)?,
            target_snapshot_id: identifier(&target.snapshot_id)?,
            current_revision: revision(&baseline_revision)?,
            target_revision: revision(&manifest_revision(&target.content)?)?,
            changes: changes
                .iter()
                .map(StoredChange::contract)
                .collect::<Result<_, _>>()?,
            created_directories: created_directories
                .iter()
                .cloned()
                .map(WorkspacePath::new)
                .collect::<Result<_, _>>()
                .map_err(|_| SnapshotError::OperationFailed)?,
        };
        Ok((
            PreparedSnapshotRestore {
                manager: self.clone(),
                lease_id,
                restore_id,
                target,
                baseline_revision,
                pre_restore_snapshot_id: current.snapshot_id,
                changes,
                created_directories,
                unrevert_of: unrevert_of.map(str::to_owned),
                pending_bytes,
            },
            preview,
        ))
    }

    async fn capture_manifest(
        &self,
        token: &CancellationToken,
    ) -> Result<StoredManifest, SnapshotError> {
        self.consistent_manifest(true, token).await
    }

    async fn consistent_manifest(
        &self,
        store_blobs: bool,
        token: &CancellationToken,
    ) -> Result<StoredManifest, SnapshotError> {
        let first = self.scan_workspace(store_blobs, token).await?;
        let second = self.scan_workspace(false, token).await?;
        if first.content != second.content {
            return Err(SnapshotError::WorkspaceChanged);
        }
        Ok(first)
    }

    async fn scan_workspace(
        &self,
        store_blobs: bool,
        token: &CancellationToken,
    ) -> Result<StoredManifest, SnapshotError> {
        self.scan_workspace_bounded(
            store_blobs,
            MAX_SNAPSHOT_CAPTURE_ENTRIES,
            usize::try_from(MAX_SNAPSHOT_CAPTURE_PATH_BYTES).unwrap_or(usize::MAX),
            token,
        )
        .await
    }

    async fn scan_workspace_bounded(
        &self,
        store_blobs: bool,
        maximum_entries: usize,
        maximum_path_bytes: usize,
        token: &CancellationToken,
    ) -> Result<StoredManifest, SnapshotError> {
        let mut files = Vec::new();
        let mut directories = vec![self.inner.workspace.root().to_path_buf()];
        let mut total_bytes = 0_u64;
        let mut traversal = CaptureTraversalBudget::default();
        while let Some(directory) = directories.pop() {
            check_cancelled(token)?;
            let mut reader = fs::read_dir(&directory)
                .await
                .map_err(|_| SnapshotError::OperationFailed)?;
            let mut entries = Vec::new();
            while let Some(entry) = reader
                .next_entry()
                .await
                .map_err(|_| SnapshotError::OperationFailed)?
            {
                let file_name = entry.file_name();
                let path_bytes = directory
                    .as_os_str()
                    .as_encoded_bytes()
                    .len()
                    .saturating_add(1)
                    .saturating_add(file_name.as_encoded_bytes().len());
                traversal.retain(path_bytes, maximum_entries, maximum_path_bytes)?;
                entries
                    .try_reserve_exact(1)
                    .map_err(|_| SnapshotError::LimitExceeded)?;
                entries.push(directory.join(file_name));
            }
            entries.sort();
            for path in entries.into_iter().rev() {
                check_cancelled(token)?;
                let relative = relative_path(self.inner.workspace.root(), &path)?;
                if self.excluded(&relative) || !self.inner.workspace.allows_canonical_entry(&path) {
                    continue;
                }
                let metadata = fs::symlink_metadata(&path)
                    .await
                    .map_err(|_| SnapshotError::WorkspaceChanged)?;
                let file_type = metadata.file_type();
                if file_type.is_symlink() {
                    return Err(SnapshotError::UnsupportedFile);
                }
                if file_type.is_dir() {
                    directories.push(path);
                    continue;
                }
                if !file_type.is_file() {
                    return Err(SnapshotError::UnsupportedFile);
                }
                if files.len() >= MAX_SNAPSHOT_FILES || metadata.len() > MAX_SNAPSHOT_FILE_BYTES {
                    return Err(SnapshotError::LimitExceeded);
                }
                total_bytes = total_bytes.saturating_add(metadata.len());
                if total_bytes > MAX_SNAPSHOT_TOTAL_BYTES {
                    return Err(SnapshotError::LimitExceeded);
                }
                let (file, bytes) = read_stable_file(&path, metadata, store_blobs, token).await?;
                if store_blobs {
                    self.persist_blob(&file.digest, &bytes).await?;
                }
                files.push(file.with_path(relative)?);
            }
        }
        files.sort_by(|left, right| left.path.cmp(&right.path));
        let content = ManifestContent {
            version: MANIFEST_VERSION.to_owned(),
            files,
            exclusions: self.inner.exclusions.clone(),
        };
        let snapshot_id = snapshot_id(&content)?;
        Ok(StoredManifest {
            snapshot_id,
            created_at_unix_ms: unix_ms(),
            content,
        })
    }

    fn excluded(&self, relative: &str) -> bool {
        self.inner.exclusions.iter().any(|excluded| {
            relative == excluded
                || relative
                    .strip_prefix(excluded)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        })
    }

    async fn missing_ancestors(
        &self,
        changes: &[StoredChange],
    ) -> Result<Vec<String>, SnapshotError> {
        let candidates = changes
            .iter()
            .filter(|change| change.target_revision.is_some())
            .flat_map(|change| ancestors(&change.path).map(str::to_owned))
            .collect::<BTreeSet<_>>();
        let mut missing = Vec::new();
        for relative in candidates {
            let path = WorkspacePath::new(relative.clone())
                .map_err(|_| SnapshotError::IntegrityFailure)?;
            let resolved = self
                .inner
                .workspace
                .resolve(&path)
                .await
                .map_err(map_workspace)?;
            match fs::symlink_metadata(&resolved).await {
                Ok(metadata)
                    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {}
                Ok(_) => return Err(SnapshotError::Conflict),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    missing.push(relative);
                }
                Err(_) => return Err(SnapshotError::OperationFailed),
            }
        }
        missing.sort_by(|left, right| {
            left.matches('/')
                .count()
                .cmp(&right.matches('/').count())
                .then_with(|| left.cmp(right))
        });
        Ok(missing)
    }

    async fn publish_directory(&self, relative: &str) -> Result<(), SnapshotError> {
        let path =
            WorkspacePath::new(relative.to_owned()).map_err(|_| SnapshotError::IntegrityFailure)?;
        let resolved = self
            .inner
            .workspace
            .resolve(&path)
            .await
            .map_err(map_workspace)?;
        match fs::symlink_metadata(&resolved).await {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => return Err(SnapshotError::Conflict),
        }
        let parent = resolved.parent().ok_or(SnapshotError::Conflict)?;
        let parent_metadata = fs::symlink_metadata(parent)
            .await
            .map_err(|_| SnapshotError::Conflict)?;
        if !parent_metadata.file_type().is_dir() || parent_metadata.file_type().is_symlink() {
            return Err(SnapshotError::Conflict);
        }
        fs::create_dir(&resolved)
            .await
            .map_err(|_| SnapshotError::Conflict)?;
        sync_directory(parent).await
    }

    async fn publish_change(
        &self,
        change: &StoredChange,
        token: &CancellationToken,
    ) -> Result<(), SnapshotError> {
        check_cancelled(token)?;
        let path =
            WorkspacePath::new(change.path.clone()).map_err(|_| SnapshotError::IntegrityFailure)?;
        let resolved = self
            .inner
            .workspace
            .resolve(&path)
            .await
            .map_err(map_workspace)?;
        let current = current_revision(&resolved, token).await?;
        if current.as_deref() != change.current_revision.as_deref() {
            return Err(SnapshotError::Conflict);
        }
        match change.kind {
            StoredChangeKind::Create | StoredChangeKind::Replace => {
                let digest = change
                    .target_digest
                    .as_deref()
                    .ok_or(SnapshotError::IntegrityFailure)?;
                let bytes = self.load_blob(digest).await?;
                atomic_workspace_write(
                    &self.inner.workspace,
                    &path,
                    &resolved,
                    &bytes,
                    change.target_mode.ok_or(SnapshotError::IntegrityFailure)?,
                    change.current_revision.as_deref(),
                    token,
                )
                .await
            }
            StoredChangeKind::Delete => {
                self.inner
                    .workspace
                    .revalidate(&resolved)
                    .await
                    .map_err(map_workspace)?;
                if current_revision(&resolved, token).await?.as_deref()
                    != change.current_revision.as_deref()
                {
                    return Err(SnapshotError::Conflict);
                }
                fs::remove_file(&resolved)
                    .await
                    .map_err(|_| SnapshotError::OperationFailed)?;
                sync_directory(resolved.parent().ok_or(SnapshotError::OperationFailed)?).await
            }
            StoredChangeKind::Conflict => Err(SnapshotError::Conflict),
        }
    }

    fn ensure_no_overlap(
        &self,
        own_lease: &str,
        changes: &[StoredChange],
        allowed_journal: Option<&str>,
    ) -> Result<(), SnapshotError> {
        let requested = changes
            .iter()
            .map(|change| change.path.as_str())
            .collect::<BTreeSet<_>>();
        let state = lock(&self.inner.state);
        let journal_overlap = state.journals.values().any(|journal| {
            Some(journal.restore_id.as_str()) != allowed_journal
                && journal.acknowledgement_required
                && journal
                    .changes
                    .iter()
                    .any(|change| requested.contains(change.path.as_str()))
        });
        let pending_overlap = state.pending.iter().any(|(lease, pending)| {
            lease != own_lease
                && pending
                    .paths
                    .iter()
                    .any(|path| requested.contains(path.as_str()))
        });
        if journal_overlap || pending_overlap {
            Err(SnapshotError::AcknowledgementRequired)
        } else {
            Ok(())
        }
    }

    async fn mark_interrupted(&self, journal: &mut StoredJournal) -> Result<(), SnapshotError> {
        journal.state = if journal.applied_files == 0 && journal.applied_directories == 0 {
            StoredRestoreState::Indeterminate
        } else {
            StoredRestoreState::Partial
        };
        journal.reconciliation_required = true;
        let result = self.persist_journal(journal).await;
        lock(&self.inner.state)
            .journals
            .insert(journal.restore_id.clone(), journal.clone());
        result
    }

    async fn mark_reverted(&self, restore_id: &str) -> Result<(), SnapshotError> {
        let mut journal = {
            let state = lock(&self.inner.state);
            state
                .journals
                .get(restore_id)
                .cloned()
                .ok_or(SnapshotError::NotFound)?
        };
        journal.state = StoredRestoreState::Reverted;
        journal.acknowledgement_required = false;
        self.persist_journal(&journal).await?;
        lock(&self.inner.state)
            .journals
            .insert(restore_id.to_owned(), journal);
        Ok(())
    }

    async fn load_and_recover_journals(&self) -> Result<(), SnapshotError> {
        let directory = self.inner.root.join("journals");
        let mut reader = fs::read_dir(&directory)
            .await
            .map_err(|_| SnapshotError::UnhealthyStorage)?;
        let mut journals = Vec::new();
        while let Some(entry) = reader
            .next_entry()
            .await
            .map_err(|_| SnapshotError::UnhealthyStorage)?
        {
            if journals.len() >= MAX_PRIVATE_ENTRIES {
                return Err(SnapshotError::UnhealthyStorage);
            }
            let metadata = fs::symlink_metadata(entry.path())
                .await
                .map_err(|_| SnapshotError::UnhealthyStorage)?;
            if !metadata.file_type().is_file() || metadata.len() > MAX_PRIVATE_METADATA_BYTES {
                return Err(SnapshotError::UnhealthyStorage);
            }
            let bytes = fs::read(entry.path())
                .await
                .map_err(|_| SnapshotError::UnhealthyStorage)?;
            let journal: StoredJournal =
                serde_json::from_slice(&bytes).map_err(|_| SnapshotError::UnhealthyStorage)?;
            validate_journal(&journal)?;
            if self.journal_path(&journal.restore_id)? != entry.path() {
                return Err(SnapshotError::UnhealthyStorage);
            }
            self.load_manifest(&journal.target_snapshot_id, true)
                .await?;
            self.load_manifest(&journal.pre_restore_snapshot_id, true)
                .await?;
            journals.push(journal);
        }
        let mut state = lock(&self.inner.state);
        for journal in journals {
            state.journals.insert(journal.restore_id.clone(), journal);
        }
        Ok(())
    }

    async fn recover_loaded_journals(&self) -> Result<(), SnapshotError> {
        let journals = lock(&self.inner.state)
            .journals
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let reverted = journals
            .iter()
            .filter(|journal| journal.state == StoredRestoreState::Completed)
            .filter_map(|journal| journal.unrevert_of.clone())
            .collect::<BTreeSet<_>>();
        for mut journal in journals {
            let mut changed = false;
            if journal.state == StoredRestoreState::Publishing || journal.reconciliation_required {
                self.reconcile_journal(&mut journal).await?;
                changed = true;
            }
            if reverted.contains(&journal.restore_id) {
                journal.state = StoredRestoreState::Reverted;
                journal.acknowledgement_required = false;
                changed = true;
            }
            if changed {
                self.persist_journal(&journal).await?;
                lock(&self.inner.state)
                    .journals
                    .insert(journal.restore_id.clone(), journal);
            }
        }
        Ok(())
    }

    async fn validate_existing_store(&self) -> Result<(), SnapshotError> {
        self.storage_usage().await?;
        let manifests = self.inner.root.join("manifests");
        if directory_count(&manifests, MAX_PRIVATE_ENTRIES).await? > MAX_PRIVATE_ENTRIES {
            return Err(SnapshotError::UnhealthyStorage);
        }
        let mut manifest_reader = fs::read_dir(&manifests)
            .await
            .map_err(|_| SnapshotError::UnhealthyStorage)?;
        while let Some(entry) = manifest_reader
            .next_entry()
            .await
            .map_err(|_| SnapshotError::UnhealthyStorage)?
        {
            let file_name = entry.file_name();
            let id = file_name
                .to_str()
                .and_then(|name| name.strip_suffix(".json"))
                .ok_or(SnapshotError::UnhealthyStorage)?;
            self.load_manifest(id, false).await?;
        }
        let checkpoints = self.inner.root.join("checkpoints");
        if directory_count(&checkpoints, MAX_SNAPSHOT_COUNT).await? > MAX_SNAPSHOT_COUNT {
            return Err(SnapshotError::UnhealthyStorage);
        }
        let mut checkpoint_reader = fs::read_dir(&checkpoints)
            .await
            .map_err(|_| SnapshotError::UnhealthyStorage)?;
        while let Some(entry) = checkpoint_reader
            .next_entry()
            .await
            .map_err(|_| SnapshotError::UnhealthyStorage)?
        {
            let bytes = read_private_file(&entry.path(), MAX_PRIVATE_METADATA_BYTES).await?;
            let checkpoint: StoredCheckpoint =
                serde_json::from_slice(&bytes).map_err(|_| SnapshotError::UnhealthyStorage)?;
            if checkpoint.version != CHECKPOINT_VERSION
                || self.checkpoint_path(&checkpoint.checkpoint_id) != entry.path()
            {
                return Err(SnapshotError::UnhealthyStorage);
            }
            self.load_manifest(&checkpoint.snapshot_id, false).await?;
        }
        Ok(())
    }

    async fn validate_manifests(&self) -> Result<(), SnapshotError> {
        let mut reader = fs::read_dir(self.inner.root.join("manifests"))
            .await
            .map_err(|_| SnapshotError::UnhealthyStorage)?;
        let mut count = 0_usize;
        while let Some(entry) = reader
            .next_entry()
            .await
            .map_err(|_| SnapshotError::UnhealthyStorage)?
        {
            count += 1;
            if count > MAX_PRIVATE_ENTRIES {
                return Err(SnapshotError::UnhealthyStorage);
            }
            let file_name = entry.file_name();
            let id = file_name
                .to_str()
                .and_then(|name| name.strip_suffix(".json"))
                .ok_or(SnapshotError::UnhealthyStorage)?;
            self.load_manifest(id, true).await?;
        }
        Ok(())
    }

    async fn validate_blobs(&self) -> Result<(), SnapshotError> {
        let mut blob_reader = fs::read_dir(self.inner.root.join("blobs"))
            .await
            .map_err(|_| SnapshotError::UnhealthyStorage)?;
        let mut count = 0_usize;
        while let Some(entry) = blob_reader
            .next_entry()
            .await
            .map_err(|_| SnapshotError::UnhealthyStorage)?
        {
            count += 1;
            if count > MAX_PRIVATE_ENTRIES {
                return Err(SnapshotError::UnhealthyStorage);
            }
            let name = entry
                .file_name()
                .to_str()
                .ok_or(SnapshotError::UnhealthyStorage)?
                .to_owned();
            self.load_blob(&format!("sha256:{name}")).await?;
        }
        Ok(())
    }

    async fn cleanup_private_temps(&self) -> Result<(), SnapshotError> {
        for directory_name in ["blobs", "manifests", "checkpoints", "journals"] {
            let directory = self.inner.root.join(directory_name);
            let mut reader = fs::read_dir(&directory)
                .await
                .map_err(|_| SnapshotError::UnhealthyStorage)?;
            let mut entries = 0_usize;
            let mut removed = false;
            while let Some(entry) = reader
                .next_entry()
                .await
                .map_err(|_| SnapshotError::UnhealthyStorage)?
            {
                entries += 1;
                if entries > MAX_PRIVATE_ENTRIES {
                    return Err(SnapshotError::UnhealthyStorage);
                }
                let name = entry
                    .file_name()
                    .to_str()
                    .ok_or(SnapshotError::UnhealthyStorage)?
                    .to_owned();
                if !name.starts_with('.') || !name.ends_with(".tmp") {
                    continue;
                }
                let metadata = fs::symlink_metadata(entry.path())
                    .await
                    .map_err(|_| SnapshotError::UnhealthyStorage)?;
                if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                    return Err(SnapshotError::UnhealthyStorage);
                }
                validate_private_permissions(&metadata)?;
                fs::remove_file(entry.path())
                    .await
                    .map_err(|_| SnapshotError::UnhealthyStorage)?;
                removed = true;
            }
            if removed {
                sync_directory(&directory).await?;
            }
        }
        Ok(())
    }

    async fn reconcile_journal(&self, journal: &mut StoredJournal) -> Result<(), SnapshotError> {
        let token = CancellationToken::new();
        let mut applied = 0_usize;
        let mut applied_directories = 0_usize;
        let mut unexpected = false;
        for relative in &journal.created_directories {
            let path = WorkspacePath::new(relative.clone())
                .map_err(|_| SnapshotError::UnhealthyStorage)?;
            let resolved = self
                .inner
                .workspace
                .resolve(&path)
                .await
                .map_err(map_workspace)?;
            match fs::symlink_metadata(resolved).await {
                Ok(metadata)
                    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() =>
                {
                    applied_directories += 1;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                _ => unexpected = true,
            }
        }
        for change in &journal.changes {
            let path = WorkspacePath::new(change.path.clone())
                .map_err(|_| SnapshotError::UnhealthyStorage)?;
            let resolved = self
                .inner
                .workspace
                .resolve(&path)
                .await
                .map_err(map_workspace)?;
            let current = current_file(&resolved, &token).await?;
            let target_matches = match change.kind {
                StoredChangeKind::Delete => current.is_none(),
                StoredChangeKind::Create | StoredChangeKind::Replace => {
                    current.as_ref().is_some_and(|file| {
                        Some(file.digest.as_str()) == change.target_digest.as_deref()
                            && Some(file.mode) == change.target_mode
                    })
                }
                StoredChangeKind::Conflict => false,
            };
            if target_matches {
                applied += 1;
            } else if current.as_ref().map(|file| file.revision.as_str())
                != change.current_revision.as_deref()
            {
                unexpected = true;
            }
        }
        journal.applied_files = applied;
        journal.applied_directories = applied_directories;
        if applied == journal.changes.len()
            && applied_directories == journal.created_directories.len()
            && !unexpected
        {
            journal.state = StoredRestoreState::Completed;
            journal.reconciliation_required = false;
        } else {
            journal.state = if applied == 0 && applied_directories == 0 {
                StoredRestoreState::Indeterminate
            } else {
                StoredRestoreState::Partial
            };
            journal.reconciliation_required = true;
        }
        Ok(())
    }

    async fn persist_manifest(&self, manifest: &StoredManifest) -> Result<(), SnapshotError> {
        let bytes = serde_json::to_vec(manifest).map_err(|_| SnapshotError::OperationFailed)?;
        let path = self.manifest_path(&manifest.snapshot_id)?;
        if fs::symlink_metadata(&path).await.is_ok() {
            let existing = self.load_manifest(&manifest.snapshot_id, false).await?;
            if existing.content == manifest.content {
                return Ok(());
            }
            return Err(SnapshotError::IntegrityFailure);
        }
        if self.manifest_count().await? >= MAX_SNAPSHOT_COUNT {
            return Err(SnapshotError::QuotaExceeded);
        }
        self.ensure_storage_capacity(&path, bytes.len() as u64)
            .await?;
        immutable_write(&path, &bytes).await
    }

    async fn load_manifest(
        &self,
        manifest_id: &str,
        verify_blobs: bool,
    ) -> Result<StoredManifest, SnapshotError> {
        validate_snapshot_id(manifest_id)?;
        let path = self.manifest_path(manifest_id)?;
        let bytes = read_private_file(&path, MAX_PRIVATE_METADATA_BYTES).await?;
        let manifest: StoredManifest =
            serde_json::from_slice(&bytes).map_err(|_| SnapshotError::IntegrityFailure)?;
        if manifest.snapshot_id != manifest_id
            || manifest.content.version != MANIFEST_VERSION
            || snapshot_id(&manifest.content)? != manifest_id
            || manifest.content.files.len() > MAX_SNAPSHOT_FILES
            || manifest.content.exclusions.len() > MAX_EXCLUSIONS
        {
            return Err(SnapshotError::IntegrityFailure);
        }
        let mut total = 0_u64;
        let mut previous = None;
        for file in &manifest.content.files {
            validate_stored_file(file)?;
            if previous.is_some_and(|path: &str| path >= file.path.as_str()) {
                return Err(SnapshotError::IntegrityFailure);
            }
            previous = Some(file.path.as_str());
            total = total.saturating_add(file.size_bytes);
            if total > MAX_SNAPSHOT_TOTAL_BYTES {
                return Err(SnapshotError::IntegrityFailure);
            }
            if verify_blobs {
                self.load_blob(&file.digest).await?;
            }
        }
        Ok(manifest)
    }

    async fn persist_blob(&self, digest: &str, bytes: &[u8]) -> Result<(), SnapshotError> {
        if bytes.len() as u64 > MAX_SNAPSHOT_FILE_BYTES || digest_bytes(bytes) != digest {
            return Err(SnapshotError::IntegrityFailure);
        }
        let path = self.blob_path(digest)?;
        if fs::symlink_metadata(&path).await.is_ok() {
            self.load_blob(digest).await?;
            return Ok(());
        }
        self.ensure_storage_capacity(&path, bytes.len() as u64)
            .await?;
        immutable_write(&path, bytes).await
    }

    async fn load_blob(&self, digest: &str) -> Result<Vec<u8>, SnapshotError> {
        let path = self.blob_path(digest)?;
        let bytes = read_private_file(&path, MAX_SNAPSHOT_FILE_BYTES).await?;
        if bytes.len() as u64 > MAX_SNAPSHOT_FILE_BYTES || digest_bytes(&bytes) != digest {
            return Err(SnapshotError::IntegrityFailure);
        }
        Ok(bytes)
    }

    async fn persist_checkpoint(
        &self,
        checkpoint_id: &str,
        snapshot_id: &str,
    ) -> Result<(), SnapshotError> {
        let checkpoint = StoredCheckpoint {
            version: CHECKPOINT_VERSION.to_owned(),
            checkpoint_id: checkpoint_id.to_owned(),
            snapshot_id: snapshot_id.to_owned(),
        };
        let bytes = serde_json::to_vec(&checkpoint).map_err(|_| SnapshotError::OperationFailed)?;
        let path = self.checkpoint_path(checkpoint_id);
        self.ensure_storage_capacity(&path, bytes.len() as u64)
            .await?;
        atomic_write(&path, &bytes).await
    }

    async fn load_checkpoint(
        &self,
        checkpoint_id: &str,
    ) -> Result<Option<StoredManifest>, SnapshotError> {
        let path = self.checkpoint_path(checkpoint_id);
        let bytes = match read_private_file(&path, MAX_PRIVATE_METADATA_BYTES).await {
            Ok(bytes) => bytes,
            Err(SnapshotError::NotFound) => return Ok(None),
            Err(error) => return Err(error),
        };
        let checkpoint: StoredCheckpoint =
            serde_json::from_slice(&bytes).map_err(|_| SnapshotError::IntegrityFailure)?;
        if checkpoint.version != CHECKPOINT_VERSION || checkpoint.checkpoint_id != checkpoint_id {
            return Err(SnapshotError::IntegrityFailure);
        }
        self.load_manifest(&checkpoint.snapshot_id, true)
            .await
            .map(Some)
    }

    async fn persist_journal(&self, journal: &StoredJournal) -> Result<(), SnapshotError> {
        validate_journal(journal)?;
        let bytes = serialize_journal(journal)?;
        let path = self.journal_path(&journal.restore_id)?;
        self.ensure_journal_capacity(&path, bytes.len() as u64)
            .await?;
        self.ensure_storage_capacity(&path, bytes.len() as u64)
            .await?;
        atomic_write(&path, &bytes).await
    }

    async fn ensure_storage_capacity(
        &self,
        replacing: &Path,
        new_bytes: u64,
    ) -> Result<(), SnapshotError> {
        let replaced = private_file_size(replacing).await?;
        let prospective = self
            .storage_usage()
            .await?
            .checked_sub(replaced)
            .and_then(|usage| usage.checked_add(new_bytes))
            .ok_or(SnapshotError::QuotaExceeded)?;
        if prospective > MAX_SNAPSHOT_STORAGE_BYTES {
            return Err(SnapshotError::QuotaExceeded);
        }
        Ok(())
    }

    async fn ensure_journal_capacity(
        &self,
        replacing: &Path,
        new_bytes: u64,
    ) -> Result<(), SnapshotError> {
        let (count, bytes) = self.journal_usage().await?;
        let replaced = private_file_size(replacing).await?;
        let prospective_count = count.saturating_add(usize::from(replaced == 0));
        let prospective_bytes = bytes
            .checked_sub(replaced)
            .and_then(|usage| usage.checked_add(new_bytes))
            .ok_or(SnapshotError::QuotaExceeded)?;
        if prospective_count > MAX_SNAPSHOT_JOURNALS
            || prospective_bytes > MAX_JOURNAL_STORAGE_BYTES
        {
            return Err(SnapshotError::QuotaExceeded);
        }
        Ok(())
    }

    async fn journal_usage(&self) -> Result<(usize, u64), SnapshotError> {
        let directory = self.inner.root.join("journals");
        let mut reader = fs::read_dir(directory)
            .await
            .map_err(|_| SnapshotError::UnhealthyStorage)?;
        let mut count = 0_usize;
        let mut bytes = 0_u64;
        while let Some(entry) = reader
            .next_entry()
            .await
            .map_err(|_| SnapshotError::UnhealthyStorage)?
        {
            count += 1;
            if count > MAX_PRIVATE_ENTRIES {
                return Err(SnapshotError::UnhealthyStorage);
            }
            bytes = bytes
                .checked_add(private_file_size(&entry.path()).await?)
                .ok_or(SnapshotError::UnhealthyStorage)?;
        }
        Ok((count, bytes))
    }

    async fn reclaim_startup_journals_to_quota(&self) -> Result<(), SnapshotError> {
        let (mut count, mut bytes) = self.journal_usage().await?;
        if count <= MAX_SNAPSHOT_JOURNALS && bytes <= MAX_JOURNAL_STORAGE_BYTES {
            return Ok(());
        }
        let mut reclaimable = lock(&self.inner.state)
            .journals
            .values()
            .filter(|journal| journal.reclaimable())
            .map(|journal| journal.restore_id.clone())
            .collect::<Vec<_>>();
        reclaimable.sort();
        let mut removed = false;
        for restore_id in reclaimable {
            let path = self.journal_path(&restore_id)?;
            let journal_bytes = private_file_size(&path).await?;
            fs::remove_file(path)
                .await
                .map_err(|_| SnapshotError::UnhealthyStorage)?;
            lock(&self.inner.state).journals.remove(&restore_id);
            count = count.saturating_sub(1);
            bytes = bytes.saturating_sub(journal_bytes);
            removed = true;
            if count <= MAX_SNAPSHOT_JOURNALS && bytes <= MAX_JOURNAL_STORAGE_BYTES {
                break;
            }
        }
        if count > MAX_SNAPSHOT_JOURNALS || bytes > MAX_JOURNAL_STORAGE_BYTES {
            return Err(SnapshotError::UnhealthyStorage);
        }
        if removed {
            sync_directory(&self.inner.root.join("journals")).await?;
        }
        Ok(())
    }

    async fn make_journal_room(
        &self,
        restore_id: &str,
        new_bytes: u64,
    ) -> Result<(), SnapshotError> {
        let path = self.journal_path(restore_id)?;
        match self.ensure_journal_capacity(&path, new_bytes).await {
            Ok(()) => return Ok(()),
            Err(SnapshotError::QuotaExceeded) => {}
            Err(error) => return Err(error),
        }
        self.remove_reclaimable_journals().await?;
        self.recover_orphans().await?;
        self.ensure_journal_capacity(&path, new_bytes).await
    }

    fn protected_snapshots(&self, excluded_lease: Option<&str>) -> BTreeSet<String> {
        let state = lock(&self.inner.state);
        let mut protected = BTreeSet::new();
        for journal in state
            .journals
            .values()
            .filter(|journal| !journal.reclaimable())
        {
            protected.insert(journal.target_snapshot_id.clone());
            protected.insert(journal.pre_restore_snapshot_id.clone());
        }
        for (lease, pending) in &state.pending {
            if Some(lease.as_str()) != excluded_lease {
                protected.extend(pending.snapshots.iter().cloned());
            }
        }
        protected
    }

    async fn cleanup_plan(
        &self,
        requested: &BTreeSet<String>,
        excluded_lease: Option<&str>,
    ) -> Result<CleanupPlan, SnapshotError> {
        self.cleanup_plan_bounded(requested, excluded_lease, usize::MAX)
            .await
    }

    async fn cleanup_plan_bounded(
        &self,
        requested: &BTreeSet<String>,
        excluded_lease: Option<&str>,
        maximum_retained_bytes: usize,
    ) -> Result<CleanupPlan, SnapshotError> {
        let protected = self.protected_snapshots(excluded_lease);
        let eligible = requested
            .difference(&protected)
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut checkpoints = Vec::new();
        let mut retained_references = protected;
        let mut checkpoint_reader = fs::read_dir(self.inner.root.join("checkpoints"))
            .await
            .map_err(|_| SnapshotError::OperationFailed)?;
        let mut checkpoint_count = 0_usize;
        while let Some(entry) = checkpoint_reader
            .next_entry()
            .await
            .map_err(|_| SnapshotError::OperationFailed)?
        {
            checkpoint_count += 1;
            if checkpoint_count > MAX_PRIVATE_ENTRIES {
                return Err(SnapshotError::LimitExceeded);
            }
            let bytes = read_private_file(&entry.path(), MAX_PRIVATE_METADATA_BYTES).await?;
            let checkpoint: StoredCheckpoint =
                serde_json::from_slice(&bytes).map_err(|_| SnapshotError::IntegrityFailure)?;
            if checkpoint.version != CHECKPOINT_VERSION
                || self.checkpoint_path(&checkpoint.checkpoint_id) != entry.path()
            {
                return Err(SnapshotError::IntegrityFailure);
            }
            if eligible.contains(&checkpoint.snapshot_id) {
                checkpoints.push(checkpoint);
            } else {
                retained_references.insert(checkpoint.snapshot_id);
            }
        }
        checkpoints.sort_by(|left, right| left.checkpoint_id.cmp(&right.checkpoint_id));

        let mut journals = Vec::new();
        for journal in lock(&self.inner.state).journals.values() {
            if journal.reclaimable() {
                let prospective = retained_vec(&journals, StoredJournal::retained_bytes)
                    .saturating_add(journal.retained_bytes());
                enforce_snapshot_preparation_bytes(prospective, maximum_retained_bytes)?;
                journals.push(journal.clone());
            } else {
                retained_references.insert(journal.target_snapshot_id.clone());
                retained_references.insert(journal.pre_restore_snapshot_id.clone());
            }
        }
        journals.sort_by(|left, right| left.restore_id.cmp(&right.restore_id));

        let mut manifest_reader = fs::read_dir(self.inner.root.join("manifests"))
            .await
            .map_err(|_| SnapshotError::OperationFailed)?;
        let mut all_manifests = Vec::new();
        while let Some(entry) = manifest_reader
            .next_entry()
            .await
            .map_err(|_| SnapshotError::OperationFailed)?
        {
            if all_manifests.len() >= MAX_PRIVATE_ENTRIES {
                return Err(SnapshotError::LimitExceeded);
            }
            let file_name = entry.file_name();
            let id = file_name
                .to_str()
                .and_then(|name| name.strip_suffix(".json"))
                .ok_or(SnapshotError::IntegrityFailure)?
                .to_owned();
            self.load_manifest(&id, false).await?;
            all_manifests.push(id);
        }
        all_manifests.sort();
        let manifests = all_manifests
            .iter()
            .filter(|id| !retained_references.contains(*id))
            .cloned()
            .collect::<Vec<_>>();
        let deleting_manifests = manifests.iter().cloned().collect::<BTreeSet<_>>();
        let mut reachable_blobs = BTreeSet::new();
        for id in all_manifests
            .iter()
            .filter(|id| !deleting_manifests.contains(*id))
        {
            reachable_blobs.extend(
                self.load_manifest(id, false)
                    .await?
                    .content
                    .files
                    .into_iter()
                    .map(|file| file.digest),
            );
        }

        let mut blob_reader = fs::read_dir(self.inner.root.join("blobs"))
            .await
            .map_err(|_| SnapshotError::OperationFailed)?;
        let mut blobs = Vec::new();
        let mut blob_count = 0_usize;
        while let Some(entry) = blob_reader
            .next_entry()
            .await
            .map_err(|_| SnapshotError::OperationFailed)?
        {
            blob_count += 1;
            if blob_count > MAX_PRIVATE_ENTRIES {
                return Err(SnapshotError::LimitExceeded);
            }
            let name = entry
                .file_name()
                .to_str()
                .ok_or(SnapshotError::IntegrityFailure)?
                .to_owned();
            let digest = format!("sha256:{name}");
            self.blob_path(&digest)?;
            if !reachable_blobs.contains(&digest) {
                blobs.push(digest);
            }
        }
        blobs.sort();

        let mut reclaimed_bytes = 0_u64;
        for checkpoint in &checkpoints {
            reclaimed_bytes = reclaimed_bytes.saturating_add(
                private_file_size(&self.checkpoint_path(&checkpoint.checkpoint_id)).await?,
            );
        }
        for journal in &journals {
            reclaimed_bytes = reclaimed_bytes
                .saturating_add(private_file_size(&self.journal_path(&journal.restore_id)?).await?);
        }
        for id in &manifests {
            reclaimed_bytes =
                reclaimed_bytes.saturating_add(private_file_size(&self.manifest_path(id)?).await?);
        }
        for digest in &blobs {
            reclaimed_bytes =
                reclaimed_bytes.saturating_add(private_file_size(&self.blob_path(digest)?).await?);
        }
        let plan = CleanupPlan {
            checkpoints,
            journals,
            manifests,
            blobs,
            reclaimed_bytes,
        };
        enforce_snapshot_preparation_bytes(plan.retained_bytes(), maximum_retained_bytes)?;
        Ok(plan)
    }

    async fn reachable_blobs(
        &self,
        excluding_manifests: &BTreeSet<String>,
        excluded_lease: Option<&str>,
    ) -> Result<BTreeSet<String>, SnapshotError> {
        let mut reachable = BTreeSet::new();
        let mut reader = fs::read_dir(self.inner.root.join("manifests"))
            .await
            .map_err(|_| SnapshotError::OperationFailed)?;
        let mut count = 0_usize;
        while let Some(entry) = reader
            .next_entry()
            .await
            .map_err(|_| SnapshotError::OperationFailed)?
        {
            count += 1;
            if count > MAX_PRIVATE_ENTRIES {
                return Err(SnapshotError::LimitExceeded);
            }
            let file_name = entry.file_name();
            let Some(name) = file_name
                .to_str()
                .and_then(|name| name.strip_suffix(".json"))
            else {
                return Err(SnapshotError::IntegrityFailure);
            };
            if excluding_manifests.contains(name) {
                continue;
            }
            let manifest = self.load_manifest(name, false).await?;
            reachable.extend(manifest.content.files.into_iter().map(|file| file.digest));
        }
        for id in self.protected_snapshots(excluded_lease) {
            if excluding_manifests.contains(&id) {
                continue;
            }
            if let Ok(manifest) = self.load_manifest(&id, false).await {
                reachable.extend(manifest.content.files.into_iter().map(|file| file.digest));
            }
        }
        Ok(reachable)
    }

    async fn gc_blobs(
        &self,
        excluded_lease: Option<&str>,
        token: &CancellationToken,
    ) -> Result<(usize, u64), SnapshotError> {
        let reachable = self
            .reachable_blobs(&BTreeSet::new(), excluded_lease)
            .await?;
        let mut deleted = 0_usize;
        let mut bytes = 0_u64;
        let directory = self.inner.root.join("blobs");
        let mut reader = fs::read_dir(&directory)
            .await
            .map_err(|_| SnapshotError::OperationFailed)?;
        let mut entries = 0_usize;
        while let Some(entry) = reader
            .next_entry()
            .await
            .map_err(|_| SnapshotError::OperationFailed)?
        {
            entries += 1;
            if entries > MAX_PRIVATE_ENTRIES {
                return Err(SnapshotError::LimitExceeded);
            }
            check_cancelled(token)?;
            let name = entry
                .file_name()
                .to_str()
                .ok_or(SnapshotError::IntegrityFailure)?
                .to_owned();
            let digest = format!("sha256:{name}");
            if reachable.contains(&digest) {
                continue;
            }
            let metadata = fs::symlink_metadata(entry.path())
                .await
                .map_err(|_| SnapshotError::OperationFailed)?;
            if !metadata.file_type().is_file() {
                return Err(SnapshotError::IntegrityFailure);
            }
            fs::remove_file(entry.path())
                .await
                .map_err(|_| SnapshotError::OperationFailed)?;
            deleted += 1;
            bytes = bytes.saturating_add(metadata.len());
        }
        sync_directory(&directory).await?;
        Ok((deleted, bytes))
    }

    async fn remove_reclaimable_journals(&self) -> Result<(), SnapshotError> {
        let removable = {
            let state = lock(&self.inner.state);
            state
                .journals
                .values()
                .filter(|journal| journal.reclaimable())
                .map(|journal| journal.restore_id.clone())
                .collect::<Vec<_>>()
        };
        for id in &removable {
            let path = self.journal_path(id)?;
            match fs::remove_file(path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(SnapshotError::OperationFailed),
            }
        }
        if !removable.is_empty() {
            sync_directory(&self.inner.root.join("journals")).await?;
        }
        let mut state = lock(&self.inner.state);
        for id in removable {
            state.journals.remove(&id);
        }
        Ok(())
    }

    async fn gc_orphan_manifests(&self, token: &CancellationToken) -> Result<u64, SnapshotError> {
        let mut referenced = self.protected_snapshots(None);
        for journal in lock(&self.inner.state).journals.values() {
            referenced.insert(journal.target_snapshot_id.clone());
            referenced.insert(journal.pre_restore_snapshot_id.clone());
        }
        let checkpoints = self.inner.root.join("checkpoints");
        let mut checkpoint_reader = fs::read_dir(&checkpoints)
            .await
            .map_err(|_| SnapshotError::OperationFailed)?;
        let mut checkpoint_count = 0_usize;
        while let Some(entry) = checkpoint_reader
            .next_entry()
            .await
            .map_err(|_| SnapshotError::OperationFailed)?
        {
            checkpoint_count += 1;
            if checkpoint_count > MAX_PRIVATE_ENTRIES {
                return Err(SnapshotError::LimitExceeded);
            }
            let bytes = read_private_file(&entry.path(), MAX_PRIVATE_METADATA_BYTES).await?;
            let checkpoint: StoredCheckpoint =
                serde_json::from_slice(&bytes).map_err(|_| SnapshotError::IntegrityFailure)?;
            if checkpoint.version != CHECKPOINT_VERSION {
                return Err(SnapshotError::IntegrityFailure);
            }
            referenced.insert(checkpoint.snapshot_id);
        }
        let manifests = self.inner.root.join("manifests");
        let mut manifest_reader = fs::read_dir(&manifests)
            .await
            .map_err(|_| SnapshotError::OperationFailed)?;
        let mut reclaimed = 0_u64;
        let mut manifest_count = 0_usize;
        while let Some(entry) = manifest_reader
            .next_entry()
            .await
            .map_err(|_| SnapshotError::OperationFailed)?
        {
            manifest_count += 1;
            if manifest_count > MAX_PRIVATE_ENTRIES {
                return Err(SnapshotError::LimitExceeded);
            }
            check_cancelled(token)?;
            let file_name = entry.file_name();
            let id = file_name
                .to_str()
                .and_then(|name| name.strip_suffix(".json"))
                .ok_or(SnapshotError::IntegrityFailure)?;
            if referenced.contains(id) {
                continue;
            }
            let metadata = fs::symlink_metadata(entry.path())
                .await
                .map_err(|_| SnapshotError::OperationFailed)?;
            fs::remove_file(entry.path())
                .await
                .map_err(|_| SnapshotError::OperationFailed)?;
            reclaimed = reclaimed.saturating_add(metadata.len());
        }
        sync_directory(&manifests).await?;
        Ok(reclaimed)
    }

    async fn recover_orphans(&self) -> Result<(), SnapshotError> {
        let token = CancellationToken::new();
        self.gc_orphan_manifests(&token).await?;
        self.gc_blobs(None, &token).await?;
        Ok(())
    }

    async fn manifest_count(&self) -> Result<usize, SnapshotError> {
        directory_count(&self.inner.root.join("manifests"), MAX_SNAPSHOT_COUNT).await
    }

    async fn storage_usage(&self) -> Result<u64, SnapshotError> {
        let mut total = 0_u64;
        for name in ["blobs", "manifests", "checkpoints", "journals"] {
            let mut reader = fs::read_dir(self.inner.root.join(name))
                .await
                .map_err(|_| SnapshotError::UnhealthyStorage)?;
            let mut entries = 0_usize;
            while let Some(entry) = reader
                .next_entry()
                .await
                .map_err(|_| SnapshotError::UnhealthyStorage)?
            {
                entries += 1;
                if entries > MAX_PRIVATE_ENTRIES {
                    return Err(SnapshotError::UnhealthyStorage);
                }
                let metadata = fs::symlink_metadata(entry.path())
                    .await
                    .map_err(|_| SnapshotError::UnhealthyStorage)?;
                if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                    return Err(SnapshotError::UnhealthyStorage);
                }
                validate_private_permissions(&metadata)
                    .map_err(|_| SnapshotError::UnhealthyStorage)?;
                total = total
                    .checked_add(metadata.len())
                    .ok_or(SnapshotError::UnhealthyStorage)?;
            }
        }
        Ok(total)
    }

    fn manifest_path(&self, snapshot_id: &str) -> Result<PathBuf, SnapshotError> {
        validate_snapshot_id(snapshot_id)?;
        Ok(self
            .inner
            .root
            .join("manifests")
            .join(format!("{snapshot_id}.json")))
    }

    fn blob_path(&self, digest: &str) -> Result<PathBuf, SnapshotError> {
        let hex = digest
            .strip_prefix("sha256:")
            .filter(|hex| valid_hex_digest(hex))
            .ok_or(SnapshotError::IntegrityFailure)?;
        Ok(self.inner.root.join("blobs").join(hex))
    }

    fn checkpoint_path(&self, checkpoint_id: &str) -> PathBuf {
        self.inner
            .root
            .join("checkpoints")
            .join(format!("{}.json", hex_sha256(checkpoint_id.as_bytes())))
    }

    fn journal_path(&self, restore_id: &str) -> Result<PathBuf, SnapshotError> {
        if !restore_id.starts_with("restore_")
            || restore_id.len() > 64
            || !restore_id.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
            })
        {
            return Err(SnapshotError::IntegrityFailure);
        }
        Ok(self
            .inner
            .root
            .join("journals")
            .join(format!("{restore_id}.json")))
    }
}

fn validate_store_binding(binding: &str) -> Result<(), SnapshotError> {
    if binding.len() > workcell_host_contract::MAX_ID_BYTES
        || !binding
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(SnapshotError::InvalidConfiguration);
    }
    Ok(())
}

impl PreparedSnapshotRestore {
    pub fn preview(&self) -> Result<SnapshotRestorePreview, SnapshotError> {
        Ok(SnapshotRestorePreview {
            restore_id: identifier(&self.restore_id)?,
            target_snapshot_id: identifier(&self.target.snapshot_id)?,
            current_revision: revision(&self.baseline_revision)?,
            target_revision: revision(&manifest_revision(&self.target.content)?)?,
            changes: self
                .changes
                .iter()
                .map(StoredChange::contract)
                .collect::<Result<_, _>>()?,
            created_directories: self
                .created_directories
                .iter()
                .cloned()
                .map(WorkspacePath::new)
                .collect::<Result<_, _>>()
                .map_err(|_| SnapshotError::IntegrityFailure)?,
        })
    }

    #[must_use]
    pub fn restore_id(&self) -> &str {
        &self.restore_id
    }

    #[must_use]
    pub fn pre_restore_snapshot_id(&self) -> &str {
        &self.pre_restore_snapshot_id
    }

    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.changes.iter().map(|change| change.path.as_str())
    }

    pub fn created_directories(&self) -> impl Iterator<Item = &str> {
        self.created_directories.iter().map(String::as_str)
    }

    fn maximum_journal_bytes(&self) -> Result<u64, SnapshotError> {
        let journal = StoredJournal {
            version: JOURNAL_VERSION.to_owned(),
            restore_id: self.restore_id.clone(),
            state: StoredRestoreState::Indeterminate,
            target_snapshot_id: self.target.snapshot_id.clone(),
            pre_restore_snapshot_id: self.pre_restore_snapshot_id.clone(),
            changes: self.changes.clone(),
            created_directories: self.created_directories.clone(),
            applied_files: self.changes.len(),
            applied_directories: self.created_directories.len(),
            reconciliation_required: true,
            acknowledgement_required: true,
            unrevert_of: self.unrevert_of.clone(),
        };
        Ok(serialize_journal(&journal)?.len() as u64)
    }
}

impl Drop for PreparedSnapshotRestore {
    fn drop(&mut self) {
        lock(&self.manager.inner.state)
            .pending
            .remove(&self.lease_id);
    }
}

impl PreparedSnapshotCleanup {
    #[must_use]
    pub const fn preview(&self) -> &SnapshotCleanupPreview {
        &self.preview
    }

    pub fn snapshot_ids(&self) -> impl Iterator<Item = &str> {
        self.preview.snapshot_ids.iter().map(Identifier::as_str)
    }

    #[must_use]
    pub fn resource_scope(&self) -> &str {
        &self.resource_scope
    }
}

impl Drop for PreparedSnapshotCleanup {
    fn drop(&mut self) {
        lock(&self.manager.inner.state)
            .pending
            .remove(&self.lease_id);
    }
}

impl StoredManifest {
    fn summary(&self, checkpoint_id: Option<&str>) -> Result<SnapshotSummary, SnapshotError> {
        Ok(SnapshotSummary {
            snapshot_id: identifier(&self.snapshot_id)?,
            checkpoint_id: checkpoint_id.map(identifier).transpose()?,
            state: SnapshotState::Complete,
            manifest_revision: revision(&manifest_revision(&self.content)?)?,
            file_count: u32::try_from(self.content.files.len()).unwrap_or(u32::MAX),
            total_bytes: self.content.files.iter().map(|file| file.size_bytes).sum(),
            created_at_unix_ms: self.created_at_unix_ms,
        })
    }
}

impl StoredFile {
    fn with_path(mut self, path: String) -> Result<Self, SnapshotError> {
        self.path = path.clone();
        self.resource_id = root_relative_resource_id(RootResourceKind::Path, &path)
            .map_err(map_workspace)?
            .as_str()
            .to_owned();
        Ok(self)
    }

    fn contract(&self) -> Result<SnapshotFile, SnapshotError> {
        Ok(SnapshotFile {
            path: WorkspacePath::new(self.path.clone())
                .map_err(|_| SnapshotError::IntegrityFailure)?,
            resource_id: ResourceId::new(self.resource_id.clone())
                .map_err(|_| SnapshotError::IntegrityFailure)?,
            identity: revision(&self.identity)?,
            revision: revision(&self.revision)?,
            digest: revision(&self.digest)?,
            mode: self.mode,
            size_bytes: self.size_bytes,
        })
    }
}

impl StoredChange {
    fn contract(&self) -> Result<SnapshotChange, SnapshotError> {
        Ok(SnapshotChange {
            path: WorkspacePath::new(self.path.clone())
                .map_err(|_| SnapshotError::IntegrityFailure)?,
            resource_id: ResourceId::new(self.resource_id.clone())
                .map_err(|_| SnapshotError::IntegrityFailure)?,
            kind: match self.kind {
                StoredChangeKind::Create => SnapshotChangeKind::Create,
                StoredChangeKind::Replace => SnapshotChangeKind::Replace,
                StoredChangeKind::Delete => SnapshotChangeKind::Delete,
                StoredChangeKind::Conflict => SnapshotChangeKind::Conflict,
            },
            current_revision: self.current_revision.as_deref().map(revision).transpose()?,
            target_revision: self.target_revision.as_deref().map(revision).transpose()?,
        })
    }
}

impl StoredJournal {
    fn reclaimable(&self) -> bool {
        !self.acknowledgement_required
            && !self.reconciliation_required
            && matches!(
                self.state,
                StoredRestoreState::Completed
                    | StoredRestoreState::Acknowledged
                    | StoredRestoreState::Reverted
            )
    }

    fn contract_status(&self) -> Result<SnapshotRestoreStatus, SnapshotError> {
        Ok(SnapshotRestoreStatus {
            restore_id: identifier(&self.restore_id)?,
            state: match self.state {
                StoredRestoreState::Publishing => SnapshotRestoreState::Publishing,
                StoredRestoreState::Completed => SnapshotRestoreState::Completed,
                StoredRestoreState::Partial => SnapshotRestoreState::Partial,
                StoredRestoreState::Indeterminate => SnapshotRestoreState::Indeterminate,
                StoredRestoreState::Acknowledged => SnapshotRestoreState::Acknowledged,
                StoredRestoreState::Reverted => SnapshotRestoreState::Reverted,
            },
            target_snapshot_id: identifier(&self.target_snapshot_id)?,
            pre_restore_snapshot_id: identifier(&self.pre_restore_snapshot_id)?,
            applied_files: u32::try_from(self.applied_files).unwrap_or(u32::MAX),
            total_files: u32::try_from(self.changes.len()).unwrap_or(u32::MAX),
            acknowledgement_required: self.acknowledgement_required,
            reconciliation_required: self.reconciliation_required,
            unrevert_of: self.unrevert_of.as_deref().map(identifier).transpose()?,
        })
    }
}

fn compare_manifests(
    current: &StoredManifest,
    target: &StoredManifest,
) -> Result<Vec<StoredChange>, SnapshotError> {
    let current_files = current
        .content
        .files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect::<BTreeMap<_, _>>();
    let target_files = target
        .content
        .files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect::<BTreeMap<_, _>>();
    let paths = current_files
        .keys()
        .chain(target_files.keys())
        .copied()
        .collect::<BTreeSet<_>>();
    let mut changes = Vec::new();
    for path in paths {
        let current = current_files.get(path).copied();
        let target = target_files.get(path).copied();
        if current.map(|file| (&file.revision, file.mode))
            == target.map(|file| (&file.revision, file.mode))
        {
            continue;
        }
        let parent_conflict =
            target.is_some() && ancestors(path).any(|parent| current_files.contains_key(parent));
        let kind = if parent_conflict {
            StoredChangeKind::Conflict
        } else {
            match (current, target) {
                (None, Some(_)) => StoredChangeKind::Create,
                (Some(_), Some(_)) => StoredChangeKind::Replace,
                (Some(_), None) => StoredChangeKind::Delete,
                (None, None) => continue,
            }
        };
        let source = target.or(current).ok_or(SnapshotError::OperationFailed)?;
        changes.push(StoredChange {
            path: path.to_owned(),
            resource_id: source.resource_id.clone(),
            kind,
            current_revision: current.map(|file| file.revision.clone()),
            target_revision: target.map(|file| file.revision.clone()),
            target_digest: target.map(|file| file.digest.clone()),
            target_mode: target.map(|file| file.mode),
        });
    }
    if changes.len() > MAX_SNAPSHOT_FILES {
        return Err(SnapshotError::LimitExceeded);
    }
    Ok(changes)
}

fn ancestors(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/').map(|(index, _)| &path[..index])
}

async fn read_stable_file(
    path: &Path,
    before: std::fs::Metadata,
    retain_bytes: bool,
    token: &CancellationToken,
) -> Result<(StoredFile, Vec<u8>), SnapshotError> {
    check_cancelled(token)?;
    let file = fs::File::open(path)
        .await
        .map_err(|_| SnapshotError::WorkspaceChanged)?;
    let (bytes, digest) = if retain_bytes {
        let mut bytes = Vec::with_capacity(before.len() as usize);
        file.take(MAX_SNAPSHOT_FILE_BYTES.saturating_add(1))
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| SnapshotError::WorkspaceChanged)?;
        if bytes.len() as u64 > MAX_SNAPSHOT_FILE_BYTES {
            return Err(SnapshotError::LimitExceeded);
        }
        let digest = digest_bytes(&bytes);
        (bytes, digest)
    } else {
        let mut file = file;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0_u8; 64 * 1_024];
        let mut bytes_read = 0_u64;
        loop {
            let read = file
                .read(&mut buffer)
                .await
                .map_err(|_| SnapshotError::WorkspaceChanged)?;
            if read == 0 {
                break;
            }
            bytes_read = bytes_read.saturating_add(read as u64);
            if bytes_read > MAX_SNAPSHOT_FILE_BYTES {
                return Err(SnapshotError::LimitExceeded);
            }
            hasher.update(&buffer[..read]);
        }
        (Vec::new(), format_sha256(hasher.finalize()))
    };
    check_cancelled(token)?;
    let after = fs::symlink_metadata(path)
        .await
        .map_err(|_| SnapshotError::WorkspaceChanged)?;
    if !after.file_type().is_file() || metadata_stamp(&before) != metadata_stamp(&after) {
        return Err(SnapshotError::WorkspaceChanged);
    }
    let identity = file_identity(&after);
    let mode = file_mode(&after);
    let revision =
        digest_serializable(&(identity.as_str(), metadata_stamp(&after), mode, &digest))?;
    Ok((
        StoredFile {
            path: String::new(),
            resource_id: String::new(),
            identity,
            revision,
            digest,
            mode,
            size_bytes: after.len(),
        },
        bytes,
    ))
}

async fn current_revision(
    path: &Path,
    token: &CancellationToken,
) -> Result<Option<String>, SnapshotError> {
    let metadata = match fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(SnapshotError::OperationFailed),
    };
    if !metadata.file_type().is_file() {
        return Err(SnapshotError::Conflict);
    }
    let (file, _) = read_stable_file(path, metadata, false, token).await?;
    Ok(Some(file.revision))
}

async fn current_file(
    path: &Path,
    token: &CancellationToken,
) -> Result<Option<StoredFile>, SnapshotError> {
    let metadata = match fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(SnapshotError::OperationFailed),
    };
    if !metadata.file_type().is_file() {
        return Err(SnapshotError::Conflict);
    }
    let (file, _) = read_stable_file(path, metadata, false, token).await?;
    Ok(Some(file))
}

async fn atomic_workspace_write(
    workspace: &WorkspaceSnapshotAccess,
    relative: &WorkspacePath,
    destination: &Path,
    bytes: &[u8],
    mode: u32,
    expected: Option<&str>,
    token: &CancellationToken,
) -> Result<(), SnapshotError> {
    let parent = destination.parent().ok_or(SnapshotError::OperationFailed)?;
    validate_workspace_parent(parent).await?;
    let basename = destination
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(SnapshotError::UnsupportedFile)?;
    let temporary = parent.join(format!(".{basename}.{}.tmp", Uuid::new_v4()));
    let result = async {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options
            .open(&temporary)
            .await
            .map_err(|_| SnapshotError::OperationFailed)?;
        file.write_all(bytes)
            .await
            .map_err(|_| SnapshotError::OperationFailed)?;
        file.sync_all()
            .await
            .map_err(|_| SnapshotError::OperationFailed)?;
        set_mode(&temporary, mode).await?;
        check_cancelled(token)?;
        let resolved = workspace.resolve(relative).await.map_err(map_workspace)?;
        if resolved != destination
            || current_revision(destination, token).await?.as_deref() != expected
        {
            return Err(SnapshotError::Conflict);
        }
        if expected.is_none() {
            fs::hard_link(&temporary, destination)
                .await
                .map_err(|_| SnapshotError::Conflict)?;
            fs::remove_file(&temporary)
                .await
                .map_err(|_| SnapshotError::OperationFailed)?;
        } else {
            fs::rename(&temporary, destination)
                .await
                .map_err(|_| SnapshotError::OperationFailed)?;
        }
        sync_directory(parent).await
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_file(&temporary).await;
    }
    result
}

async fn validate_workspace_parent(parent: &Path) -> Result<(), SnapshotError> {
    let metadata = fs::symlink_metadata(parent)
        .await
        .map_err(|_| SnapshotError::Conflict)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(SnapshotError::Conflict);
    }
    Ok(())
}

#[cfg(unix)]
async fn set_mode(path: &Path, mode: u32) -> Result<(), SnapshotError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o777))
        .await
        .map_err(|_| SnapshotError::OperationFailed)
}

#[cfg(not(unix))]
async fn set_mode(_path: &Path, _mode: u32) -> Result<(), SnapshotError> {
    Ok(())
}

fn validate_private_root(requested: &Path, workspace: &Path) -> Result<PathBuf, SnapshotError> {
    if !requested.is_absolute() {
        return Err(SnapshotError::InvalidConfiguration);
    }
    reject_symlink_components(requested)?;
    let root = requested
        .canonicalize()
        .map_err(|_| SnapshotError::InvalidConfiguration)?;
    let metadata =
        std::fs::symlink_metadata(&root).map_err(|_| SnapshotError::InvalidConfiguration)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(SnapshotError::InvalidConfiguration);
    }
    validate_private_permissions(&metadata)?;
    let workspace = workspace
        .canonicalize()
        .map_err(|_| SnapshotError::InvalidConfiguration)?;
    if root.starts_with(&workspace) || workspace.starts_with(&root) {
        return Err(SnapshotError::InvalidConfiguration);
    }
    Ok(root)
}

fn reject_symlink_components(path: &Path) -> Result<(), SnapshotError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => current.push(component.as_os_str()),
            Component::CurDir => continue,
            Component::ParentDir => return Err(SnapshotError::InvalidConfiguration),
            Component::Normal(part) => current.push(part),
        }
        let metadata =
            std::fs::symlink_metadata(&current).map_err(|_| SnapshotError::InvalidConfiguration)?;
        if metadata.file_type().is_symlink() {
            return Err(SnapshotError::InvalidConfiguration);
        }
    }
    Ok(())
}

#[cfg(unix)]
fn validate_private_permissions(metadata: &std::fs::Metadata) -> Result<(), SnapshotError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    if metadata.uid() != rustix::process::getuid().as_raw()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(SnapshotError::InvalidConfiguration);
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_permissions(metadata: &std::fs::Metadata) -> Result<(), SnapshotError> {
    if metadata.permissions().readonly() {
        return Err(SnapshotError::InvalidConfiguration);
    }
    Ok(())
}

async fn create_private_directory(path: &Path) -> Result<(), SnapshotError> {
    match fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            validate_private_permissions(&metadata)?;
        }
        Ok(_) => return Err(SnapshotError::InvalidConfiguration),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path)
                .await
                .map_err(|_| SnapshotError::InvalidConfiguration)?;
            #[cfg(unix)]
            fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o700))
                .await
                .map_err(|_| SnapshotError::InvalidConfiguration)?;
        }
        Err(_) => return Err(SnapshotError::InvalidConfiguration),
    }
    Ok(())
}

fn configured_exclusions(
    workspace: &Path,
    paths: &[PathBuf],
) -> Result<Vec<String>, SnapshotError> {
    let mut exclusions = DEFAULT_EXCLUSIONS
        .iter()
        .map(|path| (*path).to_owned())
        .collect::<BTreeSet<_>>();
    for path in paths {
        exclusions.insert(configured_exclusion(workspace, path)?);
    }
    if exclusions.len() > MAX_EXCLUSIONS {
        return Err(SnapshotError::InvalidConfiguration);
    }
    Ok(exclusions.into_iter().collect())
}

fn configured_exclusion(workspace: &Path, requested: &Path) -> Result<String, SnapshotError> {
    if requested.as_os_str().is_empty()
        || requested
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(SnapshotError::InvalidConfiguration);
    }
    let mut ancestor = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        workspace.join(requested)
    };
    let mut suffix = Vec::new();
    loop {
        match std::fs::symlink_metadata(&ancestor) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let component = ancestor
                    .file_name()
                    .ok_or(SnapshotError::InvalidConfiguration)?
                    .to_owned();
                suffix.push(component);
                if !ancestor.pop() {
                    return Err(SnapshotError::InvalidConfiguration);
                }
            }
            Err(_) => return Err(SnapshotError::InvalidConfiguration),
        }
    }
    let canonical = ancestor
        .canonicalize()
        .map_err(|_| SnapshotError::InvalidConfiguration)?;
    let mut relative = canonical
        .strip_prefix(workspace)
        .map_err(|_| SnapshotError::InvalidConfiguration)?
        .to_path_buf();
    for component in suffix.into_iter().rev() {
        relative.push(component);
    }
    let relative = path_to_posix(&relative)?;
    if relative == "." {
        return Err(SnapshotError::InvalidConfiguration);
    }
    WorkspacePath::new(relative.clone()).map_err(|_| SnapshotError::InvalidConfiguration)?;
    Ok(relative)
}

async fn immutable_write(path: &Path, bytes: &[u8]) -> Result<(), SnapshotError> {
    let maximum = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if let Ok(existing) = read_private_file(path, maximum).await {
        return if existing == bytes {
            Ok(())
        } else {
            Err(SnapshotError::IntegrityFailure)
        };
    }
    let parent = path.parent().ok_or(SnapshotError::OperationFailed)?;
    let temporary = parent.join(format!(".{}.tmp", Uuid::new_v4()));
    write_private_file(&temporary, bytes).await?;
    match fs::hard_link(&temporary, path).await {
        Ok(()) => {
            fs::remove_file(&temporary)
                .await
                .map_err(|_| SnapshotError::OperationFailed)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = fs::remove_file(&temporary).await;
            if read_private_file(path, maximum).await? != bytes {
                return Err(SnapshotError::IntegrityFailure);
            }
        }
        Err(_) => {
            let _ = fs::remove_file(&temporary).await;
            return Err(SnapshotError::OperationFailed);
        }
    }
    sync_directory(parent).await
}

async fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), SnapshotError> {
    let parent = path.parent().ok_or(SnapshotError::OperationFailed)?;
    let temporary = parent.join(format!(".{}.tmp", Uuid::new_v4()));
    write_private_file(&temporary, bytes).await?;
    fs::rename(&temporary, path)
        .await
        .map_err(|_| SnapshotError::OperationFailed)?;
    sync_directory(parent).await
}

async fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), SnapshotError> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(path)
        .await
        .map_err(|_| SnapshotError::OperationFailed)?;
    file.write_all(bytes)
        .await
        .map_err(|_| SnapshotError::OperationFailed)?;
    file.sync_all()
        .await
        .map_err(|_| SnapshotError::OperationFailed)
}

async fn read_private_file(path: &Path, maximum: u64) -> Result<Vec<u8>, SnapshotError> {
    let metadata = match fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(SnapshotError::NotFound);
        }
        Err(_) => return Err(SnapshotError::OperationFailed),
    };
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > maximum
    {
        return Err(SnapshotError::IntegrityFailure);
    }
    validate_private_permissions(&metadata).map_err(|_| SnapshotError::IntegrityFailure)?;
    fs::read(path)
        .await
        .map_err(|_| SnapshotError::OperationFailed)
}

async fn private_file_size(path: &Path) -> Result<u64, SnapshotError> {
    let metadata = match fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(_) => return Err(SnapshotError::OperationFailed),
    };
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(SnapshotError::IntegrityFailure);
    }
    validate_private_permissions(&metadata).map_err(|_| SnapshotError::IntegrityFailure)?;
    Ok(metadata.len())
}

#[cfg(unix)]
async fn sync_directory(path: &Path) -> Result<(), SnapshotError> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || std::fs::File::open(path)?.sync_all())
        .await
        .map_err(|_| SnapshotError::OperationFailed)?
        .map_err(|_| SnapshotError::OperationFailed)
}

#[cfg(not(unix))]
async fn sync_directory(_path: &Path) -> Result<(), SnapshotError> {
    Ok(())
}

async fn directory_count(path: &Path, ceiling: usize) -> Result<usize, SnapshotError> {
    let mut count = 0_usize;
    let mut reader = fs::read_dir(path)
        .await
        .map_err(|_| SnapshotError::UnhealthyStorage)?;
    while reader
        .next_entry()
        .await
        .map_err(|_| SnapshotError::UnhealthyStorage)?
        .is_some()
    {
        count += 1;
        if count > ceiling {
            return Err(SnapshotError::QuotaExceeded);
        }
    }
    Ok(count)
}

fn validate_stored_file(file: &StoredFile) -> Result<(), SnapshotError> {
    WorkspacePath::new(file.path.clone()).map_err(|_| SnapshotError::IntegrityFailure)?;
    let resource_id = root_relative_resource_id(RootResourceKind::Path, &file.path)
        .map_err(|_| SnapshotError::IntegrityFailure)?;
    if file.resource_id != resource_id.as_str() {
        return Err(SnapshotError::IntegrityFailure);
    }
    for value in [&file.identity, &file.revision, &file.digest] {
        Revision::new(value.clone()).map_err(|_| SnapshotError::IntegrityFailure)?;
    }
    if file.size_bytes > MAX_SNAPSHOT_FILE_BYTES
        || !file
            .digest
            .strip_prefix("sha256:")
            .is_some_and(valid_hex_digest)
    {
        return Err(SnapshotError::IntegrityFailure);
    }
    Ok(())
}

fn validate_journal(journal: &StoredJournal) -> Result<(), SnapshotError> {
    if journal.version != JOURNAL_VERSION
        || journal.changes.len() > MAX_SNAPSHOT_FILES
        || journal.applied_files > journal.changes.len()
        || journal.applied_directories > journal.created_directories.len()
        || journal
            .changes
            .len()
            .saturating_add(journal.created_directories.len())
            .saturating_add(2)
            > MAX_RESOURCE_INTENTS
    {
        return Err(SnapshotError::UnhealthyStorage);
    }
    Identifier::new(journal.restore_id.clone()).map_err(|_| SnapshotError::UnhealthyStorage)?;
    validate_snapshot_id(&journal.target_snapshot_id)?;
    validate_snapshot_id(&journal.pre_restore_snapshot_id)?;
    for change in &journal.changes {
        WorkspacePath::new(change.path.clone()).map_err(|_| SnapshotError::UnhealthyStorage)?;
        let resource_id = root_relative_resource_id(RootResourceKind::Path, &change.path)
            .map_err(|_| SnapshotError::UnhealthyStorage)?;
        if change.resource_id != resource_id.as_str() {
            return Err(SnapshotError::UnhealthyStorage);
        }
    }
    for directory in &journal.created_directories {
        WorkspacePath::new(directory.clone()).map_err(|_| SnapshotError::UnhealthyStorage)?;
    }
    Ok(())
}

fn serialize_journal(journal: &StoredJournal) -> Result<Vec<u8>, SnapshotError> {
    let mut bytes = serde_json::to_vec(journal).map_err(|_| SnapshotError::OperationFailed)?;
    let mut maximum = journal.clone();
    maximum.state = StoredRestoreState::Indeterminate;
    maximum.applied_files = maximum.changes.len();
    maximum.applied_directories = maximum.created_directories.len();
    maximum.reconciliation_required = false;
    maximum.acknowledgement_required = false;
    let maximum_bytes = serde_json::to_vec(&maximum)
        .map_err(|_| SnapshotError::OperationFailed)?
        .len();
    bytes.resize(maximum_bytes.max(bytes.len()), b' ');
    Ok(bytes)
}

fn validate_snapshot_id(value: &str) -> Result<(), SnapshotError> {
    value
        .strip_prefix("snap_")
        .filter(|hex| valid_hex_digest(hex))
        .ok_or(SnapshotError::IntegrityFailure)
        .map(|_| ())
}

fn valid_hex_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn parse_cursor(
    cursor: Option<&Cursor>,
    snapshot_id: &str,
    length: usize,
) -> Result<usize, SnapshotError> {
    let Some(cursor) = cursor else { return Ok(0) };
    let prefix = format!("{snapshot_id}:");
    let offset = cursor
        .as_str()
        .strip_prefix(&prefix)
        .ok_or(SnapshotError::InvalidRequest)?
        .parse::<usize>()
        .map_err(|_| SnapshotError::InvalidRequest)?;
    if offset == 0 || offset >= length {
        return Err(SnapshotError::InvalidRequest);
    }
    Ok(offset)
}

fn snapshot_id(content: &ManifestContent) -> Result<String, SnapshotError> {
    Ok(format!(
        "snap_{}",
        hex_sha256(&serde_json::to_vec(content).map_err(|_| SnapshotError::OperationFailed)?)
    ))
}

fn manifest_revision(content: &ManifestContent) -> Result<String, SnapshotError> {
    digest_serializable(content)
}

fn digest_serializable(value: &impl Serialize) -> Result<String, SnapshotError> {
    let bytes = serde_json::to_vec(value).map_err(|_| SnapshotError::OperationFailed)?;
    Ok(digest_bytes(&bytes))
}

fn digest_bytes(bytes: &[u8]) -> String {
    format!("sha256:{}", hex_sha256(bytes))
}

fn format_sha256(digest: impl IntoIterator<Item = u8>) -> String {
    let mut output = String::with_capacity(71);
    output.push_str("sha256:");
    for byte in digest {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn relative_path(root: &Path, path: &Path) -> Result<String, SnapshotError> {
    path.strip_prefix(root)
        .map_err(|_| SnapshotError::UnsupportedFile)
        .and_then(path_to_posix)
}

fn path_to_posix(path: &Path) -> Result<String, SnapshotError> {
    let parts = path
        .components()
        .map(|component| match component {
            Component::Normal(part) => part.to_str().ok_or(SnapshotError::UnsupportedFile),
            _ => Err(SnapshotError::UnsupportedFile),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if parts.is_empty() {
        Ok(".".to_owned())
    } else {
        Ok(parts.join("/"))
    }
}

#[cfg(unix)]
fn file_identity(metadata: &std::fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;

    format!("file:{}:{}", metadata.dev(), metadata.ino())
}

#[cfg(not(unix))]
fn file_identity(metadata: &std::fs::Metadata) -> String {
    format!("file:{}:{}", metadata.len(), modified_nanos(metadata))
}

#[cfg(unix)]
fn file_mode(metadata: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;

    metadata.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn file_mode(metadata: &std::fs::Metadata) -> u32 {
    u32::from(metadata.permissions().readonly())
}

#[cfg(unix)]
fn metadata_stamp(metadata: &std::fs::Metadata) -> (u64, i64, i64, u32) {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    (
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.permissions().mode() & 0o777,
    )
}

#[cfg(not(unix))]
fn metadata_stamp(metadata: &std::fs::Metadata) -> (u64, u128, bool) {
    (
        metadata.len(),
        modified_nanos(metadata),
        metadata.permissions().readonly(),
    )
}

#[cfg(not(unix))]
fn modified_nanos(metadata: &std::fs::Metadata) -> u128 {
    metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_nanos())
}

fn identifier(value: &str) -> Result<Identifier, SnapshotError> {
    Identifier::new(value.to_owned()).map_err(|_| SnapshotError::IntegrityFailure)
}

fn identifiers(values: &[String]) -> Result<Vec<Identifier>, SnapshotError> {
    values.iter().map(|value| identifier(value)).collect()
}

fn revision(value: &str) -> Result<Revision, SnapshotError> {
    Revision::new(value.to_owned()).map_err(|_| SnapshotError::IntegrityFailure)
}

fn check_cancelled(token: &CancellationToken) -> Result<(), SnapshotError> {
    if token.is_cancelled() {
        Err(SnapshotError::Cancelled)
    } else {
        Ok(())
    }
}

fn map_workspace(error: WorkspaceError) -> SnapshotError {
    match error {
        WorkspaceError::StaleResource | WorkspaceError::StaleCwd => SnapshotError::Conflict,
        WorkspaceError::Filesystem(_) => SnapshotError::Conflict,
        _ => SnapshotError::OperationFailed,
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::fs as std_fs;

    use tempfile::TempDir;
    use workcell_mcp_files::FileToolGroup;

    use super::*;

    async fn fixture() -> (TempDir, TempDir, SnapshotManager) {
        let workspace = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        std_fs::set_permissions(
            storage.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let files = FileToolGroup::new(workspace.path(), true, None)
            .await
            .unwrap();
        let manager = SnapshotManager::open(files.workspace_snapshot_access(), storage.path(), &[])
            .await
            .unwrap();
        (workspace, storage, manager)
    }

    #[tokio::test]
    async fn bound_snapshot_stores_are_isolated_by_workspace_binding() {
        let workspace = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        std_fs::set_permissions(
            storage.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        std_fs::write(workspace.path().join("file.txt"), "content").unwrap();
        let files = FileToolGroup::new(workspace.path(), true, None)
            .await
            .unwrap();
        let first_binding = Identifier::new("workspace_generation_a").unwrap();
        let first = SnapshotManager::open_bound(
            files.workspace_snapshot_access(),
            storage.path(),
            &[],
            &first_binding,
        )
        .await
        .unwrap();
        let captured = first
            .capture(
                &Identifier::new("checkpoint").unwrap(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let second = SnapshotManager::open_bound(
            files.workspace_snapshot_access(),
            storage.path(),
            &[],
            &Identifier::new("workspace_generation_b").unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(
            second
                .inspect(&captured.snapshot.snapshot_id, 1, None)
                .await
                .unwrap_err(),
            SnapshotError::NotFound
        );
        assert!(storage.path().join(first_binding.as_str()).is_dir());
    }

    #[tokio::test]
    async fn checkpoint_capture_is_idempotent_and_content_addressed() {
        let (workspace, _storage, manager) = fixture().await;
        std_fs::write(workspace.path().join("file.txt"), "one").unwrap();
        let checkpoint = Identifier::new("checkpoint-one").unwrap();
        let first = manager
            .capture(&checkpoint, &CancellationToken::new())
            .await
            .unwrap();
        let inspected = manager
            .inspect(&first.snapshot.snapshot_id, 1, None)
            .await
            .unwrap();
        assert_eq!(
            inspected.files[0].resource_id,
            root_relative_resource_id(RootResourceKind::Path, "file.txt").unwrap()
        );
        std_fs::write(workspace.path().join("file.txt"), "two").unwrap();
        let second = manager
            .capture(&checkpoint, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(first.snapshot.snapshot_id, second.snapshot.snapshot_id);
        assert_eq!(
            first.snapshot.created_at_unix_ms,
            second.snapshot.created_at_unix_ms
        );
        assert!(second.reused_checkpoint);
    }

    #[tokio::test]
    async fn concurrent_checkpoint_captures_wait_and_reuse() {
        let (_workspace, _storage, manager) = fixture().await;
        let checkpoint = Identifier::new("concurrent-checkpoint").unwrap();
        let token = CancellationToken::new();
        let publication = manager.inner.publication.lock().await;
        let first = manager.capture(&checkpoint, &token);
        let second = manager.capture(&checkpoint, &token);
        tokio::pin!(first, second);
        std::future::poll_fn(|cx| {
            assert!(first.as_mut().poll(cx).is_pending());
            assert!(second.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        drop(publication);
        let (first, second) = tokio::join!(first, second);
        let first = first.unwrap();
        let second = second.unwrap();
        assert!(!first.reused_checkpoint);
        assert!(second.reused_checkpoint);
        assert_eq!(first.snapshot.snapshot_id, second.snapshot.snapshot_id);
    }

    #[tokio::test(start_paused = true)]
    async fn capture_admission_is_cancellable_and_bounded_at_every_lock() {
        for held_lock in ["capture", "publication", "workspace"] {
            for cancel in [true, false] {
                let (_workspace, _storage, manager) = fixture().await;
                let checkpoint = Identifier::new("waiting-checkpoint").unwrap();
                let token = CancellationToken::new();
                let capture = if held_lock == "capture" {
                    Some(manager.inner.capture.lock().await)
                } else {
                    None
                };
                let publication = if held_lock == "publication" {
                    Some(manager.inner.publication.lock().await)
                } else {
                    None
                };
                let workspace = if held_lock == "workspace" {
                    Some(manager.inner.workspace.capture_guard().await)
                } else {
                    None
                };
                let pending = manager.capture(&checkpoint, &token);
                tokio::pin!(pending);
                std::future::poll_fn(|cx| {
                    assert!(pending.as_mut().poll(cx).is_pending());
                    std::task::Poll::Ready(())
                })
                .await;
                let expected = if cancel {
                    token.cancel();
                    SnapshotError::Cancelled
                } else {
                    tokio::time::advance(CAPTURE_ADMISSION_TIMEOUT).await;
                    SnapshotError::Busy
                };
                assert_eq!(pending.await.unwrap_err(), expected);
                assert!(
                    manager
                        .load_checkpoint(checkpoint.as_str())
                        .await
                        .unwrap()
                        .is_none()
                );
                drop((capture, publication, workspace));
                assert!(manager.inner.capture.try_lock().is_ok());
                assert!(manager.inner.publication.try_lock().is_ok());
                manager
                    .capture(&checkpoint, &CancellationToken::new())
                    .await
                    .unwrap();
            }
        }
    }

    #[tokio::test]
    async fn equal_workspace_content_has_one_snapshot_identity_across_checkpoints() {
        let (workspace, _storage, manager) = fixture().await;
        std_fs::write(workspace.path().join("file.txt"), "one").unwrap();
        let first = manager
            .capture(
                &Identifier::new("checkpoint-one").unwrap(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let second = manager
            .capture(
                &Identifier::new("checkpoint-two").unwrap(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(first.snapshot.snapshot_id, second.snapshot.snapshot_id);
        assert_eq!(
            first.snapshot.created_at_unix_ms,
            second.snapshot.created_at_unix_ms
        );
    }

    #[tokio::test]
    async fn workspace_file_count_is_bounded() {
        let (workspace, _storage, manager) = fixture().await;
        for index in 0..=MAX_SNAPSHOT_FILES {
            std_fs::write(workspace.path().join(format!("file-{index}")), "").unwrap();
        }
        assert_eq!(
            manager
                .capture(
                    &Identifier::new("too-many").unwrap(),
                    &CancellationToken::new(),
                )
                .await
                .unwrap_err(),
            SnapshotError::LimitExceeded
        );
    }

    #[tokio::test]
    async fn tampered_blobs_and_manifests_are_rejected() {
        let (workspace, storage, manager) = fixture().await;
        std_fs::write(workspace.path().join("file.txt"), "one").unwrap();
        let capture = manager
            .capture(
                &Identifier::new("checkpoint").unwrap(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let manifest = manager
            .load_manifest(capture.snapshot.snapshot_id.as_str(), false)
            .await
            .unwrap();
        let blob = manager
            .blob_path(&manifest.content.files[0].digest)
            .unwrap();
        std_fs::write(blob, "tampered").unwrap();
        assert_eq!(
            manager
                .inspect(&capture.snapshot.snapshot_id, 10, None)
                .await
                .unwrap_err(),
            SnapshotError::IntegrityFailure
        );
        let manifest_path = storage
            .path()
            .join("manifests")
            .join(format!("{}.json", capture.snapshot.snapshot_id.as_str()));
        std_fs::write(manifest_path, "{}").unwrap();
        assert_eq!(
            manager
                .inspect(&capture.snapshot.snapshot_id, 10, None)
                .await
                .unwrap_err(),
            SnapshotError::IntegrityFailure
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinks_and_special_files_are_rejected_without_following_them() {
        use std::os::unix::{fs::symlink, net::UnixListener};

        let (workspace, _storage, manager) = fixture().await;
        let outside = tempfile::tempdir().unwrap();
        std_fs::write(outside.path().join("secret"), "secret").unwrap();
        symlink(outside.path().join("secret"), workspace.path().join("link")).unwrap();
        assert_eq!(
            manager
                .capture(
                    &Identifier::new("symlink").unwrap(),
                    &CancellationToken::new(),
                )
                .await
                .unwrap_err(),
            SnapshotError::UnsupportedFile
        );
        std_fs::remove_file(workspace.path().join("link")).unwrap();
        let _socket = UnixListener::bind(workspace.path().join("socket")).unwrap();
        assert_eq!(
            manager
                .capture(
                    &Identifier::new("socket").unwrap(),
                    &CancellationToken::new(),
                )
                .await
                .unwrap_err(),
            SnapshotError::UnsupportedFile
        );
    }

    #[tokio::test]
    async fn stale_restore_never_overwrites_later_edits() {
        let (workspace, _storage, manager) = fixture().await;
        let path = workspace.path().join("file.txt");
        std_fs::write(&path, "before").unwrap();
        let snapshot = manager
            .capture(
                &Identifier::new("before").unwrap(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        std_fs::write(&path, "after").unwrap();
        let (prepared, preview) = manager
            .prepare_restore(&snapshot.snapshot.snapshot_id, &CancellationToken::new())
            .await
            .unwrap();
        assert!(prepared.retained_bytes() >= serde_json::to_vec(&preview).unwrap().len());
        std_fs::write(&path, "later").unwrap();
        assert_eq!(
            manager
                .execute_restore(&prepared, &CancellationToken::new())
                .await
                .unwrap_err(),
            SnapshotError::Conflict
        );
        assert_eq!(std_fs::read_to_string(path).unwrap(), "later");
    }

    #[tokio::test]
    async fn preparation_retains_pre_restore_identity_before_a_pre_journal_failure() {
        let (workspace, storage, manager) = fixture().await;
        let path = workspace.path().join("file.txt");
        std_fs::write(&path, "before").unwrap();
        let target = manager
            .capture(
                &Identifier::new("pre-journal-target").unwrap(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        std_fs::write(&path, "after").unwrap();
        let (prepared, _) = manager
            .prepare_restore(&target.snapshot.snapshot_id, &CancellationToken::new())
            .await
            .unwrap();
        let expected_pre_restore_id = prepared.pre_restore_snapshot_id().to_owned();
        assert_ne!(
            expected_pre_restore_id,
            target.snapshot.snapshot_id.as_str()
        );

        manager
            .inner
            .fail_before_journal
            .store(true, Ordering::SeqCst);
        assert_eq!(
            manager
                .execute_restore(&prepared, &CancellationToken::new())
                .await
                .unwrap_err(),
            SnapshotError::OperationFailed
        );
        assert!(lock(&manager.inner.state).journals.is_empty());
        assert!(
            storage
                .path()
                .join("manifests")
                .join(format!("{expected_pre_restore_id}.json"))
                .is_file()
        );
        let manifest = manager
            .load_manifest(&expected_pre_restore_id, true)
            .await
            .unwrap();
        assert_eq!(manifest.snapshot_id, expected_pre_restore_id);
    }

    #[tokio::test]
    async fn cancelled_restore_publishes_no_workspace_change() {
        let (workspace, _storage, manager) = fixture().await;
        let path = workspace.path().join("file.txt");
        std_fs::write(&path, "before").unwrap();
        let snapshot = manager
            .capture(
                &Identifier::new("cancel-restore-before").unwrap(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        std_fs::write(&path, "after").unwrap();
        let (prepared, _) = manager
            .prepare_restore(&snapshot.snapshot.snapshot_id, &CancellationToken::new())
            .await
            .unwrap();
        let token = CancellationToken::new();
        token.cancel();
        assert_eq!(
            manager
                .execute_restore(&prepared, &token)
                .await
                .unwrap_err(),
            SnapshotError::Cancelled
        );
        assert_eq!(std_fs::read_to_string(path).unwrap(), "after");
        assert!(lock(&manager.inner.state).journals.is_empty());
    }

    #[tokio::test]
    async fn restore_unrevert_acknowledgement_and_gc_preserve_reachable_state() {
        let (workspace, _storage, manager) = fixture().await;
        let path = workspace.path().join("file.txt");
        std_fs::write(&path, "before").unwrap();
        let snapshot = manager
            .capture(
                &Identifier::new("before").unwrap(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        std_fs::write(&path, "after").unwrap();
        let (prepared, _) = manager
            .prepare_restore(&snapshot.snapshot.snapshot_id, &CancellationToken::new())
            .await
            .unwrap();
        let restored = manager
            .execute_restore(&prepared, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(std_fs::read_to_string(&path).unwrap(), "before");
        assert_eq!(
            manager
                .prepare_restore(&snapshot.snapshot.snapshot_id, &CancellationToken::new())
                .await
                .err()
                .unwrap(),
            SnapshotError::AcknowledgementRequired
        );
        let (protected_cleanup, protected_preview) = manager
            .prepare_cleanup(std::slice::from_ref(&snapshot.snapshot.snapshot_id))
            .await
            .unwrap();
        assert!(protected_preview.snapshot_ids.is_empty());
        assert_eq!(
            protected_preview.retained_snapshot_ids.as_slice(),
            std::slice::from_ref(&snapshot.snapshot.snapshot_id)
        );
        let protected_result = manager
            .execute_cleanup(&protected_cleanup, &CancellationToken::new())
            .await
            .unwrap();
        assert!(protected_result.deleted_snapshot_ids.is_empty());
        let (unrevert, _) = manager
            .prepare_unrevert(&restored.restore_id, &CancellationToken::new())
            .await
            .unwrap();
        let unreverted = manager
            .execute_restore(&unrevert, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(std_fs::read_to_string(&path).unwrap(), "after");
        manager.acknowledge(&unreverted.restore_id).await.unwrap();

        let (cleanup, preview) = manager
            .prepare_cleanup(std::slice::from_ref(&snapshot.snapshot.snapshot_id))
            .await
            .unwrap();
        assert!(cleanup.retained_bytes() >= serde_json::to_vec(&preview).unwrap().len());
        assert_eq!(
            preview.snapshot_ids.as_slice(),
            std::slice::from_ref(&snapshot.snapshot.snapshot_id)
        );
        let result = manager
            .execute_cleanup(&cleanup, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(result.deleted_snapshot_ids, [snapshot.snapshot.snapshot_id]);
    }

    #[tokio::test]
    async fn cancelled_capture_publishes_no_manifest() {
        let (workspace, _storage, manager) = fixture().await;
        std_fs::write(workspace.path().join("file.txt"), "content").unwrap();
        let token = CancellationToken::new();
        token.cancel();
        assert_eq!(
            manager
                .capture(&Identifier::new("cancelled").unwrap(), &token)
                .await
                .unwrap_err(),
            SnapshotError::Cancelled
        );
        assert_eq!(manager.manifest_count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn capture_bounds_a_wide_directory_before_retaining_every_entry() {
        let (workspace, _storage, manager) = fixture().await;
        for name in ["one", "two", "three"] {
            std_fs::create_dir(workspace.path().join(name)).unwrap();
        }

        assert_eq!(
            manager
                .scan_workspace_bounded(false, 2, usize::MAX, &CancellationToken::new())
                .await
                .unwrap_err(),
            SnapshotError::LimitExceeded
        );
    }

    #[tokio::test]
    async fn capture_charges_aggregate_paths_for_empty_directory_trees() {
        let (workspace, _storage, manager) = fixture().await;
        let first = workspace.path().join("one");
        let second = first.join("two");
        std_fs::create_dir_all(&second).unwrap();
        let retained_path_bytes = first
            .as_os_str()
            .as_encoded_bytes()
            .len()
            .saturating_add(second.as_os_str().as_encoded_bytes().len());

        assert_eq!(
            manager
                .scan_workspace_bounded(
                    false,
                    usize::MAX,
                    retained_path_bytes - 1,
                    &CancellationToken::new(),
                )
                .await
                .unwrap_err(),
            SnapshotError::LimitExceeded
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn private_storage_rejects_overlap_symlinks_and_shared_permissions() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let workspace = tempfile::tempdir().unwrap();
        let files = FileToolGroup::new(workspace.path(), true, None)
            .await
            .unwrap();
        let inside = workspace.path().join("snapshots");
        std_fs::create_dir(&inside).unwrap();
        std_fs::set_permissions(&inside, std_fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            SnapshotManager::open(files.workspace_snapshot_access(), &inside, &[])
                .await
                .err()
                .unwrap(),
            SnapshotError::InvalidConfiguration
        );

        let storage = tempfile::tempdir().unwrap();
        std_fs::set_permissions(storage.path(), std_fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            SnapshotManager::open(files.workspace_snapshot_access(), storage.path(), &[])
                .await
                .err()
                .unwrap(),
            SnapshotError::InvalidConfiguration
        );

        let parent = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        std_fs::set_permissions(target.path(), std_fs::Permissions::from_mode(0o700)).unwrap();
        let link = parent.path().join("linked-storage");
        symlink(target.path(), &link).unwrap();
        assert_eq!(
            SnapshotManager::open(files.workspace_snapshot_access(), link, &[])
                .await
                .err()
                .unwrap(),
            SnapshotError::InvalidConfiguration
        );
    }

    #[tokio::test]
    async fn startup_removes_abandoned_atomic_metadata_temps() {
        let (workspace, storage, manager) = fixture().await;
        drop(manager);
        for directory in ["blobs", "manifests", "checkpoints", "journals"] {
            let path = storage.path().join(directory).join(".abandoned.tmp");
            std_fs::write(&path, "partial").unwrap();
            #[cfg(unix)]
            std_fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
                .unwrap();
        }
        let files = FileToolGroup::new(workspace.path(), true, None)
            .await
            .unwrap();
        SnapshotManager::open(files.workspace_snapshot_access(), storage.path(), &[])
            .await
            .unwrap();
        for directory in ["blobs", "manifests", "checkpoints", "journals"] {
            assert!(
                !storage
                    .path()
                    .join(directory)
                    .join(".abandoned.tmp")
                    .exists()
            );
        }
    }

    #[tokio::test]
    async fn startup_recovery_marks_interrupted_publication_for_reconciliation() {
        let (workspace, storage, manager) = fixture().await;
        std_fs::write(workspace.path().join("a"), "one").unwrap();
        let before = manager
            .capture(
                &Identifier::new("before").unwrap(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        std_fs::write(workspace.path().join("a"), "two").unwrap();
        let (prepared, _) = manager
            .prepare_restore(&before.snapshot.snapshot_id, &CancellationToken::new())
            .await
            .unwrap();
        let pre = manager
            .capture_manifest(&CancellationToken::new())
            .await
            .unwrap();
        manager.persist_manifest(&pre).await.unwrap();
        let journal = StoredJournal {
            version: JOURNAL_VERSION.to_owned(),
            restore_id: "restore_interrupted".to_owned(),
            state: StoredRestoreState::Publishing,
            target_snapshot_id: prepared.target.snapshot_id.clone(),
            pre_restore_snapshot_id: pre.snapshot_id,
            changes: prepared.changes.clone(),
            created_directories: prepared.created_directories.clone(),
            applied_files: 0,
            applied_directories: 0,
            reconciliation_required: false,
            acknowledgement_required: true,
            unrevert_of: None,
        };
        manager.persist_journal(&journal).await.unwrap();
        drop(prepared);
        drop(manager);
        let files = FileToolGroup::new(workspace.path(), true, None)
            .await
            .unwrap();
        let recovered =
            SnapshotManager::open(files.workspace_snapshot_access(), storage.path(), &[])
                .await
                .unwrap();
        let status = recovered
            .status(&Identifier::new("restore_interrupted").unwrap())
            .unwrap();
        assert_eq!(status.restore.state, SnapshotRestoreState::Indeterminate);
        assert!(status.restore.reconciliation_required);
    }

    #[tokio::test]
    async fn recovery_detects_a_failure_between_file_and_journal_publication() {
        let (workspace, storage, manager) = fixture().await;
        for name in ["a", "b"] {
            std_fs::write(workspace.path().join(name), "before").unwrap();
        }
        let before = manager
            .capture(
                &Identifier::new("before-two-files").unwrap(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        for name in ["a", "b"] {
            std_fs::write(workspace.path().join(name), "after").unwrap();
        }
        let (prepared, preview) = manager
            .prepare_restore(&before.snapshot.snapshot_id, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(prepared.restore_id(), preview.restore_id.as_str());
        let restore_id = preview.restore_id.clone();
        manager.inner.fail_after_file.store(0, Ordering::SeqCst);
        assert_eq!(
            manager
                .execute_restore(&prepared, &CancellationToken::new())
                .await
                .unwrap_err(),
            SnapshotError::OperationFailed
        );
        let live_status = manager.status(&restore_id).unwrap();
        assert_eq!(
            live_status.restore.state,
            SnapshotRestoreState::Indeterminate
        );
        assert!(live_status.restore.reconciliation_required);
        drop(prepared);
        drop(manager);
        let files = FileToolGroup::new(workspace.path(), true, None)
            .await
            .unwrap();
        let recovered =
            SnapshotManager::open(files.workspace_snapshot_access(), storage.path(), &[])
                .await
                .unwrap();
        let status = recovered.status(&restore_id).unwrap();
        assert_eq!(status.restore.state, SnapshotRestoreState::Partial);
        assert_eq!(status.restore.applied_files, 1);
        assert!(status.restore.reconciliation_required);
        assert_eq!(
            std_fs::read_to_string(workspace.path().join("a")).unwrap(),
            "before"
        );
        assert_eq!(
            std_fs::read_to_string(workspace.path().join("b")).unwrap(),
            "after"
        );
    }

    #[tokio::test]
    async fn failed_capture_reclaims_blobs_published_before_the_quota_failure() {
        let (workspace, storage, manager) = fixture().await;
        let content = b"captured before manifest publication";
        std_fs::write(workspace.path().join("file.txt"), content).unwrap();
        let usage = manager.storage_usage().await.unwrap();
        let filler = storage.path().join("journals").join("quota-filler");
        let file = std_fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&filler)
            .unwrap();
        file.set_len(
            MAX_SNAPSHOT_STORAGE_BYTES
                .checked_sub(usage)
                .and_then(|remaining| remaining.checked_sub(content.len() as u64))
                .unwrap(),
        )
        .unwrap();
        #[cfg(unix)]
        std_fs::set_permissions(&filler, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .unwrap();
        assert_eq!(
            manager
                .capture(
                    &Identifier::new("quota-failure").unwrap(),
                    &CancellationToken::new(),
                )
                .await
                .unwrap_err(),
            SnapshotError::QuotaExceeded
        );
        assert_eq!(
            std_fs::read_dir(storage.path().join("blobs"))
                .unwrap()
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn startup_reclaims_an_over_quota_orphan_store_without_manifest_ids() {
        let (workspace, storage, manager) = fixture().await;
        drop(manager);
        let orphan = storage.path().join("blobs").join("0".repeat(64));
        let file = std_fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&orphan)
            .unwrap();
        file.set_len(MAX_SNAPSHOT_STORAGE_BYTES + 1).unwrap();
        #[cfg(unix)]
        std_fs::set_permissions(&orphan, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .unwrap();
        let files = FileToolGroup::new(workspace.path(), true, None)
            .await
            .unwrap();
        SnapshotManager::open(files.workspace_snapshot_access(), storage.path(), &[])
            .await
            .unwrap();
        assert!(!orphan.exists());
    }

    #[tokio::test]
    async fn cleanup_without_snapshot_ids_reclaims_only_prepared_orphans() {
        let (_workspace, storage, manager) = fixture().await;
        let bytes = b"orphan";
        let digest = digest_bytes(bytes);
        manager.persist_blob(&digest, bytes).await.unwrap();
        let (prepared, preview) = manager.prepare_cleanup(&[]).await.unwrap();
        assert!(preview.snapshot_ids.is_empty());
        assert_eq!(prepared.plan.blobs, [digest]);
        manager
            .execute_cleanup(&prepared, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            std_fs::read_dir(storage.path().join("blobs"))
                .unwrap()
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn cleanup_revalidates_the_complete_prepared_deletion_set() {
        let (workspace, storage, manager) = fixture().await;
        std_fs::write(workspace.path().join("file.txt"), "content").unwrap();
        let snapshot = manager
            .capture(
                &Identifier::new("first-checkpoint").unwrap(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let (prepared, _) = manager
            .prepare_cleanup(std::slice::from_ref(&snapshot.snapshot.snapshot_id))
            .await
            .unwrap();
        manager
            .capture(
                &Identifier::new("later-checkpoint").unwrap(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            manager
                .execute_cleanup(&prepared, &CancellationToken::new())
                .await
                .unwrap_err(),
            SnapshotError::Conflict
        );
        assert!(
            storage
                .path()
                .join("manifests")
                .join(format!("{}.json", snapshot.snapshot.snapshot_id.as_str()))
                .exists()
        );
    }

    #[tokio::test]
    async fn every_cleanup_crash_phase_reopens_to_a_reachable_store() {
        for phase in 0..=2 {
            let (workspace, storage, manager) = fixture().await;
            std_fs::write(workspace.path().join("file.txt"), "content").unwrap();
            let snapshot = manager
                .capture(
                    &Identifier::new(format!("cleanup-crash-{phase}")).unwrap(),
                    &CancellationToken::new(),
                )
                .await
                .unwrap();
            let (prepared, _) = manager
                .prepare_cleanup(std::slice::from_ref(&snapshot.snapshot.snapshot_id))
                .await
                .unwrap();
            manager
                .inner
                .fail_cleanup_after_phase
                .store(phase, Ordering::SeqCst);
            assert_eq!(
                manager
                    .execute_cleanup(&prepared, &CancellationToken::new())
                    .await
                    .unwrap_err(),
                SnapshotError::OperationFailed
            );
            drop(prepared);
            drop(manager);
            let files = FileToolGroup::new(workspace.path(), true, None)
                .await
                .unwrap();
            SnapshotManager::open(files.workspace_snapshot_access(), storage.path(), &[])
                .await
                .unwrap();
            for directory in ["blobs", "manifests", "checkpoints", "journals"] {
                assert_eq!(
                    std_fs::read_dir(storage.path().join(directory))
                        .unwrap()
                        .count(),
                    0
                );
            }
        }
    }

    #[tokio::test]
    async fn restore_prepares_journals_and_only_the_missing_ancestor_directories() {
        let (workspace, _storage, manager) = fixture().await;
        let nested = workspace.path().join("one").join("two");
        std_fs::create_dir_all(&nested).unwrap();
        std_fs::write(nested.join("file.txt"), "before").unwrap();
        let snapshot = manager
            .capture(
                &Identifier::new("nested").unwrap(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        std_fs::remove_dir_all(workspace.path().join("one")).unwrap();
        let (prepared, preview) = manager
            .prepare_restore(&snapshot.snapshot.snapshot_id, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            preview
                .created_directories
                .iter()
                .map(WorkspacePath::as_str)
                .collect::<Vec<_>>(),
            ["one", "one/two"]
        );
        assert_eq!(
            prepared.created_directories().collect::<Vec<_>>(),
            ["one", "one/two"]
        );
        let status = manager
            .execute_restore(&prepared, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(status.restore_id, preview.restore_id);
        assert_eq!(
            std_fs::read_to_string(workspace.path().join("one/two/file.txt")).unwrap(),
            "before"
        );
        let journal = lock(&manager.inner.state)
            .journals
            .get(status.restore_id.as_str())
            .unwrap()
            .clone();
        assert_eq!(journal.created_directories, ["one", "one/two"]);
        assert_eq!(journal.applied_directories, 2);
    }

    #[tokio::test]
    async fn later_created_configured_exclusions_remain_excluded() {
        let workspace = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        std_fs::set_permissions(
            storage.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let files = FileToolGroup::new(workspace.path(), true, None)
            .await
            .unwrap();
        let excluded = workspace.path().join("generated/private");
        let manager = SnapshotManager::open(
            files.workspace_snapshot_access(),
            storage.path(),
            std::slice::from_ref(&excluded),
        )
        .await
        .unwrap();
        std_fs::create_dir_all(&excluded).unwrap();
        std_fs::write(excluded.join("secret"), "secret").unwrap();
        std_fs::write(workspace.path().join("visible"), "visible").unwrap();
        let capture = manager
            .capture(
                &Identifier::new("exclusion").unwrap(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let inspected = manager
            .inspect(&capture.snapshot.snapshot_id, 10, None)
            .await
            .unwrap();
        assert_eq!(
            inspected
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            ["visible"]
        );
        assert!(
            inspected
                .exclusions
                .iter()
                .any(|path| path.as_str() == "generated/private")
        );
    }

    #[tokio::test]
    async fn configured_exclusions_reject_escape_and_parent_components() {
        let workspace = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        std_fs::set_permissions(
            storage.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let files = FileToolGroup::new(workspace.path(), true, None)
            .await
            .unwrap();
        assert_eq!(
            SnapshotManager::open(
                files.workspace_snapshot_access(),
                storage.path(),
                &[outside.path().join("later")],
            )
            .await
            .err()
            .unwrap(),
            SnapshotError::InvalidConfiguration
        );
        assert_eq!(
            configured_exclusions(workspace.path(), &[PathBuf::from("safe/../escape")])
                .unwrap_err(),
            SnapshotError::InvalidConfiguration
        );
    }

    #[tokio::test]
    async fn no_op_restore_journals_remain_bounded_live_and_queryable_after_reopen() {
        let (workspace, storage, manager) = fixture().await;
        std_fs::write(workspace.path().join("file.txt"), "content").unwrap();
        let snapshot = manager
            .capture(
                &Identifier::new("no-op").unwrap(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let mut latest = None;
        for _ in 0..=MAX_SNAPSHOT_JOURNALS {
            let (prepared, preview) = manager
                .prepare_restore(&snapshot.snapshot.snapshot_id, &CancellationToken::new())
                .await
                .unwrap();
            manager
                .execute_restore(&prepared, &CancellationToken::new())
                .await
                .unwrap();
            latest = Some(preview.restore_id);
        }
        assert!(manager.journal_usage().await.unwrap().0 <= MAX_SNAPSHOT_JOURNALS);
        let latest = latest.unwrap();
        drop(manager);
        let files = FileToolGroup::new(workspace.path(), true, None)
            .await
            .unwrap();
        let reopened =
            SnapshotManager::open(files.workspace_snapshot_access(), storage.path(), &[])
                .await
                .unwrap();
        assert_eq!(reopened.status(&latest).unwrap().restore.restore_id, latest);
        assert!(reopened.journal_usage().await.unwrap().0 <= MAX_SNAPSHOT_JOURNALS);
    }

    #[tokio::test]
    async fn journal_byte_quota_is_enforced_at_the_live_restart_boundary() {
        let (workspace, storage, manager) = fixture().await;
        std_fs::write(workspace.path().join("file.txt"), "content").unwrap();
        let snapshot = manager
            .capture(
                &Identifier::new("journal-byte-boundary").unwrap(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let journal_count = MAX_JOURNAL_STORAGE_BYTES / MAX_PRIVATE_METADATA_BYTES;
        for index in 0..journal_count {
            let journal = StoredJournal {
                version: JOURNAL_VERSION.to_owned(),
                restore_id: format!("restore_byte_boundary_{index}"),
                state: StoredRestoreState::Completed,
                target_snapshot_id: snapshot.snapshot.snapshot_id.as_str().to_owned(),
                pre_restore_snapshot_id: snapshot.snapshot.snapshot_id.as_str().to_owned(),
                changes: Vec::new(),
                created_directories: Vec::new(),
                applied_files: 0,
                applied_directories: 0,
                reconciliation_required: false,
                acknowledgement_required: true,
                unrevert_of: None,
            };
            let mut bytes = serde_json::to_vec(&journal).unwrap();
            bytes.resize(MAX_PRIVATE_METADATA_BYTES as usize, b' ');
            write_private_file(&manager.journal_path(&journal.restore_id).unwrap(), &bytes)
                .await
                .unwrap();
        }
        drop(manager);
        let files = FileToolGroup::new(workspace.path(), true, None)
            .await
            .unwrap();
        let reopened =
            SnapshotManager::open(files.workspace_snapshot_access(), storage.path(), &[])
                .await
                .unwrap();
        let (prepared, _) = reopened
            .prepare_restore(&snapshot.snapshot.snapshot_id, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            reopened
                .execute_restore(&prepared, &CancellationToken::new())
                .await
                .unwrap_err(),
            SnapshotError::QuotaExceeded
        );
        assert_eq!(
            reopened.journal_usage().await.unwrap(),
            (journal_count as usize, MAX_JOURNAL_STORAGE_BYTES)
        );
    }

    #[tokio::test]
    async fn unacknowledged_journal_count_quota_rejects_a_live_restore() {
        let (workspace, _storage, manager) = fixture().await;
        std_fs::write(workspace.path().join("file.txt"), "content").unwrap();
        let snapshot = manager
            .capture(
                &Identifier::new("journal-count-boundary").unwrap(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        for index in 0..MAX_SNAPSHOT_JOURNALS {
            let journal = StoredJournal {
                version: JOURNAL_VERSION.to_owned(),
                restore_id: format!("restore_count_boundary_{index}"),
                state: StoredRestoreState::Completed,
                target_snapshot_id: snapshot.snapshot.snapshot_id.as_str().to_owned(),
                pre_restore_snapshot_id: snapshot.snapshot.snapshot_id.as_str().to_owned(),
                changes: Vec::new(),
                created_directories: Vec::new(),
                applied_files: 0,
                applied_directories: 0,
                reconciliation_required: false,
                acknowledgement_required: true,
                unrevert_of: None,
            };
            manager.persist_journal(&journal).await.unwrap();
            lock(&manager.inner.state)
                .journals
                .insert(journal.restore_id.clone(), journal);
        }
        let (prepared, _) = manager
            .prepare_restore(&snapshot.snapshot.snapshot_id, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            manager
                .execute_restore(&prepared, &CancellationToken::new())
                .await
                .unwrap_err(),
            SnapshotError::QuotaExceeded
        );
        assert_eq!(
            manager.journal_usage().await.unwrap().0,
            MAX_SNAPSHOT_JOURNALS
        );
    }
}
