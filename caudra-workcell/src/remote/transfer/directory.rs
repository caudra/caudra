use caudra_workspace::{
    DirectoryPublicationRequest, DirectoryPublicationStatus, OperationError, OperationId,
    OperationState, OperationStatus, PreparedDirectoryPublication, PublishedTransferDirectory,
    ReleaseResult, SessionWorkspaceBinding, TransferDigest, TransferPublicationState,
    WorkspaceCursor, WorkspaceError, WorkspacePath, WorkspaceTransferService,
};
use tokio_util::sync::CancellationToken;
use workcell::{CatalogRevision, host_contract as contract};

use super::super::operation_phase;

use super::{
    PUBLICATION_FAILED, PreparedWorkspaceContext, RecoveryOperation, RemoteWorkcellClient,
    RemoteWorkcellError, contract_identifier, contract_path, invalid_response, publication_state,
    resource_id, validate_fixed_contract, validate_v1,
};

pub(super) const DIRECTORY_KIND: &str = "transfer_directory_publication";
const DIRECTORY_ID_PREFIX: &str = "transfer-dir";

impl RemoteWorkcellClient {
    fn directory_wire(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &DirectoryPublicationRequest,
    ) -> Result<contract::TransferDirectoryPrepareRequest, WorkspaceError> {
        if !self.supports_directory_publication() {
            return Err(WorkspaceError::UnsupportedEntry);
        }
        Ok(contract::TransferDirectoryPrepareRequest {
            version: contract::ContractVersion::V1,
            binding: self.bind_workspace_request(binding, cursor)?,
            publication_id: contract_identifier(&request.publication_id)?,
            path: contract_path(&request.path)?,
            create_directories: request
                .create_directories
                .iter()
                .map(contract_path)
                .collect::<Result<_, _>>()?,
            precondition: contract::TransferPrecondition::MustNotExist {},
        })
    }

    pub(super) async fn prepare_directory_transfer(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &DirectoryPublicationRequest,
    ) -> Result<PreparedDirectoryPublication, WorkspaceError> {
        let wire = self.directory_wire(binding, cursor, request)?;
        let _permit = self.reserve_preparation().await?;
        let response: contract::TransferDirectoryPrepareResponse = self
            .call(
                contract::TRANSFER_DIRECTORY_PREPARE_METHOD,
                &wire,
                &self.0.cancellation.child_token(),
            )
            .await?;
        validate_v1(response.version)?;
        validate_fixed_contract(
            &response.operation.binding.contract,
            contract::TRANSFER_DIRECTORY_PUBLICATION_CONTRACT_ID,
        )?;
        let digest = CatalogRevision::for_serializable(&wire).map_err(|_| invalid_response())?;
        if response.publication_id.as_str() != request.publication_id.as_str()
            || response.operation.binding.argument_digest.as_str() != digest.as_str()
            || !response.operation.intent.mutating
            || response.operation.intent.kind != contract::OperationKind::Transfer
        {
            return Err(invalid_response());
        }
        let operation = self.prepared_handle(
            &response.operation,
            DIRECTORY_ID_PREFIX,
            Some(DIRECTORY_KIND.to_owned()),
            Some(PreparedWorkspaceContext {
                binding: binding.clone(),
                cursor: cursor.clone(),
            }),
        )?;
        let prepared = PreparedDirectoryPublication {
            operation,
            binding: binding.clone(),
            cursor: cursor.clone(),
            cwd_path: self.validate_context(binding, cursor)?.path,
            request_digest: TransferDigest::new(digest.as_str())?,
            request: request.clone(),
            review: serde_json::to_value(response.operation.intent)
                .map_err(|_| invalid_response())?,
        };
        let mut operations = self
            .0
            .operations
            .lock()
            .map_err(|_| WorkspaceError::Unavailable)?;
        let journal = operations
            .entries
            .get_mut(&prepared.operation.preparation_id)
            .and_then(|operation| operation.journal.as_mut())
            .ok_or_else(invalid_response)?;
        journal.publication_id = Some(request.publication_id.clone());
        journal.publication_cwd = Some(prepared.cwd_path.clone());
        Ok(prepared)
    }

    fn validate_directory_prepared(
        &self,
        prepared: &PreparedDirectoryPublication,
    ) -> Result<(), WorkspaceError> {
        let wire = self.directory_wire(&prepared.binding, &prepared.cursor, &prepared.request)?;
        let digest = CatalogRevision::for_serializable(&wire).map_err(|_| invalid_response())?;
        let mut operations = self
            .0
            .operations
            .lock()
            .map_err(|_| WorkspaceError::Unavailable)?;
        let stored = operations.get(&prepared.operation)?;
        validate_fixed_contract(
            &stored.binding.contract,
            contract::TRANSFER_DIRECTORY_PUBLICATION_CONTRACT_ID,
        )?;
        if stored.binding.argument_digest.as_str() != digest.as_str()
            || digest.as_str() != prepared.request_digest.as_str()
            || stored
                .context
                .as_ref()
                .is_none_or(|context| !context.matches(&prepared.binding, &prepared.cursor))
            || self
                .validate_context(&prepared.binding, &prepared.cursor)?
                .path
                != prepared.cwd_path
        {
            return Err(WorkspaceError::Conflict);
        }
        Ok(())
    }

