//! Subagent sessions: a child agent with its own history, tools, and cancel
//! scope, whose events are relayed to the parent stamped with its identity.
//!
//! The native `task` tool and the workflow engine (both through
//! [`crate::agent::task_runner`]) and `caudra.agent.session` open sessions
//! through here. The task path resolves its prompt and tools from a profile;
//! both paths use the Subagent model purpose unless explicitly overridden.

use std::env;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_lock::Mutex as AsyncMutex;
use caudra_config::decisions::FeatureMode;
use caudra_config::{CODE_TOOL_NAME, ModelPolicy, ProfileToolPolicy};
use caudra_decision::{Answer, DecisionResponse, ScoreAnswer};
use serde_json::{Value as JsonValue, json};
use tracing::{info, warn};

use caudra_providers::model::{Model, ModelPurpose};
use caudra_providers::model_lookup::{ModelLookupError, find_model};
use caudra_providers::model_registry::{self, Binding};
use caudra_providers::provider;
use caudra_providers::{
    CacheKey, HistoryItem, Message, ThinkingConfig, TokenUsage, add_cost, expand_message,
};
use caudra_storage::{
    background::JobOwner,
    decision_log::DecisionEffect,
    id::CaudraId,
    sessions::{RuntimeRetry, SessionDatabase},
    thinking::StoredThinking,
    tool_outputs::ToolOutputStore,
};

use super::cache_warming::{CacheWarmer, WarmerContext, subagent_mode};
use super::favorites;
use super::live_switch::{ConversationControl, SettingsChange};
use super::steering::{SharedSteering, Steering};
use super::{ModelRoute, resolve_model_for_purpose, resolve_purpose_model_for_inspection};
use crate::background::JobScope;
use crate::cancel::{CancelMap, CancelSlot, CancelToken};
use crate::code::CodeSession;
use crate::code::output::snippet_summary;
use crate::context::LiveHistory;
use crate::decisions::questions as decision_questions;
use crate::decisions::{
    DecisionFeature, DecisionReceipt, DecisionState, Decisions, NoteDraft, NoteOutcome,
    redacted_excerpt,
};
use crate::memory::store::MemoryStore;
use crate::permissions::PassiveDecisionRevision;
use crate::prompt::PromptId;
use crate::prompt::profile::SystemPromptProfile;
use crate::tools::native::batch::{self, MAX_BATCH_SIZE};
use crate::tools::native::memory;
use crate::tools::native::plan::PlanTarget;
use crate::tools::{
    Deadline, DescriptionContext, FileReadTracker, LocalTools, MEMORY_TOOL_NAME, ToolAudience,
    ToolContext, ToolFilter, ToolLive,
};
use crate::types::counted;
use crate::{
    ActivityChild, Agent, AgentError, AgentEvent, AgentInput, AgentMode, AgentParams,
    AgentRunParams, BatchToolStatus, CallStage, DoneReason, Envelope, EventSender, History,
    InterruptSource, McpSession, SharedHistory, SteeringQueue, SteeringQueueReceiver,
    SubagentActivity, SubagentEnd, SubagentHistoryError, SubagentHistoryLease,
    SubagentHistoryStore, SubagentInfo, SubagentProgress, SubagentTaskMode, SubagentTaskSpec,
    SubagentTaskSpecCandidate, ToolInput, ToolOutput, reasoning_summary, steering_queue,
};

pub const STRUCTURED_OUTPUT_TOOL: &str = "structured_output";
pub const BUILTIN_TASK_PROFILE_DESCRIPTION: &str = "Caudra\'s built-in task prompt";
pub const SESSION_CLOSED: &str = "session closed";
pub(crate) const RESERVATION_SESSION_MISMATCH: &str =
    "task identity reservation belongs to a different session; use this session's job scope";
const IDENTITY_CONNECTION_ERROR: &str = "task identity connection";
const IDENTITY_LOOKUP_ERROR: &str = "task identity lookup";
pub const CANCELLED: &str = "cancelled";
pub(super) const TURN_LIMIT: &str = "subagent reached its maximum turn limit";
pub(super) const TRUNCATED: &str = "subagent response was cut off at its output token limit";
const DEFAULT_SESSION_AUDIENCE: ToolAudience = ToolAudience::GENERAL_SUB;
/// A thought\'s title is its first bold line. Past this the block is into
/// prose and will never resolve one, so it stops being accumulated and a
/// long reasoning stream cannot be buffered a second time for nothing.
const THOUGHT_TITLE_SCAN_LIMIT: usize = 200;
pub(crate) const DIFFICULTY_QUESTION: &str = "difficulty";
const ROUTINE_LEVEL: usize = 1;
const OPEN_ENDED_LEVEL: usize = 3;
const SUBAGENT_PROMPT_EXCERPT_BYTES: usize = 900;
const ROUTING_INPUT_SCOPE: &str = "task_label_and_prompt_excerpt";
/// Each difficulty level by index, in the words of its criterion.
const DIFFICULTY_LEVELS: [&str; 4] = ["lookup", "routine", "judgment", "open-ended"];
const ROUTINE_BAND: &str = "routine or less";
const OPEN_ENDED_BAND: &str = "open-ended";
const ROUTED: &str = "routed to";
const ROUTING_FALLBACK: &str = "kept the default subagent model";
const BASELINE_LABEL: &str = "baseline";
const CHOSEN_LABEL: &str = "chosen";
const MODEL_SPEC_SEPARATOR: char = '/';
const LIVE_MODEL_LISTING_DEADLINE: Duration = Duration::from_secs(15);
/// Host calls a subagent's `code` row shows beneath it, newest last; its own
/// text counts them all.
const MAX_CODE_CALL_ROWS: usize = 8;
const CALL_NOUN: &str = "call";

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
            AgentEvent::Error { .. } => {
                let _ = usage_tx.send(TokenUsage::default());
                continue;
            }
            // Already stamped, so this is a grandchild's live tool work and
            // this session sits blocked on the nested task that ran it.
            // Restamping would open a card in this session's transcript for a
            // call it never made, and a `ToolOutput` flush carries the whole
            // accumulated buffer, recloned at every hop it crosses.
            AgentEvent::ToolPending { .. }
            | AgentEvent::ToolInputDelta { .. }
            | AgentEvent::ToolOutput { .. }
            | AgentEvent::SubagentFinished { .. }
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
/// A `code` run is watched the same way, its host calls as the children.
struct BatchWatch {
    id: String,
    children: Vec<ActivityChild>,
    /// How many host calls a `code` run has started, of which `children` holds
    /// the newest [`MAX_CODE_CALL_ROWS`]. `None` for a batch.
    calls: Option<usize>,
}

impl BatchWatch {
    /// The row a child's index lands on, `None` once a `code` run's window
    /// has moved past it.
    fn slot(&self, index: usize) -> Option<usize> {
        match self.calls {
            Some(total) => index.checked_sub(total - self.children.len()),
            None => Some(index),
        }
    }

    /// One host call changed: a new one joins the window, pushing the oldest
    /// out past the bound, and a known one is patched where it sits.
    fn code_call(&mut self, index: usize, call: ActivityChild) -> Option<()> {
        let total = self.calls?;
        if index >= total {
            self.calls = Some(index + 1);
            self.children.push(call);
            if self.children.len() > MAX_CODE_CALL_ROWS {
                self.children.remove(0);
            }
            return Some(());
        }
        let slot = self.slot(index)?;
        *self.children.get_mut(slot)? = call;
        Some(())
    }
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
            }) if call_id == &watch.id => {
                let summary = match watch.calls {
                    Some(total) if total > 0 => counted(total, CALL_NOUN),
                    _ => summary.clone(),
                };
                Some(
                    SubagentActivity::batch(Arc::clone(name), &summary, watch.children.clone())
                        .with_call_id(&watch.id),
                )
            }
            _ => None,
        }
    }

    /// The call's own row in `stage`. `None` when the last row belongs to some
    /// other call, which leaves the activity as it was.
    fn restaged(&self, id: &str, stage: Option<CallStage>) -> Option<SubagentActivity> {
        match &self.last {
            Some(
                activity @ SubagentActivity::Tool {
                    call_id: Some(call_id),
                    ..
                },
            ) if call_id == id => Some(activity.clone().with_stage(stage)),
            _ => None,
        }
    }

    /// A request names the call that raised it, and that call keeps its row,
    /// so the reader sees what is waiting on them rather than only that
    /// something is. A batch child is marked in the roster the batch's row
    /// goes on drawing, which is also what keeps the child's next progress
    /// landing on a row that is still watched.
    fn awaiting_approval(&mut self, id: &str) -> SubagentActivity {
        if let Some(watch) = self.batch.as_mut()
            && let Some(slot) =
                batch::child_index(&watch.id, id).and_then(|index| watch.slot(index))
            && let Some(child) = watch.children.get_mut(slot)
        {
            // A host call's row is running from the moment it is made, and is
            // republished as running once let through.
            child.status = match child.host_call {
                true if !child.status.is_terminal() => BatchToolStatus::AwaitingApproval,
                _ => child.status.max(BatchToolStatus::AwaitingApproval),
            };
            if let Some(row) = self.watched_batch() {
                return row;
            }
        }
        self.restaged(id, Some(CallStage::AwaitingApproval))
            .unwrap_or(SubagentActivity::AwaitingPermission)
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
                let snippet = match &start.input {
                    Some(ToolInput::Code { code, .. }) if &*start.tool == CODE_TOOL_NAME => {
                        Some(snippet_summary(code))
                    }
                    _ => None,
                };
                self.batch = match &start.output {
                    Some(ToolOutput::Batch { entries, .. }) => Some(BatchWatch {
                        id: start.id.clone(),
                        children: entries
                            .iter()
                            .take(MAX_BATCH_SIZE)
                            .map(ActivityChild::from)
                            .collect(),
                        calls: None,
                    }),
                    _ => snippet.is_some().then(|| BatchWatch {
                        id: start.id.clone(),
                        children: Vec::new(),
                        calls: Some(0),
                    }),
                };
                let summary = snippet.as_deref().unwrap_or(&start.summary);
                Some(
                    match &self.batch {
                        Some(watch) => SubagentActivity::batch(
                            Arc::clone(&start.tool),
                            summary,
                            watch.children.clone(),
                        ),
                        None => SubagentActivity::tool(Arc::clone(&start.tool), summary),
                    }
                    .with_call_id(&start.id),
                )
            }
            // A host call of the watched `code` run joined it or moved.
            AgentEvent::CodeProgress(event) => {
                let watch = self.batch.as_mut().filter(|w| w.id == event.id)?;
                watch.code_call(event.index, ActivityChild::from(&event.call))?;
                self.watched_batch()
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
            // The arguments closed with no header left to reveal, so the row
            // the call already has stops being written and keeps the header
            // it earned, which the event alone does not carry.
            AgentEvent::ToolInputDelta {
                id,
                preview: None,
                complete: true,
                ..
            } => self.restaged(id, None),
            AgentEvent::PermissionRequest(request) => Some(self.awaiting_approval(&request.id)),
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
            task: envelope.task.clone(),
        });
    }
}

pub struct Subagent {
    params: AgentParams,
    system: String,
    tools: JsonValue,
    mode: AgentMode,
    /// The parent's session plan, which this child may read but never write.
    plan: Option<PlanTarget>,
    environment: Option<String>,
    mode_notice: Option<String>,
    thinking: ThinkingConfig,
    fast: bool,
    mcp: Option<McpSession>,
    /// Its own, so its `code` variables never mix with its parent's.
    code: CodeSession,
    history: History,
    /// Holds every message `history` commits, as it commits it.
    mirror: SharedHistory,
    /// Lists `mirror` for a side question over this task while it is open.
    live_history: Option<LiveHistory>,
    history_lease: Option<SubagentHistoryLease>,
    sub_event_tx: EventSender,
    child_cancel: crate::cancel::CancelToken,
    interrupt_source: Arc<SteeringQueueReceiver>,
    answer_rx: Arc<AsyncMutex<flume::Receiver<String>>>,
    answer_tx: Option<flume::Sender<String>>,
    steer_tx: Option<SteeringQueue>,
    /// The user's picks of model and level, and the wake for a backoff.
    control: ConversationControl,
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
    terminal_report: Option<Arc<AtomicBool>>,
    /// Recalled from on every prompt, with no memory view in its prompt.
    memory_store: Option<Arc<MemoryStore>>,
}

