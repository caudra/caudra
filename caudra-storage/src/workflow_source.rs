//! Reads a workflow source file once, safely: the path is opened without
//! following a final symlink, the opened descriptor must be a regular file,
//! and the content must fit the caller's bound. Everything downstream (the
//! digest, the compile, the run) works from the returned buffer, so what was
//! hashed is exactly what executes.

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum SourceReadError {
    #[error("{} is a symbolic link", path.display())]
    Symlink { path: PathBuf },
    #[error("{} is not a regular file", path.display())]
    NotRegular { path: PathBuf },
    #[error("{} is {actual} bytes, the limit is {max}", path.display())]
    TooLarge {
        path: PathBuf,
        actual: u64,
        max: usize,
    },
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub fn read_bounded_regular_file(
    path: &Path,
    max_bytes: usize,
) -> Result<Vec<u8>, SourceReadError> {
    let file = open_regular(path)?;
    read_bounded(file, path, max_bytes)
}

/// Opens `path` without following a final symlink and proves, on the
/// descriptor itself, that it is a regular file.
fn open_regular(path: &Path) -> Result<File, SourceReadError> {
    #[cfg(not(unix))]
    if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(SourceReadError::Symlink {
            path: path.to_path_buf(),
        });
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    let file = match options.open(path) {
        Ok(file) => file,
        #[cfg(unix)]
        Err(error) if error.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error()) => {
            return Err(SourceReadError::Symlink {
                path: path.to_path_buf(),
            });
        }
        Err(error) => return Err(error.into()),
    };
    if !file.metadata()?.is_file() {
        return Err(SourceReadError::NotRegular {
            path: path.to_path_buf(),
        });
    }
    Ok(file)
}

/// Reads the opened file whole. The size is checked before reading and the
/// read itself is capped, so a file growing underneath still cannot exceed
/// the bound.
fn read_bounded(mut file: File, path: &Path, max_bytes: usize) -> Result<Vec<u8>, SourceReadError> {
    let limit = u64::try_from(max_bytes).unwrap_or(u64::MAX);
    let actual = file.metadata()?.len();
    if actual > limit {
        return Err(SourceReadError::TooLarge {
            path: path.to_path_buf(),
            actual,
            max: max_bytes,
        });
    }
    let mut bytes = Vec::with_capacity(usize::try_from(actual).unwrap_or(0));
    (&mut file)
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        return Err(SourceReadError::TooLarge {
            path: path.to_path_buf(),
            actual: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            max: max_bytes,
        });
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    const SOURCE: &[u8] = b"let meta = #{ name: \"review\" };";
    const REPLACEMENT: &[u8] = b"let meta = #{ name: \"other\" };";
    const MAX_BYTES: usize = 1024;
    const READS_OPENED_INODE: &str =
        "the bytes must come from the descriptor that was opened, not from the path";

    fn source_file(temp: &TempDir) -> PathBuf {
        let path = temp.path().join("workflow.rhai");
        fs::write(&path, SOURCE).unwrap();
        path
    }

    #[test]
    fn a_regular_file_is_read_whole() {
        let temp = TempDir::new().unwrap();
        let path = source_file(&temp);

        let bytes = read_bounded_regular_file(&path, MAX_BYTES).unwrap();

        assert_eq!(bytes, SOURCE);
        assert_eq!(
            read_bounded_regular_file(&path, SOURCE.len()).unwrap(),
            SOURCE
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_refused() {
        let temp = TempDir::new().unwrap();
        let target = source_file(&temp);
        let link = temp.path().join("link.rhai");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let error = read_bounded_regular_file(&link, MAX_BYTES).unwrap_err();

        assert!(matches!(error, SourceReadError::Symlink { path } if path == link));
    }

    #[test]
    fn a_directory_is_refused() {
        let temp = TempDir::new().unwrap();

        let error = read_bounded_regular_file(temp.path(), MAX_BYTES).unwrap_err();

        assert!(matches!(error, SourceReadError::NotRegular { .. }));
    }

    #[test]
    fn an_oversized_file_is_refused() {
        let temp = TempDir::new().unwrap();
        let path = source_file(&temp);

        let error = read_bounded_regular_file(&path, SOURCE.len() - 1).unwrap_err();

        assert!(matches!(
            error,
            SourceReadError::TooLarge { actual, max, .. }
                if actual == SOURCE.len() as u64 && max == SOURCE.len() - 1
        ));
    }

    #[test]
    fn a_missing_file_is_an_io_error() {
        let temp = TempDir::new().unwrap();

        let error = read_bounded_regular_file(&temp.path().join("absent"), MAX_BYTES).unwrap_err();

        assert!(
            matches!(error, SourceReadError::Io(error) if error.kind() == io::ErrorKind::NotFound)
        );
    }

    #[test]
    fn a_rename_after_open_does_not_change_what_is_read() {
        let temp = TempDir::new().unwrap();
        let path = source_file(&temp);
        let replacement = temp.path().join("replacement.rhai");
        fs::write(&replacement, REPLACEMENT).unwrap();

        let file = open_regular(&path).unwrap();
        fs::rename(&replacement, &path).unwrap();
        let bytes = read_bounded(file, &path, MAX_BYTES).unwrap();

        assert_eq!(bytes, SOURCE, "{READS_OPENED_INODE}");
        assert_eq!(fs::read(&path).unwrap(), REPLACEMENT);
    }
}