    pub(super) async fn execute_directory_transfer(
        &self,
        prepared: &PreparedDirectoryPublication,
    ) -> Result<OperationStatus<DirectoryPublicationStatus>, WorkspaceError> {
        self.validate_directory_prepared(prepared)?;
        self.execute_operation(
            &prepared.binding,
            &prepared.cursor,
            &prepared.operation,
            |value| {
                let response: contract::TransferDirectoryStatusResponse =
                    serde_json::from_value(value.clone()).map_err(|_| invalid_response())?;
                let result = directory_result(&response, prepared)?;
                if result.state != TransferPublicationState::Completed {
                    return Err(invalid_response());
                }
                Ok(result)
            },
        )
        .await
    }

    pub(super) async fn directory_transfer_status(
        &self,
        prepared: &PreparedDirectoryPublication,
    ) -> Result<DirectoryPublicationStatus, WorkspaceError> {
        if !self.supports_directory_publication() {
            return Err(WorkspaceError::UnsupportedEntry);
        }
        if prepared.binding.principal() != self.session_binding().principal() {
            return Err(WorkspaceError::IdentityMismatch);
        }
        let cursor = self
            .publication_cursor(&prepared.cwd_path, &prepared.cursor)
            .await?;
        let response: contract::TransferDirectoryStatusResponse = self
            .call(
                contract::TRANSFER_DIRECTORY_STATUS_METHOD,
                &contract::TransferDirectoryStatusRequest {
                    version: contract::ContractVersion::V1,
                    binding: self.bind_workspace_request(self.session_binding(), &cursor)?,
                    publication_id: contract_identifier(&prepared.request.publication_id)?,
                },
                &self.0.cancellation.child_token(),
            )
            .await?;
        let result = directory_result(&response, prepared)?;
        for operation in self
            .0
            .mutation_journal
            .recovery_operations()
            .map_err(WorkspaceError::from)?
        {
            if operation.operation_kind == DIRECTORY_KIND
                && operation.publication_id.as_ref() == Some(&prepared.request.publication_id)
            {
                self.settle_directory_recovery(&operation, &response)
                    .await
                    .map_err(WorkspaceError::from)?;
            }
        }
        Ok(result)
    }

    pub(super) async fn release_directory_transfer(
        &self,
        prepared: &PreparedDirectoryPublication,
    ) -> Result<ReleaseResult, WorkspaceError> {
        let retained = self
            .0
            .operations
            .lock()
            .map_err(|_| WorkspaceError::Unavailable)?
            .entries
            .contains_key(&prepared.operation.preparation_id);
        if retained {
            self.validate_directory_prepared(prepared)?;
            return self
                .release_operation(&prepared.binding, &prepared.cursor, &prepared.operation)
                .await;
        }
        let status = self.directory_transfer_status(prepared).await?;
        if !matches!(
            status.state,
            TransferPublicationState::Completed
                | TransferPublicationState::Failed
                | TransferPublicationState::Cancelled
        ) {
            return Err(WorkspaceError::IndeterminateOutcome);
        }
        let response: contract::ReleaseResponse = self
            .call(
                contract::RELEASE_METHOD,
                &contract::ReleaseRequest {
                    version: contract::ContractVersion::V1,
                    selector: contract::OperationSelector {
                        preparation_id: contract_identifier(&prepared.operation.preparation_id)?,
                        invocation_id: Some(contract_identifier(
                            prepared
                                .operation
                                .invocation_id
                                .as_ref()
                                .ok_or_else(invalid_response)?,
                        )?),
                        host: self.host_binding(),
                    },
                },
                &self.0.cancellation.child_token(),
            )
            .await?;
        validate_v1(response.version)?;
        Ok(ReleaseResult {
            state: operation_phase(response.state),
            released: response.released
                || matches!(
                    response.state,
                    contract::OperationState::Forgotten | contract::OperationState::NeverSeen
                ),
        })
    }

    pub(super) async fn recover_directory(
        &self,
        operation: &RecoveryOperation,
    ) -> Result<(), RemoteWorkcellError> {
        let publication = operation
            .publication_id
            .as_ref()
            .ok_or(RemoteWorkcellError::InvalidProtocol)?;
        if let Some(cwd) = &operation.publication_cwd
            && let Ok(cursor) = self.publication_cursor(cwd, &operation.cursor).await
        {
            let response: Result<contract::TransferDirectoryStatusResponse, _> = self
                .call(
                    contract::TRANSFER_DIRECTORY_STATUS_METHOD,
                    &contract::TransferDirectoryStatusRequest {
                        version: contract::ContractVersion::V1,
                        binding: self
                            .bind_workspace_request(self.session_binding(), &cursor)
                            .map_err(|_| RemoteWorkcellError::InvalidProtocol)?,
                        publication_id: contract_identifier(publication)
                            .map_err(|_| RemoteWorkcellError::InvalidProtocol)?,
                    },
                    &CancellationToken::new(),
                )
                .await;
            if let Ok(response) = response {
                return self.settle_directory_recovery(operation, &response).await;
            }
        }
        self.0
            .mutation_journal
            .recovery_indeterminate(&operation.operation_id)
    }

