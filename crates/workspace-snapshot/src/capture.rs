//! Capture: one descriptor-relative walk of the scope into the object store. A file the stat cache
//! vouches for is not read again; any other is read until the stamps taken around the read agree.
//! Every entry left out is counted and recorded, so a restore never touches it.

#[cfg(test)]
use std::sync::atomic::Ordering;
use std::{
    fs::{File, Metadata},
    io::{Read, Seek, SeekFrom},
    mem,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use serde::Serialize;
use tokio_util::sync::CancellationToken;
use workcell_host_contract::{
    ContractVersion, DisplayText, MAX_SNAPSHOT_CAPTURE_ENTRIES, MAX_SNAPSHOT_CAPTURE_PATH_BYTES,
    MAX_SNAPSHOT_COUNT, MAX_SNAPSHOT_FILE_BYTES, MAX_SNAPSHOT_FILES, MAX_SNAPSHOT_SKIPPED_SAMPLES,
    MAX_SNAPSHOT_STORAGE_BYTES, MAX_SNAPSHOT_TOTAL_BYTES, SnapshotCaptureLimits,
    SnapshotCaptureResponse, SnapshotLimit, SnapshotSkipReason,
};
use workcell_mcp_files::{
    SnapshotTreeFile, SnapshotTreeLimits, SnapshotTreeLink, SnapshotTreeNode, SnapshotTreeStamp,
    WorkspaceSnapshotScope,
};
use workcell_snapshot_store::{
    Content, Entry, EntryKind, FileStamp, Meta, ObjectId, SkipReason, Skipped, SkippedPath,
    StatCache, StoreError, blob_id,
};

use crate::{
    CHECKPOINT_VERSION, Checkpoint, SnapshotError, SnapshotInner, StoredCheckpoint,
    check_cancelled, limit_error, quota_error,
    snapshot::{file_kind, skip_reason, snapshot_identifier, store_error, summary},
    store::{CHECKPOINTS, MAX_METADATA_BYTES, TREE_ENTRY_OVERHEAD},
    tree_error, unix_ms,
};

const STABLE_READ_ATTEMPTS: usize = 3;
const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);
/// Git's object header and zlib's fixed cost for one object, with room to spare.
const OBJECT_OVERHEAD: u64 = 64;
/// The wrapper tree naming the file tree and the metadata, and the metadata's own framing.
const WRAPPER_BYTES: u64 = 4 * OBJECT_OVERHEAD;
/// A stat cache entry besides its path: git's index entry, padding included.
const STAT_ENTRY_OVERHEAD: u64 = 72;

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
    /// The content, and the metadata it was read under, unchanged across the read.
    Stable {
        content: Vec<u8>,
        metadata: Metadata,
    },
    Oversized,
    /// Every read overlapped a change to the file.
    Unstable,
}

struct Capture<'a> {
    inner: &'a SnapshotInner,
    limits: &'a SnapshotCaptureLimits,
    token: &'a CancellationToken,
    progress: &'a mut CaptureProgress,
    cache: StatCache,
    /// Storage bytes in use, counting everything this capture reserved.
    usage: u64,
    entries: Vec<Entry>,
    pruned: Vec<SkippedPath>,
    skipped: Skipped,
    total_bytes: u64,
    largest_file_bytes: u64,
    /// What the trees naming the entries can occupy, as if no directory were shared.
    tree_bytes: u64,
    /// What saving the stat cache can add to it.
    cached_bytes: u64,
    stored_checkpoint: Option<PathBuf>,
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
        if let Some(checkpoint) = self.load_checkpoint(checkpoint_id)? {
            if checkpoint.meta.file_count > u64::from(limits.max_files) {
                return Err(limit_error(SnapshotLimit::Files, limits.max_files));
            }
            if checkpoint.meta.total_bytes > limits.max_total_bytes {
                return Err(limit_error(
                    SnapshotLimit::TotalBytes,
                    limits.max_total_bytes,
                ));
            }
            if checkpoint.stored.largest_file_bytes > limits.max_file_bytes {
                return Err(SnapshotError::InvalidRequest);
            }
            return capture_response(&checkpoint, scope.path(), true);
        }
        let inventory = self.store.capture_inventory(token)?;
        if inventory.checkpoints >= MAX_SNAPSHOT_COUNT {
            return Err(quota_error(SnapshotLimit::Checkpoints, MAX_SNAPSHOT_COUNT));
        }
        let started = self.store.capture_started();
        let mut capture = Capture {
            inner: self,
            limits,
            token,
            progress,
            cache: self.store.objects().stat_cache(),
            usage: inventory.bytes,
            entries: Vec::new(),
            pruned: Vec::new(),
            skipped: Skipped::default(),
            total_bytes: 0,
            largest_file_bytes: 0,
            tree_bytes: 0,
            cached_bytes: 0,
            stored_checkpoint: None,
        };
        let captured = capture
            .walk(scope)
            .and_then(|()| capture.publish(scope.path(), checkpoint_id));
        match captured {
            Ok(checkpoint) => {
                capture.save_cache(scope.path(), started);
                capture_response(&checkpoint, scope.path(), false)
            }
            Err(error) => {
                capture.progress.phase(SnapshotCapturePhase::Rollback);
                capture.discard()?;
                Err(error)
            }
        }
    }
}

