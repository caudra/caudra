//! The gateway: everything a snippet run by the `mcp` tool can reach, and the only way code mode
//! touches a server.
//!
//! Every server call passes the checks a direct MCP call always did: the agent's tool policy, plan
//! and read-only modes, then the permission prompt, under an id of its own (`<run>:<n>`) so the
//! prompt and the run's card can find each other. A refusal raises `PermissionError` where the
//! call was made; only the user cancelling stops the run.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use async_lock::Mutex as AsyncMutex;
use caudra_config::ToolKey;
use caudra_storage::tool_ledger::ToolOutcome as LedgerOutcome;
use caudra_workflow::decision::{RerankRequest, decide_label, rerank_label};
use serde_json::{Map, Value, json};

use super::catalog::unavailable;
use super::decide::{self, DECIDE_PARAMS, Decide, RERANK_PARAMS, decide_request, rerank_request};
use super::naming::{
    CLASS_PREFIX, DECIDE_HELPER, DECISION_HELPERS, HELPERS, Names, READ_RESOURCE_HELPER,
    RERANK_HELPER, RESOURCES_HELPER, SEARCH_HELPER, SERVERS_HELPER,
};
use super::output::{
    CALL_SOURCE, McpCodeCall, McpCodeCallStatus, McpCodeOutcome, McpCodeOutput,
    McpCodeProgressEvent,
};
use super::runner::{
    ReplLease, ReplNote, ScriptCall, ScriptError, ScriptErrorKind, ScriptHost, ScriptInvocation,
    ScriptName, ScriptOutput, ScriptReply, ScriptRun, ScriptStatus, installed,
};
use super::search::{DEFAULT_LIMIT, search};
use super::signature::stubs;
use super::view::ServerView;
use crate::AgentEvent;
use crate::agent::change_recording::{self, Recording};
use crate::agent::tool_dispatch::error_type;
use crate::decisions::Decisions;
use crate::mcp::call::CallScope;
use crate::mcp::output::{McpBlock, McpHelper, McpInteraction, McpOutput, decoded_len};
use crate::mcp::resources::{ResourceContents, ResourceLists};
use crate::mcp::{McpSession, McpToolBinding, SEPARATOR};
use crate::permissions::{
    MCP_SERVER_RESOURCE, PermissionExecutorKind, PermissionResource, PermissionResourceAccess,
    PermissionResourceKind, PermissionRisk, mcp_permission_intent,
};
use crate::tools::native::batch::child_tool_use_id;
use crate::tools::registry::{PermissionIntent, PermissionScopes, TrustedToolSource};
use crate::tools::{
    MCP_TOOL_NAME, READ_ONLY_TOOL_RESTRICTED, ToolContext, ToolEffect, ToolFailure,
};

/// A result larger than this raises rather than reaching the snippet.
pub const MAX_RESULT_BYTES: usize = 8 * 1024 * 1024;
/// What a call's card keeps of its result; the rest becomes an excerpt.
pub const MAX_CALL_DISPLAY_BYTES: usize = 64 * 1024;

const PROPERTIES: &str = "properties";
const REQUIRED: &str = "required";
const ADDITIONAL_PROPERTIES: &str = "additionalProperties";
const QUERY_PARAM: &str = "query";
const SERVER_PARAM: &str = "server";
const LIMIT_PARAM: &str = "limit";
const OFFSET_PARAM: &str = "offset";
const URI_PARAM: &str = "uri";
const SEARCH_PARAMS: &[&str] = &[QUERY_PARAM, SERVER_PARAM, LIMIT_PARAM, OFFSET_PARAM];
const RESOURCES_PARAMS: &[&str] = &[SERVER_PARAM];
const READ_RESOURCE_PARAMS: &[&str] = &[SERVER_PARAM, URI_PARAM];
const RESOURCE_SCOPE_SEPARATOR: &str = ":";
const CONNECTED: &str = "connected";
const IMAGE_TYPE: &str = "image";
const AUDIO_TYPE: &str = "audio";
const RESOURCE_TYPE: &str = "resource";
const RESOURCE_LINK_TYPE: &str = "resource_link";
const EXCERPT_MARKER: &str = "\n[cut: the rest of this result was not kept]";
const BLOCKED_IN_PLAN: &str =
    "plan mode allows only the MCP tools a recognised server marks read-only";
const UNFINISHED: &str = "did not finish: the run stopped first";
/// What the type checker says after the stub class of a namespace whose tool it cannot find.
const NO_ATTRIBUTE: &str = " has no attribute `";
const QUOTE: char = '`';
/// The server column of a `decide()` or `rerank()` row.
pub const DECISIONS_ROW: &str = "decisions";
const NO_RUNNER: &str = "MCP code mode is unavailable: no code runner is installed in this process";
const GONE_NOTE: &str = "Variables from earlier `mcp` runs in this conversation are gone (Caudra restarted, or the conversation was resumed, rewound or forked); this run started empty. Fetch again what you need.";
const RESTORE_FAILED_NOTE: &str =
    "Variables from earlier runs could not be restored; this run started empty.";
const RUN_DISCARDED_NOTE: &str =
    "This run's variables could not be kept; the next run sees the variables from before it.";

/// Runs `code` for the agent `session` belongs to, holding its variables for the whole run. A
/// run started while servers are still making their first connection waits for them, so its
/// names cover every server that comes up.
pub async fn run(
    session: &McpSession,
    ctx: &ToolContext,
    code: String,
    reset: bool,
) -> McpCodeOutput {
    if session.connecting() && ctx.cancel.race(session.ready()).await.is_err() {
        return McpCodeOutput {
            code,
            outcome: McpCodeOutcome::Cancelled,
            ..McpCodeOutput::default()
        };
    }
    let mut lease = ReplLease::acquire(session.repl()).await;
    let forgotten = lease.take_forgotten() && !reset;
    if reset {
        lease.keep(None);
    }
    let Some(runner) = installed() else {
        return McpCodeOutput {
            code,
            outcome: McpCodeOutcome::Unavailable {
                message: NO_RUNNER.to_owned(),
            },
            ..McpCodeOutput::default()
        };
    };
    let gateway = Gateway::new(session.clone(), ctx.clone());
    let host: Arc<dyn ScriptHost> = Arc::new(gateway.clone());
    let outcome = runner
        .run(ScriptRun {
            code: code.clone(),
            host,
            state: lease,
            cancel: ctx.cancel.clone(),
        })
        .await;
    gateway.close_record().await;
    let notes: Vec<String> = [
        forgotten.then(|| GONE_NOTE.to_owned()),
        outcome.note.map(note_text),
    ]
    .into_iter()
    .flatten()
    .collect();
    let mut output = match outcome.output {
        Some(output) => finished(code, with_tool_hints(&gateway.0.names, output)),
        None => McpCodeOutput {
            code,
            outcome: McpCodeOutcome::Cancelled,
            ..McpCodeOutput::default()
        },
    };
    output.calls = gateway.finish();
    output.state_note = (!notes.is_empty()).then(|| notes.join(" "));
    for repair in outcome.repairs {
        output.lessons.extend(repair.lesson);
        output.repairs.push(repair.done);
    }
    output
}

fn note_text(note: ReplNote) -> String {
    match note {
        ReplNote::RestoreFailed => RESTORE_FAILED_NOTE.to_owned(),
        ReplNote::RunDiscarded => RUN_DISCARDED_NOTE.to_owned(),
        ReplNote::TooLarge { bytes } => format!(
            "Variables grew to {bytes} bytes, more than runs may keep, and were dropped; the next run starts empty. Keep large results out of variables."
        ),
    }
}

/// Type checking rejects a call to a tool a server lacks before the host can answer for it, so
/// the rejection gets the "did you mean" the call would have raised at runtime.
fn with_tool_hints(names: &Names, mut output: ScriptOutput) -> ScriptOutput {
    let Some(diagnostic) = output
        .diagnostic
        .as_deref()
        .filter(|_| output.status == ScriptStatus::Rejected)
    else {
        return output;
    };
    let mut hints: Vec<String> = diagnostic
        .split(CLASS_PREFIX)
        .skip(1)
        .filter_map(|rest| {
            let (namespace, rest) = rest.split_once(QUOTE)?;
            let (method, _) = rest.strip_prefix(NO_ATTRIBUTE)?.split_once(QUOTE)?;
            names.server(namespace)?;
            Some(no_tool_message(names, namespace, method))
        })
        .collect();
    hints.dedup();
    if !hints.is_empty() {
        hints.push(diagnostic.to_owned());
        output.diagnostic = Some(hints.join("\n"));
    }
    output
}

