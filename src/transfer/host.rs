use rmcp::{ErrorData, model::CustomResult};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use workcell_host_contract::{
    ContractVersion, OperationBinding, OperationIntent, OperationKind, TRANSFER_DOWNLOAD_METHOD,
    TRANSFER_INVENTORY_METHOD, TRANSFER_PREPARE_METHOD, TRANSFER_PUBLICATION_CONTRACT_ID,
    TRANSFER_RELEASE_METHOD, TRANSFER_SEAL_METHOD, TRANSFER_STAGE_METHOD, TRANSFER_STAT_METHOD,
    TRANSFER_STATUS_METHOD, TransferDownloadRequest, TransferInventoryRequest,
    TransferPrepareRequest, TransferPrepareResponse, TransferStageRequest, TransferStageSelector,
    TransferStatRequest, TransferStatResponse, TransferStatusRequest,
};

use super::{
    WorkcellServer, argument_digest, fixed_contract, method_not_found, parse_custom, remote_error,
    remote_invalid,
};
use crate::{
    remote_host::{PreparedRemoteOperation, RemoteHostState, SMALL_PREPARATION_RESERVATION_BYTES},
    transfer::reviewed::TransferError,
};

pub(super) fn is_method(method: &str) -> bool {
    matches!(
        method,
        TRANSFER_STAGE_METHOD
            | TRANSFER_SEAL_METHOD
            | TRANSFER_RELEASE_METHOD
            | TRANSFER_STAT_METHOD
            | TRANSFER_DOWNLOAD_METHOD
            | TRANSFER_PREPARE_METHOD
            | TRANSFER_STATUS_METHOD
            | TRANSFER_INVENTORY_METHOD
    )
}

impl WorkcellServer {
    pub(super) async fn reviewed_request(
        &self,
        remote: &RemoteHostState,
        method: &str,
        params: Value,
        token: &CancellationToken,
    ) -> Result<CustomResult, ErrorData> {
        let manager = self
            .transfer
            .as_ref()
            .and_then(|transfer| transfer.reviewed.as_ref())
            .ok_or_else(method_not_found)?;
        let value = match method {
            TRANSFER_INVENTORY_METHOD => serde_json::to_value(
                manager
                    .inventory(parse_custom::<TransferInventoryRequest>(params)?, token)
                    .await
                    .map_err(transfer_error)?,
            ),
            TRANSFER_STAGE_METHOD => serde_json::to_value(
                manager
                    .stage(parse_custom::<TransferStageRequest>(params)?)
                    .await
                    .map_err(transfer_error)?,
            ),
            TRANSFER_SEAL_METHOD => serde_json::to_value(
                manager
                    .seal(parse_custom::<TransferStageSelector>(params)?)
                    .await
                    .map_err(transfer_error)?,
            ),
            TRANSFER_RELEASE_METHOD => serde_json::to_value(
                manager
                    .release(parse_custom::<TransferStageSelector>(params)?)
                    .await
                    .map_err(transfer_error)?,
            ),
            TRANSFER_STAT_METHOD => {
                let request = parse_custom::<TransferStatRequest>(params)?;
                let file = manager
                    .stat(&request.binding, &request.path, token)
                    .await
                    .map_err(transfer_error)?;
                serde_json::to_value(TransferStatResponse {
                    version: ContractVersion::V1,
                    file: file.metadata,
                })
            }
            TRANSFER_DOWNLOAD_METHOD => serde_json::to_value(
                manager
                    .download(parse_custom::<TransferDownloadRequest>(params)?, token)
                    .await
                    .map_err(transfer_error)?,
            ),
            TRANSFER_PREPARE_METHOD => {
                let request = parse_custom::<TransferPrepareRequest>(params.clone())?;
                let digest = argument_digest(&params)?;
                let publication_id = request.publication_id.clone();
                let binding = OperationBinding {
                    host: request.binding.host.clone(),
                    contract: fixed_contract(TRANSFER_PUBLICATION_CONTRACT_ID)?,
                    argument_digest: digest.clone(),
                };
                remote.validate_host(&binding.host).map_err(remote_error)?;
                let reservation = remote
                    .reserve_preparation(SMALL_PREPARATION_RESERVATION_BYTES)
                    .map_err(remote_error)?;
                let prepared = manager
                    .prepare(request, digest, token)
                    .await
                    .map_err(transfer_error)?;
                let intent = OperationIntent {
                    kind: OperationKind::Transfer,
                    mutating: true,
                    resources: prepared.resources().to_vec(),
                };
                let operation = remote
                    .prepare_reserved(
                        reservation,
                        PreparedRemoteOperation::TransferPublication(prepared),
                        binding,
                        intent,
                    )
                    .map_err(remote_error)?;
                if let Err(error) = manager.record_preparation(
                    &publication_id,
                    &operation.preparation_id,
                    operation.expires_at_unix_ms,
                ) {
                    let _ =
                        remote.release(&operation.preparation_id, None, &operation.binding.host);
                    return Err(transfer_error(error));
                }
                serde_json::to_value(TransferPrepareResponse {
                    version: ContractVersion::V1,
                    publication_id,
                    operation,
                })
            }
            TRANSFER_STATUS_METHOD => {
                let request = parse_custom::<TransferStatusRequest>(params)?;
                serde_json::to_value(
                    manager
                        .status(&request.binding, &request.publication_id)
                        .await
                        .map_err(transfer_error)?,
                )
            }
            _ => return Err(method_not_found()),
        }
        .map_err(|_| remote_invalid())?;
        Ok(CustomResult::new(value))
    }
}

fn transfer_error(error: TransferError) -> ErrorData {
    ErrorData::invalid_params(
        error.to_string(),
        Some(serde_json::json!({"code":error.code()})),
    )
}
