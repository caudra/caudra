//! Coalescing write-behind cache with transactional SQLite persistence.
//!
//! Apps post session snapshots keyed by session id; the writer thread drains
//! the newest snapshot of every session per wake and performs suffix writes
//! when cursor classification proves the collections are append-only. Deletes
//! travel through the same per-session slot as saves, so
//! whichever the app asked for last is what reaches disk.

use std::collections::{HashMap, HashSet};
#[cfg(test)]
use std::fs;
use std::io;
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use caudra_storage::id::CaudraId;
#[cfg(test)]
use caudra_storage::sessions::SESSIONS_DB_FILE;
use caudra_storage::sessions::{SessionCursor, SessionDatabase, SessionError, SessionRecreation};
use caudra_storage::state::{WorkspaceTabs, write_workspace_tabs};
use caudra_storage::usage_ledger::{TurnUsage, UsageLedger};
use caudra_storage::{StateDir, StorageError};
use tracing::warn;

use crate::AppSession;

const SAVE_FAILED_PREFIX: &str = "Session save failed";
const SAVE_RECOVERED: &str = "Session save recovered";
const WORKSPACE_TABS_SAVE_FAILED_PREFIX: &str = "Workspace tabs save failed";
const STORAGE_WARNING_BYTES: u64 = 1024 * 1024 * 1024;
const WAL_WARNING_BYTES: u64 = 64 * 1024 * 1024;
const CHECKPOINT_COMMIT_INTERVAL: u32 = 128;
const CHECKPOINT_STALL_WARNING_COUNT: u32 = 2;

type Pending = Arc<Mutex<HashMap<CaudraId, Entry>>>;
type PendingWorkspaceTabs = Arc<Mutex<Option<WorkspaceTabsRequest>>>;
/// Turns accumulate rather than coalescing: the ledger sums spend, so a
/// dropped turn is money the lifetime total never learns about.
type PendingUsage = Arc<Mutex<Vec<TurnUsage>>>;

type DeleteCallback = Box<dyn FnOnce(Result<(), SessionError>) + Send>;
type SaveCallback = flume::Sender<Result<(), SessionError>>;

/// One slot per session, holding whatever the app asked for last. Deletes
/// used to ride a side channel, where a flush queued before a delete could
/// drain a save enqueued after it, so the delete unlinked a session the app
/// had just saved.
enum Entry {
    Save(Arc<AppSession>),
    SaveSync(Arc<AppSession>, SaveCallback),
    Delete(DeleteCallback),
}

struct WorkspaceTabsRequest {
    cwd: PathBuf,
    tabs: WorkspaceTabs,
}

pub struct StorageWriter {
    pending: Pending,
    wake: flume::Sender<()>,
    workspace_tabs: PendingWorkspaceTabs,
    usage: PendingUsage,
    done_rx: flume::Receiver<()>,
    generation: Arc<AtomicU64>,
}

impl StorageWriter {
    pub fn new(dir: StateDir, warn_tx: flume::Sender<String>) -> Self {
        let pending: Pending = Arc::default();
        let writer_pending = Arc::clone(&pending);
        let workspace_tabs: PendingWorkspaceTabs = Arc::default();
        let writer_workspace_tabs = Arc::clone(&workspace_tabs);
        let usage: PendingUsage = Arc::default();
        let writer_usage = Arc::clone(&usage);
        let (wake, wake_rx) = flume::bounded::<()>(1);
        let (done_tx, done_rx) = flume::bounded::<()>(1);
        let generation: Arc<AtomicU64> = Arc::default();
        let writer_generation = Arc::clone(&generation);

        std::thread::Builder::new()
            .name("storage-writer".into())
            .spawn(move || {
                let mut writer = Writer {
                    dir,
                    warn_tx,
                    database: None,
                    ledger: None,
                    cursors: HashMap::new(),
                    deleted_sessions: HashMap::new(),
                    failing: HashSet::new(),
                    size_warning_level: 0,
                    wal_warning_active: false,
                    commits_since_checkpoint: 0,
                    checkpoint_stalls: 0,
                    generation: writer_generation,
                };
                while wake_rx.recv().is_ok() {
                    writer.drain(&writer_pending, &writer_workspace_tabs, &writer_usage);
                }
                writer.drain(&writer_pending, &writer_workspace_tabs, &writer_usage);
                if let Some(database) = &writer.database
                    && let Err(error) = database.checkpoint(false)
                {
                    warn!(%error, "session database checkpoint failed during shutdown");
                }
                let _ = done_tx.send(());
            })
            .expect("failed to spawn storage writer thread");

        Self {
            pending,
            wake,
            workspace_tabs,
            usage,
            done_rx,
            generation,
        }
    }

    /// Counts stored-session changes this process has made. A view built from
    /// a disk query holds the value it read at and re-queries when it moves;
    /// nothing else tells it that a session it is showing has been retitled or
    /// erased, because those rows are not the ones the event loop publishes.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Records what a turn spent. Never coalesced and never dropped on a
    /// superseding write: two turns in one bucket must both reach the sum.
    pub fn record_usage(&self, turn: TurnUsage) {
        self.usage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(turn);
        match self.wake.try_send(()) {
            Ok(()) | Err(flume::TrySendError::Full(())) => {}
            Err(flume::TrySendError::Disconnected(())) => {
                warn!("storage writer unavailable; turn spend may not be recorded");
            }
        }
    }

