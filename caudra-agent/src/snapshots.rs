//! Content-addressed working-tree snapshots and crash-safe restores.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::num::NonZero;
use std::panic::resume_unwind;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use caudra_storage::id::CaudraId;
use caudra_storage::{
    SessionArtifactLock, StateDir, lock_session_artifacts, lock_session_artifacts_within,
};
use ignore::WalkBuilder;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::debug;

const OBJECTS_DIR: &str = "objects";
const SESSION_START_NAME: &str = "session-start";
const MANIFEST_EXT: &str = "json";
const JOURNAL_NAME: &str = "restore-journal.json";
const UNREVERT_NAME: &str = "unrevert.json";
const WORKSPACE_ROOT_NAME: &str = "workspace-root.json";
const HASH_LEN: usize = 32;
const UNIX_FILE_MODE_MASK: u32 = 0o7777;

static STORE_LOCK: Mutex<()> = Mutex::new(());

pub const DEFAULT_SNAPSHOT_CAP_BYTES: u64 = 512 * 1024 * 1024;
pub const DEFAULT_SNAPSHOT_MAX_FILES: u64 = 50_000;
pub const DEFAULT_SNAPSHOT_MAX_FILE_BYTES: u64 = 100 * 1024 * 1024;
pub const SESSION_SNAPSHOTS_DIR: &str = "session-snapshots";

const LIMIT_MAX_FILES: &str = "max_files";
const LIMIT_MAX_BYTES: &str = "max_bytes";
const ROOT_IS_FILESYSTEM_ROOT: &str = "it is the filesystem root";
const ROOT_IS_HOME: &str = "it is the home directory";

type RelPath = String;

/// What one capture may cost before it is refused. `max_bytes` doubles as the
/// object-store cap: `session-start` is a permanent GC root and objects are
/// stored uncompressed, so a working tree above the cap is over budget from the
/// very first snapshot and the two numbers must not disagree.
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
}

