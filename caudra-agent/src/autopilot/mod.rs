//! Autopilot: a delegate model that answers for the user while they are away,
//! within the budget config sets for each engagement.

mod budget;
mod context;
mod delegate;
pub(crate) mod question;
pub(crate) mod review;
mod wire;

use std::collections::VecDeque;
use std::mem;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_lock::Semaphore;
use caudra_config::AutopilotConfig;
use caudra_providers::{Billing, Message};
use caudra_storage::autopilot::{AutopilotEnd, StoredAutopilot, StoredAutopilotDecision};
use caudra_storage::sessions::SessionMeta;
use tracing::warn;

use crate::agent::{ResolvedEvaluator, last_announced, standing_notice as restated_notice};
use crate::automation::clock::{Clock, SystemClock, Wake};
use crate::prompt::{AUTOPILOT_ENDED_PROMPT, AUTOPILOT_ENGAGED_PROMPT, AUTOPILOT_MARKER};
use crate::types::AutopilotEvent;
use budget::{Budget, ReviewStreak};

pub use budget::{MAX_REVIEW_STREAK, retry_delay};
#[cfg(test)]
pub(crate) use context::MAX_CONTEXT_BYTES;
pub(crate) use context::mode_label;
pub use context::{AutopilotSnapshot, ChatSession, DecisionRequest, RECENT_DECISIONS};
#[cfg(test)]
pub(crate) use delegate::tests::{GatedProvider, ScriptedProvider, Step, engaged_handle};
pub use delegate::{DecisionOrigin, Delegate, DelegateOutcome};
pub use wire::{
    ActionName, AutopilotDecision, DecisionKind, DelegateAction, DelegateOutput,
    StoredDecisionError, WireError,
};

/// Delegate calls one session may have in flight.
pub const MAX_CONCURRENT_DECISIONS: usize = 4;

/// One session's autopilot. Clones share it, so the App keeps it across
/// respawns and hands it to every root permission manager it builds. It holds
/// no event sender: whatever it announces is returned for the caller to send.
#[derive(Clone)]
pub struct AutopilotHandle {
    state: Arc<Mutex<State>>,
    permits: Arc<Semaphore>,
    clock: Arc<dyn Clock>,
}

struct State {
    engaged: bool,
    owns_goal: bool,
    /// Unix milliseconds.
    engaged_at_ms: Option<u64>,
    budget: Budget,
    last_end: Option<AutopilotEnd>,
    next_seq: u32,
    /// This engagement's newest decisions, oldest first.
    recent: VecDeque<AutopilotDecision>,
    snapshot: Option<AutopilotSnapshot>,
    question_claimed: bool,
    plan_claimed: bool,
    review_streak: ReviewStreak,
    /// The goal generation whose second opinion was asked for.
    second_opinion_generation: Option<u64>,
    /// The delegate's model, kept while it is exactly bound.
    delegate_model: Option<ResolvedEvaluator>,
    /// The `[autopilot]` config as it stands now. Process memory only: the
    /// frontend sets it at startup and on each reload, and it is never stored.
    limits: AutopilotConfig,
}

impl Default for AutopilotHandle {
    fn default() -> Self {
        Self::new(Arc::new(SystemClock::new()))
    }
}

