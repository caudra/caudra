use std::collections::HashMap;
use std::io::Write;
use std::iter;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol_schema::{
    AgentNotification, AgentRequest, AgentResponse, ConfigOptionUpdate, ContentBlock,
    CurrentModeUpdate, EmbeddedResourceResource, Error as AcpError, ImageContent,
    InitializeRequest, JsonRpcMessage, LoadSessionRequest, McpServer, NewSessionRequest,
    Notification, PromptRequest, PromptResponse, Request, RequestId, RequestPermissionRequest,
    RequestPermissionResponse, Response, SessionId, SessionModeId, SessionNotification,
    SessionUpdate, SetSessionConfigOptionRequest, SetSessionConfigOptionResponse,
    SetSessionModeRequest, SetSessionModeResponse, StopReason, TextContent, ToolCallContent,
    ToolCallId, ToolCallUpdate, ToolCallUpdateFields,
};
#[cfg(test)]
use caudra_agent::ToolOutput;
use caudra_agent::headless::{self, InteractiveHandle, InteractiveParams};
use caudra_agent::mcp::config::{McpServerStatus, RawHttpFields, RawStdioFields, RawTransport};
use caudra_agent::mcp::{self, McpHandle};
use caudra_agent::permissions::{PermissionAnswer, PermissionRequest as CaudraPermissionRequest};
use caudra_agent::prompt::profile::{BUILTIN_PROFILE_NAME, SystemPromptProfile};
use caudra_agent::tools::{LocalToolFn, LocalTools, QUESTION_TOOL_NAME, local_tool};
use caudra_agent::types::AgentEvent;
use caudra_agent::{
    AgentInput, AgentMode, Envelope, History, ImageMediaType, ImageSource, load_stored_session,
};
use caudra_config::{MAX_SERVER_NAME_LEN, ModelPolicy};
use caudra_providers::model::Model;
use caudra_providers::provider::{available_model_specs, fetch_all_models};
use caudra_providers::{
    HistoryItem, TokenUsage, active_history_items, add_cost, resolve_history_head, settle_session,
};
#[cfg(test)]
use caudra_providers::{Message, expand_message};
use caudra_storage::StateDir;
use caudra_storage::id::{CaudraId, SessionRef};
use caudra_storage::permission_state::PermissionRuleRecord;
use caudra_storage::sessions::{SessionError, SessionLease, StoredRule, StoredTokenUsage};
use color_eyre::eyre::Context;
use flume::{Receiver, Sender, WeakSender};
use serde::Serialize;
use serde_json::Value;
use smol::io::AsyncBufReadExt;
use tracing::{debug, warn};

use crate::{AcpParams, elicitation, methods, permissions, translate};

const FIRST_OUTGOING_REQUEST_ID: i64 = 1000;
const SESSION_IN_USE_ERROR_CODE: i32 = -32001;
/// ACP has no fast-mode toggle, so a restored total is priced at standard rates.
const RESTORED_FAST: bool = false;

/// Ids come from here and are never reused, so a late answer for a closed
/// session cannot match a request of the session that replaced it.
static NEXT_OUTGOING_REQUEST_ID: AtomicI64 = AtomicI64::new(FIRST_OUTGOING_REQUEST_ID);

/// What the client still owes us. Permission requests have independent broker
/// waiters; elicitation continues to use the interactive answer channel.
#[derive(Default)]
struct Pending {
    prompt: Option<RequestId>,
    asks: HashMap<i64, AskKind>,
}

enum AskKind {
    Permission {
        request_id: String,
        exact_project_deny: bool,
    },
    ResolvedPermission,
    Elicitation,
}

type PendingState = Arc<Mutex<Pending>>;

struct SessionState {
    handle: InteractiveHandle,
    mcp: Option<McpHandle>,
    current_mode: AgentMode,
    current_model: String,
    pending: PendingState,
}

struct Server {
    out_tx: Sender<Value>,
    model_specs: Vec<String>,
    model_policy: Arc<ModelPolicy>,
    thinking: caudra_providers::ThinkingConfig,
    client_elicits_form: bool,
    session: Option<SessionState>,
}

impl Server {
    fn respond(&self, id: RequestId, result: Result<AgentResponse, AcpError>) {
        send(&self.out_tx, Response::new(id, result));
    }
}

enum Incoming {
    Line(String),
    Models(Vec<String>),
}

pub async fn serve(params: AcpParams) -> color_eyre::Result<()> {
    let (out_tx, out_rx) = flume::unbounded::<Value>();

    let writer_task = smol::spawn(async move {
        let stdout = std::io::stdout();
        while let Ok(msg) = out_rx.recv_async().await {
            let mut handle = stdout.lock();
            if serde_json::to_writer(&mut handle, &msg).is_ok() {
                let _ = handle.write_all(b"\n");
                let _ = handle.flush();
            }
        }
    });

    let mut server = Server {
        out_tx,
        model_specs: available_model_specs(&params.model_policy),
        model_policy: Arc::clone(&params.model_policy),
        thinking: params.thinking.clone(),
        client_elicits_form: false,
        session: None,
    };

    let (in_tx, in_rx) = flume::unbounded::<Incoming>();
    // Weak, so a discovery still in flight cannot keep the loop alive once stdin closes.
    discover_models(Arc::clone(&params.model_policy), in_tx.downgrade());
    let reader_task = smol::spawn(read_stdin(in_tx));

    while let Ok(incoming) = in_rx.recv_async().await {
        match incoming {
            Incoming::Line(line) => handle_line(&mut server, &line, &params).await,
            Incoming::Models(specs) => refresh_models(&mut server, specs),
        }
    }

    drop(server);
    writer_task.await;
    reader_task.await.context("read stdin")?;

    Ok(())
}

/// Lives in its own task because `read_line` is not cancel safe: the main loop
/// waits on discovery too, and a dropped read would eat half a line.
async fn read_stdin(tx: Sender<Incoming>) -> std::io::Result<()> {
    let mut reader = smol::io::BufReader::new(smol::Unblock::new(std::io::stdin()));
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        if tx.send_async(Incoming::Line(line)).await.is_err() {
            return Ok(());
        }
    }
}

/// Static manifests miss providers that only list their models over the wire
/// (OpenRouter and friends), so the same discovery the TUI runs happens here,
/// in the background. Each batch leaves the moment it lands: the slowest source
/// is a cold catalog download, and a provider the client could already pick from
/// should not wait behind it.
fn discover_models(policy: Arc<ModelPolicy>, tx: WeakSender<Incoming>) {
    smol::spawn(async move {
        fetch_all_models(
            &policy,
            |batch| {
                if let Some(tx) = tx.upgrade() {
                    let _ = tx.send(Incoming::Models(batch.models));
                }
            },
            None,
        )
        .await;
    })
    .detach();
}

/// Discovery lands in batches after the client built its selector from the
/// offline list, so every batch that adds something announces the fuller list.
fn refresh_models(srv: &mut Server, batch: Vec<String>) {
    let known = srv.model_specs.len();
    for spec in batch {
        if !srv.model_specs.contains(&spec) {
            srv.model_specs.push(spec);
        }
    }
    if srv.model_specs.len() == known {
        return;
    }
    // Merged even with no session yet, since session/new builds its selector from this list.
    let Some(session) = &srv.session else { return };
    let option = methods::model_config_option(&session.current_model, &srv.model_specs);
    session_update(
        &srv.out_tx,
        &SessionId::from(session.handle.session_id.to_string()),
        SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(vec![option])),
    );
}

