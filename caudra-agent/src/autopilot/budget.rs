//! What an engagement has used against the limits config sets: the
//! continuations it sent, what it spent, and the wall time it ran while the
//! session was open. The limits are passed in at each check and never kept,
//! so a reload applies from the next check, and a limit lowered below what was
//! used ends the engagement there.

use std::time::Duration;

use caudra_config::AutopilotConfig;
use caudra_providers::Billing;
use caudra_storage::autopilot::{AutopilotBudget, StoredAutopilot};

/// Consecutive `continue` reviews allowed with no human input between them.
pub const MAX_REVIEW_STREAK: u32 = 3;
const FIRST_RETRY_DELAY: Duration = Duration::from_secs(60);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30 * 60);
const RETRY_DELAY_GROWTH: u32 = 2;
const SECONDS_PER_MINUTE: u64 = 60;

/// One engagement's use. Times are the monotonic clock's
/// ([`crate::automation::clock::Clock::monotonic`]), which a wall clock that
/// steps cannot move.
#[derive(Debug, Clone, PartialEq)]
pub struct Budget {
    continuations: u32,
    cost_usd: f64,
    subscription_cost_usd: f64,
    /// Run time carried over from the processes that held the engagement
    /// before this one, and from before it last paused.
    elapsed_before: Duration,
    /// When this process took the engagement over, `None` once it ended.
    resumed_at: Option<Duration>,
}

impl Budget {
    pub fn new(now: Duration) -> Self {
        Self {
            continuations: 0,
            cost_usd: 0.0,
            subscription_cost_usd: 0.0,
            elapsed_before: Duration::ZERO,
            resumed_at: Some(now),
        }
    }

    /// Picks a stored engagement back up. The clock of one still engaged
    /// restarts at `now`, so the time the session spent closed is not counted.
    pub fn restored(stored: &StoredAutopilot, now: Duration) -> Self {
        Self {
            continuations: stored.continuations,
            cost_usd: stored.cost_usd,
            subscription_cost_usd: stored.subscription_cost_usd,
            elapsed_before: Duration::from_millis(stored.elapsed_ms),
            resumed_at: stored.engaged.then_some(now),
        }
    }

    /// Stops the clock where the engagement ended.
    pub fn pause(&mut self, now: Duration) {
        self.elapsed_before = self.elapsed(now);
        self.resumed_at = None;
    }

    /// Writes the use into `stored`, leaving the rest of the engagement alone.
    pub fn store(&self, stored: &mut StoredAutopilot, now: Duration) {
        stored.continuations = self.continuations;
        stored.cost_usd = self.cost_usd;
        stored.subscription_cost_usd = self.subscription_cost_usd;
        stored.elapsed_ms = u64::try_from(self.elapsed(now).as_millis()).unwrap_or(u64::MAX);
    }

    pub fn elapsed(&self, now: Duration) -> Duration {
        let running = self
            .resumed_at
            .map_or(Duration::ZERO, |resumed_at| now.saturating_sub(resumed_at));
        self.elapsed_before.saturating_add(running)
    }

    /// Files spend under whoever pays for it. Unpriced spend adds nothing.
    pub fn add_spend(&mut self, cost: Option<f64>, billing: Billing) {
        let Some(cost) = cost else {
            return;
        };
        match billing {
            Billing::Api => self.cost_usd += cost,
            Billing::Subscription => self.subscription_cost_usd += cost,
        }
    }

    /// The spending limit the engagement has reached, checked after spend is
    /// added.
    pub fn overspent(&self, limits: &AutopilotConfig, now: Duration) -> Option<AutopilotBudget> {
        if limits.max_cost.is_some_and(|max| self.cost_usd >= max) {
            return Some(AutopilotBudget::Cost);
        }
        let max_time = limits
            .max_minutes
            .map(|minutes| Duration::from_secs(u64::from(minutes) * SECONDS_PER_MINUTE));
        if max_time.is_some_and(|max| self.elapsed(now) >= max) {
            return Some(AutopilotBudget::Time);
        }
        None
    }

