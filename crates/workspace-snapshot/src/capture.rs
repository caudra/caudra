//! Capture: the state of a record's scope, stored as a snapshot tree. A file the stat cache vouches
//! for is not read again; any other is read until the stamps taken around the read agree. A path the
//! capture sees but cannot store is blind: pruned from the tree and listed, so no diff ever mistakes
//! it for an absent one.

#[cfg(test)]
use std::sync::atomic::Ordering;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, Metadata},
    io::{self, Read, Seek, SeekFrom},
    mem,
};

use tokio_util::sync::CancellationToken;
use workcell_host_contract::{
    MAX_SNAPSHOT_CAPTURE_ENTRIES, MAX_SNAPSHOT_CAPTURE_PATH_BYTES, MAX_SNAPSHOT_STORAGE_BYTES,
    RecordLimits, RecordScope, SnapshotLimit, SnapshotSkipReason, UnrecordedReason,
};
use workcell_mcp_files::{
    SnapshotTreeError, SnapshotTreeFile, SnapshotTreeLimits, SnapshotTreeNode,
    SnapshotTreeObserved, SnapshotTreeStamp, SnapshotTreeWalk,
};
use workcell_snapshot_store::{
    Content, Entry, EntryKind, FileStamp, Meta, ObjectId, ROOT_SCOPE, Skipped, SkippedPath,
    SnapshotId, StatCache, StoreError, blob_id,
};

use crate::{
    SnapshotError, Workspace, check_cancelled,
    format::{Blind, Stamp},
    limit_error, quota_error,
    snapshot::{blind_skip, file_kind, store_error, walk_skip, workspace_path},
    store::{MAX_METADATA_BYTES, Store, TREE_ENTRY_OVERHEAD},
    tree_error,
};

const STABLE_READ_ATTEMPTS: usize = 3;
/// Git's object header and zlib's fixed cost for one object, with room to spare.
const OBJECT_OVERHEAD: u64 = 64;
/// The wrapper tree naming the file tree and the metadata, and the metadata's own framing.
const WRAPPER_BYTES: u64 = 4 * OBJECT_OVERHEAD;
/// A stat cache entry besides its path: git's index entry, padding included.
const STAT_ENTRY_OVERHEAD: u64 = 72;

/// Whether a paths capture holds the whole tree as well as the paths it names.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Widening {
    /// A begin widens when a named path is one a call can write through to another path.
    Decide,
    /// A finish covers what its begin did.
    Decided(bool),
}

/// A captured scope: its snapshot tree and the paths it could not store.
pub(crate) struct Captured {
    pub(crate) tree: SnapshotId,
    pub(crate) blind: Vec<Blind>,
    /// What the store may occupy now, an upper bound.
    pub(crate) usage: u64,
    pub(crate) widened: bool,
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
    store: &'a Store,
    workspace: &'a Workspace,
    limits: &'a RecordLimits,
    token: &'a CancellationToken,
    may_widen: bool,
    widened: bool,
    cache: Option<StatCache>,
    /// What the store may occupy, counting everything this capture reserved.
    usage: u64,
    measured: bool,
    entries: BTreeMap<String, Content>,
    blind: BTreeMap<String, Blind>,
    pruned: Vec<SkippedPath>,
    seen: usize,
    path_bytes: u64,
    total_bytes: u64,
    /// What the trees naming the entries can occupy, as if no directory were shared.
    tree_bytes: u64,
    /// What saving the stat cache can add to the store.
    cached_bytes: u64,
}

/// Captures `scope` into the store, whose usage so far is at most `usage`. Whatever a failed
/// capture staged is deleted; objects it named stay for collection.
pub(crate) fn capture(
    store: &Store,
    workspace: &Workspace,
    scope: &RecordScope,
    widening: Widening,
    limits: &RecordLimits,
    usage: u64,
    token: &CancellationToken,
) -> Result<Captured, SnapshotError> {
    let started = store.capture_started();
    let mut capture = Capture {
        store,
        workspace,
        limits,
        token,
        may_widen: widening == Widening::Decide,
        widened: widening == Widening::Decided(true),
        cache: None,
        usage,
        measured: false,
        entries: BTreeMap::new(),
        blind: BTreeMap::new(),
        pruned: Vec::new(),
        seen: 0,
        path_bytes: 0,
        total_bytes: 0,
        tree_bytes: 0,
        cached_bytes: 0,
    };
    let built = capture.scope(scope).and_then(|root| capture.build(root));
    match built {
        Ok(tree) => {
            let walked = match scope {
                RecordScope::Workspace { directory } => Some(directory.as_str()),
                RecordScope::Paths { .. } => capture.widened.then_some(ROOT_SCOPE),
            };
            if let Some(walked) = walked {
                capture.save_cache(walked, started);
            }
            Ok(Captured {
                tree,
                blind: capture.blind.into_values().collect(),
                usage: capture.usage,
                widened: capture.widened,
            })
        }
        Err(error) => {
            if let Err(abandoned) = store.objects().abandon() {
                tracing::warn!(error = %abandoned, "staged workspace change objects were not deleted");
            }
            Err(error)
        }
    }
}

