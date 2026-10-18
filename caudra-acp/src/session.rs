use std::collections::HashSet;
use std::mem;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_client_protocol_schema::v1::{
    AvailableCommand, AvailableCommandsUpdate, CompactionId, CompactionStatus, ConfigOptionUpdate,
    ContentBlock, CurrentModeUpdate, EmbeddedResourceResource, Error as AcpError, ImageContent,
    PromptRequest, RequestId, SessionConfigOption, SessionId, SessionModeId, SessionUpdate,
    StopReason, TextContent,
};
use caudra_agent::background::BackgroundTasks;
use caudra_agent::headless::{InteractiveHandle, InteractiveRun, billed_cost};
use caudra_agent::mcp::McpHandle;
use caudra_agent::tools::ToolRegistry;
use caudra_agent::tools::native::plan::PlanTarget;
use caudra_agent::tools::native::skill;
use caudra_agent::types::AgentEvent;
use caudra_agent::{
    AgentInput, AgentMode, Envelope, ImageMediaType, ImageSource, McpPromptRef, Message,
    QueueItemId, SteeringQueue, mentions,
};
use caudra_providers::model::Model;
use caudra_providers::{ThinkingConfig, add_cost};
use caudra_storage::id::CaudraId;
use caudra_storage::paths;
use flume::{Receiver, Sender, WeakSender};
use smol::{Timer, future};
use tracing::{debug, warn};

use crate::asks::{
    ClientUpdates, request_permission, resolve_presented_permission, update_presented_permission,
    withdraw_asks,
};
use crate::child_work::{
    ChildContext, ChildTier, ChildWork, Children, StopOutcome, UNKNOWN_SUBAGENT, watch_tasks,
};
use crate::commands::{CommandCatalog, Invocation};
use crate::errors::{
    AUTH_REQUIRED_DETAILS, PROMPT_ACTIVE, STEER_COMPACT, agent_failure, auth_required,
    invalid_params, invalid_request, invalid_target, session_ended,
};
use crate::methods::SessionSettings;
use crate::outbound::{Outbound, PendingState, Written};
use crate::server::Incoming;
use crate::translate::{MessageCursor, OpenCalls};
use crate::turn::{DRAIN_TIMEOUT, Leftover, OwedPrompt, PromptDirective, Steer, TurnEnd, resubmit};
use crate::{AcpRuntime, locked, methods, translate};

const MISSING_PROMPT_ARGUMENTS: &str = "missing required MCP prompt arguments";

/// What a prompt asks the agent for.
enum Submission {
    Turn(Box<AgentInput>),
    Compact,
}

/// One live session: its agent, the MCP servers it started, and the runtime
/// it was resolved against.
pub(crate) struct SessionState {
    pub(crate) handle: InteractiveHandle,
    pub(crate) mcp: Option<McpHandle>,
    pub(crate) current_mode: AgentMode,
    /// Outlives mode switches, so each return to Plan revises the same plan.
    pub(crate) plan: Option<PlanTarget>,
    pub(crate) model: Model,
    /// The level as asked, snapped to the model on each turn as the TUI does.
    pub(crate) thinking: ThinkingConfig,
    pub(crate) fast: bool,
    /// Whether the client renders boolean options.
    pub(crate) boolean_options: bool,
    pub(crate) updates: ClientUpdates,
    pub(crate) child_tier: ChildTier,
    pub(crate) children: Children,
    /// The options the client was last told, so an unchanged list is not
    /// announced again.
    pub(crate) announced: Vec<SessionConfigOption>,
    /// The slash commands the client was last told, likewise.
    pub(crate) commands: Vec<AvailableCommand>,
    pub(crate) pending: PendingState,
    /// Resolves the relative paths an `@` mention names in a prompt.
    pub(crate) cwd: PathBuf,
    pub(crate) remote: bool,
    pub(crate) runtime: AcpRuntime,
}

impl SessionState {
    pub(crate) fn id(&self) -> SessionId {
        SessionId::from(self.handle.session_id.to_string())
    }

