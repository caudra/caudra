//! What every frontend owes its session's automation runtime, whichever loop drives the session:
//! the signals it sends and the state that keeps each one to a change, the busy period's tally
//! of each run, the rules a `next` delivery and its goal follow, the armings a session launches
//! with, and the controls its saves keep. The TUI and the SDK's headless loop each feed it from
//! their own events.

use std::time::Duration;

use caudra_automation::event::{
    ErrorKind, GoalFinishedDetail, GoalVerdict as FinishedVerdict, InputKind,
    MAX_LAST_RESPONSE_BYTES, SessionStatus, SessionView, StartedBy, TurnOutcome, cap_text,
};
use caudra_automation::host::DeliveryMode;
use caudra_automation::request::{
    AutomationError, DeliveryGate, GoalClaim, OutboxClaim, ProfileArming, SessionSignal,
};
use caudra_automation::snapshot::{ArmOrigin, OutboxItem, SettleBlocker, WaitReason};
use caudra_automation::untrusted::Untrusted;
use caudra_providers::{AutomationEventOrigin, ContentBlock, Message};
use caudra_storage::sessions::StoredAutomationControls;
use futures_lite::future;
use smol::Timer;
use tracing::{info, warn};

use super::busy::{BusyTracker, RunEnd};
use super::handle::AutomationHandle;
use super::manager::{AutomationRuntime, GOAL_ACTIVE, LaunchArming};
use super::store::stored_controls;
use crate::prompt::profile::SystemPromptProfile;
use crate::{AgentEvent, AgentMode, DoneReason, GoalHandle, GoalResult, GoalSnapshot, GoalVerdict};

const MILLIS_PER_SECOND: i64 = 1_000;
const BUILD_MODE: &str = "build";
const READ_ONLY_MODE: &str = "read_only";
const PLAN_MODE: &str = "plan";
const RESPONSE_SEPARATOR: &str = "\n";
/// How long a session waits for its firings to stop and its store to close before letting the
/// runtime go. Past this the next open marks the firings it cut off as interrupted.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
/// How the provider's user-facing errors read, lowercased, by kind; anything else is `other`.
const ERROR_KINDS: [(&str, ErrorKind); 6] = [
    ("authentication failed", ErrorKind::Auth),
    ("rate limited", ErrorKind::RateLimit),
    ("overloaded", ErrorKind::Overloaded),
    ("timed out", ErrorKind::Timeout),
    ("connection error", ErrorKind::Network),
    ("request error", ErrorKind::Network),
];

/// What the session waits on a person for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputWait {
    pub input: InputKind,
    /// The tool a permission prompt asks for.
    pub tool: Option<String>,
}

/// What a session last told its runtime, so each signal goes out on a change only, and the
/// busy period behind the next `idle`.
#[derive(Default)]
pub struct SessionSignals {
    pub busy: BusyTracker,
    /// What keeps the session from settling, as last reported; `None` once the runtime heard it
    /// settled.
    blockers: Option<Vec<SettleBlocker>>,
    input: Option<InputWait>,
    facts: Option<SessionView>,
    /// The active goal as last seen, for reporting a goal an error has already cleared.
    goal: Option<GoalSnapshot>,
    /// The main-agent run in flight, as far as its idle report needs it.
    run: Option<RunTally>,
}

struct RunTally {
    cost_at_start: Option<f64>,
    last_response: String,
}

impl SessionSignals {
    /// `facts` are what the runtime started from.
    pub fn new(facts: SessionView) -> Self {
        Self {
            facts: Some(facts),
            ..Self::default()
        }
    }

    /// What the session's blockers owe the runtime. The busy-to-settled edge reports the period
    /// that ended, and a busy session reports each new set of blockers. A settle with no run
    /// behind it only clears the blockers: an `idle` event always follows a run.
    pub fn settle(&mut self, blockers: Vec<SettleBlocker>, now: i64) -> Option<SessionSignal> {
        if !blockers.is_empty() {
            if self.blockers.as_ref() == Some(&blockers) {
                return None;
            }
            self.blockers = Some(blockers.clone());
            return Some(SessionSignal::Busy { blockers });
        }
        self.blockers.take()?;
        Some(match self.busy.settle(now) {
            Some(idle) => SessionSignal::Settled(Box::new(idle)),
            None => SessionSignal::Busy {
                blockers: Vec::new(),
            },
        })
    }

