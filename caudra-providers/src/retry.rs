use std::time::Duration;

const INITIAL_DELAY: Duration = Duration::from_secs(2);
const BACKOFF_FACTOR: u32 = 2;
const JITTER_FACTOR: f64 = 0.25;
const MAX_DELAY: Duration = Duration::from_secs(30);
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);
const MAX_RETRIES: u32 = 8;

#[derive(Default)]
pub struct RetryState {
    attempt: u32,
}

impl RetryState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Retries burned so far; the attempt currently failing is this plus one.
    pub fn attempts(&self) -> u32 {
        self.attempt
    }

    /// The next wait, or `None` once retrying is pointless: attempts are spent,
    /// or the provider named a window further out than we are willing to sit on.
    /// A quota that reopens in an hour is a refusal, and retrying at some
    /// shorter cap would only burn the remaining attempts before it opens.
    pub fn next_delay(&mut self, hint: Option<Duration>) -> Option<(u32, Duration)> {
        if self.attempt >= MAX_RETRIES || hint.is_some_and(|after| after > MAX_RETRY_AFTER) {
            return None;
        }
        self.attempt += 1;
        let delay = hint.unwrap_or_else(|| exponential(self.attempt, fastrand::f64()));
        Some((self.attempt, delay))
    }
}

/// Jitter is additive so a retry never fires earlier than the base delay, and
/// `random` is a parameter so the curve is testable without a seeded RNG.
fn exponential(attempt: u32, random: f64) -> Duration {
    let base = INITIAL_DELAY
        .saturating_mul(BACKOFF_FACTOR.saturating_pow(attempt.saturating_sub(1)))
        .min(MAX_DELAY);
    base.saturating_add(base.mul_f64(JITTER_FACTOR * random))
        .min(MAX_DELAY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case(1, 2  ; "first")]
    #[test_case(2, 4  ; "second")]
    #[test_case(3, 8  ; "third")]
    #[test_case(4, 16 ; "fourth")]
    fn exponential_doubles_per_attempt(attempt: u32, expected_secs: u64) {
        assert_eq!(
            exponential(attempt, 0.0),
            Duration::from_secs(expected_secs)
        );
    }

    #[test_case(5 ; "just_over")]
    #[test_case(MAX_RETRIES ; "last")]
    fn exponential_caps_at_max_delay(attempt: u32) {
        assert_eq!(exponential(attempt, 1.0), MAX_DELAY);
    }

    #[test_case(1 ; "first")]
    #[test_case(2 ; "second")]
    #[test_case(3 ; "third")]
    #[test_case(4 ; "fourth")]
    fn jitter_only_lengthens_delay(attempt: u32) {
        let base = exponential(attempt, 0.0);
        let jittered = exponential(attempt, 1.0);
        assert!(jittered >= base);
        assert!(jittered <= base.mul_f64(1.0 + JITTER_FACTOR));
    }

    #[test]
    fn retry_after_hint_overrides_backoff() {
        let hint = Duration::from_secs(5);
        let mut state = RetryState::new();
        for _ in 0..3 {
            state.next_delay(None);
        }
        assert_eq!(state.next_delay(Some(hint)), Some((4, hint)));
    }

    #[test_case(MAX_RETRY_AFTER, true                             ; "at_the_limit")]
    #[test_case(MAX_RETRY_AFTER + Duration::from_secs(1), false   ; "just_over")]
    #[test_case(Duration::from_secs(3600), false                  ; "quota_reset")]
    fn an_over_long_hint_ends_retrying(hint: Duration, usable: bool) {
        let mut state = RetryState::new();
        assert_eq!(state.next_delay(Some(hint)).is_some(), usable);
    }

    #[test]
    fn retrying_stops_after_max_retries() {
        let mut state = RetryState::new();
        for expected in 1..=MAX_RETRIES {
            assert_eq!(
                state.next_delay(None).map(|(attempt, _)| attempt),
                Some(expected)
            );
        }
        assert_eq!(state.next_delay(None), None);
        assert_eq!(state.attempts(), MAX_RETRIES);
    }

    #[test]
    fn a_spent_state_stays_spent_even_with_a_hint() {
        let mut state = RetryState::new();
        for _ in 0..MAX_RETRIES {
            state.next_delay(None);
        }
        assert_eq!(state.next_delay(Some(Duration::from_secs(1))), None);
    }

    #[test]
    fn attempts_counts_burned_retries() {
        let mut state = RetryState::new();
        assert_eq!(state.attempts(), 0);
        state.next_delay(None);
        state.next_delay(None);
        assert_eq!(state.attempts(), 2);
    }
}
