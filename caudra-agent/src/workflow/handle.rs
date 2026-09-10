//! The cheap, cloneable face of a session's workflow runtime: requests go
//! over a channel and answer asynchronously; the read model is a lock-free
//! swap the UI and the `workflow` tool can load on every frame.

use std::sync::Arc;

use arc_swap::ArcSwap;
use caudra_workflow::{RunStatus, WorkflowError, WorkflowRequest, WorkflowResponse, WorkflowState};

pub(crate) type Reply = flume::Sender<Result<WorkflowResponse, WorkflowError>>;
pub(crate) type RequestSender = flume::Sender<(WorkflowRequest, Reply)>;

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
        let (requests, inbox) = flume::unbounded::<(WorkflowRequest, Reply)>();
        smol::spawn(async move {
            while let Ok((request, reply)) = inbox.recv_async().await {
                let _ = reply.send(answer(request));
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
