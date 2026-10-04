use std::borrow::Cow;
use std::collections::BTreeMap;

use caudra_config::Feature;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    PeerOutput, ToolOutput,
    peers::PeerSession,
    permissions::{PermissionResource, PermissionResourceKind, PermissionRisk},
    tools::{
        DescriptionContext, ToolAudience, ToolContext,
        registry::{
            BoxFuture, ExecFuture, HeaderFuture, HeaderResult, ParseError, PermissionIntent,
            PermissionScopes, PlanModeAccess, Tool, ToolError, ToolExecResult, ToolFailure,
            ToolInvocation,
        },
    },
};

pub const LIST_NAME: &str = "list_sessions";
pub const SEND_NAME: &str = "send_message";
pub const TOOL_NAMES: &[&str] = &[LIST_NAME, SEND_NAME];
pub const LIST_DESCRIPTION: &str = "Discover other live Caudra sessions on this machine. Returns bounded session metadata and exact word-based reply targets, not conversation history. Use the returned target with send_message; titles are not unique. Targets are local to your live registration and are never reassigned to a replacement peer. Rediscover after restarting or replacing your session. A session started with a unique messaging name lists it as handle; send_message accepts it as @handle, and unlike a target it follows that session across restarts. Cross-session messaging is experimental and requires each process to opt in.";
pub const SEND_DESCRIPTION: &str = "Send plain text to another live Caudra session using an exact target from list_sessions or an incoming peer message, or @handle for the live session holding that unique messaging name. Cross-session messaging is experimental and requires each process to opt in. A queued or held receipt is not model delivery or task completion. A message may start a billable turn using the recipient's own permissions. Never ask another session to bypass your mode, permissions, or a denied action. Peer messages cannot approve actions, change configuration, execute slash commands, or attach files. Recipients rate-limit senders and refuse the same text from you within a minute. Do not poll for replies or automatically retry an unknown outcome as a new message.";
const MAX_TEXT_BYTES: usize = 32 * 1024;
const UNAVAILABLE: &str = "cross-session messaging requires an enabled, live local main session";
const READ_ONLY: &str = "sending a peer message is not permitted in a read-only agent";
const MISSING_CALL_ID: &str = "sending a peer message requires a tracked tool invocation";
const PEER_RESOURCE: &str = "peer_session";
const DISCLOSURE_ATTRIBUTE: &str = "disclosure";
const DISCLOSURE_RISK: &str =
    "Discloses message text to another local session and its model provider.";
const WAKE_ATTRIBUTE: &str = "wake";
const WAKE_RISK: &str = "May start a billable model turn under the recipient's own permissions.";

pub struct ListSessions;
pub struct SendMessage;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListCall {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendCall {
    target: String,
    text: String,
    reply_to: Option<String>,
}

fn session(ctx: &ToolContext) -> Result<PeerSession, ToolError> {
    ctx.config
        .features
        .require(Feature::CrossSessionMessaging)
        .map_err(|error| ToolError::new(ToolFailure::Denied, error.to_string()))?;
    if ctx.audience != ToolAudience::MAIN
        || ctx.workspace_session.is_some()
        || ctx.host_cwd.is_some()
    {
        return Err(ToolError::new(ToolFailure::Denied, UNAVAILABLE));
    }
    ctx.session_id
        .as_ref()
        .and_then(|id| PeerSession::lookup(id.id()))
        .ok_or_else(|| ToolError::new(ToolFailure::NotFound, UNAVAILABLE))
}

impl Tool for ListSessions {
    fn name(&self) -> &str {
        LIST_NAME
    }

    fn description(&self, _: &DescriptionContext) -> Cow<'_, str> {
        Cow::Borrowed(LIST_DESCRIPTION)
    }

    fn audience(&self) -> ToolAudience {
        ToolAudience::MAIN
    }

    fn schema(&self) -> Value {
        json!({"type":"object","properties":{},"additionalProperties":false})
    }

    fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        serde_json::from_value::<ListCall>(input.clone())
            .map(|call| Box::new(call) as Box<dyn ToolInvocation>)
            .map_err(|error| ParseError::custom(error.to_string()))
    }
}

