//! The change records of a remote workspace: one host request per method,
//! bound to the session's binding and the cursor the handle was made at.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use caudra_workspace::{
    CancellationResult, ChangeOperationPreview, ChangeOperationResult, HolderPage, OpenRecord,
    OperationHandle, OperationStatus, PreparedChangeOperation, RecordHolder, RecordPage,
    RecordRequest, RecordSummary, RecordTicket, ReleaseResult, ReleaseSelection, ReleaseSummary,
    RevertStatus, SessionWorkspaceBinding, WorkspaceCapability, WorkspaceChangeBinder,
    WorkspaceChangeService, WorkspaceCursor, WorkspaceError,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use workcell::host_contract as contract;

use super::{
    PreparedWorkspaceContext, RemoteWorkcellClient, invalid_response, validate_fixed_contract,
    validate_v1,
};
use crate::changes::wire;

/// Outlasts the host's wait for its store and its capture budget, so a slow
/// capture is ended by the host, which knows what it left behind.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(16 * 60);
const REVERT_KIND: &str = "changes_revert";
const UNREVERT_KIND: &str = "changes_unrevert";
const CLEANUP_KIND: &str = "changes_cleanup";

/// Whether the host keeps change records as this client reads them: the
/// whole contract at its version, pages it can serve, and the operation
/// lifecycle its reverts and cleanups run through.
pub(super) fn compatible(capabilities: &contract::RemoteHostCapabilities) -> bool {
    let lifecycle = capabilities.operations.as_ref().is_some_and(|operations| {
        operations.exact_preparation
            && operations.methods.execute
            && operations.methods.status
            && operations.methods.cancel
            && operations.methods.release
    });
    lifecycle
        && capabilities.changes.as_ref().is_some_and(|changes| {
            let methods = &changes.methods;
            changes.version == contract::ContractVersion::V1
                && methods.begin_record
                && methods.finish_record
                && methods.abandon_record
                && methods.open_records
                && methods.abandon_open_records
                && methods.records
                && methods.holders
                && methods.hold
                && methods.release
                && methods.prepare_revert
                && methods.prepare_unrevert
                && methods.acknowledge
                && methods.status
                && methods.prepare_cleanup
                && (1..=contract::MAX_RECORD_PAGE_SIZE).contains(&changes.limits.max_page_size)
        })
}

impl WorkspaceChangeBinder for RemoteWorkcellClient {
    fn bind(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
    ) -> Arc<dyn WorkspaceChangeService> {
        Arc::new(RemoteChanges {
            client: self.clone(),
            context: PreparedWorkspaceContext {
                binding: binding.clone(),
                cursor: cursor.clone(),
            },
        })
    }
}

struct RemoteChanges {
    client: RemoteWorkcellClient,
    context: PreparedWorkspaceContext,
}

impl RemoteChanges {
    fn binding(&self) -> Result<contract::WorkspaceRequestBinding, WorkspaceError> {
        self.client
            .bind_workspace_request(&self.context.binding, &self.context.cursor)
    }

    fn ticket_request(
        &self,
        ticket: &RecordTicket,
    ) -> Result<contract::ChangesTicketRequest, WorkspaceError> {
        Ok(contract::ChangesTicketRequest {
            version: contract::ContractVersion::V1,
            binding: self.binding()?,
            ticket: wire::ticket(ticket)?,
        })
    }

    fn holder_request(
        &self,
        holder: &RecordHolder,
    ) -> Result<contract::ChangesHolderRequest, WorkspaceError> {
        Ok(contract::ChangesHolderRequest {
            version: contract::ContractVersion::V1,
            binding: self.binding()?,
            holder: wire::holder(holder)?,
        })
    }

    async fn call<Req, Resp>(&self, method: &str, request: &Req) -> Result<Resp, WorkspaceError>
    where
        Req: Serialize,
        Resp: DeserializeOwned,
    {
        self.client
            .call(method, request, &self.client.0.cancellation.child_token())
            .await
    }

