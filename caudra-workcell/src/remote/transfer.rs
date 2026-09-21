use std::{future::Future, time::Duration};

use async_trait::async_trait;
use caudra_workspace::{
    ByteContent, ByteRange, DownloadedTransfer, LocalTransferSource, Mutation, MutationCondition,
    MutationEntryResult, MutationKind, MutationRequest, MutationResult, OperationError,
    OperationId, OperationState, OperationStatus, PreparedTransferPublication, ReadBytesRequest,
    ReleaseResult, RemoteTransferFile, RemoteTransferStage, SealedTransfer,
    SessionWorkspaceBinding, TransferContent, TransferDigest, TransferLimits, TransferMode,
    TransferPublicationRequest, TransferPublicationState, TransferPublicationStatus,
    WorkspaceCapability, WorkspaceCursor, WorkspaceError, WorkspacePath, WorkspaceTransferService,
    WriteContent,
};
use futures_lite::{
    future,
    io::{AsyncReadExt, Cursor},
};
use isahc::{
    AsyncBody, Response, ResponseExt,
    config::Configurable,
    http::{
        Method, Request, StatusCode,
        header::{
            ACCEPT, AUTHORIZATION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG, HeaderName,
            IF_MATCH, RANGE,
        },
    },
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use smol::Unblock;
use tokio_util::sync::CancellationToken;
use url::Url;
use workcell::{CatalogRevision, host_contract as contract};

use super::{
    CursorRecord, MAX_HTTP_RESPONSE_BYTES, OCTET_STREAM, PreparedWorkspaceContext,
    RecoveryOperation, RemoteTransport, RemoteWorkcellClient, RemoteWorkcellError,
    contract_identifier, contract_path, contract_revision, cwd_handle, invalid_response,
    operation_id, parse_content_range, read_bounded, resource_id, resource_revision, same_origin,
    same_url_origin, unix_millis, validate_fixed_contract, validate_v1,
};
use crate::transfer::{
    IO_TIMEOUT, PrivateStaging, STREAM_BUFFER_BYTES, StagedFile, content, encode_digest, mode,
};

const TRANSFER_KIND: &str = "transfer_publication";
const CWD_HEADER: &str = "x-workcell-cwd";
const DIGEST_HEADER: &str = "x-workcell-sha256";
const UPLOAD_RESPONSE_LIMIT: usize = 1024;
const PUBLICATION_FAILED: &str = "Transfer publication failed without publication";

pub(super) fn negotiated_limits(
    capability: Option<&contract::ReviewedTransferCapability>,
) -> TransferLimits {
    let mut local = PrivateStaging::limits();
    if let Some(capability) = capability {
        let remote = &capability.limits;
        local.max_file_bytes = local.max_file_bytes.min(remote.max_file_bytes);
        local.max_stages = local.max_stages.min(remote.max_stages);
        local.max_reserved_bytes = local.max_reserved_bytes.min(remote.max_reserved_bytes);
        local.max_concurrent_io = local.max_concurrent_io.min(remote.max_concurrent_io);
        local.atomic_replace_against_external_writers =
            capability.atomic_replace_against_external_writers;
    }
    local
}

pub(super) fn compatible(
    capability: &Option<contract::ReviewedTransferCapability>,
    operations: &Option<contract::RemoteOperationCapability>,
) -> bool {
    capability.as_ref().is_some_and(|capability| {
        let limits = &capability.limits;
        capability.version == contract::ContractVersion::V1
            && capability.private_staging
            && capability.sealed_publication
            && capability.conditional_download
            && capability.single_range
            && capability.durable_outcomes
            && limits.max_file_bytes > 0
            && limits.max_stages > 0
            && limits.max_reserved_bytes > 0
            && limits.max_concurrent_io > 0
            && limits.stage_ttl_ms > 0
            && limits.io_timeout_ms > 0
            && limits.max_journals > 0
            && limits.max_journal_bytes > 0
            && limits.max_journal_storage_bytes >= limits.max_journal_bytes
            && limits.outcome_retention_ms > 0
            && limits.stream_buffer_bytes > 0
    }) && operations.as_ref().is_some_and(|operations| {
        operations.version == contract::ContractVersion::V1
            && operations.exact_preparation
            && operations.methods.prepare
            && operations.methods.execute
            && operations.methods.status
            && operations.methods.release
            && operations.methods.cancel
    })
}

impl RemoteTransport {
    fn reviewed_url(&self, path: &str, selector: &str, id: &str) -> Result<Url, WorkspaceError> {
        let url = self.endpoint.join(path).map_err(|_| invalid_response())?;
        if !same_url_origin(&self.endpoint, &url)
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(RemoteWorkcellError::OriginMismatch.into());
        }
        if !path.starts_with('/')
            || path.starts_with("//")
            || url.fragment().is_some()
            || url.path() != "/files"
        {
            return Err(invalid_response());
        }
        let mut reviewed = false;
        let mut selected = false;
        for (key, value) in url.query_pairs() {
            if key == "reviewed" && value == "v1" && !reviewed {
                reviewed = true;
            } else if key == selector && value == id && !selected {
                selected = true;
            } else {
                return Err(invalid_response());
            }
        }
        if !reviewed || !selected {
            return Err(invalid_response());
        }
        Ok(url)
    }

    async fn upload_stage(
        &self,
        url: &Url,
        stage: &RemoteTransferStage,
        source: StagedFile,
        timeout: Duration,
    ) -> Result<(), WorkspaceError> {
        let mut builder = Request::builder()
            .method(Method::POST)
            .uri(url.as_str())
            .header(CONTENT_TYPE, OCTET_STREAM)
            .header(ACCEPT, "application/json")
            .header(CWD_HEADER, stage.cursor.cwd_handle().as_str())
            .timeout(timeout);
        if let Some(bearer) = &self.bearer {
            builder = builder.header(AUTHORIZATION, format!("Bearer {bearer}"));
        }
        let request = builder
            .body(AsyncBody::from_reader_sized(
                Unblock::with_capacity(STREAM_BUFFER_BYTES, source),
                stage.content.size_bytes,
            ))
            .map_err(|_| invalid_response())?;
        let mut response = self
            .client
            .send_async(request)
            .await
            .map_err(|_| WorkspaceError::Unavailable)?;
        validate_http(&response, url)?;
        if response.status() != StatusCode::OK {
            return Err(invalid_response());
        }
        let bytes = read_bounded(response.body_mut(), UPLOAD_RESPONSE_LIMIT)
            .await
            .map_err(WorkspaceError::from)?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| invalid_response())?;
        if value["version"] != "v1" || value["staged"] != true || value["published"] != false {
            return Err(invalid_response());
        }
        Ok(())
    }
}

