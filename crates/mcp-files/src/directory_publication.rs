use std::{
    fs::{File, Permissions},
    mem::size_of,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};

use rustix::fs::{AtFlags, Mode, mkdirat, statat, unlinkat};
#[cfg(target_os = "linux")]
use rustix::fs::{RenameFlags, renameat_with};
use rustix::io::Errno;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workcell_host_contract::{
    MAX_TRANSFER_DEPTH, MAX_TRANSFER_JOURNAL_BYTES, ResourceAccess, ResourceId, ResourceIntent,
    TransferDirectory, TransferPrecondition, WorkspacePath,
};

use crate::{
    BinaryError, FileToolGroup,
    binary::{
        DIRECTORY_FLAGS, cancelled, directory_identity, identity, intent, mutation_guard,
        open_child, open_parent, open_root, reject_repository, revision,
    },
    operations::FilesystemCore,
};

const DIRECTORY_MODE: Mode = Mode::from_raw_mode(0o755);
const PRIVATE_DIRECTORY_MODE: Mode = Mode::from_raw_mode(0o700);
const MAX_DIRECTORY_PATH_BYTES: usize = MAX_TRANSFER_JOURNAL_BYTES as usize / 4;
const MAX_DIRECTORY_RECEIPT_BYTES: usize = MAX_TRANSFER_JOURNAL_BYTES as usize / 2;

#[cfg(test)]
type StagingHook = Box<dyn FnOnce(&str) + Send>;

#[derive(Clone)]
pub struct DirectoryPublicationStaging(Arc<DirectoryStagingRoot>);

struct DirectoryStagingRoot {
    path: PathBuf,
    directory: File,
}

impl DirectoryPublicationStaging {
    pub fn open(root: &Path) -> Result<Self, BinaryError> {
        let directory = open_root(root)?;
        let metadata = directory
            .metadata()
            .map_err(|_| BinaryError::Inaccessible)?;
        if metadata.uid() != rustix::process::geteuid().as_raw() || metadata.mode() & 0o077 != 0 {
            return Err(BinaryError::Inaccessible);
        }
        let path = root.canonicalize().map_err(|_| BinaryError::Inaccessible)?;
        Ok(Self(Arc::new(DirectoryStagingRoot { path, directory })))
    }

    fn validate(&self, workspace: &Path, destination_parent: &File) -> Result<(), BinaryError> {
        if self.0.path.starts_with(workspace) || workspace.starts_with(&self.0.path) {
            return Err(BinaryError::Inaccessible);
        }
        let current = open_root(&self.0.path)?
            .metadata()
            .map_err(|_| BinaryError::Inaccessible)?;
        let pinned = self
            .0
            .directory
            .metadata()
            .map_err(|_| BinaryError::Inaccessible)?;
        let target = destination_parent
            .metadata()
            .map_err(|_| BinaryError::Inaccessible)?;
        if identity(&current) != identity(&pinned)
            || pinned.mode() & 0o077 != 0
            || pinned.uid() != rustix::process::geteuid().as_raw()
            || target.dev() != pinned.dev()
        {
            return Err(BinaryError::Inaccessible);
        }
        Ok(())
    }
}

pub struct PreparedDirectoryPublication {
    core: Arc<FilesystemCore>,
    path: WorkspacePath,
    directories: Vec<WorkspacePath>,
    ancestors: Vec<(u64, u64)>,
    resources: Vec<ResourceIntent>,
    receipt_size_bound: usize,
    staging: DirectoryPublicationStaging,
    _ancestor_anchor: File,
    #[cfg(test)]
    after_create: Option<Box<dyn FnOnce() + Send>>,
    #[cfg(test)]
    after_stage_create: Option<StagingHook>,
    #[cfg(test)]
    before_publish: Option<StagingHook>,
}

impl PreparedDirectoryPublication {
    #[must_use]
    pub fn receipt_size_bound(&self) -> usize {
        self.receipt_size_bound
    }

