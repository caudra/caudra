//! The private store beneath one validated root and binding: the object repository, the store
//! state, open and finished records, revert journals and eviction marks. Store files are
//! owner-only and appear whole or not at all. Every operation that writes or collects takes the
//! store lock, so several processes can share one store.

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
#[cfg(test)]
use std::sync::{
    Mutex, PoisonError,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};
use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    thread,
    time::{Duration, Instant, SystemTime},
};

use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workcell_host_contract::{
    MAX_ID_BYTES, MAX_OPEN_RECORDS, MAX_REVERT_JOURNALS, MAX_SNAPSHOT_CAPTURE_ENTRIES,
    MAX_SNAPSHOT_CAPTURE_PATH_BYTES, MAX_SNAPSHOT_FILE_BYTES,
};
#[cfg(test)]
use workcell_snapshot_store::RACY_MARGIN;
use workcell_snapshot_store::{ObjectStore, StoreOptions};

use crate::{MAX_STORE_RECORDS, SnapshotError, check_cancelled, format::StoredVersion};

pub(crate) const REPOSITORY: &str = "repo";
pub(crate) const OPEN: &str = "open";
pub(crate) const RECORDS: &str = "records";
pub(crate) const REVERTS: &str = "reverts";
pub(crate) const EVICTED: &str = "evicted";
const STATE: &str = "state";
const LOCK: &str = "lock";
pub(crate) const METADATA_SUFFIX: &str = ".json";
pub(crate) const DIGEST_PREFIX: &str = "sha256:";
/// The largest encoded snapshot metadata a capture stores, and the largest store file.
pub(crate) const MAX_METADATA_BYTES: u64 = 32 * 1_024 * 1_024;
/// No object needs more room than the largest file a capture admits: metadata is bounded below
/// it, and one tree holds each captured name at most once.
const MAX_OBJECT_BYTES: u64 = MAX_SNAPSHOT_FILE_BYTES;
/// A tree entry besides its name: the mode, two separators and a SHA-1 id.
pub(crate) const TREE_ENTRY_OVERHEAD: u64 = 28;
const _: () = assert!(
    MAX_METADATA_BYTES <= MAX_OBJECT_BYTES
        && MAX_SNAPSHOT_CAPTURE_PATH_BYTES
            + TREE_ENTRY_OVERHEAD * MAX_SNAPSHOT_CAPTURE_ENTRIES as u64
            <= MAX_OBJECT_BYTES
);
const DIRECTORIES: [&str; 4] = [OPEN, RECORDS, REVERTS, EVICTED];
/// Where hosts before the object repository kept their blobs and manifests.
const LEGACY_DIRECTORIES: [&str; 2] = ["blobs", "manifests"];
/// Where hosts before change records kept checkpoints and restore journals, with every format
/// they wrote there. Any other format is a later release's, and refuses the store.
const LEGACY_FORMATS: [(&str, &[&str]); 2] = [
    (
        "checkpoints",
        &[
            "workspace-snapshot-checkpoint.v1",
            "workspace-snapshot-checkpoint.v2",
        ],
    ),
    (
        "journals",
        &[
            "workspace-restore-journal.v1",
            "workspace-restore-journal.v2",
            "workspace-restore-journal.v3",
        ],
    ),
];
#[cfg(unix)]
const UMASK_PROBE_MODE: u32 = 0o777;
const TEMPORARY_PREFIX: &str = ".";
const TEMPORARY_SUFFIX: &str = ".tmp";
/// Room for abandoned temporaries beside every file the quotas allow.
const PRIVATE_ENTRY_SLACK: usize = 64;
const MAX_PRIVATE_ENTRIES: usize =
    MAX_STORE_RECORDS + MAX_OPEN_RECORDS + MAX_REVERT_JOURNALS + PRIVATE_ENTRY_SLACK;
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(10);
#[cfg(unix)]
const PRIVATE_FILE_MODE: u32 = 0o600;
#[cfg(unix)]
const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
#[cfg(unix)]
const SHARED_PERMISSION_BITS: u32 = 0o077;

