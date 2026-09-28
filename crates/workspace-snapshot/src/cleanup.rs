//! Cleanup deletes checkpoints, never snapshots directly: one snapshot may back the checkpoints of
//! several sessions, and every snapshot shares whatever it can with the others. Objects that no
//! remaining checkpoint, undecided restore or prepared restore reaches are collected with them.

use std::{collections::BTreeSet, mem::size_of};

use serde::Serialize;
use tokio_util::sync::CancellationToken;
use workcell_host_contract::{ContractVersion, SnapshotCleanupPreview, SnapshotCleanupResponse};
use workcell_snapshot_store::GarbagePlan;

use crate::{
    SnapshotError, SnapshotInner, identifiers,
    snapshot::{parse_snapshot_id, snapshot_identifier, store_error},
    store::{CHECKPOINTS, JOURNALS},
};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct CleanupPlan {
    checkpoints: Vec<String>,
    missing: Vec<String>,
    journals: Vec<String>,
    /// Snapshots what this cleanup deletes named, and nothing left names.
    snapshots: Vec<String>,
    /// Names exactly the objects the collection deletes.
    objects: String,
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
        .chain([&self.objects])
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
    /// journals, and every object no remaining checkpoint, open restore or preparation reaches.
    pub(crate) fn plan_cleanup(
        &self,
        requested: &BTreeSet<String>,
    ) -> Result<(CleanupPlan, GarbagePlan), SnapshotError> {
        let mut roots = self.protected_snapshots()?;
        let mut released = BTreeSet::new();
        let mut checkpoints = Vec::new();
        let mut reclaimable_bytes = 0_u64;
        for (checkpoint, id) in self.checkpoints()? {
            if requested.contains(&checkpoint.checkpoint_id) {
                reclaimable_bytes = reclaimable_bytes.saturating_add(
                    self.store
                        .size(&self.store.checkpoint_path(&checkpoint.checkpoint_id))?,
                );
                checkpoints.push(checkpoint.checkpoint_id);
                released.insert(id);
            } else {
                roots.insert(id);
            }
        }
        checkpoints.sort_unstable();
        let missing = requested
            .iter()
            .filter(|id| checkpoints.binary_search(id).is_err())
            .cloned()
            .collect();
        let mut journals = Vec::new();
        for (restore_id, snapshot_ids) in self.reclaimable_journals() {
            reclaimable_bytes = reclaimable_bytes
                .saturating_add(self.store.size(&self.store.journal_path(&restore_id)?)?);
            for snapshot_id in &snapshot_ids {
                released.insert(parse_snapshot_id(snapshot_id)?);
            }
            journals.push(restore_id);
        }
        let objects = self.store.objects();
        let garbage = objects
            .garbage(&roots)
            .map_err(|error| store_error(error, &[]))?;
        let plan = CleanupPlan {
            checkpoints,
            missing,
            journals,
            snapshots: released
                .difference(&roots)
                .filter(|id| objects.contains(&id.oid()))
                .map(snapshot_identifier)
                .collect(),
            objects: garbage
                .digest()
                .map_err(|error| store_error(error, &[]))?
                .to_string(),
            reclaimable_bytes: reclaimable_bytes.saturating_add(garbage.usage().bytes),
        };
        Ok((plan, garbage))
    }

    /// Deletes what `plan` named, provided the store would still plan exactly that, then collects
    /// what nothing reaches any more. Cancellation only leaves that collection to a later cleanup.
    pub(crate) fn execute_cleanup(
        &self,
        plan: &CleanupPlan,
        token: &CancellationToken,
    ) -> Result<SnapshotCleanupResponse, SnapshotError> {
        let (current, garbage) = self.plan_cleanup(&plan.requested())?;
        if current != *plan {
            return Err(SnapshotError::Conflict);
        }
        let mut reclaimed_bytes = 0_u64;
        for checkpoint_id in &plan.checkpoints {
            let path = self.store.checkpoint_path(checkpoint_id);
            reclaimed_bytes = reclaimed_bytes.saturating_add(self.store.size(&path)?);
            self.store.remove(&path)?;
        }
        self.store.sync(CHECKPOINTS)?;
        for restore_id in &plan.journals {
            reclaimed_bytes = reclaimed_bytes
                .saturating_add(self.store.size(&self.store.journal_path(restore_id)?)?);
            self.remove_journal(restore_id)?;
        }
        self.store.sync(JOURNALS)?;
        let collected = if token.is_cancelled() {
            Default::default()
        } else {
            self.store
                .objects()
                .collect(&garbage)
                .map_err(|error| store_error(error, &[]))?
        };
        Ok(SnapshotCleanupResponse {
            version: ContractVersion::V1,
            deleted_checkpoint_ids: identifiers(&plan.checkpoints)?,
            deleted_snapshots: u32::try_from(plan.snapshots.len()).unwrap_or(u32::MAX),
            deleted_objects: u32::try_from(collected.objects).unwrap_or(u32::MAX),
            reclaimed_bytes: reclaimed_bytes.saturating_add(collected.bytes),
        })
    }
}
