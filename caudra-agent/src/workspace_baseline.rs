//! The workspace snapshot a revert needs, captured the first time a tool call
//! might change a file.
//!
//! Capturing before a run instead meant every conversational turn walked and
//! hashed the working tree for a revert point nothing would ever use. The per
//! call effect already says which calls can change the tree, so the capture
//! moves behind the first of them: a session that only reads leaves no store on
//! disk at all, and one that writes still cannot touch a file before the state
//! it is about to overwrite has been recorded.

use std::collections::BTreeSet;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use arc_swap::{ArcSwap, ArcSwapOption};
use async_lock::Mutex;
use caudra_config::SnapshotsConfig;
use caudra_storage::StateDir;
use caudra_storage::id::CaudraId;
use caudra_storage::remote_snapshots::{
    RemoteRestoreKind, RemoteRestoreRecord, RemoteSnapshotMetadata, RemoteSnapshotMetadataError,
    RemoteSnapshotMetadataStore,
};
use caudra_storage::workspace_binding::StoredWorkspaceBinding;
use caudra_workspace::{
    CheckpointId, OperationState, PreparedSnapshotOperation, RestoreId, SnapshotCaptureLimits,
    SnapshotCaptureRequest, SnapshotChangeKind, SnapshotOperationPreview, SnapshotOperationResult,
    SnapshotRestorePreview, SnapshotRestoreState, SnapshotRestoreStatus, SnapshotState,
    WorkspaceCapability, WorkspaceError, WorkspacePath, WorkspaceSession,
    WorkspaceSnapshotMutationService, WorkspaceSnapshotReadService,
};
use event_listener::Event;
use futures_lite::future;
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use crate::snapshots::{SnapshotError, SnapshotStore};

const SNAPSHOTS_DISABLED: &str = "workspace snapshots are off in your configuration";
const CHECKPOINT_DOMAIN: &[u8] = b"caudra.remote-checkpoint.v1\0";
/// Captures a remote session keeps besides its session start and current head.
/// The host's store is shared by every session on the machine, and a capture
/// stays there as a checkpoint until the session that took it deletes it.
const REMOTE_RECENT_CHECKPOINTS: usize = 32;
/// Retention waits for this many prunable checkpoints before it deletes any:
/// every cleanup walks the host's whole store to collect what nothing names.
const REMOTE_PRUNE_BATCH: usize = 16;

#[derive(Debug, thiserror::Error)]
pub enum BaselineError {
    #[error(transparent)]
    Local(#[from] SnapshotError),
    #[error("remote workspace snapshot failed: {0}")]
    Workspace(#[from] WorkspaceError),
    /// The host declined this workspace, and asking again gets the same answer.
    #[error("remote workspace snapshot refused: {0}")]
    Refused(WorkspaceError),
    #[error(transparent)]
    Metadata(#[from] RemoteSnapshotMetadataError),
    #[error("remote workspace snapshot is corrupt")]
    CorruptSnapshot,
    #[error("no remote workspace snapshot is available to restore")]
    NoSnapshot,
    #[error("remote workspace mutation is blocked until snapshot recovery is resolved")]
    RecoveryRequired,
    #[error(
        "remote workspace restore was not executed: {}",
        conflict_summary(*.conflicts, .sample.as_ref())
    )]
    RestoreConflict {
        conflicts: u32,
        sample: Option<WorkspacePath>,
    },
    /// The host refused the restore before it changed anything.
    #[error("remote workspace restore failed: {message} ({code})")]
    RestoreFailed { code: String, message: String },
    #[error("remote workspace restore was cancelled before it changed anything")]
    RestoreCancelled,
    #[error("remote workspace snapshot cleanup did not complete")]
    CleanupIncomplete,
    #[error("remote workspace restore response is inconsistent")]
    InvalidRestore,
    #[error("workspace snapshot capture is busy; final snapshot skipped")]
    CaptureBusy,
    #[error("workspace snapshot target changed; final snapshot skipped")]
    TargetChanged,
}

impl BaselineError {
    /// Whether the restore this ended left the workspace as it was, so there is
    /// nothing to recover or acknowledge.
    pub fn left_workspace_unchanged(&self) -> bool {
        matches!(self, Self::RestoreFailed { .. } | Self::RestoreCancelled)
    }
}

fn conflict_summary(conflicts: u32, sample: Option<&WorkspacePath>) -> String {
    let paths = if conflicts == 1 { "path" } else { "paths" };
    match sample {
        Some(path) => format!("{conflicts} {paths} changed since last captured, including {path}"),
        None => format!("{conflicts} {paths} changed since last captured"),
    }
}

/// A refusal of the workspace itself turns file revert off; anything else is a
/// failure that must not let a change through without its revert point.
fn capture_error(error: WorkspaceError) -> BaselineError {
    match error {
        WorkspaceError::LimitExceeded { .. }
        | WorkspaceError::QuotaExceeded { .. }
        | WorkspaceError::UnsupportedEntry
        | WorkspaceError::UnsupportedCapability { .. } => BaselineError::Refused(error),
        error => BaselineError::Workspace(error),
    }
}

/// Refuses to run a restore with conflicts, naming how many and one of them.
fn reject_conflicts(preview: &SnapshotRestorePreview) -> Result<(), BaselineError> {
    if preview.counts.conflict == 0 {
        return Ok(());
    }
    Err(BaselineError::RestoreConflict {
        conflicts: preview.counts.conflict,
        sample: preview
            .changes
            .iter()
            .find(|change| change.kind == SnapshotChangeKind::Conflict)
            .map(|change| change.path.clone()),
    })
}

/// The same ceilings a local capture observes, which the host lowers to its own.
fn capture_limits(config: &SnapshotsConfig) -> SnapshotCaptureLimits {
    SnapshotCaptureLimits {
        max_files: config.max_files,
        max_file_bytes: config.max_file_bytes,
        max_total_bytes: config.max_bytes,
    }
}

fn snapshot_reader(
    workspace: &WorkspaceSession,
    capability: WorkspaceCapability,
) -> Result<&dyn WorkspaceSnapshotReadService, WorkspaceError> {
    workspace
        .workspace()
        .services()
        .snapshot_read
        .as_deref()
        .ok_or(WorkspaceError::UnsupportedCapability { capability })
}

fn snapshot_mutator(
    workspace: &WorkspaceSession,
    capability: WorkspaceCapability,
) -> Result<&dyn WorkspaceSnapshotMutationService, WorkspaceError> {
    workspace
        .workspace()
        .services()
        .snapshot_mutation
        .as_deref()
        .ok_or(WorkspaceError::UnsupportedCapability { capability })
}

/// What a mutating call learns before it runs.
#[derive(Debug)]
pub enum BaselineOutcome {
    /// A revert point for this run exists.
    Ready,
    /// This workspace will not be snapshotted, and the call may proceed anyway.
    /// A deliberate refusal costs file revert, not the user's work.
    Unavailable(Arc<String>),
    /// The capture was attempted and did not finish, or a restore still awaits
    /// recovery. The call must not proceed.
    Failed(BaselineError),
}

impl BaselineOutcome {
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }

    /// Whether the change may go ahead, which only a failure stops.
    pub fn into_result(self) -> Result<(), BaselineError> {
        match self {
            Self::Ready | Self::Unavailable(_) => Ok(()),
            Self::Failed(error) => Err(error),
        }
    }
}

/// The store and worktree a capture would use. Replaced wholesale by `/cd` and
/// by loading another session, because both change where the baseline lives.
enum BaselineTarget {
    Local {
        store: Arc<SnapshotStore>,
        cwd: PathBuf,
    },
    WorkspaceSession {
        workspace: WorkspaceSession,
        metadata: Box<RemoteSnapshotMetadataStore>,
    },
}

impl BaselineTarget {
    fn remote(
        storage: StateDir,
        session_id: CaudraId,
        workspace: WorkspaceSession,
        binding: StoredWorkspaceBinding,
    ) -> Self {
        Self::WorkspaceSession {
            workspace,
            metadata: Box::new(RemoteSnapshotMetadataStore::new(
                storage, session_id, binding,
            )),
        }
    }

    /// Both halves already exist, so there is nothing to capture. Two `exists`
    /// checks locally and one state read remotely, which is what makes calling
    /// this before every mutating call affordable.
    fn is_captured(&self, head: Option<CaudraId>) -> bool {
        match self {
            Self::Local { store, .. } => {
                store.has_session_start() && head.is_none_or(|head| store.has_checkpoint(head))
            }
            Self::WorkspaceSession { metadata, .. } => {
                metadata.capture(head).is_ok_and(|capture| {
                    capture.is_some_and(|capture| capture.state == SnapshotState::Complete)
                })
            }
        }
    }

    /// A reused head keeps the snapshot it already has rather than taking a
    /// fresher one: both were taken while that item was the newest, and the
    /// earlier state is the one that undoes more.
    fn capture_local(&self, head: Option<CaudraId>) -> Result<(), BaselineError> {
        let Self::Local { store, cwd } = self else {
            return Err(BaselineError::InvalidRestore);
        };
        store.snapshot_session_start(cwd)?;
        if let Some(head) = head
            && !store.has_checkpoint(head)
        {
            store.snapshot(cwd, head)?;
        }
        Ok(())
    }

    fn is_remote(&self) -> bool {
        matches!(self, Self::WorkspaceSession { .. })
    }
}

/// One per session, shared with every agent serving it, the way `PathLocks` is.
pub struct WorkspaceBaseline {
    /// From configuration, so it never changes for the life of the process and
    /// `rebind` must not clear it.
    config: SnapshotsConfig,
    target: ArcSwap<BaselineTarget>,
    target_changed: Event,
    /// Single-flights the capture: parallel tool calls and subagents all reach
    /// this, and the second one through must wait rather than start its own.
    gate: Arc<Mutex<()>>,
    /// Sticky, so a workspace the store refused is judged once rather than on
    /// every call.
    unavailable: ArcSwapOption<String>,
    capturing: AtomicBool,
    current_head: ArcSwapOption<CaudraId>,
}

struct Capturing<'a>(&'a AtomicBool);