fn finished(code: String, output: ScriptOutput) -> McpCodeOutput {
    let ScriptOutput {
        status,
        result,
        result_repr,
        mut stdout,
        stderr,
        exception,
        diagnostic,
        ..
    } = output;
    if !stderr.is_empty() {
        if !stdout.is_empty() && !stdout.ends_with('\n') {
            stdout.push('\n');
        }
        stdout.push_str(&stderr);
    }
    let with_diagnostic = |message: String| match &diagnostic {
        Some(diagnostic) if !message.is_empty() => format!("{message}\n{diagnostic}"),
        Some(diagnostic) => diagnostic.clone(),
        None => message,
    };
    let outcome = match status {
        ScriptStatus::Completed => McpCodeOutcome::Completed,
        ScriptStatus::Exception => {
            let (kind, message) = exception
                .map(|exception| (exception.kind, exception.message))
                .unwrap_or_default();
            McpCodeOutcome::Raised {
                kind,
                message: with_diagnostic(message),
            }
        }
        ScriptStatus::Rejected => McpCodeOutcome::Rejected {
            message: with_diagnostic(exception.map(|e| e.message).unwrap_or_default()),
        },
        ScriptStatus::Limited => McpCodeOutcome::Limited {
            message: with_diagnostic(String::new()),
        },
        ScriptStatus::Unavailable => McpCodeOutcome::Unavailable {
            message: with_diagnostic(String::new()),
        },
    };
    McpCodeOutput {
        code,
        calls: Vec::new(),
        stdout,
        result: result_repr.or_else(|| rendered(&result)),
        outcome,
        ..McpCodeOutput::default()
    }
}

fn rendered(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        value => serde_json::to_string_pretty(value).ok(),
    }
}

/// One run's host: what its names resolve to, and the calls it made.
#[derive(Clone)]
struct Gateway(Arc<Inner>);

struct Inner {
    session: McpSession,
    ctx: ToolContext,
    servers: Vec<ServerView>,
    names: Names,
    calls: Mutex<Vec<McpCodeCall>>,
    /// The run's change record, `None` until a call that could change files is let through.
    record: AsyncMutex<Option<Recording>>,
    /// The session's decision engine, which `decide()` and `rerank()` need.
    decisions: Option<Decisions>,
}

impl Gateway {
    fn new(session: McpSession, ctx: ToolContext) -> Self {
        let servers = session.servers();
        let names = Names::new(&servers);
        let decisions = ctx.permissions.decisions();
        Self(Arc::new(Inner {
            session,
            ctx,
            servers,
            names,
            calls: Mutex::new(Vec::new()),
            record: AsyncMutex::new(None),
            decisions,
        }))
    }

    /// Opens the one record that covers the whole run, before the first permitted call that could
    /// change files, so a run that makes none leaves no record behind. An `Err` names why the call
    /// must not run.
    async fn open_record(&self, read_only: bool) -> Result<(), String> {
        if read_only {
            return Ok(());
        }
        let mut record = self.0.record.lock().await;
        if record.is_some() {
            return Ok(());
        }
        let ctx = &self.0.ctx;
        let call_id = ctx.tool_use_id.clone().unwrap_or_default();
        let mut opened = change_recording::begin(ctx, &call_id, |_| {
            change_recording::whole_workspace(ToolEffect::Unknown)
        })
        .await?;
        opened.start();
        *record = Some(opened);
        Ok(())
    }

    async fn close_record(&self) {
        let record = self.0.record.lock().await.take();
        if let Some(record) = record {
            record.finish(&self.0.ctx.event_tx).await;
        }
    }

    /// The calls the run made, any still running settled as unfinished.
    fn finish(&self) -> Vec<McpCodeCall> {
        let mut calls = std::mem::take(&mut *self.calls());
        for call in calls.iter_mut().filter(|call| !call.status.is_terminal()) {
            call.status = McpCodeCallStatus::Error;
            call.error = Some(UNFINISHED.to_owned());
            call.failure = Some(ToolFailure::Cancelled);
        }
        calls
    }

    fn calls(&self) -> MutexGuard<'_, Vec<McpCodeCall>> {
        self.0.calls.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn view(&self, server: &str) -> Option<&ServerView> {
        self.0.servers.iter().find(|view| view.name == server)
    }

    /// The configured server a snippet names by namespace or by configured name.
    fn server_named(&self, requested: &str) -> Result<String, ScriptError> {
        let names = &self.0.names;
        names
            .server(requested)
            .or_else(|| names.namespace(requested).and(Some(requested)))
            .map(str::to_owned)
            .ok_or_else(|| {
                let suggestion = names
                    .suggest_namespace(requested)
                    .map(|namespace| format!(" Did you mean `{namespace}`?"))
                    .unwrap_or_default();
                ScriptError::new(
                    ScriptErrorKind::Value,
                    format!("No MCP server `{requested}`.{suggestion}"),
                )
            })
    }

    fn begin(&self, call: McpCodeCall) -> usize {
        let index = {
            let mut calls = self.calls();
            calls.push(call.clone());
            calls.len() - 1
        };
        self.publish(index, call);
        index
    }

    /// Ends the call at `index` as `ending` says, timed from `started`, and tells telemetry.
    fn settle(&self, index: usize, started: Instant, ending: Ending) {
        let call = {
            let mut calls = self.calls();
            let Some(call) = calls.get_mut(index) else {
                return;
            };
            let Ending {
                status,
                failure,
                output,
                mut interactions,
                error,
            } = ending;
            call.status = status;
            call.failure = failure;
            call.error = error;
            call.output = output.map(|mut output| {
                interactions.append(&mut output.interactions);
                excerpt(&mut output);
                output
            });
            call.interactions = interactions;
            call.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            call.clone()
        };
        report(&call);
        self.publish(index, call);
    }

    fn publish(&self, index: usize, call: McpCodeCall) {
        let Some(id) = self.0.ctx.tool_use_id.clone() else {
            return;
        };
        let _ = self
            .0
            .ctx
            .event_tx
            .send(AgentEvent::McpCodeProgress(Box::new(
                McpCodeProgressEvent { id, index, call },
            )));
    }

    /// The context a call runs under: the run's, with the call's own id.
    fn child(&self, index: usize) -> ToolContext {
        let ctx = &self.0.ctx;
        let mut child = ctx.clone();
        child.tool_use_id = Some(child_tool_use_id(ctx.tool_use_id.as_deref(), index));
        child.local_root_tool_use_id = ctx
            .local_root_tool_use_id
            .clone()
            .or_else(|| ctx.tool_use_id.clone());
        child
    }

    fn helper(&self, call: ScriptCall) -> ScriptInvocation {
        match call.name.as_str() {
            SEARCH_HELPER => ScriptInvocation::Ready(self.search(&call)),
            SERVERS_HELPER => ScriptInvocation::Ready(Ok(self.server_list())),
            RESOURCES_HELPER => {
                let gateway = self.clone();
                ScriptInvocation::Deferred(Box::pin(async move { gateway.resources(&call) }))
            }
            READ_RESOURCE_HELPER => {
                let gateway = self.clone();
                ScriptInvocation::Deferred(Box::pin(
                    async move { gateway.read_resource(call).await },
                ))
            }
            DECIDE_HELPER | RERANK_HELPER if let Some(decisions) = self.0.decisions.clone() => {
                let gateway = self.clone();
                ScriptInvocation::Deferred(Box::pin(async move {
                    gateway.decision(decisions, call).await
                }))
            }
            name => ScriptInvocation::Ready(Err(ScriptError::new(
                ScriptErrorKind::Name,
                format!("name '{name}' is not defined"),
            ))),
        }
    }

    fn search(&self, call: &ScriptCall) -> ScriptReply {
        let arguments = bind(SEARCH_HELPER, SEARCH_PARAMS, call)?;
        let query = text(&arguments, SEARCH_HELPER, QUERY_PARAM)?.unwrap_or_default();
        let server = text(&arguments, SEARCH_HELPER, SERVER_PARAM)?;
        let limit = count(&arguments, SEARCH_HELPER, LIMIT_PARAM)?.unwrap_or(DEFAULT_LIMIT);
        let offset = count(&arguments, SEARCH_HELPER, OFFSET_PARAM)?.unwrap_or_default();
        let hits = search(
            &self.0.servers,
            &self.0.names,
            &query,
            server.as_deref(),
            limit,
            offset,
        )
        .map_err(|message| ScriptError::new(ScriptErrorKind::Value, message))?;
        Ok(hits
            .into_iter()
            .map(|hit| {
                json!({
                    "name": format!("{}.{}", hit.namespace, hit.method),
                    "signature": format!("{}.{}", hit.namespace, hit.signature),
                    "doc": hit.doc,
                })
            })
            .collect())
    }