async fn handle_line(server: &mut Server, line: &str, params: &AcpParams) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }

    let raw: Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "invalid JSON on stdin");
            server.respond(RequestId::Null, Err(AcpError::parse_error()));
            return;
        }
    };

    let id = raw.get("id").map(request_id);

    if raw.get("result").is_some() || raw.get("error").is_some() {
        handle_incoming_response(server, &raw);
    } else if let Some(method) = raw.get("method").and_then(Value::as_str) {
        match id {
            Some(id) => handle_request(server, method, id, &raw, params).await,
            None => handle_notification(server, method),
        }
    } else if let Some(id) = id {
        server.respond(id, Err(AcpError::invalid_request()));
    }
}

fn request_id(v: &Value) -> RequestId {
    serde_json::from_value(v.clone()).unwrap_or(RequestId::Null)
}

async fn handle_request(
    srv: &mut Server,
    method: &str,
    id: RequestId,
    raw: &Value,
    params: &AcpParams,
) {
    let result = match method {
        "initialize" => {
            srv.client_elicits_form = parse_params::<InitializeRequest>(raw)
                .is_ok_and(|req| elicitation::supports_form(&req.client_capabilities));
            Ok(AgentResponse::InitializeResponse(
                methods::initialize_response(),
            ))
        }
        "session/new" => new_session(srv, raw, params).await,
        "session/load" => load_session(srv, raw, params).await,
        "session/prompt" => match handle_prompt(srv, raw, &id) {
            Ok(()) => return,
            Err(e) => Err(e),
        },
        "session/set_mode" => handle_set_mode(srv, raw),
        "session/set_config_option" => handle_set_config(srv, raw),
        _ => Err(AcpError::method_not_found()),
    };
    srv.respond(id, result);
}

async fn new_session(
    srv: &mut Server,
    raw: &Value,
    params: &AcpParams,
) -> Result<AgentResponse, AcpError> {
    let req: NewSessionRequest = parse_params(raw)?;
    let session_id = SessionRef::generate();
    let session_lease = acquire_session_lease(session_id.id())?;
    let (profile_name, profile) = resolve_prompt_profile(params, None)?;
    preflight_mcp(&req.cwd, &req.mcp_servers).await?;
    let cwd = req.cwd.clone();
    let (mut prepared, pending) = prepare_session(
        srv,
        params,
        SessionStart {
            cwd: cwd.clone(),
            session_id,
            session_lease,
            expected_write_version: None,
            history: Vec::new(),
            permissions: (Vec::new(), Vec::new(), None),
            profile: (profile_name, profile),
        },
    )
    .await?;
    close_session(srv).await;
    let mcp = start_mcp(&cwd, &req.mcp_servers).await;
    prepared.set_mcp_handle(mcp.clone());
    let handle = headless::spawn_prepared_interactive(prepared);
    caudra_otel::emit::session_started(
        caudra_otel::emit::START_FRESH,
        Some(handle.session_id.as_str()),
    );
    let spec = params.model.spec();
    let resp = methods::new_session_response(handle.session_id.as_str())
        .config_options(vec![methods::model_config_option(&spec, &srv.model_specs)]);
    install_session(srv, handle, mcp, spec, pending, cwd, None);
    Ok(AgentResponse::NewSessionResponse(resp))
}

async fn load_session(
    srv: &mut Server,
    raw: &Value,
    params: &AcpParams,
) -> Result<AgentResponse, AcpError> {
    let req: LoadSessionRequest = parse_params(raw)?;
    let session_ref: SessionRef = req
        .session_id
        .0
        .parse()
        .map_err(|_| AcpError::resource_not_found(Some(req.session_id.0.to_string())))?;
    let replacing_current = srv
        .session
        .as_ref()
        .is_some_and(|state| state.handle.session_id.id() == session_ref.id());
    if replacing_current {
        return Err(session_lease_error(SessionError::SessionInUse {
            id: session_ref.id(),
        }));
    }
    let session_lease = acquire_session_lease(session_ref.id())?;
    let mut restored = load_history(session_ref.id())?;
    let (profile_name, profile) =
        resolve_prompt_profile(params, restored.system_prompt_profile.as_deref())?;
    let history = History::restored(restored.history)
        .map_err(|error| AcpError::internal_error().data(json_str(&error)))?;
    preflight_mcp(&req.cwd, &req.mcp_servers).await?;
    let sid = SessionId::from(session_ref.to_string());
    let home = caudra_storage::paths::home();
    let replay_cwd = restored.cwd.as_deref().unwrap_or(&req.cwd);
    let replay_updates = translate::replay_history(history.as_slice(), replay_cwd, home.as_deref());
    let cwd = req.cwd.clone();
    let (mut prepared, pending) = prepare_session(
        srv,
        params,
        SessionStart {
            cwd: cwd.clone(),
            session_id: session_ref,
            session_lease,
            expected_write_version: restored.write_version,
            history: history.into_items(),
            permissions: (
                std::mem::take(&mut restored.session_rules),
                std::mem::take(&mut restored.structured_permission_rules),
                restored.yolo,
            ),
            profile: (profile_name, profile),
        },
    )
    .await?;
    close_session(srv).await;
    let mcp = start_mcp(&cwd, &req.mcp_servers).await;
    prepared.set_mcp_handle(mcp.clone());
    let handle = headless::spawn_prepared_interactive(prepared);
    for update in replay_updates {
        session_update(&srv.out_tx, &sid, update);
    }
    caudra_otel::emit::session_started(
        caudra_otel::emit::START_RESUME,
        Some(handle.session_id.as_str()),
    );
    let spec = params.model.spec();
    let resp = methods::load_session_response()
        .config_options(vec![methods::model_config_option(&spec, &srv.model_specs)]);
    // Priced against the model the session recorded, not the one selected now
    // (which may cost 10x more or less). Later turns add their own exact cost.
    let recorded_model = Model::from_spec(&restored.model).unwrap_or_else(|_| params.model.clone());
    let restored_cost = settle_session(
        &restored.usage,
        &mut restored.by_model,
        &recorded_model,
        RESTORED_FAST,
    );
    install_session(srv, handle, mcp, spec, pending, cwd, restored_cost);
    Ok(AgentResponse::LoadSessionResponse(resp))
}

struct SessionStart {
    cwd: PathBuf,
    session_id: SessionRef,
    session_lease: Arc<SessionLease>,
    expected_write_version: Option<i64>,
    history: Vec<HistoryItem>,
    permissions: (Vec<StoredRule>, Vec<PermissionRuleRecord>, Option<bool>),
    profile: (String, Option<Arc<SystemPromptProfile>>),
}