    pub fn send(&self, session: Arc<AppSession>) {
        self.enqueue(session.id, Entry::Save(session));
    }

    pub fn save_sync(&self, session: Arc<AppSession>) -> Result<(), SessionError> {
        let id = session.id;
        let (done_tx, done_rx) = flume::bounded(1);
        self.enqueue(id, Entry::SaveSync(session, done_tx));
        done_rx.recv().unwrap_or_else(|_| Err(writer_gone()))
    }

    /// Queue deletion on the writer thread; `done` fires after the canonical
    /// row is gone and external cleanup is durably queued. Deleting a session
    /// that was never written reports success, and a later save supersedes it.
    pub fn delete(
        &self,
        id: CaudraId,
        done: impl FnOnce(Result<(), SessionError>) + Send + 'static,
    ) {
        self.enqueue(id, Entry::Delete(Box::new(done)));
    }

    pub fn delete_sync(&self, id: CaudraId) -> Result<(), SessionError> {
        let (done_tx, done_rx) = flume::bounded(1);
        self.delete(id, move |result| {
            let _ = done_tx.send(result);
        });
        done_rx.recv().unwrap_or_else(|_| Err(writer_gone()))
    }

    pub fn persist_workspace_tabs(&self, cwd: PathBuf, tabs: WorkspaceTabs) {
        *self
            .workspace_tabs
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(WorkspaceTabsRequest { cwd, tabs });
        match self.wake.try_send(()) {
            Ok(()) | Err(flume::TrySendError::Full(())) => {}
            Err(flume::TrySendError::Disconnected(())) => {
                warn!("storage writer unavailable; workspace tabs may not be saved");
            }
        }
    }

    fn enqueue(&self, id: CaudraId, entry: Entry) {
        let superseded = { lock(&self.pending).insert(id, entry) };
        if let Some(superseded) = superseded {
            resolve_entry(superseded, Err(superseded_error(id)));
        }
        // One token is enough because a wake drains the whole coalesced map.
        // Keeping this bounded prevents a stalled disk from accumulating a
        // second unbounded queue beside the snapshots.
        match self.wake.try_send(()) {
            Ok(()) | Err(flume::TrySendError::Full(())) => {}
            Err(flume::TrySendError::Disconnected(())) => {
                if let Some(entry) = lock(&self.pending).remove(&id) {
                    resolve_entry(entry, Err(writer_gone()));
                }
            }
        }
    }

    pub fn shutdown(self, timeout: Duration) {
        drop(self.wake);
        if self.done_rx.recv_timeout(timeout).is_err() {
            warn!("storage writer did not drain within {timeout:?}");
        }
    }
}

fn lock(pending: &Pending) -> std::sync::MutexGuard<'_, HashMap<CaudraId, Entry>> {
    pending.lock().unwrap_or_else(|e| e.into_inner())
}

fn writer_gone() -> SessionError {
    StorageError::Io(io::Error::other("storage writer unavailable")).into()
}

fn superseded_error(id: CaudraId) -> SessionError {
    StorageError::Io(io::Error::other(format!(
        "storage operation for session {id} was superseded by a newer operation"
    )))
    .into()
}

fn resolve_entry(entry: Entry, result: Result<(), SessionError>) {
    match entry {
        Entry::Delete(done) => done(result),
        Entry::SaveSync(_, done) => {
            let _ = done.send(result);
        }
        Entry::Save(_) => {}
    }
}

/// Everything the writer thread owns. It never leaves that thread, so nothing
/// here needs a lock.
struct Writer {
    dir: StateDir,
    warn_tx: flume::Sender<String>,
    database: Option<SessionDatabase>,
    ledger: Option<UsageLedger>,
    cursors: HashMap<CaudraId, SessionCursor>,
    /// Explicit recreation capability retained only by the writer that
    /// completed an ordered delete; ordinary stale saves cannot cross tombstones.
    deleted_sessions: HashMap<CaudraId, SessionRecreation>,
    /// Sessions whose last write failed, so a sick disk warns once instead of
    /// once per frame.
    failing: HashSet<CaudraId>,
    size_warning_level: u32,
    wal_warning_active: bool,
    commits_since_checkpoint: u32,
    checkpoint_stalls: u32,
    generation: Arc<AtomicU64>,
}

impl Writer {
    fn forget(&mut self, id: CaudraId) {
        self.cursors.remove(&id);
        self.failing.remove(&id);
    }

    /// Publishes that a session row moved, before the operation reports back:
    /// a caller that waits for its own write must never then read a
    /// generation that predates it.
    fn mark_changed(&self, result: &Result<(), SessionError>) {
        if result.is_ok() {
            self.generation.fetch_add(1, Ordering::Release);
        }
    }

