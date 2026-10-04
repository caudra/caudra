use std::fmt::Display;
use std::fs;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::Connection;
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
))]
use rustix::fs::{CWD, RenameFlags, renameat_with};

#[cfg(windows)]
use crate::durable_rename_noreplace;
use crate::sync_parent_dir_io;

const SIDECARS: [&str; 3] = ["-wal", "-shm", "-journal"];
const MAX_CUTOVER_BYTES: u64 = 16 * 1024 * 1024 * 1024;
pub(crate) const OFFLINE_REQUIRED: &str = "database filename cutover requires all old Caudra processes and SQLite readers to be stopped; close them and retry";
const CONFLICT: &str = "both old and new database names or sidecars exist; preserve both and resolve the conflict offline";
const ORPHAN: &str =
    "database sidecars exist without their database; recover them offline before retrying";

pub(crate) fn exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    name.into()
}

pub(crate) fn pending(old: &Path, new: &Path) -> io::Result<bool> {
    let legacy = exists(old)?;
    let canonical = exists(new)?;
    for suffix in SIDECARS {
        if !legacy && exists(&sidecar(old, suffix))? || !canonical && exists(&sidecar(new, suffix))?
        {
            return Err(io::Error::other(ORPHAN));
        }
    }
    if legacy && canonical {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, CONFLICT));
    }
    Ok(legacy)
}

pub(crate) fn validate_source(path: &Path) -> io::Result<()> {
    let mut bytes = 0u64;
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let path = sidecar(path, suffix);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(io::Error::other("cutover input must be a regular file"));
        }
        #[cfg(unix)]
        if metadata.nlink() != 1 {
            return Err(io::Error::other("cutover input must not have hard links"));
        }
        bytes = bytes.saturating_add(metadata.len());
        if bytes > MAX_CUTOVER_BYTES {
            return Err(io::Error::other(
                "database exceeds automatic cutover byte limit; migrate offline",
            ));
        }
    }
    Ok(())
}

pub(crate) fn offline_error(error: impl Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        format!("{OFFLINE_REQUIRED}: {error}"),
    )
}

pub(crate) fn publish(connection: Connection, old: &Path, new: &Path) -> io::Result<()> {
    publish_before_rename(connection, old, new, || {})
}

fn publish_before_rename(
    connection: Connection,
    old: &Path,
    new: &Path,
    before_rename: impl FnOnce(),
) -> io::Result<()> {
    connection
        .busy_timeout(Duration::ZERO)
        .map_err(io::Error::other)?;
    connection
        .execute_batch(
            "PRAGMA trusted_schema = OFF; PRAGMA synchronous = FULL; PRAGMA locking_mode = NORMAL;",
        )
        .map_err(io::Error::other)?;
    let mode: String = connection
        .query_row("PRAGMA journal_mode = DELETE", [], |row| row.get(0))
        .map_err(offline_error)?;
    if !mode.eq_ignore_ascii_case("delete") {
        return Err(offline_error(mode));
    }
    connection
        .execute_batch("PRAGMA locking_mode = EXCLUSIVE; BEGIN EXCLUSIVE; COMMIT;")
        .map_err(offline_error)?;
    let locked_mode: String = connection
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .map_err(io::Error::other)?;
    if !locked_mode.eq_ignore_ascii_case("delete") {
        return Err(offline_error(locked_mode));
    }
    for suffix in SIDECARS {
        if exists(&sidecar(old, suffix))? {
            return Err(io::Error::other(ORPHAN));
        }
    }
    if !pending(old, new)? {
        return Err(io::Error::other("cutover source disappeared"));
    }
    before_rename();
    rename_noreplace(old, new)?;
    sync_parent_dir_io(new)?;
    connection
        .close()
        .map_err(|(_, error)| io::Error::other(error))
}

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
))]
fn rename_noreplace(old: &Path, new: &Path) -> io::Result<()> {
    Ok(renameat_with(CWD, old, CWD, new, RenameFlags::NOREPLACE)?)
}