async fn prepare_session(
    srv: &Server,
    params: &AcpParams,
    start: SessionStart,
) -> Result<(headless::PreparedInteractive, PendingState), AcpError> {
    let pending = PendingState::default();
    // Without form elicitation the question tool would spin forever waiting
    // for a TUI that does not exist, so it is dropped and the model asks in
    // plain text instead.
    let (excluded_tools, local_tools) = if srv.client_elicits_form {
        let tool = question_tool(srv.out_tx.clone(), Arc::clone(&pending));
        let map: LocalTools = Arc::new(HashMap::from([(QUESTION_TOOL_NAME.to_owned(), tool)]));
        (Vec::new(), map)
    } else {
        (vec![QUESTION_TOOL_NAME], LocalTools::default())
    };
    let (session_rules, structured_permission_rules, session_yolo) = start.permissions;
    let (system_prompt_profile_name, system_prompt_profile) = start.profile;
    let prepared = headless::prepare_interactive(InteractiveParams {
        model: params.model.clone(),
        config: params.config.clone(),
        permissions_config: params.permissions_config.clone(),
        timeouts: params.timeouts,
        prompt_slots: Arc::clone(&params.prompt_slots),
        thinking: params.thinking.clone(),
        system_prompt_profile,
        system_prompt_profile_name: Some(system_prompt_profile_name),
        prompt_profiles: Arc::clone(&params.prompt_profiles),
        excluded_tools,
        mcp_handle: None,
        initial_wd: start.cwd,
        session_id: start.session_id,
        session_lease: start.session_lease,
        expected_write_version: start.expected_write_version,
        initial_history: start.history,
        yolo: params.yolo,
        session_rules,
        structured_permission_rules,
        session_yolo,
        system_prompt_override: None,
        append_system_prompt: None,
        workflow: false,
        model_policy: Arc::clone(&params.model_policy),
        plugin_rules: Arc::clone(&params.plugin_rules),
        local_tools,
    })
    .await
    .map_err(|error| AcpError::internal_error().data(json_str(&error)))?;
    Ok((prepared, pending))
}

fn resolve_prompt_profile(
    params: &AcpParams,
    stored_name: Option<&str>,
) -> Result<(String, Option<Arc<SystemPromptProfile>>), AcpError> {
    let requested_name = params
        .system_prompt_profile_override
        .as_deref()
        .or(stored_name)
        .or(params.config.system_prompt_profile.as_deref());
    let profile = params
        .prompt_profiles
        .resolve(requested_name)
        .map_err(|error| AcpError::internal_error().data(json_str(&error)))?;
    Ok((
        requested_name.unwrap_or(BUILTIN_PROFILE_NAME).to_owned(),
        profile,
    ))
}

/// Sends a request the client must answer, registering it first so the
/// response can never race past us.
fn ask_client(
    out_tx: &Sender<Value>,
    pending: &PendingState,
    kind: AskKind,
    request: AgentRequest,
) -> i64 {
    let id = NEXT_OUTGOING_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    pending.lock().unwrap().asks.insert(id, kind);
    send(
        out_tx,
        Request {
            id: RequestId::Number(id),
            method: Arc::from(request.method()),
            params: Some(request),
        },
    );
    id
}

/// Shadows the Lua `question` tool: sends `elicitation/create` to the client
/// and blocks the tool call until the form comes back. Elicitations serialize
/// on the interactive answer channel; permission requests use their broker.
fn question_tool(out_tx: Sender<Value>, pending: PendingState) -> LocalToolFn {
    local_tool(move |input, ctx| {
        let out_tx = out_tx.clone();
        let pending = Arc::clone(&pending);
        Box::pin(async move {
            let session_id = ctx
                .session_id
                .as_ref()
                .map(ToString::to_string)
                .ok_or("no session")?;
            // Batch/code_execution children dispatch with an empty id; a
            // scope pointing at a tool call the client never saw would get
            // the elicitation rejected or dropped.
            let tool_call_id = ctx.tool_use_id.filter(|id| !id.is_empty());
            let request = elicitation::form_request(&session_id, tool_call_id, &input)?;
            let rx = ctx.user_response_rx.as_ref().ok_or("no answer channel")?;

            let guard = rx.lock().await;
            let request = AgentRequest::CreateElicitationRequest(request);
            let id = ask_client(&out_tx, &pending, AskKind::Elicitation, request);
            let response = ctx.cancel.race(guard.recv_async()).await;
            // Cleared while still holding the channel, so a stale id cannot
            // clobber whatever ask comes next.
            let _ = pending.lock().unwrap().asks.remove(&id);
            drop(guard);

            Ok(match response {
                Ok(Ok(raw)) => elicitation::format_response(&input, &raw),
                _ => elicitation::DISMISSED.to_owned(),
            })
        })
    })
}

/// Servers the client injects on `session/new` and `session/load`. A transport we
/// cannot speak is dropped like a broken `mcp.toml` entry: losing one server beats
/// losing the session.
fn injected_servers(servers: &[McpServer]) -> Vec<(String, RawTransport)> {
    servers
        .iter()
        .filter_map(|server| match server {
            McpServer::Http(http) => Some((
                server_name(&http.name),
                RawTransport::Http(RawHttpFields {
                    url: http.url.clone(),
                    headers: pairs(&http.headers, |h| (&h.name, &h.value)),
                    oauth: None,
                }),
            )),
            McpServer::Stdio(stdio) => Some((
                server_name(&stdio.name),
                RawTransport::Stdio(RawStdioFields {
                    command: iter::once(stdio.command.to_string_lossy().into_owned())
                        .chain(stdio.args.iter().cloned())
                        .collect(),
                    environment: pairs(&stdio.env, |e| (&e.name, &e.value)),
                }),
            )),
            _ => {
                warn!("ignoring injected MCP server, only http and stdio are supported");
                None
            }
        })
        .collect()
}

