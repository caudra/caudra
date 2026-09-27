use std::{mem::size_of, time::Duration};

use tokio_util::sync::CancellationToken;
use workcell_host_contract::{
    ContractVersion, Identifier, ResourceIntent, Revision, TRANSFER_IO_TIMEOUT_MS,
    TransferDirectoryPrepareRequest, TransferDirectoryStatusResponse, TransferPublicationState,
    WorkspaceRequestBinding,
};
use workcell_mcp_files::{BinaryError, PreparedDirectoryPublication};

use super::super::journal::Journal;
use super::{ReviewedTransfers, TransferError, lock, unix_ms};

pub(crate) struct PreparedDirectoryTransfer {
    manager: ReviewedTransfers,
    publication_id: Identifier,
    binding: WorkspaceRequestBinding,
    directory: Option<PreparedDirectoryPublication>,
    execution: Option<(Identifier, Identifier)>,
    #[cfg(test)]
    after_settle: Option<Box<dyn FnOnce() + Send>>,
}

impl ReviewedTransfers {
    pub async fn prepare_directory(
        &self,
        request: TransferDirectoryPrepareRequest,
        request_digest: Revision,
        token: &CancellationToken,
    ) -> Result<PreparedDirectoryTransfer, TransferError> {
        let cwd = self.validate(&request.binding).await?;
        let publication_id = request.publication_id.clone();
        let binding = request.binding.clone();
        let directory = self
            .bounded_io(token, move |manager, token| async move {
                manager
                    .0
                    .files
                    .prepare_directory_publication(
                        &manager.0.directory_staging,
                        &request.binding.cwd_handle,
                        &request.path,
                        request.precondition,
                        request.create_directories,
                        &token,
                    )
                    .await
                    .map_err(TransferError::from)
            })
            .await?;
        lock(&self.0.journals).reserve_directory(
            publication_id.clone(),
            cwd,
            request_digest,
            directory.receipt_size_bound(),
        )?;
        Ok(PreparedDirectoryTransfer {
            manager: self.clone(),
            publication_id,
            binding,
            directory: Some(directory),
            execution: None,
            #[cfg(test)]
            after_settle: None,
        })
    }

    pub async fn directory_status(
        &self,
        binding: &WorkspaceRequestBinding,
        id: &Identifier,
    ) -> Result<TransferDirectoryStatusResponse, TransferError> {
        let cwd = self.validate(binding).await?;
        let mut store = lock(&self.0.journals);
        match store.get(id) {
            Some(mut journal) if journal.cwd == cwd && journal.directory.is_some() => {
                if journal.status.state == TransferPublicationState::Prepared
                    && unix_ms() >= journal.expires_at
                {
                    journal.status.state = TransferPublicationState::Cancelled;
                    journal.updated_at = unix_ms();
                    store.put(journal.clone())?;
                }
                Ok(directory_status(journal))
            }
            Some(_) => Err(TransferError::Binding),
            None => Ok(TransferDirectoryStatusResponse {
                version: ContractVersion::V1,
                publication_id: id.clone(),
                state: TransferPublicationState::Unknown,
                preparation_id: None,
                invocation_id: None,
                request_digest: None,
                directory: None,
            }),
        }
    }
}

impl PreparedDirectoryTransfer {
    pub fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            + self.publication_id.as_str().len() * 2
            + self
                .directory
                .as_ref()
                .map_or(0, PreparedDirectoryPublication::retained_bytes)
            + serde_json::to_vec(&self.binding).map_or(usize::MAX / 2, |bytes| bytes.len() * 2)
    }

    pub fn resources(&self) -> &[ResourceIntent] {
        self.directory
            .as_ref()
            .map_or(&[], PreparedDirectoryPublication::resources)
    }

    pub fn bind_execution(&mut self, preparation: Identifier, invocation: Identifier) {
        self.execution = Some((preparation, invocation));
    }

    pub async fn execute(
        mut self,
        token: &CancellationToken,
    ) -> Result<TransferDirectoryStatusResponse, TransferError> {
        let (preparation, invocation) = self.execution.take().ok_or(TransferError::State)?;
        self.manager.validate(&self.binding).await?;
        let _permit = self.manager.permit()?;
        let mut journal = {
            let mut store = lock(&self.manager.0.journals);
            let mut journal = store
                .get(&self.publication_id)
                .ok_or(TransferError::State)?;
            if journal.status.state != TransferPublicationState::Prepared {
                return Err(TransferError::Replay);
            }
            let now = unix_ms();
            if now >= journal.expires_at || token.is_cancelled() {
                return Err(TransferError::Cancelled);
            }
            journal.status.preparation_id = Some(preparation);
            journal.status.invocation_id = Some(invocation);
            journal.status.state = TransferPublicationState::Publishing;
            journal.updated_at = now;
            store.put(journal.clone())?;
            journal
        };
        let child = token.child_token();
        let monitor_token = child.clone();
        let monitor = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(TRANSFER_IO_TIMEOUT_MS)).await;
            monitor_token.cancel();
        });
        let result = self
            .manager
            .0
            .files
            .execute_directory_publication(
                self.directory.take().ok_or(TransferError::State)?,
                &child,
            )
            .await;
        monitor.abort();
        journal.updated_at = unix_ms();
        journal.status.state = match &result {
            Ok(directory) => {
                journal
                    .directory
                    .as_mut()
                    .ok_or(TransferError::Binary(BinaryError::Indeterminate))?
                    .receipt = Some(directory.clone());
                TransferPublicationState::Completed
            }
            Err(BinaryError::Indeterminate) => TransferPublicationState::Indeterminate,
            Err(BinaryError::Cancelled) => TransferPublicationState::Cancelled,
            Err(_) => TransferPublicationState::Failed,
        };
        lock(&self.manager.0.journals)
            .put(journal.clone())
            .map_err(|_| TransferError::Binary(BinaryError::Indeterminate))?;
        result?;
        #[cfg(test)]
        if let Some(hook) = self.after_settle.take() {
            hook();
        }
        Ok(directory_status(journal))
    }
}