    /// A request the host answers only once it has read the workspace.
    async fn capture<Req, Resp>(&self, method: &str, request: &Req) -> Result<Resp, WorkspaceError>
    where
        Req: Serialize,
        Resp: DeserializeOwned,
    {
        self.client
            .call_within(
                method,
                request,
                &self.client.0.cancellation.child_token(),
                Some(CAPTURE_TIMEOUT),
            )
            .await
    }

    /// What the host advertises it records within and serves.
    fn limits(&self) -> Result<&contract::WorkspaceChangesLimits, WorkspaceError> {
        self.client
            .0
            .descriptor
            .capabilities
            .changes
            .as_ref()
            .map(|changes| &changes.limits)
            .ok_or(WorkspaceError::UnsupportedCapability {
                capability: WorkspaceCapability::ChangeRecords,
            })
    }

    /// `requested`, or the host's largest page when that is smaller.
    fn page_size(&self, requested: u32) -> u32 {
        self.limits()
            .map_or(requested, |limits| requested.min(limits.max_page_size))
    }

    async fn status_of(
        &self,
        method: &str,
        holder: &RecordHolder,
    ) -> Result<RevertStatus, WorkspaceError> {
        let request = self.holder_request(holder)?;
        let response: contract::ChangesStatusResponse = self.call(method, &request).await?;
        validate_v1(response.version)?;
        if response.status.holder != request.holder {
            return Err(WorkspaceError::IdentityMismatch);
        }
        wire::revert_status(response.status)
    }

    async fn prepare_reverting(
        &self,
        method: &str,
        request: &impl Serialize,
        holder: &contract::RecordHolder,
        direction: contract::RevertDirection,
    ) -> Result<PreparedChangeOperation, WorkspaceError> {
        let (contract_id, kind) = match direction {
            contract::RevertDirection::Revert => {
                (contract::CHANGES_REVERT_CONTRACT_ID, REVERT_KIND)
            }
            contract::RevertDirection::Unrevert => {
                (contract::CHANGES_UNREVERT_CONTRACT_ID, UNREVERT_KIND)
            }
        };
        let _permit = self.client.reserve_preparation().await?;
        let response: contract::ChangesPrepareRevertResponse =
            self.capture(method, request).await?;
        validate_v1(response.version)?;
        if &response.preview.holder != holder || response.preview.direction != direction {
            return Err(WorkspaceError::IdentityMismatch);
        }
        let preview = ChangeOperationPreview::Revert(wire::revert_preview(response.preview)?);
        self.prepared(&response.operation, contract_id, kind, preview)
    }

    fn prepared(
        &self,
        operation: &contract::PrepareResponse,
        contract_id: &str,
        kind: &str,
        preview: ChangeOperationPreview,
    ) -> Result<PreparedChangeOperation, WorkspaceError> {
        validate_fixed_contract(&operation.binding.contract, contract_id)?;
        Ok(PreparedChangeOperation {
            operation: self.client.prepared_handle(
                operation,
                kind,
                Some(kind.to_owned()),
                Some(self.context.clone()),
            )?,
            preview,
        })
    }
}

#[async_trait]
impl WorkspaceChangeService for RemoteChanges {
    async fn begin(&self, request: &RecordRequest) -> Result<RecordTicket, WorkspaceError> {
        let directory = self
            .client
            .validate_context(&self.context.binding, &self.context.cursor)?
            .path;
        let response: contract::ChangesBeginRecordResponse = self
            .capture(
                contract::CHANGES_BEGIN_RECORD_METHOD,
                &contract::ChangesBeginRecordRequest {
                    version: contract::ContractVersion::V1,
                    binding: self.binding()?,
                    record: wire::record_request(request, &directory, self.limits()?)?,
                },
            )
            .await?;
        validate_v1(response.version)?;
        wire::record_ticket(&response.ticket)
    }

    async fn finish(&self, ticket: &RecordTicket) -> Result<Option<RecordSummary>, WorkspaceError> {
        let response: contract::ChangesFinishRecordResponse = self
            .capture(
                contract::CHANGES_FINISH_RECORD_METHOD,
                &self.ticket_request(ticket)?,
            )
            .await?;
        validate_v1(response.version)?;
        Ok(response.record.map(wire::record_summary))
    }

