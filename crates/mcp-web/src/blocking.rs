use std::future::Future;
use std::sync::{Arc, LazyLock};

use tokio::sync::Semaphore;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const MAX_BLOCKING_JOBS: usize = 4;
static BLOCKING_JOBS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_BLOCKING_JOBS)));

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BlockingError {
    Cancelled,
    TimedOut,
    Panicked,
}

pub(crate) async fn run_until<T, F>(
    deadline: Instant,
    cancellation: &CancellationToken,
    operation: F,
) -> Result<T, BlockingError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let permit = select_until(
        deadline,
        cancellation,
        Arc::clone(&BLOCKING_JOBS).acquire_owned(),
    )
    .await?
    .map_err(|_| BlockingError::Panicked)?;
    let worker = tokio::task::spawn_blocking(move || {
        // Keep capacity occupied until the worker really exits. Timing out or
        // cancelling the async waiter does not kill spawn_blocking work.
        let _permit = permit;
        operation()
    });
    select_until(deadline, cancellation, worker)
        .await?
        .map_err(|_| BlockingError::Panicked)
}

async fn select_until<T>(
    deadline: Instant,
    cancellation: &CancellationToken,
    future: impl Future<Output = T>,
) -> Result<T, BlockingError> {
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(BlockingError::Cancelled),
        result = tokio::time::timeout_at(deadline, future) => {
            result.map_err(|_| BlockingError::TimedOut)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use futures_util::future::join_all;

    use super::*;

    #[tokio::test]
    async fn blocking_jobs_never_exceed_the_global_limit() {
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let deadline = Instant::now() + Duration::from_secs(2);
        let jobs = (0..MAX_BLOCKING_JOBS + 3).map(|_| {
            let active = Arc::clone(&active);
            let maximum = Arc::clone(&maximum);
            async move {
                run_until(deadline, &CancellationToken::new(), move || {
                    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                    maximum.fetch_max(current, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(10));
                    active.fetch_sub(1, Ordering::SeqCst);
                })
                .await
                .unwrap();
            }
        });
        join_all(jobs).await;
        assert!(maximum.load(Ordering::SeqCst) <= MAX_BLOCKING_JOBS);
    }
}
