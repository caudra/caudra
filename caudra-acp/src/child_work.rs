//! Subagents and background tasks, shown to the client in the richest shape
//! it said it reads: ACP's own subagent sessions, Caudra's `_caudra/*`
//! notifications, or else the child's tool calls on the parent's transcript.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_client_protocol_schema::v1::{
    ClientCapabilities, Error as AcpError, ErrorCode, ErrorStopReason, IdleStateUpdate,
    IdleStopReason, Meta, RunningStateUpdate, SessionCancelCapabilities, SessionId, SessionUpdate,
    StateUpdate, SubagentSessionCapabilities, SubagentUpdate, ToolCallId,
};
use caudra_agent::background::BackgroundTasks;
use caudra_agent::tools::ToolRegistry;
use caudra_agent::types::AgentEvent;
use caudra_agent::{SubagentActivity, SubagentEnd, SubagentInfo, TaskCard, ToolOutput};
use caudra_storage::background::JobKind;
use caudra_storage::id::CaudraId;
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::asks::{
    ask_permission, present_permission_update, resolve_presented_permission, shown_call,
};
use crate::errors::invalid_target;
use crate::locked;
use crate::outbound::{Outbound, PendingState};
use crate::translate::{self, META_NAMESPACE, MessageCursor, OpenCalls};

/// The client `_meta` key that asks for the `_caudra/*` notifications.
pub(crate) const CLIENT_CHILD_WORK: &str = "caudra/childWork";
/// The agent `_meta.caudra` key that says they are on offer.
pub(crate) const META_CHILD_WORK: &str = "childWork";
pub(crate) const SUBAGENT_SPAWNED: &str = "_caudra/subagent/spawned";
pub(crate) const SUBAGENT_PROGRESS: &str = "_caudra/subagent/progress";
pub(crate) const SUBAGENT_FINISHED: &str = "_caudra/subagent/finished";
pub(crate) const TASK_BACKGROUNDED: &str = "_caudra/task/backgrounded";
pub(crate) const TASK_COMPLETED: &str = "_caudra/task/completed";
pub(crate) const SUBAGENT_CANCEL: &str = "_caudra/subagent/cancel";
pub(crate) const TASK_KILL: &str = "_caudra/task/kill";
pub(crate) const UNKNOWN_SUBAGENT: &str = "the session ran no such subagent";
pub(crate) const UNKNOWN_TASK: &str = "the session ran no such background task";
/// How many ended subagents a session remembers, so a late cancel for one
/// still answers `not_running`.
const MAX_ENDED_SUBAGENTS: usize = 256;
/// The largest `_caudra/*` frame, envelope included.
pub(crate) const MAX_FRAME_BYTES: usize = 32 * 1024;
/// The longest descriptive text field; output fields fill the frame instead.
pub(crate) const MAX_TEXT_FIELD_BYTES: usize = 1024;
/// A subagent's progress reaches the client at most this often.
pub(crate) const PROGRESS_INTERVAL: Duration = Duration::from_secs(2);
/// Where a projected call names the subagent that made it.
pub(crate) const META_SUBAGENT_ID: &str = "subagentId";
const CHILD_CALL_SEPARATOR: char = ':';
const CHILD_SESSION_SEPARATOR: char = '/';
const TITLE_SEPARATOR: &str = ": ";
const JSONRPC_VERSION: &str = "2.0";
const OUTPUT_FIELD: &str = "output";
const TASK_RUNNING_STATES: [&str; 3] = ["queued", "running", "cancelling"];
const RESULT_OUTPUT: &str = "output";
const RESULT_SHELL: &str = "shell";
const STATUS_COMPLETED: &str = "completed";
const STATUS_FAILED: &str = "failed";
const STATUS_CANCELLED: &str = "cancelled";
const PERCENT: u64 = 100;

/// How a client sees child work, picked once from its `initialize`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ChildTier {
    /// ACP subagent sessions: the client advertised `subagents`.
    Standard,
    /// `_caudra/*` notifications beside the projected calls.
    Vendor,
    /// The child's tool calls on the parent's transcript, and nothing else.
    #[default]
    Projected,
}

impl ChildTier {
    pub(crate) fn of(capabilities: &ClientCapabilities) -> Self {
        let vendor = capabilities
            .meta
            .as_ref()
            .and_then(|meta| meta.get(CLIENT_CHILD_WORK))
            .and_then(Value::as_bool)
            == Some(true);
        if capabilities.subagents.is_some() {
            Self::Standard
        } else if vendor {
            Self::Vendor
        } else {
            Self::Projected
        }
    }
}

/// What `_caudra/subagent/cancel` and `_caudra/task/kill` answer for a
/// target the session knows.
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(crate) enum StopOutcome {
    Stopped,
    NotRunning { status: String },
}

#[derive(Default)]
struct Ledger {
    running: HashSet<String>,
    /// The most recently ended last, with the status they ended in.
    ended: VecDeque<(String, &'static str)>,
}

/// The subagents a session is running and the ones it saw end, by task id,
/// which the dispatcher checks before it cancels one.
#[derive(Clone, Default)]
pub(crate) struct Children(Arc<Mutex<Ledger>>);

impl Children {
    pub(crate) fn running(&self, subagent_id: &str) -> bool {
        locked(&self.0).running.contains(subagent_id)
    }

    pub(crate) fn ended(&self, subagent_id: &str) -> Option<&'static str> {
        locked(&self.0)
            .ended
            .iter()
            .rev()
            .find(|(ended, _)| ended == subagent_id)
            .map(|(_, status)| *status)
    }

    fn insert(&self, subagent_id: &str) {
        let mut ledger = locked(&self.0);
        ledger.ended.retain(|(ended, _)| ended != subagent_id);
        ledger.running.insert(subagent_id.to_owned());
    }

    fn finish(&self, subagent_id: &str, status: &'static str) {
        let mut ledger = locked(&self.0);
        ledger.running.remove(subagent_id);
        ledger.ended.push_back((subagent_id.to_owned(), status));
        if ledger.ended.len() > MAX_ENDED_SUBAGENTS {
            ledger.ended.pop_front();
        }
    }
}

/// Whether the shell `card` shows still runs, so a kill stops it, or what
/// the kill answers at once: `not_running` with how it ended, or invalid
/// params for a task that is no shell this session ran.
pub(crate) fn shell_stop(
    card: Option<TaskCard>,
    task_id: &str,
) -> Result<Option<StopOutcome>, AcpError> {
    match card {
        Some(card) if card.kind == JobKind::Shell && card.active() => Ok(None),
        Some(card) if card.kind == JobKind::Shell => {
            Ok(Some(StopOutcome::NotRunning { status: card.state }))
        }
        _ => Err(unknown_task(task_id)),
    }
}