    #[must_use]
    pub fn resources(&self) -> &[ResourceIntent] {
        &self.resources
    }

    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            + self.path.retained_bytes()
            + self.directories.capacity() * size_of::<WorkspacePath>()
            + self
                .directories
                .iter()
                .map(WorkspacePath::retained_bytes)
                .sum::<usize>()
            + self.ancestors.capacity() * size_of::<(u64, u64)>()
            + self.resources.capacity() * size_of::<ResourceIntent>()
            + serde_json::to_vec(&self.resources).map_or(usize::MAX / 2, |bytes| bytes.len() * 2)
    }
}

impl FileToolGroup {
    #[allow(clippy::too_many_arguments)]
    pub async fn prepare_directory_publication(
        &self,
        staging: &DirectoryPublicationStaging,
        cwd: &ResourceId,
        path: &WorkspacePath,
        precondition: TransferPrecondition,
        create_directories: Vec<WorkspacePath>,
        token: &CancellationToken,
    ) -> Result<PreparedDirectoryPublication, BinaryError> {
        self.core
            .require_write()
            .map_err(|_| BinaryError::Inaccessible)?;
        if !cfg!(target_os = "linux") {
            return Err(BinaryError::Inaccessible);
        }
        if !matches!(precondition, TransferPrecondition::MustNotExist {}) {
            return Err(BinaryError::Conflict);
        }
        if create_directories.len() >= MAX_TRANSFER_DEPTH
            || create_directories
                .iter()
                .map(|path| path.as_str().len())
                .sum::<usize>()
                + path.as_str().len()
                > MAX_DIRECTORY_PATH_BYTES
        {
            return Err(BinaryError::Integrity);
        }
        let path = self.binary_path(cwd, path).await?;
        if path.as_str().split('/').count() > MAX_TRANSFER_DEPTH {
            return Err(BinaryError::Integrity);
        }
        let mut directories = Vec::with_capacity(create_directories.len() + 1);
        for directory in create_directories {
            directories.push(self.binary_path(cwd, &directory).await?);
        }
        directories.push(path.clone());
        if directories
            .iter()
            .map(|path| path.as_str().len())
            .sum::<usize>()
            > MAX_DIRECTORY_PATH_BYTES
        {
            return Err(BinaryError::Integrity);
        }
        let placeholder = directory_identity((0, 0))?;
        let receipt = TransferDirectory {
            path: path.clone(),
            resource_id: placeholder.clone(),
            created_directories: directories
                .iter()
                .take(directories.len() - 1)
                .map(|path| (path.clone(), placeholder.clone()))
                .collect(),
        };
        let receipt_size_bound = serde_json::to_vec(&receipt)
            .map_err(|_| BinaryError::Integrity)?
            .len();
        if receipt_size_bound > MAX_DIRECTORY_RECEIPT_BYTES {
            return Err(BinaryError::Integrity);
        }
        let core = self.core.clone();
        let staging = staging.clone();
        let token = token.clone();
        let guard = mutation_guard(&core, &token).await?;
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            cancelled(&token)?;
            let (parent, ancestors, name) = open_parent(core.root(), &directories[0])?;
            staging.validate(core.root(), &parent)?;
            open_child(&parent, ".", DIRECTORY_FLAGS).map_err(|_| BinaryError::Inaccessible)?;
            match statat(&parent, &name, AtFlags::SYMLINK_NOFOLLOW) {
                Err(Errno::NOENT) => (),
                _ => return Err(BinaryError::Conflict),
            }
            let parts = path.as_str().split('/').collect::<Vec<_>>();
            let expected = (ancestors.len()..=parts.len()).map(|length| parts[..length].join("/"));
            if directories.iter().map(WorkspacePath::as_str).ne(expected) {
                return Err(BinaryError::Conflict);
            }
            let mut resources = Vec::new();
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
            Ok(PreparedDirectoryPublication {
                core,
                path,
                directories,
                ancestors,
                resources,
                receipt_size_bound,
                staging,
                _ancestor_anchor: parent,
                #[cfg(test)]
                after_create: None,
                #[cfg(test)]
                after_stage_create: None,
                #[cfg(test)]
                before_publish: None,
            })
        })
        .await
        .map_err(|_| BinaryError::Inaccessible)?
    }

    pub async fn execute_directory_publication(
        &self,
        prepared: PreparedDirectoryPublication,
        token: &CancellationToken,
    ) -> Result<TransferDirectory, BinaryError> {
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
            let mut ancestors = prepared.ancestors.clone();
            let mut published_directories = Vec::with_capacity(prepared.directories.len());
            let mut created = false;
            let result = (|| {
                for directory in &prepared.directories {
                    cancelled(&token)?;
                    let (parent, current, name) = open_parent(prepared.core.root(), directory)?;
                    if current != ancestors {
                        return Err(BinaryError::Conflict);
                    }
                    prepared.staging.validate(prepared.core.root(), &parent)?;
                    let mut staged = StagedDirectory::create(
                        &prepared.staging.0.directory,
                        #[cfg(test)]
                        prepared.after_stage_create.take(),
                    )?;
                    let publication = (|| {
                        let id = identity(
                            &staged
                                .directory
                                .metadata()
                                .map_err(|_| BinaryError::Indeterminate)?,
                        );
                        let (_, current, _) = open_parent(prepared.core.root(), directory)?;
                        if current != ancestors {
                            return Err(BinaryError::Conflict);
                        }
                        cancelled(&token)?;
                        prepared.staging.validate(prepared.core.root(), &parent)?;
                        #[cfg(test)]
                        if let Some(hook) = prepared.before_publish.take() {
                            hook(&staged.name);
                        }
                        cancelled(&token)?;
                        staged.publish(&parent, &name)?;
                        created = true;
                        #[cfg(test)]
                        if let Some(hook) = prepared.after_create.take() {
                            hook();
                        }
                        parent.sync_all().map_err(|_| BinaryError::Indeterminate)?;
                        let child = open_child(&parent, &name, DIRECTORY_FLAGS)
                            .map_err(|_| BinaryError::Indeterminate)?;
                        if identity(&child.metadata().map_err(|_| BinaryError::Indeterminate)?)
                            != id
                        {
                            return Err(BinaryError::Indeterminate);
                        }
                        reject_repository(&child)?;
                        child.sync_all().map_err(|_| BinaryError::Indeterminate)?;
                        ancestors.push(id);
                        published_directories.push(child);
                        Ok(())
                    })();
                    staged.cleanup()?;
                    publication?;
                }
                cancelled(&token)?;
                let (parent, current, name) = open_parent(prepared.core.root(), &prepared.path)?;
                if current != ancestors[..ancestors.len() - 1] {
                    return Err(BinaryError::Indeterminate);
                }
                let child = open_child(&parent, &name, DIRECTORY_FLAGS)
                    .map_err(|_| BinaryError::Indeterminate)?;
                reject_repository(&child)?;
                let id = identity(&child.metadata().map_err(|_| BinaryError::Indeterminate)?);
                if Some(&id) != ancestors.last() {
                    return Err(BinaryError::Indeterminate);
                }
                let created_directories = prepared
                    .directories
                    .iter()
                    .take(prepared.directories.len() - 1)
                    .zip(ancestors.iter().skip(prepared.ancestors.len()))
                    .map(|(path, id)| Ok((path.clone(), directory_identity(*id)?)))
                    .collect::<Result<Vec<_>, BinaryError>>()?;
                Ok(TransferDirectory {
                    path: prepared.path,
                    resource_id: directory_identity(id)?,
                    created_directories,
                })
            })();
            if created {
                result.map_err(|_| BinaryError::Indeterminate)
            } else {
                result
            }
        })
        .await
        .map_err(|_| BinaryError::Indeterminate)?
    }
}

