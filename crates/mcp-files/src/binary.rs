use std::{
    fs::{File, Metadata, Permissions},
    io::{Read, Seek, SeekFrom, Write},
    mem::size_of,
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, PermissionsExt},
    },
    path::{Component, Path},
    sync::Arc,
};

use rustix::fs::{AtFlags, Mode, OFlags, linkat, mkdirat, open, openat, renameat, unlinkat};
#[cfg(target_os = "linux")]
use rustix::fs::{ResolveFlags, openat2};
use rustix::io::Errno;
use sha2::{Digest, Sha256};
use tokio::sync::OwnedMutexGuard;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workcell_host_contract::{
    DisplayText, MAX_TRANSFER_DEPTH, MAX_TRANSFER_JOURNAL_BYTES, ResourceAccess, ResourceId,
    ResourceIntent, Revision, TRANSFER_STREAM_BUFFER_BYTES, TransferFile, TransferMode,
    TransferPrecondition, WorkspacePath,
};

use crate::{
    FileToolGroup, RootResourceKind, operations::FilesystemCore, root_relative_resource_id,
    root_relative_resource_scope,
};

pub(super) const READ_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::CLOEXEC)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK);
pub(super) const DIRECTORY_FLAGS: OFlags = READ_FLAGS.union(OFlags::DIRECTORY);

type AncestorIdentities = Vec<(u64, u64)>;
const REPOSITORY_MARKERS: &[&str] = &[".git", ".hg", ".svn"];
const MAX_DIRECTORY_PATH_BYTES: usize = MAX_TRANSFER_JOURNAL_BYTES as usize / 4;

pub(super) fn open_root(root: &Path) -> Result<File, BinaryError> {
    if !root.is_absolute() {
        return Err(BinaryError::Inaccessible);
    }
    let mut directory = File::from(
        open("/", DIRECTORY_FLAGS, Mode::empty()).map_err(|_| BinaryError::Inaccessible)?,
    );
    for component in root.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                directory = File::from(
                    openat(&directory, name, DIRECTORY_FLAGS, Mode::empty())
                        .map_err(|_| BinaryError::Inaccessible)?,
                );
            }
            _ => return Err(BinaryError::Inaccessible),
        }
    }
    Ok(directory)
}

/// NO_XDEV rejects bind mounts too. Unsupported kernels fail closed, never fall back to st_dev.
pub(super) fn open_child(parent: &File, name: &str, flags: OFlags) -> Result<File, Errno> {
    #[cfg(target_os = "linux")]
    {
        openat2(
            parent,
            name,
            flags,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
        )
        .map(File::from)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (parent, name, flags);
        Err(Errno::NOSYS)
    }
}

