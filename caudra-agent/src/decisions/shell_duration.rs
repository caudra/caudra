use caudra_config::decisions::FeatureMode;
use caudra_decision::{Answer, DecisionResponse};
use caudra_storage::{
    decision_log::DecisionLabel,
    now_epoch,
    sessions::SessionError,
    shell_durations::{
        CommandDigest, DurationOutcome, DurationSource, FamilyRuns, ShellDurationKey,
        ShellDurations,
    },
};
use serde_json::{Value, json};
use std::{collections::VecDeque, sync::Arc, thread, time::Instant};

use super::{DecisionFeature, DecisionReceipt, Decisions, push_fitting, questions};
use crate::{
    ToolDoneEvent, ToolOutput,
    permissions::{PassiveDecisionRevision, canonical_json_sha256, command_pattern},
    tools::{ToolContext, ToolExecResult},
};

pub(super) const DURATION_QUESTION: &str = "duration";
pub(super) const ENDLESS_QUESTION: &str = "endless";
/// Score agreement in stats means landing on the nearest level.
pub(super) const LEVEL_TOLERANCE: f64 = 0.5;
const COMMAND_FIELD: &str = "command";
const WORKDIR_FIELD: &str = "workdir";
const TIMEOUT_FIELD: &str = "timeoutSec";
const EARLIER_RUNS: &str = "earlier_runs";
const MAX_EARLIER_RUNS: usize = 4;
const HISTORY_CACHE_ENTRIES: usize = 256;
const MILLIS_PER_SECOND: u64 = 1_000;
const AT_ONCE: usize = 0;
const SECONDS: usize = 1;
const MINUTES: usize = 2;
const UNTIL_STOPPED: usize = 3;
/// Measured label boundaries, fixed so a label never depends on configuration:
/// a run completing within `INSTANT_MS` exits at once, and one lasting
/// `MINUTES_MS` or longer runs for minutes.
const INSTANT_MS: u64 = MILLIS_PER_SECOND;
const MINUTES_MS: u64 = 120 * MILLIS_PER_SECOND;
const SECONDS_P50_MS: u64 = 10 * MILLIS_PER_SECOND;
const MINUTES_P50_MS: u64 = 5 * 60 * MILLIS_PER_SECOND;
const MINUTES_P90_MS: u64 = 10 * 60 * MILLIS_PER_SECOND;
const ENGINE_PRIOR: &str = "engine prior";
/// The estimate each measurable level stands for, by level.
const LEVELS: [Estimate; 3] = [
    Estimate::prior(INSTANT_MS, INSTANT_MS, false),
    Estimate::prior(SECONDS_P50_MS, MINUTES_MS, false),
    Estimate::prior(MINUTES_P50_MS, MINUTES_P90_MS, true),
];
/// How each measurable level reads in `earlier_runs`, by level, in the words
/// of the level descriptions.
const TOOK: [&str; 3] = [
    "exited at once, with no noticeable wait",
    "took a few seconds before it exited",
    "exited after minutes",
];
const STOPPED: &str = "did not finish before it was stopped";
const ENDLESS_WARNING: &str =
    "Duration estimate: this command may run until stopped; its execution deadline still applies.";

#[derive(Clone, Debug)]
struct Estimate {
    p50_ms: u64,
    p90_ms: u64,
    samples: u64,
    source: &'static str,
    extend_timeout: bool,
}

impl Estimate {
    const fn prior(p50_ms: u64, p90_ms: u64, extend_timeout: bool) -> Self {
        Self {
            p50_ms,
            p90_ms,
            samples: 0,
            source: ENGINE_PRIOR,
            extend_timeout,
        }
    }
}

#[derive(Default)]
pub(super) struct ShellDurationCache {
    entries: VecDeque<(ShellDurationKey, Estimate)>,
    revision: u64,
}

