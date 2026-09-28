//! Capturing a working tree: the walk that decides what a snapshot covers, and
//! the parallel read of the files the stat cache cannot vouch for.

use std::collections::BTreeMap;
use std::fs::{self, FileType, Metadata};
use std::io;
use std::mem;
use std::num::NonZero;
use std::panic::resume_unwind;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use ignore::{DirEntry, WalkBuilder};
use tracing::{debug, warn};
use workcell::snapshot_store::{
    Content, Entry, EntryKind, FileStamp, Meta, ObjectId, ObjectStore, ROOT_SCOPE, SkipReason,
    Skipped, SkippedPath, SnapshotId, StatCache, Written,
};

use super::storage::abandon_on_error;
use super::{SnapshotError, SnapshotKey, SnapshotLimits, SnapshotStore, key_name};

const LIMIT_MAX_FILES: &str = "max_files";
const LIMIT_MAX_BYTES: &str = "max_bytes";
const ROOT_IS_FILESYSTEM_ROOT: &str = "it is the filesystem root";
const ROOT_IS_HOME: &str = "it is the home directory";
const GIT_DIR: &str = ".git";
#[cfg(unix)]
const OWNER_EXECUTE: u32 = 0o100;

/// Where the time in one capture went, and how much of the tree it had to read.
#[derive(Debug, Default)]
pub(super) struct CaptureStats {
    files: u64,
    bytes: u64,
    /// Files read because the stat cache could not vouch for them.
    hashed: u64,
    objects_written: u64,
    /// Uncompressed size of the blobs this capture added to the store.
    new_bytes: u64,
    walk: Duration,
    read: Duration,
    build: Duration,
    retention: Duration,
}

/// A path the capture will record, stamped before anything reads it.
struct Walked {
    path: String,
    absolute: PathBuf,
    kind: EntryKind,
    stamp: FileStamp,
    bytes: u64,
}

#[derive(Default)]
struct WalkedTree {
    files: Vec<Walked>,
    bytes: u64,
    skips: Skips,
}

/// Everything a capture saw and left out. A pruned path is one a restore must
/// leave alone; a name that is not UTF-8 cannot even be named, so it is only
/// counted, and no snapshot ever holds it either.
#[derive(Default)]
struct Skips {
    pruned: Vec<SkippedPath>,
    counts: BTreeMap<SkipReason, u32>,
}

impl Skips {
    fn prune(&mut self, path: String, reason: SkipReason) {
        self.count(reason);
        self.pruned.push(SkippedPath { path, reason });
    }

    fn prune_absolute(&mut self, root: &Path, absolute: &Path, reason: SkipReason) {
        match relative_path(root, absolute) {
            Some(path) => self.prune(path, reason),
            None => self.count(SkipReason::Unrepresentable),
        }
    }

    fn count(&mut self, reason: SkipReason) {
        let count = self.counts.entry(reason).or_default();
        *count = count.saturating_add(1);
    }
}

enum Read {
    Stored(Written, u64),
    Vanished,
    Unreadable,
}

