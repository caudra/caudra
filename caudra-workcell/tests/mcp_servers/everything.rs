//! `@modelcontextprotocol/server-everything`, the reference server: discovery, calls, kept
//! variables, resources, type checking, and the permission prompt.

use std::path::PathBuf;

use caudra_agent::AgentMode;
use caudra_agent::mcp::code::output::{McpCodeCallStatus, McpCodeOutcome};
use caudra_agent::permissions::{PermissionAnswer, PermissionMode};
use test_case::test_case;

use crate::support::{
    EVERYTHING, EVERYTHING_IMAGE, PLAN_FILE, PLAN_REFUSAL, Servers, completed, stdio,
    whole_tool_for_the_session,
};

const GET_SUM: &str = "get-sum";
const GET_SUM_KEY: &str = "everything.get-sum";
const GET_SUM_CALLED_AS: &str = "everything.get_sum";
const SUM: &str = "The sum of 2 and 3 is 5.";
const SECOND_SUM: &str = "The sum of 10 and 20 is 30.";
const CALL_SUM: &str = "await everything.get_sum(a=2, b=3)";
const DOCUMENT: &str = "demo://resource/static/document/architecture.md";
const PERMISSION_ERROR: &str = "PermissionError";
const GATHER_SUMS: &str =
    "await asyncio.gather(everything.get_sum(a=2, b=3), everything.get_sum(a=10, b=20))";
const IMPORTED_ASYNCIO: &str = "imported `asyncio`";
const UNAWAITED_SEARCH: &str = "dropped `await` before `search()`";
const MEANT_GET_SUM: &str = "Did you mean `get_sum`?";
const TRUE: &str = "true";
const DROPPED_RETURN: &str = "dropped `return` from the last line";
// MONTY-GATHER-SHIM: gone once Monty's `gather` takes `return_exceptions`.
const SETTLED: &str = "ran `asyncio.gather` without `return_exceptions`";
const GATHER_SETTLED: &str = "import asyncio\n\
     results = await asyncio.gather(everything.get_sum(a=2, b=3), everything.get_sum(a=10, b=20), return_exceptions=True)\n\
     [type(r).__name__ if isinstance(r, Exception) else r for r in results]";

fn everything(mode: AgentMode, permission_mode: PermissionMode) -> Servers {
    Servers::connect(
        vec![stdio(EVERYTHING, EVERYTHING_IMAGE)],
        mode,
        permission_mode,
    )
}

fn building() -> Servers {
    everything(AgentMode::Build, PermissionMode::Yolo)
}

/// Whichever protocol era the server speaks, the connection reaches its tools.
#[test]
#[ignore = "needs Docker: make test-mcp-servers"]
fn server_everything_works_in_whichever_era_it_speaks() {
    let servers = building();
    let output = servers.run("await everything.echo(message='everything')");
    assert!(completed(&output).contains(EVERYTHING), "{output:?}");
}

#[test]
#[ignore = "needs Docker: make test-mcp-servers"]
fn the_helpers_find_what_a_snippet_can_call() {
    let servers = building();
    let listed = servers.run("servers()");
    assert!(completed(&listed).contains(EVERYTHING), "{listed:?}");
    let found = servers.run("search('sum', server='everything')");
    assert!(completed(&found).contains(GET_SUM_CALLED_AS), "{found:?}");
    assert!(found.calls.is_empty(), "{:?}", found.calls);
    let awaited = servers.run("await search('sum', server='everything')");
    assert!(
        completed(&awaited).contains(GET_SUM_CALLED_AS),
        "{awaited:?}"
    );
    assert_eq!(awaited.repairs, [UNAWAITED_SEARCH]);
}

#[test]
#[ignore = "needs Docker: make test-mcp-servers"]
fn a_call_answers_with_the_server_result_and_is_recorded_as_written() {
    let servers = building();
    let output = servers.run(CALL_SUM);
    assert_eq!(completed(&output), SUM);
    let [call] = output.calls.as_slice() else {
        panic!("{:?}", output.calls);
    };
    assert_eq!(call.server, EVERYTHING);
    assert_eq!(call.tool, GET_SUM);
    assert_eq!(call.called_as, GET_SUM_CALLED_AS);
    assert_eq!(call.status, McpCodeCallStatus::Success);
}

/// `asyncio` used without its import is imported before the snippet runs.
#[test_case("import asyncio\n", &[] ; "imported")]
#[test_case("", &[IMPORTED_ASYNCIO] ; "unimported")]
#[ignore = "needs Docker: make test-mcp-servers"]
fn gathered_calls_all_answer(prelude: &str, repairs: &[&str]) {
    let servers = building();
    let output = servers.run(&format!("{prelude}{GATHER_SUMS}"));
    let result = completed(&output);
    assert!(
        result.contains(SUM) && result.contains(SECOND_SUM),
        "{result}"
    );
    assert_eq!(output.calls.len(), 2);
    assert_eq!(output.repairs, repairs);
}

#[test]
#[ignore = "needs Docker: make test-mcp-servers"]
fn a_tool_takes_one_dict_of_arguments() {
    let output = building().run("await everything.get_sum({'a': 2, 'b': 3})");
    assert_eq!(completed(&output), SUM);
}

