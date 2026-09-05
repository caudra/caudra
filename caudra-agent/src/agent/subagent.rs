//! Subagent sessions: a child agent with its own history, tools, and cancel
//! scope, whose events are relayed to the parent stamped with its identity.
//!
//! Both the native `task` tool and `caudra.agent.session` open sessions
//! through here. The task path resolves its model, prompt, and tools from a
//! profile; the generic path takes them from the caller.

use std::sync::{Arc, OnceLock};
use std::time::Instant;

use async_lock::Mutex as AsyncMutex;
use serde_json::Value as JsonValue;
use tracing::info;

use caudra_providers::model::Model;
use caudra_providers::provider;
use caudra_providers::{
    ContentBlock, HistoryItem, Message, Role, ThinkingConfig, TokenUsage, add_cost, expand_message,
};
use caudra_storage::id::CaudraId;

use crate::cancel::{CancelMap, CancelSlot};
use crate::prompt::PromptId;
use crate::tools::registry::ToolRegistry;
use crate::tools::{
    DescriptionContext, FileReadTracker, LocalTools, ToolAudience, ToolContext, ToolFilter,
    ToolLive,
};
use crate::{
    Agent, AgentEvent, AgentInput, AgentMode, AgentParams, AgentRunParams, DoneReason,
    EMPTY_RESPONSE_MARKER, Envelope, EventSender, History, InterruptSource, McpSession,
    SteeringQueue, SteeringQueueReceiver, SubagentActivity, SubagentHistoryError,
    SubagentHistoryLease, SubagentInfo, SubagentProgress, SubagentTaskMode, SubagentTaskSpec,
    SubagentTaskSpecCandidate, reasoning_summary, steering_queue,
};

pub const STRUCTURED_OUTPUT_TOOL: &str = "structured_output";
pub const BUILTIN_TASK_PROFILE_DESCRIPTION: &str = "Caudra\'s built-in task prompt";
pub const SESSION_CLOSED: &str = "session closed";
pub const CANCELLED: &str = "cancelled";
const DEFAULT_SESSION_AUDIENCE: ToolAudience = ToolAudience::GENERAL_SUB;
/// A thought\'s title is its first bold line. Past this the block is into
/// prose and will never resolve one, so it stops being accumulated and a
/// long reasoning stream cannot be buffered a second time for nothing.
const THOUGHT_TITLE_SCAN_LIMIT: usize = 200;

/// Forwards subagent events to the parent, stamped with the subagent identity.
/// Usage takes two paths: live on the tool header while the run goes on (last
/// turn's tokens plus the run's summed cost), and one total per run on
/// `usage_tx`, which `prompt` waits for. Progress takes the same two paths, so
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
            AgentEvent::Error { .. }
            | AgentEvent::ToolOutput { .. }
            | AgentEvent::ToolInputDelta { .. }
            | AgentEvent::ToolPending { .. } => continue,
            _ => {}
        }
        envelope.subagent = subagent_info.get().cloned();
        let _ = parent_tx.send_envelope(envelope);
    }
}

/// Keeps the running digest one subagent reports to its parent.
struct ProgressRelay {
    started: Instant,
    tools: u32,
    thought: String,
    thought_title: Option<String>,
    last: Option<SubagentActivity>,
}

impl ProgressRelay {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            tools: 0,
            thought: String::new(),
            thought_title: None,
            last: None,
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
            // shows, and it arrives without a tool name to match on.
            AgentEvent::ToolHeaderSnapshot { snapshot, .. } => match &self.last {
                Some(SubagentActivity::Tool { name, .. }) => Some(SubagentActivity::tool(
                    Arc::clone(name),
                    &snapshot.first_line_text(),
                )),
                _ => None,
            },
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
        let counted = matches!(envelope.event, AgentEvent::ToolStart(_));
        self.tools += u32::from(counted);
        let Some(activity) = self.activity(&envelope.event) else {
            return;
        };
        // Two identical calls in a row leave the activity untouched, and the
        // count is the only thing that moved.
        if !counted && self.last.as_ref() == Some(&activity) {
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
        });
    }
}

