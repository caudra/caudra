//! A side question beside the conversation, answered on the agent loop with
//! read-only tools. It sends the conversation's own prefix, so the provider's
//! prompt cache still holds it, keeps its turns in a history the caller owns,
//! and leaves the session as it found it: nothing it does is published,
//! recorded, compacted, or delivered to the conversation.

use std::sync::Arc;

use async_lock::Mutex as AsyncMutex;
use caudra_config::CODE_TOOL_NAME;
use caudra_providers::Model;
use caudra_storage::usage_ledger::LedgerPurpose;
use futures_lite::future;
use thiserror::Error;

use super::budget::{Overrun, RunBudget, Spend};
use super::{Agent, AgentParams, AgentRunParams, History};
use crate::cancel::CancelTrigger;
use crate::context::RequestPrefix;
use crate::tools::{
    AUTOMATION_TOOL_NAME, BATCH_TOOL_NAME, FILE_GLOB_TOOL_NAME, FILE_GREP_TOOL_NAME,
    FILE_INDEX_TOOL_NAME, FILE_READ_TOOL_NAME, SHELL_TOOL_NAME, SKILL_TOOL_NAME,
    SOURCE_CODE_CONTEXT_TOOL_NAME, SOURCE_CODE_REFS_TOOL_NAME, TOOL_OUTPUT_TOOL_NAME, ToolContext,
    ToolFilter, VIEW_IMAGE_TOOL_NAME, WEBFETCH_TOOL_NAME, WEBSEARCH_TOOL_NAME,
};
use crate::types::{AgentEvent, Envelope};
use crate::{AgentError, AgentInput, AgentMode, DoneReason};

/// What a side question may call. Each call is also held to read-only
/// effects when it runs, so `shell` and `code` reach only their reads. The
/// last is never declared: `code`'s `automation` module calls it. The `host`
/// module's `execution_environment` is not here: its probes can refresh
/// credentials or run sudo policy hooks, so it is no pure read.
pub const ASIDE_TOOLS: &[&str] = &[
    FILE_READ_TOOL_NAME,
    FILE_GLOB_TOOL_NAME,
    FILE_GREP_TOOL_NAME,
    FILE_INDEX_TOOL_NAME,
    SOURCE_CODE_CONTEXT_TOOL_NAME,
    SOURCE_CODE_REFS_TOOL_NAME,
    WEBSEARCH_TOOL_NAME,
    WEBFETCH_TOOL_NAME,
    SHELL_TOOL_NAME,
    CODE_TOOL_NAME,
    SKILL_TOOL_NAME,
    BATCH_TOOL_NAME,
    TOOL_OUTPUT_TOOL_NAME,
    VIEW_IMAGE_TOOL_NAME,
    AUTOMATION_TOOL_NAME,
];
/// Tool calls one side question may make before it is stopped, each call a
/// `code` snippet makes on the host among them.
pub const MAX_ASIDE_TOOL_CALLS: usize = 25;
/// Tokens one side question may spend, as [`RunBudget`] counts them, before
/// it is stopped.
pub const MAX_ASIDE_TOKENS: u64 = 200_000;
/// Restated whenever the side history does not already end on it, as the
/// mode contract of the run. It carries the contract's heading, so it stands
/// in for whatever mode the conversation last announced.
pub const ASIDE_REMINDER: &str = "<system-reminder>\n# Host mode contract\n\nThe user is \
asking a side question beside the conversation, not continuing it. Answer that question.\n\
- You have read-only tools: you may read and search files, run read-only commands, and look \
things up, but you cannot change files or any other state.\n\
- The workspace is read as it is now, which may differ from what the conversation above saw.\n\
- Nothing here enters the main conversation.\n</system-reminder>";
const ASIDE_BUDGET: RunBudget =
    RunBudget::counting_host_calls(MAX_ASIDE_TOOL_CALLS, MAX_ASIDE_TOKENS);