pub(super) fn reject_repository(directory: &File) -> Result<(), BinaryError> {
    for marker in REPOSITORY_MARKERS {
        match rustix::fs::statat(directory, *marker, AtFlags::SYMLINK_NOFOLLOW) {
            Err(Errno::NOENT) => {}
            _ => return Err(BinaryError::Inaccessible),
        }
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum BinaryError {
    #[error("binary resource is inaccessible or unsupported")]
    Inaccessible,
    #[error("binary resource or ancestor changed")]
    Conflict,
    #[error("binary content exceeds its bound or does not match its sealed digest and length")]
    Integrity,
    #[error("binary operation was cancelled")]
    Cancelled,
    #[error("binary publication may have taken effect; reconcile its durable outcome")]
    Indeterminate,
}

#[derive(Clone)]
pub struct BinaryPublicationContent {
    pub digest: Revision,
    pub size_bytes: u64,
    pub mode: TransferMode,
}

pub struct VerifiedBinaryFile {
    pub file: File,
    pub metadata: TransferFile,
}

pub struct PreparedBinaryPublication {
    core: Arc<FilesystemCore>,
    path: WorkspacePath,
    ancestors: AncestorIdentities,
    create_directories: Vec<WorkspacePath>,
    precondition: TransferPrecondition,
    content: BinaryPublicationContent,
    maximum: u64,
    resources: Vec<ResourceIntent>,
    #[cfg(test)]
    before_publish: Option<Box<dyn FnOnce() + Send>>,
}

impl PreparedBinaryPublication {
    #[must_use]
    pub fn resources(&self) -> &[ResourceIntent] {
        &self.resources
    }

    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            + self.path.retained_bytes()
            + self.ancestors.capacity() * size_of::<(u64, u64)>()
            + self.create_directories.capacity() * size_of::<WorkspacePath>()
            + self
                .create_directories
                .iter()
                .map(WorkspacePath::retained_bytes)
                .sum::<usize>()
            + self.content.digest.retained_bytes()
            + self.resources.capacity() * size_of::<ResourceIntent>()
            + self
                .resources
                .iter()
                .map(|resource| {
                    resource.resource_id.as_str().len() * 2
                        + resource.display.as_str().len() * 2
                        + resource.scope.capacity() * size_of::<ResourceId>()
                        + resource
                            .scope
                            .iter()
                            .map(|id| id.as_str().len() * 2)
                            .sum::<usize>()
                        + resource
                            .revision
                            .as_ref()
                            .map_or(0, Revision::retained_bytes)
                })
                .sum::<usize>()
            + match &self.precondition {
                TransferPrecondition::MustNotExist {} => 0,
                TransferPrecondition::Revision { revision } => revision.retained_bytes(),
            }
    }
}

impl FileToolGroup {
    pub(super) async fn binary_path(
        &self,
        cwd: &ResourceId,
        path: &WorkspacePath,
    ) -> Result<WorkspacePath, BinaryError> {
        let cwd = self
            .workspace_directory_path(cwd)
            .await
            .map_err(|_| BinaryError::Conflict)?;
        let joined = if cwd == "." {
            path.as_str().to_owned()
        } else {
            format!("{cwd}/{}", path.as_str())
        };
        let resolved = self
            .core
            .policy
            .resolve(&joined)
            .await
            .map_err(|_| BinaryError::Inaccessible)?;
        // Canonical resolution remains owned by RootPathPolicy. Descriptor traversal below is an
        // additional no-symlink requirement, not an alternative confinement policy.
        if resolved != self.core.root().join(&joined) {
            return Err(BinaryError::Inaccessible);
        }
        WorkspacePath::new(joined).map_err(|_| BinaryError::Inaccessible)
    }

    pub async fn open_binary(
        &self,
        cwd: &ResourceId,
        path: &WorkspacePath,
        maximum: u64,
        token: &CancellationToken,
    ) -> Result<VerifiedBinaryFile, BinaryError> {
        let path = self.binary_path(cwd, path).await?;
        let core = self.core.clone();
        let token = token.clone();
        let guard = mutation_guard(&core, &token).await?;
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            let (parent, _, name) = open_parent(core.root(), &path)?;
            let mut file = regular_file(&parent, &name)?;
            let metadata = inspect_file(&mut file, &path, maximum, &token)?;
            Ok(VerifiedBinaryFile { file, metadata })
        })
        .await
        .map_err(|_| BinaryError::Inaccessible)?
    }

    pub async fn prepare_binary_publication(
        &self,
        cwd: &ResourceId,
        path: &WorkspacePath,
        precondition: TransferPrecondition,
        content: BinaryPublicationContent,
        maximum: u64,
        token: &CancellationToken,
    ) -> Result<PreparedBinaryPublication, BinaryError> {
        self.prepare_binary_publication_with_directories(
            cwd,
            path,
            precondition,
            content,
            Vec::new(),
            maximum,
            token,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn prepare_binary_publication_with_directories(
        &self,
        cwd: &ResourceId,
        path: &WorkspacePath,
        precondition: TransferPrecondition,
        content: BinaryPublicationContent,
        create_directories: Vec<WorkspacePath>,
        maximum: u64,
        token: &CancellationToken,
    ) -> Result<PreparedBinaryPublication, BinaryError> {
        self.core
            .require_write()
            .map_err(|_| BinaryError::Inaccessible)?;
        if content.size_bytes > maximum {
            return Err(BinaryError::Integrity);
        }
        if create_directories.len() > MAX_TRANSFER_DEPTH
            || create_directories
                .iter()
                .map(|path| path.as_str().len())
                .sum::<usize>()
                > MAX_DIRECTORY_PATH_BYTES
        {
            return Err(BinaryError::Integrity);
        }
        let path = self.binary_path(cwd, path).await?;
        if path.as_str().split('/').count() > MAX_TRANSFER_DEPTH {
            return Err(BinaryError::Integrity);
        }
        let mut directories = Vec::with_capacity(create_directories.len());
        for directory in create_directories {
            directories.push(self.binary_path(cwd, &directory).await?);
        }
        let core = self.core.clone();
        let token = token.clone();
        let guard = mutation_guard(&core, &token).await?;
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            let first = directories.first().unwrap_or(&path);
            let (parent, ancestors, name) = open_parent(core.root(), first)?;
            validate_destination(&parent, &name, first, &precondition, maximum, &token)?;
            if !directories.is_empty() {
                if !matches!(precondition, TransferPrecondition::MustNotExist {}) {
                    return Err(BinaryError::Conflict);
                }
                let parts = path.as_str().split('/').collect::<Vec<_>>();
                let expected = (ancestors.len()..parts.len())
                    .map(|n| parts[..n].join("/"))
                    .collect::<Vec<_>>();
                if directories
                    .iter()
                    .map(WorkspacePath::as_str)
                    .ne(expected.iter().map(String::as_str))
                {
                    return Err(BinaryError::Conflict);
                }
            }
            let mut resources = Vec::new();
            let scope = root_relative_resource_scope(RootResourceKind::Path, path.as_str())
                .map_err(|_| BinaryError::Inaccessible)?;
            let parts = path.as_str().split('/').collect::<Vec<_>>();
            for length in 0..parts.len() {
                let ancestor = if length == 0 {
                    ".".to_owned()
                } else {
                    parts[..length].join("/")
                };
                resources.push(intent(
                    &ancestor,
                    if length + 1 >= ancestors.len() {
                        ResourceAccess::ReadWrite
                    } else {
                        ResourceAccess::Traverse
                    },
                    ancestors
                        .get(length)
                        .map(|id| revision(&format!("{id:?}")))
                        .transpose()?,
                )?);
            }
            for directory in &directories {
                resources.push(intent(directory.as_str(), ResourceAccess::Write, None)?);
            }
            let expected = match &precondition {
                TransferPrecondition::MustNotExist {} => None,
                TransferPrecondition::Revision { revision } => Some(revision.clone()),
            };
            resources.push(ResourceIntent {
                resource_id: scope.last().cloned().ok_or(BinaryError::Inaccessible)?,
                scope,
                display: DisplayText::new(path.as_str()).map_err(|_| BinaryError::Inaccessible)?,
                access: ResourceAccess::Write,
                revision: expected,
            });
            Ok(PreparedBinaryPublication {
                core,
                path,
                ancestors,
                create_directories: directories,
                precondition,
                content,
                maximum,
                resources,
                #[cfg(test)]
                before_publish: None,
            })
        })
        .await
        .map_err(|_| BinaryError::Inaccessible)?
    }

    pub async fn execute_binary_publication(
        &self,
        prepared: PreparedBinaryPublication,
        mut source: File,
        token: &CancellationToken,
    ) -> Result<TransferFile, BinaryError> {
        if !Arc::ptr_eq(&self.core, &prepared.core) {
            return Err(BinaryError::Inaccessible);
        }
        self.core
            .require_write()
            .map_err(|_| BinaryError::Inaccessible)?;
        let guard = mutation_guard(&self.core, token).await?;
        let token = token.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            #[cfg(test)]
            let mut prepared = prepared;
            let first = prepared
                .create_directories
                .first()
                .unwrap_or(&prepared.path);
            let (mut parent, mut ancestors, mut name) = open_parent(prepared.core.root(), first)?;
            if ancestors != prepared.ancestors {
                return Err(BinaryError::Conflict);
            }
            let mut created = false;
            let creation = (|| {
                for directory in &prepared.create_directories {
                    cancelled(&token)?;
                    let (_, current, _) = open_parent(prepared.core.root(), directory)?;
                    if current != ancestors {
                        return Err(BinaryError::Conflict);
                    }
                    // Every mkdir is exclusive, has a disclosed parent intent, and is durable
                    // before descending. Once any succeeds, every failure is indeterminate.
                    mkdirat(&parent, &name, Mode::from_raw_mode(0o755))
                        .map_err(|_| BinaryError::Conflict)?;
                    created = true;
                    parent.sync_all().map_err(|_| BinaryError::Indeterminate)?;
                    let child = open_child(&parent, &name, DIRECTORY_FLAGS)
                        .map_err(|_| BinaryError::Indeterminate)?;
                    reject_repository(&child)?;
                    ancestors.push(identity(
                        &child.metadata().map_err(|_| BinaryError::Inaccessible)?,
                    ));
                    parent = child;
                    let remaining = prepared
                        .path
                        .as_str()
                        .strip_prefix(directory.as_str())
                        .and_then(|s| s.strip_prefix('/'))
                        .ok_or(BinaryError::Conflict)?;
                    name = remaining
                        .split('/')
                        .next()
                        .ok_or(BinaryError::Conflict)?
                        .to_owned();
                }
                Ok(())
            })();
            if let Err(error) = creation {
                return Err(if created {
                    BinaryError::Indeterminate
                } else {
                    error
                });
            }
            let publication = (|| {
                validate_destination(
                    &parent,
                    &name,
                    &prepared.path,
                    &prepared.precondition,
                    prepared.maximum,
                    &token,
                )?;
                let temporary = format!(".workcell-publication-{}", Uuid::new_v4());
                let fd = openat(
                    &parent,
                    &temporary,
                    OFlags::CREATE
                        | OFlags::EXCL
                        | OFlags::RDWR
                        | OFlags::NOFOLLOW
                        | OFlags::CLOEXEC,
                    Mode::from_raw_mode(0o600),
                )
                .map_err(|_| BinaryError::Inaccessible)?;
                let mut staged = File::from(fd);
                let cleanup = PublicationTemporary {
                    parent: &parent,
                    name: &temporary,
                };
                let result = (|| {
                    source.rewind().map_err(|_| BinaryError::Integrity)?;
                    let (digest, size) = copy_digest(
                        &mut source,
                        Some(&mut staged),
                        prepared.content.size_bytes,
                        &token,
                    )?;
                    if digest != prepared.content.digest || size != prepared.content.size_bytes {
                        return Err(BinaryError::Integrity);
                    }
                    staged
                        .set_permissions(Permissions::from_mode(prepared.content.mode.bits()))
                        .map_err(|_| BinaryError::Inaccessible)?;
                    staged.sync_all().map_err(|_| BinaryError::Inaccessible)?;
                    let (_, current_ancestors, _) =
                        open_parent(prepared.core.root(), &prepared.path)?;
                    if current_ancestors != ancestors {
                        return Err(BinaryError::Conflict);
                    }
                    validate_destination(
                        &parent,
                        &name,
                        &prepared.path,
                        &prepared.precondition,
                        prepared.maximum,
                        &token,
                    )?;
                    cancelled(&token)?;
                    #[cfg(test)]
                    if let Some(hook) = prepared.before_publish.take() {
                        hook();
                    }
                    match prepared.precondition {
                        TransferPrecondition::MustNotExist {} => {
                            linkat(&parent, &temporary, &parent, &name, AtFlags::empty())
                                .map_err(|_| BinaryError::Conflict)?
                        }
                        TransferPrecondition::Revision { .. } => {
                            renameat(&parent, &temporary, &parent, &name)
                                .map_err(|_| BinaryError::Indeterminate)?
                        }
                    }
                    cleanup.remove()?;
                    parent.sync_all().map_err(|_| BinaryError::Indeterminate)?;
                    // Once the namespace changed, cancellation or verification failure is indeterminate.
                    let (_, current, _) = open_parent(prepared.core.root(), &prepared.path)
                        .map_err(|_| BinaryError::Indeterminate)?;
                    if current != ancestors {
                        return Err(BinaryError::Indeterminate);
                    }
                    let mut published =
                        regular_file(&parent, &name).map_err(|_| BinaryError::Indeterminate)?;
                    if identity(
                        &published
                            .metadata()
                            .map_err(|_| BinaryError::Indeterminate)?,
                    ) != identity(&staged.metadata().map_err(|_| BinaryError::Indeterminate)?)
                    {
                        return Err(BinaryError::Indeterminate);
                    }
                    let mut file =
                        inspect_file(&mut published, &prepared.path, prepared.maximum, &token)
                            .map_err(|_| BinaryError::Indeterminate)?;
                    for (directory, id) in prepared
                        .create_directories
                        .iter()
                        .zip(ancestors.iter().skip(prepared.ancestors.len()))
                    {
                        file.created_directories
                            .push((directory.clone(), directory_identity(*id)?));
                    }
                    if file.digest != prepared.content.digest
                        || file.size_bytes != prepared.content.size_bytes
                    {
                        return Err(BinaryError::Indeterminate);
                    }
                    Ok(file)
                })();
                cleanup.remove()?;
                result
            })();
            if created {
                publication.map_err(|_| BinaryError::Indeterminate)
            } else {
                publication
            }
        })
        .await
        .map_err(|_| BinaryError::Indeterminate)?
    }
}

