//! Rolling rates and backoffs. The caller passes the wall clock in unix milliseconds, and every
//! mark serialises, so a limit window survives a restart in the binding or the session meta.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::meta::AutomationLimits;

const MINUTE_MS: i64 = 60_000;
pub const ROLLING_WINDOW_MS: i64 = 60 * MINUTE_MS;
pub const BACKOFF_BASE_MS: i64 = MINUTE_MS;
pub const BACKOFF_CAP_MS: i64 = 30 * MINUTE_MS;
const BACKOFF_FACTOR: i64 = 2;

/// The limiter marks of one automation in one session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActingMarks {
    /// When each acting firing in the rolling hour started, ascending. The latest stays past the
    /// hour, because the cooldown runs from it.
    pub acting: Vec<i64>,
    /// Failed firings since the last completed one.
    pub failure_streak: u32,
    pub backoff_until: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitReason {
    Cooldown,
    MaxPerHour,
    Backoff,
}

/// Why the first action of a firing must wait, and the time it may act again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitRefusal {
    pub reason: LimitReason,
    pub until: i64,
}

impl ActingMarks {
    /// When several limits apply, the one that lasts longest, so a single wait clears them all.
    pub fn check(&self, limits: &AutomationLimits, now: i64) -> Result<(), LimitRefusal> {
        let backoff = self.backoff_until.map(|until| LimitRefusal {
            reason: LimitReason::Backoff,
            until,
        });
        let cooldown = self.acting.last().map(|last| LimitRefusal {
            reason: LimitReason::Cooldown,
            until: last.saturating_add(millis(limits.cooldown)),
        });
        let max_per_hour =
            full_until(&self.acting, limits.max_per_hour, now).map(|until| LimitRefusal {
                reason: LimitReason::MaxPerHour,
                until,
            });
        [backoff, cooldown, max_per_hour]
            .into_iter()
            .flatten()
            .filter(|refusal| refusal.until > now)
            .max_by_key(|refusal| refusal.until)
            .map_or(Ok(()), Err)
    }

    pub fn record_acting(&mut self, now: i64) {
        record(&mut self.acting, now);
    }

    pub fn record_completed(&mut self) {
        self.failure_streak = 0;
        self.backoff_until = None;
    }

    /// The next acting firing waits a minute, doubling with each further failure up to the cap.
    pub fn record_failed(&mut self, now: i64) {
        self.failure_streak = self.failure_streak.saturating_add(1);
        self.backoff_until = Some(now.saturating_add(backoff_ms(self.failure_streak)));
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Automation-started turns in the session's rolling hour.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnWindow {
    /// When each turn started, ascending.
    pub turns: Vec<i64>,
}

impl TurnWindow {
    /// The time a turn fits again when the window is full.
    pub fn check(&self, per_hour: u32, now: i64) -> Result<(), i64> {
        full_until(&self.turns, per_hour, now).map_or(Ok(()), Err)
    }

    pub fn record(&mut self, now: i64) {
        record(&mut self.turns, now);
    }
}

/// Automation-started turns since the last human input.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnattendedTurns {
    pub count: u32,
}

impl UnattendedTurns {
    /// Whether another turn fits under `cap`; without one, every turn does.
    pub fn check(&self, cap: Option<u32>) -> bool {
        cap.is_none_or(|cap| self.count < cap)
    }

    pub fn record(&mut self) {
        self.count = self.count.saturating_add(1);
    }

    pub fn reset(&mut self) {
        self.count = 0;
    }
}

/// Holds automation deliveries back while the runs they start keep ending in error.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryBackoff {
    /// Erroring runs since the last clean one or human input.
    pub errors: u32,
    pub until: Option<i64>,
}

impl DeliveryBackoff {
    /// The time the next delivery may go when one must wait.
    pub fn check(&self, now: i64) -> Result<(), i64> {
        self.until.filter(|until| *until > now).map_or(Ok(()), Err)
    }