fn directory_status(journal: Journal) -> TransferDirectoryStatusResponse {
    TransferDirectoryStatusResponse {
        version: ContractVersion::V1,
        publication_id: journal.status.publication_id,
        state: journal.status.state,
        preparation_id: journal.status.preparation_id,
        invocation_id: journal.status.invocation_id,
        request_digest: journal.status.request_digest,
        directory: journal.directory.and_then(|directory| directory.receipt),
    }
}

impl Drop for PreparedDirectoryTransfer {
    fn drop(&mut self) {
        let mut store = lock(&self.manager.0.journals);
        if let Some(mut journal) = store.get(&self.publication_id) {
            journal.status.state = match journal.status.state {
                TransferPublicationState::Prepared => TransferPublicationState::Cancelled,
                TransferPublicationState::Publishing => TransferPublicationState::Indeterminate,
                _ => return,
            };
            journal.updated_at = unix_ms();
            let _ = store.put(journal);
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::fs;

    use tokio_util::sync::CancellationToken;
    use workcell_host_contract::{
        ContractVersion, DirectoryNavigation, TransferDirectoryPrepareRequest,
        TransferPrecondition, TransferPublicationState, WorkspacePath,
    };

    use super::super::tests::{digest, fixture, id};
    use super::super::{BinaryError, TransferError, lock};

    #[tokio::test]
    async fn overlong_expanded_receipts_never_reserve_journals_or_create_directories() {
        let (root, _private, manager, mut binding) = fixture().await;
        let prefix = vec!["x".repeat(200); 12].join("/");
        fs::create_dir_all(root.path().join(&prefix)).unwrap();
        binding.cwd_handle = manager
            .0
            .files
            .workspace_resolve_directory(
                &binding.cwd_handle,
                &DirectoryNavigation::new(&prefix).unwrap(),
            )
            .await
            .unwrap()
            .handle;
        let request = TransferDirectoryPrepareRequest {
            version: ContractVersion::V1,
            binding: binding.clone(),
            publication_id: id("overlong"),
            path: WorkspacePath::new(["a"; 20].join("/")).unwrap(),
            create_directories: (1..20)
                .map(|n| WorkspacePath::new(vec!["a"; n].join("/")).unwrap())
                .collect(),
            precondition: TransferPrecondition::MustNotExist {},
        };
        assert!(matches!(
            manager
                .prepare_directory(request, digest(b"directory"), &CancellationToken::new())
                .await,
            Err(TransferError::Binary(BinaryError::Integrity))
        ));
        assert!(lock(&manager.0.journals).get(&id("overlong")).is_none());
        assert_eq!(fs::read_dir(root.path().join(prefix)).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn expired_or_cancelled_directory_operations_do_not_cross_the_durable_publication_fence()
    {
        for expire in [false, true] {
            let (root, _private, manager, binding) = fixture().await;
            let token = CancellationToken::new();
            let publication = id("cancelled-directory");
            let mut prepared = manager
                .prepare_directory(
                    TransferDirectoryPrepareRequest {
                        version: ContractVersion::V1,
                        binding: binding.clone(),
                        publication_id: publication.clone(),
                        path: WorkspacePath::new("empty").unwrap(),
                        create_directories: vec![],
                        precondition: TransferPrecondition::MustNotExist {},
                    },
                    digest(b"directory"),
                    &token,
                )
                .await
                .unwrap();
            prepared.bind_execution(id("preparation"), id("invocation"));
            if expire {
                let mut store = lock(&manager.0.journals);
                let mut journal = store.get(&publication).unwrap();
                journal.expires_at = 0;
                store.put(journal).unwrap();
            } else {
                token.cancel();
            }
            assert!(matches!(
                prepared.execute(&token).await,
                Err(TransferError::Cancelled)
            ));
            let outcome = manager
                .directory_status(&binding, &publication)
                .await
                .unwrap();
            assert_eq!(outcome.state, TransferPublicationState::Cancelled);
            assert!(outcome.invocation_id.is_none());
            assert!(outcome.directory.is_none());
            assert!(!root.path().join("empty").exists());
        }
    }

    #[tokio::test]
    async fn invalidating_cwd_after_settlement_cannot_turn_publication_into_a_no_effects_failure() {
        let (root, _private, manager, binding) = fixture().await;
        let token = CancellationToken::new();
        let mut prepared = manager
            .prepare_directory(
                TransferDirectoryPrepareRequest {
                    version: ContractVersion::V1,
                    binding: binding.clone(),
                    publication_id: id("directory"),
                    path: WorkspacePath::new("empty").unwrap(),
                    create_directories: vec![],
                    precondition: TransferPrecondition::MustNotExist {},
                },
                digest(b"directory"),
                &token,
            )
            .await
            .unwrap();
        prepared.bind_execution(id("preparation"), id("invocation"));
        let original = root.path().to_owned();
        let displaced = tempfile::tempdir().unwrap();
        prepared.after_settle = Some(Box::new(move || {
            fs::rename(&original, displaced.path().join("root")).unwrap();
            fs::create_dir(&original).unwrap();
        }));
        let status = prepared.execute(&token).await.unwrap();
        assert_eq!(status.state, TransferPublicationState::Completed);
        assert!(status.directory.is_some());
    }
}