pub(crate) struct Store {
    root: PathBuf,
    objects: ObjectStore,
    #[cfg(test)]
    pub(crate) hooks: TestHooks,
}

#[cfg(test)]
type PublicationHook = Box<dyn Fn(&str) + Send>;

#[cfg(test)]
#[derive(Default)]
pub(crate) struct TestHooks {
    pub object_sync: AtomicBool,
    pub record_write: AtomicBool,
    /// Counts the files a capture read rather than found unchanged in the stat cache.
    pub content_reads: AtomicUsize,
    /// Starts captures late enough that every file already in the workspace counts as settled.
    pub settled: AtomicBool,
    /// Replaces the admission timeout, in milliseconds, when not zero.
    pub admission_ms: AtomicU64,
    /// Runs after a revert publishes each path, as another writer acting between publications.
    pub publication: Mutex<Option<PublicationHook>>,
}

#[cfg(test)]
impl TestHooks {
    pub(crate) fn published(&self, path: &str) {
        if let Some(hook) = self
            .publication
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            hook(path);
        }
    }
}

/// Held for one operation. Closing the descriptor releases the lock.
pub(crate) struct StoreLock {
    _file: File,
}

impl Store {
    /// Validates `requested` as a private directory, outside `workspace` when one is given, and
    /// prepares the layout of the store for `binding` beneath it. Without `create`, the store
    /// must already exist.
    pub(crate) fn open(
        requested: &Path,
        workspace: Option<&Path>,
        binding: &str,
        create: bool,
    ) -> Result<Self, SnapshotError> {
        let private_root = validate_private_root(requested, workspace)?;
        validate_store_binding(binding)?;
        let root = private_root.join(binding);
        if create {
            create_private_directory(&root)?;
        } else {
            match fs::symlink_metadata(&root) {
                Ok(metadata) if metadata.file_type().is_dir() => {
                    validate_private_permissions(&metadata)?;
                }
                Ok(_) => return Err(SnapshotError::InvalidConfiguration),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Err(SnapshotError::NotFound);
                }
                Err(_) => return Err(SnapshotError::InvalidConfiguration),
            }
        }
        let repository = root.join(REPOSITORY);
        for directory in [REPOSITORY].into_iter().chain(DIRECTORIES) {
            create_private_directory(&root.join(directory))?;
        }
        validate_repository(&repository)?;
        let objects = ObjectStore::open(
            &repository,
            StoreOptions {
                private: true,
                max_object_bytes: usize::try_from(MAX_OBJECT_BYTES).ok(),
            },
        )
        .map_err(|_| SnapshotError::InvalidConfiguration)?;
        sync_directory(&repository)?;
        sync_directory(&root)?;
        sync_directory(&private_root)?;
        Ok(Self {
            root,
            objects,
            #[cfg(test)]
            hooks: TestHooks::default(),
        })
    }

    pub(crate) fn objects(&self) -> &ObjectStore {
        &self.objects
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Takes the store lock through a descriptor of its own, which excludes every other
    /// acquisition in this process as in any other. Waits until `deadline`, then reports busy.
    #[cfg(unix)]
    pub(crate) fn lock(
        &self,
        deadline: Instant,
        token: &CancellationToken,
    ) -> Result<StoreLock, SnapshotError> {
        use rustix::{
            fs::{FlockOperation, flock},
            io::Errno,
        };

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(PRIVATE_FILE_MODE)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(self.root.join(LOCK))
            .map_err(|_| SnapshotError::UnhealthyStorage)?;
        let metadata = file
            .metadata()
            .map_err(|_| SnapshotError::UnhealthyStorage)?;
        if !metadata.file_type().is_file() {
            return Err(SnapshotError::UnhealthyStorage);
        }
        validate_private_permissions(&metadata).map_err(|_| SnapshotError::UnhealthyStorage)?;
        loop {
            match flock(&file, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => return Ok(StoreLock { _file: file }),
                Err(Errno::WOULDBLOCK) => {}
                Err(Errno::INTR) => continue,
                Err(_) => return Err(SnapshotError::UnhealthyStorage),
            }
            check_cancelled(token)?;
            if Instant::now() >= deadline {
                return Err(SnapshotError::Busy);
            }
            thread::sleep(LOCK_RETRY_INTERVAL);
        }
    }

    #[cfg(not(unix))]
    pub(crate) fn lock(
        &self,
        _deadline: Instant,
        _token: &CancellationToken,
    ) -> Result<StoreLock, SnapshotError> {
        Err(SnapshotError::UnsupportedPlatform)
    }

    /// When a capture starting now counts as started for the stat cache, which trusts only files
    /// that had settled before then.
    pub(crate) fn capture_started(&self) -> SystemTime {
        let now = SystemTime::now();
        #[cfg(test)]
        if self.hooks.settled.load(Ordering::SeqCst) {
            return now + 2 * RACY_MARGIN;
        }
        now
    }

    #[cfg(test)]
    pub(crate) fn admission(&self, default: Duration) -> Duration {
        match self.hooks.admission_ms.load(Ordering::SeqCst) {
            0 => default,
            milliseconds => Duration::from_millis(milliseconds),
        }
    }

    #[cfg(not(test))]
    pub(crate) fn admission(&self, default: Duration) -> Duration {
        default
    }

    pub(crate) fn state_path(&self) -> PathBuf {
        self.root.join(STATE)
    }

    pub(crate) fn file(&self, directory: &str, name: &str) -> PathBuf {
        self.root.join(directory).join(name)
    }

    /// Sorted entry names of one store directory, abandoned temporaries excluded.
    pub(crate) fn names(&self, directory: &str) -> Result<Vec<String>, SnapshotError> {
        let mut names = Vec::new();
        for entry in private_entries(&self.root.join(directory))? {
            let (name, _) = entry?;
            if !is_temporary(&name) {
                names.push(name);
            }
        }
        names.sort_unstable();
        Ok(names)
    }

    /// Bytes held by the objects, the stat cache and every store file, temporaries included:
    /// they occupy the same disk.
    pub(crate) fn usage(&self) -> Result<u64, SnapshotError> {
        let mut bytes = self
            .objects
            .usage()
            .map_err(|_| SnapshotError::UnhealthyStorage)?
            .bytes;
        for directory in DIRECTORIES {
            for entry in private_entries(&self.root.join(directory))? {
                let (_, metadata) = entry?;
                bytes = bytes.saturating_add(metadata.len());
            }
        }
        Ok(bytes.saturating_add(self.size(&self.state_path())?))
    }

    pub(crate) fn object_count(&self) -> Result<u64, SnapshotError> {
        Ok(self
            .objects
            .usage()
            .map_err(|_| SnapshotError::UnhealthyStorage)?
            .objects)
    }

    /// Removes what a host before change records stored, which nothing reads any more, and
    /// returns how many entries went. A format it does not know refuses the store rather than
    /// lose what a later release wrote.
    pub(crate) fn remove_legacy(&self) -> Result<usize, SnapshotError> {
        let mut removed = 0;
        for directory in LEGACY_DIRECTORIES {
            match fs::remove_dir_all(self.root.join(directory)) {
                Ok(()) => removed += 1,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(_) => return Err(SnapshotError::UnhealthyStorage),
            }
        }
        for (directory, versions) in LEGACY_FORMATS {
            let path = self.root.join(directory);
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_dir() => {}
                Ok(_) => return Err(SnapshotError::UnhealthyStorage),
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(_) => return Err(SnapshotError::UnhealthyStorage),
            }
            for entry in private_entries(&path)? {
                let (name, _) = entry?;
                let file = path.join(&name);
                if !is_temporary(&name) {
                    let version = serde_json::from_slice::<StoredVersion>(&self.read(&file)?)
                        .map_err(|_| SnapshotError::UnhealthyStorage)?
                        .version;
                    if !versions.contains(&version.as_str()) {
                        tracing::warn!(
                            directory,
                            %version,
                            "workspace change storage holds a format this release does not know"
                        );
                        return Err(SnapshotError::UnhealthyStorage);
                    }
                }
                fs::remove_file(&file).map_err(|_| SnapshotError::UnhealthyStorage)?;
                removed += 1;
            }
            fs::remove_dir(&path).map_err(|_| SnapshotError::UnhealthyStorage)?;
            removed += 1;
        }
        if removed > 0 {
            sync_directory(&self.root)?;
        }
        Ok(removed)
    }

    /// Removes temporaries a crash left behind. Nothing reads them, so none is ever resumed.
    pub(crate) fn remove_temporaries(&self) -> Result<(), SnapshotError> {
        for directory in DIRECTORIES {
            let path = self.root.join(directory);
            for entry in private_entries(&path)? {
                let (name, _) = entry?;
                if is_temporary(&name) {
                    fs::remove_file(path.join(&name))
                        .map_err(|_| SnapshotError::UnhealthyStorage)?;
                }
            }
            self.sync(directory)?;
        }
        let entries = fs::read_dir(&self.root).map_err(|_| SnapshotError::UnhealthyStorage)?;
        for (index, entry) in entries.enumerate() {
            let entry = entry.map_err(|_| SnapshotError::UnhealthyStorage)?;
            if index >= MAX_PRIVATE_ENTRIES {
                return Err(SnapshotError::UnhealthyStorage);
            }
            let is_file = entry
                .file_type()
                .map_err(|_| SnapshotError::UnhealthyStorage)?
                .is_file();
            if is_file && entry.file_name().to_str().is_some_and(is_temporary) {
                fs::remove_file(entry.path()).map_err(|_| SnapshotError::UnhealthyStorage)?;
            }
        }
        sync_directory(&self.root)
    }

    /// A store file's content; a missing one is not found.
    pub(crate) fn read(&self, path: &Path) -> Result<Vec<u8>, SnapshotError> {
        let file = open_private(path)?;
        if file
            .metadata()
            .map_err(|_| SnapshotError::OperationFailed)?
            .len()
            > MAX_METADATA_BYTES
        {
            return Err(SnapshotError::UnhealthyStorage);
        }
        let mut bytes = Vec::new();
        file.take(MAX_METADATA_BYTES.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|_| SnapshotError::OperationFailed)?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_METADATA_BYTES {
            return Err(SnapshotError::UnhealthyStorage);
        }
        Ok(bytes)
    }

    /// Size of a store file, or zero when it does not exist.
    pub(crate) fn size(&self, path: &Path) -> Result<u64, SnapshotError> {
        match private_metadata(path) {
            Ok(metadata) => Ok(metadata.len()),
            Err(SnapshotError::NotFound) => Ok(0),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn write_atomic(&self, path: &Path, bytes: &[u8]) -> Result<(), SnapshotError> {
        #[cfg(test)]
        if path.parent() == Some(self.root.join(RECORDS).as_path())
            && self.hooks.record_write.load(Ordering::SeqCst)
        {
            return Err(SnapshotError::OperationFailed);
        }
        let parent = path.parent().ok_or(SnapshotError::OperationFailed)?;
        let mut temporary = Temporary::create(parent)?;
        temporary.write_all(bytes)?;
        fs::rename(&temporary.path, path).map_err(|_| SnapshotError::OperationFailed)?;
        sync_directory(parent)
    }

    /// Removes one store file and reports whether it was there. The caller syncs the directory.
    pub(crate) fn remove(&self, path: &Path) -> Result<bool, SnapshotError> {
        match fs::remove_file(path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(_) => Err(SnapshotError::OperationFailed),
        }
    }

    pub(crate) fn sync(&self, directory: &str) -> Result<(), SnapshotError> {
        sync_directory(&self.root.join(directory))
    }

    /// Makes every object written since the last sync durable. Nothing may name them before.
    pub(crate) fn sync_objects(&self) -> Result<(), SnapshotError> {
        #[cfg(test)]
        if self.hooks.object_sync.load(Ordering::SeqCst) {
            return Err(SnapshotError::OperationFailed);
        }
        self.objects
            .sync()
            .map_err(|_| SnapshotError::OperationFailed)
    }
}

/// A private temporary beside its destination, removed on drop unless renamed away.
struct Temporary {
    path: PathBuf,
    file: File,
}

impl Temporary {
    fn create(parent: &Path) -> Result<Self, SnapshotError> {
        let path = parent.join(temporary_name());
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(PRIVATE_FILE_MODE);
        let file = options
            .open(&path)
            .map_err(|_| SnapshotError::OperationFailed)?;
        Ok(Self { path, file })
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), SnapshotError> {
        self.file
            .write_all(bytes)
            .and_then(|()| self.file.sync_all())
            .map_err(|_| SnapshotError::OperationFailed)
    }
}

impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

type PrivateEntry = Result<(String, Metadata), SnapshotError>;

/// Entries of a store directory, each a private regular file, bounded against a flooded store.
/// Any entry that cannot be listed fails the listing: callers enumerating what must be kept
/// never mistake an unreadable directory for an empty one.
fn private_entries(directory: &Path) -> Result<impl Iterator<Item = PrivateEntry>, SnapshotError> {
    let reader = fs::read_dir(directory).map_err(|_| SnapshotError::UnhealthyStorage)?;
    Ok(reader.enumerate().map(|(index, entry)| {
        if index >= MAX_PRIVATE_ENTRIES {
            return Err(SnapshotError::UnhealthyStorage);
        }
        let entry = entry.map_err(|_| SnapshotError::UnhealthyStorage)?;
        let metadata = entry
            .metadata()
            .map_err(|_| SnapshotError::UnhealthyStorage)?;
        if !metadata.file_type().is_file() {
            return Err(SnapshotError::UnhealthyStorage);
        }
        validate_private_permissions(&metadata).map_err(|_| SnapshotError::UnhealthyStorage)?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| SnapshotError::UnhealthyStorage)?;
        Ok((name, metadata))
    }))
}

