use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use async_lock::{Mutex, MutexGuard};
use caudra_agent::Nudge;
use caudra_agent::background::BackgroundTasks;
use caudra_agent::workflow::WorkflowHandle;
use caudra_providers::{Message, WorkflowEventOrigin};
use caudra_storage::id::CaudraId;
use caudra_workflow::{RunStatus, WorkflowRequest};

use super::App;
use super::workflow::{WorkflowDelivery, WorkflowDeliveryResult};

#[derive(Default)]
pub(crate) struct DeliveryFence {
    pub(crate) parent: Mutex<()>,
    epoch: AtomicU64,
    rearm_after_stop: AtomicBool,
    stopping: AtomicBool,
    stopped: Nudge,
}

impl DeliveryFence {
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
    running: Option<(u64, DeliveryKey)>,
    queued: Option<DeliveryJob>,
    completed: Option<DeliveryKey>,
    sequence: u64,
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
        }
    }
}

impl BackgroundDelivery {
    pub(crate) fn invalidate(&mut self) {
        self.fence.epoch.fetch_add(1, Ordering::AcqRel);
        self.fence.rearm_after_stop.store(false, Ordering::Release);
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

    pub(super) fn rearm(&mut self) -> bool {
        self.invalidate();
        if self.stop_id.is_none() {
            return true;
        }
        self.fence.rearm_after_stop.store(true, Ordering::Release);
        !self.fence.stopping.load(Ordering::Acquire)
            && self.fence.rearm_after_stop.swap(false, Ordering::AcqRel)
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
        self.fence.rearm_after_stop.store(false, Ordering::Release);
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
            fence.stopping.store(false, Ordering::Release);
            if fence.rearm_after_stop.swap(false, Ordering::AcqRel)
                && let Some(background) = background
            {
                background.rearm();
            }
            fence.stopped.notify();
            let _ = sender.send(DeliveryReply {
                id,
                stop: true,
                key,
                messages: Vec::new(),
                result: result.map(|()| WorkflowDeliveryResult::default()),
            });
        }));
    }

    fn finish(&mut self, reply: &DeliveryReply) -> bool {
        if reply.stop {
            if self.stop_id != Some(reply.id) {
                return false;
            }
            self.stop_id = None;
            self.stop_task = None;
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
        self.suppress_background_wakes();
        let _ = self.poll_background_delivery();
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

    pub(crate) fn apply_delivery_reply(&mut self, reply: DeliveryReply) -> Result<(), String> {
        if !self.background_delivery.finish(&reply)
            || reply.key.session != self.state.session.id
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
            return reply.result.map(|_| ());
        }
        if reply.key.revision > current_revision {
            return Ok(());
        }
        let result = match reply.result {
            Ok(result) => result,
            Err(error) if reply.key.revision == current_revision => return Err(error),
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
        Ok(())
    }

    pub(crate) fn poll_background_delivery(&mut self) -> Result<(), String> {
        while let Ok(reply) = self.background_delivery.replies.try_recv() {
            self.apply_delivery_reply(reply)?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn flush_background_delivery(
        &mut self,
        final_save: bool,
    ) -> Result<(), String> {
        self.persist_background_delivery(final_save)?;
        while self.background_delivery.pending() {
            let reply = self
                .background_delivery
                .replies
                .recv_async()
                .await
                .map_err(|error| error.to_string())?;
            self.apply_delivery_reply(reply)?;
        }
        Ok(())
    }
}
