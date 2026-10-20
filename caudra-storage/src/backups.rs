//! Copies Caudra takes before it rewrites a file in place: the session
//! database before a schema migration or a permission repair, and a config
//! file before a format upgrade. Everything lands under `backups/` in the
//! state directory, which is private and is not the config directory users
//! sync between machines. Nothing here prunes.

use std::fs::{self, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use jiff::Timestamp;

use crate::id::CaudraId;
use crate::projects::project_id;
use crate::sessions::SESSIONS_DB_FILE;
use crate::{StateClass, StateDir, StorageError, sync_parent_dir_durable};

pub const BACKUPS_DIR: &str = "backups";
const CONFIG_DIR: &str = "config";
const GLOBAL_DIR: &str = "global";
const PROJECTS_DIR: &str = "projects";
const BACKUP_EXTENSION: &str = "bak";
const STAMP_FORMAT: &str = "%Y%m%dT%H%M%SZ";
#[cfg(unix)]
const BACKUP_FILE_MODE: u32 = 0o600;

/// Whose config file a backup copies.
#[derive(Debug, Clone, Copy)]
pub enum BackupScope<'a> {
    /// A file in the global config directory.
    Global,
    /// A file under the `.caudra/` directory of the project at this root.
    Project(&'a Path),
}

/// The backups directory of `state_dir`, created private if it is missing.
/// During an ephemeral run this is the volatile root, beside the database.
pub fn backups_dir(state_dir: &StateDir) -> Result<PathBuf, StorageError> {
    state_dir.ensure_subdir(BACKUPS_DIR)
}

/// Where the database is copied before migrating it from schema `from`.
pub(crate) fn database_migration_backup(
    state_dir: &StateDir,
    from: i64,
) -> Result<PathBuf, StorageError> {
    Ok(backups_dir(state_dir)?.join(format!("{SESSIONS_DB_FILE}.v{from}.{BACKUP_EXTENSION}")))
}

/// Where the database is copied before a permission review repair.
pub(crate) fn permission_review_backup(state_dir: &StateDir) -> Result<PathBuf, StorageError> {
    Ok(backups_dir(state_dir)?.join(format!(
        "{SESSIONS_DB_FILE}.permission-review-{}.{BACKUP_EXTENSION}",
        CaudraId::generate()
    )))
}

/// Writes `contents`, the bytes of config `file` at format version `from`, to
/// `backups/config/global/` or `backups/config/projects/{project_id}/`, named
/// `{file}.v{from}.{at as YYYYMMDDTHHMMSSZ}.bak`, and returns its path.
///
/// Config backups go under the persistent root even during an ephemeral run,
/// because the files they copy outlive the run.
///
/// The backup is created, never replaced: a second backup of the same file and
/// version within the same second fails with `AlreadyExists`. The first one
/// already holds the bytes from before the upgrade, so refusing keeps it intact,
/// and the caller can stop before writing anything rather than lose the copy.
pub fn back_up_config(
    state_dir: &StateDir,
    scope: BackupScope<'_>,
    file: &str,
    from: u32,
    contents: &[u8],
    at: Timestamp,
) -> Result<PathBuf, StorageError> {
    let config_dir = Path::new(BACKUPS_DIR).join(CONFIG_DIR);
    let subdir = match scope {
        BackupScope::Global => config_dir.join(GLOBAL_DIR),
        BackupScope::Project(root) => config_dir.join(PROJECTS_DIR).join(project_id(root)),
    };
    let dir = state_dir
        .for_class(StateClass::Persistent)
        .ensure_subdir(subdir)?;
    let stamp = at.strftime(STAMP_FORMAT);
    let path = dir.join(format!("{file}.v{from}.{stamp}.{BACKUP_EXTENSION}"));
    write_new_private(&path, contents)?;
    Ok(path)
}

fn write_new_private(path: &Path, contents: &[u8]) -> Result<(), StorageError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options
        .mode(BACKUP_FILE_MODE)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    let mut file = options.open(path)?;
    if let Err(error) = file.write_all(contents).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(error.into());
    }
    sync_parent_dir_durable(path)
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    const CONFIG_FILE: &str = "caudra.toml";
    const CONFIG_CONTENTS: &[u8] = b"\xEF\xBB\xBF# keep me\r\n[ui]\nmode = \"dark\"\n";
    const FROM_VERSION: u32 = 1;
    const BACKUP_AT: Timestamp = Timestamp::constant(1_792_404_000, 0);
    const BACKUP_NAME: &str = "caudra.toml.v1.20261019T100000Z.bak";
    const PROJECT_ROOT: &str = "/work/example";

    fn state_dir() -> (TempDir, StateDir) {
        let temp = TempDir::new().unwrap();
        let state_dir = StateDir::from_path(temp.path().to_path_buf());
        (temp, state_dir)
    }

    fn expected_dir(state_dir: &StateDir, scope: BackupScope<'_>) -> PathBuf {
        let config = state_dir.path().join(BACKUPS_DIR).join(CONFIG_DIR);
        match scope {
            BackupScope::Global => config.join(GLOBAL_DIR),
            BackupScope::Project(root) => config.join(PROJECTS_DIR).join(project_id(root)),
        }
    }

    #[test_case(BackupScope::Global; "global")]
    #[test_case(BackupScope::Project(Path::new(PROJECT_ROOT)); "project")]
    fn config_backup_is_a_private_byte_identical_copy(scope: BackupScope<'_>) {
        let (_temp, state_dir) = state_dir();

        let path = back_up_config(
            &state_dir,
            scope,
            CONFIG_FILE,
            FROM_VERSION,
            CONFIG_CONTENTS,
            BACKUP_AT,
        )
        .unwrap();

        assert_eq!(path, expected_dir(&state_dir, scope).join(BACKUP_NAME));
        assert_eq!(fs::read(&path).unwrap(), CONFIG_CONTENTS);
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            BACKUP_FILE_MODE
        );
    }

    #[test]
    fn second_backup_in_the_same_second_keeps_the_first() {
        let (_temp, state_dir) = state_dir();
        let path = back_up_config(
            &state_dir,
            BackupScope::Global,
            CONFIG_FILE,
            FROM_VERSION,
            CONFIG_CONTENTS,
            BACKUP_AT,
        )
        .unwrap();

        let error = back_up_config(
            &state_dir,
            BackupScope::Global,
            CONFIG_FILE,
            FROM_VERSION,
            b"",
            BACKUP_AT,
        )
        .unwrap_err();

        assert!(
            matches!(&error, StorageError::Io(io) if io.kind() == std::io::ErrorKind::AlreadyExists),
            "{error}"
        );
        assert_eq!(fs::read(&path).unwrap(), CONFIG_CONTENTS);
    }

    #[test]
    fn config_backups_outlive_an_ephemeral_run() {
        let (_volatile_temp, volatile) = state_dir();
        let (_persistent_temp, persistent) = state_dir();
        let split = StateDir::split(
            volatile.path().to_path_buf(),
            persistent.path().to_path_buf(),
        );

        let path = back_up_config(
            &split,
            BackupScope::Global,
            CONFIG_FILE,
            FROM_VERSION,
            CONFIG_CONTENTS,
            BACKUP_AT,
        )
        .unwrap();

        assert!(path.starts_with(persistent.path()));
    }
}
