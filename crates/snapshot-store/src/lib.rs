#![forbid(unsafe_code)]

//! Workspace snapshots stored as git objects: content-addressed, zlib-compressed and structurally
//! shared through trees, so an unchanged directory costs nothing in a later snapshot.
//!
//! Git is only the storage format. Nothing here reads git configuration, runs filters or hooks,
//! checks out, or packs. What gix leaves out is this crate's own. A write stages its object under
//! a temporary name, and [`ObjectStore::sync`] flushes it before giving it its name, so an object
//! that exists is complete and a later write may reuse it. Every read is verified against its id,
//! and collection is a mark-and-sweep over loose objects. Callers own locking: every writer and
//! every collection must hold the same caller lock, which is why collection needs no grace period.

mod collect;
mod meta;
mod stat_cache;
mod tree;

use std::{
    collections::{BTreeSet, HashMap},
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Write as _},
    mem,
    path::{Path, PathBuf},
    process,
    str::FromStr,
    sync::{
        Mutex, MutexGuard, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};

use gix::{
    hash::Kind as HashKind,
    objs::{Kind, encode::loose_header},
    odb::loose,
    zlib::{Compression, stream::deflate},
};

pub use collect::{GarbagePlan, Usage};
pub use gix::hash::ObjectId;
pub use meta::{Meta, ROOT_SCOPE, SkipReason, Skipped, SkippedPath, within};
pub use stat_cache::{FileStamp, RACY_MARGIN, StatCache};
pub use tree::{Change, Changes, Content, Entry, EntryKind};

const HASH_KIND: HashKind = HashKind::Sha1;
const OBJECTS: &str = "objects";
const REFS: &str = "refs";
const HEAD: &str = "HEAD";
const CONFIG: &str = "config";
/// Just enough for `git --git-dir=<store>` to accept the directory, so any snapshot can be
/// inspected with `ls-tree` or `diff`. No ref is ever written, so `git gc` or `git prune` would
/// find every object unreachable and must never run there.
const HEAD_CONTENT: &[u8] = b"ref: refs/heads/snapshots\n";
const CONFIG_CONTENT: &[u8] = b"[core]\n\trepositoryformatversion = 0\n\tbare = true\n";
/// Git writes loose objects at level 1, but zlib-rs spends level 1 on fixed Huffman codes with no
/// stored fallback: incompressible content grows by up to an eighth. From level 2 it grows by
/// 0.03%, and level 6 keeps source trees about 30% smaller than level 1 for a similar write cost.
const COMPRESSION: Compression = Compression::DEFAULT;
/// A new object waits in `objects/` under this prefix until a sync flushes it and gives it its
/// name. Collection deletes what a crash left there.
pub(crate) const TEMPORARY_PREFIX: &str = ".tmp";
/// Each flush mostly waits on the device, and the filesystem commits concurrent flushes together:
/// 2,000 small files took 0.97 s to flush from 8 threads and 0.43 s from 64.
const SYNC_WORKERS: usize = 64;
#[cfg(unix)]
const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
#[cfg(unix)]
const PRIVATE_FILE_MODE: u32 = 0o600;
/// Read-only, as git writes objects.
#[cfg(unix)]
const OBJECT_MODE: u32 = 0o444;