/// A snippet that ends on `return` runs as though it ended on the value, and the model is told why.
#[test]
#[ignore = "needs Docker: make test-mcp-servers"]
fn a_final_return_is_dropped_and_explained() {
    let output = building().run(&format!("return {CALL_SUM}"));
    assert_eq!(completed(&output), SUM);
    assert_eq!(output.repairs, [DROPPED_RETURN]);
    assert_eq!(output.lessons.len(), 1, "{output:?}");
}

/// With `return_exceptions=True` a denied call settles as its error, and the other still answers.
/// The repair is MONTY-GATHER-SHIM; without the shim it must pass with no repair listed.
#[test]
#[ignore = "needs Docker: make test-mcp-servers"]
fn a_denied_call_in_a_settled_gather_leaves_the_other_answer() {
    let servers = everything(AgentMode::Build, PermissionMode::Ask);
    let (output, asked) = servers.run_answering(
        GATHER_SETTLED,
        &[PermissionAnswer::AllowOnce, PermissionAnswer::Deny],
    );
    assert_eq!(asked.len(), 2);
    let result = completed(&output);
    assert!(result.contains(PERMISSION_ERROR), "{result}");
    assert!(
        result.contains(SUM) != result.contains(SECOND_SUM),
        "{result}"
    );
    assert_eq!(output.repairs, [SETTLED]);
    assert!(output.lessons.is_empty(), "{output:?}");
}

#[test]
#[ignore = "needs Docker: make test-mcp-servers"]
fn variables_last_between_runs_until_a_reset() {
    let servers = building();
    completed(&servers.run(&format!("kept = {CALL_SUM}")));
    assert_eq!(completed(&servers.run("kept")), SUM);
    let reset = servers.reset_and_run("kept");
    assert!(
        matches!(reset.outcome, McpCodeOutcome::Rejected { .. }),
        "{reset:?}"
    );
}

#[test]
#[ignore = "needs Docker: make test-mcp-servers"]
fn resources_are_listed_and_read() {
    let servers = building();
    let output = servers.run(&format!(
        "listed = await resources('everything')\n\
         read = await read_resource('everything', '{DOCUMENT}')\n\
         [entry['uri'] for entry in listed if entry['uri'] == '{DOCUMENT}'] == [read[0]['uri']] and len(read[0]['text']) > 0"
    ));
    assert_eq!(completed(&output), TRUE);
}

/// A snippet that cannot type-check never runs, and a misspelt tool is pointed at the real one.
#[test]
#[ignore = "needs Docker: make test-mcp-servers"]
fn a_misspelt_tool_never_runs_and_names_the_real_one() {
    let servers = building();
    let output = servers.run("await everything.get_summ(a=2, b=3)");
    let McpCodeOutcome::Rejected { message } = &output.outcome else {
        panic!("{output:?}");
    };
    assert!(message.contains(MEANT_GET_SUM), "{message}");
    assert!(output.calls.is_empty());
}

#[test_case(PermissionAnswer::AllowOnce, McpCodeCallStatus::Success ; "allowed")]
#[test_case(PermissionAnswer::Deny, McpCodeCallStatus::Refused ; "denied")]
#[ignore = "needs Docker: make test-mcp-servers"]
fn each_call_asks_and_a_denial_raises_permission_error(
    answer: PermissionAnswer,
    status: McpCodeCallStatus,
) {
    let servers = everything(AgentMode::Build, PermissionMode::Ask);
    let (output, asked) = servers.run_answering(CALL_SUM, &[answer]);
    let [request] = asked.as_slice() else {
        panic!("{asked:?}");
    };
    assert_eq!(request.tool.to_string(), GET_SUM_KEY);
    assert_eq!(output.calls[0].status, status);
    match status {
        McpCodeCallStatus::Success => assert_eq!(completed(&output), SUM),
        _ => assert!(
            matches!(&output.outcome, McpCodeOutcome::Raised { kind, .. } if kind == PERMISSION_ERROR),
            "{output:?}"
        ),
    }
}

#[test]
#[ignore = "needs Docker: make test-mcp-servers"]
fn allowing_for_the_session_asks_once() {
    let servers = everything(AgentMode::Build, PermissionMode::Ask);
    let (output, asked) = servers.run_answering(
        &format!("{CALL_SUM}\nawait everything.get_sum(a=10, b=20)"),
        &[whole_tool_for_the_session()],
    );
    assert_eq!(completed(&output), SECOND_SUM);
    assert_eq!(asked.len(), 1);
    let (again, asked_again) = servers.run_answering(CALL_SUM, &[]);
    assert_eq!(completed(&again), SUM);
    assert!(asked_again.is_empty());
}

/// Plan mode trusts a read-only annotation only from a server Caudra recognises, and this one
/// it does not.
#[test]
#[ignore = "needs Docker: make test-mcp-servers"]
fn plan_mode_refuses_an_unrecognised_server() {
    let servers = everything(
        AgentMode::Plan(PathBuf::from(PLAN_FILE)),
        PermissionMode::Yolo,
    );
    let output = servers.run(CALL_SUM);
    let McpCodeOutcome::Raised { kind, message } = &output.outcome else {
        panic!("{output:?}");
    };
    assert_eq!(kind, PERMISSION_ERROR);
    assert!(message.contains(PLAN_REFUSAL), "{message}");
    assert_eq!(output.calls[0].status, McpCodeCallStatus::Refused);
}
