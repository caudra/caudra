//! Workspace snapshots stored as git objects, and crash-safe restores between them.
//!
//! One object store per workspace, `workspace-snapshots/<key>/`, is shared by every session that
//! works there. Each session names its snapshots with pointer files in
//! `session-snapshots/<session>/<key>/`, beside its restore journal and unrevert record.

mod capture;
mod restore;
mod retention;
mod storage;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use caudra_storage::id::CaudraId;
use caudra_storage::{
    SessionArtifactLock, StateDir, lock_session_artifacts, lock_session_artifacts_within,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use workcell::snapshot_store::{Entry, SnapshotId, StoreError};

pub use retention::collect_garbage;
pub use storage::{SessionSnapshots, StoreEntry};

pub const DEFAULT_SNAPSHOT_CAP_BYTES: u64 = 512 * 1024 * 1024;
pub const DEFAULT_SNAPSHOT_MAX_FILES: u64 = 50_000;
pub const DEFAULT_SNAPSHOT_MAX_FILE_BYTES: u64 = 100 * 1024 * 1024;
pub const SESSION_SNAPSHOTS_DIR: &str = "session-snapshots";
pub const WORKSPACE_SNAPSHOTS_DIR: &str = "workspace-snapshots";
const SESSION_START_NAME: &str = "session-start";

static STORE_LOCK: Mutex<()> = Mutex::new(());

/// What one capture may cost before it is refused. `max_bytes` is also the
/// retention target of the workspace's shared store: a working tree above it
/// would be over budget from its very first snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotLimits {
    pub max_bytes: u64,
    pub max_files: u64,
    pub max_file_bytes: u64,
}

impl Default for SnapshotLimits {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_SNAPSHOT_CAP_BYTES,
            max_files: DEFAULT_SNAPSHOT_MAX_FILES,
            max_file_bytes: DEFAULT_SNAPSHOT_MAX_FILE_BYTES,
        }
    }
}

