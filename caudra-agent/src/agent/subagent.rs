//! Subagent sessions: a child agent with its own history, tools, and cancel
//! scope, whose events are relayed to the parent stamped with its identity.
//!
//! The native `task` tool and the workflow engine (both through
//! [`crate::agent::task_runner`]) and `caudra.agent.session` open sessions
//! through here. The task path resolves its prompt and tools from a profile;
//! both paths use the Subagent model purpose unless explicitly overridden.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use async_lock::Mutex as AsyncMutex;
use serde_json::Value as JsonValue;
use tracing::info;

use caudra_providers::model::{Model, ModelPurpose};
use caudra_providers::model_registry::Binding;
use caudra_providers::provider;
use caudra_providers::{
    CacheKey, HistoryItem, Message, ThinkingConfig, TokenUsage, add_cost, expand_message,
};
use caudra_storage::id::CaudraId;

use super::steering::{SharedSteering, Steering};
use super::{ModelRoute, resolve_model_for_purpose};
use crate::cancel::{CancelMap, CancelSlot};
use crate::prompt::PromptId;
use crate::prompt::profile::SystemPromptProfile;
use crate::tools::native::batch::MAX_BATCH_SIZE;
use crate::tools::{
    BuiltinDeferral, DeferredTool, DescriptionContext, FileReadTracker, LocalTools, ToolAudience,
    ToolContext, ToolFilter, ToolLive, deferral,
};
use crate::{
    ActivityChild, Agent, AgentEvent, AgentInput, AgentMode, AgentParams, AgentRunParams,
    DoneReason, Envelope, EventSender, History, InterruptSource, McpSession, SteeringQueue,
    SteeringQueueReceiver, SubagentActivity, SubagentHistoryError, SubagentHistoryLease,
    SubagentInfo, SubagentProgress, SubagentTaskMode, SubagentTaskSpec, SubagentTaskSpecCandidate,
    ToolOutput, reasoning_summary, steering_queue,
};

pub const STRUCTURED_OUTPUT_TOOL: &str = "structured_output";
pub const BUILTIN_TASK_PROFILE_DESCRIPTION: &str = "Caudra\'s built-in task prompt";
pub const SESSION_CLOSED: &str = "session closed";
pub const CANCELLED: &str = "cancelled";
pub(super) const TURN_LIMIT: &str = "subagent reached its maximum turn limit";
pub(super) const TRUNCATED: &str = "subagent response was cut off at its output token limit";
const DEFAULT_SESSION_AUDIENCE: ToolAudience = ToolAudience::GENERAL_SUB;
/// A thought\'s title is its first bold line. Past this the block is into
/// prose and will never resolve one, so it stops being accumulated and a
/// long reasoning stream cannot be buffered a second time for nothing.
const THOUGHT_TITLE_SCAN_LIMIT: usize = 200;

/// Forwards subagent events to the parent, stamped with the subagent identity.
/// Usage takes two paths: live on the tool header while the run goes on (last
/// turn's tokens plus the run's summed cost), and a Done barrier on
/// `usage_tx`, which `prompt` drains. Progress takes the same two paths, so
/// a standalone task and a batch child both report the same run.
async fn relay_session_events(
    sub_rx: flume::Receiver<Envelope>,
    parent_tx: EventSender,
    subagent_info: Arc<OnceLock<SubagentInfo>>,
    usage_tx: flume::Sender<TokenUsage>,
    live_sink: Option<flume::Sender<ToolLive>>,
) {
    let mut cost = None;
    let mut progress = ProgressRelay::new();
    while let Ok(mut envelope) = sub_rx.recv_async().await {
        progress.relay(&envelope, &parent_tx, &subagent_info, live_sink.as_ref());
        match &envelope.event {
            AgentEvent::TurnComplete(turn) => {
                add_cost(&mut cost, turn.cost);
                if let Some(sink) = &live_sink {
                    let _ = sink.send(ToolLive::Usage(turn.usage.format_sum_cost(cost)));
                }
            }
            AgentEvent::Done { usage, .. } => {
                let _ = usage_tx.send(*usage);
                continue;
            }
            // A failed subagent reaches the user as its task tool result, and
            // the parent's own session must not take a child's verdict as its
            // own: in the UI an error ends the session it arrives on.
            AgentEvent::Error { .. } => continue,
            // Already stamped, so this is a grandchild's live tool work and
            // this session sits blocked on the nested task that ran it.
            // Restamping would open a card in this session's transcript for a
            // call it never made, and a `ToolOutput` flush carries the whole
            // accumulated buffer, recloned at every hop it crosses.
            AgentEvent::ToolPending { .. }
            | AgentEvent::ToolInputDelta { .. }
            | AgentEvent::ToolOutput { .. }
                if envelope.subagent.is_some() =>
            {
                continue;
            }
            _ => {}
        }
        envelope.subagent = subagent_info.get().cloned();
        let _ = parent_tx.send_envelope(envelope);
    }
}

/// The batch a subagent is working through, so each child's state change can
/// patch one row instead of resending the roster the `ToolStart` already gave.
struct BatchWatch {
    id: String,
    children: Vec<ActivityChild>,
}

/// Keeps the running digest one subagent reports to its parent.
struct ProgressRelay {
    started: Instant,
    tools: u32,
    thought: String,
    thought_title: Option<String>,
    last: Option<SubagentActivity>,
    /// One slot, because `batch` refuses a nested `batch` child and a subagent
    /// runs one call at a time.
    batch: Option<BatchWatch>,
}

impl ProgressRelay {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            tools: 0,
            thought: String::new(),
            thought_title: None,
            last: None,
            batch: None,
        }
    }

    /// What one event adds to the tally. A batch dispatches its children with
    /// `Emit::Capture`, so they never reach this stream as starts of their
    /// own: counting the batch as one call would report a fan-out of twenty
    /// as a single tool.
    fn counted(event: &AgentEvent) -> u32 {
        match event {
            AgentEvent::ToolStart(start) => match &start.output {
                Some(ToolOutput::Batch { entries, .. }) => entries.len() as u32,
                _ => 1,
            },
            _ => 0,
        }
    }

    /// The roster row this activity should carry, rebuilt from whatever the
    /// watched batch knows. `None` when nothing is being watched, or when the
    /// last activity was not the batch's own row.
    fn watched_batch(&self) -> Option<SubagentActivity> {
        let watch = self.batch.as_ref()?;
        match &self.last {
            Some(SubagentActivity::Tool {
                name,
                summary,
                call_id: Some(call_id),
                ..
            }) if call_id == &watch.id => Some(
                SubagentActivity::batch(Arc::clone(name), summary, watch.children.clone())
                    .with_call_id(&watch.id),
            ),
            _ => None,
        }
    }

    /// The stateful half of [`SubagentActivity::from_event`]: a thought names
    /// itself over several deltas, and a tool can rename itself long after it
    /// started, so both need what came before.
    fn activity(&mut self, event: &AgentEvent) -> Option<SubagentActivity> {
        match event {
            AgentEvent::ThinkingDelta { text } => {
                // The title fence can straddle two deltas, so once it parses
                // it is kept: a heading that blinks off reads as a fault.
                if self.thought_title.is_none() && self.thought.len() < THOUGHT_TITLE_SCAN_LIMIT {
                    self.thought.push_str(text);
                    self.thought_title = reasoning_summary(&self.thought).title.map(str::to_owned);
                }
                Some(SubagentActivity::Thinking {
                    title: self.thought_title.clone(),
                })
            }
            // The header a plugin paints mid-run is the one its transcript
            // shows, and it arrives without a tool name to match on. The
            // roster survives the retitle: it describes the same call.
            AgentEvent::ToolHeaderSnapshot { id, snapshot, .. } => match &self.last {
                Some(SubagentActivity::Tool {
                    name,
                    children,
                    call_id: Some(call_id),
                    ..
                }) if call_id == id => Some(
                    SubagentActivity::batch(
                        Arc::clone(name),
                        &snapshot.first_line_text(),
                        children.clone(),
                    )
                    .with_call_id(id),
                ),
                _ => None,
            },
            // A batch hands its whole roster over in its start event, which is
            // the only place the parent ever sees it: the children run under
            // captured starts that never reach this stream.
            AgentEvent::ToolStart(start) => {
                self.thought.clear();
                self.thought_title = None;
                self.batch = match &start.output {
                    Some(ToolOutput::Batch { entries, .. }) => Some(BatchWatch {
                        id: start.id.clone(),
                        children: entries
                            .iter()
                            .take(MAX_BATCH_SIZE)
                            .map(ActivityChild::from)
                            .collect(),
                    }),
                    _ => None,
                };
                Some(
                    match &self.batch {
                        Some(watch) => SubagentActivity::batch(
                            Arc::clone(&start.tool),
                            &start.summary,
                            watch.children.clone(),
                        ),
                        None => SubagentActivity::tool(Arc::clone(&start.tool), &start.summary),
                    }
                    .with_call_id(&start.id),
                )
            }
            // One child moved. The row it names is patched in place, and the
            // digest republished so the parent's tree redraws that row alone.
            AgentEvent::BatchProgress(event) => {
                let watch = self.batch.as_mut().filter(|w| w.id == event.id)?;
                let child = watch.children.get_mut(event.index)?;
                *child = ActivityChild::from(&event.entry);
                self.watched_batch()
            }
            // Whatever the roster ends on is what the last `BatchProgress`
            // published, so the batch is only forgotten here.
            AgentEvent::ToolDone(done) => {
                if self.batch.as_ref().is_some_and(|w| w.id == done.id) {
                    self.batch = None;
                }
                None
            }
            _ => {
                let activity = SubagentActivity::from_event(event);
                // A turn ends the thought even with nothing after it, and the
                // next one must not inherit this heading.
                if activity.is_some() || matches!(event, AgentEvent::TurnComplete(_)) {
                    self.thought.clear();
                    self.thought_title = None;
                }
                activity
            }
        }
    }

    /// Publishes on every change, and only on a change: text and thinking
    /// arrive one delta at a time and would otherwise repaint the parent
    /// header on every token.
    ///
    /// An envelope that already carries a subagent came from deeper down, and
    /// this session is blocked on the nested task that produced it, so
    /// relaying a grandchild's progress here would read as this session doing
    /// that work.
    fn relay(
        &mut self,
        envelope: &Envelope,
        parent_tx: &EventSender,
        subagent_info: &OnceLock<SubagentInfo>,
        live_sink: Option<&flume::Sender<ToolLive>>,
    ) {
        if envelope.subagent.is_some() {
            return;
        }
        // `ToolPending` announces the same call, so counting that instead
        // would double every tool the subagent runs.
        let counted = Self::counted(&envelope.event);
        self.tools += counted;
        let Some(activity) = self.activity(&envelope.event) else {
            return;
        };
        if counted == 0 && self.last.as_ref() == Some(&activity) {
            return;
        }
        self.last = Some(activity.clone());
        let progress = SubagentProgress {
            activity,
            tools: self.tools,
            elapsed: self.started.elapsed(),
        };
        if let Some(sink) = live_sink {
            let _ = sink.send(ToolLive::Progress(progress.clone()));
        }
        let _ = parent_tx.send_envelope(Envelope {
            event: AgentEvent::SubagentProgress { progress },
            subagent: subagent_info.get().cloned(),
            run_id: parent_tx.run_id(),
            workflow: envelope.workflow.clone(),
        });
    }
}

