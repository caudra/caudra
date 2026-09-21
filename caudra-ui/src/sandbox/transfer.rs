use std::{
    any::Any,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
};

use async_trait::async_trait;
use caudra_agent::{
    CancelToken, CancelTrigger, Envelope, EventSender,
    permissions::{PermissionManager, PluginRuleStore},
    workspace_transfer::{
        CleanBufferLease, ComparisonRow, InventoryContext, LocalRootIdentity, PlanReview,
        PullBufferGuard, TransferAction, TransferError, TransferEvent, TransferEvents,
    },
};
use caudra_config::load_permissions;
use caudra_config::sandbox::{Revision, SandboxName};
use caudra_storage::StateDir;
use caudra_workcell::{TransferReport, TransferSession, TransferSessionHost};
use caudra_workspace::{TransferDigest, WorkspacePath};
use flume::{Receiver, Sender};

use super::SandboxSnapshotRequest;

static TRANSFER_ACTIVE: AtomicBool = AtomicBool::new(false);
const EVENT_CAPACITY: usize = 512;

pub fn active() -> bool {
    TRANSFER_ACTIVE.load(Ordering::Acquire)
}

struct WorkspaceLease;
impl Drop for WorkspaceLease {
    fn drop(&mut self) {
        TRANSFER_ACTIVE.store(false, Ordering::Release);
    }
}

pub struct TransferConnection {
    pub session: TransferSession,
    pub lifetime: Box<dyn Any + Send>,
    pub validate: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
}

#[derive(Clone)]
pub struct TransferLink {
    pub name: SandboxName,
    pub instance_revision: Revision,
    pub configuration_revision: Revision,
    pub local_root: PathBuf,
    pub remote_root: WorkspacePath,
}

pub type TransferConnector = Arc<
    dyn Fn(TransferLink, TransferSessionHost) -> Result<TransferConnection, String> + Send + Sync,
>;

pub(crate) enum TransferCommand {
    Open {
        scope: SandboxSnapshotRequest,
        link: TransferLink,
    },
    Compare,
    Review(TransferAction, Vec<WorkspacePath>),
    Execute(TransferDigest),
    Reconcile,
    Close,
}

pub(crate) enum TransferReply {
    Compared(Arc<ComparisonView>, serde_json::Value),
    Reviewed(Arc<TransferPreview>),
    Finished(Box<TransferReport>),
    Failed(String),
    Closed,
}

pub(crate) struct ComparisonView {
    pub context: InventoryContext,
    pub rows: Vec<ComparisonRow>,
    pub complete: bool,
}

impl ComparisonView {
    pub fn context(&self) -> &InventoryContext {
        &self.context
    }
    pub fn rows(&self) -> &[ComparisonRow] {
        &self.rows
    }
    pub fn complete(&self) -> bool {
        self.complete
    }
}

pub(crate) struct TransferPreview {
    pub digest: TransferDigest,
    pub review: PlanReview,
}

impl TransferPreview {
    pub fn digest(&self) -> &TransferDigest {
        &self.digest
    }
    pub fn review(&self) -> &PlanReview {
        &self.review
    }
}

struct Progress(Sender<TransferEvent>);
impl TransferEvents for Progress {
    fn emit(&self, event: TransferEvent) {
        let _ = self.0.try_send(event);
    }
}

pub struct TransferBuffers;
struct BufferLease;
impl CleanBufferLease for BufferLease {}
#[async_trait]
impl PullBufferGuard for TransferBuffers {
    async fn lock_clean(
        &self,
        _: &LocalRootIdentity,
        _: &WorkspacePath,
    ) -> Result<Box<dyn CleanBufferLease>, TransferError> {
        if !active() {
            return Err(TransferError::Stale);
        }
        Ok(Box::new(BufferLease))
    }
}

pub(crate) struct TransferWorker {
    pub scope: SandboxSnapshotRequest,
    pub replies: Receiver<TransferReply>,
    pub progress: Receiver<TransferEvent>,
    pub permissions: Arc<PermissionManager>,
    pub permission_events: Receiver<Envelope>,
    commands: Sender<TransferCommand>,
    cancel: Option<CancelTrigger>,
    join: Option<JoinHandle<()>>,
}