    pub(crate) fn mode_id(&self) -> &'static str {
        if self.current_mode.is_planning() {
            methods::MODE_PLAN
        } else {
            methods::MODE_BUILD
        }
    }

    pub(crate) fn config_options(&self, model_specs: &[String]) -> Vec<SessionConfigOption> {
        methods::config_options(&SessionSettings {
            model: &self.model,
            model_specs,
            thinking: &self.thinking,
            fast: self.fast,
            mode_id: self.mode_id(),
            permission_mode: &self.handle.permissions.mode(),
            boolean_options: self.boolean_options,
        })
    }

    /// The options a response carries, which the client holds from then on.
    pub(crate) fn hand_over_options(&mut self, model_specs: &[String]) -> Vec<SessionConfigOption> {
        self.announced = self.config_options(model_specs);
        self.announced.clone()
    }

    /// Tells the client the options changed, unless they did not, and returns
    /// them whole.
    pub(crate) fn announce_options(
        &mut self,
        outbound: &Outbound,
        model_specs: &[String],
    ) -> Vec<SessionConfigOption> {
        let options = self.config_options(model_specs);
        if options != self.announced {
            let update = ConfigOptionUpdate::new(options.clone());
            outbound.session_update(&self.id(), SessionUpdate::ConfigOptionUpdate(update));
            self.announced.clone_from(&options);
        }
        options
    }

    /// Tells the client which slash commands the session offers, unless that
    /// did not change since it was last told.
    pub(crate) fn announce_commands(&mut self, outbound: &Outbound) {
        let commands = self.catalog().commands();
        if commands != self.commands {
            let update = AvailableCommandsUpdate::new(commands.clone());
            outbound.session_update(&self.id(), SessionUpdate::AvailableCommandsUpdate(update));
            self.commands = commands;
        }
    }

    /// For a client that opened the session again and holds no list yet.
    pub(crate) fn reannounce_commands(&mut self, outbound: &Outbound) {
        self.commands.clear();
        self.announce_commands(outbound);
    }

    fn catalog(&self) -> CommandCatalog {
        CommandCatalog::of(&self.runtime.registry, self.mcp.as_ref())
    }

    pub(crate) fn offers_fast(&self) -> bool {
        self.boolean_options && self.model.supports_fast()
    }

    /// Where `session/set_mode` and the `mode` option both land.
    pub(crate) fn set_mode(&mut self, mode_id: &str, outbound: &Outbound) -> Result<(), AcpError> {
        self.switch_mode(mode_id)?;
        let update = CurrentModeUpdate::new(SessionModeId::from(mode_id.to_owned()));
        outbound.session_update(&self.id(), SessionUpdate::CurrentModeUpdate(update));
        Ok(())
    }

    pub(crate) fn switch_mode(&mut self, mode_id: &str) -> Result<(), AcpError> {
        let mode = methods::mode_id_to_agent_mode_for_session(
            mode_id,
            &self.cwd,
            self.runtime.workspace_session.as_ref(),
            self.runtime.local_documents.as_deref(),
            self.handle.session_id.as_str(),
            self.plan.as_ref(),
        )
        .ok_or_else(|| invalid_params(&format!("unknown mode: {mode_id}")))?;
        if let Some(plan) = mode.plan_target() {
            self.plan = Some(plan);
        }
        self.current_mode = mode;
        Ok(())
    }

    /// Registers `id` as the turn's answer before the agent can finish it.
    /// While one is still owed an answer, a prompt delivered as a steer joins
    /// the running turn and any other is refused.
    pub(crate) fn prompt(&self, id: RequestId, request: &PromptRequest) -> Result<(), AcpError> {
        let directive = PromptDirective::of(request.meta.as_ref());
        let mut pending = locked(&self.pending);
        if pending.agent_gone {
            return Err(session_ended());
        }
        if let Some(owed) = pending.prompt.as_mut() {
            // A client steers by cancelling and prompting again at once, so
            // that prompt waits for the cancelled turn instead of being refused.
            if owed.cancelled
                && !directive.steer
                && owed.next.is_none()
                && let Submission::Turn(input) = self.submission(&request.prompt)?
            {
                owed.next = Some(Leftover::new(id, directive.identity, *input));
                return Ok(());
            }
            if !directive.steer || owed.cancelled {
                return Err(invalid_request(PROMPT_ACTIVE));
            }
            let Submission::Turn(input) = self.submission(&request.prompt)? else {
                return Err(invalid_request(STEER_COMPACT));
            };
            owed.steers.push(Steer {
                request: id,
                identity: directive.identity,
                item: self.handle.steering.push(*input),
                consumed: false,
            });
            return Ok(());
        }
        let submission = self.submission(&request.prompt)?;
        let sent = match submission {
            Submission::Turn(input) => self.handle.input_tx.send(*input).is_ok(),
            Submission::Compact => self.handle.compact_tx.send(()).is_ok(),
        };
        if !sent {
            return Err(session_ended());
        }
        pending.prompt = Some(OwedPrompt::new(id, directive.identity));
        Ok(())
    }

    /// A prompt naming an offered slash command runs it as the TUI does;
    /// any other text is a turn as typed.
    fn submission(&self, blocks: &[ContentBlock]) -> Result<Submission, AcpError> {
        let (message, images) = extract_prompt_content(blocks);
        let catalog = self.catalog();
        let (preamble, prompt) = match catalog.route(&message) {
            None => (Vec::new(), None),
            Some(Invocation::Compact) => return Ok(Submission::Compact),
            Some(Invocation::Skill(name)) => {
                let body = skill::load(&self.runtime.registry, name)
                    .map_err(|error| invalid_params(&error))?;
                (vec![Message::observation(body)], None)
            }
            Some(Invocation::McpPrompt { prompt, args }) => {
                let arguments = prompt.parse_arguments(args);
                let missing = prompt.missing_arguments(&arguments);
                if !missing.is_empty() {
                    return Err(invalid_params(&format!(
                        "{MISSING_PROMPT_ARGUMENTS}: {}",
                        missing.join(", ")
                    )));
                }
                let prompt = McpPromptRef {
                    qualified_name: prompt.qualified_name.clone(),
                    arguments,
                };
                (Vec::new(), Some(Box::new(prompt)))
            }
        };
        Ok(Submission::Turn(Box::new(AgentInput {
            preamble,
            prompt,
            ..self.input(message, images)
        })))
    }

    fn input(&self, message: String, images: Vec<ImageSource>) -> AgentInput {
        let mentions = if self.remote {
            mentions::scan_remote(&message)
                .into_iter()
                .map(|(_, mention)| mention)
                .collect()
        } else {
            mentions::scan(&message, |path| self.cwd.join(path).exists())
                .into_iter()
                .map(|(_, mention)| mention)
                .collect()
        };
        AgentInput {
            message,
            mode: self.current_mode.clone(),
            plan: self.plan.clone(),
            images,
            mentions,
            commits: Vec::new(),
            preamble: Vec::new(),
            thinking: self.thinking.clone(),
            fast: self.fast,
            prompt: None,
            resume: false,
        }
    }

    pub(crate) fn set_model(&mut self, model: Model) -> Result<(), AcpError> {
        self.handle
            .model_tx
            .send(model.clone())
            .map_err(|_| session_ended())?;
        self.follow_model(model);
        Ok(())
    }

    /// Moves the session onto `model`, which the agent already runs.
    pub(crate) fn follow_model(&mut self, model: Model) {
        self.model = model;
        self.fit_reasoning();
    }

    /// As the TUI reconciles a model switch: what the model cannot honour
    /// turns off, and a level it can stays as asked, snapped on each turn.
    pub(crate) fn fit_reasoning(&mut self) {
        if !self.model.supports_thinking() {
            self.thinking = ThinkingConfig::Off;
        }
        if !self.model.supports_fast() {
            self.fast = false;
        }
    }

    /// Withdraws every question the turn still has before the client, whose
    /// answers are dropped on arrival, and stops the agent. A prompt no run
    /// took yet is withdrawn too and answered at once; otherwise the event
    /// pump answers it `cancelled` once the turn drains.
    pub(crate) fn cancel(&self, outbound: &Outbound) {
        withdraw_asks(self, outbound);
        let mut pending = locked(&self.pending);
        let unstarted = self.handle.pending_inputs.withdraw() > 0;
        match pending.prompt.take() {
            Some(owed) if unstarted => {
                owed.abandon(outbound, &self.handle.steering, TurnEnd::cancelled());
            }
            Some(mut owed) => {
                if let Some(next) = owed.next.take() {
                    next.answer(outbound, &Ok(StopReason::Cancelled));
                }
                owed.cancelled = true;
                pending.prompt = Some(owed);
                pending.cancel_signal.raise();
            }
            None => {}
        }
        drop(pending);
        let _ = self.handle.cancel_tx.try_send(());
    }

    /// Stops one running subagent, leaving the turn that delegated to it
    /// running with the subagent's cancelled result. One that already ended
    /// is left alone and answered with how it ended.
    pub(crate) fn cancel_subagent(&self, subagent_id: &str) -> Result<StopOutcome, AcpError> {
        if self.children.running(subagent_id) {
            self.handle
                .subagent_cancels
                .cancel_or_precancel(subagent_id.to_owned());
            return Ok(StopOutcome::Stopped);
        }
        self.children
            .ended(subagent_id)
            .map(|status| StopOutcome::NotRunning {
                status: status.to_owned(),
            })
            .ok_or_else(|| invalid_target(&format!("{UNKNOWN_SUBAGENT}: {subagent_id}")))
    }

    /// Whether `id` is a `session/prompt` this session still owes an answer,
    /// as the prompt, a steer that joined its turn, or the prompt waiting
    /// behind a cancel.
    pub(crate) fn owes(&self, id: &RequestId) -> bool {
        locked(&self.pending).prompt.as_ref().is_some_and(|owed| {
            owed.request == *id
                || owed.steers.iter().any(|steer| steer.request == *id)
                || owed.next.as_ref().is_some_and(|next| next.request() == id)
        })
    }

    /// Stops the agent and its MCP servers. The event pump dies with the
    /// agent, so the prompt it owed an answer to is answered here or the
    /// client waits on it forever.
    pub(crate) async fn stop(self, outbound: &Outbound) -> AcpRuntime {
        if let Some(owed) = locked(&self.pending).prompt.take() {
            owed.abandon(outbound, &self.handle.steering, TurnEnd::cancelled());
        }
        let InteractiveHandle {
            input_tx,
            cancel_tx,
            task,
            ..
        } = self.handle;
        let _ = cancel_tx.try_send(());
        drop(input_tx);
        task.await;
        if let Some(mcp) = self.mcp {
            mcp.shutdown().await;
        }
        self.runtime
    }

    pub(crate) fn event_pump(
        &self,
        outbound: Outbound,
        server: WeakSender<Incoming>,
        initial_cost: Option<f64>,
    ) -> EventPump {
        EventPump {
            events: self.handle.event_rx.clone(),
            runs: self.handle.run_rx.clone(),
            cancel: self.handle.cancel_tx.clone(),
            input: self.handle.input_tx.downgrade(),
            steering: self.handle.steering.clone(),
            drain_timeout: DRAIN_TIMEOUT,
            session_id: self.id(),
            outbound,
            server,
            pending: Arc::clone(&self.pending),
            registry: Arc::clone(&self.runtime.registry),
            cwd: self.cwd.clone(),
            home: paths::home(),
            initial_cost,
            updates: self.updates.clone(),
            child: ChildWork::new(self.child_tier, self.id(), self.children.clone()),
            background: self
                .handle
                .background
                .clone()
                .filter(|_| self.child_tier == ChildTier::Vendor),
        }
    }
}

