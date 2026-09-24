//! The private store beneath one validated root: content-addressed blobs and manifests, checkpoint
//! references and restore journals. Every file is owner-only and appears whole or not at all.

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
};

use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workcell_host_contract::MAX_ID_BYTES;

use crate::{
    MAX_PRIVATE_ENTRIES, SnapshotError, check_cancelled, format_sha256, hex_sha256,
    valid_hex_digest, validate_snapshot_id,
};

pub(crate) const BLOBS: &str = "blobs";
pub(crate) const MANIFESTS: &str = "manifests";
pub(crate) const CHECKPOINTS: &str = "checkpoints";
pub(crate) const JOURNALS: &str = "journals";
pub(crate) const METADATA_SUFFIX: &str = ".json";
pub(crate) const DIGEST_PREFIX: &str = "sha256:";
const DIRECTORIES: [&str; 4] = [BLOBS, MANIFESTS, CHECKPOINTS, JOURNALS];
const TEMPORARY_PREFIX: &str = ".";
const TEMPORARY_SUFFIX: &str = ".tmp";
const RESTORE_ID_PREFIX: &str = "restore_";
const MAX_RESTORE_ID_BYTES: usize = 64;
const STREAM_BUFFER_BYTES: usize = 64 * 1_024;
#[cfg(unix)]
const PRIVATE_FILE_MODE: u32 = 0o600;
#[cfg(unix)]
const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
#[cfg(unix)]
const SHARED_PERMISSION_BITS: u32 = 0o077;

