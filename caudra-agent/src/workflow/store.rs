//! One session's workflow persistence, behind a dedicated thread. The
//! thread owns its own `SessionDatabase` connection and drains commands in
//! order, so every awaiting caller sees writes land in the sequence they were
//! issued and the async executor never blocks on SQLite.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use caudra_storage::StateDir;
use caudra_storage::id::CaudraId;
use caudra_storage::sessions::SessionDatabase;
use caudra_storage::workflow::{
    WorkflowCallFinish, WorkflowCallRow, WorkflowCallStart, WorkflowEventKind, WorkflowEventRow,
    WorkflowHistoryRow, WorkflowRunPatch, WorkflowRunRow, WorkflowUpdate,
};
use caudra_storage::workflow_scratch::{ScratchDir, remove_run};
use caudra_workflow::WorkflowError;
use tracing::warn;

const THREAD_NAME: &str = "workflow-store";

type Job = Box<dyn FnOnce(&Worker) + Send>;

enum Command {
    Run(Job),
    Shutdown,
}

struct Worker {
    database: SessionDatabase,
    state_dir: StateDir,
    session_id: CaudraId,
}

/// Cloneable handle to the storage thread. Dropping every handle without
/// [`Self::shutdown`] still ends the thread once its queue drains.
#[derive(Clone)]
pub struct WorkflowStore {
    commands: flume::Sender<Command>,
    thread: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl WorkflowStore {
    pub fn spawn(state_dir: StateDir, session_id: CaudraId) -> Result<Self, WorkflowError> {
        let database = SessionDatabase::open(&state_dir).map_err(storage)?;
        let worker = Worker {
            database,
            state_dir,
            session_id,
        };
        let (commands, queue) = flume::unbounded();
        let thread = thread::Builder::new()
            .name(THREAD_NAME.to_owned())
            .spawn(move || {
                for command in queue.iter() {
                    match command {
                        Command::Run(job) => job(&worker),
                        Command::Shutdown => break,
                    }
                }
            })
            .map_err(storage)?;
        Ok(Self {
            commands,
            thread: Arc::new(Mutex::new(Some(thread))),
        })
    }

    pub async fn insert_run(&self, row: WorkflowRunRow) -> Result<(), WorkflowError> {
        self.call(move |worker| worker.database.insert_workflow_run(&row).map_err(storage))
            .await
    }

    pub async fn update_run(
        &self,
        run_id: String,
        expected_revision: u64,
        expected_epoch: u64,
        patch: WorkflowRunPatch,
    ) -> Result<WorkflowUpdate, WorkflowError> {
        self.call(move |worker| {
            worker
                .database
                .update_workflow_run(&run_id, expected_revision, expected_epoch, &patch)
                .map_err(storage)
        })
        .await
    }

    pub async fn load_runs(&self) -> Result<Vec<WorkflowRunRow>, WorkflowError> {
        self.call(|worker| {
            worker
                .database
                .load_workflow_runs(worker.session_id)
                .map_err(storage)
        })
        .await
    }

    /// Recent runs of every other session, newest first.
    pub async fn load_history(
        &self,
        limit: usize,
    ) -> Result<Vec<WorkflowHistoryRow>, WorkflowError> {
        self.call(move |worker| {
            worker
                .database
                .load_workflow_history(limit)
                .map(|rows| {
                    rows.into_iter()
                        .filter(|row| row.run.session_id != worker.session_id)
                        .collect()
                })
                .map_err(storage)
        })
        .await
    }

    pub async fn append_event(
        &self,
        run_id: String,
        kind: WorkflowEventKind,
        text: String,
    ) -> Result<u64, WorkflowError> {
        self.call(move |worker| {
            worker
                .database
                .append_workflow_event(&run_id, kind, &text)
                .map_err(storage)
        })
        .await
    }

    pub async fn load_events(
        &self,
        run_id: String,
    ) -> Result<Vec<WorkflowEventRow>, WorkflowError> {
        self.call(move |worker| {
            worker
                .database
                .load_workflow_events(&run_id)
                .map_err(storage)
        })
        .await
    }

    pub async fn load_run(&self, run_id: String) -> Result<Option<WorkflowRunRow>, WorkflowError> {
        self.call(move |worker| worker.database.load_workflow_run(&run_id).map_err(storage))
            .await
    }

    /// Ends every run the session still has active; returns how many.
    pub async fn interrupt_active(&self) -> Result<u64, WorkflowError> {
        self.call(|worker| {
            worker
                .database
                .interrupt_active_workflow_runs(worker.session_id)
                .map_err(storage)
        })
        .await
    }

    pub async fn start_call(&self, start: WorkflowCallStart) -> Result<(), WorkflowError> {
        self.call(move |worker| worker.database.start_workflow_call(&start).map_err(storage))
            .await
    }

    pub async fn finish_call(
        &self,
        run_id: String,
        call_key: u64,
        finish: WorkflowCallFinish,
    ) -> Result<(), WorkflowError> {
        self.call(move |worker| {
            worker
                .database
                .finish_workflow_call(&run_id, call_key, &finish)
                .map_err(storage)
        })
        .await
    }

    pub async fn load_call(
        &self,
        run_id: String,
        call_key: u64,
    ) -> Result<Option<WorkflowCallRow>, WorkflowError> {
        self.call(move |worker| {
            worker
                .database
                .load_workflow_call(&run_id, call_key)
                .map_err(storage)
        })
        .await
    }

    pub async fn load_calls(&self, run_id: String) -> Result<Vec<WorkflowCallRow>, WorkflowError> {
        self.call(move |worker| {
            worker
                .database
                .load_workflow_calls(&run_id)
                .map_err(storage)
        })
        .await
    }

    /// `(run_id, revision)` pairs whose latest state is still undelivered.
    pub async fn pending_outbox(&self) -> Result<Vec<(String, u64)>, WorkflowError> {
        self.call(|worker| {
            worker
                .database
                .pending_workflow_outbox(worker.session_id)
                .map_err(storage)
        })
        .await
    }

    pub async fn ack_outbox(&self, run_id: String, revision: u64) -> Result<bool, WorkflowError> {
        self.call(move |worker| {
            worker
                .database
                .ack_workflow_outbox(&run_id, revision)
                .map_err(storage)
        })
        .await
    }

    pub async fn write_scratch(
        &self,
        run_id: String,
        name: String,
        content: String,
    ) -> Result<PathBuf, WorkflowError> {
        self.call(move |worker| {
            ScratchDir::open(&worker.state_dir, worker.session_id, &run_id, true)
                .map_err(storage)?
                .write(&name, content.as_bytes())
                .map_err(storage)
        })
        .await
    }

    pub async fn remove_scratch(&self, run_id: String) -> Result<(), WorkflowError> {
        self.call(move |worker| {
            remove_run(&worker.state_dir, worker.session_id, &run_id).map_err(storage)
        })
        .await
    }

    /// Lets queued commands finish, then stops the thread and waits for it.
    pub async fn shutdown(self) {
        let _ = self.commands.send(Command::Shutdown);
        let thread = self
            .thread
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(thread) = thread
            && let Err(error) = smol::unblock(move || thread.join()).await
        {
            warn!(?error, "workflow store thread panicked");
        }
    }

    async fn call<T, F>(&self, job: F) -> Result<T, WorkflowError>
    where
        T: Send + 'static,
        F: FnOnce(&Worker) -> Result<T, WorkflowError> + Send + 'static,
    {
        let (reply, response) = flume::bounded(1);
        let job: Job = Box::new(move |worker| {
            let _ = reply.send(job(worker));
        });
        self.commands
            .send(Command::Run(job))
            .map_err(|_| WorkflowError::Unavailable)?;
        response
            .recv_async()
            .await
            .map_err(|_| WorkflowError::Unavailable)?
    }
}

fn storage(error: impl std::fmt::Display) -> WorkflowError {
    WorkflowError::Storage(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use caudra_storage::workflow::{
        WORKFLOW_CALL_NOT_STARTED, WorkflowCallKind, WorkflowCallState, WorkflowRunStatus,
        WorkflowSourceKind,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::StoredSession;

    const CWD: &str = "/project";
    const MODEL: &str = "test/model";
    const RUN_ID: &str = "run-1";
    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const REQUEST: &str = r#"{"prompt":"do it"}"#;
    const REQUEST_HASH: &str = "hash-1";
    const RESULT: &str = r#"{"text":"done"}"#;
    const SCRATCH_NAME: &str = "notes.md";
    const SCRATCH_CONTENT: &str = "first draft";
    const STALE_LOSES: &str = "a stale writer must not overwrite newer state";
    const CALLS_ARE_ORDERED: &str = "a journal must load in call order";
    const ONE_CALL_LOADS_ALONE: &str = "a single call must load by its key";
    const UNKNOWN_CALL_IS_ABSENT: &str = "a key the run never recorded must read as absent";
    const OUTBOX_ACK_IS_EXACT: &str = "an ack must only clear the revision it delivered";
    const FAILURES_ARE_REPLIES: &str = "a failing command must answer with its error, not die";
    const CLOSED_IS_UNAVAILABLE: &str = "a stopped store must refuse rather than hang";
    const CONNECTIONS_SHARE_ONE_DATABASE: &str =
        "a second connection in the same process must see the actor's writes";

    fn open() -> (TempDir, StateDir, CaudraId) {
        let temp = TempDir::new().unwrap();
        let state_dir = StateDir::from_path(temp.path().to_path_buf());
        let mut session = StoredSession::new(MODEL, CWD);
        session.save(&state_dir).unwrap();
        (temp, state_dir, session.id)
    }

    fn run(session_id: CaudraId) -> WorkflowRunRow {
        WorkflowRunRow {
            run_id: RUN_ID.into(),
            session_id,
            display_name: "Review".into(),
            workflow_name: "review".into(),
            source_kind: WorkflowSourceKind::Builtin,
            source_path: None,
            source_digest: DIGEST.into(),
            language_version: 1,
            abi_version: 1,
            source: "let meta = #{};".into(),
            args: "{}".into(),
            objective: None,
            launch_mode: "interactive".into(),
            status: WorkflowRunStatus::Active,
            pause_kind: None,
            pause_message: None,
            revision: 0,
            execution_epoch: 0,
            phase: None,
            agent_budget: 4,
            agents_admitted: 0,
            usage: "{}".into(),
            roster: "[]".into(),
            result: None,
            error: None,
            outbox_pending: false,
            created_at: 0,
            updated_at: 0,
            bytes: 0,
        }
    }

    fn start(call_key: u64) -> WorkflowCallStart {
        WorkflowCallStart {
            run_id: RUN_ID.into(),
            call_key,
            kind: WorkflowCallKind::Agent,
            request_hash: REQUEST_HASH.into(),
            request: REQUEST.into(),
            task_id: None,
        }
    }

    fn completion() -> WorkflowCallFinish {
        WorkflowCallFinish {
            result: Some(RESULT.into()),
            ..WorkflowCallFinish::default()
        }
    }

    #[test]
    fn commands_run_in_order_against_the_session() {
        smol::block_on(async {
            let (_temp, state_dir, session_id) = open();
            let observer = SessionDatabase::open(&state_dir).unwrap();
            let store = WorkflowStore::spawn(state_dir.clone(), session_id).unwrap();

            store.insert_run(run(session_id)).await.unwrap();
            assert!(
                observer.load_workflow_run(RUN_ID).unwrap().is_some(),
                "{CONNECTIONS_SHARE_ONE_DATABASE}"
            );
            let patch = WorkflowRunPatch {
                status: Some(WorkflowRunStatus::Paused),
                outbox_pending: Some(true),
                ..WorkflowRunPatch::default()
            };
            let applied = store
                .update_run(RUN_ID.into(), 0, 0, patch.clone())
                .await
                .unwrap();
            let stale = store.update_run(RUN_ID.into(), 0, 0, patch).await.unwrap();
            assert_eq!(applied, WorkflowUpdate::Applied { revision: 1 });
            assert_eq!(stale, WorkflowUpdate::Stale, "{STALE_LOSES}");

            let loaded = store.load_run(RUN_ID.into()).await.unwrap().unwrap();
            assert_eq!(loaded.status, WorkflowRunStatus::Paused);
            assert_eq!(loaded.revision, 1);
            let runs = store.load_runs().await.unwrap();
            assert_eq!(runs.len(), 1);
            assert_eq!(runs[0].run_id, RUN_ID);

            for call_key in [2, 0, 1] {
                store.start_call(start(call_key)).await.unwrap();
            }
            store
                .finish_call(RUN_ID.into(), 1, completion())
                .await
                .unwrap();
            let calls = store.load_calls(RUN_ID.into()).await.unwrap();
            let keys: Vec<u64> = calls.iter().map(|call| call.call_key).collect();
            assert_eq!(keys, [0, 1, 2], "{CALLS_ARE_ORDERED}");
            assert_eq!(calls[1].state, WorkflowCallState::Completed);
            assert_eq!(calls[1].result.as_deref(), Some(RESULT));
            assert_eq!(calls[0].state, WorkflowCallState::Started);

            let one = store.load_call(RUN_ID.into(), 1).await.unwrap();
            assert_eq!(
                one.map(|call| call.result),
                Some(Some(RESULT.to_owned())),
                "{ONE_CALL_LOADS_ALONE}"
            );
            assert!(
                store.load_call(RUN_ID.into(), 99).await.unwrap().is_none(),
                "{UNKNOWN_CALL_IS_ABSENT}"
            );

            assert_eq!(
                store.pending_outbox().await.unwrap(),
                [(RUN_ID.to_owned(), 1)]
            );
            assert!(
                !store.ack_outbox(RUN_ID.into(), 0).await.unwrap(),
                "{OUTBOX_ACK_IS_EXACT}"
            );
            assert!(store.ack_outbox(RUN_ID.into(), 1).await.unwrap());
            assert!(store.pending_outbox().await.unwrap().is_empty());

            let path = store
                .write_scratch(RUN_ID.into(), SCRATCH_NAME.into(), SCRATCH_CONTENT.into())
                .await
                .unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), SCRATCH_CONTENT);
            store.remove_scratch(RUN_ID.into()).await.unwrap();
            assert!(!path.exists());

            assert_eq!(store.interrupt_active().await.unwrap(), 0);
            store.shutdown().await;
        });
    }

    #[test]
    fn a_failing_command_answers_and_the_store_survives() {
        smol::block_on(async {
            let (_temp, state_dir, session_id) = open();
            let store = WorkflowStore::spawn(state_dir, session_id).unwrap();
            store.insert_run(run(session_id)).await.unwrap();

            let duplicate = store.insert_run(run(session_id)).await.unwrap_err();
            store.start_call(start(0)).await.unwrap();
            store
                .finish_call(RUN_ID.into(), 0, completion())
                .await
                .unwrap();
            let refinished = store
                .finish_call(RUN_ID.into(), 0, completion())
                .await
                .unwrap_err();

            assert!(
                matches!(duplicate, WorkflowError::Storage(_)),
                "{FAILURES_ARE_REPLIES}"
            );
            assert!(
                matches!(&refinished, WorkflowError::Storage(message) if message.contains(WORKFLOW_CALL_NOT_STARTED)),
                "{FAILURES_ARE_REPLIES}"
            );
            assert!(
                store.load_run(RUN_ID.into()).await.unwrap().is_some(),
                "{FAILURES_ARE_REPLIES}"
            );
            store.shutdown().await;
        });
    }

    #[test]
    fn shutdown_joins_and_later_calls_are_unavailable() {
        smol::block_on(async {
            let (_temp, state_dir, session_id) = open();
            let store = WorkflowStore::spawn(state_dir, session_id).unwrap();
            let handle = store.clone();

            store.shutdown().await;

            assert!(
                handle.thread.lock().unwrap().is_none(),
                "{CLOSED_IS_UNAVAILABLE}"
            );
            assert_eq!(
                handle.load_runs().await,
                Err(WorkflowError::Unavailable),
                "{CLOSED_IS_UNAVAILABLE}"
            );
            handle.shutdown().await;
        });
    }
}
