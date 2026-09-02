//! Tests the batch plugin's policy end-to-end: real plugin source, real
//! `caudra.async.gather`, with tool dispatch replaced by a scriptable Lua stub.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use caudra_agent::cancel::CancelToken;
use caudra_agent::tools::test_support::{stub_ctx, stub_ctx_with};
use caudra_agent::tools::{
    DescriptionContext, ExecFuture, HeaderFuture, HeaderResult, ParseError, Tool, ToolAudience,
    ToolContext, ToolExecResult, ToolInvocation, ToolRegistry, ToolSource,
};
use caudra_agent::{
    AgentEvent, AgentMode, BufferSnapshot, Envelope, EventSender, SpanStyle, ToolOutput,
};
use caudra_config::ToolOutputLines;
use caudra_lua::{KILL_GRACE, PluginHost};
use serde_json::{Value, json};

const BATCH_PLUGIN_SRC: &str = include_str!("../../plugins/batch/init.lua");

// Mirrors of the plugin's format contracts.
const MAX_BATCH_SIZE: usize = 25;
/// The batch header row, which reveals every collapsed child at once.
const REVEAL_CHILDREN: usize = 0;
const ERROR_PREFIX: &str = "[ERROR] ";
const EMPTY_ERROR: &str = "provide at least one tool call";
const NESTED_ERROR: &str = "cannot nest batch inside batch";
const DISCARDED_ERROR: &str = "maximum of 25 tools per batch";
const CANCELLED_ERROR: &str = "cancelled";
const SUMMARY_ALL_OK_FMT: &str = "All {} tools executed successfully.";
const SUMMARY_MIXED_FMT: &str = "Executed {}/{} successfully. {} failed.";

const BATCH_TOOL: &str = "batch";
const PROBE_TOOL: &str = "probe";
const OK_TOOL: &str = "ok";
const PARKJOB_TOOL: &str = "parkjob";
/// A second parked child, so one of the two can carry its own restore.
const PARKJOB_HL_TOOL: &str = "parkjob_hl";
const CMD_TOOL: &str = "cmd";
const BOOM_TOOL: &str = "boom";
const BOOM_ERR: &str = "stub tool exploded";
const PARTIAL_TOOL: &str = "partial";
const PARTIAL_ERR: &str = "half the output [cancelled by user; output above is partial]";
const CHILD_USAGE: &str = "12.3k↑ 456↓ $0.123";
const CHILD_ACTIVITY: &str = "cargo nextest run";
/// The relay formats the tally; batch only places it, so the stub feeds a
/// fixed one and keeps the clock out of the assertion.
const CHILD_TALLY: &str = "3 tools · 1.2s";
const ACTIVITY_PREFIX: &str = "  ├ ";
const ANNOTATION_SEP: &str = " · ";
const DIM_STYLE: &str = "dim";
/// Wildly generous vs the expected kill (one poll plus one
/// [`KILL_GRACE`]); only a watchdog that never fires gets here.
const RUNAWAY_BUDGET_GRACES: u32 = 20;
/// How long a `parkjob` child sits in an await that ignores the token, like a
/// child tool mid-request. It only has to outlast the cancel that follows the
/// first paint, and stay well inside the host's 5s abandon window.
const PARK_SECS: f32 = 2.0;
const PAINT_POLL_INTERVAL: Duration = Duration::from_millis(5);
/// Deliberately generous: no assertion here is a race against the clock,
/// so a slow machine costs time only when a test is already failing.
const PAINT_TIMEOUT: Duration = Duration::from_secs(5);
/// Only a batch that never settles waits this long.
const BATCH_RESULT_TIMEOUT: Duration = Duration::from_secs(10);
const SPINNER_STYLE: &str = "spinner";
const ERROR_STYLE: &str = "tool_error";
const SUCCESS_STYLE: &str = "tool_success";

/// `caudra.agent.call_tool` is stubbed; `caudra.async.gather` and the semaphore
/// stay real, so the park/release pair proves children genuinely overlap.
const STUB_PRELUDE: &str = r#"
recorder = { calls = {}, header_calls = 0 }
local sem = caudra.async.semaphore(1)
local held = sem:acquire()