impl ShellDurationCache {
    fn get(&self, key: &ShellDurationKey) -> Option<Estimate> {
        self.entries
            .iter()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, estimate)| estimate.clone())
    }

    fn insert(&mut self, key: ShellDurationKey, estimate: Estimate, revision: u64) {
        if self.revision != revision {
            return;
        }
        self.entries.retain(|(candidate, _)| candidate != &key);
        if self.entries.len() == HISTORY_CACHE_ENTRIES {
            self.entries.pop_front();
        }
        self.entries.push_back((key, estimate));
    }

    fn invalidate(&mut self, key: &ShellDurationKey) {
        self.revision = self.revision.wrapping_add(1);
        self.entries.retain(|(candidate, _)| {
            candidate.workspace != key.workspace || candidate.family != key.family
        });
    }
}

#[derive(Clone)]
pub(crate) struct ShellDurationPlan {
    decisions: Decisions,
    revision: PassiveDecisionRevision,
    key: ShellDurationKey,
    estimate: Option<Estimate>,
    endless: bool,
    receipt: Option<DecisionReceipt>,
    requested_timeout: Option<u64>,
    injected_timeout: Option<u64>,
}

impl Decisions {
    pub(crate) async fn shell_duration(
        &self,
        input: &Value,
        ctx: &ToolContext,
    ) -> Option<ShellDurationPlan> {
        let decision_revision = ctx.permissions.passive_decision_revision()?;
        if *self.mode(&DecisionFeature::ShellDuration) == FeatureMode::Off
            || ctx.cancel.is_cancelled()
            || ctx.host_cwd.is_some()
            || ctx.workspace_session.is_some()
            || ctx.remote_project_context.is_some()
        {
            return None;
        }
        let command = input.get(COMMAND_FIELD)?.as_str()?;
        let root = ctx.permissions.project_cwd();
        let workdir = input.get(WORKDIR_FIELD).and_then(Value::as_str);
        let key = history_key(&root.display().to_string(), workdir.unwrap_or("."), command);
        let (cached, revision) = {
            let cache = self
                .0
                .shell_duration_cache
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            (cache.get(&key), cache.revision)
        };
        let mut plan = ShellDurationPlan {
            decisions: self.clone(),
            revision: decision_revision,
            key,
            estimate: cached,
            endless: false,
            receipt: None,
            requested_timeout: input.get(TIMEOUT_FIELD).and_then(Value::as_u64),
            injected_timeout: None,
        };
        if plan.estimate.is_some() {
            return Some(plan);
        }
        let history_key = plan.key.clone();
        let dir = self.0.state_dir.clone();
        let history = smol::unblock(move || {
            let durations = ShellDurations::open(&dir)?;
            let estimate = durations.estimate(&history_key)?;
            let related = match estimate {
                Some(_) => Vec::new(),
                None => durations.related(&history_key, MAX_EARLIER_RUNS)?,
            };
            Ok::<_, SessionError>((estimate, related))
        })
        .await;
        let earlier_runs = match history {
            Ok((Some(estimate), _)) => {
                let estimate = Estimate {
                    p50_ms: estimate.p50_ms,
                    p90_ms: estimate.p90_ms,
                    samples: estimate.samples,
                    source: match estimate.source {
                        DurationSource::Exact => "exact history",
                        DurationSource::Family => "command family history",
                    },
                    extend_timeout: true,
                };
                self.0
                    .shell_duration_cache
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .insert(plan.key.clone(), estimate.clone(), revision);
                plan.estimate = Some(estimate);
                return Some(plan);
            }
            Ok((None, related)) => related,
            Err(error) => {
                tracing::warn!(%error, "shell duration history unavailable");
                Vec::new()
            }
        };
        if let Some(questions) = questions::SHELL_DURATION.as_ref() {
            let context = ctx.permissions.decision_context(Value::Null);
            let state = duration_state(command, workdir, earlier_runs.iter().map(earlier_run));
            if let Some(outcome) = self
                .evaluate(DecisionFeature::ShellDuration, &state, questions, &context)
                .await
            {
                plan.receipt = outcome.receipt;
                if let Ok(response) = outcome.result {
                    let thresholds = &self.config().thresholds;
                    let level = prior(
                        &response,
                        thresholds.shell_duration,
                        thresholds.shell_endless,
                    );
                    plan.endless = level == Some(UNTIL_STOPPED);
                    plan.estimate = level.and_then(|level| LEVELS.get(level)).cloned();
                }
            }
        }
        Some(plan)
    }
}