impl SnapshotStore {
    /// Captures `root` as `key`: walk, read what the stat cache cannot vouch
    /// for, build, and make the objects durable before the pointer and the
    /// cache name them. `started` bounds which stamps the cache may trust.
    ///
    /// Once the pointer is durable the capture stands. Saving the stat cache
    /// only spares the next capture its reads, and retention only keeps the
    /// store near its cap, so a failure in either is logged, not returned.
    pub(super) fn capture_to(
        &self,
        root: &Path,
        key: SnapshotKey,
        started: SystemTime,
    ) -> Result<(SnapshotId, CaptureStats), SnapshotError> {
        let walk_start = Instant::now();
        let walked = self.walk_working_tree(root)?;
        let mut stats = CaptureStats {
            walk: walk_start.elapsed(),
            ..CaptureStats::default()
        };
        let repository = self.open_repository()?;
        let mut cache = repository.stat_cache();
        let written = write_snapshot(&repository, walked, &mut cache, &mut stats);
        let (id, pruned) = abandon_on_error(&repository, written)?;

        let naming_start = Instant::now();
        self.write_pointer(key, id)?;
        if let Err(error) = repository.save_stat_cache(cache, ROOT_SCOPE, started) {
            warn!(
                snapshot = key_name(key),
                store = %repository.dir().display(),
                %error,
                "could not save the snapshot stat cache"
            );
        }
        stats.build += naming_start.elapsed();

        let retention_start = Instant::now();
        if let Err(error) = self.enforce_cap(&repository, stats.new_bytes, key) {
            warn!(
                snapshot = key_name(key),
                store = %repository.dir().display(),
                cap_bytes = self.cap_bytes,
                %error,
                "could not keep the snapshot store within its cap"
            );
        }
        stats.retention = retention_start.elapsed();

        debug!(
            snapshot = key_name(key),
            files = stats.files,
            bytes = stats.bytes,
            hashed = stats.hashed,
            objects_written = stats.objects_written,
            new_bytes = stats.new_bytes,
            pruned,
            walk_us = micros(stats.walk),
            read_us = micros(stats.read),
            build_us = micros(stats.build),
            retention_us = micros(stats.retention),
            total_us = micros(stats.walk + stats.read + stats.build + stats.retention),
            "workspace snapshot"
        );
        Ok((id, stats))
    }

    /// Enumerates what a capture would read, and refuses before reading any of
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
        let excluded = [
            self.sessions_dir(),
            self.repository.parent().unwrap_or(&self.repository),
        ]
        .map(resolved);
        let root_device = device(&fs::metadata(root)?);
        let skips = Arc::new(Mutex::new(Skips::default()));
        let mut builder = WalkBuilder::new(root);
        // `require_git(false)`: a `.gitignore` without a repository around it is
        // still the user saying which paths are disposable, and every other walk
        // in Caudra honours it. The alternative was no filtering at all outside a
        // worktree, which is how a session rooted at `$HOME` came to hash it.
        // A mount inside the workspace is not part of the project either. Where
        // devices can be compared the filter prunes it, because walkdir's own
        // `same_file_system` yields such a directory without entering it, and
        // refusing it then skips the rest of its parent instead.
        builder
            .hidden(false)
            .ignore(true)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .require_git(false)
            .same_file_system(root_device.is_none());
        let filter_root = root.to_path_buf();
        let filter_skips = Arc::clone(&skips);
        builder.filter_entry(move |entry| {
            if entry.depth() == 0 {
                return true;
            }
            if entry.file_name() == GIT_DIR
                || excluded.iter().any(|dir| entry.path().starts_with(dir))
            {
                return false;
            }
            if !entry.file_type().is_some_and(|kind| kind.is_dir()) {
                return true;
            }
            let Some(reason) = boundary(entry, root_device) else {
                return true;
            };
            lock(&filter_skips).prune_absolute(&filter_root, entry.path(), reason);
            false
        });