#[derive(Debug, Error)]
pub enum AsideError {
    #[error(transparent)]
    Agent(#[from] AgentError),
    #[error("the side question {0}")]
    OverBudget(Overrun),
}

/// Answers `question` over `history`, the conversation the side question
/// forked plus its own earlier exchanges, which gains this exchange.
///
/// `ctx` is the session's, built for this run: its event sender receives
/// every event the run sends, and cancelling its token stops the run. The
/// run's calls are rooted at `root`. The request goes out as `prefix` sends
/// it, byte for byte under its cache key.
///
/// Nobody answers the run. Its answer channel is closed, so a re-auth or a
/// server's question ends at once rather than taking an answer meant for
/// the conversation. Approvals reach the permission manager, not this
/// channel.
///
/// Each request's spend reaches the sender once as
/// [`AgentEvent::ModelUsage`] under [`LedgerPurpose::Btw`], as it happens,
/// so a run that later fails or is cancelled is still billed. Its
/// `TurnComplete` carries the same purpose and is for display only. A cap
/// stops the run only while it would go on: an answer that went past one
/// is still the answer.
pub async fn run_aside(
    ctx: &ToolContext,
    root: String,
    prefix: &RequestPrefix,
    history: &mut History,
    question: String,
) -> Result<DoneReason, AsideError> {
    let (agent_tx, agent_rx) = flume::unbounded();
    let (stop, cancel) = ctx.cancel.child();
    let (_, unanswered) = flume::unbounded();
    let params = AgentParams {
        cache_key: prefix.cache_key.clone(),
        tool_filter: aside_filter(ctx, &prefix.model),
        ..AgentParams::from_context(
            ctx,
            Arc::clone(&prefix.provider),
            prefix.model.clone(),
            root,
        )
    };
    let mut agent = Agent::new_verbatim(
        params,
        AgentRunParams {
            history,
            system: prefix.system.clone(),
            environment: None,
            instructions: None,
            memory: None,
            configuration: None,
            mode_notice: Some(ASIDE_REMINDER.to_owned()),
            event_tx: ctx.event_tx.rebind(agent_tx),
            tools: prefix.tools.clone(),
            tools_fixed: true,
            memory_store: None,
        },
    )
    .with_mcp(ctx.mcp.clone())
    .with_cancel(cancel)
    .with_auto_compact(false)
    .with_user_response_rx(Arc::new(AsyncMutex::new(unanswered)));
    let input = AgentInput {
        message: question,
        mode: AgentMode::ReadOnly,
        plan: None,
        images: Vec::new(),
        mentions: Vec::new(),
        commits: Vec::new(),
        preamble: Vec::new(),
        thinking: prefix.opts.thinking.clone(),
        fast: prefix.opts.fast,
        prompt: None,
        resume: false,
    };
    let mut watch = Watch {
        budget: ASIDE_BUDGET,
        spend: Spend {
            events: ctx.event_tx.clone(),
            ledger: None,
        },
        stop: Some(stop),
        overrun: None,
    };
    let relay = async {
        while let Ok(envelope) = agent_rx.recv_async().await {
            let answered = matches!(
                &envelope.event,
                AgentEvent::TurnComplete(turn) if !turn.message.has_tool_calls()
            );
            watch.take(envelope).await;
            if !answered {
                watch.stop_if_over();
            }
        }
        future::pending().await
    };
    let done = future::or(agent.run(input), relay).await;
    while let Ok(envelope) = agent_rx.try_recv() {
        watch.take(envelope).await;
    }
    let finished = matches!(done, Ok(reason) if reason != DoneReason::Cancelled);
    match watch.overrun {
        Some(overrun) if !finished => Err(AsideError::OverBudget(overrun)),
        _ => Ok(done?),
    }
}

/// The allowlist, narrowed by the configuration and the session's ceiling,
/// and held to read-only effects.
fn aside_filter(ctx: &ToolContext, model: &Model) -> ToolFilter {
    ToolFilter::Only(ASIDE_TOOLS.iter().map(|name| (*name).to_owned()).collect())
        .intersect(&ToolFilter::from_config(&ctx.config, model, &[]))
        .intersect(&ctx.tool_ceiling)
        .for_mode(&AgentMode::ReadOnly)
}

/// Relays the run's events, settling each request's spend on the way and
/// stopping the run at the first cap it passes.
struct Watch {
    budget: RunBudget,
    spend: Spend,
    stop: Option<CancelTrigger>,
    overrun: Option<Overrun>,
}

impl Watch {
    async fn take(&mut self, mut envelope: Envelope) {
        self.budget.observe(&envelope.event);
        if let AgentEvent::TurnComplete(turn) = &mut envelope.event {
            turn.purpose = LedgerPurpose::Btw;
            self.spend.report(turn, LedgerPurpose::Btw).await;
        }
        let _ = self.spend.events.send_envelope(envelope);
    }

