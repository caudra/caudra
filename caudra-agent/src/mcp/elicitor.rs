//! Who answers an MCP server's questions during a tool call.
//!
//! A front end that can ask plugs an [`ElicitorFactory`] into the session.
//! The default, [`QuestionForms`], asks the way the `question` tool does: a
//! question event, answered on the session's answer channel, which autopilot
//! may race for a plain form. It lives here rather than beside that tool
//! because what it asks and sends back is the MCP contract: the form a
//! requested schema makes, the content it coerces to, and the address a URL
//! request wants opened.

use std::io;
use std::sync::Arc;

use async_lock::{Mutex, MutexGuard};
use flume::Receiver;
use serde_json::{Map, Value};
use tracing::warn;

use super::call::{ElicitMode, ElicitRequest, ElicitResponse, Elicitor};
use super::elicitation::{ElicitAction, ElicitForm};
use super::kind::McpServerKind;
use super::transport::BoxFuture;
use crate::autopilot::AutopilotHandle;
use crate::autopilot::question::QuestionDelegate;
use crate::cancel::CancelToken;
use crate::tools::ToolContext;
use crate::tools::native::question::{
    Reply, asked_by_main_agent, drop_stale_replies, wait_for_answer,
};
use crate::types::{
    Answerer, AskedQuestion, QUESTION_ACCEPTED_REPLY, QUESTION_DECLINED_REPLY,
    QUESTION_DISMISSED_REPLY, QuestionEvent, QuestionSource,
};
use crate::{AgentEvent, EventSender};

/// How many times a form is put to the user before an answer it cannot take
/// ends it.
pub const MAX_FORM_ATTEMPTS: usize = 3;

/// Makes the elicitor for each MCP call a front end can answer.
pub trait ElicitorFactory: Send + Sync {
    /// `None` when nobody can answer a server during a call made under `ctx`.
    fn for_call(&self, ctx: &ToolContext) -> Option<Arc<dyn Elicitor>>;
}

/// Puts each request to the user as a question on the session's answer
/// channel, for a front end that renders MCP forms.
pub struct QuestionForms;

impl ElicitorFactory for QuestionForms {
    fn for_call(&self, ctx: &ToolContext) -> Option<Arc<dyn Elicitor>> {
        FormElicitor::for_call(ctx).map(|elicitor| Arc::new(elicitor) as Arc<dyn Elicitor>)
    }
}

type OpenUrl = fn(&str) -> io::Result<()>;

/// One call's elicitor: the answer channel, where questions go, and whether
/// autopilot may answer in the user's place.
pub struct FormElicitor {
    replies: Arc<Mutex<Receiver<String>>>,
    event_tx: EventSender,
    cancel: CancelToken,
    autopilot: Option<AutopilotHandle>,
    main_agent: bool,
    tool_use_id: Option<String>,
    open_url: OpenUrl,
}

/// The request as the user is shown it.
struct Asked<'a> {
    server: &'a str,
    kind: Option<McpServerKind>,
    message: String,
}

/// Closes the question in the front end if the wait for its answer is
/// dropped: the server withdrew it, or the call ended first.
struct Shown<'a>(Option<&'a EventSender>);

impl Shown<'_> {
    fn answered(mut self) {
        self.0 = None;
    }
}

impl Drop for Shown<'_> {
    fn drop(&mut self) {
        if let Some(event_tx) = self.0 {
            event_tx.try_send(AgentEvent::QuestionClosed);
        }
    }
}

impl FormElicitor {
    /// A call made without an answer channel gets none, so its servers'
    /// questions are declined rather than left waiting. A subagent's call
    /// holds the channel its parent relays answers on.
    pub fn for_call(ctx: &ToolContext) -> Option<Self> {
        Some(Self {
            replies: Arc::clone(ctx.user_response_rx.as_ref()?),
            event_tx: ctx.event_tx.clone(),
            cancel: ctx.cancel.clone(),
            autopilot: ctx.permissions.autopilot(),
            main_agent: asked_by_main_agent(ctx),
            tool_use_id: ctx.tool_use_id.clone(),
            open_url: |url| open::that(url),
        })
    }