pub(crate) fn history_key(workspace: &str, workdir: &str, command: &str) -> ShellDurationKey {
    // Even trailing whitespace can be escaped; retain the exact shell program.
    let family = command_pattern::reusable_prefix(command)
        .unwrap_or_else(|| canonical_json_sha256(&json!(command)));
    ShellDurationKey {
        workspace: canonical_json_sha256(&json!([workspace, workdir])),
        family,
        digest: CommandDigest::of_normalized(command),
    }
}

/// `workdir` appears only when the call names one, and `earlier_runs` keeps
/// as many entries as the state cap allows.
pub(super) fn duration_state(
    command: &str,
    workdir: Option<&str>,
    earlier_runs: impl IntoIterator<Item = Value>,
) -> Value {
    let mut state = json!({COMMAND_FIELD: command});
    if let Some(workdir) = workdir {
        state[WORKDIR_FIELD] = json!(workdir);
    }
    push_fitting(&mut state, EARLIER_RUNS, earlier_runs);
    state
}

/// A family that ever completed reads as the level of its median run; one
/// that never did reads as stopped.
fn earlier_run(runs: &FamilyRuns) -> Value {
    let (took, count) = match runs.p50_ms {
        Some(p50_ms) => (TOOK[completed_level(p50_ms)], runs.completed),
        None => (STOPPED, runs.stopped),
    };
    json!({COMMAND_FIELD: runs.family, "took": took, "runs": count})
}

/// The level the answers settle on. Until stopped wins when either signal
/// reaches `endless`; otherwise the first cumulative bound reaching
/// `duration` decides, so a split between minutes and until stopped is still
/// at least minutes.
pub(super) fn prior(response: &DecisionResponse, duration: f64, endless: f64) -> Option<usize> {
    let score = match response.answers.get(DURATION_QUESTION) {
        Some(Answer::Score(score)) => Some(score),
        _ => None,
    };
    let endless_noul = matches!(
        response.answers.get(ENDLESS_QUESTION),
        Some(Answer::Noul(answer)) if answer.noul >= endless
    );
    if endless_noul || score.is_some_and(|score| score.at_least(UNTIL_STOPPED) >= endless) {
        return Some(UNTIL_STOPPED);
    }
    let score = score?;
    [
        (MINUTES, score.at_least(MINUTES)),
        (AT_ONCE, score.at_most(AT_ONCE)),
        (SECONDS, score.at_most(SECONDS)),
    ]
    .into_iter()
    .find_map(|(level, mass)| (mass >= duration).then_some(level))
}

impl ShellDurationPlan {
    pub(crate) fn is_current(&self, ctx: &ToolContext) -> bool {
        ctx.permissions.passive_decision_is_current(self.revision)
            && ctx
                .permissions
                .decisions()
                .is_some_and(|decisions| Arc::ptr_eq(&self.decisions.0, &decisions.0))
    }

    pub(crate) fn discard_stale(plan: &mut Option<Self>, ctx: &ToolContext) -> bool {
        if plan.as_ref().is_some_and(|plan| !plan.is_current(ctx)) {
            return plan
                .take()
                .is_some_and(|plan| plan.injected_timeout.is_some());
        }
        false
    }

    fn visible(&self) -> bool {
        matches!(
            self.decisions.mode(&DecisionFeature::ShellDuration),
            FeatureMode::Advise | FeatureMode::Enforce
        )
    }

    pub(crate) fn inject_timeout(&mut self, input: &Value, schema: &Value) -> Option<Value> {
        if *self.decisions.mode(&DecisionFeature::ShellDuration) != FeatureMode::Enforce
            || input.get(TIMEOUT_FIELD).is_some()
            || self.endless
        {
            return None;
        }
        let estimate = self
            .estimate
            .as_ref()
            .filter(|estimate| estimate.extend_timeout)?;
        let default = schema.pointer("/properties/timeoutSec/default")?.as_u64()?;
        let cap = schema.pointer("/properties/timeoutSec/maximum")?.as_u64()?;
        if cap < default {
            return None;
        }
        let seconds = estimate
            .p90_ms
            .saturating_mul(3)
            .div_ceil(2 * MILLIS_PER_SECOND)
            .clamp(default, cap);
        let mut effective = input.clone();
        effective
            .as_object_mut()?
            .insert(TIMEOUT_FIELD.into(), json!(seconds));
        self.injected_timeout = Some(seconds);
        Some(effective)
    }

