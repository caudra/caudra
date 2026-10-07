//! The runtime's clocks, behind a trait so tests drive time by hand. The wall clock stamps
//! events, limits and schedules in unix milliseconds; the monotonic clock times `after` delays
//! and wakes the runtime, so a wall clock that steps never fires a delay early or late.

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_io::Timer;

#[cfg(any(test, feature = "test-support"))]
pub use fake::FakeClock;

pub type Wake = Pin<Box<dyn Future<Output = ()> + Send>>;

pub trait Clock: Send + Sync {
    /// Unix milliseconds.
    fn now_ms(&self) -> i64;
    /// Time since the clock started; it never steps back.
    fn monotonic(&self) -> Duration;
    /// Completes once [`Self::monotonic`] reaches `deadline`.
    fn sleep_until(&self, deadline: Duration) -> Wake;
}

pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, millis)
    }

    fn monotonic(&self) -> Duration {
        self.origin.elapsed()
    }

    fn sleep_until(&self, deadline: Duration) -> Wake {
        let timer = self
            .origin
            .checked_add(deadline)
            .map_or_else(Timer::never, Timer::at);
        Box::pin(async move {
            timer.await;
        })
    }
}

pub(super) fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

#[cfg(any(test, feature = "test-support"))]
mod fake {
    use std::mem;
    use std::sync::{Arc, Mutex, MutexGuard};
    use std::time::Duration;

    use super::{Clock, Wake, millis};

    /// A clock that moves only when [`Self::advance`] moves it, waking every sleeper whose
    /// deadline it passes.
    pub struct FakeClock {
        time: Mutex<FakeTime>,
    }

    struct FakeTime {
        wall_ms: i64,
        elapsed: Duration,
        sleepers: Vec<(Duration, flume::Sender<()>)>,
    }

    impl FakeClock {
        pub fn new(wall_ms: i64) -> Arc<Self> {
            Arc::new(Self {
                time: Mutex::new(FakeTime {
                    wall_ms,
                    elapsed: Duration::ZERO,
                    sleepers: Vec::new(),
                }),
            })
        }

        pub fn advance(&self, by: Duration) {
            let due = {
                let mut time = self.lock();
                time.wall_ms = time.wall_ms.saturating_add(millis(by));
                time.elapsed += by;
                let elapsed = time.elapsed;
                let (due, waiting): (Vec<_>, Vec<_>) = mem::take(&mut time.sleepers)
                    .into_iter()
                    .partition(|(deadline, _)| *deadline <= elapsed);
                time.sleepers = waiting;
                due
            };
            for (_, wake) in due {
                let _ = wake.send(());
            }
        }

        fn lock(&self) -> MutexGuard<'_, FakeTime> {
            self.time
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        }
    }

    impl Clock for FakeClock {
        fn now_ms(&self) -> i64 {
            self.lock().wall_ms
        }

        fn monotonic(&self) -> Duration {
            self.lock().elapsed
        }

        fn sleep_until(&self, deadline: Duration) -> Wake {
            let (wake, woken) = flume::bounded(1);
            {
                let mut time = self.lock();
                time.sleepers
                    .retain(|(_, sleeper)| !sleeper.is_disconnected());
                if deadline <= time.elapsed {
                    let _ = wake.send(());
                } else {
                    time.sleepers.push((deadline, wake));
                }
            }
            Box::pin(async move {
                let _ = woken.recv_async().await;
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use futures_lite::future;

    use super::*;

    const START_MS: i64 = 1_790_000_000_000;
    const DELAY: Duration = Duration::from_secs(60);
    const SHORT: Duration = Duration::from_secs(59);
    const SLEEPS_UNTIL_DEADLINE: &str = "a sleeper must wake exactly when its deadline passes";

    #[test]
    fn a_fake_sleeper_wakes_only_once_time_reaches_its_deadline() {
        let clock = FakeClock::new(START_MS);
        let mut sleep = clock.sleep_until(DELAY);

        clock.advance(SHORT);
        assert!(
            future::block_on(future::poll_once(&mut sleep)).is_none(),
            "{SLEEPS_UNTIL_DEADLINE}"
        );
        clock.advance(DELAY - SHORT);

        assert!(
            future::block_on(future::poll_once(&mut sleep)).is_some(),
            "{SLEEPS_UNTIL_DEADLINE}"
        );
        assert_eq!(clock.now_ms(), START_MS + millis(DELAY));
        assert_eq!(clock.monotonic(), DELAY);
    }

    #[test]
    fn a_past_deadline_wakes_at_once() {
        let clock = FakeClock::new(START_MS);
        clock.advance(DELAY);

        assert!(future::block_on(future::poll_once(clock.sleep_until(SHORT))).is_some());
    }
}
