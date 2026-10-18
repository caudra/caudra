use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_client_protocol_schema::v1::{
    AgentResponse, CancelRequestNotification, CloseSessionRequest, DeleteSessionRequest,
    Error as AcpError, ExtResponse, ForkSessionRequest, InitializeRequest, ListSessionsRequest,
    LoadSessionRequest, NewSessionRequest, PromptRequest, RequestId, ResumeSessionRequest,
    SessionConfigOptionValue, SessionId, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, SetSessionModeRequest, SetSessionModeResponse,
};
use caudra_config::ModelPolicy;
use caudra_providers::model::Model;
use caudra_storage::background::JobKind;
use caudra_storage::id::SessionRef;
use serde::{Deserialize, Serialize};
use serde_json::value::to_raw_value;
use serde_json::{Map, Value};
use tracing::{debug, warn};

use crate::asks::{ClientProfile, settle};
use crate::auth::{AUTHENTICATE, auth_method};
use crate::catalog::MODELS_LIST;
use crate::child_work::{SUBAGENT_CANCEL, TASK_KILL, UNKNOWN_TASK, split_child_session};
use crate::errors::{
    INITIALIZE_FIRST, SHUTTING_DOWN, internal, invalid_params, invalid_request, invalid_target,
    params, parse_params, request_cancelled, unknown_session,
};
use crate::lifecycle::{
    CloseContext, Closing, FailedRuntime, Reopen, StartContext, StartRequest, load_response,
    resume_response, spawn_close, spawn_start,
};
use crate::listing::list_sessions;
use crate::outbound::{CANCEL_REQUEST_METHOD, Outbound};
use crate::restore::replay_stored;
use crate::server::{Server, SessionSlot};
use crate::session::SessionState;
use crate::{locked, methods};

const INITIALIZE: &str = "initialize";
const SESSION_NEW: &str = "session/new";
const SESSION_LOAD: &str = "session/load";
const SESSION_RESUME: &str = "session/resume";
const SESSION_FORK: &str = "session/fork";
const SESSION_LIST: &str = "session/list";
const SESSION_CLOSE: &str = "session/close";
const SESSION_DELETE: &str = "session/delete";
const SESSION_PROMPT: &str = "session/prompt";
const SESSION_SET_MODE: &str = "session/set_mode";
const SESSION_SET_CONFIG_OPTION: &str = "session/set_config_option";
const SESSION_CANCEL: &str = "session/cancel";
const SESSION_METHOD_PREFIX: &str = "session/";
const VALUE_ID_EXPECTED: &str = "config option expects a value id";
const BOOLEAN_EXPECTED: &str = "config option expects a boolean value";
const UNKNOWN_CONFIG_OPTION: &str = "this session offers no config option";
const UNKNOWN_EFFORT: &str = "the session's model offers no effort level";
const UNKNOWN_PERMISSION_MODE: &str = "unknown permission mode";
const MODEL_NOT_ALLOWED: &str = "model is not allowed by policy";
const RELATIVE_DIRECTORY: &str = "additionalDirectories entries must be absolute paths";

