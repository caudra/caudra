//! The settle feed: which runs of the session reached a terminal status since
//! a subscriber last looked. A publication only wakes each feed, through a
//! signal that coalesces and never blocks the run that published; the feed
//! then reads the published state itself, so a subscriber that falls behind
//! still finds every run that settled meanwhile.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use caudra_workflow::{RunSnapshot, WorkflowState};
use flume::Receiver;

/// The runs of one session as they complete, fail, are cancelled, or are
/// interrupted. Each terminal state is reported once, as the snapshot the
/// state holds when the feed looks, so several terminal states of one run
/// between two looks collapse into the latest. A run already terminal when
/// the feed opened is reported only once it settles again.
///
/// A terminal state is new when its execution epoch is. Every attempt runs
/// under a fresh epoch and every interrupt commits the next one, so a resumed
/// run that settles again is news, while a republish, an acknowledgement, or
/// a usage update that lands after the outcome is not. The revision cannot
/// tell those apart: agents that finish after their script has ended still
/// advance it.
pub struct SettledRuns {
    wake: Receiver<()>,
    state: Arc<ArcSwap<WorkflowState>>,
    /// Per run, the first epoch whose terminal state the feed has not seen.
    unseen_from: HashMap<String, u64>,
}

impl SettledRuns {
    /// `wake` must be registered before this reads the state, so whatever
    /// the seed misses still wakes the feed.
    pub(super) fn new(wake: Receiver<()>, state: Arc<ArcSwap<WorkflowState>>) -> Self {
        let seed = state.load();
        let mut feed = Self {
            wake,
            state,
            unseen_from: HashMap::new(),
        };
        for run in &seed.runs {
            feed.observe(run);
        }
        feed
    }

    /// Waits until runs have settled since the last call and returns them,
    /// the oldest update first and then by run id. `None` once the runtime
    /// has shut down and nothing settled remains. Cancel-safe.
    pub async fn next(&mut self) -> Option<Vec<RunSnapshot>> {
        loop {
            let open = self.wake.recv_async().await.is_ok();
            let settled = self.settled();
            if !settled.is_empty() {
                return Some(settled);
            }
            if !open {
                return None;
            }
        }
    }

    fn settled(&mut self) -> Vec<RunSnapshot> {
        let state = self.state.load();
        let mut settled: Vec<RunSnapshot> = state
            .runs
            .iter()
            .filter(|run| self.observe(run))
            .cloned()
            .collect();
        settled.sort_by(|left, right| {
            left.updated_at
                .cmp(&right.updated_at)
                .then_with(|| left.run_id.cmp(&right.run_id))
        });
        settled
    }

    /// Notes what the feed has now seen of `run`, and whether that is a
    /// terminal state it had not seen before.
    fn observe(&mut self, run: &RunSnapshot) -> bool {
        let terminal = run.status.is_terminal();
        let seen_through = run.execution_epoch + u64::from(terminal);
        match self.unseen_from.get_mut(&run.run_id) {
            Some(unseen_from) => {
                let news = terminal && run.execution_epoch >= *unseen_from;
                *unseen_from = (*unseen_from).max(seen_through);
                news
            }
            None => {
                self.unseen_from.insert(run.run_id.clone(), seen_through);
                terminal
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use caudra_workflow::{RunStatus, RunUsage, SourceKind};
    use futures_lite::future::poll_once;

    use super::*;
    use crate::workflow::state::{Published, publish};

    const RUN_ID: &str = "run-a";
    const SECOND_RUN_ID: &str = "run-b";
    const THIRD_RUN_ID: &str = "run-c";
    const EARLIER: u64 = 10;
    const LATER: u64 = 20;
    const FIRST_EPOCH: u64 = 0;
    const RESUMED_EPOCH: u64 = 1;

    fn snapshot(
        run_id: &str,
        status: RunStatus,
        execution_epoch: u64,
        updated_at: u64,
    ) -> RunSnapshot {
        RunSnapshot {
            run_id: run_id.into(),
            display_name: run_id.into(),
            workflow_name: run_id.into(),
            source_kind: SourceKind::User,
            source_path: None,
            objective: None,
            status,
            pause_kind: None,
            pause_message: None,
            revision: 0,
            execution_epoch,
            phase: None,
            phases: Vec::new(),
            phase_history: Vec::new(),
            agent_budget: 1,
            usage: RunUsage::default(),
            roster: Vec::new(),
            result: None,
            error: None,
            logs: Vec::new(),
            outbox_pending: false,
            created_at: 0,
            updated_at,
        }
    }

    fn statuses(settled: Option<Vec<RunSnapshot>>) -> Vec<(String, RunStatus)> {
        settled
            .unwrap_or_default()
            .into_iter()
            .map(|run| (run.run_id, run.status))
            .collect()
    }

    async fn settled_now(feed: &mut SettledRuns) -> Vec<(String, RunStatus)> {
        statuses(poll_once(feed.next()).await.flatten())
    }

    #[test]
    fn a_spawned_feed_waits_out_wakes_that_settle_nothing() {
        smol::block_on(async {
            let published = Published::new(WorkflowState::default());
            let mut feed = published.observe_settled();
            let waiting = smol::spawn(async move { feed.next().await });
            publish(
                &published,
                &snapshot(RUN_ID, RunStatus::Active, FIRST_EPOCH, EARLIER),
            );
            publish(
                &published,
                &snapshot(RUN_ID, RunStatus::Completed, FIRST_EPOCH, LATER),
            );

            assert_eq!(
                statuses(waiting.await),
                [(RUN_ID.to_owned(), RunStatus::Completed)]
            );
        });
    }

    #[test]
    fn one_look_reports_the_oldest_update_first_then_by_run_id() {
        smol::block_on(async {
            let published = Published::new(WorkflowState::default());
            let mut feed = published.observe_settled();
            for (run_id, status, updated_at) in [
                (SECOND_RUN_ID, RunStatus::Completed, EARLIER),
                (RUN_ID, RunStatus::Failed, LATER),
                (THIRD_RUN_ID, RunStatus::Interrupted, EARLIER),
            ] {
                publish(
                    &published,
                    &snapshot(run_id, status, FIRST_EPOCH, updated_at),
                );
            }

            assert_eq!(
                settled_now(&mut feed).await,
                [
                    (SECOND_RUN_ID.to_owned(), RunStatus::Completed),
                    (THIRD_RUN_ID.to_owned(), RunStatus::Interrupted),
                    (RUN_ID.to_owned(), RunStatus::Failed),
                ]
            );
        });
    }

    #[test]
    fn a_stale_epoch_published_after_a_newer_one_never_settles() {
        smol::block_on(async {
            let published = Published::new(WorkflowState::default());
            let mut feed = published.observe_settled();
            let failed = snapshot(RUN_ID, RunStatus::Failed, FIRST_EPOCH, EARLIER);
            publish(&published, &failed);
            assert_eq!(
                settled_now(&mut feed).await,
                [(RUN_ID.to_owned(), RunStatus::Failed)]
            );

            publish(
                &published,
                &snapshot(RUN_ID, RunStatus::Active, RESUMED_EPOCH, LATER),
            );
            assert!(settled_now(&mut feed).await.is_empty());
            publish(&published, &failed);
            assert!(settled_now(&mut feed).await.is_empty());

            publish(
                &published,
                &snapshot(RUN_ID, RunStatus::Completed, RESUMED_EPOCH, LATER),
            );
            assert_eq!(
                settled_now(&mut feed).await,
                [(RUN_ID.to_owned(), RunStatus::Completed)]
            );
        });
    }
}