fn validate_http(response: &Response<AsyncBody>, url: &Url) -> Result<(), WorkspaceError> {
    if !same_origin(url, response.effective_uri()) {
        return Err(RemoteWorkcellError::OriginMismatch.into());
    }
    match response.status() {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(WorkspaceError::PermissionDenied),
        StatusCode::PRECONDITION_FAILED | StatusCode::CONFLICT | StatusCode::GONE => {
            Err(WorkspaceError::Conflict)
        }
        StatusCode::TOO_MANY_REQUESTS | StatusCode::PAYLOAD_TOO_LARGE => {
            Err(WorkspaceError::TransferQuota)
        }
        status if status.is_success() => Ok(()),
        _ => Err(WorkspaceError::Unavailable),
    }
}

impl RemoteWorkcellClient {
    pub(crate) async fn transfer_inventory(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        inspect: Option<&WorkspacePath>,
        policy: &contract::TransferInventoryPolicy,
    ) -> Result<contract::TransferInventoryResponse, WorkspaceError> {
        if !self.transfer_capability()?.safe_inventory {
            return Err(WorkspaceError::Unavailable);
        }
        let cancellation = self.0.cancellation.child_token();
        let _cancel_on_drop = cancellation.clone().drop_guard();
        let response: contract::TransferInventoryResponse = self
            .call(
                contract::TRANSFER_INVENTORY_METHOD,
                &contract::TransferInventoryRequest {
                    version: contract::ContractVersion::V1,
                    binding: self.bind_workspace_request(binding, cursor)?,
                    inspect: inspect.map(contract_path).transpose()?,
                    policy: policy.clone(),
                },
                &cancellation,
            )
            .await?;
        validate_v1(response.version)?;
        if response.entries.len() > contract::MAX_TRANSFER_INVENTORY_ENTRIES
            || serde_json::to_vec(&response)
                .map_err(|_| invalid_response())?
                .len()
                > contract::MAX_TRANSFER_INVENTORY_BYTES
            || response.inspection.as_ref().map(|i| i.path.as_str())
                != inspect.map(WorkspacePath::as_str)
        {
            return Err(invalid_response());
        }
        Ok(response)
    }
    pub(super) async fn read_reviewed_bytes(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &ReadBytesRequest,
    ) -> Result<ByteContent, WorkspaceError> {
        if request.max_bytes == 0 || request.max_bytes > MAX_HTTP_RESPONSE_BYTES as u64 {
            return Err(WorkspaceError::TransferQuota);
        }
        let path = self.selector_contract_path(cursor, &request.resource)?;
        let file = WorkspaceTransferService::stat(
            self,
            binding,
            cursor,
            &WorkspacePath::new(path.as_str()).map_err(|_| invalid_response())?,
        )
        .await?;
        if request
            .if_revision
            .as_ref()
            .is_some_and(|revision| revision != &file.revision)
        {
            return Err(WorkspaceError::StaleResource {
                resource_id: file.resource_id,
            });
        }
        if request.byte_offset > file.content.size_bytes {
            return Err(WorkspaceError::Conflict);
        }
        let range = ByteRange {
            start: request.byte_offset,
            end_exclusive: request
                .byte_offset
                .saturating_add(request.max_bytes)
                .min(file.content.size_bytes),
        };
        let bytes = if range.start == range.end_exclusive {
            Vec::new()
        } else {
            let downloaded = self.download(&file, Some(range)).await?;
            let mut bytes = Vec::with_capacity((range.end_exclusive - range.start) as usize);
            downloaded
                .source
                .into_reader()
                .take(request.max_bytes)
                .read_to_end(&mut bytes)
                .await
                .map_err(|_| WorkspaceError::Unavailable)?;
            bytes
        };
        Ok(ByteContent {
            bytes,
            resource_id: file.resource_id,
            revision: file.revision,
            range,
            total_bytes: Some(file.content.size_bytes),
            truncated: range.end_exclusive < file.content.size_bytes,
            next_byte_offset: (range.end_exclusive < file.content.size_bytes)
                .then_some(range.end_exclusive),
        })
    }

    fn binary_mutation_result(
        &self,
        prepared: &PreparedTransferPublication,
        value: &Value,
    ) -> Result<MutationResult, WorkspaceError> {
        let response = serde_json::from_value(value.clone()).map_err(|_| invalid_response())?;
        let publication =
            self.publication_result(&response, prepared, &prepared.sealed.stage.cursor)?;
        if publication.state != TransferPublicationState::Completed {
            return Err(invalid_response());
        }
        let file = publication.file.ok_or_else(invalid_response)?;
        Ok(MutationResult {
            committed: true,
            rolled_back: false,
            atomic_across_files: true,
            results: vec![MutationEntryResult {
                kind: match prepared.request.condition {
                    MutationCondition::MustNotExist => MutationKind::Create,
                    MutationCondition::Matches(_) => MutationKind::Write,
                },
                path: prepared.request.path.clone(),
                destination: None,
                revision: Some(file.revision),
            }],
        })
    }

