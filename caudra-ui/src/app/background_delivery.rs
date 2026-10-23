use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use async_lock::{Mutex, MutexGuard};
use caudra_agent::Nudge;
use caudra_agent::background::BackgroundTasks;
use caudra_agent::workflow::WorkflowHandle;
use caudra_providers::{Message, WorkflowEventOrigin};
use caudra_storage::id::CaudraId;
use caudra_workflow::{RunStatus, WorkflowRequest};
use thiserror::Error;
use tracing::{info, warn};

use super::App;
use super::tasks::{WAKES_SUPPRESSED_STOP_FAILED, WAKES_SUPPRESSED_STOP_REQUESTED};
use super::workflow::{WorkflowDelivery, WorkflowDeliveryResult};
use crate::components::{DisplayMessage, DisplayRole};

pub(crate) const STOP_FAILED_ERR: &str =
    "Session work did not stop cleanly; retry Stop before submitting more work";
const DELIVERY_RETRY_INITIAL: Duration = Duration::from_secs(1);
const DELIVERY_RETRY_MAX: Duration = Duration::from_secs(30);
pub(crate) const DELIVERY_NOTICE_AFTER: u32 = 3;
pub(crate) const DELIVERY_RETRY_FLASH: &str = "Background results could not be saved, retrying";
pub(crate) const DELIVERY_RETRY_NOTICE: &str = "Background results could not be saved; retrying. Automatic continuation resumes when a save succeeds.";
pub(crate) const DELIVERY_RECOVERED_NOTICE: &str =
    "Saving background results recovered; automatic continuation resumes.";

/// Why background results did not reach the session. A failed stop leaves
/// work whose state is unknown, so it stays fail-closed until the user acts;
/// a failed save is retried with backoff.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub(crate) enum DeliveryFailure {
    #[error("{0}")]
    Stop(String),
    #[error("{0}")]
    Save(String),
}

struct DeliveryRetry {
    attempts: u32,
    retry_at: Instant,
    noticed: bool,
}

struct ScheduledRetry {
    attempt: u32,
    delay: Duration,
    notice: bool,
}

fn retry_delay(attempt: u32) -> Duration {
    let factor = 1u32
        .checked_shl(attempt.saturating_sub(1))
        .unwrap_or(u32::MAX);
    DELIVERY_RETRY_INITIAL
        .saturating_mul(factor)
        .min(DELIVERY_RETRY_MAX)
}

#[derive(Default)]
pub(crate) struct DeliveryFence {
    pub(crate) parent: Mutex<()>,
    epoch: AtomicU64,
    stopping: AtomicBool,
    failed_stop: AtomicBool,
    stopped: Nudge,
}

impl DeliveryFence {
    pub(crate) fn dispatch_allowed(&self) -> bool {
        !self.stopping.load(Ordering::Acquire) && self.admission_error().is_none()
    }

    pub(crate) fn admission_error(&self) -> Option<&'static str> {
        self.failed_stop
            .load(Ordering::Acquire)
            .then_some(STOP_FAILED_ERR)
    }

    pub(crate) async fn enter(&self) -> MutexGuard<'_, ()> {
        loop {
            let stopped = self.stopped.listen();
            if self.stopping.load(Ordering::Acquire) {
                stopped.await;
                continue;
            }
            let parent = self.parent.lock().await;
            if !self.stopping.load(Ordering::Acquire) {
                return parent;
            }
            drop(parent);
            stopped.await;
        }
    }

    pub(super) fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct DeliveryKey {
    pub session: CaudraId,
    pub run: u64,
    pub revision: u64,
    pub epoch: u64,
    pub generation: Option<u64>,
    pub final_save: bool,
    pub claims: Vec<WorkflowEventOrigin>,
    pub pending: Vec<WorkflowEventOrigin>,
}

pub(super) struct DeliveryJob {
    pub key: DeliveryKey,
    pub messages: Vec<Message>,
    pub background: Option<BackgroundTasks>,
    pub workflow: WorkflowDelivery,
}

pub(crate) struct DeliveryReply {
    id: u64,
    stop: bool,
    key: DeliveryKey,
    messages: Vec<Message>,
    result: Result<WorkflowDeliveryResult, String>,
}

