//! One execution attempt of a workflow run. The Rhai engine blocks a thread
//! and every host call it makes crosses a channel to the async driver, which
//! owns the run's durable state for this epoch. Every write the driver issues
//! is conditional on the epoch it started under, so once a pause, stop, or
//! shutdown has committed the next epoch the store refuses whatever the
//! attempt still had in flight.

use std::sync::Arc;

use caudra_providers::ModelPurpose;
use caudra_storage::workflow::{
    WorkflowCallFinish, WorkflowCallKind, WorkflowCallStart, WorkflowEventKind, WorkflowRunPatch,
    WorkflowUpdate,
};
use caudra_workflow::{
    AgentRequest, AgentResult, AgentRosterEntry, CallKey, CallKind, CapabilityMode, EngineLimits,
    HostError, Journal, LogLine, MAX_PHASE_HISTORY, MAX_RUN_LOG_ENTRIES, ModelJob, PhaseRecord,
    RhaiEngine, RosterState, RunParams, RunSnapshot, RunStatus, WorkflowEngine, WorkflowError,
    WorkflowEvent, WorkflowHost, WorkflowOutcome, agent_request_value, hash_request,
    scratch_request_value,
};
use flume::{Receiver, Sender};
use futures_lite::future;
use serde_json::Value;
use tracing::{info, warn};

use super::state::{Published, now_secs, publish, snapshot_from_row, stored_status};
use super::store::WorkflowStore;
use crate::agent::subagent::TaskIdentity;
use crate::agent::task_runner::{ModeResolver, TaskOutcome, TaskRequest, TaskRunner};
use crate::cancel::{CancelToken, CancelTrigger};
use crate::subagent_history::SubagentTaskMode;
use crate::types::{AgentEvent, Envelope, EventSender, WORKFLOW_EVENT_RUN_ID, WorkflowProvenance};

const AGENT_LABEL_PREFIX: &str = "agent-";
const TASK_ID_SEPARATOR: &str = "-";
const CALL_ID_SEPARATOR: &str = ":";
const CANCELLED_CALL_ERROR: &str = "cancelled";
const AGENT_FAILED_PREFIX: &str = "agent failed: ";
const RUN_ROW_MISSING: &str = "workflow run row disappeared";
const KEY_OVERFLOW: &str = "workflow call key overflowed";
const ENGINE_LOST: &str = "workflow engine stopped without reporting an outcome";

/// What every attempt shares with the runtime that launched it.
#[derive(Clone)]
pub(super) struct RunEnv {
    pub store: WorkflowStore,
    pub runner: Arc<dyn TaskRunner>,
    pub events: Sender<Envelope>,
    pub mode: ModeResolver,
    pub published: Published,
}

/// The immutable inputs of one attempt: the script and args the row holds,
/// plus what earlier attempts already committed.
pub(super) struct RunSpec {
    pub source: String,
    pub args: Value,
    pub journal: Journal,
}

/// Ends an attempt early. `status` is committed under the next epoch before
/// anything is cancelled; `reply` answers once the engine and every agent the
/// attempt started have stopped.
pub(super) struct Interrupt {
    pub status: RunStatus,
    pub reply: Sender<RunSnapshot>,
}

pub(super) struct ActiveRun {
    pub control: Sender<Interrupt>,
    pub task: smol::Task<()>,
}

enum HostCommand {
    Agent {
        key: CallKey,
        request: AgentRequest,
        reply: Sender<Result<AgentResult, HostError>>,
    },
    Parallel {
        first_key: CallKey,
        requests: Vec<AgentRequest>,
        reply: Sender<Result<Vec<AgentResult>, HostError>>,
    },
    Phase {
        title: String,
        reply: Sender<()>,
    },
    Log {
        message: String,
        reply: Sender<()>,
    },
    Scratch {
        key: CallKey,
        name: String,
        content: String,
        reply: Sender<Result<String, HostError>>,
    },
    Finished(WorkflowOutcome),
}

