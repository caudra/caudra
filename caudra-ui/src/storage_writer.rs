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
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::Duration;

use caudra_storage::id::CaudraId;
use caudra_storage::permission_state::mutation::{
    PermissionCommitReceipt, PermissionMutationError, PermissionOwner, PreparedPermissionMutation,
    permission_databases_shared,
};
#[cfg(test)]
use caudra_storage::sessions::SESSIONS_DB_FILE;
use caudra_storage::sessions::{
    SessionCursor, SessionDatabase, SessionError, SessionRecreation, WAL_RETENTION_LIMIT_BYTES,
};
use caudra_storage::state::{WorkspaceTabs, write_workspace_tabs};
use caudra_storage::tool_ledger::{ToolCall, ToolLedger};
use caudra_storage::usage_ledger::{TurnUsage, UsageLedger};
use caudra_storage::{StateClass, StateDir, StorageError, now_epoch};
use tracing::warn;

use crate::AppSession;

const SAVE_FAILED_PREFIX: &str = "Session save failed";
const SAVE_RECOVERED: &str = "Session save recovered";
const WORKSPACE_TABS_SAVE_FAILED_PREFIX: &str = "Workspace tabs save failed";
const STORAGE_WARNING_BYTES: u64 = 1024 * 1024 * 1024;
/// A `-wal` sitting at the retention limit is the designed steady state, so an
/// alarm set there would fire on every burst. Only a WAL that has outgrown what
/// a reset is allowed to keep says checkpoints are not getting through.
const WAL_WARNING_BYTES: u64 = 2 * WAL_RETENTION_LIMIT_BYTES;
const CHECKPOINT_COMMIT_INTERVAL: u32 = 128;
const CHECKPOINT_STALL_WARNING_COUNT: u32 = 2;
const WRITER_UNAVAILABLE: &str = "storage writer unavailable";
const WRITER_TIMEOUT: &str = "storage writer operation timed out";
const WRITER_DRAIN_FAILED: &str = "storage writer stopped with unsaved session operations";
const USAGE_DRAIN_FAILED: &str = "storage writer stopped with unsaved usage contributions";
const TOOL_DRAIN_FAILED: &str = "storage writer stopped with unsaved tool activity";
const PERMISSION_QUEUE_CAPACITY: usize = 16;
const PERMISSION_QUEUE_FULL: &str = "permission mutation queue is full; nothing was enqueued";

type Pending = Arc<Mutex<HashMap<CaudraId, Entry>>>;
type PendingWorkspaceTabs = Arc<Mutex<Option<WorkspaceTabsRequest>>>;
/// Turns accumulate rather than coalescing: the ledger sums spend, so a
/// dropped turn is money the lifetime total never learns about.
type PendingUsage = Arc<Mutex<Vec<QueuedUsage>>>;
/// The same, for the ledger that sums what tools did.
type PendingToolCalls = Arc<Mutex<Vec<QueuedToolCall>>>;

type DeleteCallback = Box<dyn FnOnce(Result<(), SessionError>) + Send>;
type SaveCallback = flume::Sender<Result<(), SessionError>>;
pub type PermissionMutationAcknowledgment =
    flume::Receiver<Result<PermissionCommitReceipt, PermissionMutationError>>;

struct PermissionWrite {
    prepared: PreparedPermissionMutation,
    done: flume::Sender<Result<PermissionCommitReceipt, PermissionMutationError>>,
}

#[derive(Clone)]
pub struct PermissionMutationWriter {
    pending: flume::Sender<PermissionWrite>,
    wake: Weak<flume::Sender<()>>,
}

impl PermissionMutationWriter {
    pub fn submit(
        &self,
        prepared: PreparedPermissionMutation,
    ) -> Result<PermissionMutationAcknowledgment, PermissionMutationError> {
        let wake = self.wake.upgrade().ok_or_else(writer_gone)?;
        let (done, receiver) = flume::bounded(1);
        match self.pending.try_send(PermissionWrite { prepared, done }) {
            Ok(()) => {}
            Err(flume::TrySendError::Full(_)) => {
                let error = StorageError::Io(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    PERMISSION_QUEUE_FULL,
                ));
                return Err(SessionError::from(error).into());
            }
            Err(flume::TrySendError::Disconnected(_)) => return Err(writer_gone().into()),
        }
        match wake.try_send(()) {
            Ok(()) | Err(flume::TrySendError::Full(())) => Ok(receiver),
            Err(flume::TrySendError::Disconnected(())) => Err(writer_gone().into()),
        }
    }
}

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

#[cfg_attr(test, derive(Debug, Clone, PartialEq))]
struct QueuedUsage {
    turn: TurnUsage,
    enqueued_at: u64,
}

#[cfg_attr(test, derive(Debug, Clone, PartialEq))]
struct QueuedToolCall {
    call: ToolCall,
    enqueued_at: u64,
}

pub struct StorageWriter {
    pending: Pending,
    wake: Arc<flume::Sender<()>>,
    workspace_tabs: PendingWorkspaceTabs,
    usage: PendingUsage,
    tool_calls: PendingToolCalls,
    done_rx: flume::Receiver<Result<(), SessionError>>,
    thread: JoinHandle<()>,
    generation: Arc<AtomicU64>,
    permission_mutations: flume::Sender<PermissionWrite>,
}

