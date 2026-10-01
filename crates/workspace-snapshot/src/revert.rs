//! Reverts: the selected records composed per path and published over the live workspace, each path
//! only while it still holds what the records left there. Any conflict refuses the whole operation
//! before anything is written. A holder's reverts stack until acknowledged, and an unrevert
//! re-applies the whole stack. Journals record transitions, never content: records hold both sides
//! of every change, so an interrupted revert is re-planned from them.

use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    mem::size_of,
};

use tokio_util::sync::CancellationToken;
use workcell_host_contract::{
    MAX_REVERT_JOURNALS, MAX_REVERT_PREVIEW_PATHS, MAX_SNAPSHOT_FILE_BYTES, PendingRevert,
    RevertChangeKind, RevertConflict, RevertConflictKind, RevertCounts, RevertDirection,
    RevertPath, RevertPreview, RevertState, RevertStatus, SnapshotLimit, UnrecordedReason,
    WorkspacePath,
};
use workcell_mcp_files::{
    SnapshotTreeContent, SnapshotTreeError, SnapshotTreeExpected, SnapshotTreeObserved,
    SnapshotTreeStamp,
};
use workcell_snapshot_store::{Content, EntryKind, blob_id};

use crate::{
    Inner, SnapshotError, Workspace,
    capture::{Stability, read_stable},
    check_cancelled,
    format::{StoredJournal, StoredRecord, count, holder, revert_id},
    identifier, quota_error,
    snapshot::{OWNER_EXECUTABLE, file_kind, permission_bits, store_error, workspace_path},
    store::{RECORDS, REVERTS},
    tree_error,
};

const READ_BITS: u32 = 0o444;
const EXECUTE_BITS: u32 = 0o111;
/// What git creates a file with, before the umask.
const NEW_FILE_MODE: u32 = 0o666;
const NEW_EXECUTABLE_MODE: u32 = 0o777;

type State = Option<Content>;

/// One path's selected changes composed into one: the operation replaces `source` with `target`.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct Composed {
    pub(crate) path: String,
    pub(crate) source: State,
    pub(crate) target: State,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct Conflict {
    pub(crate) path: String,
    pub(crate) kind: RevertConflictKind,
    pub(crate) reason: Option<UnrecordedReason>,
}

pub(crate) struct RevertPlan {
    pub(crate) revert_id: String,
    pub(crate) holder: String,
    pub(crate) direction: RevertDirection,
    pub(crate) seqs: Vec<u64>,
    /// The journals an unrevert undoes, as they stood when it was planned.
    stack: Vec<String>,
    changes: Vec<PlannedChange>,
    created_directories: Vec<WorkspacePath>,
    conflicts: usize,
    pub(crate) preview: RevertPreview,
}

struct PlannedChange {
    path: WorkspacePath,
    expected: SnapshotTreeExpected,
    /// What the path held when planned.
    source: State,
    target: State,
    /// The permission bits of the file it replaces, which a published file keeps.
    replaced_mode: Option<u32>,
}

