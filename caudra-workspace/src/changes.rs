//! Per-call change records: what each tool call changed in the session
//! directory, kept so that a file revert undoes exactly that.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    CancellationResult, OperationHandle, OperationStatus, RecordHolder, RecordTicket,
    ReleaseResult, RevertId, SessionWorkspaceBinding, WorkspaceCursor, WorkspaceError,
    WorkspacePath,
};

const PATH_SEPARATOR: &str = "/";
/// Where a record places a remote session directory, whose real location the
/// host never reveals. No path the kernel accepts holds a NUL byte, so every
/// absolute path a call can act on lies outside it.
pub const UNREVEALED_ROOT: &str = "/\0";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecordScope {
    /// These paths and everything beneath them.
    Paths(BTreeSet<WorkspacePath>),
    /// The whole session directory, compared before and after the call.
    Workspace,
}

impl RecordScope {
    /// The record a call writing `files` needs, or `None` when every one of
    /// them lies outside the session directory at `root`. A call that names
    /// no file may change any.
    pub fn of_files(files: &[PathBuf], root: &Path) -> Option<Self> {
        if files.is_empty() {
            return Some(Self::Workspace);
        }
        let mut paths = BTreeSet::new();
        for file in files {
            match RecordedPath::of(file, root) {
                RecordedPath::Inside(path) => {
                    paths.insert(path);
                }
                RecordedPath::Outside => {}
                RecordedPath::Unplaced => return Some(Self::Workspace),
            }
        }
        (!paths.is_empty()).then_some(Self::Paths(paths))
    }
}

/// Where a local path lands in a record of the session directory.
#[derive(Debug, PartialEq, Eq)]
pub enum RecordedPath {
    Inside(WorkspacePath),
    Outside,
    /// Only a record of the whole directory is sure to cover it: the directory
    /// itself, a path through `..`, which the kernel resolves through symlinks
    /// rather than by its text, or a name no workspace path can spell.
    Unplaced,
}

