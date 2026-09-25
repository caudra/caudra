use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use caudra_workspace::{
    CheckpointId, OperationHandle, OperationState, SessionWorkspaceBinding, SnapshotCaptureRequest,
    SnapshotCaptureResult, SnapshotState, TransportErrorKind, WorkspaceCursor, WorkspaceError,
    WorkspacePath,
};
use event_listener::Event;
use futures_lite::future;
use serde_json::Value;
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use workcell::{CatalogRevision, host_contract as contract};

use super::{
    JsonRpcError, RemoteWorkcellClient, capture_limits, contract_identifier, convert_status,
    invalid_response, map_rpc_error, operation_id, snapshot_summary, status_request,
    validate_fixed_contract, validate_prepare, validate_snapshot_summary, validate_v1,
};

const CAPTURE_BUDGET: Duration = Duration::from_secs(15 * 60);
const FIRST_POLL: Duration = Duration::from_millis(100);
const MAX_POLL: Duration = Duration::from_secs(2);
const DIAGNOSTIC_INTERVAL: Duration = Duration::from_secs(30);
const CAPTURE_RESOURCE: &str = "snapshot-store:captures";
pub(super) const NOT_FOUND: &str = "not_found";
const REFUSAL_RPC_CODE: i64 = -32602;
const CLEANUP_BUDGET: Duration = Duration::from_secs(30);
const MAX_CLEANUP_WORKERS: usize = 4;
const CLEANUP_IDLE: u8 = 0;
const CLEANUP_QUEUED: u8 = 1;
const CLEANUP_ACTIVE: u8 = 2;
const CLEANUP_FINISHED: u8 = 3;

#[derive(Default)]
pub(super) struct CaptureRegistry {
    gate: AsyncMutex<()>,
    state: Mutex<CaptureRegistryState>,
}

#[derive(Default)]
struct CaptureRegistryState {
    entries: HashMap<CheckpointId, PendingCapture>,
    workers: usize,
}

impl CaptureRegistry {
    fn lock(&self) -> Result<MutexGuard<'_, CaptureRegistryState>, WorkspaceError> {
        self.state.lock().map_err(|_| WorkspaceError::Unavailable)
    }

    pub(super) fn len(&self) -> Result<usize, WorkspaceError> {
        Ok(self.lock()?.entries.len())
    }

    fn get(&self, checkpoint: &CheckpointId) -> Result<Option<PendingCapture>, WorkspaceError> {
        Ok(self.lock()?.entries.get(checkpoint).cloned())
    }

    fn remove(
        &self,
        checkpoint: &CheckpointId,
        expected: &PendingCapture,
    ) -> Result<(), WorkspaceError> {
        let mut state = self.lock()?;
        if state
            .entries
            .get(checkpoint)
            .is_some_and(|pending| pending.same_operation(expected))
        {
            state.entries.remove(checkpoint);
            if expected.cleanup.phase.load(Ordering::Acquire) != CLEANUP_ACTIVE {
                expected.cleanup.finish();
            }
        }
        Ok(())
    }

    fn remember_terminal(
        &self,
        checkpoint: &CheckpointId,
        pending: &PendingCapture,
        error: WorkspaceError,
    ) -> Result<(), WorkspaceError> {
        if let Some(current) = self.lock()?.entries.get_mut(checkpoint)
            && current.same_operation(pending)
        {
            current.terminal_error = Some(error);
        }
        Ok(())
    }
}

#[derive(Default)]
struct CaptureCleanup {
    phase: AtomicU8,
    done: Event,
}

impl CaptureCleanup {
    fn finish(&self) {
        self.phase.store(CLEANUP_FINISHED, Ordering::Release);
        self.done.notify(usize::MAX);
    }

    async fn wait(&self) {
        loop {
            let listener = self.done.listen();
            if self.phase.load(Ordering::Acquire) == CLEANUP_FINISHED {
                return;
            }
            listener.await;
        }
    }
}

#[derive(Clone)]
pub(super) struct PendingCapture {
    handle: OperationHandle,
    binding: contract::OperationBinding,
    scope: WorkspacePath,
    checkpoint: contract::SnapshotCheckpointRequest,
    terminal_error: Option<WorkspaceError>,
    cleanup: Arc<CaptureCleanup>,
}

struct CancelCaptureOnDrop {
    client: RemoteWorkcellClient,
    pending: Option<PendingCapture>,
}

impl Drop for CancelCaptureOnDrop {
    fn drop(&mut self) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        if self.client.queue_capture_cleanup(&pending).is_err() {
            warn!(
                phase = "cancel",
                "could not queue remote snapshot cancellation; capture remains unresolved"
            );
        }
    }
}

pub(super) fn compatible(capabilities: &contract::RemoteHostCapabilities) -> bool {
    capabilities
        .snapshots
        .as_ref()
        .is_some_and(|snapshots| snapshots.methods.prepare_capture && snapshots.methods.checkpoint)
        && capabilities.operations.as_ref().is_some_and(|operations| {
            operations.exact_preparation
                && operations.methods.execute
                && operations.methods.status
                && operations.methods.cancel
                && operations.methods.release
        })
}

