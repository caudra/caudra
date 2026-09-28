//! Restoring a snapshot, and undoing that restore, through a journal that a
//! crash at any point leaves recoverable.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use caudra_storage::id::CaudraId;
use serde::{Deserialize, Serialize};
use tempfile::{Builder, NamedTempFile};
use workcell::snapshot_store::{Change, Content, EntryKind, ObjectId, ObjectStore, blob_id};

use super::capture::{is_executable, link_target};
use super::storage::{
    JOURNAL_NAME, UNREVERT_NAME, abandon_on_error, is_legacy, read_optional_json,
    remove_file_durable, remove_if_present, write_json,
};
use super::{
    ConflictPolicy, JournalState, PathConflict, PathOutcome, PathOutcomeKind, RestoreReport,
    RestoreTarget, SnapshotError, SnapshotStore,
};

const JOURNAL_MISSING: &str = "restore journal";
const UNREVERT_MISSING: &str = "unrevert";
const JOURNAL_VANISHED: &str = "prepared restore journal disappeared";
const JOURNAL_CHANGED: &str = "restore journal changed while applying";
/// Beside the path it replaces, so the rename that publishes it is atomic.
const STAGING_PREFIX: &str = ".caudra-restore-";
#[cfg(unix)]
const NEW_FILE_MODE: u32 = 0o666;
#[cfg(unix)]
const NEW_EXECUTABLE_MODE: u32 = 0o777;
#[cfg(unix)]
const PERMISSION_BITS: u32 = 0o777;
#[cfg(unix)]
const EXECUTE_BITS: u32 = 0o111;
#[cfg(unix)]
const READ_BITS: u32 = 0o444;
#[cfg(unix)]
const OWNER_EXECUTE: u32 = 0o100;
/// Shifting a read bit this far lands on the execute bit of the same class.
#[cfg(unix)]
const READ_TO_EXECUTE_SHIFT: u32 = 2;

#[derive(Debug, Serialize, Deserialize)]
struct RestoreJournal {
    state: JournalState,
    root: PathBuf,
    paths: Vec<JournalPath>,
    destination: RestoreTarget,
    policy: ConflictPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    operation_id: Option<CaudraId>,
}

/// One path the restore changes: what it held before, what the restore makes
/// it hold, and how the report describes that.
#[derive(Debug, Serialize, Deserialize)]
struct JournalPath {
    path: String,
    #[serde(with = "stored_content")]
    before: Option<Content>,
    #[serde(with = "stored_content")]
    target: Option<Content>,
    outcome: PathOutcomeKind,
}

/// What every path restored since the last unrevert held before the first of
/// those restores, and holds after the latest.
#[derive(Debug, Default, Serialize, Deserialize)]
struct UnrevertRecord {
    paths: BTreeMap<String, UnrevertPath>,
}

#[derive(Debug, Serialize, Deserialize)]
struct UnrevertPath {
    #[serde(with = "stored_content")]
    before: Option<Content>,
    #[serde(with = "stored_content")]
    after: Option<Content>,
}

