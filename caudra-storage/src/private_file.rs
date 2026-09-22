#[cfg(unix)]
use sha2::{Digest, Sha256};
use std::path::{Component, Path, PathBuf};
use std::{fs::File, io};
use thiserror::Error;

pub const MAX_PRIVATE_FILE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileRevision {
    Missing,
    Present([u8; 32]),
}

#[cfg(unix)]
impl FileRevision {
    fn of(data: Option<&[u8]>) -> Self {
        data.map_or(Self::Missing, |data| {
            Self::Present(Sha256::digest(data).into())
        })
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PrivateFileError {
    #[error("private file path must be absolute and contain no traversal or symlinks")]
    UnsafePath,
    #[error("private file must be a regular, singly linked file")]
    NotRegular,
    #[error("private file {0} must be owner-only and owned by the current user (mode {1:o}); try: chmod 600 {0}", .path.display(), .mode)]
    Permissions { path: PathBuf, mode: u32 },
    #[error("private file directory {0} must be owner-only (mode {1:o}); try: chmod 700 {0}", .path.display(), .mode)]
    DirectoryPermissions { path: PathBuf, mode: u32 },
    #[error("private file exceeds its size limit")]
    TooLarge,
    #[error("private file changed since it was loaded; reload or save as a new name")]
    Conflict,
    #[error("another writer holds the private file lock; retry after it completes")]
    Busy,
    #[error("private file I/O failed ({0:?})")]
    Io(io::ErrorKind),
    #[error(
        "private file was published but durability could not be confirmed; reload before retrying"
    )]
    DurabilityUnknown,
    #[error("secure private file storage is unavailable on this platform")]
    UnsupportedPlatform,
}

impl From<io::Error> for PrivateFileError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.kind())
    }
}

/// An explicit client-owned path, never resolved relative to a project or remote workspace.
pub struct PrivateFile {
    path: PathBuf,
    max_bytes: usize,
}

pub struct PrivateFileSnapshot {
    pub data: Option<Vec<u8>>,
    pub revision: FileRevision,
}

impl PrivateFile {
    pub fn new(path: PathBuf, max_bytes: usize) -> Result<Self, PrivateFileError> {
        if max_bytes > MAX_PRIVATE_FILE_BYTES {
            return Err(PrivateFileError::TooLarge);
        }
        if !path.is_absolute()
            || path.file_name().is_none()
            || path
                .components()
                .any(|part| matches!(part, Component::ParentDir))
        {
            return Err(PrivateFileError::UnsafePath);
        }
        Ok(Self { path, max_bytes })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A stable, owner-only advisory lease, separate from the atomically replaced data.
    /// Holding a shared lease prevents cooperating lifecycle controllers from mutating
    /// the resource; closing the descriptor releases it without performing any I/O.
    pub fn try_lease(&self, exclusive: bool) -> Result<File, PrivateFileError> {
        #[cfg(unix)]
        {
            unix::Directory::open(&self.path, true)?
                .ok_or(PrivateFileError::UnsafePath)?
                .lease(exclusive)
        }
        #[cfg(not(unix))]
        {
            let _ = exclusive;
            Err(PrivateFileError::UnsupportedPlatform)
        }
    }

    pub fn load(&self) -> Result<PrivateFileSnapshot, PrivateFileError> {
        #[cfg(unix)]
        {
            let data = match unix::Directory::open(&self.path, false)? {
                Some(directory) => directory.read(self.max_bytes)?,
                None => None,
            };
            let revision = FileRevision::of(data.as_deref());
            Ok(PrivateFileSnapshot { data, revision })
        }
        #[cfg(not(unix))]
        Err(PrivateFileError::UnsupportedPlatform)
    }

    /// Cooperative writers serialize on a stable lock, then compare the exact loaded bytes.
    /// Editors which ignore this lock must still be quiescent during publication.
    pub fn compare_exchange(
        &self,
        expected: &FileRevision,
        contents: Option<&[u8]>,
    ) -> Result<FileRevision, PrivateFileError> {
        if contents.is_some_and(|data| data.len() > self.max_bytes) {
            return Err(PrivateFileError::TooLarge);
        }
        #[cfg(unix)]
        {
            let directory =
                unix::Directory::open(&self.path, true)?.ok_or(PrivateFileError::UnsafePath)?;
            let _lock = directory.lock()?;
            let current = directory.read(self.max_bytes)?;
            if &FileRevision::of(current.as_deref()) != expected {
                return Err(PrivateFileError::Conflict);
            }
            if current.as_deref() != contents {
                directory.publish(contents)?;
            }
            Ok(FileRevision::of(contents))
        }
        #[cfg(not(unix))]
        {
            let _ = expected;
            Err(PrivateFileError::UnsupportedPlatform)
        }
    }
}

#[cfg(unix)]
mod unix {
    use super::PrivateFileError;
    use rustix::fs::{self, AtFlags, Mode, OFlags};
    use rustix::io::Errno;
    use rustix::process::geteuid;
    use std::ffi::{OsStr, OsString};
    use std::fs::{File, Metadata};
    use std::io::{Read, Write};
    use std::os::unix::fs::MetadataExt;
    use std::path::{Component, Path, PathBuf};

