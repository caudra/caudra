use std::collections::VecDeque;
use std::future::Future;
use std::mem;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use caudra_config::ModelPolicy;
use caudra_providers::model_registry::Binding;
use caudra_providers::provider::{Provider, from_model_async};
use caudra_providers::{
    AgentError, Billing, ContentBlock, Message, Model, ModelPurpose, RequestOptions, Timeouts,
    TokenUsage,
};
use caudra_storage::goal_log::{
    GoalEnd, GoalEvaluationOutcome, GoalLogWrite, StoredGoalCursor, StoredGoalEvaluation,
    StoredGoalRecord,
};
use caudra_storage::sessions::{
    SessionMeta, StoredActiveGoal, StoredGoalResult, StoredGoalVerdict, StoredGoalWait,
    StoredTokenUsage,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use strum::Display;
use tracing::warn;

use super::goal_progress::{GoalPace, GoalTurn, StallTracker};
use super::judging::{
    self, INITIAL_TRANSCRIPT_PERCENT, ONE_JSON_OBJECT_ERROR, RETRY_TRANSCRIPT_PERCENT,
    TOOL_CALL_LINE, ToolBlock, correction, drop_oldest_rounds, json_object, result_line,
    truncate_output,
};
use super::run::estimate_message_tokens;
use super::streaming::stream_silent_with_retry;
use crate::automation::clock::{Clock, SystemClock};
use crate::cancel::CancelToken;
use crate::{AgentEvent, EventSender};

pub const MAX_GOAL_CHARS: usize = 4_000;
pub const DEFAULT_GOAL_CONTINUATION_LIMIT: u32 = 16;
pub const MAX_GOAL_CONTINUATION_LIMIT: u32 = 100;
/// Goal log writes a handle holds for its frontend. Past it the oldest is
/// dropped, so a handle nobody drains stays bounded.
const MAX_UNLOGGED_GOAL_WRITES: usize = 512;
const EVALUATOR_INITIALIZATION_TIMEOUT: Duration = Duration::from_secs(30);
const EVALUATOR_OUTPUT_TOKENS: u32 = 4_096;
const MAX_EVALUATOR_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_VALIDATION_ATTEMPTS: u32 = 3;
const NO_TEXT_ERROR: &str = "evaluator returned no text";
const OVERSIZE_ERROR: &str = "evaluator response was too large";
const TOOL_CALL_ERROR: &str = "evaluator attempted to call a tool instead of answering";
pub(crate) const GOAL_EVALUATOR_ROLE: JudgeRole = JudgeRole {
    purpose: ModelPurpose::Goal,
    label: "goal evaluator",
};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GoalError {
    #[error("goal condition is empty")]
    Empty,
    #[error("goal condition exceeds {MAX_GOAL_CHARS} characters")]
    TooLong,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum GoalVerdict {
    NotMet,
    Met,
    Impossible,
}

#[derive(Debug, Clone)]
pub struct GoalSnapshot {
    /// The goal's number in the session's goal log, counted from 1.
    pub goal: u32,
    /// Unix milliseconds.
    pub set_at_ms: u64,
    pub condition: Arc<str>,
    pub evaluations: u32,
    pub last_verdict: Option<GoalVerdict>,
    pub last_reason: Option<Arc<str>>,
    pub started_at: Instant,
    /// What the goal had already spent on the clock before this process saw
    /// it. Zero for a goal set here, and the stored total for a resumed one.
    pub elapsed_before: Duration,
    pub usage: TokenUsage,
    pub cost: Option<f64>,
    pub subscription_cost: Option<f64>,
    /// The goal was set while the session was busy, and its first run waits
    /// for the session to settle instead of riding the message queue.
    pub kickoff_pending: bool,
    pub(crate) generation: u64,
}

impl GoalSnapshot {
    pub fn elapsed(&self) -> Duration {
        self.elapsed_before + self.started_at.elapsed()
    }
}

impl From<GoalVerdict> for StoredGoalVerdict {
    fn from(verdict: GoalVerdict) -> Self {
        match verdict {
            GoalVerdict::Met => Self::Met,
            GoalVerdict::NotMet => Self::NotMet,
            GoalVerdict::Impossible => Self::Impossible,
        }
    }
}

impl From<StoredGoalVerdict> for GoalVerdict {
    fn from(verdict: StoredGoalVerdict) -> Self {
        match verdict {
            StoredGoalVerdict::Met => Self::Met,
            StoredGoalVerdict::NotMet => Self::NotMet,
            StoredGoalVerdict::Impossible => Self::Impossible,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct GoalResult {
    /// The goal's number in the session's goal log, `0` for one finished
    /// before goals were numbered.
    pub goal: u32,
    /// Unix milliseconds, `0` when not known.
    pub set_at_ms: u64,
    pub condition: Arc<str>,
    pub verdict: GoalVerdict,
    pub reason: Arc<str>,
    pub evaluations: u32,
    pub duration: Duration,
    pub usage: TokenUsage,
    pub cost: Option<f64>,
    pub subscription_cost: Option<f64>,
}

/// A goal's wait on a change outside the session: its `attempt`th since it
/// last moved, over at `until_ms`, Unix milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GoalWait {
    pub attempt: u32,
    pub until_ms: u64,
}

#[derive(Debug, Clone)]
pub enum GoalStatus {
    Active(GoalSnapshot),
    Finished(GoalResult),
}

/// A goal as `SessionMeta` keeps it: the active goal, the last finished one,
/// the session's continuation limit, and where goal log numbering stands.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredGoal {
    pub active: Option<Box<StoredActiveGoal>>,
    pub result: Option<Box<StoredGoalResult>>,
    pub continuation_limit: Option<u32>,
    pub log: Option<StoredGoalCursor>,
}

/// One check of a goal, as [`GoalHandle::log_evaluation`] files it.
pub(crate) struct EvaluationDraft {
    pub evaluation: u32,
    /// Unix milliseconds.
    pub started_at_ms: u64,
    pub history_len: usize,
    /// The evaluator's own verdict, `None` when none came.
    pub verdict: Option<GoalVerdict>,
    pub reason: String,
    pub outcome: GoalEvaluationOutcome,
    pub model: Option<String>,
    pub usage: Option<StoredTokenUsage>,
    pub cost: Option<f64>,
}

/// Where a logged evaluation landed: its number and when it was filed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoggedEvaluation {
    pub seq: u64,
    /// Unix milliseconds.
    pub at_ms: u64,
}

struct GoalState {
    generation: u64,
    active: Option<GoalSnapshot>,
    finished: Option<GoalResult>,
    continuation_limit: u32,
    /// The newest goal's number, `0` before the first.
    last_goal: u32,
    /// The newest evaluation's number, `0` before the first.
    last_seq: u64,
    /// Goal log writes the frontend has yet to take, oldest first.
    unlogged: VecDeque<GoalLogWrite>,
    stalls: StallTracker,
    /// The active goal's wait, `None` while it is not waiting.
    wait: Option<GoalWait>,
}

impl Default for GoalState {
    fn default() -> Self {
        Self {
            generation: 0,
            active: None,
            finished: None,
            continuation_limit: DEFAULT_GOAL_CONTINUATION_LIMIT,
            last_goal: 0,
            last_seq: 0,
            unlogged: VecDeque::new(),
            stalls: StallTracker::default(),
            wait: None,
        }
    }
}

impl GoalState {
    fn journal(&mut self, write: GoalLogWrite) {
        if self.unlogged.len() >= MAX_UNLOGGED_GOAL_WRITES {
            self.unlogged.pop_front();
            warn!(
                cap = MAX_UNLOGGED_GOAL_WRITES,
                "goal log backlog full, dropping its oldest write"
            );
        }
        self.unlogged.push_back(write);
    }

    /// Takes the active goal, journaling how it ended.
    fn end_active(&mut self, end: GoalEnd, at_ms: u64) -> Option<GoalSnapshot> {
        let active = self.active.take()?;
        self.wait = None;
        self.generation = self.generation.wrapping_add(1);
        self.journal(GoalLogWrite::Ended(ended_record(&active, end, at_ms)));
        Some(active)
    }
}

/// One session's goal. Clones share it. Every transition it makes is
/// journaled for the frontend to drain into the session's goal log, so no
/// caller that sets or clears a goal has to report it.
#[derive(Clone)]
pub struct GoalHandle {
    inner: Arc<Mutex<GoalState>>,
    clock: Arc<dyn Clock>,
}

impl Default for GoalHandle {
    fn default() -> Self {
        Self::new(Arc::new(SystemClock::new()))
    }
}

pub(crate) enum GoalApply {
    Stale,
    Continue { evaluation: u32 },
    Terminal,
}

impl GoalHandle {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            inner: Arc::default(),
            clock,
        }
    }

    /// Reopens everything a session stored of its goal: the active goal, the
    /// result of the last finished one, the continuation limit, and the goal
    /// log's numbering.
    pub fn from_meta(meta: &SessionMeta) -> Self {
        Self::from_meta_with_clock(meta, Arc::new(SystemClock::new()))
    }

    /// [`Self::from_meta`] on `clock`. An active goal resumes with the spend,
    /// evaluation count and clock it had. One stored before goals were
    /// numbered takes the next number and journals its start, dated back by
    /// the time it had run. A condition that no longer validates is dropped
    /// rather than resumed: the panel would have nothing to show and the
    /// evaluator nothing to check.
    pub fn from_meta_with_clock(meta: &SessionMeta, clock: Arc<dyn Clock>) -> Self {
        let handle = Self::new(clock);
        let now_ms = handle.now_ms();
        let mut state = handle.lock();
        if let Some(cursor) = &meta.goal_log {
            state.last_goal = cursor.last_goal;
            state.last_seq = cursor.last_seq;
        }
        if let Some(limit) = meta.goal_continuation_limit {
            state.continuation_limit = limit.min(MAX_GOAL_CONTINUATION_LIMIT);
        }
        if let Some(stored) = meta.active_goal.as_deref()
            && let Ok(condition) = Goal::validate(&stored.condition)
        {
            let legacy = stored.goal == 0;
            state.last_goal = if legacy {
                state.last_goal.saturating_add(1)
            } else {
                state.last_goal.max(stored.goal)
            };
            state.generation = state.generation.wrapping_add(1);
            let snapshot = GoalSnapshot {
                goal: if legacy { state.last_goal } else { stored.goal },
                set_at_ms: if stored.set_at_ms == 0 {
                    now_ms.saturating_sub(stored.elapsed_ms)
                } else {
                    stored.set_at_ms
                },
                condition,
                evaluations: stored.evaluations,
                last_verdict: stored.last_verdict.map(Into::into),
                last_reason: stored.last_reason.as_deref().map(Arc::from),
                started_at: Instant::now(),
                elapsed_before: Duration::from_millis(stored.elapsed_ms),
                usage: stored.usage.into(),
                cost: stored.usage.cost,
                subscription_cost: stored.usage.subscription_cost,
                kickoff_pending: stored.kickoff_pending,
                generation: state.generation,
            };
            if legacy {
                state.journal(GoalLogWrite::Begun(begun_record(&snapshot)));
            }
            state.wait = stored.wait.map(|wait| GoalWait {
                attempt: wait.attempt,
                until_ms: wait.until_ms,
            });
            state.stalls =
                StallTracker::resumed(state.generation, state.wait.map(|wait| wait.attempt));
            state.active = Some(snapshot);
        }
        if let Some(stored) = meta.goal_result.as_deref()
            && state.active.is_none()
        {
            state.finished = Some(GoalResult {
                goal: stored.goal,
                set_at_ms: stored.set_at_ms,
                condition: Arc::from(stored.condition.as_str()),
                verdict: stored.verdict.into(),
                reason: Arc::from(stored.reason.as_str()),
                evaluations: stored.evaluations,
                duration: Duration::from_millis(stored.duration_ms),
                usage: stored.usage.into(),
                cost: stored.usage.cost,
                subscription_cost: stored.usage.subscription_cost,
            });
        }
        drop(state);
        handle
    }

    /// The goal as [`Self::from_meta`] reads it back. A finished result is
    /// kept only while no goal is active.
    pub fn stored(&self) -> StoredGoal {
        let state = self.lock();
        StoredGoal {
            active: state.active.as_ref().map(|goal| {
                Box::new(StoredActiveGoal {
                    goal: goal.goal,
                    set_at_ms: goal.set_at_ms,
                    condition: goal.condition.to_string(),
                    evaluations: goal.evaluations,
                    elapsed_ms: saturating_millis(goal.elapsed()),
                    usage: goal.usage.spent(goal.cost, goal.subscription_cost),
                    last_verdict: goal.last_verdict.map(Into::into),
                    last_reason: goal.last_reason.as_deref().map(str::to_owned),
                    wait: state.wait.map(|wait| StoredGoalWait {
                        attempt: wait.attempt,
                        until_ms: wait.until_ms,
                    }),
                    kickoff_pending: goal.kickoff_pending,
                })
            }),
            result: state
                .finished
                .as_ref()
                .filter(|_| state.active.is_none())
                .map(|goal| {
                    Box::new(StoredGoalResult {
                        goal: goal.goal,
                        set_at_ms: goal.set_at_ms,
                        condition: goal.condition.to_string(),
                        verdict: goal.verdict.into(),
                        reason: goal.reason.to_string(),
                        evaluations: goal.evaluations,
                        duration_ms: saturating_millis(goal.duration),
                        usage: goal.usage.spent(goal.cost, goal.subscription_cost),
                    })
                }),
            continuation_limit: Some(state.continuation_limit),
            log: (state.last_goal > 0 || state.last_seq > 0).then(|| StoredGoalCursor {
                last_goal: state.last_goal,
                last_seq: state.last_seq,
            }),
        }
    }

    pub fn set(&self, condition: &str) -> Result<GoalSnapshot, GoalError> {
        let condition = Goal::validate(condition)?;
        Ok(self.set_validated(condition, false))
    }

    /// Starts the next goal, ending an active one as replaced.
    fn set_validated(&self, condition: Arc<str>, kickoff_pending: bool) -> GoalSnapshot {
        let now_ms = self.now_ms();
        let mut state = self.lock();
        state.end_active(GoalEnd::Replaced, now_ms);
        state.generation = state.generation.wrapping_add(1);
        state.last_goal = state.last_goal.saturating_add(1);
        let snapshot = GoalSnapshot {
            goal: state.last_goal,
            set_at_ms: now_ms,
            condition,
            evaluations: 0,
            last_verdict: None,
            last_reason: None,
            started_at: Instant::now(),
            elapsed_before: Duration::ZERO,
            usage: TokenUsage::default(),
            cost: None,
            subscription_cost: None,
            kickoff_pending,
            generation: state.generation,
        };
        state.journal(GoalLogWrite::Begun(begun_record(&snapshot)));
        state.finished = None;
        state.active = Some(snapshot.clone());
        snapshot
    }

    /// Sets the goal for a session that is busy: it is active at once, and its
    /// first run is owed until [`Self::take_kickoff`] claims it.
    pub fn set_pending_kickoff(&self, condition: &str) -> Result<GoalSnapshot, GoalError> {
        let condition = Goal::validate(condition)?;
        Ok(self.set_validated(condition, true))
    }

    /// Whether the active goal still owes its first run.
    pub fn kickoff_pending(&self) -> bool {
        self.lock()
            .active
            .as_ref()
            .is_some_and(|goal| goal.kickoff_pending)
    }

    /// Claims the active goal's owed first run, true when one was owed.
    pub fn take_kickoff(&self) -> bool {
        self.lock()
            .active
            .as_mut()
            .is_some_and(|goal| mem::take(&mut goal.kickoff_pending))
    }

    pub fn clear(&self) -> Option<GoalSnapshot> {
        let now_ms = self.now_ms();
        self.lock().end_active(GoalEnd::Cleared, now_ms)
    }

    /// Clears the active goal because the run failed with `message`.
    pub fn clear_after_error(&self, message: &str) -> Option<GoalSnapshot> {
        let now_ms = self.now_ms();
        self.lock().end_active(
            GoalEnd::Error {
                message: message.to_owned(),
            },
            now_ms,
        )
    }

    pub fn reset(&self) {
        let now_ms = self.now_ms();
        let mut state = self.lock();
        state.end_active(GoalEnd::Cleared, now_ms);
        state.generation = state.generation.wrapping_add(1);
        state.finished = None;
    }

    pub fn restore_finished(&self, result: GoalResult) {
        let mut state = self.lock();
        if state.active.is_none() {
            state.finished = Some(result);
        }
    }

    /// Takes over `other`'s goal, its number and its log numbering without
    /// journaling anything: the goal moves between handles, it does not
    /// start or end. Writes `other` had yet to hand over move with it.
    pub fn adopt(&self, other: &GoalHandle) {
        if Arc::ptr_eq(&self.inner, &other.inner) {
            return;
        }
        let (generation, active, finished, last_goal, last_seq, wait, mut unlogged) = {
            let mut other = other.lock();
            (
                other.generation,
                other.active.clone(),
                other.finished.clone(),
                other.last_goal,
                other.last_seq,
                other.wait,
                mem::take(&mut other.unlogged),
            )
        };
        let mut state = self.lock();
        state.generation = state.generation.max(generation).wrapping_add(1);
        let current = state.generation;
        state.wait = wait.filter(|_| active.is_some());
        state.stalls = StallTracker::resumed(current, state.wait.map(|wait| wait.attempt));
        state.active = active.map(|goal| GoalSnapshot {
            generation: current,
            ..goal
        });
        state.finished = finished;
        state.last_goal = state.last_goal.max(last_goal);
        state.last_seq = state.last_seq.max(last_seq);
        for write in unlogged.drain(..) {
            state.journal(write);
        }
    }

    pub fn snapshot(&self) -> Option<GoalSnapshot> {
        self.lock().active.clone()
    }

    pub fn status(&self) -> Option<GoalStatus> {
        let state = self.lock();
        state
            .active
            .clone()
            .map(GoalStatus::Active)
            .or_else(|| state.finished.clone().map(GoalStatus::Finished))
    }

    pub fn active_condition(&self) -> Option<String> {
        self.lock()
            .active
            .as_ref()
            .map(|goal| goal.condition.to_string())
    }

    pub fn continuation_limit(&self) -> u32 {
        self.lock().continuation_limit
    }

    pub fn set_continuation_limit(&self, limit: u32) {
        self.lock().continuation_limit = limit.min(MAX_GOAL_CONTINUATION_LIMIT);
    }

    /// Unix milliseconds, as goal log rows are stamped.
    pub fn now_ms(&self) -> u64 {
        u64::try_from(self.clock.now_ms()).unwrap_or_default()
    }

    /// Files one check of goal `goal`, the number its check started under, so
    /// a check that outlived its goal still lands under the goal it judged.
    pub(crate) fn log_evaluation(&self, goal: u32, draft: EvaluationDraft) -> LoggedEvaluation {
        let at_ms = self.now_ms();
        let mut state = self.lock();
        state.last_seq = state.last_seq.saturating_add(1);
        let seq = state.last_seq;
        state.journal(GoalLogWrite::Evaluated(Box::new(StoredGoalEvaluation {
            seq,
            goal,
            evaluation: draft.evaluation,
            started_at_ms: draft.started_at_ms,
            at_ms,
            history_len: draft.history_len,
            verdict: draft.verdict.map(|verdict| verdict.to_string()),
            reason: draft.reason,
            outcome: draft.outcome,
            model: draft.model,
            usage: draft.usage,
            cost: draft.cost,
        })));
        LoggedEvaluation { seq, at_ms }
    }

    /// The goal log writes journaled since the last take, oldest first.
    pub fn take_log(&self) -> Vec<GoalLogWrite> {
        self.lock().unlogged.drain(..).collect()
    }

    pub fn has_unlogged(&self) -> bool {
        !self.lock().unlogged.is_empty()
    }

    pub fn record_external_usage(&self, usage: TokenUsage, cost: Option<f64>, billing: Billing) {
        self.record_usage(usage, cost, billing);
    }

    pub(crate) fn record_usage(&self, usage: TokenUsage, cost: Option<f64>, billing: Billing) {
        if let Some(active) = self.lock().active.as_mut() {
            active.usage += usage;
            add_spend(active, cost, billing);
        }
    }

    pub(crate) fn record_usage_for(
        &self,
        generation: u64,
        usage: TokenUsage,
        cost: Option<f64>,
        billing: Billing,
    ) {
        let mut state = self.lock();
        if let Some(active) = state.active.as_mut()
            && active.generation == generation
        {
            active.usage += usage;
            add_spend(active, cost, billing);
        }
    }

    pub(crate) fn is_generation_active(&self, generation: u64) -> bool {
        self.lock()
            .active
            .as_ref()
            .is_some_and(|goal| goal.generation == generation)
    }

    /// Records a turn of goal `generation` and says what the goal does next,
    /// [`GoalPace::Going`] once that goal is no longer active. A wait is kept
    /// on the goal, so it is saved with the session and outlives this process.
    pub(crate) fn record_turn(&self, generation: u64, turn: &GoalTurn) -> GoalPace {
        let now_ms = self.now_ms();
        let mut state = self.lock();
        if !state
            .active
            .as_ref()
            .is_some_and(|goal| goal.generation == generation)
        {
            return GoalPace::Going;
        }
        let pace = state.stalls.record(generation, turn);
        if let GoalPace::Wait { attempt, delay } = pace {
            state.wait = Some(GoalWait {
                attempt,
                until_ms: now_ms.saturating_add(saturating_millis(delay)),
            });
        }
        pace
    }

    /// The active goal's wait on an outside change.
    pub fn wait(&self) -> Option<GoalWait> {
        self.lock().wait
    }

    /// How long the active goal still waits, `Some(ZERO)` once its wait is
    /// over and `None` while it is not waiting.
    pub fn wait_left(&self) -> Option<Duration> {
        let now_ms = self.now_ms();
        self.wait()
            .map(|wait| Duration::from_millis(wait.until_ms.saturating_sub(now_ms)))
    }

    /// Whether the active goal's wait is over, so its check-in is due.
    pub fn wait_over(&self) -> bool {
        self.wait_left().is_some_and(|left| left.is_zero())
    }

    /// Ends the active goal's wait: a run started, or the user stopped it.
    pub fn end_wait(&self) {
        self.lock().wait = None;
    }

    /// Leaves goal `generation`, stalled and sent on, one turn short of
    /// stalling again.
    pub(crate) fn hold_stall(&self, generation: u64) {
        self.lock().stalls.hold(generation);
    }

    pub(crate) fn apply_evaluation(
        &self,
        generation: u64,
        verdict: GoalVerdict,
        reason: Arc<str>,
    ) -> GoalApply {
        let now_ms = self.now_ms();
        let mut state = self.lock();
        let Some(active) = state
            .active
            .as_mut()
            .filter(|goal| goal.generation == generation)
        else {
            return GoalApply::Stale;
        };
        active.evaluations = active.evaluations.saturating_add(1);
        active.last_verdict = Some(verdict);
        active.last_reason = Some(Arc::clone(&reason));
        let evaluation = active.evaluations;
        if verdict == GoalVerdict::NotMet {
            return GoalApply::Continue { evaluation };
        }

        let end = if verdict == GoalVerdict::Met {
            GoalEnd::Met
        } else {
            GoalEnd::Impossible
        };
        let active = state
            .end_active(end, now_ms)
            .expect("active goal checked above");
        state.finished = Some(GoalResult {
            goal: active.goal,
            set_at_ms: active.set_at_ms,
            duration: active.elapsed(),
            condition: active.condition,
            verdict,
            reason,
            evaluations: active.evaluations,
            usage: active.usage,
            cost: active.cost,
            subscription_cost: active.subscription_cost,
        });
        GoalApply::Terminal
    }

    fn lock(&self) -> MutexGuard<'_, GoalState> {
        self.inner.lock().unwrap_or_else(|error| error.into_inner())
    }
}