impl Subagent {
    pub(crate) fn discard_unstarted(&mut self) {
        self.closed = true;
        self.parent_cancels.retire(&self.task_id, self.cancel_slot);
        self.history_lease.take();
        self.live_history.take();
    }

    pub(crate) fn checkpoint(&self) -> Result<(JsonValue, JsonValue), String> {
        let history =
            serde_json::to_value(self.history.as_slice()).map_err(|error| error.to_string())?;
        let spec = serde_json::to_value(self.history_lease.as_ref().and_then(|lease| lease.spec()))
            .map_err(|error| error.to_string())?;
        Ok((history, spec))
    }

    /// The transcript as each message commits, for a saver that keeps it
    /// while the task runs.
    pub(crate) fn mirror(&self) -> SharedHistory {
        Arc::clone(&self.mirror)
    }

    pub(crate) fn report_ready(&self) -> bool {
        self.report_ready
            .as_ref()
            .is_some_and(|ready| ready.load(Ordering::Acquire))
    }

    pub(crate) async fn drain_jobs(&self) -> Result<(), String> {
        if let Some(jobs) = &self.params.jobs {
            jobs.cancel_and_drain().await?;
        }
        Ok(())
    }

    pub fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.parent_cancels.retire(&self.task_id, self.cancel_slot);
        if let Some(untaken) = self.control.live.take_pending() {
            self.adopt(untaken);
        }
        let messages = std::mem::replace(&mut self.history, History::new(Vec::new())).into_vec();
        let persisted_spec = self
            .history_lease
            .as_ref()
            .and_then(|lease| lease.spec().cloned());
        if let Some(lease) = self.history_lease.take() {
            lease.complete_version(Arc::new(messages.clone()), self.parent_tool_use_id.clone());
        }
        self.live_history.take();
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

/// What every turn of a task runs under, whoever wrote its message: steering
/// carries the task's own mode, plan, and reasoning, never its sender's.
#[derive(Clone)]
pub(crate) struct TurnSettings {
    mode: AgentMode,
    plan: Option<PlanTarget>,
    thinking: ThinkingConfig,
    fast: bool,
}

impl TurnSettings {
    pub(crate) fn input(&self, message: String) -> AgentInput {
        AgentInput {
            resume: false,
            message,
            mode: self.mode.clone(),
            plan: self.plan.clone(),
            images: Vec::new(),
            mentions: Vec::new(),
            commits: Vec::new(),
            preamble: Vec::new(),
            thinking: self.thinking.clone(),
            fast: self.fast,
            prompt: None,
        }
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

    /// The queue the task is steered through, until the first prompt hands it
    /// to the UI.
    pub(crate) fn steer_queue(&self) -> Option<SteeringQueue> {
        self.steer_tx.clone()
    }

    pub(crate) fn turn_settings(&self) -> TurnSettings {
        TurnSettings {
            mode: self.mode.clone(),
            plan: self.plan.clone(),
            thinking: self.thinking.clone(),
            fast: self.fast,
        }
    }

    /// Makes a pick the task's own, so its next turn and its saved spec
    /// carry it. A move the provider made on its own stays with the turn.
    fn adopt(&mut self, change: SettingsChange) {
        let model = change.model_spec();
        let thinking = change.thinking.clone().map(StoredThinking::from);
        if let Some((provider, model)) = change.route {
            if self.params.chat_model.spec() == self.params.model.spec() {
                self.params.chat_provider = Arc::clone(&provider);
                self.params.chat_model = model.clone();
            }
            self.params.provider = provider;
            self.params.model = model;
        }
        if let Some(level) = change.thinking {
            self.thinking = level;
        }
        if let Some(lease) = &mut self.history_lease {
            lease.retarget(model, thinking);
        }
    }

    pub fn model_spec(&self) -> String {
        self.params.model.spec()
    }

    /// The level this session's requests carry, snapped to its model. `None`
    /// when the model cannot reason at all.
    pub fn effective_thinking(&self) -> Option<String> {
        self.thinking.label_for(&self.params.model)
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

    pub(crate) fn set_terminal_report(&mut self, ready: Arc<AtomicBool>) {
        self.terminal_report = Some(ready);
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
                model: Some(self.model_spec()),
                thinking: self.effective_thinking(),
                fast: self.fast,
                answer_tx: self.answer_tx.take(),
                steer_tx: self.steer_tx.take(),
                control: Some(self.control.clone()),
            });
        }

        let mut input = AgentInput {
            resume,
            preamble,
            ..self.turn_settings().input(message.unwrap_or_default())
        };
        // A replacement stops only the turn it lands in; the task carries on
        // with the replacement as its next turn, and its caller keeps waiting.
        let (result, text) = loop {
            let (trigger, turn) = self.child_cancel.child();
            self.interrupt_source.arm(trigger);
            let outcome = self.run_turn(input, turn).await;
            self.interrupt_source.disarm();
            if self.child_cancel.is_cancelled() {
                break outcome;
            }
            let Some((id, replacement)) = self.interrupt_source.take_replacement() else {
                break outcome;
            };
            self.stop_jobs().await;
            let _ = self.sub_event_tx.send(AgentEvent::QueueItemConsumed {
                id,
                text: replacement.message.clone(),
                images: replacement.images.clone(),
            });
            input = AgentInput {
                images: replacement.images,
                mentions: replacement.mentions,
                commits: replacement.commits,
                ..self.turn_settings().input(replacement.message)
            };
        };
        self.finish_prompt(result, text)
    }

    /// Stops the background work this task started, leaving it free to start
    /// more: a replacement ends a turn, not the task.
    async fn stop_jobs(&self) {
        let Some(jobs) = &self.params.jobs else {
            return;
        };
        for card in jobs.list().into_iter().filter(|card| card.active()) {
            if let Err(error) = jobs.cancel(&card.task_id).await {
                warn!(name = %self.name, task_id = %card.task_id, %error, "replacement could not stop a job");
            }
        }
        jobs.reopen();
    }

    async fn run_turn(
        &mut self,
        input: AgentInput,
        cancel: CancelToken,
    ) -> (Result<DoneReason, AgentError>, String) {
        let interrupt_source: Arc<dyn InterruptSource> = self.interrupt_source.clone();
        let mut agent = Agent::new(
            self.params.clone(),
            AgentRunParams {
                environment: self.environment.clone(),
                instructions: None,
                memory: None,
                configuration: None,
                mode_notice: self.mode_notice.clone(),
                history: &mut self.history,
                system: self.system.clone(),
                event_tx: self.sub_event_tx.clone(),
                tools: self.tools.clone(),
                tools_fixed: false,
                memory_store: self.memory_store.clone(),
            },
        )
        .with_user_response_rx(Arc::clone(&self.answer_rx))
        .with_interrupt_source(interrupt_source)
        .with_cancel(cancel)
        .with_mcp(self.mcp.clone())
        .with_code(self.code.clone())
        .with_live_switch(self.control.live.clone())
        .with_retry_now(self.control.retry_now.clone())
        .with_local_tools(Arc::clone(&self.local_tools))
        .with_steering(Arc::clone(&self.steering))
        .with_cache_warmer(CacheWarmer::new(
            subagent_mode(self.params.config.cache_warming),
            WarmerContext {
                session_id: self
                    .params
                    .session_id
                    .as_ref()
                    .map(|id| id.id().to_string())
                    .unwrap_or_default(),
                subagent: Some(self.parent_tool_use_id.clone()),
                cwd: self.params.task_environment.apply("{cwd}").into_owned(),
            },
        ));
        if let Some(ready) = &self.report_ready {
            agent = agent.with_report_ready(Arc::clone(ready));
        }
        if let Some(ready) = &self.terminal_report {
            agent = agent.with_terminal_report(Arc::clone(ready));
        }

        let result = agent.run(input).await;
        // Compaction can replace the transcript, invalidating its old length as
        // an output boundary. The agent's run-local response survives that rewrite.
        let text = agent.response_text().unwrap_or_default().to_owned();
        // Agent owns completed billing, including compaction and evaluation. Read
        // it before drop even on errors, which deliberately do not emit Done.
        self.usage += agent.usage();
        drop(agent);
        if let Some(applied) = self.control.live.take_applied() {
            self.adopt(applied);
        }
        if let Err(error) = &result {
            let _ = self.sub_event_tx.send(AgentEvent::Error {
                message: error.to_string(),
            });
        }
        if self.usage_rx.recv_async().await.is_err() {
            tracing::warn!(name = %self.name, "subagent event relay stopped before settlement");
        }
        (result, text)
    }