pub type Manifest = BTreeMap<String, FileEntry>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "id")]
pub enum SnapshotKey {
    SessionStart,
    Checkpoint(CaudraId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedManifest {
    pub key: SnapshotKey,
    pub manifest: Manifest,
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
    #[error("snapshot for {0} not found")]
    NotFound(String),
    #[error("restore aborted due to path conflicts: {0:?}")]
    Conflicts(Vec<PathConflict>),
    #[error("invalid path in snapshot manifest: {0}")]
    InvalidManifestPath(String),
    #[error("unsupported file type at snapshot path: {0}")]
    UnsupportedFileType(String),
    #[error("snapshot object {0} is missing")]
    MissingObject(String),
    #[error("snapshot object {0} failed content verification")]
    CorruptObject(String),
    #[error("restore journal belongs to {expected:?}, not {actual:?}")]
    JournalRootMismatch { expected: PathBuf, actual: PathBuf },
    #[error("snapshot store is bound to workspace root {expected:?}, not {actual:?}")]
    WorkspaceRootMismatch { expected: PathBuf, actual: PathBuf },
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

trait ContentHasher: Send + Sync {
    fn hash(&self, bytes: &[u8]) -> [u8; HASH_LEN];
}

struct Sha256Hasher;

impl ContentHasher for Sha256Hasher {
    fn hash(&self, bytes: &[u8]) -> [u8; HASH_LEN] {
        hash_bytes(bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ObservedPath {
    Absent,
    File(FileEntry),
    Other,
}

impl ObservedPath {
    fn entry(&self) -> Option<&FileEntry> {
        match self {
            Self::File(entry) => Some(entry),
            Self::Absent | Self::Other => None,
        }
    }

    fn matches(&self, expected: Option<&FileEntry>) -> bool {
        match (self, expected) {
            (Self::Absent, None) => true,
            (Self::File(actual), Some(expected)) => actual == expected,
            (Self::Other, _) | (Self::Absent, Some(_)) | (Self::File(_), None) => false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RestoreJournal {
    state: JournalState,
    root: PathBuf,
    before: Manifest,
    target: Manifest,
    paths: Vec<RelPath>,
    outcomes: Vec<PathOutcome>,
    destination: RestoreTarget,
    policy: ConflictPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    operation_id: Option<CaudraId>,
}

impl RestoreJournal {
    fn report(&self, recovered: bool) -> RestoreReport {
        RestoreReport {
            target: self.destination,
            paths: self.outcomes.clone(),
            recovered,
            operation_id: self.operation_id,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UnrevertRecord {
    before: Manifest,
    after: Manifest,
    paths: Vec<RelPath>,
}

pub struct SnapshotStore {
    dir: PathBuf,
    cap_bytes: u64,
    limits: SnapshotLimits,
    hasher: Arc<dyn ContentHasher>,
    artifact_state: Option<StateDir>,
}

/// One workspace store on disk, as `caudra storage snapshots` reports it.
#[derive(Debug, Clone, Serialize)]
pub struct StoreEntry {
    pub session_id: String,
    pub workspace_key: String,
    /// The worktree this store captures, or `None` when the marker is gone and
    /// the store is orphaned.
    pub root: Option<PathBuf>,
    pub bytes: u64,
    pub objects: u64,
    /// Manifest names, so `session-start` and each checkpoint id are visible
    /// rather than reduced to a count.
    pub manifests: Vec<String>,
}

/// Bytes and file count under `dir`, in one pass.
///
/// One walk rather than two: a store holds an object per distinct file version
/// the worktree ever had, so on a large workspace this is the expensive part of
/// reporting, and counting and sizing separately doubled it for no reason.
fn tree_totals(dir: &Path) -> (u64, u64) {
    let Ok(entries) = fs::read_dir(dir) else {
        return (0, 0);
    };
    entries
        .flatten()
        .fold((0, 0), |(bytes, files), entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => {
                let (sub_bytes, sub_files) = tree_totals(&entry.path());
                (bytes + sub_bytes, files + sub_files)
            }
            Ok(_) => (
                bytes + entry.metadata().map(|meta| meta.len()).unwrap_or(0),
                files + 1,
            ),
            Err(_) => (bytes, files),
        })
}

/// Bytes of the files sitting directly in `dir`, ignoring subdirectories.
///
/// A store's manifests, journal, and workspace root live beside `objects/`, so
/// this covers the whole store when added to an `objects/` walk, without
/// descending that expensive tree a second time.
fn shallow_bytes(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| {
            entry
                .file_type()
                .map(|kind| !kind.is_dir())
                .unwrap_or(false)
        })
        .filter_map(|entry| entry.metadata().ok())
        .map(|meta| meta.len())
        .sum()
}

fn manifest_names(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.path().extension() == Some(OsStr::new(MANIFEST_EXT)))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name != WORKSPACE_ROOT_NAME)
        .collect();
    names.sort();
    names
}

impl SnapshotStore {
    pub fn new(snapshots_dir: PathBuf) -> Self {
        Self::with_cap(snapshots_dir, DEFAULT_SNAPSHOT_CAP_BYTES)
    }

    pub fn new_managed(state_dir: StateDir, snapshots_dir: PathBuf) -> Self {
        let mut store = Self::new(snapshots_dir);
        store.artifact_state = Some(state_dir);
        store
    }

    /// The object-store cap alone, leaving the capture budget at its default.
    /// Eviction and refusal are separate concerns: a test that wants a store to
    /// overflow after a handful of files must not thereby refuse to capture them.
    pub fn with_cap(snapshots_dir: PathBuf, cap_bytes: u64) -> Self {
        Self::with_hasher(snapshots_dir, cap_bytes, Arc::new(Sha256Hasher))
    }

    /// The capture budget, whose `max_bytes` is also the object-store cap.
    pub fn with_limits(mut self, limits: SnapshotLimits) -> Self {
        self.cap_bytes = limits.max_bytes;
        self.limits = limits;
        self
    }

    fn with_hasher(snapshots_dir: PathBuf, cap_bytes: u64, hasher: Arc<dyn ContentHasher>) -> Self {
        Self {
            dir: snapshots_dir,
            cap_bytes,
            limits: SnapshotLimits::default(),
            hasher,
            artifact_state: None,
        }
    }

    fn lock_artifacts(&self) -> Result<Option<SessionArtifactLock>, SnapshotError> {
        self.artifact_state
            .as_ref()
            .map(lock_session_artifacts)
            .transpose()
            .map_err(|error| io::Error::other(error).into())
    }

    pub fn snapshot_session_start(&self, cwd: &Path) -> Result<Manifest, SnapshotError> {
        let _artifact_lock = self.lock_artifacts()?;
        let _guard = lock_store()?;
        let root = self.bind_root(cwd)?;
        if self.has_session_start() {
            return self.load_session_start_manifest();
        }
        let path = self.manifest_path(SnapshotKey::SessionStart);
        self.capture_to(&root, &path, None, "session_start")
    }

    pub fn snapshot(&self, cwd: &Path, checkpoint: CaudraId) -> Result<Manifest, SnapshotError> {
        let _artifact_lock = self.lock_artifacts()?;
        let _guard = lock_store()?;
        let root = self.bind_root(cwd)?;
        let path = self.manifest_path(SnapshotKey::Checkpoint(checkpoint));
        self.capture_to(&root, &path, Some(checkpoint), "checkpoint")
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
        let _artifact_lock = match self.artifact_state.as_ref() {
            None => None,
            Some(state) => match lock_session_artifacts_within(state, budget)
                .map_err(|error| SnapshotError::from(io::Error::other(error)))?
            {
                None => return Ok(false),
                held => held,
            },
        };
        let _guard = lock_store()?;
        let root = self.bind_root(cwd)?;
        if !self.has_session_start() {
            let path = self.manifest_path(SnapshotKey::SessionStart);
            self.capture_to(&root, &path, None, "session_start")?;
        }
        if let Some(head) = head {
            let path = self.manifest_path(SnapshotKey::Checkpoint(head));
            self.capture_to(&root, &path, Some(head), "checkpoint")?;
        }
        Ok(true)
    }

    /// Captures `root`, writes the manifest and enforces the store cap,
    /// timing each part. Capture cost splits between hashing every file and
    /// durably writing the ones whose content is new, and the two want very
    /// different fixes, so they are reported apart.
    fn capture_to(
        &self,
        root: &Path,
        path: &Path,
        preserve: Option<CaudraId>,
        kind: &'static str,
    ) -> Result<Manifest, SnapshotError> {
        let (manifest, stats) = self.capture(root)?;

        let manifest_start = Instant::now();
        self.write_manifest_path(path, &manifest)?;
        let manifest_write = manifest_start.elapsed();

        let gc_start = Instant::now();
        self.enforce_cap_preserving(preserve)?;
        let gc = gc_start.elapsed();

        debug!(
            kind,
            files = stats.files,
            bytes = stats.bytes,
            skipped_large = stats.skipped_large,
            walk_us = micros(stats.walk),
            content_us = micros(stats.content),
            hash_us = micros(stats.hash),
            objects_written = stats.objects_written,
            object_write_us = micros(stats.object_write),
            manifest_us = micros(manifest_write),
            gc_us = micros(gc),
            total_us = micros(stats.walk + stats.content + manifest_write + gc),
            "workspace snapshot"
        );
        Ok(manifest)
    }

    pub fn load_manifest(&self, checkpoint: CaudraId) -> Result<Manifest, SnapshotError> {
        self.load_manifest_path(
            &self.manifest_path(SnapshotKey::Checkpoint(checkpoint)),
            &checkpoint.to_string(),
        )
    }

    pub fn load_session_start_manifest(&self) -> Result<Manifest, SnapshotError> {
        self.load_manifest_path(
            &self.manifest_path(SnapshotKey::SessionStart),
            SESSION_START_NAME,
        )
    }

    pub fn resolve_manifest(
        &self,
        checkpoint_and_ancestors: &[CaudraId],
    ) -> Result<ResolvedManifest, SnapshotError> {
        for checkpoint in checkpoint_and_ancestors {
            let path = self.manifest_path(SnapshotKey::Checkpoint(*checkpoint));
            if path.exists() {
                return Ok(ResolvedManifest {
                    key: SnapshotKey::Checkpoint(*checkpoint),
                    manifest: self.load_manifest(*checkpoint)?,
                });
            }
        }
        if self.has_session_start() {
            return Ok(ResolvedManifest {
                key: SnapshotKey::SessionStart,
                manifest: self.load_session_start_manifest()?,
            });
        }
        Err(SnapshotError::NotFound(
            checkpoint_and_ancestors
                .first()
                .map(ToString::to_string)
                .unwrap_or_else(|| SESSION_START_NAME.to_owned()),
        ))
    }

    pub fn restore(
        &self,
        cwd: &Path,
        source_checkpoint_and_ancestors: &[CaudraId],
        target_checkpoint_and_ancestors: &[CaudraId],
    ) -> Result<RestoreReport, SnapshotError> {
        self.restore_with_policy(
            cwd,
            source_checkpoint_and_ancestors,
            target_checkpoint_and_ancestors,
            ConflictPolicy::Abort,
        )
    }

    pub fn restore_with_policy(
        &self,
        cwd: &Path,
        source_checkpoint_and_ancestors: &[CaudraId],
        target_checkpoint_and_ancestors: &[CaudraId],
        policy: ConflictPolicy,
    ) -> Result<RestoreReport, SnapshotError> {
        self.restore_impl(
            cwd,
            source_checkpoint_and_ancestors,
            target_checkpoint_and_ancestors,
            policy,
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

    fn restore_impl(
        &self,
        cwd: &Path,
        source_checkpoint_and_ancestors: &[CaudraId],
        target_checkpoint_and_ancestors: &[CaudraId],
        policy: ConflictPolicy,
        operation_id: Option<CaudraId>,
    ) -> Result<RestoreReport, SnapshotError> {
        let _artifact_lock = self.lock_artifacts()?;
        let _guard = lock_store()?;
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
        let source = self.resolve_manifest(source_checkpoint_and_ancestors)?;
        let target = self.resolve_manifest(target_checkpoint_and_ancestors)?;
        let paths = manifest_diff(&source.manifest, &target.manifest);
        let destination = RestoreTarget::Snapshot(target.key);
        self.prepare_restore(
            &root,
            &source.manifest,
            &target.manifest,
            paths,
            destination,
            policy,
            operation_id,
        )?;
        let report = self
            .finish_matching_pending(&root, false, operation_id)?
            .ok_or_else(|| {
                SnapshotError::Io(io::Error::other("prepared restore journal disappeared"))
            })?;
        validate_report(&report, operation_id, destination)?;
        Ok(report)
    }

    pub fn unrevert(&self, cwd: &Path) -> Result<RestoreReport, SnapshotError> {
        self.unrevert_with_policy(cwd, ConflictPolicy::Abort)
    }

    pub fn unrevert_with_policy(
        &self,
        cwd: &Path,
        policy: ConflictPolicy,
    ) -> Result<RestoreReport, SnapshotError> {
        self.unrevert_impl(cwd, policy, None)
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
        let _artifact_lock = self.lock_artifacts()?;
        let _guard = lock_store()?;
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
        let record: UnrevertRecord = self.read_named(UNREVERT_NAME, "unrevert")?;
        self.prepare_restore(
            &root,
            &record.after,
            &record.before,
            record.paths,
            RestoreTarget::Unrevert,
            policy,
            operation_id,
        )?;
        let report = self
            .finish_matching_pending(&root, false, operation_id)?
            .ok_or_else(|| {
                SnapshotError::Io(io::Error::other("prepared unrevert journal disappeared"))
            })?;
        validate_report(&report, operation_id, RestoreTarget::Unrevert)?;
        Ok(report)
    }

    pub fn recover(&self, cwd: &Path) -> Result<Option<RestoreReport>, SnapshotError> {
        let _artifact_lock = self.lock_artifacts()?;
        let _guard = lock_store()?;
        let root = self.bind_root(cwd)?;
        self.finish_pending(&root, true)
    }

    pub fn journal_state(&self) -> Result<Option<JournalState>, SnapshotError> {
        match self.read_journal() {
            Ok(journal) => Ok(Some(journal.state)),
            Err(SnapshotError::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn journal_operation_id(&self) -> Result<Option<CaudraId>, SnapshotError> {
        match self.read_journal() {
            Ok(journal) => Ok(journal.operation_id),
            Err(SnapshotError::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn acknowledge_operation(
        &self,
        cwd: &Path,
        operation_id: CaudraId,
    ) -> Result<(), SnapshotError> {
        let _artifact_lock = self.lock_artifacts()?;
        let _guard = lock_store()?;
        let root = self.bind_root(cwd)?;
        let journal = match self.read_journal() {
            Ok(journal) => journal,
            Err(SnapshotError::NotFound(_)) => return Ok(()),
            Err(error) => return Err(error),
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
        remove_file_durable(&self.journal_path())?;
        Ok(())
    }

    pub fn has_session_start(&self) -> bool {
        self.manifest_path(SnapshotKey::SessionStart).exists()
    }

    pub fn has_checkpoint(&self, checkpoint: CaudraId) -> bool {
        self.manifest_path(SnapshotKey::Checkpoint(checkpoint))
            .exists()
    }

    pub fn copy_ancestry_to(
        &self,
        destination: &SnapshotStore,
        checkpoints: &[CaudraId],
    ) -> Result<(), SnapshotError> {
        let _artifact_lock = self.lock_artifacts()?;
        let _guard = lock_store()?;
        let mut manifests = Vec::new();
        if self.has_session_start() {
            manifests.push((
                SnapshotKey::SessionStart,
                self.load_session_start_manifest()?,
            ));
        }
        for &checkpoint in checkpoints {
            if self.has_checkpoint(checkpoint) {
                manifests.push((
                    SnapshotKey::Checkpoint(checkpoint),
                    self.load_manifest(checkpoint)?,
                ));
            }
        }
        if manifests.is_empty() {
            return Ok(());
        }

        let root: PathBuf = self.read_named(WORKSPACE_ROOT_NAME, "workspace root")?;
        destination.ensure_dirs()?;
        destination.bind_canonical_root(&root)?;
        let mut copied = HashSet::new();
        for (_, manifest) in &manifests {
            for entry in manifest.values() {
                if !copied.insert(entry.hash.clone()) {
                    continue;
                }
                let source = self.objects_dir().join(&entry.hash);
                let bytes = fs::read(&source).map_err(|error| {
                    if error.kind() == io::ErrorKind::NotFound {
                        SnapshotError::MissingObject(entry.hash.clone())
                    } else {
                        SnapshotError::Io(error)
                    }
                })?;
                if hex_encode(&self.hasher.hash(&bytes)) != entry.hash {
                    return Err(SnapshotError::CorruptObject(entry.hash.clone()));
                }
                destination.write_object_named(&entry.hash, &bytes)?;
            }
        }
        // Ordered before the manifests below, which name these objects.
        destination.sync_objects();
        for (key, manifest) in manifests {
            destination.write_manifest_path(&destination.manifest_path(key), &manifest)?;
        }
        destination.enforce_cap_preserving(checkpoints.last().copied())
    }

    pub fn object_bytes(&self) -> Result<u64, SnapshotError> {
        let mut total = 0_u64;
        let entries = match fs::read_dir(self.objects_dir()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            let metadata = entry?.metadata()?;
            if metadata.is_file() {
                total = total.saturating_add(metadata.len());
            }
        }
        Ok(total)
    }

    pub fn enforce_cap(&self) -> Result<(), SnapshotError> {
        let _artifact_lock = self.lock_artifacts()?;
        let _guard = lock_store()?;
        self.ensure_dirs()?;
        self.enforce_cap_preserving(None)
    }

    fn ensure_dirs(&self) -> Result<(), SnapshotError> {
        if let Some(state_dir) = &self.artifact_state {
            create_managed_directory(state_dir, &self.dir)?;
            create_managed_directory(state_dir, &self.objects_dir())?;
        } else {
            fs::create_dir_all(&self.dir)?;
            fs::create_dir_all(self.objects_dir())?;
        }
        Ok(())
    }

    fn bind_root(&self, cwd: &Path) -> Result<PathBuf, SnapshotError> {
        self.ensure_dirs()?;
        let root = canonical_root(cwd)?;
        self.bind_canonical_root(&root)?;
        Ok(root)
    }

    fn bind_canonical_root(&self, root: &Path) -> Result<(), SnapshotError> {
        let path = self.dir.join(WORKSPACE_ROOT_NAME);
        if path.exists() {
            let expected: PathBuf = read_json(&path, "workspace root")?;
            if expected != root {
                return Err(SnapshotError::WorkspaceRootMismatch {
                    expected,
                    actual: root.to_path_buf(),
                });
            }
            return Ok(());
        }
        write_json(&path, &root)?;
        Ok(())
    }

    fn objects_dir(&self) -> PathBuf {
        self.dir.join(OBJECTS_DIR)
    }

    /// Every workspace store under `snapshots_dir`, largest first.
    ///
    /// Reads the layout rather than a manifest index because the store is the
    /// authority on its own size: `session-snapshots/<session>/<workspace
    /// key>/{objects,*.json}`. A store whose `workspace-root.json` is missing
    /// or unreadable still reports its cost with `root: None`, since an
    /// orphaned store is exactly what an operator is looking for.
    pub fn store_entries(snapshots_dir: &Path) -> Vec<StoreEntry> {
        let mut entries = Vec::new();
        let Ok(sessions) = fs::read_dir(snapshots_dir) else {
            return entries;
        };
        for session in sessions.flatten() {
            let Ok(workspaces) = fs::read_dir(session.path()) else {
                continue;
            };
            for workspace in workspaces.flatten() {
                let dir = workspace.path();
                if !dir.is_dir() {
                    continue;
                }
                let (object_bytes, objects) = tree_totals(&dir.join(OBJECTS_DIR));
                entries.push(StoreEntry {
                    session_id: session.file_name().to_string_lossy().into_owned(),
                    workspace_key: workspace.file_name().to_string_lossy().into_owned(),
                    root: read_json(&dir.join(WORKSPACE_ROOT_NAME), "workspace root").ok(),
                    bytes: object_bytes + shallow_bytes(&dir),
                    objects,
                    manifests: manifest_names(&dir),
                });
            }
        }
        entries.sort_by(|a, b| {
            b.bytes
                .cmp(&a.bytes)
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
        entries
    }

    fn manifest_path(&self, key: SnapshotKey) -> PathBuf {
        let name = match key {
            SnapshotKey::SessionStart => SESSION_START_NAME.to_owned(),
            SnapshotKey::Checkpoint(checkpoint) => checkpoint.to_string(),
        };
        self.dir.join(format!("{name}.{MANIFEST_EXT}"))
    }

    fn journal_path(&self) -> PathBuf {
        self.dir.join(JOURNAL_NAME)
    }

    fn unrevert_path(&self) -> PathBuf {
        self.dir.join(UNREVERT_NAME)
    }

    fn capture(&self, root: &Path) -> Result<(Manifest, CaptureStats), SnapshotError> {
        let start = Instant::now();
        let walked = self.walk_working_tree(root)?;
        let files = walked.files;
        let mut stats = CaptureStats {
            walk: start.elapsed(),
            files: files.len() as u64,
            skipped_large: walked.skipped_large,
            ..CaptureStats::default()
        };
        let content_start = Instant::now();
        let mut manifest = Manifest::new();
        for part in self.read_content(&files)? {
            stats.bytes += part.bytes;
            stats.hash += part.hash;
            stats.object_write += part.object_write;
            stats.objects_written += part.objects_written;
            manifest.extend(part.entries);
        }

        if stats.objects_written > 0 {
            let sync_start = Instant::now();
            self.sync_objects();
            stats.object_write += sync_start.elapsed();
        }
        stats.content = content_start.elapsed();
        Ok((manifest, stats))
    }

    /// Hashing the working tree is the bulk of a capture and every file is
    /// independent, so the read and hash of one must not wait on another.
    /// Objects are content addressed, so two workers landing on the same
    /// hash stage separate temp files and rename identical bytes into place.
    fn read_content(&self, files: &[WalkedFile]) -> Result<Vec<CapturePart>, SnapshotError> {
        if files.is_empty() {
            return Ok(Vec::new());
        }
        let workers = thread::available_parallelism()
            .map_or(1, NonZero::get)
            .min(files.len());
        let next = AtomicUsize::new(0);
        thread::scope(|scope| {
            let workers: Vec<_> = (0..workers)
                .map(|_| scope.spawn(|| self.read_claimed(files, &next)))
                .collect();
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap_or_else(|panic| resume_unwind(panic)))
                .collect()
        })
    }

    /// Workers claim one file at a time rather than splitting the list up
    /// front, because a working tree is a handful of large files among many
    /// small ones and any fixed split leaves everyone waiting on whoever
    /// drew the largest.
    fn read_claimed(
        &self,
        files: &[WalkedFile],
        next: &AtomicUsize,
    ) -> Result<CapturePart, SnapshotError> {
        let mut part = CapturePart::default();
        loop {
            let Some(file) = files.get(next.fetch_add(1, Ordering::Relaxed)) else {
                return Ok(part);
            };

            let read_start = Instant::now();
            let bytes = fs::read(&file.absolute)?;
            let hash = self.hasher.hash(&bytes);
            part.hash += read_start.elapsed();
            part.bytes += bytes.len() as u64;

            let write_start = Instant::now();
            part.objects_written += u64::from(self.write_object(&hash, &bytes)?);
            part.object_write += write_start.elapsed();

            part.entries.push((
                file.relative.clone(),
                FileEntry {
                    hash: hex_encode(&hash),
                    mode: file_mode(&file.metadata),
                },
            ));
        }
    }

    /// Enumerates what a capture would hash, and refuses before hashing any of
    /// it when the tree is beyond the budget. Everything here reads metadata
    /// only, so refusing a 900k-file home directory costs the inodes up to the
    /// ceiling rather than the whole tree.
    fn walk_working_tree(&self, root: &Path) -> Result<WalkedTree, SnapshotError> {
        if let Some(reason) = unsupported_root(root) {
            return Err(SnapshotError::WorkspaceUnsupported {
                root: root.display().to_string(),
                reason,
            });
        }
        let excluded = fs::canonicalize(&self.dir)
            .or_else(|_| std::path::absolute(&self.dir))
            .unwrap_or_else(|_| self.dir.clone());
        let mut builder = WalkBuilder::new(root);
        // `require_git(false)`: a `.gitignore` without a repository around it is
        // still the user saying which paths are disposable, and every other walk
        // in Caudra honours it. The alternative was no filtering at all outside a
        // worktree, which is how a session rooted at `$HOME` came to hash it.
        // `same_file_system`: a network share or external drive mounted inside
        // the workspace is not part of the project.
        builder
            .hidden(false)
            .ignore(true)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .require_git(false)
            .same_file_system(true);
        builder.filter_entry(move |entry| {
            if entry.depth() == 0 {
                return true;
            }
            if entry.file_name() == OsStr::new(".git") || entry.path().starts_with(&excluded) {
                return false;
            }
            // A directory carrying its own `.git` is a different repository,
            // and this snapshot describes one worktree. Git agrees: a nested
            // repository contributes nothing to the parent's status, whether
            // it is a submodule (`.git` file) or an unregistered clone (`.git`
            // directory). Descending anyway meant a workspace whose 357
            // tracked files sat beside a 928k-file data repository read and
            // hashed all 7 GB of it on every capture.
            !(entry.file_type().is_some_and(|kind| kind.is_dir())
                && entry.path().join(".git").exists())
        });

        let mut walked = WalkedTree::default();
        for result in builder.build() {
            let entry = result.map_err(|error| io::Error::other(error.to_string()))?;
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(root)
                .map_err(|error| io::Error::other(error.to_string()))?;
            let relative = relative.to_str().ok_or_else(|| {
                SnapshotError::InvalidManifestPath(relative.to_string_lossy().into_owned())
            })?;
            validate_relative_path(relative)?;
            let metadata = entry
                .metadata()
                .map_err(|error| io::Error::other(error.to_string()))?;
            // Skipped rather than refused: one oversized blob beside a normal
            // project should not cost the project its revert. `restore` only
            // touches paths named by a manifest, so a skipped file is never
            // deleted, it just cannot be restored. It is also what keeps
            // `read_claimed`'s whole-file read bounded.
            if metadata.len() > self.limits.max_file_bytes {
                walked.skipped_large += 1;
                continue;
            }
            walked.bytes += metadata.len();
            walked.files.push(WalkedFile {
                relative: relative.to_owned(),
                absolute: entry.path().to_path_buf(),
                metadata,
            });
            if let Some((exceeded, limit)) = self.limits.exceeded_by(&walked) {
                return Err(SnapshotError::WorkspaceTooLarge {
                    files: walked.files.len() as u64,
                    bytes: walked.bytes,
                    exceeded,
                    limit,
                });
            }
        }
        Ok(walked)
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_restore(
        &self,
        root: &Path,
        source: &Manifest,
        target: &Manifest,
        paths: Vec<RelPath>,
        destination: RestoreTarget,
        policy: ConflictPolicy,
        operation_id: Option<CaudraId>,
    ) -> Result<(), SnapshotError> {
        let mut before = Manifest::new();
        let mut observed = BTreeMap::new();
        let mut conflicts = Vec::new();

        for path in &paths {
            validate_relative_path(path)?;
            let current = self.observe_path(root, path, true)?;
            match &current {
                ObservedPath::File(entry) => {
                    before.insert(path.clone(), entry.clone());
                }
                ObservedPath::Absent => {
                    before.remove(path);
                }
                ObservedPath::Other => {}
            }
            if !current.matches(source.get(path)) {
                conflicts.push(path_conflict(path, source.get(path), &current));
            }
            observed.insert(path.clone(), current);
        }

        if !conflicts.is_empty() && policy == ConflictPolicy::Abort {
            return Err(SnapshotError::Conflicts(conflicts));
        }
        if let Some(path) = observed
            .iter()
            .find_map(|(path, current)| matches!(current, ObservedPath::Other).then_some(path))
        {
            return Err(SnapshotError::UnsupportedFileType(path.clone()));
        }

        let outcomes = paths
            .iter()
            .map(|path| PathOutcome {
                path: path.clone(),
                kind: outcome_kind(
                    observed.get(path).expect("all paths were observed"),
                    target.get(path),
                ),
            })
            .collect();
        let journal = RestoreJournal {
            state: JournalState::Prepare,
            root: root.to_path_buf(),
            before,
            target: target.clone(),
            paths,
            outcomes,
            destination,
            policy,
            operation_id,
        };
        self.write_journal(&journal)
    }

    fn finish_pending(
        &self,
        root: &Path,
        recovered: bool,
    ) -> Result<Option<RestoreReport>, SnapshotError> {
        loop {
            let Some(report) = self.advance_journal(root, recovered)? else {
                if self.journal_state()?.is_none() {
                    return Ok(None);
                }
                continue;
            };
            return Ok(Some(report));
        }
    }

    fn finish_matching_pending(
        &self,
        root: &Path,
        recovered: bool,
        operation_id: Option<CaudraId>,
    ) -> Result<Option<RestoreReport>, SnapshotError> {
        let journal = match self.read_journal() {
            Ok(journal) => journal,
            Err(SnapshotError::NotFound(_)) => return Ok(None),
            Err(error) => return Err(error),
        };
        match (operation_id, journal.operation_id) {
            (Some(expected), actual) => {
                if actual != Some(expected) {
                    return Err(SnapshotError::RestoreOperationMismatch { expected, actual });
                }
            }
            (None, Some(pending)) => {
                return Err(SnapshotError::RestoreOperationPending(pending));
            }
            (None, None) => {}
        }
        self.finish_pending(root, recovered)
    }

    fn advance_journal(
        &self,
        root: &Path,
        recovered: bool,
    ) -> Result<Option<RestoreReport>, SnapshotError> {
        let mut journal = match self.read_journal() {
            Ok(journal) => journal,
            Err(SnapshotError::NotFound(_)) => return Ok(None),
            Err(error) => return Err(error),
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
                self.write_journal(&journal)?;
                Ok(None)
            }
            JournalState::Applied => {
                self.verify_applied(root, &journal)?;
                match journal.destination {
                    RestoreTarget::Snapshot(_) => self.merge_unrevert(&journal)?,
                    RestoreTarget::Unrevert => self.remove_unrevert()?,
                }
                journal.state = JournalState::Cleared;
                self.write_journal(&journal)?;
                Ok(None)
            }
            JournalState::Cleared => {
                if journal.operation_id.is_none() {
                    remove_file_durable(&self.journal_path())?;
                }
                Ok(Some(journal.report(recovered)))
            }
        }
    }

    fn apply_prepared(&self, root: &Path, journal: &RestoreJournal) -> Result<(), SnapshotError> {
        let mut observed = BTreeMap::new();
        let mut conflicts = Vec::new();
        for path in &journal.paths {
            let current = self.observe_path(root, path, false)?;
            let before = journal.before.get(path);
            let target = journal.target.get(path);
            let partial_target = partial_target_write(&current, before, target);
            if !current.matches(before) && !current.matches(target) && !partial_target {
                conflicts.push(path_conflict(path, before, &current));
            }
            observed.insert(path, current);
        }
        if !conflicts.is_empty() && journal.policy == ConflictPolicy::Abort {
            return Err(SnapshotError::Conflicts(conflicts));
        }

        let pending = journal
            .paths
            .iter()
            .filter(|path| {
                !observed
                    .get(path)
                    .expect("all paths were observed")
                    .matches(journal.target.get(*path))
            })
            .cloned()
            .collect::<Vec<_>>();
        self.verify_target_objects(&pending, &journal.target)?;
        for path in &pending {
            self.apply_path(root, path, journal.target.get(path))?;
        }
        Ok(())
    }

    fn verify_applied(&self, root: &Path, journal: &RestoreJournal) -> Result<(), SnapshotError> {
        let mut conflicts = Vec::new();
        let mut pending = Vec::new();
        for path in &journal.paths {
            let current = self.observe_path(root, path, false)?;
            if current.matches(journal.target.get(path)) {
                continue;
            }
            if journal.policy == ConflictPolicy::Overwrite {
                pending.push(path.clone());
            } else {
                conflicts.push(path_conflict(path, journal.target.get(path), &current));
            }
        }
        if !conflicts.is_empty() {
            return Err(SnapshotError::Conflicts(conflicts));
        }
        self.verify_target_objects(&pending, &journal.target)?;
        for path in pending {
            self.apply_path(root, &path, journal.target.get(&path))?;
        }
        Ok(())
    }

    fn apply_path(
        &self,
        root: &Path,
        relative: &str,
        target: Option<&FileEntry>,
    ) -> Result<(), SnapshotError> {
        let path = checked_destination(root, relative)?;
        match target {
            Some(entry) => {
                let bytes = self.read_object(entry)?;
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                caudra_storage::atomic_write(&path, &bytes)
                    .map_err(|error| io::Error::other(error.to_string()))?;
                set_file_mode(&path, entry.mode)?;
                fs::File::open(&path)?.sync_all()?;
            }
            None => match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_file() => remove_file_durable(&path)?,
                Ok(_) => return Err(SnapshotError::UnsupportedFileType(relative.to_owned())),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            },
        }
        Ok(())
    }

    fn verify_target_objects(
        &self,
        paths: &[RelPath],
        target: &Manifest,
    ) -> Result<(), SnapshotError> {
        let mut verified = HashSet::new();
        for path in paths {
            let Some(entry) = target.get(path) else {
                continue;
            };
            if verified.insert(&entry.hash) {
                self.read_object(entry)?;
            }
        }
        Ok(())
    }

    fn read_object(&self, entry: &FileEntry) -> Result<Vec<u8>, SnapshotError> {
        let object = self.objects_dir().join(&entry.hash);
        let bytes = fs::read(&object).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                SnapshotError::MissingObject(entry.hash.clone())
            } else {
                SnapshotError::Io(error)
            }
        })?;
        if hex_encode(&self.hasher.hash(&bytes)) != entry.hash {
            return Err(SnapshotError::CorruptObject(entry.hash.clone()));
        }
        Ok(bytes)
    }

    fn observe_path(
        &self,
        root: &Path,
        relative: &str,
        store_object: bool,
    ) -> Result<ObservedPath, SnapshotError> {
        let path = checked_destination(root, relative)?;
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(ObservedPath::Absent);
            }
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_file() {
            return Ok(ObservedPath::Other);
        }
        let bytes = fs::read(path)?;
        let hash = self.hasher.hash(&bytes);
        if store_object && self.write_object(&hash, &bytes)? {
            self.sync_objects();
        }
        Ok(ObservedPath::File(FileEntry {
            hash: hex_encode(&hash),
            mode: file_mode(&metadata),
        }))
    }

    fn write_object(&self, hash: &[u8; HASH_LEN], bytes: &[u8]) -> Result<bool, SnapshotError> {
        self.write_object_named(&hex_encode(hash), bytes)
    }

    /// Reports whether the object was written. Skipping an object that is
    /// already present is the common case, so the count separates durable
    /// writes from verification reads when a capture is slow.
    ///
    /// Objects are named by their own hash, so a present object of the right
    /// length is taken as correct rather than re-read and re-hashed. Anything
    /// that slips through is still caught: `read_object` verifies the hash on
    /// every restore, which is where a bad object would do damage.
    ///
    /// Not durable on return. Callers must `sync_objects` before writing a
    /// manifest that names the object.
    fn write_object_named(&self, hash: &str, bytes: &[u8]) -> Result<bool, SnapshotError> {
        let path = self.objects_dir().join(hash);
        match fs::metadata(&path) {
            Ok(metadata) if metadata.len() == bytes.len() as u64 => return Ok(false),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        caudra_storage::atomic_write_deferred(&path, bytes)
            .map_err(|error| io::Error::other(error.to_string()))?;
        Ok(true)
    }

    /// Makes every object written since the last call durable. One flush for
    /// the whole batch, ordered before the manifest that references them.
    fn sync_objects(&self) {
        caudra_storage::sync_dir(&self.objects_dir());
    }

    fn write_manifest_path(&self, path: &Path, manifest: &Manifest) -> Result<(), SnapshotError> {
        write_json(path, manifest)
    }

    fn load_manifest_path(&self, path: &Path, name: &str) -> Result<Manifest, SnapshotError> {
        let manifest: Manifest = read_json(path, name)?;
        validate_manifest(&manifest)?;
        Ok(manifest)
    }

    fn read_named<T: DeserializeOwned>(
        &self,
        name: &str,
        missing_name: &str,
    ) -> Result<T, SnapshotError> {
        read_json(&self.dir.join(name), missing_name)
    }

    fn read_journal(&self) -> Result<RestoreJournal, SnapshotError> {
        self.read_named(JOURNAL_NAME, "restore journal")
    }

    fn write_journal(&self, journal: &RestoreJournal) -> Result<(), SnapshotError> {
        write_json(&self.journal_path(), journal)
    }

    fn write_unrevert(&self, record: &UnrevertRecord) -> Result<(), SnapshotError> {
        write_json(&self.unrevert_path(), record)
    }

    fn merge_unrevert(&self, journal: &RestoreJournal) -> Result<(), SnapshotError> {
        let mut record = match self.read_named(UNREVERT_NAME, "unrevert") {
            Ok(record) => record,
            Err(SnapshotError::NotFound(_)) => UnrevertRecord {
                before: Manifest::new(),
                after: Manifest::new(),
                paths: Vec::new(),
            },
            Err(error) => return Err(error),
        };
        let mut paths = record.paths.iter().cloned().collect::<BTreeSet<_>>();
        for path in &journal.paths {
            if paths.insert(path.clone()) {
                match journal.before.get(path) {
                    Some(entry) => {
                        record.before.insert(path.clone(), entry.clone());
                    }
                    None => {
                        record.before.remove(path);
                    }
                }
            }
            match journal.target.get(path) {
                Some(entry) => {
                    record.after.insert(path.clone(), entry.clone());
                }
                None => {
                    record.after.remove(path);
                }
            }
        }
        record.before.retain(|path, _| paths.contains(path));
        record.after.retain(|path, _| paths.contains(path));
        record.paths = paths.into_iter().collect();
        self.write_unrevert(&record)
    }

    /// Takes the store lock: a capture reads the unrevert record to build
    /// the set of live objects, so a delete racing that read lets the sweep
    /// collect objects only the unrevert still needs.
    pub fn discard_unrevert(&self) -> Result<(), SnapshotError> {
        let _artifact_lock = self.lock_artifacts()?;
        let _guard = lock_store()?;
        if self.dir.try_exists()? {
            self.ensure_dirs()?;
        }
        self.remove_unrevert()
    }

    fn remove_unrevert(&self) -> Result<(), SnapshotError> {
        match remove_file_durable(&self.unrevert_path()) {
            Ok(()) => Ok(()),
            Err(SnapshotError::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Sweeping costs a parse of every manifest in the store, so it is worth
    /// doing only once the objects it could reclaim actually matter. An object
    /// is unreferenced only after the manifest naming it is replaced or
    /// evicted, and eviction happens here, so gating the sweep on the cap
    /// leaves nothing to collect in the common case.
    fn enforce_cap_preserving(&self, preserve: Option<CaudraId>) -> Result<(), SnapshotError> {
        if self.object_bytes()? <= self.cap_bytes {
            return Ok(());
        }
        self.gc_unreferenced_objects()?;
        if self.object_bytes()? <= self.cap_bytes {
            return Ok(());
        }
        let manifests = self.checkpoint_manifests_oldest_first()?;
        let preserve = preserve.or_else(|| manifests.last().map(|(checkpoint, _, _)| *checkpoint));
        for (checkpoint, path, _) in &manifests {
            if self.object_bytes()? <= self.cap_bytes {
                break;
            }
            if Some(*checkpoint) == preserve {
                continue;
            }
            remove_file_durable(path)?;
            self.gc_unreferenced_objects()?;
        }
        Ok(())
    }

    fn checkpoint_manifests_oldest_first(
        &self,
    ) -> Result<Vec<(CaudraId, PathBuf, SystemTime)>, SnapshotError> {
        let mut manifests = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(OsStr::to_str) != Some(MANIFEST_EXT) {
                continue;
            }
            let Some(checkpoint) = path
                .file_stem()
                .and_then(OsStr::to_str)
                .and_then(|name| name.parse::<CaudraId>().ok())
            else {
                continue;
            };
            let modified = entry
                .metadata()?
                .modified()
                .unwrap_or(SystemTime::UNIX_EPOCH);
            manifests.push((checkpoint, path, modified));
        }
        manifests.sort_by(|left, right| {
            left.2
                .cmp(&right.2)
                .then_with(|| left.0.to_string().cmp(&right.0.to_string()))
        });
        Ok(manifests)
    }

    /// Flushes once for the whole sweep rather than per file. An unreferenced
    /// object is garbage, so a deletion that does not survive a crash costs
    /// nothing but the next sweep, unlike a write whose manifest is already
    /// on disk.
    fn gc_unreferenced_objects(&self) -> Result<(), SnapshotError> {
        let live = self.live_hashes()?;
        let mut removed = false;
        for entry in fs::read_dir(self.objects_dir())? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file()
                && let Some(name) = path.file_name().and_then(OsStr::to_str)
                && !live.contains(name)
            {
                fs::remove_file(&path)?;
                removed = true;
            }
        }
        if removed {
            self.sync_objects();
        }
        Ok(())
    }

    fn live_hashes(&self) -> Result<HashSet<String>, SnapshotError> {
        let mut live = HashSet::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(OsStr::to_str) != Some(MANIFEST_EXT) {
                continue;
            }
            let Some(name) = path.file_stem().and_then(OsStr::to_str) else {
                continue;
            };
            if name != SESSION_START_NAME && name.parse::<CaudraId>().is_err() {
                continue;
            }
            let manifest: Manifest = read_json(&path, name)?;
            live.extend(manifest.values().map(|entry| entry.hash.clone()));
        }
        if self.unrevert_path().exists() {
            let record: UnrevertRecord = self.read_named(UNREVERT_NAME, "unrevert")?;
            live.extend(record.before.values().map(|entry| entry.hash.clone()));
            live.extend(record.after.values().map(|entry| entry.hash.clone()));
        }
        if self.journal_path().exists() {
            let journal = self.read_journal()?;
            live.extend(journal.before.values().map(|entry| entry.hash.clone()));
            live.extend(journal.target.values().map(|entry| entry.hash.clone()));
        }
        Ok(live)
    }
}

struct WalkedFile {
    relative: RelPath,
    absolute: PathBuf,
    metadata: fs::Metadata,
}

/// What one walk found, before any of it is read.
#[derive(Default)]
struct WalkedTree {
    files: Vec<WalkedFile>,
    bytes: u64,
    skipped_large: u64,
}

impl SnapshotLimits {
    fn exceeded_by(&self, walked: &WalkedTree) -> Option<(&'static str, u64)> {
        if walked.files.len() as u64 > self.max_files {
            Some((LIMIT_MAX_FILES, self.max_files))
        } else if walked.bytes > self.max_bytes {
            Some((LIMIT_MAX_BYTES, self.max_bytes))
        } else {
            None
        }
    }
}

/// Roots no project lives at, refused before a single directory is read. A home
/// directory is never a project root, and these are the cases no ignore file
/// would ever catch, because neither has one.
///
/// Deliberately not here: a root that is a filesystem mount point. A
/// bind-mounted project root is how a container normally sees its project, so
/// refusing one would disable revert for every such setup. `same_file_system`
/// already stops the walk from leaving the device, and the size ceiling covers
/// what is left.
fn unsupported_root(root: &Path) -> Option<&'static str> {
    if root.parent().is_none() {
        return Some(ROOT_IS_FILESYSTEM_ROOT);
    }
    // Canonical on both sides: the root arrives resolved, and a home directory
    // reached through a symlink is still the home directory.
    caudra_storage::paths::home()
        .map(|home| fs::canonicalize(&home).unwrap_or(home))
        .is_some_and(|home| home == root)
        .then_some(ROOT_IS_HOME)
}

/// Where the time in one `capture` went.
#[derive(Default)]
struct CaptureStats {
    files: u64,
    bytes: u64,
    walk: Duration,
    /// Wall time of the whole parallel read, hash and write phase, against
    /// which `hash` and `object_write` are the work summed across workers.
    content: Duration,
    /// Reading every file and hashing it.
    hash: Duration,
    /// Verifying present objects and durably writing absent ones.
    object_write: Duration,
    objects_written: u64,
    /// Files the walk left out for exceeding `max_file_bytes`, and so the count
    /// of paths this snapshot cannot restore.
    skipped_large: u64,
}

#[derive(Default)]
struct CapturePart {
    entries: Vec<(RelPath, FileEntry)>,
    bytes: u64,
    hash: Duration,
    object_write: Duration,
    objects_written: u64,
}

/// Microseconds, because the per-file phases accumulate hundreds of
/// sub-millisecond samples and truncating each one to whole milliseconds
/// reports close to zero for the dominant cost.
fn micros(duration: Duration) -> u64 {
    duration.as_micros() as u64
}

fn hash_bytes(bytes: &[u8]) -> [u8; HASH_LEN] {
    Sha256::digest(bytes).into()
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

fn manifest_diff(left: &Manifest, right: &Manifest) -> Vec<RelPath> {
    left.keys()
        .chain(right.keys())
        .filter(|path| left.get(*path) != right.get(*path))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn partial_target_write(
    current: &ObservedPath,
    before: Option<&FileEntry>,
    target: Option<&FileEntry>,
) -> bool {
    let (ObservedPath::File(current), Some(target)) = (current, target) else {
        return false;
    };
    if current == target || current.hash != target.hash {
        return false;
    }
    match before {
        Some(before) => before.hash != target.hash && current.mode == before.mode,
        None => true,
    }
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
        return Err(SnapshotError::Io(io::Error::other(
            "restore journal changed while applying",
        )));
    }
    Ok(())
}

fn outcome_kind(current: &ObservedPath, target: Option<&FileEntry>) -> PathOutcomeKind {
    if current.matches(target) {
        return PathOutcomeKind::Unchanged;
    }
    match (current.entry(), target) {
        (None, Some(_)) => PathOutcomeKind::Created,
        (Some(_), None) => PathOutcomeKind::Deleted,
        (Some(current), Some(target)) if current.hash == target.hash => {
            PathOutcomeKind::MetadataChanged
        }
        (Some(_), Some(_)) => PathOutcomeKind::Modified,
        (None, None) => PathOutcomeKind::Unchanged,
    }
}

fn path_conflict(path: &str, expected: Option<&FileEntry>, actual: &ObservedPath) -> PathConflict {
    PathConflict {
        path: path.to_owned(),
        expected_hash: expected.map(|entry| entry.hash.clone()),
        actual_hash: actual.entry().map(|entry| entry.hash.clone()),
        actual_exists: !matches!(actual, ObservedPath::Absent),
    }
}

fn canonical_root(path: &Path) -> Result<PathBuf, SnapshotError> {
    let root = fs::canonicalize(path)?;
    if root.is_dir() {
        Ok(root)
    } else {
        Err(io::Error::new(io::ErrorKind::NotADirectory, root.display().to_string()).into())
    }
}

fn checked_destination(root: &Path, relative: &str) -> Result<PathBuf, SnapshotError> {
    validate_relative_path(relative)?;
    let relative_path = Path::new(relative);
    let mut current = root.to_path_buf();
    let components: Vec<_> = relative_path.components().collect();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            return Err(SnapshotError::InvalidManifestPath(relative.to_owned()));
        };
        current.push(name);
        if index + 1 == components.len() {
            break;
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => return Err(SnapshotError::UnsupportedFileType(relative.to_owned())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(current)
}

fn validate_manifest(manifest: &Manifest) -> Result<(), SnapshotError> {
    for (path, entry) in manifest {
        validate_relative_path(path)?;
        if entry.hash.len() != HASH_LEN * 2
            || !entry.hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(SnapshotError::CorruptObject(entry.hash.clone()));
        }
    }
    Ok(())
}

fn validate_relative_path(path: &str) -> Result<(), SnapshotError> {
    if path.is_empty()
        || Path::new(path)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        Err(SnapshotError::InvalidManifestPath(path.to_owned()))
    } else {
        Ok(())
    }
}

fn read_json<T: DeserializeOwned>(path: &Path, name: &str) -> Result<T, SnapshotError> {
    let bytes = fs::read(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            SnapshotError::NotFound(name.to_owned())
        } else {
            SnapshotError::Io(error)
        }
    })?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<(), SnapshotError> {
    let bytes = serde_json::to_vec(value)?;
    caudra_storage::atomic_write(path, &bytes)
        .map_err(|error| io::Error::other(error.to_string()).into())
}

fn lock_store() -> Result<MutexGuard<'static, ()>, SnapshotError> {
    STORE_LOCK.lock().map_err(|_| SnapshotError::LockPoisoned)
}

fn create_managed_directory(state_dir: &StateDir, target: &Path) -> Result<(), SnapshotError> {
    let relative = target.strip_prefix(state_dir.path()).map_err(|_| {
        SnapshotError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "managed snapshot path is outside the state directory",
        ))
    })?;
    let mut path = state_dir.path().to_path_buf();
    validate_or_create_directory(&path, false)?;
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(SnapshotError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "managed snapshot path is not normalized",
            )));
        };
        path.push(component);
        validate_or_create_directory(&path, true)?;
    }
    Ok(())
}

fn validate_or_create_directory(path: &Path, create: bool) -> Result<(), SnapshotError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound && create => {
            fs::create_dir(path)?;
            #[cfg(unix)]
            if let Some(parent) = path.parent() {
                fs::File::open(parent)?.sync_all()?;
            }
            fs::symlink_metadata(path)?
        }
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(SnapshotError::Io(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("snapshot path {} is not a real directory", path.display()),
        )));
    }
    Ok(())
}

fn remove_file_durable(path: &Path) -> Result<(), SnapshotError> {
    fs::remove_file(path)?;
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn file_mode(metadata: &fs::Metadata) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Some(metadata.permissions().mode() & UNIX_FILE_MODE_MASK)
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}

fn set_file_mode(path: &Path, mode: Option<u32>) -> Result<(), SnapshotError> {
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode & UNIX_FILE_MODE_MASK))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
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
    use std::fs::FileTimes;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    const ALPHA: &str = "alpha";
    const BETA: &str = "beta";