/// The engine's view of the host: each call ships to the driver and blocks
/// the interpreter thread until the driver answers. A closed channel means
/// the driver is gone, which the script sees as cancellation.
struct HostAdapter {
    commands: Sender<HostCommand>,
    cancel: CancelToken,
}

impl HostAdapter {
    fn ask<T>(&self, command: impl FnOnce(Sender<T>) -> HostCommand) -> Result<T, HostError> {
        let (reply, answer) = flume::bounded(1);
        self.commands
            .send(command(reply))
            .map_err(|_| HostError::Cancelled)?;
        answer.recv().map_err(|_| HostError::Cancelled)
    }
}

impl WorkflowHost for HostAdapter {
    fn agent(&self, key: CallKey, request: &AgentRequest) -> Result<AgentResult, HostError> {
        self.ask(|reply| HostCommand::Agent {
            key,
            request: request.clone(),
            reply,
        })?
    }

    fn parallel(
        &self,
        first_key: CallKey,
        requests: &[AgentRequest],
    ) -> Result<Vec<AgentResult>, HostError> {
        self.ask(|reply| HostCommand::Parallel {
            first_key,
            requests: requests.to_vec(),
            reply,
        })?
    }

    fn phase(&self, title: &str) {
        let _ = self.ask(|reply| HostCommand::Phase {
            title: title.to_owned(),
            reply,
        });
    }

    fn log(&self, message: &str) {
        let _ = self.ask(|reply| HostCommand::Log {
            message: message.to_owned(),
            reply,
        });
    }

    fn write_scratch_file(
        &self,
        key: CallKey,
        name: &str,
        content: &str,
    ) -> Result<String, HostError> {
        self.ask(|reply| HostCommand::Scratch {
            key,
            name: name.to_owned(),
            content: content.to_owned(),
            reply,
        })?
    }

    fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
}

struct AgentDone {
    key: CallKey,
    outcome: TaskOutcome,
}

/// The host call the engine is blocked on.
enum Pending {
    Agent {
        reply: Sender<Result<AgentResult, HostError>>,
    },
    Parallel {
        first_key: CallKey,
        results: Vec<Option<AgentResult>>,
        reply: Sender<Result<Vec<AgentResult>, HostError>>,
    },
}

impl Pending {
    fn cancel(self) {
        match self {
            Self::Agent { reply } => drop(reply.send(Err(HostError::Cancelled))),
            Self::Parallel { reply, .. } => drop(reply.send(Err(HostError::Cancelled))),
        }
    }
}

enum Event {
    Host(Result<HostCommand, flume::RecvError>),
    Control(Result<Interrupt, flume::RecvError>),
    Agent(AgentDone),
}

pub(super) fn launch(
    env: RunEnv,
    snapshot: RunSnapshot,
    spec: RunSpec,
    root: &CancelToken,
) -> ActiveRun {
    let (trigger, cancel) = root.child();
    let (commands, host_rx) = flume::unbounded();
    let (control, control_rx) = flume::unbounded();
    let host = HostAdapter {
        commands,
        cancel: cancel.clone(),
    };
    let agent_budget = snapshot.agent_budget;
    let engine = smol::unblock(move || {
        let outcome = RhaiEngine.run(RunParams {
            source: &spec.source,
            args: &spec.args,
            journal: &spec.journal,
            host: &host,
            limits: &EngineLimits::default(),
            agent_budget,
        });
        let _ = host.commands.send(HostCommand::Finished(outcome));
    });
    let (agents_tx, agents_rx) = flume::unbounded();
    let driver = Driver {
        epoch: snapshot.execution_epoch,
        env,
        snapshot,
        cancel,
        trigger: Some(trigger),
        agents_tx,
        pending: None,
        in_flight: 0,
        engine_done: false,
        preempted: false,
        waiters: Vec::new(),
    };
    let task = smol::spawn(async move {
        driver.drive(host_rx, control_rx, agents_rx).await;
        engine.await;
    });
    ActiveRun { control, task }
}