/// Clients name their servers freely, caudra names them like `mcp.toml` does.
fn server_name(name: &str) -> String {
    name.chars()
        .take(MAX_SERVER_NAME_LEN)
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn pairs<T>(items: &[T], split: impl Fn(&T) -> (&String, &String)) -> HashMap<String, String> {
    items
        .iter()
        .map(|item| {
            let (name, value) = split(item);
            (name.clone(), value.clone())
        })
        .collect()
}

/// MCP is per session: the client picks the cwd and may inject its own servers.
/// Returns as soon as the config is read, the first prompt waits for the tools.
async fn start_mcp(cwd: &Path, servers: &[McpServer]) -> Option<McpHandle> {
    let (handle, errors) = mcp::start_with_extra(cwd, injected_servers(servers)).await;
    if !errors.is_empty() {
        warn!(%errors, "MCP config errors");
    }
    if let Some(handle) = &handle {
        let awaiting: Vec<_> = handle
            .reader()
            .load()
            .infos
            .iter()
            .filter(|info| info.status == McpServerStatus::AwaitingTrust)
            .map(|info| info.name.clone())
            .collect();
        if !awaiting.is_empty() {
            warn!(
                servers = %awaiting.join(", "),
                "MCP trust changed after preflight; leaving servers parked"
            );
        }
    }
    handle
}

async fn preflight_mcp(cwd: &Path, servers: &[McpServer]) -> Result<(), AcpError> {
    let (awaiting, errors) = mcp::pending_startup_trust(cwd, injected_servers(servers)).await;
    if !errors.is_empty() {
        warn!(%errors, "MCP config errors");
    }
    if awaiting.is_empty() {
        return Ok(());
    }
    let message = format!(
        "project MCP servers require startup trust: {}. Run `caudra`, review them with `/mcp`, then retry",
        awaiting.join(", ")
    );
    Err(AcpError::invalid_params().data(json_str(&message)))
}

fn acquire_session_lease(id: CaudraId) -> Result<Arc<SessionLease>, AcpError> {
    let storage =
        StateDir::resolve().map_err(|error| AcpError::internal_error().data(json_str(&error)))?;
    SessionLease::acquire(&storage, id)
        .map(Arc::new)
        .map_err(session_lease_error)
}

fn session_lease_error(error: SessionError) -> AcpError {
    match error {
        SessionError::SessionInUse { .. } => {
            AcpError::new(SESSION_IN_USE_ERROR_CODE, error.to_string())
        }
        _ => AcpError::internal_error().data(json_str(&error)),
    }
}

/// Stop the installed session and release its per-session resources.
async fn close_session(srv: &mut Server) {
    let Some(state) = srv.session.take() else {
        return;
    };
    // The event pump dies with the session, so the prompt it owed an answer to
    // has to be answered here or the client waits on it forever.
    if let Some(id) = state.pending.lock().unwrap().prompt.take() {
        let resp = PromptResponse::new(StopReason::Cancelled);
        send(
            &srv.out_tx,
            Response::new(id, Ok(AgentResponse::PromptResponse(resp))),
        );
    }
    state.handle.task.cancel().await;
    if let Some(mcp) = state.mcp {
        mcp.shutdown().await;
    }
}

fn install_session(
    srv: &mut Server,
    handle: InteractiveHandle,
    mcp: Option<McpHandle>,
    current_model: String,
    pending: PendingState,
    cwd: PathBuf,
    initial_cost: Option<f64>,
) {
    start_event_pump(
        handle.event_rx.clone(),
        handle.session_id.clone(),
        srv.out_tx.clone(),
        Arc::clone(&pending),
        cwd,
        caudra_storage::paths::home(),
        initial_cost,
    );
    srv.session = Some(SessionState {
        handle,
        mcp,
        current_mode: AgentMode::Build,
        current_model,
        pending,
    });
}

#[derive(Debug)]
struct Restored {
    history: Vec<HistoryItem>,
    /// Only set when the session recorded an absolute cwd.
    cwd: Option<PathBuf>,
    usage: TokenUsage,
    by_model: HashMap<String, StoredTokenUsage>,
    model: String,
    session_rules: Vec<StoredRule>,
    structured_permission_rules: Vec<PermissionRuleRecord>,
    yolo: Option<bool>,
    system_prompt_profile: Option<String>,
    write_version: Option<i64>,
}

fn load_history(session_id: CaudraId) -> Result<Restored, AcpError> {
    let storage = caudra_storage::StateDir::resolve()
        .map_err(|e| AcpError::internal_error().data(json_str(&e)))?;
    load_history_from(&storage, session_id)
}

/// History plus the absolute cwd the session recorded in its header. Tool
/// inputs from a past run resolve against that cwd, not the client's current
/// one; a non-absolute recording falls back to the caller's cwd.
fn load_history_from(
    storage: &caudra_storage::StateDir,
    session_id: CaudraId,
) -> Result<Restored, AcpError> {
    let session = load_stored_session(session_id, storage).map_err(|e| {
        AcpError::resource_not_found(Some(format!("session/{session_id}"))).data(json_str(&e))
    })?;
    let recorded = if Path::new(&session.cwd).is_absolute() {
        Some(PathBuf::from(&session.cwd))
    } else {
        None
    };
    let head = resolve_history_head(
        session.messages(),
        session.meta.history_head,
        session.meta.pending_revert.is_some(),
    );
    let history = active_history_items(session.messages(), head)
        .map_err(|error| AcpError::internal_error().data(json_str(&error)))?;
    Ok(Restored {
        cwd: recorded,
        usage: session.token_usage,
        by_model: session.usage_by_model().clone(),
        model: session.model.clone(),
        session_rules: session.meta.session_rules.clone(),
        structured_permission_rules: session.meta.structured_permission_rules.clone(),
        yolo: session.meta.yolo,
        system_prompt_profile: session.meta.system_prompt_profile.clone(),
        write_version: session.persisted_write_version(),
        history,
    })
}

fn handle_prompt(srv: &mut Server, raw: &Value, id: &RequestId) -> Result<(), AcpError> {
    let req: PromptRequest = parse_params(raw)?;
    let session = srv.session.as_ref().ok_or_else(no_session)?;

    let (message, images) = extract_prompt_content(&req.prompt);
    let input = AgentInput {
        message,
        mode: session.current_mode.clone(),
        images,
        preamble: Vec::new(),
        thinking: srv.thinking.clone(),
        fast: false,
        workflow: false,
        prompt: None,
    };

    session
        .handle
        .input_tx
        .send(input)
        .map_err(|_| AcpError::new(-32603, "session ended"))?;
    session.pending.lock().unwrap().prompt = Some(id.clone());
    Ok(())
}

fn handle_set_mode(srv: &mut Server, raw: &Value) -> Result<AgentResponse, AcpError> {
    let req: SetSessionModeRequest = parse_params(raw)?;
    let mode_str = req.mode_id.0.to_string();
    let new_mode = methods::mode_id_to_agent_mode(&mode_str)
        .ok_or_else(|| AcpError::new(-32602, format!("unknown mode: {mode_str}")))?;

    let session = srv.session.as_mut().ok_or_else(no_session)?;
    session.current_mode = new_mode;

    let sid = SessionId::from(session.handle.session_id.to_string());
    session_update(
        &srv.out_tx,
        &sid,
        SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(SessionModeId::from(mode_str))),
    );
    Ok(AgentResponse::SetSessionModeResponse(
        SetSessionModeResponse::new(),
    ))
}

fn handle_set_config(srv: &mut Server, raw: &Value) -> Result<AgentResponse, AcpError> {
    let req: SetSessionConfigOptionRequest = parse_params(raw)?;
    if req.config_id.0.as_ref() != methods::MODEL_CONFIG_ID {
        let detail = format!("unknown config option: {}", req.config_id);
        return Err(AcpError::invalid_params().data(json_str(&detail)));
    }

    let spec = req.value.0.to_string();
    if !srv.model_policy.allows(&spec) {
        return Err(AcpError::invalid_params().data(json_str(&"model is not allowed by policy")));
    }
    let model =
        Model::from_spec(&spec).map_err(|e| AcpError::invalid_params().data(json_str(&e)))?;

    let session = srv.session.as_mut().ok_or_else(no_session)?;
    session
        .handle
        .model_tx
        .send(model)
        .map_err(|_| AcpError::new(-32603, "session ended"))?;
    session.current_model = spec.clone();

    Ok(AgentResponse::SetSessionConfigOptionResponse(
        SetSessionConfigOptionResponse::new(vec![methods::model_config_option(
            &spec,
            &srv.model_specs,
        )]),
    ))
}

fn handle_notification(srv: &Server, method: &str) {
    match method {
        "session/cancel" => {
            if let Some(session) = &srv.session {
                // Any answer still in flight belongs to the cancelled turn, so
                // forget its id and let it be dropped on arrival.
                session.pending.lock().unwrap().asks.clear();
                let _ = session.handle.cancel_tx.try_send(());
            }
        }
        _ => debug!(method, "unknown notification"),
    }
}

fn handle_incoming_response(srv: &Server, raw: &Value) {
    let Some(session) = &srv.session else { return };
    let Some(id) = raw.get("id").and_then(Value::as_i64) else {
        return;
    };
    let ask = session.pending.lock().unwrap().asks.remove(&id);
    let Some(kind) = ask else {
        warn!(id, "response for an unknown request id");
        return;
    };
    match kind {
        AskKind::Permission {
            request_id,
            exact_project_deny,
        } => {
            let answer = permission_answer(raw, exact_project_deny);
            if !session.handle.permissions.answer(&request_id, answer)
                && session
                    .handle
                    .permissions
                    .pending_request(&request_id)
                    .is_some()
            {
                warn!(%request_id, "permission response failed; denying request");
                session
                    .handle
                    .permissions
                    .answer(&request_id, PermissionAnswer::Deny);
            }
        }
        AskKind::ResolvedPermission => {}
        // The waiting question tool parses this; an error response decodes to
        // nothing and counts as a dismissal.
        AskKind::Elicitation => {
            let answer = raw
                .get("result")
                .cloned()
                .unwrap_or(Value::Null)
                .to_string();
            let _ = session.handle.answer_tx.send(answer);
        }
    }
}

