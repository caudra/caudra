//! The wire contract ACP clients actually parse, pinned so a refactor cannot
//! quietly break an integration that lives in another repository.
//!
//! Each assertion here stands for a client behaviour observed in the wild, not
//! for a line of the spec: clients read a narrower set of fields than the schema
//! offers, and the ones they read are the ones that must not move.

use agent_client_protocol_schema::v1::{PermissionOptionKind, PromptResponse};
use caudra_agent::DoneReason;
use caudra_providers::ThinkingConfig;
use caudra_providers::model::Model;
use caudra_storage::sessions::PermissionMode;
use serde_json::{Value, json};
use test_case::test_case;

use crate::auth::{AUTH_METHOD_ID, META_TERMINAL_AUTH, auth_method};
use crate::catalog::ModelCatalog;
use crate::methods::SessionSettings;
use crate::test_support::{LEVELS, OFFLINE_SPEC, SELECTED_SPEC, effort_model};
use crate::turn::{META_DURATION_MS, META_PROMPT_ID, META_USAGE, PromptIdentity};
use crate::{methods, permissions, translate};

const PROMPT_ID: &str = "prompt-under-test";

/// A client that cannot find a string session id has no session to prompt.
const SESSION_ID_FIELD: &str = "sessionId";
/// The prompt result is the only settle signal a client gets; without a stop
/// reason a finished turn is indistinguishable from a hung one.
const STOP_REASON_FIELD: &str = "stopReason";
const END_TURN: &str = "end_turn";
/// Clients pick permission options by kind, not by position.
const ALLOW_PREFIX: &str = "allow";
const REJECT_PREFIX: &str = "reject";
/// Clients find the model and effort pickers by category, not by id.
const CATEGORY_FIELD: &str = "category";

fn to_json<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("protocol types must serialize")
}

fn initialize_json_for(terminal_auth: bool) -> Value {
    let catalog = ModelCatalog::new(
        &[OFFLINE_SPEC.to_owned()],
        &Model::from_spec(SELECTED_SPEC).unwrap(),
    );
    to_json(&methods::initialize_response(
        auth_method(terminal_auth),
        to_json(&catalog),
    ))
}

fn initialize_json() -> Value {
    initialize_json_for(false)
}

#[test]
fn initialize_advertises_protocol_version_one() {
    let json = initialize_json();

    assert_eq!(json["protocolVersion"], json!(1));
    assert_eq!(json["agentInfo"]["name"], json!("caudra"));
    assert_eq!(json["agentInfo"]["title"], json!("Caudra"));
}

/// Zed offers sign-in only for a method it can run: a `terminal` one on a
/// client that runs them, else the `_meta["terminal-auth"]` command.
#[test_case(true ; "terminal_auth_client")]
#[test_case(false ; "legacy_client")]
fn initialize_offers_one_login_method(terminal: bool) {
    let json = initialize_json_for(terminal);
    let methods = json["authMethods"]
        .as_array()
        .expect("authMethods is a list");

    assert_eq!(methods.len(), 1);
    assert_eq!(methods[0]["id"], AUTH_METHOD_ID);
    assert_eq!(methods[0]["type"].is_string(), terminal);
    assert_eq!(
        methods[0]["_meta"][META_TERMINAL_AUTH]["args"].is_array(),
        !terminal
    );
}

/// A client fills its model picker before any session exists from
/// `_meta.caudra`, reading each model by these fields.
#[test]
fn initialize_carries_the_model_catalog() {
    let json = initialize_json();
    let catalog = &json["_meta"][translate::META_NAMESPACE];

    assert_eq!(catalog["defaultModel"], SELECTED_SPEC);
    let models = catalog["models"].as_array().expect("models is a list");
    assert_eq!(
        models.len(),
        2,
        "the default leads the list it is missing from"
    );
    for model in models {
        assert!(model["id"].is_string(), "{model}");
        assert!(model["name"].is_string(), "{model}");
        assert!(model["provider"].is_string(), "{model}");
        assert!(model["contextWindow"].is_u64(), "{model}");
        assert!(model["efforts"].is_array(), "{model}");
        assert!(model["fast"].is_boolean(), "{model}");
    }
}

/// `loadSession` is what lets a client resume a thread instead of starting a new
/// one per turn, and the mcp capabilities decide which injected servers it sends.
#[test]
fn initialize_advertises_the_capabilities_clients_gate_on() {
    let json = initialize_json();
    let caps = &json["agentCapabilities"];

    assert_eq!(caps["loadSession"], json!(true));
    assert_eq!(caps["promptCapabilities"]["image"], json!(true));
    assert_eq!(caps["mcpCapabilities"]["http"], json!(true));
}

