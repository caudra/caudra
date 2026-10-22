//! Hosted execution against a real `monty` worker and a fake host.
//!
//! These need the worker binary (`make code-worker`). Local runs skip without it; CI sets
//! WORKCELL_REQUIRE_CODE_WORKER so a missing worker fails the suite.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use serde_json::{Value, json};
use test_case::test_case;
use tokio_util::sync::CancellationToken;
use workcell_mcp_code::{
    CodeConfiguration, CodeHost, CodeInput, CodeOutput, CodePools, CodeToolGroup, HostCall,
    HostError, HostErrorKind, HostInvocation, HostName, HostedExecution, HostedState,
    MAX_HOST_CALLS_PER_RUN, Outcome, PreparedCode, Repair, SleepFuture, StateNote,
    WORKER_FILE_NAME, WorkerSource,
};

const NAMESPACE: &str = "gh";
const DENIED: &str = "denied by the user";
const SLOW_REPLY: &str = "answered late";
const HOME_ASSISTANT: &str = "homeassistant";
const EXECUTOR: &str = "executor";
const SEARCH: &str = "search";
const MISSING_LIST: &str = "Shopping list";
const NO_SUCH_LIST: &str = "no such list";
/// Declared as Caudra's gateway declares them: a tool takes keywords, or one dict of them.
const STUBS: &str = "from typing import Any, overload\nclass _Gh:\n    @overload\n    async def list(self, *, owner: str = '') -> dict[str, Any]: ...\n    @overload\n    async def list(self, arguments: dict[str, Any], /) -> dict[str, Any]: ...\n    async def echo(self, value: Any, *, nested: Any) -> Any: ...\n    async def boom(self) -> Any: ...\n    async def hang(self) -> Any: ...\n    async def slow(self) -> str: ...\ngh: _Gh\nclass _Homeassistant:\n    async def homeassistant__GetLiveContext(self) -> Any: ...\n    async def llm__GetDateTime(self) -> Any: ...\n    async def todo__get_items(self, *, todo_list: str, status: str | None = None) -> Any: ...\nhomeassistant: _Homeassistant\nclass _Executor:\n    @overload\n    async def skills(self) -> Any: ...\n    @overload\n    async def skills(self, arguments: dict[str, Any], /) -> Any: ...\n    async def search(self, *, query: str = '', namespace: str = '', limit: int = 0) -> Any: ...\nexecutor: _Executor\ndef search(query: str) -> list[str]: ...\n";
const HOSTED_SUSPENSION_LIMIT_MESSAGE: &str = "The snippet made too many host round trips and was stopped. Batch work into fewer calls, and check for an undefined name read inside a loop.";
const SHORT_TIMEOUT_SECS: u64 = 1;
const TIME_LIMIT_TEST_DEADLINE: Duration = Duration::from_secs(10);
const MODULE_REPR: &str = "Repr(\"<module 'json'>\")";
/// Well past the short time limit, so a run charged for the wait could not finish within it.
const HOST_DELAY: Duration = Duration::from_secs(3);
/// Past the isolated cap of 256 round trips, yet within the hosted cap even at two per call.
const ROUND_TRIPS_PAST_THE_ISOLATED_CAP: usize = 300;
/// Each call is at least one round trip, so this overruns the hosted cap of 1,024.
const ROUND_TRIPS_PAST_THE_HOSTED_CAP: usize = 2_000;
const CALLS_BEFORE_RESTORE: usize = 60;
/// A sleep the fake host never ends, so only cancelling the run ends it.
const FOREVER: Duration = Duration::from_secs(1_000_000);
/// Short enough to keep the suite fast, long enough to measure.
const REAL_SLEEP: Duration = Duration::from_millis(50);
const GATHER_UNIMPORTED: &str =
    "a, b = await asyncio.gather(gh.list(owner='a'), gh.list(owner='b'))\n[a['owner'], b['owner']]";

fn worker() -> Option<PathBuf> {
    let worker = std::env::var_os("WORKCELL_MCP_CODE_WORKER")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../../target/code-worker/bin")
                .join(WORKER_FILE_NAME)
        });
    if worker.is_file() {
        return Some(worker);
    }
    assert!(
        std::env::var_os("WORKCELL_REQUIRE_CODE_WORKER").is_none(),
        "the monty worker is required but missing"
    );
    None
}

async fn group() -> Option<CodeToolGroup> {
    group_with_type_check(true).await
}

async fn group_with_type_check(type_check: bool) -> Option<CodeToolGroup> {
    let worker = worker()?;
    Some(
        CodeToolGroup::new(CodeConfiguration {
            worker: WorkerSource::Path(&worker),
            type_check,
            pools: CodePools::All,
        })
        .await
        .expect("worker pool"),
    )
}

/// Records each deferred call when it starts, which is when the snippet first awaits it, and each
/// sleep, which it answers at once unless it is [`FOREVER`].
#[derive(Default)]
struct FakeHost {
    calls: Arc<Mutex<Vec<String>>>,
    slept: Arc<Mutex<Vec<Duration>>>,
}

impl FakeHost {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls").clone()
    }

    fn slept(&self) -> Vec<Duration> {
        let mut slept = self.slept.lock().expect("slept").clone();
        slept.sort();
        slept
    }
}

