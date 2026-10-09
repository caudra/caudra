//! SDK streaming mode: `caudra --print --input-format stream-json`.
//!
//! Wire protocol matches Claude Code's SDK interface so tools like Conductor, Windsurf, and custom
//! orchestrators work without adaptation.
//!
//! Per-message wire ids (`uuid`, assistant `message.id`) use `uuid::Uuid::now_v7()` to emit the
//! hyphenated-hex UUIDv7 shape that Claude Code SDK consumers expect, rather than caudra's base58
//! `CaudraId` canonical form.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io::{self, BufRead, Write};
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use caudra_agent::automation::clock::SystemClock;
use caudra_agent::automation::handle::AutomationHandle;
use caudra_agent::background::BackgroundTasks;
use caudra_agent::headless::{
    self, AutomationParams, InteractiveHandle, InteractiveParams, InteractiveRun,
};
use caudra_agent::mcp;
use caudra_agent::permissions::{
    PermissionAnswer, PermissionLifetime, PermissionManager, PluginRuleStore,
};
use caudra_agent::prompt::ResolvedSlots;
use caudra_agent::prompt::profile::{BUILTIN_PROFILE_NAME, PromptProfileCatalog};
use caudra_agent::tools::QUESTION_TOOL_NAME;
use caudra_agent::types::{BACKGROUND_EVENT_RUN_ID, TaskProvenance, WorkflowProvenance};
use caudra_agent::{
    AgentConfig, AgentEvent, AgentInput, AgentMode, DoneReason, Envelope, GoalHandle, GoalResult,
    GoalSnapshot, GoalStatus, GoalVerdict, History, PermissionsConfig, StoredSession,
    goal_kickoff_message,
};
use caudra_automation::request::{
    AutomationError, AutomationRequest, AutomationResponse, DropTarget,
};
use caudra_automation::snapshot::{ArmOrigin, AutomationEvent, FiringSummary, PauseSource};
use caudra_config::decisions::DecisionsConfig;
use caudra_config::{
    AutomationsConfig, ExecutionMode, Feature, FeatureDisabled, ModelPolicy, SnapshotsConfig,
    effective_shell_execution, effective_task_execution,
};
use caudra_providers::model::Model;
use caudra_providers::{
    AutomationEventOrigin, Billing, HistoryItem, HistoryItemKind, ImageSource, Message, StopReason,
    ThinkingConfig, Timeouts, TokenUsage, WorkflowEventOrigin, add_cost,
};
use caudra_storage::id::SessionRef;
use caudra_storage::local_documents::LocalDocumentStore;
use caudra_storage::permission_state::PermissionRuleRecord;
use caudra_storage::sessions::{
    PermissionMode as StoredPermissionMode, SessionError, SessionLease, StoredMode,
    StoredPlanTarget, StoredSubagentTaskSpec,
};
use caudra_storage::tool_outputs::{ToolOutputRef, ToolOutputStore};
use caudra_storage::workspace_binding::StoredWorkspaceBinding;
use caudra_storage::{StateDir, StorageError};
use caudra_workcell::automation_http_client;
use caudra_workflow::{
    LaunchRequest, WorkflowError, WorkflowEvent, WorkflowRequest, WorkflowResponse,
};
use caudra_workspace::PlanRef;
use caudra_workspace::WorkspaceSession;
use color_eyre::Result;
use color_eyre::eyre::{Context, eyre};
use flume::{Receiver, Sender};
use futures_lite::future;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::warn;

use crate::cli::Cli;

pub(crate) const AUTO_PERMISSION_MODE: &str = "auto";
const WORKFLOW_SYSTEM_SUBTYPE: &str = "workflow";
const TASK_CONTROLS: &[&str] = &["task_list", "task_status", "task_cancel", "task_promote"];
const STALE_TASK_INVOCATION: &str = "Task invocation is no longer current";
const WORKFLOW_LIST: &str = "workflow_list";
const WORKFLOW_VALIDATE: &str = "workflow_validate";
const WORKFLOW_START: &str = "workflow_start";
const WORKFLOW_STATUS: &str = "workflow_status";
const WORKFLOW_INSPECT: &str = "workflow_inspect";
const WORKFLOW_HISTORY: &str = "workflow_history";
const WORKFLOW_PAUSE: &str = "workflow_pause";
const WORKFLOW_RESUME: &str = "workflow_resume";
const WORKFLOW_STOP: &str = "workflow_stop";
const WORKFLOW_TRUST: &str = "workflow_trust";
const WORKFLOW_ACK: &str = "workflow_ack";
/// Every `control_request` subtype the workflow runtime answers, as the
/// init message advertises them.
const WORKFLOW_CONTROLS: &[&str] = &[
    WORKFLOW_LIST,
    WORKFLOW_VALIDATE,
    WORKFLOW_START,
    WORKFLOW_STATUS,
    WORKFLOW_INSPECT,
    WORKFLOW_HISTORY,
    WORKFLOW_PAUSE,
    WORKFLOW_RESUME,
    WORKFLOW_STOP,
    WORKFLOW_TRUST,
    WORKFLOW_ACK,
];
const GOAL_SYSTEM_SUBTYPE: &str = "goal";
const GOAL_SET: &str = "goal_set";
const GOAL_CLEAR: &str = "goal_clear";
const GOAL_STATUS: &str = "goal_status";
/// Every `control_request` subtype the session goal answers, as the init
/// message advertises them.
const GOAL_CONTROLS: &[&str] = &[GOAL_SET, GOAL_CLEAR, GOAL_STATUS];
const GOAL_ACTIVE: &str = "active";
const GOAL_FINISHED: &str = "finished";
const NO_GOAL: &str = "none";
const SESSION_CLOSED: &str = "the session is closed";
const AUTOMATION_FIRED_SUBTYPE: &str = "automation_fired";
const AUTOMATION_NOTICE_SUBTYPE: &str = "automation_notice";
const AUTOMATION_LIST: &str = "automation_list";
const AUTOMATION_VALIDATE: &str = "automation_validate";
const AUTOMATION_ARM: &str = "automation_arm";
const AUTOMATION_DISARM: &str = "automation_disarm";
const AUTOMATION_TRUST: &str = "automation_trust";
const AUTOMATION_INSPECT: &str = "automation_inspect";
const AUTOMATION_HISTORY: &str = "automation_history";
const AUTOMATION_FIRING: &str = "automation_firing";
const AUTOMATION_SET_ARGS: &str = "automation_set_args";
const AUTOMATION_SET_STATE: &str = "automation_set_state";
const AUTOMATION_CLEAR_STATE: &str = "automation_clear_state";
const AUTOMATION_DROP: &str = "automation_drop";
const AUTOMATION_DRY_RUN: &str = "automation_dry_run";
const AUTOMATION_PAUSE: &str = "automation_pause";
const AUTOMATION_RESUME: &str = "automation_resume";
/// Every `control_request` subtype the automation runtime answers, as the
/// init message advertises them.
const AUTOMATION_CONTROLS: &[&str] = &[
    AUTOMATION_LIST,
    AUTOMATION_VALIDATE,
    AUTOMATION_ARM,
    AUTOMATION_DISARM,
    AUTOMATION_TRUST,
    AUTOMATION_INSPECT,
    AUTOMATION_HISTORY,
    AUTOMATION_FIRING,
    AUTOMATION_DRY_RUN,
    AUTOMATION_SET_ARGS,
    AUTOMATION_SET_STATE,
    AUTOMATION_CLEAR_STATE,
    AUTOMATION_DROP,
    AUTOMATION_PAUSE,
    AUTOMATION_RESUME,
];
/// The keys an automation control's reply carries the runtime's answer and
/// its structured error under.
const AUTOMATION_REPLY: &str = "automation";
const AUTOMATION_ERROR_REPLY: &str = "automation_error";
const NAME_FIELD: &str = "name";
const ARGS_FIELD: &str = "args";
const DIGEST_FIELD: &str = "digest";
const SESSION_ID_FIELD: &str = "session_id";
const FIRE_ID_FIELD: &str = "fire_id";
const LIMIT_FIELD: &str = "limit";
const STATE_FIELD: &str = "state";
const EXPECTED_REVISION_FIELD: &str = "expected_revision";
const SEQ_FIELD: &str = "seq";
const STRING_KIND: FieldKind<String> = FieldKind {
    name: "a string",
    read: |value| value.as_str().map(str::to_owned),
};
const OBJECT_KIND: FieldKind<Value> = FieldKind {
    name: "an object",
    read: |value| value.is_object().then(|| value.clone()),
};
const INTEGER_KIND: FieldKind<u64> = FieldKind {
    name: "an integer",
    read: Value::as_u64,
};

const TOOL_NAME_MAP: &[(&str, &str)] = &[
    ("file_apply_patch", "FileApplyPatch"),
    ("file_edit", "FileEdit"),
    ("file_glob", "FileGlob"),
    ("file_grep", "FileGrep"),
    ("file_read", "FileRead"),
    ("file_write", "FileWrite"),
    ("shell", "Shell"),
    ("todo_write", "TodoWrite"),
    ("webfetch", "WebFetch"),
    ("websearch", "WebSearch"),
    ("task", "Task"),
    ("python_execution", "PythonExecution"),
    ("code_map", "CodeMap"),
    ("code_context", "CodeContext"),
    ("code_refs", "CodeRefs"),
    ("code_impact", "CodeImpact"),
    ("code_expand", "CodeExpand"),
    ("execution_environment", "ExecutionEnvironment"),
    ("file_index", "Index"),
    ("memory", "Memory"),
    ("question", "Question"),
    ("skill", "Skill"),
];

/// Emits a hyphenated-hex UUIDv7 string for Claude Code SDK wire ids
/// (message.id, assistant message.id).
#[allow(clippy::disallowed_methods)]
fn wire_uuid() -> String {
    uuid::Uuid::now_v7().to_string()
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum PermissionMode {
    Default,
    Auto,
    AcceptEdits,
    Plan,
    BypassPermissions,
}

impl PermissionMode {
    fn resolve(flag: Option<&str>, yolo: bool, auto: bool) -> Self {
        match flag {
            Some(s) => Self::parse(s).unwrap_or_else(|| {
                eprintln!("warning: unknown permission mode '{s}', using default");
                Self::Default
            }),
            None if yolo => Self::BypassPermissions,
            None if auto => Self::Auto,
            None => Self::Default,
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "default" => Some(Self::Default),
            AUTO_PERMISSION_MODE => Some(Self::Auto),
            "acceptEdits" => Some(Self::AcceptEdits),
            "plan" => Some(Self::Plan),
            "bypassPermissions" => Some(Self::BypassPermissions),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Auto => AUTO_PERMISSION_MODE,
            Self::AcceptEdits => "acceptEdits",
            Self::Plan => "plan",
            Self::BypassPermissions => "bypassPermissions",
        }
    }

    fn storage_mode(self) -> StoredPermissionMode {
        match self {
            Self::Auto => StoredPermissionMode::Auto,
            Self::BypassPermissions => StoredPermissionMode::Yolo,
            _ => StoredPermissionMode::Ask,
        }
    }

    fn preserve_plan(self, current: Self) -> Self {
        if self == Self::Auto && current == Self::Plan {
            Self::Plan
        } else {
            self
        }
    }

    fn agent_mode(self, cwd: &Path) -> AgentMode {
        match self {
            Self::Plan => AgentMode::Plan(cwd.join("plan.md")),
            _ => AgentMode::Build,
        }
    }
}

#[derive(Serialize)]
struct WireMessage {
    #[serde(flatten)]
    inner: WireInner,
    session_id: SessionRef,
    uuid: String,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireInner {
    System(SystemPayload),
    Assistant(AssistantPayload),
    User(UserPayload),
    Result(ResultPayload),
    StreamEvent(StreamEventPayload),
    ControlResponse(ControlResponsePayload),
    ControlRequest(ControlRequestPayload),
    ControlCancelRequest(ControlCancelRequestPayload),
}

#[derive(Serialize)]
struct SystemPayload {
    subtype: &'static str,
    #[serde(flatten)]
    extra: Value,
}

#[derive(Serialize)]
struct AssistantPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    task: Option<Arc<TaskProvenance>>,
    message: AssistantMessage,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_tool_use_id: Option<String>,
    /// Set on an agent a workflow run launched, as `workflow_*` keys.
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    workflow: Option<WorkflowProvenance>,
}

#[derive(Serialize)]
struct AssistantMessage {
    id: String,
    model: String,
    role: &'static str,
    content: Value,
    stop_reason: Option<StopReason>,
    usage: TokenUsage,
}

#[derive(Serialize)]
struct UserPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    task: Option<Arc<TaskProvenance>>,
    message: UserMessage,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_tool_use_id: Option<String>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    workflow: Option<WorkflowProvenance>,
}

/// The `system` / `api_retry` body. `parent_tool_use_id` names the subagent
/// whose stream is backing off, so a client can tell a task's retry from the
/// main conversation's.
#[derive(Serialize)]
struct RetryPayload<'a> {
    attempt: u32,
    retry_delay_ms: u64,
    error: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_tool_use_id: Option<&'a str>,
}

/// The `system` / `workflow` body: the run's event plus the envelope's
/// `workflow_*` provenance keys, so a client can key it by run.
#[derive(Serialize)]
struct WorkflowSystemPayload<'a> {
    event: &'a WorkflowEvent,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    workflow: Option<&'a WorkflowProvenance>,
}

/// The `system` / `automation_fired` body: a firing that ended, under the
/// summary's own names, and the earlier quiet skip it absorbed, whose row a
/// client drops.
#[derive(Serialize)]
struct AutomationFiredPayload<'a> {
    #[serde(flatten)]
    firing: &'a FiringSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    absorbed: Option<String>,
}

/// The `system` / `automation_notice` body: what a script's `notify()` asks
/// the client to show, or why the session refused an arming or a claimed
/// goal. `fire_id` names the firing it came from, when one did.
#[derive(Serialize)]
struct AutomationNoticePayload {
    automation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    fire_id: Option<String>,
    text: String,
}

/// The `system` / `goal` body: one goal event, named by `kind`. Only the main
/// agent pursues the session goal, so no run or workflow keys come with it.
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum GoalSystemPayload<'a> {
    Evaluating {
        evaluation: u32,
    },
    Evaluation {
        verdict: GoalVerdict,
        reason: &'a str,
        evaluation: u32,
        applied: bool,
        usage: TokenUsage,
        cost: Option<f64>,
        billing: Billing,
        model: &'a str,
    },
    EvaluationFailed {
        evaluation: u32,
        message: &'a str,
        applied: bool,
        usage: TokenUsage,
        cost: Option<f64>,
        billing: Billing,
        model: &'a str,
    },
    Deferred {
        active_background_tasks: usize,
    },
    Finished(GoalReport<'a>),
    LoopCap {
        evaluations: u32,
        continuations: u32,
        limit: u32,
    },
    TurnLimit {
        evaluations: u32,
    },
    ClearedAfterError {
        condition: &'a str,
        message: &'a str,
    },
}

impl<'a> GoalSystemPayload<'a> {
    /// `None` for an event that is not about the goal.
    fn from_event(event: &'a AgentEvent) -> Option<Self> {
        Some(match event {
            AgentEvent::GoalEvaluating { evaluation } => Self::Evaluating {
                evaluation: *evaluation,
            },
            AgentEvent::GoalEvaluation {
                verdict,
                reason,
                evaluation,
                applied,
                usage,
                cost,
                billing,
                model,
            } => Self::Evaluation {
                verdict: *verdict,
                reason,
                evaluation: *evaluation,
                applied: *applied,
                usage: *usage,
                cost: *cost,
                billing: *billing,
                model,
            },
            AgentEvent::GoalEvaluationFailed {
                evaluation,
                message,
                applied,
                usage,
                cost,
                billing,
                model,
            } => Self::EvaluationFailed {
                evaluation: *evaluation,
                message,
                applied: *applied,
                usage: *usage,
                cost: *cost,
                billing: *billing,
                model,
            },
            AgentEvent::GoalDeferred {
                active_background_tasks,
            } => Self::Deferred {
                active_background_tasks: *active_background_tasks,
            },
            AgentEvent::GoalFinished { result } => Self::Finished(result.into()),
            AgentEvent::GoalLoopCap {
                evaluations,
                continuations,
                limit,
            } => Self::LoopCap {
                evaluations: *evaluations,
                continuations: *continuations,
                limit: *limit,
            },
            AgentEvent::GoalTurnLimit { evaluations } => Self::TurnLimit {
                evaluations: *evaluations,
            },
            AgentEvent::GoalClearedAfterError { condition, message } => {
                Self::ClearedAfterError { condition, message }
            }
            _ => return None,
        })
    }
}

/// A goal under the names its `finished` event uses: the result of one that
/// finished, or how far one still active has come.
#[derive(Serialize)]
struct GoalReport<'a> {
    condition: &'a str,
    verdict: Option<GoalVerdict>,
    reason: Option<&'a str>,
    evaluations: u32,
    duration_ms: u128,
    usage: TokenUsage,
    cost: Option<f64>,
    subscription_cost: Option<f64>,
}

impl<'a> From<&'a GoalResult> for GoalReport<'a> {
    fn from(result: &'a GoalResult) -> Self {
        Self {
            condition: &result.condition,
            verdict: Some(result.verdict),
            reason: Some(&result.reason),
            evaluations: result.evaluations,
            duration_ms: result.duration.as_millis(),
            usage: result.usage,
            cost: result.cost,
            subscription_cost: result.subscription_cost,
        }
    }
}

impl<'a> From<&'a GoalSnapshot> for GoalReport<'a> {
    fn from(goal: &'a GoalSnapshot) -> Self {
        Self {
            condition: &goal.condition,
            verdict: goal.last_verdict,
            reason: goal.last_reason.as_deref(),
            evaluations: goal.evaluations,
            duration_ms: goal.elapsed().as_millis(),
            usage: goal.usage,
            cost: goal.cost,
            subscription_cost: goal.subscription_cost,
        }
    }
}

/// What `goal_status` answers, and `goal_set` once its goal is set: the active
/// goal, else the last finished one, and the session's continuation limit.
#[derive(Serialize)]
struct GoalStatusReply<'a> {
    status: &'static str,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    goal: Option<GoalReport<'a>>,
    continuation_limit: u32,
}

/// What `goal_clear` answers: the condition of the goal it stopped, or null
/// when none was active.
#[derive(Serialize)]
struct GoalClearReply<'a> {
    cleared: Option<&'a str>,
}

/// A client's `/goal <condition>`. `kickoff` defaults to true.
#[derive(Deserialize)]
struct GoalSetRequest {
    condition: String,
    continuation_limit: Option<u32>,
    kickoff: Option<bool>,
}

#[derive(Serialize)]
struct UserMessage {
    role: &'static str,
    content: Value,
}

#[derive(Serialize)]
struct ResultPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    run: Option<RunInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    background_active: Option<usize>,
    subtype: &'static str,
    is_error: bool,
    duration_ms: u128,
    duration_api_ms: u128,
    num_turns: u32,
    result: String,
    total_cost_usd: f64,
    /// What a subscription covered, at API list rates. Reported beside
    /// `total_cost_usd` and never added to it, which stays actual spend.
    subscription_cost_usd: f64,
    usage: TokenUsage,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    permission_denials: Vec<Value>,
}

#[derive(Serialize)]
struct StreamEventPayload {
    event: Value,
}

#[derive(Serialize)]
struct RunInfo {
    run_id: u64,
    automatic: bool,
    task_event_ids: Vec<String>,
    workflow_events: Vec<WorkflowEventOrigin>,
    /// The automation messages the run took: the claims it started with,
    /// then the guide items injected into it, in order.
    automation_events: Vec<AutomationEventOrigin>,
}

impl From<&InteractiveRun> for RunInfo {
    fn from(run: &InteractiveRun) -> Self {
        Self {
            run_id: run.run_id,
            automatic: run.automatic,
            task_event_ids: run.task_event_ids.clone(),
            workflow_events: run.workflow_events.clone(),
            automation_events: run.automation_events.clone(),
        }
    }
}

#[derive(Serialize)]
struct ControlResponsePayload {
    response: ControlResponseInner,
}

#[derive(Serialize)]
struct ControlResponseInner {
    subtype: &'static str,
    request_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    response: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct ControlRequestPayload {
    request_id: String,
    request: ControlRequestInner,
}

#[derive(Serialize)]
struct ControlRequestInner {
    #[serde(skip_serializing_if = "Option::is_none")]
    task: Option<Arc<TaskProvenance>>,
    subtype: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_use_id: Option<String>,
}

#[derive(Serialize)]
struct ControlCancelRequestPayload {
    request_id: String,
}

#[derive(serde::Deserialize)]
struct InboundMessage {
    #[serde(rename = "type")]
    msg_type: String,
    #[serde(flatten)]
    payload: Value,
}

#[derive(serde::Deserialize)]
struct InboundUser {
    message: InboundUserMessage,
}

#[derive(serde::Deserialize)]
struct InboundUserMessage {
    content: Value,
}

#[derive(serde::Deserialize)]
struct InboundControlRequest {
    request_id: String,
    request: InboundControlRequestInner,
}

#[derive(serde::Deserialize)]
struct InboundControlRequestInner {
    subtype: String,
    #[serde(flatten)]
    extra: Value,
}

#[derive(serde::Deserialize)]
struct InboundControlResponse {
    response: InboundControlResponseInner,
}

#[derive(serde::Deserialize)]
struct InboundControlResponseInner {
    subtype: String,
    request_id: String,
    #[serde(default)]
    response: Value,
}

#[derive(serde::Deserialize)]
struct InboundControlCancelRequest {
    request_id: String,
}

// StreamSynth owns all Anthropic stream state. Each method returns every wire
// event the transition needs (closing the old block, opening a new message, ...)
// so callers never have to track block lifecycle themselves.

#[derive(Clone, Copy, PartialEq)]
enum BlockKind {
    Text,
    Thinking,
}

struct StreamSynth {
    block_index: i32,
    started: bool,
    current_block: Option<BlockKind>,
    /// The tool call whose block is open, so its argument fragments land in
    /// that block and nothing else can close it by accident.
    open_tool: Option<String>,
    /// Every tool call already opened from the pending phase of this message.
    /// Its `ToolStart` arrives after dispatch, by which point the block may
    /// have been closed by the next call, and must not be emitted twice.
    streamed_tools: Vec<String>,
}

impl StreamSynth {
    fn new() -> Self {
        Self {
            block_index: -1,
            started: false,
            current_block: None,
            open_tool: None,
            streamed_tools: Vec::new(),
        }
    }

    fn reset(&mut self) {
        *self = Self::new();
    }

    fn text_delta(&mut self, model: &str, text: &str) -> Vec<Value> {
        let mut events = self.ensure_block(model, BlockKind::Text);
        events.push(serde_json::json!({
            "type": "content_block_delta",
            "index": self.block_index,
            "delta": {"type": "text_delta", "text": text}
        }));
        events
    }

    fn thinking_delta(&mut self, model: &str, text: &str) -> Vec<Value> {
        let mut events = self.ensure_block(model, BlockKind::Thinking);
        events.push(serde_json::json!({
            "type": "content_block_delta",
            "index": self.block_index,
            "delta": {"type": "thinking_delta", "thinking": text}
        }));
        events
    }

    fn thinking_boundary(&mut self) -> Vec<Value> {
        if self.current_block == Some(BlockKind::Thinking) {
            self.close_block().into_iter().collect()
        } else {
            Vec::new()
        }
    }

    /// Opens the block the moment the call is announced, so the arguments can
    /// stream into it the way the provider sent them.
    fn tool_pending(&mut self, model: &str, id: &str, name: &str) -> Vec<Value> {
        let mut events = self.ensure_started(model);
        events.extend(self.close_block());
        self.block_index += 1;
        self.open_tool = Some(id.to_string());
        self.streamed_tools.push(id.to_string());
        events.push(serde_json::json!({
            "type": "content_block_start",
            "index": self.block_index,
            "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}
        }));
        events
    }

    fn tool_input_delta(&mut self, id: &str, delta: &str) -> Vec<Value> {
        if self.open_tool.as_deref() != Some(id) {
            return Vec::new();
        }
        vec![serde_json::json!({
            "type": "content_block_delta",
            "index": self.block_index,
            "delta": {"type": "input_json_delta", "partial_json": delta}
        })]
    }

    /// A call whose arguments already streamed only needs its block closed.
    /// One that never had a pending phase — a batch child, an MCP passthrough
    /// — still gets the whole input as a single delta.
    fn tool_use(&mut self, model: &str, id: &str, name: &str, input_json: &str) -> Vec<Value> {
        if self.streamed_tools.iter().any(|streamed| streamed == id) {
            return match self.open_tool.as_deref() == Some(id) {
                true => self.close_block().into_iter().collect(),
                false => Vec::new(),
            };
        }
        let mut events = self.ensure_started(model);
        events.extend(self.close_block());
        self.block_index += 1;
        events.push(serde_json::json!({
            "type": "content_block_start",
            "index": self.block_index,
            "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}
        }));
        events.push(serde_json::json!({
            "type": "content_block_delta",
            "index": self.block_index,
            "delta": {"type": "input_json_delta", "partial_json": input_json}
        }));
        events.push(self.block_stop());
        events
    }

    fn finish_message(&mut self, usage: &TokenUsage) -> Vec<Value> {
        if !self.started {
            return Vec::new();
        }
        let mut events: Vec<Value> = self.close_block().into_iter().collect();
        events.push(serde_json::json!({
            "type": "message_delta",
            "delta": {"stop_reason": null},
            "usage": {"output_tokens": usage.output}
        }));
        events.push(serde_json::json!({"type": "message_stop"}));
        self.reset();
        events
    }

    fn ensure_started(&mut self, model: &str) -> Vec<Value> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        vec![serde_json::json!({
            "type": "message_start",
            "message": {
                "id": wire_uuid(),
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": model,
                "stop_reason": null,
                "usage": {"input_tokens": 0, "output_tokens": 0}
            }
        })]
    }

    fn ensure_block(&mut self, model: &str, kind: BlockKind) -> Vec<Value> {
        let mut events = self.ensure_started(model);
        if self.current_block == Some(kind) {
            return events;
        }
        events.extend(self.close_block());
        self.block_index += 1;
        self.current_block = Some(kind);
        let content_block = match kind {
            BlockKind::Text => serde_json::json!({"type": "text", "text": ""}),
            BlockKind::Thinking => serde_json::json!({"type": "thinking", "thinking": ""}),
        };
        events.push(serde_json::json!({
            "type": "content_block_start",
            "index": self.block_index,
            "content_block": content_block
        }));
        events
    }

    fn close_block(&mut self) -> Option<Value> {
        let tool = self.open_tool.take().is_some();
        (self.current_block.take().is_some() || tool).then(|| self.block_stop())
    }

    fn block_stop(&self) -> Value {
        serde_json::json!({
            "type": "content_block_stop",
            "index": self.block_index,
        })
    }
}

fn caudra_to_claude_tool_name(name: &str) -> &str {
    TOOL_NAME_MAP
        .iter()
        .find(|(m, _)| *m == name)
        .map(|(_, c)| *c)
        .unwrap_or(name)
}

#[derive(Clone)]
struct SdkWriter {
    session_id: SessionRef,
    out_tx: Sender<String>,
}

impl SdkWriter {
    fn emit(&self, inner: WireInner) -> Result<()> {
        let msg = WireMessage {
            inner,
            session_id: self.session_id.clone(),
            uuid: wire_uuid(),
        };
        self.out_tx
            .send(serde_json::to_string(&msg)?)
            .map_err(|_| eyre!("stdout writer closed"))
    }

    fn emit_system(&self, subtype: &'static str, extra: Value) -> Result<()> {
        self.emit(WireInner::System(SystemPayload { subtype, extra }))
    }

    fn emit_control_response(
        &self,
        request_id: &str,
        response: Option<Value>,
        error: Option<String>,
    ) -> Result<()> {
        self.emit(WireInner::ControlResponse(ControlResponsePayload {
            response: ControlResponseInner {
                subtype: if error.is_some() { "error" } else { "success" },
                request_id: request_id.into(),
                response,
                error,
            },
        }))
    }

    fn emit_answer(&self, request_id: &str, answer: Result<Value, String>) -> Result<()> {
        match answer {
            Ok(response) => self.emit_control_response(request_id, Some(response), None),
            Err(error) => self.emit_control_response(request_id, None, Some(error)),
        }
    }

    /// A firing goes out once it has ended, a notice as its script sent it.
    fn emit_automation(&self, event: AutomationEvent) -> Result<()> {
        match event {
            AutomationEvent::Firing { firing, absorbed } if !firing.status.is_pending() => self
                .emit_system(
                    AUTOMATION_FIRED_SUBTYPE,
                    serde_json::to_value(AutomationFiredPayload {
                        firing: &firing,
                        absorbed,
                    })?,
                ),
            AutomationEvent::Notice {
                automation,
                fire_id,
                text,
            } => self.emit_system(
                AUTOMATION_NOTICE_SUBTYPE,
                serde_json::to_value(AutomationNoticePayload {
                    automation,
                    fire_id,
                    text,
                })?,
            ),
            AutomationEvent::Firing { .. }
            | AutomationEvent::Session(_)
            | AutomationEvent::Automation(_)
            | AutomationEvent::Outbox(_)
            | AutomationEvent::SaveSession => Ok(()),
        }
    }

    /// Emits the automation events as they arrive, and returns only when one
    /// cannot go out: once their channel closes, the agent events alone end
    /// the pump.
    async fn emit_automations<T>(&self, automation_rx: &Receiver<AutomationEvent>) -> Result<T> {
        while let Ok(event) = automation_rx.recv_async().await {
            self.emit_automation(event)?;
        }
        future::pending().await
    }

    fn emit_direct_command_result(
        &self,
        output: caudra_agent::headless::RemoteCommandOutput,
        duration_ms: u128,
    ) -> Result<()> {
        self.emit(WireInner::Result(ResultPayload {
            run: None,
            background_active: None,
            subtype: if output.is_error { "error" } else { "success" },
            is_error: output.is_error,
            duration_ms,
            duration_api_ms: 0,
            num_turns: 0,
            result: output.output,
            total_cost_usd: 0.0,
            subscription_cost_usd: 0.0,
            usage: TokenUsage::default(),
            permission_denials: Vec::new(),
        }))
    }
}

