use std::collections::BTreeSet;

use caudra_workspace::{
    CheckpointId, OperationHandle, RestoreId, SnapshotId, SnapshotRestoreStatus, SnapshotState,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::id::CaudraId;
use crate::state::{self, StateKey};
use crate::workspace_binding::StoredWorkspaceBinding;
use crate::{StateClass, StateDir, StorageError};

const METADATA_KEY: StateKey = StateKey {
    name: "workspace.remote-snapshots",
    class: StateClass::Volatile,
};
const SCOPE_PREFIX: &str = "remote-snapshot:";
const SCOPE_DOMAIN: &[u8] = b"caudra.remote-snapshot-metadata.v1\0";
const DOCUMENT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteSnapshotMetadata {
    pub history_head: Option<CaudraId>,
    pub checkpoint_id: CheckpointId,
    pub snapshot_id: SnapshotId,
    pub manifest_revision: caudra_workspace::ResourceRevision,
    pub state: SnapshotState,
    pub created_at_unix_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteRestoreKind {
    Rewind,
    Unrevert,
    Cleanup,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteRestoreRecord {
    pub kind: RemoteRestoreKind,
    pub restore_id: RestoreId,
    pub operation: OperationHandle,
    pub target_history_head: Option<CaudraId>,
    pub target_snapshot_id: SnapshotId,
    pub source_restore_id: Option<RestoreId>,
    pub status: Option<SnapshotRestoreStatus>,
    pub acknowledged: bool,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

impl RemoteRestoreRecord {
    pub fn blocks_mutation(&self) -> bool {
        !self.acknowledged
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct RemoteSnapshotDocument {
    version: u32,
    session_id: CaudraId,
    binding: StoredWorkspaceBinding,
    captures: Vec<RemoteSnapshotMetadata>,
    restores: Vec<RemoteRestoreRecord>,
}

impl RemoteSnapshotDocument {
    fn new(session_id: CaudraId, binding: StoredWorkspaceBinding) -> Self {
        Self {
            version: DOCUMENT_VERSION,
            session_id,
            binding,
            captures: Vec::new(),
            restores: Vec::new(),
        }
    }

    fn validate(
        &self,
        session_id: CaudraId,
        binding: &StoredWorkspaceBinding,
    ) -> Result<(), RemoteSnapshotMetadataError> {
        if self.version != DOCUMENT_VERSION
            || self.session_id != session_id
            || !self.binding.exact_scope_eq(binding)
        {
            return Err(RemoteSnapshotMetadataError::IdentityMismatch);
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RemoteSnapshotMetadataError {
    #[error("remote snapshot metadata storage is unavailable")]
    Storage(#[source] StorageError),
    #[error("remote snapshot metadata belongs to a different workspace identity")]
    IdentityMismatch,
    #[error("remote snapshot restore metadata is inconsistent")]
    RestoreMismatch,
}

#[derive(Clone)]
pub struct RemoteSnapshotMetadataStore {
    state_dir: StateDir,
    session_id: CaudraId,
    binding: StoredWorkspaceBinding,
    scope: String,
}

impl RemoteSnapshotMetadataStore {
    pub fn new(state_dir: StateDir, session_id: CaudraId, binding: StoredWorkspaceBinding) -> Self {
        let scope = metadata_scope(session_id, &binding);
        Self {
            state_dir,
            session_id,
            binding,
            scope,
        }
    }

    pub fn binding(&self) -> &StoredWorkspaceBinding {
        &self.binding
    }

    pub fn carry_to_binding(
        &self,
        binding: StoredWorkspaceBinding,
    ) -> Result<(), RemoteSnapshotMetadataError> {
        if self.binding.exact_scope_eq(&binding) {
            return Ok(());
        }
        if !self.binding.same_workspace_identity(&binding)
            || self.binding.cursor_label() != binding.cursor_label()
            || !matches!((self.binding.cursor(), binding.cursor()), (Some(old), Some(new))
                if old.scope() == new.scope() && old.generation() == new.generation())
        {
            return Err(RemoteSnapshotMetadataError::IdentityMismatch);
        }
        let Some(previous) = self.load()? else {
            return Ok(());
        };
        let target = Self::new(self.state_dir.clone(), self.session_id, binding);
        target.update(|document| {
            if document.captures == previous.captures && document.restores == previous.restores {
                return Ok(());
            }
            if !document.captures.is_empty() || !document.restores.is_empty() {
                return Err(RemoteSnapshotMetadataError::IdentityMismatch);
            }
            document.captures = previous.captures;
            document.restores = previous.restores;
            Ok(())
        })
    }

    pub fn stable_scope_id(&self) -> &str {
        &self.scope
    }

    pub fn capture(
        &self,
        history_head: Option<CaudraId>,
    ) -> Result<Option<RemoteSnapshotMetadata>, RemoteSnapshotMetadataError> {
        Ok(self.load()?.and_then(|document| {
            document
                .captures
                .into_iter()
                .find(|capture| capture.history_head == history_head)
        }))
    }

    pub fn record_capture(
        &self,
        capture: RemoteSnapshotMetadata,
    ) -> Result<(), RemoteSnapshotMetadataError> {
        self.update(|document| {
            if let Some(existing) = document
                .captures
                .iter_mut()
                .find(|existing| existing.history_head == capture.history_head)
            {
                *existing = capture;
            } else {
                document.captures.push(capture);
            }
            Ok(())
        })
    }

    pub fn begin_restore(
        &self,
        restore: RemoteRestoreRecord,
    ) -> Result<(), RemoteSnapshotMetadataError> {
        self.update(|document| {
            if let Some(pending) = document
                .restores
                .iter()
                .rev()
                .find(|restore| restore.blocks_mutation())
                && !(restore.kind == RemoteRestoreKind::Unrevert
                    && restore.source_restore_id.as_ref() == Some(&pending.restore_id)
                    && pending.status.as_ref().is_some_and(|status| {
                        status.state == caudra_workspace::SnapshotRestoreState::Completed
                            && !status.reconciliation_required
                    }))
            {
                return Err(RemoteSnapshotMetadataError::RestoreMismatch);
            }
            if document
                .restores
                .iter()
                .any(|existing| existing.restore_id == restore.restore_id)
            {
                return Err(RemoteSnapshotMetadataError::RestoreMismatch);
            }
            document.restores.push(restore);
            Ok(())
        })
    }

    pub fn update_restore_status(
        &self,
        status: SnapshotRestoreStatus,
        updated_at_unix_ms: u64,
    ) -> Result<(), RemoteSnapshotMetadataError> {
        self.update(|document| {
            let restore = document
                .restores
                .iter_mut()
                .find(|restore| restore.restore_id == status.restore_id)
                .ok_or(RemoteSnapshotMetadataError::RestoreMismatch)?;
            if restore.target_snapshot_id != status.target_snapshot_id {
                return Err(RemoteSnapshotMetadataError::RestoreMismatch);
            }
            restore.status = Some(status);
            restore.updated_at_unix_ms = updated_at_unix_ms;
            Ok(())
        })
    }

    pub fn acknowledge_restore(
        &self,
        restore_id: &RestoreId,
        updated_at_unix_ms: u64,
    ) -> Result<(), RemoteSnapshotMetadataError> {
        self.update(|document| {
            let restore = document
                .restores
                .iter_mut()
                .find(|restore| &restore.restore_id == restore_id)
                .ok_or(RemoteSnapshotMetadataError::RestoreMismatch)?;
            restore.acknowledged = true;
            restore.updated_at_unix_ms = updated_at_unix_ms;
            Ok(())
        })
    }

    pub fn pending_restore(
        &self,
    ) -> Result<Option<RemoteRestoreRecord>, RemoteSnapshotMetadataError> {
        Ok(self.load()?.and_then(|document| {
            document
                .restores
                .into_iter()
                .rev()
                .find(RemoteRestoreRecord::blocks_mutation)
        }))
    }

    pub fn restore(
        &self,
        restore_id: &RestoreId,
    ) -> Result<Option<RemoteRestoreRecord>, RemoteSnapshotMetadataError> {
        Ok(self.load()?.and_then(|document| {
            document
                .restores
                .into_iter()
                .find(|restore| &restore.restore_id == restore_id)
        }))
    }

    pub fn referenced_snapshot_ids(
        &self,
    ) -> Result<BTreeSet<SnapshotId>, RemoteSnapshotMetadataError> {
        let Some(document) = self.load()? else {
            return Ok(BTreeSet::new());
        };
        let mut referenced = document
            .captures
            .into_iter()
            .map(|capture| capture.snapshot_id)
            .collect::<BTreeSet<_>>();
        for restore in document.restores {
            if restore.blocks_mutation() || !restore.acknowledged {
                referenced.insert(restore.target_snapshot_id);
                if let Some(status) = restore.status {
                    referenced.insert(status.pre_restore_snapshot_id);
                }
            }
        }
        Ok(referenced)
    }

    pub fn remove_deleted_snapshots(
        &self,
        deleted: &BTreeSet<SnapshotId>,
    ) -> Result<(), RemoteSnapshotMetadataError> {
        self.update(|document| {
            document
                .captures
                .retain(|capture| !deleted.contains(&capture.snapshot_id));
            Ok(())
        })
    }

    fn load(&self) -> Result<Option<RemoteSnapshotDocument>, RemoteSnapshotMetadataError> {
        let document =
            state::get::<RemoteSnapshotDocument>(&self.state_dir, &self.scope, METADATA_KEY)
                .map_err(RemoteSnapshotMetadataError::Storage)?;
        if let Some(document) = &document {
            document.validate(self.session_id, &self.binding)?;
        }
        Ok(document)
    }

    fn update<T>(
        &self,
        update: impl FnOnce(&mut RemoteSnapshotDocument) -> Result<T, RemoteSnapshotMetadataError>,
    ) -> Result<T, RemoteSnapshotMetadataError> {
        let session_id = self.session_id;
        let binding = self.binding.clone();
        let mut store = state::StateStore::open(&self.state_dir, METADATA_KEY.class)
            .map_err(RemoteSnapshotMetadataError::Storage)?;
        store
            .try_update(
                &self.scope,
                METADATA_KEY,
                |document: &mut Option<RemoteSnapshotDocument>| {
                    if document.is_none() {
                        *document = Some(RemoteSnapshotDocument::new(session_id, binding.clone()));
                    }
                    let document = document
                        .as_mut()
                        .ok_or(RemoteSnapshotMetadataError::IdentityMismatch)?;
                    document.validate(session_id, &binding)?;
                    update(document)
                },
            )
            .map_err(RemoteSnapshotMetadataError::Storage)?
    }
}

fn metadata_scope(session_id: CaudraId, binding: &StoredWorkspaceBinding) -> String {
    let mut hasher = Sha256::new();
    hasher.update(SCOPE_DOMAIN);
    hasher.update(session_id.as_bytes());
    for value in [
        binding.trust_anchor().as_str(),
        &binding.authority_storage_key(),
        binding.principal_id(),
        binding.project_key().as_str(),
        binding.cwd_handle().as_str(),
        binding.cursor_label().unwrap_or_default(),
    ] {
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    }
    if let Some(cursor) = binding.cursor() {
        hasher.update(cursor.generation().to_be_bytes());
        hasher.update(
            serde_json::to_vec(cursor.scope()).expect("workspace scope serialization cannot fail"),
        );
    }
    let digest = hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            use std::fmt::Write;
            let _ = write!(output, "{byte:02x}");
            output
        });
    format!("{SCOPE_PREFIX}{digest}")
}

#[cfg(test)]
mod tests {
    use caudra_workspace::{
        CwdHandle, ResourceId, ResourceRevision, ResourceScope, SessionBindingId,
        SessionWorkspaceBinding, SnapshotRestoreState, WorkspaceCursor,
    };

    use super::*;

    fn store(root: &tempfile::TempDir, generation: u64) -> RemoteSnapshotMetadataStore {
        let session_id = CaudraId::generate();
        let base = StoredWorkspaceBinding::local_from_cwd("remote-test");
        let cursor = WorkspaceCursor::new(
            base.binding(),
            ResourceScope::root(ResourceId::new("root").unwrap()),
            generation,
            CwdHandle::new("cursor").unwrap(),
        );
        let binding = StoredWorkspaceBinding::new_with_cursor(
            SessionWorkspaceBinding::new(
                base.binding().binding_id().clone(),
                base.binding().authority().clone(),
                base.binding().principal().clone(),
                base.binding().project().clone(),
            )
            .unwrap(),
            cursor,
            None,
        )
        .unwrap();
        RemoteSnapshotMetadataStore::new(
            StateDir::from_path(root.path().join("state")),
            session_id,
            binding,
        )
    }

    fn capture(head: CaudraId, snapshot: &str) -> RemoteSnapshotMetadata {
        RemoteSnapshotMetadata {
            history_head: Some(head),
            checkpoint_id: CheckpointId::new(format!("checkpoint-{snapshot}")).unwrap(),
            snapshot_id: SnapshotId::new(snapshot).unwrap(),
            manifest_revision: ResourceRevision::new("manifest").unwrap(),
            state: SnapshotState::Complete,
            created_at_unix_ms: 1,
        }
    }

    fn restore(snapshot: &str) -> RemoteRestoreRecord {
        let restore_id = RestoreId::new("restore").unwrap();
        RemoteRestoreRecord {
            kind: RemoteRestoreKind::Rewind,
            restore_id: restore_id.clone(),
            operation: OperationHandle {
                preparation_id: caudra_workspace::OperationId::new("prepared").unwrap(),
                invocation_id: Some(caudra_workspace::OperationId::new("invoked").unwrap()),
                execution_id: Some(caudra_workspace::OperationId::new("executed").unwrap()),
                expires_at_unix_ms: None,
            },
            target_history_head: None,
            target_snapshot_id: SnapshotId::new(snapshot).unwrap(),
            source_restore_id: None,
            status: None,
            acknowledged: false,
            created_at_unix_ms: 1,
            updated_at_unix_ms: 1,
        }
    }

    #[test]
    fn capture_mapping_is_exact_and_reusable() {
        let root = tempfile::tempdir().unwrap();
        let store = store(&root, 1);
        let head = CaudraId::generate();
        let capture = capture(head, "snapshot");

        store.record_capture(capture.clone()).unwrap();
        store.record_capture(capture.clone()).unwrap();

        assert_eq!(store.capture(Some(head)).unwrap(), Some(capture));
    }

    #[test]
    fn generation_isolation_uses_a_distinct_sqlite_scope() {
        let root = tempfile::tempdir().unwrap();
        let first = store(&root, 1);
        let second = RemoteSnapshotMetadataStore::new(first.state_dir.clone(), first.session_id, {
            let binding = first.binding.clone();
            let cursor = binding.cursor().unwrap();
            StoredWorkspaceBinding::new_with_cursor(
                binding.binding().clone(),
                WorkspaceCursor::new(
                    binding.binding(),
                    cursor.scope().clone(),
                    2,
                    cursor.cwd_handle().clone(),
                ),
                None,
            )
            .unwrap()
        });
        let head = CaudraId::generate();
        first.record_capture(capture(head, "first")).unwrap();

        assert_eq!(second.capture(Some(head)).unwrap(), None);
    }

    #[test]
    fn reconnect_reuses_scope_but_principal_drift_does_not() {
        let root = tempfile::tempdir().unwrap();
        let first = store(&root, 1);
        let head = CaudraId::generate();
        let recorded = capture(head, "first");
        first.record_capture(recorded.clone()).unwrap();
        let old = first.binding.clone();
        let reconnect_binding = SessionWorkspaceBinding::new(
            SessionBindingId::new("reconnected").unwrap(),
            old.binding().authority().clone(),
            old.binding().principal().clone(),
            old.binding().project().clone(),
        )
        .unwrap();
        let reconnect_cursor = WorkspaceCursor::new(
            &reconnect_binding,
            old.cursor().unwrap().scope().clone(),
            old.cursor().unwrap().generation(),
            old.cursor().unwrap().cwd_handle().clone(),
        );
        let reconnect = RemoteSnapshotMetadataStore::new(
            first.state_dir.clone(),
            first.session_id,
            StoredWorkspaceBinding::new_with_cursor(reconnect_binding, reconnect_cursor, None)
                .unwrap(),
        );
        assert_eq!(reconnect.capture(Some(head)).unwrap(), Some(recorded));

        let authority = old.binding().authority().clone();
        let drift_binding = SessionWorkspaceBinding::new(
            SessionBindingId::new("principal-drift").unwrap(),
            authority.clone(),
            caudra_workspace::AuthenticatedPrincipalId::new(authority, "other-principal").unwrap(),
            old.binding().project().clone(),
        )
        .unwrap();
        let drift_cursor = WorkspaceCursor::new(
            &drift_binding,
            old.cursor().unwrap().scope().clone(),
            old.cursor().unwrap().generation(),
            old.cursor().unwrap().cwd_handle().clone(),
        );
        let drift = RemoteSnapshotMetadataStore::new(
            first.state_dir.clone(),
            first.session_id,
            StoredWorkspaceBinding::new_with_cursor(drift_binding, drift_cursor, None).unwrap(),
        );
        assert_eq!(drift.capture(Some(head)).unwrap(), None);
    }

    #[test]
    fn pending_and_unacknowledged_restores_remain_reachable() {
        let root = tempfile::tempdir().unwrap();
        let store = store(&root, 1);
        let record = restore("target");
        let restore_id = record.restore_id.clone();
        store.begin_restore(record).unwrap();

        let referenced = store.referenced_snapshot_ids().unwrap();
        assert!(referenced.contains(&SnapshotId::new("target").unwrap()));

        store
            .update_restore_status(
                SnapshotRestoreStatus {
                    restore_id: restore_id.clone(),
                    state: SnapshotRestoreState::Completed,
                    target_snapshot_id: SnapshotId::new("target").unwrap(),
                    pre_restore_snapshot_id: SnapshotId::new("pre-restore").unwrap(),
                    applied_files: 2,
                    total_files: 2,
                    acknowledgement_required: true,
                    reconciliation_required: false,
                    unrevert_of: None,
                },
                2,
            )
            .unwrap();
        assert!(store.pending_restore().unwrap().is_some());
        store.acknowledge_restore(&restore_id, 3).unwrap();
        assert!(store.pending_restore().unwrap().is_none());
    }

    #[test]
    fn refreshed_cursor_carries_recovery_without_deleting_old_records() {
        let root = tempfile::tempdir().unwrap();
        let first = store(&root, 1);
        let head = CaudraId::generate();
        let capture = capture(head, "snapshot");
        let restore = restore("snapshot");
        first.record_capture(capture.clone()).unwrap();
        first.begin_restore(restore.clone()).unwrap();
        let old = first.binding();
        let cursor = WorkspaceCursor::new(
            old.binding(),
            old.cursor().unwrap().scope().clone(),
            old.cursor().unwrap().generation(),
            CwdHandle::new("fresh-handle").unwrap(),
        );
        let binding = old.with_cursor(cursor).unwrap();
        first.carry_to_binding(binding.clone()).unwrap();
        first.carry_to_binding(binding.clone()).unwrap();
        let target =
            RemoteSnapshotMetadataStore::new(first.state_dir.clone(), first.session_id, binding);
        assert_eq!(target.capture(Some(head)).unwrap(), Some(capture.clone()));
        assert_eq!(target.pending_restore().unwrap(), Some(restore.clone()));
        assert_eq!(first.capture(Some(head)).unwrap(), Some(capture));
        assert_eq!(first.pending_restore().unwrap(), Some(restore));
    }
}
