//! Servers Caudra knows by name. Recognition reads the name a server gives
//! itself, which anyone can claim, so it mostly adds care: a prompt that
//! always asks, a card that reads the server's own result shape. The one
//! thing it lends is trust in a `readOnlyHint`, which keeps such a tool in
//! plan mode and read-only policies. Even then the call is still checked the
//! way a stranger's would be.

use std::collections::VecDeque;
use std::slice;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

const HOME_ASSISTANT_NAME: &str = "home-assistant";
const EXECUTOR_NAME: &str = "executor";
const HOME_ASSISTANT_LABEL: &str = "Home Assistant";
const EXECUTOR_LABEL: &str = "Executor";
/// Home Assistant domains whose devices guard a home: a call naming one
/// always asks, whatever the rules or the mode say.
pub const SENSITIVE_DOMAINS: &[&str] = &["lock", "alarm_control_panel", "cover", "valve"];
/// How many paused Executor runs one connection remembers.
pub const MAX_PAUSED_RUNS: usize = 64;
const DOMAIN_ARGUMENT: &str = "domain";
const SENSITIVE_NOTE_PREFIX: &str = "Home Assistant ";
const SENSITIVE_NOTE_SUFFIX: &str = " devices always ask";
const RESUME_TOOL: &str = "resume";
const EXECUTION_ID_ARGUMENT: &str = "executionId";
const ACTION_ARGUMENT: &str = "action";
const PERSIST_ARGUMENT: &str = "persist";
const ACCEPT_ACTION: &str = "accept";
const STATUS_FIELD: &str = "status";
const WAITING_STATUS: &str = "waiting_for_interaction";
const EXPIRES_FIELD: &str = "expiresAt";
const INTERACTION_FIELD: &str = "interaction";
const MESSAGE_FIELD: &str = "message";
const ADDRESS_FIELD: &str = "address";
const ARGS_FIELD: &str = "args";
const WAITING_NOTE_PREFIX: &str = "Executor run waiting for: ";
const NOTE_SEPARATOR: &str = " · ";
const REMEMBER_PREFIX: &str = "remember: ";
const EXPIRES_PREFIX: &str = "expires ";
const UNKNOWN_RUN_PREFIX: &str = "Executor run ";
const UNKNOWN_RUN_SUFFIX: &str = ": not a run Caudra saw pause";
const MAX_MESSAGE_CHARS: usize = 200;
const MAX_ARGS_CHARS: usize = 120;
const MAX_FIELD_CHARS: usize = 80;
const CUT_MARK: char = '…';

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpServerKind {
    HomeAssistant,
    Executor,
}

impl McpServerKind {
    pub fn from_server_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            HOME_ASSISTANT_NAME => Some(Self::HomeAssistant),
            EXECUTOR_NAME => Some(Self::Executor),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::HomeAssistant => HOME_ASSISTANT_LABEL,
            Self::Executor => EXECUTOR_LABEL,
        }
    }

    /// Why a call to this server's `tool` with `input` must always ask, or
    /// `None` when normal policy decides.
    pub fn always_asks(self, tool: &str, input: &Value, paused: &PausedRuns) -> Option<String> {
        match self {
            Self::HomeAssistant => sensitive_domain(input)
                .map(|domain| format!("{SENSITIVE_NOTE_PREFIX}{domain}{SENSITIVE_NOTE_SUFFIX}")),
            Self::Executor => (tool == RESUME_TOOL && accepts(input)).then(|| paused.note(input)),
        }
    }
}

/// What a permission prompt knows about one call to a recognised server.
/// Display and risk only: the rule a grant files never depends on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpCallFacts {
    pub kind: McpServerKind,
    /// The server annotated the tool `readOnlyHint`.
    #[serde(default)]
    pub read_only: bool,
    /// Why the call always asks. Present exactly when it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl McpCallFacts {
    pub fn always_asks(&self) -> bool {
        self.note.is_some()
    }
}