    pub(super) async fn binary_mutation_status(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        prepared: &PreparedTransferPublication,
    ) -> Result<OperationStatus<MutationResult>, WorkspaceError> {
        self.operation_status(binding, cursor, &prepared.operation, |value| {
            self.binary_mutation_result(prepared, value)
        })
        .await
    }

    async fn upload_source(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        source: StagedFile,
        expected: &TransferContent,
    ) -> Result<RemoteTransferStage, WorkspaceError> {
        let response: contract::TransferStageResponse = self
            .call(
                contract::TRANSFER_STAGE_METHOD,
                &contract::TransferStageRequest {
                    version: contract::ContractVersion::V1,
                    binding: self.bind_workspace_request(binding, cursor)?,
                    size_bytes: expected.size_bytes,
                    digest: contract::Revision::new(expected.digest.as_str())
                        .map_err(|_| invalid_response())?,
                },
                &self.0.cancellation.child_token(),
            )
            .await?;
        validate_v1(response.version)?;
        let stage = RemoteTransferStage {
            id: operation_id(&response.stage_id)?,
            binding: binding.clone(),
            cursor: cursor.clone(),
            content: expected.clone(),
            expires_at_unix_ms: response.expires_at_unix_ms,
        };
        let result = async {
            if stage.expires_at_unix_ms <= unix_millis() {
                return Err(WorkspaceError::Conflict);
            }
            let url =
                self.0
                    .transport
                    .reviewed_url(&response.upload_path, "stage", stage.id.as_str())?;
            self.transfer_io(
                self.0
                    .transport
                    .upload_stage(&url, &stage, source, IO_TIMEOUT),
            )
            .await
        }
        .await;
        if let Err(error) = result {
            let _ = self.release_stage(&stage).await;
            return Err(error);
        }
        Ok(stage)
    }

    fn transfer_capability(&self) -> Result<&contract::ReviewedTransferCapability, WorkspaceError> {
        self.require_capability(WorkspaceCapability::ReviewedTransfer)?;
        self.0
            .descriptor
            .capabilities
            .reviewed_transfer
            .as_ref()
            .ok_or_else(invalid_response)
    }

    async fn transfer_io<T>(
        &self,
        work: impl Future<Output = Result<T, WorkspaceError>>,
    ) -> Result<T, WorkspaceError> {
        let capability = self.transfer_capability()?;
        let _permit = self.0.staging.io()?;
        let timeout = IO_TIMEOUT.min(Duration::from_millis(capability.limits.io_timeout_ms));
        future::race(
            work,
            future::race(
                async {
                    self.0.cancellation.cancelled().await;
                    Err(WorkspaceError::Cancelled)
                },
                async {
                    smol::Timer::after(timeout).await;
                    Err(WorkspaceError::Cancelled)
                },
            ),
        )
        .await
    }

    fn stage_selector(
        &self,
        stage: &RemoteTransferStage,
    ) -> Result<contract::TransferStageSelector, WorkspaceError> {
        self.transfer_capability()?;
        Ok(contract::TransferStageSelector {
            version: contract::ContractVersion::V1,
            binding: self.bind_workspace_request(&stage.binding, &stage.cursor)?,
            stage_id: contract_identifier(&stage.id)?,
        })
    }

    fn transfer_file(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        path: &WorkspacePath,
        file: &contract::TransferFile,
    ) -> Result<RemoteTransferFile, WorkspaceError> {
        if file.path.as_str() != self.expected_response_path(cursor, path)?.as_str()
            || file.size_bytes > self.limits()?.max_file_bytes
        {
            return Err(invalid_response());
        }
        Ok(RemoteTransferFile {
            binding: binding.clone(),
            cursor: cursor.clone(),
            path: path.clone(),
            resource_id: resource_id(&file.resource_id)?,
            revision: resource_revision(&file.revision)?,
            content: content(file)?,
        })
    }

    fn validate_transfer_prepared(
        &self,
        prepared: &PreparedTransferPublication,
    ) -> Result<(), WorkspaceError> {
        self.transfer_capability()?;
        let mut entries = self
            .0
            .operations
            .lock()
            .map_err(|_| WorkspaceError::Unavailable)?;
        if entries.get(&prepared.operation)?.transfer.as_ref() != Some(prepared) {
            return Err(WorkspaceError::Conflict);
        }
        Ok(())
    }