/// The one field every session method shares, read before the rest so the
/// session can be found, or waited for, first.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionScoped {
    session_id: SessionId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubagentTarget {
    session_id: SessionId,
    subagent_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TaskTarget {
    session_id: SessionId,
    task_id: String,
}

/// Where a request naming a stored session goes.
enum Target<'a> {
    /// Answered once the session settles.
    Queued,
    Active(&'a mut SessionState),
    Stored(SessionRef),
}

impl Server {
    pub(crate) fn handle_message(&mut self, raw: Value) {
        let id = raw.get("id").map(request_id);
        if raw.get("result").is_some() || raw.get("error").is_some() {
            self.handle_response(&raw);
            return;
        }
        let Some(method) = raw.get("method").and_then(Value::as_str).map(str::to_owned) else {
            if let Some(id) = id {
                self.outbound.respond(id, Err(AcpError::invalid_request()));
            }
            return;
        };
        match id {
            Some(id) => self.handle_request(&method, id, raw),
            None => self.handle_notification(&method, raw),
        }
    }

    fn handle_request(&mut self, method: &str, id: RequestId, raw: Value) {
        if method == INITIALIZE {
            let result = self.initialize(&raw);
            return self.outbound.respond(id, result);
        }
        if self.client.is_none() && method.starts_with(SESSION_METHOD_PREFIX) {
            return self
                .outbound
                .respond(id, Err(invalid_request(INITIALIZE_FIRST)));
        }
        match method {
            SESSION_NEW => self.new_session(id, &raw),
            SESSION_LOAD => self.reopen_session(id, raw, Reopen::Load),
            SESSION_RESUME => self.reopen_session(id, raw, Reopen::Resume),
            SESSION_FORK => self.fork_session(id, raw),
            SESSION_LIST => {
                let result = parse_params::<ListSessionsRequest>(&raw)
                    .and_then(|request| list_sessions(&self.storage, &request))
                    .map(AgentResponse::ListSessionsResponse);
                self.outbound.respond(id, result);
            }
            SESSION_CLOSE => self.close_session(id, raw, Closing::Close),
            SESSION_DELETE => self.close_session(id, raw, Closing::Delete),
            SESSION_PROMPT | SESSION_SET_MODE | SESSION_SET_CONFIG_OPTION => {
                self.session_request(method, id, raw)
            }
            SUBAGENT_CANCEL => {
                let result = parse_params::<SubagentTarget>(&raw).and_then(|target| {
                    self.active(&target.session_id)?
                        .cancel_subagent(&target.subagent_id)?;
                    acknowledged()
                });
                self.outbound.respond(id, result);
            }
            TASK_KILL => self.kill_task(id, &raw),
            AUTHENTICATE => self.authenticate(id, &raw),
            MODELS_LIST => {
                let result = ext_response(&self.model_catalog());
                self.outbound.respond(id, result);
            }
            _ => self.outbound.respond(id, Err(AcpError::method_not_found())),
        }
    }

    fn initialize(&mut self, raw: &Value) -> Result<AgentResponse, AcpError> {
        let request: InitializeRequest = parse_params(raw)?;
        let client = ClientProfile::from_initialize(&request);
        let catalog = serde_json::to_value(self.model_catalog()).map_err(|e| internal(&e))?;
        let response = methods::initialize_response(auth_method(client.terminal_auth), catalog);
        self.client = Some(client);
        Ok(AgentResponse::InitializeResponse(response))
    }

    fn new_session(&mut self, id: RequestId, raw: &Value) {
        let session = SessionRef::generate();
        let checked = parse_params::<NewSessionRequest>(raw).and_then(|request| {
            check_additional_directories(
                session.as_str(),
                &request.cwd,
                &request.additional_directories,
            )?;
            Ok(request)
        });
        match checked {
            Ok(request) => self.start(id, session, StartRequest::New(request)),
            Err(error) => self.outbound.respond(id, Err(error)),
        }
    }

    /// Loading a session this process already runs replays it again rather
    /// than starting a second agent on it; resuming one just reports it.
    fn reopen_session(&mut self, id: RequestId, raw: Value, how: Reopen) {
        let parsed = match how {
            Reopen::Load => parse_params::<LoadSessionRequest>(&raw).map(|request| {
                (
                    request.session_id,
                    request.cwd,
                    request.additional_directories,
                    request.mcp_servers,
                )
            }),
            Reopen::Resume => parse_params::<ResumeSessionRequest>(&raw).map(|request| {
                (
                    request.session_id,
                    request.cwd,
                    request.additional_directories,
                    request.mcp_servers,
                )
            }),
        };
        let checked = parsed.and_then(|(sid, cwd, directories, mcp_servers)| {
            check_additional_directories(&sid.0, &cwd, &directories)?;
            Ok((sid, cwd, mcp_servers))
        });
        let (sid, cwd, mcp_servers) = match checked {
            Ok(checked) => checked,
            Err(error) => return self.outbound.respond(id, Err(error)),
        };
        let result = match target(&mut self.sessions, &self.failed, &sid, raw) {
            Ok(Target::Queued) => return,
            Ok(Target::Active(state)) => {
                let result = match how {
                    Reopen::Load => replay_active(state, &self.outbound, &self.model_specs, &cwd),
                    Reopen::Resume => Ok(resume_response(state, &self.model_specs)),
                };
                let reopened = result.is_ok();
                self.outbound.respond(id, result);
                if reopened {
                    state.reannounce_commands(&self.outbound);
                }
                return;
            }
            Ok(Target::Stored(session)) => {
                let request = StartRequest::Reopen {
                    how,
                    cwd,
                    mcp_servers,
                };
                return self.start(id, session, request);
            }
            Err(error) => Err(error),
        };
        self.outbound.respond(id, result);
    }

    /// A fork of a session this process runs carries its live permission
    /// mode; a stored one carries the mode it saved.
    fn fork_session(&mut self, id: RequestId, raw: Value) {
        let request: ForkSessionRequest = match parse_params(&raw) {
            Ok(request) => request,
            Err(error) => return self.outbound.respond(id, Err(error)),
        };
        let fork = SessionRef::generate();
        if let Err(error) = check_additional_directories(
            fork.as_str(),
            &request.cwd,
            &request.additional_directories,
        ) {
            return self.outbound.respond(id, Err(error));
        }
        let (source, permission_mode) =
            match target(&mut self.sessions, &self.failed, &request.session_id, raw) {
                Ok(Target::Queued) => return,
                Ok(Target::Active(state)) => (
                    state.handle.session_id.clone(),
                    Some(state.handle.permissions.mode()),
                ),
                Ok(Target::Stored(source)) => (source, None),
                Err(error) => return self.outbound.respond(id, Err(error)),
            };
        let start = StartRequest::Fork {
            source,
            request,
            permission_mode,
        };
        self.start(id, fork, start);
    }

    /// Closing cancels any turn and answers its prompt before the response;
    /// deleting closes first, then removes the stored session.
    fn close_session(&mut self, id: RequestId, raw: Value, closing: Closing) {
        let sid = match closing {
            Closing::Close => parse_params::<CloseSessionRequest>(&raw).map(|r| r.session_id),
            Closing::Delete => parse_params::<DeleteSessionRequest>(&raw).map(|r| r.session_id),
        };
        let sid = match sid {
            Ok(sid) => sid,
            Err(error) => return self.outbound.respond(id, Err(error)),
        };
        let Some(events) = self.events.upgrade() else {
            return self.outbound.respond(id, Err(internal(SHUTTING_DOWN)));
        };
        let session = match target(&mut self.sessions, &self.failed, &sid, raw) {
            Ok(Target::Queued) => return,
            Ok(Target::Active(state)) => state.handle.session_id.clone(),
            Ok(Target::Stored(session)) if matches!(closing, Closing::Delete) => session,
            Ok(Target::Stored(_)) => return self.outbound.respond(id, Err(unknown_session(&sid))),
            Err(error) => return self.outbound.respond(id, Err(error)),
        };
        let closing_slot = SessionSlot::Closing(Vec::new());
        let state = match self
            .sessions
            .insert(SessionId::from(session.to_string()), closing_slot)
        {
            Some(SessionSlot::Active(state)) => Some(*state),
            _ => None,
        };
        let ctx = CloseContext {
            outbound: self.outbound.clone(),
            storage: self.storage.clone(),
            events,
        };
        spawn_close(ctx, id, session, state, closing);
    }

    fn start(&mut self, id: RequestId, session: SessionRef, request: StartRequest) {
        let Some(ctx) = self.start_context() else {
            return self.outbound.respond(id, Err(internal(SHUTTING_DOWN)));
        };
        self.sessions.insert(
            SessionId::from(session.to_string()),
            SessionSlot::Starting(Vec::new()),
        );
        spawn_start(ctx, id, session, request);
    }

    fn start_context(&self) -> Option<StartContext> {
        Some(StartContext {
            params: Arc::clone(&self.params),
            client: self.client.clone()?,
            outbound: self.outbound.clone(),
            storage: self.storage.clone(),
            model_specs: self.model_specs.clone(),
            default_model: self.default_model.clone(),
            events: self.events.upgrade()?,
        })
    }

    fn session_request(&mut self, method: &str, id: RequestId, raw: Value) {
        let sid = match session_scope(&raw) {
            Ok(sid) => sid,
            Err(error) => return self.outbound.respond(id, Err(error)),
        };
        let state = match self.sessions.get_mut(&sid) {
            Some(SessionSlot::Active(state)) => state,
            Some(SessionSlot::Starting(queued) | SessionSlot::Closing(queued)) => {
                return queued.push(raw);
            }
            None => {
                let error = self.missing_session(&sid);
                return self.outbound.respond(id, Err(error));
            }
        };
        let result = match method {
            SESSION_PROMPT => {
                let prompted = parse_params::<PromptRequest>(&raw)
                    .and_then(|request| state.prompt(id.clone(), &request));
                match prompted {
                    Ok(()) => return,
                    Err(error) => Err(error),
                }
            }
            SESSION_SET_MODE => set_mode(state, &self.outbound, &self.model_specs, &raw),
            _ => {
                let settings = ConfigContext {
                    outbound: &self.outbound,
                    policy: &self.params.model_policy,
                    model_specs: &self.model_specs,
                };
                set_config(state, &settings, &raw)
            }
        };
        self.outbound.respond(id, result);
    }

    fn active(&self, sid: &SessionId) -> Result<&SessionState, AcpError> {
        match self.sessions.get(sid) {
            Some(SessionSlot::Active(state)) => Ok(state),
            _ => Err(self.missing_session(sid)),
        }
    }

    /// Stops a running background shell; the answer waits until the task
    /// runtime recorded the stop.
    fn kill_task(&self, id: RequestId, raw: &Value) {
        let target = match parse_params::<TaskTarget>(raw) {
            Ok(target) => target,
            Err(error) => return self.outbound.respond(id, Err(error)),
        };
        let background = match self.active(&target.session_id) {
            Ok(state) => state.handle.background.clone(),
            Err(error) => return self.outbound.respond(id, Err(error)),
        };
        let Some(background) = background.filter(|background| {
            background
                .resident_status(&target.task_id)
                .is_some_and(|card| card.kind == JobKind::Shell && card.active())
        }) else {
            let error = invalid_target(&format!("{UNKNOWN_TASK}: {}", target.task_id));
            return self.outbound.respond(id, Err(error));
        };
        let outbound = self.outbound.clone();
        smol::spawn(async move {
            let result = match background.cancel(&target.task_id).await {
                Ok(_) => acknowledged(),
                Err(error) => Err(internal(&error)),
            };
            outbound.respond(id, result);
        })
        .detach();
    }

    fn missing_session(&self, sid: &SessionId) -> AcpError {
        match self.failed.get(sid) {
            Some(failure) => failure.refusal(),
            None => unknown_session(sid),
        }
    }

    fn handle_notification(&mut self, method: &str, raw: Value) {
        match method {
            SESSION_CANCEL => self.cancel_session(raw),
            CANCEL_REQUEST_METHOD => self.cancel_request(&raw),
            _ => debug!(method, "unknown notification"),
        }
    }

    fn cancel_session(&mut self, raw: Value) {
        let Ok(sid) = session_scope(&raw) else {
            debug!("session/cancel without a session id");
            return;
        };
        match self.sessions.get_mut(&sid) {
            Some(SessionSlot::Active(state)) => state.cancel(&self.outbound),
            Some(SessionSlot::Starting(queued)) => queued.push(raw),
            Some(SessionSlot::Closing(_)) => debug!(session = %sid, "session/cancel while closing"),
            None => self.cancel_child_session(&sid),
        }
    }

    /// A standard-tier client cancels a subagent by its child session id.
    fn cancel_child_session(&self, child: &SessionId) {
        let Some((parent, subagent)) = split_child_session(child) else {
            return debug!(session = %child, "session/cancel for an unknown session");
        };
        match self.active(&parent) {
            Ok(state) => {
                if let Err(error) = state.cancel_subagent(subagent) {
                    debug!(session = %child, error = %error.message, "session/cancel names no running subagent");
                }
            }
            Err(_) => debug!(session = %child, "session/cancel for an unknown session"),
        }
    }

    /// A prompt a session owes is cancelled as `session/cancel` cancels its
    /// session. A request still queued behind a starting or closing session
    /// is dropped and answered cancelled; one already running finishes.
    fn cancel_request(&mut self, raw: &Value) {
        let Ok(CancelRequestNotification {
            request_id: cancelled,
            ..
        }) = parse_params(raw)
        else {
            debug!("$/cancel_request without a request id");
            return;
        };
        for slot in self.sessions.values_mut() {
            match slot {
                SessionSlot::Active(state) if state.owes(&cancelled) => {
                    return state.cancel(&self.outbound);
                }
                SessionSlot::Active(_) => {}
                SessionSlot::Starting(queued) | SessionSlot::Closing(queued) => {
                    let before = queued.len();
                    queued.retain(|message| {
                        message.get("id").map(request_id).as_ref() != Some(&cancelled)
                    });
                    if queued.len() != before {
                        return self.outbound.respond(cancelled, Err(request_cancelled()));
                    }
                }
            }
        }
        debug!(
            request_id = ?cancelled,
            "$/cancel_request names no request that can still be cancelled"
        );
    }

    /// Outgoing ids are unique across sessions, so the one that asked is the
    /// one still waiting on this id.
    pub(crate) fn handle_response(&self, raw: &Value) {
        let Some(id) = raw.get("id").and_then(Value::as_i64) else {
            return;
        };
        let waiting = self.sessions.values().find_map(|slot| match slot {
            SessionSlot::Active(state) => locked(&state.pending)
                .asks
                .remove(&id)
                .map(|kind| (state, kind)),
            SessionSlot::Starting(_) | SessionSlot::Closing(_) => None,
        });
        match waiting {
            Some((state, kind)) => settle(state, kind, raw),
            None => warn!(id, "response for an unknown request id"),
        }
    }
}

/// Finds the session `sid` names, queueing `raw` behind one that is still
/// starting or closing.
fn target<'a>(
    sessions: &'a mut HashMap<SessionId, SessionSlot>,
    failed: &HashMap<SessionId, FailedRuntime>,
    sid: &SessionId,
    raw: Value,
) -> Result<Target<'a>, AcpError> {
    let Ok(session) = sid.0.parse::<SessionRef>() else {
        return Err(AcpError::resource_not_found(Some(sid.0.to_string())));
    };
    let sid = SessionId::from(session.to_string());
    if let Some(failure) = failed.get(&sid) {
        return Err(failure.refusal());
    }
    match sessions.get_mut(&sid) {
        Some(SessionSlot::Starting(queued) | SessionSlot::Closing(queued)) => {
            queued.push(raw);
            Ok(Target::Queued)
        }
        Some(SessionSlot::Active(state)) => Ok(Target::Active(state)),
        None => Ok(Target::Stored(session)),
    }
}

