//! The private store beneath one validated root: the object repository, checkpoint references and
//! restore journals. Checkpoints and journals are owner-only files that appear whole or not at all.
//! Objects live in owner-only directories and every read of one is verified against its id.

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
#[cfg(test)]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    time::SystemTime,
};

use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workcell_host_contract::{
    MAX_ID_BYTES, MAX_SNAPSHOT_CAPTURE_ENTRIES, MAX_SNAPSHOT_CAPTURE_PATH_BYTES,
    MAX_SNAPSHOT_COUNT, MAX_SNAPSHOT_FILE_BYTES, MAX_SNAPSHOT_JOURNALS,
};
#[cfg(test)]
use workcell_snapshot_store::RACY_MARGIN;
use workcell_snapshot_store::{ObjectStore, StoreOptions};

use crate::{SnapshotError, check_cancelled, hex_sha256};

pub(crate) const REPOSITORY: &str = "repo";
pub(crate) const CHECKPOINTS: &str = "checkpoints";
pub(crate) const JOURNALS: &str = "journals";
pub(crate) const METADATA_SUFFIX: &str = ".json";
pub(crate) const DIGEST_PREFIX: &str = "sha256:";
/// The largest encoded snapshot metadata a capture stores.
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
const DIRECTORIES: [&str; 2] = [CHECKPOINTS, JOURNALS];
/// Where hosts before the object repository kept their blobs and manifests.
const LEGACY_DIRECTORIES: [&str; 2] = ["blobs", "manifests"];
/// Created and removed at open to learn the umask, and removed first should a crash have left it.
#[cfg(unix)]
const UMASK_PROBE: &str = ".umask-probe";
#[cfg(unix)]
const UMASK_PROBE_MODE: u32 = 0o777;
const TEMPORARY_PREFIX: &str = ".";
const TEMPORARY_SUFFIX: &str = ".tmp";
const RESTORE_ID_PREFIX: &str = "restore_";
const MAX_RESTORE_ID_BYTES: usize = 64;
/// Room for abandoned temporaries beside every checkpoint and journal the quotas allow.
const PRIVATE_ENTRY_SLACK: usize = 64;
const MAX_PRIVATE_ENTRIES: usize = MAX_SNAPSHOT_COUNT + MAX_SNAPSHOT_JOURNALS + PRIVATE_ENTRY_SLACK;
#[cfg(unix)]
const PRIVATE_FILE_MODE: u32 = 0o600;
#[cfg(unix)]
const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
#[cfg(unix)]
const SHARED_PERMISSION_BITS: u32 = 0o077;

