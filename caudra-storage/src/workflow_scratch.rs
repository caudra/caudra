//! Scratch files a workflow run writes for itself, under
//! `<state>/workflow_scratch/<session>/<run>/`. Every directory on that path
//! is created owner-only and must be a real directory, every file is written
//! atomically at 0600, and a run's footprint is bounded by count and bytes.

use std::fmt;
use std::fs;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use crate::id::CaudraId;
use crate::{StateDir, StorageError, atomic_write_permissions, sync_parent_dir_io};

pub const WORKFLOW_SCRATCH_DIR: &str = "workflow_scratch";
const MAX_NAME_BYTES: usize = 128;
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_FILES_PER_RUN: u64 = 64;
const MAX_RUN_BYTES: u64 = 16 * 1024 * 1024;
const DIRECTORY_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;
const NAME_EMPTY: &str = "empty";
const NAME_TOO_LONG: &str = "longer than 128 bytes";
const NAME_DOT: &str = "a directory reference";
const NAME_SEPARATOR: &str = "contains a path separator";
const NAME_CONTROL: &str = "contains a control character";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaKind {
    FileBytes,
    FileCount,
    RunBytes,
}

impl fmt::Display for QuotaKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::FileBytes => "file size",
            Self::FileCount => "file count",
            Self::RunBytes => "run size",
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ScratchError {
    #[error("scratch file name {name:?} is {reason}")]
    InvalidName { name: String, reason: &'static str },
    #[error("scratch entry {} is not a regular file", path.display())]
    NotRegular { path: PathBuf },
    #[error("scratch {kind} quota exceeded: {actual} of {maximum}")]
    Quota {
        kind: QuotaKind,
        actual: u64,
        maximum: u64,
    },
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// One run's scratch directory, validated on open.
#[derive(Debug, Clone)]
pub struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    /// Opens `<state>/workflow_scratch/<session>/<run>/`, creating each
    /// missing level owner-only when `create` is set. A level that exists as
    /// anything but a real directory is refused.
    pub fn open(
        state_dir: &StateDir,
        session_id: CaudraId,
        run_id: &str,
        create: bool,
    ) -> io::Result<Self> {
        validate_name(run_id).map_err(invalid_input)?;
        let mut path = state_dir.path().to_path_buf();
        ensure_real_directory(&path, false)?;
        for component in [WORKFLOW_SCRATCH_DIR, &session_id.to_string(), run_id] {
            path.push(component);
            ensure_real_directory(&path, create)?;
        }
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Writes `content` to `name` atomically, replacing an earlier regular
    /// file of that name. Quotas count what is already on disk, so a rewrite
    /// is charged the difference.
    pub fn write(&self, name: &str, content: &[u8]) -> Result<PathBuf, ScratchError> {
        validate_name(name)?;
        let content_len = u64::try_from(content.len()).unwrap_or(u64::MAX);
        quota(QuotaKind::FileBytes, content_len, MAX_FILE_BYTES)?;
        let target = self.path.join(name);
        let existing = match fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.is_file() => Some(metadata.len()),
            Ok(_) => return Err(ScratchError::NotRegular { path: target }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let (files, bytes) = self.footprint()?;
        quota(
            QuotaKind::FileCount,
            files + u64::from(existing.is_none()),
            MAX_FILES_PER_RUN,
        )?;
        quota(
            QuotaKind::RunBytes,
            bytes.saturating_sub(existing.unwrap_or(0)) + content_len,
            MAX_RUN_BYTES,
        )?;
        atomic_write_permissions(&target, content, FILE_MODE).map_err(storage_io)?;
        Ok(target)
    }

    /// Bytes of every regular file directly in the run directory.
    pub fn bytes(&self) -> u64 {
        self.footprint().map_or(0, |(_, bytes)| bytes)
    }

    fn footprint(&self) -> io::Result<(u64, u64)> {
        let mut files = 0;
        let mut bytes = 0;
        for entry in fs::read_dir(&self.path)? {
            let metadata = fs::symlink_metadata(entry?.path())?;
            if metadata.is_file() {
                files += 1;
                bytes += metadata.len();
            }
        }
        Ok((files, bytes))
    }
}

pub fn remove_run(state_dir: &StateDir, session_id: CaudraId, run_id: &str) -> io::Result<()> {
    validate_name(run_id).map_err(invalid_input)?;
    remove_below(
        state_dir.path(),
        &[WORKFLOW_SCRATCH_DIR, &session_id.to_string(), run_id],
    )
}

pub fn remove_session(state_dir: &StateDir, session_id: CaudraId) -> io::Result<()> {
    remove_below(
        state_dir.path(),
        &[WORKFLOW_SCRATCH_DIR, &session_id.to_string()],
    )
}

/// Bytes of every regular file below a session's scratch tree, following no
/// symlinks. Unreadable entries count as zero: this feeds accounting only.
pub fn session_scratch_bytes(state_dir: &StateDir, session_id: CaudraId) -> u64 {
    tree_bytes(
        &state_dir
            .path()
            .join(WORKFLOW_SCRATCH_DIR)
            .join(session_id.to_string()),
    )
}

fn validate_name(name: &str) -> Result<(), ScratchError> {
    let reason = if name.is_empty() {
        NAME_EMPTY
    } else if name.len() > MAX_NAME_BYTES {
        NAME_TOO_LONG
    } else if name == "." || name == ".." {
        NAME_DOT
    } else if name.contains(['/', '\\']) {
        NAME_SEPARATOR
    } else if name.chars().any(char::is_control) {
        NAME_CONTROL
    } else {
        return Ok(());
    };
    Err(ScratchError::InvalidName {
        name: name.to_owned(),
        reason,
    })
}

fn quota(kind: QuotaKind, actual: u64, maximum: u64) -> Result<(), ScratchError> {
    if actual > maximum {
        return Err(ScratchError::Quota {
            kind,
            actual,
            maximum,
        });
    }
    Ok(())
}

fn ensure_real_directory(path: &Path, create: bool) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound && create => {
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            builder.mode(DIRECTORY_MODE);
            match builder.create(path) {
                Ok(()) => sync_parent_dir_io(path)?,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
            fs::symlink_metadata(path)?
        }
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing scratch access through {}", path.display()),
        ));
    }
    Ok(())
}

