use std::collections::HashSet;
use std::fs::{self, File};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use crate::exclusive_state_lock;
use crate::id::CaudraId;
use crate::{StateDir, StorageError, shared_state_lock, try_exclusive_state_lock};

use super::{SESSIONS_DB_LOCK_FILE, SessionError};

const ACTIVE_LEASE_PREFIX: &str = "caudra.sqlite.active-";
const ACTIVE_LEASE_SUFFIX: &str = ".lock";
const ACTIVE_LEASE_CATALOG: &str = "caudra.sqlite.active.lock";
const OWNER_FILE_MODE: u32 = 0o600;

type LeaseKey = (PathBuf, CaudraId);

static PROCESS_LEASES: OnceLock<Mutex<HashSet<LeaseKey>>> = OnceLock::new();

pub struct SessionLease {
    id: CaudraId,
    process_key: LeaseKey,
    active_path: PathBuf,
    active_lock: Option<File>,
    _migration_lock: File,
}

impl SessionLease {
    pub fn acquire(state_dir: &StateDir, id: CaudraId) -> Result<Self, SessionError> {
        let migration_lock = shared_state_lock(
            &state_dir.path().join(SESSIONS_DB_LOCK_FILE),
            OWNER_FILE_MODE,
        )?;

        let state_path = fs::canonicalize(state_dir.path()).map_err(StorageError::from)?;
        let process_key = (state_path, id);
        let _catalog_lock = exclusive_state_lock(
            &state_dir.path().join(ACTIVE_LEASE_CATALOG),
            OWNER_FILE_MODE,
        )?;
        cleanup_inactive_leases(state_dir, &process_key.0)?;
        {
            let mut leases = process_leases();
            if !leases.insert(process_key.clone()) {
                return Err(SessionError::SessionInUse { id });
            }
        }

        let lock_path = state_dir
            .path()
            .join(format!("{ACTIVE_LEASE_PREFIX}{id}{ACTIVE_LEASE_SUFFIX}"));
        let active_lock = match try_exclusive_state_lock(&lock_path, OWNER_FILE_MODE) {
            Ok(Some(file)) => file,
            Ok(None) => {
                process_leases().remove(&process_key);
                return Err(SessionError::SessionInUse { id });
            }
            Err(error) => {
                process_leases().remove(&process_key);
                return Err(error.into());
            }
        };

        Ok(Self {
            id,
            process_key,
            active_path: lock_path,
            active_lock: Some(active_lock),
            _migration_lock: migration_lock,
        })
    }

    pub fn id(&self) -> CaudraId {
        self.id
    }

    pub fn validate(&self, state_dir: &StateDir, id: CaudraId) -> Result<(), SessionError> {
        if self.id != id {
            return Err(SessionError::IdMismatch {
                log_id: self.id,
                given_id: id,
            });
        }
        let requested = fs::canonicalize(state_dir.path()).map_err(StorageError::from)?;
        if requested == self.process_key.0 {
            return Ok(());
        }
        Err(StorageError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "session lease for {id} belongs to {}, not {}",
                self.process_key.0.display(),
                requested.display()
            ),
        ))
        .into())
    }
}

impl std::fmt::Debug for SessionLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionLease")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        if let Some(parent) = self.active_path.parent()
            && let Ok(_catalog_lock) =
                exclusive_state_lock(&parent.join(ACTIVE_LEASE_CATALOG), OWNER_FILE_MODE)
            && let Some(file) = self.active_lock.take()
        {
            let _ = file.unlock();
            drop(file);
            let _ = fs::remove_file(&self.active_path);
        }
        process_leases().remove(&self.process_key);
    }
}

fn process_leases() -> std::sync::MutexGuard<'static, HashSet<LeaseKey>> {
    PROCESS_LEASES
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

/// Session ids whose lease is held right now, by this process or any other.
///
/// A lease file that still accepts an exclusive lock was left by a process that
/// has since gone, so it names nothing that is open. Best effort throughout:
/// this only ever explains a failure that already happened, and must not
/// replace it with one of its own.
pub(crate) fn held_session_ids(state_dir: &StateDir) -> Vec<CaudraId> {
    let Ok(entries) = fs::read_dir(state_dir.path()) else {
        return Vec::new();
    };
    let mut ids = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(lease_session_id) else {
            continue;
        };
        if let Ok(None) = try_exclusive_state_lock(&entry.path(), OWNER_FILE_MODE) {
            ids.push(id);
        }
    }
    ids.sort_by_key(CaudraId::to_string);
    ids
}

fn lease_session_id(name: &str) -> Option<CaudraId> {
    name.strip_prefix(ACTIVE_LEASE_PREFIX)
        .and_then(|name| name.strip_suffix(ACTIVE_LEASE_SUFFIX))
        .and_then(|raw| raw.parse().ok())
}

