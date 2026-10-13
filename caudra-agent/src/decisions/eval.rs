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
use std::iter;
use std::time::Instant;

use caudra_config::decisions::{DecisionProtocol, FeatureMode, RawDecisionsConfig};
use caudra_decision::{Answer, QuestionSet};
use caudra_providers::ModelPurpose;
use caudra_storage::StateDir;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use test_case::test_case;

use super::output_pointers::{MIN_WINDOW_LINES, OmittedOutput, advise_pointers, output_focus};
use super::state::MAX_STATE_BYTES;
use super::{
    CodeSymbol, DecisionContext, DecisionFeature, DecisionOutcome, DecisionState, Decisions,
    MAX_RERANK_BATCH, PermissionAction, content, permission, question_tool_nudge, questions,
    rerank_code_context, shell_duration, shell_effect,
};
use crate::agent::subagent::{RoutingTask, routing_state, subagent_job};
use crate::agent::{
    GOAL_MET_QUESTION, Retention, ToolResultView, goal_prescreen_state, retention_query,
    select_tool_results, should_skip_goal, skill_questions, skill_shortlist, skill_state,
    suggested_skill,
};
use crate::memory::relevance::{self, Advice, Judge};
use crate::memory::search::{Candidate, Hit};
use crate::permissions::{PermissionResource, decision_state};
use crate::tools::deferral::rank_tool_search;
use crate::tools::native::skill::{SkillInventoryEntry, SkillScope};
use crate::types::{TodoItem, TodoPriority, TodoStatus};
use crate::{CancelToken, SubagentTaskMode};

const URL_ENV: &str = "CAUDRA_DECISION_EVAL_URL";
const MODEL_ENV: &str = "CAUDRA_DECISION_EVAL_MODEL";
const PROTOCOL_ENV: &str = "CAUDRA_DECISION_EVAL_PROTOCOL";
const PROTOCOL_INVALID: &str = "CAUDRA_DECISION_EVAL_PROTOCOL must be typesafe or openai";
const URL_MISSING: &str = "set CAUDRA_DECISION_EVAL_URL to the decision engine base URL";
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
/// The first rank's discount is `1 / log2(2)`.
const DCG_RANK_OFFSET: f64 = 2.0;
const NOT_ADVISORY: &str = "n/a";
const RANK_FIXTURE_INVALID: &str = "not a ranking fixture";
const RANK_K_ZERO: &str = "k must be at least 1";
const RANK_NOTHING_RELEVANT: &str = "no case labels a relevant candidate";
const RANK_CASE_EMPTY: &str = "a case needs a query and candidates";
const RANK_RELEVANT_OUT_OF_RANGE: &str = "a relevant index is not a candidate";
const RANK_CANDIDATE_UNBOUNDED: &str = "a candidate exceeds the bounded decision context";
const NOTE_INVALID: &str = "a memory candidate needs a name, a heading and a body";
const NEW_NOTE_INVALID: &str =
    "a duplicates query is the new note's name, a newline, then its text";
const NEW_NOTE_SEPARATOR: char = '\n';
const SYMBOL_ROW_INVALID: &str = "a code_context candidate is not a symbol row";
const TOOL_RESULT_INVALID: &str =
    "a compaction_retention candidate needs a tool, an input and its content";
const EVAL_OUTPUT_ID: &str = "output-eval";
const POINTER_QUERY_INVALID: &str =
    "an output_pointers query is the shell command, a newline, then the request";
const POINTER_QUERY_SEPARATOR: char = '\n';
const POINTER_WINDOW_INVALID: &str =
    "an output_pointers case holds at most 16 windows, each a string no longer than a window";
const POINTER_TOOL: &str = "shell";
const POINTER_COMMAND_KEY: &str = "command";
const POINTER_OUTPUT_HEAD: &str = "$ synthetic output";
const POINTER_OUTPUT_TAIL: &str = "[exit status 1]";
const POINTER_LINE_BREAK: &str = "\n";
/// The output's head is its first line, so the omitted lines start at the
/// second.
const POINTER_FIRST_OMITTED: usize = 2;

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

/// One hand-labelled reranking fixture: candidates in their lexical order,
/// and which of them are relevant to the query.
#[derive(Deserialize)]
struct RankFixture {
    k: usize,
    baseline: RankTally,
    #[serde(default)]
    baseline_note: Option<String>,
    cases: Vec<RankCase>,
}

