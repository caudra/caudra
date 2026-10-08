#[cfg(test)]
use std::cell::Cell;
use std::fmt;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use caudra_providers::Message;
use caudra_storage::{
    StateDir,
    background::{
        BackgroundCursor, JobOwner, JobPayload, MAX_INVOCATIONS, ShellJobMetadata, TaskEvent,
        TaskRecord,
    },
    id::CaudraId,
    now_epoch,
    sessions::{RuntimeRetry, SessionDatabase},
    tool_ledger::ToolOutcome,
    tool_outputs::ToolOutputStore,
};
use futures_lite::FutureExt;
use serde_json::{Value, json};

use super::{
    Admission, BackgroundTasks, CLOSED, DriverGuard, MAX_ACTIVE, MAX_ID_BYTES, MAX_REQUEST_BYTES,
    MAX_RESULT_BYTES, STALE_INVOCATION, TRANSITION, TaskHistoryPage, bounded, deliverable,
    shells::ShellExecutions,
};
use crate::{
    CancelToken, History, SubagentHistoryStore, TaskCard, TaskProvenance, ToolDoneEvent,
    background_reminder::RuntimeSnapshot, tool_output::shell_output_label, tools::Deadline,
};

const MAX_OWNER_ACTIVE: usize = 16;
const CAPACITY: &str = "session or owner shell admission capacity exhausted";
const RETRY_MISMATCH: &str = "shell retry differs from the admitted request";
const FOREIGN_JOB: &str = "job does not belong to this invocation";
const SHELL_PANIC: &str =
    "owned shell execution panicked; execution effects may require reconciliation";

#[derive(Clone)]
pub struct JobScope {
    tasks: BackgroundTasks,
    owner: JobOwner,
    generation: u64,
    task_id: Option<Arc<str>>,
}

impl fmt::Debug for JobScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JobScope")
            .field("owner", &self.owner)
            .field("generation", &self.generation)
            .field("task_id", &self.task_id)
            .finish_non_exhaustive()
    }
}

impl BackgroundTasks {
    pub fn main_scope(&self) -> JobScope {
        JobScope {
            tasks: self.clone(),
            owner: JobOwner::Main,
            generation: self.generation(),
            task_id: None,
        }
    }

    pub fn child_scope(&self, invocation_id: impl Into<String>) -> JobScope {
        JobScope {
            tasks: self.clone(),
            owner: JobOwner::Child {
                invocation_id: invocation_id.into(),
            },
            generation: self.generation(),
            task_id: None,
        }
    }
}

impl JobScope {
    pub fn child_scope(&self, invocation_id: impl Into<String>) -> Self {
        Self {
            tasks: self.tasks.clone(),
            owner: JobOwner::Child {
                invocation_id: invocation_id.into(),
            },
            generation: self.generation,
            task_id: None,
        }
    }

    /// Names the task this scope's owner runs, for history that outlives it.
    pub fn for_task(mut self, task_id: impl Into<Arc<str>>) -> Self {
        self.task_id = Some(task_id.into());
        self
    }

    pub fn owner(&self) -> &JobOwner {
        &self.owner
    }

    pub fn task_id(&self) -> Option<&str> {
        self.task_id.as_deref()
    }

    pub fn shells(&self) -> &ShellExecutions {
        self.tasks.shells()
    }

