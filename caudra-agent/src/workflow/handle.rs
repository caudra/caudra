//! The cheap, cloneable face of a session's workflow runtime: requests go
//! over a channel and answer asynchronously; the read model is a lock-free
//! swap the UI and the `workflow` tool can load on every frame.

use std::sync::Arc;

use crate::remote_project_context::RemoteProjectContext;
use arc_swap::ArcSwap;
use caudra_workflow::{RunStatus, WorkflowError, WorkflowRequest, WorkflowResponse, WorkflowState};
use caudra_workspace::WorkspaceSession;

pub struct WorkspaceRebind {
    pub workspace: WorkspaceSession,
    pub context: Arc<RemoteProjectContext>,
    pub cwd: String,
}

pub(crate) enum RuntimeRequest {
    Workflow(WorkflowRequest),
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
}

impl WorkflowHandle {
    pub(crate) fn new(requests: RequestSender, state: Arc<ArcSwap<WorkflowState>>) -> Self {
        Self { requests, state }
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
                    RuntimeRequest::Workflow(request) => answer(request),
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
        self.send(RuntimeRequest::Workflow(request)).await
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

    /// Runs whose script is executing right now.
    pub fn active_count(&self) -> usize {
        self.state
            .load()
            .runs
            .iter()
            .filter(|run| run.status == RunStatus::Active)
            .count()
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
    use arc_swap::ArcSwap;
    use caudra_workflow::WorkflowState;
    use std::sync::Arc;

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