pub struct SdkParams {
    pub cli: Cli,
    pub model: Model,
    pub config: AgentConfig,
    pub permissions_config: PermissionsConfig,
    pub decisions_config: DecisionsConfig,
    pub seed_permission_mode: Option<StoredPermissionMode>,
    pub snapshots: SnapshotsConfig,
    pub timeouts: Timeouts,
    pub prompt_slots: ResolvedSlots,
    pub prompt_profiles: Arc<PromptProfileCatalog>,
    pub fast: bool,
    pub thinking: ThinkingConfig,
    pub model_policy: Arc<ModelPolicy>,
    pub plugin_rules: Arc<PluginRuleStore>,
    pub workspace_binding: Option<StoredWorkspaceBinding>,
    pub remote_environment: Option<headless::RemoteEnvironment>,
    pub workspace_session: Option<WorkspaceSession>,
    pub remote_project_context:
        Option<Arc<caudra_agent::remote_project_context::RemoteProjectContext>>,
    pub local_documents: Option<Arc<LocalDocumentStore>>,
    pub automations: AutomationsConfig,
}

struct Shared {
    model: Model,
    permission_mode: PermissionMode,
    pending: HashMap<String, String>,
    task_permissions: HashMap<String, (BackgroundTasks, Envelope)>,
    resolved_permission_requests: HashSet<String>,
    workspace_session: Option<WorkspaceSession>,
    local_documents: Option<Arc<LocalDocumentStore>>,
    session_id: SessionRef,
    remote_plan: Option<PlanRef>,
}

impl Shared {
    fn agent_mode(&mut self, cwd: &Path) -> AgentMode {
        self.agent_mode_for(self.permission_mode, cwd)
    }

    fn agent_mode_for(&mut self, permission_mode: PermissionMode, cwd: &Path) -> AgentMode {
        if permission_mode != PermissionMode::Plan || self.workspace_session.is_none() {
            return permission_mode.agent_mode(cwd);
        }
        if self.remote_plan.is_none()
            && let (Some(workspace), Some(store)) = (&self.workspace_session, &self.local_documents)
        {
            self.remote_plan = store
                .create_plan(
                    workspace.binding().project().key(),
                    self.session_id.as_str(),
                )
                .map_err(|error| warn!(%error, "remote plan could not be created"))
                .ok();
        }
        self.remote_plan
            .clone()
            .map(AgentMode::RemotePlan)
            .unwrap_or(AgentMode::ReadOnly)
    }
}

/// What a client's text needs to run as a typed prompt: the mode the session
/// is in, and the mentions and commits the text names.
struct PromptContext<'a> {
    shared: &'a Mutex<Shared>,
    cwd: &'a Path,
    remote: bool,
    thinking: &'a ThinkingConfig,
    fast: bool,
}

impl PromptContext<'_> {
    fn input(&self, prompt: String, images: Vec<ImageSource>) -> AgentInput {
        let mode = self.shared.lock().unwrap().agent_mode(self.cwd);
        let mentions = if self.remote {
            caudra_agent::mentions::scan_remote(&prompt)
                .into_iter()
                .map(|(_, mention)| mention)
                .collect()
        } else {
            caudra_agent::mentions::scan(&prompt, |path| self.cwd.join(path).exists())
                .into_iter()
                .map(|(_, mention)| mention)
                .collect()
        };
        let commits = caudra_agent::commits::scan(&prompt, |_| true)
            .into_iter()
            .map(|(_, commit)| commit)
            .collect();
        AgentInput {
            message: prompt,
            mode,
            plan: None,
            images,
            mentions,
            commits,
            preamble: Vec::new(),
            thinking: self.thinking.clone(),
            fast: self.fast,
            prompt: None,
            resume: false,
        }
    }
}

pub fn run(params: SdkParams) -> Result<()> {
    let SdkParams {
        cli,
        model,
        mut config,
        permissions_config,
        decisions_config,
        seed_permission_mode,
        snapshots,
        timeouts,
        prompt_slots,
        prompt_profiles,
        fast,
        thinking,
        model_policy,
        plugin_rules,
        workspace_binding,
        remote_environment,
        workspace_session,
        remote_project_context,
        local_documents,
        automations,
    } = params;
    cli.warn_ignored_flags();
    if let Some(max) = cli.max_turns {
        config.max_turns = Some(max);
    }
    let max_output_lines = config.max_output_lines;
    let max_output_bytes = config.max_output_bytes;
    let mut requested_permission_mode =
        PermissionMode::resolve(cli.permission_mode.as_deref(), cli.yolo, cli.auto);
    let system_prompt_override = cli.system_prompt.clone().filter(|s| !s.is_empty());

    let cwd = remote_environment.as_ref().map_or_else(
        || std::env::current_dir().unwrap_or_else(|_| ".".into()),
        |environment| environment.cwd.clone().into(),
    );
    let working_dir = cwd.to_string_lossy().into_owned();
    let ResolvedSession {
        session_id,
        session_lease,
        expected_write_version,
        initial_history,
        structured_permission_rules,
        session_permission_mode,
        stored_system_prompt_profile,
        restored_plan,
        restored_legacy_plan,
        restored_plan_mode,
    } = resolve_session(
        &cli,
        &working_dir,
        &prompt_profiles,
        config.system_prompt_profile.as_deref(),
        system_prompt_override.is_some(),
        workspace_binding.as_ref(),
    )?;
    let session_permission_mode =
        startup_permission_mode(&cli, requested_permission_mode, session_permission_mode);
    let restored_plan = restored_plan.or_else(|| {
        restored_legacy_plan.as_deref().and_then(|path| {
            workspace_session
                .as_ref()
                .zip(local_documents.as_ref())
                .and_then(|(workspace, store)| {
                    store
                        .adopt_legacy_plan(
                            workspace.binding().project().key(),
                            session_id.as_str(),
                            path,
                        )
                        .ok()
                })
        })
    });
    if restored_plan_mode
        && ((cli.permission_mode.is_none() && !cli.yolo)
            || requested_permission_mode == PermissionMode::Auto)
    {
        requested_permission_mode = PermissionMode::Plan;
    }
    crate::setup::report_session_start(
        if initial_history.is_empty() {
            caudra_otel::emit::START_FRESH
        } else {
            caudra_otel::emit::START_RESUME
        },
        Some(&session_id),
    );

    let (mcp_handle, mcp_config_errors) = if remote_environment.is_some() {
        smol::block_on(mcp::start_global_connected(&cwd))
    } else {
        smol::block_on(mcp::start_connected(&cwd))
    };
    if !mcp_config_errors.is_empty() {
        eprintln!("MCP config error: {mcp_config_errors}");
    }
    if let Some(handle) = &mcp_handle {
        let awaiting: Vec<_> = handle
            .reader()
            .load()
            .infos
            .iter()
            .filter(|info| info.status == caudra_agent::McpServerStatus::AwaitingTrust)
            .map(|info| info.name.clone())
            .collect();
        if !awaiting.is_empty() {
            return Err(eyre!(
                "project MCP servers require startup trust: {}. Run `caudra`, review them with `/mcp`, then retry",
                awaiting.join(", ")
            ));
        }
    }
    let sdk_mcp_servers: Vec<_> = mcp_handle
        .as_ref()
        .map(|handle| {
            handle
                .reader()
                .load()
                .infos
                .iter()
                .map(|info| {
                    let status = match &info.status {
                        caudra_agent::McpServerStatus::Running => "connected",
                        caudra_agent::McpServerStatus::Connecting => "connecting",
                        caudra_agent::McpServerStatus::AwaitingTrust => "pending",
                        caudra_agent::McpServerStatus::Disabled => "disabled",
                        caudra_agent::McpServerStatus::Failed(_) => "failed",
                        caudra_agent::McpServerStatus::NeedsAuth { .. } => "needs-auth",
                    };
                    serde_json::json!({"name": info.name, "status": status})
                })
                .collect()
        })
        .unwrap_or_default();

    let (system_prompt_profile_name, system_prompt_profile) = resolve_prompt_profile(
        &prompt_profiles,
        cli.system_prompt_profile.as_deref(),
        stored_system_prompt_profile.as_deref(),
        config.system_prompt_profile.as_deref(),
        system_prompt_override.is_some(),
    )?;

    let startup_model = model.clone();
    let shared = Arc::new(Mutex::new(Shared {
        model: startup_model.clone(),
        permission_mode: requested_permission_mode,
        pending: HashMap::new(),
        task_permissions: HashMap::new(),
        resolved_permission_requests: HashSet::new(),
        workspace_session: workspace_session.clone(),
        local_documents: local_documents.clone(),
        session_id: session_id.clone(),
        remote_plan: restored_plan,
    }));
    // Workflow agents start under whatever mode the client last set, which
    // may be long after the prompt that launched their run.
    let workflow_mode = Arc::new({
        let shared = Arc::clone(&shared);
        let cwd = cwd.clone();
        move || shared.lock().unwrap().agent_mode(&cwd)
    });
    let handle = smol::block_on(headless::spawn_persistent_interactive(InteractiveParams {
        model,
        config: config.clone(),
        permissions_config,
        decisions_config,
        snapshots,
        timeouts,
        prompt_slots: Arc::new(prompt_slots),
        thinking: thinking.clone(),
        system_prompt_profile,
        system_prompt_profile_name,
        prompt_profiles,
        excluded_tools: vec![QUESTION_TOOL_NAME],
        mcp_handle,
        initial_wd: cwd.clone(),
        session_id,
        session_lease,
        expected_write_version,
        initial_history,
        seed_permission_mode,
        structured_permission_rules,
        session_permission_mode,
        system_prompt_override,
        append_system_prompt: cli.append_system_prompt.clone().filter(|s| !s.is_empty()),
        model_policy: Arc::clone(&model_policy),
        plugin_rules,
        local_tools: Default::default(),
        automations: config
            .features
            .enabled(Feature::Automations)
            .then(|| AutomationParams {
                mode: workflow_mode.clone(),
                fast,
                config: automations,
                clock: Arc::new(SystemClock::new()),
                http: automation_http_client(),
                user_config_dir: None,
            }),
        workflow_mode: Some(workflow_mode),
        workspace_binding: workspace_binding.clone(),
        remote_environment: remote_environment.clone(),
        workspace_session,
        remote_project_context,
        // `cwd` is the sandbox's path once a remote environment exists, so the
        // host checkout has to be named separately.
        host_cwd: remote_environment
            .is_some()
            .then(|| std::env::current_dir().ok())
            .flatten(),
        local_documents,
    }))
    .map_err(|error| eyre!(error))?;
    if let Some(workspace) = handle.remote_workspace_session() {
        shared.lock().unwrap().workspace_session = Some(workspace);
    }
    let permission_mode =
        effective_permission_mode(requested_permission_mode, handle.permissions.mode());
    shared.lock().unwrap().permission_mode = permission_mode;

    let (out_tx, out_rx) = flume::unbounded::<String>();
    let writer_thread = std::thread::spawn(move || {
        let mut stdout = io::stdout().lock();
        while let Ok(line) = out_rx.recv() {
            if writeln!(stdout, "{line}").and(stdout.flush()).is_err() {
                break;
            }
        }
    });

    let writer = SdkWriter {
        session_id: handle.session_id.clone(),
        out_tx,
    };
    let tools: Vec<&str> = handle
        .tool_names
        .iter()
        .map(|t| caudra_to_claude_tool_name(t))
        .collect();
    writer.emit_system(
        "init",
        init_payload(
            serde_json::json!({
                "cwd": working_dir,
                "tools": tools,
                "model": startup_model.id,
                "permissionMode": permission_mode.as_str(),
                "apiKeySource": "none",
                "mcp_servers": sdk_mcp_servers,
                "slash_commands": [],
                "output_style": "default",
            }),
            handle.workflow.is_some(),
            handle.automations.is_some(),
            &config,
            handle.background.is_some(),
        ),
    )?;

    let pump = EventPump {
        run_rx: handle.run_rx.clone(),
        pending_runs: HashMap::new(),
        run: None,
        background: handle.background.clone(),
        writer: writer.clone(),
        shared: Arc::clone(&shared),
        permissions: Arc::clone(&handle.permissions),
        include_partial_messages: cli.include_partial_messages,
        synth: StreamSynth::new(),
        result_text: String::new(),
        cost: None,
        subscription_cost: None,
        auxiliary_usage: TokenUsage::default(),
        request_counter: 0,
    }
    .spawn(handle.event_rx.clone(), handle.automation_events.clone());

    let prompts = PromptContext {
        shared: &shared,
        cwd: &cwd,
        remote: remote_environment.is_some(),
        thinking: &thinking,
        fast,
    };
    let input_result = (|| -> Result<()> {
        for line in io::stdin().lock().lines() {
            let line = line.context("read stdin")?;
            if line.is_empty() {
                continue;
            }

            let msg: InboundMessage = match serde_json::from_str(&line) {
                Ok(msg) => msg,
                Err(e) => {
                    eprintln!("warning: ignoring malformed input line: {e}");
                    continue;
                }
            };

            match msg.msg_type.as_str() {
                "user" => {
                    let Some(user) = parse_or_warn::<InboundUser>(msg.payload, "user message")
                    else {
                        continue;
                    };
                    let content = user.message.content;
                    let prompt = content_text(&content).unwrap_or_else(|| content.to_string());
                    let images = content_images(&content);
                    if images.is_empty()
                        && let Some(args) = prompt
                            .strip_prefix("/remote")
                            .filter(|args| args.is_empty() || args.starts_with(char::is_whitespace))
                    {
                        match smol::block_on(handle.remote_control(args)) {
                            Ok(status) => writer
                                .emit_system("remote", serde_json::json!({ "status": status }))?,
                            Err(error) => writer.emit_system(
                                "remote_error",
                                serde_json::json!({ "error": error }),
                            )?,
                        }
                        continue;
                    }
                    if remote_environment.is_some()
                        && images.is_empty()
                        && let Some(path) = remote_cd_path(&prompt)
                    {
                        match smol::block_on(handle.change_remote_directory(path)) {
                            Ok(cwd) => {
                                shared.lock().unwrap().workspace_session =
                                    handle.remote_workspace_session();
                                writer.emit_system("cwd", serde_json::json!({ "cwd": cwd }))?
                            }
                            Err(error) => writer
                                .emit_system("cwd_error", serde_json::json!({ "error": error }))?,
                        }
                        continue;
                    }
                    if remote_environment.is_some()
                        && images.is_empty()
                        && let Some(command) = headless::direct_shell_command(&prompt)
                    {
                        let started = Instant::now();
                        let output = match handle.remote_workspace_session() {
                            Some(workspace) => smol::block_on(headless::execute_remote_command(
                                &workspace,
                                command,
                                &caudra_agent::CancelToken::none(),
                                max_output_lines,
                                max_output_bytes,
                                |_| {},
                            )),
                            None => headless::RemoteCommandOutput {
                                output: "Remote command execution is unavailable".into(),
                                is_error: true,
                            },
                        };
                        writer.emit_direct_command_result(output, started.elapsed().as_millis())?;
                        continue;
                    }
                    if handle.input_tx.send(prompts.input(prompt, images)).is_err() {
                        break;
                    }
                }
                "control_request" => {
                    let Some(cr) =
                        parse_or_warn::<InboundControlRequest>(msg.payload, "control_request")
                    else {
                        continue;
                    };
                    handle_control_request(
                        &cr,
                        &writer,
                        &handle,
                        &prompts,
                        &startup_model,
                        &model_policy,
                    )?;
                }
                "control_response" => {
                    let Some(cr) =
                        parse_or_warn::<InboundControlResponse>(msg.payload, "control_response")
                    else {
                        continue;
                    };
                    answer_permission_response(&shared, &handle.permissions, cr.response);
                }
                "control_cancel_request" => {
                    let Some(ccr) = parse_or_warn::<InboundControlCancelRequest>(
                        msg.payload,
                        "control_cancel_request",
                    ) else {
                        continue;
                    };
                    answer_pending_permission(
                        &shared,
                        &handle.permissions,
                        &ccr.request_id,
                        PermissionAnswer::Deny,
                    );
                }
                other => warn!("unknown inbound message type: {other}"),
            }
        }

        Ok(())
    })();
    let InteractiveHandle {
        input_tx,
        cancel_tx,
        task,
        ..
    } = handle;
    drop(input_tx);
    let _ = cancel_tx.try_send(());
    smol::block_on(async {
        task.await;
        pump.await;
    });
    drop(writer);
    let _ = writer_thread.join();
    input_result
}

fn remote_cd_path(prompt: &str) -> Option<&str> {
    let prompt = prompt.trim();
    if prompt == "/cd" {
        return Some(".");
    }
    prompt
        .strip_prefix("/cd")
        .filter(|rest| rest.starts_with(char::is_whitespace))
        .map(str::trim)
        .filter(|path| !path.is_empty())
}

struct ResolvedSession {
    session_id: SessionRef,
    session_lease: Arc<SessionLease>,
    expected_write_version: Option<i64>,
    initial_history: Vec<HistoryItem>,
    structured_permission_rules: Vec<PermissionRuleRecord>,
    session_permission_mode: Option<StoredPermissionMode>,
    stored_system_prompt_profile: Option<String>,
    restored_plan: Option<PlanRef>,
    restored_legacy_plan: Option<PathBuf>,
    restored_plan_mode: bool,
}

fn restored_plan(session: &StoredSession) -> Option<PlanRef> {
    match &session.meta.plan_target {
        Some(StoredPlanTarget::PlanRef { reference }) => Some(reference.clone()),
        _ => None,
    }
}

fn session_permissions(
    session: &StoredSession,
    fork: bool,
) -> (Vec<PermissionRuleRecord>, Option<StoredPermissionMode>) {
    if fork {
        (Vec::new(), None)
    } else {
        (
            session.meta.structured_permission_rules.clone(),
            session.meta.permission_mode.clone(),
        )
    }
}

fn resolve_session(
    cli: &Cli,
    cwd: &str,
    prompt_profiles: &PromptProfileCatalog,
    configured_profile: Option<&str>,
    raw_prompt_override: bool,
    workspace_binding: Option<&StoredWorkspaceBinding>,
) -> Result<ResolvedSession> {
    let storage = StateDir::resolve().context("resolve state dir")?;
    let cli_session_id = cli
        .session_id
        .as_deref()
        .map(|session_id| {
            session_id
                .parse::<SessionRef>()
                .map_err(|error| eyre!("invalid session id {session_id:?}: {error}"))
        })
        .transpose()?;

    if let Some(id) = &cli.session {
        let session_ref: SessionRef = id
            .parse()
            .map_err(|e| eyre!("invalid session id {id}: {e}"))?;
        let source_lease = if cli.fork_session {
            None
        } else {
            Some(Arc::new(SessionLease::acquire(&storage, session_ref.id())?))
        };
        let session = crate::setup::load_session(session_ref.id(), &storage)
            .map_err(|e| eyre!("load session {id}: {e}"))?;
        if !cli.fork_session {
            StoredWorkspaceBinding::validate_resume_identity(
                session.workspace_binding(),
                workspace_binding,
            )?;
        }
        let history = crate::setup::active_session_history(&session)
            .map_err(|e| eyre!("load active history for session {id}: {e}"))?;
        if cli.fork_session {
            resolve_prompt_profile(
                prompt_profiles,
                cli.system_prompt_profile.as_deref(),
                session.meta.system_prompt_profile.as_deref(),
                configured_profile,
                raw_prompt_override,
            )?;
            let (structured_permission_rules, session_permission_mode) =
                session_permissions(&session, true);
            let target = cli_session_id.clone().unwrap_or_else(SessionRef::generate);
            if target.id() == session_ref.id() {
                return Err(eyre!(
                    "fork session ID must differ from source session {id}"
                ));
            }
            let session_lease = Arc::new(SessionLease::acquire(&storage, target.id())?);
            ensure_fork_target_available(&storage, &target)?;
            let mut reachable = reachable_subagent_ids(&history, &session);
            let mut versions =
                caudra_agent::active_task_history_versions_with_outputs(&history, |call_id| {
                    session.tool_outputs().get(call_id).map(Arc::as_ref)
                });
            reachable.extend(versions.keys().cloned());
            for subagent in session.subagents() {
                if reachable.contains(&subagent.tool_use_id)
                    && !versions.contains_key(&subagent.tool_use_id)
                    && let Some(version_id) = &subagent.parent_tool_use_id
                    && session.subagent_messages().contains_key(version_id)
                {
                    versions.insert(subagent.tool_use_id.clone(), version_id.clone());
                }
            }
            let version_ids: HashSet<&str> = versions
                .iter()
                .filter_map(|(task_id, version_id)| {
                    (task_id != version_id && session.subagent_messages().contains_key(version_id))
                        .then_some(version_id.as_str())
                })
                .collect();
            let history = rebase_history(history)?;
            let subagent_histories = reachable
                .iter()
                .filter(|task_id| !version_ids.contains(task_id.as_str()))
                .filter_map(|task_id| {
                    let version_id = versions.get(task_id).unwrap_or(task_id);
                    session
                        .subagent_messages()
                        .get(version_id)
                        .map(|items| (task_id, items))
                })
                .map(|(task_id, items)| {
                    rebase_history(items.as_ref().clone()).map(|history| (task_id.clone(), history))
                })
                .collect::<Result<HashMap<_, _>>>()?;
            copy_history_outputs(
                &storage,
                &session_ref,
                &target,
                std::iter::once(history.as_slice())
                    .chain(subagent_histories.values().map(Vec::as_slice)),
            )?;
            if let Err(error) = save_sdk_fork(
                &storage,
                &session,
                &target,
                &history,
                subagent_histories,
                workspace_binding,
                cwd,
            ) {
                let _ = ToolOutputStore::new(storage.clone()).delete_session(target.id());
                return Err(error);
            }
            return Ok(ResolvedSession {
                session_id: target,
                session_lease,
                expected_write_version: Some(0),
                initial_history: history,
                structured_permission_rules,
                session_permission_mode,
                stored_system_prompt_profile: session.meta.system_prompt_profile.clone(),
                restored_plan: None,
                restored_legacy_plan: None,
                restored_plan_mode: false,
            });
        }

        if cli_session_id
            .as_ref()
            .is_some_and(|target| target.id() != session_ref.id())
        {
            return Err(eyre!(
                "--session-id cannot replace the resumed session ID without --fork-session"
            ));
        }
        let (structured_permission_rules, session_permission_mode) =
            session_permissions(&session, false);
        return Ok(ResolvedSession {
            session_id: session_ref,
            session_lease: source_lease.expect("non-fork resume has a lease"),
            expected_write_version: session.persisted_write_version(),
            initial_history: history,
            structured_permission_rules,
            session_permission_mode,
            stored_system_prompt_profile: session.meta.system_prompt_profile.clone(),
            restored_plan: restored_plan(&session),
            restored_legacy_plan: session
                .meta
                .plan_target
                .as_ref()
                .and_then(|target| match target {
                    StoredPlanTarget::LocalPath { path } => Some(PathBuf::from(path)),
                    StoredPlanTarget::PlanRef { .. } => None,
                })
                .or_else(|| session.meta.plan_path.as_deref().map(PathBuf::from)),
            restored_plan_mode: session.meta.mode == Some(StoredMode::Plan),
        });
    }

    let summaries = if let Some(binding) = workspace_binding {
        caudra_storage::sessions::SessionDatabase::open_state(&storage)?
            .list_for_workspace_identity(binding)?
    } else {
        StoredSession::list(cwd, &storage)?
    };
    if cli.continue_session
        && let Some(summary) = summaries.into_iter().next()
    {
        let session_ref = SessionRef::from(summary.id);
        let session_lease = Arc::new(SessionLease::acquire(&storage, summary.id)?);
        let session = crate::setup::load_session(summary.id, &storage)?;
        StoredWorkspaceBinding::validate_resume_identity(
            session.workspace_binding(),
            workspace_binding,
        )?;
        let history = crate::setup::active_session_history(&session)?;
        let (structured_permission_rules, session_permission_mode) =
            session_permissions(&session, false);
        return Ok(ResolvedSession {
            session_id: session_ref,
            session_lease,
            expected_write_version: session.persisted_write_version(),
            initial_history: history,
            structured_permission_rules,
            session_permission_mode,
            stored_system_prompt_profile: session.meta.system_prompt_profile.clone(),
            restored_plan: restored_plan(&session),
            restored_legacy_plan: session
                .meta
                .plan_target
                .as_ref()
                .and_then(|target| match target {
                    StoredPlanTarget::LocalPath { path } => Some(PathBuf::from(path)),
                    StoredPlanTarget::PlanRef { .. } => None,
                })
                .or_else(|| session.meta.plan_path.as_deref().map(PathBuf::from)),
            restored_plan_mode: session.meta.mode == Some(StoredMode::Plan),
        });
    }

    let session_id = cli_session_id.unwrap_or_else(SessionRef::generate);
    let session_lease = Arc::new(SessionLease::acquire(&storage, session_id.id())?);
    Ok(ResolvedSession {
        session_id,
        session_lease,
        expected_write_version: None,
        initial_history: Vec::new(),
        structured_permission_rules: Vec::new(),
        session_permission_mode: None,
        stored_system_prompt_profile: None,
        restored_plan: None,
        restored_legacy_plan: None,
        restored_plan_mode: false,
    })
}

#[cfg(test)]
fn validate_workspace_binding(
    session: &StoredSession,
    expected: Option<&StoredWorkspaceBinding>,
    allow_rebind: bool,
) -> Result<()> {
    if !allow_rebind {
        StoredWorkspaceBinding::validate_resume(session.workspace_binding(), expected)?;
    }
    Ok(())
}

fn resolve_prompt_profile(
    catalog: &PromptProfileCatalog,
    cli_name: Option<&str>,
    stored_name: Option<&str>,
    configured_name: Option<&str>,
    raw_prompt_override: bool,
) -> Result<(
    Option<String>,
    Option<Arc<caudra_agent::prompt::profile::SystemPromptProfile>>,
)> {
    if raw_prompt_override {
        return Ok((None, None));
    }
    let requested_name = cli_name.or(stored_name).or(configured_name);
    let profile = catalog
        .resolve(requested_name)
        .context("resolve system prompt profile")?;
    Ok((
        Some(requested_name.unwrap_or(BUILTIN_PROFILE_NAME).to_owned()),
        profile,
    ))
}

fn startup_permission_mode(
    cli: &Cli,
    requested: PermissionMode,
    restored: Option<StoredPermissionMode>,
) -> Option<StoredPermissionMode> {
    if cli.permission_mode.is_some() || cli.yolo || cli.auto {
        Some(requested.storage_mode())
    } else {
        restored
    }
}

fn effective_permission_mode(
    requested: PermissionMode,
    mode: StoredPermissionMode,
) -> PermissionMode {
    match (requested, mode) {
        (PermissionMode::Plan, _) => PermissionMode::Plan,
        (_, StoredPermissionMode::Yolo) => PermissionMode::BypassPermissions,
        (_, StoredPermissionMode::Auto) => PermissionMode::Auto,
        (PermissionMode::AcceptEdits, StoredPermissionMode::Ask) => PermissionMode::AcceptEdits,
        (_, StoredPermissionMode::Ask) => PermissionMode::Default,
    }
}

fn rebase_history(items: Vec<HistoryItem>) -> Result<Vec<HistoryItem>> {
    let messages = History::restored(items)?.into_vec();
    Ok(History::new(messages).into_items())
}

fn copy_history_outputs<'a>(
    storage: &StateDir,
    source: &SessionRef,
    target: &SessionRef,
    histories: impl IntoIterator<Item = &'a [HistoryItem]>,
) -> Result<()> {
    let mut seen = HashSet::new();
    let mut references = Vec::new();
    for item in histories.into_iter().flatten() {
        let output_refs: &[ToolOutputRef] = match &item.kind {
            HistoryItemKind::ToolResult {
                output_ref: Some(output_ref),
                ..
            } => std::slice::from_ref(output_ref),
            HistoryItemKind::AssistantText {
                retained_output_refs,
                ..
            }
            | HistoryItemKind::User {
                retained_output_refs,
                ..
            } => retained_output_refs,
            _ => &[],
        };
        for output_ref in output_refs {
            if seen.insert(output_ref.id.clone()) {
                references.push(output_ref.clone());
            }
        }
    }
    ToolOutputStore::new(storage.clone())
        .copy_session_outputs(source.id(), target.id(), &references)
        .map_err(|error| eyre!("copy tool outputs from session {source} to fork {target}: {error}"))
}

fn ensure_fork_target_available(storage: &StateDir, target: &SessionRef) -> Result<()> {
    match caudra_agent::load_stored_session(target.id(), storage) {
        Ok(_) => Err(eyre!("fork target session {target} already exists")),
        Err(SessionError::Storage(StorageError::NotFound(_))) => Ok(()),
        Err(error) => Err(eyre!(
            "check whether fork target session {target} exists: {error}"
        )),
    }
}

fn save_sdk_fork(
    storage: &StateDir,
    source: &StoredSession,
    target: &SessionRef,
    history: &[HistoryItem],
    subagent_histories: HashMap<String, Vec<HistoryItem>>,
    workspace_binding: Option<&StoredWorkspaceBinding>,
    cwd: &str,
) -> Result<()> {
    let mut fork = workspace_binding.map_or_else(
        || StoredSession::new(&source.model, cwd),
        |binding| StoredSession::new_with_workspace(&source.model, cwd, binding.clone()),
    );
    fork.id = target.id();
    fork.meta.system_prompt_profile = source.meta.system_prompt_profile.clone();
    fork.replace_messages(history.to_vec());
    fork.set_title(format!("{} (fork)", source.title));
    let mut stored_tool_ids = all_tool_call_ids(history);
    for history in subagent_histories.values() {
        stored_tool_ids.extend(all_tool_call_ids(history));
    }
    let copied_task_ids: HashSet<String> = subagent_histories.keys().cloned().collect();
    let versions = caudra_agent::active_task_history_versions_with_outputs(history, |call_id| {
        source.tool_outputs().get(call_id).map(Arc::as_ref)
    });
    for (task_id, history) in subagent_histories {
        if let Some(version_id) = versions
            .get(&task_id)
            .filter(|version| *version != &task_id)
        {
            fork.set_subagent_history(
                version_id.clone(),
                history.clone(),
                Some(StoredSubagentTaskSpec::version()),
            );
        }
        let spec = source.subagent_task_specs().get(&task_id).cloned();
        fork.set_subagent_history(task_id, history, spec);
    }
    fork.set_subagents(
        source
            .subagents()
            .iter()
            .filter(|subagent| copied_task_ids.contains(&subagent.tool_use_id))
            .cloned()
            .collect(),
    );
    for tool_id in stored_tool_ids {
        if let Some(output) = source.tool_outputs().get(&tool_id) {
            fork.insert_tool_output(tool_id, output.as_ref().clone());
        }
    }
    fork.save(storage)
        .map_err(|error| eyre!("save fork session {target}: {error}"))
}