pub(crate) fn unknown_task(task_id: &str) -> AcpError {
    invalid_target(&format!("{UNKNOWN_TASK}: {task_id}"))
}

/// The session a standard-tier client knows a subagent by.
pub(crate) fn child_session_id(parent: &SessionId, subagent_id: &str) -> SessionId {
    SessionId::from(format!("{parent}{CHILD_SESSION_SEPARATOR}{subagent_id}"))
}

/// The parent session and subagent a child session id names.
pub(crate) fn split_child_session(child: &SessionId) -> Option<(SessionId, &str)> {
    child
        .0
        .split_once(CHILD_SESSION_SEPARATOR)
        .map(|(parent, subagent)| (SessionId::from(parent.to_owned()), subagent))
}

/// What the pump lends child work for one event.
pub(crate) struct ChildContext<'a> {
    pub(crate) outbound: &'a Outbound,
    pub(crate) pending: &'a PendingState,
    pub(crate) registry: &'a ToolRegistry,
    pub(crate) cwd: &'a Path,
    pub(crate) home: Option<&'a Path>,
    /// The parent's calls in flight, where a child's ask lands when none of
    /// its own calls was shown.
    pub(crate) announced: &'a OpenCalls,
}

impl ChildContext<'_> {
    fn parent_update(&self, sid: &SessionId, update: SessionUpdate) {
        let meta = locked(self.pending).update_meta();
        self.outbound.session_update_meta(sid, update, meta);
    }
}

struct Tracked {
    label: String,
    started: Instant,
    turns: u32,
    tool_calls: u32,
    tokens: u64,
    context_pct: Option<u64>,
    activity: Option<String>,
    open: OpenCalls,
    reply: Option<MessageCursor>,
    last_progress: Option<(Instant, Value)>,
}

impl Tracked {
    fn new(info: &SubagentInfo, now: Instant) -> Self {
        Self {
            label: capped(&info.name),
            started: now,
            turns: 0,
            tool_calls: 0,
            tokens: 0,
            context_pct: None,
            activity: None,
            open: OpenCalls::new(),
            reply: None,
            last_progress: None,
        }
    }

    fn open_call(&mut self, id: &str, name: &str) -> bool {
        if let Some(reply) = &mut self.reply {
            reply.interrupt();
        }
        self.open.insert(id.to_owned(), Arc::from(name)).is_none()
    }

    fn streamed(&mut self) -> &mut MessageCursor {
        self.reply
            .get_or_insert_with(|| MessageCursor::new(CaudraId::generate().to_string()))
    }
}

/// One session's child work, fed every stamped event by the pump.
pub(crate) struct ChildWork {
    route: Route,
    children: Children,
    tracked: HashMap<String, Tracked>,
}

impl ChildWork {
    pub(crate) fn new(tier: ChildTier, parent: SessionId, children: Children) -> Self {
        Self {
            route: Route { tier, parent },
            children,
            tracked: HashMap::new(),
        }
    }

    /// The first event of a subagent announces it, so nothing of it reaches
    /// the client before the client knows it exists.
    pub(crate) fn observe(
        &mut self,
        ctx: &ChildContext,
        info: &SubagentInfo,
        event: AgentEvent,
        now: Instant,
    ) {
        let Self {
            route,
            children,
            tracked,
        } = self;
        if !tracked.contains_key(&info.task_id) {
            children.insert(&info.task_id);
            route.announce(ctx, info);
            tracked.insert(info.task_id.clone(), Tracked::new(info, now));
        }
        if let AgentEvent::SubagentFinished { end } = &event {
            children.finish(&info.task_id, end_status(end));
            if let Some(child) = tracked.remove(&info.task_id) {
                route.finish(ctx, info, child, end, now);
            }
            return;
        }
        let Some(child) = tracked.get_mut(&info.task_id) else {
            return;
        };
        let standard = route.tier == ChildTier::Standard;
        match event {
            AgentEvent::SubagentProgress { progress } => {
                child.tool_calls = child.tool_calls.max(progress.tools);
                child.activity = Some(describe(&progress.activity));
                route.progress(ctx, info, child, now);
            }
            AgentEvent::TurnComplete(turn) => {
                child.turns += 1;
                child.tokens += u64::from(turn.usage.total_input()) + u64::from(turn.usage.output);
                let used = turn
                    .context_size
                    .unwrap_or_else(|| turn.usage.context_tokens());
                child.context_pct = (turn.context_window > 0)
                    .then(|| u64::from(used) * PERCENT / u64::from(turn.context_window));
                route.progress(ctx, info, child, now);
            }
            AgentEvent::MessageStarted { id, .. } if standard => {
                child.reply = Some(MessageCursor::new(id.to_string()));
            }
            AgentEvent::TextDelta { text } if standard => {
                route.child_update(ctx, info, child.streamed().text(&text));
            }
            AgentEvent::ThinkingDelta { text } if standard => {
                route.child_update(ctx, info, child.streamed().thought(&text));
            }
            AgentEvent::ToolInputDelta {
                id,
                name,
                preview: Some(preview),
                ..
            } => {
                let first = child.open_call(&id, &name);
                let update = translate::tool_preview(ctx.registry, &id, &name, preview, first);
                route.send_call(ctx, info, &child.label, update);
            }
            AgentEvent::ToolStart(start) => {
                let first = child.open_call(&start.id, &start.tool);
                let update = translate::tool_start(ctx.registry, &start, ctx.cwd, ctx.home, first);
                route.send_call(ctx, info, &child.label, update);
            }
            AgentEvent::ToolOutput { id, content } => {
                if let Some(name) = child.open.get(&id) {
                    let update = translate::tool_output(&id, Some(name), &content);
                    route.send_call(ctx, info, &child.label, update);
                }
            }
            AgentEvent::ToolDone(done) => {
                if child.open.remove(&done.id).is_some() {
                    let update = translate::tool_done(&done, ctx.cwd, ctx.home);
                    route.send_call(ctx, info, &child.label, update);
                }
            }
            AgentEvent::PermissionRequest(request) => {
                let (sid, call) = route.ask_target(ctx, info, &child.open, &request.id);
                ask_permission(ctx.outbound, ctx.pending, &sid, call, *request);
            }
            AgentEvent::PermissionRequestUpdated(request) => {
                let (sid, call) = route.ask_target(ctx, info, &child.open, &request.id);
                present_permission_update(ctx.outbound, ctx.pending, &sid, call, &request);
            }
            AgentEvent::PermissionRequestResolved { request_id, .. } => {
                resolve_presented_permission(ctx.outbound, ctx.pending, &request_id);
            }
            _ => {}
        }
    }
}