impl Drop for Capturing<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl WorkspaceBaseline {
    pub fn new(store: Arc<SnapshotStore>, cwd: PathBuf, config: SnapshotsConfig) -> Arc<Self> {
        Self::with_target(BaselineTarget::Local { store, cwd }, config)
    }

    pub fn new_workspace_session(
        storage: StateDir,
        session_id: CaudraId,
        workspace: WorkspaceSession,
        binding: StoredWorkspaceBinding,
        config: SnapshotsConfig,
    ) -> Arc<Self> {
        Self::with_target(
            BaselineTarget::remote(storage, session_id, workspace, binding),
            config,
        )
    }

    fn with_target(target: BaselineTarget, config: SnapshotsConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            target: ArcSwap::from_pointee(target),
            target_changed: Event::new(),
            gate: Arc::new(Mutex::default()),
            unavailable: ArcSwapOption::empty(),
            capturing: AtomicBool::new(false),
            current_head: ArcSwapOption::empty(),
        })
    }

    /// Points the baseline at another store and worktree. The refusal is dropped
    /// with the old target: a new workspace earns its own verdict.
    pub fn rebind(&self, store: Arc<SnapshotStore>, cwd: PathBuf) {
        self.target
            .store(Arc::new(BaselineTarget::Local { store, cwd }));
        self.unavailable.store(None);
        self.target_changed.notify(usize::MAX);
    }

    pub fn rebind_workspace_session(
        &self,
        storage: StateDir,
        session_id: CaudraId,
        workspace: WorkspaceSession,
        binding: StoredWorkspaceBinding,
    ) {
        self.target.store(Arc::new(BaselineTarget::remote(
            storage, session_id, workspace, binding,
        )));
        self.unavailable.store(None);
        self.target_changed.notify(usize::MAX);
    }

    /// Why this workspace has no file revert, whether by configuration or by
    /// a refusal earned on the tree itself.
    pub fn unavailable_reason(&self) -> Option<Arc<String>> {
        if !self.config.enabled {
            return Some(Arc::new(SNAPSHOTS_DISABLED.to_owned()));
        }
        self.unavailable.load_full()
    }

    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// The refusal alone, for the UI to report once. Configuration is not news
    /// worth interrupting anyone over: they chose it.
    pub fn refusal(&self) -> Option<Arc<String>> {
        self.unavailable.load_full()
    }

    /// Whether a capture has landed, so a caller that only wants to refresh an
    /// existing baseline can tell there is nothing to refresh.
    pub fn is_captured(&self) -> bool {
        self.target.load().is_captured(None)
    }

    pub fn cwd(&self) -> PathBuf {
        match &**self.target.load() {
            BaselineTarget::Local { cwd, .. } => cwd.clone(),
            BaselineTarget::WorkspaceSession { .. } => PathBuf::new(),
        }
    }

    pub fn is_remote(&self) -> bool {
        self.target.load().is_remote()
    }

    pub fn is_capturing(&self) -> bool {
        self.capturing.load(Ordering::Acquire)
    }

    pub fn set_current_head(&self, head: Option<CaudraId>) {
        self.current_head.store(head.map(Arc::new));
    }

    pub async fn ensure_current(&self) -> BaselineOutcome {
        self.ensure(self.current_head.load_full().map(|head| *head))
            .await
    }

    pub fn reserve_remote_capture(
        self: &Arc<Self>,
        head: CaudraId,
    ) -> Result<Option<impl Future<Output = BaselineOutcome> + Send + 'static>, BaselineError> {
        let target = self.target.load_full();
        if self.unavailable_reason().is_some()
            || !target.is_remote()
            || !target.is_captured(None)
            || target.is_captured(Some(head))
        {
            return Ok(None);
        }
        let gate = self.gate.try_lock_arc().ok_or(BaselineError::CaptureBusy)?;
        let changed = self.target_changed.listen();
        let baseline = Arc::clone(self);
        Ok(Some(async move {
            let _gate = gate;
            future::or(
                async move {
                    changed.await;
                    BaselineOutcome::Failed(BaselineError::TargetChanged)
                },
                baseline.ensure_target(target, Some(head)),
            )
            .await
        }))
    }

    /// Holds the gate across the capture, so the call that asked second arrives
    /// after the baseline is on disk rather than alongside it.
    pub async fn ensure(&self, head: Option<CaudraId>) -> BaselineOutcome {
        if !self.config.enabled {
            match self.pending_remote_restore() {
                Ok(None) => {
                    return BaselineOutcome::Unavailable(Arc::new(SNAPSHOTS_DISABLED.to_owned()));
                }
                Ok(Some(_)) => {}
                Err(error) => return BaselineOutcome::Failed(error),
            }
        }
        if !self.is_remote()
            && let Some(reason) = self.unavailable_reason()
        {
            return BaselineOutcome::Unavailable(reason);
        }
        let _gate = self.gate.lock().await;
        let target = self.target.load_full();
        self.ensure_target(target, head).await
    }

    async fn ensure_target(
        &self,
        target: Arc<BaselineTarget>,
        head: Option<CaudraId>,
    ) -> BaselineOutcome {
        if !Arc::ptr_eq(&target, &self.target.load_full()) {
            return BaselineOutcome::Failed(BaselineError::TargetChanged);
        }
        // A restore awaiting its verdict gates every change whether or not this
        // workspace can be captured: a change on top of a partial restore would
        // bury what recovery has to inspect.
        if let BaselineTarget::WorkspaceSession {
            workspace,
            metadata,
        } = &*target
            && let Err(error) = acknowledge_completed_before_mutation(workspace, metadata).await
        {
            return BaselineOutcome::Failed(error);
        }
        // Re-read under the gate: whoever held it may have just answered this.
        if let Some(reason) = self.unavailable_reason() {
            return BaselineOutcome::Unavailable(reason);
        }
        if target.is_captured(head) {
            return BaselineOutcome::Ready;
        }
        self.capturing.store(true, Ordering::Release);
        let _capturing = Capturing(&self.capturing);
        let result = match &*target {
            BaselineTarget::Local { .. } => {
                let work = Arc::clone(&target);
                smol::unblock(move || work.capture_local(head)).await
            }
            BaselineTarget::WorkspaceSession {
                workspace,
                metadata,
            } => capture_remote(workspace, metadata, head, &capture_limits(&self.config)).await,
        };
        if !Arc::ptr_eq(&target, &self.target.load_full()) {
            return BaselineOutcome::Failed(BaselineError::TargetChanged);
        }
        match result {
            Ok(()) => BaselineOutcome::Ready,
            Err(BaselineError::Local(error)) if error.is_workspace_refusal() => {
                self.refuse(error.to_string())
            }
            Err(error @ BaselineError::Refused(_)) => self.refuse(error.to_string()),
            Err(error) => BaselineOutcome::Failed(error),
        }
    }

    fn refuse(&self, reason: String) -> BaselineOutcome {
        warn!(
            remote = self.is_remote(),
            cwd = %self.cwd().display(),
            %reason,
            "workspace refused for snapshots, file revert is off"
        );
        let reason = Arc::new(reason);
        self.unavailable.store(Some(Arc::clone(&reason)));
        BaselineOutcome::Unavailable(reason)
    }

    pub fn remote_capture(
        &self,
        head: Option<CaudraId>,
    ) -> Result<Option<RemoteSnapshotMetadata>, BaselineError> {
        let target = self.target.load();
        let BaselineTarget::WorkspaceSession { metadata, .. } = &**target else {
            return Ok(None);
        };
        Ok(metadata.capture(head)?)
    }

    /// Prepares a rewind from the capture nearest the head the workspace now
    /// reflects to the one nearest the target head. Each chain runs from its
    /// head back through the ancestors, so a head never captured, or pruned
    /// since, resolves to the closest earlier capture and last to the session
    /// start. Only paths the two captures disagree on are touched, and any of
    /// them changed since the first was taken is a conflict.
    pub async fn prepare_remote_restore(
        &self,
        source_chain: &[CaudraId],
        target_chain: &[CaudraId],
    ) -> Result<PreparedSnapshotOperation, BaselineError> {
        let target = self.target.load_full();
        let BaselineTarget::WorkspaceSession {
            workspace,
            metadata,
        } = &*target
        else {
            return Err(BaselineError::InvalidRestore);
        };
        acknowledge_completed_before_mutation(workspace, metadata).await?;
        let (Some(restore_to), Some(restore_from)) = (
            metadata.nearest_capture(target_chain)?,
            metadata.nearest_capture(source_chain)?,
        ) else {
            return Err(BaselineError::NoSnapshot);
        };
        let service = snapshot_mutator(workspace, WorkspaceCapability::SnapshotPrepareRestore)?;
        let prepared = service
            .prepare_restore(
                workspace.binding(),
                workspace.cursor(),
                &restore_to.snapshot_id,
                &restore_from.snapshot_id,
            )
            .await?;
        let SnapshotOperationPreview::Restore(preview) = &prepared.preview else {
            return Err(BaselineError::InvalidRestore);
        };
        if preview.target_snapshot_id != restore_to.snapshot_id
            || preview.source_snapshot_id != restore_from.snapshot_id
        {
            return Err(BaselineError::InvalidRestore);
        }
        if let Err(conflict) = reject_conflicts(preview) {
            let _ = service
                .release(workspace.binding(), workspace.cursor(), &prepared)
                .await;
            return Err(conflict);
        }
        Ok(prepared)
    }

    pub async fn execute_remote_restore(
        &self,
        prepared: PreparedSnapshotOperation,
        target_head: Option<CaudraId>,
    ) -> Result<SnapshotRestoreStatus, BaselineError> {
        self.execute_prepared_remote_restore(prepared, RemoteRestoreKind::Rewind, target_head, None)
            .await
    }

    pub async fn release_remote_prepared(
        &self,
        prepared: &PreparedSnapshotOperation,
    ) -> Result<(), BaselineError> {
        let target = self.target.load_full();
        let BaselineTarget::WorkspaceSession { workspace, .. } = &*target else {
            return Err(BaselineError::InvalidRestore);
        };
        snapshot_mutator(workspace, WorkspaceCapability::SnapshotRelease)?
            .release(workspace.binding(), workspace.cursor(), prepared)
            .await?;
        Ok(())
    }

    pub async fn prepare_remote_unrevert(
        &self,
        restore_id: &RestoreId,
    ) -> Result<PreparedSnapshotOperation, BaselineError> {
        let target = self.target.load_full();
        let BaselineTarget::WorkspaceSession {
            workspace,
            metadata,
        } = &*target
        else {
            return Err(BaselineError::InvalidRestore);
        };
        if let Some(pending) = metadata.pending_restore()?
            && (pending.restore_id != *restore_id
                || pending.status.as_ref().is_none_or(|status| {
                    status.state != SnapshotRestoreState::Completed
                        || status.reconciliation_required
                }))
        {
            return Err(BaselineError::RecoveryRequired);
        }
        let service = snapshot_mutator(workspace, WorkspaceCapability::SnapshotPrepareUnrevert)?;
        let prepared = service
            .prepare_unrevert(workspace.binding(), workspace.cursor(), restore_id)
            .await?;
        let SnapshotOperationPreview::Unrevert(preview) = &prepared.preview else {
            return Err(BaselineError::InvalidRestore);
        };
        if preview.source_restore_id != *restore_id {
            return Err(BaselineError::InvalidRestore);
        }
        if let Err(conflict) = reject_conflicts(&preview.restore) {
            let _ = service
                .release(workspace.binding(), workspace.cursor(), &prepared)
                .await;
            return Err(conflict);
        }
        Ok(prepared)
    }

    pub async fn execute_remote_unrevert(
        &self,
        prepared: PreparedSnapshotOperation,
        target_head: Option<CaudraId>,
        source_restore_id: RestoreId,
    ) -> Result<SnapshotRestoreStatus, BaselineError> {
        self.execute_prepared_remote_restore(
            prepared,
            RemoteRestoreKind::Unrevert,
            target_head,
            Some(source_restore_id),
        )
        .await
    }

    /// Records the restore before it runs, so an answer lost on the way back
    /// still leaves something to recover from. A definitive refusal changed
    /// nothing and is forgotten again.
    async fn execute_prepared_remote_restore(
        &self,
        prepared: PreparedSnapshotOperation,
        kind: RemoteRestoreKind,
        target_head: Option<CaudraId>,
        source_restore_id: Option<RestoreId>,
    ) -> Result<SnapshotRestoreStatus, BaselineError> {
        let target = self.target.load_full();
        let BaselineTarget::WorkspaceSession {
            workspace,
            metadata,
        } = &*target
        else {
            return Err(BaselineError::InvalidRestore);
        };
        let preview = match &prepared.preview {
            SnapshotOperationPreview::Restore(preview) => preview,
            SnapshotOperationPreview::Unrevert(preview) => &preview.restore,
            SnapshotOperationPreview::Cleanup(_) => return Err(BaselineError::InvalidRestore),
        };
        let now = unix_millis();
        metadata.begin_restore(RemoteRestoreRecord {
            kind,
            restore_id: preview.restore_id.clone(),
            operation: prepared.operation.clone(),
            target_history_head: target_head,
            target_snapshot_id: preview.target_snapshot_id.clone(),
            source_restore_id,
            status: None,
            acknowledged: false,
            created_at_unix_ms: now,
            updated_at_unix_ms: now,
        })?;
        let result = snapshot_mutator(workspace, WorkspaceCapability::SnapshotExecute)?
            .execute(workspace.binding(), workspace.cursor(), &prepared)
            .await?;
        let status = match result.state {
            OperationState::Completed {
                result: SnapshotOperationResult::Restore(status),
                ..
            } => status,
            OperationState::Failed {
                error,
                side_effects_possible: false,
            } => {
                metadata.forget_restore(&preview.restore_id)?;
                return Err(BaselineError::RestoreFailed {
                    code: error.code.as_str().to_owned(),
                    message: error.message,
                });
            }
            OperationState::Cancelled {
                side_effects_possible: false,
            } => {
                metadata.forget_restore(&preview.restore_id)?;
                return Err(BaselineError::RestoreCancelled);
            }
            OperationState::NeverSeen
            | OperationState::Prepared
            | OperationState::Running
            | OperationState::Forgotten
            | OperationState::Indeterminate { .. }
            | OperationState::Failed { .. }
            | OperationState::Cancelled { .. }
            | OperationState::Completed {
                result: SnapshotOperationResult::Cleanup(_),
                ..
            } => self.remote_restore_status(&preview.restore_id).await?,
        };
        metadata.update_restore_status(status.clone(), unix_millis())?;
        if kind == RemoteRestoreKind::Unrevert
            && matches!(status.state, SnapshotRestoreState::Completed)
            && !status.reconciliation_required
            && let Some(source_restore_id) = metadata
                .restore(&status.restore_id)?
                .and_then(|restore| restore.source_restore_id)
        {
            metadata.acknowledge_restore(&source_restore_id, unix_millis())?;
        }
        Ok(status)
    }

    pub async fn remote_restore_status(
        &self,
        restore_id: &RestoreId,
    ) -> Result<SnapshotRestoreStatus, BaselineError> {
        let target = self.target.load_full();
        let BaselineTarget::WorkspaceSession { workspace, .. } = &*target else {
            return Err(BaselineError::InvalidRestore);
        };
        Ok(
            snapshot_reader(workspace, WorkspaceCapability::SnapshotStatus)?
                .restore_status(workspace.binding(), workspace.cursor(), restore_id)
                .await?,
        )
    }

    pub async fn reconcile_remote_restore(
        &self,
    ) -> Result<Option<SnapshotRestoreStatus>, BaselineError> {
        let target = self.target.load_full();
        let BaselineTarget::WorkspaceSession { metadata, .. } = &*target else {
            return Ok(None);
        };
        let Some(pending) = metadata.pending_restore()? else {
            return Ok(None);
        };
        let status = self.remote_restore_status(&pending.restore_id).await?;
        metadata.update_restore_status(status.clone(), unix_millis())?;
        if pending.kind == RemoteRestoreKind::Unrevert
            && matches!(
                status.state,
                SnapshotRestoreState::Completed | SnapshotRestoreState::Acknowledged
            )
            && !status.reconciliation_required
            && let Some(source_restore_id) = pending.source_restore_id
        {
            metadata.acknowledge_restore(&source_restore_id, unix_millis())?;
        }
        Ok(Some(status))
    }

    pub async fn acknowledge_remote_restore(
        &self,
        restore_id: &RestoreId,
    ) -> Result<SnapshotRestoreStatus, BaselineError> {
        let target = self.target.load_full();
        let BaselineTarget::WorkspaceSession {
            workspace,
            metadata,
        } = &*target
        else {
            return Err(BaselineError::InvalidRestore);
        };
        let status = snapshot_mutator(workspace, WorkspaceCapability::SnapshotAcknowledge)?
            .acknowledge(workspace.binding(), workspace.cursor(), restore_id)
            .await?;
        metadata.update_restore_status(status.clone(), unix_millis())?;
        metadata.acknowledge_restore(restore_id, unix_millis())?;
        Ok(status)
    }

    pub fn mark_remote_restore_acknowledged(
        &self,
        restore_id: &RestoreId,
    ) -> Result<(), BaselineError> {
        let target = self.target.load();
        let BaselineTarget::WorkspaceSession { metadata, .. } = &**target else {
            return Err(BaselineError::InvalidRestore);
        };
        metadata.acknowledge_restore(restore_id, unix_millis())?;
        Ok(())
    }

    pub fn pending_remote_restore(&self) -> Result<Option<RemoteRestoreRecord>, BaselineError> {
        let target = self.target.load();
        let BaselineTarget::WorkspaceSession { metadata, .. } = &**target else {
            return Ok(None);
        };
        Ok(metadata.pending_restore()?)
    }
}