/// The repository's layout is the engine's, but everything at its top must be as private as the
/// rest of the store: nothing there may be a link, shared, or another user's. What another
/// process removes meanwhile, such as its stat cache temporary, is gone rather than suspect.
fn validate_repository(repository: &Path) -> Result<(), SnapshotError> {
    let entries = fs::read_dir(repository).map_err(|_| SnapshotError::InvalidConfiguration)?;
    for (index, entry) in entries.enumerate() {
        let metadata = match entry.and_then(|entry| entry.metadata()) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => return Err(SnapshotError::InvalidConfiguration),
        };
        if index >= MAX_PRIVATE_ENTRIES || metadata.file_type().is_symlink() {
            return Err(SnapshotError::InvalidConfiguration);
        }
        validate_private_permissions(&metadata)?;
    }
    Ok(())
}

fn temporary_name() -> String {
    format!(
        "{TEMPORARY_PREFIX}{}{TEMPORARY_SUFFIX}",
        Uuid::new_v4().simple()
    )
}

fn is_temporary(name: &str) -> bool {
    name.starts_with(TEMPORARY_PREFIX) && name.ends_with(TEMPORARY_SUFFIX)
}

fn private_metadata(path: &Path) -> Result<Metadata, SnapshotError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(SnapshotError::NotFound);
        }
        Err(_) => return Err(SnapshotError::OperationFailed),
    };
    if !metadata.file_type().is_file() {
        return Err(SnapshotError::UnhealthyStorage);
    }
    validate_private_permissions(&metadata).map_err(|_| SnapshotError::UnhealthyStorage)?;
    Ok(metadata)
}

