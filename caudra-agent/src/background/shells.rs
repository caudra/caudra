//! Session-wide observation and control of foreground native shell
//! executions, from every agent the session runs. It watches and stops them
//! and never schedules one: a foreground shell stays owned by the call that
//! started it, and a background shell by its job record.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use arc_swap::ArcSwap;
use caudra_storage::{
    StateDir,
    auth::now_millis,
    id::CaudraId,
    sessions::{RuntimeRetry, SessionDatabase},
    shell_history::{
        MAX_SHELL_COMMAND_BYTES, MAX_SHELL_EXECUTIONS, MAX_SHELL_OUTPUT_BYTES,
        MAX_SHELL_REASON_BYTES, MAX_SHELL_SUMMARY_BYTES, MAX_SHELL_WORKDIR_BYTES,
        ShellExecutionOwner, ShellExecutionRecord, ShellExecutionState, ShellWorkflowOwner,
    },
};
use event_listener::Event;
use serde_json::Value;
use tracing::warn;

use super::jobs::JobScope;
use crate::{
    CancelToken, CancelTrigger, SharedBuf, SnapshotLine, TextOutput, ToolDoneEvent, ToolOutput,
    tools::{Deadline, ToolContext},
};

const COMMAND_FIELD: &str = "command";
const WORKDIR_FIELD: &str = "workdir";
const DEFAULT_WORKDIR: &str = ".";
const STREAM_BUDGET: usize = MAX_SHELL_OUTPUT_BYTES / 8;
pub(super) const SHELL_INTERRUPTED: &str = "the session ended before this command settled; its outcome is unknown and it was not run again";
const CLOSING: &str = "the session is closing, so the shell command was not started";
const UNRECORDED: &str = "shell history could not record the command, so it was not started";
const CANCELLED: &str = "cancelled";
const OWNER_LEFT: &str = "the caller stopped waiting before the command settled";
const SETTLED: &str = "the shell command has already settled";
const UNKNOWN: &str = "unknown shell execution";

/// Where an executor publishes one execution's live output.
#[derive(Clone, Default)]
pub struct ShellLive(Arc<OnceLock<Arc<SharedBuf>>>);

impl ShellLive {
    pub fn attach(&self, buffer: &Arc<SharedBuf>) {
        let _ = self.0.set(Arc::clone(buffer));
    }

    /// Reads through [`SharedBuf::read`], so the dirty flag and change
    /// callback stay with the transcript that owns them.
    pub fn lines(&self) -> Option<Arc<Vec<SnapshotLine>>> {
        self.0.get().map(|buffer| buffer.read())
    }
}

#[derive(Clone)]
pub enum ShellOutputView {
    Live(ShellLive),
    /// Kept by an earlier runtime and not read yet.
    Stored,
    Loading,
    Kept(Arc<ToolOutput>),
    /// Nothing was kept, or what was kept no longer reads.
    Missing,
}

#[derive(Clone)]
pub struct ShellView {
    pub record: ShellExecutionRecord,
    pub output: ShellOutputView,
    /// Why history could not keep how this execution ended. The record still
    /// reports the real outcome.
    pub history_error: Option<String>,
}

#[derive(Default)]
pub struct ShellSnapshot {
    /// Active executions oldest first, then settled ones newest first.
    pub executions: Vec<Arc<ShellView>>,
    /// Live output of background shell jobs, by invocation id.
    pub jobs: BTreeMap<String, ShellLive>,
}

impl ShellSnapshot {
    pub fn active_count(&self) -> usize {
        self.executions
            .iter()
            .filter(|view| view.record.state.is_active())
            .count()
    }
}

#[derive(Clone)]
pub struct ShellExecutions(Arc<Inner>);

struct Inner {
    dir: StateDir,
    session: CaudraId,
    state: Mutex<State>,
    published: ArcSwap<ShellSnapshot>,
    changed: Event,
}

#[derive(Default)]
struct State {
    active: BTreeMap<String, Active>,
    settled: VecDeque<Arc<ShellView>>,
    jobs: BTreeMap<String, ShellLive>,
    writes: usize,
    closed: bool,
}

struct Active {
    view: ShellView,
    trigger: Option<CancelTrigger>,
}

/// Counts a write until its task ends, however it ends, so a drain never
/// waits on a write that was abandoned.
struct PendingWrite(ShellExecutions);

impl Drop for PendingWrite {
    fn drop(&mut self) {
        self.0.lock().writes -= 1;
        self.0.0.changed.notify(usize::MAX);
    }
}