    async fn abandon(&self, ticket: &RecordTicket) -> Result<bool, WorkspaceError> {
        let response: contract::ChangesAbandonRecordResponse = self
            .call(
                contract::CHANGES_ABANDON_RECORD_METHOD,
                &self.ticket_request(ticket)?,
            )
            .await?;
        validate_v1(response.version)?;
        Ok(response.abandoned)
    }

    async fn open_records(&self, holder: &RecordHolder) -> Result<Vec<OpenRecord>, WorkspaceError> {
        let response: contract::ChangesOpenRecordsResponse = self
            .call(
                contract::CHANGES_OPEN_RECORDS_METHOD,
                &self.holder_request(holder)?,
            )
            .await?;
        validate_v1(response.version)?;
        response
            .records
            .into_iter()
            .map(wire::open_record)
            .collect()
    }

    async fn abandon_open_records(&self, holder: &RecordHolder) -> Result<u32, WorkspaceError> {
        let response: contract::ChangesAbandonOpenRecordsResponse = self
            .call(
                contract::CHANGES_ABANDON_OPEN_RECORDS_METHOD,
                &self.holder_request(holder)?,
            )
            .await?;
        validate_v1(response.version)?;
        Ok(response.abandoned)
    }

    async fn records(
        &self,
        holder: &RecordHolder,
        after_seq: Option<u64>,
        page_size: u32,
    ) -> Result<RecordPage, WorkspaceError> {
        let page_size = self.page_size(page_size);
        let response: contract::ChangesRecordsResponse = self
            .call(
                contract::CHANGES_RECORDS_METHOD,
                &contract::ChangesRecordsRequest {
                    version: contract::ContractVersion::V1,
                    binding: self.binding()?,
                    holder: wire::holder(holder)?,
                    after_seq,
                    page_size,
                },
            )
            .await?;
        validate_v1(response.version)?;
        let ascending = after_seq
            .into_iter()
            .chain(response.page.records.iter().map(|record| record.seq))
            .is_sorted_by(|earlier, later| earlier < later);
        if !ascending || response.page.records.len() > page_size as usize {
            return Err(invalid_response());
        }
        Ok(wire::record_page(response.page))
    }

    async fn holders(
        &self,
        after: Option<&RecordHolder>,
        page_size: u32,
    ) -> Result<HolderPage, WorkspaceError> {
        let page_size = self.page_size(page_size);
        let response: contract::ChangesHoldersResponse = self
            .call(
                contract::CHANGES_HOLDERS_METHOD,
                &contract::ChangesHoldersRequest {
                    version: contract::ContractVersion::V1,
                    binding: self.binding()?,
                    after: after.map(wire::holder).transpose()?,
                    page_size,
                },
            )
            .await?;
        validate_v1(response.version)?;
        if response.holders.len() > page_size as usize {
            return Err(invalid_response());
        }
        wire::holder_page(response.holders, response.next_after)
    }

    async fn hold(&self, from: &RecordHolder, to: &RecordHolder) -> Result<u32, WorkspaceError> {
        let response: contract::ChangesHoldResponse = self
            .call(
                contract::CHANGES_HOLD_METHOD,
                &contract::ChangesHoldRequest {
                    version: contract::ContractVersion::V1,
                    binding: self.binding()?,
                    from: wire::holder(from)?,
                    to: wire::holder(to)?,
                },
            )
            .await?;
        validate_v1(response.version)?;
        Ok(response.held)
    }

    async fn release(
        &self,
        holder: &RecordHolder,
        selection: &ReleaseSelection,
    ) -> Result<ReleaseSummary, WorkspaceError> {
        let response: contract::ChangesReleaseResponse = self
            .call(
                contract::CHANGES_RELEASE_METHOD,
                &contract::ChangesReleaseRequest {
                    version: contract::ContractVersion::V1,
                    binding: self.binding()?,
                    holder: wire::holder(holder)?,
                    selection: wire::release_selection(selection),
                },
            )
            .await?;
        validate_v1(response.version)?;
        Ok(wire::release_summary(response.release))
    }

