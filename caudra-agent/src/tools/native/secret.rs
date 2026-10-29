//! `request_secret`: ask the user for a token through a private card, and hand it to the
//! session's local shell commands as an environment variable.
//!
//! The value travels once, over the in-memory answer channel, straight into the session's vault.
//! The result the model reads is built here and names the variable alone, so the value never
//! reaches a tool result, the transcript or a log.

use std::borrow::Cow;
use std::path::Path;

use caudra_config::global_config_path;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::warn;

use crate::AgentEvent;
use crate::secrets::scope::{self, ForeignEntry};
use crate::secrets::{SecretError, SecretScope, SecretValue, SessionSecrets, check_name};
use crate::tools::native::question::drop_stale_replies;
use crate::tools::registry::{
    ExecFuture, HeaderFuture, HeaderResult, ParseError, Tool, ToolExecResult, ToolFailure,
    ToolInvocation,
};
use crate::tools::schema::{ParamKind, ParamSchema, Property, to_json_schema, validate};
use crate::tools::{DescriptionContext, REQUEST_SECRET_TOOL_NAME, ToolAudience, ToolContext};
use crate::types::{AnswerKind, AskedQuestion, QUESTION_DECLINED_REPLY, QuestionEvent, ToolOutput};

pub const DESCRIPTION: &str = "Ask the user for a secret, such as an API token, key or password, on a private card. The value never enters the chat. This session's shell commands receive it as the environment variable `name`.

Usage notes:
- Call this yourself as soon as a command needs a credential the environment lacks. Never ask for one in chat, and never explain how to provide it instead.
- `name` is the variable the command reads, such as CLOUDFLARE_API_TOKEN or GH_TOKEN.
- `reason` says what the secret is for and where the user gets it.
- Reference the secret as `$NAME` in commands. Never print, echo or log it.
- The user may save it for this session or for all sessions, or decline.";

/// Injected into the `tool_usage` prompt slot whenever this tool is offered.
pub const TOOL_USAGE: &str = "- When a command needs a token, API key or password, call **request_secret** yourself. Never ask for it in chat, and never explain how to provide it instead.";

pub const NO_VAULT: &str =
    "request_secret needs a session to keep the secret in, and this run has none";
pub const NOT_LOCAL: &str = "secrets reach only shell commands that run on this machine, and \
this session's shell runs on a remote Workcell or in a sandbox";
pub const NO_UI: &str =
    "request_secret needs an interactive front end to show its card, and this session has none";
pub const CANCELLED: &str = "cancelled";
const NO_CONFIG_DIR: &str = "no global config directory";
const NAME_FIELD: &str = "name";
const LABEL_FIELD: &str = "label";
const REASON_FIELD: &str = "reason";
const PLACEHOLDER_FIELD: &str = "placeholder";
/// Keys the reply the card sends down the answer channel.
const REPLY_KEY: &str = "request_secret";
const VALUE_KEY: &str = "value";

static NAME_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "The environment variable shell commands read the secret from, such as CLOUDFLARE_API_TOKEN",
};
static LABEL_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "Card title naming the secret, such as Cloudflare API token (2-6 words)",
};
static REASON_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "What the secret is for and where the user gets it",
};
static PLACEHOLDER_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "Hint shown in the empty field, such as the token's usual prefix",
};
static PROPERTIES: &[Property] = &[
    (NAME_FIELD, &NAME_PARAM, true, &[]),
    (LABEL_FIELD, &LABEL_PARAM, true, &[]),
    (REASON_FIELD, &REASON_PARAM, true, &[]),
    (PLACEHOLDER_FIELD, &PLACEHOLDER_PARAM, false, &[]),
];
static SCHEMA: ParamSchema = ParamSchema::Object {
    properties: PROPERTIES,
    description: "",
    reject_unknown: false,
};

pub struct RequestSecretTool;

impl Tool for RequestSecretTool {
    fn name(&self) -> &str {
        REQUEST_SECRET_TOOL_NAME
    }

    fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
        Cow::Borrowed(DESCRIPTION)
    }

    fn schema(&self) -> Value {
        to_json_schema(&SCHEMA)
    }

    fn audience(&self) -> ToolAudience {
        ToolAudience::MAIN | ToolAudience::GENERAL_SUB
    }

    fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        Ok(Box::new(SecretRequest::parse(input)?))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRequest {
    pub name: String,
    pub label: String,
    pub reason: String,
    pub placeholder: Option<String>,
}

impl SecretRequest {
    /// Refuses a name a shell command cannot receive before any card goes up.
    pub fn parse(input: &Value) -> Result<Self, ParseError> {
        let input = validate(&SCHEMA, input.clone())?;
        let field = |key: &str| {
            input
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or_default()
                .to_owned()
        };
        let name = field(NAME_FIELD);
        check_name(&name).map_err(|error| ParseError::custom(error.to_string()))?;
        let placeholder = Some(field(PLACEHOLDER_FIELD)).filter(|text| !text.is_empty());
        Ok(Self {
            name,
            label: field(LABEL_FIELD),
            reason: field(REASON_FIELD),
            placeholder,
        })
    }

    /// The card, which says when the value replaces the one `name` holds now.
    pub fn question(&self, replaces: bool) -> AskedQuestion {
        AskedQuestion {
            question: self.reason.clone(),
            header: self.label.clone(),
            answer: AnswerKind::Secret {
                name: self.name.clone(),
                placeholder: self.placeholder.clone(),
                replaces,
            },
            ..AskedQuestion::default()
        }
    }
}

/// The session's vault, when this context's shell commands can receive a secret from it. A
/// remote Workcell carries a workspace session, and a sandbox names the host directory apart.
pub fn local_vault(ctx: &ToolContext) -> Result<&SessionSecrets, &'static str> {
    let secrets = ctx.secrets.as_ref().ok_or(NO_VAULT)?;
    match ctx.workspace_session.is_some() || ctx.host_cwd.is_some() {
        true => Err(NOT_LOCAL),
        false => Ok(secrets),
    }
}

/// What the card sends back. It crosses the answer channel once, and the tool moves the value
/// into the vault at once.
#[derive(Debug)]
pub enum SecretReply {
    Save {
        value: SecretValue,
        scope: SecretScope,
    },
    Decline,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Keep {
    Session,
    AllSessions,
}

#[derive(Deserialize)]
struct WireReply {
    #[serde(rename = "request_secret")]
    keep: Keep,
    value: String,
}

impl SecretReply {
    pub fn encode(&self) -> String {
        let Self::Save { value, scope } = self else {
            return QUESTION_DECLINED_REPLY.to_owned();
        };
        let keep = match scope {
            SecretScope::Session => Keep::Session,
            SecretScope::Remembered => Keep::AllSessions,
        };
        json!({ REPLY_KEY: keep, VALUE_KEY: value.expose() }).to_string()
    }

