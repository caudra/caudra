use caudra_config::decisions::FeatureMode;
use caudra_config::resolve_shell_background;
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
use std::{
    collections::VecDeque,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use super::{
    DecisionFeature, DecisionReceipt, Decisions, NoteAnchor, NoteDraft, NoteOutcome, counted,
    push_fitting, questions,
};
use crate::{
    ToolDoneEvent, ToolOutput,
    permissions::{PassiveDecisionRevision, canonical_json_sha256, command_pattern},
    tools::{ToolContext, ToolExecResult, humanize_duration},
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
const ETA_LABEL: &str = "ETA ";
const ETA_RANGE: &str = "–";
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
const DURATION_FALLBACK: &str = "ran the call without an estimate";
const EXACT_HISTORY: &str = "exact history";
const FAMILY_HISTORY: &str = "command family history";
const EFFECT_JOINER: &str = " and ";
const REASON_SEPARATOR: &str = ": ";
const SHORT_TIMEOUT_TAIL: &str = " timeout may be too short";
const TAKES: &str = "it should take ";
const EXITS_AT_ONCE: &str = "it should exit at once";
const WITHOUT_ESTIMATE: &str = "Without the estimate, its ";
const OWN_TIMEOUT_WOULD: &str = " timeout would have ";
const MEASURED_OVER: &str = "Measured over ";
const ENGINE_SOURCE: &str = "Estimated by the decision engine from the command alone.";
const RUN: &str = "earlier run";
const RUNS: &str = "earlier runs";
const THIS_COMMAND: &str = "of this command";
const SIMILAR_COMMANDS: &str = "of similar commands";

/// What a duration estimate changed about a shell call. Its note's summary
/// opens with the words of each, so a reader of the note alone can tell which
/// one it was; each opening reads the same after "would have", for shadow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DurationEffect {
    ShortTimeout,
    Background,
    Foreground,
    Timeout,
}

impl DurationEffect {
    /// Most telling first: a warning, then a move, then a timeout.
    const ALL: [Self; 4] = [
        Self::ShortTimeout,
        Self::Background,
        Self::Foreground,
        Self::Timeout,
    ];

    const fn opening(self) -> &'static str {
        match self {
            Self::ShortTimeout => "warned that its ",
            Self::Background => "sent it to the background",
            Self::Foreground => "kept it in the foreground",
            Self::Timeout => "set the timeout to ",
        }
    }

    /// What a card's chip says about the effect, or would have done in
    /// shadow.
    pub const fn chip(self, shadow: bool) -> &'static str {
        match (self, shadow) {
            (Self::ShortTimeout, false) => "timeout may be short",
            (Self::ShortTimeout, true) => "would warn",
            (Self::Background, false) => "sent to background",
            (Self::Background, true) => "would background",
            (Self::Foreground, false) => "kept in foreground",
            (Self::Foreground, true) => "would keep in foreground",
            (Self::Timeout, false) => "timeout set",
            (Self::Timeout, true) => "would set timeout",
        }
    }

    /// Whether the effect warns the reader rather than changing the call.
    pub const fn warns(self) -> bool {
        matches!(self, Self::ShortTimeout)
    }

    /// The most telling effect a note's `summary` names. A note written
    /// before effects had their own words names none.
    pub fn named_in(summary: &str) -> Option<Self> {
        let summary = summary
            .strip_prefix(NoteOutcome::Shadow.lead())
            .unwrap_or(summary);
        let (changes, _) = summary.split_once(REASON_SEPARATOR)?;
        let changes: Vec<&str> = changes.split(EFFECT_JOINER).collect();
        Self::ALL.into_iter().find(|effect| {
            changes
                .iter()
                .any(|change| change.starts_with(effect.opening()))
        })
    }
}