impl RestoreJournal {
    fn report(&self, recovered: bool) -> RestoreReport {
        RestoreReport {
            target: self.destination,
            paths: self
                .paths
                .iter()
                .map(|path| PathOutcome {
                    path: path.path.clone(),
                    kind: path.outcome,
                })
                .collect(),
            recovered,
            operation_id: self.operation_id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Observed {
    Absent,
    Present(Content),
    Other,
}

impl Observed {
    fn content(self) -> Option<Content> {
        match self {
            Self::Present(content) => Some(content),
            Self::Absent | Self::Other => None,
        }
    }

    fn matches(self, expected: Option<Content>) -> bool {
        match (self, expected) {
            (Self::Absent, None) => true,
            (Self::Present(actual), Some(expected)) => actual == expected,
            (Self::Other, _) | (Self::Absent, Some(_)) | (Self::Present(_), None) => false,
        }
    }
}

enum Step {
    Absent,
    Advanced,
    Finished(RestoreReport),
}

impl SnapshotStore {
    #[cfg(test)]
    pub(super) fn restore(
        &self,
        cwd: &Path,
        source_checkpoint_and_ancestors: &[CaudraId],
        target_checkpoint_and_ancestors: &[CaudraId],
    ) -> Result<RestoreReport, SnapshotError> {
        self.restore_impl(
            cwd,
            source_checkpoint_and_ancestors,
            target_checkpoint_and_ancestors,
            ConflictPolicy::Abort,
            None,
        )
    }

    pub fn restore_transaction_with_policy(
        &self,
        cwd: &Path,
        source_checkpoint_and_ancestors: &[CaudraId],
        target_checkpoint_and_ancestors: &[CaudraId],
        policy: ConflictPolicy,
        operation_id: CaudraId,
    ) -> Result<RestoreReport, SnapshotError> {
        self.restore_impl(
            cwd,
            source_checkpoint_and_ancestors,
            target_checkpoint_and_ancestors,
            policy,
            Some(operation_id),
        )
    }

    pub(super) fn restore_impl(
        &self,
        cwd: &Path,
        source_checkpoint_and_ancestors: &[CaudraId],
        target_checkpoint_and_ancestors: &[CaudraId],
        policy: ConflictPolicy,
        operation_id: Option<CaudraId>,
    ) -> Result<RestoreReport, SnapshotError> {
        let _lock = self.lock()?;
        let root = self.bind_root(cwd)?;
        if let Some(report) = self.finish_matching_pending(&root, true, operation_id)? {
            match operation_id {
                Some(expected) if report.operation_id == Some(expected) => return Ok(report),
                Some(expected) => {
                    return Err(SnapshotError::RestoreOperationMismatch {
                        expected,
                        actual: report.operation_id,
                    });
                }
                None => {
                    if let Some(operation_id) = report.operation_id {
                        return Err(SnapshotError::RestoreOperationPending(operation_id));
                    }
                }
            }
        }
        let source = self.resolve_snapshot(source_checkpoint_and_ancestors)?;
        let target = self.resolve_snapshot(target_checkpoint_and_ancestors)?;
        let changes = self
            .open_repository()?
            .changes(&source.id, &target.id)?
            .changes;
        let destination = RestoreTarget::Snapshot(target.key);
        self.prepare_restore(&root, changes, destination, policy, operation_id)?;
        let report = self
            .finish_matching_pending(&root, false, operation_id)?
            .ok_or_else(|| io::Error::other(JOURNAL_VANISHED))?;
        validate_report(&report, operation_id, destination)?;
        Ok(report)
    }

    #[cfg(test)]
    pub(super) fn unrevert(&self, cwd: &Path) -> Result<RestoreReport, SnapshotError> {
        self.unrevert_impl(cwd, ConflictPolicy::Abort, None)
    }

    pub fn unrevert_transaction_with_policy(
        &self,
        cwd: &Path,
        policy: ConflictPolicy,
        operation_id: CaudraId,
    ) -> Result<RestoreReport, SnapshotError> {
        self.unrevert_impl(cwd, policy, Some(operation_id))
    }

    fn unrevert_impl(
        &self,
        cwd: &Path,
        policy: ConflictPolicy,
        operation_id: Option<CaudraId>,
    ) -> Result<RestoreReport, SnapshotError> {
        let _lock = self.lock()?;
        let root = self.bind_root(cwd)?;
        if let Some(report) = self.finish_matching_pending(&root, true, operation_id)? {
            match operation_id {
                Some(expected)
                    if report.operation_id == Some(expected)
                        && report.target == RestoreTarget::Unrevert =>
                {
                    return Ok(report);
                }
                Some(expected) => {
                    return Err(SnapshotError::RestoreOperationMismatch {
                        expected,
                        actual: report.operation_id,
                    });
                }
                None if report.operation_id.is_none()
                    && report.target == RestoreTarget::Unrevert =>
                {
                    return Ok(report);
                }
                None => {
                    if let Some(operation_id) = report.operation_id {
                        return Err(SnapshotError::RestoreOperationPending(operation_id));
                    }
                }
            }
        }
        let record = self
            .read_unrevert()?
            .ok_or_else(|| SnapshotError::NotFound(UNREVERT_MISSING.to_owned()))?;
        let changes = record
            .paths
            .into_iter()
            .map(|(path, contents)| Change {
                path,
                source: contents.after,
                target: contents.before,
            })
            .collect();
        self.prepare_restore(
            &root,
            changes,
            RestoreTarget::Unrevert,
            policy,
            operation_id,
        )?;
        let report = self
            .finish_matching_pending(&root, false, operation_id)?
            .ok_or_else(|| io::Error::other(JOURNAL_VANISHED))?;
        validate_report(&report, operation_id, RestoreTarget::Unrevert)?;
        Ok(report)
    }

    pub fn recover(&self, cwd: &Path) -> Result<Option<RestoreReport>, SnapshotError> {
        let _lock = self.lock()?;
        let root = self.bind_root(cwd)?;
        self.finish_pending(&root, true)
    }

    pub fn journal_state(&self) -> Result<Option<JournalState>, SnapshotError> {
        Ok(self.read_journal()?.map(|journal| journal.state))
    }

    pub fn journal_operation_id(&self) -> Result<Option<CaudraId>, SnapshotError> {
        Ok(self
            .read_journal()?
            .and_then(|journal| journal.operation_id))
    }

    pub fn acknowledge_operation(
        &self,
        cwd: &Path,
        operation_id: CaudraId,
    ) -> Result<(), SnapshotError> {
        let _lock = self.lock()?;
        let root = self.bind_root(cwd)?;
        let Some(journal) = self.read_journal()? else {
            return Ok(());
        };
        if journal.root != root {
            return Err(SnapshotError::JournalRootMismatch {
                expected: journal.root,
                actual: root,
            });
        }
        if journal.operation_id != Some(operation_id) {
            return Err(SnapshotError::RestoreOperationMismatch {
                expected: operation_id,
                actual: journal.operation_id,
            });
        }
        if journal.state != JournalState::Cleared {
            return Err(SnapshotError::RestoreOperationNotApplied(operation_id));
        }
        remove_file_durable(&self.journal_path())
    }

    /// Takes the store lock: collection reads the unrevert record to learn
    /// which objects are live, so a delete racing that read would let it
    /// collect objects only the unrevert still needs.
    pub fn discard_unrevert(&self) -> Result<(), SnapshotError> {
        let _lock = self.lock()?;
        if self.dir.try_exists()? {
            self.ensure_dirs()?;
        }
        remove_if_present(&self.unrevert_path())
    }

    /// Records what the restore will change and what each path holds now,
    /// storing that content so an unrevert can put it back. Nothing in the
    /// worktree changes until the journal is on disk.
    pub(super) fn prepare_restore(
        &self,
        root: &Path,
        changes: Vec<Change>,
        destination: RestoreTarget,
        policy: ConflictPolicy,
        operation_id: Option<CaudraId>,
    ) -> Result<(), SnapshotError> {
        let repository = self.open_repository()?;
        let observed = journal_paths(&repository, root, changes, policy);
        let paths = abandon_on_error(&repository, observed)?;
        repository.sync()?;
        self.write_journal(&RestoreJournal {
            state: JournalState::Prepare,
            root: root.to_path_buf(),
            paths,
            destination,
            policy,
            operation_id,
        })
    }

    fn finish_pending(
        &self,
        root: &Path,
        recovered: bool,
    ) -> Result<Option<RestoreReport>, SnapshotError> {
        loop {
            match self.advance_journal(root, recovered)? {
                Step::Absent => return Ok(None),
                Step::Advanced => {}
                Step::Finished(report) => return Ok(Some(report)),
            }
        }
    }

    fn finish_matching_pending(
        &self,
        root: &Path,
        recovered: bool,
        operation_id: Option<CaudraId>,
    ) -> Result<Option<RestoreReport>, SnapshotError> {
        let Some(journal) = self.read_journal()? else {
            return Ok(None);
        };
        match (operation_id, journal.operation_id) {
            (Some(expected), actual) if actual != Some(expected) => {
                return Err(SnapshotError::RestoreOperationMismatch { expected, actual });
            }
            (None, Some(pending)) => return Err(SnapshotError::RestoreOperationPending(pending)),
            _ => {}
        }
        self.finish_pending(root, recovered)
    }

    fn advance_journal(&self, root: &Path, recovered: bool) -> Result<Step, SnapshotError> {
        let Some(mut journal) = self.read_journal()? else {
            return Ok(Step::Absent);
        };
        if journal.root != root {
            return Err(SnapshotError::JournalRootMismatch {
                expected: journal.root,
                actual: root.to_path_buf(),
            });
        }
        match journal.state {
            JournalState::Prepare => {
                self.apply_prepared(root, &journal)?;
                journal.state = JournalState::Applied;
            }
            JournalState::Applied => {
                self.verify_applied(root, &journal)?;
                match journal.destination {
                    RestoreTarget::Snapshot(_) => self.merge_unrevert(&journal)?,
                    RestoreTarget::Unrevert => remove_if_present(&self.unrevert_path())?,
                }
                journal.state = JournalState::Cleared;
            }
            JournalState::Cleared => {
                if journal.operation_id.is_none() {
                    remove_file_durable(&self.journal_path())?;
                }
                return Ok(Step::Finished(journal.report(recovered)));
            }
        }
        self.write_journal(&journal)?;
        Ok(Step::Advanced)
    }

    /// Writes every path that does not hold its target yet. A path holding
    /// neither its content from before nor its target was changed by someone
    /// else since the journal was prepared.
    fn apply_prepared(&self, root: &Path, journal: &RestoreJournal) -> Result<(), SnapshotError> {
        let repository = self.open_repository()?;
        let mut pending = Vec::new();
        let mut conflicts = Vec::new();
        for path in &journal.paths {
            let current = observe(&repository, root, &path.path, false)?;
            if current.matches(path.target) {
                continue;
            }
            if !current.matches(path.before) {
                conflicts.push(path_conflict(&path.path, path.before, current));
            }
            pending.push(path);
        }
        if !conflicts.is_empty() && journal.policy == ConflictPolicy::Abort {
            return Err(SnapshotError::Conflicts(conflicts));
        }
        apply(&repository, root, &pending)
    }

    fn verify_applied(&self, root: &Path, journal: &RestoreJournal) -> Result<(), SnapshotError> {
        let repository = self.open_repository()?;
        let mut pending = Vec::new();
        let mut conflicts = Vec::new();
        for path in &journal.paths {
            let current = observe(&repository, root, &path.path, false)?;
            if current.matches(path.target) {
                continue;
            }
            if journal.policy == ConflictPolicy::Overwrite {
                pending.push(path);
            } else {
                conflicts.push(path_conflict(&path.path, path.target, current));
            }
        }
        if !conflicts.is_empty() {
            return Err(SnapshotError::Conflicts(conflicts));
        }
        apply(&repository, root, &pending)
    }

    fn merge_unrevert(&self, journal: &RestoreJournal) -> Result<(), SnapshotError> {
        let mut record = self.read_unrevert()?.unwrap_or_default();
        for path in &journal.paths {
            record
                .paths
                .entry(path.path.clone())
                .and_modify(|recorded| recorded.after = path.target)
                .or_insert(UnrevertPath {
                    before: path.before,
                    after: path.target,
                });
        }
        write_json(&self.unrevert_path(), &record)
    }

    /// A store in the pre-git format has no journal or unrevert record this
    /// format can read, and is replaced on its next write.
    fn read_journal(&self) -> Result<Option<RestoreJournal>, SnapshotError> {
        if self.is_legacy() {
            return Ok(None);
        }
        read_optional_json(&self.journal_path(), JOURNAL_MISSING)
    }

    fn read_unrevert(&self) -> Result<Option<UnrevertRecord>, SnapshotError> {
        if self.is_legacy() {
            return Ok(None);
        }
        read_optional_json(&self.unrevert_path(), UNREVERT_MISSING)
    }

    fn write_journal(&self, journal: &RestoreJournal) -> Result<(), SnapshotError> {
        write_json(&self.journal_path(), journal)
    }
}

/// Objects a session's restore journal and unrevert record name. No snapshot
/// may reach them any more, and a restore still needs them.
pub(super) fn restore_objects(session_dir: &Path) -> Result<Vec<ObjectId>, SnapshotError> {
    if is_legacy(session_dir) {
        return Ok(Vec::new());
    }
    let journal: Option<RestoreJournal> =
        read_optional_json(&session_dir.join(JOURNAL_NAME), JOURNAL_MISSING)?;
    let unrevert: Option<UnrevertRecord> =
        read_optional_json(&session_dir.join(UNREVERT_NAME), UNREVERT_MISSING)?;
    let journaled = journal
        .iter()
        .flat_map(|journal| &journal.paths)
        .flat_map(|path| [path.before, path.target]);
    let unreverted = unrevert
        .iter()
        .flat_map(|record| record.paths.values())
        .flat_map(|path| [path.before, path.after]);
    Ok(journaled
        .chain(unreverted)
        .flatten()
        .map(|content| content.oid)
        .collect())
}

/// Verifies every object the pending paths need before writing any of them,
/// so a missing or corrupt object fails the restore before it starts.
fn apply(
    repository: &ObjectStore,
    root: &Path,
    pending: &[&JournalPath],
) -> Result<(), SnapshotError> {
    let mut verified = HashSet::new();
    for content in pending.iter().filter_map(|path| path.target) {
        if verified.insert(content.oid) {
            repository.read_blob(&content.oid)?;
        }
    }
    pending
        .iter()
        .try_for_each(|path| apply_path(repository, root, &path.path, path.target))
}

fn apply_path(
    repository: &ObjectStore,
    root: &Path,
    relative: &str,
    target: Option<Content>,
) -> Result<(), SnapshotError> {
    let path = checked_destination(root, relative)?;
    let Some(content) = target else {
        return match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
                remove_file_durable(&path)
            }
            Ok(_) => Err(SnapshotError::UnsupportedFileType(relative.to_owned())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        };
    };
    let bytes = repository.read_blob(&content.oid)?;
    let parent = path
        .parent()
        .ok_or_else(|| SnapshotError::InvalidPath(relative.to_owned()))?;
    fs::create_dir_all(parent)?;
    match content.kind {
        EntryKind::Symlink => write_symlink(parent, &path, &bytes)?,
        EntryKind::File | EntryKind::Executable => {
            let mut staged = staged_file(parent, &path, content.kind == EntryKind::Executable)?;
            staged.write_all(&bytes)?;
            staged.as_file().sync_all()?;
            staged.persist(&path).map_err(|error| error.error)?;
        }
    }
    caudra_storage::sync_dir(parent);
    Ok(())
}

/// An existing file keeps its permission bits, with the execute bits made to
/// match. A new one gets what git checkout gives it: the open mode, filtered
/// through the umask.
#[cfg(unix)]
fn staged_file(parent: &Path, path: &Path, executable: bool) -> io::Result<NamedTempFile> {
    use std::os::unix::fs::PermissionsExt;

    let open_mode = if executable {
        NEW_EXECUTABLE_MODE
    } else {
        NEW_FILE_MODE
    };
    let staged = Builder::new()
        .prefix(STAGING_PREFIX)
        .permissions(fs::Permissions::from_mode(open_mode))
        .tempfile_in(parent)?;
    if let Some(current) = fs::symlink_metadata(path)
        .ok()
        .filter(fs::Metadata::is_file)
    {
        let mode = execute_bits_matching(current.permissions().mode(), executable);
        fs::set_permissions(staged.path(), fs::Permissions::from_mode(mode))?;
    }
    Ok(staged)
}

#[cfg(not(unix))]
fn staged_file(parent: &Path, _path: &Path, _executable: bool) -> io::Result<NamedTempFile> {
    Builder::new().prefix(STAGING_PREFIX).tempfile_in(parent)
}

/// Execute for everyone who may read, as `chmod +x` would under no umask, or
/// for no one.
#[cfg(unix)]
fn execute_bits_matching(mode: u32, executable: bool) -> u32 {
    let mode = mode & PERMISSION_BITS;
    if executable {
        mode | OWNER_EXECUTE | ((mode & READ_BITS) >> READ_TO_EXECUTE_SHIFT)
    } else {
        mode & !EXECUTE_BITS
    }
}

#[cfg(unix)]
fn write_symlink(parent: &Path, path: &Path, target: &[u8]) -> Result<(), SnapshotError> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;

    let target = OsStr::from_bytes(target);
    Builder::new()
        .prefix(STAGING_PREFIX)
        .make_in(parent, |staged| symlink(target, staged))?
        .persist(path)
        .map_err(|error| error.error.into())
}

#[cfg(not(unix))]
fn write_symlink(_parent: &Path, path: &Path, _target: &[u8]) -> Result<(), SnapshotError> {
    Err(SnapshotError::UnsupportedFileType(
        path.display().to_string(),
    ))
}

/// Each change as the journal records it, with what its path holds now staged
/// in `repository` for an unrevert to put back. Refuses conflicts under
/// `Abort`, and any path that is neither a file nor a symlink.
fn journal_paths(
    repository: &ObjectStore,
    root: &Path,
    changes: Vec<Change>,
    policy: ConflictPolicy,
) -> Result<Vec<JournalPath>, SnapshotError> {
    let mut paths = Vec::with_capacity(changes.len());
    let mut conflicts = Vec::new();
    let mut unsupported = None;
    for change in changes {
        let current = observe(repository, root, &change.path, true)?;
        if !current.matches(change.source) {
            conflicts.push(path_conflict(&change.path, change.source, current));
        }
        if current == Observed::Other {
            unsupported.get_or_insert_with(|| change.path.clone());
        }
        paths.push(JournalPath {
            outcome: outcome_kind(current, change.target),
            before: current.content(),
            target: change.target,
            path: change.path,
        });
    }
    if !conflicts.is_empty() && policy == ConflictPolicy::Abort {
        return Err(SnapshotError::Conflicts(conflicts));
    }
    if let Some(path) = unsupported {
        return Err(SnapshotError::UnsupportedFileType(path));
    }
    Ok(paths)
}

/// What `relative` holds now, as a snapshot would record it. With `store`,
/// its content is also staged in the store for an unrevert to put back.
fn observe(
    repository: &ObjectStore,
    root: &Path,
    relative: &str,
    store: bool,
) -> Result<Observed, SnapshotError> {
    let path = checked_destination(root, relative)?;
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Observed::Absent),
        Err(error) => return Err(error.into()),
    };
    let (kind, bytes) = if metadata.is_file() {
        let kind = if is_executable(&metadata) {
            EntryKind::Executable
        } else {
            EntryKind::File
        };
        (kind, fs::read(&path)?)
    } else if cfg!(unix) && metadata.file_type().is_symlink() {
        (EntryKind::Symlink, link_target(&path)?)
    } else {
        return Ok(Observed::Other);
    };
    let oid = if store {
        repository.write_blob(&bytes)?.oid
    } else {
        blob_id(&bytes)?
    };
    Ok(Observed::Present(Content { kind, oid }))
}