struct StagedDirectory<'a> {
    parent: &'a File,
    name: String,
    directory: File,
    published: bool,
}

impl<'a> StagedDirectory<'a> {
    fn create(
        parent: &'a File,
        #[cfg(test)] after_create: Option<StagingHook>,
    ) -> Result<Self, BinaryError> {
        let name = format!(".workcell-directory-{}", Uuid::new_v4());
        mkdirat(parent, &name, PRIVATE_DIRECTORY_MODE).map_err(|_| BinaryError::Inaccessible)?;
        #[cfg(test)]
        if let Some(hook) = after_create {
            hook(&name);
        }
        let directory =
            open_child(parent, &name, DIRECTORY_FLAGS).map_err(|_| BinaryError::Indeterminate)?;
        let staged = Self {
            parent,
            name,
            directory,
            published: false,
        };
        let initialized = (|| {
            staged
                .directory
                .set_permissions(Permissions::from_mode(DIRECTORY_MODE.as_raw_mode()))
                .map_err(|_| BinaryError::Inaccessible)?;
            staged
                .directory
                .sync_all()
                .map_err(|_| BinaryError::Inaccessible)
        })();
        if let Err(error) = initialized {
            staged.cleanup()?;
            return Err(error);
        }
        Ok(staged)
    }

    fn publish(&mut self, destination: &File, name: &str) -> Result<(), BinaryError> {
        #[cfg(target_os = "linux")]
        {
            renameat_with(
                self.parent,
                &self.name,
                destination,
                name,
                RenameFlags::NOREPLACE,
            )
            .map_err(|_| BinaryError::Conflict)?;
            self.published = true;
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (destination, name);
            Err(BinaryError::Inaccessible)
        }
    }
    fn cleanup(self) -> Result<(), BinaryError> {
        if !self.published {
            let current = open_child(self.parent, &self.name, DIRECTORY_FLAGS)
                .map_err(|_| BinaryError::Indeterminate)?;
            let current = current.metadata().map_err(|_| BinaryError::Indeterminate)?;
            let staged = self
                .directory
                .metadata()
                .map_err(|_| BinaryError::Indeterminate)?;
            if identity(&current) != identity(&staged) {
                return Err(BinaryError::Indeterminate);
            }
            unlinkat(self.parent, &self.name, AtFlags::REMOVEDIR)
                .map_err(|_| BinaryError::Indeterminate)?;
        }
        self.parent
            .sync_all()
            .map_err(|_| BinaryError::Indeterminate)
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        sync::{Arc, Mutex},
    };

    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;
    use workcell_host_contract::{
        DirectoryNavigation, ResourceAccess, ResourceId, TransferInventoryPolicy,
        TransferPrecondition, WorkspacePath,
    };