    fn stop_if_over(&mut self) {
        if let Some(overrun) = self.budget.overrun()
            && let Some(stop) = self.stop.take()
        {
            self.overrun = Some(overrun);
            stop.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::collections::{HashSet, VecDeque};
    use std::path::PathBuf;
    use std::sync::Mutex;

    use caudra_config::decisions::DecisionsConfig;
    use caudra_config::{AgentConfig, DefaultEffect, FeatureFlags, PermissionsConfig};
    use caudra_providers::provider::{BoxFuture, Provider};
    use caudra_providers::{
        CacheKey, ContentBlock, Message, ModelInfo, ProviderEvent, RequestOptions, Role,
        StopReason, StreamResponse, TokenUsage,
    };
    use caudra_storage::StateDir;
    use serde_json::{Map, Value, json};
    use test_case::test_case;

    use super::*;
    use crate::agent::tool_dispatch::{MCP_NAME_SEPARATOR, MCP_THROUGH_CODE, TOOL_DISABLED_SUFFIX};
    use crate::cancel::CancelToken;
    use crate::code::names::{HOST_MODULE, IMAGE_MODULE};
    use crate::code::runner::{
        RunFuture, ScriptCall, ScriptInvocation, ScriptOutcome, ScriptOutput, ScriptRun,
        ScriptRunner, ScriptStatus, install,
    };
    use crate::code::tool_module::READ_ONLY_UNAVAILABLE;
    use crate::context::{ContextKey, ContextStore};
    use crate::decisions::Decisions;
    use crate::mcp::recognised_stub_session;
    use crate::permissions::{PermissionManager, PermissionMode};
    use crate::prompt::TASK_MODE_MARKER;
    use crate::tools::execution::configure_declared;
    use crate::tools::native::skill::{BuiltinSkill, install_builtin_skill};
    use crate::tools::native::{self, peers};
    use crate::tools::registry::{
        ExecFuture, HeaderFuture, HeaderResult, ParseError, PermissionScopes, Tool, ToolExecResult,
        ToolInvocation, ToolRegistry,
    };
    use crate::tools::test_support::NamedMock;
    use crate::tools::{
        DescriptionContext, EXECUTION_ENVIRONMENT_TOOL_NAME, FILE_EDIT_TOOL_NAME,
        FILE_WRITE_TOOL_NAME, FileReadTracker, READ_ONLY_TOOL_RESTRICTED, TASK_TOOL_NAME,
        ToolEffect, interpreter_ctx,
    };
    use crate::types::ToolOutput;
    use crate::{Envelope, EventSender};

    const MODEL_SPEC: &str = "anthropic/claude-sonnet-4-5";
    const WINDOW: u32 = 50_000;
    const NEAR_FULL: u32 = WINDOW - 1;
    const OVER_BUDGET: u32 = 200_001;
    const PROJECT: &str = "/tmp";
    const PROJECT_FILE: &str = "aside.rs";
    const SYSTEM: &str = "the main conversation's system prompt";
    const CACHE_TASK: &str = "main";
    const BASE: &str = "earlier in the conversation";
    const QUESTION: &str = "what does run_aside return?";
    const FOLLOW_UP: &str = "and when does it fail?";
    const ANSWER: &str = "the done reason";
    const CALL_ID: &str = "toolu_aside";
    const RAN: &str = "the mock ran";
    const COMMAND_FIELD: &str = "command";
    const READ_ONLY_COMMAND: &str = "git status";
    const WRITING_COMMAND: &str = "touch x";
    const SKILL_NAME: &str = "aside-test-skill";
    const SKILL_BODY: &str = "how to answer a side question";
    const LOOKUP_SERVER: &str = "home-assistant";
    const LOOKUP_TOOL: &str = "stub.lookup";
    const LOOKUP_NAMESPACE: &str = "stub";
    const LOOKUP_METHOD: &str = "lookup";
    const LOOKUP_RESULT: &str = "the kitchen light is on";
    const SNIPPET: &str = "await stub.lookup()";
    const ENVIRONMENT_METHOD: &str = "environment";
    const GENERATE_METHOD: &str = "generate";
    const ROOT: &str = "aside-call";
    const MAIN_ANSWER: &str = "an answer the main chat is owed";
    const AUTH_ERROR_STATUS: u16 = 401;
    const AUTH_ERROR_MESSAGE: &str = "expired";
    const DECISION_PROVIDER: &str = "decider";
    const DECISION_MODEL: &str = "decider-1";
    const DECISION_TOKENS: u32 = 12;
    const ONE_TURN: u32 = 1;
    const MOCKS: &[(&str, ToolEffect, bool)] = &[
        (FILE_READ_TOOL_NAME, ToolEffect::ReadOnly, false),
        (EXECUTION_ENVIRONMENT_TOOL_NAME, ToolEffect::Mutating, false),
        (FILE_GREP_TOOL_NAME, ToolEffect::ReadOnly, false),
        (FILE_WRITE_TOOL_NAME, ToolEffect::Mutating, false),
        (FILE_EDIT_TOOL_NAME, ToolEffect::Mutating, false),
        (SHELL_TOOL_NAME, ToolEffect::Mutating, true),
    ];

    struct Request {
        system: String,
        tools: Value,
        cache_key: Option<CacheKey>,
        messages: Vec<Message>,
    }

    /// Answers each request with the next scripted response after yielding
    /// once, so the events of the turn before reach the relay first.
    struct ScriptedProvider {
        responses: Mutex<VecDeque<Result<StreamResponse, AgentError>>>,
        requests: Arc<Mutex<Vec<Request>>>,
    }

    impl Provider for ScriptedProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            messages: &'a [Message],
            system: &'a str,
            tools: &'a Value,
            _: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            cache_key: Option<&'a CacheKey>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async move {
                self.requests.lock().unwrap().push(Request {
                    system: system.to_owned(),
                    tools: tools.clone(),
                    cache_key: cache_key.cloned(),
                    messages: messages.to_vec(),
                });
                future::yield_now().await;
                self.responses
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("a scripted response")
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn adjust_model(&self, model: &mut Model) {
            model.context_window = WINDOW;
        }
    }

    /// Stands in for a Workcell tool under its real name and registered
    /// effect. A `shell` call narrows to read-only for `READ_ONLY_COMMAND`
    /// alone, as the real classifier would.
    struct Mock {
        name: &'static str,
        read_only_calls: bool,
    }

    struct MockCall {
        command: Option<String>,
    }

    impl Tool for Mock {
        fn name(&self) -> &str {
            self.name
        }

        fn description(&self, _: &DescriptionContext) -> Cow<'_, str> {
            RAN.into()
        }

        fn schema(&self) -> Value {
            json!({"type": "object"})
        }

        fn has_read_only_calls(&self) -> bool {
            self.read_only_calls
        }

        fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
            Ok(Box::new(MockCall {
                command: input
                    .get(COMMAND_FIELD)
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            }))
        }
    }