enum Live {
    Absent,
    Entry {
        content: Content,
        stamp: SnapshotTreeStamp,
        /// Permission bits, for a regular file.
        mode: Option<u32>,
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
    workspace: &'a Workspace,
    token: &'a CancellationToken,
    directories: BTreeMap<String, Directory>,
    created: BTreeSet<String>,
    changes: Vec<PlannedChange>,
    counts: RevertCounts,
    planned: Vec<RevertPath>,
    conflicts: Vec<Conflict>,
}

enum Publication {
    Complete,
    /// Stopped before changing the entry it was on, so exactly what was counted is published.
    Refused(SnapshotError),
    /// An entry may or may not have changed.
    Uncertain,
}

/// Composes each path's changes across `records`, in seq order. Consecutive changes must chain,
/// and a path any record could not store cannot be composed at all.
pub(crate) fn compose(
    records: &[StoredRecord],
    direction: RevertDirection,
) -> (Vec<Composed>, Vec<Conflict>) {
    let mut edges = BTreeMap::<&str, Vec<(State, State)>>::new();
    let mut unrecorded = BTreeMap::<&str, UnrecordedReason>::new();
    for record in records {
        for change in &record.changes {
            edges.entry(change.path.as_str()).or_default().push((
                change.before.map(|content| content.0),
                change.after.map(|content| content.0),
            ));
        }
        for path in &record.unrecorded {
            unrecorded.entry(path.path.as_str()).or_insert(path.reason);
        }
    }
    let mut conflicts = unrecorded
        .iter()
        .map(|(path, reason)| Conflict {
            path: (*path).to_owned(),
            kind: RevertConflictKind::Unrecorded,
            reason: Some(*reason),
        })
        .collect::<Vec<_>>();
    let mut composed = Vec::new();
    for (path, chain) in edges {
        if unrecorded.contains_key(path) {
            continue;
        }
        if !chain.windows(2).all(|pair| pair[0].1 == pair[1].0) {
            conflicts.push(Conflict {
                path: path.to_owned(),
                kind: RevertConflictKind::Interleaved,
                reason: None,
            });
            continue;
        }
        let (before, after) = (chain[0].0, chain[chain.len() - 1].1);
        if before == after {
            continue;
        }
        let (source, target) = match direction {
            RevertDirection::Revert => (after, before),
            RevertDirection::Unrevert => (before, after),
        };
        composed.push(Composed {
            path: path.to_owned(),
            source,
            target,
        });
    }
    conflicts.sort_unstable_by(|left, right| left.path.cmp(&right.path));
    (composed, conflicts)
}

impl RevertPlan {
    /// Every path the plan writes, the directories it creates first.
    pub(crate) fn paths(&self) -> impl Iterator<Item = &WorkspacePath> {
        self.created_directories
            .iter()
            .chain(self.changes.iter().map(|change| &change.path))
    }

    /// Conservative bytes the plan holds while it waits to be executed.
    pub(crate) fn retained_bytes(&self) -> usize {
        let paths = |paths: &[WorkspacePath]| {
            paths
                .iter()
                .map(WorkspacePath::retained_bytes)
                .fold(0, usize::saturating_add)
        };
        let preview = self
            .preview
            .planned
            .iter()
            .map(|path| size_of::<RevertPath>().saturating_add(path.path.retained_bytes()))
            .chain(self.preview.conflicts.iter().map(|conflict| {
                size_of::<RevertConflict>().saturating_add(conflict.path.retained_bytes())
            }))
            .fold(
                paths(&self.preview.created_directories),
                usize::saturating_add,
            );
        let changes = self
            .changes
            .iter()
            .map(|change| size_of::<PlannedChange>().saturating_add(change.path.retained_bytes()))
            .fold(0, usize::saturating_add);
        [&self.revert_id, &self.holder]
            .into_iter()
            .chain(&self.stack)
            .map(String::capacity)
            .fold(size_of::<Self>(), usize::saturating_add)
            .saturating_add(self.seqs.capacity().saturating_mul(size_of::<u64>()))
            .saturating_add(changes)
            .saturating_add(paths(&self.created_directories))
            .saturating_add(preview)
    }
}

impl Inner {
    /// Plans reverting `seqs`, which `holder` must hold and no pending revert may name.
    pub(crate) fn prepare_revert(
        &self,
        holder: &str,
        seqs: &[u64],
        token: &CancellationToken,
    ) -> Result<RevertPlan, SnapshotError> {
        let seqs = seqs.iter().copied().collect::<BTreeSet<_>>();
        if seqs.is_empty() {
            return Err(SnapshotError::InvalidRequest);
        }
        let seqs = seqs.into_iter().collect::<Vec<_>>();
        self.settle()?;
        let journals = self.store.journals()?;
        if journals.len() >= MAX_REVERT_JOURNALS {
            return Err(quota_error(SnapshotLimit::Journals, MAX_REVERT_JOURNALS));
        }
        if journals.iter().any(|journal| {
            journal
                .seqs
                .iter()
                .any(|seq| seqs.binary_search(seq).is_ok())
        }) {
            return Err(SnapshotError::Conflict);
        }
        let records = self.held(holder, &seqs)?;
        self.plan(
            revert_id(),
            holder,
            RevertDirection::Revert,
            seqs,
            Vec::new(),
            &records,
            token,
        )
    }