/// A completed restore is accepted before the next change, after which it can
/// no longer be unreverted. Any other open restore stops the change.
async fn acknowledge_completed_before_mutation(
    workspace: &WorkspaceSession,
    metadata: &RemoteSnapshotMetadataStore,
) -> Result<(), BaselineError> {
    let Some(pending) = metadata.pending_restore()? else {
        return Ok(());
    };
    let status = match pending.status {
        Some(status) => status,
        None => {
            snapshot_reader(workspace, WorkspaceCapability::SnapshotStatus)?
                .restore_status(workspace.binding(), workspace.cursor(), &pending.restore_id)
                .await?
        }
    };
    if status.state != SnapshotRestoreState::Completed || status.reconciliation_required {
        return Err(BaselineError::RecoveryRequired);
    }
    let acknowledged = snapshot_mutator(workspace, WorkspaceCapability::SnapshotAcknowledge)?
        .acknowledge(workspace.binding(), workspace.cursor(), &pending.restore_id)
        .await?;
    metadata.update_restore_status(acknowledged, unix_millis())?;
    metadata.acknowledge_restore(&pending.restore_id, unix_millis())?;
    Ok(())
}

/// The session start comes first: it is what a rewind falls back to when no
/// head on its way back was captured.
async fn capture_remote(
    workspace: &WorkspaceSession,
    metadata: &RemoteSnapshotMetadataStore,
    head: Option<CaudraId>,
    limits: &SnapshotCaptureLimits,
) -> Result<(), BaselineError> {
    if head.is_some() && metadata.capture(None)?.is_none() {
        capture_checkpoint(workspace, metadata, None, limits).await?;
    }
    if let Some(capture) = metadata.capture(head)? {
        return match capture.state {
            SnapshotState::Complete => Ok(()),
            SnapshotState::Corrupt => Err(BaselineError::CorruptSnapshot),
        };
    }
    capture_checkpoint(workspace, metadata, head, limits).await
}

