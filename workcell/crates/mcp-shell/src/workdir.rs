//! Initial working-directory validation.
//!
//! Confined resolution blocks lexical traversal and symlink escapes at launch time; host-managed
//! resolution accepts paths outside its base cwd. Neither mode confines the command after launch:
//! this is validation, not a command sandbox.

use std::{
    io,
    mem::size_of,
    path::{Component, Path, PathBuf},
};

use crate::ShellPreparationError;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(windows)]
use std::{fs::OpenOptions, os::windows::fs::OpenOptionsExt};
#[cfg(windows)]
use winapi_util::file::information;
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES};

pub const STALE_WORKDIR_ERROR: &str =
    "Prepared shell workdir is stale because its path or directory identity changed";
const MISSING_WORKDIR_ERROR: &str = "Invalid arguments: workdir must be an existing directory";
const UNINSPECTABLE_WORKDIR_ERROR: &str = "Invalid arguments: workdir cannot be inspected";
const CONFINED_WORKDIR_ERROR: &str =
    "Invalid arguments: workdir must be a directory inside the configured root";

#[derive(Debug)]
pub(crate) struct WorkdirBinding {
    canonical: PathBuf,
    requested: PathBuf,
    root: PathBuf,
    relative: String,
    identity: WorkdirIdentity,
    confined: bool,
}

impl WorkdirBinding {
    pub(crate) fn canonical(&self) -> &Path {
        &self.canonical
    }

    pub(crate) fn relative(&self) -> &str {
        &self.relative
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.canonical.capacity())
            .saturating_add(self.requested.capacity())
            .saturating_add(self.root.capacity())
            .saturating_add(self.relative.capacity())
    }
}

#[derive(Debug, Eq, PartialEq)]
struct WorkdirIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(windows)]
    volume: u64,
    #[cfg(windows)]
    file_index: u64,
}

pub(crate) async fn canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    // Canonicalization may block on filesystem traversal, so keep it off Tokio worker threads.
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || dunce::canonicalize(path))
        .await
        .map_err(std::io::Error::other)?
}

pub(crate) async fn resolve(
    root: &Path,
    requested: &str,
) -> Result<WorkdirBinding, ShellPreparationError> {
    let requested = if requested.is_empty() { "." } else { requested };
    let path = Path::new(requested);
    let lexical = normalize(if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    });
    if !inside(&lexical, root) {
        return Err(ShellPreparationError::OutsideRoot(
            "Invalid arguments: workdir escapes the configured root".into(),
        ));
    }
    // Check again after canonicalization: the lexical check catches `..`, while this check catches
    // symlinks whose target leaves the configured root.
    let canonical = canonicalize(&lexical).await.map_err(missing_workdir)?;
    if !inside(&canonical, root) {
        return Err(ShellPreparationError::OutsideRoot(
            CONFINED_WORKDIR_ERROR.into(),
        ));
    }
    if !inspect_directory(&canonical).await? {
        return Err(ShellPreparationError::Invalid(
            CONFINED_WORKDIR_ERROR.into(),
        ));
    }
    let relative = canonical
        .strip_prefix(root)
        .map_err(|_| ShellPreparationError::Invalid("Invalid workdir".to_owned()))?;
    let relative = if relative.as_os_str().is_empty() {
        ".".to_owned()
    } else {
        relative.to_string_lossy().replace('\\', "/")
    };
    let identity = identity(&canonical)
        .await
        .map_err(ShellPreparationError::Invalid)?;
    Ok(WorkdirBinding {
        canonical,
        requested: lexical,
        root: root.to_owned(),
        relative,
        identity,
        confined: true,
    })
}

pub(crate) async fn resolve_unconfined(
    base_cwd: &Path,
    requested: &str,
) -> Result<WorkdirBinding, ShellPreparationError> {
    let requested = if requested.is_empty() { "." } else { requested };
    let path = Path::new(requested);
    let lexical = normalize(if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_cwd.join(path)
    });
    let canonical = canonicalize(&lexical).await.map_err(missing_workdir)?;
    if !inspect_directory(&canonical).await? {
        return Err(ShellPreparationError::Invalid(
            "Invalid arguments: workdir must be a directory".into(),
        ));
    }
    let relative = canonical.strip_prefix(base_cwd).map_or_else(
        |_| canonical.to_string_lossy().replace('\\', "/"),
        |relative| {
            if relative.as_os_str().is_empty() {
                ".".to_owned()
            } else {
                relative.to_string_lossy().replace('\\', "/")
            }
        },
    );
    let identity = identity(&canonical)
        .await
        .map_err(ShellPreparationError::Invalid)?;
    Ok(WorkdirBinding {
        canonical,
        requested: lexical,
        root: base_cwd.to_owned(),
        relative,
        identity,
        confined: false,
    })
}