    struct CountingHasher(AtomicUsize);

    impl CountingHasher {
        const fn new() -> Self {
            Self(AtomicUsize::new(0))
        }

        fn count(&self) -> usize {
            self.0.load(Ordering::SeqCst)
        }
    }

    impl ContentHasher for CountingHasher {
        fn hash(&self, bytes: &[u8]) -> [u8; HASH_LEN] {
            self.0.fetch_add(1, Ordering::SeqCst);
            hash_bytes(bytes)
        }
    }

    fn checkpoint(sequence: u32) -> CaudraId {
        let mut bytes = [0u8; 16];
        bytes[12..].copy_from_slice(&sequence.to_be_bytes());
        CaudraId::from_bytes(bytes)
    }

    fn setup() -> (TempDir, PathBuf, PathBuf) {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("repo");
        let snapshots = temp.path().join("snapshots");
        fs::create_dir_all(&root).unwrap();
        (temp, root, snapshots)
    }

    fn write(root: &Path, relative: &str, bytes: impl AsRef<[u8]>) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, bytes).unwrap();
    }

    fn outcome(report: &RestoreReport, path: &str) -> PathOutcomeKind {
        report
            .paths
            .iter()
            .find(|outcome| outcome.path == path)
            .unwrap()
            .kind
    }

    #[test]
    fn restore_modifications_creations_deletions_and_unrevert() {
        let (_temp, root, snapshots) = setup();
        write(&root, "modified.txt", ALPHA);
        write(&root, "deleted.txt", BETA);
        let store = SnapshotStore::new(snapshots);
        store.snapshot_session_start(&root).unwrap();

        write(&root, "modified.txt", "changed");
        fs::remove_file(root.join("deleted.txt")).unwrap();
        write(&root, "created.txt", "new");
        let source = checkpoint(1);
        store.snapshot(&root, source).unwrap();

        let report = store.restore(&root, &[source], &[]).unwrap();
        assert_eq!(
            fs::read_to_string(root.join("modified.txt")).unwrap(),
            ALPHA
        );
        assert_eq!(fs::read_to_string(root.join("deleted.txt")).unwrap(), BETA);
        assert!(!root.join("created.txt").exists());
        assert_eq!(outcome(&report, "modified.txt"), PathOutcomeKind::Modified);
        assert_eq!(outcome(&report, "deleted.txt"), PathOutcomeKind::Created);
        assert_eq!(outcome(&report, "created.txt"), PathOutcomeKind::Deleted);

        store.unrevert(&root).unwrap();
        assert_eq!(
            fs::read_to_string(root.join("modified.txt")).unwrap(),
            "changed"
        );
        assert!(!root.join("deleted.txt").exists());
        assert_eq!(fs::read_to_string(root.join("created.txt")).unwrap(), "new");
    }

    #[test]
    fn chained_restores_keep_original_and_later_path_baselines() {
        let (_temp, root, snapshots) = setup();
        write(&root, "a.txt", "first-target");
        let store = SnapshotStore::new(snapshots);
        store.snapshot_session_start(&root).unwrap();

        write(&root, "a.txt", "second-target");
        write(&root, "b.txt", "second-target-b");
        let second_target = checkpoint(1);
        store.snapshot(&root, second_target).unwrap();

        write(&root, "a.txt", "original-before-first-revert");
        fs::remove_file(root.join("b.txt")).unwrap();
        let first_source = checkpoint(2);
        store.snapshot(&root, first_source).unwrap();
        store.restore(&root, &[first_source], &[]).unwrap();

        write(&root, "b.txt", "current-before-second-revert");
        let second_source = checkpoint(3);
        store.snapshot(&root, second_source).unwrap();
        store
            .restore(&root, &[second_source], &[second_target])
            .unwrap();
        assert_eq!(
            fs::read_to_string(root.join("a.txt")).unwrap(),
            "second-target"
        );
        assert_eq!(
            fs::read_to_string(root.join("b.txt")).unwrap(),
            "second-target-b"
        );

        store.unrevert(&root).unwrap();
        assert_eq!(
            fs::read_to_string(root.join("a.txt")).unwrap(),
            "original-before-first-revert"
        );
        assert_eq!(
            fs::read_to_string(root.join("b.txt")).unwrap(),
            "current-before-second-revert"
        );
    }

    #[test]
    fn binary_contents_round_trip_exactly() {
        let (_temp, root, snapshots) = setup();
        let original = [0, 1, 2, 0xff, 0, 0x80];
        write(&root, "binary.dat", original);
        let store = SnapshotStore::new(snapshots);
        store.snapshot_session_start(&root).unwrap();
        write(&root, "binary.dat", [9, 0, 8, 0, 7]);
        let source = checkpoint(1);
        store.snapshot(&root, source).unwrap();

        store.restore(&root, &[source], &[]).unwrap();
        assert_eq!(fs::read(root.join("binary.dat")).unwrap(), original);
    }

    #[test]
    fn capture_replaces_a_corrupt_existing_object() {
        let (_temp, root, snapshots) = setup();
        write(&root, "file.txt", ALPHA);
        let store = SnapshotStore::new(snapshots);
        let baseline = store.snapshot_session_start(&root).unwrap();
        let object = store.objects_dir().join(&baseline["file.txt"].hash);
        fs::write(&object, "corrupt").unwrap();

        store.snapshot(&root, checkpoint(1)).unwrap();
        assert_eq!(fs::read(&object).unwrap(), ALPHA.as_bytes());

        write(&root, "file.txt", BETA);
        let source = checkpoint(2);
        store.snapshot(&root, source).unwrap();
        store.restore(&root, &[source], &[]).unwrap();
        assert_eq!(fs::read_to_string(root.join("file.txt")).unwrap(), ALPHA);
    }

    /// Objects are named by their content hash, so `capture` trusts a
    /// present object of the right length instead of re-reading every one.
    /// That trade is only safe because restore still verifies, so pin both
    /// halves: no repair on write, and no silent bad restore either.
    #[test]
    fn same_length_object_corruption_surfaces_on_restore() {
        const SAME_LENGTH_GARBAGE: &str = "BRAVO";
        assert_eq!(SAME_LENGTH_GARBAGE.len(), ALPHA.len());

        let (_temp, root, snapshots) = setup();
        write(&root, "keep.txt", ALPHA);
        write(&root, "file.txt", ALPHA);
        let store = SnapshotStore::new(snapshots);
        let baseline = store.snapshot_session_start(&root).unwrap();
        let object = store.objects_dir().join(&baseline["file.txt"].hash);
        fs::write(&object, SAME_LENGTH_GARBAGE).unwrap();

        // `keep.txt` still hashes to the corrupt object, so this capture sees
        // it and must leave it alone rather than pay to re-read every object.
        write(&root, "file.txt", BETA);
        let source = checkpoint(1);
        store.snapshot(&root, source).unwrap();
        assert_eq!(fs::read_to_string(&object).unwrap(), SAME_LENGTH_GARBAGE);

        // Restoring back to the session start needs those bytes, and verifies.
        let error = store.restore(&root, &[source], &[]).unwrap_err();
        assert!(
            matches!(error, SnapshotError::CorruptObject(_)),
            "expected corruption to surface, got {error:?}"
        );
        assert_eq!(
            fs::read_to_string(root.join("file.txt")).unwrap(),
            BETA,
            "a failed restore must leave the workspace alone"
        );
    }

    #[cfg(unix)]
    #[test]
    fn executable_mode_is_restored() {
        use std::os::unix::fs::PermissionsExt;

        let (_temp, root, snapshots) = setup();
        write(&root, "run.sh", "#!/bin/sh\n");
        fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        let store = SnapshotStore::new(snapshots);
        store.snapshot_session_start(&root).unwrap();
        fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o644)).unwrap();
        let source = checkpoint(1);
        store.snapshot(&root, source).unwrap();

        let report = store.restore(&root, &[source], &[]).unwrap();
        let mode = fs::metadata(root.join("run.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755);
        assert_eq!(outcome(&report, "run.sh"), PathOutcomeKind::MetadataChanged);
    }

    #[cfg(unix)]
    #[test]
    fn special_permission_bits_are_captured_and_restored() {
        use std::os::unix::fs::PermissionsExt;

        const TARGET_MODE: u32 = 0o7640;

        let (_temp, root, snapshots) = setup();
        write(&root, "special", ALPHA);
        fs::set_permissions(
            root.join("special"),
            fs::Permissions::from_mode(TARGET_MODE),
        )
        .unwrap();
        let store = SnapshotStore::new(snapshots);
        let target = store.snapshot_session_start(&root).unwrap();
        assert_eq!(target["special"].mode, Some(TARGET_MODE));

        fs::set_permissions(root.join("special"), fs::Permissions::from_mode(0o600)).unwrap();
        let source = checkpoint(1);
        store.snapshot(&root, source).unwrap();
        store.restore(&root, &[source], &[]).unwrap();

        let mode = fs::metadata(root.join("special"))
            .unwrap()
            .permissions()
            .mode()
            & UNIX_FILE_MODE_MASK;
        assert_eq!(mode, TARGET_MODE);
    }

    #[cfg(unix)]
    #[test]
    fn prepared_journal_recovers_content_written_before_executable_mode() {
        use std::os::unix::fs::PermissionsExt;

        let (_temp, root, snapshots) = setup();
        write(&root, "run.sh", "target");
        fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        let store = SnapshotStore::new(snapshots.clone());
        store.snapshot_session_start(&root).unwrap();
        write(&root, "run.sh", "source");
        fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o644)).unwrap();
        let source_id = checkpoint(1);
        store.snapshot(&root, source_id).unwrap();
        let source = store.resolve_manifest(&[source_id]).unwrap();
        let target = store.resolve_manifest(&[]).unwrap();
        let canonical = canonical_root(&root).unwrap();
        store
            .prepare_restore(
                &canonical,
                &source.manifest,
                &target.manifest,
                manifest_diff(&source.manifest, &target.manifest),
                RestoreTarget::Snapshot(target.key),
                ConflictPolicy::Abort,
                None,
            )
            .unwrap();

        write(&root, "run.sh", "target");
        drop(store);
        let reopened = SnapshotStore::new(snapshots);
        reopened.recover(&root).unwrap();
        let mode = fs::metadata(root.join("run.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[cfg(unix)]
    #[test]
    fn prepared_mode_only_restore_rejects_an_intervening_chmod() {
        use std::os::unix::fs::PermissionsExt;

        let (_temp, root, snapshots) = setup();
        write(&root, "run.sh", ALPHA);
        fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        let store = SnapshotStore::new(snapshots);
        store.snapshot_session_start(&root).unwrap();
        fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o644)).unwrap();
        let source_id = checkpoint(1);
        store.snapshot(&root, source_id).unwrap();
        let source = store.resolve_manifest(&[source_id]).unwrap();
        let target = store.resolve_manifest(&[]).unwrap();
        let canonical = canonical_root(&root).unwrap();
        store
            .prepare_restore(
                &canonical,
                &source.manifest,
                &target.manifest,
                manifest_diff(&source.manifest, &target.manifest),
                RestoreTarget::Snapshot(target.key),
                ConflictPolicy::Abort,
                None,
            )
            .unwrap();

        fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(
            store.recover(&root),
            Err(SnapshotError::Conflicts(_))
        ));
        let mode = fs::metadata(root.join("run.sh"))
            .unwrap()
            .permissions()
            .mode()
            & UNIX_FILE_MODE_MASK;
        assert_eq!(mode, 0o600);
        assert_eq!(store.journal_state().unwrap(), Some(JournalState::Prepare));
    }

    #[test]
    fn conflict_aborts_without_writing_and_overwrite_is_explicit() {
        let (_temp, root, snapshots) = setup();
        write(&root, "file.txt", ALPHA);
        let store = SnapshotStore::new(snapshots);
        store.snapshot_session_start(&root).unwrap();
        write(&root, "file.txt", BETA);
        let source = checkpoint(1);
        store.snapshot(&root, source).unwrap();
        write(&root, "file.txt", "third-party");

        let error = store.restore(&root, &[source], &[]).unwrap_err();
        let SnapshotError::Conflicts(conflicts) = error else {
            panic!("expected conflicts, got {error:?}");
        };
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].path, "file.txt");
        assert_eq!(
            fs::read_to_string(root.join("file.txt")).unwrap(),
            "third-party"
        );
        assert_eq!(store.journal_state().unwrap(), None);

        store
            .restore_with_policy(&root, &[source], &[], ConflictPolicy::Overwrite)
            .unwrap();
        assert_eq!(fs::read_to_string(root.join("file.txt")).unwrap(), ALPHA);
        store.unrevert(&root).unwrap();
        assert_eq!(
            fs::read_to_string(root.join("file.txt")).unwrap(),
            "third-party"
        );
    }

    #[test]
    fn restore_leaves_paths_outside_source_target_union_untouched() {
        let (_temp, root, snapshots) = setup();
        write(&root, "tracked.txt", ALPHA);
        let store = SnapshotStore::new(snapshots);
        store.snapshot_session_start(&root).unwrap();
        write(&root, "tracked.txt", BETA);
        let source = checkpoint(1);
        store.snapshot(&root, source).unwrap();
        write(&root, "unrelated.txt", "leave me");

        store.restore(&root, &[source], &[]).unwrap();
        assert_eq!(fs::read_to_string(root.join("tracked.txt")).unwrap(), ALPHA);
        assert_eq!(
            fs::read_to_string(root.join("unrelated.txt")).unwrap(),
            "leave me"
        );
    }

    #[test]
    fn chained_unrevert_leaves_unchanged_paths_edited_between_restores() {
        let (_temp, root, snapshots) = setup();
        write(&root, "a.txt", "a0");
        write(&root, "b.txt", "b0");
        write(&root, "c.txt", "c0");
        let store = SnapshotStore::new(snapshots);
        store.snapshot_session_start(&root).unwrap();

        write(&root, "c.txt", "c1");
        let second_source = checkpoint(1);
        store.snapshot(&root, second_source).unwrap();

        write(&root, "c.txt", "c0");
        write(&root, "a.txt", "a1");
        let first_source = checkpoint(2);
        store.snapshot(&root, first_source).unwrap();
        store.restore(&root, &[first_source], &[]).unwrap();

        write(&root, "b.txt", "external");
        write(&root, "c.txt", "c1");
        let report = store.restore(&root, &[second_source], &[]).unwrap();
        assert_eq!(report.paths.len(), 1);
        assert_eq!(report.paths[0].path, "c.txt");

        store.unrevert(&root).unwrap();
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "a1");
        assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "external");
        assert_eq!(fs::read_to_string(root.join("c.txt")).unwrap(), "c1");
    }

    #[test]
    fn nearest_ancestor_checkpoint_is_resolved_before_anchor() {
        let (_temp, root, snapshots) = setup();
        write(&root, "file.txt", ALPHA);
        let store = SnapshotStore::new(snapshots);
        store.snapshot_session_start(&root).unwrap();
        write(&root, "file.txt", BETA);
        let ancestor = checkpoint(1);
        store.snapshot(&root, ancestor).unwrap();

        let resolved = store.resolve_manifest(&[checkpoint(2), ancestor]).unwrap();
        assert_eq!(resolved.key, SnapshotKey::Checkpoint(ancestor));
        assert_eq!(
            resolved.manifest["file.txt"].hash,
            hex_encode(&hash_bytes(BETA.as_bytes()))
        );
    }

    #[test]
    fn copy_ancestry_copies_manifests_and_objects_without_touching_worktree() {
        let (temp, root, snapshots) = setup();
        let child_snapshots = temp.path().join("child-snapshots");
        write(&root, "file.txt", ALPHA);
        let source = SnapshotStore::new(snapshots);
        source.snapshot_session_start(&root).unwrap();
        write(&root, "file.txt", BETA);
        let included = checkpoint(1);
        let omitted = checkpoint(2);
        source.snapshot(&root, included).unwrap();
        write(&root, "file.txt", "current");
        source.snapshot(&root, omitted).unwrap();
        let child = SnapshotStore::new(child_snapshots);

        source.copy_ancestry_to(&child, &[included]).unwrap();

        assert!(child.has_session_start());
        assert!(child.has_checkpoint(included));
        assert!(!child.has_checkpoint(omitted));
        assert_eq!(
            child.load_manifest(included).unwrap(),
            source.load_manifest(included).unwrap()
        );
        assert_eq!(
            fs::read_to_string(root.join("file.txt")).unwrap(),
            "current"
        );
        assert!(child.object_bytes().unwrap() > 0);
    }

    #[test]
    fn copy_ancestry_replaces_a_corrupt_destination_object() {
        let (temp, root, snapshots) = setup();
        let child_snapshots = temp.path().join("child-snapshots");
        write(&root, "file.txt", ALPHA);
        let source = SnapshotStore::new(snapshots);
        source.snapshot_session_start(&root).unwrap();
        let child = SnapshotStore::new(child_snapshots);
        source.copy_ancestry_to(&child, &[]).unwrap();
        let manifest = child.load_session_start_manifest().unwrap();
        let object = child.objects_dir().join(&manifest["file.txt"].hash);
        fs::write(&object, "corrupt").unwrap();

        source.copy_ancestry_to(&child, &[]).unwrap();
        assert_eq!(fs::read(object).unwrap(), ALPHA.as_bytes());
    }

    #[test]
    fn store_rejects_capture_restore_and_recovery_for_another_root() {
        let (temp, root, snapshots) = setup();
        let other_root = temp.path().join("other-repo");
        fs::create_dir(&other_root).unwrap();
        write(&root, "file.txt", ALPHA);
        write(&other_root, "file.txt", "other");
        let store = SnapshotStore::new(snapshots.clone());
        store.snapshot_session_start(&root).unwrap();
        write(&root, "file.txt", BETA);
        let source = checkpoint(1);
        store.snapshot(&root, source).unwrap();
        drop(store);
        let store = SnapshotStore::new(snapshots);

        let expected = canonical_root(&root).unwrap();
        let actual = canonical_root(&other_root).unwrap();
        for error in [
            store.snapshot(&other_root, checkpoint(2)).unwrap_err(),
            store.restore(&other_root, &[source], &[]).unwrap_err(),
            store.recover(&other_root).unwrap_err(),
        ] {
            let SnapshotError::WorkspaceRootMismatch {
                expected: error_expected,
                actual: error_actual,
            } = error
            else {
                panic!("expected workspace root mismatch, got {error:?}");
            };
            assert_eq!(error_expected, expected);
            assert_eq!(error_actual, actual);
        }
        assert_eq!(
            fs::read_to_string(other_root.join("file.txt")).unwrap(),
            "other"
        );
    }

    #[test]
    fn workspace_keys_are_canonical_and_distinguish_roots() {
        let (temp, root, _) = setup();
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

    #[cfg(unix)]
    #[test]
    fn managed_store_rejects_symlinked_snapshot_roots() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let outside = temp.path().join("outside");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(state_dir.path()).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        symlink(&outside, state_dir.path().join(SESSION_SNAPSHOTS_DIR)).unwrap();
        let session_id = CaudraId::generate();
        let store = SnapshotStore::new_managed(
            state_dir.clone(),
            state_dir
                .path()
                .join(SESSION_SNAPSHOTS_DIR)
                .join(session_id.to_string())
                .join("workspace-key"),
        );

        assert!(store.snapshot(&workspace, CaudraId::generate()).is_err());
        assert!(fs::read_dir(&outside).unwrap().next().is_none());
    }

    #[test]
    fn coordinated_restore_keeps_its_journal_until_acknowledged() {
        let (_temp, root, snapshots) = setup();
        write(&root, "file.txt", ALPHA);
        let store = SnapshotStore::new(snapshots);
        store.snapshot_session_start(&root).unwrap();
        write(&root, "file.txt", BETA);
        let source = checkpoint(1);
        store.snapshot(&root, source).unwrap();
        let operation_id = checkpoint(2);

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
        let wrong_operation = checkpoint(3);
        assert!(matches!(
            store.acknowledge_operation(&root, wrong_operation),
            Err(SnapshotError::RestoreOperationMismatch {
                expected,
                actual: Some(actual),
            }) if expected == wrong_operation && actual == operation_id
        ));
        store.acknowledge_operation(&root, operation_id).unwrap();
        assert_eq!(store.journal_state().unwrap(), None);
        assert_eq!(fs::read_to_string(root.join("file.txt")).unwrap(), ALPHA);
    }

    #[test]
    fn another_operation_does_not_apply_a_prepared_journal() {
        let (_temp, root, snapshots) = setup();
        write(&root, "file.txt", ALPHA);
        let store = SnapshotStore::new(snapshots.clone());
        store.snapshot_session_start(&root).unwrap();
        write(&root, "file.txt", BETA);
        let source_id = checkpoint(1);
        store.snapshot(&root, source_id).unwrap();
        let source = store.resolve_manifest(&[source_id]).unwrap();
        let target = store.resolve_manifest(&[]).unwrap();
        let canonical = canonical_root(&root).unwrap();
        let prepared_operation = checkpoint(2);
        store
            .prepare_restore(
                &canonical,
                &source.manifest,
                &target.manifest,
                manifest_diff(&source.manifest, &target.manifest),
                RestoreTarget::Snapshot(target.key),
                ConflictPolicy::Abort,
                Some(prepared_operation),
            )
            .unwrap();

        let other = SnapshotStore::new(snapshots);
        let requested_operation = checkpoint(3);
        assert!(matches!(
            other.restore_transaction_with_policy(
                &root,
                &[source_id],
                &[],
                ConflictPolicy::Abort,
                requested_operation,
            ),
            Err(SnapshotError::RestoreOperationMismatch {
                expected,
                actual: Some(actual),
            }) if expected == requested_operation && actual == prepared_operation
        ));
        assert_eq!(fs::read_to_string(root.join("file.txt")).unwrap(), BETA);
        assert_eq!(other.journal_state().unwrap(), Some(JournalState::Prepare));
    }

    #[test]
    fn report_validation_rejects_another_operation() {
        let expected = checkpoint(1);
        let actual = checkpoint(2);
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
        let (_temp, root, snapshots) = setup();
        write(&root, "a.txt", "a0");
        write(&root, "b.txt", "b0");
        let store = SnapshotStore::new(snapshots.clone());
        store.snapshot_session_start(&root).unwrap();
        write(&root, "a.txt", "a1");
        write(&root, "b.txt", "b1");
        let source_id = checkpoint(1);
        store.snapshot(&root, source_id).unwrap();
        let source = store.resolve_manifest(&[source_id]).unwrap();
        let target = store.resolve_manifest(&[]).unwrap();
        let canonical = canonical_root(&root).unwrap();
        store
            .prepare_restore(
                &canonical,
                &source.manifest,
                &target.manifest,
                manifest_diff(&source.manifest, &target.manifest),
                RestoreTarget::Snapshot(target.key),
                ConflictPolicy::Abort,
                None,
            )
            .unwrap();
        assert_eq!(store.journal_state().unwrap(), Some(JournalState::Prepare));

        store
            .apply_path(&canonical, "a.txt", target.manifest.get("a.txt"))
            .unwrap();
        drop(store);
        let reopened = SnapshotStore::new(snapshots.clone());
        reopened.advance_journal(&canonical, true).unwrap();
        assert_eq!(
            reopened.journal_state().unwrap(),
            Some(JournalState::Applied)
        );
        drop(reopened);

        let reopened = SnapshotStore::new(snapshots.clone());
        reopened.advance_journal(&canonical, true).unwrap();
        assert_eq!(
            reopened.journal_state().unwrap(),
            Some(JournalState::Cleared)
        );
        drop(reopened);

        let reopened = SnapshotStore::new(snapshots);
        let report = reopened.recover(&root).unwrap().unwrap();
        assert!(report.recovered);
        assert_eq!(reopened.journal_state().unwrap(), None);
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "a0");
        assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "b0");
        reopened.unrevert(&root).unwrap();
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "a1");
        assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "b1");
    }

    #[test]
    fn prepared_deletion_completes_before_the_journal_advances() {
        let (_temp, root, snapshots) = setup();
        let store = SnapshotStore::new(snapshots);
        store.snapshot_session_start(&root).unwrap();
        write(&root, "file.txt", ALPHA);
        let source_id = checkpoint(1);
        store.snapshot(&root, source_id).unwrap();
        let source = store.resolve_manifest(&[source_id]).unwrap();
        let target = store.resolve_manifest(&[]).unwrap();
        let canonical = canonical_root(&root).unwrap();
        store
            .prepare_restore(
                &canonical,
                &source.manifest,
                &target.manifest,
                manifest_diff(&source.manifest, &target.manifest),
                RestoreTarget::Snapshot(target.key),
                ConflictPolicy::Abort,
                None,
            )
            .unwrap();

        store.advance_journal(&canonical, false).unwrap();
        assert!(!root.join("file.txt").exists());
        assert_eq!(store.journal_state().unwrap(), Some(JournalState::Applied));
    }

    #[test]
    fn unrevert_recovery_after_mutation_is_consumed_once() {
        let (_temp, root, snapshots) = setup();
        write(&root, "file.txt", ALPHA);
        let store = SnapshotStore::new(snapshots.clone());
        store.snapshot_session_start(&root).unwrap();
        write(&root, "file.txt", BETA);
        let source = checkpoint(1);
        store.snapshot(&root, source).unwrap();
        store.restore(&root, &[source], &[]).unwrap();

        let record: UnrevertRecord = store.read_named(UNREVERT_NAME, "unrevert").unwrap();
        let canonical = canonical_root(&root).unwrap();
        store
            .prepare_restore(
                &canonical,
                &record.after,
                &record.before,
                record.paths,
                RestoreTarget::Unrevert,
                ConflictPolicy::Abort,
                None,
            )
            .unwrap();
        let journal = store.read_journal().unwrap();
        store.apply_prepared(&canonical, &journal).unwrap();
        assert_eq!(fs::read_to_string(root.join("file.txt")).unwrap(), BETA);
        assert_eq!(store.journal_state().unwrap(), Some(JournalState::Prepare));
        drop(store);

        let reopened = SnapshotStore::new(snapshots);
        let report = reopened.unrevert(&root).unwrap();
        assert!(report.recovered);
        assert_eq!(report.target, RestoreTarget::Unrevert);
        assert_eq!(fs::read_to_string(root.join("file.txt")).unwrap(), BETA);
        assert_eq!(reopened.journal_state().unwrap(), None);
        assert!(!reopened.unrevert_path().exists());

        assert!(matches!(
            reopened.unrevert(&root),
            Err(SnapshotError::NotFound(name)) if name == "unrevert"
        ));
        assert_eq!(fs::read_to_string(root.join("file.txt")).unwrap(), BETA);
    }

    #[test]
    fn recovery_rejects_changes_after_prepare() {
        let (_temp, root, snapshots) = setup();
        write(&root, "file.txt", ALPHA);
        let store = SnapshotStore::new(snapshots.clone());
        store.snapshot_session_start(&root).unwrap();
        write(&root, "file.txt", BETA);
        let source_id = checkpoint(1);
        store.snapshot(&root, source_id).unwrap();
        let source = store.resolve_manifest(&[source_id]).unwrap();
        let target = store.resolve_manifest(&[]).unwrap();
        let canonical = canonical_root(&root).unwrap();
        store
            .prepare_restore(
                &canonical,
                &source.manifest,
                &target.manifest,
                manifest_diff(&source.manifest, &target.manifest),
                RestoreTarget::Snapshot(target.key),
                ConflictPolicy::Abort,
                None,
            )
            .unwrap();
        write(&root, "file.txt", "external");
        drop(store);

        let reopened = SnapshotStore::new(snapshots);
        assert!(matches!(
            reopened.recover(&root),
            Err(SnapshotError::Conflicts(_))
        ));
        assert_eq!(
            fs::read_to_string(root.join("file.txt")).unwrap(),
            "external"
        );
        assert_eq!(
            reopened.journal_state().unwrap(),
            Some(JournalState::Prepare)
        );
    }

    #[test]
    fn abort_restore_preflights_all_objects_before_writing() {
        let (_temp, root, snapshots) = setup();
        write(&root, "a.txt", "a0");
        write(&root, "b.txt", "b0");
        let store = SnapshotStore::new(snapshots);
        store.snapshot_session_start(&root).unwrap();
        write(&root, "a.txt", "a1");
        write(&root, "b.txt", "b1");
        let source_id = checkpoint(1);
        store.snapshot(&root, source_id).unwrap();
        let source = store.resolve_manifest(&[source_id]).unwrap();
        let target = store.resolve_manifest(&[]).unwrap();
        let canonical = canonical_root(&root).unwrap();
        store
            .prepare_restore(
                &canonical,
                &source.manifest,
                &target.manifest,
                manifest_diff(&source.manifest, &target.manifest),
                RestoreTarget::Snapshot(target.key),
                ConflictPolicy::Abort,
                None,
            )
            .unwrap();

        let missing_hash = target.manifest["b.txt"].hash.clone();
        let missing_path = store.objects_dir().join(&missing_hash);
        let missing_bytes = fs::read(&missing_path).unwrap();
        fs::remove_file(&missing_path).unwrap();

        assert!(matches!(
            store.recover(&root),
            Err(SnapshotError::MissingObject(hash)) if hash == missing_hash
        ));
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "a1");
        assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "b1");
        assert_eq!(store.journal_state().unwrap(), Some(JournalState::Prepare));

        fs::write(missing_path, missing_bytes).unwrap();
        store.recover(&root).unwrap().unwrap();
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "a0");
        assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "b0");
        assert_eq!(store.journal_state().unwrap(), None);
    }

    #[test]
    fn cap_drops_old_checkpoint_objects_but_keeps_anchor_and_newest() {
        let (_temp, root, snapshots) = setup();
        write(&root, "anchor.txt", "aaaa");
        let store = SnapshotStore::with_cap(snapshots, 8);
        store.snapshot_session_start(&root).unwrap();

        write(&root, "old.txt", "bbbb");
        let old = checkpoint(1);
        store.snapshot(&root, old).unwrap();
        fs::remove_file(root.join("old.txt")).unwrap();
        write(&root, "new.txt", "cccc");
        let new = checkpoint(2);
        store.snapshot(&root, new).unwrap();

        assert!(store.has_session_start());
        assert!(matches!(
            store.load_manifest(old),
            Err(SnapshotError::NotFound(_))
        ));
        assert!(store.load_manifest(new).is_ok());
        assert!(store.object_bytes().unwrap() <= 8);
        assert_eq!(fs::read_to_string(root.join("anchor.txt")).unwrap(), "aaaa");
    }

    #[test]
    fn anchor_survives_when_it_alone_exceeds_cap() {
        let (_temp, root, snapshots) = setup();
        write(&root, "large.txt", "anchor-is-larger-than-cap");
        let store = SnapshotStore::with_cap(snapshots, 1);
        store.snapshot_session_start(&root).unwrap();

        assert!(store.has_session_start());
        assert!(store.object_bytes().unwrap() > 1);
        assert_eq!(store.load_session_start_manifest().unwrap().len(), 1);
    }

    #[test]
    fn cap_is_a_target_when_anchor_and_new_checkpoint_exceed_it() {
        let (_temp, root, snapshots) = setup();
        write(&root, "file.txt", ALPHA);
        let store = SnapshotStore::with_cap(snapshots, 1);
        store.snapshot_session_start(&root).unwrap();
        write(&root, "file.txt", BETA);
        let newest = checkpoint(1);
        store.snapshot(&root, newest).unwrap();

        assert!(store.has_session_start());
        assert!(store.has_checkpoint(newest));
        assert!(store.object_bytes().unwrap() > 1);
    }

    #[test]
    fn snapshot_recaptures_and_replaces_an_existing_checkpoint() {
        let (_temp, root, snapshots) = setup();
        write(&root, "file.txt", ALPHA);
        let store = SnapshotStore::with_cap(snapshots, BETA.len() as u64);
        let checkpoint = checkpoint(1);
        let first = store.snapshot(&root, checkpoint).unwrap();
        let first_object = store.objects_dir().join(&first["file.txt"].hash);
        assert!(first_object.exists());

        write(&root, "file.txt", BETA);
        let second = store.snapshot(&root, checkpoint).unwrap();

        assert_ne!(first["file.txt"].hash, second["file.txt"].hash);
        assert_eq!(store.load_manifest(checkpoint).unwrap(), second);
        assert!(!first_object.exists());
        assert_eq!(store.object_bytes().unwrap(), BETA.len() as u64);
    }

    #[test]
    fn garbage_is_swept_only_under_cap_pressure() {
        let (_temp, root, snapshots) = setup();
        write(&root, "file.txt", ALPHA);
        let roomy = SnapshotStore::with_cap(snapshots.clone(), u64::MAX);
        let checkpoint = checkpoint(1);
        let first = roomy.snapshot(&root, checkpoint).unwrap();
        let orphan = roomy.objects_dir().join(&first["file.txt"].hash);

        write(&root, "file.txt", BETA);
        roomy.snapshot(&root, checkpoint).unwrap();
        assert!(orphan.exists());

        let tight = SnapshotStore::with_cap(snapshots, BETA.len() as u64);
        tight.enforce_cap().unwrap();
        assert!(!orphan.exists());
    }

    #[test]
    fn cap_enforcement_keeps_unrevert_usable() {
        let (_temp, root, snapshots) = setup();
        write(&root, "file.txt", ALPHA);
        let store = SnapshotStore::with_cap(snapshots, 1);
        store.snapshot_session_start(&root).unwrap();
        write(&root, "file.txt", BETA);
        let source = checkpoint(1);
        store.snapshot(&root, source).unwrap();
        store.restore(&root, &[source], &[]).unwrap();

        let current = checkpoint(2);
        store.snapshot(&root, current).unwrap();
        store.enforce_cap().unwrap();
        assert!(store.has_session_start());
        assert!(store.has_checkpoint(current));
        assert!(store.unrevert_path().exists());
        assert!(store.object_bytes().unwrap() > 1);

        store.unrevert(&root).unwrap();
        assert_eq!(fs::read_to_string(root.join("file.txt")).unwrap(), BETA);
    }

    #[test]
    fn discarding_the_unrevert_record_tolerates_its_absence() {
        let (_temp, root, snapshots) = setup();
        write(&root, "file.txt", ALPHA);
        let store = SnapshotStore::new(snapshots);
        let source = checkpoint(1);
        store.snapshot_session_start(&root).unwrap();
        write(&root, "file.txt", BETA);
        store.snapshot(&root, source).unwrap();
        store.restore(&root, &[source], &[]).unwrap();
        assert!(store.unrevert_path().exists());

        store.discard_unrevert().unwrap();
        assert!(!store.unrevert_path().exists());
        store.discard_unrevert().unwrap();
    }

    #[test]
    fn capture_hashes_same_size_same_mtime_changes() {
        let (_temp, root, snapshots) = setup();
        write(&root, "a.txt", ALPHA);
        let hasher = Arc::new(CountingHasher::new());
        let store = SnapshotStore::with_hasher(snapshots, u64::MAX, hasher.clone());

        let first = store.snapshot(&root, checkpoint(1)).unwrap();
        assert_eq!(hasher.count(), 1);
        let path = root.join("a.txt");
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        write(&root, "a.txt", "bravo");
        fs::File::open(&path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(modified))
            .unwrap();
        let metadata = fs::metadata(&path).unwrap();
        assert_eq!(metadata.len(), ALPHA.len() as u64);
        assert_eq!(metadata.modified().unwrap(), modified);

        let second = store.snapshot(&root, checkpoint(2)).unwrap();
        assert_eq!(hasher.count(), 2);
        assert_ne!(first["a.txt"].hash, second["a.txt"].hash);
        assert_eq!(second["a.txt"].hash, hex_encode(&hash_bytes(b"bravo")));
    }

    /// Exit takes this path, so a lock held by an unrelated caudra has to come
    /// back as "did not run" inside the budget rather than as a stalled exit.
    #[test]
    fn capture_head_within_gives_up_on_a_held_artifact_lock() {
        const BUSY_MSG: &str = "a held artifact lock must report no capture";
        const FREE_MSG: &str = "a free artifact lock must capture";
        const BUDGET: Duration = Duration::from_millis(120);
        let (_temp, root, _unused) = setup();
        let state = TempDir::new().unwrap();
        let state_dir = StateDir::from_path(state.path().to_path_buf());
        write(&root, "kept.txt", ALPHA);
        let store =
            SnapshotStore::new_managed(state_dir.clone(), state.path().join(SESSION_SNAPSHOTS_DIR));

        let held = lock_session_artifacts(&state_dir).unwrap();
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
    }

    const NESTED_REPO_MSG: &str = "a nested repository belongs to itself, not to this worktree";

    /// The parent's own files still have to be captured; only the nested
    /// repository is out of scope. A repository nested in a plain directory is
    /// just as much its own worktree as one nested in another repository.
    #[test_case(true, true   ; "unregistered_clone_is_a_boundary")]
    #[test_case(true, false  ; "submodule_gitlink_is_a_boundary")]
    #[test_case(false, true  ; "a_boundary_without_an_enclosing_repository")]
    fn a_nested_repository_is_not_part_of_this_worktree(
        parent_is_repository: bool,
        nested_git_is_dir: bool,
    ) {
        let (_temp, root, snapshots) = setup();
        if parent_is_repository {
            fs::create_dir(root.join(".git")).unwrap();
        }
        write(&root, "kept.txt", ALPHA);
        fs::create_dir_all(root.join("training-data")).unwrap();
        if nested_git_is_dir {
            fs::create_dir(root.join("training-data/.git")).unwrap();
        } else {
            write(&root, "training-data/.git", "gitdir: ../.git/modules/td");
        }
        write(&root, "training-data/huge.bin", BETA);

        let store = SnapshotStore::new(snapshots);
        let manifest = store.snapshot_session_start(&root).unwrap();

        assert!(manifest.contains_key("kept.txt"), "{NESTED_REPO_MSG}");
        assert!(
            !manifest.contains_key("training-data/huge.bin"),
            "{NESTED_REPO_MSG}"
        );
    }

    #[test]
    fn git_walk_honors_ignore_rules() {
        let (_temp, root, snapshots) = setup();
        fs::create_dir(root.join(".git")).unwrap();
        write(&root, ".gitignore", "*.ignored\n");
        write(&root, "kept.txt", ALPHA);
        write(&root, "secret.ignored", BETA);
        let store = SnapshotStore::new(snapshots);

        let manifest = store.snapshot_session_start(&root).unwrap();
        assert!(manifest.contains_key("kept.txt"));
        assert!(!manifest.contains_key("secret.ignored"));
        assert!(!manifest.keys().any(|path| path.starts_with(".git/")));
    }

    const NON_GIT_IGNORE_MSG: &str =
        "an ignore file states intent with or without a repository around it";

    /// Without this, a directory that is not a repository had no filtering at
    /// all, which is how a session rooted at a home directory came to hash it.
    #[test_case(".gitignore" ; "gitignore")]
    #[test_case(".ignore"    ; "ignore")]
    fn a_walk_outside_a_repository_honors_ignore_files(ignore_file: &str) {
        let (_temp, root, snapshots) = setup();
        write(&root, ignore_file, "*.ignored\nheavy/\n");
        write(&root, "kept.txt", ALPHA);
        write(&root, "secret.ignored", BETA);
        write(&root, "heavy/blob.bin", BETA);
        let store = SnapshotStore::new(snapshots);

        let manifest = store.snapshot_session_start(&root).unwrap();
        assert!(manifest.contains_key("kept.txt"), "{NON_GIT_IGNORE_MSG}");
        assert!(
            !manifest.contains_key("secret.ignored"),
            "{NON_GIT_IGNORE_MSG}"
        );
        assert!(
            !manifest.contains_key("heavy/blob.bin"),
            "{NON_GIT_IGNORE_MSG}"
        );
    }

    const REFUSAL_MSG: &str = "a tree over the budget is refused, not captured";
    const UNHASHED_MSG: &str = "a refusal must cost metadata only, never a hash";

    /// The point of deciding from the walk is that the expensive half never
    /// runs, so the hasher is the assertion: a refusal that had already read
    /// and hashed the tree would be no cheaper than capturing it.
    #[test_case(
        SnapshotLimits { max_files: 1, ..SnapshotLimits::default() },
        LIMIT_MAX_FILES,
        1
        ; "over_the_file_ceiling"
    )]
    #[test_case(
        SnapshotLimits { max_bytes: 4, ..SnapshotLimits::default() },
        LIMIT_MAX_BYTES,
        4
        ; "over_the_byte_ceiling"
    )]
    fn a_tree_over_the_budget_is_refused_before_anything_is_hashed(
        limits: SnapshotLimits,
        expected_limit_name: &str,
        expected_limit: u64,
    ) {
        let (_temp, root, snapshots) = setup();
        write(&root, "one.txt", ALPHA);
        write(&root, "two.txt", BETA);
        let hasher = Arc::new(CountingHasher::new());
        let store =
            SnapshotStore::with_hasher(snapshots, u64::MAX, hasher.clone()).with_limits(limits);

        let error = store.snapshot_session_start(&root).unwrap_err();
        assert!(error.is_workspace_refusal(), "{REFUSAL_MSG}: {error}");
        let SnapshotError::WorkspaceTooLarge {
            exceeded, limit, ..
        } = error
        else {
            panic!("{REFUSAL_MSG}: {error}");
        };
        assert_eq!(exceeded, expected_limit_name, "{REFUSAL_MSG}");
        assert_eq!(limit, expected_limit, "{REFUSAL_MSG}");
        assert_eq!(hasher.count(), 0, "{UNHASHED_MSG}");
        assert!(!store.has_session_start(), "{REFUSAL_MSG}");
    }

    const OVERSIZED_MSG: &str =
        "an oversized file is left out of the snapshot and left alone on disk";

    #[test]
    fn an_oversized_file_is_skipped_and_never_restored_over() {
        let (_temp, root, snapshots) = setup();
        let big = vec![b'x'; 64];
        write(&root, "small.txt", ALPHA);
        write(&root, "big.bin", &big);
        let store = SnapshotStore::new(snapshots).with_limits(SnapshotLimits {
            max_file_bytes: 32,
            ..SnapshotLimits::default()
        });

        let manifest = store.snapshot_session_start(&root).unwrap();
        assert!(manifest.contains_key("small.txt"), "{OVERSIZED_MSG}");
        assert!(!manifest.contains_key("big.bin"), "{OVERSIZED_MSG}");

        write(&root, "small.txt", "changed");
        let after = vec![b'y'; 64];
        write(&root, "big.bin", &after);
        let head = checkpoint(1);
        store.snapshot(&root, head).unwrap();

        store.restore(&root, &[head], &[]).unwrap();
        assert_eq!(
            fs::read_to_string(root.join("small.txt")).unwrap(),
            ALPHA,
            "{OVERSIZED_MSG}"
        );
        assert_eq!(
            fs::read(root.join("big.bin")).unwrap(),
            after,
            "{OVERSIZED_MSG}"
        );
    }

    const UNSUPPORTED_ROOT_MSG: &str = "a root no project lives at is refused without a walk";

    #[test]
    fn the_filesystem_root_and_the_home_directory_are_refused() {
        assert_eq!(
            unsupported_root(Path::new("/")),
            Some(ROOT_IS_FILESYSTEM_ROOT),
            "{UNSUPPORTED_ROOT_MSG}"
        );
        let home = caudra_storage::paths::home().expect("a home directory");
        assert_eq!(
            unsupported_root(&home),
            Some(ROOT_IS_HOME),
            "{UNSUPPORTED_ROOT_MSG}"
        );
        assert_eq!(
            unsupported_root(&home.join("projects/app")),
            None,
            "{UNSUPPORTED_ROOT_MSG}"
        );
    }

    #[test]
    fn capturing_the_home_directory_is_refused() {
        let temp = TempDir::new().unwrap();
        let home = caudra_storage::paths::home().expect("a home directory");
        let store = SnapshotStore::new(temp.path().join("snapshots"));

        let error = store.snapshot_session_start(&home).unwrap_err();
        assert!(
            matches!(
                error,
                SnapshotError::WorkspaceUnsupported {
                    reason: ROOT_IS_HOME,
                    ..
                }
            ),
            "{UNSUPPORTED_ROOT_MSG}: {error}"
        );
        assert!(error.is_workspace_refusal(), "{UNSUPPORTED_ROOT_MSG}");
    }
}
