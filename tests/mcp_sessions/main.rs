//! `caudra -p` sessions that reach server-everything through the `mcp` tool: the SDK permission
//! flow, variables kept between turns, and a resumed session. Every test is ignored;
//! `make test-mcp-servers` builds the server image and runs them.
#![cfg(unix)]

mod sdk;

use sdk::{Home, Reply};
use test_case::test_case;

const ASK: &str = "default";
const YOLO: &str = "bypassPermissions";
const SUCCESS: &str = "success";
const GET_SUM_KEY: &str = "everything.get-sum";
const SUM: &str = "The sum of 2 and 3 is 5.";
const TRUE: &str = "true";
const PERMISSION_ERROR: &str = "PermissionError";
const RESTARTED: &str = "were lost when Caudra restarted";
const CALL_SUM: &str = "await everything.get_sum(a=2, b=3)";

fn run(code: &str) -> String {
    format!(
        "Call the `mcp` tool once, with exactly this Python as its `code`, then reply with only the word done.\n\n```python\n{code}\n```"
    )
}

#[test_case("allowed_call", Reply::Allow, SUM ; "allowed")]
#[test_case("denied_call", Reply::Deny, PERMISSION_ERROR ; "denied")]
#[ignore = "needs Docker: make test-mcp-servers"]
fn an_sdk_client_answers_each_server_call(cassette: &str, reply: Reply, result: &str) {
    let home = Home::new(cassette);
    let mut session = home.start(ASK, None);
    let turn = session.turn(&run(CALL_SUM), &[reply]);
    session.finish();
    assert_eq!(turn.asked, [GET_SUM_KEY]);
    assert!(turn.mentions(result), "{turn:?}");
    assert_eq!(turn.outcome, SUCCESS);
}

/// An SDK rule allows the call it answered, so the same call made again runs unasked.
#[test]
#[ignore = "needs Docker: make test-mcp-servers"]
fn a_session_rule_from_the_sdk_covers_the_same_call_again() {
    let home = Home::new("session_rule");
    let mut session = home.start(ASK, None);
    let turn = session.turn(
        &run(&format!(
            "first = {CALL_SUM}\nsecond = {CALL_SUM}\nfirst == second"
        )),
        &[Reply::AllowForSession],
    );
    session.finish();
    assert_eq!(turn.asked, [GET_SUM_KEY]);
    assert!(turn.mentions(TRUE), "{turn:?}");
}

#[test]
#[ignore = "needs Docker: make test-mcp-servers"]
fn variables_last_between_turns_and_a_resumed_session_says_they_were_lost() {
    let home = Home::new("kept_variables");
    let mut first = home.start(YOLO, None);
    first.turn(&run(&format!("kept = {CALL_SUM}")), &[]);
    let kept = first.turn(&run("kept"), &[]);
    let id = first.finish();
    assert!(kept.mentions(SUM), "{kept:?}");

    let mut resumed = home.start(YOLO, Some(&id));
    let after = resumed.turn(&run("1 + 1"), &[]);
    assert_eq!(resumed.finish(), id);
    assert!(after.mentions(RESTARTED), "{after:?}");
}