    /// Checks every limit ahead of a continuation and counts it when none is
    /// reached.
    pub fn try_continue(
        &mut self,
        limits: &AutopilotConfig,
        now: Duration,
    ) -> Result<(), AutopilotBudget> {
        if self.continuations >= limits.max_continuations {
            return Err(AutopilotBudget::Continuations);
        }
        if let Some(budget) = self.overspent(limits, now) {
            return Err(budget);
        }
        self.continuations += 1;
        Ok(())
    }
}

#[allow(
    dead_code,
    reason = "the status panel reads the use, and it is not built yet"
)]
impl Budget {
    pub fn continuations(&self) -> u32 {
        self.continuations
    }

    /// API-billed spend, the part [`AutopilotConfig::max_cost`] caps.
    pub fn cost_usd(&self) -> f64 {
        self.cost_usd
    }

    /// Spend a subscription covered: shown, never capped.
    pub fn subscription_cost_usd(&self) -> f64 {
        self.subscription_cost_usd
    }
}

/// Consecutive `continue` reviews with no human input between them, so a
/// stuck loop stops before the budget does.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReviewStreak(u32);

impl ReviewStreak {
    /// Counts a review the delegate continued, or `false` once
    /// [`MAX_REVIEW_STREAK`] are in a row, when the review escalates instead.
    pub fn try_continue(&mut self) -> bool {
        if self.0 >= MAX_REVIEW_STREAK {
            return false;
        }
        self.0 += 1;
        true
    }

    /// Human input breaks the streak.
    pub fn reset(&mut self) {
        self.0 = 0;
    }
}

