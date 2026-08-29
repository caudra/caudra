//! SDK streaming mode: `maki --print --input-format stream-json`.
//!
//! Wire protocol matches Claude Code's SDK interface so tools like Conductor, Windsurf, and custom
//! orchestrators work without adaptation.
//!
//! Per-message wire ids (`uuid`, assistant `message.id`) use `uuid::Uuid::now_v7()` to emit the
//! hyphenated-hex UUIDv7 shape that Claude Code SDK consumers expect, rather than maki's base58
//! `MakiId` canonical form.

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, Write};
use std::mem;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use color_eyre::Result;
use color_eyre::eyre::{Context, eyre};
use flume::{Receiver, Sender};
use maki_agent::headless::{self, InteractiveHandle, InteractiveParams};
use maki_agent::mcp;
use maki_agent::permissions::{
    PermissionAnswer, PermissionLifetime, PermissionManager, PluginRuleStore,
};
use maki_agent::prompt::ResolvedSlots;
use maki_agent::prompt::profile::{BUILTIN_PROFILE_NAME, PromptProfileCatalog};
use maki_agent::tools::QUESTION_TOOL_NAME;
use maki_agent::{
    AgentConfig, AgentEvent, AgentInput, AgentMode, DoneReason, Envelope, History,
    PermissionsConfig, StoredSession,
};
use maki_config::ModelPolicy;
use maki_providers::model::Model;
use maki_providers::{
    HistoryItem, HistoryItemKind, ImageSource, StopReason, Timeouts, TokenUsage, add_cost,
};
use maki_storage::id::SessionRef;
use maki_storage::permission_state::PermissionRuleRecord;
use maki_storage::sessions::{SessionError, StoredRule};
use maki_storage::tool_outputs::{ToolOutputRef, ToolOutputStore};
use maki_storage::{StateDir, StorageError};
use serde::Serialize;
use serde_json::Value;
use tracing::warn;

use crate::cli::Cli;

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
    ("code_execution", "CodeExecution"),
    ("execution_environment", "ExecutionEnvironment"),
    ("index", "Index"),
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
}