    pub(crate) fn expected_secs(&self) -> Option<u64> {
        (*self.decisions.mode(&DecisionFeature::ShellDuration) == FeatureMode::Enforce)
            .then(|| {
                self.estimate
                    .as_ref()
                    .map(|estimate| estimate.p90_ms.div_ceil(MILLIS_PER_SECOND))
            })
            .flatten()
    }

    pub(crate) fn annotation(&self) -> Option<String> {
        if !self.visible() {
            return None;
        }
        self.estimate.as_ref().map(|estimate| {
            format!(
                "ETA {}–{}s ({}; {} samples)",
                estimate.p50_ms.div_ceil(MILLIS_PER_SECOND),
                estimate.p90_ms.div_ceil(MILLIS_PER_SECOND),
                estimate.source,
                estimate.samples
            )
        })
    }

    pub(crate) fn advise(&self, done: &mut ToolDoneEvent, ctx: &ToolContext) {
        if !self.visible() || !self.is_current(ctx) {
            return;
        }
        let mut notes = Vec::new();
        if self.endless {
            notes.push(ENDLESS_WARNING.to_owned());
        }
        if let Some(estimate) = &self.estimate {
            if self
                .requested_timeout
                .is_some_and(|timeout| timeout.saturating_mul(MILLIS_PER_SECOND) < estimate.p90_ms)
            {
                notes.push(format!(
                    "The explicit timeout is below estimated p90 {}s ({}); it was not changed.",
                    estimate.p90_ms.div_ceil(MILLIS_PER_SECOND),
                    estimate.source
                ));
            }
            if let Some(timeout) = self.injected_timeout {
                notes.push(format!(
                    "Omitted timeout set to {timeout}s from p90 {}s ({}; {} samples).",
                    estimate.p90_ms.div_ceil(MILLIS_PER_SECOND),
                    estimate.source,
                    estimate.samples
                ));
            }
        }
        if !notes.is_empty() {
            if let Some(suffix) = done.model_suffix.take() {
                notes.insert(0, suffix);
            }
            done.model_suffix = Some(notes.join("\n\n"));
        }
    }

    pub(crate) fn begin(self) -> ShellDurationRun {
        ShellDurationRun {
            plan: Some(self),
            started: Instant::now(),
        }
    }

    async fn record(self, outcome: DurationOutcome, elapsed_ms: u64) {
        let dir = self.decisions.0.state_dir.clone();
        let key = self.key.clone();
        let label = duration_label(&outcome, elapsed_ms);
        let censored = outcome != DurationOutcome::Ok;
        if let Err(error) = smol::unblock(move || {
            ShellDurations::open(&dir)?.record(&self.key, outcome, elapsed_ms)
        })
        .await
        {
            tracing::warn!(%error, "shell duration observation could not be stored");
        }
        self.decisions
            .0
            .shell_duration_cache
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .invalidate(&key);
        if let Some(receipt) = self.receipt
            && let Some(duration) = label
            && let Err(error) = self
                .decisions
                .attach_label(
                    &receipt,
                    &DecisionLabel {
                        expected: json!({DURATION_QUESTION: duration}),
                        source: "measured".into(),
                        timestamp: now_epoch(),
                        meta: json!({"elapsed_ms": elapsed_ms, "censored": censored}),
                    },
                )
                .await
        {
            tracing::warn!(%error, "shell duration label could not be stored");
        }
    }
}

pub(crate) struct ShellDurationRun {
    plan: Option<ShellDurationPlan>,
    started: Instant,
}

