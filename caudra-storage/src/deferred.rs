//! Rows a crash may lose: what Caudra learns about tools, models and shell
//! runs. Each is self-contained, so no other write refers to it, and each only
//! sharpens a statistic, so losing the last second of them costs little.
//!
//! They wait in memory and are written together, in one transaction, once the
//! oldest has waited [`PERSIST_WITHIN`], when a caller asks with [`flush`], and
//! as the process ends with [`flush_all`]. A held row leaves memory only once
//! the transaction carrying it commits, and a transaction is all or nothing,
//! so a crash keeps every row written before it and no part of a write it cut
//! off, and a retry never adds a row twice.

use std::collections::HashMap;
use std::mem;
use std::path::{Path, PathBuf};
use std::slice;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use flume::{Receiver, RecvTimeoutError, Sender};
use rusqlite::Connection;
use tracing::warn;

use crate::sessions::{RuntimeRetry, SessionDatabase, SessionError};
use crate::shell_runs::{self, Prediction, ShellRun};
use crate::token_calibration::{self, StoredCalibration};
use crate::tool_ledger::{self, ToolCall};
use crate::{StateClass, StateDir, StorageError};

/// The longest a held row waits for its write.
pub const PERSIST_WITHIN: Duration = Duration::from_secs(1);
/// How long a writer with nothing held keeps its thread and connection.
const IDLE_EXIT: Duration = Duration::from_secs(60);
/// A database that keeps refusing writes costs the oldest rows past this,
/// rather than memory without bound.
const MAX_HELD_ROWS: usize = 10_000;
const DEFERRED_WRITE: &str = "deferred rows write";
const THREAD_NAME: &str = "deferred-rows";
const WRITER_GONE: &str = "deferred row writer stopped";

#[derive(Debug, Clone)]
pub enum DeferredRow {
    ToolCall {
        call: ToolCall,
        at: u64,
    },
    Calibration(StoredCalibration),
    ShellRun {
        run: ShellRun,
        prediction: Option<Prediction>,
        at: u64,
    },
}

impl DeferredRow {
    /// A calibration is its model's whole learned state, so a newer one
    /// replaces the one still held rather than queueing behind it.
    fn replaces(&self, held: &Self) -> bool {
        matches!(
            (self, held),
            (Self::Calibration(newer), Self::Calibration(older))
                if newer.provider == older.provider && newer.model == older.model
        )
    }
}

enum Message {
    Hold(Box<DeferredRow>),
    Flush(Sender<Result<(), SessionError>>),
}

struct Handle {
    id: u64,
    sender: Sender<Message>,
}

/// Holds `row` until the next write of the state dir `dir` persists to.
pub fn hold(dir: &StateDir, row: DeferredRow) {
    let persistent = dir.for_class(StateClass::Persistent);
    let key = persistent.path().to_path_buf();
    let mut writers = writers();
    let mut message = Message::Hold(Box::new(row));
    if let Some(handle) = writers.get(&key) {
        match handle.sender.send(message) {
            Ok(()) => return,
            Err(flume::SendError(unsent)) => message = unsent,
        }
    }
    match spawn(persistent, key.clone()) {
        Ok(handle) => {
            let _ = handle.sender.send(message);
            writers.insert(key, handle);
        }
        Err(error) => {
            warn!(%error, path = %key.display(), "deferred row writer unavailable; row dropped")
        }
    }
}

/// Writes every row held for `dir` and reports how that went.
pub fn flush(dir: &StateDir) -> Result<(), SessionError> {
    let key = dir.for_class(StateClass::Persistent).path().to_path_buf();
    let done = {
        let writers = writers();
        let Some(handle) = writers.get(&key) else {
            return Ok(());
        };
        request_flush(handle)?
    };
    done.recv().map_err(|_| writer_gone())?
}

/// Writes what every state dir holds, as the process ends, waiting at most
/// `timeout` in all.
pub fn flush_all(timeout: Duration) {
    let deadline = Instant::now() + timeout;
    let requests: Vec<_> = writers()
        .iter()
        .map(|(path, handle)| (path.clone(), request_flush(handle)))
        .collect();
    for (path, request) in requests {
        let result = request.and_then(|done| match done.recv_deadline(deadline) {
            Ok(result) => result,
            Err(_) => Err(writer_gone()),
        });
        if let Err(error) = result {
            warn!(%error, path = %path.display(), "held rows were not written before exit");
        }
    }
}

fn request_flush(handle: &Handle) -> Result<Receiver<Result<(), SessionError>>, SessionError> {
    let (done, receiver) = flume::bounded(1);
    handle
        .sender
        .send(Message::Flush(done))
        .map_err(|_| writer_gone())?;
    Ok(receiver)
}