    fn publication_result(
        &self,
        response: &contract::TransferStatusResponse,
        prepared: &PreparedTransferPublication,
        cursor: &WorkspaceCursor,
    ) -> Result<TransferPublicationStatus, WorkspaceError> {
        validate_publication(
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
        let stage = &prepared.sealed.stage;
        let file = response
            .file
            .as_ref()
            .map(|file| {
                let file = self.transfer_file(
                    self.session_binding(),
                    cursor,
                    &prepared.request.path,
                    file,
                )?;
                if file.content != stage.content {
                    return Err(WorkspaceError::TransferIntegrity);
                }
                Ok(file)
            })
            .transpose()?;
        Ok(TransferPublicationStatus {
            publication_id: prepared.request.publication_id.clone(),
            state: publication_state(&response.state),
            file,
            created_directories: response
                .file
                .as_ref()
                .map(|file| {
                    file.created_directories
                        .iter()
                        .map(|(path, id)| {
                            let relative = if prepared.cwd_path.is_root() {
                                path.as_str()
                            } else {
                                path.as_str()
                                    .strip_prefix(prepared.cwd_path.as_str())
                                    .and_then(|path| path.strip_prefix('/'))
                                    .ok_or_else(invalid_response)?
                            };
                            Ok((
                                WorkspacePath::new(relative).map_err(|_| invalid_response())?,
                                resource_id(id)?,
                            ))
                        })
                        .collect::<Result<Vec<_>, WorkspaceError>>()
                })
                .transpose()?
                .unwrap_or_default(),
        })
    }

    async fn publication_cursor(
        &self,
        cwd: &WorkspacePath,
        expected: &WorkspaceCursor,
    ) -> Result<WorkspaceCursor, WorkspaceError> {
        if expected.project() != &self.0.project {
            return Err(WorkspaceError::IdentityMismatch);
        }
        let base = self.0.descriptor.cwd.display_path.as_str();
        let parents = if base == "." {
            0
        } else {
            base.split('/').count()
        };
        let navigation = contract::DirectoryNavigation::new(format!(
            "{}{}",
            "../".repeat(parents),
            cwd.as_str()
        ))
        .map_err(|_| invalid_response())?;
        let host = self.host_binding();
        let resolved: contract::ResolveDirectoryResponse = self
            .call(
                contract::RESOLVE_DIRECTORY_METHOD,
                &contract::ResolveDirectoryRequest {
                    version: contract::ContractVersion::V1,
                    binding: contract::WorkspaceRequestBinding {
                        cwd_handle: host.cwd_handle.clone(),
                        host,
                    },
                    path: navigation,
                },
                &CancellationToken::new(),
            )
            .await?;
        validate_v1(resolved.version)?;
        if resolved.directory.resource_id.as_str() != expected.scope().resource_id().as_str()
            || resolved.directory.display_path.as_str() != cwd.as_str()
        {
            return Err(WorkspaceError::IdentityMismatch);
        }
        let cursor = WorkspaceCursor::new(
            self.session_binding(),
            expected.scope().clone(),
            expected.generation(),
            cwd_handle(&resolved.directory.handle)?,
        );
        self.0
            .cursors
            .lock()
            .map_err(|_| WorkspaceError::Unavailable)?
            .insert(
                cursor.cwd_handle().clone(),
                CursorRecord {
                    cursor: cursor.clone(),
                    path: cwd.clone(),
                },
            )?;
        Ok(cursor)
    }

    pub(super) async fn recover_transfer(
        &self,
        operation: &RecoveryOperation,
    ) -> Result<(), RemoteWorkcellError> {
        let publication_id = operation
            .publication_id
            .as_ref()
            .ok_or(RemoteWorkcellError::InvalidProtocol)?;
        let Some(cwd) = &operation.publication_cwd else {
            return self
                .0
                .mutation_journal
                .recovery_indeterminate(&operation.operation_id);
        };
        let Ok(cursor) = self.publication_cursor(cwd, &operation.cursor).await else {
            return self
                .0
                .mutation_journal
                .recovery_indeterminate(&operation.operation_id);
        };
        let response: Result<contract::TransferStatusResponse, _> = self
            .call(
                contract::TRANSFER_STATUS_METHOD,
                &contract::TransferStatusRequest {
                    version: contract::ContractVersion::V1,
                    binding: contract::WorkspaceRequestBinding {
                        host: self.host_binding(),
                        cwd_handle: contract::ResourceId::new(cursor.cwd_handle().as_str())
                            .map_err(|_| RemoteWorkcellError::InvalidProtocol)?,
                    },
                    publication_id: contract_identifier(publication_id)
                        .map_err(|_| RemoteWorkcellError::InvalidProtocol)?,
                },
                &CancellationToken::new(),
            )
            .await;
        if let Ok(response) = response
            && validate_publication(
                &response,
                publication_id,
                &operation.preparation_id,
                &operation.invocation_id,
                operation.request_digest.as_str(),
            )
            .is_ok()
            && let Some(state) = terminal_publication(&response)?
            && self
                .0
                .mutation_journal
                .reconcile_recovery(&operation.operation_id, &state)?
        {
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

    pub(super) async fn execute_binary_mutation(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &MutationRequest,
    ) -> Result<OperationStatus<MutationResult>, WorkspaceError> {
        self.transfer_capability()?;
        let [
            Mutation::Write {
                path,
                content: WriteContent::Bytes(bytes),
                condition,
            },
        ] = request.mutations.as_slice()
        else {
            return Err(WorkspaceError::Conflict);
        };
        if bytes.len() as u64 > self.limits()?.max_file_bytes {
            return Err(WorkspaceError::TransferQuota);
        }
        let content = TransferContent {
            digest: encode_digest(Sha256::digest(bytes))?,
            size_bytes: bytes.len() as u64,
            mode: TransferMode::Regular,
        };
        let (source, _) = self
            .transfer_io(self.0.staging.receive(
                &mut Cursor::new(bytes.as_slice()),
                content.size_bytes,
                Some(&content.digest),
            ))
            .await?;
        let stage = self
            .upload_source(binding, cursor, source, &content)
            .await?;
        let result = async {
            let sealed = self.seal(&stage).await?;
            let prepared = self
                .prepare_publication(
                    &sealed,
                    &TransferPublicationRequest {
                        publication_id: operation_id(&self.next_identifier("publication")?)?,
                        path: path.clone(),
                        condition: condition.clone(),
                        create_directories: Vec::new(),
                    },
                )
                .await?;
            // The caller of MutationService already authorized this exact binary destination.
            let status = self
                .execute_operation(binding, cursor, &prepared.operation, |value| {
                    self.binary_mutation_result(&prepared, value)
                })
                .await;
            if status.is_err() {
                let _ = self.release_publication(&prepared).await;
            }
            status
        }
        .await;
        // Releasing a stage cancels a pending publication, so retain it on uncertainty.
        if !self
            .pending_remote_operations()
            .iter()
            .any(|operation| operation.operation_kind == TRANSFER_KIND)
        {
            let _ = self.release_stage(&stage).await;
        }
        result
    }
}

#[async_trait]
impl WorkspaceTransferService for RemoteWorkcellClient {
    fn limits(&self) -> Result<TransferLimits, WorkspaceError> {
        let capability = self.transfer_capability()?;
        Ok(negotiated_limits(Some(capability)))
    }

    async fn stage(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        source: LocalTransferSource,
        expected: &TransferContent,
    ) -> Result<RemoteTransferStage, WorkspaceError> {
        self.bind_workspace_request(binding, cursor)?;
        if expected.size_bytes > self.limits()?.max_file_bytes {
            return Err(WorkspaceError::TransferQuota);
        }
        let (source, _) = self
            .transfer_io(self.0.staging.receive(
                &mut source.into_reader(),
                expected.size_bytes,
                Some(&expected.digest),
            ))
            .await?;
        self.upload_source(binding, cursor, source, expected).await
    }

    async fn seal(&self, stage: &RemoteTransferStage) -> Result<SealedTransfer, WorkspaceError> {
        let response: contract::TransferSealResponse = self
            .call(
                contract::TRANSFER_SEAL_METHOD,
                &self.stage_selector(stage)?,
                &self.0.cancellation.child_token(),
            )
            .await?;
        validate_v1(response.version)?;
        if response.stage_id.as_str() != stage.id.as_str()
            || response.digest.as_str() != stage.content.digest.as_str()
            || response.size_bytes != stage.content.size_bytes
        {
            return Err(WorkspaceError::TransferIntegrity);
        }
        Ok(SealedTransfer {
            stage: stage.clone(),
        })
    }

    async fn release_stage(&self, stage: &RemoteTransferStage) -> Result<bool, WorkspaceError> {
        let response: contract::TransferReleaseResponse = self
            .call(
                contract::TRANSFER_RELEASE_METHOD,
                &self.stage_selector(stage)?,
                &CancellationToken::new(),
            )
            .await?;
        validate_v1(response.version)?;
        Ok(response.released)
    }

    async fn stat(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        path: &WorkspacePath,
    ) -> Result<RemoteTransferFile, WorkspaceError> {
        self.transfer_capability()?;
        let response: contract::TransferStatResponse = self
            .call(
                contract::TRANSFER_STAT_METHOD,
                &contract::TransferStatRequest {
                    version: contract::ContractVersion::V1,
                    binding: self.bind_workspace_request(binding, cursor)?,
                    path: contract_path(path)?,
                },
                &self.0.cancellation.child_token(),
            )
            .await?;
        validate_v1(response.version)?;
        self.transfer_file(binding, cursor, path, &response.file)
    }

    async fn download(
        &self,
        file: &RemoteTransferFile,
        range: Option<ByteRange>,
    ) -> Result<DownloadedTransfer, WorkspaceError> {
        self.transfer_capability()?;
        let response: contract::TransferDownloadResponse = self
            .call(
                contract::TRANSFER_DOWNLOAD_METHOD,
                &contract::TransferDownloadRequest {
                    version: contract::ContractVersion::V1,
                    binding: self.bind_workspace_request(&file.binding, &file.cursor)?,
                    path: contract_path(&file.path)?,
                    revision: contract_revision(&file.revision)?,
                    digest: contract::Revision::new(file.content.digest.as_str())
                        .map_err(|_| invalid_response())?,
                },
                &self.0.cancellation.child_token(),
            )
            .await?;
        let lease = RemoteTransferStage {
            id: operation_id(&response.download_id)?,
            binding: file.binding.clone(),
            cursor: file.cursor.clone(),
            content: file.content.clone(),
            expires_at_unix_ms: response.expires_at_unix_ms,
        };
        let result = self
            .transfer_io(async {
                validate_v1(response.version)?;
                if &self.transfer_file(&file.binding, &file.cursor, &file.path, &response.file)?
                    != file
                    || response.expires_at_unix_ms <= unix_millis()
                {
                    return Err(WorkspaceError::Conflict);
                }
                let url = self.0.transport.reviewed_url(
                    &response.download_path,
                    "download",
                    response.download_id.as_str(),
                )?;
                let selected = range.unwrap_or(ByteRange {
                    start: 0,
                    end_exclusive: file.content.size_bytes,
                });
                if selected.start > selected.end_exclusive
                    || selected.end_exclusive > file.content.size_bytes
                    || (range.is_some() && selected.start == selected.end_exclusive)
                {
                    return Err(invalid_response());
                }
                let length = selected.end_exclusive - selected.start;
                let whole =
                    selected.start == 0 && selected.end_exclusive == file.content.size_bytes;
                let mut builder = Request::builder()
                    .method(Method::GET)
                    .uri(url.as_str())
                    .timeout(IO_TIMEOUT)
                    .header(ACCEPT, OCTET_STREAM)
                    .header(CWD_HEADER, file.cursor.cwd_handle().as_str())
                    .header(IF_MATCH, format!("\"{}\"", file.revision.as_str()));
                if range.is_some() {
                    builder = builder.header(
                        RANGE,
                        format!("bytes={}-{}", selected.start, selected.end_exclusive - 1),
                    );
                }
                if let Some(bearer) = &self.0.transport.bearer {
                    builder = builder.header(AUTHORIZATION, format!("Bearer {bearer}"));
                }
                let mut http = self
                    .0
                    .transport
                    .client
                    .send_async(builder.body(()).map_err(|_| invalid_response())?)
                    .await
                    .map_err(|_| WorkspaceError::Unavailable)?;
                validate_http(&http, &url)?;
                let header = |name| {
                    http.headers()
                        .get(name)
                        .and_then(|value| value.to_str().ok())
                };
                if http.status()
                    != if range.is_some() {
                        StatusCode::PARTIAL_CONTENT
                    } else {
                        StatusCode::OK
                    }
                    || header(CONTENT_TYPE) != Some(OCTET_STREAM)
                    || header(CONTENT_LENGTH).and_then(|length| length.parse::<u64>().ok())
                        != Some(length)
                    || header(ETAG) != Some(format!("\"{}\"", file.revision.as_str()).as_str())
                    || header(HeaderName::from_static(DIGEST_HEADER))
                        != Some(file.content.digest.as_str())
                {
                    return Err(invalid_response());
                }
                if let Some(value) = header(CONTENT_RANGE) {
                    let (start, end, total) =
                        parse_content_range(value).map_err(WorkspaceError::from)?;
                    if range.is_none()
                        || start != selected.start
                        || end != selected.end_exclusive
                        || total != Some(file.content.size_bytes)
                    {
                        return Err(invalid_response());
                    }
                } else if range.is_some() {
                    return Err(invalid_response());
                }
                let (staged, digest) = self
                    .0
                    .staging
                    .receive(
                        http.body_mut(),
                        length,
                        whole.then_some(&file.content.digest),
                    )
                    .await?;
                Ok(DownloadedTransfer {
                    source: LocalTransferSource::new(Unblock::with_capacity(
                        STREAM_BUFFER_BYTES,
                        staged,
                    )),
                    content: TransferContent {
                        digest,
                        size_bytes: length,
                        mode: file.content.mode.clone(),
                    },
                    range: selected,
                    whole_file_verified: whole,
                })
            })
            .await;
        let _ = self.release_stage(&lease).await;
        result
    }

    async fn prepare_publication(
        &self,
        sealed: &SealedTransfer,
        request: &TransferPublicationRequest,
    ) -> Result<PreparedTransferPublication, WorkspaceError> {
        self.transfer_capability()?;
        if !request.create_directories.is_empty()
            && !self.transfer_capability()?.creates_directories
        {
            return Err(WorkspaceError::Unavailable);
        }
        let stage = &sealed.stage;
        let _permit = self.reserve_preparation()?;
        let wire = contract::TransferPrepareRequest {
            version: contract::ContractVersion::V1,
            binding: self.stage_selector(stage)?.binding,
            publication_id: contract_identifier(&request.publication_id)?,
            stage_id: contract_identifier(&stage.id)?,
            digest: contract::Revision::new(stage.content.digest.as_str())
                .map_err(|_| invalid_response())?,
            size_bytes: stage.content.size_bytes,
            path: contract_path(&request.path)?,
            create_directories: request
                .create_directories
                .iter()
                .map(contract_path)
                .collect::<Result<_, _>>()?,
            precondition: match &request.condition {
                MutationCondition::MustNotExist => contract::TransferPrecondition::MustNotExist {},
                MutationCondition::Matches(revision) => contract::TransferPrecondition::Revision {
                    revision: contract_revision(revision)?,
                },
            },
            mode: mode(&stage.content.mode),
        };
        let response: contract::TransferPrepareResponse = self
            .call(
                contract::TRANSFER_PREPARE_METHOD,
                &wire,
                &self.0.cancellation.child_token(),
            )
            .await?;
        validate_v1(response.version)?;
        validate_fixed_contract(
            &response.operation.binding.contract,
            contract::TRANSFER_PUBLICATION_CONTRACT_ID,
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
            "transfer",
            Some((TRANSFER_KIND.to_owned(), false)),
            Some(PreparedWorkspaceContext {
                binding: stage.binding.clone(),
                cursor: stage.cursor.clone(),
            }),
        )?;
        let prepared = PreparedTransferPublication {
            operation,
            cwd_path: self.validate_context(&stage.binding, &stage.cursor)?.path,
            request_digest: TransferDigest::new(digest.as_str())?,
            sealed: sealed.clone(),
            request: request.clone(),
            review: serde_json::to_value(response.operation.intent)
                .map_err(|_| invalid_response())?,
        };
        let mut entries = self
            .0
            .operations
            .lock()
            .map_err(|_| WorkspaceError::Unavailable)?;
        let stored = entries
            .entries
            .get_mut(&prepared.operation.preparation_id)
            .ok_or_else(invalid_response)?;
        stored.transfer = Some(prepared.clone());
        stored
            .journal
            .as_mut()
            .ok_or_else(invalid_response)?
            .publication_id = Some(request.publication_id.clone());
        stored
            .journal
            .as_mut()
            .ok_or_else(invalid_response)?
            .publication_cwd = Some(prepared.cwd_path.clone());
        Ok(prepared)
    }

    async fn execute_publication(
        &self,
        prepared: &PreparedTransferPublication,
    ) -> Result<OperationStatus<TransferPublicationStatus>, WorkspaceError> {
        self.validate_transfer_prepared(prepared)?;
        let stage = &prepared.sealed.stage;
        self.execute_operation(
            &stage.binding,
            &stage.cursor,
            &prepared.operation,
            |value| {
                let response: contract::TransferStatusResponse =
                    serde_json::from_value(value.clone()).map_err(|_| invalid_response())?;
                let result = self.publication_result(&response, prepared, &stage.cursor)?;
                if result.state != TransferPublicationState::Completed {
                    return Err(invalid_response());
                }
                Ok(result)
            },
        )
        .await
    }

    async fn publication_status(
        &self,
        prepared: &PreparedTransferPublication,
    ) -> Result<TransferPublicationStatus, WorkspaceError> {
        self.transfer_capability()?;
        let stage = &prepared.sealed.stage;
        if stage.binding.principal() != self.session_binding().principal() {
            return Err(WorkspaceError::IdentityMismatch);
        }
        let cursor = self
            .publication_cursor(&prepared.cwd_path, &stage.cursor)
            .await?;
        let response: contract::TransferStatusResponse = self
            .call(
                contract::TRANSFER_STATUS_METHOD,
                &contract::TransferStatusRequest {
                    version: contract::ContractVersion::V1,
                    binding: self.bind_workspace_request(self.session_binding(), &cursor)?,
                    publication_id: contract_identifier(&prepared.request.publication_id)?,
                },
                &self.0.cancellation.child_token(),
            )
            .await?;
        let result = self.publication_result(&response, prepared, &cursor)?;
        for operation in self
            .0
            .mutation_journal
            .recovery_operations()
            .map_err(WorkspaceError::from)?
        {
            if operation.publication_id.as_ref() == Some(&prepared.request.publication_id) {
                validate_publication(
                    &response,
                    &prepared.request.publication_id,
                    &operation.preparation_id,
                    &operation.invocation_id,
                    operation.request_digest.as_str(),
                )?;
                if let Some(state) =
                    terminal_publication(&response).map_err(WorkspaceError::from)?
                {
                    self.0
                        .mutation_journal
                        .reconcile_recovery(&operation.operation_id, &state)
                        .map_err(WorkspaceError::from)?;
                    self.0
                        .operations
                        .lock()
                        .map_err(|_| WorkspaceError::Unavailable)?
                        .remove(&operation.preparation_id);
                    self.release_confirmed_operation(
                        &operation.preparation_id,
                        &operation.invocation_id,
                    )
                    .await;
                } else {
                    self.0
                        .mutation_journal
                        .mark_indeterminate(&operation.operation_id)?;
                }
            }
        }
        Ok(result)
    }

    async fn release_publication(
        &self,
        prepared: &PreparedTransferPublication,
    ) -> Result<ReleaseResult, WorkspaceError> {
        self.validate_transfer_prepared(prepared)?;
        let stage = &prepared.sealed.stage;
        let result = self
            .release_operation(&stage.binding, &stage.cursor, &prepared.operation)
            .await?;
        if result.released {
            let _ = self.release_stage(stage).await;
        }
        Ok(result)
    }
}

fn validate_publication(
    response: &contract::TransferStatusResponse,
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
        if response.preparation_id.is_some()
            || response.invocation_id.is_some()
            || response.request_digest.is_some()
            || response.file.is_some()
        {
            return Err(invalid_response());
        }
        return Ok(());
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
            != response.file.is_some())
    {
        return Err(WorkspaceError::IdentityMismatch);
    }
    if let Some(file) = &response.file {
        content(file)?;
    }
    Ok(())
}

fn terminal_publication(
    response: &contract::TransferStatusResponse,
) -> Result<Option<OperationState<()>>, RemoteWorkcellError> {
    Ok(match response.state {
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
    })
}

fn publication_state(state: &contract::TransferPublicationState) -> TransferPublicationState {
    match state {
        contract::TransferPublicationState::Prepared => TransferPublicationState::Prepared,
        contract::TransferPublicationState::Publishing => TransferPublicationState::Publishing,
        contract::TransferPublicationState::Completed => TransferPublicationState::Completed,
        contract::TransferPublicationState::Failed => TransferPublicationState::Failed,
        contract::TransferPublicationState::Cancelled => TransferPublicationState::Cancelled,
        contract::TransferPublicationState::Indeterminate => {
            TransferPublicationState::Indeterminate
        }
        contract::TransferPublicationState::Unknown => TransferPublicationState::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        IO_TIMEOUT, PrivateStaging, RemoteTransport, encode_digest, terminal_publication,
        validate_publication,
    };
    use caudra_config::workcell::WorkcellEndpoint;
    use caudra_storage::workspace_binding::StoredWorkspaceBinding;
    use caudra_workspace::{
        CwdHandle, OperationId, RemoteTransferStage, ResourceId, ResourceScope, TransferContent,
        TransferMode, WorkspaceCursor, WorkspaceError,
    };
    use futures_lite::io::Cursor;
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::TcpListener,
        sync::Arc,
        thread,
    };
    use test_case::test_case;
    use workcell::host_contract as contract;

    const SECRET: &str = "never-disclose-transfer-bearer";
    const BYTES: &[u8] = b"\xff\0binary upload";
    const STAGE_ID: &str = "stage-test";
    const RESPONSE: &str = r#"{"version":"v1","staged":true,"published":false}"#;

    #[test_case("/files?path=legacy"; "legacy_endpoint")]
    #[test_case("https://other.invalid/files?reviewed=v1&stage=stage-test"; "cross_origin")]
    #[test_case("//127.0.0.1/files?reviewed=v1&stage=stage-test"; "network_path")]
    #[test_case("/files?reviewed=v1&stage=other"; "wrong_stage")]
    #[test_case("/files?reviewed=v1&stage=stage-test&token=secret"; "secret_query")]
    #[test_case("/files?reviewed=v1&stage=stage-test#fragment"; "fragment")]
    fn unsafe_transfer_routes_are_refused_before_sending_credentials(path: &str) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint =
            WorkcellEndpoint::parse(&format!("http://{}/mcp", listener.local_addr().unwrap()))
                .unwrap();
        let (transport, _) = RemoteTransport::new(&endpoint, Some(Arc::from(SECRET))).unwrap();
        let error = transport.reviewed_url(path, "stage", STAGE_ID).unwrap_err();
        assert!(!format!("{error:?} {error}").contains(SECRET));
        listener.set_nonblocking(true).unwrap();
        assert!(listener.accept().is_err());
    }