    use crate::binary::{directory_identity, identity};
    use crate::{BinaryError, DirectoryPublicationStaging, FileToolGroup};

    fn path(value: &str) -> WorkspacePath {
        WorkspacePath::new(value).unwrap()
    }

    async fn fixture(
        write: bool,
    ) -> (
        TempDir,
        TempDir,
        FileToolGroup,
        ResourceId,
        DirectoryPublicationStaging,
    ) {
        let root = tempfile::tempdir().unwrap();
        let files = FileToolGroup::new(root.path(), write, None).await.unwrap();
        let cwd = files.workspace_root().await.unwrap().handle;
        let private = tempfile::tempdir().unwrap();
        fs::set_permissions(private.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let staging = DirectoryPublicationStaging::open(private.path()).unwrap();
        (root, private, files, cwd, staging)
    }

    #[tokio::test]
    async fn workspace_stage_substitution_before_first_open_cannot_supply_the_published_inode() {
        let (root, private, files, cwd, staging) = fixture(true).await;
        let token = CancellationToken::new();
        let mut prepared = files
            .prepare_directory_publication(
                &staging,
                &cwd,
                &path("empty"),
                TransferPrecondition::MustNotExist {},
                vec![],
                &token,
            )
            .await
            .unwrap();
        let observed = Arc::new(Mutex::new(None));
        let expected = observed.clone();
        let workspace = root.path().to_owned();
        let trusted = private.path().to_owned();
        prepared.after_stage_create = Some(Box::new(move |name| {
            *observed.lock().unwrap() = Some(identity(&fs::metadata(trusted.join(name)).unwrap()));
            let candidate = workspace.join(name);
            assert!(!candidate.exists());
            fs::create_dir(&candidate).unwrap();
            fs::rename(&candidate, workspace.join("foreign-old")).unwrap();
            fs::create_dir(&candidate).unwrap();
            fs::set_permissions(&candidate, fs::Permissions::from_mode(0o700)).unwrap();
            fs::write(candidate.join("foreign"), b"never publish").unwrap();
        }));
        let receipt = files
            .execute_directory_publication(prepared, &token)
            .await
            .unwrap();
        assert_eq!(
            receipt.resource_id,
            directory_identity(expected.lock().unwrap().unwrap()).unwrap()
        );
        assert_eq!(fs::read_dir(root.path().join("empty")).unwrap().count(), 0);
        let foreign = fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".workcell-directory-")
            })
            .unwrap();
        assert_eq!(
            fs::metadata(&foreign).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(fs::read(foreign.join("foreign")).unwrap(), b"never publish");
        assert_eq!(fs::read_dir(private.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn replacing_the_private_root_before_first_open_never_retargets_its_pinned_descriptor() {
        let (root, private, files, cwd, staging) = fixture(true).await;
        let token = CancellationToken::new();
        let displaced = tempfile::tempdir().unwrap();
        let moved = displaced.path().join("private");
        let mut prepared = files
            .prepare_directory_publication(
                &staging,
                &cwd,
                &path("empty"),
                TransferPrecondition::MustNotExist {},
                vec![],
                &token,
            )
            .await
            .unwrap();
        let trusted = private.path().to_owned();
        let moved_for_hook = moved.clone();
        prepared.after_stage_create = Some(Box::new(move |name| {
            fs::rename(&trusted, &moved_for_hook).unwrap();
            fs::create_dir(&trusted).unwrap();
            fs::create_dir(trusted.join(name)).unwrap();
            fs::write(trusted.join(name).join("foreign"), b"preserved").unwrap();
        }));
        assert!(matches!(
            files.execute_directory_publication(prepared, &token).await,
            Err(BinaryError::Inaccessible)
        ));
        assert!(!root.path().join("empty").exists());
        assert_eq!(fs::read_dir(moved).unwrap().count(), 0);
        let foreign = fs::read_dir(private.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(foreign.join("foreign").exists());
    }

    #[tokio::test]
    async fn cleanup_failure_or_moved_staging_is_indeterminate_even_before_final_publication() {
        for failure in ["none", "nonempty", "moved", "replaced"] {
            for cancelled in [false, true] {
                let (root, private, files, cwd, staging) = fixture(true).await;
                let token = CancellationToken::new();
                let mut prepared = files
                    .prepare_directory_publication(
                        &staging,
                        &cwd,
                        &path("empty"),
                        TransferPrecondition::MustNotExist {},
                        vec![],
                        &token,
                    )
                    .await
                    .unwrap();
                let trusted = private.path().to_owned();
                let target = root.path().join("empty");
                let cancellation = token.clone();
                prepared.before_publish = Some(Box::new(move |name| {
                    match failure {
                        "nonempty" => {
                            fs::write(trusted.join(name).join("block-cleanup"), b"preserved")
                                .unwrap()
                        }
                        "moved" | "replaced" => {
                            fs::rename(trusted.join(name), trusted.join("moved")).unwrap();
                            if failure == "replaced" {
                                fs::create_dir(trusted.join(name)).unwrap();
                            }
                        }
                        _ => (),
                    }
                    if cancelled {
                        cancellation.cancel();
                    } else {
                        fs::create_dir(target).unwrap();
                    }
                }));
                let result = files.execute_directory_publication(prepared, &token).await;
                if failure == "none" {
                    if cancelled {
                        assert!(matches!(result, Err(BinaryError::Cancelled)));
                    } else {
                        assert!(matches!(result, Err(BinaryError::Conflict)));
                    }
                    assert_eq!(fs::read_dir(private.path()).unwrap().count(), 0);
                } else {
                    assert!(matches!(result, Err(BinaryError::Indeterminate)));
                    assert!(fs::read_dir(private.path()).unwrap().count() > 0);
                }
                assert_eq!(root.path().join("empty").exists(), !cancelled);
            }
        }
    }

    #[tokio::test]
    async fn staging_requires_private_disjoint_same_device_storage() {
        let (root, private, files, cwd, _staging) = fixture(true).await;
        fs::set_permissions(private.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(DirectoryPublicationStaging::open(private.path()).is_err());
        let nested = root.path().join("private");
        fs::create_dir(&nested).unwrap();
        fs::set_permissions(&nested, fs::Permissions::from_mode(0o700)).unwrap();
        let nested = DirectoryPublicationStaging::open(&nested).unwrap();
        assert!(matches!(
            files
                .prepare_directory_publication(
                    &nested,
                    &cwd,
                    &path("empty"),
                    TransferPrecondition::MustNotExist {},
                    vec![],
                    &CancellationToken::new()
                )
                .await,
            Err(BinaryError::Inaccessible)
        ));
        assert!(!root.path().join("empty").exists());
        fs::set_permissions(private.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let staging = DirectoryPublicationStaging::open(private.path()).unwrap();
        let proc = FileToolGroup::new("/proc", true, None).await.unwrap();
        let cwd = proc.workspace_root().await.unwrap().handle;
        assert!(matches!(
            proc.prepare_directory_publication(
                &staging,
                &cwd,
                &path("workcell-new-directory"),
                TransferPrecondition::MustNotExist {},
                vec![],
                &CancellationToken::new()
            )
            .await,
            Err(BinaryError::Inaccessible)
        ));
        assert_eq!(fs::read_dir(private.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn nested_empty_directories_are_explicit_authorized_and_have_inventory_identities() {
        let (root, _private, files, cwd, staging) = fixture(true).await;
        let token = CancellationToken::new();
        for missing in [
            vec![],
            vec![path("a/b")],
            vec![path("a"), path("elsewhere")],
        ] {
            assert!(
                files
                    .prepare_directory_publication(
                        &staging,
                        &cwd,
                        &path("a/b/c"),
                        TransferPrecondition::MustNotExist {},
                        missing,
                        &token
                    )
                    .await
                    .is_err()
            );
            assert!(!root.path().join("a").exists());
        }
        let prepared = files
            .prepare_directory_publication(
                &staging,
                &cwd,
                &path("a/b/c"),
                TransferPrecondition::MustNotExist {},
                vec![path("a"), path("a/b")],
                &token,
            )
            .await
            .unwrap();
        for name in [".", "a", "a/b", "a/b/c"] {
            assert!(
                prepared
                    .resources()
                    .iter()
                    .any(|intent| intent.display.as_str() == name
                        && matches!(
                            intent.access,
                            ResourceAccess::Write | ResourceAccess::ReadWrite
                        ))
            );
        }
        assert!(!root.path().join("a").exists());
        let receipt = files
            .execute_directory_publication(prepared, &token)
            .await
            .unwrap();
        assert_eq!(receipt.created_directories.len(), 2);
        assert_eq!(fs::read_dir(root.path().join("a/b/c")).unwrap().count(), 0);
        let inventory = files
            .transfer_inventory(&cwd, None, TransferInventoryPolicy::default(), &token)
            .await
            .unwrap();
        assert_eq!(
            receipt.resource_id,
            inventory
                .entries
                .iter()
                .find(|node| node.path == receipt.path)
                .unwrap()
                .resource_id
        );
        for (path, id) in receipt.created_directories {
            assert_eq!(
                id,
                inventory
                    .entries
                    .iter()
                    .find(|node| node.path == path)
                    .unwrap()
                    .resource_id
            );
        }
        let prepared = files
            .prepare_directory_publication(
                &staging,
                &cwd,
                &path("a/b/second"),
                TransferPrecondition::MustNotExist {},
                vec![],
                &token,
            )
            .await
            .unwrap();
        assert!(
            files
                .execute_directory_publication(prepared, &token)
                .await
                .unwrap()
                .created_directories
                .is_empty()
        );
    }

    #[tokio::test]
    async fn directory_publication_refuses_native_deny_protected_paths_symlinks_and_collisions() {
        let (root, _private, files, cwd, staging) = fixture(false).await;
        let token = CancellationToken::new();
        assert!(matches!(
            files
                .prepare_directory_publication(
                    &staging,
                    &cwd,
                    &path("empty"),
                    TransferPrecondition::MustNotExist {},
                    vec![],
                    &token
                )
                .await,
            Err(BinaryError::Inaccessible)
        ));
        assert!(!root.path().join("empty").exists());
        let (root, _private, files, cwd, staging) = fixture(true).await;
        fs::create_dir(root.path().join("existing")).unwrap();
        fs::write(root.path().join("file"), b"preserved").unwrap();
        symlink("existing", root.path().join("link")).unwrap();
        fs::create_dir_all(root.path().join("repo/.git")).unwrap();
        for name in [
            "existing",
            "file",
            "link",
            "link/empty",
            ".git",
            ".ssh",
            "repo/empty",
        ] {
            assert!(
                files
                    .prepare_directory_publication(
                        &staging,
                        &cwd,
                        &path(name),
                        TransferPrecondition::MustNotExist {},
                        vec![],
                        &token
                    )
                    .await
                    .is_err(),
                "{name}"
            );
        }
        assert_eq!(fs::read(root.path().join("file")).unwrap(), b"preserved");
    }

    #[tokio::test]
    async fn stale_parents_and_destination_races_do_not_publish() {
        for race in ["parent", "destination"] {
            let (root, _private, files, cwd, staging) = fixture(true).await;
            let token = CancellationToken::new();
            fs::create_dir(root.path().join("parent")).unwrap();
            let prepared = files
                .prepare_directory_publication(
                    &staging,
                    &cwd,
                    &path("parent/empty"),
                    TransferPrecondition::MustNotExist {},
                    vec![],
                    &token,
                )
                .await
                .unwrap();
            if race == "parent" {
                fs::rename(root.path().join("parent"), root.path().join("old")).unwrap();
                fs::create_dir(root.path().join("parent")).unwrap();
            } else {
                fs::create_dir(root.path().join("parent/empty")).unwrap();
                fs::write(root.path().join("parent/empty/preserve"), b"preserve").unwrap();
            }
            assert!(matches!(
                files.execute_directory_publication(prepared, &token).await,
                Err(BinaryError::Conflict)
            ));
            assert!(!root.path().join("old/empty").exists());
            if race == "parent" {
                assert!(!root.path().join("parent/empty").exists());
            } else {
                assert!(root.path().join("parent/empty/preserve").exists());
            }
        }
    }

    #[tokio::test]
    async fn expanded_paths_and_json_escaped_receipts_are_bounded_before_creation() {
        let (root, _private, files, cwd, staging) = fixture(true).await;
        let token = CancellationToken::new();
        let prefix = vec!["x".repeat(200); 12].join("/");
        fs::create_dir_all(root.path().join(&prefix)).unwrap();
        let nested = files
            .workspace_resolve_directory(&cwd, &DirectoryNavigation::new(&prefix).unwrap())
            .await
            .unwrap();
        let missing = (1..20)
            .map(|length| path(&vec!["a"; length].join("/")))
            .collect();
        let result = files
            .prepare_directory_publication(
                &staging,
                &nested.handle,
                &path(&["a"; 20].join("/")),
                TransferPrecondition::MustNotExist {},
                missing,
                &token,
            )
            .await;
        assert!(matches!(result, Err(BinaryError::Integrity)));
        assert_eq!(fs::read_dir(root.path().join(prefix)).unwrap().count(), 0);
        let component = "\"".repeat(220);
        let destination = [component.as_str(); 8].join("/");
        let missing = (1..8)
            .map(|length| path(&vec![component.as_str(); length].join("/")))
            .collect();
        let result = files
            .prepare_directory_publication(
                &staging,
                &cwd,
                &path(&destination),
                TransferPrecondition::MustNotExist {},
                missing,
                &token,
            )
            .await;
        assert!(matches!(result, Err(BinaryError::Integrity)));
        assert!(!root.path().join(component).exists());
    }

    #[tokio::test]
    async fn mount_crossing_is_refused_before_creation() {
        let private = tempfile::tempdir().unwrap();
        fs::set_permissions(private.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let staging = DirectoryPublicationStaging::open(private.path()).unwrap();
        let files = FileToolGroup::new("/", true, None).await.unwrap();
        let cwd = files.workspace_root().await.unwrap().handle;
        assert!(matches!(
            files
                .prepare_directory_publication(
                    &staging,
                    &cwd,
                    &path("proc/workcell-directory-test"),
                    TransferPrecondition::MustNotExist {},
                    vec![],
                    &CancellationToken::new()
                )
                .await,
            Err(BinaryError::Inaccessible)
        ));
    }

    #[tokio::test]
    async fn replaced_published_directories_are_never_adopted_or_used_for_descendants() {
        for nested in [false, true] {
            let (root, _private, files, cwd, staging) = fixture(true).await;
            let token = CancellationToken::new();
            let destination = if nested { "parent/empty" } else { "parent" };
            let ancestors = if nested { vec![path("parent")] } else { vec![] };
            let mut prepared = files
                .prepare_directory_publication(
                    &staging,
                    &cwd,
                    &path(destination),
                    TransferPrecondition::MustNotExist {},
                    ancestors,
                    &token,
                )
                .await
                .unwrap();
            let parent = root.path().join("parent");
            let displaced = root.path().join("displaced");
            prepared.after_create = Some(Box::new(move || {
                fs::rename(&parent, displaced).unwrap();
                fs::create_dir(&parent).unwrap();
            }));
            assert!(matches!(
                files.execute_directory_publication(prepared, &token).await,
                Err(BinaryError::Indeterminate)
            ));
            assert_eq!(fs::read_dir(root.path().join("parent")).unwrap().count(), 0);
            assert_eq!(
                fs::read_dir(root.path().join("displaced")).unwrap().count(),
                0
            );
        }
    }

    #[tokio::test]
    async fn cancellation_and_release_before_effect_are_safe_but_partial_creation_is_indeterminate()
    {
        for phase in ["prepare", "release", "execute", "partial"] {
            let (root, _private, files, cwd, staging) = fixture(true).await;
            let token = CancellationToken::new();
            if phase == "prepare" {
                token.cancel();
            }
            let prepared = files
                .prepare_directory_publication(
                    &staging,
                    &cwd,
                    &path("parent/empty"),
                    TransferPrecondition::MustNotExist {},
                    vec![path("parent")],
                    &token,
                )
                .await;
            if phase == "prepare" {
                assert!(matches!(prepared, Err(BinaryError::Cancelled)));
            } else {
                let mut prepared = prepared.unwrap();
                if phase == "release" {
                    drop(prepared);
                } else {
                    if phase == "execute" {
                        token.cancel();
                    } else {
                        let token = token.clone();
                        prepared.after_create = Some(Box::new(move || token.cancel()));
                    }
                    let result = files.execute_directory_publication(prepared, &token).await;
                    if phase == "partial" {
                        assert!(matches!(result, Err(BinaryError::Indeterminate)));
                    } else {
                        assert!(matches!(result, Err(BinaryError::Cancelled)));
                    }
                }
            }
            assert_eq!(root.path().join("parent").exists(), phase == "partial");
            assert!(!root.path().join("parent/empty").exists());
        }
    }
}