#[cfg(windows)]
fn rename_noreplace(old: &Path, new: &Path) -> io::Result<()> {
    durable_rename_noreplace(old, new)
}

#[cfg(not(any(
    windows,
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
)))]
fn rename_noreplace(_old: &Path, _new: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic database filename cutover is unavailable on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs::{self, File};
    use std::io;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use std::process::Command;
    use std::time::Duration;

    use rusqlite::{Connection, ErrorCode, OpenFlags};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{
        CONFLICT, MAX_CUTOVER_BYTES, ORPHAN, pending, publish_before_rename, rename_noreplace,
        sidecar, validate_source,
    };

    const OLD: &str = "old.db";
    const NEW: &str = "new.db";
    const ORIGINAL: &[u8] = b"original";
    const DESTINATION: &[u8] = b"destination";
    const COMPETING_WRITER_PATH: &str = "CAUDRA_TEST_CUTOVER_WRITER_PATH";
    const COMPETING_WRITER_TEST: &str = "database_cutover::tests::competing_writer_child";
    const RETAINED_VALUE: &str = "retained";
    const COMPETING_VALUE: &str = "competing";
    const WRITER_MUST_BE_EXCLUDED: &str =
        "ordinary SQLite writers must remain excluded until publication";
    const DELETE_JOURNAL_MODE: &str = "delete";

    #[test]
    fn competing_writer_child() {
        let Some(path) = env::var_os(COMPETING_WRITER_PATH) else {
            return;
        };
        let connection =
            Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
        connection.busy_timeout(Duration::ZERO).unwrap();
        let error = connection
            .execute_batch(&format!(
                "PRAGMA journal_mode=WAL; INSERT INTO cutover_state VALUES ('{COMPETING_VALUE}');"
            ))
            .expect_err(WRITER_MUST_BE_EXCLUDED);
        assert_eq!(
            error.sqlite_error_code(),
            Some(ErrorCode::DatabaseBusy),
            "{WRITER_MUST_BE_EXCLUDED}"
        );
    }

    #[test_case(false; "committed_wal")]
    #[test_case(true; "interrupted_delete_journal")]
    fn sqlite_exclusion_spans_atomic_publication(interrupted: bool) {
        let temp = TempDir::new().unwrap();
        let old = temp.path().join(OLD);
        let new = temp.path().join(NEW);
        let connection = Connection::open(&old).unwrap();
        connection
            .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE cutover_state(value TEXT);")
            .unwrap();
        connection
            .execute("INSERT INTO cutover_state VALUES (?1)", [RETAINED_VALUE])
            .unwrap();
        if interrupted {
            connection
                .pragma_update(None, "journal_mode", "DELETE")
                .unwrap();
        }
        publish_before_rename(connection, &old, &new, || {
            let output = Command::new(env::current_exe().unwrap())
                .args(["--exact", COMPETING_WRITER_TEST, "--nocapture"])
                .env(COMPETING_WRITER_PATH, &old)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{WRITER_MUST_BE_EXCLUDED}: {output:?}"
            );
        })
        .unwrap();
        assert!(!old.exists());
        assert!(!sidecar(&old, "-wal").exists());
        let published =
            Connection::open_with_flags(&new, OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
        let values = published
            .prepare("SELECT value FROM cutover_state")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(values, [RETAINED_VALUE]);
        let mode: String = published
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(mode, DELETE_JOURNAL_MODE);
    }

    #[test_case(false; "racing_destination")]
    #[test_case(true; "publication_directory_missing")]
    fn failed_publication_preserves_source_and_releases_exclusion(missing_directory: bool) {
        let temp = TempDir::new().unwrap();
        let old = temp.path().join(OLD);
        let new = if missing_directory {
            temp.path().join(NEW).join(NEW)
        } else {
            temp.path().join(NEW)
        };
        let connection = Connection::open(&old).unwrap();
        connection
            .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE cutover_state(value TEXT);")
            .unwrap();
        connection
            .execute("INSERT INTO cutover_state VALUES (?1)", [RETAINED_VALUE])
            .unwrap();
        let result = publish_before_rename(connection, &old, &new, || {
            if !missing_directory {
                fs::write(&new, DESTINATION).unwrap();
            }
        });
        assert!(result.is_err());
        if !missing_directory {
            assert_eq!(fs::read(new).unwrap(), DESTINATION);
        }
        let source = Connection::open_with_flags(old, OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
        source.busy_timeout(Duration::ZERO).unwrap();
        let value: String = source
            .query_row("SELECT value FROM cutover_state", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, RETAINED_VALUE);
        source
            .execute("INSERT INTO cutover_state VALUES (?1)", [COMPETING_VALUE])
            .unwrap();
    }

    #[test_case(""; "database")]
    #[test_case("-wal"; "orphan_wal")]
    #[test_case("-shm"; "orphan_shm")]
    #[test_case("-journal"; "orphan_journal")]
    fn conflicting_destination_is_preserved(suffix: &str) {
        let temp = TempDir::new().unwrap();
        let old = temp.path().join(OLD);
        let new = temp.path().join(NEW);
        fs::write(&old, ORIGINAL).unwrap();
        fs::write(sidecar(&new, suffix), DESTINATION).unwrap();
        let error = pending(&old, &new).unwrap_err();
        assert_eq!(
            error.to_string(),
            if suffix.is_empty() { CONFLICT } else { ORPHAN }
        );
        assert_eq!(fs::read(old).unwrap(), ORIGINAL);
        assert_eq!(fs::read(sidecar(&new, suffix)).unwrap(), DESTINATION);
    }

    #[test_case(false; "rename_succeeds")]
    #[test_case(true; "racing_destination_not_overwritten")]
    fn publication_never_replaces_existing_data(conflict: bool) {
        let temp = TempDir::new().unwrap();
        let old = temp.path().join(OLD);
        let new = temp.path().join(NEW);
        fs::write(&old, ORIGINAL).unwrap();
        if conflict {
            fs::write(&new, DESTINATION).unwrap();
            assert_eq!(
                rename_noreplace(&old, &new).unwrap_err().kind(),
                io::ErrorKind::AlreadyExists
            );
            assert_eq!(fs::read(old).unwrap(), ORIGINAL);
            assert_eq!(fs::read(new).unwrap(), DESTINATION);
        } else {
            rename_noreplace(&old, &new).unwrap();
            assert!(!old.exists());
            assert_eq!(fs::read(new).unwrap(), ORIGINAL);
        }
    }

    #[test_case(false; "database_bytes")]
    #[test_case(true; "total_with_wal")]
    fn automatic_cutover_bounds_total_input_bytes(wal: bool) {
        let temp = TempDir::new().unwrap();
        let old = temp.path().join(OLD);
        File::create(&old)
            .unwrap()
            .set_len(MAX_CUTOVER_BYTES)
            .unwrap();
        let path = if wal {
            sidecar(&old, "-wal")
        } else {
            old.clone()
        };
        File::options()
            .create(true)
            .write(true)
            .truncate(false)
            .open(path)
            .unwrap()
            .set_len(if wal { 1 } else { MAX_CUTOVER_BYTES + 1 })
            .unwrap();
        assert!(validate_source(&old).is_err());
    }

    #[cfg(unix)]
    #[test_case(false; "symlink")]
    #[test_case(true; "hardlink")]
    fn cutover_refuses_linked_inputs(hardlink: bool) {
        let temp = TempDir::new().unwrap();
        let old = temp.path().join(OLD);
        let new = temp.path().join(NEW);
        fs::write(&new, ORIGINAL).unwrap();
        if hardlink {
            fs::hard_link(&new, &old).unwrap();
        } else {
            symlink(&new, &old).unwrap();
        }
        assert!(validate_source(&old).is_err());
        assert_eq!(fs::read(new).unwrap(), ORIGINAL);
    }
}