/// A full store first gives up this session's older checkpoints and then gets
/// one more try: covering the change about to happen is worth more than a
/// rewind to old work.
async fn capture_checkpoint(
    workspace: &WorkspaceSession,
    metadata: &RemoteSnapshotMetadataStore,
    head: Option<CaudraId>,
    limits: &SnapshotCaptureLimits,
) -> Result<(), BaselineError> {
    let request = SnapshotCaptureRequest {
        checkpoint_id: remote_checkpoint_id(metadata, head)?,
        label: None,
        limits: limits.clone(),
    };
    let service =
        snapshot_reader(workspace, WorkspaceCapability::SnapshotCapture).map_err(capture_error)?;
    let mut captured = service
        .capture(workspace.binding(), workspace.cursor(), &request)
        .await;
    if matches!(captured, Err(WorkspaceError::QuotaExceeded { .. }))
        && make_room(workspace, metadata, head).await
    {
        captured = service
            .capture(workspace.binding(), workspace.cursor(), &request)
            .await;
    }
    let snapshot = captured.map_err(capture_error)?.snapshot;
    if snapshot.checkpoint_id.as_ref() != Some(&request.checkpoint_id) {
        return Err(BaselineError::CorruptSnapshot);
    }
    debug!(
        session_start = head.is_none(),
        files = snapshot.file_count,
        bytes = snapshot.total_bytes,
        skipped = snapshot.skipped.total(),
        nested_repositories = snapshot.skipped.nested_repositories,
        mounts = snapshot.skipped.mounts,
        special_files = snapshot.skipped.special_files,
        oversized_files = snapshot.skipped.oversized_files,
        unreadable_entries = snapshot.skipped.unreadable_entries,
        unstable_files = snapshot.skipped.unstable_files,
        unrepresentable_names = snapshot.skipped.unrepresentable_names,
        "remote workspace snapshot"
    );
    let state = snapshot.state;
    metadata.record_capture(RemoteSnapshotMetadata {
        history_head: head,
        checkpoint_id: request.checkpoint_id,
        snapshot_id: snapshot.snapshot_id,
        manifest_revision: snapshot.manifest_revision,
        state,
        created_at_unix_ms: snapshot.created_at_unix_ms,
    })?;
    match state {
        SnapshotState::Complete => {}
        SnapshotState::Corrupt => return Err(BaselineError::CorruptSnapshot),
    }
    match prune(
        workspace,
        metadata,
        head,
        REMOTE_RECENT_CHECKPOINTS,
        REMOTE_PRUNE_BATCH,
    )
    .await
    {
        Ok(0) => {}
        Ok(deleted) => debug!(deleted, "pruned remote workspace checkpoints"),
        Err(error) => warn!(%error, "remote workspace checkpoint retention failed"),
    }
    Ok(())
}

/// Deletes every checkpoint of this session but the session start and `head`.
/// Answers whether that freed any, which is what makes another try worth it.
async fn make_room(
    workspace: &WorkspaceSession,
    metadata: &RemoteSnapshotMetadataStore,
    head: Option<CaudraId>,
) -> bool {
    match prune(workspace, metadata, head, 0, 1).await {
        Ok(deleted) => deleted > 0,
        Err(error) => {
            warn!(%error, "could not free room for a remote workspace snapshot");
            false
        }
    }
}

/// Deletes this session's checkpoints nothing needs any more once `batch` of
/// them have piled up, as many at a time as the host takes. Answers how many
/// the host no longer holds. A host that takes none keeps everything.
async fn prune(
    workspace: &WorkspaceSession,
    metadata: &RemoteSnapshotMetadataStore,
    head: Option<CaudraId>,
    keep_recent: usize,
    batch: usize,
) -> Result<usize, BaselineError> {
    let prunable = metadata.prunable_checkpoints(&[head], keep_recent)?;
    if prunable.len() < batch {
        return Ok(0);
    }
    let service = snapshot_mutator(workspace, WorkspaceCapability::SnapshotPrepareCleanup)?;
    let chunk = service.max_cleanup_checkpoints();
    if chunk == 0 {
        return Ok(0);
    }
    let mut deleted = 0;
    for checkpoints in prunable.chunks(chunk) {
        let prepared = service
            .prepare_cleanup(workspace.binding(), workspace.cursor(), checkpoints)
            .await?;
        let SnapshotOperationPreview::Cleanup(preview) = &prepared.preview else {
            return Err(BaselineError::InvalidRestore);
        };
        let mut gone = preview
            .missing_checkpoint_ids
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let status = service
            .execute(workspace.binding(), workspace.cursor(), &prepared)
            .await?;
        let OperationState::Completed {
            result: SnapshotOperationResult::Cleanup(result),
            ..
        } = status.state
        else {
            return Err(BaselineError::CleanupIncomplete);
        };
        gone.extend(result.deleted_checkpoint_ids);
        metadata.remove_captures(&gone)?;
        deleted += gone.len();
    }
    Ok(deleted)
}

fn remote_checkpoint_id(
    metadata: &RemoteSnapshotMetadataStore,
    head: Option<CaudraId>,
) -> Result<CheckpointId, BaselineError> {
    let mut hasher = Sha256::new();
    hasher.update(CHECKPOINT_DOMAIN);
    hasher.update(metadata.stable_scope_id().as_bytes());
    match head {
        Some(head) => hasher.update(head.as_bytes()),
        None => hasher.update(b"session-start"),
    }
    let digest = hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            use std::fmt::Write;
            let _ = write!(output, "{byte:02x}");
            output
        });
    CheckpointId::new(format!("caudra-{digest}")).map_err(|_| BaselineError::CorruptSnapshot)
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// The baseline plus the head this run started from, which is the item a revert
/// brackets. Carried by `ToolContext` and inherited by subagents, so a child's
/// first write captures the parent run's revert point.
#[derive(Clone)]
pub struct BaselineGate {
    baseline: Arc<WorkspaceBaseline>,
    head: Option<CaudraId>,
}

impl BaselineGate {
    pub fn new(baseline: Arc<WorkspaceBaseline>, head: Option<CaudraId>) -> Self {
        Self { baseline, head }
    }

