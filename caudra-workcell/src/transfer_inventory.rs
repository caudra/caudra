use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use caudra_agent::workspace_transfer::{
    Inspection, InventoryContext, InventoryNode, InventoryPage, LocalRootIdentity, NodeKind,
    OrchestrationLimits, PullBufferGuard, RemoteRootIdentity, Side, TransferAuthorization,
    TransferError, TransferEvents, TransferFilters, TransferInventory, TransferRoots,
    TransferServices, WorkspaceTransfer,
};
use caudra_workspace::{
    CollectionRevision, ContinuationToken, LocalTransferAuthorization, LocalTransferSource,
    ResourceId, ResourceRevision, TransferDigest, WorkspaceError, WorkspacePath,
};
use futures_lite::future;
use smol::{Timer, Unblock};
use tokio_util::sync::CancellationToken;
use workcell::host_contract as contract;

use crate::{
    LocalTransferPublisher, RemoteWorkcellClient,
    transfer::{IO_TIMEOUT, binary_error},
};

/// Plan approval, host-local publication permission, dirty-buffer leases, and events are
/// deliberately separate. This factory supplies no allow-all or remote-to-local approval bridge.
pub struct ReviewedTransferHost {
    pub authorization: Arc<dyn TransferAuthorization>,
    pub local_publication: Arc<dyn LocalTransferAuthorization>,
    pub buffers: Arc<dyn PullBufferGuard>,
    pub events: Arc<dyn TransferEvents>,
}

/// Construct a reviewed transfer for the remote root cursor. Requires Linux descriptor traversal
/// locally and an attested remote inventory capability. The owner-only status path must be outside
/// the local workspace; callers separately choose a TransferJournal path and supply all approvals.
pub async fn reviewed_workspace_transfer(
    local_root: PathBuf,
    local_status_path: PathBuf,
    remote: RemoteWorkcellClient,
    remote_root: RemoteRootIdentity,
    filters: TransferFilters,
    limits: OrchestrationLimits,
    host: ReviewedTransferHost,
) -> Result<WorkspaceTransfer, TransferError> {
    let roots = TransferRoots {
        local: LocalRootIdentity::capture(&local_root)?,
        remote: remote_root,
    };
    host.authorization.roots(&roots).await?;
    let local = Arc::new(
        LocalTransferPublisher::new_durable(
            roots.local.canonical_path().to_path_buf(),
            local_status_path,
            host.local_publication,
        )
        .await?
        .with_max_file_bytes(limits.max_file_bytes)?,
    );
    let inventory = RootedTransferInventory::new(
        roots,
        local.clone(),
        remote.clone(),
        limits.max_file_bytes,
        &filters,
    )?;
    // The remote method checks the server's negotiated safe-inventory capability. A successful
    // ordinary directory listing is never promoted into a safety attestation.
    inventory.context().await?;
    WorkspaceTransfer::new(
        TransferServices {
            inventory: Arc::new(inventory),
            local,
            remote: Arc::new(remote),
            authorization: host.authorization,
            buffers: host.buffers,
            events: host.events,
        },
        filters,
        limits,
    )
}

#[async_trait]
trait RemoteInventorySource: Send + Sync {
    async fn inventory(
        &self,
        root: &RemoteRootIdentity,
        path: Option<&WorkspacePath>,
        policy: &contract::TransferInventoryPolicy,
    ) -> Result<contract::TransferInventoryResponse, WorkspaceError>;
}

#[async_trait]
impl RemoteInventorySource for RemoteWorkcellClient {
    async fn inventory(
        &self,
        root: &RemoteRootIdentity,
        path: Option<&WorkspacePath>,
        policy: &contract::TransferInventoryPolicy,
    ) -> Result<contract::TransferInventoryResponse, WorkspaceError> {
        self.transfer_inventory(&root.binding, &root.cursor, path, policy)
            .await
    }
}

pub struct RootedTransferInventory {
    roots: TransferRoots,
    local: Arc<LocalTransferPublisher>,
    remote: Arc<dyn RemoteInventorySource>,
    max_file_bytes: u64,
    policy: contract::TransferInventoryPolicy,
}