#[derive(Deserialize)]
struct RankCase {
    query: String,
    candidates: Vec<Value>,
    relevant: BTreeSet<usize>,
}

/// A note in a memory fixture, as the store would offer it.
#[derive(Deserialize)]
struct FixtureNote {
    name: String,
    heading: String,
    body: String,
}

#[derive(Deserialize)]
struct SymbolRow {
    name: String,
    kind: String,
    path: String,
    line_start: usize,
    line_end: usize,
    callers: usize,
    calls: usize,
}

#[derive(Deserialize)]
struct FixtureToolResult {
    tool: String,
    input: Value,
    content: String,
}

/// An advisory baseline carries a false-advice rate; an enforcing one does
/// not.
#[derive(Deserialize)]
struct RankTally {
    ndcg_at_k: f64,
    recall_at_k: f64,
    #[serde(default)]
    false_advice_rate: Option<f64>,
    #[serde(default)]
    p50_ms: u64,
    #[serde(default)]
    p95_ms: u64,
}

impl RankTally {
    fn summary(&self) -> String {
        let false_advice = self
            .false_advice_rate
            .map_or_else(|| NOT_ADVISORY.to_owned(), |rate| format!("{rate:.3}"));
        format!(
            "nDCG@k {:.3}, recall@k {:.3}, false advice {false_advice}, p50 {} ms, p95 {} ms",
            self.ndcg_at_k, self.recall_at_k, self.p50_ms, self.p95_ms
        )
    }
}

/// What a surface presents for one case: every candidate index, best first,
/// and the ones it names as advice.
struct Ranking {
    order: Vec<usize>,
    advised: Vec<usize>,
    latency_ms: u64,
}

#[derive(Default)]
struct RankMeter {
    ndcg: Vec<f64>,
    recall: Vec<f64>,
    advised: usize,
    false_advice: usize,
    latencies: Vec<u64>,
}

impl RankMeter {
    fn add(&mut self, ranking: &Ranking, relevant: &BTreeSet<usize>, k: usize) {
        if !relevant.is_empty() {
            self.ndcg.push(ndcg_at(&ranking.order, relevant, k));
            self.recall.push(recall_at(&ranking.order, relevant, k));
        }
        self.advised += ranking.advised.len();
        self.false_advice += ranking
            .advised
            .iter()
            .filter(|index| !relevant.contains(index))
            .count();
        self.latencies.push(ranking.latency_ms);
    }

    fn tally(mut self, advisory: bool) -> RankTally {
        self.latencies.sort_unstable();
        RankTally {
            ndcg_at_k: mean(&self.ndcg),
            recall_at_k: mean(&self.recall),
            false_advice_rate: advisory.then(|| match self.advised {
                0 => 0.0,
                advised => self.false_advice as f64 / advised as f64,
            }),
            p50_ms: percentile(&self.latencies, P50),
            p95_ms: percentile(&self.latencies, P95),
        }
    }
}

#[test]
#[ignore = "needs CAUDRA_DECISION_EVAL_URL"]
fn decision_eval() {
    let url = env::var(URL_ENV).expect(URL_MISSING);
    let temp = tempfile::tempdir().unwrap();
    let decisions = &service(&url, StateDir::from_path(temp.path().into()));
    let judge = &Judge {
        decisions: decisions.clone(),
        context: DecisionContext::default(),
        permissions: None,
        cancel: CancelToken::none(),
    };
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
            run_ranked(
                "memory_search",
                include_str!("eval/memory_search.json"),
                |case| memory_search_case(judge, case),
            )
            .await,
            run_ranked(
                "memory_recall",
                include_str!("eval/memory_recall.json"),
                |case| memory_recall_case(judge, case),
            )
            .await,
            run_ranked(
                "memory_duplicates",
                include_str!("eval/memory_duplicates.json"),
                |case| memory_duplicates_case(judge, case),
            )
            .await,
            run_ranked(
                "code_context",
                include_str!("eval/code_context.json"),
                |case| code_context_case(decisions, case),
            )
            .await,
            run_ranked(
                "compaction_retention",
                include_str!("eval/compaction_retention.json"),
                |case| compaction_retention_case(decisions, case),
            )
            .await,
            run_ranked(
                "output_pointers",
                include_str!("eval/output_pointers.json"),
                |case| output_pointers_case(decisions, case),
            )
            .await,
        ]
    });
    assert!(passed.iter().all(|passed| *passed), "{REGRESSED}");
}