/// `relative` under `root`, refusing any ancestor that is not a real
/// directory: a symlinked ancestor would take the write outside the root.
fn checked_destination(root: &Path, relative: &str) -> Result<PathBuf, SnapshotError> {
    let components: Vec<_> = Path::new(relative).components().collect();
    if components.is_empty() {
        return Err(SnapshotError::InvalidPath(relative.to_owned()));
    }
    let mut current = root.to_path_buf();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            return Err(SnapshotError::InvalidPath(relative.to_owned()));
        };
        current.push(name);
        if index + 1 == components.len() {
            break;
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Err(SnapshotError::UnsupportedFileType(relative.to_owned())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(current)
}

fn validate_report(
    report: &RestoreReport,
    operation_id: Option<CaudraId>,
    destination: RestoreTarget,
) -> Result<(), SnapshotError> {
    if let Some(expected) = operation_id
        && report.operation_id != Some(expected)
    {
        return Err(SnapshotError::RestoreOperationMismatch {
            expected,
            actual: report.operation_id,
        });
    }
    if report.operation_id != operation_id || report.target != destination {
        return Err(io::Error::other(JOURNAL_CHANGED).into());
    }
    Ok(())
}

fn outcome_kind(current: Observed, target: Option<Content>) -> PathOutcomeKind {
    if current.matches(target) {
        return PathOutcomeKind::Unchanged;
    }
    match (current.content(), target) {
        (None, Some(_)) => PathOutcomeKind::Created,
        (Some(_), None) => PathOutcomeKind::Deleted,
        (Some(current), Some(target))
            if current.oid == target.oid
                && ![current.kind, target.kind].contains(&EntryKind::Symlink) =>
        {
            PathOutcomeKind::MetadataChanged
        }
        (Some(_), Some(_)) => PathOutcomeKind::Modified,
        (None, None) => PathOutcomeKind::Unchanged,
    }
}

fn path_conflict(path: &str, expected: Option<Content>, actual: Observed) -> PathConflict {
    PathConflict {
        path: path.to_owned(),
        expected_hash: expected.map(|content| content.oid.to_string()),
        actual_hash: actual.content().map(|content| content.oid.to_string()),
        actual_exists: actual != Observed::Absent,
    }
}

/// Content as `git ls-tree` prints it, `<mode> <object id>`, so a journal
/// reads the same as the store it points into.
mod stored_content {
    use serde::de::Error;
    use serde::{Deserialize, Deserializer, Serializer};
    use workcell::snapshot_store::{Content, EntryKind, parse_oid};

    const FILE_MODE: &str = "100644";
    const EXECUTABLE_MODE: &str = "100755";
    const SYMLINK_MODE: &str = "120000";

    pub(super) fn serialize<S: Serializer>(
        content: &Option<Content>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match content {
            Some(content) => {
                let mode = match content.kind {
                    EntryKind::File => FILE_MODE,
                    EntryKind::Executable => EXECUTABLE_MODE,
                    EntryKind::Symlink => SYMLINK_MODE,
                };
                serializer.collect_str(&format_args!("{mode} {}", content.oid))
            }
            None => serializer.serialize_none(),
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Content>, D::Error> {
        let Some(text) = Option::<String>::deserialize(deserializer)? else {
            return Ok(None);
        };
        let (mode, oid) = text
            .split_once(' ')
            .ok_or_else(|| D::Error::custom(format!("invalid snapshot content {text:?}")))?;
        let kind = match mode {
            FILE_MODE => EntryKind::File,
            EXECUTABLE_MODE => EntryKind::Executable,
            SYMLINK_MODE => EntryKind::Symlink,
            _ => return Err(D::Error::custom(format!("invalid snapshot mode {mode:?}"))),
        };
        let oid = parse_oid(oid)
            .ok_or_else(|| D::Error::custom(format!("invalid snapshot object id {oid:?}")))?;
        Ok(Some(Content { kind, oid }))
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;
    use workcell::snapshot_store::StoreError;

    use super::*;
    use crate::snapshots::tests::{
        ALPHA, BETA, content, id, object_path, read, setup, store_in, write,
    };
    use crate::snapshots::{SnapshotKey, SnapshotLimits};

    const PREFLIGHT_MSG: &str = "a damaged object must fail the restore before any path is written";
    const MODE_MSG: &str =
        "a restored file keeps its permission bits, with execute matching the snapshot";
    const UMASK_MSG: &str =
        "a file a restore creates gets the open mode filtered through the umask, as git checkout";
    const SYMLINK_MSG: &str = "a restore puts a symlink back as a link to the same target";
    const OVERSIZED_MSG: &str = "a file too large to capture is left alone by every restore";
    const LINK_TARGET: &str = "missing-target";
    const MAX_FILE_BYTES: u64 = 32;
    const OVERSIZED_BYTES: usize = 64;
    const SMALL_BYTES: usize = 16;

    fn outcome(report: &RestoreReport, path: &str) -> PathOutcomeKind {
        report
            .paths
            .iter()
            .find(|outcome| outcome.path == path)
            .unwrap()
            .kind
    }

    /// Journals a restore from `source` back to the session start, applying
    /// none of it, and answers the root the store is bound to.
    fn prepare_to_start(
        store: &SnapshotStore,
        root: &Path,
        source: CaudraId,
        operation_id: Option<CaudraId>,
    ) -> PathBuf {
        let root = store.bind_root(root).unwrap();
        let source = store.snapshot_id(SnapshotKey::Checkpoint(source)).unwrap();
        let target = store.snapshot_id(SnapshotKey::SessionStart).unwrap();
        let changes = store
            .open_repository()
            .unwrap()
            .changes(&source, &target)
            .unwrap()
            .changes;
        store
            .prepare_restore(
                &root,
                changes,
                RestoreTarget::Snapshot(SnapshotKey::SessionStart),
                ConflictPolicy::Abort,
                operation_id,
            )
            .unwrap();
        root
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;

        fs::metadata(path).unwrap().permissions().mode() & PERMISSION_BITS
    }

    #[test]
    fn restore_modifications_creations_deletions_and_unrevert() {
        let (temp, root) = setup();
        write(&root, "modified.txt", ALPHA);
        write(&root, "deleted.txt", BETA);
        let store = store_in(&temp);
        store.snapshot_session_start(&root).unwrap();

        write(&root, "modified.txt", "changed");
        fs::remove_file(root.join("deleted.txt")).unwrap();
        write(&root, "created.txt", "new");
        let source = id(1);
        store.snapshot(&root, source).unwrap();

        let report = store.restore(&root, &[source], &[]).unwrap();
        assert_eq!(read(&root, "modified.txt"), ALPHA);
        assert_eq!(read(&root, "deleted.txt"), BETA);
        assert!(!root.join("created.txt").exists());
        assert_eq!(outcome(&report, "modified.txt"), PathOutcomeKind::Modified);
        assert_eq!(outcome(&report, "deleted.txt"), PathOutcomeKind::Created);
        assert_eq!(outcome(&report, "created.txt"), PathOutcomeKind::Deleted);

        store.unrevert(&root).unwrap();
        assert_eq!(read(&root, "modified.txt"), "changed");
        assert!(!root.join("deleted.txt").exists());
        assert_eq!(read(&root, "created.txt"), "new");
    }

    #[test]
    fn chained_restores_keep_original_and_later_path_baselines() {
        let (temp, root) = setup();
        write(&root, "a.txt", "first-target");
        let store = store_in(&temp);
        store.snapshot_session_start(&root).unwrap();

        write(&root, "a.txt", "second-target");
        write(&root, "b.txt", "second-target-b");
        let second_target = id(1);
        store.snapshot(&root, second_target).unwrap();

        write(&root, "a.txt", "original-before-first-revert");
        fs::remove_file(root.join("b.txt")).unwrap();
        let first_source = id(2);
        store.snapshot(&root, first_source).unwrap();
        store.restore(&root, &[first_source], &[]).unwrap();

        write(&root, "b.txt", "current-before-second-revert");
        let second_source = id(3);
        store.snapshot(&root, second_source).unwrap();
        store
            .restore(&root, &[second_source], &[second_target])
            .unwrap();
        assert_eq!(read(&root, "a.txt"), "second-target");
        assert_eq!(read(&root, "b.txt"), "second-target-b");

        store.unrevert(&root).unwrap();
        assert_eq!(read(&root, "a.txt"), "original-before-first-revert");
        assert_eq!(read(&root, "b.txt"), "current-before-second-revert");
    }

    #[test]
    fn chained_unrevert_leaves_unchanged_paths_edited_between_restores() {
        let (temp, root) = setup();
        write(&root, "a.txt", "a0");
        write(&root, "b.txt", "b0");
        write(&root, "c.txt", "c0");
        let store = store_in(&temp);
        store.snapshot_session_start(&root).unwrap();

        write(&root, "c.txt", "c1");
        let second_source = id(1);
        store.snapshot(&root, second_source).unwrap();

        write(&root, "c.txt", "c0");
        write(&root, "a.txt", "a1");
        let first_source = id(2);
        store.snapshot(&root, first_source).unwrap();
        store.restore(&root, &[first_source], &[]).unwrap();

        write(&root, "b.txt", "external");
        write(&root, "c.txt", "c1");
        let report = store.restore(&root, &[second_source], &[]).unwrap();
        assert_eq!(report.paths.len(), 1);
        assert_eq!(report.paths[0].path, "c.txt");

        store.unrevert(&root).unwrap();
        assert_eq!(read(&root, "a.txt"), "a1");
        assert_eq!(read(&root, "b.txt"), "external");
        assert_eq!(read(&root, "c.txt"), "c1");
    }

    #[test]
    fn binary_contents_round_trip_exactly() {
        let (temp, root) = setup();
        let original = [0, 1, 2, 0xff, 0, 0x80];
        write(&root, "binary.dat", original);
        let store = store_in(&temp);
        store.snapshot_session_start(&root).unwrap();
        write(&root, "binary.dat", [9, 0, 8, 0, 7]);
        let source = id(1);
        store.snapshot(&root, source).unwrap();

        store.restore(&root, &[source], &[]).unwrap();
        assert_eq!(fs::read(root.join("binary.dat")).unwrap(), original);
    }

    /// Paths are written in order, so a damaged object for the last of them
    /// shows whether every object was verified before the first write.
    #[test_case(false ; "missing")]
    #[test_case(true  ; "corrupt")]
    fn a_damaged_object_fails_the_restore_before_any_path_is_written(corrupt: bool) {
        let (temp, root) = setup();
        write(&root, "a.txt", "a0");
        write(&root, "b.txt", "b0");
        let store = store_in(&temp);
        store.snapshot_session_start(&root).unwrap();
        write(&root, "a.txt", "a1");
        write(&root, "b.txt", "b1");
        let source = id(1);
        store.snapshot(&root, source).unwrap();
        let damaged = blob_id(b"b0").unwrap();
        let path = object_path(&store, &damaged);
        let intact = fs::read(&path).unwrap();
        fs::remove_file(&path).unwrap();
        if corrupt {
            fs::write(&path, ALPHA).unwrap();
        }

        let error = store.restore(&root, &[source], &[]).unwrap_err();
        assert!(
            matches!(
                error,
                SnapshotError::Store(StoreError::Missing(oid) | StoreError::Corrupt(oid))
                    if oid == damaged
            ),
            "{PREFLIGHT_MSG}: {error:?}"
        );
        assert_eq!(read(&root, "a.txt"), "a1", "{PREFLIGHT_MSG}");
        assert_eq!(read(&root, "b.txt"), "b1", "{PREFLIGHT_MSG}");
        assert_eq!(
            store.journal_state().unwrap(),
            Some(JournalState::Prepare),
            "{PREFLIGHT_MSG}"
        );

        let _ = fs::remove_file(&path);
        fs::write(&path, intact).unwrap();
        store.recover(&root).unwrap().unwrap();
        assert_eq!(read(&root, "a.txt"), "a0");
        assert_eq!(read(&root, "b.txt"), "b0");
        assert_eq!(store.journal_state().unwrap(), None);
    }

    #[cfg(unix)]
    #[test_case(0o755, 0o644, 0o755 ; "made_executable_for_everyone_who_may_read")]
    #[test_case(0o755, 0o600, 0o700 ; "made_executable_for_the_owner_alone")]
    #[test_case(0o755, 0o640, 0o750 ; "made_executable_for_owner_and_group")]
    #[test_case(0o644, 0o750, 0o640 ; "made_executable_for_nobody")]
    fn restoring_the_execute_bit_keeps_the_other_permission_bits(
        snapshot_mode: u32,
        current_mode: u32,
        restored_mode: u32,
    ) {
        let (temp, root) = setup();
        let path = root.join("run.sh");
        write(&root, "run.sh", ALPHA);
        set_mode(&path, snapshot_mode);
        let store = store_in(&temp);
        store.snapshot_session_start(&root).unwrap();
        set_mode(&path, current_mode);
        let source = id(1);
        store.snapshot(&root, source).unwrap();

        let report = store.restore(&root, &[source], &[]).unwrap();

        assert_eq!(mode(&path), restored_mode, "{MODE_MSG}");
        assert_eq!(
            outcome(&report, "run.sh"),
            PathOutcomeKind::MetadataChanged,
            "{MODE_MSG}"
        );
    }

    #[cfg(unix)]
    #[test_case(0o644, NEW_FILE_MODE       ; "file")]
    #[test_case(0o755, NEW_EXECUTABLE_MODE ; "executable")]
    fn a_created_file_gets_the_mode_git_checkout_gives_it(snapshot_mode: u32, open_mode: u32) {
        use std::os::unix::fs::OpenOptionsExt;

        let (temp, root) = setup();
        let path = root.join("tool");
        write(&root, "tool", ALPHA);
        set_mode(&path, snapshot_mode);
        let store = store_in(&temp);
        store.snapshot_session_start(&root).unwrap();
        fs::remove_file(&path).unwrap();
        let source = id(1);
        store.snapshot(&root, source).unwrap();

        store.restore(&root, &[source], &[]).unwrap();

        let probe = temp.path().join("probe");
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(open_mode)
            .open(&probe)
            .unwrap();
        assert_eq!(mode(&path), mode(&probe), "{UMASK_MSG}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_restored_as_a_link() {
        use std::os::unix::fs::symlink;

        let (temp, root) = setup();
        let path = root.join("link");
        symlink(LINK_TARGET, &path).unwrap();
        let store = store_in(&temp);
        store.snapshot_session_start(&root).unwrap();
        fs::remove_file(&path).unwrap();
        write(&root, "link", ALPHA);
        let source = id(1);
        store.snapshot(&root, source).unwrap();

        let report = store.restore(&root, &[source], &[]).unwrap();

        assert!(
            fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "{SYMLINK_MSG}"
        );
        assert_eq!(
            fs::read_link(&path).unwrap(),
            Path::new(LINK_TARGET),
            "{SYMLINK_MSG}"
        );
        assert_eq!(
            outcome(&report, "link"),
            PathOutcomeKind::Modified,
            "{SYMLINK_MSG}"
        );
        store.unrevert(&root).unwrap();
        assert_eq!(read(&root, "link"), ALPHA, "{SYMLINK_MSG}");
    }

    /// Wherever the file was too large, the snapshot never saw it, so neither
    /// its deletion nor an older version of it is the restore's to apply.
    #[test_case(OVERSIZED_BYTES, OVERSIZED_BYTES ; "oversized_in_both")]
    #[test_case(OVERSIZED_BYTES, SMALL_BYTES     ; "oversized_in_the_target")]
    #[test_case(SMALL_BYTES, OVERSIZED_BYTES     ; "oversized_in_the_source")]
    fn an_oversized_file_is_left_untouched_by_a_restore(target_bytes: usize, source_bytes: usize) {
        let (temp, root) = setup();
        write(&root, "small.txt", ALPHA);
        write(&root, "big.bin", vec![b'x'; target_bytes]);
        let store = store_in(&temp).with_limits(SnapshotLimits {
            max_file_bytes: MAX_FILE_BYTES,
            ..SnapshotLimits::default()
        });
        store.snapshot_session_start(&root).unwrap();
        write(&root, "small.txt", BETA);
        let current = vec![b'y'; source_bytes];
        write(&root, "big.bin", &current);
        let source = id(1);
        store.snapshot(&root, source).unwrap();

        store.restore(&root, &[source], &[]).unwrap();

        assert_eq!(read(&root, "small.txt"), ALPHA, "{OVERSIZED_MSG}");
        assert_eq!(
            fs::read(root.join("big.bin")).ok(),
            Some(current),
            "{OVERSIZED_MSG}"
        );
    }

    #[test]
    fn conflict_aborts_without_writing_and_overwrite_is_explicit() {
        let (temp, root) = setup();
        write(&root, "file.txt", ALPHA);
        let store = store_in(&temp);
        store.snapshot_session_start(&root).unwrap();
        write(&root, "file.txt", BETA);
        let source = id(1);
        store.snapshot(&root, source).unwrap();
        write(&root, "file.txt", "third-party");

        let error = store.restore(&root, &[source], &[]).unwrap_err();
        let SnapshotError::Conflicts(conflicts) = error else {
            panic!("expected conflicts, got {error:?}");
        };
        assert_eq!(
            conflicts,
            [PathConflict {
                path: "file.txt".to_owned(),
                expected_hash: Some(blob_id(BETA.as_bytes()).unwrap().to_string()),
                actual_hash: Some(blob_id(b"third-party").unwrap().to_string()),
                actual_exists: true,
            }]
        );
        assert_eq!(read(&root, "file.txt"), "third-party");
        assert_eq!(store.journal_state().unwrap(), None);

        store
            .restore_impl(&root, &[source], &[], ConflictPolicy::Overwrite, None)
            .unwrap();
        assert_eq!(read(&root, "file.txt"), ALPHA);
        store.unrevert(&root).unwrap();
        assert_eq!(read(&root, "file.txt"), "third-party");
    }

    #[test]
    fn restore_leaves_paths_outside_source_target_union_untouched() {
        let (temp, root) = setup();
        write(&root, "tracked.txt", ALPHA);
        let store = store_in(&temp);
        store.snapshot_session_start(&root).unwrap();
        write(&root, "tracked.txt", BETA);
        let source = id(1);
        store.snapshot(&root, source).unwrap();
        write(&root, "unrelated.txt", "leave me");

        store.restore(&root, &[source], &[]).unwrap();
        assert_eq!(read(&root, "tracked.txt"), ALPHA);
        assert_eq!(read(&root, "unrelated.txt"), "leave me");
    }

    #[test]
    fn coordinated_restore_keeps_its_journal_until_acknowledged() {
        let (temp, root) = setup();
        write(&root, "file.txt", ALPHA);
        let store = store_in(&temp);
        store.snapshot_session_start(&root).unwrap();
        write(&root, "file.txt", BETA);
        let source = id(1);
        store.snapshot(&root, source).unwrap();
        let operation_id = id(2);

        let report = store
            .restore_transaction_with_policy(
                &root,
                &[source],
                &[],
                ConflictPolicy::Abort,
                operation_id,
            )
            .unwrap();

        assert_eq!(report.operation_id, Some(operation_id));
        assert_eq!(store.journal_state().unwrap(), Some(JournalState::Cleared));
        assert_eq!(
            store.recover(&root).unwrap().unwrap().operation_id,
            Some(operation_id)
        );
        let wrong_operation = id(3);
        assert!(matches!(
            store.acknowledge_operation(&root, wrong_operation),
            Err(SnapshotError::RestoreOperationMismatch {
                expected,
                actual: Some(actual),
            }) if expected == wrong_operation && actual == operation_id
        ));
        store.acknowledge_operation(&root, operation_id).unwrap();
        assert_eq!(store.journal_state().unwrap(), None);
        assert_eq!(read(&root, "file.txt"), ALPHA);
    }

    #[test]
    fn another_operation_does_not_apply_a_prepared_journal() {
        let (temp, root) = setup();
        write(&root, "file.txt", ALPHA);
        let store = store_in(&temp);
        store.snapshot_session_start(&root).unwrap();
        write(&root, "file.txt", BETA);
        let source = id(1);
        store.snapshot(&root, source).unwrap();
        let prepared_operation = id(2);
        prepare_to_start(&store, &root, source, Some(prepared_operation));

        let other = store_in(&temp);
        let requested_operation = id(3);
        assert!(matches!(
            other.restore_transaction_with_policy(
                &root,
                &[source],
                &[],
                ConflictPolicy::Abort,
                requested_operation,
            ),
            Err(SnapshotError::RestoreOperationMismatch {
                expected,
                actual: Some(actual),
            }) if expected == requested_operation && actual == prepared_operation
        ));
        assert_eq!(read(&root, "file.txt"), BETA);
        assert_eq!(other.journal_state().unwrap(), Some(JournalState::Prepare));
    }

    #[test]
    fn report_validation_rejects_another_operation() {
        let expected = id(1);
        let actual = id(2);
        let report = RestoreReport {
            target: RestoreTarget::Unrevert,
            paths: Vec::new(),
            recovered: false,
            operation_id: Some(actual),
        };

        assert!(matches!(
            validate_report(&report, Some(expected), RestoreTarget::Unrevert),
            Err(SnapshotError::RestoreOperationMismatch {
                expected: error_expected,
                actual: Some(error_actual),
            }) if error_expected == expected && error_actual == actual
        ));
    }

    #[test]
    fn prepared_applied_and_cleared_journal_states_recover() {
        let (temp, root) = setup();
        write(&root, "a.txt", "a0");
        write(&root, "b.txt", "b0");
        let store = store_in(&temp);
        store.snapshot_session_start(&root).unwrap();
        write(&root, "a.txt", "a1");
        write(&root, "b.txt", "b1");
        let source = id(1);
        store.snapshot(&root, source).unwrap();
        let canonical = prepare_to_start(&store, &root, source, None);
        assert_eq!(store.journal_state().unwrap(), Some(JournalState::Prepare));

        apply_path(
            &store.open_repository().unwrap(),
            &canonical,
            "a.txt",
            content(&store, SnapshotKey::SessionStart, "a.txt"),
        )
        .unwrap();
        drop(store);
        let reopened = store_in(&temp);
        reopened.advance_journal(&canonical, true).unwrap();
        assert_eq!(
            reopened.journal_state().unwrap(),
            Some(JournalState::Applied)
        );
        drop(reopened);

        let reopened = store_in(&temp);
        reopened.advance_journal(&canonical, true).unwrap();
        assert_eq!(
            reopened.journal_state().unwrap(),
            Some(JournalState::Cleared)
        );
        drop(reopened);

        let reopened = store_in(&temp);
        let report = reopened.recover(&root).unwrap().unwrap();
        assert!(report.recovered);
        assert_eq!(reopened.journal_state().unwrap(), None);
        assert_eq!(read(&root, "a.txt"), "a0");
        assert_eq!(read(&root, "b.txt"), "b0");
        reopened.unrevert(&root).unwrap();
        assert_eq!(read(&root, "a.txt"), "a1");
        assert_eq!(read(&root, "b.txt"), "b1");
    }

    #[test]
    fn prepared_deletion_completes_before_the_journal_advances() {
        let (temp, root) = setup();
        let store = store_in(&temp);
        store.snapshot_session_start(&root).unwrap();
        write(&root, "file.txt", ALPHA);
        let source = id(1);
        store.snapshot(&root, source).unwrap();
        let canonical = prepare_to_start(&store, &root, source, None);

        store.advance_journal(&canonical, false).unwrap();
        assert!(!root.join("file.txt").exists());
        assert_eq!(store.journal_state().unwrap(), Some(JournalState::Applied));
    }

    #[test]
    fn unrevert_recovery_after_mutation_is_consumed_once() {
        let (temp, root) = setup();
        write(&root, "file.txt", ALPHA);
        let store = store_in(&temp);
        store.snapshot_session_start(&root).unwrap();
        write(&root, "file.txt", BETA);
        let source = id(1);
        store.snapshot(&root, source).unwrap();
        store.restore(&root, &[source], &[]).unwrap();

        let canonical = store.bind_root(&root).unwrap();
        let changes = store
            .read_unrevert()
            .unwrap()
            .unwrap()
            .paths
            .into_iter()
            .map(|(path, contents)| Change {
                path,
                source: contents.after,
                target: contents.before,
            })
            .collect();
        store
            .prepare_restore(
                &canonical,
                changes,
                RestoreTarget::Unrevert,
                ConflictPolicy::Abort,
                None,
            )
            .unwrap();
        let journal = store.read_journal().unwrap().unwrap();
        store.apply_prepared(&canonical, &journal).unwrap();
        assert_eq!(read(&root, "file.txt"), BETA);
        assert_eq!(store.journal_state().unwrap(), Some(JournalState::Prepare));
        drop(store);

        let reopened = store_in(&temp);
        let report = reopened.unrevert(&root).unwrap();
        assert!(report.recovered);
        assert_eq!(report.target, RestoreTarget::Unrevert);
        assert_eq!(read(&root, "file.txt"), BETA);
        assert_eq!(reopened.journal_state().unwrap(), None);
        assert!(!reopened.unrevert_path().exists());

        assert!(matches!(
            reopened.unrevert(&root),
            Err(SnapshotError::NotFound(name)) if name == UNREVERT_MISSING
        ));
        assert_eq!(read(&root, "file.txt"), BETA);
    }

    #[test]
    fn recovery_rejects_changes_after_prepare() {
        let (temp, root) = setup();
        write(&root, "file.txt", ALPHA);
        let store = store_in(&temp);
        store.snapshot_session_start(&root).unwrap();
        write(&root, "file.txt", BETA);
        let source = id(1);
        store.snapshot(&root, source).unwrap();
        prepare_to_start(&store, &root, source, None);
        write(&root, "file.txt", "external");
        drop(store);

        let reopened = store_in(&temp);
        assert!(matches!(
            reopened.recover(&root),
            Err(SnapshotError::Conflicts(_))
        ));
        assert_eq!(read(&root, "file.txt"), "external");
        assert_eq!(
            reopened.journal_state().unwrap(),
            Some(JournalState::Prepare)
        );
    }

    #[test]
    fn discarding_the_unrevert_record_tolerates_its_absence() {
        let (temp, root) = setup();
        write(&root, "file.txt", ALPHA);
        let store = store_in(&temp);
        let source = id(1);
        store.snapshot_session_start(&root).unwrap();
        write(&root, "file.txt", BETA);
        store.snapshot(&root, source).unwrap();
        store.restore(&root, &[source], &[]).unwrap();
        assert!(store.unrevert_path().exists());

        store.discard_unrevert().unwrap();
        assert!(!store.unrevert_path().exists());
        store.discard_unrevert().unwrap();
    }
}