impl ShellExecutions {
    pub(super) fn restore(
        dir: StateDir,
        session: CaudraId,
        records: Vec<ShellExecutionRecord>,
    ) -> Self {
        let executions = Self(Arc::new(Inner {
            dir,
            session,
            state: Mutex::new(State {
                settled: records
                    .into_iter()
                    .map(|record| {
                        Arc::new(ShellView {
                            record,
                            output: ShellOutputView::Stored,
                            history_error: None,
                        })
                    })
                    .collect(),
                ..State::default()
            }),
            published: ArcSwap::from_pointee(ShellSnapshot::default()),
            changed: Event::new(),
        }));
        executions.publish(&executions.lock());
        executions
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// Called with the state locked, so two publishes cannot land out of order.
    fn publish(&self, state: &State) {
        let mut executions: Vec<_> = state
            .active
            .values()
            .map(|active| Arc::new(active.view.clone()))
            .collect();
        executions.sort_by_key(|view| view.record.created_at_ms);
        executions.extend(state.settled.iter().cloned());
        self.0.published.store(Arc::new(ShellSnapshot {
            executions,
            jobs: state.jobs.clone(),
        }));
        self.0.changed.notify(usize::MAX);
    }

    /// A fresh `Arc` after every change, so a watcher compares pointers.
    pub fn snapshot(&self) -> Arc<ShellSnapshot> {
        self.0.published.load_full()
    }

    pub(super) fn pending(&self) -> bool {
        let state = self.lock();
        !state.active.is_empty() || state.writes > 0
    }

    /// Stops this execution and nothing else: its caller gets the ordinary
    /// cancelled result and carries on.
    pub fn cancel(&self, execution_id: &str) -> Result<(), String> {
        let trigger = {
            let mut state = self.lock();
            let Some(active) = state.active.get_mut(execution_id) else {
                let settled = state
                    .settled
                    .iter()
                    .any(|view| view.record.execution_id == execution_id);
                return Err(if settled { SETTLED } else { UNKNOWN }.into());
            };
            let trigger = active.trigger.take();
            active.view.record.state = ShellExecutionState::Cancelling;
            self.publish(&state);
            trigger
        };
        drop(trigger);
        Ok(())
    }

    /// Reads what an earlier runtime kept of a settled execution, once.
    pub fn load_output(&self, execution_id: &str) {
        {
            let mut state = self.lock();
            let Some(view) = state
                .settled
                .iter_mut()
                .find(|view| view.record.execution_id == execution_id)
            else {
                return;
            };
            if !matches!(view.output, ShellOutputView::Stored) {
                return;
            }
            Arc::make_mut(view).output = ShellOutputView::Loading;
            self.publish(&state);
        }
        let executions = self.clone();
        let execution_id = execution_id.to_owned();
        smol::spawn(async move {
            let dir = executions.0.dir.clone();
            let session = executions.0.session;
            let id = execution_id.clone();
            let loaded = smol::unblock(move || {
                let retry = RuntimeRetry::new(None, &|| false);
                SessionDatabase::open_runtime(&dir, &retry)
                    .and_then(|database| database.shell_execution_output(session, &id))
                    .map_err(|error| error.to_string())
            })
            .await;
            let output = match loaded.and_then(|output| {
                output
                    .map(serde_json::from_value::<ToolOutput>)
                    .transpose()
                    .map_err(|error| error.to_string())
            }) {
                Ok(Some(output)) => ShellOutputView::Kept(Arc::new(output)),
                Ok(None) => ShellOutputView::Missing,
                Err(error) => {
                    warn!(%execution_id, %error, "shell history output unreadable");
                    ShellOutputView::Missing
                }
            };
            executions.update_settled(&execution_id, |view| view.output = output);
        })
        .detach();
    }

    fn update_settled(&self, execution_id: &str, update: impl FnOnce(&mut ShellView)) {
        let mut state = self.lock();
        if let Some(view) = state
            .settled
            .iter_mut()
            .find(|view| view.record.execution_id == execution_id)
        {
            update(Arc::make_mut(view));
            self.publish(&state);
        }
    }

    /// Publishes a background shell job's live output under its invocation
    /// for as long as the returned guard lives.
    pub(crate) fn observe_job(&self, invocation_id: &str) -> ObservedJob {
        let live = ShellLive::default();
        let mut state = self.lock();
        state.jobs.insert(invocation_id.to_owned(), live.clone());
        self.publish(&state);
        ObservedJob {
            executions: self.clone(),
            invocation_id: invocation_id.to_owned(),
            live,
        }
    }

    async fn begin(
        &self,
        ctx: &ToolContext,
        record: ShellExecutionRecord,
    ) -> Result<ShellExecution, String> {
        let (trigger, cancel) = ctx.cancel.child();
        let live = ShellLive::default();
        {
            let mut state = self.lock();
            if state.closed {
                return Err(CLOSING.into());
            }
            state.active.insert(
                record.execution_id.clone(),
                Active {
                    view: ShellView {
                        record: record.clone(),
                        output: ShellOutputView::Live(live.clone()),
                        history_error: None,
                    },
                    trigger: Some(trigger),
                },
            );
            self.publish(&state);
        }
        let released = ShellExecutionRecord {
            state: ShellExecutionState::Running,
            started: true,
            ..record.clone()
        };
        let mut execution = ShellExecution {
            executions: self.clone(),
            execution_id: record.execution_id,
            recording: Some(smol::spawn(self.write(
                released,
                None,
                ctx.deadline,
                Some(cancel.clone()),
            ))),
            cancel,
            live,
            started: false,
            settled: false,
        };
        match execution.recorded().await {
            Ok(_) => {
                execution.started = true;
                let mut state = self.lock();
                if let Some(active) = state.active.get_mut(&execution.execution_id)
                    && active.view.record.state == ShellExecutionState::Preparing
                {
                    active.view.record.state = ShellExecutionState::Running;
                    active.view.record.started = true;
                    self.publish(&state);
                }
                drop(state);
                Ok(execution)
            }
            Err(error) => {
                let (state, message) = if execution.cancel.is_cancelled() {
                    (ShellExecutionState::Cancelled, CANCELLED.to_owned())
                } else {
                    warn!(execution_id = %execution.execution_id, %error, "shell history refused a command");
                    (
                        ShellExecutionState::Failed,
                        format!("{UNRECORDED}: {error}"),
                    )
                };
                execution.settle(state, Some(message.clone()), None);
                Err(message)
            }
        }
    }

    /// Every write for an execution goes through here, off the async threads.
    /// Only the write that releases a command may be cancelled by it. A write
    /// counts from the moment it is asked for, so a drain cannot slip past one
    /// still queued behind an earlier write of the same execution.
    fn write(
        &self,
        record: ShellExecutionRecord,
        output: Option<Value>,
        deadline: Deadline,
        cancel: Option<CancelToken>,
    ) -> impl Future<Output = Result<bool, String>> + Send + 'static {
        self.lock().writes += 1;
        let pending = PendingWrite(self.clone());
        let dir = self.0.dir.clone();
        let session = self.0.session;
        async move {
            let _pending = pending;
            smol::unblock(move || {
                let cancelled = || cancel.as_ref().is_some_and(CancelToken::is_cancelled);
                let deadline = match deadline {
                    Deadline::None => None,
                    Deadline::At(deadline) => Some(deadline),
                };
                let retry = RuntimeRetry::new(deadline, &cancelled);
                SessionDatabase::open_runtime(&dir, &retry)
                    .and_then(|database| {
                        database.save_shell_execution_runtime(
                            session,
                            &fitted(record),
                            output.as_ref(),
                            &retry,
                        )
                    })
                    .map_err(|error| error.to_string())
            })
            .await
        }
    }