    pub fn record_error(&mut self, now: i64) {
        self.errors = self.errors.saturating_add(1);
        self.until = Some(now.saturating_add(backoff_ms(self.errors)));
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// When the rolling hour of `times` (ascending) has room for one more, or `None` while it has
/// room now. A zero rate never has room.
fn full_until(times: &[i64], per_hour: u32, now: i64) -> Option<i64> {
    let window = &times[times.partition_point(|at| *at <= now.saturating_sub(ROLLING_WINDOW_MS))..];
    let oldest_to_leave = window.len().checked_sub(per_hour as usize)?;
    Some(
        window
            .get(oldest_to_leave)
            .map_or(i64::MAX, |at| at.saturating_add(ROLLING_WINDOW_MS)),
    )
}

/// Inserts `now` in order and drops what left the rolling hour. The latest entry is never at
/// or before `now`'s window start, so it always stays.
fn record(times: &mut Vec<i64>, now: i64) {
    times.insert(times.partition_point(|at| *at <= now), now);
    let left = times.partition_point(|at| *at <= now.saturating_sub(ROLLING_WINDOW_MS));
    times.drain(..left);
}

fn backoff_ms(streak: u32) -> i64 {
    BACKOFF_FACTOR
        .checked_pow(streak.saturating_sub(1))
        .and_then(|factor| BACKOFF_BASE_MS.checked_mul(factor))
        .map_or(BACKOFF_CAP_MS, |delay| delay.min(BACKOFF_CAP_MS))
}

fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use serde::de::DeserializeOwned;
    use test_case::test_case;

    use super::*;

    const NOW: i64 = 1_790_000_000_000;
    const COOLDOWN: Duration = Duration::from_mins(5);
    const COOLDOWN_MS: i64 = 5 * MINUTE_MS;
    const LONG_COOLDOWN: Duration = Duration::from_hours(2);
    const LONG_COOLDOWN_MS: i64 = 2 * ROLLING_WINDOW_MS;
    const PER_HOUR: u32 = 3;
    const TURNS_PER_HOUR: u32 = 2;
    const UNATTENDED_CAP: u32 = 2;
    const ROUND_TRIP: &str = "limiter marks must round-trip through JSON";

    fn limits(cooldown: Duration, max_per_hour: u32) -> AutomationLimits {
        AutomationLimits {
            cooldown,
            max_per_hour,
        }
    }

    fn refusal(reason: LimitReason, until: i64) -> Result<(), LimitRefusal> {
        Err(LimitRefusal { reason, until })
    }

    fn acted_at(times: &[i64]) -> ActingMarks {
        let mut marks = ActingMarks::default();
        for at in times {
            marks.record_acting(*at);
        }
        marks
    }

    fn round_trip<T: Serialize + DeserializeOwned>(value: &T) -> T {
        serde_json::from_value(serde_json::to_value(value).expect(ROUND_TRIP)).expect(ROUND_TRIP)
    }

    #[test]
    fn a_fresh_automation_may_act() {
        assert_eq!(
            ActingMarks::default().check(&AutomationLimits::default(), NOW),
            Ok(())
        );
    }

    #[test_case(COOLDOWN_MS - 1 => refusal(LimitReason::Cooldown, NOW + COOLDOWN_MS); "inside_the_cooldown")]
    #[test_case(COOLDOWN_MS => Ok(()); "when_it_ends")]
    fn the_cooldown_runs_from_the_last_acting_firing(elapsed: i64) -> Result<(), LimitRefusal> {
        acted_at(&[NOW]).check(&limits(COOLDOWN, PER_HOUR), NOW + elapsed)
    }

    #[test]
    fn a_cooldown_longer_than_the_window_survives_pruning() {
        let marks = acted_at(&[NOW, NOW + ROLLING_WINDOW_MS + MINUTE_MS]);
        let last = NOW + ROLLING_WINDOW_MS + MINUTE_MS;
        assert_eq!(marks.acting, [last]);
        assert_eq!(
            marks.check(&limits(LONG_COOLDOWN, PER_HOUR), last + ROLLING_WINDOW_MS),
            refusal(LimitReason::Cooldown, last + LONG_COOLDOWN_MS)
        );
    }

    #[test_case(10 * MINUTE_MS => refusal(LimitReason::MaxPerHour, NOW + ROLLING_WINDOW_MS); "while_the_window_is_full")]
    #[test_case(ROLLING_WINDOW_MS => Ok(()); "once_the_oldest_leaves")]
    fn max_per_hour_counts_the_rolling_window(elapsed: i64) -> Result<(), LimitRefusal> {
        acted_at(&[NOW, NOW + MINUTE_MS, NOW + 2 * MINUTE_MS])
            .check(&limits(Duration::ZERO, PER_HOUR), NOW + elapsed)
    }

    #[test]
    fn recording_prunes_firings_that_left_the_window() {
        let marks = acted_at(&[NOW, NOW + MINUTE_MS, NOW + ROLLING_WINDOW_MS]);
        assert_eq!(marks.acting, [NOW + MINUTE_MS, NOW + ROLLING_WINDOW_MS]);
    }

    #[test]
    fn a_clock_that_steps_back_keeps_the_marks_in_order() {
        let marks = acted_at(&[NOW, NOW - MINUTE_MS]);
        assert_eq!(marks.acting, [NOW - MINUTE_MS, NOW]);
    }

    #[test_case(1 => BACKOFF_BASE_MS; "first_failure")]
    #[test_case(2 => 2 * BACKOFF_BASE_MS; "second_failure")]
    #[test_case(5 => 16 * BACKOFF_BASE_MS; "fifth_failure")]
    #[test_case(6 => BACKOFF_CAP_MS; "capped")]
    #[test_case(u32::MAX => BACKOFF_CAP_MS; "far_past_the_cap")]
    fn the_backoff_doubles_up_to_the_cap(streak: u32) -> i64 {
        backoff_ms(streak)
    }

    #[test]
    fn failures_back_off_and_a_completion_clears_it() {
        let mut marks = ActingMarks::default();
        marks.record_failed(NOW);
        marks.record_failed(NOW);
        assert_eq!(marks.failure_streak, 2);
        assert_eq!(
            marks.check(&AutomationLimits::default(), NOW),
            refusal(LimitReason::Backoff, NOW + 2 * BACKOFF_BASE_MS)
        );
        marks.record_completed();
        assert_eq!(marks, ActingMarks::default());
        assert_eq!(marks.check(&AutomationLimits::default(), NOW), Ok(()));
    }

    #[test]
    fn the_longest_refusal_wins() {
        let mut marks = acted_at(&[NOW]);
        marks.record_failed(NOW);
        assert_eq!(
            marks.check(&limits(COOLDOWN, PER_HOUR), NOW),
            refusal(LimitReason::Cooldown, NOW + COOLDOWN_MS)
        );
    }

    #[test]
    fn reset_forgets_every_mark() {
        let mut marks = acted_at(&[NOW]);
        marks.record_failed(NOW);
        marks.reset();
        assert_eq!(marks, ActingMarks::default());
    }

    #[test_case(MINUTE_MS => Err(NOW + ROLLING_WINDOW_MS); "while_full")]
    #[test_case(ROLLING_WINDOW_MS => Ok(()); "once_the_first_turn_leaves")]
    fn the_turn_window_frees_a_slot_an_hour_after_a_turn(elapsed: i64) -> Result<(), i64> {
        let mut window = TurnWindow::default();
        window.record(NOW);
        window.record(NOW + MINUTE_MS);
        window.check(TURNS_PER_HOUR, NOW + elapsed)
    }

    #[test]
    fn a_zero_turn_rate_never_has_room() {
        assert_eq!(TurnWindow::default().check(0, NOW), Err(i64::MAX));
    }

    #[test]
    fn unattended_turns_stop_at_the_cap_until_reset() {
        let mut turns = UnattendedTurns::default();
        for _ in 0..UNATTENDED_CAP {
            assert!(turns.check(Some(UNATTENDED_CAP)));
            turns.record();
        }
        assert!(!turns.check(Some(UNATTENDED_CAP)));
        assert!(turns.check(None));
        turns.reset();
        assert!(turns.check(Some(UNATTENDED_CAP)));
    }

    #[test]
    fn the_delivery_backoff_doubles_and_resets() {
        let mut backoff = DeliveryBackoff::default();
        assert_eq!(backoff.check(NOW), Ok(()));
        backoff.record_error(NOW);
        assert_eq!(backoff.check(NOW), Err(NOW + BACKOFF_BASE_MS));
        backoff.record_error(NOW + BACKOFF_BASE_MS);
        assert_eq!(
            backoff.check(NOW + BACKOFF_BASE_MS),
            Err(NOW + 3 * BACKOFF_BASE_MS)
        );
        assert_eq!(backoff.check(NOW + 3 * BACKOFF_BASE_MS), Ok(()));
        backoff.reset();
        assert_eq!(backoff, DeliveryBackoff::default());
    }

    #[test]
    fn every_mark_round_trips_through_json() {
        let mut marks = acted_at(&[NOW, NOW + MINUTE_MS]);
        marks.record_failed(NOW);
        let mut window = TurnWindow::default();
        window.record(NOW);
        let mut turns = UnattendedTurns::default();
        turns.record();
        let mut backoff = DeliveryBackoff::default();
        backoff.record_error(NOW);
        let refusal = LimitRefusal {
            reason: LimitReason::MaxPerHour,
            until: NOW,
        };
        assert_eq!(round_trip(&marks), marks);
        assert_eq!(round_trip(&window), window);
        assert_eq!(round_trip(&turns), turns);
        assert_eq!(round_trip(&backoff), backoff);
        assert_eq!(round_trip(&refusal), refusal);
    }
}