/// A response we cannot read still has to answer the agent, or the tool waits
/// on a permission that will never come.
fn permission_answer(raw: &Value, exact_project_deny: bool) -> PermissionAnswer {
    match raw
        .get("result")
        .map(|result| serde_json::from_value::<RequestPermissionResponse>(result.clone()))
    {
        Some(Ok(resp)) => permissions::outcome_to_answer(&resp.outcome, exact_project_deny),
        _ => PermissionAnswer::Deny,
    }
}

fn permission_scope_summary(scopes: &[String]) -> String {
    format!(
        "Permission scopes:\n{}",
        serde_json::to_string_pretty(scopes).expect("strings always serialize")
    )
}

fn request_permission(
    out_tx: &Sender<Value>,
    pending: &PendingState,
    sid: &SessionId,
    request: CaudraPermissionRequest,
) {
    let fields = ToolCallUpdateFields::new()
        .title(request.presentation.action.clone())
        .content(vec![ToolCallContent::from(ContentBlock::Text(
            TextContent::new(permission_scope_summary(&request.scopes)),
        ))])
        .raw_input(request.input.clone());
    let client_request = AgentRequest::RequestPermissionRequest(RequestPermissionRequest::new(
        sid.clone(),
        ToolCallUpdate::new(ToolCallId::from(request.id.clone()), fields),
        permissions::permission_options(),
    ));
    let kind = AskKind::Permission {
        request_id: request.id.clone(),
        exact_project_deny: permissions::exact_project_deny_is_representable(&request),
    };
    ask_client(out_tx, pending, kind, client_request);
}