    /// Plans re-applying every revert `holder` has pending.
    pub(crate) fn prepare_unrevert(
        &self,
        holder: &str,
        token: &CancellationToken,
    ) -> Result<RevertPlan, SnapshotError> {
        self.settle()?;
        let journals = self.store.journals()?;
        if journals.len() >= MAX_REVERT_JOURNALS {
            return Err(quota_error(SnapshotLimit::Journals, MAX_REVERT_JOURNALS));
        }
        let stack = stack_of(&journals, holder);
        if stack.is_empty() {
            return Err(SnapshotError::NotFound);
        }
        let seqs = reverted_seqs(&stack).into_iter().collect::<Vec<_>>();
        let records = self.named(&seqs)?;
        self.plan(
            revert_id(),
            holder,
            RevertDirection::Unrevert,
            seqs,
            stack
                .iter()
                .map(|journal| journal.revert_id.clone())
                .collect(),
            &records,
            token,
        )
    }

    /// Publishes a plan whose records and stack are still as planned, journaling as it goes.
    pub(crate) fn execute_revert(
        &self,
        plan: &RevertPlan,
        token: &CancellationToken,
    ) -> Result<RevertStatus, SnapshotError> {
        let workspace = self.workspace()?;
        self.settle()?;
        let journals = self.store.journals()?;
        let stack = stack_of(&journals, &plan.holder);
        let unchanged = match plan.direction {
            RevertDirection::Revert => {
                self.held(&plan.holder, &plan.seqs)?;
                !journals.iter().any(|journal| {
                    journal
                        .seqs
                        .iter()
                        .any(|seq| plan.seqs.binary_search(seq).is_ok())
                })
            }
            RevertDirection::Unrevert => stack
                .iter()
                .map(|journal| &journal.revert_id)
                .eq(&plan.stack),
        };
        if !unchanged || plan.conflicts > 0 {
            return Err(SnapshotError::Conflict);
        }
        if journals.len() >= MAX_REVERT_JOURNALS {
            return Err(quota_error(SnapshotLimit::Journals, MAX_REVERT_JOURNALS));
        }
        let mut journal = StoredJournal::new(
            plan.revert_id.clone(),
            &plan.holder,
            stack
                .last()
                .map_or(0, |journal| journal.order.saturating_add(1)),
            plan.direction,
            plan.seqs.clone(),
            plan.changes.len(),
            plan.created_directories.len(),
        );
        if plan.changes.is_empty() && plan.created_directories.is_empty() {
            journal.state = RevertState::Completed;
        }
        self.store.write_journal(&journal)?;
        if journal.state == RevertState::Publishing {
            match self.publish(workspace, plan, &mut journal, token) {
                Publication::Complete => journal.state = RevertState::Completed,
                Publication::Refused(error)
                    if journal.applied_files == 0 && journal.created_directories == 0 =>
                {
                    self.store
                        .remove(&self.store.journal_path(&journal.revert_id)?)?;
                    self.store.sync(REVERTS)?;
                    return Err(error);
                }
                Publication::Refused(_) => journal.state = RevertState::Partial,
                Publication::Uncertain => {
                    journal.state = RevertState::Indeterminate;
                    journal.reconciliation_required = true;
                }
            }
            self.store.write_journal(&journal)?;
        }
        self.settled(&journal)?;
        self.status(&plan.holder)
    }

    /// Settles `holder`'s pending reverts: their records are deleted for every holder, since what
    /// they changed is gone from the workspace.
    pub(crate) fn acknowledge(&self, holder: &str) -> Result<RevertStatus, SnapshotError> {
        self.settle()?;
        let journals = self.store.journals()?;
        let stack = stack_of(&journals, holder);
        let mut deleted = 0_u64;
        for seq in reverted_seqs(&stack) {
            if self.store.remove(&self.store.record_path(seq))? {
                deleted += 1;
            }
        }
        if deleted > 0 {
            self.store.sync(RECORDS)?;
            let mut state = self.store.state()?;
            state.records = state.records.saturating_sub(deleted);
            self.store.write_state(&state)?;
        }
        self.clear_stack(holder)?;
        self.status(holder)
    }