/// The wait before retry `attempt` of a retryable provider error, counted from
/// 1: a minute, doubling each time, capped at 30 minutes. `None` past
/// `error_retries`, when the error ends the engagement.
pub fn retry_delay(attempt: u32, error_retries: u32) -> Option<Duration> {
    if attempt == 0 || attempt > error_retries {
        return None;
    }
    let delay = RETRY_DELAY_GROWTH
        .checked_pow(attempt - 1)
        .and_then(|growth| FIRST_RETRY_DELAY.checked_mul(growth))
        .unwrap_or(MAX_RETRY_DELAY);
    Some(delay.min(MAX_RETRY_DELAY))
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;
    use crate::automation::clock::{Clock, FakeClock};

    const START_MS: i64 = 1_790_000_000_000;
    const RESTART_GAP: Duration = Duration::from_secs(24 * 60 * 60);
    const MINUTE: Duration = Duration::from_secs(60);
    const MAX_CONTINUATIONS: u32 = 3;
    const MAX_COST: f64 = 1.0;
    const MAX_MINUTES: u32 = 10;
    const ERROR_RETRIES: u32 = 5;
    const MOST_ERROR_RETRIES: u32 = 20;
    const HALF_COST: f64 = 0.5;
    const SUBSCRIPTION_SPEND: f64 = 100.0;
    const RAN_BEFORE_RESTART: Duration = Duration::from_secs(4 * 60);
    const RAN_AFTER_RESTART: Duration = Duration::from_secs(5 * 60);
    const STREAK_ESCALATES: &str = "a review past the streak must escalate";
    const HUMAN_RESETS: &str = "human input must break the streak";
    const COUNTS_EACH: &str = "a continuation must count once it is allowed";
    const REFUSED_NOT_COUNTED: &str = "a refused continuation must not count";
    const SUBSCRIPTION_UNCAPPED: &str = "subscription spend must never end an engagement";
    const CLOSED_TIME_FREE: &str = "time the session spent closed must not count";
    const ENDED_TIME_FREE: &str = "time after an engagement ended must not count";

    fn limits() -> AutopilotConfig {
        AutopilotConfig {
            max_continuations: MAX_CONTINUATIONS,
            max_cost: Some(MAX_COST),
            max_minutes: Some(MAX_MINUTES),
            error_retries: ERROR_RETRIES,
        }
    }

    fn unlimited() -> AutopilotConfig {
        AutopilotConfig {
            max_continuations: u32::MAX,
            max_cost: None,
            max_minutes: None,
            ..limits()
        }
    }

    fn engagement() -> StoredAutopilot {
        StoredAutopilot {
            engaged: true,
            owns_goal: false,
            engaged_at_ms: None,
            continuations: 0,
            cost_usd: 0.0,
            subscription_cost_usd: 0.0,
            elapsed_ms: 0,
            last_end: None,
        }
    }

    #[test]
    fn continuations_run_out_at_the_limit() {
        let clock = FakeClock::new(START_MS);
        let mut budget = Budget::new(clock.monotonic());

        for _ in 0..MAX_CONTINUATIONS {
            assert_eq!(budget.try_continue(&limits(), clock.monotonic()), Ok(()));
        }
        assert_eq!(budget.continuations(), MAX_CONTINUATIONS, "{COUNTS_EACH}");

        assert_eq!(
            budget.try_continue(&limits(), clock.monotonic()),
            Err(AutopilotBudget::Continuations)
        );
        assert_eq!(
            budget.continuations(),
            MAX_CONTINUATIONS,
            "{REFUSED_NOT_COUNTED}"
        );
    }

    #[test]
    fn api_spend_runs_out_at_the_cost_limit() {
        let clock = FakeClock::new(START_MS);
        let mut budget = Budget::new(clock.monotonic());

        budget.add_spend(Some(HALF_COST), Billing::Api);
        budget.add_spend(None, Billing::Api);
        assert_eq!(budget.overspent(&limits(), clock.monotonic()), None);
        budget.add_spend(Some(HALF_COST), Billing::Api);

        assert_eq!(
            budget.overspent(&limits(), clock.monotonic()),
            Some(AutopilotBudget::Cost)
        );
        assert_eq!(
            budget.try_continue(&limits(), clock.monotonic()),
            Err(AutopilotBudget::Cost)
        );
    }

    #[test]
    fn subscription_spend_is_shown_but_never_capped() {
        let clock = FakeClock::new(START_MS);
        let mut budget = Budget::new(clock.monotonic());

        budget.add_spend(Some(SUBSCRIPTION_SPEND), Billing::Subscription);

        assert_eq!(budget.subscription_cost_usd(), SUBSCRIPTION_SPEND);
        assert_eq!(budget.cost_usd(), 0.0);
        assert_eq!(
            budget.try_continue(&limits(), clock.monotonic()),
            Ok(()),
            "{SUBSCRIPTION_UNCAPPED}"
        );
    }

    #[test]
    fn wall_time_runs_out_at_the_time_limit() {
        let clock = FakeClock::new(START_MS);
        let budget = Budget::new(clock.monotonic());

        clock.advance(MINUTE * (MAX_MINUTES - 1));
        assert_eq!(budget.overspent(&limits(), clock.monotonic()), None);
        clock.advance(MINUTE);

        assert_eq!(
            budget.overspent(&limits(), clock.monotonic()),
            Some(AutopilotBudget::Time)
        );
    }

    #[test_case(|limits| limits.max_continuations = 1, AutopilotBudget::Continuations ; "continuations")]
    #[test_case(|limits| limits.max_cost = Some(HALF_COST), AutopilotBudget::Cost ; "cost")]
    #[test_case(|limits| limits.max_minutes = Some(1), AutopilotBudget::Time ; "time")]
    fn a_limit_lowered_below_use_ends_at_the_next_check(
        lower: fn(&mut AutopilotConfig),
        expected: AutopilotBudget,
    ) {
        let clock = FakeClock::new(START_MS);
        let mut budget = Budget::new(clock.monotonic());
        budget
            .try_continue(&unlimited(), clock.monotonic())
            .unwrap();
        budget
            .try_continue(&unlimited(), clock.monotonic())
            .unwrap();
        budget.add_spend(Some(MAX_COST), Billing::Api);
        clock.advance(MINUTE * 2);
        let mut lowered = unlimited();
        lower(&mut lowered);

        assert_eq!(
            budget.try_continue(&lowered, clock.monotonic()),
            Err(expected)
        );
    }

    #[test]
    fn wall_time_carries_across_a_restart_but_not_the_gap() {
        let clock = FakeClock::new(START_MS);
        let mut budget = Budget::new(clock.monotonic());
        budget.try_continue(&limits(), clock.monotonic()).unwrap();
        budget.add_spend(Some(HALF_COST), Billing::Api);
        budget.add_spend(Some(HALF_COST), Billing::Subscription);
        clock.advance(RAN_BEFORE_RESTART);
        let mut stored = engagement();
        budget.store(&mut stored, clock.monotonic());
        let saved = serde_json::to_string(&stored).unwrap();

        let reopened = FakeClock::new(START_MS + i64::try_from(RESTART_GAP.as_millis()).unwrap());
        let loaded: StoredAutopilot = serde_json::from_str(&saved).unwrap();
        let resumed = Budget::restored(&loaded, reopened.monotonic());
        assert_eq!(
            resumed.elapsed(reopened.monotonic()),
            RAN_BEFORE_RESTART,
            "{CLOSED_TIME_FREE}"
        );
        reopened.advance(RAN_AFTER_RESTART);

        assert_eq!(
            resumed.elapsed(reopened.monotonic()),
            RAN_BEFORE_RESTART + RAN_AFTER_RESTART
        );
        assert_eq!(resumed.continuations(), budget.continuations());
        assert_eq!(resumed.cost_usd(), budget.cost_usd());
        assert_eq!(
            resumed.subscription_cost_usd(),
            budget.subscription_cost_usd()
        );
        assert_eq!(
            resumed.overspent(&limits(), reopened.monotonic()),
            None,
            "{CLOSED_TIME_FREE}"
        );
    }

    #[test]
    fn an_ended_engagement_stops_counting_time() {
        let clock = FakeClock::new(START_MS);
        let mut budget = Budget::new(clock.monotonic());
        clock.advance(RAN_BEFORE_RESTART);
        budget.pause(clock.monotonic());
        clock.advance(RAN_AFTER_RESTART);
        assert_eq!(
            budget.elapsed(clock.monotonic()),
            RAN_BEFORE_RESTART,
            "{ENDED_TIME_FREE}"
        );
        let mut stored = StoredAutopilot {
            engaged: false,
            ..engagement()
        };
        budget.store(&mut stored, clock.monotonic());

        let reopened = Budget::restored(&stored, clock.monotonic());
        clock.advance(RAN_AFTER_RESTART);

        assert_eq!(
            reopened.elapsed(clock.monotonic()),
            RAN_BEFORE_RESTART,
            "{ENDED_TIME_FREE}"
        );
    }

    #[test]
    fn the_review_streak_escalates_past_its_limit_until_a_human_speaks() {
        let mut streak = ReviewStreak::default();
        for _ in 0..MAX_REVIEW_STREAK {
            assert!(streak.try_continue());
        }

        assert!(!streak.try_continue(), "{STREAK_ESCALATES}");
        streak.reset();
        assert!(streak.try_continue(), "{HUMAN_RESETS}");
    }

    #[test_case(1, ERROR_RETRIES, Some(MINUTE) ; "first")]
    #[test_case(2, ERROR_RETRIES, Some(MINUTE * 2) ; "second")]
    #[test_case(3, ERROR_RETRIES, Some(MINUTE * 4) ; "third")]
    #[test_case(4, ERROR_RETRIES, Some(MINUTE * 8) ; "fourth")]
    #[test_case(5, ERROR_RETRIES, Some(MINUTE * 16) ; "fifth")]
    #[test_case(6, ERROR_RETRIES, None ; "past_the_retries")]
    #[test_case(6, MOST_ERROR_RETRIES, Some(MAX_RETRY_DELAY) ; "capped")]
    #[test_case(MOST_ERROR_RETRIES, MOST_ERROR_RETRIES, Some(MAX_RETRY_DELAY) ; "last_of_the_most")]
    #[test_case(0, ERROR_RETRIES, None ; "attempt_zero")]
    #[test_case(1, 0, None ; "no_retries")]
    fn retries_back_off_by_doubling_to_a_cap(
        attempt: u32,
        error_retries: u32,
        expected: Option<Duration>,
    ) {
        assert_eq!(retry_delay(attempt, error_retries), expected);
    }
}