struct Driver {
    env: RunEnv,
    /// The working copy; `revision` follows every applied write.
    snapshot: RunSnapshot,
    /// The epoch this attempt runs under. Never bumped here: the interrupt
    /// commit moves the row past it, and that is what makes later writes stale.
    epoch: u64,
    cancel: CancelToken,
    trigger: Option<CancelTrigger>,
    agents_tx: Sender<AgentDone>,
    pending: Option<Pending>,
    in_flight: usize,
    engine_done: bool,
    /// The row moved past this attempt's epoch; nothing more will be written.
    preempted: bool,
    waiters: Vec<Sender<RunSnapshot>>,
}

impl Driver {
    async fn drive(
        mut self,
        host_rx: Receiver<HostCommand>,
        control_rx: Receiver<Interrupt>,
        agents_rx: Receiver<AgentDone>,
    ) {
        let mut host_rx = Some(host_rx);
        let mut control_rx = Some(control_rx);
        while !(self.engine_done && self.in_flight == 0) {
            let event = future::race(
                future::race(
                    async { Event::Host(recv_or_pending(host_rx.as_ref()).await) },
                    async { Event::Control(recv_or_pending(control_rx.as_ref()).await) },
                ),
                async {
                    match agents_rx.recv_async().await {
                        Ok(done) => Event::Agent(done),
                        Err(_) => future::pending().await,
                    }
                },
            )
            .await;
            match event {
                Event::Host(Ok(command)) => self.handle_host(command).await,
                Event::Host(Err(_)) => {
                    host_rx = None;
                    if !self.engine_done {
                        self.finish(WorkflowOutcome::Failed(ENGINE_LOST.to_owned()))
                            .await;
                    }
                }
                Event::Control(Ok(interrupt)) => self.interrupt(interrupt).await,
                Event::Control(Err(_)) => control_rx = None,
                Event::Agent(done) => self.agent_done(done).await,
            }
        }
        if let Err(error) = self.reload().await {
            warn!(run_id = %self.snapshot.run_id, %error, "workflow run could not be reread after its attempt");
        }
        for waiter in self.waiters.drain(..) {
            let _ = waiter.send(self.snapshot.clone());
        }
    }

    async fn handle_host(&mut self, command: HostCommand) {
        match command {
            HostCommand::Agent {
                key,
                request,
                reply,
            } => match self.admit(key, std::slice::from_ref(&request)).await {
                Ok(()) => self.pending = Some(Pending::Agent { reply }),
                Err(error) => drop(reply.send(Err(error))),
            },
            HostCommand::Parallel {
                first_key,
                requests,
                reply,
            } => match self.admit(first_key, &requests).await {
                Ok(()) => {
                    self.pending = Some(Pending::Parallel {
                        first_key,
                        results: vec![None; requests.len()],
                        reply,
                    });
                }
                Err(error) => drop(reply.send(Err(error))),
            },
            HostCommand::Phase { title, reply } => {
                self.set_phase(title).await;
                let _ = reply.send(());
            }
            HostCommand::Log { message, reply } => {
                self.log(message).await;
                let _ = reply.send(());
            }
            HostCommand::Scratch {
                key,
                name,
                content,
                reply,
            } => {
                let _ = reply.send(self.scratch(key, &name, &content).await);
            }
            HostCommand::Finished(outcome) => self.finish(outcome).await,
        }
    }