impl TransferWorker {
    pub fn start(
        scope: SandboxSnapshotRequest,
        mut link: TransferLink,
        connector: TransferConnector,
        state: &StateDir,
    ) -> Result<Self, String> {
        if !link.local_root.is_absolute() {
            return Err(TransferError::LocalRoot.to_string());
        }
        link.local_root = LocalRootIdentity::capture(&link.local_root)
            .map_err(|error| error.to_string())?
            .canonical_path()
            .to_owned();
        let permissions = Arc::new(transfer_permissions(link.local_root.clone(), state));
        let (events, permission_events) = flume::unbounded();
        if TRANSFER_ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err("Another transfer owns the local buffer guard".into());
        }
        let lease = WorkspaceLease;
        let (cancel, token) = CancelToken::new();
        let (progress_tx, progress) = flume::bounded(EVENT_CAPACITY);
        let validity_token = token.clone();
        let host = TransferSessionHost {
            permissions: permissions.clone(),
            permission_events: EventSender::new(events, 0),
            cancel: token.clone(),
            progress: Arc::new(Progress(progress_tx)),
            buffers: Arc::new(TransferBuffers),
            validity: Arc::new(move || {
                if validity_token.is_cancelled() {
                    Err("Transfer window closed".into())
                } else {
                    Ok(())
                }
            }),
        };
        let (reply_tx, replies) = flume::unbounded();
        let (commands, receiver) = flume::bounded(1);
        let join = thread::Builder::new()
            .name("workspace-transfer".into())
            .spawn(move || {
                let _lease = lease;
                let result = connector(link, host);
                match result {
                    Ok(mut connection) => smol::block_on(async {
                        let mut command = TransferCommand::Compare;
                        loop {
                            if let Err(error) = (connection.validate)() {
                                let _ = reply_tx.send(TransferReply::Failed(error));
                                break;
                            }
                            let result = match command {
                                TransferCommand::Compare => {
                                    connection.session.compare(&token).await.and_then(
                                        |comparison| {
                                            Ok(TransferReply::Compared(
                                                Arc::new(ComparisonView {
                                                    context: comparison.context().clone(),
                                                    rows: comparison.rows().to_vec(),
                                                    complete: comparison.complete(),
                                                }),
                                                connection.session.recovery()?,
                                            ))
                                        },
                                    )
                                }
                                TransferCommand::Review(action, paths) => {
                                    connection.session.review(action, &paths, &token).await.map(
                                        |plan| {
                                            TransferReply::Reviewed(Arc::new(TransferPreview {
                                                digest: plan.digest().clone(),
                                                review: plan.review().clone(),
                                            }))
                                        },
                                    )
                                }
                                TransferCommand::Execute(digest) => connection
                                    .session
                                    .execute(&digest, &token)
                                    .await
                                    .map(|report| TransferReply::Finished(Box::new(report))),
                                TransferCommand::Reconcile => connection
                                    .session
                                    .reconcile(&token)
                                    .await
                                    .map(|report| TransferReply::Finished(Box::new(report))),
                                TransferCommand::Close | TransferCommand::Open { .. } => break,
                            };
                            let _ =
                                reply_tx.send(result.unwrap_or_else(|error| {
                                    TransferReply::Failed(error.to_string())
                                }));
                            if token.is_cancelled() {
                                break;
                            }
                            match token.race(receiver.recv_async()).await {
                                Ok(Ok(next)) => command = next,
                                _ => break,
                            }
                        }
                    }),
                    Err(error) => {
                        let _ = reply_tx.send(TransferReply::Failed(error));
                    }
                }
                let _ = reply_tx.send(TransferReply::Closed);
            })
            .map_err(|error| error.to_string())?;
        Ok(Self {
            scope,
            replies,
            progress,
            permissions,
            permission_events,
            commands,
            cancel: Some(cancel),
            join: Some(join),
        })
    }

    pub fn send(&self, command: TransferCommand) -> Result<(), String> {
        self.commands
            .try_send(command)
            .map_err(|_| "Transfer is busy; wait for settlement".into())
    }

    pub fn cancel(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.cancel();
        }
    }
}

pub fn transfer_permissions(local_root: PathBuf, state: &StateDir) -> PermissionManager {
    PermissionManager::new_persistent_in(
        load_permissions(&local_root),
        local_root,
        Arc::new(PluginRuleStore::default()),
        state.clone(),
    )
}

impl Drop for TransferWorker {
    fn drop(&mut self) {
        self.cancel();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CancelToken, SandboxSnapshotRequest, TransferWorker};
    use caudra_agent::permissions::PermissionManager;
    use caudra_config::{PermissionsConfig, sandbox::Revision};
    use caudra_storage::id::CaudraId;
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread,
    };

    const REVISION: &str =
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn cancel_keeps_worker_owned_until_cleanup_is_awaited() {
        let (cancel, token) = CancelToken::new();
        let (release, cleanup) = flume::bounded(1);
        let (cancelled, observed) = flume::bounded(1);
        let done = Arc::new(AtomicBool::new(false));
        let completed = done.clone();
        let join = thread::spawn(move || {
            smol::block_on(token.cancelled());
            cancelled.send(()).unwrap();
            cleanup.recv().unwrap();
            completed.store(true, Ordering::Release);
        });
        let (commands, _) = flume::bounded(1);
        let (_, replies) = flume::unbounded();
        let (_, progress) = flume::unbounded();
        let mut worker = TransferWorker {
            scope: SandboxSnapshotRequest {
                conversation: CaudraId::generate(),
                manager_session: 1,
                configuration_revision: Revision::parse(REVISION).unwrap(),
                configuration_epoch: 0,
            },
            replies,
            progress,
            permissions: Arc::new(PermissionManager::new_nonpersistent(
                PermissionsConfig::default(),
                std::env::temp_dir(),
                Arc::default(),
            )),
            permission_events: flume::unbounded().1,
            commands,
            cancel: Some(cancel),
            join: Some(join),
        };
        worker.cancel();
        observed.recv().unwrap();
        assert!(!done.load(Ordering::Acquire));
        release.send(()).unwrap();
        drop(worker);
        assert!(done.load(Ordering::Acquire));
    }
}
