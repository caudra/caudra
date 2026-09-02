//! `batch`: run independent tool calls concurrently.
//!
//! `entries` is the single source of truth. The model's answer, the live view,
//! and the restored transcript are all pure functions of it, so the three can
//! never disagree. Children dispatch through the same
//! [`tool_dispatch::run`] every other call goes through, silenced so the
//! transcript shows one batch rather than N loose tools; the batch republishes
//! each child's state as it changes.

use std::borrow::Cow;
use std::sync::{Arc, LazyLock, Mutex};

use serde_json::{Map, Value};

use crate::agent::tool_dispatch::{self, Emit};
use crate::tools::registry::{
    ExecFuture, HeaderFuture, HeaderResult, ParseError, Tool, ToolExecResult, ToolInvocation,
};
use crate::tools::schema::{ParamSchema, Property, to_json_schema, validate};
use crate::tools::{DescriptionContext, ToolAudience, ToolContext};
use crate::types::{BatchProgressEvent, BatchToolEntry, BatchToolStatus, ToolOutput};
use crate::{AgentEvent, task_set::TaskSet};

pub const MAX_BATCH_SIZE: usize = 25;

pub const DESCRIPTION: &str = "Executes multiple independent tool calls concurrently to reduce round-trips.

ALWAYS USE THE BATCH TOOL WHEN YOU HAVE MULTIPLE INDEPENDENT TOOL CALLS. This dramatically improves performance.

Rules:
- 1-25 tool calls per batch
- All calls run in parallel; order NOT guaranteed
- Partial failures do not stop other calls
- Do NOT nest batch inside batch
- Do NOT use for dependent operations or when filtering results (use code_execution)";

const SECTION_PREFIX: &str = "## ";
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
                { "tool": "index", "parameters": { "path": "/project/index.ts" } },
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
        HeaderFuture::Ready(HeaderResult::plain(format!(
            "{} tools",
            self.children.len()
        )))
    }

    /// Publishing the roster up front is what lets progress events name a
    /// child by index instead of resending the whole batch each time.
    fn start_output(&self, _ctx: &ToolContext) -> Option<ToolOutput> {
        Some(ToolOutput::Batch {
            entries: self.children.iter().map(Child::pending_entry).collect(),
            text: String::new(),
        })
    }

    fn execute<'a>(self: Box<Self>, ctx: &'a ToolContext) -> ExecFuture<'a> {
        Box::pin(async move { self.run(ctx).await })
    }
}

impl Child {
    fn pending_entry(&self) -> BatchToolEntry {
        match self.rejection {
            Some(reason) => BatchToolEntry {
                tool: self.tool.clone(),
                summary: String::new(),
                status: BatchToolStatus::Error,
                input: None,
                output: Some(ToolOutput::Plain(reason.into())),
                annotation: None,
            },
            None => BatchToolEntry {
                tool: self.tool.clone(),
                summary: String::new(),
                status: BatchToolStatus::Pending,
                input: None,
                output: None,
                annotation: None,
            },
        }
    }
}

impl BatchCall {
    async fn run(self: Box<Self>, ctx: &ToolContext) -> ToolExecResult {
        let entries: Vec<BatchToolEntry> = self.children.iter().map(Child::pending_entry).collect();
        let entries = Arc::new(Mutex::new(entries));
        let mut set = TaskSet::new();
        for (index, child) in self.children.into_iter().enumerate() {
            if child.rejection.is_some() {
                continue;
            }
            // The registry, MCP session, and context are all shared handles,
            // so a child task borrows nothing from this frame.
            let (registry, mcp, ctx) = (
                Arc::clone(&ctx.registry),
                ctx.mcp.clone(),
                child_context(ctx, index),
            );
            let entries = Arc::clone(&entries);
            set.spawn(async move {
                publish(&entries, index, &ctx, |entry| {
                    entry.status = BatchToolStatus::Running;
                });
                let done = tool_dispatch::run(
                    &registry,
                    mcp.as_ref(),
                    ctx.tool_use_id.clone().unwrap_or_default(),
                    &child.tool,
                    &child.params,
                    &ctx,
                    Emit::Silent,
                )
                .await;
                publish(&entries, index, &ctx, |entry| {
                    entry.status = if done.is_error {
                        BatchToolStatus::Error
                    } else {
                        BatchToolStatus::Success
                    };
                    entry.annotation = done.annotation.clone();
                    entry.output = Some(done.output.clone());
                });
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
    child.tool_use_id = ctx
        .tool_use_id
        .as_ref()
        .map(|id| format!("{id}:{index}"))
        .or(Some(index.to_string()));
    child.live_sink = None;
    child
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
    let Some(id) = batch_id(ctx) else { return };
    let _ = ctx
        .event_tx
        .send(AgentEvent::BatchProgress(Box::new(BatchProgressEvent {
            id,
            index,
            entry,
        })));
}

/// The child ids are the batch's own with an index appended, so the batch row
/// is recovered by dropping the suffix `child_context` added.
fn batch_id(ctx: &ToolContext) -> Option<String> {
    let id = ctx.tool_use_id.as_ref()?;
    Some(
        id.rsplit_once(':')
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
    use crate::tools::registry::ToolRegistry;
    use crate::tools::test_support::stub_ctx;
    use serde_json::json;
    use test_case::test_case;

    const READ: &str = "read";
    const GREP: &str = "grep";
    const PATTERN: &str = "src/**/*.rs";
    const BODY: &str = "file contents";
    const FAILURE: &str = "no such file";
    const PARK: &str = "park";
    const RELEASE: &str = "release";
    const DEADLINE_REASON: &str = crate::tools::DEADLINE_EXCEEDED;

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
            tool: tool.into(),
            summary: String::new(),
            status,
            input: None,
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
        assert_eq!(child.pending_entry().status, BatchToolStatus::Error);
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
}