    fn flush(&mut self, pending: &Pending) {
        // Bound first: a `for` head temporary lives for the whole loop, so
        // iterating the guard directly would deadlock the re-insert below.
        let batch = mem::take(&mut *lock(pending));
        for (id, entry) in batch {
            match entry {
                Entry::Save(session) => {
                    let result = self.write(&session);
                    if result.as_ref().is_err_and(retryable) {
                        // `checkpoint` never resends an unchanged revision, so
                        // a dropped snapshot would miss disk for good.
                        // `or_insert` lets a newer op win; the shutdown flush
                        // is the last retry.
                        lock(pending).entry(id).or_insert(Entry::Save(session));
                    }
                    self.mark_changed(&result);
                    self.report(id, &result);
                }
                Entry::SaveSync(session, done) => {
                    let result = self.write(&session);
                    self.mark_changed(&result);
                    self.report(id, &result);
                    let _ = done.send(result);
                }
                Entry::Delete(done) => {
                    let (session_result, recreation) = self.delete(id);
                    if session_result.is_ok() {
                        self.forget(id);
                        if let Some(recreation) = recreation {
                            self.deleted_sessions.insert(id, recreation);
                        }
                        self.checkpoint(true);
                    }
                    self.mark_changed(&session_result);
                    done(session_result);
                }
            }
        }
    }

    fn flush_usage(&mut self, pending: &PendingUsage) {
        let turns = mem::take(&mut *pending.lock().unwrap_or_else(|e| e.into_inner()));
        if turns.is_empty() {
            return;
        }
        let ledger = match &self.ledger {
            Some(ledger) => ledger,
            None => match UsageLedger::open(&self.dir) {
                Ok(ledger) => self.ledger.insert(ledger),
                Err(error) => {
                    warn!(%error, turns = turns.len(), "usage ledger unavailable; spend not recorded");
                    return;
                }
            },
        };
        for turn in &turns {
            if let Err(error) = ledger.record(turn) {
                warn!(%error, model = turn.model, "usage ledger write failed");
            }
        }
    }

    /// One pass over everything queued. The tabs request is claimed before the
    /// snapshots are written, because `write_workspace_tabs` keeps only the ids
    /// the database still holds and the slot is emptied by the claim. Reading
    /// it afterwards let a delete queued ahead of the request land behind it,
    /// leaving a tab pointing at a session that was on its way out and no
    /// second request to correct it.
    fn drain(&mut self, pending: &Pending, tabs: &PendingWorkspaceTabs, usage: &PendingUsage) {
        let request = tabs.lock().unwrap_or_else(|e| e.into_inner()).take();
        self.flush(pending);
        if let Some(request) = request {
            self.flush_workspace_tabs(request);
        }
        self.flush_usage(usage);
    }

    fn flush_workspace_tabs(&mut self, request: WorkspaceTabsRequest) {
        let cwd = request.cwd.clone();
        if let Err(error) = self.write_workspace_tabs(request) {
            warn!(cwd = %cwd.display(), %error, "workspace tabs write failed");
            let _ = self
                .warn_tx
                .send(format!("{WORKSPACE_TABS_SAVE_FAILED_PREFIX}: {error}"));
        }
    }

    fn write_workspace_tabs(
        &mut self,
        mut request: WorkspaceTabsRequest,
    ) -> Result<(), SessionError> {
        if self.dir.is_ephemeral() {
            return Ok(());
        }
        if self.database.is_none() {
            self.database = Some(SessionDatabase::open(&self.dir)?);
        }
        let cwd = request.cwd.canonicalize().map_err(StorageError::from)?;
        let existing = self
            .database
            .as_ref()
            .expect("database initialized")
            .session_facts(None)?
            .into_iter()
            .filter_map(|facts| {
                Path::new(&facts.cwd)
                    .canonicalize()
                    .ok()
                    .filter(|stored_cwd| stored_cwd == &cwd)
                    .map(|_| facts.id)
            })
            .collect::<HashSet<_>>();
        let mut seen = HashSet::new();
        request
            .tabs
            .open
            .retain(|id| existing.contains(id) && seen.insert(*id));
        request.tabs.focused = request
            .tabs
            .focused
            .filter(|id| request.tabs.open.contains(id));
        write_workspace_tabs(&self.dir, &cwd, &request.tabs)?;
        Ok(())
    }

    fn write(&mut self, session: &AppSession) -> Result<(), SessionError> {
        if self.database.is_none() {
            self.database = Some(SessionDatabase::open(&self.dir)?);
        }
        let database = self.database.as_mut().expect("database initialized");
        if let Some(cursor) = self.cursors.get(&session.id)
            && cursor.shares_lineage(session)
            && session
                .persisted_write_version()
                .is_none_or(|base| base < cursor.write_version())
        {
            // This queue is the serialization proof: a snapshot enqueued while
            // its predecessor was committing is causally newer, so it may adopt
            // that commit before the repository checks its frozen base.
            session.adopt_persisted_write_version(cursor.write_version());
        }
        let saved = if let Some(recreation) = self.deleted_sessions.get(&session.id) {
            database.recreate(session, recreation)?
        } else {
            // Borrow rather than remove: a retryable failure must retain the
            // latest committed cursor instead of falling back to a stale full write.
            database.save(session, self.cursors.get(&session.id))?
        };
        self.deleted_sessions.remove(&session.id);
        self.cursors.insert(session.id, saved);
        self.commits_since_checkpoint = self.commits_since_checkpoint.saturating_add(1);
        self.checkpoint(false);
        self.report_storage_growth();
        Ok(())
    }