impl AutopilotHandle {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        let state = State {
            engaged: false,
            owns_goal: false,
            engaged_at_ms: None,
            budget: Budget::new(clock.monotonic()),
            last_end: None,
            next_seq: 0,
            recent: VecDeque::new(),
            snapshot: None,
            question_claimed: false,
            plan_claimed: false,
            review_streak: ReviewStreak::default(),
            second_opinion_generation: None,
            delegate_model: None,
            limits: AutopilotConfig::default(),
        };
        Self {
            state: Arc::new(Mutex::new(state)),
            permits: Arc::new(Semaphore::new(MAX_CONCURRENT_DECISIONS)),
            clock,
        }
    }

    /// Reopens the engagement a session stored, still engaged if it was, with
    /// the budget it had used. `decisions` are the session's, oldest first:
    /// numbering continues after them, and the ones this engagement made are
    /// the recent ones again.
    pub fn from_meta(
        meta: &SessionMeta,
        decisions: Vec<StoredAutopilotDecision>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let handle = Self::new(clock);
        let now = handle.clock.monotonic();
        let mut state = handle.lock();
        if let Some(stored) = meta.autopilot.as_deref() {
            state.engaged = stored.engaged;
            state.owns_goal = stored.owns_goal;
            state.engaged_at_ms = stored.engaged_at_ms;
            state.budget = Budget::restored(stored, now);
            state.last_end = stored.last_end.clone();
        }
        state.next_seq = decisions
            .iter()
            .map(|decision| decision.seq.saturating_add(1))
            .max()
            .map_or(0, |next| u32::try_from(next).unwrap_or(u32::MAX));
        let since = state.engaged_at_ms.unwrap_or_default();
        state.recent = decisions
            .into_iter()
            .filter(|decision| decision.at_ms >= since)
            .filter_map(|decision| {
                AutopilotDecision::try_from(decision)
                    .inspect_err(|error| warn!(%error, "skipping an unreadable autopilot decision"))
                    .ok()
            })
            .collect();
        trim_recent(&mut state.recent);
        drop(state);
        handle
    }

    /// The engagement as [`Self::from_meta`] reads it back, or `None` when
    /// autopilot was never engaged here.
    pub fn stored(&self) -> Option<StoredAutopilot> {
        let now = self.clock.monotonic();
        let state = self.lock();
        if !state.engaged && state.last_end.is_none() {
            return None;
        }
        let mut stored = StoredAutopilot {
            engaged: state.engaged,
            owns_goal: state.owns_goal,
            engaged_at_ms: state.engaged_at_ms,
            continuations: 0,
            cost_usd: 0.0,
            subscription_cost_usd: 0.0,
            elapsed_ms: 0,
            last_end: state.last_end.clone(),
        };
        state.budget.store(&mut stored, now);
        Some(stored)
    }

    /// Starts an engagement with a fresh budget. Already engaged, it only
    /// takes ownership of a goal it set, and announces nothing.
    pub fn engage(&self, owns_goal: bool) -> Option<AutopilotEvent> {
        let now = self.clock.monotonic();
        let engaged_at_ms = u64::try_from(self.clock.now_ms()).ok();
        let mut state = self.lock();
        if state.engaged {
            state.owns_goal |= owns_goal;
            return None;
        }
        state.engaged = true;
        state.owns_goal = owns_goal;
        state.engaged_at_ms = engaged_at_ms;
        state.budget = Budget::new(now);
        state.last_end = None;
        state.recent.clear();
        state.question_claimed = false;
        state.plan_claimed = false;
        state.review_streak.reset();
        Some(AutopilotEvent::Engaged)
    }

    /// Ends the engagement for `reason`. Not engaged, it does nothing.
    pub fn end(&self, reason: AutopilotEnd) -> Option<AutopilotEvent> {
        let now = self.clock.monotonic();
        let mut state = self.lock();
        state
            .engaged
            .then(|| end_engagement(&mut state, reason, now))
    }

    pub fn is_engaged(&self) -> bool {
        self.lock().engaged
    }

    /// Whether the active goal is the one autopilot set.
    pub fn owns_goal(&self) -> bool {
        self.lock().owns_goal
    }

    /// The owned goal was replaced or cleared by hand. Autopilot stays.
    pub fn drop_goal_ownership(&self) {
        self.lock().owns_goal = false;
    }

    /// Counts a continuation, or ends the engagement on the limit it reached.
    /// Not engaged, no budget applies.
    pub fn try_continue(&self, limits: &AutopilotConfig) -> Result<(), AutopilotEvent> {
        let now = self.clock.monotonic();
        let mut state = self.lock();
        if !state.engaged {
            return Ok(());
        }
        state
            .budget
            .try_continue(limits, now)
            .map_err(|kind| end_engagement(&mut state, AutopilotEnd::Budget { kind }, now))
    }

    /// Adds spend to the engagement, and ends it when the cost or time limit
    /// is now reached.
    pub fn record_spend(
        &self,
        cost: Option<f64>,
        billing: Billing,
        limits: &AutopilotConfig,
    ) -> Option<AutopilotEvent> {
        let now = self.clock.monotonic();
        let mut state = self.lock();
        if !state.engaged {
            return None;
        }
        state.budget.add_spend(cost, billing);
        let kind = state.budget.overspent(limits, now)?;
        Some(end_engagement(
            &mut state,
            AutopilotEnd::Budget { kind },
            now,
        ))
    }

    /// The user spoke. It breaks the review streak and nothing else: an
    /// engaged autopilot carries on around it, and an ended one stays ended.
    pub fn human_input(&self) {
        self.lock().review_streak.reset();
    }

    /// Counts a review the delegate continued, or `false` when the streak is
    /// full and the review escalates instead.
    pub fn review_continue(&self) -> bool {
        self.lock().review_streak.try_continue()
    }

    pub fn set_limits(&self, limits: AutopilotConfig) {
        self.lock().limits = limits;
    }

    pub fn limits(&self) -> AutopilotConfig {
        self.lock().limits.clone()
    }

    /// Completes once `delay` from now has passed on the engagement's clock.
    pub fn sleep(&self, delay: Duration) -> Wake {
        self.clock
            .sleep_until(self.clock.monotonic().saturating_add(delay))
    }

    pub fn observe(&self, snapshot: AutopilotSnapshot) {
        self.lock().snapshot = Some(snapshot);
    }

    pub fn snapshot(&self) -> Option<AutopilotSnapshot> {
        self.lock().snapshot.clone()
    }

    /// Takes the waiting question for whoever answers first, `false` when it
    /// was already taken.
    pub fn claim_question(&self) -> bool {
        !mem::replace(&mut self.lock().question_claimed, true)
    }

    pub fn question_claimed(&self) -> bool {
        self.lock().question_claimed
    }

    pub fn reset_question_claim(&self) {
        self.lock().question_claimed = false;
    }

    /// Takes the waiting plan for whoever answers first, `false` when it was
    /// already taken.
    pub fn claim_plan(&self) -> bool {
        !mem::replace(&mut self.lock().plan_claimed, true)
    }

    pub fn plan_claimed(&self) -> bool {
        self.lock().plan_claimed
    }

    pub fn reset_plan_claim(&self) {
        self.lock().plan_claimed = false;
    }

    pub fn next_seq(&self) -> u32 {
        let mut state = self.lock();
        let seq = state.next_seq;
        state.next_seq = seq.saturating_add(1);
        seq
    }

    /// Unix milliseconds, as a decision is stamped.
    pub fn now_ms(&self) -> u64 {
        u64::try_from(self.clock.now_ms()).unwrap_or_default()
    }

    pub fn record(&self, decision: AutopilotDecision) -> AutopilotEvent {
        let mut state = self.lock();
        state.recent.push_back(decision.clone());
        trim_recent(&mut state.recent);
        AutopilotEvent::Decided(Box::new(decision))
    }

    /// This engagement's newest decisions, oldest first.
    pub fn recent_decisions(&self) -> Vec<AutopilotDecision> {
        self.lock().recent.iter().cloned().collect()
    }

    /// Takes the one second opinion goal `generation` gets, `false` once it
    /// was taken.
    pub fn take_second_opinion(&self, generation: u64) -> bool {
        self.lock()
            .second_opinion_generation
            .replace(generation)
            .is_none_or(|taken| taken != generation)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }
}

