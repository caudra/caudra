use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::{iter, mem};

use agent_client_protocol_schema::v1::{
    AgentNotification, AgentRequest, CompleteElicitationNotification, ContentBlock, ElicitationId,
    InitializeRequest, RequestPermissionRequest, RequestPermissionResponse, SessionId,
    SessionUpdate, TextContent, ToolCallContent, ToolCallId, ToolCallUpdate, ToolCallUpdateFields,
};
use caudra_agent::ElicitAction;
use caudra_agent::mcp::call::{ElicitMode, ElicitRequest, ElicitResponse, Elicitor};
use caudra_agent::mcp::elicitation::ElicitForm;
use caudra_agent::mcp::elicitor::ElicitorFactory;
use caudra_agent::mcp::transport::BoxFuture;
use caudra_agent::permissions::{
    EngineFlag, PermissionAdvisory, PermissionAnswer, PermissionRequest as CaudraPermissionRequest,
};
use caudra_agent::tools::native::secret::{self, SecretRequest};
use caudra_agent::tools::{
    LocalToolFn, LocalTools, QUESTION_TOOL_NAME, REQUEST_SECRET_TOOL_NAME, ToolContext, ToolEffect,
    ToolError, ToolFailure, typed_local_tool,
};
use serde_json::Value;
use tracing::warn;

use crate::child_work::ChildTier;
use crate::outbound::{AskKind, Outbound, PendingState, next_request_id};
use crate::session::SessionState;
use crate::translate::OpenCalls;
use crate::{elicitation, locked, permissions};

/// The client `_meta` flag that asks for questions through
/// `session/request_permission` instead of form elicitation.
pub(crate) const CLIENT_QUESTIONS_VIA_PERMISSION: &str = "caudra/questionsViaPermission";
/// A client that relied on that convention before the flag existed, so it is
/// still matched by name.
pub(crate) const PERMISSION_QUESTION_CLIENT: &str = "openmausbot";
const DECISION_ADVISORY_GUIDANCE: &str = "Decision engine estimates do not authorize this action.";
/// Joins a call's id to a child's index: a `batch` or `code` child runs as
/// `<parent>:<n>`, and its own children append another `:<n>`.
const CHILD_CALL_SEPARATOR: char = ':';
const NO_SESSION: &str = "no session";
const NO_ANSWER_CHANNEL: &str = "no answer channel";

/// How the client said, on `initialize`, it can be asked things.
#[derive(Clone, Default)]
pub(crate) struct ClientProfile {
    pub(crate) elicits_form: bool,
    pub(crate) elicits_url: bool,
    pub(crate) asks_via_permission: bool,
    /// Whether the client renders `type: "boolean"` config options, which
    /// ACP lets an agent offer only to a client that said so.
    pub(crate) boolean_options: bool,
    pub(crate) updates: ClientUpdates,
    pub(crate) child_tier: ChildTier,
    /// Whether the client runs `type: "terminal"` auth methods.
    pub(crate) terminal_auth: bool,
}

/// The session updates ACP lets an agent send only to a client that said it
/// renders them.
#[derive(Clone, Default)]
pub(crate) struct ClientUpdates {
    pub(crate) compaction: bool,
    pub(crate) notices: bool,
}

impl ClientProfile {
    pub(crate) fn from_initialize(request: &InitializeRequest) -> Self {
        let capabilities = &request.client_capabilities;
        let session = capabilities.session.as_ref();
        let flagged = capabilities
            .meta
            .as_ref()
            .and_then(|meta| meta.get(CLIENT_QUESTIONS_VIA_PERMISSION))
            .and_then(Value::as_bool)
            == Some(true);
        let legacy = request
            .client_info
            .as_ref()
            .is_some_and(|info| info.name == PERMISSION_QUESTION_CLIENT);
        Self {
            updates: ClientUpdates {
                compaction: session.is_some_and(|session| session.compaction.is_some()),
                notices: session.is_some_and(|session| session.notices.is_some()),
            },
            elicits_form: elicitation::supports_form(capabilities),
            elicits_url: elicitation::supports_url(capabilities),
            asks_via_permission: flagged || legacy,
            boolean_options: session
                .and_then(|session| session.config_options.as_ref())
                .is_some_and(|options| options.boolean.is_some()),
            child_tier: ChildTier::of(capabilities),
            terminal_auth: capabilities.auth.terminal,
        }
    }