    /// Reserves budget for every request at once, journals each as started,
    /// and launches them. A batch the budget cannot hold admits nothing.
    async fn admit(
        &mut self,
        first_key: CallKey,
        requests: &[AgentRequest],
    ) -> Result<(), HostError> {
        if self.preempted || self.cancel.is_cancelled() {
            return Err(HostError::Cancelled);
        }
        let count = u32::try_from(requests.len()).map_err(|_| HostError::BudgetExhausted)?;
        let admitted = self
            .snapshot
            .usage
            .agents_admitted
            .checked_add(count)
            .filter(|admitted| *admitted <= self.snapshot.agent_budget)
            .ok_or(HostError::BudgetExhausted)?;
        let kind = if requests.len() == 1 {
            CallKind::Agent
        } else {
            CallKind::Parallel
        };
        let keyed: Vec<(CallKey, &AgentRequest)> = requests
            .iter()
            .enumerate()
            .map(|(index, request)| {
                let key = u64::try_from(index)
                    .ok()
                    .and_then(|offset| first_key.offset(offset))
                    .ok_or_else(|| HostError::Failed(KEY_OVERFLOW.to_owned()))?;
                Ok((key, request))
            })
            .collect::<Result<_, HostError>>()?;
        self.snapshot.usage.agents_admitted = admitted;
        for (key, request) in &keyed {
            self.roster_upsert(AgentRosterEntry {
                call_key: key.0,
                label: label_of(*key, request),
                phase: request
                    .phase
                    .clone()
                    .or_else(|| self.snapshot.phase.clone()),
                task_id: Some(task_id(&self.snapshot.run_id, *key)),
                state: RosterState::Running,
                tokens_used: 0,
                duration_ms: 0,
            });
        }
        let applied = self
            .commit(WorkflowRunPatch {
                agents_admitted: Some(u64::from(admitted)),
                usage: Some(json_text(&self.snapshot.usage)),
                roster: Some(json_text(&self.snapshot.roster)),
                ..WorkflowRunPatch::default()
            })
            .await
            .map_err(host_failure)?;
        if !applied {
            return Err(HostError::Cancelled);
        }
        for (key, request) in keyed {
            let request_value = agent_request_value(request);
            self.env
                .store
                .start_call(WorkflowCallStart {
                    run_id: self.snapshot.run_id.clone(),
                    call_key: key.0,
                    kind: stored_call_kind(kind),
                    request_hash: hash_request(kind, &request_value).to_string(),
                    request: request_value.to_string(),
                    task_id: Some(task_id(&self.snapshot.run_id, key)),
                })
                .await
                .map_err(host_failure)?;
            self.spawn_agent(key, request);
        }
        self.publish();
        Ok(())
    }

    fn spawn_agent(&mut self, key: CallKey, request: &AgentRequest) {
        let (trigger, cancel) = self.cancel.child();
        let provenance = self.provenance(key.0, request.phase.clone());
        let task_request = TaskRequest {
            prompt: Some(request.prompt.clone()),
            label: label_of(key, request),
            task: TaskIdentity::Fresh(task_id(&self.snapshot.run_id, key)),
            mode: Some(match request.capability_mode {
                CapabilityMode::ReadOnly => SubagentTaskMode::Plan,
                CapabilityMode::Build => SubagentTaskMode::Build,
            }),
            profile: request.profile.clone(),
            model_job: request.model_job.and_then(model_purpose),
            output_schema: request.output_schema.clone(),
            call_id: format!("{}{CALL_ID_SEPARATOR}{}", self.snapshot.run_id, key.0),
            provenance: Some(provenance.clone()),
        };
        let events = EventSender::new(self.env.events.clone(), WORKFLOW_EVENT_RUN_ID)
            .with_workflow(provenance);
        let runner = Arc::clone(&self.env.runner);
        let done = self.agents_tx.clone();
        self.in_flight += 1;
        smol::spawn(async move {
            let outcome = runner.run(task_request, cancel, events).await;
            drop(trigger);
            let _ = done.send(AgentDone { key, outcome });
        })
        .detach();
    }