    impl ToolInvocation for MockCall {
        fn start_header(&self) -> HeaderFuture {
            HeaderFuture::Ready(HeaderResult::plain(RAN.into()))
        }

        fn permission_scopes(&self) -> BoxFuture<'_, Option<PermissionScopes>> {
            let scope = self
                .command
                .clone()
                .unwrap_or_else(|| format!("{PROJECT}/{PROJECT_FILE}"));
            Box::pin(future::ready(Some(PermissionScopes::single(scope))))
        }

        fn call_effect(&self, registered: ToolEffect) -> ToolEffect {
            match self.command.as_deref() {
                Some(READ_ONLY_COMMAND) => ToolEffect::ReadOnly,
                _ => registered,
            }
        }

        fn execute<'a>(self: Box<Self>, _: &'a ToolContext) -> ExecFuture<'a> {
            Box::pin(async { ToolExecResult::from(Ok::<_, String>(ToolOutput::Plain(RAN.into()))) })
        }
    }

    /// Runs any snippet as `times` `namespace.method()` calls in turn and
    /// returns what the last answered.
    struct CallingRunner {
        namespace: &'static str,
        method: &'static str,
        times: usize,
    }

    impl CallingRunner {
        fn once(namespace: &'static str, method: &'static str) -> Self {
            Self {
                namespace,
                method,
                times: 1,
            }
        }
    }

    impl ScriptRunner for CallingRunner {
        fn run(&self, run: ScriptRun) -> RunFuture {
            let (namespace, method, times) = (self.namespace, self.method, self.times);
            Box::pin(async move {
                let mut reply = Ok(Value::Null);
                for _ in 0..times {
                    let invocation = run.host.call(ScriptCall {
                        namespace: Some(namespace.to_owned()),
                        name: method.to_owned(),
                        args: Vec::new(),
                        kwargs: Map::new(),
                    });
                    reply = match invocation {
                        ScriptInvocation::Ready(reply) => reply,
                        ScriptInvocation::Deferred(future) => future.await,
                    };
                }
                ScriptOutcome {
                    output: Some(ScriptOutput {
                        result: reply.unwrap_or_else(|error| Value::from(error.message)),
                        diagnostic: None,
                        ..ScriptOutput::refused(ScriptStatus::Completed, String::new())
                    }),
                    ..ScriptOutcome::default()
                }
            })
        }
    }

    struct Fixture {
        ctx: ToolContext,
        events: flume::Receiver<Envelope>,
        requests: Arc<Mutex<Vec<Request>>>,
        prefix: RequestPrefix,
    }

    impl Fixture {
        fn new(responses: Vec<StreamResponse>, mode: PermissionMode) -> Self {
            Self::replying(responses.into_iter().map(Ok).collect(), mode)
        }

        fn replying(
            replies: VecDeque<Result<StreamResponse, AgentError>>,
            mode: PermissionMode,
        ) -> Self {
            let (tx, events) = flume::unbounded();
            let permissions = Arc::new(PermissionManager::new_nonpersistent(
                PermissionsConfig {
                    default: DefaultEffect::Allow,
                    rules: vec![],
                    ..Default::default()
                },
                PathBuf::from(PROJECT),
                Arc::default(),
            ));
            permissions.set_session_mode(Some(mode));
            let registry = Arc::new(ToolRegistry::new());
            native::register(&registry, FeatureFlags::default()).unwrap();
            for &(name, effect, read_only_calls) in MOCKS {
                registry
                    .register_audited(
                        Arc::new(Mock {
                            name,
                            read_only_calls,
                        }),
                        NamedMock::source(),
                        effect,
                    )
                    .unwrap();
            }
            let ctx = interpreter_ctx(
                &AgentMode::Build,
                &EventSender::new(tx, 0),
                CancelToken::none(),
                permissions,
                FileReadTracker::fresh(),
                None,
                registry,
            );
            let requests = Arc::default();
            let provider = Arc::new(ScriptedProvider {
                responses: Mutex::new(replies),
                requests: Arc::clone(&requests),
            });
            Self {
                ctx,
                events,
                requests,
                prefix: main_prefix(provider),
            }
        }

        fn ask(&self, history: &mut History, question: &str) -> Result<DoneReason, AsideError> {
            let asked = run_aside(
                &self.ctx,
                ROOT.to_owned(),
                &self.prefix,
                history,
                question.to_owned(),
            );
            assert_send(&asked);
            smol::block_on(asked)
        }

        fn events(&self) -> Vec<AgentEvent> {
            self.events
                .try_iter()
                .map(|envelope| envelope.event)
                .collect()
        }
    }

    fn assert_send<T: Send>(_: &T) {}

    /// The main conversation's tools, configured for a session with
    /// background work, peers among them.
    fn main_tools() -> Value {
        let mut tools = Value::Array(
            [
                FILE_READ_TOOL_NAME,
                SHELL_TOOL_NAME,
                TASK_TOOL_NAME,
                peers::TOOL_NAMES[0],
            ]
            .iter()
            .map(|name| {
                json!({
                    "name": name,
                    "description": RAN,
                    "input_schema": {"type": "object", "properties": {}},
                })
            })
            .collect(),
        );
        configure_declared(&mut tools, &AgentConfig::default(), true, true);
        tools
    }

    fn main_prefix(provider: Arc<dyn Provider>) -> RequestPrefix {
        RequestPrefix {
            provider,
            model: Model::from_spec(MODEL_SPEC).unwrap(),
            system: SYSTEM.to_owned(),
            tools: main_tools(),
            opts: RequestOptions::default(),
            cache_key: Some(CacheKey::task(None, CACHE_TASK)),
        }
    }

    fn forked() -> History {
        History::new(vec![
            Message::user(BASE.to_owned()),
            assistant(vec![ContentBlock::Text { text: BASE.into() }]),
        ])
    }

    fn assistant(content: Vec<ContentBlock>) -> Message {
        Message {
            role: Role::Assistant,
            content,
            ..Default::default()
        }
    }

    fn response(content: Vec<ContentBlock>, stop: StopReason, input: u32) -> StreamResponse {
        StreamResponse {
            message: assistant(content),
            usage: TokenUsage {
                input,
                ..TokenUsage::default()
            },
            stop_reason: Some(stop),
            ..Default::default()
        }
    }

    fn answer() -> StreamResponse {
        response(
            vec![ContentBlock::Text {
                text: ANSWER.into(),
            }],
            StopReason::EndTurn,
            0,
        )
    }

    fn calling(tool: &str, input: Value, usage: u32) -> Vec<StreamResponse> {
        vec![
            response(
                vec![ContentBlock::tool_use(CALL_ID, tool, input)],
                StopReason::ToolUse,
                usage,
            ),
            answer(),
        ]
    }

    /// The result the call got, and whether it was an error.
    fn result(history: &History) -> (String, bool) {
        history
            .as_slice()
            .iter()
            .flat_map(|message| &message.content)
            .find_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    ..
                } if tool_use_id == CALL_ID => Some((content.clone(), *is_error)),
                _ => None,
            })
            .expect("the call has a result")
    }

    fn texts(messages: &[Message]) -> Vec<String> {
        messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_prefix_goes_out_byte_for_byte_under_its_cache_key() {
        let fixture = Fixture::new(vec![answer()], PermissionMode::Ask);
        let mut rebuilt = fixture.prefix.tools.clone();
        configure_declared(&mut rebuilt, &AgentConfig::default(), false, false);
        assert_ne!(
            rebuilt, fixture.prefix.tools,
            "the tools differ for a run without jobs"
        );

        let mut history = forked();
        assert_eq!(
            fixture.ask(&mut history, QUESTION).unwrap(),
            DoneReason::EndTurn
        );

        let requests = fixture.requests.lock().unwrap();
        let [request] = requests.as_slice() else {
            panic!("one request");
        };
        assert_eq!(request.system, SYSTEM);
        assert_eq!(
            serde_json::to_string(&request.tools).unwrap(),
            serde_json::to_string(&fixture.prefix.tools).unwrap()
        );
        assert_eq!(request.cache_key, fixture.prefix.cache_key);
        let sent = texts(&request.messages);
        assert_eq!(sent[..2], [BASE, BASE]);
        assert!(sent.iter().any(|text| text == QUESTION), "{sent:?}");
        assert!(sent.iter().any(|text| text == ASIDE_REMINDER), "{sent:?}");
    }

    #[test]
    fn the_reminder_is_the_mode_contract_and_rides_once_per_thread() {
        assert!(ASIDE_REMINDER.contains(TASK_MODE_MARKER));
        let fixture = Fixture::new(vec![answer(), answer()], PermissionMode::Ask);
        let mut history = forked();
        fixture.ask(&mut history, QUESTION).unwrap();
        fixture.ask(&mut history, FOLLOW_UP).unwrap();

        let reminders = texts(history.as_slice())
            .iter()
            .filter(|text| *text == ASIDE_REMINDER)
            .count();
        assert_eq!(reminders, 1);
        assert_eq!(texts(history.as_slice()).last().unwrap(), ANSWER);
    }

    #[test_case(FILE_READ_TOOL_NAME, json!({}), RAN ; "file_read")]
    #[test_case(FILE_GREP_TOOL_NAME, json!({}), RAN ; "file_grep")]
    #[test_case(SHELL_TOOL_NAME, json!({COMMAND_FIELD: READ_ONLY_COMMAND}), RAN ; "read_only_shell")]
    #[test_case(SKILL_TOOL_NAME, json!({"name": SKILL_NAME}), SKILL_BODY ; "skill")]
    fn a_read_runs(tool: &str, input: Value, expected: &str) {
        install_builtin_skill(BuiltinSkill {
            name: SKILL_NAME.into(),
            description: SKILL_BODY.into(),
            resolve: Box::new(|_| (SKILL_BODY.into(), None)),
            pages: None,
        });
        let fixture = Fixture::new(calling(tool, input, 0), PermissionMode::Ask);
        let mut history = forked();
        assert_eq!(
            fixture.ask(&mut history, QUESTION).unwrap(),
            DoneReason::EndTurn
        );
        let (content, is_error) = result(&history);
        assert!(!is_error && content.contains(expected), "{content}");
    }

    #[test_case(FILE_WRITE_TOOL_NAME, json!({}), READ_ONLY_TOOL_RESTRICTED, PermissionMode::Ask ; "file_write_ask")]
    #[test_case(FILE_WRITE_TOOL_NAME, json!({}), READ_ONLY_TOOL_RESTRICTED, PermissionMode::Auto ; "file_write_auto")]
    #[test_case(FILE_WRITE_TOOL_NAME, json!({}), READ_ONLY_TOOL_RESTRICTED, PermissionMode::Yolo ; "file_write_yolo")]
    #[test_case(FILE_EDIT_TOOL_NAME, json!({}), READ_ONLY_TOOL_RESTRICTED, PermissionMode::Ask ; "file_edit_ask")]
    #[test_case(FILE_EDIT_TOOL_NAME, json!({}), READ_ONLY_TOOL_RESTRICTED, PermissionMode::Auto ; "file_edit_auto")]
    #[test_case(FILE_EDIT_TOOL_NAME, json!({}), READ_ONLY_TOOL_RESTRICTED, PermissionMode::Yolo ; "file_edit_yolo")]
    #[test_case(SHELL_TOOL_NAME, json!({COMMAND_FIELD: WRITING_COMMAND}), READ_ONLY_TOOL_RESTRICTED, PermissionMode::Ask ; "writing_shell_ask")]
    #[test_case(SHELL_TOOL_NAME, json!({COMMAND_FIELD: WRITING_COMMAND}), READ_ONLY_TOOL_RESTRICTED, PermissionMode::Auto ; "writing_shell_auto")]
    #[test_case(SHELL_TOOL_NAME, json!({COMMAND_FIELD: WRITING_COMMAND}), READ_ONLY_TOOL_RESTRICTED, PermissionMode::Yolo ; "writing_shell_yolo")]
    #[test_case(TASK_TOOL_NAME, json!({}), TOOL_DISABLED_SUFFIX, PermissionMode::Ask ; "task_ask")]
    #[test_case(TASK_TOOL_NAME, json!({}), TOOL_DISABLED_SUFFIX, PermissionMode::Auto ; "task_auto")]
    #[test_case(TASK_TOOL_NAME, json!({}), TOOL_DISABLED_SUFFIX, PermissionMode::Yolo ; "task_yolo")]
    fn a_write_is_refused_in_every_permission_mode(
        tool: &str,
        input: Value,
        refusal: &str,
        mode: PermissionMode,
    ) {
        let fixture = Fixture::new(calling(tool, input, 0), mode);
        let mut history = forked();
        assert_eq!(
            fixture.ask(&mut history, QUESTION).unwrap(),
            DoneReason::EndTurn
        );
        let (content, is_error) = result(&history);
        assert!(is_error && content.contains(refusal), "{content}");
        assert!(!content.contains(RAN), "{content}");
    }

    #[test_case(true, true ; "a_read_only_mcp_call_runs")]
    #[test_case(false, false ; "a_writing_mcp_call_is_refused")]
    fn code_reaches_only_read_only_host_calls(annotated_read_only: bool, runs: bool) {
        install(Arc::new(CallingRunner::once(
            LOOKUP_NAMESPACE,
            LOOKUP_METHOD,
        )));
        let mut fixture = Fixture::new(
            calling(CODE_TOOL_NAME, json!({"code": SNIPPET}), 0),
            PermissionMode::Yolo,
        );
        fixture.ctx.mcp = Some(recognised_stub_session(
            LOOKUP_SERVER,
            &[(LOOKUP_TOOL, annotated_read_only)],
            json!({"content": [{"type": "text", "text": LOOKUP_RESULT}]}),
        ));
        let mut history = forked();
        fixture.ask(&mut history, QUESTION).unwrap();
        let (content, _) = result(&history);
        assert_eq!(content.contains(LOOKUP_RESULT), runs, "{content}");
    }

    /// MCP tools are never declared, so a call by a server tool's name runs
    /// nothing, read-only or not, and points the model to `code`.
    #[test_case(true ; "read_only")]
    #[test_case(false ; "writing")]
    fn a_direct_mcp_call_points_to_code(annotated_read_only: bool) {
        let direct = format!("{LOOKUP_SERVER}{MCP_NAME_SEPARATOR}{LOOKUP_TOOL}");
        let mut fixture = Fixture::new(calling(&direct, json!({}), 0), PermissionMode::Yolo);
        fixture.ctx.mcp = Some(recognised_stub_session(
            LOOKUP_SERVER,
            &[(LOOKUP_TOOL, annotated_read_only)],
            json!({"content": [{"type": "text", "text": LOOKUP_RESULT}]}),
        ));
        let mut history = forked();
        fixture.ask(&mut history, QUESTION).unwrap();
        let (content, is_error) = result(&history);
        assert!(is_error && content.contains(MCP_THROUGH_CODE), "{content}");
        assert!(!content.contains(LOOKUP_RESULT), "{content}");
    }

    /// The host's environment is no pure read: its probes can refresh
    /// credentials, so it is refused like a write.
    #[test_case(HOST_MODULE, ENVIRONMENT_METHOD ; "host_environment_is_refused")]
    #[test_case(IMAGE_MODULE, GENERATE_METHOD ; "image_generate_is_refused")]
    fn code_modules_that_only_change_are_refused(namespace: &'static str, method: &'static str) {
        install(Arc::new(CallingRunner::once(namespace, method)));
        let fixture = Fixture::new(
            calling(CODE_TOOL_NAME, json!({"code": SNIPPET}), 0),
            PermissionMode::Yolo,
        );
        let mut history = forked();
        fixture.ask(&mut history, QUESTION).unwrap();
        let (content, _) = result(&history);
        assert!(content.contains(READ_ONLY_UNAVAILABLE), "{content}");
        assert!(!content.contains(RAN), "{content}");
    }

    #[test]
    fn a_side_question_never_compacts_publishes_or_admits_an_exchange() {
        let mut fixture = Fixture::new(
            calling(FILE_READ_TOOL_NAME, json!({}), NEAR_FULL),
            PermissionMode::Ask,
        );
        let store = ContextStore::new();
        fixture.ctx.context_publisher = Some(store.publisher(ContextKey::Main));
        let mut history = forked();
        assert_eq!(
            fixture.ask(&mut history, QUESTION).unwrap(),
            DoneReason::EndTurn
        );

        assert_eq!(fixture.requests.lock().unwrap().len(), 2);
        let events = fixture.events();
        assert!(!events.iter().any(|event| matches!(
            event,
            AgentEvent::Compacting | AgentEvent::UserExchangeAdmitted { .. }
        )));
        assert!(store.latest(&ContextKey::Main).is_none());
        assert!(store.request_prefix(&ContextKey::Main).is_none());
    }

    #[test]
    fn each_request_is_billed_once_as_btw() {
        let fixture = Fixture::new(
            calling(FILE_READ_TOOL_NAME, json!({}), NEAR_FULL),
            PermissionMode::Ask,
        );
        let mut history = forked();
        fixture.ask(&mut history, QUESTION).unwrap();

        let events = fixture.events();
        let billed: Vec<(LedgerPurpose, u32)> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ModelUsage { purpose, usage, .. } => Some((*purpose, usage.input)),
                _ => None,
            })
            .collect();
        assert_eq!(
            billed,
            [(LedgerPurpose::Btw, NEAR_FULL), (LedgerPurpose::Btw, 0)]
        );
        assert!(events.iter().all(|event| match event {
            AgentEvent::TurnComplete(turn) => turn.purpose == LedgerPurpose::Btw,
            _ => true,
        }));
    }

    #[test]
    fn the_token_cap_stops_a_run_that_would_go_on_and_still_bills_it() {
        let mut responses = calling(FILE_READ_TOOL_NAME, json!({}), 0);
        responses[0].usage.output = OVER_BUDGET;
        let fixture = Fixture::new(responses, PermissionMode::Ask);
        let mut history = forked();
        let error = fixture.ask(&mut history, QUESTION).unwrap_err();
        assert!(
            matches!(error, AsideError::OverBudget(Overrun::Tokens)),
            "{error}"
        );
        assert!(fixture.events().iter().any(|event| matches!(
            event,
            AgentEvent::ModelUsage {
                purpose: LedgerPurpose::Btw,
                ..
            }
        )));
    }

    #[test]
    fn an_answer_that_went_past_the_token_cap_is_still_the_answer() {
        let mut spent = answer();
        spent.usage.output = OVER_BUDGET;
        let fixture = Fixture::new(vec![spent], PermissionMode::Ask);
        let mut history = forked();
        assert_eq!(
            fixture.ask(&mut history, QUESTION).unwrap(),
            DoneReason::EndTurn
        );
        assert_eq!(texts(history.as_slice()).last().unwrap(), ANSWER);
    }

    /// The conversation's prompt is what the side question was handed, so a
    /// cold cache spends none of its budget.
    #[test]
    fn the_forked_prompt_spends_none_of_the_budget() {
        let mut responses = calling(FILE_READ_TOOL_NAME, json!({}), OVER_BUDGET);
        responses[0].usage.cache_creation = OVER_BUDGET;
        let fixture = Fixture::new(responses, PermissionMode::Ask);
        let mut history = forked();
        assert_eq!(
            fixture.ask(&mut history, QUESTION).unwrap(),
            DoneReason::EndTurn
        );
    }

    #[test]
    fn each_host_call_of_a_snippet_counts_toward_the_call_cap() {
        install(Arc::new(CallingRunner {
            namespace: LOOKUP_NAMESPACE,
            method: LOOKUP_METHOD,
            times: MAX_ASIDE_TOOL_CALLS,
        }));
        let mut fixture = Fixture::new(
            calling(CODE_TOOL_NAME, json!({"code": SNIPPET}), 0),
            PermissionMode::Yolo,
        );
        fixture.ctx.mcp = Some(recognised_stub_session(
            LOOKUP_SERVER,
            &[(LOOKUP_TOOL, true)],
            json!({"content": [{"type": "text", "text": LOOKUP_RESULT}]}),
        ));
        let mut history = forked();
        let error = fixture.ask(&mut history, QUESTION).unwrap_err();
        assert!(
            matches!(error, AsideError::OverBudget(Overrun::ToolCalls)),
            "{error}"
        );
    }

    /// A re-auth reads the answer channel. The side question's own is
    /// closed, so it ends at once and the main chat's answer stays queued.
    #[test]
    fn a_side_question_never_takes_an_answer_meant_for_the_main_chat() {
        let mut fixture = Fixture::replying(
            VecDeque::from([
                Err(AgentError::api(AUTH_ERROR_STATUS, AUTH_ERROR_MESSAGE)),
                Ok(answer()),
            ]),
            PermissionMode::Ask,
        );
        let (answers, main) = flume::unbounded();
        answers.send(MAIN_ANSWER.to_owned()).unwrap();
        fixture.ctx.user_response_rx = Some(Arc::new(AsyncMutex::new(main.clone())));
        let mut history = forked();

        let done = fixture.ask(&mut history, QUESTION);

        assert!(
            !matches!(done, Ok(DoneReason::EndTurn)),
            "the run must end unanswered"
        );
        assert_eq!(main.try_recv().as_deref(), Ok(MAIN_ANSWER));
        assert_eq!(fixture.requests.lock().unwrap().len(), 1);
    }

    /// Decision spend queues on the permission manager the side question
    /// shares with the conversation, which books it, so none of it shows as
    /// the side question's.
    #[test]
    fn the_conversations_decision_spend_stays_queued() {
        let state = tempfile::tempdir().unwrap();
        let decisions = Decisions::new(
            DecisionsConfig::default(),
            &StateDir::from_path(state.path().to_owned()),
        )
        .unwrap();
        decisions.queue_spend(
            DECISION_PROVIDER,
            DECISION_MODEL,
            TokenUsage {
                input: DECISION_TOKENS,
                ..TokenUsage::default()
            },
        );
        let fixture = Fixture::new(vec![answer()], PermissionMode::Ask);
        fixture
            .ctx
            .permissions
            .set_decisions(Some(decisions.clone()));
        let mut history = forked();
        fixture.ask(&mut history, QUESTION).unwrap();

        assert!(!fixture.events().iter().any(|event| matches!(
            event,
            AgentEvent::ModelUsage {
                purpose: LedgerPurpose::Decision,
                ..
            }
        )));
        assert_eq!(decisions.take_spend().len(), 1);
    }

    /// Whether every call in `messages` has its result in the message after it.
    fn paired(messages: &[Message]) -> bool {
        messages.iter().enumerate().all(|(index, message)| {
            let answered: HashSet<&str> = messages
                .get(index + 1)
                .into_iter()
                .flat_map(|next| &next.content)
                .filter_map(|block| match block {
                    ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                    _ => None,
                })
                .collect();
            message.tool_uses().all(|(id, _, _)| answered.contains(id))
        })
    }

    fn over_the_cap_calling() -> Fixture {
        let mut responses = calling(FILE_READ_TOOL_NAME, json!({}), 0);
        responses[0].usage.output = OVER_BUDGET;
        Fixture::new(responses, PermissionMode::Ask)
    }

    fn out_of_turns() -> Fixture {
        let mut fixture = Fixture::new(
            vec![
                calling(FILE_READ_TOOL_NAME, json!({}), 0).remove(0),
                answer(),
                answer(),
            ],
            PermissionMode::Ask,
        );
        fixture.ctx.config.max_turns = Some(ONE_TURN);
        fixture
    }

    /// An answer that ended short of its turn is kept, and the follow-up
    /// after it still sends every call with its result.
    #[test_case(over_the_cap_calling ; "token_cap_after_a_call")]
    #[test_case(out_of_turns ; "max_turns_after_a_call")]
    fn a_follow_up_after_an_unfinished_answer_sends_every_call_paired(setup: fn() -> Fixture) {
        let fixture = setup();
        let mut history = forked();
        let ended = fixture.ask(&mut history, QUESTION);
        assert!(!matches!(ended, Ok(DoneReason::EndTurn)), "{ended:?}");
        fixture.ask(&mut history, FOLLOW_UP).unwrap();

        let requests = fixture.requests.lock().unwrap();
        let follow_up = &requests.last().unwrap().messages;
        assert!(paired(follow_up), "{follow_up:?}");
        assert!(texts(follow_up).iter().any(|text| text == FOLLOW_UP));
    }
}