fn resolve_presented_permission(out_tx: &Sender<Value>, pending: &PendingState, request_id: &str) {
    let ids = {
        let mut pending = pending.lock().unwrap();
        let ids: Vec<_> = pending
            .asks
            .iter()
            .filter_map(|(&id, kind)| match kind {
                AskKind::Permission {
                    request_id: candidate,
                    ..
                } if candidate == request_id => Some(id),
                _ => None,
            })
            .collect();
        for id in &ids {
            pending.asks.insert(*id, AskKind::ResolvedPermission);
        }
        ids
    };
    for id in ids {
        send(
            out_tx,
            Notification {
                method: Arc::from("$/cancel_request"),
                params: Some(serde_json::json!({ "id": id })),
            },
        );
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

fn start_event_pump(
    event_rx: Receiver<Envelope>,
    session_id: SessionRef,
    out_tx: Sender<Value>,
    pending: PendingState,
    cwd: PathBuf,
    home: Option<PathBuf>,
    initial_cost: Option<f64>,
) {
    smol::spawn(async move {
        let sid = SessionId::from(session_id.to_string());
        let mut cost_total = initial_cost;

        while let Ok(Envelope {
            event, subagent, ..
        }) = event_rx.recv_async().await
        {
            // Subagent stream events stay out of the transcript, but their
            // turns still spend session money.
            if let AgentEvent::TurnComplete(tc) = &event {
                add_cost(&mut cost_total, tc.cost);
            }
            if subagent.is_some()
                && !matches!(
                    &event,
                    AgentEvent::PermissionRequest(_) | AgentEvent::PermissionRequestResolved { .. }
                )
            {
                continue;
            }

            let update = match event {
                AgentEvent::TextDelta { text } => translate::text_delta(&text),
                AgentEvent::ThinkingDelta { text } => translate::thinking_delta(&text),
                AgentEvent::ToolPending { id, name } => translate::tool_pending(&id, &name),
                AgentEvent::ToolStart(event) => {
                    translate::tool_start(&event, &cwd, home.as_deref())
                }
                AgentEvent::ToolOutput { id, content } => translate::tool_output(&id, &content),
                AgentEvent::ToolDone(event) => translate::tool_done(&event, &cwd, home.as_deref()),
                AgentEvent::TurnComplete(event) => translate::usage_update(&event, cost_total),
                AgentEvent::PermissionRequest(request) => {
                    request_permission(&out_tx, &pending, &sid, *request);
                    continue;
                }
                AgentEvent::PermissionRequestResolved { request_id, .. } => {
                    resolve_presented_permission(&out_tx, &pending, &request_id);
                    continue;
                }
                AgentEvent::Done { reason, .. } => {
                    if let Some(id) = pending.lock().unwrap().prompt.take() {
                        let resp = PromptResponse::new(translate::map_done_reason(reason));
                        send(
                            &out_tx,
                            Response::new(id, Ok(AgentResponse::PromptResponse(resp))),
                        );
                    }
                    continue;
                }
                AgentEvent::Error { message } => {
                    if let Some(id) = pending.lock().unwrap().prompt.take() {
                        let error = AcpError::internal_error().data(Value::String(message));
                        send(&out_tx, Response::<AgentResponse>::new(id, Err(error)));
                    }
                    continue;
                }
                _ => continue,
            };
            session_update(&out_tx, &sid, update);
        }
    })
    .detach();
}

fn send(out_tx: &Sender<Value>, msg: impl Serialize) {
    if let Ok(json) = serde_json::to_value(JsonRpcMessage::wrap(msg)) {
        let _ = out_tx.send(json);
    }
}

fn session_update(out_tx: &Sender<Value>, sid: &SessionId, update: SessionUpdate) {
    let notification =
        AgentNotification::SessionNotification(SessionNotification::new(sid.clone(), update));
    send(
        out_tx,
        Notification {
            method: Arc::from("session/update"),
            params: Some(notification),
        },
    );
}

fn no_session() -> AcpError {
    AcpError::new(-32600, "no active session")
}

fn parse_params<T: serde::de::DeserializeOwned>(raw: &Value) -> Result<T, AcpError> {
    serde_json::from_value(raw.get("params").cloned().unwrap_or(Value::Null))
        .map_err(|e| AcpError::invalid_params().data(json_str(&e)))
}

fn json_str(e: &impl std::fmt::Display) -> Value {
    Value::String(e.to_string())
}

#[cfg(test)]
mod tests {
    use caudra_agent::permissions::{PermissionLifetime, PermissionManager, PermissionRequest};
    use caudra_agent::tools::PermissionScopes;
    use caudra_providers::{ContentBlock as MsgBlock, Role, TokenUsage};
    use caudra_storage::StateDir;
    use caudra_storage::sessions::Session;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    const ANSWERED_ID: i64 = 1001;
    const UNKNOWN_ID: i64 = 1002;
    const CAUDRA_REQUEST_ID: &str = "caudra-permission-1";
    const SECOND_CAUDRA_REQUEST_ID: &str = "caudra-permission-2";
    const DISCOVERED_SPEC: &str = "openrouter/discovered-model";
    const OFFLINE_SPEC: &str = "openai/gpt-5";
    const SELECTED_SPEC: &str = "openai/gpt-5.6-sol";
    /// Neither resolves in the price tables, so nothing can re-price a restored
    /// session back onto the recorded number by luck.
    const RETIRED_SPEC: &str = "retired-vendor/retired-model-9000";
    const RETIRED_MODEL_ID: &str = "retired-model-9000";
    const RECORDED_COST: f64 = 1.25;
    const SESSION_PERMISSION_SCOPE: &str = "cargo *";

    fn history_items(messages: &[Message]) -> Vec<HistoryItem> {
        let mut items = Vec::new();
        for message in messages {
            let parent_id = items.last().map(|item: &HistoryItem| item.id);
            items.extend(expand_message(message, parent_id));
        }
        items
    }

    fn allow_once(id: i64) -> Value {
        selected_permission(id, "allow_once")
    }

    fn allow_exact(lifetime: PermissionLifetime) -> PermissionAnswer {
        PermissionAnswer::AllowOption {
            option_id: "allow_exact".into(),
            lifetime,
        }
    }

    fn selected_permission(id: i64, option_id: &str) -> Value {
        serde_json::json!({
            "id": id,
            "result": { "outcome": { "outcome": "selected", "optionId": option_id } },
        })
    }

    #[test_case(allow_once(ANSWERED_ID), true, allow_exact(PermissionLifetime::Once) ; "selected_option")]
    #[test_case(selected_permission(ANSWERED_ID, "allow_always"), true, allow_exact(PermissionLifetime::Conversation) ; "allow_always_is_exact_conversation")]
    #[test_case(selected_permission(ANSWERED_ID, "reject_always"), true, PermissionAnswer::DenyAlwaysLocal ; "representable_reject_always")]
    #[test_case(selected_permission(ANSWERED_ID, "reject_always"), false, PermissionAnswer::Deny ; "unrepresentable_reject_always_fails_closed_once")]
    #[test_case(serde_json::json!({ "id": ANSWERED_ID, "result": { "outcome": { "outcome": "cancelled" } } }), true, PermissionAnswer::Deny ; "cancelled_outcome")]
    #[test_case(serde_json::json!({ "id": ANSWERED_ID, "result": { "nonsense": true } }), true, PermissionAnswer::Deny ; "unparsable_result")]
    #[test_case(serde_json::json!({ "id": ANSWERED_ID, "error": { "code": -32603 } }), true, PermissionAnswer::Deny ; "jsonrpc_error")]
    fn permission_answer_maps_response(
        raw: Value,
        exact_project_deny: bool,
        expected: PermissionAnswer,
    ) {
        assert_eq!(permission_answer(&raw, exact_project_deny), expected);
    }

    #[test]
    fn session_in_use_has_a_distinct_server_error() {
        let id = CaudraId::generate();

        let error = session_lease_error(SessionError::SessionInUse { id });

        assert_eq!(
            error.code,
            agent_client_protocol_schema::ErrorCode::Other(SESSION_IN_USE_ERROR_CODE)
        );
        assert!(error.message.contains(&id.to_string()));
    }

    fn permission_manager() -> Arc<PermissionManager> {
        Arc::new(PermissionManager::new_nonpersistent(
            caudra_config::PermissionsConfig::default(),
            PathBuf::from("/project"),
            Arc::default(),
        ))
    }

    fn server_with_asks(
        permissions: Arc<PermissionManager>,
        asks: HashMap<i64, AskKind>,
    ) -> (Server, Receiver<String>, Receiver<Value>) {
        let (answer_tx, answer_rx) = flume::unbounded();
        let (out_tx, out_rx) = flume::unbounded();
        let session_id = SessionRef::from(CaudraId::generate());
        static LEASE_DIR: std::sync::OnceLock<TempDir> = std::sync::OnceLock::new();
        let lease_dir = LEASE_DIR.get_or_init(|| TempDir::new().unwrap());
        let session_lease = Arc::new(
            SessionLease::acquire(
                &StateDir::from_path(lease_dir.path().to_path_buf()),
                session_id.id(),
            )
            .unwrap(),
        );
        let handle = InteractiveHandle {
            event_rx: flume::unbounded().1,
            tool_names: Vec::new(),
            input_tx: flume::unbounded().0,
            answer_tx,
            cancel_tx: flume::unbounded().0,
            model_tx: flume::unbounded().0,
            session_id,
            session_lease,
            permissions,
            task: smol::spawn(async {}),
        };
        let server = Server {
            out_tx,
            model_specs: Vec::new(),
            model_policy: Arc::new(ModelPolicy::default()),
            thinking: Default::default(),
            client_elicits_form: false,
            session: Some(SessionState {
                handle,
                mcp: None,
                current_mode: AgentMode::Build,
                current_model: String::new(),
                pending: Arc::new(Mutex::new(Pending { prompt: None, asks })),
            }),
        };
        (server, answer_rx, out_rx)
    }

    fn server_with_ask(kind: AskKind) -> (Server, Receiver<String>, Receiver<Value>) {
        server_with_asks(permission_manager(), HashMap::from([(ANSWERED_ID, kind)]))
    }

    fn pending_permission(
        manager: Arc<PermissionManager>,
        request_id: &str,
        scope: &str,
    ) -> (smol::Task<bool>, Receiver<Envelope>) {
        let (event_tx, event_rx) = flume::unbounded();
        let event_tx = caudra_agent::EventSender::new(event_tx, 0);
        let request_id = request_id.to_owned();
        let scope = scope.to_owned();
        let task = smol::spawn(async move {
            let (_legacy_tx, legacy_rx) = flume::unbounded();
            let legacy_rx = smol::lock::Mutex::new(legacy_rx);
            manager
                .enforce(
                    &caudra_config::ToolKey::native("bash"),
                    &PermissionScopes::single(scope.clone()),
                    &serde_json::json!({"command": scope}),
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
                &caudra_config::ToolKey::native("bash"),
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

    #[test]
    fn only_the_outstanding_request_id_is_answered() {
        smol::block_on(async {
            let manager = permission_manager();
            let (task, event_rx) =
                pending_permission(Arc::clone(&manager), CAUDRA_REQUEST_ID, "cargo test");
            let _ = event_rx.recv_async().await.unwrap();
            let kind = AskKind::Permission {
                request_id: CAUDRA_REQUEST_ID.into(),
                exact_project_deny: true,
            };
            let (srv, answer_rx, ..) =
                server_with_asks(Arc::clone(&manager), HashMap::from([(ANSWERED_ID, kind)]));

            handle_incoming_response(&srv, &allow_once(UNKNOWN_ID));
            assert_eq!(manager.pending_count(), 1, "an unknown id is dropped");

            handle_incoming_response(&srv, &allow_once(ANSWERED_ID));
            assert!(task.await);
            assert!(answer_rx.is_empty(), "permission bypasses answer_tx");

            handle_incoming_response(&srv, &allow_once(ANSWERED_ID));
            assert!(answer_rx.is_empty(), "a replayed answer is dropped");
        });
    }

    #[test]
    fn concurrent_permission_requests_are_correlated_out_of_order() {
        smol::block_on(async {
            let manager = permission_manager();
            let (first, first_events) =
                pending_permission(Arc::clone(&manager), CAUDRA_REQUEST_ID, "cargo test");
            let (second, second_events) = pending_permission(
                Arc::clone(&manager),
                SECOND_CAUDRA_REQUEST_ID,
                "cargo check",
            );
            let _ = first_events.recv_async().await.unwrap();
            let _ = second_events.recv_async().await.unwrap();
            let asks = HashMap::from([
                (
                    ANSWERED_ID,
                    AskKind::Permission {
                        request_id: CAUDRA_REQUEST_ID.into(),
                        exact_project_deny: true,
                    },
                ),
                (
                    UNKNOWN_ID,
                    AskKind::Permission {
                        request_id: SECOND_CAUDRA_REQUEST_ID.into(),
                        exact_project_deny: true,
                    },
                ),
            ]);
            let (srv, answer_rx, ..) = server_with_asks(Arc::clone(&manager), asks);

            handle_incoming_response(&srv, &allow_once(UNKNOWN_ID));
            handle_incoming_response(&srv, &selected_permission(ANSWERED_ID, "reject_once"));

            assert!(!first.await);
            assert!(second.await);
            assert!(answer_rx.is_empty());
        });
    }

    #[test]
    fn reusable_approval_cancels_a_covered_permission_request() {
        smol::block_on(async {
            let manager = permission_manager();
            let (first, first_events) =
                pending_permission(Arc::clone(&manager), CAUDRA_REQUEST_ID, "cargo test");
            let (second, second_events) =
                pending_permission(Arc::clone(&manager), SECOND_CAUDRA_REQUEST_ID, "cargo test");
            first_events.recv_async().await.unwrap();
            second_events.recv_async().await.unwrap();
            let asks = HashMap::from([
                (
                    ANSWERED_ID,
                    AskKind::Permission {
                        request_id: CAUDRA_REQUEST_ID.into(),
                        exact_project_deny: true,
                    },
                ),
                (
                    UNKNOWN_ID,
                    AskKind::Permission {
                        request_id: SECOND_CAUDRA_REQUEST_ID.into(),
                        exact_project_deny: true,
                    },
                ),
            ]);
            let (srv, answer_rx, out_rx) = server_with_asks(Arc::clone(&manager), asks);

            handle_incoming_response(&srv, &selected_permission(ANSWERED_ID, "allow_always"));
            let resolution = second_events.recv_async().await.unwrap().event;
            let AgentEvent::PermissionRequestResolved { request_id, .. } = resolution else {
                panic!("expected permission resolution, got {resolution:?}");
            };
            resolve_presented_permission(
                &srv.out_tx,
                &srv.session.as_ref().unwrap().pending,
                &request_id,
            );

            assert!(first.await);
            assert!(second.await);
            assert!(answer_rx.is_empty());
            let cancelled = out_rx.recv_async().await.unwrap();
            assert_eq!(cancelled["method"], "$/cancel_request");
            assert_eq!(cancelled["params"]["id"], UNKNOWN_ID);
            handle_incoming_response(&srv, &allow_once(UNKNOWN_ID));
            assert!(answer_rx.is_empty());
        });
    }

    #[test]
    fn acp_allow_always_reuses_only_the_exact_conversation_scope() {
        smol::block_on(async {
            let manager = permission_manager();
            let (task, event_rx) =
                pending_permission(Arc::clone(&manager), CAUDRA_REQUEST_ID, "cargo test");
            let _ = event_rx.recv_async().await.unwrap();
            let kind = AskKind::Permission {
                request_id: CAUDRA_REQUEST_ID.into(),
                exact_project_deny: true,
            };
            let (srv, ..) =
                server_with_asks(Arc::clone(&manager), HashMap::from([(ANSWERED_ID, kind)]));

            handle_incoming_response(&srv, &selected_permission(ANSWERED_ID, "allow_always"));

            assert!(task.await);
            assert!(enforcement_without_answer(&manager, "cargo test").await);
            assert!(!enforcement_without_answer(&manager, "cargo publish").await);
        });
    }

    #[test]
    fn cancel_drops_the_outstanding_permission_request() {
        let kind = AskKind::Permission {
            request_id: CAUDRA_REQUEST_ID.into(),
            exact_project_deny: true,
        };
        let (srv, answer_rx, ..) = server_with_ask(kind);
        handle_notification(&srv, "session/cancel");

        handle_incoming_response(&srv, &allow_once(ANSWERED_ID));
        assert!(answer_rx.is_empty(), "the cancelled turn owns that answer");
    }

    #[test]
    fn elicitation_response_forwards_the_raw_result() {
        let (srv, answer_rx, ..) = server_with_ask(AskKind::Elicitation);
        let raw = serde_json::json!({
            "id": ANSWERED_ID,
            "result": { "action": "accept", "content": { "q1": "axum" } },
        });

        handle_incoming_response(&srv, &raw);
        let forwarded = answer_rx.try_recv().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&forwarded).unwrap(),
            raw["result"]
        );
    }

    #[test]
    fn discovered_models_are_pushed_to_the_client() {
        let (mut srv, .., out_rx) = server_with_ask(AskKind::Elicitation);
        srv.model_specs = vec![OFFLINE_SPEC.to_owned()];
        let batch = vec![DISCOVERED_SPEC.to_owned()];

        refresh_models(&mut srv, batch.clone());
        let update = out_rx.try_recv().expect("the fuller list is announced");
        let option = &update["params"]["update"]["configOptions"][0];
        assert_eq!(option["id"], methods::MODEL_CONFIG_ID);
        let selectable: Vec<&str> = option["options"]
            .as_array()
            .expect("the option is a select")
            .iter()
            .filter_map(|o| o["value"].as_str())
            .collect();
        assert!(
            selectable.contains(&OFFLINE_SPEC) && selectable.contains(&DISCOVERED_SPEC),
            "a batch is merged into the offline list, not swapped for it: {selectable:?}"
        );

        refresh_models(&mut srv, batch);
        assert!(out_rx.is_empty(), "a batch adding nothing is not announced");
    }

    #[test]
    fn subagent_permission_is_forwarded_with_full_structured_details() {
        smol::block_on(async {
            let (event_tx, event_rx) = flume::unbounded();
            let (out_tx, out_rx) = flume::unbounded();
            let pending = PendingState::default();
            let session_id = SessionRef::generate();
            let scope = format!("line one\n{}", "x".repeat(512));
            let input = serde_json::json!({
                "command": scope,
                "nested": {"complete": true}
            });
            let request = PermissionRequest::from_legacy(
                CAUDRA_REQUEST_ID.into(),
                caudra_config::ToolKey::native("bash"),
                vec![scope.clone()],
                input.clone(),
                Path::new("/project"),
                false,
            );
            let action = request.presentation.action.clone();
            start_event_pump(
                event_rx,
                session_id,
                out_tx,
                Arc::clone(&pending),
                PathBuf::from("/project"),
                None,
                None,
            );
            event_tx
                .send(Envelope {
                    event: AgentEvent::PermissionRequest(Box::new(request)),
                    subagent: Some(caudra_agent::SubagentInfo {
                        parent_tool_use_id: "task-call".into(),
                        task_id: "task-1".into(),
                        name: "worker".into(),
                        prompt: None,
                        model: None,
                        answer_tx: None,
                        steer_tx: None,
                    }),
                    run_id: 0,
                })
                .unwrap();

            let message = out_rx.recv_async().await.unwrap();
            assert_eq!(message["method"], "session/request_permission");
            assert_eq!(message["params"]["toolCall"]["title"], action);
            assert_eq!(message["params"]["toolCall"]["rawInput"], input);
            assert_eq!(
                message["params"]["toolCall"]["content"][0]["content"]["text"],
                permission_scope_summary(&[scope])
            );
            assert!(matches!(
                pending.lock().unwrap().asks.values().next(),
                Some(AskKind::Permission { request_id, .. }) if request_id == CAUDRA_REQUEST_ID
            ));
        });
    }

    #[test]
    fn load_history_round_trips_stored_items() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let messages = vec![
            Message::user("rename foo to bar".into()),
            Message {
                role: Role::Assistant,
                content: vec![MsgBlock::Text {
                    text: "done".into(),
                }],
                display_text: None,
                ..Default::default()
            },
        ];
        let items = history_items(&messages);
        let mut session: Session<HistoryItem, TokenUsage, ToolOutput> =
            Session::new("anthropic/test-model", "/project");
        session.replace_messages(items.clone());
        session.token_usage = TokenUsage {
            input: 1_000,
            output: 200,
            ..Default::default()
        };
        session.meta.session_rules = vec![StoredRule {
            tool: "bash".into(),
            scope: Some(SESSION_PERMISSION_SCOPE.into()),
            effect: caudra_storage::sessions::StoredEffect::Allow,
        }];
        let request = PermissionRequest::from_legacy(
            "stored-structured".into(),
            caudra_config::ToolKey::native("bash"),
            vec!["cargo test".into()],
            serde_json::json!({"command": "cargo test"}),
            Path::new("/project"),
            false,
        );
        session.meta.structured_permission_rules = vec![
            PermissionRuleRecord::conversation(
                request
                    .option_rule(
                        "allow_exact",
                        caudra_agent::permissions::PermissionLifetime::Conversation,
                    )
                    .unwrap(),
            )
            .unwrap(),
        ];
        session.meta.yolo = Some(true);
        session.meta.system_prompt_profile = Some("review".into());
        session.save(&dir).unwrap();

        let id: CaudraId = session.id;
        let restored = load_history_from(&dir, id).unwrap();
        assert_eq!(restored.model, "anthropic/test-model");
        assert_eq!(restored.history, items);
        assert_eq!(restored.cwd, Some(PathBuf::from("/project")));
        assert_eq!(restored.usage, session.token_usage);
        assert_eq!(restored.session_rules, session.meta.session_rules);
        assert_eq!(
            restored.structured_permission_rules,
            session.meta.structured_permission_rules
        );
        assert_eq!(restored.yolo, Some(true));
        assert_eq!(restored.system_prompt_profile.as_deref(), Some("review"));
    }

    #[test]
    fn load_history_returns_only_the_persisted_active_path() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let root = history_items(&[Message::user("root".into())])[0].clone();
        let abandoned =
            expand_message(&Message::user("abandoned".into()), Some(root.id))[0].clone();
        let branch = expand_message(&Message::user("branch".into()), Some(root.id))[0].clone();
        let mut session: Session<HistoryItem, TokenUsage, ToolOutput> =
            Session::new("anthropic/test-model", "/project");
        session.replace_messages(vec![root.clone(), abandoned, branch.clone()]);
        session.meta.history_head = Some(branch.id);
        session.save(&dir).unwrap();

        let restored = load_history_from(&dir, session.id).unwrap();

        assert_eq!(restored.history, [root, branch]);
    }

    #[test]
    fn load_history_expands_legacy_messages_into_one_item_chain() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let messages = vec![
            Message::user("rename foo to bar".into()),
            Message {
                role: Role::Assistant,
                content: vec![MsgBlock::Text {
                    text: "done".into(),
                }],
                ..Default::default()
            },
        ];
        let mut session: Session<Message, TokenUsage, ToolOutput> =
            Session::new("anthropic/test-model", "/project");
        session.replace_messages(messages.clone());
        session.save(&dir).unwrap();

        let restored = load_history_from(&dir, session.id).unwrap();

        assert!(
            restored
                .history
                .windows(2)
                .all(|pair| pair[1].parent_id == Some(pair[0].id))
        );
        assert_eq!(
            serde_json::to_value(History::restored(restored.history).unwrap().into_vec()).unwrap(),
            serde_json::to_value(messages).unwrap()
        );
        assert!(Session::<HistoryItem, TokenUsage, ToolOutput>::load(session.id, &dir).is_ok());
    }

    /// Resuming must bill what the session actually paid. If `by_model` came
    /// back empty or lost its recorded costs, ACP would re-price the restored
    /// total against today's table and disagree with the TUI.
    #[test]
    fn load_history_prices_a_resumed_session_at_what_it_paid() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let mut session: Session<HistoryItem, TokenUsage, ToolOutput> =
            Session::new(RETIRED_SPEC, "/project");
        session.token_usage = TokenUsage {
            input: 1_000_000,
            output: 200_000,
            ..Default::default()
        };
        session.add_model_usage(
            RETIRED_MODEL_ID,
            StoredTokenUsage {
                input: 1_000_000,
                output: 200_000,
                cost: Some(RECORDED_COST),
                ..Default::default()
            },
        );
        session.save(&dir).unwrap();

        let mut restored = load_history_from(&dir, session.id).unwrap();
        assert_eq!(
            restored.by_model[RETIRED_MODEL_ID].cost,
            Some(RECORDED_COST),
            "the per-model breakdown survives the file"
        );

        // Mirrors `load_session`: the recorded spec no longer parses, so the
        // selected model stands in, and that must not change the bill.
        let recorded_model = Model::from_spec(&restored.model)
            .unwrap_or_else(|_| Model::from_spec(SELECTED_SPEC).expect("a shipped model"));
        assert_eq!(
            settle_session(
                &restored.usage,
                &mut restored.by_model,
                &recorded_model,
                RESTORED_FAST
            ),
            Some(RECORDED_COST)
        );
    }

    #[test]
    fn load_history_records_absolute_cwd_only() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let mut session: Session<HistoryItem, TokenUsage, ToolOutput> =
            Session::new("anthropic/test-model", "relative/project");
        session.save(&dir).unwrap();
        assert_eq!(load_history_from(&dir, session.id).unwrap().cwd, None);
    }

    #[test]
    fn load_missing_session_is_resource_not_found() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let err = load_history_from(&dir, CaudraId::generate()).unwrap_err();
        assert_eq!(err.code, AcpError::resource_not_found(None).code);
    }

    #[test]
    fn converts_injected_mcp_servers() {
        let raw = serde_json::json!({
            "params": {
                "sessionId": CaudraId::generate().to_string(),
                "cwd": "/project",
                "mcpServers": [
                    {
                        "type": "http",
                        "name": "kan.dev/mcp",
                        "url": "http://127.0.0.1:41012",
                        "headers": [{ "name": "Authorization", "value": "Bearer abc" }]
                    },
                    {
                        "name": "local",
                        "command": "/usr/bin/mcp",
                        "args": ["--stdio"],
                        "env": [{ "name": "TOKEN", "value": "t" }]
                    },
                    {
                        "type": "sse",
                        "name": "legacy",
                        "url": "http://127.0.0.1:41013",
                        "headers": []
                    }
                ]
            }
        });

        let req: LoadSessionRequest = parse_params(&raw).unwrap();
        let servers = injected_servers(&req.mcp_servers);
        assert_eq!(servers.len(), 2, "sse is dropped, not converted");

        let (name, RawTransport::Http(http)) = &servers[0] else {
            panic!("expected http transport");
        };
        assert_eq!(name, "kan-dev-mcp", "wire names are coerced to valid ones");
        assert_eq!(http.url, "http://127.0.0.1:41012");
        assert_eq!(
            http.headers.get("Authorization").map(String::as_str),
            Some("Bearer abc")
        );

        let (name, RawTransport::Stdio(stdio)) = &servers[1] else {
            panic!("expected stdio transport");
        };
        assert_eq!(name, "local");
        assert_eq!(stdio.command, ["/usr/bin/mcp", "--stdio"]);
        assert_eq!(
            stdio.environment.get("TOKEN").map(String::as_str),
            Some("t")
        );
    }
}