    pub(crate) fn status(&self, holder_id: &str) -> Result<RevertStatus, SnapshotError> {
        let journals = self.store.journals()?;
        Ok(RevertStatus {
            holder: holder(holder_id)?,
            pending: stack_of(&journals, holder_id)
                .into_iter()
                .map(|journal| {
                    Ok(PendingRevert {
                        revert_id: identifier(&journal.revert_id)?,
                        direction: journal.direction,
                        state: journal.state,
                        records: count(journal.seqs.len()),
                        applied_files: count(journal.applied_files),
                        total_files: count(journal.total_files),
                        reconciliation_required: journal.reconciliation_required,
                        stopped_at: journal
                            .stopped_at
                            .as_deref()
                            .map(workspace_path)
                            .transpose()?,
                    })
                })
                .collect::<Result<_, SnapshotError>>()?,
        })
    }

    /// Reconciles every journal a crash left publishing: under the store lock, no other one is.
    /// Without the workspace there is nothing to reconcile against, so they stay as they are.
    pub(crate) fn settle(&self) -> Result<(), SnapshotError> {
        if self.workspace.is_none() {
            return Ok(());
        }
        for mut journal in self.store.journals()? {
            if journal.state == RevertState::Publishing {
                self.reconcile(&mut journal);
                self.store.write_journal(&journal)?;
                self.settled(&journal)?;
            }
        }
        Ok(())
    }

    /// Every seq a pending revert names, whichever holder made it.
    pub(crate) fn reverted(&self) -> Result<BTreeSet<u64>, SnapshotError> {
        Ok(self
            .store
            .journals()?
            .iter()
            .flat_map(|journal| journal.seqs.iter().copied())
            .collect())
    }

    pub(crate) fn clear_stack(&self, holder: &str) -> Result<(), SnapshotError> {
        let mut removed = false;
        for journal in self.store.journals()? {
            if journal.holder == holder {
                removed |= self
                    .store
                    .remove(&self.store.journal_path(&journal.revert_id)?)?;
            }
        }
        if removed {
            self.store.sync(REVERTS)?;
        }
        Ok(())
    }

    /// An unrevert that completed undid its holder's whole stack, which is then gone.
    fn settled(&self, journal: &StoredJournal) -> Result<(), SnapshotError> {
        if journal.direction == RevertDirection::Unrevert && journal.state == RevertState::Completed
        {
            self.clear_stack(&journal.holder)?;
        }
        Ok(())
    }

    /// Recomputes an interrupted revert from its records and the live workspace.
    fn reconcile(&self, journal: &mut StoredJournal) {
        let replanned = self.named(&journal.seqs).and_then(|records| {
            self.plan(
                journal.revert_id.clone(),
                &journal.holder,
                journal.direction,
                journal.seqs.clone(),
                Vec::new(),
                &records,
                &CancellationToken::new(),
            )
        });
        match replanned {
            Ok(plan) if plan.conflicts == 0 => {
                journal.applied_files = journal.total_files.saturating_sub(plan.changes.len());
                journal.state = if plan.changes.is_empty() {
                    RevertState::Completed
                } else {
                    RevertState::Partial
                };
            }
            _ => {
                journal.state = RevertState::Indeterminate;
                journal.reconciliation_required = true;
            }
        }
    }

    /// The records `seqs` names, each held by `holder`.
    fn held(&self, holder: &str, seqs: &[u64]) -> Result<Vec<StoredRecord>, SnapshotError> {
        seqs.iter()
            .map(|seq| {
                self.store
                    .record(*seq)?
                    .filter(|record| record.holders.contains(holder))
                    .ok_or(SnapshotError::NotFound)
            })
            .collect()
    }