    /// A cancelled agent and one that never opened a session leave a failed
    /// call the next attempt retries; anything that ran is journaled as its
    /// `AgentResult`, success or not, so replay returns exactly what the
    /// script saw.
    async fn agent_done(&mut self, done: AgentDone) {
        self.in_flight -= 1;
        let AgentDone { key, outcome } = done;
        let run_id = self.snapshot.run_id.clone();
        let mut finish = WorkflowCallFinish {
            task_id: outcome.task_id.clone(),
            tokens_used: outcome.tokens_used,
            duration_ms: outcome.duration_ms,
            ..WorkflowCallFinish::default()
        };
        let mut result = match &outcome.error {
            Some(_) if outcome.cancelled => {
                finish.error = Some(CANCELLED_CALL_ERROR.to_owned());
                Err(HostError::Cancelled)
            }
            Some(error) if outcome.task_id.is_none() => {
                finish.error = Some(error.clone());
                Err(HostError::Failed(format!("{AGENT_FAILED_PREFIX}{error}")))
            }
            _ => {
                let result = AgentResult {
                    agent_id: outcome
                        .task_id
                        .clone()
                        .unwrap_or_else(|| task_id(&run_id, key)),
                    success: outcome.success,
                    output: outcome.output.clone(),
                    cancelled: false,
                    tokens_used: outcome.tokens_used,
                    duration_ms: outcome.duration_ms,
                };
                finish.result = Some(json_text(&result));
                Ok(result)
            }
        };
        if let Some(error) = &outcome.error {
            self.log(format!("{}: {error}", label_in_roster(&self.snapshot, key)))
                .await;
        }
        if let Err(error) = self.env.store.finish_call(run_id, key.0, finish).await {
            warn!(run_id = %self.snapshot.run_id, call_key = key.0, %error, "workflow call could not be journaled");
            result = Err(host_failure(error));
        }
        self.roster_finish(key, &outcome, &result);
        self.snapshot.usage.tokens_used += outcome.tokens_used;
        match self
            .commit(WorkflowRunPatch {
                usage: Some(json_text(&self.snapshot.usage)),
                roster: Some(json_text(&self.snapshot.roster)),
                ..WorkflowRunPatch::default()
            })
            .await
        {
            Ok(true) => self.publish(),
            Ok(false) => result = Err(HostError::Cancelled),
            Err(error) => {
                warn!(run_id = %self.snapshot.run_id, %error, "workflow usage could not be stored");
                result = Err(host_failure(error));
            }
        }
        self.resolve_pending(key, result);
    }

    fn resolve_pending(&mut self, key: CallKey, result: Result<AgentResult, HostError>) {
        match self.pending.take() {
            Some(Pending::Agent { reply }) => drop(reply.send(result)),
            Some(Pending::Parallel {
                first_key,
                mut results,
                reply,
            }) => match (result, key.0.checked_sub(first_key.0)) {
                (Ok(result), Some(index)) if (index as usize) < results.len() => {
                    results[index as usize] = Some(result);
                    if results.iter().all(Option::is_some) {
                        let _ = reply.send(Ok(results.into_iter().flatten().collect()));
                    } else {
                        self.pending = Some(Pending::Parallel {
                            first_key,
                            results,
                            reply,
                        });
                    }
                }
                (Ok(_), _) => {
                    self.pending = Some(Pending::Parallel {
                        first_key,
                        results,
                        reply,
                    });
                }
                (Err(error), _) => drop(reply.send(Err(error))),
            },
            None => {}
        }
    }

    async fn set_phase(&mut self, title: String) {
        if self.preempted {
            return;
        }
        self.snapshot.phase = Some(title.clone());
        match self
            .commit(WorkflowRunPatch {
                phase: Some(self.snapshot.phase.clone()),
                ..WorkflowRunPatch::default()
            })
            .await
        {
            Ok(true) => {
                if self.snapshot.phase_history.len() >= MAX_PHASE_HISTORY {
                    self.snapshot.phase_history.remove(0);
                }
                self.snapshot.phase_history.push(PhaseRecord {
                    title: title.clone(),
                    started_at: now_secs(),
                });
                self.record_event(WorkflowEventKind::Phase, title).await;
                self.publish();
            }
            Ok(false) => {}
            Err(error) => {
                warn!(run_id = %self.snapshot.run_id, %error, "workflow phase could not be stored");
            }
        }
    }

