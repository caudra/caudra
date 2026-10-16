//! The gateway: everything a snippet run by the `code` tool can reach, and the only way a run
//! touches anything beyond its interpreter.
//!
//! Host modules describe the calls they offer. Every call they plan passes the checks a direct
//! tool call always did: plan and read-only modes, then the permission prompt, under an id of its
//! own (`<run>:<n>`) so the prompt and the run's card can find each other. A refusal raises
//! `PermissionError` where the call was made; only the user cancelling stops the run.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use async_lock::Mutex as AsyncMutex;
use caudra_config::ToolKey;
use caudra_storage::tool_ledger::ToolOutcome as LedgerOutcome;
use caudra_workflow::decision::{RerankRequest, decide_label, rerank_label};
use serde_json::{Value, json};

use super::args::bind;
use super::decide::{
    self, DECIDE_PARAMS, DECISION_STUBS, Decide, RERANK_PARAMS, decide_request, rerank_request,
};
use super::history::HistoryModule;
use super::module::{CallFailure, CallRun, HostCallPlan, HostModule, Prepared};
use super::names::{DECIDE_HELPER, DECISION_HELPERS, RERANK_HELPER, STUB_IMPORTS};
use super::output::{
    CALL_SOURCE, CodeCall, CodeCallStatus, CodeOutcome, CodeOutput, CodeProgressEvent,
};
use super::runner::{
    ReplLease, ReplNote, ScriptCall, ScriptError, ScriptErrorKind, ScriptHost, ScriptInvocation,
    ScriptName, ScriptOutput, ScriptReply, ScriptRun, ScriptStatus, installed,
};
use crate::AgentEvent;
use crate::agent::change_recording::{self, Recording};
use crate::agent::tool_dispatch::error_type;
use crate::decisions::Decisions;
use crate::mcp::code::module::McpModule;
use crate::mcp::output::{McpBlock, McpHelper, McpInteraction, McpOutput};
use crate::permissions::{PermissionExecutorKind, PermissionSubject};
use crate::tools::native::batch::child_tool_use_id;
use crate::tools::registry::PermissionIntent;
use crate::tools::{READ_ONLY_TOOL_RESTRICTED, ToolContext, ToolEffect, ToolFailure};

/// A result larger than this raises rather than reaching the snippet.
pub const MAX_RESULT_BYTES: usize = 8 * 1024 * 1024;
/// What a call's card keeps of its result; the rest becomes an excerpt.
pub const MAX_CALL_DISPLAY_BYTES: usize = 64 * 1024;
/// The server column of a `decide()` or `rerank()` row.
pub const DECISIONS_ROW: &str = "decisions";

const EXCERPT_MARKER: &str = "\n[cut: the rest of this result was not kept]";
const BLOCKED_IN_PLAN: &str = "plan mode allows only calls that change nothing, such as the MCP tools a recognised server marks read-only";
const UNFINISHED: &str = "did not finish: the run stopped first";
const NO_RUNNER: &str =
    "The `code` tool is unavailable: no code runner is installed in this process";
const GONE_NOTE: &str = "Variables from earlier `code` runs in this conversation are gone (Caudra restarted, or the conversation was resumed, rewound or forked); this run started empty. Fetch again what you need.";
const RESTORE_FAILED_NOTE: &str =
    "Variables from earlier runs could not be restored; this run started empty.";
const RUN_DISCARDED_NOTE: &str =
    "This run's variables could not be kept; the next run sees the variables from before it.";