    /// The records a pending revert names, which nothing deletes while it is pending.
    fn named(&self, seqs: &[u64]) -> Result<Vec<StoredRecord>, SnapshotError> {
        seqs.iter()
            .map(|seq| {
                self.store
                    .record(*seq)?
                    .ok_or(SnapshotError::UnhealthyStorage)
            })
            .collect()
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "a plan names everything its journal will"
    )]
    fn plan(
        &self,
        revert_id: String,
        holder_id: &str,
        direction: RevertDirection,
        seqs: Vec<u64>,
        stack: Vec<String>,
        records: &[StoredRecord],
        token: &CancellationToken,
    ) -> Result<RevertPlan, SnapshotError> {
        let (composed, conflicts) = compose(records, direction);
        let mut planner = Planner {
            workspace: self.workspace()?,
            token,
            directories: BTreeMap::new(),
            created: BTreeSet::new(),
            changes: Vec::new(),
            counts: RevertCounts::default(),
            planned: Vec::new(),
            conflicts,
        };
        for change in &composed {
            planner.visit(change)?;
        }
        let mut conflicts = planner.conflicts;
        conflicts.sort_unstable_by(|left, right| left.path.cmp(&right.path));
        planner.counts.conflicts = count(conflicts.len());
        planner.counts.created_directories = count(planner.created.len());
        let created_directories = planner
            .created
            .iter()
            .map(|path| workspace_path(path))
            .collect::<Result<Vec<_>, _>>()?;
        let preview = RevertPreview {
            revert_id: identifier(&revert_id)?,
            holder: holder(holder_id)?,
            direction,
            records: count(seqs.len()),
            counts: planner.counts,
            planned: planner.planned,
            conflicts: conflicts
                .iter()
                .take(MAX_REVERT_PREVIEW_PATHS)
                .map(|conflict| {
                    Ok(RevertConflict {
                        path: workspace_path(&conflict.path)?,
                        kind: conflict.kind,
                        reason: conflict.reason,
                    })
                })
                .collect::<Result<_, SnapshotError>>()?,
            created_directories: created_directories
                .iter()
                .take(MAX_REVERT_PREVIEW_PATHS)
                .cloned()
                .collect(),
        };
        Ok(RevertPlan {
            revert_id,
            holder: holder_id.to_owned(),
            direction,
            seqs,
            stack,
            changes: planner.changes,
            created_directories,
            conflicts: conflicts.len(),
            preview,
        })
    }

    fn publish(
        &self,
        workspace: &Workspace,
        plan: &RevertPlan,
        journal: &mut StoredJournal,
        token: &CancellationToken,
    ) -> Publication {
        for directory in &plan.created_directories {
            if token.is_cancelled() {
                return Publication::Refused(SnapshotError::Cancelled);
            }
            match workspace.access.create_tree_directory(directory) {
                Ok(()) => journal.created_directories += 1,
                Err(error) => return stopped(journal, directory, refusal(error)),
            }
        }
        let mut replaced = Vec::new();
        for change in &plan.changes {
            if token.is_cancelled() {
                return Publication::Refused(SnapshotError::Cancelled);
            }
            let reread = match relinked(workspace, change, &replaced, token) {
                Ok(reread) => reread,
                Err(publication) => return stopped(journal, &change.path, publication),
            };
            let expected = reread.as_ref().unwrap_or(&change.expected);
            if let Err(publication) = self.publish_change(workspace, change, expected) {
                return stopped(journal, &change.path, publication);
            }
            if let SnapshotTreeExpected::Present(stamp) = &change.expected {
                replaced.push(stamp);
            }
            journal.applied_files += 1;
            #[cfg(test)]
            self.store.hooks.published(change.path.as_str());
        }
        Publication::Complete
    }

    fn publish_change(
        &self,
        workspace: &Workspace,
        change: &PlannedChange,
        expected: &SnapshotTreeExpected,
    ) -> Result<(), Publication> {
        let Some(target) = change.target else {
            return workspace
                .access
                .publish_tree_entry(&change.path, expected, SnapshotTreeContent::Absent)
                .map_err(refusal);
        };
        let bytes = self
            .store
            .objects()
            .read_blob(&target.oid)
            .map_err(|error| Publication::Refused(store_error(error)))?;
        let mut source = bytes.as_slice();
        let content = match target.kind {
            EntryKind::Symlink => SnapshotTreeContent::Symlink { target: &bytes },
            kind => SnapshotTreeContent::File {
                source: &mut source,
                mode: restored_mode(
                    change.replaced_mode,
                    kind == EntryKind::Executable,
                    workspace.umask,
                ),
            },
        };
        workspace
            .access
            .publish_tree_entry(&change.path, expected, content)
            .map_err(refusal)
    }
}

