//! The gateway: everything a snippet run by the `code` tool can reach, and the only way a run
//! touches anything beyond its interpreter.
//!
//! Host modules describe the calls they offer. Every call they plan passes the checks a direct
//! tool call always did: plan and read-only modes, then the permission prompt, under an id of its
//! own (`<run>:<n>`) so the prompt and the run's card can find each other. A refusal raises
//! `PermissionError` where the call was made; only the user cancelling stops the run.

use std::mem;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use async_io::Timer;
use async_lock::Mutex as AsyncMutex;
use caudra_config::ToolKey;
use caudra_storage::tool_ledger::ToolOutcome as LedgerOutcome;
use futures_lite::future;
use serde_json::Value;

use super::args::{bind, text};
use super::decide::DecisionsModule;
use super::history::HistoryModule;
use super::hosted::HostedCall;
use super::module::{
    AbsentModule, CallFailure, HostCallPlan, HostModule, ModuleDoc, Prepared, ToolCallPlan,
};
use super::names::{HELP, STUB_IMPORTS, renamed};
use super::output::{
    CALL_SOURCE, CodeCall, CodeCallStatus, CodeOutcome, CodeOutput, CodeProgressEvent,
    HostedResult, repaired_lines, written_call,
};
use super::repeats::{Answer, printed_repeats, result_repeats};
use super::runner::{
    ReplLease, ReplNote, ScriptCall, ScriptError, ScriptErrorKind, ScriptHost, ScriptInvocation,
    ScriptName, ScriptOutput, ScriptReply, ScriptRun, ScriptStatus, WaitFuture, installed,
};
use super::tool_module::{ToolModule, output_value};
use crate::agent::change_recording::{self, Recording};
use crate::agent::tool_dispatch::{self, Emit, error_type};
use crate::mcp::code::module::{DOC as MCP_DOC, McpModule};
use crate::mcp::output::{McpBlock, McpInteraction, McpOutput};
use crate::permissions::{PermissionExecutorKind, PermissionSubject};
use crate::tools::native::batch::child_tool_use_id;
use crate::tools::registry::PermissionIntent;
use crate::tools::{READ_ONLY_TOOL_RESTRICTED, ToolContext, ToolEffect, ToolFailure};
use crate::{AgentEvent, ToolStartEvent};

/// A result larger than this raises rather than reaching the snippet.
pub const MAX_RESULT_BYTES: usize = 8 * 1024 * 1024;
/// What a call's card keeps of its result; the rest becomes an excerpt.
pub const MAX_CALL_DISPLAY_BYTES: usize = 64 * 1024;
/// The server column of a `decide()` or `rerank()` row.
pub const DECISIONS_ROW: &str = "decisions";

const EXCERPT_MARKER: &str = "\n[cut: the rest of this result was not kept]";
const BLOCKED_IN_PLAN: &str = "plan mode allows only calls that change nothing, such as the MCP tools a recognised server marks read-only";
const UNFINISHED: &str = "did not finish: the run stopped first";
const WAITING_PREFIX: &str = "waiting ";
const NO_MCP: &str = "mcp is unavailable: no MCP server is enabled; add one in mcp.toml";
const HELP_PARAMS: &[&str] = &["topic"];
const HELP_STUB: &str = "def help(topic: str | None = None) -> str: ...";
const HELP_INDEX: &str = "Modules (`help(\"<module>\")` for one, `help(\"<module>.<name>\")` for a call, `help(\"<server>\")` for an MCP server):";
const AVAILABLE: &str = "available";
/// How the worker shows a host namespace passed as a value, so `help(history)` reads its name.
const MODULE_REPR: (&str, &str) = ("<module '", "'>");
const NO_RUNNER: &str =
    "The `code` tool is unavailable: no code runner is installed in this process";
const GONE_NOTE: &str = "Variables from earlier `code` runs in this conversation are gone (Caudra restarted, or the conversation was resumed, rewound or forked); this run started empty. Fetch again what you need.";
const RESTORE_FAILED_NOTE: &str =
    "Variables from earlier runs could not be restored; this run started empty.";
const RUN_DISCARDED_NOTE: &str =
    "This run's variables could not be kept; the next run sees the variables from before it.";

/// Runs `code` for the agent `ctx` belongs to, holding its variables for the whole run, and stops
/// it once `timeout` has passed.
pub async fn run(ctx: &ToolContext, code: String, reset: bool, timeout: Duration) -> CodeOutput {
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
    let (deadline, cancel) = ctx.cancel.child();
    let running = runner.run(ScriptRun {
        code: code.clone(),
        host,
        state: lease,
        cancel,
    });
    let timed_out = AtomicBool::new(false);
    let outcome = future::or(running, async {
        Timer::after(timeout).await;
        timed_out.store(!ctx.cancel.is_cancelled(), Ordering::Relaxed);
        deadline.cancel();
        future::pending().await
    })
    .await;
    let timed_out = timed_out.into_inner();
    gateway.close_record().await;
    let notes: Vec<String> = [
        forgotten.then(|| GONE_NOTE.to_owned()),
        outcome.note.map(note_text),
    ]
    .into_iter()
    .flatten()
    .collect();
    let mut output = match outcome.output {
        Some(output) => {
            let (printed_repeats, result_repeats) = gateway.repeats(&output);
            CodeOutput {
                printed_repeats,
                result_repeats,
                ..finished(code, gateway.with_rejection_hints(output))
            }
        }
        None => CodeOutput {
            code,
            outcome: match timed_out {
                true => CodeOutcome::TimedOut {
                    after_secs: timeout.as_secs(),
                },
                false => CodeOutcome::Cancelled,
            },
            ..CodeOutput::default()
        },
    };
    output.calls = gateway.finish();
    output.state_note = (!notes.is_empty()).then(|| notes.join(" "));
    for repair in outcome.repairs {
        output.lessons.extend(repair.lesson);
        output.repairs.push(repair.done);
    }
    if let Some(taught) = outcome.taught_code {
        output.repaired_lines = repaired_lines(&output.code, &taught);
    }
    output
}