impl RootedTransferInventory {
    pub fn new(
        roots: TransferRoots,
        local: Arc<LocalTransferPublisher>,
        remote: RemoteWorkcellClient,
        max_file_bytes: u64,
        filters: &TransferFilters,
    ) -> Result<Self, TransferError> {
        if roots.local != local.root || roots.remote.binding != *remote.session_binding() {
            return Err(TransferError::Stale);
        }
        Ok(Self {
            roots,
            local,
            remote: Arc::new(remote),
            max_file_bytes,
            policy: contract::TransferInventoryPolicy {
                excludes: filters.inventory_excludes().to_vec(),
                respect_gitignore: filters.respects_gitignore(),
            },
        })
    }

    async fn snapshot(
        &self,
        side: &Side,
        inspect: Option<&WorkspacePath>,
    ) -> Result<contract::TransferInventoryResponse, TransferError> {
        let response = match side {
            Side::Remote => {
                self.remote
                    .inventory(&self.roots.remote, inspect, &self.policy)
                    .await?
            }
            Side::Local => {
                if LocalRootIdentity::capture(self.roots.local.canonical_path())?
                    != self.roots.local
                {
                    return Err(TransferError::Stale);
                }
                let files = self.local.files.clone();
                let cwd = self.local.cwd.clone();
                let inspect = inspect
                    .map(|path| contract::WorkspacePath::new(path.as_str()))
                    .transpose()
                    .map_err(|_| TransferError::UnsafeInventory)?;
                let cancel = CancellationToken::new();
                let _guard = cancel.clone().drop_guard();
                let policy = self.policy.clone();
                let permit = self.local.staging.io()?;
                let task = self.local.runtime.spawn(async move {
                    let _permit = permit;
                    files
                        .transfer_inventory(&cwd, inspect, policy, &cancel)
                        .await
                })?;
                future::race(
                    async {
                        task.await
                            .map_err(|_| WorkspaceError::Unavailable)?
                            .map_err(binary_error)
                    },
                    async {
                        Timer::after(IO_TIMEOUT).await;
                        Err(WorkspaceError::Cancelled)
                    },
                )
                .await?
            }
        };
        if response.entries.len() > contract::MAX_TRANSFER_INVENTORY_ENTRIES
            || response
                .entries
                .windows(2)
                .any(|pair| pair[0].path.as_str() >= pair[1].path.as_str())
        {
            return Err(TransferError::UnsafeInventory);
        }
        Ok(response)
    }
}

#[async_trait]
impl TransferInventory for RootedTransferInventory {
    async fn context(&self) -> Result<InventoryContext, TransferError> {
        let local = self.snapshot(&Side::Local, None).await?;
        let remote = self.snapshot(&Side::Remote, None).await?;
        Ok(InventoryContext {
            roots: self.roots.clone(),
            local_ignore_digest: TransferDigest::new(local.ignore_digest.as_str())?,
            remote_ignore_digest: TransferDigest::new(remote.ignore_digest.as_str())?,
            safe_local_traversal: true,
            safe_remote_traversal: true,
        })
    }