/// What to publish `change` over when this revert already replaced or removed another name of the
/// same file, as planned: that moved the file's change time, so the path is read again and must
/// still hold what the plan found there.
fn relinked(
    workspace: &Workspace,
    change: &PlannedChange,
    replaced: &[&SnapshotTreeStamp],
    token: &CancellationToken,
) -> Result<Option<SnapshotTreeExpected>, Publication> {
    let SnapshotTreeExpected::Present(planned) = &change.expected else {
        return Ok(None);
    };
    if !replaced.contains(&planned) {
        return Ok(None);
    }
    match observe(workspace, &change.path, token).map_err(Publication::Refused)? {
        Live::Entry { content, stamp, .. } if change.source == Some(content) => {
            Ok(Some(SnapshotTreeExpected::Present(stamp)))
        }
        _ => Err(Publication::Refused(SnapshotError::Conflict)),
    }
}

impl Planner<'_> {
    fn visit(&mut self, change: &Composed) -> Result<(), SnapshotError> {
        check_cancelled(self.token)?;
        let path = workspace_path(&change.path)?;
        let live = observe(self.workspace, &path, self.token)?;
        if live.matches(change.target) {
            self.counts.unchanged = self.counts.unchanged.saturating_add(1);
            return Ok(());
        }
        let (expected, replaced_mode) = match (&live, live.matches(change.source)) {
            (Live::Absent, true) => (SnapshotTreeExpected::Absent, None),
            (Live::Entry { stamp, mode, .. }, true) => {
                (SnapshotTreeExpected::Present(stamp.clone()), *mode)
            }
            _ => {
                self.changed_since(change);
                return Ok(());
            }
        };
        if change.target.is_some() && !self.parents_ready(&change.path)? {
            self.changed_since(change);
            return Ok(());
        }
        let (kind, count) = match (change.source, change.target) {
            (None, _) => (RevertChangeKind::Create, &mut self.counts.create),
            (Some(_), Some(_)) => (RevertChangeKind::Replace, &mut self.counts.replace),
            (Some(_), None) => (RevertChangeKind::Delete, &mut self.counts.delete),
        };
        *count = count.saturating_add(1);
        if self.planned.len() < MAX_REVERT_PREVIEW_PATHS {
            self.planned.push(RevertPath {
                path: path.clone(),
                kind,
            });
        }
        self.changes.push(PlannedChange {
            path,
            expected,
            source: change.source,
            target: change.target,
            replaced_mode,
        });
        Ok(())
    }

    fn changed_since(&mut self, change: &Composed) {
        self.conflicts.push(Conflict {
            path: change.path.clone(),
            kind: RevertConflictKind::ChangedSince,
            reason: None,
        });
    }

    fn observe_directory(&self, path: &str) -> Result<Directory, SnapshotError> {
        match self
            .workspace
            .access
            .observe_tree_entry(&workspace_path(path)?)
        {
            Ok(SnapshotTreeObserved::Directory) => Ok(Directory::Present),
            Ok(SnapshotTreeObserved::Absent) => Ok(Directory::Missing),
            Ok(_) | Err(SnapshotTreeError::Blocked | SnapshotTreeError::Protected) => {
                Ok(Directory::Blocked)
            }
            Err(error) => Err(tree_error(error)),
        }
    }

    /// Whether every ancestor is a plain directory or can be created, noting those to create.
    fn parents_ready(&mut self, path: &str) -> Result<bool, SnapshotError> {
        let mut missing = false;
        for ancestor in path.match_indices('/').map(|(index, _)| &path[..index]) {
            let state = match self.directories.get(ancestor) {
                Some(state) => *state,
                None if missing => Directory::Missing,
                None => self.observe_directory(ancestor)?,
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
    /// Equal kind, which for a file is whether it is executable, and equal content.
    fn matches(&self, content: State) -> bool {
        match (self, content) {
            (Self::Absent, None) => true,
            (Self::Entry { content: live, .. }, Some(content)) => *live == content,
            _ => false,
        }
    }
}

/// The live entry at `path`, read the way a capture reads it.
fn observe(
    workspace: &Workspace,
    path: &WorkspacePath,
    token: &CancellationToken,
) -> Result<Live, SnapshotError> {
    match workspace.access.observe_tree_entry(path) {
        Ok(SnapshotTreeObserved::Absent) => Ok(Live::Absent),
        Ok(SnapshotTreeObserved::File(mut file)) => Ok(
            match read_stable(&mut file.file, MAX_SNAPSHOT_FILE_BYTES, token)? {
                Stability::Stable { content, metadata } => {
                    blob_id(&content).map_or(Live::Other, |oid| Live::Entry {
                        content: Content {
                            kind: file_kind(&metadata),
                            oid,
                        },
                        stamp: SnapshotTreeStamp::of(&metadata),
                        mode: Some(permission_bits(&metadata)),
                    })
                }
                Stability::Oversized | Stability::Unstable => Live::Other,
            },
        ),
        Ok(SnapshotTreeObserved::Symlink(link)) => Ok(blob_id(&link.target).map_or(
            Live::Other,
            |oid| Live::Entry {
                content: Content {
                    kind: EntryKind::Symlink,
                    oid,
                },
                stamp: link.stamp,
                mode: None,
            },
        )),
        Ok(SnapshotTreeObserved::Directory | SnapshotTreeObserved::Other)
        | Err(SnapshotTreeError::Blocked | SnapshotTreeError::Protected) => Ok(Live::Other),
        Err(error) => Err(tree_error(error)),
    }
}

/// A holder's journals, oldest first.
fn stack_of<'a>(journals: &'a [StoredJournal], holder: &str) -> Vec<&'a StoredJournal> {
    journals
        .iter()
        .filter(|journal| journal.holder == holder)
        .collect()
}