/// What wakes the event pump other than an agent event.
enum Wake {
    /// The client cancelled the owed prompt, so its drain wait starts.
    Cancelled,
    /// The cancelled turn did not end within the drain wait.
    Drained,
    /// The agent's events stopped for good.
    Ended,
}

/// Turns one session's agent events into the client's session updates.
pub(crate) struct EventPump {
    pub(crate) events: Receiver<Envelope>,
    /// Each run as it starts, ahead of its events, saying whether a prompt
    /// started it or the agent did on its own.
    pub(crate) runs: Receiver<InteractiveRun>,
    /// Stops a run the client was told failed, so it does not carry on unseen.
    pub(crate) cancel: Sender<()>,
    /// Where a steer no turn took starts the next one. Weak, so the agent
    /// still sees its input close when the session stops.
    pub(crate) input: WeakSender<AgentInput>,
    pub(crate) steering: SteeringQueue,
    /// How long a cancelled turn may take to report its end.
    pub(crate) drain_timeout: Duration,
    pub(crate) session_id: SessionId,
    pub(crate) outbound: Outbound,
    /// Where an event that changes the session's settings goes, since only
    /// the main loop owns them.
    pub(crate) server: WeakSender<Incoming>,
    pub(crate) pending: PendingState,
    /// The session's own tools, which name the kind each call reports.
    pub(crate) registry: Arc<ToolRegistry>,
    pub(crate) cwd: PathBuf,
    pub(crate) home: Option<PathBuf>,
    pub(crate) initial_cost: Option<f64>,
    pub(crate) updates: ClientUpdates,
    pub(crate) child: ChildWork,
    /// The shells to report, for a client that reads `_caudra/task/*`.
    pub(crate) background: Option<BackgroundTasks>,
}

impl EventPump {
    /// Holds every event back until `announced` settles, so nothing about a
    /// session reaches the client before the response that names it.
    pub(crate) fn spawn(self, announced: Written) {
        smol::spawn(async move {
            announced.wait().await;
            self.run().await;
        })
        .detach();
    }

    async fn run(self) {
        let Self {
            events,
            runs,
            cancel,
            input,
            steering,
            drain_timeout,
            session_id: sid,
            outbound,
            server,
            pending,
            registry,
            cwd,
            home,
            initial_cost,
            updates,
            mut child,
            background,
        } = self;
        let _task_watch = background
            .map(|background| smol::spawn(watch_tasks(background, outbound.clone(), sid.clone())));
        let turn = TurnOutput {
            outbound: &outbound,
            sid: &sid,
            pending: &pending,
            steering: &steering,
            input: &input,
        };
        let cancelled = locked(&pending).cancel_signal.clone();
        let mut cost_total = initial_cost;
        let mut turn_spend = translate::TurnSpend::default();
        // The compaction the client was told is in progress, which the next
        // completion, end of run or error settles.
        let mut compaction: Option<CompactionId> = None;
        // Tool calls whose `tool_call` creation has been sent. An id leaves the
        // set when the call finishes, so this tracks only calls in flight.
        let mut announced = OpenCalls::new();
        // The assistant message being streamed, which names its chunks.
        let mut reply: Option<MessageCursor> = None;
        // The run serving the owed prompt, once its start or one of its
        // events named it.
        let mut turn_run: Option<u64> = None;
        // Runs that answer no prompt: those the agent started on its own, and
        // those whose prompt was answered without them. Their events name no
        // prompt, and their end, should it ever come, answers nothing.
        let mut answerless: HashSet<u64> = HashSet::new();
        // When a cancelled turn stops waiting for its own end.
        let mut drain_deadline: Option<Instant> = None;

        loop {
            let wake: Result<Envelope, Wake> = future::or(
                async { events.recv_async().await.map_err(|_| Wake::Ended) },
                future::or(
                    async {
                        cancelled.raised().await;
                        Err(Wake::Cancelled)
                    },
                    async {
                        match drain_deadline {
                            Some(deadline) => {
                                Timer::at(deadline).await;
                                Err(Wake::Drained)
                            }
                            None => future::pending().await,
                        }
                    },
                ),
            )
            .await;
            // A run is announced before its first event, so every run whose
            // events or drain are handled below is known here.
            for run in runs.try_iter() {
                if run.automatic {
                    answerless.insert(run.run_id);
                } else if turn_run.is_none() && turn.owed() {
                    turn_run = Some(run.run_id);
                }
            }
            let Envelope {
                event,
                subagent,
                run_id,
                ..
            } = match wake {
                Ok(envelope) => envelope,
                Err(Wake::Cancelled) => {
                    if turn.cancelled() {
                        drain_deadline.get_or_insert_with(|| Instant::now() + drain_timeout);
                    }
                    continue;
                }
                Err(Wake::Drained) => {
                    drain_deadline = None;
                    if !turn.cancelled() {
                        continue;
                    }
                    warn!(
                        session = %sid,
                        drain_timeout_ms = drain_timeout.as_millis(),
                        "cancelled turn did not end in time; answering its prompt cancelled"
                    );
                    answerless.extend(turn_run.take());
                    turn.settle_open_calls(&mut announced, true);
                    let spend = mem::take(&mut turn_spend);
                    turn.answer(TurnEnd::Stopped {
                        stop: StopReason::Cancelled,
                        spend,
                    });
                    continue;
                }
                Err(Wake::Ended) => {
                    turn.settle_open_calls(&mut announced, true);
                    let mut owed = locked(&pending);
                    owed.agent_gone = true;
                    if let Some(prompt) = owed.prompt.take() {
                        prompt.abandon(&outbound, &steering, TurnEnd::Failed(session_ended()));
                    }
                    return;
                }
            };
            // Subagent stream events stay out of the transcript, but their
            // turns and side calls still spend session money.
            add_cost(&mut cost_total, billed_cost(&event));
            if let AgentEvent::TurnComplete(tc) = &event {
                turn_spend.add(tc);
            }
            if let Some(info) = &subagent {
                let ctx = ChildContext {
                    outbound: &outbound,
                    pending: &pending,
                    registry: &registry,
                    cwd: &cwd,
                    home: home.as_deref(),
                    announced: &announced,
                };
                child.observe(&ctx, info, event, Instant::now());
                continue;
            }
            let answers = !answerless.contains(&run_id);
            if answers && turn_run.is_none() && is_run_activity(&event) && turn.owed() {
                turn_run = Some(run_id);
            }

            let update = match event {
                AgentEvent::MessageStarted { id } => {
                    reply = Some(MessageCursor::new(id.to_string()));
                    continue;
                }
                AgentEvent::QueueItemConsumed { id, .. } => {
                    turn.consume_steers(&[id]);
                    continue;
                }
                AgentEvent::QueueBatchConsumed { items } => {
                    let ids: Vec<_> = items.iter().map(|item| item.id).collect();
                    turn.consume_steers(&ids);
                    continue;
                }
                AgentEvent::TextDelta { text } => streamed(&mut reply).text(&text),
                AgentEvent::ThinkingDelta { text } => streamed(&mut reply).thought(&text),
                // The call is announced once its arguments name it, so a bare
                // `ToolPending` has nothing to say yet.
                AgentEvent::ToolPending { .. } => continue,
                AgentEvent::ToolInputDelta {
                    id,
                    name,
                    preview: Some(preview),
                    ..
                } => {
                    interrupt(&mut reply);
                    let first = announced
                        .insert(id.clone(), Arc::from(name.as_str()))
                        .is_none();
                    translate::tool_preview(&registry, &id, &name, preview, first)
                }
                AgentEvent::ToolInputDelta { .. } => continue,
                AgentEvent::ToolStart(event) => {
                    interrupt(&mut reply);
                    let first = announced
                        .insert(event.id.clone(), Arc::clone(&event.tool))
                        .is_none();
                    translate::tool_start(&registry, &event, &cwd, home.as_deref(), first)
                }
                AgentEvent::ToolOutput { id, content } => {
                    let name = announced.get(&id).map(|name| &**name);
                    translate::tool_output(&id, name, &content)
                }
                AgentEvent::ToolDone(event) => {
                    announced.remove(&event.id);
                    // A finished todo_write is the agent's plan changing, which
                    // ACP reports separately from the call that caused it.
                    if let Some(plan) = translate::plan_update(&event.output) {
                        turn.update(plan, answers);
                    }
                    translate::tool_done(&event, &cwd, home.as_deref())
                }
                AgentEvent::TurnComplete(event) => {
                    reply = None;
                    translate::usage_update(&event, cost_total)
                }
                AgentEvent::PermissionRequest(request) => {
                    request_permission(&outbound, &pending, &sid, &announced, *request);
                    continue;
                }
                AgentEvent::PermissionRequestUpdated(request) => {
                    update_presented_permission(&outbound, &pending, &sid, &announced, &request);
                    continue;
                }
                AgentEvent::PermissionRequestResolved { request_id, .. } => {
                    resolve_presented_permission(&outbound, &pending, &request_id);
                    continue;
                }
                AgentEvent::SessionTitle {
                    title: Some(title), ..
                } => translate::session_info(title, None),
                AgentEvent::Compacting if updates.compaction => {
                    let id = CompactionId::new(CaudraId::generate().to_string());
                    let update =
                        translate::compaction_update(&id, CompactionStatus::InProgress, None);
                    compaction = Some(id);
                    update
                }
                AgentEvent::CompactionDone => match compaction.take() {
                    Some(id) => {
                        translate::compaction_update(&id, CompactionStatus::Completed, None)
                    }
                    None => continue,
                },
                AgentEvent::Retry {
                    attempt,
                    message,
                    delay_ms,
                } if updates.notices => translate::retry_notice(attempt, message, delay_ms),
                AgentEvent::Done { reason, .. } => {
                    if let Some(id) = compaction.take() {
                        let status = translate::compaction_outcome(&reason);
                        turn.update(translate::compaction_update(&id, status, None), answers);
                    }
                    let spend = mem::take(&mut turn_spend);
                    turn.settle_open_calls(&mut announced, answers);
                    if answers {
                        drain_deadline = None;
                        turn_run = None;
                        turn.answer(TurnEnd::Stopped {
                            stop: translate::map_done_reason(reason),
                            spend,
                        });
                    } else {
                        answerless.remove(&run_id);
                    }
                    // A run may have changed the skills or MCP prompts on offer.
                    if let Some(server) = server.upgrade() {
                        let _ = server.send(Incoming::CommandsChanged(sid.clone()));
                    }
                    continue;
                }
                AgentEvent::Error { message } => {
                    if let Some(id) = compaction.take() {
                        let failed = translate::compaction_update(
                            &id,
                            CompactionStatus::Failed,
                            Some(message.clone()),
                        );
                        turn.update(failed, answers);
                    }
                    turn.settle_open_calls(&mut announced, answers);
                    if answers {
                        drain_deadline = None;
                        turn_run = None;
                        turn.answer(TurnEnd::Failed(agent_failure(&message)));
                    }
                    continue;
                }
                // The agent waits for new credentials, which the client cannot
                // see; it is told to sign in and the run stops rather than
                // resuming behind an answered prompt, whose own end then
                // answers nothing.
                AgentEvent::AuthRequired => {
                    if turn.owed() {
                        turn.settle_open_calls(&mut announced, true);
                        drain_deadline = None;
                        answerless.extend(turn_run.take());
                        turn.answer(TurnEnd::Failed(auth_required(AUTH_REQUIRED_DETAILS)));
                        let _ = cancel.try_send(());
                    }
                    continue;
                }
                AgentEvent::ModelRerouted(reroute) => {
                    if let Some(server) = server.upgrade() {
                        let _ = server.send(Incoming::ModelRerouted {
                            session: sid.clone(),
                            spec: reroute.to,
                        });
                    }
                    continue;
                }
                AgentEvent::Workflow(event) => {
                    debug!(
                        ?event,
                        "workflow event dropped: ACP sessions run no workflows"
                    );
                    continue;
                }
                _ => continue,
            };
            turn.update(update, answers);
        }
    }
}

