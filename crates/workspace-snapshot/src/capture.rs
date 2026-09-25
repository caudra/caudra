//! Capture: one descriptor-relative walk of the scope. Each regular file is read until the stamps
//! taken around the read agree, its content is stored once by digest, and every entry left out is
//! counted and recorded so a restore never touches it.

use std::{
    fs::{File, Metadata},
    io::{Read, Seek, SeekFrom},
    mem,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use serde::Serialize;
use tokio_util::sync::CancellationToken;
use workcell_host_contract::{
    ContractVersion, DisplayText, MAX_SNAPSHOT_CAPTURE_ENTRIES, MAX_SNAPSHOT_CAPTURE_PATH_BYTES,
    MAX_SNAPSHOT_COUNT, MAX_SNAPSHOT_FILE_BYTES, MAX_SNAPSHOT_FILES, MAX_SNAPSHOT_SKIPPED_SAMPLES,
    MAX_SNAPSHOT_STORAGE_BYTES, MAX_SNAPSHOT_TOTAL_BYTES, SnapshotCaptureLimits,
    SnapshotCaptureResponse, SnapshotLimit, SnapshotSkipReason, SnapshotSkipped,
    SnapshotSkippedEntry,
};
use workcell_mcp_files::{
    SnapshotTreeFile, SnapshotTreeLimits, SnapshotTreeLink, SnapshotTreeNode, SnapshotTreeStamp,
    WorkspaceSnapshotScope,
};

use crate::{
    CHECKPOINT_VERSION, SnapshotError, SnapshotInner, StoredCheckpoint, check_cancelled,
    digest_bytes, limit_error,
    manifest::{
        MANIFEST_VERSION, MAX_MANIFEST_BYTES, Manifest, ManifestContent, PERMISSION_BITS,
        PrunedEntry, SYMLINK_MODE, StoredEntry, StoredEntryKind,
    },
    quota_error,
    store::{BLOBS, BlobWrite, CHECKPOINTS, MANIFESTS, StagedBlob, Store, digest_stream},
    tree_error, unix_ms,
};

const STABLE_READ_ATTEMPTS: usize = 3;
const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);
pub(crate) const MAX_BLOB_BATCH_FILES: usize = 64;
const MAX_BLOB_BATCH_BYTES: u64 = 4 * 1_024 * 1_024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SnapshotCapturePhase {
    Queued,
    Scanning,
    Persisting,
    Publishing,
    Rollback,
    Finished,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotCaptureProgress {
    pub phase: SnapshotCapturePhase,
    pub entries: u64,
    pub files: u64,
    pub bytes: u64,
    pub elapsed_ms: u64,
}

pub trait SnapshotCaptureProgressSink: Send + Sync {
    fn publish(&self, progress: SnapshotCaptureProgress);
}

pub(crate) struct CaptureProgress {
    sink: Option<Arc<dyn SnapshotCaptureProgressSink>>,
    started: Instant,
    emitted: Instant,
    progress: SnapshotCaptureProgress,
}

impl CaptureProgress {
    pub(crate) fn new(sink: Option<Arc<dyn SnapshotCaptureProgressSink>>) -> Self {
        let now = Instant::now();
        let mut progress = Self {
            sink,
            started: now,
            emitted: now,
            progress: SnapshotCaptureProgress {
                phase: SnapshotCapturePhase::Queued,
                entries: 0,
                files: 0,
                bytes: 0,
                elapsed_ms: 0,
            },
        };
        progress.emit(now);
        progress
    }

    pub(crate) fn phase(&mut self, phase: SnapshotCapturePhase) {
        self.progress.phase = phase;
        self.emit(Instant::now());
    }

    fn entry(&mut self, files: usize, bytes: u64, now: Instant) {
        self.progress.entries += 1;
        self.progress.files = u64::try_from(files).unwrap_or(u64::MAX);
        self.progress.bytes = bytes;
        if now.duration_since(self.emitted) >= PROGRESS_INTERVAL {
            self.emit(now);
        }
    }

    fn emit(&mut self, now: Instant) {
        self.emitted = now;
        self.progress.elapsed_ms =
            u64::try_from(now.duration_since(self.started).as_millis()).unwrap_or(u64::MAX);
        if let Some(sink) = &self.sink {
            sink.publish(self.progress.clone());
        }
    }
}

pub(crate) enum Stability {
    Stable {
        digest: String,
        size: u64,
        mode: u32,
        stamp: SnapshotTreeStamp,
    },
    Oversized,
    /// Every read overlapped a change to the file.
    Unstable,
}

struct Capture<'a> {
    store: &'a Store,
    limits: &'a SnapshotCaptureLimits,
    token: &'a CancellationToken,
    usage: u64,
    stored_blobs: Vec<PathBuf>,
    pending_blobs: Vec<StagedBlob>,
    pending_bytes: u64,
    stored_manifest: Option<PathBuf>,
    stored_checkpoint: Option<PathBuf>,
    entries: Vec<StoredEntry>,
    pruned: Vec<PrunedEntry>,
    skipped: SnapshotSkipped,
    total_bytes: u64,
    manifest_count: usize,
    progress: &'a mut CaptureProgress,
}