/// The first of [`SENSITIVE_DOMAINS`] a Home Assistant call's `domain`
/// argument names. Intents take an array of domains, other tools a string.
pub fn sensitive_domain(input: &Value) -> Option<&'static str> {
    let named = match input.get(DOMAIN_ARGUMENT)? {
        Value::Array(domains) => domains.as_slice(),
        domain => slice::from_ref(domain),
    };
    named.iter().filter_map(Value::as_str).find_map(|domain| {
        SENSITIVE_DOMAINS
            .iter()
            .copied()
            .find(|sensitive| domain.trim().eq_ignore_ascii_case(sensitive))
    })
}

fn accepts(input: &Value) -> bool {
    input
        .get(ACTION_ARGUMENT)
        .and_then(Value::as_str)
        .is_some_and(|action| action.trim().eq_ignore_ascii_case(ACCEPT_ACTION))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PausedRun {
    message: String,
    address: String,
    args: String,
    expires_at: Option<String>,
}

/// The runs an Executor server said are waiting for an answer, oldest first,
/// so a prompt to `resume` one can say what it approves. Held per connection
/// and dropped with it.
#[derive(Debug, Default)]
pub struct PausedRuns(Mutex<VecDeque<(String, PausedRun)>>);

impl PausedRuns {
    /// Remembers the run a result's structured content says is paused,
    /// forgetting the oldest past [`MAX_PAUSED_RUNS`].
    pub fn record(&self, structured: &Value) {
        if structured.get(STATUS_FIELD).and_then(Value::as_str) != Some(WAITING_STATUS) {
            return;
        }
        let Some(id) = structured
            .get(EXECUTION_ID_ARGUMENT)
            .and_then(Value::as_str)
        else {
            return;
        };
        let interaction = &structured[INTERACTION_FIELD];
        let text = |field: &str, max: usize| {
            one_line(interaction[field].as_str().unwrap_or_default(), max)
        };
        let run = PausedRun {
            message: text(MESSAGE_FIELD, MAX_MESSAGE_CHARS),
            address: text(ADDRESS_FIELD, MAX_FIELD_CHARS),
            args: match &interaction[ARGS_FIELD] {
                Value::Null => String::new(),
                args => one_line(&args.to_string(), MAX_ARGS_CHARS),
            },
            expires_at: structured
                .get(EXPIRES_FIELD)
                .and_then(Value::as_str)
                .map(|expires| one_line(expires, MAX_FIELD_CHARS)),
        };
        let mut runs = self.0.lock().unwrap_or_else(|error| error.into_inner());
        runs.retain(|(known, _)| known != id);
        if runs.len() == MAX_PAUSED_RUNS {
            runs.pop_front();
        }
        runs.push_back((id.to_owned(), run));
    }

    /// What accepting the run `input` names approves, as a prompt says it.
    fn note(&self, input: &Value) -> String {
        let id = input
            .get(EXECUTION_ID_ARGUMENT)
            .and_then(Value::as_str)
            .unwrap_or_default();
        let runs = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let Some((_, run)) = runs.iter().find(|(known, _)| known == id) else {
            return format!(
                "{UNKNOWN_RUN_PREFIX}{}{UNKNOWN_RUN_SUFFIX}",
                one_line(id, MAX_FIELD_CHARS)
            );
        };
        let mut note = format!(
            "{WAITING_NOTE_PREFIX}{}{NOTE_SEPARATOR}{}({})",
            run.message, run.address, run.args
        );
        if let Some(persist) = input.get(PERSIST_ARGUMENT).and_then(Value::as_str) {
            note.push_str(NOTE_SEPARATOR);
            note.push_str(REMEMBER_PREFIX);
            note.push_str(&one_line(persist, MAX_FIELD_CHARS));
        }
        if let Some(expires_at) = &run.expires_at {
            note.push_str(NOTE_SEPARATOR);
            note.push_str(EXPIRES_PREFIX);
            note.push_str(expires_at);
        }
        note
    }
}

/// `text` on one line, its whitespace runs collapsed, cut to `max` chars.
fn one_line(text: &str, max: usize) -> String {
    let mut line = String::new();
    for (index, word) in text.split_whitespace().enumerate() {
        if index > 0 {
            line.push(' ');
        }
        line.push_str(word);
    }
    match line.char_indices().nth(max) {
        Some((cut, _)) => {
            line.truncate(cut);
            line.push(CUT_MARK);
            line
        }
        None => line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use test_case::test_case;

    const RUN_ID: &str = "exec_1";
    const OTHER_RUN_ID: &str = "exec_2";
    const RUN_MESSAGE: &str = "Allow github to create an issue?";
    const RUN_ADDRESS: &str = "github.issues.create";
    const RUN_EXPIRY: &str = "2026-10-13T12:00:00Z";
    const PERSIST_SCOPE: &str = "session";
    const WAITING_NOTE: &str = "Executor run waiting for: Allow github to create an issue? · github.issues.create({\"title\":\"Bug\"})";
    const REMEMBERED_NOTE: &str = "Executor run waiting for: Allow github to create an issue? · github.issues.create({\"title\":\"Bug\"}) · remember: session · expires 2026-10-13T12:00:00Z";
    const UNKNOWN_NOTE: &str = "Executor run exec_2: not a run Caudra saw pause";
    const LOCK_NOTE: &str = "Home Assistant lock devices always ask";
    const COVER_NOTE: &str = "Home Assistant cover devices always ask";
    const HOME_ASSISTANT_TOOL: &str = "HassTurnOn";
    const EXECUTE_TOOL: &str = "execute";

    fn paused(id: &str, expires_at: Option<&str>) -> Value {
        let mut structured = json!({
            "status": WAITING_STATUS,
            "executionId": id,
            "interaction": {
                "kind": "form",
                "message": RUN_MESSAGE,
                "address": RUN_ADDRESS,
                "args": {"title": "Bug"},
            },
        });
        if let Some(expires_at) = expires_at {
            structured[EXPIRES_FIELD] = json!(expires_at);
        }
        structured
    }

    fn resume(id: &str, action: &str) -> Value {
        json!({"executionId": id, "action": action})
    }

    #[test_case("home-assistant", Some(McpServerKind::HomeAssistant) ; "home_assistant")]
    #[test_case("Executor", Some(McpServerKind::Executor) ; "executor_any_case")]
    #[test_case("homeassistant", None ; "near_miss")]
    #[test_case("", None ; "empty")]
    fn recognises_by_server_name(name: &str, kind: Option<McpServerKind>) {
        assert_eq!(McpServerKind::from_server_name(name), kind);
    }

    #[test_case(json!({"domain": "lock"}), Some("lock") ; "a_string_domain")]
    #[test_case(json!({"domain": ["light", "cover"]}), Some("cover") ; "an_array_domain")]
    #[test_case(json!({"domain": ["Alarm_Control_Panel"]}), Some("alarm_control_panel") ; "a_mixed_case_domain")]
    #[test_case(json!({"domain": " VALVE "}), Some("valve") ; "a_padded_domain")]
    #[test_case(json!({"domain": ["light", "switch"]}), None ; "harmless_domains")]
    #[test_case(json!({"name": "Front door lock"}), None ; "no_domain")]
    #[test_case(json!({"domain": 7}), None ; "a_domain_that_is_not_text")]
    fn finds_a_sensitive_domain(input: Value, expected: Option<&str>) {
        assert_eq!(sensitive_domain(&input), expected);
    }

    #[test_case(json!({"domain": ["lock"]}), Some(LOCK_NOTE) ; "a_lock")]
    #[test_case(json!({"domain": "COVER"}), Some(COVER_NOTE) ; "a_cover")]
    #[test_case(json!({"name": "Kitchen"}), None ; "no_domain")]
    fn home_assistant_names_the_domain_that_always_asks(input: Value, note: Option<&str>) {
        assert_eq!(
            McpServerKind::HomeAssistant
                .always_asks(HOME_ASSISTANT_TOOL, &input, &PausedRuns::default())
                .as_deref(),
            note
        );
    }

    #[test_case(resume(RUN_ID, "accept"), None, Some(WAITING_NOTE) ; "accept_says_what_the_run_waits_for")]
    #[test_case(resume(RUN_ID, "ACCEPT"), None, Some(WAITING_NOTE) ; "accept_in_any_case")]
    #[test_case(json!({"executionId": RUN_ID, "action": "accept", "persist": PERSIST_SCOPE}), Some(RUN_EXPIRY), Some(REMEMBERED_NOTE) ; "accept_with_persist_and_expiry")]
    #[test_case(resume(OTHER_RUN_ID, "accept"), None, Some(UNKNOWN_NOTE) ; "accept_of_an_unknown_run")]
    #[test_case(resume(RUN_ID, "decline"), None, None ; "decline_follows_policy")]
    #[test_case(resume(RUN_ID, "cancel"), None, None ; "cancel_follows_policy")]
    fn executor_resume_accept_always_asks(input: Value, expiry: Option<&str>, note: Option<&str>) {
        let runs = PausedRuns::default();
        runs.record(&paused(RUN_ID, expiry));
        assert_eq!(
            McpServerKind::Executor
                .always_asks(RESUME_TOOL, &input, &runs)
                .as_deref(),
            note
        );
    }

    #[test]
    fn executor_other_tools_follow_policy() {
        let runs = PausedRuns::default();
        runs.record(&paused(RUN_ID, None));
        assert_eq!(
            McpServerKind::Executor.always_asks(
                EXECUTE_TOOL,
                &resume(RUN_ID, ACCEPT_ACTION),
                &runs
            ),
            None
        );
    }

    #[test]
    fn the_oldest_paused_run_is_forgotten_first() {
        let runs = PausedRuns::default();
        for index in 0..=MAX_PAUSED_RUNS {
            runs.record(&paused(&format!("run-{index}"), None));
        }
        let note = |id: &str| runs.note(&resume(id, ACCEPT_ACTION));
        assert!(note("run-0").starts_with(UNKNOWN_RUN_PREFIX));
        assert!(note("run-1").starts_with(WAITING_NOTE_PREFIX));
        assert!(note(&format!("run-{MAX_PAUSED_RUNS}")).starts_with(WAITING_NOTE_PREFIX));
    }

    #[test]
    fn a_run_paused_again_keeps_one_entry() {
        let runs = PausedRuns::default();
        runs.record(&paused(RUN_ID, None));
        runs.record(&paused(RUN_ID, Some(RUN_EXPIRY)));
        let entries = runs.0.lock().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].1.expires_at.as_deref(), Some(RUN_EXPIRY));
    }

    #[test_case(json!({"status": "completed", "executionId": RUN_ID}) ; "a_completed_run")]
    #[test_case(json!({"status": WAITING_STATUS}) ; "no_execution_id")]
    fn only_a_waiting_run_is_remembered(structured: Value) {
        let runs = PausedRuns::default();
        runs.record(&structured);
        assert!(runs.0.lock().unwrap().is_empty());
    }

    #[test]
    fn long_args_are_cut_on_one_line() {
        let long = "word ".repeat(MAX_ARGS_CHARS);
        let line = one_line(&format!("{long}\n{long}"), MAX_ARGS_CHARS);
        assert_eq!(line.chars().count(), MAX_ARGS_CHARS + 1);
        assert!(line.ends_with(CUT_MARK));
        assert!(!line.contains('\n'));
    }

    #[test]
    fn call_facts_from_an_older_request_decode() {
        let facts: McpCallFacts = serde_json::from_value(json!({"kind": "executor"})).unwrap();
        assert_eq!(
            facts,
            McpCallFacts {
                kind: McpServerKind::Executor,
                read_only: false,
                note: None,
            }
        );
        let wire = serde_json::to_value(&facts).unwrap();
        assert!(wire.get("note").is_none());
    }
}
