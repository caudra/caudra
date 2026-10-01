use std::time::Duration;

const INITIAL_DELAY: Duration = Duration::from_secs(2);
const BACKOFF_FACTOR: u32 = 2;
const JITTER_FACTOR: f64 = 0.25;
const MAX_DELAY: Duration = Duration::from_secs(30);
pub const MAX_RETRIES: u32 = 8;

/// Where the wait came from. A server hint and a locally computed backoff fail
/// for different reasons, so a log that conflates them cannot tell whether the
/// provider is throttling or the network is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelaySource {
    ServerHint,
    Exponential,
}

impl DelaySource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ServerHint => "server_hint",
            Self::Exponential => "exponential",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GiveUpReason {
    AttemptsSpent,
}

impl GiveUpReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AttemptsSpent => "attempts_spent",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDecision {
    Wait {
        attempt: u32,
        delay: Duration,
        source: DelaySource,
    },
    GiveUp(GiveUpReason),
}

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

    pub fn decide(&mut self, hint: Option<Duration>) -> RetryDecision {
        if self.attempt >= MAX_RETRIES {
            return RetryDecision::GiveUp(GiveUpReason::AttemptsSpent);
        }
        self.attempt += 1;
        match hint {
            Some(delay) => RetryDecision::Wait {
                attempt: self.attempt,
                delay,
                source: DelaySource::ServerHint,
            },
            None => RetryDecision::Wait {
                attempt: self.attempt,
                delay: exponential(self.attempt, fastrand::f64()),
                source: DelaySource::Exponential,
            },
        }
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

    const QUOTA_RESET: Duration = Duration::from_secs(13 * 3_600);
    const WEEKLY_RESET: Duration = Duration::from_secs(7 * 24 * 3_600);

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
    fn retry_after_hint_overrides_backoff_and_says_so() {
        let hint = Duration::from_secs(5);
        let mut state = RetryState::new();
        for _ in 0..3 {
            state.decide(None);
        }
        assert_eq!(
            state.decide(Some(hint)),
            RetryDecision::Wait {
                attempt: 4,
                delay: hint,
                source: DelaySource::ServerHint,
            }
        );
    }

    #[test]
    fn a_computed_backoff_reports_its_source() {
        let mut state = RetryState::new();
        assert!(matches!(
            state.decide(None),
            RetryDecision::Wait {
                source: DelaySource::Exponential,
                ..
            }
        ));
    }

    #[test_case(Duration::from_secs(60) ; "former_limit")]
    #[test_case(Duration::from_secs(61) ; "above_former_limit")]
    #[test_case(QUOTA_RESET ; "subscription_quota_reset")]
    #[test_case(WEEKLY_RESET ; "weekly_quota_reset")]
    fn server_hint_is_not_shortened_or_refused(hint: Duration) {
        let mut state = RetryState::new();
        assert_eq!(
            state.decide(Some(hint)),
            RetryDecision::Wait {
                attempt: 1,
                delay: hint,
                source: DelaySource::ServerHint,
            }
        );
    }

    #[test_case(None ; "exponential")]
    #[test_case(Some(QUOTA_RESET) ; "subscription_quota_reset")]
    fn retrying_stops_after_max_retries(hint: Option<Duration>) {
        let mut state = RetryState::new();
        for expected in 1..=MAX_RETRIES {
            assert!(matches!(
                state.decide(hint),
                RetryDecision::Wait { attempt, .. } if attempt == expected
            ));
        }
        assert_eq!(
            state.decide(hint),
            RetryDecision::GiveUp(GiveUpReason::AttemptsSpent)
        );
        assert_eq!(state.attempts(), MAX_RETRIES);
    }

    #[test_case(Duration::from_secs(1) ; "short_hint")]
    #[test_case(QUOTA_RESET ; "subscription_quota_reset")]
    fn a_spent_state_stays_spent_even_with_a_hint(hint: Duration) {
        let mut state = RetryState::new();
        for _ in 0..MAX_RETRIES {
            state.decide(None);
        }
        assert_eq!(
            state.decide(Some(hint)),
            RetryDecision::GiveUp(GiveUpReason::AttemptsSpent)
        );
    }

    #[test]
    fn attempts_counts_burned_retries() {
        let mut state = RetryState::new();
        assert_eq!(state.attempts(), 0);
        state.decide(None);
        state.decide(None);
        assert_eq!(state.attempts(), 2);
    }
}
