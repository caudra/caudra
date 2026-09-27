//! `batch`: run independent tool calls concurrently.
//!
//! `entries` is the single source of truth. The model's answer, the live view,
//! and the restored transcript are all pure functions of it, so the three can
//! never disagree. Children dispatch through the same
//! [`tool_dispatch::run`] every other call goes through, silenced so the
//! transcript shows one batch rather than N loose tools; the batch republishes
//! each child's state as it changes.

use std::borrow::Cow;
use std::mem;
use std::sync::{Arc, LazyLock, Mutex};

use serde_json::{Map, Value};

use crate::agent::speculative::{Peeked, with_live};
use crate::agent::tool_dispatch::{self, Emit};
use crate::tools::registry::{
    ExecFuture, HeaderFuture, HeaderResult, ParseError, Tool, ToolExecResult, ToolInvocation,
};
use crate::tools::schema::{ParamSchema, Property, to_json_schema, validate};
use crate::tools::{DescriptionContext, ToolAudience, ToolContext, ToolEffect};
use crate::types::{
    BatchProgressEvent, BatchToolEntry, BatchToolStatus, ToolDoneEvent, ToolOutput, ToolStartEvent,
};
use crate::{AgentEvent, task_set::TaskSet};

pub const MAX_BATCH_SIZE: usize = 25;

pub const DESCRIPTION: &str = "Executes multiple independent tool calls concurrently to reduce round-trips.

ALWAYS USE THE BATCH TOOL WHEN YOU HAVE MULTIPLE INDEPENDENT TOOL CALLS. This dramatically improves performance.

Rules:
- 1-25 tool calls per batch
- All calls run in parallel; order NOT guaranteed
- Partial failures do not stop other calls
- Do NOT nest batch inside batch
- Do NOT use for dependent operations or when filtering results (use python_execution)";

const SECTION_PREFIX: &str = "## ";
const HEADER_SUFFIX: &str = " tools";
const ERROR_PREFIX: &str = "[ERROR] ";
const EMPTY_ERROR: &str = "provide at least one tool call";
const NESTED_ERROR: &str = "cannot nest batch inside batch";
const CANCELLED_ERROR: &str = "cancelled";
const DISCARDED_PREFIX: &str = "maximum of ";
const DISCARDED_SUFFIX: &str = " tools per batch";
const ENTRY_NOT_OBJECT: &str = "batch entry must be an object";
const ENTRY_NO_TOOL: &str = "batch entry missing 'tool'";
const ENTRY_NO_PARAMS: &str = "batch entry missing 'parameters'";
const PARAMS_NOT_OBJECT: &str = "'parameters' must be an object when flat fields are also present";
const CALLS_NOT_ARRAY: &str = "tool_calls must be an array";
const TOOL_FIELD: &str = "tool";
const PARAMETERS_FIELD: &str = "parameters";
const FUNCTIONS_PREFIX: &str = "functions.";
/// What joins a batch's id to a child's index to make the child's own id.
const CHILD_ID_SEPARATOR: char = ':';

static CALL_PARAM: ParamSchema = ParamSchema::Any {
    description: "Tool invocation: { tool: string, parameters: object } or flat { tool: string, ...params }",
};
static CALLS_PARAM: ParamSchema = ParamSchema::Array {
    items: &CALL_PARAM,
    description: "Array of tool calls to execute in parallel",
};
static PROPERTIES: &[Property] = &[("tool_calls", &CALLS_PARAM, true, &[])];
static SCHEMA: ParamSchema = ParamSchema::Object {
    properties: PROPERTIES,
    description: "",
    reject_unknown: false,
};

pub struct BatchTool;

impl Tool for BatchTool {
    fn name(&self) -> &str {
        crate::tools::BATCH_TOOL_NAME
    }

    fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
        Cow::Borrowed(DESCRIPTION)
    }

    fn schema(&self) -> Value {
        to_json_schema(&SCHEMA)
    }

    fn audience(&self) -> ToolAudience {
        ToolAudience::MAIN | ToolAudience::RESEARCH_SUB | ToolAudience::GENERAL_SUB
    }

    fn examples(&self) -> Option<Value> {
        Some(serde_json::json!([{
            "tool_calls": [
                { "tool": "file_glob", "parameters": { "pattern": "src/**/*.ts" } },
                { "tool": "file_grep", "parameters": { "pattern": "import", "include": "*.ts" } },
                { "tool": "file_index", "parameters": { "path": "/project/index.ts" } },
            ],
        }]))
    }

    fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        let input = validate(&SCHEMA, input.clone())?;
        let calls = input
            .get("tool_calls")
            .and_then(Value::as_array)
            .ok_or_else(|| ParseError::custom(CALLS_NOT_ARRAY))?;
        let mut children = calls
            .iter()
            .map(normalize)
            .collect::<Result<Vec<_>, _>>()
            .map_err(ParseError::custom)?;
        if children.is_empty() {
            return Err(ParseError::custom(EMPTY_ERROR));
        }
        // Entries past the cap are born rejected rather than dropped: the
        // model asked for them and needs to see they did not run.
        for child in children.iter_mut().skip(MAX_BATCH_SIZE) {
            child.rejection = Some(&DISCARDED_ERROR);
        }
        Ok(Box::new(BatchCall { children }))
    }
}

/// One requested call, before the registry has seen it.
static DISCARDED_ERROR: LazyLock<String> =
    LazyLock::new(|| format!("{DISCARDED_PREFIX}{MAX_BATCH_SIZE}{DISCARDED_SUFFIX}"));

/// What a batch calls itself. Shared with the reader that counts the children
/// out of the still-arriving arguments, so the header it shows while the call
/// streams is the one it keeps once the call runs.
pub(crate) fn roster_header(count: usize) -> String {
    format!("{count}{HEADER_SUFFIX}")
}

struct Child {
    tool: String,
    params: Value,
    /// Set for a child that will never run: it renders as a failure and its
    /// reason is what the model gets back.
    rejection: Option<&'static str>,
}

/// Models send entries in two shapes, `{ tool, parameters }` and flat
/// `{ tool, ...params }`, so accept either, or even both merged, as long as no
/// key appears twice.
fn normalize(entry: &Value) -> Result<Child, String> {
    let entry = entry.as_object().ok_or(ENTRY_NOT_OBJECT)?;
    let tool = entry
        .get(TOOL_FIELD)
        .and_then(Value::as_str)
        .ok_or(ENTRY_NO_TOOL)?;
    // Strip GPT's `functions.` prefix so headers and the nested-batch guard
    // both see the name the registry knows.
    let tool = tool
        .strip_prefix(FUNCTIONS_PREFIX)
        .unwrap_or(tool)
        .to_owned();
    let mut flat: Map<String, Value> = entry
        .iter()
        .filter(|(key, _)| *key != TOOL_FIELD && *key != PARAMETERS_FIELD)
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let params = match entry.get(PARAMETERS_FIELD) {
        None if flat.is_empty() => return Err(ENTRY_NO_PARAMS.into()),
        None => Value::Object(flat),
        Some(nested) if flat.is_empty() => nested.clone(),
        Some(Value::Object(nested)) => {
            for (key, value) in nested {
                if flat.insert(key.clone(), value.clone()).is_some() {
                    return Err(format!(
                        "duplicate parameter '{key}' in both 'parameters' and flat fields"
                    ));
                }
            }
            Value::Object(flat)
        }
        Some(_) => return Err(PARAMS_NOT_OBJECT.into()),
    };
    let rejection = (tool == crate::tools::BATCH_TOOL_NAME).then_some(NESTED_ERROR);
    Ok(Child {
        tool,
        params,
        rejection,
    })
}

struct BatchCall {
    children: Vec<Child>,
}

impl ToolInvocation for BatchCall {
    fn start_header(&self) -> HeaderFuture {
        HeaderFuture::Ready(HeaderResult::plain(roster_header(self.children.len())))
    }

    /// Publishing the roster up front is what lets progress events name a
    /// child by index instead of resending the whole batch each time.
    ///
    /// A child already running from the stream keeps the row it has earned:
    /// this output replaces whatever the roster drew, so a pending row here
    /// would take a running child back to the start and leave it there.
    fn start_output(&self, ctx: &ToolContext) -> Option<ToolOutput> {
        let mut entries: Vec<BatchToolEntry> = self
            .children
            .iter()
            .map(|child| child.pending_entry(ctx))
            .collect();
        if let Some(runs) = &ctx.speculative {
            let calls = self
                .children
                .iter()
                .map(|child| (child.tool.as_str(), &child.params));
            let peeked = runs.peek_all(ctx.tool_use_id.as_deref().unwrap_or_default(), calls);
            for ((entry, child), peeked) in entries.iter_mut().zip(&self.children).zip(peeked) {
                if let Some(started) = peeked
                    .as_ref()
                    .filter(|_| child.rejection.is_none())
                    .and_then(Peeked::entry)
                {
                    adopt_row(entry, started);
                }
            }
        }
        Some(ToolOutput::Batch {
            entries,
            text: String::new(),
        })
    }

