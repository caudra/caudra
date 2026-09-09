//! A one-way wake for a wait that is already scheduled.

use std::sync::Arc;

use event_listener::{Event, EventListener};

/// Cuts short a wait someone else is already in. Carrying no state is the
/// point: a nudge that arrives while nothing is waiting is dropped rather than
/// queued, so it can only ever shorten a delay, never schedule work of its own.
/// That is what lets a stale click be harmless.
#[derive(Clone)]
pub struct Nudge(Arc<Event>);

impl Default for Nudge {
    fn default() -> Self {
        Self(Arc::new(Event::new()))
    }
}

impl Nudge {
    pub fn notify(&self) {
        self.0.notify(usize::MAX);
    }

    /// Registers interest now and waits later. A caller that announces the wait
    /// before entering it must listen first, or the nudge answering that
    /// announcement lands while nothing is waiting and is dropped.
    pub fn listen(&self) -> EventListener {
        self.0.listen()
    }

    pub async fn notified(&self) {
        self.listen().await;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    const NEVER: Duration = Duration::from_secs(3600);

    #[test]
    fn a_nudge_wakes_a_waiter() {
        smol::block_on(async {
            let nudge = Nudge::default();
            let waiter = nudge.clone();
            let woken = futures_lite::future::race(
                async {
                    waiter.notified().await;
                    true
                },
                async {
                    smol::future::yield_now().await;
                    nudge.notify();
                    smol::Timer::after(NEVER).await;
                    false
                },
            )
            .await;
            assert!(woken);
        });
    }

    /// The whole point of dropping an unheard nudge: a click that lands
    /// between two waits must not make the next one return immediately.
    #[test]
    fn a_nudge_with_no_waiter_is_dropped() {
        smol::block_on(async {
            let nudge = Nudge::default();
            nudge.notify();
            let timed_out = futures_lite::future::race(
                async {
                    nudge.notified().await;
                    false
                },
                async {
                    smol::Timer::after(Duration::ZERO).await;
                    true
                },
            )
            .await;
            assert!(timed_out, "a nudge must not be queued for a later waiter");
        });
    }
}