fn humanize_secs(secs: u64) -> String {
    humanize_duration(Duration::from_secs(secs))
}

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

    /// The wait the estimate predicts, in the units of a timeout. A command
    /// expected to exit at once has no wait to show.
    fn eta(&self) -> Option<String> {
        self.span().map(|span| format!("{ETA_LABEL}{span}"))
    }

    /// The likely wait, from the median to the p90, or one value when they
    /// read the same.
    fn span(&self) -> Option<String> {
        if self.p90_ms <= INSTANT_MS {
            return None;
        }
        let [p50, p90] =
            [self.p50_ms, self.p90_ms].map(|ms| humanize_secs(ms.div_ceil(MILLIS_PER_SECOND)));
        Some(if p50 == p90 {
            p90
        } else {
            format!("{p50}{ETA_RANGE}{p90}")
        })
    }

    /// How long the call should take, as a note summary ends.
    fn expectation(&self) -> String {
        match self.span() {
            Some(span) => format!("{TAKES}{span}"),
            None => EXITS_AT_ONCE.to_owned(),
        }
    }

    /// Where the estimate came from, as a reader would put it.
    fn source_line(&self) -> String {
        let whose = match self.source {
            EXACT_HISTORY => THIS_COMMAND,
            FAMILY_HISTORY => SIMILAR_COMMANDS,
            _ => return ENGINE_SOURCE.to_owned(),
        };
        let runs = usize::try_from(self.samples).unwrap_or(usize::MAX);
        format!("{MEASURED_OVER}{} {whose}.", counted(runs, RUN, RUNS))
    }

    /// Where the estimate came from, with a sample count only for history.
    fn provenance(&self) -> String {
        match self.samples {
            0 => self.source.to_owned(),
            samples => format!("{}, {samples} samples", self.source),
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
    /// What went wrong when the engine newly failed to estimate this call.
    failure: Option<String>,
}

/// What a shell call's row is built from, once its delivery is decided.
pub(crate) struct ShellCall<'a> {
    /// The tool use id whose card the row follows.
    pub(crate) id: &'a str,
    /// The input the call runs with.
    pub(crate) input: &'a Value,
    pub(crate) schema: &'a Value,
    pub(crate) timeout_secs: u64,
    pub(crate) background_supported: bool,
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
            failure: None,
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
                        DurationSource::Exact => EXACT_HISTORY,
                        DurationSource::Family => FAMILY_HISTORY,
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
                plan.failure = outcome.fresh_failure();
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

/// The default and the cap a shell schema gives its timeout, when the cap
/// leaves room for the default.
fn timeout_bounds(schema: &Value) -> Option<(u64, u64)> {
    let default = schema.pointer("/properties/timeoutSec/default")?.as_u64()?;
    let cap = schema.pointer("/properties/timeoutSec/maximum")?.as_u64()?;
    (default <= cap).then_some((default, cap))
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
        if *self.decisions.mode(&DecisionFeature::ShellDuration) != FeatureMode::Enforce {
            return None;
        }
        let seconds = self.estimated_timeout(input, schema)?;
        let mut effective = input.clone();
        effective
            .as_object_mut()?
            .insert(TIMEOUT_FIELD.into(), json!(seconds));
        self.injected_timeout = Some(seconds);
        Some(effective)
    }

    /// The timeout the estimate fills in for a call that names none.
    fn estimated_timeout(&self, input: &Value, schema: &Value) -> Option<u64> {
        if input.get(TIMEOUT_FIELD).is_some() || self.endless {
            return None;
        }
        let estimate = self
            .estimate
            .as_ref()
            .filter(|estimate| estimate.extend_timeout)?;
        let (default, cap) = timeout_bounds(schema)?;
        Some(
            estimate
                .p90_ms
                .saturating_mul(3)
                .div_ceil(2 * MILLIS_PER_SECOND)
                .clamp(default, cap),
        )
    }

    pub(crate) fn expected_secs(&self) -> Option<u64> {
        (*self.decisions.mode(&DecisionFeature::ShellDuration) == FeatureMode::Enforce)
            .then(|| self.estimated_secs())
            .flatten()
    }

    fn estimated_secs(&self) -> Option<u64> {
        self.estimate
            .as_ref()
            .map(|estimate| estimate.p90_ms.div_ceil(MILLIS_PER_SECOND))
    }

    /// The call's own timeout, when it is below the estimated p90.
    fn short_timeout(&self) -> Option<(u64, &Estimate)> {
        let estimate = self.estimate.as_ref()?;
        self.requested_timeout
            .filter(|timeout| timeout.saturating_mul(MILLIS_PER_SECOND) < estimate.p90_ms)
            .map(|timeout| (timeout, estimate))
    }

    /// Reports what the estimate changed about `call`: the timeout it filled
    /// in, a delivery that differs from the one the call alone gets, and the
    /// short-timeout warning advice adds. Shadow reports what enforce would
    /// have done. A first engine failure in advise or enforce reports that
    /// instead.
    pub(crate) fn report(&self, ctx: &ToolContext, call: &ShellCall) {
        if ctx.cancel.is_cancelled() {
            return;
        }
        let feature = DecisionFeature::ShellDuration;
        let mode = self.decisions.mode(&feature);
        let emit = |draft: NoteDraft| {
            ctx.event_tx
                .try_send(draft.anchored(NoteAnchor::Tool(call.id.to_owned())));
        };
        let outcome = match mode {
            FeatureMode::Advise | FeatureMode::Enforce => NoteOutcome::Acted,
            FeatureMode::Shadow => NoteOutcome::Shadow,
            FeatureMode::Off => return,
        };
        if let Some(failure) = &self.failure {
            if outcome == NoteOutcome::Acted {
                emit(NoteDraft::fell_back(
                    &self.decisions,
                    feature,
                    failure,
                    DURATION_FALLBACK,
                ));
            }
            return;
        }
        let timeout = match mode {
            FeatureMode::Enforce => self.injected_timeout,
            FeatureMode::Shadow => self.estimated_timeout(call.input, call.schema),
            FeatureMode::Advise | FeatureMode::Off => None,
        };
        let Some(estimate) = &self.estimate else {
            return;
        };
        let mut effects = Vec::new();
        let mut details = Vec::new();
        if let Some(timeout) = timeout {
            effects.push(format!(
                "{}{}",
                DurationEffect::Timeout.opening(),
                humanize_secs(timeout)
            ));
        }
        if *mode != FeatureMode::Advise
            && let Some((background, own_timeout)) = self.moved_delivery(ctx, call, timeout)
        {
            let (effect, otherwise) = if background {
                (DurationEffect::Background, DurationEffect::Foreground)
            } else {
                (DurationEffect::Foreground, DurationEffect::Background)
            };
            effects.push(effect.opening().to_owned());
            details.push(format!(
                "{WITHOUT_ESTIMATE}{}{OWN_TIMEOUT_WOULD}{}.",
                humanize_secs(own_timeout),
                otherwise.opening()
            ));
        }
        if let Some((requested, _)) = self.short_timeout() {
            effects.push(format!(
                "{}{}{SHORT_TIMEOUT_TAIL}",
                DurationEffect::ShortTimeout.opening(),
                humanize_secs(requested)
            ));
        }
        if effects.is_empty() {
            return;
        }
        let summary = format!(
            "{}{REASON_SEPARATOR}{}",
            effects.join(EFFECT_JOINER),
            estimate.expectation()
        );
        details.push(estimate.source_line());
        emit(NoteDraft::new(
            &self.decisions,
            feature,
            outcome,
            &summary,
            details,
        ));
    }

    /// The delivery the estimate chooses, whether background, when it differs
    /// from the one the call gets from its own timeout, with that timeout.
    /// `timeout` is the one the estimate filled in, if any.
    fn moved_delivery(
        &self,
        ctx: &ToolContext,
        call: &ShellCall,
        timeout: Option<u64>,
    ) -> Option<(bool, u64)> {
        let own_timeout = match timeout {
            Some(_) => timeout_bounds(call.schema)?.0,
            None => call.timeout_secs,
        };
        let deliver = |timeout, expected| {
            resolve_shell_background(&ctx.config, call.background_supported, timeout, expected).ok()
        };
        let own = deliver(own_timeout, None)?;
        let estimated = deliver(timeout.unwrap_or(own_timeout), self.estimated_secs())?;
        (own != estimated).then_some((estimated, own_timeout))
    }

    pub(crate) fn annotation(&self) -> Option<String> {
        if !self.visible() {
            return None;
        }
        self.estimate.as_ref().and_then(Estimate::eta)
    }

    pub(crate) fn advise(&self, done: &mut ToolDoneEvent, ctx: &ToolContext) {
        if !self.visible() || !self.is_current(ctx) {
            return;
        }
        let mut notes = Vec::new();
        if self.endless {
            notes.push(ENDLESS_WARNING.to_owned());
        }
        if let Some((_, estimate)) = self.short_timeout() {
            notes.push(format!(
                "The explicit timeout is below estimated p90 {}s ({}); it was not changed.",
                estimate.p90_ms.div_ceil(MILLIS_PER_SECOND),
                estimate.source
            ));
        }
        if let Some(estimate) = &self.estimate
            && let Some(timeout) = self.injected_timeout
        {
            notes.push(format!(
                "Omitted timeout set to {timeout}s from p90 {}s ({}).",
                estimate.p90_ms.div_ceil(MILLIS_PER_SECOND),
                estimate.provenance()
            ));
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
        AT_ONCE, DURATION_FALLBACK, DURATION_QUESTION, DurationEffect, EARLIER_RUNS,
        ENDLESS_QUESTION, ENGINE_PRIOR, ENGINE_SOURCE, Estimate, FAMILY_HISTORY,
        HISTORY_CACHE_ENTRIES, INSTANT_MS, LEVELS, MAX_EARLIER_RUNS, MINUTES, MINUTES_MS, SECONDS,
        STOPPED, ShellCall, ShellDurationCache, ShellDurationPlan, TOOK, UNTIL_STOPPED,
        WORKDIR_FIELD, duration_label, duration_state, earlier_run, history_key, prior,
    };
    use crate::{
        AgentEvent, AgentMode, CancelToken, EventSender,
        decisions::{
            DecisionFeature, DecisionNote, DecisionState, Decisions, NoteAnchor, NoteOutcome,
        },
        permissions::PermissionManager,
        tools::{
            ToolExecResult,
            test_support::{stub_ctx_with, stub_ctx_with_permissions},
        },
    };
    use async_trait::async_trait;
    use caudra_config::decisions::{DecisionEndpoint, DecisionProtocol};
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
    const CALL_ID: &str = "call-duration";
    const ENGINE_FAILURE: &str = "the decision engine timed out";
    const LONG_P90_MS: u64 = 200_001;
    const SHORT_TIMEOUT_SECS: u64 = 5;
    const ONE_NOTE: &str = "expected exactly one decision note";
    const EXACT_HISTORY: &str = "exact history";
    const HISTORY_SAMPLES: u64 = 3;
    const EXACT_PROVENANCE: &str = "exact history, 3 samples";
    const SECONDS_ETA: &str = "ETA 10s–2m";
    const MINUTES_ETA: &str = "ETA 5m–10m";
    const EQUAL_ENDS_ETA: &str = "ETA 8s";
    const SUB_SECOND_P50_ETA: &str = "ETA 1s–2s";
    const LONG_TIMEOUT_SECS: u64 = 900;
    const JARGON: [&str; 3] = ["p50", "p90", ENGINE_PRIOR];
    const FOREGROUND_SUMMARY: &str = "kept it in the foreground: it should take 10s–2m";
    const SHADOW_FOREGROUND_SUMMARY: &str =
        "would have kept it in the foreground: it should take 10s–2m";
    const FOREGROUND_REASON: &str =
        "Without the estimate, its 15m timeout would have sent it to the background.";
    const EXACT_SOURCE: &str = "Measured over 3 earlier runs of this command.";
    const FAMILY_SOURCE: &str = "Measured over 1 earlier run of similar commands.";
    const LEGACY_SUMMARY: &str =
        "moved it to the foreground from an estimate of p90 120 s (engine prior)";
    const BOTH_SUMMARY: &str =
        "set the timeout to 5m and sent it to the background: it should take 5m–10m";
    const WARNED_SUMMARY: &str = "kept it in the foreground and warned that its 5s timeout may be too short: it should take 10s–2m";

    fn history(p50_ms: u64, p90_ms: u64) -> Estimate {
        Estimate {
            p50_ms,
            p90_ms,
            samples: HISTORY_SAMPLES,
            source: EXACT_HISTORY,
            extend_timeout: true,
        }
    }

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
            estimate: Some(history(p90_ms, p90_ms)),
            endless: false,
            receipt: None,
            requested_timeout: None,
            injected_timeout: None,
            failure: None,
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

    /// The notes `plan` reports about a call that asked for `requested`.
    fn reported(
        mut plan: ShellDurationPlan,
        requested: Option<u64>,
        background_supported: bool,
        cancelled: bool,
    ) -> Vec<DecisionNote> {
        plan.requested_timeout = requested;
        let schema =
            json!({"properties":{"timeoutSec":{"default":DEFAULT_SECS,"maximum":CAP_SECS}}});
        let input = match requested {
            Some(timeout) => json!({"command": COMMAND, "timeoutSec": timeout}),
            None => json!({"command": COMMAND}),
        };
        let effective = plan.inject_timeout(&input, &schema);
        let (sender, events) = flume::unbounded();
        let event_tx = EventSender::new(sender, 0);
        let mut ctx = stub_ctx_with(&AgentMode::Build, Some(&event_tx), None);
        if cancelled {
            let (trigger, token) = CancelToken::new();
            trigger.cancel();
            ctx.cancel = token;
        }
        plan.report(
            &ctx,
            &ShellCall {
                id: CALL_ID,
                input: effective.as_ref().unwrap_or(&input),
                schema: &schema,
                timeout_secs: requested.unwrap_or(DEFAULT_SECS),
                background_supported,
            },
        );
        events
            .try_iter()
            .filter_map(|envelope| match envelope.event {
                AgentEvent::Decision(note) => Some(*note),
                _ => None,
            })
            .collect()
    }

    #[test_case(FeatureMode::Enforce, None, None, false, Some((NoteOutcome::Acted, Some(DurationEffect::Timeout))); "enforce_sets_the_timeout")]
    #[test_case(FeatureMode::Shadow, None, None, false, Some((NoteOutcome::Shadow, Some(DurationEffect::Timeout))); "shadow_would_set_the_timeout")]
    #[test_case(FeatureMode::Advise, Some(SHORT_TIMEOUT_SECS), None, false, Some((NoteOutcome::Acted, Some(DurationEffect::ShortTimeout))); "advise_warns_of_a_short_timeout")]
    #[test_case(FeatureMode::Advise, None, None, false, None; "an_eta_alone_says_nothing")]
    #[test_case(FeatureMode::Enforce, None, Some(ENGINE_FAILURE), false, Some((NoteOutcome::FellBack, None)); "fresh_failure_falls_back")]
    #[test_case(FeatureMode::Shadow, None, Some(ENGINE_FAILURE), false, None; "shadow_failure_says_nothing")]
    #[test_case(FeatureMode::Enforce, None, None, true, None; "cancelled_says_nothing")]
    fn shell_duration_notes_only_what_it_changed(
        mode: FeatureMode,
        requested: Option<u64>,
        failure: Option<&str>,
        cancelled: bool,
        expected: Option<(NoteOutcome, Option<DurationEffect>)>,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let dir = StateDir::from_path(temp.path().join("state"));
        let mut plan = plan(&dir, mode.clone(), LONG_P90_MS);
        plan.failure = failure.map(String::from);
        let notes = reported(plan, requested, false, cancelled);
        let Some((outcome, effect)) = expected else {
            assert!(notes.is_empty());
            return;
        };
        let [note] = notes.as_slice() else {
            panic!("{ONE_NOTE}");
        };
        assert_eq!(note.feature, DecisionFeature::ShellDuration);
        assert_eq!(note.mode, mode);
        assert_eq!(note.outcome, outcome);
        assert_eq!(note.anchor, NoteAnchor::Tool(CALL_ID.into()));
        assert!(note.summary.starts_with(outcome.lead()));
        assert_eq!(DurationEffect::named_in(&note.summary), effect);
        if outcome == NoteOutcome::FellBack {
            assert!(note.summary.ends_with(DURATION_FALLBACK));
            return;
        }
        for line in iter::once(&note.summary).chain(&note.details) {
            for jargon in JARGON {
                assert!(!line.contains(jargon), "{line}");
            }
        }
    }

    #[test_case(FeatureMode::Enforce, FOREGROUND_SUMMARY; "enforce_keeps_it_in_the_foreground")]
    #[test_case(FeatureMode::Shadow, SHADOW_FOREGROUND_SUMMARY; "shadow_would_have")]
    fn a_foreground_move_says_why_in_plain_words(mode: FeatureMode, summary: &str) {
        let temp = tempfile::tempdir().unwrap();
        let dir = StateDir::from_path(temp.path().join("state"));
        let mut plan = plan(&dir, mode, LONG_P90_MS);
        plan.estimate = Some(LEVELS[SECONDS].clone());
        let notes = reported(plan, Some(LONG_TIMEOUT_SECS), true, false);
        let [note] = notes.as_slice() else {
            panic!("{ONE_NOTE}");
        };
        assert_eq!(note.summary, summary);
        assert_eq!(note.details, [FOREGROUND_REASON, ENGINE_SOURCE]);
        assert_eq!(
            DurationEffect::named_in(&note.summary),
            Some(DurationEffect::Foreground)
        );
    }

    #[test_case(LEVELS[SECONDS].clone(), ENGINE_SOURCE; "an_engine_estimate")]
    #[test_case(history(MINUTES_MS, MINUTES_MS), EXACT_SOURCE; "this_commands_runs")]
    #[test_case(Estimate { source: FAMILY_HISTORY, samples: 1, ..history(MINUTES_MS, MINUTES_MS) }, FAMILY_SOURCE; "one_similar_run")]
    fn the_source_line_names_where_the_estimate_came_from(estimate: Estimate, expected: &str) {
        assert_eq!(estimate.source_line(), expected);
    }

    #[test_case(LEGACY_SUMMARY, None; "a_note_from_before_effects_had_words")]
    #[test_case(BOTH_SUMMARY, Some(DurationEffect::Background); "a_move_outranks_a_timeout")]
    #[test_case(WARNED_SUMMARY, Some(DurationEffect::ShortTimeout); "a_warning_outranks_everything")]
    #[test_case(SHADOW_FOREGROUND_SUMMARY, Some(DurationEffect::Foreground); "shadow_lead_is_skipped")]
    fn named_in_picks_the_most_telling_effect(summary: &str, expected: Option<DurationEffect>) {
        assert_eq!(DurationEffect::named_in(summary), expected);
    }

    #[test_case(LEVELS[AT_ONCE].clone(), None; "at_once_prior")]
    #[test_case(history(300, 900), None; "measured_sub_second")]
    #[test_case(LEVELS[SECONDS].clone(), Some(SECONDS_ETA); "seconds_prior")]
    #[test_case(LEVELS[MINUTES].clone(), Some(MINUTES_ETA); "minutes_prior")]
    #[test_case(history(7_200, 7_900), Some(EQUAL_ENDS_ETA); "equal_ends_collapse")]
    #[test_case(history(300, 1_500), Some(SUB_SECOND_P50_ETA); "sub_second_p50")]
    fn eta_shows_only_a_wait(estimate: Estimate, expected: Option<&str>) {
        assert_eq!(estimate.eta().as_deref(), expected);
    }

    #[test_case(LEVELS[SECONDS].clone(), ENGINE_PRIOR; "an_engine_prior")]
    #[test_case(history(MINUTES_MS, MINUTES_MS), EXACT_PROVENANCE; "measured_history")]
    fn provenance_counts_only_measured_samples(estimate: Estimate, expected: &str) {
        assert_eq!(estimate.provenance(), expected);
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
                endpoint: Some(DecisionEndpoint::new(
                    DecisionProtocol::TypeSafe.as_str(),
                    DecisionProtocol::TypeSafe,
                    BASE_URL.parse().unwrap(),
                )),
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