    const FILE_MODE: u32 = 0o600;
    const DIRECTORY_MODE: u32 = 0o700;
    const OTHER_ACCESS: u32 = 0o077;
    const OTHER_WRITE: u32 = 0o022;
    const STICKY: u32 = 0o1000;
    const MODE_MASK: u32 = 0o777;
    const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
        .union(OFlags::DIRECTORY)
        .union(OFlags::NOFOLLOW)
        .union(OFlags::CLOEXEC);
    const FILE_FLAGS: OFlags = OFlags::NOFOLLOW
        .union(OFlags::NONBLOCK)
        .union(OFlags::CLOEXEC);

    /// `path` is the validated parent directory, kept so a refusal can name the
    /// file it is about instead of leaving the caller to walk the tree by hand.
    pub(super) struct Directory {
        file: File,
        path: PathBuf,
        name: OsString,
    }

    fn syscall(error: Errno) -> PrivateFileError {
        match error {
            Errno::LOOP | Errno::NOTDIR => PrivateFileError::UnsafePath,
            _ => PrivateFileError::from(std::io::Error::from(error)),
        }
    }

    fn validate_file(path: &Path, metadata: &Metadata) -> Result<(), PrivateFileError> {
        if !metadata.is_file() || metadata.nlink() != 1 {
            return Err(PrivateFileError::NotRegular);
        }
        validate_file_owner(path, metadata.uid(), metadata.mode())
    }

    pub(super) fn validate_file_owner(
        path: &Path,
        owner: u32,
        mode: u32,
    ) -> Result<(), PrivateFileError> {
        if owner != geteuid().as_raw() || mode & OTHER_ACCESS != 0 {
            return Err(PrivateFileError::Permissions {
                path: path.to_path_buf(),
                mode: mode & MODE_MASK,
            });
        }
        Ok(())
    }

    fn validate_directory(file: &File, path: &Path, leaf: bool) -> Result<(), PrivateFileError> {
        let metadata = file.metadata()?;
        let owner = metadata.uid();
        let trusted_owner = owner == geteuid().as_raw() || (!leaf && owner == 0);
        let trusted_sticky = !leaf && owner == 0 && metadata.mode() & STICKY != 0;
        if !trusted_owner || (metadata.mode() & OTHER_WRITE != 0 && !trusted_sticky) {
            return Err(PrivateFileError::DirectoryPermissions {
                path: path.to_path_buf(),
                mode: metadata.mode() & MODE_MASK,
            });
        }
        Ok(())
    }

    impl Directory {
        pub(super) fn open(path: &Path, create: bool) -> Result<Option<Self>, PrivateFileError> {
            let parent = path.parent().ok_or(PrivateFileError::UnsafePath)?;
            let mut directory =
                File::from(fs::open("/", DIRECTORY_FLAGS, Mode::empty()).map_err(syscall)?);
            let mut walked = PathBuf::from(Component::RootDir.as_os_str());
            for component in parent.components() {
                let name = match component {
                    Component::RootDir => continue,
                    Component::Normal(name) => name,
                    _ => return Err(PrivateFileError::UnsafePath),
                };
                validate_directory(&directory, &walked, false)?;
                let child = match fs::openat(&directory, name, DIRECTORY_FLAGS, Mode::empty()) {
                    Ok(child) => child,
                    Err(Errno::NOENT) if create => {
                        match fs::mkdirat(&directory, name, Mode::from_raw_mode(DIRECTORY_MODE)) {
                            Ok(()) => directory.sync_all()?,
                            Err(Errno::EXIST) => {}
                            Err(error) => return Err(syscall(error)),
                        }
                        fs::openat(&directory, name, DIRECTORY_FLAGS, Mode::empty())
                            .map_err(syscall)?
                    }
                    Err(Errno::NOENT) => return Ok(None),
                    Err(error) => return Err(syscall(error)),
                };
                directory = File::from(child);
                walked.push(name);
            }
            validate_directory(&directory, &walked, true)?;
            Ok(Some(Self {
                file: directory,
                path: walked,
                name: path.file_name().ok_or(PrivateFileError::UnsafePath)?.into(),
            }))
        }