    fn server_list(&self) -> Value {
        self.0
            .names
            .namespaces()
            .filter_map(|(namespace, server)| {
                let view = self.view(server)?;
                Some(json!({
                    "name": namespace,
                    "server": server,
                    "title": view.title,
                    "status": unavailable(&view.state).unwrap_or_else(|| CONNECTED.to_owned()),
                    "tools": view.tools.len(),
                    "resources": view.resources,
                }))
            })
            .collect()
    }

    fn resources(&self, call: &ScriptCall) -> ScriptReply {
        let arguments = bind(RESOURCES_HELPER, RESOURCES_PARAMS, call)?;
        let requested = required_text(&arguments, RESOURCES_HELPER, SERVER_PARAM)?;
        let server = self.server_named(&requested)?;
        let snapshot = self.0.session.reader().load_full();
        Ok(snapshot
            .resources_of(&server)
            .map(|offer| resource_entries(&offer.lists))
            .unwrap_or_default())
    }

    async fn read_resource(self, call: ScriptCall) -> ScriptReply {
        let arguments = bind(READ_RESOURCE_HELPER, READ_RESOURCE_PARAMS, &call)?;
        let requested = required_text(&arguments, READ_RESOURCE_HELPER, SERVER_PARAM)?;
        let uri = required_text(&arguments, READ_RESOURCE_HELPER, URI_PARAM)?;
        let server = self.server_named(&requested)?;
        let input = json!({ URI_PARAM: uri });
        let index = self.begin(McpCodeCall {
            server: server.clone(),
            tool: READ_RESOURCE_HELPER.to_owned(),
            called_as: format!(
                "{READ_RESOURCE_HELPER}({}, {})",
                Value::from(requested),
                Value::from(uri.as_str())
            ),
            input: input.clone(),
            ..McpCodeCall::default()
        });
        let child = self.child(index);
        let started = Instant::now();
        if let Err(error) = authorize_read(&child, &server, &uri, &input).await {
            return self.refused(index, started, error);
        }
        let session = &self.0.session;
        let scope = CallScope::new(
            session.elicitor_for(&child),
            self.0.ctx.live_sink.clone(),
            self.0.ctx.cancel.clone(),
        );
        match session.read_resource(&server, &uri, &scope).await {
            Ok(contents) => {
                let value = contents_value(&contents);
                let output = McpOutput {
                    blocks: contents.into_iter().map(read_block).collect(),
                    interactions: scope.take_interactions(),
                    ..McpOutput::default()
                };
                self.succeeded(index, started, output, value)
            }
            Err(error) => self.failed(
                index,
                started,
                Ending::failed(error.failure(), error.to_string())
                    .with_interactions(scope.take_interactions()),
            ),
        }
    }

    /// A `decide()` or `rerank()` call, shown as a row like any server call. It asks no permission:
    /// the engine is the user's own, and an answer is data that grants nothing.
    async fn decision(self, decisions: Decisions, call: ScriptCall) -> ScriptReply {
        let rerank = call.name == RERANK_HELPER;
        let (helper, params, marked) = match rerank {
            true => (RERANK_HELPER, RERANK_PARAMS, McpHelper::Rerank),
            false => (DECIDE_HELPER, DECIDE_PARAMS, McpHelper::Decide),
        };
        let arguments = bind(helper, params, &call)?;
        let pending = match rerank {
            true => Pending::Rerank(rerank_request(&arguments)?),
            false => Pending::Decide(decide_request(&arguments)?),
        };
        if let Pending::Rerank(request) = &pending
            && request.candidates.is_empty()
        {
            return Ok(Value::Array(Vec::new()));
        }
        let called_as = match &pending {
            Pending::Decide(decide) => decide_label(&decide.request.questions),
            Pending::Rerank(request) => rerank_label(request.candidates.len(), 1, 1),
        };
        let index = self.begin(McpCodeCall {
            server: DECISIONS_ROW.to_owned(),
            tool: helper.to_owned(),
            called_as,
            input: Value::Object(arguments),
            helper: Some(marked),
            ..McpCodeCall::default()
        });
        let started = Instant::now();
        let ctx = &self.0.ctx;
        let context = ctx.permissions.decision_context(json!({
            "tool_use_id": child_tool_use_id(ctx.tool_use_id.as_deref(), index),
        }));
        let answered = match &pending {
            Pending::Decide(decide) => {
                decide::ask(&decisions, &context, &decide.request, &ctx.cancel)
                    .await
                    .and_then(|result| {
                        decide
                            .questions
                            .answers(&result)
                            .map_err(|message| ScriptError::new(ScriptErrorKind::Runtime, message))
                    })
            }
            Pending::Rerank(request) => {
                decide::rerank(&decisions, &context, request, &ctx.cancel).await
            }
        };
        match answered {
            Ok(value) => {
                let output = McpOutput {
                    blocks: vec![McpBlock::Text {
                        text: serde_json::to_string_pretty(&value).unwrap_or_default(),
                    }],
                    ..McpOutput::default()
                };
                self.succeeded(index, started, output, value)
            }
            Err(error) => {
                let failure = match ctx.cancel.is_cancelled() {
                    true => ToolFailure::Cancelled,
                    false => ToolFailure::Other,
                };
                self.failed(index, started, Ending::failed(failure, error.message))
            }
        }
    }

    async fn call_tool(self, namespace: String, method: String, call: ScriptCall) -> ScriptReply {
        let names = &self.0.names;
        let server = names.server(&namespace).unwrap_or_default().to_owned();
        let Some(view) = self.view(&server) else {
            return Err(undefined_namespace(&namespace));
        };
        if let Some(reason) = unavailable(&view.state) {
            return Err(ScriptError::new(
                ScriptErrorKind::Runtime,
                format!("MCP server `{namespace}` is not connected ({reason})"),
            ));
        }
        let Some(tool) = names
            .raw_tool(&namespace, &method)
            .and_then(|raw| view.tools.iter().find(|tool| tool.name == raw))
        else {
            return Err(ScriptError::new(
                ScriptErrorKind::Attribute,
                no_tool_message(names, &namespace, &method),
            ));
        };
        let input = Value::Object(tool_arguments(&method, call, &tool.input_schema)?);
        let qualified = format!("{server}{SEPARATOR}{}", tool.name);
        let index = self.begin(McpCodeCall {
            server: server.clone(),
            tool: tool.name.clone(),
            called_as: format!("{namespace}{SEPARATOR}{method}"),
            title: tool.title.clone(),
            input: input.clone(),
            ..McpCodeCall::default()
        });
        let child = self.child(index);
        let started = Instant::now();
        let session = &self.0.session;
        let binding = match authorize(&child, session, &qualified, &input).await {
            Ok(binding) => binding,
            Err(error) => return self.refused(index, started, error),
        };
        if let Err(message) = self.open_record(binding.read_only()).await {
            return self.failed(index, started, Ending::failed(ToolFailure::Other, message));
        }
        let scope = CallScope::new(
            session.elicitor_for(&child),
            self.0.ctx.live_sink.clone(),
            self.0.ctx.cancel.clone(),
        );
        match binding.call(&input, &scope).await {
            Ok(output) if output.is_error => {
                let error = binding.reported_error(&output);
                self.failed(
                    index,
                    started,
                    Ending::failed(error.failure(), error.to_string()).with_output(output),
                )
            }
            Ok(output) => {
                let value = output_value(&output);
                self.succeeded(index, started, output, value)
            }
            Err(error) => self.failed(
                index,
                started,
                Ending::failed(error.failure(), error.to_string())
                    .with_interactions(scope.take_interactions()),
            ),
        }
    }

    fn succeeded(
        &self,
        index: usize,
        started: Instant,
        output: McpOutput,
        value: Value,
    ) -> ScriptReply {
        let size = serde_json::to_vec(&value).map_or(0, |bytes| bytes.len());
        if size > MAX_RESULT_BYTES {
            let name = self
                .calls()
                .get(index)
                .map(|call| call.called_as.clone())
                .unwrap_or_default();
            let message = format!(
                "the result of `{name}` is {size} bytes, more than the {MAX_RESULT_BYTES} a snippet can receive; ask the tool for less"
            );
            return self.failed(
                index,
                started,
                Ending::failed(ToolFailure::Other, message).with_output(output),
            );
        }
        self.settle(
            index,
            started,
            Ending {
                status: McpCodeCallStatus::Success,
                output: Some(output),
                ..Ending::default()
            },
        );
        Ok(value)
    }