pub(crate) struct BackgroundDelivery {
    pub(crate) fence: Arc<DeliveryFence>,
    pub(crate) replies: flume::Receiver<DeliveryReply>,
    sender: flume::Sender<DeliveryReply>,
    task: Option<smol::Task<()>>,
    stop_task: Option<smol::Task<()>>,
    stop_id: Option<u64>,
    rearm_after_stop: bool,
    running: Option<(u64, DeliveryKey)>,
    queued: Option<DeliveryJob>,
    completed: Option<DeliveryKey>,
    sequence: u64,
    retry: Option<DeliveryRetry>,
}

impl Default for BackgroundDelivery {
    fn default() -> Self {
        let (sender, replies) = flume::unbounded();
        Self {
            fence: Arc::default(),
            replies,
            sender,
            task: None,
            running: None,
            queued: None,
            completed: None,
            sequence: 0,
            stop_task: None,
            stop_id: None,
            rearm_after_stop: false,
            retry: None,
        }
    }
}

impl BackgroundDelivery {
    pub(crate) fn invalidate(&mut self) {
        self.fence.epoch.fetch_add(1, Ordering::AcqRel);
        self.rearm_after_stop = false;
        self.task = None;
        self.running = None;
        self.queued = None;
        self.completed = None;
    }

    pub(super) fn needs(&self, key: &DeliveryKey) -> bool {
        self.running
            .as_ref()
            .is_none_or(|(_, running)| running != key)
            && self.queued.as_ref().is_none_or(|queued| &queued.key != key)
            && self.completed.as_ref() != Some(key)
    }

    pub(crate) fn pending(&self) -> bool {
        self.running.is_some() || self.queued.is_some() || self.stop_id.is_some()
    }

    #[cfg(test)]
    pub(crate) fn jobs_started(&self) -> u64 {
        self.sequence
    }

    /// Whether a failed save is still backing off; nothing delivers until `now` passes it.
    pub(crate) fn retry_pending(&self, now: Instant) -> bool {
        self.retry
            .as_ref()
            .is_some_and(|retry| now < retry.retry_at)
    }

    #[cfg(test)]
    pub(crate) fn retry_at(&self) -> Option<Instant> {
        self.retry.as_ref().map(|retry| retry.retry_at)
    }

    fn record_failure(&mut self, now: Instant) -> ScheduledRetry {
        self.invalidate();
        let (attempts, noticed) = self
            .retry
            .as_ref()
            .map_or((0, false), |retry| (retry.attempts, retry.noticed));
        let attempt = attempts.saturating_add(1);
        let delay = retry_delay(attempt);
        let notice = !noticed && attempt >= DELIVERY_NOTICE_AFTER;
        self.retry = Some(DeliveryRetry {
            attempts: attempt,
            retry_at: now + delay,
            noticed: noticed || notice,
        });
        ScheduledRetry {
            attempt,
            delay,
            notice,
        }
    }

    pub(super) fn rearm(&mut self) -> bool {
        self.invalidate();
        self.retry = None;
        if self.stop_id.is_some() {
            self.rearm_after_stop = true;
            return false;
        }
        self.fence.admission_error().is_none()
    }

    pub(super) fn schedule(&mut self, job: DeliveryJob) {
        if self.running.is_some() || self.stop_id.is_some() {
            self.queued = Some(job);
        } else {
            self.start(job);
        }
    }

    fn start(&mut self, job: DeliveryJob) {
        self.sequence += 1;
        let id = self.sequence;
        self.running = Some((id, job.key.clone()));
        let fence = Arc::clone(&self.fence);
        let sender = self.sender.clone();
        self.task = Some(smol::spawn(async move {
            let result = async {
                let _parent = if job.key.final_save {
                    Some(fence.parent.lock().await)
                } else {
                    None
                };
                if fence.epoch() != job.key.epoch
                    || job.background.as_ref().map(BackgroundTasks::generation)
                        != job.key.generation
                {
                    return Ok(WorkflowDeliveryResult::default());
                }
                if let Some(background) = &job.background {
                    if job.key.final_save {
                        background.finalize_messages(&job.messages).await?;
                    } else {
                        background.accept_messages(&job.messages).await?;
                    }
                    background.settle_launches(&job.messages).await?;
                }
                job.workflow.run(job.key.final_save).await
            }
            .await;
            let _ = sender.send(DeliveryReply {
                id,
                stop: false,
                key: job.key,
                messages: job.messages,
                result,
            });
        }));
    }

