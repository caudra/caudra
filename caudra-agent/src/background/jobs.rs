#[cfg(test)]
use std::cell::Cell;
use std::fmt;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use caudra_providers::Message;
use caudra_storage::{
    StateDir,
    background::{
        BackgroundCursor, JobKind, JobOwner, JobPayload, MAX_INVOCATIONS, QuestionJobMetadata,
        ShellJobMetadata, TaskEvent, TaskRecord, TranscriptWrite,
    },
    id::CaudraId,
    now_epoch,
    sessions::{RuntimeRetry, SessionDatabase},
    tool_ledger::ToolOutcome,
    tool_outputs::ToolOutputStore,
};
use futures_lite::{FutureExt, future};
use serde_json::{Value, json};
use tracing::warn;

use super::{
    Admission, BackgroundTasks, CLOSED, DriverGuard, MAX_ACTIVE, MAX_ID_BYTES, MAX_REQUEST_BYTES,
    MAX_RESULT_BYTES, RUNNING_STATE, STALE_INVOCATION, State, TASK_DURATION_FIELD, TRANSITION,
    TaskHistoryPage, bounded, deliverable,
    shells::{ShellExecutions, ShellLive},
    wait_nudge::{self, WaitNudge},
};
use crate::{
    CancelToken, CancelTrigger, History, SubagentHistoryStore, TaskCard, TaskProvenance,
    ToolDoneEvent,
    background_reminder::RuntimeSnapshot,
    tool_output::shell_output_label,
    tools::{Deadline, ToolFailure},
};

const MAX_OWNER_ACTIVE: usize = 16;
/// Deferred questions the main agent may have open at once.
pub const MAX_DEFERRED_QUESTIONS: usize = 3;
const CAPACITY: &str = "session or owner job admission capacity exhausted";
const QUESTION_CAPACITY: &str = "deferred question capacity exhausted";
const QUESTION_CLOSED: &str = "the question is no longer open";
const QUESTION_IDENTITY: &str = "question";
const QUESTION_MODE: &str = "question";
const QUESTION_OUTPUT_LABEL: &str = "output-question";
const QUESTION_OUTCOME_FIELD: &str = "answers";
const SHELL_OUTCOME_FIELD: &str = "shell";
const RETRY_MISMATCH: &str = "job retry differs from the admitted request";
const FOREIGN_JOB: &str = "job does not belong to this invocation";
const JOB_PANIC: &str =
    "owned job execution panicked; execution effects may require reconciliation";
const ADOPTION_WITHDRAWN: &str =
    "the command stayed with its caller because its job was stopped before it ran";
const ADOPTION_RETRIED: &str = "this call already has a job, so the command stayed with its caller";
const SHELL_TASK_GONE: &str = "the shell command's task was already released";

/// A foreground shell command still running on a task of its own, which a
/// job can take over without restarting it. Dropping it stops the command
/// and lets it clean up on its own.
pub(crate) struct RunningShell {
    task: Option<smol::Task<ToolDoneEvent>>,
    trigger: Option<CancelTrigger>,
}

impl RunningShell {
    /// `trigger` is the only way to stop `task`: it must be rooted, not a
    /// child of the turn, or the command would stop when the turn does.
    pub(crate) fn new(task: smol::Task<ToolDoneEvent>, trigger: CancelTrigger) -> Self {
        Self {
            task: Some(task),
            trigger: Some(trigger),
        }
    }

    /// The command's result, stopping it first once `cancel` fires.
    pub(crate) async fn finish(&mut self, cancel: &CancelToken) -> ToolDoneEvent {
        let Some(task) = self.task.as_mut() else {
            return ToolDoneEvent::error(String::new(), SHELL_TASK_GONE);
        };
        if let Ok(done) = cancel.race(&mut *task).await {
            return done;
        }
        self.trigger.take();
        task.await
    }

    /// `None` when `limit` passes first, with the command still running.
    pub(crate) async fn wait(
        &mut self,
        cancel: &CancelToken,
        limit: Duration,
    ) -> Option<ToolDoneEvent> {
        let task = self.task.as_mut()?;
        let settled = future::race(async { Some(cancel.race(task).await) }, async {
            smol::Timer::after(limit).await;
            None
        })
        .await;
        match settled {
            Some(Ok(done)) => Some(done),
            Some(Err(_)) => Some(self.finish(cancel).await),
            None => None,
        }
    }
}

impl Drop for RunningShell {
    fn drop(&mut self) {
        self.trigger.take();
        if let Some(task) = self.task.take() {
            task.detach();
        }
    }
}

fn lock_running(slot: &Mutex<Option<RunningShell>>) -> MutexGuard<'_, Option<RunningShell>> {
    slot.lock().unwrap_or_else(|error| error.into_inner())
}