fn end_engagement(state: &mut State, reason: AutopilotEnd, now: Duration) -> AutopilotEvent {
    state.engaged = false;
    state.owns_goal = false;
    state.budget.pause(now);
    state.last_end = Some(reason.clone());
    AutopilotEvent::Ended { reason }
}

fn trim_recent(recent: &mut VecDeque<AutopilotDecision>) {
    let excess = recent.len().saturating_sub(RECENT_DECISIONS);
    recent.drain(..excess);
}

/// The `# Autopilot` block a turn should carry, `None` when the transcript
/// already states it. Engaged, that is the engaged text. Not engaged, it is
/// the ended text, and only to withdraw an engaged text still in force.
pub fn standing_notice(history: &[Message], engaged: bool) -> Option<Message> {
    let text = if engaged {
        Some(AUTOPILOT_ENGAGED_PROMPT)
    } else {
        last_announced(history, &[AUTOPILOT_MARKER]).map(|_| AUTOPILOT_ENDED_PROMPT)
    };
    restated_notice(history, AUTOPILOT_MARKER, text)
}

#[cfg(test)]
mod tests {
    use caudra_providers::{AutopilotMessageKind, AutopilotOrigin};
    use caudra_storage::autopilot::{AutopilotBudget, AutopilotStopper};
    use caudra_storage::sessions::PermissionMode;
    use serde_json::{Value, json};
    use test_case::test_case;