impl Capture<'_> {
    fn walk(&mut self, scope: &WorkspaceSnapshotScope) -> Result<(), SnapshotError> {
        self.progress.phase(SnapshotCapturePhase::Persisting);
        check_cancelled(self.token)?;
        let tree_limits = SnapshotTreeLimits {
            max_entries: MAX_SNAPSHOT_CAPTURE_ENTRIES,
            max_path_bytes: usize::try_from(MAX_SNAPSHOT_CAPTURE_PATH_BYTES).unwrap_or(usize::MAX),
        };
        let walk = self
            .inner
            .workspace
            .walk_tree_bound(
                scope,
                self.inner.exclusions.clone(),
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
        Ok(())
    }

    fn file(&mut self, path: String, mut file: SnapshotTreeFile) -> Result<(), SnapshotError> {
        if file.metadata.len() > self.limits.max_file_bytes {
            self.skip(path, SnapshotSkipReason::Oversized);
            return Ok(());
        }
        let stamp = FileStamp::of(&file.metadata);
        if let Some(oid) = self.cache.lookup(&path, &stamp)
            && self.inner.store.objects().contains(&oid)
        {
            self.admit(file.metadata.len())?;
            self.cached(&path, stamp, oid);
            self.record(path, file_kind(&file.metadata), oid, file.metadata.len());
            return Ok(());
        }
        #[cfg(test)]
        self.inner
            .store
            .hooks
            .content_reads
            .fetch_add(1, Ordering::SeqCst);
        let (content, metadata) =
            match read_stable(&mut file.file, self.limits.max_file_bytes, self.token)? {
                Stability::Stable { content, metadata } => (content, metadata),
                Stability::Oversized => {
                    self.skip(path, SnapshotSkipReason::Oversized);
                    return Ok(());
                }
                Stability::Unstable => {
                    self.skip(path, SnapshotSkipReason::Unstable);
                    return Ok(());
                }
            };
        let size = u64::try_from(content.len()).unwrap_or(u64::MAX);
        self.admit(size)?;
        let Some(oid) = self.store(&content)? else {
            self.skip(path, SnapshotSkipReason::Unreadable);
            return Ok(());
        };
        self.cached(&path, FileStamp::of(&metadata), oid);
        self.record(path, file_kind(&metadata), oid, size);
        Ok(())
    }

    fn symlink(&mut self, path: String, link: SnapshotTreeLink) -> Result<(), SnapshotError> {
        let size = u64::try_from(link.target.len()).unwrap_or(u64::MAX);
        if size > self.limits.max_file_bytes {
            self.skip(path, SnapshotSkipReason::Oversized);
            return Ok(());
        }
        self.admit(size)?;
        let Some(oid) = self.store(&link.target)? else {
            self.skip(path, SnapshotSkipReason::Unreadable);
            return Ok(());
        };
        self.record(path, EntryKind::Symlink, oid, size);
        Ok(())
    }

    fn skip(&mut self, path: String, reason: SnapshotSkipReason) {
        let reason = skip_reason(reason);
        let count = self.skipped.counts.entry(reason).or_default();
        *count = count.saturating_add(1);
        if self.skipped.samples.len() < MAX_SNAPSHOT_SKIPPED_SAMPLES
            && DisplayText::new(path.clone()).is_ok()
        {
            self.skipped.samples.push(SkippedPath {
                path: path.clone(),
                reason,
            });
        }
        if reason != SkipReason::Unrepresentable {
            self.pruned.push(SkippedPath { path, reason });
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

    fn record(&mut self, path: String, kind: EntryKind, oid: ObjectId, size: u64) {
        let components = u64::try_from(path.split('/').count()).unwrap_or(u64::MAX);
        self.tree_bytes = self.tree_bytes.saturating_add(
            (TREE_ENTRY_OVERHEAD + OBJECT_OVERHEAD)
                .saturating_mul(components)
                .saturating_add(u64::try_from(path.len()).unwrap_or(u64::MAX)),
        );
        self.total_bytes = self.total_bytes.saturating_add(size);
        self.largest_file_bytes = self.largest_file_bytes.max(size);
        self.entries.push(Entry {
            path,
            content: Content { kind, oid },
        });
    }

    fn cached(&mut self, path: &str, stamp: FileStamp, oid: ObjectId) {
        self.cached_bytes = self.cached_bytes.saturating_add(
            STAT_ENTRY_OVERHEAD.saturating_add(u64::try_from(path.len()).unwrap_or(u64::MAX)),
        );
        self.cache.record(path.to_owned(), stamp, oid);
    }

    /// Stores content, charging the quota only for what is new: content the store already holds
    /// costs nothing more. `None` is content git refuses to name, a known SHA-1 collision.
    fn store(&mut self, content: &[u8]) -> Result<Option<ObjectId>, SnapshotError> {
        let objects = self.inner.store.objects();
        let bound = compressed_bound(
            u64::try_from(content.len())
                .unwrap_or(u64::MAX)
                .saturating_add(OBJECT_OVERHEAD),
        );
        let reservation = self.reserve(bound);
        if let Err(refusal) = reservation {
            return match blob_id(content) {
                Ok(oid) if objects.contains(&oid) => Ok(Some(oid)),
                Ok(_) => Err(refusal),
                Err(_) => Ok(None),
            };
        }
        match objects.write_blob(content) {
            Ok(written) => {
                if !written.new {
                    self.usage = self.usage.saturating_sub(bound);
                }
                Ok(Some(written.oid))
            }
            Err(StoreError::Collision) => {
                self.usage = self.usage.saturating_sub(bound);
                Ok(None)
            }
            Err(error) => Err(store_error(error, &[])),
        }
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

    /// Builds the snapshot, makes every object it names durable, then names it by the checkpoint.
    fn publish(&mut self, scope: &str, checkpoint_id: &str) -> Result<Checkpoint, SnapshotError> {
        self.progress.phase(SnapshotCapturePhase::Publishing);
        check_cancelled(self.token)?;
        let mut entries = mem::take(&mut self.entries);
        entries.sort_unstable_by(|left, right| left.path.cmp(&right.path));
        let mut pruned = mem::take(&mut self.pruned);
        pruned.sort_unstable_by(|left, right| left.path.cmp(&right.path));
        let meta = Meta {
            scope: scope.to_owned(),
            pruned,
            exclusions: self.inner.exclusions.clone(),
            skipped: mem::take(&mut self.skipped),
            file_count: u64::try_from(entries.len()).unwrap_or(u64::MAX),
            total_bytes: self.total_bytes,
        };
        let meta_bytes = serde_json::to_vec(&meta)
            .map_err(|_| SnapshotError::OperationFailed)?
            .len();
        let meta_bytes = u64::try_from(meta_bytes).unwrap_or(u64::MAX);
        if meta_bytes > MAX_METADATA_BYTES {
            return Err(limit_error(
                SnapshotLimit::MetadataBytes,
                MAX_METADATA_BYTES,
            ));
        }
        self.reserve(compressed_bound(
            self.tree_bytes
                .saturating_add(meta_bytes)
                .saturating_add(WRAPPER_BYTES),
        ))?;
        let store = &self.inner.store;
        let id = store
            .objects()
            .build(&entries, &meta)
            .map_err(|error| store_error(error, &[]))?;
        store.sync_objects()?;
        let stored = StoredCheckpoint {
            version: CHECKPOINT_VERSION.to_owned(),
            checkpoint_id: checkpoint_id.to_owned(),
            snapshot_id: snapshot_identifier(&id),
            created_at_unix_ms: self.inner.created_at(&id)?.unwrap_or_else(unix_ms),
            largest_file_bytes: self.largest_file_bytes,
        };
        let bytes = serde_json::to_vec(&stored).map_err(|_| SnapshotError::OperationFailed)?;
        self.reserve(u64::try_from(bytes.len()).unwrap_or(u64::MAX))?;
        check_cancelled(self.token)?;
        let path = store.checkpoint_path(checkpoint_id);
        self.stored_checkpoint = Some(path.clone());
        store.write_atomic(&path, &bytes)?;
        Ok(Checkpoint { stored, id, meta })
    }

    /// Saves what this capture read, so the next one need not read it again. Only a cache: when
    /// it does not fit the quota or cannot be written, the next capture reads everything.
    fn save_cache(self, scope: &str, started: SystemTime) {
        let fits = self
            .usage
            .checked_add(self.cached_bytes)
            .is_some_and(|usage| usage <= MAX_SNAPSHOT_STORAGE_BYTES);
        if fits
            && let Err(error) = self
                .inner
                .store
                .objects()
                .save_stat_cache(self.cache, scope, started)
        {
            tracing::warn!(%error, "workspace snapshot stat cache was not saved");
        }
    }

    /// Deletes what this capture staged and removes the checkpoint should its write have become
    /// visible before failing. Objects it named stay for collection: nothing names those this
    /// capture added, and anything may name the others.
    fn discard(&self) -> Result<(), SnapshotError> {
        let store = &self.inner.store;
        if let Err(error) = store.objects().abandon() {
            tracing::warn!(%error, "staged workspace snapshot objects were not deleted");
        }
        if let Some(path) = &self.stored_checkpoint {
            store
                .remove(path)
                .and_then(|()| store.sync(CHECKPOINTS))
                .map_err(|_| SnapshotError::RollbackFailed)?;
        }
        Ok(())
    }
}

/// What stored objects can occupy for `bytes` of their content, headers included. zlib never spends
/// more than nine bits on a byte, the cost of a fixed Huffman literal, so an eighth covers any
/// content; `OBJECT_OVERHEAD` covers each object's framing.
const fn compressed_bound(bytes: u64) -> u64 {
    bytes.saturating_add(bytes.div_ceil(8))
}

/// Reads `file` from its start until the metadata taken around one read agrees, so the content is
/// what the file held for the whole read.
pub(crate) fn read_stable(
    file: &mut File,
    maximum: u64,
    token: &CancellationToken,
) -> Result<Stability, SnapshotError> {
    for _ in 0..STABLE_READ_ATTEMPTS {
        check_cancelled(token)?;
        let before = file
            .metadata()
            .map_err(|_| SnapshotError::OperationFailed)?;
        if before.len() > maximum {
            return Ok(Stability::Oversized);
        }
        file.seek(SeekFrom::Start(0))
            .map_err(|_| SnapshotError::OperationFailed)?;
        let mut content = Vec::with_capacity(usize::try_from(before.len()).unwrap_or(0));
        Read::by_ref(file)
            .take(maximum.saturating_add(1))
            .read_to_end(&mut content)
            .map_err(|_| SnapshotError::OperationFailed)?;
        let after = file
            .metadata()
            .map_err(|_| SnapshotError::OperationFailed)?;
        if SnapshotTreeStamp::of(&before) == SnapshotTreeStamp::of(&after)
            && u64::try_from(content.len()).ok() == Some(after.len())
        {
            return Ok(Stability::Stable {
                content,
                metadata: after,
            });
        }
    }
    Ok(Stability::Unstable)
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
    checkpoint: &Checkpoint,
    scope: &str,
    reused_checkpoint: bool,
) -> Result<SnapshotCaptureResponse, SnapshotError> {
    if checkpoint.meta.scope != scope {
        return Err(SnapshotError::InvalidRequest);
    }
    Ok(SnapshotCaptureResponse {
        version: ContractVersion::V1,
        snapshot: summary(
            &checkpoint.id,
            &checkpoint.meta,
            Some(&checkpoint.stored.checkpoint_id),
            checkpoint.stored.created_at_unix_ms,
        )?,
        reused_checkpoint,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use workcell_snapshot_store::{ObjectStore, StoreOptions};

    use super::*;

    const ENTRIES: usize = 20_000;
    const INCOMPRESSIBLE_BYTES: usize = 1_024 * 1_024;
    const XORSHIFT_SEED: u64 = 0x9e37_79b9_7f4a_7c15;

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

    #[test]
    fn the_quota_charge_for_an_object_covers_what_storing_incompressible_content_takes() {
        let storage = tempfile::tempdir().unwrap();
        let objects = ObjectStore::open(
            storage.path(),
            StoreOptions {
                private: false,
                max_object_bytes: None,
            },
        )
        .unwrap();
        let empty = objects.usage().unwrap().bytes;
        let mut state = XORSHIFT_SEED;
        let content = (0..INCOMPRESSIBLE_BYTES)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state.to_le_bytes()[0]
            })
            .collect::<Vec<_>>();
        objects.write_blob(&content).unwrap();

        let stored = objects.usage().unwrap().bytes - empty;
        assert!(stored > content.len() as u64);
        assert!(stored <= compressed_bound(content.len() as u64 + OBJECT_OVERHEAD));
    }
}