    fn failed(&self, index: usize, started: Instant, ending: Ending) -> ScriptReply {
        let message = ending.error.clone().unwrap_or_default();
        self.settle(index, started, ending);
        Err(ScriptError::new(ScriptErrorKind::Runtime, message))
    }

    fn refused(&self, index: usize, started: Instant, error: ScriptError) -> ScriptReply {
        let failure = match self.0.ctx.cancel.is_cancelled() {
            true => ToolFailure::Cancelled,
            false => ToolFailure::Denied,
        };
        self.settle(
            index,
            started,
            Ending {
                status: McpCodeCallStatus::Refused,
                ..Ending::failed(failure, error.message.clone())
            },
        );
        Err(error)
    }
}

/// A decision call whose arguments were read.
enum Pending {
    Decide(Decide),
    Rerank(RerankRequest),
}

/// How a call ended, as its row keeps it.
#[derive(Default)]
struct Ending {
    status: McpCodeCallStatus,
    failure: Option<ToolFailure>,
    output: Option<McpOutput>,
    interactions: Vec<McpInteraction>,
    error: Option<String>,
}

impl Ending {
    fn failed(failure: ToolFailure, message: String) -> Self {
        Self {
            status: McpCodeCallStatus::Error,
            failure: Some(failure),
            error: Some(message),
            ..Self::default()
        }
    }

    fn with_output(self, output: McpOutput) -> Self {
        Self {
            output: Some(output),
            ..self
        }
    }

    fn with_interactions(self, interactions: Vec<McpInteraction>) -> Self {
        Self {
            interactions,
            ..self
        }
    }
}

impl ScriptHost for Gateway {
    fn lookup(&self, namespace: Option<&str>, name: &str) -> ScriptName {
        let names = &self.0.names;
        let Some(namespace) = namespace else {
            if names.server(name).is_some() {
                return ScriptName::Namespace;
            }
            if HELPERS.contains(&name)
                && (self.0.decisions.is_some() || !DECISION_HELPERS.contains(&name))
            {
                return ScriptName::Function;
            }
            return ScriptName::Undefined(names.suggest_namespace(name).map(|namespace| {
                format!("name '{name}' is not defined. Did you mean the MCP server `{namespace}`?")
            }));
        };
        let reachable = names
            .server(namespace)
            .and_then(|server| self.view(server))
            .is_some_and(|view| unavailable(&view.state).is_none());
        // A server that is not connected answers every name, so the call raises why.
        if !reachable || names.raw_tool(namespace, name).is_some() {
            return ScriptName::Function;
        }
        ScriptName::Undefined(Some(no_tool_message(names, namespace, name)))
    }

    fn call(&self, call: ScriptCall) -> ScriptInvocation {
        let Some(namespace) = call.namespace.clone() else {
            return self.helper(call);
        };
        let gateway = self.clone();
        let method = call.name.clone();
        ScriptInvocation::Deferred(Box::pin(gateway.call_tool(namespace, method, call)))
    }

    fn stubs(&self) -> String {
        stubs(&self.0.names, &self.0.servers, self.0.decisions.is_some())
    }
}

/// Lets `ctx`'s agent call `qualified` with `input`, or says why not: the checks a direct MCP
/// call made, then the permission prompt.
async fn authorize(
    ctx: &ToolContext,
    session: &McpSession,
    qualified: &str,
    input: &Value,
) -> Result<McpToolBinding, ScriptError> {
    let refuse = |message: String| ScriptError::new(ScriptErrorKind::Permission, message);
    let binding = session
        .bind_tool(qualified)
        .map_err(|error| ScriptError::new(ScriptErrorKind::Runtime, error.to_string()))?;
    if ctx.mode.is_planning() && !binding.read_only() {
        return Err(refuse(BLOCKED_IN_PLAN.to_owned()));
    }
    if ctx.policy().is_read_only() && !binding.read_only() {
        return Err(refuse(format!("{READ_ONLY_TOOL_RESTRICTED}: {qualified}")));
    }
    if session.is_disabled(qualified) {
        return Err(refuse(format!("`{qualified}` is disabled for this agent")));
    }
    let key = ToolKey::parse(qualified)
        .map_err(|error| ScriptError::new(ScriptErrorKind::Runtime, error.to_string()))?;
    let intent = mcp_permission_intent(&key, input, binding.call_facts(input));
    ctx.permissions
        .enforce_with_intent(
            &key,
            &intent,
            input,
            &ctx.event_tx,
            ctx.user_response_rx.as_deref(),
            ctx.tool_use_id.as_deref().unwrap_or_default(),
            &ctx.cancel,
            ctx.mode.plan_path(),
            Some((binding.subject().clone(), PermissionExecutorKind::Mcp)),
            false,
        )
        .await
        .map_err(|error| refuse(error.to_string()))?;
    Ok(binding)
}

/// Reading a server's resources asks as the `mcp` tool, scoped to the server and URI, and stays
/// in reach while planning and under a read-only policy. The built-in allow that lets the `mcp`
/// tool itself run unasked never covers a read.
async fn authorize_read(
    ctx: &ToolContext,
    server: &str,
    uri: &str,
    input: &Value,
) -> Result<(), ScriptError> {
    let key = ToolKey::native(MCP_TOOL_NAME);
    let identity = ctx
        .registry
        .get(MCP_TOOL_NAME)
        .and_then(|entry| TrustedToolSource::from_registered(&entry, None))
        .map(|source| (source.subject().clone(), source.executor().clone()));
    let intent = PermissionIntent::new(
        PermissionScopes::single(format!("{server}{RESOURCE_SCOPE_SEPARATOR}{uri}")),
        vec![PermissionResource {
            kind: PermissionResourceKind::Custom {
                name: MCP_SERVER_RESOURCE.to_owned(),
            },
            value: server.to_owned(),
            access: Some(PermissionResourceAccess::Read),
            protected: false,
            requires_prompt: false,
            attributes: BTreeMap::new(),
        }],
        PermissionRisk::Low,
    );
    ctx.permissions
        .enforce_with_intent(
            &key,
            &intent,
            input,
            &ctx.event_tx,
            ctx.user_response_rx.as_deref(),
            ctx.tool_use_id.as_deref().unwrap_or_default(),
            &ctx.cancel,
            ctx.mode.plan_path(),
            identity,
            false,
        )
        .await
        .map_err(|error| ScriptError::new(ScriptErrorKind::Permission, error.to_string()))
}

fn report(call: &McpCodeCall) {
    let tool_input = caudra_otel::logs_tool_details().then(|| call.input.to_string());
    let outcome = call.ledger_outcome();
    caudra_otel::emit::tool_result(&caudra_otel::emit::ToolResult {
        tool_name: &call.qualified_name(),
        tool_source: CALL_SOURCE,
        success: outcome == LedgerOutcome::Ok,
        duration: Duration::from_millis(call.duration_ms),
        error_type: error_type(outcome),
        tool_input: tool_input.as_deref(),
    });
}

fn undefined_namespace(namespace: &str) -> ScriptError {
    ScriptError::new(
        ScriptErrorKind::Name,
        format!("name '{namespace}' is not defined"),
    )
}

fn no_tool_message(names: &Names, namespace: &str, method: &str) -> String {
    let suggestion = names
        .suggest_tool(namespace, method)
        .map(|tool| format!(" Did you mean `{tool}`?"))
        .unwrap_or_default();
    format!(
        "MCP server `{namespace}` has no tool `{method}`.{suggestion} Find tools with `search(\"...\", server=\"{namespace}\")`."
    )
}

/// A helper's arguments by parameter name, positional ones first.
fn bind(
    helper: &str,
    params: &[&str],
    call: &ScriptCall,
) -> Result<Map<String, Value>, ScriptError> {
    let type_error = |message: String| ScriptError::new(ScriptErrorKind::Type, message);
    if call.args.len() > params.len() {
        return Err(type_error(format!(
            "{helper}() takes at most {} positional arguments ({} given)",
            params.len(),
            call.args.len()
        )));
    }
    let mut bound: Map<String, Value> = params
        .iter()
        .zip(&call.args)
        .map(|(param, value)| ((*param).to_owned(), value.clone()))
        .collect();
    for (name, value) in &call.kwargs {
        if !params.contains(&name.as_str()) {
            return Err(type_error(format!(
                "{helper}() got an unexpected keyword argument '{name}'"
            )));
        }
        if bound.insert(name.clone(), value.clone()).is_some() {
            return Err(type_error(format!(
                "{helper}() got multiple values for argument '{name}'"
            )));
        }
    }
    Ok(bound)
}