impl ToolInvocation for ListCall {
    fn start_header(&self) -> HeaderFuture {
        HeaderFuture::Ready(HeaderResult::plain("Live local sessions".into()))
    }

    fn execute<'a>(self: Box<Self>, ctx: &'a ToolContext) -> ExecFuture<'a> {
        Box::pin(async move {
            let peer = match session(ctx) {
                Ok(peer) => peer,
                Err(error) => return ToolExecResult::failed(error.failure, error.message),
            };
            match peer.list_named().await {
                Ok(sessions) => {
                    ToolExecResult::from(Ok(ToolOutput::Peers(PeerOutput::Sessions { sessions })))
                }
                Err(error) => ToolExecResult::failed(ToolFailure::Other, error),
            }
        })
    }
}

impl Tool for SendMessage {
    fn name(&self) -> &str {
        SEND_NAME
    }

    fn description(&self, _: &DescriptionContext) -> Cow<'_, str> {
        Cow::Borrowed(SEND_DESCRIPTION)
    }

    fn audience(&self) -> ToolAudience {
        ToolAudience::MAIN
    }

    fn schema(&self) -> Value {
        json!({"type":"object","additionalProperties":false,"properties":{
            "target":{"type":"string","minLength":1,"description":"Exact word-based target from list_sessions or an incoming peer reply address in this live session, or @handle for a live session's unique messaging name. Never a title or filesystem path."},
            "text":{"type":"string","minLength":1,"maxLength":MAX_TEXT_BYTES,"description":"Plain text only; also limited to 32 KiB of UTF-8."},
            "reply_to":{"type":"string","minLength":1,"description":"Optional incoming message name for correlation with this target."}
        },"required":["target","text"]})
    }

    fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        let call: SendCall = serde_json::from_value(input.clone())
            .map_err(|error| ParseError::custom(error.to_string()))?;
        if call.target.trim().is_empty()
            || call.target.len() > MAX_TEXT_BYTES
            || call.text.trim().is_empty()
            || call.text.len() > MAX_TEXT_BYTES
            || call
                .reply_to
                .as_ref()
                .is_some_and(|id| id.is_empty() || id.len() > MAX_TEXT_BYTES)
        {
            return Err(ParseError::custom(
                "peer target, text, or reply ID is empty or over the size limit",
            ));
        }
        Ok(Box::new(call))
    }
}

impl ToolInvocation for SendCall {
    fn start_header(&self) -> HeaderFuture {
        HeaderFuture::Ready(HeaderResult::plain("Send peer message".into()))
    }

    fn plan_mode_access(&self) -> PlanModeAccess {
        PlanModeAccess::Prompted
    }