impl StorageWriter {
    pub fn new(dir: StateDir, warn_tx: flume::Sender<String>) -> Self {
        let pending: Pending = Arc::default();
        let writer_pending = Arc::clone(&pending);
        let workspace_tabs: PendingWorkspaceTabs = Arc::default();
        let writer_workspace_tabs = Arc::clone(&workspace_tabs);
        let usage: PendingUsage = Arc::default();
        let writer_usage = Arc::clone(&usage);
        let tool_calls: PendingToolCalls = Arc::default();
        let writer_tool_calls = Arc::clone(&tool_calls);
        let (wake, wake_rx) = flume::bounded::<()>(1);
        let wake = Arc::new(wake);
        let (done_tx, done_rx) = flume::bounded(1);
        let generation: Arc<AtomicU64> = Arc::default();
        let writer_generation = Arc::clone(&generation);
        let (permission_mutations, permission_mutations_rx) =
            flume::bounded(PERMISSION_QUEUE_CAPACITY);

        let thread = std::thread::Builder::new()
            .name("storage-writer".into())
            .spawn(move || {
                let mut writer = Writer {
                    dir,
                    warn_tx,
                    database: None,
                    ledger: None,
                    tool_ledger: None,
                    cursors: HashMap::new(),
                    deleted_sessions: HashMap::new(),
                    failing: HashSet::new(),
                    workspace_tabs_errors: HashMap::new(),
                    size_warning_level: 0,
                    wal_warning_active: false,
                    commits_since_checkpoint: 0,
                    checkpoint_stalls: 0,
                    generation: writer_generation,
                    permission_mutations: permission_mutations_rx,
                };
                while wake_rx.recv().is_ok() {
                    writer.drain(
                        &writer_pending,
                        &writer_workspace_tabs,
                        &writer_usage,
                        &writer_tool_calls,
                    );
                }
                let result = writer.finish(
                    &writer_pending,
                    &writer_workspace_tabs,
                    &writer_usage,
                    &writer_tool_calls,
                );
                drop(writer);
                let _ = done_tx.send(result);
            })
            .expect("failed to spawn storage writer thread");

        Self {
            pending,
            wake,
            workspace_tabs,
            usage,
            tool_calls,
            done_rx,
            thread,
            generation,
            permission_mutations,
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
        let usage = QueuedUsage {
            turn,
            enqueued_at: now_epoch(),
        };
        self.usage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(usage);
        match self.wake.try_send(()) {
            Ok(()) | Err(flume::TrySendError::Full(())) => {}
            Err(flume::TrySendError::Disconnected(())) => {
                warn!("storage writer unavailable; turn spend may not be recorded");
            }
        }
    }

    /// Records what one finished tool call did. Never coalesced, for the reason
    /// [`Self::record_usage`] is not: the ledger sums, so two calls in one hour
    /// must both reach the total.
    pub fn record_tool_call(&self, call: ToolCall) {
        self.tool_calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(QueuedToolCall {
                call,
                enqueued_at: now_epoch(),
            });
        match self.wake.try_send(()) {
            Ok(()) | Err(flume::TrySendError::Full(())) => {}
            Err(flume::TrySendError::Disconnected(())) => {
                warn!("storage writer unavailable; tool activity may not be recorded");
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

    pub fn save_sync_timeout(
        &self,
        session: Arc<AppSession>,
        timeout: Duration,
    ) -> Result<(), SessionError> {
        let (done_tx, done_rx) = flume::bounded(1);
        self.enqueue(session.id, Entry::SaveSync(session, done_tx));
        done_rx.recv_timeout(timeout).map_err(writer_wait_error)?
    }

    pub fn submit_permission_mutation(
        &self,
        prepared: PreparedPermissionMutation,
    ) -> Result<PermissionMutationAcknowledgment, PermissionMutationError> {
        self.permission_mutation_writer().submit(prepared)
    }

    pub fn permission_mutation_writer(&self) -> PermissionMutationWriter {
        PermissionMutationWriter {
            pending: self.permission_mutations.clone(),
            wake: Arc::downgrade(&self.wake),
        }
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

    pub fn shutdown_checked(self, timeout: Duration) -> Result<(), SessionError> {
        drop(self.wake);
        let result = self
            .done_rx
            .recv_timeout(timeout)
            .map_err(writer_wait_error)?;
        self.thread.join().map_err(|_| writer_gone())?;
        result
    }
}

fn lock(pending: &Pending) -> std::sync::MutexGuard<'_, HashMap<CaudraId, Entry>> {
    pending.lock().unwrap_or_else(|e| e.into_inner())
}

fn writer_gone() -> SessionError {
    StorageError::Io(io::Error::other(WRITER_UNAVAILABLE)).into()
}

fn writer_wait_error(error: flume::RecvTimeoutError) -> SessionError {
    match error {
        flume::RecvTimeoutError::Timeout => {
            StorageError::Io(io::Error::new(io::ErrorKind::TimedOut, WRITER_TIMEOUT)).into()
        }
        flume::RecvTimeoutError::Disconnected => writer_gone(),
    }
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
    tool_ledger: Option<ToolLedger>,
    cursors: HashMap<CaudraId, SessionCursor>,
    /// Explicit recreation capability retained only by the writer that
    /// completed an ordered delete; ordinary stale saves cannot cross tombstones.
    deleted_sessions: HashMap<CaudraId, SessionRecreation>,
    /// Sessions whose last write failed, so a sick disk warns once instead of
    /// once per frame.
    failing: HashSet<CaudraId>,
    workspace_tabs_errors: HashMap<PathBuf, SessionError>,
    size_warning_level: u32,
    wal_warning_active: bool,
    commits_since_checkpoint: u32,
    checkpoint_stalls: u32,
    generation: Arc<AtomicU64>,
    permission_mutations: flume::Receiver<PermissionWrite>,
}

impl Writer {
    fn flush_permissions(&mut self) {
        for _ in 0..PERMISSION_QUEUE_CAPACITY {
            let Ok(PermissionWrite { prepared, done }) = self.permission_mutations.try_recv()
            else {
                break;
            };
            let result = self.commit_permission_mutation(&prepared);
            if result.is_ok() {
                self.generation.fetch_add(1, Ordering::Release);
            }
            let _ = done.send(result);
        }
    }

    fn commit_permission_mutation(
        &mut self,
        prepared: &PreparedPermissionMutation,
    ) -> Result<PermissionCommitReceipt, PermissionMutationError> {
        if prepared.persistent_only() {
            return SessionDatabase::open_state(&self.dir.for_class(StateClass::Persistent))?
                .commit_permission_mutation(prepared);
        }
        if prepared
            .expected()
            .iter()
            .any(|snapshot| snapshot.revision.owner == PermissionOwner::Persistent)
            && !permission_databases_shared(&self.dir)?
        {
            return Err(PermissionMutationError::DifferentDatabase);
        }
        if self.database.is_none() {
            self.database = Some(SessionDatabase::open(&self.dir)?);
        }
        self.database
            .as_ref()
            .ok_or_else(writer_gone)?
            .commit_permission_mutation(prepared)
    }

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
                    } else {
                        self.failing.insert(id);
                    }
                    self.mark_changed(&session_result);
                    done(session_result);
                }
            }
        }
    }

    fn flush_usage(&mut self, pending: &PendingUsage) {
        let mut turns = mem::take(&mut *pending.lock().unwrap_or_else(|e| e.into_inner()));
        if turns.is_empty() {
            return;
        }
        let ledger = match &self.ledger {
            Some(ledger) => ledger,
            None => match UsageLedger::open(&self.dir) {
                Ok(ledger) => self.ledger.insert(ledger),
                Err(error) => {
                    warn!(%error, turns = turns.len(), "usage ledger unavailable; spend retained for retry");
                    pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .extend(turns);
                    return;
                }
            },
        };
        turns.retain(|usage| match ledger.record_at(&usage.turn, usage.enqueued_at) {
            Ok(()) => false,
            Err(error) => {
                warn!(%error, model = usage.turn.model, "usage ledger write failed; spend retained for retry");
                true
            }
        });
        if !turns.is_empty() {
            pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend(turns);
        }
    }

    fn flush_tool_calls(&mut self, pending: &PendingToolCalls) {
        let mut calls = mem::take(&mut *pending.lock().unwrap_or_else(|e| e.into_inner()));
        if calls.is_empty() {
            return;
        }
        let ledger = match &self.tool_ledger {
            Some(ledger) => ledger,
            None => match ToolLedger::open(&self.dir) {
                Ok(ledger) => self.tool_ledger.insert(ledger),
                Err(error) => {
                    warn!(%error, calls = calls.len(), "tool ledger unavailable; activity retained for retry");
                    pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .extend(calls);
                    return;
                }
            },
        };
        calls.retain(|queued| match ledger.record_at(&queued.call, queued.enqueued_at) {
            Ok(()) => false,
            Err(error) => {
                warn!(%error, tool = queued.call.tool, "tool ledger write failed; activity retained for retry");
                true
            }
        });
        if !calls.is_empty() {
            pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend(calls);
        }
    }

    /// One pass over everything queued. The tabs request is claimed before the
    /// snapshots are written, because `write_workspace_tabs` keeps only the ids
    /// the database still holds and the slot is emptied by the claim. Reading
    /// it afterwards let a delete queued ahead of the request land behind it,
    /// leaving a tab pointing at a session that was on its way out and no
    /// second request to correct it.
    fn drain(
        &mut self,
        pending: &Pending,
        tabs: &PendingWorkspaceTabs,
        usage: &PendingUsage,
        tool_calls: &PendingToolCalls,
    ) {
        let request = tabs.lock().unwrap_or_else(|e| e.into_inner()).take();
        self.flush(pending);
        self.flush_permissions();
        if let Some(request) = request {
            self.flush_workspace_tabs(request);
        }
        self.flush_usage(usage);
        self.flush_tool_calls(tool_calls);
    }

    fn finish(
        &mut self,
        pending: &Pending,
        tabs: &PendingWorkspaceTabs,
        usage: &PendingUsage,
        tool_calls: &PendingToolCalls,
    ) -> Result<(), SessionError> {
        self.drain(pending, tabs, usage, tool_calls);
        let checkpoint = self
            .database
            .as_ref()
            .map_or(Ok(()), |database| database.checkpoint(false).map(|_| ()));
        if let Err(error) = &checkpoint {
            warn!(%error, "session database checkpoint failed during shutdown");
        }
        if !self.failing.is_empty() || !lock(pending).is_empty() {
            return Err(StorageError::Io(io::Error::other(WRITER_DRAIN_FAILED)).into());
        }
        if !usage.lock().unwrap_or_else(|e| e.into_inner()).is_empty() {
            return Err(StorageError::Io(io::Error::other(USAGE_DRAIN_FAILED)).into());
        }
        if !tool_calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
        {
            return Err(StorageError::Io(io::Error::other(TOOL_DRAIN_FAILED)).into());
        }
        if let Some((_, error)) = self.workspace_tabs_errors.drain().next() {
            return Err(error);
        }
        checkpoint
    }

    fn flush_workspace_tabs(&mut self, request: WorkspaceTabsRequest) {
        let cwd = request.cwd.clone();
        if let Err(error) = self.write_workspace_tabs(request) {
            warn!(cwd = %cwd.display(), %error, "workspace tabs write failed");
            let _ = self
                .warn_tx
                .send(format!("{WORKSPACE_TABS_SAVE_FAILED_PREFIX}: {error}"));
            self.workspace_tabs_errors.insert(cwd, error);
        } else {
            self.workspace_tabs_errors.remove(&cwd);
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
        let cwd = workspace_tabs_path(&request.cwd)?;
        let mut existing = HashSet::new();
        for facts in self
            .database
            .as_ref()
            .expect("database initialized")
            .session_facts(None)?
        {
            if workspace_tabs_path(Path::new(&facts.cwd))? == cwd {
                existing.insert(facts.id);
            }
        }
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
                mib(total)
            ));
            self.size_warning_level = self.size_warning_level.saturating_add(1);
        }
        if self.wal_outgrew_its_limit(stats.wal_bytes) {
            let _ = self.warn_tx.send(format!(
                "Session WAL is {} MiB, past the {} MiB a checkpoint trims to; a reader may be blocking them",
                mib(stats.wal_bytes),
                mib(WAL_RETENTION_LIMIT_BYTES)
            ));
        }
    }

    /// Latches, so one burst warns once. It clears at the retention limit
    /// rather than at half the alarm: `journal_size_limit` holds the file
    /// there, so a lower clearing point is never reached and the warning could
    /// never fire a second time.
    fn wal_outgrew_its_limit(&mut self, wal_bytes: u64) -> bool {
        if wal_bytes <= WAL_RETENTION_LIMIT_BYTES {
            self.wal_warning_active = false;
            return false;
        }
        if wal_bytes < WAL_WARNING_BYTES || self.wal_warning_active {
            return false;
        }
        self.wal_warning_active = true;
        true
    }
}