        fn open_file(&self, name: &OsStr, flags: OFlags) -> Result<Option<File>, PrivateFileError> {
            let file = match fs::openat(
                &self.file,
                name,
                flags | FILE_FLAGS,
                Mode::from_raw_mode(FILE_MODE),
            ) {
                Ok(file) => File::from(file),
                Err(Errno::NOENT) => return Ok(None),
                Err(error) => return Err(syscall(error)),
            };
            validate_file(&self.path.join(name), &file.metadata()?)?;
            Ok(Some(file))
        }

        pub(super) fn read(&self, max_bytes: usize) -> Result<Option<Vec<u8>>, PrivateFileError> {
            let Some(file) = self.open_file(&self.name, OFlags::RDONLY)? else {
                return Ok(None);
            };
            if file.metadata()?.len() > max_bytes as u64 {
                return Err(PrivateFileError::TooLarge);
            }
            let mut data = Vec::new();
            file.take(max_bytes as u64 + 1).read_to_end(&mut data)?;
            if data.len() > max_bytes {
                return Err(PrivateFileError::TooLarge);
            }
            Ok(Some(data))
        }

        pub(super) fn lock(&self) -> Result<File, PrivateFileError> {
            self.lease(true)
        }

        pub(super) fn lease(&self, exclusive: bool) -> Result<File, PrivateFileError> {
            let mut name = self.name.clone();
            name.push(".lock");
            let file = self
                .open_file(&name, OFlags::RDWR | OFlags::CREATE)?
                .ok_or(PrivateFileError::UnsafePath)?;
            (if exclusive {
                file.try_lock()
            } else {
                file.try_lock_shared()
            })
            .map_err(|error| match error {
                std::fs::TryLockError::WouldBlock => PrivateFileError::Busy,
                std::fs::TryLockError::Error(error) => PrivateFileError::from(error),
            })?;
            Ok(file)
        }