    async fn log(&mut self, message: String) {
        let at = now_secs();
        if self.snapshot.logs.len() >= MAX_RUN_LOG_ENTRIES {
            self.snapshot.logs.remove(0);
        }
        self.snapshot.logs.push(LogLine {
            at,
            message: message.clone(),
        });
        if !self.preempted {
            self.record_event(WorkflowEventKind::Log, message.clone())
                .await;
        }
        publish(&self.env.published, &self.snapshot);
        self.emit(WorkflowEvent::Log {
            run_id: self.snapshot.run_id.clone(),
            revision: self.snapshot.revision,
            at,
            message,
        });
    }

    /// The timeline is a view, so a row it cannot keep is a warning rather
    /// than a failed run.
    async fn record_event(&self, kind: WorkflowEventKind, text: String) {
        if let Err(error) = self
            .env
            .store
            .append_event(self.snapshot.run_id.clone(), kind, text)
            .await
        {
            warn!(run_id = %self.snapshot.run_id, %kind, %error, "workflow timeline row could not be stored");
        }
    }

    async fn scratch(
        &mut self,
        key: CallKey,
        name: &str,
        content: &str,
    ) -> Result<String, HostError> {
        if self.preempted || self.cancel.is_cancelled() {
            return Err(HostError::Cancelled);
        }
        let run_id = self.snapshot.run_id.clone();
        let request = scratch_request_value(name, content);
        self.env
            .store
            .start_call(WorkflowCallStart {
                run_id: run_id.clone(),
                call_key: key.0,
                kind: WorkflowCallKind::ScratchFile,
                request_hash: hash_request(CallKind::ScratchFile, &request).to_string(),
                request: request.to_string(),
                task_id: None,
            })
            .await
            .map_err(scratch_failure)?;
        let path = self
            .env
            .store
            .write_scratch(run_id.clone(), name.to_owned(), content.to_owned())
            .await
            .map_err(scratch_failure)?
            .to_string_lossy()
            .into_owned();
        self.env
            .store
            .finish_call(
                run_id,
                key.0,
                WorkflowCallFinish {
                    result: Some(Value::String(path.clone()).to_string()),
                    ..WorkflowCallFinish::default()
                },
            )
            .await
            .map_err(scratch_failure)?;
        Ok(path)
    }

    async fn finish(&mut self, outcome: WorkflowOutcome) {
        self.engine_done = true;
        info!(run_id = %self.snapshot.run_id, epoch = self.epoch, ?outcome, "workflow attempt ended");
        let mut patch = WorkflowRunPatch {
            outbox_pending: Some(true),
            ..WorkflowRunPatch::default()
        };
        let status = match outcome {
            WorkflowOutcome::Completed(value) => {
                patch.result = Some(Some(value.to_string()));
                self.snapshot.result = Some(value);
                RunStatus::Completed
            }
            WorkflowOutcome::Paused { kind, message } => {
                patch.pause_kind = Some(Some(kind.to_string()));
                patch.pause_message = Some(Some(message.clone()));
                self.snapshot.pause_kind = Some(kind.to_string());
                self.snapshot.pause_message = Some(message);
                RunStatus::Paused
            }
            WorkflowOutcome::Failed(error) => {
                patch.error = Some(Some(error.clone()));
                self.snapshot.error = Some(error);
                RunStatus::Failed
            }
            WorkflowOutcome::Cancelled => RunStatus::Cancelled,
            WorkflowOutcome::BudgetLimited => RunStatus::BudgetLimited,
        };
        patch.status = Some(stored_status(status));
        self.snapshot.status = status;
        self.snapshot.outbox_pending = true;
        match self.commit(patch).await {
            Ok(true) => self.publish(),
            Ok(false) => {}
            Err(error) => {
                warn!(run_id = %self.snapshot.run_id, %error, "workflow outcome could not be stored");
            }
        }
    }