    fn execute<'a>(self: Box<Self>, ctx: &'a ToolContext) -> ExecFuture<'a> {
        Box::pin(async move { self.run(ctx).await })
    }
}

impl Child {
    /// The row a child draws before it has introduced itself. Resolved from
    /// the call the model wrote rather than left blank: a child can be settled
    /// without ever publishing a start — adopted from a speculative run still
    /// in permission gating, refused at the boundary, swept by a cancelled
    /// turn — and this row is then its only description. A bare `Ran` names
    /// nothing the reader can act on.
    fn pending_header(&self, ctx: &ToolContext) -> String {
        let name = ctx.resolve_tool_name_alias(&self.tool);
        let header = ctx.registry.resolve_header(name, &self.params);
        match header == name {
            true => String::new(),
            false => header,
        }
    }

    fn pending_entry(&self, ctx: &ToolContext) -> BatchToolEntry {
        let (status, output) = match self.rejection {
            Some(reason) => (
                BatchToolStatus::Error,
                Some(ToolOutput::Plain(reason.into())),
            ),
            None => (BatchToolStatus::Pending, None),
        };
        BatchToolEntry {
            model_suffix: None,
            tool: self.tool.clone(),
            effect: ToolEffect::Unknown,
            summary: self.pending_header(ctx),
            status,
            input: None,
            raw_input: Some(self.params.clone()),
            output,
            annotation: None,
        }
    }
}

impl BatchCall {
    async fn run(mut self: Box<Self>, ctx: &ToolContext) -> ToolExecResult {
        for child in &mut self.children {
            if child.rejection.is_none()
                && ctx.resolve_tool_name_alias(&child.tool) == crate::tools::BATCH_TOOL_NAME
            {
                child.rejection = Some(NESTED_ERROR);
            }
        }
        let entries: Vec<BatchToolEntry> = self
            .children
            .iter()
            .map(|child| child.pending_entry(ctx))
            .collect();
        let entries = Arc::new(Mutex::new(entries));
        // Reserve the complete child roster before any child executes, so
        // concurrent completion cannot choose the collector's bounded prefix.
        if let Some(observations) = &ctx.steering_observations {
            observations.expand();
        }
        let runnable: Vec<_> = self
            .children
            .into_iter()
            .enumerate()
            .filter_map(|(index, child)| {
                let mut child_ctx = child_context(ctx, index);
                if let Some(observations) = ctx.speculative.as_ref().and_then(|runs| {
                    runs.observation_for(child_ctx.tool_use_id.as_deref().unwrap_or_default())
                }) {
                    child_ctx.steering_observations = Some(observations);
                } else {
                    tool_dispatch::observe_context(&mut child_ctx, &child.tool, &child.params);
                }
                if child.rejection.is_some() {
                    child_ctx.mark_tool_result_repairable();
                    if !ctx.cancel.is_cancelled()
                        && let Some(observations) = &child_ctx.steering_observations
                    {
                        observations.finish(true);
                    }
                    None
                } else {
                    Some((index, child, child_ctx))
                }
            })
            .collect();
        let mut set = TaskSet::new();
        for (index, child, ctx) in runnable {
            // The registry, MCP session, and context are all shared handles,
            // so a child task borrows nothing from this frame.
            let (registry, mcp, ctx) = (Arc::clone(&ctx.registry), ctx.mcp.clone(), ctx);
            let entries = Arc::clone(&entries);
            // A child the stream already started is taken over rather than
            // started again: the work is the same work, and running it twice
            // is exactly what the early start was for.
            if let Some(adopted) = ctx.speculative.as_ref().and_then(|runs| {
                runs.claim(
                    ctx.tool_use_id.as_deref().unwrap_or_default(),
                    &child.tool,
                    &child.params,
                )
            }) {
                set.spawn(async move {
                    if let Some(start) = adopted.start() {
                        publish(&entries, index, &ctx, |entry| {
                            adopt_row(entry, started_entry(start));
                        });
                    }
                    let done = adopted.finish().await;
                    // The adopting context reserved this child, and the run
                    // that answered it never did, so the verdict is recorded
                    // here instead of by the dispatch that produced it.
                    if !ctx.cancel.is_cancelled()
                        && let Some(observations) = &ctx.steering_observations
                    {
                        observations.finish(done.is_error);
                    }
                    publish(&entries, index, &ctx, |entry| settle_entry(entry, &done));
                });
                continue;
            }
            if let Some(message) = ctx
                .speculative
                .as_ref()
                .and_then(|runs| runs.admit(&ctx, &child.tool, &child.params))
            {
                if !ctx.cancel.is_cancelled()
                    && let Some(observations) = &ctx.steering_observations
                {
                    observations.finish(true);
                }
                let done =
                    ToolDoneEvent::error(ctx.tool_use_id.clone().unwrap_or_default(), message);
                publish(&entries, index, &ctx, |entry| settle_entry(entry, &done));
                continue;
            }
            set.spawn(async move {
                let done = with_live(&ctx, |live_ctx| {
                    let entries = &entries;
                    async move {
                        let ctx = &live_ctx;
                        tool_dispatch::run(
                            &registry,
                            mcp.as_ref(),
                            ctx.tool_use_id.clone().unwrap_or_default(),
                            &child.tool,
                            &child.params,
                            ctx,
                            // The roster shows a child the way a standalone card
                            // would, which is the header the call introduced itself
                            // with rather than a bare tool name. Taken as the child
                            // starts, or the row carries no title for as long as it
                            // runs, which on a batch is the whole time worth watching.
                            //
                            // The name comes from the same event for the same reason:
                            // start is the first point where an alias has been
                            // resolved. Under Anthropic OAuth the model calls a tool by
                            // its wire name, so the roster is holding `mcp_Shell` until
                            // this replaces it with `shell`, and every reader of the
                            // name past here is looking at the tool the registry knows.
                            Emit::Capture(&mut |start: &ToolStartEvent| {
                                publish(entries, index, ctx, |entry| {
                                    *entry = started_entry(start);
                                });
                            }),
                        )
                        .await
                    }
                })
                .await;
                publish(&entries, index, &ctx, |entry| settle_entry(entry, &done));
            });
        }
        // A panicked child leaves its entry mid-flight, so anything still
        // non-terminal after the join is settled here: none is left dangling,
        // on screen or in the answer.
        for panic in set.join_all().await.into_iter() {
            if let Err(message) = panic {
                tracing::error!(%message, "batch child panicked");
            }
        }
        let mut entries = take(&entries);
        sweep(&mut entries, ctx);
        let cancelled = ctx.cancel.is_cancelled();
        let text = render_llm(&entries);
        ToolExecResult {
            is_error: cancelled,
            ..ToolExecResult::from(Ok(ToolOutput::Batch { entries, text }))
        }
    }
}

/// A child runs under the batch's context minus the live sink, which belongs
/// to the batch's own row, and with its own id so its permission prompts and
/// output refs do not collide with a sibling's.
fn child_context(ctx: &ToolContext, index: usize) -> ToolContext {
    let mut child = ctx.clone();
    child.tool_use_id = Some(child_tool_use_id(ctx.tool_use_id.as_deref(), index));
    child.local_root_tool_use_id = ctx
        .local_root_tool_use_id
        .clone()
        .or_else(|| ctx.tool_use_id.clone());
    child.steering_order.push(index);
    child.live_sink = None;
    child
}

/// The id a child runs under. Shared with the streaming reader that predicts
/// it, so a chat opened before the dispatch cannot be keyed on an id the
/// dispatch would never use.
pub(crate) fn child_tool_use_id(parent: Option<&str>, index: usize) -> String {
    match parent {
        Some(id) => format!("{id}{CHILD_ID_SEPARATOR}{index}"),
        None => index.to_string(),
    }
}

/// The index `id` names among `parent`'s children, the inverse of
/// [`child_tool_use_id`]. `None` for an id that is not one of them.
pub(crate) fn child_index(parent: &str, id: &str) -> Option<usize> {
    id.strip_prefix(parent)?
        .strip_prefix(CHILD_ID_SEPARATOR)?
        .parse()
        .ok()
}