    async fn ask(&self, server: &str, request: ElicitRequest) -> ElicitResponse {
        let asked = Asked {
            server,
            kind: request.kind,
            message: request.message,
        };
        match request.mode {
            ElicitMode::Url { url, .. } => self.open(&asked, url).await,
            ElicitMode::Form { schema } => match ElicitForm::parse(&schema) {
                Ok(form) => self.fill(&asked, &form).await,
                Err(error) => {
                    warn!(server, %error, "declined an MCP form Caudra cannot show");
                    ElicitResponse::unanswered(ElicitAction::Decline)
                }
            },
        }
    }

    /// Asks the user to open `url`, and opens it once they agree.
    async fn open(&self, asked: &Asked<'_>, url: String) -> ElicitResponse {
        let replies = self.lock_replies().await;
        let shown = self.show(asked, Vec::new(), asked.message.clone(), Some(url.clone()));
        let Ok(reply) = replies.recv_async().await else {
            return ElicitResponse::unanswered(ElicitAction::Cancel);
        };
        shown.answered();
        let action = sentinel_action(&reply).unwrap_or(ElicitAction::Cancel);
        if action == ElicitAction::Accept
            && let Err(error) = (self.open_url)(&url)
        {
            warn!(server = asked.server, %error, "could not open the address an MCP server asked for");
        }
        ElicitResponse::unanswered(action)
    }

    /// Puts the form until it is answered with content it takes, declined,
    /// or dismissed. An answer it cannot take puts it again with the reason
    /// ahead of the server's message, at most [`MAX_FORM_ATTEMPTS`] times.
    async fn fill(&self, asked: &Asked<'_>, form: &ElicitForm) -> ElicitResponse {
        let replies = self.lock_replies().await;
        let questions = form.questions();
        let mut delegate = self.delegate(asked, form, &questions);
        let mut message = asked.message.clone();
        for _ in 0..MAX_FORM_ATTEMPTS {
            let shown = self.show(asked, questions.clone(), message, None);
            let fit = |picked: &[Vec<String>]| form.content(picked);
            let wait = wait_for_answer(&replies, delegate.take(), &self.cancel, fit);
            let raw = match wait.await {
                Some(Reply::Human(raw)) => raw,
                Some(Reply::Autopilot(content)) => {
                    shown.answered();
                    return accepted(form, content, Answerer::Autopilot);
                }
                None => return ElicitResponse::unanswered(ElicitAction::Cancel),
            };
            shown.answered();
            let picked = match sentinel_action(&raw) {
                Some(ElicitAction::Accept) => Vec::new(),
                Some(action) => return ElicitResponse::unanswered(action),
                None => match serde_json::from_str::<Vec<Vec<String>>>(&raw) {
                    Ok(picked) => picked,
                    Err(_) => return ElicitResponse::unanswered(ElicitAction::Cancel),
                },
            };
            match form.content(&picked) {
                Ok(content) => return accepted(form, content, Answerer::User),
                Err(error) => message = format!("{error}\n\n{}", asked.message),
            }
        }
        warn!(
            server = asked.server,
            attempts = MAX_FORM_ATTEMPTS,
            "gave up on an MCP form none of the answers fit"
        );
        ElicitResponse::unanswered(ElicitAction::Cancel)
    }