pub struct Subagent {
    params: AgentParams,
    system: String,
    tools: JsonValue,
    mode: AgentMode,
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

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    pub async fn prompt(&mut self, message: String) -> Result<PromptResult, PromptFailure> {
        if self.closed {
            return Err(PromptFailure {
                error: SESSION_CLOSED.to_owned(),
                partial: None,
            });
        }
        // The first prompt is what names the subagent in the parent's UI, so
        // the identity is published here rather than at open time.
        if self.subagent_info.get().is_none() {
            let _ = self.subagent_info.set(SubagentInfo {
                parent_tool_use_id: self.parent_tool_use_id.clone(),
                task_id: self.task_id.clone(),
                name: self.name.clone(),
                prompt: Some(message.clone()),
                model: Some(self.params.model.spec()),
                answer_tx: self.answer_tx.take(),
                steer_tx: self.steer_tx.take(),
            });
        }

        let history_len = self.history.len();
        let interrupt_source: Arc<dyn InterruptSource> = self.interrupt_source.clone();
        let mut agent = Agent::new(
            self.params.clone(),
            AgentRunParams {
                history: &mut self.history,
                system: self.system.clone(),
                event_tx: self.sub_event_tx.clone(),
                tools: self.tools.clone(),
            },
        )
        .with_user_response_rx(Arc::clone(&self.answer_rx))
        .with_interrupt_source(interrupt_source)
        .with_cancel(self.child_cancel.clone())
        .with_mcp(self.mcp.clone())
        .with_local_tools(Arc::clone(&self.local_tools));

        let result = agent
            .run(AgentInput {
                message,
                mode: self.mode.clone(),
                images: Vec::new(),
                preamble: Vec::new(),
                thinking: self.thinking.clone(),
                fast: self.fast,
                workflow: false,
                prompt: None,
            })
            .await;
        drop(agent);
        // Only this call's messages count: older turns may hold stale preamble
        // text, and the agent loop's empty-response retry leaves a synthetic
        // "(empty)" assistant marker that must not pass for a real response.
        // Auto-compaction can shrink the history mid-run, so clamp the start:
        // after a rewrite the tail is this call's output either way.
        let turn = &self.history.as_slice()[history_len.min(self.history.len())..];
        // A subagent can be cancelled on its own, and its caller should hear
        // about that instead of taking a half-finished answer for a real one,
        // so cancel reads like an error here even though the run ended
        // normally.
        let cut_short = match &result {
            Err(error) => Some(error.to_string()),
            Ok(DoneReason::Cancelled) => Some(CANCELLED.to_owned()),
            Ok(_) => None,
        };
        if let Some(error) = cut_short {
            let partial = assistant_text(turn).join("\n");
            return Err(PromptFailure {
                error,
                partial: (!partial.is_empty()).then_some(partial),
            });
        }
        // Waiting here doubles as an ordering barrier: the relay reaches
        // `Done` only after every `TurnComplete`, so all our `ToolLive::Usage`
        // messages sit in the live channel before `dispatch_racing_live`
        // drains it for the last time.
        match self.usage_rx.recv_async().await {
            Ok(usage) => self.usage += usage,
            Err(_) => tracing::warn!(
                name = %self.name,
                "subagent usage tracker stopped, token counts may lag"
            ),
        }
        Ok(PromptResult {
            text: last_assistant_text(turn).unwrap_or_default(),
            duration: self.start.elapsed(),
            input_tokens: self.usage.total_input(),
            output_tokens: self.usage.output,
        })
    }
}

