//! Live evaluation of the built-in question sets against the hand-labelled
//! cases in `eval/*.json`. Every case goes through the feature's own state
//! and question builders and its own decision function, so what is scored is
//! the action the feature would take, compared with the baseline stored in
//! the fixture.
//!
//! ```text
//! CAUDRA_DECISION_EVAL_URL=http://127.0.0.1:8080/typesafe \
//!     cargo nextest run -p caudra-agent --run-ignored only -E 'test(decision_eval)' --no-capture
//! ```

use std::collections::BTreeSet;
use std::env;
use std::fmt::Debug;
use std::future::Future;
use std::time::Instant;

use caudra_config::decisions::{DecisionsConfig, FeatureMode};
use caudra_decision::{Answer, QuestionSet};
use caudra_providers::ModelPurpose;
use caudra_storage::StateDir;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::{
    DecisionContext, DecisionFeature, DecisionOutcome, Decisions, PermissionAction, content,
    permission, question_tool_nudge, questions, shell_duration, shell_effect,
};
use crate::SubagentTaskMode;
use crate::agent::subagent::{RoutingTask, routing_state, subagent_job};
use crate::agent::{
    GOAL_MET_QUESTION, goal_prescreen_state, should_skip_goal, skill_questions, skill_shortlist,
    skill_state, suggested_skill,
};
use crate::permissions::{PermissionResource, decision_state};
use crate::tools::deferral::rank_tool_search;
use crate::tools::native::skill::{SkillInventoryEntry, SkillScope};
use crate::types::{TodoItem, TodoPriority, TodoStatus};

const URL_ENV: &str = "CAUDRA_DECISION_EVAL_URL";
const MODEL_ENV: &str = "CAUDRA_DECISION_EVAL_MODEL";
const DEFAULT_MODEL: &str = "jev-latest";
const URL_MISSING: &str = "set CAUDRA_DECISION_EVAL_URL to the decision engine base URL";
const URL_INVALID: &str = "CAUDRA_DECISION_EVAL_URL is not a URL";
const SERVICE_INVALID: &str = "the evaluation decision service could not be built";
const SET_INVALID: &str = "a built-in question set is invalid";
const FEATURE_DISABLED: &str = "the evaluation service has the feature disabled";
const UNREACHED: &str = "this case never reaches the engine";
const REGRESSED: &str = "a feature scored fewer right or more wrong actions than its baseline";
const TIMEOUT_MS: u64 = 10_000;
/// A yes/no answer at or past this probability, or at or below its
/// complement, counts as decided where the feature has no threshold of its
/// own to act on.
const DECIDED: f64 = 0.85;
const NONE: &str = "none";
const FAST: &str = "fast";
const BEST: &str = "best";
const P50: f64 = 0.5;
const P95: f64 = 0.95;
/// Fixture names for the levels of the `duration` score, lowest first.
const DURATION_LEVELS: [&str; 4] = ["instant", "seconds", "minutes", "endless"];

#[derive(Deserialize)]
struct Fixture<C> {
    baseline: Tally,
    #[serde(default)]
    baseline_note: Option<String>,
    cases: Vec<C>,
}