        let mut walked = WalkedTree::default();
        for result in builder.build() {
            let entry = match result {
                Ok(entry) => entry,
                Err(error) => match error_path(&error) {
                    Some(path) if path != root => {
                        lock(&skips).prune_absolute(root, path, SkipReason::Unreadable);
                        continue;
                    }
                    _ => return Err(io::Error::other(error.to_string()).into()),
                },
            };
            let Some(file_type) = entry.file_type() else {
                continue;
            };
            if entry.depth() == 0 || file_type.is_dir() {
                continue;
            }
            let Some(path) = relative_path(root, entry.path()) else {
                lock(&skips).count(SkipReason::Unrepresentable);
                continue;
            };
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(error)
                    if error
                        .io_error()
                        .is_some_and(|error| error.kind() == io::ErrorKind::NotFound) =>
                {
                    continue;
                }
                Err(error) => return Err(io::Error::other(error.to_string()).into()),
            };
            let Some(kind) = entry_kind(file_type, &metadata) else {
                lock(&skips).prune(path, SkipReason::Special);
                continue;
            };
            // Pruned rather than refused: one oversized blob beside a normal
            // project should not cost the project its revert, and a restore
            // leaves a pruned path alone. It is also what keeps the whole-file
            // read of every captured file bounded.
            if metadata.len() > self.limits.max_file_bytes {
                lock(&skips).prune(path, SkipReason::Oversized);
                continue;
            }
            walked.bytes += metadata.len();
            walked.files.push(Walked {
                path,
                kind,
                stamp: FileStamp::of(&metadata),
                bytes: metadata.len(),
                absolute: entry.into_path(),
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
        walked.skips = mem::take(&mut *lock(&skips));
        Ok(walked)
    }
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

/// Stores what the stat cache cannot vouch for, builds the snapshot, and syncs,
/// so that everything the pointer is about to name is durable. Answers the
/// snapshot and how many paths it records as pruned.
fn write_snapshot(
    repository: &ObjectStore,
    walked: WalkedTree,
    cache: &mut StatCache,
    stats: &mut CaptureStats,
) -> Result<(SnapshotId, usize), SnapshotError> {
    let read_start = Instant::now();
    let WalkedTree {
        files, mut skips, ..
    } = walked;
    let mut oids: Vec<Option<ObjectId>> = files
        .iter()
        .map(|file| {
            cache
                .lookup(&file.path, &file.stamp)
                .filter(|oid| repository.contains(oid))
        })
        .collect();
    let misses: Vec<usize> = (0..files.len())
        .filter(|index| oids[*index].is_none())
        .collect();
    for (index, read) in read_misses(repository, &files, &misses)? {
        match read {
            Read::Stored(written, bytes) => {
                oids[index] = Some(written.oid);
                stats.hashed += 1;
                if written.new {
                    stats.objects_written += 1;
                    stats.new_bytes += bytes;
                }
            }
            Read::Vanished => {}
            Read::Unreadable => skips.prune(files[index].path.clone(), SkipReason::Unreadable),
        }
    }
    let mut entries = Vec::with_capacity(files.len());
    for (file, oid) in files.into_iter().zip(oids) {
        let Some(oid) = oid else { continue };
        stats.bytes += file.bytes;
        cache.record(file.path.clone(), file.stamp, oid);
        entries.push(Entry {
            path: file.path,
            content: Content {
                kind: file.kind,
                oid,
            },
        });
    }
    stats.files = entries.len() as u64;
    stats.read = read_start.elapsed();

    let build_start = Instant::now();
    entries.sort_unstable_by(|left, right| left.path.cmp(&right.path));
    skips
        .pruned
        .sort_unstable_by(|left, right| left.path.cmp(&right.path));
    let pruned = skips.pruned.len();
    let meta = Meta {
        scope: ROOT_SCOPE.to_owned(),
        pruned: skips.pruned,
        exclusions: Vec::new(),
        skipped: Skipped {
            counts: skips.counts,
            samples: Vec::new(),
        },
        file_count: stats.files,
        total_bytes: stats.bytes,
    };
    let id = repository.build(&entries, &meta)?;
    repository.sync()?;
    stats.build = build_start.elapsed();
    Ok((id, pruned))
}

/// Reading is the bulk of any capture the stat cache cannot skip, and every
/// file is independent. Workers claim one file at a time rather than splitting
/// the list up front, because a working tree is a handful of large files among
/// many small ones and any fixed split leaves everyone waiting on whoever drew
/// the largest.
fn read_misses(
    repository: &ObjectStore,
    files: &[Walked],
    misses: &[usize],
) -> Result<Vec<(usize, Read)>, SnapshotError> {
    if misses.is_empty() {
        return Ok(Vec::new());
    }
    let workers = thread::available_parallelism()
        .map_or(1, NonZero::get)
        .min(misses.len());
    let next = AtomicUsize::new(0);
    thread::scope(|scope| {
        let workers: Vec<_> = (0..workers)
            .map(|_| scope.spawn(|| read_claimed(repository, files, misses, &next)))
            .collect();
        let mut reads = Vec::with_capacity(misses.len());
        for worker in workers {
            reads.extend(worker.join().unwrap_or_else(|panic| resume_unwind(panic))?);
        }
        Ok(reads)
    })
}

fn read_claimed(
    repository: &ObjectStore,
    files: &[Walked],
    misses: &[usize],
    next: &AtomicUsize,
) -> Result<Vec<(usize, Read)>, SnapshotError> {
    let mut reads = Vec::new();
    while let Some(&index) = misses.get(next.fetch_add(1, Ordering::Relaxed)) {
        let file = &files[index];
        // The walk and the read are separate passes over a working tree
        // nothing has frozen, so a file can be gone by the time its turn
        // comes. It is then simply not in the snapshot; failing the whole
        // capture would let any concurrent build or editor break it.
        let content = match file.kind {
            EntryKind::Symlink => link_target(&file.absolute),
            EntryKind::File | EntryKind::Executable => fs::read(&file.absolute),
        };
        let read = match content {
            Ok(content) => Read::Stored(repository.write_blob(&content)?, content.len() as u64),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Read::Vanished,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Read::Unreadable,
            Err(error) => return Err(error.into()),
        };
        reads.push((index, read));
    }
    Ok(reads)
}

/// Why a directory is a boundary the walk must not cross.
///
/// A directory carrying its own `.git` is a different repository, and this
/// snapshot describes one worktree. Git agrees: a nested repository adds
/// nothing to the parent's status, whether it is a submodule (`.git` file) or
/// an unregistered clone (`.git` directory). Descending anyway meant a
/// workspace whose 357 tracked files sat beside a 928k-file data repository
/// read all 7 GB of it on every capture.
fn boundary(entry: &DirEntry, root_device: Option<u64>) -> Option<SkipReason> {
    if entry.path().join(GIT_DIR).exists() {
        return Some(SkipReason::NestedRepository);
    }
    let entry_device = entry.metadata().ok().and_then(|metadata| device(&metadata));
    root_device
        .zip(entry_device)
        .is_some_and(|(root, entry)| root != entry)
        .then_some(SkipReason::Mount)
}

fn entry_kind(file_type: FileType, metadata: &Metadata) -> Option<EntryKind> {
    if file_type.is_file() {
        Some(if is_executable(metadata) {
            EntryKind::Executable
        } else {
            EntryKind::File
        })
    } else if cfg!(unix) && file_type.is_symlink() {
        Some(EntryKind::Symlink)
    } else {
        None
    }
}

/// Git's notion of executable: the owner may run it.
pub(super) fn is_executable(metadata: &Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & OWNER_EXECUTE != 0
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        false
    }
}

/// The raw bytes a symlink points at, which its blob holds.
pub(super) fn link_target(path: &Path) -> io::Result<Vec<u8>> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(fs::read_link(path)?.into_os_string().into_vec())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(io::ErrorKind::Unsupported.into())
    }
}