static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("snapshot store I/O failed")]
    Io(#[from] io::Error),
    #[error("snapshot object {0} is missing")]
    Missing(ObjectId),
    #[error("snapshot object {0} is corrupt")]
    Corrupt(ObjectId),
    #[error("content hashes to a known SHA-1 collision")]
    Collision,
    #[error("invalid snapshot input: {0}")]
    InvalidInput(&'static str),
    #[error("snapshot scopes are disjoint")]
    DisjointScopes,
}

#[derive(Clone, Debug)]
pub struct StoreOptions {
    /// Owner-only directories and files, for stores other local users must not read.
    pub private: bool,
    /// Refuses to inflate any object larger than this, so a damaged header cannot force a huge
    /// allocation. `None` trusts the store.
    pub max_object_bytes: Option<usize>,
}

/// The id of a snapshot: a tree holding the `files` tree and the `meta` blob, so equal content
/// and metadata always produce the same id.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SnapshotId(ObjectId);

impl SnapshotId {
    #[must_use]
    pub fn oid(&self) -> ObjectId {
        self.0
    }
}

impl fmt::Display for SnapshotId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for SnapshotId {
    type Err = StoreError;

    fn from_str(hex: &str) -> Result<Self, Self::Err> {
        parse_oid(hex)
            .map(Self)
            .ok_or(StoreError::InvalidInput("snapshot id"))
    }
}

/// Parses a full lowercase hex object id, the only form this crate prints.
#[must_use]
pub fn parse_oid(hex: &str) -> Option<ObjectId> {
    (hex.len() == HASH_KIND.len_in_hex()
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
    .then(|| ObjectId::from_hex(hex.as_bytes()).ok())
    .flatten()
}

/// The id `data` has as a blob, without storing it.
pub fn blob_id(data: &[u8]) -> Result<ObjectId, StoreError> {
    gix::objs::compute_hash(HASH_KIND, Kind::Blob, data).map_err(|_| StoreError::Collision)
}

/// The outcome of storing one object.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Written {
    pub oid: ObjectId,
    /// Whether this write staged the object rather than finding it stored or staged.
    pub new: bool,
}

/// A store handle. It writes objects itself rather than through gix, which renames an object into
/// place before anything flushes it: a crash could then leave an empty object under its id, and
/// every later write of that content would reuse it.
pub struct ObjectStore {
    dir: PathBuf,
    loose: loose::Store,
    private: bool,
    pending: Mutex<Pending>,
}

/// What this handle has written and not yet made durable.
#[derive(Default)]
struct Pending {
    /// Each object staged under a temporary name, unreadable until a sync names it.
    staged: HashMap<ObjectId, PathBuf>,
    /// Fan-out directories the next sync flushes besides those of staged objects: those naming
    /// objects this handle found stored, whose writer may have crashed before flushing them, and
    /// those a failed sync named objects in.
    fan_outs: BTreeSet<u8>,
}

impl ObjectStore {
    /// Opens the store in `dir`, creating it when absent. The directory is a bare git repository
    /// that git never needs to be configured for; nothing reads system, global or environment
    /// configuration.
    pub fn open(dir: &Path, options: StoreOptions) -> Result<Self, StoreError> {
        let objects = dir.join(OBJECTS);
        create_dir(&objects, options.private)?;
        create_dir(&dir.join(REFS), options.private)?;
        create_file(&dir.join(HEAD), HEAD_CONTENT, options.private)?;
        create_file(&dir.join(CONFIG), CONFIG_CONTENT, options.private)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            loose: loose::Store::at_opts(
                objects,
                HASH_KIND,
                loose::Options {
                    alloc_limit_bytes: options.max_object_bytes,
                    ..loose::Options::default()
                },
            ),
            private: options.private,
            pending: Mutex::default(),
        })
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Stages `data` as a blob. Safe to call from many threads at once; the blob becomes readable
    /// and durable once [`Self::sync`] names it.
    pub fn write_blob(&self, data: &[u8]) -> Result<Written, StoreError> {
        self.write_object(Kind::Blob, data)
    }

    /// Reads a blob, verifying its bytes hash to `oid`.
    pub fn read_blob(&self, oid: &ObjectId) -> Result<Vec<u8>, StoreError> {
        self.read_object(oid, Kind::Blob)
    }

    /// The size of a blob from its header alone, without inflating or verifying its content.
    pub fn blob_size(&self, oid: &ObjectId) -> Result<u64, StoreError> {
        match self.loose.try_header(oid) {
            Ok(Some((size, Kind::Blob))) => Ok(size),
            Ok(None) => Err(StoreError::Missing(*oid)),
            Ok(Some(_)) | Err(_) => Err(StoreError::Corrupt(*oid)),
        }
    }

    /// Whether a snapshot built through this handle may name `oid`: it is stored, or staged here.
    /// Naming a stored object has the next sync flush its directory, which its writer may have
    /// crashed before doing.
    #[must_use]
    pub fn contains(&self, oid: &ObjectId) -> bool {
        if self.pending().staged.contains_key(oid) {
            return true;
        }
        let stored = self.loose.contains(oid);
        if stored {
            self.pending().fan_outs.insert(oid.first_byte());
        }
        stored
    }

    /// Flushes every object staged since the last sync, names it, and flushes the directories
    /// naming it. Call it before anything outside the store names those objects. A failed sync
    /// deletes what it did not name, so those objects must be written again.
    pub fn sync(&self) -> Result<(), StoreError> {
        let (staged, mut fan_outs) = {
            let mut pending = self.pending();
            (
                mem::take(&mut pending.staged),
                mem::take(&mut pending.fan_outs),
            )
        };
        fan_outs.extend(staged.keys().map(|oid| oid.first_byte()));
        let published = self.publish(&staged, &fan_outs);
        if published.is_err() {
            let _ = discard(staged.values());
            self.pending().fan_outs.extend(fan_outs);
        }
        published
    }

    /// Deletes every object staged since the last sync, for an operation that failed before
    /// naming them. Dropping the handle does the same.
    pub fn abandon(&self) -> Result<(), StoreError> {
        let staged = mem::take(&mut self.pending().staged);
        discard(staged.values())
    }

    fn publish(
        &self,
        staged: &HashMap<ObjectId, PathBuf>,
        fan_outs: &BTreeSet<u8>,
    ) -> Result<(), StoreError> {
        if fan_outs.is_empty() {
            return Ok(());
        }
        let objects = self.loose.path();
        let mut directories: Vec<PathBuf> = fan_outs
            .iter()
            .map(|byte| objects.join(fan_out(*byte)))
            .collect();
        for directory in &directories {
            create_dir(directory, self.private)?;
        }
        let names: Vec<(&PathBuf, PathBuf)> = staged
            .iter()
            .map(|(oid, temporary)| (temporary, self.loose.object_path(oid)))
            .collect();
        for_each_parallel(&names, |(temporary, name)| {
            sync_file(temporary)?;
            Ok(fs::rename(temporary, name)?)
        })?;
        directories.push(objects.to_path_buf());
        for_each_parallel(&directories, |directory| sync_directory(directory))
    }

    fn pending(&self) -> MutexGuard<'_, Pending> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn write_object(&self, kind: Kind, data: &[u8]) -> Result<Written, StoreError> {
        let oid =
            gix::objs::compute_hash(HASH_KIND, kind, data).map_err(|_| StoreError::Collision)?;
        if self.contains(&oid) {
            return Ok(Written { oid, new: false });
        }
        let temporary = self.stage(kind, data)?;
        let Some(duplicate) = self.pending().staged.insert(oid, temporary) else {
            return Ok(Written { oid, new: true });
        };
        remove(&duplicate)?;
        Ok(Written { oid, new: false })
    }

    /// Writes an object in git's loose format under a new temporary name, without flushing it.
    fn stage(&self, kind: Kind, data: &[u8]) -> Result<PathBuf, StoreError> {
        let (file, temporary) = self.create_temporary()?;
        let mut deflate = deflate::Write::new(file, COMPRESSION);
        let written = deflate
            .write_all(&loose_header(kind, data.len() as u64))
            .and_then(|()| deflate.write_all(data))
            .and_then(|()| deflate.flush());
        drop(deflate);
        if let Err(error) = written {
            let _ = remove(&temporary);
            return Err(error.into());
        }
        Ok(temporary)
    }

    fn create_temporary(&self) -> Result<(File, PathBuf), StoreError> {
        loop {
            let temporary = self.loose.path().join(format!(
                "{TEMPORARY_PREFIX}{}-{}",
                process::id(),
                NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed)
            ));
            match object_options()
                .write(true)
                .create_new(true)
                .open(&temporary)
            {
                Ok(file) => return Ok((file, temporary)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn read_object(&self, oid: &ObjectId, kind: Kind) -> Result<Vec<u8>, StoreError> {
        let mut buffer = Vec::new();
        let data = self
            .loose
            .try_find(oid, &mut buffer)
            .map_err(|_| StoreError::Corrupt(*oid))?
            .ok_or(StoreError::Missing(*oid))?;
        if data.kind != kind || data.verify_checksum(oid).is_err() {
            return Err(StoreError::Corrupt(*oid));
        }
        Ok(buffer)
    }

    /// Replaces `name` in the store directory durably: what it names must survive a crash as long
    /// as the objects it refers to.
    fn replace_file(&self, name: &str, temporary: &str, bytes: &[u8]) -> Result<(), StoreError> {
        let temporary = self.dir.join(temporary);
        let mut file = open_options(self.private)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, self.dir.join(name))?;
        sync_directory(&self.dir)
    }
}

impl Drop for ObjectStore {
    fn drop(&mut self) {
        let _ = self.abandon();
    }
}

fn fan_out(byte: u8) -> String {
    format!("{byte:02x}")
}

fn create_dir(path: &Path, private: bool) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(PRIVATE_DIRECTORY_MODE);
    }
    #[cfg(not(unix))]
    let _ = private;
    builder.create(path)
}