/// What one admission writes on its record, whatever kind of job it is.
struct JobSpec {
    payload: JobPayload,
    call_id: String,
    root_call_id: String,
    /// Names the generated task id.
    identity: String,
    /// What lists and reminders show for the job.
    label: String,
    mode: String,
    /// How often to ask the owner whether a running shell still deserves the wait.
    wait_nudge: Option<WaitNudge>,
}

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
            None,
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
        wait_nudge: Option<WaitNudge>,
        execute: F,
    ) -> Result<TaskCard, String>
    where
        F: FnOnce(CancelToken, TaskProvenance) -> Fut + Send + 'static,
        Fut: Future<Output = ToolDoneEvent> + Send + 'static,
    {
        if metadata.command.is_empty() || metadata.timeout_ms == 0 {
            return Err(
                "shell admission requires bounded identities, a command and a positive timeout"
                    .into(),
            );
        }
        let label = shell_output_label(&metadata.command);
        let spec = JobSpec {
            call_id: metadata.call_id.clone(),
            root_call_id: metadata.root_call_id.clone(),
            label: metadata.command.clone(),
            identity: if label == "shell" {
                label
            } else {
                format!("shell-{label}")
            },
            mode: metadata.mode.clone(),
            payload: JobPayload::Shell(metadata),
            wait_nudge,
        };
        self.admit_job(spec, history, cancel, deadline, execute)
            .await
    }

    /// Hands a foreground command that is still running to a new shell job,
    /// without restarting it. `settle` turns its result into the job's. A
    /// refused admission hands the command back, still running.
    ///
    /// Every refusal fires the job's own cancel before its execution starts,
    /// so a job that starts cancelled leaves the command where it is, to be
    /// handed back or, once admitted, dropped and so stopped.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn adopt_shell<S, Fut>(
        &self,
        metadata: ShellJobMetadata,
        history: &SubagentHistoryStore,
        cancel: &CancelToken,
        deadline: Deadline,
        wait_nudge: Option<WaitNudge>,
        running: RunningShell,
        settle: S,
    ) -> Result<TaskCard, (String, Option<RunningShell>)>
    where
        S: FnOnce(ToolDoneEvent) -> Fut + Send + 'static,
        Fut: Future<Output = ToolDoneEvent> + Send + 'static,
    {
        let call_id = metadata.call_id.clone();
        let slot = Arc::new(Mutex::new(Some(running)));
        let held = Arc::clone(&slot);
        let admitted = self
            .admit_shell_cancellable(
                metadata,
                history,
                cancel,
                deadline,
                wait_nudge,
                move |cancel, _| async move {
                    let adopted = match cancel.is_cancelled() {
                        true => None,
                        false => lock_running(&held).take(),
                    };
                    let done = match adopted {
                        Some(mut running) => running.finish(&cancel).await,
                        None => ToolDoneEvent::error(call_id, ADOPTION_WITHDRAWN)
                            .with_failure(ToolFailure::Cancelled),
                    };
                    settle(done).await
                },
            )
            .await;
        match admitted {
            // A retried admission drops `execute` unrun, so the command is still here.
            Ok(_) if Arc::strong_count(&slot) == 1 && lock_running(&slot).is_some() => {
                Err((ADOPTION_RETRIED.into(), lock_running(&slot).take()))
            }
            Ok(card) => Ok(card),
            Err(message) => Err((message, lock_running(&slot).take())),
        }
    }

    /// Admits a `question` the main agent asked without waiting. `execute`
    /// gets the receiver the user's answer arrives on, open for exactly as
    /// long as the question is.
    pub async fn admit_question<F, Fut>(
        &self,
        metadata: QuestionJobMetadata,
        history: &SubagentHistoryStore,
        cancel: &CancelToken,
        deadline: Deadline,
        execute: F,
    ) -> Result<TaskCard, String>
    where
        F: FnOnce(CancelToken, TaskProvenance, flume::Receiver<String>) -> Fut + Send + 'static,
        Fut: Future<Output = ToolDoneEvent> + Send + 'static,
    {
        let tasks = self.tasks.clone();
        let spec = JobSpec {
            call_id: metadata.call_id.clone(),
            root_call_id: metadata.root_call_id.clone(),
            label: metadata.label.clone(),
            identity: QUESTION_IDENTITY.into(),
            mode: QUESTION_MODE.into(),
            payload: JobPayload::Question(metadata),
            wait_nudge: None,
        };
        self.admit_job(
            spec,
            history,
            cancel,
            deadline,
            move |cancel, provenance| {
                let (desk, answers) = tasks.open_desk(&provenance.task_id);
                async move {
                    let _desk = desk;
                    execute(cancel, provenance, answers).await
                }
            },
        )
        .await
    }

    async fn admit_job<F, Fut>(
        &self,
        spec: JobSpec,
        history: &SubagentHistoryStore,
        cancel: &CancelToken,
        deadline: Deadline,
        execute: F,
    ) -> Result<TaskCard, String>
    where
        F: FnOnce(CancelToken, TaskProvenance) -> Fut + Send + 'static,
        Fut: Future<Output = ToolDoneEvent> + Send + 'static,
    {
        if [&spec.call_id, &spec.root_call_id]
            .iter()
            .any(|id| id.is_empty() || id.len() > MAX_ID_BYTES)
            || matches!(&self.owner, JobOwner::Child { invocation_id } if invocation_id.is_empty() || invocation_id.len() > MAX_ID_BYTES)
        {
            return Err("job admission requires bounded identities".into());
        }
        let mut request = json!({"call_id": spec.call_id, "label": spec.label});
        if serde_json::to_vec(&spec.payload)
            .map_err(|error| error.to_string())?
            .len()
            > MAX_REQUEST_BYTES
        {
            return Err("job metadata exceeds admission byte limit".into());
        }
        let questions = matches!(spec.payload, JobPayload::Question(_));
        let wait_nudge = spec.wait_nudge;
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
            .retry_record(&self.owner, &spec.call_id, &admission)
            .await?
        {
            return if record.payload == spec.payload {
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
            if questions && open_questions(&state, &self.owner) >= MAX_DEFERRED_QUESTIONS {
                return Err(format!(
                    "{QUESTION_CAPACITY}: {MAX_DEFERRED_QUESTIONS} / {MAX_DEFERRED_QUESTIONS}"
                ));
            }
        }
        let dir = self.tasks.0.dir.clone();
        let session = self.tasks.session_id();
        let history = history.clone();
        let identity = spec.identity;
        let reservation = admission.clone();
        let lease = smol::unblock(move || {
            reservation.check()?;
            let cancelled = || reservation.is_cancelled();
            let retry = RuntimeRetry::new(reservation.runtime_deadline(), &cancelled);
            let database = SessionDatabase::open_runtime(&dir, &retry)
                .map_err(|error| format!("job identity connection setup: {error}"))?;
            let lease = history.reserve_generated(&identity, |id| {
                database
                    .task_identity_exists_runtime(session, id, &retry)
                    .map_err(|error| format!("job identity lookup: {error}"))
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
            payload: spec.payload,
            owner: self.owner.clone(),
            created_at: now_epoch(),
            updated_at: now_epoch(),
            sequence: self.tasks.next_sequence(),
            task_id: task_id.clone(),
            invocation_id: invocation_id.clone(),
            root_call_id: spec.root_call_id,
            generation: self.generation,
            state: "queued".into(),
            background: true,
            receipt_accepted: false,
            mode: spec.mode,
            request,
            outcome: None,
            output_ref: None,
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
                    .persist_record(record, Some(admission.clone()), TranscriptWrite::Keep)
                    .await;
                scope.tasks.lock().admitting.remove(&driver_id);
                if let Err(error) = admitted {
                    scope.tasks.lock().cancels.remove(&driver_id);
                    drop(gate);
                    let error = match execute_job(execute, cancel, provenance).await {
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
                    .run_job(&driver_id, cancel, provenance, wait_nudge, execute)
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
            .map_err(|_| "job admission ended before settlement".to_owned())?
    }

    async fn run_job<F, Fut>(
        &self,
        invocation: &str,
        cancel: CancelToken,
        provenance: TaskProvenance,
        wait_nudge: Option<WaitNudge>,
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
            execute_job(execute, cancel, provenance).await?;
            return Err(error);
        }
        let nudges = wait_nudge.map(|nudge| {
            let (stop, stopped) = flume::bounded::<()>(1);
            let scope = self.clone();
            let invocation = invocation.to_owned();
            let nudges =
                smol::spawn(async move { scope.keep_nudging(&invocation, nudge, stopped).await });
            (stop, nudges)
        });
        let done = execute_job(execute, cancel.clone(), provenance).await;
        if let Some((stop, nudges)) = nudges {
            drop(stop);
            nudges.await;
        }
        let done = done?;
        let _gate = self.tasks.0.gate.lock().await;
        let mut record = self.tasks.record(invocation)?;
        record.state = shell_job_state(cancel.is_cancelled(), &done).into();
        for event in &mut record.events {
            event.suppressed |= event.wait_nudge && !event.accepted;
        }
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
                    JobPayload::Question(_) => QUESTION_OUTPUT_LABEL.into(),
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
        let output_field = match record.kind() {
            JobKind::Question => QUESTION_OUTCOME_FIELD,
            JobKind::Agent | JobKind::Shell => SHELL_OUTCOME_FIELD,
        };
        let mut outcome = json!({"output": bounded(&terminal, MAX_RESULT_BYTES), "is_error": done.is_error, TASK_DURATION_FIELD: done.accounting.duration_ms, "outcome": done.accounting.outcome});
        outcome[output_field] = json!(done.output);
        let interrupting = self.tasks.interrupting() && cancel.is_cancelled();
        if interrupting {
            super::mark_shutdown_interrupted(&mut record, &mut outcome);
        }
        record.outcome = Some(outcome);
        let suppressed = interrupting || self.current().is_err();
        record.events.push(TaskEvent {
            sequence: self.tasks.next_sequence(),
            event_id: CaudraId::generate().to_string(),
            call_id: invocation.into(),
            body: bounded(&terminal, MAX_RESULT_BYTES),
            terminal: true,
            accepted: false,
            suppressed,
            wait_nudge: false,
        });
        self.tasks.persist(record).await
    }

    /// Asks the owner every interval the shell runs whether it is still
    /// worth the wait, until `stop` closes.
    async fn keep_nudging(&self, invocation: &str, nudge: WaitNudge, stop: flume::Receiver<()>) {
        let mut previous: Option<String> = None;
        let mut due = nudge.started;
        loop {
            while due <= Instant::now() {
                due += nudge.interval;
            }
            let ticked = async {
                smol::Timer::at(due).await;
                true
            }
            .or(async {
                let _ = stop.recv_async().await;
                false
            })
            .await;
            if !ticked {
                return;
            }
            let tail = self
                .tasks
                .shells()
                .snapshot()
                .jobs
                .get(invocation)
                .and_then(ShellLive::lines)
                .and_then(|lines| nudge.tail(&lines));
            let output = wait_nudge::Output::since(tail, previous.as_deref());
            if let Err(error) = self
                .nudge(invocation, nudge.started.elapsed(), nudge.interval, &output)
                .await
            {
                warn!(invocation, %error, "shell wait nudge save failed");
            }
            previous = output.tail().map(str::to_owned);
        }
    }

    /// Puts one nudge in front of the owner. One it has not claimed yet is
    /// brought up to date rather than joined by another, and one it holds
    /// already will reach it, so this one is left out.
    async fn nudge(
        &self,
        invocation: &str,
        elapsed: Duration,
        interval: Duration,
        output: &wait_nudge::Output,
    ) -> Result<(), String> {
        let _gate = self.tasks.0.gate.lock().await;
        if self.current().is_err() {
            return Ok(());
        }
        let mut record = self.tasks.record(invocation)?;
        let JobPayload::Shell(metadata) = &record.payload else {
            return Ok(());
        };
        if record.state != RUNNING_STATE {
            return Ok(());
        }
        let body = wait_nudge::render(&record.task_id, metadata, elapsed, interval, output);
        let pending = record
            .events
            .iter()
            .position(|event| event.wait_nudge && deliverable(&record, event));
        match pending {
            Some(index)
                if self
                    .tasks
                    .lock()
                    .claims
                    .contains_key(&record.events[index].event_id) =>
            {
                return Ok(());
            }
            Some(index) => record.events[index].body = body,
            None => record.events.push(TaskEvent {
                sequence: self.tasks.next_sequence(),
                event_id: CaudraId::generate().to_string(),
                call_id: invocation.into(),
                body,
                terminal: false,
                accepted: false,
                suppressed: false,
                wait_nudge: true,
            }),
        }
        self.tasks.persist(record).await
    }

    /// Lets a child that [`Self::cancel_and_drain`] closed admit work again,
    /// for a replacement turn that carries on the same invocation. A stop,
    /// shutdown or failure of the whole runtime still refuses it.
    pub(crate) fn reopen(&self) {
        debug_assert!(matches!(self.owner, JobOwner::Child { .. }));
        let mut state = self.tasks.lock();
        if state.generation == self.generation {
            state.closed_owners.remove(&self.owner);
        }
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

async fn execute_job<F, Fut>(
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
        .map_err(|_| JOB_PANIC.into())
}

fn open_questions(state: &State, owner: &JobOwner) -> usize {
    state
        .records
        .values()
        .filter(|record| {
            record.owner == *owner
                && record.generation == state.generation
                && record.kind() == JobKind::Question
                && record.active()
        })
        .count()
}

/// The open end of one deferred question's answer channel. Dropped when the
/// question settles, so a late answer is refused rather than lost.
struct AnswerDesk {
    tasks: BackgroundTasks,
    task_id: String,
}

impl Drop for AnswerDesk {
    fn drop(&mut self) {
        self.tasks.lock().answers.remove(&self.task_id);
    }
}

impl BackgroundTasks {
    fn open_desk(&self, task_id: &str) -> (AnswerDesk, flume::Receiver<String>) {
        let (sender, answers) = flume::bounded(1);
        self.lock().answers.insert(task_id.to_owned(), sender);
        let desk = AnswerDesk {
            tasks: self.clone(),
            task_id: task_id.to_owned(),
        };
        (desk, answers)
    }

    /// Hands the user's reply to the deferred question `task_id` is waiting
    /// on, in the shape the question form sends. Refused once it is no longer
    /// open: answered, withdrawn, or stopped.
    pub fn answer_question(&self, task_id: &str, reply: String) -> Result<(), String> {
        self.lock()
            .answers
            .get(task_id)
            .ok_or(QUESTION_CLOSED)?
            .try_send(reply)
            .map_err(|_| QUESTION_CLOSED.to_owned())
    }

    /// Deferred questions the main agent is still waiting on.
    pub fn open_questions(&self) -> usize {
        open_questions(&self.lock(), &JobOwner::Main)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::{Duration, Instant};

    use caudra_providers::{ContentBlock, Message, Role};
    use caudra_storage::{
        StateDir,
        background::{JobKind, TaskEvent},
        id::CaudraId,
        sessions::SessionDatabase,
        tool_outputs::ToolOutputStore,
    };
    use futures_lite::future::poll_once;
    use serde_json::json;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{
        BackgroundTasks, CLOSED, FOREIGN_JOB, JobScope, MAX_DEFERRED_QUESTIONS, QUESTION_CAPACITY,
        QUESTION_CLOSED, QuestionJobMetadata, RETRY_MISMATCH, RunningShell, STALE_INVOCATION,
        ShellJobMetadata, WaitNudge,
    };
    use crate::{
        AgentEvent, AgentMode, Envelope, History, SharedBuf, SnapshotLine, StoredSession,
        SubagentHistoryStore, SubagentInfo, TaskCard, TaskProvenance, ToolDoneEvent,
        agent::{
            subagent::{
                RESERVATION_SESSION_MISMATCH, TaskIdentity, TaskOptions, open_task,
                reserve_task_identity,
            },
            task_runner::{ModelResolver, SubagentTaskRunner, TaskRunner, WorkflowHostContext},
        },
        background::{
            ADMISSION_CANCELLED, DELIVERY_CUE, MAX_BATCH_BYTES, MAX_HISTORY_PAGE, MAX_INVOCATIONS,
            MAX_RESULT_BYTES, MAX_SHELL_COMMAND_BYTES, QUESTION_INTERRUPTED,
            tests::{
                ARCHIVE_LIFECYCLE_COUNT, assert_archive_count, hold_writer, seed_archived_tasks,
            },
        },
        background_reminder::render,
        cancel::{CancelMap, CancelToken},
        tools::{
            DEADLINE_EXCEEDED, Deadline, LocalTools, PartialOutputFilter, native::question,
            test_support::stub_ctx,
        },
        types::BACKGROUND_EVENT_RUN_ID,
    };

    const CALL: &str = "shell-call";
    const ROOT: &str = "enclosing-batch";
    const CHILD: &str = "child-invocation";
    const OTHER: &str = "other-invocation";
    const COMMAND: &str = "printf bounded-output";
    const MULTILINE_COMMAND: &str =
        "printf '**literal** `code`'\n\ncat <<'EOF' > destination\n<html> & output\n\nEOF\n";
    const BACKTICK_COMMAND: &str = "cat <<'EOF'\n```\n\n`````embedded````\nEOF\n";
    const COMMAND_FENCE: &str = "```";
    const BACKTICK_COMMAND_FENCE: &str = "``````";
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
    const QUESTION_LABEL: &str = "Cache backend";
    const ANSWER: &str = "Cache backend: SQLite";
    const WITHDRAWN_TEXT: &str = "withdrawn before an answer";
    const QUESTION_KIND: &str = "question";
    const AWAITING_ANSWER: &str = "awaiting answer";
    const LATE_ANSWER: &str = "an answer to a settled question must be refused";
    const GOAL_WAITS: &str = "an open question must count as pending session work";

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
            eta: None,
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

    /// A command that finishes with [`OUTPUT`] once `release` sends.
    fn running_shell() -> (RunningShell, flume::Sender<()>) {
        let (release, released) = flume::bounded::<()>(1);
        let (trigger, cancel) = CancelToken::new();
        let task = smol::spawn(async move {
            let _ = cancel.race(released.recv_async()).await;
            done()
        });
        (RunningShell::new(task, trigger), release)
    }

    #[test]
    fn an_adopted_shell_settles_as_its_job_without_rerunning() {
        smol::block_on(async {
            let fixture = Fixture::new().await;
            let scope = fixture.tasks.main_scope();
            let (running, release) = running_shell();
            let settled = Arc::new(AtomicUsize::new(0));
            let counted = Arc::clone(&settled);
            let card = scope
                .adopt_shell(
                    metadata(),
                    &fixture.history,
                    &CancelToken::none(),
                    Deadline::None,
                    None,
                    running,
                    move |done| async move {
                        counted.fetch_add(1, Ordering::SeqCst);
                        done
                    },
                )
                .await
                .map_err(|(message, _)| message)
                .unwrap();
            release.send(()).unwrap();
            fixture.tasks.join_jobs().await.unwrap();
            assert_eq!(scope.status(&card.task_id).unwrap().state, SUCCEEDED);
            assert_eq!(settled.load(Ordering::SeqCst), 1);
            fixture.tasks.shutdown().await.unwrap();
        });
    }

    #[test_case(false; "admission_closed")]
    #[test_case(true; "call_already_admitted")]
    fn a_refused_adoption_hands_the_command_back_still_running(retried: bool) {
        smol::block_on(async {
            let fixture = Fixture::new().await;
            let scope = fixture.tasks.main_scope();
            if retried {
                scope
                    .admit_shell(metadata(), &fixture.history, |_, _| async { done() })
                    .await
                    .unwrap();
                fixture.tasks.join_jobs().await.unwrap();
            } else {
                fixture.tasks.stop().await.unwrap();
            }
            let (running, release) = running_shell();
            let (message, returned) = scope
                .adopt_shell(
                    metadata(),
                    &fixture.history,
                    &CancelToken::none(),
                    Deadline::None,
                    None,
                    running,
                    |_| async { panic!("a refused adoption cannot settle") },
                )
                .await
                .unwrap_err();
            if retried {
                assert_eq!(message, super::ADOPTION_RETRIED);
            }
            let mut returned = returned.unwrap();
            release.send(()).unwrap();
            let finished = returned.finish(&CancelToken::none()).await;
            assert!(!finished.is_error);
            assert_eq!(finished.output.as_text(), OUTPUT);
            fixture.tasks.shutdown().await.unwrap();
        });
    }

    #[test_case(0; "empty_archive")]
    #[test_case(MAX_HISTORY_PAGE - ARCHIVE_LIFECYCLE_COUNT; "history_page_boundary")]
    #[test_case(MAX_INVOCATIONS - ARCHIVE_LIFECYCLE_COUNT; "lifetime_capacity_boundary")]
    fn archived_shell_retry_keeps_outcome_and_never_replays_factory(archived_count: usize) {
        smol::block_on(async {
            let mut fixture = Fixture::new().await;
            let scope = fixture.tasks.main_scope();
            let mut first = None;
            for index in 0..ARCHIVE_LIFECYCLE_COUNT {
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
            let template = fixture
                .tasks
                .record_invocation(&first.invocation_id)
                .await
                .unwrap();
            seed_archived_tasks(&fixture.dir, fixture.session.id, &template, archived_count);
            let restored = BackgroundTasks::spawn(fixture.dir.clone(), fixture.session.id)
                .await
                .unwrap();
            let count = archived_count + ARCHIVE_LIFECYCLE_COUNT;
            assert_archive_count(&restored, count).await;
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
            restored.join_jobs().await.unwrap();
            restored.shutdown().await.unwrap();
            let gate = restored.0.gate.lock_arc().await;
            drop(restored.archive_settled(gate, None).await.unwrap());
            assert!(restored.lock().records.is_empty());
            assert_archive_count(&restored, count + 1).await;
        });
    }

    /// A settled command's record is archived without its outcome, so the
    /// card the panel is handed afterwards, live or after a resume, must
    /// still carry the time the command took.
    #[test]
    fn an_archived_shell_keeps_its_measured_duration() {
        const RAN_FOR_MS: u64 = 4_100;
        smol::block_on(async {
            let mut fixture = Fixture::new().await;
            let scope = fixture.tasks.main_scope();
            let card = scope
                .admit_shell(metadata(), &fixture.history, |_, _| async {
                    let mut done = done();
                    done.accounting.duration_ms = RAN_FOR_MS;
                    done
                })
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
            assert!(fixture.tasks.lock().records.is_empty());
            let listed = |tasks: &BackgroundTasks| {
                tasks
                    .list()
                    .into_iter()
                    .find(|listed| listed.task_id == card.task_id)
                    .unwrap()
                    .duration_ms
            };
            assert_eq!(listed(&fixture.tasks), Some(RAN_FOR_MS));
            fixture.tasks.shutdown().await.unwrap();
            let restored = BackgroundTasks::spawn(fixture.dir.clone(), fixture.session.id)
                .await
                .unwrap();
            assert_eq!(listed(&restored), Some(RAN_FOR_MS));
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
                        None,
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
                        None,
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
                    None,
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
                        None,
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
                        model: Default::default(),
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

    #[test]
    fn shell_reminders_are_scoped_to_their_owner() {
        smol::block_on(async {
            let fixture = Fixture::new().await;
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

    #[test_case(false, false, COMMAND, COMMAND_FENCE; "main_success")]
    #[test_case(true, false, COMMAND, COMMAND_FENCE; "child_success")]
    #[test_case(false, true, COMMAND, COMMAND_FENCE; "main_failure")]
    #[test_case(true, true, COMMAND, COMMAND_FENCE; "child_failure")]
    #[test_case(false, false, MULTILINE_COMMAND, COMMAND_FENCE; "main_multiline_success")]
    #[test_case(true, false, MULTILINE_COMMAND, COMMAND_FENCE; "child_multiline_success")]
    #[test_case(false, true, MULTILINE_COMMAND, COMMAND_FENCE; "main_multiline_failure")]
    #[test_case(true, true, MULTILINE_COMMAND, COMMAND_FENCE; "child_multiline_failure")]
    #[test_case(false, false, BACKTICK_COMMAND, BACKTICK_COMMAND_FENCE; "main_backticks_success")]
    #[test_case(true, false, BACKTICK_COMMAND, BACKTICK_COMMAND_FENCE; "child_backticks_success")]
    #[test_case(false, true, BACKTICK_COMMAND, BACKTICK_COMMAND_FENCE; "main_backticks_failure")]
    #[test_case(true, true, BACKTICK_COMMAND, BACKTICK_COMMAND_FENCE; "child_backticks_failure")]
    fn shell_envelopes_validate_owner_and_deliver_literal_output(
        child: bool,
        failed: bool,
        command: &str,
        fence: &str,
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
                    "Shell {}: {status}.\n\nCommand:\n{fence}bash\n{command}\n{fence}\n\n{LITERAL_OUTPUT}",
                    card.task_id
                )
            );
            fixture.tasks.shutdown().await.unwrap();
        });
    }

    #[test_case(false, false; "main_shell")]
    #[test_case(true, false; "child_shell")]
    #[test_case(false, true; "main_shell_longest_fence")]
    #[test_case(true, true; "child_shell_longest_fence")]
    fn oversized_shell_command_does_not_block_delivery(child: bool, longest_fence: bool) {
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
            let prefix = if longest_fence {
                "`".repeat(MAX_SHELL_COMMAND_BYTES)
            } else {
                MULTIBYTE.repeat(MAX_SHELL_COMMAND_BYTES / MULTIBYTE.len())
            };
            let command = format!(
                "{prefix}{}",
                MULTIBYTE.repeat(MAX_BATCH_BYTES / MULTIBYTE.len() + 1)
            );
            let fence = if longest_fence {
                "`".repeat(MAX_SHELL_COMMAND_BYTES + 1)
            } else {
                COMMAND_FENCE.into()
            };
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
            assert_eq!(
                result,
                format!(
                    "Shell {}: success.\n\nCommand:\n{fence}bash\n{prefix}\n{TRUNCATED}\n{fence}\n\n{output}",
                    card.task_id
                )
            );
            assert!(result.len() < MAX_BATCH_BYTES);
            assert!(
                messages
                    .iter()
                    .map(|message| message.first_text_content().unwrap().len())
                    .sum::<usize>()
                    < MAX_BATCH_BYTES
            );
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
                super::JOB_PANIC
            );
            assert_eq!(
                fixture.tasks.status(&card.task_id).unwrap().result,
                Some(json!({"error": super::JOB_PANIC}))
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

    #[derive(Clone, Copy)]
    enum AfterDrain {
        Reopen,
        StayClosed,
        ReopenFromOldGeneration,
    }

    #[test_case(AfterDrain::Reopen, None; "reopened_owner_admits_and_delivers")]
    #[test_case(AfterDrain::StayClosed, Some(CLOSED); "drained_owner_stays_closed")]
    #[test_case(AfterDrain::ReopenFromOldGeneration, Some(CLOSED); "old_generation_cannot_reopen")]
    fn reopening_a_drained_child_owner(after: AfterDrain, refusal: Option<&str>) {
        smol::block_on(async {
            let fixture = Fixture::new().await;
            let old = fixture.tasks.child_scope(CHILD);
            if matches!(after, AfterDrain::ReopenFromOldGeneration) {
                fixture.tasks.stop().await.unwrap();
                fixture.tasks.rearm();
            }
            let scope = fixture.tasks.child_scope(CHILD);
            scope
                .admit_shell(metadata(), &fixture.history, |_, _| async { done() })
                .await
                .unwrap();
            scope.cancel_and_drain().await.unwrap();
            match after {
                AfterDrain::Reopen => scope.reopen(),
                AfterDrain::StayClosed => {}
                AfterDrain::ReopenFromOldGeneration => old.reopen(),
            }
            let next = ShellJobMetadata {
                call_id: OTHER.into(),
                ..metadata()
            };
            let admitted = scope
                .admit_shell(next, &fixture.history, |_, _| async { done() })
                .await;
            assert_eq!(admitted.as_ref().err().map(String::as_str), refusal);
            if admitted.is_ok() {
                fixture.tasks.join_jobs().await.unwrap();
                scope.settle_launches(&[receipt()]).await.unwrap();
                assert!(!scope.claim_messages().unwrap().is_empty());
            }
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

    fn question(call: &str) -> QuestionJobMetadata {
        QuestionJobMetadata {
            call_id: call.into(),
            root_call_id: call.into(),
            label: QUESTION_LABEL.into(),
        }
    }

    fn question_receipt(call: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: call.into(),
                content: "deferred".into(),
                is_error: false,
                output_ref: None,
            }],
            ..Default::default()
        }
    }

    /// Admits a question whose job settles with the reply it is handed, or
    /// as withdrawn when cancelled. `opened` fires once its answer desk is
    /// open.
    async fn ask(
        fixture: &Fixture,
        call: &str,
        opened: flume::Sender<()>,
    ) -> Result<TaskCard, String> {
        let call_id = call.to_owned();
        fixture
            .tasks
            .main_scope()
            .admit_question(
                question(call),
                &fixture.history,
                &CancelToken::none(),
                Deadline::None,
                move |cancel, _, answers| async move {
                    let _ = opened.send(());
                    let mut done = match cancel.race(answers.recv_async()).await {
                        Ok(Ok(reply)) => ToolDoneEvent::error(call_id, reply),
                        _ => ToolDoneEvent::error(call_id, WITHDRAWN_TEXT),
                    };
                    done.is_error = cancel.is_cancelled();
                    done
                },
            )
            .await
    }

    fn delivered(tasks: &BackgroundTasks) -> String {
        tasks
            .claim_messages()
            .unwrap()
            .iter()
            .find(|message| message.task_event.is_some())
            .and_then(Message::first_text_content)
            .map(str::to_owned)
            .expect("the question settled with a delivery")
    }

    #[test]
    fn a_deferred_question_takes_its_answer_from_its_desk() {
        smol::block_on(async {
            let fixture = Fixture::new().await;
            let (opened, open) = flume::unbounded();
            let card = ask(&fixture, CALL, opened).await.unwrap();
            assert_eq!(
                (card.kind, card.label.as_str()),
                (JobKind::Question, QUESTION_LABEL)
            );
            open.recv_async().await.unwrap();
            assert_eq!(fixture.tasks.open_questions(), 1);

            fixture
                .tasks
                .answer_question(&card.task_id, ANSWER.into())
                .unwrap();
            fixture.tasks.join_jobs().await.unwrap();
            fixture
                .tasks
                .main_scope()
                .settle_launches(&[question_receipt(CALL)])
                .await
                .unwrap();

            assert_eq!(fixture.tasks.open_questions(), 0);
            assert_eq!(
                fixture
                    .tasks
                    .answer_question(&card.task_id, ANSWER.into())
                    .unwrap_err(),
                QUESTION_CLOSED,
                "{LATE_ANSWER}"
            );
            assert_eq!(
                delivered(&fixture.tasks),
                question::delivery_text(&card.task_id, question::ANSWERED_KIND, ANSWER)
            );
            fixture.tasks.shutdown().await.unwrap();
        });
    }

    #[test]
    fn a_withdrawn_question_is_delivered_as_withdrawn() {
        smol::block_on(async {
            let fixture = Fixture::new().await;
            let (opened, open) = flume::unbounded();
            let card = ask(&fixture, CALL, opened).await.unwrap();
            open.recv_async().await.unwrap();

            fixture
                .tasks
                .main_scope()
                .cancel(&card.task_id)
                .await
                .unwrap();
            fixture.tasks.join_jobs().await.unwrap();
            fixture
                .tasks
                .main_scope()
                .settle_launches(&[question_receipt(CALL)])
                .await
                .unwrap();

            assert_eq!(
                delivered(&fixture.tasks),
                question::delivery_text(&card.task_id, question::WITHDRAWN_KIND, WITHDRAWN_TEXT)
            );
            fixture.tasks.shutdown().await.unwrap();
        });
    }

    #[test]
    fn a_question_past_the_cap_is_refused() {
        smol::block_on(async {
            let fixture = Fixture::new().await;
            let (opened, _open) = flume::unbounded();
            for index in 0..MAX_DEFERRED_QUESTIONS {
                ask(&fixture, &format!("{CALL}-{index}"), opened.clone())
                    .await
                    .unwrap();
            }

            let error = ask(&fixture, CALL, opened).await.unwrap_err();

            assert!(error.starts_with(QUESTION_CAPACITY), "{error}");
            assert_eq!(fixture.tasks.open_questions(), MAX_DEFERRED_QUESTIONS);
            fixture.tasks.shutdown().await.unwrap();
        });
    }

    #[test]
    fn an_open_question_lists_as_waiting_on_the_user() {
        smol::block_on(async {
            let fixture = Fixture::new().await;
            let (opened, open) = flume::unbounded();
            ask(&fixture, CALL, opened).await.unwrap();
            open.recv_async().await.unwrap();

            let (reminder, ..) =
                render(Some(&fixture.tasks.main_scope().reminder_snapshot()), None);

            assert!(reminder.contains(QUESTION_KIND), "{reminder}");
            assert!(reminder.contains(AWAITING_ANSWER), "{reminder}");
            assert!(reminder.contains(QUESTION_LABEL), "{reminder}");
            assert!(fixture.tasks.main_scope().pending(), "{GOAL_WAITS}");
            fixture.tasks.shutdown().await.unwrap();
        });
    }

    #[test]
    fn a_question_open_at_a_crash_restores_as_interrupted() {
        smol::block_on(async {
            let fixture = Fixture::new().await;
            let (opened, open) = flume::unbounded();
            let card = ask(&fixture, CALL, opened).await.unwrap();
            open.recv_async().await.unwrap();
            fixture
                .tasks
                .answer_question(&card.task_id, ANSWER.into())
                .unwrap();
            fixture.tasks.join_jobs().await.unwrap();
            let mut record = fixture.tasks.record(&card.invocation_id).unwrap();
            record.state = "running".into();
            record.outcome = None;
            record.events.clear();
            fixture.tasks.persist(record).await.unwrap();

            let restored = BackgroundTasks::spawn(fixture.dir.clone(), fixture.session.id)
                .await
                .unwrap();

            let status = restored.status(&card.task_id).unwrap();
            assert_eq!(status.state, "interrupted");
            assert_eq!(
                status
                    .result
                    .as_ref()
                    .and_then(|result| result["error"].as_str()),
                Some(QUESTION_INTERRUPTED)
            );
            restored.shutdown().await.unwrap();
        });
    }

    const NUDGE_HEADING: &str = "# Long-running shell";
    const NUDGE_INTERVAL: Duration = Duration::from_millis(1);
    const PRINTED: &str = "Compiling caudra v0.2.0";
    const FILTERED: &str = "[filtered: 1 compile line]";
    const ONE_NUDGE: &str = "a nudge nobody claimed yet is updated, never stacked";
    const NO_CUE: &str = "a nudge alone is not a task report, so it needs no delivery cue";
    const STALE_NUDGE: &str = "a finished shell must not ask whether to keep waiting on it";

    struct HungShell {
        card: TaskCard,
        release: flume::Sender<()>,
    }

    /// A shell that prints [`PRINTED`] and runs until released, asked about
    /// at every [`NUDGE_INTERVAL`] once its receipt reached the owner.
    async fn hung_shell(fixture: &Fixture, scope: &JobScope) -> HungShell {
        let (release, released) = flume::bounded::<()>(1);
        let (attached, printed) = flume::bounded::<()>(1);
        let shells = fixture.tasks.shells().clone();
        let reduce: PartialOutputFilter = Arc::new(|_: &str| FILTERED.to_owned());
        let nudge = WaitNudge {
            interval: NUDGE_INTERVAL,
            reduce: Some(reduce),
            started: Instant::now(),
        };
        let card = scope
            .admit_shell_cancellable(
                metadata(),
                &fixture.history,
                &CancelToken::none(),
                Deadline::None,
                Some(nudge),
                move |_, provenance| async move {
                    let observed = shells.observe_job(&provenance.invocation_id);
                    let buffer = Arc::new(SharedBuf::new());
                    buffer.append(SnapshotLine::plain(PRINTED.into()));
                    observed.live().attach(&buffer);
                    attached.send(()).unwrap();
                    let _ = released.recv_async().await;
                    done()
                },
            )
            .await
            .unwrap();
        scope.settle_launches(&[receipt()]).await.unwrap();
        printed.recv_async().await.unwrap();
        // The second save after the output appeared read it after the first.
        let revision = scope.wait_for_change(scope.revision()).await.unwrap();
        scope.wait_for_change(revision).await.unwrap();
        HungShell { card, release }
    }

    fn nudges(fixture: &Fixture, invocation: &str) -> Vec<TaskEvent> {
        fixture
            .tasks
            .record(invocation)
            .unwrap()
            .events
            .into_iter()
            .filter(|event| event.wait_nudge)
            .collect()
    }

    fn says_nudge(message: &Message) -> bool {
        message
            .first_text_content()
            .is_some_and(|text| text.contains(NUDGE_HEADING))
    }

    #[test_case(false ; "main")]
    #[test_case(true ; "child")]
    fn a_long_shell_asks_its_owner_whether_to_keep_waiting(child: bool) {
        smol::block_on(async {
            let fixture = Fixture::new().await;
            let scope = if child {
                fixture.tasks.child_scope(CHILD)
            } else {
                fixture.tasks.main_scope()
            };
            let shell = hung_shell(&fixture, &scope).await;
            assert_eq!(
                nudges(&fixture, &shell.card.invocation_id).len(),
                1,
                "{ONE_NUDGE}"
            );

            let messages = scope.claim_messages().unwrap();
            assert_eq!(messages.len(), 1, "{NO_CUE}");
            let nudge = &messages[0];
            assert!(nudge.task_event.is_some());
            let text = nudge.first_text_content().unwrap();
            for expected in [
                NUDGE_HEADING,
                shell.card.task_id.as_str(),
                COMMAND,
                FILTERED,
            ] {
                assert!(text.contains(expected), "{expected} missing: {text}");
            }
            assert!(!text.contains(PRINTED), "the output is filtered: {text}");

            shell.release.send(()).unwrap();
            fixture.tasks.join_jobs().await.unwrap();
            let delivered = scope.claim_messages().unwrap();
            assert!(!delivered.iter().any(says_nudge), "{STALE_NUDGE}");
            assert!(delivered.iter().any(|message| message.task_event.is_some()));
            fixture.tasks.shutdown().await.unwrap();
        });
    }

    #[test]
    fn a_finished_shell_drops_the_nudge_nobody_claimed() {
        smol::block_on(async {
            let fixture = Fixture::new().await;
            let scope = fixture.tasks.main_scope();
            let shell = hung_shell(&fixture, &scope).await;
            shell.release.send(()).unwrap();
            fixture.tasks.join_jobs().await.unwrap();

            let stale = nudges(&fixture, &shell.card.invocation_id);
            assert!(stale.iter().all(|event| event.suppressed), "{STALE_NUDGE}");
            let delivered = scope.claim_messages().unwrap();
            assert!(!delivered.iter().any(says_nudge), "{STALE_NUDGE}");
            assert_eq!(
                delivered[0].first_text_content(),
                Some(DELIVERY_CUE),
                "a result keeps its delivery cue"
            );
            fixture.tasks.shutdown().await.unwrap();
        });
    }
}