        pub(super) fn publish(&self, contents: Option<&[u8]>) -> Result<(), PrivateFileError> {
            if let Some(contents) = contents {
                let mut random = [0; 16];
                getrandom::fill(&mut random)
                    .map_err(|_| PrivateFileError::Io(std::io::ErrorKind::Other))?;
                let temporary = format!(".sandbox-{:032x}.tmp", u128::from_le_bytes(random));
                let mut file = self
                    .open_file(
                        OsStr::new(&temporary),
                        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
                    )?
                    .ok_or(PrivateFileError::UnsafePath)?;
                let result = (|| {
                    file.write_all(contents)?;
                    file.sync_all()?;
                    fs::renameat(&self.file, &temporary, &self.file, &self.name).map_err(syscall)
                })();
                if result.is_err() {
                    let _ = fs::unlinkat(&self.file, &temporary, AtFlags::empty());
                }
                result?;
            } else {
                fs::unlinkat(&self.file, &self.name, AtFlags::empty()).map_err(syscall)?;
            }
            self.file
                .sync_all()
                .map_err(|_| PrivateFileError::DurabilityUnknown)
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::{FileRevision, MAX_PRIVATE_FILE_BYTES, PrivateFile, PrivateFileError, unix};
    use rustix::fs::{CWD, Mode, mkfifoat};
    use rustix::process::geteuid;
    use std::fs::{self, File, Permissions};
    use std::io::{self, Read};
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::{Path, PathBuf};
    use std::sync::Barrier;
    use std::thread;
    use tempfile::{Builder, TempDir};
    use test_case::test_case;

    const FILE_NAME: &str = "sandboxes.toml";
    const LIMIT: usize = 128;
    const ORIGINAL: &[u8] = b"original data";
    const UPDATED: &[u8] = b"updated data";
    const OWNER_MODE: u32 = 0o600;
    const MODE_MASK: u32 = 0o777;
    const DIRECTORY_MODE: u32 = 0o700;
    const WORLD_WRITABLE_MODE: u32 = 0o777;

    fn tempdir() -> io::Result<TempDir> {
        Builder::new()
            .permissions(Permissions::from_mode(DIRECTORY_MODE))
            .tempdir()
    }

    #[test]
    fn missing_load_is_inert_and_publication_is_atomic_owner_only() {
        let temp = tempdir().unwrap();
        let parent = temp.path().join("new");
        let file = PrivateFile::new(parent.join(FILE_NAME), LIMIT).unwrap();
        let missing = file.load().unwrap();
        assert_eq!(missing.revision, FileRevision::Missing);
        assert!(!parent.exists());
        let first = file
            .compare_exchange(&missing.revision, Some(ORIGINAL))
            .unwrap();
        let mut old_reader = File::open(file.path()).unwrap();
        let second = file.compare_exchange(&first, Some(UPDATED)).unwrap();
        assert_ne!(first, second);
        let mut old_bytes = Vec::new();
        old_reader.read_to_end(&mut old_bytes).unwrap();
        assert_eq!(old_bytes, ORIGINAL);
        assert_eq!(file.load().unwrap().data.as_deref(), Some(UPDATED));
        assert_eq!(
            fs::metadata(file.path()).unwrap().permissions().mode() & MODE_MASK,
            OWNER_MODE
        );
        assert_eq!(fs::read_dir(&parent).unwrap().count(), 2);
        assert_eq!(
            file.compare_exchange(&first, Some(ORIGINAL)),
            Err(PrivateFileError::Conflict)
        );
        assert_eq!(
            file.compare_exchange(&second, None).unwrap(),
            FileRevision::Missing
        );
    }

    #[test]
    fn bounds_reads_and_writes() {
        let temp = tempdir().unwrap();
        assert!(matches!(
            PrivateFile::new(temp.path().join(FILE_NAME), MAX_PRIVATE_FILE_BYTES + 1),
            Err(PrivateFileError::TooLarge)
        ));
        let file = PrivateFile::new(temp.path().join(FILE_NAME), LIMIT).unwrap();
        let bytes = vec![b'x'; LIMIT + 1];
        assert_eq!(
            file.compare_exchange(&FileRevision::Missing, Some(&bytes)),
            Err(PrivateFileError::TooLarge)
        );
        assert!(!file.path().exists());
        fs::write(file.path(), bytes).unwrap();
        fs::set_permissions(file.path(), Permissions::from_mode(OWNER_MODE)).unwrap();
        assert_eq!(file.load().err(), Some(PrivateFileError::TooLarge));
    }

    #[test_case(0o644)]
    #[test_case(0o640)]
    #[test_case(0o666)]
    fn rejects_nonprivate_files_on_load_and_save(mode: u32) {
        let temp = tempdir().unwrap();
        let file = PrivateFile::new(temp.path().join(FILE_NAME), LIMIT).unwrap();
        let revision = file
            .compare_exchange(&FileRevision::Missing, Some(ORIGINAL))
            .unwrap();
        fs::set_permissions(file.path(), Permissions::from_mode(mode)).unwrap();
        let refused = || PrivateFileError::Permissions {
            path: file.path().to_path_buf(),
            mode,
        };
        assert_eq!(file.load().err(), Some(refused()));
        assert_eq!(
            file.compare_exchange(&revision, Some(UPDATED)),
            Err(refused())
        );
        assert_eq!(fs::read(file.path()).unwrap(), ORIGINAL);
    }

    #[test_case(false; "existing_target")]
    #[test_case(true; "dangling_target")]
    fn rejects_file_symlinks_without_following_or_replacing_them(dangling: bool) {
        let temp = tempdir().unwrap();
        let target = temp.path().join("target");
        if !dangling {
            fs::write(&target, ORIGINAL).unwrap();
        }
        let file = PrivateFile::new(temp.path().join(FILE_NAME), LIMIT).unwrap();
        symlink(&target, file.path()).unwrap();
        assert_eq!(file.load().err(), Some(PrivateFileError::UnsafePath));
        assert_eq!(
            file.compare_exchange(&FileRevision::Missing, Some(UPDATED)),
            Err(PrivateFileError::UnsafePath)
        );
        assert!(fs::symlink_metadata(file.path()).unwrap().is_symlink());
        if !dangling {
            assert_eq!(fs::read(target).unwrap(), ORIGINAL);
        }
    }

    #[test]
    fn rejects_ancestor_and_lock_symlinks() {
        let temp = tempdir().unwrap();
        let directory = temp.path().join("directory");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, Permissions::from_mode(DIRECTORY_MODE)).unwrap();
        let link = temp.path().join("link");
        symlink(&directory, &link).unwrap();
        let file = PrivateFile::new(link.join(FILE_NAME), LIMIT).unwrap();
        assert_eq!(file.load().err(), Some(PrivateFileError::UnsafePath));
        assert_eq!(
            file.compare_exchange(&FileRevision::Missing, Some(UPDATED)),
            Err(PrivateFileError::UnsafePath)
        );
        let file = PrivateFile::new(directory.join(FILE_NAME), LIMIT).unwrap();
        symlink(
            temp.path().join("absent"),
            directory.join(format!("{FILE_NAME}.lock")),
        )
        .unwrap();
        assert_eq!(
            file.compare_exchange(&FileRevision::Missing, Some(UPDATED)),
            Err(PrivateFileError::UnsafePath)
        );
        assert!(!file.path().exists());
    }