    /// With neither transport the question tool would spin forever waiting for
    /// a TUI that does not exist, so it is dropped and the model asks in plain
    /// text instead. A form renders the whole thing, so it wins when both are
    /// offered.
    ///
    /// `request_secret` needs a masked field, which only a form has, so a
    /// client without one gets the refusal a session without a front end does.
    pub(crate) fn question_tools(
        &self,
        outbound: &Outbound,
        pending: &PendingState,
    ) -> (Vec<&'static str>, LocalTools) {
        let secret = match self.elicits_form {
            true => secret_tool(outbound.clone(), Arc::clone(pending)),
            false => typed_local_tool(ToolEffect::Isolated, |_, _| {
                Box::pin(async { Err(ToolError::new(ToolFailure::Other, secret::NO_UI)) })
            }),
        };
        let mut tools = HashMap::from([(REQUEST_SECRET_TOOL_NAME.to_owned(), secret)]);
        if !self.elicits_form && !self.asks_via_permission {
            return (vec![QUESTION_TOOL_NAME], Arc::new(tools));
        }
        let question = question_tool(outbound.clone(), Arc::clone(pending), !self.elicits_form);
        tools.insert(QUESTION_TOOL_NAME.to_owned(), question);
        (Vec::new(), Arc::new(tools))
    }

    /// `None` for a client that renders no elicitation, so every MCP server's
    /// question is declined at once.
    pub(crate) fn mcp_elicitors(
        &self,
        outbound: &Outbound,
        pending: &PendingState,
    ) -> Option<Arc<dyn ElicitorFactory>> {
        let elicitors = McpElicitors {
            outbound: outbound.clone(),
            pending: Arc::clone(pending),
            form: self.elicits_form,
            url: self.elicits_url,
        };
        (elicitors.form || elicitors.url).then(|| Arc::new(elicitors) as Arc<dyn ElicitorFactory>)
    }
}

/// Shadows the Lua `question` tool: asks the client and blocks the tool call
/// until the answer comes back. Both transports settle on the interactive
/// answer channel, so they share `AskKind::Elicitation`; permission requests
/// proper use their broker.
fn question_tool(outbound: Outbound, pending: PendingState, via_permission: bool) -> LocalToolFn {
    typed_local_tool(ToolEffect::Unknown, move |input, ctx| {
        let outbound = outbound.clone();
        let pending = Arc::clone(&pending);
        Box::pin(async move {
            let session_id = ctx
                .session_id
                .as_ref()
                .map(ToString::to_string)
                .ok_or(NO_SESSION)?;
            let tool_call_id = shown_tool_call(&ctx);
            let request = if via_permission {
                AgentRequest::RequestPermissionRequest(
                    elicitation::question_permission_request(&session_id, tool_call_id, &input)
                        .map_err(unaskable)?,
                )
            } else {
                AgentRequest::CreateElicitationRequest(
                    elicitation::form_request(&session_id, tool_call_id, &input)
                        .map_err(unaskable)?,
                )
            };
            let rx = ctx.user_response_rx.as_ref().ok_or(NO_ANSWER_CHANNEL)?;

            let guard = rx.lock().await;
            let id = outbound.ask(&pending, AskKind::Elicitation, request);
            let response = ctx.cancel.race(guard.recv_async()).await;
            // Cleared while still holding the channel, so a stale id cannot
            // clobber whatever ask comes next.
            let _ = locked(&pending).asks.remove(&id);
            drop(guard);

            let Ok(Ok(raw)) = response else {
                return Ok(elicitation::DISMISSED.to_owned());
            };
            Ok(if via_permission {
                elicitation::format_permission_answer(&input, &raw)
            } else {
                elicitation::format_response(&input, &raw)
            })
        })
    })
}

/// Shadows the native `request_secret`: the same checks and the same vault,
/// with the card rendered as the client's form. The client's answer settles on
/// the interactive answer channel like a question's, and the value goes
/// straight into the vault.
fn secret_tool(outbound: Outbound, pending: PendingState) -> LocalToolFn {
    typed_local_tool(ToolEffect::Isolated, move |input, ctx| {
        let outbound = outbound.clone();
        let pending = Arc::clone(&pending);
        Box::pin(async move {
            let request = SecretRequest::parse(&input)
                .map_err(|error| ToolError::new(ToolFailure::InvalidInput, error.to_string()))?;
            let secrets = secret::local_vault(&ctx)
                .map_err(|refusal| ToolError::new(ToolFailure::Other, refusal))?;
            let session_id = ctx
                .session_id
                .as_ref()
                .map(ToString::to_string)
                .ok_or(NO_SESSION)?;
            let rx = ctx
                .user_response_rx
                .as_ref()
                .ok_or_else(|| ToolError::new(ToolFailure::Other, secret::NO_UI))?;
            let create = elicitation::secret_request(
                &session_id,
                shown_tool_call(&ctx),
                &request,
                secrets.contains(&request.name),
            );
            let guard = rx.lock().await;
            let id = outbound.ask(
                &pending,
                AskKind::Elicitation,
                AgentRequest::CreateElicitationRequest(create),
            );
            let response = ctx.cancel.race(guard.recv_async()).await;
            let _ = locked(&pending).asks.remove(&id);
            drop(guard);
            let Ok(Ok(raw)) = response else {
                return Err(ToolError::new(ToolFailure::Cancelled, secret::CANCELLED));
            };
            secret::store(secrets, &request.name, elicitation::secret_reply(&raw))
                .map_err(|error| ToolError::new(ToolFailure::InvalidInput, error.to_string()))
        })
    })
}

