use std::collections::BTreeMap;
use std::mem;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use async_io::Timer;
use caudra_decision::{
    Billed, Deadlines, DecisionEngine, DecisionError, DecisionRequest, DecisionResponse, Meter,
};
use caudra_providers::TokenUsage;
use futures_lite::future;
use smol::Task;

use super::Decisions;
use super::admission::Hold;

/// How long a call whose caller stopped waiting may still read its reply, so
/// what the endpoint billed for it is booked.
pub(super) const DECISION_LINGER: Duration = Duration::from_secs(30);
/// Abandoned calls one engine lets read on at once. Past it an abandoned call
/// is cancelled and books only what already arrived.
pub(super) const MAX_LINGERING_CALLS: usize = 16;

/// What a session's decision calls spent since the agent last booked it
/// against the session, its goal and autopilot, per model.
#[derive(Default)]
pub struct DecisionSpend(Mutex<BTreeMap<String, Spent>>);

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Spent {
    pub usage: TokenUsage,
    pub cost: Option<f64>,
}

impl DecisionSpend {
    pub(super) fn add(&self, model: &str, usage: TokenUsage, cost: Option<f64>) {
        let mut pending = lock(&self.0);
        let spent = pending.entry(model.to_owned()).or_default();
        spent.usage += usage;
        spent.cost = match (spent.cost, cost) {
            (Some(booked), Some(cost)) => Some(booked + cost),
            (booked, cost) => booked.or(cost),
        };
    }

    pub fn take(&self) -> BTreeMap<String, Spent> {
        mem::take(&mut *lock(&self.0))
    }

    pub(super) fn absorb(&self, other: &Self) {
        for (model, spent) in other.take() {
            self.add(&model, spent.usage, spent.cost);
        }
    }
}

#[derive(Default)]
struct Collected(Mutex<Vec<Billed>>);

impl Meter for Collected {
    fn billed(&self, billed: Billed) {
        lock(&self.0).push(billed);
    }
}

impl Collected {
    fn take(&self) -> Vec<Billed> {
        mem::take(&mut *lock(&self.0))
    }
}

/// One request on its own task. Dropped before its reply arrives, it reads
/// on until `DECISION_LINGER` and books whatever the endpoint billed.
pub(super) struct MeteredCall {
    service: Decisions,
    model: String,
    cwd: String,
    meter: Arc<Collected>,
    deadlines: Deadlines,
    task: Option<Task<Result<DecisionResponse, DecisionError>>>,
}

impl MeteredCall {
    /// `hold` rides on the request's task and drops with it.
    pub(super) fn start(
        service: &Decisions,
        engine: Arc<dyn DecisionEngine>,
        request: DecisionRequest,
        deadlines: Deadlines,
        cwd: String,
        hold: Hold,
    ) -> Self {
        let meter = Arc::new(Collected::default());
        let model = request.model.clone();
        let task = smol::spawn({
            let meter = Arc::clone(&meter);
            let deadlines = deadlines.clone();
            async move {
                let _hold = hold;
                let linger = deadlines.linger;
                future::race(
                    engine.decide_metered(&request, deadlines, meter.as_ref()),
                    async {
                        Timer::at(linger).await;
                        Err(DecisionError::Timeout)
                    },
                )
                .await
            }
        });
        Self {
            service: service.clone(),
            model,
            cwd,
            meter,
            deadlines,
            task: Some(task),
        }
    }

    /// The reply, or `None` when `deadline` comes first.
    pub(super) async fn reply(
        &mut self,
        deadline: Instant,
    ) -> Option<Result<DecisionResponse, DecisionError>> {
        let task = self.task.as_mut()?;
        let reply = future::race(async { Some(task.await) }, async {
            Timer::at(deadline).await;
            None
        })
        .await;
        if reply.is_some() {
            self.task = None;
        }
        reply
    }

    pub(super) async fn book(&self) {
        self.service
            .book(&self.model, &self.cwd, self.meter.take())
            .await;
    }
}

impl Drop for MeteredCall {
    fn drop(&mut self) {
        let service = self.service.clone();
        let model = mem::take(&mut self.model);
        let cwd = mem::take(&mut self.cwd);
        let meter = Arc::clone(&self.meter);
        if self.task.is_some() {
            self.deadlines.abandon();
        }
        let lingering = self.task.take().and_then(|task| {
            LingerSlot::claim(&service.0.limits.lingering).map(|slot| (task, slot))
        });
        if lingering.is_none() && lock(&meter.0).is_empty() {
            return;
        }
        smol::spawn(async move {
            if let Some((task, _slot)) = lingering {
                let _ = task.await;
            }
            service.book(&model, &cwd, meter.take()).await;
        })
        .detach();
    }
}

/// One of an engine's `MAX_LINGERING_CALLS`, held while an abandoned call reads on.
struct LingerSlot(Arc<AtomicUsize>);

impl LingerSlot {
    fn claim(lingering: &Arc<AtomicUsize>) -> Option<Self> {
        lingering
            .try_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                (held < MAX_LINGERING_CALLS).then_some(held + 1)
            })
            .ok()?;
        Some(Self(Arc::clone(lingering)))
    }
}

impl Drop for LingerSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    use caudra_providers::TokenUsage;
    use test_case::test_case;

    use super::{DecisionSpend, LingerSlot, MAX_LINGERING_CALLS, Spent};

    const MODEL: &str = "jev-1.14.0";
    const OTHER_MODEL: &str = "decision-1";

    fn usage(input: u32) -> TokenUsage {
        TokenUsage {
            input,
            ..TokenUsage::default()
        }
    }

    #[test_case(Some(1.0), Some(2.0), Some(3.0); "priced")]
    #[test_case(None, None, None; "unpriced")]
    #[test_case(None, Some(2.0), Some(2.0); "priced_after_unpriced")]
    fn spend_coalesces_by_model(first: Option<f64>, second: Option<f64>, total: Option<f64>) {
        let spend = DecisionSpend::default();
        spend.add(MODEL, usage(1), first);
        spend.add(MODEL, usage(2), second);
        spend.add(OTHER_MODEL, usage(4), None);
        let taken = spend.take();
        assert_eq!(
            taken[MODEL],
            Spent {
                usage: usage(3),
                cost: total
            }
        );
        assert_eq!(taken[OTHER_MODEL].usage, usage(4));
        assert!(spend.take().is_empty());
    }

    #[test]
    fn linger_slots_are_bounded_and_returned() {
        let lingering = Arc::new(AtomicUsize::new(0));
        let slots: Vec<_> = (0..MAX_LINGERING_CALLS)
            .map(|_| LingerSlot::claim(&lingering).unwrap())
            .collect();
        assert!(LingerSlot::claim(&lingering).is_none());
        drop(slots);
        assert!(LingerSlot::claim(&lingering).is_some());
    }
}