impl Capture<'_> {
    /// Captures the scope and returns the directory its tree covers.
    fn scope(&mut self, scope: &RecordScope) -> Result<String, SnapshotError> {
        match scope {
            RecordScope::Workspace { directory } => {
                self.walk(directory.as_str())?;
                Ok(directory.as_str().to_owned())
            }
            RecordScope::Paths { paths } => {
                let named = paths
                    .iter()
                    .map(|path| path.as_str())
                    .collect::<BTreeSet<_>>();
                for path in named {
                    check_cancelled(self.token)?;
                    self.named(path)?;
                }
                if self.widened {
                    self.walk(ROOT_SCOPE)?;
                }
                Ok(ROOT_SCOPE.to_owned())
            }
        }
    }

    /// A path the record names, captured even when ignored. An excluded or protected one is left
    /// out silently, as a walk leaves it out.
    fn named(&mut self, path: &str) -> Result<(), SnapshotError> {
        if self.workspace.excluded(path) || self.captured(path) {
            return Ok(());
        }
        self.charge(path)?;
        let observed = self
            .workspace
            .access
            .observe_tree_entry(&workspace_path(path)?);
        match observed {
            Ok(SnapshotTreeObserved::Absent) | Err(SnapshotTreeError::Protected) => Ok(()),
            Ok(SnapshotTreeObserved::File(file)) => {
                if hard_linked(&file.metadata) {
                    self.widen();
                }
                self.file(path.to_owned(), file)
            }
            Ok(SnapshotTreeObserved::Symlink(link)) => {
                self.widen();
                self.symlink(path.to_owned(), &link.target)
            }
            Ok(SnapshotTreeObserved::Directory) => match self.open_walk(path) {
                Ok(walk) => self.take(walk),
                Err(SnapshotTreeError::ScopeUnavailable) => {
                    self.blind(path.to_owned(), UnrecordedReason::Unreadable, None);
                    Ok(())
                }
                Err(error) => Err(tree_error(error)),
            },
            Ok(SnapshotTreeObserved::Other) => {
                self.blind(path.to_owned(), UnrecordedReason::Special, None);
                Ok(())
            }
            Err(SnapshotTreeError::Blocked) => {
                self.widen();
                if !self.widened {
                    self.blind(path.to_owned(), UnrecordedReason::Blocked, None);
                }
                Ok(())
            }
            Err(SnapshotTreeError::Failed(error))
                if error.kind() == io::ErrorKind::PermissionDenied =>
            {
                self.blind(path.to_owned(), UnrecordedReason::Unreadable, None);
                Ok(())
            }
            Err(error) => Err(tree_error(error)),
        }
    }

    /// A call can write through this named path to another one, so a begin takes the whole tree
    /// as well: wherever the write lands inside the root, the record holds it. A widened record
    /// leaves a blocked named path to that walk.
    fn widen(&mut self) {
        self.widened |= self.may_widen;
    }

    fn captured(&self, path: &str) -> bool {
        self.entries.contains_key(path) || self.blind.contains_key(path)
    }

    fn walk(&mut self, directory: &str) -> Result<(), SnapshotError> {
        let walk = self.open_walk(directory).map_err(tree_error)?;
        self.take(walk)
    }

    fn open_walk(&self, directory: &str) -> Result<SnapshotTreeWalk, SnapshotTreeError> {
        let limits = SnapshotTreeLimits {
            max_entries: MAX_SNAPSHOT_CAPTURE_ENTRIES,
            max_path_bytes: usize::try_from(MAX_SNAPSHOT_CAPTURE_PATH_BYTES).unwrap_or(usize::MAX),
        };
        self.workspace.access.walk_tree(
            directory,
            self.workspace.exclusions.clone(),
            limits,
            self.token.clone(),
        )
    }

    /// Captures what a walk yields that this capture does not already hold.
    fn take(&mut self, walk: SnapshotTreeWalk) -> Result<(), SnapshotError> {
        check_cancelled(self.token)?;
        for entry in walk {
            let entry = entry.map_err(tree_error)?;
            if self.captured(&entry.path) {
                continue;
            }
            self.charge(&entry.path)?;
            match entry.node {
                SnapshotTreeNode::File(file) => self.file(entry.path, file)?,
                SnapshotTreeNode::Symlink(link) => self.symlink(entry.path, &link.target)?,
                SnapshotTreeNode::Skipped(reason) => self.skip(entry.path, reason),
            }
        }
        Ok(())
    }

    fn file(&mut self, path: String, mut file: SnapshotTreeFile) -> Result<(), SnapshotError> {
        let stamp = Stamp::of(&file.metadata);
        let size = file.metadata.len();
        if size > self.limits.max_file_bytes {
            self.blind(path, UnrecordedReason::Oversized, Some(stamp));
            return Ok(());
        }
        let file_stamp = FileStamp::of(&file.metadata);
        if let Some(oid) = self.cache().lookup(&path, &file_stamp)
            && self.store.objects().contains(&oid)
        {
            self.admit(size)?;
            self.cached(&path, file_stamp, oid);
            self.record(path, file_kind(&file.metadata), oid, size);
            return Ok(());
        }
        #[cfg(test)]
        self.store
            .hooks
            .content_reads
            .fetch_add(1, Ordering::SeqCst);
        let (content, metadata) =
            match read_stable(&mut file.file, self.limits.max_file_bytes, self.token)? {
                Stability::Stable { content, metadata } => (content, metadata),
                Stability::Oversized => {
                    self.blind(path, UnrecordedReason::Oversized, Some(stamp));
                    return Ok(());
                }
                Stability::Unstable => {
                    self.blind(path, UnrecordedReason::Unstable, Some(stamp));
                    return Ok(());
                }
            };
        let size = byte_count(content.len());
        self.admit(size)?;
        let Some(oid) = self.write(&content)? else {
            self.blind(path, UnrecordedReason::Unreadable, Some(stamp));
            return Ok(());
        };
        self.cached(&path, FileStamp::of(&metadata), oid);
        self.record(path, file_kind(&metadata), oid, size);
        Ok(())
    }

    fn symlink(&mut self, path: String, target: &[u8]) -> Result<(), SnapshotError> {
        let size = byte_count(target.len());
        if size > self.limits.max_file_bytes {
            self.blind(path, UnrecordedReason::Oversized, None);
            return Ok(());
        }
        self.admit(size)?;
        let Some(oid) = self.write(target)? else {
            self.blind(path, UnrecordedReason::Unreadable, None);
            return Ok(());
        };
        self.record(path, EntryKind::Symlink, oid, size);
        Ok(())
    }

    /// What a walk left out is left out of the record too: no diff reports it.
    fn skip(&mut self, path: String, reason: SnapshotSkipReason) {
        if reason != SnapshotSkipReason::Unrepresentable {
            self.pruned.push(SkippedPath {
                path,
                reason: walk_skip(reason),
            });
        }
    }

    fn blind(&mut self, path: String, reason: UnrecordedReason, stamp: Option<Stamp>) {
        self.blind.insert(
            path.clone(),
            Blind {
                path,
                reason,
                stamp,
            },
        );
    }

    /// Counts one entry against the ceilings every capture shares, however many walks it takes.
    fn charge(&mut self, path: &str) -> Result<(), SnapshotError> {
        self.seen += 1;
        self.path_bytes = self.path_bytes.saturating_add(byte_count(path.len()));
        if self.seen > MAX_SNAPSHOT_CAPTURE_ENTRIES {
            return Err(limit_error(
                SnapshotLimit::CaptureEntries,
                MAX_SNAPSHOT_CAPTURE_ENTRIES,
            ));
        }
        if self.path_bytes > MAX_SNAPSHOT_CAPTURE_PATH_BYTES {
            return Err(limit_error(
                SnapshotLimit::CapturePathBytes,
                MAX_SNAPSHOT_CAPTURE_PATH_BYTES,
            ));
        }
        Ok(())
    }

    /// Refuses an entry that would carry the record past its file or byte limit, in either scope:
    /// one named directory must not pull an unbounded subtree into the store.
    fn admit(&self, size: u64) -> Result<(), SnapshotError> {
        if self.entries.len() >= usize::try_from(self.limits.max_files).unwrap_or(usize::MAX) {
            return Err(limit_error(SnapshotLimit::Files, self.limits.max_files));
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
        let components = byte_count(path.split('/').count());
        self.tree_bytes = self.tree_bytes.saturating_add(
            (TREE_ENTRY_OVERHEAD + OBJECT_OVERHEAD)
                .saturating_mul(components)
                .saturating_add(byte_count(path.len())),
        );
        self.total_bytes = self.total_bytes.saturating_add(size);
        self.entries.insert(path, Content { kind, oid });
    }

    fn cache(&mut self) -> &StatCache {
        self.cache
            .get_or_insert_with(|| self.store.objects().stat_cache())
    }

    /// Notes what the capture read, which saving the cache after a whole-tree capture keeps for
    /// the next one.
    fn cached(&mut self, path: &str, stamp: FileStamp, oid: ObjectId) {
        self.cached_bytes = self
            .cached_bytes
            .saturating_add(STAT_ENTRY_OVERHEAD.saturating_add(byte_count(path.len())));
        if let Some(cache) = &mut self.cache {
            cache.record(path.to_owned(), stamp, oid);
        }
    }

    /// Stores content, charging the store only for what is new. `None` is content git refuses to
    /// name, a known SHA-1 collision.
    fn write(&mut self, content: &[u8]) -> Result<Option<ObjectId>, SnapshotError> {
        let objects = self.store.objects();
        let bound = compressed_bound(byte_count(content.len()).saturating_add(OBJECT_OVERHEAD));
        if let Err(refusal) = self.reserve(bound) {
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
            Err(error) => Err(store_error(error)),
        }
    }

    /// Charges `bytes` against the store's ceiling. An estimate past it is measured once before
    /// the capture is refused: the store is full only when nothing more fits.
    fn reserve(&mut self, bytes: u64) -> Result<(), SnapshotError> {
        if self.usage.saturating_add(bytes) > MAX_SNAPSHOT_STORAGE_BYTES && !self.measured {
            self.measured = true;
            self.usage = self.store.usage()?;
        }
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

    /// Builds the tree of what was captured and makes every object it names durable.
    fn build(&mut self, scope: String) -> Result<SnapshotId, SnapshotError> {
        check_cancelled(self.token)?;
        for path in self.blind.keys() {
            self.entries.remove(path);
        }
        let entries = mem::take(&mut self.entries)
            .into_iter()
            .map(|(path, content)| Entry { path, content })
            .collect::<Vec<_>>();
        let mut pruned = mem::take(&mut self.pruned);
        pruned.extend(self.blind.values().map(|blind| SkippedPath {
            path: blind.path.clone(),
            reason: blind_skip(blind.reason),
        }));
        pruned.sort_unstable_by(|left, right| left.path.cmp(&right.path));
        pruned.dedup_by(|left, right| left.path == right.path);
        let meta = Meta {
            scope,
            pruned,
            exclusions: self.workspace.exclusions.clone(),
            skipped: Skipped::default(),
            file_count: byte_count(entries.len()),
            total_bytes: self.total_bytes,
        };
        let meta_bytes = byte_count(
            serde_json::to_vec(&meta)
                .map_err(|_| SnapshotError::OperationFailed)?
                .len(),
        );
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
        let tree = self
            .store
            .objects()
            .build(&entries, &meta)
            .map_err(store_error)?;
        self.store.sync_objects()?;
        Ok(tree)
    }

    /// Saves what a whole-tree capture read, so the next one need not read it again. Only a cache:
    /// when it does not fit the store or cannot be written, the next capture reads everything.
    fn save_cache(&mut self, scope: &str, started: std::time::SystemTime) {
        let Some(cache) = self.cache.take() else {
            return;
        };
        let Some(usage) = self
            .usage
            .checked_add(self.cached_bytes)
            .filter(|usage| *usage <= MAX_SNAPSHOT_STORAGE_BYTES)
        else {
            return;
        };
        match self.store.objects().save_stat_cache(cache, scope, started) {
            Ok(()) => self.usage = usage,
            Err(error) => tracing::warn!(%error, "workspace change stat cache was not saved"),
        }
    }
}

/// What stored objects can occupy for `bytes` of their content, headers included. zlib never spends
/// more than nine bits on a byte, the cost of a fixed Huffman literal, so an eighth covers any
/// content; `OBJECT_OVERHEAD` covers each object's framing.
const fn compressed_bound(bytes: u64) -> u64 {
    bytes.saturating_add(bytes.div_ceil(8))
}

fn byte_count(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// Whether another name shares the file's inode, so a write through one changes the other.
#[cfg(unix)]
fn hard_linked(metadata: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    metadata.nlink() > 1
}

#[cfg(not(unix))]
fn hard_linked(_metadata: &Metadata) -> bool {
    false
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