fn create_file(path: &Path, content: &[u8], private: bool) -> io::Result<()> {
    match open_options(private)
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => file.write_all(content),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

fn open_options(private: bool) -> OpenOptions {
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(PRIVATE_FILE_MODE);
    }
    #[cfg(not(unix))]
    let _ = private;
    options
}

fn object_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(OBJECT_MODE);
    }
    options
}

fn remove(path: &Path) -> Result<(), StoreError> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}

/// Deletes staged objects, trying every one and reporting the first failure.
fn discard<'a>(temporaries: impl IntoIterator<Item = &'a PathBuf>) -> Result<(), StoreError> {
    let mut first_failure = None;
    for temporary in temporaries {
        if let Err(error) = remove(temporary) {
            first_failure.get_or_insert(error);
        }
    }
    first_failure.map_or(Ok(()), Err)
}

/// Runs `work` on each item across up to [`SYNC_WORKERS`] threads, because each mostly waits on
/// the device.
fn for_each_parallel<T: Sync>(
    items: &[T],
    work: impl Fn(&T) -> Result<(), StoreError> + Sync,
) -> Result<(), StoreError> {
    if items.len() <= 1 {
        return items.iter().try_for_each(&work);
    }
    thread::scope(|scope| {
        let work = &work;
        let workers: Vec<_> = items
            .chunks(items.len().div_ceil(SYNC_WORKERS))
            .map(|chunk| {
                thread::Builder::new().spawn_scoped(scope, move || chunk.iter().try_for_each(work))
            })
            .collect();
        let mut outcome = Ok(());
        for worker in workers {
            let result = worker.map_err(StoreError::Io).and_then(|handle| {
                handle
                    .join()
                    .unwrap_or_else(|_| Err(io::Error::other("sync worker panicked").into()))
            });
            if outcome.is_ok() {
                outcome = result;
            }
        }
        outcome
    })
}