fn text(
    arguments: &Map<String, Value>,
    helper: &str,
    param: &str,
) -> Result<Option<String>, ScriptError> {
    match arguments.get(param) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(ScriptError::new(
            ScriptErrorKind::Type,
            format!("{helper}() argument '{param}' must be a str"),
        )),
    }
}

fn required_text(
    arguments: &Map<String, Value>,
    helper: &str,
    param: &str,
) -> Result<String, ScriptError> {
    text(arguments, helper, param)?.ok_or_else(|| {
        ScriptError::new(
            ScriptErrorKind::Type,
            format!("{helper}() missing required argument: '{param}'"),
        )
    })
}

fn count(
    arguments: &Map<String, Value>,
    helper: &str,
    param: &str,
) -> Result<Option<usize>, ScriptError> {
    match arguments.get(param) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .and_then(|count| usize::try_from(count).ok())
            .map(Some)
            .ok_or_else(|| {
                ScriptError::new(
                    ScriptErrorKind::Value,
                    format!("{helper}() argument '{param}' must be a non-negative int"),
                )
            }),
    }
}

/// A tool call's arguments: keywords, or one positional dict, checked against what the tool's
/// schema requires and allows.
fn tool_arguments(
    method: &str,
    call: ScriptCall,
    schema: &Value,
) -> Result<Map<String, Value>, ScriptError> {
    let type_error = |message: String| ScriptError::new(ScriptErrorKind::Type, message);
    let ScriptCall { args, kwargs, .. } = call;
    let arguments = match <[Value; 1]>::try_from(args) {
        Ok([Value::Object(arguments)]) if kwargs.is_empty() => arguments,
        Err(args) if args.is_empty() => kwargs,
        _ => {
            return Err(type_error(format!(
                "{method}() takes keyword arguments, or a single dict of them"
            )));
        }
    };
    let properties = schema.get(PROPERTIES).and_then(Value::as_object);
    let missing: Vec<&str> = schema
        .get(REQUIRED)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|name| !arguments.contains_key(*name))
        .collect();
    if !missing.is_empty() {
        return Err(type_error(format!(
            "{method}() missing required arguments: {}",
            quoted(missing.into_iter())
        )));
    }
    let open = schema
        .get(ADDITIONAL_PROPERTIES)
        .is_some_and(|allowed| allowed.as_bool() != Some(false));
    if let Some(properties) = properties.filter(|_| !open) {
        let unknown: Vec<&str> = arguments
            .keys()
            .map(String::as_str)
            .filter(|name| !properties.contains_key(*name))
            .collect();
        if !unknown.is_empty() {
            let known = properties.keys().map(String::as_str);
            return Err(type_error(format!(
                "{method}() got unexpected arguments: {}. It takes: {}",
                quoted(unknown.into_iter()),
                quoted(known)
            )));
        }
    }
    Ok(arguments)
}

