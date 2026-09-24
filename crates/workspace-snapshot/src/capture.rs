//! Capture: one descriptor-relative walk of the scope. Each regular file is read until the stamps
//! taken around the read agree, its content is stored once by digest, and every entry left out is
//! counted and recorded so a restore never touches it.

use std::{
    fs::{File, Metadata},
    io::{Read, Seek, SeekFrom},
    mem,
    path::PathBuf,
};

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
};

use crate::{
    CHECKPOINT_VERSION, SnapshotError, SnapshotInner, StoredCheckpoint, digest_bytes, limit_error,
    manifest::{
        MANIFEST_VERSION, MAX_MANIFEST_BYTES, Manifest, ManifestContent, PERMISSION_BITS,
        PrunedEntry, SYMLINK_MODE, StoredEntry, StoredEntryKind,
    },
    quota_error,
    store::{BLOBS, BlobWrite, CHECKPOINTS, MANIFESTS, Store, digest_stream},
    tree_error, unix_ms,
};

const STABLE_READ_ATTEMPTS: usize = 3;

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
    stored_manifest: Option<PathBuf>,
    stored_checkpoint: Option<PathBuf>,
    entries: Vec<StoredEntry>,
    pruned: Vec<PrunedEntry>,
    skipped: SnapshotSkipped,
    total_bytes: u64,
}

impl SnapshotInner {
    /// Captures `scope` as `checkpoint_id`. An existing checkpoint is returned as it was captured.
    pub(crate) fn capture(
        &self,
        checkpoint_id: &str,
        scope: &str,
        limits: &SnapshotCaptureLimits,
        token: &CancellationToken,
    ) -> Result<SnapshotCaptureResponse, SnapshotError> {
        validate_limits(limits)?;
        if let Some(manifest) = self.load_checkpoint(checkpoint_id)? {
            return capture_response(&manifest, checkpoint_id, true);
        }
        if self.store.count(CHECKPOINTS)? >= MAX_SNAPSHOT_COUNT {
            return Err(quota_error(SnapshotLimit::Checkpoints, MAX_SNAPSHOT_COUNT));
        }
        let mut capture = Capture {
            store: &self.store,
            limits,
            token,
            usage: self.store.usage()?,
            stored_blobs: Vec::new(),
            stored_manifest: None,
            stored_checkpoint: None,
            entries: Vec::new(),
            pruned: Vec::new(),
            skipped: SnapshotSkipped::default(),
            total_bytes: 0,
        };
        let captured = capture
            .walk(self, scope)
            .and_then(|content| capture.publish(self, content, checkpoint_id));
        match captured {
            Ok(manifest) => capture_response(&manifest, checkpoint_id, false),
            Err(error) => {
                capture.discard();
                Err(error)
            }
        }
    }
}

impl Capture<'_> {
    fn walk(
        &mut self,
        inner: &SnapshotInner,
        scope: &str,
    ) -> Result<ManifestContent, SnapshotError> {
        let tree_limits = SnapshotTreeLimits {
            max_entries: MAX_SNAPSHOT_CAPTURE_ENTRIES,
            max_path_bytes: usize::try_from(MAX_SNAPSHOT_CAPTURE_PATH_BYTES).unwrap_or(usize::MAX),
        };
        let walk = inner
            .workspace
            .walk_tree(
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
        }
        self.entries
            .sort_unstable_by(|left, right| left.path.cmp(&right.path));
        self.pruned
            .sort_unstable_by(|left, right| left.path.cmp(&right.path));
        Ok(ManifestContent {
            version: MANIFEST_VERSION.to_owned(),
            scope: scope.to_owned(),
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
        if self.store.exists(&path)? {
            return Ok(true);
        }
        self.reserve(size)?;
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
        let (manifest, bytes) = Manifest::encode(content, unix_ms())?;
        let path = self.store.manifest_path(&manifest.snapshot_id)?;
        if !self.stored_blobs.is_empty() {
            self.store.sync(BLOBS)?;
        }
        let manifest = if self.store.exists(&path)? {
            inner.load_manifest(&manifest.snapshot_id)?
        } else {
            if self.store.count(MANIFESTS)? >= MAX_SNAPSHOT_COUNT {
                return Err(quota_error(SnapshotLimit::Snapshots, MAX_SNAPSHOT_COUNT));
            }
            if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_MANIFEST_BYTES {
                return Err(limit_error(
                    SnapshotLimit::ManifestBytes,
                    MAX_MANIFEST_BYTES,
                ));
            }
            self.reserve(u64::try_from(bytes.len()).unwrap_or(u64::MAX))?;
            self.store.write_immutable(&path, &bytes)?;
            self.stored_manifest = Some(path);
            manifest
        };
        let checkpoint = serde_json::to_vec(&StoredCheckpoint {
            version: CHECKPOINT_VERSION.to_owned(),
            checkpoint_id: checkpoint_id.to_owned(),
            snapshot_id: manifest.snapshot_id.clone(),
        })
        .map_err(|_| SnapshotError::OperationFailed)?;
        self.reserve(u64::try_from(checkpoint.len()).unwrap_or(u64::MAX))?;
        let path = self.store.checkpoint_path(checkpoint_id);
        self.stored_checkpoint = Some(path.clone());
        self.store.write_atomic(&path, &checkpoint)?;
        Ok(manifest)
    }

    /// Removes what this capture stored, including a checkpoint whose write failed only after it
    /// became visible. Nothing else can name any of it: captures and cleanups are serialized, the
    /// checkpoint did not exist when the capture began, and a manifest that named a blob before
    /// this capture kept it from being written.
    fn discard(&self) {
        for path in self
            .stored_blobs
            .iter()
            .chain(&self.stored_manifest)
            .chain(&self.stored_checkpoint)
        {
            let _ = self.store.remove(path);
        }
        for directory in [BLOBS, MANIFESTS, CHECKPOINTS] {
            let _ = self.store.sync(directory);
        }
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

fn validate_limits(limits: &SnapshotCaptureLimits) -> Result<(), SnapshotError> {
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

fn capture_response(
    manifest: &Manifest,
    checkpoint_id: &str,
    reused_checkpoint: bool,
) -> Result<SnapshotCaptureResponse, SnapshotError> {
    Ok(SnapshotCaptureResponse {
        version: ContractVersion::V1,
        snapshot: manifest.summary(Some(checkpoint_id))?,
        reused_checkpoint,
    })
}
