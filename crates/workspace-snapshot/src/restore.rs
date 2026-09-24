//! Restore: publish the entries that differ between two captures, each only while the live entry
//! still matches the side it replaces. The journal records transitions, never paths, and a restore
//! awaiting acknowledgement is undone by restoring the same two captures the other way round.

use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    mem::size_of,
};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workcell_host_contract::{
    ContractVersion, MAX_SNAPSHOT_FILE_BYTES, MAX_SNAPSHOT_JOURNALS, MAX_SNAPSHOT_PREVIEW_CHANGES,
    MAX_SNAPSHOT_STORAGE_BYTES, SnapshotAcknowledgeResponse, SnapshotChange, SnapshotChangeCounts,
    SnapshotChangeKind, SnapshotLimit, SnapshotRestorePreview, SnapshotRestoreState,
    SnapshotRestoreStatus, WorkspacePath,
};
use workcell_mcp_files::{
    SnapshotTreeContent, SnapshotTreeError, SnapshotTreeExpected, SnapshotTreeObserved,
    SnapshotTreeStamp,
};

use crate::{
    MAX_PRIVATE_METADATA_BYTES, SnapshotError, SnapshotInner,
    capture::Stability,
    capture::read_stable,
    check_cancelled, digest_bytes, identifier, lock,
    manifest::{
        Difference, Manifest, SYMLINK_MODE, StoredEntry, StoredEntryKind, ancestors, differences,
    },
    path_resource_id, quota_error, revision,
    store::{JOURNALS, METADATA_SUFFIX, blob_error},
    tree_error, validate_snapshot_id,
};

const JOURNAL_VERSION: &str = "workspace-restore-journal.v2";
const RESTORE_ID_PREFIX: &str = "restore_";
const MAX_JOURNAL_STORAGE_BYTES: u64 = 64 * 1_024 * 1_024;

pub(crate) struct RestorePlan {
    pub(crate) restore_id: String,
    pub(crate) scope: String,
    pub(crate) target_snapshot_id: String,
    pub(crate) source_snapshot_id: String,
    pub(crate) unrevert_of: Option<String>,
    changes: Vec<PlannedChange>,
    created_directories: Vec<WorkspacePath>,
    conflicts: usize,
    pub(crate) preview: SnapshotRestorePreview,
}

struct PlannedChange {
    path: WorkspacePath,
    expected: SnapshotTreeExpected,
    target: Option<StoredEntry>,
}

enum Live {
    Absent,
    Entry {
        kind: StoredEntryKind,
        digest: String,
        mode: u32,
        stamp: SnapshotTreeStamp,
    },
    /// A directory, special file or mount, an entry that would not hold still to be read, or one
    /// behind an ancestor that is not a plain directory. None of these is ever replaced.
    Other,
}

#[derive(Clone, Copy)]
enum Directory {
    Present,
    Missing,
    Blocked,
}

struct Planner<'a> {
    inner: &'a SnapshotInner,
    token: &'a CancellationToken,
    directories: BTreeMap<String, Directory>,
    created: BTreeSet<String>,
    changes: Vec<PlannedChange>,
    counts: SnapshotChangeCounts,
    conflicts: Vec<SnapshotChange>,
    planned: Vec<SnapshotChange>,
    conflict_count: usize,
}