struct PublicationTemporary<'a> {
    parent: &'a File,
    name: &'a str,
}

impl PublicationTemporary<'_> {
    fn remove(&self) -> Result<(), BinaryError> {
        match unlinkat(self.parent, self.name, AtFlags::empty()) {
            Ok(()) | Err(rustix::io::Errno::NOENT) => Ok(()),
            Err(_) => Err(BinaryError::Indeterminate),
        }
    }
}

impl Drop for PublicationTemporary<'_> {
    fn drop(&mut self) {
        let _ = unlinkat(self.parent, self.name, AtFlags::empty());
    }
}

pub(super) fn open_parent(
    root: &Path,
    path: &WorkspacePath,
) -> Result<(File, AncestorIdentities, String), BinaryError> {
    let mut directory = open_root(root)?;
    let mut identities = vec![identity(
        &directory
            .metadata()
            .map_err(|_| BinaryError::Inaccessible)?,
    )];
    let mut parts = path.as_str().split('/').peekable();
    while let Some(part) = parts.next() {
        if part == "." || part.is_empty() {
            return Err(BinaryError::Inaccessible);
        }
        if parts.peek().is_none() {
            return Ok((directory, identities, part.to_owned()));
        }
        directory =
            open_child(&directory, part, DIRECTORY_FLAGS).map_err(|_| BinaryError::Inaccessible)?;
        reject_repository(&directory)?;
        identities.push(identity(
            &directory
                .metadata()
                .map_err(|_| BinaryError::Inaccessible)?,
        ));
    }
    Err(BinaryError::Inaccessible)
}