impl ShellDurationRun {
    pub(crate) async fn finish(mut self, result: &ToolExecResult, cancelled: bool) {
        let outcome = match &result.output {
            Ok(ToolOutput::Shell(shell)) if shell.timed_out => Some(DurationOutcome::Timeout),
            _ if cancelled => Some(DurationOutcome::Cancelled),
            Ok(ToolOutput::Shell(shell))
                if shell.signal.is_some() || shell.output_limit_exceeded =>
            {
                Some(DurationOutcome::Cancelled)
            }
            Ok(ToolOutput::Shell(shell)) if shell.exit_code.is_some() => Some(DurationOutcome::Ok),
            _ => None,
        };
        let elapsed = self
            .started
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
        if let Some(plan) = self.plan.take()
            && let Some(outcome) = outcome
        {
            plan.record(outcome, elapsed).await;
        }
    }
}

impl Drop for ShellDurationRun {
    fn drop(&mut self) {
        if !thread::panicking()
            && let Some(plan) = self.plan.take()
        {
            let elapsed = self
                .started
                .elapsed()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX);
            smol::spawn(plan.record(DurationOutcome::Cancelled, elapsed)).detach();
        }
    }
}

/// A stopped run only shows how long it lasted, so it labels a level once it
/// outlasted the seconds level. Until stopped is never measured.
fn duration_label(outcome: &DurationOutcome, elapsed_ms: u64) -> Option<usize> {
    match outcome {
        DurationOutcome::Ok => Some(completed_level(elapsed_ms)),
        _ if elapsed_ms >= MINUTES_MS => Some(MINUTES),
        _ => None,
    }
}