    use super::*;
    use crate::AgentEvent;
    use crate::automation::clock::FakeClock;
    use context::tests::idle_chat;

    const START_MS: i64 = 1_790_000_000_000;
    const MINUTE: Duration = Duration::from_secs(60);
    const MAX_CONTINUATIONS: u32 = 2;
    const MAX_COST: f64 = 1.0;
    const MAX_MINUTES: u32 = 10;
    const ERROR_RETRIES: u32 = 5;
    const SPEND: f64 = 0.25;
    const RAN: Duration = Duration::from_secs(3 * 60);
    const STORED_DECISIONS: u64 = 3;
    const GENERATION: u64 = 7;
    const SUBJECT: &str = "shell: cargo test";
    const REASON: &str = "the user asked for the tests";
    const PROMPT: &str = "fix the parser";
    const STOPPED: AutopilotEnd = AutopilotEnd::Stopped {
        by: AutopilotStopper::Command,
    };
    const ENDED_JSON: &str =
        r#"{"type":"autopilot","event":"ended","reason":{"reason":"budget","kind":"cost"}}"#;
    const ENGAGED_JSON: &str = r#"{"type":"autopilot","event":"engaged"}"#;
    const DECIDING_JSON: &str = r#"{"type":"autopilot","event":"deciding","seq":3,"kind":"permission","subject":"shell: cargo test"}"#;
    const DECIDED_EVENT: &str = "decided";
    const ANNOUNCED_ONCE: &str = "an engagement must be announced once";
    const ENDED_REPLACES: &str = "the ended text must withdraw an engaged one in force";
    const NOTHING_TO_WITHDRAW: &str = "with nothing in force there is nothing to end";
    const DELEGATE_TEXT_IGNORED: &str = "the delegate's own text must not pass for the notice";
    const HUMAN_KEEPS_STATE: &str = "human input must neither engage nor disengage autopilot";
    const RESTORE_KEEPS_BUDGET: &str = "a restored engagement must keep the budget it used";
    const SEQ_CONTINUES: &str = "numbering must continue after the stored decisions";
    const OWNERSHIP_ENDS: &str = "an ended engagement owns no goal";
    const BUDGET_ENDS: &str = "a budget that runs out must end the engagement";

    fn handle() -> (Arc<FakeClock>, AutopilotHandle) {
        let clock = FakeClock::new(START_MS);
        (Arc::clone(&clock), AutopilotHandle::new(clock))
    }

    fn limits() -> AutopilotConfig {
        AutopilotConfig {
            max_continuations: Some(MAX_CONTINUATIONS),
            max_cost: Some(MAX_COST),
            max_minutes: Some(MAX_MINUTES),
            error_retries: ERROR_RETRIES,
        }
    }

    fn decision(seq: u32) -> AutopilotDecision {
        AutopilotDecision {
            seq,
            at_ms: u64::try_from(START_MS).unwrap(),
            kind: DecisionKind::Permission,
            action: ActionName::Allow,
            subject: SUBJECT.into(),
            reason: REASON.into(),
            agent: None,
            tool_use_id: None,
            history_len: 0,
            model: None,
            usage: None,
            cost: None,
        }
    }

    fn budget_end(kind: AutopilotBudget) -> AutopilotEvent {
        AutopilotEvent::Ended {
            reason: AutopilotEnd::Budget { kind },
        }
    }

