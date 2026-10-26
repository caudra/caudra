use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Instant;

use async_io::Timer;
use async_lock::{Semaphore, SemaphoreGuardArc};
use futures_lite::future;
use smol::channel::{self, Receiver, Sender};

/// Content screening calls one engine runs at once, across every session on
/// it, so a batch of fetches queues rather than timing out together.
pub(super) const MAX_SCREENING_CALLS: usize = 4;

/// What every service on one engine shares: the calls left lingering, and the
/// slots content screening reaches the engine through.
#[derive(Clone)]
pub(super) struct EngineLimits {
    pub(super) lingering: Arc<AtomicUsize>,
    screening: Arc<Semaphore>,
}

impl Default for EngineLimits {
    fn default() -> Self {
        Self {
            lingering: Arc::default(),
            screening: Arc::new(Semaphore::new(MAX_SCREENING_CALLS)),
        }
    }
}

impl EngineLimits {
    pub(super) fn try_admit(&self) -> Option<Admission> {
        self.screening
            .try_acquire_arc()
            .map(|slot| Admission { _slot: slot })
    }

    /// A slot, or `None` once `until` passes without one.
    pub(super) async fn admit(&self, until: Instant) -> Option<Admission> {
        future::race(
            async {
                Some(Admission {
                    _slot: self.screening.acquire_arc().await,
                })
            },
            async {
                Timer::at(until).await;
                None
            },
        )
        .await
    }
}

/// A screening slot. The request it admits holds it until that request is
/// over, lingering included, so a call its caller gave up on still counts.
pub struct Admission {
    _slot: SemaphoreGuardArc,
}

/// Rides on a request's task. Dropped when the task ends or is cancelled, it
/// frees the slot and settles the request's [`Settled`].
pub(super) struct Hold {
    _admission: Option<Admission>,
    _done: Sender<()>,
}

/// Resolves once a request's task is over, so the same question is never in
/// flight twice.
pub struct Settled(Receiver<()>);

impl Settled {
    pub async fn wait(self) {
        let _ = self.0.recv().await;
    }
}

pub(super) fn hold(admission: Option<Admission>) -> (Hold, Settled) {
    let (done, settled) = channel::bounded(1);
    (
        Hold {
            _admission: admission,
            _done: done,
        },
        Settled(settled),
    )
}