fn service(url: &str, state_dir: StateDir) -> Decisions {
    let protocol = env::var(PROTOCOL_ENV).map_or(DecisionProtocol::TypeSafe, |value| {
        serde_json::from_value(Value::String(value)).expect(PROTOCOL_INVALID)
    });
    let mut raw = RawDecisionsConfig::default();
    raw.protocol = Some(protocol);
    raw.base_url = Some(url.into());
    raw.model = env::var(MODEL_ENV).ok();
    raw.allow_remote = Some(true);
    raw.allow_http = Some(true);
    raw.timeout_ms = Some(TIMEOUT_MS);
    raw.log = Some(false);
    let mut config = raw.resolve(None).expect(SERVICE_INVALID);
    println!(
        "Decision evaluation: protocol={} model={}",
        config.protocol.as_str(),
        config.model
    );
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
    features.memory_search = FeatureMode::Enforce;
    features.memory_recall = FeatureMode::Advise;
    features.memory_duplicates = FeatureMode::Advise;
    features.code_context = FeatureMode::Enforce;
    features.compaction_retention = FeatureMode::Enforce;
    features.output_pointers = FeatureMode::Advise;
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

/// Scores each case's ranking against its labels, next to the lexical order
/// the surface falls back to, and passes when no metric is worse than the
/// stored baseline.
async fn run_ranked<F: Future<Output = Ranking>>(
    feature: &str,
    fixture: &str,
    rank: impl Fn(RankCase) -> F,
) -> bool {
    let fixture = rank_fixture(fixture)
        .unwrap_or_else(|error| panic!("{feature} fixture is invalid: {error}"));
    if let Some(note) = &fixture.baseline_note {
        println!("{feature} baseline: {note}");
    }
    let mut measured = RankMeter::default();
    let mut lexical = RankMeter::default();
    for case in fixture.cases {
        let query = case.query.clone();
        let relevant = case.relevant.clone();
        let fallback = Ranking {
            order: (0..case.candidates.len()).collect(),
            advised: Vec::new(),
            latency_ms: 0,
        };
        let ranking = rank(case).await;
        let missed_top = !relevant.is_empty()
            && ranking
                .order
                .first()
                .is_none_or(|index| !relevant.contains(index));
        let false_advice = ranking
            .advised
            .iter()
            .any(|index| !relevant.contains(index));
        if missed_top || false_advice {
            println!(
                "  {feature} {query:?}: order {:?}, advised {:?}, relevant {relevant:?}",
                ranking.order, ranking.advised
            );
        }
        measured.add(&ranking, &relevant, fixture.k);
        lexical.add(&fallback, &relevant, fixture.k);
    }
    let advisory = fixture.baseline.false_advice_rate.is_some();
    let measured = measured.tally(advisory);
    println!(
        "{feature} at k={}\n  measured {}\n  lexical  {}\n  baseline {}",
        fixture.k,
        measured.summary(),
        lexical.tally(advisory).summary(),
        fixture.baseline.summary()
    );
    measured.ndcg_at_k >= fixture.baseline.ndcg_at_k
        && measured.recall_at_k >= fixture.baseline.recall_at_k
        && fixture.baseline.false_advice_rate.is_none_or(|baseline| {
            measured
                .false_advice_rate
                .is_some_and(|rate| rate <= baseline)
        })
}

/// Parses a ranking fixture and checks its shape offline, so a malformed
/// fixture fails without an engine.
fn rank_fixture(source: &str) -> Result<RankFixture, &'static str> {
    let fixture: RankFixture = serde_json::from_str(source).map_err(|_| RANK_FIXTURE_INVALID)?;
    if fixture.k == 0 {
        return Err(RANK_K_ZERO);
    }
    if fixture.cases.iter().all(|case| case.relevant.is_empty()) {
        return Err(RANK_NOTHING_RELEVANT);
    }
    for case in &fixture.cases {
        if case.query.trim().is_empty() || case.candidates.is_empty() {
            return Err(RANK_CASE_EMPTY);
        }
        if case
            .relevant
            .last()
            .is_some_and(|index| *index >= case.candidates.len())
        {
            return Err(RANK_RELEVANT_OUT_OF_RANGE);
        }
        if case
            .candidates
            .iter()
            .any(|candidate| DecisionState::new(candidate).is_err())
        {
            return Err(RANK_CANDIDATE_UNBOUNDED);
        }
    }
    Ok(fixture)
}