fn device(metadata: &Metadata) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(metadata.dev())
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}

/// `absolute` below `root`, `/`-separated as a snapshot names it, or `None`
/// for the root itself or a name that is not UTF-8.
fn relative_path(root: &Path, absolute: &Path) -> Option<String> {
    let mut path = String::new();
    for component in absolute.strip_prefix(root).ok()?.components() {
        let Component::Normal(name) = component else {
            return None;
        };
        if !path.is_empty() {
            path.push('/');
        }
        path.push_str(name.to_str()?);
    }
    (!path.is_empty()).then_some(path)
}

fn error_path(error: &ignore::Error) -> Option<&Path> {
    match error {
        ignore::Error::WithPath { path, .. } => Some(path),
        ignore::Error::WithDepth { err, .. } | ignore::Error::WithLineNumber { err, .. } => {
            error_path(err)
        }
        _ => None,
    }
}

fn lock(skips: &Mutex<Skips>) -> MutexGuard<'_, Skips> {
    skips.lock().unwrap_or_else(PoisonError::into_inner)
}

fn resolved(path: &Path) -> PathBuf {
    fs::canonicalize(path)
        .or_else(|_| std::path::absolute(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

/// Roots no project lives at, refused before a single directory is read. A home
/// directory is never a project root, and these are the cases no ignore file
/// would ever catch, because neither has one.
///
/// Deliberately not here: a root that is a filesystem mount point. A
/// bind-mounted project root is how a container normally sees its project, so
/// refusing one would disable revert for every such setup. The walk already
/// stays on the root's device, and the size ceiling covers what is left.
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

/// Microseconds, because the phases of a capture of an unchanged tree are
/// mostly well under a millisecond each.
fn micros(duration: Duration) -> u64 {
    duration.as_micros() as u64
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use test_case::test_case;
    use workcell::snapshot_store::{RACY_MARGIN, blob_id};

    use super::*;
    use crate::snapshots::tests::{
        ALPHA, BETA, OTHER_SESSION, SESSION, content, id, object_count, object_path, paths,
        session_store, setup, store_in, write,
    };

    const UNCHANGED_MSG: &str = "a capture of an unchanged tree must read no file";
    const CHANGED_MSG: &str = "a capture must read exactly the files that changed";
    const SHARED_MSG: &str =
        "a second session's first capture reuses the objects and stat cache of the first";
    const SYMLINK_MSG: &str = "a symlink is captured as the link itself, never followed";
    const EXECUTABLE_MSG: &str = "a file is executable when its owner may run it, as git decides";
    const NESTED_REPO_MSG: &str = "a nested repository belongs to itself, not to this worktree";
    const NON_GIT_IGNORE_MSG: &str =
        "an ignore file states intent with or without a repository around it";
    const REFUSAL_MSG: &str = "a tree over the budget is refused, not captured";
    const UNREAD_MSG: &str = "a refusal must cost metadata only, never a read";
    const UNSUPPORTED_ROOT_MSG: &str = "a root no project lives at is refused without a walk";
    const DANGLING_TARGET: &str = "missing-target";
    const UNSAVED_CACHE_MSG: &str = "a capture stands once its pointer is durable, cache or not";
    const UNSAVED_PREMISE_MSG: &str = "a directory in place of the stat cache must stop it saving";
    const MISSING_OBJECT_MSG: &str =
        "a file whose cached object is gone from the store is read and stored again";
    /// The stat cache's file in the store.
    const STAT_INDEX: &str = "stat-index";

    /// Captures with a start the stat cache trusts every file written so far
    /// against. A ctime cannot be moved into the past, so the start moves into
    /// the future instead.
    fn try_capture_settled(
        store: &SnapshotStore,
        root: &Path,
        key: SnapshotKey,
    ) -> Result<(SnapshotId, CaptureStats), SnapshotError> {
        let root = store.bind_root(root).unwrap();
        store.capture_to(&root, key, SystemTime::now() + RACY_MARGIN * 2)
    }

    fn capture_settled(store: &SnapshotStore, root: &Path, key: SnapshotKey) -> CaptureStats {
        try_capture_settled(store, root, key).unwrap().1
    }

    /// The stat cache only spares later captures their reads, so failing to
    /// save it must not cost a capture whose pointer is already durable. A
    /// directory in place of its file fails the save whoever runs the test.
    #[test]
    fn a_capture_whose_stat_cache_cannot_be_saved_still_stands() {
        let (temp, root) = setup();
        write(&root, "file.txt", ALPHA);
        let store = store_in(&temp);
        fs::create_dir_all(store.repository.join(STAT_INDEX)).unwrap();

        let captured = try_capture_settled(&store, &root, SnapshotKey::SessionStart);

        assert!(captured.is_ok(), "{UNSAVED_CACHE_MSG}: {captured:?}");
        assert_eq!(
            paths(&store, SnapshotKey::SessionStart),
            ["file.txt"],
            "{UNSAVED_CACHE_MSG}"
        );
        let again = capture_settled(&store, &root, SnapshotKey::Checkpoint(id(1)));
        assert_eq!(again.hashed, 1, "{UNSAVED_PREMISE_MSG}");
    }

    /// The walk lists what to read and the read comes after, with nothing
    /// holding the tree still in between. A build or an editor clearing a file
    /// in that window must cost the capture that one file, not all of it.
    #[test]
    fn a_file_deleted_between_the_walk_and_the_read_is_left_out() {
        let (temp, root) = setup();
        write(&root, "kept.txt", ALPHA);
        write(&root, "vanishes.txt", BETA);
        let store = store_in(&temp);
        let walked = store.walk_working_tree(&root).unwrap();
        fs::remove_file(root.join("vanishes.txt")).unwrap();

        let reads = read_misses(&store.open_repository().unwrap(), &walked.files, &[0, 1]).unwrap();

        let stored: Vec<&str> = reads
            .iter()
            .filter(|(_, read)| matches!(read, Read::Stored(..)))
            .map(|(index, _)| walked.files[*index].path.as_str())
            .collect();
        assert_eq!(stored, ["kept.txt"]);
    }

    #[test]
    fn only_files_the_stat_cache_cannot_vouch_for_are_read() {
        let (temp, root) = setup();
        write(&root, "same.txt", ALPHA);
        write(&root, "dir/changes.txt", ALPHA);
        let store = store_in(&temp);
        capture_settled(&store, &root, SnapshotKey::SessionStart);

        let unchanged = capture_settled(&store, &root, SnapshotKey::Checkpoint(id(1)));
        assert_eq!(unchanged.hashed, 0, "{UNCHANGED_MSG}");
        assert_eq!(unchanged.objects_written, 0, "{UNCHANGED_MSG}");

        write(&root, "dir/changes.txt", BETA);
        let changed = capture_settled(&store, &root, SnapshotKey::Checkpoint(id(2)));
        assert_eq!(changed.hashed, 1, "{CHANGED_MSG}");
        assert_eq!(changed.objects_written, 1, "{CHANGED_MSG}");
    }

    #[test]
    fn a_file_whose_cached_object_is_gone_is_read_again() {
        let (temp, root) = setup();
        write(&root, "file.txt", ALPHA);
        let store = store_in(&temp);
        capture_settled(&store, &root, SnapshotKey::SessionStart);
        let blob = blob_id(ALPHA.as_bytes()).unwrap();
        fs::remove_file(object_path(&store, &blob)).unwrap();

        let again = capture_settled(&store, &root, SnapshotKey::Checkpoint(id(1)));

        assert_eq!(again.hashed, 1, "{MISSING_OBJECT_MSG}");
        assert!(
            store.open_repository().unwrap().contains(&blob),
            "{MISSING_OBJECT_MSG}"
        );
    }

    #[test]
    fn a_second_sessions_first_capture_writes_no_objects() {
        let (temp, root) = setup();
        write(&root, "file.txt", ALPHA);
        write(&root, "dir/nested.txt", BETA);
        let first = session_store(&temp, SESSION);
        capture_settled(&first, &root, SnapshotKey::SessionStart);
        let objects = object_count(&first);

        let second = session_store(&temp, OTHER_SESSION);
        let stats = capture_settled(&second, &root, SnapshotKey::SessionStart);

        assert_eq!(stats.hashed, 0, "{SHARED_MSG}");
        assert_eq!(object_count(&second), objects, "{SHARED_MSG}");
        assert_eq!(
            second.snapshot_id(SnapshotKey::SessionStart).unwrap(),
            first.snapshot_id(SnapshotKey::SessionStart).unwrap(),
            "{SHARED_MSG}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_captured_as_its_target_and_never_followed() {
        use std::os::unix::fs::symlink;

        let (temp, root) = setup();
        let outside = temp.path().join("outside");
        write(&outside, "secret.txt", ALPHA);
        symlink(&outside, root.join("linked")).unwrap();
        symlink(DANGLING_TARGET, root.join("dangling")).unwrap();
        let store = store_in(&temp);
        store.snapshot_session_start(&root).unwrap();

        assert_eq!(
            paths(&store, SnapshotKey::SessionStart),
            ["dangling", "linked"],
            "{SYMLINK_MSG}"
        );
        assert_eq!(
            content(&store, SnapshotKey::SessionStart, "linked"),
            Some(Content {
                kind: EntryKind::Symlink,
                oid: blob_id(outside.as_os_str().as_encoded_bytes()).unwrap(),
            }),
            "{SYMLINK_MSG}"
        );
    }

    #[cfg(unix)]
    #[test_case(0o755, EntryKind::Executable ; "everyone_may_run_it")]
    #[test_case(0o744, EntryKind::Executable ; "only_the_owner_may_run_it")]
    #[test_case(0o655, EntryKind::File       ; "everyone_but_the_owner_may_run_it")]
    #[test_case(0o644, EntryKind::File       ; "nobody_may_run_it")]
    fn the_owner_execute_bit_decides_whether_a_file_is_executable(mode: u32, kind: EntryKind) {
        use std::os::unix::fs::PermissionsExt;

        let (temp, root) = setup();
        write(&root, "tool", ALPHA);
        fs::set_permissions(root.join("tool"), fs::Permissions::from_mode(mode)).unwrap();
        let store = store_in(&temp);
        store.snapshot_session_start(&root).unwrap();

        assert_eq!(
            content(&store, SnapshotKey::SessionStart, "tool").map(|content| content.kind),
            Some(kind),
            "{EXECUTABLE_MSG}"
        );
    }

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
        let (temp, root) = setup();
        if parent_is_repository {
            fs::create_dir(root.join(GIT_DIR)).unwrap();
        }
        write(&root, "kept.txt", ALPHA);
        if nested_git_is_dir {
            fs::create_dir_all(root.join("training-data").join(GIT_DIR)).unwrap();
        } else {
            write(&root, "training-data/.git", "gitdir: ../.git/modules/td");
        }
        write(&root, "training-data/huge.bin", BETA);
        let store = store_in(&temp);
        store.snapshot_session_start(&root).unwrap();

        assert_eq!(
            paths(&store, SnapshotKey::SessionStart),
            ["kept.txt"],
            "{NESTED_REPO_MSG}"
        );
    }

    #[test]
    fn git_walk_honors_ignore_rules() {
        let (temp, root) = setup();
        write(&root, ".git/config", ALPHA);
        write(&root, ".gitignore", "*.ignored\n");
        write(&root, "kept.txt", ALPHA);
        write(&root, "secret.ignored", BETA);
        let store = store_in(&temp);
        store.snapshot_session_start(&root).unwrap();

        assert_eq!(
            paths(&store, SnapshotKey::SessionStart),
            [".gitignore", "kept.txt"]
        );
    }

    /// Without this, a directory that is not a repository had no filtering at
    /// all, which is how a session rooted at a home directory came to hash it.
    #[test_case(".gitignore" ; "gitignore")]
    #[test_case(".ignore"    ; "ignore")]
    fn a_walk_outside_a_repository_honors_ignore_files(ignore_file: &str) {
        let (temp, root) = setup();
        write(&root, ignore_file, "*.ignored\nheavy/\n");
        write(&root, "kept.txt", ALPHA);
        write(&root, "secret.ignored", BETA);
        write(&root, "heavy/blob.bin", BETA);
        let store = store_in(&temp);
        store.snapshot_session_start(&root).unwrap();

        assert_eq!(
            paths(&store, SnapshotKey::SessionStart),
            [ignore_file, "kept.txt"],
            "{NON_GIT_IGNORE_MSG}"
        );
    }

    /// The point of deciding from the walk is that the expensive half never
    /// runs: a refusal that had already read the tree into the store would be
    /// no cheaper than capturing it.
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
    fn a_tree_over_the_budget_is_refused_before_anything_is_read(
        limits: SnapshotLimits,
        expected_limit_name: &str,
        expected_limit: u64,
    ) {
        let (temp, root) = setup();
        write(&root, "one.txt", ALPHA);
        write(&root, "two.txt", BETA);
        let store = store_in(&temp).with_limits(limits);

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
        assert_eq!(object_count(&store), 0, "{UNREAD_MSG}");
        assert!(!store.has_session_start(), "{REFUSAL_MSG}");
    }

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
        let store = store_in(&temp);

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