caudra.agent.call_tool = function(ctx, name, input, opts)
  recorder.calls[#recorder.calls + 1] = { tool = name, params = input }
  if name == "ok" then
    return "ok:" .. tostring(input.tag or "?")
  elseif name == "annotated" then
    if opts and opts.on_annotation then
      opts.on_annotation("model-x")
      opts.on_annotation("5 lines")
    end
    return "annotated_done"
  elseif name == "used" then
    if opts and opts.on_annotation then
      opts.on_annotation("model-x")
    end
    if opts and opts.on_usage then
      opts.on_usage("@CHILD_USAGE@")
    end
    return "used_done"
  elseif name == "busy" then
    -- Reports, then parks like a real subagent mid-turn, so the paint that
    -- carries the progress is observable before the child settles.
    if opts and opts.on_progress then
      opts.on_progress("shell", "@CHILD_ACTIVITY@", "@CHILD_TALLY@")
    end
    caudra.fn.jobwait(caudra.fn.jobstart("sleep @PARK_SECS@"))
    return "busy_done"
  elseif name == "park" then
    -- Deadlocks unless a sibling runs concurrently and releases.
    local p = sem:acquire()
    p:release()
    return "parked_done"
  elseif name == "release" then
    held:release()
    return "released_done"
  elseif name == "boom" then
    return nil, "@BOOM_ERR@"
  elseif name:find("^parkjob") then
    -- Parks in an await that ignores the token, like a real child tool
    -- waiting on a request. Any `parkjob*` name parks, so a test can give
    -- one of them its own restore.
    caudra.fn.jobwait(caudra.fn.jobstart("sleep @PARK_SECS@"))
    return "parkjob_done"
  elseif name == "partial" then
    -- Parks past the sweep, then comes back with more than "cancelled".
    caudra.fn.jobwait(caudra.fn.jobstart("sleep @PARK_SECS@"))
    return nil, "@PARTIAL_ERR@"
  elseif name == "spin" then
    -- Never yields, so nothing but the watchdog can end it.
    while true do end
  end
  return nil, "unknown tool: " .. name
end

caudra.api.register_tool({
  name = "probe",
  description = "recorder snapshot",
  schema = { type = "object", properties = {}, additionalProperties = false },
  audiences = { "main" },
  handler = function(input, ctx)
    return (caudra.json.encode(recorder))
  end,
})

caudra.api.register_tool({
  name = "hdrtool",
  description = "child with custom header",
  schema = { type = "object", properties = {} },
  audiences = { "main" },
  header = function(input)
    recorder.header_calls = recorder.header_calls + 1
    return "H:" .. tostring(input.x)
  end,
  handler = function() return "unused" end,
})

caudra.api.register_tool({
  name = "badhdr",
  description = "child whose header throws",
  schema = { type = "object", properties = {} },
  audiences = { "main" },
  header = function(input)
    error("header kaboom")
  end,
  handler = function() return "unused" end,
})

caudra.api.register_tool({
  name = "badrestore",
  description = "child whose restore throws",
  schema = { type = "object", properties = {} },
  audiences = { "main" },
  restore = function() error("restore kaboom") end,
  handler = function() return "unused" end,
})

local ToolView = require("caudra.tool_view")
caudra.api.register_tool({
  name = "viewer",
  description = "child with a truncating ToolView restore",
  schema = { type = "object", properties = {} },
  audiences = { "main" },
  restore = function(input, output, is_error, rctx)
    return ToolView.restore(output, { max_lines = 2, keep = "head" })
  end,
  handler = function() return "unused" end,
})
"#;

fn load_batch_host() -> (Arc<ToolRegistry>, PluginHost) {
    load_batch_host_with("")
}

/// `child_src` registers extra child tools next to the stub ones, so a
/// test can give a child its own header/restore behaviour.
fn stub_prelude() -> String {
    STUB_PRELUDE
        .replace("@BOOM_ERR@", BOOM_ERR)
        .replace("@CHILD_ACTIVITY@", CHILD_ACTIVITY)
        .replace("@CHILD_TALLY@", CHILD_TALLY)
        .replace("@CHILD_USAGE@", CHILD_USAGE)
        .replace("@PARTIAL_ERR@", PARTIAL_ERR)
        .replace("@PARK_SECS@", &PARK_SECS.to_string())
}

fn load_batch_host_with(child_src: &str) -> (Arc<ToolRegistry>, PluginHost) {
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let prelude = stub_prelude();
    host.load_source(
        "batch_policy",
        &format!("{prelude}\n{child_src}\n{BATCH_PLUGIN_SRC}"),
    )
    .unwrap();
    (reg, host)
}

fn output_text(out: ToolOutput) -> String {
    match out {
        ToolOutput::Plain(s) | ToolOutput::Markdown(s) => s.text,
        other => panic!("unexpected output: {other:?}"),
    }
}

fn exec_with_ctx(
    reg: &ToolRegistry,
    name: &str,
    input: Value,
    ctx: &ToolContext,
) -> Result<ToolOutput, String> {
    let entry = reg
        .get(name)
        .unwrap_or_else(|| panic!("tool {name} not registered"));
    let inv = entry.tool.parse(&input).expect("parse failed");
    smol::block_on(async { inv.execute(ctx).await }).output
}

fn exec_tool(reg: &ToolRegistry, name: &str, input: Value) -> Result<String, String> {
    exec_with_ctx(reg, name, input, &stub_ctx(&AgentMode::Build)).map(output_text)
}

fn batch_input(tool_calls: Value) -> Value {
    json!({ "tool_calls": tool_calls })
}

/// Same as [`run_batch`], but the run's token is tripped before a single
/// child starts, standing in for esc pressed while the batch is in flight.
fn run_cancelled_batch(reg: &ToolRegistry, tool_calls: Value) -> Result<String, String> {
    let mut ctx = stub_ctx(&AgentMode::Build);
    let (trigger, token) = CancelToken::new();
    trigger.cancel();
    ctx.cancel = token;
    exec_with_ctx(reg, BATCH_TOOL, batch_input(tool_calls), &ctx).map(output_text)
}

fn run_batch(reg: &ToolRegistry, tool_calls: Value) -> Result<String, String> {
    exec_tool(reg, BATCH_TOOL, batch_input(tool_calls))
}

fn run_batch_state(reg: &ToolRegistry, tool_calls: Value) -> Value {
    exec_with_ctx(
        reg,
        BATCH_TOOL,
        batch_input(tool_calls),
        &stub_ctx(&AgentMode::Build),
    )
    .expect("batch failed")
    .state()
    .cloned()
    .expect("no state on batch output")
}

fn recorded_calls(reg: &ToolRegistry) -> Vec<Value> {
    let out = exec_tool(reg, PROBE_TOOL, json!({})).expect("probe failed");
    let snap: Value = serde_json::from_str(&out).expect("probe returned invalid json");
    snap["calls"].as_array().cloned().unwrap_or_default()
}

fn recorded_header_calls(reg: &ToolRegistry) -> u64 {
    let out = exec_tool(reg, PROBE_TOOL, json!({})).expect("probe failed");
    let snap: Value = serde_json::from_str(&out).expect("probe returned invalid json");
    snap["header_calls"].as_u64().unwrap_or_default()
}

fn section(tool: &str, body: &str) -> String {
    format!("## {tool}\n{body}\n\n")
}

fn summary_all_ok(total: usize) -> String {
    SUMMARY_ALL_OK_FMT.replacen("{}", &total.to_string(), 1)
}

fn summary_mixed(ok: usize, total: usize, failed: usize) -> String {
    SUMMARY_MIXED_FMT
        .replacen("{}", &ok.to_string(), 1)
        .replacen("{}", &total.to_string(), 1)
        .replacen("{}", &failed.to_string(), 1)
}

#[test]
fn all_success_exact_llm_output() {
    let (reg, _host) = load_batch_host();
    let out = run_batch(
        &reg,
        json!([
            { "tool": "ok", "parameters": { "tag": "a" } },
            { "tool": "ok", "parameters": { "tag": "b" } },
        ]),
    )
    .expect("batch failed");
    let expected = format!(
        "{}{}{}",
        section("ok", "ok:a"),
        section("ok", "ok:b"),
        summary_all_ok(2)
    );
    assert_eq!(out, expected);
}

#[test]
fn oauth_wire_names_dispatch_and_preserve_nested_guard() {
    let (reg, _host) = load_batch_host();
    let mut ctx = stub_ctx(&AgentMode::Build);
    ctx.tool_name_aliases = Some(Arc::new(HashMap::from([
        ("mcp_Ok".into(), "ok".into()),
        ("mcp_Batch".into(), BATCH_TOOL.into()),
    ])));

    let out = exec_with_ctx(
        &reg,
        BATCH_TOOL,
        batch_input(json!([
            { "tool": "mcp_Ok", "parameters": { "tag": "aliased" } },
            { "tool": "mcp_Batch", "parameters": { "tool_calls": [] } },
        ])),
        &ctx,
    )
    .map(output_text)
    .expect("batch failed");

    let expected = format!(
        "{}{}{}",
        section("ok", "ok:aliased"),
        section(BATCH_TOOL, &format!("{ERROR_PREFIX}{NESTED_ERROR}")),
        summary_mixed(1, 2, 1)
    );
    assert_eq!(out, expected);
    assert_eq!(recorded_calls(&reg).len(), 1);
}

#[test]
fn read_only_batch_does_not_run_unsafe_child_presentation_callbacks() {
    let mut research_ctx = stub_ctx(&AgentMode::Build);
    research_ctx.audience = ToolAudience::RESEARCH_SUB;
    for ctx in [
        stub_ctx(&AgentMode::ReadOnly),
        stub_ctx(&AgentMode::Plan("/tmp/plan.md".into())),
        research_ctx,
    ] {
        let (reg, _host) = load_batch_host();
        let input = batch_input(json!([{ "tool": "hdrtool", "parameters": { "x": "A" } }]));

        exec_with_ctx(&reg, BATCH_TOOL, input, &ctx).expect("batch failed");

        assert_eq!(recorded_header_calls(&reg), 0);
    }
}

#[test]
fn restore_does_not_run_header_for_an_errored_child() {
    let (reg, host) = load_batch_host();
    let input = batch_input(json!([{ "tool": "hdrtool", "parameters": { "x": "A" } }]));
    let output = format!(
        "{}{}",
        section("hdrtool", &format!("{ERROR_PREFIX}denied")),
        summary_mixed(0, 1, 1)
    );

    restore_snapshot_lines(
        &host,
        input,
        &output,
        Some(json!([{ "tool": "hdrtool", "status": "error", "output": "denied" }])),
    );

    assert_eq!(recorded_header_calls(&reg), 0);
}

#[test]
fn restore_does_not_run_replaced_child_presentation_contract() {
    let (reg, host) = load_batch_host();
    let calls = json!([{ "tool": "hdrtool", "parameters": { "x": "A" } }]);
    let input = batch_input(calls.clone());
    let state = run_batch_state(&reg, calls);
    let output = format!("{}{}", section("hdrtool", "unused"), summary_all_ok(1));
    let prelude = stub_prelude();
    host.load_source(
        "batch_policy",
        &format!("{prelude}\n{BATCH_PLUGIN_SRC}\n-- changed implementation"),
    )
    .unwrap();

    restore_snapshot_lines(&host, input, &output, Some(state));

    assert_eq!(recorded_header_calls(&reg), 0);
}

/// The grace is a budget, not a licence to hang: a child that never yields
/// still gets shot, so esc hands the session back instead of wedging it
/// until the outer backstop timeout. The kill lands inside the child, so
/// the batch survives and reports it cancelled like any other child.
#[test]
fn runaway_child_under_cancel_still_terminates() {
    let batch = start_batch(json!([{ "tool": "spin", "parameters": {} }]));
    assert_indicators(&batch.body, SPINNER_STYLE, 1, PAINT_TIMEOUT);

    batch.trigger.cancel();

    let outcome = batch
        .result
        .recv_timeout(KILL_GRACE * RUNAWAY_BUDGET_GRACES)
        .expect("a never-yielding child must be killed, not run forever");
    let expected = format!(
        "{}{}",
        section("spin", &format!("{ERROR_PREFIX}{CANCELLED_ERROR}")),
        summary_mixed(0, 1, 1)
    );
    assert_eq!(outcome, Ok(expected));
}

/// A batch running on its own thread, with what a cancel test needs to press
/// esc at a chosen moment: the buf it paints into, the trigger, and where its
/// result lands.
struct RunningBatch {
    trigger: caudra_agent::cancel::CancelTrigger,
    body: Arc<caudra_agent::SharedBuf>,
    result: flume::Receiver<Result<String, String>>,
    /// Kept alive so the handler's later events still have a receiver.
    _events: flume::Receiver<caudra_agent::Envelope>,
}

fn start_batch(tool_calls: Value) -> RunningBatch {
    start_batch_with(String::new(), tool_calls)
}

fn start_batch_with(child_src: String, tool_calls: Value) -> RunningBatch {
    let (event_tx, events) = flume::unbounded();
    let event_tx = EventSender::new(event_tx, 0);
    let (trigger, token) = CancelToken::new();
    let (result_tx, result) = flume::bounded(1);
    std::thread::spawn(move || {
        let (reg, _host) = load_batch_host_with(&child_src);
        let mut ctx = stub_ctx_with(&AgentMode::Build, Some(&event_tx), Some(BATCH_ID));
        ctx.cancel = token;
        let input = batch_input(tool_calls);
        result_tx
            .send(exec_with_ctx(&reg, BATCH_TOOL, input, &ctx).map(output_text))
            .ok();
    });
    RunningBatch {
        trigger,
        body: recv_live_buf(&events),
        result,
        _events: events,
    }
}

fn recv_live_buf(rx: &flume::Receiver<caudra_agent::Envelope>) -> Arc<caudra_agent::SharedBuf> {
    let deadline = Instant::now() + PAINT_TIMEOUT;
    while let Ok(env) = rx.recv_deadline(deadline) {
        if let AgentEvent::LiveToolBuf { id, body } = env.event
            && id == BATCH_ID
        {
            return body;
        }
    }
    panic!("batch must publish its live buf");
}

/// A child header opens with the indicator span the batch draws for its
/// status, so counting one style counts the children in that state.
fn wait_for_indicators(
    buf: &caudra_agent::SharedBuf,
    style: &str,
    want: usize,
    budget: Duration,
) -> bool {
    let wanted = SpanStyle::Named(style.to_owned());
    let deadline = Instant::now() + budget;
    loop {
        let got = buf
            .take()
            .lines
            .iter()
            .filter(|l| l.spans.first().is_some_and(|s| s.style == wanted))
            .count();
        if got == want {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(PAINT_POLL_INTERVAL);
    }
}

fn assert_indicators(buf: &caudra_agent::SharedBuf, style: &str, want: usize, budget: Duration) {
    assert!(
        wait_for_indicators(buf, style, want, budget),
        "expected {want} {style} children"
    );
}

/// The bug esc still had: the token reaches neither `call_tool` nor `gather`,
/// so a child parked in a request stayed drawn as running until the host
/// abandoned the handler seconds later. Only the cancel hook can repaint one,
/// so no budget here has to be tight: a sweep that waits for `gather` finds
/// both parked children back as plain successes, however long we wait.
///
/// That sweep runs outside the coroutine, where a child restore that awaits
/// raises, and one such body must not swallow the rest of the sweep. Children
/// that already finished keep their own status and output, and a child whose
/// own call lands after the sweep with more than "cancelled" trades the
/// placeholder for it.
#[test]
fn cancel_mid_flight_repaints_parked_children_and_spares_finished_ones() {
    let batch = start_batch_with(
        hl_child_src(PARKJOB_HL_TOOL),
        json!([
            { "tool": OK_TOOL, "parameters": { "tag": "a" } },
            { "tool": PARKJOB_TOOL, "parameters": {} },
            { "tool": PARKJOB_HL_TOOL, "parameters": {} },
            { "tool": PARTIAL_TOOL, "parameters": {} },
            { "tool": BOOM_TOOL, "parameters": {} },
        ]),
    );
    for (style, want) in [(SUCCESS_STYLE, 1), (ERROR_STYLE, 1), (SPINNER_STYLE, 3)] {
        assert_indicators(&batch.body, style, want, PAINT_TIMEOUT);
    }

    batch.trigger.cancel();

    assert_indicators(&batch.body, ERROR_STYLE, 4, PAINT_TIMEOUT);
    assert_indicators(&batch.body, SPINNER_STYLE, 0, Duration::ZERO);
    assert_indicators(&batch.body, SUCCESS_STYLE, 1, Duration::ZERO);

    let cancelled = format!("{ERROR_PREFIX}{CANCELLED_ERROR}");
    let expected = format!(
        "{}{}{}{}{}{}",
        section(OK_TOOL, "ok:a"),
        section(PARKJOB_TOOL, &cancelled),
        section(PARKJOB_HL_TOOL, &cancelled),
        section(PARTIAL_TOOL, &format!("{ERROR_PREFIX}{PARTIAL_ERR}")),
        section(BOOM_TOOL, &format!("{ERROR_PREFIX}{BOOM_ERR}")),
        summary_mixed(1, 5, 4)
    );
    let settled = batch
        .result
        .recv_timeout(BATCH_RESULT_TIMEOUT)
        .expect("batch must settle instead of hanging on its cancelled children");
    assert_eq!(settled, Err(expected));
}

/// Esc pressed before the batch is even dispatched: the handler still runs
/// (only `caudra.async.run` spawns are skipped on a cancelled token) and
/// `gather` runs every fun it was handed, so only the batch itself can stop a
/// swept child from executing anyway. A child born terminal keeps its own
/// error, so the two sweeps never settle the same child twice.
#[test]
fn cancelled_batch_dispatches_nothing_and_keeps_born_terminal_errors() {
    let (reg, _host) = load_batch_host();
    let out = run_cancelled_batch(
        &reg,
        json!([
            { "tool": BATCH_TOOL, "parameters": { "tool_calls": [] } },
            { "tool": OK_TOOL, "parameters": { "tag": "a" } },
        ]),
    )
    .expect_err("a cancelled batch is an error reply");
    let expected = format!(
        "{}{}{}",
        section(BATCH_TOOL, &format!("{ERROR_PREFIX}{NESTED_ERROR}")),
        section(OK_TOOL, &format!("{ERROR_PREFIX}{CANCELLED_ERROR}")),
        summary_mixed(0, 2, 2)
    );
    assert_eq!(out, expected);
    assert!(
        recorded_calls(&reg).is_empty(),
        "a batch cancelled before it starts must dispatch nothing"
    );
}

#[test]
fn flat_nested_and_merged_params_normalize_identically() {
    let (reg, _host) = load_batch_host();
    run_batch(
        &reg,
        json!([
            { "tool": "ok", "tag": "flat", "n": 1 },
            { "tool": "ok", "parameters": { "tag": "nested", "n": 1 } },
            { "tool": "ok", "parameters": { "tag": "merged" }, "n": 1 },
        ]),
    )
    .expect("batch failed");

    let mut calls = recorded_calls(&reg);
    assert_eq!(calls.len(), 3);
    calls.sort_by_key(|c| c["params"]["tag"].as_str().unwrap_or("").to_owned());
    assert_eq!(calls[0]["params"], json!({ "tag": "flat", "n": 1 }));
    assert_eq!(calls[1]["params"], json!({ "tag": "merged", "n": 1 }));
    assert_eq!(calls[2]["params"], json!({ "tag": "nested", "n": 1 }));
}

/// The batch plugin strips GPT's `functions.` prefix in Lua so child
/// lookups and the nested-batch guard key on the canonical name.
#[test]
fn functions_prefix_stripped_from_child_tool_names() {
    let (reg, _host) = load_batch_host();
    let out = run_batch(
        &reg,
        json!([
            { "tool": "functions.ok", "parameters": { "tag": "a" } },
            { "tool": "functions.batch", "parameters": { "tool_calls": [] } },
        ]),
    )
    .expect("batch failed");
    let expected = format!(
        "{}{}{}",
        section("ok", "ok:a"),
        section("batch", &format!("{ERROR_PREFIX}{NESTED_ERROR}")),
        summary_mixed(1, 2, 1)
    );
    assert_eq!(out, expected);
    let calls = recorded_calls(&reg);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["tool"], json!("ok"));
}

#[test_case::test_case(json!([]), EMPTY_ERROR ; "empty_list")]
#[test_case::test_case(json!([{ "tool": "ok", "parameters": { "tag": "x" }, "tag": "y" }]), "duplicate parameter 'tag'" ; "duplicate_key")]
fn invalid_input_errors_without_dispatch(tool_calls: Value, expected_err: &str) {
    let (reg, _host) = load_batch_host();
    let err = run_batch(&reg, tool_calls).unwrap_err();
    assert!(err.contains(expected_err), "got: {err}");
    assert!(
        recorded_calls(&reg).is_empty(),
        "nothing must be dispatched"
    );
}

#[test]
fn nested_batch_rejected_without_dispatch() {
    let (reg, _host) = load_batch_host();
    let out = run_batch(
        &reg,
        json!([
            { "tool": "batch", "parameters": { "tool_calls": [] } },
            { "tool": "ok", "parameters": { "tag": "a" } },
        ]),
    )
    .expect("batch failed");
    let expected = format!(
        "{}{}{}",
        section("batch", &format!("{ERROR_PREFIX}{NESTED_ERROR}")),
        section("ok", "ok:a"),
        summary_mixed(1, 2, 1)
    );
    assert_eq!(out, expected);
    let calls = recorded_calls(&reg);
    assert_eq!(calls.len(), 1, "nested batch must not be dispatched");
    assert_eq!(calls[0]["tool"], json!("ok"));
}

#[test]
fn overflow_entries_discarded_with_section() {
    let (reg, _host) = load_batch_host();
    let entries: Vec<Value> = (0..MAX_BATCH_SIZE + 1)
        .map(|i| json!({ "tool": "ok", "parameters": { "tag": i.to_string() } }))
        .collect();
    let out = run_batch(&reg, json!(entries)).expect("batch failed");
    assert!(
        out.contains(&format!("{ERROR_PREFIX}{DISCARDED_ERROR}")),
        "got: {out}"
    );
    assert!(
        out.ends_with(&summary_mixed(MAX_BATCH_SIZE, MAX_BATCH_SIZE + 1, 1)),
        "got: {out}"
    );
    assert_eq!(
        recorded_calls(&reg).len(),
        MAX_BATCH_SIZE,
        "only the first {MAX_BATCH_SIZE} entries may dispatch"
    );
}

/// One exact-string assertion pins the llm output contract: success
/// sections, error sections (child failure and unknown tool), input order,
/// and the mixed summary line.
#[test]
fn mixed_success_and_error_keeps_input_order() {
    let (reg, _host) = load_batch_host();
    let out = run_batch(
        &reg,
        json!([
            { "tool": "ok", "parameters": { "tag": "a" } },
            { "tool": "boom", "parameters": {} },
            { "tool": "nope", "parameters": {} },
            { "tool": "ok", "parameters": { "tag": "b" } },
        ]),
    )
    .expect("batch failed");
    let expected = format!(
        "{}{}{}{}{}",
        section("ok", "ok:a"),
        section("boom", &format!("{ERROR_PREFIX}{BOOM_ERR}")),
        section("nope", &format!("{ERROR_PREFIX}unknown tool: nope")),
        section("ok", "ok:b"),
        summary_mixed(2, 4, 2)
    );
    assert_eq!(out, expected);
}

/// `park` blocks until `release` runs: completion order is release-first,
/// yet sections must come out in input order. Also proves real overlap
/// (serial execution would deadlock this test).
#[test]
fn children_overlap_and_output_keeps_input_order() {
    let (reg, _host) = load_batch_host();
    let out = run_batch(
        &reg,
        json!([
            { "tool": "park", "parameters": {} },
            { "tool": "release", "parameters": {} },
        ]),
    )
    .expect("batch failed");
    let expected = format!(
        "{}{}{}",
        section("park", "parked_done"),
        section("release", "released_done"),
        summary_all_ok(2)
    );
    assert_eq!(out, expected);
}

fn restore_snapshot_lines(
    host: &PluginHost,
    input: Value,
    output: &str,
    state: Option<Value>,
) -> Vec<Vec<(String, SpanStyle)>> {
    restore_snapshot_lines_opts(host, input, output, state, Vec::new())
}

/// Children start at their header, so anything asserting on a body has to
/// reveal it first. Row 0 is the batch header, which reveals all of them.
fn restore_snapshot_lines_revealed(
    host: &PluginHost,
    input: Value,
    output: &str,
    state: Option<Value>,
) -> Vec<Vec<(String, SpanStyle)>> {
    restore_snapshot_lines_opts(host, input, output, state, vec![REVEAL_CHILDREN])
}

fn restore_snapshot_lines_opts(
    host: &PluginHost,
    input: Value,
    output: &str,
    state: Option<Value>,
    clicks: Vec<usize>,
) -> Vec<Vec<(String, SpanStyle)>> {
    let handle = host.event_handle();
    let (tx, rx) = flume::unbounded();
    handle.request_restore(
        caudra_lua::RestoreItem {
            tool: Arc::from(BATCH_TOOL),
            tool_use_id: BATCH_ID.to_owned(),
            output: output.to_owned(),
            input,
            is_error: false,
            tool_output_lines: ToolOutputLines::default(),
            theme_gen: None,
            clicks,
            state,
            lua_provenance: None,
        },
        EventSender::new(tx, 0),
    );
    handle.wait_restore_complete_for_test();
    barrier(host);
    snapshot_lines(drain_snapshots(&rx).last().expect("no snapshot emitted"))
}

/// The empty LoadSource drains the async gate, so spawned follow-ups
/// (highlight rewrites etc.) finish before we read snapshots.
fn barrier(host: &PluginHost) {
    host.load_source("barrier", "").unwrap();
}

fn drain_snapshots(rx: &flume::Receiver<Envelope>) -> Vec<BufferSnapshot> {
    rx.drain()
        .filter_map(|env| match env.event {
            AgentEvent::ToolSnapshot { id, snapshot, .. } => {
                assert_eq!(id, BATCH_ID);
                Some(snapshot)
            }
            _ => None,
        })
        .collect()
}

fn snapshot_lines(snapshot: &BufferSnapshot) -> Vec<Vec<(String, SpanStyle)>> {
    snapshot
        .lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| (s.text.clone(), s.style.clone()))
                .collect()
        })
        .collect()
}