fn ndcg_at(order: &[usize], relevant: &BTreeSet<usize>, k: usize) -> f64 {
    let gained: f64 = order
        .iter()
        .take(k)
        .enumerate()
        .filter(|(_, index)| relevant.contains(index))
        .map(|(rank, _)| discount(rank))
        .sum();
    let ideal: f64 = (0..relevant.len().min(k)).map(discount).sum();
    gained / ideal
}

fn discount(rank: usize) -> f64 {
    (rank as f64 + DCG_RANK_OFFSET).log2().recip()
}

fn recall_at(order: &[usize], relevant: &BTreeSet<usize>, k: usize) -> f64 {
    let found = order
        .iter()
        .take(k)
        .filter(|index| relevant.contains(index))
        .count();
    found as f64 / relevant.len() as f64
}

fn mean(values: &[f64]) -> f64 {
    match values.len() {
        0 => 0.0,
        count => values.iter().sum::<f64>() / count as f64,
    }
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
    let chosen = question_tool_nudge::nudge_probability(outcome, threshold).is_some();
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
        assert!(state.value().to_string().len() <= MAX_STATE_BYTES);
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
        newly_failing: false,
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
    let chosen = ranking
        .ok()
        .and_then(|ranking| ranking.top())
        .map(|index| case.candidates[index].name.as_str());
    let verdict = matched(true, chosen, expected_choice(&case.expect));
    scored(
        (&case.query, &case.expect),
        chosen,
        vec![verdict],
        latency_ms,
    )
}

async fn memory_search_case(judge: &Judge, case: RankCase) -> Ranking {
    let notes = memory_notes(&case.candidates).expect(RANK_FIXTURE_INVALID);
    let started = Instant::now();
    let (ranking, _) = relevance::rank_search(judge, &case.query, &notes).await;
    Ranking {
        order: ranking.map_or_else(|| lexical(&notes), |ranking| ranking.kept),
        advised: Vec::new(),
        latency_ms: latency_since(started),
    }
}

async fn memory_recall_case(judge: &Judge, case: RankCase) -> Ranking {
    let notes = memory_notes(&case.candidates).expect(RANK_FIXTURE_INVALID);
    let started = Instant::now();
    let advice = relevance::recall_advice(judge, &case.query, &notes).await;
    advised(advice.ok(), &notes, started)
}

async fn memory_duplicates_case(judge: &Judge, case: RankCase) -> Ranking {
    let notes = memory_notes(&case.candidates).expect(RANK_FIXTURE_INVALID);
    let (name, content) = new_note(&case.query).expect(RANK_FIXTURE_INVALID);
    let started = Instant::now();
    let advice = relevance::duplicate_advice(judge, name, content, &notes).await;
    advised(advice.ok(), &notes, started)
}

/// The notes a memory case offers, in its order, as the store would.
fn memory_notes(candidates: &[Value]) -> Result<Vec<Candidate>, &'static str> {
    (0..)
        .zip(candidates)
        .map(|(seq, candidate)| {
            let note = FixtureNote::deserialize(candidate).map_err(|_| NOTE_INVALID)?;
            Ok(Candidate {
                hit: Hit {
                    seq,
                    name: note.name,
                    heading: note.heading,
                    line: None,
                },
                excerpt: note.body,
            })
        })
        .collect()
}

fn new_note(query: &str) -> Result<(&str, &str), &'static str> {
    query
        .split_once(NEW_NOTE_SEPARATOR)
        .filter(|(name, content)| !name.trim().is_empty() && !content.trim().is_empty())
        .ok_or(NEW_NOTE_INVALID)
}

fn lexical(notes: &[Candidate]) -> Vec<usize> {
    (0..notes.len()).collect()
}