/// A goal as its log row records it, with no end and its history placement
/// left for the frontend to stamp.
fn begun_record(goal: &GoalSnapshot) -> StoredGoalRecord {
    StoredGoalRecord {
        goal: goal.goal,
        condition: goal.condition.to_string(),
        set_at_ms: goal.set_at_ms,
        set_history_len: 0,
        ended_at_ms: None,
        ended_history_len: None,
        end: None,
        evaluations: goal.evaluations,
        elapsed_ms: saturating_millis(goal.elapsed()),
        usage: goal.usage.spent(goal.cost, goal.subscription_cost),
        cost_usd: goal.cost.unwrap_or_default(),
        subscription_cost_usd: goal.subscription_cost.unwrap_or_default(),
    }
}

fn ended_record(goal: &GoalSnapshot, end: GoalEnd, at_ms: u64) -> StoredGoalRecord {
    StoredGoalRecord {
        ended_at_ms: Some(at_ms),
        end: Some(end),
        ..begun_record(goal)
    }
}

struct Goal;

impl Goal {
    fn validate(condition: &str) -> Result<Arc<str>, GoalError> {
        let condition = condition.trim();
        if condition.is_empty() {
            return Err(GoalError::Empty);
        }
        if condition.chars().count() > MAX_GOAL_CHARS {
            return Err(GoalError::TooLong);
        }
        Ok(Arc::from(condition))
    }
}