pub(crate) struct Store {
    root: PathBuf,
    objects: ObjectStore,
    umask: u32,
    #[cfg(test)]
    pub(crate) hooks: TestHooks,
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct TestHooks {
    pub checkpoint_sync: AtomicBool,
    pub checkpoint_remove: AtomicBool,
    pub object_sync: AtomicBool,
    /// Counts the files a capture read rather than found unchanged in the stat cache.
    pub content_reads: AtomicUsize,
    /// Starts captures late enough that every file already in the workspace counts as settled.
    pub settled: AtomicBool,
}

pub(crate) struct CaptureInventory {
    pub bytes: u64,
    pub checkpoints: usize,
}

impl Store {
    /// Validates `requested` as a private directory outside the workspace and prepares its layout,
    /// beneath `binding` when one store root serves several workspaces.
    pub(crate) fn open(
        requested: &Path,
        workspace: &Path,
        binding: Option<&str>,
    ) -> Result<Self, SnapshotError> {
        let mut root = validate_private_root(requested, workspace)?;
        if let Some(binding) = binding {
            validate_store_binding(binding)?;
            root.push(binding);
            create_private_directory(&root)?;
        }
        let repository = root.join(REPOSITORY);
        for directory in [REPOSITORY, CHECKPOINTS, JOURNALS] {
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
        sync_directory(root.parent().ok_or(SnapshotError::InvalidConfiguration)?)?;
        Ok(Self {
            umask: probe_umask(&root)?,
            root,
            objects,
            #[cfg(test)]
            hooks: TestHooks::default(),
        })
    }

    pub(crate) fn objects(&self) -> &ObjectStore {
        &self.objects
    }

    /// The permission bits the process umask withholds from a file it creates.
    pub(crate) fn umask(&self) -> u32 {
        self.umask
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

    /// Checkpoint ids are client-chosen, so their file names are digests of them.
    pub(crate) fn checkpoint_path(&self, checkpoint_id: &str) -> PathBuf {
        self.root.join(CHECKPOINTS).join(format!(
            "{}{METADATA_SUFFIX}",
            hex_sha256(checkpoint_id.as_bytes())
        ))
    }

    pub(crate) fn journal_path(&self, restore_id: &str) -> Result<PathBuf, SnapshotError> {
        if !restore_id.starts_with(RESTORE_ID_PREFIX)
            || restore_id.len() > MAX_RESTORE_ID_BYTES
            || !restore_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(SnapshotError::IntegrityFailure);
        }
        Ok(self
            .root
            .join(JOURNALS)
            .join(format!("{restore_id}{METADATA_SUFFIX}")))
    }

    pub(crate) fn directory(&self, name: &str) -> PathBuf {
        self.root.join(name)
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

    /// Bytes held by the objects, the stat cache and every Workcell file, temporaries included:
    /// they occupy the same disk.
    pub(crate) fn usage(&self) -> Result<u64, SnapshotError> {
        Ok(self.capture_inventory(&CancellationToken::new())?.bytes)
    }

    pub(crate) fn capture_inventory(
        &self,
        token: &CancellationToken,
    ) -> Result<CaptureInventory, SnapshotError> {
        let mut inventory = CaptureInventory {
            bytes: self
                .objects
                .usage()
                .map_err(|_| SnapshotError::UnhealthyStorage)?
                .bytes,
            checkpoints: 0,
        };
        for directory in DIRECTORIES {
            for entry in private_entries(&self.root.join(directory))? {
                check_cancelled(token)?;
                let (name, metadata) = entry?;
                inventory.bytes = inventory
                    .bytes
                    .checked_add(metadata.len())
                    .ok_or(SnapshotError::UnhealthyStorage)?;
                inventory.checkpoints +=
                    usize::from(directory == CHECKPOINTS && !is_temporary(&name));
            }
        }
        Ok(inventory)
    }

    /// Removes what a host before the object repository stored, which nothing reads any more,
    /// and returns how many of its directories there were.
    pub(crate) fn remove_legacy_directories(&self) -> Result<usize, SnapshotError> {
        let mut removed = 0;
        for directory in LEGACY_DIRECTORIES {
            match fs::remove_dir_all(self.root.join(directory)) {
                Ok(()) => removed += 1,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(_) => return Err(SnapshotError::UnhealthyStorage),
            }
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
        Ok(())
    }

    pub(crate) fn read(&self, path: &Path, maximum: u64) -> Result<Vec<u8>, SnapshotError> {
        let file = open_private(path)?;
        if file
            .metadata()
            .map_err(|_| SnapshotError::OperationFailed)?
            .len()
            > maximum
        {
            return Err(SnapshotError::IntegrityFailure);
        }
        let mut bytes = Vec::new();
        file.take(maximum.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|_| SnapshotError::OperationFailed)?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > maximum {
            return Err(SnapshotError::IntegrityFailure);
        }
        Ok(bytes)
    }

    /// Size of a private file, or zero when it does not exist.
    pub(crate) fn size(&self, path: &Path) -> Result<u64, SnapshotError> {
        match private_metadata(path) {
            Ok(metadata) => Ok(metadata.len()),
            Err(SnapshotError::NotFound) => Ok(0),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn write_atomic(&self, path: &Path, bytes: &[u8]) -> Result<(), SnapshotError> {
        let parent = path.parent().ok_or(SnapshotError::OperationFailed)?;
        let mut temporary = Temporary::create(parent)?;
        temporary.write_all(bytes)?;
        fs::rename(&temporary.path, path).map_err(|_| SnapshotError::OperationFailed)?;
        self.sync_path(parent)
    }

    /// Removes one store file; an already missing one is success. The caller syncs the directory.
    pub(crate) fn remove(&self, path: &Path) -> Result<(), SnapshotError> {
        #[cfg(test)]
        if path.parent() == Some(self.directory(CHECKPOINTS).as_path())
            && self.hooks.checkpoint_remove.load(Ordering::SeqCst)
        {
            return Err(SnapshotError::OperationFailed);
        }
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(SnapshotError::OperationFailed),
        }
    }

    pub(crate) fn sync(&self, directory: &str) -> Result<(), SnapshotError> {
        self.sync_path(&self.root.join(directory))
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

    fn sync_path(&self, path: &Path) -> Result<(), SnapshotError> {
        #[cfg(test)]
        if path == self.directory(CHECKPOINTS) && self.hooks.checkpoint_sync.load(Ordering::SeqCst)
        {
            return Err(SnapshotError::OperationFailed);
        }
        sync_directory(path)
    }
}

/// A private temporary beside its destination, removed on drop unless renamed away.
struct Temporary {
    path: PathBuf,
    file: File,
}

impl Temporary {
    fn create(parent: &Path) -> Result<Self, SnapshotError> {
        let path = parent.join(format!(
            "{TEMPORARY_PREFIX}{}{TEMPORARY_SUFFIX}",
            Uuid::new_v4()
        ));
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
/// rest of the store: nothing there may be a link, shared, or another user's.
fn validate_repository(repository: &Path) -> Result<(), SnapshotError> {
    let entries = fs::read_dir(repository).map_err(|_| SnapshotError::InvalidConfiguration)?;
    for (index, entry) in entries.enumerate() {
        let metadata = entry
            .and_then(|entry| entry.metadata())
            .map_err(|_| SnapshotError::InvalidConfiguration)?;
        if index >= MAX_PRIVATE_ENTRIES || metadata.file_type().is_symlink() {
            return Err(SnapshotError::InvalidConfiguration);
        }
        validate_private_permissions(&metadata)?;
    }
    Ok(())
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
        return Err(SnapshotError::IntegrityFailure);
    }
    validate_private_permissions(&metadata).map_err(|_| SnapshotError::IntegrityFailure)?;
    Ok(metadata)
}

fn open_private(path: &Path) -> Result<File, SnapshotError> {
    let metadata = private_metadata(path)?;
    let file = File::open(path).map_err(|_| SnapshotError::OperationFailed)?;
    let opened = file
        .metadata()
        .map_err(|_| SnapshotError::OperationFailed)?;
    if !same_file(&metadata, &opened) {
        return Err(SnapshotError::IntegrityFailure);
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

fn validate_private_root(requested: &Path, workspace: &Path) -> Result<PathBuf, SnapshotError> {
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
    let workspace = workspace
        .canonicalize()
        .map_err(|_| SnapshotError::InvalidConfiguration)?;
    if root.starts_with(&workspace) || workspace.starts_with(&root) {
        return Err(SnapshotError::InvalidConfiguration);
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
    if binding.len() > MAX_ID_BYTES
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

fn create_private_directory(path: &Path) -> Result<(), SnapshotError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => validate_private_permissions(&metadata),
        Ok(_) => Err(SnapshotError::InvalidConfiguration),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(|_| SnapshotError::InvalidConfiguration)?;
            #[cfg(unix)]
            fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE))
                .map_err(|_| SnapshotError::InvalidConfiguration)?;
            Ok(())
        }
        Err(_) => Err(SnapshotError::InvalidConfiguration),
    }
}

/// The umask, read off a file the kernel created under it: asking for it directly would change it
/// for every thread until it was set back.
#[cfg(unix)]
fn probe_umask(root: &Path) -> Result<u32, SnapshotError> {
    let path = root.join(UMASK_PROBE);
    let remove = || match fs::remove_file(&path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            Err(SnapshotError::InvalidConfiguration)
        }
        _ => Ok(()),
    };
    remove()?;
    let created = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(UMASK_PROBE_MODE)
        .open(&path)
        .and_then(|probe| probe.metadata())
        .map_err(|_| SnapshotError::InvalidConfiguration);
    remove()?;
    Ok(UMASK_PROBE_MODE & !created?.permissions().mode())
}

#[cfg(not(unix))]
fn probe_umask(_root: &Path) -> Result<u32, SnapshotError> {
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