    /// Anything but a save, a dismissal included, declines.
    pub fn decode(raw: &str) -> Self {
        match serde_json::from_str::<WireReply>(raw) {
            Ok(WireReply { keep, value }) => Self::Save {
                value: SecretValue::new(value),
                scope: match keep {
                    Keep::Session => SecretScope::Session,
                    Keep::AllSessions => SecretScope::Remembered,
                },
            },
            Err(_) => Self::Decline,
        }
    }
}

/// [`settle`] against the global config file, where remembering writes.
pub fn store(
    secrets: &SessionSecrets,
    name: &str,
    reply: SecretReply,
) -> Result<String, SecretError> {
    settle(secrets, global_config_path().as_deref(), name, reply)
}

/// Moves the user's reply into `secrets` and words what the model reads. Remembering for all
/// sessions writes `config`; where that cannot happen the secret stays saved for this session.
pub fn settle(
    secrets: &SessionSecrets,
    config: Option<&Path>,
    name: &str,
    reply: SecretReply,
) -> Result<String, SecretError> {
    let SecretReply::Save { value, scope } = reply else {
        return Ok(declined(name));
    };
    secrets.set(name, value)?;
    if scope == SecretScope::Session {
        return Ok(saved(name));
    }
    let Some(config) = config else {
        return Ok(not_remembered(name, NO_CONFIG_DIR));
    };
    match scope::remember(secrets, config, name, ForeignEntry::Refuse) {
        Ok(()) => Ok(remembered(name)),
        Err(SecretError::ForeignEntry(_)) => Ok(format!(
            "{} It is saved for this session only, because [agent.shell_env] already sets {name}. \
             The user can move it in /secrets.",
            saved(name)
        )),
        Err(error) => {
            warn!(name, %error, "a secret could not be remembered for all sessions");
            Ok(not_remembered(name, &error.to_string()))
        }
    }
}

pub fn replaces_note(name: &str) -> String {
    format!("Saving replaces the value ${name} holds now.")
}

pub fn kept_note(name: &str) -> String {
    format!("The value stays out of the chat. This session's shell commands receive it as ${name}.")
}

fn saved(name: &str) -> String {
    format!(
        "${name} is set for this session's shell commands. Use it as ${name} and never print it."
    )
}

fn remembered(name: &str) -> String {
    format!(
        "${name} is set for this session's shell commands and remembered for all sessions. \
         Use it as ${name} and never print it."
    )
}

fn not_remembered(name: &str, why: &str) -> String {
    format!(
        "{} It is saved for this session only, because remembering it for all sessions failed: \
         {why}",
        saved(name)
    )
}

fn declined(name: &str) -> String {
    format!("The user declined to provide ${name}.")
}

impl ToolInvocation for SecretRequest {
    fn start_header(&self) -> HeaderFuture {
        HeaderFuture::Ready(HeaderResult::plain(format!("${}", self.name)))
    }

    fn execute<'a>(self: Box<Self>, ctx: &'a ToolContext) -> ExecFuture<'a> {
        Box::pin(async move {
            let secrets = match local_vault(ctx) {
                Ok(secrets) => secrets,
                Err(refusal) => return refused(ToolFailure::Other, refusal),
            };
            let Some(replies) = ctx.user_response_rx.as_ref() else {
                return refused(ToolFailure::Other, NO_UI);
            };
            let replies = replies.lock().await;
            if ctx.permissions.autopilot().is_some() {
                drop_stale_replies(&replies);
            }
            let _ = ctx
                .event_tx
                .send(AgentEvent::Question(Box::new(QuestionEvent {
                    questions: vec![self.question(secrets.contains(&self.name))],
                    call_id: ctx.tool_use_id.clone(),
                    ..QuestionEvent::default()
                })));
            let Ok(Ok(raw)) = ctx.cancel.race(replies.recv_async()).await else {
                return refused(ToolFailure::Cancelled, CANCELLED);
            };
            drop(replies);
            match store(secrets, &self.name, SecretReply::decode(&raw)) {
                Ok(text) => ToolExecResult::from(Ok(ToolOutput::Plain(text.into()))),
                Err(error) => ToolExecResult::failed(ToolFailure::InvalidInput, error.to_string()),
            }
        })
    }
}