    fn stop(
        &mut self,
        key: DeliveryKey,
        background: Option<BackgroundTasks>,
        workflow: Option<WorkflowHandle>,
    ) {
        self.rearm_after_stop = false;
        self.retry = None;
        if self.stop_id.is_some() {
            return;
        }
        self.fence.stopping.store(true, Ordering::Release);
        self.sequence += 1;
        let id = self.sequence;
        self.stop_id = Some(id);
        let fence = Arc::clone(&self.fence);
        let sender = self.sender.clone();
        self.stop_task = Some(smol::spawn(async move {
            let _parent = fence.parent.lock().await;
            let mut result = match &background {
                Some(background) => background.stop().await,
                None => Ok(()),
            };
            if let Some(workflow) = workflow {
                for run in &workflow.state().runs {
                    if matches!(
                        run.status,
                        RunStatus::Active | RunStatus::Paused | RunStatus::BudgetLimited
                    ) && let Err(error) = workflow
                        .request(WorkflowRequest::Stop {
                            run_id: run.run_id.clone(),
                        })
                        .await
                        && result.is_ok()
                    {
                        result = Err(error.to_string());
                    }
                }
            }
            let _ = sender.send(DeliveryReply {
                id,
                stop: true,
                key,
                messages: Vec::new(),
                result: result.map(|()| WorkflowDeliveryResult::default()),
            });
        }));
    }

    fn finish(&mut self, reply: &DeliveryReply, background: Option<&BackgroundTasks>) -> bool {
        if reply.stop {
            if self.stop_id != Some(reply.id) {
                return false;
            }
            self.stop_id = None;
            self.stop_task = None;
            self.fence
                .failed_stop
                .store(reply.result.is_err(), Ordering::Release);
            if std::mem::take(&mut self.rearm_after_stop)
                && reply.result.is_ok()
                && let Some(background) = background
            {
                background.rearm();
            }
            self.fence.stopping.store(false, Ordering::Release);
            self.fence.stopped.notify();
            if let Some(job) = self.queued.take() {
                self.start(job);
            }
            return true;
        }
        if self.running.as_ref().is_none_or(|(id, _)| *id != reply.id) {
            return false;
        }
        self.running = None;
        self.task = None;
        if reply.result.is_ok() {
            self.completed = Some(reply.key.clone());
        }
        if let Some(job) = self.queued.take() {
            self.start(job);
        }
        true
    }
}

impl App {
    pub(crate) fn stop_background_work(&mut self) {
        self.drop_questions();
        self.suppress_background_wakes(WAKES_SUPPRESSED_STOP_REQUESTED);
        let key = DeliveryKey {
            session: self.state.session.id,
            run: self.run_id,
            revision: self.state.session.content_revision(),
            epoch: self.background_delivery.fence.epoch(),
            generation: self.background.as_ref().map(BackgroundTasks::generation),
            final_save: false,
            claims: Vec::new(),
            pending: Vec::new(),
        };
        self.background_delivery
            .stop(key, self.background.clone(), self.workflow.runtime_handle());
    }

    pub(crate) fn apply_delivery_reply(
        &mut self,
        reply: DeliveryReply,
    ) -> Result<(), DeliveryFailure> {
        let background = self
            .background
            .as_ref()
            .filter(|_| reply.key.session == self.state.session.id);
        if !self.background_delivery.finish(&reply, background) {
            return Ok(());
        }
        if reply.stop && reply.key.session == self.state.session.id {
            self.queue.wake_dispatch();
            return reply.result.map(|_| ()).map_err(DeliveryFailure::Stop);
        }
        if reply.key.session != self.state.session.id
            || reply.key.run != self.run_id
            || reply.key.epoch != self.background_delivery.fence.epoch()
            || (!reply.stop
                && reply.key.generation
                    != self.background.as_ref().map(BackgroundTasks::generation))
        {
            return Ok(());
        }
        let current_revision = self.state.session.content_revision();
        if reply.stop {
            return reply.result.map(|_| ()).map_err(DeliveryFailure::Stop);
        }
        if reply.key.revision > current_revision {
            return Ok(());
        }
        let result = match reply.result {
            Ok(result) => result,
            Err(error) if reply.key.revision == current_revision => {
                return Err(DeliveryFailure::Save(error));
            }
            Err(_) => return Ok(()),
        };
        if reply.key.final_save && reply.key.revision == current_revision {
            self.background_claims.clear();
        } else {
            self.background_claims.retain(|claim| {
                claim.task_event.as_ref().is_some_and(|origin| {
                    !reply
                        .messages
                        .iter()
                        .any(|message| message.task_event.as_ref() == Some(origin))
                })
            });
        }
        self.workflow.apply_delivery(result);
        self.delivery_recovered();
        Ok(())
    }