/// Where a session's child work goes, which only its tier decides.
struct Route {
    tier: ChildTier,
    parent: SessionId,
}

impl Route {
    fn child(&self, info: &SubagentInfo) -> SessionId {
        child_session_id(&self.parent, &info.task_id)
    }

    fn announce(&self, ctx: &ChildContext, info: &SubagentInfo) {
        match self.tier {
            ChildTier::Standard => {
                let mut update = SubagentUpdate::new(self.child(info))
                    .title(capped(&info.name))
                    .capabilities(
                        SubagentSessionCapabilities::new().cancel(SessionCancelCapabilities::new()),
                    )
                    .state(StateUpdate::Running(RunningStateUpdate::new()));
                if let Some(prompt) = &info.prompt {
                    update = update.description(capped(prompt));
                }
                ctx.parent_update(&self.parent, SessionUpdate::SubagentUpdate(update));
            }
            ChildTier::Vendor => {
                let parent_prompt = locked(ctx.pending)
                    .prompt
                    .as_ref()
                    .map(|owed| owed.identity.id.clone());
                let params = json!({
                    "sessionId": self.parent,
                    "subagentId": info.task_id,
                    "parentPromptId": parent_prompt,
                    "description": capped(&info.name),
                    "model": info.model.as_deref().map(capped),
                });
                notify(ctx.outbound, SUBAGENT_SPAWNED, params);
            }
            ChildTier::Projected => {}
        }
    }

    /// Sent only when something besides the clock moved, and no sooner than
    /// [`PROGRESS_INTERVAL`] after the last one, both read off `now`.
    fn progress(&self, ctx: &ChildContext, info: &SubagentInfo, child: &mut Tracked, now: Instant) {
        if self.tier != ChildTier::Vendor {
            return;
        }
        let digest = json!({
            "turnCount": child.turns,
            "toolCallCount": child.tool_calls,
            "tokensUsed": child.tokens,
            "contextUsagePct": child.context_pct,
            "activity": child.activity,
        });
        if let Some((sent, last)) = &child.last_progress
            && (*last == digest || now.saturating_duration_since(*sent) < PROGRESS_INTERVAL)
        {
            return;
        }
        let mut params = Map::from_iter([
            ("sessionId".to_owned(), json!(self.parent)),
            ("subagentId".to_owned(), json!(info.task_id)),
            ("durationMs".to_owned(), json!(millis(now, child.started))),
        ]);
        if let Value::Object(fields) = &digest {
            params.extend(fields.clone());
        }
        child.last_progress = Some((now, digest));
        notify(ctx.outbound, SUBAGENT_PROGRESS, Value::Object(params));
    }

    fn finish(
        &self,
        ctx: &ChildContext,
        info: &SubagentInfo,
        mut child: Tracked,
        end: &SubagentEnd,
        now: Instant,
    ) {
        for update in translate::cancel_open_calls(&mut child.open) {
            self.send_call(ctx, info, &child.label, update);
        }
        match self.tier {
            ChildTier::Standard => {
                let stop = match end {
                    SubagentEnd::Completed { .. } => IdleStopReason::EndTurn,
                    SubagentEnd::Cancelled { .. } => IdleStopReason::Cancelled,
                    SubagentEnd::Failed { error, .. } => {
                        IdleStopReason::Error(ErrorStopReason::new().error(AcpError::new(
                            ErrorCode::InternalError.into(),
                            capped(error),
                        )))
                    }
                };
                let update = SubagentUpdate::new(self.child(info))
                    .state(StateUpdate::Idle(IdleStateUpdate::new().stop_reason(stop)));
                ctx.parent_update(&self.parent, SessionUpdate::SubagentUpdate(update));
            }
            ChildTier::Vendor => {
                let (error, output) = match end {
                    SubagentEnd::Completed { output } => (None, Some(output)),
                    SubagentEnd::Failed { error, partial } => {
                        (Some(capped(error)), partial.as_ref())
                    }
                    SubagentEnd::Cancelled { partial } => (None, partial.as_ref()),
                };
                let params = json!({
                    "sessionId": self.parent,
                    "subagentId": info.task_id,
                    "status": end_status(end),
                    "error": error,
                    "durationMs": millis(now, child.started),
                    "toolCalls": child.tool_calls,
                    "turns": child.turns,
                    "tokensUsed": child.tokens,
                    OUTPUT_FIELD: output,
                });
                notify(ctx.outbound, SUBAGENT_FINISHED, params);
            }
            ChildTier::Projected => {}
        }
    }

    fn child_update(&self, ctx: &ChildContext, info: &SubagentInfo, update: SessionUpdate) {
        ctx.outbound.session_update(&self.child(info), update);
    }

    /// A standard-tier client reads the call on the child's own session;
    /// any other reads it on the parent's, named for the subagent.
    fn send_call(
        &self,
        ctx: &ChildContext,
        info: &SubagentInfo,
        label: &str,
        update: SessionUpdate,
    ) {
        match self.tier {
            ChildTier::Standard => self.child_update(ctx, info, update),
            ChildTier::Vendor | ChildTier::Projected => {
                ctx.parent_update(&self.parent, project(update, &info.task_id, label));
            }
        }
    }

    /// The session and card a child's question is asked on: its own call
    /// when the client saw it, else the parent's call that delegated it.
    fn ask_target(
        &self,
        ctx: &ChildContext,
        info: &SubagentInfo,
        open: &OpenCalls,
        request_id: &str,
    ) -> (SessionId, String) {
        let call = shown_call(open, request_id);
        match self.tier {
            ChildTier::Standard => (self.child(info), call.to_owned()),
            ChildTier::Vendor | ChildTier::Projected if open.contains_key(call) => {
                (self.parent.clone(), projected_id(&info.task_id, call))
            }
            ChildTier::Vendor | ChildTier::Projected => (
                self.parent.clone(),
                shown_call(ctx.announced, &info.parent_tool_use_id).to_owned(),
            ),
        }
    }
}

/// The id a child's call carries on the parent's transcript.
pub(crate) fn projected_id(subagent_id: &str, call_id: &str) -> String {
    format!("{subagent_id}{CHILD_CALL_SEPARATOR}{call_id}")
}

