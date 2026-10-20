//! Writes a config file only while it still holds what its writer read, so a
//! concurrent edit is never lost, and replaces it atomically.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf, absolute};
use std::sync::{Mutex, PoisonError};

use caudra_storage::private_file::FileRevision;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tracing::warn;

use crate::own_writes;

/// Serializes this process's commits, so two edits from one process cannot both pass the
/// revision check.
static COMMITS: Mutex<()> = Mutex::new(());

#[derive(Debug, Error)]
pub enum AtomicFileError {
    #[error("{} changed since it was loaded; reload it and make the change again", path.display())]
    Conflict { path: PathBuf },
    #[error("cannot update {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// The file's bytes, or `None` when there is none.
pub fn read(path: &Path) -> Result<Option<Vec<u8>>, AtomicFileError> {
    read_bytes(path).map_err(|source| AtomicFileError::Io {
        path: path.to_path_buf(),
        source,
    })
}

pub fn revision_of(bytes: Option<&[u8]>) -> FileRevision {
    bytes.map_or(FileRevision::Missing, |bytes| {
        FileRevision::Present(Sha256::digest(bytes).into())
    })
}

/// Replaces the file with `contents` while it still holds `expected`.
pub fn commit(
    path: &Path,
    expected: &FileRevision,
    contents: &[u8],
) -> Result<FileRevision, AtomicFileError> {
    commit_after(path, expected, contents, || Ok(())).map(|(revision, ())| revision)
}

/// [`commit`], running `prepare` once the file is known to still hold `expected` and to be
/// writable, and replacing it only when `prepare` succeeds.
///
/// Writes through a symlink to the file it names, so a linked dotfile stays linked. The new
/// file keeps the old one's permissions; a new file is owner-only. A read-only file is
/// refused rather than replaced.
pub fn commit_after<T>(
    path: &Path,
    expected: &FileRevision,
    contents: &[u8],
    prepare: impl FnOnce() -> io::Result<T>,
) -> Result<(FileRevision, T), AtomicFileError> {
    let _serialized = COMMITS.lock().unwrap_or_else(PoisonError::into_inner);
    let failed = |source: io::Error| AtomicFileError::Io {
        path: path.to_path_buf(),
        source,
    };
    let target = match fs::canonicalize(path) {
        Ok(target) => target,
        Err(error) if error.kind() == io::ErrorKind::NotFound => absolute(path).map_err(failed)?,
        Err(error) => return Err(failed(error)),
    };
    let current = read_bytes(&target).map_err(failed)?;
    if revision_of(current.as_deref()) != *expected {
        return Err(AtomicFileError::Conflict {
            path: path.to_path_buf(),
        });
    }
    let permissions = match current {
        Some(_) => Some(fs::metadata(&target).map_err(failed)?.permissions()),
        None => None,
    };
    if permissions
        .as_ref()
        .is_some_and(|permissions| permissions.readonly())
    {
        return Err(failed(io::ErrorKind::PermissionDenied.into()));
    }
    let prepared = prepare().map_err(failed)?;
    if current.as_deref().unwrap_or_default() == contents {
        return Ok((expected.clone(), prepared));
    }
    let directory = target
        .parent()
        .ok_or_else(|| failed(io::ErrorKind::InvalidInput.into()))?;
    fs::create_dir_all(directory).map_err(failed)?;
    let mut staged = tempfile::Builder::new()
        .prefix(&staging_prefix(&target))
        .tempfile_in(directory)
        .map_err(failed)?;
    staged.write_all(contents).map_err(failed)?;
    if let Some(permissions) = permissions {
        staged
            .as_file()
            .set_permissions(permissions)
            .map_err(failed)?;
    }
    staged.as_file().sync_all().map_err(failed)?;
    staged
        .persist(&target)
        .map_err(|error| failed(error.error))?;
    if let Err(error) = File::open(directory).and_then(|directory| directory.sync_all()) {
        warn!(path = %target.display(), %error, "config directory sync failed after an edit");
    }
    own_writes::record(path, contents);
    Ok((revision_of(Some(contents)), prepared))
}

/// `.{file}.`, so a staged file left by a crash names the file it was for and stays hidden.
fn staging_prefix(target: &Path) -> String {
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    format!(".{name}.")
}

fn read_bytes(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::{PermissionsExt, symlink};

    use tempfile::TempDir;

    use super::{AtomicFileError, commit, commit_after, read, revision_of};
    use crate::own_writes;

    const FILE: &str = "mcp.toml";
    const BEFORE: &str = "version = 1\n";
    const AFTER: &str = "version = 1\n[mcp.a]\nenabled = false\n";
    const ELSEWHERE: &str = "# edited elsewhere\n";
    #[cfg(unix)]
    const OWNER_ONLY: u32 = 0o600;
    #[cfg(unix)]
    const SHARED: u32 = 0o644;
    #[cfg(unix)]
    const READ_ONLY: u32 = 0o444;
    #[cfg(unix)]
    const MODE_BITS: u32 = 0o777;

    fn fixture(contents: Option<&str>) -> (TempDir, std::path::PathBuf) {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join(FILE);
        if let Some(contents) = contents {
            fs::write(&path, contents).unwrap();
        }
        (directory, path)
    }

    #[test]
    fn a_commit_replaces_the_file_it_read_and_records_the_write() {
        let (_directory, path) = fixture(Some(BEFORE));
        let revision = revision_of(read(&path).unwrap().as_deref());

        let committed = commit(&path, &revision, AFTER.as_bytes()).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), AFTER);
        assert_eq!(committed, revision_of(Some(AFTER.as_bytes())));
        assert!(own_writes::is_own(&path, AFTER.as_bytes()));
    }

    #[test]
    fn a_commit_over_a_changed_file_conflicts_and_skips_prepare() {
        let (_directory, path) = fixture(Some(BEFORE));
        let revision = revision_of(read(&path).unwrap().as_deref());
        fs::write(&path, ELSEWHERE).unwrap();

        let error = commit_after(
            &path,
            &revision,
            AFTER.as_bytes(),
            || -> std::io::Result<()> { panic!("prepare ran over a conflict") },
        )
        .unwrap_err();

        assert!(matches!(error, AtomicFileError::Conflict { .. }), "{error}");
        assert_eq!(fs::read_to_string(&path).unwrap(), ELSEWHERE);
    }

    #[test]
    fn a_failed_prepare_leaves_the_file_alone() {
        let (_directory, path) = fixture(Some(BEFORE));
        let revision = revision_of(read(&path).unwrap().as_deref());

        let error = commit_after(&path, &revision, AFTER.as_bytes(), || {
            Err::<(), _>(std::io::ErrorKind::Other.into())
        })
        .unwrap_err();

        assert!(matches!(error, AtomicFileError::Io { .. }), "{error}");
        assert_eq!(fs::read_to_string(&path).unwrap(), BEFORE);
    }

    #[cfg(unix)]
    #[test]
    fn a_new_file_is_owner_only_and_an_old_one_keeps_its_mode() {
        let (_directory, path) = fixture(None);
        commit(&path, &revision_of(None), BEFORE.as_bytes()).unwrap();
        let mode = |path| fs::metadata(path).unwrap().permissions().mode() & MODE_BITS;
        assert_eq!(mode(&path), OWNER_ONLY);

        fs::set_permissions(&path, fs::Permissions::from_mode(SHARED)).unwrap();
        commit(
            &path,
            &revision_of(Some(BEFORE.as_bytes())),
            AFTER.as_bytes(),
        )
        .unwrap();
        assert_eq!(mode(&path), SHARED);
    }

    #[cfg(unix)]
    #[test]
    fn a_read_only_file_is_refused() {
        let (_directory, path) = fixture(Some(BEFORE));
        fs::set_permissions(&path, fs::Permissions::from_mode(READ_ONLY)).unwrap();

        let error = commit(
            &path,
            &revision_of(Some(BEFORE.as_bytes())),
            AFTER.as_bytes(),
        )
        .unwrap_err();

        assert!(matches!(error, AtomicFileError::Io { .. }), "{error}");
        assert_eq!(fs::read_to_string(&path).unwrap(), BEFORE);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_written_through_and_stays_a_link() {
        let (directory, target) = fixture(Some(BEFORE));
        let link = directory.path().join("linked.toml");
        symlink(&target, &link).unwrap();

        commit(
            &link,
            &revision_of(Some(BEFORE.as_bytes())),
            AFTER.as_bytes(),
        )
        .unwrap();

        assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(fs::read_to_string(&target).unwrap(), AFTER);
    }
}