fn reachable_subagent_ids(history: &[HistoryItem], session: &StoredSession) -> HashSet<String> {
    let mut reachable = history_task_ids(history);
    let mut active_calls = caudra_agent::history_tool_call_ids(history);
    expand_reachable_subagents(&mut reachable, &mut active_calls, session);
    let legacy_fallback = reachable.is_empty()
        || history.iter().any(|item| {
            matches!(
                &item.kind,
                HistoryItemKind::AssistantText {
                    retained_subagent_ids,
                    is_compaction_summary: true,
                    ..
                } if retained_subagent_ids.is_empty()
            )
        });
    reachable.extend(
        session
            .subagent_task_specs()
            .iter()
            .filter(|(task_id, spec)| {
                if !spec.is_generic() {
                    return false;
                }
                let descriptor = session
                    .subagents()
                    .iter()
                    .find(|subagent| subagent.tool_use_id == **task_id);
                descriptor.is_none()
                    || active_calls.contains(*task_id)
                    || descriptor
                        .and_then(|subagent| subagent.root_tool_use_id.as_ref())
                        .is_some_and(|root| active_calls.contains(root))
            })
            .map(|(task_id, _)| task_id.clone()),
    );
    if legacy_fallback {
        reachable.extend(
            session
                .subagent_messages()
                .keys()
                .filter(|task_id| !session.subagent_task_specs().contains_key(*task_id))
                .filter(|task_id| {
                    !session.subagents().iter().any(|subagent| {
                        subagent.tool_use_id.as_str() != task_id.as_str()
                            && subagent.parent_tool_use_id.as_ref() == Some(*task_id)
                    })
                })
                .cloned(),
        );
    }
    expand_reachable_subagents(&mut reachable, &mut active_calls, session);
    reachable
}

fn expand_reachable_subagents(
    reachable: &mut HashSet<String>,
    active_calls: &mut HashSet<String>,
    session: &StoredSession,
) {
    let mut visited = HashSet::new();
    loop {
        for subagent in session.subagents() {
            if subagent
                .parent_tool_use_id
                .as_ref()
                .is_some_and(|parent| reachable.contains(parent) || active_calls.contains(parent))
                || subagent
                    .root_tool_use_id
                    .as_ref()
                    .is_some_and(|root| active_calls.contains(root))
            {
                reachable.insert(subagent.tool_use_id.clone());
            }
        }
        let Some(task_id) = reachable
            .iter()
            .find(|task_id| !visited.contains(*task_id))
            .cloned()
        else {
            break;
        };
        visited.insert(task_id.clone());
        if let Some(state) = session
            .tool_outputs()
            .get(&task_id)
            .and_then(|output| output.state())
        {
            collect_task_metadata_from_value(state, reachable);
        }
        if let Some(nested) = session.subagent_messages().get(&task_id) {
            reachable.extend(history_task_ids(nested));
            active_calls.extend(caudra_agent::history_tool_call_ids(nested));
        }
    }
}

fn history_task_ids(items: &[HistoryItem]) -> HashSet<String> {
    let mut ids = HashSet::new();
    for item in items {
        match &item.kind {
            HistoryItemKind::ToolCall { call_id, name, .. }
                if name == "task" || name == "batch" =>
            {
                ids.insert(call_id.clone());
            }
            HistoryItemKind::AssistantText {
                retained_subagent_ids,
                ..
            } => ids.extend(retained_subagent_ids.iter().cloned()),
            _ => {}
        }
    }
    ids
}

fn all_tool_call_ids(items: &[HistoryItem]) -> HashSet<String> {
    items
        .iter()
        .filter_map(|item| match &item.kind {
            HistoryItemKind::ToolCall { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect()
}

fn collect_task_metadata_from_value(value: &Value, ids: &mut HashSet<String>) {
    match value {
        Value::String(text) => collect_task_metadata(text, ids),
        Value::Array(values) => {
            for value in values {
                collect_task_metadata_from_value(value, ids);
            }
        }
        Value::Object(values) => {
            if let Some(tool) = values.get("tool").and_then(Value::as_str) {
                if tool == "task" {
                    if let Some(invocation_id) = values.get("invocation_id").and_then(Value::as_str)
                    {
                        ids.insert(invocation_id.to_owned());
                    }
                    if let Some(output) = values.get("output").and_then(Value::as_str) {
                        collect_task_metadata(output, ids);
                    }
                }
                return;
            }
            for value in values.values() {
                collect_task_metadata_from_value(value, ids);
            }
        }
        _ => {}
    }
}

fn collect_task_metadata(content: &str, ids: &mut HashSet<String>) {
    for block in content.split("<task_metadata>").skip(1) {
        let Some(metadata) = block.split("</task_metadata>").next() else {
            continue;
        };
        if let Some(task_id) = metadata
            .lines()
            .find_map(|line| line.trim().strip_prefix("task_id: "))
        {
            ids.insert(task_id.to_owned());
        }
    }
}

fn parse_or_warn<T: serde::de::DeserializeOwned>(payload: Value, what: &str) -> Option<T> {
    match serde_json::from_value(payload) {
        Ok(v) => Some(v),
        Err(e) => {
            eprintln!("warning: ignoring malformed {what}: {e}");
            None
        }
    }
}

fn content_text(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(blocks) => Some(
            blocks
                .iter()
                .filter_map(|b| {
                    (b.get("type").and_then(Value::as_str) == Some("text"))
                        .then(|| b.get("text").and_then(Value::as_str))
                        .flatten()
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => None,
    }
}

// Claude Code stream-json block shape:
// {"type":"image","source":{"type":"base64","media_type":"image/png","data":"..."}}
// `source` deserializes straight into ImageSource; malformed blocks are skipped.
fn content_images(content: &Value) -> Vec<ImageSource> {
    let Value::Array(blocks) = content else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("image"))
        .filter_map(|b| serde_json::from_value::<ImageSource>(b.get("source")?.clone()).ok())
        .collect()
}

fn handle_control_request(
    cr: &InboundControlRequest,
    writer: &SdkWriter,
    handle: &InteractiveHandle,
    prompts: &PromptContext,
    startup_model: &Model,
    model_policy: &ModelPolicy,
) -> Result<()> {
    let ok = Some(Value::Object(Default::default()));
    match cr.request.subtype.as_str() {
        "initialize" => {
            if let Some(extra) = cr.request.extra.as_object()
                && (extra.contains_key("hooks") || extra.contains_key("agents"))
            {
                eprintln!("note: hooks/agents payloads are ignored");
            }
            writer.emit_control_response(
                &cr.request_id,
                Some(serde_json::json!({"commands": []})),
                None,
            )
        }
        "interrupt" => match smol::block_on(handle.interrupt()) {
            Ok(()) => writer.emit_control_response(&cr.request_id, ok, None),
            Err(error) => writer.emit_control_response(&cr.request_id, None, Some(error)),
        },
        subtype if TASK_CONTROLS.contains(&subtype) => {
            handle_task_control_request(cr, writer, handle.background.as_ref())
        }
        "set_permission_mode" => {
            let mode_str = cr.request.extra.get("mode").and_then(Value::as_str);
            match mode_str.and_then(PermissionMode::parse) {
                Some(PermissionMode::Auto) if !handle.permissions.decision_engine() => writer
                    .emit_control_response(
                        &cr.request_id,
                        None,
                        Some(FeatureDisabled(Feature::DecisionEngine).to_string()),
                    ),
                Some(mode) => {
                    let execution_mode =
                        mode.preserve_plan(prompts.shared.lock().unwrap().permission_mode);
                    let agent_mode = prompts
                        .shared
                        .lock()
                        .unwrap()
                        .agent_mode_for(execution_mode, &handle.permissions.project_cwd());
                    match smol::block_on(handle.set_mode(agent_mode, mode.storage_mode())) {
                        Ok(()) => {
                            prompts.shared.lock().unwrap().permission_mode = execution_mode;
                            writer.emit_control_response(&cr.request_id, ok, None)
                        }
                        Err(error) => {
                            writer.emit_control_response(&cr.request_id, None, Some(error))
                        }
                    }
                }
                None => writer.emit_control_response(
                    &cr.request_id,
                    None,
                    Some(format!(
                        "invalid permission mode: {}",
                        mode_str.unwrap_or("<missing>")
                    )),
                ),
            }
        }
        "set_model" => {
            match resolve_set_model(cr.request.extra.get("model"), startup_model, model_policy) {
                Some(model) => match smol::block_on(handle.set_model(model)) {
                    Ok(model) => {
                        prompts.shared.lock().unwrap().model = model;
                        writer.emit_control_response(&cr.request_id, ok, None)
                    }
                    Err(error) => writer.emit_control_response(
                        &cr.request_id,
                        None,
                        Some(error.user_message()),
                    ),
                },
                None => writer.emit_control_response(
                    &cr.request_id,
                    None,
                    Some("invalid or disallowed model".into()),
                ),
            }
        }
        GOAL_SET => writer.emit_answer(
            &cr.request_id,
            goal_set(&cr.request.extra, &handle.goal, prompts, &handle.input_tx),
        ),
        GOAL_CLEAR => writer.emit_answer(&cr.request_id, goal_clear(&handle.goal)),
        GOAL_STATUS => writer.emit_answer(&cr.request_id, goal_status(&handle.goal)),
        other => {
            if let Some(request) = automation_request(other, &cr.request.extra) {
                return answer_automation_control(
                    writer,
                    &cr.request_id,
                    request,
                    handle.automations.as_ref(),
                );
            }
            match workflow_request(other, &cr.request.extra) {
                Some(Ok(request)) => {
                    forward_control(
                        writer,
                        &cr.request_id,
                        handle.workflow_control(request),
                        workflow_control_response,
                    );
                    Ok(())
                }
                Some(Err(message)) => {
                    writer.emit_control_response(&cr.request_id, None, Some(message))
                }
                None => writer.emit_control_response(
                    &cr.request_id,
                    None,
                    Some(format!("unsupported: {other}")),
                ),
            }
        }
    }
}

fn handle_task_control_request(
    cr: &InboundControlRequest,
    writer: &SdkWriter,
    tasks: Option<&BackgroundTasks>,
) -> Result<()> {
    let result = smol::block_on(async {
        let tasks = tasks.ok_or("Background tasks unavailable".to_owned())?;
        let subtype = cr.request.subtype.as_str();
        if subtype == "task_list" {
            return serde_json::to_value(tasks.list()).map_err(|error| error.to_string());
        }
        let id = cr
            .request
            .extra
            .get("task_id")
            .and_then(Value::as_str)
            .ok_or("task_id is required".to_owned())?;
        let generation = tasks.generation();
        let status = tasks.status(id)?;
        if cr
            .request
            .extra
            .get("invocation_id")
            .is_some_and(|invocation| invocation.as_str() != Some(&status.invocation_id))
            || cr
                .request
                .extra
                .get("generation")
                .is_some_and(|expected| expected.as_u64() != Some(generation))
        {
            return Err(STALE_TASK_INVOCATION.into());
        }
        let status = match subtype {
            "task_cancel" => {
                tasks
                    .cancel_invocation(id, &status.invocation_id, generation)
                    .await?
            }
            "task_promote" => {
                tasks
                    .promote_invocation(id, &status.invocation_id, generation)
                    .await?
            }
            _ => status,
        };
        serde_json::to_value(status).map_err(|error| error.to_string())
    });
    match result {
        Ok(status) => writer.emit_control_response(&cr.request_id, Some(status), None),
        Err(error) => writer.emit_control_response(&cr.request_id, None, Some(error)),
    }
}

/// `/goal <condition>` for a client. The condition is validated as `/goal`
/// validates it, the limit clamped as the TUI clamps it, and an active goal
/// replaced as `/goal` replaces it. Unless `kickoff` is false, the kickoff
/// goes out as `/goal` sends it, along the path a typed prompt takes.
fn goal_set(
    extra: &Value,
    goal: &GoalHandle,
    prompts: &PromptContext,
    input_tx: &Sender<AgentInput>,
) -> Result<Value, String> {
    let request =
        GoalSetRequest::deserialize(extra).map_err(|error| format!("{GOAL_SET}: {error}"))?;
    let active = goal
        .set(&request.condition)
        .map_err(|error| error.to_string())?;
    if let Some(limit) = request.continuation_limit {
        goal.set_continuation_limit(limit);
    }
    if request.kickoff.unwrap_or(true) {
        let mut input = prompts.input(active.condition.to_string(), Vec::new());
        input
            .preamble
            .push(Message::synthetic(goal_kickoff_message(&active.condition)));
        input_tx
            .send(input)
            .map_err(|_| SESSION_CLOSED.to_owned())?;
    }
    goal_status(goal)
}

/// `/goal clear`: the active goal stops, and a finished one stays on record.
fn goal_clear(goal: &GoalHandle) -> Result<Value, String> {
    let cleared = goal.clear();
    serde_json::to_value(GoalClearReply {
        cleared: cleared.as_ref().map(|goal| &*goal.condition),
    })
    .map_err(|error| error.to_string())
}

fn goal_status(goal: &GoalHandle) -> Result<Value, String> {
    let status = goal.status();
    let (status, report) = match &status {
        Some(GoalStatus::Active(goal)) => (GOAL_ACTIVE, Some(GoalReport::from(goal))),
        Some(GoalStatus::Finished(result)) => (GOAL_FINISHED, Some(GoalReport::from(result))),
        None => (NO_GOAL, None),
    };
    serde_json::to_value(GoalStatusReply {
        status,
        goal: report,
        continuation_limit: goal.continuation_limit(),
    })
    .map_err(|error| error.to_string())
}

/// The init message with what the session can do beyond the Claude Code
/// shape: `workflows` and `automations` say whether those runtimes are
/// attached, and `workflow_controls`, `automation_controls` and
/// `goal_controls` name the `control_request` subtypes the runtimes and the
/// session goal answer.
fn init_payload(
    mut payload: Value,
    workflows: bool,
    automations: bool,
    config: &AgentConfig,
    jobs: bool,
) -> Value {
    let workflow_controls: &[&str] = if workflows { WORKFLOW_CONTROLS } else { &[] };
    let automation_controls: &[&str] = if automations {
        AUTOMATION_CONTROLS
    } else {
        &[]
    };
    payload["workflows"] = Value::Bool(workflows);
    payload["workflow_controls"] = serde_json::json!(workflow_controls);
    payload["automations"] = Value::Bool(automations);
    payload["automation_controls"] = serde_json::json!(automation_controls);
    payload["goal_controls"] = serde_json::json!(GOAL_CONTROLS);
    let task_mode = effective_task_execution(config, jobs);
    let shell_mode = effective_shell_execution(config, jobs);
    let background_capable = |mode: &Option<ExecutionMode>| {
        jobs && matches!(mode, Some(ExecutionMode::Auto | ExecutionMode::Async))
    };
    payload["background_tasks"] = Value::Bool(background_capable(&task_mode));
    payload["background_shell"] = Value::Bool(background_capable(&shell_mode));
    payload["background_jobs"] = Value::Bool(jobs);
    payload["task_execution"] =
        serde_json::json!({"configured": config.task_execution, "effective": task_mode});
    payload["shell_execution"] = serde_json::json!({"configured": config.shell_execution, "effective": shell_mode, "async_threshold_secs": config.shell_async_threshold_secs});
    payload["job_kinds"] = if jobs {
        serde_json::json!(["agent", "shell"])
    } else {
        serde_json::json!([])
    };
    payload["task_controls"] = serde_json::json!(
        TASK_CONTROLS
            .iter()
            .filter(|control| jobs
                && (**control != "task_promote" || task_mode == Some(ExecutionMode::Auto)))
            .collect::<Vec<_>>()
    );
    payload
}

/// `None` when `subtype` is not a workflow control; `Some(Err)` names the
/// field a workflow control lacks.
fn workflow_request(subtype: &str, extra: &Value) -> Option<Result<WorkflowRequest, String>> {
    let text = |field: &str| {
        extra
            .get(field)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("{subtype} requires a string {field}"))
    };
    let agent_budget = || {
        extra
            .get("agent_budget")
            .and_then(Value::as_u64)
            .map(|budget| u32::try_from(budget).unwrap_or(u32::MAX))
    };
    let request = match subtype {
        WORKFLOW_LIST => Ok(WorkflowRequest::List),
        WORKFLOW_VALIDATE => text("name").map(|name| WorkflowRequest::Validate { name }),
        WORKFLOW_START => text("name").map(|name| {
            WorkflowRequest::Start(LaunchRequest {
                name,
                args: match extra.get("args") {
                    Some(args @ Value::Object(_)) => args.clone(),
                    _ => Value::Object(serde_json::Map::new()),
                },
                agent_budget: agent_budget(),
            })
        }),
        WORKFLOW_STATUS => Ok(WorkflowRequest::Status {
            run_id: text("run_id").ok(),
        }),
        WORKFLOW_INSPECT => text("run_id").map(|run_id| WorkflowRequest::Inspect { run_id }),
        WORKFLOW_HISTORY => Ok(WorkflowRequest::History {
            limit: extra
                .get("limit")
                .and_then(Value::as_u64)
                .map(|limit| usize::try_from(limit).unwrap_or(usize::MAX)),
        }),
        WORKFLOW_PAUSE => text("run_id").map(|run_id| WorkflowRequest::Pause { run_id }),
        WORKFLOW_RESUME => text("run_id").map(|run_id| WorkflowRequest::Resume {
            run_id,
            agent_budget: agent_budget(),
        }),
        WORKFLOW_STOP => text("run_id").map(|run_id| WorkflowRequest::Stop { run_id }),
        WORKFLOW_TRUST => text("name")
            .and_then(|name| text("digest").map(|digest| WorkflowRequest::Trust { name, digest })),
        WORKFLOW_ACK => text("run_id").and_then(|run_id| {
            extra
                .get("revision")
                .and_then(Value::as_u64)
                .map(|revision| WorkflowRequest::AckCompletion { run_id, revision })
                .ok_or_else(|| format!("{subtype} requires an integer revision"))
        }),
        _ => return None,
    };
    Some(request)
}

/// Answers off the stdin thread: a workflow pause or stop waits for the run's
/// agents to stop, and the client's permission replies must keep flowing
/// meanwhile.
fn forward_control<T: 'static>(
    writer: &SdkWriter,
    request_id: &str,
    answer: impl Future<Output = T> + Send + 'static,
    reply: fn(T) -> (Option<Value>, Option<String>),
) {
    let writer = writer.clone();
    let request_id = request_id.to_owned();
    smol::spawn(async move {
        let (response, error) = reply(answer.await);
        if let Err(error) = writer.emit_control_response(&request_id, response, error) {
            warn!(%error, request_id, "control response not delivered");
        }
    })
    .detach();
}

/// How a control field reads, and what a refusal calls its type.
struct FieldKind<T> {
    name: &'static str,
    read: fn(&Value) -> Option<T>,
}

/// One control's fields, read so that a refusal names the control and the
/// field.
struct ControlFields<'a> {
    subtype: &'a str,
    extra: &'a Value,
}

impl ControlFields<'_> {
    /// An absent or null field reads as `None`; any other value must be of
    /// `kind`.
    fn optional<T>(&self, field: &str, kind: FieldKind<T>) -> Result<Option<T>, String> {
        match self.extra.get(field) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => (kind.read)(value)
                .map(Some)
                .ok_or_else(|| field_refusal(self.subtype, field, kind.name)),
        }
    }

    fn required<T>(&self, field: &str, kind: FieldKind<T>) -> Result<T, String> {
        let name = kind.name;
        self.optional(field, kind)?
            .ok_or_else(|| field_refusal(self.subtype, field, name))
    }
}

fn field_refusal(subtype: &str, field: &str, kind: &str) -> String {
    format!("{subtype} requires {kind} {field}")
}

/// `None` when `subtype` is not an automation control; `Some(Err)` names the
/// field an automation control lacks, or carries as another type.
fn automation_request(subtype: &str, extra: &Value) -> Option<Result<AutomationRequest, String>> {
    let fields = ControlFields { subtype, extra };
    let request = || -> Result<Option<AutomationRequest>, String> {
        Ok(Some(match subtype {
            AUTOMATION_LIST => AutomationRequest::List,
            AUTOMATION_VALIDATE => AutomationRequest::Validate {
                name: fields.required(NAME_FIELD, STRING_KIND)?,
            },
            AUTOMATION_ARM => AutomationRequest::Arm {
                name: fields.required(NAME_FIELD, STRING_KIND)?,
                args: fields.optional(ARGS_FIELD, OBJECT_KIND)?,
                origin: ArmOrigin::Sdk,
            },
            AUTOMATION_DISARM => AutomationRequest::Disarm {
                name: fields.required(NAME_FIELD, STRING_KIND)?,
            },
            AUTOMATION_TRUST => AutomationRequest::Trust {
                name: fields.required(NAME_FIELD, STRING_KIND)?,
                digest: fields.required(DIGEST_FIELD, STRING_KIND)?,
            },
            AUTOMATION_INSPECT => AutomationRequest::Inspect {
                name: fields.required(NAME_FIELD, STRING_KIND)?,
                session_id: fields.optional(SESSION_ID_FIELD, STRING_KIND)?,
            },
            AUTOMATION_HISTORY => AutomationRequest::History {
                name: fields.optional(NAME_FIELD, STRING_KIND)?,
                fire_id: fields.optional(FIRE_ID_FIELD, STRING_KIND)?,
                limit: fields
                    .optional(LIMIT_FIELD, INTEGER_KIND)?
                    .map(|limit| usize::try_from(limit).unwrap_or(usize::MAX)),
            },
            AUTOMATION_FIRING => AutomationRequest::Firing {
                fire_id: fields.required(FIRE_ID_FIELD, STRING_KIND)?,
            },
            AUTOMATION_DRY_RUN => AutomationRequest::DryRun {
                fire_id: fields.required(FIRE_ID_FIELD, STRING_KIND)?,
            },
            AUTOMATION_SET_ARGS => AutomationRequest::SetArgs {
                name: fields.required(NAME_FIELD, STRING_KIND)?,
                args: fields.required(ARGS_FIELD, OBJECT_KIND)?,
            },
            AUTOMATION_SET_STATE => AutomationRequest::SetState {
                name: fields.required(NAME_FIELD, STRING_KIND)?,
                state: fields.required(STATE_FIELD, OBJECT_KIND)?,
                expected_revision: fields.required(EXPECTED_REVISION_FIELD, INTEGER_KIND)?,
            },
            AUTOMATION_CLEAR_STATE => AutomationRequest::ClearState {
                name: fields.required(NAME_FIELD, STRING_KIND)?,
                expected_revision: fields.required(EXPECTED_REVISION_FIELD, INTEGER_KIND)?,
            },
            AUTOMATION_DROP => {
                let fire_id = fields.required(FIRE_ID_FIELD, STRING_KIND)?;
                AutomationRequest::Drop(match fields.optional(SEQ_FIELD, INTEGER_KIND)? {
                    Some(seq) => DropTarget::OutboxItem { fire_id, seq },
                    None => DropTarget::Firing { fire_id },
                })
            }
            AUTOMATION_PAUSE => AutomationRequest::Pause {
                by: PauseSource::Sdk,
            },
            AUTOMATION_RESUME => AutomationRequest::Resume,
            _ => return Ok(None),
        }))
    };
    request().transpose()
}

/// Without a runtime every automation control answers unavailable, whatever
/// it asked. With one, a control whose fields make no request is refused by
/// name, and the runtime answers the rest off the stdin thread.
fn answer_automation_control(
    writer: &SdkWriter,
    request_id: &str,
    request: Result<AutomationRequest, String>,
    automations: Option<&AutomationHandle>,
) -> Result<()> {
    let Some(automations) = automations else {
        let (response, error) = automation_control_response(Err(AutomationError::Unavailable));
        return writer.emit_control_response(request_id, response, error);
    };
    match request {
        Ok(request) => {
            let automations = automations.clone();
            forward_control(
                writer,
                request_id,
                async move { automations.request(request).await },
                automation_control_response,
            );
            Ok(())
        }
        Err(message) => writer.emit_control_response(request_id, None, Some(message)),
    }
}

/// A success carries the runtime's answer under `automation`; a failure is an
/// error response whose message is the error's text, with the structured
/// error under `automation_error`.
fn automation_control_response(
    answer: Result<AutomationResponse, AutomationError>,
) -> (Option<Value>, Option<String>) {
    match answer {
        Ok(response) => (
            Some(serde_json::json!({ AUTOMATION_REPLY: response })),
            None,
        ),
        Err(error) => (
            Some(serde_json::json!({ AUTOMATION_ERROR_REPLY: error })),
            Some(error.to_string()),
        ),
    }
}

/// A success carries the runtime's answer under `workflow`; a failure is an
/// error response whose message is the error's text, with the structured
/// error under `workflow_error`.
fn workflow_control_response(
    answer: Result<WorkflowResponse, WorkflowError>,
) -> (Option<Value>, Option<String>) {
    match answer {
        Ok(response) => (Some(serde_json::json!({ "workflow": response })), None),
        Err(error) => (
            Some(serde_json::json!({ "workflow_error": error })),
            Some(error.to_string()),
        ),
    }
}

fn resolve_set_model(
    model_val: Option<&Value>,
    startup_model: &Model,
    model_policy: &ModelPolicy,
) -> Option<Model> {
    match model_val? {
        Value::Null => Some(startup_model.clone()),
        Value::String(model_str) => {
            let spec = resolve_model_spec(model_str);
            if !model_policy.allows(&spec) {
                warn!(model = %spec, "ignoring model disallowed by policy");
                return None;
            }
            match Model::from_spec(&spec) {
                Ok(m) => Some(m),
                Err(e) => {
                    warn!(model = %model_str, error = %e, "ignoring invalid model");
                    None
                }
            }
        }
        _ => None,
    }
}

fn resolve_model_spec(model_id: &str) -> String {
    if model_id.contains('/') {
        return model_id.to_string();
    }
    if model_id.starts_with("claude-") {
        return format!("anthropic/{model_id}");
    }
    model_id.to_string()
}

fn decode_permission_response(
    data: &Value,
    expected_input: Option<&Value>,
    expected_tool: Option<&str>,
) -> PermissionAnswer {
    match data.get("behavior").and_then(Value::as_str) {
        Some("allow") => {
            if let Some(updated) = data.get("updatedInput")
                && expected_input != Some(updated)
            {
                return PermissionAnswer::Deny;
            }
            let Some(updates) = data.get("updatedPermissions") else {
                return exact_permission_allow(PermissionLifetime::Once);
            };
            let Some(updates) = updates.as_array() else {
                return PermissionAnswer::Deny;
            };
            if updates.is_empty() {
                return exact_permission_allow(PermissionLifetime::Once);
            }
            let mut destination = None;
            for update in updates {
                if update.get("type").and_then(Value::as_str) != Some("addRules")
                    || update.get("behavior").and_then(Value::as_str) != Some("allow")
                {
                    return PermissionAnswer::Deny;
                }
                let Some(expected_tool) = expected_tool else {
                    return PermissionAnswer::Deny;
                };
                let Some(rules) = update.get("rules").and_then(Value::as_array) else {
                    return PermissionAnswer::Deny;
                };
                if rules.is_empty()
                    || rules.iter().any(|rule| {
                        rule.get("toolName").and_then(Value::as_str) != Some(expected_tool)
                    })
                {
                    return PermissionAnswer::Deny;
                }
                let current = update.get("destination").and_then(Value::as_str);
                if current.is_none() || destination.is_some_and(|value| Some(value) != current) {
                    return PermissionAnswer::Deny;
                }
                destination = current;
            }
            match destination {
                Some("session") => exact_permission_allow(PermissionLifetime::Conversation),
                Some("projectSettings" | "localSettings") => {
                    exact_permission_allow(PermissionLifetime::Project)
                }
                Some("userSettings") => exact_permission_allow(PermissionLifetime::Global),
                _ => PermissionAnswer::Deny,
            }
        }
        Some("deny") => match data.get("message").and_then(Value::as_str) {
            Some(msg) if !msg.is_empty() => PermissionAnswer::DenyWithGuidance(msg.to_string()),
            _ => PermissionAnswer::Deny,
        },
        _ => PermissionAnswer::Deny,
    }
}

fn exact_permission_allow(lifetime: PermissionLifetime) -> PermissionAnswer {
    PermissionAnswer::AllowOption {
        option_id: "allow_exact".into(),
        lifetime,
    }
}

fn answer_permission_response(
    shared: &Mutex<Shared>,
    permissions: &PermissionManager,
    response: InboundControlResponseInner,
) {
    let request_id = shared
        .lock()
        .unwrap()
        .pending
        .get(&response.request_id)
        .cloned();
    let pending_request = request_id
        .as_deref()
        .and_then(|request_id| permissions.pending_request(request_id));
    let pending_tool = pending_request.as_ref().map(|request| {
        let tool = request.tool.to_string();
        caudra_to_claude_tool_name(&tool).to_owned()
    });
    let answer = if response.subtype == "success" {
        decode_permission_response(
            &response.response,
            pending_request.as_ref().map(|request| &request.input),
            pending_tool.as_deref(),
        )
    } else {
        PermissionAnswer::Deny
    };
    answer_pending_permission(shared, permissions, &response.request_id, answer);
}

fn answer_pending_permission(
    shared: &Mutex<Shared>,
    permissions: &PermissionManager,
    sdk_request_id: &str,
    mut answer: PermissionAnswer,
) -> bool {
    let request_id = {
        let mut shared = shared.lock().unwrap();
        if let Some((tasks, envelope)) = shared.task_permissions.remove(sdk_request_id)
            && !tasks.event_is_current(&envelope)
        {
            answer = PermissionAnswer::Deny;
        }
        if let Some(request_id) = shared.pending.remove(sdk_request_id) {
            Some(request_id)
        } else if shared.resolved_permission_requests.remove(sdk_request_id) {
            return true;
        } else {
            None
        }
    };
    let Some(request_id) = request_id else {
        warn!(
            sdk_request_id,
            "response for unknown SDK permission request"
        );
        return false;
    };
    if permissions.answer(&request_id, answer) || permissions.pending_request(&request_id).is_none()
    {
        true
    } else {
        warn!(%request_id, "SDK permission response failed; denying request");
        permissions.answer(&request_id, PermissionAnswer::Deny);
        false
    }
}

struct EventPump {
    run_rx: Receiver<InteractiveRun>,
    pending_runs: HashMap<u64, InteractiveRun>,
    run: Option<InteractiveRun>,
    background: Option<BackgroundTasks>,
    writer: SdkWriter,
    shared: Arc<Mutex<Shared>>,
    permissions: Arc<PermissionManager>,
    include_partial_messages: bool,
    synth: StreamSynth,
    result_text: String,
    /// Summed as the turns land: rates move mid-prompt, and only a turn knows
    /// the rate it paid.
    cost: Option<f64>,
    subscription_cost: Option<f64>,
    auxiliary_usage: TokenUsage,
    request_counter: u64,
}