    /// The channel, held for the whole request so nothing else consumes its
    /// reply, with any reply to an earlier form autopilot took dropped.
    async fn lock_replies(&self) -> MutexGuard<'_, Receiver<String>> {
        let replies = self.replies.lock().await;
        if self.autopilot.is_some() {
            drop_stale_replies(&replies);
        }
        replies
    }

    /// Autopilot answers only a plain form the main agent's call raised: never
    /// a URL, a form with nothing to fill, one that asks for a credential, or
    /// one from Executor, whose forms gate the code it runs.
    fn delegate(
        &self,
        asked: &Asked<'_>,
        form: &ElicitForm,
        questions: &[AskedQuestion],
    ) -> Option<QuestionDelegate> {
        let eligible = self.main_agent
            && !questions.is_empty()
            && asked.kind != Some(McpServerKind::Executor)
            && !form.has_credential_field();
        let handle = self
            .autopilot
            .clone()
            .filter(|handle| eligible && handle.is_engaged())?;
        Some(QuestionDelegate::for_server(
            handle,
            asked.server,
            &asked.message,
            questions,
            self.tool_use_id.clone(),
            &self.event_tx,
        ))
    }

    fn show(
        &self,
        asked: &Asked<'_>,
        questions: Vec<AskedQuestion>,
        message: String,
        url: Option<String>,
    ) -> Shown<'_> {
        self.event_tx
            .try_send(AgentEvent::Question(Box::new(QuestionEvent {
                questions,
                source: Some(QuestionSource::Mcp {
                    server: asked.server.to_owned(),
                    kind: asked.kind,
                    message,
                    url,
                }),
                call_id: self.tool_use_id.clone(),
                ..QuestionEvent::default()
            })));
        Shown(Some(&self.event_tx))
    }
}

impl Elicitor for FormElicitor {
    fn elicit<'a>(
        &'a self,
        server: &'a str,
        request: ElicitRequest,
    ) -> BoxFuture<'a, ElicitResponse> {
        Box::pin(self.ask(server, request))
    }
}

/// The action a reply that carries no answers stands for.
fn sentinel_action(reply: &str) -> Option<ElicitAction> {
    match reply {
        QUESTION_ACCEPTED_REPLY => Some(ElicitAction::Accept),
        QUESTION_DECLINED_REPLY => Some(ElicitAction::Decline),
        QUESTION_DISMISSED_REPLY => Some(ElicitAction::Cancel),
        _ => None,
    }
}