    pub(crate) fn reminder_snapshot(&self) -> RuntimeSnapshot {
        self.tasks.reminder_snapshot_for(&self.owner)
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn session_id(&self) -> CaudraId {
        self.tasks.session_id()
    }

    pub(crate) fn state_dir(&self) -> &StateDir {
        &self.tasks.0.dir
    }

    pub fn revision(&self) -> u64 {
        self.tasks
            .lock()
            .revisions
            .get(&self.owner)
            .copied()
            .unwrap_or_default()
    }

    pub(super) fn current(&self) -> Result<(), String> {
        let state = self.tasks.lock();
        if state.generation != self.generation {
            return Err(STALE_INVOCATION.into());
        }
        if let Some(error) = &state.failure {
            return Err(error.clone());
        }
        if !state.open
            || state.shutdown
            || state.pending_stops > 0
            || state.closed_owners.contains(&self.owner)
        {
            return Err(CLOSED.into());
        }
        Ok(())
    }

    pub fn has_pending(&self) -> bool {
        self.tasks.has_pending_for(&self.owner, self.generation)
    }

    pub fn pending(&self) -> bool {
        self.tasks.lock().records.values().any(|record| {
            self.owns(record)
                && (record.active() || record.events.iter().any(|event| deliverable(record, event)))
        })
    }

    pub async fn wait(&self) -> Result<(), String> {
        loop {
            let listener = self.tasks.0.changed.listen();
            self.current()?;
            if self.has_pending() || !self.pending() {
                return Ok(());
            }
            listener.await;
        }
    }

    pub async fn wait_for_change(&self, revision: u64) -> Result<u64, String> {
        loop {
            let listener = self.tasks.0.changed.listen();
            self.current()?;
            let current = self.revision();
            if current != revision {
                return Ok(current);
            }
            listener.await;
        }
    }

    fn owns(&self, record: &TaskRecord) -> bool {
        record.owner == self.owner && record.generation == self.generation
    }

    pub fn list(&self) -> Vec<TaskCard> {
        self.tasks
            .list()
            .into_iter()
            .filter(|card| {
                card.owner == self.owner
                    && (self.owner == JobOwner::Main || card.generation == self.generation)
            })
            .collect()
    }

    pub fn status(&self, task_id: &str) -> Result<TaskCard, String> {
        let card = self.tasks.status(task_id)?;
        if card.owner != self.owner
            || (self.owner != JobOwner::Main && card.generation != self.generation)
        {
            return Err(FOREIGN_JOB.into());
        }
        Ok(card)
    }

    pub async fn history_page(
        &self,
        before: Option<BackgroundCursor>,
        limit: usize,
    ) -> Result<TaskHistoryPage, String> {
        self.tasks
            .history_page_for(
                before,
                limit,
                Some(self.owner.clone()),
                (self.owner != JobOwner::Main).then_some(self.generation),
            )
            .await
    }

    pub async fn status_async(&self, task_id: &str) -> Result<TaskCard, String> {
        let scope = self.clone();
        let task_id = task_id.to_owned();
        smol::unblock(move || scope.status(&task_id)).await
    }

    pub async fn cancel(&self, task_id: &str) -> Result<TaskCard, String> {
        let card = self.status_async(task_id).await?;
        self.tasks
            .cancel_invocation(task_id, &card.invocation_id, self.generation)
            .await
    }

    pub fn claim_messages(&self) -> Result<Vec<Message>, String> {
        self.tasks.claim_messages_for(&self.owner, self.generation)
    }

    pub async fn accept_messages(&self, messages: &[Message]) -> Result<(), String> {
        self.tasks
            .reconcile_messages(messages, false, &self.owner, Some(self.generation))
            .await
    }

    pub async fn finalize_messages(&self, messages: &[Message]) -> Result<(), String> {
        self.tasks
            .reconcile_messages(messages, true, &self.owner, Some(self.generation))
            .await
    }

    pub fn release_messages(&self, messages: &[Message]) {
        self.tasks
            .release_messages_for(messages, &self.owner, Some(self.generation));
    }

    pub async fn checkpoint(&self, task_id: &str, messages: &[Message]) -> Result<(), String> {
        let JobOwner::Child { invocation_id } = &self.owner else {
            return Err("main job receipts must be checkpointed with the session history".into());
        };
        let dir = self.tasks.0.dir.clone();
        let session = self.session_id();
        let invocation = invocation_id.clone();
        let task_id = task_id.to_owned();
        let history = History::new(messages.to_vec()).into_items();
        {
            let _gate = self.tasks.0.gate.lock().await;
            if self.generation != self.tasks.generation() {
                return Err(STALE_INVOCATION.into());
            }
            #[cfg(test)]
            let save_checked = self.tasks.lock().save_checked.clone();
            smol::unblock(move || {
                #[cfg(test)]
                let saving = Cell::new(false);
                let cancelled = || {
                    #[cfg(test)]
                    if saving.get()
                        && let Some(checked) = &save_checked
                    {
                        let _ = checked.send(());
                    }
                    false
                };
                let retry = RuntimeRetry::new(None, &cancelled);
                let database = SessionDatabase::open_runtime(&dir, &retry)
                    .map_err(|error| format!("job checkpoint connection setup: {error}"))?;
                #[cfg(test)]
                saving.set(true);
                database
                    .checkpoint_job_owner_runtime(session, &invocation, &task_id, &history, &retry)
                    .map_err(|error| format!("job checkpoint save: {error}"))
            })
            .await?;
        }
        self.settle_launches(messages).await
    }

    pub async fn settle_launches(&self, messages: &[Message]) -> Result<(), String> {
        self.tasks
            .settle_launches_for(messages, &self.owner, Some(self.generation))
            .await
    }

    pub async fn admit_shell<F, Fut>(
        &self,
        metadata: ShellJobMetadata,
        history: &SubagentHistoryStore,
        execute: F,
    ) -> Result<TaskCard, String>
    where
        F: FnOnce(CancelToken, TaskProvenance) -> Fut + Send + 'static,
        Fut: Future<Output = ToolDoneEvent> + Send + 'static,
    {
        self.admit_shell_cancellable(
            metadata,
            history,
            &CancelToken::none(),
            Deadline::None,
            execute,
        )
        .await
    }

    pub async fn admit_shell_cancellable<F, Fut>(
        &self,
        metadata: ShellJobMetadata,
        history: &SubagentHistoryStore,
        cancel: &CancelToken,
        deadline: Deadline,
        execute: F,
    ) -> Result<TaskCard, String>
    where
        F: FnOnce(CancelToken, TaskProvenance) -> Fut + Send + 'static,
        Fut: Future<Output = ToolDoneEvent> + Send + 'static,
    {
        if [&metadata.call_id, &metadata.root_call_id]
            .iter()
            .any(|id| id.is_empty() || id.len() > MAX_ID_BYTES)
            || matches!(&self.owner, JobOwner::Child { invocation_id } if invocation_id.is_empty() || invocation_id.len() > MAX_ID_BYTES)
            || metadata.command.is_empty()
            || metadata.timeout_ms == 0
        {
            return Err(
                "shell admission requires bounded identities, a command and a positive timeout"
                    .into(),
            );
        }
        let mut request = json!({"call_id": metadata.call_id, "label": metadata.command});
        if serde_json::to_vec(&metadata)
            .map_err(|error| error.to_string())?
            .len()
            > MAX_REQUEST_BYTES
        {
            return Err("shell metadata exceeds admission byte limit".into());
        }
        let admission = Admission {
            scope: self.clone(),
            cancel: cancel.clone(),
            deadline,
        };
        admission.check()?;
        let gate = self.tasks.0.gate.lock_arc().await;
        admission.check()?;
        if let Some(record) = self
            .tasks
            .retry_record(&self.owner, &metadata.call_id, &admission)
            .await?
        {
            return if record.payload == JobPayload::Shell(metadata) {
                Ok(TaskCard::from(&record))
            } else {
                Err(RETRY_MISMATCH.into())
            };
        }
        let gate = self
            .tasks
            .archive_settled(gate, Some(admission.clone()))
            .await?;
        admission.check()?;
        {
            let state = self.tasks.lock();
            if state.transition.is_some() {
                return Err(TRANSITION.into());
            }
            if state.records.len() >= MAX_INVOCATIONS {
                return Err(format!(
                    "session pending-delivery/resident capacity exhausted: {} / {MAX_INVOCATIONS}",
                    state.records.len()
                ));
            }
            let active = state
                .records
                .values()
                .filter(|record| record.active())
                .count();
            let owned = state
                .records
                .values()
                .filter(|record| self.owns(record) && record.active())
                .count();
            if active >= MAX_ACTIVE || owned >= MAX_OWNER_ACTIVE {
                return Err(format!(
                    "{CAPACITY}: active {active}/{MAX_ACTIVE}, owner {owned}/{MAX_OWNER_ACTIVE}"
                ));
            }
        }
        let dir = self.tasks.0.dir.clone();
        let session = self.tasks.session_id();
        let history = history.clone();
        let label = shell_output_label(&metadata.command);
        let label = if label == "shell" {
            label
        } else {
            format!("shell-{label}")
        };
        let reservation = admission.clone();
        let lease = smol::unblock(move || {
            reservation.check()?;
            let cancelled = || reservation.is_cancelled();
            let retry = RuntimeRetry::new(reservation.runtime_deadline(), &cancelled);
            let database = SessionDatabase::open_runtime(&dir, &retry)
                .map_err(|error| format!("shell identity connection setup: {error}"))?;
            let lease = history.reserve_generated(&label, |id| {
                database
                    .task_identity_exists_runtime(session, id, &retry)
                    .map_err(|error| format!("shell identity lookup: {error}"))
            })?;
            reservation.check()?;
            Ok::<_, String>(lease)
        })
        .await?;
        let task_id = lease.task_id().to_owned();
        admission.check()?;
        if let JobOwner::Child { invocation_id } = &self.owner {
            let state = self.tasks.lock();
            let owner = state.records.get(invocation_id);
            request["owner_call_id"] = json!(
                owner
                    .and_then(|record| record.request.get("call_id").and_then(Value::as_str))
                    .unwrap_or(invocation_id)
            );
            if let Some(owner) = owner {
                request["owner_task_id"] = json!(owner.task_id);
            }
        }
        let invocation_id = CaudraId::generate().to_string();
        let record = TaskRecord {
            payload: JobPayload::Shell(metadata.clone()),
            owner: self.owner.clone(),
            created_at: now_epoch(),
            updated_at: now_epoch(),
            sequence: self.tasks.next_sequence(),
            task_id: task_id.clone(),
            invocation_id: invocation_id.clone(),
            root_call_id: metadata.root_call_id,
            generation: self.generation,
            state: "queued".into(),
            background: true,
            receipt_accepted: false,
            mode: metadata.mode,
            request,
            outcome: None,
            output_ref: None,
            history: Value::Null,
            spec: Value::Null,
            events: Vec::new(),
        };
        let (trigger, cancel) = CancelToken::new();
        let (admitted_tx, admitted_rx) = flume::bounded(1);
        let scope = self.clone();
        let driver_id = invocation_id.clone();
        {
            let mut state = self.tasks.lock();
            state.jobs.retain(|_, job| !job.is_finished());
            state.admitting.insert(invocation_id.clone());
            state
                .drivers
                .insert(invocation_id.clone(), (self.owner.clone(), self.generation));
            state.cancels.insert(invocation_id.clone(), trigger);
            let job = smol::spawn(async move {
                let _driver = DriverGuard {
                    tasks: scope.tasks.clone(),
                    invocation: driver_id.clone(),
                };
                let card = TaskCard::summary(&record);
                let provenance = TaskProvenance {
                    session_id: session,
                    task_id,
                    invocation_id: driver_id.clone(),
                };
                let admitted = scope
                    .tasks
                    .persist_record(record, Some(admission.clone()))
                    .await;
                scope.tasks.lock().admitting.remove(&driver_id);
                if let Err(error) = admitted {
                    scope.tasks.lock().cancels.remove(&driver_id);
                    drop(gate);
                    let error = match execute_shell(execute, cancel, provenance).await {
                        Ok(_) => error,
                        Err(cleanup) => {
                            scope.tasks.lock().failure = Some(cleanup.clone());
                            format!("{error}; {cleanup}")
                        }
                    };
                    drop(lease);
                    let _ = admitted_tx.send(Err(error));
                    scope.tasks.0.changed.notify(usize::MAX);
                    return;
                }
                drop(lease);
                let current = admission.check();
                if current.is_err() {
                    scope.tasks.lock().cancels.remove(&driver_id);
                }
                drop(gate);
                if current.is_ok() {
                    let _ = admitted_tx.send(Ok(card));
                }
                let result = scope
                    .run_shell(&driver_id, cancel, provenance, execute)
                    .await;
                if let Err(error) = result {
                    let mut state = scope.tasks.lock();
                    state.failure = Some(error.clone());
                    state.open = false;
                    if let Some(record) = state.records.get_mut(&driver_id) {
                        record.state = "interrupted".into();
                        record.outcome = Some(json!({"error": error}));
                    }
                }
                scope.tasks.lock().cancels.remove(&driver_id);
                scope.tasks.0.changed.notify(usize::MAX);
                if let Err(error) = current {
                    let _ = admitted_tx.send(Err(error));
                }
            });
            state.jobs.insert(invocation_id, job);
        }
        admitted_rx
            .recv_async()
            .await
            .map_err(|_| "shell admission ended before settlement".to_owned())?
    }

    async fn run_shell<F, Fut>(
        &self,
        invocation: &str,
        cancel: CancelToken,
        provenance: TaskProvenance,
        execute: F,
    ) -> Result<(), String>
    where
        F: FnOnce(CancelToken, TaskProvenance) -> Fut,
        Fut: Future<Output = ToolDoneEvent>,
    {
        let started = async {
            let _gate = self.tasks.0.gate.lock().await;
            let mut record = self.tasks.record(invocation)?;
            if record.state == "queued" {
                record.state = "running".into();
                self.tasks.persist(record).await?;
            }
            Ok::<_, String>(())
        }
        .await;
        if let Err(error) = started {
            self.tasks.lock().cancels.remove(invocation);
            execute_shell(execute, cancel, provenance).await?;
            return Err(error);
        }
        let done = execute_shell(execute, cancel.clone(), provenance).await?;
        let _gate = self.tasks.0.gate.lock().await;
        let mut record = self.tasks.record(invocation)?;
        record.state = shell_job_state(cancel.is_cancelled(), &done).into();
        let terminal = done.composed_model_output();
        let reference = match done.output_ref {
            Some(reference) => reference,
            None => {
                let dir = self.tasks.0.dir.clone();
                let session = self.tasks.session_id();
                let retained = terminal.clone();
                let label = match &record.payload {
                    JobPayload::Shell(metadata) => {
                        format!("output-{}", shell_output_label(&metadata.command))
                    }
                    JobPayload::Agent => "output-shell".into(),
                };
                smol::unblock(move || {
                    ToolOutputStore::new(dir)
                        .put_named(session, &retained, &label)
                        .map_err(|error| error.to_string())
                })
                .await?
            }
        };
        record.output_ref = Some(reference);
        record.outcome = Some(
            json!({"output": bounded(&terminal, MAX_RESULT_BYTES), "shell": done.output, "is_error": done.is_error, "duration_ms": done.accounting.duration_ms, "outcome": done.accounting.outcome}),
        );
        let suppressed = self.current().is_err();
        record.events.push(TaskEvent {
            sequence: self.tasks.next_sequence(),
            event_id: CaudraId::generate().to_string(),
            call_id: invocation.into(),
            body: bounded(&terminal, MAX_RESULT_BYTES),
            terminal: true,
            accepted: false,
            suppressed,
        });
        self.tasks.persist(record).await
    }

    pub async fn cancel_and_drain(&self) -> Result<(), String> {
        let (tx, rx) = flume::bounded(1);
        let scope = self.clone();
        let id = CaudraId::generate();
        {
            let mut state = self.tasks.lock();
            if state.generation != self.generation {
                return Err(STALE_INVOCATION.into());
            }
            state.closed_owners.insert(self.owner.clone());
            state.stop_running.insert(id);
            let job = smol::spawn(async move {
                let result = scope.drain_owned().await;
                let _ = tx.send(result);
                scope.tasks.lock().stop_running.remove(&id);
                scope.tasks.0.changed.notify(usize::MAX);
            });
            state.stop_jobs.insert(id, job);
        }
        let result = rx
            .recv_async()
            .await
            .map_err(|_| "owner drain ended before settlement".to_owned())?;
        let job = self.tasks.lock().stop_jobs.remove(&id);
        if let Some(job) = job {
            job.await;
        }
        result
    }

    async fn drain_owned(&self) -> Result<(), String> {
        let mut failure = None;
        {
            let _gate = self.tasks.0.gate.lock().await;
            let records = self
                .tasks
                .lock()
                .records
                .values()
                .filter(|record| self.owns(record))
                .cloned()
                .collect::<Vec<_>>();
            for mut record in records {
                self.tasks.lock().cancels.remove(&record.invocation_id);
                if record.active() {
                    record.state = "cancelling".into();
                }
                for event in &mut record.events {
                    event.suppressed = true;
                }
                if let Err(error) = self.tasks.persist(record).await {
                    failure = Some(error);
                }
            }
        }
        loop {
            let listener = self.tasks.0.changed.listen();
            let running = {
                let state = self.tasks.lock();
                state.drivers.values().any(|(owner, generation)| {
                    *owner == self.owner && *generation == self.generation
                })
            };
            if !running {
                break;
            }
            listener.await;
        }
        let jobs = {
            let mut state = self.tasks.lock();
            let ids = state
                .records
                .values()
                .filter(|record| self.owns(record))
                .map(|record| record.invocation_id.clone())
                .collect::<Vec<_>>();
            ids.into_iter()
                .filter_map(|id| state.jobs.remove(&id))
                .collect::<Vec<_>>()
        };
        for job in jobs {
            job.await;
        }
        failure
            .or_else(|| self.tasks.lock().failure.clone())
            .map_or(Ok(()), Err)
    }
}

/// The outcome is the one the result's producer typed, so words a command
/// printed never decide its state.
fn shell_job_state(cancelled: bool, done: &ToolDoneEvent) -> &'static str {
    match (cancelled, done.accounting.outcome) {
        (true, _) | (_, Some(ToolOutcome::Cancelled)) => "cancelled",
        (_, Some(ToolOutcome::Timeout)) => "timed_out",
        _ if done.is_error => "failed",
        _ => "succeeded",
    }
}