pub struct Subagent {
    params: AgentParams,
    system: String,
    tools: JsonValue,
    deferred: Vec<DeferredTool>,
    mode: AgentMode,
    environment: Option<String>,
    mode_notice: Option<String>,
    thinking: ThinkingConfig,
    fast: bool,
    /// Fresh per session so `tool_search` loads never leak between a
    /// subagent and its parent.
    mcp: Option<McpSession>,
    history: History,
    history_lease: Option<SubagentHistoryLease>,
    sub_event_tx: EventSender,
    child_cancel: crate::cancel::CancelToken,
    interrupt_source: Arc<SteeringQueueReceiver>,
    answer_rx: Arc<AsyncMutex<flume::Receiver<String>>>,
    answer_tx: Option<flume::Sender<String>>,
    steer_tx: Option<SteeringQueue>,
    parent_cancels: Arc<CancelMap<String>>,
    parent_tool_use_id: String,
    root_tool_use_id: String,
    task_id: String,
    /// Which cancellation registration under `task_id` is ours.
    cancel_slot: CancelSlot,
    parent_event_tx: EventSender,
    subagent_info: Arc<OnceLock<SubagentInfo>>,
    local_tools: LocalTools,
    name: String,
    usage: TokenUsage,
    usage_rx: flume::Receiver<TokenUsage>,
    start: Instant,
    closed: bool,
    steering: SharedSteering,
    report_ready: Option<Arc<AtomicBool>>,
}

impl Subagent {
    pub fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.parent_cancels.retire(&self.task_id, self.cancel_slot);
        let messages = std::mem::replace(&mut self.history, History::new(Vec::new())).into_vec();
        let persisted_spec = self
            .history_lease
            .as_ref()
            .and_then(|lease| lease.spec().cloned());
        if let Some(lease) = self.history_lease.take() {
            lease.complete_version(Arc::new(messages.clone()), self.parent_tool_use_id.clone());
        }
        let _ = self.parent_event_tx.send(AgentEvent::SubagentHistory {
            task_id: self.task_id.clone(),
            parent_tool_use_id: self.parent_tool_use_id.clone(),
            root_tool_use_id: self.root_tool_use_id.clone(),
            name: self.name.clone(),
            model: self.params.model.spec(),
            messages,
            spec: persisted_spec,
        });
        info!(
            name = %self.name,
            duration_ms = self.start.elapsed().as_millis() as u64,
            input_tokens = self.usage.total_input(),
            output_tokens = self.usage.output,
            "subagent session closed",
        );
    }
}

/// What one `prompt` call produced. `text` is empty when the subagent only
/// called tools and never wrote a message.
pub struct PromptResult {
    pub text: String,
    pub duration: std::time::Duration,
    pub input_tokens: u32,
    pub output_tokens: u32,
}

/// A run that did not finish. `partial` carries whatever the subagent had
/// already streamed, because half a transcript beats a bare error.
pub struct PromptFailure {
    pub error: String,
    pub partial: Option<String>,
}

impl Subagent {
    pub fn id(&self) -> &str {
        &self.task_id
    }

    /// The mode this session actually runs under, which is not always what the
    /// caller asked for: an omitted mode is inherited and an over-reaching one
    /// is clamped.
    pub fn task_mode(&self) -> SubagentTaskMode {
        match self.mode {
            AgentMode::ReadOnly => SubagentTaskMode::Plan,
            _ => SubagentTaskMode::Build,
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Tokens every prompt of this session has consumed so far.
    pub fn usage(&self) -> TokenUsage {
        self.usage
    }

    /// `None` resumes: the subagent picks its own history back up with no new
    /// instruction, which is all a caller continuing an interrupted task has
    /// to say.
    pub async fn prompt(&mut self, message: Option<String>) -> Result<PromptResult, PromptFailure> {
        self.ensure_open()?;
        // Only an external prompt or explicit resume starts a new invocation budget.
        // Automatic report corrections rebuild Agent but retain this shared state.
        self.steering = Arc::new(Mutex::new(Steering::new(
            self.params
                .config
                .steering
                .resolve(&self.params.model.spec()),
        )));
        if let Some(ready) = &self.report_ready {
            ready.store(false, Ordering::Release);
        }
        let resume = message.is_none();
        self.prompt_inner(message, Vec::new(), resume).await
    }

    pub(crate) fn with_report_ready(mut self, ready: Arc<AtomicBool>) -> Self {
        self.report_ready = Some(ready);
        self
    }

    pub(crate) async fn correct_report(
        &mut self,
        validating: bool,
    ) -> Result<Option<PromptResult>, PromptFailure> {
        self.ensure_open()?;
        if self
            .steering
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .turn_limit_reached(self.params.config.max_turns)
        {
            return Err(PromptFailure {
                error: TURN_LIMIT.into(),
                partial: None,
            });
        }
        let correction = self
            .steering
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .report_correction(validating)
            .map_err(|error| PromptFailure {
                error: error.to_string(),
                partial: None,
            })?;
        let Some(correction) = correction else {
            return Ok(None);
        };
        // The preamble preserves host provenance without introducing a user turn
        // or the separate continuation instruction attached to explicit resume.
        self.prompt_inner(None, vec![correction], false)
            .await
            .map(Some)
    }

    fn ensure_open(&self) -> Result<(), PromptFailure> {
        if self.closed || self.child_cancel.is_cancelled() {
            return Err(PromptFailure {
                error: if self.closed {
                    SESSION_CLOSED
                } else {
                    CANCELLED
                }
                .to_owned(),
                partial: None,
            });
        }
        Ok(())
    }

    async fn prompt_inner(
        &mut self,
        message: Option<String>,
        preamble: Vec<Message>,
        resume: bool,
    ) -> Result<PromptResult, PromptFailure> {
        self.ensure_open()?;
        // The first prompt is what names the subagent in the parent's UI, so
        // the identity is published here rather than at open time.
        if self.subagent_info.get().is_none() {
            let _ = self.subagent_info.set(SubagentInfo {
                parent_tool_use_id: self.parent_tool_use_id.clone(),
                task_id: self.task_id.clone(),
                name: self.name.clone(),
                prompt: message.clone(),
                model: Some(self.params.model.spec()),
                thinking: self
                    .params
                    .model
                    .supports_thinking()
                    .then(|| self.thinking.resolve(&self.params.model).to_string()),
                fast: self.fast,
                answer_tx: self.answer_tx.take(),
                steer_tx: self.steer_tx.take(),
            });
        }

        let interrupt_source: Arc<dyn InterruptSource> = self.interrupt_source.clone();
        let mut agent = Agent::new(
            self.params.clone(),
            AgentRunParams {
                environment: self.environment.clone(),
                instructions: None,
                mode_notice: self.mode_notice.clone(),
                history: &mut self.history,
                system: self.system.clone(),
                event_tx: self.sub_event_tx.clone(),
                tools: self.tools.clone(),
                deferred: self.deferred.clone(),
            },
        )
        .with_user_response_rx(Arc::clone(&self.answer_rx))
        .with_interrupt_source(interrupt_source)
        .with_cancel(self.child_cancel.clone())
        .with_mcp(self.mcp.clone())
        .with_local_tools(Arc::clone(&self.local_tools))
        .with_steering(Arc::clone(&self.steering));
        if let Some(ready) = &self.report_ready {
            agent = agent.with_report_ready(Arc::clone(ready));
        }

        let result = agent
            .run(AgentInput {
                resume,
                message: message.unwrap_or_default(),
                mode: self.mode.clone(),
                images: Vec::new(),
                mentions: Vec::new(),
                preamble,
                thinking: self.thinking.clone(),
                fast: self.fast,
                prompt: None,
            })
            .await;
        // Compaction can replace the transcript, invalidating its old length as
        // an output boundary. The agent's run-local response survives that rewrite.
        let text = agent.response_text().unwrap_or_default().to_owned();
        // Agent owns completed billing, including compaction and evaluation. Read
        // it before drop even on errors, which deliberately do not emit Done.
        self.usage += agent.usage();
        drop(agent);
        // Consume every emitted Done, including cancellation and turn-limit exits,
        // so a later explicit invocation cannot read a stale usage total. Provider
        // errors do not emit Done and must not wait for the relay.
        if result.is_ok() {
            match self.usage_rx.recv_async().await {
                Ok(_) => {}
                Err(_) => tracing::warn!(
                    name = %self.name,
                    "subagent event relay stopped before Done"
                ),
            }
        }
        // A normal runtime exit can still leave the task unfinished. Only a
        // completed response may enter task-report validation or correction.
        let cut_short = match &result {
            Err(error) => Some(error.to_string()),
            Ok(DoneReason::Cancelled) => Some(CANCELLED.to_owned()),
            Ok(DoneReason::MaxTurns) => Some(TURN_LIMIT.to_owned()),
            Ok(DoneReason::MaxTokens) => Some(TRUNCATED.to_owned()),
            Ok(_) => None,
        };
        if let Some(error) = cut_short {
            return Err(PromptFailure {
                error,
                partial: (!text.is_empty()).then_some(text),
            });
        }
        Ok(PromptResult {
            text,
            duration: self.start.elapsed(),
            input_tokens: self.usage.total_input(),
            output_tokens: self.usage.output,
        })
    }
}

/// Everything both openers must decide before a [`Subagent`] can exist.
struct Resolved {
    model: Model,
    provider: Arc<dyn provider::Provider>,
    system: String,
    tools: JsonValue,
    /// Held back behind `tool_search` for this child. A subagent starts with
    /// nothing loaded: what the parent searched for says nothing about what
    /// the child needs.
    deferred: Vec<DeferredTool>,
    mode: AgentMode,
    /// Announced to the child rather than assembled into `system`, so its
    /// prompt carries nothing that varies between runs. Both are `None` for a
    /// generic session, whose caller-authored prompt never agreed to the
    /// reminder contract.
    environment: Option<String>,
    mode_notice: Option<String>,
    audience: ToolAudience,
    thinking: ThinkingConfig,
    mcp_enabled: bool,
    task_id: String,
    history_lease: SubagentHistoryLease,
    default_task_prompt_profile_name: Arc<str>,
    active_prompt_profile_name: Option<Arc<str>>,
}

/// How a task session is named, and whether it starts from history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskIdentity {
    /// A new task named after the calling tool use, as the `task` tool does.
    Derive,
    /// A new task under an id the caller chose, such as a workflow engine
    /// that must find the same task again after a restart.
    Fresh(String),
    /// Continues an earlier task's history.
    Continue(String),
}

impl TaskIdentity {
    /// The `task` tool's contract: a task id means resume, none means start.
    pub fn continue_or_derive(task_id: Option<String>) -> Self {
        task_id.map_or(Self::Derive, Self::Continue)
    }