fn cleanup_inactive_leases(
    state_dir: &StateDir,
    canonical_state_path: &std::path::Path,
) -> Result<(), SessionError> {
    for entry in fs::read_dir(state_dir.path()).map_err(StorageError::from)? {
        let entry = entry.map_err(StorageError::from)?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(raw_id) = name
            .strip_prefix(ACTIVE_LEASE_PREFIX)
            .and_then(|name| name.strip_suffix(ACTIVE_LEASE_SUFFIX))
        else {
            continue;
        };
        let Ok(id) = raw_id.parse::<CaudraId>() else {
            continue;
        };
        if process_leases().contains(&(canonical_state_path.to_path_buf(), id)) {
            continue;
        }
        let path = entry.path();
        if let Some(file) = try_exclusive_state_lock(&path, OWNER_FILE_MODE)? {
            drop(file);
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(StorageError::from(error).into()),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    use tempfile::TempDir;

    use super::*;

    const CHILD_ENV: &str = "CAUDRA_SESSION_LEASE_TEST_CHILD";
    const CHILD_PATH_ENV: &str = "CAUDRA_SESSION_LEASE_TEST_PATH";
    const CHILD_ID_ENV: &str = "CAUDRA_SESSION_LEASE_TEST_ID";
    const CHILD_READY: &str = "session-lease-ready";

    fn state_dir() -> (TempDir, StateDir) {
        let temp = TempDir::new().unwrap();
        let state_dir = StateDir::from_path(temp.path().to_path_buf());
        (temp, state_dir)
    }

    #[test]
    fn second_lease_for_same_session_is_rejected() {
        let (_temp, state_dir) = state_dir();
        let id = CaudraId::generate();
        let _lease = SessionLease::acquire(&state_dir, id).unwrap();

        assert!(matches!(
            SessionLease::acquire(&state_dir, id),
            Err(SessionError::SessionInUse { id: actual }) if actual == id
        ));
    }

    #[test]
    fn different_sessions_can_hold_leases() {
        let (_temp, state_dir) = state_dir();

        let first = SessionLease::acquire(&state_dir, CaudraId::generate()).unwrap();
        let second = SessionLease::acquire(&state_dir, CaudraId::generate()).unwrap();

        assert_ne!(first.id(), second.id());
    }

    #[test]
    fn lease_is_bound_to_its_state_directory() {
        let (_first_temp, first_state) = state_dir();
        let (_second_temp, second_state) = state_dir();
        let id = CaudraId::generate();
        let lease = SessionLease::acquire(&first_state, id).unwrap();

        assert!(lease.validate(&second_state, id).is_err());
    }

    #[test]
    fn acquisition_removes_stale_crash_files() {
        let (_temp, state_dir) = state_dir();
        let stale_id = CaudraId::generate();
        let stale_path = state_dir.path().join(format!(
            "{ACTIVE_LEASE_PREFIX}{stale_id}{ACTIVE_LEASE_SUFFIX}"
        ));
        fs::create_dir_all(state_dir.path()).unwrap();
        fs::write(&stale_path, []).unwrap();

        let _lease = SessionLease::acquire(&state_dir, CaudraId::generate()).unwrap();

        assert!(!stale_path.exists());
    }

    #[test]
    fn dropped_lease_can_be_reacquired() {
        let (_temp, state_dir) = state_dir();
        let id = CaudraId::generate();
        let lock_path = state_dir
            .path()
            .join(format!("{ACTIVE_LEASE_PREFIX}{id}{ACTIVE_LEASE_SUFFIX}"));
        drop(SessionLease::acquire(&state_dir, id).unwrap());

        assert!(!lock_path.exists());
        assert_eq!(SessionLease::acquire(&state_dir, id).unwrap().id(), id);
    }

    #[test]
    fn lease_holder_process() {
        if std::env::var_os(CHILD_ENV).is_none() {
            return;
        }
        let state_dir =
            StateDir::from_path(PathBuf::from(std::env::var_os(CHILD_PATH_ENV).unwrap()));
        let id = std::env::var(CHILD_ID_ENV).unwrap().parse().unwrap();
        let _lease = SessionLease::acquire(&state_dir, id).unwrap();
        println!("{CHILD_READY}");
        std::io::stdout().flush().unwrap();
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).unwrap();
    }

    #[test]
    fn killed_process_releases_lease() {
        let (_temp, state_dir) = state_dir();
        let id = CaudraId::generate();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "sessions::lease::tests::lease_holder_process",
                "--nocapture",
            ])
            .env(CHILD_ENV, "1")
            .env(CHILD_PATH_ENV, state_dir.path())
            .env(CHILD_ID_ENV, id.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            while stdout.read_line(&mut line).unwrap() != 0 {
                if line.contains(CHILD_READY) {
                    let _ = ready_tx.send(true);
                    return;
                }
                line.clear();
            }
            let _ = ready_tx.send(false);
        });
        if ready_rx.recv_timeout(std::time::Duration::from_secs(5)) != Ok(true) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("lease holder did not become ready");
        }

        assert!(matches!(
            SessionLease::acquire(&state_dir, id),
            Err(SessionError::SessionInUse { id: actual }) if actual == id
        ));

        child.kill().unwrap();
        child.wait().unwrap();

        assert_eq!(SessionLease::acquire(&state_dir, id).unwrap().id(), id);
    }
}