fn missing_workdir(error: io::Error) -> ShellPreparationError {
    if error.kind() == io::ErrorKind::NotFound {
        ShellPreparationError::NotFound(MISSING_WORKDIR_ERROR.into())
    } else {
        ShellPreparationError::Invalid(MISSING_WORKDIR_ERROR.into())
    }
}

async fn inspect_directory(path: &Path) -> Result<bool, ShellPreparationError> {
    tokio::fs::metadata(path)
        .await
        .map(|metadata| metadata.is_dir())
        .map_err(|_| ShellPreparationError::Invalid(UNINSPECTABLE_WORKDIR_ERROR.into()))
}

pub(crate) async fn revalidate(binding: &WorkdirBinding) -> Result<(), String> {
    if binding.confined && !inside(&binding.requested, &binding.root) {
        return Err(STALE_WORKDIR_ERROR.to_owned());
    }
    let canonical = canonicalize(&binding.requested)
        .await
        .map_err(|_| STALE_WORKDIR_ERROR.to_owned())?;
    if canonical != binding.canonical
        || (binding.confined && !inside(&canonical, &binding.root))
        || identity(&canonical).await? != binding.identity
    {
        return Err(STALE_WORKDIR_ERROR.to_owned());
    }
    Ok(())
}

#[cfg(not(windows))]
async fn identity(path: &Path) -> Result<WorkdirIdentity, String> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|_| STALE_WORKDIR_ERROR.to_owned())?;
    if !metadata.is_dir() {
        return Err(STALE_WORKDIR_ERROR.to_owned());
    }
    #[cfg(unix)]
    {
        Ok(WorkdirIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = metadata;
        Ok(WorkdirIdentity {})
    }
}

#[cfg(windows)]
async fn identity(path: &Path) -> Result<WorkdirIdentity, String> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        let file = OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .map_err(|_| STALE_WORKDIR_ERROR.to_owned())?;
        if !file
            .metadata()
            .map_err(|_| STALE_WORKDIR_ERROR.to_owned())?
            .is_dir()
        {
            return Err(STALE_WORKDIR_ERROR.to_owned());
        }
        let info = information(&file).map_err(|_| STALE_WORKDIR_ERROR.to_owned())?;
        Ok(WorkdirIdentity {
            volume: info.volume_serial_number(),
            file_index: info.file_index(),
        })
    })
    .await
    .map_err(|_| STALE_WORKDIR_ERROR.to_owned())?
}

fn normalize(path: PathBuf) -> PathBuf {
    // This normalization is intentionally filesystem-independent; canonicalization below remains
    // authoritative for links and existence.
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            _ => out.push(part.as_os_str()),
        }
    }
    out
}
#[cfg(not(windows))]
fn inside(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}
#[cfg(windows)]
fn inside(path: &Path, root: &Path) -> bool {
    // Windows path prefixes are case-insensitive in the common filesystems supported here.
    let mut parts = path.components();
    root.components().all(|expected| {
        parts.next().is_some_and(|actual| {
            actual
                .as_os_str()
                .to_string_lossy()
                .eq_ignore_ascii_case(&expected.as_os_str().to_string_lossy())
        })
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{STALE_WORKDIR_ERROR, canonicalize, resolve, revalidate};

    #[tokio::test]
    async fn directory_identity_accepts_content_changes_but_rejects_replacement() {
        let root = tempdir().unwrap();
        let root = canonicalize(root.path()).await.unwrap();
        let path = root.join("selected");
        fs::create_dir(&path).unwrap();
        let binding = resolve(&root, "selected").await.unwrap();
        fs::write(path.join("child"), "content").unwrap();
        revalidate(&binding).await.unwrap();

        fs::rename(&path, root.join("original")).unwrap();
        fs::create_dir(&path).unwrap();

        assert_eq!(revalidate(&binding).await, Err(STALE_WORKDIR_ERROR.into()));
        fs::remove_dir(&path).unwrap();
        assert_eq!(revalidate(&binding).await, Err(STALE_WORKDIR_ERROR.into()));
        fs::write(&path, "not a directory").unwrap();
        assert_eq!(revalidate(&binding).await, Err(STALE_WORKDIR_ERROR.into()));
    }
}