/// The call one element describes, or `None` when it is one this tool refuses
/// anyway. A refusal belongs to the batch's own answer, so a speculative run
/// never stands in for it.
pub(crate) fn dispatchable(entry: &Value, ctx: &ToolContext) -> Option<(String, Value)> {
    let child = normalize(entry).ok()?;
    let runnable = child.rejection.is_none()
        && ctx.resolve_tool_name_alias(&child.tool) != crate::tools::BATCH_TOOL_NAME;
    runnable.then_some((child.tool, child.params))
}

/// The roster row a child draws once it has introduced itself. The header is
/// the one the call published rather than a bare tool name, and the name is
/// the one the registry knows rather than the wire alias the model used.
pub(crate) fn started_entry(start: &ToolStartEvent) -> BatchToolEntry {
    BatchToolEntry {
        model_suffix: None,
        tool: start.tool.to_string(),
        effect: start.effect,
        summary: start.summary.clone(),
        status: BatchToolStatus::Running,
        input: start.input.clone(),
        raw_input: start.raw_input.clone(),
        output: None,
        annotation: None,
    }
}

/// Replaces a roster row with one a running child published, keeping what the
/// new row does not carry. A speculative child claimed while it is still in
/// permission gating has no header yet, and taking its row verbatim would
/// erase the description the batch resolved from the call itself.
pub(crate) fn adopt_row(entry: &mut BatchToolEntry, mut row: BatchToolEntry) {
    if row.summary.is_empty() {
        row.summary = mem::take(&mut entry.summary);
    }
    if row.raw_input.is_none() {
        row.raw_input = entry.raw_input.take();
    }
    *entry = row;
}

/// The same row once the child has answered.
pub(crate) fn settle_entry(entry: &mut BatchToolEntry, done: &crate::ToolDoneEvent) {
    entry.status = if done.is_error {
        BatchToolStatus::Error
    } else {
        BatchToolStatus::Success
    };
    entry.annotation = done.annotation.clone();
    entry.model_suffix = done.model_suffix.clone();
    entry.output = Some(done.output.clone());
}

/// Addresses one child's row on the batch that owns it, which is the only row
/// a child has: `ctx` is the child's, and the batch's id is its own with the
/// index suffix dropped.
pub(crate) fn publish_child(ctx: &ToolContext, index: usize, entry: BatchToolEntry) {
    let Some(id) = batch_id(ctx) else { return };
    let _ = ctx
        .event_tx
        .send(AgentEvent::BatchProgress(Box::new(BatchProgressEvent {
            id,
            index,
            entry,
        })));
}

fn publish(
    entries: &Mutex<Vec<BatchToolEntry>>,
    index: usize,
    ctx: &ToolContext,
    update: impl FnOnce(&mut BatchToolEntry),
) {
    let entry = {
        let mut entries = lock(entries);
        let Some(entry) = entries.get_mut(index) else {
            return;
        };
        update(entry);
        entry.clone()
    };
    publish_child(ctx, index, entry);
}

/// The child ids are the batch's own with an index appended, so the batch row
/// is recovered by dropping the suffix `child_context` added.
fn batch_id(ctx: &ToolContext) -> Option<String> {
    let id = ctx.tool_use_id.as_ref()?;
    Some(
        id.rsplit_once(CHILD_ID_SEPARATOR)
            .map_or(id.as_str(), |(head, _)| head)
            .to_owned(),
    )
}

/// Whatever never reached a verdict becomes a cancelled child, so the model
/// sees a reason for every call it asked for. The reason is its only clue why
/// the children stopped: a blown deadline must not read as an Esc it can never
/// retry its way out of.
fn sweep(entries: &mut [BatchToolEntry], ctx: &ToolContext) {
    let reason = ctx
        .deadline
        .check()
        .err()
        .unwrap_or_else(|| CANCELLED_ERROR.into());
    for entry in entries.iter_mut().filter(|e| !e.status.is_terminal()) {
        entry.status = BatchToolStatus::Error;
        entry.output = Some(ToolOutput::Plain(reason.clone().into()));
    }
}

fn take(entries: &Mutex<Vec<BatchToolEntry>>) -> Vec<BatchToolEntry> {
    std::mem::take(&mut *lock(entries))
}