impl EventPump {
    /// Agent events and automation events go out from this one task, in the
    /// order they arrive. The agent events end it once the session ends,
    /// after what the automations sent before then; the automation events
    /// closing first, as they start out without a runtime, only stops their
    /// reading.
    fn spawn(
        mut self,
        event_rx: Receiver<Envelope>,
        automation_rx: Receiver<AutomationEvent>,
    ) -> smol::Task<()> {
        smol::spawn(async move {
            let pumped: Result<()> = async {
                loop {
                    let next = future::or(
                        async { Ok(event_rx.recv_async().await.ok()) },
                        self.writer.emit_automations(&automation_rx),
                    )
                    .await?;
                    let Some(envelope) = next else {
                        return automation_rx
                            .try_iter()
                            .try_for_each(|event| self.writer.emit_automation(event));
                    };
                    self.handle(envelope)?;
                }
            }
            .await;
            if let Err(error) = pumped {
                warn!(%error, "sdk event pump stopped");
            }
        })
    }

    fn model_id(&self) -> String {
        self.shared.lock().unwrap().model.id.clone()
    }

    fn emit_stream(&self, events: Vec<Value>) -> Result<()> {
        events.into_iter().try_for_each(|event| {
            self.writer
                .emit(WireInner::StreamEvent(StreamEventPayload { event }))
        })
    }

    /// The run's spend, filed under whoever pays for it, so `total_cost_usd`
    /// stays money owed rather than a price a plan already covers.
    fn add_spend(&mut self, cost: Option<f64>, billing: Billing) {
        match billing {
            Billing::Api => add_cost(&mut self.cost, cost),
            Billing::Subscription => add_cost(&mut self.subscription_cost, cost),
        }
    }

    fn emit_goal(&self, event: &AgentEvent) -> Result<()> {
        match GoalSystemPayload::from_event(event) {
            Some(payload) => self
                .writer
                .emit_system(GOAL_SYSTEM_SUBTYPE, serde_json::to_value(payload)?),
            None => Ok(()),
        }
    }

    fn reset_turn(&mut self) {
        self.synth.reset();
        self.result_text.clear();
        self.cost = None;
        self.subscription_cost = None;
        self.auxiliary_usage = TokenUsage::default();
        self.run = None;
    }

    fn emit_turn_result(
        &mut self,
        is_error: bool,
        result: String,
        num_turns: u32,
        usage: TokenUsage,
    ) -> Result<()> {
        let duration_ms = self
            .run
            .as_ref()
            .map_or(0, |run| run.started.elapsed().as_millis());
        // Zero on an unpriced model, which is what its turns reported too.
        let total_cost_usd = self.cost.unwrap_or_default();
        let subscription_cost_usd = self.subscription_cost.unwrap_or_default();
        self.writer.emit(WireInner::Result(ResultPayload {
            run: self.run.as_ref().map(RunInfo::from),
            background_active: self.background.as_ref().map(BackgroundTasks::active_count),
            subtype: if is_error {
                "error_during_execution"
            } else {
                "success"
            },
            is_error,
            duration_ms,
            duration_api_ms: duration_ms,
            num_turns,
            result,
            total_cost_usd,
            subscription_cost_usd,
            usage,
            permission_denials: Vec::new(),
        }))?;
        self.reset_turn();
        Ok(())
    }

    fn handle(&mut self, envelope: Envelope) -> Result<()> {
        if (envelope.task.is_some() || envelope.run_id == BACKGROUND_EVENT_RUN_ID)
            && !self
                .background
                .as_ref()
                .is_some_and(|tasks| tasks.owns_event(&envelope))
        {
            return Ok(());
        }
        for run in self.run_rx.try_iter() {
            self.pending_runs.insert(run.run_id, run);
        }
        let parent_event =
            envelope.subagent.is_none() && envelope.workflow.is_none() && envelope.task.is_none();
        if parent_event && let Some(run) = self.pending_runs.remove(&envelope.run_id) {
            self.reset_turn();
            self.writer
                .emit_system("turn_start", serde_json::to_value(RunInfo::from(&run))?)?;
            self.run = Some(run);
        }
        let detached = envelope.workflow.is_some()
            || envelope.task.as_ref().is_some_and(|child| {
                self.background.as_ref().is_some_and(|tasks| {
                    tasks
                        .status(&child.task_id)
                        .is_ok_and(|task| task.background)
                })
            });
        let parent_tool_use_id = envelope
            .subagent
            .as_ref()
            .map(|s| s.parent_tool_use_id.clone());

        match &envelope.event {
            AgentEvent::TextDelta { text } => {
                if self.include_partial_messages && parent_event {
                    let model = self.model_id();
                    let events = self.synth.text_delta(&model, text);
                    self.emit_stream(events)?;
                }
            }
            AgentEvent::ThinkingDelta { text } => {
                if self.include_partial_messages && parent_event {
                    let model = self.model_id();
                    let events = self.synth.thinking_delta(&model, text);
                    self.emit_stream(events)?;
                }
            }
            AgentEvent::ThinkingBoundary => {
                if self.include_partial_messages && parent_event {
                    let events = self.synth.thinking_boundary();
                    self.emit_stream(events)?;
                }
            }
            AgentEvent::ToolPending { id, name } => {
                if self.include_partial_messages && parent_event {
                    let model = self.model_id();
                    let events =
                        self.synth
                            .tool_pending(&model, id, caudra_to_claude_tool_name(name));
                    self.emit_stream(events)?;
                }
            }
            AgentEvent::ToolInputDelta { id, delta, .. } => {
                if self.include_partial_messages && parent_event {
                    let events = self.synth.tool_input_delta(id, delta);
                    self.emit_stream(events)?;
                }
            }
            AgentEvent::ToolStart(ts) => {
                let name = ts.tool.to_string();
                let input = ts.raw_input.clone().unwrap_or(Value::Null);

                if self.include_partial_messages && parent_event {
                    let model = self.model_id();
                    let events = self.synth.tool_use(
                        &model,
                        &ts.id,
                        caudra_to_claude_tool_name(&name),
                        &serde_json::to_string(&input)?,
                    );
                    self.emit_stream(events)?;
                }
            }
            AgentEvent::ToolOutput { .. }
            | AgentEvent::ToolAnnotation { .. }
            | AgentEvent::ToolDone(_)
            | AgentEvent::BatchProgress(_)
            | AgentEvent::Question(_)
            | AgentEvent::QueueItemConsumed { .. }
            | AgentEvent::QueueBatchConsumed { .. }
            | AgentEvent::QueueDrained
            | AgentEvent::Compacting
            | AgentEvent::CompactionDone
            | AgentEvent::MemoryChanged
            | AgentEvent::SessionTitle { .. }
            | AgentEvent::AuthRequired
            | AgentEvent::AuthRestored
            | AgentEvent::SubagentProgress { .. }
            | AgentEvent::SubagentHistory { .. }
            | AgentEvent::ToolSnapshot { .. }
            | AgentEvent::ToolHeaderSnapshot { .. }
            | AgentEvent::LiveToolBuf { .. }
            | AgentEvent::Nudge { .. }
            | AgentEvent::ToolsLoaded { .. }
            | AgentEvent::Unrecorded { .. }
            | AgentEvent::PromptProgress { .. } => {}
            // The claims a run starts with are on record already and come back
            // here as its preamble lands; a guide item joins the run mid-way.
            AgentEvent::Injected {
                automation_event, ..
            } => {
                if parent_event
                    && let Some(origin) = automation_event
                    && let Some(run) = self
                        .run
                        .as_mut()
                        .filter(|run| run.run_id == envelope.run_id)
                    && !run.automation_events.contains(origin)
                {
                    run.automation_events.push(origin.clone());
                }
            }
            AgentEvent::TaskAdmitted(task) => {
                self.writer.emit_system(
                    "task_admitted",
                    serde_json::json!({
                        "task": task,
                        "parent_tool_use_id": parent_tool_use_id.as_deref().unwrap_or(&task.call_id),
                        "workflow": envelope.workflow,
                        "run_id": envelope.run_id,
                    }),
                )?;
            }
            AgentEvent::Workflow(event) => {
                self.writer.emit_system(
                    WORKFLOW_SYSTEM_SUBTYPE,
                    serde_json::to_value(WorkflowSystemPayload {
                        event,
                        workflow: envelope.workflow.as_ref(),
                    })?,
                )?;
            }
            AgentEvent::StreamReset => {
                if self.include_partial_messages && parent_event {
                    let events = self.synth.finish_message(&TokenUsage::default());
                    self.emit_stream(events)?;
                }
            }
            AgentEvent::GoalEvaluating { .. }
            | AgentEvent::GoalFinished { .. }
            | AgentEvent::GoalDeferred { .. }
            | AgentEvent::GoalLoopCap { .. }
            | AgentEvent::GoalTurnLimit { .. }
            | AgentEvent::GoalClearedAfterError { .. } => self.emit_goal(&envelope.event)?,
            AgentEvent::GoalEvaluation { cost, billing, .. }
            | AgentEvent::GoalEvaluationFailed { cost, billing, .. } => {
                if !detached {
                    self.add_spend(*cost, *billing);
                }
                self.emit_goal(&envelope.event)?;
            }
            AgentEvent::Retry {
                attempt,
                message,
                delay_ms,
            } => {
                if self.include_partial_messages && parent_event {
                    let events = self.synth.finish_message(&TokenUsage::default());
                    self.emit_stream(events)?;
                }
                self.writer.emit_system(
                    "api_retry",
                    serde_json::to_value(RetryPayload {
                        attempt: *attempt,
                        retry_delay_ms: *delay_ms,
                        error: message,
                        parent_tool_use_id: parent_tool_use_id.as_deref(),
                    })?,
                )?;
            }
            AgentEvent::TurnComplete(tc) => {
                if detached {
                    self.writer.emit_system("background_usage", serde_json::json!({
                        "usage": tc.usage, "cost": tc.cost, "billing": tc.billing,
                        "parent_tool_use_id": parent_tool_use_id, "run_id": envelope.run_id, "task": envelope.task,
                    }))?;
                } else {
                    self.add_spend(tc.cost, tc.billing);
                }
                if self.include_partial_messages && parent_event {
                    let events = self.synth.finish_message(&tc.usage);
                    self.emit_stream(events)?;
                }

                let content_value = serde_json::to_value(&tc.message.content)?;
                if parent_event {
                    self.result_text = content_text(&content_value).unwrap_or_default();
                }
                self.writer.emit(WireInner::Assistant(AssistantPayload {
                    task: envelope.task.clone(),
                    message: AssistantMessage {
                        id: wire_uuid(),
                        model: tc.model.clone(),
                        role: "assistant",
                        content: map_tool_names_in_content(&content_value),
                        stop_reason: None,
                        usage: tc.usage,
                    },
                    parent_tool_use_id,
                    workflow: envelope.workflow.clone(),
                }))?;
            }
            AgentEvent::ModelUsage {
                usage,
                cost,
                billing,
                ..
            } => {
                if !detached {
                    self.add_spend(*cost, *billing);
                }
                if parent_event {
                    self.auxiliary_usage += *usage;
                }
                self.writer.emit_system(
                    "model_usage",
                    serde_json::json!({
                        "accounting": envelope.event,
                        "parent_tool_use_id": parent_tool_use_id,
                        "workflow": envelope.workflow,
                        "run_id": envelope.run_id,
                        "background": detached,
                        "task": envelope.task,
                    }),
                )?;
            }
            AgentEvent::ToolResultsSubmitted { message } => {
                self.writer.emit(WireInner::User(UserPayload {
                    task: envelope.task.clone(),
                    message: UserMessage {
                        role: "user",
                        content: serde_json::to_value(&message.content)?,
                    },
                    parent_tool_use_id,
                    workflow: envelope.workflow.clone(),
                }))?;
            }
            AgentEvent::PermissionRequest(request) => {
                if self.shared.lock().unwrap().permission_mode == PermissionMode::BypassPermissions
                {
                    self.permissions
                        .answer(&request.id, PermissionAnswer::AllowOnce);
                    return Ok(());
                }

                self.request_counter += 1;
                let req_id = format!("req_{}", self.request_counter);
                self.shared
                    .lock()
                    .unwrap()
                    .pending
                    .insert(req_id.clone(), request.id.clone());
                if let Some(tasks) = &self.background
                    && envelope.task.is_some()
                {
                    self.shared.lock().unwrap().task_permissions.insert(
                        req_id.clone(),
                        (
                            tasks.clone(),
                            Envelope {
                                event: AgentEvent::PermissionRequest(request.clone()),
                                subagent: envelope.subagent.clone(),
                                run_id: envelope.run_id,
                                workflow: envelope.workflow.clone(),
                                task: envelope.task.clone(),
                            },
                        ),
                    );
                }
                let tool_name = request.tool.to_string();

                let emitted = self
                    .writer
                    .emit(WireInner::ControlRequest(ControlRequestPayload {
                        request_id: req_id.clone(),
                        request: ControlRequestInner {
                            task: envelope.task.clone(),
                            subtype: "can_use_tool",
                            tool_name: Some(caudra_to_claude_tool_name(&tool_name).into()),
                            input: Some(request.input.clone()),
                            tool_use_id: Some(request.id.clone()),
                        },
                    }));
                if let Err(error) = emitted {
                    let mut shared = self.shared.lock().unwrap();
                    shared.pending.remove(&req_id);
                    shared.task_permissions.remove(&req_id);
                    self.permissions.answer(&request.id, PermissionAnswer::Deny);
                    return Err(error);
                }
            }
            AgentEvent::PermissionRequestUpdated(_) => {}
            AgentEvent::PermissionRequestResolved { request_id, .. } => {
                let sdk_request_ids = {
                    let mut shared = self.shared.lock().unwrap();
                    let ids: Vec<_> = shared
                        .pending
                        .iter()
                        .filter(|(_, pending_request_id)| pending_request_id.as_str() == request_id)
                        .map(|(sdk_request_id, _)| sdk_request_id.clone())
                        .collect();
                    for sdk_request_id in &ids {
                        shared.pending.remove(sdk_request_id);
                        shared.task_permissions.remove(sdk_request_id);
                        shared
                            .resolved_permission_requests
                            .insert(sdk_request_id.clone());
                    }
                    ids
                };
                for sdk_request_id in sdk_request_ids {
                    self.writer.emit(WireInner::ControlCancelRequest(
                        ControlCancelRequestPayload {
                            request_id: sdk_request_id,
                        },
                    ))?;
                }
            }
            AgentEvent::Done {
                usage,
                num_turns,
                reason,
            } => {
                if !parent_event {
                    return Ok(());
                }
                self.shared
                    .lock()
                    .unwrap()
                    .resolved_permission_requests
                    .clear();
                // An interrupted run leaves a partial answer, so it is not a success.
                let is_error = *reason != DoneReason::EndTurn;
                let result = mem::take(&mut self.result_text);
                self.emit_turn_result(is_error, result, *num_turns, *usage)?;
            }
            AgentEvent::Error { message } => {
                if !parent_event {
                    return self.writer.emit_system(
                        "background_error",
                        serde_json::json!({
                            "message": message, "parent_tool_use_id": parent_tool_use_id,
                            "run_id": envelope.run_id, "task": envelope.task,
                        }),
                    );
                }
                self.emit_turn_result(true, message.clone(), 0, self.auxiliary_usage)?;
            }
        }
        Ok(())
    }
}