    fn delete(&mut self, id: CaudraId) -> (Result<(), SessionError>, Option<SessionRecreation>) {
        if self.database.is_none() {
            match SessionDatabase::open(&self.dir) {
                Ok(database) => self.database = Some(database),
                Err(error) => return (Err(error), None),
            }
        }
        let database = self.database.as_mut().expect("database initialized");
        let expected = match self.cursors.get(&id).map(SessionCursor::write_version) {
            Some(version) => Some(version),
            None => match database.write_version(id) {
                Ok(version) => version,
                Err(error) => return (Err(error), None),
            },
        };
        match AppSession::delete_for_recreation(id, &self.dir, expected) {
            Ok(version) => (Ok(()), Some(version)),
            Err(SessionError::Storage(StorageError::NotFound(_))) => (Ok(()), None),
            Err(error) => (Err(error), None),
        }
    }

    fn report(&mut self, id: CaudraId, result: &Result<(), impl std::fmt::Display>) {
        match result {
            Ok(()) => {
                if self.failing.remove(&id) {
                    let _ = self.warn_tx.send(SAVE_RECOVERED.to_string());
                }
            }
            Err(e) => {
                warn!(error = %e, %id, "session write failed");
                if self.failing.insert(id) {
                    let _ = self.warn_tx.send(format!("{SAVE_FAILED_PREFIX}: {e}"));
                }
            }
        }
    }

    fn checkpoint(&mut self, force: bool) {
        if !force && self.commits_since_checkpoint < CHECKPOINT_COMMIT_INTERVAL {
            return;
        }
        self.commits_since_checkpoint = 0;
        let Some(database) = &self.database else {
            return;
        };
        match database.checkpoint(false) {
            Ok(result) => {
                tracing::debug!(
                    busy = result.busy,
                    log_frames = result.log_frames,
                    checkpointed_frames = result.checkpointed_frames,
                    "session database passive checkpoint"
                );
                let stalled =
                    result.busy > 0 || (result.log_frames > 0 && result.checkpointed_frames == 0);
                if stalled {
                    self.checkpoint_stalls = self.checkpoint_stalls.saturating_add(1);
                    if self.checkpoint_stalls == CHECKPOINT_STALL_WARNING_COUNT {
                        let _ = self.warn_tx.send(format!(
                            "Session WAL checkpoint made no progress ({} frames remain)",
                            result.log_frames
                        ));
                    }
                } else {
                    self.checkpoint_stalls = 0;
                }
            }
            Err(error) => warn!(%error, "session database passive checkpoint failed"),
        }
    }

    fn report_storage_growth(&mut self) {
        let Some(database) = &self.database else {
            return;
        };
        let Ok(stats) = database.stats() else {
            return;
        };
        let total = stats.database_bytes.saturating_add(stats.wal_bytes);
        let threshold = STORAGE_WARNING_BYTES
            .checked_shl(self.size_warning_level)
            .unwrap_or(u64::MAX);
        if total >= threshold {
            let _ = self.warn_tx.send(format!(
                "Session storage is {} MiB (database plus WAL)",
                total / (1024 * 1024)
            ));
            self.size_warning_level = self.size_warning_level.saturating_add(1);
        }
        if stats.wal_bytes >= WAL_WARNING_BYTES && !self.wal_warning_active {
            let _ = self.warn_tx.send(format!(
                "Session WAL is {} MiB; a reader may be blocking checkpoints",
                stats.wal_bytes / (1024 * 1024)
            ));
            self.wal_warning_active = true;
        } else if stats.wal_bytes < WAL_WARNING_BYTES / 2 {
            self.wal_warning_active = false;
        }
    }
}