    fn finish_prompt(
        &self,
        result: Result<DoneReason, AgentError>,
        text: String,
    ) -> Result<PromptResult, PromptFailure> {
        // A normal runtime exit can still leave the task unfinished. Only a
        // completed response may enter task-report validation or correction.
        let cut_short = match &result {
            Err(error) => Some(error.to_string()),
            Ok(DoneReason::Cancelled) => Some(CANCELLED.to_owned()),
            Ok(DoneReason::MaxTurns) => Some(TURN_LIMIT.to_owned()),
            Ok(DoneReason::MaxTokens) => Some(TRUNCATED.to_owned()),
            Ok(_) => None,
        };
        let partial = (!text.is_empty()).then(|| text.clone());
        let end = match &cut_short {
            None => SubagentEnd::Completed {
                output: text.clone(),
            },
            Some(_) if matches!(result, Ok(DoneReason::Cancelled)) => SubagentEnd::Cancelled {
                partial: partial.clone(),
            },
            Some(error) => SubagentEnd::Failed {
                error: error.clone(),
                partial: partial.clone(),
            },
        };
        let _ = self.sub_event_tx.send(AgentEvent::SubagentFinished { end });
        if let Some(error) = cut_short {
            return Err(PromptFailure { error, partial });
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
    tool_ceiling: ToolFilter,
    profile_tool_policy: Arc<ProfileToolPolicy>,
    model: Model,
    provider: Arc<dyn provider::Provider>,
    system: String,
    tools: JsonValue,
    /// Tools the child reaches only through a `code` module, so absent from `tools`.
    hosted: Vec<String>,
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
#[derive(Debug)]
pub enum TaskIdentity {
    Derive,
    /// A new task under an id the caller chose, such as a workflow engine
    /// that must find the same task again after a restart.
    Fresh(String),
    Exact(String),
    Reserved(SubagentHistoryLease),
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
            || matches!(self, Self::Reserved(lease) if lease.history().is_some())
    }

    pub(crate) fn requested(&self) -> Option<&str> {
        match self {
            Self::Derive => None,
            Self::Fresh(id) | Self::Exact(id) | Self::Continue(id) => Some(id),
            Self::Reserved(lease) => Some(lease.task_id()),
        }
    }
}

/// What the caller asked to run a task on. A chosen model or level outranks a
/// profile's pin, which in turn outranks a job.
#[derive(Debug, Clone, Default)]
pub struct TaskModel {
    /// Which model job runs this one task, when the caller knows what kind of
    /// work it is, as a workflow script does. A profile that pins
    /// `subagent_model` still wins, because the user configured that and the
    /// caller only asked for a job. Not persisted with the task, so a
    /// continuation resolves the model the ordinary way.
    pub job: Option<ModelPurpose>,
    /// A model as someone named it: an exact `provider/model` spec or a short
    /// name such as `sonnet`. Stored with the task once resolved.
    pub name: Option<String>,
    /// Stored with the task as asked, and snapped to the model per request.
    pub thinking: Option<StoredThinking>,
}

/// A `task` call: the profile may override the Subagent model and thinking;
/// prompt, tools, and mode otherwise come from task policy.
pub struct TaskOptions {
    pub name: String,
    pub task_id: TaskIdentity,
    pub profile: Option<String>,
    pub mode: Option<SubagentTaskMode>,
    pub model: TaskModel,
    /// The prompt the task will be sent, read only by subagent routing, which
    /// sees a redacted excerpt of it.
    pub routing_prompt: Option<String>,
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
    let continuation = opts.task_id.is_continuation();
    let ids = Identity::derive(ctx, opts.task_id.requested());
    let default_spec = SubagentTaskSpec {
        profile_name: ctx.default_task_prompt_profile_name.to_string(),
        mode: inherited_mode(&ctx.mode),
        ..SubagentTaskSpec::default()
    };
    let chosen_model = match opts.model.name.as_deref() {
        Some(name) => Some(resolve_task_model(ctx, name).await?),
        None => None,
    };
    let candidate = SubagentTaskSpecCandidate {
        profile_name: opts.profile,
        mode: opts.mode,
        model: chosen_model,
        thinking: opts.model.thinking,
    };
    let (task_id, history_lease) = match opts.task_id {
        TaskIdentity::Continue(_) => (
            ids.task_id.clone(),
            ctx.subagent_history
                .continue_task_with_defaults(&ids.task_id, candidate, default_spec)
                .map_err(|error| error.to_string())?,
        ),
        TaskIdentity::Exact(_) | TaskIdentity::Fresh(_) => {
            let lease = ctx
                .subagent_history
                .reserve_with_spec(ids.task_id.clone(), candidate.or_defaults(default_spec))
                .map_err(|error| error.to_string())?;
            (ids.task_id.clone(), lease)
        }
        TaskIdentity::Reserved(lease) => {
            let spec = lease
                .spec()
                .cloned()
                .unwrap_or_else(|| candidate.or_defaults(default_spec));
            (lease.task_id().to_owned(), lease.with_spec(spec))
        }
        TaskIdentity::Derive => {
            let lease = reserve_task_identity_async(ctx, &opts.name).await?;
            (
                lease.task_id().to_owned(),
                lease.with_spec(candidate.or_defaults(default_spec)),
            )
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
    let catalog_profile = ctx
        .prompt_profiles
        .resolve(Some(&spec.profile_name))
        .map_err(|error| error.to_string())?;
    // A task that chose its own model or level never runs the profile on the
    // Subagent model, so that model cannot make the profile unavailable.
    let chosen = spec.model.is_some() || spec.thinking.is_some();
    let profile = if chosen {
        catalog_profile
    } else {
        bindings
            .resolve(&spec.profile_name)
            .map_err(|error| error.to_string())?
    };

    let vars = ctx.task_environment.clone().set(
        "{task_system_prompt_profiles}",
        bindings.task_tool_summary(BUILTIN_TASK_PROFILE_DESCRIPTION),
    );
    let instructions = match &ctx.remote_project_context {
        Some(context) => {
            crate::agent::load_remote_instructions(context, ctx.host_cwd.as_deref()).text
        }
        None => {
            let cwd = vars.apply("{cwd}").into_owned();
            smol::unblock(move || crate::agent::load_instruction_text(&cwd)).await
        }
    };
    let mut routed = match spec.model {
        Some(_) => None,
        None => {
            route_subagent(
                ctx,
                continuation,
                profile.as_deref(),
                opts.model.job,
                RoutingTask {
                    label: &opts.name,
                    prompt: opts.routing_prompt.as_deref(),
                    mode: spec.mode,
                    profile: (*spec.profile_name != *ctx.default_task_prompt_profile_name)
                        .then_some(spec.profile_name.as_str()),
                },
            )
            .await
        }
    };
    let chosen_binding = spec.model.clone().map(Binding::Exact);
    let asked = opts
        .model
        .job
        .or_else(|| routed.as_ref().map(|route| route.purpose))
        .map(Binding::Same);
    let model_binding =
        subagent_model_binding(chosen_binding.as_ref(), profile.as_deref(), asked.as_ref());
    let baseline = routed
        .as_ref()
        .and_then(|_| inspected_subagent_model(ctx, None));
    let resolved = resolve_provider(ctx, model_binding).await;
    let (model, provider) = match resolved {
        result
            if routed.as_ref().is_some_and(|route| {
                result.is_err() || !ctx.permissions.passive_decision_is_current(route.revision)
            }) =>
        {
            routed = None;
            resolve_provider(ctx, None).await?
        }
        result => result?,
    };
    announce_model(ctx, &model);

    let thinking = spec
        .thinking
        .clone()
        .or_else(|| {
            profile
                .as_deref()
                .and_then(|profile| profile.subagent_thinking().cloned())
        })
        .map(ThinkingConfig::from)
        .unwrap_or_else(|| ctx.opts.thinking.clone());
    if !chosen
        && profile.as_ref().is_some_and(|profile| {
            profile.subagent_model().is_some() || profile.subagent_thinking().is_some()
        })
        && let Err(error) = thinking.resolve_exact(&model)
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
    let base_filter = ToolFilter::from_config(&ctx.config, &model, &[])
        .intersect(&ctx.tool_ceiling)
        .for_mode(&mode);
    let profile_tool_policy = Arc::new(
        profile
            .as_ref()
            .map(|profile| profile.tools().clone())
            .unwrap_or_default(),
    );
    profile_tool_policy.validate_bindings(
        ctx.registry
            .iter()
            .iter()
            .map(|entry| entry.name())
            .chain(opts.local_tools.keys().map(String::as_str)),
    )?;
    let prompt_filter = ctx.registry.profile_filter(
        &DescriptionContext {
            filter: &base_filter,
            audience,
            workflows_available: false,
        },
        &profile_tool_policy,
        ctx.has_session_plan(),
    );
    let guidance = crate::prompt::execution_guidance(
        &ctx.config,
        false,
        ctx.job_scope().is_some(),
        false,
        prompt_filter.matches(crate::tools::SHELL_TOOL_NAME)
            && ctx.registry.get(crate::tools::SHELL_TOOL_NAME).is_some(),
    );
    let prompt_slots = ctx.prompt_slots.with_execution_guidance(&guidance);
    let mut assembled = crate::prompt::assemble_task_with_filter(
        prompt_id,
        &prompt_slots,
        &prompt_filter,
        &instructions,
        profile.as_deref(),
    );
    if opts
        .local_tools
        .contains_key(crate::tools::native::report_to_parent::NAME)
    {
        assembled.push_str("\n\n");
        assembled.push_str(crate::tools::native::report_to_parent::CONTRACT);
    }
    let mut definitions = ctx.registry.definitions_split_with_policy(
        &vars,
        &DescriptionContext {
            filter: &base_filter,
            audience,
            workflows_available: false,
        },
        model.supports_tool_examples(),
        &profile_tool_policy,
        ctx.has_session_plan(),
    );
    crate::tools::execution::configure_declared(
        &mut definitions.declared,
        &ctx.config,
        false,
        ctx.job_scope().is_some(),
    );
    crate::tools::profile_policy::append_local_definitions(
        &mut definitions,
        opts.local_definitions,
        &opts.local_tools,
        &base_filter,
        &profile_tool_policy,
    );
    let profile_name: Arc<str> = Arc::from(spec.profile_name.as_str());
    // The child's own model, which a profile's `subagent_model` can move away
    // from the parent's.
    let environment = crate::agent::environment_block(&vars, &model);

    let subagent = build(
        ctx,
        ids,
        Resolved {
            tool_ceiling: ctx.tool_ceiling.clone().for_mode(&mode),
            profile_tool_policy,
            model,
            provider,
            system: vars.apply(&assembled).into_owned(),
            tools: definitions.declared,
            hosted: definitions.hosted,
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
    )?;
    if let Some(route) = routed
        && let Some(baseline) = baseline
        && baseline.spec() != subagent.params.model.spec()
    {
        ctx.emit_decision(routing_note(
            &route.decisions,
            NoteOutcome::Acted,
            route.purpose,
            &route.difficulty,
            &baseline,
            &subagent.params.model,
        ));
        if let Some(receipt) = route.receipt {
            route
                .decisions
                .record_effect_detached(&receipt, DecisionEffect::Rerouted);
        }
    }
    Ok(subagent)
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
        None => reserve_fresh(ctx, ids.task_id.clone(), None).await?,
    };
    let model_binding = opts.model_spec.map(Binding::Exact);
    let (model, provider) = resolve_provider(ctx, model_binding.as_ref()).await?;
    announce_model(ctx, &model);
    let tool_ceiling = ToolFilter::Only(
        ctx.registry
            .iter()
            .iter()
            .map(|entry| entry.name().to_owned())
            .chain(ctx.local_tools.keys().cloned())
            .filter(|name| ctx.tool_available(name))
            .collect(),
    );
    let mut inherited = ctx.clone();
    inherited.mcp = inherited.mcp.map(|mcp| {
        mcp.with_actor_policy(Arc::clone(&ctx.profile_tool_policy))
            .freeze_actor_ceiling()
    });
    build(
        &inherited,
        ids,
        Resolved {
            model,
            provider,
            system: opts.system,
            tool_ceiling,
            profile_tool_policy: Arc::clone(&ctx.profile_tool_policy),
            tools: opts.tools,
            hosted: Vec::new(),
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

pub(crate) fn reserve_task_identity(
    history: &SubagentHistoryStore,
    label: &str,
    outputs: Option<&ToolOutputStore>,
    session: Option<CaudraId>,
    jobs: Option<&JobScope>,
) -> Result<SubagentHistoryLease, String> {
    let retry = RuntimeRetry::new(None, &|| false);
    reserve_task_identity_with_retry(history, label, outputs, session, jobs, &retry)
}

async fn reserve_task_identity_async(
    ctx: &ToolContext,
    label: &str,
) -> Result<SubagentHistoryLease, String> {
    let history = ctx.subagent_history.clone();
    let outputs = ctx.tool_output_store.clone();
    let session = ctx.session_id.as_ref().map(|id| id.id());
    let jobs = ctx.job_scope();
    let cancel = ctx.cancel.clone();
    let deadline = match ctx.deadline {
        Deadline::None => None,
        Deadline::At(instant) => Some(instant),
    };
    let label = label.to_owned();
    let lease = smol::unblock(move || {
        let cancelled = || cancel.is_cancelled();
        let retry = RuntimeRetry::new(deadline, &cancelled);
        reserve_task_identity_with_retry(
            &history,
            &label,
            outputs.as_deref(),
            session,
            jobs.as_ref(),
            &retry,
        )
    })
    .await;
    if ctx.cancel.is_cancelled() {
        return Err(CANCELLED.into());
    }
    ctx.deadline.check()?;
    lease
}

pub(crate) fn reserve_task_identity_with_retry(
    history: &SubagentHistoryStore,
    label: &str,
    outputs: Option<&ToolOutputStore>,
    session: Option<CaudraId>,
    jobs: Option<&JobScope>,
    retry: &RuntimeRetry<'_>,
) -> Result<SubagentHistoryLease, String> {
    if let Some(jobs) = jobs
        && session.is_some_and(|session| session != jobs.session_id())
    {
        return Err(RESERVATION_SESSION_MISMATCH.into());
    }
    let session = session.or_else(|| jobs.map(JobScope::session_id));
    let database = outputs
        .map(ToolOutputStore::state_dir)
        .or_else(|| jobs.map(JobScope::state_dir))
        .zip(session)
        .map(|(dir, session)| {
            SessionDatabase::open_runtime(dir, retry)
                .map(|database| (database, session))
                .map_err(|error| format!("{IDENTITY_CONNECTION_ERROR}: {error}"))
        })
        .transpose()?;
    history.reserve_generated(label, |id| match &database {
        Some((database, session)) => database
            .task_identity_exists_runtime(*session, id, retry)
            .map_err(|error| format!("{IDENTITY_LOOKUP_ERROR}: {error}")),
        None => Ok(false),
    })
}

/// A tool call that ran twice under one id collides with its own history, so
/// a taken id is answered with a fresh one rather than an error.
async fn reserve_fresh(
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
            let lease = reserve_task_identity_async(ctx, "task")
                .await?
                .with_spec(spec.unwrap_or_else(SubagentTaskSpec::generic));
            Ok((lease.task_id().to_owned(), lease))
        }
        Err(error) => Err(error.to_string()),
    }
}

struct SubagentRoute {
    purpose: ModelPurpose,
    revision: PassiveDecisionRevision,
    decisions: Decisions,
    receipt: Option<DecisionReceipt>,
    difficulty: ScoreAnswer,
}

/// What the router reads about a task. `profile` is set only when the caller
/// chose one, since the default profile says nothing about the work.
pub(crate) struct RoutingTask<'a> {
    pub(crate) label: &'a str,
    pub(crate) prompt: Option<&'a str>,
    pub(crate) mode: SubagentTaskMode,
    pub(crate) profile: Option<&'a str>,
}

async fn route_subagent(
    ctx: &ToolContext,
    continuation: bool,
    profile: Option<&SystemPromptProfile>,
    requested: Option<ModelPurpose>,
    task: RoutingTask<'_>,
) -> Option<SubagentRoute> {
    let revision = ctx.permissions.passive_decision_revision()?;
    if continuation
        || requested.is_some()
        || profile.is_some_and(|profile| profile.subagent_model().is_some())
        || task.label.trim().is_empty()
    {
        return None;
    }
    let decisions = ctx.permissions.decisions()?;
    let feature = DecisionFeature::SubagentRouting;
    if !decisions.enabled(&feature) {
        return None;
    }
    let questions = decision_questions::SUBAGENT.as_ref()?;
    let state = routing_state(&task);
    let context = ctx
        .permissions
        .decision_context(json!({"input_scope": ROUTING_INPUT_SCOPE}));
    let pending = ctx.begin_decision(&decisions, feature.clone());
    let outcome = ctx
        .cancel
        .race(
            ctx.permissions
                .run_passive_decision(decisions.evaluate(feature, &state, questions, &context)),
        )
        .await
        .ok()
        .flatten()
        .flatten();
    drop(pending);
    let outcome = outcome?;
    if !ctx.permissions.passive_decision_is_current(revision) {
        return None;
    }
    let mode = decisions.mode(&DecisionFeature::SubagentRouting).clone();
    let Ok(response) = &outcome.result else {
        if mode == FeatureMode::Enforce
            && let Some(failure) = outcome.fresh_failure()
        {
            ctx.emit_decision(NoteDraft::fell_back(
                &decisions,
                DecisionFeature::SubagentRouting,
                &failure,
                ROUTING_FALLBACK,
            ));
        }
        return None;
    };
    let purpose = subagent_job(response, decisions.config().thresholds.routing_confidence)?;
    let difficulty = difficulty(response)?.clone();
    match mode {
        FeatureMode::Enforce => Some(SubagentRoute {
            purpose,
            revision,
            decisions,
            receipt: outcome.receipt,
            difficulty,
        }),
        FeatureMode::Shadow => {
            if let Some(baseline) = inspected_subagent_model(ctx, None)
                && let Some(chosen) = inspected_subagent_model(ctx, Some(&Binding::Same(purpose)))
                && baseline.spec() != chosen.spec()
            {
                ctx.emit_decision(routing_note(
                    &decisions,
                    NoteOutcome::Shadow,
                    purpose,
                    &difficulty,
                    &baseline,
                    &chosen,
                ));
            }
            None
        }
        FeatureMode::Off | FeatureMode::Advise => None,
    }
}

/// The Subagent model a binding resolves to, without warming the catalog.
fn inspected_subagent_model(ctx: &ToolContext, binding: Option<&Binding>) -> Option<Model> {
    resolve_purpose_model_for_inspection(
        ModelPurpose::Subagent,
        binding,
        &ctx.model,
        &ctx.chat_model,
        &ctx.model_policy,
    )
    .ok()
}

/// The model change and the band that chose it, then each level's
/// probability and both models.
fn routing_note(
    decisions: &Decisions,
    outcome: NoteOutcome,
    purpose: ModelPurpose,
    difficulty: &ScoreAnswer,
    baseline: &Model,
    chosen: &Model,
) -> NoteDraft {
    let (band, probability) = match purpose {
        ModelPurpose::Fast => (ROUTINE_BAND, difficulty.at_most(ROUTINE_LEVEL)),
        _ => (OPEN_ENDED_BAND, difficulty.at_least(OPEN_ENDED_LEVEL)),
    };
    let (chosen, baseline) = (chosen.spec(), baseline.spec());
    let mut details: Vec<String> = DIFFICULTY_LEVELS
        .iter()
        .enumerate()
        .filter_map(|(level, name)| {
            let probability = difficulty.probabilities.get(&level.to_string())?;
            Some(format!("{probability:.2}  {name}"))
        })
        .collect();
    details.push(format!("{BASELINE_LABEL} {baseline}"));
    details.push(format!("{CHOSEN_LABEL} {chosen}"));
    NoteDraft::new(
        decisions,
        DecisionFeature::SubagentRouting,
        outcome,
        &format!("{ROUTED} {chosen} instead of {baseline} (difficulty {band}, {probability:.2})"),
        details,
    )
}

/// The label, a redacted excerpt of the prompt, the mode and any chosen
/// profile; the label alone when the rest does not fit the state cap.
pub(crate) fn routing_state(task: &RoutingTask) -> JsonValue {
    let mut described = json!({"label": task.label});
    if let Some(prompt) = task.prompt {
        described["prompt"] = json!(redacted_excerpt(prompt, SUBAGENT_PROMPT_EXCERPT_BYTES));
    }
    described["mode"] = json!(task.mode);
    if let Some(profile) = task.profile {
        described["profile"] = json!(profile);
    }
    let state = json!({"task": described});
    if DecisionState::new(&state).is_ok() {
        state
    } else {
        json!({"task": {"label": task.label}})
    }
}

fn difficulty(response: &DecisionResponse) -> Option<&ScoreAnswer> {
    match response.answers.get(DIFFICULTY_QUESTION)? {
        Answer::Score(difficulty) => Some(difficulty),
        Answer::Noul(_) | Answer::Choice(_) => None,
    }
}

/// Routine work or less runs on the Fast model and open-ended work on the
/// Best one; anything between keeps the Subagent model.
pub(crate) fn subagent_job(response: &DecisionResponse, threshold: f64) -> Option<ModelPurpose> {
    let difficulty = difficulty(response)?;
    if difficulty.at_most(ROUTINE_LEVEL) >= threshold {
        Some(ModelPurpose::Fast)
    } else if difficulty.at_least(OPEN_ENDED_LEVEL) >= threshold {
        Some(ModelPurpose::Best)
    } else {
        None
    }
}

/// A model chosen for this one task wins over a profile pin, which wins over
/// the job the caller named: the pin is a default the user configured, the
/// choice is what was asked for now, and a job only says what kind of work
/// this is.
fn subagent_model_binding<'a>(
    chosen: Option<&'a Binding>,
    profile: Option<&'a SystemPromptProfile>,
    asked: Option<&'a Binding>,
) -> Option<&'a Binding> {
    chosen
        .or_else(|| profile.and_then(SystemPromptProfile::subagent_model))
        .or(asked)
}

/// The exact spec a task's `model` names among the models this session may
/// use, preferring the providers it already runs on. A name the user's
/// favorites alone resolve picks that favorite, ahead of a newer model or
/// another provider, because it is the one the user means. The models the session
/// runs and binds always count. A miss among the models known offline asks
/// the providers for their live lists, which is where a server's own models,
/// such as a local `qwen3.8-27b`, come from. A qualified spec nothing lists is
/// taken as written when it resolves, so an unlisted model id stays reachable.
async fn resolve_task_model(ctx: &ToolContext, name: &str) -> Result<String, String> {
    let policy = Arc::clone(&ctx.model_policy);
    let in_use: Vec<String> = [ctx.model.spec(), ctx.chat_model.spec()]
        .into_iter()
        .chain(ModelPurpose::ALL.into_iter().filter_map(|purpose| {
            match model_registry::binding(purpose)? {
                Binding::Exact(spec) => Some(spec),
                Binding::Same(_) => None,
            }
        }))
        .filter(|spec| policy.allows(spec))
        .collect();
    let preferred = preferred_providers(&in_use);
    let name = name.trim().to_owned();
    let (result, mut candidates) = {
        let (policy, in_use, name) = (Arc::clone(&policy), in_use.clone(), name.clone());
        smol::unblock(move || offline_task_model(&policy, &in_use, &name)).await
    };
    match result {
        Err(ModelLookupError::NotFound { .. }) => {}
        result => return result.map_err(|error| error.to_string()),
    }
    candidates.extend(provider::listed_model_specs(&policy, LIVE_MODEL_LISTING_DEADLINE).await);
    candidates.sort_unstable();
    candidates.dedup();
    let result = find_model(&name, &candidates, &preferred);
    if matches!(result, Err(ModelLookupError::NotFound { .. }))
        && name.contains(MODEL_SPEC_SEPARATOR)
    {
        let written = name.clone();
        if smol::unblock(move || Model::from_spec_with_policy(&written, &policy).is_ok()).await {
            return Ok(name);
        }
    }
    result.map_err(|error| error.to_string())
}

/// What a task's model name resolves to among the models known without asking a provider:
/// the allowed favorites first, then every delegable and in-use spec. Blocking.
pub(crate) fn resolve_task_model_offline(
    policy: &ModelPolicy,
    in_use: &[String],
    name: &str,
) -> Result<String, String> {
    offline_task_model(policy, in_use, name.trim())
        .0
        .map_err(|error| error.to_string())
}

/// The offline lookup and, after a miss among the favorites, the candidates it searched, which
/// a live listing extends.
fn offline_task_model(
    policy: &ModelPolicy,
    in_use: &[String],
    name: &str,
) -> (Result<String, ModelLookupError>, Vec<String>) {
    let preferred = preferred_providers(in_use);
    if let Ok(favorite) = find_model(name, &favorites::allowed(policy), &preferred) {
        return (Ok(favorite), Vec::new());
    }
    let mut candidates = provider::delegable_model_specs(policy);
    candidates.extend(in_use.iter().cloned());
    candidates.sort_unstable();
    candidates.dedup();
    (find_model(name, &candidates, &preferred), candidates)
}

fn preferred_providers(in_use: &[String]) -> Vec<&str> {
    in_use
        .iter()
        .filter_map(|spec| Some(spec.split_once(MODEL_SPEC_SEPARATOR)?.0))
        .collect()
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

/// The UI names a task's model from `SubagentInfo`; a dispatching caller with
/// a live sink of its own, such as a Lua plugin, hears it as an annotation.
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
    // A hosted tool is absent from `tools`, yet its `code` module dispatches it. It goes in
    // ahead of the config intersect, which is what keeps a disabled tool disabled.
    let tool_filter = ToolFilter::Only(
        resolved
            .tools
            .as_array()
            .expect("tools are an array")
            .iter()
            .filter_map(|definition| definition.get("name")?.as_str().map(str::to_owned))
            .collect(),
    )
    .including(resolved.hosted)
    .intersect(&ToolFilter::from_config(&ctx.config, &resolved.model, &[]))
    .intersect(&resolved.tool_ceiling)
    .including(
        local_tools
            .iter()
            .filter(|(_, entry)| entry.is_required())
            .map(|(name, _)| name.clone()),
    )
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
    let mirror = SharedHistory::default();
    let history = History::restored(history_items)
        .map_err(|error| error.to_string())?
        .with_mirror(Arc::clone(&mirror));

    // Register a cancel trigger so the child token does not fire on drop and
    // kill the subagent at birth.
    let (child_trigger, child_cancel) = ctx.cancel.child();
    // Several sessions can share one task id, so keep the slot and retire only
    // ours on close instead of clearing the whole key.
    let cancel_slot = ctx
        .subagent_cancels
        .insert(resolved.task_id.clone(), child_trigger);

    info!(name = %name, model = %resolved.model.id, "subagent session opened");
    let memory_store = recall_store(ctx, &tool_filter);
    let context_publisher = ctx
        .context_publisher
        .as_ref()
        .map(|publisher| publisher.for_task(resolved.task_id.clone()));
    let live_history = context_publisher
        .as_ref()
        .map(|publisher| publisher.track_history(Arc::clone(&mirror)));

    Ok(Subagent {
        steering: Arc::new(Mutex::new(Steering::new(
            ctx.config.steering.resolve(&resolved.model.spec()),
        ))),
        report_ready: None,
        terminal_report: None,
        params: AgentParams {
            provider: resolved.provider,
            model: resolved.model,
            chat_provider: Arc::clone(&ctx.chat_provider),
            chat_model: Model::clone(&ctx.chat_model),
            config: ctx.config.clone(),
            tool_output_lines: caudra_config::ToolOutputLines::default(),
            tool_output_store: ctx.tool_output_store.clone(),
            permissions: Arc::clone(&ctx.permissions),
            session_id: ctx.session_id.clone(),
            cache_key: Some(CacheKey::task(ctx.session_id.as_ref(), &resolved.task_id)),
            workspace_session: ctx.workspace_session.clone(),
            remote_project_context: ctx.remote_project_context.clone(),
            host_cwd: ctx.host_cwd.clone(),
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
            changes: ctx.changes.clone(),
            prompt_slots: Arc::clone(&ctx.prompt_slots),
            prompt_profiles: Arc::clone(&ctx.prompt_profiles),
            default_task_prompt_profile_name: resolved.default_task_prompt_profile_name,
            active_prompt_profile_name: resolved.active_prompt_profile_name,
            subagent_cancels: Arc::new(CancelMap::new()),
            subagent_history: ctx.subagent_history.clone(),
            registry: Arc::clone(&ctx.registry),
            audience: resolved.audience,
            tool_filter,
            tool_ceiling: resolved.tool_ceiling,
            profile_tool_policy: Arc::clone(&resolved.profile_tool_policy),
            model_policy: Arc::clone(&ctx.model_policy),
            workflow: None,
            background: None,
            jobs: ctx.job_scope().map(|scope| {
                if ctx.audience == ToolAudience::MAIN
                    && matches!(scope.owner(), JobOwner::Child { .. })
                {
                    scope
                } else {
                    scope.child_scope(ids.parent_tool_use_id.clone())
                }
                .for_task(resolved.task_id.as_str())
            }),
            task_id: Some(resolved.task_id.clone()),
        },
        system: resolved.system,
        tools: resolved.tools,
        mode: resolved.mode,
        plan: ctx.session_plan(),
        environment: resolved.environment,
        mode_notice: resolved.mode_notice,
        thinking: resolved.thinking,
        fast,
        mcp: ctx
            .mcp
            .as_ref()
            .filter(|_| resolved.mcp_enabled)
            .map(|mcp| {
                mcp.clone()
                    .with_actor_policy(Arc::clone(&resolved.profile_tool_policy))
            }),
        code: CodeSession::new(history.as_slice()),
        history,
        mirror,
        live_history,
        history_lease: Some(resolved.history_lease),
        sub_event_tx,
        child_cancel,
        interrupt_source: Arc::new(steer_rx),
        answer_rx: Arc::new(AsyncMutex::new(answer_rx)),
        answer_tx: Some(answer_tx),
        steer_tx: Some(steer_tx),
        control: ConversationControl::default(),
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
        memory_store,
    })
}

/// The notes a child recalls from: the ones its memory tool would read,
/// opened only while recall is on and the child may use that tool. Never a
/// learner's.
fn recall_store(ctx: &ToolContext, tool_filter: &ToolFilter) -> Option<Arc<MemoryStore>> {
    let recalls = !ctx.config.no_memory_recall
        && tool_filter.matches(MEMORY_TOOL_NAME)
        && ctx
            .permissions
            .decisions()
            .is_some_and(|decisions| decisions.enabled(&DecisionFeature::MemoryRecall));
    if !recalls {
        return None;
    }
    memory::open_store(ctx, env::current_dir().ok().as_deref())
        .inspect_err(|error| warn!(%error, "subagent memory recall unavailable"))
        .ok()
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
    use caudra_config::decisions::{DecisionEndpoint, DecisionProtocol};
    use std::path::Path;
    use std::sync::atomic::AtomicUsize;

    use crate::AgentError;
    use caudra_config::decisions::DecisionsConfig;
    use caudra_decision::{DecisionEngine, DecisionError, DecisionRequest};
    use caudra_providers::provider::{BoxFuture, Provider};
    use caudra_providers::{ModelInfo, ProviderEvent, RequestOptions, StreamResponse};
    use caudra_storage::StateDir;
    use caudra_storage::decision_log::{DecisionLabel, DecisionLog};
    use caudra_storage::now_epoch;
    use serde_json::json;

    use super::*;
    use crate::agent::LoadedInstructions;
    use crate::agent::change_recording::ChangeRecorder;
    use crate::agent::change_recording::fixture::{FakeChanges, holder};
    use crate::agent::task_runner::{HostExtras, ModelResolver, WorkflowHostContext};
    use crate::code::output::{CodeCall, CodeCallStatus, CodeProgressEvent};
    use crate::context::{
        ContextInventory, ContextKey, ContextReadiness, ContextSnapshot, ContextStore,
        ContextUsage, ContextWindow,
    };
    use crate::decisions::NoteAnchor;
    use crate::permissions::{PermissionManager, PermissionMode, PermissionRequest};
    use crate::prompt::EFFICIENT_TOOLS_LABEL;
    use crate::tools::BATCH_TOOL_NAME;
    use crate::tools::DEADLINE_EXCEEDED;
    use crate::tools::native::plan::{NAME as PLAN_TOOL, PlanTool, WRITE_DENIED};
    use crate::tools::registry::Tool;
    use crate::tools::test_support::NamedMock;
    use crate::tools::{FILE_GREP_TOOL_NAME, TASK_TOOL_NAME};
    use crate::tools::{ToolEffect, audited_local_tool};
    use crate::{AgentEvent, CancelToken, EventSender, ToolDoneEvent, TurnCompleteEvent};
    use caudra_config::{ModelPolicy, PermissionsConfig, ToolKey};
    use caudra_providers::{Billing, ContentBlock, Message, Role};
    use caudra_storage::id::SessionRef;
    use caudra_storage::usage_ledger::LedgerPurpose;
    use tempfile::TempDir;
    use test_case::test_case;

    const RUN_ID: u64 = 7;
    const ROUTING_BASE_URL: &str = "http://127.0.0.1:1";
    const ROUTING_TASK: &str = "Rename a variable";
    const ROUTING_BASELINE: &str = "anthropic/claude-sonnet-4-6";
    const ROUTING_FAST: &str = "anthropic/claude-haiku-5-5";
    const ROUTING_BASELINE_NAME: &str = "sonnet 4.6";
    const ROUTING_PROFILE: &str = "security";
    const ROUTING_SECRET: &str = "do-not-send-this-value";
    const FAST_LEVELS: [f64; 4] = [0.6, 0.35, 0.05, 0.0];
    const CHOSEN_THINKING: &str = "high";
    const ROUTING_ROUNDS: usize = 2;
    const LONG_LABEL_WORDS: usize = 140;
    const LONG_PROMPT_WORDS: usize = 400;
    const EFFECT_TEST_TIMEOUT: Duration = Duration::from_secs(10);
    const DUPLICATE_CALL_ERROR: &str = "duplicates call ID";
    const SUBAGENT_ROOT: &str = "/work/project";
    const LOCAL_READER: &str = "local_reader";
    const LOCAL_RESULT: &str = "local result";
    const SONNET_NAME: &str = "sonnet";
    const NEWEST_SONNET: &str = "anthropic/claude-sonnet-5-5";
    const FAVORITE_SONNET: &str = "anthropic/claude-sonnet-4-5";
    const OTHER_PROVIDER_SONNET: &str = "copilot/claude-sonnet-4.6";

    struct RevisionChangingProvider {
        permissions: Arc<PermissionManager>,
        leave_yolo: bool,
    }

    impl Provider for RevisionChangingProvider {
        fn stream_message<'a>(
            &'a self,
            _model: &'a Model,
            _messages: &'a [Message],
            _system: &'a str,
            _tools: &'a JsonValue,
            _event_tx: &'a flume::Sender<ProviderEvent>,
            _opts: RequestOptions,
            _cache_key: Option<&'a CacheKey>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(std::future::pending())
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn adjust_model(&self, _model: &mut Model) {
            self.permissions
                .set_session_mode(Some(PermissionMode::Yolo));
            if self.leave_yolo {
                self.permissions.set_session_mode(Some(PermissionMode::Ask));
            }
        }
    }

    struct RoutingEngine {
        levels: [f64; 4],
        fail: bool,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl DecisionEngine for RoutingEngine {
        async fn decide(
            &self,
            request: &DecisionRequest,
            _deadline: Instant,
        ) -> Result<DecisionResponse, DecisionError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.fail {
                return Err(DecisionError::Timeout);
            }
            let score: f64 = self
                .levels
                .iter()
                .enumerate()
                .map(|(level, probability)| level as f64 * probability)
                .sum();
            Ok(serde_json::from_value(json!({
                "model": request.model,
                "answers": {
                    DIFFICULTY_QUESTION: {"type": "score", "score": score, "confidence": 1.0,
                        "legend": {"0": "", "1": "", "2": "", "3": ""},
                        "probabilities": {"0": self.levels[0], "1": self.levels[1],
                            "2": self.levels[2], "3": self.levels[3]}},
                }, "usage": {"input_tokens": 0, "output_tokens": 0}
            }))
            .unwrap())
        }
    }

    fn routing_task(label: &str) -> RoutingTask<'_> {
        RoutingTask {
            label,
            prompt: None,
            mode: SubagentTaskMode::Build,
            profile: None,
        }
    }

    #[test]
    fn routing_state_redacts_prompt_excerpt() {
        let prompt = format!(
            "Deploy with TOKEN={ROUTING_SECRET} and {}",
            "check the logs ".repeat(LONG_PROMPT_WORDS)
        );
        let state = routing_state(&RoutingTask {
            prompt: Some(&prompt),
            profile: Some(ROUTING_PROFILE),
            ..routing_task(ROUTING_TASK)
        });
        let described = &state["task"];
        assert_eq!(described["label"], ROUTING_TASK);
        assert_eq!(described["mode"], json!(SubagentTaskMode::Build));
        assert_eq!(described["profile"], ROUTING_PROFILE);
        let excerpt = described["prompt"].as_str().unwrap();
        assert!(!excerpt.contains(ROUTING_SECRET));
        assert!(excerpt.len() <= SUBAGENT_PROMPT_EXCERPT_BYTES);
    }

    #[test]
    fn routing_state_falls_back_to_label() {
        let label = "word ".repeat(LONG_LABEL_WORDS);
        let prompt = "step ".repeat(LONG_PROMPT_WORDS);
        let state = routing_state(&RoutingTask {
            prompt: Some(&prompt),
            ..routing_task(&label)
        });
        assert_eq!(state, json!({"task": {"label": label}}));
    }

    fn routing_decisions(
        directory: &TempDir,
        mode: FeatureMode,
        engine: RoutingEngine,
    ) -> Decisions {
        let mut config = DecisionsConfig {
            endpoint: Some(DecisionEndpoint::new(
                DecisionProtocol::TypeSafe.as_str(),
                DecisionProtocol::TypeSafe,
                ROUTING_BASE_URL.parse().unwrap(),
            )),
            log: true,
            ..DecisionsConfig::default()
        };
        config.features.subagent_routing = mode;
        Decisions::with_engine(
            config,
            &StateDir::from_path(directory.path().into()),
            engine,
        )
        .unwrap()
    }

    fn routing_context() -> ToolContext {
        crate::tools::test_support::stub_ctx_with_permissions(
            &AgentMode::Build,
            Arc::new(PermissionManager::new_nonpersistent(
                PermissionsConfig {
                    ..Default::default()
                },
                Path::new("/tmp").to_path_buf(),
                Arc::default(),
            )),
        )
    }

    #[test_case(FeatureMode::Enforce, FAST_LEVELS, false, Some(ModelPurpose::Fast); "routine_or_less_is_fast")]
    #[test_case(FeatureMode::Enforce, [0.0, 0.0, 0.05, 0.95], false, Some(ModelPurpose::Best); "open_ended_is_best")]
    #[test_case(FeatureMode::Enforce, [0.0, 0.1, 0.85, 0.05], false, None; "judgment_keeps_the_default")]
    #[test_case(FeatureMode::Enforce, [0.5, 0.0, 0.0, 0.5], false, None; "split_keeps_the_default")]
    #[test_case(FeatureMode::Shadow, FAST_LEVELS, false, None; "shadow")]
    #[test_case(FeatureMode::Enforce, FAST_LEVELS, true, None; "engine_error")]
    fn subagent_job_routes_on_difficulty_bounds(
        mode: FeatureMode,
        levels: [f64; 4],
        fail: bool,
        expected: Option<ModelPurpose>,
    ) {
        smol::block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let ctx = routing_context();
            ctx.permissions.set_session_mode(Some(PermissionMode::Ask));
            let calls = Arc::new(AtomicUsize::new(0));
            ctx.permissions.set_decisions(Some(routing_decisions(
                &directory,
                mode,
                RoutingEngine {
                    levels,
                    fail,
                    calls: Arc::clone(&calls),
                },
            )));
            assert_eq!(
                route_subagent(&ctx, false, None, None, routing_task(ROUTING_TASK))
                    .await
                    .map(|route| route.purpose),
                expected
            );
            assert_eq!(calls.load(Ordering::Relaxed), 1);
            assert!(!ctx.permissions.is_yolo());
        });
    }

    #[test_case(false, false, false, true, FeatureMode::Enforce; "yolo")]
    #[test_case(true, false, false, true, FeatureMode::Enforce; "yolo_continuation")]
    #[test_case(false, true, false, true, FeatureMode::Enforce; "yolo_explicit_job")]
    #[test_case(false, false, true, true, FeatureMode::Enforce; "yolo_profile_pin")]
    #[test_case(true, false, false, false, FeatureMode::Enforce; "continuation")]
    #[test_case(false, true, false, false, FeatureMode::Enforce; "explicit_job")]
    #[test_case(false, false, true, false, FeatureMode::Enforce; "profile_pin")]
    #[test_case(false, false, false, false, FeatureMode::Off; "off")]
    fn decision_subagent_routing_guards(
        continuation: bool,
        explicit: bool,
        pinned: bool,
        yolo: bool,
        mode: FeatureMode,
    ) {
        smol::block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let ctx = routing_context();
            ctx.permissions
                .set_session_mode(Some(PermissionMode::from(yolo)));
            let calls = Arc::new(AtomicUsize::new(0));
            let routed = !continuation && !explicit && !pinned && mode == FeatureMode::Enforce;
            ctx.permissions.set_decisions(Some(routing_decisions(
                &directory,
                mode,
                RoutingEngine {
                    levels: FAST_LEVELS,
                    fail: false,
                    calls: Arc::clone(&calls),
                },
            )));
            let profile = pinned.then(|| profile_pinning(Some(PINNED_MODEL_SPEC)));
            assert_eq!(
                route_subagent(
                    &ctx,
                    continuation,
                    profile.as_deref(),
                    explicit.then_some(ModelPurpose::Fast),
                    routing_task(ROUTING_TASK)
                )
                .await
                .map(|route| route.purpose),
                routed.then_some(ModelPurpose::Fast)
            );
            assert_eq!(calls.load(Ordering::Relaxed), usize::from(routed));
            assert_eq!(ctx.permissions.is_yolo(), yolo);
        });
    }
    #[test_case(false, None, FeatureMode::Enforce, false, false, DecisionEffect::Rerouted; "selected_different_model")]
    #[test_case(true, None, FeatureMode::Enforce, false, false, DecisionEffect::None; "selected_same_model")]
    #[test_case(false, Some(false), FeatureMode::Enforce, false, false, DecisionEffect::Rerouted; "yolo_during_resolution")]
    #[test_case(false, Some(true), FeatureMode::Enforce, false, false, DecisionEffect::Rerouted; "yolo_round_trip_during_resolution")]
    #[test_case(false, None, FeatureMode::Shadow, false, false, DecisionEffect::None; "shadow_route")]
    #[test_case(false, None, FeatureMode::Enforce, true, false, DecisionEffect::None; "decision_error")]
    #[test_case(false, None, FeatureMode::Enforce, false, true, DecisionEffect::None; "failed_creation")]
    fn decision_subagent_effect(
        same_model: bool,
        leave_yolo: Option<bool>,
        mode: FeatureMode,
        fail: bool,
        fail_creation: bool,
        expected: DecisionEffect,
    ) {
        smol::block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let mut ctx = routing_context();
            ctx.model = Arc::new(
                Model::from_spec(if same_model {
                    ROUTING_FAST
                } else {
                    ROUTING_BASELINE
                })
                .unwrap(),
            );
            ctx.chat_model = Arc::new(Model::from_spec(ROUTING_FAST).unwrap());
            if let Some(leave_yolo) = leave_yolo {
                ctx.chat_provider = Arc::new(RevisionChangingProvider {
                    permissions: Arc::clone(&ctx.permissions),
                    leave_yolo,
                });
            }
            ctx.permissions.set_decisions(Some(routing_decisions(
                &directory,
                mode,
                RoutingEngine {
                    levels: FAST_LEVELS,
                    fail,
                    calls: Arc::new(AtomicUsize::new(0)),
                },
            )));
            let mut options = task_options(None);
            options.name = ROUTING_TASK.into();
            if fail_creation {
                ctx.subagent_history
                    .reserve_with_spec(PARENT_ID, SubagentTaskSpec::default())
                    .unwrap()
                    .complete(vec![Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::tool_use(TOOL_ID, HOSTED_TOOL, json!({})); 2],
                        ..Message::default()
                    }]);
                options.task_id = TaskIdentity::Reserved(
                    ctx.subagent_history
                        .continue_task_with(PARENT_ID, SubagentTaskSpecCandidate::default())
                        .unwrap(),
                );
            }
            let opened = open_task(&ctx, options).await;
            if fail_creation {
                assert!(opened.err().unwrap().contains(DUPLICATE_CALL_ERROR));
            } else {
                let mut subagent = opened.unwrap();
                assert_eq!(
                    subagent.params.model.spec(),
                    if expected == DecisionEffect::Rerouted {
                        ROUTING_FAST.into()
                    } else {
                        ctx.model.spec()
                    }
                );
                subagent.close();
            }
            let log =
                DecisionLog::open_existing(&StateDir::from_path(directory.path().into())).unwrap();
            if fail_creation {
                assert!(log.is_none());
                return;
            }
            let mut log = log.unwrap();
            log.attach_label(
                1,
                &DecisionLabel {
                    expected: json!({DIFFICULTY_QUESTION: 0}),
                    source: ROUTING_TASK.into(),
                    timestamp: now_epoch(),
                    meta: JsonValue::Null,
                },
            )
            .unwrap();
            let deadline = Instant::now() + EFFECT_TEST_TIMEOUT;
            loop {
                let mut exported = Vec::new();
                assert_eq!(log.export_jsonl(&mut exported, None).unwrap(), 1);
                let row: JsonValue = serde_json::from_slice(&exported).unwrap();
                let actual: DecisionEffect =
                    serde_json::from_value(row["caudra"]["effect"].clone()).unwrap();
                if actual == expected || Instant::now() >= deadline {
                    assert_eq!(actual, expected);
                    break;
                }
                futures_lite::future::yield_now().await;
            }
        });
    }

    /// Routing would move this task to the Fast model; a model the caller
    /// chose is never second-guessed, so the engine is not even asked. The
    /// session's own model counts as usable even with no provider signed in.
    #[test_case(ROUTING_BASELINE ; "exact_spec")]
    #[test_case(ROUTING_BASELINE_NAME ; "short_name")]
    fn a_chosen_model_and_thinking_run_the_task_unrouted(name: &str) {
        smol::block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let mut ctx = routing_context();
            ctx.model = Arc::new(Model::from_spec(ROUTING_BASELINE).unwrap());
            ctx.chat_model = Arc::new(Model::from_spec(ROUTING_FAST).unwrap());
            let calls = Arc::new(AtomicUsize::new(0));
            ctx.permissions.set_decisions(Some(routing_decisions(
                &directory,
                FeatureMode::Enforce,
                RoutingEngine {
                    levels: FAST_LEVELS,
                    fail: false,
                    calls: Arc::clone(&calls),
                },
            )));
            let thinking = StoredThinking::Effort {
                level: CHOSEN_THINKING.into(),
            };
            let mut options = task_options(None);
            options.name = ROUTING_TASK.into();
            options.model = TaskModel {
                name: Some(name.into()),
                thinking: Some(thinking.clone()),
                ..TaskModel::default()
            };

            let mut subagent = open_task(&ctx, options).await.unwrap();

            assert_eq!(subagent.model_spec(), ROUTING_BASELINE);
            assert_eq!(subagent.thinking, ThinkingConfig::from(thinking.clone()));
            assert_eq!(calls.load(Ordering::Relaxed), 0);
            let spec = subagent
                .history_lease
                .as_ref()
                .and_then(SubagentHistoryLease::spec)
                .cloned()
                .expect("an opened task carries its specification");
            assert_eq!(spec.model.as_deref(), Some(ROUTING_BASELINE));
            assert_eq!(spec.thinking, Some(thinking));
            subagent.close();
        });
    }

    /// The session runs the newest Sonnet, which a bare `sonnet` names unless
    /// the user starred another one.
    #[test_case(&[], &[] => NEWEST_SONNET ; "without_favorites_the_newest_wins")]
    #[test_case(&[FAVORITE_SONNET], &[] => FAVORITE_SONNET ; "a_favorite_beats_a_newer_model")]
    #[test_case(&[OTHER_PROVIDER_SONNET, FAVORITE_SONNET], &[] => FAVORITE_SONNET ; "the_session_provider_breaks_a_tie_between_favorites")]
    #[test_case(&[FAVORITE_SONNET], &[FAVORITE_SONNET] => NEWEST_SONNET ; "an_excluded_favorite_falls_through")]
    fn a_task_model_name_prefers_a_favorite(favorites: &[&str], excluded: &[&str]) -> String {
        smol::block_on(async {
            let directory = TempDir::new().unwrap();
            let dir = StateDir::from_path(directory.path().to_path_buf());
            for spec in favorites.iter().rev() {
                model_registry::toggle_favorite_and_persist(spec, &dir).unwrap();
            }
            let mut ctx = crate::tools::test_support::stub_ctx(&AgentMode::Build);
            ctx.model = Arc::new(Model::from_spec(NEWEST_SONNET).unwrap());
            ctx.chat_model = Arc::clone(&ctx.model);
            let excluded: Vec<String> = excluded.iter().map(|spec| spec.to_string()).collect();
            ctx.model_policy = Arc::new(ModelPolicy::new(&[], &excluded).unwrap());

            resolve_task_model(&ctx, SONNET_NAME).await.unwrap()
        })
    }

    #[test_case(FeatureMode::Enforce, false, false, &[NoteOutcome::Acted, NoteOutcome::Acted], ROUTED; "enforce_routes")]
    #[test_case(FeatureMode::Shadow, false, false, &[NoteOutcome::Shadow, NoteOutcome::Shadow], ROUTED; "shadow_would_route")]
    #[test_case(FeatureMode::Enforce, true, false, &[], ROUTED; "same_model_says_nothing")]
    #[test_case(FeatureMode::Enforce, false, true, &[NoteOutcome::FellBack], ROUTING_FALLBACK; "failure_is_noted_once")]
    fn decision_subagent_routing_notes(
        mode: FeatureMode,
        same_model: bool,
        fail: bool,
        expected: &[NoteOutcome],
        phrase: &str,
    ) {
        smol::block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let decisions = routing_decisions(
                &directory,
                mode.clone(),
                RoutingEngine {
                    levels: FAST_LEVELS,
                    fail,
                    calls: Arc::new(AtomicUsize::new(0)),
                },
            );
            let (sender, events) = flume::unbounded();
            for _ in 0..ROUTING_ROUNDS {
                let mut ctx = routing_context();
                ctx.model = Arc::new(
                    Model::from_spec(if same_model {
                        ROUTING_FAST
                    } else {
                        ROUTING_BASELINE
                    })
                    .unwrap(),
                );
                ctx.chat_model = Arc::new(Model::from_spec(ROUTING_FAST).unwrap());
                ctx.event_tx = EventSender::new(sender.clone(), RUN_ID);
                ctx.tool_use_id = Some(TOOL_ID.into());
                ctx.permissions.set_decisions(Some(decisions.clone()));
                let mut options = task_options(None);
                options.name = ROUTING_TASK.into();
                open_task(&ctx, options).await.unwrap().close();
            }
            let notes: Vec<_> = events
                .try_iter()
                .filter_map(|envelope| match envelope.event {
                    AgentEvent::Decision(note) => Some(note),
                    _ => None,
                })
                .collect();
            assert_eq!(
                notes.iter().map(|note| note.outcome).collect::<Vec<_>>(),
                expected
            );
            for note in &notes {
                assert_eq!(note.feature, DecisionFeature::SubagentRouting);
                assert_eq!(note.mode, mode);
                assert_eq!(note.anchor, NoteAnchor::Tool(TOOL_ID.into()));
                match note.outcome {
                    NoteOutcome::FellBack => assert!(note.summary.ends_with(phrase)),
                    NoteOutcome::Acted | NoteOutcome::Shadow => assert_eq!(
                        note.summary,
                        format!(
                            "{}{phrase} {ROUTING_FAST} instead of {ROUTING_BASELINE} (difficulty {ROUTINE_BAND}, {:.2})",
                            note.outcome.lead(),
                            FAST_LEVELS[0] + FAST_LEVELS[1]
                        )
                    ),
                }
            }
        });
    }

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
    const SECOND_SUBAGENT_ID: &str = "collision-2";
    const INHERITED_PROFILE: &str = "parent-default";
    const SUBAGENT_SYSTEM: &str = "system";
    const REMOTE_CWD: &str = "remote/project";
    const REMOTE_PLATFORM: &str = "remote-os";
    const PLAN_PATH: &str = "plan.md";
    const BOUND_PLAN: &str = "bound.md";
    const PLAN_CONTENT: &str = "# Plan";
    const ENVIRONMENT_MISSING: &str = "a task must be told its environment";
    const HOSTED_TOOL: &str = "execution_environment";
    const MAIN_ONLY_TOOL: &str = "main_only_mock";
    const PUBLISHER_MISSING: &str = "subagent must inherit a task-scoped context publisher";
    const LIVE_HISTORY_MISSING: &str = "an open subagent must list its live history";
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
            mode: AgentMode::Build,
            audience: ToolAudience::GENERAL_SUB,
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
            model: Default::default(),
            routing_prompt: None,
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

    #[test_case(false, false, true; "independent_worker")]
    #[test_case(true, false, false; "hard_ceiling")]
    #[test_case(false, true, false; "generic_cannot_widen")]
    fn profile_delegation_separates_actor_mask_from_ceiling(
        blocked: bool,
        generic: bool,
        allowed: bool,
    ) {
        smol::block_on(async {
            let mut ctx = crate::tools::test_support::stub_ctx(&AgentMode::Build);
            ctx.registry
                .register_audited(
                    Arc::new(NamedMock::new(HOSTED_TOOL, ToolAudience::all())),
                    NamedMock::source(),
                    crate::tools::ToolEffect::ReadOnly,
                )
                .unwrap();
            ctx.profile_tool_policy =
                Arc::new(serde_json::from_value(json!({"default":"disabled"})).unwrap());
            ctx.tool_filter = ToolFilter::Only(Vec::new());
            if blocked {
                ctx.tool_ceiling = ToolFilter::Only(Vec::new());
            }
            let mut child = if generic {
                let mut options = generic_options();
                options.tools = json!([{"name":HOSTED_TOOL,"input_schema":{"type":"object"}}]);
                open_generic(&ctx, options).await.unwrap()
            } else {
                open_task(&ctx, task_options(None)).await.unwrap()
            };
            assert_eq!(child.params.tool_filter.matches(HOSTED_TOOL), allowed);
            child.close();
        });
    }

    #[test_case(false; "local_binding_available")]
    #[test_case(true; "local_binding_ceiling")]
    fn task_local_bindings_use_the_worker_ceiling(blocked: bool) {
        smol::block_on(async {
            let mut ctx = crate::tools::test_support::stub_ctx(&AgentMode::Build);
            ctx.profile_tool_policy =
                Arc::new(serde_json::from_value(json!({"default":"disabled"})).unwrap());
            ctx.tool_filter = ToolFilter::Only(Vec::new());
            if blocked {
                ctx.tool_ceiling = ToolFilter::Only(Vec::new());
            }
            let mut options = task_options(None);
            options.local_definitions =
                vec![json!({"name":LOCAL_READER,"input_schema":{"type":"object"}})];
            options.local_tools = Arc::new(
                [(
                    LOCAL_READER.into(),
                    audited_local_tool(ToolEffect::ReadOnly, |_, _| {
                        Box::pin(async { Ok(LOCAL_RESULT.into()) })
                    }),
                )]
                .into(),
            );
            let mut child = open_task(&ctx, options).await.unwrap();
            assert_eq!(child.params.tool_filter.matches(LOCAL_READER), !blocked);
            child.close();
        });
    }

    #[test_case(false, false; "cancelled_without_database")]
    #[test_case(false, true; "cancelled_with_database")]
    #[test_case(true, false; "expired_without_database")]
    #[test_case(true, true; "expired_with_database")]
    fn interrupted_identity_reservation_does_not_open_a_child(expired: bool, stored: bool) {
        smol::block_on(async {
            let temp = TempDir::new().unwrap();
            let mut ctx = crate::tools::test_support::stub_ctx(&AgentMode::Build);
            if stored {
                ctx.session_id = Some(SessionRef::generate());
                ctx.tool_output_store = Some(Arc::new(ToolOutputStore::new(StateDir::from_path(
                    temp.path().to_owned(),
                ))));
            }
            let expected = if expired {
                ctx.deadline = Deadline::At(Instant::now());
                DEADLINE_EXCEEDED
            } else {
                let (trigger, cancel) = CancelToken::new();
                ctx.cancel = cancel;
                trigger.cancel();
                CANCELLED
            };
            let error = open_task(&ctx, task_options(None)).await.err().unwrap();
            assert_eq!(error, expected);
            assert_eq!(ctx.subagent_history.active_count(), 0);
            assert!(ctx.subagent_history.snapshot().records().is_empty());
        });
    }

    #[test_case(false; "active")]
    #[test_case(true; "completed")]
    fn repeated_task_descriptions_allocate_fresh_ids_and_continuations_keep_them(completed: bool) {
        smol::block_on(async {
            let ctx = crate::tools::test_support::stub_ctx(&AgentMode::Build);
            let mut first = open_task(&ctx, task_options(None)).await.unwrap();
            assert_eq!(first.id(), COLLIDING_SUBAGENT_NAME);
            if completed {
                first.close();
            }
            let mut second = open_task(&ctx, task_options(None)).await.unwrap();
            assert_eq!(second.id(), SECOND_SUBAGENT_ID);
            first.close();
            second.close();
            let mut options = task_options(None);
            options.task_id = TaskIdentity::Continue(COLLIDING_SUBAGENT_NAME.into());
            options.name = SECOND_SUBAGENT_ID.into();
            let mut continued = open_task(&ctx, options).await.unwrap();
            assert_eq!(continued.id(), COLLIDING_SUBAGENT_NAME);
            continued.close();
        });
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
    fn a_subagent_records_for_the_root_session() {
        smol::block_on(async {
            let mut ctx = crate::tools::test_support::stub_ctx(&AgentMode::Build);
            ctx.changes = Some(Arc::new(FakeChanges::default()).recorder(Path::new(SUBAGENT_ROOT)));

            let mut subagent = open_task(&ctx, task_options(None)).await.unwrap();

            assert_eq!(
                subagent.params.changes.as_ref().map(ChangeRecorder::holder),
                Some(&holder())
            );
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
                    model: Default::default(),
                    routing_prompt: None,
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
        crate::prompt::profile::PromptProfileCatalog::discover_with(Some(dir.path()), false)
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
            subagent_model_binding(None, Some(&profile), asked.as_ref()),
            Some(&Binding::Exact(PINNED_MODEL_SPEC.into()))
        );
    }

    /// The pin is a default and the job a hint; a model chosen for this one
    /// task is what was asked for now.
    #[test_case(true, None ; "over_a_pin")]
    #[test_case(true, Some(ModelPurpose::Fast) ; "over_a_pin_and_a_job")]
    #[test_case(false, Some(ModelPurpose::Best) ; "over_a_job")]
    fn a_chosen_model_outranks_the_pin_and_the_job(pinned: bool, model_job: Option<ModelPurpose>) {
        let profile = pinned.then(|| profile_pinning(Some(PINNED_MODEL_SPEC)));
        let asked = model_job.map(Binding::Same);
        let chosen = Binding::Exact(PLAN_MODEL_SPEC.into());

        assert_eq!(
            subagent_model_binding(Some(&chosen), profile.as_deref(), asked.as_ref()),
            Some(&chosen)
        );
    }

    #[test_case(true ; "unpinned_profile")]
    #[test_case(false ; "no_profile")]
    fn the_asked_for_job_is_used_without_a_pin(unpinned_profile: bool) {
        let profile = unpinned_profile.then(|| profile_pinning(None));
        let asked = Some(Binding::Same(ModelPurpose::Fast));

        assert_eq!(
            subagent_model_binding(None, profile.as_deref(), asked.as_ref()),
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

    /// A `code` module dispatches a hosted tool the child's request never declares, so the
    /// filter dispatch judges it by has to know the name too. What the child's audience never
    /// offered stays out.
    #[test_case(HOSTED_TOOL, true ; "a_hosted_builtin_stays_callable")]
    #[test_case(MAIN_ONLY_TOOL, false ; "a_tool_outside_the_audience_does_not")]
    fn a_task_may_call_what_its_code_modules_host(tool: &str, callable: bool) {
        smol::block_on(async {
            let ctx = crate::tools::test_support::stub_ctx(&AgentMode::Build);
            ctx.registry
                .register_many([
                    (
                        Arc::new(NamedMock::new(HOSTED_TOOL, ToolAudience::all())) as Arc<dyn Tool>,
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

    /// The prompt is filtered by the child's audience like its tools are, so it
    /// never recommends `task`, which only the main agent may call.
    #[test]
    fn a_task_is_only_recommended_tools_it_can_call() {
        smol::block_on(async {
            let ctx = crate::tools::test_support::stub_ctx(&AgentMode::Build);
            ctx.registry
                .register_many([
                    (
                        Arc::new(NamedMock::new(FILE_GREP_TOOL_NAME, ToolAudience::all()))
                            as Arc<dyn Tool>,
                        NamedMock::source(),
                    ),
                    (
                        Arc::new(NamedMock::new(TASK_TOOL_NAME, ToolAudience::MAIN))
                            as Arc<dyn Tool>,
                        NamedMock::source(),
                    ),
                ])
                .unwrap();

            let mut subagent = open_task(&ctx, task_options(Some(SubagentTaskMode::Build)))
                .await
                .unwrap();

            let line = format!("{EFFICIENT_TOOLS_LABEL} `{FILE_GREP_TOOL_NAME}`.");
            assert!(subagent.system.contains(&line), "{}", subagent.system);
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

    /// A child reads the plan its parent works on: the draft while the parent
    /// plans, the session plan once it builds. A continued task inherits it
    /// again, and the child's own context refuses a write before any
    /// permission or storage work.
    #[test_case(AgentMode::Plan(PLAN_PATH.into()), false; "plan_parent_task")]
    #[test_case(AgentMode::Build, false; "build_parent_task")]
    #[test_case(AgentMode::Build, true; "build_parent_generic")]
    fn a_child_inherits_the_parents_session_plan(parent: AgentMode, generic: bool) {
        smol::block_on(async {
            let mut ctx = crate::tools::test_support::stub_ctx(&parent);
            ctx.plan = Some(PlanTarget::Local(BOUND_PLAN.into()));
            let expected = ctx.session_plan();
            let mut child = if generic {
                open_generic(&ctx, generic_options()).await.unwrap()
            } else {
                open_task(&ctx, task_options(None)).await.unwrap()
            };
            assert_eq!(child.plan, expected);

            let child_ctx = ToolContext {
                mode: child.mode.clone(),
                plan: child.plan.clone(),
                audience: child.params.audience,
                ..ctx.clone()
            };
            let write = PlanTool
                .parse(&json!({"action": "write", "content": PLAN_CONTENT}))
                .unwrap();
            let refusal = write.preflight(&child_ctx).await.unwrap_err();
            assert_eq!(refusal.message, WRITE_DENIED);
            child.close();

            if !generic {
                let mut options = task_options(None);
                options.task_id = TaskIdentity::Continue(child.id().to_owned());
                let mut continued = open_task(&ctx, options).await.unwrap();
                assert_eq!(continued.plan, expected);
                continued.close();
            }
        });
    }

    /// A task is offered `plan` exactly when it inherits a session plan, so
    /// its own `/tools` view follows the parent's plan rather than its mode.
    #[test_case(AgentMode::Plan(PLAN_PATH.into()), false, true; "plan_parent")]
    #[test_case(AgentMode::Build, true, true; "bound_build_parent")]
    #[test_case(AgentMode::Build, false, false; "unbound_build_parent")]
    fn a_task_is_offered_the_plan_it_inherits(parent: AgentMode, bound: bool, offered: bool) {
        smol::block_on(async {
            let mut ctx = crate::tools::test_support::stub_ctx(&parent);
            ctx.plan = bound.then(|| PlanTarget::Local(BOUND_PLAN.into()));
            ctx.registry
                .register_audited(
                    Arc::new(PlanTool),
                    NamedMock::source(),
                    ToolEffect::Mutating,
                )
                .unwrap();
            let mut child = open_task(&ctx, task_options(None)).await.unwrap();
            let declared = child
                .tools
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == PLAN_TOOL);
            assert_eq!(declared, offered);
            child.close();
        });
    }

    /// A workflow host is built from session parameters, which carry no plan,
    /// so what it launches inherits none even when its parent has one.
    #[test]
    fn a_workflow_agent_inherits_no_session_plan() {
        smol::block_on(async {
            let mut ctx = crate::tools::test_support::stub_ctx(&AgentMode::Build);
            ctx.plan = Some(PlanTarget::Local(BOUND_PLAN.into()));
            let mut session = open_task(&ctx, task_options(None)).await.unwrap();
            let model: ModelResolver = Arc::new({
                let provider = Arc::clone(&ctx.provider);
                let model = Arc::clone(&ctx.model);
                move || (Arc::clone(&provider), Arc::clone(&model))
            });
            let host = WorkflowHostContext::from_agent_params(
                &session.params,
                HostExtras {
                    mcp: None,
                    loaded_instructions: LoadedInstructions::default(),
                    user_response_rx: None,
                },
                model,
                Arc::new(|| AgentMode::Build),
                Arc::new(CancelMap::new()),
            );
            session.close();
            let (event_tx, _event_rx) = flume::unbounded();
            let launched = host
                .tool_context(CancelToken::none(), EventSender::new(event_tx, 0), TOOL_ID)
                .await
                .unwrap();

            assert_eq!(launched.session_plan(), None);
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

    #[test]
    fn an_open_subagent_lists_its_history_until_the_store_holds_it() {
        smol::block_on(async {
            let store = ContextStore::new();
            let mut ctx =
                crate::tools::test_support::stub_ctx_with(&AgentMode::Build, None, Some(PARENT_ID));
            ctx.context_publisher = Some(store.publisher(ContextKey::Main));
            let mut subagent = open_generic(&ctx, generic_options()).await.unwrap();
            let key = ContextKey::task(subagent.id());

            let listed = store.live_history(&key).expect(LIVE_HISTORY_MISSING);
            assert!(Arc::ptr_eq(&listed, &subagent.mirror()));
            subagent.close();

            assert!(store.live_history(&key).is_none());
            assert!(
                ctx.subagent_history
                    .snapshot()
                    .records()
                    .contains_key(subagent.id())
            );
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
                    model: Default::default(),
                    routing_prompt: None,
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
            task: None,
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
            control: None,
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
            interrupted: false,
            cost: Some(cost),
            billing: Billing::Api,
            account: None,
            context_size: None,
            context_window: 0,
        }))
    }

    #[test_case(false; "completed_run")]
    #[test_case(true; "failed_run_flushes_completed_responses")]
    fn relay_session_events_reports_live_usage_and_done_total(failed: bool) {
        let (sub_tx, sub_rx) = flume::unbounded();
        let (parent_raw_tx, parent_rx) = flume::unbounded();
        let subagent_info = parent_info();
        let (usage_tx, usage_rx) = flume::unbounded();
        let (live_tx, live_rx) = flume::unbounded();

        for event in [
            turn(tokens(100, 20), 0.25),
            turn(tokens(50, 10), 0.5),
            AgentEvent::SubagentHistory {
                task_id: "nested-task".into(),
                parent_tool_use_id: "nested-call".into(),
                root_tool_use_id: PARENT_ID.into(),
                name: "nested".into(),
                model: "provider/model".into(),
                messages: Vec::new(),
                spec: None,
            },
            if failed {
                AgentEvent::Error {
                    message: IGNORED_ERROR.into(),
                }
            } else {
                AgentEvent::Done {
                    usage: DONE_USAGE,
                    num_turns: 2,
                    reason: DoneReason::EndTurn,
                }
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
        assert_eq!(
            usage_rx.try_recv(),
            Ok(if failed {
                TokenUsage::default()
            } else {
                DONE_USAGE
            })
        );
        assert!(usage_rx.try_recv().is_err());

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
            refused: false,
            accounting: Default::default(),
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

    const SNIPPET: &str = "rows = history.search('flaky')\nrows";
    const SNIPPET_SUMMARY: &str = "rows = history.search('flaky') · 2 lines";
    const HOST_CALL: &str = "history.search";
    const ONE_CALL: &str = "1 call";

    fn code_start(code: &str) -> AgentEvent {
        let AgentEvent::ToolStart(mut start) = tool_start(CODE_TOOL_NAME) else {
            unreachable!("tool_start builds a start event")
        };
        start.summary = "2 lines".to_owned();
        start.input = Some(ToolInput::Code {
            language: "python".to_owned(),
            code: code.to_owned(),
        });
        AgentEvent::ToolStart(start)
    }

    fn code_progress(id: &str, index: usize, status: CodeCallStatus) -> AgentEvent {
        AgentEvent::CodeProgress(Box::new(CodeProgressEvent {
            id: id.to_owned(),
            index,
            call: CodeCall {
                server: "history".to_owned(),
                tool: "search".to_owned(),
                called_as: HOST_CALL.to_owned(),
                input: json!({ "query": format!("q{index}") }),
                status,
                ..CodeCall::default()
            },
        }))
    }

    /// A published `code` row: its text, then each call row written out with its status.
    type CodeRow = (Option<String>, Vec<(String, BatchToolStatus)>);

    /// Each published `code` row as its text and its call rows written out.
    fn code_rows(events: Vec<AgentEvent>) -> Vec<CodeRow> {
        let mut relay = ProgressRelay::new();
        events
            .into_iter()
            .filter_map(|event| {
                let activity = relay.activity(&event)?;
                relay.last = Some(activity.clone());
                let calls = activity
                    .children()
                    .iter()
                    .inspect(|child| assert!(child.host_call))
                    .map(|child| (format!("{}{}", child.tool, child.summary), child.status))
                    .collect();
                Some((activity.detail().map(str::to_owned), calls))
            })
            .collect()
    }

    fn written(index: usize) -> String {
        format!("{HOST_CALL}(query=\"q{index}\")")
    }

    /// The reported bug: a subagent's `code` row read `code 1 line` whatever it did. Before any
    /// host call it reads as its snippet, then as its calls, each written as the snippet made it.
    #[test]
    fn a_code_row_reads_as_its_snippet_then_as_its_host_calls() {
        let rows = code_rows(vec![
            code_start(SNIPPET),
            code_progress(TOOL_ID, 0, CodeCallStatus::Running),
            code_progress(TOOL_ID, 0, CodeCallStatus::Success),
            code_progress(TOOL_ID, 1, CodeCallStatus::Refused),
        ]);

        assert_eq!(
            rows,
            [
                (Some(SNIPPET_SUMMARY.to_owned()), vec![]),
                (
                    Some(ONE_CALL.to_owned()),
                    vec![(written(0), BatchToolStatus::Running)]
                ),
                (
                    Some(ONE_CALL.to_owned()),
                    vec![(written(0), BatchToolStatus::Success)]
                ),
                (
                    Some(counted(2, CALL_NOUN)),
                    vec![
                        (written(0), BatchToolStatus::Success),
                        (written(1), BatchToolStatus::Error)
                    ]
                ),
            ]
        );
    }

    /// A loop of calls keeps the row bounded: it counts them all and shows the newest, and a
    /// late report for a call that scrolled out changes nothing.
    #[test]
    fn a_code_row_shows_its_newest_calls_and_counts_them_all() {
        let total = MAX_CODE_CALL_ROWS + 2;
        let mut relay = ProgressRelay::new();
        relay.last = relay.activity(&code_start(SNIPPET));
        for index in 0..total {
            relay.last = relay.activity(&code_progress(TOOL_ID, index, CodeCallStatus::Running));
        }
        let activity = relay.last.clone().expect("a row");

        assert_eq!(activity.detail(), Some(counted(total, CALL_NOUN).as_str()));
        let shown: Vec<String> = activity
            .children()
            .iter()
            .map(|child| format!("{}{}", child.tool, child.summary))
            .collect();
        let newest: Vec<String> = (total - MAX_CODE_CALL_ROWS..total).map(written).collect();
        assert_eq!(shown, newest);
        assert!(
            relay
                .activity(&code_progress(TOOL_ID, 0, CodeCallStatus::Success))
                .is_none()
        );
    }

    #[test]
    fn another_runs_host_calls_change_nothing() {
        let mut relay = ProgressRelay::new();
        relay.last = relay.activity(&code_start(SNIPPET));

        assert!(
            relay
                .activity(&code_progress(NEXT_TOOL_ID, 0, CodeCallStatus::Running))
                .is_none()
        );
    }

    /// A host call waiting on the user says so on its own row, and reads as running again once
    /// the gateway republishes it.
    #[test]
    fn a_host_call_awaiting_approval_marks_its_own_row() {
        let rows = code_rows(vec![
            code_start(SNIPPET),
            code_progress(TOOL_ID, 0, CodeCallStatus::Running),
            permission_request(&batch::child_tool_use_id(Some(TOOL_ID), 0)),
            code_progress(TOOL_ID, 0, CodeCallStatus::Running),
        ]);

        let statuses: Vec<BatchToolStatus> =
            rows.iter().skip(1).map(|(_, calls)| calls[0].1).collect();
        assert_eq!(
            statuses,
            [
                BatchToolStatus::Running,
                BatchToolStatus::AwaitingApproval,
                BatchToolStatus::Running
            ]
        );
    }

    fn permission_request(id: &str) -> AgentEvent {
        AgentEvent::PermissionRequest(Box::new(PermissionRequest::from_legacy(
            id.to_owned(),
            ToolKey::native(PENDING_TOOL),
            vec![INPUT_PREVIEW.to_owned()],
            json!({ "command": INPUT_PREVIEW }),
            Path::new(REMOTE_CWD),
            false,
        )))
    }

    /// The fragment that closes the arguments, revealing nothing new.
    fn closing_input_delta() -> AgentEvent {
        let mut event = tool_input_delta();
        if let AgentEvent::ToolInputDelta {
            preview, complete, ..
        } = &mut event
        {
            *preview = None;
            *complete = true;
        }
        event
    }

    fn running_row() -> SubagentActivity {
        SubagentActivity::tool(Arc::from(PENDING_TOOL), INPUT_PREVIEW).with_call_id(TOOL_ID)
    }

    /// The call that asked keeps its row, so the reader sees which command
    /// waits on them rather than only that something does.
    #[test]
    fn approval_restages_the_calls_own_row() {
        let mut relay = ProgressRelay::new();
        relay.last = relay.activity(&tool_input_delta());

        assert_eq!(
            relay.activity(&permission_request(TOOL_ID)),
            Some(running_row().with_stage(Some(CallStage::AwaitingApproval)))
        );
    }

    /// The regression: a child's request used to swap the batch row for the
    /// bare phase, and every later report for the batch then patched a row
    /// no longer on show, so the phase outlived the wait.
    #[test]
    fn a_batch_child_awaiting_approval_keeps_the_batch_row() {
        let published = rosters(vec![
            batch_start(2),
            permission_request(&batch::child_tool_use_id(Some(TOOL_ID), 1)),
            batch_progress(TOOL_ID, 1, BatchToolStatus::Running),
        ]);

        let row = |status| (CHILD_TOOL.to_owned(), status);
        assert_eq!(
            published,
            [
                vec![row(BatchToolStatus::Pending), row(BatchToolStatus::Pending)],
                vec![
                    row(BatchToolStatus::Pending),
                    row(BatchToolStatus::AwaitingApproval)
                ],
                vec![row(BatchToolStatus::Pending), row(BatchToolStatus::Running)],
            ]
        );
    }

    /// The closing fragment usually reveals nothing, and the row must stop
    /// reading as written without losing the header it already earned.
    #[test]
    fn completion_without_a_preview_keeps_the_summary() {
        let mut relay = ProgressRelay::new();
        relay.last = relay.activity(&tool_input_delta());

        assert_eq!(relay.activity(&closing_input_delta()), Some(running_row()));
    }

    #[test_case(Vec::new(), TOOL_ID ; "nothing_on_the_row_yet")]
    #[test_case(vec![tool_start(CHILD_TOOL)], NEXT_TOOL_ID ; "another_calls_row")]
    #[test_case(
        vec![batch_start(2)],
        &batch::child_tool_use_id(Some(TOOL_ID), 9)
        ; "a_child_the_roster_does_not_have"
    )]
    fn an_unmatched_request_falls_back_to_the_phase(before: Vec<AgentEvent>, id: &str) {
        let mut relay = ProgressRelay::new();
        for event in &before {
            relay.last = relay.activity(event);
        }

        assert_eq!(
            relay.activity(&permission_request(id)),
            Some(SubagentActivity::AwaitingPermission)
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
            todos: None,
            patch: None,
            delegations: Vec::new(),
            complete: false,
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