    async fn settle_directory_recovery(
        &self,
        operation: &RecoveryOperation,
        response: &contract::TransferDirectoryStatusResponse,
    ) -> Result<(), RemoteWorkcellError> {
        let publication = operation
            .publication_id
            .as_ref()
            .ok_or(RemoteWorkcellError::InvalidProtocol)?;
        let terminal = match response.state {
            contract::TransferPublicationState::Completed => Some(OperationState::Completed {
                result: (),
                side_effects_possible: false,
            }),
            contract::TransferPublicationState::Failed => Some(OperationState::Failed {
                error: OperationError {
                    code: OperationId::new("transfer_failed")
                        .map_err(|_| RemoteWorkcellError::InvalidProtocol)?,
                    message: PUBLICATION_FAILED.to_owned(),
                },
                side_effects_possible: false,
            }),
            contract::TransferPublicationState::Cancelled => Some(OperationState::Cancelled {
                side_effects_possible: false,
            }),
            _ => None,
        };
        if validate_directory_status(
            response,
            publication,
            &operation.preparation_id,
            &operation.invocation_id,
            operation.request_digest.as_str(),
        )
        .is_ok()
            && let Some(terminal) = terminal
        {
            self.0
                .mutation_journal
                .reconcile_recovery(&operation.operation_id, &terminal, true)?;
            self.0
                .operations
                .lock()
                .map_err(|_| RemoteWorkcellError::JournalUnavailable)?
                .remove(&operation.preparation_id);
            self.release_confirmed_operation(&operation.preparation_id, &operation.invocation_id)
                .await;
            return Ok(());
        }
        self.0
            .mutation_journal
            .recovery_indeterminate(&operation.operation_id)
    }
}

fn validate_directory_status(
    response: &contract::TransferDirectoryStatusResponse,
    publication: &OperationId,
    preparation: &OperationId,
    invocation: &OperationId,
    digest: &str,
) -> Result<(), WorkspaceError> {
    validate_v1(response.version)?;
    if response.publication_id.as_str() != publication.as_str() {
        return Err(WorkspaceError::IdentityMismatch);
    }
    if response.state == contract::TransferPublicationState::Unknown {
        return if response.preparation_id.is_none()
            && response.invocation_id.is_none()
            && response.request_digest.is_none()
            && response.directory.is_none()
        {
            Ok(())
        } else {
            Err(invalid_response())
        };
    }
    if response.preparation_id.as_ref().map(|id| id.as_str()) != Some(preparation.as_str())
        || response.request_digest.as_ref().map(|id| id.as_str()) != Some(digest)
        || response
            .invocation_id
            .as_ref()
            .is_some_and(|id| id.as_str() != invocation.as_str())
        || (response.state == contract::TransferPublicationState::Completed
            && response.invocation_id.is_none())
        || ((response.state == contract::TransferPublicationState::Completed)
            != response.directory.is_some())
    {
        return Err(WorkspaceError::IdentityMismatch);
    }
    Ok(())
}

fn directory_result(
    response: &contract::TransferDirectoryStatusResponse,
    prepared: &PreparedDirectoryPublication,
) -> Result<DirectoryPublicationStatus, WorkspaceError> {
    validate_directory_status(
        response,
        &prepared.request.publication_id,
        &prepared.operation.preparation_id,
        prepared
            .operation
            .invocation_id
            .as_ref()
            .ok_or_else(invalid_response)?,
        prepared.request_digest.as_str(),
    )?;
    let relative = |path: &contract::WorkspacePath| {
        let path = if prepared.cwd_path.is_root() {
            path.as_str()
        } else {
            path.as_str()
                .strip_prefix(prepared.cwd_path.as_str())
                .and_then(|path| path.strip_prefix('/'))
                .ok_or_else(invalid_response)?
        };
        WorkspacePath::new(path).map_err(|_| invalid_response())
    };
    let directory = response
        .directory
        .as_ref()
        .map(|directory| {
            let path = relative(&directory.path)?;
            let created_directories = directory
                .created_directories
                .iter()
                .map(|(path, id)| Ok((relative(path)?, resource_id(id)?)))
                .collect::<Result<Vec<_>, WorkspaceError>>()?;
            if path != prepared.request.path
                || created_directories
                    .iter()
                    .map(|(path, _)| path)
                    .ne(prepared.request.create_directories.iter())
            {
                return Err(invalid_response());
            }
            Ok(PublishedTransferDirectory {
                path,
                resource_id: resource_id(&directory.resource_id)?,
                created_directories,
            })
        })
        .transpose()?;
    Ok(DirectoryPublicationStatus {
        publication_id: prepared.request.publication_id.clone(),
        state: publication_state(&response.state),
        directory,
    })
}