/// Removes the tree at `components` below `root`, refusing to walk through
/// anything that is not a real directory. A missing level is success.
fn remove_below(root: &Path, components: &[&str]) -> io::Result<()> {
    let mut path = root.to_path_buf();
    for component in components {
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("refusing scratch cleanup through {}", path.display()),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        }
        path.push(component);
    }
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(&path)?;
        }
        Ok(_) => fs::remove_file(&path)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    sync_parent_dir_io(&path)
}

fn tree_bytes(path: &Path) -> u64 {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return 0;
    };
    if metadata.is_file() {
        return metadata.len();
    }
    if !metadata.is_dir() {
        return 0;
    }
    fs::read_dir(path).map_or(0, |entries| {
        entries
            .flatten()
            .map(|entry| tree_bytes(&entry.path()))
            .sum()
    })
}

fn invalid_input(error: ScratchError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, error)
}

fn storage_io(error: StorageError) -> ScratchError {
    match error {
        StorageError::Io(error) => ScratchError::Io(error),
        other => ScratchError::Io(io::Error::other(other)),
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    const RUN_ID: &str = "run-1";
    const OTHER_RUN_ID: &str = "run-2";
    const NAME: &str = "notes.md";
    const CONTENT: &[u8] = b"first draft";
    const REPLACEMENT: &[u8] = b"second draft, longer";
    const REJECTS_LINK: &str = "a symlink must never be written through";
    const REJECTS_SIBLING_LINK: &str = "a symlinked level must never be traversed";
    const OVERWRITE_IS_ATOMIC: &str = "a rewrite must leave exactly the new content";
    const REWRITE_CHARGES_DIFFERENCE: &str = "rewriting a file must not count it twice";
    const CLEANUP_IS_COMPLETE: &str = "removal must leave nothing of the run behind";
    #[cfg(unix)]
    const MODE_MASK: u32 = 0o777;

    fn state() -> (TempDir, StateDir, CaudraId) {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("state");
        fs::create_dir(&root).unwrap();
        (temp, StateDir::from_path(root), CaudraId::generate())
    }

    #[test_case("", NAME_EMPTY; "empty")]
    #[test_case(".", NAME_DOT; "dot")]
    #[test_case("..", NAME_DOT; "dot_dot")]
    #[test_case("a/b", NAME_SEPARATOR; "slash")]
    #[test_case("a\\b", NAME_SEPARATOR; "backslash")]
    #[test_case("a\0b", NAME_CONTROL; "nul")]
    #[test_case("a\nb", NAME_CONTROL; "newline")]
    fn invalid_names_are_refused(name: &str, expected: &str) {
        let error = validate_name(name).unwrap_err();

        assert!(matches!(
            error,
            ScratchError::InvalidName { reason, .. } if reason == expected
        ));
    }

    #[test]
    fn an_overlong_name_is_refused() {
        let name = "n".repeat(MAX_NAME_BYTES + 1);

        let error = validate_name(&name).unwrap_err();

        assert!(matches!(
            error,
            ScratchError::InvalidName {
                reason: NAME_TOO_LONG,
                ..
            }
        ));
        assert!(validate_name(&"n".repeat(MAX_NAME_BYTES)).is_ok());
    }

    #[test]
    fn open_creates_owner_only_levels_and_write_is_owner_only() {
        let (_temp, state_dir, session_id) = state();

        let scratch = ScratchDir::open(&state_dir, session_id, RUN_ID, true).unwrap();
        let path = scratch.write(NAME, CONTENT).unwrap();

        assert_eq!(fs::read(&path).unwrap(), CONTENT);
        assert_eq!(scratch.bytes(), CONTENT.len() as u64);
        #[cfg(unix)]
        {
            let mut level = path.clone();
            assert_eq!(
                fs::metadata(&level).unwrap().permissions().mode() & MODE_MASK,
                FILE_MODE
            );
            while level.pop() && level != state_dir.path() {
                assert_eq!(
                    fs::metadata(&level).unwrap().permissions().mode() & MODE_MASK,
                    DIRECTORY_MODE
                );
            }
        }
        assert!(ScratchDir::open(&state_dir, session_id, OTHER_RUN_ID, false).is_err());
        assert!(ScratchDir::open(&state_dir, session_id, "../run", true).is_err());
    }

    #[test]
    fn overwrite_is_atomic_and_charged_by_difference() {
        let (_temp, state_dir, session_id) = state();
        let scratch = ScratchDir::open(&state_dir, session_id, RUN_ID, true).unwrap();
        scratch.write(NAME, CONTENT).unwrap();

        scratch.write(NAME, REPLACEMENT).unwrap();

        assert_eq!(
            fs::read(scratch.path().join(NAME)).unwrap(),
            REPLACEMENT,
            "{OVERWRITE_IS_ATOMIC}"
        );
        assert_eq!(fs::read_dir(scratch.path()).unwrap().count(), 1);
        assert_eq!(
            scratch.bytes(),
            REPLACEMENT.len() as u64,
            "{REWRITE_CHARGES_DIFFERENCE}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_entry_is_not_written_through() {
        let (temp, state_dir, session_id) = state();
        let scratch = ScratchDir::open(&state_dir, session_id, RUN_ID, true).unwrap();
        let outside = temp.path().join("outside");
        fs::write(&outside, CONTENT).unwrap();
        std::os::unix::fs::symlink(&outside, scratch.path().join(NAME)).unwrap();

        let error = scratch.write(NAME, REPLACEMENT).unwrap_err();

        assert!(
            matches!(error, ScratchError::NotRegular { .. }),
            "{REJECTS_LINK}"
        );
        assert_eq!(fs::read(&outside).unwrap(), CONTENT, "{REJECTS_LINK}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_level_is_not_traversed() {
        let (temp, state_dir, session_id) = state();
        ScratchDir::open(&state_dir, session_id, RUN_ID, true).unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let session_dir = state_dir
            .path()
            .join(WORKFLOW_SCRATCH_DIR)
            .join(session_id.to_string());
        fs::remove_dir_all(&session_dir).unwrap();
        std::os::unix::fs::symlink(&outside, &session_dir).unwrap();

        let open = ScratchDir::open(&state_dir, session_id, RUN_ID, true);
        let run_removal = remove_run(&state_dir, session_id, RUN_ID);
        assert!(open.is_err(), "{REJECTS_SIBLING_LINK}");
        assert!(run_removal.is_err(), "{REJECTS_SIBLING_LINK}");
        assert_eq!(session_scratch_bytes(&state_dir, session_id), 0);

        remove_session(&state_dir, session_id).unwrap();

        assert!(fs::symlink_metadata(&session_dir).is_err());
        assert!(outside.is_dir(), "{REJECTS_SIBLING_LINK}");
    }

    #[test]
    fn a_file_over_the_size_quota_is_refused() {
        let (_temp, state_dir, session_id) = state();
        let scratch = ScratchDir::open(&state_dir, session_id, RUN_ID, true).unwrap();
        let oversized = vec![0u8; usize::try_from(MAX_FILE_BYTES).unwrap() + 1];

        let error = scratch.write(NAME, &oversized).unwrap_err();

        assert!(matches!(
            error,
            ScratchError::Quota {
                kind: QuotaKind::FileBytes,
                ..
            }
        ));
        assert!(!scratch.path().join(NAME).exists());
    }

    #[test]
    fn a_run_over_the_file_count_quota_is_refused() {
        let (_temp, state_dir, session_id) = state();
        let scratch = ScratchDir::open(&state_dir, session_id, RUN_ID, true).unwrap();
        for index in 0..MAX_FILES_PER_RUN {
            scratch.write(&format!("file-{index}"), CONTENT).unwrap();
        }

        let error = scratch.write("one-too-many", CONTENT).unwrap_err();

        assert!(matches!(
            error,
            ScratchError::Quota {
                kind: QuotaKind::FileCount,
                ..
            }
        ));
        scratch.write("file-0", REPLACEMENT).unwrap();
    }

    #[test]
    fn a_run_over_the_byte_quota_is_refused() {
        let (_temp, state_dir, session_id) = state();
        let scratch = ScratchDir::open(&state_dir, session_id, RUN_ID, true).unwrap();
        let chunk = vec![0u8; usize::try_from(MAX_FILE_BYTES).unwrap()];
        for index in 0..MAX_RUN_BYTES / MAX_FILE_BYTES {
            scratch.write(&format!("chunk-{index}"), &chunk).unwrap();
        }

        let error = scratch.write("overflow", CONTENT).unwrap_err();

        assert!(matches!(
            error,
            ScratchError::Quota {
                kind: QuotaKind::RunBytes,
                ..
            }
        ));
        assert_eq!(scratch.bytes(), MAX_RUN_BYTES);
    }

    #[test]
    fn removal_and_accounting_cover_the_session_tree() {
        let (_temp, state_dir, session_id) = state();
        let first = ScratchDir::open(&state_dir, session_id, RUN_ID, true).unwrap();
        let second = ScratchDir::open(&state_dir, session_id, OTHER_RUN_ID, true).unwrap();
        first.write(NAME, CONTENT).unwrap();
        second.write(NAME, REPLACEMENT).unwrap();

        assert_eq!(
            session_scratch_bytes(&state_dir, session_id),
            (CONTENT.len() + REPLACEMENT.len()) as u64
        );
        remove_run(&state_dir, session_id, RUN_ID).unwrap();
        assert!(!first.path().exists(), "{CLEANUP_IS_COMPLETE}");
        assert_eq!(
            session_scratch_bytes(&state_dir, session_id),
            REPLACEMENT.len() as u64
        );
        remove_session(&state_dir, session_id).unwrap();
        assert!(!second.path().exists(), "{CLEANUP_IS_COMPLETE}");
        assert_eq!(session_scratch_bytes(&state_dir, session_id), 0);
        remove_session(&state_dir, session_id).unwrap();
        remove_session(
            &StateDir::from_path(state_dir.path().join("absent")),
            session_id,
        )
        .unwrap();
    }
}
