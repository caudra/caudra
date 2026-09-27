use std::{
    any::Any,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use async_trait::async_trait;
use caudra_agent::{
    CancelToken, CancelTrigger, Envelope, EventSender,
    permissions::{PermissionManager, PluginRuleStore},
    workspace_transfer::{
        CleanBufferLease, ComparisonRow, InventoryContext, LocalRootIdentity, PlanReview,
        PullBufferGuard, TransferAction, TransferError, TransferEvent, TransferEvents,
        TransferPreview as FileComparisonPreview,
    },
};
use caudra_config::load_permissions;
use caudra_config::sandbox::{Revision, SandboxName};
use caudra_storage::{StateDir, id::CaudraId, workspace_binding::StoredWorkspaceBinding};
use caudra_workcell::{TransferReport, TransferSession, TransferSessionHost};
use caudra_workspace::{TransferDigest, WorkspacePath};
use flume::{Receiver, Sender};
use futures_lite::future;
use smol::Timer;

static TRANSFER_ACTIVE: AtomicBool = AtomicBool::new(false);
const EVENT_CAPACITY: usize = 512;
const VALIDITY_INTERVAL: Duration = Duration::from_secs(2);

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
    pub attached_binding: Option<StoredWorkspaceBinding>,
}

pub type TransferConnector = Arc<
    dyn Fn(TransferLink, TransferSessionHost) -> Result<TransferConnection, String> + Send + Sync,
>;

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct TransferScope {
    pub conversation: CaudraId,
    pub binding: StoredWorkspaceBinding,
    pub name: SandboxName,
    pub instance_revision: Revision,
    pub configuration_revision: Revision,
    pub generation: u64,
}

pub(crate) enum TransferCommand {
    Open {
        scope: Box<TransferScope>,
        link: Box<TransferLink>,
    },
    Compare,
    Inspect(WorkspacePath),
    Review(TransferAction, Vec<WorkspacePath>),
    Execute(TransferDigest),
    Reconcile,
}

pub(crate) enum TransferReply {
    Compared(Arc<ComparisonView>, serde_json::Value, bool),
    Reviewed(Arc<TransferPreview>),
    Inspected(Box<FileComparisonPreview>),
    Finished(Box<TransferReport>),
    Failed(String),
    Closed,
}

pub(crate) struct ComparisonView {
    pub context: InventoryContext,
    pub rows: Vec<ComparisonRow>,
    pub complete: bool,
}

pub(crate) struct TransferPreview {
    pub digest: TransferDigest,
    pub review: PlanReview,
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
    pub scope: TransferScope,
    pub directory_effects: bool,
    pub recovery_required: bool,
    pub completed: usize,
    pub total: usize,
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
        scope: TransferScope,
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
                        'commands: loop {
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
                                                connection.session.supports_directory_publication(),
                                            ))
                                        },
                                    )
                                }
                                TransferCommand::Review(action, paths) => connection
                                    .session
                                    .review_selection(action, &paths, &token)
                                    .await
                                    .map(|plan| {
                                        TransferReply::Reviewed(Arc::new(TransferPreview {
                                            digest: plan.digest().clone(),
                                            review: plan.review().clone(),
                                        }))
                                    }),
                                TransferCommand::Inspect(path) => connection
                                    .session
                                    .preview(&path, &token)
                                    .await
                                    .map(|preview| TransferReply::Inspected(Box::new(preview))),
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
                                TransferCommand::Open { .. } => break,
                            };
                            let refresh = matches!(result, Ok(TransferReply::Finished(_)));
                            let _ =
                                reply_tx.send(result.unwrap_or_else(|error| {
                                    TransferReply::Failed(error.to_string())
                                }));
                            if token.is_cancelled() {
                                break;
                            }
                            if refresh {
                                command = TransferCommand::Compare;
                                continue;
                            }
                            loop {
                                let next = token
                                    .race(future::or(
                                        async { Some(receiver.recv_async().await) },
                                        async {
                                            Timer::after(VALIDITY_INTERVAL).await;
                                            None
                                        },
                                    ))
                                    .await;
                                match next {
                                    Ok(Some(Ok(next))) => {
                                        command = next;
                                        break;
                                    }
                                    Ok(None) => {
                                        if let Err(error) = (connection.validate)() {
                                            let _ = reply_tx.send(TransferReply::Failed(error));
                                            break 'commands;
                                        }
                                    }
                                    _ => break 'commands,
                                }
                            }
                        }
                    }),
                    Err(error) => {
                        let _ = reply_tx.send(TransferReply::Failed(error));
                    }
                }
                drop(_lease);
                let _ = reply_tx.send(TransferReply::Closed);
            })
            .map_err(|error| error.to_string())?;
        Ok(Self {
            scope,
            directory_effects: false,
            recovery_required: false,
            completed: 0,
            total: 0,
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

    pub fn settled(&self) -> bool {
        self.join.as_ref().is_none_or(JoinHandle::is_finished)
    }

    #[cfg(test)]
    pub fn finish(mut self) {
        self.cancel();
        if let Some(join) = self.join.take() {
            join.join().unwrap();
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
        if let Some(join) = self.join.take().filter(JoinHandle::is_finished) {
            let _ = join.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CancelToken, TransferScope, TransferWorker};
    use caudra_agent::permissions::PermissionManager;
    use caudra_config::{
        PermissionsConfig,
        sandbox::{Revision, SandboxName},
    };
    use caudra_storage::{id::CaudraId, workspace_binding::StoredWorkspaceBinding};
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
    fn drop_requests_cancel_without_blocking_cleanup() {
        let (cancel, token) = CancelToken::new();
        let (release, cleanup) = flume::bounded(1);
        let (cancelled, observed) = flume::bounded(1);
        let (settled, finished) = flume::bounded(1);
        let done = Arc::new(AtomicBool::new(false));
        let completed = done.clone();
        let join = thread::spawn(move || {
            smol::block_on(token.cancelled());
            cancelled.send(()).unwrap();
            cleanup.recv().unwrap();
            completed.store(true, Ordering::Release);
            settled.send(()).unwrap();
        });
        let (commands, _) = flume::bounded(1);
        let (_, replies) = flume::unbounded();
        let (_, progress) = flume::unbounded();
        let mut worker = TransferWorker {
            directory_effects: false,
            recovery_required: false,
            completed: 0,
            total: 0,
            scope: TransferScope {
                conversation: CaudraId::generate(),
                binding: StoredWorkspaceBinding::local_from_cwd("/tmp"),
                name: SandboxName::parse("test").unwrap(),
                instance_revision: Revision::parse(REVISION).unwrap(),
                configuration_revision: Revision::parse(REVISION).unwrap(),
                generation: 1,
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
        assert!(!worker.settled());
        drop(worker);
        release.send(()).unwrap();
        finished.recv().unwrap();
        assert!(done.load(Ordering::Acquire));
    }
}
