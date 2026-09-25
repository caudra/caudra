#![forbid(unsafe_code)]

//! Workspace snapshots for Workcell hosts: bounded captures of the session directory into a
//! private content-addressed store, and journaled restores between any two of them.

mod capture;
mod cleanup;
mod manifest;
mod restore;
mod store;

pub use capture::{SnapshotCapturePhase, SnapshotCaptureProgress, SnapshotCaptureProgressSink};

use std::{
    collections::{BTreeSet, HashMap},
    fmt::Write as _,
    mem::size_of,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workcell_host_contract::{
    ContractVersion, Cursor, Identifier, MAX_PAGE_SIZE, MAX_SNAPSHOT_CAPTURE_ENTRIES,
    MAX_SNAPSHOT_CAPTURE_PATH_BYTES, MAX_SNAPSHOT_CLEANUP, MAX_SNAPSHOT_COUNT,
    MAX_SNAPSHOT_FILE_BYTES, MAX_SNAPSHOT_FILES, MAX_SNAPSHOT_JOURNALS, MAX_SNAPSHOT_STORAGE_BYTES,
    MAX_SNAPSHOT_TOTAL_BYTES, ResourceId, Revision, SnapshotAcknowledgeResponse,
    SnapshotCaptureLimits, SnapshotCaptureResponse, SnapshotCleanupPreview,
    SnapshotCleanupResponse, SnapshotInspectResponse, SnapshotLimit, SnapshotRestorePreview,
    SnapshotRestoreStatus, SnapshotStatusResponse, WorkspacePath, WorkspaceSnapshotCapability,
    WorkspaceSnapshotLimits, WorkspaceSnapshotMethods,
};
use workcell_mcp_files::{
    RootResourceKind, SnapshotTreeError, SnapshotTreeLimit, WorkspaceSnapshotAccess,
    WorkspaceSnapshotScope, root_relative_resource_id,
};

use crate::{
    capture::{CaptureProgress, capture_response, validate_limits},
    cleanup::CleanupPlan,
    manifest::{MAX_MANIFEST_BYTES, Manifest, SNAPSHOT_ID_PREFIX, StoredEntry},
    restore::{RestorePlan, StoredJournal},
    store::{CHECKPOINTS, DIGEST_PREFIX, Store},
};

const CHECKPOINT_VERSION: &str = "workspace-snapshot-checkpoint.v1";
const CAPTURE_ADMISSION_TIMEOUT: Duration = Duration::from_secs(30);
const CAPTURE_EXECUTION_BUDGET: Duration = Duration::from_secs(15 * 60);
const MAX_PRIVATE_METADATA_BYTES: u64 = 2 * 1_024 * 1_024;
const MAX_EXCLUSIONS: usize = 32;
/// Room for abandoned temporaries beside every entry the quotas allow.
const PRIVATE_ENTRY_SLACK: usize = 64;
const MAX_PRIVATE_ENTRIES: usize = MAX_SNAPSHOT_COUNT * MAX_SNAPSHOT_FILES
    + MAX_SNAPSHOT_COUNT
    + MAX_SNAPSHOT_JOURNALS
    + PRIVATE_ENTRY_SLACK;
const LEASE_PREFIX: &str = "lease_";
const CLEANUP_SCOPE_PREFIX: &str = "snapshot-store:cleanup:";
const CURSOR_SEPARATOR: char = ':';
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
    store: Store,
    exclusions: Vec<String>,
    capture: Arc<AsyncMutex<()>>,
    publication: Arc<AsyncMutex<()>>,
    state: Mutex<RuntimeState>,
}

#[derive(Default)]
struct RuntimeState {
    journals: HashMap<String, StoredJournal>,
    /// Snapshots each prepared restore reads, kept from cleanup until it is executed or dropped.
    pending: HashMap<String, BTreeSet<String>>,
}

pub struct PreparedSnapshotRestore {
    manager: SnapshotManager,
    lease_id: String,
    plan: Arc<RestorePlan>,
}

pub struct PreparedSnapshotCapture {
    manager: SnapshotManager,
    checkpoint_id: Identifier,
    scope: WorkspaceSnapshotScope,
    limits: SnapshotCaptureLimits,
}