/// A child's call as the parent shows it: under its own id space, titled
/// with the subagent's label, and naming the subagent in `_meta`.
fn project(update: SessionUpdate, subagent_id: &str, label: &str) -> SessionUpdate {
    let titled = |title: &str| format!("{label}{TITLE_SEPARATOR}{title}");
    match update {
        SessionUpdate::ToolCall(mut call) => {
            call.tool_call_id = projected_call(subagent_id, &call.tool_call_id);
            call.title = titled(&call.title);
            call.meta = Some(naming(call.meta, subagent_id));
            SessionUpdate::ToolCall(call)
        }
        SessionUpdate::ToolCallUpdate(mut update) => {
            update.tool_call_id = projected_call(subagent_id, &update.tool_call_id);
            update.fields.title = update.fields.title.as_deref().map(titled);
            update.meta = Some(naming(update.meta, subagent_id));
            SessionUpdate::ToolCallUpdate(update)
        }
        other => other,
    }
}

fn projected_call(subagent_id: &str, id: &ToolCallId) -> ToolCallId {
    ToolCallId::from(projected_id(subagent_id, &id.0))
}

fn naming(meta: Option<Meta>, subagent_id: &str) -> Meta {
    let mut meta = meta.unwrap_or_default();
    let caudra = meta
        .entry(META_NAMESPACE.to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    if let Value::Object(caudra) = caudra {
        caudra.insert(META_SUBAGENT_ID.to_owned(), json!(subagent_id));
    }
    meta
}

/// The `status` a subagent ended in, as `_caudra/subagent/finished` names it.
fn end_status(end: &SubagentEnd) -> &'static str {
    match end {
        SubagentEnd::Completed { .. } => STATUS_COMPLETED,
        SubagentEnd::Failed { .. } => STATUS_FAILED,
        SubagentEnd::Cancelled { .. } => STATUS_CANCELLED,
    }
}

fn describe(activity: &SubagentActivity) -> String {
    match activity.detail() {
        Some(detail) => capped(&format!("{}{TITLE_SEPARATOR}{detail}", activity.label())),
        None => capped(activity.label()),
    }
}

fn millis(now: Instant, started: Instant) -> u64 {
    u64::try_from(now.saturating_duration_since(started).as_millis()).unwrap_or(u64::MAX)
}

/// `text` cut to [`MAX_TEXT_FIELD_BYTES`] on a character boundary.
pub(crate) fn capped(text: &str) -> String {
    text[..text.floor_char_boundary(MAX_TEXT_FIELD_BYTES)].to_owned()
}

fn notify(outbound: &Outbound, method: &str, params: Value) {
    outbound.notify_ext(method, fit_frame(method, params));
}

/// `params` without its unknown fields, its output cut until the whole frame
/// fits [`MAX_FRAME_BYTES`].
fn fit_frame(method: &str, mut params: Value) -> Value {
    if let Value::Object(fields) = &mut params {
        fields.retain(|_, value| !value.is_null());
    }
    loop {
        let size = frame_size(method, &params);
        let Some(output) = params.get_mut(OUTPUT_FIELD).and_then(|value| match value {
            Value::String(output) => Some(output),
            _ => None,
        }) else {
            return params;
        };
        if size <= MAX_FRAME_BYTES || output.is_empty() {
            return params;
        }
        let keep = output.len().saturating_sub(size - MAX_FRAME_BYTES);
        let keep = output.floor_char_boundary(keep);
        output.truncate(keep);
    }
}

fn frame_size(method: &str, params: &Value) -> usize {
    let frame = json!({ "jsonrpc": JSONRPC_VERSION, "method": method, "params": params });
    serde_json::to_vec(&frame).map_or(usize::MAX, |bytes| bytes.len())
}

/// Turns what a session's background shells do into `_caudra/task/*`
/// notifications. Shells admitted before it started are left unreported.
pub(crate) struct TaskWatch {
    parent: SessionId,
    running: HashMap<String, bool>,
}

impl TaskWatch {
    pub(crate) fn new(parent: SessionId, cards: &[TaskCard]) -> Self {
        let running = cards
            .iter()
            .filter(|card| card.kind == JobKind::Shell)
            .map(|card| (card.invocation_id.clone(), active(card)))
            .collect();
        Self { parent, running }
    }

    /// The notifications `cards` call for. `settled` looks up a finished
    /// shell's full card, the one carrying its result.
    pub(crate) fn observe(
        &mut self,
        cards: &[TaskCard],
        settled: impl Fn(&TaskCard) -> Option<TaskCard>,
    ) -> Vec<(&'static str, Value)> {
        let mut notifications = Vec::new();
        for card in cards.iter().filter(|card| card.kind == JobKind::Shell) {
            let now = active(card);
            let before = self.running.insert(card.invocation_id.clone(), now);
            if before.is_none() {
                notifications.push((TASK_BACKGROUNDED, self.backgrounded(card)));
            }
            if before != Some(false) && !now {
                let full = settled(card).unwrap_or_else(|| card.clone());
                notifications.push((TASK_COMPLETED, self.completed(&full)));
            }
        }
        notifications
    }

    fn backgrounded(&self, card: &TaskCard) -> Value {
        let shell = card.shell.as_deref();
        fit_frame(
            TASK_BACKGROUNDED,
            json!({
                "sessionId": self.parent,
                "toolCallId": card.call_id,
                "taskId": card.task_id,
                "command": capped(shell.map_or(card.label.as_str(), |shell| shell.command.as_str())),
                "cwd": shell.map(|shell| capped(&shell.workdir)),
            }),
        )
    }

    fn completed(&self, card: &TaskCard) -> Value {
        let result = card.result.as_ref();
        let output = result
            .and_then(|result| result.get(RESULT_OUTPUT))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let shell = result
            .and_then(|result| result.get(RESULT_SHELL))
            .and_then(|shell| serde_json::from_value::<ToolOutput>(shell.clone()).ok());
        let (exit_code, signal) = match shell {
            Some(ToolOutput::Shell(shell)) => (shell.exit_code, shell.signal),
            _ => (None, None),
        };
        let total = card
            .output_ref
            .as_ref()
            .map_or(output.len(), |reference| reference.byte_count);
        let mut params = fit_frame(
            TASK_COMPLETED,
            json!({
                "sessionId": self.parent,
                "taskId": card.task_id,
                "state": card.state,
                "exitCode": exit_code,
                "signal": signal,
                OUTPUT_FIELD: output,
                "truncated": true,
                "outputTotalBytes": total,
            }),
        );
        let shown = params[OUTPUT_FIELD].as_str().map_or(0, str::len);
        params["truncated"] = json!(shown < total || card.result_truncated);
        params
    }
}

fn active(card: &TaskCard) -> bool {
    TASK_RUNNING_STATES.contains(&card.state.as_str())
}

