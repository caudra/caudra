use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::resolver::SourceIconError;

pub(crate) struct ResolutionBudget {
    deadline: Instant,
    remaining_requests: AtomicUsize,
}

impl ResolutionBudget {
    pub(crate) fn new(total_timeout: Duration, max_requests: usize) -> Self {
        Self {
            deadline: Instant::now() + total_timeout,
            remaining_requests: AtomicUsize::new(max_requests),
        }
    }

    pub(crate) const fn deadline(&self) -> Instant {
        self.deadline
    }

    pub(crate) fn begin_request(
        &self,
        per_request_timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<Option<Duration>, SourceIconError> {
        if cancellation.is_cancelled() {
            return Err(SourceIconError::Cancelled);
        }
        let Some(remaining) = self.deadline.checked_duration_since(Instant::now()) else {
            return Ok(None);
        };
        if self
            .remaining_requests
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_sub(1)
            })
            .is_err()
        {
            return Ok(None);
        }
        Ok(Some(per_request_timeout.min(remaining)))
    }
}