fn open_private(path: &Path) -> Result<File, SnapshotError> {
    let metadata = private_metadata(path)?;
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(SnapshotError::NotFound);
        }
        Err(_) => return Err(SnapshotError::OperationFailed),
    };
    let opened = file
        .metadata()
        .map_err(|_| SnapshotError::OperationFailed)?;
    if !same_file(&metadata, &opened) {
        return Err(SnapshotError::UnhealthyStorage);
    }
    Ok(file)
}

#[cfg(unix)]
fn same_file(left: &Metadata, right: &Metadata) -> bool {
    (left.dev(), left.ino()) == (right.dev(), right.ino())
}

#[cfg(not(unix))]
fn same_file(left: &Metadata, right: &Metadata) -> bool {
    left.len() == right.len()
}

fn validate_private_root(
    requested: &Path,
    workspace: Option<&Path>,
) -> Result<PathBuf, SnapshotError> {
    if !requested.is_absolute() {
        return Err(SnapshotError::InvalidConfiguration);
    }
    reject_symlink_components(requested)?;
    let root = requested
        .canonicalize()
        .map_err(|_| SnapshotError::InvalidConfiguration)?;
    let metadata = fs::symlink_metadata(&root).map_err(|_| SnapshotError::InvalidConfiguration)?;
    if !metadata.file_type().is_dir() {
        return Err(SnapshotError::InvalidConfiguration);
    }
    validate_private_permissions(&metadata)?;
    if let Some(workspace) = workspace {
        let workspace = workspace
            .canonicalize()
            .map_err(|_| SnapshotError::InvalidConfiguration)?;
        if root.starts_with(&workspace) || workspace.starts_with(&root) {
            return Err(SnapshotError::InvalidConfiguration);
        }
    }
    Ok(root)
}