    fn engaged_notice() -> Message {
        Message::observation(AUTOPILOT_ENGAGED_PROMPT.into())
    }

    fn ended_notice() -> Message {
        Message::observation(AUTOPILOT_ENDED_PROMPT.into())
    }

    fn delegate_reply() -> Message {
        Message::autopilot_observation(
            AUTOPILOT_ENGAGED_PROMPT.into(),
            AutopilotOrigin {
                seq: 0,
                kind: AutopilotMessageKind::Reply,
            },
        )
    }

    #[test]
    fn an_engagement_is_announced_and_ended_once() {
        let (_, handle) = handle();

        assert_eq!(handle.engage(false), Some(AutopilotEvent::Engaged));
        assert_eq!(handle.engage(false), None);
        assert!(handle.is_engaged());
        assert_eq!(
            handle.end(STOPPED),
            Some(AutopilotEvent::Ended { reason: STOPPED })
        );
        assert_eq!(handle.end(STOPPED), None);

        assert!(!handle.is_engaged());
        assert_eq!(handle.stored().unwrap().last_end, Some(STOPPED));
    }

    #[test]
    fn engaging_again_with_a_goal_only_takes_ownership() {
        let (_, handle) = handle();
        handle.engage(false);
        handle.try_continue(&limits()).unwrap();

        assert_eq!(handle.engage(true), None);

        assert!(handle.owns_goal());
        assert_eq!(handle.stored().unwrap().continuations, 1);
    }

    #[test]
    fn ownership_drops_by_hand_and_with_the_engagement() {
        let (_, handle) = handle();
        handle.engage(true);
        handle.drop_goal_ownership();
        assert!(!handle.owns_goal());
        assert!(handle.is_engaged());

        handle.engage(true);
        handle.end(STOPPED);

        assert!(!handle.owns_goal(), "{OWNERSHIP_ENDS}");
    }

    #[test_case(true ; "engaged")]
    #[test_case(false ; "not_engaged")]
    fn human_input_breaks_the_review_streak_and_nothing_else(engaged: bool) {
        let (_, handle) = handle();
        if engaged {
            handle.engage(false);
        }
        for _ in 0..MAX_REVIEW_STREAK {
            assert!(handle.review_continue());
        }
        assert!(!handle.review_continue());

        handle.human_input();

        assert!(handle.review_continue());
        assert_eq!(handle.is_engaged(), engaged, "{HUMAN_KEEPS_STATE}");
    }

    #[test]
    fn continuations_run_out_through_the_handle() {
        let (_, handle) = handle();
        handle.engage(false);
        for _ in 0..MAX_CONTINUATIONS {
            assert_eq!(handle.try_continue(&limits()), Ok(()));
        }

        assert_eq!(
            handle.try_continue(&limits()),
            Err(budget_end(AutopilotBudget::Continuations))
        );
        assert!(!handle.is_engaged(), "{BUDGET_ENDS}");
    }

    #[test]
    fn cost_runs_out_through_the_handle() {
        let (_, handle) = handle();
        handle.engage(false);

        assert_eq!(
            handle.record_spend(Some(SPEND), Billing::Api, &limits()),
            None
        );
        assert_eq!(
            handle.record_spend(Some(MAX_COST), Billing::Api, &limits()),
            Some(budget_end(AutopilotBudget::Cost))
        );
        assert!(!handle.is_engaged(), "{BUDGET_ENDS}");
    }

    #[test_case(true ; "at_the_next_continuation")]
    #[test_case(false ; "at_the_next_spend")]
    fn time_runs_out_through_the_handle(continuing: bool) {
        let (clock, handle) = handle();
        handle.engage(false);
        clock.advance(MINUTE * MAX_MINUTES);

        let ended = if continuing {
            handle.try_continue(&limits()).err()
        } else {
            handle.record_spend(None, Billing::Api, &limits())
        };

        assert_eq!(ended, Some(budget_end(AutopilotBudget::Time)));
        assert!(!handle.is_engaged(), "{BUDGET_ENDS}");
    }

