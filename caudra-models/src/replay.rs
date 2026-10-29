use caudra_automation::args::resolve;
use caudra_automation::engine::FiringLimits;
use caudra_automation::event::{Event, EventDetail};
use caudra_automation::matcher::{TopicMatcher, first_match};
use caudra_automation::meta::Trigger;
use caudra_automation::replay::{DryRun, dry_run};
use caudra_automation::validate::validate;
use color_eyre::eyre::{Result, WrapErr, bail};
use serde::Deserialize;
use serde_json::{Value, json};

const EMPTY_CASES: &str = "replay needs at least one case";
const UNROUTED: &str = "no declared trigger routes this event";

struct ExactTopics;

impl TopicMatcher for ExactTopics {
    fn matches(&self, pattern: &str, topic: &str) -> bool {
        pattern == topic
    }
}

#[derive(Deserialize)]
pub struct Case {
    name: String,
    event: Event,
    state: Value,
    args: Value,
    now_ms: i64,
    expected: Value,
}

pub fn check(source: &str, cases: Vec<Case>) -> Result<usize> {
    let Some(first) = cases.first() else {
        bail!(EMPTY_CASES);
    };
    let meta = validate(source, first.now_ms)?.meta;
    for case in &cases {
        let routed = match &case.event.detail {
            EventDetail::Schedule { .. } => meta
                .triggers
                .iter()
                .any(|trigger| matches!(trigger, Trigger::Schedule(_))),
            detail => first_match(&meta.triggers, detail, &ExactTopics).is_some(),
        };
        if !routed {
            bail!("{}: {UNROUTED}", case.name);
        }
        let args = resolve(&meta.args, &case.args)
            .wrap_err_with(|| format!("{}: resolving args", case.name))?;
        let report = dry_run(DryRun {
            source,
            meta: &meta,
            event: &case.event,
            state: &case.state,
            args: &args,
            limits: &FiringLimits::default(),
            now_ms: case.now_ms,
            journal: &[],
            admission: Ok(()),
        });
        let actual = json!({
            "end": report.outcome.end,
            "actions": report.actions.iter().map(|action| action.request.to_journal()).collect::<Vec<_>>(),
            "state": report.outcome.state.as_ref().map(|change| &change.state).unwrap_or(&case.state),
        });
        if actual != case.expected {
            bail!(
                "{}: replay mismatch\nexpected: {}\nactual: {}",
                case.name,
                serde_json::to_string_pretty(&case.expected)?,
                serde_json::to_string_pretty(&actual)?,
            );
        }
    }
    Ok(cases.len())
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use test_case::test_case;

    use super::{Case, EMPTY_CASES, UNROUTED, check};

    const SOURCE: &str = r#"
        let meta = #{ name: "replay-test", description: "Replay test", triggers: [#{ kind: "armed" }], timezone: "UTC" };
        state.runs = (state.runs ?? 0) + 1;
        notify("checked");
    "#;
    const CASE: &str = "recorded firing";
    const MESSAGE: &str = "checked";
    const NOW_MS: i64 = 1_791_194_400_000;
    const MISMATCH: &str = "replay mismatch";
    const CASE_COUNT: usize = 1;

    fn case() -> Case {
        serde_json::from_value(json!({
            "name": CASE,
            "event": {
                "fire_id": "test", "at": NOW_MS / 1000,
                "session": {
                    "id": "test", "title": {"$untrusted": "title"}, "name": null,
                    "mode": "build", "status": "idle", "status_since": NOW_MS / 1000,
                    "goal": null, "autopilot": null, "cost": null,
                    "groups": [], "work": {"held": null, "paused": []}
                },
                "trigger": "armed", "reason": "manual"
            },
            "state": {}, "args": {}, "now_ms": NOW_MS,
            "expected": {
                "end": {"end": "completed"},
                "actions": [{"kind": "notify", "text": MESSAGE}],
                "state": {"runs": 1}
            }
        }))
        .unwrap()
    }

    #[test]
    fn recorded_actions_and_state_match_without_a_live_host() {
        assert_eq!(check(SOURCE, vec![case()]).unwrap(), CASE_COUNT);
    }

    #[test_case("end"; "outcome")]
    #[test_case("actions"; "messages")]
    #[test_case("state"; "state")]
    fn any_changed_expectation_fails(field: &str) {
        let mut case = case();
        case.expected[field] = Value::Null;
        let error = check(SOURCE, vec![case]).unwrap_err().to_string();
        assert!(error.contains(CASE));
        assert!(error.contains(MISMATCH));
    }

    #[test]
    fn an_empty_suite_is_not_a_passing_test() {
        assert_eq!(check(SOURCE, vec![]).unwrap_err().to_string(), EMPTY_CASES);
    }

    #[test]
    fn removing_a_trigger_fails_its_recorded_cases() {
        let source = SOURCE.replace("\"armed\"", "\"idle\"");
        assert!(
            check(&source, vec![case()])
                .unwrap_err()
                .to_string()
                .contains(UNROUTED)
        );
    }
}