/// Runs `code` for the agent `ctx` belongs to, holding its variables for the whole run.
pub async fn run(ctx: &ToolContext, code: String, reset: bool) -> CodeOutput {
    let Some(modules) = modules(ctx).await else {
        return CodeOutput {
            code,
            outcome: CodeOutcome::Cancelled,
            ..CodeOutput::default()
        };
    };
    let mut lease = ReplLease::acquire(ctx.code.repl()).await;
    let forgotten = lease.take_forgotten() && !reset;
    if reset {
        lease.keep(None);
    }
    let Some(runner) = installed() else {
        return CodeOutput {
            code,
            outcome: CodeOutcome::Unavailable {
                message: NO_RUNNER.to_owned(),
            },
            ..CodeOutput::default()
        };
    };
    let gateway = Gateway::new(ctx.clone(), modules);
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
        Some(output) => finished(code, gateway.with_rejection_hints(output)),
        None => CodeOutput {
            code,
            outcome: CodeOutcome::Cancelled,
            ..CodeOutput::default()
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

/// The modules a run reaches, in the order its names resolve. A run started while MCP servers are
/// still making their first connection waits for them, so its names cover every server that comes
/// up; `None` when the run was cancelled first.
async fn modules(ctx: &ToolContext) -> Option<Vec<Arc<dyn HostModule>>> {
    let mut modules: Vec<Arc<dyn HostModule>> = Vec::new();
    if let Some(session) = &ctx.mcp {
        if session.connecting() && ctx.cancel.race(session.ready()).await.is_err() {
            return None;
        }
        modules.push(Arc::new(McpModule::new(session.clone())));
    }
    modules.push(Arc::new(HistoryModule::for_session(ctx)));
    Some(modules)
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

fn finished(code: String, output: ScriptOutput) -> CodeOutput {
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
        ScriptStatus::Completed => CodeOutcome::Completed,
        ScriptStatus::Exception => {
            let (kind, message) = exception
                .map(|exception| (exception.kind, exception.message))
                .unwrap_or_default();
            CodeOutcome::Raised {
                kind,
                message: with_diagnostic(message),
            }
        }
        ScriptStatus::Rejected => CodeOutcome::Rejected {
            message: with_diagnostic(exception.map(|e| e.message).unwrap_or_default()),
        },
        ScriptStatus::Limited => CodeOutcome::Limited {
            message: with_diagnostic(String::new()),
        },
        ScriptStatus::Unavailable => CodeOutcome::Unavailable {
            message: with_diagnostic(String::new()),
        },
    };
    CodeOutput {
        code,
        calls: Vec::new(),
        stdout,
        result: result_repr.or_else(|| rendered(&result)),
        outcome,
        ..CodeOutput::default()
    }
}

fn rendered(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        value => serde_json::to_string_pretty(value).ok(),
    }
}

/// Whether, and as what, a planned call asks before it runs. Only the gateway can make a call that
/// asks nothing, and only for `decide()` and `rerank()`, whose answers are data that grants
/// nothing from the user's own engine.
pub struct Permission(Option<Box<Enforcement>>);

struct Enforcement {
    key: ToolKey,
    intent: PermissionIntent,
    identity: Option<(PermissionSubject, PermissionExecutorKind)>,
    context: Option<String>,
}

impl Permission {
    pub fn ask(
        key: ToolKey,
        intent: PermissionIntent,
        identity: Option<(PermissionSubject, PermissionExecutorKind)>,
    ) -> Self {
        Self(Some(Box::new(Enforcement {
            key,
            intent,
            identity,
            context: None,
        })))
    }

    /// What a prompt for a call that asks as a native tool says under the call itself, such as
    /// whose history it reads.
    pub fn with_context(mut self, context: String) -> Self {
        if let Some(enforcement) = &mut self.0 {
            enforcement.context = Some(context);
        }
        self
    }
}

/// One run's host: what its names resolve to, and the calls it made.
#[derive(Clone)]
pub(super) struct Gateway(Arc<Inner>);

struct Inner {
    ctx: ToolContext,
    modules: Vec<Arc<dyn HostModule>>,
    calls: Mutex<Vec<CodeCall>>,
    /// The run's change record, `None` until a call that could change files is let through.
    record: AsyncMutex<Option<Recording>>,
    /// The session's decision engine, which `decide()` and `rerank()` need.
    decisions: Option<Decisions>,
}

impl Gateway {
    pub(super) fn new(ctx: ToolContext, modules: Vec<Arc<dyn HostModule>>) -> Self {
        let decisions = ctx.permissions.decisions();
        Self(Arc::new(Inner {
            ctx,
            modules,
            calls: Mutex::new(Vec::new()),
            record: AsyncMutex::new(None),
            decisions,
        }))
    }

    /// The module that defines `name` as `kind`.
    fn defining(&self, name: &str, kind: &ScriptName) -> Option<&Arc<dyn HostModule>> {
        self.0
            .modules
            .iter()
            .find(|module| module.global(name).as_ref() == Some(kind))
    }

    /// A rejected snippet's diagnostic, led by what the modules can say about it.
    fn with_rejection_hints(&self, mut output: ScriptOutput) -> ScriptOutput {
        let Some(diagnostic) = output
            .diagnostic
            .as_deref()
            .filter(|_| output.status == ScriptStatus::Rejected)
        else {
            return output;
        };
        let mut hints: Vec<String> = self
            .0
            .modules
            .iter()
            .flat_map(|module| module.rejection_hints(diagnostic))
            .collect();
        hints.dedup();
        if !hints.is_empty() {
            hints.push(diagnostic.to_owned());
            output.diagnostic = Some(hints.join("\n"));
        }
        output
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
    pub(super) fn finish(&self) -> Vec<CodeCall> {
        let mut calls = std::mem::take(&mut *self.calls());
        for call in calls.iter_mut().filter(|call| !call.status.is_terminal()) {
            call.status = CodeCallStatus::Error;
            call.error = Some(UNFINISHED.to_owned());
            call.failure = Some(ToolFailure::Cancelled);
        }
        calls
    }

    fn calls(&self) -> MutexGuard<'_, Vec<CodeCall>> {
        self.0.calls.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn begin(&self, call: CodeCall) -> usize {
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

    fn publish(&self, index: usize, call: CodeCall) {
        let Some(id) = self.0.ctx.tool_use_id.clone() else {
            return;
        };
        let _ = self
            .0
            .ctx
            .event_tx
            .send(AgentEvent::CodeProgress(Box::new(CodeProgressEvent {
                id,
                index,
                call,
            })));
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

    /// Shows a planned call as a row, holds it to the session's mode and permissions, and runs it
    /// once it is let through.
    async fn execute(self, plan: HostCallPlan) -> ScriptReply {
        let HostCallPlan {
            row,
            permission,
            read_only,
            run,
        } = plan;
        let name = row.qualified_name();
        let input = row.input.clone();
        let index = self.begin(row);
        let child = self.child(index);
        let started = Instant::now();
        if let Err(error) = authorize(&child, &name, &input, permission, read_only).await {
            return self.refused(index, started, error);
        }
        if let Err(message) = self.open_record(read_only).await {
            return self.failed(
                index,
                started,
                CallFailure::new(ToolFailure::Other, message),
            );
        }
        match run(child).await {
            Ok((output, value)) => self.succeeded(index, started, output, value),
            Err(failure) => self.failed(index, started, failure),
        }
    }

    /// A `decide()` or `rerank()` call, shown as a row like any other call. It asks no
    /// permission: the engine is the user's own, and an answer is data that grants nothing.
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
        let row = CodeCall {
            server: DECISIONS_ROW.to_owned(),
            tool: helper.to_owned(),
            called_as,
            input: Value::Object(arguments),
            helper: Some(marked),
            ..CodeCall::default()
        };
        let run: CallRun = Box::new(move |ctx: ToolContext| {
            Box::pin(async move { pending.answer(&decisions, &ctx).await })
        });
        self.execute(HostCallPlan {
            row,
            permission: Permission(None),
            read_only: true,
            run,
        })
        .await
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
                CallFailure::new(ToolFailure::Other, message).with_output(output),
            );
        }
        self.settle(
            index,
            started,
            Ending {
                status: CodeCallStatus::Success,
                output: Some(output),
                ..Ending::default()
            },
        );
        Ok(value)
    }

    fn failed(&self, index: usize, started: Instant, failure: CallFailure) -> ScriptReply {
        let message = failure.error.clone();
        self.settle(index, started, Ending::from(failure));
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
                status: CodeCallStatus::Refused,
                failure: Some(failure),
                error: Some(error.message.clone()),
                ..Ending::default()
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

impl Pending {
    async fn answer(
        &self,
        decisions: &Decisions,
        ctx: &ToolContext,
    ) -> Result<(McpOutput, Value), CallFailure> {
        let context = ctx.permissions.decision_context(json!({
            "tool_use_id": ctx.tool_use_id,
        }));
        let answered = match self {
            Self::Decide(decide) => decide::ask(decisions, &context, &decide.request, &ctx.cancel)
                .await
                .and_then(|result| {
                    decide
                        .questions
                        .answers(&result)
                        .map_err(|message| ScriptError::new(ScriptErrorKind::Runtime, message))
                }),
            Self::Rerank(request) => {
                decide::rerank(decisions, &context, request, &ctx.cancel).await
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
                Ok((output, value))
            }
            Err(error) => {
                let failure = match ctx.cancel.is_cancelled() {
                    true => ToolFailure::Cancelled,
                    false => ToolFailure::Other,
                };
                Err(CallFailure::new(failure, error.message))
            }
        }
    }
}

/// How a call ended, as its row keeps it.
#[derive(Default)]
struct Ending {
    status: CodeCallStatus,
    failure: Option<ToolFailure>,
    output: Option<McpOutput>,
    interactions: Vec<McpInteraction>,
    error: Option<String>,
}

impl From<CallFailure> for Ending {
    fn from(failure: CallFailure) -> Self {
        Self {
            status: CodeCallStatus::Error,
            failure: Some(failure.failure),
            output: failure.output.map(|output| *output),
            interactions: failure.interactions,
            error: Some(failure.error),
        }
    }
}

impl ScriptHost for Gateway {
    fn lookup(&self, namespace: Option<&str>, name: &str) -> ScriptName {
        let Some(namespace) = namespace else {
            if DECISION_HELPERS.contains(&name) && self.0.decisions.is_some() {
                return ScriptName::Function;
            }
            let mut hint = None;
            for module in &self.0.modules {
                match module.global(name) {
                    Some(ScriptName::Undefined(message)) => hint = hint.or(message),
                    Some(found) => return found,
                    None => {}
                }
            }
            return ScriptName::Undefined(hint);
        };
        self.defining(namespace, &ScriptName::Namespace)
            .map_or(ScriptName::Undefined(None), |module| {
                module.member(namespace, name)
            })
    }

    fn call(&self, call: ScriptCall) -> ScriptInvocation {
        if call.namespace.is_none()
            && DECISION_HELPERS.contains(&call.name.as_str())
            && let Some(decisions) = self.0.decisions.clone()
        {
            let gateway = self.clone();
            return ScriptInvocation::Deferred(Box::pin(gateway.decision(decisions, call)));
        }
        let owner = match &call.namespace {
            Some(namespace) => self.defining(namespace, &ScriptName::Namespace),
            None => self.defining(&call.name, &ScriptName::Function),
        };
        let Some(module) = owner else {
            let name = call.namespace.as_deref().unwrap_or(&call.name);
            return ScriptInvocation::Ready(Err(ScriptError::new(
                ScriptErrorKind::Name,
                format!("name '{name}' is not defined"),
            )));
        };
        match module.prepare(&self.0.ctx, call) {
            Prepared::Answer(invocation) => invocation,
            Prepared::Plan(plan) => {
                let gateway = self.clone();
                ScriptInvocation::Deferred(Box::pin(async move { gateway.execute(*plan?).await }))
            }
            Prepared::Later(plan) => {
                let gateway = self.clone();
                ScriptInvocation::Deferred(Box::pin(
                    async move { gateway.execute(*plan.await?).await },
                ))
            }
        }
    }

    fn stubs(&self) -> String {
        let mut stubs = format!("{STUB_IMPORTS}\n");
        for module in &self.0.modules {
            stubs.push_str(&module.stubs());
        }
        if self.0.decisions.is_some() {
            for stub in DECISION_STUBS {
                stubs.push_str(stub);
                stubs.push('\n');
            }
        }
        stubs
    }
}

/// Holds a call to the session's mode, then asks as its plan says.
async fn authorize(
    ctx: &ToolContext,
    name: &str,
    input: &Value,
    permission: Permission,
    read_only: bool,
) -> Result<(), ScriptError> {
    let refuse = |message: String| ScriptError::new(ScriptErrorKind::Permission, message);
    if ctx.mode.is_planning() && !read_only {
        return Err(refuse(BLOCKED_IN_PLAN.to_owned()));
    }
    if ctx.policy().is_read_only() && !read_only {
        return Err(refuse(format!("{READ_ONLY_TOOL_RESTRICTED}: {name}")));
    }
    let Some(enforcement) = permission.0 else {
        return Ok(());
    };
    let Enforcement {
        key,
        mut intent,
        identity,
        context,
    } = *enforcement;
    if matches!(key, ToolKey::Native(_)) {
        intent = intent.with_action(prompt_action(name, input, context.as_deref()));
    }
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
        .map_err(|error| refuse(error.to_string()))
}

/// The call as the snippet made it, `history.read(ref="…", limit=5000)`, then the module's
/// context, so a prompt names what runs rather than the `code` tool around it.
fn prompt_action(name: &str, input: &Value, context: Option<&str>) -> String {
    let arguments = match input {
        Value::Object(arguments) => arguments
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join(", "),
        Value::Null => String::new(),
        value => value.to_string(),
    };
    let call = format!("{name}({arguments})");
    match context {
        Some(context) => format!("{call}\n{context}"),
        None => call,
    }
}

fn report(call: &CodeCall) {
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
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

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
    use serde_json::Map;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::AgentMode;
    use crate::agent::change_recording::fixture::{FakeChanges, Step};
    use crate::code::CodeSession;
    use crate::code::runner::{ReplState, RunFuture, ScriptOutcome, ScriptRunner, install};
    use crate::mcp::code::naming::CLASS_PREFIX;
    use crate::mcp::{McpSession, SEPARATOR, recognised_stub_session, stub_session_replying};
    use crate::permissions::{
        PERMISSION_DENIED_PREFIX, PermissionManager, PermissionMode, PermissionResource,
        PermissionResourceAccess, PermissionResourceKind, PermissionRisk,
    };
    use crate::tools::CODE_TOOL_NAME;
    use crate::tools::registry::PermissionScopes;
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

    fn earlier_code_run() -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: String::new(),
                name: CODE_TOOL_NAME.to_owned(),
                input: Value::Null,
                thought_signature: None,
            }],
            ..Message::default()
        }
    }

    fn lookup_result() -> Value {
        json!({"content": [{"type": "text", "text": LOOKUP_RESULT}]})
    }

    fn mcp_gateway(session: McpSession, ctx: ToolContext) -> Gateway {
        Gateway::new(ctx, vec![Arc::new(McpModule::new(session))])
    }

    fn lookup(session: McpSession, ctx: ToolContext) -> (ScriptReply, Vec<CodeCall>) {
        let gateway = mcp_gateway(session, ctx);
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
        invoke(
            gateway,
            ScriptCall {
                namespace: Some(NAMESPACE.to_owned()),
                name: method.to_owned(),
                args: Vec::new(),
                kwargs,
            },
        )
        .await
    }

    async fn invoke(gateway: &Gateway, call: ScriptCall) -> ScriptReply {
        match gateway.call(call) {
            ScriptInvocation::Ready(reply) => reply,
            ScriptInvocation::Deferred(future) => future.await,
        }
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
            Verdict::Ran => CodeCallStatus::Success,
            Verdict::Asked | Verdict::Refused => CodeCallStatus::Refused,
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
        assert_eq!(calls[0].status, CodeCallStatus::Refused);
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
        assert_eq!(calls[0].status, CodeCallStatus::Error);
        assert!(calls[0].output.is_some());
    }

    /// Variables live only in memory, so a conversation that ran `code` under another agent, in an
    /// earlier process or before a rewind or fork, says once that they are gone, unless the run
    /// asked to start empty anyway. No MCP server is needed for any of it.
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
            true => vec![earlier_code_run()],
            false => Vec::new(),
        };
        let mut ctx = stub_ctx(&AgentMode::Build);
        ctx.code = CodeSession::new(&history);
        let first = smol::block_on(run(&ctx, String::new(), reset));
        let second = smol::block_on(run(&ctx, String::new(), false));
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
        let gateway = mcp_gateway(session, ctx);
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
        let gateway = mcp_gateway(
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
        let gateway = mcp_gateway(
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
        let gateway = mcp_gateway(session, stub_ctx(&AgentMode::Build));
        let output = gateway.with_rejection_hints(ScriptOutput::refused(
            ScriptStatus::Rejected,
            rejection.clone(),
        ));
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
        let gateway = mcp_gateway(
            session,
            stub_ctx_with_permissions(&AgentMode::Build, permissions),
        );
        let error = smol::block_on(call_method(&gateway, METHOD, Map::new())).unwrap_err();
        let calls = gateway.finish();
        assert_eq!(calls[0].qualified_name(), SERVER_TOOL);
        let text = CodeOutput {
            calls,
            outcome: CodeOutcome::Raised {
                kind: RAISED.to_owned(),
                message: error.message,
            },
            ..CodeOutput::default()
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
        let gateway = mcp_gateway(session, stub_ctx_with_permissions(&mode, permissions));
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
        invoke(gateway, call).await
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
        assert_eq!(calls[0].status, CodeCallStatus::Success);
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
        assert_eq!(gateway.finish()[0].status, CodeCallStatus::Error);
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

    const FAKE_MODULE: &str = "ledger";
    const FAKE_METHOD: &str = "append";

    /// A module that plans one call which could change files, and counts the times it ran.
    #[derive(Default)]
    struct FakeModule {
        runs: Arc<AtomicUsize>,
    }

    impl HostModule for FakeModule {
        fn global(&self, name: &str) -> Option<ScriptName> {
            (name == FAKE_MODULE).then_some(ScriptName::Namespace)
        }

        fn member(&self, _namespace: &str, name: &str) -> ScriptName {
            match name == FAKE_METHOD {
                true => ScriptName::Function,
                false => ScriptName::Undefined(None),
            }
        }

        fn stubs(&self) -> String {
            String::new()
        }

        fn prepare(&self, _ctx: &ToolContext, call: ScriptCall) -> Prepared {
            let runs = Arc::clone(&self.runs);
            let run: CallRun = Box::new(move |_ctx: ToolContext| {
                Box::pin(async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    Ok((McpOutput::default(), json!(LOOKUP_RESULT)))
                })
            });
            let qualified = format!("{FAKE_MODULE}{SEPARATOR}{}", call.name);
            Prepared::Plan(Ok(Box::new(HostCallPlan {
                row: CodeCall {
                    server: FAKE_MODULE.to_owned(),
                    tool: call.name.clone(),
                    called_as: qualified.clone(),
                    ..CodeCall::default()
                },
                permission: Permission::ask(
                    ToolKey::parse(&qualified).unwrap(),
                    PermissionIntent::new(
                        PermissionScopes::single(qualified),
                        vec![PermissionResource {
                            kind: PermissionResourceKind::Custom {
                                name: FAKE_MODULE.to_owned(),
                            },
                            value: FAKE_MODULE.to_owned(),
                            access: Some(PermissionResourceAccess::Write),
                            protected: false,
                            requires_prompt: false,
                            attributes: BTreeMap::new(),
                        }],
                        PermissionRisk::Low,
                    ),
                    None,
                ),
                read_only: false,
                run,
            })))
        }
    }

    /// Whatever module plans a call, the gateway holds it to the mode and the permission mode
    /// before it runs.
    #[test_case(AgentMode::Build, PermissionMode::Yolo, Verdict::Ran ; "build_runs_it")]
    #[test_case(AgentMode::Build, PermissionMode::Ask, Verdict::Asked ; "ask_asks")]
    #[test_case(AgentMode::Plan(PathBuf::from(PLAN_FILE)), PermissionMode::Yolo, Verdict::Refused ; "plan_refuses_it")]
    #[test_case(AgentMode::ReadOnly, PermissionMode::Yolo, Verdict::Refused ; "read_only_refuses_it")]
    fn the_gateway_authorizes_every_module_call(
        mode: AgentMode,
        permission_mode: PermissionMode,
        expected: Verdict,
    ) {
        let module = FakeModule::default();
        let runs = Arc::clone(&module.runs);
        let (_dir, permissions) = permissions(PermissionsConfig::default(), permission_mode);
        let gateway = Gateway::new(
            stub_ctx_with_permissions(&mode, permissions),
            vec![Arc::new(module)],
        );
        assert!(matches!(
            gateway.lookup(Some(FAKE_MODULE), FAKE_METHOD),
            ScriptName::Function
        ));
        let call = ScriptCall {
            namespace: Some(FAKE_MODULE.to_owned()),
            name: FAKE_METHOD.to_owned(),
            args: Vec::new(),
            kwargs: Map::new(),
        };
        let reply = smol::block_on(invoke(&gateway, call));
        assert_eq!(verdict(&reply), expected, "{reply:?}");
        assert_eq!(
            runs.load(Ordering::SeqCst),
            usize::from(expected == Verdict::Ran)
        );
        assert_eq!(gateway.finish().len(), 1);
    }

    const HISTORY_READ: &str = "history.read";
    const READ_CONTEXT: &str = "Reads from \"Release notes\" in the history of /work/other";

    /// A prompt for a host call names the call as the snippet made it, then what the module adds.
    #[test_case(json!({"ref": "abc/def", "limit": 5000}), Some(READ_CONTEXT), "history.read(ref=\"abc/def\", limit=5000)\nReads from \"Release notes\" in the history of /work/other" ; "arguments_and_context")]
    #[test_case(json!({}), None, "history.read()" ; "no_arguments")]
    fn a_prompt_names_the_call_and_its_context(
        input: Value,
        context: Option<&str>,
        expected: &str,
    ) {
        assert_eq!(prompt_action(HISTORY_READ, &input, context), expected);
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