#[derive(Debug, Default, Deserialize)]
struct Tally {
    right: usize,
    wrong: usize,
    undecided: usize,
    p50_ms: u64,
    p95_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Verdict {
    Right,
    Wrong,
    Undecided,
}

struct Scored {
    label: String,
    verdicts: Vec<Verdict>,
    latency_ms: u64,
}

#[derive(Deserialize)]
struct DurationCase {
    command: String,
    #[serde(default)]
    earlier_runs: Vec<Value>,
    expect: String,
}

#[derive(Deserialize)]
struct PermissionCase {
    tool: String,
    input: Value,
    resources: Vec<PermissionResource>,
    flags: BTreeSet<String>,
    #[serde(default)]
    either: BTreeSet<String>,
}

#[derive(Deserialize)]
struct EffectCase {
    command: String,
    writes_project_files: bool,
    changes_system_state: bool,
}

#[derive(Deserialize)]
struct ContentCase {
    content: String,
    flagged: bool,
}

#[derive(Deserialize)]
struct GoalCase {
    goal: String,
    assistant_tail: String,
    tool_outcomes: Vec<Value>,
    #[serde(default)]
    open_todos: Vec<String>,
    met: bool,
}

#[derive(Deserialize)]
struct SubagentCase {
    label: String,
    prompt: String,
    mode: SubagentTaskMode,
    profile: Option<String>,
    route: Option<String>,
}

#[derive(Deserialize)]
struct Described {
    name: String,
    description: String,
}

#[derive(Deserialize)]
struct SkillCase {
    request: String,
    skills: Vec<Described>,
    expect: String,
}

#[derive(Deserialize)]
struct ToolCase {
    query: String,
    candidates: Vec<Described>,
    expect: String,
}

#[derive(Deserialize)]
struct QuestionToolCase {
    label: String,
    cohort: String,
    user_request: String,
    assistant_reply: String,
    needed: bool,
}

#[test]
#[ignore = "needs CAUDRA_DECISION_EVAL_URL"]
fn decision_eval() {
    let url = env::var(URL_ENV).expect(URL_MISSING);
    let temp = tempfile::tempdir().unwrap();
    let decisions = &service(&url, StateDir::from_path(temp.path().into()));
    let passed = smol::block_on(async {
        [
            run(
                "shell_duration",
                include_str!("eval/shell_duration.json"),
                |case| shell_duration_case(decisions, case),
            )
            .await,
            run("permission", include_str!("eval/permission.json"), |case| {
                permission_case(decisions, case)
            })
            .await,
            run(
                "shell_effect",
                include_str!("eval/shell_effect.json"),
                |case| shell_effect_case(decisions, case),
            )
            .await,
            run("content", include_str!("eval/content.json"), |case| {
                content_case(decisions, case)
            })
            .await,
            run("goal", include_str!("eval/goal.json"), |case| {
                goal_case(decisions, case)
            })
            .await,
            run("subagent", include_str!("eval/subagent.json"), |case| {
                subagent_case(decisions, case)
            })
            .await,
            run("skill", include_str!("eval/skill.json"), |case| {
                skill_case(decisions, case)
            })
            .await,
            run(
                "tool_search",
                include_str!("eval/tool_search.json"),
                |case| tool_search_case(decisions, case),
            )
            .await,
            run(
                "question_tool_nudge",
                include_str!("eval/question_tool_nudge.json"),
                |case| question_tool_nudge_case(decisions, case),
            )
            .await,
        ]
    });
    assert!(passed.iter().all(|passed| *passed), "{REGRESSED}");
}

fn service(url: &str, state_dir: StateDir) -> Decisions {
    let mut config = DecisionsConfig {
        base_url: Some(url.parse().expect(URL_INVALID)),
        model: env::var(MODEL_ENV).unwrap_or_else(|_| DEFAULT_MODEL.into()),
        allow_remote: true,
        allow_http: true,
        timeout_ms: TIMEOUT_MS,
        log: false,
        ..DecisionsConfig::default()
    };
    let features = &mut config.features;
    features.permission_advice = FeatureMode::Advise;
    features.shell_effect = FeatureMode::Advise;
    features.content_screening = FeatureMode::Advise;
    features.shell_duration = FeatureMode::Enforce;
    features.tool_search = FeatureMode::Enforce;
    features.skill_suggestions = FeatureMode::Advise;
    features.goal_prescreen = FeatureMode::Enforce;
    features.subagent_routing = FeatureMode::Enforce;
    features.question_tool_nudge = FeatureMode::Advise;
    Decisions::new(config, &state_dir).expect(SERVICE_INVALID)
}

async fn run<C: DeserializeOwned, F: Future<Output = Scored>>(
    feature: &str,
    fixture: &str,
    score: impl Fn(C) -> F,
) -> bool {
    let fixture: Fixture<C> = serde_json::from_str(fixture)
        .unwrap_or_else(|error| panic!("{feature} fixture is invalid: {error}"));
    if let Some(note) = &fixture.baseline_note {
        println!("{feature} baseline: {note}");
    }
    let mut measured = Tally::default();
    let mut latencies = Vec::new();
    for case in fixture.cases {
        let scored = score(case).await;
        latencies.push(scored.latency_ms);
        for verdict in &scored.verdicts {
            match verdict {
                Verdict::Right => measured.right += 1,
                Verdict::Wrong => measured.wrong += 1,
                Verdict::Undecided => measured.undecided += 1,
            }
        }
        if scored
            .verdicts
            .iter()
            .any(|verdict| *verdict != Verdict::Right)
        {
            println!("  {feature} {:?}: {}", scored.verdicts, scored.label);
        }
    }
    latencies.sort_unstable();
    measured.p50_ms = percentile(&latencies, P50);
    measured.p95_ms = percentile(&latencies, P95);
    println!(
        "{feature}\n  measured {measured:?}\n  baseline {:?}",
        fixture.baseline
    );
    measured.wrong <= fixture.baseline.wrong && measured.right >= fixture.baseline.right
}

fn percentile(sorted: &[u64], quantile: f64) -> u64 {
    let rank = (quantile * sorted.len() as f64).ceil() as usize;
    sorted
        .get(rank.saturating_sub(1))
        .copied()
        .unwrap_or_default()
}

async fn evaluate(
    decisions: &Decisions,
    feature: DecisionFeature,
    state: &Value,
    questions: &QuestionSet,
) -> DecisionOutcome {
    decisions
        .evaluate(feature, state, questions, &DecisionContext::default())
        .await
        .expect(FEATURE_DISABLED)
}

/// The action taken against the one expected, where `None` is no action.
fn matched<T: PartialEq>(answered: bool, chosen: Option<T>, expected: Option<T>) -> Verdict {
    match (answered, chosen) {
        (false, _) => Verdict::Undecided,
        (true, None) if expected.is_some() => Verdict::Undecided,
        (true, chosen) if chosen == expected => Verdict::Right,
        (true, _) => Verdict::Wrong,
    }
}

fn noul(outcome: &DecisionOutcome, id: &str) -> Option<f64> {
    match outcome.result.as_ref().ok()?.answers.get(id)? {
        Answer::Noul(answer) => Some(answer.noul),
        _ => None,
    }
}

fn decided(probability: Option<f64>, expected: bool) -> Verdict {
    match probability {
        Some(probability) if probability >= DECIDED => match expected {
            true => Verdict::Right,
            false => Verdict::Wrong,
        },
        Some(probability) if probability <= 1.0 - DECIDED => match expected {
            true => Verdict::Wrong,
            false => Verdict::Right,
        },
        _ => Verdict::Undecided,
    }
}

fn latency_since(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn expected_choice(expect: &str) -> Option<&str> {
    (expect != NONE).then_some(expect)
}

fn scored<T: Debug>(
    label: impl Debug,
    chosen: T,
    verdicts: Vec<Verdict>,
    latency_ms: u64,
) -> Scored {
    Scored {
        label: format!("{label:?} -> {chosen:?}"),
        verdicts,
        latency_ms,
    }
}

#[test]
#[ignore = "needs CAUDRA_DECISION_EVAL_URL"]
fn question_tool_nudge_eval() {
    let url = env::var(URL_ENV).expect(URL_MISSING);
    let temp = tempfile::tempdir().unwrap();
    let decisions = service(&url, StateDir::from_path(temp.path().into()));
    let passed = smol::block_on(run(
        "question_tool_nudge",
        include_str!("eval/question_tool_nudge.json"),
        |case| question_tool_nudge_case(&decisions, case),
    ));
    assert!(passed, "{REGRESSED}");
}

fn question_tool_verdict(outcome: &DecisionOutcome, threshold: f64, needed: bool) -> Verdict {
    let chosen = question_tool_nudge::should_nudge(&FeatureMode::Advise, outcome, threshold);
    matched(
        question_tool_nudge::probability(outcome).is_some(),
        chosen.then_some(true),
        needed.then_some(true),
    )
}

async fn question_tool_nudge_case(decisions: &Decisions, case: QuestionToolCase) -> Scored {
    let questions = questions::QUESTION_TOOL_NUDGE.as_ref().expect(SET_INVALID);
    let state =
        question_tool_nudge::state(&case.user_request, &case.assistant_reply).expect(UNREACHED);
    let outcome = evaluate(
        decisions,
        DecisionFeature::QuestionToolNudge,
        state.value(),
        questions,
    )
    .await;
    let verdict = question_tool_verdict(
        &outcome,
        decisions.config().thresholds.question_tool_nudge,
        case.needed,
    );
    scored(
        (case.cohort, case.label),
        question_tool_nudge::probability(&outcome),
        vec![verdict],
        outcome.latency_ms,
    )
}

#[test]
fn question_tool_fixture_preserves_the_reviewed_and_held_out_cohort() {
    let fixture: Fixture<QuestionToolCase> =
        serde_json::from_str(include_str!("eval/question_tool_nudge.json")).unwrap();
    let cases = &fixture.cases;
    assert_eq!(cases.len(), 36);
    assert_eq!(cases.iter().filter(|case| case.needed).count(), 16);
    assert_eq!(
        cases
            .iter()
            .map(|case| &case.label)
            .collect::<BTreeSet<_>>()
            .len(),
        cases.len()
    );
    for (cohort, count, positive) in [
        ("reviewed_paraphrase", 12, 7),
        ("synthetic_initial", 12, 3),
        ("synthetic_held_out", 12, 6),
    ] {
        let group: Vec<_> = cases.iter().filter(|case| case.cohort == cohort).collect();
        assert_eq!(group.len(), count);
        assert_eq!(group.iter().filter(|case| case.needed).count(), positive);
    }
    for case in cases {
        let state = question_tool_nudge::state(&case.user_request, &case.assistant_reply).unwrap();
        assert!(state.value().to_string().len() <= super::state::MAX_STATE_BYTES);
    }
    assert!(
        cases
            .iter()
            .any(|case| case.label == "rust_quiz_known_miss" && case.needed)
    );
    assert_eq!(fixture.baseline.right, 35);
    assert_eq!(fixture.baseline.wrong, 0);
    assert_eq!(fixture.baseline.undecided, 1);
}

#[test]
fn question_tool_quiz_miss_is_not_scored_as_success() {
    let outcome = DecisionOutcome {
        result: Ok(serde_json::from_value(json!({
            "model": "test",
            "answers": {question_tool_nudge::QUESTION: {"type": "noul", "noul": 0.5212}},
            "usage": {"input_tokens": 0, "output_tokens": 0},
        }))
        .unwrap()),
        latency_ms: 0,
        receipt: None,
    };
    assert_eq!(
        question_tool_verdict(&outcome, DECIDED, true),
        Verdict::Undecided
    );
}

async fn shell_duration_case(decisions: &Decisions, case: DurationCase) -> Scored {
    let questions = questions::SHELL_DURATION.as_ref().expect(SET_INVALID);
    let state = shell_duration::duration_state(&case.command, None, case.earlier_runs.clone());
    let outcome = evaluate(decisions, DecisionFeature::ShellDuration, &state, questions).await;
    let thresholds = &decisions.config().thresholds;
    let level = outcome.result.as_ref().ok().and_then(|response| {
        shell_duration::prior(
            response,
            thresholds.shell_duration,
            thresholds.shell_endless,
        )
        .and_then(|level| DURATION_LEVELS.get(level).copied())
    });
    let verdict = matched(outcome.result.is_ok(), level, Some(case.expect.as_str()));
    scored(
        (&case.command, &case.earlier_runs, &case.expect),
        level,
        vec![verdict],
        outcome.latency_ms,
    )
}

async fn permission_case(decisions: &Decisions, case: PermissionCase) -> Scored {
    let questions = Decisions::permission_questions().expect(SET_INVALID);
    let state = decision_state(&case.tool, &case.input, &case.resources);
    let feature = DecisionFeature::PermissionAdvice;
    let outcome = evaluate(decisions, feature.clone(), &state, &questions).await;
    let raised: BTreeSet<_> = match decisions.permission_action(&feature, &outcome.result) {
        Some(PermissionAction::Advice(flags) | PermissionAction::Escalate(flags)) => {
            flags.into_iter().map(|flag| flag.flag).collect()
        }
        None => BTreeSet::new(),
    };
    let verdicts = permission::FLAGS
        .iter()
        .filter(|flag| !case.either.contains(**flag))
        .map(|flag| match outcome.result {
            Err(_) => Verdict::Undecided,
            Ok(_) if raised.contains(*flag) == case.flags.contains(*flag) => Verdict::Right,
            Ok(_) => Verdict::Wrong,
        })
        .collect();
    scored(
        (&case.input, &case.flags),
        raised,
        verdicts,
        outcome.latency_ms,
    )
}

async fn shell_effect_case(decisions: &Decisions, case: EffectCase) -> Scored {
    let questions = questions::SHELL_EFFECT.as_ref().expect(SET_INVALID);
    let state = shell_effect::effect_state(&case.command);
    let outcome = evaluate(decisions, DecisionFeature::ShellEffect, &state, questions).await;
    let answers = [shell_effect::WRITES, shell_effect::CHANGES_SYSTEM].map(|id| noul(&outcome, id));
    let verdicts = vec![
        decided(answers[0], case.writes_project_files),
        decided(answers[1], case.changes_system_state),
    ];
    scored(
        (
            &case.command,
            case.writes_project_files,
            case.changes_system_state,
        ),
        answers,
        verdicts,
        outcome.latency_ms,
    )
}

async fn content_case(decisions: &Decisions, case: ContentCase) -> Scored {
    let questions = questions::CONTENT.as_ref().expect(SET_INVALID);
    let thresholds = &decisions.config().thresholds;
    let chunks = content::chunks(&case.content);
    assert!(!chunks.is_empty(), "{UNREACHED}: {}", case.content);
    let mut latency_ms = 0;
    let mut flags = Vec::new();
    for chunk in chunks {
        let state = content::chunk_state(&chunk);
        let outcome = evaluate(
            decisions,
            DecisionFeature::ContentScreening,
            &state,
            questions,
        )
        .await;
        latency_ms += outcome.latency_ms;
        flags.push(outcome.result.as_ref().ok().map(|response| {
            content::content_flagged(
                response,
                thresholds.content_injection,
                thresholds.content_addressed_to_agent,
            )
        }));
    }
    let flagged = flags
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .map(|flags| flags.contains(&true));
    let verdict = match flagged {
        Some(flagged) if flagged == case.flagged => Verdict::Right,
        Some(_) => Verdict::Wrong,
        None => Verdict::Undecided,
    };
    scored(
        (&case.content, case.flagged),
        flagged,
        vec![verdict],
        latency_ms,
    )
}

async fn goal_case(decisions: &Decisions, case: GoalCase) -> Scored {
    let questions = questions::GOAL.as_ref().expect(SET_INVALID);
    let todos: Vec<_> = case
        .open_todos
        .iter()
        .map(|content| TodoItem {
            content: content.clone(),
            status: TodoStatus::Pending,
            priority: TodoPriority::default(),
        })
        .collect();
    let state = goal_prescreen_state(
        &case.goal,
        Some(&case.assistant_tail),
        &todos,
        case.tool_outcomes,
    );
    let outcome = evaluate(decisions, DecisionFeature::GoalPrescreen, &state, questions).await;
    let skipped = should_skip_goal(
        &FeatureMode::Enforce,
        &outcome,
        decisions.config().thresholds.goal_skip_below,
        0,
        0,
        u32::MAX,
    );
    let verdict = match (outcome.result.is_ok(), skipped, case.met) {
        (false, _, _) | (true, false, false) => Verdict::Undecided,
        (true, true, true) => Verdict::Wrong,
        (true, true, false) | (true, false, true) => Verdict::Right,
    };
    scored(
        (&case.goal, &case.open_todos, case.met),
        (skipped, noul(&outcome, GOAL_MET_QUESTION)),
        vec![verdict],
        outcome.latency_ms,
    )
}

async fn subagent_case(decisions: &Decisions, case: SubagentCase) -> Scored {
    let questions = questions::SUBAGENT.as_ref().expect(SET_INVALID);
    let state = routing_state(&RoutingTask {
        label: &case.label,
        prompt: Some(&case.prompt),
        mode: case.mode,
        profile: case.profile.as_deref(),
    });
    let outcome = evaluate(
        decisions,
        DecisionFeature::SubagentRouting,
        &state,
        questions,
    )
    .await;
    let threshold = decisions.config().thresholds.routing_confidence;
    let route = outcome
        .result
        .as_ref()
        .ok()
        .and_then(|response| subagent_job(response, threshold))
        .map(|purpose| match purpose {
            ModelPurpose::Fast => FAST,
            ModelPurpose::Best => BEST,
            _ => NONE,
        });
    let verdict = matched(outcome.result.is_ok(), route, case.route.as_deref());
    scored(
        (
            &case.label,
            &case.prompt,
            &case.mode,
            &case.profile,
            &case.route,
        ),
        route,
        vec![verdict],
        outcome.latency_ms,
    )
}

async fn skill_case(decisions: &Decisions, case: SkillCase) -> Scored {
    let inventory = case
        .skills
        .into_iter()
        .map(|skill| SkillInventoryEntry {
            location: format!("builtin:{}", skill.name),
            name: skill.name,
            description: skill.description,
            scope: SkillScope::Builtin,
        })
        .collect();
    let candidates = skill_shortlist(&case.request, inventory, &BTreeSet::new());
    let questions =
        skill_questions(&candidates).unwrap_or_else(|| panic!("{UNREACHED}: {}", case.request));
    let outcome = evaluate(
        decisions,
        DecisionFeature::SkillSuggestions,
        &skill_state(&case.request),
        &questions,
    )
    .await;
    let threshold = decisions.config().thresholds.routing_confidence;
    let chosen = suggested_skill(&outcome, &candidates, threshold);
    let verdict = matched(
        outcome.result.is_ok(),
        chosen,
        expected_choice(&case.expect),
    );
    scored(
        (&case.request, &case.expect),
        chosen,
        vec![verdict],
        outcome.latency_ms,
    )
}

async fn tool_search_case(decisions: &Decisions, case: ToolCase) -> Scored {
    let definitions: Vec<_> = case
        .candidates
        .iter()
        .map(|tool| json!({"description": tool.description}))
        .collect();
    let candidates = case
        .candidates
        .iter()
        .zip(&definitions)
        .map(|(tool, definition)| (tool.name.as_str(), definition));
    let started = Instant::now();
    let ranking = rank_tool_search(
        &case.query,
        candidates,
        Some(decisions),
        &DecisionContext::default(),
    )
    .await;
    let latency_ms = latency_since(started);
    let chosen = ranking.map(|ranking| case.candidates[ranking.index()].name.as_str());
    let verdict = matched(true, chosen, expected_choice(&case.expect));
    scored(
        (&case.query, &case.expect),
        chosen,
        vec![verdict],
        latency_ms,
    )
}