    async fn prepare_revert(
        &self,
        holder: &RecordHolder,
        seqs: &[u64],
    ) -> Result<PreparedChangeOperation, WorkspaceError> {
        let request = contract::ChangesPrepareRevertRequest {
            version: contract::ContractVersion::V1,
            binding: self.binding()?,
            holder: wire::holder(holder)?,
            seqs: seqs.to_vec(),
        };
        self.prepare_reverting(
            contract::CHANGES_PREPARE_REVERT_METHOD,
            &request,
            &request.holder,
            contract::RevertDirection::Revert,
        )
        .await
    }

    async fn prepare_unrevert(
        &self,
        holder: &RecordHolder,
    ) -> Result<PreparedChangeOperation, WorkspaceError> {
        let request = self.holder_request(holder)?;
        self.prepare_reverting(
            contract::CHANGES_PREPARE_UNREVERT_METHOD,
            &request,
            &request.holder,
            contract::RevertDirection::Unrevert,
        )
        .await
    }

    async fn acknowledge(&self, holder: &RecordHolder) -> Result<RevertStatus, WorkspaceError> {
        self.status_of(contract::CHANGES_ACKNOWLEDGE_METHOD, holder)
            .await
    }

    async fn status(&self, holder: &RecordHolder) -> Result<RevertStatus, WorkspaceError> {
        self.status_of(contract::CHANGES_STATUS_METHOD, holder)
            .await
    }

    async fn prepare_cleanup(
        &self,
        retention_bytes: u64,
    ) -> Result<PreparedChangeOperation, WorkspaceError> {
        let _permit = self.client.reserve_preparation().await?;
        let response: contract::ChangesPrepareCleanupResponse = self
            .call(
                contract::CHANGES_PREPARE_CLEANUP_METHOD,
                &contract::ChangesPrepareCleanupRequest {
                    version: contract::ContractVersion::V1,
                    binding: self.binding()?,
                    retention_bytes,
                },
            )
            .await?;
        validate_v1(response.version)?;
        self.prepared(
            &response.operation,
            contract::CHANGES_CLEANUP_CONTRACT_ID,
            CLEANUP_KIND,
            ChangeOperationPreview::Cleanup(wire::cleanup_preview(response.preview)),
        )
    }

    async fn execute(
        &self,
        prepared: &PreparedChangeOperation,
    ) -> Result<OperationStatus<ChangeOperationResult>, WorkspaceError> {
        let cleanup = matches!(prepared.preview, ChangeOperationPreview::Cleanup(_));
        self.client
            .execute_operation(
                &self.context.binding,
                &self.context.cursor,
                &prepared.operation,
                move |value| {
                    let result = change_result(value)?;
                    (matches!(result, ChangeOperationResult::Cleanup(_)) == cleanup)
                        .then_some(result)
                        .ok_or_else(invalid_response)
                },
            )
            .await
    }

    async fn operation_status(
        &self,
        operation: &OperationHandle,
    ) -> Result<OperationStatus<ChangeOperationResult>, WorkspaceError> {
        self.client
            .operation_status(
                &self.context.binding,
                &self.context.cursor,
                operation,
                change_result,
            )
            .await
    }

    async fn cancel(
        &self,
        operation: &OperationHandle,
    ) -> Result<CancellationResult, WorkspaceError> {
        self.client
            .cancel_operation(&self.context.binding, &self.context.cursor, operation)
            .await
    }

    async fn release_prepared(
        &self,
        prepared: &PreparedChangeOperation,
    ) -> Result<ReleaseResult, WorkspaceError> {
        self.client
            .release_operation(
                &self.context.binding,
                &self.context.cursor,
                &prepared.operation,
            )
            .await
    }
}

/// A revert answers with its holder's status, a cleanup with its summary.
fn change_result(value: &Value) -> Result<ChangeOperationResult, WorkspaceError> {
    if let Ok(status) = contract::RevertStatus::deserialize(value) {
        return wire::revert_status(status).map(ChangeOperationResult::Revert);
    }
    contract::CleanupSummary::deserialize(value)
        .map(|summary| ChangeOperationResult::Cleanup(wire::cleanup_summary(summary)))
        .map_err(|_| invalid_response())
}