/// Questions this client cannot render are the model's to rephrase.
fn unaskable(message: String) -> ToolError {
    ToolError::new(ToolFailure::InvalidInput, message)
}

/// Forwards MCP servers' elicitations to the client, in the modes it
/// declared it renders.
#[derive(Clone)]
pub(crate) struct McpElicitors {
    pub(crate) outbound: Outbound,
    pub(crate) pending: PendingState,
    pub(crate) form: bool,
    pub(crate) url: bool,
}

pub(crate) struct McpElicitor {
    client: McpElicitors,
    session_id: String,
    tool_call_id: Option<String>,
    /// The id each URL elicitation was forwarded under, by its server and the
    /// server's own id, so the server's completion notice reaches the client.
    forwarded_urls: Mutex<HashMap<(String, String), String>>,
}

impl McpElicitor {
    pub(crate) fn new(
        client: McpElicitors,
        session_id: String,
        tool_call_id: Option<String>,
    ) -> Self {
        Self {
            client,
            session_id,
            tool_call_id,
            forwarded_urls: Mutex::default(),
        }
    }
}

/// Forgets an ask whose wait was dropped, so its late result is discarded.
struct ForgetAsk<'a> {
    pending: &'a PendingState,
    id: i64,
}

impl Drop for ForgetAsk<'_> {
    fn drop(&mut self) {
        locked(self.pending).asks.remove(&self.id);
    }
}

impl ElicitorFactory for McpElicitors {
    /// A call without the answer channel is declined, as in the terminal. A
    /// subagent's call is scoped to the parent's tool call, the only one of
    /// its calls the client has seen.
    fn for_call(&self, ctx: &ToolContext) -> Option<Arc<dyn Elicitor>> {
        ctx.user_response_rx.as_ref()?;
        Some(Arc::new(McpElicitor::new(
            self.clone(),
            ctx.session_id.as_ref()?.to_string(),
            shown_tool_call(ctx),
        )))
    }
}

/// The client never sees a subagent's own calls, nor a `batch` or `code`
/// child's, so either is scoped to the root call the client was shown.
fn shown_tool_call(ctx: &ToolContext) -> Option<String> {
    ctx.root_tool_use_id
        .as_ref()
        .or(ctx.tool_use_id.as_ref())
        .filter(|id| !id.is_empty())
        .cloned()
}

impl Elicitor for McpElicitor {
    fn elicit<'a>(
        &'a self,
        server: &'a str,
        request: ElicitRequest,
    ) -> BoxFuture<'a, ElicitResponse> {
        Box::pin(async move {
            let (rendered, form) = match &request.mode {
                ElicitMode::Form { schema } => (self.client.form, ElicitForm::parse(schema).ok()),
                ElicitMode::Url { .. } => (self.client.url, None),
            };
            if !rendered {
                return ElicitResponse::unanswered(ElicitAction::Decline);
            }
            let forwarded_id = next_request_id().to_string();
            let create = match elicitation::mcp_request(
                &self.session_id,
                self.tool_call_id.clone(),
                forwarded_id.clone(),
                &request,
            ) {
                Ok(create) => create,
                Err(error) => {
                    warn!(server, %error, "declined an MCP form the client cannot be sent");
                    return ElicitResponse::unanswered(ElicitAction::Decline);
                }
            };
            if let ElicitMode::Url {
                elicitation_id: Some(server_id),
                ..
            } = request.mode
            {
                locked(&self.forwarded_urls).insert((server.to_owned(), server_id), forwarded_id);
            }
            let (answer_tx, answer) = flume::bounded(1);
            let id = self.client.outbound.ask(
                &self.client.pending,
                AskKind::McpElicitation(answer_tx),
                AgentRequest::CreateElicitationRequest(create),
            );
            let _forget = ForgetAsk {
                pending: &self.client.pending,
                id,
            };
            let result = answer.recv_async().await.unwrap_or(Value::Null);
            elicitation::mcp_response(result, form.as_ref())
        })
    }

    fn url_completed(&self, server: &str, elicitation_id: &str) {
        let key = (server.to_owned(), elicitation_id.to_owned());
        if let Some(forwarded_id) = locked(&self.forwarded_urls).remove(&key) {
            self.client
                .outbound
                .notify(AgentNotification::CompleteElicitationNotification(
                    CompleteElicitationNotification::new(ElicitationId::new(forwarded_id)),
                ));
        }
    }
}