fn writers() -> MutexGuard<'static, HashMap<PathBuf, Handle>> {
    static WRITERS: OnceLock<Mutex<HashMap<PathBuf, Handle>>> = OnceLock::new();
    WRITERS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

fn spawn(dir: StateDir, key: PathBuf) -> Result<Handle, std::io::Error> {
    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (sender, receiver) = flume::unbounded();
    thread::Builder::new()
        .name(THREAD_NAME.into())
        .spawn(move || run(dir, &key, id, &receiver))?;
    Ok(Handle { id, sender })
}

fn run(dir: StateDir, key: &Path, id: u64, receiver: &Receiver<Message>) {
    let mut writer = Writer {
        dir,
        database: None,
        held: Held::default(),
    };
    loop {
        if writer.held.due.is_some_and(|due| due <= Instant::now()) {
            writer.write_logged();
        }
        let received = match writer.held.due {
            Some(due) => receiver.recv_deadline(due),
            None => receiver.recv_timeout(IDLE_EXIT),
        };
        match received {
            Ok(Message::Hold(row)) => writer.held.hold(*row, Instant::now()),
            Ok(Message::Flush(done)) => {
                let _ = done.send(writer.write());
            }
            Err(RecvTimeoutError::Timeout) => {
                if writer.held.due.is_none() && retire(key, id, receiver) {
                    return;
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                writer.write_logged();
                return;
            }
        }
    }
}

/// Lets an idle writer go, unless a row reached it meanwhile. Rows are sent
/// with the registry locked, so none can arrive between the check and the
/// removal, and the next one starts a new writer.
fn retire(key: &Path, id: u64, receiver: &Receiver<Message>) -> bool {
    let mut writers = writers();
    if !receiver.is_empty() {
        return false;
    }
    if writers.get(key).is_some_and(|handle| handle.id == id) {
        writers.remove(key);
    }
    true
}

struct Writer {
    dir: StateDir,
    database: Option<SessionDatabase>,
    held: Held,
}

impl Writer {
    fn write(&mut self) -> Result<(), SessionError> {
        if self.held.rows.is_empty() {
            self.held.due = None;
            return Ok(());
        }
        let retry = RuntimeRetry::new(None, &|| false);
        let database = match &self.database {
            Some(database) => database,
            None => match SessionDatabase::open_runtime(&self.dir, &retry) {
                Ok(database) => self.database.insert(database),
                Err(error) => {
                    self.held.due = Some(Instant::now() + PERSIST_WITHIN);
                    return Err(error);
                }
            },
        };
        let result = self.held.write(database, &retry);
        if result.is_err() {
            self.database = None;
        }
        result
    }

    fn write_logged(&mut self) {
        if let Err(error) = self.write() {
            warn!(%error, rows = self.held.rows.len(), "held rows kept for the next write");
        }
    }
}

#[derive(Default)]
struct Held {
    rows: Vec<DeferredRow>,
    /// When the oldest held row must be written.
    due: Option<Instant>,
}

impl Held {
    fn hold(&mut self, row: DeferredRow, now: Instant) {
        self.rows.retain(|held| !row.replaces(held));
        if self.rows.len() >= MAX_HELD_ROWS {
            self.rows.remove(0);
            warn!(
                limit = MAX_HELD_ROWS,
                "deferred rows dropped the oldest: the database keeps refusing them"
            );
        }
        self.rows.push(row);
        self.due.get_or_insert(now + PERSIST_WITHIN);
    }

    /// Writes every held row in one transaction and forgets them only once it
    /// commits. When the database refuses the batch for something other than
    /// contention, each row is tried alone, so one it will never take cannot
    /// hold back the rest.
    fn write(
        &mut self,
        database: &SessionDatabase,
        retry: &RuntimeRetry<'_>,
    ) -> Result<(), SessionError> {
        let result = match commit(database, &self.rows, retry) {
            Ok(()) => {
                self.rows.clear();
                Ok(())
            }
            Err(error) if contended(&error) => Err(error),
            Err(error) => {
                warn!(%error, rows = self.rows.len(), "held rows refused together; writing each alone");
                self.write_each(database, retry);
                if self.rows.is_empty() {
                    Ok(())
                } else {
                    Err(error)
                }
            }
        };
        self.due = (!self.rows.is_empty()).then(|| Instant::now() + PERSIST_WITHIN);
        result
    }

    /// A row refused on its own is dropped: it would be refused again.
    fn write_each(&mut self, database: &SessionDatabase, retry: &RuntimeRetry<'_>) {
        for row in mem::take(&mut self.rows) {
            match commit(database, slice::from_ref(&row), retry) {
                Ok(()) => {}
                Err(error) if contended(&error) => self.rows.push(row),
                Err(error) => warn!(%error, ?row, "held row dropped: the database refuses it"),
            }
        }
    }
}

fn commit(
    database: &SessionDatabase,
    rows: &[DeferredRow],
    retry: &RuntimeRetry<'_>,
) -> Result<(), SessionError> {
    database.runtime_transaction(retry, DEFERRED_WRITE, None, |transaction| {
        write_rows(transaction, rows)
    })
}

fn write_rows(connection: &Connection, rows: &[DeferredRow]) -> Result<(), SessionError> {
    for row in rows {
        match row {
            DeferredRow::ToolCall { call, at } => tool_ledger::merge_call(connection, call, *at)?,
            DeferredRow::Calibration(calibration) => {
                token_calibration::upsert(connection, calibration)?;
            }
            DeferredRow::ShellRun {
                run,
                prediction,
                at,
            } => shell_runs::insert(connection, *at, run, prediction.as_ref())?,
        }
    }
    if rows
        .iter()
        .any(|row| matches!(row, DeferredRow::ShellRun { .. }))
    {
        shell_runs::prune(connection)?;
    }
    Ok(())
}

fn contended(error: &SessionError) -> bool {
    matches!(
        error,
        SessionError::RuntimeContention { .. }
            | SessionError::RuntimeDeadline { .. }
            | SessionError::RuntimeCancelled { .. }
    )
}

fn writer_gone() -> SessionError {
    StorageError::Io(std::io::Error::other(WRITER_GONE)).into()
}

#[cfg(test)]
mod tests {
    use std::mem;
    use std::time::{Duration, Instant};

    use rusqlite::{Transaction, TransactionBehavior};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{DeferredRow, Held, MAX_HELD_ROWS, PERSIST_WITHIN, flush, hold, write_rows};
    use crate::StateDir;
    use crate::sessions::{RuntimeRetry, SessionDatabase};
    use crate::shell_runs::reading::{Call, read};
    use crate::shell_runs::{RunOutcome, ShellRun, ShellRuns};
    use crate::token_calibration::{StoredCalibration, TokenCalibrationStore};
    use crate::tool_ledger::{ToolCall, ToolLedger, ToolOutcome};

    const TOOL: &str = "shell";
    const SOURCE: &str = "native";
    const CWD: &str = "/work/project";
    const COMMAND: &str = "cargo test";
    const PROVIDER: &str = "anthropic";
    const MODEL: &str = "claude-opus-5-5";
    const WRITTEN_RATIO: f64 = 1.1;
    const HELD_RATIO: f64 = 1.3;
    const NEWEST_RATIO: f64 = 1.5;
    const AT: u64 = 1_800_000_000;
    const ELAPSED_MS: u64 = 1_200;
    const RUNS_READ: usize = 10;
    const CONTENDED_FOR: Duration = Duration::from_millis(50);
    const INTEGRITY_OK: &str = "ok";
    const DOUBLE_COUNTED: &str = "a retried call must count once";

    fn store() -> (TempDir, StateDir, SessionDatabase) {
        let temp = TempDir::new().unwrap();
        let dir = StateDir::from_path(temp.path().to_path_buf());
        let database = SessionDatabase::open_state(&dir).unwrap();
        (temp, dir, database)
    }

    fn tool_call() -> DeferredRow {
        DeferredRow::ToolCall {
            call: ToolCall {
                tool: TOOL.into(),
                source: SOURCE.into(),
                cwd: CWD.into(),
                outcome: ToolOutcome::Ok,
                duration_ms: ELAPSED_MS,
                tokens: 1,
            },
            at: AT,
        }
    }

    fn calibration(ratio: f64) -> DeferredRow {
        DeferredRow::Calibration(StoredCalibration {
            provider: PROVIDER.into(),
            model: MODEL.into(),
            ratio,
            weight: 1.0,
            samples: 1,
            updated_at: AT,
        })
    }

    fn shell_run(elapsed_ms: u64) -> DeferredRow {
        DeferredRow::ShellRun {
            run: ShellRun {
                reading: read(&Call {
                    command: COMMAND,
                    workdir: None,
                    cwd: Some(CWD),
                    timeout_secs: None,
                }),
                host: None,
                elapsed_ms,
                outcome: RunOutcome::Completed,
            },
            prediction: None,
            at: AT,
        }
    }

    /// A shell run whose time no column can hold, so its insert fails after
    /// the rows before it in the same transaction ran.
    fn refused_row() -> DeferredRow {
        shell_run(u64::MAX)
    }

    fn retry() -> RuntimeRetry<'static> {
        RuntimeRetry::new(None, &|| false)
    }

    fn calls(dir: &StateDir) -> u64 {
        ToolLedger::open(dir).unwrap().lifetime().unwrap().calls
    }

    fn ratios(dir: &StateDir) -> Vec<f64> {
        TokenCalibrationStore::open(dir)
            .unwrap()
            .load_all()
            .unwrap()
            .into_iter()
            .map(|calibration| calibration.ratio)
            .collect()
    }

    fn shell_runs(dir: &StateDir) -> usize {
        ShellRuns::open(dir)
            .unwrap()
            .newest(RUNS_READ)
            .unwrap()
            .len()
    }

    fn held(rows: impl IntoIterator<Item = DeferredRow>) -> Held {
        let mut held = Held::default();
        for row in rows {
            held.hold(row, Instant::now());
        }
        held
    }

    #[test_case(false; "rows_held_in_memory")]
    #[test_case(true; "a_write_cut_off_before_its_commit")]
    fn a_crash_mid_window_leaves_only_the_older_valid_state(cut_off: bool) {
        let (_temp, dir, database) = store();
        held([tool_call(), calibration(WRITTEN_RATIO)])
            .write(&database, &retry())
            .unwrap();
        let pending = held([tool_call(), calibration(HELD_RATIO), shell_run(ELAPSED_MS)]);
        if cut_off {
            let transaction =
                Transaction::new_unchecked(database.connection(), TransactionBehavior::Immediate)
                    .unwrap();
            write_rows(&transaction, &pending.rows).unwrap();
            mem::forget(transaction);
        }
        drop(pending);
        drop(database);

        let reopened = SessionDatabase::open_state(&dir).unwrap();
        let integrity: String = reopened
            .connection()
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(integrity, INTEGRITY_OK);
        assert_eq!(calls(&dir), 1);
        assert_eq!(ratios(&dir), [WRITTEN_RATIO]);
        assert_eq!(shell_runs(&dir), 0);
    }

    #[test]
    fn a_contended_write_keeps_its_rows_and_its_retry_counts_them_once() {
        let (_temp, dir, database) = store();
        let blocker = SessionDatabase::open_state(&dir).unwrap();
        blocker
            .connection()
            .execute_batch("BEGIN IMMEDIATE")
            .unwrap();
        let mut pending = held([tool_call(), tool_call()]);
        let impatient = RuntimeRetry::new(Some(Instant::now() + CONTENDED_FOR), &|| false);

        assert!(pending.write(&database, &impatient).is_err());
        assert_eq!(pending.rows.len(), 2);
        assert!(pending.due.is_some());

        blocker.connection().execute_batch("ROLLBACK").unwrap();
        pending.write(&database, &retry()).unwrap();
        assert!(pending.rows.is_empty());
        assert_eq!(calls(&dir), 2, "{DOUBLE_COUNTED}");
    }

    #[test]
    fn a_refused_row_rolls_back_its_batch_and_the_rest_count_once() {
        let (_temp, dir, database) = store();
        let mut pending = held([tool_call(), tool_call(), refused_row()]);

        pending.write(&database, &retry()).unwrap();

        assert!(pending.rows.is_empty());
        assert_eq!(pending.due, None);
        assert_eq!(calls(&dir), 2, "{DOUBLE_COUNTED}");
        assert_eq!(shell_runs(&dir), 0);
    }

    #[test]
    fn a_newer_calibration_replaces_the_held_one() {
        let (_temp, dir, database) = store();
        let mut pending = held([calibration(HELD_RATIO), calibration(NEWEST_RATIO)]);
        assert_eq!(pending.rows.len(), 1);
        pending.write(&database, &retry()).unwrap();
        assert_eq!(ratios(&dir), [NEWEST_RATIO]);
    }

    #[test]
    fn the_oldest_row_sets_the_deadline() {
        let first = Instant::now();
        let mut pending = Held::default();
        pending.hold(tool_call(), first);
        pending.hold(tool_call(), first + PERSIST_WITHIN);
        assert_eq!(pending.due, Some(first + PERSIST_WITHIN));
    }

    #[test]
    fn a_full_hold_drops_its_oldest_row() {
        let mut pending = Held::default();
        for _ in 0..MAX_HELD_ROWS {
            pending.hold(tool_call(), Instant::now());
        }
        pending.hold(shell_run(ELAPSED_MS), Instant::now());
        assert_eq!(pending.rows.len(), MAX_HELD_ROWS);
        assert!(matches!(
            pending.rows.last(),
            Some(DeferredRow::ShellRun { .. })
        ));
    }

    #[test]
    fn a_flush_writes_what_the_writer_holds() {
        let (_temp, dir, _database) = store();
        hold(&dir, tool_call());
        hold(&dir, shell_run(ELAPSED_MS));
        flush(&dir).unwrap();
        assert_eq!(calls(&dir), 1);
        assert_eq!(shell_runs(&dir), 1);
    }
}