/// The modules a run reaches, in the order its names resolve. A run started while MCP servers are
/// still making their first connection waits for them, so its names cover every server that comes
/// up; `None` when the run was cancelled first.
async fn modules(ctx: &ToolContext) -> Option<Vec<Arc<dyn HostModule>>> {
    let mcp: Arc<dyn HostModule> = match &ctx.mcp {
        Some(session) => {
            if session.connecting() && ctx.cancel.race(session.ready()).await.is_err() {
                return None;
            }
            Arc::new(McpModule::new(session.clone()))
        }
        None => Arc::new(AbsentModule::new(&MCP_DOC, NO_MCP)),
    };
    Some(vec![
        Arc::new(HistoryModule::for_session(ctx)),
        mcp,
        Arc::new(DecisionsModule::new(ctx.permissions.decisions())),
        Arc::new(ToolModule::image(ctx)),
        Arc::new(ToolModule::host(ctx)),
        Arc::new(ToolModule::workflow(ctx)),
        Arc::new(ToolModule::automation(ctx)),
    ])
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
    /// A call that asks nothing, which only this module's own modules may plan.
    pub(super) fn none() -> Self {
        Self(None)
    }

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
    /// What each call that succeeded handed the snippet, kept for the run so the card can tell a
    /// print of it from output of the snippet's own.
    answers: Mutex<Vec<Answer>>,
    /// The run's change record, `None` until a call that could change files is let through.
    record: AsyncMutex<Option<Recording>>,
    /// Sleeps under way, which the card notes while any is.
    waits: AtomicUsize,
}

impl Gateway {
    pub(super) fn new(ctx: ToolContext, modules: Vec<Arc<dyn HostModule>>) -> Self {
        Self(Arc::new(Inner {
            ctx,
            modules,
            calls: Mutex::new(Vec::new()),
            answers: Mutex::new(Vec::new()),
            record: AsyncMutex::new(None),
            waits: AtomicUsize::new(0),
        }))
    }