/// Where the pump reports the owed prompt's turn: its updates carry the
/// prompt's id, and its end answers the prompt and its steers.
struct TurnOutput<'a> {
    outbound: &'a Outbound,
    sid: &'a SessionId,
    pending: &'a PendingState,
    steering: &'a SteeringQueue,
    input: &'a WeakSender<AgentInput>,
}

impl TurnOutput<'_> {
    fn owed(&self) -> bool {
        locked(self.pending).prompt.is_some()
    }

    fn cancelled(&self) -> bool {
        locked(self.pending)
            .prompt
            .as_ref()
            .is_some_and(|owed| owed.cancelled)
    }

    /// Sends `update`, naming the owed prompt only when its run serves it.
    fn update(&self, update: SessionUpdate, owned: bool) {
        let meta = owned.then(|| locked(self.pending).update_meta()).flatten();
        self.outbound.session_update_meta(self.sid, update, meta);
    }

    /// Calls still open when their turn ends never finish, so the client is
    /// told they failed before it reads the turn's answer.
    fn settle_open_calls(&self, open: &mut OpenCalls, owned: bool) {
        for update in translate::cancel_open_calls(open) {
            self.update(update, owned);
        }
    }

    fn consume_steers(&self, items: &[QueueItemId]) {
        if let Some(owed) = locked(self.pending).prompt.as_mut() {
            for steer in &mut owed.steers {
                steer.consumed |= items.contains(&steer.item);
            }
        }
    }

    /// Answers the owed prompt, under the same lock that hands a steer the
    /// turn never took the next turn, so no prompt slips in between.
    fn answer(&self, end: TurnEnd) {
        let mut pending = locked(self.pending);
        if let Some(owed) = pending.prompt.take() {
            let leftovers = owed.finish(self.outbound, self.steering, end);
            pending.prompt = resubmit(leftovers, self.outbound, self.steering, self.input);
        }
    }
}

/// Events only a running turn sends, which name the run serving it.
fn is_run_activity(event: &AgentEvent) -> bool {
    matches!(
        event,
        AgentEvent::MessageStarted { .. }
            | AgentEvent::ToolStart(_)
            | AgentEvent::TurnComplete(_)
            | AgentEvent::Done { .. }
    )
}

/// The message deltas belong to. One that arrives with no message announced
/// gets an id of its own.
fn streamed(message: &mut Option<MessageCursor>) -> &mut MessageCursor {
    message.get_or_insert_with(|| MessageCursor::new(CaudraId::generate().to_string()))
}

fn interrupt(message: &mut Option<MessageCursor>) {
    if let Some(message) = message {
        message.interrupt();
    }
}

fn extract_prompt_content(blocks: &[ContentBlock]) -> (String, Vec<ImageSource>) {
    let mut text = String::new();
    let mut images = Vec::new();

    for block in blocks {
        match block {
            ContentBlock::Text(TextContent { text: t, .. }) => append(&mut text, t),
            ContentBlock::Image(ImageContent {
                data, mime_type, ..
            }) => images.push(ImageSource {
                media_type: image_media_type(mime_type),
                data: Arc::from(data.as_str()),
            }),
            ContentBlock::Resource(res) => {
                if let EmbeddedResourceResource::TextResourceContents(trc) = &res.resource {
                    append(&mut text, &format!("--- {} ---\n{}", trc.uri, trc.text));
                }
            }
            ContentBlock::ResourceLink(rl) => append(&mut text, &format!("[Resource: {}]", rl.uri)),
            _ => {}
        }
    }

    (text, images)
}

fn append(text: &mut String, part: &str) {
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(part);
}

fn image_media_type(mime: &str) -> ImageMediaType {
    match mime {
        "image/png" => ImageMediaType::Png,
        "image/gif" => ImageMediaType::Gif,
        "image/webp" => ImageMediaType::Webp,
        _ => ImageMediaType::Jpeg,
    }
}