fn retryable(error: &SessionError) -> bool {
    !matches!(
        error,
        SessionError::AlreadyExists { .. }
            | SessionError::SessionInUse { .. }
            | SessionError::ConcurrentSessionWriter { .. }
            | SessionError::UnsupportedSchemaVersion { .. }
            | SessionError::CorruptDatabaseValue { .. }
            | SessionError::LimitExceeded { .. }
            | SessionError::LoadBudgetExceeded { .. }
            | SessionError::VersionMismatch { .. }
            | SessionError::IdMismatch { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use caudra_storage::usage_ledger::LedgerPurpose;
    use tempfile::TempDir;

    const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);
    const MODEL: &str = "test-model";
    const CWD: &str = "/tmp/writer";
    const MSG_PREFIX: &str = "msg-";
    const RESUMED_MSG: &str = "resumed";
    const TOOL_ID: &str = "tool-1";
    const TOOL_TEXT: &str = "tool output";
    const PROVIDER: &str = "anthropic";
    const TURNS_ACCUMULATE: &str =
        "each turn must reach the sum; the ledger is money, not a snapshot";
    const SPEND_OUTLIVES: &str = "forgetting a session must not erase what it cost";
    const TITLE: &str = "renamed after reload";
    const GENERATION_LAGS: &str =
        "a change that has already reported back must be visible in the generation that follows it";
    const GENERATION_RAN_AHEAD: &str =
        "nothing reached disk, so nothing should ask a reader to look again";

    fn state_dir() -> (TempDir, StateDir) {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        (tmp, dir)
    }

    /// A writer driven directly, with no thread behind it, so a test can
    /// step one flush at a time.
    fn bare_writer(dir: &StateDir, warn_tx: flume::Sender<String>) -> Writer {
        Writer {
            dir: dir.clone(),
            warn_tx,
            database: None,
            ledger: None,
            cursors: HashMap::new(),
            deleted_sessions: HashMap::new(),
            failing: HashSet::new(),
            size_warning_level: 0,
            wal_warning_active: false,
            commits_since_checkpoint: 0,
            checkpoint_stalls: 0,
            generation: Arc::default(),
        }
    }

    fn writer(dir: &StateDir) -> (StorageWriter, flume::Receiver<String>) {
        let (warn_tx, warn_rx) = flume::unbounded();
        (StorageWriter::new(dir.clone(), warn_tx), warn_rx)
    }

    fn message_texts(session: &AppSession) -> Vec<String> {
        caudra_providers::project_messages(session.messages())
            .unwrap()
            .iter()
            .map(|m| m.user_text().unwrap_or_default().to_string())
            .collect()
    }

    fn msg_text(n: usize) -> String {
        format!("{MSG_PREFIX}{n}")
    }

    fn user_message(n: usize) -> caudra_providers::Message {
        caudra_providers::Message::user(msg_text(n))
    }

    fn block_session_database(dir: &StateDir) {
        std::fs::create_dir(dir.path().join(SESSIONS_DB_FILE)).unwrap();
    }

    /// A caller that waited for its own delete then read the generation used
    /// to be able to see the value from before it, and treat a store it had
    /// just changed as unchanged.
    #[test]
    fn a_completed_change_is_never_older_than_the_generation_it_reports() {
        let (_tmp, dir) = state_dir();
        let (writer, _warn_rx) = writer(&dir);
        let session = AppSession::new(MODEL, CWD);
        let id = session.id;
        let empty = writer.generation();

        writer.save_sync(Arc::new(session)).unwrap();
        let saved = writer.generation();
        writer.delete_sync(id).unwrap();
        let deleted = writer.generation();

        assert!(saved > empty, "{GENERATION_LAGS}");
        assert!(deleted > saved, "{GENERATION_LAGS}");
    }

    /// A write that is going to be retried has changed nothing yet, and a
    /// view that re-queries the store on every attempt would spend a disk
    /// read per failure to find the same rows.
    #[test]
    fn a_failed_write_leaves_the_generation_where_it_was() {
        let (_tmp, dir) = state_dir();
        block_session_database(&dir);
        let (writer, _warn_rx) = writer(&dir);
        let before = writer.generation();

        writer
            .save_sync(Arc::new(AppSession::new(MODEL, CWD)))
            .unwrap_err();

        assert_eq!(writer.generation(), before, "{GENERATION_RAN_AHEAD}");
    }

    /// Snapshots must coalesce per session id, not into one `latest` slot:
    /// two racing sessions used to silently drop one.
    #[test]
    fn shutdown_drains_newest_snapshot_of_every_session() {
        let (_tmp, dir) = state_dir();
        let (writer, _warn_rx) = writer(&dir);
        let a = AppSession::new("test-model", "/tmp/a");
        let mut b = AppSession::new("test-model", "/tmp/b");
        let (a_id, b_id) = (a.id, b.id);
        writer.send(Arc::new(a));
        writer.send(Arc::new(b.clone()));
        b.set_title("renamed".into());
        writer.send(Arc::new(b));
        writer.shutdown(DRAIN_TIMEOUT);

        assert!(AppSession::load(a_id, &dir).is_ok());
        assert_eq!(AppSession::load(b_id, &dir).unwrap().title, "renamed");
    }

    #[test]
    fn delete_discards_pending_snapshot() {
        let (_tmp, dir) = state_dir();
        let (writer, _warn_rx) = writer(&dir);
        let session = AppSession::new("test-model", "/tmp/c");
        let id = session.id;
        writer.send(Arc::new(session));
        let (done_tx, done_rx) = flume::bounded(1);
        writer.delete(id, move |res| {
            let _ = done_tx.send(res);
        });
        writer.shutdown(DRAIN_TIMEOUT);

        assert!(done_rx.recv().unwrap().is_ok());
        assert!(AppSession::load(id, &dir).is_err());
    }

    #[test]
    fn workspace_tabs_keep_only_sessions_present_after_ordered_writes() {
        let (tmp, dir) = state_dir();
        let workspace = tmp.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let cwd = workspace.to_string_lossy();
        let (writer, _warn_rx) = writer(&dir);
        let kept = Arc::new(AppSession::new(MODEL, &cwd));
        let deleted = Arc::new(AppSession::new(MODEL, &cwd));
        let unsaved = AppSession::new(MODEL, &cwd);
        let (kept_id, deleted_id, unsaved_id) = (kept.id, deleted.id, unsaved.id);

        writer.save_sync(Arc::clone(&deleted)).unwrap();
        writer.send(kept);
        let (done_tx, done_rx) = flume::bounded(1);
        writer.delete(deleted_id, move |result| {
            let _ = done_tx.send(result);
        });
        writer.persist_workspace_tabs(
            workspace.clone(),
            WorkspaceTabs {
                open: vec![kept_id, deleted_id, unsaved_id, kept_id],
                focused: Some(deleted_id),
            },
        );
        writer.shutdown(DRAIN_TIMEOUT);

        done_rx.recv().unwrap().unwrap();
        assert_eq!(
            caudra_storage::state::read_workspace_tabs(&dir, &workspace).unwrap(),
            Some(WorkspaceTabs {
                open: vec![kept_id],
                focused: None,
            })
        );
    }

    fn spend(model: &str, cost: Option<f64>) -> TurnUsage {
        TurnUsage {
            provider: PROVIDER.into(),
            model: model.into(),
            cwd: CWD.into(),
            purpose: LedgerPurpose::Chat,
            input: 1,
            output: 2,
            cache_creation: 0,
            cache_read: 0,
            cost,
            subscription: false,
        }
    }

    #[test]
    fn every_recorded_turn_reaches_the_ledger() {
        let (_tmp, dir) = state_dir();
        let (writer, _warn_rx) = writer(&dir);

        writer.record_usage(spend(MODEL, Some(1.0)));
        writer.record_usage(spend(MODEL, Some(2.0)));
        writer.record_usage(spend(MODEL, None));
        writer.shutdown(DRAIN_TIMEOUT);

        let rows = UsageLedger::open(&dir).unwrap().buckets(None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].cost, 3.0, "{TURNS_ACCUMULATE}");
        assert_eq!(rows[0].priced_turns, 2, "{TURNS_ACCUMULATE}");
        assert_eq!(rows[0].unpriced_turns, 1, "{TURNS_ACCUMULATE}");
        assert_eq!(rows[0].input, 3, "{TURNS_ACCUMULATE}");
    }

    #[test]
    fn deleting_a_session_leaves_its_recorded_spend_behind() {
        let (_tmp, dir) = state_dir();
        let (writer, _warn_rx) = writer(&dir);
        let mut session = AppSession::new(MODEL, CWD);
        crate::push_history_message(&mut session, user_message(0));
        let id = session.id;
        writer.save_sync(Arc::new(session)).unwrap();
        writer.record_usage(spend(MODEL, Some(9.0)));
        writer.record_usage(spend(MODEL, Some(1.0)));

        writer.delete_sync(id).unwrap();
        writer.shutdown(DRAIN_TIMEOUT);

        assert!(AppSession::load(id, &dir).is_err());
        let rows = UsageLedger::open(&dir).unwrap().buckets(None).unwrap();
        assert_eq!(rows.len(), 1, "{SPEND_OUTLIVES}");
        assert_eq!(rows[0].cost, 10.0, "{SPEND_OUTLIVES}");
    }

    #[test]
    fn synchronous_save_is_visible_before_it_returns() {
        let (_tmp, dir) = state_dir();
        let (writer, _warn_rx) = writer(&dir);
        let mut session = AppSession::new(MODEL, CWD);
        crate::push_history_message(&mut session, user_message(0));
        let id = session.id;

        writer.save_sync(Arc::new(session)).unwrap();

        assert!(AppSession::load(id, &dir).is_ok());
        writer.shutdown(DRAIN_TIMEOUT);
    }

    #[test]
    fn delete_removes_all_workspace_snapshot_roots_for_the_session() {
        let (_tmp, dir) = state_dir();
        let (writer, _warn_rx) = writer(&dir);
        let mut session = AppSession::new(MODEL, CWD);
        crate::push_history_message(&mut session, user_message(0));
        let id = session.id;
        writer.save_sync(Arc::new(session)).unwrap();
        let snapshots = dir
            .path()
            .join(caudra_agent::snapshots::SESSION_SNAPSHOTS_DIR)
            .join(id.to_string());
        fs::create_dir_all(snapshots.join("workspace-a")).unwrap();
        fs::write(
            snapshots.join("workspace-a").join("journal.json"),
            "pending",
        )
        .unwrap();
        let (done_tx, done_rx) = flume::bounded(1);

        writer.delete(id, move |result| {
            let _ = done_tx.send(result);
        });

        done_rx.recv_timeout(DRAIN_TIMEOUT).unwrap().unwrap();
        assert!(!snapshots.exists());
        writer.shutdown(DRAIN_TIMEOUT);
    }

    #[test]
    fn newer_save_resolves_superseded_delete_callback_once() {
        let pending: Pending = Arc::default();
        let session = Arc::new(AppSession::new(MODEL, CWD));
        let id = session.id;
        let (done_tx, done_rx) = flume::unbounded();
        let callback = move |result| {
            let _ = done_tx.send(result);
        };

        lock(&pending).insert(id, Entry::Delete(Box::new(callback)));
        if let Some(superseded) = lock(&pending).insert(id, Entry::Save(session)) {
            resolve_entry(superseded, Err(superseded_error(id)));
        }

        let error = done_rx.recv().unwrap().unwrap_err().to_string();
        assert!(error.contains("superseded"), "{error}");
        assert!(done_rx.try_recv().is_err());
        assert!(matches!(lock(&pending).get(&id), Some(Entry::Save(_))));
    }

    #[test]
    fn save_sync_and_delete_interleaving_resolves_each_superseded_callback() {
        let pending: Pending = Arc::default();
        let session = Arc::new(AppSession::new(MODEL, CWD));
        let id = session.id;
        let (save_tx, save_rx) = flume::bounded(1);
        let (delete_tx, delete_rx) = flume::bounded(1);

        lock(&pending).insert(id, Entry::SaveSync(Arc::clone(&session), save_tx));
        let delete = Entry::Delete(Box::new(move |result| {
            let _ = delete_tx.send(result);
        }));
        if let Some(superseded) = lock(&pending).insert(id, delete) {
            resolve_entry(superseded, Err(superseded_error(id)));
        }
        if let Some(superseded) = lock(&pending).insert(id, Entry::Save(session)) {
            resolve_entry(superseded, Err(superseded_error(id)));
        }

        assert!(
            save_rx
                .recv()
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("superseded")
        );
        assert!(
            delete_rx
                .recv()
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("superseded")
        );
        assert!(save_rx.try_recv().is_err());
        assert!(delete_rx.try_recv().is_err());
        assert!(matches!(lock(&pending).get(&id), Some(Entry::Save(_))));
    }

    /// A fresh writer has no in-memory cursor for this live snapshot. Its first
    /// save must use persisted version state and replace a diverged collection,
    /// never infer a suffix from an unrelated cursor.
    #[test]
    fn reopened_log_rewrites_diverged_file_instead_of_appending() {
        let (_tmp, dir) = state_dir();
        let mut session = AppSession::new(MODEL, CWD);
        let id = session.id;
        for i in 0..5 {
            crate::push_history_message(&mut session, user_message(i));
        }
        let (first, _first_warn_rx) = writer(&dir);
        first.send(Arc::new(session.clone()));
        first.shutdown(DRAIN_TIMEOUT);

        session.truncate_messages(2);
        crate::push_history_message(
            &mut session,
            caudra_providers::Message::user(RESUMED_MSG.into()),
        );
        session.insert_tool_output(
            TOOL_ID.into(),
            caudra_agent::ToolOutput::Plain(TOOL_TEXT.to_string().into()),
        );
        session.set_title(TITLE.into());

        let (second, second_warn_rx) = writer(&dir);
        second.send(Arc::new(session.clone()));
        second.shutdown(DRAIN_TIMEOUT);

        let loaded = AppSession::load(id, &dir).unwrap();
        assert_eq!(
            message_texts(&loaded),
            [msg_text(0), msg_text(1), RESUMED_MSG.to_string()]
        );
        assert_eq!(loaded.title, TITLE);
        match loaded.tool_outputs().get(TOOL_ID).map(Arc::as_ref) {
            Some(caudra_agent::ToolOutput::Plain(out)) => assert_eq!(out.text, TOOL_TEXT),
            other => panic!("tool output lost: {other:?}"),
        }
        assert!(second_warn_rx.is_empty());
    }

    /// A disk that keeps failing warns once, not once per frame, and says so
    /// exactly once when writes start working again.
    #[test]
    fn failing_flush_warns_once_and_reports_recovery() {
        let (_tmp, dir) = state_dir();
        block_session_database(&dir);
        let (writer, warn_rx) = writer(&dir);
        let session = Arc::new(AppSession::new(MODEL, CWD));
        let id = session.id;

        writer.send(Arc::clone(&session));
        let warning = warn_rx.recv_timeout(DRAIN_TIMEOUT).unwrap();
        assert!(warning.starts_with(SAVE_FAILED_PREFIX), "{warning}");

        // The save is enqueued before the delete, so the flush that runs the
        // delete has already drained it; a repeat failure must stay silent.
        writer.send(Arc::clone(&session));
        let (done_tx, done_rx) = flume::bounded(1);
        writer.delete(CaudraId::generate(), move |res| {
            let _ = done_tx.send(res);
        });
        assert!(done_rx.recv_timeout(DRAIN_TIMEOUT).unwrap().is_err());
        assert!(warn_rx.is_empty(), "second failure warned again");

        std::fs::remove_dir(dir.path().join(SESSIONS_DB_FILE)).unwrap();
        writer.send(session);
        let recovered = warn_rx.recv_timeout(DRAIN_TIMEOUT).unwrap();
        assert_eq!(recovered, SAVE_RECOVERED);
        writer.shutdown(DRAIN_TIMEOUT);

        assert!(warn_rx.is_empty());
        assert!(AppSession::load(id, &dir).is_ok());
    }

    /// A save enqueued after a delete must win: clear a draft and retype it
    /// fast enough, and the queued delete used to unlink the file the retype
    /// had just saved.
    #[test]
    fn save_enqueued_after_delete_survives() {
        let (_tmp, dir) = state_dir();
        let (writer, warn_rx) = writer(&dir);
        let mut session = AppSession::new(MODEL, CWD);
        let id = session.id;
        crate::push_history_message(&mut session, user_message(0));
        writer.send(Arc::new(session.clone()));
        writer.delete(id, |_| {});
        crate::push_history_message(
            &mut session,
            caudra_providers::Message::user(RESUMED_MSG.into()),
        );
        writer.send(Arc::new(session));
        writer.shutdown(DRAIN_TIMEOUT);

        let loaded = AppSession::load(id, &dir).unwrap_or_else(|error| {
            panic!(
                "recreated session missing: {error}; warnings: {:?}",
                warn_rx.try_iter().collect::<Vec<_>>()
            )
        });
        assert_eq!(
            message_texts(&loaded),
            [msg_text(0), RESUMED_MSG.to_string()]
        );
        assert!(warn_rx.is_empty());
    }

    /// A failed write stays queued: `checkpoint` never resends an unchanged
    /// revision, so the writer owns the retry, and the shutdown flush is the
    /// last one.
    #[test]
    fn failed_write_is_retried_by_a_later_flush() {
        let (_tmp, dir) = state_dir();
        block_session_database(&dir);
        let (writer, warn_rx) = writer(&dir);
        let session = Arc::new(AppSession::new(MODEL, CWD));
        let id = session.id;

        writer.send(session);
        let warning = warn_rx.recv_timeout(DRAIN_TIMEOUT).unwrap();
        assert!(warning.starts_with(SAVE_FAILED_PREFIX), "{warning}");

        std::fs::remove_dir(dir.path().join(SESSIONS_DB_FILE)).unwrap();
        writer.shutdown(DRAIN_TIMEOUT);

        assert!(AppSession::load(id, &dir).is_ok());
        assert_eq!(warn_rx.recv_timeout(DRAIN_TIMEOUT).unwrap(), SAVE_RECOVERED);
    }

    #[test]
    fn failed_write_keeps_its_cursor_for_retry() {
        let (_tmp, dir) = state_dir();
        let (warn_tx, _warn_rx) = flume::unbounded();
        let mut writer = bare_writer(&dir, warn_tx);
        let mut stale = AppSession::new(MODEL, CWD);
        let id = stale.id;
        writer.write(&stale).unwrap();
        let mut external = AppSession::load(id, &dir).unwrap();
        external.set_title("external".into());
        external.save(&dir).unwrap();
        stale.set_title("stale".into());

        let error = writer.write(&stale).unwrap_err();

        assert!(matches!(
            error,
            SessionError::ConcurrentSessionWriter { .. }
        ));
        assert_eq!(writer.cursors[&id].write_version(), 0);
    }

    #[test]
    fn queued_resumed_snapshot_adopts_the_writers_own_commit() {
        let (_tmp, dir) = state_dir();
        let mut stored = AppSession::new(MODEL, CWD);
        stored.save(&dir).unwrap();
        let first = stored.clone();
        let mut queued = stored.clone();
        queued.set_title("queued".into());
        let (warn_tx, warn_rx) = flume::unbounded();
        let mut writer = bare_writer(&dir, warn_tx);

        writer.write(&first).unwrap();
        writer.write(&queued).unwrap();

        assert_eq!(AppSession::load(stored.id, &dir).unwrap().title, "queued");
        assert!(warn_rx.is_empty());
    }

    #[test]
    fn queued_snapshot_never_adopts_an_unrelated_lineage() {
        let (_tmp, dir) = state_dir();
        let mut stored = AppSession::new(MODEL, CWD);
        stored.save(&dir).unwrap();
        let first = AppSession::load(stored.id, &dir).unwrap();
        let mut unrelated = AppSession::load(stored.id, &dir).unwrap();
        unrelated.set_title("unrelated stale".into());
        let (warn_tx, _warn_rx) = flume::unbounded();
        let mut writer = bare_writer(&dir, warn_tx);
        writer.write(&first).unwrap();

        assert!(matches!(
            writer.write(&unrelated),
            Err(SessionError::ConcurrentSessionWriter { .. })
        ));
        assert_ne!(
            AppSession::load(stored.id, &dir).unwrap().title,
            unrelated.title
        );
    }

    /// A delete invalidates the ordinary cursor and creates a tombstone. The
    /// deleting writer alone retains the explicit version needed to recreate
    /// the session from a later snapshot.
    #[test]
    fn session_recreated_after_delete_is_written_in_full() {
        let (_tmp, dir) = state_dir();
        let (writer, warn_rx) = writer(&dir);
        let mut session = AppSession::new(MODEL, CWD);
        let id = session.id;
        crate::push_history_message(&mut session, user_message(0));
        writer.send(Arc::new(session.clone()));

        let (done_tx, done_rx) = flume::bounded(1);
        writer.delete(id, move |res| {
            let _ = done_tx.send(res);
        });
        done_rx.recv_timeout(DRAIN_TIMEOUT).unwrap().unwrap();
        assert!(AppSession::load(id, &dir).is_err());

        crate::push_history_message(
            &mut session,
            caudra_providers::Message::user(RESUMED_MSG.into()),
        );
        writer.send(Arc::new(session));
        writer.shutdown(DRAIN_TIMEOUT);

        let loaded = AppSession::load(id, &dir).unwrap();
        assert_eq!(
            message_texts(&loaded),
            [msg_text(0), RESUMED_MSG.to_string()]
        );
        assert!(warn_rx.is_empty());
    }
}