fn quoted<'a>(names: impl Iterator<Item = &'a str>) -> String {
    names
        .map(|name| format!("'{name}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// What a snippet receives: `structuredContent`, else the content, a JSON text parsed.
fn output_value(output: &McpOutput) -> Value {
    if let Some(structured) = output.structured_content() {
        return structured.clone();
    }
    let mut values: Vec<Value> = output.blocks.iter().map(block_value).collect();
    match values.len() {
        0 => Value::Null,
        1 => values.remove(0),
        _ => Value::Array(values),
    }
}

fn block_value(block: &McpBlock) -> Value {
    match block {
        McpBlock::Text { text } | McpBlock::Excerpt { text } => json_text(text),
        McpBlock::Structured { value } => value.clone(),
        McpBlock::Image { mime_type, bytes } => media(IMAGE_TYPE, mime_type, *bytes),
        McpBlock::Audio { mime_type, bytes } => media(AUDIO_TYPE, mime_type, *bytes),
        McpBlock::Resource {
            uri,
            mime_type,
            text,
            bytes,
        } => json!({
            "type": RESOURCE_TYPE,
            "uri": uri,
            "mime_type": mime_type,
            "text": text,
            "bytes": bytes,
        }),
        McpBlock::ResourceLink {
            uri,
            name,
            mime_type,
        } => json!({
            "type": RESOURCE_LINK_TYPE,
            "uri": uri,
            "name": name,
            "mime_type": mime_type,
        }),
    }
}

/// Text that is a JSON object or array, parsed; anything else stays text.
fn json_text(text: &str) -> Value {
    let trimmed = text.trim();
    trimmed
        .starts_with(['{', '['])
        .then(|| serde_json::from_str(trimmed).ok())
        .flatten()
        .unwrap_or_else(|| Value::String(text.to_owned()))
}

/// Media the model never reads, named by kind and size.
fn media(kind: &str, mime_type: &Option<String>, bytes: usize) -> Value {
    json!({ "type": kind, "mime_type": mime_type, "bytes": bytes })
}

fn resource_entries(lists: &ResourceLists) -> Value {
    let resources = lists.resources.iter().map(|resource| {
        json!({
            "uri": resource.uri,
            "name": resource.label(),
            "description": resource.description,
            "mime_type": resource.mime_type,
            "template": false,
        })
    });
    let templates = lists.templates.iter().map(|template| {
        json!({
            "uri": template.uri_template,
            "name": template.label(),
            "description": template.description,
            "mime_type": template.mime_type,
            "template": true,
        })
    });
    resources.chain(templates).collect()
}

fn contents_value(contents: &[ResourceContents]) -> Value {
    contents
        .iter()
        .map(|content| match &content.blob {
            Some(blob) => json!({
                "uri": content.uri,
                "mime_type": content.mime_type,
                "bytes": decoded_len(blob),
            }),
            None => json!({
                "uri": content.uri,
                "mime_type": content.mime_type,
                "text": content.text.clone().unwrap_or_default(),
            }),
        })
        .collect()
}

fn read_block(content: ResourceContents) -> McpBlock {
    let bytes = content.blob.as_deref().map(decoded_len);
    match bytes {
        Some(bytes) if content.is_image() => McpBlock::Image {
            mime_type: content.mime_type,
            bytes,
        },
        _ => McpBlock::Resource {
            uri: content.uri,
            mime_type: content.mime_type,
            text: content.text,
            bytes,
        },
    }
}

/// Keeps what a card draws of `output` within [`MAX_CALL_DISPLAY_BYTES`].
fn excerpt(output: &mut McpOutput) {
    let text = output.display_text();
    if text.len() <= MAX_CALL_DISPLAY_BYTES {
        return;
    }
    let mut end = MAX_CALL_DISPLAY_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    output.blocks = vec![McpBlock::Excerpt {
        text: format!("{}{EXCERPT_MARKER}", &text[..end]),
    }];
    output.structured = None;
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use async_trait::async_trait;
    use caudra_config::decisions::DecisionsConfig;
    use caudra_config::{Effect, PermissionRule, PermissionsConfig};
    use caudra_decision::{
        Answer, ChoiceAnswer, DecisionEngine, DecisionError, DecisionRequest as EngineRequest,
        DecisionResponse, NoulAnswer, QuestionType, ScoreAnswer, Usage,
    };
    use caudra_providers::{ContentBlock, Message, Role};
    use caudra_storage::StateDir;
    use caudra_workspace::RecordScope;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::AgentMode;
    use crate::agent::change_recording::fixture::{FakeChanges, Step};
    use crate::mcp::code::runner::{ReplState, RunFuture, ScriptOutcome, ScriptRunner, install};
    use crate::mcp::{recognised_stub_session, stub_session_replying};
    use crate::permissions::{PERMISSION_DENIED_PREFIX, PermissionManager, PermissionMode};
    use crate::tools::test_support::{stub_ctx, stub_ctx_with_permissions};

    const NAMESPACE: &str = "stub";
    const LOOKUP: &str = "lookup";
    const LOOKUP_TOOL: &str = "stub.lookup";
    const LOOKUP_RESULT: &str = "the kitchen light is on";
    const HOME_ASSISTANT: &str = "home-assistant";
    const LOOKALIKE: &str = "home-assistant-ish";
    const PLAN_FILE: &str = "/tmp/caudra-plan.md";
    const ERROR_TEXT: &str = "issue tracker is down";
    const RECORD_ROOT: &str = "/workspace";
    const EXECUTOR: &str = "executor";
    const RESUME: &str = "resume";

    #[derive(Debug, PartialEq)]
    enum Verdict {
        Ran,
        Asked,
        Refused,
    }

    struct KeepsVariables;

    impl ScriptRunner for KeepsVariables {
        fn run(&self, mut run: ScriptRun) -> RunFuture {
            run.state.keep(Some(ReplState::new(0, ())));
            Box::pin(async {
                ScriptOutcome {
                    output: Some(ScriptOutput {
                        diagnostic: None,
                        ..ScriptOutput::refused(ScriptStatus::Completed, String::new())
                    }),
                    ..ScriptOutcome::default()
                }
            })
        }
    }

    fn earlier_mcp_run() -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: String::new(),
                name: MCP_TOOL_NAME.to_owned(),
                input: Value::Null,
                thought_signature: None,
            }],
            ..Message::default()
        }
    }

    fn lookup_result() -> Value {
        json!({"content": [{"type": "text", "text": LOOKUP_RESULT}]})
    }

    fn lookup(session: McpSession, ctx: ToolContext) -> (ScriptReply, Vec<McpCodeCall>) {
        let gateway = Gateway::new(session, ctx);
        let reply = smol::block_on(call_lookup(&gateway));
        (reply, gateway.finish())
    }

    async fn call_lookup(gateway: &Gateway) -> ScriptReply {
        call_method(gateway, LOOKUP, Map::new()).await
    }

    async fn call_method(
        gateway: &Gateway,
        method: &str,
        kwargs: Map<String, Value>,
    ) -> ScriptReply {
        let call = ScriptCall {
            namespace: Some(NAMESPACE.to_owned()),
            name: method.to_owned(),
            args: Vec::new(),
            kwargs,
        };
        gateway
            .clone()
            .call_tool(NAMESPACE.into(), method.into(), call)
            .await
    }

    fn verdict(reply: &ScriptReply) -> Verdict {
        match reply {
            Ok(value) if value == LOOKUP_RESULT => Verdict::Ran,
            Err(error) if error.kind == ScriptErrorKind::Permission => {
                match error.message.starts_with(PERMISSION_DENIED_PREFIX) {
                    true => Verdict::Asked,
                    false => Verdict::Refused,
                }
            }
            reply => panic!("{reply:?}"),
        }
    }

    fn permissions(
        config: PermissionsConfig,
        mode: PermissionMode,
    ) -> (TempDir, Arc<PermissionManager>) {
        let dir = TempDir::new().unwrap();
        let permissions = Arc::new(PermissionManager::new_nonpersistent(
            config,
            dir.path().to_path_buf(),
            Arc::default(),
        ));
        permissions.set_session_mode(Some(mode));
        (dir, permissions)
    }

    /// A tool a recognised server annotates read-only stays callable in plan mode and under a
    /// read-only policy, and still answers to the permission mode there. Any other tool raises
    /// `PermissionError` there, even in Yolo.
    #[test_case(AgentMode::Plan(PathBuf::from(PLAN_FILE)), PermissionMode::Yolo, HOME_ASSISTANT, true, Verdict::Ran ; "plan_runs_a_read_only_tool")]
    #[test_case(AgentMode::ReadOnly, PermissionMode::Yolo, HOME_ASSISTANT, true, Verdict::Ran ; "read_only_policy_runs_a_read_only_tool")]
    #[test_case(AgentMode::Build, PermissionMode::Yolo, HOME_ASSISTANT, false, Verdict::Ran ; "build_runs_a_tool_that_writes")]
    #[test_case(AgentMode::Plan(PathBuf::from(PLAN_FILE)), PermissionMode::Ask, HOME_ASSISTANT, true, Verdict::Asked ; "ask_still_asks_in_plan_mode")]
    #[test_case(AgentMode::ReadOnly, PermissionMode::Ask, HOME_ASSISTANT, true, Verdict::Asked ; "ask_still_asks_under_a_read_only_policy")]
    #[test_case(AgentMode::Plan(PathBuf::from(PLAN_FILE)), PermissionMode::Yolo, LOOKALIKE, true, Verdict::Refused ; "plan_refuses_a_stranger_claiming_read_only")]
    #[test_case(AgentMode::ReadOnly, PermissionMode::Yolo, LOOKALIKE, true, Verdict::Refused ; "read_only_policy_refuses_a_stranger_claiming_read_only")]
    #[test_case(AgentMode::Plan(PathBuf::from(PLAN_FILE)), PermissionMode::Yolo, HOME_ASSISTANT, false, Verdict::Refused ; "plan_refuses_a_recognised_tool_that_writes")]
    #[test_case(AgentMode::ReadOnly, PermissionMode::Yolo, HOME_ASSISTANT, false, Verdict::Refused ; "read_only_policy_refuses_a_recognised_tool_that_writes")]
    fn calls_answer_to_the_mode_and_the_permission_mode(
        mode: AgentMode,
        permission_mode: PermissionMode,
        server: &str,
        annotated: bool,
        expected: Verdict,
    ) {
        let session = recognised_stub_session(server, &[(LOOKUP_TOOL, annotated)], lookup_result());
        let (_dir, permissions) = permissions(PermissionsConfig::default(), permission_mode);
        let (reply, calls) = lookup(session, stub_ctx_with_permissions(&mode, permissions));
        assert_eq!(verdict(&reply), expected, "{reply:?}");
        let status = match expected {
            Verdict::Ran => McpCodeCallStatus::Success,
            Verdict::Asked | Verdict::Refused => McpCodeCallStatus::Refused,
        };
        assert_eq!(
            calls.iter().map(|call| call.status).collect::<Vec<_>>(),
            [status]
        );
    }

    #[test]
    fn a_denied_call_raises_permission_error_and_is_recorded_as_refused() {
        let config = PermissionsConfig {
            rules: vec![PermissionRule {
                tool: ToolKey::parse(LOOKUP_TOOL).unwrap(),
                scope: None,
                effect: Effect::Deny,
            }],
            ..Default::default()
        };
        let (_dir, permissions) = permissions(config, PermissionMode::Yolo);
        let session = stub_session_replying(&[(LOOKUP_TOOL, "")], None);
        let (reply, calls) = lookup(
            session,
            stub_ctx_with_permissions(&AgentMode::Build, permissions),
        );
        let error = reply.unwrap_err();
        assert_eq!(error.kind, ScriptErrorKind::Permission);
        assert!(
            error.message.starts_with(PERMISSION_DENIED_PREFIX),
            "{}",
            error.message
        );
        assert_eq!(calls[0].status, McpCodeCallStatus::Refused);
        assert_eq!(calls[0].error.as_deref(), Some(error.message.as_str()));
    }

    /// The model reads the server's own text, and the card keeps the output it came with.
    #[test]
    fn a_result_flagged_is_error_raises_runtime_error_and_keeps_its_output() {
        let reply = json!({
            "content": [{"type": "text", "text": ERROR_TEXT}],
            "isError": true,
        });
        let session = stub_session_replying(&[(LOOKUP_TOOL, "")], Some(reply));
        let (_dir, permissions) = permissions(PermissionsConfig::default(), PermissionMode::Yolo);
        let (reply, calls) = lookup(
            session,
            stub_ctx_with_permissions(&AgentMode::Build, permissions),
        );
        let error = reply.unwrap_err();
        assert_eq!(error.kind, ScriptErrorKind::Runtime);
        assert!(error.message.contains(ERROR_TEXT), "{}", error.message);
        assert_eq!(calls[0].status, McpCodeCallStatus::Error);
        assert!(calls[0].output.is_some());
    }

    /// Variables live only in memory, so a conversation that ran `mcp` under another agent, in an
    /// earlier process or before a rewind or fork, says once that they are gone, unless the run
    /// asked to start empty anyway.
    #[test_case(true, false, Some(GONE_NOTE) ; "a_resumed_conversation_says_its_variables_are_gone")]
    #[test_case(true, true, None ; "a_reset_run_has_nothing_to_miss")]
    #[test_case(false, false, None ; "a_new_conversation_lost_nothing")]
    fn only_the_first_run_of_a_new_agent_notes_the_lost_variables(
        ran_before: bool,
        reset: bool,
        expected: Option<&str>,
    ) {
        install(Arc::new(KeepsVariables));
        let history = match ran_before {
            true => vec![earlier_mcp_run()],
            false => Vec::new(),
        };
        let session = stub_session_replying(&[], None).fresh(&history);
        let ctx = stub_ctx(&AgentMode::Build);
        let first = smol::block_on(run(&session, &ctx, String::new(), reset));
        let second = smol::block_on(run(&session, &ctx, String::new(), false));
        assert_eq!(first.state_note.as_deref(), expected);
        assert_eq!(second.state_note, None);
    }

    /// One record covers the run, from its first permitted call that could change files to its
    /// end. A refused call, or one a recognised server marks read-only, opens none.
    #[test_case(false, Effect::Allow, &[Step::Begin(RecordScope::Workspace), Step::Finish] ; "permitted_calls_share_one_record")]
    #[test_case(true, Effect::Allow, &[] ; "read_only_calls_open_none")]
    #[test_case(false, Effect::Deny, &[] ; "refused_calls_open_none")]
    fn a_run_is_recorded_from_its_first_call_that_could_change_files(
        annotated: bool,
        effect: Effect,
        expected: &[Step],
    ) {
        let config = PermissionsConfig {
            rules: vec![PermissionRule {
                tool: ToolKey::parse(LOOKUP_TOOL).unwrap(),
                scope: None,
                effect,
            }],
            ..Default::default()
        };
        let (_dir, permissions) = permissions(config, PermissionMode::Yolo);
        let changes = Arc::new(FakeChanges::default());
        let mut ctx = stub_ctx_with_permissions(&AgentMode::Build, permissions);
        ctx.changes = Some(changes.recorder(Path::new(RECORD_ROOT)));
        let session =
            recognised_stub_session(HOME_ASSISTANT, &[(LOOKUP_TOOL, annotated)], lookup_result());
        let gateway = Gateway::new(session, ctx);
        smol::block_on(async {
            for _ in 0..2 {
                assert_eq!(call_lookup(&gateway).await.is_ok(), effect == Effect::Allow);
            }
            gateway.close_record().await;
        });
        assert_eq!(changes.steps(), expected);
    }

    /// A call its recognised server guards asks even where a rule allows the tool, exactly as a
    /// direct call did: a Home Assistant call naming a sensitive domain, and an Executor accept.
    #[test_case(HOME_ASSISTANT, LOOKUP, json!({"domain": "lock"}), Verdict::Asked ; "a_sensitive_home_assistant_domain")]
    #[test_case(HOME_ASSISTANT, LOOKUP, json!({"domain": "light"}), Verdict::Ran ; "a_harmless_home_assistant_domain")]
    #[test_case(EXECUTOR, RESUME, json!({"executionId": "exec_1", "action": "accept"}), Verdict::Asked ; "an_executor_accept")]
    #[test_case(EXECUTOR, RESUME, json!({"executionId": "exec_1", "action": "decline"}), Verdict::Ran ; "an_executor_decline")]
    fn a_guarded_call_asks_despite_an_allow_rule(
        server: &str,
        method: &str,
        input: Value,
        expected: Verdict,
    ) {
        let qualified = format!("{NAMESPACE}{SEPARATOR}{method}");
        let config = PermissionsConfig {
            rules: vec![PermissionRule {
                tool: ToolKey::parse(&qualified).unwrap(),
                scope: None,
                effect: Effect::Allow,
            }],
            ..Default::default()
        };
        let (_dir, permissions) = permissions(config, PermissionMode::Ask);
        let session =
            recognised_stub_session(server, &[(qualified.as_str(), false)], lookup_result());
        let gateway = Gateway::new(
            session,
            stub_ctx_with_permissions(&AgentMode::Build, permissions),
        );
        let Value::Object(kwargs) = input else {
            panic!("arguments are an object");
        };
        let reply = smol::block_on(call_method(&gateway, method, kwargs));
        assert_eq!(verdict(&reply), expected, "{reply:?}");
    }

    /// A profile that loads the server lazily leaves its tools callable, only unlisted; one that
    /// disables them leaves nothing to call.
    #[test_case("lazy", true ; "lazy_stays_callable")]
    #[test_case("disabled", false ; "disabled_is_gone")]
    fn a_profile_decides_what_a_snippet_can_call(exposure: &str, callable: bool) {
        let profile = Arc::new(
            serde_json::from_value(json!({"overrides": {format!("{NAMESPACE}.*"): exposure}}))
                .unwrap(),
        );
        let (_dir, permissions) = permissions(PermissionsConfig::default(), PermissionMode::Yolo);
        let session = stub_session_replying(&[(LOOKUP_TOOL, "")], Some(lookup_result()))
            .with_actor_policy(profile);
        let gateway = Gateway::new(
            session,
            stub_ctx_with_permissions(&AgentMode::Build, permissions),
        );
        assert_eq!(
            matches!(
                gateway.lookup(Some(NAMESPACE), LOOKUP),
                ScriptName::Function
            ),
            callable
        );
        let reply = smol::block_on(call_lookup(&gateway));
        match callable {
            true => assert_eq!(verdict(&reply), Verdict::Ran),
            false => assert_eq!(reply.unwrap_err().kind, ScriptErrorKind::Attribute),
        }
    }

    /// The type checker names a namespace by its stub class; a misspelt tool there gets the hint
    /// the runtime would have given, and anything else keeps the checker's own words.
    #[test_case(NAMESPACE, "lookpu", Some("Did you mean `lookup`?") ; "a_misspelt_tool")]
    #[test_case("elsewhere", "lookpu", None ; "a_class_no_server_owns")]
    fn a_rejected_unknown_tool_says_what_was_meant(
        namespace: &str,
        method: &str,
        hint: Option<&str>,
    ) {
        let rejection = format!(
            "snippet.py:1:7: error[unresolved-attribute] Object of type `{CLASS_PREFIX}{namespace}` has no attribute `{method}`"
        );
        let session = stub_session_replying(&[(LOOKUP_TOOL, "")], None);
        let gateway = Gateway::new(session, stub_ctx(&AgentMode::Build));
        let output = with_tool_hints(
            &gateway.0.names,
            ScriptOutput::refused(ScriptStatus::Rejected, rejection.clone()),
        );
        let diagnostic = output.diagnostic.expect("diagnostic");
        match hint {
            Some(hint) => assert!(
                diagnostic.contains(hint) && diagnostic.ends_with(&rejection),
                "{diagnostic}"
            ),
            None => assert_eq!(diagnostic, rejection),
        }
    }

    /// The model reads back the calls a failed run made under the names its snippet used, while
    /// permission rules, the ledger and the cards keep the server's own.
    #[test]
    fn a_failed_run_lists_its_calls_as_the_snippet_wrote_them() {
        const SERVER_TOOL: &str = "stub.get-sum";
        const METHOD: &str = "get_sum";
        const RAISED: &str = "RuntimeError";
        let reply = json!({
            "content": [{"type": "text", "text": ERROR_TEXT}],
            "isError": true,
        });
        let session = stub_session_replying(&[(SERVER_TOOL, "")], Some(reply));
        let (_dir, permissions) = permissions(PermissionsConfig::default(), PermissionMode::Yolo);
        let gateway = Gateway::new(
            session,
            stub_ctx_with_permissions(&AgentMode::Build, permissions),
        );
        let error = smol::block_on(call_method(&gateway, METHOD, Map::new())).unwrap_err();
        let calls = gateway.finish();
        assert_eq!(calls[0].qualified_name(), SERVER_TOOL);
        let text = McpCodeOutput {
            calls,
            outcome: McpCodeOutcome::Raised {
                kind: RAISED.to_owned(),
                message: error.message,
            },
            ..McpCodeOutput::default()
        }
        .model_text();
        assert!(
            text.contains(&format!("{NAMESPACE}{SEPARATOR}{METHOD}"))
                && !text.contains(SERVER_TOOL),
            "{text}"
        );
    }

    const DECISION_MODEL: &str = "mcp-decision-model";
    const DECISION_BASE_URL: &str = "http://127.0.0.1:1";
    const EXPLICIT_TIMEOUT_MS: u64 = 100;
    const RELEVANT: &str = "parser";
    const RELEVANT_SCORE: f64 = 0.9;
    const UNRELATED_SCORE: f64 = 0.1;

    /// Answers a predicate by whether its text names [`RELEVANT`], a choice with its first
    /// option, and a score with its top level, or refuses everything.
    #[derive(Default)]
    struct TypedEngine {
        refuse: bool,
        budgets: Arc<Mutex<Vec<Duration>>>,
    }

    #[async_trait]
    impl DecisionEngine for TypedEngine {
        async fn decide(
            &self,
            request: &EngineRequest,
            deadline: Instant,
        ) -> Result<DecisionResponse, DecisionError> {
            self.budgets
                .lock()
                .unwrap()
                .push(deadline.saturating_duration_since(Instant::now()));
            if self.refuse {
                return Err(DecisionError::Refused);
            }
            let answers = request
                .questions
                .iter()
                .map(|(id, question)| {
                    let criteria = question.criteria.clone().unwrap_or_default();
                    let answer = match question.kind {
                        QuestionType::Noul => Answer::Noul(NoulAnswer {
                            noul: match question.instructions.to_string().contains(RELEVANT) {
                                true => RELEVANT_SCORE,
                                false => UNRELATED_SCORE,
                            },
                        }),
                        QuestionType::Choice => {
                            let options: Vec<String> =
                                criteria.as_object().unwrap().keys().cloned().collect();
                            Answer::Choice(ChoiceAnswer {
                                choice: options[0].clone(),
                                probabilities: options
                                    .iter()
                                    .enumerate()
                                    .map(|(index, option)| {
                                        (option.clone(), f64::from(u8::from(index == 0)))
                                    })
                                    .collect(),
                                confidence: 1.0,
                            })
                        }
                        QuestionType::Score => {
                            let levels = criteria.as_array().unwrap().len();
                            let top = levels - 1;
                            Answer::Score(ScoreAnswer {
                                score: top as f64,
                                legend: (0..levels)
                                    .map(|index| (index.to_string(), index.to_string()))
                                    .collect(),
                                probabilities: (0..levels)
                                    .map(|index| {
                                        (index.to_string(), f64::from(u8::from(index == top)))
                                    })
                                    .collect(),
                                confidence: 1.0,
                            })
                        }
                    };
                    (id.clone(), answer)
                })
                .collect();
            Ok(DecisionResponse {
                model: DECISION_MODEL.to_owned(),
                answers,
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
                cache_hit: false,
            })
        }
    }

    /// A gateway whose session has a decision engine when `engine` is given.
    fn deciding_gateway(mode: AgentMode, engine: Option<TypedEngine>) -> (TempDir, Gateway) {
        let config = PermissionsConfig {
            decision_engine: true,
            ..Default::default()
        };
        let (dir, permissions) = permissions(config, PermissionMode::Ask);
        if let Some(engine) = engine {
            let config = DecisionsConfig {
                base_url: Some(DECISION_BASE_URL.parse().unwrap()),
                ..Default::default()
            };
            let state = StateDir::from_path(dir.path().join("state"));
            permissions.set_decisions(Some(
                Decisions::with_engine(config, &state, engine).unwrap(),
            ));
        }
        let session = stub_session_replying(&[(LOOKUP_TOOL, "")], None);
        let gateway = Gateway::new(session, stub_ctx_with_permissions(&mode, permissions));
        (dir, gateway)
    }

    async fn call_helper(gateway: &Gateway, name: &str, kwargs: Value) -> ScriptReply {
        let Value::Object(kwargs) = kwargs else {
            panic!("keyword arguments are an object");
        };
        let call = ScriptCall {
            namespace: None,
            name: name.to_owned(),
            args: Vec::new(),
            kwargs,
        };
        match gateway.call(call) {
            ScriptInvocation::Ready(reply) => reply,
            ScriptInvocation::Deferred(future) => future.await,
        }
    }

    /// `decide()` asks no permission, so it answers while planning too, in the snippet's own
    /// order and shape, and shows as a row.
    #[test]
    fn decide_answers_in_the_snippets_shape_even_while_planning() {
        let (_dir, gateway) = deciding_gateway(
            AgentMode::Plan(PathBuf::from(PLAN_FILE)),
            Some(TypedEngine::default()),
        );
        let reply = smol::block_on(call_helper(
            &gateway,
            DECIDE_HELPER,
            json!({
                "input": "The parser crashes on empty files.",
                "questions": [
                    {"type": "score", "name": "severity", "instructions": "How bad?", "levels": ["low", "high"]},
                    {"type": "predicate", "name": "parser_bug", "instructions": "A parser bug?"},
                    {"type": "choice", "name": "team", "instructions": "Which team?", "choices": ["core", "docs"]},
                ],
            }),
        ))
        .unwrap();
        let names: Vec<&str> = reply["answers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|answer| answer["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["severity", "parser_bug", "team"]);
        assert_eq!(reply["model"], DECISION_MODEL);
        assert_eq!(
            reply["by_name"]["parser_bug"]["probability"],
            RELEVANT_SCORE
        );
        assert_eq!(reply["by_name"]["team"]["choice"], "core");
        assert_eq!(reply["by_name"]["severity"]["score"], 1.0);
        let calls = gateway.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].qualified_name(),
            format!("{DECISIONS_ROW}{SEPARATOR}{DECIDE_HELPER}")
        );
        assert_eq!(calls[0].called_as, "decide: severity, parser_bug, team");
        assert_eq!(calls[0].status, McpCodeCallStatus::Success);
        assert_eq!(calls[0].helper, Some(McpHelper::Decide));
    }

    #[test]
    fn rerank_keeps_the_relevant_candidates_best_first() {
        let (_dir, gateway) = deciding_gateway(AgentMode::Build, Some(TypedEngine::default()));
        let reply = smol::block_on(call_helper(
            &gateway,
            RERANK_HELPER,
            json!({
                "query": "where parse errors are reported",
                "candidates": ["lexer.rs", "parser.rs", {"path": "docs/parser.md"}, "config.rs"],
                "threshold": 0.5,
            }),
        ))
        .unwrap();
        assert_eq!(
            reply,
            json!([{"index": 1, "score": RELEVANT_SCORE}, {"index": 2, "score": RELEVANT_SCORE}])
        );
        let calls = gateway.finish();
        assert_eq!(calls[0].called_as, "rerank: 4 candidates");
        assert_eq!(calls[0].helper, Some(McpHelper::Rerank));
    }

    /// A batch of 16 outlasts one decision, so a rerank without a timeout gets the rerank
    /// budget rather than the shorter decision deadline, and an explicit timeout still wins.
    #[test_case(None, |budget| budget > Duration::from_millis(DecisionsConfig::default().timeout_ms) ; "rerank_budget_by_default")]
    #[test_case(Some(EXPLICIT_TIMEOUT_MS), |budget| budget <= Duration::from_millis(EXPLICIT_TIMEOUT_MS) ; "explicit_timeout")]
    fn rerank_waits_for_the_rerank_budget(timeout_ms: Option<u64>, fits: fn(Duration) -> bool) {
        let engine = TypedEngine::default();
        let budgets = Arc::clone(&engine.budgets);
        let (_dir, gateway) = deciding_gateway(AgentMode::Build, Some(engine));
        smol::block_on(call_helper(
            &gateway,
            RERANK_HELPER,
            json!({"query": "parser", "candidates": ["parser.rs"], "timeout_ms": timeout_ms}),
        ))
        .unwrap();
        let budget = budgets.lock().unwrap()[0];
        assert!(fits(budget), "{budget:?}");
    }

    #[test]
    fn an_empty_rerank_answers_without_a_call() {
        let (_dir, gateway) = deciding_gateway(
            AgentMode::Build,
            Some(TypedEngine {
                refuse: true,
                ..Default::default()
            }),
        );
        let reply = smol::block_on(call_helper(
            &gateway,
            RERANK_HELPER,
            json!({"query": "parser", "candidates": []}),
        ));
        assert_eq!(reply.unwrap(), json!([]));
        assert!(gateway.finish().is_empty());
    }

    #[test_case(DECIDE_HELPER, json!({"input": "x", "questions": [{"type": "predicate", "name": "a", "instructions": "x"}]}) ; "decide")]
    #[test_case(RERANK_HELPER, json!({"query": "parser", "candidates": ["parser.rs"]}) ; "rerank")]
    fn a_refused_decision_raises_and_marks_its_row(helper: &str, kwargs: Value) {
        let (_dir, gateway) = deciding_gateway(
            AgentMode::Build,
            Some(TypedEngine {
                refuse: true,
                ..Default::default()
            }),
        );
        let error = smol::block_on(call_helper(&gateway, helper, kwargs)).unwrap_err();
        assert_eq!(error.kind, ScriptErrorKind::Runtime);
        assert_eq!(gateway.finish()[0].status, McpCodeCallStatus::Error);
    }

    #[test_case(DECIDE_HELPER ; "decide")]
    #[test_case(RERANK_HELPER ; "rerank")]
    fn without_an_engine_the_decision_helpers_do_not_exist(helper: &str) {
        let (_dir, gateway) = deciding_gateway(AgentMode::Build, None);
        assert!(matches!(
            gateway.lookup(None, helper),
            ScriptName::Undefined(_)
        ));
        assert!(!gateway.stubs().contains(&format!("def {helper}(")));
        let reply = smol::block_on(call_helper(&gateway, helper, json!({})));
        assert_eq!(reply.unwrap_err().kind, ScriptErrorKind::Name);
    }

    #[test]
    fn a_disabled_tool_is_not_there_to_call() {
        let session = stub_session_replying(&[(LOOKUP_TOOL, "")], None)
            .with_disabled_tools(&[LOOKUP_TOOL.to_owned()]);
        let (reply, calls) = lookup(session, stub_ctx(&AgentMode::Build));
        assert_eq!(reply.unwrap_err().kind, ScriptErrorKind::Attribute);
        assert!(calls.is_empty());
    }
}
