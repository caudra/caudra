use std::future::Future;
use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::NetError;

/// Run one stage under the operation-wide deadline and cancellation token.
/// Keeping this common prevents DNS, redirects, retries, or body reads from
/// accidentally receiving independent time budgets.
pub(crate) async fn run_until<F, T>(
    deadline: Instant,
    cancellation: &CancellationToken,
    future: F,
) -> Result<T, NetError>
where
    F: Future<Output = T>,
{
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(NetError::Cancelled),
        result = tokio::time::timeout_at(deadline, future) => result.map_err(|_| NetError::Timeout),
    }
}

pub(crate) async fn sleep_until_or_cancel(
    delay: Duration,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<(), NetError> {
    run_until(deadline, cancellation, tokio::time::sleep(delay)).await
}

pub(crate) fn remaining(deadline: Instant) -> Result<Duration, NetError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or(NetError::Timeout)
}
