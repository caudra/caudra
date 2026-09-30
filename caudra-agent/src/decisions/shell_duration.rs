use caudra_config::decisions::FeatureMode;
use caudra_decision::{Answer, DecisionResponse, Question, QuestionSet, QuestionType};
use caudra_storage::{
    decision_log::DecisionLabel,
    now_epoch,
    shell_durations::{
        CommandDigest, DurationOutcome, DurationSource, ShellDurationKey, ShellDurations,
    },
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
    thread,
    time::Instant,
};

use super::{DecisionContext, DecisionFeature, DecisionReceipt, Decisions};
use crate::{
    ToolDoneEvent, ToolOutput,
    permissions::{canonical_json_sha256, command_pattern},
    tools::{ToolContext, ToolExecResult},
};

const QUESTION_SET: &str = "shell_duration.v2";
const INSTANT: &str = "instant";
const SHORT: &str = "short";
const LONG: &str = "long";
const ENDLESS: &str = "endless";
const TIMEOUT_FIELD: &str = "timeoutSec";
const HISTORY_CACHE_ENTRIES: usize = 256;
const MILLIS_PER_SECOND: u64 = 1_000;
const INSTANT_MS: u64 = MILLIS_PER_SECOND;
const SHORT_P50_MS: u64 = 10 * MILLIS_PER_SECOND;
const LONG_P50_MS: u64 = 5 * 60 * MILLIS_PER_SECOND;
const LONG_P90_MS: u64 = 10 * 60 * MILLIS_PER_SECOND;
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
    revision: u64,
    key: ShellDurationKey,
    estimate: Option<Estimate>,
    endless: bool,
    receipt: Option<DecisionReceipt>,
    threshold_ms: u64,
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
            || ctx.permissions.is_yolo()
            || ctx.cancel.is_cancelled()
            || ctx.host_cwd.is_some()
            || ctx.workspace_session.is_some()
            || ctx.remote_project_context.is_some()
        {
            return None;
        }
        let command = input.get("command")?.as_str()?;
        let root = ctx.permissions.project_cwd();
        let workdir = input.get("workdir").and_then(Value::as_str).unwrap_or(".");
        let key = history_key(&root.display().to_string(), workdir, command);
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
            threshold_ms: ctx
                .config
                .shell_async_threshold_secs
                .saturating_mul(MILLIS_PER_SECOND),
            requested_timeout: input.get(TIMEOUT_FIELD).and_then(Value::as_u64),
            injected_timeout: None,
        };
        if plan.estimate.is_some() {
            return Some(plan);
        }
        let history_key = plan.key.clone();
        let dir = self.0.state_dir.clone();
        let history =
            smol::unblock(move || ShellDurations::open(&dir)?.estimate(&history_key)).await;
        match history {
            Ok(Some(estimate)) => {
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
            Err(error) => tracing::warn!(%error, "shell duration history unavailable"),
            Ok(None) => {}
        }
        if let Some(questions) = duration_questions(plan.threshold_ms) {
            let context = DecisionContext {
                project: Some(root.display().to_string()),
                session: ctx.session_id.as_ref().map(ToString::to_string),
                ..Default::default()
            };
            if let Some(outcome) = self
                .evaluate(
                    DecisionFeature::ShellDuration,
                    &json!({"command": command, "workdir": workdir}),
                    &questions,
                    &context,
                )
                .await
            {
                plan.receipt = outcome.receipt;
                if let Ok(response) = outcome.result {
                    (plan.estimate, plan.endless) = prior(
                        &response,
                        self.config().thresholds.shell_heavy,
                        self.config().thresholds.shell_endless,
                        plan.threshold_ms,
                    );
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

/// The duration buckets follow `duration_label`, so a measured label always
/// names an option the engine was offered.
fn duration_questions(threshold_ms: u64) -> Option<QuestionSet> {
    let instant_secs = INSTANT_MS / MILLIS_PER_SECOND;
    let threshold_secs = threshold_ms.div_ceil(MILLIS_PER_SECOND);
    let mut criteria = BTreeMap::from([
        (
            INSTANT,
            format!("Finishes on its own within {instant_secs} second."),
        ),
        (
            ENDLESS,
            "Keeps running until stopped, such as a server, watcher, or interactive session."
                .to_owned(),
        ),
    ]);
    if threshold_ms > INSTANT_MS {
        criteria.insert(
            SHORT,
            format!(
                "Finishes on its own after more than {instant_secs} second and in under {threshold_secs} seconds."
            ),
        );
        criteria.insert(
            LONG,
            format!("Finishes on its own, but only after {threshold_secs} seconds or more."),
        );
    } else {
        criteria.insert(
            LONG,
            format!("Finishes on its own, but only after more than {instant_secs} second."),
        );
    }
    let noul = |instructions: &str| Question {
        kind: QuestionType::Noul,
        instructions: json!(instructions),
        criteria: None,
    };
    QuestionSet::new(
        QUESTION_SET,
        BTreeMap::from([
            (
                "endless".into(),
                noul("Does this command normally keep running until stopped?"),
            ),
            (
                "heavy".into(),
                noul(
                    "Is this command likely to require substantial computation or lengthy network transfers?",
                ),
            ),
            (
                "duration".into(),
                Question {
                    kind: QuestionType::Choice,
                    instructions: json!("Estimate this command's execution duration."),
                    criteria: Some(json!(criteria)),
                },
            ),
        ]),
    )
    .ok()
}

fn prior(
    response: &DecisionResponse,
    heavy: f64,
    endless: f64,
    threshold_ms: u64,
) -> (Option<Estimate>, bool) {
    if matches!(response.answers.get("endless"), Some(Answer::Noul(answer)) if answer.noul >= endless)
    {
        return (None, true);
    }
    let choice = match response.answers.get("duration") {
        Some(Answer::Choice(answer)) if answer.confidence >= heavy => Some(answer.choice.as_str()),
        _ => None,
    };
    if choice == Some(ENDLESS) {
        return (None, true);
    }
    let is_heavy =
        matches!(response.answers.get("heavy"), Some(Answer::Noul(answer)) if answer.noul >= heavy);
    let (p50_ms, p90_ms, extend_timeout) = match choice {
        Some(LONG) => (LONG_P50_MS, LONG_P90_MS, true),
        Some(INSTANT) => (INSTANT_MS, INSTANT_MS, false),
        Some(SHORT) => (SHORT_P50_MS.min(threshold_ms), threshold_ms, false),
        _ if is_heavy => (LONG_P50_MS, LONG_P90_MS, true),
        _ => return (None, false),
    };
    (
        Some(Estimate {
            p50_ms,
            p90_ms,
            samples: 0,
            source: "engine prior",
            extend_timeout,
        }),
        false,
    )
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
        let label = duration_label(&outcome, elapsed_ms, self.threshold_ms);
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
                        expected: json!({"duration": duration}),
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

fn duration_label(
    outcome: &DurationOutcome,
    elapsed_ms: u64,
    threshold_ms: u64,
) -> Option<&'static str> {
    match outcome {
        DurationOutcome::Ok if elapsed_ms <= INSTANT_MS => Some(INSTANT),
        DurationOutcome::Ok if elapsed_ms < threshold_ms => Some(SHORT),
        DurationOutcome::Ok => Some(LONG),
        _ if elapsed_ms >= threshold_ms => Some(LONG),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ENDLESS, Estimate, HISTORY_CACHE_ENTRIES, INSTANT, INSTANT_MS, LONG, LONG_P90_MS, SHORT,
        ShellDurationCache, ShellDurationPlan, duration_label, duration_questions, history_key,
        prior,
    };
    use crate::{
        AgentMode,
        decisions::Decisions,
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
        shell_durations::{DurationOutcome, ShellDurations},
    };
    use serde_json::{Value, json};
    use std::{collections::BTreeSet, sync::Arc, time::Instant};
    use test_case::test_case;

    const COMMAND: &str = "cargo test -p private-package";
    const WORKSPACE: &str = "/project";
    const THRESHOLD_MS: u64 = 120_000;
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
            revision: 0,
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
            threshold_ms: THRESHOLD_MS,
            requested_timeout: None,
            injected_timeout: None,
        }
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

    #[test_case(DurationOutcome::Ok, 1_000, Some("instant"))]
    #[test_case(DurationOutcome::Ok, 2_000, Some("short"))]
    #[test_case(DurationOutcome::Ok, THRESHOLD_MS, Some("long"))]
    #[test_case(DurationOutcome::Timeout, 2_000, None)]
    #[test_case(DurationOutcome::Cancelled, 2_000, None)]
    #[test_case(DurationOutcome::Timeout, THRESHOLD_MS, Some("long"))]
    #[test_case(DurationOutcome::Cancelled, THRESHOLD_MS, Some("long"))]
    fn measured_labels_are_censored(
        outcome: DurationOutcome,
        elapsed: u64,
        expected: Option<&str>,
    ) {
        assert_eq!(duration_label(&outcome, elapsed, THRESHOLD_MS), expected);
    }

    #[test_case(LONG, 0.99, 0.01, 0.01, Some(LONG_P90_MS), false)]
    #[test_case(SHORT, 0.99, 0.01, 0.01, Some(THRESHOLD_MS), false)]
    #[test_case(INSTANT, 0.01, 0.99, 0.01, Some(LONG_P90_MS), false)]
    #[test_case(LONG, 0.01, 0.01, 0.01, None, false)]
    #[test_case(LONG, 0.99, 0.99, 0.99, None, true)]
    #[test_case(ENDLESS, 0.99, 0.99, 0.01, None, true)]
    fn priors_require_confidence_and_never_extend_endless(
        choice: &str,
        confidence: f64,
        heavy: f64,
        endless: f64,
        expected: Option<u64>,
        is_endless: bool,
    ) {
        let response: DecisionResponse = serde_json::from_value(json!({
            "model": MODEL,
            "answers": {
                "duration": {"type":"choice","choice":choice,"confidence":confidence,"probabilities":{INSTANT:0.1,SHORT:0.1,LONG:0.7,ENDLESS:0.1}},
                "heavy": {"type":"noul","noul":heavy},
                "endless": {"type":"noul","noul":endless}
            },
            "usage":{"input_tokens":0,"output_tokens":0}
        })).unwrap();
        let (estimate, endless) = prior(&response, 0.9, 0.9, THRESHOLD_MS);
        assert_eq!(estimate.map(|estimate| estimate.p90_ms), expected);
        assert_eq!(endless, is_endless);
    }

    #[test_case(THRESHOLD_MS, &[ENDLESS, INSTANT, LONG, SHORT]; "default_threshold")]
    #[test_case(INSTANT_MS, &[ENDLESS, INSTANT, LONG]; "threshold_within_instant")]
    fn duration_options_match_measured_labels(threshold_ms: u64, expected: &[&str]) {
        let questions = duration_questions(threshold_ms).unwrap();
        let options: BTreeSet<_> = questions.questions()["duration"]
            .criteria
            .as_ref()
            .and_then(Value::as_object)
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let measured: BTreeSet<_> = [INSTANT_MS, INSTANT_MS + 1, threshold_ms - 1, threshold_ms]
            .into_iter()
            .filter_map(|elapsed| duration_label(&DurationOutcome::Ok, elapsed, threshold_ms))
            .chain([ENDLESS])
            .collect();
        assert_eq!(options, measured);
        assert_eq!(options, expected.iter().copied().collect());
    }

    #[test_case(FeatureMode::Off, false, false)]
    #[test_case(FeatureMode::Enforce, true, false)]
    #[test_case(FeatureMode::Enforce, false, true)]
    fn disabled_yolo_and_remote_do_no_io(mode: FeatureMode, yolo: bool, remote: bool) {
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

    #[test]
    fn endpoint_free_history_precedes_prior_and_separates_workdirs() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let dir = StateDir::from_path(temp.path().join("state"));
            let decisions = service(&dir, FeatureMode::Enforce);
            let permissions = Arc::new(PermissionManager::new_nonpersistent(
                PermissionsConfig::default(),
                temp.path().to_owned(),
                Arc::default(),
            ));
            let ctx = stub_ctx_with_permissions(&AgentMode::Build, permissions);
            let input = json!({"command":COMMAND});
            let first = decisions.shell_duration(&input, &ctx).await.unwrap();
            assert!(first.estimate.is_none());
            for _ in 0..3 {
                first
                    .clone()
                    .record(DurationOutcome::Ok, THRESHOLD_MS)
                    .await;
            }
            let exact = decisions.shell_duration(&input, &ctx).await.unwrap();
            assert_eq!(exact.estimate.as_ref().unwrap().source, "exact history");
            assert_eq!(exact.estimate.as_ref().unwrap().samples, 3);
            for _ in 0..2 {
                first
                    .clone()
                    .record(DurationOutcome::Ok, THRESHOLD_MS)
                    .await;
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
            let plan = plan(&dir, FeatureMode::Enforce, THRESHOLD_MS);
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
        let estimate = plan(&dir, FeatureMode::Enforce, THRESHOLD_MS)
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