/// Reports `background`'s shells until the pump that spawned it drops it.
pub(crate) async fn watch_tasks(
    background: BackgroundTasks,
    outbound: Outbound,
    parent: SessionId,
) {
    let mut watch = TaskWatch::new(parent, &background.list());
    loop {
        let changed = background.listen();
        let cards = background.list();
        let settled = |card: &TaskCard| {
            background.resident_invocation_status(&card.task_id, &card.invocation_id)
        };
        for (method, params) in watch.observe(&cards, settled) {
            outbound.notify_ext(method, params);
        }
        changed.await;
    }
}

#[cfg(test)]
mod tests {
    use caudra_agent::CancelToken;
    use caudra_agent::tools::ToolEffect;
    use caudra_agent::types::{ShellOutput, ToolStartEvent, TurnCompleteEvent};
    use caudra_providers::{Billing, Message, TokenUsage};
    use caudra_storage::background::{JobOwner, ShellJobMetadata};
    use caudra_storage::usage_ledger::LedgerPurpose;
    use flume::Receiver;
    use test_case::test_case;

    use super::*;
    use crate::outbound::Frame;
    use crate::test_support::{PROMPT_ID, TestServer, owed};

    const PARENT: &str = "parent-session";
    const SUBAGENT: &str = "explore-1";
    const DELEGATING_CALL: &str = "toolu_task";
    const LABEL: &str = "Find the bug";
    const PROMPT: &str = "Look through src for the panic";
    const MODEL: &str = "openai/gpt-5";
    const CALL: &str = "toolu_read";
    const TOOL: &str = "file_read";
    const SUMMARY: &str = "src/lib.rs";
    const OUTPUT: &str = "The panic is in parse()";
    const FAILURE: &str = "provider refused";
    const SESSION_UPDATE: &str = "session/update";
    const OUTPUT_TOKENS: u32 = 40;
    const CONTEXT_SIZE: u32 = 50_000;
    const CONTEXT_WINDOW: u32 = 200_000;
    const TASK: &str = "shell-cargo";
    const INVOCATION: &str = "invocation-1";
    const COMMAND: &str = "cargo test";
    const WORKDIR: &str = "/project";
    const SHELL_MODE: &str = "build";
    const TIMEOUT_MS: u64 = 600_000;
    const EXIT_CODE: i32 = 101;
    const RUNNING: &str = "running";
    const SUCCEEDED: &str = "succeeded";
    const FAILED_STATE: &str = "failed";
    const INVALID_PARAMS: i64 = -32602;

    struct Harness {
        work: ChildWork,
        outbound: Outbound,
        frames: Receiver<Frame>,
        pending: PendingState,
        registry: ToolRegistry,
        started: Instant,
    }

    impl Harness {
        fn new(tier: ChildTier) -> Self {
            let (outbound, frames) = Outbound::channel();
            Self {
                work: ChildWork::new(tier, SessionId::from(PARENT), Children::default()),
                outbound,
                frames,
                pending: PendingState::default(),
                registry: ToolRegistry::default(),
                started: Instant::now(),
            }
        }

        fn feed(&mut self, event: AgentEvent, after: Duration) {
            let ctx = ChildContext {
                outbound: &self.outbound,
                pending: &self.pending,
                registry: &self.registry,
                cwd: Path::new(WORKDIR),
                home: None,
                announced: &OpenCalls::new(),
            };
            self.work
                .observe(&ctx, &info(), event, self.started + after);
        }

        fn sent(&self) -> Vec<Value> {
            self.frames.drain().map(|frame| frame.message).collect()
        }
    }

    fn info() -> SubagentInfo {
        SubagentInfo {
            parent_tool_use_id: DELEGATING_CALL.into(),
            task_id: SUBAGENT.into(),
            name: LABEL.into(),
            prompt: Some(PROMPT.into()),
            model: Some(MODEL.into()),
            thinking: None,
            fast: false,
            answer_tx: None,
            steer_tx: None,
            control: None,
        }
    }

    fn read_start() -> AgentEvent {
        AgentEvent::ToolStart(Box::new(ToolStartEvent {
            id: CALL.into(),
            tool: Arc::from(TOOL),
            effect: ToolEffect::Unknown,
            summary: SUMMARY.into(),
            render_header: None,
            annotation: None,
            input: None,
            raw_input: None,
            output: None,
        }))
    }

    fn progress(tools: u32) -> AgentEvent {
        AgentEvent::SubagentProgress {
            progress: caudra_agent::SubagentProgress {
                activity: SubagentActivity::tool(Arc::from(TOOL), SUMMARY),
                tools,
                elapsed: Duration::ZERO,
            },
        }
    }