    pub(crate) fn poll_background_delivery(&mut self) -> Result<(), DeliveryFailure> {
        while let Ok(reply) = self.background_delivery.replies.try_recv() {
            self.apply_delivery_reply(reply)?;
        }
        Ok(())
    }

    /// Persists and delivers background results unless a failed save is
    /// still backing off. Returns whether delivery went ahead; a failure is
    /// handed to [`App::delivery_failed`].
    pub(crate) fn persist_or_retry_background_delivery(
        &mut self,
        final_save: bool,
        site: &'static str,
        now: Instant,
    ) -> bool {
        if self.background_delivery.retry_pending(now) {
            return false;
        }
        match self.persist_background_delivery(final_save) {
            Ok(()) => true,
            Err(failure) => {
                self.delivery_failed(site, failure, now);
                false
            }
        }
    }

    /// A failed save backs off and tries again, leaving background admission
    /// and automatic wakes alone so running tasks carry on. Only a failed
    /// stop latches, as it did before.
    pub(crate) fn delivery_failed(
        &mut self,
        site: &'static str,
        failure: DeliveryFailure,
        now: Instant,
    ) {
        let error = match failure {
            DeliveryFailure::Stop(error) => {
                self.suppress_background_wakes(WAKES_SUPPRESSED_STOP_FAILED);
                self.flash(error);
                return;
            }
            DeliveryFailure::Save(error) => error,
        };
        let retry = self.background_delivery.record_failure(now);
        warn!(
            session = %self.state.session.id,
            site,
            attempt = retry.attempt,
            retry_in_ms = retry.delay.as_millis(),
            %error,
            "background delivery failed; retrying"
        );
        if retry.attempt == 1 {
            self.flash(format!("{DELIVERY_RETRY_FLASH}: {error}"));
        }
        if retry.notice {
            self.main_chat().push(DisplayMessage::new(
                DisplayRole::Notice,
                DELIVERY_RETRY_NOTICE.into(),
            ));
        }
    }

    pub(super) fn delivery_recovered(&mut self) {
        let Some(retry) = self.background_delivery.retry.take() else {
            return;
        };
        info!(
            session = %self.state.session.id,
            attempts = retry.attempts,
            "background delivery recovered"
        );
        if retry.noticed {
            self.main_chat().push(DisplayMessage::new(
                DisplayRole::Notice,
                DELIVERY_RECOVERED_NOTICE.into(),
            ));
        }
    }