    pub async fn ensure(&self) -> BaselineOutcome {
        self.baseline.ensure(self.head).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::fs;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use caudra_workspace::{
        AuthenticatedPrincipalId, AuthorityIdentity, CancellationResult, CwdHandle, OperationError,
        OperationHandle, OperationId, OperationPhase, OperationStatus, ProjectIdentity, ProjectKey,
        ReleaseResult, ResourceId, ResourceRevision, ResourceScope, SequenceMetadata,
        SessionBindingId, SessionWorkspaceBinding, SnapshotCaptureResult, SnapshotChange,
        SnapshotChangeCounts, SnapshotCleanupPreview, SnapshotCleanupResult, SnapshotId,
        SnapshotSkipped, SnapshotSummary, SnapshotUnrevertPreview, SourceTrustAnchor,
        WorkspaceCapabilities, WorkspaceCursor, WorkspaceHandle, WorkspaceServices,
    };
    use futures_lite::future::poll_once;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::snapshots::SnapshotLimits;

    const FILE: &str = "tracked.txt";
    const CONTENTS: &str = "alpha";
    const READY_MSG: &str = "a mutating call gets a revert point";
    const UNAVAILABLE_MSG: &str = "a refused workspace lets the call through";
    const BLOCKED_MSG: &str = "a capture that failed stops the call and says why";
    const RECOVERY_MSG: &str = "an open restore stops every change until it is recovered";
    const RETENTION_MSG: &str = "retention deletes only what the session no longer needs";
    const REWIND_MSG: &str = "a rewind runs between the nearest captures of its two heads";
    const REFUSED_RESTORE_MSG: &str = "a refused restore changed nothing and leaves nothing open";
    const DISABLED_MSG: &str = "snapshots off by configuration capture nothing and report nothing";
    const CONFLICT_PATH: &str = "file.txt";
    const RESTORE_FAILURE_CODE: &str = "conflict";
    const RESTORE_FAILURE_REASON: &str = "a file changed while the restore ran";
    const FILES_LIMIT: &str = "files";
    const STORAGE_LIMIT: &str = "storageBytes";
    const MAX_CLEANUP: usize = 5;

    struct FakeSnapshots {
        capture_calls: AtomicUsize,
        status_calls: AtomicUsize,
        execute_calls: AtomicUsize,
        release_calls: AtomicUsize,
        acknowledge_calls: AtomicUsize,
        capture_errors: Mutex<VecDeque<WorkspaceError>>,
        capture_pause: Mutex<Option<flume::Receiver<()>>>,
        capture_limits: Mutex<Option<SnapshotCaptureLimits>>,
        snapshot_state: Mutex<SnapshotState>,
        restore_state: Mutex<SnapshotRestoreState>,
        conflict: AtomicBool,
        refuse_restores: AtomicBool,
        next_restore: AtomicUsize,
        restores: Mutex<HashMap<String, FakeRestore>>,
        checkpoints: Mutex<BTreeSet<CheckpointId>>,
        cleanup_batches: Mutex<Vec<usize>>,
    }

    #[derive(Clone)]
    struct FakeRestore {
        target: SnapshotId,
        source: SnapshotId,
        unrevert_of: Option<RestoreId>,
    }

    impl Default for FakeSnapshots {
        fn default() -> Self {
            Self {
                capture_calls: AtomicUsize::new(0),
                status_calls: AtomicUsize::new(0),
                execute_calls: AtomicUsize::new(0),
                release_calls: AtomicUsize::new(0),
                acknowledge_calls: AtomicUsize::new(0),
                capture_errors: Mutex::new(VecDeque::new()),
                capture_pause: Mutex::new(None),
                capture_limits: Mutex::new(None),
                snapshot_state: Mutex::new(SnapshotState::Complete),
                restore_state: Mutex::new(SnapshotRestoreState::Completed),
                conflict: AtomicBool::new(false),
                refuse_restores: AtomicBool::new(false),
                next_restore: AtomicUsize::new(1),
                restores: Mutex::new(HashMap::new()),
                checkpoints: Mutex::new(BTreeSet::new()),
                cleanup_batches: Mutex::new(Vec::new()),
            }
        }
    }

    impl FakeSnapshots {
        fn summary(&self, request: &SnapshotCaptureRequest) -> SnapshotSummary {
            SnapshotSummary {
                snapshot_id: SnapshotId::new(format!(
                    "snapshot-{}",
                    request.checkpoint_id.as_str()
                ))
                .unwrap(),
                checkpoint_id: Some(request.checkpoint_id.clone()),
                label: None,
                state: *self.snapshot_state.lock().unwrap(),
                manifest_revision: ResourceRevision::new("manifest-r1").unwrap(),
                scope: WorkspacePath::new(".").unwrap(),
                file_count: 1,
                total_bytes: 5,
                skipped: SnapshotSkipped::default(),
                created_at_unix_ms: 1,
            }
        }

        fn operation(&self) -> OperationHandle {
            let sequence = self.next_restore.fetch_add(1, Ordering::SeqCst);
            OperationHandle {
                preparation_id: OperationId::new(format!("prepare-{sequence}")).unwrap(),
                invocation_id: Some(OperationId::new(format!("invoke-{sequence}")).unwrap()),
                execution_id: Some(OperationId::new(format!("execute-{sequence}")).unwrap()),
                expires_at_unix_ms: None,
            }
        }

        fn preview(
            &self,
            target: &SnapshotId,
            source: &SnapshotId,
            unrevert_of: Option<RestoreId>,
        ) -> SnapshotRestorePreview {
            let sequence = self.next_restore.load(Ordering::SeqCst);
            let restore_id = RestoreId::new(format!("restore-{sequence}")).unwrap();
            self.restores.lock().unwrap().insert(
                restore_id.as_str().to_owned(),
                FakeRestore {
                    target: target.clone(),
                    source: source.clone(),
                    unrevert_of,
                },
            );
            let conflict = self.conflict.load(Ordering::SeqCst);
            SnapshotRestorePreview {
                restore_id,
                target_snapshot_id: target.clone(),
                source_snapshot_id: source.clone(),
                counts: SnapshotChangeCounts {
                    replace: u32::from(!conflict),
                    conflict: u32::from(conflict),
                    ..SnapshotChangeCounts::default()
                },
                changes: vec![SnapshotChange {
                    path: WorkspacePath::new(CONFLICT_PATH).unwrap(),
                    resource_id: ResourceId::new("file").unwrap(),
                    kind: if conflict {
                        SnapshotChangeKind::Conflict
                    } else {
                        SnapshotChangeKind::Replace
                    },
                    current_revision: Some(ResourceRevision::new("current-file").unwrap()),
                    target_revision: Some(ResourceRevision::new("target-file").unwrap()),
                }],
                created_directories: Vec::new(),
            }
        }

        fn status_of(
            &self,
            restore_id: &RestoreId,
        ) -> Result<SnapshotRestoreStatus, WorkspaceError> {
            let FakeRestore {
                target,
                source,
                unrevert_of,
            } = self
                .restores
                .lock()
                .unwrap()
                .get(restore_id.as_str())
                .cloned()
                .ok_or(WorkspaceError::Conflict)?;
            let state = *self.restore_state.lock().unwrap();
            Ok(SnapshotRestoreStatus {
                restore_id: restore_id.clone(),
                state,
                target_snapshot_id: target,
                source_snapshot_id: source,
                applied_files: u32::from(state != SnapshotRestoreState::Publishing),
                total_files: 1,
                acknowledgement_required: state == SnapshotRestoreState::Completed,
                reconciliation_required: matches!(
                    state,
                    SnapshotRestoreState::Partial | SnapshotRestoreState::Indeterminate
                ),
                unrevert_of,
            })
        }
    }

    #[async_trait]
    impl WorkspaceSnapshotReadService for FakeSnapshots {
        async fn capture(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            request: &SnapshotCaptureRequest,
        ) -> Result<SnapshotCaptureResult, WorkspaceError> {
            self.capture_calls.fetch_add(1, Ordering::SeqCst);
            let pause = self.capture_pause.lock().unwrap().clone();
            if let Some(pause) = pause {
                pause.recv_async().await.unwrap();
            }
            *self.capture_limits.lock().unwrap() = Some(request.limits.clone());
            if let Some(error) = self.capture_errors.lock().unwrap().pop_front() {
                return Err(error);
            }
            self.checkpoints
                .lock()
                .unwrap()
                .insert(request.checkpoint_id.clone());
            Ok(SnapshotCaptureResult {
                snapshot: self.summary(request),
                reused_checkpoint: false,
            })
        }

        async fn inspect(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            _request: &caudra_workspace::SnapshotInspectRequest,
        ) -> Result<caudra_workspace::SnapshotInspectPage, WorkspaceError> {
            Err(WorkspaceError::UnsupportedCapability {
                capability: WorkspaceCapability::SnapshotInspect,
            })
        }

        async fn restore_status(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            restore_id: &RestoreId,
        ) -> Result<SnapshotRestoreStatus, WorkspaceError> {
            self.status_calls.fetch_add(1, Ordering::SeqCst);
            self.status_of(restore_id)
        }
    }

    #[async_trait]
    impl WorkspaceSnapshotMutationService for FakeSnapshots {
        fn max_cleanup_checkpoints(&self) -> usize {
            MAX_CLEANUP
        }

        async fn prepare_restore(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            target: &SnapshotId,
            source: &SnapshotId,
        ) -> Result<PreparedSnapshotOperation, WorkspaceError> {
            let operation = self.operation();
            Ok(PreparedSnapshotOperation {
                preview: SnapshotOperationPreview::Restore(self.preview(target, source, None)),
                operation,
            })
        }

        async fn prepare_unrevert(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            restore_id: &RestoreId,
        ) -> Result<PreparedSnapshotOperation, WorkspaceError> {
            let original = self.status_of(restore_id)?;
            let operation = self.operation();
            Ok(PreparedSnapshotOperation {
                preview: SnapshotOperationPreview::Unrevert(SnapshotUnrevertPreview {
                    source_restore_id: restore_id.clone(),
                    restore: self.preview(
                        &original.source_snapshot_id,
                        &original.target_snapshot_id,
                        Some(restore_id.clone()),
                    ),
                }),
                operation,
            })
        }

        async fn prepare_cleanup(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            checkpoint_ids: &[CheckpointId],
        ) -> Result<PreparedSnapshotOperation, WorkspaceError> {
            self.cleanup_batches
                .lock()
                .unwrap()
                .push(checkpoint_ids.len());
            let held = self.checkpoints.lock().unwrap();
            let (present, missing) = checkpoint_ids
                .iter()
                .cloned()
                .partition(|checkpoint| held.contains(checkpoint));
            Ok(PreparedSnapshotOperation {
                operation: self.operation(),
                preview: SnapshotOperationPreview::Cleanup(SnapshotCleanupPreview {
                    checkpoint_ids: present,
                    missing_checkpoint_ids: missing,
                    reclaimable_bytes: 1,
                }),
            })
        }

        async fn execute(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            prepared: &PreparedSnapshotOperation,
        ) -> Result<OperationStatus<SnapshotOperationResult>, WorkspaceError> {
            self.execute_calls.fetch_add(1, Ordering::SeqCst);
            let restore_id = match &prepared.preview {
                SnapshotOperationPreview::Restore(preview) => &preview.restore_id,
                SnapshotOperationPreview::Unrevert(preview) => &preview.restore.restore_id,
                SnapshotOperationPreview::Cleanup(preview) => {
                    self.checkpoints
                        .lock()
                        .unwrap()
                        .retain(|checkpoint| !preview.checkpoint_ids.contains(checkpoint));
                    return Ok(operation_status(
                        prepared.operation.clone(),
                        OperationState::Completed {
                            result: SnapshotOperationResult::Cleanup(SnapshotCleanupResult {
                                deleted_checkpoint_ids: preview.checkpoint_ids.clone(),
                                deleted_snapshots: 1,
                                deleted_blobs: 1,
                                reclaimed_bytes: 1,
                            }),
                            side_effects_possible: true,
                        },
                    ));
                }
            };
            let state = if self.refuse_restores.load(Ordering::SeqCst) {
                OperationState::Failed {
                    error: OperationError {
                        code: OperationId::new(RESTORE_FAILURE_CODE).unwrap(),
                        message: RESTORE_FAILURE_REASON.to_owned(),
                    },
                    side_effects_possible: false,
                }
            } else {
                OperationState::Completed {
                    result: SnapshotOperationResult::Restore(self.status_of(restore_id)?),
                    side_effects_possible: true,
                }
            };
            Ok(operation_status(prepared.operation.clone(), state))
        }

        async fn operation_status(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            _operation: &OperationHandle,
        ) -> Result<OperationStatus<SnapshotOperationResult>, WorkspaceError> {
            Err(WorkspaceError::IndeterminateOutcome)
        }

        async fn cancel(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            _operation: &OperationHandle,
        ) -> Result<CancellationResult, WorkspaceError> {
            Ok(CancellationResult {
                state: OperationPhase::Cancelled,
                cancellation_requested: true,
            })
        }

        async fn acknowledge(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            restore_id: &RestoreId,
        ) -> Result<SnapshotRestoreStatus, WorkspaceError> {
            self.acknowledge_calls.fetch_add(1, Ordering::SeqCst);
            let mut status = self.status_of(restore_id)?;
            status.state = SnapshotRestoreState::Acknowledged;
            status.acknowledgement_required = false;
            Ok(status)
        }

        async fn release(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            _prepared: &PreparedSnapshotOperation,
        ) -> Result<ReleaseResult, WorkspaceError> {
            self.release_calls.fetch_add(1, Ordering::SeqCst);
            Ok(ReleaseResult {
                state: OperationPhase::Cancelled,
                released: true,
            })
        }
    }

    fn operation_status<T>(
        handle: OperationHandle,
        state: OperationState<T>,
    ) -> OperationStatus<T> {
        OperationStatus {
            handle,
            state,
            progress: Vec::new(),
            progress_metadata: SequenceMetadata {
                first_retained_sequence: None,
                next_sequence: 0,
                gap_before_first: false,
            },
        }
    }

    fn remote_baseline(
        temp: &TempDir,
        service: Arc<FakeSnapshots>,
        config: SnapshotsConfig,
    ) -> Arc<WorkspaceBaseline> {
        let authority = AuthorityIdentity::new(
            SourceTrustAnchor::new("test-source").unwrap(),
            "authority",
            "workspace",
            "generation",
            "namespace",
        )
        .unwrap();
        let principal = AuthenticatedPrincipalId::new(authority.clone(), "principal").unwrap();
        let project = ProjectIdentity::new(authority.clone(), ProjectKey::new("project").unwrap());
        let binding = SessionWorkspaceBinding::new(
            SessionBindingId::new("binding").unwrap(),
            authority.clone(),
            principal,
            project,
        )
        .unwrap();
        let cursor = WorkspaceCursor::new(
            &binding,
            ResourceScope::root(ResourceId::new("root").unwrap()),
            1,
            CwdHandle::new("cursor").unwrap(),
        );
        let capabilities = WorkspaceCapabilities::new([
            WorkspaceCapability::SnapshotCapture,
            WorkspaceCapability::SnapshotStatus,
            WorkspaceCapability::SnapshotPrepareRestore,
            WorkspaceCapability::SnapshotPrepareUnrevert,
            WorkspaceCapability::SnapshotAcknowledge,
            WorkspaceCapability::SnapshotPrepareCleanup,
            WorkspaceCapability::SnapshotExecute,
            WorkspaceCapability::SnapshotOperationStatus,
            WorkspaceCapability::SnapshotCancel,
            WorkspaceCapability::SnapshotRelease,
        ]);
        let read: Arc<dyn WorkspaceSnapshotReadService> = service.clone();
        let mutation: Arc<dyn WorkspaceSnapshotMutationService> = service;
        let workspace = WorkspaceHandle::new(
            authority,
            capabilities,
            WorkspaceServices {
                snapshot_read: Some(read),
                snapshot_mutation: Some(mutation),
                ..WorkspaceServices::default()
            },
        )
        .unwrap();
        let stored =
            StoredWorkspaceBinding::new_with_cursor(binding.clone(), cursor.clone(), None).unwrap();
        WorkspaceBaseline::new_workspace_session(
            StateDir::from_path(temp.path().join("state")),
            head(99),
            WorkspaceSession::new(workspace, binding, cursor).unwrap(),
            stored,
            config,
        )
    }

    fn ensure(baseline: &WorkspaceBaseline, sequence: u32) -> BaselineOutcome {
        smol::block_on(baseline.ensure(Some(head(sequence))))
    }

    /// From the first head back to the session start.
    fn rewind(baseline: &WorkspaceBaseline) -> Result<PreparedSnapshotOperation, BaselineError> {
        smol::block_on(baseline.prepare_remote_restore(&[head(1)], &[]))
    }

    fn quota_exceeded() -> WorkspaceError {
        WorkspaceError::QuotaExceeded {
            limit: Some(STORAGE_LIMIT.into()),
            maximum: Some(1),
        }
    }

    #[test_case(false; "complete")]
    #[test_case(true; "cancel")]
    fn reserved_remote_capture_gates_mutation_before_it_is_polled(cancel: bool) {
        let temp = TempDir::new().unwrap();
        let service = Arc::new(FakeSnapshots::default());
        let baseline = remote_baseline(&temp, Arc::clone(&service), SnapshotsConfig::default());
        smol::block_on(baseline.ensure(None)).into_result().unwrap();
        let (resume, pause) = flume::bounded(1);
        *service.capture_pause.lock().unwrap() = Some(pause);
        baseline.set_current_head(Some(head(1)));
        let mut capture = Box::pin(baseline.reserve_remote_capture(head(1)).unwrap().unwrap());
        baseline.set_current_head(Some(head(2)));
        let mut mutation = Box::pin(baseline.ensure_current());

        assert!(smol::block_on(poll_once(&mut mutation)).is_none());
        assert_eq!(service.capture_calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            baseline.reserve_remote_capture(head(2)),
            Err(BaselineError::CaptureBusy)
        ));
        assert!(smol::block_on(poll_once(&mut capture)).is_none());
        assert!(baseline.is_capturing());
        assert!(smol::block_on(poll_once(&mut mutation)).is_none());
        assert!(baseline.remote_capture(Some(head(1))).unwrap().is_none());
        if !cancel {
            resume.send(()).unwrap();
            assert!(matches!(
                smol::block_on(&mut capture),
                BaselineOutcome::Ready
            ));
        }
        drop(capture);
        assert!(!baseline.is_capturing());
        assert_eq!(
            baseline.remote_capture(Some(head(1))).unwrap().is_some(),
            !cancel
        );
        *service.capture_pause.lock().unwrap() = None;
        assert!(matches!(smol::block_on(mutation), BaselineOutcome::Ready));
        assert!(baseline.remote_capture(Some(head(2))).unwrap().is_some());
    }