impl CodeHost for FakeHost {
    fn lookup(&self, namespace: Option<&str>, name: &str) -> HostName {
        match (namespace, name) {
            (None, NAMESPACE | HOME_ASSISTANT | EXECUTOR) => HostName::Namespace,
            (None, "search")
            | (Some(NAMESPACE), "list" | "echo" | "boom" | "hang" | "slow")
            | (Some(HOME_ASSISTANT | EXECUTOR), _) => HostName::Function,
            (Some(_), _) => HostName::Undefined(Some(format!("no tool `{name}`; use search()"))),
            (None, _) => HostName::Undefined(None),
        }
    }

    fn call(&self, call: HostCall) -> HostInvocation {
        if call.namespace.is_none() {
            return HostInvocation::Ready(Ok(json!([call.name])));
        }
        let calls = Arc::clone(&self.calls);
        HostInvocation::Deferred(Box::pin(async move {
            calls.lock().expect("calls").push(call.name.clone());
            match call.name.as_str() {
                "echo" => Ok(json!({"args": call.args, "kwargs": call.kwargs})),
                "boom" => Err(HostError::new(HostErrorKind::Permission, DENIED)),
                "todo__get_items" if call.kwargs.get("todo_list") == Some(&json!(MISSING_LIST)) => {
                    Err(HostError::new(HostErrorKind::Runtime, NO_SUCH_LIST))
                }
                "hang" => std::future::pending().await,
                SEARCH => Ok(search_answer()),
                "slow" => {
                    tokio::time::sleep(HOST_DELAY).await;
                    Ok(json!(SLOW_REPLY))
                }
                _ => {
                    let owner = call.kwargs.get("owner").or_else(|| {
                        call.args
                            .first()
                            .and_then(|arguments| arguments.get("owner"))
                    });
                    Ok(json!({ "owner": owner.cloned().unwrap_or(Value::Null) }))
                }
            }
        }))
    }

    fn stubs(&self) -> String {
        STUBS.to_owned()
    }

    fn sleep(&self, delay: Duration) -> SleepFuture {
        let slept = Arc::clone(&self.slept);
        Box::pin(async move {
            slept.lock().expect("slept").push(delay);
            if delay >= FOREVER {
                std::future::pending::<()>().await;
            }
        })
    }
}

/// A host with nothing to offer but the default sleep, which really waits.
struct BareHost;

impl CodeHost for BareHost {
    fn lookup(&self, _namespace: Option<&str>, _name: &str) -> HostName {
        HostName::Undefined(None)
    }

    fn call(&self, _call: HostCall) -> HostInvocation {
        HostInvocation::Ready(Err(HostError::new(HostErrorKind::Name, "no host calls")))
    }

    fn stubs(&self) -> String {
        String::new()
    }
}

/// What `executor.search` answers, whatever it is asked.
fn search_answer() -> Value {
    json!({"items": [{"name": "skill"}], "namespaces": [EXECUTOR], "unavailableApps": []})
}

async fn run(
    group: &CodeToolGroup,
    host: &Arc<FakeHost>,
    code: &str,
    state: Option<HostedState>,
) -> HostedExecution {
    let prepared = group
        .prepare_hosted(code.to_owned())
        .expect("valid snippet");
    execute(group, host, prepared, state).await
}

/// Hosted preparation always grants the maximum time limit; isolated preparation binds a short one.
async fn run_briefly(
    group: &CodeToolGroup,
    host: &Arc<FakeHost>,
    code: &str,
    state: Option<HostedState>,
) -> HostedExecution {
    let prepared = group
        .prepare(CodeInput {
            code: code.to_owned(),
            timeout_sec: Some(SHORT_TIMEOUT_SECS),
        })
        .expect("valid snippet");
    execute(group, host, prepared, state).await
}

async fn execute(
    group: &CodeToolGroup,
    host: &Arc<FakeHost>,
    prepared: PreparedCode,
    state: Option<HostedState>,
) -> HostedExecution {
    let host: Arc<dyn CodeHost> = host.clone();
    group
        .execute_hosted(prepared, host, state, CancellationToken::new())
        .await
}

/// Awaits up to `calls` host calls one at a time, counting each into an existing `made`.
fn awaited_calls(calls: usize) -> String {
    format!(
        "try:\n    for i in range({calls}):\n        await gh.list(owner='x')\n        made += 1\nexcept RuntimeError:\n    pass\nmade"
    )
}

fn output(executed: &HostedExecution) -> &CodeOutput {
    &executed.execution.as_ref().expect("not cancelled").output
}

fn result(executed: &HostedExecution) -> &Value {
    &output(executed).result
}

fn outcome(executed: &HostedExecution) -> Outcome {
    output(executed).outcome
}