impl StreamSynth {
    fn new() -> Self {
        Self {
            block_index: -1,
            started: false,
            current_block: None,
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

    fn tool_use(&mut self, model: &str, id: &str, name: &str, input_json: &str) -> Vec<Value> {
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
        self.current_block.take().map(|_| self.block_stop())
    }

    fn block_stop(&self) -> Value {
        serde_json::json!({
            "type": "content_block_stop",
            "index": self.block_index,
        })
    }
}

fn maki_to_claude_tool_name(name: &str) -> &str {
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
    pub workflow: bool,
    pub model_policy: Arc<ModelPolicy>,
    pub plugin_rules: Arc<PluginRuleStore>,
}

struct Shared {
    model: Model,
    permission_mode: PermissionMode,
    turn_start: Instant,
    pending: HashMap<String, String>,
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
        workflow,
        model_policy,
        plugin_rules,
    } = params;
    cli.warn_ignored_flags();
    if let Some(max) = cli.max_turns {
        config.max_turns = Some(max);
    }
    let requested_permission_mode =
        PermissionMode::resolve(cli.permission_mode.as_deref(), cli.yolo);
    let system_prompt_override = cli.system_prompt.clone().filter(|s| !s.is_empty());

    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let working_dir = cwd.to_string_lossy().into_owned();
    let ResolvedSession {
        session_id,
        initial_history,
        session_rules,
        structured_permission_rules,
        session_yolo,
        stored_system_prompt_profile,
    } = resolve_session(
        &cli,
        &working_dir,
        &prompt_profiles,
        config.system_prompt_profile.as_deref(),
        system_prompt_override.is_some(),
    )?;
    crate::setup::report_session_start(
        if initial_history.is_empty() {
            maki_otel::emit::START_FRESH
        } else {
            maki_otel::emit::START_RESUME
        },
        session_id.as_ref(),
    );

    let (mcp_handle, mcp_config_errors) = smol::block_on(mcp::start_connected(&cwd));
    if !mcp_config_errors.is_empty() {
        eprintln!("MCP config error: {mcp_config_errors}");
    }
    if let Some(handle) = &mcp_handle {
        let awaiting: Vec<_> = handle
            .reader()
            .load()
            .infos
            .iter()
            .filter(|info| info.status == maki_agent::McpServerStatus::AwaitingTrust)
            .map(|info| info.name.clone())
            .collect();
        if !awaiting.is_empty() {
            return Err(eyre!(
                "project MCP servers require startup trust: {}. Run `maki`, review them with `/mcp`, then retry",
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
                        maki_agent::McpServerStatus::Running => "connected",
                        maki_agent::McpServerStatus::Connecting => "connecting",
                        maki_agent::McpServerStatus::AwaitingTrust => "pending",
                        maki_agent::McpServerStatus::Disabled => "disabled",
                        maki_agent::McpServerStatus::Failed(_) => "failed",
                        maki_agent::McpServerStatus::NeedsAuth { .. } => "needs-auth",
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
    let handle = headless::spawn_interactive(InteractiveParams {
        model,
        config,
        permissions_config,
        timeouts,
        prompt_slots: Arc::new(prompt_slots),
        system_prompt_profile,
        system_prompt_profile_name,
        excluded_tools: vec![QUESTION_TOOL_NAME],
        mcp_handle,
        initial_wd: cwd.clone(),
        session_id,
        initial_history,
        yolo: requested_permission_mode == PermissionMode::BypassPermissions,
        session_rules,
        structured_permission_rules,
        session_yolo,
        system_prompt_override,
        append_system_prompt: cli.append_system_prompt.clone().filter(|s| !s.is_empty()),
        workflow,
        model_policy: Arc::clone(&model_policy),
        plugin_rules,
        local_tools: Default::default(),
    });
    let permission_mode =
        effective_permission_mode(requested_permission_mode, handle.permissions.is_yolo());

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
        .map(|t| maki_to_claude_tool_name(t))
        .collect();
    writer.emit_system(
        "init",
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
    )?;

    let shared = Arc::new(Mutex::new(Shared {
        model: startup_model.clone(),
        permission_mode,
        turn_start: Instant::now(),
        pending: HashMap::new(),
    }));

    let pump = EventPump {
        writer: writer.clone(),
        shared: Arc::clone(&shared),
        permissions: Arc::clone(&handle.permissions),
        include_partial_messages: cli.include_partial_messages,
        synth: StreamSynth::new(),
        result_text: String::new(),
        cost: None,
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
                let mode = {
                    let mut shared = shared.lock().unwrap();
                    shared.turn_start = Instant::now();
                    shared.permission_mode
                };
                let input = AgentInput {
                    message: prompt,
                    mode: mode.agent_mode(&cwd),
                    images,
                    preamble: Vec::new(),
                    thinking: Default::default(),
                    fast,
                    workflow,
                    prompt: None,
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

struct ResolvedSession {
    session_id: Option<SessionRef>,
    initial_history: Vec<HistoryItem>,
    session_rules: Vec<StoredRule>,
    structured_permission_rules: Vec<PermissionRuleRecord>,
    session_yolo: Option<bool>,
    stored_system_prompt_profile: Option<String>,
}

fn session_permissions(
    session: &StoredSession,
    fork: bool,
) -> (Vec<StoredRule>, Vec<PermissionRuleRecord>, Option<bool>) {
    if fork {
        (Vec::new(), Vec::new(), None)
    } else {
        (
            session.meta.session_rules.clone(),
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
) -> Result<ResolvedSession> {
    let cli_session_id = cli
        .session_id
        .as_deref()
        .map(|session_id| {
            session_id
                .parse::<SessionRef>()
                .map_err(|error| eyre!("invalid session id {session_id:?}: {error}"))
        })
        .transpose()?;

    let (
        resumed_id,
        initial_history,
        session_rules,
        structured_permission_rules,
        session_yolo,
        stored_system_prompt_profile,
    ) = if let Some(id) = &cli.session {
        let storage = StateDir::resolve().context("resolve state dir")?;
        let session_ref: SessionRef = id
            .parse()
            .map_err(|e| eyre!("invalid session id {id}: {e}"))?;
        let session = crate::setup::load_session(session_ref.id(), &storage)
            .map_err(|e| eyre!("load session {id}: {e}"))?;
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
            let (session_rules, structured_permission_rules, session_yolo) =
                session_permissions(&session, true);
            let target = cli_session_id.clone().unwrap_or_else(SessionRef::generate);
            if target.id() == session_ref.id() {
                return Err(eyre!(
                    "fork session ID must differ from source session {id}"
                ));
            }
            ensure_fork_target_available(&storage, &target)?;
            let history = rebase_history(history)?;
            let subagent_histories = session
                .subagent_messages()
                .iter()
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
            if let Err(error) =
                save_sdk_fork(&storage, &session, &target, &history, subagent_histories)
            {
                let _ = ToolOutputStore::new(storage.clone()).delete_session(target.id());
                return Err(error);
            }
            (
                Some(target),
                history,
                session_rules,
                structured_permission_rules,
                session_yolo,
                session.meta.system_prompt_profile.clone(),
            )
        } else {
            if cli_session_id
                .as_ref()
                .is_some_and(|target| target.id() != session_ref.id())
            {
                return Err(eyre!(
                    "--session-id cannot replace the resumed session ID without --fork-session"
                ));
            }
            let (session_rules, structured_permission_rules, session_yolo) =
                session_permissions(&session, false);
            (
                Some(session_ref),
                history,
                session_rules,
                structured_permission_rules,
                session_yolo,
                session.meta.system_prompt_profile.clone(),
            )
        }
    } else if cli.continue_session {
        let storage = StateDir::resolve().context("resolve state dir")?;
        match crate::setup::latest_session(cwd, &storage) {
            Ok(Some(session)) => {
                let history = crate::setup::active_session_history(&session)?;
                let (session_rules, structured_permission_rules, session_yolo) =
                    session_permissions(&session, false);
                (
                    Some(SessionRef::from(session.id)),
                    history,
                    session_rules,
                    structured_permission_rules,
                    session_yolo,
                    session.meta.system_prompt_profile.clone(),
                )
            }
            _ => (None, Vec::new(), Vec::new(), Vec::new(), None, None),
        }
    } else {
        (None, Vec::new(), Vec::new(), Vec::new(), None, None)
    };

    Ok(ResolvedSession {
        session_id: cli_session_id.or(resumed_id),
        initial_history,
        session_rules,
        structured_permission_rules,
        session_yolo,
        stored_system_prompt_profile,
    })
}

fn resolve_prompt_profile(
    catalog: &PromptProfileCatalog,
    cli_name: Option<&str>,
    stored_name: Option<&str>,
    configured_name: Option<&str>,
    raw_prompt_override: bool,
) -> Result<(
    Option<String>,
    Option<Arc<maki_agent::prompt::profile::SystemPromptProfile>>,
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
    match maki_agent::load_stored_session(target.id(), storage) {
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
) -> Result<()> {
    let mut fork = StoredSession::new(&source.model, &source.cwd);
    fork.id = target.id();
    fork.meta.system_prompt_profile = source.meta.system_prompt_profile.clone();
    fork.replace_messages(history.to_vec());
    fork.set_title(format!("{} (fork)", source.title));
    for (task_id, history) in subagent_histories {
        fork.set_subagent_messages(task_id, history);
    }
    fork.set_subagents(source.subagents().to_vec());
    fork.save(storage)
        .map_err(|error| eyre!("save fork session {target}: {error}"))
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
                Some(model) => {
                    let _ = handle.model_tx.send(model.clone());
                    shared.lock().unwrap().model = model;
                    writer.emit_control_response(&cr.request_id, ok, None)
                }
                None => writer.emit_control_response(
                    &cr.request_id,
                    None,
                    Some("invalid or disallowed model".into()),
                ),
            }
        }
        other => writer.emit_control_response(
            &cr.request_id,
            None,
            Some(format!("unsupported: {other}")),
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
        maki_to_claude_tool_name(&tool).to_owned()
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
    let Some(request_id) = shared.lock().unwrap().pending.remove(sdk_request_id) else {
        warn!(
            sdk_request_id,
            "response for unknown SDK permission request"
        );
        return false;
    };
    if permissions.answer(&request_id, answer) {
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

    fn reset_turn(&mut self) {
        self.synth.reset();
        self.result_text.clear();
        self.cost = None;
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
            AgentEvent::ToolStart(ts) => {
                let name = ts.tool.to_string();
                let input = ts.raw_input.clone().unwrap_or(Value::Null);

                if self.include_partial_messages {
                    let model = self.model_id();
                    let events = self.synth.tool_use(
                        &model,
                        &ts.id,
                        maki_to_claude_tool_name(&name),
                        &serde_json::to_string(&input)?,
                    );
                    self.emit_stream(events)?;
                }
            }
            AgentEvent::ToolPending { .. }
            | AgentEvent::ToolOutput { .. }
            | AgentEvent::ToolDone(_)
            | AgentEvent::QueueItemConsumed { .. }
            | AgentEvent::QueueBatchConsumed { .. }
            | AgentEvent::QueueDrained
            | AgentEvent::AutoCompacting
            | AgentEvent::CompactionDone
            | AgentEvent::AuthRequired
            | AgentEvent::SubagentHistory { .. }
            | AgentEvent::ToolSnapshot { .. }
            | AgentEvent::ToolHeaderSnapshot { .. }
            | AgentEvent::LiveToolBuf { .. }
            | AgentEvent::Nudge
            | AgentEvent::PromptProgress { .. } => {}
            AgentEvent::GoalEvaluating { .. }
            | AgentEvent::GoalFinished { .. }
            | AgentEvent::GoalDeferred { .. }
            | AgentEvent::GoalLoopCap { .. }
            | AgentEvent::GoalTurnLimit { .. }
            | AgentEvent::GoalClearedAfterError { .. } => {}
            AgentEvent::GoalEvaluation { cost, .. } => add_cost(&mut self.cost, *cost),
            AgentEvent::GoalEvaluationFailed { cost, .. } => add_cost(&mut self.cost, *cost),
            AgentEvent::Retry {
                attempt,
                message,
                delay_ms,
            } => {
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
                add_cost(&mut self.cost, tc.cost);
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
                }))?;
            }
            AgentEvent::ToolResultsSubmitted { message } => {
                self.writer.emit(WireInner::User(UserPayload {
                    message: UserMessage {
                        role: "user",
                        content: serde_json::to_value(&message.content)?,
                    },
                    parent_tool_use_id,
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
                            tool_name: Some(maki_to_claude_tool_name(&tool_name).into()),
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
            AgentEvent::Done {
                usage,
                num_turns,
                reason,
            } => {
                // An interrupted run leaves a partial answer, so it is not a success.
                let is_error = *reason == DoneReason::Cancelled;
                let result = mem::take(&mut self.result_text);
                self.emit_turn_result(is_error, result, *num_turns, *usage)?;
            }
            AgentEvent::Error { message } => {
                self.emit_turn_result(true, message.clone(), 0, TokenUsage::default())?;
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
                        b["name"] = Value::String(maki_to_claude_tool_name(name).to_string());
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
    use maki_agent::permissions::PermissionRequest;
    use maki_agent::tools::PermissionScopes;
    use maki_providers::{ContentBlock, Message, Role};
    use maki_storage::sessions::{StoredEffect, StoredRule};
    use tempfile::TempDir;
    use test_case::test_case;

    const SESSION_PERMISSION_SCOPE: &str = "cargo *";
    const MAKI_REQUEST_ID: &str = "maki-permission-1";
    const SECOND_MAKI_REQUEST_ID: &str = "maki-permission-2";

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
        }))
    }

    fn pending_permission(
        manager: Arc<PermissionManager>,
        request_id: &str,
        scope: &str,
        input: Value,
    ) -> (smol::Task<bool>, Receiver<Envelope>) {
        let (event_tx, event_rx) = flume::unbounded();
        let event_tx = maki_agent::EventSender::new(event_tx, 0);
        let request_id = request_id.to_owned();
        let scopes = PermissionScopes::single(scope.to_owned());
        let task = smol::spawn(async move {
            let (_legacy_tx, legacy_rx) = flume::unbounded();
            let legacy_rx = smol::lock::Mutex::new(legacy_rx);
            manager
                .enforce(
                    &maki_config::ToolKey::native("shell"),
                    &scopes,
                    &input,
                    &event_tx,
                    Some(&legacy_rx),
                    &request_id,
                    &maki_agent::CancelToken::none(),
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
                &maki_config::ToolKey::native("shell"),
                &PermissionScopes::single(scope.to_owned()),
                &serde_json::json!({"command": scope}),
                &maki_agent::EventSender::new(event_tx, 0),
                None,
                "follow-up",
                &maki_agent::CancelToken::none(),
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
            request_counter: 0,
        };
        (pump, out_rx, shared)
    }

    fn history_messages(items: Vec<HistoryItem>) -> Vec<Message> {
        History::restored(items).unwrap().into_vec()
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

    fn stored_session_rule() -> StoredRule {
        StoredRule {
            tool: "bash".into(),
            scope: Some(SESSION_PERMISSION_SCOPE.into()),
            effect: StoredEffect::Allow,
        }
    }

    fn stored_structured_rule() -> PermissionRuleRecord {
        let request = PermissionRequest::from_legacy(
            "stored-structured".into(),
            maki_config::ToolKey::native("bash"),
            vec!["cargo test".into()],
            serde_json::json!({"command": "cargo test"}),
            Path::new("/repo"),
            false,
        );
        PermissionRuleRecord::conversation(
            request
                .option_rule(
                    "allow_exact",
                    maki_agent::permissions::PermissionLifetime::Conversation,
                )
                .unwrap(),
        )
        .unwrap()
    }

    fn claude_to_maki_tool_name(name: &str) -> &str {
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
    #[test_case("code_execution", "CodeExecution")]
    #[test_case("execution_environment", "ExecutionEnvironment")]
    #[test_case("index", "Index")]
    #[test_case("memory", "Memory")]
    #[test_case("question", "Question")]
    fn maki_to_claude_roundtrip(maki: &str, claude: &str) {
        assert_eq!(maki_to_claude_tool_name(maki), claude);
        assert_eq!(claude_to_maki_tool_name(claude), maki);
    }

    #[test]
    fn unknown_tool_name_passthrough() {
        assert_eq!(maki_to_claude_tool_name("unknown_tool"), "unknown_tool");
        assert_eq!(claude_to_maki_tool_name("UnknownTool"), "UnknownTool");
    }

    #[test]
    fn public_user_message_json_omits_managed_output_ref() {
        let output_ref = ToolOutputRef {
            id: maki_storage::id::MakiId::generate()
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
        source.meta.session_rules = vec![stored_session_rule()];
        source.meta.structured_permission_rules = vec![stored_structured_rule()];
        source.meta.yolo = Some(true);
        let target = SessionRef::generate();
        let history = History::new(vec![Message::user("main".into())]).into_items();
        let subagent = History::new(vec![Message::user("nested".into())]).into_items();

        save_sdk_fork(
            &storage,
            &source,
            &target,
            &history,
            HashMap::from([("task-1".into(), subagent.clone())]),
        )
        .unwrap();

        let loaded = maki_agent::load_stored_session(target.id(), &storage).unwrap();
        assert_eq!(loaded.messages(), history);
        assert_eq!(loaded.subagent_messages()["task-1"].as_ref(), &subagent);
        assert!(loaded.meta.session_rules.is_empty());
        assert!(loaded.meta.structured_permission_rules.is_empty());
        assert_eq!(loaded.meta.yolo, None);
        assert_eq!(loaded.meta.system_prompt_profile.as_deref(), Some("review"));
        assert!(ensure_fork_target_available(&storage, &target).is_err());
        assert!(ensure_fork_target_available(&storage, &SessionRef::generate()).is_ok());
    }

    #[test]
    fn sdk_resume_restores_permissions_while_fork_starts_clean() {
        let mut session = StoredSession::new("provider/model", "/repo");
        session.meta.session_rules = vec![stored_session_rule()];
        session.meta.structured_permission_rules = vec![stored_structured_rule()];
        session.meta.yolo = Some(true);

        let resumed = session_permissions(&session, false);
        let forked = session_permissions(&session, true);

        assert_eq!(
            resumed,
            (
                session.meta.session_rules.clone(),
                session.meta.structured_permission_rules.clone(),
                Some(true)
            )
        );
        assert_eq!(forked, (Vec::new(), Vec::new(), None));
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
            MAKI_REQUEST_ID.into(),
            maki_config::ToolKey::native("bash"),
            vec!["cargo test".into()],
            input.clone(),
            Path::new("/project"),
            false,
        );

        pump.handle(Envelope {
            event: AgentEvent::PermissionRequest(Box::new(request)),
            subagent: None,
            run_id: 0,
        })
        .unwrap();

        let message: Value = serde_json::from_str(&out_rx.try_recv().unwrap()).unwrap();
        assert_eq!(message["type"], "control_request");
        assert_eq!(message["request"]["subtype"], "can_use_tool");
        assert_eq!(message["request"]["input"], input);
        assert_eq!(message["request"]["tool_use_id"], MAKI_REQUEST_ID);
        assert_eq!(shared.lock().unwrap().pending["req_1"], MAKI_REQUEST_ID);
    }

    #[test]
    fn sdk_permission_responses_correlate_concurrent_requests_out_of_order() {
        smol::block_on(async {
            let manager = permission_manager();
            let (first, first_events) = pending_permission(
                Arc::clone(&manager),
                MAKI_REQUEST_ID,
                "cargo test",
                serde_json::json!({"command": "cargo test"}),
            );
            let (second, second_events) = pending_permission(
                Arc::clone(&manager),
                SECOND_MAKI_REQUEST_ID,
                "cargo check",
                serde_json::json!({"command": "cargo check"}),
            );
            let _ = first_events.recv_async().await.unwrap();
            let _ = second_events.recv_async().await.unwrap();
            let shared = shared_with_pending(HashMap::from([
                ("req_1".into(), MAKI_REQUEST_ID.into()),
                ("req_2".into(), SECOND_MAKI_REQUEST_ID.into()),
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
    fn sdk_updated_permissions_grant_only_exact_conversation_scope() {
        smol::block_on(async {
            let manager = permission_manager();
            let (task, events) = pending_permission(
                Arc::clone(&manager),
                MAKI_REQUEST_ID,
                "cargo test",
                serde_json::json!({"command": "cargo test"}),
            );
            let _ = events.recv_async().await.unwrap();
            let shared =
                shared_with_pending(HashMap::from([("req_1".into(), MAKI_REQUEST_ID.into())]));

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
                MAKI_REQUEST_ID,
                "cargo test",
                serde_json::json!({"command": "cargo test"}),
            );
            let _ = events.recv_async().await.unwrap();
            let shared =
                shared_with_pending(HashMap::from([("req_1".into(), MAKI_REQUEST_ID.into())]));

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
                MAKI_REQUEST_ID,
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
            &maki_config::ModelPolicy::default(),
        )
        .unwrap();
        assert_eq!(result.id, startup.id);
    }

    #[test]
    fn resolve_set_model_rejects_disallowed_exact_spec() {
        let startup = Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap();
        let raw: maki_config::RawConfig = serde_json::from_value(serde_json::json!({
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
}
