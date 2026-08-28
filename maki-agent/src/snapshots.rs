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

use ignore::WalkBuilder;
use maki_storage::id::MakiId;
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
pub const SESSION_SNAPSHOTS_DIR: &str = "session-snapshots";

type RelPath = String;

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
    Checkpoint(MakiId),
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
    pub operation_id: Option<MakiId>,
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
    RestoreOperationPending(MakiId),
    #[error("restore journal belongs to operation {actual:?}, not {expected}")]
    RestoreOperationMismatch {
        expected: MakiId,
        actual: Option<MakiId>,
    },
    #[error("restore operation {0} has not finished applying")]
    RestoreOperationNotApplied(MakiId),
    #[error("snapshot store lock is poisoned")]
    LockPoisoned,
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
    operation_id: Option<MakiId>,
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
    hasher: Arc<dyn ContentHasher>,
}

impl SnapshotStore {
    pub fn new(snapshots_dir: PathBuf) -> Self {
        Self::with_cap(snapshots_dir, DEFAULT_SNAPSHOT_CAP_BYTES)
    }

    pub fn with_cap(snapshots_dir: PathBuf, cap_bytes: u64) -> Self {
        Self::with_hasher(snapshots_dir, cap_bytes, Arc::new(Sha256Hasher))
    }

    fn with_hasher(snapshots_dir: PathBuf, cap_bytes: u64, hasher: Arc<dyn ContentHasher>) -> Self {
        Self {
            dir: snapshots_dir,
            cap_bytes,
            hasher,
        }
    }

    pub fn snapshot_session_start(&self, cwd: &Path) -> Result<Manifest, SnapshotError> {
        let _guard = lock_store()?;
        let root = self.bind_root(cwd)?;
        if self.has_session_start() {
            return self.load_session_start_manifest();
        }
        let path = self.manifest_path(SnapshotKey::SessionStart);
        self.capture_to(&root, &path, None, "session_start")
    }

    pub fn snapshot(&self, cwd: &Path, checkpoint: MakiId) -> Result<Manifest, SnapshotError> {
        let _guard = lock_store()?;
        let root = self.bind_root(cwd)?;
        let path = self.manifest_path(SnapshotKey::Checkpoint(checkpoint));
        self.capture_to(&root, &path, Some(checkpoint), "checkpoint")
    }

