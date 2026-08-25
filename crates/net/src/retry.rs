use std::collections::BTreeSet;
use std::time::{Duration, SystemTime};

use http::{HeaderMap, StatusCode};

/// Bounded retry behavior for idempotent GET requests.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetryPolicy {
    /// Number of attempts after the initial request, capped by the caller.
    pub max_retries: usize,
    /// Initial exponential-backoff delay.
    pub base_delay: Duration,
    /// Maximum delay, including a server-provided `Retry-After` value.
    pub max_delay: Duration,
    /// HTTP statuses eligible for retry.
    pub statuses: BTreeSet<StatusCode>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 1,
            base_delay: Duration::from_millis(75),
            max_delay: Duration::from_millis(250),
            statuses: [
                StatusCode::REQUEST_TIMEOUT,
                StatusCode::TOO_EARLY,
                StatusCode::TOO_MANY_REQUESTS,
                StatusCode::INTERNAL_SERVER_ERROR,
                StatusCode::BAD_GATEWAY,
                StatusCode::SERVICE_UNAVAILABLE,
                StatusCode::GATEWAY_TIMEOUT,
            ]
            .into_iter()
            .collect(),
        }
    }
}

impl RetryPolicy {
    /// A retry policy that performs exactly one request.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            max_retries: 0,
            ..Self::default()
        }
    }

    /// Return the bounded delay for a zero-based retry number.
    #[must_use]
    pub fn delay_for(&self, retry_number: usize, headers: Option<&HeaderMap>) -> Duration {
        if let Some(delay) =
            headers.and_then(|headers| retry_after_delay(headers, SystemTime::now()))
        {
            return delay.min(self.max_delay);
        }
        let exponent = u32::try_from(retry_number).unwrap_or(u32::MAX).min(31);
        self.base_delay
            .saturating_mul(2_u32.saturating_pow(exponent))
            .min(self.max_delay)
    }
}

/// Parse `Retry-After` as delta seconds or an HTTP date.
#[must_use]
pub fn retry_after_delay(headers: &HeaderMap, now: SystemTime) -> Option<Duration> {
    let value = headers
        .get(http::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    httpdate::parse_http_date(value)
        .ok()
        .map(|at| at.duration_since(now).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_is_exponential_and_capped() {
        let policy = RetryPolicy {
            base_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(25),
            ..RetryPolicy::default()
        };
        assert_eq!(policy.delay_for(0, None), Duration::from_millis(10));
        assert_eq!(policy.delay_for(1, None), Duration::from_millis(20));
        assert_eq!(policy.delay_for(2, None), Duration::from_millis(25));
    }
}