fn lines_text(lines: &[Vec<(String, SpanStyle)>]) -> String {
    lines
        .iter()
        .map(|l| l.iter().map(|(t, _)| t.as_str()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Pins the child header contract: indicator span, `{tool}> ` prefix in
/// `tool_prefix` style, the child's own header spans, then the persisted
/// annotation like standalone (`push_header` in tool_display.rs).
#[test]
fn restore_with_state_renders_child_header_contract() {
    let (_reg, host) = load_batch_host();
    let lines = restore_snapshot_lines_revealed(
        &host,
        json!({ "tool_calls": [{ "tool": "hdrtool", "parameters": { "x": "A" } }] }),
        "irrelevant",
        Some(json!({ "children": [
            { "tool": "hdrtool", "status": "success", "output": "line one\nline two", "annotation": "12 lines" }
        ] })),
    );
    let header = &lines[0];
    assert_eq!(
        header[0],
        ("● ".to_owned(), SpanStyle::Named("tool_success".to_owned()))
    );
    assert_eq!(
        header[1],
        (
            "hdrtool> ".to_owned(),
            SpanStyle::Named("tool_prefix".to_owned())
        )
    );
    assert_eq!(header[2].0, "H:A", "child's own header spans must follow");
    assert_eq!(
        header.last().unwrap(),
        &(
            " (12 lines)".to_owned(),
            SpanStyle::Named("tool_annotation".to_owned())
        ),
        "persisted annotation ends the header"
    );

    let text = lines_text(&lines);
    assert!(text.contains("line one"), "body from state: {text}");
    assert!(text.contains("line two"), "body from state: {text}");
}

/// Errors render like standalone: red `●` indicator and the plain body,
/// without the llm-only `[ERROR]` prefix or a red body.
#[test]
fn restore_error_child_renders_error_style() {
    let (_reg, host) = load_batch_host();
    let lines = restore_snapshot_lines_revealed(
        &host,
        json!({ "tool_calls": [{ "tool": "ok", "parameters": {} }] }),
        "irrelevant",
        Some(json!({ "children": [
            { "tool": "ok", "status": "error", "output": "it broke" }
        ] })),
    );
    assert_eq!(
        lines[0][0],
        ("● ".to_owned(), SpanStyle::Named("tool_error".to_owned()))
    );
    let text = lines_text(&lines);
    assert!(text.contains("it broke"), "got: {text}");
    assert!(
        !text.contains(ERROR_PREFIX),
        "UI body must not carry the llm error prefix: {text}"
    );
    let body_spans: Vec<_> = lines[1..].iter().flatten().collect();
    assert!(
        body_spans
            .iter()
            .all(|(_, style)| *style != SpanStyle::Named("tool_error".to_owned())),
        "error body must render plain, not red: {body_spans:?}"
    );
}

/// A batch child's body is the child tool's own restore view, including
/// its ToolView truncation notice.
#[test]
fn restore_child_body_equals_child_restore_view() {
    let (_reg, host) = load_batch_host();
    let lines = restore_snapshot_lines_revealed(
        &host,
        json!({ "tool_calls": [{ "tool": "viewer", "parameters": {} }] }),
        "irrelevant",
        Some(json!({ "children": [
            { "tool": "viewer", "status": "success", "output": "l1\nl2\nl3\nl4\nl5" }
        ] })),
    );
    let text = lines_text(&lines);
    assert!(text.contains("l1"), "visible head line: {text}");
    assert!(text.contains("l2"), "visible head line: {text}");
    assert!(
        !text.contains("l3"),
        "lines beyond the child's own cap stay hidden: {text}"
    );
    assert!(
        text.contains("... (3 lines) (click to expand)"),
        "the child's own ToolView notice must render: {text}"
    );
}

#[test]
fn usage_renders_inline_on_matching_child() {
    let (reg, host) = load_batch_host();
    let (state, snapshots) = exec_batch_live(
        &host,
        &reg,
        json!([
            { "tool": "used", "parameters": {} },
            { "tool": "ok", "parameters": { "tag": "b" } }
        ]),
    );
    assert_eq!(state["children"][0]["usage"], CHILD_USAGE);

    let lines = snapshot_lines(snapshots.last().expect("no snapshot emitted"));
    let header = &lines[0];
    assert_eq!(
        &header[header.len() - 2..],
        [
            (
                " (model-x)".to_owned(),
                SpanStyle::Named("tool_annotation".to_owned())
            ),
            (
                format!("  {CHILD_USAGE}"),
                SpanStyle::Named("dim".to_owned())
            ),
        ],
        "usage closes the child header, after the annotation"
    );
    let text = lines_text(&lines);
    assert_eq!(
        text.matches(CHILD_USAGE).count(),
        1,
        "usage belongs to one child only: {text}"
    );
}

fn progress_line(buf: &caudra_agent::SharedBuf) -> Option<Vec<(String, SpanStyle)>> {
    buf.take()
        .lines
        .iter()
        .find(|line| {
            line.spans
                .first()
                .is_some_and(|s| s.text == ACTIVITY_PREFIX)
        })
        .map(|line| {
            line.spans
                .iter()
                .map(|s| (s.text.clone(), s.style.clone()))
                .collect()
        })
}

fn dim(text: &str) -> (String, SpanStyle) {
    (text.to_owned(), SpanStyle::Named(DIM_STYLE.to_owned()))
}

/// A subagent child reports what it is doing on its own row under the
/// header. What it was doing is stale once it settles; what it did is the
/// only record of the work its output does not show, so the tally stays.
#[test]
fn a_running_child_shows_its_progress_and_keeps_the_tally_once_it_settles() {
    let batch = start_batch(json!([{ "tool": "busy", "parameters": {} }]));

    let deadline = Instant::now() + PAINT_TIMEOUT;
    let line = loop {
        if let Some(line) = progress_line(&batch.body) {
            break line;
        }
        assert!(Instant::now() < deadline, "no paint carried the progress");
        std::thread::sleep(PAINT_POLL_INTERVAL);
    };
    assert_eq!(
        line,
        [
            dim(ACTIVITY_PREFIX),
            (
                "shell".to_owned(),
                SpanStyle::Named("tool_prefix".to_owned())
            ),
            dim(&format!(" {CHILD_ACTIVITY}")),
            dim(ANNOTATION_SEP),
            dim(CHILD_TALLY),
        ]
    );

    batch
        .result
        .recv_timeout(BATCH_RESULT_TIMEOUT)
        .expect("batch never settled")
        .expect("batch failed");
    assert_eq!(
        progress_line(&batch.body),
        Some(vec![dim(ACTIVITY_PREFIX), dim(CHILD_TALLY)]),
        "a settled child keeps what it did, not what it was doing"
    );
}

/// A child can annotate more than once (a task child streams its model,
/// then its completion note arrives on the same channel); batch joins
/// them on the child header in order.
#[test]
fn annotations_append_on_child_in_order() {
    let (reg, _host) = load_batch_host();
    let state = run_batch_state(&reg, json!([{ "tool": "annotated", "parameters": {} }]));
    assert_eq!(
        state["children"][0]["annotation"],
        json!("model-x · 5 lines")
    );
}

/// Throwing child header/restore callbacks degrade that child to plain
/// rendering without failing the batch.
#[test]
fn throwing_child_callbacks_degrade_to_plain() {
    let (_reg, host) = load_batch_host();
    let lines = restore_snapshot_lines_revealed(
        &host,
        json!({ "tool_calls": [
            { "tool": "badhdr", "parameters": {} },
            { "tool": "badrestore", "parameters": {} },
        ] }),
        "irrelevant",
        Some(json!({ "children": [
            { "tool": "badhdr", "status": "success", "output": "hdr body" },
            { "tool": "badrestore", "status": "success", "output": "restore body" },
        ] })),
    );
    let text = lines_text(&lines);
    assert!(text.contains("badhdr> "), "plain-name header: {text}");
    assert!(text.contains("hdr body"), "body still renders: {text}");
    assert!(text.contains("restore body"), "plain body fallback: {text}");
}

/// No state: parse `## tool` sections from the LLM output.
#[test]
fn restore_without_state_parses_llm_sections() {
    let (_reg, host) = load_batch_host();
    let stored = format!(
        "{}{}{}",
        section("hdrtool", "line one\nline two"),
        section("ok", &format!("{ERROR_PREFIX}{BOOM_ERR}")),
        summary_mixed(1, 2, 1)
    );
    let lines = restore_snapshot_lines_revealed(
        &host,
        json!({ "tool_calls": [
            { "tool": "hdrtool", "parameters": { "x": "A" } },
            { "tool": "ok", "parameters": {} },
        ] }),
        &stored,
        None,
    );
    assert_eq!(lines[0][2].0, "H:A", "child header fn still applies");
    let text = lines_text(&lines);
    assert!(
        lines.iter().any(|l| l
            .iter()
            .any(|(_, s)| *s == SpanStyle::Named("tool_error".into()))),
        "[ERROR] section maps to error status: {lines:?}"
    );
    assert!(text.contains("line one"), "body from section: {text}");
    assert!(text.contains(BOOM_ERR), "error body: {text}");
    assert!(
        !text.contains(ERROR_PREFIX),
        "llm error prefix must not render: {text}"
    );
    assert!(
        !text.contains("successfully"),
        "summary line must not render: {text}"
    );
}

/// A body line that looks like the next section header, but is not
/// preceded by the blank line `render_llm` always emits, must stay body
/// text instead of splitting the section early.
#[test]
fn restore_without_state_keeps_header_lookalike_in_body() {
    let (_reg, host) = load_batch_host();
    let stored = format!(
        "{}{}{}",
        section("ok", "body line\n## hdrtool\nbody tail"),
        section("hdrtool", "real body"),
        summary_all_ok(2)
    );
    let lines = restore_snapshot_lines_revealed(
        &host,
        json!({ "tool_calls": [
            { "tool": "ok", "parameters": {} },
            { "tool": "hdrtool", "parameters": { "x": "A" } },
        ] }),
        &stored,
        None,
    );
    let text = lines_text(&lines);
    assert!(text.contains("## hdrtool"), "lookalike stays body: {text}");
    assert!(text.contains("body tail"), "section 1 intact: {text}");
    assert!(text.contains("real body"), "section 2 body: {text}");
    assert!(
        !text.contains("successfully"),
        "parsed path, not legacy fallback: {text}"
    );
}

/// Unparseable output: raw text as one plain body.
#[test]
fn restore_without_state_falls_back_to_raw_output() {
    let (_reg, host) = load_batch_host();
    let lines = restore_snapshot_lines(
        &host,
        json!({ "tool_calls": [{ "tool": "ok", "parameters": { "tag": "a" } }] }),
        "not the section format",
        None,
    );
    let text = lines_text(&lines);
    assert!(text.contains("ok> "), "header from input: {text}");
    assert!(text.contains("not the section format"), "raw body: {text}");
}

/// Two viewer children, five lines each, long enough that both start
/// truncated with a "(click to expand)" notice.
fn two_truncated_viewers() -> (Value, Value) {
    let input = json!({ "tool_calls": [
        { "tool": "viewer", "parameters": {} },
        { "tool": "viewer", "parameters": {} },
    ] });
    let state = json!({ "children": [
        { "tool": "viewer", "status": "success", "output": "a1\na2\na3\na4\na5" },
        { "tool": "viewer", "status": "success", "output": "b1\nb2\nb3\nb4\nb5" },
    ] });
    (input, state)
}

/// A replayed click row must reach exactly the child it lands on and run
/// its real toggle. Two regressions pinned here: the async `buf:click`
/// held the child userdata borrow across the handler, silently dropping
/// the click, and the `{row=0}` replay toggled every child instead of the
/// clicked one.
#[test]
fn replayed_click_expands_only_the_clicked_child() {
    let (_reg, host) = load_batch_host();
    let (input, state) = two_truncated_viewers();

    // Rows are 1-based (row 0 = header), so snapshot line i = row i+1.
    // Find child2's notice dynamically so layout changes can't break this.
    let revealed =
        restore_snapshot_lines_revealed(&host, input.clone(), "irrelevant", Some(state.clone()));
    let notice_row = 1 + revealed
        .iter()
        .enumerate()
        .filter(|(_, l)| l.iter().any(|(t, _)| t.contains("(click to expand)")))
        .nth(1)
        .map(|(i, _)| i)
        .expect("second child's truncation notice");

    let text = lines_text(&restore_snapshot_lines_opts(
        &host,
        input.clone(),
        "irrelevant",
        Some(state.clone()),
        vec![REVEAL_CHILDREN, notice_row],
    ));
    assert!(text.contains("b5"), "clicked child expands: {text}");
    assert!(!text.contains("a3"), "other child stays truncated: {text}");
    assert!(
        text.contains("(click to expand)"),
        "other child keeps its notice: {text}"
    );

    // A second click inside the now-expanded child collapses it again.
    let text = lines_text(&restore_snapshot_lines_opts(
        &host,
        input,
        "irrelevant",
        Some(state),
        vec![REVEAL_CHILDREN, notice_row, notice_row],
    ));
    assert!(!text.contains("b3"), "second click collapses: {text}");
}

/// A batch is a summary first: every child settles at its header so the
/// transcript stays scannable, and a body arrives only when asked for.
#[test]
fn children_start_collapsed_and_open_on_click() {
    let (_reg, host) = load_batch_host();
    let (input, state) = two_truncated_viewers();

    let collapsed = lines_text(&restore_snapshot_lines(
        &host,
        input.clone(),
        "irrelevant",
        Some(state.clone()),
    ));
    assert_eq!(
        collapsed.lines().filter(|l| l.contains("viewer> ")).count(),
        2,
        "both child headers render: {collapsed}"
    );
    assert!(
        !collapsed.contains("a1") && !collapsed.contains("b1"),
        "no child body before a click: {collapsed}"
    );

    let revealed = lines_text(&restore_snapshot_lines_revealed(
        &host,
        input,
        "irrelevant",
        Some(state),
    ));
    assert!(
        revealed.contains("a1") && revealed.contains("b1"),
        "a revealed child shows its body: {revealed}"
    );
}

/// Row 0 is the batch header: a click there fans out to every child. It
/// reveals them first, then expands them, then collapses again, pinning the
/// broadcast as a real toggle rather than an expand-all.
#[test]
fn header_click_toggles_all_children() {
    let (_reg, host) = load_batch_host();
    let (input, state) = two_truncated_viewers();

    let text = lines_text(&restore_snapshot_lines_opts(
        &host,
        input.clone(),
        "irrelevant",
        Some(state.clone()),
        vec![REVEAL_CHILDREN, REVEAL_CHILDREN],
    ));
    assert!(
        text.contains("a5") && text.contains("b5"),
        "header click expands every child: {text}"
    );

    let text = lines_text(&restore_snapshot_lines_opts(
        &host,
        input,
        "irrelevant",
        Some(state),
        vec![REVEAL_CHILDREN, REVEAL_CHILDREN, REVEAL_CHILDREN],
    ));
    assert!(
        !text.contains("a3") && !text.contains("b3"),
        "a later header click collapses every child: {text}"
    );
}

/// Regression: an edit child's body must show the code change (old lines
/// in `diff_old`, new lines in `diff_new`), not the llm summary. Runs the
/// real edit and batch plugins. The extensionless path outside any real
/// filesystem pins the plain diff render: no async highlight rewrite (which
/// replaces these spans; covered at text level in real_plugins_restore.rs)
/// and no line numbers read back from disk.
#[test]
fn edit_child_body_renders_diff_not_summary() {
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::with_all_builtins(Arc::clone(&reg)).unwrap();
    let lines = restore_snapshot_lines_revealed(
        &host,
        json!({ "tool_calls": [{ "tool": "edit", "parameters": {
            "path": "/nonexistent/f",
            "old_string": "let a = 1;",
            "new_string": "let a = 2;",
        } }] }),
        "irrelevant",
        Some(json!({ "children": [
            { "tool": "edit", "status": "success", "output": "edited /nonexistent/f" }
        ] })),
    );
    let spans: Vec<(String, SpanStyle)> = lines.into_iter().flatten().collect();
    let old_span = (
        "- let a = 1;".to_owned(),
        SpanStyle::Named("diff_old".into()),
    );
    let new_span = (
        "+ let a = 2;".to_owned(),
        SpanStyle::Named("diff_new".into()),
    );
    assert!(spans.contains(&old_span), "old line in diff_old: {spans:?}");
    assert!(spans.contains(&new_span), "new line in diff_new: {spans:?}");
    assert!(
        !spans
            .iter()
            .any(|(t, _)| t.contains("edited /nonexistent/f")),
        "summary must not be the body: {spans:?}"
    );
}

// --- Live execution snapshots (real dispatch, no call_tool stub) ---

/// Regression: child restores used to snapshot the first-created buf
/// instead of the batch root buf, letting a tiny header overwrite the
/// full batch render.
#[test]
fn async_highlight_tasks_never_shrink_and_reach_final_snapshot() {
    let (host, state, snapshots) = live_batch(HL_CHILD_SRC, "hl");
    let header_counts: Vec<usize> = snapshots
        .iter()
        .map(|s| s.text().matches("hl> ").count())
        .collect();
    assert!(
        header_counts.windows(2).all(|w| w[0] <= w[1]),
        "a later snapshot must never lose children: {header_counts:?}"
    );
    assert_eq!(
        header_counts.last(),
        Some(&2),
        "final snapshot must carry all children"
    );
    let revealed = restore_snapshot_lines_revealed(
        &host,
        json!({ "tool_calls": [
            { "tool": "hl", "parameters": {} },
            { "tool": "hl", "parameters": {} },
        ] }),
        "irrelevant",
        Some(state),
    );
    let has_inline = revealed
        .iter()
        .flatten()
        .any(|(_, style)| matches!(style, SpanStyle::Inline(_)));
    assert!(
        has_inline,
        "a revealed child must contain highlighted spans, got:\n{}",
        lines_text(&revealed)
    );
}

/// Restores that await async APIs (like bash highlighting its header)
/// must not throw out of the `get_tool` wrapper.
#[test]
fn child_restore_awaiting_async_api_keeps_its_body() {
    let (host, state, _) = live_batch(&hl_child_src(CMD_TOOL), CMD_TOOL);
    let text = lines_text(&restore_snapshot_lines_revealed(
        &host,
        json!({ "tool_calls": [
            { "tool": CMD_TOOL, "parameters": {} },
            { "tool": CMD_TOOL, "parameters": {} },
        ] }),
        "irrelevant",
        Some(state),
    ));
    assert!(
        text.contains("echo header-marker"),
        "child restore header must survive, got:\n{text}"
    );
}

const NATIVE_TOOL: &str = "file_reader";
const NATIVE_PATH: &str = "caudra-config/src/lib.rs";
const NATIVE_BODY: &str = "const A: usize = 1;";
const NATIVE_MODEL_JSON: &str = "{\n  \"kind\": \"read\",\n  \"totalLines\": 1\n}";

/// A native tool whose header renders its argument, the way the real file
/// tools do. Native tools have no Lua handle, so a batch child used to fall
/// back to the bare tool name and render `file_reader> file_reader`.
struct NativeHeaderTool(String);

impl ToolInvocation for NativeHeaderTool {
    fn start_header(&self) -> HeaderFuture {
        HeaderFuture::Ready(HeaderResult::plain(self.0.clone()))
    }

    /// A structured result whose model form is a JSON record, the way every
    /// Workcell tool answers.
    fn execute<'a>(self: Box<Self>, _ctx: &'a ToolContext) -> ExecFuture<'a> {
        let path = self.0.clone();
        Box::pin(async move {
            ToolExecResult::from(Ok::<_, String>(ToolOutput::ReadCode {
                path,
                start_line: 1,
                lines: vec![NATIVE_BODY.to_owned()],
                total_lines: 1,
                instructions: None,
            }))
            .with_model_output(Some(NATIVE_MODEL_JSON.to_owned()))
        })
    }
}

impl Tool for NativeHeaderTool {
    fn name(&self) -> &str {
        NATIVE_TOOL
    }

    fn description(&self, _ctx: &DescriptionContext) -> std::borrow::Cow<'_, str> {
        "native header".into()
    }

    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": { "path": { "type": "string" } } })
    }

    fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        Ok(Box::new(NativeHeaderTool(
            input
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        )))
    }
}