pub struct PreparedSnapshotCleanup {
    manager: SnapshotManager,
    plan: Arc<CleanupPlan>,
    preview: SnapshotCleanupPreview,
    resource_scope: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredCheckpoint {
    version: String,
    checkpoint_id: String,
    snapshot_id: String,
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
    #[error("the snapshot scope or a restored path is not a plain workspace entry")]
    UnsupportedFile,
    #[error("workspace snapshots need descriptor-relative traversal this host does not support")]
    UnsupportedPlatform,
    #[error("snapshot {limit} limit was exceeded")]
    LimitExceeded {
        limit: SnapshotLimit,
        maximum: Option<u64>,
    },
    #[error("snapshot {limit} quota was exceeded")]
    QuotaExceeded {
        limit: SnapshotLimit,
        maximum: Option<u64>,
    },
    #[error("snapshot storage is busy")]
    Busy,
    #[error("snapshot capture execution budget was exhausted")]
    TimedOut,
    #[error("the workspace no longer matches what the snapshot operation expects")]
    Conflict,
    #[error("another restore awaits acknowledgement")]
    AcknowledgementRequired,
    #[error("snapshot operation was cancelled")]
    Cancelled,
    #[error("snapshot operation failed")]
    OperationFailed,
    #[error("snapshot capture rollback could not be confirmed")]
    RollbackFailed,
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
            Self::UnsupportedPlatform => "unsupported_platform",
            Self::LimitExceeded { .. } => "limit_exceeded",
            Self::QuotaExceeded { .. } => "quota_exceeded",
            Self::Busy => "busy",
            Self::TimedOut => "timed_out",
            Self::Conflict => "conflict",
            Self::AcknowledgementRequired => "acknowledgement_required",
            Self::Cancelled => "cancelled",
            Self::OperationFailed => "operation_failed",
            Self::RollbackFailed => "rollback_failed",
        }
    }

    /// The limit or quota reached, with its maximum where it has one.
    #[must_use]
    pub const fn limit(self) -> Option<(SnapshotLimit, Option<u64>)> {
        match self {
            Self::LimitExceeded { limit, maximum } | Self::QuotaExceeded { limit, maximum } => {
                Some((limit, maximum))
            }
            _ => None,
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

    /// Opens the store beneath `private_root` that belongs to one workspace binding.
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
        let private_root = private_root.to_path_buf();
        let excluded_paths = excluded_paths.to_vec();
        let binding = workspace_binding.map(|binding| binding.as_str().to_owned());
        let inner = tokio::task::spawn_blocking(move || {
            let store = Store::open(&private_root, workspace.root(), binding.as_deref())?;
            let exclusions = configured_exclusions(workspace.root(), &excluded_paths)?;
            let inner = SnapshotInner {
                workspace,
                store,
                exclusions,
                capture: Arc::default(),
                publication: Arc::default(),
                state: Mutex::default(),
            };
            inner.store.remove_temporaries()?;
            inner.load_journals()?;
            Ok::<_, SnapshotError>(inner)
        })
        .await
        .map_err(|_| SnapshotError::OperationFailed)??;
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    #[must_use]
    pub fn capability() -> WorkspaceSnapshotCapability {
        WorkspaceSnapshotCapability {
            version: ContractVersion::V1,
            methods: WorkspaceSnapshotMethods {
                capture: true,
                prepare_capture: true,
                checkpoint: true,
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
                max_cleanup_checkpoints: u32::try_from(MAX_SNAPSHOT_CLEANUP).unwrap_or(u32::MAX),
            },
            atomic_across_files: false,
            durable_per_file_journal: false,
        }
    }

    /// Captures the root-relative directory `scope` as `checkpoint_id`, or returns the capture
    /// that checkpoint already names.
    pub async fn capture(
        &self,
        checkpoint_id: &Identifier,
        scope: &WorkspacePath,
        limits: &SnapshotCaptureLimits,
        token: &CancellationToken,
    ) -> Result<SnapshotCaptureResponse, SnapshotError> {
        validate_limits(limits)?;
        let scope = self
            .inner
            .workspace
            .snapshot_scope(scope)
            .await
            .map_err(|_| SnapshotError::UnsupportedFile)?;
        let prepared = self.prepare_capture(checkpoint_id, &scope, limits)?;
        self.execute_capture(&prepared, token, None).await
    }

    pub fn prepare_capture(
        &self,
        checkpoint_id: &Identifier,
        scope: &WorkspaceSnapshotScope,
        limits: &SnapshotCaptureLimits,
    ) -> Result<PreparedSnapshotCapture, SnapshotError> {
        validate_limits(limits)?;
        Ok(PreparedSnapshotCapture {
            manager: self.clone(),
            checkpoint_id: checkpoint_id.clone(),
            scope: scope.clone(),
            limits: limits.clone(),
        })
    }

    pub async fn execute_capture(
        &self,
        prepared: &PreparedSnapshotCapture,
        token: &CancellationToken,
        progress: Option<Arc<dyn SnapshotCaptureProgressSink>>,
    ) -> Result<SnapshotCaptureResponse, SnapshotError> {
        if !Arc::ptr_eq(&self.inner, &prepared.manager.inner) {
            return Err(SnapshotError::InvalidRequest);
        }
        let mut progress = CaptureProgress::new(progress);
        let cancellation = token.child_token();
        let _cancel_on_drop = cancellation.clone().drop_guard();
        let execution = async {
            let guards = match self.acquire_capture_guards(&cancellation).await {
                Ok(guards) => guards,
                Err(error) => {
                    progress.phase(SnapshotCapturePhase::Finished);
                    return Err(error);
                }
            };
            let checkpoint_id = prepared.checkpoint_id.as_str().to_owned();
            let scope = prepared.scope.clone();
            let limits = prepared.limits.clone();
            let token = cancellation.clone();
            self.blocking(guards, move |inner| {
                let result = inner.capture(&checkpoint_id, &scope, &limits, &token, &mut progress);
                progress.phase(SnapshotCapturePhase::Finished);
                result
            })
            .await
        };
        tokio::pin!(execution);
        tokio::select! {
            biased;
            result = &mut execution => result,
            () = tokio::time::sleep(CAPTURE_EXECUTION_BUDGET) => {
                cancellation.cancel();
                match execution.await {
                    Err(SnapshotError::Cancelled) => Err(SnapshotError::TimedOut),
                    result => result,
                }
            }
        }
    }

    pub async fn checkpoint(
        &self,
        checkpoint_id: &Identifier,
        scope: &WorkspacePath,
    ) -> Result<SnapshotCaptureResponse, SnapshotError> {
        let checkpoint_id = checkpoint_id.as_str().to_owned();
        let scope = scope.as_str().to_owned();
        let publication = Arc::clone(&self.inner.publication)
            .try_lock_owned()
            .map_err(|_| SnapshotError::Busy)?;
        self.blocking(publication, move |inner| {
            let manifest = inner
                .load_checkpoint(&checkpoint_id)?
                .ok_or(SnapshotError::NotFound)?;
            capture_response(&manifest, &checkpoint_id, &scope, true)
        })
        .await
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
        let snapshot_id = snapshot_id.as_str().to_owned();
        let cursor = cursor.cloned();
        self.blocking((), move |inner| {
            inner.inspect(&snapshot_id, page_size, cursor.as_ref())
        })
        .await
    }

    pub fn status(&self, restore_id: &Identifier) -> Result<SnapshotStatusResponse, SnapshotError> {
        Ok(SnapshotStatusResponse {
            version: ContractVersion::V1,
            restore: self.inner.journal(restore_id.as_str())?.status()?,
        })
    }

    /// Plans restoring `snapshot_id` over a workspace believed to match `source_snapshot_id`.
    pub async fn prepare_restore(
        &self,
        snapshot_id: &Identifier,
        source_snapshot_id: &Identifier,
        maximum_retained_bytes: usize,
        token: &CancellationToken,
    ) -> Result<(PreparedSnapshotRestore, SnapshotRestorePreview), SnapshotError> {
        let guards = self.capture_guards(token).await?;
        let manager = self.clone();
        let target = snapshot_id.as_str().to_owned();
        let source = source_snapshot_id.as_str().to_owned();
        let token = token.clone();
        self.blocking(guards, move |inner| {
            let plan = inner.prepare_restore(&target, &source, None, &token)?;
            manager.lease(plan, maximum_retained_bytes)
        })
        .await
    }

    /// Plans undoing a restore that still awaits acknowledgement.
    pub async fn prepare_unrevert(
        &self,
        restore_id: &Identifier,
        maximum_retained_bytes: usize,
        token: &CancellationToken,
    ) -> Result<(PreparedSnapshotRestore, SnapshotRestorePreview), SnapshotError> {
        let guards = self.capture_guards(token).await?;
        let manager = self.clone();
        let restore_id = restore_id.as_str().to_owned();
        let token = token.clone();
        self.blocking(guards, move |inner| {
            let plan = inner.prepare_unrevert(&restore_id, &token)?;
            manager.lease(plan, maximum_retained_bytes)
        })
        .await
    }

    pub async fn acknowledge(
        &self,
        restore_id: &Identifier,
    ) -> Result<SnapshotAcknowledgeResponse, SnapshotError> {
        let publication = Arc::clone(&self.inner.publication).lock_owned().await;
        let restore_id = restore_id.as_str().to_owned();
        self.blocking(publication, move |inner| inner.acknowledge(&restore_id))
            .await
    }

    /// Plans deleting `checkpoint_ids` and everything only they kept.
    pub async fn prepare_cleanup(
        &self,
        checkpoint_ids: &[Identifier],
        maximum_retained_bytes: usize,
    ) -> Result<(PreparedSnapshotCleanup, SnapshotCleanupPreview), SnapshotError> {
        let requested = checkpoint_ids
            .iter()
            .map(|id| id.as_str().to_owned())
            .collect::<BTreeSet<_>>();
        if checkpoint_ids.len() > MAX_SNAPSHOT_CLEANUP || requested.len() != checkpoint_ids.len() {
            return Err(SnapshotError::InvalidRequest);
        }
        let publication = Arc::clone(&self.inner.publication).lock_owned().await;
        let plan = self
            .blocking(publication, move |inner| inner.plan_cleanup(&requested))
            .await?;
        let preview = plan.preview()?;
        let prepared = PreparedSnapshotCleanup {
            manager: self.clone(),
            resource_scope: format!("{CLEANUP_SCOPE_PREFIX}{}", digest_serializable(&plan)?),
            plan: Arc::new(plan),
            preview: preview.clone(),
        };
        if prepared.retained_bytes() > maximum_retained_bytes {
            return Err(limit_error(
                SnapshotLimit::PreparedBytes,
                maximum_retained_bytes,
            ));
        }
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
        let publication = Arc::clone(&self.inner.publication).lock_owned().await;
        let workspace = self
            .inner
            .workspace
            .mutation_guard()
            .await
            .map_err(|_| SnapshotError::OperationFailed)?;
        let plan = Arc::clone(&prepared.plan);
        let token = token.clone();
        self.blocking((publication, workspace), move |inner| {
            inner.execute_restore(&plan, &token)
        })
        .await
    }

    pub async fn execute_cleanup(
        &self,
        prepared: &PreparedSnapshotCleanup,
        token: &CancellationToken,
    ) -> Result<SnapshotCleanupResponse, SnapshotError> {
        if !Arc::ptr_eq(&self.inner, &prepared.manager.inner) {
            return Err(SnapshotError::InvalidRequest);
        }
        let publication = Arc::clone(&self.inner.publication).lock_owned().await;
        let plan = Arc::clone(&prepared.plan);
        let token = token.clone();
        self.blocking(publication, move |inner| {
            inner.execute_cleanup(&plan, &token)
        })
        .await
    }

    /// Runs `work` off the executor. `guards` are released only when it finishes, even if the
    /// caller stops waiting, so no other operation overlaps it.
    async fn blocking<G, T>(
        &self,
        guards: G,
        work: impl FnOnce(&SnapshotInner) -> Result<T, SnapshotError> + Send + 'static,
    ) -> Result<T, SnapshotError>
    where
        G: Send + 'static,
        T: Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let _guards = guards;
            work(&inner)
        })
        .await
        .map_err(|_| SnapshotError::OperationFailed)?
    }

    /// One admission deadline covers every lock a capture or restore preparation holds.
    async fn capture_guards(
        &self,
        token: &CancellationToken,
    ) -> Result<[OwnedMutexGuard<()>; 3], SnapshotError> {
        tokio::time::timeout(
            CAPTURE_ADMISSION_TIMEOUT,
            self.acquire_capture_guards(token),
        )
        .await
        .map_err(|_| SnapshotError::Busy)?
    }

    async fn acquire_capture_guards(
        &self,
        token: &CancellationToken,
    ) -> Result<[OwnedMutexGuard<()>; 3], SnapshotError> {
        let acquire = async {
            let capture = Arc::clone(&self.inner.capture).lock_owned().await;
            let publication = Arc::clone(&self.inner.publication).lock_owned().await;
            let workspace = self.inner.workspace.capture_guard().await;
            [capture, publication, workspace]
        };
        tokio::select! {
            biased;
            () = token.cancelled() => Err(SnapshotError::Cancelled),
            guards = acquire => Ok(guards),
        }
    }

    /// Keeps the plan's snapshots from cleanup for as long as the prepared restore lives. Runs
    /// under the preparation's locks, so no cleanup sees the plan unprotected.
    fn lease(
        &self,
        plan: RestorePlan,
        maximum_retained_bytes: usize,
    ) -> Result<(PreparedSnapshotRestore, SnapshotRestorePreview), SnapshotError> {
        let lease_id = format!("{LEASE_PREFIX}{}", Uuid::new_v4());
        lock(&self.inner.state).pending.insert(
            lease_id.clone(),
            BTreeSet::from([
                plan.target_snapshot_id.clone(),
                plan.source_snapshot_id.clone(),
            ]),
        );
        let preview = plan.preview.clone();
        let prepared = PreparedSnapshotRestore {
            manager: self.clone(),
            lease_id,
            plan: Arc::new(plan),
        };
        if prepared.retained_bytes() > maximum_retained_bytes {
            return Err(limit_error(
                SnapshotLimit::PreparedBytes,
                maximum_retained_bytes,
            ));
        }
        Ok((prepared, preview))
    }
}

impl SnapshotInner {
    fn load_manifest(&self, snapshot_id: &str) -> Result<Manifest, SnapshotError> {
        let bytes = self
            .store
            .read(&self.store.manifest_path(snapshot_id)?, MAX_MANIFEST_BYTES)?;
        Manifest::decode(snapshot_id, &bytes)
    }

    fn read_checkpoint(&self, path: &Path) -> Result<StoredCheckpoint, SnapshotError> {
        let checkpoint: StoredCheckpoint =
            serde_json::from_slice(&self.store.read(path, MAX_PRIVATE_METADATA_BYTES)?)
                .map_err(|_| SnapshotError::IntegrityFailure)?;
        if checkpoint.version != CHECKPOINT_VERSION
            || self.store.checkpoint_path(&checkpoint.checkpoint_id) != path
        {
            return Err(SnapshotError::IntegrityFailure);
        }
        Ok(checkpoint)
    }