    /// Stops every active execution and waits until each has settled and
    /// every record has been written. Nothing may start afterwards.
    pub(super) async fn shutdown(&self) {
        let triggers: Vec<_> = {
            let mut state = self.lock();
            state.closed = true;
            let triggers = state
                .active
                .values_mut()
                .filter_map(|active| {
                    let trigger = active.trigger.take()?;
                    active.view.record.state = ShellExecutionState::Cancelling;
                    Some(trigger)
                })
                .collect();
            self.publish(&state);
            triggers
        };
        drop(triggers);
        loop {
            let listener = self.0.changed.listen();
            {
                let state = self.lock();
                if state.active.is_empty() && state.writes == 0 {
                    return;
                }
            }
            listener.await;
        }
    }
}

impl JobScope {
    /// Registers one foreground shell and records it before it may run. The
    /// command must run under [`ShellExecution::context`].
    pub(crate) async fn track_shell(
        &self,
        ctx: &ToolContext,
        call_id: &str,
        input: &Value,
        timeout: Duration,
        remote: bool,
    ) -> Result<ShellExecution, String> {
        let command = input
            .get(COMMAND_FIELD)
            .and_then(Value::as_str)
            .unwrap_or_default();
        let (command, command_truncated) = prefix(command, MAX_SHELL_COMMAND_BYTES);
        let workdir = input
            .get(WORKDIR_FIELD)
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_WORKDIR);
        let record = ShellExecutionRecord {
            execution_id: CaudraId::generate().to_string(),
            owner: ShellExecutionOwner {
                job: self.owner().clone(),
                task_id: self.task_id().map(str::to_owned),
                workflow: ctx.event_tx.workflow().map(|workflow| ShellWorkflowOwner {
                    run_id: workflow.run_id.clone(),
                    epoch: workflow.epoch,
                    call_key: workflow.call_key,
                }),
            },
            call_id: call_id.to_owned(),
            command: command.to_owned(),
            command_truncated,
            workdir: prefix(workdir, MAX_SHELL_WORKDIR_BYTES).0.to_owned(),
            remote: Some(remote),
            timeout_ms: Some(timeout.as_millis().try_into().unwrap_or(u64::MAX)),
            created_at_ms: now_millis(),
            finished_at_ms: None,
            state: ShellExecutionState::Preparing,
            started: false,
            reason: None,
        };
        self.shells().begin(ctx, record).await
    }
}