    #[test]
    fn no_budget_applies_while_disengaged() {
        let (_, handle) = handle();

        assert_eq!(
            handle.record_spend(Some(MAX_COST), Billing::Api, &limits()),
            None
        );
        for _ in 0..=MAX_CONTINUATIONS {
            assert_eq!(handle.try_continue(&limits()), Ok(()));
        }
        assert_eq!(handle.stored(), None);
    }

    #[test]
    fn a_restored_engagement_keeps_its_budget_and_numbering() {
        let (clock, handle) = handle();
        handle.engage(true);
        handle.try_continue(&limits()).unwrap();
        handle.record_spend(Some(SPEND), Billing::Api, &limits());
        clock.advance(RAN);
        let meta = SessionMeta {
            autopilot: handle.stored().map(Box::new),
            ..SessionMeta::default()
        };
        let decisions = (0..STORED_DECISIONS)
            .map(|seq| decision(u32::try_from(seq).unwrap()).into())
            .collect();

        let restored = AutopilotHandle::from_meta(&meta, decisions, FakeClock::new(START_MS));

        assert!(restored.is_engaged());
        assert!(restored.owns_goal());
        assert_eq!(
            restored.stored(),
            meta.autopilot.as_deref().cloned(),
            "{RESTORE_KEEPS_BUDGET}"
        );
        assert_eq!(
            u64::from(restored.next_seq()),
            STORED_DECISIONS,
            "{SEQ_CONTINUES}"
        );
        assert_eq!(
            restored.recent_decisions().len(),
            usize::try_from(STORED_DECISIONS).unwrap()
        );
    }

    #[test_case(None ; "engaged")]
    #[test_case(Some(STOPPED) ; "ended")]
    fn an_engagement_round_trips_through_storage(end: Option<AutopilotEnd>) {
        let (clock, handle) = handle();
        handle.engage(false);
        handle.record_spend(Some(SPEND), Billing::Subscription, &limits());
        clock.advance(RAN);
        if let Some(end) = end {
            handle.end(end);
        }
        let stored = handle.stored().unwrap();
        let meta = SessionMeta {
            autopilot: Some(Box::new(
                serde_json::from_str(&serde_json::to_string(&stored).unwrap()).unwrap(),
            )),
            ..SessionMeta::default()
        };

        let restored = AutopilotHandle::from_meta(&meta, Vec::new(), clock);

        assert_eq!(restored.stored(), Some(stored));
    }

    #[test]
    fn an_ended_engagement_stops_its_clock() {
        let (clock, handle) = handle();
        handle.engage(false);
        clock.advance(RAN);
        handle.end(STOPPED);
        clock.advance(RAN);

        assert_eq!(
            handle.stored().unwrap().elapsed_ms,
            u64::try_from(RAN.as_millis()).unwrap()
        );
    }

    #[test]
    fn engaging_resets_the_budget_and_the_claims() {
        let (_, handle) = handle();
        handle.engage(false);
        handle.try_continue(&limits()).unwrap();
        handle.record(decision(handle.next_seq()));
        assert!(handle.claim_question());
        assert!(handle.claim_plan());
        handle.end(STOPPED);

        handle.engage(false);

        let stored = handle.stored().unwrap();
        assert_eq!((stored.continuations, stored.last_end), (0, None));
        assert!(!handle.question_claimed());
        assert!(!handle.plan_claimed());
        assert!(handle.recent_decisions().is_empty());
        assert_eq!(handle.next_seq(), 1, "{SEQ_CONTINUES}");
    }

    #[test]
    fn a_claim_goes_to_whoever_takes_it_first() {
        let (_, handle) = handle();

        assert!(handle.claim_question());
        assert!(!handle.claim_question());
        assert!(handle.question_claimed());
        assert!(!handle.plan_claimed());
        handle.reset_question_claim();

        assert!(!handle.question_claimed());
        assert!(handle.claim_plan());
        assert!(!handle.claim_plan());
        handle.reset_plan_claim();
        assert!(!handle.plan_claimed());
    }