    async fn list(
        &self,
        side: &Side,
        directory: &WorkspacePath,
        continuation: Option<ContinuationToken>,
        limit: u32,
    ) -> Result<InventoryPage, TransferError> {
        if limit == 0 {
            return Err(TransferError::Quota);
        }
        let snapshot = self.snapshot(side, None).await?;
        let revision = CollectionRevision::new(snapshot.revision.as_str())
            .map_err(|_| TransferError::UnsafeInventory)?;
        let offset = match continuation {
            None => 0,
            Some(token) => {
                let (stamp, offset) = token
                    .as_str()
                    .rsplit_once(':')
                    .ok_or(TransferError::Stale)?;
                if stamp != revision.as_str() {
                    return Err(TransferError::Stale);
                }
                offset.parse::<usize>().map_err(|_| TransferError::Stale)?
            }
        };
        let children = snapshot
            .entries
            .iter()
            .map(node)
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|node| node.path.parent().as_ref() == Some(directory))
            .collect::<Vec<_>>();
        if offset > children.len() {
            return Err(TransferError::Stale);
        }
        let end = children.len().min(offset.saturating_add(limit as usize));
        let next = (end < children.len())
            .then(|| ContinuationToken::new(format!("{}:{end}", revision.as_str())))
            .transpose()
            .map_err(|_| TransferError::Stale)?;
        Ok(InventoryPage {
            revision,
            entries: children[offset..end].to_vec(),
            next,
            incomplete: !snapshot.complete,
        })
    }

    async fn inspect(
        &self,
        side: &Side,
        path: &WorkspacePath,
    ) -> Result<Inspection, TransferError> {
        let snapshot = self.snapshot(side, Some(path)).await?;
        if !snapshot.complete {
            return Err(TransferError::UnsafeInventory);
        }
        let inspection = snapshot.inspection.ok_or(TransferError::UnsafeInventory)?;
        if inspection.path.as_str() != path.as_str() {
            return Err(TransferError::UnsafeInventory);
        }
        Ok(Inspection {
            node: inspection.node.as_ref().map(node).transpose()?,
            ignored: Some(inspection.ignored),
        })
    }

    async fn open_local(
        &self,
        root: &LocalRootIdentity,
        path: &WorkspacePath,
        revision: &ResourceRevision,
    ) -> Result<LocalTransferSource, TransferError> {
        if root != &self.roots.local || LocalRootIdentity::capture(root.canonical_path())? != *root
        {
            return Err(TransferError::Stale);
        }
        let files = self.local.files.clone();
        let cwd = self.local.cwd.clone();
        let path = contract::WorkspacePath::new(path.as_str())
            .map_err(|_| TransferError::UnsafeInventory)?;
        let maximum = self.max_file_bytes;
        let cancel = CancellationToken::new();
        let _guard = cancel.clone().drop_guard();
        let permit = self.local.staging.io()?;
        let task = self.local.runtime.spawn(async move {
            let _permit = permit;
            files.open_binary(&cwd, &path, maximum, &cancel).await
        })?;
        let file = future::race(
            async {
                task.await
                    .map_err(|_| WorkspaceError::Unavailable)?
                    .map_err(binary_error)
            },
            async {
                Timer::after(IO_TIMEOUT).await;
                Err(WorkspaceError::Cancelled)
            },
        )
        .await?;
        if file.metadata.revision.as_str() != revision.as_str() {
            return Err(TransferError::Stale);
        }
        Ok(LocalTransferSource::new(Unblock::new(file.file)))
    }
}

fn node(node: &contract::TransferInventoryNode) -> Result<InventoryNode, TransferError> {
    Ok(InventoryNode {
        path: WorkspacePath::new(node.path.as_str())?,
        identity: ResourceId::new(node.resource_id.as_str())
            .map_err(|_| TransferError::UnsafeInventory)?,
        revision: ResourceRevision::new(node.revision.as_str())
            .map_err(|_| TransferError::UnsafeInventory)?,
        kind: match node.kind {
            contract::TransferNodeKind::File => NodeKind::File,
            contract::TransferNodeKind::Directory => NodeKind::Directory,
            contract::TransferNodeKind::Symlink => NodeKind::Symlink,
            contract::TransferNodeKind::Mount => NodeKind::Mount,
            contract::TransferNodeKind::Special => NodeKind::Special,
            contract::TransferNodeKind::NestedRepository => NodeKind::NestedRepository,
        },
        size_bytes: node.size_bytes,
        ignored: Some(node.ignored),
    })
}

#[cfg(test)]
mod tests {
    use super::{RemoteInventorySource, RootedTransferInventory};
    use crate::LocalTransferPublisher;
    use async_trait::async_trait;
    use caudra_agent::workspace_transfer::{
        LocalRootIdentity, RemoteRootIdentity, Side, TransferInventory, TransferRoots,
    };
    use caudra_workspace::{
        AuthenticatedPrincipalId, AuthorityIdentity, CwdHandle, LocalTransferAuthorization,
        LocalTransferPath, LocalTransferReview, LocalTransferService, ProjectIdentity, ProjectKey,
        ResourceId, ResourceScope, SessionBindingId, SessionWorkspaceBinding, SourceTrustAnchor,
        WorkspaceCursor, WorkspaceError, WorkspacePath,
    };
    use futures_lite::io::AsyncReadExt;
    use std::{fs, sync::Arc};
    use workcell::host_contract as contract;

    const CONTENT: &[u8] = b"\xff\0descriptor-bound source";
    const OTHER: &[u8] = b"changed";
    const DIGEST: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000000";