    #[test]
    fn rejects_unsafe_directories_hardlinks_and_traversal() {
        let temp = tempdir().unwrap();
        let directory = temp.path().join("directory");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, Permissions::from_mode(WORLD_WRITABLE_MODE)).unwrap();
        let file = PrivateFile::new(directory.join(FILE_NAME), LIMIT).unwrap();
        let refused = || PrivateFileError::DirectoryPermissions {
            path: directory.clone(),
            mode: WORLD_WRITABLE_MODE,
        };
        assert_eq!(file.load().err(), Some(refused()));
        assert_eq!(
            file.compare_exchange(&FileRevision::Missing, Some(UPDATED)),
            Err(refused())
        );
        fs::set_permissions(&directory, Permissions::from_mode(DIRECTORY_MODE)).unwrap();
        let revision = file
            .compare_exchange(&FileRevision::Missing, Some(ORIGINAL))
            .unwrap();
        fs::hard_link(file.path(), directory.join("alias")).unwrap();
        assert_eq!(file.load().err(), Some(PrivateFileError::NotRegular));
        assert_eq!(
            file.compare_exchange(&revision, None),
            Err(PrivateFileError::NotRegular)
        );
        assert!(matches!(
            PrivateFile::new(temp.path().join("missing/../file"), LIMIT),
            Err(PrivateFileError::UnsafePath)
        ));
    }

    #[test]
    fn refuses_nonregular_files_without_blocking() {
        let temp = tempdir().unwrap();
        let file = PrivateFile::new(temp.path().join(FILE_NAME), LIMIT).unwrap();
        mkfifoat(CWD, file.path(), Mode::RUSR | Mode::WUSR).unwrap();
        assert_eq!(file.load().err(), Some(PrivateFileError::NotRegular));
        assert_eq!(
            file.compare_exchange(&FileRevision::Missing, Some(UPDATED)),
            Err(PrivateFileError::NotRegular)
        );
    }

    #[test]
    fn rejects_wrong_owner_even_with_private_mode() {
        assert_eq!(
            unix::validate_file_owner(
                Path::new(FILE_NAME),
                geteuid().as_raw().wrapping_add(1),
                OWNER_MODE
            ),
            Err(PrivateFileError::Permissions {
                path: PathBuf::from(FILE_NAME),
                mode: OWNER_MODE,
            })
        );
    }

    #[test]
    fn competing_writers_cannot_both_commit_the_loaded_revision() {
        let temp = tempdir().unwrap();
        let file = PrivateFile::new(temp.path().join(FILE_NAME), LIMIT).unwrap();
        let revision = file
            .compare_exchange(&FileRevision::Missing, Some(ORIGINAL))
            .unwrap();
        let barrier = Barrier::new(2);
        let results = thread::scope(|scope| {
            let left = scope.spawn(|| {
                barrier.wait();
                file.compare_exchange(&revision, Some(UPDATED))
            });
            let right = scope.spawn(|| {
                barrier.wait();
                file.compare_exchange(&revision, None)
            });
            [left.join().unwrap(), right.join().unwrap()]
        });
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert!(results.iter().any(|result| matches!(
            result,
            Err(PrivateFileError::Busy | PrivateFileError::Conflict)
        )));
    }
}