    #[test_case(200, RESPONSE, true; "streaming_binary_upload")]
    #[test_case(307, RESPONSE, false; "redirect_not_followed")]
    #[test_case(200, r#"{"version":"v1","staged":true,"published":true}"#, false; "published_is_not_stage")]
    #[test_case(403, SECRET, false; "untrusted_error_redacted")]
    fn fake_server_upload_is_private_bound_and_does_not_redirect(
        status: u16,
        body: &'static str,
        success: bool,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let trap = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint =
            WorkcellEndpoint::parse(&format!("http://{}/mcp", listener.local_addr().unwrap()))
                .unwrap();
        let redirect = format!("http://{}/credentials", trap.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            reader.read_line(&mut first).unwrap();
            assert!(first.starts_with("POST /files?reviewed=v1&stage=stage-test "));
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                headers.push_str(&line.to_ascii_lowercase());
            }
            assert!(headers.contains(&format!("authorization: bearer {SECRET}")));
            assert!(headers.contains("x-workcell-cwd: local-cwd"));
            assert!(headers.contains(&format!("content-length: {}", BYTES.len())));
            let mut received = [0; BYTES.len()];
            reader.read_exact(&mut received).unwrap();
            assert_eq!(received, BYTES);
            write!(stream, "HTTP/1.1 {status} Test\r\nLocation: {redirect}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        smol::block_on(async {
            let (transport, _) = RemoteTransport::new(&endpoint, Some(Arc::from(SECRET))).unwrap();
            let binding = StoredWorkspaceBinding::local_from_cwd("local-path-canary");
            let cursor = WorkspaceCursor::new(
                binding.binding(),
                ResourceScope::root(ResourceId::new("local-resource").unwrap()),
                0,
                CwdHandle::new("local-cwd").unwrap(),
            );
            let staging = PrivateStaging::default();
            let _permit = staging.io().unwrap();
            let digest = encode_digest(Sha256::digest(BYTES)).unwrap();
            let (source, _) = staging
                .receive(&mut Cursor::new(BYTES), BYTES.len() as u64, Some(&digest))
                .await
                .unwrap();
            let stage = RemoteTransferStage {
                id: OperationId::new(STAGE_ID).unwrap(),
                binding: binding.binding().clone(),
                cursor,
                content: TransferContent {
                    digest,
                    size_bytes: BYTES.len() as u64,
                    mode: TransferMode::Regular,
                },
                expires_at_unix_ms: u64::MAX,
            };
            let url = transport
                .reviewed_url("/files?reviewed=v1&stage=stage-test", "stage", STAGE_ID)
                .unwrap();
            let result = transport
                .upload_stage(&url, &stage, source, IO_TIMEOUT)
                .await;
            assert_eq!(result.is_ok(), success);
            if let Err(error) = result {
                assert!(!format!("{error:?} {error}").contains(SECRET));
            }
        });
        server.join().unwrap();
        trap.set_nonblocking(true).unwrap();
        assert!(trap.accept().is_err());
    }

    #[test_case("unknown"; "unknown_retains_lock")]
    #[test_case("indeterminate"; "indeterminate_retains_lock")]
    #[test_case("publishing"; "publishing_retains_lock")]
    #[test_case("prepared"; "prepared_retains_lock")]
    fn only_positive_terminal_publications_can_release_locks(state: &str) {
        let digest = encode_digest(Sha256::digest(BYTES)).unwrap();
        let mut value = json!({"version":"v1", "publicationId":"publication", "state":state,
            "preparationId":"preparation", "invocationId":"invocation", "requestDigest":digest.as_str(), "file":null});
        if state == "unknown" {
            for key in ["preparationId", "invocationId", "requestDigest"] {
                value[key] = json!(null);
            }
        }
        let response: contract::TransferStatusResponse = serde_json::from_value(value).unwrap();
        validate_publication(
            &response,
            &OperationId::new("publication").unwrap(),
            &OperationId::new("preparation").unwrap(),
            &OperationId::new("invocation").unwrap(),
            digest.as_str(),
        )
        .unwrap();
        assert!(terminal_publication(&response).unwrap().is_none());
    }

    #[test_case("publicationId"; "publication_identity")]
    #[test_case("preparationId"; "preparation_identity")]
    #[test_case("invocationId"; "invocation_identity")]
    #[test_case("requestDigest"; "reviewed_digest")]
    fn mismatched_publication_evidence_is_not_accepted(field: &str) {
        let digest = encode_digest(Sha256::digest(BYTES)).unwrap();
        let mut value = json!({"version":"v1", "publicationId":"publication", "state":"failed",
            "preparationId":"preparation", "invocationId":"invocation", "requestDigest":digest.as_str(), "file":null});
        value[field] = json!("different");
        let response: contract::TransferStatusResponse = serde_json::from_value(value).unwrap();
        assert_eq!(
            validate_publication(
                &response,
                &OperationId::new("publication").unwrap(),
                &OperationId::new("preparation").unwrap(),
                &OperationId::new("invocation").unwrap(),
                digest.as_str()
            ),
            Err(WorkspaceError::IdentityMismatch)
        );
    }
}