fn completed_level(elapsed_ms: u64) -> usize {
    if elapsed_ms <= INSTANT_MS {
        AT_ONCE
    } else if elapsed_ms < MINUTES_MS {
        SECONDS
    } else {
        MINUTES
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AT_ONCE, DURATION_QUESTION, EARLIER_RUNS, ENDLESS_QUESTION, Estimate,
        HISTORY_CACHE_ENTRIES, INSTANT_MS, MAX_EARLIER_RUNS, MINUTES, MINUTES_MS, SECONDS, STOPPED,
        ShellDurationCache, ShellDurationPlan, TOOK, UNTIL_STOPPED, WORKDIR_FIELD, duration_label,
        duration_state, earlier_run, history_key, prior,
    };
    use crate::{
        AgentMode,
        decisions::{DecisionState, Decisions},
        permissions::PermissionManager,
        tools::{ToolExecResult, test_support::stub_ctx_with_permissions},
    };
    use async_trait::async_trait;
    use caudra_config::{
        PermissionsConfig,
        decisions::{DecisionsConfig, FeatureMode},
    };
    use caudra_decision::{DecisionEngine, DecisionError, DecisionRequest, DecisionResponse};
    use caudra_storage::{
        StateDir,
        shell_durations::{DurationOutcome, FamilyRuns, ShellDurations},
    };
    use serde_json::{Value, json};
    use std::{iter, sync::Arc, time::Instant};
    use test_case::test_case;

    const COMMAND: &str = "cargo test -p private-package";
    const FAMILY: &str = "cargo test *";
    const WORKSPACE: &str = "/project";
    const WORKDIR: &str = "crates/core";
    const THRESHOLD: f64 = 0.9;
    const LONG_FAMILY_BYTES: usize = 400;
    const DEFAULT_SECS: u64 = 120;
    const CAP_SECS: u64 = 21_600;
    const EXECUTION_ERROR: &str = "execution unavailable";
    const BASE_URL: &str = "http://127.0.0.1:1";
    const MODEL: &str = "jev-latest";

    struct FailedEngine(DecisionError);

    #[async_trait]
    impl DecisionEngine for FailedEngine {
        async fn decide(
            &self,
            _: &DecisionRequest,
            _: Instant,
        ) -> Result<DecisionResponse, DecisionError> {
            Err(self.0.clone())
        }
    }

    fn service(dir: &StateDir, mode: FeatureMode) -> Decisions {
        let mut config = DecisionsConfig::default();
        config.features.shell_duration = mode;
        Decisions::new(config, dir).unwrap()
    }

    fn plan(dir: &StateDir, mode: FeatureMode, p90_ms: u64) -> ShellDurationPlan {
        ShellDurationPlan {
            decisions: service(dir, mode),
            revision: PermissionManager::new_nonpersistent(
                PermissionsConfig::default(),
                WORKSPACE.into(),
                Arc::default(),
            )
            .passive_decision_revision()
            .unwrap(),
            key: history_key(WORKSPACE, ".", COMMAND),
            estimate: Some(Estimate {
                p50_ms: p90_ms,
                p90_ms,
                samples: 3,
                source: "exact history",
                extend_timeout: true,
            }),
            endless: false,
            receipt: None,
            requested_timeout: None,
            injected_timeout: None,
        }
    }

    fn response(probabilities: [f64; 4], endless: f64) -> DecisionResponse {
        let score: f64 = probabilities
            .iter()
            .enumerate()
            .map(|(level, probability)| level as f64 * probability)
            .sum();
        serde_json::from_value(json!({
            "model": MODEL,
            "answers": {
                DURATION_QUESTION: {
                    "type": "score",
                    "score": score,
                    "legend": {"0": "", "1": "", "2": "", "3": ""},
                    "probabilities": {
                        "0": probabilities[0],
                        "1": probabilities[1],
                        "2": probabilities[2],
                        "3": probabilities[3],
                    },
                    "confidence": THRESHOLD,
                },
                ENDLESS_QUESTION: {"type": "noul", "noul": endless},
            },
            "usage": {"input_tokens": 0, "output_tokens": 0},
        }))
        .unwrap()
    }

    #[test_case(FeatureMode::Enforce, 1_000, Some(DEFAULT_SECS))]
    #[test_case(FeatureMode::Enforce, 200_001, Some(301))]
    #[test_case(FeatureMode::Enforce, u64::MAX, Some(CAP_SECS))]
    #[test_case(FeatureMode::Advise, 200_001, None)]
    #[test_case(FeatureMode::Shadow, 200_001, None)]
    fn omitted_timeout_is_clamped_only_in_enforce(
        mode: FeatureMode,
        p90_ms: u64,
        expected: Option<u64>,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let dir = StateDir::from_path(temp.path().join("state"));
        let mut plan = plan(&dir, mode, p90_ms);
        let schema =
            json!({"properties":{"timeoutSec":{"default":DEFAULT_SECS,"maximum":CAP_SECS}}});
        let input = json!({"command": COMMAND});
        let effective = plan.inject_timeout(&input, &schema);
        assert_eq!(
            effective
                .as_ref()
                .and_then(|value| value["timeoutSec"].as_u64()),
            expected
        );
        for explicit in [json!(1), json!(CAP_SECS), Value::Null, json!("invalid")] {
            assert!(
                plan.inject_timeout(&json!({"command":COMMAND,"timeoutSec":explicit}), &schema)
                    .is_none()
            );
        }
        plan.endless = true;
        assert!(plan.inject_timeout(&input, &schema).is_none());
    }

    #[test_case(DurationOutcome::Ok, INSTANT_MS, Some(AT_ONCE); "completed_at_once")]
    #[test_case(DurationOutcome::Ok, INSTANT_MS + 1, Some(SECONDS); "completed_past_an_instant")]
    #[test_case(DurationOutcome::Ok, MINUTES_MS - 1, Some(SECONDS); "completed_within_seconds")]
    #[test_case(DurationOutcome::Ok, MINUTES_MS, Some(MINUTES); "completed_in_minutes")]
    #[test_case(DurationOutcome::Timeout, MINUTES_MS - 1, None; "timed_out_early")]
    #[test_case(DurationOutcome::Cancelled, MINUTES_MS - 1, None; "cancelled_early")]
    #[test_case(DurationOutcome::Timeout, MINUTES_MS, Some(MINUTES); "timed_out_after_minutes")]
    #[test_case(DurationOutcome::Cancelled, MINUTES_MS, Some(MINUTES); "cancelled_after_minutes")]
    fn duration_label_uses_fixed_boundaries(
        outcome: DurationOutcome,
        elapsed: u64,
        expected: Option<usize>,
    ) {
        assert_eq!(duration_label(&outcome, elapsed), expected);
    }

    #[test_case([0.0, 0.05, 0.5, 0.45], 0.1, Some(MINUTES); "minutes_and_until_stopped_split")]
    #[test_case([0.95, 0.05, 0.0, 0.0], 0.0, Some(AT_ONCE); "at_once")]
    #[test_case([0.4, 0.55, 0.05, 0.0], 0.0, Some(SECONDS); "seconds")]
    #[test_case([0.1, 0.5, 0.4, 0.0], 0.0, None; "undecided_split")]
    #[test_case([0.95, 0.05, 0.0, 0.0], 0.95, Some(UNTIL_STOPPED); "endless_by_noul")]
    #[test_case([0.0, 0.0, 0.05, 0.95], 0.2, Some(UNTIL_STOPPED); "endless_by_level_mass")]
    fn prior_acts_on_cumulative_bounds(
        probabilities: [f64; 4],
        endless: f64,
        expected: Option<usize>,
    ) {
        assert_eq!(
            prior(&response(probabilities, endless), THRESHOLD, THRESHOLD),
            expected
        );
    }

    #[test_case(3, 0, Some(INSTANT_MS), TOOK[AT_ONCE], 3; "completed_at_once")]
    #[test_case(2, 1, Some(MINUTES_MS), TOOK[MINUTES], 2; "completed_in_minutes")]
    #[test_case(0, 4, None, STOPPED, 4; "never_completed")]
    fn earlier_runs_read_as_the_median_level(
        completed: u64,
        stopped: u64,
        p50_ms: Option<u64>,
        took: &str,
        runs: u64,
    ) {
        let family = FamilyRuns {
            family: FAMILY.into(),
            completed,
            stopped,
            p50_ms,
        };
        assert_eq!(
            earlier_run(&family),
            json!({"command": FAMILY, "took": took, "runs": runs})
        );
    }

    #[test]
    fn earlier_runs_fit_the_state_cap() {
        let bare = duration_state(COMMAND, None, []);
        assert_eq!(bare, json!({"command": COMMAND}));
        let runs = FamilyRuns {
            family: format!("{} *", "x".repeat(LONG_FAMILY_BYTES)),
            completed: 1,
            stopped: 0,
            p50_ms: Some(MINUTES_MS),
        };
        let state = duration_state(
            COMMAND,
            Some(WORKDIR),
            iter::repeat_n(earlier_run(&runs), MAX_EARLIER_RUNS),
        );
        assert!(DecisionState::new(&state).is_ok());
        assert_eq!(state[WORKDIR_FIELD], WORKDIR);
        let kept = state[EARLIER_RUNS].as_array().unwrap().len();
        assert!((1..MAX_EARLIER_RUNS).contains(&kept));
    }

    #[test_case(FeatureMode::Off, false, false)]
    #[test_case(FeatureMode::Off, true, false)]
    #[test_case(FeatureMode::Enforce, false, true)]
    #[test_case(FeatureMode::Enforce, true, true)]
    fn disabled_and_remote_do_no_io(mode: FeatureMode, yolo: bool, remote: bool) {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("absent-state");
            let decisions = service(&StateDir::from_path(path.clone()), mode);
            let permissions = Arc::new(PermissionManager::new_nonpersistent(
                PermissionsConfig {
                    yolo,
                    ..Default::default()
                },
                temp.path().to_owned(),
                Arc::default(),
            ));
            let mut ctx = stub_ctx_with_permissions(&AgentMode::Build, permissions);
            ctx.host_cwd = remote.then(|| temp.path().to_owned());
            assert!(
                decisions
                    .shell_duration(&json!({"command":COMMAND}), &ctx)
                    .await
                    .is_none()
            );
            assert!(!path.exists());
        });
    }

    #[test_case(false; "ask")]
    #[test_case(true; "yolo")]
    fn endpoint_free_history_precedes_prior_and_separates_workdirs(yolo: bool) {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let dir = StateDir::from_path(temp.path().join("state"));
            let decisions = service(&dir, FeatureMode::Enforce);
            let permissions = Arc::new(PermissionManager::new_nonpersistent(
                PermissionsConfig {
                    yolo,
                    ..Default::default()
                },
                temp.path().to_owned(),
                Arc::default(),
            ));
            let ctx = stub_ctx_with_permissions(&AgentMode::Build, permissions);
            let input = json!({"command":COMMAND});
            let first = decisions.shell_duration(&input, &ctx).await.unwrap();
            assert!(first.estimate.is_none());
            for _ in 0..3 {
                first.clone().record(DurationOutcome::Ok, MINUTES_MS).await;
            }
            let exact = decisions.shell_duration(&input, &ctx).await.unwrap();
            assert_eq!(exact.estimate.as_ref().unwrap().source, "exact history");
            assert_eq!(exact.estimate.as_ref().unwrap().samples, 3);
            for _ in 0..2 {
                first.clone().record(DurationOutcome::Ok, MINUTES_MS).await;
            }
            let family = decisions
                .shell_duration(&json!({"command":"cargo test --workspace"}), &ctx)
                .await
                .unwrap();
            assert_eq!(family.estimate.unwrap().source, "command family history");
            let other = decisions
                .shell_duration(&json!({"command":COMMAND,"workdir":"other"}), &ctx)
                .await
                .unwrap();
            assert!(other.estimate.is_none());
        });
    }

    #[test]
    fn execution_failures_are_not_successful_duration_samples() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let dir = StateDir::from_path(temp.path().join("state"));
            let plan = plan(&dir, FeatureMode::Enforce, MINUTES_MS);
            let key = plan.key.clone();
            for _ in 0..3 {
                plan.clone()
                    .begin()
                    .finish(&ToolExecResult::from(Err(EXECUTION_ERROR.into())), false)
                    .await;
            }
            assert!(
                ShellDurations::open(&dir)
                    .unwrap()
                    .estimate(&key)
                    .unwrap()
                    .is_none()
            );
            assert_ne!(
                key.digest,
                history_key(WORKSPACE, ".", "cargo test --workspace").digest
            );
            assert_eq!(key.family, "cargo test *");
        });
    }

    #[test_case(DecisionError::Unreachable)]
    #[test_case(DecisionError::Timeout)]
    #[test_case(DecisionError::Refused)]
    fn failed_engine_preserves_timeout_and_delivery(error: DecisionError) {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let dir = StateDir::from_path(temp.path().join("state"));
            let mut config = DecisionsConfig {
                base_url: Some(BASE_URL.parse().unwrap()),
                ..Default::default()
            };
            config.features.shell_duration = FeatureMode::Enforce;
            let decisions = Decisions::with_engine(config, &dir, FailedEngine(error)).unwrap();
            let permissions = Arc::new(PermissionManager::new_nonpersistent(
                PermissionsConfig::default(),
                temp.path().to_owned(),
                Arc::default(),
            ));
            let ctx = stub_ctx_with_permissions(&AgentMode::Build, permissions);
            let input = json!({"command":COMMAND});
            let mut plan = decisions.shell_duration(&input, &ctx).await.unwrap();
            assert!(plan.estimate.is_none());
            assert!(plan.expected_secs().is_none());
            assert!(plan.inject_timeout(&input, &json!({"properties":{"timeoutSec":{"default":DEFAULT_SECS,"maximum":CAP_SECS}}})).is_none());
        });
    }

    #[test]
    fn history_cache_is_bounded_and_rejects_stale_reads() {
        let temp = tempfile::tempdir().unwrap();
        let dir = StateDir::from_path(temp.path().join("state"));
        let estimate = plan(&dir, FeatureMode::Enforce, MINUTES_MS)
            .estimate
            .unwrap();
        let mut cache = ShellDurationCache::default();
        let key = history_key(WORKSPACE, ".", COMMAND);
        cache.insert(key.clone(), estimate.clone(), 0);
        cache.invalidate(&key);
        cache.insert(key.clone(), estimate.clone(), 0);
        assert!(cache.get(&key).is_none());
        for index in 0..=HISTORY_CACHE_ENTRIES {
            cache.insert(
                history_key(WORKSPACE, ".", &format!("cargo test -p package{index}")),
                estimate.clone(),
                cache.revision,
            );
        }
        assert_eq!(cache.entries.len(), HISTORY_CACHE_ENTRIES);
        cache.invalidate(&key);
        assert!(cache.entries.is_empty());
    }
}