/// `enabled` has no place here: a store that cannot capture must still restore
/// what it already holds, so the switch belongs to whoever asks for a capture.
impl From<caudra_config::SnapshotsConfig> for SnapshotLimits {
    fn from(config: caudra_config::SnapshotsConfig) -> Self {
        Self {
            max_bytes: config.max_bytes,
            max_files: config.max_files,
            max_file_bytes: config.max_file_bytes,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "id")]
pub enum SnapshotKey {
    SessionStart,
    Checkpoint(CaudraId),
}

/// The snapshot a checkpoint chain resolves to: its nearest checkpoint that
/// still exists, or the session start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedSnapshot {
    pub key: SnapshotKey,
    pub id: SnapshotId,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictPolicy {
    #[default]
    Abort,
    Overwrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalState {
    Prepare,
    Applied,
    Cleared,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "snapshot")]
pub enum RestoreTarget {
    Snapshot(SnapshotKey),
    Unrevert,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathOutcomeKind {
    Created,
    Modified,
    MetadataChanged,
    Deleted,
    Unchanged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathOutcome {
    pub path: String,
    pub kind: PathOutcomeKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreReport {
    pub target: RestoreTarget,
    pub paths: Vec<PathOutcome>,
    pub recovered: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<CaudraId>,
}

/// A path whose current content is not what the restore expected. Hashes are
/// git object ids.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathConflict {
    pub path: String,
    pub expected_hash: Option<String>,
    pub actual_hash: Option<String>,
    pub actual_exists: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreFailureKind {
    Conflicts,
    NotFound,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RestoreStatus {
    Restored {
        report: RestoreReport,
    },
    Failed {
        kind: RestoreFailureKind,
        message: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        conflicts: Vec<PathConflict>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        worktree_reverted: bool,
    },
}

impl RestoreStatus {
    pub fn from_result(result: &Result<RestoreReport, SnapshotError>) -> Self {
        match result {
            Ok(report) => Self::Restored {
                report: report.clone(),
            },
            Err(error) => {
                let (kind, conflicts) = match error {
                    SnapshotError::Conflicts(conflicts) => {
                        (RestoreFailureKind::Conflicts, conflicts.clone())
                    }
                    SnapshotError::NotFound(_) => (RestoreFailureKind::NotFound, Vec::new()),
                    _ => (RestoreFailureKind::Other, Vec::new()),
                };
                Self::Failed {
                    kind,
                    message: error.to_string(),
                    conflicts,
                    worktree_reverted: false,
                }
            }
        }
    }

    pub fn is_restored(&self) -> bool {
        matches!(self, Self::Restored { .. })
    }

    pub fn worktree_is_reverted(&self) -> bool {
        match self {
            Self::Restored { .. } => true,
            Self::Failed {
                worktree_reverted, ..
            } => *worktree_reverted,
        }
    }

    pub fn mark_worktree_reverted(&mut self) {
        if let Self::Failed {
            worktree_reverted, ..
        } = self
        {
            *worktree_reverted = true;
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("snapshot for {0} not found")]
    NotFound(String),
    #[error("restore aborted due to path conflicts: {0:?}")]
    Conflicts(Vec<PathConflict>),
    #[error("invalid snapshot path: {0}")]
    InvalidPath(String),
    #[error("snapshot pointer {0:?} does not name a snapshot")]
    InvalidPointer(PathBuf),
    #[error("unsupported file type at snapshot path: {0}")]
    UnsupportedFileType(String),
    #[error("restore journal belongs to {expected:?}, not {actual:?}")]
    JournalRootMismatch { expected: PathBuf, actual: PathBuf },
    #[error("snapshot store is bound to workspace root {expected:?}, not {actual:?}")]
    WorkspaceRootMismatch { expected: PathBuf, actual: PathBuf },
    #[error("snapshots of one workspace cannot be shared with another")]
    ForeignWorkspace,
    #[error("restore operation {0} must be acknowledged before another restore")]
    RestoreOperationPending(CaudraId),
    #[error("restore journal belongs to operation {actual:?}, not {expected}")]
    RestoreOperationMismatch {
        expected: CaudraId,
        actual: Option<CaudraId>,
    },
    #[error("restore operation {0} has not finished applying")]
    RestoreOperationNotApplied(CaudraId),
    #[error("snapshot store lock is poisoned")]
    LockPoisoned,
    #[error(
        "workspace is too large to snapshot: {files} files and {bytes} bytes, over {exceeded} = {limit}"
    )]
    WorkspaceTooLarge {
        files: u64,
        bytes: u64,
        exceeded: &'static str,
        limit: u64,
    },
    #[error("{root} cannot be snapshotted: {reason}")]
    WorkspaceUnsupported { root: String, reason: &'static str },
}

impl SnapshotError {
    /// Whether this is a deliberate refusal of the workspace rather than a
    /// failure to carry out a capture. A refusal turns file revert off and lets
    /// the work continue; a failure does not.
    pub fn is_workspace_refusal(&self) -> bool {
        matches!(
            self,
            Self::WorkspaceTooLarge { .. } | Self::WorkspaceUnsupported { .. }
        )
    }
}

pub struct SnapshotStore {
    /// `<root>/session-snapshots/<session>/<key>`: this session's pointers and
    /// restore records.
    dir: PathBuf,
    /// `<root>/workspace-snapshots/<key>`: the objects every session of the
    /// workspace shares.
    repository: PathBuf,
    cap_bytes: u64,
    limits: SnapshotLimits,
    artifact_state: Option<StateDir>,
}

/// Held across every change to a store. The artifact lock orders this process
/// against every other Caudra on the machine, the mutex orders its threads.
struct StoreLock {
    _artifacts: Option<SessionArtifactLock>,
    _threads: MutexGuard<'static, ()>,
}

impl SnapshotStore {
    /// `session`'s snapshots of the workspace `key` under `root`, without the
    /// artifact lock: for a root no other process writes.
    pub fn new(root: &Path, session: CaudraId, key: &str) -> Self {
        Self {
            dir: root
                .join(SESSION_SNAPSHOTS_DIR)
                .join(session.to_string())
                .join(key),
            repository: root.join(WORKSPACE_SNAPSHOTS_DIR).join(key),
            cap_bytes: DEFAULT_SNAPSHOT_CAP_BYTES,
            limits: SnapshotLimits::default(),
            artifact_state: None,
        }
    }

    /// `session`'s snapshots of the workspace `key` in the state directory,
    /// holding the artifact lock that every writer of that directory takes.
    pub fn new_managed(state: StateDir, session: CaudraId, key: &str) -> Self {
        let store = Self::new(state.path(), session, key);
        Self {
            artifact_state: Some(state),
            ..store
        }
    }

    /// The capture budget, whose `max_bytes` is also the retention target.
    pub fn with_limits(mut self, limits: SnapshotLimits) -> Self {
        self.cap_bytes = limits.max_bytes;
        self.limits = limits;
        self
    }

    /// The retention target alone. Eviction and refusal are separate concerns:
    /// a store meant to overflow after a few files must still capture them.
    #[cfg(test)]
    fn with_cap(mut self, cap_bytes: u64) -> Self {
        self.cap_bytes = cap_bytes;
        self
    }

    pub fn snapshot_session_start(&self, cwd: &Path) -> Result<SnapshotId, SnapshotError> {
        let _lock = self.lock()?;
        let root = self.bind_root(cwd)?;
        if let Some(id) = self.pointer(SnapshotKey::SessionStart)? {
            return Ok(id);
        }
        self.capture_to(&root, SnapshotKey::SessionStart, SystemTime::now())
            .map(|(id, _)| id)
    }

    pub fn snapshot(&self, cwd: &Path, checkpoint: CaudraId) -> Result<SnapshotId, SnapshotError> {
        let _lock = self.lock()?;
        let root = self.bind_root(cwd)?;
        self.capture_to(
            &root,
            SnapshotKey::Checkpoint(checkpoint),
            SystemTime::now(),
        )
        .map(|(id, _)| id)
    }

    /// Session start plus `head`, holding the artifact lock across both and
    /// giving it up after `budget`. Answers whether the capture ran.
    ///
    /// A `false` is not a failure. The artifact lock is one file for every
    /// caudra on the machine, so waiting for it is waiting on unrelated
    /// workspaces, and a caller on a deadline would rather lose one snapshot
    /// than inherit that wait.
    pub fn capture_head_within(
        &self,
        cwd: &Path,
        head: Option<CaudraId>,
        budget: Duration,
    ) -> Result<bool, SnapshotError> {
        let artifacts = match self.artifact_state.as_ref() {
            None => None,
            Some(state) => match lock_session_artifacts_within(state, budget)
                .map_err(|error| SnapshotError::from(io::Error::other(error)))?
            {
                None => return Ok(false),
                held => held,
            },
        };
        let _lock = StoreLock {
            _artifacts: artifacts,
            _threads: lock_threads()?,
        };
        let root = self.bind_root(cwd)?;
        if !self.has_session_start() {
            self.capture_to(&root, SnapshotKey::SessionStart, SystemTime::now())?;
        }
        if let Some(head) = head {
            self.capture_to(&root, SnapshotKey::Checkpoint(head), SystemTime::now())?;
        }
        Ok(true)
    }

    pub fn snapshot_id(&self, key: SnapshotKey) -> Result<SnapshotId, SnapshotError> {
        self.pointer(key)?
            .ok_or_else(|| SnapshotError::NotFound(key_name(key)))
    }

    /// Every path a snapshot holds, in path order.
    pub fn snapshot_entries(&self, key: SnapshotKey) -> Result<Vec<Entry>, SnapshotError> {
        let id = self.snapshot_id(key)?;
        Ok(self.open_repository()?.entries(&id)?)
    }

    pub fn resolve_snapshot(
        &self,
        checkpoint_and_ancestors: &[CaudraId],
    ) -> Result<ResolvedSnapshot, SnapshotError> {
        let chain = checkpoint_and_ancestors
            .iter()
            .map(|checkpoint| SnapshotKey::Checkpoint(*checkpoint))
            .chain([SnapshotKey::SessionStart]);
        for key in chain {
            if let Some(id) = self.pointer(key)? {
                return Ok(ResolvedSnapshot { key, id });
            }
        }
        Err(SnapshotError::NotFound(key_name(
            checkpoint_and_ancestors
                .first()
                .map_or(SnapshotKey::SessionStart, |checkpoint| {
                    SnapshotKey::Checkpoint(*checkpoint)
                }),
        )))
    }

    pub fn has_session_start(&self) -> bool {
        self.pointer_path(SnapshotKey::SessionStart).exists()
    }

    pub fn has_checkpoint(&self, checkpoint: CaudraId) -> bool {
        self.pointer_path(SnapshotKey::Checkpoint(checkpoint))
            .exists()
    }

    fn lock(&self) -> Result<StoreLock, SnapshotError> {
        let artifacts = self
            .artifact_state
            .as_ref()
            .map(lock_session_artifacts)
            .transpose()
            .map_err(io::Error::other)?;
        Ok(StoreLock {
            _artifacts: artifacts,
            _threads: lock_threads()?,
        })
    }
}

fn lock_threads() -> Result<MutexGuard<'static, ()>, SnapshotError> {
    STORE_LOCK.lock().map_err(|_| SnapshotError::LockPoisoned)
}

fn key_name(key: SnapshotKey) -> String {
    match key {
        SnapshotKey::SessionStart => SESSION_START_NAME.to_owned(),
        SnapshotKey::Checkpoint(checkpoint) => checkpoint.to_string(),
    }
}

pub fn workspace_key(cwd: &Path) -> Result<String, SnapshotError> {
    let root = canonical_root(cwd)?;
    let mut hasher = Sha256::new();
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        hasher.update(root.as_os_str().as_bytes());
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        for unit in root.as_os_str().encode_wide() {
            hasher.update(unit.to_le_bytes());
        }
    }
    #[cfg(not(any(unix, windows)))]
    hasher.update(root.to_string_lossy().as_bytes());
    Ok(hex_encode(&hasher.finalize()))
}

fn canonical_root(path: &Path) -> Result<PathBuf, SnapshotError> {
    let root = std::fs::canonicalize(path)?;
    if root.is_dir() {
        Ok(root)
    } else {
        Err(io::Error::new(io::ErrorKind::NotADirectory, root.display().to_string()).into())
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Instant;

    use caudra_storage::lock_session_artifacts;
    use tempfile::TempDir;
    use workcell::snapshot_store::{Content, ObjectId};

    use super::*;
    use crate::snapshots::storage::OBJECTS_DIR;

    pub(super) const ALPHA: &str = "alpha";
    pub(super) const BETA: &str = "beta";
    pub(super) const KEY: &str = "workspace";
    pub(super) const OTHER_KEY: &str = "other-workspace";
    pub(super) const SESSION: u32 = 1_000;
    pub(super) const OTHER_SESSION: u32 = 1_001;
    const STATE_DIR: &str = "state";
    const WORKTREE_DIR: &str = "repo";
    /// Loose objects sit in a directory named by the first two hex digits.
    const FAN_OUT_DIGITS: usize = 2;

    /// Ids that sort by `sequence`, as time-ordered ids sort by creation.
    pub(super) fn id(sequence: u32) -> CaudraId {
        CaudraId::from_bytes(u128::from(sequence).to_be_bytes())
    }

    /// A worktree, with the state directory its snapshots go to beside it.
    pub(super) fn setup() -> (TempDir, PathBuf) {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join(WORKTREE_DIR);
        fs::create_dir_all(&root).unwrap();
        (temp, root)
    }

    pub(super) fn state_root(temp: &TempDir) -> PathBuf {
        temp.path().join(STATE_DIR)
    }

    pub(super) fn session_store(temp: &TempDir, session: u32) -> SnapshotStore {
        SnapshotStore::new(&state_root(temp), id(session), KEY)
    }

    pub(super) fn store_in(temp: &TempDir) -> SnapshotStore {
        session_store(temp, SESSION)
    }

    pub(super) fn write(root: &Path, relative: &str, bytes: impl AsRef<[u8]>) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, bytes).unwrap();
    }

    pub(super) fn read(root: &Path, relative: &str) -> String {
        fs::read_to_string(root.join(relative)).unwrap()
    }

    pub(super) fn content(store: &SnapshotStore, key: SnapshotKey, path: &str) -> Option<Content> {
        store
            .snapshot_entries(key)
            .unwrap()
            .into_iter()
            .find(|entry| entry.path == path)
            .map(|entry| entry.content)
    }

    pub(super) fn paths(store: &SnapshotStore, key: SnapshotKey) -> Vec<String> {
        store
            .snapshot_entries(key)
            .unwrap()
            .into_iter()
            .map(|entry| entry.path)
            .collect()
    }

    pub(super) fn object_count(store: &SnapshotStore) -> u64 {
        store.open_repository().unwrap().usage().unwrap().objects
    }

    pub(super) fn object_path(store: &SnapshotStore, oid: &ObjectId) -> PathBuf {
        let hex = oid.to_string();
        let (fan_out, rest) = hex.split_at(FAN_OUT_DIGITS);
        store.repository.join(OBJECTS_DIR).join(fan_out).join(rest)
    }

    #[test]
    fn the_nearest_existing_checkpoint_resolves_before_the_session_start() {
        let (temp, root) = setup();
        write(&root, "file.txt", ALPHA);
        let store = store_in(&temp);
        let start = store.snapshot_session_start(&root).unwrap();
        write(&root, "file.txt", BETA);
        let ancestor = id(1);
        let captured = store.snapshot(&root, ancestor).unwrap();

        assert_eq!(
            store.resolve_snapshot(&[id(2), ancestor]).unwrap(),
            ResolvedSnapshot {
                key: SnapshotKey::Checkpoint(ancestor),
                id: captured,
            }
        );
        assert_eq!(
            store.resolve_snapshot(&[id(2)]).unwrap(),
            ResolvedSnapshot {
                key: SnapshotKey::SessionStart,
                id: start,
            }
        );
    }

    #[test]
    fn workspace_keys_are_canonical_and_distinguish_roots() {
        let (temp, root) = setup();
        let other = temp.path().join("other");
        fs::create_dir(&other).unwrap();

        assert_eq!(
            workspace_key(&root).unwrap(),
            workspace_key(&root.join(".")).unwrap()
        );
        assert_ne!(
            workspace_key(&root).unwrap(),
            workspace_key(&other).unwrap()
        );
    }

    /// Exit takes this path, so a lock held by an unrelated caudra has to come
    /// back as "did not run" inside the budget rather than as a stalled exit.
    #[test]
    fn capture_head_within_gives_up_on_a_held_artifact_lock() {
        const BUSY_MSG: &str = "a held artifact lock must report no capture";
        const FREE_MSG: &str = "a free artifact lock must capture";
        const BUDGET: Duration = Duration::from_millis(120);
        let (temp, root) = setup();
        let state = StateDir::from_path(state_root(&temp));
        fs::create_dir_all(state.path()).unwrap();
        write(&root, "kept.txt", ALPHA);
        let store = SnapshotStore::new_managed(state.clone(), id(SESSION), KEY);

        let held = lock_session_artifacts(&state).unwrap();
        let started = Instant::now();
        assert!(
            !store.capture_head_within(&root, None, BUDGET).unwrap(),
            "{BUSY_MSG}"
        );
        assert!(started.elapsed() >= BUDGET, "{BUSY_MSG}");

        drop(held);
        assert!(
            store.capture_head_within(&root, None, BUDGET).unwrap(),
            "{FREE_MSG}"
        );
        assert!(store.has_session_start(), "{FREE_MSG}");
    }
}