enum Publication {
    Complete,
    /// Stopped before changing the entry it was on, so exactly what was counted is published.
    Refused(SnapshotError),
    /// An entry may or may not have changed.
    Uncertain,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StoredRestoreState {
    Publishing,
    Completed,
    Partial,
    Indeterminate,
    Acknowledged,
    Reverted,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredJournal {
    version: String,
    pub(crate) restore_id: String,
    pub(crate) state: StoredRestoreState,
    pub(crate) target_snapshot_id: String,
    pub(crate) source_snapshot_id: String,
    total_files: usize,
    applied_files: usize,
    total_directories: usize,
    created_directories: usize,
    acknowledgement_required: bool,
    reconciliation_required: bool,
    unrevert_of: Option<String>,
}

impl RestorePlan {
    /// Conservative bytes the plan holds while it waits to be executed.
    pub(crate) fn retained_bytes(&self) -> usize {
        let strings = [
            &self.restore_id,
            &self.scope,
            &self.target_snapshot_id,
            &self.source_snapshot_id,
        ]
        .into_iter()
        .chain(&self.unrevert_of)
        .map(String::capacity)
        .fold(size_of::<Self>(), usize::saturating_add);
        let changes = self
            .changes
            .iter()
            .map(|change| {
                change
                    .path
                    .retained_bytes()
                    .saturating_add(change.target.as_ref().map_or(0, |entry| {
                        entry
                            .path
                            .capacity()
                            .saturating_add(entry.digest.capacity())
                    }))
            })
            .fold(
                self.changes
                    .capacity()
                    .saturating_mul(size_of::<PlannedChange>()),
                usize::saturating_add,
            );
        let directories = self
            .created_directories
            .iter()
            .map(WorkspacePath::retained_bytes)
            .fold(0, usize::saturating_add);
        let preview = self
            .preview
            .changes
            .iter()
            .map(|change| {
                size_of::<SnapshotChange>()
                    .saturating_add(change.path.retained_bytes())
                    .saturating_add(change.resource_id.as_str().len())
                    .saturating_add(
                        change
                            .current_revision
                            .as_ref()
                            .map_or(0, |value| value.len()),
                    )
                    .saturating_add(
                        change
                            .target_revision
                            .as_ref()
                            .map_or(0, |value| value.len()),
                    )
            })
            .chain(
                self.preview
                    .created_directories
                    .iter()
                    .map(WorkspacePath::retained_bytes),
            )
            .fold(0, usize::saturating_add);
        strings
            .saturating_add(changes)
            .saturating_add(directories)
            .saturating_add(preview)
    }
}

impl SnapshotInner {
    pub(crate) fn prepare_restore(
        &self,
        target_snapshot_id: &str,
        source_snapshot_id: &str,
        unrevert_of: Option<&str>,
        token: &CancellationToken,
    ) -> Result<RestorePlan, SnapshotError> {
        self.ensure_acknowledged(unrevert_of)?;
        let target = self.load_manifest(target_snapshot_id)?;
        let source = self.load_manifest(source_snapshot_id)?;
        self.plan(
            format!("{RESTORE_ID_PREFIX}{}", Uuid::new_v4()),
            &target,
            &source,
            unrevert_of,
            token,
        )
    }

    /// Restores the source of an unacknowledged restore over its target.
    pub(crate) fn prepare_unrevert(
        &self,
        restore_id: &str,
        token: &CancellationToken,
    ) -> Result<RestorePlan, SnapshotError> {
        let journal = self.journal(restore_id)?;
        if !journal.awaits_decision() {
            return Err(SnapshotError::Conflict);
        }
        self.prepare_restore(
            &journal.source_snapshot_id,
            &journal.target_snapshot_id,
            Some(restore_id),
            token,
        )
    }

    pub(crate) fn execute_restore(
        &self,
        plan: &RestorePlan,
        token: &CancellationToken,
    ) -> Result<SnapshotRestoreStatus, SnapshotError> {
        if plan.conflicts > 0 {
            return Err(SnapshotError::Conflict);
        }
        self.ensure_acknowledged(plan.unrevert_of.as_deref())?;
        let mut journal = StoredJournal::new(plan);
        if plan.changes.is_empty() && plan.created_directories.is_empty() {
            journal.state = StoredRestoreState::Completed;
            journal.acknowledgement_required = false;
        }
        self.make_journal_room(&journal)?;
        self.persist_journal(&journal)?;
        if journal.state == StoredRestoreState::Publishing {
            let outcome = self.publish(plan, &mut journal, token);
            match outcome {
                Publication::Complete => journal.state = StoredRestoreState::Completed,
                Publication::Refused(error)
                    if journal.applied_files == 0 && journal.created_directories == 0 =>
                {
                    self.forget_journal(&journal.restore_id)?;
                    return Err(error);
                }
                Publication::Refused(_) => journal.state = StoredRestoreState::Partial,
                Publication::Uncertain => {
                    journal.state = StoredRestoreState::Indeterminate;
                    journal.reconciliation_required = true;
                }
            }
            self.persist_journal(&journal).inspect_err(|_| {
                journal.state = StoredRestoreState::Indeterminate;
                journal.reconciliation_required = true;
                lock(&self.state)
                    .journals
                    .insert(journal.restore_id.clone(), journal.clone());
            })?;
        }
        if journal.state == StoredRestoreState::Completed
            && let Some(original) = &plan.unrevert_of
        {
            self.settle_journal(original, StoredRestoreState::Reverted)?;
        }
        journal.status()
    }

    /// Accepts the outcome of a restore, which then can no longer be undone.
    pub(crate) fn acknowledge(
        &self,
        restore_id: &str,
    ) -> Result<SnapshotAcknowledgeResponse, SnapshotError> {
        let journal = self.journal(restore_id)?;
        if journal.state == StoredRestoreState::Publishing {
            return Err(SnapshotError::Conflict);
        }
        let journal = if journal.awaits_decision() {
            self.settle_journal(restore_id, StoredRestoreState::Acknowledged)?
        } else {
            journal
        };
        Ok(SnapshotAcknowledgeResponse {
            version: ContractVersion::V1,
            restore: journal.status()?,
        })
    }

    pub(crate) fn journal(&self, restore_id: &str) -> Result<StoredJournal, SnapshotError> {
        lock(&self.state)
            .journals
            .get(restore_id)
            .cloned()
            .ok_or(SnapshotError::NotFound)
    }

    /// Reads every journal and settles those a crash left publishing, before anything else runs.
    pub(crate) fn load_journals(&self) -> Result<(), SnapshotError> {
        for name in self.store.names(JOURNALS)? {
            let restore_id = name
                .strip_suffix(METADATA_SUFFIX)
                .ok_or(SnapshotError::UnhealthyStorage)?;
            let path = self.store.journal_path(restore_id)?;
            let journal =
                StoredJournal::decode(&self.store.read(&path, MAX_PRIVATE_METADATA_BYTES)?)?;
            if journal.restore_id != restore_id {
                return Err(SnapshotError::UnhealthyStorage);
            }
            lock(&self.state)
                .journals
                .insert(journal.restore_id.clone(), journal);
        }
        let interrupted = lock(&self.state)
            .journals
            .values()
            .filter(|journal| journal.state == StoredRestoreState::Publishing)
            .cloned()
            .collect::<Vec<_>>();
        for mut journal in interrupted {
            self.reconcile(&mut journal);
            self.persist_journal(&journal)?;
        }
        let reverted = lock(&self.state)
            .journals
            .values()
            .filter(|journal| journal.state == StoredRestoreState::Completed)
            .filter_map(|journal| journal.unrevert_of.clone())
            .collect::<Vec<_>>();
        for original in reverted {
            if self
                .journal(&original)
                .is_ok_and(|journal| journal.state != StoredRestoreState::Reverted)
            {
                self.settle_journal(&original, StoredRestoreState::Reverted)?;
            }
        }
        self.reclaim_journals(0)
    }

    pub(crate) fn protected_snapshots(&self) -> BTreeSet<String> {
        let state = lock(&self.state);
        state
            .journals
            .values()
            .filter(|journal| !journal.reclaimable())
            .flat_map(|journal| {
                [
                    journal.target_snapshot_id.clone(),
                    journal.source_snapshot_id.clone(),
                ]
            })
            .chain(state.pending.values().flatten().cloned())
            .collect()
    }

    pub(crate) fn reclaimable_journals(&self) -> Vec<String> {
        let mut journals = lock(&self.state)
            .journals
            .values()
            .filter(|journal| journal.reclaimable())
            .map(|journal| journal.restore_id.clone())
            .collect::<Vec<_>>();
        journals.sort_unstable();
        journals
    }

    pub(crate) fn remove_journal(&self, restore_id: &str) -> Result<(), SnapshotError> {
        self.store.remove(&self.store.journal_path(restore_id)?)?;
        lock(&self.state).journals.remove(restore_id);
        Ok(())
    }

    fn plan(
        &self,
        restore_id: String,
        target: &Manifest,
        source: &Manifest,
        unrevert_of: Option<&str>,
        token: &CancellationToken,
    ) -> Result<RestorePlan, SnapshotError> {
        let (scope, differences) = differences(source, target)?;
        let mut planner = Planner {
            inner: self,
            token,
            directories: BTreeMap::new(),
            created: BTreeSet::new(),
            changes: Vec::new(),
            counts: SnapshotChangeCounts::default(),
            conflicts: Vec::new(),
            planned: Vec::new(),
            conflict_count: 0,
        };
        for difference in differences {
            planner.visit(&difference)?;
        }
        planner.counts.created_directories =
            u32::try_from(planner.created.len()).unwrap_or(u32::MAX);
        let created_directories = planner
            .created
            .into_iter()
            .map(|path| WorkspacePath::new(path).map_err(|_| SnapshotError::IntegrityFailure))
            .collect::<Result<Vec<_>, _>>()?;
        let mut sample = planner.conflicts;
        sample.extend(planner.planned);
        sample.truncate(MAX_SNAPSHOT_PREVIEW_CHANGES);
        let preview = SnapshotRestorePreview {
            restore_id: identifier(&restore_id)?,
            target_snapshot_id: identifier(&target.snapshot_id)?,
            source_snapshot_id: identifier(&source.snapshot_id)?,
            counts: planner.counts,
            changes: sample,
            created_directories: created_directories
                .iter()
                .take(MAX_SNAPSHOT_PREVIEW_CHANGES)
                .cloned()
                .collect(),
        };
        Ok(RestorePlan {
            restore_id,
            scope: scope.to_owned(),
            target_snapshot_id: target.snapshot_id.clone(),
            source_snapshot_id: source.snapshot_id.clone(),
            unrevert_of: unrevert_of.map(str::to_owned),
            changes: planner.changes,
            created_directories,
            conflicts: planner.conflict_count,
            preview,
        })
    }

    fn publish(
        &self,
        plan: &RestorePlan,
        journal: &mut StoredJournal,
        token: &CancellationToken,
    ) -> Publication {
        for directory in &plan.created_directories {
            if token.is_cancelled() {
                return Publication::Refused(SnapshotError::Cancelled);
            }
            match self.workspace.create_tree_directory(directory) {
                Ok(()) => journal.created_directories += 1,
                Err(error) => return refusal(error),
            }
        }
        for change in &plan.changes {
            if token.is_cancelled() {
                return Publication::Refused(SnapshotError::Cancelled);
            }
            if let Err(publication) = self.publish_change(change) {
                return publication;
            }
            journal.applied_files += 1;
        }
        Publication::Complete
    }

    fn publish_change(&self, change: &PlannedChange) -> Result<(), Publication> {
        let published = match &change.target {
            None => self.workspace.publish_tree_entry(
                &change.path,
                &change.expected,
                SnapshotTreeContent::Absent,
            ),
            Some(entry) if entry.kind == StoredEntryKind::Symlink => {
                let target = self
                    .store
                    .read_blob(&entry.digest, entry.size_bytes)
                    .map_err(Publication::Refused)?;
                self.workspace.publish_tree_entry(
                    &change.path,
                    &change.expected,
                    SnapshotTreeContent::Symlink { target: &target },
                )
            }
            Some(entry) => {
                let mut source = self
                    .store
                    .open_blob(&entry.digest, entry.size_bytes)
                    .map_err(Publication::Refused)?;
                self.workspace.publish_tree_entry(
                    &change.path,
                    &change.expected,
                    SnapshotTreeContent::File {
                        source: &mut source,
                        mode: entry.mode,
                    },
                )
            }
        };
        published.map_err(refusal)
    }

    /// The live entry at `path`, read the way a capture reads it.
    fn observe(
        &self,
        path: &WorkspacePath,
        token: &CancellationToken,
    ) -> Result<Live, SnapshotError> {
        match self.workspace.observe_tree_entry(path) {
            Ok(SnapshotTreeObserved::Absent) => Ok(Live::Absent),
            Ok(SnapshotTreeObserved::File(mut file)) => Ok(
                match read_stable(&mut file.file, MAX_SNAPSHOT_FILE_BYTES, token)? {
                    Stability::Stable {
                        digest,
                        mode,
                        stamp,
                        ..
                    } => Live::Entry {
                        kind: StoredEntryKind::File,
                        digest,
                        mode,
                        stamp,
                    },
                    Stability::Oversized | Stability::Unstable => Live::Other,
                },
            ),
            Ok(SnapshotTreeObserved::Symlink(link)) => Ok(Live::Entry {
                kind: StoredEntryKind::Symlink,
                digest: digest_bytes(&link.target),
                mode: SYMLINK_MODE,
                stamp: link.stamp,
            }),
            Ok(SnapshotTreeObserved::Directory | SnapshotTreeObserved::Other)
            | Err(SnapshotTreeError::Blocked | SnapshotTreeError::Protected) => Ok(Live::Other),
            Err(error) => Err(tree_error(error)),
        }
    }

    fn observe_directory(&self, path: &str) -> Result<Directory, SnapshotError> {
        let path = WorkspacePath::new(path).map_err(|_| SnapshotError::IntegrityFailure)?;
        match self.workspace.observe_tree_entry(&path) {
            Ok(SnapshotTreeObserved::Directory) => Ok(Directory::Present),
            Ok(SnapshotTreeObserved::Absent) => Ok(Directory::Missing),
            Ok(_) | Err(SnapshotTreeError::Blocked | SnapshotTreeError::Protected) => {
                Ok(Directory::Blocked)
            }
            Err(error) => Err(tree_error(error)),
        }
    }

    /// A restore changes nothing while another awaits acknowledgement, except undoing that one.
    fn ensure_acknowledged(&self, allowed: Option<&str>) -> Result<(), SnapshotError> {
        if lock(&self.state).journals.values().any(|journal| {
            journal.acknowledgement_required && Some(journal.restore_id.as_str()) != allowed
        }) {
            return Err(SnapshotError::AcknowledgementRequired);
        }
        Ok(())
    }

    /// Recomputes an interrupted restore from its captures and the live workspace.
    fn reconcile(&self, journal: &mut StoredJournal) {
        let recomputed = self
            .load_manifest(&journal.target_snapshot_id)
            .and_then(|target| {
                let source = self.load_manifest(&journal.source_snapshot_id)?;
                self.plan(
                    journal.restore_id.clone(),
                    &target,
                    &source,
                    None,
                    &CancellationToken::new(),
                )
            });
        journal.acknowledgement_required = true;
        match recomputed {
            Ok(plan) if plan.conflicts == 0 => {
                journal.applied_files = journal.total_files.saturating_sub(plan.changes.len());
                journal.state = if plan.changes.is_empty() {
                    StoredRestoreState::Completed
                } else {
                    StoredRestoreState::Partial
                };
            }
            _ => {
                journal.state = StoredRestoreState::Indeterminate;
                journal.reconciliation_required = true;
            }
        }
    }

    fn settle_journal(
        &self,
        restore_id: &str,
        state: StoredRestoreState,
    ) -> Result<StoredJournal, SnapshotError> {
        let mut journal = self.journal(restore_id)?;
        journal.state = state;
        journal.acknowledgement_required = false;
        journal.reconciliation_required = false;
        self.persist_journal(&journal)?;
        Ok(journal)
    }

    fn persist_journal(&self, journal: &StoredJournal) -> Result<(), SnapshotError> {
        self.store.write_atomic(
            &self.store.journal_path(&journal.restore_id)?,
            &journal.encode()?,
        )?;
        lock(&self.state)
            .journals
            .insert(journal.restore_id.clone(), journal.clone());
        Ok(())
    }

    fn forget_journal(&self, restore_id: &str) -> Result<(), SnapshotError> {
        self.remove_journal(restore_id)?;
        self.store.sync(JOURNALS)
    }

    /// Room for one more journal of `journal`'s size, reclaiming settled journals if need be.
    fn make_journal_room(&self, journal: &StoredJournal) -> Result<(), SnapshotError> {
        let bytes = u64::try_from(journal.encode()?.len()).unwrap_or(u64::MAX);
        if self.store.usage()?.saturating_add(bytes) > MAX_SNAPSHOT_STORAGE_BYTES {
            return Err(quota_error(
                SnapshotLimit::StorageBytes,
                MAX_SNAPSHOT_STORAGE_BYTES,
            ));
        }
        self.reclaim_journals(bytes)
    }

    /// Removes settled journals, oldest id first, until `reserve` more bytes and one more journal
    /// fit within the journal quotas.
    fn reclaim_journals(&self, reserve: u64) -> Result<(), SnapshotError> {
        let reserved_count = usize::from(reserve > 0);
        let (mut count, mut bytes) = self.journal_usage()?;
        let fits = |count: usize, bytes: u64| {
            count.saturating_add(reserved_count) <= MAX_SNAPSHOT_JOURNALS
                && bytes.saturating_add(reserve) <= MAX_JOURNAL_STORAGE_BYTES
        };
        let mut removed = false;
        for restore_id in self.reclaimable_journals() {
            if fits(count, bytes) {
                break;
            }
            let size = self.store.size(&self.store.journal_path(&restore_id)?)?;
            self.remove_journal(&restore_id)?;
            count = count.saturating_sub(1);
            bytes = bytes.saturating_sub(size);
            removed = true;
        }
        if removed {
            self.store.sync(JOURNALS)?;
        }
        if !fits(count, bytes) {
            return Err(quota_error(SnapshotLimit::Journals, MAX_SNAPSHOT_JOURNALS));
        }
        Ok(())
    }

    fn journal_usage(&self) -> Result<(usize, u64), SnapshotError> {
        let names = self.store.names(JOURNALS)?;
        let mut bytes = 0_u64;
        for name in &names {
            bytes = bytes.saturating_add(
                self.store
                    .size(&self.store.directory(JOURNALS).join(name))?,
            );
        }
        Ok((names.len(), bytes))
    }
}

impl Planner<'_> {
    fn visit(&mut self, difference: &Difference<'_>) -> Result<(), SnapshotError> {
        check_cancelled(self.token)?;
        let path =
            WorkspacePath::new(difference.path).map_err(|_| SnapshotError::IntegrityFailure)?;
        let live = self.inner.observe(&path, self.token)?;
        if live.matches(difference.target) {
            self.counts.unchanged = self.counts.unchanged.saturating_add(1);
            return Ok(());
        }
        let expected = match (&live, live.matches(difference.source)) {
            (Live::Absent, true) => SnapshotTreeExpected::Absent,
            (Live::Entry { stamp, .. }, true) => SnapshotTreeExpected::Present(stamp.clone()),
            _ => return self.conflict(difference, &live),
        };
        if difference.target.is_some() && !self.parents_ready(difference.path)? {
            return self.conflict(difference, &live);
        }
        let (kind, count) = match (difference.source, difference.target) {
            (None, _) => (SnapshotChangeKind::Create, &mut self.counts.create),
            (Some(_), Some(_)) => (SnapshotChangeKind::Replace, &mut self.counts.replace),
            (Some(_), None) => (SnapshotChangeKind::Delete, &mut self.counts.delete),
        };
        *count = count.saturating_add(1);
        if self.planned.len() < MAX_SNAPSHOT_PREVIEW_CHANGES {
            self.planned.push(sample(
                difference,
                kind,
                difference.source.map(|entry| entry.digest.as_str()),
            )?);
        }
        self.changes.push(PlannedChange {
            path,
            expected,
            target: difference.target.cloned(),
        });
        Ok(())
    }

    fn conflict(&mut self, difference: &Difference<'_>, live: &Live) -> Result<(), SnapshotError> {
        self.conflict_count += 1;
        self.counts.conflict = self.counts.conflict.saturating_add(1);
        if self.conflicts.len() < MAX_SNAPSHOT_PREVIEW_CHANGES {
            let current = match live {
                Live::Entry { digest, .. } => Some(digest.as_str()),
                Live::Absent | Live::Other => None,
            };
            self.conflicts
                .push(sample(difference, SnapshotChangeKind::Conflict, current)?);
        }
        Ok(())
    }

    /// Whether every ancestor is a plain directory or can be created, noting those to create.
    fn parents_ready(&mut self, path: &str) -> Result<bool, SnapshotError> {
        let mut missing = false;
        for ancestor in ancestors(path) {
            let state = match self.directories.get(ancestor) {
                Some(state) => *state,
                None if missing => Directory::Missing,
                None => self.inner.observe_directory(ancestor)?,
            };
            self.directories.insert(ancestor.to_owned(), state);
            match state {
                Directory::Present => {}
                Directory::Missing => {
                    missing = true;
                    self.created.insert(ancestor.to_owned());
                }
                Directory::Blocked => return Ok(false),
            }
        }
        Ok(true)
    }
}

impl Live {
    fn matches(&self, entry: Option<&StoredEntry>) -> bool {
        match (self, entry) {
            (Self::Absent, None) => true,
            (
                Self::Entry {
                    kind, digest, mode, ..
                },
                Some(entry),
            ) => {
                *kind == entry.kind
                    && *digest == entry.digest
                    && (*kind == StoredEntryKind::Symlink || *mode == entry.mode)
            }
            _ => false,
        }
    }
}

impl StoredJournal {
    fn new(plan: &RestorePlan) -> Self {
        Self {
            version: JOURNAL_VERSION.to_owned(),
            restore_id: plan.restore_id.clone(),
            state: StoredRestoreState::Publishing,
            target_snapshot_id: plan.target_snapshot_id.clone(),
            source_snapshot_id: plan.source_snapshot_id.clone(),
            total_files: plan.changes.len(),
            applied_files: 0,
            total_directories: plan.created_directories.len(),
            created_directories: 0,
            acknowledgement_required: true,
            reconciliation_required: false,
            unrevert_of: plan.unrevert_of.clone(),
        }
    }