fn advised(advice: Option<Advice>, notes: &[Candidate], started: Instant) -> Ranking {
    let latency_ms = latency_since(started);
    match advice {
        Some(advice) => Ranking {
            order: advice.order(),
            advised: advice.advised,
            latency_ms,
        },
        None => Ranking {
            order: lexical(notes),
            advised: Vec::new(),
            latency_ms,
        },
    }
}

async fn code_context_case(decisions: &Decisions, case: RankCase) -> Ranking {
    let mut rows: Vec<(usize, SymbolRow)> = case
        .candidates
        .into_iter()
        .map(|candidate| serde_json::from_value(candidate).expect(SYMBOL_ROW_INVALID))
        .enumerate()
        .collect();
    let started = Instant::now();
    rerank_code_context(
        &mut rows,
        |(_, row)| CodeSymbol {
            name: &row.name,
            kind: &row.kind,
            path: &row.path,
            line_start: row.line_start,
            line_end: row.line_end,
            callers: row.callers,
            calls: row.calls,
        },
        &case.query,
        decisions,
        &DecisionContext::default(),
        None,
        &CancelToken::none(),
    )
    .await;
    Ranking {
        order: rows.iter().map(|(index, _)| *index).collect(),
        advised: Vec::new(),
        latency_ms: latency_since(started),
    }
}

async fn compaction_retention_case(decisions: &Decisions, case: RankCase) -> Ranking {
    let results = tool_results(&case.candidates).expect(TOOL_RESULT_INVALID);
    let views: Vec<ToolResultView<'_>> = results
        .iter()
        .map(|result| ToolResultView {
            tool: &result.tool,
            input: Some(&result.input),
            content: &result.content,
            output_id: Some(EVAL_OUTPUT_ID),
        })
        .collect();
    let retention = Retention {
        decisions: decisions.clone(),
        context: DecisionContext::default(),
        query: retention_query(None, &[], Some(&case.query)).expect(RANK_CASE_EMPTY),
        permissions: None,
    };
    let started = Instant::now();
    let kept = select_tool_results(&retention, &views, &CancelToken::none())
        .await
        .map(|selection| selection.kept)
        .unwrap_or_default();
    Ranking {
        order: kept
            .iter()
            .copied()
            .chain((0..views.len()).filter(|index| !kept.contains(index)))
            .collect(),
        advised: Vec::new(),
        latency_ms: latency_since(started),
    }
}

/// Lays the case's windows out as one output, each padded with blank lines
/// to a whole window, and asks the feature which of them to point at.
async fn output_pointers_case(decisions: &Decisions, case: RankCase) -> Ranking {
    let (command, request) = pointer_query(&case.query).expect(POINTER_QUERY_INVALID);
    let windows = pointer_windows(&case.candidates).expect(POINTER_WINDOW_INVALID);
    let text = pointer_output(&windows);
    let focus = output_focus(&[], Some(request)).expect(RANK_CASE_EMPTY);
    let input = json!({POINTER_COMMAND_KEY: command});
    let started = Instant::now();
    let advice = advise_pointers(
        OmittedOutput {
            focus: &focus,
            tool: POINTER_TOOL,
            input: &input,
            text: &text,
            lines: POINTER_FIRST_OMITTED
                ..=POINTER_FIRST_OMITTED + windows.len() * MIN_WINDOW_LINES - 1,
        },
        decisions,
        &DecisionContext::default(),
        None,
        &CancelToken::none(),
    )
    .await;
    let latency_ms = latency_since(started);
    match advice {
        Ok(advice) => Ranking {
            order: advice.order(),
            advised: advice.advised,
            latency_ms,
        },
        Err(_) => Ranking {
            order: (0..windows.len()).collect(),
            advised: Vec::new(),
            latency_ms,
        },
    }
}

fn pointer_query(query: &str) -> Result<(&str, &str), &'static str> {
    query
        .split_once(POINTER_QUERY_SEPARATOR)
        .filter(|(command, request)| !command.trim().is_empty() && !request.trim().is_empty())
        .ok_or(POINTER_QUERY_INVALID)
}