/// Hands the client's answer to whatever in `session` waits on it.
pub(crate) fn settle(session: &SessionState, kind: AskKind, raw: &Value) {
    match kind {
        AskKind::Permission {
            request_id,
            exact_project_deny,
        } => {
            let permissions = &session.handle.permissions;
            let answer = permission_answer(raw, exact_project_deny);
            if !permissions.answer(&request_id, answer)
                && permissions.pending_request(&request_id).is_some()
            {
                warn!(%request_id, "permission response failed; denying request");
                permissions.answer(&request_id, PermissionAnswer::Deny);
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
        AskKind::McpElicitation(answer) => {
            let _ = answer.send(raw.get("result").cloned().unwrap_or(Value::Null));
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

pub(crate) fn permission_scope_summary(scopes: &[String]) -> String {
    format!(
        "Permission scopes:\n{}",
        serde_json::to_string_pretty(scopes).expect("strings always serialize")
    )
}

/// The announced call a request belongs to. A `batch` or `code` child asks
/// under `<parent>:<n>`, an id the client never saw, so it is attributed to
/// the nearest ancestor that was announced; an id with none passes through.
pub(crate) fn shown_call<'a>(announced: &OpenCalls, id: &'a str) -> &'a str {
    iter::successors(Some(id), |call| {
        call.rsplit_once(CHILD_CALL_SEPARATOR)
            .map(|(parent, _)| parent)
    })
    .find(|call| announced.contains_key(*call))
    .unwrap_or(id)
}

/// The request keeps its own title, content, and input, so the user approves
/// the inner call even when it is shown on its parent's card.
pub(crate) fn request_permission(
    outbound: &Outbound,
    pending: &PendingState,
    sid: &SessionId,
    announced: &OpenCalls,
    request: CaudraPermissionRequest,
) {
    let tool_call_id = shown_call(announced, &request.id).to_owned();
    ask_permission(outbound, pending, sid, tool_call_id, request);
}

/// Asks the client about `request` on the card `tool_call_id` names.
pub(crate) fn ask_permission(
    outbound: &Outbound,
    pending: &PendingState,
    sid: &SessionId,
    tool_call_id: String,
    request: CaudraPermissionRequest,
) {
    let fields = ToolCallUpdateFields::new()
        .title(request.presentation.action.clone())
        .content(permission_content(&request))
        .raw_input(request.input.clone());
    let tool_call_id = ToolCallId::from(tool_call_id);
    let client_request = AgentRequest::RequestPermissionRequest(RequestPermissionRequest::new(
        sid.clone(),
        ToolCallUpdate::new(tool_call_id, fields),
        permissions::permission_options(),
    ));
    let kind = AskKind::Permission {
        request_id: request.id.clone(),
        exact_project_deny: permissions::exact_project_deny_is_representable(&request),
    };
    outbound.ask(pending, kind, client_request);
}

fn permission_content(request: &CaudraPermissionRequest) -> Vec<ToolCallContent> {
    let mut text = request
        .mcp
        .as_ref()
        .and_then(|facts| facts.note.as_deref())
        .map(|note| format!("{note}\n\n"))
        .unwrap_or_default();
    text.push_str(&permission_scope_summary(&request.scopes));
    let mut has_advisories = false;
    for flag in EngineFlag::ALL {
        if let Some(caution) = request
            .presentation
            .advisories
            .iter()
            .filter(|advisory| advisory.flag == flag)
            .find_map(PermissionAdvisory::summary)
        {
            text.push_str(&format!("\n\nDecision engine caution: {caution}."));
            has_advisories = true;
        }
    }
    if has_advisories {
        text.push_str("\n\n");
        text.push_str(DECISION_ADVISORY_GUIDANCE);
    }
    vec![ToolCallContent::from(ContentBlock::Text(TextContent::new(
        text,
    )))]
}

pub(crate) fn update_presented_permission(
    outbound: &Outbound,
    pending: &PendingState,
    sid: &SessionId,
    announced: &OpenCalls,
    request: &CaudraPermissionRequest,
) {
    let tool_call_id = shown_call(announced, &request.id).to_owned();
    present_permission_update(outbound, pending, sid, tool_call_id, request);
}

/// Shows the client what changed about a request it is still asking about.
pub(crate) fn present_permission_update(
    outbound: &Outbound,
    pending: &PendingState,
    sid: &SessionId,
    tool_call_id: String,
    request: &CaudraPermissionRequest,
) {
    let active = locked(pending).asks.values().any(
        |kind| matches!(kind, AskKind::Permission { request_id, .. } if request_id == &request.id),
    );
    if active {
        outbound.session_update(
            sid,
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                ToolCallId::from(tool_call_id),
                ToolCallUpdateFields::new().content(permission_content(request)),
            )),
        );
    }
}

pub(crate) fn resolve_presented_permission(
    outbound: &Outbound,
    pending: &PendingState,
    request_id: &str,
) {
    let ids = {
        let mut pending = locked(pending);
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
        outbound.cancel_request(id);
    }
}

/// Takes back every question `session` still has before its client: the
/// client is told each is withdrawn, and whatever waits on one hears a deny
/// or a cancel. A question tool's wait ends with the cancelled run instead,
/// since an answer it no longer reads would reach its next question.
pub(crate) fn withdraw_asks(session: &SessionState, outbound: &Outbound) {
    let asks = mem::take(&mut locked(&session.pending).asks);
    for (id, kind) in asks {
        match kind {
            AskKind::ResolvedPermission => continue,
            AskKind::Permission { request_id, .. } => {
                session
                    .handle
                    .permissions
                    .answer(&request_id, PermissionAnswer::Deny);
            }
            AskKind::Elicitation => {}
            AskKind::McpElicitation(answer) => {
                let _ = answer.send(Value::Null);
            }
        }
        outbound.cancel_request(id);
    }
}

#[cfg(test)]
mod tests {
    use caudra_agent::permissions::PermissionLifetime;
    use caudra_agent::secrets::SecretVaults;
    use caudra_agent::tools::test_support::stub_ctx;
    use caudra_agent::{AgentMode, McpCallFacts, McpServerKind};
    use caudra_storage::StateDir;
    use caudra_storage::id::{CaudraId, SessionRef};
    use serde_json::json;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::outbound::CANCEL_REQUEST_METHOD;
    use crate::test_support::{
        ADVISORY_PROBABILITY, ANSWERED_ID, CAUDRA_REQUEST_ID, DELETE_CAUTION, MCP_TOOL_CALL,
        advisory_request, allow_once, mcp_form, selected_permission,
    };

    const UPLOAD_CAUTION: &str = "Decision engine caution: May upload or send data (88%).";
    const OTHER_CLIENT: &str = "zed";
    const CLIENT_VERSION: &str = "0.0.0";
    const MCP_NOTE: &str = "Home Assistant lock devices always ask";
    const MCP_SERVER: &str = "deploys";
    const MCP_URL: &str = "https://example.com/authorize";
    const MCP_ELICITATION_ID: &str = "server-elicitation-1";
    const OTHER_MCP_SERVER: &str = "billing";
    const ELICITATION_COMPLETE_METHOD: &str = "elicitation/complete";
    const MCP_TASK_CALL: &str = "toolu_task";
    const BATCH_CHILD_CALL: &str = "";
    const MCP_CHILD_CALL: &str = "toolu_mcp:0";
    const NESTED_CHILD_CALL: &str = "toolu_mcp:0:1";
    const UNANNOUNCED_CHILD_CALL: &str = "toolu_unseen:0";
    /// Some providers mint ids that already contain the separator.
    const COLON_CALL: &str = "functions.batch:3";
    const COLON_CALL_CHILD: &str = "functions.batch:3:0";

    #[test_case(EngineFlag::Deletes, ADVISORY_PROBABILITY, Some(DELETE_CAUTION); "known_caution")]
    #[test_case(EngineFlag::Uploads, ADVISORY_PROBABILITY, Some(UPLOAD_CAUTION); "upload_caution")]
    #[test_case(EngineFlag::Deletes, f64::NAN, None; "nan")]
    #[test_case(EngineFlag::Deletes, f64::INFINITY, None; "infinity")]
    #[test_case(EngineFlag::Deletes, -0.1, None; "negative")]
    #[test_case(EngineFlag::Deletes, 1.1, None; "over_one")]
    fn advisory_content_is_bounded_caution_text(
        flag: EngineFlag,
        probability: f64,
        caution: Option<&str>,
    ) {
        let request = advisory_request(flag, probability);
        let content = serde_json::to_value(permission_content(&request)).unwrap();
        let text = content[0]["content"]["text"].as_str().unwrap();
        if let Some(caution) = caution {
            assert!(text.contains(caution));
            assert!(text.contains(DECISION_ADVISORY_GUIDANCE));
        } else {
            assert_eq!(text, permission_scope_summary(&request.scopes));
        }
    }

    #[test]
    fn mcp_note_leads_the_permission_text() {
        let mut request = advisory_request(EngineFlag::Deletes, f64::NAN);
        request.mcp = Some(McpCallFacts {
            kind: McpServerKind::HomeAssistant,
            read_only: false,
            note: Some(MCP_NOTE.into()),
        });
        let content = serde_json::to_value(permission_content(&request)).unwrap();
        let text = content[0]["content"]["text"].as_str().unwrap();
        assert_eq!(
            text,
            format!(
                "{MCP_NOTE}\n\n{}",
                permission_scope_summary(&request.scopes)
            )
        );
    }

    #[test]
    fn advisory_updates_do_not_replace_permission_authority_or_reopen_resolved_prompts() {
        let (outbound, out_rx) = Outbound::channel();
        let pending = PendingState::default();
        let sid = SessionId::from(SessionRef::generate().to_string());
        let announced = OpenCalls::new();
        let mut request = advisory_request(EngineFlag::Deletes, ADVISORY_PROBABILITY);
        request_permission(&outbound, &pending, &sid, &announced, request.clone());
        let initial = out_rx.try_recv().unwrap().message;
        assert!(
            initial["params"]["toolCall"]["content"][0]["content"]["text"]
                .as_str()
                .unwrap()
                .contains(DELETE_CAUTION)
        );
        let pending_id = *locked(&pending).asks.keys().next().unwrap();
        request.presentation.advisories[0].flag = EngineFlag::Uploads;
        update_presented_permission(&outbound, &pending, &sid, &announced, &request);
        let updated = out_rx.try_recv().unwrap().message;
        assert_eq!(updated["method"], "session/update");
        assert_eq!(updated["params"]["update"]["toolCallId"], CAUDRA_REQUEST_ID);
        assert!(updated["params"]["update"]["rawInput"].is_null());
        assert_eq!(locked(&pending).asks.len(), 1);
        assert!(locked(&pending).asks.contains_key(&pending_id));
        resolve_presented_permission(&outbound, &pending, CAUDRA_REQUEST_ID);
        assert_eq!(
            out_rx.try_recv().unwrap().message["method"],
            CANCEL_REQUEST_METHOD
        );
        update_presented_permission(&outbound, &pending, &sid, &announced, &request);
        assert!(out_rx.is_empty());
    }

    /// The client approves on a card it has, but answers the broker's own
    /// request, and reads what it approves from the inner call.
    #[test_case(MCP_TOOL_CALL, MCP_TOOL_CALL ; "an_announced_call_keeps_its_id")]
    #[test_case(MCP_CHILD_CALL, MCP_TOOL_CALL ; "a_child_is_shown_on_its_parent")]
    #[test_case(NESTED_CHILD_CALL, MCP_TOOL_CALL ; "a_nested_child_is_shown_on_its_root")]
    #[test_case(COLON_CALL, COLON_CALL ; "an_announced_id_with_a_separator_keeps_its_id")]
    #[test_case(COLON_CALL_CHILD, COLON_CALL ; "a_child_of_an_id_with_a_separator")]
    #[test_case(UNANNOUNCED_CHILD_CALL, UNANNOUNCED_CHILD_CALL ; "an_unannounced_id_passes_through")]
    fn a_permission_request_is_shown_on_the_call_the_client_saw(request_id: &str, shown: &str) {
        let (outbound, out_rx) = Outbound::channel();
        let pending = PendingState::default();
        let sid = SessionId::from(SessionRef::generate().to_string());
        let announced = OpenCalls::from([
            (MCP_TOOL_CALL.to_owned(), Arc::from(MCP_TOOL_CALL)),
            (COLON_CALL.to_owned(), Arc::from(COLON_CALL)),
        ]);
        let mut request = advisory_request(EngineFlag::Deletes, ADVISORY_PROBABILITY);
        request.id = request_id.to_owned();

        request_permission(&outbound, &pending, &sid, &announced, request.clone());
        let asked = out_rx.try_recv().unwrap().message;
        assert_eq!(asked["params"]["toolCall"]["toolCallId"], shown);
        assert_eq!(
            asked["params"]["toolCall"]["title"],
            request.presentation.action
        );
        assert_eq!(asked["params"]["toolCall"]["rawInput"], request.input);
        assert!(matches!(
            locked(&pending).asks.values().next(),
            Some(AskKind::Permission { request_id: asked_for, .. }) if asked_for == request_id
        ));

        update_presented_permission(&outbound, &pending, &sid, &announced, &request);
        let updated = out_rx.try_recv().unwrap().message;
        assert_eq!(updated["params"]["update"]["toolCallId"], shown);
    }

    fn allow_exact(lifetime: PermissionLifetime) -> PermissionAnswer {
        PermissionAnswer::AllowOption {
            option_id: "allow_exact".into(),
            lifetime,
        }
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

    fn url_elicitor(outbound: Outbound, pending: PendingState, url: bool) -> McpElicitor {
        McpElicitor::new(
            McpElicitors {
                outbound,
                pending,
                form: true,
                url,
            },
            SessionRef::generate().to_string(),
            Some(MCP_TOOL_CALL.into()),
        )
    }

    fn url_request() -> ElicitRequest {
        ElicitRequest {
            mode: ElicitMode::Url {
                url: MCP_URL.into(),
                elicitation_id: Some(MCP_ELICITATION_ID.into()),
            },
            ..mcp_form()
        }
    }

    #[test]
    fn an_mcp_elicitation_in_a_mode_the_client_lacks_is_declined_unsent() {
        let (outbound, out_rx) = Outbound::channel();
        let elicitor = url_elicitor(outbound, PendingState::default(), false);

        let response = smol::block_on(elicitor.elicit(MCP_SERVER, url_request()));

        assert_eq!(response, ElicitResponse::unanswered(ElicitAction::Decline));
        assert!(out_rx.is_empty());
    }

    /// The client learns the browser step is over once, under the id it was
    /// shown, and only for the server that asked.
    #[test]
    fn a_completed_url_elicitation_is_reported_to_the_client_once() {
        let (outbound, out_rx) = Outbound::channel();
        let pending = PendingState::default();
        let elicitor = url_elicitor(outbound, Arc::clone(&pending), true);

        let (response, forwarded) = smol::block_on(smol::future::zip(
            elicitor.elicit(MCP_SERVER, url_request()),
            async {
                let asked = out_rx.recv_async().await.unwrap().message;
                let Some(AskKind::McpElicitation(answer)) =
                    locked(&pending).asks.remove(&asked["id"].as_i64().unwrap())
                else {
                    panic!("an MCP elicitation is outstanding");
                };
                answer
                    .send(serde_json::json!({"action": "accept"}))
                    .unwrap();
                asked["params"]["elicitationId"].clone()
            },
        ));
        assert_eq!(response.action, ElicitAction::Accept);

        elicitor.url_completed(OTHER_MCP_SERVER, MCP_ELICITATION_ID);
        assert!(out_rx.is_empty(), "another server's notice is not ours");
        elicitor.url_completed(MCP_SERVER, MCP_ELICITATION_ID);
        let completed = out_rx.try_recv().unwrap().message;
        assert_eq!(completed["method"], ELICITATION_COMPLETE_METHOD);
        assert_eq!(completed["params"]["elicitationId"], forwarded);
        elicitor.url_completed(MCP_SERVER, MCP_ELICITATION_ID);
        assert!(out_rx.is_empty(), "a repeated notice is reported once");
    }

    #[test_case(None, Some(MCP_TOOL_CALL) => Some(MCP_TOOL_CALL.to_owned()) ; "the_main_agents_own")]
    #[test_case(Some(MCP_TASK_CALL), Some(MCP_TOOL_CALL) => Some(MCP_TASK_CALL.to_owned()) ; "a_subagents_task")]
    #[test_case(None, Some(BATCH_CHILD_CALL) => None ; "none_for_a_batch_child")]
    #[test_case(Some(MCP_TOOL_CALL), Some(MCP_CHILD_CALL) => Some(MCP_TOOL_CALL.to_owned()) ; "an_mcp_childs_root")]
    fn an_mcp_elicitation_is_scoped_to_a_call_the_client_saw(
        root: Option<&str>,
        call: Option<&str>,
    ) -> Option<String> {
        let mut ctx = stub_ctx(&AgentMode::Build);
        ctx.root_tool_use_id = root.map(str::to_owned);
        ctx.tool_use_id = call.map(str::to_owned);
        shown_tool_call(&ctx)
    }

    #[test_case(false, false => false ; "neither_mode")]
    #[test_case(true, false => true ; "form")]
    #[test_case(false, true => true ; "url")]
    fn only_a_client_that_renders_elicitation_is_asked(form: bool, url: bool) -> bool {
        let client = ClientProfile {
            elicits_form: form,
            elicits_url: url,
            ..ClientProfile::default()
        };
        client
            .mcp_elicitors(&Outbound::channel().0, &PendingState::default())
            .is_some()
    }

    #[test_case(json!({}), None => false ; "a_plain_client")]
    #[test_case(json!({"_meta": {CLIENT_QUESTIONS_VIA_PERMISSION: true}}), None => true ; "the_meta_flag")]
    #[test_case(json!({"_meta": {CLIENT_QUESTIONS_VIA_PERMISSION: "yes"}}), None => false ; "a_flag_that_is_not_boolean")]
    #[test_case(json!({}), Some(PERMISSION_QUESTION_CLIENT) => true ; "the_legacy_client_name")]
    #[test_case(json!({}), Some(OTHER_CLIENT) => false ; "another_client_name")]
    fn questions_ride_permission_requests_when_the_client_asks(
        capabilities: Value,
        client: Option<&str>,
    ) -> bool {
        let mut request = json!({"protocolVersion": 1, "clientCapabilities": capabilities});
        if let Some(name) = client {
            request["clientInfo"] = json!({"name": name, "version": CLIENT_VERSION});
        }
        let profile = ClientProfile::from_initialize(&serde_json::from_value(request).unwrap());
        let (_, tools) = profile.question_tools(&Outbound::channel().0, &PendingState::default());
        tools.contains_key(QUESTION_TOOL_NAME)
    }

    const SECRET_NAME: &str = "GH_TOKEN";
    const SECRET_CANARY: &str = "secret-canary-4711";
    const SECRET_LEAKED: &str = "the secret's value must reach only the vault";
    const FORM_MODE: &str = "form";

    fn secret_input() -> Value {
        json!({
            "name": SECRET_NAME,
            "label": "GitHub token",
            "reason": "gh needs it to open the pull request",
        })
    }

    /// A session context with a vault and an answer channel, and the sender
    /// the client's answers settle on.
    fn secret_ctx(state: &StateDir) -> (ToolContext, flume::Sender<String>) {
        let mut ctx = stub_ctx(&AgentMode::Build);
        ctx.session_id = Some(SessionRef::generate());
        ctx.tool_use_id = Some(MCP_TOOL_CALL.into());
        ctx.secrets = Some(SecretVaults::for_session(state, CaudraId::generate()));
        let (answers, rx) = flume::unbounded();
        ctx.user_response_rx = Some(Arc::new(smol::lock::Mutex::new(rx)));
        (ctx, answers)
    }

    #[test]
    fn a_secret_asked_through_a_form_lands_only_in_the_vault() {
        let temp = TempDir::new().unwrap();
        let state = StateDir::from_path(temp.path().to_path_buf());
        let (outbound, out_rx) = Outbound::channel();
        let pending = PendingState::default();
        let client = ClientProfile {
            elicits_form: true,
            ..ClientProfile::default()
        };
        let (_, tools) = client.question_tools(&outbound, &pending);
        let (ctx, answers) = secret_ctx(&state);
        let secrets = ctx.secrets.clone().unwrap();

        let (result, asked) = smol::block_on(smol::future::zip(
            tools[REQUEST_SECRET_TOOL_NAME].call(secret_input(), ctx),
            async {
                let asked = out_rx.recv_async().await.unwrap().message;
                let answer = json!({
                    "action": "accept",
                    "content": {elicitation::SECRET_FIELD: SECRET_CANARY},
                });
                answers.send(answer.to_string()).unwrap();
                asked
            },
        ));

        let text = result.unwrap();
        assert_eq!(asked["params"]["mode"], FORM_MODE, "{asked}");
        assert!(!text.contains(SECRET_CANARY), "{SECRET_LEAKED}: {text}");
        assert!(text.contains(SECRET_NAME), "{text}");
        let injected = secrets
            .overlay()
            .into_iter()
            .find(|(name, _)| name == SECRET_NAME)
            .map(|(_, value)| value.into_string().unwrap());
        assert_eq!(injected.as_deref(), Some(SECRET_CANARY));
    }

    #[test_case(false ; "a_plain_client")]
    #[test_case(true ; "a_permission_only_client")]
    fn a_client_without_a_form_refuses_a_secret_unasked(via_permission: bool) {
        let temp = TempDir::new().unwrap();
        let state = StateDir::from_path(temp.path().to_path_buf());
        let (outbound, out_rx) = Outbound::channel();
        let client = ClientProfile {
            asks_via_permission: via_permission,
            ..ClientProfile::default()
        };
        let (_, tools) = client.question_tools(&outbound, &PendingState::default());
        let (ctx, _answers) = secret_ctx(&state);

        let error =
            smol::block_on(tools[REQUEST_SECRET_TOOL_NAME].call(secret_input(), ctx)).unwrap_err();

        assert_eq!(error.message, secret::NO_UI);
        assert_eq!(error.failure, ToolFailure::Other);
        assert!(out_rx.is_empty());
    }
}