/// Every seq the stack's reverts name, which an unrevert re-applies and an acknowledgement deletes.
fn reverted_seqs(stack: &[&StoredJournal]) -> BTreeSet<u64> {
    stack
        .iter()
        .filter(|journal| journal.direction == RevertDirection::Revert)
        .flat_map(|journal| journal.seqs.iter().copied())
        .collect()
}

/// A published file keeps the permission bits of the file it replaces, gaining execute permission
/// for its owner and whoever may read it, or losing it for everyone. A new file gets the mode git
/// creates one with under the umask.
fn restored_mode(replaced: Option<u32>, executable: bool, umask: u32) -> u32 {
    match replaced {
        Some(mode) if (mode & OWNER_EXECUTABLE != 0) == executable => mode,
        Some(mode) if executable => mode | OWNER_EXECUTABLE | (mode & READ_BITS) >> 2,
        Some(mode) => mode & !EXECUTE_BITS,
        None if executable => NEW_EXECUTABLE_MODE & !umask,
        None => NEW_FILE_MODE & !umask,
    }
}

/// Journals `path` as where the revert stopped, left as it was unless `publication` is uncertain.
fn stopped(
    journal: &mut StoredJournal,
    path: &WorkspacePath,
    publication: Publication,
) -> Publication {
    journal.stopped_at = Some(path.as_str().to_owned());
    publication
}