    /// Whether [`Self::settle`] would tell the runtime anything about `blockers`.
    pub fn changes(&self, blockers: &[SettleBlocker]) -> bool {
        match &self.blockers {
            Some(reported) => reported != blockers,
            None => !blockers.is_empty(),
        }
    }

    /// Whether the runtime heard the session settle, so a claim never joins the period that
    /// settle closed.
    pub fn settled(&self) -> bool {
        self.blockers.is_none()
    }

    /// What the session's input wait owes the runtime: each new wait once, and its end.
    pub fn wait(&mut self, wait: Option<InputWait>) -> Option<SessionSignal> {
        if wait == self.input {
            return None;
        }
        let signal = wait
            .as_ref()
            .map_or(SessionSignal::InputResolved, needs_input);
        self.input = wait;
        Some(signal)
    }

    /// The wait last reported.
    pub fn input(&self) -> Option<&InputWait> {
        self.input.as_ref()
    }

    /// When `status` began, in unix seconds: when the reported facts last changed to it, or now.
    pub fn status_since(&self, status: SessionStatus, now: i64) -> i64 {
        match &self.facts {
            Some(facts) if facts.status == status => facts.status_since,
            _ => now / MILLIS_PER_SECOND,
        }
    }

    /// `facts`, when they differ from the facts last reported.
    pub fn facts(&mut self, facts: SessionView) -> Option<SessionSignal> {
        if self.facts.as_ref() == Some(&facts) {
            return None;
        }
        self.facts = Some(facts.clone());
        Some(SessionSignal::Facts(Box::new(facts)))
    }

    /// Keeps the active goal's latest tally; no goal keeps the last one seen.
    pub fn observe_goal(&mut self, goal: Option<GoalSnapshot>) {
        if goal.is_some() {
            self.goal = goal;
        }
    }

    /// A main-agent run started, with the session's running spend in USD at its start.
    pub fn run_started(&mut self, started_by: StartedBy, now: i64, cost: Option<f64>) {
        self.busy.run_started(started_by, now);
        self.run = Some(RunTally {
            cost_at_start: cost,
            last_response: String::new(),
        });
    }

    pub fn running(&self) -> bool {
        self.run.is_some()
    }

    /// A run of the period took a message an automation queued.
    pub fn injected(&mut self, origin: &AutomationEventOrigin) {
        self.busy.injected(origin);
    }

    /// A turn that called no tool is the run's response so far.
    pub fn turn_complete(&mut self, message: &Message) {
        let Some(run) = &mut self.run else {
            return;
        };
        if message.has_tool_calls() {
            return;
        }
        let text = message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(RESPONSE_SEPARATOR);
        run.last_response = cap_text(text, MAX_LAST_RESPONSE_BYTES).0;
    }

    /// Ends the run in flight, `cost` being the session's running spend now, and says so.
    pub fn run_ended(
        &mut self,
        outcome: TurnOutcome,
        error: Option<String>,
        cost: Option<f64>,
    ) -> SessionSignal {
        let run = self.run.take();
        self.busy.run_ended(RunEnd {
            outcome,
            error_kind: error.as_deref().map(error_kind),
            error,
            cost: run
                .as_ref()
                .and_then(|run| spent_since(run.cost_at_start, cost)),
            last_response: run.map(|run| run.last_response).unwrap_or_default(),
        });
        SessionSignal::RunEnded(outcome)
    }

    /// The agent clears the goal before it reports the error, so the goal's tally comes from
    /// the last snapshot seen of it.
    pub fn goal_cleared(&self, condition: &str, message: &str) -> SessionSignal {
        let goal = self
            .goal
            .as_ref()
            .filter(|goal| *goal.condition == *condition);
        SessionSignal::GoalFinished(Box::new(GoalFinishedDetail {
            verdict: FinishedVerdict::Cleared,
            condition: condition.to_owned(),
            reason: Untrusted::text(message),
            evaluations: goal.map_or(0, |goal| goal.evaluations),
            duration_s: goal.map_or(0, |goal| goal.elapsed().as_secs()),
            cost: goal.and_then(|goal| goal.cost),
        }))
    }
}