impl RemoteWorkcellClient {
    pub(super) async fn capture_snapshot<F, W>(
        &self,
        binding: &SessionWorkspaceBinding,
        cursor: &WorkspaceCursor,
        request: &SnapshotCaptureRequest,
        wait: F,
    ) -> Result<SnapshotCaptureResult, WorkspaceError>
    where
        F: Fn(Duration) -> W,
        W: Future<Output = ()>,
    {
        let scope = self.validate_context(binding, cursor)?.path;
        let wire = contract::SnapshotPrepareCaptureRequest {
            version: contract::ContractVersion::V1,
            binding: self.bind_workspace_request(binding, cursor)?,
            checkpoint_id: contract_identifier(&request.checkpoint_id)?,
            limits: capture_limits(&request.limits, self.snapshot_limits()?),
        };
        let _gate = future::race(async { Ok(self.0.captures.gate.lock().await) }, async {
            self.0.cancellation.cancelled().await;
            Err(WorkspaceError::Cancelled)
        })
        .await?;
        if self
            .0
            .captures
            .get(&request.checkpoint_id)?
            .is_some_and(|pending| pending.scope != scope)
        {
            return Err(WorkspaceError::IdentityMismatch);
        }
        let mut cleanup = CancelCaptureOnDrop {
            client: self.clone(),
            pending: self.0.captures.get(&request.checkpoint_id)?,
        };
        let mut result = future::race(
            self.await_snapshot_capture(&wire, request, &scope, &mut cleanup, wait),
            async {
                Err(future::race(
                    async {
                        self.0.cancellation.cancelled().await;
                        WorkspaceError::Cancelled
                    },
                    async {
                        smol::Timer::after(CAPTURE_BUDGET).await;
                        WorkspaceError::Transport {
                            kind: TransportErrorKind::Timeout,
                        }
                    },
                )
                .await)
            },
        )
        .await;
        if self.0.cancellation.is_cancelled() {
            result = Err(WorkspaceError::Cancelled);
        }
        if result.is_err()
            && let Some(pending) = self.0.captures.get(&request.checkpoint_id)?
        {
            self.queue_capture_cleanup(&pending)?;
            future::race(pending.cleanup.wait(), async {
                smol::Timer::after(CLEANUP_BUDGET).await;
            })
            .await;
        }
        cleanup.pending = None;
        result
    }

    async fn await_snapshot_capture<F, W>(
        &self,
        wire: &contract::SnapshotPrepareCaptureRequest,
        request: &SnapshotCaptureRequest,
        scope: &WorkspacePath,
        cleanup: &mut CancelCaptureOnDrop,
        wait: F,
    ) -> Result<SnapshotCaptureResult, WorkspaceError>
    where
        F: Fn(Duration) -> W,
        W: Future<Output = ()>,
    {
        info!(phase = "checkpoint", "remote snapshot capture");
        let checkpoint = contract::SnapshotCheckpointRequest {
            version: contract::ContractVersion::V1,
            binding: wire.binding.clone(),
            checkpoint_id: wire.checkpoint_id.clone(),
        };
        match self
            .lookup_checkpoint(
                &checkpoint,
                &request.checkpoint_id,
                scope,
                &self.0.cancellation,
            )
            .await
        {
            Ok(Some(result)) => {
                if let Some(pending) = self.0.captures.get(&request.checkpoint_id)? {
                    self.0.captures.remove(&request.checkpoint_id, &pending)?;
                    cleanup.pending = None;
                    self.release_capture(&pending).await;
                }
                return Ok(result);
            }
            Ok(None) => {
                if let Some(pending) = self.0.captures.get(&request.checkpoint_id)?
                    && (pending.terminal_error.is_some()
                        || pending.binding.host.instance_id != wire.binding.host.instance_id)
                {
                    self.0.captures.remove(&request.checkpoint_id, &pending)?;
                    cleanup.pending = None;
                    self.release_capture(&pending).await;
                }
            }
            Err(WorkspaceError::Busy) if self.0.captures.get(&request.checkpoint_id)?.is_some() => {
            }
            Err(error) => return Err(error),
        }
        let mut pending = self.0.captures.get(&request.checkpoint_id)?;
        let response = if pending.is_some() {
            None
        } else {
            if self.0.captures.len()? >= self.operation_limit() {
                self.reclaim_capture_capacity().await;
            }
            if self.0.captures.len()? >= self.operation_limit() {
                return Err(WorkspaceError::Busy);
            }
            let _permit = self.reserve_preparation().await?;
            info!(phase = "prepare", "remote snapshot capture");
            let prepared: contract::PrepareResponse = self
                .call(
                    contract::SNAPSHOT_PREPARE_CAPTURE_METHOD,
                    wire,
                    &self.0.cancellation,
                )
                .await?;
            validate_capture_preparation(&prepared, wire, scope)?;
            if self.0.captures.lock()?.entries.values().any(|pending| {
                pending.handle.preparation_id.as_str() == prepared.preparation_id.as_str()
            }) {
                return Err(WorkspaceError::IdentityMismatch);
            }
            let prepared_capture = PendingCapture {
                handle: OperationHandle {
                    preparation_id: operation_id(&prepared.preparation_id)?,
                    invocation_id: Some(operation_id(&self.next_identifier("capture")?)?),
                    execution_id: None,
                    expires_at_unix_ms: Some(prepared.expires_at_unix_ms),
                },
                binding: prepared.binding,
                scope: scope.clone(),
                checkpoint: checkpoint.clone(),
                terminal_error: None,
                cleanup: Arc::new(CaptureCleanup::default()),
            };
            let execute = contract::ExecuteRequest {
                version: contract::ContractVersion::V1,
                preparation_id: prepared.preparation_id,
                invocation_id: contract_identifier(
                    prepared_capture
                        .handle
                        .invocation_id
                        .as_ref()
                        .ok_or_else(invalid_response)?,
                )?,
                host: wire.binding.host.clone(),
            };
            cleanup.pending = Some(prepared_capture.clone());
            self.0
                .captures
                .lock()?
                .entries
                .insert(request.checkpoint_id.clone(), prepared_capture.clone());
            pending = Some(prepared_capture);
            info!(phase = "execute", "remote snapshot capture");
            Some(
                self.call(contract::EXECUTE_METHOD, &execute, &self.0.cancellation)
                    .await,
            )
        };
        let mut pending = pending.ok_or_else(invalid_response)?;
        if let Some(error) = &pending.terminal_error {
            return Err(error.clone());
        }
        let started = Instant::now();
        let mut diagnostic = DIAGNOSTIC_INTERVAL;
        let mut delay = FIRST_POLL;
        let mut response = response;
        let mut after_sequence = None;
        let result = loop {
            let response: Result<contract::StatusResponse, WorkspaceError> = match response.take() {
                Some(response) => response,
                None => {
                    let mut status = status_request(&pending.handle, &pending.binding.host)?;
                    status.after_sequence = after_sequence;
                    self.call(contract::STATUS_METHOD, &status, &self.0.cancellation)
                        .await
                }
            };
            let response = match response {
                Ok(response) => response,
                Err(error @ WorkspaceError::Transport { .. }) => {
                    if let Ok(Some(result)) = self
                        .lookup_checkpoint(
                            &checkpoint,
                            &request.checkpoint_id,
                            scope,
                            &self.0.cancellation,
                        )
                        .await
                    {
                        break Ok(result);
                    }
                    break Err(error);
                }
                Err(error) => break Err(error),
            };
            self.validate_status_limits(&response)?;
            let expected = pending.expected_handle(&response);
            let status = convert_status(
                &response,
                &expected,
                &pending.binding.host,
                Some(&pending.binding),
                |value| {
                    let capture =
                        serde_json::from_value(value.clone()).map_err(|_| invalid_response())?;
                    self.capture_result(&capture, &request.checkpoint_id, scope)
                },
            )?;
            after_sequence = response.progress_metadata.next_sequence.checked_sub(1);
            log_progress(&response, started.elapsed());
            match status.state {
                OperationState::Completed { result, .. } => break Ok(result),
                OperationState::Running => {
                    pending.handle = status.handle;
                    if let Some(current) = self
                        .0
                        .captures
                        .lock()?
                        .entries
                        .get_mut(&request.checkpoint_id)
                        && current.same_operation(&pending)
                    {
                        current.handle = pending.handle.clone();
                    }
                    if started.elapsed() >= diagnostic {
                        warn!(
                            phase = "running",
                            elapsed_ms = started.elapsed().as_millis(),
                            "remote snapshot capture is still running; baseline remains blocked"
                        );
                        diagnostic += DIAGNOSTIC_INTERVAL;
                    }
                    wait(delay).await;
                    delay = (delay * 2).min(MAX_POLL);
                }
                OperationState::Failed { .. } => {
                    let error = capture_failure(&response)?;
                    pending.terminal_error = Some(error.clone());
                    self.0.captures.remember_terminal(
                        &request.checkpoint_id,
                        &pending,
                        error.clone(),
                    )?;
                    break Err(error);
                }
                OperationState::Cancelled { .. } => {
                    pending.terminal_error = Some(WorkspaceError::Cancelled);
                    self.0.captures.remember_terminal(
                        &request.checkpoint_id,
                        &pending,
                        WorkspaceError::Cancelled,
                    )?;
                    break Err(WorkspaceError::Cancelled);
                }
                _ => {
                    if let Ok(Some(result)) = self
                        .lookup_checkpoint(
                            &checkpoint,
                            &request.checkpoint_id,
                            scope,
                            &self.0.cancellation,
                        )
                        .await
                    {
                        break Ok(result);
                    }
                    break Err(WorkspaceError::IndeterminateOutcome);
                }
            }
        };
        if result.is_ok() {
            self.0.captures.remove(&request.checkpoint_id, &pending)?;
            cleanup.pending = None;
            self.release_capture(&pending).await;
        }
        result
    }