#[cfg(test)]
mod tests {
    use std::iter;
    use std::path::Path;

    use agent_client_protocol_schema::v1::ErrorCode;
    use caudra_agent::permissions::{EngineFlag, PermissionAdvisory, PermissionRequest};
    use caudra_agent::tools::{ToolEffect, VIEW_IMAGE_TOOL_NAME, native};
    use caudra_agent::types::{ToolStartEvent, TurnCompleteEvent};
    use caudra_agent::{DoneReason, History, SubagentInfo, steering_queue};
    use caudra_config::FeatureFlags;
    use caudra_providers::{
        Billing, ContentBlock as MsgBlock, Role, TokenUsage, expand_message_in,
    };
    use caudra_storage::id::{CaudraId, SessionRef};
    use caudra_storage::sessions::StoredMode;
    use caudra_storage::usage_ledger::LedgerPurpose;
    use serde_json::Value;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::asks::permission_scope_summary;
    use crate::errors::DATA_DETAILS;
    use crate::outbound::AskKind;
    use crate::restore::{restored_mode, restored_plan};
    use crate::test_support::{
        ADVISORY_PROBABILITY, CANCELLED, CAUDRA_REQUEST_ID, DELETE_CAUTION, PLAN_FILE, PROMPT_ID,
        TestServer, no_server, owed, stored_local_plan,
    };
    use crate::turn::{META_DURATION_MS, META_PROMPT_ID, PromptIdentity};

    const PLAN_CONTENT: &str = "# Plan\nShip it.";
    const SESSION_REQUEST_PERMISSION: &str = "session/request_permission";
    const VIEW_IMAGE_CALL: &str = "view-image-call";
    const READ_KIND: &str = "read";
    const TITLE: &str = "Rename foo to bar";
    const END_MARKER: &str = "end of events";
    const COMPACTION_FAILURE: &str = "summary request failed";
    const RETRY_MESSAGE: &str = "overloaded";
    const RETRY_ATTEMPT: u32 = 2;
    const RETRY_DELAY_MS: u64 = 1500;
    const UNKNOWN_COMMAND: &str = "/unknown please";
    const SESSION_INFO_UPDATE: &str = "session_info_update";
    const COMPACTION_UPDATE: &str = "compaction_update";
    const NOTICE: &str = "notice";
    const IN_PROGRESS: &str = "in_progress";
    const COMPLETED: &str = "completed";
    const FAILED: &str = "failed";
    const REFUSAL: &str = "refusal";
    const SHELL_TOOL: &str = "shell";
    const OPEN_CALL: &str = "open-call";
    const TASK_CALL: &str = "task-call";
    const CHUNK_SUFFIX: &str = "_chunk";
    const USAGE_UPDATE: &str = "usage_update";
    const AUTH_FAILURE: &str =
        "authentication failed: invalid x-api-key, run `caudra auth login` or check your API key";
    const CONTEXT_WINDOW: u32 = 200_000;
    const SIDE_CALL_COST: f64 = 0.25;
    const TURN_COST: f64 = 0.5;
    const CLIENT_PROMPT_ID: &str = "client-prompt-7";
    const SECOND_PROMPT: i64 = 9;
    const STALE_RUN: u64 = 4;
    const AUTOMATIC_RUN: u64 = 6;
    const PROMPTED_RUN: u64 = 7;
    const TOOL_CALL: &str = "tool_call";
    const TOOL_CALL_UPDATE: &str = "tool_call_update";
    const END_TURN: &str = "end_turn";
    const STALE_END_ANSWERS_NOTHING: &str =
        "the cancelled run's late end must not answer the prompt that followed it";

    fn envelope(event: AgentEvent) -> Envelope {
        Envelope {
            event,
            task: None,
            subagent: None,
            run_id: 0,
            workflow: None,
        }
    }

    fn titled() -> AgentEvent {
        AgentEvent::SessionTitle {
            title: Some(TITLE.into()),
            usage: Default::default(),
            cost: None,
            billing: Default::default(),
            model: String::new(),
            provider: String::new(),
        }
    }

    fn retry() -> AgentEvent {
        AgentEvent::Retry {
            attempt: RETRY_ATTEMPT,
            message: RETRY_MESSAGE.into(),
            delay_ms: RETRY_DELAY_MS,
        }
    }

    fn done(reason: DoneReason) -> AgentEvent {
        AgentEvent::Done {
            usage: Default::default(),
            num_turns: 1,
            reason,
        }
    }

    fn failed() -> AgentEvent {
        AgentEvent::Error {
            message: COMPACTION_FAILURE.into(),
        }
    }

    fn test_pump(events: Receiver<Envelope>, outbound: Outbound) -> EventPump {
        let session_id = SessionId::from(SessionRef::generate().to_string());
        EventPump {
            events,
            runs: flume::unbounded().1,
            cancel: flume::unbounded().0,
            input: flume::unbounded().0.downgrade(),
            steering: steering_queue().0,
            drain_timeout: DRAIN_TIMEOUT,
            outbound,
            server: no_server(),
            pending: PendingState::default(),
            registry: Arc::default(),
            cwd: PathBuf::new(),
            home: None,
            initial_cost: None,
            updates: ClientUpdates::default(),
            child: ChildWork::new(
                ChildTier::Projected,
                session_id.clone(),
                Children::default(),
            ),
            background: None,
            session_id,
        }
    }

    /// Every frame `events` send through the pump `configure` sets up.
    fn pump_frames(
        configure: impl FnOnce(EventPump) -> EventPump,
        events: Vec<AgentEvent>,
    ) -> Vec<Value> {
        smol::block_on(async {
            let (event_tx, events_rx) = flume::unbounded();
            let (outbound, out_rx) = Outbound::channel();
            configure(test_pump(events_rx, outbound)).spawn(Written::settled());
            let end = AgentEvent::TextDelta {
                text: END_MARKER.into(),
            };
            for event in events.into_iter().chain([end]) {
                event_tx.send(envelope(event)).unwrap();
            }
            let mut seen = Vec::new();
            loop {
                let message = out_rx.recv_async().await.unwrap().message;
                if message["params"]["update"]["content"]["text"] == END_MARKER {
                    return seen;
                }
                seen.push(message);
            }
        })
    }

    /// The updates `events` turn into, each as its kind and the field that
    /// tells it apart.
    fn pumped(updates: ClientUpdates, events: Vec<AgentEvent>) -> Vec<String> {
        pump_frames(|pump| EventPump { updates, ..pump }, events)
            .iter()
            .map(|message| {
                let update = &message["params"]["update"];
                let detail = [&update["title"], &update["status"], &update["error"]]
                    .into_iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("{} {detail}", update["sessionUpdate"].as_str().unwrap())
            })
            .collect()
    }

    /// A pump owing `PROMPT_ID` an answer, and the receiver its cancel lands on.
    fn prompted() -> (PendingState, Receiver<()>, Sender<()>) {
        let pending = PendingState::default();
        locked(&pending).prompt = Some(owed(PROMPT_ID));
        let (cancel_tx, cancel_rx) = flume::unbounded();
        (pending, cancel_rx, cancel_tx)
    }

    fn shell_start(id: &str) -> AgentEvent {
        AgentEvent::ToolStart(Box::new(ToolStartEvent {
            id: id.into(),
            effect: ToolEffect::Unknown,
            tool: Arc::from(SHELL_TOOL),
            summary: String::new(),
            render_header: None,
            annotation: None,
            input: None,
            raw_input: None,
            output: None,
        }))
    }