/// One tracked foreground execution. Settles its record exactly once: from
/// the result, or as interrupted when its caller drops it first.
pub(crate) struct ShellExecution {
    executions: ShellExecutions,
    execution_id: String,
    recording: Option<smol::Task<Result<bool, String>>>,
    cancel: CancelToken,
    live: ShellLive,
    started: bool,
    settled: bool,
}

impl ShellExecution {
    /// The caller's context with this execution's own cancellation, which
    /// also fires with the caller's.
    pub(crate) fn context(&self, ctx: &ToolContext) -> ToolContext {
        let mut context = ctx.clone();
        context.cancel = self.cancel.clone();
        context.shell_live = Some(self.live.clone());
        context
    }

    pub(crate) fn finish(mut self, done: &ToolDoneEvent) {
        let state = if done.is_error && self.cancel.is_cancelled() {
            ShellExecutionState::Cancelled
        } else if matches!(&done.output, ToolOutput::Shell(shell) if shell.timed_out) {
            ShellExecutionState::TimedOut
        } else if done.is_error {
            ShellExecutionState::Failed
        } else {
            ShellExecutionState::Succeeded
        };
        self.settle(state, None, history_output(&done.output));
    }

    async fn recorded(&mut self) -> Result<bool, String> {
        let recorded = match &mut self.recording {
            Some(recording) => recording.await,
            None => Ok(true),
        };
        self.recording = None;
        recorded
    }

    fn settle(
        &mut self,
        state: ShellExecutionState,
        reason: Option<String>,
        output: Option<ToolOutput>,
    ) {
        self.settled = true;
        let executions = self.executions.clone();
        let (record, value) = {
            let mut guard = executions.lock();
            let Some(active) = guard.active.remove(&self.execution_id) else {
                return;
            };
            let mut record = active.view.record;
            record.state = state;
            record.started = self.started;
            record.finished_at_ms = Some(now_millis());
            record.reason =
                reason.map(|reason| prefix(&reason, MAX_SHELL_REASON_BYTES).0.to_owned());
            let value = output
                .as_ref()
                .and_then(|output| serde_json::to_value(output).ok());
            guard.settled.push_front(Arc::new(ShellView {
                record: record.clone(),
                output: output.map_or(ShellOutputView::Missing, |output| {
                    ShellOutputView::Kept(Arc::new(output))
                }),
                history_error: None,
            }));
            guard.settled.truncate(MAX_SHELL_EXECUTIONS);
            executions.publish(&guard);
            (record, value)
        };
        let prior = self.recording.take();
        let write = executions.write(record, value, Deadline::None, None);
        let execution_id = self.execution_id.clone();
        smol::spawn(async move {
            if let Some(prior) = prior {
                let _ = prior.await;
            }
            if let Err(error) = write.await {
                warn!(%execution_id, %error, "shell history could not record an outcome");
                executions.update_settled(&execution_id, |view| view.history_error = Some(error));
            }
        })
        .detach();
    }
}

impl Drop for ShellExecution {
    fn drop(&mut self) {
        if !self.settled {
            self.settle(
                ShellExecutionState::Interrupted,
                Some(OWNER_LEFT.into()),
                None,
            );
        }
    }
}

pub(crate) struct ObservedJob {
    executions: ShellExecutions,
    invocation_id: String,
    live: ShellLive,
}

impl ObservedJob {
    pub(crate) fn live(&self) -> ShellLive {
        self.live.clone()
    }
}

impl Drop for ObservedJob {
    fn drop(&mut self) {
        let mut state = self.executions.lock();
        state.jobs.remove(&self.invocation_id);
        self.executions.publish(&state);
    }
}

fn prefix(text: &str, maximum: usize) -> (&str, bool) {
    let end = text.floor_char_boundary(maximum);
    (&text[..end], end < text.len())
}

fn keep_tail(text: &mut String, budget: usize) -> bool {
    if text.len() <= budget {
        return false;
    }
    let start = text.ceil_char_boundary(text.len() - budget);
    text.drain(..start);
    true
}

/// Halves an oversized command until the summary fits, rather than lose the
/// record of a command that ran.
fn fitted(mut record: ShellExecutionRecord) -> ShellExecutionRecord {
    while !record.command.is_empty()
        && serde_json::to_vec(&record).is_ok_and(|summary| summary.len() > MAX_SHELL_SUMMARY_BYTES)
    {
        let keep = record.command.floor_char_boundary(record.command.len() / 2);
        record.command.truncate(keep);
        record.command_truncated = true;
    }
    record
}