fn workspace_tabs_path(path: &Path) -> Result<PathBuf, StorageError> {
    match path.canonicalize() {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(path.to_path_buf()),
        Err(error) => Err(error.into()),
    }
}

fn mib(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
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
    use caudra_storage::permission_state::mutation::{
        PermissionMutation, PermissionRecordIdentity, prepare_mutation,
    };
    use caudra_storage::permission_state::{
        PermissionArgumentConstraint, PermissionExecutorKind, PermissionLifetime,
        PermissionRuleRecord, PermissionSubject, StructuredPermissionEffect,
        StructuredPermissionRule,
    };
    use caudra_storage::sessions::SessionRelocation;
    use caudra_storage::usage_ledger::{BUCKET_SECONDS, LedgerPurpose, bucket_for};
    use jiff::civil::DateTime;
    use jiff::tz::TimeZone;
    use tempfile::TempDir;
    use test_case::test_case;

    const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);
    const MODEL: &str = "test-model";
    const CWD: &str = "/tmp/writer";
    const RELOCATION_DESTINATION: &str = "/tmp/writer-relocated";
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
    const WAL_AT_LIMIT_WARNED: &str =
        "a WAL at the size the retention limit keeps is the steady state, not an alarm";
    const WAL_GROWTH_SILENT: &str = "a WAL past the alarm must be announced";
    const WAL_WARNED_TWICE: &str = "one burst must warn once, not once per commit";
    const WAL_LATCH_STUCK: &str = "falling back to the retention limit must re-arm the warning";
    const WORKSPACE: &str = "workspace";
    const OTHER_WORKSPACE: &str = "other-workspace";
    const BLOCKED_PARENT: &str = "blocked-parent";
    const MONTH_START: &str = "2020-02-01T00:00:00";
    const PREVIOUS_MONTH: &str = "2020-01";
    const NEXT_MONTH: &str = "2020-02";

    fn conversation_permission() -> PermissionRuleRecord {
        PermissionRuleRecord::conversation(StructuredPermissionRule {
            subject: PermissionSubject::Native {
                owner: "workcell".into(),
                contract: "file.read.v1".into(),
            },
            executor: PermissionExecutorKind::Native,
            resources: Vec::new(),
            arguments: PermissionArgumentConstraint::Unconstrained,
            lifetime: PermissionLifetime::Conversation,
            effect: StructuredPermissionEffect::Allow,
            family: None,
        })
        .unwrap()
    }

    #[test]
    fn permission_lane_survives_snapshot_coalescing_and_acknowledges_durability() {
        let (_temp, dir) = state_dir();
        let (writer, _warnings) = writer(&dir);
        let mut session = AppSession::new(MODEL, CWD);
        crate::push_history_message(&mut session, user_message(0));
        writer.save_sync(Arc::new(session.clone())).unwrap();
        let database = SessionDatabase::open_state(&dir).unwrap();
        let owner = PermissionOwner::Conversation(session.id);
        let expected = database.permission_snapshot(owner.clone()).unwrap();
        let original = conversation_permission();
        let first = prepare_mutation(
            vec![expected.clone()],
            PermissionMutation::Create {
                destination: owner.clone(),
                records: Box::new([original.clone()]),
            },
        )
        .unwrap();
        let second = prepare_mutation(
            vec![expected],
            PermissionMutation::Create {
                destination: owner.clone(),
                records: Box::new([conversation_permission()]),
            },
        )
        .unwrap();
        let release = pause_writer(&writer);
        let first_result = writer.submit_permission_mutation(first.clone()).unwrap();
        let second_result = writer.submit_permission_mutation(second).unwrap();
        crate::push_history_message(&mut session, user_message(1));
        writer.send(Arc::new(session.clone()));
        crate::push_history_message(&mut session, user_message(2));
        writer.send(Arc::new(session.clone()));
        release.send(()).unwrap();
        let receipt = first_result.recv_timeout(DRAIN_TIMEOUT).unwrap().unwrap();
        assert!(matches!(
            second_result.recv_timeout(DRAIN_TIMEOUT).unwrap(),
            Err(PermissionMutationError::Conflict { .. })
        ));
        assert_eq!(
            database.permission_receipt(first.operation_id()).unwrap(),
            Some(receipt)
        );
        writer.save_sync(Arc::new(session.clone())).unwrap();
        let loaded: AppSession = database.load(session.id).unwrap();
        assert_eq!(message_texts(&loaded), message_texts(&session));
        assert_eq!(loaded.meta.structured_permission_rules, vec![original]);
        crate::push_history_message(&mut session, user_message(3));
        writer.save_sync(Arc::new(session.clone())).unwrap();
        let loaded: AppSession = database.load(session.id).unwrap();
        assert_eq!(loaded.meta.structured_permission_rules.len(), 1);
        assert_eq!(message_texts(&loaded), message_texts(&session));
        writer.shutdown_checked(DRAIN_TIMEOUT).unwrap();
    }

    #[test]
    fn permission_lane_is_bounded_and_lost_ack_is_queryable() {
        let (_temp, dir) = state_dir();
        let (writer, _warnings) = writer(&dir);
        let session = Arc::new(AppSession::new(MODEL, CWD));
        writer.save_sync(Arc::clone(&session)).unwrap();
        let database = SessionDatabase::open_state(&dir).unwrap();
        let owner = PermissionOwner::Conversation(session.id);
        let prepared = prepare_mutation(
            vec![database.permission_snapshot(owner.clone()).unwrap()],
            PermissionMutation::Create {
                destination: owner.clone(),
                records: Box::new([conversation_permission()]),
            },
        )
        .unwrap();
        let release = pause_writer(&writer);
        let mut results = Vec::new();
        for _ in 0..PERMISSION_QUEUE_CAPACITY {
            results.push(writer.submit_permission_mutation(prepared.clone()).unwrap());
        }
        let full = writer
            .submit_permission_mutation(prepared.clone())
            .unwrap_err();
        let PermissionMutationError::Session(error) = full else {
            panic!("{full}")
        };
        assert_writer_error(error, io::ErrorKind::WouldBlock, PERMISSION_QUEUE_FULL);
        drop(results.remove(0));
        release.send(()).unwrap();
        for result in results {
            result.recv_timeout(DRAIN_TIMEOUT).unwrap().unwrap();
        }
        assert!(
            database
                .permission_receipt(prepared.operation_id())
                .unwrap()
                .is_some()
        );
        assert_eq!(
            database.permission_snapshot(owner).unwrap().records.len(),
            1
        );
        writer.shutdown_checked(DRAIN_TIMEOUT).unwrap();
    }

    #[test]
    fn permission_coordinator_does_not_keep_a_shutdown_writer_alive() {
        let (_temp, dir) = state_dir();
        let (writer, _warnings) = writer(&dir);
        let session = Arc::new(AppSession::new(MODEL, CWD));
        writer.save_sync(Arc::clone(&session)).unwrap();
        let coordinator = writer.permission_mutation_writer();
        let database = SessionDatabase::open_state(&dir).unwrap();
        let owner = PermissionOwner::Conversation(session.id);
        let prepared = prepare_mutation(
            vec![database.permission_snapshot(owner.clone()).unwrap()],
            PermissionMutation::Create {
                destination: owner,
                records: Box::new([conversation_permission()]),
            },
        )
        .unwrap();
        writer.shutdown_checked(DRAIN_TIMEOUT).unwrap();
        let error = coordinator.submit(prepared.clone()).unwrap_err();
        let PermissionMutationError::Session(error) = error else {
            panic!("{error}")
        };
        assert_writer_error(error, io::ErrorKind::Other, WRITER_UNAVAILABLE);
        assert!(
            database
                .permission_receipt(prepared.operation_id())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn queued_permission_mutation_reports_conflict_when_its_session_is_deleted() {
        let (_temp, dir) = state_dir();
        let (warnings, _warnings) = flume::unbounded();
        let mut writer = bare_writer(&dir, warnings);
        let session = AppSession::new(MODEL, CWD);
        writer.write(&session).unwrap();
        let database = SessionDatabase::open_state(&dir).unwrap();
        let owner = PermissionOwner::Conversation(session.id);
        let prepared = prepare_mutation(
            vec![database.permission_snapshot(owner.clone()).unwrap()],
            PermissionMutation::Create {
                destination: owner.clone(),
                records: Box::new([conversation_permission()]),
            },
        )
        .unwrap();
        let (mutations, commands) = flume::bounded(PERMISSION_QUEUE_CAPACITY);
        writer.permission_mutations = commands;
        let (done, acknowledgment) = flume::bounded(1);
        assert!(
            mutations
                .send(PermissionWrite {
                    prepared: prepared.clone(),
                    done,
                })
                .is_ok()
        );
        let (deleted, deletion) = flume::bounded(1);
        let pending: Pending = Arc::default();
        lock(&pending).insert(
            session.id,
            Entry::Delete(Box::new(move |result| {
                deleted.send(result).unwrap();
            })),
        );
        writer.drain(&pending, &Arc::default(), &Arc::default(), &Arc::default());
        deletion.recv_timeout(DRAIN_TIMEOUT).unwrap().unwrap();
        assert!(matches!(
            acknowledgment.recv_timeout(DRAIN_TIMEOUT).unwrap(),
            Err(PermissionMutationError::Conflict { .. })
        ));
        assert!(
            !database
                .permission_snapshot(owner)
                .unwrap()
                .revision
                .row_present
        );
        assert!(
            database
                .permission_receipt(prepared.operation_id())
                .unwrap()
                .is_none()
        );
        writer
            .finish(&pending, &Arc::default(), &Arc::default(), &Arc::default())
            .unwrap();
    }

    #[test]
    fn historical_permission_acknowledgment_does_not_recreate_a_deleted_session() {
        let (_temp, dir) = state_dir();
        let (writer, _warnings) = writer(&dir);
        let session = Arc::new(AppSession::new(MODEL, CWD));
        writer.save_sync(Arc::clone(&session)).unwrap();
        let database = SessionDatabase::open_state(&dir).unwrap();
        let owner = PermissionOwner::Conversation(session.id);
        let prepared = prepare_mutation(
            vec![database.permission_snapshot(owner.clone()).unwrap()],
            PermissionMutation::Create {
                destination: owner.clone(),
                records: Box::new([conversation_permission()]),
            },
        )
        .unwrap();
        let receipt = writer
            .submit_permission_mutation(prepared.clone())
            .unwrap()
            .recv_timeout(DRAIN_TIMEOUT)
            .unwrap()
            .unwrap();
        writer.delete_sync(session.id).unwrap();
        let before = database.permission_snapshot(owner.clone()).unwrap();
        let generation = database.permission_generation().unwrap();
        assert_eq!(
            writer
                .submit_permission_mutation(prepared)
                .unwrap()
                .recv_timeout(DRAIN_TIMEOUT)
                .unwrap()
                .unwrap(),
            receipt
        );
        assert_eq!(database.permission_generation().unwrap(), generation);
        assert_eq!(database.permission_snapshot(owner).unwrap(), before);
        assert!(!before.revision.row_present);
        writer.shutdown_checked(DRAIN_TIMEOUT).unwrap();
    }

    #[test]
    fn permission_revocation_survives_writer_delete_and_stale_recreation() {
        let (_temp, dir) = state_dir();
        let (writer, _warnings) = writer(&dir);
        let mut session = AppSession::new(MODEL, CWD);
        crate::push_history_message(&mut session, user_message(0));
        writer.save_sync(Arc::new(session.clone())).unwrap();
        let database = SessionDatabase::open_state(&dir).unwrap();
        let owner = PermissionOwner::Conversation(session.id);
        let original = conversation_permission();
        let approval = prepare_mutation(
            vec![database.permission_snapshot(owner.clone()).unwrap()],
            PermissionMutation::Create {
                destination: owner.clone(),
                records: Box::new([original.clone()]),
            },
        )
        .unwrap();
        writer
            .submit_permission_mutation(approval)
            .unwrap()
            .recv_timeout(DRAIN_TIMEOUT)
            .unwrap()
            .unwrap();
        let approved = database.permission_snapshot(owner.clone()).unwrap();
        approved
            .apply_to_meta(session.id, &mut session.meta)
            .unwrap();
        let revoke = prepare_mutation(
            vec![approved],
            PermissionMutation::Revoke {
                source: PermissionRecordIdentity {
                    owner: owner.clone(),
                    record_id: original.id,
                },
            },
        )
        .unwrap();
        writer
            .submit_permission_mutation(revoke)
            .unwrap()
            .recv_timeout(DRAIN_TIMEOUT)
            .unwrap()
            .unwrap();
        let revoked = database.permission_snapshot(owner.clone()).unwrap();
        writer.delete_sync(session.id).unwrap();
        crate::push_history_message(&mut session, user_message(1));
        writer.save_sync(Arc::new(session.clone())).unwrap();
        let current = database.permission_snapshot(owner).unwrap();
        assert_eq!(current.records, revoked.records);
        assert!(current.records.iter().all(|record| !record.is_active()));
        assert_ne!(current.revision.lineage, revoked.revision.lineage);
        assert!(current.revision.generation > revoked.revision.generation);
        let loaded: AppSession = database.load(session.id).unwrap();
        assert_eq!(message_texts(&loaded), message_texts(&session));
        writer.shutdown_checked(DRAIN_TIMEOUT).unwrap();
    }

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
            tool_ledger: None,
            cursors: HashMap::new(),
            deleted_sessions: HashMap::new(),
            failing: HashSet::new(),
            workspace_tabs_errors: HashMap::new(),
            size_warning_level: 0,
            wal_warning_active: false,
            commits_since_checkpoint: 0,
            checkpoint_stalls: 0,
            generation: Arc::default(),
            permission_mutations: flume::bounded(PERMISSION_QUEUE_CAPACITY).1,
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

    fn pause_writer(writer: &StorageWriter) -> flume::Sender<()> {
        let (entered_tx, entered_rx) = flume::bounded(1);
        let (release_tx, release_rx) = flume::bounded(1);
        writer.delete(CaudraId::generate(), move |result| {
            result.unwrap();
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(DRAIN_TIMEOUT).unwrap();
        });
        entered_rx.recv_timeout(DRAIN_TIMEOUT).unwrap();
        release_tx
    }

    fn assert_writer_error(error: SessionError, kind: io::ErrorKind, message: &str) {
        let SessionError::Storage(StorageError::Io(error)) = error else {
            panic!("unexpected writer error: {error}");
        };
        assert_eq!(error.kind(), kind);
        assert_eq!(error.to_string(), message);
    }

    #[test]
    fn save_sync_timeout_does_not_cancel_the_write() {
        let (_tmp, dir) = state_dir();
        let (writer, _warn_rx) = writer(&dir);
        let release = pause_writer(&writer);
        let session = Arc::new(AppSession::new(MODEL, CWD));
        let id = session.id;

        let error = writer
            .save_sync_timeout(session, Duration::ZERO)
            .unwrap_err();

        assert_writer_error(error, io::ErrorKind::TimedOut, WRITER_TIMEOUT);
        assert!(matches!(
            lock(&writer.pending).get(&id),
            Some(Entry::SaveSync(..))
        ));
        release.send(()).unwrap();
        writer.shutdown_checked(DRAIN_TIMEOUT).unwrap();
        assert!(AppSession::load(id, &dir).is_ok());
    }

    #[test_case(false; "legacy_save")]
    #[test_case(true; "timed_save")]
    fn disconnected_save_resolves_its_callback(timed: bool) {
        let (_tmp, dir) = state_dir();
        let (mut writer, _warn_rx) = writer(&dir);
        let release = pause_writer(&writer);
        let (wake, wake_rx) = flume::bounded(1);
        drop(wake_rx);
        writer.wake = Arc::new(wake);
        let session = Arc::new(AppSession::new(MODEL, CWD));

        let error = if timed {
            writer.save_sync_timeout(session, DRAIN_TIMEOUT)
        } else {
            writer.save_sync(session)
        }
        .unwrap_err();

        assert_writer_error(error, io::ErrorKind::Other, WRITER_UNAVAILABLE);
        assert!(lock(&writer.pending).is_empty());
        release.send(()).unwrap();
        writer.shutdown_checked(DRAIN_TIMEOUT).unwrap();
    }

    #[test]
    fn checked_shutdown_times_out_without_claiming_the_writer_stopped() {
        let (_tmp, dir) = state_dir();
        let (writer, _warn_rx) = writer(&dir);
        let release = pause_writer(&writer);
        let done_rx = writer.done_rx.clone();

        let error = writer.shutdown_checked(Duration::ZERO).unwrap_err();

        assert_writer_error(error, io::ErrorKind::TimedOut, WRITER_TIMEOUT);
        release.send(()).unwrap();
        done_rx.recv_timeout(DRAIN_TIMEOUT).unwrap().unwrap();
    }

    #[test]
    fn checked_shutdown_rejects_a_disconnected_completion_channel() {
        let (_tmp, dir) = state_dir();
        let (mut writer, _warn_rx) = writer(&dir);
        let done_rx = writer.done_rx.clone();
        let (done_tx, disconnected_rx) = flume::bounded(1);
        drop(done_tx);
        writer.done_rx = disconnected_rx;

        let error = writer.shutdown_checked(DRAIN_TIMEOUT).unwrap_err();

        assert_writer_error(error, io::ErrorKind::Other, WRITER_UNAVAILABLE);
        done_rx.recv_timeout(DRAIN_TIMEOUT).unwrap().unwrap();
    }

    #[test_case(false; "pending_retryable_save")]
    #[test_case(true; "failed_save_without_pending_retry")]
    fn checked_shutdown_reports_session_persistence_failure(synchronous: bool) {
        let (_tmp, dir) = state_dir();
        block_session_database(&dir);
        let (writer, _warn_rx) = writer(&dir);
        let session = Arc::new(AppSession::new(MODEL, CWD));
        let pending = Arc::clone(&writer.pending);
        let (done_tx, done_rx) = flume::bounded(1);
        let entry = if synchronous {
            Entry::SaveSync(Arc::clone(&session), done_tx)
        } else {
            Entry::Save(Arc::clone(&session))
        };
        lock(&pending).insert(session.id, entry);

        let error = writer.shutdown_checked(DRAIN_TIMEOUT).unwrap_err();

        assert_writer_error(error, io::ErrorKind::Other, WRITER_DRAIN_FAILED);
        assert_eq!(lock(&pending).is_empty(), synchronous);
        if synchronous {
            assert!(done_rx.recv_timeout(DRAIN_TIMEOUT).unwrap().is_err());
        }
        assert_eq!(Arc::strong_count(&pending), 1);
    }

    #[test]
    fn checked_shutdown_resolves_failed_delete_callbacks() {
        let (_tmp, dir) = state_dir();
        block_session_database(&dir);
        let (writer, _warn_rx) = writer(&dir);
        let (done_tx, done_rx) = flume::bounded(1);
        lock(&writer.pending).insert(
            CaudraId::generate(),
            Entry::Delete(Box::new(move |result| done_tx.send(result).unwrap())),
        );

        let error = writer.shutdown_checked(DRAIN_TIMEOUT).unwrap_err();

        assert_writer_error(error, io::ErrorKind::Other, WRITER_DRAIN_FAILED);
        assert!(done_rx.recv_timeout(DRAIN_TIMEOUT).unwrap().is_err());
        assert!(done_rx.try_recv().is_err());
    }

    #[test_case(false; "final_tab_write")]
    #[test_case(true; "earlier_tab_failure")]
    fn checked_shutdown_reports_workspace_tabs_failure(before_shutdown: bool) {
        let (tmp, dir) = state_dir();
        let (writer, warn_rx) = writer(&dir);
        let parent = tmp.path().join(BLOCKED_PARENT);
        fs::write(&parent, []).unwrap();
        let cwd = parent.join(WORKSPACE);
        if before_shutdown {
            writer.persist_workspace_tabs(cwd, WorkspaceTabs::default());
            let warning = warn_rx.recv_timeout(DRAIN_TIMEOUT).unwrap();
            assert!(warning.starts_with(WORKSPACE_TABS_SAVE_FAILED_PREFIX));
        } else {
            *writer.workspace_tabs.lock().unwrap() = Some(WorkspaceTabsRequest {
                cwd,
                tabs: WorkspaceTabs::default(),
            });
        }

        let error = writer.shutdown_checked(DRAIN_TIMEOUT).unwrap_err();

        assert!(matches!(
            error,
            SessionError::Storage(StorageError::Io(error)) if error.kind() == io::ErrorKind::NotADirectory
        ));
    }

    #[test_case(false; "same_workspace_recovers")]
    #[test_case(true; "other_workspace_does_not_clear_failure")]
    fn workspace_tabs_failure_clears_only_after_that_workspace_is_saved(other_workspace: bool) {
        let (tmp, dir) = state_dir();
        let (warn_tx, _warn_rx) = flume::unbounded();
        let mut writer = bare_writer(&dir, warn_tx);
        let parent = tmp.path().join(BLOCKED_PARENT);
        fs::write(&parent, []).unwrap();
        let cwd = parent.join(WORKSPACE);
        writer.flush_workspace_tabs(WorkspaceTabsRequest {
            cwd: cwd.clone(),
            tabs: WorkspaceTabs::default(),
        });
        let saved_cwd = if other_workspace {
            tmp.path().join(OTHER_WORKSPACE)
        } else {
            cwd
        };
        fs::remove_file(&parent).unwrap();
        fs::create_dir_all(&saved_cwd).unwrap();
        writer.flush_workspace_tabs(WorkspaceTabsRequest {
            cwd: saved_cwd,
            tabs: WorkspaceTabs::default(),
        });

        let result = writer.finish(
            &Arc::default(),
            &Arc::default(),
            &Arc::default(),
            &Arc::default(),
        );

        assert_eq!(result.is_err(), other_workspace);
    }

    #[test]
    fn checked_shutdown_drains_callbacks_and_tabs_and_joins_the_writer() {
        let (tmp, dir) = state_dir();
        let cwd = tmp.path().join(WORKSPACE);
        fs::create_dir(&cwd).unwrap();
        let (writer, _warn_rx) = writer(&dir);
        let release = pause_writer(&writer);
        let pending = Arc::clone(&writer.pending);
        let tabs = Arc::clone(&writer.workspace_tabs);
        let session = Arc::new(AppSession::new(MODEL, &cwd.to_string_lossy()));
        let id = session.id;
        let (save_tx, save_rx) = flume::bounded(1);
        let (delete_tx, delete_rx) = flume::bounded(1);
        lock(&pending).insert(id, Entry::SaveSync(session, save_tx));
        lock(&pending).insert(
            CaudraId::generate(),
            Entry::Delete(Box::new(move |result| delete_tx.send(result).unwrap())),
        );
        let expected_tabs = WorkspaceTabs {
            open: vec![id],
            focused: Some(id),
        };
        *tabs.lock().unwrap() = Some(WorkspaceTabsRequest {
            cwd: cwd.clone(),
            tabs: expected_tabs.clone(),
        });
        release.send(()).unwrap();

        writer.shutdown_checked(DRAIN_TIMEOUT).unwrap();

        save_rx.recv_timeout(DRAIN_TIMEOUT).unwrap().unwrap();
        delete_rx.recv_timeout(DRAIN_TIMEOUT).unwrap().unwrap();
        assert!(AppSession::load(id, &dir).is_ok());
        assert_eq!(
            caudra_storage::state::read_workspace_tabs(&dir, &cwd).unwrap(),
            Some(expected_tabs)
        );
        assert!(lock(&pending).is_empty());
        assert!(tabs.lock().unwrap().is_none());
        assert_eq!(Arc::strong_count(&pending), 1);
        assert_eq!(Arc::strong_count(&tabs), 1);
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

    #[test_case(false; "missing_workspace")]
    #[test_case(true; "renamed_workspace")]
    fn checked_shutdown_saves_missing_workspace_tabs_with_exact_path_matching(renamed: bool) {
        let (tmp, dir) = state_dir();
        let cwd = tmp.path().join(WORKSPACE);
        let destination = tmp.path().join(OTHER_WORKSPACE);
        if renamed {
            fs::create_dir(&cwd).unwrap();
        }
        let (writer, warn_rx) = writer(&dir);
        let kept = Arc::new(AppSession::new(MODEL, &cwd.to_string_lossy()));
        let kept_id = kept.id;
        writer.save_sync(kept).unwrap();
        let mut open = vec![kept_id];
        for path in [cwd.join(OTHER_WORKSPACE), destination.clone()] {
            let session = Arc::new(AppSession::new(MODEL, &path.to_string_lossy()));
            open.push(session.id);
            writer.save_sync(session).unwrap();
        }
        open.extend([kept_id, CaudraId::generate()]);
        if renamed {
            fs::rename(&cwd, &destination).unwrap();
        }
        writer.persist_workspace_tabs(
            cwd.clone(),
            WorkspaceTabs {
                open,
                focused: Some(kept_id),
            },
        );

        writer.shutdown_checked(DRAIN_TIMEOUT).unwrap();

        assert_eq!(
            caudra_storage::state::read_workspace_tabs(&dir, &cwd).unwrap(),
            Some(WorkspaceTabs {
                open: vec![kept_id],
                focused: Some(kept_id),
            })
        );
        assert_eq!(
            caudra_storage::state::read_workspace_tabs(&dir, &destination).unwrap(),
            None
        );
        assert!(warn_rx.is_empty());
    }

    #[test_case(false; "existing_workspace")]
    #[test_case(true; "missing_workspace")]
    fn checked_shutdown_reports_stored_workspace_path_errors(missing: bool) {
        let (tmp, dir) = state_dir();
        let cwd = tmp.path().join(WORKSPACE);
        if !missing {
            fs::create_dir(&cwd).unwrap();
        }
        let parent = tmp.path().join(BLOCKED_PARENT);
        fs::write(&parent, []).unwrap();
        let stored_cwd = parent.join(OTHER_WORKSPACE);
        let (writer, warn_rx) = writer(&dir);
        let session = Arc::new(AppSession::new(MODEL, &stored_cwd.to_string_lossy()));
        writer.save_sync(session).unwrap();
        writer.persist_workspace_tabs(cwd.clone(), WorkspaceTabs::default());

        let error = writer.shutdown_checked(DRAIN_TIMEOUT).unwrap_err();

        assert!(matches!(
            error,
            SessionError::Storage(StorageError::Io(error)) if error.kind() == io::ErrorKind::NotADirectory
        ));
        assert!(
            warn_rx
                .recv_timeout(DRAIN_TIMEOUT)
                .unwrap()
                .starts_with(WORKSPACE_TABS_SAVE_FAILED_PREFIX)
        );
        assert_eq!(
            caudra_storage::state::read_workspace_tabs(&dir, &cwd).unwrap(),
            None
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

    fn queued_spend(cost: Option<f64>, enqueued_at: u64) -> QueuedUsage {
        QueuedUsage {
            turn: spend(MODEL, cost),
            enqueued_at,
        }
    }

    #[test_case(false; "normal_drain")]
    #[test_case(true; "final_drain")]
    fn unavailable_usage_ledger_retains_every_turn_until_recovery(final_drain: bool) {
        let (_tmp, dir) = state_dir();
        block_session_database(&dir);
        let (warn_tx, _warn_rx) = flume::unbounded();
        let mut writer = bare_writer(&dir, warn_tx);
        let turns = vec![queued_spend(Some(1.0), 0), queued_spend(Some(2.0), 0)];
        let usage: PendingUsage = Arc::new(Mutex::new(turns.clone()));

        writer.drain(&Arc::default(), &Arc::default(), &usage, &Arc::default());
        assert_eq!(*usage.lock().unwrap(), turns);
        assert!(writer.ledger.is_none());
        let error = writer
            .finish(&Arc::default(), &Arc::default(), &usage, &Arc::default())
            .unwrap_err();
        assert_writer_error(error, io::ErrorKind::Other, USAGE_DRAIN_FAILED);
        assert_eq!(*usage.lock().unwrap(), turns);

        fs::remove_dir(dir.path().join(SESSIONS_DB_FILE)).unwrap();
        usage.lock().unwrap().push(queued_spend(None, 0));
        if !final_drain {
            writer.drain(&Arc::default(), &Arc::default(), &usage, &Arc::default());
            assert!(usage.lock().unwrap().is_empty());
        }
        writer
            .finish(&Arc::default(), &Arc::default(), &usage, &Arc::default())
            .unwrap();
        assert!(usage.lock().unwrap().is_empty());
        let total = UsageLedger::open(&dir).unwrap().lifetime().unwrap();
        assert_eq!(total.cost, 3.0);
        assert_eq!(total.input, 3);
        assert_eq!(total.output, 6);
        assert_eq!(total.priced_turns, 2);
        assert_eq!(total.unpriced_turns, 1);
    }

    #[test_case(0; "first_write_fails")]
    #[test_case(1; "middle_write_fails")]
    #[test_case(2; "last_write_fails")]
    fn usage_retry_does_not_replay_committed_turns(failed_index: usize) {
        let (_tmp, dir) = state_dir();
        let (warn_tx, _warn_rx) = flume::unbounded();
        let mut writer = bare_writer(&dir, warn_tx);
        let mut turns = vec![queued_spend(Some(1.0), 0), queued_spend(Some(2.0), 0)];
        turns.insert(failed_index, queued_spend(Some(f64::NAN), 0));
        let usage: PendingUsage = Arc::new(Mutex::new(turns));

        writer.flush_usage(&usage);

        assert_eq!(usage.lock().unwrap().len(), 1);
        assert!(usage.lock().unwrap()[0].turn.cost.unwrap().is_nan());
        let error = writer
            .finish(&Arc::default(), &Arc::default(), &usage, &Arc::default())
            .unwrap_err();
        assert_writer_error(error, io::ErrorKind::Other, USAGE_DRAIN_FAILED);
        let total = UsageLedger::open(&dir).unwrap().lifetime().unwrap();
        assert_eq!(total.cost, 3.0);
        assert_eq!(total.priced_turns, 2);
        assert_eq!(total.input, 2);

        usage.lock().unwrap()[0].turn.cost = Some(3.0);
        writer.flush_usage(&usage);
        writer
            .finish(&Arc::default(), &Arc::default(), &usage, &Arc::default())
            .unwrap();

        assert!(usage.lock().unwrap().is_empty());
        let total = UsageLedger::open(&dir).unwrap().lifetime().unwrap();
        assert_eq!(total.cost, 6.0);
        assert_eq!(total.priced_turns, 3);
        assert_eq!(total.input, 3);
        assert_eq!(total.output, 6);
    }

    #[test_case(false; "ledger_open_failure")]
    #[test_case(true; "ledger_write_failure")]
    fn usage_retry_preserves_enqueue_hour_and_month(write_failure: bool) {
        let month_start = MONTH_START
            .parse::<DateTime>()
            .unwrap()
            .to_zoned(TimeZone::system())
            .unwrap()
            .timestamp()
            .as_second() as u64;
        let boundary = bucket_for(month_start + BUCKET_SECONDS - 1) as u64;
        let enqueued_at = boundary - 1;
        let (_tmp, dir) = state_dir();
        if !write_failure {
            block_session_database(&dir);
        }
        let (warn_tx, _warn_rx) = flume::unbounded();
        let mut writer = bare_writer(&dir, warn_tx);
        let usage: PendingUsage = Arc::new(Mutex::new(vec![queued_spend(
            Some(if write_failure { f64::NAN } else { 1.0 }),
            enqueued_at,
        )]));

        writer.flush_usage(&usage);
        assert_eq!(usage.lock().unwrap().len(), 1);
        assert_eq!(usage.lock().unwrap()[0].enqueued_at, enqueued_at);
        let error = writer
            .finish(&Arc::default(), &Arc::default(), &usage, &Arc::default())
            .unwrap_err();
        assert_writer_error(error, io::ErrorKind::Other, USAGE_DRAIN_FAILED);
        assert_eq!(usage.lock().unwrap()[0].enqueued_at, enqueued_at);

        if write_failure {
            usage.lock().unwrap()[0].turn.cost = Some(1.0);
        } else {
            fs::remove_dir(dir.path().join(SESSIONS_DB_FILE)).unwrap();
        }
        usage
            .lock()
            .unwrap()
            .push(queued_spend(Some(2.0), boundary));
        writer.flush_usage(&usage);
        writer
            .finish(&Arc::default(), &Arc::default(), &usage, &Arc::default())
            .unwrap();

        assert!(usage.lock().unwrap().is_empty());
        let ledger = UsageLedger::open(&dir).unwrap();
        let buckets = ledger.buckets(None).unwrap();
        assert_eq!(buckets.len(), 2);
        for (timestamp, cost) in [(enqueued_at, 1.0), (boundary, 2.0)] {
            let bucket = buckets
                .iter()
                .find(|bucket| bucket.bucket_start == bucket_for(timestamp))
                .unwrap();
            assert_eq!(bucket.cost, cost);
            assert_eq!(bucket.priced_turns, 1);
        }
        let total = ledger.lifetime().unwrap();
        assert_eq!(total.by_month.len(), 2);
        for (month, cost) in [(PREVIOUS_MONTH, 1.0), (NEXT_MONTH, 2.0)] {
            let slice = total
                .by_month
                .iter()
                .find(|slice| slice.label == month)
                .unwrap();
            assert_eq!(slice.cost, cost);
            assert_eq!(slice.turns, 1);
        }
    }

    #[test_case(false; "ledger_open_failure")]
    #[test_case(true; "ledger_write_failure")]
    fn checked_shutdown_reports_unsaved_usage(write_failure: bool) {
        let (_tmp, dir) = state_dir();
        if !write_failure {
            block_session_database(&dir);
        }
        let (writer, _warn_rx) = writer(&dir);
        let usage = Arc::clone(&writer.usage);
        writer.record_usage(spend(
            MODEL,
            Some(if write_failure { f64::NAN } else { 1.0 }),
        ));
        if write_failure {
            writer
                .save_sync(Arc::new(AppSession::new(MODEL, CWD)))
                .unwrap();
        }

        let error = writer.shutdown_checked(DRAIN_TIMEOUT).unwrap_err();

        assert_writer_error(error, io::ErrorKind::Other, USAGE_DRAIN_FAILED);
        assert_eq!(usage.lock().unwrap().len(), 1);
        assert_eq!(Arc::strong_count(&usage), 1);
    }

    #[test]
    fn queued_usage_is_drained_before_relocation_and_merged_once() {
        let (_tmp, dir) = state_dir();
        let (writer, _warn_rx) = writer(&dir);
        let session = Arc::new(AppSession::new(MODEL, CWD));
        writer.save_sync(Arc::clone(&session)).unwrap();
        let release = pause_writer(&writer);
        writer.record_usage(spend(MODEL, Some(1.0)));
        writer.record_usage(spend(MODEL, Some(2.0)));
        let mut destination_turn = spend(MODEL, Some(3.0));
        destination_turn.cwd = RELOCATION_DESTINATION.into();
        writer.record_usage(destination_turn);
        assert_eq!(writer.usage.lock().unwrap().len(), 3);
        release.send(()).unwrap();
        writer.shutdown_checked(DRAIN_TIMEOUT).unwrap();

        let ledger = UsageLedger::open(&dir).unwrap();
        let before = ledger.lifetime().unwrap();
        assert_eq!(before.cost, 6.0);
        assert_eq!(before.priced_turns, 3);
        let mut database = SessionDatabase::open(&dir).unwrap();
        let result = database
            .relocate_sessions(&SessionRelocation {
                sessions: database.local_session_locations().unwrap(),
                source_cwd: Some(CWD.into()),
                destination: RELOCATION_DESTINATION.into(),
                include_project_usage: true,
            })
            .unwrap();

        assert_eq!(result.sessions_moved, 1);
        assert!(result.project_usage.unwrap().buckets_moved > 0);
        assert_eq!(
            AppSession::load(session.id, &dir).unwrap().cwd,
            RELOCATION_DESTINATION
        );
        assert!(
            ledger
                .buckets(None)
                .unwrap()
                .iter()
                .all(|bucket| bucket.cwd == RELOCATION_DESTINATION)
        );
        let after = ledger.lifetime().unwrap();
        assert_eq!(after.cost, before.cost);
        assert_eq!(after.input, before.input);
        assert_eq!(after.output, before.output);
        assert_eq!(after.priced_turns, before.priced_turns);
        let repeated = database
            .relocate_project_usage(CWD, RELOCATION_DESTINATION)
            .unwrap();
        assert_eq!(repeated.buckets_moved, 0);
        assert_eq!(ledger.lifetime().unwrap(), after);
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

    #[test_case(false; "legacy_save")]
    #[test_case(true; "timed_save")]
    fn synchronous_save_is_visible_before_it_returns(timed: bool) {
        let (_tmp, dir) = state_dir();
        let (writer, _warn_rx) = writer(&dir);
        let mut session = AppSession::new(MODEL, CWD);
        crate::push_history_message(&mut session, user_message(0));
        let id = session.id;

        if timed {
            writer.save_sync_timeout(Arc::new(session), DRAIN_TIMEOUT)
        } else {
            writer.save_sync(Arc::new(session))
        }
        .unwrap();

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
    #[test_case(false; "legacy_shutdown")]
    #[test_case(true; "checked_shutdown")]
    fn failed_write_is_retried_by_a_later_flush(checked: bool) {
        let (_tmp, dir) = state_dir();
        block_session_database(&dir);
        let (writer, warn_rx) = writer(&dir);
        let session = Arc::new(AppSession::new(MODEL, CWD));
        let id = session.id;

        writer.send(session);
        let warning = warn_rx.recv_timeout(DRAIN_TIMEOUT).unwrap();
        assert!(warning.starts_with(SAVE_FAILED_PREFIX), "{warning}");

        std::fs::remove_dir(dir.path().join(SESSIONS_DB_FILE)).unwrap();
        if checked {
            writer.shutdown_checked(DRAIN_TIMEOUT).unwrap();
        } else {
            writer.shutdown(DRAIN_TIMEOUT);
        }

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

    /// `journal_size_limit` leaves the file at the retention limit, so a latch
    /// that cleared below it would arm once and never again.
    #[test]
    fn the_wal_warning_clears_where_the_retention_limit_leaves_the_file() {
        let (_tmp, dir) = state_dir();
        let (warn_tx, _warn_rx) = flume::unbounded();
        let mut writer = bare_writer(&dir, warn_tx);

        assert!(
            !writer.wal_outgrew_its_limit(WAL_RETENTION_LIMIT_BYTES),
            "{WAL_AT_LIMIT_WARNED}"
        );
        assert!(
            writer.wal_outgrew_its_limit(WAL_WARNING_BYTES),
            "{WAL_GROWTH_SILENT}"
        );
        assert!(
            !writer.wal_outgrew_its_limit(WAL_WARNING_BYTES),
            "{WAL_WARNED_TWICE}"
        );
        assert!(
            !writer.wal_outgrew_its_limit(WAL_RETENTION_LIMIT_BYTES),
            "{WAL_AT_LIMIT_WARNED}"
        );
        assert!(
            writer.wal_outgrew_its_limit(WAL_WARNING_BYTES),
            "{WAL_LATCH_STUCK}"
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