fn map_tool_names_in_content(content: &Value) -> Value {
    match content {
        Value::Array(blocks) => {
            let mapped: Vec<Value> = blocks
                .iter()
                .map(|block| {
                    if block.get("type").and_then(Value::as_str) == Some("tool_use")
                        && let Some(name) = block.get("name").and_then(Value::as_str)
                    {
                        let mut b = block.clone();
                        b["name"] = Value::String(caudra_to_claude_tool_name(name).to_string());
                        return b;
                    }
                    block.clone()
                })
                .collect();
            Value::Array(mapped)
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use caudra_agent::automation::catalog::{Frontend, UNAVAILABLE_IN_SDK};
    use caudra_agent::automation::frontend::set_claimed_goal;
    use caudra_agent::automation::manager::{AutomationRuntime, PAUSED_BY_SDK, RuntimeDeps};
    use caudra_agent::automation::testing::{AutomationFixture, FakeWorkflows};
    use caudra_agent::automation::workflows::Workflows;
    use caudra_agent::permissions::PermissionRequest;
    use caudra_agent::tools::PermissionScopes;
    use caudra_agent::types::WORKFLOW_EVENT_RUN_ID;
    use std::time::Duration;

    use caudra_agent::{
        DEFAULT_GOAL_CONTINUATION_LIMIT, GoalError, MAX_GOAL_CHARS, SubagentInfo, TaskCard,
        ToolOutput,
    };
    use caudra_automation::event::{ArmedReason, Event, EventDetail};
    use caudra_automation::host::ActionKind;
    use caudra_automation::meta::TriggerKind;
    use caudra_automation::replay::{Answer, DRY_RUN_ID};
    use caudra_automation::request::{GoalClaim, REPLAY_NOT_FINISHED, UNAVAILABLE};
    use caudra_automation::snapshot::{ErrorView, FiringStatus, StateOutcome};
    use caudra_providers::{ContentBlock, Message, Role, TaskEventOrigin};
    use caudra_storage::background::{JobPayload, ShellJobMetadata, TaskRecord};
    use caudra_storage::id::CaudraId;
    use caudra_storage::sessions::{
        SessionDatabase, StoredAutomationControls, StoredDeliveryBackoff, StoredUnattendedTurns,
    };
    use caudra_storage::usage_ledger::LedgerPurpose;
    use caudra_workflow::{RunSnapshot, RunStatus, RunUsage, SourceKind};
    use clap::Parser;
    use tempfile::TempDir;
    use test_case::test_case;

    const CAUDRA_REQUEST_ID: &str = "caudra-permission-1";
    const SECOND_CAUDRA_REQUEST_ID: &str = "caudra-permission-2";
    const REPAIR_COST: f64 = 0.25;
    const REPAIR_INPUT: u32 = 17;
    const REPAIR_PARENT: &str = "repair-parent";
    const REPAIR_FAILURE: &str = "transport failed";
    const RETRY_PARENT: &str = "toolu_task_1";
    const RETRY_ERROR: &str = "overloaded";
    const RETRY_DELAY_MS: u64 = 1_000;
    const PARENT_KEY: &str = "parent_tool_use_id";
    const WORKSPACE_REBIND_REQUIRED: &str =
        "session workspace identity changed; fork or explicitly rebind the session";
    const FORK_HISTORY: &str = "history retained without source authority";
    const SOURCE_PLAN: &str = "/source/plan.md";
    const PHRASE_TASK: &str = "happy-cute-tick";
    const TASK_CALL: &str = "task-launch-call";
    const TASK_INVOCATION: &str = "task-runtime-invocation";
    const TASK_EVENT: &str = "task-result-event";
    const TASK_SUCCEEDED: &str = "succeeded";
    const LARGE_TASK_RESULT_BYTES: usize = 64 * 1024;
    const TASK_PROMOTION_ERROR: &str =
        "task promotion requires task_execution = auto and an agent task";

    #[test_case(false; "unsupported_frontend")]
    #[test_case(true; "persistent_frontend")]
    fn init_execution_capabilities_are_independent(jobs: bool) {
        for task in [
            ExecutionMode::Sync,
            ExecutionMode::Auto,
            ExecutionMode::Async,
        ] {
            for shell in [
                ExecutionMode::Sync,
                ExecutionMode::Auto,
                ExecutionMode::Async,
            ] {
                let config = AgentConfig {
                    task_execution: task.clone(),
                    shell_execution: shell.clone(),
                    ..Default::default()
                };
                let payload = init_payload(serde_json::json!({}), false, false, &config, jobs);
                assert_eq!(
                    payload["task_execution"]["configured"],
                    serde_json::json!(task)
                );
                assert_eq!(
                    payload["shell_execution"]["configured"],
                    serde_json::json!(shell)
                );
                assert_eq!(
                    payload["task_execution"]["effective"],
                    serde_json::json!(task.effective(jobs))
                );
                assert_eq!(
                    payload["shell_execution"]["effective"],
                    serde_json::json!(shell.effective(jobs))
                );
                assert_eq!(
                    payload["background_tasks"],
                    jobs && task != ExecutionMode::Sync
                );
                assert_eq!(
                    payload["background_shell"],
                    jobs && shell != ExecutionMode::Sync
                );
                let controls = payload["task_controls"].as_array().unwrap();
                assert_eq!(
                    controls.contains(&serde_json::json!("task_promote")),
                    jobs && task == ExecutionMode::Auto
                );
                for control in ["task_list", "task_status", "task_cancel"] {
                    assert_eq!(controls.contains(&serde_json::json!(control)), jobs);
                }
                assert_eq!(
                    payload["job_kinds"],
                    if jobs {
                        serde_json::json!(["agent", "shell"])
                    } else {
                        serde_json::json!([])
                    }
                );
            }
        }
    }

    #[test_case(ExecutionMode::Sync, Value::Null, TASK_PROMOTION_ERROR; "sync")]
    #[test_case(ExecutionMode::Async, Value::Null, TASK_PROMOTION_ERROR; "async_mode")]
    #[test_case(ExecutionMode::Auto, serde_json::json!({"invocation_id": "stale"}), STALE_TASK_INVOCATION; "stale_invocation")]
    #[test_case(ExecutionMode::Auto, serde_json::json!({"generation": 0}), STALE_TASK_INVOCATION; "stale_generation")]
    fn stale_sdk_promotion_is_rejected(mode: ExecutionMode, extra: Value, error: &str) {
        let temp = TempDir::new().unwrap();
        let storage = StateDir::from_path(temp.path().into());
        let mut session = StoredSession::new("provider/model", "/repo");
        session.save(&storage).unwrap();
        SessionDatabase::open(&storage)
            .unwrap()
            .save_background_task(session.id, &task_record(Value::Null))
            .unwrap();
        let tasks = smol::block_on(BackgroundTasks::spawn(storage, session.id)).unwrap();
        tasks.set_task_execution(mode);
        let (pump, out, _) = permission_event_pump(permission_manager(), PermissionMode::Default);
        let mut arguments = serde_json::json!({"task_id": PHRASE_TASK});
        if let Some(extra) = extra.as_object() {
            arguments.as_object_mut().unwrap().extend(extra.clone());
        }
        handle_task_control_request(
            &InboundControlRequest {
                request_id: TASK_CALL.into(),
                request: InboundControlRequestInner {
                    subtype: "task_promote".into(),
                    extra: arguments,
                },
            },
            &pump.writer,
            Some(&tasks),
        )
        .unwrap();
        let response = next_message(&out);
        assert_eq!(response["response"]["subtype"], "error");
        assert_eq!(response["response"]["error"], error);
        assert!(!tasks.status(PHRASE_TASK).unwrap().active());
        smol::block_on(tasks.shutdown()).unwrap();
    }

    fn task_record(output: Value) -> TaskRecord {
        TaskRecord {
            payload: Default::default(),
            owner: Default::default(),
            created_at: 0,
            updated_at: 0,
            sequence: 1,
            task_id: PHRASE_TASK.into(),
            invocation_id: TASK_INVOCATION.into(),
            root_call_id: TASK_CALL.into(),
            generation: 1,
            state: TASK_SUCCEEDED.into(),
            background: true,
            receipt_accepted: true,
            mode: "build".into(),
            request: serde_json::json!({"call_id": TASK_CALL, "label": PHRASE_TASK}),
            outcome: Some(serde_json::json!({
                "task_id": PHRASE_TASK, "mode": "build", "success": true, "cancelled": false,
                "output": output, "error": null, "tokens_used": 0, "duration_ms": 0
            })),
            output_ref: None,
            history: serde_json::json!([]),
            spec: Value::Null,
            events: Vec::new(),
        }
    }

    #[test_case(serde_json::json!({"answer": [1, true, null]}), false, false; "object")]
    #[test_case(serde_json::json!([1, {"answer": true}]), false, false; "array")]
    #[test_case(serde_json::json!("answer"), false, false; "text")]
    #[test_case(serde_json::json!("x".repeat(LARGE_TASK_RESULT_BYTES)), true, false; "large_result_reference")]
    #[test_case(serde_json::json!({"stdout": "shell output", "exit_code": 0}), false, true; "shell_output")]
    #[test_case(serde_json::json!("x".repeat(LARGE_TASK_RESULT_BYTES)), true, true; "large_shell_output")]
    fn task_status_and_tool_result_preserve_native_outcome(
        output: Value,
        oversized: bool,
        shell: bool,
    ) {
        let temp = TempDir::new().unwrap();
        let storage = StateDir::from_path(temp.path().into());
        let mut session = StoredSession::new("provider/model", "/repo");
        session.save(&storage).unwrap();
        let mut record = task_record(output.clone());
        if shell {
            record.payload = JobPayload::Shell(ShellJobMetadata {
                call_id: TASK_CALL.into(),
                root_call_id: TASK_CALL.into(),
                command: "printf shell-output".into(),
                workdir: ".".into(),
                timeout_ms: 120_000,
                mode: "build".into(),
            });
        }
        SessionDatabase::open(&storage)
            .unwrap()
            .save_background_task(session.id, &record)
            .unwrap();
        let tasks = smol::block_on(BackgroundTasks::spawn(storage.clone(), session.id)).unwrap();
        let (mut pump, out, _) =
            permission_event_pump(permission_manager(), PermissionMode::Default);
        handle_task_control_request(
            &InboundControlRequest {
                request_id: TASK_CALL.into(),
                request: InboundControlRequestInner {
                    subtype: "task_status".into(),
                    extra: serde_json::json!({"task_id": PHRASE_TASK}),
                },
            },
            &pump.writer,
            Some(&tasks),
        )
        .unwrap();
        let response = next_message(&out);
        let status = &response["response"]["response"];
        assert_eq!(response["type"], "control_response");
        assert_eq!(status["task_id"], PHRASE_TASK);
        assert_eq!(status["invocation_id"], TASK_INVOCATION);
        assert_eq!(status["state"], TASK_SUCCEEDED);
        assert_eq!(status["kind"], if shell { "shell" } else { "agent" });
        assert_eq!(status["result_truncated"], oversized);
        if oversized {
            assert!(status.get("result").is_none());
            assert!(status["result_preview"].is_string());
        } else {
            assert_eq!(status["result"], record.outcome.clone().unwrap());
            assert_eq!(status["result"]["output"], output);
        }
        let reference: ToolOutputRef =
            serde_json::from_value(status["output_ref"].clone()).unwrap();
        let full = ToolOutputStore::new(storage)
            .load_text(session.id, reference.id.clone())
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&full).unwrap(),
            record.outcome.clone().unwrap()
        );

        let content = ToolOutput::Tasks(vec![tasks.status(PHRASE_TASK).unwrap()]).as_text();
        pump.handle(Envelope {
            event: AgentEvent::ToolResultsSubmitted {
                message: Box::new(Message {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: TASK_CALL.into(),
                        content,
                        is_error: false,
                        output_ref: None,
                    }],
                    ..Default::default()
                }),
            },
            subagent: None,
            run_id: 1,
            task: None,
            workflow: None,
        })
        .unwrap();
        let message = next_message(&out);
        let cards: Value = serde_json::from_str(
            message["message"]["content"][0]["content"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(cards[0]["task_id"], PHRASE_TASK);
        if oversized {
            assert!(cards[0].get("result").is_none());
            assert_eq!(cards[0]["result_truncated"], true);
            assert_eq!(cards[0]["output_ref"], status["output_ref"]);
            assert_eq!(
                cards[0]["read_output"]["output_id"],
                reference.id.to_string()
            );
        } else {
            assert_eq!(cards[0]["result"], record.outcome.unwrap());
            assert_eq!(cards[0]["result"]["output"], output);
            assert!(cards[0].get("output_ref").is_none());
            assert!(cards[0].get("read_output").is_none());
        }
        for key in ["invocation_id", "call_id", "root_call_id", "generation"] {
            assert!(cards[0].get(key).is_none());
        }
        pump.background = Some(tasks.clone());
        pump.handle(Envelope {
            event: AgentEvent::Done {
                usage: TokenUsage::default(),
                num_turns: 0,
                reason: DoneReason::EndTurn,
            },
            subagent: None,
            run_id: BACKGROUND_EVENT_RUN_ID,
            workflow: None,
            task: Some(Arc::new(TaskProvenance {
                session_id: session.id,
                task_id: PHRASE_TASK.into(),
                invocation_id: TASK_INVOCATION.into(),
            })),
        })
        .unwrap();
        assert!(out.is_empty());
        smol::block_on(tasks.shutdown()).unwrap();
    }

    #[test_case(false; "foreground")]
    #[test_case(true; "background")]
    fn task_admission_is_a_lifecycle_event_without_a_new_user_turn(background: bool) {
        const RUN: u64 = 42;
        let (mut pump, out, _) =
            permission_event_pump(permission_manager(), PermissionMode::Default);
        pump.run = Some(InteractiveRun {
            run_id: RUN,
            started: Instant::now(),
            automatic: false,
            task_event_ids: Vec::new(),
            workflow_events: Vec::new(),
            automation_events: Vec::new(),
        });
        pump.result_text = REPAIR_PARENT.into();
        pump.synth.text_delta(&pump.model_id(), REPAIR_PARENT);
        let mut card = TaskCard::from(&task_record(Value::Null));
        card.state = "queued".into();
        card.result = None;
        card.background = background;
        pump.handle(Envelope {
            event: AgentEvent::TaskAdmitted(card.clone()),
            subagent: None,
            run_id: RUN,
            task: None,
            workflow: None,
        })
        .unwrap();
        let message = next_message(&out);
        assert_eq!(message["type"], "system");
        assert_eq!(message["subtype"], "task_admitted");
        assert_eq!(message["task"], serde_json::to_value(card).unwrap());
        assert_eq!(message["parent_tool_use_id"], TASK_CALL);
        assert_eq!(message["run_id"], RUN);
        assert!(out.is_empty());
        assert_eq!(pump.run.as_ref().unwrap().run_id, RUN);
        assert_eq!(pump.result_text, REPAIR_PARENT);
        assert!(pump.synth.started);
    }

    fn permission_manager() -> Arc<PermissionManager> {
        Arc::new(PermissionManager::new_nonpersistent(
            PermissionsConfig::default(),
            Path::new("/project").to_path_buf(),
            Arc::default(),
        ))
    }

    fn shared_with_pending(pending: HashMap<String, String>) -> Arc<Mutex<Shared>> {
        Arc::new(Mutex::new(Shared {
            model: Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap(),
            permission_mode: PermissionMode::Default,
            pending,
            task_permissions: HashMap::new(),
            resolved_permission_requests: HashSet::new(),
            workspace_session: None,
            local_documents: None,
            session_id: SessionRef::generate(),
            remote_plan: None,
        }))
    }

    fn pending_permission(
        manager: Arc<PermissionManager>,
        request_id: &str,
        scope: &str,
        input: Value,
    ) -> (smol::Task<bool>, Receiver<Envelope>) {
        let (event_tx, event_rx) = flume::unbounded();
        let event_tx = caudra_agent::EventSender::new(event_tx, 0);
        let request_id = request_id.to_owned();
        let scopes = PermissionScopes::single(scope.to_owned());
        let task = smol::spawn(async move {
            let (_legacy_tx, legacy_rx) = flume::unbounded();
            let legacy_rx = smol::lock::Mutex::new(legacy_rx);
            manager
                .enforce(
                    &caudra_config::ToolKey::native("shell"),
                    &scopes,
                    &input,
                    &event_tx,
                    Some(&legacy_rx),
                    &request_id,
                    &caudra_agent::CancelToken::none(),
                    None,
                )
                .await
                .is_ok()
        });
        (task, event_rx)
    }

    async fn enforcement_without_answer(manager: &PermissionManager, scope: &str) -> bool {
        let (event_tx, _) = flume::unbounded();
        manager
            .enforce(
                &caudra_config::ToolKey::native("shell"),
                &PermissionScopes::single(scope.to_owned()),
                &serde_json::json!({"command": scope}),
                &caudra_agent::EventSender::new(event_tx, 0),
                None,
                "follow-up",
                &caudra_agent::CancelToken::none(),
                None,
            )
            .await
            .is_ok()
    }

    fn permission_event_pump(
        permissions: Arc<PermissionManager>,
        permission_mode: PermissionMode,
    ) -> (EventPump, Receiver<String>, Arc<Mutex<Shared>>) {
        let (out_tx, out_rx) = flume::unbounded();
        let shared = shared_with_pending(HashMap::new());
        shared.lock().unwrap().permission_mode = permission_mode;
        let pump = EventPump {
            run_rx: flume::unbounded().1,
            pending_runs: HashMap::new(),
            run: None,
            background: None,
            writer: SdkWriter {
                session_id: SessionRef::generate(),
                out_tx,
            },
            shared: Arc::clone(&shared),
            permissions,
            include_partial_messages: false,
            synth: StreamSynth::new(),
            result_text: String::new(),
            cost: None,
            subscription_cost: None,
            auxiliary_usage: TokenUsage::default(),
            request_counter: 0,
        };
        (pump, out_rx, shared)
    }

    fn history_messages(items: Vec<HistoryItem>) -> Vec<Message> {
        History::restored(items).unwrap().into_vec()
    }

    #[test_case(false; "normal_completion")]
    #[test_case(true; "failed_parent")]
    fn parent_result_preserves_child_permission_requests(failed: bool) {
        smol::block_on(async {
            let manager = permission_manager();
            let (mut pump, out, shared) =
                permission_event_pump(Arc::clone(&manager), PermissionMode::Default);
            let (child, events) = pending_permission(
                Arc::clone(&manager),
                CAUDRA_REQUEST_ID,
                REPAIR_PARENT,
                serde_json::json!({"command": REPAIR_PARENT}),
            );
            pump.handle(events.recv_async().await.unwrap()).unwrap();
            let request = next_message(&out);
            pump.handle(Envelope {
                event: if failed {
                    AgentEvent::Error {
                        message: REPAIR_FAILURE.into(),
                    }
                } else {
                    AgentEvent::Done {
                        usage: TokenUsage::default(),
                        num_turns: 1,
                        reason: DoneReason::EndTurn,
                    }
                },
                subagent: None,
                run_id: 1,
                task: None,
                workflow: None,
            })
            .unwrap();
            assert_eq!(shared.lock().unwrap().pending.len(), 1);
            assert_eq!(next_message(&out)["type"], "result");
            answer_permission_response(
                &shared,
                &manager,
                InboundControlResponseInner {
                    subtype: "success".into(),
                    request_id: request["request_id"].as_str().unwrap().into(),
                    response: serde_json::json!({"behavior": "allow"}),
                },
            );
            assert!(child.await);
        });
    }

    #[test_case(false; "user_run")]
    #[test_case(true; "automatic_run")]
    fn admission_metadata_correlates_start_and_result(automatic: bool) {
        const RUN: u64 = 42;
        const EVENT: &str = "task-report-event";
        const WORKFLOW_RUN: &str = "workflow-run";
        let (mut pump, out, _) =
            permission_event_pump(permission_manager(), PermissionMode::Default);
        let (tx, rx) = flume::unbounded();
        pump.run_rx = rx;
        tx.send(InteractiveRun {
            run_id: RUN,
            started: Instant::now(),
            automatic,
            task_event_ids: vec![EVENT.into()],
            workflow_events: vec![WorkflowEventOrigin {
                run_id: WORKFLOW_RUN.into(),
                revision: RUN,
            }],
            automation_events: Vec::new(),
        })
        .unwrap();
        pump.handle(Envelope {
            event: AgentEvent::Done {
                usage: TokenUsage::default(),
                num_turns: 1,
                reason: DoneReason::EndTurn,
            },
            subagent: None,
            run_id: RUN,
            task: None,
            workflow: None,
        })
        .unwrap();
        let start = next_message(&out);
        let result = next_message(&out);
        assert_eq!(start["subtype"], "turn_start");
        assert_eq!(start["run_id"], RUN);
        assert_eq!(result["run"]["run_id"], RUN);
        assert_eq!(result["run"]["automatic"], automatic);
        assert_eq!(result["run"]["task_event_ids"], serde_json::json!([EVENT]));
        assert_eq!(
            result["run"]["workflow_events"],
            serde_json::json!([{ "run_id": WORKFLOW_RUN, "revision": RUN }])
        );
    }

    #[test_case(false; "missing_provenance")]
    #[test_case(true; "foreign_provenance")]
    fn unowned_task_events_cannot_finalize_the_parent(tagged: bool) {
        let (mut pump, out, _) =
            permission_event_pump(permission_manager(), PermissionMode::Default);
        pump.result_text = REPAIR_PARENT.into();
        pump.handle(Envelope {
            event: AgentEvent::Error {
                message: REPAIR_FAILURE.into(),
            },
            subagent: None,
            run_id: BACKGROUND_EVENT_RUN_ID,
            workflow: None,
            task: tagged.then(|| {
                Arc::new(TaskProvenance {
                    session_id: SessionRef::generate().id(),
                    task_id: REPAIR_PARENT.into(),
                    invocation_id: REPAIR_PARENT.into(),
                })
            }),
        })
        .unwrap();
        assert!(out.is_empty());
        assert_eq!(pump.result_text, REPAIR_PARENT);
    }

    #[test]
    fn unowned_task_permission_response_is_denied() {
        smol::block_on(async {
            let temp = TempDir::new().unwrap();
            let tasks = BackgroundTasks::spawn(
                StateDir::from_path(temp.path().into()),
                SessionRef::generate().id(),
            )
            .await
            .unwrap();
            let manager = permission_manager();
            let (child, events) = pending_permission(
                Arc::clone(&manager),
                CAUDRA_REQUEST_ID,
                REPAIR_PARENT,
                serde_json::json!({"command": REPAIR_PARENT}),
            );
            let event = events.recv_async().await.unwrap();
            let shared = shared_with_pending(HashMap::from([(
                CAUDRA_REQUEST_ID.into(),
                CAUDRA_REQUEST_ID.into(),
            )]));
            shared
                .lock()
                .unwrap()
                .task_permissions
                .insert(CAUDRA_REQUEST_ID.into(), (tasks.clone(), event));
            answer_pending_permission(
                &shared,
                &manager,
                CAUDRA_REQUEST_ID,
                PermissionAnswer::AllowOnce,
            );
            assert!(!child.await);
            assert!(shared.lock().unwrap().task_permissions.is_empty());
            tasks.shutdown().await.unwrap();
        });
    }

    #[test_case(false; "done")]
    #[test_case(true; "error")]
    fn repair_accounting_preserves_stream_and_subagent_totals(failed: bool) {
        let (mut pump, out, _) =
            permission_event_pump(permission_manager(), PermissionMode::Default);
        let usage = TokenUsage {
            input: REPAIR_INPUT,
            ..Default::default()
        };
        pump.result_text = REPAIR_PARENT.into();
        let model = pump.model_id();
        pump.synth.text_delta(&model, REPAIR_PARENT);
        for subagent in [
            None,
            Some(SubagentInfo {
                parent_tool_use_id: REPAIR_PARENT.into(),
                task_id: REPAIR_PARENT.into(),
                name: REPAIR_PARENT.into(),
                prompt: None,
                model: None,
                thinking: None,
                fast: false,
                answer_tx: None,
                steer_tx: None,
            }),
        ] {
            pump.handle(Envelope {
                event: AgentEvent::ModelUsage {
                    usage,
                    cost: Some(REPAIR_COST),
                    billing: Billing::Api,
                    provider: REPAIR_PARENT.into(),
                    model: model.clone(),
                    purpose: LedgerPurpose::ToolJsonRepair,
                },
                subagent,
                run_id: 1,
                task: None,
                workflow: None,
            })
            .unwrap();
        }
        assert!(pump.synth.started);
        assert_eq!(pump.result_text, REPAIR_PARENT);
        let accounting: Vec<Value> = out
            .try_iter()
            .map(|line| serde_json::from_str(&line).unwrap())
            .collect();
        assert_eq!(accounting.len(), 2);
        for event in &accounting {
            assert_eq!(event["type"], "system");
            assert_eq!(event["subtype"], "model_usage");
            assert_eq!(
                event["accounting"]["purpose"],
                LedgerPurpose::ToolJsonRepair.storage_name()
            );
        }
        assert_eq!(accounting[1]["parent_tool_use_id"], REPAIR_PARENT);
        let event = if failed {
            AgentEvent::Error {
                message: REPAIR_FAILURE.into(),
            }
        } else {
            AgentEvent::Done {
                usage,
                num_turns: 1,
                reason: DoneReason::EndTurn,
            }
        };
        pump.handle(Envelope {
            event,
            subagent: None,
            run_id: 1,
            task: None,
            workflow: None,
        })
        .unwrap();
        let result: Value = serde_json::from_str(&out.recv().unwrap()).unwrap();
        assert_eq!(result["type"], "result");
        assert_eq!(result["usage"]["input_tokens"], REPAIR_INPUT);
        assert_eq!(result["total_cost_usd"], REPAIR_COST * 2.0);
        assert_eq!(pump.auxiliary_usage, TokenUsage::default());
    }

    fn sample_messages() -> Vec<Message> {
        vec![
            Message::user("hello".into()),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "first".into(),
                    },
                    ContentBlock::Text {
                        text: "second".into(),
                    },
                ],
                ..Default::default()
            },
        ]
    }

    fn stored_structured_rule() -> PermissionRuleRecord {
        let request = PermissionRequest::from_legacy(
            "stored-structured".into(),
            caudra_config::ToolKey::native("bash"),
            vec!["cargo test".into()],
            serde_json::json!({"command": "cargo test"}),
            Path::new("/repo"),
            false,
        );
        PermissionRuleRecord::conversation(
            request
                .option_rule(
                    "allow_exact",
                    caudra_agent::permissions::PermissionLifetime::Conversation,
                )
                .unwrap(),
        )
        .unwrap()
    }

    fn claude_to_caudra_tool_name(name: &str) -> &str {
        TOOL_NAME_MAP
            .iter()
            .find(|(_, c)| *c == name)
            .map(|(m, _)| *m)
            .unwrap_or(name)
    }

    #[test_case("file_apply_patch", "FileApplyPatch")]
    #[test_case("file_edit", "FileEdit")]
    #[test_case("file_glob", "FileGlob")]
    #[test_case("file_grep", "FileGrep")]
    #[test_case("file_read", "FileRead")]
    #[test_case("file_write", "FileWrite")]
    #[test_case("shell", "Shell")]
    #[test_case("todo_write", "TodoWrite")]
    #[test_case("webfetch", "WebFetch")]
    #[test_case("websearch", "WebSearch")]
    #[test_case("task", "Task")]
    #[test_case("python_execution", "PythonExecution")]
    #[test_case("execution_environment", "ExecutionEnvironment")]
    #[test_case("file_index", "Index")]
    #[test_case("memory", "Memory")]
    #[test_case("question", "Question")]
    fn caudra_to_claude_roundtrip(caudra: &str, claude: &str) {
        assert_eq!(caudra_to_claude_tool_name(caudra), claude);
        assert_eq!(claude_to_caudra_tool_name(claude), caudra);
    }

    #[test]
    fn unknown_tool_name_passthrough() {
        assert_eq!(caudra_to_claude_tool_name("unknown_tool"), "unknown_tool");
        assert_eq!(claude_to_caudra_tool_name("UnknownTool"), "UnknownTool");
    }

    #[test]
    fn public_user_message_json_omits_managed_output_ref() {
        let output_ref = ToolOutputRef {
            id: caudra_storage::id::CaudraId::generate()
                .to_string()
                .parse()
                .unwrap(),
            byte_count: 12,
            line_count: 2,
        };
        let message = Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call-1".into(),
                content: "result".into(),
                is_error: false,
                output_ref: Some(output_ref),
            }],
            ..Default::default()
        };
        let payload = UserPayload {
            task: None,
            message: UserMessage {
                role: "user",
                content: serde_json::to_value(&message.content).unwrap(),
            },
            parent_tool_use_id: None,
            workflow: None,
        };

        let json = serde_json::to_value(payload).unwrap();

        assert!(json["message"]["content"][0].get("output_ref").is_none());
    }

    #[test]
    fn fork_rebases_item_identity_without_changing_provider_content() {
        let items = History::new(sample_messages()).into_items();
        let original_ids: Vec<_> = items.iter().map(|item| item.id).collect();
        let expected = serde_json::to_value(history_messages(items.clone())).unwrap();

        let forked = rebase_history(items).unwrap();

        assert!(forked.iter().all(|item| !original_ids.contains(&item.id)));
        assert_eq!(
            serde_json::to_value(history_messages(forked)).unwrap(),
            expected
        );
    }

    #[test]
    fn fork_copies_rebased_history_output_refs_to_target_session() {
        let temp = TempDir::new().unwrap();
        let storage = StateDir::from_path(temp.path().to_path_buf());
        let store = ToolOutputStore::new(storage.clone());
        let source = SessionRef::generate();
        let target = SessionRef::generate();
        let output_ref = store.put(source.id(), "complete artifact").unwrap();
        let retained_ref = store.put(source.id(), "nested artifact").unwrap();
        let history = History::new(vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use(
                    "call-1",
                    "bash",
                    serde_json::json!({}),
                )],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call-1".into(),
                    content: "preview".into(),
                    is_error: false,
                    output_ref: Some(output_ref.clone()),
                }],
                ..Default::default()
            },
        ])
        .into_items();
        let rebased = rebase_history(history).unwrap();
        let compacted = rebase_history(
            History::new(vec![Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: format!("retained output ID: {}", retained_ref.id),
                }],
                retained_output_refs: vec![retained_ref.clone()],
                ..Default::default()
            }])
            .into_items(),
        )
        .unwrap();

        copy_history_outputs(
            &storage,
            &source,
            &target,
            [rebased.as_slice(), compacted.as_slice()],
        )
        .unwrap();

        assert_eq!(
            store.load_text(target.id(), output_ref.id.clone()).unwrap(),
            "complete artifact"
        );
        assert_eq!(
            store.load_text(target.id(), retained_ref.id).unwrap(),
            "nested artifact"
        );
        assert!(rebased.iter().any(|item| matches!(
            &item.kind,
            HistoryItemKind::ToolResult {
                output_ref: Some(reference),
                ..
            } if reference == &output_ref
        )));
    }

    #[test]
    fn fork_copies_task_notice_output_without_a_retrieval_call() {
        let temp = TempDir::new().unwrap();
        let storage = StateDir::from_path(temp.path().to_path_buf());
        let store = ToolOutputStore::new(storage.clone());
        let source = SessionRef::generate();
        let target = SessionRef::generate();
        let complete = "x".repeat(LARGE_TASK_RESULT_BYTES);
        let reference = store.put(source.id(), &complete).unwrap();
        let origin = TaskEventOrigin {
            task_id: PHRASE_TASK.into(),
            invocation_id: TASK_INVOCATION.into(),
            event_id: TASK_EVENT.into(),
        };
        let mut notice = Message::task_observation(
            format!("Task {PHRASE_TASK}: success.\n\n[truncated]"),
            origin.clone(),
        );
        notice.retained_output_refs.push(reference.clone());
        let items = History::new(vec![notice]).into_items();
        let restored = serde_json::from_value(serde_json::to_value(items).unwrap()).unwrap();
        let forked = rebase_history(restored).unwrap();

        copy_history_outputs(&storage, &source, &target, [forked.as_slice()]).unwrap();

        assert_eq!(
            store.load_text(target.id(), reference.id.clone()).unwrap(),
            complete
        );
        let messages = history_messages(forked);
        assert_eq!(messages[0].task_event.as_ref(), Some(&origin));
        assert_eq!(messages[0].retained_output_refs, [reference]);
    }

    #[test_case(false; "direct")]
    #[test_case(true; "batch")]
    fn sdk_fork_preserves_selected_task_version_stream(batch: bool) {
        const ROOT_CALL: &str = "root-launch";
        const SELECTED: &str = "selected earlier history";
        const LATEST: &str = "latest must not replace selected";
        let temp = TempDir::new().unwrap();
        let storage = StateDir::from_path(temp.path().into());
        let mut source = StoredSession::new("provider/model", "/repo");
        let target = SessionRef::generate();
        let output = ToolOutput::Tasks(vec![TaskCard::from(&task_record(Value::Null))]);
        let output = if batch {
            serde_json::from_value(serde_json::json!({"Batch": {
                "entries": [{"tool": "task", "summary": PHRASE_TASK, "status": "Success", "output": output}],
                "text": ""
            }})).unwrap()
        } else {
            output
        };
        source.insert_tool_output(ROOT_CALL.into(), output);
        source.set_subagent_history(
            PHRASE_TASK.into(),
            History::new(vec![Message::user(LATEST.into())]).into_items(),
            Some(StoredSubagentTaskSpec::default()),
        );
        let selected = History::new(vec![Message::user(SELECTED.into())]).into_items();
        source.set_subagent_history(
            TASK_CALL.into(),
            selected.clone(),
            Some(StoredSubagentTaskSpec::version()),
        );
        let history = History::new(vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                ROOT_CALL,
                if batch { "batch" } else { "task" },
                serde_json::json!({}),
            )],
            ..Default::default()
        }])
        .into_items();
        save_sdk_fork(
            &storage,
            &source,
            &target,
            &history,
            HashMap::from([(PHRASE_TASK.into(), selected.clone())]),
            None,
            "/repo",
        )
        .unwrap();
        let restored = caudra_agent::load_stored_session(target.id(), &storage).unwrap();
        let versions = caudra_agent::active_task_history_versions_with_outputs(
            restored.messages(),
            |call_id| restored.tool_outputs().get(call_id).map(Arc::as_ref),
        );
        assert_eq!(versions[PHRASE_TASK], TASK_CALL);
        assert_eq!(restored.subagent_messages()[TASK_CALL].as_ref(), &selected);
        assert!(restored.subagent_task_specs()[TASK_CALL].is_version());
        assert_eq!(
            restored.subagent_messages()[PHRASE_TASK].as_ref(),
            &selected
        );
    }

    #[test]
    fn saved_sdk_fork_restores_rebased_subagent_history_and_rejects_collision() {
        let temp = TempDir::new().unwrap();
        let storage = StateDir::from_path(temp.path().to_path_buf());
        let mut source = StoredSession::new("provider/model", "/repo");
        source.meta.system_prompt_profile = Some("review".into());
        source.meta.structured_permission_rules = vec![stored_structured_rule()];
        source.meta.permission_mode = Some(StoredPermissionMode::Yolo);
        source.meta.plan_target = Some(StoredPlanTarget::PlanRef {
            reference: caudra_workspace::PlanRef::new(format!("plan-{}", "a".repeat(32))).unwrap(),
        });
        let target = SessionRef::generate();
        let history = History::new(vec![
            Message::user("main".into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use(
                    "batch-call",
                    "batch",
                    serde_json::json!({}),
                )],
                ..Default::default()
            },
        ])
        .into_items();
        let subagent = History::new(vec![Message::user("nested".into())]).into_items();
        source.insert_tool_output(
            "batch-call".into(),
            caudra_agent::ToolOutput::Plain(caudra_agent::TextOutput {
                text: "batch output".into(),
                instructions: None,
                state: Some(serde_json::json!([])),
                lua_provenance: None,
            }),
        );

        save_sdk_fork(
            &storage,
            &source,
            &target,
            &history,
            HashMap::from([("task-1".into(), subagent.clone())]),
            None,
            "/fork-target",
        )
        .unwrap();

        let loaded = caudra_agent::load_stored_session(target.id(), &storage).unwrap();
        assert_eq!(loaded.messages(), history);
        assert_eq!(loaded.subagent_messages()["task-1"].as_ref(), &subagent);
        assert!(loaded.tool_outputs().contains_key("batch-call"));
        assert!(loaded.meta.structured_permission_rules.is_empty());
        assert_eq!(loaded.meta.permission_mode, None);
        assert_eq!(loaded.cwd, "/fork-target");
        assert!(loaded.meta.plan_target.is_none());
        assert!(loaded.meta.pending_revert.is_none());
        assert_eq!(loaded.meta.system_prompt_profile.as_deref(), Some("review"));
        assert!(ensure_fork_target_available(&storage, &target).is_err());
        assert!(ensure_fork_target_available(&storage, &SessionRef::generate()).is_ok());
    }

    #[test]
    fn sdk_fork_reachability_excludes_other_branches_and_includes_nested_tasks() {
        let mut session = StoredSession::new("provider/model", "/repo");
        let main = History::new(vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                "task-live",
                "task",
                serde_json::json!({}),
            )],
            ..Default::default()
        }])
        .into_items();
        let live = History::new(vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                "task-nested",
                "task",
                serde_json::json!({}),
            )],
            ..Default::default()
        }])
        .into_items();
        session.set_subagent_history(
            "task-live".into(),
            live,
            Some(caudra_storage::sessions::StoredSubagentTaskSpec::default()),
        );
        session.set_subagent_history(
            "task-nested".into(),
            Vec::new(),
            Some(caudra_storage::sessions::StoredSubagentTaskSpec::default()),
        );
        session.set_subagent_history(
            "task-other-branch".into(),
            Vec::new(),
            Some(caudra_storage::sessions::StoredSubagentTaskSpec::default()),
        );

        let reachable = reachable_subagent_ids(&main, &session);

        assert_eq!(
            reachable,
            HashSet::from(["task-live".into(), "task-nested".into()])
        );

        let compacted = History::new(vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "summary".into(),
            }],
            retained_subagent_ids: vec!["task-live".into()],
            is_compaction_summary: true,
            ..Default::default()
        }])
        .into_items();
        assert_eq!(
            reachable_subagent_ids(&compacted, &session),
            HashSet::from(["task-live".into(), "task-nested".into()])
        );
    }

    #[test]
    fn sdk_resume_restores_permissions_while_fork_starts_clean() {
        let mut session = StoredSession::new("provider/model", "/repo");
        session.meta.structured_permission_rules = vec![stored_structured_rule()];
        session.meta.permission_mode = Some(StoredPermissionMode::Yolo);

        let resumed = session_permissions(&session, false);
        let forked = session_permissions(&session, true);

        assert_eq!(
            resumed,
            (
                session.meta.structured_permission_rules.clone(),
                Some(StoredPermissionMode::Yolo)
            )
        );
        assert_eq!(forked, (Vec::new(), None));
    }

    #[test_case(false; "fork_to_local")]
    #[test_case(true; "fork_to_explicit_sandbox")]
    fn sdk_fork_persistence_rebinds_without_source_grants_or_plan(remote_target: bool) {
        let directory = TempDir::new().unwrap();
        let storage = StateDir::from_path(directory.path().join("state"));
        let local = StoredWorkspaceBinding::local_from_cwd("/source");
        let remote = serde_json::from_str::<StoredWorkspaceBinding>(
            &serde_json::to_string(&local)
                .unwrap()
                .replace(local.trust_anchor().as_str(), "https://sandbox.test"),
        )
        .unwrap();
        let mut source = StoredSession::new_with_workspace(
            "provider/model",
            "/source",
            remote
                .clone()
                .with_sandbox_record(SessionRef::generate().id())
                .unwrap(),
        );
        source.meta.structured_permission_rules = vec![stored_structured_rule()];
        source.meta.permission_mode = Some(StoredPermissionMode::Yolo);
        source.meta.mode = Some(StoredMode::Plan);
        source.meta.plan_path = Some(SOURCE_PLAN.into());
        source.meta.plan_target = Some(StoredPlanTarget::LocalPath {
            path: SOURCE_PLAN.into(),
        });
        let binding = remote_target.then(|| {
            remote
                .with_sandbox_record(SessionRef::generate().id())
                .unwrap()
        });
        let history = History::new(vec![Message::user(FORK_HISTORY.into())]).into_items();
        let target = SessionRef::generate();
        let cwd = if remote_target {
            "."
        } else {
            directory.path().to_str().unwrap()
        };
        save_sdk_fork(
            &storage,
            &source,
            &target,
            &history,
            HashMap::new(),
            binding.as_ref(),
            cwd,
        )
        .unwrap();
        let fork = crate::setup::load_session(target.id(), &storage).unwrap();
        StoredWorkspaceBinding::validate_resume_identity(
            fork.workspace_binding(),
            binding.as_ref(),
        )
        .unwrap();
        assert_ne!(fork.workspace_binding(), source.workspace_binding());
        assert!(fork.meta.structured_permission_rules.is_empty());
        assert_eq!(fork.meta.permission_mode, None);
        assert_eq!(fork.meta.plan_target, None);
        assert_eq!(fork.meta.plan_path, None);
        assert_ne!(fork.meta.mode, Some(StoredMode::Plan));
        assert_eq!(
            crate::setup::active_session_history(&fork).unwrap(),
            history
        );
    }

    #[test]
    fn remote_session_mismatch_fails_before_permissions_can_be_reused() {
        let mut session = StoredSession::new("provider/model", "/first");
        session.meta.permission_mode = Some(StoredPermissionMode::Yolo);
        let expected = StoredWorkspaceBinding::local_from_cwd("/second");
        let expected = serde_json::from_str::<StoredWorkspaceBinding>(
            &serde_json::to_string(&expected)
                .unwrap()
                .replace("caudra:local:v1", "https://remote.example"),
        )
        .unwrap();

        let error = validate_workspace_binding(&session, Some(&expected), false).unwrap_err();

        assert_eq!(error.to_string(), WORKSPACE_REBIND_REQUIRED);
    }

    #[test]
    fn remote_session_mismatch_can_be_explicitly_rebound_by_forking() {
        let session = StoredSession::new("provider/model", "/first");
        let expected = StoredWorkspaceBinding::local_from_cwd("/second");

        validate_workspace_binding(&session, Some(&expected), true).unwrap();
    }

    #[test]
    fn sdk_resume_remote_to_embedded_requires_explicit_fork() {
        let local = StoredWorkspaceBinding::local_from_cwd("/remote");
        let remote = serde_json::from_str::<StoredWorkspaceBinding>(
            &serde_json::to_string(&local)
                .unwrap()
                .replace("caudra:local:v1", "https://remote.example"),
        )
        .unwrap();
        let session = StoredSession::new_with_workspace("provider/model", "/remote", remote);
        assert_eq!(
            validate_workspace_binding(&session, None, false)
                .unwrap_err()
                .to_string(),
            WORKSPACE_REBIND_REQUIRED
        );
        validate_workspace_binding(&session, None, true).unwrap();
    }

    #[test]
    fn sdk_profile_precedence_and_raw_override_are_explicit() {
        let catalog = PromptProfileCatalog::default();

        let (name, profile) = resolve_prompt_profile(
            &catalog,
            Some(BUILTIN_PROFILE_NAME),
            Some("missing-stored"),
            Some("missing-config"),
            false,
        )
        .unwrap();
        assert_eq!(name.as_deref(), Some(BUILTIN_PROFILE_NAME));
        assert!(profile.is_none());

        let (name, profile) = resolve_prompt_profile(
            &catalog,
            Some("missing-cli"),
            Some("missing-stored"),
            Some("missing-config"),
            true,
        )
        .unwrap();
        assert!(name.is_none());
        assert!(profile.is_none());
    }

    const MODEL: &str = "test-model";

    fn types(events: &[Value]) -> Vec<&str> {
        events.iter().map(|e| e["type"].as_str().unwrap()).collect()
    }

    #[test]
    fn text_delta_starts_message_and_subsequent_is_delta_only() {
        let mut synth = StreamSynth::new();
        let events = synth.text_delta(MODEL, "hi");
        assert_eq!(
            types(&events),
            [
                "message_start",
                "content_block_start",
                "content_block_delta"
            ]
        );
        assert_eq!(events[0]["message"]["model"], MODEL);
        assert_eq!(events[1]["index"], 0);
        assert_eq!(events[1]["content_block"]["type"], "text");
        assert_eq!(events[2]["delta"]["text"], "hi");

        let more = synth.text_delta(MODEL, "again");
        assert_eq!(types(&more), ["content_block_delta"]);
    }

    #[test]
    fn block_transition_closes_previous_and_increments_index() {
        let mut synth = StreamSynth::new();
        synth.text_delta(MODEL, "a");
        let events = synth.thinking_delta(MODEL, "b");
        assert_eq!(
            types(&events),
            [
                "content_block_stop",
                "content_block_start",
                "content_block_delta"
            ]
        );
        assert_eq!(events[0]["index"], 0);
        assert_eq!(events[1]["index"], 1);
        assert_eq!(events[1]["content_block"]["type"], "thinking");
    }

    #[test]
    fn thinking_boundary_closes_the_block_before_the_next_summary() {
        let mut synth = StreamSynth::new();
        synth.thinking_delta(MODEL, "first");

        let boundary = synth.thinking_boundary();
        let second = synth.thinking_delta(MODEL, "second");

        assert_eq!(types(&boundary), ["content_block_stop"]);
        assert_eq!(
            types(&second),
            ["content_block_start", "content_block_delta"]
        );
        assert_eq!(second[0]["index"], 1);
        assert_eq!(second[0]["content_block"]["type"], "thinking");
    }

    #[test]
    fn tool_use_emits_complete_block() {
        let mut synth = StreamSynth::new();
        synth.text_delta(MODEL, "a");
        let events = synth.tool_use(MODEL, "tool_1", "Read", r#"{"path":"t"}"#);
        assert_eq!(
            types(&events),
            [
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop"
            ]
        );
        assert_eq!(events[1]["content_block"]["type"], "tool_use");
        assert_eq!(events[1]["content_block"]["name"], "Read");
        assert_eq!(events[2]["delta"]["type"], "input_json_delta");
    }

    #[test]
    fn a_pending_call_streams_its_arguments_and_closes_on_start() {
        let mut synth = StreamSynth::new();
        synth.text_delta(MODEL, "a");

        let opened = synth.tool_pending(MODEL, "tool_1", "Read");
        assert_eq!(
            types(&opened),
            ["content_block_stop", "content_block_start"]
        );
        assert_eq!(opened[1]["content_block"]["type"], "tool_use");

        let first = synth.tool_input_delta("tool_1", r#"{"path":"#);
        let second = synth.tool_input_delta("tool_1", r#""t"}"#);
        assert_eq!(first[0]["delta"]["partial_json"], r#"{"path":"#);
        assert_eq!(second[0]["delta"]["partial_json"], r#""t"}"#);
        assert_eq!(second[0]["index"], opened[1]["index"]);

        let closed = synth.tool_use(MODEL, "tool_1", "Read", r#"{"path":"t"}"#);
        assert_eq!(types(&closed), ["content_block_stop"]);
    }

    #[test]
    fn a_fragment_for_a_call_that_is_not_open_is_dropped() {
        let mut synth = StreamSynth::new();
        synth.tool_pending(MODEL, "tool_1", "Read");
        assert!(synth.tool_input_delta("tool_2", "{}").is_empty());
    }

    /// Parallel calls close the previous block as the next opens, so the
    /// trailing `ToolStart` for an earlier call must not reopen it.
    #[test]
    fn a_start_after_the_block_moved_on_emits_nothing() {
        let mut synth = StreamSynth::new();
        synth.tool_pending(MODEL, "t1", "Read");
        synth.tool_input_delta("t1", "{}");
        let reopened = synth.tool_pending(MODEL, "t2", "Write");
        assert_eq!(
            types(&reopened),
            ["content_block_stop", "content_block_start"]
        );

        assert!(synth.tool_use(MODEL, "t1", "Read", "{}").is_empty());
        assert_eq!(
            types(&synth.tool_use(MODEL, "t2", "Write", "{}")),
            ["content_block_stop"]
        );
    }

    /// A call with no pending phase — a batch child, an MCP passthrough —
    /// still gets the whole input in one delta.
    #[test]
    fn an_unannounced_call_still_emits_a_complete_block() {
        let mut synth = StreamSynth::new();
        let events = synth.tool_use(MODEL, "tool_1", "Read", r#"{"path":"t"}"#);
        assert_eq!(
            types(&events),
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop"
            ]
        );
    }

    #[test]
    fn multiple_tool_uses_increment_block_index() {
        let mut synth = StreamSynth::new();
        synth.text_delta(MODEL, "x");
        let t1 = synth.tool_use(MODEL, "t1", "Read", "{}");
        let t2 = synth.tool_use(MODEL, "t2", "Write", "{}");
        let idx = |events: &[Value]| {
            events
                .iter()
                .find(|e| e["type"] == "content_block_start")
                .unwrap()["index"]
                .as_i64()
        };
        assert_eq!(idx(&t1), Some(1));
        assert_eq!(idx(&t2), Some(2));
    }

    #[test]
    fn finish_message_closes_block_and_resets() {
        let mut synth = StreamSynth::new();
        synth.text_delta(MODEL, "a");
        let usage = TokenUsage {
            output: 5,
            ..Default::default()
        };
        let events = synth.finish_message(&usage);
        assert_eq!(
            types(&events),
            ["content_block_stop", "message_delta", "message_stop"]
        );
        assert_eq!(events[1]["usage"]["output_tokens"], 5);

        assert!(synth.finish_message(&usage).is_empty());

        let next = synth.text_delta(MODEL, "new");
        assert_eq!(next[1]["index"], 0);
    }

    #[test]
    fn finish_message_before_start_is_empty() {
        let mut synth = StreamSynth::new();
        assert!(synth.finish_message(&TokenUsage::default()).is_empty());
    }

    #[test]
    fn stream_reset_closes_the_partial_sdk_message() {
        let (mut pump, out_rx, _) =
            permission_event_pump(permission_manager(), PermissionMode::Default);
        pump.include_partial_messages = true;
        pump.handle(Envelope {
            event: AgentEvent::TextDelta {
                text: "partial".into(),
            },
            subagent: None,
            run_id: 0,
            task: None,
            workflow: None,
        })
        .unwrap();
        pump.handle(Envelope {
            event: AgentEvent::StreamReset,
            subagent: None,
            run_id: 0,
            task: None,
            workflow: None,
        })
        .unwrap();

        let event_types = out_rx
            .try_iter()
            .map(|line| {
                serde_json::from_str::<Value>(&line).unwrap()["event"]["type"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            event_types,
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert!(pump.synth.finish_message(&TokenUsage::default()).is_empty());
    }

    #[test]
    fn tool_use_on_fresh_synth_has_no_spurious_stop() {
        let mut synth = StreamSynth::new();
        let events = synth.tool_use(MODEL, "t1", "Read", r#"{"path":"x"}"#);
        assert_eq!(events[0]["type"], "message_start");
        let start_pos = events
            .iter()
            .position(|e| e["type"] == "content_block_start")
            .unwrap();
        let stop_pos = events
            .iter()
            .position(|e| e["type"] == "content_block_stop")
            .unwrap();
        assert!(stop_pos > start_pos);
    }

    #[test_case("default", PermissionMode::Default)]
    #[test_case("auto", PermissionMode::Auto)]
    #[test_case("acceptEdits", PermissionMode::AcceptEdits)]
    #[test_case("plan", PermissionMode::Plan)]
    #[test_case("bypassPermissions", PermissionMode::BypassPermissions)]
    fn permission_mode_roundtrip(s: &str, mode: PermissionMode) {
        assert_eq!(PermissionMode::parse(s), Some(mode));
        assert_eq!(mode.as_str(), s);
    }

    #[test]
    fn permission_mode_resolve() {
        assert_eq!(
            PermissionMode::resolve(None, false, false),
            PermissionMode::Default
        );
        assert_eq!(
            PermissionMode::resolve(None, true, false),
            PermissionMode::BypassPermissions
        );
        assert_eq!(
            PermissionMode::resolve(Some("plan"), true, false),
            PermissionMode::Plan
        );
        assert_eq!(
            PermissionMode::resolve(Some("bogus"), false, false),
            PermissionMode::Default
        );
    }

    #[test_case(PermissionMode::Default, StoredPermissionMode::Yolo => PermissionMode::BypassPermissions ; "stored_yolo_is_reported")]
    #[test_case(PermissionMode::BypassPermissions, StoredPermissionMode::Ask => PermissionMode::Default ; "effective_ask_is_reported")]
    #[test_case(PermissionMode::Plan, StoredPermissionMode::Yolo => PermissionMode::Plan ; "plan_mode_is_preserved")]
    #[test_case(PermissionMode::Default, StoredPermissionMode::Auto => PermissionMode::Auto ; "stored_auto_is_reported")]
    #[test_case(PermissionMode::Plan, StoredPermissionMode::Auto => PermissionMode::Plan ; "auto_keeps_plan")]
    fn effective_mode_tracks_restored_permissions(
        requested: PermissionMode,
        mode: StoredPermissionMode,
    ) -> PermissionMode {
        effective_permission_mode(requested, mode)
    }

    #[test_case(&["caudra"], None, None; "unset_stays_unset")]
    #[test_case(&["caudra"], Some(StoredPermissionMode::Ask), Some(StoredPermissionMode::Ask); "explicit_ask_restores")]
    #[test_case(&["caudra"], Some(StoredPermissionMode::Auto), Some(StoredPermissionMode::Auto); "auto_restores")]
    #[test_case(&["caudra", "--auto"], Some(StoredPermissionMode::Ask), Some(StoredPermissionMode::Auto); "auto_overrides_ask")]
    #[test_case(&["caudra", "--auto"], Some(StoredPermissionMode::Yolo), Some(StoredPermissionMode::Auto); "auto_overrides_yolo")]
    #[test_case(&["caudra", "--yolo"], Some(StoredPermissionMode::Auto), Some(StoredPermissionMode::Yolo); "yolo_overrides_auto")]
    #[test_case(&["caudra", "--permission-mode", "default"], Some(StoredPermissionMode::Auto), Some(StoredPermissionMode::Ask); "default_is_explicit_ask")]
    #[test_case(&["caudra", "--permission-mode", "auto"], Some(StoredPermissionMode::Ask), Some(StoredPermissionMode::Auto); "sdk_auto_overrides_ask")]
    fn explicit_startup_permission_mode_wins(
        args: &[&str],
        restored: Option<StoredPermissionMode>,
        expected: Option<StoredPermissionMode>,
    ) {
        let cli = Cli::try_parse_from(args).unwrap();
        let requested = PermissionMode::resolve(cli.permission_mode.as_deref(), cli.yolo, cli.auto);
        assert_eq!(startup_permission_mode(&cli, requested, restored), expected);
    }

    #[test_case(PermissionMode::Plan, PermissionMode::Plan; "plan_is_preserved")]
    #[test_case(PermissionMode::Default, PermissionMode::Auto; "build_enters_auto")]
    fn auto_control_preserves_plan_execution(current: PermissionMode, expected: PermissionMode) {
        assert_eq!(PermissionMode::Auto.preserve_plan(current), expected);
        assert_eq!(
            PermissionMode::Auto.storage_mode(),
            StoredPermissionMode::Auto
        );
    }

    #[test]
    fn content_text_extracts_from_all_shapes() {
        assert_eq!(content_text(&serde_json::json!("hi")), Some("hi".into()));
        let blocks = serde_json::json!([
            {"type": "text", "text": "a"},
            {"type": "image", "source": {}},
            {"type": "text", "text": "b"},
        ]);
        assert_eq!(content_text(&blocks), Some("a\nb".into()));
        assert_eq!(content_text(&serde_json::json!(42)), None);
    }

    #[test]
    fn content_images_extracts_base64_blocks() {
        let blocks = serde_json::json!([
            {"type": "text", "text": "look at this"},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
            {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "BBBB"}},
        ]);
        let images = content_images(&blocks);
        assert_eq!(images.len(), 2);
        assert_eq!(&*images[0].data, "AAAA");
        assert_eq!(&*images[1].data, "BBBB");

        // Non-array content and malformed image blocks yield no images.
        assert!(content_images(&serde_json::json!("hi")).is_empty());
        let bad = serde_json::json!([{"type": "image", "source": {"data": "x"}}]);
        assert!(content_images(&bad).is_empty());
    }

    #[test]
    fn wire_result_serializes_correctly() {
        let msg = WireMessage {
            inner: WireInner::Result(ResultPayload {
                run: None,
                background_active: None,
                subtype: "success",
                is_error: false,
                duration_ms: 1000,
                duration_api_ms: 1000,
                num_turns: 1,
                result: "done".into(),
                total_cost_usd: 0.01,
                subscription_cost_usd: 0.0,
                usage: TokenUsage::default(),
                permission_denials: Vec::new(),
            }),
            session_id: SessionRef::generate(),
            uuid: "u".into(),
        };
        let json: Value = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "result");
        assert_eq!(json["subtype"], "success");
        assert_eq!(json["num_turns"], 1);
        assert!(json.get("session_id").is_some());
    }

    #[test]
    fn direct_remote_command_emits_a_zero_turn_result() {
        let (out_tx, out_rx) = flume::unbounded();
        let writer = SdkWriter {
            session_id: SessionRef::generate(),
            out_tx,
        };
        writer
            .emit_direct_command_result(
                headless::RemoteCommandOutput {
                    output: "remote output".into(),
                    is_error: false,
                },
                12,
            )
            .unwrap();
        let json: Value = serde_json::from_str(&out_rx.recv().unwrap()).unwrap();

        assert_eq!(json["type"], "result");
        assert_eq!(json["subtype"], "success");
        assert_eq!(json["num_turns"], 0);
        assert_eq!(json["result"], "remote output");
        assert_eq!(json["duration_ms"], 12);
    }

    #[test]
    fn wire_init_serializes_correctly() {
        let msg = WireMessage {
            inner: WireInner::System(SystemPayload {
                subtype: "init",
                extra: serde_json::json!({
                    "cwd": "/tmp",
                    "tools": ["Read"],
                    "model": "test",
                    "permissionMode": "default",
                }),
            }),
            session_id: SessionRef::generate(),
            uuid: "u".into(),
        };
        let json: Value = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "system");
        assert_eq!(json["subtype"], "init");
        assert_eq!(json["cwd"], "/tmp");
    }

    #[test]
    fn wire_control_response_serializes() {
        let msg = WireMessage {
            inner: WireInner::ControlResponse(ControlResponsePayload {
                response: ControlResponseInner {
                    subtype: "success",
                    request_id: "req_1".into(),
                    response: Some(serde_json::json!({"commands": []})),
                    error: None,
                },
            }),
            session_id: SessionRef::generate(),
            uuid: "u".into(),
        };
        let json: Value = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "control_response");
        assert_eq!(json["response"]["request_id"], "req_1");
    }

    #[test]
    fn wire_control_request_serializes() {
        let msg = WireMessage {
            inner: WireInner::ControlRequest(ControlRequestPayload {
                request_id: "req_5".into(),
                request: ControlRequestInner {
                    task: None,
                    subtype: "can_use_tool",
                    tool_name: Some("Read".into()),
                    input: Some(serde_json::json!({"path": "/tmp"})),
                    tool_use_id: Some("tool_123".into()),
                },
            }),
            session_id: SessionRef::generate(),
            uuid: "u".into(),
        };
        let json: Value = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "control_request");
        assert_eq!(json["request"]["subtype"], "can_use_tool");
        assert_eq!(json["request"]["tool_name"], "Read");
    }

    #[test_case("claude-opus-4-6", "anthropic/claude-opus-4-6"; "claude_prefix")]
    #[test_case("openai/gpt-4", "openai/gpt-4"; "explicit_provider")]
    #[test_case("gpt-4o", "gpt-4o"; "unknown_passthrough")]
    fn resolve_model_spec_cases(input: &str, expected: &str) {
        assert_eq!(resolve_model_spec(input), expected);
    }

    #[test]
    fn decode_permission_response_variants() {
        assert_eq!(
            decode_permission_response(&serde_json::json!({"behavior": "allow"}), None, None),
            exact_permission_allow(PermissionLifetime::Once)
        );
        assert_eq!(
            decode_permission_response(
                &serde_json::json!({"behavior": "allow", "updatedPermissions": []}),
                None,
                None,
            ),
            exact_permission_allow(PermissionLifetime::Once)
        );
        assert!(matches!(
            decode_permission_response(&serde_json::json!({}), None, None),
            PermissionAnswer::Deny
        ));
        assert!(matches!(
            decode_permission_response(
                &serde_json::json!({"behavior": "something_else"}),
                None,
                None,
            ),
            PermissionAnswer::Deny
        ));
        match decode_permission_response(
            &serde_json::json!({"behavior": "deny", "message": "not now"}),
            None,
            None,
        ) {
            PermissionAnswer::DenyWithGuidance(msg) => assert_eq!(msg, "not now"),
            other => panic!("expected guidance, got {other:?}"),
        }
        let expected = serde_json::json!({"command": "cargo test"});
        assert!(matches!(
            decode_permission_response(
                &serde_json::json!({
                    "behavior": "allow",
                    "updatedInput": {"command": "cargo publish"}
                }),
                Some(&expected),
                Some("Bash"),
            ),
            PermissionAnswer::Deny
        ));
        assert_eq!(
            decode_permission_response(
                &serde_json::json!({
                    "behavior": "allow",
                    "updatedInput": expected,
                    "updatedPermissions": [{
                        "type": "addRules",
                        "behavior": "allow",
                        "destination": "session",
                        "rules": [{"toolName": "Bash"}]
                    }]
                }),
                Some(&serde_json::json!({"command": "cargo test"})),
                Some("Bash"),
            ),
            exact_permission_allow(PermissionLifetime::Conversation)
        );
        assert!(matches!(
            decode_permission_response(
                &serde_json::json!({
                    "behavior": "allow",
                    "updatedPermissions": [{
                        "type": "addRules",
                        "behavior": "allow",
                        "destination": "session",
                        "rules": [{"toolName": "Read"}]
                    }]
                }),
                Some(&serde_json::json!({"command": "cargo test"})),
                Some("Bash"),
            ),
            PermissionAnswer::Deny
        ));
    }

    #[test]
    fn sdk_permission_request_uses_full_structured_input() {
        let manager = permission_manager();
        let (mut pump, out_rx, shared) =
            permission_event_pump(Arc::clone(&manager), PermissionMode::Default);
        let input = serde_json::json!({
            "command": "x".repeat(512),
            "nested": {"complete": true, "values": [1, 2, 3]}
        });
        let request = PermissionRequest::from_legacy(
            CAUDRA_REQUEST_ID.into(),
            caudra_config::ToolKey::native("bash"),
            vec!["cargo test".into()],
            input.clone(),
            Path::new("/project"),
            false,
        );

        pump.handle(Envelope {
            event: AgentEvent::PermissionRequest(Box::new(request)),
            subagent: None,
            run_id: 0,
            task: None,
            workflow: None,
        })
        .unwrap();

        let message: Value = serde_json::from_str(&out_rx.try_recv().unwrap()).unwrap();
        assert_eq!(message["type"], "control_request");
        assert_eq!(message["request"]["subtype"], "can_use_tool");
        assert_eq!(message["request"]["input"], input);
        assert_eq!(message["request"]["tool_use_id"], CAUDRA_REQUEST_ID);
        assert_eq!(shared.lock().unwrap().pending["req_1"], CAUDRA_REQUEST_ID);
    }

    #[test]
    fn sdk_permission_coverage_update_preserves_the_outstanding_request() {
        let manager = permission_manager();
        let (mut pump, out_rx, shared) = permission_event_pump(manager, PermissionMode::Default);
        let request = PermissionRequest::from_legacy(
            CAUDRA_REQUEST_ID.into(),
            caudra_config::ToolKey::native("bash"),
            vec!["cargo test".into()],
            serde_json::json!({"command": "cargo test"}),
            Path::new("/project"),
            false,
        );
        for event in [
            AgentEvent::PermissionRequest(Box::new(request.clone())),
            AgentEvent::PermissionRequestUpdated(Box::new(request)),
        ] {
            pump.handle(Envelope {
                event,
                subagent: None,
                run_id: 0,
                task: None,
                workflow: None,
            })
            .unwrap();
        }
        assert_eq!(out_rx.len(), 1);
        let shared = shared.lock().unwrap();
        assert_eq!(shared.pending.len(), 1);
        assert_eq!(shared.pending["req_1"], CAUDRA_REQUEST_ID);
        assert!(shared.resolved_permission_requests.is_empty());
    }

    #[test]
    fn sdk_permission_responses_correlate_concurrent_requests_out_of_order() {
        smol::block_on(async {
            let manager = permission_manager();
            let (first, first_events) = pending_permission(
                Arc::clone(&manager),
                CAUDRA_REQUEST_ID,
                "cargo test",
                serde_json::json!({"command": "cargo test"}),
            );
            let (second, second_events) = pending_permission(
                Arc::clone(&manager),
                SECOND_CAUDRA_REQUEST_ID,
                "cargo check",
                serde_json::json!({"command": "cargo check"}),
            );
            let _ = first_events.recv_async().await.unwrap();
            let _ = second_events.recv_async().await.unwrap();
            let shared = shared_with_pending(HashMap::from([
                ("req_1".into(), CAUDRA_REQUEST_ID.into()),
                ("req_2".into(), SECOND_CAUDRA_REQUEST_ID.into()),
            ]));

            answer_permission_response(
                &shared,
                &manager,
                InboundControlResponseInner {
                    subtype: "success".into(),
                    request_id: "req_2".into(),
                    response: serde_json::json!({"behavior": "allow"}),
                },
            );
            answer_permission_response(
                &shared,
                &manager,
                InboundControlResponseInner {
                    subtype: "success".into(),
                    request_id: "req_1".into(),
                    response: serde_json::json!({"behavior": "unknown"}),
                },
            );

            assert!(!first.await, "unknown response denies its request");
            assert!(second.await, "out-of-order allow reaches its request");
            assert!(shared.lock().unwrap().pending.is_empty());
        });
    }

    #[test]
    fn sdk_reusable_approval_cancels_a_covered_permission_callback() {
        smol::block_on(async {
            let manager = permission_manager();
            let input = serde_json::json!({"command": "cargo test"});
            let (first, first_events) = pending_permission(
                Arc::clone(&manager),
                CAUDRA_REQUEST_ID,
                "cargo test",
                input.clone(),
            );
            let (second, second_events) = pending_permission(
                Arc::clone(&manager),
                SECOND_CAUDRA_REQUEST_ID,
                "cargo test",
                input,
            );
            let first_request = first_events.recv_async().await.unwrap();
            let second_request = second_events.recv_async().await.unwrap();
            let (mut pump, out_rx, shared) =
                permission_event_pump(Arc::clone(&manager), PermissionMode::Default);
            pump.handle(first_request).unwrap();
            pump.handle(second_request).unwrap();
            let _: Value = serde_json::from_str(&out_rx.recv_async().await.unwrap()).unwrap();
            let _: Value = serde_json::from_str(&out_rx.recv_async().await.unwrap()).unwrap();

            answer_permission_response(
                &shared,
                &manager,
                InboundControlResponseInner {
                    subtype: "success".into(),
                    request_id: "req_1".into(),
                    response: serde_json::json!({
                        "behavior": "allow",
                        "updatedPermissions": [{
                            "type": "addRules",
                            "rules": [{"toolName": "Shell"}],
                            "behavior": "allow",
                            "destination": "session"
                        }]
                    }),
                },
            );
            pump.handle(second_events.recv_async().await.unwrap())
                .unwrap();

            assert!(first.await);
            assert!(second.await);
            assert!(shared.lock().unwrap().pending.is_empty());
            let cancelled: Value =
                serde_json::from_str(&out_rx.recv_async().await.unwrap()).unwrap();
            assert_eq!(cancelled["type"], "control_cancel_request");
            assert_eq!(cancelled["request_id"], "req_2");
            assert!(answer_pending_permission(
                &shared,
                &manager,
                "req_2",
                PermissionAnswer::Deny,
            ));
        });
    }

    #[test]
    fn sdk_updated_permissions_grant_only_exact_conversation_scope() {
        smol::block_on(async {
            let manager = permission_manager();
            let (task, events) = pending_permission(
                Arc::clone(&manager),
                CAUDRA_REQUEST_ID,
                "cargo test",
                serde_json::json!({"command": "cargo test"}),
            );
            let _ = events.recv_async().await.unwrap();
            let shared =
                shared_with_pending(HashMap::from([("req_1".into(), CAUDRA_REQUEST_ID.into())]));

            answer_permission_response(
                &shared,
                &manager,
                InboundControlResponseInner {
                    subtype: "success".into(),
                    request_id: "req_1".into(),
                    response: serde_json::json!({
                        "behavior": "allow",
                        "updatedPermissions": [{
                            "type": "addRules",
                            "rules": [{"toolName": "Shell"}],
                            "behavior": "allow",
                            "destination": "session"
                        }]
                    }),
                },
            );

            assert!(task.await);
            assert!(enforcement_without_answer(&manager, "cargo test").await);
            assert!(!enforcement_without_answer(&manager, "cargo publish").await);
        });
    }

    #[test]
    fn sdk_permission_cancellation_denies_the_correlated_request() {
        smol::block_on(async {
            let manager = permission_manager();
            let (task, events) = pending_permission(
                Arc::clone(&manager),
                CAUDRA_REQUEST_ID,
                "cargo test",
                serde_json::json!({"command": "cargo test"}),
            );
            let _ = events.recv_async().await.unwrap();
            let shared =
                shared_with_pending(HashMap::from([("req_1".into(), CAUDRA_REQUEST_ID.into())]));

            assert!(answer_pending_permission(
                &shared,
                &manager,
                "req_1",
                PermissionAnswer::Deny
            ));
            assert!(!task.await);
            assert!(shared.lock().unwrap().pending.is_empty());
        });
    }

    #[test]
    fn sdk_bypass_answers_the_structured_request_id() {
        smol::block_on(async {
            let manager = permission_manager();
            let (task, events) = pending_permission(
                Arc::clone(&manager),
                CAUDRA_REQUEST_ID,
                "cargo test",
                serde_json::json!({"command": "cargo test"}),
            );
            let envelope = events.recv_async().await.unwrap();
            let (mut pump, out_rx, _) =
                permission_event_pump(Arc::clone(&manager), PermissionMode::BypassPermissions);

            pump.handle(envelope).unwrap();

            assert!(task.await);
            assert!(out_rx.is_empty());
        });
    }

    #[test]
    fn sdk_control_response_deserializes_claude_wire_envelope() {
        let response: InboundControlResponse = serde_json::from_value(serde_json::json!({
            "response": {
                "subtype": "success",
                "request_id": "req_1",
                "response": {"behavior": "allow", "updatedInput": {}}
            }
        }))
        .unwrap();

        assert_eq!(response.response.request_id, "req_1");
        assert_eq!(response.response.subtype, "success");
        assert_eq!(response.response.response["behavior"], "allow");
    }

    #[test]
    fn resolve_set_model_null_returns_startup() {
        let startup = Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap();
        let result = resolve_set_model(
            Some(&Value::Null),
            &startup,
            &caudra_config::ModelPolicy::default(),
        )
        .unwrap();
        assert_eq!(result.id, startup.id);
    }

    #[test]
    fn resolve_set_model_rejects_disallowed_exact_spec() {
        let startup = Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap();
        let raw: caudra_config::RawConfig = serde_json::from_value(serde_json::json!({
            "provider": {"allowed_models": [startup.spec()]}
        }))
        .unwrap();
        let policy = raw.into_config(false).unwrap().provider.model_policy;

        assert!(
            resolve_set_model(
                Some(&Value::String("openai/gpt-5".into())),
                &startup,
                &policy
            )
            .is_none()
        );
    }

    #[test]
    fn map_tool_names_in_content_maps_known_and_preserves_rest() {
        let content = serde_json::json!([
            {"type": "text", "text": "hello"},
            {"type": "tool_use", "name": "file_read", "id": "1", "input": {}},
            {"type": "tool_use", "name": "unknown_native", "id": "2", "input": {}},
        ]);
        let mapped = map_tool_names_in_content(&content);
        assert_eq!(mapped[0]["type"], "text");
        assert_eq!(mapped[1]["name"], "FileRead");
        assert_eq!(mapped[2]["name"], "unknown_native");
    }

    const RUN_ID: &str = "run-1";
    const WORKFLOW_NAME: &str = "review";
    const DIGEST: &str = "sha256:abc";
    const EPOCH: u64 = 2;
    const CALL_KEY: u64 = 7;
    const PHASE: &str = "Gather";
    const REVISION: u64 = 3;
    const AGENT_BUDGET: u32 = 4;
    const UNKNOWN_RUN_MESSAGE: &str = "unknown workflow run \"run-1\"";

    fn provenance() -> WorkflowProvenance {
        WorkflowProvenance {
            run_id: RUN_ID.into(),
            epoch: EPOCH,
            call_key: CALL_KEY,
            phase: Some(PHASE.into()),
        }
    }

    fn snapshot() -> RunSnapshot {
        RunSnapshot {
            run_id: RUN_ID.into(),
            display_name: WORKFLOW_NAME.into(),
            workflow_name: WORKFLOW_NAME.into(),
            source_kind: SourceKind::Project,
            source_path: None,
            objective: None,
            status: RunStatus::Completed,
            pause_kind: None,
            pause_message: None,
            revision: REVISION,
            execution_epoch: EPOCH,
            phase: Some(PHASE.into()),
            phases: vec![PHASE.into()],
            phase_history: Vec::new(),
            agent_budget: AGENT_BUDGET,
            usage: RunUsage::default(),
            roster: Vec::new(),
            result: None,
            error: None,
            logs: Vec::new(),
            outbox_pending: true,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn workflow_envelope(event: AgentEvent, subagent: Option<SubagentInfo>) -> Envelope {
        Envelope {
            event,
            subagent,
            run_id: WORKFLOW_EVENT_RUN_ID,
            task: None,
            workflow: Some(provenance()),
        }
    }

    fn next_message(out_rx: &Receiver<String>) -> Value {
        serde_json::from_str(&out_rx.try_recv().unwrap()).unwrap()
    }

    #[test]
    fn a_workflow_snapshot_is_a_system_workflow_message_keyed_by_run() {
        let (mut pump, out_rx, _) =
            permission_event_pump(permission_manager(), PermissionMode::Default);

        pump.handle(workflow_envelope(
            AgentEvent::Workflow(Box::new(WorkflowEvent::Snapshot(Box::new(snapshot())))),
            None,
        ))
        .unwrap();

        let message = next_message(&out_rx);
        assert_eq!(message["type"], "system");
        assert_eq!(message["subtype"], WORKFLOW_SYSTEM_SUBTYPE);
        assert_eq!(message["event"]["kind"], "snapshot");
        assert_eq!(message["event"]["run_id"], RUN_ID);
        assert_eq!(message["event"]["status"], "completed");
        assert_eq!(message["workflow_run_id"], RUN_ID);
        assert_eq!(message["workflow_epoch"], EPOCH);
        assert_eq!(message["workflow_call_key"], CALL_KEY);
        assert_eq!(message["workflow_phase"], PHASE);
        assert!(out_rx.is_empty());
    }

    #[test]
    fn a_workflow_child_event_keeps_its_parent_and_workflow_keys() {
        let (mut pump, out_rx, _) =
            permission_event_pump(permission_manager(), PermissionMode::Default);

        pump.handle(workflow_envelope(
            AgentEvent::ToolResultsSubmitted {
                message: Box::new(Message::user("done".into())),
            },
            Some(SubagentInfo {
                parent_tool_use_id: "wf-call".into(),
                task_id: "task-1".into(),
                name: "worker".into(),
                prompt: None,
                model: None,
                thinking: None,
                fast: false,
                answer_tx: None,
                steer_tx: None,
            }),
        ))
        .unwrap();

        let message = next_message(&out_rx);
        assert_eq!(message["type"], "user");
        assert_eq!(message["parent_tool_use_id"], "wf-call");
        assert_eq!(message["workflow_run_id"], RUN_ID);
        assert_eq!(message["workflow_epoch"], EPOCH);
    }

    /// A consumer cannot otherwise tell a task's backoff from the main
    /// conversation's, and the key stays absent for the main one so older
    /// readers see the shape they always did.
    #[test_case(None => None ; "main_retry_omits_the_key")]
    #[test_case(Some(RETRY_PARENT) => Some(RETRY_PARENT.to_owned()) ; "subagent_retry_names_its_parent")]
    fn api_retry_attributes_the_stream_that_backed_off(parent: Option<&str>) -> Option<String> {
        let (mut pump, out_rx, _) =
            permission_event_pump(permission_manager(), PermissionMode::Default);

        pump.handle(Envelope {
            event: AgentEvent::Retry {
                attempt: 1,
                message: RETRY_ERROR.into(),
                delay_ms: RETRY_DELAY_MS,
            },
            subagent: parent.map(|id| SubagentInfo {
                parent_tool_use_id: id.into(),
                task_id: id.into(),
                name: id.into(),
                prompt: None,
                model: None,
                thinking: None,
                fast: false,
                answer_tx: None,
                steer_tx: None,
            }),
            run_id: 1,
            task: None,
            workflow: None,
        })
        .unwrap();

        let message = next_message(&out_rx);
        assert_eq!(message["subtype"], "api_retry");
        assert_eq!(message["error"], RETRY_ERROR);
        message
            .get(PARENT_KEY)
            .map(|id| id.as_str().expect("serialized as a string").to_owned())
    }

    #[test_case(WORKFLOW_LIST, serde_json::json!({}) => WorkflowRequest::List; "list")]
    #[test_case(WORKFLOW_VALIDATE, serde_json::json!({"name": WORKFLOW_NAME}) => WorkflowRequest::Validate { name: WORKFLOW_NAME.into() }; "validate")]
    #[test_case(WORKFLOW_START, serde_json::json!({"name": WORKFLOW_NAME, "args": {"branch": "main"}, "agent_budget": AGENT_BUDGET}) => WorkflowRequest::Start(LaunchRequest { name: WORKFLOW_NAME.into(), args: serde_json::json!({"branch": "main"}), agent_budget: Some(AGENT_BUDGET) }); "start")]
    #[test_case(WORKFLOW_START, serde_json::json!({"name": WORKFLOW_NAME}) => WorkflowRequest::Start(LaunchRequest { name: WORKFLOW_NAME.into(), args: serde_json::json!({}), agent_budget: None }); "start_defaults_to_empty_args")]
    #[test_case(WORKFLOW_STATUS, serde_json::json!({}) => WorkflowRequest::Status { run_id: None }; "status_all")]
    #[test_case(WORKFLOW_STATUS, serde_json::json!({"run_id": RUN_ID}) => WorkflowRequest::Status { run_id: Some(RUN_ID.into()) }; "status_one")]
    #[test_case(WORKFLOW_INSPECT, serde_json::json!({"run_id": RUN_ID}) => WorkflowRequest::Inspect { run_id: RUN_ID.into() }; "inspect")]
    #[test_case(WORKFLOW_HISTORY, serde_json::json!({"limit": 5}) => WorkflowRequest::History { limit: Some(5) }; "history_with_limit")]
    #[test_case(WORKFLOW_HISTORY, serde_json::json!({}) => WorkflowRequest::History { limit: None }; "history_defaults")]
    #[test_case(WORKFLOW_PAUSE, serde_json::json!({"run_id": RUN_ID}) => WorkflowRequest::Pause { run_id: RUN_ID.into() }; "pause")]
    #[test_case(WORKFLOW_RESUME, serde_json::json!({"run_id": RUN_ID, "agent_budget": AGENT_BUDGET}) => WorkflowRequest::Resume { run_id: RUN_ID.into(), agent_budget: Some(AGENT_BUDGET) }; "resume")]
    #[test_case(WORKFLOW_STOP, serde_json::json!({"run_id": RUN_ID}) => WorkflowRequest::Stop { run_id: RUN_ID.into() }; "stop")]
    #[test_case(WORKFLOW_TRUST, serde_json::json!({"name": WORKFLOW_NAME, "digest": DIGEST}) => WorkflowRequest::Trust { name: WORKFLOW_NAME.into(), digest: DIGEST.into() }; "trust")]
    #[test_case(WORKFLOW_ACK, serde_json::json!({"run_id": RUN_ID, "revision": REVISION}) => WorkflowRequest::AckCompletion { run_id: RUN_ID.into(), revision: REVISION }; "ack")]
    fn workflow_controls_map_to_runtime_requests(subtype: &str, extra: Value) -> WorkflowRequest {
        workflow_request(subtype, &extra).unwrap().unwrap()
    }

    #[test_case(WORKFLOW_VALIDATE, serde_json::json!({}); "validate_without_name")]
    #[test_case(WORKFLOW_TRUST, serde_json::json!({"name": WORKFLOW_NAME}); "trust_without_digest")]
    #[test_case(WORKFLOW_ACK, serde_json::json!({"run_id": RUN_ID}); "ack_without_revision")]
    #[test_case(WORKFLOW_INSPECT, serde_json::json!({}); "inspect_without_run_id")]
    #[test_case(WORKFLOW_STOP, serde_json::json!({"run_id": 7}); "stop_with_a_non_string_run_id")]
    fn a_workflow_control_missing_a_field_is_refused_by_name(subtype: &str, extra: Value) {
        let message = workflow_request(subtype, &extra).unwrap().unwrap_err();
        assert!(message.starts_with(subtype), "got: {message}");
    }

    #[test]
    fn a_non_workflow_subtype_is_not_a_workflow_control() {
        assert!(workflow_request("interrupt", &serde_json::json!({})).is_none());
    }

    #[test]
    fn a_runtime_error_is_an_error_control_response_with_the_structured_error() {
        let (out_tx, out_rx) = flume::unbounded();
        let writer = SdkWriter {
            session_id: SessionRef::generate(),
            out_tx,
        };
        let (response, error) = workflow_control_response(Err(WorkflowError::UnknownRun {
            run_id: RUN_ID.into(),
        }));

        writer
            .emit_control_response("req_9", response, error)
            .unwrap();

        let message = next_message(&out_rx);
        assert_eq!(message["type"], "control_response");
        assert_eq!(message["response"]["subtype"], "error");
        assert_eq!(message["response"]["request_id"], "req_9");
        assert_eq!(message["response"]["error"], UNKNOWN_RUN_MESSAGE);
        assert_eq!(
            message["response"]["response"]["workflow_error"],
            serde_json::json!({"kind": "unknown_run", "detail": {"run_id": RUN_ID}})
        );
    }

    #[test]
    fn a_runtime_answer_is_a_success_control_response_under_workflow() {
        let (out_tx, out_rx) = flume::unbounded();
        let writer = SdkWriter {
            session_id: SessionRef::generate(),
            out_tx,
        };
        let (response, error) = workflow_control_response(Ok(WorkflowResponse::Acked(true)));

        writer
            .emit_control_response("req_9", response, error)
            .unwrap();

        let message = next_message(&out_rx);
        assert_eq!(message["response"]["subtype"], "success");
        assert_eq!(
            message["response"]["response"]["workflow"],
            serde_json::json!({"kind": "acked", "detail": true})
        );
    }

    #[test]
    fn a_control_without_a_runtime_answers_unavailable() {
        let (out_tx, out_rx) = flume::unbounded();
        let writer = SdkWriter {
            session_id: SessionRef::generate(),
            out_tx,
        };

        forward_control(
            &writer,
            "req_1",
            async { Err(WorkflowError::Unavailable) },
            workflow_control_response,
        );

        let message: Value =
            serde_json::from_str(&smol::block_on(out_rx.recv_async()).unwrap()).unwrap();
        assert_eq!(message["response"]["subtype"], "error");
        assert_eq!(
            message["response"]["error"],
            WorkflowError::Unavailable.to_string()
        );
    }

    #[test_case(true => (true, WORKFLOW_CONTROLS.len()); "with_a_runtime")]
    #[test_case(false => (false, 0); "without_a_runtime")]
    fn init_advertises_workflow_support(workflows: bool) -> (bool, usize) {
        let payload = init_payload(
            serde_json::json!({"cwd": "/tmp"}),
            workflows,
            false,
            &AgentConfig::default(),
            false,
        );
        assert_eq!(payload["cwd"], "/tmp");
        assert_eq!(
            payload["goal_controls"],
            serde_json::json!([GOAL_SET, GOAL_CLEAR, GOAL_STATUS])
        );
        (
            payload["workflows"].as_bool().unwrap(),
            payload["workflow_controls"].as_array().unwrap().len(),
        )
    }

    const GOAL_CONDITION: &str = "the suite passes";
    const OTHER_GOAL_CONDITION: &str = "the suite is fast";
    const GOAL_REASON: &str = "two tests still fail";
    const GOAL_MODEL: &str = "anthropic/claude-haiku";
    const GOAL_FAILURE: &str = "evaluator returned no text";
    const GOAL_COST: f64 = 0.125;
    const GOAL_EVALUATION: u32 = 2;
    const GOAL_CONTINUATIONS: u32 = 1;
    const GOAL_LIMIT: u32 = 4;
    const GOAL_DURATION_MS: u64 = 1_500;
    const GOAL_TASKS: usize = 2;
    const GOAL_CWD: &str = "/project";
    static GOAL_THINKING: ThinkingConfig = ThinkingConfig::Off;

    fn goal_result() -> GoalResult {
        GoalResult {
            condition: Arc::from(GOAL_CONDITION),
            verdict: GoalVerdict::Met,
            reason: Arc::from(GOAL_REASON),
            evaluations: GOAL_EVALUATION,
            duration: Duration::from_millis(GOAL_DURATION_MS),
            usage: TokenUsage::default(),
            cost: Some(GOAL_COST),
            subscription_cost: None,
        }
    }

    /// A goal event, the `goal` message fields it must become, and the
    /// spend it adds to the turn.
    fn goal_event(kind: &str) -> (AgentEvent, Value, Option<f64>) {
        match kind {
            "evaluating" => (
                AgentEvent::GoalEvaluating {
                    evaluation: GOAL_EVALUATION,
                },
                serde_json::json!({"evaluation": GOAL_EVALUATION}),
                None,
            ),
            "evaluation" => (
                AgentEvent::GoalEvaluation {
                    verdict: GoalVerdict::NotMet,
                    reason: GOAL_REASON.into(),
                    evaluation: GOAL_EVALUATION,
                    applied: true,
                    usage: TokenUsage::default(),
                    cost: Some(GOAL_COST),
                    billing: Billing::Api,
                    model: GOAL_MODEL.into(),
                },
                serde_json::json!({
                    "verdict": "not_met",
                    "reason": GOAL_REASON,
                    "evaluation": GOAL_EVALUATION,
                    "applied": true,
                    "cost": GOAL_COST,
                    "model": GOAL_MODEL,
                }),
                Some(GOAL_COST),
            ),
            "evaluation_failed" => (
                AgentEvent::GoalEvaluationFailed {
                    evaluation: GOAL_EVALUATION,
                    message: GOAL_FAILURE.into(),
                    applied: false,
                    usage: TokenUsage::default(),
                    cost: Some(GOAL_COST),
                    billing: Billing::Api,
                    model: GOAL_MODEL.into(),
                },
                serde_json::json!({
                    "evaluation": GOAL_EVALUATION,
                    "message": GOAL_FAILURE,
                    "applied": false,
                    "cost": GOAL_COST,
                    "model": GOAL_MODEL,
                }),
                Some(GOAL_COST),
            ),
            "deferred" => (
                AgentEvent::GoalDeferred {
                    active_background_tasks: GOAL_TASKS,
                },
                serde_json::json!({"active_background_tasks": GOAL_TASKS}),
                None,
            ),
            "finished" => (
                AgentEvent::GoalFinished {
                    result: goal_result(),
                },
                serde_json::json!({
                    "condition": GOAL_CONDITION,
                    "verdict": "met",
                    "reason": GOAL_REASON,
                    "evaluations": GOAL_EVALUATION,
                    "duration_ms": GOAL_DURATION_MS,
                    "cost": GOAL_COST,
                }),
                None,
            ),
            "loop_cap" => (
                AgentEvent::GoalLoopCap {
                    evaluations: GOAL_EVALUATION,
                    continuations: GOAL_CONTINUATIONS,
                    limit: GOAL_LIMIT,
                },
                serde_json::json!({
                    "evaluations": GOAL_EVALUATION,
                    "continuations": GOAL_CONTINUATIONS,
                    "limit": GOAL_LIMIT,
                }),
                None,
            ),
            "turn_limit" => (
                AgentEvent::GoalTurnLimit {
                    evaluations: GOAL_EVALUATION,
                },
                serde_json::json!({"evaluations": GOAL_EVALUATION}),
                None,
            ),
            "cleared_after_error" => (
                AgentEvent::GoalClearedAfterError {
                    condition: GOAL_CONDITION.into(),
                    message: GOAL_FAILURE.into(),
                },
                serde_json::json!({"condition": GOAL_CONDITION, "message": GOAL_FAILURE}),
                None,
            ),
            other => unreachable!("no goal event is named {other}"),
        }
    }

    #[test_case("evaluating"; "evaluating")]
    #[test_case("evaluation"; "evaluation")]
    #[test_case("evaluation_failed"; "evaluation_failed")]
    #[test_case("deferred"; "deferred")]
    #[test_case("finished"; "finished")]
    #[test_case("loop_cap"; "loop_cap")]
    #[test_case("turn_limit"; "turn_limit")]
    #[test_case("cleared_after_error"; "cleared_after_error")]
    fn a_goal_event_is_a_goal_system_message(kind: &str) {
        let (event, fields, spend) = goal_event(kind);
        let (mut pump, out_rx, _) =
            permission_event_pump(permission_manager(), PermissionMode::Default);

        pump.handle(Envelope {
            event,
            subagent: None,
            run_id: 1,
            task: None,
            workflow: None,
        })
        .unwrap();

        let message = next_message(&out_rx);
        assert_eq!(message["type"], "system");
        assert_eq!(message["subtype"], GOAL_SYSTEM_SUBTYPE);
        assert_eq!(message["kind"], kind);
        for (key, value) in fields.as_object().unwrap() {
            assert_eq!(&message[key], value, "{kind}.{key}");
        }
        assert!(out_rx.is_empty());
        assert_eq!(pump.cost, spend);
    }

    fn goal_prompts(shared: &Mutex<Shared>) -> PromptContext<'_> {
        PromptContext {
            shared,
            cwd: Path::new(GOAL_CWD),
            remote: false,
            thinking: &GOAL_THINKING,
            fast: false,
        }
    }

    #[test_case(serde_json::json!({"condition": GOAL_CONDITION, "continuation_limit": GOAL_LIMIT}), true; "kickoff_by_default")]
    #[test_case(serde_json::json!({"condition": GOAL_CONDITION, "continuation_limit": GOAL_LIMIT, "kickoff": false}), false; "without_kickoff")]
    fn goal_set_replaces_the_goal_and_queues_its_kickoff_as_a_typed_prompt(
        request: Value,
        kickoff: bool,
    ) {
        let shared = shared_with_pending(HashMap::new());
        let goal = GoalHandle::default();
        goal.set(OTHER_GOAL_CONDITION).unwrap();
        let (input_tx, input_rx) = flume::unbounded();

        let reply = goal_set(&request, &goal, &goal_prompts(&shared), &input_tx).unwrap();

        assert_eq!(goal.active_condition().as_deref(), Some(GOAL_CONDITION));
        assert_eq!(reply["status"], GOAL_ACTIVE);
        assert_eq!(reply["condition"], GOAL_CONDITION);
        assert_eq!(reply["evaluations"], 0);
        assert_eq!(reply["continuation_limit"], GOAL_LIMIT);
        let queued: Vec<_> = input_rx.try_iter().collect();
        assert_eq!(queued.len(), usize::from(kickoff));
        if let Some(input) = queued.first() {
            let kickoff = goal_kickoff_message(GOAL_CONDITION);
            assert_eq!(input.message, GOAL_CONDITION);
            assert_eq!(input.mode, AgentMode::Build);
            assert_eq!(
                input
                    .preamble
                    .iter()
                    .map(|message| message.first_text_content())
                    .collect::<Vec<_>>(),
                [Some(kickoff.as_str())]
            );
        }
    }

    #[test_case(" ", GoalError::Empty; "empty")]
    #[test_case(&"x".repeat(MAX_GOAL_CHARS + 1), GoalError::TooLong; "too_long")]
    fn goal_set_refuses_what_goal_refuses(condition: &str, error: GoalError) {
        let shared = shared_with_pending(HashMap::new());
        let goal = GoalHandle::default();
        goal.set(OTHER_GOAL_CONDITION).unwrap();
        let (input_tx, input_rx) = flume::unbounded();

        let refusal = goal_set(
            &serde_json::json!({"condition": condition}),
            &goal,
            &goal_prompts(&shared),
            &input_tx,
        )
        .unwrap_err();

        assert_eq!(refusal, error.to_string());
        assert_eq!(
            goal.active_condition().as_deref(),
            Some(OTHER_GOAL_CONDITION)
        );
        assert!(input_rx.is_empty());
    }

    #[test]
    fn goal_clear_answers_the_goal_it_stopped_and_keeps_a_finished_one() {
        let goal = GoalHandle::default();
        assert_eq!(goal_clear(&goal).unwrap()["cleared"], Value::Null);
        goal.set(GOAL_CONDITION).unwrap();
        assert_eq!(goal_clear(&goal).unwrap()["cleared"], GOAL_CONDITION);
        assert!(goal.snapshot().is_none());

        goal.restore_finished(goal_result());
        assert_eq!(goal_clear(&goal).unwrap()["cleared"], Value::Null);
        assert_eq!(goal_status(&goal).unwrap()["status"], GOAL_FINISHED);
    }

    #[test]
    fn goal_status_reports_the_goal_under_the_finished_payload_names() {
        let goal = GoalHandle::default();
        assert_eq!(
            goal_status(&goal).unwrap(),
            serde_json::json!({
                "status": NO_GOAL,
                "continuation_limit": DEFAULT_GOAL_CONTINUATION_LIMIT,
            })
        );

        goal.set_continuation_limit(GOAL_LIMIT);
        goal.restore_finished(goal_result());
        let (_, mut expected, _) = goal_event("finished");
        expected["status"] = GOAL_FINISHED.into();
        expected["continuation_limit"] = GOAL_LIMIT.into();
        expected["subscription_cost"] = Value::Null;
        expected["usage"] = serde_json::to_value(TokenUsage::default()).unwrap();
        assert_eq!(goal_status(&goal).unwrap(), expected);

        goal.set(OTHER_GOAL_CONDITION).unwrap();
        let active = goal_status(&goal).unwrap();
        assert_eq!(active["status"], GOAL_ACTIVE);
        assert_eq!(active["condition"], OTHER_GOAL_CONDITION);
        assert_eq!(active["verdict"], Value::Null);
        assert_eq!(active["evaluations"], 0);
    }

    const AUTOMATION_NAME: &str = "deploy-watch";
    const GUIDE_AUTOMATION: &str = "ci-guide";
    const FIRE_ID: &str = "fire-2";
    const ABSORBED_FIRE_ID: &str = "fire-1";
    const GUIDE_FIRE_ID: &str = "fire-3";
    const SCRIPT_DIGEST: &str = "sha256:5f0c2a";
    const AUTOMATION_SESSION: &str = "session-7";
    const AUTOMATION_BRANCH: &str = "main";
    const EVENT_KEY: &str = "message-7";
    const FIRING_REASON: &str = "the deploy is still running";
    const SCRIPT_ERROR_KIND: &str = "runtime";
    const SCRIPT_ERROR: &str = "index out of bounds";
    const SCRIPT_LINE: u32 = 3;
    const SCRIPT_COLUMN: u32 = 9;
    const TRIGGER_INDEX: u32 = 1;
    const FIRING_REPEATS: u64 = 2;
    const FIRING_ATTEMPTS: u64 = 1;
    const FIRING_OPERATIONS: u64 = 4;
    const FIRING_ACTIONS: u64 = 1;
    const FIRED_AT_MS: i64 = 1_790_000_000_000;
    const STATE_REVISION: u64 = 3;
    const HISTORY_LIMIT: usize = 5;
    const NEGATIVE_LIMIT: i64 = -1;
    const OUTBOX_SEQ: u64 = 2;
    const CLAIM_SEQ: u32 = 0;
    const GUIDE_SEQ: u32 = 1;
    const NOTICE_TEXT: &str = "deploy finished";
    const AUTOMATION_PROMPT: &str = "check CI";
    const CONTROL_ID: &str = "req_automation";
    const INTERRUPT: &str = "interrupt";
    const ARMED_TRIGGER: &str = r#"triggers: [#{ kind: "armed" }]"#;
    const WIRE_IDS: [&str; 2] = ["session_id", "uuid"];
    const PUMP_ROUNDS: usize = 3;
    const UNATTENDED_TURNS: u32 = 2;
    const BACKOFF_ERRORS: u32 = 1;
    const RUNTIME_STARTS: &str = "the fixture's runtime must start";
    const PUMP_WRITING: &str = "the pump must keep writing while the session runs";
    const GOAL_KEPT: &str = "an active goal must refuse a claim that does not replace it";
    const EVENTS_OPEN: &str = "the runtime's events must stay open while the test reads them";
    const START_HELD: &str = "the running firing must wait on its workflow start";
    const QUEUED_BEHIND: &str = "arming again while a firing runs must queue its event";
    const NOT_A_DRY_RUN: &str = "a dry run of a finished firing must answer with its detail";
    const NOT_CONTROLS: &str = "a pause and a resume must answer with the session's controls";
    const LATCHED: &str = "automation_pause must set the latch";
    const NOT_A_TRACE: &str = "a firing request must answer with its trace";

    fn sdk_writer() -> (SdkWriter, Receiver<String>) {
        let (out_tx, out_rx) = flume::unbounded();
        let writer = SdkWriter {
            session_id: SessionRef::generate(),
            out_tx,
        };
        (writer, out_rx)
    }

    /// A message without the ids every message carries.
    fn without_wire_ids(mut message: Value) -> Value {
        for id in WIRE_IDS {
            message.as_object_mut().unwrap().remove(id);
        }
        message
    }

    async fn next_line(out_rx: &Receiver<String>) -> Value {
        serde_json::from_str(&out_rx.recv_async().await.expect(PUMP_WRITING)).unwrap()
    }

    fn automation_args() -> Value {
        serde_json::json!({ "branch": AUTOMATION_BRANCH })
    }

    #[test_case(AUTOMATION_LIST, serde_json::json!({}) => AutomationRequest::List; "list")]
    #[test_case(AUTOMATION_VALIDATE, serde_json::json!({"name": AUTOMATION_NAME}) => AutomationRequest::Validate { name: AUTOMATION_NAME.into() }; "validate")]
    #[test_case(AUTOMATION_ARM, serde_json::json!({"name": AUTOMATION_NAME, "args": automation_args()}) => AutomationRequest::Arm { name: AUTOMATION_NAME.into(), args: Some(automation_args()), origin: ArmOrigin::Sdk }; "arm")]
    #[test_case(AUTOMATION_ARM, serde_json::json!({"name": AUTOMATION_NAME, "args": null}) => AutomationRequest::Arm { name: AUTOMATION_NAME.into(), args: None, origin: ArmOrigin::Sdk }; "arm_with_its_stored_args")]
    #[test_case(AUTOMATION_DISARM, serde_json::json!({"name": AUTOMATION_NAME}) => AutomationRequest::Disarm { name: AUTOMATION_NAME.into() }; "disarm")]
    #[test_case(AUTOMATION_TRUST, serde_json::json!({"name": AUTOMATION_NAME, "digest": SCRIPT_DIGEST}) => AutomationRequest::Trust { name: AUTOMATION_NAME.into(), digest: SCRIPT_DIGEST.into() }; "trust")]
    #[test_case(AUTOMATION_INSPECT, serde_json::json!({"name": AUTOMATION_NAME}) => AutomationRequest::Inspect { name: AUTOMATION_NAME.into(), session_id: None }; "inspect")]
    #[test_case(AUTOMATION_INSPECT, serde_json::json!({"name": AUTOMATION_NAME, "session_id": AUTOMATION_SESSION}) => AutomationRequest::Inspect { name: AUTOMATION_NAME.into(), session_id: Some(AUTOMATION_SESSION.into()) }; "inspect_another_session")]
    #[test_case(AUTOMATION_HISTORY, serde_json::json!({}) => AutomationRequest::History { name: None, fire_id: None, limit: None }; "history")]
    #[test_case(AUTOMATION_HISTORY, serde_json::json!({"name": AUTOMATION_NAME, "fire_id": FIRE_ID, "limit": HISTORY_LIMIT}) => AutomationRequest::History { name: Some(AUTOMATION_NAME.into()), fire_id: Some(FIRE_ID.into()), limit: Some(HISTORY_LIMIT) }; "history_narrowed")]
    #[test_case(AUTOMATION_FIRING, serde_json::json!({"fire_id": FIRE_ID}) => AutomationRequest::Firing { fire_id: FIRE_ID.into() }; "firing")]
    #[test_case(AUTOMATION_DRY_RUN, serde_json::json!({"fire_id": FIRE_ID}) => AutomationRequest::DryRun { fire_id: FIRE_ID.into() }; "dry_run")]
    #[test_case(AUTOMATION_SET_ARGS, serde_json::json!({"name": AUTOMATION_NAME, "args": automation_args()}) => AutomationRequest::SetArgs { name: AUTOMATION_NAME.into(), args: automation_args() }; "set_args")]
    #[test_case(AUTOMATION_SET_STATE, serde_json::json!({"name": AUTOMATION_NAME, "state": automation_args(), "expected_revision": STATE_REVISION}) => AutomationRequest::SetState { name: AUTOMATION_NAME.into(), state: automation_args(), expected_revision: STATE_REVISION }; "set_state")]
    #[test_case(AUTOMATION_CLEAR_STATE, serde_json::json!({"name": AUTOMATION_NAME, "expected_revision": STATE_REVISION}) => AutomationRequest::ClearState { name: AUTOMATION_NAME.into(), expected_revision: STATE_REVISION }; "clear_state")]
    #[test_case(AUTOMATION_DROP, serde_json::json!({"fire_id": FIRE_ID}) => AutomationRequest::Drop(DropTarget::Firing { fire_id: FIRE_ID.into() }); "drop_a_firing")]
    #[test_case(AUTOMATION_DROP, serde_json::json!({"fire_id": FIRE_ID, "seq": OUTBOX_SEQ}) => AutomationRequest::Drop(DropTarget::OutboxItem { fire_id: FIRE_ID.into(), seq: OUTBOX_SEQ }); "drop_an_outbox_item")]
    #[test_case(AUTOMATION_PAUSE, serde_json::json!({}) => AutomationRequest::Pause { by: PauseSource::Sdk }; "pause")]
    #[test_case(AUTOMATION_RESUME, serde_json::json!({}) => AutomationRequest::Resume; "resume")]
    fn automation_controls_map_to_runtime_requests(
        subtype: &str,
        extra: Value,
    ) -> AutomationRequest {
        automation_request(subtype, &extra).unwrap().unwrap()
    }

    #[test_case(AUTOMATION_VALIDATE, serde_json::json!({}), NAME_FIELD, STRING_KIND.name; "validate_without_name")]
    #[test_case(AUTOMATION_ARM, serde_json::json!({"args": automation_args()}), NAME_FIELD, STRING_KIND.name; "arm_without_name")]
    #[test_case(AUTOMATION_ARM, serde_json::json!({"name": AUTOMATION_NAME, "args": AUTOMATION_BRANCH}), ARGS_FIELD, OBJECT_KIND.name; "arm_with_args_that_are_not_an_object")]
    #[test_case(AUTOMATION_DISARM, serde_json::json!({"name": STATE_REVISION}), NAME_FIELD, STRING_KIND.name; "disarm_with_a_numeric_name")]
    #[test_case(AUTOMATION_TRUST, serde_json::json!({"name": AUTOMATION_NAME}), DIGEST_FIELD, STRING_KIND.name; "trust_without_digest")]
    #[test_case(AUTOMATION_INSPECT, serde_json::json!({"name": AUTOMATION_NAME, "session_id": STATE_REVISION}), SESSION_ID_FIELD, STRING_KIND.name; "inspect_with_a_numeric_session_id")]
    #[test_case(AUTOMATION_HISTORY, serde_json::json!({"fire_id": STATE_REVISION}), FIRE_ID_FIELD, STRING_KIND.name; "history_with_a_numeric_fire_id")]
    #[test_case(AUTOMATION_HISTORY, serde_json::json!({"limit": NEGATIVE_LIMIT}), LIMIT_FIELD, INTEGER_KIND.name; "history_with_a_negative_limit")]
    #[test_case(AUTOMATION_FIRING, serde_json::json!({}), FIRE_ID_FIELD, STRING_KIND.name; "firing_without_fire_id")]
    #[test_case(AUTOMATION_DRY_RUN, serde_json::json!({}), FIRE_ID_FIELD, STRING_KIND.name; "dry_run_without_fire_id")]
    #[test_case(AUTOMATION_DRY_RUN, serde_json::json!({"fire_id": STATE_REVISION}), FIRE_ID_FIELD, STRING_KIND.name; "dry_run_with_a_numeric_fire_id")]
    #[test_case(AUTOMATION_SET_ARGS, serde_json::json!({"name": AUTOMATION_NAME}), ARGS_FIELD, OBJECT_KIND.name; "set_args_without_args")]
    #[test_case(AUTOMATION_SET_STATE, serde_json::json!({"name": AUTOMATION_NAME, "expected_revision": STATE_REVISION}), STATE_FIELD, OBJECT_KIND.name; "set_state_without_state")]
    #[test_case(AUTOMATION_SET_STATE, serde_json::json!({"name": AUTOMATION_NAME, "state": automation_args()}), EXPECTED_REVISION_FIELD, INTEGER_KIND.name; "set_state_without_expected_revision")]
    #[test_case(AUTOMATION_CLEAR_STATE, serde_json::json!({"name": AUTOMATION_NAME, "expected_revision": AUTOMATION_BRANCH}), EXPECTED_REVISION_FIELD, INTEGER_KIND.name; "clear_state_with_a_text_revision")]
    #[test_case(AUTOMATION_DROP, serde_json::json!({"seq": OUTBOX_SEQ}), FIRE_ID_FIELD, STRING_KIND.name; "drop_without_fire_id")]
    #[test_case(AUTOMATION_DROP, serde_json::json!({"fire_id": FIRE_ID, "seq": FIRE_ID}), SEQ_FIELD, INTEGER_KIND.name; "drop_with_a_text_seq")]
    fn an_automation_control_with_a_missing_or_mistyped_field_is_refused_by_name(
        subtype: &str,
        extra: Value,
        field: &str,
        kind: &str,
    ) {
        assert_eq!(
            automation_request(subtype, &extra).unwrap().unwrap_err(),
            field_refusal(subtype, field, kind)
        );
    }

    #[test_case(INTERRUPT; "a_session_control")]
    #[test_case(WORKFLOW_LIST; "a_workflow_control")]
    fn a_subtype_outside_the_automation_controls_is_not_one(subtype: &str) {
        assert!(automation_request(subtype, &serde_json::json!({})).is_none());
    }

    #[test]
    fn an_automation_answer_is_a_success_control_response_under_automation() {
        let (writer, out_rx) = sdk_writer();
        let (response, error) = automation_control_response(Ok(AutomationResponse::State {
            revision: STATE_REVISION,
        }));

        writer
            .emit_control_response(CONTROL_ID, response, error)
            .unwrap();

        assert_eq!(
            without_wire_ids(next_message(&out_rx)),
            serde_json::json!({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": CONTROL_ID,
                    "response": {"automation": {"kind": "state", "detail": {"revision": STATE_REVISION}}},
                },
            })
        );
    }

    #[test]
    fn an_automation_error_is_an_error_control_response_with_the_structured_error() {
        let (writer, out_rx) = sdk_writer();
        let error = AutomationError::StateConflict {
            name: AUTOMATION_NAME.into(),
            current: STATE_REVISION,
        };
        let (response, text) = automation_control_response(Err(error.clone()));

        writer
            .emit_control_response(CONTROL_ID, response, text)
            .unwrap();

        assert_eq!(
            without_wire_ids(next_message(&out_rx)),
            serde_json::json!({
                "type": "control_response",
                "response": {
                    "subtype": "error",
                    "request_id": CONTROL_ID,
                    "error": error.to_string(),
                    "response": {"automation_error": {
                        "kind": "state_conflict",
                        "detail": {"name": AUTOMATION_NAME, "current": STATE_REVISION},
                    }},
                },
            })
        );
    }

    /// A session handle the stdin loop answers controls from, with `automations` as its runtime.
    fn control_session(
        state_dir: &StateDir,
        session_id: CaudraId,
        automations: Option<AutomationHandle>,
    ) -> InteractiveHandle {
        let lease = Arc::new(SessionLease::acquire(state_dir, session_id).unwrap());
        let mut session =
            InteractiveHandle::for_test(lease, permission_manager(), flume::unbounded().0);
        session.automations = automations;
        session
    }

    /// Answers `subtype` as the stdin loop answers a `control_request`.
    fn send_control(writer: &SdkWriter, session: &InteractiveHandle, subtype: &str, extra: Value) {
        let shared = shared_with_pending(HashMap::new());
        let model = shared.lock().unwrap().model.clone();
        handle_control_request(
            &InboundControlRequest {
                request_id: CONTROL_ID.into(),
                request: InboundControlRequestInner {
                    subtype: subtype.into(),
                    extra,
                },
            },
            writer,
            session,
            &goal_prompts(&shared),
            &model,
            &ModelPolicy::default(),
        )
        .unwrap();
    }

    #[test_case(AUTOMATION_LIST; "list")]
    #[test_case(AUTOMATION_VALIDATE; "validate")]
    #[test_case(AUTOMATION_ARM; "arm")]
    #[test_case(AUTOMATION_DISARM; "disarm")]
    #[test_case(AUTOMATION_TRUST; "trust")]
    #[test_case(AUTOMATION_INSPECT; "inspect")]
    #[test_case(AUTOMATION_HISTORY; "history")]
    #[test_case(AUTOMATION_FIRING; "firing")]
    #[test_case(AUTOMATION_DRY_RUN; "dry_run")]
    #[test_case(AUTOMATION_SET_ARGS; "set_args")]
    #[test_case(AUTOMATION_SET_STATE; "set_state")]
    #[test_case(AUTOMATION_CLEAR_STATE; "clear_state")]
    #[test_case(AUTOMATION_DROP; "drop")]
    #[test_case(AUTOMATION_PAUSE; "pause")]
    #[test_case(AUTOMATION_RESUME; "resume")]
    fn without_a_runtime_an_automation_control_answers_unavailable(subtype: &str) {
        let state = TempDir::new().unwrap();
        let session = control_session(
            &StateDir::from_path(state.path().to_path_buf()),
            SessionRef::generate().id(),
            None,
        );
        let (writer, out_rx) = sdk_writer();

        send_control(&writer, &session, subtype, serde_json::json!({}));

        let reply = next_message(&out_rx);
        assert_eq!(reply["response"]["subtype"], "error");
        assert_eq!(reply["response"]["error"], UNAVAILABLE);
        assert_eq!(
            reply["response"]["response"],
            serde_json::json!({"automation_error": {"kind": "unavailable"}})
        );
    }

    #[test_case(true => (true, serde_json::json!([
        AUTOMATION_LIST,
        AUTOMATION_VALIDATE,
        AUTOMATION_ARM,
        AUTOMATION_DISARM,
        AUTOMATION_TRUST,
        AUTOMATION_INSPECT,
        AUTOMATION_HISTORY,
        AUTOMATION_FIRING,
        AUTOMATION_DRY_RUN,
        AUTOMATION_SET_ARGS,
        AUTOMATION_SET_STATE,
        AUTOMATION_CLEAR_STATE,
        AUTOMATION_DROP,
        AUTOMATION_PAUSE,
        AUTOMATION_RESUME,
    ])); "with_a_runtime")]
    #[test_case(false => (false, serde_json::json!([])); "without_a_runtime")]
    fn init_advertises_automation_support(automations: bool) -> (bool, Value) {
        let payload = init_payload(
            serde_json::json!({}),
            false,
            automations,
            &AgentConfig::default(),
            false,
        );
        (
            payload["automations"].as_bool().unwrap(),
            payload["automation_controls"].clone(),
        )
    }

    fn firing_event(status: FiringStatus, absorbed: Option<&str>) -> AutomationEvent {
        AutomationEvent::Firing {
            firing: Box::new(FiringSummary {
                fire_id: FIRE_ID.into(),
                automation: AUTOMATION_NAME.into(),
                digest: SCRIPT_DIGEST.into(),
                trigger: TriggerKind::MessageReceived,
                trigger_index: TRIGGER_INDEX,
                event_key: Some(EVENT_KEY.into()),
                consumed: true,
                status,
                reason: Some(FIRING_REASON.into()),
                error: Some(ErrorView {
                    kind: SCRIPT_ERROR_KIND.into(),
                    message: SCRIPT_ERROR.into(),
                    line: Some(SCRIPT_LINE),
                    column: Some(SCRIPT_COLUMN),
                }),
                repeats: FIRING_REPEATS,
                attempts: FIRING_ATTEMPTS,
                operations: FIRING_OPERATIONS,
                state_outcome: Some(StateOutcome::Conflict),
                queued_at: FIRED_AT_MS,
                deferred_until: Some(FIRED_AT_MS),
                started_at: Some(FIRED_AT_MS),
                finished_at: Some(FIRED_AT_MS),
                action_count: FIRING_ACTIONS,
                first_action: Some(ActionKind::Message),
            }),
            absorbed: absorbed.map(str::to_owned),
        }
    }

    #[test_case(FiringStatus::Queued => false; "queued")]
    #[test_case(FiringStatus::Deferred => false; "deferred")]
    #[test_case(FiringStatus::Running => false; "running")]
    #[test_case(FiringStatus::Completed => true; "completed")]
    #[test_case(FiringStatus::Skipped => true; "skipped")]
    #[test_case(FiringStatus::Released => true; "released")]
    #[test_case(FiringStatus::Failed => true; "failed")]
    #[test_case(FiringStatus::RateLimited => true; "rate_limited")]
    #[test_case(FiringStatus::Cancelled => true; "cancelled")]
    #[test_case(FiringStatus::Paused => true; "paused")]
    #[test_case(FiringStatus::Dropped => true; "dropped")]
    #[test_case(FiringStatus::Interrupted => true; "interrupted")]
    fn a_firing_reaches_the_wire_once_it_ended(status: FiringStatus) -> bool {
        let (writer, out_rx) = sdk_writer();

        writer.emit_automation(firing_event(status, None)).unwrap();

        let reported: Vec<Value> = out_rx
            .try_iter()
            .map(|line| serde_json::from_str(&line).unwrap())
            .collect();
        assert!(reported.len() <= 1);
        reported.first().is_some_and(|fired| {
            fired["subtype"] == AUTOMATION_FIRED_SUBTYPE && fired["status"] == status.as_str()
        })
    }

    /// A wire contract: the summary's own names beside the subtype, and `absorbed` only when
    /// the firing took a quiet skip's place.
    #[test_case(Some(ABSORBED_FIRE_ID); "absorbing_a_quiet_skip")]
    #[test_case(None; "on_its_own")]
    fn automation_fired_is_the_firing_summary_under_its_own_names(absorbed: Option<&str>) {
        let (writer, out_rx) = sdk_writer();

        writer
            .emit_automation(firing_event(FiringStatus::Failed, absorbed))
            .unwrap();

        let mut expected = serde_json::json!({
            "type": "system",
            "subtype": AUTOMATION_FIRED_SUBTYPE,
            "fire_id": FIRE_ID,
            "automation": AUTOMATION_NAME,
            "digest": SCRIPT_DIGEST,
            "trigger": "message_received",
            "trigger_index": TRIGGER_INDEX,
            "event_key": EVENT_KEY,
            "consumed": true,
            "status": "failed",
            "reason": FIRING_REASON,
            "error": {
                "kind": SCRIPT_ERROR_KIND,
                "message": SCRIPT_ERROR,
                "line": SCRIPT_LINE,
                "column": SCRIPT_COLUMN,
            },
            "repeats": FIRING_REPEATS,
            "attempts": FIRING_ATTEMPTS,
            "operations": FIRING_OPERATIONS,
            "state_outcome": "conflict",
            "queued_at": FIRED_AT_MS,
            "deferred_until": FIRED_AT_MS,
            "started_at": FIRED_AT_MS,
            "finished_at": FIRED_AT_MS,
            "action_count": FIRING_ACTIONS,
            "first_action": "message",
        });
        if let Some(absorbed) = absorbed {
            expected["absorbed"] = absorbed.into();
        }
        assert_eq!(without_wire_ids(next_message(&out_rx)), expected);
        assert!(out_rx.is_empty());
    }

    /// What the session answers a claimed goal while another goal is active.
    fn claimed_goal_refusal() -> String {
        let goal = GoalHandle::default();
        goal.set(OTHER_GOAL_CONDITION).unwrap();
        let claim = GoalClaim {
            condition: GOAL_CONDITION.into(),
            continuation_limit: None,
            replace: false,
        };
        set_claimed_goal(&goal, &claim).expect(GOAL_KEPT)
    }

    #[test_case(NOTICE_TEXT.to_owned(), Some(FIRE_ID); "a_script_notice")]
    #[test_case(claimed_goal_refusal(), Some(FIRE_ID); "a_claimed_goal_refusal")]
    #[test_case(UNAVAILABLE_IN_SDK.to_owned(), None; "an_arming_refusal")]
    fn a_notice_is_an_automation_notice_message(text: String, fire_id: Option<&str>) {
        let (writer, out_rx) = sdk_writer();

        writer
            .emit_automation(AutomationEvent::Notice {
                automation: AUTOMATION_NAME.into(),
                fire_id: fire_id.map(str::to_owned),
                text: text.clone(),
            })
            .unwrap();

        let mut expected = serde_json::json!({
            "type": "system",
            "subtype": AUTOMATION_NOTICE_SUBTYPE,
            "automation": AUTOMATION_NAME,
            "text": text,
        });
        if let Some(fire_id) = fire_id {
            expected[FIRE_ID_FIELD] = fire_id.into();
        }
        assert_eq!(without_wire_ids(next_message(&out_rx)), expected);
    }

    fn automation_origin(automation: &str, fire_id: &str, seq: u32) -> AutomationEventOrigin {
        AutomationEventOrigin {
            automation: automation.into(),
            fire_id: fire_id.into(),
            seq,
        }
    }

    /// How a run's automation message reads on the wire.
    fn origin_entry(origin: &AutomationEventOrigin) -> Value {
        serde_json::json!({
            "automation": origin.automation,
            "fire_id": origin.fire_id,
            "seq": origin.seq,
        })
    }

    fn injected(
        run_id: u64,
        subagent: Option<SubagentInfo>,
        origin: &AutomationEventOrigin,
    ) -> Envelope {
        Envelope {
            event: AgentEvent::Injected {
                text: AUTOMATION_PROMPT.into(),
                task_event: None,
                peer_event: None,
                automation_event: Some(origin.clone()),
            },
            subagent,
            run_id,
            task: None,
            workflow: None,
        }
    }

    #[test]
    fn a_run_reports_its_claims_then_the_guide_items_injected_into_it() {
        const RUN: u64 = 7;
        let (mut pump, out_rx, _) =
            permission_event_pump(permission_manager(), PermissionMode::Default);
        let (run_tx, run_rx) = flume::unbounded();
        pump.run_rx = run_rx;
        let claim = automation_origin(AUTOMATION_NAME, FIRE_ID, CLAIM_SEQ);
        let first_guide = automation_origin(GUIDE_AUTOMATION, GUIDE_FIRE_ID, CLAIM_SEQ);
        let second_guide = automation_origin(GUIDE_AUTOMATION, GUIDE_FIRE_ID, GUIDE_SEQ);
        let elsewhere = automation_origin(GUIDE_AUTOMATION, ABSORBED_FIRE_ID, CLAIM_SEQ);
        run_tx
            .send(InteractiveRun {
                run_id: RUN,
                started: Instant::now(),
                automatic: true,
                task_event_ids: Vec::new(),
                workflow_events: Vec::new(),
                automation_events: vec![claim.clone()],
            })
            .unwrap();
        let subagent = SubagentInfo {
            parent_tool_use_id: RETRY_PARENT.into(),
            task_id: RETRY_PARENT.into(),
            name: RETRY_PARENT.into(),
            prompt: None,
            model: None,
            thinking: None,
            fast: false,
            answer_tx: None,
            steer_tx: None,
        };

        for envelope in [
            injected(RUN, None, &claim),
            injected(RUN, None, &first_guide),
            injected(RUN, Some(subagent), &elsewhere),
            injected(RUN - 1, None, &elsewhere),
            injected(RUN, None, &second_guide),
            Envelope {
                event: AgentEvent::Done {
                    usage: TokenUsage::default(),
                    num_turns: 1,
                    reason: DoneReason::EndTurn,
                },
                subagent: None,
                run_id: RUN,
                task: None,
                workflow: None,
            },
        ] {
            pump.handle(envelope).unwrap();
        }

        let start = next_message(&out_rx);
        assert_eq!(start["subtype"], "turn_start");
        assert_eq!(
            start["automation_events"],
            serde_json::json!([origin_entry(&claim)])
        );
        let result = next_message(&out_rx);
        assert_eq!(
            result["run"]["automation_events"],
            serde_json::json!([
                origin_entry(&claim),
                origin_entry(&first_guide),
                origin_entry(&second_guide),
            ])
        );
        assert!(out_rx.is_empty());
    }

    fn goal_envelope() -> Envelope {
        Envelope {
            event: goal_event("evaluating").0,
            subagent: None,
            run_id: 1,
            task: None,
            workflow: None,
        }
    }

    fn notice(text: &str) -> AutomationEvent {
        AutomationEvent::Notice {
            automation: AUTOMATION_NAME.into(),
            fire_id: Some(FIRE_ID.into()),
            text: text.into(),
        }
    }

    /// One task writes both streams, so each message goes out as it arrives. The automation
    /// events closing, as they do without a runtime, never stops the agent events; the agent
    /// events ending stops the pump while the automation events stay open.
    #[test_case(true; "with_automations")]
    #[test_case(false; "without_automations")]
    fn the_pump_writes_both_streams_until_the_agent_events_end(automations: bool) {
        smol::block_on(async {
            let (pump, out_rx, _) =
                permission_event_pump(permission_manager(), PermissionMode::Default);
            let (event_tx, event_rx) = flume::unbounded();
            let (automation_tx, automation_rx) = flume::unbounded();
            let automation_tx = automations.then_some(automation_tx);
            let pump = pump.spawn(event_rx, automation_rx);

            let mut subtypes = Vec::new();
            for _ in 0..PUMP_ROUNDS {
                event_tx.send(goal_envelope()).unwrap();
                subtypes.push(next_line(&out_rx).await["subtype"].clone());
                if let Some(automation_tx) = &automation_tx {
                    automation_tx.send(notice(NOTICE_TEXT)).unwrap();
                    subtypes.push(next_line(&out_rx).await["subtype"].clone());
                }
            }
            drop(event_tx);
            pump.await;

            let round: &[&str] = if automations {
                &[GOAL_SYSTEM_SUBTYPE, AUTOMATION_NOTICE_SUBTYPE]
            } else {
                &[GOAL_SYSTEM_SUBTYPE]
            };
            assert_eq!(subtypes, round.repeat(PUMP_ROUNDS));
            drop(automation_tx);
        });
    }

    #[test]
    fn the_pump_writes_what_automations_sent_before_the_session_ended() {
        smol::block_on(async {
            let (pump, out_rx, _) =
                permission_event_pump(permission_manager(), PermissionMode::Default);
            let (event_tx, event_rx) = flume::unbounded::<Envelope>();
            let (automation_tx, automation_rx) = flume::unbounded();
            for text in [NOTICE_TEXT, AUTOMATION_PROMPT] {
                automation_tx.send(notice(text)).unwrap();
            }
            drop(event_tx);

            pump.spawn(event_rx, automation_rx).await;

            let texts: Vec<Value> = out_rx
                .try_iter()
                .map(|line| serde_json::from_str::<Value>(&line).unwrap()["text"].clone())
                .collect();
            assert_eq!(texts, [NOTICE_TEXT, AUTOMATION_PROMPT]);
            drop(automation_tx);
        });
    }

    /// The fixture's deps as an SDK session hands them to its runtime.
    fn sdk_deps(fixture: &AutomationFixture) -> RuntimeDeps {
        RuntimeDeps {
            frontend: Frontend::Sdk,
            ..fixture.deps(fixture.session_id(), &[])
        }
    }

    /// A runtime serving the fixture's session as an SDK session serves it.
    async fn sdk_runtime(fixture: &AutomationFixture) -> AutomationRuntime {
        AutomationRuntime::spawn(sdk_deps(fixture))
            .await
            .expect(RUNTIME_STARTS)
    }

    /// The writer is a rendezvous: an answer written on the calling thread would wait there for
    /// a reader that only comes once the call returned.
    #[test]
    fn an_automation_control_is_answered_off_the_stdin_thread() {
        let fixture = AutomationFixture::default();
        let runtime = smol::block_on(sdk_runtime(&fixture));
        let (out_tx, out_rx) = flume::bounded(0);
        let writer = SdkWriter {
            session_id: SessionRef::generate(),
            out_tx,
        };

        answer_automation_control(
            &writer,
            CONTROL_ID,
            Ok(AutomationRequest::List),
            Some(&runtime.handle()),
        )
        .unwrap();

        let reply: Value = serde_json::from_str(&out_rx.recv().unwrap()).unwrap();
        assert_eq!(reply["response"]["subtype"], "success");
        assert_eq!(
            reply["response"]["response"]["automation"]["kind"],
            "automations"
        );
        smol::block_on(runtime.shutdown());
    }

    /// The SDK's part of the plan's end-to-end run, on a real runtime without the headless loop:
    /// a control that names no script is refused by name, `automation_arm` arms as the SDK's,
    /// the firing it starts reaches the wire once, as it ended, beside its `notify()`, and the
    /// `interrupt` control pauses the session's automations.
    #[test]
    fn an_sdk_arming_fires_through_the_runtime_onto_the_wire() {
        let fixture = AutomationFixture::default();
        fixture.script(
            &fixture.user_scripts(),
            AUTOMATION_NAME,
            ARMED_TRIGGER,
            &format!(r#"message("{AUTOMATION_PROMPT}"); notify("{NOTICE_TEXT}");"#),
        );
        let runtime = smol::block_on(sdk_runtime(&fixture));
        let automations = runtime.handle();
        let session = control_session(
            fixture.state_dir(),
            fixture.session_id(),
            Some(automations.clone()),
        );
        let (pump, out_rx, _) =
            permission_event_pump(permission_manager(), PermissionMode::Default);
        let writer = pump.writer.clone();
        let (event_tx, event_rx) = flume::unbounded();
        let pump = pump.spawn(event_rx, automations.events());

        send_control(&writer, &session, AUTOMATION_ARM, serde_json::json!({}));
        let refusal = next_message(&out_rx);
        send_control(
            &writer,
            &session,
            AUTOMATION_ARM,
            serde_json::json!({"name": AUTOMATION_NAME}),
        );
        let completed = |message: &Value| {
            message["subtype"] == AUTOMATION_FIRED_SUBTYPE
                && message["status"] == FiringStatus::Completed.as_str()
        };
        let noticed = |message: &Value| message["subtype"] == AUTOMATION_NOTICE_SUBTYPE;
        let (mut armed, mut wire) = (None, Vec::new());
        smol::block_on(async {
            while armed.is_none() || !wire.iter().any(completed) || !wire.iter().any(noticed) {
                let message = next_line(&out_rx).await;
                if message["type"] == "control_response" {
                    armed = Some(message);
                } else {
                    wire.push(message);
                }
            }
        });
        send_control(&writer, &session, INTERRUPT, serde_json::json!({}));
        smol::block_on(automations.request(AutomationRequest::List)).unwrap();
        let pause = automations.state().session.controls.pause.clone();
        drop(event_tx);
        smol::block_on(pump);
        smol::block_on(runtime.shutdown());
        let (interrupted, rest): (Vec<Value>, Vec<Value>) = out_rx
            .try_iter()
            .map(|line| serde_json::from_str::<Value>(&line).unwrap())
            .partition(|message| message["type"] == "control_response");
        wire.extend(rest);

        assert_eq!(refusal["response"]["subtype"], "error");
        assert_eq!(
            refusal["response"]["error"],
            field_refusal(AUTOMATION_ARM, NAME_FIELD, STRING_KIND.name)
        );
        let armed = &armed.unwrap()["response"];
        assert_eq!(armed["subtype"], "success");
        assert_eq!(armed["response"]["automation"]["kind"], "automation");
        assert_eq!(
            armed["response"]["automation"]["detail"]["armed"],
            ArmOrigin::Sdk.as_str()
        );
        let mut subtypes: Vec<&Value> = wire.iter().map(|message| &message["subtype"]).collect();
        subtypes.sort_by_key(|subtype| subtype.as_str());
        assert_eq!(
            subtypes,
            [AUTOMATION_FIRED_SUBTYPE, AUTOMATION_NOTICE_SUBTYPE]
        );
        let fired = wire.iter().find(|message| completed(message)).unwrap();
        assert_eq!(fired["automation"], AUTOMATION_NAME);
        assert_eq!(fired["trigger"], "armed");
        let notice = wire.iter().find(|message| noticed(message)).unwrap();
        assert_eq!(notice["automation"], AUTOMATION_NAME);
        assert_eq!(notice[FIRE_ID_FIELD], fired[FIRE_ID_FIELD]);
        assert_eq!(notice["text"], NOTICE_TEXT);
        assert_eq!(interrupted.len(), 1);
        assert_eq!(interrupted[0]["response"]["subtype"], "success");
        assert_eq!(pause.map(|latch| latch.source), Some(PauseSource::Sdk));
    }

    /// A runtime serving the fixture's session, a stdin loop that answers controls from it, and
    /// the wire their replies go out on.
    struct WiredRuntime {
        runtime: AutomationRuntime,
        session: InteractiveHandle,
        writer: SdkWriter,
        out_rx: Receiver<String>,
    }

    impl WiredRuntime {
        fn spawn(fixture: &AutomationFixture, deps: RuntimeDeps) -> Self {
            let runtime = smol::block_on(AutomationRuntime::spawn(deps)).expect(RUNTIME_STARTS);
            let session = control_session(
                fixture.state_dir(),
                fixture.session_id(),
                Some(runtime.handle()),
            );
            let (writer, out_rx) = sdk_writer();
            Self {
                runtime,
                session,
                writer,
                out_rx,
            }
        }

        /// Sends `subtype` as a client does, and waits for its reply.
        fn control(&self, subtype: &str, extra: Value) -> Value {
            send_control(&self.writer, &self.session, subtype, extra);
            smol::block_on(next_line(&self.out_rx))
        }
    }

    /// The next firing the runtime reports in `status`.
    async fn reported(events: &Receiver<AutomationEvent>, status: FiringStatus) -> FiringSummary {
        loop {
            if let AutomationEvent::Firing { firing, .. } =
                events.recv_async().await.expect(EVENTS_OPEN)
                && firing.status == status
            {
                return *firing;
            }
        }
    }

    /// The runtime's answer a control reply carries, or the error it refused the control with.
    fn answered(reply: &Value) -> Result<AutomationResponse, AutomationError> {
        let body = &reply["response"]["response"];
        match body.get(AUTOMATION_REPLY) {
            Some(answer) => Ok(serde_json::from_value(answer.clone()).unwrap()),
            None => Err(serde_json::from_value(body[AUTOMATION_ERROR_REPLY].clone()).unwrap()),
        }
    }

    /// A finished firing runs again under the placeholder id, and the action it took is
    /// recorded rather than performed.
    #[test]
    fn automation_dry_run_answers_a_finished_firing_with_its_trace() {
        let fixture = AutomationFixture::default();
        fixture.script(
            &fixture.user_scripts(),
            AUTOMATION_NAME,
            ARMED_TRIGGER,
            &format!(r#"notify("{NOTICE_TEXT}");"#),
        );
        let wired = WiredRuntime::spawn(&fixture, sdk_deps(&fixture));
        let events = wired.runtime.handle().events();

        wired.control(AUTOMATION_ARM, serde_json::json!({"name": AUTOMATION_NAME}));
        let fired = smol::block_on(reported(&events, FiringStatus::Completed));
        let reply = wired.control(
            AUTOMATION_DRY_RUN,
            serde_json::json!({"fire_id": fired.fire_id}),
        );
        smol::block_on(wired.runtime.shutdown());

        let Ok(AutomationResponse::DryRun(detail)) = answered(&reply) else {
            panic!("{NOT_A_DRY_RUN}: {reply}");
        };
        let kinds: Vec<ActionKind> = detail.trace.actions.iter().map(|row| row.kind).collect();
        assert_eq!(detail.fire_id, fired.fire_id);
        assert_eq!(detail.trace.firing.fire_id, DRY_RUN_ID);
        assert_eq!(detail.trace.firing.automation, AUTOMATION_NAME);
        assert_eq!(kinds, [ActionKind::Notify]);
        assert_eq!(detail.answers, [Answer::Recorded]);
    }

    /// An event that waits behind its automation's running firing has not finished, so it
    /// cannot run again yet.
    #[test]
    fn automation_dry_run_refuses_a_queued_firing_as_not_replayable() {
        let fixture = AutomationFixture::default();
        fixture.script(
            &fixture.user_scripts(),
            AUTOMATION_NAME,
            &format!(r#"{ARMED_TRIGGER}, workflows: ["{WORKFLOW_NAME}"]"#),
            &format!(r#"start_workflow("{WORKFLOW_NAME}", #{{}});"#),
        );
        let (workflows, starts, _settles) = FakeWorkflows::new();
        let wired = WiredRuntime::spawn(
            &fixture,
            RuntimeDeps {
                workflows: Some(workflows as Arc<dyn Workflows>),
                ..sdk_deps(&fixture)
            },
        );
        let arm = serde_json::json!({"name": AUTOMATION_NAME});

        wired.control(AUTOMATION_ARM, arm.clone());
        let held = starts.recv().expect(START_HELD);
        wired.control(AUTOMATION_ARM, arm);
        let queued = wired
            .runtime
            .handle()
            .state()
            .recent
            .iter()
            .find(|firing| firing.status == FiringStatus::Queued)
            .expect(QUEUED_BEHIND)
            .fire_id
            .clone();
        let reply = wired.control(AUTOMATION_DRY_RUN, serde_json::json!({"fire_id": queued}));
        drop(starts);
        drop(held);
        smol::block_on(wired.runtime.shutdown());

        let refusal = AutomationError::NotReplayable {
            fire_id: queued,
            reason: REPLAY_NOT_FINISHED.into(),
        };
        assert_eq!(reply["response"]["error"], refusal.to_string());
        assert_eq!(answered(&reply), Err(refusal));
    }

    /// `automation_pause` sets the latch as the SDK's. `automation_resume` lifts it without a
    /// prompt, so `armed` fires with reason `unpaused` while the unattended turns and the
    /// delivery backoff that human input resets stay as they were. The replies are checked
    /// before the wait for that firing, so a control that went wrong fails instead of waiting.
    #[test]
    fn automation_pause_and_resume_set_and_lift_the_latch_without_human_input() {
        let fixture = AutomationFixture::default();
        fixture.script(
            &fixture.user_scripts(),
            AUTOMATION_NAME,
            ARMED_TRIGGER,
            &format!(r#"notify("{NOTICE_TEXT}");"#),
        );
        let counted = StoredAutomationControls {
            unattended: StoredUnattendedTurns {
                count: UNATTENDED_TURNS,
            },
            delivery_backoff: StoredDeliveryBackoff {
                errors: BACKOFF_ERRORS,
                until: None,
            },
            ..StoredAutomationControls::default()
        };
        let wired = WiredRuntime::spawn(
            &fixture,
            RuntimeDeps {
                controls: Some(counted),
                ..sdk_deps(&fixture)
            },
        );
        let automations = wired.runtime.handle();
        let events = automations.events();

        wired.control(AUTOMATION_ARM, serde_json::json!({"name": AUTOMATION_NAME}));
        smol::block_on(reported(&events, FiringStatus::Completed));
        let paused = answered(&wired.control(AUTOMATION_PAUSE, serde_json::json!({})));
        let resumed = answered(&wired.control(AUTOMATION_RESUME, serde_json::json!({})));
        let (Ok(AutomationResponse::Controls(paused)), Ok(AutomationResponse::Controls(resumed))) =
            (paused, resumed)
        else {
            panic!("{NOT_CONTROLS}");
        };
        let latch = paused.controls.pause.expect(LATCHED);
        assert_eq!(
            (latch.source, latch.reason.as_str()),
            (PauseSource::Sdk, PAUSED_BY_SDK)
        );
        assert_eq!(resumed.controls.pause, None);
        assert_eq!(
            (
                resumed.controls.unattended.count,
                resumed.controls.delivery_backoff.errors
            ),
            (UNATTENDED_TURNS, BACKOFF_ERRORS)
        );
        let unpaused = smol::block_on(reported(&events, FiringStatus::Completed));
        let trace = smol::block_on(automations.request(AutomationRequest::Firing {
            fire_id: unpaused.fire_id,
        }));
        smol::block_on(wired.runtime.shutdown());

        let Ok(AutomationResponse::Firing(trace)) = trace else {
            panic!("{NOT_A_TRACE}: {trace:?}");
        };
        let event: Event = serde_json::from_value(trace.event).unwrap();
        assert_eq!(
            event.detail,
            EventDetail::Armed {
                reason: ArmedReason::Unpaused
            }
        );
    }
}
