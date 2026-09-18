//! The wire contract ACP clients actually parse, pinned so a refactor cannot
//! quietly break an integration that lives in another repository.
//!
//! Each assertion here stands for a client behaviour observed in the wild, not
//! for a line of the spec: clients read a narrower set of fields than the schema
//! offers, and the ones they read are the ones that must not move.

use agent_client_protocol_schema::v1::PermissionOptionKind;
use caudra_agent::DoneReason;
use serde_json::{Value, json};
use test_case::test_case;

use crate::{methods, permissions, translate};

/// A client that cannot find a string session id has no session to prompt.
const SESSION_ID_FIELD: &str = "sessionId";
/// The prompt result is the only settle signal a client gets; without a stop
/// reason a finished turn is indistinguishable from a hung one.
const STOP_REASON_FIELD: &str = "stopReason";
const END_TURN: &str = "end_turn";
/// Clients pick permission options by kind, not by position.
const ALLOW_PREFIX: &str = "allow";
const REJECT_PREFIX: &str = "reject";

fn to_json<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("protocol types must serialize")
}

#[test]
fn initialize_advertises_protocol_version_one() {
    let json = to_json(&methods::initialize_response());

    assert_eq!(json["protocolVersion"], json!(1));
    assert_eq!(json["agentInfo"]["name"], json!("caudra"));
}

/// `loadSession` is what lets a client resume a thread instead of starting a new
/// one per turn, and the mcp capabilities decide which injected servers it sends.
#[test]
fn initialize_advertises_the_capabilities_clients_gate_on() {
    let json = to_json(&methods::initialize_response());
    let caps = &json["agentCapabilities"];

    assert_eq!(caps["loadSession"], json!(true));
    assert_eq!(caps["promptCapabilities"]["image"], json!(true));
    assert_eq!(caps["mcpCapabilities"]["http"], json!(true));
}

#[test]
fn new_session_returns_a_string_session_id() {
    let json = to_json(&methods::new_session_response("sess_1"));

    assert_eq!(json[SESSION_ID_FIELD], json!("sess_1"));
    assert!(
        json[SESSION_ID_FIELD].is_string(),
        "a non-string session id strands every later request"
    );
}

#[test_case(DoneReason::EndTurn, END_TURN ; "end_turn")]
#[test_case(DoneReason::Cancelled, "cancelled" ; "cancelled")]
#[test_case(DoneReason::MaxTokens, "max_tokens" ; "max_tokens")]
#[test_case(DoneReason::MaxTurns, "max_turn_requests" ; "max_turn_requests")]
fn prompt_response_carries_a_stop_reason(reason: DoneReason, expected: &str) {
    let json = to_json(&translate::TurnSpend::default().into_response(reason));

    assert_eq!(json[STOP_REASON_FIELD], json!(expected));
}

/// Token accounting reaches clients two ways, because `usage` rides an unstable
/// schema feature while `_meta` is stable extensibility. Dropping either one
/// silently zeroes somebody's spend ledger.
#[test]
fn prompt_response_reports_usage_in_both_places() {
    let json = to_json(&translate::TurnSpend::default().into_response(DoneReason::EndTurn));

    for source in [&json["usage"], &json["_meta"]] {
        assert!(source.is_object(), "missing usage source in {json}");
        assert!(source["inputTokens"].is_number(), "no inputTokens: {json}");
        assert!(
            source["outputTokens"].is_number(),
            "no outputTokens: {json}"
        );
    }
}

/// A client matches options by an `allow`/`reject` kind prefix and refuses to
/// guess when neither is present, so every option needs both fields.
#[test]
fn permission_options_are_matchable_by_id_and_kind() {
    let options = permissions::permission_options();
    let json = to_json(&options);

    for option in json.as_array().expect("options are a list") {
        let id = option["optionId"].as_str().expect("option needs an id");
        assert!(!id.is_empty(), "empty option id in {option}");
        let kind = option["kind"].as_str().expect("option needs a kind");
        assert!(
            kind.starts_with(ALLOW_PREFIX) || kind.starts_with(REJECT_PREFIX),
            "unmatchable option kind {kind}"
        );
        assert!(
            !option["name"].as_str().unwrap_or_default().is_empty(),
            "option needs a label in {option}"
        );
    }

    let kinds: Vec<_> = options.iter().map(|o| o.kind).collect();
    assert!(kinds.contains(&PermissionOptionKind::AllowOnce));
    assert!(kinds.contains(&PermissionOptionKind::RejectOnce));
    assert!(
        kinds.contains(&PermissionOptionKind::AllowAlways),
        "without allow_always a client invents its own session-wide allow"
    );
}