    async fn lookup_checkpoint(
        &self,
        wire: &contract::SnapshotCheckpointRequest,
        checkpoint: &CheckpointId,
        scope: &WorkspacePath,
        cancellation: &CancellationToken,
    ) -> Result<Option<SnapshotCaptureResult>, WorkspaceError> {
        let response = self
            .call::<_, contract::SnapshotCaptureResponse>(
                contract::SNAPSHOT_CHECKPOINT_METHOD,
                wire,
                cancellation,
            )
            .await;
        match response {
            Ok(response) => self.capture_result(&response, checkpoint, scope).map(Some),
            Err(WorkspaceError::Refused { symbolic, .. }) if symbolic == NOT_FOUND => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn capture_result(
        &self,
        response: &contract::SnapshotCaptureResponse,
        checkpoint: &CheckpointId,
        scope: &WorkspacePath,
    ) -> Result<SnapshotCaptureResult, WorkspaceError> {
        validate_v1(response.version)?;
        let snapshot = snapshot_summary(&response.snapshot)?;
        validate_snapshot_summary(&snapshot, self.snapshot_limits()?)?;
        if snapshot.checkpoint_id.as_ref() != Some(checkpoint) || &snapshot.scope != scope {
            return Err(WorkspaceError::IdentityMismatch);
        }
        if snapshot.state != SnapshotState::Complete {
            return Err(invalid_response());
        }
        Ok(SnapshotCaptureResult {
            snapshot,
            reused_checkpoint: response.reused_checkpoint,
        })
    }

    fn queue_capture_cleanup(&self, pending: &PendingCapture) -> Result<(), WorkspaceError> {
        let checkpoint = CheckpointId::new(pending.checkpoint.checkpoint_id.as_str())
            .map_err(|_| invalid_response())?;
        let mut state = self.0.captures.lock()?;
        if state
            .entries
            .get(&checkpoint)
            .is_some_and(|current| current.same_operation(pending))
        {
            let _ = pending.cleanup.phase.compare_exchange(
                CLEANUP_IDLE,
                CLEANUP_QUEUED,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        } else if pending.cleanup.phase.load(Ordering::Acquire) != CLEANUP_ACTIVE {
            pending.cleanup.finish();
        }
        if state.workers < MAX_CLEANUP_WORKERS
            && state
                .entries
                .values()
                .any(|pending| pending.cleanup.phase.load(Ordering::Acquire) == CLEANUP_QUEUED)
        {
            state.workers += 1;
            let client = self.clone();
            smol::spawn(async move {
                client.capture_cleanup_worker().await;
            })
            .detach();
        }
        Ok(())
    }

    async fn capture_cleanup_worker(&self) {
        loop {
            let pending = {
                let Ok(mut state) = self.0.captures.lock() else {
                    return;
                };
                let pending = state
                    .entries
                    .values()
                    .find(|pending| pending.cleanup.phase.load(Ordering::Acquire) == CLEANUP_QUEUED)
                    .cloned();
                let Some(pending) = pending else {
                    state.workers -= 1;
                    return;
                };
                pending
                    .cleanup
                    .phase
                    .store(CLEANUP_ACTIVE, Ordering::Release);
                pending
            };
            let result = self
                .capture_cleanup_attempt(&pending, async {
                    smol::Timer::after(CLEANUP_BUDGET).await;
                })
                .await;
            pending.cleanup.finish();
            warn!(
                phase = "cancel",
                settled = matches!(result, Ok(true)),
                "remote snapshot cancellation cleanup finished; unconfirmed captures remain blocked"
            );
        }
    }

    async fn capture_cleanup_attempt(
        &self,
        pending: &PendingCapture,
        deadline: impl Future<Output = ()>,
    ) -> Result<bool, WorkspaceError> {
        future::race(self.cancel_and_reconcile_capture(pending), async {
            deadline.await;
            Err(WorkspaceError::Transport {
                kind: TransportErrorKind::Timeout,
            })
        })
        .await
    }

    async fn cancel_and_reconcile_capture(
        &self,
        pending: &PendingCapture,
    ) -> Result<bool, WorkspaceError> {
        if pending.terminal_error.is_none() && pending.binding.host == self.host_binding() {
            let selector = status_request(&pending.handle, &pending.binding.host)?;
            let cancelled: contract::CancelResponse = self
                .call(
                    contract::CANCEL_METHOD,
                    &contract::CancelRequest {
                        version: contract::ContractVersion::V1,
                        preparation_id: selector.selector.preparation_id.clone(),
                        invocation_id: selector
                            .selector
                            .invocation_id
                            .clone()
                            .ok_or_else(invalid_response)?,
                        host: pending.binding.host.clone(),
                    },
                    &CancellationToken::new(),
                )
                .await?;
            validate_v1(cancelled.version)?;
        }
        self.reconcile_capture(pending).await
    }

    async fn reconcile_capture(&self, pending: &PendingCapture) -> Result<bool, WorkspaceError> {
        let checkpoint = CheckpointId::new(pending.checkpoint.checkpoint_id.as_str())
            .map_err(|_| invalid_response())?;
        let mut wire = pending.checkpoint.clone();
        wire.binding.host = self.host_binding();
        let mut settled = pending.terminal_error.is_some()
            || pending.binding.host.instance_id != wire.binding.host.instance_id;
        let mut committed = false;
        if !settled {
            let status: contract::StatusResponse = self
                .call(
                    contract::STATUS_METHOD,
                    &status_request(&pending.handle, &pending.binding.host)?,
                    &CancellationToken::new(),
                )
                .await?;
            self.validate_status_limits(&status)?;
            let expected = pending.expected_handle(&status);
            let converted = convert_status(
                &status,
                &expected,
                &pending.binding.host,
                Some(&pending.binding),
                |value| {
                    let capture =
                        serde_json::from_value(value.clone()).map_err(|_| invalid_response())?;
                    self.capture_result(&capture, &checkpoint, &pending.scope)
                },
            )?;
            let error = match converted.state {
                OperationState::Completed { .. } => {
                    committed = true;
                    None
                }
                OperationState::Failed { .. } => Some(capture_failure(&status)?),
                OperationState::Cancelled { .. } => Some(WorkspaceError::Cancelled),
                _ => None,
            };
            if let Some(error) = error {
                self.0
                    .captures
                    .remember_terminal(&checkpoint, pending, error)?;
                settled = true;
            }
        }
        let retire = committed
            || match self
                .lookup_checkpoint(
                    &wire,
                    &checkpoint,
                    &pending.scope,
                    &CancellationToken::new(),
                )
                .await?
            {
                Some(_) => true,
                None => settled,
            };
        if retire {
            self.0.captures.remove(&checkpoint, pending)?;
            self.release_capture(pending).await;
        }
        Ok(retire)
    }

    async fn reclaim_capture_capacity(&self) {
        let candidates = {
            let Ok(state) = self.0.captures.lock() else {
                return;
            };
            state.entries.values().cloned().collect::<Vec<_>>()
        };
        future::race(
            async {
                for pending in candidates {
                    if self
                        .0
                        .captures
                        .len()
                        .is_ok_and(|len| len < self.operation_limit())
                    {
                        break;
                    }
                    let _ = self.reconcile_capture(&pending).await;
                }
            },
            async {
                smol::Timer::after(CLEANUP_BUDGET).await;
            },
        )
        .await;
    }

    async fn release_capture(&self, pending: &PendingCapture) {
        if pending.binding.host != self.host_binding() {
            return;
        }
        let Ok(selector) = status_request(&pending.handle, &pending.binding.host) else {
            return;
        };
        let result: Result<contract::ReleaseResponse, WorkspaceError> = self
            .call(
                contract::RELEASE_METHOD,
                &contract::ReleaseRequest {
                    version: contract::ContractVersion::V1,
                    selector: selector.selector,
                },
                &CancellationToken::new(),
            )
            .await;
        if !matches!(result, Ok(response) if response.version == contract::ContractVersion::V1 && response.released)
        {
            warn!(
                phase = "release",
                "remote snapshot capture settled but handle release was not confirmed"
            );
        }
    }
}

impl PendingCapture {
    fn same_operation(&self, other: &Self) -> bool {
        self.binding == other.binding
            && self.handle.preparation_id == other.handle.preparation_id
            && self.handle.invocation_id == other.handle.invocation_id
    }

    fn expected_handle(&self, response: &contract::StatusResponse) -> OperationHandle {
        let mut expected = self.handle.clone();
        if response.binding.is_none() {
            expected.execution_id = None;
        }
        expected
    }
}

fn log_progress(response: &contract::StatusResponse, elapsed: Duration) {
    let progress = response
        .progress
        .iter()
        .rev()
        .find(|progress| progress.kind.as_str() == "snapshotCapture")
        .and_then(|progress| serde_json::from_str::<Value>(progress.chunk.as_str()).ok());
    let phase = progress.as_ref().and_then(|value| value["phase"].as_str());
    let phase = phase
        .filter(|phase| {
            matches!(
                *phase,
                "queued" | "scanning" | "persisting" | "publishing" | "rollback" | "finished"
            )
        })
        .unwrap_or("waiting");
    info!(phase, state = ?response.state, elapsed_ms = elapsed.as_millis(),
        entries = progress.as_ref().and_then(|value| value["entries"].as_u64()),
        files = progress.as_ref().and_then(|value| value["files"].as_u64()),
        bytes = progress.as_ref().and_then(|value| value["bytes"].as_u64()),
        "remote snapshot capture progress");
}

fn validate_capture_preparation(
    prepared: &contract::PrepareResponse,
    request: &contract::SnapshotPrepareCaptureRequest,
    scope: &WorkspacePath,
) -> Result<(), WorkspaceError> {
    validate_prepare(prepared, &request.binding.host)?;
    validate_fixed_contract(
        &prepared.binding.contract,
        contract::SNAPSHOT_CAPTURE_CONTRACT_ID,
    )?;
    let encoded = serde_json::to_value(request).map_err(|_| invalid_response())?;
    let digest = CatalogRevision::for_serializable(&encoded).map_err(|_| invalid_response())?;
    let intent = &prepared.intent;
    if prepared.binding.argument_digest.as_str() != digest.as_str()
        || intent.kind != contract::OperationKind::Mutate
        || !intent.mutating
    {
        return Err(invalid_response());
    }
    let expected = [
        (format!("file:{scope}"), contract::ResourceAccess::Read),
        (CAPTURE_RESOURCE.to_owned(), contract::ResourceAccess::Read),
        (CAPTURE_RESOURCE.to_owned(), contract::ResourceAccess::Write),
        (
            CAPTURE_RESOURCE.to_owned(),
            contract::ResourceAccess::Delete,
        ),
    ];
    if intent.resources.len() != expected.len()
        || intent
            .resources
            .iter()
            .zip(expected)
            .any(|(resource, (display, access))| {
                resource.display.as_str() != display
                    || resource.access != access
                    || resource.revision.is_some()
            })
    {
        return Err(invalid_response());
    }
    Ok(())
}

fn capture_failure(response: &contract::StatusResponse) -> Result<WorkspaceError, WorkspaceError> {
    let outcome = response.outcome.as_ref().ok_or_else(invalid_response)?;
    let data = outcome
        .result
        .as_ref()
        .and_then(|result| result.structured_content.as_ref())
        .and_then(|value| value.get("error"))
        .cloned()
        .or_else(|| {
            outcome
                .error
                .as_ref()
                .map(|error| serde_json::json!({"code": error.code}))
        })
        .ok_or_else(invalid_response)?;
    if data.get("code").and_then(Value::as_str).is_none() {
        return Err(invalid_response());
    }
    Ok(map_rpc_error(&JsonRpcError {
        code: REFUSAL_RPC_CODE,
        message: String::new(),
        data: Some(data),
    })
    .into())
}

#[cfg(test)]
mod tests {
    use std::pin::pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::task::Poll;
    use std::time::Duration;

    use caudra_storage::StateDir;
    use caudra_workspace::{CheckpointId, OperationId, SnapshotCaptureRequest, WorkspaceError};
    use futures_lite::future;
    use serde_json::{Value, json};
    use test_case::test_case;
    use tokio_util::sync::CancellationToken;
    use workcell::host_contract as contract;

    use super::super::tests::{
        snapshot_checkpoint, snapshot_client, snapshot_host, snapshot_refusal, snapshot_request,
    };
    use super::{
        CancelCaptureOnDrop, CaptureCleanup, MAX_CLEANUP_WORKERS, PendingCapture,
        RemoteWorkcellClient,
    };

    const BARRIER_BUDGET: Duration = Duration::from_secs(10);
    const DUPLICATE_DROPS: usize = 128;
    const CAPACITY: usize = 2;
    const QUOTA_LIMIT: &str = "storageBytes";
    const QUOTA_MAXIMUM: u64 = 1;

    fn status(mut response: Value, state: &str) -> Value {
        response["state"] = json!(state);
        match state {
            "running" => response["outcome"] = Value::Null,
            "cancelled" | "cancelledWithEffects" => {
                response["state"] = json!("cancelled");
                response["outcome"] = json!({"kind":"cancelled", "sideEffectsPossible":state == "cancelledWithEffects", "result":null, "error":null});
            }
            "failed" => {
                response["outcome"]["kind"] = json!("failed");
                response["outcome"]["result"]["isError"] = json!(true);
                response["outcome"]["result"]["structuredContent"] = json!({"error":{
                    "code":"quota_exceeded", "limit":QUOTA_LIMIT, "maximum":QUOTA_MAXIMUM
                }});
            }
            "indeterminate" | "neverSeen" | "forgotten" => {
                for field in ["binding", "executionId", "expiresAtUnixMs", "outcome"] {
                    response[field] = Value::Null;
                }
            }
            _ => panic!("unexpected test status"),
        }
        response
    }

    async fn capture(
        client: &RemoteWorkcellClient,
        request: &SnapshotCaptureRequest,
    ) -> Result<(), WorkspaceError> {
        client
            .capture_snapshot(
                client.session_binding(),
                client.root_cursor(),
                request,
                |_| async {},
            )
            .await
            .map(|_| ())
    }

    async fn drop_capture(
        client: &RemoteWorkcellClient,
        request: &SnapshotCaptureRequest,
    ) -> PendingCapture {
        let waiting = CancellationToken::new();
        let observed = Mutex::new(None);
        future::race(
            async {
                let _ = client
                    .capture_snapshot(
                        client.session_binding(),
                        client.root_cursor(),
                        request,
                        |_| {
                            *observed.lock().unwrap() =
                                client.0.captures.get(&request.checkpoint_id).unwrap();
                            waiting.cancel();
                            future::pending::<()>()
                        },
                    )
                    .await;
                panic!("capture should still be running");
            },
            waiting.cancelled(),
        )
        .await;
        observed.into_inner().unwrap().unwrap()
    }

    #[test_case("failed", false; "quota_reclaimed")]
    #[test_case("failed", true; "quota_retained_until_absence")]
    #[test_case("cancelled", false; "confirmed_cancel_reclaimed")]
    #[test_case("cancelled", true; "confirmed_cancel_waits_for_absence")]
    #[test_case("timed_out", false; "confirmed_timeout_reclaimed")]
    fn terminal_failure_can_retry_same_checkpoint(terminal: &'static str, busy: bool) {
        let failed = Arc::new(AtomicBool::new(false));
        let retry = Arc::new(AtomicBool::new(false));
        let host_retry = retry.clone();
        let host = snapshot_host(move |method, _, reply| {
            if method == contract::EXECUTE_METHOD && !host_retry.load(Ordering::Acquire) {
                failed.store(true, Ordering::Release);
                let mut response = status(
                    reply.unwrap(),
                    if terminal == "timed_out" {
                        "failed"
                    } else {
                        terminal
                    },
                );
                if terminal == "timed_out" {
                    response["outcome"]["result"]["structuredContent"] =
                        json!({"error":{"code":"timed_out"}});
                }
                return Ok(response);
            }
            if method == contract::SNAPSHOT_CHECKPOINT_METHOD
                && busy
                && failed.load(Ordering::Acquire)
                && !host_retry.load(Ordering::Acquire)
            {
                return Err(snapshot_refusal("busy"));
            }
            reply
        });
        let temp = tempfile::tempdir().unwrap();
        let client = snapshot_client(
            &host.endpoint,
            &StateDir::from_path(temp.path().join("state")),
        );
        smol::block_on(async {
            let error = capture(&client, &snapshot_request()).await.unwrap_err();
            assert_eq!(
                error,
                match terminal {
                    "cancelled" => WorkspaceError::Cancelled,
                    "timed_out" => super::super::RemoteWorkcellError::Timeout.into(),
                    _ => WorkspaceError::QuotaExceeded {
                        limit: Some(QUOTA_LIMIT.into()),
                        maximum: Some(QUOTA_MAXIMUM)
                    },
                }
            );
            assert_eq!(client.0.captures.len().unwrap(), usize::from(busy));
            retry.store(true, Ordering::Release);
            capture(&client, &snapshot_request()).await.unwrap();
            assert_eq!(
                host.params_for(contract::SNAPSHOT_PREPARE_CAPTURE_METHOD)
                    .len(),
                2
            );
            assert_eq!(host.params_for(contract::EXECUTE_METHOD).len(), 2);
            assert_eq!(client.0.captures.len().unwrap(), 0);
        });
    }

    #[test_case("cancelled", true; "cancelled_abandoned_heads")]
    #[test_case("failed", true; "failed_abandoned_heads")]
    #[test_case("restarted", true; "old_host_abandoned_heads")]
    #[test_case("indeterminate", false; "indeterminate_heads_block")]
    #[test_case("neverSeen", false; "absent_operations_stay_unknown")]
    #[test_case("forgotten", false; "forgotten_heads_block")]
    #[test_case("running", false; "active_heads_block")]
    #[test_case("cancelledWithEffects", false; "unconfirmed_side_effects_block")]
    fn capacity_reconciliation_reclaims_only_proven_settled_heads(
        next_status: &'static str,
        reclaimed: bool,
    ) {
        let abandoning = Arc::new(AtomicBool::new(true));
        let host_abandoning = abandoning.clone();
        let host = snapshot_host(move |method, _, reply| {
            if method == contract::EXECUTE_METHOD && host_abandoning.load(Ordering::Acquire) {
                return Ok(status(reply.unwrap(), "running"));
            }
            if method == contract::STATUS_METHOD {
                return Ok(status(
                    reply.unwrap(),
                    if host_abandoning.load(Ordering::Acquire) || next_status == "restarted" {
                        "running"
                    } else {
                        next_status
                    },
                ));
            }
            reply
        });
        let temp = tempfile::tempdir().unwrap();
        let client = snapshot_client(
            &host.endpoint,
            &StateDir::from_path(temp.path().join("state")),
        );
        client.0.operations.lock().unwrap().limit = CAPACITY;
        smol::block_on(async {
            for index in 0..CAPACITY {
                let mut request = snapshot_request();
                request.checkpoint_id = CheckpointId::new(format!("abandoned-{index}")).unwrap();
                let pending = drop_capture(&client, &request).await;
                pending.cleanup.wait().await;
            }
            assert_eq!(client.0.captures.len().unwrap(), CAPACITY);
            abandoning.store(false, Ordering::Release);
            if next_status == "restarted" {
                client.0.host_binding.lock().unwrap().instance_id =
                    contract::Identifier::new("restarted").unwrap();
            }
            let result = capture(&client, &snapshot_request()).await;
            if reclaimed {
                result.unwrap();
            } else {
                assert_eq!(result.unwrap_err(), WorkspaceError::Busy);
            }
            assert_eq!(
                host.params_for(contract::EXECUTE_METHOD).len(),
                CAPACITY + usize::from(reclaimed)
            );
            assert_eq!(
                client.0.captures.len().unwrap(),
                CAPACITY - usize::from(reclaimed)
            );
        });
    }

    #[test_case(false; "cancelled_and_absent")]
    #[test_case(true; "publication_wins_cancel")]
    fn dropped_capture_cleanup_reclaims_settled_entries(published: bool) {
        let cancelled = AtomicBool::new(false);
        let host = snapshot_host(move |method, _, reply| match method {
            contract::EXECUTE_METHOD => Ok(status(reply.unwrap(), "running")),
            contract::CANCEL_METHOD => {
                cancelled.store(true, Ordering::Release);
                reply
            }
            contract::STATUS_METHOD => Ok(status(reply.unwrap(), "cancelled")),
            contract::SNAPSHOT_CHECKPOINT_METHOD
                if published && cancelled.load(Ordering::Acquire) =>
            {
                Ok(snapshot_checkpoint())
            }
            _ => reply,
        });
        let temp = tempfile::tempdir().unwrap();
        let client = snapshot_client(
            &host.endpoint,
            &StateDir::from_path(temp.path().join("state")),
        );
        smol::block_on(async {
            let pending = drop_capture(&client, &snapshot_request()).await;
            pending.cleanup.wait().await;
            assert_eq!(client.0.captures.len().unwrap(), 0);
            assert_eq!(host.params_for(contract::CANCEL_METHOD).len(), 1);
            assert_eq!(host.params_for(contract::RELEASE_METHOD).len(), 1);
            if published {
                capture(&client, &snapshot_request()).await.unwrap();
                assert_eq!(host.params_for(contract::EXECUTE_METHOD).len(), 1);
            }
        });
    }

    #[test]
    fn duplicate_drops_coalesce_and_cleanup_concurrency_is_bounded() {
        let entered = CancellationToken::new();
        let signal = entered.clone();
        let (release, resume) = mpsc::channel();
        let first = AtomicBool::new(true);
        let host = snapshot_host(move |method, _, reply| {
            if method == contract::CANCEL_METHOD && first.swap(false, Ordering::AcqRel) {
                signal.cancel();
                resume.recv_timeout(BARRIER_BUDGET).unwrap();
            }
            if matches!(method, contract::EXECUTE_METHOD | contract::STATUS_METHOD) {
                return Ok(status(reply.unwrap(), "running"));
            }
            reply
        });
        let temp = tempfile::tempdir().unwrap();
        let client = snapshot_client(
            &host.endpoint,
            &StateDir::from_path(temp.path().join("state")),
        );
        smol::block_on(async {
            let pending = drop_capture(&client, &snapshot_request()).await;
            entered.cancelled().await;
            for _ in 0..DUPLICATE_DROPS {
                drop(CancelCaptureOnDrop {
                    client: client.clone(),
                    pending: Some(pending.clone()),
                });
            }
            assert_eq!(client.0.captures.lock().unwrap().workers, 1);
            let mut jobs = vec![pending.clone()];
            for index in 0..MAX_CLEANUP_WORKERS {
                let mut other = pending.clone();
                let checkpoint = CheckpointId::new(format!("queued-{index}")).unwrap();
                other.checkpoint.checkpoint_id =
                    contract::Identifier::new(checkpoint.as_str()).unwrap();
                other.handle.invocation_id =
                    Some(OperationId::new(format!("queued-invocation-{index}")).unwrap());
                other.cleanup = Arc::new(CaptureCleanup::default());
                client
                    .0
                    .captures
                    .lock()
                    .unwrap()
                    .entries
                    .insert(checkpoint, other.clone());
                drop(CancelCaptureOnDrop {
                    client: client.clone(),
                    pending: Some(other.clone()),
                });
                jobs.push(other);
            }
            let workers = client.0.captures.lock().unwrap().workers;
            release.send(()).unwrap();
            assert!(workers <= MAX_CLEANUP_WORKERS);
            for job in &jobs {
                job.cleanup.wait().await;
            }
            for _ in 0..DUPLICATE_DROPS {
                drop(CancelCaptureOnDrop {
                    client: client.clone(),
                    pending: Some(pending.clone()),
                });
            }
            assert_eq!(host.params_for(contract::CANCEL_METHOD).len(), jobs.len());
            assert_eq!(client.0.captures.len().unwrap(), jobs.len());
        });
    }

    #[test_case("failed"; "terminal_failure")]
    #[test_case("cancelled"; "confirmed_cancel")]
    #[test_case("completed"; "committed_result")]
    #[test_case("published"; "checkpoint_wins_cancel")]
    fn settled_capture_retires_before_stalled_release(settlement: &'static str) {
        let settling = Arc::new(AtomicBool::new(false));
        let host_settling = settling.clone();
        let entered = CancellationToken::new();
        let signal = entered.clone();
        let (release, resume) = mpsc::channel();
        let host = snapshot_host(move |method, _, reply| {
            match method {
                contract::EXECUTE_METHOD => return Ok(status(reply.unwrap(), "running")),
                contract::STATUS_METHOD => {
                    let state = if host_settling.load(Ordering::Acquire) {
                        settlement
                    } else {
                        "running"
                    };
                    return if state == "completed" {
                        reply
                    } else {
                        Ok(status(
                            reply.unwrap(),
                            if state == "published" {
                                "running"
                            } else {
                                state
                            },
                        ))
                    };
                }
                contract::SNAPSHOT_CHECKPOINT_METHOD
                    if settlement == "published" && host_settling.load(Ordering::Acquire) =>
                {
                    return Ok(snapshot_checkpoint());
                }
                contract::RELEASE_METHOD => {
                    signal.cancel();
                    resume.recv_timeout(BARRIER_BUDGET).unwrap();
                }
                _ => {}
            }
            reply
        });
        let temp = tempfile::tempdir().unwrap();
        let client = snapshot_client(
            &host.endpoint,
            &StateDir::from_path(temp.path().join("state")),
        );
        smol::block_on(async {
            let pending = drop_capture(&client, &snapshot_request()).await;
            pending.cleanup.wait().await;
            assert_eq!(client.0.captures.len().unwrap(), 1);
            settling.store(true, Ordering::Release);
            let retained_during_release = {
                let mut attempt = pin!(client.capture_cleanup_attempt(&pending, future::pending()));
                future::race(
                    async {
                        let _ = attempt.as_mut().await;
                        panic!("release should still be blocked");
                    },
                    entered.cancelled(),
                )
                .await;
                client.0.captures.len().unwrap()
            };
            release.send(()).unwrap();
            assert_eq!(retained_during_release, 0);
            assert_eq!(client.0.captures.len().unwrap(), 0);
        });
    }

    #[test_case(false; "completed_capture")]
    #[test_case(true; "recovered_checkpoint")]
    fn successful_capture_retires_before_stalled_release(recovered: bool) {
        let committed = Arc::new(AtomicBool::new(!recovered));
        let host_committed = committed.clone();
        let entered = CancellationToken::new();
        let signal = entered.clone();
        let (release, resume) = mpsc::channel();
        let host = snapshot_host(move |method, _, reply| {
            match method {
                contract::EXECUTE_METHOD | contract::STATUS_METHOD
                    if !host_committed.load(Ordering::Acquire) =>
                {
                    return Ok(status(reply.unwrap(), "running"));
                }
                contract::SNAPSHOT_CHECKPOINT_METHOD
                    if recovered && host_committed.load(Ordering::Acquire) =>
                {
                    return Ok(snapshot_checkpoint());
                }
                contract::RELEASE_METHOD => {
                    signal.cancel();
                    resume.recv_timeout(BARRIER_BUDGET).unwrap();
                }
                _ => {}
            }
            reply
        });
        let temp = tempfile::tempdir().unwrap();
        let client = snapshot_client(
            &host.endpoint,
            &StateDir::from_path(temp.path().join("state")),
        );
        smol::block_on(async {
            let request = snapshot_request();
            if recovered {
                let pending = drop_capture(&client, &request).await;
                pending.cleanup.wait().await;
                committed.store(true, Ordering::Release);
            }
            let retained_during_release = {
                let mut attempt = pin!(capture(&client, &request));
                future::race(
                    async {
                        let _ = attempt.as_mut().await;
                        panic!("release should still be blocked");
                    },
                    entered.cancelled(),
                )
                .await;
                client.0.captures.len().unwrap()
            };
            release.send(()).unwrap();
            assert_eq!(retained_during_release, 0);
            assert_eq!(client.0.captures.len().unwrap(), 0);
            assert_eq!(
                host.params_for(contract::CANCEL_METHOD).len(),
                usize::from(recovered)
            );
        });
    }

    #[test]
    fn cleanup_deadline_preserves_unresolved_capture() {
        let entered = CancellationToken::new();
        let signal = entered.clone();
        let (release, resume) = mpsc::channel();
        let first = AtomicBool::new(true);
        let host = snapshot_host(move |method, _, reply| {
            if method == contract::CANCEL_METHOD && !first.swap(false, Ordering::AcqRel) {
                signal.cancel();
                resume.recv_timeout(BARRIER_BUDGET).unwrap();
            }
            if matches!(method, contract::EXECUTE_METHOD | contract::STATUS_METHOD) {
                return Ok(status(reply.unwrap(), "running"));
            }
            reply
        });
        let temp = tempfile::tempdir().unwrap();
        let client = snapshot_client(
            &host.endpoint,
            &StateDir::from_path(temp.path().join("state")),
        );
        smol::block_on(async {
            let pending = drop_capture(&client, &snapshot_request()).await;
            pending.cleanup.wait().await;
            let expired = AtomicBool::new(false);
            let deadline = future::poll_fn(|_| {
                if expired.load(Ordering::Acquire) {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            });
            let mut attempt = pin!(client.capture_cleanup_attempt(&pending, deadline));
            future::race(
                async {
                    let _ = attempt.as_mut().await;
                    panic!("cleanup should be waiting for its cancel response");
                },
                entered.cancelled(),
            )
            .await;
            expired.store(true, Ordering::Release);
            let result = future::poll_once(attempt.as_mut()).await;
            release.send(()).unwrap();
            assert_eq!(
                result,
                Some(Err(super::super::RemoteWorkcellError::Timeout.into()))
            );
            assert_eq!(client.0.captures.len().unwrap(), 1);
        });
    }

    #[test]
    fn stale_cleanup_cannot_remove_or_settle_replacement_invocation() {
        let host = snapshot_host(|method, _, reply| {
            if matches!(method, contract::EXECUTE_METHOD | contract::STATUS_METHOD) {
                Ok(status(reply.unwrap(), "running"))
            } else {
                reply
            }
        });
        let temp = tempfile::tempdir().unwrap();
        let client = snapshot_client(
            &host.endpoint,
            &StateDir::from_path(temp.path().join("state")),
        );
        smol::block_on(async {
            let request = snapshot_request();
            let old = drop_capture(&client, &request).await;
            old.cleanup.wait().await;
            let mut replacement = old.clone();
            replacement.handle.invocation_id =
                Some(OperationId::new("replacement-invocation").unwrap());
            replacement.cleanup = Arc::new(CaptureCleanup::default());
            client
                .0
                .captures
                .lock()
                .unwrap()
                .entries
                .insert(request.checkpoint_id.clone(), replacement.clone());
            client
                .0
                .captures
                .remember_terminal(&request.checkpoint_id, &old, WorkspaceError::Cancelled)
                .unwrap();
            client
                .0
                .captures
                .remove(&request.checkpoint_id, &old)
                .unwrap();
            drop(CancelCaptureOnDrop {
                client: client.clone(),
                pending: Some(old),
            });
            let retained = client
                .0
                .captures
                .get(&request.checkpoint_id)
                .unwrap()
                .unwrap();
            assert!(retained.same_operation(&replacement));
            assert!(retained.terminal_error.is_none());
            assert_eq!(
                retained.cleanup.phase.load(Ordering::Acquire),
                super::CLEANUP_IDLE
            );
            assert_eq!(host.params_for(contract::CANCEL_METHOD).len(), 1);
        });
    }
}