/// A native child's header comes from the registry, so it reads like the
/// standalone tool instead of repeating the tool name.
#[test]
fn native_child_header_comes_from_the_registry() {
    let reg = Arc::new(ToolRegistry::new());
    reg.register(
        Arc::new(NativeHeaderTool(String::new())),
        ToolSource::Native {
            owner: "test".into(),
            contract: "test".into(),
            trusted: true,
        },
    )
    .unwrap();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source("batch_only", BATCH_PLUGIN_SRC).unwrap();

    let text = lines_text(&restore_snapshot_lines(
        &host,
        json!({ "tool_calls": [
            { "tool": NATIVE_TOOL, "parameters": { "path": NATIVE_PATH } },
        ] }),
        "irrelevant",
        Some(json!({ "children": [
            { "tool": NATIVE_TOOL, "status": "success", "output": "x" },
        ] })),
    ));
    assert!(
        text.contains(&format!("{NATIVE_TOOL}> {NATIVE_PATH}")),
        "native child header must name what it acted on: {text}"
    );
}

/// A native tool answers the model with a structured record. The reader gets
/// the same result written for a person, so a body is never a JSON dump.
#[test]
fn a_native_child_body_is_not_the_model_record() {
    let reg = Arc::new(ToolRegistry::new());
    reg.register(
        Arc::new(NativeHeaderTool(String::new())),
        ToolSource::Native {
            owner: "test".into(),
            contract: "test".into(),
            trusted: true,
        },
    )
    .unwrap();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source("batch_only", BATCH_PLUGIN_SRC).unwrap();

    let (state, _) = exec_batch_live(
        &host,
        &reg,
        json!([{ "tool": NATIVE_TOOL, "parameters": { "path": NATIVE_PATH } }]),
    );
    let text = lines_text(&restore_snapshot_lines_revealed(
        &host,
        json!({ "tool_calls": [
            { "tool": NATIVE_TOOL, "parameters": { "path": NATIVE_PATH } },
        ] }),
        "irrelevant",
        Some(state),
    ));
    assert!(
        text.contains(NATIVE_BODY),
        "the body must carry the result a reader can use: {text}"
    );
    assert!(
        !text.contains("\"kind\""),
        "the model's structured record must not reach the transcript: {text}"
    );
}