fn refused(failure: ToolFailure, message: &'static str) -> ToolExecResult {
    ToolExecResult::from(Ok(ToolOutput::Plain(message.into()))).with_failure(failure)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;

    use caudra_config::PermissionsConfig;
    use caudra_config::config_file::{CONFIG_FILE, CONFIG_VERSION};
    use caudra_config::shell_env::GlobalShellEnvFile;
    use caudra_storage::StateDir;
    use caudra_storage::id::CaudraId;
    use flume::{Receiver, Sender};
    use futures_lite::future;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::autopilot::{ScriptedProvider, Step, engaged_handle};
    use crate::cancel::CancelToken;
    use crate::permissions::PermissionManager;
    use crate::secrets::SecretVaults;
    use crate::secrets::scope::remembered_reference;
    use crate::tools::native::tests::workspace_for_principal;
    use crate::tools::test_support::stub_ctx_with_permissions;
    use crate::types::QUESTION_DISMISSED_REPLY;
    use crate::{AgentMode, Envelope, EventSender};

    const NAME: &str = "CLOUDFLARE_API_TOKEN";
    const LABEL: &str = "Cloudflare API token";
    const REASON: &str = "wrangler deploys with it; create one at dash.cloudflare.com";
    const PLACEHOLDER: &str = "cf-…";
    const CANARY: &str = "secret-canary-4711";
    const FOREIGN: &str = "configured-elsewhere";
    const CWD: &str = "/tmp";
    const TOOL_USE_ID: &str = "toolu_secret";
    const PARENT_CALL: &str = "toolu_task";
    const ANSWER_YES: &str = r#"{"action":"answer","reason":"looks fine","answers":[["Yes"]]}"#;
    const LEAKED: &str = "the secret's value must never reach a result, an event or a card";

    struct Asked {
        _temp: TempDir,
        state: StateDir,
        ctx: ToolContext,
        events: Receiver<Envelope>,
        replies: Sender<String>,
    }

    impl Asked {
        fn new() -> Self {
            Self::with_autopilot(false)
        }

        fn with_autopilot(engaged: bool) -> Self {
            let temp = TempDir::new().unwrap();
            let state = StateDir::from_path(temp.path().to_path_buf());
            let permissions = Arc::new(PermissionManager::new_nonpersistent(
                PermissionsConfig::default(),
                PathBuf::from(CWD),
                Arc::default(),
            ));
            if engaged {
                permissions.set_autopilot(Some(engaged_handle(ScriptedProvider::new(vec![
                    Step::Answer(ANSWER_YES),
                ]))));
            }
            let mut ctx = stub_ctx_with_permissions(&AgentMode::Build, permissions);
            let (raw_tx, events) = flume::unbounded();
            ctx.event_tx = EventSender::new(raw_tx, 0);
            let (replies, rx) = flume::unbounded();
            ctx.user_response_rx = Some(Arc::new(async_lock::Mutex::new(rx)));
            ctx.secrets = Some(SecretVaults::for_session(&state, CaudraId::generate()));
            ctx.tool_use_id = Some(TOOL_USE_ID.into());
            ctx.root_tool_use_id = Some(TOOL_USE_ID.into());
            ctx.local_root_tool_use_id = Some(TOOL_USE_ID.into());
            Self {
                _temp: temp,
                state,
                ctx,
                events,
                replies,
            }
        }

        fn secrets(&self) -> &SessionSecrets {
            self.ctx.secrets.as_ref().unwrap()
        }

        fn injected(&self) -> Option<String> {
            self.secrets()
                .overlay()
                .into_iter()
                .find(|(name, _)| name == NAME)
                .map(|(_, value)| value.into_string().unwrap())
        }

        /// Asks, and answers the card with `reply` once it is up, or cancels the call without one.
        fn ask(&mut self, reply: Option<SecretReply>) -> (ToolExecResult, QuestionEvent) {
            let (cancel, token) = CancelToken::new();
            self.ctx.cancel = token;
            let Self {
                ctx,
                events,
                replies,
                ..
            } = &*self;
            let answer = async move {
                let event = loop {
                    if let AgentEvent::Question(event) = events.recv_async().await.unwrap().event {
                        break *event;
                    }
                };
                let held = match reply {
                    Some(reply) => {
                        replies.send(reply.encode()).unwrap();
                        Some(cancel)
                    }
                    None => {
                        cancel.cancel();
                        None
                    }
                };
                (event, held)
            };
            let (result, (event, _held)) =
                smol::block_on(future::zip(request().execute(ctx), answer));
            (result, event)
        }

        fn asked_nothing(&self) -> bool {
            !self
                .events
                .drain()
                .any(|envelope| matches!(envelope.event, AgentEvent::Question(_)))
        }
    }

    fn input() -> Value {
        json!({
            NAME_FIELD: NAME,
            LABEL_FIELD: LABEL,
            REASON_FIELD: REASON,
            PLACEHOLDER_FIELD: PLACEHOLDER,
        })
    }

    fn request() -> Box<SecretRequest> {
        Box::new(SecretRequest::parse(&input()).unwrap())
    }

    fn save(scope: SecretScope) -> SecretReply {
        SecretReply::Save {
            value: SecretValue::new(CANARY.to_owned()),
            scope,
        }
    }

    fn text(result: &ToolExecResult) -> String {
        result.output.as_ref().unwrap().as_text().to_string()
    }

    fn assert_no_canary(result: &ToolExecResult, event: &QuestionEvent) {
        let output = serde_json::to_string(result.output.as_ref().unwrap()).unwrap();
        let card = serde_json::to_string(event).unwrap();
        assert!(!output.contains(CANARY), "{LEAKED}: {output}");
        assert!(!card.contains(CANARY), "{LEAKED}: {card}");
        assert!(result.model_output.is_none(), "{LEAKED}");
    }

    /// A global `caudra.toml` at the current version, with `NAME` set to `set` if given.
    fn config(dir: &Path, set: Option<&str>) -> PathBuf {
        let path = dir.join(CONFIG_FILE);
        let mut text = format!("version = {CONFIG_VERSION}\n");
        if let Some(value) = set {
            text.push_str(&format!(
                "\n[agent.shell_env]\nset = {{ {NAME} = \"{value}\" }}\n"
            ));
        }
        fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn a_saved_secret_reaches_the_shell_and_only_its_name_reaches_the_model() {
        let mut asked = Asked::new();

        let (result, event) = asked.ask(Some(save(SecretScope::Session)));

        assert!(!result.is_error);
        assert_eq!(text(&result), saved(NAME));
        assert_eq!(asked.injected().as_deref(), Some(CANARY));
        assert_no_canary(&result, &event);
        let [question] = event.questions.as_slice() else {
            panic!("the card asks one question");
        };
        assert_eq!(question.header, LABEL);
        assert_eq!(question.question, REASON);
        assert_eq!(
            question.answer,
            AnswerKind::Secret {
                name: NAME.into(),
                placeholder: Some(PLACEHOLDER.into()),
                replaces: false,
            }
        );
        assert_eq!(event.call_id.as_deref(), Some(TOOL_USE_ID));
    }

    #[test]
    fn the_card_says_when_it_replaces_a_live_secret() {
        let mut asked = Asked::new();
        asked
            .secrets()
            .set(NAME, SecretValue::new(FOREIGN.to_owned()))
            .unwrap();

        let (_, event) = asked.ask(Some(save(SecretScope::Session)));

        assert!(matches!(
            event.questions[0].answer,
            AnswerKind::Secret { replaces: true, .. }
        ));
        assert_eq!(asked.injected().as_deref(), Some(CANARY));
    }

    #[test_case(Some(SecretReply::Decline), None, declined(NAME) ; "declined")]
    #[test_case(None, Some(ToolFailure::Cancelled), CANCELLED.to_owned() ; "cancelled")]
    fn an_unsaved_card_leaves_the_vault_empty(
        reply: Option<SecretReply>,
        failure: Option<ToolFailure>,
        expected: String,
    ) {
        let mut asked = Asked::new();

        let (result, _) = asked.ask(reply);

        assert_eq!(result.failure, failure);
        assert_eq!(text(&result), expected);
        assert!(asked.secrets().listing().is_empty());
    }

    #[test_case("1TOKEN" ; "not_a_variable")]
    #[test_case("PATH" ; "allowlisted")]
    #[test_case("BASH_ENV" ; "bash_startup")]
    fn a_name_a_shell_cannot_receive_is_refused_before_any_card(name: &str) {
        let refused = SecretRequest::parse(&json!({
            NAME_FIELD: name,
            LABEL_FIELD: LABEL,
            REASON_FIELD: REASON,
        }))
        .unwrap_err();

        let expected = check_name(name).unwrap_err().to_string();
        assert!(refused.to_string().contains(&expected), "{refused}");
    }

    enum Gap {
        Remote,
        Sandbox,
        NoFrontEnd,
        NoSession,
    }

    #[test_case(Gap::Remote, NOT_LOCAL ; "remote_workcell")]
    #[test_case(Gap::Sandbox, NOT_LOCAL ; "sandbox")]
    #[test_case(Gap::NoFrontEnd, NO_UI ; "headless")]
    #[test_case(Gap::NoSession, NO_VAULT ; "no_session")]
    fn a_card_that_cannot_reach_a_local_shell_is_refused_unasked(gap: Gap, expected: &str) {
        let mut asked = Asked::new();
        match gap {
            Gap::Remote => {
                asked.ctx.workspace_session = Some(workspace_for_principal(PARENT_CALL));
            }
            Gap::Sandbox => asked.ctx.host_cwd = Some(PathBuf::from(CWD)),
            Gap::NoFrontEnd => asked.ctx.user_response_rx = None,
            Gap::NoSession => asked.ctx.secrets = None,
        }

        let result = smol::block_on(request().execute(&asked.ctx));

        assert_eq!(result.failure, Some(ToolFailure::Other));
        assert_eq!(text(&result), expected);
        assert!(asked.asked_nothing());
    }

    #[test]
    fn autopilot_never_claims_the_card() {
        let mut asked = Asked::with_autopilot(true);
        let autopilot = asked.ctx.permissions.autopilot().unwrap();

        let (result, _) = asked.ask(Some(save(SecretScope::Session)));

        assert_eq!(text(&result), saved(NAME));
        assert!(!autopilot.question_claimed(TOOL_USE_ID));
        assert!(autopilot.recent_decisions().is_empty());
        assert!(
            !asked
                .events
                .drain()
                .any(|envelope| matches!(envelope.event, AgentEvent::Autopilot(_)))
        );
    }

    #[test]
    fn a_subagent_saves_into_the_vault_its_parent_reads() {
        let mut asked = Asked::new();
        let root = CaudraId::generate();
        asked.ctx.secrets = Some(SecretVaults::for_session(&asked.state, root));
        asked.ctx.audience = ToolAudience::GENERAL_SUB;
        asked.ctx.root_tool_use_id = Some(PARENT_CALL.into());

        let (result, _) = asked.ask(Some(save(SecretScope::Session)));

        assert_eq!(text(&result), saved(NAME));
        assert!(SecretVaults::for_session(&asked.state, root).contains(NAME));
    }

    #[test]
    fn remember_writes_the_global_set_entry() {
        let asked = Asked::new();
        let config = config(asked._temp.path(), None);

        let answer = settle(
            asked.secrets(),
            Some(&config),
            NAME,
            save(SecretScope::Remembered),
        )
        .unwrap();

        assert_eq!(answer, remembered(NAME));
        assert!(!answer.contains(CANARY), "{LEAKED}");
        let set = GlobalShellEnvFile::load(&config).unwrap().set().clone();
        assert_eq!(
            set.get(NAME),
            Some(&remembered_reference(&asked.state, NAME).unwrap())
        );
        assert_eq!(asked.injected().as_deref(), Some(CANARY));
    }

    #[test]
    fn remember_over_a_foreign_entry_saves_for_this_session_only() {
        let asked = Asked::new();
        let config = config(asked._temp.path(), Some(FOREIGN));

        let answer = settle(
            asked.secrets(),
            Some(&config),
            NAME,
            save(SecretScope::Remembered),
        )
        .unwrap();

        assert!(answer.starts_with(&saved(NAME)), "{answer}");
        assert!(answer.contains("/secrets"), "{answer}");
        assert_eq!(
            GlobalShellEnvFile::load(&config).unwrap().set().get(NAME),
            Some(&FOREIGN.to_owned())
        );
        assert!(asked.secrets().contains(NAME));
    }

    #[test_case(save(SecretScope::Session) ; "session")]
    #[test_case(save(SecretScope::Remembered) ; "all_sessions")]
    fn a_reply_round_trips_the_answer_channel(reply: SecretReply) {
        let decoded = SecretReply::decode(&reply.encode());

        let (
            SecretReply::Save { value, scope },
            SecretReply::Save {
                scope: expected, ..
            },
        ) = (decoded, reply)
        else {
            panic!("a save decodes as a save");
        };
        assert_eq!(value.expose(), CANARY);
        assert_eq!(scope, expected);
    }

    #[test_case(QUESTION_DISMISSED_REPLY ; "a_dismissal")]
    #[test_case("[[\"Yes\"]]" ; "a_question_answer")]
    fn anything_but_a_save_declines(raw: &str) {
        assert!(matches!(SecretReply::decode(raw), SecretReply::Decline));
    }
}