impl SnapshotInner {
    /// Captures `scope` as `checkpoint_id`. An existing checkpoint is returned as it was captured.
    pub(crate) fn capture(
        &self,
        checkpoint_id: &str,
        scope: &WorkspaceSnapshotScope,
        limits: &SnapshotCaptureLimits,
        token: &CancellationToken,
        progress: &mut CaptureProgress,
    ) -> Result<SnapshotCaptureResponse, SnapshotError> {
        check_cancelled(token)?;
        progress.phase(SnapshotCapturePhase::Scanning);
        if let Some(manifest) = self.load_checkpoint(checkpoint_id)? {
            if manifest.content.entries.len() > limits.max_files as usize {
                return Err(limit_error(SnapshotLimit::Files, limits.max_files));
            }
            let bytes: u64 = manifest
                .content
                .entries
                .iter()
                .map(|entry| entry.size_bytes)
                .sum();
            if bytes > limits.max_total_bytes {
                return Err(limit_error(
                    SnapshotLimit::TotalBytes,
                    limits.max_total_bytes,
                ));
            }
            if manifest
                .content
                .entries
                .iter()
                .any(|entry| entry.size_bytes > limits.max_file_bytes)
            {
                return Err(SnapshotError::InvalidRequest);
            }
            return capture_response(&manifest, checkpoint_id, scope.path(), true);
        }
        let inventory = self.store.capture_inventory(token)?;
        if inventory.checkpoints >= MAX_SNAPSHOT_COUNT {
            return Err(quota_error(SnapshotLimit::Checkpoints, MAX_SNAPSHOT_COUNT));
        }
        let mut capture = Capture {
            store: &self.store,
            limits,
            token,
            usage: inventory.bytes,
            stored_blobs: Vec::new(),
            pending_blobs: Vec::new(),
            pending_bytes: 0,
            stored_manifest: None,
            stored_checkpoint: None,
            entries: Vec::new(),
            pruned: Vec::new(),
            skipped: SnapshotSkipped::default(),
            total_bytes: 0,
            manifest_count: inventory.manifests,
            progress,
        };
        let captured = capture
            .walk(self, scope)
            .and_then(|content| capture.publish(self, content, checkpoint_id));
        match captured {
            Ok(manifest) => capture_response(&manifest, checkpoint_id, scope.path(), false),
            Err(error) => {
                capture.progress.phase(SnapshotCapturePhase::Rollback);
                capture.discard()?;
                Err(error)
            }
        }
    }
}