pub(crate) struct EvaluationResult {
    pub verdict: GoalVerdict,
    pub reason: String,
    /// Whether the evaluator saw the goal move since its previous verdict.
    pub progress: bool,
    /// Whether the evaluator saw the goal waiting on a change outside the
    /// session that may come by itself.
    pub waiting: bool,
    pub usage: TokenUsage,
    pub cost: Option<f64>,
    pub billing: Billing,
    pub model: String,
}

pub(crate) struct EvaluationError {
    pub error: AgentError,
    pub usage: TokenUsage,
    pub cost: Option<f64>,
    pub billing: Billing,
    pub model: String,
}

/// A side model that judges the conversation: the purpose its binding is read
/// under, and the name its resolution errors give it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct JudgeRole {
    pub purpose: ModelPurpose,
    pub label: &'static str,
}

#[derive(Clone)]
pub(crate) struct ResolvedEvaluator {
    /// The binding this was resolved from, or `None` when nothing is bound and
    /// the default rule chose the model.
    pub binding: Option<Binding>,
    pub provider: Arc<dyn Provider>,
    pub model: Model,
}

impl ResolvedEvaluator {
    /// Only an exactly bound model is worth keeping for the next resolution:
    /// any other binding can resolve elsewhere as the conversation model
    /// changes.
    pub(crate) fn cacheable(&self) -> bool {
        matches!(self.binding, Some(Binding::Exact(_)))
    }

    /// Only an unbound evaluator retreats to the chat model. A model the user
    /// named is reported instead, since silently substituting it would hide the
    /// choice they made.
    pub(crate) fn fallback_to_current(
        &self,
        error: &AgentError,
        current_provider: &Arc<dyn Provider>,
        current_model: &Model,
    ) -> Option<Self> {
        if self.binding.is_some()
            || !error.is_model_unavailable()
            || (self.model.provider == current_model.provider && self.model.id == current_model.id)
        {
            return None;
        }
        Some(Self {
            binding: None,
            provider: Arc::clone(current_provider),
            model: with_evaluator_limits(current_model.clone()),
        })
    }
}

/// Resolves `role`'s model under `binding`, reusing `cached`'s provider when it
/// was resolved from that same exact binding.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn resolve_evaluator(
    role: JudgeRole,
    current_provider: &Arc<dyn Provider>,
    current_model: &Model,
    binding: Option<Binding>,
    timeouts: Timeouts,
    model_policy: &ModelPolicy,
    cancel: &CancelToken,
    cached: Option<&ResolvedEvaluator>,
) -> Result<ResolvedEvaluator, AgentError> {
    let cached_provider = cached
        .filter(|cached| cached.cacheable() && cached.binding == binding)
        .map(|cached| Arc::clone(&cached.provider));
    let current_provider = Arc::clone(current_provider);
    let current_model = current_model.clone();
    let current_slug = Arc::clone(&current_model.provider);
    let model_policy = model_policy.clone();
    let bound = binding.clone();
    let (model, provider) = initialization_with_limits(cancel, async move {
        let mut model = smol::unblock(move || {
            evaluator_model(role, bound.as_ref(), &current_model, &model_policy)
        })
        .await?;
        let provider: Arc<dyn Provider> = if model.provider == current_slug {
            current_provider.adjust_model(&mut model);
            current_provider
        } else if let Some(provider) = cached_provider {
            provider.adjust_model(&mut model);
            provider
        } else {
            Arc::from(from_model_async(&mut model, timeouts).await?)
        };
        Ok((model, provider))
    })
    .await?;
    let model = with_evaluator_limits(model);
    Ok(ResolvedEvaluator {
        binding,
        provider,
        model,
    })
}