async fn execute_shell<F, Fut>(
    execute: F,
    cancel: CancelToken,
    provenance: TaskProvenance,
) -> Result<ToolDoneEvent, String>
where
    F: FnOnce(CancelToken, TaskProvenance) -> Fut,
    Fut: Future<Output = ToolDoneEvent>,
{
    AssertUnwindSafe(async move { execute(cancel, provenance).await })
        .catch_unwind()
        .await
        .map_err(|_| SHELL_PANIC.into())
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;

    use caudra_config::ExecutionMode;
    use caudra_providers::{ContentBlock, Message, Role};
    use caudra_storage::{
        StateDir, background::JobKind, id::CaudraId, sessions::SessionDatabase,
        tool_outputs::ToolOutputStore,
    };
    use futures_lite::future::poll_once;
    use serde_json::json;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{
        BackgroundTasks, CLOSED, FOREIGN_JOB, RETRY_MISMATCH, STALE_INVOCATION, ShellJobMetadata,
    };
    use crate::{
        AgentEvent, AgentMode, Envelope, History, StoredSession, SubagentHistoryStore,
        SubagentInfo, TaskProvenance, ToolDoneEvent,
        agent::{
            subagent::{
                RESERVATION_SESSION_MISMATCH, TaskIdentity, TaskOptions, open_task,
                reserve_task_identity,
            },
            task_runner::{ModelResolver, SubagentTaskRunner, TaskRunner, WorkflowHostContext},
        },
        background::{
            ADMISSION_CANCELLED, MAX_BATCH_BYTES, MAX_RESULT_BYTES, MAX_SHELL_COMMAND_BYTES,
            tests::hold_writer,
        },
        background_reminder::render,
        cancel::{CancelMap, CancelToken},
        tools::{DEADLINE_EXCEEDED, Deadline, LocalTools, test_support::stub_ctx},
        types::BACKGROUND_EVENT_RUN_ID,
    };

    const CALL: &str = "shell-call";
    const ROOT: &str = "enclosing-batch";
    const CHILD: &str = "child-invocation";
    const OTHER: &str = "other-invocation";
    const COMMAND: &str = "printf bounded-output";
    const MULTILINE_COMMAND: &str =
        "printf '**literal** `code`'\nprintf '<html> & output' > destination\n";
    const OUTPUT: &str = "bounded-output";
    const TIMEOUT_MS: u64 = 120_000;
    const LITERAL_OUTPUT: &str = "<html> & output > destination\n";
    const CARGO_COMMAND: &str = "cargo test private_test_marker";
    const CARGO_ID: &str = "shell-cargo-test";
    const CARGO_SECOND_ID: &str = "shell-cargo-test-2";
    const CARGO_THIRD_ID: &str = "shell-cargo-test-3";
    const CARGO_OUTPUT: &str = "output-cargo-test";
    const COMPLEX_COMMAND: &str = "cargo test | cat";
    const SHELL_OUTPUT: &str = "output-shell";
    const EXISTING_OUTPUT: &str = "output-existing";
    const SUCCEEDED: &str = "succeeded";
    const ADMISSION_SAVE: &str = "background admission save";
    const ADMISSION_BUDGET: Duration = Duration::from_millis(250);
    const DEADLINE_ERROR: &str = "deadline exceeded";
    const CONTENTION_ERROR: &str = "SQLite contention exhausted";
    const CANCELLED: &str = "cancelled";
    const PREMATURE_ACK: &str =
        "task event must be durably saved in parent history before acknowledgment";

    struct Fixture {
        _temp: TempDir,
        dir: StateDir,
        session: StoredSession,
        tasks: BackgroundTasks,
        history: SubagentHistoryStore,
    }

    impl Fixture {
        async fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let dir = StateDir::from_path(temp.path().to_owned());
            let mut session = StoredSession::new("test-model", temp.path().to_str().unwrap());
            session.save(&dir).unwrap();
            let tasks = BackgroundTasks::spawn(dir.clone(), session.id)
                .await
                .unwrap();
            Self {
                _temp: temp,
                dir,
                session,
                tasks,
                history: SubagentHistoryStore::default(),
            }
        }
    }

    fn metadata() -> ShellJobMetadata {
        ShellJobMetadata {
            call_id: CALL.into(),
            root_call_id: ROOT.into(),
            command: COMMAND.into(),
            workdir: ".".into(),
            timeout_ms: TIMEOUT_MS,
            mode: "build".into(),
        }
    }

    fn done() -> ToolDoneEvent {
        let mut done = ToolDoneEvent::error(CALL.into(), OUTPUT);
        done.is_error = false;
        done
    }

    fn receipt() -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: ROOT.into(),
                content: "admitted".into(),
                is_error: false,
                output_ref: None,
            }],
            ..Default::default()
        }
    }

    #[test_case(false; "owner_limit")]
    #[test_case(true; "session_limit")]
    fn active_shell_limits_do_not_evict_or_replay_work(session_limit: bool) {
        smol::block_on(async {
            let fixture = Fixture::new().await;
            let count = if session_limit {
                super::MAX_ACTIVE
            } else {
                super::MAX_OWNER_ACTIVE
            };
            let (_release, wait) = flume::bounded::<()>(1);
            for index in 0..count {
                let scope = if index < super::MAX_OWNER_ACTIVE {
                    fixture.tasks.child_scope(CHILD)
                } else {
                    fixture.tasks.child_scope(OTHER)
                };
                let wait = wait.clone();
                scope
                    .admit_shell(
                        ShellJobMetadata {
                            call_id: format!("{CALL}-{index}"),
                            ..metadata()
                        },
                        &fixture.history,
                        move |cancel, _| async move {
                            let _ = cancel.race(wait.recv_async()).await;
                            done()
                        },
                    )
                    .await
                    .unwrap();
            }
            let scope = if session_limit {
                fixture.tasks.main_scope()
            } else {
                fixture.tasks.child_scope(CHILD)
            };
            let error = scope
                .admit_shell(metadata(), &fixture.history, |_, _| async {
                    panic!("capacity rejection cannot execute")
                })
                .await
                .unwrap_err();
            assert!(error.starts_with(super::CAPACITY));
            assert_eq!(fixture.tasks.active_count(), count);
            fixture.tasks.shutdown().await.unwrap();
        });
    }

    #[test]
    fn archived_shell_retry_keeps_outcome_and_never_replays_factory() {
        smol::block_on(async {
            let mut fixture = Fixture::new().await;
            let scope = fixture.tasks.main_scope();
            let count = super::MAX_INVOCATIONS + 2;
            let mut first = None;
            for index in 0..count {
                let metadata = ShellJobMetadata {
                    call_id: format!("{CALL}-{index}"),
                    ..metadata()
                };
                let card = scope
                    .admit_shell(metadata, &fixture.history, |_, _| async { done() })
                    .await
                    .unwrap();
                fixture.tasks.join_jobs().await.unwrap();
                scope.settle_launches(&[receipt()]).await.unwrap();
                let messages = scope.claim_messages().unwrap();
                fixture
                    .session
                    .replace_messages(History::new(messages.clone()).into_items());
                fixture.session.save(&fixture.dir).unwrap();
                scope.accept_messages(&messages).await.unwrap();
                if first.is_none() {
                    first = Some(fixture.tasks.status(&card.task_id).unwrap());
                }
                assert!(fixture.tasks.lock().records.is_empty());
            }
            let first = first.unwrap();
            fixture.tasks.shutdown().await.unwrap();
            let restored = BackgroundTasks::spawn(fixture.dir.clone(), fixture.session.id)
                .await
                .unwrap();
            let retried = restored
                .main_scope()
                .admit_shell(
                    ShellJobMetadata {
                        call_id: format!("{CALL}-0"),
                        ..metadata()
                    },
                    &fixture.history,
                    |_, _| async { panic!("archived retry must not execute") },
                )
                .await
                .unwrap();
            assert_eq!(retried.invocation_id, first.invocation_id);
            assert_eq!(retried.result, first.result);
            assert_eq!(retried.output_ref, first.output_ref);
            assert!(restored.lock().records.is_empty());
            restored
                .main_scope()
                .admit_shell(
                    ShellJobMetadata {
                        call_id: format!("{CALL}-new"),
                        ..metadata()
                    },
                    &fixture.history,
                    |_, _| async { done() },
                )
                .await
                .unwrap();
            restored.shutdown().await.unwrap();
        });
    }

    #[test_case(false, false; "eventual_admission")]
    #[test_case(true, false; "stop_joins_cancelled_factory")]
    #[test_case(false, true; "caller_cancellation_joins_cancelled_factory")]
    fn shell_admission_busy_retry_never_replays_factory(stop: bool, cancel_caller: bool) {
        smol::block_on(async {
            let fixture = Fixture::new().await;
            let scope = fixture.tasks.main_scope();
            let (release, writer) = hold_writer(fixture.dir.clone()).await;
            let (checked_tx, checked_rx) = flume::unbounded();
            fixture.tasks.lock().save_checked = Some(checked_tx);
            let factories = Arc::new(AtomicUsize::new(0));
            let executions = Arc::new(AtomicUsize::new(0));
            let factory_count = Arc::clone(&factories);
            let execution_count = Arc::clone(&executions);
            let owned = scope.clone();
            let history = fixture.history.clone();
            let (trigger, cancel) = CancelToken::new();
            let caller = smol::spawn(async move {
                owned
                    .admit_shell_cancellable(
                        ShellJobMetadata {
                            command: CARGO_COMMAND.into(),
                            ..metadata()
                        },
                        &history,
                        &cancel,
                        Deadline::None,
                        move |cancel, _| async move {
                            factory_count.fetch_add(1, Ordering::SeqCst);
                            if !cancel.is_cancelled() {
                                execution_count.fetch_add(1, Ordering::SeqCst);
                            }
                            done()
                        },
                    )
                    .await
            });
            checked_rx.recv_async().await.unwrap();
            checked_rx.recv_async().await.unwrap();
            assert_eq!(factories.load(Ordering::SeqCst), 0);
            assert!(fixture.tasks.list().is_empty());
            assert!(fixture.history.is_active(CARGO_ID));
            if stop || cancel_caller {
                let mut stopping = Box::pin(fixture.tasks.stop());
                if stop {
                    assert!(poll_once(&mut stopping).await.is_none());
                } else {
                    trigger.cancel();
                }
                let error = caller.await.unwrap_err();
                assert!(error.contains(ADMISSION_SAVE), "{error}");
                if stop {
                    stopping.await.unwrap();
                }
                assert!(fixture.tasks.list().is_empty());
                assert_eq!(executions.load(Ordering::SeqCst), 0);
                release.send(()).unwrap();
                writer.await;
            } else {
                release.send(()).unwrap();
                writer.await;
                let card = caller.await.unwrap();
                fixture.tasks.join_jobs().await.unwrap();
                let retried = scope
                    .admit_shell(
                        ShellJobMetadata {
                            command: CARGO_COMMAND.into(),
                            ..metadata()
                        },
                        &fixture.history,
                        |_, _| async { panic!("retry executed") },
                    )
                    .await
                    .unwrap();
                assert_eq!(card.task_id, CARGO_ID);
                assert_eq!(retried.invocation_id, card.invocation_id);
                assert_eq!(executions.load(Ordering::SeqCst), 1);
            }
            assert_eq!(factories.load(Ordering::SeqCst), 1);
            assert!(!fixture.history.is_active(CARGO_ID));
            let records = SessionDatabase::open_state(&fixture.dir)
                .unwrap()
                .background_tasks(fixture.session.id)
                .unwrap();
            assert_eq!(records.len(), usize::from(!stop && !cancel_caller));
            fixture.tasks.shutdown().await.unwrap();
        });
    }

    #[test]
    fn shell_admission_cancels_busy_archive_before_launch() {
        smol::block_on(async {
            let fixture = Fixture::new().await;
            let scope = fixture.tasks.main_scope();
            let card = scope
                .admit_shell(metadata(), &fixture.history, |_, _| async { done() })
                .await
                .unwrap();
            fixture.tasks.join_jobs().await.unwrap();
            let mut record = fixture.tasks.record(&card.invocation_id).unwrap();
            record.receipt_accepted = true;
            for event in &mut record.events {
                event.accepted = true;
            }
            fixture.tasks.persist(record).await.unwrap();
            let (release, writer) = hold_writer(fixture.dir.clone()).await;
            let checked = fixture.tasks.observe_saves_for_test();
            let (trigger, cancel) = CancelToken::new();
            let history = fixture.history.clone();
            let caller = smol::spawn(async move {
                scope
                    .admit_shell_cancellable(
                        ShellJobMetadata {
                            call_id: OTHER.into(),
                            ..metadata()
                        },
                        &history,
                        &cancel,
                        Deadline::None,
                        |_, _| async { panic!("cancelled admission executed") },
                    )
                    .await
            });
            checked.recv_async().await.unwrap();
            checked.recv_async().await.unwrap();
            assert!(fixture.tasks.0.gate.try_lock_arc().is_none());
            trigger.cancel();
            let error = caller.await.unwrap_err();
            assert!(error.contains(CANCELLED), "{error}");
            assert!(fixture.tasks.0.gate.try_lock_arc().is_some());
            assert_eq!(fixture.tasks.lock().records.len(), 1);
            assert_eq!(fixture.history.active_count(), 0);
            release.send(()).unwrap();
            writer.await;
            fixture.tasks.shutdown().await.unwrap();
        });
    }

    #[test_case(false; "already_expired")]
    #[test_case(true; "expires_under_writer_contention")]
    fn shell_admission_deadline_never_starts_execution(contended: bool) {
        smol::block_on(async {
            let fixture = Fixture::new().await;
            let (release, writer) = hold_writer(fixture.dir.clone()).await;
            let deadline = Deadline::after(if contended {
                ADMISSION_BUDGET
            } else {
                Duration::ZERO
            });
            let error = fixture
                .tasks
                .main_scope()
                .admit_shell_cancellable(
                    metadata(),
                    &fixture.history,
                    &CancelToken::none(),
                    deadline,
                    |cancel, _| async move {
                        assert!(cancel.is_cancelled());
                        done()
                    },
                )
                .await
                .unwrap_err();
            if contended {
                assert!(
                    error.contains(DEADLINE_ERROR) || error.contains(CONTENTION_ERROR),
                    "{error}"
                );
            } else {
                assert_eq!(error, DEADLINE_EXCEEDED);
            }
            assert!(fixture.tasks.list().is_empty());
            assert_eq!(fixture.history.active_count(), 0);
            release.send(()).unwrap();
            writer.await;
            fixture.tasks.shutdown().await.unwrap();
        });
    }

    #[test_case(false; "cancel_after_commit_before_handoff")]
    #[test_case(true; "cancel_after_successful_handoff")]
    fn shell_caller_cancellation_obeys_admission_boundary(admitted: bool) {
        smol::block_on(async {
            let fixture = Fixture::new().await;
            let (committed_tx, committed_rx) = flume::bounded(1);
            let (resume_tx, resume_rx) = flume::bounded(1);
            let (execute_tx, execute_rx) = flume::bounded(1);
            let (cleanup_tx, cleanup_rx) = flume::bounded(1);
            if !admitted {
                fixture.tasks.lock().admission_committed = Some((committed_tx, resume_rx));
            }
            let (trigger, cancel) = CancelToken::new();
            let scope = fixture.tasks.main_scope();
            let history = fixture.history.clone();
            let mut waiter = smol::spawn(async move {
                scope
                    .admit_shell_cancellable(
                        ShellJobMetadata {
                            command: CARGO_COMMAND.into(),
                            ..metadata()
                        },
                        &history,
                        &cancel,
                        Deadline::None,
                        move |cancel, _| async move {
                            cleanup_tx.send(()).unwrap();
                            execute_rx.recv_async().await.unwrap();
                            assert_eq!(cancel.is_cancelled(), !admitted);
                            done()
                        },
                    )
                    .await
            });
            if admitted {
                waiter.await.unwrap();
                trigger.cancel();
                execute_tx.send(()).unwrap();
            } else {
                committed_rx.recv_async().await.unwrap();
                trigger.cancel();
                resume_tx.send(()).unwrap();
                cleanup_rx.recv_async().await.unwrap();
                assert!(poll_once(&mut waiter).await.is_none());
                execute_tx.send(()).unwrap();
                assert_eq!(waiter.await.unwrap_err(), ADMISSION_CANCELLED);
            }
            fixture.tasks.join_jobs().await.unwrap();
            assert_eq!(
                fixture.tasks.status(CARGO_ID).unwrap().state,
                if admitted { SUCCEEDED } else { CANCELLED }
            );
            assert_eq!(fixture.history.active_count(), 0);
            fixture.tasks.shutdown().await.unwrap();
        });
    }

    #[test_case(false; "eventual_outcome")]
    #[test_case(true; "stop_drains_outcome")]
    fn shell_outcome_contention_preserves_one_execution_and_output(stop: bool) {
        smol::block_on(async {
            let fixture = Fixture::new().await;
            let executions = Arc::new(AtomicUsize::new(0));
            let count = Arc::clone(&executions);
            let (started_tx, started_rx) = flume::bounded(1);
            let (finish_tx, finish_rx) = flume::bounded(1);
            let card = fixture
                .tasks
                .main_scope()
                .admit_shell(metadata(), &fixture.history, move |_, _| async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    started_tx.send(()).unwrap();
                    finish_rx.recv_async().await.unwrap();
                    done()
                })
                .await
                .unwrap();
            started_rx.recv_async().await.unwrap();
            let (release, writer) = hold_writer(fixture.dir.clone()).await;
            let (checked_tx, checked_rx) = flume::unbounded();
            fixture.tasks.lock().save_checked = Some(checked_tx);
            finish_tx.send(()).unwrap();
            checked_rx.recv_async().await.unwrap();
            checked_rx.recv_async().await.unwrap();
            let mut stopping = Box::pin(fixture.tasks.stop());
            if stop {
                assert!(poll_once(&mut stopping).await.is_none());
            }
            release.send(()).unwrap();
            writer.await;
            if stop {
                stopping.await.unwrap();
            } else {
                fixture.tasks.join_jobs().await.unwrap();
            }
            assert_eq!(executions.load(Ordering::SeqCst), 1);
            let record = fixture.tasks.record(&card.invocation_id).unwrap();
            let stored = SessionDatabase::open_state(&fixture.dir)
                .unwrap()
                .background_tasks(fixture.session.id)
                .unwrap();
            assert_eq!(stored.len(), 1);
            assert_eq!(stored[0].state, SUCCEEDED);
            assert_eq!(stored[0].events.len(), 1);
            assert_eq!(stored[0].events[0].event_id, record.events[0].event_id);
            assert_eq!(stored[0].output_ref, record.output_ref);
            assert_eq!(stored[0].events[0].suppressed, stop);
            assert_eq!(record.output_ref.unwrap().id.as_str(), SHELL_OUTPUT);
            fixture.tasks.shutdown().await.unwrap();
        });
    }

    #[test_case(false; "main")]
    #[test_case(true; "workflow_child")]
    fn shell_and_task_share_active_and_durable_identity_namespace(child: bool) {
        smol::block_on(async {
            let fixture = Fixture::new().await;
            let scope = if child {
                fixture.tasks.child_scope(CHILD)
            } else {
                fixture.tasks.main_scope()
            };
            let metadata = ShellJobMetadata {
                command: CARGO_COMMAND.into(),
                ..metadata()
            };
            let task = fixture
                .history
                .reserve_generated(CARGO_ID, |_| Ok(false))
                .unwrap();
            let first = scope
                .admit_shell(metadata.clone(), &fixture.history, |_, _| async { done() })
                .await
                .unwrap();
            assert_eq!(first.task_id, CARGO_SECOND_ID);
            assert_eq!(first.label, CARGO_COMMAND);
            assert!(!fixture.history.is_active(CARGO_SECOND_ID));
            assert!(fixture.history.snapshot().records().is_empty());
            let retry = scope
                .admit_shell(metadata.clone(), &fixture.history, |_, _| async {
                    panic!("retry executed")
                })
                .await
                .unwrap();
            assert_eq!(retry.task_id, first.task_id);
            assert_eq!(retry.invocation_id, first.invocation_id);
            let next = ShellJobMetadata {
                call_id: OTHER.into(),
                ..metadata
            };
            let second = scope
                .admit_shell(next, &fixture.history, |_, _| async { done() })
                .await
                .unwrap();
            assert_eq!(second.task_id, CARGO_THIRD_ID);
            drop(task);
            fixture.tasks.shutdown().await.unwrap();
            let restored = BackgroundTasks::spawn(fixture.dir.clone(), fixture.session.id)
                .await
                .unwrap();
            let outputs = ToolOutputStore::new(fixture.dir.clone());
            let task = reserve_task_identity(
                &fixture.history,
                CARGO_SECOND_ID,
                Some(&outputs),
                Some(fixture.session.id),
                None,
            )
            .unwrap();
            assert_ne!(task.task_id(), CARGO_SECOND_ID);
            assert_eq!(
                restored.status(CARGO_SECOND_ID).unwrap().task_id,
                CARGO_SECOND_ID
            );
            restored.shutdown().await.unwrap();
        });
    }

    #[test_case(CARGO_COMMAND, CARGO_OUTPUT, false; "recognized_short_output")]
    #[test_case(COMPLEX_COMMAND, SHELL_OUTPUT, false; "complex_short_output")]
    #[test_case(CARGO_COMMAND, EXISTING_OUTPUT, true; "existing_reference_is_preserved")]
    fn shell_retained_output_uses_safe_producer_name(
        command: &str,
        expected: &str,
        existing: bool,
    ) {
        smol::block_on(async {
            let fixture = Fixture::new().await;
            let outputs = ToolOutputStore::new(fixture.dir.clone());
            let reference = existing.then(|| {
                outputs
                    .put_named(fixture.session.id, OUTPUT, EXISTING_OUTPUT)
                    .unwrap()
            });
            let retained = reference.clone();
            let card = fixture
                .tasks
                .main_scope()
                .admit_shell(
                    ShellJobMetadata {
                        command: command.into(),
                        ..metadata()
                    },
                    &fixture.history,
                    move |_, _| async move {
                        let mut result = done();
                        result.output_ref = retained;
                        result
                    },
                )
                .await
                .unwrap();
            fixture.tasks.join_jobs().await.unwrap();
            let stored = fixture
                .tasks
                .status(&card.task_id)
                .unwrap()
                .output_ref
                .unwrap();
            assert_eq!(stored.id.as_str(), expected);
            if let Some(reference) = reference {
                assert_eq!(stored, reference);
            }
            assert_eq!(
                outputs.load_text(fixture.session.id, stored.id).unwrap(),
                OUTPUT
            );
            fixture.tasks.shutdown().await.unwrap();
        });
    }

    #[test_case(false; "foreground")]
    #[test_case(true; "workflow")]
    fn task_without_output_store_skips_durable_shell_identity(workflow: bool) {
        smol::block_on(async {
            let fixture = Fixture::new().await;
            let scope = fixture.tasks.main_scope();
            let shell = scope
                .admit_shell(
                    ShellJobMetadata {
                        command: CARGO_COMMAND.into(),
                        ..metadata()
                    },
                    &fixture.history,
                    |_, _| async { done() },
                )
                .await
                .unwrap();
            fixture.tasks.join_jobs().await.unwrap();
            assert_eq!(shell.task_id, CARGO_ID);
            assert_eq!(fixture.history.active_count(), 0);
            assert!(fixture.history.snapshot().records().is_empty());
            let mut ctx = stub_ctx(&AgentMode::Build);
            ctx.subagent_history = fixture.history.clone();
            ctx.jobs = Some(scope);
            ctx.session_id = Some(fixture.session.id.into());
            ctx.tool_output_store = None;
            if workflow {
                let model: ModelResolver = Arc::new({
                    let provider = Arc::clone(&ctx.provider);
                    let model = Arc::clone(&ctx.model);
                    move || (Arc::clone(&provider), Arc::clone(&model))
                });
                let host = WorkflowHostContext::from_tool_context(
                    &ctx,
                    model,
                    Arc::new(|| AgentMode::Build),
                    Arc::new(CancelMap::new()),
                );
                assert!(host.tool_output_store.is_none());
                let runner = SubagentTaskRunner::new(Arc::new(host));
                let lease = runner.reserve_task(None, CARGO_ID).unwrap();
                assert_eq!(lease.task_id(), CARGO_SECOND_ID);
            } else {
                let mut task = open_task(
                    &ctx,
                    TaskOptions {
                        name: CARGO_ID.into(),
                        task_id: TaskIdentity::Derive,
                        profile: None,
                        mode: None,
                        model_job: None,
                        routing_prompt: None,
                        local_definitions: Vec::new(),
                        local_tools: LocalTools::default(),
                    },
                )
                .await
                .unwrap();
                assert_eq!(task.id(), CARGO_SECOND_ID);
                task.close();
            }
            assert!(ctx.tool_output_store.is_none());
            fixture.tasks.shutdown().await.unwrap();
        });
    }

    #[test_case(false; "without_outputs")]
    #[test_case(true; "with_outputs")]
    fn task_reservation_rejects_foreign_job_scope(with_outputs: bool) {
        smol::block_on(async {
            let fixture = Fixture::new().await;
            let scope = fixture.tasks.main_scope();
            let outputs = with_outputs.then(|| ToolOutputStore::new(fixture.dir.clone()));
            let error = reserve_task_identity(
                &fixture.history,
                CARGO_ID,
                outputs.as_ref(),
                Some(CaudraId::generate()),
                Some(&scope),
            )
            .unwrap_err();
            assert_eq!(error, RESERVATION_SESSION_MISMATCH);
            assert_eq!(fixture.history.active_count(), 0);
            fixture.tasks.shutdown().await.unwrap();
        });
    }

    #[test]
    fn shell_retains_lease_until_durable_admission_finishes() {
        smol::block_on(async {
            let fixture = Fixture::new().await;
            let scope = fixture.tasks.main_scope();
            let (committed_tx, committed_rx) = flume::bounded(1);
            let (resume_tx, resume_rx) = flume::bounded(1);
            fixture
                .tasks
                .pause_admission_for_test(committed_tx, resume_rx);
            let history = fixture.history.clone();
            let waiter = smol::spawn(async move {
                scope
                    .admit_shell(
                        ShellJobMetadata {
                            command: CARGO_COMMAND.into(),
                            ..metadata()
                        },
                        &history,
                        |_, _| async { done() },
                    )
                    .await
            });
            committed_rx.recv_async().await.unwrap();
            assert!(fixture.history.is_active(CARGO_ID));
            let task = fixture
                .history
                .reserve_generated(CARGO_ID, |_| Ok(false))
                .unwrap();
            assert_eq!(task.task_id(), CARGO_SECOND_ID);
            resume_tx.send(()).unwrap();
            let shell = waiter.await.unwrap();
            assert_eq!(shell.task_id, CARGO_ID);
            assert!(!fixture.history.is_active(CARGO_ID));
            assert!(fixture.history.snapshot().records().is_empty());
            fixture.tasks.shutdown().await.unwrap();
        });
    }

    #[test_case(false; "task")]
    #[test_case(true; "shell")]
    fn owner_checkpoint_only_identity_survives_reopening(shell: bool) {
        smol::block_on(async {
            let fixture = Fixture::new().await;
            let database = SessionDatabase::open(&fixture.dir).unwrap();
            database
                .checkpoint_job_owner::<Message>(fixture.session.id, CHILD, CARGO_ID, &[])
                .unwrap();
            assert!(
                database
                    .background_tasks(fixture.session.id)
                    .unwrap()
                    .is_empty()
            );
            drop(database);
            assert!(fixture.history.snapshot().records().is_empty());
            if shell {
                let card = fixture
                    .tasks
                    .main_scope()
                    .admit_shell(
                        ShellJobMetadata {
                            command: CARGO_COMMAND.into(),
                            ..metadata()
                        },
                        &fixture.history,
                        |_, _| async { done() },
                    )
                    .await
                    .unwrap();
                assert_eq!(card.task_id, CARGO_SECOND_ID);
            } else {
                let outputs = ToolOutputStore::new(fixture.dir.clone());
                let lease = reserve_task_identity(
                    &fixture.history,
                    CARGO_ID,
                    Some(&outputs),
                    Some(fixture.session.id),
                    None,
                )
                .unwrap();
                assert_eq!(lease.task_id(), CARGO_SECOND_ID);
            }
            fixture.tasks.shutdown().await.unwrap();
        });
    }

    #[test]
    fn concurrent_shell_and_task_reservations_do_not_collide() {
        smol::block_on(async {
            let fixture = Fixture::new().await;
            let scope = fixture.tasks.main_scope();
            let (start, ready) = flume::bounded(2);
            let shell_ready = ready.clone();
            let history = fixture.history.clone();
            let shell = smol::spawn(async move {
                shell_ready.recv_async().await.unwrap();
                scope
                    .admit_shell(
                        ShellJobMetadata {
                            command: CARGO_COMMAND.into(),
                            ..metadata()
                        },
                        &history,
                        |_, _| async { done() },
                    )
                    .await
                    .unwrap()
            });
            let history = fixture.history.clone();
            let outputs = ToolOutputStore::new(fixture.dir.clone());
            let session = fixture.session.id;
            let task = smol::spawn(async move {
                ready.recv_async().await.unwrap();
                reserve_task_identity(&history, CARGO_ID, Some(&outputs), Some(session), None)
                    .unwrap()
            });
            start.send(()).unwrap();
            start.send(()).unwrap();
            let (shell, task) = futures_lite::future::zip(shell, task).await;
            let mut ids = [shell.task_id.as_str(), task.task_id()];
            ids.sort_unstable();
            assert_eq!(ids, [CARGO_ID, CARGO_SECOND_ID]);
            assert!(fixture.history.snapshot().records().is_empty());
            fixture.tasks.shutdown().await.unwrap();
        });
    }

    #[test_case(ExecutionMode::Sync; "task_sync_does_not_disable_shell")]
    #[test_case(ExecutionMode::Async; "task_async_does_not_change_shell")]
    fn shell_policy_independence_and_scoped_reminders(mode: ExecutionMode) {
        smol::block_on(async {
            let fixture = Fixture::new().await;
            fixture.tasks.set_task_execution(mode);
            let main = fixture.tasks.main_scope();
            let child = fixture.tasks.child_scope(CHILD);
            let main_card = main
                .admit_shell(metadata(), &fixture.history, |cancel, _| async move {
                    cancel.cancelled().await;
                    done()
                })
                .await
                .unwrap();
            let child_card = child
                .admit_shell(metadata(), &fixture.history, |cancel, _| async move {
                    cancel.cancelled().await;
                    done()
                })
                .await
                .unwrap();
            let (main_text, _, _) = render(Some(&main.reminder_snapshot()), None);
            let (child_text, _, _) = render(Some(&child.reminder_snapshot()), None);
            let main_id = format!("\"{}\"", main_card.task_id);
            let child_id = format!("\"{}\"", child_card.task_id);
            assert!(main_text.contains(&main_id));
            assert!(!main_text.contains(&child_id));
            assert!(child_text.contains(&child_id));
            assert!(!child_text.contains(&main_id));
            fixture.tasks.shutdown().await.unwrap();
        });
    }

    #[test_case(false; "admission_failure_never_executes")]
    #[test_case(true; "terminal_failure_never_delivers_success")]
    fn durable_save_failures_do_not_advertise_execution_or_success(after_admission: bool) {
        smol::block_on(async {
            let fixture = Fixture::new().await;
            let scope = fixture.tasks.main_scope();
            let executions = Arc::new(AtomicUsize::new(0));
            let count = Arc::clone(&executions);
            let (release_tx, release_rx) = flume::bounded(1);
            let (started_tx, started_rx) = flume::bounded(1);
            let factory = move |_, _| async move {
                count.fetch_add(1, Ordering::SeqCst);
                started_tx.send(()).unwrap();
                release_rx.recv_async().await.unwrap();
                done()
            };
            let card = if after_admission {
                Some(
                    scope
                        .admit_shell(metadata(), &fixture.history, factory)
                        .await
                        .unwrap(),
                )
            } else {
                None
            };
            if after_admission {
                started_rx.recv_async().await.unwrap();
            }
            SessionDatabase::open(&fixture.dir)
                .unwrap()
                .delete(
                    fixture.session.id,
                    fixture.session.persisted_write_version(),
                )
                .unwrap();
            if after_admission {
                release_tx.send(()).unwrap();
                assert!(fixture.tasks.join_jobs().await.is_err());
                assert_eq!(
                    scope.status(&card.unwrap().task_id).unwrap().state,
                    "interrupted"
                );
                assert!(!scope.has_pending());
                assert!(scope.claim_messages().is_err());
            } else {
                let count = Arc::clone(&executions);
                assert!(
                    scope
                        .admit_shell(metadata(), &fixture.history, move |cancel, _| async move {
                            if !cancel.is_cancelled() {
                                count.fetch_add(1, Ordering::SeqCst);
                            }
                            done()
                        })
                        .await
                        .is_err()
                );
                assert_eq!(executions.load(Ordering::SeqCst), 0);
                assert!(fixture.tasks.list().is_empty());
                fixture.tasks.shutdown().await.unwrap();
            }
        });
    }

    #[test_case(false; "main_owner")]
    #[test_case(true; "child_owner")]
    fn durable_admission_and_owner_receipt_gate(child: bool) {
        smol::block_on(async {
            let mut fixture = Fixture::new().await;
            let scope = if child {
                fixture.tasks.child_scope(CHILD)
            } else {
                fixture.tasks.main_scope()
            };
            let other = fixture.tasks.child_scope(OTHER);
            let dir = fixture.dir.clone();
            let session_id = fixture.session.id;
            let card = scope
                .admit_shell(
                    metadata(),
                    &fixture.history,
                    move |_, provenance| async move {
                        let records = SessionDatabase::open(&dir)
                            .unwrap()
                            .background_tasks(session_id)
                            .unwrap();
                        let record = records
                            .iter()
                            .find(|record| record.invocation_id == provenance.invocation_id)
                            .unwrap();
                        assert_eq!(record.kind(), JobKind::Shell);
                        assert!(record.history.is_null());
                        assert!(record.spec.is_null());
                        done()
                    },
                )
                .await
                .unwrap();
            fixture.tasks.join_jobs().await.unwrap();
            assert_eq!(card.kind, JobKind::Shell);
            assert!(scope.pending());
            assert!(scope.claim_messages().unwrap().is_empty());
            assert_eq!(other.status(&card.task_id).unwrap_err(), FOREIGN_JOB);
            assert_eq!(other.cancel(&card.task_id).await.unwrap_err(), FOREIGN_JOB);
            assert_eq!(other.revision(), 0);
            other.settle_launches(&[receipt()]).await.unwrap();
            assert!(scope.claim_messages().unwrap().is_empty());
            if child {
                scope.checkpoint(CHILD, &[receipt()]).await.unwrap();
            } else {
                fixture
                    .session
                    .replace_messages(History::new(vec![receipt()]).into_items());
                fixture.session.save(&fixture.dir).unwrap();
                scope.settle_launches(&[receipt()]).await.unwrap();
            }
            if child {
                assert!(fixture.tasks.claim_messages().unwrap().is_empty());
            }
            let messages = scope.claim_messages().unwrap();
            assert_eq!(
                messages
                    .iter()
                    .filter(|message| message.task_event.is_some())
                    .count(),
                1
            );
            assert_eq!(
                scope.accept_messages(&messages).await.unwrap_err(),
                PREMATURE_ACK
            );
            if child {
                scope.checkpoint(CHILD, &messages).await.unwrap();
            } else {
                fixture
                    .session
                    .replace_messages(History::new(messages.clone()).into_items());
                fixture.session.save(&fixture.dir).unwrap();
            }
            scope.accept_messages(&messages).await.unwrap();
            assert!(!scope.pending());
            assert!(scope.claim_messages().unwrap().is_empty());
            fixture.tasks.shutdown().await.unwrap();
        });
    }

    #[test_case(false; "admission_save_failure")]
    #[test_case(true; "running_save_failure")]
    fn persistence_failure_awaits_abandon_even_without_admission_waiter(running: bool) {
        smol::block_on(async {
            let fixture = Fixture::new().await;
            let scope = fixture.tasks.child_scope(CHILD);
            let (committed_tx, committed_rx) = flume::bounded(1);
            let (resume_tx, resume_rx) = flume::bounded(1);
            if running {
                fixture.tasks.lock().admission_committed = Some((committed_tx, resume_rx));
            }
            let mut database = SessionDatabase::open(&fixture.dir).unwrap();
            if !running {
                database
                    .delete(
                        fixture.session.id,
                        fixture.session.persisted_write_version(),
                    )
                    .unwrap();
            }
            let (cleanup_tx, cleanup_rx) = flume::bounded(1);
            let (release_tx, release_rx) = flume::bounded(1);
            let owned = scope.clone();
            let tasks = fixture.tasks.clone();
            let history = fixture.history.clone();
            let waiter = smol::spawn(async move {
                owned
                    .admit_shell(metadata(), &history, move |cancel, _| async move {
                        assert!(cancel.is_cancelled());
                        {
                            let _gate = tasks.0.gate.lock().await;
                        }
                        cleanup_tx.send(()).unwrap();
                        release_rx.recv_async().await.unwrap();
                        done()
                    })
                    .await
            });
            if running {
                committed_rx.recv_async().await.unwrap();
                database
                    .delete(
                        fixture.session.id,
                        fixture.session.persisted_write_version(),
                    )
                    .unwrap();
                waiter.cancel().await;
                resume_tx.send(()).unwrap();
            } else {
                cleanup_rx.recv_async().await.unwrap();
                waiter.cancel().await;
            }
            if running {
                cleanup_rx.recv_async().await.unwrap();
            }
            let mut drain = Box::pin(scope.cancel_and_drain());
            assert!(poll_once(&mut drain).await.is_none());
            if !running {
                assert!(fixture.history.is_active("shell"));
            }
            release_tx.send(()).unwrap();
            assert_eq!(drain.await.is_err(), running);
            assert!(fixture.tasks.lock().drivers.is_empty());
            assert_eq!(fixture.history.active_count(), 0);
            assert!(fixture.history.snapshot().records().is_empty());
        });
    }

    #[test_case(false, false, COMMAND; "main_success")]
    #[test_case(true, false, COMMAND; "child_success")]
    #[test_case(false, true, COMMAND; "main_failure")]
    #[test_case(true, true, COMMAND; "child_failure")]
    #[test_case(false, false, MULTILINE_COMMAND; "multiline_command")]
    fn shell_envelopes_validate_owner_and_deliver_literal_output(
        child: bool,
        failed: bool,
        command: &str,
    ) {
        smol::block_on(async {
            let fixture = Fixture::new().await;
            let scope = if child {
                fixture.tasks.child_scope(CHILD)
            } else {
                fixture.tasks.main_scope()
            };
            let card = scope
                .admit_shell(
                    ShellJobMetadata {
                        command: command.into(),
                        ..metadata()
                    },
                    &fixture.history,
                    move |_, _| async move {
                        let mut done = ToolDoneEvent::error(CALL.into(), LITERAL_OUTPUT);
                        done.is_error = failed;
                        done
                    },
                )
                .await
                .unwrap();
            fixture.tasks.join_jobs().await.unwrap();
            let child_info = SubagentInfo {
                parent_tool_use_id: CHILD.into(),
                task_id: OTHER.into(),
                name: OTHER.into(),
                prompt: None,
                model: None,
                thinking: None,
                fast: false,
                answer_tx: None,
                steer_tx: None,
            };
            let mut envelope = Envelope {
                event: AgentEvent::TaskAdmitted(card.clone()),
                subagent: child.then_some(child_info.clone()),
                run_id: BACKGROUND_EVENT_RUN_ID,
                workflow: None,
                task: Some(Arc::new(TaskProvenance {
                    session_id: fixture.session.id,
                    task_id: card.task_id.clone(),
                    invocation_id: card.invocation_id.clone(),
                })),
            };
            assert!(fixture.tasks.owns_event(&envelope));
            if child {
                envelope.subagent.as_mut().unwrap().parent_tool_use_id = OTHER.into();
            } else {
                envelope.subagent = Some(child_info);
            }
            assert!(!fixture.tasks.owns_event(&envelope));
            if child {
                envelope.subagent = None;
                assert!(!fixture.tasks.owns_event(&envelope));
            }
            scope.settle_launches(&[receipt()]).await.unwrap();
            let messages = scope.claim_messages().unwrap();
            let result = messages
                .iter()
                .find(|message| message.task_event.is_some())
                .unwrap()
                .first_text_content()
                .unwrap();
            let status = if failed { "failure" } else { "success" };
            assert_eq!(
                result,
                format!(
                    "Shell {}: {status}.\n\nCommand:\n{command}\n\n{LITERAL_OUTPUT}",
                    card.task_id
                )
            );
            fixture.tasks.shutdown().await.unwrap();
        });
    }

    #[test_case(false; "main_shell")]
    #[test_case(true; "child_shell")]
    fn oversized_shell_command_does_not_block_delivery(child: bool) {
        const MULTIBYTE: &str = "界";
        const TRUNCATED: &str = "[truncated; inspect task status]";
        const OUTPUT_BYTE: &str = "x";
        smol::block_on(async {
            let fixture = Fixture::new().await;
            let scope = if child {
                fixture.tasks.child_scope(CHILD)
            } else {
                fixture.tasks.main_scope()
            };
            let command = MULTIBYTE.repeat(MAX_BATCH_BYTES / MULTIBYTE.len() + 1);
            let output = OUTPUT_BYTE.repeat(MAX_RESULT_BYTES);
            let retained_output = output.clone();
            let card = scope
                .admit_shell(
                    ShellJobMetadata {
                        command,
                        ..metadata()
                    },
                    &fixture.history,
                    move |_, _| async move {
                        let mut done = ToolDoneEvent::error(CALL.into(), retained_output);
                        done.is_error = false;
                        done
                    },
                )
                .await
                .unwrap();
            fixture.tasks.join_jobs().await.unwrap();
            scope.settle_launches(&[receipt()]).await.unwrap();
            let messages = scope.claim_messages().unwrap();
            let result = messages
                .iter()
                .find(|message| message.task_event.is_some())
                .unwrap()
                .first_text_content()
                .unwrap();
            let prefix = MULTIBYTE.repeat(MAX_SHELL_COMMAND_BYTES / MULTIBYTE.len());
            assert_eq!(
                result,
                format!(
                    "Shell {}: success.\n\nCommand:\n{prefix}\n{TRUNCATED}\n\n{output}",
                    card.task_id
                )
            );
            assert!(result.len() < MAX_BATCH_BYTES);
            fixture.tasks.shutdown().await.unwrap();
        });
    }

    #[test_case(false; "factory_panics")]
    #[test_case(true; "execution_panics")]
    fn shell_panics_settle_the_owned_driver(asynchronous: bool) {
        smol::block_on(async {
            let fixture = Fixture::new().await;
            let card = fixture
                .tasks
                .main_scope()
                .admit_shell(metadata(), &fixture.history, move |_, _| {
                    assert!(asynchronous, "factory panic");
                    async { panic!("execution panic") }
                })
                .await
                .unwrap();
            assert_eq!(
                fixture.tasks.join_jobs().await.unwrap_err(),
                super::SHELL_PANIC
            );
            assert_eq!(
                fixture.tasks.status(&card.task_id).unwrap().result,
                Some(json!({"error": super::SHELL_PANIC}))
            );
            assert_eq!(fixture.tasks.active_count(), 0);
        });
    }

    #[test_case(false; "ordinary_receipt_waiter")]
    #[test_case(true; "dropped_admission_waiter")]
    fn execution_is_owned_and_retry_is_not_reexecuted(drop_waiter: bool) {
        smol::block_on(async {
            let fixture = Fixture::new().await;
            let scope = fixture.tasks.main_scope();
            let executions = Arc::new(AtomicUsize::new(0));
            let count = Arc::clone(&executions);
            let (committed_tx, committed_rx) = flume::bounded(1);
            let (resume_tx, resume_rx) = flume::bounded(1);
            if drop_waiter {
                fixture.tasks.lock().admission_committed = Some((committed_tx, resume_rx));
            }
            let waiter_scope = scope.clone();
            let history = fixture.history.clone();
            let waiter = smol::spawn(async move {
                waiter_scope
                    .admit_shell(metadata(), &history, move |_, _| async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        done()
                    })
                    .await
            });
            if drop_waiter {
                committed_rx.recv_async().await.unwrap();
                waiter.cancel().await;
                resume_tx.send(()).unwrap();
            } else {
                waiter.await.unwrap();
            }
            fixture.tasks.join_jobs().await.unwrap();
            let retry = scope
                .admit_shell(metadata(), &fixture.history, |_, _| async {
                    panic!("retry executed")
                })
                .await
                .unwrap();
            assert_eq!(retry.state, "succeeded");
            assert_eq!(executions.load(Ordering::SeqCst), 1);
            let mut changed = metadata();
            changed.command = OUTPUT.into();
            assert_eq!(
                scope
                    .admit_shell(changed, &fixture.history, |_, _| async { done() })
                    .await
                    .unwrap_err(),
                RETRY_MISMATCH
            );
            fixture.tasks.shutdown().await.unwrap();
        });
    }

    #[test_case(false; "owner_cleanup")]
    #[test_case(true; "concurrent_session_stop")]
    fn cancellation_waits_for_owned_cleanup(concurrent_stop: bool) {
        smol::block_on(async {
            let fixture = Fixture::new().await;
            let scope = fixture.tasks.child_scope(CHILD);
            let (cancelled_tx, cancelled_rx) = flume::bounded(1);
            let (clean_tx, clean_rx) = flume::bounded(1);
            scope
                .admit_shell(metadata(), &fixture.history, move |cancel, _| async move {
                    cancel.cancelled().await;
                    cancelled_tx.send(()).unwrap();
                    clean_rx.recv_async().await.unwrap();
                    done()
                })
                .await
                .unwrap();
            let drain_scope = scope.clone();
            let drain = smol::spawn(async move { drain_scope.cancel_and_drain().await });
            cancelled_rx.recv_async().await.unwrap();
            let tasks = fixture.tasks.clone();
            let mut stop = Box::pin(async move {
                if concurrent_stop {
                    tasks.stop().await
                } else {
                    Ok(())
                }
            });
            if concurrent_stop {
                assert!(poll_once(&mut stop).await.is_none());
            }
            let mut drain = Box::pin(drain);
            assert!(poll_once(&mut drain).await.is_none());
            clean_tx.send(()).unwrap();
            drain.await.unwrap();
            stop.await.unwrap();
            assert!(!scope.pending());
            assert_eq!(
                scope
                    .admit_shell(metadata(), &fixture.history, |_, _| async { done() })
                    .await
                    .unwrap_err(),
                CLOSED
            );
            fixture.tasks.shutdown().await.unwrap();
        });
    }

    #[test_case(false; "new_generation_fences_old_handle")]
    #[test_case(true; "recovery_never_replays")]
    fn stopped_and_restored_jobs_do_not_execute_again(restore: bool) {
        smol::block_on(async {
            let fixture = Fixture::new().await;
            let scope = fixture.tasks.main_scope();
            let card = scope
                .admit_shell(metadata(), &fixture.history, |_, _| async { done() })
                .await
                .unwrap();
            fixture.tasks.join_jobs().await.unwrap();
            if restore {
                let mut record = fixture.tasks.record(&card.invocation_id).unwrap();
                record.state = "running".into();
                record.outcome = None;
                record.events.clear();
                fixture.tasks.persist(record).await.unwrap();
                let restored = BackgroundTasks::spawn(fixture.dir.clone(), fixture.session.id)
                    .await
                    .unwrap();
                assert_eq!(restored.status(&card.task_id).unwrap().state, "interrupted");
                assert_eq!(restored.active_count(), 0);
                assert!(restored.claim_messages().unwrap().is_empty());
                restored.shutdown().await.unwrap();
            } else {
                fixture.tasks.stop().await.unwrap();
                fixture.tasks.rearm();
                assert_eq!(
                    scope
                        .admit_shell(metadata(), &fixture.history, |_, _| async { done() })
                        .await
                        .unwrap_err(),
                    STALE_INVOCATION
                );
                assert_eq!(
                    scope.cancel_and_drain().await.unwrap_err(),
                    STALE_INVOCATION
                );
                fixture
                    .tasks
                    .main_scope()
                    .admit_shell(metadata(), &fixture.history, |_, _| async { done() })
                    .await
                    .unwrap();
                fixture.tasks.shutdown().await.unwrap();
            }
        });
    }
}