const BATCH_ID: &str = "batch_id";

const HL_CHILD_SRC: &str = r#"
local ToolView = require("caudra.tool_view")
caudra.api.register_tool({
  name = "hl",
  description = "styled header + async-highlight restore",
  schema = { type = "object", properties = {} },
  audiences = { "main" },
  header = function(input)
    local b = caudra.ui.buf()
    b:set_lines({ { { "hl-header", "tool" } } })
    return b
  end,
  restore = function(input, output, is_error, rctx)
    local buf = caudra.ui.buf()
    local view = ToolView.new(buf, { max_lines = 10, keep = "head" })
    view:set_highlight(output, "lua")
    view:finish()
    return buf
  end,
  handler = function() return "local x = 1" end,
})
"#;

/// A child whose restore awaits an async api inline, the way bash highlights
/// its header. Registered under `@TOOL@` so the same source can stand in for
/// whichever child a test needs.
const SYNC_HL_CHILD_SRC: &str = r#"
local ToolView = require("caudra.tool_view")
caudra.api.register_tool({
  name = "@TOOL@",
  description = "restore awaits caudra.ui.highlight inline",
  schema = { type = "object", properties = {} },
  audiences = { "main" },
  restore = function(input, output, is_error, rctx)
    local buf = caudra.ui.buf()
    local view = ToolView.new(buf, { max_lines = 10, keep = "tail" })
    local header = caudra.ui.highlight("echo header-marker", "bash") or { { { "echo header-marker" } } }
    view:set_header(header)
    view:append(output)
    view:finish()
    return buf
  end,
  handler = function() return "cmd-output" end,
})
"#;