pub fn goal_finished(result: &GoalResult) -> SessionSignal {
    SessionSignal::GoalFinished(Box::new(GoalFinishedDetail {
        verdict: match result.verdict {
            GoalVerdict::Met => FinishedVerdict::Met,
            GoalVerdict::Impossible | GoalVerdict::NotMet => FinishedVerdict::Impossible,
        },
        condition: result.condition.to_string(),
        reason: Untrusted::text(result.reason.to_string()),
        evaluations: result.evaluations,
        duration_s: result.duration.as_secs(),
        cost: result.cost,
    }))
}

/// Sets the goal a claimed item asks for. An active goal stays unless the item asked to replace
/// it; the refusal comes back for the frontend to show, and the turn starts either way.
pub fn set_claimed_goal(goal: &GoalHandle, claim: &GoalClaim) -> Option<String> {
    if goal.snapshot().is_some() && !claim.replace {
        return Some(GOAL_ACTIVE.to_owned());
    }
    if let Err(error) = goal.set(&claim.condition) {
        return Some(error.to_string());
    }
    if let Some(limit) = claim.continuation_limit {
        goal.set_continuation_limit(limit);
    }
    None
}

/// How a run ended, for a main-agent terminal event.
pub fn run_end(event: &AgentEvent) -> Option<(TurnOutcome, Option<String>)> {
    match event {
        AgentEvent::Done { reason, .. } => Some((
            match reason {
                DoneReason::EndTurn | DoneReason::MaxTokens => TurnOutcome::Completed,
                DoneReason::MaxTurns => TurnOutcome::MaxTurns,
                DoneReason::Cancelled => TurnOutcome::Cancelled,
            },
            None,
        )),
        AgentEvent::Error { message } => Some((TurnOutcome::Error, Some(message.clone()))),
        _ => None,
    }
}

pub fn needs_input(wait: &InputWait) -> SessionSignal {
    SessionSignal::NeedsInput {
        input: wait.input,
        tool: wait.tool.clone(),
    }
}

pub fn mode_name(mode: &AgentMode) -> &'static str {
    match mode {
        AgentMode::Build => BUILD_MODE,
        AgentMode::ReadOnly => READ_ONLY_MODE,
        AgentMode::Plan(_) | AgentMode::RemotePlan(_) => PLAN_MODE,
    }
}

pub fn error_kind(message: &str) -> ErrorKind {
    let message = message.to_lowercase();
    ERROR_KINDS
        .iter()
        .find(|(label, _)| message.contains(label))
        .map_or(ErrorKind::Other, |&(_, kind)| kind)
}

/// What the session spent since `start`, both running totals in USD.
pub fn spent_since(start: Option<f64>, now: Option<f64>) -> Option<f64> {
    Some(now? - start.unwrap_or_default())
}

/// Who a wake's run reads as started by: a wake for workflow results or for background tasks
/// alone names its source, and anything else is the mailbox.
pub fn wake_origin(woken: bool, workflow_messages: usize, background_messages: usize) -> StartedBy {
    match (woken, workflow_messages, background_messages) {
        (false, 1.., 0) => StartedBy::Workflow,
        (false, 0, 1..) => StartedBy::Background,
        _ => StartedBy::Mailbox,
    }
}

/// Whether nothing that still holds keeps `item` from a turn that starts now. A wait on the
/// session itself is stale by the time the caller asks, and the claim checks it again.
pub fn deliverable(item: &OutboxItem, now: i64) -> bool {
    match item.wait {
        Some(WaitReason::Paused | WaitReason::UnattendedCap { .. }) => false,
        Some(WaitReason::TurnRateFull { until } | WaitReason::Backoff { until }) => now >= until,
        _ => true,
    }
}

