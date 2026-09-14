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
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use arc_swap::{ArcSwap, ArcSwapOption};
use caudra_storage::StateDir;
use caudra_storage::id::CaudraId;
use caudra_storage::remote_snapshots::{
    RemoteRestoreKind, RemoteRestoreRecord, RemoteSnapshotMetadata, RemoteSnapshotMetadataError,
    RemoteSnapshotMetadataStore,
};
use caudra_storage::workspace_binding::StoredWorkspaceBinding;
use caudra_workspace::{
    CheckpointId, OperationState, PreparedSnapshotOperation, RestoreId, SnapshotChangeKind,
    SnapshotId, SnapshotOperationPreview, SnapshotOperationResult, SnapshotRestoreState,
    SnapshotRestoreStatus, SnapshotState, WorkspaceError, WorkspaceSession,
};
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::snapshots::{SnapshotError, SnapshotStore};

const SNAPSHOTS_DISABLED: &str = "workspace snapshots are off in your configuration";
const CHECKPOINT_DOMAIN: &[u8] = b"caudra.remote-checkpoint.v1\0";

#[derive(Debug, thiserror::Error)]
pub enum BaselineError {
    #[error(transparent)]
    Local(#[from] SnapshotError),
    #[error("remote workspace snapshot service is unavailable")]
    Workspace(#[from] WorkspaceError),
    #[error(transparent)]
    Metadata(#[from] RemoteSnapshotMetadataError),
    #[error("remote workspace snapshot is corrupt")]
    CorruptSnapshot,
    #[error("remote workspace mutation is blocked until snapshot recovery is resolved")]
    RecoveryRequired,
    #[error("remote workspace restore has conflicts and was not executed")]
    RestoreConflict,
    #[error("remote workspace restore failed")]
    RestoreFailed,
    #[error("remote workspace restore response is inconsistent")]
    InvalidRestore,
}

/// What a mutating call learns before it runs.
#[derive(Debug)]
pub enum BaselineOutcome {
    /// A revert point for this run exists.
    Ready,
    /// This workspace will not be snapshotted, and the call may proceed anyway.
    /// A deliberate refusal costs file revert, not the user's work.
    Unavailable(Arc<String>),
    /// The capture was attempted and did not finish, so there is no revert
    /// point. The call must not proceed.
    Failed(BaselineError),
}

impl BaselineOutcome {
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable(_))
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
    /// Both halves already exist on disk, so there is nothing to capture. Two
    /// `exists` checks, which is what makes calling this before every mutating
    /// call affordable.
    fn is_captured(&self, head: Option<CaudraId>) -> bool {
        match self {
            Self::Local { store, .. } => {
                store.has_session_start() && head.is_none_or(|head| store.has_checkpoint(head))
            }
            Self::WorkspaceSession { metadata, .. } => {
                metadata
                    .pending_restore()
                    .is_ok_and(|pending| pending.is_none())
                    && metadata.capture(head).is_ok_and(|capture| {
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

    async fn capture_remote(&self, head: Option<CaudraId>) -> Result<(), BaselineError> {
        let Self::WorkspaceSession {
            workspace,
            metadata,
        } = self
        else {
            return Err(BaselineError::InvalidRestore);
        };
        Self::acknowledge_completed_before_mutation(workspace, metadata).await?;
        if head.is_some() && metadata.capture(None)?.is_none() {
            Self::capture_remote_checkpoint(workspace, metadata, None).await?;
        }
        if let Some(capture) = metadata.capture(head)? {
            return match capture.state {
                SnapshotState::Complete => Ok(()),
                SnapshotState::Corrupt => Err(BaselineError::CorruptSnapshot),
            };
        }
        Self::capture_remote_checkpoint(workspace, metadata, head).await
    }

    async fn capture_remote_checkpoint(
        workspace: &WorkspaceSession,
        metadata: &RemoteSnapshotMetadataStore,
        head: Option<CaudraId>,
    ) -> Result<(), BaselineError> {
        let checkpoint_id = remote_checkpoint_id(metadata, head)?;
        let service = workspace
            .workspace()
            .services()
            .snapshot_read
            .as_ref()
            .ok_or(WorkspaceError::Unavailable)?;
        let result = service
            .capture(
                workspace.binding(),
                workspace.cursor(),
                &caudra_workspace::SnapshotCaptureRequest {
                    checkpoint_id: checkpoint_id.clone(),
                    label: None,
                },
            )
            .await
            .map_err(BaselineError::Workspace)?;
        if result.snapshot.checkpoint_id.as_ref() != Some(&checkpoint_id) {
            return Err(BaselineError::CorruptSnapshot);
        }
        let capture = RemoteSnapshotMetadata {
            history_head: head,
            checkpoint_id,
            snapshot_id: result.snapshot.snapshot_id,
            manifest_revision: result.snapshot.manifest_revision,
            state: result.snapshot.state,
            created_at_unix_ms: result.snapshot.created_at_unix_ms,
        };
        metadata.record_capture(capture.clone())?;
        match capture.state {
            SnapshotState::Complete => Ok(()),
            SnapshotState::Corrupt => Err(BaselineError::CorruptSnapshot),
        }
    }

    fn is_remote(&self) -> bool {
        matches!(self, Self::WorkspaceSession { .. })
    }

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
                workspace
                    .workspace()
                    .services()
                    .snapshot_read
                    .as_ref()
                    .ok_or(WorkspaceError::Unavailable)?
                    .restore_status(workspace.binding(), workspace.cursor(), &pending.restore_id)
                    .await?
            }
        };
        if !matches!(status.state, SnapshotRestoreState::Completed)
            || status.reconciliation_required
        {
            return Err(BaselineError::RecoveryRequired);
        }
        let acknowledged = workspace
            .workspace()
            .services()
            .snapshot_mutation
            .as_ref()
            .ok_or(WorkspaceError::Unavailable)?
            .acknowledge(workspace.binding(), workspace.cursor(), &pending.restore_id)
            .await?;
        metadata.update_restore_status(acknowledged, unix_millis())?;
        metadata.acknowledge_restore(&pending.restore_id, unix_millis())?;
        Ok(())
    }
}

/// One per session, shared with every agent serving it, the way `PathLocks` is.
pub struct WorkspaceBaseline {
    /// From configuration, so it never changes for the life of the process and
    /// `rebind` must not clear it.
    enabled: bool,
    target: ArcSwap<BaselineTarget>,
    /// Single-flights the capture: parallel tool calls and subagents all reach
    /// this, and the second one through must wait rather than start its own.
    gate: async_lock::Mutex<()>,
    /// Sticky, so a workspace the store refused is judged once rather than on
    /// every call.
    unavailable: ArcSwapOption<String>,
    capturing: AtomicBool,
    current_head: ArcSwapOption<CaudraId>,
}

impl WorkspaceBaseline {
    pub fn new(store: Arc<SnapshotStore>, cwd: PathBuf, enabled: bool) -> Arc<Self> {
        Arc::new(Self {
            enabled,
            target: ArcSwap::from_pointee(BaselineTarget::Local { store, cwd }),
            gate: async_lock::Mutex::default(),
            unavailable: ArcSwapOption::empty(),
            capturing: AtomicBool::new(false),
            current_head: ArcSwapOption::empty(),
        })
    }

    pub fn new_workspace_session(
        storage: StateDir,
        session_id: CaudraId,
        workspace: WorkspaceSession,
        binding: StoredWorkspaceBinding,
        enabled: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            enabled,
            target: ArcSwap::from_pointee(BaselineTarget::WorkspaceSession {
                workspace,
                metadata: Box::new(RemoteSnapshotMetadataStore::new(
                    storage, session_id, binding,
                )),
            }),
            gate: async_lock::Mutex::default(),
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
    }

    pub fn rebind_workspace_session(
        &self,
        storage: StateDir,
        session_id: CaudraId,
        workspace: WorkspaceSession,
        binding: StoredWorkspaceBinding,
    ) {
        self.target
            .store(Arc::new(BaselineTarget::WorkspaceSession {
                workspace,
                metadata: Box::new(RemoteSnapshotMetadataStore::new(
                    storage, session_id, binding,
                )),
            }));
        self.unavailable.store(None);
    }

    /// Why this workspace has no file revert, whether by configuration or by
    /// a refusal earned on the tree itself.
    pub fn unavailable_reason(&self) -> Option<Arc<String>> {
        if !self.enabled {
            return Some(Arc::new(SNAPSHOTS_DISABLED.to_owned()));
        }
        self.unavailable.load_full()
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

    /// Holds the gate across the capture, so the call that asked second arrives
    /// after the baseline is on disk rather than alongside it.
    pub async fn ensure(&self, head: Option<CaudraId>) -> BaselineOutcome {
        if let Some(reason) = self.unavailable_reason() {
            return BaselineOutcome::Unavailable(reason);
        }
        let _gate = self.gate.lock().await;
        // Re-read under the gate: whoever held it may have just answered this.
        if let Some(reason) = self.unavailable.load_full() {
            return BaselineOutcome::Unavailable(reason);
        }
        let target = self.target.load_full();
        if target.is_captured(head) {
            return BaselineOutcome::Ready;
        }
        self.capturing.store(true, Ordering::Release);
        let result = match &*target {
            BaselineTarget::Local { .. } => {
                let work = Arc::clone(&target);
                smol::unblock(move || work.capture_local(head)).await
            }
            BaselineTarget::WorkspaceSession { .. } => target.capture_remote(head).await,
        };
        self.capturing.store(false, Ordering::Release);
        match result {
            Ok(()) => BaselineOutcome::Ready,
            Err(BaselineError::Local(error)) if error.is_workspace_refusal() => {
                let cwd = self.cwd();
                warn!(
                    cwd = %cwd.display(),
                    %error,
                    "workspace refused for snapshots, file revert is off"
                );
                let reason = Arc::new(error.to_string());
                self.unavailable.store(Some(Arc::clone(&reason)));
                BaselineOutcome::Unavailable(reason)
            }
            Err(error) => BaselineOutcome::Failed(error),
        }
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

    pub async fn prepare_remote_restore(
        &self,
        target_head: Option<CaudraId>,
    ) -> Result<PreparedSnapshotOperation, BaselineError> {
        let target = self.target.load_full();
        let BaselineTarget::WorkspaceSession {
            workspace,
            metadata,
        } = &*target
        else {
            return Err(BaselineError::InvalidRestore);
        };
        BaselineTarget::acknowledge_completed_before_mutation(workspace, metadata).await?;
        let capture = metadata
            .capture(target_head)?
            .filter(|capture| capture.state == SnapshotState::Complete)
            .ok_or(BaselineError::CorruptSnapshot)?;
        let service = workspace
            .workspace()
            .services()
            .snapshot_mutation
            .as_ref()
            .ok_or(WorkspaceError::Unavailable)?;
        let prepared = service
            .prepare_restore(
                workspace.binding(),
                workspace.cursor(),
                &capture.snapshot_id,
            )
            .await
            .map_err(BaselineError::Workspace)?;
        let SnapshotOperationPreview::Restore(preview) = &prepared.preview else {
            return Err(BaselineError::InvalidRestore);
        };
        if preview.target_snapshot_id != capture.snapshot_id {
            return Err(BaselineError::InvalidRestore);
        }
        if preview
            .changes
            .iter()
            .any(|change| change.kind == SnapshotChangeKind::Conflict)
        {
            let _ = service
                .release(workspace.binding(), workspace.cursor(), &prepared)
                .await;
            return Err(BaselineError::RestoreConflict);
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
        workspace
            .workspace()
            .services()
            .snapshot_mutation
            .as_ref()
            .ok_or(WorkspaceError::Unavailable)?
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
        let service = workspace
            .workspace()
            .services()
            .snapshot_mutation
            .as_ref()
            .ok_or(WorkspaceError::Unavailable)?;
        let prepared = service
            .prepare_unrevert(workspace.binding(), workspace.cursor(), restore_id)
            .await
            .map_err(BaselineError::Workspace)?;
        let SnapshotOperationPreview::Unrevert(preview) = &prepared.preview else {
            return Err(BaselineError::InvalidRestore);
        };
        if preview.source_restore_id != *restore_id {
            return Err(BaselineError::InvalidRestore);
        }
        if preview
            .restore
            .changes
            .iter()
            .any(|change| change.kind == SnapshotChangeKind::Conflict)
        {
            let _ = service
                .release(workspace.binding(), workspace.cursor(), &prepared)
                .await;
            return Err(BaselineError::RestoreConflict);
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
        let service = workspace
            .workspace()
            .services()
            .snapshot_mutation
            .as_ref()
            .ok_or(WorkspaceError::Unavailable)?;
        let result = service
            .execute(workspace.binding(), workspace.cursor(), &prepared)
            .await
            .map_err(BaselineError::Workspace)?;
        let status = match result.state {
            OperationState::Completed {
                result: SnapshotOperationResult::Restore(status),
                ..
            } => status,
            OperationState::Failed { .. } | OperationState::Cancelled { .. } => {
                return Err(BaselineError::RestoreFailed);
            }
            OperationState::NeverSeen
            | OperationState::Prepared
            | OperationState::Running
            | OperationState::Forgotten
            | OperationState::Indeterminate { .. }
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
        workspace
            .workspace()
            .services()
            .snapshot_read
            .as_ref()
            .ok_or(WorkspaceError::Unavailable)?
            .restore_status(workspace.binding(), workspace.cursor(), restore_id)
            .await
            .map_err(BaselineError::Workspace)
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
        let status = workspace
            .workspace()
            .services()
            .snapshot_mutation
            .as_ref()
            .ok_or(WorkspaceError::Unavailable)?
            .acknowledge(workspace.binding(), workspace.cursor(), restore_id)
            .await
            .map_err(BaselineError::Workspace)?;
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

    pub async fn cleanup_remote_snapshots(
        &self,
        candidates: impl IntoIterator<Item = SnapshotId>,
    ) -> Result<Vec<SnapshotId>, BaselineError> {
        let target = self.target.load_full();
        let BaselineTarget::WorkspaceSession {
            workspace,
            metadata,
        } = &*target
        else {
            return Ok(Vec::new());
        };
        if metadata.pending_restore()?.is_some() {
            return Err(BaselineError::RecoveryRequired);
        }
        let referenced = metadata.referenced_snapshot_ids()?;
        let candidates = candidates
            .into_iter()
            .filter(|snapshot| !referenced.contains(snapshot))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let service = workspace
            .workspace()
            .services()
            .snapshot_mutation
            .as_ref()
            .ok_or(WorkspaceError::Unavailable)?;
        let prepared = service
            .prepare_cleanup(workspace.binding(), workspace.cursor(), &candidates)
            .await?;
        let SnapshotOperationPreview::Cleanup(preview) = &prepared.preview else {
            return Err(BaselineError::InvalidRestore);
        };
        if preview
            .retained_snapshot_ids
            .iter()
            .any(|snapshot| referenced.contains(snapshot))
            || preview
                .snapshot_ids
                .iter()
                .any(|snapshot| referenced.contains(snapshot))
        {
            return Err(BaselineError::InvalidRestore);
        }
        let status = service
            .execute(workspace.binding(), workspace.cursor(), &prepared)
            .await?;
        let result = match status.state {
            OperationState::Completed {
                result: SnapshotOperationResult::Cleanup(result),
                ..
            } => result,
            OperationState::Failed { .. } | OperationState::Cancelled { .. } => {
                return Err(BaselineError::RestoreFailed);
            }
            _ => return Err(BaselineError::RecoveryRequired),
        };
        let deleted = result
            .deleted_snapshot_ids
            .into_iter()
            .collect::<BTreeSet<_>>();
        if deleted.iter().any(|snapshot| referenced.contains(snapshot)) {
            return Err(BaselineError::InvalidRestore);
        }
        metadata.remove_deleted_snapshots(&deleted)?;
        Ok(deleted.into_iter().collect())
    }
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

    pub fn is_remote(&self) -> bool {
        self.baseline.is_remote()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use caudra_workspace::{
        AuthenticatedPrincipalId, AuthorityIdentity, CancellationResult, CwdHandle,
        OperationHandle, OperationId, OperationPhase, OperationStatus, PreparedSnapshotOperation,
        ProjectIdentity, ProjectKey, ReleaseResult, ResourceId, ResourceRevision, ResourceScope,
        SequenceMetadata, SessionBindingId, SessionWorkspaceBinding, SnapshotCaptureRequest,
        SnapshotCaptureResult, SnapshotCleanupPreview, SnapshotCleanupResult,
        SnapshotOperationPreview, SnapshotOperationResult, SnapshotRestorePreview,
        SnapshotRestoreState, SnapshotRestoreStatus, SnapshotSummary, SnapshotUnrevertPreview,
        SourceTrustAnchor, WorkspaceCapabilities, WorkspaceCapability, WorkspaceCursor,
        WorkspaceHandle, WorkspaceServices, WorkspaceSnapshotMutationService,
        WorkspaceSnapshotReadService,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::snapshots::SnapshotLimits;

    const FILE: &str = "tracked.txt";
    const CONTENTS: &str = "alpha";
    const READY_MSG: &str = "a mutating call gets a revert point";
    const UNAVAILABLE_MSG: &str = "a refused workspace lets the call through";

    struct FakeSnapshots {
        capture_calls: AtomicUsize,
        execute_calls: AtomicUsize,
        release_calls: AtomicUsize,
        acknowledge_calls: AtomicUsize,
        capture_error: Mutex<Option<WorkspaceError>>,
        snapshot_state: Mutex<SnapshotState>,
        restore_state: Mutex<SnapshotRestoreState>,
        conflict: AtomicBool,
        next_restore: AtomicUsize,
        restores: Mutex<HashMap<String, (SnapshotId, Option<RestoreId>)>>,
    }

    impl Default for FakeSnapshots {
        fn default() -> Self {
            Self {
                capture_calls: AtomicUsize::new(0),
                execute_calls: AtomicUsize::new(0),
                release_calls: AtomicUsize::new(0),
                acknowledge_calls: AtomicUsize::new(0),
                capture_error: Mutex::new(None),
                snapshot_state: Mutex::new(SnapshotState::Complete),
                restore_state: Mutex::new(SnapshotRestoreState::Completed),
                conflict: AtomicBool::new(false),
                next_restore: AtomicUsize::new(1),
                restores: Mutex::new(HashMap::new()),
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
                file_count: 1,
                total_bytes: 5,
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

        fn preview(&self, snapshot_id: &SnapshotId) -> SnapshotRestorePreview {
            let sequence = self.next_restore.load(Ordering::SeqCst);
            let restore_id = RestoreId::new(format!("restore-{sequence}")).unwrap();
            self.restores
                .lock()
                .unwrap()
                .insert(restore_id.as_str().to_owned(), (snapshot_id.clone(), None));
            SnapshotRestorePreview {
                restore_id,
                target_snapshot_id: snapshot_id.clone(),
                current_revision: ResourceRevision::new("current-r1").unwrap(),
                target_revision: ResourceRevision::new("target-r1").unwrap(),
                changes: self
                    .conflict
                    .load(Ordering::SeqCst)
                    .then(|| caudra_workspace::SnapshotChange {
                        path: caudra_workspace::WorkspacePath::new("file.txt").unwrap(),
                        resource_id: ResourceId::new("file").unwrap(),
                        kind: SnapshotChangeKind::Conflict,
                        current_revision: Some(ResourceRevision::new("current-file").unwrap()),
                        target_revision: Some(ResourceRevision::new("target-file").unwrap()),
                    })
                    .into_iter()
                    .collect(),
                created_directories: Vec::new(),
            }
        }

        fn restore_status(
            &self,
            restore_id: RestoreId,
            target_snapshot_id: SnapshotId,
            unrevert_of: Option<RestoreId>,
        ) -> SnapshotRestoreStatus {
            let state = *self.restore_state.lock().unwrap();
            SnapshotRestoreStatus {
                restore_id,
                state,
                target_snapshot_id,
                pre_restore_snapshot_id: SnapshotId::new("pre-restore").unwrap(),
                applied_files: u32::from(state != SnapshotRestoreState::Publishing),
                total_files: 1,
                acknowledgement_required: state == SnapshotRestoreState::Completed,
                reconciliation_required: matches!(
                    state,
                    SnapshotRestoreState::Partial | SnapshotRestoreState::Indeterminate
                ),
                unrevert_of,
            }
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
            if let Some(error) = self.capture_error.lock().unwrap().clone() {
                return Err(error);
            }
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
            let (snapshot, unrevert_of) = self
                .restores
                .lock()
                .unwrap()
                .get(restore_id.as_str())
                .cloned()
                .ok_or(WorkspaceError::Conflict)?;
            Ok(self.restore_status(restore_id.clone(), snapshot, unrevert_of))
        }
    }

    #[async_trait]
    impl WorkspaceSnapshotMutationService for FakeSnapshots {
        async fn prepare_restore(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            snapshot_id: &SnapshotId,
        ) -> Result<PreparedSnapshotOperation, WorkspaceError> {
            let operation = self.operation();
            Ok(PreparedSnapshotOperation {
                preview: SnapshotOperationPreview::Restore(self.preview(snapshot_id)),
                operation,
            })
        }

        async fn prepare_unrevert(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            restore_id: &RestoreId,
        ) -> Result<PreparedSnapshotOperation, WorkspaceError> {
            let operation = self.operation();
            let preview = self.preview(&SnapshotId::new("pre-restore").unwrap());
            self.restores.lock().unwrap().insert(
                preview.restore_id.as_str().to_owned(),
                (preview.target_snapshot_id.clone(), Some(restore_id.clone())),
            );
            Ok(PreparedSnapshotOperation {
                preview: SnapshotOperationPreview::Unrevert(SnapshotUnrevertPreview {
                    source_restore_id: restore_id.clone(),
                    restore: preview,
                }),
                operation,
            })
        }

        async fn prepare_cleanup(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            snapshot_ids: &[SnapshotId],
        ) -> Result<PreparedSnapshotOperation, WorkspaceError> {
            Ok(PreparedSnapshotOperation {
                operation: self.operation(),
                preview: SnapshotOperationPreview::Cleanup(SnapshotCleanupPreview {
                    snapshot_ids: snapshot_ids.to_vec(),
                    retained_snapshot_ids: Vec::new(),
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
            let result = match &prepared.preview {
                SnapshotOperationPreview::Restore(preview) => {
                    SnapshotOperationResult::Restore(self.restore_status(
                        preview.restore_id.clone(),
                        preview.target_snapshot_id.clone(),
                        None,
                    ))
                }
                SnapshotOperationPreview::Unrevert(preview) => {
                    SnapshotOperationResult::Restore(self.restore_status(
                        preview.restore.restore_id.clone(),
                        preview.restore.target_snapshot_id.clone(),
                        Some(preview.source_restore_id.clone()),
                    ))
                }
                SnapshotOperationPreview::Cleanup(preview) => {
                    SnapshotOperationResult::Cleanup(SnapshotCleanupResult {
                        deleted_snapshot_ids: preview.snapshot_ids.clone(),
                        deleted_blobs: 1,
                        reclaimed_bytes: 1,
                    })
                }
            };
            Ok(operation_status(
                prepared.operation.clone(),
                OperationState::Completed {
                    result,
                    side_effects_possible: true,
                },
            ))
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
            let (snapshot, unrevert_of) = self
                .restores
                .lock()
                .unwrap()
                .get(restore_id.as_str())
                .cloned()
                .ok_or(WorkspaceError::Conflict)?;
            let mut status = self.restore_status(restore_id.clone(), snapshot, unrevert_of);
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
        generation: u64,
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
            generation,
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
            true,
        )
    }

    #[test]
    fn remote_capture_is_idempotent_and_concurrent_calls_are_coalesced() {
        let temp = TempDir::new().unwrap();
        let service = Arc::new(FakeSnapshots::default());
        let baseline = remote_baseline(&temp, Arc::clone(&service), 1);
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

    #[test]
    fn remote_quota_and_corruption_fail_closed() {
        let quota_temp = TempDir::new().unwrap();
        let quota = Arc::new(FakeSnapshots::default());
        *quota.capture_error.lock().unwrap() = Some(WorkspaceError::PolicyDenied);
        let baseline = remote_baseline(&quota_temp, quota, 1);
        assert!(matches!(
            smol::block_on(baseline.ensure(Some(head(1)))),
            BaselineOutcome::Failed(BaselineError::Workspace(WorkspaceError::PolicyDenied))
        ));

        let corrupt_temp = TempDir::new().unwrap();
        let corrupt = Arc::new(FakeSnapshots::default());
        *corrupt.snapshot_state.lock().unwrap() = SnapshotState::Corrupt;
        let baseline = remote_baseline(&corrupt_temp, corrupt, 1);
        assert!(matches!(
            smol::block_on(baseline.ensure(Some(head(1)))),
            BaselineOutcome::Failed(BaselineError::CorruptSnapshot)
        ));
    }

    #[test]
    fn remote_restore_conflict_is_previewed_and_never_executed() {
        let temp = TempDir::new().unwrap();
        let service = Arc::new(FakeSnapshots::default());
        service.conflict.store(true, Ordering::SeqCst);
        let baseline = remote_baseline(&temp, Arc::clone(&service), 1);
        smol::block_on(baseline.ensure(Some(head(1))));

        let result = smol::block_on(baseline.prepare_remote_restore(None));

        assert!(matches!(result, Err(BaselineError::RestoreConflict)));
        assert_eq!(service.execute_calls.load(Ordering::SeqCst), 0);
        assert_eq!(service.release_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn completed_rewind_can_be_unreverted_before_acknowledgement() {
        let temp = TempDir::new().unwrap();
        let service = Arc::new(FakeSnapshots::default());
        let baseline = remote_baseline(&temp, Arc::clone(&service), 1);
        smol::block_on(baseline.ensure(Some(head(1))));
        let prepared = smol::block_on(baseline.prepare_remote_restore(None)).unwrap();
        let restored = smol::block_on(baseline.execute_remote_restore(prepared, None)).unwrap();
        assert_eq!(restored.state, SnapshotRestoreState::Completed);
        assert!(baseline.pending_remote_restore().unwrap().is_some());

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

    #[test_case::test_case(SnapshotRestoreState::Partial ; "partial")]
    #[test_case::test_case(SnapshotRestoreState::Indeterminate ; "indeterminate")]
    fn incomplete_restore_survives_reopen_and_blocks_mutation(state: SnapshotRestoreState) {
        let temp = TempDir::new().unwrap();
        let service = Arc::new(FakeSnapshots::default());
        *service.restore_state.lock().unwrap() = state;
        let baseline = remote_baseline(&temp, Arc::clone(&service), 1);
        smol::block_on(baseline.ensure(Some(head(1))));
        let prepared = smol::block_on(baseline.prepare_remote_restore(None)).unwrap();
        let partial = smol::block_on(baseline.execute_remote_restore(prepared, None)).unwrap();
        assert_eq!(partial.state, state);

        let reopened = remote_baseline(&temp, service, 1);
        let recovered = smol::block_on(reopened.reconcile_remote_restore())
            .unwrap()
            .unwrap();
        assert_eq!(recovered.state, state);
        assert!(matches!(
            smol::block_on(reopened.ensure(Some(head(2)))),
            BaselineOutcome::Failed(BaselineError::RecoveryRequired)
        ));
    }

    #[test]
    fn completed_restore_is_acknowledged_before_the_next_mutation() {
        let temp = TempDir::new().unwrap();
        let service = Arc::new(FakeSnapshots::default());
        let baseline = remote_baseline(&temp, Arc::clone(&service), 1);
        smol::block_on(baseline.ensure(Some(head(1))));
        let prepared = smol::block_on(baseline.prepare_remote_restore(None)).unwrap();
        smol::block_on(baseline.execute_remote_restore(prepared, None)).unwrap();

        assert!(matches!(
            smol::block_on(baseline.ensure(None)),
            BaselineOutcome::Ready
        ));
        assert_eq!(service.acknowledge_calls.load(Ordering::SeqCst), 1);
        assert!(baseline.pending_remote_restore().unwrap().is_none());
    }

    #[test]
    fn cleanup_never_deletes_referenced_snapshots() {
        let temp = TempDir::new().unwrap();
        let service = Arc::new(FakeSnapshots::default());
        let baseline = remote_baseline(&temp, service, 1);
        smol::block_on(baseline.ensure(Some(head(1))));
        let referenced = baseline.remote_capture(None).unwrap().unwrap().snapshot_id;
        let orphan = SnapshotId::new("orphan").unwrap();

        let deleted =
            smol::block_on(baseline.cleanup_remote_snapshots([referenced.clone(), orphan.clone()]))
                .unwrap();

        assert_eq!(deleted, vec![orphan]);
        assert_eq!(
            baseline.remote_capture(None).unwrap().unwrap().snapshot_id,
            referenced
        );
    }

    fn baseline(enabled: bool, limits: SnapshotLimits) -> (TempDir, Arc<WorkspaceBaseline>) {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(FILE), CONTENTS).unwrap();
        let store = Arc::new(SnapshotStore::new(temp.path().join("snapshots")).with_limits(limits));
        let baseline = WorkspaceBaseline::new(store, root, enabled);
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

    #[test]
    fn a_disabled_baseline_is_unavailable_without_touching_the_store() {
        let (_temp, baseline) = baseline(false, SnapshotLimits::default());

        let outcome = smol::block_on(baseline.ensure(None));
        assert!(
            matches!(outcome, BaselineOutcome::Unavailable(reason) if reason.contains("off")),
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