    async fn interrupt(&mut self, interrupt: Interrupt) {
        self.waiters.push(interrupt.reply);
        if self.snapshot.status == RunStatus::Active && !self.preempted {
            let next_epoch = self.epoch + 1;
            self.snapshot.status = interrupt.status;
            self.snapshot.execution_epoch = next_epoch;
            self.snapshot.outbox_pending = true;
            for entry in &mut self.snapshot.roster {
                if entry.state == RosterState::Running {
                    entry.state = RosterState::Cancelled;
                }
            }
            match self
                .commit(WorkflowRunPatch {
                    status: Some(stored_status(interrupt.status)),
                    execution_epoch: Some(next_epoch),
                    roster: Some(json_text(&self.snapshot.roster)),
                    outbox_pending: Some(true),
                    ..WorkflowRunPatch::default()
                })
                .await
            {
                Ok(true) => {
                    self.preempted = true;
                    self.publish();
                }
                Ok(false) => {}
                Err(error) => {
                    warn!(run_id = %self.snapshot.run_id, %error, "workflow interrupt could not be stored");
                }
            }
        }
        drop(self.trigger.take());
        if let Some(pending) = self.pending.take() {
            pending.cancel();
        }
    }

    /// Applies `patch` under this attempt's epoch. `false` means the row has
    /// moved on, in which case the working copy is refreshed from it.
    async fn commit(&mut self, patch: WorkflowRunPatch) -> Result<bool, WorkflowError> {
        let update = self
            .env
            .store
            .update_run(
                self.snapshot.run_id.clone(),
                self.snapshot.revision,
                self.epoch,
                patch,
            )
            .await?;
        match update {
            WorkflowUpdate::Applied { revision } => {
                self.snapshot.revision = revision;
                self.snapshot.updated_at = now_secs();
                Ok(true)
            }
            WorkflowUpdate::Stale => {
                self.preempted = true;
                self.reload().await?;
                Ok(false)
            }
        }
    }

    /// Refreshes the working copy from the row. Only the read model is
    /// updated: whoever moved the row past this attempt already announced it.
    async fn reload(&mut self) -> Result<(), WorkflowError> {
        let row = self
            .env
            .store
            .load_run(self.snapshot.run_id.clone())
            .await?
            .ok_or_else(|| WorkflowError::Internal(RUN_ROW_MISSING.to_owned()))?;
        let logs = std::mem::take(&mut self.snapshot.logs);
        let phase_history = std::mem::take(&mut self.snapshot.phase_history);
        self.snapshot = snapshot_from_row(&row);
        self.snapshot.logs = logs;
        self.snapshot.phase_history = phase_history;
        publish(&self.env.published, &self.snapshot);
        Ok(())
    }

    fn roster_upsert(&mut self, entry: AgentRosterEntry) {
        match self
            .snapshot
            .roster
            .iter_mut()
            .find(|existing| existing.call_key == entry.call_key)
        {
            Some(existing) => *existing = entry,
            None => self.snapshot.roster.push(entry),
        }
    }

    fn roster_finish(
        &mut self,
        key: CallKey,
        outcome: &TaskOutcome,
        result: &Result<AgentResult, HostError>,
    ) {
        let Some(entry) = self
            .snapshot
            .roster
            .iter_mut()
            .find(|entry| entry.call_key == key.0)
        else {
            return;
        };
        entry.state = match result {
            Ok(result) if result.success => RosterState::Completed,
            Ok(_) | Err(HostError::Failed(_) | HostError::Scratch(_)) => RosterState::Failed,
            Err(HostError::Cancelled | HostError::BudgetExhausted) => RosterState::Cancelled,
        };
        entry.tokens_used = outcome.tokens_used;
        entry.duration_ms = outcome.duration_ms;
        if outcome.task_id.is_some() {
            entry.task_id.clone_from(&outcome.task_id);
        }
    }