impl Capture<'_> {
    fn walk(
        &mut self,
        inner: &SnapshotInner,
        scope: &WorkspaceSnapshotScope,
    ) -> Result<ManifestContent, SnapshotError> {
        self.progress.phase(SnapshotCapturePhase::Persisting);
        check_cancelled(self.token)?;
        let tree_limits = SnapshotTreeLimits {
            max_entries: MAX_SNAPSHOT_CAPTURE_ENTRIES,
            max_path_bytes: usize::try_from(MAX_SNAPSHOT_CAPTURE_PATH_BYTES).unwrap_or(usize::MAX),
        };
        let walk = inner
            .workspace
            .walk_tree_bound(
                scope,
                inner.exclusions.clone(),
                tree_limits,
                self.token.clone(),
            )
            .map_err(tree_error)?;
        for entry in walk {
            let entry = entry.map_err(tree_error)?;
            match entry.node {
                SnapshotTreeNode::File(file) => self.file(entry.path, file)?,
                SnapshotTreeNode::Symlink(link) => self.symlink(entry.path, link)?,
                SnapshotTreeNode::Skipped(reason) => self.skip(entry.path, reason),
            }
            self.progress
                .entry(self.entries.len(), self.total_bytes, Instant::now());
        }
        self.flush_blobs()?;
        self.entries
            .sort_unstable_by(|left, right| left.path.cmp(&right.path));
        self.pruned
            .sort_unstable_by(|left, right| left.path.cmp(&right.path));
        Ok(ManifestContent {
            version: MANIFEST_VERSION.to_owned(),
            scope: scope.path().to_owned(),
            entries: mem::take(&mut self.entries),
            pruned: mem::take(&mut self.pruned),
            skipped: mem::take(&mut self.skipped),
            exclusions: inner.exclusions.clone(),
        })
    }

    fn file(&mut self, path: String, mut file: SnapshotTreeFile) -> Result<(), SnapshotError> {
        if file.metadata.len() > self.limits.max_file_bytes {
            self.skip(path, SnapshotSkipReason::Oversized);
            return Ok(());
        }
        let (digest, size, mode) =
            match read_stable(&mut file.file, self.limits.max_file_bytes, self.token)? {
                Stability::Stable {
                    digest, size, mode, ..
                } => (digest, size, mode),
                Stability::Oversized => {
                    self.skip(path, SnapshotSkipReason::Oversized);
                    return Ok(());
                }
                Stability::Unstable => {
                    self.skip(path, SnapshotSkipReason::Unstable);
                    return Ok(());
                }
            };
        self.admit(size)?;
        file.file
            .seek(SeekFrom::Start(0))
            .map_err(|_| SnapshotError::OperationFailed)?;
        if !self.store_blob(&digest, size, &mut file.file)? {
            self.skip(path, SnapshotSkipReason::Unstable);
            return Ok(());
        }
        self.record(StoredEntry {
            path,
            kind: StoredEntryKind::File,
            digest,
            mode,
            size_bytes: size,
        });
        Ok(())
    }

    fn symlink(&mut self, path: String, link: SnapshotTreeLink) -> Result<(), SnapshotError> {
        let size = u64::try_from(link.target.len()).unwrap_or(u64::MAX);
        if size > self.limits.max_file_bytes {
            self.skip(path, SnapshotSkipReason::Oversized);
            return Ok(());
        }
        let digest = digest_bytes(&link.target);
        self.admit(size)?;
        if !self.store_blob(&digest, size, &mut link.target.as_slice())? {
            return Err(SnapshotError::OperationFailed);
        }
        self.record(StoredEntry {
            path,
            kind: StoredEntryKind::Symlink,
            digest,
            mode: SYMLINK_MODE,
            size_bytes: size,
        });
        Ok(())
    }

    fn skip(&mut self, path: String, reason: SnapshotSkipReason) {
        let skipped = &mut self.skipped;
        let count = match reason {
            SnapshotSkipReason::NestedRepository => &mut skipped.nested_repositories,
            SnapshotSkipReason::Mount => &mut skipped.mounts,
            SnapshotSkipReason::Special => &mut skipped.special_files,
            SnapshotSkipReason::Oversized => &mut skipped.oversized_files,
            SnapshotSkipReason::Unreadable => &mut skipped.unreadable_entries,
            SnapshotSkipReason::Unstable => &mut skipped.unstable_files,
            SnapshotSkipReason::Unrepresentable => &mut skipped.unrepresentable_names,
        };
        *count = count.saturating_add(1);
        if skipped.samples.len() < MAX_SNAPSHOT_SKIPPED_SAMPLES
            && let Ok(display) = DisplayText::new(path.clone())
        {
            skipped.samples.push(SnapshotSkippedEntry {
                path: display,
                reason,
            });
        }
        if reason != SnapshotSkipReason::Unrepresentable {
            self.pruned.push(PrunedEntry { path, reason });
        }
    }

    /// Refuses an entry that would carry the capture past the client's limits.
    fn admit(&self, size: u64) -> Result<(), SnapshotError> {
        if self.entries.len() >= usize::try_from(self.limits.max_files).unwrap_or(usize::MAX) {
            return Err(limit_error(
                SnapshotLimit::Files,
                u64::from(self.limits.max_files),
            ));
        }
        if self.total_bytes.saturating_add(size) > self.limits.max_total_bytes {
            return Err(limit_error(
                SnapshotLimit::TotalBytes,
                self.limits.max_total_bytes,
            ));
        }
        Ok(())
    }

    fn record(&mut self, entry: StoredEntry) {
        self.total_bytes = self.total_bytes.saturating_add(entry.size_bytes);
        self.entries.push(entry);
    }

    /// Stores content the store lacks. `false` means the source no longer held the content hashed.
    fn store_blob(
        &mut self,
        digest: &str,
        size: u64,
        source: &mut dyn Read,
    ) -> Result<bool, SnapshotError> {
        let path = self.store.blob_path(digest)?;
        if self.store.exists(&path)? || self.pending_blobs.iter().any(|blob| blob.path == path) {
            return Ok(true);
        }
        if self.pending_bytes.saturating_add(size) > MAX_BLOB_BATCH_BYTES {
            self.flush_blobs()?;
        }
        self.reserve(size)?;
        if size <= MAX_BLOB_BATCH_BYTES {
            let Some(staged) = self.store.stage_blob(source, digest, size, self.token)? else {
                self.usage = self.usage.saturating_sub(size);
                return Ok(false);
            };
            self.pending_bytes += size;
            self.pending_blobs.push(staged);
            if self.pending_blobs.len() >= MAX_BLOB_BATCH_FILES
                || self.pending_bytes >= MAX_BLOB_BATCH_BYTES
            {
                self.flush_blobs()?;
            }
            return Ok(true);
        }
        match self.store.write_blob(source, digest, size, self.token)? {
            BlobWrite::Stored => {
                self.stored_blobs.push(path);
                Ok(true)
            }
            BlobWrite::Present => {
                self.usage = self.usage.saturating_sub(size);
                Ok(true)
            }
            BlobWrite::Mismatch => {
                self.usage = self.usage.saturating_sub(size);
                Ok(false)
            }
        }
    }

    fn flush_blobs(&mut self) -> Result<(), SnapshotError> {
        let synced = self
            .store
            .sync_blob_batch(&self.pending_blobs, self.token)?;
        for (staged, synced) in self.pending_blobs.iter().zip(synced) {
            if self.store.publish_blob(synced, self.token)? {
                self.stored_blobs.push(staged.path.clone());
            } else {
                self.usage = self.usage.saturating_sub(staged.size);
            }
        }
        self.clear_pending_blobs()
    }

    fn clear_pending_blobs(&mut self) -> Result<(), SnapshotError> {
        for staged in &self.pending_blobs {
            self.store.discard_staged_blob(staged)?;
        }
        self.pending_blobs.clear();
        self.pending_bytes = 0;
        Ok(())
    }

    fn reserve(&mut self, bytes: u64) -> Result<(), SnapshotError> {
        self.usage = self
            .usage
            .checked_add(bytes)
            .filter(|usage| *usage <= MAX_SNAPSHOT_STORAGE_BYTES)
            .ok_or(quota_error(
                SnapshotLimit::StorageBytes,
                MAX_SNAPSHOT_STORAGE_BYTES,
            ))?;
        Ok(())
    }

    fn publish(
        &mut self,
        inner: &SnapshotInner,
        content: ManifestContent,
        checkpoint_id: &str,
    ) -> Result<Manifest, SnapshotError> {
        self.progress.phase(SnapshotCapturePhase::Publishing);
        check_cancelled(self.token)?;
        let (manifest, bytes) = Manifest::encode(content, unix_ms())?;
        let path = self.store.manifest_path(&manifest.snapshot_id)?;
        self.store.sync(BLOBS)?;
        let manifest = if self.store.exists(&path)? {
            inner.load_manifest(&manifest.snapshot_id)?
        } else {
            if self.manifest_count >= MAX_SNAPSHOT_COUNT {
                return Err(quota_error(SnapshotLimit::Snapshots, MAX_SNAPSHOT_COUNT));
            }
            if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_MANIFEST_BYTES {
                return Err(limit_error(
                    SnapshotLimit::ManifestBytes,
                    MAX_MANIFEST_BYTES,
                ));
            }
            self.reserve(u64::try_from(bytes.len()).unwrap_or(u64::MAX))?;
            self.stored_manifest = Some(path.clone());
            self.store.write_immutable(&path, &bytes)?;
            manifest
        };
        self.store.sync(MANIFESTS)?;
        let checkpoint = serde_json::to_vec(&StoredCheckpoint {
            version: CHECKPOINT_VERSION.to_owned(),
            checkpoint_id: checkpoint_id.to_owned(),
            snapshot_id: manifest.snapshot_id.clone(),
        })
        .map_err(|_| SnapshotError::OperationFailed)?;
        self.reserve(u64::try_from(checkpoint.len()).unwrap_or(u64::MAX))?;
        check_cancelled(self.token)?;
        let path = self.store.checkpoint_path(checkpoint_id);
        self.stored_checkpoint = Some(path.clone());
        self.store.write_atomic(&path, &checkpoint)?;
        Ok(manifest)
    }

    /// Removes what this capture stored, including a checkpoint whose write failed only after it
    /// became visible. Nothing else can name any of it: captures and cleanups are serialized, the
    /// checkpoint did not exist when the capture began, and a manifest that named a blob before
    /// this capture kept it from being written.
    fn discard(&mut self) -> Result<(), SnapshotError> {
        self.clear_pending_blobs()
            .map_err(|_| SnapshotError::RollbackFailed)?;
        for (path, directory) in [
            (&self.stored_checkpoint, CHECKPOINTS),
            (&self.stored_manifest, MANIFESTS),
        ] {
            if let Some(path) = path {
                self.store
                    .remove(path)
                    .map_err(|_| SnapshotError::RollbackFailed)?;
                self.store
                    .sync(directory)
                    .map_err(|_| SnapshotError::RollbackFailed)?;
            }
        }
        for path in &self.stored_blobs {
            self.store
                .remove(path)
                .map_err(|_| SnapshotError::RollbackFailed)?;
        }
        self.store
            .sync(BLOBS)
            .map_err(|_| SnapshotError::RollbackFailed)
    }
}