    pub fn is_continuation(&self) -> bool {
        matches!(self, Self::Continue(_))
    }

    fn requested(&self) -> Option<&str> {
        match self {
            Self::Derive => None,
            Self::Fresh(id) | Self::Continue(id) => Some(id),
        }
    }
}

/// A `task` call: the profile may override the Subagent model and thinking;
/// prompt, tools, and mode otherwise come from task policy.
pub struct TaskOptions {
    pub name: String,
    pub task_id: TaskIdentity,
    pub profile: Option<String>,
    pub mode: Option<SubagentTaskMode>,
    /// Which model job runs this one task, when the caller knows what kind of
    /// work it is. A profile that pins `subagent_model` still wins, because the
    /// user configured that and the caller only asked for a job. Not persisted
    /// with the task, so a continuation resolves the model the ordinary way.
    pub model_job: Option<ModelPurpose>,
    /// Tool definitions the subagent sees on top of the registry's, paired
    /// with `local_tools` by name.
    pub local_definitions: Vec<JsonValue>,
    pub local_tools: LocalTools,
}

/// A plugin-defined session: the caller brings its own prompt and tools, and
/// may replace the Subagent model with an exact spec.
pub struct GenericOptions {
    pub name: String,
    pub task_id: Option<String>,
    pub model_spec: Option<String>,
    pub system: String,
    pub tools: JsonValue,
    pub audience: Option<ToolAudience>,
    pub thinking: Option<ThinkingConfig>,
    pub fast: Option<bool>,
    pub mcp: Option<bool>,
    pub local_tools: LocalTools,
}

pub async fn open_task(ctx: &ToolContext, opts: TaskOptions) -> Result<Subagent, String> {
    let ids = Identity::derive(ctx, opts.task_id.requested());
    let default_spec = SubagentTaskSpec {
        profile_name: ctx.default_task_prompt_profile_name.to_string(),
        mode: inherited_mode(&ctx.mode),
        ..SubagentTaskSpec::default()
    };
    let (task_id, history_lease) = match opts.task_id {
        TaskIdentity::Continue(_) => (
            ids.task_id.clone(),
            ctx.subagent_history
                .continue_task_with_defaults(
                    &ids.task_id,
                    SubagentTaskSpecCandidate {
                        profile_name: opts.profile,
                        mode: opts.mode,
                    },
                    default_spec,
                )
                .map_err(|error| error.to_string())?,
        ),
        TaskIdentity::Derive | TaskIdentity::Fresh(_) => {
            let spec = SubagentTaskSpec {
                profile_name: opts.profile.unwrap_or(default_spec.profile_name),
                mode: opts.mode.unwrap_or(default_spec.mode),
                ..SubagentTaskSpec::default()
            };
            reserve_fresh(ctx, ids.task_id.clone(), Some(spec))?
        }
    };
    let spec = history_lease
        .spec()
        .cloned()
        .expect("task leases always carry a specification");
    if spec.mode == SubagentTaskMode::Build && !matches!(ctx.mode, AgentMode::Build) {
        return Err(BUILD_FROM_READ_ONLY.into());
    }

    let bindings = ctx.prompt_profiles.bind_for_tasks(
        &ctx.model,
        &ctx.chat_model,
        &ctx.opts.thinking,
        &ctx.model_policy,
        ctx.timeouts,
    );
    ctx.prompt_profiles
        .resolve(Some(&spec.profile_name))
        .map_err(|error| error.to_string())?;
    let profile = bindings
        .resolve(&spec.profile_name)
        .map_err(|error| error.to_string())?;

    let asked = opts.model_job.map(Binding::Same);
    let model_binding = subagent_model_binding(profile.as_deref(), asked.as_ref());
    let (model, provider) = resolve_provider(ctx, model_binding).await?;
    announce_model(ctx, &model);

    let thinking = profile
        .as_deref()
        .and_then(|profile| profile.subagent_thinking().cloned())
        .map(ThinkingConfig::from)
        .unwrap_or_else(|| ctx.opts.thinking.clone());
    if profile.as_ref().is_some_and(|profile| {
        profile.subagent_model().is_some() || profile.subagent_thinking().is_some()
    }) && let Err(error) = thinking.resolve_exact(&model)
    {
        return Err(format!(
            "system prompt profile {:?} is unavailable for subagents: thinking {thinking} is incompatible with model {:?}: {error}",
            spec.profile_name,
            model.spec()
        ));
    }

    let (mode, prompt_id, mode_notice, audience) = match spec.mode {
        SubagentTaskMode::Plan => (
            AgentMode::ReadOnly,
            PromptId::Research,
            crate::prompt::TASK_PLAN_CONTRACT,
            ToolAudience::RESEARCH_SUB,
        ),
        SubagentTaskMode::Build => (
            AgentMode::Build,
            PromptId::General,
            crate::prompt::TASK_BUILD_CONTRACT,
            ToolAudience::GENERAL_SUB,
        ),
    };
    let vars = ctx.task_environment.clone().set(
        "{task_system_prompt_profiles}",
        bindings.task_tool_summary(BUILTIN_TASK_PROFILE_DESCRIPTION),
    );
    let instructions = match &ctx.remote_project_context {
        Some(context) => crate::agent::load_remote_instructions(context).text,
        None => {
            let cwd = vars.apply("{cwd}").into_owned();
            smol::unblock(move || crate::agent::load_instruction_text(&cwd)).await
        }
    };
    let base_filter = ToolFilter::from_config(&ctx.config, &model, &[]).for_mode(&mode);
    let assembled = crate::prompt::assemble_task_with_filter(
        prompt_id,
        &ctx.prompt_slots,
        &base_filter,
        &instructions,
        profile.as_deref(),
    );
    let mut definitions = ctx.registry.definitions_split(
        &vars,
        &DescriptionContext {
            filter: &base_filter,
            audience,
            workflows_available: false,
        },
        model.supports_tool_examples(),
        &deferral::deferred_names(
            &ctx.config.allowed_tools,
            BuiltinDeferral::resolve(&ctx.config, &model),
        ),
    );
    definitions
        .declared
        .as_array_mut()
        .expect("definitions return an array")
        .extend(opts.local_definitions);
    let profile_name: Arc<str> = Arc::from(spec.profile_name.as_str());
    // The child's own model, which a profile's `subagent_model` can move away
    // from the parent's.
    let environment = crate::agent::environment_block(&vars, &model);

    build(
        ctx,
        ids,
        Resolved {
            model,
            provider,
            system: vars.apply(&assembled).into_owned(),
            tools: definitions.declared,
            deferred: definitions.deferred,
            mode,
            environment: Some(environment),
            mode_notice: Some(mode_notice.to_owned()),
            audience,
            thinking,
            mcp_enabled: spec.mode == SubagentTaskMode::Build,
            task_id,
            history_lease,
            default_task_prompt_profile_name: Arc::clone(&profile_name),
            active_prompt_profile_name: Some(profile_name),
        },
        opts.local_tools,
        ctx.opts.fast,
        opts.name,
    )
}

pub async fn open_generic(ctx: &ToolContext, opts: GenericOptions) -> Result<Subagent, String> {
    if !matches!(ctx.mode, AgentMode::Build) {
        return Err(GENERIC_FROM_READ_ONLY.into());
    }
    let ids = Identity::derive(ctx, opts.task_id.as_deref());
    let (task_id, history_lease) = match opts.task_id {
        Some(_) => (
            ids.task_id.clone(),
            ctx.subagent_history
                .continue_task(&ids.task_id)
                .map_err(|error| error.to_string())?,
        ),
        None => reserve_fresh(ctx, ids.task_id.clone(), None)?,
    };
    let model_binding = opts.model_spec.map(Binding::Exact);
    let (model, provider) = resolve_provider(ctx, model_binding.as_ref()).await?;
    announce_model(ctx, &model);
    build(
        ctx,
        ids,
        Resolved {
            model,
            provider,
            system: opts.system,
            tools: opts.tools,
            deferred: Vec::new(),
            mode: AgentMode::Build,
            environment: None,
            mode_notice: None,
            audience: opts.audience.unwrap_or(DEFAULT_SESSION_AUDIENCE),
            thinking: opts.thinking.unwrap_or_else(|| ctx.opts.thinking.clone()),
            mcp_enabled: opts.mcp.unwrap_or(true),
            task_id,
            history_lease,
            default_task_prompt_profile_name: Arc::clone(&ctx.default_task_prompt_profile_name),
            active_prompt_profile_name: None,
        },
        opts.local_tools,
        opts.fast.unwrap_or(ctx.opts.fast),
        opts.name,
    )
}

/// What a task runs as when its caller did not say. A parent hands down the
/// authority it holds and no more, so delegating from build mode delegates the
/// ability to build; everything read-only stays read-only.
pub fn inherited_mode(mode: &AgentMode) -> SubagentTaskMode {
    match mode {
        AgentMode::Build => SubagentTaskMode::Build,
        AgentMode::ReadOnly | AgentMode::Plan(_) | AgentMode::RemotePlan(_) => {
            SubagentTaskMode::Plan
        }
    }
}

pub const BUILD_FROM_READ_ONLY: &str =
    "build-mode task cannot be launched from a read-only or plan-mode parent";
pub const GENERIC_FROM_READ_ONLY: &str =
    "generic subagent sessions cannot be launched from a read-only or plan-mode parent";

/// The call ids a subagent inherits. `parent_tool_use_id` roots its events in
/// the parent's transcript; `task_id` is what a continuation names.
struct Identity {
    parent_tool_use_id: String,
    root_tool_use_id: String,
    task_id: String,
}

impl Identity {
    /// `requested` names the task outright, continued or fresh; without it
    /// the task takes the calling tool use's id.
    fn derive(ctx: &ToolContext, requested: Option<&str>) -> Self {
        let parent_tool_use_id = ctx.tool_use_id.clone().unwrap_or_else(generated_session_id);
        Self {
            root_tool_use_id: ctx
                .root_tool_use_id
                .clone()
                .unwrap_or_else(|| parent_tool_use_id.clone()),
            task_id: requested
                .map(str::to_owned)
                .unwrap_or_else(|| parent_tool_use_id.clone()),
            parent_tool_use_id,
        }
    }
}

/// Roots a subagent launched from outside any tool call, such as a plugin
/// session or a `call_tool` bridge.
pub fn generated_session_id() -> String {
    format!("session-{}", CaudraId::generate())
}

/// A tool call that ran twice under one id collides with its own history, so
/// a taken id is answered with a fresh one rather than an error.
fn reserve_fresh(
    ctx: &ToolContext,
    task_id: String,
    spec: Option<SubagentTaskSpec>,
) -> Result<(String, SubagentHistoryLease), String> {
    let reserve = |id: String| match &spec {
        Some(spec) => ctx.subagent_history.reserve_with_spec(id, spec.clone()),
        None => ctx.subagent_history.reserve(id),
    };
    match reserve(task_id.clone()) {
        Ok(lease) => Ok((task_id, lease)),
        Err(
            SubagentHistoryError::AlreadyActive { .. }
            | SubagentHistoryError::AlreadyCompleted { .. },
        ) => {
            let fresh = generated_session_id();
            let lease = reserve(fresh.clone()).map_err(|error| error.to_string())?;
            Ok((fresh, lease))
        }
        Err(error) => Err(error.to_string()),
    }
}

/// A profile pin wins over the job the caller named: the user chose the model,
/// the caller only said what kind of work this is.
fn subagent_model_binding<'a>(
    profile: Option<&'a SystemPromptProfile>,
    asked: Option<&'a Binding>,
) -> Option<&'a Binding> {
    profile
        .and_then(SystemPromptProfile::subagent_model)
        .or(asked)
}