fn pointer_windows(candidates: &[Value]) -> Result<Vec<&str>, &'static str> {
    if candidates.len() > MAX_RERANK_BATCH {
        return Err(POINTER_WINDOW_INVALID);
    }
    candidates
        .iter()
        .map(|candidate| {
            candidate
                .as_str()
                .filter(|window| window.split(POINTER_LINE_BREAK).count() <= MIN_WINDOW_LINES)
                .ok_or(POINTER_WINDOW_INVALID)
        })
        .collect()
}

fn pointer_output(windows: &[&str]) -> String {
    let mut lines = vec![POINTER_OUTPUT_HEAD];
    for window in windows {
        let window_lines: Vec<&str> = window.split(POINTER_LINE_BREAK).collect();
        let padding = MIN_WINDOW_LINES - window_lines.len();
        lines.extend(window_lines);
        lines.extend(iter::repeat_n("", padding));
    }
    lines.push(POINTER_OUTPUT_TAIL);
    lines.join(POINTER_LINE_BREAK)
}

#[test]
fn output_pointers_fixture_cases_lay_out_whole_windows() {
    let fixture =
        rank_fixture(include_str!("eval/output_pointers.json")).expect(POINTER_WINDOW_INVALID);
    for case in &fixture.cases {
        pointer_query(&case.query).unwrap();
        let windows = pointer_windows(&case.candidates).unwrap();
        let output = pointer_output(&windows);
        assert_eq!(
            output.split(POINTER_LINE_BREAK).count(),
            POINTER_FIRST_OMITTED + windows.len() * MIN_WINDOW_LINES
        );
    }
}

fn tool_results(candidates: &[Value]) -> Result<Vec<FixtureToolResult>, &'static str> {
    candidates
        .iter()
        .map(|candidate| FixtureToolResult::deserialize(candidate).map_err(|_| TOOL_RESULT_INVALID))
        .collect()
}

#[test]
fn compaction_retention_fixture_candidates_are_tool_results() {
    let fixture =
        rank_fixture(include_str!("eval/compaction_retention.json")).expect(TOOL_RESULT_INVALID);
    for case in &fixture.cases {
        tool_results(&case.candidates).expect(TOOL_RESULT_INVALID);
    }
}

#[test]
fn code_context_fixture_candidates_are_symbol_rows() {
    let fixture = rank_fixture(include_str!("eval/code_context.json")).expect(SYMBOL_ROW_INVALID);
    for candidate in fixture.cases.into_iter().flat_map(|case| case.candidates) {
        serde_json::from_value::<SymbolRow>(candidate).expect(SYMBOL_ROW_INVALID);
    }
}

const SYNTHETIC_FEATURE: &str = "synthetic";
const RANK_QUERY: &str = "how are releases tagged";
const METRIC_TOLERANCE: f64 = 1e-12;
const RANK_ENFORCED: &str = r#"{
    "k": 2,
    "baseline": {"ndcg_at_k": 0.9, "recall_at_k": 0.9},
    "cases": [
        {"query": "how are releases tagged", "candidates": [{"name": "build"}, {"name": "lint"}, {"name": "release"}], "relevant": [2]},
        {"query": "deploy to staging", "candidates": [{"name": "fonts"}], "relevant": []}
    ]
}"#;
const RANK_ADVISED: &str = r#"{
    "k": 2,
    "baseline": {"ndcg_at_k": 0.9, "recall_at_k": 0.9, "false_advice_rate": 0.0},
    "cases": [
        {"query": "how are releases tagged", "candidates": [{"name": "build"}, {"name": "lint"}, {"name": "release"}], "relevant": [2]},
        {"query": "deploy to staging", "candidates": [{"name": "fonts"}], "relevant": []}
    ]
}"#;

fn rank_source(k: usize, candidates: Value, relevant: Value) -> String {
    json!({
        "k": k,
        "baseline": {"ndcg_at_k": 0.0, "recall_at_k": 0.0},
        "cases": [{"query": RANK_QUERY, "candidates": candidates, "relevant": relevant}],
    })
    .to_string()
}

