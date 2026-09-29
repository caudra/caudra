//! The cheap, cloneable face of a session's workflow runtime: requests go
//! over a channel and answer asynchronously; the read model is a lock-free
//! swap the UI and the `workflow` tool can load on every frame.

use std::sync::{Arc, OnceLock};

use crate::background::{BackgroundTasks, SessionWork};
use crate::background_reminder::{RuntimeHealth, RuntimeSnapshot};
use crate::remote_project_context::RemoteProjectContext;
use arc_swap::ArcSwap;
use caudra_providers::WorkflowEventOrigin;
use caudra_workflow::{RunStatus, WorkflowError, WorkflowRequest, WorkflowResponse, WorkflowState};
use caudra_workspace::WorkspaceSession;

const INVALID_RECEIPT_RESPONSE: &str = "invalid workflow receipt response";

pub struct WorkspaceRebind {
    pub workspace: WorkspaceSession,
    pub context: Arc<RemoteProjectContext>,
    pub cwd: String,
}

pub(crate) enum RuntimeRequest {
    BindBackground(BackgroundTasks),
    ReceivedCompletion(WorkflowEventOrigin),
    Workflow(WorkflowRequest, Option<u64>),
    #[cfg(test)]
    Park(flume::Sender<()>, flume::Receiver<()>),
    Suspend(Arc<()>),
    Rebind(Arc<()>, WorkspaceRebind),
    Commit(Arc<()>),
    Release(Arc<()>),
}

pub(crate) type Reply = flume::Sender<Result<WorkflowResponse, WorkflowError>>;
pub(crate) type RequestSender = flume::Sender<(RuntimeRequest, Reply)>;

pub struct WorkflowTransition {
    handle: WorkflowHandle,
    token: Arc<()>,
}

impl WorkflowTransition {
    pub async fn commit(self) -> Result<(), WorkflowError> {
        self.handle
            .send(RuntimeRequest::Commit(Arc::clone(&self.token)))
            .await
            .map(|_| ())
    }
    pub async fn rebind(&self, workspace: WorkspaceRebind) -> Result<(), WorkflowError> {
        self.handle
            .send(RuntimeRequest::Rebind(Arc::clone(&self.token), workspace))
            .await
            .map(|_| ())
    }
}

impl Drop for WorkflowTransition {
    fn drop(&mut self) {
        let (reply, _) = flume::bounded(1);
        let _ = self
            .handle
            .requests
            .try_send((RuntimeRequest::Release(Arc::clone(&self.token)), reply));
    }
}

#[derive(Clone)]
pub struct WorkflowHandle {
    requests: RequestSender,
    state: Arc<ArcSwap<WorkflowState>>,
    pub(super) background: Arc<OnceLock<BackgroundTasks>>,
    pub(super) health: Arc<ArcSwap<RuntimeHealth>>,
}

impl WorkflowHandle {
    pub(crate) fn new(requests: RequestSender, state: Arc<ArcSwap<WorkflowState>>) -> Self {
        Self {
            requests,
            state,
            background: Arc::new(OnceLock::new()),
            health: Arc::new(ArcSwap::from_pointee(RuntimeHealth::Current)),
        }
    }

    /// A handle whose runtime is `answer`, for testing callers in isolation.
    #[cfg(test)]
    pub(crate) fn scripted(
        answer: impl Fn(WorkflowRequest) -> Result<WorkflowResponse, WorkflowError> + Send + 'static,
    ) -> Self {
        let (requests, inbox) = flume::unbounded::<(RuntimeRequest, Reply)>();
        smol::spawn(async move {
            while let Ok((request, reply)) = inbox.recv_async().await {
                let result = match request {
                    RuntimeRequest::Workflow(request, _) => answer(request),
                    _ => Ok(WorkflowResponse::Ack),
                };
                let _ = reply.send(result);
            }
        })
        .detach();
        Self::new(
            requests,
            Arc::new(ArcSwap::from_pointee(WorkflowState::default())),
        )
    }

    /// `Unavailable` once the runtime has shut down or died.
    pub async fn request(
        &self,
        request: WorkflowRequest,
    ) -> Result<WorkflowResponse, WorkflowError> {
        let generation = if matches!(
            request,
            WorkflowRequest::Start(_) | WorkflowRequest::Resume { .. }
        ) {
            self.background.get().map(BackgroundTasks::generation)
        } else {
            None
        };
        self.send(RuntimeRequest::Workflow(request, generation))
            .await
    }

    #[cfg(test)]
    pub(super) async fn park(
        &self,
        entered: flume::Sender<()>,
        release: flume::Receiver<()>,
    ) -> Result<WorkflowResponse, WorkflowError> {
        self.send(RuntimeRequest::Park(entered, release)).await
    }