/// Whether an outbox item of `handle`'s runtime could start a turn now.
pub fn delivery_due(handle: &AutomationHandle) -> bool {
    let now = handle.now_ms();
    handle
        .state()
        .outbox
        .iter()
        .any(|item| deliverable(item, now))
}

/// Claims the next `next` item for a turn. The runtime records it delivered before answering,
/// so the caller has already checked everything that could stop that turn, and only calls
/// once the runtime heard the session settle with no prompt queued.
pub async fn claim_next(handle: &AutomationHandle) -> Result<Option<OutboxClaim>, AutomationError> {
    handle
        .claim(DeliveryGate {
            mode: DeliveryMode::Next,
            settled: true,
            prompt_queued: false,
            modal_open: false,
            peers_first: false,
            now: handle.now_ms(),
        })
        .await
}

/// What `SessionMeta.automations` keeps while `handle`'s runtime serves the session. The mirror
/// outlives the runtime, so the last save keeps its final counters.
pub fn saved_controls(handle: &AutomationHandle) -> StoredAutomationControls {
    let state = handle.state();
    stored_controls(&state.session.controls, state.armed_names())
}

/// The profile's `automations:` first, then the CLI's, which the runtime lets override a
/// profile entry's args.
pub fn launch_armings(
    profile: Option<&SystemPromptProfile>,
    cli: Vec<ProfileArming>,
) -> Vec<LaunchArming> {
    profile_armings(profile)
        .into_iter()
        .map(|arming| LaunchArming {
            arming,
            origin: ArmOrigin::Profile,
        })
        .chain(cli.into_iter().map(|arming| LaunchArming {
            arming,
            origin: ArmOrigin::Cli,
        }))
        .collect()
}

pub fn profile_armings(profile: Option<&SystemPromptProfile>) -> Vec<ProfileArming> {
    profile
        .map(SystemPromptProfile::automations)
        .unwrap_or_default()
        .iter()
        .map(|automation| ProfileArming {
            name: automation.name.clone(),
            args: automation.args.clone(),
        })
        .collect()
}

/// Stops every firing and waits, for a bounded time, for the store to close, so nothing of the
/// session's runtime survives into the next one.
pub async fn stop_runtime(runtime: AutomationRuntime) {
    let session_id = runtime.handle().session_id();
    info!(%session_id, "automation runtime shutting down");
    let closed = future::or(
        async {
            runtime.shutdown().await;
            true
        },
        async {
            Timer::after(SHUTDOWN_TIMEOUT).await;
            false
        },
    )
    .await;
    if !closed {
        warn!(
            %session_id,
            timeout = ?SHUTDOWN_TIMEOUT,
            "automation runtime did not close in time, abandoning it"
        );
    }
}

#[cfg(test)]
mod tests {
    use caudra_automation::host::ActionKind;
    use caudra_providers::AgentError;
    use test_case::test_case;

    use super::*;

    const NOW: i64 = 1_790_000_000_000;
    const LATER: i64 = NOW + MILLIS_PER_SECOND;
    const AUTOMATION: &str = "courier";
    const FIRE_ID: &str = "fire-1";
    const TOOL: &str = "bash";
    const DETAIL: &str = "try again";
    const SETTLED_ONCE: &str = "a settled session must not report its settle again";
    const NOT_SETTLED: &str = "the settle after a run must report the period it closes";

    fn busy(blockers: &[SettleBlocker]) -> Option<SessionSignal> {
        Some(SessionSignal::Busy {
            blockers: blockers.to_vec(),
        })
    }

    #[test]
    fn a_busy_session_reports_each_new_set_of_blockers_once() {
        let mut signals = SessionSignals::default();
        let working = [SettleBlocker::Busy];
        let queued = [SettleBlocker::Busy, SettleBlocker::PromptQueued];

        assert_eq!(signals.settle(working.to_vec(), NOW), busy(&working));
        assert!(!signals.changes(&working));
        assert_eq!(signals.settle(working.to_vec(), NOW), None);
        assert!(signals.changes(&queued));
        assert_eq!(signals.settle(queued.to_vec(), NOW), busy(&queued));
    }