pub(super) fn identity(metadata: &Metadata) -> (u64, u64) {
    (metadata.dev(), metadata.ino())
}

pub(super) fn regular_file(parent: &File, name: &str) -> Result<File, BinaryError> {
    #[cfg(target_os = "linux")]
    let descriptor = open_child(
        parent,
        name,
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
    )
    .map_err(|_| BinaryError::Inaccessible)?;
    #[cfg(not(target_os = "linux"))]
    return Err(BinaryError::Inaccessible);
    #[cfg(target_os = "linux")]
    {
        let before = descriptor
            .metadata()
            .map_err(|_| BinaryError::Inaccessible)?;
        if !before.is_file() {
            return Err(BinaryError::Inaccessible);
        }
        // Reopen the already validated regular inode, not its replaceable directory entry.
        let file = File::from(
            open(
                format!("/proc/self/fd/{}", descriptor.as_raw_fd()),
                OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK,
                Mode::empty(),
            )
            .map_err(|_| BinaryError::Inaccessible)?,
        );
        if !file
            .metadata()
            .map_err(|_| BinaryError::Inaccessible)?
            .is_file()
        {
            return Err(BinaryError::Inaccessible);
        }
        Ok(file)
    }
}

fn validate_destination(
    parent: &File,
    name: &str,
    path: &WorkspacePath,
    expected: &TransferPrecondition,
    maximum: u64,
    token: &CancellationToken,
) -> Result<(), BinaryError> {
    cancelled(token)?;
    match expected {
        TransferPrecondition::MustNotExist {} => {
            match rustix::fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
                Err(rustix::io::Errno::NOENT) => Ok(()),
                _ => Err(BinaryError::Conflict),
            }
        }
        TransferPrecondition::Revision { revision } => {
            let mut file = regular_file(parent, name).map_err(|_| BinaryError::Conflict)?;
            if inspect_file(&mut file, path, maximum, token)?.revision != *revision {
                return Err(BinaryError::Conflict);
            }
            Ok(())
        }
    }
}