    #[test_case(false; "reserved")]
    #[test_case(true; "running")]
    fn reserved_remote_capture_cannot_cross_a_rebind(running: bool) {
        let temp = TempDir::new().unwrap();
        let service = Arc::new(FakeSnapshots::default());
        let baseline = remote_baseline(&temp, Arc::clone(&service), SnapshotsConfig::default());
        smol::block_on(baseline.ensure(None)).into_result().unwrap();
        let (_resume, pause) = flume::bounded(1);
        *service.capture_pause.lock().unwrap() = Some(pause);
        let mut capture = Box::pin(baseline.reserve_remote_capture(head(1)).unwrap().unwrap());
        if running {
            assert!(smol::block_on(poll_once(&mut capture)).is_none());
            assert!(baseline.is_capturing());
        }
        let other_temp = TempDir::new().unwrap();
        let other_service = Arc::new(FakeSnapshots::default());
        let other = remote_baseline(
            &other_temp,
            Arc::clone(&other_service),
            SnapshotsConfig::default(),
        );
        let target = other.target.load_full();
        let BaselineTarget::WorkspaceSession { workspace, .. } = &*target else {
            unreachable!();
        };
        baseline.rebind_workspace_session(
            StateDir::from_path(other_temp.path().join("state")),
            head(99),
            workspace.clone(),
            StoredWorkspaceBinding::new_with_cursor(
                workspace.binding().clone(),
                workspace.cursor().clone(),
                None,
            )
            .unwrap(),
        );
        assert!(matches!(
            smol::block_on(capture),
            BaselineOutcome::Failed(BaselineError::TargetChanged)
        ));
        assert!(!baseline.is_capturing());
        assert_eq!(
            service.capture_calls.load(Ordering::SeqCst),
            1 + usize::from(running)
        );
        assert_eq!(other_service.capture_calls.load(Ordering::SeqCst), 0);
        assert!(baseline.remote_capture(Some(head(1))).unwrap().is_none());
        assert!(baseline.gate.try_lock().is_some());
    }

    #[test_case(false; "before_poll")]
    #[test_case(true; "during_capture")]
    fn cancelling_remote_ensure_clears_the_capture_flag(running: bool) {
        let temp = TempDir::new().unwrap();
        let service = Arc::new(FakeSnapshots::default());
        let baseline = remote_baseline(&temp, Arc::clone(&service), SnapshotsConfig::default());
        let (_resume, pause) = flume::bounded(1);
        *service.capture_pause.lock().unwrap() = Some(pause);
        let mut capture = Box::pin(baseline.ensure(None));
        if running {
            assert!(smol::block_on(poll_once(&mut capture)).is_none());
            assert!(baseline.is_capturing());
        }
        drop(capture);
        assert!(!baseline.is_capturing());
        assert!(baseline.gate.try_lock().is_some());
        assert!(baseline.remote_capture(None).unwrap().is_none());
    }

    #[test]
    fn remote_capture_is_idempotent_and_concurrent_calls_are_coalesced() {
        let temp = TempDir::new().unwrap();
        let service = Arc::new(FakeSnapshots::default());
        let baseline = remote_baseline(&temp, Arc::clone(&service), SnapshotsConfig::default());
        let first = Arc::clone(&baseline);
        let second = Arc::clone(&baseline);

        smol::block_on(async {
            let (left, right) = futures_lite::future::zip(
                first.ensure(Some(head(1))),
                second.ensure(Some(head(1))),
            )
            .await;
            assert!(matches!(left, BaselineOutcome::Ready));
            assert!(matches!(right, BaselineOutcome::Ready));
            assert!(matches!(
                baseline.ensure(Some(head(1))).await,
                BaselineOutcome::Ready
            ));
        });

        assert_eq!(service.capture_calls.load(Ordering::SeqCst), 2);
        assert!(!temp.path().join("snapshots").exists());
    }

    #[test_case(WorkspaceError::LimitExceeded { limit: Some(FILES_LIMIT.into()), maximum: Some(1) } ; "a_limit")]
    #[test_case(quota_exceeded() ; "a_full_store_with_nothing_to_delete")]
    #[test_case(WorkspaceError::UnsupportedEntry ; "an_unsupported_entry")]
    #[test_case(WorkspaceError::UnsupportedCapability { capability: WorkspaceCapability::SnapshotCapture } ; "a_missing_capability")]
    fn a_refused_remote_workspace_turns_revert_off_and_lets_changes_through(error: WorkspaceError) {
        let temp = TempDir::new().unwrap();
        let service = Arc::new(FakeSnapshots::default());
        service
            .capture_errors
            .lock()
            .unwrap()
            .push_back(error.clone());
        let baseline = remote_baseline(&temp, Arc::clone(&service), SnapshotsConfig::default());
        let expected = BaselineError::Refused(error).to_string();

        let outcomes = [smol::block_on(baseline.ensure(None)), ensure(&baseline, 1)];

        for outcome in outcomes {
            let BaselineOutcome::Unavailable(reason) = outcome else {
                panic!("{UNAVAILABLE_MSG}: {outcome:?}");
            };
            assert_eq!(*reason, expected, "{UNAVAILABLE_MSG}");
        }
        assert_eq!(
            baseline.refusal().as_deref(),
            Some(&expected),
            "{UNAVAILABLE_MSG}"
        );
        assert_eq!(
            service.capture_calls.load(Ordering::SeqCst),
            1,
            "{UNAVAILABLE_MSG}"
        );
    }

    #[test_case(WorkspaceError::Busy ; "a_busy_host")]
    #[test_case(WorkspaceError::PolicyDenied ; "a_policy_denial")]
    #[test_case(WorkspaceError::Unavailable ; "an_unreachable_host")]
    fn a_failed_remote_capture_blocks_the_change_and_names_its_reason(error: WorkspaceError) {
        let temp = TempDir::new().unwrap();
        let service = Arc::new(FakeSnapshots::default());
        service
            .capture_errors
            .lock()
            .unwrap()
            .push_back(error.clone());
        let baseline = remote_baseline(&temp, service, SnapshotsConfig::default());

        let outcome = smol::block_on(baseline.ensure(None));

        let BaselineOutcome::Failed(failure) = outcome else {
            panic!("{BLOCKED_MSG}: {outcome:?}");
        };
        assert_eq!(
            failure.to_string(),
            BaselineError::Workspace(error).to_string(),
            "{BLOCKED_MSG}"
        );
        assert_eq!(baseline.refusal(), None, "{BLOCKED_MSG}");
    }