/// A client offers a session picker, resume, close, delete, and fork only when
/// the matching capability is present. `additionalDirectories` stays absent
/// because Caudra grants a session no root beyond its cwd.
#[test_case("list" => true ; "list")]
#[test_case("resume" => true ; "resume")]
#[test_case("close" => true ; "close")]
#[test_case("delete" => true ; "delete")]
#[test_case("fork" => true ; "fork")]
#[test_case("additionalDirectories" => false ; "additional_directories")]
fn initialize_advertises_session_lifecycle(capability: &str) -> bool {
    let json = initialize_json();
    json["agentCapabilities"]["sessionCapabilities"][capability].is_object()
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
#[test_case(DoneReason::Refusal, "refusal" ; "refusal")]
fn prompt_response_carries_a_stop_reason(reason: DoneReason, expected: &str) {
    let json = to_json(&answer(reason));

    assert_eq!(json[STOP_REASON_FIELD], json!(expected));
}

fn answer(reason: DoneReason) -> PromptResponse {
    translate::TurnSpend::default().into_response(
        translate::map_done_reason(reason),
        &PromptIdentity::new(PROMPT_ID.to_owned()),
    )
}

/// Token accounting reaches clients two ways, because `usage` rides an unstable
/// schema feature while `_meta` is stable extensibility. Dropping either one
/// silently zeroes somebody's spend ledger.
#[test]
fn prompt_response_reports_usage_in_both_places() {
    let json = to_json(&answer(DoneReason::EndTurn));

    let caudra = &json["_meta"][translate::META_NAMESPACE];
    assert_eq!(caudra[META_PROMPT_ID], PROMPT_ID);
    assert!(caudra[META_DURATION_MS].is_u64(), "no durationMs: {json}");
    for source in [&json["usage"], &json["_meta"], &caudra[META_USAGE]] {
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

fn config_options_json() -> Value {
    let model = effort_model(&LEVELS, true);
    to_json(&methods::config_options(&SessionSettings {
        model: &model,
        model_specs: &[OFFLINE_SPEC.to_owned()],
        thinking: &ThinkingConfig::Off,
        fast: false,
        mode_id: methods::MODE_BUILD,
        permission_mode: &PermissionMode::Ask,
        boolean_options: false,
    }))
}

fn config_option(id: &str) -> Value {
    config_options_json()
        .as_array()
        .expect("config options are a list")
        .iter()
        .find(|option| option["id"] == id)
        .cloned()
        .unwrap_or_else(|| panic!("no {id} option"))
}

/// Orca picks the model selector by `category: "model"` and the effort
/// selector by `"thought_level"`; Zed places the mode selector by `"mode"`.
#[test_case(methods::MODEL_CONFIG_ID => json!("model") ; "model")]
#[test_case(methods::EFFORT_CONFIG_ID => json!("thought_level") ; "effort")]
#[test_case(methods::MODE_CONFIG_ID => json!("mode") ; "mode")]
#[test_case(methods::PERMISSIONS_CONFIG_ID => Value::Null ; "permissions")]
fn config_options_carry_the_category_clients_find_them_by(id: &str) -> Value {
    config_option(id)[CATEGORY_FIELD].clone()
}

/// Every option is a select whose current value is one of its own values, or
/// a client renders a picker with nothing chosen and sets back a value it
/// made up.
#[test_case(methods::EFFORT_CONFIG_ID ; "effort")]
#[test_case(methods::MODE_CONFIG_ID ; "mode")]
#[test_case(methods::PERMISSIONS_CONFIG_ID ; "permissions")]
fn a_select_offers_its_current_value(id: &str) {
    let option = config_option(id);
    assert_eq!(option["type"], "select");
    let values: Vec<&Value> = option["options"]
        .as_array()
        .expect("a select lists its values")
        .iter()
        .map(|choice| &choice["value"])
        .collect();
    assert!(values.contains(&&option["currentValue"]), "{option}");
}

/// Model choices arrive grouped under a provider header, each value the
/// `provider/model` spec a client sends back.
#[test]
fn the_model_select_is_grouped_by_provider() {
    let option = config_option(methods::MODEL_CONFIG_ID);
    let groups = option["options"].as_array().expect("groups are a list");
    for group in groups {
        assert!(group["group"].is_string(), "{group}");
        assert!(group["name"].is_string(), "{group}");
        for choice in group["options"]
            .as_array()
            .expect("a group lists its values")
        {
            let spec = choice["value"].as_str().expect("a spec value");
            assert!(spec.starts_with(group["group"].as_str().unwrap()), "{spec}");
        }
    }
}