fn reject_symlink_components(path: &Path) -> Result<(), SnapshotError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => current.push(component.as_os_str()),
            Component::CurDir => continue,
            Component::ParentDir => return Err(SnapshotError::InvalidConfiguration),
            Component::Normal(part) => current.push(part),
        }
        let metadata =
            fs::symlink_metadata(&current).map_err(|_| SnapshotError::InvalidConfiguration)?;
        if metadata.file_type().is_symlink() {
            return Err(SnapshotError::InvalidConfiguration);
        }
    }
    Ok(())
}

fn validate_store_binding(binding: &str) -> Result<(), SnapshotError> {
    if binding.is_empty()
        || binding.len() > MAX_ID_BYTES
        || !binding
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(SnapshotError::InvalidConfiguration);
    }
    Ok(())
}

#[cfg(unix)]
fn validate_private_permissions(metadata: &Metadata) -> Result<(), SnapshotError> {
    if metadata.uid() != rustix::process::getuid().as_raw()
        || metadata.permissions().mode() & SHARED_PERMISSION_BITS != 0
    {
        return Err(SnapshotError::InvalidConfiguration);
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_permissions(metadata: &Metadata) -> Result<(), SnapshotError> {
    if metadata.permissions().readonly() {
        return Err(SnapshotError::InvalidConfiguration);
    }
    Ok(())
}

/// Creates a private directory, or accepts one already there, even one another process created
/// a moment ago.
fn create_private_directory(path: &Path) -> Result<(), SnapshotError> {
    let created = match fs::create_dir(path) {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => false,
        Err(_) => return Err(SnapshotError::InvalidConfiguration),
    };
    #[cfg(unix)]
    if created {
        fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE))
            .map_err(|_| SnapshotError::InvalidConfiguration)?;
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| SnapshotError::InvalidConfiguration)?;
    if !metadata.file_type().is_dir() {
        return Err(SnapshotError::InvalidConfiguration);
    }
    validate_private_permissions(&metadata)
}

/// The umask, read off a file the kernel created under it: asking for it directly would change it
/// for every thread until it was set back. Each probe has a name of its own, so processes opening
/// the store at once never meet, and one a crash left is removed as a temporary.
#[cfg(unix)]
pub(crate) fn probe_umask(root: &Path) -> Result<u32, SnapshotError> {
    let path = root.join(temporary_name());
    let created = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(UMASK_PROBE_MODE)
        .open(&path)
        .and_then(|probe| probe.metadata())
        .map_err(|_| SnapshotError::InvalidConfiguration);
    let removed = fs::remove_file(&path);
    let mode = created?.permissions().mode();
    removed.map_err(|_| SnapshotError::InvalidConfiguration)?;
    Ok(UMASK_PROBE_MODE & !mode)
}

#[cfg(not(unix))]
pub(crate) fn probe_umask(_root: &Path) -> Result<u32, SnapshotError> {
    Ok(0)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), SnapshotError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| SnapshotError::OperationFailed)
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), SnapshotError> {
    Ok(())
}