#[tokio::test(flavor = "multi_thread")]
async fn namespaced_calls_return_host_values_and_gather_runs_them_together() {
    let Some(group) = group().await else { return };
    let host = Arc::new(FakeHost::default());
    let executed = run(
        &group,
        &host,
        "import asyncio\na, b = await asyncio.gather(gh.list(owner='a'), gh.list(owner='b'))\n[a['owner'], b['owner'], search('q')]",
        None,
    )
    .await;
    assert_eq!(result(&executed), &json!(["a", "b", ["search"]]));
    assert_eq!(host.calls().len(), 2);
    group.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_call_raises_a_catchable_permission_error() {
    let Some(group) = group().await else { return };
    let host = Arc::new(FakeHost::default());
    let caught = run(
        &group,
        &host,
        "try:\n    await gh.boom()\n    r = 'ran'\nexcept PermissionError as e:\n    r = str(e)\nr",
        None,
    )
    .await;
    assert_eq!(result(&caught), &json!(DENIED));

    let uncaught = run(&group, &host, "await gh.boom()", None).await;
    assert_eq!(outcome(&uncaught), Outcome::Exception);
    let exception = uncaught
        .execution
        .as_ref()
        .and_then(|execution| execution.output.exception.as_ref())
        .expect("exception");
    assert_eq!(exception.r#type, "PermissionError");
    assert_eq!(exception.message, DENIED);
    group.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_tools_fail_type_checking_before_any_call() {
    let Some(group) = group().await else { return };
    let host = Arc::new(FakeHost::default());
    for code in [
        "await gh.list(owner='a')\nawait gh.nope()",
        "await gh.list(owner=1)",
    ] {
        let executed = run(&group, &host, code, None).await;
        assert_eq!(outcome(&executed), Outcome::Rejected, "{code}");
    }
    assert!(host.calls().is_empty());
    group.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn variables_and_namespace_references_survive_between_runs() {
    let Some(group) = group().await else { return };
    let host = Arc::new(FakeHost::default());
    let first = run(
        &group,
        &host,
        "kept = await gh.list(owner='o')\nalias = gh\n1",
        None,
    )
    .await;
    assert_eq!(first.state_note, None);
    let state = first.state.expect("saved session");

    let second = run(
        &group,
        &host,
        "again = await alias.list(owner='p')\nkept = kept['owner']\n[kept, again['owner']]",
        Some(state),
    )
    .await;
    assert_eq!(result(&second), &json!(["o", "p"]));
    assert_eq!(second.state_note, None);

    let raised = run(
        &group,
        &host,
        "again = 'lost'\nawait gh.boom()",
        second.state,
    )
    .await;
    assert_eq!(outcome(&raised), Outcome::Exception);
    let third = run(&group, &host, "[kept, again]", raised.state).await;
    assert_eq!(result(&third), &json!(["o", "lost"]));
    group.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn complex_values_keep_their_type_through_session_dump_and_restore() {
    let Some(group) = group().await else { return };
    let host = Arc::new(FakeHost::default());
    let first = run(
        &group,
        &host,
        "z = complex(1, 2)\nkept = {'values': [z]}\nkept",
        None,
    )
    .await;
    assert_eq!(outcome(&first), Outcome::Completed);
    assert_eq!(result(&first), &json!({"values": ["(1+2j)"]}));
    assert_eq!(first.state_note, None);
    let state = first.state.expect("saved complex session");
    let second = run(
        &group,
        &host,
        "[z * 1j, kept['values'][0] + complex(3, -2)]",
        Some(state),
    )
    .await;
    assert_eq!(outcome(&second), Outcome::Completed);
    assert_eq!(result(&second), &json!(["(-2+1j)", "(4+0j)"]));
    assert_eq!(
        output(&second).result_repr.as_deref(),
        Some("[(-2+1j), (4+0j)]")
    );
    assert_eq!(second.state_note, None);
    assert!(second.state.is_some());
    group.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn complex_host_arguments_cross_as_json_strings_including_nested_values() {
    let Some(group) = group().await else { return };
    let host = Arc::new(FakeHost::default());
    let executed = run(
        &group,
        &host,
        "z = 1 + 2j\nawait gh.echo(z, nested={'values': [z, complex()]})",
        None,
    )
    .await;
    assert_eq!(outcome(&executed), Outcome::Completed);
    assert_eq!(
        result(&executed),
        &json!({"args": ["(1+2j)"], "kwargs": {"nested": {"values": ["(1+2j)", "0j"]}}})
    );
    assert_eq!(output(&executed).result_repr, None);
    assert_eq!(host.calls(), ["echo"]);
    group.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn imported_modules_render_and_cross_host_arguments_as_strings() {
    let Some(group) = group().await else { return };
    let host = Arc::new(FakeHost::default());
    let first = run(&group, &host, "import json\njson", None).await;
    assert_eq!(outcome(&first), Outcome::Completed, "{:?}", output(&first));
    assert_eq!(result(&first), &json!(MODULE_REPR));
    assert_eq!(output(&first).result_repr.as_deref(), Some(MODULE_REPR));
    let second = run(
        &group,
        &host,
        "await gh.echo(json, nested={'modules': [json]})",
        first.state,
    )
    .await;
    assert_eq!(
        outcome(&second),
        Outcome::Completed,
        "{:?}",
        output(&second)
    );
    assert_eq!(
        result(&second),
        &json!({"args": [MODULE_REPR], "kwargs": {"nested": {"modules": [MODULE_REPR]}}})
    );
    assert_eq!(output(&second).result_repr, None);
    assert_eq!(host.calls(), ["echo"]);
    group.shutdown().await;
}

#[test_case("import gh", "ModuleNotFoundError" ; "host_namespace_is_not_an_import")]
#[test_case("import subprocess", "ModuleNotFoundError" ; "host_python_is_not_available")]
#[test_case("from pathlib import Path\nPath('/etc/passwd').read_text()", "PermissionError" ; "unmounted_path")]
#[tokio::test(flavor = "multi_thread")]
async fn import_and_path_requests_never_dispatch_host_tools(code: &str, exception: &str) {
    let Some(group) = group_with_type_check(false).await else {
        return;
    };
    let host = Arc::new(FakeHost::default());
    let code = format!("{code}\nawait gh.list(owner='unreachable')");
    let executed = run(&group, &host, &code, None).await;
    let failed = output(&executed);
    assert_eq!(failed.outcome, Outcome::Exception, "{failed:?}");
    assert_eq!(
        failed.exception.as_ref().expect("refused").r#type,
        exception
    );
    assert!(host.calls().is_empty());
    group.shutdown().await;
}

/// A module used without its import is imported before the snippet runs, whether or not the
/// session was restored, and the snippet runs as written.
#[test_case(false ; "fresh")]
#[test_case(true ; "restored")]
#[tokio::test(flavor = "multi_thread")]
async fn a_module_used_without_its_import_is_imported_first(restored: bool) {
    let Some(group) = group().await else { return };
    let host = Arc::new(FakeHost::default());
    let state = match restored {
        true => Some(
            run(&group, &host, "kept = 1", None)
                .await
                .state
                .expect("saved session"),
        ),
        false => None,
    };
    let executed = run(&group, &host, GATHER_UNIMPORTED, state).await;
    assert_eq!(
        result(&executed),
        &json!(["a", "b"]),
        "{:?}",
        output(&executed)
    );
    assert_eq!(executed.repairs, [Repair::Imported("asyncio")]);
    group.shutdown().await;
}

/// A restored session's own variable named like a module stays its own, and no import is
/// reported for it.
#[tokio::test(flavor = "multi_thread")]
async fn a_variable_named_like_a_module_is_not_replaced_by_it() {
    let Some(group) = group().await else { return };
    let host = Arc::new(FakeHost::default());
    let first = run(&group, &host, "json = {'a': 1}", None).await;
    let second = run(&group, &host, "json.get('a')", first.state).await;
    assert_eq!(result(&second), &json!(1));
    assert!(second.repairs.is_empty(), "{:?}", second.repairs);
    group.shutdown().await;
}

/// Every repair keeps the snippet's lines where they were written, so a checker error names the
/// line the author wrote.
#[tokio::test(flavor = "multi_thread")]
async fn a_repaired_snippet_keeps_its_line_numbers() {
    let Some(group) = group().await else { return };
    let host = Arc::new(FakeHost::default());
    let executed = run(
        &group,
        &host,
        "```python\nhits = await search('q')\njson.dumps(hits)\nawait gh.list(owner=1)\n```",
        None,
    )
    .await;
    assert_eq!(
        executed.repairs,
        [
            Repair::Unfenced,
            Repair::Imported("json"),
            Repair::Unawaited("search".to_owned()),
        ]
    );
    assert_eq!(outcome(&executed), Outcome::Rejected);
    let diagnostic = output(&executed).diagnostic.as_deref().unwrap_or_default();
    assert!(diagnostic.contains("snippet.py:4:"), "{diagnostic}");
    assert!(!diagnostic.contains("unresolved-reference"), "{diagnostic}");
    group.shutdown().await;
}

/// A stray space before a statement, which Python rejects as an unexpected indent, is taken off.
#[tokio::test(flavor = "multi_thread")]
async fn an_over_indented_line_runs_lined_up_with_its_block() {
    let Some(group) = group().await else { return };
    let host = Arc::new(FakeHost::default());
    let executed = run(
        &group,
        &host,
        "import datetime\n datetime.datetime(2026, 1, 2).isoformat()",
        None,
    )
    .await;
    assert_eq!(
        result(&executed),
        &json!("2026-01-02T00:00:00"),
        "{:?}",
        output(&executed)
    );
    assert_eq!(executed.repairs, [Repair::Dedented]);
    group.shutdown().await;
}

/// A tool takes its arguments as keywords or as one dict, and both forms are checked.
#[tokio::test(flavor = "multi_thread")]
async fn a_tool_takes_one_dict_of_arguments() {
    let Some(group) = group().await else { return };
    let host = Arc::new(FakeHost::default());
    let executed = run(
        &group,
        &host,
        "(await gh.list({'owner': 'a'}))['owner']",
        None,
    )
    .await;
    assert_eq!(result(&executed), &json!("a"), "{:?}", output(&executed));
    let rejected = run(&group, &host, "await gh.list(1)", None).await;
    assert_eq!(outcome(&rejected), Outcome::Rejected);
    assert_eq!(host.calls(), ["list"]);
    group.shutdown().await;
}

const FIELD_GATHER_RETURNED: &str = r#"results = await asyncio.gather(
    homeassistant.homeassistant__GetLiveContext(),
    homeassistant.llm__GetDateTime(),
    homeassistant.todo__get_items(todo_list="To-Do", status="all"),
    homeassistant.todo__get_items(todo_list="Shopping list", status="all"),
    return_exceptions=True,
)
return {name: r if not isinstance(r, Exception) else f"ERROR: {r}" for name, r in zip(["liveContext", "dateTime", "todo", "shopping"], results)}"#;
const FIELD_GATHER_LAMBDAS: &str = r#"results = await asyncio.gather(
    homeassistant.homeassistant__GetLiveContext(),
    homeassistant.llm__GetDateTime(),
    homeassistant.todo__get_items(todo_list="To-Do", status="all"),
    homeassistant.todo__get_items(todo_list="Shopping list", status="all"),
    return_exceptions=True,
)
{
    "liveContext": (lambda r: f"ERROR: {r}" if isinstance(r, Exception) else r)(results[0]),
    "dateTime": (lambda r: f"ERROR: {r}" if isinstance(r, Exception) else r)(results[1]),
    "todo": (lambda r: f"ERROR: {r}" if isinstance(r, Exception) else r)(results[2]),
    "shopping": (lambda r: f"ERROR: {r}" if isinstance(r, Exception) else r)(results[3]),
}"#;
const FIELD_GATHER_PRINTED: &str = r#"results = await asyncio.gather(
    homeassistant.llm__GetDateTime(),
    homeassistant.homeassistant__GetLiveContext(),
    homeassistant.todo__get_items(todo_list='To-Do', status='all'),
    homeassistant.todo__get_items(todo_list='Shopping list', status='all'),
    return_exceptions=True,
)
for r in results:
    if isinstance(r, Exception):
        print(type(r).__name__, r)
    else:
        print(r)"#;
const FIELD_EMPTY_DICT: &str = "await executor.skills({})";
const FIELD_JAVASCRIPT: &str = include_str!("fixtures/field_javascript.js");
const FIELD_JAVASCRIPT_PYTHON: &str = include_str!("fixtures/field_javascript.py");
const JAVASCRIPT_SWITCH: &str =
    "const r = await gh.list({owner: 'a'});\nswitch (r.owner) { case 'a': console.log(1); }";
const SWITCH_UNTRANSLATED: &str =
    "It was not translated: `switch` on line 2 has no Python form that keeps its meaning.";
/// A wrong argument on the snippet's fifth line, which is the fourth of the Python that ran.
const JAVASCRIPT_WRONG_ARGUMENT: &str = "const r = await gh.list({owner: 'a'});\nif (r.owner === 'a') {\n  console.log(r.owner);\n}\nawait gh.list({owner: 1});";
const WRONG_ARGUMENT_LINE: usize = 4;
const WRONG_ARGUMENT_PYTHON: &str = "await gh.list(owner=1)";
const TOP_LEVEL_RETURN: &str = "module code, not a function body";
const JAVASCRIPT_STRINGIFY: &str = "const r = await gh.list({owner: 'é'});\nif (r.owner === 'é') {\n  console.log(JSON.stringify(r));\n}\nreturn JSON.stringify(r, null, 2);";
const STRINGIFIED_COMPACT: &str = "{\"owner\":\"é\"}\n";
const STRINGIFIED_INDENTED: &str = "{\n  \"owner\": \"é\"\n}";

/// Snippets models wrote against real servers that failed before Caudra repaired or explained
/// them, verbatim. A failure seen in the field goes here first, with what it should do now.
///
/// The `return_exceptions` cases lean on MONTY-GATHER-SHIM. Once Monty's own `gather` takes the
/// keyword and the shim is gone, they must still pass with `Repair::Settled` dropped from their
/// expected repairs.
#[test_case(FIELD_GATHER_RETURNED, &[Repair::Imported("asyncio"), Repair::FinalReturn, Repair::Settled], "ERROR: no such list" ; "gather_with_a_final_return")]
#[test_case(FIELD_GATHER_LAMBDAS, &[Repair::Imported("asyncio"), Repair::Settled], "ERROR: no such list" ; "gather_into_lambdas")]
#[test_case(FIELD_GATHER_PRINTED, &[Repair::Imported("asyncio"), Repair::Settled], "RuntimeError no such list" ; "gather_printed")]
#[test_case(FIELD_EMPTY_DICT, &[], "owner" ; "an_empty_dict_of_arguments")]
#[tokio::test(flavor = "multi_thread")]
async fn field_snippets_now_run(code: &str, repairs: &[Repair], shows: &str) {
    let Some(group) = group().await else { return };
    let host = Arc::new(FakeHost::default());
    let executed = run(&group, &host, code, None).await;
    let output = output(&executed);
    assert_eq!(output.outcome, Outcome::Completed, "{output:?}");
    let shown = format!("{}{}", output.stdout, output.result);
    assert!(shown.contains(shows), "{shown}");
    assert_eq!(executed.repairs, repairs);
    group.shutdown().await;
}

#[test_case(JAVASCRIPT_SWITCH, SWITCH_UNTRANSLATED ; "javascript_outside_the_subset")]
#[test_case("if True:\n    return 1\n2", TOP_LEVEL_RETURN ; "a_return_before_the_end")]
#[tokio::test(flavor = "multi_thread")]
async fn field_snippets_that_cannot_run_say_why(code: &str, explained_by: &str) {
    let Some(group) = group().await else { return };
    let host = Arc::new(FakeHost::default());
    let executed = run(&group, &host, code, None).await;
    assert_eq!(outcome(&executed), Outcome::Rejected);
    let diagnostic = output(&executed).diagnostic.as_deref().unwrap_or_default();
    assert!(diagnostic.contains(explained_by), "{diagnostic}");
    assert!(host.calls().is_empty());
    group.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn field_javascript_runs_as_the_python_it_translates_to() {
    let Some(group) = group().await else { return };
    let host = Arc::new(FakeHost::default());
    let executed = run(&group, &host, FIELD_JAVASCRIPT, None).await;
    assert_eq!(
        outcome(&executed),
        Outcome::Completed,
        "{:?}",
        output(&executed)
    );
    assert_eq!(host.calls(), [SEARCH, SEARCH]);
    assert_eq!(executed.repairs, [Repair::Translated]);
    assert_eq!(executed.taught_code, None);
    assert_eq!(
        executed.ran_as.as_deref(),
        Some(FIELD_JAVASCRIPT_PYTHON.trim_end())
    );
    assert_eq!(
        result(&executed),
        &json!([search_answer(), search_answer()])
    );
    group.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn translated_json_stringify_writes_the_text_javascript_does() {
    let Some(group) = group().await else { return };
    let host = Arc::new(FakeHost::default());
    let executed = run(&group, &host, JAVASCRIPT_STRINGIFY, None).await;
    assert_eq!(
        output(&executed).stdout,
        STRINGIFIED_COMPACT,
        "{:?}",
        output(&executed)
    );
    assert_eq!(result(&executed), &json!(STRINGIFIED_INDENTED));
    group.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_translated_snippet_reports_lines_of_the_python_that_ran() {
    let Some(group) = group().await else { return };
    let host = Arc::new(FakeHost::default());
    let executed = run(&group, &host, JAVASCRIPT_WRONG_ARGUMENT, None).await;
    assert_eq!(outcome(&executed), Outcome::Rejected);
    let ran_as = executed.ran_as.as_deref().expect("translated");
    assert_eq!(
        ran_as.lines().nth(WRONG_ARGUMENT_LINE - 1),
        Some(WRONG_ARGUMENT_PYTHON)
    );
    let diagnostic = output(&executed).diagnostic.as_deref().unwrap_or_default();
    assert!(
        diagnostic.contains(&format!("snippet.py:{WRONG_ARGUMENT_LINE}:")),
        "{diagnostic}"
    );
    assert!(host.calls().is_empty());
    group.shutdown().await;
}

/// A run on a restored session is checked on its own: a tool the server lacks or an argument of
/// the wrong type is refused before any call, at the snippet's own line and column, and the session
/// is kept. Names from earlier runs stay unknown to that check, so rebinding one still runs.
#[test_case("kept = kept['owner']\nawait gh.nope()", Some("snippet.py:2:7:") ; "a_tool_the_server_lacks")]
#[test_case("await gh.list(owner=kept)\nawait gh.list(owner=1)", Some("snippet.py:2:") ; "an_argument_of_the_wrong_type")]
#[test_case("import json\nawait gh.list(owner=kept)\nwrong: int = 'bad'", Some("snippet.py:3:") ; "builtin_import_with_independent_type_error")]
#[test_case("await gh.list(owner=kept)\nimport unavailable_module\nwrong: int = 'bad'", Some("snippet.py:3:") ; "import_stubs_with_independent_type_error")]
#[test_case("# nothing to run", None ; "only_a_comment")]
#[test_case("kept = kept['owner']\nkept", None ; "rebinding_an_earlier_name")]
#[tokio::test(flavor = "multi_thread")]
async fn a_restored_run_is_checked_against_the_tools_it_calls(
    code: &str,
    rejected_at: Option<&str>,
) {
    let Some(group) = group().await else { return };
    let host = Arc::new(FakeHost::default());
    let first = run(&group, &host, "kept = await gh.list(owner='o')", None).await;
    let checked = run(&group, &host, code, first.state).await;
    match rejected_at {
        Some(position) => {
            assert_eq!(
                outcome(&checked),
                Outcome::Rejected,
                "{:?}",
                output(&checked)
            );
            let diagnostic = output(&checked).diagnostic.as_deref().unwrap_or_default();
            assert!(diagnostic.contains(position), "{diagnostic}");
            assert_eq!(host.calls().len(), 1);
        }
        None => assert_eq!(
            outcome(&checked),
            Outcome::Completed,
            "{:?}",
            output(&checked)
        ),
    }
    let kept = run(&group, &host, "kept", checked.state).await;
    assert_ne!(result(&kept), &Value::Null);
    group.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_oversized_session_is_dropped_and_reported() {
    let Some(group) = group().await else { return };
    let host = Arc::new(FakeHost::default());
    let executed = run(&group, &host, "big = 'a' * 20_000_000\nlen(big)", None).await;
    assert_eq!(result(&executed), &json!(20_000_000));
    assert!(executed.state.is_none());
    assert!(matches!(
        executed.state_note,
        Some(StateNote::TooLarge { .. })
    ));
    group.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn calls_past_the_run_budget_raise_at_the_call_site() {
    let Some(group) = group().await else { return };
    let host = Arc::new(FakeHost::default());
    let code = format!("made = 0\n{}", awaited_calls(MAX_HOST_CALLS_PER_RUN + 5));
    let executed = run(&group, &host, &code, None).await;
    assert_eq!(result(&executed), &json!(MAX_HOST_CALLS_PER_RUN));
    assert_eq!(host.calls().len(), MAX_HOST_CALLS_PER_RUN);
    group.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restored_run_gets_a_fresh_call_budget_that_still_holds() {
    let Some(group) = group().await else { return };
    let host = Arc::new(FakeHost::default());
    let first = run(
        &group,
        &host,
        &format!("made = 0\n{}", awaited_calls(CALLS_BEFORE_RESTORE)),
        None,
    )
    .await;
    assert_eq!(result(&first), &json!(CALLS_BEFORE_RESTORE));

    let restored = run(
        &group,
        &host,
        &awaited_calls(MAX_HOST_CALLS_PER_RUN + 5),
        first.state,
    )
    .await;
    assert_eq!(restored.state_note, None);
    let made = CALLS_BEFORE_RESTORE + MAX_HOST_CALLS_PER_RUN;
    assert_eq!(result(&restored), &json!(made));
    assert_eq!(host.calls().len(), made);
    group.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn round_trips_past_the_isolated_cap_still_complete() {
    let Some(group) = group().await else { return };
    let host = Arc::new(FakeHost::default());
    let code = format!("len([search('q') for i in range({ROUND_TRIPS_PAST_THE_ISOLATED_CAP})])");
    let executed = run(&group, &host, &code, None).await;
    assert_eq!(result(&executed), &json!(ROUND_TRIPS_PAST_THE_ISOLATED_CAP));
    group.shutdown().await;
}

#[test_case(false ; "in_a_fresh_session")]
#[test_case(true ; "after_a_restore")]
#[tokio::test(flavor = "multi_thread")]
async fn too_many_round_trips_stop_the_run_as_limited(restored: bool) {
    let Some(group) = group().await else { return };
    let host = Arc::new(FakeHost::default());
    let prior = if restored {
        Some(
            run(&group, &host, "kept = 1", None)
                .await
                .state
                .expect("saved session"),
        )
    } else {
        None
    };
    let code = format!("for i in range({ROUND_TRIPS_PAST_THE_HOSTED_CAP}):\n    search('q')\n1");
    let executed = run(&group, &host, &code, prior.clone()).await;

    let limited = output(&executed);
    assert_eq!(limited.outcome, Outcome::Limited);
    assert!(limited.suspension_limit_exceeded);
    assert_eq!(
        limited.diagnostic.as_deref(),
        Some(HOSTED_SUSPENSION_LIMIT_MESSAGE)
    );
    assert_eq!(
        executed.state_note,
        restored.then_some(StateNote::RunDiscarded)
    );
    assert_eq!(
        executed.state.as_ref().map(HostedState::len),
        prior.as_ref().map(HostedState::len)
    );
    group.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn waiting_on_the_host_is_not_charged_to_the_time_limit() {
    let Some(group) = group().await else { return };
    let host = Arc::new(FakeHost::default());
    let executed = run_briefly(&group, &host, "await gh.slow()", None).await;

    let completed = output(&executed);
    assert_eq!(completed.outcome, Outcome::Completed, "{completed:?}");
    assert_eq!(completed.result, json!(SLOW_REPLY));
    assert!(Duration::from_millis(completed.timeout_ms) < HOST_DELAY);
    assert!(Duration::from_millis(completed.duration_ms) >= HOST_DELAY);
    group.shutdown().await;
}

#[test_case(false ; "in_a_fresh_session")]
#[test_case(true ; "after_a_restore")]
#[tokio::test(flavor = "multi_thread")]
async fn a_busy_loop_still_hits_the_time_limit(restored: bool) {
    let Some(group) = group().await else { return };
    let host = Arc::new(FakeHost::default());
    let prior = if restored {
        Some(
            run_briefly(&group, &host, "spins = 0", None)
                .await
                .state
                .expect("saved session"),
        )
    } else {
        None
    };
    let executed = run_briefly(
        &group,
        &host,
        "spins = 0\nwhile True:\n    spins += 1",
        prior,
    )
    .await;

    let limited = output(&executed);
    assert_eq!(limited.outcome, Outcome::Limited, "{limited:?}");
    assert!(limited.timed_out);
    assert_ne!(executed.state_note, Some(StateNote::RestoreFailed));
    group.shutdown().await;
}

#[test_case(false ; "restore_tightens_the_saved_limit")]
#[test_case(true ; "restore_keeps_a_stricter_saved_limit")]
#[tokio::test(flavor = "multi_thread")]
async fn changed_restore_limits_keep_the_stricter_time_budget(saved_short: bool) {
    let Some(group) = group().await else { return };
    let host = Arc::new(FakeHost::default());
    let first = if saved_short {
        run_briefly(&group, &host, "kept = 7", None).await
    } else {
        run(&group, &host, "kept = 7", None).await
    };
    let state = first.state.expect("saved session");
    let code = "while True:\n    pass";
    let executed = tokio::time::timeout(TIME_LIMIT_TEST_DEADLINE, async {
        if saved_short {
            run(&group, &host, code, Some(state)).await
        } else {
            run_briefly(&group, &host, code, Some(state)).await
        }
    })
    .await
    .expect("the stricter time limit must stop the restored run");
    let limited = output(&executed);
    assert_eq!(limited.outcome, Outcome::Limited, "{limited:?}");
    assert!(limited.timed_out);
    assert_ne!(executed.state_note, Some(StateNote::RestoreFailed));
    assert!(host.calls().is_empty());
    let after = run(&group, &host, "kept", executed.state).await;
    assert_eq!(result(&after), &json!(7));
    group.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_during_a_host_call_keeps_the_prior_session() {
    let Some(group) = group().await else { return };
    let host = Arc::new(FakeHost::default());
    let first = run(&group, &host, "kept = 7", None).await;
    let state = first.state.expect("saved session");

    let cancellation = CancellationToken::new();
    let prepared = group
        .prepare_hosted("kept = 8\nawait gh.hang()".to_owned())
        .expect("valid snippet");
    let trigger = cancellation.clone();
    let dyn_host: Arc<dyn CodeHost> = host.clone();
    // A run that ends without reaching the host returns an execution, failing the assert below.
    let executed = tokio::select! {
        executed = group.execute_hosted(prepared, dyn_host, Some(state), cancellation) => executed,
        () = async move {
            while host.calls().is_empty() {
                tokio::task::yield_now().await;
            }
            trigger.cancel();
            std::future::pending::<()>().await;
        } => unreachable!("waits forever once cancelled"),
    };
    assert!(executed.execution.is_none());
    let host = Arc::new(FakeHost::default());
    let after = run(&group, &host, "kept", executed.state).await;
    assert_eq!(result(&after), &json!(7));
    group.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn both_sleeps_wait_through_the_host() {
    let Some(group) = group().await else { return };
    let host = Arc::new(FakeHost::default());
    let code =
        "import asyncio\nimport time\ntime.sleep(2.5)\nawait asyncio.sleep(0.5, result='done')";
    let executed = run(&group, &host, code, None).await;
    assert_eq!(
        outcome(&executed),
        Outcome::Completed,
        "{:?}",
        output(&executed)
    );
    assert_eq!(result(&executed), &json!("done"));
    assert_eq!(
        host.slept(),
        [Duration::from_millis(500), Duration::from_millis(2_500)]
    );
    group.shutdown().await;
}

/// Sleeps take no host call slot, so they run beside the calls they are gathered with.
#[tokio::test(flavor = "multi_thread")]
async fn gathered_sleeps_run_beside_host_calls() {
    let Some(group) = group().await else { return };
    let host = Arc::new(FakeHost::default());
    let code = "import asyncio\nr = await asyncio.gather(asyncio.sleep(1), gh.list(owner='a'), asyncio.sleep(2))\nr[1]['owner']";
    let executed = run(&group, &host, code, None).await;
    assert_eq!(result(&executed), &json!("a"), "{:?}", output(&executed));
    assert_eq!(host.calls(), ["list"]);
    assert_eq!(
        host.slept(),
        [Duration::from_secs(1), Duration::from_secs(2)]
    );
    group.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_default_sleep_really_waits() {
    let Some(group) = group().await else { return };
    let prepared = group
        .prepare_hosted(format!(
            "import time\ntime.sleep({})\n1",
            REAL_SLEEP.as_secs_f64()
        ))
        .expect("valid snippet");
    let executed = group
        .execute_hosted(prepared, Arc::new(BareHost), None, CancellationToken::new())
        .await;
    let completed = output(&executed);
    assert_eq!(completed.outcome, Outcome::Completed, "{completed:?}");
    assert!(Duration::from_millis(completed.duration_ms) >= REAL_SLEEP);
    group.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn clocks_read_the_host_while_entropy_stays_refused() {
    let Some(group) = group().await else { return };
    let host = Arc::new(FakeHost::default());
    let clocks = "import time\nfrom datetime import date, datetime\n[time.time() > 1.7e9, time.monotonic() > 0, time.perf_counter() > 0, datetime.now().year > 2000, date.today().year > 2000]";
    let executed = run(&group, &host, clocks, None).await;
    assert_eq!(
        result(&executed),
        &json!([true, true, true, true, true]),
        "{:?}",
        output(&executed)
    );
    let entropy = run(&group, &host, "import random\nrandom.random()", None).await;
    let refused = output(&entropy);
    assert_eq!(refused.outcome, Outcome::Exception, "{refused:?}");
    assert_eq!(
        refused
            .exception
            .as_ref()
            .map(|exception| exception.r#type.as_str()),
        Some("RuntimeError")
    );
    group.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_sleep_stops_the_run() {
    let Some(group) = group().await else { return };
    let host = Arc::new(FakeHost::default());
    let cancellation = CancellationToken::new();
    let prepared = group
        .prepare_hosted(format!(
            "import asyncio\nawait asyncio.sleep({})",
            FOREVER.as_secs()
        ))
        .expect("valid snippet");
    let trigger = cancellation.clone();
    let dyn_host: Arc<dyn CodeHost> = host.clone();
    let executed = tokio::select! {
        executed = group.execute_hosted(prepared, dyn_host, None, cancellation) => executed,
        () = async move {
            while host.slept().is_empty() {
                tokio::task::yield_now().await;
            }
            trigger.cancel();
            std::future::pending::<()>().await;
        } => unreachable!("waits forever once cancelled"),
    };
    assert!(executed.execution.is_none());
    group.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unawaited_call_never_reaches_the_host() {
    let Some(group) = group().await else { return };
    let host = Arc::new(FakeHost::default());
    let executed = run(&group, &host, "pending = gh.hang()\n1", None).await;
    assert_eq!(result(&executed), &json!(1));
    assert!(host.calls().is_empty());
    group.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn isolated_execution_still_refuses_host_names() {
    let Some(group) = group().await else { return };
    let executed = group
        .execute(
            CodeInput {
                code: "gh.list()".to_owned(),
                timeout_sec: None,
            },
            CancellationToken::new(),
        )
        .await
        .expect("valid snippet")
        .expect("not cancelled");
    assert_ne!(executed.output.outcome, Outcome::Completed);
    group.shutdown().await;
}

/// A host that only runs hosted snippets builds one pool: plain computation runs on it, and an
/// isolated run is refused rather than borrowing it.
#[tokio::test(flavor = "multi_thread")]
async fn a_hosted_only_group_computes_and_refuses_isolated_runs() {
    let Some(worker) = worker() else { return };
    let group = CodeToolGroup::new(CodeConfiguration {
        worker: WorkerSource::Path(&worker),
        type_check: true,
        pools: CodePools::HostedOnly,
    })
    .await
    .expect("hosted pool");
    let host = Arc::new(FakeHost::default());

    let executed = run(&group, &host, "sum([1, 2, 3, 4])", None).await;
    assert_eq!(result(&executed), &json!(10));
    assert!(host.calls().is_empty());

    let isolated = group
        .execute(
            CodeInput {
                code: "1 + 1".to_owned(),
                timeout_sec: None,
            },
            CancellationToken::new(),
        )
        .await;
    assert!(isolated.is_err());
    group.shutdown().await;
}