    /// Captures `root`, writes the manifest and enforces the store cap,
    /// timing each part. Capture cost splits between hashing every file and
    /// durably writing the ones whose content is new, and the two want very
    /// different fixes, so they are reported apart.
    fn capture_to(
        &self,
        root: &Path,
        path: &Path,
        preserve: Option<MakiId>,
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

    pub fn load_manifest(&self, checkpoint: MakiId) -> Result<Manifest, SnapshotError> {
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
        checkpoint_and_ancestors: &[MakiId],
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
        source_checkpoint_and_ancestors: &[MakiId],
        target_checkpoint_and_ancestors: &[MakiId],
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
        source_checkpoint_and_ancestors: &[MakiId],
        target_checkpoint_and_ancestors: &[MakiId],
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
        source_checkpoint_and_ancestors: &[MakiId],
        target_checkpoint_and_ancestors: &[MakiId],
        policy: ConflictPolicy,
        operation_id: MakiId,
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
        source_checkpoint_and_ancestors: &[MakiId],
        target_checkpoint_and_ancestors: &[MakiId],
        policy: ConflictPolicy,
        operation_id: Option<MakiId>,
    ) -> Result<RestoreReport, SnapshotError> {
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
        operation_id: MakiId,
    ) -> Result<RestoreReport, SnapshotError> {
        self.unrevert_impl(cwd, policy, Some(operation_id))
    }

    fn unrevert_impl(
        &self,
        cwd: &Path,
        policy: ConflictPolicy,
        operation_id: Option<MakiId>,
    ) -> Result<RestoreReport, SnapshotError> {
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

    pub fn journal_operation_id(&self) -> Result<Option<MakiId>, SnapshotError> {
        match self.read_journal() {
            Ok(journal) => Ok(journal.operation_id),
            Err(SnapshotError::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn acknowledge_operation(
        &self,
        cwd: &Path,
        operation_id: MakiId,
    ) -> Result<(), SnapshotError> {
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

    pub fn has_checkpoint(&self, checkpoint: MakiId) -> bool {
        self.manifest_path(SnapshotKey::Checkpoint(checkpoint))
            .exists()
    }

    pub fn copy_ancestry_to(
        &self,
        destination: &SnapshotStore,
        checkpoints: &[MakiId],
    ) -> Result<(), SnapshotError> {
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
        let _guard = lock_store()?;
        self.ensure_dirs()?;
        self.enforce_cap_preserving(None)
    }

    fn ensure_dirs(&self) -> Result<(), SnapshotError> {
        fs::create_dir_all(&self.dir)?;
        fs::create_dir_all(self.objects_dir())?;
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
        let files = self.walk_working_tree(root)?;
        let mut stats = CaptureStats {
            walk: start.elapsed(),
            files: files.len() as u64,
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

    fn walk_working_tree(&self, root: &Path) -> Result<Vec<WalkedFile>, SnapshotError> {
        let git_worktree = root.ancestors().any(|path| path.join(".git").exists());
        let excluded = fs::canonicalize(&self.dir)
            .or_else(|_| std::path::absolute(&self.dir))
            .unwrap_or_else(|_| self.dir.clone());
        let mut builder = WalkBuilder::new(root);
        builder.hidden(false);
        if git_worktree {
            builder
                .ignore(true)
                .git_ignore(true)
                .git_global(true)
                .git_exclude(true)
                .require_git(true);
        } else {
            builder.standard_filters(false);
        }
        builder.filter_entry(move |entry| {
            entry.depth() == 0
                || (entry.file_name() != OsStr::new(".git") && !entry.path().starts_with(&excluded))
        });

        let mut files = Vec::new();
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
            files.push(WalkedFile {
                relative: relative.to_owned(),
                absolute: entry.path().to_path_buf(),
                metadata,
            });
        }
        Ok(files)
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
        operation_id: Option<MakiId>,
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
        operation_id: Option<MakiId>,
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
                maki_storage::atomic_write(&path, &bytes)
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
        maki_storage::atomic_write_deferred(&path, bytes)
            .map_err(|error| io::Error::other(error.to_string()))?;
        Ok(true)
    }

    /// Makes every object written since the last call durable. One flush for
    /// the whole batch, ordered before the manifest that references them.
    fn sync_objects(&self) {
        maki_storage::sync_dir(&self.objects_dir());
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
    fn enforce_cap_preserving(&self, preserve: Option<MakiId>) -> Result<(), SnapshotError> {
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
    ) -> Result<Vec<(MakiId, PathBuf, SystemTime)>, SnapshotError> {
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
                .and_then(|name| name.parse::<MakiId>().ok())
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
            if name != SESSION_START_NAME && name.parse::<MakiId>().is_err() {
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
    operation_id: Option<MakiId>,
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
    maki_storage::atomic_write(path, &bytes)
        .map_err(|error| io::Error::other(error.to_string()).into())
}

fn lock_store() -> Result<MutexGuard<'static, ()>, SnapshotError> {
    STORE_LOCK.lock().map_err(|_| SnapshotError::LockPoisoned)
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

    fn checkpoint(sequence: u32) -> MakiId {
        format!("00000000-0000-7000-8000-{sequence:012x}")
            .parse()
            .unwrap()
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

    #[test]
    fn non_git_walk_does_not_treat_gitignore_as_an_exclusion() {
        let (_temp, root, snapshots) = setup();
        write(&root, ".gitignore", "*.ignored\n");
        write(&root, "included.ignored", BETA);
        let store = SnapshotStore::new(snapshots);

        let manifest = store.snapshot_session_start(&root).unwrap();
        assert!(manifest.contains_key("included.ignored"));
    }
}