    fn load_checkpoint(&self, checkpoint_id: &str) -> Result<Option<Manifest>, SnapshotError> {
        match self.read_checkpoint(&self.store.checkpoint_path(checkpoint_id)) {
            Ok(checkpoint) => {
                let manifest = self.load_manifest(&checkpoint.snapshot_id)?;
                self.store.sync(CHECKPOINTS)?;
                Ok(Some(manifest))
            }
            Err(SnapshotError::NotFound) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn inspect(
        &self,
        snapshot_id: &str,
        page_size: u32,
        cursor: Option<&Cursor>,
    ) -> Result<SnapshotInspectResponse, SnapshotError> {
        let manifest = self.load_manifest(snapshot_id)?;
        let entries = &manifest.content.entries;
        let offset = parse_cursor(cursor, snapshot_id, entries.len())?;
        let end = offset
            .saturating_add(usize::try_from(page_size).unwrap_or(usize::MAX))
            .min(entries.len());
        let next_cursor = (end < entries.len())
            .then(|| Cursor::new(format!("{snapshot_id}{CURSOR_SEPARATOR}{end}")))
            .transpose()
            .map_err(|_| SnapshotError::OperationFailed)?;
        Ok(SnapshotInspectResponse {
            version: ContractVersion::V1,
            snapshot: manifest.summary(None)?,
            files: entries[offset..end]
                .iter()
                .map(StoredEntry::contract)
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
}

impl PreparedSnapshotRestore {
    /// Conservative retained bytes, excluding the snapshot manager shared with the host.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.lease_id.capacity())
            .saturating_add(self.plan.retained_bytes())
    }

    #[must_use]
    pub fn restore_id(&self) -> &str {
        &self.plan.restore_id
    }

    /// The root-relative directory, `.` for the root, beneath which the restore changes entries.
    #[must_use]
    pub fn scope(&self) -> &str {
        &self.plan.scope
    }

    /// The restore this one undoes, whose journal it settles once it completes.
    #[must_use]
    pub fn unrevert_of(&self) -> Option<&str> {
        self.plan.unrevert_of.as_deref()
    }
}

impl PreparedSnapshotCapture {
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.checkpoint_id.as_str().len())
            .saturating_add(self.scope.retained_bytes())
    }

    #[must_use]
    pub fn scope(&self) -> &str {
        self.scope.path()
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
    /// Conservative retained bytes, excluding the snapshot manager shared with the host.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.plan.retained_bytes())
            .saturating_add(self.resource_scope.capacity())
            .saturating_add(
                self.preview
                    .checkpoint_ids
                    .iter()
                    .chain(&self.preview.missing_checkpoint_ids)
                    .map(Identifier::retained_bytes)
                    .fold(0, usize::saturating_add),
            )
    }

    #[must_use]
    pub const fn preview(&self) -> &SnapshotCleanupPreview {
        &self.preview
    }

    #[must_use]
    pub fn resource_scope(&self) -> &str {
        &self.resource_scope
    }
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

/// The root-relative form of a configured exclusion, resolved through whatever part of it
/// exists so a later-created path stays excluded.
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
                suffix.push(
                    ancestor
                        .file_name()
                        .ok_or(SnapshotError::InvalidConfiguration)?
                        .to_owned(),
                );
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
    let relative = relative
        .components()
        .map(|component| match component {
            Component::Normal(part) => part.to_str().ok_or(SnapshotError::InvalidConfiguration),
            _ => Err(SnapshotError::InvalidConfiguration),
        })
        .collect::<Result<Vec<_>, _>>()?
        .join("/");
    if relative.is_empty() || !manifest::valid_path(&relative) {
        return Err(SnapshotError::InvalidConfiguration);
    }
    Ok(relative)
}

fn validate_snapshot_id(value: &str) -> Result<(), SnapshotError> {
    value
        .strip_prefix(SNAPSHOT_ID_PREFIX)
        .filter(|hex| valid_hex_digest(hex))
        .map(|_| ())
        .ok_or(SnapshotError::IntegrityFailure)
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
    let offset = cursor
        .as_str()
        .strip_prefix(snapshot_id)
        .and_then(|rest| rest.strip_prefix(CURSOR_SEPARATOR))
        .ok_or(SnapshotError::InvalidRequest)?
        .parse::<usize>()
        .map_err(|_| SnapshotError::InvalidRequest)?;
    if offset == 0 || offset >= length {
        return Err(SnapshotError::InvalidRequest);
    }
    Ok(offset)
}

fn digest_serializable(value: &impl Serialize) -> Result<String, SnapshotError> {
    let bytes = serde_json::to_vec(value).map_err(|_| SnapshotError::OperationFailed)?;
    Ok(digest_bytes(&bytes))
}

fn digest_bytes(bytes: &[u8]) -> String {
    format_sha256(Sha256::digest(bytes))
}

fn format_sha256(digest: impl IntoIterator<Item = u8>) -> String {
    format!("{DIGEST_PREFIX}{}", hex(digest))
}

fn hex_sha256(bytes: &[u8]) -> String {
    hex(Sha256::digest(bytes))
}

fn hex(bytes: impl IntoIterator<Item = u8>) -> String {
    let mut output = String::new();
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
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

fn path_resource_id(path: &str) -> Result<ResourceId, SnapshotError> {
    root_relative_resource_id(RootResourceKind::Path, path)
        .map_err(|_| SnapshotError::IntegrityFailure)
}

fn check_cancelled(token: &CancellationToken) -> Result<(), SnapshotError> {
    if token.is_cancelled() {
        Err(SnapshotError::Cancelled)
    } else {
        Ok(())
    }
}

fn limit_error(limit: SnapshotLimit, maximum: impl TryInto<u64>) -> SnapshotError {
    SnapshotError::LimitExceeded {
        limit,
        maximum: maximum.try_into().ok(),
    }
}

fn quota_error(limit: SnapshotLimit, maximum: impl TryInto<u64>) -> SnapshotError {
    SnapshotError::QuotaExceeded {
        limit,
        maximum: maximum.try_into().ok(),
    }
}

fn tree_error(error: SnapshotTreeError) -> SnapshotError {
    match error {
        SnapshotTreeError::LimitExceeded { limit, maximum } => limit_error(
            match limit {
                SnapshotTreeLimit::Entries => SnapshotLimit::CaptureEntries,
                SnapshotTreeLimit::PathBytes => SnapshotLimit::CapturePathBytes,
                SnapshotTreeLimit::Depth => SnapshotLimit::Depth,
            },
            maximum,
        ),
        SnapshotTreeError::IgnoreRulesExceeded => SnapshotError::LimitExceeded {
            limit: SnapshotLimit::IgnoreRules,
            maximum: None,
        },
        SnapshotTreeError::ScopeUnavailable
        | SnapshotTreeError::Protected
        | SnapshotTreeError::Blocked => SnapshotError::UnsupportedFile,
        SnapshotTreeError::Changed => SnapshotError::Conflict,
        SnapshotTreeError::Unsupported => SnapshotError::UnsupportedPlatform,
        SnapshotTreeError::Cancelled => SnapshotError::Cancelled,
        SnapshotTreeError::Failed(_) | SnapshotTreeError::Unsettled(_) => {
            SnapshotError::OperationFailed
        }
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        ffi::OsStr,
        fs,
        io::Write as _,
        os::unix::{
            ffi::OsStrExt,
            fs::{OpenOptionsExt, PermissionsExt, symlink},
            net::UnixListener,
        },
        sync::{atomic::Ordering, mpsc},
        task::Poll,
        time::Instant,
    };

    use rustix::fs::{CWD, FileType, Mode, mknodat};
    use tempfile::TempDir;
    use tokio::sync::Notify;
    use workcell_host_contract::{
        SnapshotChangeCounts, SnapshotChangeKind, SnapshotEntryKind, SnapshotRestoreState,
        SnapshotSkipReason, SnapshotSkipped, SnapshotSummary,
    };
    use workcell_mcp_files::FileToolGroup;

    use super::*;
    use crate::{
        capture::MAX_BLOB_BATCH_FILES,
        store::{BLOBS, CHECKPOINTS, JOURNALS, MANIFESTS},
    };

    const ROOT: &str = ".";
    const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
    const PRIVATE_FILE_MODE: u32 = 0o600;
    const BARRIER_TIMEOUT: Duration = Duration::from_secs(10);
    const BENCHMARK_FILES: usize = 20_000;
    const BENCHMARK_DIRECTORIES: usize = 100;
    const BENCHMARK_FILE_BYTES: usize = 128;

    struct CaptureBarrier {
        entered: Notify,
        release: Mutex<mpsc::Receiver<()>>,
        phase: SnapshotCapturePhase,
    }

    impl SnapshotCaptureProgressSink for CaptureBarrier {
        fn publish(&self, progress: SnapshotCaptureProgress) {
            if progress.phase == self.phase {
                self.entered.notify_one();
                lock(&self.release).recv_timeout(BARRIER_TIMEOUT).unwrap();
            }
        }
    }

    struct Fixture {
        workspace: TempDir,
        storage: TempDir,
        manager: SnapshotManager,
    }

    impl Fixture {
        async fn new() -> Self {
            let workspace = tempfile::tempdir().unwrap();
            let storage = private_directory();
            let manager = open_manager(workspace.path(), storage.path(), &[])
                .await
                .unwrap();
            Self {
                workspace,
                storage,
                manager,
            }
        }

        async fn reopen(&mut self) {
            self.manager = open_manager(self.workspace.path(), self.storage.path(), &[])
                .await
                .unwrap();
        }

        fn path(&self, relative: &str) -> PathBuf {
            self.workspace.path().join(relative)
        }

        fn write(&self, relative: &str, contents: &str) {
            let path = self.path(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }

        fn read(&self, relative: &str) -> String {
            fs::read_to_string(self.path(relative)).unwrap()
        }

        fn stored(&self, directory: &str) -> usize {
            fs::read_dir(self.storage.path().join(directory))
                .unwrap()
                .count()
        }

        async fn try_capture(
            &self,
            checkpoint: &str,
            scope: &str,
            limits: &SnapshotCaptureLimits,
        ) -> Result<SnapshotCaptureResponse, SnapshotError> {
            self.manager
                .capture(
                    &id(checkpoint),
                    &WorkspacePath::new(scope).unwrap(),
                    limits,
                    &CancellationToken::new(),
                )
                .await
        }

        async fn capture(&self, checkpoint: &str) -> SnapshotSummary {
            self.try_capture(checkpoint, ROOT, &limits())
                .await
                .unwrap()
                .snapshot
        }

        async fn capture_scope(&self, checkpoint: &str, scope: &str) -> SnapshotSummary {
            self.try_capture(checkpoint, scope, &limits())
                .await
                .unwrap()
                .snapshot
        }

        async fn prepare(
            &self,
            target: &SnapshotSummary,
            source: &SnapshotSummary,
        ) -> Result<(PreparedSnapshotRestore, SnapshotRestorePreview), SnapshotError> {
            self.manager
                .prepare_restore(
                    &target.snapshot_id,
                    &source.snapshot_id,
                    usize::MAX,
                    &CancellationToken::new(),
                )
                .await
        }

        async fn execute(
            &self,
            prepared: &PreparedSnapshotRestore,
        ) -> Result<SnapshotRestoreStatus, SnapshotError> {
            self.manager
                .execute_restore(prepared, &CancellationToken::new())
                .await
        }

        async fn restore(
            &self,
            target: &SnapshotSummary,
            source: &SnapshotSummary,
        ) -> SnapshotRestoreStatus {
            let (prepared, _) = self.prepare(target, source).await.unwrap();
            self.execute(&prepared).await.unwrap()
        }

        async fn cleanup(&self, checkpoints: &[&str]) -> SnapshotCleanupResponse {
            let checkpoint_ids = checkpoints
                .iter()
                .map(|checkpoint| id(checkpoint))
                .collect::<Vec<_>>();
            let (prepared, _) = self
                .manager
                .prepare_cleanup(&checkpoint_ids, usize::MAX)
                .await
                .unwrap();
            self.manager
                .execute_cleanup(&prepared, &CancellationToken::new())
                .await
                .unwrap()
        }

        async fn paths(&self, snapshot: &SnapshotSummary) -> Vec<String> {
            let mut paths = Vec::new();
            let mut cursor = None;
            loop {
                let page = self
                    .manager
                    .inspect(&snapshot.snapshot_id, MAX_PAGE_SIZE, cursor.as_ref())
                    .await
                    .unwrap();
                paths.extend(page.files.iter().map(|file| file.path.as_str().to_owned()));
                cursor = page.next_cursor;
                if cursor.is_none() {
                    return paths;
                }
            }
        }

        fn journals(&self) -> usize {
            lock(&self.manager.inner.state).journals.len()
        }
    }

    fn id(value: &str) -> Identifier {
        Identifier::new(value).unwrap()
    }

    fn limits() -> SnapshotCaptureLimits {
        SnapshotCaptureLimits {
            max_files: u32::try_from(MAX_SNAPSHOT_FILES).unwrap(),
            max_file_bytes: MAX_SNAPSHOT_FILE_BYTES,
            max_total_bytes: MAX_SNAPSHOT_TOTAL_BYTES,
        }
    }

    fn private_directory() -> TempDir {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(
            directory.path(),
            fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE),
        )
        .unwrap();
        directory
    }

    fn write_private(path: &Path, contents: &[u8]) {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(PRIVATE_FILE_MODE)
            .open(path)
            .unwrap()
            .write_all(contents)
            .unwrap();
    }

    async fn open_manager(
        workspace: &Path,
        storage: &Path,
        exclusions: &[PathBuf],
    ) -> Result<SnapshotManager, SnapshotError> {
        let files = FileToolGroup::new(workspace, true, None).await.unwrap();
        SnapshotManager::open(files.workspace_snapshot_access(), storage, exclusions).await
    }

    fn samples(snapshot: &SnapshotSummary) -> Vec<(&str, SnapshotSkipReason)> {
        snapshot
            .skipped
            .samples
            .iter()
            .map(|sample| (sample.path.as_str(), sample.reason))
            .collect()
    }

    fn state(status: &SnapshotRestoreStatus) -> (SnapshotRestoreState, u32, u32, bool, bool) {
        (
            status.state,
            status.applied_files,
            status.total_files,
            status.acknowledgement_required,
            status.reconciliation_required,
        )
    }

    #[tokio::test]
    async fn bound_snapshot_stores_are_isolated_by_workspace_binding() {
        let workspace = tempfile::tempdir().unwrap();
        let storage = private_directory();
        fs::write(workspace.path().join("file.txt"), "content").unwrap();
        let files = FileToolGroup::new(workspace.path(), true, None)
            .await
            .unwrap();
        let first_binding = id("workspace_generation_a");
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
                &id("checkpoint"),
                &WorkspacePath::new(ROOT).unwrap(),
                &limits(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let second = SnapshotManager::open_bound(
            files.workspace_snapshot_access(),
            storage.path(),
            &[],
            &id("workspace_generation_b"),
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
    async fn a_checkpoint_keeps_naming_its_first_capture() {
        let fixture = Fixture::new().await;
        fixture.write("file.txt", "one");
        let first = fixture
            .try_capture("checkpoint", ROOT, &limits())
            .await
            .unwrap();
        fixture.write("file.txt", "two");
        let second = fixture
            .try_capture("checkpoint", ROOT, &limits())
            .await
            .unwrap();

        assert!(!first.reused_checkpoint);
        assert!(second.reused_checkpoint);
        assert_eq!(first.snapshot, second.snapshot);
        assert_eq!(
            fixture
                .manager
                .inspect(&first.snapshot.snapshot_id, 1, None)
                .await
                .unwrap()
                .files[0]
                .resource_id,
            root_relative_resource_id(RootResourceKind::Path, "file.txt").unwrap()
        );
    }

    #[tokio::test]
    async fn equal_content_has_one_snapshot_identity_across_checkpoints() {
        let fixture = Fixture::new().await;
        fixture.write("file.txt", "one");
        let first = fixture.capture("first").await;
        let second = fixture.capture("second").await;

        assert_eq!(first.snapshot_id, second.snapshot_id);
        assert_eq!(first.created_at_unix_ms, second.created_at_unix_ms);
        assert_eq!(fixture.stored(MANIFESTS), 1);
        assert_eq!(fixture.stored(CHECKPOINTS), 2);
    }

    #[tokio::test]
    #[ignore = "local 20,000-file snapshot persistence benchmark"]
    async fn capture_persistence_benchmark() {
        let fixture = Fixture::new().await;
        for index in 0..BENCHMARK_FILES {
            fixture.write(
                &format!("dir-{}/file-{index}", index % BENCHMARK_DIRECTORIES),
                &format!("{index:0width$}", width = BENCHMARK_FILE_BYTES),
            );
        }
        let started = Instant::now();
        let first = fixture.capture("first").await;
        let first_elapsed = started.elapsed();
        let started = Instant::now();
        let unchanged = fixture.capture("unchanged").await;
        eprintln!(
            "files={} bytes_per_file={} first_seconds={:.3} unchanged_seconds={:.3}",
            BENCHMARK_FILES,
            BENCHMARK_FILE_BYTES,
            first_elapsed.as_secs_f64(),
            started.elapsed().as_secs_f64()
        );
        assert_eq!(first.snapshot_id, unchanged.snapshot_id);
        assert_eq!(first.file_count as usize, BENCHMARK_FILES);
        assert_eq!(
            first.total_bytes,
            (BENCHMARK_FILES * BENCHMARK_FILE_BYTES) as u64
        );
        assert_eq!(fixture.stored(BLOBS), BENCHMARK_FILES);
    }

    #[tokio::test]
    async fn a_checkpoint_references_only_complete_durable_batches_including_the_final_partial_batch()
     {
        let mut fixture = Fixture::new().await;
        for index in 0..=MAX_BLOB_BATCH_FILES {
            fixture.write(
                &format!("unique-{index}"),
                &format!("{index:0width$}", width = BENCHMARK_FILE_BYTES),
            );
        }
        let summary = fixture.capture("batches").await;
        assert_eq!(summary.file_count as usize, MAX_BLOB_BATCH_FILES + 1);
        fixture.reopen().await;
        let manifest = fixture
            .manager
            .inner
            .load_manifest(summary.snapshot_id.as_str())
            .unwrap();
        for entry in manifest.content.entries {
            assert_eq!(
                fixture
                    .manager
                    .inner
                    .store
                    .read_blob(&entry.digest, entry.size_bytes)
                    .unwrap()
                    .len(),
                BENCHMARK_FILE_BYTES
            );
        }
        assert_eq!(
            fs::read_dir(fixture.manager.inner.store.directory(BLOBS))
                .unwrap()
                .count(),
            MAX_BLOB_BATCH_FILES + 1
        );
    }

    #[tokio::test]
    async fn batch_sync_link_and_cancellation_failures_clean_new_content_without_touching_receipts()
    {
        for cancel in [false, true] {
            for sync in [false, true] {
                let fixture = Fixture::new().await;
                fixture.write("original.txt", "original");
                let original = fixture.capture("original").await;
                for index in 0..MAX_BLOB_BATCH_FILES + 3 {
                    fixture.write(&format!("new-{index}"), &index.to_string());
                }
                let store = &fixture.manager.inner.store;
                let faults = &store.faults;
                let calls = if sync {
                    &faults.blob_sync_calls
                } else {
                    &faults.blob_link_calls
                };
                let trigger = match (cancel, sync) {
                    (false, false) => &faults.blob_link_failure,
                    (false, true) => &faults.blob_sync_failure,
                    (true, false) => &faults.cancel_after_blob_link,
                    (true, true) => &faults.cancel_after_blob_sync,
                };
                trigger.store(
                    calls.load(Ordering::SeqCst) + MAX_BLOB_BATCH_FILES + 2,
                    Ordering::SeqCst,
                );
                let failed = fixture
                    .try_capture("failed-batch", ROOT, &limits())
                    .await
                    .unwrap_err();
                assert_eq!(
                    failed,
                    if cancel {
                        SnapshotError::Cancelled
                    } else {
                        SnapshotError::OperationFailed
                    }
                );
                for directory in [BLOBS, MANIFESTS, CHECKPOINTS] {
                    assert_eq!(fs::read_dir(store.directory(directory)).unwrap().count(), 1);
                }
                let scope = WorkspacePath::new(ROOT).unwrap();
                assert_eq!(
                    fixture
                        .manager
                        .checkpoint(&id("failed-batch"), &scope)
                        .await
                        .unwrap_err(),
                    SnapshotError::NotFound
                );
                assert_eq!(
                    fixture
                        .manager
                        .checkpoint(&id("original"), &scope)
                        .await
                        .unwrap()
                        .snapshot,
                    original
                );
                trigger.store(0, Ordering::SeqCst);
                let recovered = fixture.capture("failed-batch").await;
                assert_eq!(recovered.file_count as usize, MAX_BLOB_BATCH_FILES + 4);
                let manifest = fixture
                    .manager
                    .inner
                    .load_manifest(recovered.snapshot_id.as_str())
                    .unwrap();
                for entry in manifest.content.entries {
                    store.read_blob(&entry.digest, entry.size_bytes).unwrap();
                }
            }
        }
    }

    #[tokio::test]
    async fn checkpoint_lookup_recovers_only_the_original_published_scope_after_restart() {
        let mut fixture = Fixture::new().await;
        fixture.write("file.txt", "original");
        let checkpoint = id("receipt");
        let scope = WorkspacePath::new(ROOT).unwrap();
        assert_eq!(
            fixture
                .manager
                .checkpoint(&checkpoint, &scope)
                .await
                .unwrap_err(),
            SnapshotError::NotFound
        );
        assert_eq!(fixture.stored(CHECKPOINTS), 0);
        let original = fixture.capture(checkpoint.as_str()).await;
        fixture.write("file.txt", "changed after lost reply");
        fixture.reopen().await;
        let receipt = fixture
            .manager
            .checkpoint(&checkpoint, &scope)
            .await
            .unwrap();
        assert_eq!(receipt.snapshot, original);
        assert!(receipt.reused_checkpoint);
        fixture.write("other/file.txt", "different scope");
        let other = WorkspacePath::new("other").unwrap();
        assert_eq!(
            fixture
                .manager
                .checkpoint(&checkpoint, &other)
                .await
                .unwrap_err(),
            SnapshotError::InvalidRequest
        );
        assert_eq!(
            fixture
                .try_capture(checkpoint.as_str(), "other", &limits())
                .await
                .unwrap_err(),
            SnapshotError::InvalidRequest
        );
        assert_eq!(
            fixture
                .manager
                .checkpoint(&checkpoint, &scope)
                .await
                .unwrap()
                .snapshot,
            original
        );
    }

    #[tokio::test]
    async fn uncertain_publication_and_failed_rollback_preserve_sources_until_receipt_sync_succeeds()
     {
        let mut fixture = Fixture::new().await;
        fixture.write("file.txt", "original");
        let original = fixture.capture("original").await;
        fixture.write("file.txt", "interrupted");
        let store = &fixture.manager.inner.store;
        store.faults.checkpoint_sync.store(true, Ordering::SeqCst);
        store.faults.checkpoint_remove.store(true, Ordering::SeqCst);
        let failed = fixture.try_capture("interrupted", ROOT, &limits()).await;
        for directory in [BLOBS, MANIFESTS, CHECKPOINTS] {
            assert_eq!(fixture.stored(directory), 2);
        }
        assert_eq!(failed.unwrap_err(), SnapshotError::RollbackFailed);
        let checkpoint = id("interrupted");
        let scope = WorkspacePath::new(ROOT).unwrap();
        let stored = fixture
            .manager
            .inner
            .read_checkpoint(&store.checkpoint_path(checkpoint.as_str()))
            .unwrap();
        let expected = fixture
            .manager
            .inner
            .load_manifest(&stored.snapshot_id)
            .unwrap()
            .summary(Some(checkpoint.as_str()))
            .unwrap();
        assert_eq!(
            fixture
                .manager
                .checkpoint(&checkpoint, &scope)
                .await
                .unwrap_err(),
            SnapshotError::OperationFailed
        );
        assert_eq!(
            store.remove_temporaries().unwrap_err(),
            SnapshotError::OperationFailed
        );
        store.faults.checkpoint_sync.store(false, Ordering::SeqCst);
        let receipt = fixture
            .manager
            .checkpoint(&checkpoint, &scope)
            .await
            .unwrap();
        assert_eq!(receipt.snapshot, expected);
        fixture.reopen().await;
        assert_eq!(
            fixture
                .manager
                .checkpoint(&checkpoint, &scope)
                .await
                .unwrap()
                .snapshot,
            expected
        );
        assert_eq!(
            fixture
                .manager
                .checkpoint(&id("original"), &scope)
                .await
                .unwrap()
                .snapshot,
            original
        );
    }

    #[tokio::test]
    async fn prepared_capture_refuses_replaced_directory_after_admission_but_accepts_content_changes()
     {
        for replace in [false, true] {
            let fixture = Fixture::new().await;
            fixture.write("sub/original.txt", "original");
            let path = WorkspacePath::new("sub").unwrap();
            let scope = fixture
                .manager
                .inner
                .workspace
                .snapshot_scope(&path)
                .await
                .unwrap();
            let prepared = fixture
                .manager
                .prepare_capture(&id("bound"), &scope, &limits())
                .unwrap();
            let admission = fixture.manager.inner.publication.lock().await;
            let token = CancellationToken::new();
            let execution = fixture.manager.execute_capture(&prepared, &token, None);
            tokio::pin!(execution);
            std::future::poll_fn(|cx| {
                assert!(execution.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            if replace {
                fs::rename(fixture.path("sub"), fixture.path("moved")).unwrap();
            }
            fixture.write("sub/new.txt", "new");
            drop(admission);
            let result = execution.await;
            if replace {
                assert_eq!(result.unwrap_err(), SnapshotError::Conflict);
                assert_eq!(fixture.stored(CHECKPOINTS), 0);
                assert_eq!(fixture.stored(BLOBS), 0);
            } else {
                assert_eq!(result.unwrap().snapshot.file_count, 2);
            }
        }
    }

    #[tokio::test]
    async fn checkpoint_reuse_refuses_lowered_limits_without_replacing_the_receipt() {
        let fixture = Fixture::new().await;
        fixture.write("first.txt", "original");
        fixture.write("second.txt", "original");
        let original = fixture.capture("limits").await;
        for lowered in [
            SnapshotCaptureLimits {
                max_files: 1,
                ..limits()
            },
            SnapshotCaptureLimits {
                max_total_bytes: 1,
                ..limits()
            },
            SnapshotCaptureLimits {
                max_file_bytes: 1,
                ..limits()
            },
        ] {
            assert!(fixture.try_capture("limits", ROOT, &lowered).await.is_err());
        }
        assert_eq!(fixture.capture("limits").await, original);
    }

    #[tokio::test]
    async fn dropping_the_capture_task_cancels_the_blocking_worker_even_without_its_timer() {
        let fixture = Fixture::new().await;
        fixture.write("file.txt", "must roll back");
        let (release, receiver) = mpsc::channel();
        let barrier = Arc::new(CaptureBarrier {
            entered: Notify::new(),
            release: Mutex::new(receiver),
            phase: SnapshotCapturePhase::Publishing,
        });
        let manager = fixture.manager.clone();
        let scope = manager
            .inner
            .workspace
            .snapshot_scope(&WorkspacePath::new(ROOT).unwrap())
            .await
            .unwrap();
        let prepared = manager
            .prepare_capture(&id("dropped"), &scope, &limits())
            .unwrap();
        let progress = barrier.clone();
        let task = tokio::spawn(async move {
            manager
                .execute_capture(&prepared, &CancellationToken::new(), Some(progress))
                .await
        });
        barrier.entered.notified().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(fixture.manager.inner.capture.try_lock().is_err());
        release.send(()).unwrap();
        let _settled = fixture.manager.inner.capture.lock().await;
        for directory in [BLOBS, MANIFESTS, CHECKPOINTS] {
            assert_eq!(fixture.stored(directory), 0);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn capture_cancellation_and_budget_wait_for_worker_rollback_before_releasing_locks() {
        for expire in [false, true] {
            let fixture = Fixture::new().await;
            fixture.write("file.txt", "captured before cancellation");
            let checkpoint = id("barrier");
            let scope = WorkspacePath::new(ROOT).unwrap();
            let admission = fixture.manager.inner.capture.lock().await;
            let bound = fixture
                .manager
                .inner
                .workspace
                .snapshot_scope(&scope)
                .await
                .unwrap();
            let prepared = fixture
                .manager
                .prepare_capture(&checkpoint, &bound, &limits())
                .unwrap();
            assert_eq!(fixture.stored(BLOBS), 0);
            assert_eq!(fixture.stored(CHECKPOINTS), 0);
            drop(admission);
            let (release, receiver) = mpsc::channel();
            let barrier = Arc::new(CaptureBarrier {
                entered: Notify::new(),
                release: Mutex::new(receiver),
                phase: SnapshotCapturePhase::Publishing,
            });
            let manager = fixture.manager.clone();
            let token = CancellationToken::new();
            let execution_token = token.clone();
            let progress = barrier.clone();
            let task = tokio::spawn(async move {
                manager
                    .execute_capture(&prepared, &execution_token, Some(progress))
                    .await
            });
            barrier.entered.notified().await;
            assert_eq!(fixture.stored(BLOBS), 1);
            assert_eq!(
                fixture
                    .manager
                    .checkpoint(&checkpoint, &scope)
                    .await
                    .unwrap_err(),
                SnapshotError::Busy
            );
            if expire {
                tokio::time::advance(CAPTURE_EXECUTION_BUDGET).await;
            } else {
                token.cancel();
            }
            assert!(!task.is_finished());
            assert!(fixture.manager.inner.capture.try_lock().is_err());
            assert!(fixture.manager.inner.publication.try_lock().is_err());
            let workspace = fixture.manager.inner.workspace.capture_guard();
            tokio::pin!(workspace);
            std::future::poll_fn(|cx| {
                assert!(workspace.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            release.send(()).unwrap();
            assert_eq!(
                task.await.unwrap().unwrap_err(),
                if expire {
                    SnapshotError::TimedOut
                } else {
                    SnapshotError::Cancelled
                }
            );
            for directory in [BLOBS, MANIFESTS, CHECKPOINTS] {
                assert_eq!(fixture.stored(directory), 0);
            }
            assert!(fixture.manager.inner.capture.try_lock().is_ok());
            assert!(fixture.manager.inner.publication.try_lock().is_ok());
            drop(workspace.await);
            assert_eq!(
                fixture
                    .manager
                    .checkpoint(&checkpoint, &scope)
                    .await
                    .unwrap_err(),
                SnapshotError::NotFound
            );
            fixture.capture(checkpoint.as_str()).await;
        }
    }

    #[tokio::test]
    async fn cancellation_after_durable_capture_keeps_the_successful_receipt() {
        let fixture = Fixture::new().await;
        fixture.write("file.txt", "published");
        let (release, receiver) = mpsc::channel();
        let barrier = Arc::new(CaptureBarrier {
            entered: Notify::new(),
            release: Mutex::new(receiver),
            phase: SnapshotCapturePhase::Finished,
        });
        let manager = fixture.manager.clone();
        let checkpoint = id("published");
        let scope = WorkspacePath::new(ROOT).unwrap();
        let bound = manager
            .inner
            .workspace
            .snapshot_scope(&scope)
            .await
            .unwrap();
        let prepared = manager
            .prepare_capture(&checkpoint, &bound, &limits())
            .unwrap();
        let token = CancellationToken::new();
        let execution_token = token.clone();
        let progress = barrier.clone();
        let task = tokio::spawn(async move {
            manager
                .execute_capture(&prepared, &execution_token, Some(progress))
                .await
        });
        barrier.entered.notified().await;
        assert_eq!(fixture.stored(CHECKPOINTS), 1);
        token.cancel();
        release.send(()).unwrap();
        let result = task.await.unwrap().unwrap();
        assert_eq!(
            fixture
                .manager
                .checkpoint(&checkpoint, &scope)
                .await
                .unwrap()
                .snapshot,
            result.snapshot
        );
    }

    #[tokio::test]
    async fn concurrent_captures_of_one_checkpoint_wait_and_reuse() {
        let fixture = Fixture::new().await;
        let checkpoint = id("concurrent");
        let scope = WorkspacePath::new(ROOT).unwrap();
        let limits = limits();
        let token = CancellationToken::new();
        let publication = fixture.manager.inner.publication.lock().await;
        let first = fixture
            .manager
            .capture(&checkpoint, &scope, &limits, &token);
        let second = fixture
            .manager
            .capture(&checkpoint, &scope, &limits, &token);
        tokio::pin!(first, second);
        std::future::poll_fn(|cx| {
            assert!(first.as_mut().poll(cx).is_pending());
            assert!(second.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        drop(publication);
        let (first, second) = tokio::join!(first, second);
        let (first, second) = (first.unwrap(), second.unwrap());

        assert_ne!(first.reused_checkpoint, second.reused_checkpoint);
        assert_eq!(first.snapshot.snapshot_id, second.snapshot.snapshot_id);
    }

    #[tokio::test(start_paused = true)]
    async fn capture_admission_is_cancellable_and_bounded_at_every_lock() {
        for held_lock in ["capture", "publication", "workspace"] {
            for cancel in [true, false] {
                let fixture = Fixture::new().await;
                let manager = &fixture.manager;
                let checkpoint = id("waiting");
                let scope = WorkspacePath::new(ROOT).unwrap();
                let limits = limits();
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
                let pending = manager.capture(&checkpoint, &scope, &limits, &token);
                tokio::pin!(pending);
                std::future::poll_fn(|cx| {
                    assert!(pending.as_mut().poll(cx).is_pending());
                    Poll::Ready(())
                })
                .await;
                let expected = if cancel {
                    token.cancel();
                    SnapshotError::Cancelled
                } else {
                    tokio::time::advance(CAPTURE_EXECUTION_BUDGET).await;
                    SnapshotError::TimedOut
                };

                assert_eq!(pending.await.unwrap_err(), expected);
                assert!(
                    manager
                        .inner
                        .load_checkpoint(checkpoint.as_str())
                        .unwrap()
                        .is_none()
                );
                drop((capture, publication, workspace));
                assert!(manager.inner.capture.try_lock().is_ok());
                assert!(manager.inner.publication.try_lock().is_ok());
                manager
                    .capture(&checkpoint, &scope, &limits, &CancellationToken::new())
                    .await
                    .unwrap();
            }
        }
    }

    #[tokio::test]
    async fn a_cancelled_capture_publishes_nothing() {
        let fixture = Fixture::new().await;
        fixture.write("file.txt", "content");
        let token = CancellationToken::new();
        token.cancel();

        assert_eq!(
            fixture
                .manager
                .capture(
                    &id("cancelled"),
                    &WorkspacePath::new(ROOT).unwrap(),
                    &limits(),
                    &token,
                )
                .await
                .unwrap_err(),
            SnapshotError::Cancelled
        );
        for directory in [BLOBS, MANIFESTS, CHECKPOINTS] {
            assert_eq!(fixture.stored(directory), 0);
        }
    }

    #[tokio::test]
    async fn a_capture_past_a_client_limit_names_the_limit_and_keeps_nothing() {
        let fixture = Fixture::new().await;
        fixture.write("a", "abc");
        fixture.write("b", "def");
        let refusals = [
            (
                SnapshotCaptureLimits {
                    max_files: 1,
                    ..limits()
                },
                SnapshotError::LimitExceeded {
                    limit: SnapshotLimit::Files,
                    maximum: Some(1),
                },
            ),
            (
                SnapshotCaptureLimits {
                    max_total_bytes: 5,
                    ..limits()
                },
                SnapshotError::LimitExceeded {
                    limit: SnapshotLimit::TotalBytes,
                    maximum: Some(5),
                },
            ),
        ];

        for (limits, refusal) in refusals {
            assert_eq!(
                fixture
                    .try_capture("limited", ROOT, &limits)
                    .await
                    .unwrap_err(),
                refusal
            );
            for directory in [BLOBS, MANIFESTS, CHECKPOINTS] {
                assert_eq!(fixture.stored(directory), 0);
            }
        }
    }

    #[tokio::test]
    async fn a_capture_refused_by_the_storage_quota_removes_the_blobs_it_stored() {
        let fixture = Fixture::new().await;
        let first = "first file";
        fixture.write("a", first);
        fixture.write("b", "second file");
        let filler = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(PRIVATE_FILE_MODE)
            .open(fixture.storage.path().join(JOURNALS).join("filler"))
            .unwrap();
        filler
            .set_len(MAX_SNAPSHOT_STORAGE_BYTES - u64::try_from(first.len()).unwrap())
            .unwrap();

        assert_eq!(
            fixture
                .try_capture("over-quota", ROOT, &limits())
                .await
                .unwrap_err(),
            SnapshotError::QuotaExceeded {
                limit: SnapshotLimit::StorageBytes,
                maximum: Some(MAX_SNAPSHOT_STORAGE_BYTES),
            }
        );
        assert_eq!(fixture.stored(BLOBS), 0);
    }

    #[tokio::test]
    async fn a_store_holding_the_most_checkpoints_refuses_another_and_names_the_quota() {
        let fixture = Fixture::new().await;
        for index in 0..MAX_SNAPSHOT_COUNT {
            fixture.capture(&format!("checkpoint-{index}")).await;
        }

        assert_eq!(
            fixture
                .try_capture("one-more", ROOT, &limits())
                .await
                .unwrap_err(),
            SnapshotError::QuotaExceeded {
                limit: SnapshotLimit::Checkpoints,
                maximum: Some(u64::try_from(MAX_SNAPSHOT_COUNT).unwrap()),
            }
        );
        assert!(
            fixture
                .try_capture("checkpoint-0", ROOT, &limits())
                .await
                .unwrap()
                .reused_checkpoint
        );
    }

    #[tokio::test]
    async fn an_oversized_file_is_skipped_and_left_alone_by_a_restore() {
        let fixture = Fixture::new().await;
        let limits = SnapshotCaptureLimits {
            max_file_bytes: 4,
            ..limits()
        };
        fixture.write("grown", "abc");
        fixture.write("small", "abc");
        let before = fixture
            .try_capture("before", ROOT, &limits)
            .await
            .unwrap()
            .snapshot;
        fixture.write("grown", "0123456789");
        fixture.write("small", "xyz");
        let after = fixture
            .try_capture("after", ROOT, &limits)
            .await
            .unwrap()
            .snapshot;

        assert_eq!(after.skipped.oversized_files, 1);
        assert_eq!(samples(&after), [("grown", SnapshotSkipReason::Oversized)]);
        assert_eq!(fixture.paths(&after).await, ["small"]);
        let (prepared, preview) = fixture.prepare(&before, &after).await.unwrap();
        assert_eq!(
            preview.counts,
            SnapshotChangeCounts {
                replace: 1,
                ..SnapshotChangeCounts::default()
            }
        );
        fixture.execute(&prepared).await.unwrap();
        assert_eq!(fixture.read("small"), "abc");
        assert_eq!(fixture.read("grown"), "0123456789");
    }

    #[tokio::test]
    async fn oversized_link_targets_are_skipped_and_identical_checkpoint_retries_succeed() {
        let fixture = Fixture::new().await;
        symlink("abc", fixture.path("oversized-link")).unwrap();
        symlink("a", fixture.path("bounded-link")).unwrap();
        let limits = SnapshotCaptureLimits {
            max_file_bytes: 1,
            ..limits()
        };
        let first = fixture
            .try_capture("link-limits", ROOT, &limits)
            .await
            .unwrap();
        let retry = fixture
            .try_capture("link-limits", ROOT, &limits)
            .await
            .unwrap();
        assert!(!first.reused_checkpoint);
        assert!(retry.reused_checkpoint);
        assert_eq!(first.snapshot, retry.snapshot);
        assert_eq!(first.snapshot.skipped.oversized_files, 1);
        assert_eq!(first.snapshot.file_count, 1);
        let entries = fixture
            .manager
            .inspect(&first.snapshot.snapshot_id, MAX_PAGE_SIZE, None)
            .await
            .unwrap()
            .files;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path.as_str(), "bounded-link");
        assert_eq!(entries[0].kind, SnapshotEntryKind::Symlink);
        assert_eq!(entries[0].size_bytes, limits.max_file_bytes);
    }

    #[tokio::test]
    async fn a_link_is_captured_as_the_link_itself() {
        let fixture = Fixture::new().await;
        fixture.write("AGENTS.md", "instructions");
        symlink("AGENTS.md", fixture.path("CLAUDE.md")).unwrap();
        symlink("/nonexistent/target", fixture.path("dangling")).unwrap();
        let snapshot = fixture.capture("links").await;
        let files = fixture
            .manager
            .inspect(&snapshot.snapshot_id, MAX_PAGE_SIZE, None)
            .await
            .unwrap()
            .files;

        assert_eq!(
            files
                .iter()
                .map(|file| (file.path.as_str(), file.kind))
                .collect::<Vec<_>>(),
            [
                ("AGENTS.md", SnapshotEntryKind::File),
                ("CLAUDE.md", SnapshotEntryKind::Symlink),
                ("dangling", SnapshotEntryKind::Symlink),
            ]
        );
        assert_eq!(files[1].digest.as_str(), digest_bytes(b"AGENTS.md"));
        assert_eq!(files[1].size_bytes, 9);
        assert_eq!(files[1].mode, manifest::SYMLINK_MODE);
        assert_eq!(snapshot.skipped, SnapshotSkipped::default());
    }

    #[tokio::test]
    async fn a_restore_recreates_links_and_never_writes_through_one() {
        let fixture = Fixture::new().await;
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret");
        fs::write(&secret, "secret").unwrap();
        fixture.write("config", "original");
        symlink("AGENTS.md", fixture.path("CLAUDE.md")).unwrap();
        let before = fixture.capture("before").await;
        fs::remove_file(fixture.path("config")).unwrap();
        symlink(&secret, fixture.path("config")).unwrap();
        fs::remove_file(fixture.path("CLAUDE.md")).unwrap();
        symlink("README.md", fixture.path("CLAUDE.md")).unwrap();
        let after = fixture.capture("after").await;

        fixture.restore(&before, &after).await;

        assert_eq!(
            fs::read_link(fixture.path("CLAUDE.md")).unwrap(),
            Path::new("AGENTS.md")
        );
        assert!(
            fs::symlink_metadata(fixture.path("config"))
                .unwrap()
                .is_file()
        );
        assert_eq!(fixture.read("config"), "original");
        assert_eq!(fs::read_to_string(&secret).unwrap(), "secret");
    }

    #[tokio::test]
    async fn special_files_are_skipped_with_their_reason() {
        let fixture = Fixture::new().await;
        fixture.write("file.txt", "content");
        let _socket = UnixListener::bind(fixture.path("socket")).unwrap();
        mknodat(
            CWD,
            fixture.path("fifo").as_path(),
            FileType::Fifo,
            Mode::from_raw_mode(PRIVATE_FILE_MODE),
            0,
        )
        .unwrap();
        let snapshot = fixture.capture("special").await;

        assert_eq!(snapshot.skipped.special_files, 2);
        assert_eq!(
            samples(&snapshot),
            [
                ("fifo", SnapshotSkipReason::Special),
                ("socket", SnapshotSkipReason::Special),
            ]
        );
        assert_eq!(fixture.paths(&snapshot).await, ["file.txt"]);
    }

    #[tokio::test]
    async fn a_name_that_is_not_utf8_is_counted_and_never_recorded() {
        let fixture = Fixture::new().await;
        fs::write(
            fixture
                .workspace
                .path()
                .join(OsStr::from_bytes(b"invalid-\xff")),
            "content",
        )
        .unwrap();
        fixture.write("valid", "content");
        let snapshot = fixture.capture("names").await;

        assert_eq!(snapshot.skipped.unrepresentable_names, 1);
        assert_eq!(
            samples(&snapshot),
            [("invalid-\u{fffd}", SnapshotSkipReason::Unrepresentable)]
        );
        assert_eq!(fixture.paths(&snapshot).await, ["valid"]);
    }

    #[tokio::test]
    async fn a_nested_repository_is_skipped_and_never_touched_by_a_restore() {
        let fixture = Fixture::new().await;
        fixture.write(".git/HEAD", "ref: refs/heads/main\n");
        fixture.write("vendor/lib/.git/HEAD", "ref: refs/heads/main\n");
        fixture.write("vendor/lib/src.rs", "one");
        fixture.write("main.rs", "one");
        let before = fixture.capture("before").await;
        fixture.write("vendor/lib/src.rs", "two");
        fixture.write("main.rs", "two");
        let after = fixture.capture("after").await;

        assert_eq!(after.skipped.nested_repositories, 1);
        assert_eq!(
            samples(&after),
            [("vendor/lib", SnapshotSkipReason::NestedRepository)]
        );
        assert_eq!(fixture.paths(&after).await, ["main.rs"]);
        fixture.restore(&before, &after).await;
        assert_eq!(fixture.read("main.rs"), "one");
        assert_eq!(fixture.read("vendor/lib/src.rs"), "two");
    }

    #[tokio::test]
    async fn ignored_entries_are_left_out_of_a_capture() {
        let fixture = Fixture::new().await;
        fixture.write(".gitignore", "target/\n*.log\n");
        fixture.write("target/debug/app", "binary");
        fixture.write("run.log", "log");
        fixture.write("src/.gitignore", "generated.rs\n");
        fixture.write("src/generated.rs", "generated");
        fixture.write("src/lib.rs", "code");
        let snapshot = fixture.capture("ignored").await;

        assert_eq!(
            fixture.paths(&snapshot).await,
            [".gitignore", "src/.gitignore", "src/lib.rs"]
        );
        assert_eq!(snapshot.skipped, SnapshotSkipped::default());
    }

    #[tokio::test]
    async fn a_scope_that_is_not_a_plain_directory_is_refused() {
        let fixture = Fixture::new().await;
        fixture.write("real/file.txt", "content");
        symlink("real", fixture.path("linked")).unwrap();

        for scope in ["missing", "linked", "real/file.txt"] {
            assert_eq!(
                fixture
                    .try_capture("scoped", scope, &limits())
                    .await
                    .unwrap_err(),
                SnapshotError::UnsupportedFile
            );
        }
    }

    #[tokio::test]
    async fn a_restore_changes_only_the_deeper_of_its_two_scopes() {
        let fixture = Fixture::new().await;
        fixture.write("app/main.rs", "one");
        fixture.write("notes.md", "one");
        let root = fixture.capture("root").await;
        let app = fixture.capture_scope("app", "app").await;
        fixture.write("app/main.rs", "two");
        fixture.write("notes.md", "two");
        let app_after = fixture.capture_scope("app-after", "app").await;

        assert_eq!(app.scope.as_str(), "app");
        assert_eq!(fixture.paths(&app).await, ["app/main.rs"]);
        let (prepared, _) = fixture.prepare(&root, &app_after).await.unwrap();
        assert_eq!(prepared.scope(), "app");
        let status = fixture.execute(&prepared).await.unwrap();
        assert_eq!(fixture.read("app/main.rs"), "one");
        assert_eq!(fixture.read("notes.md"), "two");
        fixture
            .manager
            .acknowledge(&status.restore_id)
            .await
            .unwrap();
        fixture.write("docs/guide.md", "guide");
        let docs = fixture.capture_scope("docs", "docs").await;
        assert_eq!(
            fixture.prepare(&docs, &app_after).await.err(),
            Some(SnapshotError::InvalidRequest)
        );
    }

    #[tokio::test]
    async fn a_restore_publishes_exactly_the_differences_between_its_two_captures() {
        let fixture = Fixture::new().await;
        fixture.write("deleted", "one");
        fixture.write("edited", "one");
        fixture.write("kept", "same");
        fixture.write("script", "run");
        fs::set_permissions(fixture.path("script"), fs::Permissions::from_mode(0o755)).unwrap();
        let before = fixture.capture("before").await;
        fs::remove_file(fixture.path("deleted")).unwrap();
        fixture.write("edited", "two");
        fixture.write("created/new", "new");
        fs::set_permissions(fixture.path("script"), fs::Permissions::from_mode(0o644)).unwrap();
        let after = fixture.capture("after").await;
        fixture.write("untracked", "later");

        let (prepared, preview) = fixture.prepare(&before, &after).await.unwrap();
        assert_eq!(
            preview.counts,
            SnapshotChangeCounts {
                create: 1,
                replace: 2,
                delete: 1,
                ..SnapshotChangeCounts::default()
            }
        );
        let status = fixture.execute(&prepared).await.unwrap();
        assert_eq!(
            state(&status),
            (SnapshotRestoreState::Completed, 4, 4, true, false)
        );
        assert_eq!(fixture.read("deleted"), "one");
        assert_eq!(fixture.read("edited"), "one");
        assert_eq!(fixture.read("kept"), "same");
        assert_eq!(
            fs::metadata(fixture.path("script"))
                .unwrap()
                .permissions()
                .mode()
                & manifest::PERMISSION_BITS,
            0o755
        );
        assert!(!fixture.path("created/new").exists());
        assert_eq!(fixture.read("untracked"), "later");
    }

    #[tokio::test]
    async fn a_restore_never_overwrites_an_edit_made_after_it_was_prepared() {
        let fixture = Fixture::new().await;
        fixture.write("file.txt", "before");
        let before = fixture.capture("before").await;
        fixture.write("file.txt", "after");
        let after = fixture.capture("after").await;
        let (prepared, preview) = fixture.prepare(&before, &after).await.unwrap();
        fixture.write("file.txt", "edited later");

        assert_eq!(preview.counts.replace, 1);
        assert_eq!(
            fixture.execute(&prepared).await.unwrap_err(),
            SnapshotError::Conflict
        );
        assert_eq!(fixture.read("file.txt"), "edited later");
        assert_eq!(fixture.journals(), 0);
        assert_eq!(fixture.stored(JOURNALS), 0);
    }

    #[tokio::test]
    async fn an_entry_matching_neither_capture_is_a_conflict_that_blocks_the_restore() {
        let fixture = Fixture::new().await;
        fixture.write("file.txt", "before");
        let before = fixture.capture("before").await;
        fixture.write("file.txt", "after");
        let after = fixture.capture("after").await;
        fixture.write("file.txt", "edited later");

        let (prepared, preview) = fixture.prepare(&before, &after).await.unwrap();
        assert_eq!(
            preview.counts,
            SnapshotChangeCounts {
                conflict: 1,
                ..SnapshotChangeCounts::default()
            }
        );
        assert_eq!(preview.changes[0].kind, SnapshotChangeKind::Conflict);
        assert_eq!(
            preview.changes[0]
                .current_revision
                .as_ref()
                .map(Revision::as_str),
            Some(digest_bytes(b"edited later").as_str())
        );
        assert_eq!(
            fixture.execute(&prepared).await.unwrap_err(),
            SnapshotError::Conflict
        );
        assert_eq!(fixture.read("file.txt"), "edited later");
    }

    #[tokio::test]
    async fn a_directory_where_the_target_has_a_file_is_a_conflict() {
        let fixture = Fixture::new().await;
        fixture.write("entry", "file");
        let before = fixture.capture("before").await;
        fs::remove_file(fixture.path("entry")).unwrap();
        fixture.write("entry/child", "child");
        let after = fixture.capture("after").await;

        let (prepared, preview) = fixture.prepare(&before, &after).await.unwrap();
        assert_eq!(
            preview.counts,
            SnapshotChangeCounts {
                delete: 1,
                conflict: 1,
                ..SnapshotChangeCounts::default()
            }
        );
        assert_eq!(
            fixture.execute(&prepared).await.unwrap_err(),
            SnapshotError::Conflict
        );
        assert_eq!(fixture.read("entry/child"), "child");
    }

    #[tokio::test]
    async fn a_restore_creates_only_the_missing_ancestor_directories() {
        let fixture = Fixture::new().await;
        fixture.write("one/kept", "kept");
        fixture.write("one/two/three/file.txt", "before");
        let before = fixture.capture("before").await;
        fs::remove_dir_all(fixture.path("one/two")).unwrap();
        let after = fixture.capture("after").await;

        let (prepared, preview) = fixture.prepare(&before, &after).await.unwrap();
        assert_eq!(
            preview
                .created_directories
                .iter()
                .map(WorkspacePath::as_str)
                .collect::<Vec<_>>(),
            ["one/two", "one/two/three"]
        );
        assert_eq!(
            preview.counts,
            SnapshotChangeCounts {
                create: 1,
                created_directories: 2,
                ..SnapshotChangeCounts::default()
            }
        );
        fixture.execute(&prepared).await.unwrap();
        assert_eq!(fixture.read("one/two/three/file.txt"), "before");
    }

    #[tokio::test]
    async fn a_cancelled_restore_changes_nothing() {
        let fixture = Fixture::new().await;
        fixture.write("file.txt", "before");
        let before = fixture.capture("before").await;
        fixture.write("file.txt", "after");
        let after = fixture.capture("after").await;
        let (prepared, _) = fixture.prepare(&before, &after).await.unwrap();
        let token = CancellationToken::new();
        token.cancel();

        assert_eq!(
            fixture
                .manager
                .execute_restore(&prepared, &token)
                .await
                .unwrap_err(),
            SnapshotError::Cancelled
        );
        assert_eq!(fixture.read("file.txt"), "after");
        assert_eq!(fixture.journals(), 0);
    }

    #[tokio::test]
    async fn a_restore_refused_midway_reports_what_it_published_and_awaits_a_decision() {
        let fixture = Fixture::new().await;
        fixture.write("a", "one");
        fixture.write("b", "one");
        let before = fixture.capture("before").await;
        fixture.write("a", "two");
        fixture.write("b", "two");
        let after = fixture.capture("after").await;
        let (prepared, _) = fixture.prepare(&before, &after).await.unwrap();
        fixture.write("b", "edited later");

        let status = fixture.execute(&prepared).await.unwrap();
        assert_eq!(
            state(&status),
            (SnapshotRestoreState::Partial, 1, 2, true, false)
        );
        assert_eq!(fixture.read("a"), "one");
        assert_eq!(fixture.read("b"), "edited later");
        assert_eq!(
            fixture.prepare(&after, &before).await.err(),
            Some(SnapshotError::AcknowledgementRequired)
        );
    }

    #[tokio::test]
    async fn an_unrevert_restores_the_source_and_settles_the_original_as_reverted() {
        let fixture = Fixture::new().await;
        fixture.write("file.txt", "before");
        let before = fixture.capture("before").await;
        fixture.write("file.txt", "after");
        let after = fixture.capture("after").await;
        let restored = fixture.restore(&before, &after).await;

        assert_eq!(fixture.read("file.txt"), "before");
        assert_eq!(
            fixture.prepare(&after, &before).await.err(),
            Some(SnapshotError::AcknowledgementRequired)
        );
        let (unrevert, preview) = fixture
            .manager
            .prepare_unrevert(&restored.restore_id, usize::MAX, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            (&preview.target_snapshot_id, &preview.source_snapshot_id),
            (&after.snapshot_id, &before.snapshot_id)
        );
        let unreverted = fixture.execute(&unrevert).await.unwrap();
        assert_eq!(fixture.read("file.txt"), "after");
        assert_eq!(unreverted.unrevert_of.as_ref(), Some(&restored.restore_id));
        assert_eq!(
            state(
                &fixture
                    .manager
                    .status(&restored.restore_id)
                    .unwrap()
                    .restore
            ),
            (SnapshotRestoreState::Reverted, 1, 1, false, false)
        );
        assert_eq!(
            fixture
                .manager
                .acknowledge(&unreverted.restore_id)
                .await
                .unwrap()
                .restore
                .state,
            SnapshotRestoreState::Acknowledged
        );
        assert!(fixture.prepare(&before, &after).await.is_ok());
    }

    #[tokio::test]
    async fn a_restore_interrupted_by_a_crash_reopens_reconciled_against_the_workspace() {
        let mut fixture = Fixture::new().await;
        fixture.write("a", "one");
        fixture.write("b", "one");
        let before = fixture.capture("before").await;
        fixture.write("a", "two");
        fixture.write("b", "two");
        let after = fixture.capture("after").await;
        let restored = fixture.restore(&before, &after).await;
        let journal = fixture
            .storage
            .path()
            .join(JOURNALS)
            .join(format!("{}.json", restored.restore_id.as_str()));
        let interrupt = || {
            let mut started: serde_json::Value =
                serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
            started["state"] = "publishing".into();
            started["applied_files"] = 0.into();
            fs::write(&journal, serde_json::to_vec(&started).unwrap()).unwrap();
        };

        fixture.write("b", "two");
        interrupt();
        fixture.reopen().await;
        assert_eq!(
            state(
                &fixture
                    .manager
                    .status(&restored.restore_id)
                    .unwrap()
                    .restore
            ),
            (SnapshotRestoreState::Partial, 1, 2, true, false)
        );
        fixture.write("b", "edited later");
        interrupt();
        fixture.reopen().await;
        assert_eq!(
            state(
                &fixture
                    .manager
                    .status(&restored.restore_id)
                    .unwrap()
                    .restore
            ),
            (SnapshotRestoreState::Indeterminate, 0, 2, true, true)
        );
    }

    #[tokio::test]
    async fn restores_that_change_nothing_leave_bounded_queryable_journals() {
        let mut fixture = Fixture::new().await;
        fixture.write("file.txt", "content");
        let snapshot = fixture.capture("no-op").await;
        let mut latest = None;
        for _ in 0..=MAX_SNAPSHOT_JOURNALS {
            let status = fixture.restore(&snapshot, &snapshot).await;
            assert_eq!(
                state(&status),
                (SnapshotRestoreState::Completed, 0, 0, false, false)
            );
            latest = Some(status.restore_id);
        }

        assert!(fixture.stored(JOURNALS) <= MAX_SNAPSHOT_JOURNALS);
        fixture.reopen().await;
        let latest = latest.unwrap();
        assert_eq!(
            fixture.manager.status(&latest).unwrap().restore.restore_id,
            latest
        );
    }

    #[tokio::test]
    async fn tampered_blobs_and_manifests_fail_integrity_verification() {
        let fixture = Fixture::new().await;
        fixture.write("file.txt", "before");
        let before = fixture.capture("before").await;
        fixture.write("file.txt", "after");
        let after = fixture.capture("after").await;
        let blob = fixture
            .manager
            .inner
            .store
            .blob_path(&digest_bytes(b"before"))
            .unwrap();
        fs::write(blob, "tampered").unwrap();
        let (prepared, _) = fixture.prepare(&before, &after).await.unwrap();

        assert_eq!(
            fixture.execute(&prepared).await.unwrap_err(),
            SnapshotError::IntegrityFailure
        );
        assert_eq!(fixture.read("file.txt"), "after");
        let manifest = fixture
            .manager
            .inner
            .store
            .manifest_path(before.snapshot_id.as_str())
            .unwrap();
        fs::write(manifest, "{}").unwrap();
        assert_eq!(
            fixture
                .manager
                .inspect(&before.snapshot_id, 1, None)
                .await
                .unwrap_err(),
            SnapshotError::IntegrityFailure
        );
    }

    #[tokio::test]
    async fn a_checkpoint_an_earlier_host_captured_stays_restorable() {
        let fixture = Fixture::new().await;
        let legacy = "legacy";
        let digest = digest_bytes(legacy.as_bytes());
        let resource_id = path_resource_id("file.txt").unwrap();
        let content = format!(
            r#"{{"version":"workspace-snapshot.v1","files":[{{"path":"file.txt","resource_id":"{}","identity":"identity","revision":"{digest}","digest":"{digest}","mode":420,"size_bytes":6}}],"exclusions":[".git"]}}"#,
            resource_id.as_str()
        );
        let snapshot_id = format!("{SNAPSHOT_ID_PREFIX}{}", hex_sha256(content.as_bytes()));
        let store = &fixture.manager.inner.store;
        let manifest = store.manifest_path(&snapshot_id).unwrap();
        write_private(
            &manifest,
            format!(
                r#"{{"snapshot_id":"{snapshot_id}","created_at_unix_ms":1,"content":{content}}}"#
            )
            .as_bytes(),
        );
        write_private(&store.blob_path(&digest).unwrap(), legacy.as_bytes());
        write_private(
            &store.checkpoint_path("legacy"),
            &serde_json::to_vec(&StoredCheckpoint {
                version: CHECKPOINT_VERSION.to_owned(),
                checkpoint_id: "legacy".to_owned(),
                snapshot_id: snapshot_id.clone(),
            })
            .unwrap(),
        );
        fixture.write("file.txt", "current");
        let current = fixture.capture("current").await;
        let captured = fixture
            .try_capture("legacy", ROOT, &limits())
            .await
            .unwrap();

        assert!(captured.reused_checkpoint);
        assert_eq!(captured.snapshot.snapshot_id.as_str(), snapshot_id);
        fixture.restore(&captured.snapshot, &current).await;
        assert_eq!(fixture.read("file.txt"), legacy);
        let tampered = fs::read_to_string(&manifest)
            .unwrap()
            .replace(r#""mode":420"#, r#""mode":493"#);
        fs::write(&manifest, tampered).unwrap();
        assert_eq!(
            fixture
                .manager
                .inspect(&captured.snapshot.snapshot_id, 1, None)
                .await
                .unwrap_err(),
            SnapshotError::IntegrityFailure
        );
    }

    #[tokio::test]
    async fn cleanup_deletes_checkpoints_and_collects_only_what_no_other_checkpoint_reaches() {
        let fixture = Fixture::new().await;
        fixture.write("shared", "shared");
        fixture.write("file.txt", "one");
        let first = fixture.capture("first").await;
        fixture.write("file.txt", "two");
        let second = fixture.capture("second").await;

        let (prepared, preview) = fixture
            .manager
            .prepare_cleanup(&[id("first"), id("missing")], usize::MAX)
            .await
            .unwrap();
        assert_eq!(preview.checkpoint_ids, [id("first")]);
        assert_eq!(preview.missing_checkpoint_ids, [id("missing")]);
        let result = fixture
            .manager
            .execute_cleanup(&prepared, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(result.deleted_checkpoint_ids, [id("first")]);
        assert_eq!((result.deleted_snapshots, result.deleted_blobs), (1, 1));
        assert_eq!(fixture.stored(BLOBS), 2);
        assert_eq!(
            fixture
                .manager
                .inspect(&first.snapshot_id, 1, None)
                .await
                .unwrap_err(),
            SnapshotError::NotFound
        );
        assert_eq!(fixture.paths(&second).await, ["file.txt", "shared"]);
    }

    #[tokio::test]
    async fn cleanup_keeps_every_snapshot_a_pending_or_undecided_restore_reads() {
        let fixture = Fixture::new().await;
        fixture.write("file.txt", "before");
        let before = fixture.capture("before").await;
        fixture.write("file.txt", "after");
        let after = fixture.capture("after").await;
        let (prepared, _) = fixture.prepare(&before, &after).await.unwrap();

        assert_eq!(
            fixture
                .cleanup(&["before", "after"])
                .await
                .deleted_snapshots,
            0
        );
        let status = fixture.execute(&prepared).await.unwrap();
        drop(prepared);
        assert_eq!(fixture.read("file.txt"), "before");
        assert_eq!(fixture.cleanup(&[]).await.deleted_snapshots, 0);
        fixture
            .manager
            .acknowledge(&status.restore_id)
            .await
            .unwrap();
        let result = fixture.cleanup(&[]).await;
        assert_eq!((result.deleted_snapshots, result.deleted_blobs), (2, 2));
        assert_eq!(fixture.stored(JOURNALS), 0);
    }

    #[tokio::test]
    async fn cleanup_refuses_a_deletion_set_the_store_would_no_longer_plan() {
        let fixture = Fixture::new().await;
        fixture.write("file.txt", "content");
        let first = fixture.capture("first").await;
        let (prepared, _) = fixture
            .manager
            .prepare_cleanup(&[id("first")], usize::MAX)
            .await
            .unwrap();
        fixture.capture("later").await;

        assert_eq!(
            fixture
                .manager
                .execute_cleanup(&prepared, &CancellationToken::new())
                .await
                .unwrap_err(),
            SnapshotError::Conflict
        );
        assert!(
            fixture
                .manager
                .inspect(&first.snapshot_id, 1, None)
                .await
                .is_ok()
        );
        assert_eq!(fixture.stored(CHECKPOINTS), 2);
    }

    #[tokio::test]
    async fn private_storage_rejects_overlap_symlinks_and_shared_permissions() {
        let workspace = tempfile::tempdir().unwrap();
        let inside = workspace.path().join("snapshots");
        fs::create_dir(&inside).unwrap();
        fs::set_permissions(&inside, fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE)).unwrap();
        let shared = tempfile::tempdir().unwrap();
        fs::set_permissions(shared.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let parent = tempfile::tempdir().unwrap();
        let target = private_directory();
        let linked = parent.path().join("linked-storage");
        symlink(target.path(), &linked).unwrap();

        for storage in [inside.as_path(), shared.path(), linked.as_path()] {
            assert_eq!(
                open_manager(workspace.path(), storage, &[]).await.err(),
                Some(SnapshotError::InvalidConfiguration)
            );
        }
    }

    #[tokio::test]
    async fn startup_removes_abandoned_temporaries() {
        let mut fixture = Fixture::new().await;
        for directory in [BLOBS, MANIFESTS, CHECKPOINTS, JOURNALS] {
            write_private(
                &fixture
                    .storage
                    .path()
                    .join(directory)
                    .join(".abandoned.tmp"),
                b"partial",
            );
        }

        fixture.reopen().await;
        for directory in [BLOBS, MANIFESTS, CHECKPOINTS, JOURNALS] {
            assert_eq!(fixture.stored(directory), 0);
        }
    }

    #[tokio::test]
    async fn later_created_configured_exclusions_remain_excluded() {
        let workspace = tempfile::tempdir().unwrap();
        let storage = private_directory();
        let excluded = workspace.path().join("generated/private");
        let manager = open_manager(
            workspace.path(),
            storage.path(),
            std::slice::from_ref(&excluded),
        )
        .await
        .unwrap();
        fs::create_dir_all(&excluded).unwrap();
        fs::write(excluded.join("secret"), "secret").unwrap();
        fs::write(workspace.path().join("visible"), "visible").unwrap();
        let capture = manager
            .capture(
                &id("exclusion"),
                &WorkspacePath::new(ROOT).unwrap(),
                &limits(),
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
        let storage = private_directory();
        let outside = tempfile::tempdir().unwrap();

        assert_eq!(
            open_manager(
                workspace.path(),
                storage.path(),
                &[outside.path().join("later")]
            )
            .await
            .err(),
            Some(SnapshotError::InvalidConfiguration)
        );
        assert_eq!(
            configured_exclusions(workspace.path(), &[PathBuf::from("safe/../escape")])
                .unwrap_err(),
            SnapshotError::InvalidConfiguration
        );
    }
}