fn accepted(form: &ElicitForm, content: Map<String, Value>, by: Answerer) -> ElicitResponse {
    ElicitResponse {
        action: ElicitAction::Accept,
        answers: form.summary(&content),
        content: Some(content),
        by,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::future::Future;
    use std::path::PathBuf;
    use std::time::Duration;

    use caudra_config::PermissionsConfig;
    use futures_lite::future;
    use serde_json::json;
    use test_case::test_case;

    use super::*;
    use crate::autopilot::{ActionName, AutopilotDecision, ScriptedProvider, Step, engaged_handle};
    use crate::mcp::call::CallScope;
    use crate::mcp::config::LocalExecutionPolicy;
    use crate::mcp::elicitation::MASKED_ANSWER;
    use crate::mcp::protocol::TOOLS_CALL_METHOD;
    use crate::mcp::stdio::StdioTransport;
    use crate::mcp::stub_session;
    use crate::mcp::transport::McpTransport;
    use crate::permissions::PermissionManager;
    use crate::tools::test_support::{stub_ctx, stub_ctx_with_permissions};
    use crate::types::AutopilotEvent;
    use crate::{AgentMode, Envelope};

    const SERVER: &str = "deploys";
    const MESSAGE: &str = "Where should the release go?";
    const BRANCH: &str = "branch";
    const MAIN: &str = "main";
    const COUNT: &str = "count";
    const NOT_A_COUNT: &str = "many";
    const THREE: &str = "3";
    const PASSWORD: &str = "password";
    const URL: &str = "https://example.com/authorize";
    const CWD: &str = "/tmp";
    const PARENT_CALL: &str = "toolu_task";
    const MCP_CALL: &str = "toolu_mcp";
    const UNREADABLE_REPLY: &str = "not a reply";
    const STRING_TYPE: &str = "string";
    const INTEGER_TYPE: &str = "integer";
    const DELEGATE_PICKS_MAIN: &str =
        r#"{"action":"answer","reason":"the user works on main","answers":[["main"]]}"#;
    const DELEGATE_MISCOUNTS: &str =
        r#"{"action":"answer","reason":"a guess","answers":[["many"]]}"#;
    const ELICITATION_ENV: &str = "ELICITATION";
    const ELICITATION_ID: &str = "srv-1";
    const CALL_TIMEOUT: Duration = Duration::from_secs(5);
    /// Asks `$ELICITATION` while the first call is open, then answers the
    /// call with whatever Caudra replied, as a legacy server does.
    const ELICITING_SERVER: &str = r#"read -r call
printf '%s\n' "$ELICITATION"
read -r reply
printf '{"jsonrpc":"2.0","id":1,"result":{"reply":%s}}\n' "$reply"
cat >/dev/null
"#;
    const OPENED_EARLY: &str = "the address must not open before the user agrees";
    const UNASKED: &str = "the delegate must not be consulted";
    const LEFT_OPEN: &str = "a question nobody answered must be closed";
    const CLOSED_TWICE: &str = "an answered question is already closed";

    thread_local! {
        static OPENED: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    }

    fn record_open(url: &str) -> io::Result<()> {
        OPENED.with_borrow_mut(|opened| opened.push(url.to_owned()));
        Ok(())
    }

    fn fail_open(_: &str) -> io::Result<()> {
        Err(io::Error::other(URL))
    }

    fn opened() -> Vec<String> {
        OPENED.with_borrow(Clone::clone)
    }

    /// A main-agent call whose questions reach `events` and are answered on
    /// `replies`, with `autopilot` on its session.
    struct Session {
        ctx: ToolContext,
        events: Receiver<Envelope>,
        replies: flume::Sender<String>,
    }

    impl Session {
        fn new(autopilot: Option<AutopilotHandle>) -> Self {
            let permissions = Arc::new(PermissionManager::new_nonpersistent(
                PermissionsConfig::default(),
                PathBuf::from(CWD),
                Arc::default(),
            ));
            permissions.set_autopilot(autopilot);
            let mut ctx = stub_ctx_with_permissions(&AgentMode::Build, permissions);
            let (raw_tx, events) = flume::unbounded();
            ctx.event_tx = EventSender::new(raw_tx, 0);
            ctx.tool_use_id = Some(MCP_CALL.into());
            let (replies, rx) = flume::unbounded();
            ctx.user_response_rx = Some(Arc::new(Mutex::new(rx)));
            Self {
                ctx,
                events,
                replies,
            }
        }

        fn elicitor(&self) -> FormElicitor {
            FormElicitor {
                open_url: record_open,
                ..FormElicitor::for_call(&self.ctx).unwrap()
            }
        }

        /// Asks, with `human` beside the wait.
        fn ask<T>(
            &self,
            kind: Option<McpServerKind>,
            mode: ElicitMode,
            human: impl Future<Output = T>,
        ) -> (ElicitResponse, T) {
            let elicitor = self.elicitor();
            smol::block_on(future::zip(
                elicitor.elicit(SERVER, request(kind, mode)),
                human,
            ))
        }

        async fn next(&self, wanted: impl Fn(&AgentEvent) -> bool) -> AgentEvent {
            loop {
                let event = self.events.recv_async().await.unwrap().event;
                if wanted(&event) {
                    return event;
                }
            }
        }

        async fn asked(&self) -> QuestionEvent {
            let event = self
                .next(|event| matches!(event, AgentEvent::Question(_)))
                .await;
            let AgentEvent::Question(question) = event else {
                unreachable!("only a question is waited for");
            };
            *question
        }

        async fn decided(&self) -> AutopilotDecision {
            let event = self
                .next(|event| matches!(event, AgentEvent::Autopilot(AutopilotEvent::Decided(_))))
                .await;
            let AgentEvent::Autopilot(AutopilotEvent::Decided(decision)) = event else {
                unreachable!("only a decision is waited for");
            };
            *decision
        }

        fn reply(&self, reply: &str) {
            self.replies.send(reply.to_owned()).unwrap();
        }

        fn answer(&self, labels: &[&str]) {
            self.reply(&json!([labels]).to_string());
        }

        fn closed(&self) -> bool {
            self.events
                .drain()
                .any(|envelope| matches!(envelope.event, AgentEvent::QuestionClosed))
        }
    }

    fn request(kind: Option<McpServerKind>, mode: ElicitMode) -> ElicitRequest {
        ElicitRequest {
            mode,
            message: MESSAGE.into(),
            kind,
        }
    }

    fn form(field: &str, kind: &str) -> ElicitMode {
        ElicitMode::Form {
            schema: json!({
                "type": "object",
                "properties": {field: {"type": kind}},
                "required": [field],
            }),
        }
    }

    fn empty_form() -> ElicitMode {
        ElicitMode::Form {
            schema: json!({"type": "object", "properties": {}}),
        }
    }

    fn url() -> ElicitMode {
        ElicitMode::Url {
            url: URL.into(),
            elicitation_id: None,
        }
    }

    fn content(field: &str, value: Value) -> Option<Map<String, Value>> {
        Some(Map::from_iter([(field.to_owned(), value)]))
    }

    fn message_of(question: &QuestionEvent) -> &str {
        match &question.source {
            Some(QuestionSource::Mcp { message, .. }) => message,
            None => unreachable!("an MCP question names its server"),
        }
    }

    #[test]
    fn an_answered_form_is_accepted_with_its_content() {
        let session = Session::new(None);

        let (response, question) = session.ask(None, form(BRANCH, STRING_TYPE), async {
            let question = session.asked().await;
            session.answer(&[MAIN]);
            question
        });

        assert_eq!(
            question.source,
            Some(QuestionSource::Mcp {
                server: SERVER.into(),
                kind: None,
                message: MESSAGE.into(),
                url: None,
            })
        );
        assert_eq!(question.questions.len(), 1);
        assert_eq!(
            response,
            ElicitResponse {
                action: ElicitAction::Accept,
                content: content(BRANCH, json!(MAIN)),
                by: Answerer::User,
                answers: vec![(BRANCH.into(), MAIN.into())],
            }
        );
        assert!(!session.closed(), "{CLOSED_TWICE}");
    }

    #[test_case(QUESTION_DECLINED_REPLY, ElicitAction::Decline ; "declined")]
    #[test_case(QUESTION_DISMISSED_REPLY, ElicitAction::Cancel ; "dismissed")]
    #[test_case(UNREADABLE_REPLY, ElicitAction::Cancel ; "unreadable")]
    fn a_reply_without_answers_ends_the_form(reply: &str, action: ElicitAction) {
        let session = Session::new(None);

        let (response, ()) = session.ask(None, form(BRANCH, STRING_TYPE), async {
            session.asked().await;
            session.reply(reply);
        });

        assert_eq!(response, ElicitResponse::unanswered(action));
    }

    #[test]
    fn a_form_without_fields_is_accepted_empty() {
        let session = Session::new(None);

        let (response, question) = session.ask(None, empty_form(), async {
            let question = session.asked().await;
            session.reply(QUESTION_ACCEPTED_REPLY);
            question
        });

        assert!(question.questions.is_empty());
        assert_eq!(response.action, ElicitAction::Accept);
        assert_eq!(response.content, Some(Map::new()));
    }

    #[test]
    fn a_form_caudra_cannot_show_is_declined_unasked() {
        let session = Session::new(None);
        let unsupported = ElicitMode::Form {
            schema: json!({"type": "array"}),
        };

        let (response, ()) = session.ask(None, unsupported, future::ready(()));

        assert_eq!(response, ElicitResponse::unanswered(ElicitAction::Decline));
        assert!(session.events.is_empty());
    }

    #[test]
    fn a_form_is_put_again_with_the_reason_its_answer_did_not_fit() {
        let session = Session::new(None);

        let (response, retried) = session.ask(None, form(COUNT, INTEGER_TYPE), async {
            session.asked().await;
            session.answer(&[NOT_A_COUNT]);
            let retried = session.asked().await;
            session.answer(&[THREE]);
            retried
        });

        let message = message_of(&retried);
        assert!(
            message.ends_with(MESSAGE) && message != MESSAGE,
            "{message}"
        );
        assert_eq!(response.action, ElicitAction::Accept);
        assert_eq!(response.content, content(COUNT, json!(3)));
    }

    #[test]
    fn a_form_whose_answers_never_fit_is_cancelled() {
        let session = Session::new(None);

        let (response, asked) = session.ask(None, form(COUNT, INTEGER_TYPE), async {
            for _ in 0..MAX_FORM_ATTEMPTS {
                session.asked().await;
                session.answer(&[NOT_A_COUNT]);
            }
            MAX_FORM_ATTEMPTS
        });

        assert_eq!(asked, MAX_FORM_ATTEMPTS);
        assert_eq!(response, ElicitResponse::unanswered(ElicitAction::Cancel));
        assert!(session.events.is_empty(), "no form is put after the last");
    }

    #[test_case(QUESTION_ACCEPTED_REPLY, ElicitAction::Accept, &[URL] ; "accepted_opens_it")]
    #[test_case(QUESTION_DECLINED_REPLY, ElicitAction::Decline, &[] ; "declined")]
    #[test_case(QUESTION_DISMISSED_REPLY, ElicitAction::Cancel, &[] ; "dismissed")]
    fn an_address_opens_only_once_the_user_agrees(
        reply: &str,
        action: ElicitAction,
        expected: &[&str],
    ) {
        let session = Session::new(None);

        let (response, (question, before)) = session.ask(None, url(), async {
            let question = session.asked().await;
            let before = opened();
            session.reply(reply);
            (question, before)
        });

        assert!(before.is_empty(), "{OPENED_EARLY}");
        assert!(question.questions.is_empty());
        assert!(matches!(
            question.source,
            Some(QuestionSource::Mcp { url: Some(ref shown), .. }) if shown == URL
        ));
        assert_eq!(response, ElicitResponse::unanswered(action));
        assert_eq!(opened(), expected);
    }

    #[test]
    fn an_address_that_fails_to_open_is_still_accepted() {
        let session = Session::new(None);
        let elicitor = FormElicitor {
            open_url: fail_open,
            ..session.elicitor()
        };

        let (response, ()) = smol::block_on(future::zip(
            elicitor.elicit(SERVER, request(None, url())),
            async {
                session.asked().await;
                session.reply(QUESTION_ACCEPTED_REPLY);
            },
        ));

        assert_eq!(response, ElicitResponse::unanswered(ElicitAction::Accept));
    }

    #[test_case(form(BRANCH, STRING_TYPE) ; "a_form")]
    #[test_case(url() ; "an_address")]
    fn a_withdrawn_question_is_closed(mode: ElicitMode) {
        let session = Session::new(None);
        let elicitor = session.elicitor();

        smol::block_on(future::or(
            async {
                elicitor.elicit(SERVER, request(None, mode)).await;
                unreachable!("nobody answered");
            },
            async {
                session.asked().await;
            },
        ));

        assert!(session.closed(), "{LEFT_OPEN}");
    }

    #[test]
    fn the_delegate_answers_a_plain_form() {
        let handle = engaged_handle(ScriptedProvider::new(vec![Step::Answer(
            DELEGATE_PICKS_MAIN,
        )]));
        let session = Session::new(Some(handle.clone()));

        let (response, ()) = session.ask(None, form(BRANCH, STRING_TYPE), future::ready(()));

        assert_eq!(response.action, ElicitAction::Accept);
        assert_eq!(response.by, Answerer::Autopilot);
        assert_eq!(response.content, content(BRANCH, json!(MAIN)));
        assert!(handle.question_claimed(MCP_CALL));
        assert!(!session.closed(), "{CLOSED_TWICE}");
    }

    #[test]
    fn an_answer_the_form_cannot_take_is_left_to_the_human() {
        let handle = engaged_handle(ScriptedProvider::new(vec![Step::Answer(
            DELEGATE_MISCOUNTS,
        )]));
        let session = Session::new(Some(handle));

        let (response, decision) = session.ask(None, form(COUNT, INTEGER_TYPE), async {
            let decision = session.decided().await;
            session.answer(&[THREE]);
            decision
        });

        assert_eq!(decision.action, ActionName::Escalate);
        assert_eq!(response.by, Answerer::User);
        assert_eq!(response.content, content(COUNT, json!(3)));
    }

    #[test_case(Some(McpServerKind::Executor), form(BRANCH, STRING_TYPE), None ; "executor")]
    #[test_case(None, form(PASSWORD, STRING_TYPE), None ; "a_credential")]
    #[test_case(None, url(), None ; "an_address")]
    #[test_case(None, empty_form(), None ; "a_form_without_fields")]
    #[test_case(None, form(BRANCH, STRING_TYPE), Some(PARENT_CALL) ; "a_subagent")]
    fn the_delegate_never_answers(
        kind: Option<McpServerKind>,
        mode: ElicitMode,
        parent: Option<&str>,
    ) {
        let provider = ScriptedProvider::new(Vec::new());
        let mut session = Session::new(Some(engaged_handle(provider.clone())));
        session.ctx.root_tool_use_id = parent.map(str::to_owned);

        let (response, ()) = session.ask(kind, mode, async {
            session.asked().await;
            session.reply(QUESTION_DECLINED_REPLY);
        });

        assert_eq!(response, ElicitResponse::unanswered(ElicitAction::Decline));
        assert!(provider.prompts().is_empty(), "{UNASKED}");
    }

    #[test]
    fn a_credential_is_masked_in_what_the_card_shows() {
        let session = Session::new(None);

        let (response, ()) = session.ask(None, form(PASSWORD, STRING_TYPE), async {
            session.asked().await;
            session.answer(&[MAIN]);
        });

        assert_eq!(response.content, content(PASSWORD, json!(MAIN)));
        assert_eq!(
            response.answers,
            [(PASSWORD.to_owned(), MASKED_ANSWER.to_owned())]
        );
    }

    #[test_case(true, true => true ; "a_front_end_that_asks")]
    #[test_case(true, false => false ; "a_front_end_that_never_asks")]
    #[test_case(false, true => false ; "a_call_without_the_answer_channel")]
    fn only_a_call_someone_can_answer_gets_an_elicitor(channel: bool, asks: bool) -> bool {
        let mut ctx = stub_ctx(&AgentMode::Build);
        if channel {
            ctx.user_response_rx = Some(Arc::new(Mutex::new(flume::unbounded().1)));
        }
        let factory = asks.then(|| Arc::new(QuestionForms) as Arc<dyn ElicitorFactory>);
        let mcp = stub_session(&[]).with_elicitors(factory);

        mcp.elicitor_for(&ctx).is_some()
    }

    #[test]
    fn a_servers_form_is_answered_on_the_answer_channel_end_to_end() {
        let session = Session::new(None);
        let elicitation = json!({
            "jsonrpc": "2.0",
            "id": ELICITATION_ID,
            "method": "elicitation/create",
            "params": {
                "message": MESSAGE,
                "requestedSchema": {"type": "object", "properties": {BRANCH: {"type": STRING_TYPE}}},
            },
        });
        let transport = StdioTransport::spawn(
            SERVER,
            "/bin/sh",
            &["-c".into(), ELICITING_SERVER.into()],
            &HashMap::from([(ELICITATION_ENV.into(), elicitation.to_string())]),
            CALL_TIMEOUT,
            LocalExecutionPolicy::Embedded,
            true,
        )
        .unwrap();
        let scope = CallScope::new(
            Some(Arc::new(session.elicitor())),
            None,
            CancelToken::none(),
        );

        let (called, ()) = smol::block_on(future::zip(
            transport.send_request(TOOLS_CALL_METHOD, None, &scope),
            async {
                session.asked().await;
                session.answer(&[MAIN]);
            },
        ));
        smol::block_on(transport.shutdown());

        let reply = &called.unwrap()["reply"];
        assert_eq!(reply["id"], ELICITATION_ID);
        assert_eq!(
            reply["result"],
            json!({"action": "accept", "content": {BRANCH: MAIN}})
        );
        let interactions = scope.take_interactions();
        assert_eq!(
            interactions[0].answers,
            [(BRANCH.to_owned(), MAIN.to_owned())]
        );
    }
}