pub(crate) struct Store {
    root: PathBuf,
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
        for directory in DIRECTORIES {
            create_private_directory(&root.join(directory))?;
        }
        Ok(Self { root })
    }

    pub(crate) fn manifest_path(&self, snapshot_id: &str) -> Result<PathBuf, SnapshotError> {
        validate_snapshot_id(snapshot_id)?;
        Ok(self
            .root
            .join(MANIFESTS)
            .join(format!("{snapshot_id}{METADATA_SUFFIX}")))
    }

    pub(crate) fn blob_path(&self, digest: &str) -> Result<PathBuf, SnapshotError> {
        let hex = digest
            .strip_prefix(DIGEST_PREFIX)
            .filter(|hex| valid_hex_digest(hex))
            .ok_or(SnapshotError::IntegrityFailure)?;
        Ok(self.root.join(BLOBS).join(hex))
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

    pub(crate) fn count(&self, directory: &str) -> Result<usize, SnapshotError> {
        Ok(self.names(directory)?.len())
    }

    /// Bytes held by every store file, temporaries included: they occupy the same disk.
    pub(crate) fn usage(&self) -> Result<u64, SnapshotError> {
        let mut total = 0_u64;
        for directory in DIRECTORIES {
            for entry in private_entries(&self.root.join(directory))? {
                let (_, metadata) = entry?;
                total = total
                    .checked_add(metadata.len())
                    .ok_or(SnapshotError::UnhealthyStorage)?;
            }
        }
        Ok(total)
    }

    /// Removes temporaries a crash left behind. Nothing reads them, so none is ever resumed.
    pub(crate) fn remove_temporaries(&self) -> Result<(), SnapshotError> {
        for directory in DIRECTORIES {
            let path = self.root.join(directory);
            let mut removed = false;
            for entry in private_entries(&path)? {
                let (name, _) = entry?;
                if is_temporary(&name) {
                    fs::remove_file(path.join(&name))
                        .map_err(|_| SnapshotError::UnhealthyStorage)?;
                    removed = true;
                }
            }
            if removed {
                sync_directory(&path)?;
            }
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

    pub(crate) fn exists(&self, path: &Path) -> Result<bool, SnapshotError> {
        match private_metadata(path) {
            Ok(_) => Ok(true),
            Err(SnapshotError::NotFound) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Publishes content-addressed bytes. An existing file must already hold exactly them.
    pub(crate) fn write_immutable(&self, path: &Path, bytes: &[u8]) -> Result<(), SnapshotError> {
        let maximum = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        match self.read(path, maximum) {
            Ok(existing) if existing == bytes => return Ok(()),
            Ok(_) => return Err(SnapshotError::IntegrityFailure),
            Err(SnapshotError::NotFound) => {}
            Err(error) => return Err(error),
        }
        let parent = path.parent().ok_or(SnapshotError::OperationFailed)?;
        let mut temporary = Temporary::create(parent)?;
        temporary.write_all(bytes)?;
        match fs::hard_link(&temporary.path, path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if self.read(path, maximum)? != bytes {
                    return Err(SnapshotError::IntegrityFailure);
                }
            }
            Err(_) => return Err(SnapshotError::OperationFailed),
        }
        drop(temporary);
        sync_directory(parent)
    }

    pub(crate) fn write_atomic(&self, path: &Path, bytes: &[u8]) -> Result<(), SnapshotError> {
        let parent = path.parent().ok_or(SnapshotError::OperationFailed)?;
        let mut temporary = Temporary::create(parent)?;
        temporary.write_all(bytes)?;
        fs::rename(&temporary.path, path).map_err(|_| SnapshotError::OperationFailed)?;
        sync_directory(parent)
    }

    /// Removes one store file; an already missing one is success. The caller syncs the directory.
    pub(crate) fn remove(&self, path: &Path) -> Result<(), SnapshotError> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(SnapshotError::OperationFailed),
        }
    }

    pub(crate) fn sync(&self, directory: &str) -> Result<(), SnapshotError> {
        sync_directory(&self.root.join(directory))
    }

    /// Stores `size` bytes from `source` as the blob `digest`, verifying both on the way. The
    /// caller syncs the blob directory before anything durable names the blob.
    pub(crate) fn write_blob(
        &self,
        source: &mut dyn Read,
        digest: &str,
        size: u64,
        token: &CancellationToken,
    ) -> Result<BlobWrite, SnapshotError> {
        let path = self.blob_path(digest)?;
        let mut temporary = Temporary::create(&self.root.join(BLOBS))?;
        let (written_digest, written) =
            digest_stream(source, Some(&mut temporary.file), size, token)?;
        if written != size || written_digest != digest {
            return Ok(BlobWrite::Mismatch);
        }
        temporary
            .file
            .sync_all()
            .map_err(|_| SnapshotError::OperationFailed)?;
        match fs::hard_link(&temporary.path, &path) {
            Ok(()) => Ok(BlobWrite::Stored),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(BlobWrite::Present),
            Err(_) => Err(SnapshotError::OperationFailed),
        }
    }

    /// Opens a blob for streaming. The reader fails at end of input unless it read exactly `size`
    /// bytes hashing to `digest`, so a corrupt blob never reaches a consumer whole.
    pub(crate) fn open_blob(&self, digest: &str, size: u64) -> Result<BlobReader, SnapshotError> {
        let file = open_private(&self.blob_path(digest)?).map_err(|error| match error {
            SnapshotError::NotFound => SnapshotError::IntegrityFailure,
            error => error,
        })?;
        Ok(BlobReader {
            file,
            hasher: Sha256::new(),
            read: 0,
            size,
            digest: digest.to_owned(),
        })
    }

    pub(crate) fn read_blob(&self, digest: &str, size: u64) -> Result<Vec<u8>, SnapshotError> {
        let mut bytes = Vec::new();
        self.open_blob(digest, size)?
            .read_to_end(&mut bytes)
            .map_err(blob_error)?;
        Ok(bytes)
    }
}

pub(crate) struct BlobReader {
    file: File,
    hasher: Sha256,
    read: u64,
    size: u64,
    digest: String,
}

impl Read for BlobReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.file.read(buffer)?;
        self.read = self
            .read
            .saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
        self.hasher.update(&buffer[..count]);
        let complete = count == 0 || self.read > self.size;
        if complete
            && (self.read != self.size
                || format_sha256(self.hasher.clone().finalize()) != self.digest)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "snapshot blob failed integrity verification",
            ));
        }
        Ok(count)
    }
}

/// Integrity failures surface from a blob reader as `InvalidData`.
pub(crate) fn blob_error(error: io::Error) -> SnapshotError {
    if error.kind() == io::ErrorKind::InvalidData {
        SnapshotError::IntegrityFailure
    } else {
        SnapshotError::OperationFailed
    }
}

/// Digest and length of everything `source` yields, stopping one byte past `maximum` so growth
/// shows as a longer length rather than an unbounded read. Bytes are copied to `sink` as read.
pub(crate) fn digest_stream(
    source: &mut dyn Read,
    mut sink: Option<&mut File>,
    maximum: u64,
    token: &CancellationToken,
) -> Result<(String, u64), SnapshotError> {
    let mut source = source.take(maximum.saturating_add(1));
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; STREAM_BUFFER_BYTES];
    let mut total = 0_u64;
    loop {
        check_cancelled(token)?;
        let count = match source.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(blob_error(error)),
        };
        hasher.update(&buffer[..count]);
        if let Some(sink) = sink.as_mut() {
            sink.write_all(&buffer[..count])
                .map_err(|_| SnapshotError::OperationFailed)?;
        }
        total = total.saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
    }
    Ok((format_sha256(hasher.finalize()), total))
}

pub(crate) enum BlobWrite {
    Stored,
    /// An identical blob already existed; this write created nothing.
    Present,
    /// The source did not hold exactly the expected content, so nothing was stored.
    Mismatch,
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