    struct EmptyRemote;
    #[async_trait]
    impl RemoteInventorySource for EmptyRemote {
        async fn inventory(
            &self,
            _: &RemoteRootIdentity,
            path: Option<&WorkspacePath>,
            _: &contract::TransferInventoryPolicy,
        ) -> Result<contract::TransferInventoryResponse, WorkspaceError> {
            Ok(contract::TransferInventoryResponse {
                version: contract::ContractVersion::V1,
                entries: Vec::new(),
                revision: contract::Revision::new(DIGEST).unwrap(),
                ignore_digest: contract::Revision::new(DIGEST).unwrap(),
                complete: true,
                inspection: path.map(|path| contract::TransferInspection {
                    path: contract::WorkspacePath::new(path.as_str()).unwrap(),
                    node: None,
                    ignored: path.as_str() == "remote-ignored",
                }),
            })
        }
    }
    #[async_trait]
    impl LocalTransferAuthorization for EmptyRemote {
        async fn authorize(&self, _: &LocalTransferReview) -> Result<(), WorkspaceError> {
            Err(WorkspaceError::PermissionDenied)
        }
    }

    fn remote_root() -> RemoteRootIdentity {
        let authority = AuthorityIdentity::new(
            SourceTrustAnchor::new("inventory-test").unwrap(),
            "server",
            "workspace",
            "generation",
            "namespace",
        )
        .unwrap();
        let principal = AuthenticatedPrincipalId::new(authority.clone(), "principal").unwrap();
        let project = ProjectIdentity::new(authority.clone(), ProjectKey::new("project").unwrap());
        let binding = SessionWorkspaceBinding::new(
            SessionBindingId::new("session").unwrap(),
            authority,
            principal,
            project,
        )
        .unwrap();
        let cursor = WorkspaceCursor::new(
            &binding,
            ResourceScope::root(ResourceId::new("root").unwrap()),
            1,
            CwdHandle::new("cwd").unwrap(),
        );
        RemoteRootIdentity {
            binding,
            cursor,
            cwd: WorkspacePath::root(),
        }
    }

    #[test]
    fn real_local_tree_and_fake_remote_keep_absence_ignores_and_open_revisions_distinct() {
        smol::block_on(async {
            let root = tempfile::tempdir().unwrap();
            fs::create_dir(root.path().join("src")).unwrap();
            fs::write(root.path().join("src/file"), CONTENT).unwrap();
            fs::write(root.path().join(".gitignore"), "missing/\n").unwrap();
            let local = Arc::new(
                LocalTransferPublisher::new(root.path().into(), Arc::new(EmptyRemote))
                    .await
                    .unwrap(),
            );
            let roots = TransferRoots {
                local: LocalRootIdentity::capture(root.path()).unwrap(),
                remote: remote_root(),
            };
            let inventory = RootedTransferInventory {
                roots: roots.clone(),
                local: local.clone(),
                remote: Arc::new(EmptyRemote),
                max_file_bytes: 1024,
                policy: contract::TransferInventoryPolicy::default(),
            };
            let context = inventory.context().await.unwrap();
            assert!(context.safe_local_traversal && context.safe_remote_traversal);
            let missing = inventory
                .inspect(
                    &Side::Local,
                    &WorkspacePath::new("missing/deep/file").unwrap(),
                )
                .await
                .unwrap();
            assert!(missing.node.is_none());
            assert_eq!(missing.ignored, Some(true));
            let remote = inventory
                .inspect(
                    &Side::Remote,
                    &WorkspacePath::new("remote-ignored").unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(remote.ignored, Some(true));
            let page = inventory
                .list(&Side::Local, &WorkspacePath::root(), None, 1)
                .await
                .unwrap();
            assert!(!page.incomplete);
            assert!(page.next.is_some());
            let path = LocalTransferPath::new("src/file").unwrap();
            let (revision, _) = local.stat(&path).await.unwrap();
            let relative = WorkspacePath::new(path.as_str()).unwrap();
            let mut source = inventory
                .open_local(&roots.local, &relative, &revision.0)
                .await
                .unwrap()
                .into_reader();
            let mut bytes = Vec::new();
            source.read_to_end(&mut bytes).await.unwrap();
            assert_eq!(bytes, CONTENT);
            fs::write(root.path().join(path.as_str()), OTHER).unwrap();
            assert!(
                inventory
                    .open_local(&roots.local, &relative, &revision.0)
                    .await
                    .is_err()
            );
            fs::write(root.path().join("another-file"), OTHER).unwrap();
            assert!(
                inventory
                    .list(&Side::Local, &WorkspacePath::root(), page.next, 1)
                    .await
                    .is_err()
            );
        });
    }
}