/// Reads `file` from its start until the stamps taken around one read agree, so the digest names
/// content the file held for the whole read.
pub(crate) fn read_stable(
    file: &mut File,
    maximum: u64,
    token: &CancellationToken,
) -> Result<Stability, SnapshotError> {
    for _ in 0..STABLE_READ_ATTEMPTS {
        let before = file
            .metadata()
            .map_err(|_| SnapshotError::OperationFailed)?;
        if before.len() > maximum {
            return Ok(Stability::Oversized);
        }
        file.seek(SeekFrom::Start(0))
            .map_err(|_| SnapshotError::OperationFailed)?;
        let (digest, size) = digest_stream(file, None, maximum, token)?;
        let after = file
            .metadata()
            .map_err(|_| SnapshotError::OperationFailed)?;
        let stamp = SnapshotTreeStamp::of(&after);
        if SnapshotTreeStamp::of(&before) == stamp && size == after.len() {
            return Ok(Stability::Stable {
                digest,
                size,
                mode: file_mode(&after),
                stamp,
            });
        }
    }
    Ok(Stability::Unstable)
}

#[cfg(unix)]
fn file_mode(metadata: &Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;

    metadata.permissions().mode() & PERMISSION_BITS
}

#[cfg(not(unix))]
fn file_mode(metadata: &Metadata) -> u32 {
    u32::from(metadata.permissions().readonly())
}