fn with_evaluator_limits(mut model: Model) -> Model {
    model.max_output_tokens = Some(
        model
            .max_output_tokens
            .unwrap_or(EVALUATOR_OUTPUT_TOKENS)
            .min(EVALUATOR_OUTPUT_TOKENS),
    );
    model
}

async fn initialization_with_limits<T>(
    cancel: &CancelToken,
    future: impl Future<Output = Result<T, AgentError>>,
) -> Result<T, AgentError> {
    let timed = futures_lite::future::race(future, async {
        smol::Timer::after(EVALUATOR_INITIALIZATION_TIMEOUT).await;
        Err(AgentError::Timeout {
            secs: EVALUATOR_INITIALIZATION_TIMEOUT.as_secs(),
        })
    });
    cancel
        .race(timed)
        .await
        .map_err(|_| AgentError::Cancelled)?
}

fn evaluator_model(
    role: JudgeRole,
    binding: Option<&Binding>,
    current_model: &Model,
    model_policy: &ModelPolicy,
) -> Result<Model, AgentError> {
    Model::resolve_binding(role.purpose, binding, current_model, model_policy).map_err(|error| {
        AgentError::Config {
            message: format!("cannot resolve the {} model: {error}", role.label),
        }
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EvaluationWire {
    ok: bool,
    reason: String,
    #[serde(default)]
    impossible: bool,
    #[serde(default = "assume_progress")]
    progress: bool,
    #[serde(default)]
    waiting: bool,
}

/// An evaluator that leaves `progress` out has not seen the goal stall.
fn assume_progress() -> bool {
    true
}

pub(crate) struct Evaluator<'a> {
    pub provider: &'a dyn Provider,
    pub model: &'a Model,
    pub history: &'a [Message],
    pub condition: &'a str,
    pub evaluation: u32,
    pub event_tx: &'a EventSender,
    pub cancel: &'a CancelToken,
}

impl Evaluator<'_> {
    pub async fn run(&self) -> Result<EvaluationResult, EvaluationError> {
        let model = self.model;

        let mut percent = INITIAL_TRANSCRIPT_PERCENT;
        let mut validation_attempt = 0;
        let mut previous_error = None;
        let mut previous_output = None;
        let mut usage = TokenUsage::default();
        let mut cost = None;

        loop {
            let mut messages = transcript_within_budget(self.history, model, percent);
            messages.push(Message::synthetic(evaluator_prompt(
                self.condition,
                self.evaluation,
                previous_error.as_deref(),
                previous_output.as_deref(),
            )));

            let response = match evaluator_request(
                self.provider,
                model,
                &messages,
                crate::prompt::GOAL_EVALUATOR,
                self.event_tx,
                self.cancel,
            )
            .await
            {
                Ok(response) => response,
                Err(error)
                    if error.is_context_overflow() && percent == INITIAL_TRANSCRIPT_PERCENT =>
                {
                    percent = RETRY_TRANSCRIPT_PERCENT;
                    continue;
                }
                Err(error) => {
                    return Err(EvaluationError {
                        error,
                        usage,
                        cost,
                        billing: model.billing,
                        model: model.spec(),
                    });
                }
            };
            usage += response.usage;
            add_cost(&mut cost, model.billed_cost(&response.usage, false));

            let (error, output) = match response_text(&response.message) {
                Ok(output) => match parse_evaluation(&output) {
                    Ok(Evaluation {
                        verdict,
                        reason,
                        progress,
                        waiting,
                    }) => {
                        return Ok(EvaluationResult {
                            verdict,
                            reason,
                            progress,
                            waiting,
                            usage,
                            cost,
                            billing: model.billing,
                            model: model.spec(),
                        });
                    }
                    Err(error) => (error, Some(output)),
                },
                Err(error) => (error, None),
            };
            validation_attempt += 1;
            if validation_attempt >= MAX_VALIDATION_ATTEMPTS {
                return Err(EvaluationError {
                    error: AgentError::Tool {
                        tool: "goal_evaluator".into(),
                        message: format!(
                            "invalid evaluator response after {MAX_VALIDATION_ATTEMPTS} attempts: {error}"
                        ),
                    },
                    usage,
                    cost,
                    billing: model.billing,
                    model: model.spec(),
                });
            }
            previous_error = Some(error);
            previous_output = output.as_deref().map(truncate_output);
        }
    }
}

pub(crate) async fn evaluator_request(
    provider: &dyn Provider,
    model: &Model,
    messages: &[Message],
    system: &str,
    event_tx: &EventSender,
    cancel: &CancelToken,
) -> Result<caudra_providers::StreamResponse, AgentError> {
    let tools = json!([]);
    let request = stream_silent_with_retry(
        provider,
        model,
        messages,
        system,
        &tools,
        Some(event_tx),
        cancel,
        RequestOptions::default(),
        None,
    );
    let result = request.await.map_err(Into::into);
    event_tx.send(AgentEvent::PromptProgress {
        processed: 0,
        total: 0,
        cache: 0,
    })?;
    result
}

/// Re-containers tool traffic as plain text. Tool output is the evidence the
/// evaluator judges, so nothing is dropped; the point is that a transcript of
/// `tool_use` blocks demonstrates tool calling to the one model being asked not
/// to call tools, and weak models follow the demonstration over the
/// instruction. It also stops the request contradicting itself, since the
/// evaluator declares no tools while replaying a conversation full of them.
///
/// Message count is preserved, which the omission notice in
/// [`transcript_within_budget`] counts on.
fn flatten_tool_blocks(messages: &mut [Message]) {
    judging::flatten_tool_blocks(messages, |block| match block {
        ToolBlock::Call { name, input } => format!("{TOOL_CALL_LINE} {name} {input}"),
        ToolBlock::Result {
            name,
            content,
            is_error,
        } => format!("{} {name}\n{content}", result_line(is_error)),
    });
}

/// Drops the agent's reasoning, and any turn that held nothing else. It is
/// the agent's own working rather than evidence, and only the model that
/// wrote it can replay it: an interrupted block carries no signature, and
/// another model's or provider's block fails its check.
fn strip_reasoning(messages: &mut Vec<Message>) {
    messages.retain_mut(|message| {
        let before = message.content.len();
        message.content.retain(|block| !block.is_thinking());
        message.content.len() == before || message.is_empty_padding() || !message.content.is_empty()
    });
}

fn transcript_within_budget(history: &[Message], model: &Model, percent: u32) -> Vec<Message> {
    let mut messages = history.to_vec();
    // Before budgeting, so the estimate measures what is actually sent.
    strip_reasoning(&mut messages);
    flatten_tool_blocks(&mut messages);
    let budget = model.context_window.saturating_mul(percent) / 100;
    let omitted = drop_oldest_rounds(&mut messages, |messages| {
        estimate_message_tokens(messages) <= budget
    });
    if omitted > 0 {
        messages.insert(
            0,
            Message::synthetic(format!(
                "[Earlier conversation truncated to fit the goal evaluator context window: {omitted} message(s) omitted. If required evidence may be in the omitted prefix, return not met with reason \"insufficient evidence in transcript\".]"
            )),
        );
    }
    messages
}

fn evaluator_prompt(
    condition: &str,
    evaluation: u32,
    previous_error: Option<&str>,
    previous_output: Option<&str>,
) -> String {
    let condition = serde_json::to_string(condition).unwrap_or_else(|_| "\"\"".into());
    let mut prompt = format!("Evaluation number: {evaluation}\nCondition: {condition}");
    if let Some(error) = previous_error {
        prompt.push_str("\n\n");
        prompt.push_str(&correction(error, previous_output));
    }
    prompt
}

/// A stray tool call beside a real answer is ignored rather than fatal:
/// [`parse_evaluation`] is the arbiter of the text, and a bad answer reports the
/// more actionable JSON error. Only a reply that is *nothing but* a tool call
/// fails, and it names the tool so the retry has something concrete to correct.
fn response_text(message: &Message) -> Result<String, String> {
    let mut output = String::new();
    for block in &message.content {
        if let ContentBlock::Text { text } = block {
            output.push_str(text);
        }
    }
    if output.trim().is_empty() {
        return match message.tool_uses().next() {
            Some((_, name, _)) => Err(format!("{TOOL_CALL_ERROR} ({name})")),
            None => Err(NO_TEXT_ERROR.into()),
        };
    }
    if output.len() > MAX_EVALUATOR_OUTPUT_BYTES {
        return Err(OVERSIZE_ERROR.into());
    }
    Ok(output)
}

#[derive(Debug, PartialEq)]
struct Evaluation {
    verdict: GoalVerdict,
    reason: String,
    progress: bool,
    waiting: bool,
}

fn parse_evaluation(output: &str) -> Result<Evaluation, String> {
    let candidate = json_object(output).ok_or_else(|| ONE_JSON_OBJECT_ERROR.to_owned())?;
    let value =
        serde_json::from_str::<EvaluationWire>(candidate).map_err(|error| error.to_string())?;
    let reason = value.reason.trim();
    if reason.is_empty() {
        return Err("reason is empty".into());
    }
    if value.ok && value.impossible {
        return Err("ok and impossible cannot both be true".into());
    }
    let verdict = if value.impossible {
        GoalVerdict::Impossible
    } else if value.ok {
        GoalVerdict::Met
    } else {
        GoalVerdict::NotMet
    };
    Ok(Evaluation {
        verdict,
        reason: reason.to_string(),
        progress: value.progress,
        waiting: value.waiting && !value.ok,
    })
}

/// Saturates rather than wraps: a goal left open for longer than `u64`
/// milliseconds is not a number worth panicking over.
fn saturating_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn add_cost(total: &mut Option<f64>, cost: Option<f64>) {
    if let Some(cost) = cost {
        *total = Some(total.unwrap_or_default() + cost);
    }
}

/// A goal's running spend, filed under whoever pays for the turn that added it.
fn add_spend(goal: &mut GoalSnapshot, cost: Option<f64>, billing: Billing) {
    match billing {
        Billing::Api => add_cost(&mut goal.cost, cost),
        Billing::Subscription => add_cost(&mut goal.subscription_cost, cost),
    }
}

pub(crate) fn continuation_message(condition: &str, reason: &str) -> String {
    let condition = serde_json::to_string(condition).unwrap_or_else(|_| "\"\"".into());
    let reason = serde_json::to_string(reason).unwrap_or_else(|_| "\"\"".into());
    format!(
        "The active goal is not yet satisfied.\nGoal: {condition}\nEvaluator reason: {reason}\nContinue working toward the goal and produce evidence in the conversation."
    )
}

pub fn goal_kickoff_message(condition: &str) -> String {
    let condition = serde_json::to_string(condition).unwrap_or_else(|_| "\"\"".into());
    format!(
        "A session-scoped goal is active with condition: {condition}. Briefly acknowledge the goal, then immediately start or continue working toward it. Treat the condition as your directive and do not pause to ask what to do. Completion will be evaluated automatically; do not claim success without evidence."
    )
}

pub fn goal_checkin_message(condition: &str) -> String {
    let condition = serde_json::to_string(condition).unwrap_or_else(|_| "\"\"".into());
    format!(
        "Goal check-in: {condition} is still active. Evaluation was deferred while background work ran, and that work is no longer running. Review its results and continue toward the goal."
    )
}

/// The hidden prompt of the run that checks a goal again once its wait on an
/// outside change is over.
pub fn goal_wait_message(condition: &str) -> String {
    let condition = serde_json::to_string(condition).unwrap_or_else(|_| "\"\"".into());
    format!(
        "Goal check-in: {condition} is still active. It was waiting on a change outside the session, and the wait is over. Check whether that change came, then continue toward the goal."
    )
}

pub(crate) fn is_unrecoverable(error: &AgentError) -> bool {
    if error.is_auth_error() || error.is_context_overflow() || error.is_model_unavailable() {
        return true;
    }
    let AgentError::Api {
        status, message, ..
    } = error
    else {
        return false;
    };
    let message = message.to_ascii_lowercase();
    *status == 402
        || message.contains("credit balance")
        || message.contains("insufficient credit")
        || ((*status == 401 || *status == 403)
            && (message.contains("authentication")
                || message.contains("oauth")
                || message.contains("organization")
                || message.contains("account on hold")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::goal_progress::Judgement;
    use crate::agent::judging::{TOOL_ERROR_LINE, TOOL_RESULT_LINE, UNNAMED_TOOL};
    use crate::automation::clock::FakeClock;
    use caudra_providers::provider::BoxFuture;
    use caudra_providers::{
        CacheKey, ImageMediaType, ImageSource, ModelInfo, ProviderEvent, Role, StreamResponse,
    };
    use serde_json::Value;
    use test_case::test_case;

    const CHAT_SPEC: &str = "anthropic/claude-sonnet-4-20250514";
    const HAIKU_SPEC: &str = "anthropic/claude-haiku-4-5";
    const OPUS_SPEC: &str = "anthropic/claude-opus-4-6-20260101";
    const CALL_ID: &str = "call_1";
    const TOOL_NAME: &str = "file_read";
    const READ_PATH: &str = "src/main.rs";
    const RESULT_BODY: &str = "fn main() {}";
    const PROMPT: &str = "ship it";
    const THOUGHT: &str = "weighing the evidence";
    const SIGNATURE: &str = "signed-by-the-chat-model";
    const REDACTED_DATA: &str = "opaque";
    const NO_REASONING: &str = "the evaluator must never receive the agent's reasoning";
    const REASONING_ONLY_TURN_DROPPED: &str = "a turn that held only reasoning has nothing left";
    const IMAGE_DATA: &str = "aGk=";
    const ANSWER_JSON: &str = r#"{"ok":true,"reason":"tests pass","impossible":false}"#;
    const NO_TOOL_BLOCKS: &str = "flattening must leave no tool blocks behind";
    const VERIFIED: &str = "verified";
    const ELAPSED_BEFORE_RESUME_MS: u64 = 90_000;
    const COUNTS_TIME_BEFORE_RESUME: &str =
        "a resumed goal's duration must include the time it ran before the resume";
    const OTHER_PROMPT: &str = "ship it again";
    const RUN_ERROR: &str = "credit balance too low";
    const START_MS: i64 = 1_767_225_600_000;
    const START_MS_U64: u64 = START_MS as u64;
    const RAN: Duration = Duration::from_secs(90);
    const RAN_MS: u64 = 90_000;
    const GOAL_COST: f64 = 0.25;
    const NUMBERED_GOAL: u32 = 4;
    const LAST_SEQ: u64 = 40;
    const EXTRA_WRITES: usize = 3;
    const JOURNALS_IN_ORDER: &str = "each transition must journal its goal log writes in order";
    const JOURNALS_TIMES: &str = "a finished goal must journal when it ran and what it spent";
    const STALE_KEEPS_GOAL: &str = "a stale check must land under the goal it judged";
    const RESUMES_NUMBERING: &str = "numbering must continue from the stored cursor";
    const RESTORED_NUMBERED: &str =
        "a restored goal must keep its number, or take and journal the next one";
    const CAP_DROPS_OLDEST: &str = "a full backlog must drop its oldest write";
    const ADOPT_LOGS_NOTHING: &str = "adopting a goal must journal nothing of its own";
    const KICKOFF_SURVIVES: &str = "a goal's owed kickoff must survive a resume";
    const KICKOFF_CLAIMED_ONCE: &str = "an owed kickoff must be claimed exactly once";
    const KICKOFF_ENDS_WITH_GOAL: &str = "a replaced or ended goal must owe no kickoff";

    struct NullProvider;

    struct ProgressProvider;

    impl Provider for NullProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            _: &'a [Message],
            _: &'a str,
            _: &'a Value,
            _: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a CacheKey>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async { unimplemented!() })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { unimplemented!() })
        }
    }

    impl Provider for ProgressProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            _: &'a [Message],
            _: &'a str,
            _: &'a Value,
            event_tx: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a CacheKey>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async {
                event_tx.send(ProviderEvent::PromptProgress {
                    processed: 100,
                    total: 1_000,
                    cache: 50,
                })?;
                Ok(StreamResponse::default())
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { unimplemented!() })
        }
    }

    #[test]
    fn validates_condition_by_characters() {
        assert_eq!(Goal::validate("  ship it  ").unwrap().as_ref(), "ship it");
        assert_eq!(Goal::validate("").unwrap_err(), GoalError::Empty);
        assert_eq!(
            Goal::validate(&"é".repeat(MAX_GOAL_CHARS + 1)).unwrap_err(),
            GoalError::TooLong
        );
    }

    #[test]
    fn continuation_limit_is_session_scoped_and_bounded() {
        let handle = GoalHandle::default();
        assert_eq!(handle.continuation_limit(), DEFAULT_GOAL_CONTINUATION_LIMIT);

        handle.set_continuation_limit(24);
        handle.set("first").unwrap();
        handle.clear();
        handle.set("second").unwrap();
        assert_eq!(handle.continuation_limit(), 24);

        handle.set_continuation_limit(MAX_GOAL_CONTINUATION_LIMIT + 1);
        assert_eq!(handle.continuation_limit(), MAX_GOAL_CONTINUATION_LIMIT);
    }

    #[test]
    fn evaluator_request_forwards_prefill_progress_and_clears_it() {
        smol::block_on(async {
            let provider = ProgressProvider;
            let model = Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap();
            let (raw_tx, event_rx) = flume::unbounded();
            let event_tx = EventSender::new(raw_tx, 0);

            evaluator_request(
                &provider,
                &model,
                &[],
                "system",
                &event_tx,
                &CancelToken::none(),
            )
            .await
            .unwrap();
            drop(event_tx);

            let events: Vec<_> = event_rx.drain().map(|envelope| envelope.event).collect();
            assert!(matches!(
                events.as_slice(),
                [
                    AgentEvent::PromptProgress {
                        processed: 100,
                        total: 1_000,
                        cache: 50,
                    },
                    AgentEvent::PromptProgress {
                        processed: 0,
                        total: 0,
                        cache: 0,
                    },
                ]
            ));
        });
    }

    #[test]
    fn stale_evaluation_cannot_clear_replacement() {
        let handle = GoalHandle::default();
        let old = handle.set("old").unwrap();
        handle.set("new").unwrap();
        assert!(matches!(
            handle.apply_evaluation(old.generation, GoalVerdict::Met, Arc::from("done")),
            GoalApply::Stale
        ));
        assert_eq!(handle.snapshot().unwrap().condition.as_ref(), "new");
    }

    #[test]
    fn terminal_evaluation_records_finished_status() {
        let handle = GoalHandle::default();
        let goal = handle.set("tests pass").unwrap();
        assert!(matches!(
            handle.apply_evaluation(goal.generation, GoalVerdict::Met, Arc::from("verified")),
            GoalApply::Terminal
        ));
        assert!(handle.snapshot().is_none());
        let Some(GoalStatus::Finished(result)) = handle.status() else {
            panic!("finished goal missing");
        };
        assert_eq!(result.verdict, GoalVerdict::Met);
        assert_eq!(result.reason.as_ref(), "verified");
    }

    #[test]
    fn a_resumed_goal_finishes_with_the_time_it_ran_before_the_resume() {
        let handle = GoalHandle::from_meta(&SessionMeta {
            active_goal: Some(Box::new(stored_active(NUMBERED_GOAL))),
            ..SessionMeta::default()
        });
        let goal = handle.snapshot().expect("the stored goal must resume");
        assert!(matches!(
            handle.apply_evaluation(goal.generation, GoalVerdict::Met, Arc::from(VERIFIED)),
            GoalApply::Terminal
        ));
        let Some(GoalStatus::Finished(result)) = handle.status() else {
            panic!("finished goal missing");
        };
        assert!(
            result.duration >= Duration::from_millis(ELAPSED_BEFORE_RESUME_MS),
            "{COUNTS_TIME_BEFORE_RESUME}"
        );
    }

    #[derive(Debug, PartialEq)]
    enum Journaled {
        Begun(u32),
        Ended(u32, GoalEnd),
        Evaluated { seq: u64, goal: u32 },
    }

    #[derive(Debug, Clone, Copy)]
    enum Transition {
        Set,
        Replace,
        Clear,
        ClearAfterError,
        Met,
        Impossible,
        Reset,
    }

    fn clocked() -> (Arc<FakeClock>, GoalHandle) {
        let clock = FakeClock::new(START_MS);
        (Arc::clone(&clock), GoalHandle::new(clock))
    }

    fn journaled(handle: &GoalHandle) -> Vec<Journaled> {
        handle
            .take_log()
            .into_iter()
            .map(|write| match write {
                GoalLogWrite::Begun(record) => Journaled::Begun(record.goal),
                GoalLogWrite::Ended(record) => {
                    Journaled::Ended(record.goal, record.end.expect("an end must say how"))
                }
                GoalLogWrite::Evaluated(evaluation) => Journaled::Evaluated {
                    seq: evaluation.seq,
                    goal: evaluation.goal,
                },
            })
            .collect()
    }

    fn draft() -> EvaluationDraft {
        EvaluationDraft {
            evaluation: 1,
            started_at_ms: START_MS_U64,
            history_len: 0,
            verdict: Some(GoalVerdict::NotMet),
            reason: VERIFIED.into(),
            outcome: GoalEvaluationOutcome::Applied,
            model: None,
            usage: None,
            cost: None,
        }
    }

    fn stored_active(goal: u32) -> StoredActiveGoal {
        StoredActiveGoal {
            goal,
            set_at_ms: 0,
            condition: PROMPT.to_owned(),
            evaluations: 0,
            elapsed_ms: ELAPSED_BEFORE_RESUME_MS,
            usage: Default::default(),
            last_verdict: None,
            last_reason: None,
            wait: None,
            kickoff_pending: false,
        }
    }

    #[test_case(Transition::Set, vec![Journaled::Begun(1)] ; "set_begins")]
    #[test_case(Transition::Replace, vec![Journaled::Begun(1), Journaled::Ended(1, GoalEnd::Replaced), Journaled::Begun(2)] ; "replace_ends_then_begins")]
    #[test_case(Transition::Clear, vec![Journaled::Begun(1), Journaled::Ended(1, GoalEnd::Cleared)] ; "clear_ends_cleared")]
    #[test_case(Transition::ClearAfterError, vec![Journaled::Begun(1), Journaled::Ended(1, GoalEnd::Error { message: RUN_ERROR.into() })] ; "error_ends_with_its_message")]
    #[test_case(Transition::Met, vec![Journaled::Begun(1), Journaled::Ended(1, GoalEnd::Met)] ; "met_ends_met")]
    #[test_case(Transition::Impossible, vec![Journaled::Begun(1), Journaled::Ended(1, GoalEnd::Impossible)] ; "impossible_ends_impossible")]
    #[test_case(Transition::Reset, vec![Journaled::Begun(1), Journaled::Ended(1, GoalEnd::Cleared)] ; "reset_ends_cleared")]
    fn each_transition_journals_its_writes_in_order(
        transition: Transition,
        expected: Vec<Journaled>,
    ) {
        let (_clock, handle) = clocked();
        let goal = handle.set(PROMPT).unwrap();

        match transition {
            Transition::Set => {}
            Transition::Replace => {
                handle.set(OTHER_PROMPT).unwrap();
            }
            Transition::Clear => {
                handle.clear();
            }
            Transition::ClearAfterError => {
                handle.clear_after_error(RUN_ERROR);
            }
            Transition::Met | Transition::Impossible => {
                let verdict = if matches!(transition, Transition::Met) {
                    GoalVerdict::Met
                } else {
                    GoalVerdict::Impossible
                };
                handle.apply_evaluation(goal.generation, verdict, Arc::from(VERIFIED));
            }
            Transition::Reset => handle.reset(),
        }

        assert_eq!(journaled(&handle), expected, "{JOURNALS_IN_ORDER}");
    }

    #[test]
    fn a_finished_goal_journals_its_times_and_spend() {
        let (clock, handle) = clocked();
        let goal = handle.set(PROMPT).unwrap();
        handle.record_usage_for(
            goal.generation,
            TokenUsage::default(),
            Some(GOAL_COST),
            Billing::Api,
        );
        clock.advance(RAN);

        handle.apply_evaluation(goal.generation, GoalVerdict::Met, Arc::from(VERIFIED));

        let writes = handle.take_log();
        let [GoalLogWrite::Begun(begun), GoalLogWrite::Ended(ended)] = writes.as_slice() else {
            panic!("a goal set then met must journal its start and end: {writes:?}");
        };
        assert_eq!(
            (begun.set_at_ms, ended.set_at_ms, ended.ended_at_ms),
            (START_MS_U64, START_MS_U64, Some(START_MS_U64 + RAN_MS)),
            "{JOURNALS_TIMES}"
        );
        assert_eq!(
            (ended.evaluations, ended.cost_usd, ended.usage.cost),
            (1, GOAL_COST, Some(GOAL_COST)),
            "{JOURNALS_TIMES}"
        );
    }

    #[test]
    fn a_stale_evaluation_keeps_the_number_of_the_goal_it_judged() {
        let (_clock, handle) = clocked();
        let old = handle.set(PROMPT).unwrap();
        handle.set(OTHER_PROMPT).unwrap();
        handle.take_log();

        let logged = handle.log_evaluation(old.goal, draft());

        assert_eq!(
            (logged.seq, journaled(&handle)),
            (
                1,
                vec![Journaled::Evaluated {
                    seq: 1,
                    goal: old.goal
                }]
            ),
            "{STALE_KEEPS_GOAL}"
        );
    }

    #[test]
    fn numbering_resumes_from_the_cursor() {
        let handle = GoalHandle::from_meta_with_clock(
            &SessionMeta {
                goal_log: Some(StoredGoalCursor {
                    last_goal: NUMBERED_GOAL,
                    last_seq: LAST_SEQ,
                }),
                ..SessionMeta::default()
            },
            FakeClock::new(START_MS),
        );

        let goal = handle.set(PROMPT).unwrap();
        let logged = handle.log_evaluation(goal.goal, draft());

        assert_eq!(
            (goal.goal, logged.seq, handle.stored().log),
            (
                NUMBERED_GOAL + 1,
                LAST_SEQ + 1,
                Some(StoredGoalCursor {
                    last_goal: NUMBERED_GOAL + 1,
                    last_seq: LAST_SEQ + 1,
                })
            ),
            "{RESUMES_NUMBERING}"
        );
    }

    #[test]
    fn a_wait_outlives_the_session_and_its_backoff_carries_on() {
        const WAIT_SURVIVES: &str = "a goal's wait and its count must survive a resume";
        let waiting = GoalTurn {
            judgement: Some(Judgement {
                progressed: false,
                waiting: true,
            }),
            ..GoalTurn::default()
        };
        let (clock, handle) = clocked();
        let generation = handle.set(PROMPT).unwrap().generation;
        handle.record_turn(generation, &waiting);
        handle.record_turn(generation, &waiting);
        let wait = handle.wait().expect(WAIT_SURVIVES);
        let stored = handle.stored();

        let resumed = GoalHandle::from_meta_with_clock(
            &SessionMeta {
                active_goal: stored.active,
                ..SessionMeta::default()
            },
            Arc::clone(&clock) as Arc<dyn Clock>,
        );

        assert_eq!(resumed.wait(), Some(wait), "{WAIT_SURVIVES}");
        assert!(!resumed.wait_over(), "{WAIT_SURVIVES}");
        clock.advance(Duration::from_millis(wait.until_ms - START_MS_U64));
        assert!(resumed.wait_over(), "{WAIT_SURVIVES}");
        let generation = resumed.snapshot().unwrap().generation;
        assert!(
            matches!(
                resumed.record_turn(generation, &waiting),
                GoalPace::Wait { attempt, .. } if attempt == wait.attempt + 1
            ),
            "{WAIT_SURVIVES}"
        );
        resumed.end_wait();
        assert_eq!(resumed.wait(), None);
    }

    #[test]
    fn an_owed_kickoff_survives_a_resume_until_claimed() {
        let (clock, handle) = clocked();
        handle.set_pending_kickoff(PROMPT).unwrap();

        let resumed = GoalHandle::from_meta_with_clock(
            &SessionMeta {
                active_goal: handle.stored().active,
                ..SessionMeta::default()
            },
            clock as Arc<dyn Clock>,
        );

        assert!(resumed.kickoff_pending(), "{KICKOFF_SURVIVES}");
        assert!(resumed.take_kickoff(), "{KICKOFF_CLAIMED_ONCE}");
        assert!(!resumed.take_kickoff(), "{KICKOFF_CLAIMED_ONCE}");
        assert!(!resumed.kickoff_pending(), "{KICKOFF_CLAIMED_ONCE}");
        assert!(
            resumed
                .stored()
                .active
                .is_some_and(|goal| !goal.kickoff_pending),
            "{KICKOFF_CLAIMED_ONCE}"
        );
    }

    #[test_case(Transition::Replace ; "replaced_by_an_idle_goal")]
    #[test_case(Transition::Clear ; "cleared")]
    #[test_case(Transition::Reset ; "reset")]
    fn a_new_or_ended_goal_owes_no_kickoff(transition: Transition) {
        let (_clock, handle) = clocked();
        handle.set_pending_kickoff(PROMPT).unwrap();

        match transition {
            Transition::Replace => {
                handle.set(OTHER_PROMPT).unwrap();
            }
            Transition::Clear => {
                handle.clear();
            }
            Transition::Reset => handle.reset(),
            _ => unreachable!(),
        }

        assert!(!handle.kickoff_pending(), "{KICKOFF_ENDS_WITH_GOAL}");
        assert!(!handle.take_kickoff(), "{KICKOFF_ENDS_WITH_GOAL}");
    }

    #[test_case(0, NUMBERED_GOAL + 1, true ; "legacy_goal_takes_the_next_number")]
    #[test_case(NUMBERED_GOAL, NUMBERED_GOAL, false ; "numbered_goal_keeps_its_number")]
    fn a_restored_goal_is_numbered(stored_goal: u32, expected_goal: u32, begins: bool) {
        let handle = GoalHandle::from_meta_with_clock(
            &SessionMeta {
                active_goal: Some(Box::new(stored_active(stored_goal))),
                goal_log: Some(StoredGoalCursor {
                    last_goal: NUMBERED_GOAL,
                    last_seq: LAST_SEQ,
                }),
                ..SessionMeta::default()
            },
            FakeClock::new(START_MS),
        );

        let writes = handle.take_log();
        let snapshot = handle.snapshot().expect("the stored goal must resume");
        assert_eq!(snapshot.goal, expected_goal, "{RESTORED_NUMBERED}");
        let expected = begins.then_some((expected_goal, START_MS_U64 - ELAPSED_BEFORE_RESUME_MS));
        let begun = match writes.as_slice() {
            [GoalLogWrite::Begun(record)] => Some((record.goal, record.set_at_ms)),
            [] => None,
            other => panic!("a restore journals at most its start: {other:?}"),
        };
        assert_eq!(begun, expected, "{RESTORED_NUMBERED}");
    }

    #[test]
    fn a_full_backlog_drops_its_oldest_write() {
        let (_clock, handle) = clocked();
        for _ in 0..MAX_UNLOGGED_GOAL_WRITES + EXTRA_WRITES {
            handle.log_evaluation(NUMBERED_GOAL, draft());
        }

        let writes = journaled(&handle);

        assert_eq!(
            (writes.len(), writes.first()),
            (
                MAX_UNLOGGED_GOAL_WRITES,
                Some(&Journaled::Evaluated {
                    seq: EXTRA_WRITES as u64 + 1,
                    goal: NUMBERED_GOAL
                })
            ),
            "{CAP_DROPS_OLDEST}"
        );
    }

    #[test_case(true ; "drained_source")]
    #[test_case(false ; "undrained_source")]
    fn adopting_a_goal_journals_nothing_new(drained: bool) {
        let (_clock, source) = clocked();
        let goal = source.set(PROMPT).unwrap();
        source.log_evaluation(goal.goal, draft());
        if drained {
            source.take_log();
        }
        let (_other_clock, handle) = clocked();

        handle.adopt(&source);

        let expected = if drained {
            Vec::new()
        } else {
            vec![
                Journaled::Begun(goal.goal),
                Journaled::Evaluated {
                    seq: 1,
                    goal: goal.goal,
                },
            ]
        };
        assert_eq!(journaled(&handle), expected, "{ADOPT_LOGS_NOTHING}");
        assert!(!source.has_unlogged(), "{ADOPT_LOGS_NOTHING}");
        let adopted = handle.snapshot().expect("the adopted goal must be active");
        assert_eq!(
            (adopted.goal, handle.stored().log),
            (
                goal.goal,
                Some(StoredGoalCursor {
                    last_goal: goal.goal,
                    last_seq: 1,
                })
            ),
            "{ADOPT_LOGS_NOTHING}"
        );
        assert!(
            handle.is_generation_active(adopted.generation),
            "{ADOPT_LOGS_NOTHING}"
        );
    }

    #[test]
    fn parses_strict_and_fenced_json() {
        for input in [
            r#"{"ok":true,"reason":"tests pass","impossible":false}"#,
            "```json\n{\"ok\":false,\"reason\":\"lint missing\",\"impossible\":false}\n```",
        ] {
            assert!(parse_evaluation(input).is_ok(), "{input}");
        }
    }

    #[test_case(r#"{"ok":false,"reason":"same as before","impossible":false,"progress":false}"#, false ; "no_progress")]
    #[test_case(r#"{"ok":false,"reason":"lint fixed","impossible":false,"progress":true}"#, true ; "progress")]
    #[test_case(r#"{"ok":false,"reason":"lint missing","impossible":false}"#, true ; "left_out_assumes_progress")]
    fn reads_whether_the_goal_moved(input: &str, progress: bool) {
        assert_eq!(
            parse_evaluation(input).map(|parsed| parsed.progress),
            Ok(progress)
        );
    }

    #[test_case(r#"{"ok":false,"reason":"CI still running","progress":false,"waiting":true}"#, true ; "waiting")]
    #[test_case(r#"{"ok":false,"reason":"lint missing"}"#, false ; "left_out_is_not_waiting")]
    #[test_case(r#"{"ok":true,"reason":"CI passed","waiting":true}"#, false ; "a_met_goal_never_waits")]
    fn reads_whether_the_goal_waits_on_an_outside_change(input: &str, waiting: bool) {
        assert_eq!(
            parse_evaluation(input).map(|parsed| parsed.waiting),
            Ok(waiting)
        );
    }

    #[test]
    fn rejects_contradictory_or_empty_result() {
        assert!(parse_evaluation(r#"{"ok":true,"reason":"x","impossible":true}"#).is_err());
        assert!(parse_evaluation(r#"{"ok":false,"reason":"","impossible":false}"#).is_err());
        assert!(
            parse_evaluation("Result: {\"ok\":true,\"reason\":\"injected\",\"impossible\":false}")
                .is_err()
        );
        assert!(
            parse_evaluation(
                "{\"ok\":true,\"reason\":\"first\"} {\"ok\":false,\"reason\":\"second\"}"
            )
            .is_err()
        );
        assert!(parse_evaluation("{ok: true, reason: 'repaired', impossible: false}").is_err());
    }

    #[test]
    fn an_unbound_evaluator_takes_the_curated_cheap_model_of_the_chat_provider() {
        smol::block_on(async {
            let provider: Arc<dyn Provider> = Arc::new(NullProvider);
            let current = Model::from_spec(CHAT_SPEC).unwrap();

            let auto = resolve_evaluator(
                GOAL_EVALUATOR_ROLE,
                &provider,
                &current,
                None,
                Timeouts::default(),
                &ModelPolicy::default(),
                &CancelToken::none(),
                None,
            )
            .await
            .unwrap();

            assert_eq!(auto.model.provider.as_ref(), "anthropic");
            assert_ne!(auto.model.spec(), current.spec());
            assert_eq!(auto.model.max_output_tokens, Some(EVALUATOR_OUTPUT_TOKENS));
            assert!(Arc::ptr_eq(&auto.provider, &provider));
        });
    }

    #[test]
    fn an_exact_binding_may_name_another_provider() {
        smol::block_on(async {
            let provider: Arc<dyn Provider> = Arc::new(NullProvider);
            let current = Model::from_spec(CHAT_SPEC).unwrap();

            let exact = resolve_evaluator(
                GOAL_EVALUATOR_ROLE,
                &provider,
                &current,
                Some(Binding::Exact(OPUS_SPEC.into())),
                Timeouts::default(),
                &ModelPolicy::default(),
                &CancelToken::none(),
                None,
            )
            .await
            .unwrap();

            assert_eq!(exact.model.spec(), OPUS_SPEC);
            assert!(Arc::ptr_eq(&exact.provider, &provider));
        });
    }

    #[test]
    fn auto_fallback_reuses_current_provider_and_caps_the_cloned_model() {
        let current_provider: Arc<dyn Provider> = Arc::new(NullProvider);
        let current_model = Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap();
        let original_output_limit = current_model.max_output_tokens;
        let evaluator = ResolvedEvaluator {
            binding: None,
            provider: Arc::new(NullProvider),
            model: Model::from_spec(HAIKU_SPEC).unwrap(),
        };

        let fallback = evaluator
            .fallback_to_current(
                &AgentError::api(404, "model 'claude-haiku-4-5' not found"),
                &current_provider,
                &current_model,
            )
            .unwrap();

        assert!(Arc::ptr_eq(&fallback.provider, &current_provider));
        assert_eq!(fallback.model.spec(), current_model.spec());
        assert_eq!(
            fallback.model.max_output_tokens,
            Some(EVALUATOR_OUTPUT_TOKENS)
        );
        assert_eq!(current_model.max_output_tokens, original_output_limit);
    }

    #[test]
    fn only_an_unbound_evaluator_falls_back_from_an_unavailable_model() {
        let current_provider: Arc<dyn Provider> = Arc::new(NullProvider);
        let current_model = Model::from_spec(CHAT_SPEC).unwrap();
        let unavailable = AgentError::api(404, "unknown model claude-haiku-4-5");

        for binding in [
            Binding::Same(ModelPurpose::Fast),
            Binding::Exact(HAIKU_SPEC.into()),
        ] {
            let evaluator = ResolvedEvaluator {
                binding: Some(binding),
                provider: Arc::new(NullProvider),
                model: Model::from_spec(HAIKU_SPEC).unwrap(),
            };
            assert!(
                evaluator
                    .fallback_to_current(&unavailable, &current_provider, &current_model)
                    .is_none()
            );
        }
    }

    #[test]
    fn auto_does_not_fallback_for_an_unrelated_error_or_the_current_model() {
        let current_provider: Arc<dyn Provider> = Arc::new(NullProvider);
        let current_model = Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap();
        let evaluator = ResolvedEvaluator {
            binding: None,
            provider: Arc::new(NullProvider),
            model: current_model.clone(),
        };

        assert!(
            evaluator
                .fallback_to_current(
                    &AgentError::api(404, "model 'claude-sonnet-4-20250514' not found"),
                    &current_provider,
                    &current_model,
                )
                .is_none()
        );

        let other_model = ResolvedEvaluator {
            model: Model::from_spec(HAIKU_SPEC).unwrap(),
            ..evaluator
        };
        assert!(
            other_model
                .fallback_to_current(
                    &AgentError::api(404, "route not found"),
                    &current_provider,
                    &current_model,
                )
                .is_none()
        );
    }

    #[test]
    fn explicit_goal_evaluator_never_silently_falls_back() {
        smol::block_on(async {
            let provider: Arc<dyn Provider> = Arc::new(NullProvider);
            let current = Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap();
            let policy = ModelPolicy::new(&[], &["anthropic/claude-opus*".into()]).unwrap();

            let disallowed = resolve_evaluator(
                GOAL_EVALUATOR_ROLE,
                &provider,
                &current,
                Some(Binding::Exact("anthropic/claude-opus-4-6".into())),
                Timeouts::default(),
                &policy,
                &CancelToken::none(),
                None,
            )
            .await
            .err()
            .expect("disallowed exact model should fail");
            assert!(disallowed.to_string().contains("not allowed"));
        });
    }

    #[test]
    fn evaluator_initialization_honors_cancellation() {
        smol::block_on(async {
            let (trigger, cancel) = CancelToken::new();
            let initialization = initialization_with_limits(
                &cancel,
                futures_lite::future::pending::<Result<(), AgentError>>(),
            );
            let cancel_now = async move {
                trigger.cancel();
            };

            let (result, ()) = futures_lite::future::zip(initialization, cancel_now).await;

            assert!(matches!(result, Err(AgentError::Cancelled)));
        });
    }

    fn assistant(content: Vec<ContentBlock>) -> Message {
        Message {
            role: Role::Assistant,
            content,
            ..Default::default()
        }
    }

    fn tool_result(is_error: bool) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: CALL_ID.into(),
                content: RESULT_BODY.into(),
                is_error,
                output_ref: None,
            }],
            ..Default::default()
        }
    }

    fn tool_call() -> Message {
        assistant(vec![ContentBlock::tool_use(
            CALL_ID,
            TOOL_NAME,
            json!({ "path": READ_PATH }),
        )])
    }

    fn block_text(message: &Message) -> String {
        message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn has_tool_blocks(messages: &[Message]) -> bool {
        messages.iter().any(|message| {
            message.content.iter().any(|block| {
                matches!(
                    block,
                    ContentBlock::ToolUse { .. } | ContentBlock::ToolResult { .. }
                )
            })
        })
    }

    #[test]
    fn a_flattened_transcript_keeps_its_evidence_and_loses_its_tool_blocks() {
        let mut messages = vec![
            Message::user(PROMPT.into()),
            tool_call(),
            tool_result(false),
        ];

        flatten_tool_blocks(&mut messages);

        assert!(!has_tool_blocks(&messages), "{NO_TOOL_BLOCKS}");
        assert_eq!(messages.len(), 3);
        let call = block_text(&messages[1]);
        assert!(
            call.starts_with(&format!("{TOOL_CALL_LINE} {TOOL_NAME}")),
            "{call}"
        );
        assert!(call.contains(READ_PATH), "{call}");
        assert!(block_text(&messages[2]).contains(RESULT_BODY));
    }

    #[test_case(false, TOOL_RESULT_LINE ; "a_result_is_flattened_as_a_result")]
    #[test_case(true, TOOL_ERROR_LINE ; "an_error_result_stays_marked_as_an_error")]
    fn a_flattened_result_names_the_tool_that_produced_it(is_error: bool, line: &str) {
        let mut messages = vec![tool_call(), tool_result(is_error)];

        flatten_tool_blocks(&mut messages);

        let text = block_text(&messages[1]);
        assert!(text.starts_with(&format!("{line} {TOOL_NAME}")), "{text}");
    }

    /// The owning call is gone once truncation drains the front of the
    /// transcript, leaving the result with nothing to name it.
    #[test]
    fn an_orphaned_result_flattens_under_a_placeholder_name() {
        let mut messages = vec![tool_result(false)];

        flatten_tool_blocks(&mut messages);

        let text = block_text(&messages[0]);
        assert!(
            text.starts_with(&format!("{TOOL_RESULT_LINE} {UNNAMED_TOOL}")),
            "{text}"
        );
    }

    #[test]
    fn thinking_and_images_survive_flattening() {
        let mut messages = vec![assistant(vec![
            ContentBlock::thinking(THOUGHT.into(), None),
            ContentBlock::Image {
                source: ImageSource::new(ImageMediaType::Png, IMAGE_DATA.into()),
            },
            ContentBlock::tool_use(CALL_ID, TOOL_NAME, json!({})),
        ])];

        flatten_tool_blocks(&mut messages);

        assert!(matches!(
            messages[0].content.as_slice(),
            [
                ContentBlock::Thinking { .. },
                ContentBlock::Image { .. },
                ContentBlock::Text { .. }
            ]
        ));
    }

    #[test]
    fn the_budgeted_transcript_is_flattened_and_needs_no_placeholder_result() {
        let history = vec![Message::user(PROMPT.into()), tool_call()];

        let messages = transcript_within_budget(
            &history,
            &Model::from_spec(CHAT_SPEC).unwrap(),
            INITIAL_TRANSCRIPT_PERCENT,
        );

        assert_eq!(messages.len(), history.len());
        assert!(!has_tool_blocks(&messages), "{NO_TOOL_BLOCKS}");
        assert!(
            !messages
                .iter()
                .any(|message| block_text(message).contains(crate::UNAVAILABLE_RESULT)),
            "a dangling call has nothing left to close"
        );
    }

    fn has_reasoning(messages: &[Message]) -> bool {
        messages
            .iter()
            .flat_map(|message| &message.content)
            .any(ContentBlock::is_thinking)
    }

    #[test_case(ContentBlock::Thinking {
        thinking: THOUGHT.into(),
        signature: None,
        duration_ms: None,
        interrupted: true,
        responses: None,
    } ; "unsigned_interrupted_thinking")]
    #[test_case(ContentBlock::thinking(THOUGHT.into(), Some(SIGNATURE.into())) ; "signed_thinking")]
    #[test_case(ContentBlock::RedactedThinking { data: REDACTED_DATA.into() } ; "redacted_thinking")]
    fn reasoning_never_reaches_the_evaluator(reasoning: ContentBlock) {
        let history = vec![
            Message::user(PROMPT.into()),
            assistant(vec![
                reasoning.clone(),
                ContentBlock::Text {
                    text: VERIFIED.into(),
                },
            ]),
            assistant(vec![reasoning]),
            Message::user(OTHER_PROMPT.into()),
        ];

        let messages = transcript_within_budget(
            &history,
            &Model::from_spec(CHAT_SPEC).unwrap(),
            INITIAL_TRANSCRIPT_PERCENT,
        );

        assert!(!has_reasoning(&messages), "{NO_REASONING}");
        assert_eq!(
            messages.len(),
            history.len() - 1,
            "{REASONING_ONLY_TURN_DROPPED}"
        );
        assert_eq!(block_text(&messages[1]), VERIFIED);
    }

    #[test]
    fn padding_survives_losing_its_reasoning() {
        let history = vec![
            Message::user(PROMPT.into()),
            Message {
                padding: true,
                ..assistant(vec![ContentBlock::thinking(
                    THOUGHT.into(),
                    Some(SIGNATURE.into()),
                )])
            },
            Message::user(OTHER_PROMPT.into()),
        ];

        let messages = transcript_within_budget(
            &history,
            &Model::from_spec(CHAT_SPEC).unwrap(),
            INITIAL_TRANSCRIPT_PERCENT,
        );

        assert!(!has_reasoning(&messages), "{NO_REASONING}");
        assert_eq!(messages.len(), history.len());
        assert!(messages[1].is_empty_padding());
    }

    #[test]
    fn a_stray_tool_call_does_not_discard_a_valid_answer() {
        let message = assistant(vec![
            ContentBlock::Text {
                text: ANSWER_JSON.into(),
            },
            ContentBlock::tool_use(CALL_ID, TOOL_NAME, json!({})),
        ]);

        let output = response_text(&message).unwrap();

        assert_eq!(parse_evaluation(&output).unwrap().verdict, GoalVerdict::Met);
    }

    #[test]
    fn a_tool_call_with_no_text_names_the_tool_it_tried() {
        let message = tool_call();

        let error = response_text(&message).unwrap_err();

        assert!(error.starts_with(TOOL_CALL_ERROR), "{error}");
        assert!(error.contains(TOOL_NAME), "{error}");
    }

    #[test]
    fn an_answerless_response_still_reports_no_text() {
        let message = assistant(vec![ContentBlock::Text { text: "  ".into() }]);

        assert_eq!(response_text(&message).unwrap_err(), NO_TEXT_ERROR);
    }
}