    #[cfg(test)]
    pub(crate) async fn flush_background_delivery(
        &mut self,
        final_save: bool,
    ) -> Result<(), DeliveryFailure> {
        self.persist_background_delivery(final_save)?;
        while self.background_delivery.pending() {
            let reply = self
                .background_delivery
                .replies
                .recv_async()
                .await
                .map_err(|error| DeliveryFailure::Save(error.to_string()))?;
            self.apply_delivery_reply(reply)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    use caudra_agent::{AgentEvent, AgentMode, DoneReason, EventSender, PromptAdmission};
    use futures_lite::future::poll_once;
    use test_case::test_case;

    use super::{
        App, BackgroundDelivery, DELIVERY_NOTICE_AFTER, DELIVERY_RECOVERED_NOTICE,
        DELIVERY_RETRY_FLASH, DELIVERY_RETRY_MAX, DELIVERY_RETRY_NOTICE, DeliveryFailure,
        DeliveryKey, DeliveryReply, STOP_FAILED_ERR, WorkflowDeliveryResult,
    };
    use crate::agent::QueuedMessage;
    use crate::agent::shared_queue::{self, QueueItem};
    use crate::app::queue::SubmitOutcome;
    use crate::app::tests::test_app;
    use crate::app::{Mode, Msg, PlanState};
    use crate::components::{Action, DisplayRole, Status};

    const ORIGINAL_RUN: u64 = 7;
    const REPLACEMENT_RUN: u64 = ORIGINAL_RUN + 1;
    const PROMPT: &str = "Plan the replacement safely";
    const PLAN_FILE: &str = "replacement-plan.md";
    const STOP_ERROR: &str = "Background drain failed";
    const RUN_ERROR: &str = "Original run failed";
    const SAVE_ERROR: &str = "database is locked";
    const SAVE_SITE: &str = "test";

    fn prompt() -> QueuedMessage {
        QueuedMessage {
            text: PROMPT.into(),
            images: Vec::new(),
            mentions: Vec::new(),
            commits: Vec::new(),
            paste_ranges: Vec::new(),
        }
    }

    fn item(app: &App) -> QueueItem {
        QueueItem::Message {
            text: PROMPT.into(),
            image_count: 0,
            paste_ranges: Vec::new(),
            input: Box::new(app.build_agent_input(&prompt())),
            run_id: app.run_id,
            admission: PromptAdmission::Interrupt,
            displayed: false,
        }
    }

    #[test_case(false, false; "undispatched_replacement")]
    #[test_case(false, true; "unpolled_failure")]
    #[test_case(true, false; "original_still_processing")]
    #[test_case(true, true; "processing_with_unpolled_failure")]
    fn failed_stop_retry_preserves_replacement(processing: bool, unpolled: bool) {
        smol::block_on(async {
            let mut app = test_app();
            app.run_id = ORIGINAL_RUN;
            app.status = Status::Streaming;
            app.state.mode = Mode::Build;
            app.state.applied_mode = Mode::Build;
            app.state.plan =
                PlanState::Drafting(PathBuf::from(&app.state.session.cwd).join(PLAN_FILE));
            let (sender, receiver) = shared_queue::queue();
            let fence = Arc::clone(&app.background_delivery.fence);
            let dispatch = Arc::clone(&fence);
            sender.set_dispatch_guard(Arc::new(move || dispatch.dispatch_allowed()));
            app.queue.set_shared(sender.clone());
            if processing {
                sender.push(item(&app));
                assert_eq!(receiver.claim_idle(ORIGINAL_RUN).len(), 1);
            }
            app.state.mode = Mode::Plan;
            app.stop_background_work();
            let (replacement, active) = sender.replace(ORIGINAL_RUN, REPLACEMENT_RUN, item(&app));
            app.begin_main_cancel(true, active);
            app.replacement_item = Some(replacement);
            let prompts = app.queue.pending_prompts();
            let mut failed = app.background_delivery.replies.recv_async().await.unwrap();
            let stale = DeliveryReply {
                id: failed.id,
                stop: true,
                key: failed.key.clone(),
                messages: Vec::new(),
                result: Ok(WorkflowDeliveryResult::default()),
            };
            failed.result = Err(STOP_ERROR.into());
            if unpolled {
                app.background_delivery.sender.send(failed).unwrap();
            } else {
                assert_eq!(
                    app.apply_delivery_reply(failed),
                    Err(DeliveryFailure::Stop(STOP_ERROR.into()))
                );
                assert!(matches!(
                    app.submit_prompt(prompt()),
                    SubmitOutcome::Rejected(STOP_FAILED_ERR)
                ));
            }

            let parent = fence.parent.lock().await;
            assert!(app.handle_cancel().is_empty());
            assert_eq!(app.run_id, REPLACEMENT_RUN);
            assert_eq!(app.cancelling_run, processing.then_some(ORIGINAL_RUN));
            assert_eq!(app.replacement_item, Some(replacement));
            assert_eq!(app.queue.pending_prompts(), prompts);
            assert_eq!(app.status, Status::Streaming);
            assert!(!fence.dispatch_allowed());
            assert!(receiver.claim_idle(REPLACEMENT_RUN).is_empty());
            let retry_id = app.background_delivery.stop_id;
            app.apply_delivery_reply(stale).unwrap();
            assert_eq!(app.background_delivery.stop_id, retry_id);
            assert_eq!(fence.admission_error(), Some(STOP_FAILED_ERR));
            assert!(!fence.dispatch_allowed());
            drop(parent);

            let reply = app.background_delivery.replies.recv_async().await.unwrap();
            assert!(!fence.dispatch_allowed());
            assert!(receiver.claim_idle(REPLACEMENT_RUN).is_empty());
            app.apply_delivery_reply(reply).unwrap();
            assert!(fence.dispatch_allowed());
            assert_eq!(fence.admission_error(), None);
            assert_eq!(app.cancelling_run, processing.then_some(ORIGINAL_RUN));
            receiver.clear_active_run();
            let claimed = receiver.claim_idle(REPLACEMENT_RUN);
            assert_eq!(claimed.len(), 1);
            assert_eq!(claimed[0].0, replacement);
            assert!(matches!(
                &claimed[0].1,
                QueueItem::Message { input, run_id: REPLACEMENT_RUN, .. }
                    if matches!(input.mode, AgentMode::Plan(_))
            ));
            app.on_queue_item_consumed(replacement, PROMPT.into(), Arc::default());
            app.state.applied_mode = Mode::Plan;
            assert_eq!(app.cancelling_run, None);
            assert_eq!(app.replacement_item, None);
            assert!(matches!(app.submit_prompt(prompt()), SubmitOutcome::Queued));
        });
    }

    #[test_case(false; "streaming_but_not_dispatched")]
    #[test_case(true; "actually_processing")]
    fn cancel_waits_only_for_a_dispatched_run(processing: bool) {
        smol::block_on(async {
            let mut app = test_app();
            app.run_id = ORIGINAL_RUN;
            app.status = Status::Streaming;
            app.state.mode = Mode::Build;
            app.state.applied_mode = Mode::Build;
            let (sender, receiver) = shared_queue::queue();
            sender.push(item(&app));
            app.queue.set_shared(sender);
            if processing {
                assert_eq!(receiver.claim_idle(ORIGINAL_RUN).len(), 1);
            }
            assert!(matches!(
                app.handle_cancel().as_slice(),
                [Action::CancelAgent {
                    run_id: ORIGINAL_RUN
                }]
            ));
            assert_eq!(app.cancelling_run, processing.then_some(ORIGINAL_RUN));
            assert!(app.queue.is_empty());
            assert_eq!(
                app.status,
                if processing {
                    Status::Streaming
                } else {
                    Status::Idle
                }
            );
            let reply = app.background_delivery.replies.recv_async().await.unwrap();
            app.apply_delivery_reply(reply).unwrap();
            if !processing {
                assert!(matches!(
                    app.submit_prompt(prompt()),
                    SubmitOutcome::Started(_)
                ));
            }
        });
    }

    #[test_case(DoneReason::EndTurn; "completed")]
    #[test_case(DoneReason::Cancelled; "cancelled")]
    fn cancel_after_consumed_done_does_not_await_terminal(reason: DoneReason) {
        smol::block_on(async {
            let mut app = test_app();
            app.run_id = ORIGINAL_RUN;
            app.status = Status::Streaming;
            let (sender, receiver) = shared_queue::queue();
            sender.push(item(&app));
            app.queue.set_shared(sender);
            assert_eq!(receiver.claim_idle(ORIGINAL_RUN).len(), 1);
            receiver.hold_next_turn();
            let (events, incoming) = flume::unbounded();
            EventSender::new(events, ORIGINAL_RUN)
                .send(AgentEvent::Done {
                    usage: Default::default(),
                    num_turns: 1,
                    reason,
                })
                .unwrap();
            app.update(Msg::Agent(Box::new(incoming.recv().unwrap())));
            assert_eq!(app.status, Status::Idle);
            assert!(app.queue.is_processing());

            assert!(matches!(
                app.handle_cancel().as_slice(),
                [Action::CancelAgent {
                    run_id: ORIGINAL_RUN
                }]
            ));
            assert_eq!(app.cancelling_run, None);
            assert_eq!(app.status, Status::Idle);
            assert!(receiver.finish_run(ORIGINAL_RUN, false));
            let reply = app.background_delivery.replies.recv_async().await.unwrap();
            app.apply_delivery_reply(reply).unwrap();
            assert!(matches!(
                app.submit_prompt(prompt()),
                SubmitOutcome::Started(_)
            ));
        });
    }

    #[test_case(true; "failure_before_stop")]
    #[test_case(false; "stop_before_failure")]
    fn cancel_at_failure_finalization_does_not_leave_queue_paused(finish_before_stop: bool) {
        smol::block_on(async {
            let mut app = test_app();
            app.run_id = ORIGINAL_RUN;
            app.status = Status::Streaming;
            let (sender, receiver) = shared_queue::queue();
            let fence = Arc::clone(&app.background_delivery.fence);
            sender.push(item(&app));
            app.queue.set_shared(sender.clone());
            assert_eq!(receiver.claim_idle(ORIGINAL_RUN).len(), 1);
            receiver.hold_next_turn();
            let dispatch = Arc::clone(&fence);
            let processing = sender.clone();
            sender.set_dispatch_guard(Arc::new(move || {
                assert!(processing.is_processing());
                dispatch.dispatch_allowed()
            }));
            let (advance, next) = flume::bounded(0);
            let (finalized, finished) = flume::bounded(0);
            let (events, incoming) = flume::unbounded();
            let worker = thread::spawn(move || {
                next.recv().unwrap();
                finalized
                    .send(receiver.finish_run(ORIGINAL_RUN, true))
                    .unwrap();
                next.recv().unwrap();
                EventSender::new(events, ORIGINAL_RUN)
                    .send(AgentEvent::Error {
                        message: RUN_ERROR.into(),
                    })
                    .unwrap();
                receiver
            });
            if finish_before_stop {
                advance.send(()).unwrap();
                assert!(!finished.recv().unwrap());
            }
            assert_eq!(app.queue.is_processing(), !finish_before_stop);
            assert!(matches!(
                app.handle_cancel().as_slice(),
                [Action::CancelAgent {
                    run_id: ORIGINAL_RUN
                }]
            ));
            assert_eq!(
                app.cancelling_run,
                (!finish_before_stop).then_some(ORIGINAL_RUN)
            );
            assert!(!fence.dispatch_allowed());
            if !finish_before_stop {
                advance.send(()).unwrap();
                assert!(!finished.recv().unwrap());
            }
            advance.send(()).unwrap();
            let terminal = incoming.recv().unwrap();
            assert!(matches!(
                &terminal.event,
                AgentEvent::Error { message } if message == RUN_ERROR
            ));
            let receiver = worker.join().unwrap();
            app.update(Msg::Agent(Box::new(terminal)));
            assert_eq!(app.cancelling_run, None);
            assert_eq!(app.status, Status::Idle);
            let reply = app.background_delivery.replies.recv_async().await.unwrap();
            app.apply_delivery_reply(reply).unwrap();
            sender.set_dispatch_guard(Arc::new(move || fence.dispatch_allowed()));
            assert!(matches!(
                app.submit_prompt(prompt()),
                SubmitOutcome::Started(_)
            ));
            let pending = sender.push(item(&app));
            let claimed = receiver.claim_idle(app.run_id);
            assert_eq!(claimed.len(), 1);
            assert_eq!(claimed[0].0, pending);
        });
    }

    #[test_case(false, false, false; "rearm_during_drain")]
    #[test_case(true, false, false; "failed_drain_does_not_rearm")]
    #[test_case(false, true, false; "rearm_after_drain_before_reply")]
    #[test_case(true, true, false; "rearm_before_failure_is_applied")]
    #[test_case(false, true, true; "second_stop_revokes_pending_rearm")]
    #[test_case(true, true, true; "second_stop_cannot_hide_failed_drain")]
    fn stop_reply_serializes_rearm(failed: bool, rearm_after_reply: bool, stop_again: bool) {
        smol::block_on(async {
            let mut app = test_app();
            let fence = Arc::clone(&app.background_delivery.fence);
            let parent = fence.parent.lock().await;
            app.stop_background_work();
            if !rearm_after_reply {
                app.rearm_background();
            }
            assert!(!fence.dispatch_allowed());
            assert!(poll_once(fence.enter()).await.is_none());
            drop(parent);
            let mut reply = app.background_delivery.replies.recv_async().await.unwrap();
            if rearm_after_reply {
                app.rearm_background();
            }
            assert!(app.background_delivery.rearm_after_stop);
            assert!(!fence.dispatch_allowed());
            assert!(poll_once(fence.enter()).await.is_none());
            if stop_again {
                app.stop_background_work();
                assert_eq!(app.background_delivery.stop_id, Some(reply.id));
                assert!(!app.background_delivery.rearm_after_stop);
            }
            if failed {
                reply.result = Err(STOP_ERROR.into());
            }
            assert_eq!(
                app.apply_delivery_reply(reply),
                if failed {
                    Err(DeliveryFailure::Stop(STOP_ERROR.into()))
                } else {
                    Ok(())
                }
            );
            assert!(!app.background_delivery.rearm_after_stop);
            assert_eq!(fence.admission_error(), failed.then_some(STOP_FAILED_ERR));
            assert_eq!(fence.dispatch_allowed(), !failed);
            assert_eq!(app.background_delivery.rearm(), !failed);
            assert_eq!(fence.dispatch_allowed(), !failed);
        });
    }

    #[test_case(1, Duration::from_secs(1); "first_failure_waits_a_second")]
    #[test_case(2, Duration::from_secs(2); "second_failure_doubles")]
    #[test_case(3, Duration::from_secs(4); "third_failure_doubles_again")]
    #[test_case(5, Duration::from_secs(16); "fifth_failure")]
    #[test_case(6, DELIVERY_RETRY_MAX; "sixth_failure_is_capped")]
    #[test_case(40, DELIVERY_RETRY_MAX; "shift_overflow_stays_capped")]
    fn failed_saves_back_off_doubling_to_the_cap(failures: u32, delay: Duration) {
        let mut delivery = BackgroundDelivery::default();
        let now = Instant::now();
        for _ in 0..failures {
            delivery.record_failure(now);
        }
        assert_eq!(delivery.retry_at(), Some(now + delay));
        assert!(delivery.retry_pending(now + delay - Duration::from_millis(1)));
        assert!(!delivery.retry_pending(now + delay));
    }

    #[test_case(false; "invalidate_keeps_the_backoff")]
    #[test_case(true; "rearm_clears_the_backoff")]
    fn only_rearm_clears_a_pending_retry(rearm: bool) {
        let mut delivery = BackgroundDelivery::default();
        let now = Instant::now();
        delivery.record_failure(now);
        if rearm {
            assert!(delivery.rearm());
        } else {
            delivery.invalidate();
        }
        assert_eq!(delivery.retry_pending(now), !rearm);
    }

    fn notices(app: &mut App, text: &str) -> usize {
        let chat = app.main_chat();
        (0..chat.message_count())
            .filter_map(|index| chat.message_at(index))
            .filter(|message| message.role == DisplayRole::Notice && message.text == text)
            .count()
    }

    #[test_case(1, false; "first_failure_only_flashes")]
    #[test_case(DELIVERY_NOTICE_AFTER - 1, false; "notice_waits_for_repeated_failures")]
    #[test_case(DELIVERY_NOTICE_AFTER, true; "notice_after_repeated_failures")]
    #[test_case(DELIVERY_NOTICE_AFTER + 3, true; "notice_shown_once")]
    fn failed_saves_retry_without_closing_admission(failures: u32, noticed: bool) {
        let mut app = test_app();
        let now = Instant::now();
        for _ in 0..failures {
            app.delivery_failed(SAVE_SITE, DeliveryFailure::Save(SAVE_ERROR.into()), now);
        }
        assert!(!app.automatic_wakes_suppressed);
        assert!(app.background_delivery.fence.dispatch_allowed());
        assert!(app.background_delivery.retry_pending(now));
        assert_eq!(
            app.status_bar.flash_text(),
            Some(format!("{DELIVERY_RETRY_FLASH}: {SAVE_ERROR}").as_str())
        );
        assert_eq!(
            notices(&mut app, DELIVERY_RETRY_NOTICE),
            usize::from(noticed)
        );
        app.delivery_recovered();
        app.delivery_recovered();
        assert!(!app.background_delivery.retry_pending(now));
        assert_eq!(
            notices(&mut app, DELIVERY_RECOVERED_NOTICE),
            usize::from(noticed)
        );
    }

    #[test]
    fn failed_stop_still_latches() {
        let mut app = test_app();
        let now = Instant::now();
        app.delivery_failed(SAVE_SITE, DeliveryFailure::Stop(STOP_ERROR.into()), now);
        assert!(app.automatic_wakes_suppressed);
        assert!(!app.background_delivery.retry_pending(now));
        assert_eq!(app.status_bar.flash_text(), Some(STOP_ERROR));
    }

    #[test]
    fn failed_delivery_reply_is_a_retried_save() {
        let mut app = test_app();
        let key = DeliveryKey {
            session: app.state.session.id,
            run: app.run_id,
            revision: app.state.session.content_revision(),
            epoch: app.background_delivery.fence.epoch(),
            generation: None,
            final_save: false,
            claims: Vec::new(),
            pending: Vec::new(),
        };
        app.background_delivery.running = Some((ORIGINAL_RUN, key.clone()));
        let reply = DeliveryReply {
            id: ORIGINAL_RUN,
            stop: false,
            key,
            messages: Vec::new(),
            result: Err(SAVE_ERROR.into()),
        };
        assert_eq!(
            app.apply_delivery_reply(reply),
            Err(DeliveryFailure::Save(SAVE_ERROR.into()))
        );
    }

    #[test]
    fn cancel_during_backoff_still_latches() {
        let mut app = test_app();
        let now = Instant::now();
        app.delivery_failed(SAVE_SITE, DeliveryFailure::Save(SAVE_ERROR.into()), now);
        app.stop_background_work();
        assert!(app.automatic_wakes_suppressed);
        assert!(!app.background_delivery.retry_pending(now));
    }
}