    /// What the run's card says beside its header while the run goes on; empty clears it.
    fn annotate(&self, annotation: String) {
        if let Some(id) = self.0.ctx.tool_use_id.clone() {
            self.0
                .ctx
                .event_tx
                .try_send(AgentEvent::ToolAnnotation { id, annotation });
        }
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
        let mut calls = mem::take(&mut *self.calls());
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

    fn answers(&self) -> MutexGuard<'_, Vec<Answer>> {
        self.0
            .answers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The call at `index` handed the snippet `value`.
    fn answered(&self, index: usize, value: Value) -> ScriptReply {
        self.answers().push((index, value.clone()));
        Ok(value)
    }

    /// Which calls the run's printed output and ending value only repeat.
    pub(super) fn repeats(&self, output: &ScriptOutput) -> (Vec<usize>, Option<usize>) {
        let answers = mem::take(&mut *self.answers());
        let printed = match output.stderr.is_empty() {
            true => printed_repeats(&output.stdout, &answers),
            false => Vec::new(),
        };
        (printed, result_repeats(&output.result, &answers))
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
                hosted,
            } = ending;
            call.status = status;
            call.failure = failure;
            call.error = error;
            call.hosted = hosted;
            call.output = output.map(|mut output| {
                interactions.append(&mut output.interactions);
                excerpt(&mut output);
                output
            });
            call.interactions = interactions;
            call.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            call.clone()
        };
        if call.hosted.is_none() {
            report(&call);
        }
        self.publish(index, call);
    }

    /// A hosted tool has started: its row takes the card the tool introduced itself with, so the
    /// header and body it draws on its own show while it works.
    fn started(&self, index: usize, start: &ToolStartEvent) {
        let call = {
            let mut calls = self.calls();
            let Some(call) = calls.get_mut(index) else {
                return;
            };
            if !start.summary.is_empty() {
                call.title = Some(start.summary.clone());
            }
            let card = call
                .hosted
                .get_or_insert_with(|| Box::new(HostedResult::pending(&start.tool)));
            card.effect = start.effect;
            card.input.clone_from(&start.input);
            card.annotation.clone_from(&start.annotation);
            call.clone()
        };
        self.publish(index, call);
    }

    /// The card the hosted tool at `index` has shown so far.
    fn hosted_card(&self, index: usize, tool: &str) -> HostedResult {
        self.calls()
            .get(index)
            .and_then(|call| call.hosted.as_deref().cloned())
            .unwrap_or_else(|| HostedResult::pending(tool))
    }

    /// The call at `index` again, once let through: a row a watcher marked as waiting on the
    /// user's answer reads as running from here.
    fn republish(&self, index: usize) {
        let call = self.calls().get(index).cloned();
        if let Some(call) = call {
            self.publish(index, call);
        }
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
        self.republish(index);
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

    /// Runs a registered tool for a module call through tool dispatch, which holds it to the
    /// session's mode, asks under the tool's own rules and records what it changes, as it would a
    /// call the model made. The row keeps the tool's own result for its card and the ledger.
    async fn run_tool(self, plan: ToolCallPlan) -> ScriptReply {
        let ToolCallPlan {
            mut row,
            tool,
            input,
        } = plan;
        let name = row.called_as.clone();
        let hosted = HostedCall::new(tool, prompt_action(&name, &row.input, None));
        row.title = Some(self.0.ctx.registry.resolve_header(tool, &input));
        row.hosted = Some(Box::new(HostedResult::pending(tool)));
        let index = self.begin(row);
        let mut child = self.child(index);
        child.live_sink = None;
        child.hosted = Some(hosted.clone());
        let started = Instant::now();
        let id = child.tool_use_id.clone().unwrap_or_default();
        let gateway = self.clone();
        let mut on_start = move |start: &ToolStartEvent| gateway.started(index, start);
        let done = tool_dispatch::run(
            &child.registry,
            child.mcp.as_ref(),
            id,
            tool,
            &input,
            &child,
            Emit::Capture(&mut on_start),
        )
        .await;
        let failure = match (done.is_error, done.accounting.outcome) {
            (false, _) => None,
            (true, outcome) => Some(outcome.map_or(ToolFailure::Other, failure_of)),
        };
        let text = done.composed_model_output();
        let value = hosted
            .take()
            .or_else(|| output_value(&done.output))
            .unwrap_or_else(|| Value::String(text.clone()));
        let result = Some(Box::new(HostedResult {
            source: done.accounting.source.as_deref().map(str::to_owned),
            output: Some(done.output),
            annotation: done.annotation,
            ..self.hosted_card(index, tool)
        }));
        let (status, error) = match failure {
            None => {
                let size = serde_json::to_vec(&value).map_or(0, |bytes| bytes.len());
                if size <= MAX_RESULT_BYTES {
                    self.settle(
                        index,
                        started,
                        Ending {
                            status: CodeCallStatus::Success,
                            hosted: result,
                            ..Ending::default()
                        },
                    );
                    return self.answered(index, value);
                }
                (
                    CodeCallStatus::Error,
                    ScriptError::new(ScriptErrorKind::Runtime, too_large(&name, size)),
                )
            }
            Some(ToolFailure::Denied) => (
                CodeCallStatus::Refused,
                ScriptError::new(ScriptErrorKind::Permission, text),
            ),
            Some(_) => (
                CodeCallStatus::Error,
                ScriptError::new(ScriptErrorKind::Runtime, text),
            ),
        };
        self.settle(
            index,
            started,
            Ending {
                status,
                failure: failure.or(Some(ToolFailure::Other)),
                error: Some(error.message.clone()),
                hosted: result,
                ..Ending::default()
            },
        );
        Err(error)
    }

    /// `help()`: the modules and whether each can answer now, or one module's or call's page.
    /// Answered at once, with no row and nothing to approve.
    fn help(&self, call: &ScriptCall) -> ScriptReply {
        let arguments = bind(HELP, HELP_PARAMS, call)?;
        let Some(topic) = text(&arguments, HELP, HELP_PARAMS[0])? else {
            return Ok(Value::String(self.help_index()));
        };
        let topic = topic
            .strip_prefix(MODULE_REPR.0)
            .and_then(|rest| rest.strip_suffix(MODULE_REPR.1))
            .unwrap_or(&topic);
        let (namespace, member) = match topic.split_once('.') {
            Some((namespace, member)) => (namespace, Some(member)),
            None => (topic, None),
        };
        if let Some(module) = self
            .0
            .modules
            .iter()
            .find(|module| module.doc().namespace == namespace)
        {
            let doc = module.doc();
            let page = match member {
                None => {
                    self.0
                        .ctx
                        .code
                        .mark_read(doc.read_first().map(|member| doc.qualified(member)));
                    doc.module_help()
                }
                Some(member) => {
                    let member = doc.member(member).ok_or_else(|| {
                        ScriptError::new(ScriptErrorKind::Value, doc.no_member(member))
                    })?;
                    self.0
                        .ctx
                        .code
                        .mark_read(member.read_first.then(|| doc.qualified(member)));
                    doc.member_help(member)
                }
            };
            return Ok(Value::String(match module.unavailable() {
                Some(reason) => format!("{page}\n\nNot available now: {reason}"),
                None => page,
            }));
        }
        self.0
            .modules
            .iter()
            .find_map(|module| module.dynamic_help(topic))
            .map(Value::String)
            .ok_or_else(|| {
                ScriptError::new(
                    ScriptErrorKind::Value,
                    format!("help() knows no topic `{topic}`; call help() for the modules"),
                )
            })
    }

    /// A read-first call this agent has not been shown the reference of raises with it instead of
    /// running, before any row, prompt or spend. It counts as read from then on, so the retry
    /// runs: the gate guides the model, it does not guard anything.
    fn unread(&self, doc: &ModuleDoc, call: &ScriptCall) -> Option<ScriptError> {
        if call.namespace.as_deref() != Some(doc.namespace) {
            return None;
        }
        let member = doc.member(&call.name).filter(|member| member.read_first)?;
        let qualified = doc.qualified(member);
        let code = &self.0.ctx.code;
        if code.has_read(&qualified) {
            return None;
        }
        code.mark_read([qualified]);
        Some(ScriptError::new(
            ScriptErrorKind::Runtime,
            doc.unread(member),
        ))
    }

    fn help_index(&self) -> String {
        let mut index = HELP_INDEX.to_owned();
        for module in &self.0.modules {
            let doc = module.doc();
            let state = module.unavailable().unwrap_or_else(|| AVAILABLE.to_owned());
            let members: Vec<&str> = doc.member_names().collect();
            index.push_str(&format!(
                "\n- {}: {} ({state})",
                doc.namespace,
                members.join(", ")
            ));
            for line in module.index_lines() {
                index.push_str(&format!("\n  - {line}"));
            }
        }
        index
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
            let message = too_large(&name, size);
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
        self.answered(index, value)
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

/// How a call ended, as its row keeps it.
#[derive(Default)]
struct Ending {
    status: CodeCallStatus,
    failure: Option<ToolFailure>,
    output: Option<McpOutput>,
    interactions: Vec<McpInteraction>,
    error: Option<String>,
    hosted: Option<Box<HostedResult>>,
}

impl From<CallFailure> for Ending {
    fn from(failure: CallFailure) -> Self {
        Self {
            status: CodeCallStatus::Error,
            failure: Some(failure.failure),
            output: failure.output.map(|output| *output),
            interactions: failure.interactions,
            error: Some(failure.error),
            hosted: None,
        }
    }
}

fn too_large(name: &str, size: usize) -> String {
    format!(
        "the result of `{name}` is {size} bytes, more than the {MAX_RESULT_BYTES} a snippet can receive; ask the tool for less"
    )
}

/// The failure a hosted tool's ledger outcome stands for.
fn failure_of(outcome: LedgerOutcome) -> ToolFailure {
    match outcome {
        LedgerOutcome::Cancelled => ToolFailure::Cancelled,
        LedgerOutcome::Timeout => ToolFailure::Timeout,
        LedgerOutcome::Denied => ToolFailure::Denied,
        LedgerOutcome::NotFound => ToolFailure::NotFound,
        LedgerOutcome::InvalidInput => ToolFailure::InvalidInput,
        LedgerOutcome::Ok | LedgerOutcome::Other => ToolFailure::Other,
    }
}

impl ScriptHost for Gateway {
    fn lookup(&self, namespace: Option<&str>, name: &str) -> ScriptName {
        let Some(namespace) = namespace else {
            if name == HELP {
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
            return ScriptName::Undefined(hint.or_else(|| renamed(name)));
        };
        self.defining(namespace, &ScriptName::Namespace)
            .map_or(ScriptName::Undefined(None), |module| {
                module.member(namespace, name)
            })
    }

    fn call(&self, call: ScriptCall) -> ScriptInvocation {
        if call.namespace.is_none() && call.name == HELP {
            return ScriptInvocation::Ready(self.help(&call));
        }
        let owner = match &call.namespace {
            Some(namespace) => self.defining(namespace, &ScriptName::Namespace),
            None => self.defining(&call.name, &ScriptName::Function),
        };
        let Some(module) = owner else {
            let name = call.namespace.as_deref().unwrap_or(&call.name);
            let message = renamed(name).unwrap_or_else(|| format!("name '{name}' is not defined"));
            return ScriptInvocation::Ready(Err(ScriptError::new(ScriptErrorKind::Name, message)));
        };
        if call.namespace.as_deref() == Some(module.doc().namespace)
            && let Some(reason) = module.unavailable()
        {
            return ScriptInvocation::Ready(Err(ScriptError::new(
                ScriptErrorKind::Runtime,
                reason,
            )));
        }
        if let Some(refusal) = self.unread(module.doc(), &call) {
            return ScriptInvocation::Ready(Err(refusal));
        }
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
            Prepared::Tool(plan) => {
                let gateway = self.clone();
                ScriptInvocation::Deferred(Box::pin(async move { gateway.run_tool(*plan).await }))
            }
        }
    }

    fn sleep(&self, delay: Duration) -> WaitFuture {
        let gateway = self.clone();
        Box::pin(async move {
            let _waiting = Waiting::start(gateway, delay);
            Timer::after(delay).await;
        })
    }

    fn stubs(&self) -> String {
        let mut stubs = format!("{STUB_IMPORTS}\n");
        stubs.push_str(HELP_STUB);
        stubs.push('\n');
        for module in &self.0.modules {
            stubs.push_str(&module.stubs());
        }
        stubs
    }
}

/// A sleep under way, shown on the run's card until the last one ends.
struct Waiting(Gateway);

impl Waiting {
    fn start(gateway: Gateway, delay: Duration) -> Self {
        gateway.0.waits.fetch_add(1, Ordering::Relaxed);
        gateway.annotate(format!("{WAITING_PREFIX}{}", seconds(delay)));
        Self(gateway)
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        if self.0.0.waits.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.0.annotate(String::new());
        }
    }
}

/// `30s`, or `0.5s` for a delay with a fraction.
fn seconds(delay: Duration) -> String {
    let seconds = delay.as_secs_f64();
    match seconds.fract() == 0.0 {
        true => format!("{seconds:.0}s"),
        false => format!("{seconds:.1}s"),
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
    let call = written_call(name, input);
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
    if output
        .structured
        .as_ref()
        .is_some_and(|value| value.to_string().len() > MAX_CALL_DISPLAY_BYTES)
    {
        output.structured = None;
    }
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
    use caudra_config::decisions::{DecisionEndpoint, DecisionProtocol};
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
    use serde_json::{Map, json};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::AgentMode;
    use crate::agent::change_recording::fixture::{FakeChanges, Step};
    use crate::code::CodeSession;
    use crate::code::decide;
    use crate::code::history::{self, UNSAVED};
    use crate::code::module::{CallRun, MemberDoc, UNREAD_NOTE};
    use crate::code::names::HISTORY_MODULE;
    use crate::code::names::{DECIDE_HELPER, DECISIONS_MODULE, RERANK_HELPER, SEARCH_HELPER};
    use crate::code::runner::{ReplState, RunFuture, ScriptOutcome, ScriptRunner, install};
    use crate::decisions::Decisions;
    use crate::mcp::code::naming::CLASS_PREFIX;
    use crate::mcp::output::{DecisionHelper, McpHelper};
    use crate::mcp::{McpSession, SEPARATOR, recognised_stub_session, stub_session_replying};
    use crate::permissions::{
        PERMISSION_DENIED_PREFIX, PermissionManager, PermissionMode, PermissionResource,
        PermissionResourceAccess, PermissionResourceKind, PermissionRisk,
    };
    use std::borrow::Cow;

    use crate::cancel::CancelToken;
    use crate::code::names::HOST_MODULE;
    use crate::tools::registry::{
        ExecFuture, HeaderFuture, HeaderResult, ParseError, PermissionScopes, Tool, ToolExecResult,
        ToolInvocation, ToolRegistry,
    };
    use crate::tools::test_support::{stub_ctx, stub_ctx_with, stub_ctx_with_permissions};
    use crate::tools::{CODE_TOOL_NAME, DescriptionContext, ToolSource};
    use crate::types::ToolOutput;
    use crate::{Envelope, EventSender};

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

    /// Waits until stopped, keeping its variables only once the wait is cancelled, as the real
    /// runner does: a run dropped instead of cancelled would lose them.
    const WAIT: &str = "wait";
    /// Makes one lookup, then waits as `WAIT` does.
    const LOOKUP_THEN_WAIT: &str = "lookup then wait";
    const LONG_WAIT: Duration = Duration::from_secs(3600);
    const KEPT_BYTES: usize = 7;
    const RUN_ID: &str = "run";
    const DEADLINE: Duration = Duration::from_secs(120);
    const SHORT_DEADLINE: Duration = Duration::from_secs(1);

    /// Any other snippet completes at once and returns the size of the variables an earlier run
    /// kept.
    struct FakeRunner;

    impl ScriptRunner for FakeRunner {
        fn run(&self, mut run: ScriptRun) -> RunFuture {
            Box::pin(async move {
                if run.code == LOOKUP_THEN_WAIT
                    && let ScriptInvocation::Deferred(call) = run.host.call(ScriptCall {
                        namespace: Some(NAMESPACE.to_owned()),
                        name: LOOKUP.to_owned(),
                        args: Vec::new(),
                        kwargs: Map::new(),
                    })
                {
                    call.await.expect("the lookup runs");
                }
                if run.code == WAIT || run.code == LOOKUP_THEN_WAIT {
                    let waited = run.cancel.race(run.host.sleep(LONG_WAIT)).await;
                    assert!(waited.is_err(), "a long wait ends only when stopped");
                    run.state.keep(Some(ReplState::new(KEPT_BYTES, ())));
                    return ScriptOutcome::default();
                }
                let earlier = run.state.take().map(|state| state.bytes());
                run.state.keep(Some(ReplState::new(0, ())));
                ScriptOutcome {
                    output: Some(ScriptOutput {
                        result: json!(earlier),
                        diagnostic: None,
                        ..ScriptOutput::refused(ScriptStatus::Completed, String::new())
                    }),
                    ..ScriptOutcome::default()
                }
            })
        }
    }

    fn annotations(events: &flume::Receiver<Envelope>) -> Vec<String> {
        events
            .try_iter()
            .filter_map(|envelope| match envelope.event {
                AgentEvent::ToolAnnotation { annotation, .. } => Some(annotation),
                _ => None,
            })
            .collect()
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
        install(Arc::new(FakeRunner));
        let history = match ran_before {
            true => vec![earlier_code_run()],
            false => Vec::new(),
        };
        let mut ctx = stub_ctx(&AgentMode::Build);
        ctx.code = CodeSession::new(&history);
        let first = smol::block_on(run(&ctx, String::new(), reset, DEADLINE));
        let second = smol::block_on(run(&ctx, String::new(), false, DEADLINE));
        assert_eq!(first.state_note.as_deref(), expected);
        assert_eq!(second.state_note, None);
    }

    /// The deadline stops the run the way a cancel would, so the call it made keeps its row and
    /// the next run still sees its variables.
    #[test]
    fn a_run_past_its_deadline_times_out_keeping_its_calls_and_variables() {
        install(Arc::new(FakeRunner));
        let session =
            recognised_stub_session(HOME_ASSISTANT, &[(LOOKUP_TOOL, true)], lookup_result());
        let (_dir, permissions) = permissions(PermissionsConfig::default(), PermissionMode::Yolo);
        let mut ctx = stub_ctx_with_permissions(&AgentMode::Build, permissions);
        ctx.mcp = Some(session);
        let timed_out = smol::block_on(run(
            &ctx,
            LOOKUP_THEN_WAIT.to_owned(),
            false,
            SHORT_DEADLINE,
        ));
        assert_eq!(
            timed_out.outcome,
            CodeOutcome::TimedOut {
                after_secs: SHORT_DEADLINE.as_secs()
            }
        );
        assert_eq!(
            timed_out
                .calls
                .iter()
                .map(|call| call.status)
                .collect::<Vec<_>>(),
            [CodeCallStatus::Success]
        );
        let next = smol::block_on(run(&ctx, String::new(), false, DEADLINE));
        assert_eq!(next.result, Some(KEPT_BYTES.to_string()));
    }

    /// A wait shows on the card until it ends, and the user stopping the run ends it as cancelled,
    /// not timed out.
    #[test]
    fn a_wait_is_noted_on_the_card_and_ends_when_the_user_stops_the_run() {
        install(Arc::new(FakeRunner));
        let (tx, events) = flume::unbounded();
        let mut ctx = stub_ctx_with(
            &AgentMode::Build,
            Some(&EventSender::new(tx, 0)),
            Some(RUN_ID),
        );
        let (stop, cancel) = CancelToken::new();
        ctx.cancel = cancel;
        let stopping = async {
            loop {
                let envelope = events.recv_async().await.expect("events stay open");
                if let AgentEvent::ToolAnnotation { annotation, .. } = envelope.event {
                    assert_eq!(
                        annotation,
                        format!("{WAITING_PREFIX}{}s", LONG_WAIT.as_secs())
                    );
                    break;
                }
            }
            stop.cancel();
        };
        let (output, ()) = smol::block_on(future::zip(
            run(&ctx, WAIT.to_owned(), false, DEADLINE),
            stopping,
        ));
        assert_eq!(output.outcome, CodeOutcome::Cancelled);
        assert_eq!(annotations(&events), [String::new()]);
    }

    #[test_case(Duration::from_secs(30), "30s" ; "whole_seconds")]
    #[test_case(Duration::from_millis(500), "0.5s" ; "a_fraction")]
    fn waits_read_in_seconds(delay: Duration, expected: &str) {
        assert_eq!(seconds(delay), expected);
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

    /// An eager server's tools are callable; a profile that disables them leaves nothing to call.
    #[test_case("eager", true ; "eager_is_callable")]
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
            ..Default::default()
        };
        let (dir, permissions) = permissions(config, PermissionMode::Ask);
        if let Some(engine) = engine {
            let config = DecisionsConfig {
                endpoint: Some(DecisionEndpoint::new(
                    DecisionProtocol::TypeSafe.as_str(),
                    DecisionProtocol::TypeSafe,
                    DECISION_BASE_URL.parse().unwrap(),
                )),
                ..Default::default()
            };
            let state = StateDir::from_path(dir.path().join("state"));
            permissions.set_decisions(Some(
                Decisions::with_engine(config, &state, engine).unwrap(),
            ));
        }
        let decisions = DecisionsModule::new(permissions.decisions());
        let gateway = Gateway::new(
            stub_ctx_with_permissions(&mode, permissions),
            vec![Arc::new(decisions)],
        );
        (dir, gateway)
    }

    async fn call_helper(gateway: &Gateway, name: &str, kwargs: Value) -> ScriptReply {
        let Value::Object(kwargs) = kwargs else {
            panic!("keyword arguments are an object");
        };
        let call = ScriptCall {
            namespace: Some(DECISIONS_MODULE.to_owned()),
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
        assert_eq!(
            calls[0].helper,
            Some(McpHelper::Decision(DecisionHelper::Decide))
        );
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
        assert_eq!(
            calls[0].helper,
            Some(McpHelper::Decision(DecisionHelper::Rerank))
        );
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

    /// The names resolve and type-check in every session, so a snippet reads the same with or
    /// without an engine; without one, a call raises why and `help()` says so.
    #[test_case(DECIDE_HELPER ; "decide")]
    #[test_case(RERANK_HELPER ; "rerank")]
    fn without_an_engine_a_decision_call_raises_why(helper: &str) {
        let (_dir, gateway) = deciding_gateway(AgentMode::Build, None);
        assert_eq!(
            gateway.lookup(Some(DECISIONS_MODULE), helper),
            ScriptName::Function
        );
        assert!(
            gateway
                .stubs()
                .contains(&format!("async def {helper}(self, "))
        );
        let error = smol::block_on(call_helper(&gateway, helper, json!({}))).unwrap_err();
        assert_eq!(error.kind, ScriptErrorKind::Runtime);
        assert!(gateway.help_index().contains(&error.message), "{error:?}");
        assert!(gateway.finish().is_empty());
    }

    /// An old global raises `NameError` naming the call that replaced it.
    #[test_case(DECIDE_HELPER ; "decide")]
    #[test_case(SEARCH_HELPER ; "search")]
    fn an_old_global_names_its_replacement(name: &str) {
        let (_dir, gateway) = deciding_gateway(AgentMode::Build, Some(TypedEngine::default()));
        let expected = renamed(name).unwrap();
        assert_eq!(
            gateway.lookup(None, name),
            ScriptName::Undefined(Some(expected.clone()))
        );
        let error = smol::block_on(invoke(
            &gateway,
            ScriptCall {
                namespace: None,
                name: name.to_owned(),
                args: Vec::new(),
                kwargs: Map::new(),
            },
        ))
        .unwrap_err();
        assert_eq!(
            (error.kind, error.message),
            (ScriptErrorKind::Name, expected)
        );
    }

    fn help_gateway() -> Gateway {
        Gateway::new(
            stub_ctx(&AgentMode::Build),
            vec![
                Arc::new(HistoryModule::new(None)),
                Arc::new(AbsentModule::new(&MCP_DOC, NO_MCP)),
                Arc::new(DecisionsModule::new(None)),
            ],
        )
    }

    fn help(gateway: &Gateway, topic: Option<&str>) -> ScriptReply {
        smol::block_on(invoke(
            gateway,
            ScriptCall {
                namespace: None,
                name: HELP.to_owned(),
                args: topic.map(Value::from).into_iter().collect(),
                kwargs: Map::new(),
            },
        ))
    }

    /// `help()` lists every module with why it cannot answer now, and leaves no row.
    #[test]
    fn help_lists_every_module_with_its_availability() {
        let gateway = help_gateway();
        assert_eq!(gateway.lookup(None, HELP), ScriptName::Function);
        let index = help(&gateway, None).unwrap();
        let index = index.as_str().unwrap();
        for reason in [UNSAVED, NO_MCP] {
            assert!(index.contains(reason), "{index}");
        }
        for doc in [&history::DOC, &MCP_DOC, &decide::DOC] {
            assert!(index.contains(&format!("- {}: ", doc.namespace)), "{index}");
        }
        assert!(gateway.finish().is_empty());
    }

    #[test_case(HISTORY_MODULE ; "by_name")]
    #[test_case("<module 'history'>" ; "by_the_namespace_itself")]
    fn help_shows_a_module_page_with_its_calls(topic: &str) {
        let page = help(&help_gateway(), Some(topic)).unwrap();
        let page = page.as_str().unwrap();
        assert!(page.starts_with(history::DOC.page), "{page}");
        for member in history::DOC.members {
            assert!(
                page.contains(&format!("await history.{}", member.signature)),
                "{page}"
            );
        }
        assert!(
            page.ends_with(&format!("Not available now: {UNSAVED}")),
            "{page}"
        );
    }

    #[test]
    fn help_shows_one_calls_reference() {
        let topic = format!("{HISTORY_MODULE}.{}", history::DOC.members[0].name);
        let page = help(&help_gateway(), Some(&topic)).unwrap();
        assert!(
            page.as_str()
                .unwrap()
                .contains(history::DOC.members[0].page),
            "{page}"
        );
    }

    #[test_case("nowhere" ; "unknown_topic")]
    #[test_case("history.nothing" ; "unknown_member")]
    fn help_on_an_unknown_topic_raises_value_error(topic: &str) {
        let error = help(&help_gateway(), Some(topic)).unwrap_err();
        assert_eq!(error.kind, ScriptErrorKind::Value, "{error:?}");
    }

    /// Every module's names type-check in every session, `help` among them.
    #[test]
    fn stubs_declare_help_and_every_module() {
        let stubs = help_gateway().stubs();
        assert!(stubs.contains(HELP_STUB), "{stubs}");
        for doc in [&history::DOC, &MCP_DOC, &decide::DOC] {
            assert!(stubs.contains(&doc.stubs()), "{stubs}");
        }
    }

    const FAKE_MODULE: &str = "ledger";
    const FAKE_METHOD: &str = "append";
    const FAKE_READ_FIRST_METHOD: &str = "post";
    const FAKE_READ_FIRST_PAGE: &str = "Posts the ledger; `amount` is in cents.";

    /// A module that plans one call which could change files, and counts the times it ran.
    #[derive(Default)]
    struct FakeModule {
        runs: Arc<AtomicUsize>,
    }

    static FAKE_DOC: ModuleDoc = ModuleDoc {
        namespace: FAKE_MODULE,
        summary: "",
        page: "",
        members: &[
            MemberDoc {
                name: FAKE_METHOD,
                signature: "append() -> Any",
                awaited: true,
                read_first: false,
                page: "",
            },
            MemberDoc {
                name: FAKE_READ_FIRST_METHOD,
                signature: "post() -> Any",
                awaited: true,
                read_first: true,
                page: FAKE_READ_FIRST_PAGE,
            },
        ],
    };

    impl HostModule for FakeModule {
        fn doc(&self) -> &'static ModuleDoc {
            &FAKE_DOC
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

    fn read_first_gateway() -> (tempfile::TempDir, Gateway, Arc<AtomicUsize>) {
        let module = FakeModule::default();
        let runs = Arc::clone(&module.runs);
        let (dir, permissions) = permissions(PermissionsConfig::default(), PermissionMode::Yolo);
        let gateway = Gateway::new(
            stub_ctx_with_permissions(&AgentMode::Build, permissions),
            vec![Arc::new(module)],
        );
        (dir, gateway, runs)
    }

    fn call_fake(gateway: &Gateway, method: &str) -> ScriptReply {
        smol::block_on(invoke(
            gateway,
            ScriptCall {
                namespace: Some(FAKE_MODULE.to_owned()),
                name: method.to_owned(),
                args: Vec::new(),
                kwargs: Map::new(),
            },
        ))
    }

    /// A read-first call made before its reference was shown raises with the reference instead
    /// of running, leaving no row; the call after it runs.
    #[test]
    fn a_read_first_call_raises_with_its_reference_then_runs() {
        let (_dir, gateway, runs) = read_first_gateway();

        let error = call_fake(&gateway, FAKE_READ_FIRST_METHOD).unwrap_err();

        assert_eq!(error.kind, ScriptErrorKind::Runtime);
        assert!(
            error.message.starts_with(&format!(
                "`{FAKE_MODULE}.{FAKE_READ_FIRST_METHOD}`{UNREAD_NOTE}"
            )),
            "{error:?}"
        );
        assert!(error.message.ends_with(FAKE_READ_FIRST_PAGE), "{error:?}");
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        assert!(gateway.finish().is_empty());

        assert_eq!(
            verdict(&call_fake(&gateway, FAKE_READ_FIRST_METHOD)),
            Verdict::Ran
        );
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[test_case(Some(FAKE_MODULE) ; "the_module")]
    #[test_case(Some("ledger.post") ; "the_call")]
    fn a_read_first_call_runs_at_once_after_its_help(topic: Option<&str>) {
        let (_dir, gateway, runs) = read_first_gateway();

        let page = help(&gateway, topic).unwrap();

        assert!(
            page.as_str().unwrap().contains(FAKE_READ_FIRST_PAGE),
            "{page}"
        );
        assert_eq!(
            verdict(&call_fake(&gateway, FAKE_READ_FIRST_METHOD)),
            Verdict::Ran
        );
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_call_not_marked_read_first_never_waits_for_help() {
        let (_dir, gateway, runs) = read_first_gateway();

        assert_eq!(verdict(&call_fake(&gateway, FAKE_METHOD)), Verdict::Ran);
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    /// Once the context lost the reference, as a compaction drops it, the next call asks again.
    #[test]
    fn forgetting_references_gates_a_read_first_call_again() {
        let (_dir, gateway, _) = read_first_gateway();
        help(&gateway, Some(FAKE_MODULE)).unwrap();

        gateway.0.ctx.code.forget_references();

        assert!(call_fake(&gateway, FAKE_READ_FIRST_METHOD).is_err());
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

    const HOSTED_TOOL: &str = crate::tools::EXECUTION_ENVIRONMENT_TOOL_NAME;
    const HOSTED_HEADER: &str = "inspect the host";
    const HOSTED_HEADLINE: &str = "linux/x86_64";
    const HOSTED_OWNER: &str = "test";
    const ENVIRONMENT_METHOD: &str = "environment";
    const ENVIRONMENT_CALL: &str = "host.environment()";

    /// Stands in for the Workcell tool `host.environment()` runs.
    struct FakeEnvironment;

    impl Tool for FakeEnvironment {
        fn name(&self) -> &str {
            HOSTED_TOOL
        }

        fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
            Cow::Borrowed("")
        }

        fn schema(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }

        fn parse(&self, _input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
            Ok(Box::new(FakeEnvironmentCall))
        }
    }

    struct FakeEnvironmentCall;

    impl ToolInvocation for FakeEnvironmentCall {
        fn start_header(&self) -> HeaderFuture {
            HeaderFuture::Ready(HeaderResult::plain(HOSTED_HEADER.to_owned()))
        }

        fn execute<'a>(self: Box<Self>, _ctx: &'a ToolContext) -> ExecFuture<'a> {
            Box::pin(async {
                ToolExecResult::from(Ok(ToolOutput::Environment {
                    headline: HOSTED_HEADLINE.to_owned(),
                    summary: String::new(),
                    facts: Vec::new(),
                    commands: Vec::new(),
                }))
            })
        }
    }

    fn hosted_ctx(mode: &AgentMode, effect: ToolEffect) -> ToolContext {
        hosted_ctx_with(mode, effect, None)
    }

    fn hosted_ctx_with(
        mode: &AgentMode,
        effect: ToolEffect,
        events: Option<&EventSender>,
    ) -> ToolContext {
        let registry = Arc::new(ToolRegistry::new());
        registry
            .register_audited(
                Arc::new(FakeEnvironment),
                ToolSource::Native {
                    owner: HOSTED_OWNER.into(),
                    contract: HOSTED_TOOL.into(),
                    trusted: true,
                },
                effect,
            )
            .unwrap();
        let mut ctx = stub_ctx_with(mode, events, Some(RUN_ID));
        ctx.registry = registry;
        ctx
    }

    fn call_environment(ctx: &ToolContext) -> (ScriptReply, Vec<CodeCall>) {
        let gateway = Gateway::new(ctx.clone(), vec![Arc::new(ToolModule::host(ctx))]);
        let call = ScriptCall {
            namespace: Some(HOST_MODULE.to_owned()),
            name: ENVIRONMENT_METHOD.to_owned(),
            args: Vec::new(),
            kwargs: Map::new(),
        };
        let reply = smol::block_on(invoke(&gateway, call));
        (reply, gateway.finish())
    }

    /// A module call runs its tool through dispatch: the snippet reads the card's fields, and
    /// the row keeps the tool's own card and the name the ledger counts it under.
    #[test]
    fn a_hosted_call_runs_its_tool_and_keeps_its_card() {
        let (reply, calls) = call_environment(&hosted_ctx(&AgentMode::Build, ToolEffect::ReadOnly));
        assert_eq!(reply.unwrap()["headline"], HOSTED_HEADLINE);
        let [call] = calls.as_slice() else {
            panic!("one row: {calls:?}");
        };
        assert_eq!(call.status, CodeCallStatus::Success);
        assert_eq!(call.title.as_deref(), Some(HOSTED_HEADER));
        let hosted = call.hosted.as_ref().expect("the tool's card");
        assert!(matches!(
            hosted.output,
            Some(ToolOutput::Environment { .. })
        ));
        assert_eq!(call.ledger_name().0, HOSTED_TOOL);
    }

    /// The reported bug: a slow tool's row read as a bare module call until the tool answered.
    /// Its row is the tool's own card from the start, and takes the header and effect the tool
    /// introduced itself with before it answers.
    #[test]
    fn a_hosted_row_shows_its_tools_card_while_the_tool_runs() {
        let (tx, events) = flume::unbounded();
        let ctx = hosted_ctx_with(
            &AgentMode::Build,
            ToolEffect::ReadOnly,
            Some(&EventSender::new(tx, 0)),
        );
        call_environment(&ctx).0.expect("the tool answers");
        let running: Vec<CodeCall> = events
            .try_iter()
            .filter_map(|envelope| match envelope.event {
                AgentEvent::CodeProgress(progress) => Some(progress.call),
                _ => None,
            })
            .filter(|call| call.status == CodeCallStatus::Running)
            .collect();

        let [begun, started] = running.as_slice() else {
            panic!("a row when it begins and when the tool starts: {running:?}");
        };
        for call in [begun, started] {
            let card = call.hosted.as_ref().expect("the tool's card");
            assert_eq!(card.tool, HOSTED_TOOL);
            assert!(card.output.is_none());
        }
        assert_eq!(started.title.as_deref(), Some(HOSTED_HEADER));
        assert_eq!(
            started.hosted.as_ref().map(|card| card.effect),
            Some(ToolEffect::ReadOnly)
        );
    }

    /// Plan mode holds a hosted tool to its own effect, as when the model called it.
    #[test]
    fn plan_mode_refuses_a_hosted_tool_that_writes() {
        let ctx = hosted_ctx(
            &AgentMode::Plan(PathBuf::from(PLAN_FILE)),
            ToolEffect::Mutating,
        );
        let (reply, calls) = call_environment(&ctx);
        assert!(reply.is_err());
        assert_ne!(calls[0].status, CodeCallStatus::Success);
    }

    /// Calling a hosted tool directly, at the top level or inside `batch`, names the module call.
    #[test]
    fn a_direct_call_of_a_hosted_tool_names_its_module_call() {
        let ctx = hosted_ctx(&AgentMode::Build, ToolEffect::ReadOnly);
        let done = smol::block_on(tool_dispatch::run(
            &ctx.registry,
            None,
            RUN_ID.to_owned(),
            HOSTED_TOOL,
            &json!({}),
            &ctx,
            Emit::Silent,
        ));
        assert!(done.is_error);
        assert!(done.composed_model_output().contains(ENVIRONMENT_CALL));
    }
}