fn assistant_text(turn: &[Message]) -> Vec<&str> {
    turn.iter()
        .filter(|message| matches!(message.role, Role::Assistant))
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::Text { text } if text != EMPTY_RESPONSE_MARKER => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn last_assistant_text(turn: &[Message]) -> Option<String> {
    turn.iter()
        .rfind(|message| matches!(message.role, Role::Assistant))?
        .content
        .iter()
        .find_map(|block| match block {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
}

/// Everything both openers must decide before a [`Subagent`] can exist.
struct Resolved {
    model: Model,
    provider: Arc<dyn provider::Provider>,
    system: String,
    tools: JsonValue,
    mode: AgentMode,
    audience: ToolAudience,
    thinking: ThinkingConfig,
    mcp_enabled: bool,
    task_id: String,
    history_lease: SubagentHistoryLease,
    profile_name: Arc<str>,
}

/// A `task` call: model, prompt, tools, and mode all come from the profile,
/// so the caller supplies only what identifies the work.
pub struct TaskOptions {
    pub name: String,
    /// Continues an earlier task's history when set.
    pub task_id: Option<String>,
    pub profile: Option<String>,
    pub mode: Option<SubagentTaskMode>,
    /// Tool definitions the subagent sees on top of the registry's, paired
    /// with `local_tools` by name.
    pub local_definitions: Vec<JsonValue>,
    pub local_tools: LocalTools,
}

/// A plugin-defined session: the caller brings its own prompt and tools.
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
    let ids = Identity::derive(ctx, opts.task_id.as_deref());
    let default_spec = SubagentTaskSpec {
        profile_name: ctx.system_prompt_profile_name.to_string(),
        mode: SubagentTaskMode::Plan,
        ..SubagentTaskSpec::default()
    };
    let (task_id, history_lease) = match opts.task_id {
        Some(_) => (
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
        None => {
            let spec = SubagentTaskSpec {
                profile_name: opts.profile.unwrap_or(default_spec.profile_name),
                mode: opts.mode.unwrap_or(SubagentTaskMode::Plan),
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

    let model_spec = profile
        .as_deref()
        .and_then(|profile| profile.subagent_model())
        .map(str::to_owned);
    let (model, provider) = resolve_provider(ctx, model_spec.as_deref()).await?;
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

    let (mode, prompt_id, contract, audience) = match spec.mode {
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
    let vars = crate::template::env_vars().set(
        "{task_system_prompt_profiles}",
        bindings.task_tool_summary(BUILTIN_TASK_PROFILE_DESCRIPTION),
    );
    let cwd = vars.apply("{cwd}").into_owned();
    let instructions = smol::unblock(move || crate::agent::load_instruction_text(&cwd)).await;
    let base_filter = ToolFilter::from_config(&ctx.config, &model, &[]).for_mode(&mode);
    let assembled = crate::prompt::assemble_task_with_filter(
        prompt_id,
        &ctx.prompt_slots,
        &base_filter,
        &instructions,
        profile.as_deref(),
        contract,
    );
    let mut tools = ToolRegistry::global().definitions(
        &vars,
        &DescriptionContext {
            filter: &base_filter,
            audience,
            workflow: false,
        },
        model.supports_tool_examples(),
    );
    tools
        .as_array_mut()
        .expect("definitions return an array")
        .extend(opts.local_definitions);

    build(
        ctx,
        ids,
        Resolved {
            model,
            provider,
            system: vars.apply(&assembled).into_owned(),
            tools,
            mode,
            audience,
            thinking,
            mcp_enabled: spec.mode == SubagentTaskMode::Build,
            task_id,
            history_lease,
            profile_name: Arc::from(spec.profile_name.as_str()),
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
    let (model, provider) = resolve_provider(ctx, opts.model_spec.as_deref()).await?;
    announce_model(ctx, &model);
    build(
        ctx,
        ids,
        Resolved {
            model,
            provider,
            system: opts.system,
            tools: opts.tools,
            mode: AgentMode::Build,
            audience: opts.audience.unwrap_or(DEFAULT_SESSION_AUDIENCE),
            thinking: opts.thinking.unwrap_or_else(|| ctx.opts.thinking.clone()),
            mcp_enabled: opts.mcp.unwrap_or(true),
            task_id,
            history_lease,
            profile_name: Arc::clone(&ctx.system_prompt_profile_name),
        },
        opts.local_tools,
        opts.fast.unwrap_or(ctx.opts.fast),
        opts.name,
    )
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
    fn derive(ctx: &ToolContext, continued: Option<&str>) -> Self {
        let parent_tool_use_id = ctx.tool_use_id.clone().unwrap_or_else(generated_session_id);
        Self {
            root_tool_use_id: ctx
                .root_tool_use_id
                .clone()
                .unwrap_or_else(|| parent_tool_use_id.clone()),
            task_id: continued
                .map(str::to_owned)
                .unwrap_or_else(|| parent_tool_use_id.clone()),
            parent_tool_use_id,
        }
    }
}

fn generated_session_id() -> String {
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

async fn resolve_provider(
    ctx: &ToolContext,
    model_spec: Option<&str>,
) -> Result<(Model, Arc<dyn provider::Provider>), String> {
    let Some(spec) = model_spec else {
        return Ok((Model::clone(&ctx.model), Arc::clone(&ctx.provider)));
    };
    let mut model =
        Model::from_spec_with_policy(spec, &ctx.model_policy).map_err(|error| error.to_string())?;
    let provider = provider::from_model_async(&mut model, ctx.timeouts)
        .await
        .map_err(|error| error.to_string())?;
    Ok((model, Arc::from(provider)))
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
    let tool_filter = ToolFilter::Only(
        resolved
            .tools
            .as_array()
            .expect("tools are an array")
            .iter()
            .filter_map(|definition| definition.get("name")?.as_str().map(str::to_owned))
            .collect(),
    )
    .intersect(&ToolFilter::from_config(&ctx.config, &resolved.model, &[]))
    .including(local_tools.keys().cloned())
    .for_mode(&resolved.mode);

    let (sub_tx, sub_rx) = flume::unbounded::<Envelope>();
    let sub_event_tx = EventSender::new(sub_tx, ctx.event_tx.run_id());
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

    Ok(Subagent {
        params: AgentParams {
            provider: resolved.provider,
            model: resolved.model,
            config: ctx.config.clone(),
            tool_output_lines: caudra_config::ToolOutputLines::default(),
            permissions: Arc::clone(&ctx.permissions),
            session_id: ctx.session_id.clone(),
            root_tool_use_id: Some(ids.root_tool_use_id.clone()),
            mailbox: None,
            timeouts: ctx.timeouts,
            file_tracker: FileReadTracker::fresh(),
            // Shared, not fresh: a subagent tracks its own reads but must not
            // write a file a sibling agent is writing.
            path_locks: Arc::clone(&ctx.path_locks),
            prompt_slots: Arc::clone(&ctx.prompt_slots),
            prompt_profiles: Arc::clone(&ctx.prompt_profiles),
            system_prompt_profile_name: resolved.profile_name,
            subagent_cancels: Arc::new(CancelMap::new()),
            subagent_history: ctx.subagent_history.clone(),
            registry: Arc::clone(ToolRegistry::global_arc()),
            audience: resolved.audience,
            tool_filter,
            model_policy: Arc::clone(&ctx.model_policy),
        },
        system: resolved.system,
        tools: resolved.tools,
        mode: resolved.mode,
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
    use crate::TurnCompleteEvent;
    use caudra_providers::Message;

    const RUN_ID: u64 = 7;
    const PARENT_ID: &str = "task-1";
    const TOOL_ID: &str = "toolu_01";
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

    fn envelope(event: AgentEvent) -> Envelope {
        Envelope {
            event,
            subagent: None,
            run_id: RUN_ID,
        }
    }

    fn parent_info() -> Arc<OnceLock<SubagentInfo>> {
        let info = Arc::new(OnceLock::new());
        info.set(SubagentInfo {
            parent_tool_use_id: PARENT_ID.into(),
            task_id: PARENT_ID.into(),
            name: "research".into(),
            prompt: None,
            model: None,
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
            cost: Some(cost),
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

    /// Drives the relay's stateful half directly: the published sequence is
    /// what a parent header would show, in order.
    fn activities(events: Vec<AgentEvent>) -> Vec<(String, Option<String>)> {
        let (sub_tx, sub_rx) = flume::unbounded();
        let (parent_raw_tx, parent_rx) = flume::unbounded();
        let (usage_tx, _usage_rx) = flume::unbounded();
        for event in events {
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

        parent_rx
            .drain()
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