    fn turn() -> AgentEvent {
        AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
            interrupted: false,
            message: Message::default(),
            usage: TokenUsage {
                output: OUTPUT_TOKENS,
                ..TokenUsage::default()
            },
            model: String::new(),
            provider: String::new(),
            purpose: LedgerPurpose::Chat,
            cost: None,
            billing: Billing::Api,
            account: None,
            context_size: Some(CONTEXT_SIZE),
            context_window: CONTEXT_WINDOW,
        }))
    }

    fn finished(end: SubagentEnd) -> AgentEvent {
        AgentEvent::SubagentFinished { end }
    }

    fn completed() -> SubagentEnd {
        SubagentEnd::Completed {
            output: OUTPUT.into(),
        }
    }

    fn child_sid() -> String {
        child_session_id(&SessionId::from(PARENT), SUBAGENT)
            .0
            .to_string()
    }

    fn methods(frames: &[Value]) -> Vec<&str> {
        frames
            .iter()
            .map(|frame| frame["method"].as_str().unwrap())
            .collect()
    }

    #[test_case(json!({}) => ChildTier::Projected ; "nothing_advertised_projects")]
    #[test_case(json!({"_meta": {CLIENT_CHILD_WORK: true}}) => ChildTier::Vendor ; "vendor_meta_flag")]
    #[test_case(json!({"_meta": {CLIENT_CHILD_WORK: false}}) => ChildTier::Projected ; "vendor_flag_off")]
    #[test_case(json!({"_meta": {CLIENT_CHILD_WORK: "yes"}}) => ChildTier::Projected ; "vendor_flag_not_boolean")]
    #[test_case(json!({"subagents": {}}) => ChildTier::Standard ; "subagents_capability")]
    #[test_case(json!({"subagents": {}, "_meta": {CLIENT_CHILD_WORK: true}}) => ChildTier::Standard ; "standard_wins_over_vendor")]
    fn the_tier_follows_the_client_capabilities(capabilities: Value) -> ChildTier {
        ChildTier::of(&serde_json::from_value(capabilities).unwrap())
    }

    #[test]
    fn standard_announces_the_child_before_its_traffic() {
        let mut harness = Harness::new(ChildTier::Standard);
        harness.feed(read_start(), Duration::ZERO);
        let frames = harness.sent();

        assert_eq!(methods(&frames), [SESSION_UPDATE, SESSION_UPDATE]);
        assert_eq!(frames[0]["params"]["sessionId"], PARENT);
        assert_eq!(
            frames[0]["params"]["update"],
            json!({
                "sessionUpdate": "subagent_update",
                "sessionId": child_sid(),
                "title": LABEL,
                "description": PROMPT,
                "capabilities": {"cancel": {}},
                "state": {"state": "running"},
            })
        );
        assert_eq!(frames[1]["params"]["sessionId"], child_sid());
        assert_eq!(frames[1]["params"]["update"]["sessionUpdate"], "tool_call");
        assert_eq!(frames[1]["params"]["update"]["toolCallId"], CALL);
    }

    #[test_case(completed() => json!({"state": "idle", "stopReason": "end_turn"}) ; "completed_ends_the_turn")]
    #[test_case(SubagentEnd::Cancelled { partial: None } => json!({"state": "idle", "stopReason": "cancelled"}) ; "cancelled")]
    #[test_case(
        SubagentEnd::Failed { error: FAILURE.into(), partial: None }
        => json!({"state": "idle", "stopReason": "error", "error": {"code": -32603, "message": FAILURE}})
        ; "failed_carries_the_error"
    )]
    fn standard_reports_the_end_as_an_idle_state(end: SubagentEnd) -> Value {
        let mut harness = Harness::new(ChildTier::Standard);
        harness.feed(read_start(), Duration::ZERO);
        harness.sent();
        harness.feed(finished(end), Duration::ZERO);
        let frames = harness.sent();

        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0]["params"]["sessionId"], child_sid());
        assert_eq!(frames[0]["params"]["update"]["status"], STATUS_FAILED);
        assert_eq!(frames[1]["params"]["sessionId"], PARENT);
        assert_eq!(frames[1]["params"]["update"]["sessionId"], child_sid());
        frames[1]["params"]["update"]["state"].clone()
    }

    #[test]
    fn standard_streams_the_child_reply_on_its_own_session_only() {
        let mut harness = Harness::new(ChildTier::Standard);
        harness.feed(read_start(), Duration::ZERO);
        harness.feed(
            AgentEvent::TextDelta {
                text: OUTPUT.into(),
            },
            Duration::ZERO,
        );
        let frames = harness.sent();

        let on_parent: Vec<_> = frames
            .iter()
            .filter(|frame| frame["params"]["sessionId"] == PARENT)
            .map(|frame| frame["params"]["update"]["sessionUpdate"].clone())
            .collect();
        assert_eq!(on_parent, [json!("subagent_update")]);
        assert_eq!(frames[2]["params"]["update"]["content"]["text"], OUTPUT);
    }

    #[test_case(ChildTier::Projected ; "projected")]
    #[test_case(ChildTier::Vendor ; "vendor")]
    fn child_calls_are_projected_onto_the_parent_once(tier: ChildTier) {
        let mut harness = Harness::new(tier);
        harness.feed(read_start(), Duration::ZERO);
        harness.feed(
            AgentEvent::TextDelta {
                text: OUTPUT.into(),
            },
            Duration::ZERO,
        );
        let frames = harness.sent();

        let calls: Vec<_> = frames
            .iter()
            .filter(|frame| frame["method"] == SESSION_UPDATE)
            .collect();
        assert_eq!(calls.len(), 1);
        let call = &calls[0]["params"];
        assert_eq!(call["sessionId"], PARENT);
        assert_eq!(call["update"]["toolCallId"], projected_id(SUBAGENT, CALL));
        assert_eq!(
            call["update"]["title"],
            format!("{LABEL}{TITLE_SEPARATOR}{SUMMARY}")
        );
        assert_eq!(
            call["update"]["_meta"][META_NAMESPACE][META_SUBAGENT_ID],
            SUBAGENT
        );
        let notified = frames.len() - calls.len();
        assert_eq!(notified, usize::from(tier == ChildTier::Vendor));
    }

    #[test]
    fn vendor_notifications_carry_the_documented_fields() {
        let mut harness = Harness::new(ChildTier::Vendor);
        locked(&harness.pending).prompt = Some(owed(PROMPT_ID));
        let prompt_id = locked(&harness.pending)
            .prompt
            .as_ref()
            .unwrap()
            .identity
            .id
            .clone();
        harness.feed(progress(1), Duration::ZERO);
        harness.feed(turn(), PROGRESS_INTERVAL);
        harness.feed(finished(completed()), PROGRESS_INTERVAL * 2);
        let frames = harness.sent();

        assert_eq!(
            methods(&frames),
            [
                SUBAGENT_SPAWNED,
                SUBAGENT_PROGRESS,
                SUBAGENT_PROGRESS,
                SUBAGENT_FINISHED
            ]
        );
        let interval = u64::try_from(PROGRESS_INTERVAL.as_millis()).unwrap();
        let activity = format!("{TOOL}{TITLE_SEPARATOR}{SUMMARY}");
        let tokens = u64::from(OUTPUT_TOKENS);
        let pct = u64::from(CONTEXT_SIZE) * PERCENT / u64::from(CONTEXT_WINDOW);
        assert_eq!(
            frames[0]["params"],
            json!({
                "sessionId": PARENT,
                "subagentId": SUBAGENT,
                "parentPromptId": prompt_id,
                "description": LABEL,
                "model": MODEL,
            })
        );
        assert_eq!(
            frames[1]["params"],
            json!({
                "sessionId": PARENT,
                "subagentId": SUBAGENT,
                "durationMs": 0,
                "turnCount": 0,
                "toolCallCount": 1,
                "tokensUsed": 0,
                "activity": activity,
            })
        );
        assert_eq!(
            frames[2]["params"],
            json!({
                "sessionId": PARENT,
                "subagentId": SUBAGENT,
                "durationMs": interval,
                "turnCount": 1,
                "toolCallCount": 1,
                "tokensUsed": tokens,
                "contextUsagePct": pct,
                "activity": activity,
            })
        );
        assert_eq!(
            frames[3]["params"],
            json!({
                "sessionId": PARENT,
                "subagentId": SUBAGENT,
                "status": STATUS_COMPLETED,
                "durationMs": interval * 2,
                "toolCalls": 1,
                "turns": 1,
                "tokensUsed": tokens,
                "output": OUTPUT,
            })
        );
    }

    #[test]
    fn vendor_failure_reports_status_and_error() {
        let mut harness = Harness::new(ChildTier::Vendor);
        harness.feed(
            finished(SubagentEnd::Failed {
                error: FAILURE.into(),
                partial: None,
            }),
            Duration::ZERO,
        );
        let frames = harness.sent();

        assert_eq!(methods(&frames), [SUBAGENT_SPAWNED, SUBAGENT_FINISHED]);
        assert_eq!(frames[1]["params"]["status"], STATUS_FAILED);
        assert_eq!(frames[1]["params"]["error"], FAILURE);
        assert!(frames[1]["params"].get(OUTPUT_FIELD).is_none());
    }

    #[test]
    fn progress_waits_out_the_interval_and_skips_what_did_not_change() {
        let mut harness = Harness::new(ChildTier::Vendor);
        let half = PROGRESS_INTERVAL / 2;
        for (tools, after) in [
            (1, Duration::ZERO),
            (2, half),
            (3, PROGRESS_INTERVAL),
            (3, PROGRESS_INTERVAL * 3),
        ] {
            harness.feed(progress(tools), after);
        }
        let counts: Vec<_> = harness
            .sent()
            .iter()
            .filter(|frame| frame["method"] == SUBAGENT_PROGRESS)
            .map(|frame| frame["params"]["toolCallCount"].clone())
            .collect();

        assert_eq!(counts, [json!(1), json!(3)]);
    }

    #[test]
    fn frames_and_text_fields_stay_within_their_caps() {
        let mut harness = Harness::new(ChildTier::Vendor);
        let huge = "é".repeat(MAX_FRAME_BYTES);
        let long_label = SubagentInfo {
            name: huge.clone(),
            ..info()
        };
        let ctx = ChildContext {
            outbound: &harness.outbound,
            pending: &harness.pending,
            registry: &harness.registry,
            cwd: Path::new(WORKDIR),
            home: None,
            announced: &OpenCalls::new(),
        };
        let end = finished(SubagentEnd::Completed { output: huge });
        harness
            .work
            .observe(&ctx, &long_label, end, harness.started);
        let frames = harness.sent();

        assert_eq!(methods(&frames), [SUBAGENT_SPAWNED, SUBAGENT_FINISHED]);
        let description = frames[0]["params"]["description"].as_str().unwrap();
        assert!(description.len() <= MAX_TEXT_FIELD_BYTES);
        let size = serde_json::to_vec(&frames[1]).unwrap().len();
        assert!(size <= MAX_FRAME_BYTES, "{size} bytes");
        assert!(
            !frames[1]["params"][OUTPUT_FIELD]
                .as_str()
                .unwrap()
                .is_empty()
        );
    }

    fn shell_card(state: &str, result: Option<Value>) -> TaskCard {
        TaskCard {
            kind: JobKind::Shell,
            owner: JobOwner::Main,
            shell: Some(Box::new(ShellJobMetadata {
                call_id: CALL.into(),
                root_call_id: CALL.into(),
                command: COMMAND.into(),
                workdir: WORKDIR.into(),
                timeout_ms: TIMEOUT_MS,
                mode: SHELL_MODE.into(),
                eta: None,
            })),
            task_id: TASK.into(),
            invocation_id: INVOCATION.into(),
            call_id: CALL.into(),
            root_call_id: CALL.into(),
            label: COMMAND.into(),
            state: state.into(),
            background: true,
            mode: SHELL_MODE.into(),
            generation: 0,
            created_at: 0,
            updated_at: 0,
            result,
            output_ref: None,
            result_preview: None,
            result_truncated: false,
            reports: Vec::new(),
            reports_truncated: false,
            duration_ms: None,
        }
    }

    fn shell_result(output: &str) -> Value {
        let shell = ShellOutput {
            model_text: output.into(),
            relative_workdir: WORKDIR.into(),
            timeout_ms: TIMEOUT_MS,
            duration_ms: 0,
            exit_code: Some(EXIT_CODE),
            signal: None,
            timed_out: false,
            output_limit_exceeded: false,
            final_sequence: 0,
            stdout_utf8_bytes: 0,
            stderr_utf8_bytes: 0,
            stdout: output.into(),
            stderr: String::new(),
            stdout_capture_truncated: false,
            stderr_capture_truncated: false,
            stdout_preview_truncated: false,
            stderr_preview_truncated: false,
            stdout_redraws_collapsed: 0,
            stderr_redraws_collapsed: 0,
            filter: None,
        };
        json!({ RESULT_OUTPUT: output, RESULT_SHELL: ToolOutput::Shell(shell) })
    }

    #[test]
    fn a_background_shell_is_reported_when_admitted_and_when_it_ends() {
        let mut watch = TaskWatch::new(SessionId::from(PARENT), &[]);
        let admitted = watch.observe(&[shell_card(RUNNING, None)], |_| None);
        let settled = shell_card(FAILED_STATE, Some(shell_result(OUTPUT)));
        let ended = watch.observe(&[shell_card(FAILED_STATE, None)], |_| Some(settled.clone()));
        let repeated = watch.observe(&[shell_card(FAILED_STATE, None)], |_| None);

        assert_eq!(
            admitted,
            [(
                TASK_BACKGROUNDED,
                json!({
                    "sessionId": PARENT,
                    "toolCallId": CALL,
                    "taskId": TASK,
                    "command": COMMAND,
                    "cwd": WORKDIR,
                })
            )]
        );
        assert_eq!(
            ended,
            [(
                TASK_COMPLETED,
                json!({
                    "sessionId": PARENT,
                    "taskId": TASK,
                    "state": FAILED_STATE,
                    "exitCode": EXIT_CODE,
                    "output": OUTPUT,
                    "truncated": false,
                    "outputTotalBytes": OUTPUT.len(),
                })
            )]
        );
        assert!(repeated.is_empty());
    }

    #[test]
    fn a_shell_that_ended_between_looks_is_reported_whole() {
        let mut watch = TaskWatch::new(SessionId::from(PARENT), &[]);
        let methods: Vec<_> = watch
            .observe(&[shell_card(SUCCEEDED, None)], |_| None)
            .into_iter()
            .map(|(method, _)| method)
            .collect();

        assert_eq!(methods, [TASK_BACKGROUNDED, TASK_COMPLETED]);
    }

    #[test]
    fn shells_running_before_the_watch_are_not_reported() {
        let mut watch = TaskWatch::new(SessionId::from(PARENT), &[shell_card(RUNNING, None)]);

        assert!(
            watch
                .observe(&[shell_card(RUNNING, None)], |_| None)
                .is_empty()
        );
    }

    #[test]
    fn a_long_task_output_is_cut_to_the_frame_and_marked_truncated() {
        let mut watch = TaskWatch::new(SessionId::from(PARENT), &[shell_card(RUNNING, None)]);
        let output = "x".repeat(MAX_FRAME_BYTES * 2);
        let settled = shell_card(SUCCEEDED, Some(shell_result(&output)));
        let ended = watch.observe(&[shell_card(SUCCEEDED, None)], |_| Some(settled.clone()));
        let (method, params) = &ended[0];
        let frame = json!({"jsonrpc": JSONRPC_VERSION, "method": method, "params": params});

        assert!(serde_json::to_vec(&frame).unwrap().len() <= MAX_FRAME_BYTES);
        assert_eq!(params["truncated"], true);
        assert_eq!(params["outputTotalBytes"], output.len());
    }

    fn request(method: &str, params: Value) -> Value {
        json!({"jsonrpc": JSONRPC_VERSION, "id": PROMPT_ID, "method": method, "params": params})
    }

    #[test_case(SUBAGENT_CANCEL, json!({"subagentId": SUBAGENT}), UNKNOWN_SUBAGENT ; "unknown_subagent")]
    #[test_case(TASK_KILL, json!({"taskId": TASK}), UNKNOWN_TASK ; "unknown_task")]
    fn an_unknown_target_is_invalid_params(method: &str, mut params: Value, expected: &str) {
        let mut test = TestServer::with_session();
        params["sessionId"] = json!(test.sid);
        test.server.handle_message(request(method, params));
        let answer = test.next_message();

        assert_eq!(answer["error"]["code"], INVALID_PARAMS);
        assert!(
            answer["error"]["message"]
                .as_str()
                .unwrap()
                .starts_with(expected)
        );
    }

    #[test]
    fn cancelling_a_running_subagent_reaches_its_cancel_trigger() {
        let mut test = TestServer::with_session();
        let (trigger, token) = CancelToken::new();
        test.session()
            .handle
            .subagent_cancels
            .insert(SUBAGENT.to_owned(), trigger);
        test.session().children.insert(SUBAGENT);
        let params = json!({"sessionId": test.sid, "subagentId": SUBAGENT});
        test.server.handle_message(request(SUBAGENT_CANCEL, params));

        assert_eq!(test.next_message()["result"], json!({"outcome": "stopped"}));
        assert!(token.is_cancelled());
    }

    #[test_case(STATUS_COMPLETED ; "completed")]
    #[test_case(STATUS_FAILED ; "failed")]
    #[test_case(STATUS_CANCELLED ; "cancelled")]
    fn cancelling_an_ended_subagent_answers_how_it_ended(status: &'static str) {
        let mut test = TestServer::with_session();
        test.session().children.insert(SUBAGENT);
        test.session().children.finish(SUBAGENT, status);
        let params = json!({"sessionId": test.sid, "subagentId": SUBAGENT});
        test.server.handle_message(request(SUBAGENT_CANCEL, params));

        assert_eq!(
            test.next_message()["result"],
            json!({"outcome": "not_running", "status": status})
        );
    }

    #[test_case(SUBAGENT_CANCEL ; "subagent_cancel")]
    #[test_case(TASK_KILL ; "task_kill")]
    fn a_missing_target_is_invalid_params(method: &str) {
        let mut test = TestServer::with_session();
        let params = json!({"sessionId": test.sid});
        test.server.handle_message(request(method, params));

        assert_eq!(test.next_message()["error"]["code"], INVALID_PARAMS);
    }

    #[test]
    fn a_session_forgets_its_oldest_ended_subagents_past_the_cap() {
        let children = Children::default();
        let ids: Vec<String> = (0..=MAX_ENDED_SUBAGENTS)
            .map(|index| format!("{SUBAGENT}-{index}"))
            .collect();
        for id in &ids {
            children.insert(id);
            children.finish(id, STATUS_COMPLETED);
        }

        assert_eq!(children.ended(&ids[0]), None);
        assert_eq!(children.ended(&ids[1]), Some(STATUS_COMPLETED));
        assert_eq!(
            children.ended(&ids[MAX_ENDED_SUBAGENTS]),
            Some(STATUS_COMPLETED)
        );
    }

    #[test_case(Some(shell_card(RUNNING, None)), Some(None) ; "running_shell_is_stopped")]
    #[test_case(Some(shell_card(FAILED_STATE, None)), Some(Some(StopOutcome::NotRunning { status: FAILED_STATE.into() })) ; "ended_shell_is_not_running")]
    #[test_case(Some(TaskCard { kind: JobKind::Agent, ..shell_card(RUNNING, None) }), None ; "agent_task_is_unknown")]
    #[test_case(None, None ; "unseen_task_is_unknown")]
    fn a_kill_follows_the_shell_card(
        card: Option<TaskCard>,
        expected: Option<Option<StopOutcome>>,
    ) {
        match (shell_stop(card, TASK), expected) {
            (Ok(stop), Some(expected)) => assert_eq!(stop, expected),
            (Err(error), None) => {
                assert_eq!(error.code, AcpError::invalid_params().code);
                assert!(error.message.starts_with(UNKNOWN_TASK));
            }
            (stop, expected) => panic!("{stop:?} is not {expected:?}"),
        }
    }

    #[test]
    fn a_child_session_cancel_for_an_ended_subagent_is_a_no_op() {
        let mut test = TestServer::with_session();
        test.session().children.insert(SUBAGENT);
        test.session().children.finish(SUBAGENT, STATUS_COMPLETED);
        let child = child_session_id(&test.sid, SUBAGENT);
        test.server.handle_message(json!({
            "jsonrpc": JSONRPC_VERSION,
            "method": "session/cancel",
            "params": {"sessionId": child},
        }));

        assert!(test.frames.try_recv().is_err());
    }

    #[test]
    fn a_child_session_cancel_reaches_its_subagent() {
        let mut test = TestServer::with_session();
        let (trigger, token) = CancelToken::new();
        test.session()
            .handle
            .subagent_cancels
            .insert(SUBAGENT.to_owned(), trigger);
        test.session().children.insert(SUBAGENT);
        let child = child_session_id(&test.sid, SUBAGENT);
        test.server.handle_message(json!({
            "jsonrpc": JSONRPC_VERSION,
            "method": "session/cancel",
            "params": {"sessionId": child},
        }));

        assert!(token.is_cancelled());
    }
}