pub(crate) fn validate_limits(limits: &SnapshotCaptureLimits) -> Result<(), SnapshotError> {
    let files = usize::try_from(limits.max_files).unwrap_or(usize::MAX);
    if files == 0
        || files > MAX_SNAPSHOT_FILES
        || limits.max_file_bytes == 0
        || limits.max_file_bytes > MAX_SNAPSHOT_FILE_BYTES
        || limits.max_total_bytes == 0
        || limits.max_total_bytes > MAX_SNAPSHOT_TOTAL_BYTES
    {
        return Err(SnapshotError::InvalidRequest);
    }
    Ok(())
}

pub(crate) fn capture_response(
    manifest: &Manifest,
    checkpoint_id: &str,
    scope: &str,
    reused_checkpoint: bool,
) -> Result<SnapshotCaptureResponse, SnapshotError> {
    if manifest.content.scope != scope {
        return Err(SnapshotError::InvalidRequest);
    }
    Ok(SnapshotCaptureResponse {
        version: ContractVersion::V1,
        snapshot: manifest.summary(Some(checkpoint_id))?,
        reused_checkpoint,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::BlobIo;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::{
        fs, io,
        sync::{Mutex, atomic::Ordering},
    };
    use tempfile::TempDir;

    const ENTRIES: usize = 20_000;
    #[cfg(unix)]
    const PRIVATE_MODE: u32 = 0o700;
    const QUOTA_DATA: &[u8] = b"retained";
    const EXTRA_DATA: &[u8] = b"extra";
    const SOURCE_READ_FAILURE: &str = "fixture source read failed";
    const TEST_LIMITS: SnapshotCaptureLimits = SnapshotCaptureLimits {
        max_files: MAX_SNAPSHOT_FILES as u32,
        max_file_bytes: MAX_SNAPSHOT_FILE_BYTES,
        max_total_bytes: MAX_SNAPSHOT_TOTAL_BYTES,
    };

    enum StagingFailure {
        Mismatch,
        ReadError,
        Cancelled,
    }

    struct InterruptedSource {
        token: CancellationToken,
        cancel: bool,
        read: bool,
    }

    impl Read for InterruptedSource {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if buffer.is_empty() {
                return Ok(0);
            }
            if self.read {
                return Err(io::Error::other(SOURCE_READ_FAILURE));
            }
            self.read = true;
            buffer[0] = QUOTA_DATA[0];
            if self.cancel {
                self.token.cancel();
            }
            Ok(1)
        }
    }

    fn test_store() -> (TempDir, TempDir, Store) {
        let workspace = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        fs::set_permissions(storage.path(), fs::Permissions::from_mode(PRIVATE_MODE)).unwrap();
        let store = Store::open(storage.path(), workspace.path(), None).unwrap();
        (workspace, storage, store)
    }

    fn capture<'a>(
        store: &'a Store,
        token: &'a CancellationToken,
        progress: &'a mut CaptureProgress,
    ) -> Capture<'a> {
        Capture {
            store,
            token,
            progress,
            limits: &TEST_LIMITS,
            usage: store.usage().unwrap(),
            stored_blobs: Vec::new(),
            pending_blobs: Vec::new(),
            pending_bytes: 0,
            stored_manifest: None,
            stored_checkpoint: None,
            entries: Vec::new(),
            pruned: Vec::new(),
            skipped: SnapshotSkipped::default(),
            total_bytes: 0,
            manifest_count: 0,
        }
    }

    #[test]
    fn bounded_blob_batches_deduplicate_and_sync_every_file_before_any_link() {
        let (_workspace, _storage, store) = test_store();
        let token = CancellationToken::new();
        let mut progress = CaptureProgress::new(None);
        let mut capture = capture(&store, &token, &mut progress);
        for index in 0..MAX_BLOB_BATCH_FILES {
            let data = index.to_string();
            let digest = digest_bytes(data.as_bytes());
            let size = data.len() as u64;
            assert!(
                capture
                    .store_blob(&digest, size, &mut data.as_bytes())
                    .unwrap()
            );
            let usage = capture.usage;
            let pending = capture.pending_blobs.len();
            assert!(
                capture
                    .store_blob(&digest, size, &mut data.as_bytes())
                    .unwrap()
            );
            assert_eq!(capture.usage, usage);
            assert_eq!(capture.pending_blobs.len(), pending);
            assert!(pending < MAX_BLOB_BATCH_FILES);
            assert!(capture.pending_bytes <= MAX_BLOB_BATCH_BYTES);
            if index + 1 < MAX_BLOB_BATCH_FILES {
                assert!(store.names(BLOBS).unwrap().is_empty());
                assert_eq!(store.usage().unwrap(), capture.pending_bytes);
            }
        }
        assert!(capture.pending_blobs.is_empty());
        assert_eq!(store.names(BLOBS).unwrap().len(), MAX_BLOB_BATCH_FILES);
        let events = store.faults.blob_io.lock().unwrap();
        assert_eq!(events.len(), MAX_BLOB_BATCH_FILES * 3);
        assert!(
            events[..MAX_BLOB_BATCH_FILES]
                .iter()
                .all(|event| matches!(event, BlobIo::Staged(_)))
        );
        assert!(
            events[MAX_BLOB_BATCH_FILES..MAX_BLOB_BATCH_FILES * 2]
                .iter()
                .all(|event| *event == BlobIo::Synced)
        );
        assert!(
            events[MAX_BLOB_BATCH_FILES * 2..]
                .iter()
                .all(|event| *event == BlobIo::Linked)
        );
    }

    #[test]
    fn byte_caps_flush_before_staging_and_large_blobs_bypass_the_pending_batch() {
        let (_workspace, _storage, store) = test_store();
        let token = CancellationToken::new();
        let mut progress = CaptureProgress::new(None);
        let mut capture = capture(&store, &token, &mut progress);
        let size = MAX_BLOB_BATCH_BYTES / 2 + 1;
        for byte in [1_u8, 2] {
            let data = vec![byte; size as usize];
            assert!(
                capture
                    .store_blob(&digest_bytes(&data), size, &mut data.as_slice())
                    .unwrap()
            );
            assert_eq!(capture.pending_bytes, size);
            assert_eq!(capture.pending_blobs.len(), 1);
            assert_eq!(store.names(BLOBS).unwrap().len(), usize::from(byte - 1));
        }
        let large = vec![3; MAX_BLOB_BATCH_BYTES as usize + 1];
        assert!(
            capture
                .store_blob(
                    &digest_bytes(&large),
                    large.len() as u64,
                    &mut large.as_slice()
                )
                .unwrap()
        );
        assert!(capture.pending_blobs.is_empty());
        assert_eq!(capture.pending_bytes, 0);
        assert_eq!(store.names(BLOBS).unwrap().len(), 3);
        assert_eq!(
            store
                .read_blob(&digest_bytes(&large), large.len() as u64)
                .unwrap(),
            large
        );
    }

    #[test]
    fn mismatched_staging_releases_quota_and_pending_bytes_are_charged_before_writing() {
        let (_workspace, _storage, store) = test_store();
        let token = CancellationToken::new();
        let mut progress = CaptureProgress::new(None);
        let mut capture = capture(&store, &token, &mut progress);
        let digest = digest_bytes(QUOTA_DATA);
        assert!(
            !capture
                .store_blob(
                    &digest,
                    QUOTA_DATA.len() as u64,
                    &mut b"modified".as_slice()
                )
                .unwrap()
        );
        assert_eq!(capture.usage, 0);
        assert_eq!(fs::read_dir(store.directory(BLOBS)).unwrap().count(), 0);
        capture.usage = MAX_SNAPSHOT_STORAGE_BYTES - QUOTA_DATA.len() as u64;
        let mut source = QUOTA_DATA;
        assert!(
            capture
                .store_blob(&digest, QUOTA_DATA.len() as u64, &mut source)
                .unwrap()
        );
        assert_eq!(capture.usage, MAX_SNAPSHOT_STORAGE_BYTES);
        let mut source = EXTRA_DATA;
        assert_eq!(
            capture
                .store_blob(
                    &digest_bytes(EXTRA_DATA),
                    EXTRA_DATA.len() as u64,
                    &mut source
                )
                .unwrap_err(),
            quota_error(SnapshotLimit::StorageBytes, MAX_SNAPSHOT_STORAGE_BYTES)
        );
        assert_eq!(capture.pending_blobs.len(), 1);
        capture.discard().unwrap();
        assert_eq!(fs::read_dir(store.directory(BLOBS)).unwrap().count(), 0);
    }

    #[test]
    fn failed_staging_requires_confirmed_cleanup_before_refund_or_clean_cancellation() {
        for remove_fails in [false, true] {
            for failure in [
                StagingFailure::Mismatch,
                StagingFailure::ReadError,
                StagingFailure::Cancelled,
            ] {
                let (_workspace, _storage, store) = test_store();
                let token = CancellationToken::new();
                let mut progress = CaptureProgress::new(None);
                let mut capture = capture(&store, &token, &mut progress);
                let reserved = QUOTA_DATA.len() as u64;
                capture.usage = MAX_SNAPSHOT_STORAGE_BYTES - reserved;
                store
                    .faults
                    .temporary_remove
                    .store(remove_fails, Ordering::SeqCst);
                let digest = digest_bytes(QUOTA_DATA);
                let result = match &failure {
                    StagingFailure::Mismatch => {
                        capture.store_blob(&digest, reserved, &mut b"modified".as_slice())
                    }
                    StagingFailure::ReadError | StagingFailure::Cancelled => {
                        let mut source = InterruptedSource {
                            token: token.clone(),
                            cancel: matches!(&failure, StagingFailure::Cancelled),
                            read: false,
                        };
                        capture.store_blob(&digest, reserved, &mut source)
                    }
                };
                if remove_fails {
                    assert_eq!(result.unwrap_err(), SnapshotError::RollbackFailed);
                    assert_eq!(capture.usage, MAX_SNAPSHOT_STORAGE_BYTES);
                    assert_eq!(fs::read_dir(store.directory(BLOBS)).unwrap().count(), 1);
                    let retained = store.usage().unwrap();
                    assert!(retained > 0 && retained <= reserved);
                } else {
                    match failure {
                        StagingFailure::Mismatch => {
                            assert!(!result.unwrap());
                            assert_eq!(capture.usage, MAX_SNAPSHOT_STORAGE_BYTES - reserved);
                        }
                        StagingFailure::ReadError => {
                            assert_eq!(result.unwrap_err(), SnapshotError::OperationFailed)
                        }
                        StagingFailure::Cancelled => {
                            assert_eq!(result.unwrap_err(), SnapshotError::Cancelled)
                        }
                    }
                    assert_eq!(fs::read_dir(store.directory(BLOBS)).unwrap().count(), 0);
                }
                assert!(store.names(BLOBS).unwrap().is_empty());
                assert!(capture.pending_blobs.is_empty());
                assert!(capture.stored_blobs.is_empty());
                store.faults.temporary_remove.store(false, Ordering::SeqCst);
                store.remove_temporaries().unwrap();
                assert_eq!(store.usage().unwrap(), 0);
            }
        }
    }

    #[test]
    fn single_blob_sync_link_and_cancel_failures_do_not_hide_failed_temporary_cleanup() {
        for cancel in [false, true] {
            for sync in [false, true] {
                let (_workspace, _storage, store) = test_store();
                let faults = &store.faults;
                let trigger = match (cancel, sync) {
                    (false, false) => &faults.blob_link_failure,
                    (false, true) => &faults.blob_sync_failure,
                    (true, false) => &faults.cancel_after_blob_link,
                    (true, true) => &faults.cancel_after_blob_sync,
                };
                trigger.store(1, Ordering::SeqCst);
                faults.temporary_remove.store(true, Ordering::SeqCst);
                let mut source = QUOTA_DATA;
                let result = store.write_blob(
                    &mut source,
                    &digest_bytes(QUOTA_DATA),
                    QUOTA_DATA.len() as u64,
                    &CancellationToken::new(),
                );
                assert_eq!(
                    result.map(|_| ()).unwrap_err(),
                    SnapshotError::RollbackFailed
                );
                assert!(store.usage().unwrap() >= QUOTA_DATA.len() as u64);
                faults.temporary_remove.store(false, Ordering::SeqCst);
                store.remove_temporaries().unwrap();
                assert_eq!(
                    fs::read_dir(store.directory(BLOBS)).unwrap().count(),
                    usize::from(cancel && !sync)
                );
            }
        }
    }

    #[test]
    fn a_growing_source_is_detected_without_writing_past_the_staging_reservation() {
        let mut file = tempfile::tempfile().unwrap();
        let maximum = QUOTA_DATA.len() as u64 - 1;
        let mut source = QUOTA_DATA;
        let (digest, read) = digest_stream(
            &mut source,
            Some(&mut file),
            maximum,
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(digest, digest_bytes(QUOTA_DATA));
        assert_eq!(read, QUOTA_DATA.len() as u64);
        assert_eq!(file.metadata().unwrap().len(), maximum);
    }

    #[derive(Default)]
    struct ProgressSink(Mutex<Vec<SnapshotCaptureProgress>>);

    impl SnapshotCaptureProgressSink for ProgressSink {
        fn publish(&self, progress: SnapshotCaptureProgress) {
            self.0.lock().unwrap().push(progress);
        }
    }

    #[test]
    fn capture_progress_is_rate_bounded_without_losing_final_counters() {
        let sink = Arc::new(ProgressSink::default());
        let mut progress = CaptureProgress::new(Some(sink.clone()));
        progress.phase(SnapshotCapturePhase::Persisting);
        let now = progress.emitted;
        for files in 1..=ENTRIES {
            progress.entry(files, files as u64, now);
        }
        assert_eq!(sink.0.lock().unwrap().len(), 2);
        progress.entry(ENTRIES + 1, ENTRIES as u64 + 1, now + PROGRESS_INTERVAL);
        assert_eq!(sink.0.lock().unwrap().len(), 3);
        progress.phase(SnapshotCapturePhase::Publishing);
        let events = sink.0.lock().unwrap();
        assert_eq!(events.len(), 4);
        assert_eq!(events[3].files, ENTRIES as u64 + 1);
        assert_eq!(events[3].entries, ENTRIES as u64 + 1);
        assert_eq!(events[3].bytes, ENTRIES as u64 + 1);
    }
}