async fn resolve_provider(
    ctx: &ToolContext,
    binding_override: Option<&Binding>,
) -> Result<(Model, Arc<dyn provider::Provider>), String> {
    let (provider, model) = resolve_model_for_purpose(
        ModelRoute {
            provider: &ctx.provider,
            model: &ctx.model,
        },
        ModelRoute {
            provider: &ctx.chat_provider,
            model: &ctx.chat_model,
        },
        ModelPurpose::Subagent,
        binding_override,
        ctx.timeouts,
        &ctx.model_policy,
    )
    .await
    .map_err(|error| error.to_string())?;
    Ok((model, provider))
}

/// A standalone task shows its model via `SubagentInfo` on the header; a
/// dispatching caller (batch) gets the same thing as a live annotation.
fn announce_model(ctx: &ToolContext, model: &Model) {
    if let Some(sink) = &ctx.live_sink {
        let _ = sink.send(ToolLive::Annotation(model.spec()));
    }
}

fn build(
    ctx: &ToolContext,
    ids: Identity,
    resolved: Resolved,
    local_tools: LocalTools,
    fast: bool,
    name: String,
) -> Result<Subagent, String> {
    // A deferred definition starts outside the request, so it is absent from
    // `tools`. Leaving it out of the filter too would let the child load a tool
    // with `tool_search` that dispatch then refuses. It goes in ahead of the
    // config intersect, which is what keeps a disabled tool disabled.
    let tool_filter = ToolFilter::Only(
        resolved
            .tools
            .as_array()
            .expect("tools are an array")
            .iter()
            .filter_map(|definition| definition.get("name")?.as_str().map(str::to_owned))
            .collect(),
    )
    .including(resolved.deferred.iter().map(|tool| tool.name.to_string()))
    .intersect(&ToolFilter::from_config(&ctx.config, &resolved.model, &[]))
    .including(local_tools.keys().cloned())
    .for_mode(&resolved.mode);

    let (sub_tx, sub_rx) = flume::unbounded::<Envelope>();
    let sub_event_tx = ctx.event_tx.rebind(sub_tx);
    let parent_tx = ctx.event_tx.clone();
    let (answer_tx, answer_rx) = flume::unbounded::<String>();
    let (steer_tx, steer_rx) = steering_queue();
    let subagent_info: Arc<OnceLock<SubagentInfo>> = Arc::new(OnceLock::new());
    let (usage_tx, usage_rx) = flume::unbounded();

    smol::spawn(relay_session_events(
        sub_rx,
        parent_tx.clone(),
        Arc::clone(&subagent_info),
        usage_tx,
        ctx.live_sink.clone(),
    ))
    .detach();

    let history_items = resolved
        .history_lease
        .history()
        .map_or_else(Vec::new, |messages| expand_history(messages));
    let history = History::restored(history_items).map_err(|error| error.to_string())?;

    // Register a cancel trigger so the child token does not fire on drop and
    // kill the subagent at birth.
    let (child_trigger, child_cancel) = ctx.cancel.child();
    // Several sessions can share one task id, so keep the slot and retire only
    // ours on close instead of clearing the whole key.
    let cancel_slot = ctx
        .subagent_cancels
        .insert(resolved.task_id.clone(), child_trigger);

    info!(name = %name, model = %resolved.model.id, "subagent session opened");
    let context_publisher = ctx
        .context_publisher
        .as_ref()
        .map(|publisher| publisher.for_task(resolved.task_id.clone()));

    Ok(Subagent {
        steering: Arc::new(Mutex::new(Steering::new(
            ctx.config.steering.resolve(&resolved.model.spec()),
        ))),
        report_ready: None,
        params: AgentParams {
            provider: resolved.provider,
            model: resolved.model,
            chat_provider: Arc::clone(&ctx.chat_provider),
            chat_model: Model::clone(&ctx.chat_model),
            config: ctx.config.clone(),
            tool_output_lines: caudra_config::ToolOutputLines::default(),
            permissions: Arc::clone(&ctx.permissions),
            session_id: ctx.session_id.clone(),
            cache_key: Some(CacheKey::task(ctx.session_id.as_ref(), &resolved.task_id)),
            workspace_session: ctx.workspace_session.clone(),
            remote_project_context: ctx.remote_project_context.clone(),
            local_documents: ctx.local_documents.clone(),
            task_environment: ctx.task_environment.clone(),
            root_tool_use_id: Some(ids.root_tool_use_id.clone()),
            mailbox: None,
            context_publisher,
            timeouts: ctx.timeouts,
            file_tracker: FileReadTracker::fresh(),
            // Shared, not fresh: a subagent tracks its own reads but must not
            // write a file a sibling agent is writing.
            path_locks: Arc::clone(&ctx.path_locks),
            baseline: ctx.baseline.clone(),
            prompt_slots: Arc::clone(&ctx.prompt_slots),
            prompt_profiles: Arc::clone(&ctx.prompt_profiles),
            default_task_prompt_profile_name: resolved.default_task_prompt_profile_name,
            active_prompt_profile_name: resolved.active_prompt_profile_name,
            subagent_cancels: Arc::new(CancelMap::new()),
            subagent_history: ctx.subagent_history.clone(),
            registry: Arc::clone(&ctx.registry),
            audience: resolved.audience,
            tool_filter,
            model_policy: Arc::clone(&ctx.model_policy),
            workflow: None,
        },
        system: resolved.system,
        tools: resolved.tools,
        deferred: resolved.deferred,
        mode: resolved.mode,
        environment: resolved.environment,
        mode_notice: resolved.mode_notice,
        thinking: resolved.thinking,
        fast,
        mcp: ctx
            .mcp
            .as_ref()
            .filter(|_| resolved.mcp_enabled)
            .map(McpSession::fresh),
        history,
        history_lease: Some(resolved.history_lease),
        sub_event_tx,
        child_cancel,
        interrupt_source: Arc::new(steer_rx),
        answer_rx: Arc::new(AsyncMutex::new(answer_rx)),
        answer_tx: Some(answer_tx),
        steer_tx: Some(steer_tx),
        parent_cancels: Arc::clone(&ctx.subagent_cancels),
        parent_tool_use_id: ids.parent_tool_use_id,
        root_tool_use_id: ids.root_tool_use_id,
        task_id: resolved.task_id,
        cancel_slot,
        parent_event_tx: parent_tx,
        subagent_info,
        local_tools,
        name,
        usage: TokenUsage::default(),
        usage_rx,
        start: Instant::now(),
        closed: false,
    })
}