    pub async fn suspend(&self) -> Result<WorkflowTransition, WorkflowError> {
        let transition = WorkflowTransition {
            handle: self.clone(),
            token: Arc::new(()),
        };
        self.send(RuntimeRequest::Suspend(Arc::clone(&transition.token)))
            .await?;
        Ok(transition)
    }

    pub async fn bind_background(&self, background: BackgroundTasks) -> Result<(), WorkflowError> {
        self.send(RuntimeRequest::BindBackground(background))
            .await
            .map(|_| ())
    }

    pub async fn received_completion(
        &self,
        origin: WorkflowEventOrigin,
    ) -> Result<bool, WorkflowError> {
        match self
            .send(RuntimeRequest::ReceivedCompletion(origin))
            .await?
        {
            WorkflowResponse::Acked(received) => Ok(received),
            _ => Err(WorkflowError::Internal(INVALID_RECEIPT_RESPONSE.into())),
        }
    }

    async fn send(&self, request: RuntimeRequest) -> Result<WorkflowResponse, WorkflowError> {
        let (reply, answer) = flume::bounded(1);
        self.requests
            .send_async((request, reply))
            .await
            .map_err(|_| WorkflowError::Unavailable)?;
        answer
            .recv_async()
            .await
            .map_err(|_| WorkflowError::Unavailable)?
    }

    pub fn state(&self) -> Arc<WorkflowState> {
        self.state.load_full()
    }

    pub(crate) fn reminder_snapshot(&self) -> RuntimeSnapshot {
        let state = self.state();
        let mut health = self.health.load().as_ref().clone();
        if self.requests.is_disconnected() && health != RuntimeHealth::Closed {
            health = RuntimeHealth::Unavailable;
        }
        let mut snapshot = RuntimeSnapshot::new(health, !state.runs.is_empty());
        for run in state
            .runs
            .iter()
            .filter(|run| run.status == RunStatus::Active)
        {
            snapshot.add(
                "workflow",
                &run.run_id,
                "running",
                &run.workflow_name,
                run.phase.as_deref(),
            );
        }
        snapshot
    }

    /// Runs whose script is executing right now.
    pub fn active_count(&self) -> usize {
        self.state
            .load()
            .runs
            .iter()
            .filter(|run| run.status == RunStatus::Active)
            .count()
    }

    /// `Stopping` alone is not work: a suspended runtime has already joined
    /// its runs, and one shutting down keeps each run active until it ends.
    pub(crate) fn work(&self) -> SessionWork {
        let state = self.state.load();
        let health = self.health.load();
        SessionWork {
            running: state.runs.iter().any(|run| run.status == RunStatus::Active),
            settling: state.runs.iter().any(|run| run.outbox_pending),
            unavailable: **health == RuntimeHealth::Unavailable
                || (self.requests.is_disconnected() && **health != RuntimeHealth::Closed),
        }
    }

    /// Runs whose latest state nobody has acknowledged yet.
    pub fn pending_completions(&self) -> usize {
        self.state
            .load()
            .runs
            .iter()
            .filter(|run| run.outbox_pending)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::{RuntimeRequest, WorkflowHandle};
    use crate::background_reminder::RuntimeHealth;
    use arc_swap::ArcSwap;
    use caudra_workflow::WorkflowState;
    use std::sync::Arc;

    #[test]
    fn read_only_health_distinguishes_closed_and_disconnected() {
        let (requests, received) = flume::unbounded();
        let handle = WorkflowHandle::new(
            requests,
            Arc::new(ArcSwap::from_pointee(WorkflowState::default())),
        );
        assert_eq!(handle.reminder_snapshot().health, RuntimeHealth::Current);
        handle.health.store(Arc::new(RuntimeHealth::Stopping));
        assert_eq!(handle.reminder_snapshot().health, RuntimeHealth::Stopping);
        assert!(received.is_empty());
        drop(received);
        assert_eq!(
            handle.reminder_snapshot().health,
            RuntimeHealth::Unavailable
        );
        handle.health.store(Arc::new(RuntimeHealth::Closed));
        assert_eq!(handle.reminder_snapshot().health, RuntimeHealth::Closed);
    }

    #[test]
    fn cancelling_suspend_releases_only_its_own_reservation() {
        smol::block_on(async {
            let (requests, received) = flume::unbounded();
            let handle = WorkflowHandle::new(
                requests,
                Arc::new(ArcSwap::from_pointee(WorkflowState::default())),
            );
            let mut suspend = Box::pin(handle.suspend());
            assert!(
                futures_lite::future::poll_once(&mut suspend)
                    .await
                    .is_none()
            );
            let (RuntimeRequest::Suspend(token), _) = received.recv().unwrap() else {
                panic!("suspend request");
            };
            drop(suspend);
            let (RuntimeRequest::Release(released), _) = received.recv().unwrap() else {
                panic!("release request");
            };
            assert!(Arc::ptr_eq(&token, &released));
        });
    }
}