/// The empty result an extension request answers with once it took effect.
fn acknowledged() -> Result<AgentResponse, AcpError> {
    ext_response(&Map::new())
}

fn ext_response(result: &impl Serialize) -> Result<AgentResponse, AcpError> {
    let raw = to_raw_value(result).map_err(|error| internal(&error))?;
    Ok(AgentResponse::ExtMethodResponse(ExtResponse::new(
        Arc::from(raw),
    )))
}

fn request_id(v: &Value) -> RequestId {
    RequestId::deserialize(v).unwrap_or(RequestId::Null)
}

fn session_scope(raw: &Value) -> Result<SessionId, AcpError> {
    SessionScoped::deserialize(params(raw))
        .map(|scoped| scoped.session_id)
        .map_err(|error| invalid_params(&error))
}

/// The extra roots a client asked for: absolute, each once, and never the cwd.
/// Caudra grants a session no roots beyond its cwd, so they are checked and
/// noted but change nothing.
fn check_additional_directories(
    session: &str,
    cwd: &Path,
    requested: &[PathBuf],
) -> Result<Vec<PathBuf>, AcpError> {
    let mut directories: Vec<PathBuf> = Vec::new();
    for directory in requested {
        if !directory.is_absolute() {
            return Err(invalid_params(RELATIVE_DIRECTORY));
        }
        if directory != cwd && !directories.contains(directory) {
            directories.push(directory.clone());
        }
    }
    if !directories.is_empty() {
        warn!(
            %session,
            ?directories,
            "additionalDirectories are not honoured; the session reaches only its cwd"
        );
    }
    Ok(directories)
}

fn replay_active(
    state: &mut SessionState,
    outbound: &Outbound,
    model_specs: &[String],
    request_cwd: &Path,
) -> Result<AgentResponse, AcpError> {
    let updates = replay_stored(
        &state.runtime.registry,
        &state.handle.state_dir,
        state.handle.session_id.id(),
        request_cwd,
    )?;
    let sid = state.id();
    for update in updates {
        outbound.session_update(&sid, update);
    }
    Ok(load_response(state, model_specs))
}

fn set_mode(
    state: &mut SessionState,
    outbound: &Outbound,
    model_specs: &[String],
    raw: &Value,
) -> Result<AgentResponse, AcpError> {
    let request: SetSessionModeRequest = parse_params(raw)?;
    state.set_mode(&request.mode_id.0, outbound)?;
    state.announce_options(outbound, model_specs);
    Ok(AgentResponse::SetSessionModeResponse(
        SetSessionModeResponse::new(),
    ))
}

/// What applying a config option needs from the server.
struct ConfigContext<'a> {
    outbound: &'a Outbound,
    policy: &'a ModelPolicy,
    model_specs: &'a [String],
}

/// Applies one option, announces what changed, and answers with every option,
/// which is what a client re-renders its pickers from.
fn set_config(
    state: &mut SessionState,
    settings: &ConfigContext<'_>,
    raw: &Value,
) -> Result<AgentResponse, AcpError> {
    let request: SetSessionConfigOptionRequest = parse_params(raw)?;
    let value = &request.value;
    match request.config_id.0.as_ref() {
        methods::MODEL_CONFIG_ID => {
            let spec = value_id(value)?;
            if !settings.policy.allows(spec) {
                return Err(invalid_params(MODEL_NOT_ALLOWED));
            }
            let model = Model::from_spec(spec).map_err(|e| invalid_params(&e))?;
            state.set_model(model)?;
        }
        methods::EFFORT_CONFIG_ID if state.model.supports_thinking() => {
            let level = value_id(value)?;
            state.thinking = methods::effort_choice(&state.model, level)
                .ok_or_else(|| invalid_params(&format!("{UNKNOWN_EFFORT}: {level}")))?;
        }
        methods::MODE_CONFIG_ID => state.set_mode(value_id(value)?, settings.outbound)?,
        methods::PERMISSIONS_CONFIG_ID => {
            let id = value_id(value)?;
            let mode = methods::permission_mode(id)
                .ok_or_else(|| invalid_params(&format!("{UNKNOWN_PERMISSION_MODE}: {id}")))?;
            state.handle.permissions.set_session_mode(Some(mode));
        }
        methods::FAST_CONFIG_ID if state.offers_fast() => {
            state.fast = value
                .as_bool()
                .ok_or_else(|| invalid_params(BOOLEAN_EXPECTED))?;
        }
        unknown => {
            return Err(invalid_params(&format!(
                "{UNKNOWN_CONFIG_OPTION}: {unknown}"
            )));
        }
    }
    let options = state.announce_options(settings.outbound, settings.model_specs);
    Ok(AgentResponse::SetSessionConfigOptionResponse(
        SetSessionConfigOptionResponse::new(options),
    ))
}

