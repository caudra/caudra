//! SDK streaming mode: `caudra --print --input-format stream-json`.
//!
//! Wire protocol matches Claude Code's SDK interface so tools like Conductor, Windsurf, and custom
//! orchestrators work without adaptation.
//!
//! Per-message wire ids (`uuid`, assistant `message.id`) use `uuid::Uuid::now_v7()` to emit the
//! hyphenated-hex UUIDv7 shape that Claude Code SDK consumers expect, rather than caudra's base58
//! `CaudraId` canonical form.

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, Write};
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use caudra_agent::headless::{self, InteractiveHandle, InteractiveParams};
use caudra_agent::mcp;
use caudra_agent::permissions::{
    PermissionAnswer, PermissionLifetime, PermissionManager, PluginRuleStore,
};
use caudra_agent::prompt::ResolvedSlots;
use caudra_agent::prompt::profile::{BUILTIN_PROFILE_NAME, PromptProfileCatalog};
use caudra_agent::tools::QUESTION_TOOL_NAME;
use caudra_agent::types::WorkflowProvenance;
use caudra_agent::workflow::WorkflowHandle;
use caudra_agent::{
    AgentConfig, AgentEvent, AgentInput, AgentMode, DoneReason, Envelope, History,
    PermissionsConfig, StoredSession,
};
use caudra_config::ModelPolicy;
use caudra_providers::model::Model;
use caudra_providers::{
    Billing, HistoryItem, HistoryItemKind, ImageSource, StopReason, ThinkingConfig, Timeouts,
    TokenUsage, add_cost,
};
use caudra_storage::id::SessionRef;
use caudra_storage::local_documents::LocalDocumentStore;
use caudra_storage::permission_state::PermissionRuleRecord;
use caudra_storage::sessions::{SessionError, SessionLease, StoredMode, StoredPlanTarget};
use caudra_storage::tool_outputs::{ToolOutputRef, ToolOutputStore};
use caudra_storage::workspace_binding::StoredWorkspaceBinding;
use caudra_storage::{StateDir, StorageError};
use caudra_workflow::{
    LaunchRequest, WorkflowError, WorkflowEvent, WorkflowRequest, WorkflowResponse,
};
use caudra_workspace::PlanRef;
use caudra_workspace::WorkspaceSession;
use color_eyre::Result;
use color_eyre::eyre::{Context, eyre};
use flume::{Receiver, Sender};
use serde::Serialize;
use serde_json::Value;
use tracing::warn;

use crate::cli::Cli;

const WORKFLOW_SYSTEM_SUBTYPE: &str = "workflow";
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
    AcceptEdits,
    Plan,
    BypassPermissions,
}

impl PermissionMode {
    fn resolve(flag: Option<&str>, yolo: bool) -> Self {
        match flag {
            Some(s) => Self::parse(s).unwrap_or_else(|| {
                eprintln!("warning: unknown permission mode '{s}', using default");
                Self::Default
            }),
            None if yolo => Self::BypassPermissions,
            None => Self::Default,
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "default" => Some(Self::Default),
            "acceptEdits" => Some(Self::AcceptEdits),
            "plan" => Some(Self::Plan),
            "bypassPermissions" => Some(Self::BypassPermissions),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::Plan => "plan",
            Self::BypassPermissions => "bypassPermissions",
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
    message: UserMessage,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_tool_use_id: Option<String>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    workflow: Option<WorkflowProvenance>,
}

/// The `system` / `workflow` body: the run's event plus the envelope's
/// `workflow_*` provenance keys, so a client can key it by run.
#[derive(Serialize)]
struct WorkflowSystemPayload<'a> {
    event: &'a WorkflowEvent,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    workflow: Option<&'a WorkflowProvenance>,
}

#[derive(Serialize)]
struct UserMessage {
    role: &'static str,
    content: Value,
}

#[derive(Serialize)]
struct ResultPayload {
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