    #[test]
    fn a_goal_gets_one_second_opinion() {
        let (_, handle) = handle();

        assert!(handle.take_second_opinion(GENERATION));
        assert!(!handle.take_second_opinion(GENERATION));
        assert!(handle.take_second_opinion(GENERATION + 1));
    }

    #[test]
    fn only_the_recent_decisions_are_kept() {
        let (_, handle) = handle();
        handle.engage(false);

        for _ in 0..=RECENT_DECISIONS {
            let event = handle.record(decision(handle.next_seq()));
            assert!(matches!(event, AutopilotEvent::Decided(_)));
        }

        let recent = handle.recent_decisions();
        assert_eq!(recent.len(), RECENT_DECISIONS);
        assert_eq!(recent.first().map(|decision| decision.seq), Some(1));
    }

    #[test]
    fn the_observed_snapshot_is_the_latest() {
        let (_, handle) = handle();
        assert!(handle.snapshot().is_none());
        let snapshot = AutopilotSnapshot::new(
            &[Message::user(PROMPT.into())],
            [],
            None,
            PermissionMode::default(),
            idle_chat(),
        );
        let messages = Arc::clone(&snapshot.messages);

        handle.observe(snapshot);

        assert!(Arc::ptr_eq(&handle.snapshot().unwrap().messages, &messages));
    }

    #[test_case(AutopilotEvent::Engaged, ENGAGED_JSON ; "engaged")]
    #[test_case(AutopilotEvent::Deciding { seq: 3, kind: DecisionKind::Permission, subject: SUBJECT.into() }, DECIDING_JSON ; "deciding")]
    #[test_case(budget_end(AutopilotBudget::Cost), ENDED_JSON ; "ended")]
    fn an_event_serializes_under_the_agent_event_tag(event: AutopilotEvent, expected: &str) {
        let value = serde_json::to_value(AgentEvent::Autopilot(event)).unwrap();

        assert_eq!(value, serde_json::from_str::<Value>(expected).unwrap());
    }

    #[test]
    fn a_decided_event_carries_the_decision_inline() {
        let value = serde_json::to_value(AgentEvent::from(AutopilotEvent::Decided(Box::new(
            decision(0),
        ))))
        .unwrap();

        assert_eq!(value["event"], json!(DECIDED_EVENT));
        assert_eq!(value["action"], json!(ActionName::Allow.name()));
        assert_eq!(value["subject"], json!(SUBJECT));
    }

    #[test]
    fn an_engagement_is_announced_once() {
        let notice = standing_notice(&[], true).expect(ANNOUNCED_ONCE);
        assert_eq!(notice.user_text(), Some(AUTOPILOT_ENGAGED_PROMPT));

        assert!(
            standing_notice(&[notice], true).is_none(),
            "{ANNOUNCED_ONCE}"
        );
    }

    #[test]
    fn the_ended_text_withdraws_the_engaged_one() {
        let history = [engaged_notice()];

        let notice = standing_notice(&history, false).expect(ENDED_REPLACES);

        assert_eq!(notice.user_text(), Some(AUTOPILOT_ENDED_PROMPT));
    }

    #[test_case(vec![] ; "never_engaged")]
    #[test_case(vec![engaged_notice(), ended_notice()] ; "already_ended")]
    #[test_case(vec![delegate_reply()] ; "only_delegate_text")]
    fn nothing_is_ended_without_an_engaged_text_in_force(history: Vec<Message>) {
        assert!(
            standing_notice(&history, false).is_none(),
            "{NOTHING_TO_WITHDRAW}"
        );
    }

    #[test]
    fn engaging_again_is_announced_again() {
        let history = [engaged_notice(), ended_notice()];

        let notice = standing_notice(&history, true).expect(ANNOUNCED_ONCE);

        assert_eq!(notice.user_text(), Some(AUTOPILOT_ENGAGED_PROMPT));
    }

    #[test]
    fn delegate_text_with_the_heading_is_not_an_announcement() {
        let history = [delegate_reply()];

        let notice = standing_notice(&history, true).expect(DELEGATE_TEXT_IGNORED);

        assert_eq!(notice.user_text(), Some(AUTOPILOT_ENGAGED_PROMPT));
    }
}