fn expand_history(messages: &[Message]) -> Vec<HistoryItem> {
    let mut items: Vec<HistoryItem> = Vec::new();
    for message in messages {
        items.extend(expand_message(message, items.last().map(|item| item.id)));
    }
    items
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::context::{
        ContextInventory, ContextKey, ContextReadiness, ContextSnapshot, ContextStore,
        ContextUsage, ContextWindow,
    };
    use crate::tools::BATCH_TOOL_NAME;
    use crate::tools::registry::Tool;
    use crate::tools::test_support::NamedMock;
    use crate::{BatchToolStatus, ToolDoneEvent, TurnCompleteEvent};
    use caudra_providers::{Billing, ContentBlock, Message, Role};
    use caudra_storage::id::SessionRef;
    use caudra_storage::usage_ledger::LedgerPurpose;
    use tempfile::TempDir;
    use test_case::test_case;

    const RUN_ID: u64 = 7;
    const CHILD_WORKFLOW_RUN: &str = "wf-child";
    const PARENT_WORKFLOW_RUN: &str = "wf-parent";
    const PARENT_ID: &str = "task-1";
    const TOOL_ID: &str = "toolu_01";
    const NEXT_TOOL_ID: &str = "toolu_02";
    const BATCH_HEADER: &str = "2 tools";
    const PLAN_MODEL_SPEC: &str = "openai/gpt-5.4";
    const PINNED_MODEL_SPEC: &str = "anthropic/claude-haiku-4-5";
    const PINNING_PROFILE: &str = "pinned";
    const PROFILE_BODY: &str = "Pinned.";
    const PROFILE_MISSING: &str = "a written profile must load";
    const COLLIDING_SUBAGENT_NAME: &str = "collision";
    const INHERITED_PROFILE: &str = "parent-default";
    const SUBAGENT_SYSTEM: &str = "system";
    const REMOTE_CWD: &str = "remote/project";
    const REMOTE_PLATFORM: &str = "remote-os";
    const PLAN_PATH: &str = "plan.md";
    const ENVIRONMENT_MISSING: &str = "a task must be told its environment";
    /// A real entry of `DEFERRED_BUILTIN_TOOLS`, so the split under test happens.
    const DEFERRED_TOOL: &str = "python_execution";
    const MAIN_ONLY_TOOL: &str = "main_only_mock";
    const PUBLISHER_MISSING: &str = "subagent must inherit a task-scoped context publisher";
    const IGNORED_ERROR: &str = "handled by the session caller";
    const DONE_USAGE: TokenUsage = tokens(150, 30);
    /// Spelt out rather than read back off the activity, so a rename in
    /// caudra-agent fails here instead of passing silently.
    const THINKING_LABEL: &str = "thinking";
    const RESPONDING_LABEL: &str = "responding";
    const THOUGHT_TITLE: &str = "Weighing the options";

    const fn tokens(input: u32, output: u32) -> TokenUsage {
        TokenUsage {
            input,
            output,
            cache_creation: 0,
            cache_read: 0,
        }
    }

    fn context_snapshot(model: &Model) -> ContextSnapshot {
        ContextSnapshot {
            readiness: ContextReadiness::PreparedNextRequest,
            model: model.into(),
            window: ContextWindow::new(model, false, None),
            usage: ContextUsage::default(),
            measured: None,
            inventory: ContextInventory::default(),
        }
    }

    fn task_options(mode: Option<SubagentTaskMode>) -> TaskOptions {
        TaskOptions {
            name: COLLIDING_SUBAGENT_NAME.into(),
            task_id: TaskIdentity::Derive,
            profile: Some(crate::prompt::profile::BUILTIN_PROFILE_NAME.into()),
            mode,
            model_job: None,
            local_definitions: Vec::new(),
            local_tools: LocalTools::default(),
        }
    }

    fn generic_options() -> GenericOptions {
        GenericOptions {
            name: COLLIDING_SUBAGENT_NAME.into(),
            task_id: None,
            model_spec: None,
            system: SUBAGENT_SYSTEM.into(),
            tools: json!([]),
            audience: None,
            thinking: None,
            fast: None,
            mcp: Some(false),
            local_tools: LocalTools::default(),
        }
    }

    #[test_case(false ; "unbound_global")]
    #[test_case(true ; "exact_same_model")]
    fn generic_subagent_reuses_the_parent_when_resolution_keeps_its_model(exact: bool) {
        smol::block_on(async {
            let ctx = crate::tools::test_support::stub_ctx(&AgentMode::Build);
            let mut options = generic_options();
            options.model_spec = exact.then(|| ctx.model.spec());

            let mut subagent = open_generic(&ctx, options).await.unwrap();

            assert_eq!(subagent.params.model.spec(), ctx.model.spec());
            assert!(Arc::ptr_eq(&subagent.params.provider, &ctx.provider));
            subagent.close();
        });
    }

    #[test]
    fn nested_subagent_keeps_the_selected_chat_anchor() {
        smol::block_on(async {
            let mut ctx = crate::tools::test_support::stub_ctx(&AgentMode::Build);
            let chat_model = Arc::clone(&ctx.chat_model);
            ctx.model = Arc::new(Model::from_spec(PLAN_MODEL_SPEC).unwrap());

            let mut subagent = open_generic(&ctx, generic_options()).await.unwrap();

            assert_eq!(subagent.params.model.spec(), ctx.model.spec());
            assert_eq!(subagent.params.chat_model.spec(), chat_model.spec());
            assert!(Arc::ptr_eq(
                &subagent.params.chat_provider,
                &ctx.chat_provider
            ));
            subagent.close();
        });
    }

    #[test_case(SubagentTaskMode::Plan ; "plan")]
    #[test_case(SubagentTaskMode::Build ; "build")]
    fn task_mode_does_not_select_the_subagent_model(mode: SubagentTaskMode) {
        smol::block_on(async {
            let ctx = crate::tools::test_support::stub_ctx(&AgentMode::Build);
            let mut subagent = open_task(
                &ctx,
                TaskOptions {
                    name: COLLIDING_SUBAGENT_NAME.into(),
                    task_id: TaskIdentity::Derive,
                    profile: Some(crate::prompt::profile::BUILTIN_PROFILE_NAME.into()),
                    mode: Some(mode),
                    model_job: None,
                    local_definitions: Vec::new(),
                    local_tools: LocalTools::default(),
                },
            )
            .await
            .unwrap();

            assert_eq!(subagent.params.model.spec(), ctx.model.spec());
            assert!(Arc::ptr_eq(&subagent.params.provider, &ctx.provider));
            subagent.close();
        });
    }

    /// Built through the real loader, so the pin under test is the one a user
    /// writing that frontmatter would get.
    fn profile_pinning(model: Option<&str>) -> Arc<SystemPromptProfile> {
        let dir = TempDir::new().unwrap();
        let profiles = dir.path().join(crate::prompt::profile::PROFILE_DIR);
        std::fs::create_dir(&profiles).unwrap();
        let frontmatter = model
            .map(|model| format!("---\nsubagent_model: {model}\n---\n"))
            .unwrap_or_default();
        std::fs::write(
            profiles.join(format!("{PINNING_PROFILE}.md")),
            format!("{frontmatter}{PROFILE_BODY}"),
        )
        .unwrap();
        crate::prompt::profile::PromptProfileCatalog::discover_with(Some(dir.path()))
            .get(PINNING_PROFILE)
            .expect(PROFILE_MISSING)
    }

    /// The user configured the pin and the caller only said what kind of work
    /// this is, so the pin outranks the job asked for.
    #[test_case(None ; "no_job_asked")]
    #[test_case(Some(ModelPurpose::Fast) ; "fast_job_asked")]
    #[test_case(Some(ModelPurpose::Best) ; "best_job_asked")]
    fn a_profile_pin_outranks_the_asked_for_job(model_job: Option<ModelPurpose>) {
        let profile = profile_pinning(Some(PINNED_MODEL_SPEC));
        let asked = model_job.map(Binding::Same);

        assert_eq!(
            subagent_model_binding(Some(&profile), asked.as_ref()),
            Some(&Binding::Exact(PINNED_MODEL_SPEC.into()))
        );
    }

    #[test_case(true ; "unpinned_profile")]
    #[test_case(false ; "no_profile")]
    fn the_asked_for_job_is_used_without_a_pin(unpinned_profile: bool) {
        let profile = unpinned_profile.then(|| profile_pinning(None));
        let asked = Some(Binding::Same(ModelPurpose::Fast));

        assert_eq!(
            subagent_model_binding(profile.as_deref(), asked.as_ref()),
            Some(&Binding::Same(ModelPurpose::Fast))
        );
    }

    /// Announced rather than assembled, so the prompt a subagent caches never
    /// carries a working directory, a date, or a model.
    #[test]
    fn task_subagent_is_told_the_remote_environment() {
        smol::block_on(async {
            let mut ctx = crate::tools::test_support::stub_ctx(&AgentMode::Build);
            ctx.task_environment = crate::template::Vars::new()
                .set("{cwd}", REMOTE_CWD)
                .set("{platform}", REMOTE_PLATFORM);
            let mut subagent = open_task(&ctx, task_options(Some(SubagentTaskMode::Build)))
                .await
                .unwrap();

            let environment = subagent.environment.as_deref().expect(ENVIRONMENT_MISSING);
            assert!(environment.contains(REMOTE_CWD));
            assert!(environment.contains(REMOTE_PLATFORM));
            assert!(environment.contains(&subagent.params.model.spec()));
            assert!(!subagent.system.contains(REMOTE_CWD));
            subagent.close();
        });
    }

    /// The mode is the one thing a subagent must not be able to talk itself out
    /// of, so it arrives the same way the main agent's does.
    #[test_case(SubagentTaskMode::Plan, crate::prompt::TASK_PLAN_CONTRACT ; "plan")]
    #[test_case(SubagentTaskMode::Build, crate::prompt::TASK_BUILD_CONTRACT ; "build")]
    fn a_task_is_told_the_mode_it_was_granted(mode: SubagentTaskMode, expected: &str) {
        smol::block_on(async {
            let ctx = crate::tools::test_support::stub_ctx(&AgentMode::Build);
            let mut subagent = open_task(&ctx, task_options(Some(mode))).await.unwrap();

            assert_eq!(subagent.mode_notice.as_deref(), Some(expected));
            assert!(!subagent.system.contains(crate::prompt::TASK_MODE_MARKER));
            subagent.close();
        });
    }

    /// A generic session's prompt is written by its caller and never agreed to
    /// the reminder contract, so it is told nothing it cannot read.
    #[test]
    fn a_generic_session_is_announced_nothing() {
        smol::block_on(async {
            let ctx = crate::tools::test_support::stub_ctx(&AgentMode::Build);
            let mut subagent = open_generic(&ctx, generic_options()).await.unwrap();

            assert!(subagent.environment.is_none());
            assert!(subagent.mode_notice.is_none());
            subagent.close();
        });
    }

    /// `tool_search` can load a deferred builtin into the child's next request,
    /// so the filter dispatch judges it by has to know the name too. What the
    /// child's audience never offered stays out.
    #[test_case(DEFERRED_TOOL, true ; "a_loadable_builtin_stays_callable")]
    #[test_case(MAIN_ONLY_TOOL, false ; "a_tool_outside_the_audience_does_not")]
    fn a_task_may_call_what_it_was_allowed_to_load(tool: &str, callable: bool) {
        smol::block_on(async {
            let mut ctx = crate::tools::test_support::stub_ctx(&AgentMode::Build);
            // Pinned rather than left to the model's class, so the split under
            // test happens wherever this runs.
            ctx.config.defer_builtin_tools = caudra_config::DeferBuiltinTools::Always;
            ctx.registry
                .register_many([
                    (
                        Arc::new(NamedMock::new(DEFERRED_TOOL, ToolAudience::all()))
                            as Arc<dyn Tool>,
                        NamedMock::source(),
                    ),
                    (
                        Arc::new(NamedMock::new(MAIN_ONLY_TOOL, ToolAudience::MAIN))
                            as Arc<dyn Tool>,
                        NamedMock::source(),
                    ),
                ])
                .unwrap();

            let mut subagent = open_task(&ctx, task_options(Some(SubagentTaskMode::Build)))
                .await
                .unwrap();

            assert_eq!(subagent.params.tool_filter.matches(tool), callable);
            subagent.close();
        });
    }

    /// A caller hands down the authority it holds, so delegation from build
    /// mode delegates the ability to build instead of silently downgrading.
    #[test_case(AgentMode::Build, SubagentTaskMode::Build ; "build_delegates_build")]
    #[test_case(AgentMode::ReadOnly, SubagentTaskMode::Plan ; "read_only_delegates_plan")]
    #[test_case(AgentMode::Plan(PLAN_PATH.into()), SubagentTaskMode::Plan ; "plan_delegates_plan")]
    fn an_omitted_task_mode_is_inherited(caller: AgentMode, expected: SubagentTaskMode) {
        smol::block_on(async {
            let ctx = crate::tools::test_support::stub_ctx(&caller);
            let mut subagent = open_task(&ctx, task_options(None)).await.unwrap();

            assert_eq!(subagent.task_mode(), expected);
            subagent.close();
        });
    }

    #[test]
    fn context_publisher_uses_actual_reserved_task_id_after_collision() {
        smol::block_on(async {
            let store = ContextStore::new();
            let mut ctx =
                crate::tools::test_support::stub_ctx_with(&AgentMode::Build, None, Some(PARENT_ID));
            ctx.context_publisher = Some(store.publisher(ContextKey::Main));
            let _occupied = ctx.subagent_history.reserve(PARENT_ID).unwrap();
            let mut subagent = open_generic(&ctx, generic_options()).await.unwrap();
            let actual_task_id = subagent.id().to_owned();
            assert_ne!(actual_task_id, PARENT_ID);

            subagent
                .params
                .context_publisher
                .as_ref()
                .expect(PUBLISHER_MISSING)
                .publish(context_snapshot(&subagent.params.model));

            assert!(store.latest(&ContextKey::task(actual_task_id)).is_some());
            assert!(store.latest(&ContextKey::task(PARENT_ID)).is_none());
            assert!(store.latest(&ContextKey::Main).is_none());
            subagent.close();
        });
    }

    /// Siblings must not fight over the parent's cache slot, so each is keyed
    /// by the task it actually reserved.
    #[test]
    fn each_subagent_is_cache_keyed_by_its_own_task_within_the_session() {
        smol::block_on(async {
            let mut ctx =
                crate::tools::test_support::stub_ctx_with(&AgentMode::Build, None, Some(PARENT_ID));
            let session = SessionRef::generate();
            ctx.session_id = Some(session.clone());
            let mut first = open_generic(&ctx, generic_options()).await.unwrap();
            let mut second = open_generic(&ctx, generic_options()).await.unwrap();

            let keys = [&first, &second].map(|subagent| {
                assert_eq!(
                    subagent.params.cache_key,
                    Some(CacheKey::task(Some(&session), subagent.id()))
                );
                subagent.params.cache_key.clone().unwrap()
            });

            assert_ne!(keys[0], keys[1]);
            assert!(keys.iter().all(|key| *key != CacheKey::session(&session)));
            first.close();
            second.close();
        });
    }

    #[test]
    fn generic_subagent_inherits_task_default_without_an_active_profile() {
        smol::block_on(async {
            let mut ctx = crate::tools::test_support::stub_ctx(&AgentMode::Build);
            ctx.default_task_prompt_profile_name = Arc::from(INHERITED_PROFILE);

            let mut subagent = open_generic(&ctx, generic_options()).await.unwrap();

            assert_eq!(
                subagent.params.default_task_prompt_profile_name.as_ref(),
                INHERITED_PROFILE
            );
            assert!(subagent.params.active_prompt_profile_name.is_none());
            subagent.close();
        });
    }

    #[test]
    fn task_subagent_marks_its_resolved_profile_active() {
        smol::block_on(async {
            let ctx = crate::tools::test_support::stub_ctx(&AgentMode::Build);
            let mut subagent = open_task(
                &ctx,
                TaskOptions {
                    name: COLLIDING_SUBAGENT_NAME.into(),
                    task_id: TaskIdentity::Derive,
                    profile: Some(crate::prompt::profile::BUILTIN_PROFILE_NAME.into()),
                    mode: Some(SubagentTaskMode::Plan),
                    model_job: None,
                    local_definitions: Vec::new(),
                    local_tools: LocalTools::default(),
                },
            )
            .await
            .unwrap();

            assert_eq!(
                subagent.params.default_task_prompt_profile_name.as_ref(),
                crate::prompt::profile::BUILTIN_PROFILE_NAME
            );
            assert_eq!(
                subagent.params.active_prompt_profile_name.as_deref(),
                Some(crate::prompt::profile::BUILTIN_PROFILE_NAME)
            );
            subagent.close();
        });
    }

    fn envelope(event: AgentEvent) -> Envelope {
        Envelope {
            event,
            subagent: None,
            run_id: RUN_ID,
            workflow: None,
        }
    }

    fn parent_info() -> Arc<OnceLock<SubagentInfo>> {
        session_info(PARENT_ID)
    }

    fn session_info(id: &str) -> Arc<OnceLock<SubagentInfo>> {
        let info = Arc::new(OnceLock::new());
        info.set(SubagentInfo {
            parent_tool_use_id: id.into(),
            task_id: id.into(),
            name: "research".into(),
            prompt: None,
            model: None,
            thinking: None,
            fast: false,
            answer_tx: None,
            steer_tx: None,
        })
        .unwrap();
        info
    }

    fn turn(usage: TokenUsage, cost: f64) -> AgentEvent {
        AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
            message: Message::default(),
            usage,
            model: "test-model".into(),
            provider: "test-provider".into(),
            purpose: LedgerPurpose::Chat,
            cost: Some(cost),
            billing: Billing::Api,
            context_size: None,
            context_window: 0,
        }))
    }

    #[test]
    fn relay_session_events_reports_live_usage_and_done_total() {
        let (sub_tx, sub_rx) = flume::unbounded();
        let (parent_raw_tx, parent_rx) = flume::unbounded();
        let subagent_info = parent_info();
        let (usage_tx, usage_rx) = flume::unbounded();
        let (live_tx, live_rx) = flume::unbounded();

        for event in [
            turn(tokens(100, 20), 0.25),
            turn(tokens(50, 10), 0.5),
            AgentEvent::Error {
                message: IGNORED_ERROR.into(),
            },
            AgentEvent::SubagentHistory {
                task_id: "nested-task".into(),
                parent_tool_use_id: "nested-call".into(),
                root_tool_use_id: PARENT_ID.into(),
                name: "nested".into(),
                model: "provider/model".into(),
                messages: Vec::new(),
                spec: None,
            },
            AgentEvent::Done {
                usage: DONE_USAGE,
                num_turns: 2,
                reason: DoneReason::EndTurn,
            },
        ] {
            sub_tx.send(envelope(event)).unwrap();
        }
        drop(sub_tx);

        smol::block_on(relay_session_events(
            sub_rx,
            EventSender::new(parent_raw_tx, RUN_ID),
            subagent_info,
            usage_tx,
            Some(live_tx),
        ));

        let live = live_rx
            .drain()
            .map(|event| match event {
                ToolLive::Usage(usage) => usage,
                _ => panic!("relay must only publish usage"),
            })
            .collect::<Vec<_>>();
        let expected = [
            tokens(100, 20).format_sum_cost(Some(0.25)),
            tokens(50, 10).format_sum_cost(Some(0.75)),
        ];
        assert_eq!(live, expected);
        assert_eq!(usage_rx.try_recv(), Ok(DONE_USAGE));

        let forwarded = parent_rx.drain().collect::<Vec<_>>();
        assert_eq!(forwarded.len(), expected.len() + 1);
        assert!(forwarded.iter().all(|envelope| {
            matches!(
                envelope.event,
                AgentEvent::TurnComplete(_) | AgentEvent::SubagentHistory { .. }
            ) && envelope
                .subagent
                .as_ref()
                .is_some_and(|info| info.parent_tool_use_id == PARENT_ID)
        }));
    }

    fn text_delta() -> AgentEvent {
        AgentEvent::TextDelta {
            text: "summarising".into(),
        }
    }

    /// Deltas arrive one token at a time, and a nested task's events pass
    /// through here already stamped while this session sits blocked on it.
    /// Neither may reach the parent header.
    #[test]
    fn relay_publishes_activity_once_per_change_and_ignores_nested_events() {
        let (sub_tx, sub_rx) = flume::unbounded();
        let (parent_raw_tx, parent_rx) = flume::unbounded();
        let (usage_tx, _usage_rx) = flume::unbounded();
        let (live_tx, live_rx) = flume::unbounded();

        for event in [text_delta(), text_delta()] {
            sub_tx.send(envelope(event)).unwrap();
        }
        sub_tx
            .send(envelope(AgentEvent::ThinkingDelta { text: "hm".into() }))
            .unwrap();
        let mut nested = envelope(text_delta());
        nested.subagent = Some(parent_info().get().unwrap().clone());
        sub_tx.send(nested).unwrap();
        drop(sub_tx);

        smol::block_on(relay_session_events(
            sub_rx,
            EventSender::new(parent_raw_tx, RUN_ID),
            parent_info(),
            usage_tx,
            Some(live_tx),
        ));

        let live = live_rx
            .drain()
            .map(|event| match event {
                ToolLive::Progress(progress) => progress.activity.label().to_owned(),
                _ => panic!("relay must only publish progress here"),
            })
            .collect::<Vec<_>>();
        assert_eq!(live, [RESPONDING_LABEL, THINKING_LABEL]);

        assert_eq!(
            relayed_progress(&parent_rx),
            [
                (RESPONDING_LABEL.to_owned(), 0, Some(PARENT_ID.to_owned())),
                (THINKING_LABEL.to_owned(), 0, Some(PARENT_ID.to_owned())),
            ]
        );
    }

    fn provenance(run_id: &str) -> crate::types::WorkflowProvenance {
        crate::types::WorkflowProvenance {
            run_id: run_id.into(),
            epoch: 1,
            call_key: 1,
            phase: None,
        }
    }

    /// A workflow agent's events must reach the parent attributed to the
    /// workflow run: a child envelope that names one keeps it, and one that
    /// does not takes the parent sender's, on relayed and synthesized
    /// progress envelopes alike.
    #[test_case(Some(CHILD_WORKFLOW_RUN), CHILD_WORKFLOW_RUN; "the_childs_own_run_wins")]
    #[test_case(None, PARENT_WORKFLOW_RUN; "the_parent_sender_fills_the_gap")]
    fn relay_keeps_workflow_provenance(child: Option<&str>, expected: &str) {
        let (sub_tx, sub_rx) = flume::unbounded();
        let (parent_raw_tx, parent_rx) = flume::unbounded();
        let (usage_tx, _usage_rx) = flume::unbounded();

        let mut child_envelope = envelope(text_delta());
        child_envelope.workflow = child.map(provenance);
        sub_tx.send(child_envelope).unwrap();
        drop(sub_tx);

        smol::block_on(relay_session_events(
            sub_rx,
            EventSender::new(parent_raw_tx, RUN_ID).with_workflow(provenance(PARENT_WORKFLOW_RUN)),
            parent_info(),
            usage_tx,
            None,
        ));

        let forwarded: Vec<Envelope> = parent_rx.drain().collect();
        assert_eq!(forwarded.len(), 2, "one relayed delta and its progress");
        assert!(forwarded.iter().all(|envelope| {
            envelope
                .workflow
                .as_ref()
                .is_some_and(|workflow| workflow.run_id == expected)
        }));
    }

    fn relayed_progress(rx: &flume::Receiver<Envelope>) -> Vec<(String, u32, Option<String>)> {
        rx.drain()
            .filter_map(|envelope| match envelope.event {
                AgentEvent::SubagentProgress { progress } => Some((
                    progress.activity.label().to_owned(),
                    progress.tools,
                    envelope.subagent.map(|info| info.parent_tool_use_id),
                )),
                _ => None,
            })
            .collect()
    }

    fn tool_start(tool: &str) -> AgentEvent {
        AgentEvent::ToolStart(Box::new(crate::ToolStartEvent {
            id: TOOL_ID.into(),
            effect: crate::tools::ToolEffect::Unknown,
            tool: Arc::from(tool),
            summary: String::new(),
            render_header: None,
            annotation: None,
            input: None,
            raw_input: None,
            output: None,
        }))
    }

    const CHILD_TOOL: &str = "shell";
    const ROSTER_MISCOUNT_MSG: &str =
        "a batch is worth its roster, not one call: its children never reach this stream";

    fn roster_entry(index: usize, status: BatchToolStatus) -> crate::BatchToolEntry {
        crate::BatchToolEntry {
            model_suffix: None,
            tool: CHILD_TOOL.to_owned(),
            effect: crate::tools::ToolEffect::Unknown,
            summary: format!("c{index}"),
            status,
            input: None,
            raw_input: None,
            output: None,
            annotation: None,
        }
    }

    /// A batch's start event, carrying the roster it is about to run.
    fn batch_start(children: usize) -> AgentEvent {
        let entries = (0..children)
            .map(|index| roster_entry(index, BatchToolStatus::Pending))
            .collect();
        let AgentEvent::ToolStart(mut start) = tool_start(BATCH_TOOL_NAME) else {
            unreachable!("tool_start builds a start event")
        };
        start.output = Some(ToolOutput::Batch {
            entries,
            text: String::new(),
        });
        AgentEvent::ToolStart(start)
    }

    fn batch_progress(id: &str, index: usize, status: BatchToolStatus) -> AgentEvent {
        AgentEvent::BatchProgress(Box::new(crate::BatchProgressEvent {
            id: id.to_owned(),
            index,
            entry: roster_entry(index, status),
        }))
    }

    #[test_case(tool_pending() ; "pending")]
    #[test_case(tool_input_delta() ; "input_preview")]
    #[test_case(tool_start(CHILD_TOOL) ; "started")]
    fn tool_activity_uses_the_event_call_id(event: AgentEvent) {
        let mut relay = ProgressRelay::new();
        for activity in [SubagentActivity::from_event(&event), relay.activity(&event)] {
            assert!(matches!(
                activity,
                Some(SubagentActivity::Tool { call_id: Some(id), .. }) if id == TOOL_ID
            ));
        }
    }

    #[test_case(batch_progress(TOOL_ID, 0, BatchToolStatus::Running) ; "child_status")]
    #[test_case(header_snapshot(BATCH_HEADER) ; "retitled")]
    fn watched_batch_updates_keep_the_call_id(event: AgentEvent) {
        let mut relay = ProgressRelay::new();
        relay.last = relay.activity(&batch_start(2));

        assert!(matches!(
            relay.activity(&event),
            Some(SubagentActivity::Tool { call_id: Some(id), .. }) if id == TOOL_ID
        ));
    }

    #[test_case(batch_progress(TOOL_ID, 0, BatchToolStatus::Running) ; "child_status")]
    #[test_case(header_snapshot(BATCH_HEADER) ; "retitled")]
    fn old_batch_events_do_not_change_a_new_call(event: AgentEvent) {
        let mut relay = ProgressRelay::new();
        relay.last = relay.activity(&batch_start(2));
        relay.last = relay.activity(&AgentEvent::ToolPending {
            id: NEXT_TOOL_ID.into(),
            name: BATCH_TOOL_NAME.into(),
        });

        assert!(relay.activity(&event).is_none());
    }

    #[test_case(BatchToolStatus::Success ; "success")]
    #[test_case(BatchToolStatus::Error ; "error")]
    fn identical_batch_calls_publish_distinct_identities(status: BatchToolStatus) {
        let mut next = batch_start(1);
        if let AgentEvent::ToolStart(start) = &mut next {
            start.id = NEXT_TOOL_ID.into();
        }
        let published: Vec<_> = relayed(vec![
            batch_start(1),
            batch_progress(TOOL_ID, 0, status),
            next,
            batch_progress(NEXT_TOOL_ID, 0, BatchToolStatus::Running),
        ])
        .into_iter()
        .filter_map(|envelope| match envelope.event {
            AgentEvent::SubagentProgress {
                progress:
                    SubagentProgress {
                        activity:
                            SubagentActivity::Tool {
                                call_id, children, ..
                            },
                        ..
                    },
            } => Some((call_id, children[0].status)),
            _ => None,
        })
        .collect();

        assert_eq!(
            published,
            [
                (Some(TOOL_ID.into()), BatchToolStatus::Pending),
                (Some(TOOL_ID.into()), status),
                (Some(NEXT_TOOL_ID.into()), BatchToolStatus::Pending),
                (Some(NEXT_TOOL_ID.into()), BatchToolStatus::Running),
            ]
        );
    }

    /// Every roster the fragments published, as tool names and statuses.
    fn rosters(events: Vec<AgentEvent>) -> Vec<Vec<(String, BatchToolStatus)>> {
        let mut relay = ProgressRelay::new();
        events
            .into_iter()
            .filter_map(|event| {
                let activity = relay.activity(&event)?;
                relay.last = Some(activity.clone());
                Some(
                    activity
                        .children()
                        .iter()
                        .map(|child| (child.tool.to_string(), child.status))
                        .collect(),
                )
            })
            .collect()
    }

    /// The reported bug: a batch of twenty reported as one tool call, because
    /// its children are dispatched with `Emit::Capture` and never arrive here
    /// as starts of their own.
    #[test]
    fn a_batch_counts_every_call_it_dispatches() {
        let mut relay = ProgressRelay::new();
        relay.tools += ProgressRelay::counted(&batch_start(3));
        relay.tools += ProgressRelay::counted(&tool_start(CHILD_TOOL));
        relay.tools +=
            ProgressRelay::counted(&batch_progress(TOOL_ID, 0, BatchToolStatus::Running));

        assert_eq!(relay.tools, 4, "{ROSTER_MISCOUNT_MSG}");
    }

    /// A model can ask for more children than `batch` will run. Every one of
    /// them is a call it made, so every one is counted, while the rows stop at
    /// the cap: past it each says the same discard message.
    #[test]
    fn an_oversized_roster_counts_whole_and_draws_to_the_cap() {
        let over = MAX_BATCH_SIZE + 5;
        let start = batch_start(over);
        let mut relay = ProgressRelay::new();

        let activity = relay.activity(&start).expect("a start names its tool");

        assert_eq!(ProgressRelay::counted(&start), over as u32);
        assert_eq!(activity.children().len(), MAX_BATCH_SIZE);
    }

    #[test]
    fn a_child_moving_republishes_the_roster_that_holds_it() {
        let published = rosters(vec![
            batch_start(2),
            batch_progress(TOOL_ID, 1, BatchToolStatus::Running),
            batch_progress(TOOL_ID, 1, BatchToolStatus::Success),
        ]);

        let row = |status| (CHILD_TOOL.to_owned(), status);
        assert_eq!(
            published,
            [
                vec![row(BatchToolStatus::Pending), row(BatchToolStatus::Pending)],
                vec![row(BatchToolStatus::Pending), row(BatchToolStatus::Running)],
                vec![row(BatchToolStatus::Pending), row(BatchToolStatus::Success)],
            ]
        );
    }

    /// A report for a batch this session is not watching describes someone
    /// else's call, and patching a row from it would draw the wrong roster.
    #[test_case("other-batch", 0 ; "another_batch")]
    #[test_case(TOOL_ID, 9      ; "a_row_the_roster_does_not_have")]
    fn an_unrecognised_child_report_publishes_nothing(id: &str, index: usize) {
        let mut relay = ProgressRelay::new();
        relay.last = relay.activity(&batch_start(2));

        assert!(
            relay
                .activity(&batch_progress(id, index, BatchToolStatus::Running))
                .is_none()
        );
    }

    /// The roster describes the call the header names, so a plugin renaming
    /// that header mid-run must not drop it.
    #[test]
    fn a_retitled_batch_keeps_its_roster() {
        let mut relay = ProgressRelay::new();
        relay.last = relay.activity(&batch_start(2));

        let retitled = relay
            .activity(&header_snapshot("2 tools"))
            .expect("a snapshot retitles the call it belongs to");

        assert_eq!(retitled.detail(), Some("2 tools"));
        assert_eq!(retitled.children().len(), 2);
    }

    /// The batch is over, so a later report cannot patch a roster that no
    /// longer describes anything.
    #[test]
    fn a_finished_batch_stops_being_watched() {
        let mut relay = ProgressRelay::new();
        relay.last = relay.activity(&batch_start(1));
        let done = AgentEvent::ToolDone(Box::new(ToolDoneEvent::error(
            TOOL_ID.to_owned(),
            SESSION_CLOSED,
        )));
        assert!(relay.activity(&done).is_none());

        assert!(
            relay
                .activity(&batch_progress(TOOL_ID, 0, BatchToolStatus::Success))
                .is_none()
        );
    }

    /// One relay hop, stamping with `info`: every envelope it handed on.
    fn relay_hop(info: Arc<OnceLock<SubagentInfo>>, envelopes: Vec<Envelope>) -> Vec<Envelope> {
        let (sub_tx, sub_rx) = flume::unbounded();
        let (parent_raw_tx, parent_rx) = flume::unbounded();
        let (usage_tx, _usage_rx) = flume::unbounded();
        for envelope in envelopes {
            sub_tx.send(envelope).unwrap();
        }
        drop(sub_tx);

        smol::block_on(relay_session_events(
            sub_rx,
            EventSender::new(parent_raw_tx, RUN_ID),
            info,
            usage_tx,
            None,
        ));

        parent_rx.drain().collect()
    }

    /// Everything one session's events made the relay hand the parent.
    fn relayed(events: Vec<AgentEvent>) -> Vec<Envelope> {
        relay_hop(parent_info(), events.into_iter().map(envelope).collect())
    }

    /// Drives the relay's stateful half directly: the published sequence is
    /// what a parent header would show, in order.
    fn activities(events: Vec<AgentEvent>) -> Vec<(String, Option<String>)> {
        relayed(events)
            .into_iter()
            .filter_map(|envelope| match envelope.event {
                AgentEvent::SubagentProgress { progress } => Some((
                    progress.activity.label().to_owned(),
                    progress.activity.detail().map(str::to_owned),
                )),
                _ => None,
            })
            .collect()
    }

    fn thinking(text: &str) -> AgentEvent {
        AgentEvent::ThinkingDelta { text: text.into() }
    }

    fn thought(title: Option<&str>) -> (String, Option<String>) {
        (THINKING_LABEL.to_owned(), title.map(str::to_owned))
    }

    /// A thought names itself in a heading that no single delta carries, so
    /// the relay has to accumulate the block to read it.
    #[test]
    fn a_thought_is_named_once_its_heading_has_streamed_in() {
        let published = activities(vec![
            thinking("**Weighing"),
            thinking(" the options"),
            thinking("**"),
            thinking("\n\nBoth read the same file."),
        ]);

        assert_eq!(published, [thought(None), thought(Some(THOUGHT_TITLE))]);
    }

    /// The heading is split across deltas, so a parse that reran per delta
    /// would drop it the moment a chunk landed between the fence and the
    /// blank line. It must survive that.
    #[test]
    fn a_named_thought_keeps_its_heading_through_the_rest_of_the_block() {
        let published = activities(vec![
            thinking("**Weighing the options**"),
            thinking("\n"),
            thinking("\nBoth read the same file."),
        ]);

        assert_eq!(published, [thought(Some(THOUGHT_TITLE))]);
    }

    /// An unnamed thought would otherwise be buffered in full a second time,
    /// and a reasoning stream runs to tens of kilobytes.
    #[test]
    fn an_unnamed_thought_stops_being_accumulated() {
        let mut relay = ProgressRelay::new();
        let text = "prose ".repeat(THOUGHT_TITLE_SCAN_LIMIT);
        let delta = thinking(&text);

        for _ in 0..3 {
            assert_eq!(
                relay.activity(&delta),
                Some(SubagentActivity::Thinking { title: None })
            );
        }

        assert_eq!(
            relay.thought.len(),
            text.len(),
            "the delta that passes the limit is the last one buffered"
        );
    }

    /// Two thoughts in a row with only a turn boundary between them: the
    /// second is unnamed and must say so rather than wear the first's name.
    #[test]
    fn a_new_turn_starts_a_new_thought() {
        let published = activities(vec![
            thinking("**Weighing the options**\n\nBoth read the same file."),
            turn(TokenUsage::default(), 0.0),
            thinking("Still unsure."),
        ]);

        assert_eq!(published, [thought(Some(THOUGHT_TITLE)), thought(None)]);
    }

    fn header_snapshot(text: &str) -> AgentEvent {
        AgentEvent::ToolHeaderSnapshot {
            id: TOOL_ID.into(),
            snapshot: crate::BufferSnapshot::plain_text(text.into()),
            theme_gen: None,
        }
    }

    /// A plugin paints its real header after the call starts, and that is the
    /// text its transcript shows. The snapshot names no tool, so it can only
    /// retitle the call already on the header.
    #[test]
    fn a_mid_run_header_retitles_the_tool_it_belongs_to() {
        let published = activities(vec![
            tool_start("batch"),
            header_snapshot("3 tools"),
            AgentEvent::TextDelta {
                text: "done".into(),
            },
            header_snapshot("stray"),
        ]);

        assert_eq!(
            published,
            [
                ("batch".to_owned(), None),
                ("batch".to_owned(), Some("3 tools".to_owned())),
                (RESPONDING_LABEL.to_owned(), None),
            ]
        );
    }

    /// `ToolPending` announces the call the following `ToolStart` runs, so a
    /// relay counting both would report twice the work. Two identical calls
    /// in a row leave the activity untouched, and the count is then the only
    /// thing that says anything happened.
    #[test]
    fn relay_counts_started_tools_once_each() {
        let (sub_tx, sub_rx) = flume::unbounded();
        let (parent_raw_tx, parent_rx) = flume::unbounded();
        let (usage_tx, _usage_rx) = flume::unbounded();

        for event in [
            AgentEvent::ToolPending {
                id: "toolu_01".into(),
                name: "shell".into(),
            },
            tool_start("shell"),
            tool_start("shell"),
        ] {
            sub_tx.send(envelope(event)).unwrap();
        }
        drop(sub_tx);

        smol::block_on(relay_session_events(
            sub_rx,
            EventSender::new(parent_raw_tx, RUN_ID),
            parent_info(),
            usage_tx,
            None,
        ));

        let counts: Vec<u32> = relayed_progress(&parent_rx)
            .into_iter()
            .map(|(_, tools, _)| tools)
            .collect();
        assert_eq!(counts, [0, 1, 2]);
    }

    const PENDING_TOOL: &str = "shell";
    const INPUT_PREVIEW: &str = "ls -la";
    const TOOL_CHUNK: &str = "total 0\n";
    const TRANSCRIPT_DROPPED: &str =
        "a live tool event must reach the subagent's own transcript, not just the parent header";
    const TRANSCRIPT_LEAKED: &str = "a session-level verdict belongs to the caller, not the relay";

    fn tool_pending() -> AgentEvent {
        AgentEvent::ToolPending {
            id: TOOL_ID.into(),
            name: PENDING_TOOL.into(),
        }
    }

    fn tool_input_delta() -> AgentEvent {
        AgentEvent::ToolInputDelta {
            id: TOOL_ID.into(),
            name: PENDING_TOOL.into(),
            delta: r#"{"command":"#.into(),
            preview: Some(INPUT_PREVIEW.into()),
            size: None,
            body: None,
            roster: None,
            delegations: Vec::new(),
        }
    }

    fn tool_output() -> AgentEvent {
        AgentEvent::ToolOutput {
            id: TOOL_ID.into(),
            content: TOOL_CHUNK.into(),
        }
    }

    fn error_event() -> AgentEvent {
        AgentEvent::Error {
            message: IGNORED_ERROR.into(),
        }
    }

    fn done_event() -> AgentEvent {
        AgentEvent::Done {
            usage: DONE_USAGE,
            num_turns: 1,
            reason: DoneReason::EndTurn,
        }
    }

    fn without_digests(envelopes: Vec<Envelope>) -> Vec<Envelope> {
        envelopes
            .into_iter()
            .filter(|envelope| !matches!(envelope.event, AgentEvent::SubagentProgress { .. }))
            .collect()
    }

    /// What the relay forwarded that was not a digest it synthesized itself.
    fn forwarded(events: Vec<AgentEvent>) -> Vec<Envelope> {
        without_digests(relayed(events))
    }

    /// The reported bug: a subagent's own transcript only showed a call once
    /// it had finished, because the events that open one were dropped here
    /// after the parent header had already been built from them.
    #[test_case(tool_pending()     ; "pending")]
    #[test_case(tool_input_delta() ; "input_delta")]
    #[test_case(tool_output()      ; "output")]
    fn a_live_tool_event_reaches_the_subagents_transcript(event: AgentEvent) {
        let expected = serde_json::to_value(&event).unwrap();

        let envelopes = forwarded(vec![event]);

        let [envelope] = envelopes.as_slice() else {
            panic!("{TRANSCRIPT_DROPPED}");
        };
        assert_eq!(serde_json::to_value(&envelope.event).unwrap(), expected);
        assert_eq!(
            envelope
                .subagent
                .as_ref()
                .map(|info| info.parent_tool_use_id.as_str()),
            Some(PARENT_ID),
            "{TRANSCRIPT_DROPPED}"
        );
    }

    /// `Error` ends the session it arrives on, and `Done` is the barrier
    /// `prompt` drains off `usage_tx`. Neither describes the parent's run.
    #[test_case(error_event() ; "error")]
    #[test_case(done_event()  ; "done")]
    fn a_session_level_event_is_withheld_from_the_parent(event: AgentEvent) {
        assert!(forwarded(vec![event]).is_empty(), "{TRANSCRIPT_LEAKED}");
    }

    /// The header is built from the same events the transcript now also sees,
    /// so widening the relay must leave the digests it publishes untouched.
    #[test]
    fn forwarding_live_tool_events_leaves_the_digests_alone() {
        let published = activities(vec![
            tool_pending(),
            tool_input_delta(),
            tool_start(PENDING_TOOL),
            tool_output(),
            text_delta(),
        ]);

        assert_eq!(
            published,
            [
                (PENDING_TOOL.to_owned(), None),
                (PENDING_TOOL.to_owned(), Some(INPUT_PREVIEW.to_owned())),
                (PENDING_TOOL.to_owned(), None),
                (RESPONDING_LABEL.to_owned(), None),
            ]
        );
    }

    const GRANDCHILD_ID: &str = "task-2";
    const GRANDCHILD_LEAKED: &str =
        "a grandchild's live tool work must not be restamped into this session's transcript";
    const GRANDCHILD_REROUTED: &str =
        "a grandchild's finished call keeps the routing it had before the live kinds were added";

    fn tool_done() -> AgentEvent {
        AgentEvent::ToolDone(Box::new(ToolDoneEvent::error(
            TOOL_ID.to_owned(),
            SESSION_CLOSED,
        )))
    }

    /// A grandchild's events as the parent finally sees them: stamped by the
    /// relay of the session that ran them, then handled by the relay this
    /// session runs while it is blocked on that nested task.
    fn through_two_relays(events: Vec<AgentEvent>) -> Vec<Envelope> {
        let from_grandchild = relay_hop(
            session_info(GRANDCHILD_ID),
            events.into_iter().map(envelope).collect(),
        );
        without_digests(relay_hop(parent_info(), from_grandchild))
    }

    /// Restamped, these would open a card in this session's transcript for a
    /// call it never made, and `ToolOutput` would re-send its whole buffer
    /// once per hop. The digest refuses the same nesting.
    #[test_case(tool_pending()     ; "pending")]
    #[test_case(tool_input_delta() ; "input_delta")]
    #[test_case(tool_output()      ; "output")]
    fn a_grandchilds_live_tool_event_stops_at_the_session_that_ran_it(event: AgentEvent) {
        assert!(
            through_two_relays(vec![event]).is_empty(),
            "{GRANDCHILD_LEAKED}"
        );
    }

    /// The kinds this relay already forwarded are left alone: the guard is on
    /// the newly forwarded ones, so a grandchild's call still arrives wearing
    /// this session's identity, exactly as it always has.
    #[test_case(tool_start(PENDING_TOOL) ; "start")]
    #[test_case(tool_done()              ; "done")]
    fn a_grandchilds_finished_call_is_relayed_as_before(event: AgentEvent) {
        let expected = serde_json::to_value(&event).unwrap();

        let envelopes = through_two_relays(vec![event]);

        let [envelope] = envelopes.as_slice() else {
            panic!("{GRANDCHILD_REROUTED}");
        };
        assert_eq!(serde_json::to_value(&envelope.event).unwrap(), expected);
        assert_eq!(
            envelope
                .subagent
                .as_ref()
                .map(|info| info.parent_tool_use_id.as_str()),
            Some(PARENT_ID),
            "{GRANDCHILD_REROUTED}"
        );
    }

    #[test]
    fn grouped_history_expands_with_stable_parent_chain_and_round_trips() {
        const CALL_ID: &str = "call-1";
        const TOOL_NAME: &str = "read";
        let messages = vec![
            Message::user("inspect".into()),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "checking".into(),
                    },
                    ContentBlock::tool_use(CALL_ID, TOOL_NAME, json!({"path": "src/lib.rs"})),
                ],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: CALL_ID.into(),
                    content: "contents".into(),
                    is_error: false,
                    output_ref: None,
                }],
                ..Default::default()
            },
        ];

        let items = expand_history(&messages);

        assert!(
            items
                .windows(2)
                .all(|pair| pair[1].parent_id == Some(pair[0].id))
        );
        let projected = History::restored(items).unwrap().into_vec();
        assert_eq!(
            serde_json::to_value(projected).unwrap(),
            serde_json::to_value(messages).unwrap()
        );
    }
}