    #[test]
    fn the_settle_after_a_run_reports_its_period_once() {
        let mut signals = SessionSignals::default();
        signals.busy.run_started(StartedBy::User, NOW);
        signals.settle(vec![SettleBlocker::Busy], NOW);
        assert!(signals.changes(&[]));

        let Some(SessionSignal::Settled(idle)) = signals.settle(Vec::new(), LATER) else {
            panic!("{NOT_SETTLED}");
        };
        assert_eq!((idle.started_by, idle.runs), (StartedBy::User, 1));
        assert!(signals.settled());
        assert_eq!(signals.settle(Vec::new(), LATER), None, "{SETTLED_ONCE}");
    }

    /// An `idle` event always follows a run, so a session that only waited reports no period.
    #[test]
    fn a_settle_with_no_run_behind_it_only_clears_the_blockers() {
        let mut signals = SessionSignals::default();
        assert!(!signals.changes(&[]));
        assert_eq!(signals.settle(Vec::new(), NOW), None, "{SETTLED_ONCE}");
        signals.settle(vec![SettleBlocker::MailboxWake], NOW);
        assert!(!signals.settled());

        assert_eq!(signals.settle(Vec::new(), LATER), busy(&[]));
    }

    #[test]
    fn each_wait_is_reported_once_and_so_is_its_end() {
        let mut signals = SessionSignals::default();
        let question = InputWait {
            input: InputKind::Question,
            tool: None,
        };
        let permission = InputWait {
            input: InputKind::Permission,
            tool: Some(TOOL.into()),
        };

        assert_eq!(
            signals.wait(Some(question.clone())),
            Some(SessionSignal::NeedsInput {
                input: InputKind::Question,
                tool: None,
            })
        );
        assert_eq!(signals.wait(Some(question)), None);
        assert_eq!(
            signals.wait(Some(permission)),
            Some(SessionSignal::NeedsInput {
                input: InputKind::Permission,
                tool: Some(TOOL.into()),
            })
        );
        assert_eq!(signals.wait(None), Some(SessionSignal::InputResolved));
        assert_eq!(signals.wait(None), None);
    }

    #[test_case(None, true ; "nothing_holds_it")]
    #[test_case(Some(WaitReason::Busy), true ; "a_stale_session_wait")]
    #[test_case(Some(WaitReason::Paused), false ; "paused")]
    #[test_case(Some(WaitReason::UnattendedCap { cap: 1 }), false ; "unattended_cap")]
    #[test_case(Some(WaitReason::TurnRateFull { until: LATER }), false ; "turn_rate_full")]
    #[test_case(Some(WaitReason::TurnRateFull { until: NOW }), true ; "turn_rate_freed")]
    #[test_case(Some(WaitReason::Backoff { until: LATER }), false ; "backing_off")]
    #[test_case(Some(WaitReason::Backoff { until: NOW }), true ; "backoff_over")]
    fn an_item_is_deliverable_once_nothing_that_still_holds_waits(
        wait: Option<WaitReason>,
        expected: bool,
    ) {
        let item = OutboxItem {
            automation: AUTOMATION.into(),
            fire_id: FIRE_ID.into(),
            seq: 0,
            kind: ActionKind::Message,
            summary: String::new(),
            delivery: DeliveryMode::Next,
            queued_at: NOW,
            expires_at: None,
            wait,
        };
        assert_eq!(deliverable(&item, NOW), expected);
    }

    #[test_case(AgentError::api(401, DETAIL), ErrorKind::Auth ; "auth")]
    #[test_case(AgentError::api(429, DETAIL), ErrorKind::RateLimit ; "rate_limit")]
    #[test_case(AgentError::api(529, DETAIL), ErrorKind::Overloaded ; "overloaded")]
    #[test_case(AgentError::Timeout { secs: 1 }, ErrorKind::Timeout ; "timeout")]
    #[test_case(AgentError::Channel, ErrorKind::Other ; "other")]
    fn a_run_error_is_classified_by_the_message_the_session_shows(
        error: AgentError,
        expected: ErrorKind,
    ) {
        assert_eq!(error_kind(&error.user_message()), expected);
    }
}
