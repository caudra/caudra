//! Checks a script before anyone arms it: the header, which compiles the script, the args the
//! body reads, and one smoke run per declared trigger. A smoke run is a dry run of a canned event
//! that the trigger's options match, with the args' defaults or examples and an empty state, so
//! the capability checks and per-firing limits apply while nothing is performed.

use serde::Serialize;
use serde_json::{Map, Value};

use crate::args::{ArgsError, SMOKE_STRING, resolve, smoke_args, undeclared};
use crate::engine::{FiringEnd, FiringError, FiringLimits};
use crate::event::{
    Admission, ArmedReason, Audience, Delivery, Event, EventDetail, GoalFinishedDetail,
    GoalVerdict, IdleDetail, InputKind, MessageDetail, PauseReason, SenderKind, SessionStatus,
    SessionView, StartedBy, TurnOutcome, WorkFinishedDetail, WorkState, WorkView,
    WorkflowFinishedDetail, WorkflowStatus,
};
use crate::meta::{
    AutomationMeta, MessageFilter, MetaError, NAME_WILDCARD, References, Trigger, TriggerKind,
    parse_meta, references,
};
use crate::replay::{DryRun, DryRunReport, dry_run};
use crate::untrusted::Untrusted;

const MILLIS_PER_SECOND: i64 = 1_000;
/// Names the canned session, its firing, its message and its run.
const SMOKE_ID: &str = "smoke";
const SMOKE_NAME: &str = "@smoke";
const SMOKE_PEER: &str = "@smoke-peer";
const SMOKE_MODE: &str = "build";
const SMOKE_TOOL: &str = "shell";
const SMOKE_GROUP: &str = "smoke-group";
const SMOKE_WORK: &str = "smoke-work";
const SMOKE_TOPIC: &str = "smoke.topic";
const SMOKE_WORKFLOW: &str = "smoke-workflow";
const SMOKE_COUNT: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ValidationReport {
    pub meta: AutomationMeta,
    /// What the body reads from `args` and the functions it calls.
    pub references: References,
    /// One per entry of `meta.triggers`, in order.
    pub smoke: Vec<SmokeRun>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SmokeRun {
    pub trigger: TriggerKind,
    pub run: DryRunReport,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    #[error(transparent)]
    Meta(#[from] MetaError),
    #[error("the body reads args that meta.args does not declare: {}", .0.join(", "))]
    UndeclaredArgs(Vec<String>),
    #[error("the smoke run's args do not resolve: {0}")]
    Args(#[from] ArgsError),
    /// A smoke run failed or was stopped.
    #[error("the smoke run of meta.triggers[{index}] ended with {error}")]
    Smoke { index: usize, error: FiringError },
}

/// Validates `source` and smoke-runs each of its triggers at `now_ms`.
pub fn validate(source: &str, now_ms: i64) -> Result<ValidationReport, ValidationError> {
    let meta = parse_meta(source)?;
    let references = references(source)?;
    let unknown = undeclared(&meta.args, &references);
    if !unknown.is_empty() {
        return Err(ValidationError::UndeclaredArgs(unknown));
    }
    let args = resolve(&meta.args, &Value::Object(smoke_args(&meta.args)))?;
    let state = Value::Object(Map::new());
    let limits = FiringLimits::default();
    let smoke = meta
        .triggers
        .iter()
        .enumerate()
        .map(|(index, trigger)| {
            let run = dry_run(DryRun {
                source,
                meta: &meta,
                event: &canned_event(&meta, trigger, now_ms),
                state: &state,
                args: &args,
                limits: &limits,
                now_ms,
                journal: &[],
                admission: Ok(()),
            });
            match &run.outcome.end {
                FiringEnd::Failed(error) | FiringEnd::Stopped(error) => {
                    Err(ValidationError::Smoke {
                        index,
                        error: error.clone(),
                    })
                }
                _ => Ok(SmokeRun {
                    trigger: trigger.kind(),
                    run,
                }),
            }
        })
        .collect::<Result<_, _>>()?;
    Ok(ValidationReport {
        meta,
        references,
        smoke,
    })
}

/// An event at `now_ms` that `trigger`'s static options match.
fn canned_event(meta: &AutomationMeta, trigger: &Trigger, now_ms: i64) -> Event {
    let at = now_ms.div_euclid(MILLIS_PER_SECOND);
    let detail = match trigger {
        Trigger::Armed => EventDetail::Armed {
            reason: ArmedReason::Launch,
        },
        Trigger::Idle { .. } => EventDetail::Idle(IdleDetail {
            outcome: TurnOutcome::Completed,
            error_kind: None,
            error: None,
            started_by: StartedBy::Automation {
                automation: meta.name.clone(),
                fire_id: SMOKE_ID.to_owned(),
            },
            automations: vec![meta.name.clone()],
            runs: SMOKE_COUNT,
            busy_s: 0,
            cost: None,
            work: Vec::new(),
            last_response: text(),
        }),
        Trigger::NeedsInput { delay, inputs } => {
            let input = first(inputs, InputKind::Permission);
            EventDetail::NeedsInput {
                input,
                tool: (input == InputKind::Permission).then(|| SMOKE_TOOL.to_owned()),
                waiting_s: delay.as_secs(),
            }
        }
        Trigger::GoalFinished { verdicts } => EventDetail::GoalFinished(GoalFinishedDetail {
            verdict: first(verdicts, GoalVerdict::Met),
            condition: SMOKE_STRING.to_owned(),
            reason: text(),
            evaluations: SMOKE_COUNT,
            duration_s: 0,
            cost: None,
        }),
        Trigger::MessageReceived(filter) => EventDetail::MessageReceived(canned_message(filter)),
        Trigger::WorkFinished { groups, states } => {
            let state = first(states, WorkState::Completed);
            EventDetail::WorkFinished(WorkFinishedDetail {
                group: groups
                    .first()
                    .map_or(SMOKE_GROUP, String::as_str)
                    .to_owned(),
                work: SMOKE_WORK.to_owned(),
                message_id: SMOKE_ID.to_owned(),
                topic: Some(SMOKE_TOPIC.to_owned()),
                state,
                attempts: SMOKE_COUNT,
                max_attempts: SMOKE_COUNT,
                member: Some(SMOKE_PEER.to_owned()),
                pause_reason: (state == WorkState::Paused)
                    .then_some(PauseReason::CompletionRequired),
                detail: Some(text()),
            })
        }
        Trigger::WorkflowFinished {
            workflows,
            statuses,
        } => {
            let workflow = workflows
                .first()
                .or(meta.workflows.first())
                .map_or(SMOKE_WORKFLOW, String::as_str);
            EventDetail::WorkflowFinished(WorkflowFinishedDetail {
                run_id: SMOKE_ID.to_owned(),
                name: workflow.to_owned(),
                workflow: workflow.to_owned(),
                status: first(statuses, WorkflowStatus::Completed),
                report: Some(text()),
                result: None,
                error: None,
                scratch_dir: None,
                agents: SMOKE_COUNT,
                tokens: 0,
            })
        }
        Trigger::Schedule(_) => EventDetail::Schedule {
            scheduled_for: at,
            late_by_s: 0,
        },
    };
    let status = match trigger.kind() {
        TriggerKind::NeedsInput => SessionStatus::NeedsInput,
        _ => SessionStatus::Idle,
    };
    Event {
        fire_id: SMOKE_ID.to_owned(),
        at,
        session: SessionView {
            id: SMOKE_ID.to_owned(),
            title: text(),
            name: Some(SMOKE_NAME.to_owned()),
            mode: SMOKE_MODE.to_owned(),
            status,
            status_since: at,
            goal: None,
            cost: None,
            groups: Vec::new(),
            work: WorkView::default(),
        },
        detail,
    }
}

/// A message the filter matches: on its first topic when it names topics, and from its first
/// sender, else from its first script.
fn canned_message(filter: &MessageFilter) -> MessageDetail {
    let audience = if filter.topics.is_empty() || !filter.audiences.contains(&Audience::Topic) {
        first(&filter.audiences, Audience::Direct)
    } else {
        Audience::Topic
    };
    let label = filter.scripts.first().filter(|_| filter.senders.is_empty());
    MessageDetail {
        message_id: SMOKE_ID.to_owned(),
        audience,
        topic: (audience == Audience::Topic).then(|| {
            filter
                .topics
                .first()
                .map_or(SMOKE_TOPIC, String::as_str)
                .to_owned()
        }),
        sender_kind: label.map_or(SenderKind::Session, |_| SenderKind::Script),
        sender: label.is_none().then(|| {
            filter
                .senders
                .first()
                .map_or_else(|| SMOKE_PEER.to_owned(), |pattern| sender_matching(pattern))
        }),
        sender_automation: None,
        sender_label: label.map(Untrusted::text),
        sender_title: label.is_none().then(text),
        sender_cwd: None,
        text: text(),
        reply_to: None,
        admission: first(&filter.admissions, Admission::Queued),
        delivery: Delivery::Live,
        consumed: filter.consume,
    }
}

/// An `@name` that a sender pattern matches.
fn sender_matching(pattern: &str) -> String {
    match pattern.strip_suffix(NAME_WILDCARD) {
        Some("") => SMOKE_PEER.to_owned(),
        Some(prefix) => format!("{prefix}{SMOKE_ID}"),
        None => pattern.to_owned(),
    }
}

fn first<T: Copy>(listed: &[T], fallback: T) -> T {
    listed.first().copied().unwrap_or(fallback)
}

fn text() -> Untrusted {
    Untrusted::text(SMOKE_STRING)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::*;
    use crate::engine::{ErrorKind, StopKind};
    use crate::host::{ActionKind, FailureKind};
    use crate::matcher::{TopicMatcher, first_match};

    const NOW_MS: i64 = 1_791_189_000_000;
    const EVERY_TRIGGER: &str = r#"let meta = #{
    name: "every-trigger",
    description: "Declares one trigger of each kind",
    triggers: [
        #{ kind: "armed" },
        #{ kind: "idle", after: "2m" },
        #{ kind: "needs_input", inputs: ["plan", "messages"] },
        #{ kind: "goal_finished", verdicts: ["impossible"] },
        #{ kind: "message_received", audiences: ["direct", "topic"], topics: ["ci.failures"], senders: ["@ci-*"], consume: true },
        #{ kind: "work_finished", groups: ["swarm-tasks"], states: ["paused"] },
        #{ kind: "workflow_finished", workflows: ["review-changes"], statuses: ["failed"] },
        #{ kind: "schedule", every: "10m" },
    ],
    workflows: ["review-changes"],
};
log(event.trigger);
"#;
    const ARGS_SCRIPT: &str = r#"let meta = #{
    name: "smoke-args",
    description: "Reads every kind of arg",
    triggers: [#{ kind: "armed" }],
    args: #{
        goals: #{ type: "list", min: 2 },
        file: #{ type: "string", example: "TODO.md" },
        mode: #{ type: "string", choices: ["fast", "careful"] },
        limit: #{ type: "int", min: 3, max: 9 },
        ratio: #{ type: "float", default_value: 0.5 },
        dry: #{ type: "bool" },
    },
};
state.args = args;
"#;
    const READS_UNDECLARED: &str = r#"let meta = #{ name: "typos", description: "Misspells its args", triggers: [#{ kind: "armed" }], args: #{ goal: #{ type: "string", default_value: "Ship it" } } };
message(args.goal + args.gaol + args["other"]);
"#;
    const SENDS_UNDECLARED: &str = r#"let meta = #{ name: "chatty", description: "Sends without declaring it", triggers: [#{ kind: "armed" }, #{ kind: "idle" }] };
if event.trigger == "idle" { send("@anyone", "hi"); }
"#;
    const DIVIDES_BY_ZERO: &str = r#"let meta = #{ name: "broken", description: "Fails on its smoke run", triggers: [#{ kind: "armed" }] };
let ratio = 1 / 0;
"#;
    const MUST_VALIDATE: &str = "the script validates";
    const MUST_COMMIT: &str = "the smoke run commits its args";

    struct ExactTopics;

    impl TopicMatcher for ExactTopics {
        fn matches(&self, pattern: &str, topic: &str) -> bool {
            pattern == topic
        }
    }

    #[test]
    fn each_trigger_smoke_runs_on_an_event_its_options_match() {
        let report = validate(EVERY_TRIGGER, NOW_MS).expect(MUST_VALIDATE);
        assert_eq!(
            report
                .smoke
                .iter()
                .map(|smoke| smoke.trigger)
                .collect::<Vec<_>>(),
            report
                .meta
                .triggers
                .iter()
                .map(Trigger::kind)
                .collect::<Vec<_>>()
        );
        for (index, trigger) in report.meta.triggers.iter().enumerate() {
            let event = canned_event(&report.meta, trigger, NOW_MS);
            let expected = (trigger.kind() != TriggerKind::Schedule).then_some(index);
            assert_eq!(
                first_match(&report.meta.triggers, &event.detail, &ExactTopics),
                expected,
                "{event:?}"
            );
        }
        assert!(report.smoke.iter().all(|smoke| {
            smoke.run.outcome.end == FiringEnd::Completed
                && smoke
                    .run
                    .actions
                    .iter()
                    .map(|action| action.request.kind())
                    .eq([ActionKind::Log])
        }));
    }

    #[test]
    fn smoke_args_take_defaults_examples_or_values_within_bounds() {
        let report = validate(ARGS_SCRIPT, NOW_MS).expect(MUST_VALIDATE);
        let committed = report.smoke[0]
            .run
            .outcome
            .state
            .clone()
            .expect(MUST_COMMIT)
            .state;
        assert_eq!(
            committed,
            json!({ "args": {
                "goals": [SMOKE_STRING, SMOKE_STRING],
                "file": "TODO.md",
                "mode": "fast",
                "limit": 3,
                "ratio": 0.5,
                "dry": false,
            } })
        );
    }

    #[test]
    fn undeclared_args_are_named() {
        assert_eq!(
            validate(READS_UNDECLARED, NOW_MS),
            Err(ValidationError::UndeclaredArgs(vec![
                "gaol".to_owned(),
                "other".to_owned()
            ]))
        );
    }

    #[test]
    fn meta_errors_come_first() {
        assert_eq!(
            validate("let ready = true;", NOW_MS),
            Err(ValidationError::Meta(MetaError::NotFirst))
        );
    }

    #[test_case(SENDS_UNDECLARED => (1, ErrorKind::Stop(StopKind::Capability)); "a_stop_on_the_second_trigger")]
    #[test_case(DIVIDES_BY_ZERO => (0, ErrorKind::Failure(FailureKind::Script)); "a_script_error")]
    fn a_smoke_run_that_does_not_complete_fails_validation(source: &str) -> (usize, ErrorKind) {
        match validate(source, NOW_MS) {
            Err(ValidationError::Smoke { index, error }) => (index, error.kind),
            other => panic!("{other:?}"),
        }
    }
}
