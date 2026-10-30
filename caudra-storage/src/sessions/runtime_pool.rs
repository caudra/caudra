//! Runtime connections kept open between writes. A fresh connection costs its
//! setup and, on its first commit, a directory fsync: SQLite syncs the
//! directory once for every connection that opens the WAL.

use std::mem;
use std::ops::Deref;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use tracing::warn;

use super::{RUNTIME_CONNECTION_SETUP, RuntimeRetry, SessionDatabase, SessionError};
use crate::StateDir;

/// How long an idle connection stays open, as the deferred writer's does.
const IDLE_CLOSE: Duration = Duration::from_secs(60);
/// Idle connections kept per state dir. More concurrent writers open their own.
const MAX_IDLE_PER_DIR: usize = 4;
const REAPER_THREAD: &str = "caudra-runtime-pool";

struct Idle {
    database: SessionDatabase,
    since: Instant,
}

static IDLE: Mutex<Vec<Idle>> = Mutex::new(Vec::new());
static REAPING: AtomicBool = AtomicBool::new(false);

/// A runtime connection that goes back to the pool when dropped, unless a
/// transaction is still open on it.
pub struct RuntimeDatabase(Option<SessionDatabase>);

impl SessionDatabase {
    /// [`SessionDatabase::open_runtime`], reusing a connection an earlier
    /// runtime operation on `state_dir` finished with.
    pub fn runtime(
        state_dir: &StateDir,
        retry: &RuntimeRetry<'_>,
    ) -> Result<RuntimeDatabase, SessionError> {
        retry.check(RUNTIME_CONNECTION_SETUP)?;
        let database = match take(state_dir) {
            Some(database) => database,
            None => Self::open_runtime(state_dir, retry)?,
        };
        Ok(RuntimeDatabase(Some(database)))
    }
}

impl Deref for RuntimeDatabase {
    type Target = SessionDatabase;

    fn deref(&self) -> &SessionDatabase {
        self.0.as_ref().expect("taken only on drop")
    }
}

impl Drop for RuntimeDatabase {
    fn drop(&mut self) {
        if let Some(database) = self.0.take()
            && database.connection.is_autocommit()
        {
            release(database);
        }
    }
}

fn idle() -> std::sync::MutexGuard<'static, Vec<Idle>> {
    IDLE.lock().unwrap_or_else(|error| error.into_inner())
}

/// Removes the connections idle past [`IDLE_CLOSE`], for the caller to close
/// outside the lock.
fn expired(pool: &mut Vec<Idle>) -> Vec<Idle> {
    let (expired, kept) = mem::take(pool)
        .into_iter()
        .partition(|idle| idle.since.elapsed() >= IDLE_CLOSE);
    *pool = kept;
    expired
}

fn take(state_dir: &StateDir) -> Option<SessionDatabase> {
    let (closing, reused) = {
        let mut pool = idle();
        let closing = expired(&mut pool);
        let reused = pool
            .iter()
            .rposition(|idle| idle.database.state_dir == *state_dir)
            .map(|index| pool.swap_remove(index).database);
        (closing, reused)
    };
    drop(closing);
    reused
}

fn release(database: SessionDatabase) {
    let refused = {
        let mut pool = idle();
        let held = pool
            .iter()
            .filter(|idle| idle.database.state_dir == database.state_dir)
            .count();
        if held >= MAX_IDLE_PER_DIR {
            Some(database)
        } else {
            pool.push(Idle {
                database,
                since: Instant::now(),
            });
            None
        }
    };
    drop(refused);
    if !REAPING.swap(true, Ordering::AcqRel)
        && let Err(error) = thread::Builder::new()
            .name(REAPER_THREAD.into())
            .spawn(reap)
    {
        REAPING.store(false, Ordering::Release);
        warn!(%error, "idle runtime connections close on their next use instead");
    }
}

/// Closes idle connections as they expire, and exits once none are left. A
/// release that lands after the pool emptied either finds `REAPING` cleared
/// and starts a reaper, or is picked up here.
fn reap() {
    loop {
        thread::sleep(IDLE_CLOSE);
        let (closing, empty) = {
            let mut pool = idle();
            let closing = expired(&mut pool);
            (closing, pool.is_empty())
        };
        drop(closing);
        if empty {
            REAPING.store(false, Ordering::Release);
            if idle().is_empty() || REAPING.swap(true, Ordering::AcqRel) {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn pooled(state_dir: &StateDir) -> usize {
        idle()
            .iter()
            .filter(|idle| idle.database.state_dir == *state_dir)
            .count()
    }

    #[test]
    fn a_finished_operation_leaves_its_connection_for_the_next() {
        let temp = TempDir::new().unwrap();
        let dir = StateDir::from_path(temp.path().to_path_buf());
        let retry = RuntimeRetry::new(None, &|| false);
        drop(SessionDatabase::runtime(&dir, &retry).unwrap());
        assert_eq!(pooled(&dir), 1);

        let reused = SessionDatabase::runtime(&dir, &retry).unwrap();
        assert_eq!(pooled(&dir), 0);
        drop(reused);
        assert_eq!(pooled(&dir), 1);
    }

    #[test]
    fn a_connection_inside_a_transaction_is_closed_not_pooled() {
        let temp = TempDir::new().unwrap();
        let dir = StateDir::from_path(temp.path().to_path_buf());
        let retry = RuntimeRetry::new(None, &|| false);
        let database = SessionDatabase::runtime(&dir, &retry).unwrap();
        database
            .connection
            .execute_batch("BEGIN IMMEDIATE")
            .unwrap();
        drop(database);
        assert_eq!(pooled(&dir), 0);

        let writer = SessionDatabase::open_state(&dir).unwrap();
        writer
            .connection
            .execute_batch("BEGIN IMMEDIATE; COMMIT")
            .unwrap();
    }

    #[test]
    fn concurrent_operations_on_one_dir_keep_at_most_the_idle_cap() {
        let temp = TempDir::new().unwrap();
        let dir = StateDir::from_path(temp.path().to_path_buf());
        let retry = RuntimeRetry::new(None, &|| false);
        let open: Vec<_> = (0..MAX_IDLE_PER_DIR + 2)
            .map(|_| SessionDatabase::runtime(&dir, &retry).unwrap())
            .collect();
        drop(open);
        assert_eq!(pooled(&dir), MAX_IDLE_PER_DIR);
    }

    #[test]
    fn connections_of_other_dirs_are_never_reused() {
        let first = TempDir::new().unwrap();
        let second = TempDir::new().unwrap();
        let first_dir = StateDir::from_path(first.path().to_path_buf());
        let second_dir = StateDir::from_path(second.path().to_path_buf());
        let retry = RuntimeRetry::new(None, &|| false);
        drop(SessionDatabase::runtime(&first_dir, &retry).unwrap());

        let other = SessionDatabase::runtime(&second_dir, &retry).unwrap();
        assert_eq!(other.state_dir, second_dir);
        assert_eq!(pooled(&first_dir), 1);
    }
}