#[test_case(RANK_ENFORCED, None; "enforced")]
#[test_case(RANK_ADVISED, None; "advised")]
#[test_case(include_str!("eval/code_context.json"), None; "code_context")]
#[test_case(include_str!("eval/compaction_retention.json"), None; "compaction_retention")]
#[test_case(include_str!("eval/output_pointers.json"), None; "output_pointers")]
#[test_case("{}", Some(RANK_FIXTURE_INVALID); "not_a_fixture")]
#[test_case(&rank_source(0, json!([{}]), json!([0])), Some(RANK_K_ZERO); "k_zero")]
#[test_case(&rank_source(1, json!([{}]), json!([])), Some(RANK_NOTHING_RELEVANT); "nothing_relevant")]
#[test_case(&rank_source(1, json!([]), json!([0])), Some(RANK_CASE_EMPTY); "no_candidates")]
#[test_case(&rank_source(1, json!([{}]), json!([1])), Some(RANK_RELEVANT_OUT_OF_RANGE); "relevant_out_of_range")]
#[test_case(&rank_source(1, json!([{"text": "x".repeat(MAX_STATE_BYTES)}]), json!([0])), Some(RANK_CANDIDATE_UNBOUNDED); "unbounded_candidate")]
fn ranking_fixture_shape_is_checked_offline(source: &str, error: Option<&str>) {
    assert_eq!(rank_fixture(source).err(), error);
}

#[test_case(&[0, 1, 2], &[0], 3, 1.0, 1.0; "relevant_first")]
#[test_case(&[1, 0, 2], &[0], 3, discount(1), 1.0; "relevant_second")]
#[test_case(&[1, 2, 0], &[0], 2, 0.0, 0.0; "relevant_beyond_k")]
#[test_case(&[0, 2, 1], &[0, 1], 2, 1.0 / (1.0 + discount(1)), 0.5; "half_found")]
fn ranking_metrics_score_order_against_labels(
    order: &[usize],
    relevant: &[usize],
    k: usize,
    ndcg: f64,
    recall: f64,
) {
    let relevant: BTreeSet<usize> = relevant.iter().copied().collect();
    assert!((ndcg_at(order, &relevant, k) - ndcg).abs() < METRIC_TOLERANCE);
    assert!((recall_at(order, &relevant, k) - recall).abs() < METRIC_TOLERANCE);
}

#[test_case(RANK_ENFORCED, true, true, true; "enforced_ranking_meets_baseline")]
#[test_case(RANK_ENFORCED, false, true, false; "lexical_order_regresses")]
#[test_case(RANK_ADVISED, true, true, true; "relevant_advice_meets_baseline")]
#[test_case(RANK_ADVISED, true, false, false; "false_advice_regresses")]
fn ranked_runs_pass_only_at_or_above_the_baseline(
    fixture: &str,
    relevant_first: bool,
    advise_relevant: bool,
    passed: bool,
) {
    let outcome = smol::block_on(run_ranked(
        SYNTHETIC_FEATURE,
        fixture,
        move |case| async move {
            let (mut order, rest): (Vec<usize>, Vec<usize>) = (0..case.candidates.len())
                .partition(|index| relevant_first && case.relevant.contains(index));
            order.extend(rest);
            let advised = match advise_relevant {
                true => case.relevant.iter().copied().collect(),
                false => vec![0],
            };
            Ranking {
                order,
                advised,
                latency_ms: 0,
            }
        },
    ));
    assert_eq!(outcome, passed);
}

#[test_case(include_str!("eval/memory_search.json"), false; "memory_search")]
#[test_case(include_str!("eval/memory_recall.json"), false; "memory_recall")]
#[test_case(include_str!("eval/memory_duplicates.json"), true; "memory_duplicates")]
fn memory_fixtures_hold_what_their_surfaces_take(source: &str, new_note_query: bool) {
    let fixture = rank_fixture(source).unwrap();
    for case in &fixture.cases {
        memory_notes(&case.candidates).unwrap();
        if new_note_query {
            new_note(&case.query).unwrap();
        }
    }
}

#[test_case(json!({"name": "a.md", "heading": "A"}); "no_body")]
#[test_case(json!("a.md"); "not_a_note")]
fn a_memory_candidate_must_be_a_note(candidate: Value) {
    assert_eq!(memory_notes(&[candidate]).err(), Some(NOTE_INVALID));
}

#[test_case("a.md"; "no_text")]
#[test_case("a.md\n "; "blank_text")]
#[test_case("\nA note"; "no_name")]
fn a_duplicates_query_must_name_the_new_note(query: &str) {
    assert_eq!(new_note(query).err(), Some(NEW_NOTE_INVALID));
}