fn sync_file(path: &Path) -> Result<(), StoreError> {
    Ok(File::open(path)?.sync_all()?)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), StoreError> {
    Ok(File::open(path)?.sync_all()?)
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), StoreError> {
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    pub(crate) const HELLO: &[u8] = b"hello\n";
    /// `git hash-object` of [`HELLO`].
    const HELLO_ID: &str = "ce013625030ba8dba906f756967f9e9ca394464a";
    const INCOMPRESSIBLE_BYTES: usize = 1_024 * 1_024;
    const XORSHIFT_SEED: u64 = 0x9e37_79b9_7f4a_7c15;
    /// Stored deflate blocks cost 5 bytes per 16 KiB, well under a thousandth.
    const STORED_BLOCK_SHARE: usize = 1_024;
    /// The object header and zlib's wrapper.
    const FRAMING_BYTES: usize = 64;
    const UNNAMED: &str = "an object must take its name only once a sync has flushed it";

    pub(crate) fn store(private: bool) -> (TempDir, ObjectStore) {
        let dir = TempDir::new().unwrap();
        let store = ObjectStore::open(&dir.path().join("store"), options(private)).unwrap();
        (dir, store)
    }

    fn options(private: bool) -> StoreOptions {
        StoreOptions {
            private,
            max_object_bytes: None,
        }
    }

    /// What writes left staged in `objects`.
    fn temporaries(objects: &Path) -> Vec<PathBuf> {
        fs::read_dir(objects)
            .unwrap()
            .map(|entry| entry.unwrap())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(TEMPORARY_PREFIX)
            })
            .map(|entry| entry.path())
            .collect()
    }

    #[test]
    fn blobs_are_named_by_their_git_object_id() {
        let (_dir, store) = store(false);
        let written = store.write_blob(HELLO).unwrap();
        assert_eq!(written.oid.to_string(), HELLO_ID);
        assert_eq!(blob_id(HELLO).unwrap(), written.oid);
        assert!(written.new);
        assert!(!store.write_blob(HELLO).unwrap().new);
        store.sync().unwrap();
        assert!(!store.write_blob(HELLO).unwrap().new);
        assert_eq!(store.read_blob(&written.oid).unwrap(), HELLO);
        assert_eq!(store.blob_size(&written.oid).unwrap(), HELLO.len() as u64);
    }

    #[test]
    fn a_written_object_takes_its_name_only_when_synced() {
        let (_dir, store) = store(false);
        let oid = store.write_blob(HELLO).unwrap().oid;
        assert!(store.contains(&oid));
        assert!(!store.loose.contains(&oid), "{UNNAMED}");
        assert!(matches!(store.read_blob(&oid), Err(StoreError::Missing(_))));
        assert_eq!(temporaries(store.loose.path()).len(), 1);
        store.sync().unwrap();
        assert_eq!(store.read_blob(&oid).unwrap(), HELLO);
        assert!(temporaries(store.loose.path()).is_empty());
    }

    #[test]
    fn a_failed_sync_names_nothing_and_leaves_its_directories_to_the_next() {
        let (_dir, store) = store(false);
        let oid = store.write_blob(HELLO).unwrap().oid;
        let fan_out = store
            .loose
            .object_path(&oid)
            .parent()
            .unwrap()
            .to_path_buf();
        fs::write(&fan_out, b"").unwrap();
        assert!(store.sync().is_err());
        assert!(temporaries(store.loose.path()).is_empty());
        assert!(store.pending().fan_outs.contains(&oid.first_byte()));
        fs::remove_file(&fan_out).unwrap();
        store.sync().unwrap();
        assert!(!store.contains(&oid), "{UNNAMED}");
        assert!(store.write_blob(HELLO).unwrap().new);
    }

    #[test]
    fn naming_an_object_another_handle_stored_flushes_its_directory_at_the_next_sync() {
        let (dir, store) = store(false);
        let oid = store.write_blob(HELLO).unwrap().oid;
        store.sync().unwrap();
        let other = ObjectStore::open(&dir.path().join("store"), options(false)).unwrap();
        assert!(!other.write_blob(HELLO).unwrap().new);
        assert!(other.pending().fan_outs.contains(&oid.first_byte()));
        other.sync().unwrap();
        assert!(other.pending().fan_outs.is_empty());
    }

    #[test_case(true ; "abandoned")]
    #[test_case(false ; "dropped with their handle")]
    fn staged_objects_are_deleted_when(abandoned: bool) {
        let (_dir, store) = store(false);
        let oid = store.write_blob(HELLO).unwrap().oid;
        let objects = store.loose.path().to_path_buf();
        if abandoned {
            store.abandon().unwrap();
            assert!(!store.contains(&oid));
        } else {
            drop(store);
        }
        assert!(temporaries(&objects).is_empty());
    }

    #[test]
    fn a_blob_whose_bytes_no_longer_match_its_id_is_corrupt() {
        let (_dir, store) = store(false);
        let original = store.write_blob(HELLO).unwrap().oid;
        let impostor = store.write_blob(b"goodbye\n").unwrap().oid;
        store.sync().unwrap();
        let path = store.loose.object_path(&original);
        fs::remove_file(&path).unwrap();
        fs::copy(store.loose.object_path(&impostor), &path).unwrap();
        assert!(matches!(
            store.read_blob(&original),
            Err(StoreError::Corrupt(oid)) if oid == original
        ));
    }

    #[test]
    fn a_truncated_object_is_corrupt_and_an_absent_one_missing() {
        let (_dir, store) = store(false);
        let oid = store.write_blob(HELLO).unwrap().oid;
        store.sync().unwrap();
        let path = store.loose.object_path(&oid);
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"x").unwrap();
        assert!(matches!(store.read_blob(&oid), Err(StoreError::Corrupt(_))));
        fs::remove_file(&path).unwrap();
        assert!(matches!(store.read_blob(&oid), Err(StoreError::Missing(_))));
    }

    #[test_case(HELLO_ID, true ; "a full lowercase id")]
    #[test_case("CE013625030BA8DBA906F756967F9E9CA394464A", false ; "uppercase")]
    #[test_case("ce013625", false ; "a prefix")]
    #[test_case("ce013625030ba8dba906f756967f9e9ca394464g", false ; "a non hex digit")]
    fn only_full_lowercase_ids_parse(hex: &str, parses: bool) {
        assert_eq!(parse_oid(hex).is_some(), parses);
        assert_eq!(hex.parse::<SnapshotId>().is_ok(), parses);
    }

    #[test]
    fn the_store_is_a_repository_git_can_inspect() {
        let (dir, _store) = store(false);
        let store_dir = dir.path().join("store");
        assert_eq!(fs::read(store_dir.join(HEAD)).unwrap(), HEAD_CONTENT);
        assert_eq!(fs::read(store_dir.join(CONFIG)).unwrap(), CONFIG_CONTENT);
        assert!(store_dir.join(REFS).is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn a_private_store_is_owner_only_down_to_its_fan_out_directories() {
        use std::os::unix::fs::PermissionsExt;

        let (dir, store) = store(true);
        let oid = store.write_blob(HELLO).unwrap().oid;
        store.sync().unwrap();
        let store_dir = dir.path().join("store");
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        for directory in [
            store_dir.clone(),
            store_dir.join(OBJECTS),
            store_dir.join(REFS),
            store
                .loose
                .object_path(&oid)
                .parent()
                .unwrap()
                .to_path_buf(),
        ] {
            assert_eq!(mode(&directory), PRIVATE_DIRECTORY_MODE);
        }
        assert_eq!(mode(&store_dir.join(HEAD)), PRIVATE_FILE_MODE);
    }

    #[test]
    fn incompressible_content_costs_almost_nothing_extra_to_store() {
        let (_dir, store) = store(false);
        let mut state = XORSHIFT_SEED;
        let content = (0..INCOMPRESSIBLE_BYTES)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state.to_le_bytes()[0]
            })
            .collect::<Vec<_>>();
        let oid = store.write_blob(&content).unwrap().oid;
        store.sync().unwrap();
        let stored = fs::metadata(store.loose.object_path(&oid)).unwrap().len();
        let bound = content.len() + content.len() / STORED_BLOCK_SHARE + FRAMING_BYTES;
        assert!(
            stored <= bound as u64,
            "{stored} bytes stored, {bound} allowed"
        );
    }
}