    #[test]
    fn a_corrupt_remote_capture_blocks_the_change() {
        let temp = TempDir::new().unwrap();
        let service = Arc::new(FakeSnapshots::default());
        *service.snapshot_state.lock().unwrap() = SnapshotState::Corrupt;
        let baseline = remote_baseline(&temp, service, SnapshotsConfig::default());

        assert!(
            matches!(
                ensure(&baseline, 1),
                BaselineOutcome::Failed(BaselineError::CorruptSnapshot)
            ),
            "{BLOCKED_MSG}"
        );
    }

    #[test]
    fn a_remote_capture_asks_for_the_configured_limits() {
        let temp = TempDir::new().unwrap();
        let service = Arc::new(FakeSnapshots::default());
        let config = SnapshotsConfig {
            max_bytes: 3,
            max_files: 1,
            max_file_bytes: 2,
            ..SnapshotsConfig::default()
        };
        let baseline = remote_baseline(&temp, Arc::clone(&service), config);

        smol::block_on(baseline.ensure(None));

        assert_eq!(
            *service.capture_limits.lock().unwrap(),
            Some(SnapshotCaptureLimits {
                max_files: 1,
                max_file_bytes: 2,
                max_total_bytes: 3,
            })
        );
    }

    #[test_case(false; "fresh")]
    #[test_case(true; "existing_captures")]
    fn a_remote_workspace_with_snapshots_off_skips_capture_and_gate(existing: bool) {
        let temp = TempDir::new().unwrap();
        let service = Arc::new(FakeSnapshots::default());
        if existing {
            let baseline = remote_baseline(&temp, Arc::clone(&service), SnapshotsConfig::default());
            assert!(matches!(ensure(&baseline, 1), BaselineOutcome::Ready));
        }
        let capture_calls = service.capture_calls.load(Ordering::SeqCst);
        let baseline = remote_baseline(
            &temp,
            Arc::clone(&service),
            SnapshotsConfig {
                enabled: false,
                ..SnapshotsConfig::default()
            },
        );

        let old_capture = baseline.remote_capture(None).unwrap();
        let _gate = baseline.gate.try_lock().unwrap();
        assert!(baseline.reserve_remote_capture(head(2)).unwrap().is_none());
        for head in [None, Some(head(1)), Some(head(2))] {
            let gate = BaselineGate::new(Arc::clone(&baseline), head);
            let outcome = smol::block_on(poll_once(gate.ensure())).unwrap();
            let BaselineOutcome::Unavailable(reason) = &outcome else {
                panic!("{DISABLED_MSG}: {outcome:?}");
            };
            assert_eq!(reason.as_str(), SNAPSHOTS_DISABLED, "{DISABLED_MSG}");
            outcome.into_result().unwrap();
        }
        assert_eq!(
            service.capture_calls.load(Ordering::SeqCst),
            capture_calls,
            "{DISABLED_MSG}"
        );
        assert_eq!(service.status_calls.load(Ordering::SeqCst), 0);
        assert_eq!(service.acknowledge_calls.load(Ordering::SeqCst), 0);
        assert_eq!(service.execute_calls.load(Ordering::SeqCst), 0);
        assert_eq!(service.release_calls.load(Ordering::SeqCst), 0);
        assert!(service.cleanup_batches.lock().unwrap().is_empty());
        assert_eq!(baseline.remote_capture(None).unwrap(), old_capture);
        assert_eq!(old_capture.is_some(), existing);
        assert_eq!(baseline.remote_capture(Some(head(2))).unwrap(), None);
        assert_eq!(baseline.refusal(), None, "{DISABLED_MSG}");
    }

    #[test]
    fn a_full_store_gives_up_older_checkpoints_and_captures_again() {
        let temp = TempDir::new().unwrap();
        let service = Arc::new(FakeSnapshots::default());
        let baseline = remote_baseline(&temp, Arc::clone(&service), SnapshotsConfig::default());
        for sequence in 1..=3 {
            assert!(
                matches!(ensure(&baseline, sequence), BaselineOutcome::Ready),
                "{READY_MSG}"
            );
        }
        service
            .capture_errors
            .lock()
            .unwrap()
            .push_back(quota_exceeded());

        let outcome = ensure(&baseline, 4);

        assert!(
            matches!(outcome, BaselineOutcome::Ready),
            "{READY_MSG}: {outcome:?}"
        );
        assert_eq!(
            *service.cleanup_batches.lock().unwrap(),
            vec![3],
            "{RETENTION_MSG}"
        );
        for sequence in 1..=3 {
            assert_eq!(
                baseline.remote_capture(Some(head(sequence))).unwrap(),
                None,
                "{RETENTION_MSG}"
            );
        }
        assert!(
            baseline.remote_capture(Some(head(4))).unwrap().is_some(),
            "{READY_MSG}"
        );
        assert!(
            baseline.remote_capture(None).unwrap().is_some(),
            "{RETENTION_MSG}"
        );
    }

    /// Nothing is deleted until a batch is prunable, then the batch goes in
    /// chunks the host takes, and the session start and the newest captures
    /// stay.
    #[test]
    fn retention_deletes_old_checkpoints_in_batches_the_host_accepts() {
        let temp = TempDir::new().unwrap();
        let service = Arc::new(FakeSnapshots::default());
        let baseline = remote_baseline(&temp, Arc::clone(&service), SnapshotsConfig::default());
        let newest = u32::try_from(REMOTE_RECENT_CHECKPOINTS + REMOTE_PRUNE_BATCH).unwrap();
        let oldest_kept = u32::try_from(REMOTE_PRUNE_BATCH).unwrap() + 1;

        for sequence in 1..=newest {
            assert!(
                matches!(ensure(&baseline, sequence), BaselineOutcome::Ready),
                "{READY_MSG}"
            );
        }

        let batches = service.cleanup_batches.lock().unwrap().clone();
        assert_eq!(
            batches.len(),
            REMOTE_PRUNE_BATCH.div_ceil(MAX_CLEANUP),
            "{RETENTION_MSG}: {batches:?}"
        );
        assert!(
            batches.iter().all(|batch| *batch <= MAX_CLEANUP),
            "{RETENTION_MSG}: {batches:?}"
        );
        assert_eq!(
            batches.iter().sum::<usize>(),
            REMOTE_PRUNE_BATCH,
            "{RETENTION_MSG}"
        );
        assert_eq!(
            baseline
                .remote_capture(Some(head(oldest_kept - 1)))
                .unwrap(),
            None,
            "{RETENTION_MSG}"
        );
        assert!(
            baseline
                .remote_capture(Some(head(oldest_kept)))
                .unwrap()
                .is_some(),
            "{RETENTION_MSG}"
        );
        assert!(
            baseline.remote_capture(None).unwrap().is_some(),
            "{RETENTION_MSG}"
        );
        assert_eq!(
            service.checkpoints.lock().unwrap().len(),
            REMOTE_RECENT_CHECKPOINTS + 1,
            "{RETENTION_MSG}"
        );
    }

    #[test]
    fn a_rewind_runs_between_the_nearest_captures_of_its_two_heads() {
        let temp = TempDir::new().unwrap();
        let service = Arc::new(FakeSnapshots::default());
        let baseline = remote_baseline(&temp, service, SnapshotsConfig::default());
        ensure(&baseline, 1);
        ensure(&baseline, 2);
        let snapshot = |sequence| {
            baseline
                .remote_capture(Some(head(sequence)))
                .unwrap()
                .unwrap()
                .snapshot_id
        };

        let prepared = smol::block_on(
            baseline.prepare_remote_restore(&[head(3), head(2), head(1)], &[head(1)]),
        )
        .unwrap();

        let SnapshotOperationPreview::Restore(preview) = prepared.preview else {
            panic!("{REWIND_MSG}");
        };
        assert_eq!(
            (preview.target_snapshot_id, preview.source_snapshot_id),
            (snapshot(1), snapshot(2)),
            "{REWIND_MSG}"
        );
    }

    #[test]
    fn a_rewind_before_any_capture_has_no_snapshot_to_restore() {
        let temp = TempDir::new().unwrap();
        let baseline = remote_baseline(
            &temp,
            Arc::new(FakeSnapshots::default()),
            SnapshotsConfig::default(),
        );

        assert!(
            matches!(rewind(&baseline), Err(BaselineError::NoSnapshot)),
            "{REWIND_MSG}"
        );
    }

