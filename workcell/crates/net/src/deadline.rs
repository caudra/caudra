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

/// Run one fallible stage like [`run_until`], judging its failure by the
/// deadline rather than by what the stage called it.
///
/// `timeout_at` polls the stage before its timer, so a task polled after both
/// expired sees the stage's own failure, and a transport whose timer runs on
/// the remaining time fails exactly then. A failure that surfaces once the
/// deadline has passed is therefore the deadline. One that surfaces earlier,
/// an OS connect timeout included, keeps its own error, so a transport failure
/// stays retryable. A success is returned whenever it arrives.
pub(crate) async fn try_run_until<F, T, E>(
    deadline: Instant,
    cancellation: &CancellationToken,
    future: F,
) -> Result<T, NetError>
where
    F: Future<Output = Result<T, E>>,
    E: Into<NetError>,
{
    run_until(deadline, cancellation, future)
        .await?
        .map_err(|error| {
            if Instant::now() >= deadline {
                NetError::Timeout
            } else {
                error.into()
            }
        })
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