    pub(crate) fn reclaimable(&self) -> bool {
        !self.acknowledgement_required && self.state != StoredRestoreState::Publishing
    }

    /// Completed, partial or indeterminate, and neither acknowledged nor undone.
    fn awaits_decision(&self) -> bool {
        self.acknowledgement_required
            && matches!(
                self.state,
                StoredRestoreState::Completed
                    | StoredRestoreState::Partial
                    | StoredRestoreState::Indeterminate
            )
    }

    pub(crate) fn status(&self) -> Result<SnapshotRestoreStatus, SnapshotError> {
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
            source_snapshot_id: identifier(&self.source_snapshot_id)?,
            applied_files: u32::try_from(self.applied_files).unwrap_or(u32::MAX),
            total_files: u32::try_from(self.total_files).unwrap_or(u32::MAX),
            acknowledgement_required: self.acknowledgement_required,
            reconciliation_required: self.reconciliation_required,
            unrevert_of: self.unrevert_of.as_deref().map(identifier).transpose()?,
        })
    }

    fn decode(bytes: &[u8]) -> Result<Self, SnapshotError> {
        let journal: Self =
            serde_json::from_slice(bytes).map_err(|_| SnapshotError::UnhealthyStorage)?;
        let valid = journal.version == JOURNAL_VERSION
            && journal.applied_files <= journal.total_files
            && journal.created_directories <= journal.total_directories
            && journal.restore_id.starts_with(RESTORE_ID_PREFIX)
            && validate_snapshot_id(&journal.target_snapshot_id).is_ok()
            && validate_snapshot_id(&journal.source_snapshot_id).is_ok()
            && journal
                .unrevert_of
                .as_deref()
                .is_none_or(|original| original.starts_with(RESTORE_ID_PREFIX));
        if !valid {
            return Err(SnapshotError::UnhealthyStorage);
        }
        Ok(journal)
    }

    /// Padded to the longest encoding any later state of this journal can have, so rewriting it
    /// in place never needs more room than its first write reserved.
    fn encode(&self) -> Result<Vec<u8>, SnapshotError> {
        let mut bytes = serde_json::to_vec(self).map_err(|_| SnapshotError::OperationFailed)?;
        let longest = Self {
            state: StoredRestoreState::Indeterminate,
            applied_files: self.total_files,
            created_directories: self.total_directories,
            acknowledgement_required: false,
            reconciliation_required: false,
            ..self.clone()
        };
        let longest = serde_json::to_vec(&longest)
            .map_err(|_| SnapshotError::OperationFailed)?
            .len();
        bytes.resize(longest.max(bytes.len()), b' ');
        Ok(bytes)
    }
}

fn sample(
    difference: &Difference<'_>,
    kind: SnapshotChangeKind,
    current: Option<&str>,
) -> Result<SnapshotChange, SnapshotError> {
    Ok(SnapshotChange {
        path: WorkspacePath::new(difference.path).map_err(|_| SnapshotError::IntegrityFailure)?,
        resource_id: path_resource_id(difference.path)?,
        kind,
        current_revision: current.map(revision).transpose()?,
        target_revision: difference
            .target
            .map(|entry| revision(&entry.digest))
            .transpose()?,
    })
}

/// How a failed publication step leaves the restore.
fn refusal(error: SnapshotTreeError) -> Publication {
    match error {
        SnapshotTreeError::Changed | SnapshotTreeError::Blocked => {
            Publication::Refused(SnapshotError::Conflict)
        }
        SnapshotTreeError::Protected => Publication::Refused(SnapshotError::UnsupportedFile),
        SnapshotTreeError::Failed(error) if error.kind() == io::ErrorKind::InvalidData => {
            Publication::Refused(blob_error(error))
        }
        SnapshotTreeError::Unsettled(_) => Publication::Uncertain,
        error => Publication::Refused(tree_error(error)),
    }
}