/// How a failed publication step leaves the revert.
fn refusal(error: SnapshotTreeError) -> Publication {
    match error {
        SnapshotTreeError::Changed | SnapshotTreeError::Blocked => {
            Publication::Refused(SnapshotError::Conflict)
        }
        SnapshotTreeError::Protected => Publication::Refused(SnapshotError::UnsupportedFile),
        SnapshotTreeError::Failed(error) if error.kind() == io::ErrorKind::InvalidData => {
            Publication::Refused(SnapshotError::IntegrityFailure)
        }
        SnapshotTreeError::Unsettled(_) => Publication::Uncertain,
        error => Publication::Refused(tree_error(error)),
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;
    use workcell_snapshot_store::blob_id;

    use super::*;
    use crate::format::{StoredChange, StoredContent, StoredUnrecorded};

    const UMASK: u32 = 0o027;
    const PATH: &str = "file.txt";

    fn content(text: &str) -> State {
        Some(Content {
            kind: EntryKind::File,
            oid: blob_id(text.as_bytes()).unwrap(),
        })
    }

    fn record(seq: u64, edges: &[(&str, State, State)], unrecorded: &[&str]) -> StoredRecord {
        let mut record: StoredRecord = serde_json::from_value(serde_json::json!({
            "version": "workspace-change-record.v1",
            "seq": seq,
            "ticket": "rec_0123456789abcdef0123456789abcdef",
            "holders": ["holder"],
            "client": {},
            "changes": [],
            "unrecorded": [],
        }))
        .unwrap();
        record.changes = edges
            .iter()
            .map(|(path, before, after)| StoredChange {
                path: (*path).to_owned(),
                before: before.map(StoredContent),
                after: after.map(StoredContent),
            })
            .collect();
        record.unrecorded = unrecorded
            .iter()
            .map(|path| StoredUnrecorded {
                path: (*path).to_owned(),
                reason: UnrecordedReason::Oversized,
            })
            .collect();
        record
    }

    #[test_case(RevertDirection::Revert, content("c"), content("a") ; "revert goes back to the oldest state")]
    #[test_case(RevertDirection::Unrevert, content("a"), content("c") ; "unrevert goes forward to the newest state")]
    fn chained_records_compose_into_one_change(
        direction: RevertDirection,
        source: State,
        target: State,
    ) {
        let records = [
            record(1, &[(PATH, content("a"), content("b"))], &[]),
            record(2, &[(PATH, content("b"), content("c"))], &[]),
        ];

        let (composed, conflicts) = compose(&records, direction);

        assert_eq!(
            composed,
            [Composed {
                path: PATH.to_owned(),
                source,
                target,
            }]
        );
        assert!(conflicts.is_empty());
    }

    #[test]
    fn records_that_end_where_they_started_compose_to_nothing() {
        let records = [
            record(1, &[(PATH, content("a"), content("b"))], &[]),
            record(2, &[(PATH, content("b"), content("a"))], &[]),
        ];

        let (composed, conflicts) = compose(&records, RevertDirection::Revert);

        assert!(composed.is_empty());
        assert!(conflicts.is_empty());
    }

    #[test_case(
        &[(PATH, content("a"), content("b"))], &[(PATH, content("x"), content("c"))], &[],
        RevertConflictKind::Interleaved ;
        "records that do not chain"
    )]
    #[test_case(
        &[(PATH, content("a"), content("b"))], &[], &[PATH],
        RevertConflictKind::Unrecorded ;
        "a record that could not store the path"
    )]
    fn a_path_that_cannot_be_composed_is_a_conflict(
        first: &[(&str, State, State)],
        second: &[(&str, State, State)],
        unrecorded: &[&str],
        kind: RevertConflictKind,
    ) {
        let records = [record(1, first, &[]), record(2, second, unrecorded)];

        let (composed, conflicts) = compose(&records, RevertDirection::Revert);

        assert!(composed.is_empty());
        assert_eq!(
            conflicts
                .iter()
                .map(|conflict| (conflict.path.as_str(), conflict.kind))
                .collect::<Vec<_>>(),
            [(PATH, kind)]
        );
    }

    #[test_case(Some(0o640), false, 0o640; "a plain file stays exactly as it was")]
    #[test_case(Some(0o750), true, 0o750; "an executable file stays exactly as it was")]
    #[test_case(Some(0o640), true, 0o750; "execute follows read when a file becomes executable")]
    #[test_case(Some(0o200), true, 0o300; "the owner can always execute an executable file")]
    #[test_case(Some(0o751), false, 0o640; "no one can execute a file that is no longer executable")]
    #[test_case(None, false, 0o640; "a new file gets git's mode under the umask")]
    #[test_case(None, true, 0o750; "a new executable gets git's mode under the umask")]
    fn restored_modes_follow_git_and_keep_what_they_replace(
        replaced: Option<u32>,
        executable: bool,
        expected: u32,
    ) {
        assert_eq!(restored_mode(replaced, executable, UMASK), expected);
    }
}