fn value_id(value: &SessionConfigOptionValue) -> Result<&str, AcpError> {
    value
        .as_value_id()
        .map(|id| id.0.as_ref())
        .ok_or_else(|| invalid_params(VALUE_ID_EXPECTED))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use agent_client_protocol_schema::v1::ErrorCode;
    use caudra_agent::headless::PendingInputs;
    use caudra_agent::mcp::call::Elicitor;
    use caudra_agent::permissions::PermissionManager;
    use caudra_agent::tools::PermissionScopes;
    use caudra_agent::tools::native::plan::PlanTarget;
    use caudra_agent::types::AgentEvent;
    use caudra_agent::{
        AgentInput, CancelToken, DoneReason, ElicitAction, Envelope, EventSender, ExtractedCommand,
        InterruptSource, QueueItemId, SteeringQueueReceiver, StoredSession, ToolOutput,
        steering_queue,
    };
    use caudra_providers::{HistoryItem, ThinkingConfig, ThinkingSupport};
    use caudra_providers::{Message, TokenUsage};
    use caudra_storage::id::CaudraId;
    use caudra_storage::sessions::Session;
    use flume::Receiver;
    use serde_json::json;
    use test_case::test_case;

    use super::*;
    use crate::AcpRuntimeGuard;
    use crate::asks::{McpElicitor, McpElicitors, resolve_presented_permission};
    use crate::errors::{
        DATA_DETAILS, PROMPT_ACTIVE, REQUEST_CANCELLED, SESSION_ENDED, UNKNOWN_SESSION,
    };
    use crate::lifecycle::{StartOutcome, close_session};
    use crate::outbound::{AskKind, Written};
    use crate::test_support::{
        ANSWERED_ID, AVAILABLE_COMMANDS_UPDATE, CANCELLED, CAUDRA_REQUEST_ID, FAST_SPEC, LEVELS,
        LOGIN_MISSING, MCP_FIELD, MCP_MESSAGE, MCP_TOOL_CALL, OFFLINE_SPEC, PLAN_FILE, PROMPT_ID,
        SECOND_CAUDRA_REQUEST_ID, SECOND_PROMPT_ID, TestServer, UNKNOWN_ID, allow_once,
        effort_model, history_items, mcp_form, no_server, owed, owed_request, permission_manager,
        selected_permission, state_dir,
    };
    use crate::translate::META_NAMESPACE;
    use crate::turn::{DELIVERY_STEER, META_DELIVERY, META_PROMPT_ID, META_STEERED_INTO};

    const MCP_SERVER: &str = "deploys";
    const MCP_ANSWER: &str = "main";
    const ELICITATION_METHOD: &str = "elicitation/create";
    const ANSWER_CHANNEL_CLEAR: &str =
        "an MCP answer must never reach the session's answer channel";
    const PROMPT_TEXT: &str = "rename foo to bar";
    const GUARD_REFUSED: &str = "plugin host would not stop";
    const PROJECT: &str = "/project";
    const STEER_TEXT: &str = "rename baz too";
    const STEER_PROMPT_ID: &str = "steer-prompt";
    const TURN_RUN: u64 = 1;
    const MAX_TOKENS: &str = "max_tokens";
    const END_MARKER: &str = "end of events";
    const ANSWERED_ONCE: &str = "a prompt is answered exactly once";

    fn request(id: i64, method: &str, params: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
    }

    fn prompt(id: i64, sid: &SessionId) -> Value {
        request(
            id,
            SESSION_PROMPT,
            json!({"sessionId": sid, "prompt": [{"type": "text", "text": PROMPT_TEXT}]}),
        )
    }

    fn load(id: i64, sid: &SessionId) -> Value {
        request(
            id,
            SESSION_LOAD,
            json!({"sessionId": sid, "cwd": PROJECT, "mcpServers": []}),
        )
    }

    fn cancel(sid: &SessionId) -> Value {
        json!({"jsonrpc": "2.0", "method": SESSION_CANCEL, "params": {"sessionId": sid}})
    }

    fn error_of(message: &Value) -> (i32, &str) {
        let error = &message["error"];
        (
            error["code"].as_i64().unwrap() as i32,
            error["message"].as_str().unwrap(),
        )
    }

    /// Replaces the stub agent's input channel with one the test can read.
    fn agent_inputs(test: &mut TestServer) -> Receiver<AgentInput> {
        let (input_tx, inputs) = flume::unbounded();
        test.session_mut().handle.input_tx = input_tx;
        inputs
    }

    fn pending_permission(
        manager: Arc<PermissionManager>,
        request_id: &str,
        scope: &str,
    ) -> (smol::Task<bool>, Receiver<Envelope>) {
        let (event_tx, event_rx) = flume::unbounded();
        let event_tx = EventSender::new(event_tx, 0);
        let request_id = request_id.to_owned();
        let scope = scope.to_owned();
        let task = smol::spawn(async move {
            let (_legacy_tx, legacy_rx) = flume::unbounded();
            let legacy_rx = smol::lock::Mutex::new(legacy_rx);
            manager
                .enforce(
                    &caudra_config::ToolKey::native("bash"),
                    &PermissionScopes::single(scope.clone()),
                    &json!({"command": scope}),
                    &event_tx,
                    Some(&legacy_rx),
                    &request_id,
                    &CancelToken::none(),
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
                &json!({"command": scope}),
                &EventSender::new(event_tx, 0),
                None,
                "follow-up",
                &CancelToken::none(),
                None,
            )
            .await
            .is_ok()
    }

    fn permission_ask(request_id: &str) -> AskKind {
        AskKind::Permission {
            request_id: request_id.into(),
            exact_project_deny: true,
        }
    }

    #[test]
    fn only_the_outstanding_request_id_is_answered() {
        smol::block_on(async {
            let manager = permission_manager();
            let (task, event_rx) =
                pending_permission(Arc::clone(&manager), CAUDRA_REQUEST_ID, "cargo test");
            let _ = event_rx.recv_async().await.unwrap();
            let test = TestServer::with_asks(
                Arc::clone(&manager),
                HashMap::from([(ANSWERED_ID, permission_ask(CAUDRA_REQUEST_ID))]),
            );

            test.server.handle_response(&allow_once(UNKNOWN_ID));
            assert_eq!(manager.pending_count(), 1, "an unknown id is dropped");

            test.server.handle_response(&allow_once(ANSWERED_ID));
            assert!(task.await);
            assert!(test.answers.is_empty(), "permission bypasses answer_tx");

            test.server.handle_response(&allow_once(ANSWERED_ID));
            assert!(test.answers.is_empty(), "a replayed answer is dropped");
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
                (ANSWERED_ID, permission_ask(CAUDRA_REQUEST_ID)),
                (UNKNOWN_ID, permission_ask(SECOND_CAUDRA_REQUEST_ID)),
            ]);
            let test = TestServer::with_asks(Arc::clone(&manager), asks);

            test.server.handle_response(&allow_once(UNKNOWN_ID));
            test.server
                .handle_response(&selected_permission(ANSWERED_ID, "reject_once"));

            assert!(!first.await);
            assert!(second.await);
            assert!(test.answers.is_empty());
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
                (ANSWERED_ID, permission_ask(CAUDRA_REQUEST_ID)),
                (UNKNOWN_ID, permission_ask(SECOND_CAUDRA_REQUEST_ID)),
            ]);
            let test = TestServer::with_asks(Arc::clone(&manager), asks);

            test.server
                .handle_response(&selected_permission(ANSWERED_ID, "allow_always"));
            let resolution = second_events.recv_async().await.unwrap().event;
            let AgentEvent::PermissionRequestResolved { request_id, .. } = resolution else {
                panic!("expected permission resolution, got {resolution:?}");
            };
            resolve_presented_permission(
                &test.server.outbound,
                &test.session().pending,
                &request_id,
            );

            assert!(first.await);
            assert!(second.await);
            assert!(test.answers.is_empty());
            let cancelled = test.frames.recv_async().await.unwrap().message;
            assert_eq!(cancelled["method"], "$/cancel_request");
            assert_eq!(cancelled["params"]["requestId"], UNKNOWN_ID);
            test.server.handle_response(&allow_once(UNKNOWN_ID));
            assert!(test.answers.is_empty());
        });
    }

    #[test]
    fn acp_allow_always_reuses_only_the_exact_conversation_scope() {
        smol::block_on(async {
            let manager = permission_manager();
            let (task, event_rx) =
                pending_permission(Arc::clone(&manager), CAUDRA_REQUEST_ID, "cargo test");
            let _ = event_rx.recv_async().await.unwrap();
            let test = TestServer::with_asks(
                Arc::clone(&manager),
                HashMap::from([(ANSWERED_ID, permission_ask(CAUDRA_REQUEST_ID))]),
            );

            test.server
                .handle_response(&selected_permission(ANSWERED_ID, "allow_always"));

            assert!(task.await);
            assert!(enforcement_without_answer(&manager, "cargo test").await);
            assert!(!enforcement_without_answer(&manager, "cargo publish").await);
        });
    }

    #[test]
    fn cancel_drops_the_outstanding_permission_request() {
        let mut test = TestServer::with_ask(permission_ask(CAUDRA_REQUEST_ID));
        test.server.handle_message(cancel(&test.sid));

        test.server.handle_response(&allow_once(ANSWERED_ID));
        assert!(
            test.answers.is_empty(),
            "the cancelled turn owns that answer"
        );
    }

    fn cancel_request(id: i64) -> Value {
        json!({"jsonrpc": "2.0", "method": CANCEL_REQUEST_METHOD, "params": {"requestId": id}})
    }

    fn steer(id: i64, sid: &SessionId) -> Value {
        request(
            id,
            SESSION_PROMPT,
            json!({
                "sessionId": sid,
                "prompt": [{"type": "text", "text": STEER_TEXT}],
                "_meta": {
                    META_PROMPT_ID: STEER_PROMPT_ID,
                    META_NAMESPACE: {META_DELIVERY: DELIVERY_STEER},
                },
            }),
        )
    }

    fn turn_event(event: AgentEvent) -> Envelope {
        Envelope {
            event,
            task: None,
            subagent: None,
            run_id: TURN_RUN,
            workflow: None,
        }
    }

    fn done(reason: DoneReason) -> AgentEvent {
        AgentEvent::Done {
            usage: TokenUsage::default(),
            num_turns: 1,
            reason,
        }
    }

    /// The session's agent events come from the returned sender, and its
    /// pump answers prompts the way a live session does.
    fn pumped(test: &TestServer) -> flume::Sender<Envelope> {
        let (event_tx, events) = flume::unbounded();
        let mut pump = test
            .session()
            .event_pump(test.server.outbound.clone(), no_server(), None);
        pump.events = events;
        pump.spawn(Written::settled());
        event_tx
    }

    /// Replaces the stub agent's stop channel with one the test can read.
    fn agent_stops(test: &mut TestServer) -> Receiver<()> {
        let (cancel_tx, stops) = flume::unbounded();
        test.session_mut().handle.cancel_tx = cancel_tx;
        stops
    }

    /// A client's `$/cancel_request` for its prompt is a `session/cancel`; a
    /// turn still settling when it lands ends `cancelled` all the same.
    #[test_case(cancel_request(PROMPT_ID) ; "cancel_request")]
    #[test_case(cancel(&SessionId::from(String::new())) ; "session_cancel")]
    fn a_cancelled_prompt_is_answered_cancelled_once(cancelling: Value) {
        let mut test = TestServer::with_session();
        let inputs = agent_inputs(&mut test);
        let stops = agent_stops(&mut test);
        let sid = test.sid.clone();
        let cancelling = match cancelling["method"].as_str() {
            Some(SESSION_CANCEL) => cancel(&sid),
            _ => cancelling,
        };
        let events = pumped(&test);
        test.server.handle_message(prompt(PROMPT_ID, &sid));
        inputs.try_recv().expect("the agent took the prompt");

        test.server.handle_message(cancelling);

        assert_eq!(stops.try_recv(), Ok(()), "the agent is stopped");
        events.send(turn_event(done(DoneReason::EndTurn))).unwrap();
        let answered = test.frames.recv().unwrap().message;
        assert_eq!(answered["id"], PROMPT_ID);
        assert_eq!(answered["result"]["stopReason"], CANCELLED);
        assert_eq!(owed_request(&test.session().pending), None);
        events
            .send(turn_event(AgentEvent::TextDelta {
                text: END_MARKER.into(),
            }))
            .unwrap();
        let next = test.frames.recv().unwrap().message;
        assert_eq!(
            next["params"]["update"]["content"]["text"], END_MARKER,
            "{ANSWERED_ONCE}"
        );
    }

    #[test]
    fn cancel_withdraws_every_outstanding_ask() {
        let (mcp_answer, mcp_answered) = flume::bounded(1);
        let mut test = TestServer::with_asks(
            permission_manager(),
            HashMap::from([
                (ANSWERED_ID, permission_ask(CAUDRA_REQUEST_ID)),
                (UNKNOWN_ID, AskKind::McpElicitation(mcp_answer)),
            ]),
        );

        test.server.handle_message(cancel(&test.sid));

        let mut withdrawn: Vec<i64> = test
            .frames
            .drain()
            .map(|frame| {
                assert_eq!(frame.message["method"], CANCEL_REQUEST_METHOD);
                frame.message["params"]["requestId"].as_i64().unwrap()
            })
            .collect();
        withdrawn.sort_unstable();
        assert_eq!(withdrawn, [ANSWERED_ID, UNKNOWN_ID]);
        assert!(locked(&test.session().pending).asks.is_empty());
        assert_eq!(mcp_answered.try_recv(), Ok(Value::Null));
    }

    /// A prompt still queued for a busy agent is taken back, so it never runs
    /// behind the client's back, and answered at once.
    #[test]
    fn a_cancel_before_the_agent_took_the_prompt_withdraws_and_answers_it() {
        let mut test = TestServer::with_session();
        let inputs = agent_inputs(&mut test);
        test.session_mut().handle.pending_inputs = PendingInputs::new(inputs.clone());
        let sid = test.sid.clone();

        test.server.handle_message(prompt(PROMPT_ID, &sid));
        test.server.handle_message(cancel(&sid));

        let answered = test.next_message();
        assert_eq!(answered["id"], PROMPT_ID);
        assert_eq!(answered["result"]["stopReason"], CANCELLED);
        assert!(answered["result"]["_meta"][META_NAMESPACE][META_PROMPT_ID].is_string());
        assert!(inputs.is_empty(), "the prompt never reaches the agent");
        assert_eq!(owed_request(&test.session().pending), None);
        assert!(test.frames.is_empty(), "{ANSWERED_ONCE}");
    }

    #[test]
    fn a_cancel_request_drops_a_request_queued_behind_a_starting_session() {
        let mut test = TestServer::initialized();
        let sid = SessionId::from(UNKNOWN_SESSION);
        test.server
            .sessions
            .insert(sid.clone(), SessionSlot::Starting(Vec::new()));
        test.server.handle_message(prompt(PROMPT_ID, &sid));

        test.server.handle_message(cancel_request(PROMPT_ID));

        let refused = test.next_message();
        assert_eq!(refused["id"], PROMPT_ID);
        assert_eq!(
            error_of(&refused),
            (i32::from(ErrorCode::RequestCancelled), REQUEST_CANCELLED)
        );
        assert!(matches!(
            test.server.sessions.get(&sid),
            Some(SessionSlot::Starting(queued)) if queued.is_empty()
        ));
    }

    /// Replaces the stub agent's steering queue with one the test drains as
    /// the agent's next boundary would.
    fn agent_steering(test: &mut TestServer) -> SteeringQueueReceiver {
        let (steering, boundary) = steering_queue();
        test.session_mut().handle.steering = steering;
        boundary
    }

    fn taken_steer(boundary: &SteeringQueueReceiver) -> (QueueItemId, String) {
        match boundary.poll() {
            Some(ExtractedCommand::Interrupt(input, _, id)) => (id, input.message),
            _ => panic!("one steer waits at the boundary"),
        }
    }

    fn answers(test: &TestServer, count: usize) -> Vec<Value> {
        (0..count)
            .map(|_| test.frames.recv().unwrap().message)
            .collect()
    }

    /// A steer joins the running turn, and both prompts are answered once,
    /// with the turn's stop reason, when that turn ends.
    #[test]
    fn a_steer_joins_the_running_turn_and_ends_with_it() {
        let mut test = TestServer::with_session();
        let inputs = agent_inputs(&mut test);
        let boundary = agent_steering(&mut test);
        let sid = test.sid.clone();
        let events = pumped(&test);
        test.server.handle_message(prompt(PROMPT_ID, &sid));

        test.server.handle_message(steer(SECOND_PROMPT_ID, &sid));

        assert!(test.frames.is_empty(), "a steer is answered with its turn");
        assert_eq!(inputs.len(), 1, "a steer starts no turn of its own");
        let (item, message) = taken_steer(&boundary);
        assert_eq!(message, STEER_TEXT);
        events
            .send(turn_event(AgentEvent::QueueItemConsumed {
                id: item,
                text: message,
                images: Vec::new(),
            }))
            .unwrap();
        events
            .send(turn_event(done(DoneReason::MaxTokens)))
            .unwrap();

        let [prompted, steered] = answers(&test, 2).try_into().unwrap();
        assert_eq!(prompted["id"], PROMPT_ID);
        assert_eq!(steered["id"], SECOND_PROMPT_ID);
        for answer in [&prompted, &steered] {
            assert_eq!(answer["result"]["stopReason"], MAX_TOKENS);
        }
        let steered = &steered["result"]["_meta"][META_NAMESPACE];
        assert_eq!(steered[META_PROMPT_ID], STEER_PROMPT_ID);
        assert_eq!(
            steered[META_STEERED_INTO],
            prompted["result"]["_meta"][META_NAMESPACE][META_PROMPT_ID]
        );
        assert_eq!(owed_request(&test.session().pending), None);
        assert!(test.frames.is_empty(), "{ANSWERED_ONCE}");
    }

    /// A steer the turn ended before taking starts the next turn, which owes
    /// it the answer.
    #[test]
    fn a_steer_the_turn_never_took_starts_the_next_turn() {
        let mut test = TestServer::with_session();
        let inputs = agent_inputs(&mut test);
        let boundary = agent_steering(&mut test);
        let sid = test.sid.clone();
        let events = pumped(&test);
        test.server.handle_message(prompt(PROMPT_ID, &sid));
        test.server.handle_message(steer(SECOND_PROMPT_ID, &sid));
        assert_eq!(inputs.try_recv().unwrap().message, PROMPT_TEXT);

        events.send(turn_event(done(DoneReason::EndTurn))).unwrap();

        let [prompted] = answers(&test, 1).try_into().unwrap();
        assert_eq!(prompted["id"], PROMPT_ID);
        assert_eq!(inputs.recv().unwrap().message, STEER_TEXT);
        assert!(boundary.poll().is_none(), "the steer left the queue");
        assert_eq!(
            owed_request(&test.session().pending),
            Some(RequestId::Number(SECOND_PROMPT_ID))
        );
        events.send(turn_event(done(DoneReason::EndTurn))).unwrap();
        let [steered] = answers(&test, 1).try_into().unwrap();
        assert_eq!(steered["id"], SECOND_PROMPT_ID);
        assert_eq!(
            steered["result"]["_meta"][META_NAMESPACE][META_PROMPT_ID],
            STEER_PROMPT_ID
        );
    }

    #[test]
    fn a_steer_with_no_turn_running_is_a_prompt() {
        let mut test = TestServer::with_session();
        let inputs = agent_inputs(&mut test);
        let sid = test.sid.clone();

        test.server.handle_message(steer(PROMPT_ID, &sid));

        assert_eq!(inputs.try_recv().unwrap().message, STEER_TEXT);
        assert_eq!(
            owed_request(&test.session().pending),
            Some(RequestId::Number(PROMPT_ID))
        );
    }

    #[test]
    fn cancel_reaches_only_the_named_session() {
        let mut test = TestServer::with_ask(AskKind::Elicitation);
        let other = test.add_session();

        test.server.handle_message(cancel(&other));

        assert!(
            locked(&test.session().pending)
                .asks
                .contains_key(&ANSWERED_ID)
        );
    }

    #[test]
    fn elicitation_response_forwards_the_raw_result() {
        let test = TestServer::with_ask(AskKind::Elicitation);
        let raw = json!({
            "id": ANSWERED_ID,
            "result": { "action": "accept", "content": { "q1": "axum" } },
        });

        test.server.handle_response(&raw);
        let forwarded = test.answers.try_recv().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&forwarded).unwrap(),
            raw["result"]
        );
    }

    fn mcp_elicitor(test: &TestServer) -> McpElicitor {
        McpElicitor::new(
            McpElicitors {
                outbound: test.server.outbound.clone(),
                pending: Arc::clone(&test.session().pending),
                form: true,
                url: false,
            },
            test.sid.to_string(),
            Some(MCP_TOOL_CALL.into()),
        )
    }

    #[test]
    fn an_mcp_elicitation_round_trips_through_the_client() {
        let test = TestServer::with_session();
        let elicitor = mcp_elicitor(&test);

        let (response, ()) = smol::block_on(smol::future::zip(
            elicitor.elicit(MCP_SERVER, mcp_form()),
            async {
                let sent = test.frames.recv_async().await.unwrap().message;
                assert_eq!(sent["method"], ELICITATION_METHOD);
                assert_eq!(sent["params"]["message"], MCP_MESSAGE);
                assert_eq!(sent["params"]["toolCallId"], MCP_TOOL_CALL);
                assert_eq!(
                    sent["params"]["requestedSchema"]["properties"][MCP_FIELD]["type"],
                    "string"
                );
                let result = json!({"action": "accept", "content": {MCP_FIELD: MCP_ANSWER}});
                test.server
                    .handle_response(&json!({"id": sent["id"], "result": result}));
            },
        ));

        assert_eq!(response.action, ElicitAction::Accept);
        assert_eq!(
            response.content,
            json!({MCP_FIELD: MCP_ANSWER}).as_object().cloned()
        );
        assert_eq!(response.answers, [(MCP_FIELD.into(), MCP_ANSWER.into())]);
        assert!(test.answers.is_empty(), "{ANSWER_CHANNEL_CLEAR}");
    }

    #[test]
    fn a_withdrawn_mcp_elicitation_forgets_its_ask() {
        let test = TestServer::with_session();
        let elicitor = mcp_elicitor(&test);

        smol::block_on(smol::future::or(
            async {
                elicitor.elicit(MCP_SERVER, mcp_form()).await;
            },
            async {
                test.frames.recv_async().await.unwrap();
            },
        ));

        assert!(locked(&test.session().pending).asks.is_empty());
    }

    #[test_case(prompt(PROMPT_ID, &SessionId::from(UNKNOWN_SESSION)) ; "prompt")]
    #[test_case(request(PROMPT_ID, SESSION_SET_MODE, json!({"sessionId": UNKNOWN_SESSION, "modeId": methods::MODE_PLAN})) ; "set_mode")]
    #[test_case(request(PROMPT_ID, SESSION_SET_CONFIG_OPTION, json!({"sessionId": UNKNOWN_SESSION, "configId": methods::MODEL_CONFIG_ID, "value": "x"})) ; "set_config_option")]
    fn a_request_for_an_unknown_session_is_resource_not_found(message: Value) {
        let mut test = TestServer::with_session();
        test.server.handle_message(message);
        let answer = test.next_message();
        let (code, text) = error_of(&answer);
        assert_eq!(code, i32::from(ErrorCode::ResourceNotFound));
        assert!(text.starts_with(UNKNOWN_SESSION));
    }

    #[test]
    fn a_prompt_is_registered_before_the_agent_sees_it() {
        let mut test = TestServer::with_session();
        let inputs = agent_inputs(&mut test);

        test.server.handle_message(prompt(PROMPT_ID, &test.sid));

        assert_eq!(inputs.try_recv().unwrap().message, PROMPT_TEXT);
        assert_eq!(
            owed_request(&test.session().pending),
            Some(RequestId::Number(PROMPT_ID))
        );
        assert!(test.frames.is_empty(), "the turn answers the prompt");
    }

    #[test]
    fn a_second_prompt_is_refused_while_the_first_is_owed() {
        let mut test = TestServer::with_session();
        let inputs = agent_inputs(&mut test);
        let sid = test.sid.clone();

        test.server.handle_message(prompt(PROMPT_ID, &sid));
        test.server.handle_message(prompt(SECOND_PROMPT_ID, &sid));

        let refused = test.next_message();
        assert_eq!(refused["id"], SECOND_PROMPT_ID);
        assert_eq!(
            error_of(&refused),
            (i32::from(ErrorCode::InvalidRequest), PROMPT_ACTIVE)
        );
        assert_eq!(inputs.len(), 1);
        assert_eq!(
            owed_request(&test.session().pending),
            Some(RequestId::Number(PROMPT_ID))
        );
    }

    #[test]
    fn a_prompt_the_agent_cannot_take_is_not_left_owed() {
        let mut test = TestServer::with_session();
        test.server.handle_message(prompt(PROMPT_ID, &test.sid));
        assert_eq!(error_of(&test.next_message()).1, SESSION_ENDED);
        assert_eq!(owed_request(&test.session().pending), None);
    }

    #[test_case(request(PROMPT_ID, SESSION_NEW, json!({"cwd": PROJECT, "mcpServers": []})) ; "new")]
    #[test_case(load(PROMPT_ID, &SessionId::from(UNKNOWN_SESSION)) ; "load")]
    #[test_case(prompt(PROMPT_ID, &SessionId::from(UNKNOWN_SESSION)) ; "prompt")]
    fn session_methods_wait_for_initialize(message: Value) {
        let mut test = TestServer::initialized();
        test.server.client = None;
        test.server.handle_message(message);
        assert_eq!(
            error_of(&test.next_message()),
            (i32::from(ErrorCode::InvalidRequest), INITIALIZE_FIRST)
        );
        assert!(test.server.sessions.is_empty());
    }

    #[test_case(json!({"protocolVersion": "one"}) => Some(i64::from(i32::from(ErrorCode::InvalidParams))) ; "malformed")]
    #[test_case(json!({"protocolVersion": 1}) => None ; "well_formed")]
    fn initialize_checks_its_params(params: Value) -> Option<i64> {
        let mut test = TestServer::initialized();
        test.server.client = None;
        test.server
            .handle_message(request(PROMPT_ID, INITIALIZE, params));
        let answer = test.next_message();
        assert_eq!(answer["error"].is_null(), test.server.client.is_some());
        answer["error"]["code"].as_i64()
    }

    #[test]
    fn requests_for_a_starting_session_wait_and_then_see_how_it_ended() {
        let mut test = TestServer::initialized();
        let sid = SessionId::from(UNKNOWN_SESSION);
        test.server
            .sessions
            .insert(sid.clone(), SessionSlot::Starting(Vec::new()));

        test.server.handle_message(prompt(SECOND_PROMPT_ID, &sid));
        assert!(test.frames.is_empty(), "the prompt waits for the start");

        test.server.settle_start(StartOutcome {
            request_id: RequestId::Number(PROMPT_ID),
            session: sid,
            result: Err(AcpError::internal_error()),
            failure: None,
        });

        assert_eq!(test.next_message()["id"], PROMPT_ID);
        let queued = test.next_message();
        assert_eq!(queued["id"], SECOND_PROMPT_ID);
        assert_eq!(error_of(&queued).0, i32::from(ErrorCode::ResourceNotFound));
    }

    #[test]
    fn loading_a_live_session_replays_it_without_a_second_agent() {
        let mut test = TestServer::with_session();
        let mut stored: Session<HistoryItem, TokenUsage, ToolOutput> =
            Session::new(crate::test_support::SELECTED_SPEC, PROJECT);
        stored.id = test.session().handle.session_id.id();
        stored.replace_messages(history_items(&[Message::user(PROMPT_TEXT.into())]));
        stored.save(&state_dir()).unwrap();
        let sid = test.sid.clone();

        for id in [PROMPT_ID, SECOND_PROMPT_ID] {
            test.server.handle_message(load(id, &sid));
            let replayed = test.next_message();
            assert_eq!(replayed["params"]["sessionId"], sid.0.as_ref());
            assert_eq!(replayed["params"]["update"]["content"]["text"], PROMPT_TEXT);
            let loaded = test.next_message();
            assert_eq!(loaded["id"], id);
            assert_eq!(
                loaded["result"]["modes"]["currentModeId"],
                methods::MODE_BUILD
            );
            let commands = test.next_message();
            assert_eq!(
                commands["params"]["update"]["sessionUpdate"],
                AVAILABLE_COMMANDS_UPDATE
            );
        }
        assert_eq!(test.server.sessions.len(), 1);
    }

    const CLOSE_ID: i64 = 21;
    const OUTSIDE: &str = "/outside";
    const RELATIVE: &str = "relative/dir";

    fn close(id: i64, method: &str, sid: &SessionId) -> Value {
        request(id, method, json!({"sessionId": sid}))
    }

    fn store_history(sid: CaudraId) {
        let mut stored: Session<HistoryItem, TokenUsage, ToolOutput> =
            Session::new(crate::test_support::SELECTED_SPEC, PROJECT);
        stored.id = sid;
        stored.replace_messages(history_items(&[Message::user(PROMPT_TEXT.into())]));
        stored.save(&state_dir()).unwrap();
    }

    #[test]
    fn closing_answers_the_owed_prompt_then_forgets_the_session() {
        let mut test = TestServer::with_session();
        let sid = test.sid.clone();
        locked(&test.session().pending).prompt = Some(owed(PROMPT_ID));

        test.server
            .handle_message(close(CLOSE_ID, SESSION_CLOSE, &sid));
        test.server.handle_message(prompt(SECOND_PROMPT_ID, &sid));
        test.settle_next();

        let cancelled = test.next_message();
        assert_eq!(cancelled["id"], PROMPT_ID);
        assert_eq!(cancelled["result"]["stopReason"], CANCELLED);
        let closed = test.next_message();
        assert_eq!(closed["id"], CLOSE_ID);
        assert!(closed["error"].is_null());
        let refused = test.next_message();
        assert_eq!(refused["id"], SECOND_PROMPT_ID);
        assert_eq!(error_of(&refused).0, i32::from(ErrorCode::ResourceNotFound));
        assert!(test.server.sessions.is_empty());
    }

    #[test_case(SESSION_CLOSE ; "close")]
    #[test_case(SESSION_DELETE ; "delete")]
    fn closing_an_unknown_session_is_resource_not_found(method: &str) {
        let mut test = TestServer::initialized();
        let sid = SessionId::from(SessionRef::generate().to_string());

        test.server.handle_message(close(CLOSE_ID, method, &sid));
        if method == SESSION_DELETE {
            test.settle_next();
        }

        let refused = test.next_message();
        assert_eq!(error_of(&refused).0, i32::from(ErrorCode::ResourceNotFound));
        assert!(test.server.sessions.is_empty());
    }

    #[test_case(true ; "live_session")]
    #[test_case(false ; "stored_session")]
    fn deleting_removes_the_stored_session(live: bool) {
        let mut test = TestServer::initialized();
        let sid = if live {
            test.add_session()
        } else {
            SessionId::from(SessionRef::generate().to_string())
        };
        let id = sid.0.parse::<SessionRef>().unwrap().id();
        store_history(id);

        test.server
            .handle_message(close(CLOSE_ID, SESSION_DELETE, &sid));
        test.settle_next();

        let deleted = test.next_message();
        assert_eq!(deleted["id"], CLOSE_ID);
        assert!(deleted["error"].is_null(), "{deleted}");
        assert!(test.server.sessions.is_empty());
        assert!(caudra_agent::open_stored_session(id, &state_dir()).is_err());
    }

    #[test]
    fn resuming_a_live_session_reports_it_without_replay() {
        let mut test = TestServer::with_session();
        let sid = test.sid.clone();
        store_history(test.session().handle.session_id.id());

        test.server.handle_message(request(
            PROMPT_ID,
            SESSION_RESUME,
            json!({"sessionId": sid, "cwd": PROJECT}),
        ));

        let resumed = test.next_message();
        assert_eq!(resumed["id"], PROMPT_ID);
        assert_eq!(
            resumed["result"]["modes"]["currentModeId"],
            methods::MODE_BUILD
        );
        let commands = test.next_message();
        assert_eq!(
            commands["params"]["update"]["sessionUpdate"],
            AVAILABLE_COMMANDS_UPDATE
        );
        assert!(test.frames.is_empty(), "resume replays nothing");
        assert_eq!(test.server.sessions.len(), 1);
    }

    #[test]
    fn forking_a_missing_session_is_resource_not_found() {
        let mut test = TestServer::initialized();
        let source = SessionRef::generate();

        test.server.handle_message(request(
            PROMPT_ID,
            SESSION_FORK,
            json!({"sessionId": source.to_string(), "cwd": PROJECT}),
        ));
        test.settle_next();

        let refused = test.next_message();
        assert_eq!(error_of(&refused).0, i32::from(ErrorCode::ResourceNotFound));
        assert!(test.server.sessions.is_empty());
    }

    /// The client learns it must sign in from the request that opens the
    /// session, before any runtime or lease is taken for it.
    #[test_case(SESSION_NEW, false ; "new")]
    #[test_case(SESSION_LOAD, true ; "load")]
    #[test_case(SESSION_RESUME, true ; "resume")]
    #[test_case(SESSION_FORK, true ; "fork")]
    fn a_session_without_credentials_is_auth_required(method: &str, stored: bool) {
        let mut test = TestServer::signed_out();
        let mut params = json!({"cwd": PROJECT, "mcpServers": []});
        if stored {
            let mut session = StoredSession::new(OFFLINE_SPEC, PROJECT);
            session.save(&state_dir()).unwrap();
            params["sessionId"] = json!(SessionRef::from(session.id).to_string());
        }

        test.server
            .handle_message(request(PROMPT_ID, method, params));
        test.settle_next();

        let refused = test.next_message();
        assert_eq!(
            error_of(&refused),
            (i32::from(ErrorCode::AuthRequired), LOGIN_MISSING)
        );
        assert_eq!(refused["error"]["data"][DATA_DETAILS], LOGIN_MISSING);
        assert!(test.server.sessions.is_empty());
    }

    #[test]
    fn the_model_catalog_is_listed_without_a_session() {
        let mut test = TestServer::initialized();
        test.server.model_specs = vec![OFFLINE_SPEC.to_owned()];

        test.server
            .handle_message(request(PROMPT_ID, MODELS_LIST, json!({})));

        let listed = test.next_message();
        assert_eq!(listed["id"], PROMPT_ID);
        let catalog = &listed["result"];
        assert_eq!(catalog["defaultModel"], test.server.default_model.spec());
        assert_eq!(catalog["models"][1]["id"], OFFLINE_SPEC);
    }

    #[test_case(json!({"auth": {"terminal": true}}), true ; "terminal_auth_client")]
    #[test_case(json!({}), false ; "client_without_terminal_auth")]
    fn initialize_offers_the_login_the_client_can_run(capabilities: Value, terminal: bool) {
        let mut test = TestServer::initialized();

        test.server.handle_message(request(
            PROMPT_ID,
            INITIALIZE,
            json!({"protocolVersion": 1, "clientCapabilities": capabilities}),
        ));

        let result = &test.next_message()["result"];
        assert_eq!(result["authMethods"][0]["type"].is_string(), terminal);
        assert_eq!(
            result["_meta"][META_NAMESPACE]["defaultModel"],
            test.server.default_model.spec()
        );
    }

    #[test_case(SESSION_NEW, json!({"cwd": PROJECT, "mcpServers": [], "additionalDirectories": [RELATIVE]}) ; "new")]
    #[test_case(SESSION_RESUME, json!({"sessionId": SessionRef::generate().to_string(), "cwd": PROJECT, "additionalDirectories": [RELATIVE]}) ; "resume")]
    #[test_case(SESSION_FORK, json!({"sessionId": SessionRef::generate().to_string(), "cwd": PROJECT, "additionalDirectories": [RELATIVE]}) ; "fork")]
    fn a_relative_additional_directory_is_invalid_params(method: &str, params: Value) {
        let mut test = TestServer::initialized();

        test.server
            .handle_message(request(PROMPT_ID, method, params));

        let refused = test.next_message();
        assert_eq!(error_of(&refused).0, i32::from(ErrorCode::InvalidParams));
        assert_eq!(refused["error"]["data"], RELATIVE_DIRECTORY);
        assert!(test.server.sessions.is_empty());
    }

    #[test]
    fn additional_directories_are_deduplicated_without_the_cwd() {
        let requested = [OUTSIDE, PROJECT, OUTSIDE].map(PathBuf::from);

        let directories =
            check_additional_directories(UNKNOWN_SESSION, Path::new(PROJECT), &requested).unwrap();

        assert_eq!(directories, [PathBuf::from(OUTSIDE)]);
    }

    struct RefusingGuard;

    impl AcpRuntimeGuard for RefusingGuard {
        fn shutdown(&mut self) -> Result<(), String> {
            Err(GUARD_REFUSED.into())
        }
    }

    #[test]
    fn a_runtime_that_will_not_stop_refuses_only_its_own_session() {
        let mut test = TestServer::with_session();
        let other = test.add_session();
        test.session_mut().runtime.guard = Some(Box::new(RefusingGuard));
        let Some(SessionSlot::Active(state)) = test.server.sessions.remove(&test.sid) else {
            panic!("the test session is active");
        };

        let failure = smol::block_on(close_session(*state, &test.server.outbound));
        test.server
            .retain_failed(failure.expect("the refusing runtime fails to stop"));

        test.server.handle_message(load(PROMPT_ID, &test.sid));
        let refused = test.next_message();
        assert!(
            refused["error"]["data"][DATA_DETAILS]
                .as_str()
                .unwrap()
                .contains(GUARD_REFUSED)
        );
        let inputs = {
            let (input_tx, inputs) = flume::unbounded();
            let Some(SessionSlot::Active(state)) = test.server.sessions.get_mut(&other) else {
                panic!("the other session is active");
            };
            state.handle.input_tx = input_tx;
            inputs
        };
        test.server.handle_message(prompt(SECOND_PROMPT_ID, &other));
        assert_eq!(inputs.len(), 1, "the other session still takes prompts");
    }

    const CONFIG_OPTION_UPDATE: &str = "config_option_update";
    const CURRENT_MODE_UPDATE: &str = "current_mode_update";
    const EFFORT_HIGH: &str = "high";
    const YOLO: &str = "yolo";

    fn set_option(sid: &SessionId, config_id: &str, value: Value) -> Value {
        let mut params = json!({"sessionId": sid, "configId": config_id});
        if let (Some(params), Value::Object(value)) = (params.as_object_mut(), value) {
            params.extend(value);
        }
        request(PROMPT_ID, SESSION_SET_CONFIG_OPTION, params)
    }

    /// A session on a model with known levels, whose agent takes model
    /// switches, and whose Plan mode needs no plan allocated.
    fn reasoning_session() -> (TestServer, Receiver<Model>) {
        let mut test = TestServer::with_session();
        let (model_tx, models) = flume::unbounded();
        let session = test.session_mut();
        session.handle.model_tx = model_tx;
        session.model = effort_model(&LEVELS, true);
        session.plan = Some(PlanTarget::Local(PathBuf::from(PLAN_FILE)));
        (test, models)
    }

    fn current<'a>(options: &'a Value, id: &str) -> &'a Value {
        let option = options
            .as_array()
            .unwrap()
            .iter()
            .find(|option| option["id"] == id)
            .unwrap_or_else(|| panic!("no {id} option in {options}"));
        &option["currentValue"]
    }

    fn session_update_kind(message: &Value) -> &Value {
        &message["params"]["update"]["sessionUpdate"]
    }

    #[test_case(methods::MODEL_CONFIG_ID, json!({"value": OFFLINE_SPEC}), json!(OFFLINE_SPEC) ; "model")]
    #[test_case(methods::EFFORT_CONFIG_ID, json!({"value": EFFORT_HIGH}), json!(EFFORT_HIGH) ; "effort")]
    #[test_case(methods::MODE_CONFIG_ID, json!({"value": methods::MODE_PLAN}), json!(methods::MODE_PLAN) ; "mode")]
    #[test_case(methods::PERMISSIONS_CONFIG_ID, json!({"value": YOLO}), json!(YOLO) ; "permissions")]
    fn each_option_is_applied_announced_and_answered_in_full(
        config_id: &str,
        value: Value,
        expected: Value,
    ) {
        let (mut test, _models) = reasoning_session();
        let sid = test.sid.clone();

        test.server
            .handle_message(set_option(&sid, config_id, value));

        if config_id == methods::MODE_CONFIG_ID {
            let mode = test.next_message();
            assert_eq!(session_update_kind(&mode), CURRENT_MODE_UPDATE);
            assert_eq!(
                mode["params"]["update"]["currentModeId"],
                methods::MODE_PLAN
            );
        }
        let update = test.next_message();
        assert_eq!(session_update_kind(&update), CONFIG_OPTION_UPDATE);
        let answer = test.next_message();
        let options = &answer["result"]["configOptions"];
        assert_eq!(&update["params"]["update"]["configOptions"], options);
        let full = serde_json::to_value(test.session().config_options(&[])).unwrap();
        assert_eq!(options, &full);
        assert_eq!(current(options, config_id), &expected);
    }

    #[test]
    fn set_mode_announces_the_mode_and_the_options() {
        let (mut test, _models) = reasoning_session();
        let sid = test.sid.clone();

        test.server.handle_message(request(
            PROMPT_ID,
            SESSION_SET_MODE,
            json!({"sessionId": sid, "modeId": methods::MODE_PLAN}),
        ));

        assert_eq!(
            session_update_kind(&test.next_message()),
            CURRENT_MODE_UPDATE
        );
        let update = test.next_message();
        assert_eq!(session_update_kind(&update), CONFIG_OPTION_UPDATE);
        let options = &update["params"]["update"]["configOptions"];
        assert_eq!(
            current(options, methods::MODE_CONFIG_ID),
            methods::MODE_PLAN
        );
        assert!(test.next_message()["error"].is_null());
    }

    #[test]
    fn an_unchanged_option_is_answered_but_not_announced_again() {
        let (mut test, _models) = reasoning_session();
        let sid = test.sid.clone();
        let choose_yolo =
            || set_option(&sid, methods::PERMISSIONS_CONFIG_ID, json!({"value": YOLO}));

        test.server.handle_message(choose_yolo());
        assert_eq!(
            session_update_kind(&test.next_message()),
            CONFIG_OPTION_UPDATE
        );
        test.next_message();
        test.server.handle_message(choose_yolo());

        let answer = test.next_message();
        assert_eq!(
            current(
                &answer["result"]["configOptions"],
                methods::PERMISSIONS_CONFIG_ID
            ),
            YOLO
        );
        assert!(test.frames.is_empty(), "an unchanged list is not announced");
    }

    #[test]
    fn a_model_switch_reaches_the_agent() {
        let (mut test, models) = reasoning_session();
        let sid = test.sid.clone();

        test.server.handle_message(set_option(
            &sid,
            methods::MODEL_CONFIG_ID,
            json!({"value": OFFLINE_SPEC}),
        ));

        assert_eq!(models.try_recv().unwrap().spec(), OFFLINE_SPEC);
        assert_eq!(test.session().model.spec(), OFFLINE_SPEC);
    }

    #[test]
    fn the_next_prompt_runs_on_the_chosen_effort_and_fast_mode() {
        let mut test = TestServer::with_session();
        let inputs = agent_inputs(&mut test);
        let model = Model::from_spec(FAST_SPEC).unwrap();
        let level = ThinkingConfig::choices(&model)
            .pop()
            .expect("a fast model reasons");
        let session = test.session_mut();
        session.model = model;
        session.boolean_options = true;
        let sid = test.sid.clone();

        for (config_id, value) in [
            (
                methods::EFFORT_CONFIG_ID,
                json!({"value": level.to_string()}),
            ),
            (
                methods::FAST_CONFIG_ID,
                json!({"type": "boolean", "value": true}),
            ),
        ] {
            test.server
                .handle_message(set_option(&sid, config_id, value));
            assert_eq!(
                session_update_kind(&test.next_message()),
                CONFIG_OPTION_UPDATE
            );
            assert!(test.next_message()["error"].is_null());
        }
        test.server.handle_message(prompt(SECOND_PROMPT_ID, &sid));

        let input = inputs.try_recv().unwrap();
        assert_eq!(input.thinking, level);
        assert!(input.fast);
    }

    #[test_case("temperature", json!({"value": "hot"}), UNKNOWN_CONFIG_OPTION ; "unknown_option")]
    #[test_case(methods::EFFORT_CONFIG_ID, json!({"value": "turbo"}), UNKNOWN_EFFORT ; "undeclared_level")]
    #[test_case(methods::PERMISSIONS_CONFIG_ID, json!({"value": "root"}), UNKNOWN_PERMISSION_MODE ; "unknown_permission_mode")]
    #[test_case(methods::MODE_CONFIG_ID, json!({"type": "boolean", "value": true}), VALUE_ID_EXPECTED ; "boolean_for_a_select")]
    #[test_case(methods::FAST_CONFIG_ID, json!({"type": "boolean", "value": true}), UNKNOWN_CONFIG_OPTION ; "fast_where_it_is_not_offered")]
    fn an_invalid_option_is_refused_with_a_readable_reason(
        config_id: &str,
        value: Value,
        reason: &str,
    ) {
        let (mut test, _models) = reasoning_session();
        let sid = test.sid.clone();

        test.server
            .handle_message(set_option(&sid, config_id, value));

        let refused = test.next_message();
        assert_eq!(error_of(&refused).0, i32::from(ErrorCode::InvalidParams));
        assert!(
            refused["error"]["data"]
                .as_str()
                .unwrap()
                .starts_with(reason),
            "{refused}"
        );
        assert!(test.frames.is_empty(), "a refusal changes nothing");
    }

    #[test]
    fn effort_is_refused_on_a_model_that_cannot_reason() {
        let (mut test, _models) = reasoning_session();
        test.session_mut().model.thinking_override = Some(ThinkingSupport::No);
        let sid = test.sid.clone();

        test.server.handle_message(set_option(
            &sid,
            methods::EFFORT_CONFIG_ID,
            json!({"value": EFFORT_HIGH}),
        ));

        let refused = test.next_message();
        assert!(
            refused["error"]["data"]
                .as_str()
                .unwrap()
                .starts_with(UNKNOWN_CONFIG_OPTION)
        );
    }
}
