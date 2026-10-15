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
    CodeConfiguration, CodeHost, CodeInput, CodeOutput, CodeToolGroup, HostCall, HostError,
    HostErrorKind, HostInvocation, HostName, HostedExecution, HostedState, MAX_HOST_CALLS_PER_RUN,
    Outcome, PreparedCode, Repair, StateNote, WORKER_FILE_NAME, WorkerSource,
};

const NAMESPACE: &str = "gh";
const DENIED: &str = "denied by the user";
const SLOW_REPLY: &str = "answered late";
const STUBS: &str = "from typing import Any\nclass _Gh:\n    async def list(self, *, owner: str = '') -> dict[str, Any]: ...\n    async def boom(self) -> Any: ...\n    async def hang(self) -> Any: ...\n    async def slow(self) -> str: ...\ngh: _Gh\ndef search(query: str) -> list[str]: ...\n";
const HOSTED_SUSPENSION_LIMIT_MESSAGE: &str = "The snippet made too many host round trips and was stopped. Batch work into fewer calls, and check for an undefined name read inside a loop.";
const SHORT_TIMEOUT_SECS: u64 = 1;
/// Well past the short time limit, so a run charged for the wait could not finish within it.
const HOST_DELAY: Duration = Duration::from_secs(3);
/// Past the isolated cap of 256 round trips, yet within the hosted cap even at two per call.
const ROUND_TRIPS_PAST_THE_ISOLATED_CAP: usize = 300;
/// Each call is at least one round trip, so this overruns the hosted cap of 1,024.
const ROUND_TRIPS_PAST_THE_HOSTED_CAP: usize = 2_000;
const CALLS_BEFORE_RESTORE: usize = 60;
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
    let worker = worker()?;
    Some(
        CodeToolGroup::new(CodeConfiguration {
            worker: WorkerSource::Path(&worker),
            type_check: true,
        })
        .await
        .expect("worker pool"),
    )
}

/// Records each deferred call when it starts, which is when the snippet first awaits it.
#[derive(Default)]
struct FakeHost {
    calls: Arc<Mutex<Vec<String>>>,
}

impl FakeHost {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls").clone()
    }
}

impl CodeHost for FakeHost {
    fn lookup(&self, namespace: Option<&str>, name: &str) -> HostName {
        match (namespace, name) {
            (None, NAMESPACE) => HostName::Namespace,
            (None, "search") | (Some(NAMESPACE), "list" | "boom" | "hang" | "slow") => {
                HostName::Function
            }
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
                "boom" => Err(HostError::new(HostErrorKind::Permission, DENIED)),
                "hang" => std::future::pending().await,
                "slow" => {
                    tokio::time::sleep(HOST_DELAY).await;
                    Ok(json!(SLOW_REPLY))
                }
                _ => {
                    Ok(json!({ "owner": call.kwargs.get("owner").cloned().unwrap_or(Value::Null) }))
                }
            }
        }))
    }

    fn stubs(&self) -> String {
        STUBS.to_owned()
    }
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

/// A run on a restored session is checked on its own: a tool the server lacks or an argument of
/// the wrong type is refused before any call, at the snippet's own line and column, and the session
/// is kept. Names from earlier runs stay unknown to that check, so rebinding one still runs.
#[test_case("kept = kept['owner']\nawait gh.nope()", Some("snippet.py:2:7:") ; "a_tool_the_server_lacks")]
#[test_case("await gh.list(owner=kept)\nawait gh.list(owner=1)", Some("snippet.py:2:") ; "an_argument_of_the_wrong_type")]
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