    fn provenance(&self, call_key: u64, phase: Option<String>) -> WorkflowProvenance {
        WorkflowProvenance {
            run_id: self.snapshot.run_id.clone(),
            epoch: self.epoch,
            call_key,
            phase: phase.or_else(|| self.snapshot.phase.clone()),
        }
    }

    fn publish(&self) {
        publish(&self.env.published, &self.snapshot);
        self.emit(WorkflowEvent::Snapshot(Box::new(self.snapshot.clone())));
    }

    fn emit(&self, event: WorkflowEvent) {
        EventSender::new(self.env.events.clone(), WORKFLOW_EVENT_RUN_ID)
            .with_workflow(self.provenance(0, None))
            .try_send(AgentEvent::Workflow(Box::new(event)));
    }
}

async fn recv_or_pending<T>(rx: Option<&Receiver<T>>) -> Result<T, flume::RecvError> {
    match rx {
        Some(rx) => rx.recv_async().await,
        None => future::pending().await,
    }
}

/// The job a script asked for, as a routing purpose. `subagent` is how a script
/// says "however subagents are configured here", which is the absence of an
/// override rather than a binding that would resolve to itself.
fn model_purpose(job: ModelJob) -> Option<ModelPurpose> {
    match job {
        ModelJob::Chat => Some(ModelPurpose::Chat),
        ModelJob::Plan => Some(ModelPurpose::Plan),
        ModelJob::Fast => Some(ModelPurpose::Fast),
        ModelJob::Best => Some(ModelPurpose::Best),
        ModelJob::Subagent => None,
    }
}

fn label_of(key: CallKey, request: &AgentRequest) -> String {
    request
        .label
        .clone()
        .unwrap_or_else(|| format!("{AGENT_LABEL_PREFIX}{}", key.0))
}

fn label_in_roster(snapshot: &RunSnapshot, key: CallKey) -> String {
    snapshot
        .roster
        .iter()
        .find(|entry| entry.call_key == key.0)
        .map_or_else(
            || format!("{AGENT_LABEL_PREFIX}{}", key.0),
            |entry| entry.label.clone(),
        )
}

pub(super) fn task_id(run_id: &str, key: CallKey) -> String {
    format!("{run_id}{TASK_ID_SEPARATOR}{}", key.0)
}

fn stored_call_kind(kind: CallKind) -> WorkflowCallKind {
    match kind {
        CallKind::Agent => WorkflowCallKind::Agent,
        CallKind::Parallel => WorkflowCallKind::Parallel,
        CallKind::ScratchFile => WorkflowCallKind::ScratchFile,
    }
}

fn json_text<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .map(|value| value.to_string())
        .unwrap_or_else(|_| Value::Null.to_string())
}

fn host_failure(error: WorkflowError) -> HostError {
    HostError::Failed(error.to_string())
}

fn scratch_failure(error: WorkflowError) -> HostError {
    HostError::Scratch(error.to_string())
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::{ModelJob, ModelPurpose, model_purpose};

    /// The `subagent` case is the one worth pinning: it must resolve to no
    /// override, not to a purpose that would route a subagent to itself.
    #[test_case(ModelJob::Chat => Some(ModelPurpose::Chat); "chat")]
    #[test_case(ModelJob::Plan => Some(ModelPurpose::Plan); "plan")]
    #[test_case(ModelJob::Fast => Some(ModelPurpose::Fast); "fast")]
    #[test_case(ModelJob::Best => Some(ModelPurpose::Best); "best")]
    #[test_case(ModelJob::Subagent => None; "subagent")]
    fn a_model_job_maps_to_its_routing_purpose(job: ModelJob) -> Option<ModelPurpose> {
        model_purpose(job)
    }
}