impl RecordedPath {
    pub fn of(path: &Path, root: &Path) -> Self {
        if path
            .components()
            .any(|component| component == Component::ParentDir)
        {
            return Self::Unplaced;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            return Self::Outside;
        };
        relative
            .components()
            .map(|component| match component {
                Component::Normal(name) => name.to_str(),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()
            .and_then(|names| WorkspacePath::new(names.join(PATH_SEPARATOR)).ok())
            .map_or(Self::Unplaced, Self::Inside)
    }
}

/// Ceilings for one record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordLimits {
    /// Files a whole-directory record may walk.
    pub max_files: u32,
    /// A larger file is left unrecorded, in either scope.
    pub max_file_bytes: u64,
    /// Bytes a whole-directory record may walk, and the size retention trims
    /// the store to.
    pub max_total_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordRequest {
    pub scope: RecordScope,
    pub holder: RecordHolder,
    /// Stored with the record and returned as given, never interpreted.
    pub client: Value,
    pub limits: RecordLimits,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordSummary {
    pub seq: u64,
    pub paths: u32,
    pub unrecorded: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecordState {
    Applied,
    /// Named by a revert that awaits acknowledgement, whoever made it.
    Reverted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordListing {
    pub seq: u64,
    pub client: Value,
    pub state: RecordState,
    pub paths: u32,
    pub unrecorded: u32,
}

/// One holder's records in seq order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordPage {
    pub records: Vec<RecordListing>,
    /// Where the next page starts, absent on the last one.
    pub next_after_seq: Option<u64>,
    /// The client metadata of the newest of the holder's records retention
    /// evicted.
    pub evicted_through: Option<Value>,
}

/// A record begun and never finished.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenRecord {
    pub ticket: RecordTicket,
    pub client: Value,
    pub opened_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HolderSummary {
    pub holder: RecordHolder,
    pub records: u32,
    pub open_records: u32,
    pub pending_reverts: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HolderPage {
    pub holders: Vec<HolderSummary>,
    /// Where the next page starts, absent on the last one.
    pub next_after: Option<RecordHolder>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReleaseSelection {
    All,
    Seqs(Vec<u64>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseSummary {
    /// Records the holder held and no longer does.
    pub released: u32,
    /// Of those, the records nobody holds any more, which are gone.
    pub deleted: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RevertDirection {
    Revert,
    Unrevert,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RevertChangeKind {
    Create,
    Replace,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevertPath {
    pub path: WorkspacePath,
    pub kind: RevertChangeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RevertConflictKind {
    /// The live entry matches neither side: something changed it since.
    ChangedSince,
    /// The selected records do not chain on this path: something changed it
    /// between them.
    Interleaved,
    /// A selected record could not store this path.
    Unrecorded,
}

/// Why a record holds a path it could not store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnrecordedReason {
    Oversized,
    /// Every read overlapped a change to it.
    Unstable,
    Unreadable,
    /// Behind an ancestor that is a link or not a plain directory.
    Blocked,
    /// A special file or mount point.
    Special,
    /// Another record changed it while this call ran, in a way the two cannot
    /// be told apart.
    Interleaved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevertConflict {
    pub path: WorkspacePath,
    pub kind: RevertConflictKind,
    pub reason: Option<UnrecordedReason>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevertCounts {
    pub create: u32,
    pub replace: u32,
    pub delete: u32,
    /// Changed paths that already hold what the revert would write.
    pub unchanged: u32,
    pub conflicts: u32,
    pub created_directories: u32,
}

/// Any conflict refuses the whole revert, which then writes nothing. The
/// lists are bounded samples; `counts` is complete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevertPreview {
    pub revert_id: RevertId,
    pub direction: RevertDirection,
    pub records: u32,
    pub counts: RevertCounts,
    pub planned: Vec<RevertPath>,
    pub conflicts: Vec<RevertConflict>,
    pub created_directories: Vec<WorkspacePath>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RevertState {
    Publishing,
    Completed,
    /// Refused midway, after publishing part of its plan.
    Partial,
    /// Interrupted where an entry may or may not have changed.
    Indeterminate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingRevert {
    pub revert_id: RevertId,
    pub direction: RevertDirection,
    pub state: RevertState,
    pub records: u32,
    pub applied_files: u32,
    pub total_files: u32,
    pub reconciliation_required: bool,
    /// The path whose publication stopped the revert short: left as it was
    /// when `Partial`, possibly changed when `Indeterminate`. Absent when
    /// cancellation or a crash stopped it.
    pub stopped_at: Option<WorkspacePath>,
}

/// A holder's reverts awaiting acknowledgement, oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevertStatus {
    pub pending: Vec<PendingRevert>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupPreview {
    /// Records begun longer ago than any call may run, which are abandoned.
    pub stale_open_records: u32,
    /// The oldest records retention evicts to bring the store within its
    /// target.
    pub evicted_records: u32,
    pub reclaimable_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupSummary {
    pub abandoned_open_records: u32,
    pub evicted_records: u32,
    pub deleted_objects: u32,
    pub reclaimed_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeOperationPreview {
    Revert(RevertPreview),
    Cleanup(CleanupPreview),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedChangeOperation {
    pub operation: OperationHandle,
    pub preview: ChangeOperationPreview,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeOperationResult {
    Revert(RevertStatus),
    Cleanup(CleanupSummary),
}

/// The change records of one session directory. A handle is bound to its
/// directory when it is made, so no method names a binding or cursor.
#[async_trait]
pub trait WorkspaceChangeService: Send + Sync {
    /// Captures what `request.scope` holds before a call runs. A service
    /// lowers each of `request.limits` to at most what its store accepts.
    async fn begin(&self, request: &RecordRequest) -> Result<RecordTicket, WorkspaceError>;

    /// Commits what the call changed, or nothing when it changed nothing.
    async fn finish(&self, ticket: &RecordTicket) -> Result<Option<RecordSummary>, WorkspaceError>;

    async fn abandon(&self, ticket: &RecordTicket) -> Result<bool, WorkspaceError>;

    async fn open_records(&self, holder: &RecordHolder) -> Result<Vec<OpenRecord>, WorkspaceError>;

    async fn abandon_open_records(&self, holder: &RecordHolder) -> Result<u32, WorkspaceError>;

    /// The records of `holder` after `after_seq`. A service serves at most its
    /// own largest page, whatever `page_size` asks for.
    async fn records(
        &self,
        holder: &RecordHolder,
        after_seq: Option<u64>,
        page_size: u32,
    ) -> Result<RecordPage, WorkspaceError>;

    /// The holders after `after`, at most the service's own largest page
    /// whatever `page_size` asks for.
    async fn holders(
        &self,
        after: Option<&RecordHolder>,
        page_size: u32,
    ) -> Result<HolderPage, WorkspaceError>;

    /// Makes `to` hold every record `from` holds, returning how many.
    async fn hold(&self, from: &RecordHolder, to: &RecordHolder) -> Result<u32, WorkspaceError>;

    async fn release(
        &self,
        holder: &RecordHolder,
        selection: &ReleaseSelection,
    ) -> Result<ReleaseSummary, WorkspaceError>;

    /// Plans undoing the selected records newest first.
    async fn prepare_revert(
        &self,
        holder: &RecordHolder,
        seqs: &[u64],
    ) -> Result<PreparedChangeOperation, WorkspaceError>;

    /// Plans re-applying every revert of `holder` awaiting acknowledgement.
    async fn prepare_unrevert(
        &self,
        holder: &RecordHolder,
    ) -> Result<PreparedChangeOperation, WorkspaceError>;

    /// Settles the pending reverts of `holder` and deletes the records they
    /// undid, for every holder.
    async fn acknowledge(&self, holder: &RecordHolder) -> Result<RevertStatus, WorkspaceError>;

    async fn status(&self, holder: &RecordHolder) -> Result<RevertStatus, WorkspaceError>;

    async fn prepare_cleanup(
        &self,
        retention_bytes: u64,
    ) -> Result<PreparedChangeOperation, WorkspaceError>;

    async fn execute(
        &self,
        prepared: &PreparedChangeOperation,
    ) -> Result<OperationStatus<ChangeOperationResult>, WorkspaceError>;

    async fn operation_status(
        &self,
        operation: &OperationHandle,
    ) -> Result<OperationStatus<ChangeOperationResult>, WorkspaceError>;

    async fn cancel(
        &self,
        operation: &OperationHandle,
    ) -> Result<CancellationResult, WorkspaceError>;

    async fn release_prepared(
        &self,
        prepared: &PreparedChangeOperation,
    ) -> Result<ReleaseResult, WorkspaceError>;
}

/// Binds a remote workspace's change records to one session's binding and
/// cursor.
pub trait WorkspaceChangeBinder: Send + Sync {
    fn bind(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
    ) -> Arc<dyn WorkspaceChangeService>;
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    use test_case::test_case;

    use super::{RecordScope, RecordedPath};
    use crate::WorkspacePath;

    const ROOT: &str = "/work/project";

    #[test_case("/work/project/src/lib.rs" => RecordedPath::Inside(WorkspacePath::new("src/lib.rs").unwrap()) ; "a_path_inside_is_relative_to_the_root")]
    #[test_case("/work/project/./src//lib.rs" => RecordedPath::Inside(WorkspacePath::new("src/lib.rs").unwrap()) ; "redundant_separators_are_normalized")]
    #[test_case("/work/projector/lib.rs" => RecordedPath::Outside ; "a_shared_prefix_is_no_ancestor")]
    #[test_case("/work/other/../project/lib.rs" => RecordedPath::Unplaced ; "a_parent_step_is_unplaced_even_when_it_leads_back")]
    #[test_case("/work/project" => RecordedPath::Unplaced ; "the_root_itself_is_unplaced")]
    #[test_case("/work/project/bell\u{7}.txt" => RecordedPath::Unplaced ; "a_name_no_workspace_path_spells_is_unplaced")]
    fn local_paths_are_placed(path: &str) -> RecordedPath {
        RecordedPath::of(Path::new(path), Path::new(ROOT))
    }

    #[test_case(&[] => Some(RecordScope::Workspace) ; "no_named_file_may_change_any")]
    #[test_case(&["/tmp/scratch"] => None ; "files_outside_need_no_record")]
    #[test_case(&["/work/project/a", "/tmp/scratch", "/work/project/b"] => Some(RecordScope::Paths(paths(&["a", "b"]))) ; "outside_files_are_dropped")]
    #[test_case(&["/work/project/a", "/work/project/x/../b"] => Some(RecordScope::Workspace) ; "one_unplaced_file_records_everything")]
    fn named_files_are_scoped(files: &[&str]) -> Option<RecordScope> {
        let files: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();
        RecordScope::of_files(&files, Path::new(ROOT))
    }

    fn paths(paths: &[&str]) -> BTreeSet<WorkspacePath> {
        paths
            .iter()
            .map(|path| WorkspacePath::new(*path).unwrap())
            .collect()
    }
}
