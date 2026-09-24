//! Cleanup deletes checkpoints, never snapshots directly: one content-addressed snapshot may back the
//! checkpoints of several sessions. Snapshots nothing references any more go with them, and blobs
//! no remaining snapshot names are collected last.

use std::{collections::BTreeSet, mem::size_of};

use serde::Serialize;
use tokio_util::sync::CancellationToken;
use workcell_host_contract::{ContractVersion, SnapshotCleanupPreview, SnapshotCleanupResponse};

use crate::{
    SnapshotError, SnapshotInner, identifiers,
    store::{BLOBS, CHECKPOINTS, DIGEST_PREFIX, JOURNALS, MANIFESTS, METADATA_SUFFIX},
};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct CleanupPlan {
    checkpoints: Vec<String>,
    missing: Vec<String>,
    journals: Vec<String>,
    snapshots: Vec<String>,
    reclaimable_bytes: u64,
}

impl CleanupPlan {
    pub(crate) fn preview(&self) -> Result<SnapshotCleanupPreview, SnapshotError> {
        Ok(SnapshotCleanupPreview {
            checkpoint_ids: identifiers(&self.checkpoints)?,
            missing_checkpoint_ids: identifiers(&self.missing)?,
            reclaimable_bytes: self.reclaimable_bytes,
        })
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        [
            &self.checkpoints,
            &self.missing,
            &self.journals,
            &self.snapshots,
        ]
        .into_iter()
        .flatten()
        .map(|value| size_of::<String>().saturating_add(value.capacity()))
        .fold(size_of::<Self>(), usize::saturating_add)
    }

    fn requested(&self) -> BTreeSet<String> {
        self.checkpoints
            .iter()
            .chain(&self.missing)
            .cloned()
            .collect()
    }
}

impl SnapshotInner {
    /// What deleting `requested` checkpoints removes: the checkpoints that exist, settled restore
    /// journals, and every snapshot no remaining checkpoint, open restore or preparation names.
    pub(crate) fn plan_cleanup(
        &self,
        requested: &BTreeSet<String>,
    ) -> Result<CleanupPlan, SnapshotError> {
        let mut retained = self.protected_snapshots();
        let mut checkpoints = Vec::new();
        let mut reclaimable_bytes = 0_u64;
        for name in self.store.names(CHECKPOINTS)? {
            let path = self.store.directory(CHECKPOINTS).join(name);
            let checkpoint = self.read_checkpoint(&path)?;
            if requested.contains(&checkpoint.checkpoint_id) {
                reclaimable_bytes = reclaimable_bytes.saturating_add(self.store.size(&path)?);
                checkpoints.push(checkpoint.checkpoint_id);
            } else {
                retained.insert(checkpoint.snapshot_id);
            }
        }
        checkpoints.sort_unstable();
        let missing = requested
            .iter()
            .filter(|id| checkpoints.binary_search(id).is_err())
            .cloned()
            .collect();
        let journals = self.reclaimable_journals();
        for restore_id in &journals {
            reclaimable_bytes = reclaimable_bytes
                .saturating_add(self.store.size(&self.store.journal_path(restore_id)?)?);
        }
        let mut snapshots = Vec::new();
        for name in self.store.names(MANIFESTS)? {
            let snapshot_id = name
                .strip_suffix(METADATA_SUFFIX)
                .ok_or(SnapshotError::UnhealthyStorage)?;
            let path = self.store.manifest_path(snapshot_id)?;
            if !retained.contains(snapshot_id) {
                reclaimable_bytes = reclaimable_bytes.saturating_add(self.store.size(&path)?);
                snapshots.push(snapshot_id.to_owned());
            }
        }
        Ok(CleanupPlan {
            checkpoints,
            missing,
            journals,
            snapshots,
            reclaimable_bytes,
        })
    }

    /// Deletes what `plan` named, provided the store would still plan exactly that, then collects
    /// unreferenced blobs. Cancellation only cuts the blob collection short.
    pub(crate) fn execute_cleanup(
        &self,
        plan: &CleanupPlan,
        token: &CancellationToken,
    ) -> Result<SnapshotCleanupResponse, SnapshotError> {
        if self.plan_cleanup(&plan.requested())? != *plan {
            return Err(SnapshotError::Conflict);
        }
        for checkpoint_id in &plan.checkpoints {
            self.store
                .remove(&self.store.checkpoint_path(checkpoint_id))?;
        }
        self.store.sync(CHECKPOINTS)?;
        for restore_id in &plan.journals {
            self.remove_journal(restore_id)?;
        }
        self.store.sync(JOURNALS)?;
        for snapshot_id in &plan.snapshots {
            self.store.remove(&self.store.manifest_path(snapshot_id)?)?;
        }
        self.store.sync(MANIFESTS)?;
        let (deleted_blobs, blob_bytes) = self.collect_blobs(token)?;
        Ok(SnapshotCleanupResponse {
            version: ContractVersion::V1,
            deleted_checkpoint_ids: identifiers(&plan.checkpoints)?,
            deleted_snapshots: u32::try_from(plan.snapshots.len()).unwrap_or(u32::MAX),
            deleted_blobs,
            reclaimed_bytes: plan.reclaimable_bytes.saturating_add(blob_bytes),
        })
    }

    /// Deletes blobs no manifest names. A manifest that cannot be read might name any blob, so
    /// then nothing is deleted.
    fn collect_blobs(&self, token: &CancellationToken) -> Result<(u32, u64), SnapshotError> {
        let mut reachable = BTreeSet::new();
        for name in self.store.names(MANIFESTS)? {
            let Some(manifest) = name
                .strip_suffix(METADATA_SUFFIX)
                .and_then(|snapshot_id| self.load_manifest(snapshot_id).ok())
            else {
                return Ok((0, 0));
            };
            reachable.extend(manifest.digests().map(str::to_owned));
        }
        let (mut deleted, mut bytes) = (0_u32, 0_u64);
        for name in self.store.names(BLOBS)? {
            if token.is_cancelled() {
                break;
            }
            let digest = format!("{DIGEST_PREFIX}{name}");
            let Ok(path) = self.store.blob_path(&digest) else {
                continue;
            };
            if reachable.contains(&digest) {
                continue;
            }
            bytes = bytes.saturating_add(self.store.size(&path)?);
            self.store.remove(&path)?;
            deleted = deleted.saturating_add(1);
        }
        if deleted > 0 {
            self.store.sync(BLOBS)?;
        }
        Ok((deleted, bytes))
    }
}