/// Real dispatch, no `call_tool` stub, so children stream their snapshots the
/// way they do in a session.
fn exec_batch_live(
    host: &PluginHost,
    reg: &Arc<ToolRegistry>,
    tool_calls: Value,
) -> (Value, Vec<BufferSnapshot>) {
    let (tx, rx) = flume::unbounded();
    let event_tx = EventSender::new(tx, 0);
    let mut ctx = stub_ctx_with(&AgentMode::Build, Some(&event_tx), Some(BATCH_ID));
    ctx.registry = Arc::clone(reg);

    let input = json!({ "tool_calls": tool_calls });
    let inv = reg
        .get(BATCH_TOOL)
        .expect("batch registered")
        .tool
        .parse(&input)
        .expect("parse failed");
    let state = smol::block_on(async { inv.execute(&ctx).await })
        .output
        .expect("batch failed")
        .state()
        .cloned()
        .expect("no state on batch output");

    barrier(host);
    (state, drain_snapshots(&rx))
}

fn hl_child_src(tool: &str) -> String {
    SYNC_HL_CHILD_SRC.replace("@TOOL@", tool)
}

/// The host and state outlive the run so a caller can reveal the children and
/// inspect a body the live snapshots leave collapsed.
fn live_batch(child_src: &str, tool: &str) -> (PluginHost, Value, Vec<BufferSnapshot>) {
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source("live_batch", &format!("{child_src}\n{BATCH_PLUGIN_SRC}"))
        .unwrap();
    let (state, snapshots) = exec_batch_live(
        &host,
        &reg,
        json!([
            { "tool": tool, "parameters": {} },
            { "tool": tool, "parameters": {} },
        ]),
    );
    (host, state, snapshots)
}