    fn emit_direct_command_result(
        &self,
        output: caudra_agent::headless::RemoteCommandOutput,
        duration_ms: u128,
    ) -> Result<()> {
        self.emit(WireInner::Result(ResultPayload {
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
}

struct Shared {
    model: Model,
    permission_mode: PermissionMode,
    turn_start: Instant,
    pending: HashMap<String, String>,
    resolved_permission_requests: HashSet<String>,
    workspace_session: Option<WorkspaceSession>,
    local_documents: Option<Arc<LocalDocumentStore>>,
    session_id: SessionRef,
    remote_plan: Option<PlanRef>,
}

impl Shared {
    fn agent_mode(&mut self, cwd: &Path) -> AgentMode {
        if self.permission_mode != PermissionMode::Plan || self.workspace_session.is_none() {
            return self.permission_mode.agent_mode(cwd);
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

pub fn run(params: SdkParams) -> Result<()> {
    let SdkParams {
        cli,
        model,
        mut config,
        permissions_config,
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
    } = params;
    cli.warn_ignored_flags();
    if let Some(max) = cli.max_turns {
        config.max_turns = Some(max);
    }
    let max_output_lines = config.max_output_lines;
    let max_output_bytes = config.max_output_bytes;
    let mut requested_permission_mode =
        PermissionMode::resolve(cli.permission_mode.as_deref(), cli.yolo);
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
        session_yolo,
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
    if restored_plan_mode && cli.permission_mode.is_none() && !cli.yolo {
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
        turn_start: Instant::now(),
        pending: HashMap::new(),
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
    let handle = smol::block_on(headless::spawn_interactive(InteractiveParams {
        model,
        config,
        permissions_config,
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
        yolo: requested_permission_mode == PermissionMode::BypassPermissions,
        structured_permission_rules,
        session_yolo,
        system_prompt_override,
        append_system_prompt: cli.append_system_prompt.clone().filter(|s| !s.is_empty()),
        model_policy: Arc::clone(&model_policy),
        plugin_rules,
        local_tools: Default::default(),
        workflow_mode: Some(workflow_mode),
        workspace_binding: workspace_binding.clone(),
        remote_environment: remote_environment.clone(),
        workspace_session,
        remote_project_context,
        local_documents,
    }))
    .map_err(|error| eyre!(error))?;
    if let Some(workspace) = handle.remote_workspace_session() {
        shared.lock().unwrap().workspace_session = Some(workspace);
    }
    let permission_mode =
        effective_permission_mode(requested_permission_mode, handle.permissions.is_yolo());
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
        ),
    )?;

    let pump = EventPump {
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
    .spawn(handle.event_rx.clone());

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
                let Some(user) = parse_or_warn::<InboundUser>(msg.payload, "user message") else {
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
                        Ok(status) => {
                            writer.emit_system("remote", serde_json::json!({ "status": status }))?
                        }
                        Err(error) => writer
                            .emit_system("remote_error", serde_json::json!({ "error": error }))?,
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
                    let output = match (
                        handle.remote_workspace_session(),
                        handle.remote_workspace_baseline(),
                    ) {
                        (Some(workspace), Some(baseline)) => {
                            smol::block_on(headless::execute_remote_command(
                                &workspace,
                                &baseline,
                                command,
                                &caudra_agent::CancelToken::none(),
                                max_output_lines,
                                max_output_bytes,
                                |_| {},
                            ))
                        }
                        _ => headless::RemoteCommandOutput {
                            output: "Remote command execution is unavailable".into(),
                            is_error: true,
                        },
                    };
                    writer.emit_direct_command_result(output, started.elapsed().as_millis())?;
                    continue;
                }
                let mode = {
                    let mut shared = shared.lock().unwrap();
                    shared.turn_start = Instant::now();
                    shared.agent_mode(&cwd)
                };
                let mentions = if remote_environment.is_some() {
                    caudra_agent::mentions::scan_remote(&prompt)
                        .into_iter()
                        .map(|(_, mention)| mention)
                        .collect()
                } else {
                    caudra_agent::mentions::scan(&prompt, |path| cwd.join(path).exists())
                        .into_iter()
                        .map(|(_, mention)| mention)
                        .collect()
                };
                let input = AgentInput {
                    message: prompt,
                    mode,
                    images,
                    mentions,
                    preamble: Vec::new(),
                    thinking: thinking.clone(),
                    fast,
                    prompt: None,
                    resume: false,
                };
                if handle.input_tx.send(input).is_err() {
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
                    &shared,
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

    let InteractiveHandle { input_tx, task, .. } = handle;
    drop(input_tx);
    smol::block_on(async {
        task.await;
        pump.await;
    });
    drop(writer);
    let _ = writer_thread.join();
    Ok(())
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
    session_yolo: Option<bool>,
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
) -> (Vec<PermissionRuleRecord>, Option<bool>) {
    if fork {
        (Vec::new(), None)
    } else {
        (
            session.meta.structured_permission_rules.clone(),
            session.meta.yolo,
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
            let (structured_permission_rules, session_yolo) = session_permissions(&session, true);
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
                caudra_agent::active_task_history_versions_with_batch_state(&history, |call_id| {
                    session
                        .tool_outputs()
                        .get(call_id)
                        .and_then(|output| output.state())
                });
            reachable.extend(
                versions
                    .iter()
                    .filter(|(_, version_id)| session.subagent_messages().contains_key(*version_id))
                    .map(|(task_id, _)| task_id.clone()),
            );
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
                        .or_else(|| session.subagent_messages().get(task_id))
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
                session_yolo,
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
        let (structured_permission_rules, session_yolo) = session_permissions(&session, false);
        return Ok(ResolvedSession {
            session_id: session_ref,
            session_lease: source_lease.expect("non-fork resume has a lease"),
            expected_write_version: session.persisted_write_version(),
            initial_history: history,
            structured_permission_rules,
            session_yolo,
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
        let (structured_permission_rules, session_yolo) = session_permissions(&session, false);
        return Ok(ResolvedSession {
            session_id: session_ref,
            session_lease,
            expected_write_version: session.persisted_write_version(),
            initial_history: history,
            structured_permission_rules,
            session_yolo,
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
        session_yolo: None,
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

fn effective_permission_mode(requested: PermissionMode, yolo: bool) -> PermissionMode {
    match (requested, yolo) {
        (PermissionMode::Default, true) => PermissionMode::BypassPermissions,
        (PermissionMode::BypassPermissions, false) => PermissionMode::Default,
        _ => requested,
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
            } => retained_output_refs,
            _ => &[],
        };
        for output_ref in output_refs {
            if seen.insert(output_ref.id) {
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
    for (task_id, history) in subagent_histories {
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
    shared: &Mutex<Shared>,
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
        "interrupt" => {
            let _ = handle.cancel_tx.try_send(());
            writer.emit_control_response(&cr.request_id, ok, None)
        }
        "set_permission_mode" => {
            let mode_str = cr.request.extra.get("mode").and_then(Value::as_str);
            match mode_str.and_then(PermissionMode::parse) {
                Some(mode) => {
                    shared.lock().unwrap().permission_mode = mode;
                    handle
                        .permissions
                        .set_session_yolo(Some(mode == PermissionMode::BypassPermissions));
                    writer.emit_control_response(&cr.request_id, ok, None)
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
                        shared.lock().unwrap().model = model;
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
        other => match workflow_request(other, &cr.request.extra) {
            Some(Ok(request)) => {
                forward_workflow_request(writer, handle.workflow.as_ref(), &cr.request_id, request);
                Ok(())
            }
            Some(Err(message)) => writer.emit_control_response(&cr.request_id, None, Some(message)),
            None => writer.emit_control_response(
                &cr.request_id,
                None,
                Some(format!("unsupported: {other}")),
            ),
        },
    }
}

/// The init message with what the session can do beyond the Claude Code
/// shape: `workflows` says whether a runtime is attached, and
/// `workflow_controls` names the `control_request` subtypes it answers.
fn init_payload(mut payload: Value, workflows: bool) -> Value {
    let controls: &[&str] = if workflows { WORKFLOW_CONTROLS } else { &[] };
    payload["workflows"] = Value::Bool(workflows);
    payload["workflow_controls"] = serde_json::json!(controls);
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

/// Answers off the stdin thread: a pause or stop waits for the run's agents
/// to stop, and the client's permission replies must keep flowing meanwhile.
fn forward_workflow_request(
    writer: &SdkWriter,
    workflow: Option<&WorkflowHandle>,
    request_id: &str,
    request: WorkflowRequest,
) {
    let writer = writer.clone();
    let workflow = workflow.cloned();
    let request_id = request_id.to_owned();
    smol::spawn(async move {
        let answer = match workflow {
            Some(workflow) => workflow.request(request).await,
            None => Err(WorkflowError::Unavailable),
        };
        let (response, error) = workflow_control_response(answer);
        if let Err(error) = writer.emit_control_response(&request_id, response, error) {
            warn!(%error, request_id, "workflow control response not delivered");
        }
    })
    .detach();
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
    answer: PermissionAnswer,
) -> bool {
    let request_id = {
        let mut shared = shared.lock().unwrap();
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
    fn spawn(mut self, event_rx: Receiver<Envelope>) -> smol::Task<()> {
        smol::spawn(async move {
            while let Ok(envelope) = event_rx.recv_async().await {
                if let Err(e) = self.handle(envelope) {
                    warn!(error = %e, "sdk event pump stopped");
                    break;
                }
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

    fn reset_turn(&mut self) {
        self.synth.reset();
        self.result_text.clear();
        self.cost = None;
        self.subscription_cost = None;
        self.auxiliary_usage = TokenUsage::default();
        let pending = mem::take(&mut self.shared.lock().unwrap().pending);
        for request_id in pending.into_values() {
            self.permissions.answer(&request_id, PermissionAnswer::Deny);
        }
    }

    fn emit_turn_result(
        &mut self,
        is_error: bool,
        result: String,
        num_turns: u32,
        usage: TokenUsage,
    ) -> Result<()> {
        let duration_ms = self.shared.lock().unwrap().turn_start.elapsed().as_millis();
        // Zero on an unpriced model, which is what its turns reported too.
        let total_cost_usd = self.cost.unwrap_or_default();
        let subscription_cost_usd = self.subscription_cost.unwrap_or_default();
        self.writer.emit(WireInner::Result(ResultPayload {
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
        let parent_tool_use_id = envelope
            .subagent
            .as_ref()
            .map(|s| s.parent_tool_use_id.clone());

        match &envelope.event {
            AgentEvent::TextDelta { text } => {
                if self.include_partial_messages {
                    let model = self.model_id();
                    let events = self.synth.text_delta(&model, text);
                    self.emit_stream(events)?;
                }
            }
            AgentEvent::ThinkingDelta { text } => {
                if self.include_partial_messages {
                    let model = self.model_id();
                    let events = self.synth.thinking_delta(&model, text);
                    self.emit_stream(events)?;
                }
            }
            AgentEvent::ThinkingBoundary => {
                if self.include_partial_messages {
                    let events = self.synth.thinking_boundary();
                    self.emit_stream(events)?;
                }
            }
            AgentEvent::ToolPending { id, name } => {
                if self.include_partial_messages {
                    let model = self.model_id();
                    let events =
                        self.synth
                            .tool_pending(&model, id, caudra_to_claude_tool_name(name));
                    self.emit_stream(events)?;
                }
            }
            AgentEvent::ToolInputDelta { id, delta, .. } => {
                if self.include_partial_messages {
                    let events = self.synth.tool_input_delta(id, delta);
                    self.emit_stream(events)?;
                }
            }
            AgentEvent::ToolStart(ts) => {
                let name = ts.tool.to_string();
                let input = ts.raw_input.clone().unwrap_or(Value::Null);

                if self.include_partial_messages {
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
            | AgentEvent::SessionTitle { .. }
            | AgentEvent::AuthRequired
            | AgentEvent::AuthRestored
            | AgentEvent::SubagentProgress { .. }
            | AgentEvent::SubagentHistory { .. }
            | AgentEvent::ToolSnapshot { .. }
            | AgentEvent::ToolHeaderSnapshot { .. }
            | AgentEvent::LiveToolBuf { .. }
            | AgentEvent::Nudge
            | AgentEvent::Injected { .. }
            | AgentEvent::ToolsLoaded { .. }
            | AgentEvent::PromptProgress { .. } => {}
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
                if self.include_partial_messages {
                    let events = self.synth.finish_message(&TokenUsage::default());
                    self.emit_stream(events)?;
                }
            }
            AgentEvent::GoalEvaluating { .. }
            | AgentEvent::GoalFinished { .. }
            | AgentEvent::GoalDeferred { .. }
            | AgentEvent::GoalLoopCap { .. }
            | AgentEvent::GoalTurnLimit { .. }
            | AgentEvent::GoalClearedAfterError { .. } => {}
            AgentEvent::GoalEvaluation { cost, billing, .. }
            | AgentEvent::GoalEvaluationFailed { cost, billing, .. } => {
                self.add_spend(*cost, *billing);
            }
            AgentEvent::Retry {
                attempt,
                message,
                delay_ms,
            } => {
                if self.include_partial_messages {
                    let events = self.synth.finish_message(&TokenUsage::default());
                    self.emit_stream(events)?;
                }
                self.writer.emit_system(
                    "api_retry",
                    serde_json::json!({
                        "attempt": attempt,
                        "retry_delay_ms": delay_ms,
                        "error": message,
                    }),
                )?;
            }
            AgentEvent::TurnComplete(tc) => {
                self.add_spend(tc.cost, tc.billing);
                if self.include_partial_messages {
                    let events = self.synth.finish_message(&tc.usage);
                    self.emit_stream(events)?;
                }

                let content_value = serde_json::to_value(&tc.message.content)?;
                if parent_tool_use_id.is_none() {
                    self.result_text = content_text(&content_value).unwrap_or_default();
                }
                self.writer.emit(WireInner::Assistant(AssistantPayload {
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
                self.add_spend(*cost, *billing);
                if parent_tool_use_id.is_none() {
                    self.auxiliary_usage += *usage;
                }
                self.writer.emit_system(
                    "model_usage",
                    serde_json::json!({
                        "accounting": envelope.event,
                        "parent_tool_use_id": parent_tool_use_id,
                        "workflow": envelope.workflow,
                    }),
                )?;
            }
            AgentEvent::ToolResultsSubmitted { message } => {
                self.writer.emit(WireInner::User(UserPayload {
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
                let tool_name = request.tool.to_string();

                let emitted = self
                    .writer
                    .emit(WireInner::ControlRequest(ControlRequestPayload {
                        request_id: req_id.clone(),
                        request: ControlRequestInner {
                            subtype: "can_use_tool",
                            tool_name: Some(caudra_to_claude_tool_name(&tool_name).into()),
                            input: Some(request.input.clone()),
                            tool_use_id: Some(request.id.clone()),
                        },
                    }));
                if let Err(error) = emitted {
                    self.shared.lock().unwrap().pending.remove(&req_id);
                    self.permissions.answer(&request.id, PermissionAnswer::Deny);
                    return Err(error);
                }
            }
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
                self.shared
                    .lock()
                    .unwrap()
                    .resolved_permission_requests
                    .clear();
                // An interrupted run leaves a partial answer, so it is not a success.
                let is_error = *reason == DoneReason::Cancelled;
                let result = mem::take(&mut self.result_text);
                self.emit_turn_result(is_error, result, *num_turns, *usage)?;
            }
            AgentEvent::Error { message } => {
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
    use caudra_agent::SubagentInfo;
    use caudra_agent::permissions::PermissionRequest;
    use caudra_agent::tools::PermissionScopes;
    use caudra_agent::types::WORKFLOW_EVENT_RUN_ID;
    use caudra_providers::{ContentBlock, Message, Role};
    use caudra_storage::usage_ledger::LedgerPurpose;
    use caudra_workflow::{RunSnapshot, RunStatus, RunUsage, SourceKind};
    use tempfile::TempDir;
    use test_case::test_case;

    const CAUDRA_REQUEST_ID: &str = "caudra-permission-1";
    const SECOND_CAUDRA_REQUEST_ID: &str = "caudra-permission-2";
    const REPAIR_COST: f64 = 0.25;
    const REPAIR_INPUT: u32 = 17;
    const REPAIR_PARENT: &str = "repair-parent";
    const REPAIR_FAILURE: &str = "transport failed";
    const WORKSPACE_REBIND_REQUIRED: &str =
        "session workspace identity changed; fork or explicitly rebind the session";

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
            turn_start: Instant::now(),
            pending,
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
            store.load_text(target.id(), output_ref.id).unwrap(),
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
    fn saved_sdk_fork_restores_rebased_subagent_history_and_rejects_collision() {
        let temp = TempDir::new().unwrap();
        let storage = StateDir::from_path(temp.path().to_path_buf());
        let mut source = StoredSession::new("provider/model", "/repo");
        source.meta.system_prompt_profile = Some("review".into());
        source.meta.structured_permission_rules = vec![stored_structured_rule()];
        source.meta.yolo = Some(true);
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
        assert_eq!(loaded.meta.yolo, None);
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
        session.meta.yolo = Some(true);

        let resumed = session_permissions(&session, false);
        let forked = session_permissions(&session, true);

        assert_eq!(
            resumed,
            (session.meta.structured_permission_rules.clone(), Some(true))
        );
        assert_eq!(forked, (Vec::new(), None));
    }

    #[test]
    fn remote_session_mismatch_fails_before_permissions_can_be_reused() {
        let mut session = StoredSession::new("provider/model", "/first");
        session.meta.yolo = Some(true);
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
            workflow: None,
        })
        .unwrap();
        pump.handle(Envelope {
            event: AgentEvent::StreamReset,
            subagent: None,
            run_id: 0,
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
            PermissionMode::resolve(None, false),
            PermissionMode::Default
        );
        assert_eq!(
            PermissionMode::resolve(None, true),
            PermissionMode::BypassPermissions
        );
        assert_eq!(
            PermissionMode::resolve(Some("plan"), true),
            PermissionMode::Plan
        );
        assert_eq!(
            PermissionMode::resolve(Some("bogus"), false),
            PermissionMode::Default
        );
    }

    #[test_case(PermissionMode::Default, true => PermissionMode::BypassPermissions ; "stored_yolo_is_reported")]
    #[test_case(PermissionMode::BypassPermissions, false => PermissionMode::Default ; "stored_off_overrides_flag")]
    #[test_case(PermissionMode::Plan, true => PermissionMode::Plan ; "plan_mode_is_preserved")]
    fn effective_mode_tracks_restored_yolo(
        requested: PermissionMode,
        yolo: bool,
    ) -> PermissionMode {
        effective_permission_mode(requested, yolo)
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

        forward_workflow_request(&writer, None, "req_1", WorkflowRequest::List);

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
        let payload = init_payload(serde_json::json!({"cwd": "/tmp"}), workflows);
        assert_eq!(payload["cwd"], "/tmp");
        (
            payload["workflows"].as_bool().unwrap(),
            payload["workflow_controls"].as_array().unwrap().len(),
        )
    }
}