fn lock(entries: &Mutex<Vec<BatchToolEntry>>) -> std::sync::MutexGuard<'_, Vec<BatchToolEntry>> {
    entries
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The model's view: one `## tool` section per child in input order, then a
/// tally. `batch_policy.rs` pins this byte for byte.
fn render_llm(entries: &[BatchToolEntry]) -> String {
    let mut out = String::new();
    let mut failed = 0;
    for entry in entries {
        out.push_str(SECTION_PREFIX);
        out.push_str(&entry.tool);
        out.push('\n');
        let text = entry
            .output
            .as_ref()
            .map(ToolOutput::as_text)
            .unwrap_or_default();
        if entry.status == BatchToolStatus::Success {
            out.push_str(&text);
        } else {
            failed += 1;
            out.push_str(ERROR_PREFIX);
            out.push_str(&text);
        }
        for note in [&entry.annotation, &entry.model_suffix]
            .into_iter()
            .flatten()
        {
            out.push('\n');
            out.push_str(note);
        }
        out.push_str("\n\n");
    }
    let total = entries.len();
    if failed > 0 {
        out.push_str(&format!(
            "Executed {}/{total} successfully. {failed} failed.",
            total - failed
        ));
    } else {
        out.push_str(&format!("All {total} tools executed successfully."));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentMode;
    use crate::agent::speculative::{REVISED_INPUT, SpeculativeRuns};
    use crate::agent::tool_dispatch::{ResponseObservations, ToolOutcome};
    use crate::tools::registry::{BoxFuture, PermissionIntent, ToolRegistry, ToolSource};
    use crate::tools::test_support::{stub_ctx, stub_ctx_with};
    use crate::tools::{LockKey, STALE_READ_MSG};
    use futures_lite::future;
    use serde_json::json;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use test_case::test_case;

    const READ: &str = "read";
    const GREP: &str = "grep";
    const PATTERN: &str = "src/**/*.rs";
    const BODY: &str = "file contents";
    const FAILURE: &str = "no such file";
    const PARK: &str = "park";
    const RELEASE: &str = "release";
    const DEADLINE_REASON: &str = crate::tools::DEADLINE_EXCEEDED;
    const BATCH_ID: &str = "batch-1";
    const RAN_TWICE: &str = "an adopted child must not be run a second time";
    const NEVER_STARTED: &str = "the stream must have started the child before the batch runs";

    fn observed_batch_ctx() -> ToolContext {
        let ctx = stub_ctx(&AgentMode::Build);
        ctx.registry
            .register(
                Arc::new(BatchTool),
                ToolSource::Native {
                    owner: super::super::OWNER.into(),
                    contract: crate::tools::BATCH_TOOL_NAME.into(),
                    trusted: true,
                },
            )
            .unwrap();
        ctx
    }

    #[test_case(json!([]); "empty_batch")]
    #[test_case(json!([{"parameters": {}}]); "invalid_child")]
    fn rejected_batch_inputs_are_repairable(calls: Value) {
        smol::block_on(async {
            let mut ctx = observed_batch_ctx();
            let observations = ResponseObservations::new(1);
            ctx.steering_observations = Some(observations.clone());
            let result = tool_dispatch::run(
                &ctx.registry,
                None,
                READ.into(),
                crate::tools::BATCH_TOOL_NAME,
                &json!({"tool_calls": calls}),
                &ctx,
                Emit::Silent,
            )
            .await;
            assert!(result.is_error);
            let (facts, all_repairable) = observations.take();
            assert!(all_repairable);
            assert_eq!(facts.len(), 1);
            assert_eq!(facts[0].outcome, ToolOutcome::Repairable);
        });
    }

    #[test_case(false; "success_after_repairable_sibling")]
    #[test_case(true; "panic_after_repairable_sibling")]
    fn batch_observes_leaves_without_replaying_or_counting_the_wrapper(panics: bool) {
        smol::block_on(async {
            let mut ctx = observed_batch_ctx();
            let executed = Arc::new(AtomicUsize::new(0));
            let count = Arc::clone(&executed);
            ctx.local_tools = Arc::new(HashMap::from([(
                READ.into(),
                crate::tools::local_tool(move |_, _| {
                    let count = Arc::clone(&count);
                    Box::pin(async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        assert!(!panics, "{FAILURE}");
                        Ok(BODY.into())
                    })
                }),
            )]));
            let observations = ResponseObservations::new(2);
            ctx.steering_observations = Some(observations.clone());
            ctx.steering_order = vec![0];
            let result = tool_dispatch::run(
                &ctx.registry,
                None,
                READ.into(),
                crate::tools::BATCH_TOOL_NAME,
                &calls(json!([
                    {"tool": crate::tools::BATCH_TOOL_NAME, "parameters": {}},
                    {"tool": READ, "parameters": {}}
                ])),
                &ctx,
                Emit::Silent,
            )
            .await;
            assert!(!result.is_error);
            let (facts, all_repairable) = observations.take();
            assert_eq!(executed.load(Ordering::SeqCst), 1);
            assert_eq!(facts.len(), 2);
            assert_eq!(facts[0].outcome, ToolOutcome::Repairable);
            assert_eq!(facts[1].name, READ);
            assert_eq!(
                facts[1].outcome,
                if panics {
                    ToolOutcome::Failure
                } else {
                    ToolOutcome::Success
                }
            );
            assert!(!all_repairable);
        });
    }

    #[test]
    fn aliased_nested_batches_and_overflow_are_all_repairable() {
        smol::block_on(async {
            let mut ctx = observed_batch_ctx();
            ctx.tool_name_aliases = Some(Arc::new(HashMap::from([(
                READ.into(),
                crate::tools::BATCH_TOOL_NAME.into(),
            )])));
            let observations = ResponseObservations::new(1);
            ctx.steering_observations = Some(observations.clone());
            let children = vec![json!({"tool": READ, "parameters": {}}); MAX_BATCH_SIZE + 1];
            let result = tool_dispatch::run(
                &ctx.registry,
                None,
                READ.into(),
                crate::tools::BATCH_TOOL_NAME,
                &calls(json!(children)),
                &ctx,
                Emit::Silent,
            )
            .await;
            assert!(!result.is_error);
            let (facts, all_repairable) = observations.take();
            assert_eq!(facts.len(), 1);
            assert!(all_repairable);
        });
    }

    #[test]
    fn batch_fingerprints_use_normalized_child_parameters() {
        smol::block_on(async {
            let mut ctx = observed_batch_ctx();
            ctx.tool_name_aliases = Some(Arc::new(HashMap::from([(GREP.into(), READ.into())])));
            ctx.local_tools = Arc::new(HashMap::from([(
                READ.into(),
                crate::tools::local_tool(|input, _| {
                    Box::pin(async move {
                        assert_eq!(input, json!({"path": PATTERN}));
                        Ok(BODY.into())
                    })
                }),
            )]));
            let observations = ResponseObservations::new(2);
            ctx.steering_observations = Some(observations.clone());
            let result = tool_dispatch::run(
                &ctx.registry,
                None,
                READ.into(),
                crate::tools::BATCH_TOOL_NAME,
                &calls(json!([
                    {"tool": GREP, "path": PATTERN},
                    {"tool": READ, "parameters": {"path": PATTERN}}
                ])),
                &ctx,
                Emit::Silent,
            )
            .await;
            assert!(!result.is_error);
            let (facts, all_repairable) = observations.take();
            assert!(!all_repairable);
            assert_eq!(facts.len(), 2);
            assert_eq!(facts[0], facts[1]);
            assert_eq!(facts[0].name, READ);
            assert_eq!(facts[0].outcome, ToolOutcome::Success);
        });
    }

    /// The model's answer to a batch over `children`, with whatever the
    /// stream started already in `ctx`.
    async fn run_batch(ctx: &ToolContext, children: Value) -> String {
        tool_dispatch::run(
            &ctx.registry,
            None,
            BATCH_ID.into(),
            crate::tools::BATCH_TOOL_NAME,
            &calls(children),
            ctx,
            Emit::Silent,
        )
        .await
        .output
        .as_text()
    }

    /// A context whose one tool numbers its calls, so the answer says which
    /// run produced it: the first belongs to the stream, a second could only
    /// come from the batch starting the work over.
    fn counting_batch_ctx() -> (ToolContext, Arc<AtomicUsize>) {
        let mut ctx = observed_batch_ctx();
        ctx.tool_use_id = Some(BATCH_ID.into());
        let ran = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&ran);
        ctx.local_tools = Arc::new(HashMap::from([(
            READ.into(),
            crate::tools::local_tool(move |_, _| {
                let count = Arc::clone(&count);
                Box::pin(async move {
                    let nth = count.fetch_add(1, Ordering::SeqCst) + 1;
                    Ok(format!("{BODY} {nth}"))
                })
            }),
        )]));
        (ctx, ran)
    }

    fn nth_body(nth: usize) -> String {
        format!("{BODY} {nth}")
    }

    fn child(path: &str) -> Value {
        json!({ "tool": READ, "parameters": { "path": path } })
    }

    /// The whole point: a child the stream started is the child the batch
    /// answers with, run once.
    #[test]
    fn a_child_the_stream_started_is_adopted_rather_than_run_again() {
        smol::block_on(async {
            let (mut ctx, ran) = counting_batch_ctx();
            let runs = Arc::new(SpeculativeRuns::new(&ctx, None));
            runs.register(BATCH_ID, crate::tools::BATCH_TOOL_NAME);
            runs.start(BATCH_ID, 0, &child(PATTERN).to_string());
            runs.settled().await;
            assert_eq!(ran.load(Ordering::SeqCst), 1, "{NEVER_STARTED}");
            ctx.speculative = Some(Arc::clone(&runs));
            let text = run_batch(&ctx, json!([child(PATTERN)])).await;
            assert_eq!(
                text,
                format!(
                    "## {READ}\n{}\n\nAll 1 tools executed successfully.",
                    nth_body(1)
                ),
                "the answer is the run the stream started"
            );
            assert_eq!(ran.load(Ordering::SeqCst), 1, "{RAN_TWICE}");
        });
    }

    #[test]
    fn a_child_the_model_rewrote_reports_its_original_execution() {
        smol::block_on(async {
            let (mut ctx, ran) = counting_batch_ctx();
            let runs = Arc::new(SpeculativeRuns::new(&ctx, None));
            runs.register(BATCH_ID, crate::tools::BATCH_TOOL_NAME);
            runs.start(BATCH_ID, 0, &child("stale.rs").to_string());
            runs.settled().await;
            assert_eq!(ran.load(Ordering::SeqCst), 1, "{NEVER_STARTED}");
            ctx.speculative = Some(Arc::clone(&runs));
            let text = run_batch(&ctx, json!([child(PATTERN)])).await;
            assert!(text.contains(&nth_body(1)));
            assert!(text.contains(REVISED_INPUT));
            assert_eq!(ran.load(Ordering::SeqCst), 1);
            assert!(runs.drain_report().is_none());
        });
    }

    /// An adopted child is the one the response reserved, and its verdict has
    /// to reach that reservation: the dispatch that produced it was made
    /// before the response existed.
    #[test]
    fn an_adopted_child_still_reports_its_outcome_to_the_response() {
        smol::block_on(async {
            let (mut ctx, ran) = counting_batch_ctx();
            let runs = Arc::new(SpeculativeRuns::new(&ctx, None));
            runs.register(BATCH_ID, crate::tools::BATCH_TOOL_NAME);
            runs.start(BATCH_ID, 0, &child(PATTERN).to_string());
            runs.settled().await;
            assert_eq!(ran.load(Ordering::SeqCst), 1, "{NEVER_STARTED}");
            ctx.speculative = Some(Arc::clone(&runs));
            let observations = ResponseObservations::new(2);
            ctx.steering_observations = Some(observations.clone());
            ctx.steering_order = vec![0];
            let text = run_batch(&ctx, json!([child(PATTERN)])).await;
            assert!(text.contains(&nth_body(1)), "{RAN_TWICE}");
            assert_eq!(ran.load(Ordering::SeqCst), 1, "{RAN_TWICE}");
            let (facts, _) = observations.take();
            assert_eq!(facts.len(), 1);
            assert_eq!(facts[0].name, READ);
            assert_eq!(facts[0].outcome, ToolOutcome::Success);
        });
    }

    fn parsed(input: Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        BatchTool.parse(&input)
    }

    fn calls(entries: Value) -> Value {
        json!({ "tool_calls": entries })
    }

    fn normalized(entry: Value) -> Result<Child, String> {
        normalize(&entry)
    }

    /// `Child` holds a raw `Value` and is not worth a `Debug` impl for the
    /// error paths alone, so failures unwrap by hand.
    fn rejection_of(entry: Value) -> String {
        match normalized(entry) {
            Err(error) => error,
            Ok(child) => panic!("entry was accepted as {}", child.tool),
        }
    }

    fn entry(tool: &str, status: BatchToolStatus, text: &str) -> BatchToolEntry {
        BatchToolEntry {
            model_suffix: None,
            tool: tool.into(),
            effect: ToolEffect::Unknown,
            summary: String::new(),
            status,
            input: None,
            raw_input: None,
            output: Some(ToolOutput::Plain(text.into())),
            annotation: None,
        }
    }

    #[test]
    fn the_nested_shape_is_taken_verbatim() {
        let child = normalized(json!({ "tool": READ, "parameters": { "path": PATTERN } })).unwrap();
        assert_eq!(child.tool, READ);
        assert_eq!(child.params, json!({ "path": PATTERN }));
    }

    #[test]
    fn the_flat_shape_becomes_the_parameters() {
        let child = normalized(json!({ "tool": READ, "path": PATTERN })).unwrap();
        assert_eq!(child.params, json!({ "path": PATTERN }));
    }

    #[test]
    fn the_two_shapes_merge_when_they_do_not_collide() {
        let child = normalized(
            json!({ "tool": GREP, "include": "*.rs", "parameters": { "pattern": PATTERN } }),
        )
        .unwrap();
        assert_eq!(
            child.params,
            json!({ "include": "*.rs", "pattern": PATTERN })
        );
    }

    #[test]
    fn a_key_given_twice_is_rejected_rather_than_silently_picked() {
        let error = rejection_of(
            json!({ "tool": GREP, "pattern": "flat", "parameters": { "pattern": "nested" } }),
        );
        assert!(
            error.contains("duplicate parameter 'pattern'"),
            "got: {error}"
        );
    }

    #[test]
    fn a_non_object_parameters_alongside_flat_fields_is_rejected() {
        assert_eq!(
            rejection_of(json!({ "tool": GREP, "include": "*.rs", "parameters": 7 })),
            PARAMS_NOT_OBJECT
        );
    }

    #[test_case(json!(["not an object"]), ENTRY_NOT_OBJECT; "not_an_object")]
    #[test_case(json!({ "parameters": {} }), ENTRY_NO_TOOL; "no_tool")]
    #[test_case(json!({ "tool": READ }), ENTRY_NO_PARAMS; "no_parameters")]
    fn a_malformed_entry_is_named(entry: Value, expected: &str) {
        assert_eq!(rejection_of(entry), expected);
    }

    #[test]
    fn gpts_functions_prefix_is_stripped_so_the_registry_recognises_the_name() {
        let child = normalized(json!({ "tool": "functions.read", "path": PATTERN })).unwrap();
        assert_eq!(child.tool, READ);
    }

    #[test]
    fn a_nested_batch_is_born_rejected_rather_than_dispatched() {
        let child =
            normalized(json!({ "tool": crate::tools::BATCH_TOOL_NAME, "tool_calls": [] })).unwrap();
        assert_eq!(child.rejection, Some(NESTED_ERROR));
        let ctx = stub_ctx(&AgentMode::Build);
        assert_eq!(
            child.pending_entry(&ctx).status,
            BatchToolStatus::Error,
            "{NESTED_ERROR}"
        );
    }

    #[test]
    fn a_functions_prefixed_nested_batch_is_caught_too() {
        let child = normalized(json!({ "tool": "functions.batch", "tool_calls": [] })).unwrap();
        assert_eq!(child.rejection, Some(NESTED_ERROR));
    }

    #[test]
    fn an_empty_batch_is_refused() {
        assert!(parsed(calls(json!([]))).is_err());
    }

    #[test]
    fn entries_past_the_cap_are_rejected_but_still_reported() {
        let entries: Vec<Value> = (0..MAX_BATCH_SIZE + 2)
            .map(|_| json!({ "tool": READ, "path": PATTERN }))
            .collect();
        let output = parsed(calls(json!(entries)))
            .expect("a full batch still parses")
            .start_output(&stub_ctx(&AgentMode::Build))
            .expect("batch publishes its roster");
        let ToolOutput::Batch { entries, .. } = output else {
            panic!("expected a batch roster");
        };
        assert_eq!(
            entries.len(),
            MAX_BATCH_SIZE + 2,
            "every call is accounted for"
        );
        assert_eq!(entries[MAX_BATCH_SIZE - 1].status, BatchToolStatus::Pending);
        assert_eq!(entries[MAX_BATCH_SIZE].status, BatchToolStatus::Error);
        assert_eq!(
            entries[MAX_BATCH_SIZE]
                .output
                .as_ref()
                .map(ToolOutput::as_text),
            Some(DISCARDED_ERROR.clone())
        );
    }

    #[test]
    fn the_roster_names_every_child_before_any_of_them_runs() {
        let output = parsed(calls(
            json!([{ "tool": READ, "path": PATTERN }, { "tool": GREP, "pattern": "x" }]),
        ))
        .unwrap()
        .start_output(&stub_ctx(&AgentMode::Build))
        .expect("batch publishes its roster");
        let ToolOutput::Batch { entries, .. } = output else {
            panic!("expected a batch roster");
        };
        let tools: Vec<&str> = entries.iter().map(|e| e.tool.as_str()).collect();
        assert_eq!(tools, [READ, GREP]);
        assert!(entries.iter().all(|e| e.status == BatchToolStatus::Pending));
    }

    #[test]
    fn a_clean_run_reports_every_section_in_input_order_then_a_tally() {
        let text = render_llm(&[
            entry(READ, BatchToolStatus::Success, BODY),
            entry(GREP, BatchToolStatus::Success, "3 matches"),
        ]);
        assert_eq!(
            text,
            format!(
                "## {READ}\n{BODY}\n\n## {GREP}\n3 matches\n\nAll 2 tools executed successfully."
            )
        );
    }

    #[test]
    fn a_failed_child_is_marked_and_counted_without_stopping_the_others() {
        let text = render_llm(&[
            entry(READ, BatchToolStatus::Error, FAILURE),
            entry(GREP, BatchToolStatus::Success, "3 matches"),
        ]);
        assert_eq!(
            text,
            format!(
                "## {READ}\n{ERROR_PREFIX}{FAILURE}\n\n## {GREP}\n3 matches\n\nExecuted 1/2 successfully. 1 failed."
            )
        );
    }

    #[test]
    fn a_child_left_mid_flight_is_settled_with_the_reason_it_stopped() {
        let mut entries = vec![
            entry(READ, BatchToolStatus::Success, BODY),
            entry(GREP, BatchToolStatus::Running, ""),
        ];
        sweep(&mut entries, &stub_ctx(&AgentMode::Build));
        assert_eq!(
            entries[0].status,
            BatchToolStatus::Success,
            "settled children are left alone"
        );
        assert_eq!(entries[1].status, BatchToolStatus::Error);
        assert_eq!(
            entries[1].output.as_ref().map(ToolOutput::as_text),
            Some(CANCELLED_ERROR.to_owned())
        );
    }

    #[test]
    fn a_blown_deadline_does_not_read_as_a_cancel_the_model_could_retry() {
        let mut ctx = stub_ctx(&AgentMode::Build);
        ctx.deadline = crate::tools::Deadline::At(std::time::Instant::now());
        let mut entries = vec![entry(GREP, BatchToolStatus::Pending, "")];
        sweep(&mut entries, &ctx);
        assert_eq!(
            entries[0].output.as_ref().map(ToolOutput::as_text),
            Some(DEADLINE_REASON.to_owned())
        );
    }

    #[test_case(Some("toolu_01:3"), Some("toolu_01"); "child_id_drops_its_index")]
    #[test_case(Some("toolu_01"), Some("toolu_01"); "an_unsuffixed_id_is_its_own_batch")]
    #[test_case(None, None; "no_id_means_nothing_to_patch")]
    fn a_progress_event_names_the_batch_row_not_the_child(
        id: Option<&str>,
        expected: Option<&str>,
    ) {
        let mut ctx = stub_ctx(&AgentMode::Build);
        ctx.tool_use_id = id.map(String::from);
        assert_eq!(batch_id(&ctx).as_deref(), expected);
    }

    #[test]
    fn each_child_runs_under_its_own_id_so_siblings_cannot_collide() {
        let mut ctx = stub_ctx(&AgentMode::Build);
        ctx.tool_use_id = Some("toolu_01".into());
        assert_eq!(
            child_context(&ctx, 0).tool_use_id.as_deref(),
            Some("toolu_01:0")
        );
        assert_eq!(
            child_context(&ctx, 1).tool_use_id.as_deref(),
            Some("toolu_01:1")
        );
        assert!(
            child_context(&ctx, 0).live_sink.is_none(),
            "the live row belongs to the batch, not to a child"
        );
    }

    #[test_case(None, "local-batch"; "direct_batch_uses_parent_call")]
    #[test_case(Some("local-receipt"), "local-receipt"; "explicit_local_root_is_preserved")]
    fn child_receipt_root_is_independent_of_cross_agent_display_root(
        local: Option<&str>,
        expected: &str,
    ) {
        const DISPLAY_ROOT: &str = "parent-agent-task";
        let mut ctx = stub_ctx(&AgentMode::Build);
        ctx.tool_use_id = Some("local-batch".into());
        ctx.root_tool_use_id = Some(DISPLAY_ROOT.into());
        ctx.local_root_tool_use_id = local.map(str::to_owned);
        let child = child_context(&ctx, 0);
        assert_eq!(child.local_root_tool_use_id.as_deref(), Some(expected));
        assert_eq!(child.root_tool_use_id.as_deref(), Some(DISPLAY_ROOT));
    }

    /// A tool that only returns once every sibling has reached the same
    /// barrier. Nothing here times out: overlap is proven by the run
    /// finishing at all, since a sequential dispatch would park forever.
    struct BarrierTool {
        name: &'static str,
        gate: Arc<async_lock::Barrier>,
    }

    struct BarrierCall(Arc<async_lock::Barrier>);

    impl ToolInvocation for BarrierCall {
        fn start_header(&self) -> HeaderFuture {
            HeaderFuture::Ready(HeaderResult::plain(String::new()))
        }
        fn execute<'a>(self: Box<Self>, _ctx: &'a ToolContext) -> ExecFuture<'a> {
            Box::pin(async move {
                self.0.wait().await;
                ToolExecResult::from(Ok(ToolOutput::Plain(BODY.into())))
            })
        }
    }

    impl Tool for BarrierTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
            Cow::Borrowed("")
        }
        fn schema(&self) -> Value {
            json!({ "type": "object", "properties": {} })
        }
        fn parse(&self, _input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
            Ok(Box::new(BarrierCall(Arc::clone(&self.gate))))
        }
    }

    const HEADER_TOOL: &str = "header_tool";
    const HEADER_PATH: &str = "src/lib.rs";
    const EXPECT_SUMMARY: &str =
        "the roster shows the header the child introduced itself with, not a bare tool name";

    /// Derives its header from the input, so a summary that merely echoed the
    /// tool name could not pass.
    struct HeaderTool;

    struct HeaderCall(String);

    impl ToolInvocation for HeaderCall {
        fn start_header(&self) -> HeaderFuture {
            HeaderFuture::Ready(HeaderResult::plain(self.0.clone()))
        }
        fn execute<'a>(self: Box<Self>, _ctx: &'a ToolContext) -> ExecFuture<'a> {
            Box::pin(async move { ToolExecResult::from(Ok(ToolOutput::Plain(BODY.into()))) })
        }
    }

    impl Tool for HeaderTool {
        fn name(&self) -> &str {
            HEADER_TOOL
        }
        fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
            Cow::Borrowed("")
        }
        fn schema(&self) -> Value {
            json!({ "type": "object", "properties": {} })
        }
        fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
            Ok(Box::new(HeaderCall(
                input["path"].as_str().unwrap_or_default().to_owned(),
            )))
        }
    }

    /// `BatchToolEntry::summary` is documented as the child's header line and
    /// was left empty at every construction site, so the roster drew a bare
    /// `tool>` where a standalone card names what it acted on.
    #[test]
    fn a_child_carries_the_header_it_introduced_itself_with() {
        let registry = Arc::new(ToolRegistry::new());
        registry
            .register(
                Arc::new(HeaderTool),
                crate::tools::ToolSource::Native {
                    owner: super::super::OWNER.into(),
                    contract: HEADER_TOOL.into(),
                    trusted: true,
                },
            )
            .expect("registering a stub child");
        let mut ctx = stub_ctx(&AgentMode::Build);
        ctx.registry = Arc::clone(&registry);

        let result = smol::block_on(async {
            parsed(calls(json!([{ "tool": HEADER_TOOL, "path": HEADER_PATH }])))
                .unwrap()
                .execute(&ctx)
                .await
        });

        let Ok(ToolOutput::Batch { entries, .. }) = result.output else {
            panic!("expected a batch result");
        };
        assert_eq!(entries[0].summary, HEADER_PATH, "{EXPECT_SUMMARY}");
    }

    const EXPECT_PENDING_SUMMARY: &str = "a child settled without ever starting keeps the header the batch resolved from the call, \
         so the roster never draws a bare tense with nothing after it";

    /// A child is adopted from a speculative run still in permission gating,
    /// refused at the boundary, or swept by a cancelled turn, and then settles
    /// without ever publishing a start. Its row is whatever the batch built up
    /// front, which was empty, so a finished `shell` read as `Ran` and named
    /// no command at all.
    #[test]
    fn a_child_names_its_call_before_it_starts() {
        let registry = Arc::new(ToolRegistry::new());
        registry
            .register(
                Arc::new(HeaderTool),
                crate::tools::ToolSource::Native {
                    owner: super::super::OWNER.into(),
                    contract: HEADER_TOOL.into(),
                    trusted: true,
                },
            )
            .expect("registering a stub child");
        let mut ctx = stub_ctx(&AgentMode::Build);
        ctx.registry = registry;
        let child = normalized(json!({ "tool": HEADER_TOOL, "path": HEADER_PATH })).unwrap();

        let entry = child.pending_entry(&ctx);

        assert_eq!(entry.summary, HEADER_PATH, "{EXPECT_PENDING_SUMMARY}");
        assert_eq!(
            entry.raw_input,
            Some(json!({ "path": HEADER_PATH })),
            "{EXPECT_PENDING_SUMMARY}"
        );
    }

    /// The other half: a start that arrived without a header must not erase
    /// the one the call itself gave.
    #[test]
    fn an_adopted_row_keeps_the_header_it_already_had() {
        let mut row = BatchToolEntry {
            summary: HEADER_PATH.to_owned(),
            raw_input: Some(json!({ "path": HEADER_PATH })),
            ..entry(HEADER_TOOL, BatchToolStatus::Pending, BODY)
        };

        adopt_row(&mut row, entry(HEADER_TOOL, BatchToolStatus::Running, BODY));

        assert_eq!(row.status, BatchToolStatus::Running, "{EXPECT_SUMMARY}");
        assert_eq!(row.summary, HEADER_PATH, "{EXPECT_PENDING_SUMMARY}");
        assert_eq!(
            row.raw_input,
            Some(json!({ "path": HEADER_PATH })),
            "{EXPECT_PENDING_SUMMARY}"
        );
    }

    /// A child's permission prompt carries only its id, so reading the index
    /// back is how the prompt finds its row, and must undo exactly what
    /// minting the id did.
    #[test_case(&child_tool_use_id(Some(BATCH_ID), 0), Some(0) ; "the_first_child")]
    #[test_case(
        &child_tool_use_id(Some(BATCH_ID), MAX_BATCH_SIZE - 1),
        Some(MAX_BATCH_SIZE - 1)
        ; "the_last_child"
    )]
    #[test_case(BATCH_ID, None ; "the_batch_itself")]
    #[test_case("batch-10:0", None ; "a_batch_whose_id_extends_this_one")]
    #[test_case("batch-1:x", None ; "a_suffix_that_is_no_index")]
    fn a_child_id_reads_back_as_its_index(id: &str, expected: Option<usize>) {
        assert_eq!(child_index(BATCH_ID, id), expected);
    }

    const MODEL_SUFFIX: &str = "<task_metadata>\ntask_id: task-1\n</task_metadata>";
    const EXPECT_MODEL_ONLY: &str = "guidance a child addressed to the model belongs in the answer the model reads, never in \
         the annotation the card draws beside the child's header";

    #[test]
    fn typed_task_receipt_retains_model_suffix_in_batch_and_snapshot() {
        let card = serde_json::from_value(json!({
            "task_id": "task-1", "invocation_id": "internal-invocation",
            "call_id": "batch-1:0", "root_call_id": BATCH_ID,
            "label": BODY, "state": "queued", "background": true, "mode": "build",
            "generation": 1, "created_at": 1, "updated_at": 1
        }))
        .unwrap();
        let receipt = super::super::task::receipt(card);
        let mut done = ToolDoneEvent::error(READ.into(), BODY);
        done.is_error = false;
        done.output = receipt.output.unwrap();
        done.model_suffix = receipt.model_suffix;
        let mut row = entry("task", BatchToolStatus::Pending, BODY);
        settle_entry(&mut row, &done);
        let restored: BatchToolEntry =
            serde_json::from_value(serde_json::to_value(&row).unwrap()).unwrap();
        assert!(matches!(restored.output, Some(ToolOutput::Tasks(_))));
        assert_eq!(restored.model_suffix, row.model_suffix);
        let model = render_llm(&[restored]);
        assert!(model.contains("<task_metadata>"));
        assert!(model.contains("Reports and the final outcome will arrive automatically"));
        assert!(!model.contains("internal-invocation"));
        let visible = row.output.unwrap().as_display_text();
        assert!(!visible.contains("<task_metadata>"));
        assert!(!visible.contains("internal-invocation"));
        assert!(!visible.contains("Reports and the final outcome will arrive automatically"));
    }

    /// `task` hands back a `<task_metadata>` block so the model can resume the
    /// subagent. Folded into the annotation it reached the model and the
    /// child's row alike, and the card drew the raw block after the header.
    #[test]
    fn model_only_guidance_stays_out_of_the_row() {
        let mut row = entry(READ, BatchToolStatus::Pending, BODY);
        let done = ToolDoneEvent::error(READ.to_owned(), BODY)
            .with_model_suffix(Some(MODEL_SUFFIX.to_owned()));

        settle_entry(&mut row, &done);

        assert_eq!(row.annotation, None, "{EXPECT_MODEL_ONLY}");
        assert_eq!(
            row.model_suffix.as_deref(),
            Some(MODEL_SUFFIX),
            "{EXPECT_MODEL_ONLY}"
        );
        assert!(
            render_llm(&[row]).contains(MODEL_SUFFIX),
            "{EXPECT_MODEL_ONLY}"
        );
    }

    const HEADER_TOOL_WIRE: &str = "mcp_Header_tool";
    const EXPECT_RESOLVED_NAME: &str =
        "the roster names the tool that ran, not the alias it was called by";

    /// Anthropic OAuth renames every tool on the wire, so the model asks for
    /// `mcp_Header_tool` and the registry only knows `header_tool`. Dispatch
    /// resolved the alias to find the tool but the roster kept the model's
    /// string, which left the transcript naming a tool that does not exist and
    /// handed that same name back to the model in the batch's own answer.
    #[test]
    fn a_child_is_named_by_the_tool_that_ran() {
        let registry = Arc::new(ToolRegistry::new());
        registry
            .register(
                Arc::new(HeaderTool),
                crate::tools::ToolSource::Native {
                    owner: super::super::OWNER.into(),
                    contract: HEADER_TOOL.into(),
                    trusted: true,
                },
            )
            .expect("registering a stub child");
        let mut ctx = stub_ctx(&AgentMode::Build);
        ctx.registry = Arc::clone(&registry);
        ctx.tool_name_aliases = Some(Arc::new(HashMap::from([(
            HEADER_TOOL_WIRE.to_owned(),
            HEADER_TOOL.to_owned(),
        )])));

        let result = smol::block_on(async {
            parsed(calls(
                json!([{ "tool": HEADER_TOOL_WIRE, "path": HEADER_PATH }]),
            ))
            .unwrap()
            .execute(&ctx)
            .await
        });

        let Ok(ToolOutput::Batch { entries, text }) = result.output else {
            panic!("expected a batch result");
        };
        assert_eq!(entries[0].tool, HEADER_TOOL, "{EXPECT_RESOLVED_NAME}");
        assert!(
            !text.contains(HEADER_TOOL_WIRE),
            "{EXPECT_RESOLVED_NAME}: {text:?}"
        );
    }

    const EXPECT_STAMPED_EFFECT: &str =
        "a child carries the effect it ran under, so the card folds it by the standalone rule";

    /// The roster is all the transcript has: a child has no card of its own to
    /// ask the registry about, and a restored session may be read long after
    /// the tool is gone. Taken as the child starts, for the same reason its
    /// name is.
    #[test]
    fn a_child_carries_the_effect_it_ran_under() {
        let registry = Arc::new(ToolRegistry::new());
        registry
            .register_audited(
                Arc::new(HeaderTool),
                crate::tools::ToolSource::Native {
                    owner: super::super::OWNER.into(),
                    contract: HEADER_TOOL.into(),
                    trusted: true,
                },
                ToolEffect::Mutating,
            )
            .expect("registering a stub child");
        let mut ctx = stub_ctx(&AgentMode::Build);
        ctx.registry = Arc::clone(&registry);

        let result = smol::block_on(async {
            parsed(calls(json!([{ "tool": HEADER_TOOL, "path": HEADER_PATH }])))
                .unwrap()
                .execute(&ctx)
                .await
        });

        let Ok(ToolOutput::Batch { entries, .. }) = result.output else {
            panic!("expected a batch result");
        };
        assert_eq!(
            entries[0].effect,
            ToolEffect::Mutating,
            "{EXPECT_STAMPED_EFFECT}"
        );
    }

    const EXPECT_RUNNING_SUMMARY: &str =
        "a child names what it is acting on while it runs, not only once it is done";

    /// The header was taken from the value `run_capturing` returns, which only
    /// arrives once the child has finished. Every row in a running batch was a
    /// bare `tool>` for exactly as long as the batch was worth watching.
    #[test]
    fn a_running_child_already_carries_its_header() {
        let registry = Arc::new(ToolRegistry::new());
        registry
            .register(
                Arc::new(HeaderTool),
                crate::tools::ToolSource::Native {
                    owner: super::super::OWNER.into(),
                    contract: HEADER_TOOL.into(),
                    trusted: true,
                },
            )
            .expect("registering a stub child");
        let (tx, rx) = flume::unbounded::<crate::Envelope>();
        let mut ctx = stub_ctx_with(
            &AgentMode::Build,
            Some(&crate::EventSender::new(tx, 0)),
            Some(BATCH_ID),
        );
        ctx.registry = Arc::clone(&registry);

        smol::block_on(async {
            parsed(calls(json!([{ "tool": HEADER_TOOL, "path": HEADER_PATH }])))
                .unwrap()
                .execute(&ctx)
                .await
        });

        let running = rx
            .drain()
            .filter_map(|envelope| match envelope.event {
                AgentEvent::BatchProgress(progress) => Some(progress.entry),
                _ => None,
            })
            .find(|entry| entry.status == BatchToolStatus::Running)
            .expect("a child reports that it started");
        assert_eq!(running.summary, HEADER_PATH, "{EXPECT_RUNNING_SUMMARY}");
    }

    /// Two children that can only both finish if they ran at the same time, so
    /// a sequential dispatch would deadlock rather than merely run slowly.
    #[test]
    fn children_really_do_overlap() {
        let registry = Arc::new(ToolRegistry::new());
        let gate = Arc::new(async_lock::Barrier::new(2));
        for name in [PARK, RELEASE] {
            registry
                .register(
                    Arc::new(BarrierTool {
                        name,
                        gate: Arc::clone(&gate),
                    }),
                    crate::tools::ToolSource::Native {
                        owner: super::super::OWNER.into(),
                        contract: name.into(),
                        trusted: true,
                    },
                )
                .expect("registering a stub child");
        }
        let mut ctx = stub_ctx(&AgentMode::Build);
        ctx.registry = Arc::clone(&registry);

        let result = smol::block_on(async {
            parsed(calls(
                json!([{ "tool": PARK, "x": 1 }, { "tool": RELEASE, "x": 1 }]),
            ))
            .unwrap()
            .execute(&ctx)
            .await
        });

        let Ok(ToolOutput::Batch { entries, .. }) = result.output else {
            panic!("expected a batch result");
        };
        assert!(
            entries.iter().all(|e| e.status == BatchToolStatus::Success),
            "both children completed, so both were in flight together"
        );
    }

    const MUTATOR: &str = "mutator";
    const MARK_FIELD: &str = "mark";
    const FIRST_MARK: &str = "first";
    const SECOND_MARK: &str = "second";
    const SEED: &str = "seed\n";
    const EXPECT_SERIALIZED: &str = "children mutating one file run one at a time";
    const EXPECT_NO_STALE: &str =
        "a sibling's write is not a stale read: the tracker moves under the same guard";
    const EXPECT_BOTH_APPLIED: &str =
        "both edits survive, so neither read-modify-write clobbered the other";

    #[derive(Default)]
    struct Gauge {
        inside: AtomicUsize,
        peak: AtomicUsize,
    }

    /// Replays what a real mutating tool does between dispatch's guards: check
    /// the tracker, read-modify-write, then record the new mtime. The yields
    /// widen the window an unguarded sibling used to slip into.
    struct MutatorTool {
        gauge: Arc<Gauge>,
        path: PathBuf,
    }

    struct MutatorCall {
        gauge: Arc<Gauge>,
        path: PathBuf,
        mark: String,
    }

    impl ToolInvocation for MutatorCall {
        fn start_header(&self) -> HeaderFuture {
            HeaderFuture::Ready(HeaderResult::plain(self.mark.clone()))
        }
        fn mutation_targets(&self, _ctx: &ToolContext) -> Vec<PathBuf> {
            vec![self.path.clone()]
        }
        fn execute<'a>(self: Box<Self>, ctx: &'a ToolContext) -> ExecFuture<'a> {
            Box::pin(async move {
                let depth = self.gauge.inside.fetch_add(1, Ordering::SeqCst) + 1;
                self.gauge.peak.fetch_max(depth, Ordering::SeqCst);
                let outcome = match ctx.file_tracker.check_before_edit(&self.path) {
                    Ok(()) => {
                        future::yield_now().await;
                        let mut body = std::fs::read_to_string(&self.path).unwrap_or_default();
                        body.push_str(&self.mark);
                        std::fs::write(&self.path, body).expect("stub mutator writes");
                        future::yield_now().await;
                        ctx.file_tracker.record_read(&self.path);
                        Ok(ToolOutput::Plain(BODY.into()))
                    }
                    Err(error) => Err(error),
                };
                self.gauge.inside.fetch_sub(1, Ordering::SeqCst);
                ToolExecResult::from(outcome)
            })
        }
    }

    impl Tool for MutatorTool {
        fn name(&self) -> &str {
            MUTATOR
        }
        fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
            Cow::Borrowed("")
        }
        fn schema(&self) -> Value {
            json!({ "type": "object", "properties": {} })
        }
        fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
            Ok(Box::new(MutatorCall {
                gauge: Arc::clone(&self.gauge),
                path: self.path.clone(),
                mark: input[MARK_FIELD].as_str().unwrap_or_default().to_owned(),
            }))
        }
    }

    /// The reported bug: two `file_edit` children on one file, where the loser
    /// either failed a stale check the tracker had not caught up to or silently
    /// dropped the other's edit.
    #[test]
    fn two_children_mutating_one_file_are_serialized() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("contended.rs");
        std::fs::write(&path, SEED).expect("seeding the contended file");

        let gauge = Arc::new(Gauge::default());
        let registry = Arc::new(ToolRegistry::new());
        registry
            .register(
                Arc::new(MutatorTool {
                    gauge: Arc::clone(&gauge),
                    path: path.clone(),
                }),
                crate::tools::ToolSource::Native {
                    owner: super::super::OWNER.into(),
                    contract: MUTATOR.into(),
                    trusted: true,
                },
            )
            .expect("registering a stub child");
        let mut ctx = stub_ctx(&AgentMode::Build);
        ctx.registry = Arc::clone(&registry);
        // Without a recorded read the stale check is a no-op, and the test
        // would prove nothing about the window it is pinning.
        ctx.file_tracker.record_read(&path);

        let result = smol::block_on(async {
            parsed(calls(json!([
                { "tool": MUTATOR, MARK_FIELD: FIRST_MARK },
                { "tool": MUTATOR, MARK_FIELD: SECOND_MARK },
            ])))
            .unwrap()
            .execute(&ctx)
            .await
        });

        let Ok(ToolOutput::Batch { entries, text }) = result.output else {
            panic!("expected a batch result");
        };
        assert_eq!(gauge.peak.load(Ordering::SeqCst), 1, "{EXPECT_SERIALIZED}");
        assert!(!text.contains(STALE_READ_MSG), "{EXPECT_NO_STALE}: {text}");
        assert!(
            entries.iter().all(|e| e.status == BatchToolStatus::Success),
            "{EXPECT_NO_STALE}: {entries:?}"
        );
        let body = std::fs::read_to_string(&path).expect("reading the contended file");
        assert_eq!(
            body,
            format!("{SEED}{FIRST_MARK}{SECOND_MARK}"),
            "{EXPECT_BOTH_APPLIED}"
        );
    }

    const PREPARING: &str = "preparing";
    const KEY_FIELD: &str = "key";
    const REMOTE_FILE: &str = "/workspace/contended.rs";
    const OTHER_REMOTE_FILE: &str = "/workspace/other.rs";

    /// Stands in for a remote write, whose preparation fixes the version of
    /// the file it will replace. The gauge counts calls between the start of
    /// preparation and the end of execution, the span its key has to cover.
    struct PreparingTool {
        gauge: Arc<Gauge>,
    }

    struct PreparingCall {
        gauge: Arc<Gauge>,
        key: Option<String>,
    }

    impl ToolInvocation for PreparingCall {
        fn start_header(&self) -> HeaderFuture {
            HeaderFuture::Ready(HeaderResult::plain(PREPARING.to_owned()))
        }
        fn preflight_write_keys(&self, _ctx: &ToolContext) -> Vec<LockKey> {
            self.key.iter().cloned().map(LockKey::Remote).collect()
        }
        fn preflight<'a>(
            &'a self,
            _ctx: &'a ToolContext,
        ) -> BoxFuture<'a, Result<Option<PermissionIntent>, String>> {
            Box::pin(async move {
                let depth = self.gauge.inside.fetch_add(1, Ordering::SeqCst) + 1;
                self.gauge.peak.fetch_max(depth, Ordering::SeqCst);
                future::yield_now().await;
                Ok(None)
            })
        }
        fn execute<'a>(self: Box<Self>, _ctx: &'a ToolContext) -> ExecFuture<'a> {
            Box::pin(async move {
                future::yield_now().await;
                self.gauge.inside.fetch_sub(1, Ordering::SeqCst);
                ToolExecResult::from(Ok::<_, String>(ToolOutput::Plain(BODY.into())))
            })
        }
    }

    impl Tool for PreparingTool {
        fn name(&self) -> &str {
            PREPARING
        }
        fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
            Cow::Borrowed("")
        }
        fn schema(&self) -> Value {
            json!({ "type": "object", "properties": {} })
        }
        fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
            Ok(Box::new(PreparingCall {
                gauge: Arc::clone(&self.gauge),
                key: input[KEY_FIELD].as_str().map(str::to_owned),
            }))
        }
    }

    /// A remote write's key is held from before its preparation to the end of
    /// its execution, so a second write to that file prepares only once the
    /// first has published. Other files, and calls naming none, such as a
    /// shell command, run alongside it.
    #[test_case(Some(REMOTE_FILE), Some(REMOTE_FILE), 1 ; "one_remote_file_is_serialized")]
    #[test_case(Some(REMOTE_FILE), Some(OTHER_REMOTE_FILE), 2 ; "different_remote_files_overlap")]
    #[test_case(Some(REMOTE_FILE), None, 2 ; "a_call_naming_no_file_overlaps_a_writer")]
    fn remote_write_keys_cover_preparation_through_execution(
        first: Option<&str>,
        second: Option<&str>,
        expected_peak: usize,
    ) {
        let gauge = Arc::new(Gauge::default());
        let registry = Arc::new(ToolRegistry::new());
        registry
            .register(
                Arc::new(PreparingTool {
                    gauge: Arc::clone(&gauge),
                }),
                ToolSource::Native {
                    owner: super::super::OWNER.into(),
                    contract: PREPARING.into(),
                    trusted: true,
                },
            )
            .expect("registering a stub child");
        let mut ctx = stub_ctx(&AgentMode::Build);
        ctx.registry = Arc::clone(&registry);

        let result = smol::block_on(async {
            parsed(calls(json!([
                { "tool": PREPARING, KEY_FIELD: first },
                { "tool": PREPARING, KEY_FIELD: second },
            ])))
            .unwrap()
            .execute(&ctx)
            .await
        });

        let Ok(ToolOutput::Batch { entries, .. }) = result.output else {
            panic!("expected a batch result");
        };
        assert!(
            entries.iter().all(|e| e.status == BatchToolStatus::Success),
            "{entries:?}"
        );
        assert_eq!(gauge.peak.load(Ordering::SeqCst), expected_peak);
    }
}