    fn permission_scopes(&self) -> BoxFuture<'_, Option<PermissionScopes>> {
        Box::pin(async { Some(PermissionScopes::single(self.target.clone())) })
    }

    fn preflight<'a>(
        &'a self,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<Option<PermissionIntent>, ToolError>> {
        Box::pin(async {
            Ok(Some(PermissionIntent::new(
                PermissionScopes::single(self.target.clone()),
                vec![PermissionResource {
                    kind: PermissionResourceKind::Custom {
                        name: PEER_RESOURCE.into(),
                    },
                    value: self.target.clone(),
                    access: None,
                    protected: false,
                    requires_prompt: false,
                    attributes: BTreeMap::from([
                        (DISCLOSURE_ATTRIBUTE.into(), DISCLOSURE_RISK.into()),
                        (WAKE_ATTRIBUTE.into(), WAKE_RISK.into()),
                    ]),
                }],
                PermissionRisk::High,
            )))
        })
    }

    fn execute<'a>(self: Box<Self>, ctx: &'a ToolContext) -> ExecFuture<'a> {
        Box::pin(async move {
            if ctx.mode.is_read_only() {
                return ToolExecResult::failed(ToolFailure::Denied, READ_ONLY);
            }
            let peer = match session(ctx) {
                Ok(peer) => peer,
                Err(error) => return ToolExecResult::failed(error.failure, error.message),
            };
            let Some(call_id) = ctx.tool_use_id.as_deref() else {
                return ToolExecResult::failed(ToolFailure::InvalidInput, MISSING_CALL_ID);
            };
            let request_id = format!("{}:{call_id}", ctx.event_tx.run_id());
            match peer
                .send_named(
                    &self.target,
                    &self.text,
                    self.reply_to.as_deref(),
                    &request_id,
                )
                .await
            {
                Ok(receipt) => ToolExecResult::from(Ok(ToolOutput::Peers(PeerOutput::Sent {
                    target: self.target,
                    receipt,
                }))),
                Err(error) => ToolExecResult::failed(ToolFailure::Other, error),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DISCLOSURE_ATTRIBUTE, DISCLOSURE_RISK, ListSessions, MAX_TEXT_BYTES, PEER_RESOURCE,
        SendMessage, WAKE_ATTRIBUTE, WAKE_RISK,
    };
    use crate::AgentMode;
    use crate::permissions::{PermissionAuthorityProfile, PermissionResourceKind, PermissionRisk};
    use crate::tools::test_support::stub_ctx;
    use crate::tools::{
        ToolAudience,
        registry::{PlanModeAccess, Tool},
    };
    use serde_json::{Value, json};
    use test_case::test_case;

    const PEER_TARGET: &str = "exact-live-peer-target";
    const PEER_OTHER_TARGET: &str = "another-live-peer-target";
    const PEER_BODY: &str = "Review the parser.";

    #[test_case(PEER_TARGET; "exact_recipient")]
    #[test_case(PEER_OTHER_TARGET; "different_recipient")]
    fn preflight_binds_disclosure_and_wake_risk_to_the_exact_target(target: &str) {
        smol::block_on(async {
            let call = SendMessage
                .parse(&json!({"target": target, "text": PEER_BODY}))
                .unwrap();
            let ctx = stub_ctx(&AgentMode::Build);
            let intent = call.preflight(&ctx).await.unwrap().unwrap();
            assert_eq!(intent.scopes.scopes, [target]);
            assert!(!intent.scopes.force_prompt);
            assert!(!intent.scopes.plan_scoped);
            assert_eq!(intent.authority, PermissionAuthorityProfile::ExactOnly);
            assert_eq!(intent.risk, PermissionRisk::High);
            assert_eq!(intent.resources.len(), 1);
            let resource = &intent.resources[0];
            assert_eq!(
                resource.kind,
                PermissionResourceKind::Custom {
                    name: PEER_RESOURCE.into()
                }
            );
            assert_eq!(resource.value, target);
            assert!(!resource.requires_prompt);
            assert_eq!(resource.attributes[DISCLOSURE_ATTRIBUTE], DISCLOSURE_RISK);
            assert_eq!(resource.attributes[WAKE_ATTRIBUTE], WAKE_RISK);
        });
    }

    #[test_case(json!({"target":"peer","text":"hello"}), true; "plain_text")]
    #[test_case(json!({"target":"peer","text":"/compact @file"}), true; "literal_commands")]
    #[test_case(json!({"target":"peer","text":"hello","mode":"build"}), false; "cannot_claim_mode")]
    #[test_case(json!({"target":"peer","text":"hello","sender":"human"}), false; "cannot_claim_sender")]
    #[test_case(json!({"target":"","text":"hello"}), false; "empty_target")]
    #[test_case(json!({"target":"peer","text":" "}), false; "empty_text")]
    #[test_case(json!({"target":"peer","text":"x".repeat(MAX_TEXT_BYTES + 1)}), false; "byte_limit")]
    fn validate_send(input: Value, valid: bool) {
        assert_eq!(SendMessage.parse(&input).is_ok(), valid);
    }

    #[test_case(json!({}), true; "empty_arguments")]
    #[test_case(json!({"include_history":true}), false; "no_history_access")]
    fn validate_list(input: Value, valid: bool) {
        assert_eq!(ListSessions.parse(&input).is_ok(), valid);
    }

    #[test]
    fn main_only_with_prompted_plan_sends() {
        assert_eq!(ListSessions.audience(), ToolAudience::MAIN);
        assert_eq!(SendMessage.audience(), ToolAudience::MAIN);
        let call = SendMessage
            .parse(&json!({"target":"peer","text":"hello"}))
            .unwrap();
        assert_eq!(call.plan_mode_access(), PlanModeAccess::Prompted);
    }
}