    #[test]
    fn remote_restore_conflict_is_previewed_and_never_executed() {
        let temp = TempDir::new().unwrap();
        let service = Arc::new(FakeSnapshots::default());
        service.conflict.store(true, Ordering::SeqCst);
        let baseline = remote_baseline(&temp, Arc::clone(&service), SnapshotsConfig::default());
        ensure(&baseline, 1);

        let Err(BaselineError::RestoreConflict { conflicts, sample }) = rewind(&baseline) else {
            panic!("a conflict stops the restore before it runs");
        };

        assert_eq!(
            (conflicts, sample),
            (1, Some(WorkspacePath::new(CONFLICT_PATH).unwrap()))
        );
        assert_eq!(service.execute_calls.load(Ordering::SeqCst), 0);
        assert_eq!(service.release_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_refused_restore_changes_nothing_and_leaves_nothing_to_recover() {
        let temp = TempDir::new().unwrap();
        let service = Arc::new(FakeSnapshots::default());
        service.refuse_restores.store(true, Ordering::SeqCst);
        let baseline = remote_baseline(&temp, service, SnapshotsConfig::default());
        ensure(&baseline, 1);
        let prepared = rewind(&baseline).unwrap();

        let Err(error) = smol::block_on(baseline.execute_remote_restore(prepared, None)) else {
            panic!("{REFUSED_RESTORE_MSG}");
        };

        assert!(error.left_workspace_unchanged(), "{REFUSED_RESTORE_MSG}");
        let BaselineError::RestoreFailed { code, message } = error else {
            panic!("{REFUSED_RESTORE_MSG}: {error}");
        };
        assert_eq!(
            (code.as_str(), message.as_str()),
            (RESTORE_FAILURE_CODE, RESTORE_FAILURE_REASON),
            "{REFUSED_RESTORE_MSG}"
        );
        assert_eq!(
            baseline.pending_remote_restore().unwrap(),
            None,
            "{REFUSED_RESTORE_MSG}"
        );
        assert!(
            matches!(ensure(&baseline, 2), BaselineOutcome::Ready),
            "{REFUSED_RESTORE_MSG}"
        );
    }

    #[test_case(true; "enabled")]
    #[test_case(false; "disabled")]
    fn completed_rewind_can_be_unreverted_before_acknowledgement(enabled: bool) {
        let temp = TempDir::new().unwrap();
        let service = Arc::new(FakeSnapshots::default());
        let baseline = remote_baseline(&temp, Arc::clone(&service), SnapshotsConfig::default());
        ensure(&baseline, 1);
        let prepared = rewind(&baseline).unwrap();
        let restored = smol::block_on(baseline.execute_remote_restore(prepared, None)).unwrap();
        assert_eq!(restored.state, SnapshotRestoreState::Completed);
        assert!(baseline.pending_remote_restore().unwrap().is_some());

        let baseline = remote_baseline(
            &temp,
            Arc::clone(&service),
            SnapshotsConfig {
                enabled,
                ..SnapshotsConfig::default()
            },
        );
        let prepared =
            smol::block_on(baseline.prepare_remote_unrevert(&restored.restore_id)).unwrap();
        let unreverted = smol::block_on(baseline.execute_remote_unrevert(
            prepared,
            Some(head(1)),
            restored.restore_id,
        ))
        .unwrap();
        smol::block_on(baseline.acknowledge_remote_restore(&unreverted.restore_id)).unwrap();

        assert!(baseline.pending_remote_restore().unwrap().is_none());
        assert_eq!(service.execute_calls.load(Ordering::SeqCst), 2);
        assert_eq!(service.acknowledge_calls.load(Ordering::SeqCst), 1);
    }

    #[test_case(SnapshotRestoreState::Partial, true ; "partial")]
    #[test_case(SnapshotRestoreState::Indeterminate, true ; "indeterminate")]
    #[test_case(SnapshotRestoreState::Partial, false ; "partial_with_snapshots_off")]
    #[test_case(SnapshotRestoreState::Indeterminate, false ; "indeterminate_with_snapshots_off")]
    fn incomplete_restore_survives_reopen_and_blocks_mutation(
        state: SnapshotRestoreState,
        enabled: bool,
    ) {
        let temp = TempDir::new().unwrap();
        let service = Arc::new(FakeSnapshots::default());
        *service.restore_state.lock().unwrap() = state;
        let baseline = remote_baseline(&temp, Arc::clone(&service), SnapshotsConfig::default());
        ensure(&baseline, 1);
        let prepared = rewind(&baseline).unwrap();
        let partial = smol::block_on(baseline.execute_remote_restore(prepared, None)).unwrap();
        assert_eq!(partial.state, state);

        let reopened = remote_baseline(
            &temp,
            Arc::clone(&service),
            SnapshotsConfig {
                enabled,
                ..SnapshotsConfig::default()
            },
        );
        let recovered = smol::block_on(reopened.reconcile_remote_restore())
            .unwrap()
            .unwrap();
        assert_eq!(recovered.state, state);
        let pending = reopened.pending_remote_restore().unwrap();
        let capture_calls = service.capture_calls.load(Ordering::SeqCst);
        assert!(
            matches!(
                ensure(&reopened, 2),
                BaselineOutcome::Failed(BaselineError::RecoveryRequired)
            ),
            "{RECOVERY_MSG}"
        );
        assert_eq!(reopened.pending_remote_restore().unwrap(), pending);
        assert_eq!(service.acknowledge_calls.load(Ordering::SeqCst), 0);
        assert_eq!(service.capture_calls.load(Ordering::SeqCst), capture_calls);
    }

    #[test_case(true; "enabled")]
    #[test_case(false; "disabled")]
    fn completed_restore_is_acknowledged_before_the_next_mutation(enabled: bool) {
        let temp = TempDir::new().unwrap();
        let service = Arc::new(FakeSnapshots::default());
        let baseline = remote_baseline(&temp, Arc::clone(&service), SnapshotsConfig::default());
        ensure(&baseline, 1);
        let prepared = rewind(&baseline).unwrap();
        smol::block_on(baseline.execute_remote_restore(prepared, None)).unwrap();

        let baseline = remote_baseline(
            &temp,
            Arc::clone(&service),
            SnapshotsConfig {
                enabled,
                ..SnapshotsConfig::default()
            },
        );
        let capture_calls = service.capture_calls.load(Ordering::SeqCst);
        let outcome = smol::block_on(baseline.ensure(None));
        assert_eq!(outcome.is_unavailable(), !enabled);
        outcome.into_result().unwrap();
        assert_eq!(service.capture_calls.load(Ordering::SeqCst), capture_calls);
        assert_eq!(service.acknowledge_calls.load(Ordering::SeqCst), 1);
        assert!(baseline.pending_remote_restore().unwrap().is_none());
    }

    fn baseline(enabled: bool, limits: SnapshotLimits) -> (TempDir, Arc<WorkspaceBaseline>) {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(FILE), CONTENTS).unwrap();
        let store = Arc::new(SnapshotStore::new(temp.path().join("snapshots")).with_limits(limits));
        let baseline = WorkspaceBaseline::new(
            store,
            root,
            SnapshotsConfig {
                enabled,
                ..SnapshotsConfig::default()
            },
        );
        (temp, baseline)
    }

    fn head(sequence: u32) -> CaudraId {
        let mut bytes = [0u8; 16];
        bytes[12..].copy_from_slice(&sequence.to_be_bytes());
        CaudraId::from_bytes(bytes)
    }

    #[test]
    fn the_first_capture_writes_the_anchor_and_the_head() {
        let (_temp, baseline) = baseline(true, SnapshotLimits::default());
        assert!(!baseline.is_captured(), "{READY_MSG}");

        let outcome = smol::block_on(baseline.ensure(Some(head(1))));
        assert!(matches!(outcome, BaselineOutcome::Ready), "{READY_MSG}");
        assert!(baseline.is_captured(), "{READY_MSG}");
        let target = baseline.target.load();
        let BaselineTarget::Local { store, .. } = &**target else {
            panic!("{READY_MSG}");
        };
        assert!(store.has_checkpoint(head(1)), "{READY_MSG}");
    }

    /// Parallel tool calls all reach the gate, and a capture is the expensive
    /// half of a mutating call: doing it twice would double the cost of the
    /// first write in every batch.
    #[test]
    fn concurrent_calls_capture_once() {
        let (_temp, baseline) = baseline(true, SnapshotLimits::default());
        let first = Arc::clone(&baseline);
        let second = Arc::clone(&baseline);

        smol::block_on(async move {
            let (left, right) = futures_lite::future::zip(
                first.ensure(Some(head(1))),
                second.ensure(Some(head(1))),
            )
            .await;
            assert!(matches!(left, BaselineOutcome::Ready), "{READY_MSG}");
            assert!(matches!(right, BaselineOutcome::Ready), "{READY_MSG}");
        });
        let target = baseline.target.load();
        let BaselineTarget::Local { store, .. } = &**target else {
            panic!("{READY_MSG}");
        };
        assert_eq!(
            store.load_session_start_manifest().unwrap().len(),
            1,
            "{READY_MSG}"
        );
    }

    #[test_case(false; "original_workspace")]
    #[test_case(true; "rebound_workspace")]
    fn a_disabled_baseline_is_unavailable_without_touching_the_store(rebind: bool) {
        let (temp, baseline) = baseline(false, SnapshotLimits::default());
        if rebind {
            baseline.rebind(
                Arc::new(SnapshotStore::new(temp.path().join("other-snapshots"))),
                baseline.cwd(),
            );
        }
        let _gate = baseline.gate.try_lock().unwrap();

        let outcome = smol::block_on(poll_once(baseline.ensure(None))).unwrap();
        assert!(
            matches!(outcome, BaselineOutcome::Unavailable(reason) if *reason == SNAPSHOTS_DISABLED),
            "{UNAVAILABLE_MSG}"
        );
        assert!(!baseline.is_captured(), "{UNAVAILABLE_MSG}");
        assert!(baseline.unavailable_reason().is_some(), "{UNAVAILABLE_MSG}");
    }

    #[test]
    fn a_workspace_over_the_budget_is_refused_once_and_stays_refused() {
        let (_temp, baseline) = baseline(
            true,
            SnapshotLimits {
                max_files: 0,
                ..SnapshotLimits::default()
            },
        );

        let outcome = smol::block_on(baseline.ensure(None));
        let BaselineOutcome::Unavailable(first) = outcome else {
            panic!("{UNAVAILABLE_MSG}: {outcome:?}");
        };
        let second = smol::block_on(baseline.ensure(None));
        assert!(
            matches!(second, BaselineOutcome::Unavailable(reason) if reason == first),
            "{UNAVAILABLE_MSG}"
        );
        assert_eq!(
            baseline.unavailable_reason(),
            Some(first),
            "{UNAVAILABLE_MSG}"
        );
    }

    #[test]
    fn rebinding_clears_a_refusal() {
        let (temp, baseline) = baseline(
            true,
            SnapshotLimits {
                max_files: 0,
                ..SnapshotLimits::default()
            },
        );
        assert!(
            smol::block_on(baseline.ensure(None)).is_unavailable(),
            "{UNAVAILABLE_MSG}"
        );

        let other = temp.path().join("other");
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join(FILE), CONTENTS).unwrap();
        baseline.rebind(
            Arc::new(SnapshotStore::new(temp.path().join("other-snapshots"))),
            other,
        );

        assert!(baseline.unavailable_reason().is_none(), "{READY_MSG}");
        assert!(
            matches!(
                smol::block_on(baseline.ensure(None)),
                BaselineOutcome::Ready
            ),
            "{READY_MSG}"
        );
    }
}