    fn turn_complete(cost: f64) -> AgentEvent {
        AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
            message: Message::default(),
            usage: TokenUsage::default(),
            model: String::new(),
            provider: String::new(),
            purpose: LedgerPurpose::Chat,
            cost: Some(cost),
            billing: Billing::Api,
            account: None,
            context_size: None,
            context_window: CONTEXT_WINDOW,
        }))
    }

    fn side_call(cost: f64) -> AgentEvent {
        AgentEvent::ModelUsage {
            usage: TokenUsage::default(),
            cost: Some(cost),
            billing: Billing::Api,
            provider: String::new(),
            model: String::new(),
            purpose: LedgerPurpose::Title,
            account: None,
        }
    }

    /// The live stream of `blocks`, as the agent sends it for message `id`.
    fn streamed_blocks(id: CaudraId, blocks: &[MsgBlock]) -> Vec<AgentEvent> {
        iter::once(AgentEvent::MessageStarted { id })
            .chain(blocks.iter().map(|block| match block {
                MsgBlock::Text { text } => AgentEvent::TextDelta { text: text.clone() },
                MsgBlock::Thinking { thinking, .. } => AgentEvent::ThinkingDelta {
                    text: thinking.clone(),
                },
                MsgBlock::ToolUse { id, .. } => shell_start(id),
                _ => panic!("only streamed blocks"),
            }))
            .collect()
    }

    /// Each chunk as its kind, message id, and text.
    fn chunks<'a>(updates: impl IntoIterator<Item = &'a Value>) -> Vec<(Value, Value, Value)> {
        updates
            .into_iter()
            .filter(|update| {
                update["sessionUpdate"]
                    .as_str()
                    .is_some_and(|kind| kind.ends_with(CHUNK_SUFFIX))
            })
            .map(|update| {
                (
                    update["sessionUpdate"].clone(),
                    update["messageId"].clone(),
                    update["content"]["text"].clone(),
                )
            })
            .collect()
    }

    fn thought(text: &str) -> MsgBlock {
        MsgBlock::thinking(text.into(), None)
    }

    fn said(text: &str) -> MsgBlock {
        MsgBlock::Text { text: text.into() }
    }

    fn called(id: &str) -> MsgBlock {
        MsgBlock::tool_use(id, SHELL_TOOL, serde_json::json!({}))
    }

    #[test_case(vec![said("hi")] ; "text_only")]
    #[test_case(vec![thought("plan"), said("answer")] ; "thought_then_text")]
    #[test_case(vec![thought("a"), said("b"), thought("c"), said("d")] ; "interleaved_thoughts")]
    #[test_case(vec![thought("a"), said("b"), called("call-1"), thought("c")] ; "thought_after_a_call")]
    fn live_chunks_carry_the_ids_their_replay_does(blocks: Vec<MsgBlock>) {
        let id = CaudraId::generate();
        let live = pump_frames(|pump| pump, streamed_blocks(id, &blocks));
        let message = Message {
            role: Role::Assistant,
            content: blocks,
            ..Default::default()
        };
        let history = History::restored(expand_message_in(&message, None, id)).unwrap();
        let replayed: Vec<Value> =
            translate::replay_history(&ToolRegistry::default(), &history, Path::new(""), None)
                .into_iter()
                .map(|update| serde_json::to_value(update).unwrap())
                .collect();

        let live = chunks(live.iter().map(|frame| &frame["params"]["update"]));
        assert!(!live.is_empty());
        assert_eq!(live, chunks(&replayed));
    }

    #[test_case(done(DoneReason::Cancelled), "result" ; "cancelled_turn")]
    #[test_case(done(DoneReason::EndTurn), "result" ; "ended_turn")]
    #[test_case(failed(), "error" ; "failed_turn")]
    fn calls_left_open_fail_before_the_prompt_is_answered(end: AgentEvent, answer: &str) {
        let (pending, _cancel_rx, cancel) = prompted();
        let frames = pump_frames(
            |pump| EventPump {
                pending,
                cancel,
                ..pump
            },
            vec![shell_start(OPEN_CALL), end],
        );

        let [created, settled, answered] = frames.as_slice() else {
            panic!("a creation, its settlement, and the answer: {frames:?}");
        };
        assert_eq!(created["params"]["update"]["sessionUpdate"], "tool_call");
        let settled = &settled["params"]["update"];
        assert_eq!(settled["toolCallId"], OPEN_CALL);
        assert_eq!(settled["status"], FAILED);
        assert_eq!(
            settled["content"][0]["content"]["text"],
            translate::CANCELLED_TOOL_TEXT
        );
        assert_eq!(
            settled["_meta"][translate::META_NAMESPACE][translate::META_TOOL_NAME],
            SHELL_TOOL
        );
        assert_eq!(answered["id"], PROMPT_ID);
        assert!(answered.get(answer).is_some(), "{answered}");
    }

    #[test]
    fn a_refusal_ends_the_prompt_as_refused() {
        let (pending, _cancel_rx, cancel) = prompted();
        let frames = pump_frames(
            |pump| EventPump {
                pending,
                cancel,
                ..pump
            },
            vec![done(DoneReason::Refusal)],
        );
        assert_eq!(frames[0]["result"]["stopReason"], REFUSAL);
    }

    #[test_case(AUTH_FAILURE, i32::from(ErrorCode::AuthRequired) ; "auth_failure")]
    #[test_case(COMPACTION_FAILURE, i32::from(ErrorCode::InternalError) ; "other_failure")]
    fn a_failed_turn_answers_with_its_text(message: &str, code: i32) {
        let (pending, _cancel_rx, cancel) = prompted();
        let frames = pump_frames(
            |pump| EventPump {
                pending,
                cancel,
                ..pump
            },
            vec![AgentEvent::Error {
                message: message.into(),
            }],
        );

        let error = &frames[0]["error"];
        assert_eq!(error["code"], code);
        assert_eq!(error["message"], message);
        assert_eq!(error["data"][DATA_DETAILS], message);
    }

    /// The agent would wait for credentials the client cannot give it, so the
    /// prompt is answered and the run stopped.
    #[test]
    fn a_rejected_credential_mid_turn_asks_for_sign_in_and_stops_the_run() {
        let (pending, cancel_rx, cancel) = prompted();
        let frames = pump_frames(
            |pump| EventPump {
                pending,
                cancel,
                ..pump
            },
            vec![AgentEvent::AuthRequired],
        );

        let error = &frames[0]["error"];
        assert_eq!(error["code"], i32::from(ErrorCode::AuthRequired));
        assert_eq!(error["data"][DATA_DETAILS], AUTH_REQUIRED_DETAILS);
        assert_eq!(cancel_rx.try_recv(), Ok(()));
    }

    #[test]
    fn side_calls_count_toward_the_session_spend() {
        let frames = pump_frames(
            |pump| pump,
            vec![side_call(SIDE_CALL_COST), turn_complete(TURN_COST)],
        );
        let usage = &frames[0]["params"]["update"];
        assert_eq!(usage["sessionUpdate"], USAGE_UPDATE);
        assert_eq!(usage["cost"]["amount"], SIDE_CALL_COST + TURN_COST);
    }

    fn capable() -> ClientUpdates {
        ClientUpdates {
            compaction: true,
            notices: true,
        }
    }

    fn compaction(status: &str) -> String {
        format!("{COMPACTION_UPDATE} {status}")
    }

    #[test_case(ClientUpdates::default(), vec![titled()], vec![format!("{SESSION_INFO_UPDATE} {TITLE}")] ; "title_needs_no_capability")]
    #[test_case(ClientUpdates::default(), vec![AgentEvent::Compacting, AgentEvent::CompactionDone], vec![] ; "compaction_without_capability")]
    #[test_case(capable(), vec![AgentEvent::Compacting, AgentEvent::CompactionDone], vec![compaction(IN_PROGRESS), compaction(COMPLETED)] ; "auto_compaction")]
    #[test_case(capable(), vec![AgentEvent::Compacting, done(DoneReason::EndTurn)], vec![compaction(IN_PROGRESS), compaction(COMPLETED)] ; "manual_compaction_ends_with_its_run")]
    #[test_case(capable(), vec![AgentEvent::Compacting, done(DoneReason::Cancelled)], vec![compaction(IN_PROGRESS), compaction(CANCELLED)] ; "cancelled_compaction")]
    #[test_case(capable(), vec![AgentEvent::Compacting, failed()], vec![compaction(IN_PROGRESS), compaction(&format!("{FAILED} {COMPACTION_FAILURE}"))] ; "failed_compaction")]
    #[test_case(capable(), vec![done(DoneReason::EndTurn)], vec![] ; "a_run_without_compaction_reports_none")]
    #[test_case(capable(), vec![retry()], vec![format!("{NOTICE} {} {RETRY_ATTEMPT}", translate::RETRY_TITLE)] ; "retry_notice")]
    #[test_case(ClientUpdates::default(), vec![retry()], vec![] ; "retry_without_notices")]
    fn events_become_the_updates_the_client_renders(
        updates: ClientUpdates,
        events: Vec<AgentEvent>,
        expected: Vec<String>,
    ) {
        assert_eq!(pumped(updates, events), expected);
    }

    #[test]
    fn a_retry_notice_carries_its_attempt_and_delay() {
        let SessionUpdate::Notice(notice) =
            translate::retry_notice(RETRY_ATTEMPT, RETRY_MESSAGE.into(), RETRY_DELAY_MS)
        else {
            panic!("a retry is a notice");
        };
        assert_eq!(notice.description.as_deref(), Some(RETRY_MESSAGE));
        let retry = &notice.meta.unwrap()[translate::META_NAMESPACE][translate::META_RETRY];
        assert_eq!(retry[translate::META_ATTEMPT], RETRY_ATTEMPT);
        assert_eq!(retry[translate::META_DELAY_MS], RETRY_DELAY_MS);
    }

    #[test_case("/compact", true ; "compact_command")]
    #[test_case(UNKNOWN_COMMAND, false ; "unknown_command_is_a_turn")]
    fn a_slash_prompt_reaches_the_entry_point_it_names(text: &str, compacts: bool) {
        let mut test = TestServer::with_session();
        let (input_tx, input_rx) = flume::unbounded();
        let (compact_tx, compact_rx) = flume::unbounded();
        let session = test.session_mut();
        session.handle.input_tx = input_tx;
        session.handle.compact_tx = compact_tx;
        let request = PromptRequest::new(
            session.id(),
            vec![ContentBlock::Text(TextContent::new(text))],
        );

        session.prompt(RequestId::Number(1), &request).unwrap();

        assert_eq!(compact_rx.try_recv().is_ok(), compacts);
        let turn = input_rx.try_recv().ok().map(|input| input.message);
        assert_eq!(turn.as_deref(), (!compacts).then_some(text));
    }

    #[test]
    fn plan_mode_reuses_the_session_plan_across_switches() {
        let mut test = TestServer::with_session();
        let session = test.session_mut();
        let plan = PlanTarget::Local(PathBuf::from(PLAN_FILE));
        session.plan = Some(plan.clone());
        for (mode_id, expected) in [
            (methods::MODE_PLAN, AgentMode::planning(plan.clone())),
            (methods::MODE_BUILD, AgentMode::Build),
            (methods::MODE_PLAN, AgentMode::planning(plan.clone())),
        ] {
            session.switch_mode(mode_id).unwrap();
            assert_eq!(session.current_mode, expected);
            assert_eq!(session.plan.as_ref(), Some(&plan));
        }
    }

    #[test_case(Some(StoredMode::Plan), true; "plan_session")]
    #[test_case(Some(StoredMode::Build), false; "build_session")]
    #[test_case(None, false; "unrecorded_mode")]
    fn a_restored_local_plan_is_bound_in_any_mode(stored: Option<StoredMode>, planning: bool) {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join(PLAN_FILE);
        std::fs::write(&path, PLAN_CONTENT).unwrap();
        let session_ref = SessionRef::from(CaudraId::generate());
        let plan = restored_plan(
            Some(&stored_local_plan(&path)),
            None,
            &AcpRuntime::default(),
            &session_ref,
        );
        assert_eq!(plan, Some(PlanTarget::Local(path.clone())));
        let mut test = TestServer::with_session();
        let session = test.session_mut();
        session.current_mode = restored_mode(stored, plan.as_ref());
        session.plan = plan;
        assert_eq!(session.current_mode.is_planning(), planning);
        session.switch_mode(methods::MODE_PLAN).unwrap();
        assert_eq!(session.current_mode, AgentMode::Plan(path));
    }

    #[test]
    fn subagent_permission_is_forwarded_with_full_structured_details() {
        smol::block_on(async {
            let (event_tx, events) = flume::unbounded();
            let (outbound, out_rx) = Outbound::channel();
            let pending = PendingState::default();
            let scope = format!("line one\n{}", "x".repeat(512));
            let input = serde_json::json!({
                "command": scope,
                "nested": {"complete": true}
            });
            let mut request = PermissionRequest::from_legacy(
                CAUDRA_REQUEST_ID.into(),
                caudra_config::ToolKey::native("bash"),
                vec![scope.clone()],
                input.clone(),
                Path::new("/project"),
                false,
            );
            let action = request.presentation.action.clone();
            let subagent = Some(SubagentInfo {
                parent_tool_use_id: TASK_CALL.into(),
                task_id: "task-1".into(),
                name: "worker".into(),
                prompt: None,
                model: None,
                thinking: None,
                fast: false,
                answer_tx: None,
                steer_tx: None,
            });
            EventPump {
                pending: Arc::clone(&pending),
                cwd: PathBuf::from("/project"),
                ..test_pump(events, outbound)
            }
            .spawn(Written::settled());
            event_tx
                .send(Envelope {
                    event: AgentEvent::PermissionRequest(Box::new(request.clone())),
                    task: None,
                    subagent: subagent.clone(),
                    run_id: 0,
                    workflow: None,
                })
                .unwrap();

            let message = out_rx.recv_async().await.unwrap().message;
            assert_eq!(message["method"], SESSION_REQUEST_PERMISSION);
            assert_eq!(message["params"]["toolCall"]["title"], action);
            assert_eq!(message["params"]["toolCall"]["rawInput"], input);
            assert_eq!(
                message["params"]["toolCall"]["content"][0]["content"]["text"],
                permission_scope_summary(&[scope])
            );
            assert!(matches!(
                locked(&pending).asks.values().next(),
                Some(AskKind::Permission { request_id, .. }) if request_id == CAUDRA_REQUEST_ID
            ));
            request.presentation.advisories.push(PermissionAdvisory {
                flag: EngineFlag::Deletes,
                probability: ADVISORY_PROBABILITY,
            });
            event_tx
                .send(Envelope {
                    event: AgentEvent::PermissionRequestUpdated(Box::new(request)),
                    task: None,
                    subagent,
                    run_id: 0,
                    workflow: None,
                })
                .unwrap();
            let update = out_rx.recv_async().await.unwrap().message;
            assert_eq!(update["method"], "session/update");
            assert_eq!(update["params"]["update"]["toolCallId"], TASK_CALL);
            assert!(
                update["params"]["update"]["content"][0]["content"]["text"]
                    .as_str()
                    .unwrap()
                    .contains(DELETE_CAUTION)
            );
        });
    }

    #[test]
    fn each_session_names_tool_kinds_from_its_own_registry() {
        smol::block_on(async {
            let mut test = TestServer::with_session();
            let other = test.add_session();
            native::register(&test.session().runtime.registry, FeatureFlags::all()).unwrap();
            // Each pump stays fed, so none ends and settles its call mid-test.
            let mut feeds = Vec::new();
            // `other` is the schema default, so the wire omits it.
            for (sid, expected) in [
                (test.sid.clone(), Value::from(READ_KIND)),
                (other, Value::Null),
            ] {
                let (event_tx, events) = flume::unbounded();
                let mut pump =
                    test.active(&sid)
                        .event_pump(test.server.outbound.clone(), no_server(), None);
                pump.events = events;
                pump.spawn(Written::settled());
                event_tx
                    .send(Envelope {
                        event: AgentEvent::ToolStart(Box::new(ToolStartEvent {
                            id: VIEW_IMAGE_CALL.into(),
                            effect: ToolEffect::Unknown,
                            tool: Arc::from(VIEW_IMAGE_TOOL_NAME),
                            summary: String::new(),
                            render_header: None,
                            annotation: None,
                            input: None,
                            raw_input: None,
                            output: None,
                        })),
                        task: None,
                        subagent: None,
                        run_id: 0,
                        workflow: None,
                    })
                    .unwrap();
                feeds.push(event_tx);
                let update = test.frames.recv_async().await.unwrap().message;
                assert_eq!(update["params"]["sessionId"], sid.0.as_ref());
                assert_eq!(update["params"]["update"]["kind"], expected);
            }
        });
    }

    #[test]
    fn nothing_about_a_session_precedes_its_announcing_response() {
        smol::block_on(async {
            let (event_tx, events) = flume::unbounded();
            let (outbound, out_rx) = Outbound::channel();
            let (announced_tx, announced) = Written::gate();
            test_pump(events, outbound).spawn(announced);
            event_tx
                .send(Envelope {
                    event: AgentEvent::TextDelta {
                        text: PLAN_CONTENT.into(),
                    },
                    task: None,
                    subagent: None,
                    run_id: 0,
                    workflow: None,
                })
                .unwrap();
            drop(event_tx);
            assert!(out_rx.is_empty());
            announced_tx.send(()).unwrap();
            let update = out_rx.recv_async().await.unwrap().message;
            assert_eq!(update["params"]["update"]["content"]["text"], PLAN_CONTENT);
        });
    }

    fn run_envelope(event: AgentEvent, run_id: u64) -> Envelope {
        Envelope {
            run_id,
            ..envelope(event)
        }
    }

    /// Every update of a turn and its answer name the prompt that started it.
    #[test]
    fn a_turn_names_its_prompt_on_every_update_and_its_answer() {
        let pending = PendingState::default();
        locked(&pending).prompt = Some(OwedPrompt::new(
            RequestId::Number(PROMPT_ID),
            PromptIdentity::new(CLIENT_PROMPT_ID.into()),
        ));
        let frames = pump_frames(
            |pump| EventPump { pending, ..pump },
            vec![
                AgentEvent::TextDelta { text: TITLE.into() },
                shell_start(OPEN_CALL),
                turn_complete(TURN_COST),
                done(DoneReason::EndTurn),
            ],
        );

        let (answer, updates) = frames.split_last().expect("updates and an answer");
        assert_eq!(
            updates.len(),
            4,
            "chunk, call, usage, settled call: {updates:?}"
        );
        for update in updates {
            assert_eq!(
                update["params"]["_meta"][translate::META_NAMESPACE][META_PROMPT_ID],
                CLIENT_PROMPT_ID
            );
        }
        let caudra = &answer["result"]["_meta"][translate::META_NAMESPACE];
        assert_eq!(answer["id"], PROMPT_ID);
        assert_eq!(caudra[META_PROMPT_ID], CLIENT_PROMPT_ID);
        assert!(caudra[META_DURATION_MS].is_u64());
        assert_eq!(caudra["usage"]["costUsd"], TURN_COST);
    }

    fn started_run(run_id: u64, automatic: bool) -> InteractiveRun {
        InteractiveRun {
            run_id,
            started: Instant::now(),
            automatic,
            task_event_ids: Vec::new(),
            workflow_events: Vec::new(),
            automation_events: Vec::new(),
        }
    }

    /// A prompt queued behind a run the agent started on its own, such as
    /// one reporting a finished background task, is answered by its own run:
    /// the earlier run's updates name no prompt and its end answers nothing,
    /// however it ends.
    #[test_case(DoneReason::EndTurn ; "automatic_run_ends")]
    #[test_case(DoneReason::Cancelled ; "automatic_run_cancelled")]
    fn a_run_the_agent_started_on_its_own_answers_no_prompt(automatic_end: DoneReason) {
        smol::block_on(async {
            let (event_tx, events) = flume::unbounded();
            let (run_tx, runs) = flume::unbounded();
            let (outbound, out_rx) = Outbound::channel();
            let pending = PendingState::default();
            locked(&pending).prompt = Some(OwedPrompt::new(
                RequestId::Number(PROMPT_ID),
                PromptIdentity::new(CLIENT_PROMPT_ID.into()),
            ));
            EventPump {
                pending,
                runs,
                ..test_pump(events, outbound)
            }
            .spawn(Written::settled());

            run_tx.send(started_run(AUTOMATIC_RUN, true)).unwrap();
            for event in [shell_start(OPEN_CALL), done(automatic_end)] {
                event_tx.send(run_envelope(event, AUTOMATIC_RUN)).unwrap();
            }
            for kind in [TOOL_CALL, TOOL_CALL_UPDATE] {
                let frame = out_rx.recv_async().await.unwrap().message;
                let update = &frame["params"]["update"];
                assert_eq!(update["sessionUpdate"], kind, "{frame}");
                assert_eq!(update["toolCallId"], OPEN_CALL, "{frame}");
                assert!(frame["params"]["_meta"].is_null(), "{frame}");
            }

            run_tx.send(started_run(PROMPTED_RUN, false)).unwrap();
            for event in [
                AgentEvent::TextDelta { text: TITLE.into() },
                done(DoneReason::EndTurn),
            ] {
                event_tx.send(run_envelope(event, PROMPTED_RUN)).unwrap();
            }
            let chunk = out_rx.recv_async().await.unwrap().message;
            assert_eq!(chunk["params"]["update"]["content"]["text"], TITLE);
            assert_eq!(
                chunk["params"]["_meta"][translate::META_NAMESPACE][META_PROMPT_ID],
                CLIENT_PROMPT_ID
            );
            let answered = out_rx.recv_async().await.unwrap().message;
            assert_eq!(answered["id"], PROMPT_ID);
            assert_eq!(answered["result"]["stopReason"], END_TURN);
        });
    }

    /// A cancelled turn whose agent never reports its end still gets its open
    /// calls failed and its prompt answered, once; the late end answers nothing.
    #[test]
    fn a_cancelled_turn_that_never_ends_is_answered_after_the_drain_wait() {
        smol::block_on(async {
            let (event_tx, events) = flume::unbounded();
            let (outbound, out_rx) = Outbound::channel();
            let (pending, _cancel_rx, cancel) = prompted();
            EventPump {
                pending: Arc::clone(&pending),
                cancel,
                drain_timeout: Duration::ZERO,
                ..test_pump(events, outbound)
            }
            .spawn(Written::settled());
            event_tx
                .send(run_envelope(shell_start(OPEN_CALL), STALE_RUN))
                .unwrap();
            let created = out_rx.recv_async().await.unwrap().message;
            assert_eq!(created["params"]["update"]["toolCallId"], OPEN_CALL);

            {
                let mut owed = locked(&pending);
                owed.prompt.as_mut().unwrap().cancelled = true;
                owed.cancel_signal.raise();
            }

            let settled = out_rx.recv_async().await.unwrap().message;
            assert_eq!(settled["params"]["update"]["status"], FAILED);
            let answered = out_rx.recv_async().await.unwrap().message;
            assert_eq!(answered["id"], PROMPT_ID);
            assert_eq!(answered["result"]["stopReason"], CANCELLED);

            locked(&pending).prompt = Some(owed(SECOND_PROMPT));
            event_tx
                .send(run_envelope(done(DoneReason::Cancelled), STALE_RUN))
                .unwrap();
            event_tx
                .send(run_envelope(
                    AgentEvent::TextDelta {
                        text: END_MARKER.into(),
                    },
                    STALE_RUN + 1,
                ))
                .unwrap();
            let next = out_rx.recv_async().await.unwrap().message;
            assert_eq!(next["params"]["update"]["content"]["text"], END_MARKER);
            assert!(
                locked(&pending).prompt.is_some(),
                "{STALE_END_ANSWERS_NOTHING}"
            );
        });
    }
}