/// The output as history keeps it: whole when it fits, otherwise the tails
/// of each stream with their truncation marked.
fn history_output(output: &ToolOutput) -> Option<ToolOutput> {
    let fits = |output: &ToolOutput| {
        serde_json::to_vec(output).is_ok_and(|bytes| bytes.len() <= MAX_SHELL_OUTPUT_BYTES)
    };
    if fits(output) {
        return Some(output.clone());
    }
    let bounded = match output {
        ToolOutput::Shell(shell) => {
            let mut shell = shell.clone();
            shell.stdout_preview_truncated |= keep_tail(&mut shell.stdout, STREAM_BUDGET);
            shell.stderr_preview_truncated |= keep_tail(&mut shell.stderr, STREAM_BUDGET);
            keep_tail(&mut shell.model_text, STREAM_BUDGET);
            ToolOutput::Shell(shell)
        }
        other => {
            let mut text = other.as_text();
            keep_tail(&mut text, STREAM_BUDGET);
            ToolOutput::Plain(TextOutput::from(text))
        }
    };
    fits(&bounded).then_some(bounded)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use caudra_storage::{
        StateDir,
        background::JobOwner,
        sessions::{RuntimeRetry, SessionDatabase},
        shell_history::{
            MAX_SHELL_EXECUTIONS, MAX_SHELL_OUTPUT_BYTES, MAX_SHELL_SUMMARY_BYTES,
            ShellExecutionOwner, ShellExecutionRecord, ShellExecutionState,
        },
    };
    use futures_lite::future::poll_once;
    use serde_json::json;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{
        CANCELLED, CLOSING, OWNER_LEFT, SETTLED, SHELL_INTERRUPTED, ShellExecution,
        ShellExecutions, ShellOutputView, UNKNOWN, UNRECORDED, fitted, history_output,
    };
    use crate::{
        AgentMode, SharedBuf, SnapshotLine, StoredSession, ToolDoneEvent, ToolOutput,
        background::{BackgroundTasks, tests::hold_writer},
        cancel::CancelToken,
        tools::{Deadline, ToolContext, test_support::stub_ctx},
        types::{ShellOutput, WorkflowProvenance},
    };

    const CALL: &str = "tracked-shell-call";
    const COMMAND: &str = "printf tracked";
    const OUTPUT: &str = "tracked output";
    const INVOCATION: &str = "observed-shell-job";
    const MISSING: &str = "no-such-execution";
    const RUNNING_ID: &str = "left-running";
    const FINISHED_ID: &str = "finished-earlier";
    const TASK: &str = "tracked-shell-owner";
    const RUN: &str = "tracked-shell-workflow";
    const EPOCH: u64 = 3;
    const CALL_KEY: u64 = 7;
    const TIMEOUT: Duration = Duration::from_secs(120);

    struct Fixture {
        _temp: TempDir,
        dir: StateDir,
        tasks: BackgroundTasks,
        ctx: ToolContext,
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
            let mut ctx = stub_ctx(&AgentMode::Build);
            ctx.jobs = Some(tasks.main_scope());
            Self {
                _temp: temp,
                dir,
                tasks,
                ctx,
            }
        }

        async fn track(&self) -> Result<ShellExecution, String> {
            self.ctx
                .job_scope()
                .unwrap()
                .track_shell(
                    &self.ctx,
                    CALL,
                    &json!({"command": COMMAND}),
                    TIMEOUT,
                    false,
                )
                .await
        }

        fn shells(&self) -> &ShellExecutions {
            self.tasks.shells()
        }

        fn shown(&self) -> Vec<(ShellExecutionState, bool)> {
            self.shells()
                .snapshot()
                .executions
                .iter()
                .map(|view| (view.record.state, view.record.started))
                .collect()
        }

        fn stored(&self) -> Vec<ShellExecutionRecord> {
            SessionDatabase::open(&self.dir)
                .unwrap()
                .shell_executions(self.tasks.session_id(), None, MAX_SHELL_EXECUTIONS)
                .unwrap()
        }
    }

    fn done(output: ToolOutput, is_error: bool) -> ToolDoneEvent {
        let mut done = ToolDoneEvent::error(CALL.into(), String::new());
        done.output = output;
        done.is_error = is_error;
        done
    }

    fn shell_output(stdout: String, timed_out: bool) -> ToolOutput {
        ToolOutput::Shell(ShellOutput {
            model_text: stdout.clone(),
            relative_workdir: ".".into(),
            timeout_ms: 120_000,
            duration_ms: 1,
            exit_code: (!timed_out).then_some(0),
            signal: None,
            timed_out,
            output_limit_exceeded: false,
            final_sequence: 1,
            stdout_utf8_bytes: stdout.len() as u64,
            stderr_utf8_bytes: 0,
            stdout,
            stderr: String::new(),
            stdout_capture_truncated: false,
            stderr_capture_truncated: false,
            stdout_preview_truncated: false,
            stderr_preview_truncated: false,
            stdout_redraws_collapsed: 0,
            stderr_redraws_collapsed: 0,
            filter: None,
        })
    }

    fn stored_record(
        execution_id: &str,
        created_at_ms: u64,
        state: ShellExecutionState,
    ) -> ShellExecutionRecord {
        ShellExecutionRecord {
            execution_id: execution_id.into(),
            owner: ShellExecutionOwner::default(),
            call_id: CALL.into(),
            command: COMMAND.into(),
            command_truncated: false,
            workdir: ".".into(),
            remote: Some(true),
            timeout_ms: Some(120_000),
            created_at_ms,
            finished_at_ms: None,
            state,
            started: true,
            reason: None,
        }
    }

    async fn loaded(shells: &ShellExecutions, execution_id: &str) -> ShellOutputView {
        loop {
            let listener = shells.0.changed.listen();
            let output = shells
                .snapshot()
                .executions
                .iter()
                .find(|view| view.record.execution_id == execution_id)
                .map(|view| view.output.clone())
                .unwrap();
            if !matches!(output, ShellOutputView::Stored | ShellOutputView::Loading) {
                return output;
            }
            listener.await;
        }
    }

    #[test_case(shell_output(OUTPUT.into(), false), false, ShellExecutionState::Succeeded; "success")]
    #[test_case(shell_output(OUTPUT.into(), false), true, ShellExecutionState::Failed; "failure")]
    #[test_case(shell_output(OUTPUT.into(), true), true, ShellExecutionState::TimedOut; "timeout")]
    fn a_shell_is_recorded_before_it_may_start_and_settles_with_its_result(
        output: ToolOutput,
        is_error: bool,
        expected: ShellExecutionState,
    ) {
        smol::block_on(async {
            let fixture = Fixture::new().await;
            let execution = fixture.track().await.unwrap();
            let stored = fixture.stored();
            assert_eq!(stored.len(), 1);
            assert_eq!(stored[0].state, ShellExecutionState::Running);
            assert!(stored[0].started);
            assert_eq!(stored[0].command, COMMAND);
            assert_eq!(stored[0].owner.job, JobOwner::Main);
            assert_eq!(stored[0].remote, Some(false));
            assert_eq!(fixture.shown(), [(ShellExecutionState::Running, true)]);
            assert_eq!(fixture.shells().snapshot().active_count(), 1);
            execution.finish(&done(output.clone(), is_error));
            assert_eq!(fixture.shown(), [(expected, true)]);
            assert_eq!(fixture.shells().snapshot().active_count(), 0);
            fixture.tasks.shutdown().await.unwrap();
            let stored = fixture.stored();
            assert_eq!(stored[0].state, expected);
            assert!(stored[0].finished_at_ms.is_some());
            let kept = SessionDatabase::open(&fixture.dir)
                .unwrap()
                .shell_execution_output(fixture.tasks.session_id(), &stored[0].execution_id)
                .unwrap()
                .unwrap();
            assert_eq!(kept, serde_json::to_value(&output).unwrap());
        });
    }

    #[test_case(None, None; "main")]
    #[test_case(Some(TASK), None; "child")]
    #[test_case(Some(TASK), Some(RUN); "workflow")]
    fn a_shell_records_the_agent_that_owns_it(task: Option<&str>, run: Option<&str>) {
        smol::block_on(async {
            let mut fixture = Fixture::new().await;
            if let Some(task) = task {
                fixture.ctx.jobs = Some(fixture.tasks.child_scope(INVOCATION).for_task(task));
            }
            if let Some(run) = run {
                fixture.ctx.event_tx =
                    fixture
                        .ctx
                        .event_tx
                        .clone()
                        .with_workflow(WorkflowProvenance {
                            run_id: run.into(),
                            epoch: EPOCH,
                            call_key: CALL_KEY,
                            phase: None,
                        });
            }
            let execution = fixture.track().await.unwrap();
            let owner = fixture.shells().snapshot().executions[0]
                .record
                .owner
                .clone();
            assert_eq!(
                owner.job,
                task.map_or(JobOwner::Main, |_| JobOwner::Child {
                    invocation_id: INVOCATION.into()
                })
            );
            assert_eq!(owner.task_id.as_deref(), task);
            assert_eq!(
                owner
                    .workflow
                    .map(|workflow| (workflow.run_id, workflow.epoch, workflow.call_key)),
                run.map(|run| (run.to_owned(), EPOCH, CALL_KEY))
            );
            execution.finish(&done(shell_output(OUTPUT.into(), false), false));
            fixture.tasks.shutdown().await.unwrap();
        });
    }

    #[test_case(false; "stop")]
    #[test_case(true; "caller_cancelled")]
    fn stopping_one_shell_leaves_its_caller_and_siblings_running(caller: bool) {
        smol::block_on(async {
            let mut fixture = Fixture::new().await;
            let (trigger, token) = CancelToken::new();
            fixture.ctx.cancel = token;
            let first = fixture.track().await.unwrap();
            let second = fixture.track().await.unwrap();
            let first_context = first.context(&fixture.ctx);
            let second_context = second.context(&fixture.ctx);
            let stopped = first.execution_id.clone();
            if caller {
                trigger.cancel();
                assert!(second_context.cancel.is_cancelled());
            } else {
                fixture.shells().cancel(&stopped).unwrap();
                assert!(!second_context.cancel.is_cancelled());
                assert!(!fixture.ctx.cancel.is_cancelled());
                assert_eq!(
                    fixture.shown(),
                    [
                        (ShellExecutionState::Cancelling, true),
                        (ShellExecutionState::Running, true)
                    ]
                );
            }
            assert!(first_context.cancel.is_cancelled());
            first.finish(&done(ToolOutput::Plain(CANCELLED.into()), true));
            assert_eq!(fixture.shells().cancel(&stopped), Err(SETTLED.into()));
            assert_eq!(fixture.shells().cancel(MISSING), Err(UNKNOWN.into()));
            second.finish(&done(shell_output(OUTPUT.into(), false), false));
            fixture.tasks.shutdown().await.unwrap();
            let mut states: Vec<_> = fixture.stored().iter().map(|record| record.state).collect();
            states.sort_by_key(|state| format!("{state:?}"));
            assert_eq!(
                states,
                [
                    ShellExecutionState::Cancelled,
                    ShellExecutionState::Succeeded
                ]
            );
        });
    }

    #[test]
    fn a_shell_its_caller_abandons_is_recorded_as_interrupted() {
        smol::block_on(async {
            let fixture = Fixture::new().await;
            drop(fixture.track().await.unwrap());
            let snapshot = fixture.shells().snapshot();
            assert_eq!(
                snapshot.executions[0].record.state,
                ShellExecutionState::Interrupted
            );
            assert_eq!(
                snapshot.executions[0].record.reason.as_deref(),
                Some(OWNER_LEFT)
            );
            fixture.tasks.shutdown().await.unwrap();
            let stored = fixture.stored();
            assert_eq!(stored[0].state, ShellExecutionState::Interrupted);
            assert_eq!(stored[0].reason.as_deref(), Some(OWNER_LEFT));
        });
    }

    #[test]
    fn shutdown_stops_and_awaits_every_shell_then_refuses_new_ones() {
        smol::block_on(async {
            let fixture = Fixture::new().await;
            let execution = fixture.track().await.unwrap();
            let context = execution.context(&fixture.ctx);
            let shells = fixture.shells().clone();
            let mut shutdown = Box::pin(shells.shutdown());
            assert!(poll_once(shutdown.as_mut()).await.is_none());
            assert!(context.cancel.is_cancelled());
            assert_eq!(fixture.shown(), [(ShellExecutionState::Cancelling, true)]);
            assert!(poll_once(shutdown.as_mut()).await.is_none());
            execution.finish(&done(ToolOutput::Plain(CANCELLED.into()), true));
            shutdown.await;
            assert_eq!(fixture.stored()[0].state, ShellExecutionState::Cancelled);
            assert_eq!(fixture.track().await.err().as_deref(), Some(CLOSING));
            fixture.tasks.shutdown().await.unwrap();
        });
    }

    #[test_case(true; "expired_deadline")]
    #[test_case(false; "stopped_while_recording")]
    fn a_shell_whose_record_cannot_be_written_never_starts(deadline: bool) {
        smol::block_on(async {
            let mut fixture = Fixture::new().await;
            if deadline {
                fixture.ctx.deadline = Deadline::after(Duration::ZERO);
            }
            let (release, writer) = hold_writer(fixture.dir.clone()).await;
            let mut tracking = Box::pin(fixture.track());
            if !deadline {
                assert!(poll_once(tracking.as_mut()).await.is_none());
                let snapshot = fixture.shells().snapshot();
                assert_eq!(fixture.shown(), [(ShellExecutionState::Preparing, false)]);
                fixture
                    .shells()
                    .cancel(&snapshot.executions[0].record.execution_id)
                    .unwrap();
            }
            let error = tracking.await.err().unwrap();
            let expected = if deadline {
                ShellExecutionState::Failed
            } else {
                ShellExecutionState::Cancelled
            };
            assert_eq!(error.starts_with(UNRECORDED), deadline, "{error}");
            assert_eq!(error == CANCELLED, !deadline, "{error}");
            assert_eq!(fixture.shown(), [(expected, false)]);
            release.send(()).unwrap();
            writer.await;
            fixture.tasks.shutdown().await.unwrap();
            let stored = fixture.stored();
            assert_eq!(stored[0].state, expected);
            assert!(!stored[0].started);
        });
    }

    #[test]
    fn restore_reports_unfinished_shells_as_interrupted_and_reads_kept_output_on_demand() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let dir = StateDir::from_path(temp.path().to_owned());
            let mut session = StoredSession::new("test-model", temp.path().to_str().unwrap());
            session.save(&dir).unwrap();
            let retry = RuntimeRetry::new(None, &|| false);
            let database = SessionDatabase::open(&dir).unwrap();
            for (record, output) in [
                (
                    stored_record(RUNNING_ID, 1, ShellExecutionState::Running),
                    None,
                ),
                (
                    stored_record(FINISHED_ID, 2, ShellExecutionState::Succeeded),
                    Some(serde_json::to_value(ToolOutput::Plain(OUTPUT.into())).unwrap()),
                ),
            ] {
                assert!(
                    database
                        .save_shell_execution_runtime(session.id, &record, output.as_ref(), &retry)
                        .unwrap()
                );
            }
            let tasks = BackgroundTasks::spawn(dir, session.id).await.unwrap();
            let shells = tasks.shells();
            let snapshot = shells.snapshot();
            assert_eq!(snapshot.active_count(), 0);
            let restored: Vec<_> = snapshot
                .executions
                .iter()
                .map(|view| {
                    assert!(matches!(view.output, ShellOutputView::Stored));
                    (
                        view.record.execution_id.as_str(),
                        view.record.state,
                        view.record.reason.as_deref(),
                    )
                })
                .collect();
            assert_eq!(
                restored,
                [
                    (FINISHED_ID, ShellExecutionState::Succeeded, None),
                    (
                        RUNNING_ID,
                        ShellExecutionState::Interrupted,
                        Some(SHELL_INTERRUPTED)
                    ),
                ]
            );
            assert_eq!(shells.cancel(RUNNING_ID), Err(SETTLED.into()));
            for (execution_id, expected) in [(FINISHED_ID, Some(OUTPUT)), (RUNNING_ID, None)] {
                shells.load_output(execution_id);
                let output = match loaded(shells, execution_id).await {
                    ShellOutputView::Kept(output) => Some(output.as_text()),
                    ShellOutputView::Missing => None,
                    _ => panic!("history output must settle once loaded"),
                };
                assert_eq!(output.as_deref(), expected);
            }
            tasks.shutdown().await.unwrap();
        });
    }

    #[test]
    fn a_background_job_is_observed_only_while_it_runs() {
        smol::block_on(async {
            let fixture = Fixture::new().await;
            let observed = fixture.shells().observe_job(INVOCATION);
            let buffer = Arc::new(SharedBuf::new());
            observed.live().attach(&buffer);
            buffer.append(SnapshotLine::plain(OUTPUT.into()));
            let snapshot = fixture.shells().snapshot();
            assert!(snapshot.executions.is_empty());
            assert_eq!(snapshot.active_count(), 0);
            let lines = snapshot.jobs[INVOCATION].lines().unwrap();
            assert_eq!(lines[0].spans[0].text, OUTPUT);
            assert!(buffer.read_if_dirty().is_some());
            drop(observed);
            assert!(fixture.shells().snapshot().jobs.is_empty());
            fixture.tasks.shutdown().await.unwrap();
        });
    }

    #[test_case(OUTPUT.len(), false; "fits")]
    #[test_case(MAX_SHELL_OUTPUT_BYTES, true; "oversized")]
    fn history_keeps_whole_output_that_fits_and_stream_tails_otherwise(size: usize, bounded: bool) {
        let output = shell_output("x".repeat(size), false);
        let kept = history_output(&output).unwrap();
        assert!(serde_json::to_vec(&kept).unwrap().len() <= MAX_SHELL_OUTPUT_BYTES);
        let ToolOutput::Shell(shell) = kept else {
            panic!("shell output keeps its shape");
        };
        assert_eq!(shell.stdout.len() < size, bounded);
        assert_eq!(shell.stdout_preview_truncated, bounded);
    }

    #[test]
    fn an_oversized_summary_keeps_a_marked_prefix_of_its_command() {
        let mut oversized = stored_record(RUNNING_ID, 1, ShellExecutionState::Running);
        oversized.command = "é".repeat(MAX_SHELL_SUMMARY_BYTES);
        let kept = fitted(oversized);
        assert!(serde_json::to_vec(&kept).unwrap().len() <= MAX_SHELL_SUMMARY_BYTES);
        assert!(kept.command_truncated);
        assert!(!kept.command.is_empty());
    }
}