pub(super) fn stamp(metadata: &Metadata) -> (u64, u64, u64, u32, i64, i64, i64, i64) {
    (
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mode(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
    )
}

fn inspect_file(
    file: &mut File,
    path: &WorkspacePath,
    maximum: u64,
    token: &CancellationToken,
) -> Result<TransferFile, BinaryError> {
    let before = file.metadata().map_err(|_| BinaryError::Inaccessible)?;
    if !before.is_file() || before.len() > maximum {
        return Err(BinaryError::Integrity);
    }
    let (digest, size_bytes) = digest_file(file, maximum, token)?;
    let after = file.metadata().map_err(|_| BinaryError::Inaccessible)?;
    if stamp(&before) != stamp(&after) || size_bytes != after.len() {
        return Err(BinaryError::Conflict);
    }
    Ok(TransferFile {
        path: path.clone(),
        resource_id: root_relative_resource_id(RootResourceKind::Path, path.as_str())
            .map_err(|_| BinaryError::Inaccessible)?,
        revision: revision(&format!("{:?}:{}", stamp(&after), digest.as_str()))?,
        digest,
        size_bytes,
        mode: if after.mode() & 0o111 == 0 {
            TransferMode::Regular
        } else {
            TransferMode::Executable
        },
        created_directories: Vec::new(),
    })
}

/// Hash a bounded regular stream without retaining its bytes; leave it rewound for consumption.
pub fn digest_file(
    file: &mut File,
    maximum: u64,
    token: &CancellationToken,
) -> Result<(Revision, u64), BinaryError> {
    file.rewind().map_err(|_| BinaryError::Inaccessible)?;
    let result = copy_digest(file, None, maximum, token)?;
    file.seek(SeekFrom::Start(0))
        .map_err(|_| BinaryError::Inaccessible)?;
    Ok(result)
}

fn copy_digest(
    source: &mut File,
    mut destination: Option<&mut File>,
    maximum: u64,
    token: &CancellationToken,
) -> Result<(Revision, u64), BinaryError> {
    let mut buffer = [0u8; TRANSFER_STREAM_BUFFER_BYTES];
    let mut digest = Sha256::new();
    let mut size = 0u64;
    loop {
        cancelled(token)?;
        let count = source
            .read(&mut buffer)
            .map_err(|_| BinaryError::Inaccessible)?;
        if count == 0 {
            break;
        }
        size = size
            .checked_add(count as u64)
            .filter(|size| *size <= maximum)
            .ok_or(BinaryError::Integrity)?;
        digest.update(&buffer[..count]);
        if let Some(file) = destination.as_mut() {
            file.write_all(&buffer[..count])
                .map_err(|_| BinaryError::Inaccessible)?;
        }
    }
    Ok((digest_revision(digest.finalize())?, size))
}

fn revision(value: &str) -> Result<Revision, BinaryError> {
    digest_revision(Sha256::digest(value.as_bytes()))
}

pub(super) fn digest_revision(
    bytes: impl IntoIterator<Item = u8>,
) -> Result<Revision, BinaryError> {
    use std::fmt::Write as _;
    let mut value = String::from("sha256:");
    for byte in bytes {
        write!(value, "{byte:02x}").map_err(|_| BinaryError::Integrity)?;
    }
    Revision::new(value).map_err(|_| BinaryError::Integrity)
}

pub(super) fn directory_identity(id: (u64, u64)) -> Result<ResourceId, BinaryError> {
    ResourceId::new(format!(
        "directory:{}",
        revision(&format!("{id:?}"))?.as_str()
    ))
    .map_err(|_| BinaryError::Inaccessible)
}

fn intent(
    path: &str,
    access: ResourceAccess,
    revision: Option<Revision>,
) -> Result<ResourceIntent, BinaryError> {
    Ok(ResourceIntent {
        resource_id: root_relative_resource_id(RootResourceKind::Path, path)
            .map_err(|_| BinaryError::Inaccessible)?,
        scope: root_relative_resource_scope(RootResourceKind::Path, path)
            .map_err(|_| BinaryError::Inaccessible)?,
        display: DisplayText::new(path).map_err(|_| BinaryError::Inaccessible)?,
        access,
        revision,
    })
}

pub(super) fn cancelled(token: &CancellationToken) -> Result<(), BinaryError> {
    if token.is_cancelled() {
        Err(BinaryError::Cancelled)
    } else {
        Ok(())
    }
}

pub(super) async fn mutation_guard(
    core: &FilesystemCore,
    token: &CancellationToken,
) -> Result<OwnedMutexGuard<()>, BinaryError> {
    tokio::select! {
        biased;
        () = token.cancelled() => Err(BinaryError::Cancelled),
        guard = core.mutation.clone().lock_owned() => Ok(guard),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BinaryError, BinaryPublicationContent, PreparedBinaryPublication, digest_revision,
    };
    use crate::{FileToolGroup, FilesystemLimits};
    use sha2::{Digest, Sha256};
    use std::{fs, io::Write, os::unix::fs::symlink};
    use tempfile::{TempDir, tempfile};
    use tokio_util::sync::CancellationToken;
    use workcell_host_contract::{
        ResourceAccess, ResourceId, TransferMode, TransferPrecondition, WorkspacePath,
    };

    const PAYLOAD: &[u8] = b"\xff\0binary payload";
    const OTHER: &[u8] = b"external";
    const MAXIMUM: u64 = 1024;
    const TARGET: &str = "nested/target.bin";

    async fn fixture() -> (TempDir, FileToolGroup, ResourceId) {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("nested")).unwrap();
        let files = FileToolGroup::new(
            root.path(),
            true,
            Some(FilesystemLimits {
                max_file_bytes: 1,
                ..FilesystemLimits::default()
            }),
        )
        .await
        .unwrap();
        let cwd = files.workspace_root().await.unwrap().handle;
        (root, files, cwd)
    }

    async fn prepare(
        files: &FileToolGroup,
        cwd: &ResourceId,
        precondition: TransferPrecondition,
    ) -> PreparedBinaryPublication {
        files
            .prepare_binary_publication(
                cwd,
                &WorkspacePath::new(TARGET).unwrap(),
                precondition,
                BinaryPublicationContent {
                    digest: digest_revision(Sha256::digest(PAYLOAD)).unwrap(),
                    size_bytes: PAYLOAD.len() as u64,
                    mode: TransferMode::Regular,
                },
                MAXIMUM,
                &CancellationToken::new(),
            )
            .await
            .unwrap()
    }

    fn source(bytes: &[u8]) -> fs::File {
        let mut file = tempfile().unwrap();
        file.write_all(bytes).unwrap();
        file
    }

    #[tokio::test]
    async fn missing_directories_are_explicit_conditional_and_return_durable_ancestry() {
        let (root, files, cwd) = fixture().await;
        let path = WorkspacePath::new("new/deep/file").unwrap();
        let directories = vec![
            WorkspacePath::new("new").unwrap(),
            WorkspacePath::new("new/deep").unwrap(),
        ];
        let content = BinaryPublicationContent {
            digest: digest_revision(Sha256::digest(PAYLOAD)).unwrap(),
            size_bytes: PAYLOAD.len() as u64,
            mode: TransferMode::Regular,
        };
        assert!(
            files
                .prepare_binary_publication(
                    &cwd,
                    &path,
                    TransferPrecondition::MustNotExist {},
                    content.clone(),
                    MAXIMUM,
                    &CancellationToken::new()
                )
                .await
                .is_err()
        );
        assert!(!root.path().join("new").exists());
        let prepared = files
            .prepare_binary_publication_with_directories(
                &cwd,
                &path,
                TransferPrecondition::MustNotExist {},
                content,
                directories.clone(),
                MAXIMUM,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!root.path().join("new").exists());
        for directory in &directories {
            assert!(
                prepared
                    .resources()
                    .iter()
                    .any(|resource| resource.display.as_str() == directory.as_str()
                        && resource.access == ResourceAccess::Write
                        && resource.revision.is_none())
            );
        }
        let result = files
            .execute_binary_publication(prepared, source(PAYLOAD), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(fs::read(root.path().join(path.as_str())).unwrap(), PAYLOAD);
        assert_eq!(result.created_directories.len(), directories.len());
        for (path, id) in result.created_directories {
            assert_eq!(
                id,
                super::directory_identity(super::identity(
                    &fs::metadata(root.path().join(path.as_str())).unwrap()
                ))
                .unwrap()
            );
        }
    }

    #[tokio::test]
    async fn a_raced_directory_creation_is_not_reused_and_partial_creation_is_indeterminate() {
        let (root, files, cwd) = fixture().await;
        let path = WorkspacePath::new("new/file").unwrap();
        let content = BinaryPublicationContent {
            digest: digest_revision(Sha256::digest(PAYLOAD)).unwrap(),
            size_bytes: PAYLOAD.len() as u64,
            mode: TransferMode::Regular,
        };
        let token = CancellationToken::new();
        let prepared = files
            .prepare_binary_publication_with_directories(
                &cwd,
                &path,
                TransferPrecondition::MustNotExist {},
                content.clone(),
                vec![WorkspacePath::new("new").unwrap()],
                MAXIMUM,
                &token,
            )
            .await
            .unwrap();
        fs::create_dir(root.path().join("new")).unwrap();
        assert!(matches!(
            files
                .execute_binary_publication(prepared, source(PAYLOAD), &token)
                .await,
            Err(BinaryError::Conflict)
        ));
        fs::remove_dir(root.path().join("new")).unwrap();
        let mut prepared = files
            .prepare_binary_publication_with_directories(
                &cwd,
                &path,
                TransferPrecondition::MustNotExist {},
                content,
                vec![WorkspacePath::new("new").unwrap()],
                MAXIMUM,
                &token,
            )
            .await
            .unwrap();
        let target = root.path().join("new/file");
        prepared.before_publish = Some(Box::new(move || fs::write(target, OTHER).unwrap()));
        assert!(matches!(
            files
                .execute_binary_publication(prepared, source(PAYLOAD), &token)
                .await,
            Err(BinaryError::Indeterminate)
        ));
        assert_eq!(fs::read(root.path().join("new/file")).unwrap(), OTHER);
    }

    #[tokio::test]
    async fn binary_publication_bypasses_text_limits_and_discloses_canonical_ancestor_effects() {
        let (root, files, cwd) = fixture().await;
        let prepared = prepare(&files, &cwd, TransferPrecondition::MustNotExist {}).await;
        assert_eq!(prepared.resources.len(), 3);
        for resource in prepared.resources() {
            resource.validate().unwrap();
        }
        let output = files
            .execute_binary_publication(prepared, source(PAYLOAD), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(fs::read(root.path().join(TARGET)).unwrap(), PAYLOAD);
        assert_eq!(
            output,
            files
                .open_binary(
                    &cwd,
                    &WorkspacePath::new(TARGET).unwrap(),
                    MAXIMUM,
                    &CancellationToken::new()
                )
                .await
                .unwrap()
                .metadata
        );
        assert_eq!(fs::read_dir(root.path().join("nested")).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn no_replace_survives_creation_between_final_validation_and_publication() {
        let (root, files, cwd) = fixture().await;
        let mut prepared = prepare(&files, &cwd, TransferPrecondition::MustNotExist {}).await;
        let target = root.path().join(TARGET);
        let conflicting = target.clone();
        prepared.before_publish = Some(Box::new(move || fs::write(conflicting, OTHER).unwrap()));
        assert!(matches!(
            files
                .execute_binary_publication(prepared, source(PAYLOAD), &CancellationToken::new())
                .await,
            Err(BinaryError::Conflict)
        ));
        assert_eq!(fs::read(target).unwrap(), OTHER);
        assert_eq!(fs::read_dir(root.path().join("nested")).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn stale_destination_and_changed_source_never_publish() {
        let (root, files, cwd) = fixture().await;
        fs::write(root.path().join(TARGET), OTHER).unwrap();
        let revision = files
            .open_binary(
                &cwd,
                &WorkspacePath::new(TARGET).unwrap(),
                MAXIMUM,
                &CancellationToken::new(),
            )
            .await
            .unwrap()
            .metadata
            .revision;
        let prepared = prepare(&files, &cwd, TransferPrecondition::Revision { revision }).await;
        fs::write(root.path().join(TARGET), b"changed").unwrap();
        assert!(matches!(
            files
                .execute_binary_publication(prepared, source(PAYLOAD), &CancellationToken::new())
                .await,
            Err(BinaryError::Conflict)
        ));
        fs::remove_file(root.path().join(TARGET)).unwrap();
        for payload in [OTHER, &[0u8; PAYLOAD.len()], &[0u8; MAXIMUM as usize + 1]] {
            let prepared = prepare(&files, &cwd, TransferPrecondition::MustNotExist {}).await;
            assert!(matches!(
                files
                    .execute_binary_publication(
                        prepared,
                        source(payload),
                        &CancellationToken::new()
                    )
                    .await,
                Err(BinaryError::Integrity)
            ));
            assert!(!root.path().join(TARGET).exists());
            assert_eq!(fs::read_dir(root.path().join("nested")).unwrap().count(), 0);
        }
    }

    #[tokio::test]
    async fn symlink_targets_and_rebound_ancestors_are_refused() {
        let (root, files, cwd) = fixture().await;
        fs::write(root.path().join("original"), OTHER).unwrap();
        symlink(root.path().join("original"), root.path().join(TARGET)).unwrap();
        assert!(
            files
                .open_binary(
                    &cwd,
                    &WorkspacePath::new(TARGET).unwrap(),
                    MAXIMUM,
                    &CancellationToken::new()
                )
                .await
                .is_err()
        );
        fs::remove_file(root.path().join(TARGET)).unwrap();
        let prepared = prepare(&files, &cwd, TransferPrecondition::MustNotExist {}).await;
        fs::rename(root.path().join("nested"), root.path().join("old")).unwrap();
        fs::create_dir(root.path().join("nested")).unwrap();
        assert!(
            files
                .execute_binary_publication(prepared, source(PAYLOAD), &CancellationToken::new())
                .await
                .is_err()
        );
        assert!(!root.path().join(TARGET).exists());
        assert_eq!(fs::read(root.path().join("original")).unwrap(), OTHER);
    }

    #[tokio::test]
    async fn cancellation_does_not_wait_for_a_held_shared_mutation_lock() {
        let (root, files, cwd) = fixture().await;
        let prepared = prepare(&files, &cwd, TransferPrecondition::MustNotExist {}).await;
        let _guard = files
            .workspace_snapshot_access()
            .mutation_guard()
            .await
            .unwrap();
        let token = CancellationToken::new();
        token.cancel();
        assert!(matches!(
            files
                .execute_binary_publication(prepared, source(PAYLOAD), &token)
                .await,
            Err(BinaryError::Cancelled)
        ));
        assert!(!root.path().join(TARGET).exists());
    }
}
