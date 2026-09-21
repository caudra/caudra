use std::{env, fs, path::PathBuf, str::FromStr, time::Duration};

use async_trait::async_trait;
use caudra_agent::agent::tool_dispatch::{self, Emit};
use caudra_agent::tools::{FileReadTracker, ToolEffect, ToolRegistry, interpreter_ctx};
use caudra_agent::workspace_baseline::{BaselineGate, WorkspaceBaseline};
use caudra_agent::{
    AgentEvent, AgentMode, CancelToken, EventSender,
    permissions::{
        PermissionAnswer, PermissionAuthorityProfile, PermissionManager, PermissionResourceAccess,
        PermissionResourceKind, PermissionSubject, PluginRuleStore, RemotePermissionIdentity,
    },
    workspace_transfer::{
        CleanBufferLease, ComparisonKind, FileOutcome, LocalAccess, LocalRootIdentity,
        OrchestrationLimits, PlannedFile, PullBufferGuard, RemoteRootIdentity, TransferAction,
        TransferAuthorization, TransferError, TransferEvent, TransferEvents, TransferFilters,
        TransferJournal, TransferPlan, TransferRoots,
    },
};
use caudra_config::sandbox::TransferPolicy;
use caudra_config::workcell::{
    ExpectedWorkcellId, RemoteWorkcellSelection, WorkcellEndpoint, WorkcellSourceRef,
};
use caudra_config::{Effect, PermissionRule, PermissionsConfig, ToolKey};
use caudra_storage::{
    StateDir,
    auth::{WorkcellCredential, WorkcellCredentialName, WorkcellCredentialRef},
    id::CaudraId,
    remote_operation_journal::{RemoteOperationJournal, RemoteOperationState},
};
use caudra_workcell::{
    LocalTransferPublisher, RemoteWorkcellHost, ReviewedTransferHost, reviewed_workspace_transfer,
};
use caudra_workcell::{
    NamedBearerCredential, RemoteToolResultEnvelope, RemoteWorkcellClient, RemoteWorkcellError,
};
use caudra_workcell::{TransferSession, TransferSessionHost};
use caudra_workspace::PreparedTransferPublication;
use caudra_workspace::WorkspaceError;
use caudra_workspace::{
    ByteRange, LocalTransferAuthorization, LocalTransferCondition, LocalTransferDestination,
    LocalTransferPath, LocalTransferReview, LocalTransferService, LocalTransferSource, Mutation,
    MutationCondition, MutationRequest, OperationId, TransferContent, TransferDigest, TransferMode,
    TransferPublicationRequest, TransferPublicationState, WorkspaceCapability,
    WorkspaceMutationService, WorkspaceTransferService, WriteContent,
};
use caudra_workspace::{
    CheckpointId, DirectoryNavigation, ListRequest, OperationState, ReadBytesRequest,
    ResourceSelector, ScmDiscoverRequest, ScmStatusRequest, SearchRequest, SessionBindingId,
    SnapshotCaptureRequest, SnapshotInspectRequest, SnapshotOperationPreview, ToolPrepareRequest,
    WatchOpenRequest, WatchPollRequest, WorkspaceAssetService, WorkspaceCursor, WorkspacePath,
    WorkspaceReadService, WorkspaceScmReadService, WorkspaceSearchService,
    WorkspaceSnapshotMutationService, WorkspaceSnapshotReadService, WorkspaceWatchService,
};
use caudra_workspace::{
    ScmDiffRequest, ScmDiffTarget, ScmLogRequest, ScmMutation, ScmReadSideRequest, ScmSide,
    WorkspaceScmMutationService,
};
use futures_lite::io::{AsyncReadExt, repeat};
use isahc::AsyncReadResponseExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};
use std::{fmt::Write as _, sync::Arc};
use tokio_util::sync::CancellationToken;

const LIMIT: u32 = 100;
const EXECUTION_MARKER: &str = "executed\n";
const RECOVERED_CONTENT: &str = "exactly once\n";
const TOMBSTONE_CAPACITY: usize = 256;
const BINARY_BYTES: u64 = 6 * 1024 * 1024;
const BINARY_BYTE: u8 = 0xff;
const TRANSFER_CHUNK: usize = 64 * 1024;
const LOCAL_CANARY: &[u8] = b"client-owned canary";
const INVENTORY_FIRST: &str = "inventory-seed/deep/first.bin";
const INVENTORY_SECOND: &str = "inventory-seed/deep/second.bin";
const INVENTORY_RESTART: &str = "inventory-restart/deep/file.bin";
const INVENTORY_CONTENT: &[u8] = b"\xff\0reviewed inventory bytes";
const PRIVATE_STATE_MODE: u32 = 0o700;
const NATIVE_FILE: &str = "native/deep/reviewed.txt";
const ISOLATED_PYTHON_RESOURCE: &str = "isolated-python";
const PYTHON_SUM: &str = "1 + 1";
const PYTHON_SUM_RESULT: &str = "result: 2";
const PYTHON_NO_HOST_ACCESS: &str =
    "python_execution has no filesystem, network, or environment access";
const PYTHON_NO_SOCKET: &str = "Cannot resolve imported module `socket`";
const PERMISSION_DENIED: &str = "Permission denied";
const BATCH_TIMEOUT: Duration = Duration::from_secs(15);
const BATCH_POLL: Duration = Duration::from_millis(10);
const BATCH_FIRST: &str = "batch-first.txt";
const BATCH_SECOND: &str = "batch-second.txt";
const BATCH_WRITE: &str = "batch-write.txt";
const BATCH_CONTENT: &str = "batch completed";
const PENDING_MUTATION: &str = "workspace mutation is blocked by pending operation";
const BATCH_CANCELLED: &str = "batch-cancelled.txt";
const REMOTE_PREPARATION_CAPACITY: usize = 64;
const HELD_PREPARATIONS: usize = 4;

struct TransferTestHost {
    root: PathBuf,
    fault: PathBuf,
    lose_response: AtomicBool,
}
struct TransferCleanLease;
impl CleanBufferLease for TransferCleanLease {}

#[async_trait]
impl TransferAuthorization for TransferTestHost {
    async fn roots(&self, roots: &TransferRoots) -> Result<(), TransferError> {
        if roots.local.canonical_path() == self.root {
            Ok(())
        } else {
            Err(TransferError::Stale)
        }
    }
    async fn local(
        &self,
        _: &TransferRoots,
        _: &WorkspacePath,
        _: LocalAccess,
    ) -> Result<(), TransferError> {
        Ok(())
    }
    async fn review_plan(&self, plan: &TransferPlan) -> Result<(), TransferError> {
        assert!(!plan.review().atomic_across_files);
        assert!(
            plan.review()
                .files
                .iter()
                .all(|file| file.path.as_str().starts_with("inventory-"))
        );
        Ok(())
    }
    async fn review_remote_publication(
        &self,
        _: &TransferPlan,
        file: &PlannedFile,
        prepared: &PreparedTransferPublication,
    ) -> Result<(), TransferError> {
        assert_eq!(prepared.request.create_directories, file.create_directories);
        if self.lose_response.swap(false, Ordering::AcqRel) {
            fs::write(
                self.fault.with_extension("preparation"),
                prepared.operation.preparation_id.as_str(),
            )
            .unwrap();
            fs::write(&self.fault, b"lose reviewed inventory publication response").unwrap();
        }
        Ok(())
    }
}

#[async_trait]
impl PullBufferGuard for TransferTestHost {
    async fn lock_clean(
        &self,
        _: &LocalRootIdentity,
        _: &WorkspacePath,
    ) -> Result<Box<dyn CleanBufferLease>, TransferError> {
        Ok(Box::new(TransferCleanLease))
    }
}
impl TransferEvents for TransferTestHost {
    fn emit(&self, _: TransferEvent) {}
}

struct FactoryLocalApproval;
#[async_trait]
impl LocalTransferAuthorization for FactoryLocalApproval {
    async fn authorize(&self, review: &LocalTransferReview) -> Result<(), WorkspaceError> {
        if review.destination.path.as_str() == INVENTORY_FIRST {
            Ok(())
        } else {
            Err(WorkspaceError::PermissionDenied)
        }
    }
}

async fn production_inventory_transfer(
    client: RemoteWorkcellClient,
    remote_path: &Path,
    fault: &Path,
    selection: &RemoteWorkcellSelection,
    credential: &NamedBearerCredential,
    remote_state: &StateDir,
) -> RemoteWorkcellClient {
    let local = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    fs::set_permissions(state.path(), fs::Permissions::from_mode(PRIVATE_STATE_MODE)).unwrap();
    fs::create_dir_all(local.path().join("inventory-seed/deep")).unwrap();
    for name in [
        INVENTORY_FIRST,
        INVENTORY_SECOND,
        ".env",
        "local-only",
        "remote-only",
    ] {
        fs::write(local.path().join(name), INVENTORY_CONTENT).unwrap();
    }
    fs::write(local.path().join(".gitignore"), "local-only\n").unwrap();
    fs::write(remote_path.join(".gitignore"), "remote-only\n").unwrap();
    fs::create_dir(local.path().join("target")).unwrap();
    fs::write(local.path().join("target/not-selected"), INVENTORY_CONTENT).unwrap();
    let approval = Arc::new(TransferTestHost {
        root: local.path().to_owned(),
        fault: fault.to_owned(),
        lose_response: AtomicBool::new(false),
    });
    let build = |client: RemoteWorkcellClient| {
        reviewed_workspace_transfer(
            local.path().into(),
            state.path().join("local-publications.json"),
            client.clone(),
            RemoteRootIdentity {
                binding: client.session_binding().clone(),
                cursor: client.root_cursor().clone(),
                cwd: WorkspacePath::root(),
            },
            TransferFilters::new(&TransferPolicy::default(), &[]).unwrap(),
            OrchestrationLimits::default(),
            ReviewedTransferHost {
                authorization: approval.clone(),
                local_publication: Arc::new(FactoryLocalApproval),
                buffers: approval.clone(),
                events: approval.clone(),
            },
        )
    };
    let engine = build(client.clone()).await.unwrap();
    let comparison = engine.compare(&CancelToken::none()).await.unwrap();
    assert!(comparison.complete());
    for name in [".env", "local-only", "remote-only", "target"] {
        assert_eq!(
            comparison
                .rows()
                .iter()
                .find(|row| row.path.as_str() == name)
                .unwrap()
                .kind,
            ComparisonKind::Excluded,
            "{name}"
        );
    }
    let selected = [
        WorkspacePath::new(INVENTORY_FIRST).unwrap(),
        WorkspacePath::new(INVENTORY_SECOND).unwrap(),
    ];
    let parents = BTreeSet::from([
        WorkspacePath::new("inventory-seed").unwrap(),
        WorkspacePath::new("inventory-seed/deep").unwrap(),
    ]);
    let plan = engine
        .plan(
            &comparison,
            TransferAction::Seed,
            &selected,
            &parents,
            &CancelToken::none(),
        )
        .await
        .unwrap();
    assert_eq!(
        plan.review().files[0].create_directories.len(),
        parents.len()
    );
    assert!(!remote_path.join("inventory-seed").exists());
    let mut journal = TransferJournal::new(state.path().join("transfers.json")).unwrap();
    let run = engine
        .execute(&plan, &mut journal, &CancelToken::none())
        .await;
    assert!(run.stopped.is_none(), "{:?}", run.stopped);
    assert_eq!(run.outcomes.len(), selected.len());
    for name in [INVENTORY_FIRST, INVENTORY_SECOND] {
        assert_eq!(fs::read(remote_path.join(name)).unwrap(), INVENTORY_CONTENT);
    }
    assert!(!remote_path.join(".env").exists());
    assert!(!remote_path.join("target").exists());
    fs::write(local.path().join(INVENTORY_FIRST), LOCAL_CANARY).unwrap();
    let comparison = engine.compare(&CancelToken::none()).await.unwrap();
    let pull = engine
        .plan(
            &comparison,
            TransferAction::Pull,
            &selected[..1],
            &parents,
            &CancelToken::none(),
        )
        .await
        .unwrap();
    let run = engine
        .execute(&pull, &mut journal, &CancelToken::none())
        .await;
    assert!(run.stopped.is_none(), "{:?}", run.stopped);
    assert_eq!(
        fs::read(local.path().join(INVENTORY_FIRST)).unwrap(),
        INVENTORY_CONTENT
    );
    fs::remove_dir_all(local.path().join("inventory-seed")).unwrap();
    let comparison = engine.compare(&CancelToken::none()).await.unwrap();
    let pull = engine
        .plan(
            &comparison,
            TransferAction::Pull,
            &selected[..1],
            &parents,
            &CancelToken::none(),
        )
        .await
        .unwrap();
    assert_eq!(
        pull.review().files[0].create_directories.len(),
        parents.len()
    );
    assert!(!local.path().join("inventory-seed").exists());
    let run = engine
        .execute(&pull, &mut journal, &CancelToken::none())
        .await;
    assert!(run.stopped.is_none(), "{:?}", run.stopped);
    assert_eq!(
        fs::read(local.path().join(INVENTORY_FIRST)).unwrap(),
        INVENTORY_CONTENT
    );
    assert_eq!(
        journal
            .entries()
            .unwrap()
            .iter()
            .find(|entry| entry.operation_id == pull.review().files[0].operation_id)
            .unwrap()
            .created_directories
            .len(),
        parents.len()
    );
    native_reviewed_session(&client, remote_path, approval.clone()).await;
    fs::create_dir_all(local.path().join("inventory-restart/deep")).unwrap();
    fs::write(local.path().join(INVENTORY_RESTART), INVENTORY_CONTENT).unwrap();
    let comparison = engine.compare(&CancelToken::none()).await.unwrap();
    let plan = engine
        .plan(
            &comparison,
            TransferAction::Seed,
            &[WorkspacePath::new(INVENTORY_RESTART).unwrap()],
            &BTreeSet::from([
                WorkspacePath::new("inventory-restart").unwrap(),
                WorkspacePath::new("inventory-restart/deep").unwrap(),
            ]),
            &CancelToken::none(),
        )
        .await
        .unwrap();
    approval.lose_response.store(true, Ordering::Release);
    let run = engine
        .execute(&plan, &mut journal, &CancelToken::none())
        .await;
    assert_eq!(
        run.outcomes[&plan.review().files[0].operation_id],
        FileOutcome::Unknown
    );
    let count = fs::read_to_string(fault.with_extension("count")).unwrap();
    fs::remove_file(fault).unwrap();
    fs::write(
        fault.with_extension("restart"),
        b"restart standalone server",
    )
    .unwrap();
    drop(engine);
    drop(client);
    let client = RemoteWorkcellClient::connect(
        selection,
        Some(credential.clone()),
        SessionBindingId::new("transfer-session").unwrap(),
        RemoteOperationJournal::open(remote_state).unwrap(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let reopened = build(client.clone()).await.unwrap();
    let run = reopened.reconcile(&mut journal, &CancelToken::none()).await;
    assert!(run.stopped.is_none(), "{:?}", run.stopped);
    assert_eq!(
        run.outcomes[&plan.review().files[0].operation_id],
        FileOutcome::Confirmed
    );
    assert_eq!(
        fs::read(remote_path.join(INVENTORY_RESTART)).unwrap(),
        INVENTORY_CONTENT
    );
    assert_eq!(
        fs::read_to_string(fault.with_extension("count")).unwrap(),
        count
    );
    eprintln!(
        "PASS production inventory, excluded files, reviewed nested directories, Pull, and per-file standalone restart recovery"
    );
    client
}

async fn native_reviewed_session(
    client: &RemoteWorkcellClient,
    remote_path: &Path,
    buffers: Arc<TransferTestHost>,
) {
    let local = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    fs::set_permissions(state.path(), fs::Permissions::from_mode(PRIVATE_STATE_MODE)).unwrap();
    fs::create_dir_all(local.path().join("native/deep")).unwrap();
    fs::write(local.path().join(NATIVE_FILE), INVENTORY_CONTENT).unwrap();
    let permissions = Arc::new(PermissionManager::new_nonpersistent(
        PermissionsConfig::default(),
        local.path().into(),
        Arc::new(PluginRuleStore::default()),
    ));
    let (events, received) = flume::unbounded();
    let deny = Arc::new(AtomicBool::new(false));
    let prompts = smol::spawn({
        let permissions = permissions.clone();
        let deny = deny.clone();
        async move {
            let mut subjects = Vec::new();
            while let Ok(envelope) = received.recv_async().await {
                let caudra_agent::Envelope { event, .. } = envelope;
                if let AgentEvent::PermissionRequest(request) = event {
                    subjects.push(request.subject.clone());
                    assert!(!request.resources.is_empty());
                    if !permissions.answer(
                        &request.id,
                        if deny.load(Ordering::Acquire) {
                            PermissionAnswer::Deny
                        } else {
                            PermissionAnswer::AllowOnce
                        },
                    ) {
                        eprintln!("Rejected native decision: {request:?}");
                        permissions.answer(&request.id, PermissionAnswer::Deny);
                        break;
                    }
                }
            }
            subjects
        }
    });
    let remote_root = WorkspacePath::new("inventory-seed").unwrap();
    let resolved = client
        .resolve_directory_cursor(client.session_binding(), client.root_cursor(), &remote_root)
        .await
        .unwrap();
    let mut session = TransferSession::open(
        local.path().into(),
        client.clone(),
        RemoteRootIdentity {
            binding: client.session_binding().clone(),
            cursor: resolved.cursor,
            cwd: remote_root.clone(),
        },
        &TransferPolicy::default(),
        &StateDir::from_path(state.path().into()),
        TransferSessionHost {
            permissions,
            permission_events: EventSender::new(events, 0),
            buffers: buffers.clone(),
            progress: buffers,
            cancel: CancelToken::none(),
            validity: Arc::new(|| Ok(())),
        },
    )
    .await
    .unwrap();
    let comparison = session.compare(&CancelToken::none()).await.unwrap();
    assert!(comparison.complete());
    let selected = [WorkspacePath::new(NATIVE_FILE).unwrap()];
    let plan = session
        .review(TransferAction::Seed, &selected, &CancelToken::none())
        .await
        .unwrap();
    assert!(
        !remote_path
            .join(remote_root.as_str())
            .join(NATIVE_FILE)
            .exists()
    );
    deny.store(true, Ordering::Release);
    let denied = session
        .execute(plan.digest(), &CancelToken::none())
        .await
        .unwrap();
    assert!(denied.stopped.is_some());
    assert!(
        !remote_path
            .join(remote_root.as_str())
            .join(NATIVE_FILE)
            .exists()
    );
    deny.store(false, Ordering::Release);
    session.compare(&CancelToken::none()).await.unwrap();
    let plan = session
        .review(TransferAction::Seed, &selected, &CancelToken::none())
        .await
        .unwrap();
    let result = session
        .execute(plan.digest(), &CancelToken::none())
        .await
        .unwrap();
    assert!(result.stopped.is_none(), "{:?}", result.stopped);
    assert_eq!(
        fs::read(remote_path.join(remote_root.as_str()).join(NATIVE_FILE)).unwrap(),
        INVENTORY_CONTENT
    );
    assert!(!remote_path.join(NATIVE_FILE).exists());
    assert!(
        session
            .execute(plan.digest(), &CancelToken::none())
            .await
            .is_err()
    );
    drop(session);
    let subjects = prompts.await;
    assert!(
        subjects
            .iter()
            .any(|subject| matches!(subject, PermissionSubject::Native { .. }))
    );
    assert!(
        subjects
            .iter()
            .any(|subject| matches!(subject, PermissionSubject::RemoteNative { .. }))
    );
    eprintln!(
        "PASS shared UI/CLI transfer session, independent nested root, explicit review, native both-end prompts, denial and consumed plan"
    );
}

struct PullAuthorization;

#[async_trait]
impl LocalTransferAuthorization for PullAuthorization {
    async fn authorize(&self, review: &LocalTransferReview) -> Result<(), WorkspaceError> {
        if review.destination.path.as_str() == "pulled.bin" {
            Ok(())
        } else {
            Err(WorkspaceError::PermissionDenied)
        }
    }
}

fn binary_content(size: u64) -> TransferContent {
    let mut hash = Sha256::new();
    let chunk = [BINARY_BYTE; TRANSFER_CHUNK];
    let mut left = size;
    while left > 0 {
        let count = left.min(chunk.len() as u64) as usize;
        hash.update(&chunk[..count]);
        left -= count as u64;
    }
    let mut digest = String::from("sha256:");
    for byte in hash.finalize() {
        write!(digest, "{byte:02x}").unwrap();
    }
    TransferContent {
        digest: TransferDigest::new(digest).unwrap(),
        size_bytes: size,
        mode: TransferMode::Regular,
    }
}

async fn assert_binary(source: LocalTransferSource, expected: u64) {
    let mut reader = source.into_reader();
    let mut buffer = vec![0; TRANSFER_CHUNK].into_boxed_slice();
    let mut received = 0u64;
    loop {
        let count = reader.read(&mut buffer).await.unwrap();
        if count == 0 {
            break;
        }
        assert!(buffer[..count].iter().all(|byte| *byte == BINARY_BYTE));
        received += count as u64;
        assert!(received <= expected);
    }
    assert_eq!(received, expected);
}

#[test]
fn reviewed_transfer() {
    if env::var_os("WORKCELL_TEST_ENDPOINT").is_none() {
        return;
    }
    smol::block_on(async {
        let endpoint = env::var("WORKCELL_TEST_ENDPOINT").unwrap();
        let root = PathBuf::from(env::var_os("WORKCELL_TEST_ROOT").unwrap());
        let state = StateDir::from_path(PathBuf::from(env::var_os("WORKCELL_TEST_STATE").unwrap()));
        let fault = PathBuf::from(env::var_os("WORKCELL_TEST_FAULT").unwrap());
        let selection = RemoteWorkcellSelection {
            source: WorkcellSourceRef::Direct,
            endpoint: WorkcellEndpoint::parse(&endpoint).unwrap(),
            cwd: WorkspacePath::root(),
            credential_ref: Some(
                WorkcellCredentialRef::from_str("credential:integration").unwrap(),
            ),
            expected_server_id: Some(ExpectedWorkcellId::new("integration-server-id").unwrap()),
            expected_workspace_id: Some(
                ExpectedWorkcellId::new("integration-workspace-id").unwrap(),
            ),
        };
        let credential = NamedBearerCredential::new(
            WorkcellCredentialName::new("integration").unwrap(),
            WorkcellCredential::new(
                fs::read_to_string(env::var_os("WORKCELL_TEST_TOKEN_FILE").unwrap()).unwrap(),
            )
            .unwrap(),
        );
        let connect = || {
            RemoteWorkcellClient::connect(
                &selection,
                Some(credential.clone()),
                SessionBindingId::new("transfer-session").unwrap(),
                RemoteOperationJournal::open(&state).unwrap(),
                CancellationToken::new(),
            )
        };
        let client = connect().await.unwrap();
        let client =
            production_inventory_transfer(client, &root, &fault, &selection, &credential, &state)
                .await;
        assert!(
            client
                .workspace_handle()
                .unwrap()
                .capabilities()
                .supports(WorkspaceCapability::ReviewedTransfer)
        );
        assert!(
            !client
                .limits()
                .unwrap()
                .atomic_replace_against_external_writers
        );
        let local = tempfile::tempdir().unwrap();
        fs::create_dir(local.path().join("nested")).unwrap();
        fs::write(local.path().join("nested/transfer.bin"), LOCAL_CANARY).unwrap();
        let binding = client.session_binding();
        let cursor = client.root_cursor();
        let content = binary_content(BINARY_BYTES);
        let stage = client
            .stage(
                binding,
                cursor,
                LocalTransferSource::new(repeat(BINARY_BYTE).take(BINARY_BYTES)),
                &content,
            )
            .await
            .unwrap();
        let sealed = client.seal(&stage).await.unwrap();
        let prepared = client
            .prepare_publication(
                &sealed,
                &TransferPublicationRequest {
                    publication_id: OperationId::new("binary-publication").unwrap(),
                    create_directories: Vec::new(),
                    path: WorkspacePath::new("nested/transfer.bin").unwrap(),
                    condition: MutationCondition::MustNotExist,
                },
            )
            .await
            .unwrap();
        assert!(!root.join("nested/transfer.bin").exists());
        assert_eq!(prepared.review["mutating"], true);
        let mut tampered = prepared.clone();
        tampered.request.path = WorkspacePath::new("unreviewed.bin").unwrap();
        assert!(matches!(
            client.execute_publication(&tampered).await,
            Err(WorkspaceError::Conflict)
        ));
        let result = client.execute_publication(&prepared).await.unwrap();
        assert!(
            matches!(result.state, OperationState::Completed { .. }),
            "{result:?}"
        );
        assert_eq!(
            client.publication_status(&prepared).await.unwrap().state,
            TransferPublicationState::Completed
        );
        assert!(client.pending_remote_operations().is_empty());
        client.release_stage(&stage).await.unwrap();
        assert_eq!(
            fs::read(local.path().join("nested/transfer.bin")).unwrap(),
            LOCAL_CANARY
        );
        let file = WorkspaceTransferService::stat(&client, binding, cursor, &prepared.request.path)
            .await
            .unwrap();
        assert_eq!(file.content, content);
        fs::write(
            fault.with_extension("corrupt"),
            b"corrupt streamed response",
        )
        .unwrap();
        assert!(matches!(
            client.download(&file, None).await,
            Err(WorkspaceError::TransferIntegrity)
        ));
        fs::remove_file(fault.with_extension("corrupt")).unwrap();
        let range = ByteRange {
            start: BINARY_BYTES - 1024,
            end_exclusive: BINARY_BYTES,
        };
        let partial = client.download(&file, Some(range)).await.unwrap();
        assert!(!partial.whole_file_verified);
        assert_binary(partial.source, 1024).await;
        let full = client.download(&file, None).await.unwrap();
        assert!(full.whole_file_verified);
        let publisher =
            LocalTransferPublisher::new(local.path().to_owned(), Arc::new(PullAuthorization))
                .await
                .unwrap();
        let pull = publisher
            .prepare(
                full.source,
                LocalTransferDestination {
                    create_directories: Vec::new(),
                    path: LocalTransferPath::new("pulled.bin").unwrap(),
                    condition: LocalTransferCondition::MustNotExist,
                },
                full.content,
            )
            .await
            .unwrap();
        assert!(!local.path().join("pulled.bin").exists());
        publisher.execute(&pull).await.unwrap();
        assert_binary(
            LocalTransferSource::new(
                smol::fs::File::open(local.path().join("pulled.bin"))
                    .await
                    .unwrap(),
            ),
            BINARY_BYTES,
        )
        .await;
        let read = WorkspaceReadService::read_bytes(
            &client,
            binding,
            cursor,
            &ReadBytesRequest {
                resource: ResourceSelector::Path(prepared.request.path.clone()),
                byte_offset: range.start,
                max_bytes: 1024,
                if_revision: Some(file.revision.clone()),
            },
        )
        .await
        .unwrap();
        assert_eq!(read.bytes, vec![BINARY_BYTE; 1024]);
        let short = binary_content(1024);
        let stage = client
            .stage(
                binding,
                cursor,
                LocalTransferSource::new(repeat(BINARY_BYTE).take(1024)),
                &short,
            )
            .await
            .unwrap();
        let sealed = client.seal(&stage).await.unwrap();
        let mut request = TransferPublicationRequest {
            publication_id: OperationId::new("must-not-replace").unwrap(),
            create_directories: Vec::new(),
            path: prepared.request.path.clone(),
            condition: MutationCondition::MustNotExist,
        };
        assert!(matches!(
            client.prepare_publication(&sealed, &request).await,
            Err(WorkspaceError::Conflict)
        ));
        request.publication_id = OperationId::new("stale-replace").unwrap();
        request.condition = MutationCondition::Matches(file.revision.clone());
        let stale = client.prepare_publication(&sealed, &request).await.unwrap();
        fs::write(root.join("nested/transfer.bin"), LOCAL_CANARY).unwrap();
        assert!(matches!(
            client.download(&file, None).await,
            Err(WorkspaceError::Conflict)
        ));
        let result = client.execute_publication(&stale).await.unwrap();
        assert!(
            matches!(
                result.state,
                OperationState::Failed {
                    side_effects_possible: false,
                    ..
                }
            ),
            "{result:?}"
        );
        assert_eq!(
            fs::read(root.join("nested/transfer.bin")).unwrap(),
            LOCAL_CANARY
        );
        client.release_stage(&stage).await.unwrap();
        let abandoned = client
            .stage(
                binding,
                cursor,
                LocalTransferSource::new(repeat(BINARY_BYTE).take(1024)),
                &short,
            )
            .await
            .unwrap();
        client.seal(&abandoned).await.unwrap();
        client.release_stage(&abandoned).await.unwrap();
        assert!(client.seal(&abandoned).await.is_err());
        let mismatch = client
            .stage(
                binding,
                cursor,
                LocalTransferSource::new(repeat(0).take(1024)),
                &short,
            )
            .await;
        assert!(matches!(mismatch, Err(WorkspaceError::TransferIntegrity)));
        let image = WorkspaceMutationService::execute(
            &client,
            binding,
            cursor,
            &MutationRequest {
                mutations: vec![Mutation::Write {
                    path: WorkspacePath::new("image.png").unwrap(),
                    content: WriteContent::Bytes(vec![BINARY_BYTE; BINARY_BYTES as usize]),
                    condition: MutationCondition::MustNotExist,
                }],
            },
        )
        .await
        .unwrap();
        assert!(
            matches!(image.state, OperationState::Completed { .. }),
            "{image:?}"
        );
        assert_eq!(
            fs::metadata(root.join("image.png")).unwrap().len(),
            BINARY_BYTES
        );
        eprintln!(
            "PASS 6MiB binary Push/Pull, review, Range/If-Match, stale and no-replace, local canary, image binary compatibility"
        );
        for restart in [false, true] {
            let client = connect().await.unwrap();
            let nested = client
                .resolve_directory_cursor(
                    client.session_binding(),
                    client.root_cursor(),
                    &WorkspacePath::new("nested").unwrap(),
                )
                .await
                .unwrap();
            let stage = client
                .stage(
                    client.session_binding(),
                    &nested.cursor,
                    LocalTransferSource::new(repeat(BINARY_BYTE).take(1024)),
                    &short,
                )
                .await
                .unwrap();
            let sealed = client.seal(&stage).await.unwrap();
            let name = if restart { "restart.bin" } else { "lost.bin" };
            let prepared = client
                .prepare_publication(
                    &sealed,
                    &TransferPublicationRequest {
                        publication_id: OperationId::new(name).unwrap(),
                        create_directories: Vec::new(),
                        path: WorkspacePath::new(name).unwrap(),
                        condition: MutationCondition::MustNotExist,
                    },
                )
                .await
                .unwrap();
            fs::write(
                fault.with_extension("preparation"),
                prepared.operation.preparation_id.as_str(),
            )
            .unwrap();
            fs::write(&fault, b"lose execute response").unwrap();
            let result = client.execute_publication(&prepared).await.unwrap();
            assert!(matches!(result.state, OperationState::Indeterminate { .. }));
            assert_eq!(client.pending_remote_operations().len(), 1);
            assert!(matches!(
                client.execute_publication(&prepared).await,
                Err(WorkspaceError::PendingOperation { .. })
            ));
            let overlap = client.prepare_canonical_tool(client.session_binding(), client.root_cursor(), &ToolPrepareRequest { name: "file_write".into(), input: json!({"filePath":format!("nested/{name}"),"content":"forbidden overlapping write"}) }).await.unwrap();
            assert!(matches!(
                client
                    .execute_canonical_tool(
                        client.session_binding(),
                        client.root_cursor(),
                        &overlap.prepared
                    )
                    .await,
                Err(WorkspaceError::PendingOperation { .. })
            ));
            client
                .release_canonical_tool(
                    client.session_binding(),
                    client.root_cursor(),
                    &overlap.prepared,
                )
                .await
                .unwrap();
            let count = fs::read_to_string(fault.with_extension("count")).unwrap();
            fs::remove_file(&fault).unwrap();
            if restart {
                fs::write(fault.with_extension("restart"), b"restart").unwrap();
            }
            fs::write(fault.with_extension("unknown"), b"forget durable result").unwrap();
            drop(client);
            let recovered = connect().await.unwrap();
            assert_eq!(recovered.pending_remote_operations().len(), 1);
            let restored =
                serde_json::from_value(serde_json::to_value(&prepared).unwrap()).unwrap();
            assert_eq!(
                recovered.publication_status(&restored).await.unwrap().state,
                TransferPublicationState::Unknown
            );
            fs::remove_file(fault.with_extension("unknown")).unwrap();
            recovered
                .reconnect(&CancellationToken::new())
                .await
                .unwrap();
            assert!(recovered.pending_remote_operations().is_empty());
            let status = recovered.publication_status(&restored).await.unwrap();
            assert_eq!(status.state, TransferPublicationState::Completed);
            assert_binary(
                recovered
                    .download(&status.file.unwrap(), None)
                    .await
                    .unwrap()
                    .source,
                1024,
            )
            .await;
            assert_eq!(
                fs::read_to_string(fault.with_extension("count")).unwrap(),
                count
            );
            assert_binary(
                LocalTransferSource::new(
                    smol::fs::File::open(root.join("nested").join(name))
                        .await
                        .unwrap(),
                ),
                1024,
            )
            .await;
        }
        eprintln!(
            "PASS lost execute, durable restart recovery, cross-cursor locks, Unknown retains lock, no replay"
        );
    });
}

async fn tool(
    client: &RemoteWorkcellClient,
    cursor: &WorkspaceCursor,
    name: &str,
    input: Value,
) -> RemoteToolResultEnvelope {
    let binding = client.session_binding();
    let prepared = client
        .prepare_canonical_tool(
            binding,
            cursor,
            &ToolPrepareRequest {
                name: name.into(),
                input,
            },
        )
        .await
        .expect(name);
    let mut status = client
        .execute_canonical_tool(binding, cursor, &prepared.prepared)
        .await
        .expect(name);
    let mut after_sequence = 0;
    for _ in 0..100 {
        for event in &status.progress {
            assert_eq!(event.sequence, after_sequence + 1);
            after_sequence = event.sequence;
        }
        assert_eq!(status.progress_metadata.next_sequence, after_sequence + 1);
        match status.state {
            OperationState::Completed { result, .. } => {
                assert!(!result.is_error, "{name}: {}", result.model_output);
                return result;
            }
            OperationState::Running => smol::Timer::after(Duration::from_millis(20)).await,
            state => panic!("{name}: {state:?}"),
        };
        status = client
            .canonical_tool_status_after(binding, cursor, &status.handle, Some(after_sequence))
            .await
            .unwrap();
    }
    panic!("{name}: operation exceeded bounded polling");
}

async fn isolated_python_permissions(client: &RemoteWorkcellClient, root: &Path, state: &StateDir) {
    let registry = Arc::new(ToolRegistry::new());
    RemoteWorkcellHost::new(client.clone())
        .register(&registry)
        .unwrap();
    let config = PermissionsConfig::default();
    assert!(!config.yolo);
    let permissions = Arc::new(PermissionManager::new_nonpersistent(
        config,
        root.to_path_buf(),
        Arc::default(),
    ));
    let policy = || {
        permissions
            .active_policy()
            .into_iter()
            .map(|entry| (entry.source, entry.rule))
            .collect::<Vec<_>>()
    };
    let policy_before = policy();
    let journal = RemoteOperationJournal::open(state).unwrap();
    let snapshot = || {
        let path = journal.path();
        [
            path.to_path_buf(),
            PathBuf::from(format!("{}-wal", path.display())),
        ]
        .map(|path| fs::read(path).ok())
    };
    let journal_before = snapshot();
    let (tx, rx) = flume::unbounded();
    let (_response_tx, response_rx) = flume::unbounded();
    let mut ctx = interpreter_ctx(
        &AgentMode::Build,
        &EventSender::new(tx, 0),
        CancelToken::none(),
        permissions.clone(),
        Arc::new(FileReadTracker::new()),
        Some(Arc::new(smol::lock::Mutex::new(response_rx))),
        registry.clone(),
    );
    ctx.workspace_session = Some(
        caudra_workspace::WorkspaceSession::new(
            client.workspace_handle().unwrap(),
            client.session_binding().clone(),
            client.root_cursor().clone(),
        )
        .unwrap(),
    );
    let tool = registry.get("python_execution").unwrap();
    let invocation = tool.tool.parse(&json!({"code":PYTHON_SUM})).unwrap();
    let intent = invocation.preflight(&ctx).await.unwrap().unwrap();
    assert_eq!(invocation.call_effect(tool.effect), ToolEffect::Isolated);
    assert_eq!(intent.authority, PermissionAuthorityProfile::RemoteResource);
    assert_eq!(intent.resources.len(), 1);
    let resource = &intent.resources[0];
    assert_eq!(
        resource.kind,
        PermissionResourceKind::RemoteResource {
            identity: RemotePermissionIdentity::from_binding(client.session_binding()),
            resource_kind: "code".into(),
        }
    );
    assert!(!resource.value.is_empty());
    assert_eq!(resource.access, Some(PermissionResourceAccess::Execute));
    assert!(!resource.protected && !resource.requires_prompt);
    assert_eq!(
        resource.attributes["display_code"],
        ISOLATED_PYTHON_RESOURCE
    );
    assert_eq!(resource.attributes["operation_kind"], "execute");
    invocation.abandon(&ctx).await;

    for (mode, answer, code) in [
        (AgentMode::Build, PermissionAnswer::AllowOnce, PYTHON_SUM),
        (AgentMode::Build, PermissionAnswer::Deny, PYTHON_SUM),
        (
            AgentMode::RemotePlan(caudra_workspace::PlanRef::new("python-plan").unwrap()),
            PermissionAnswer::AllowOnce,
            PYTHON_SUM,
        ),
        (
            AgentMode::Build,
            PermissionAnswer::AllowOnce,
            "open('python-escape', 'w')",
        ),
        (
            AgentMode::Build,
            PermissionAnswer::AllowOnce,
            "import socket\nsocket.socket()",
        ),
    ] {
        ctx.mode = mode;
        let denied = matches!(answer, PermissionAnswer::Deny);
        let input = json!({"code":code});
        let dispatch = tool_dispatch::run(
            &registry,
            None,
            "ordinary-python".into(),
            "python_execution",
            &input,
            &ctx,
            Emit::Silent,
        );
        let respond = async {
            loop {
                let envelope = rx.recv_async().await.unwrap();
                if let AgentEvent::PermissionRequest(request) = envelope.event {
                    assert_eq!(request.resources, intent.resources);
                    assert!(permissions.answer(&request.id, answer));
                    return;
                }
            }
        };
        let done = futures_lite::future::race(
            async { futures_lite::future::zip(dispatch, respond).await.0 },
            async {
                smol::Timer::after(Duration::from_secs(15)).await;
                panic!("normal remote Python policy did not finish with exactly one prompt");
            },
        )
        .await;
        assert_eq!(done.is_error, denied, "{}", done.output.as_text());
        if code != PYTHON_SUM {
            let diagnostic = if code.starts_with("import") {
                PYTHON_NO_SOCKET
            } else {
                PYTHON_NO_HOST_ACCESS
            };
            assert!(
                done.output.as_text().contains(diagnostic),
                "{}",
                done.output.as_text()
            );
        } else if !denied {
            assert_eq!(done.output.as_text().trim(), PYTHON_SUM_RESULT);
        } else {
            assert!(done.output.as_text().contains(PERMISSION_DENIED));
        }
        assert_eq!(policy(), policy_before);
        assert!(
            permissions
                .structured_conversation_rules_snapshot()
                .is_empty()
        );
        assert!(client.pending_remote_operations().is_empty());
        assert_eq!(
            snapshot(),
            journal_before,
            "isolated execution wrote the durable journal"
        );
    }
    ctx.user_response_rx = None;
    for (name, input) in [
        ("shell", json!({"command":"touch python-shell-grant"})),
        (
            "file_write",
            json!({"filePath":"python-file-grant", "content":"forbidden"}),
        ),
        ("python_execution", json!({"code":PYTHON_SUM})),
    ] {
        let done = tool_dispatch::run(
            &registry,
            None,
            "no-broad-python-grant".into(),
            name,
            &input,
            &ctx,
            Emit::Silent,
        )
        .await;
        assert!(done.is_error, "{name}: {}", done.output.as_text());
        assert!(
            done.output.as_text().contains(PERMISSION_DENIED),
            "{}",
            done.output.as_text()
        );
    }
    assert!(!root.join("python-shell-grant").exists());
    assert!(!root.join("python-file-grant").exists());
    ctx.permissions = Arc::new(PermissionManager::new_nonpersistent(
        PermissionsConfig {
            rules: vec![PermissionRule {
                tool: ToolKey::native("python_execution"),
                scope: None,
                effect: Effect::Deny,
            }],
            ..Default::default()
        },
        root.to_path_buf(),
        Arc::default(),
    ));
    let (_response_tx, response_rx) = flume::unbounded();
    ctx.user_response_rx = Some(Arc::new(smol::lock::Mutex::new(response_rx)));
    let input = json!({"code":PYTHON_SUM});
    let done = futures_lite::future::race(
        tool_dispatch::run(
            &registry,
            None,
            "configured-python-deny".into(),
            "python_execution",
            &input,
            &ctx,
            Emit::Silent,
        ),
        async {
            loop {
                if let AgentEvent::PermissionRequest(_) = rx.recv_async().await.unwrap().event {
                    panic!("configured deny must not offer an approval prompt");
                }
            }
        },
    )
    .await;
    assert!(done.is_error);
    assert!(done.output.as_text().contains(PERMISSION_DENIED));
    assert_eq!(snapshot(), journal_before);
    assert!(!root.join("python-escape").exists());
    eprintln!(
        "PASS non-yolo Python exact remote authority, AllowOnce, deny, plan, no persistent grants/journal, no filesystem/network access"
    );
}

async fn concurrent_registry_regressions(
    client: &RemoteWorkcellClient,
    root: &Path,
    selection: &RemoteWorkcellSelection,
    credential: &NamedBearerCredential,
    state: &StateDir,
) {
    let capacity_client = RemoteWorkcellClient::connect(
        selection,
        Some(credential.clone()),
        SessionBindingId::new("capacity-session").unwrap(),
        RemoteOperationJournal::open(state).unwrap(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let registry = Arc::new(ToolRegistry::new());
    RemoteWorkcellHost::new(client.clone())
        .register(&registry)
        .unwrap();
    let permissions = Arc::new(PermissionManager::new_nonpersistent(
        PermissionsConfig {
            yolo: true,
            ..Default::default()
        },
        root.to_path_buf(),
        Arc::default(),
    ));
    let (tx, rx) = flume::unbounded();
    let mut ctx = interpreter_ctx(
        &AgentMode::Build,
        &EventSender::new(tx, 0),
        CancelToken::none(),
        permissions.clone(),
        Arc::new(FileReadTracker::new()),
        None,
        registry.clone(),
    );
    ctx.workspace_session = Some(
        caudra_workspace::WorkspaceSession::new(
            client.workspace_handle().unwrap(),
            client.session_binding().clone(),
            client.root_cursor().clone(),
        )
        .unwrap(),
    );
    let fault = PathBuf::from(env::var_os("WORKCELL_TEST_FAULT").unwrap());
    let gate = fault.with_file_name("batch-execute-gate");
    let trace = fault.with_file_name("batch-rpc-trace");
    let baseline_state = tempfile::tempdir().unwrap();
    let baseline = WorkspaceBaseline::new_workspace_session(
        StateDir::from_path(baseline_state.path().into()),
        CaudraId::generate(),
        ctx.workspace_session.clone().unwrap(),
        client.stored_binding().clone(),
        true,
    );
    ctx.baseline = Some(BaselineGate::new(baseline.clone(), None));
    let first = json!({"command":format!("printf '{BATCH_CONTENT}' > {BATCH_FIRST}; printf '{BATCH_CONTENT}'")});
    let second = json!({"command":format!("printf '{BATCH_CONTENT}' > {BATCH_SECOND}; printf '{BATCH_CONTENT}'")});
    let write = json!({"filePath":BATCH_WRITE,"content":BATCH_CONTENT});
    let run = |id: &'static str, name, input| {
        tool_dispatch::run(&registry, None, id.into(), name, input, &ctx, Emit::Notify)
    };
    let exercise = async {
        assert!(client.pending_remote_operations().is_empty());
        fs::write(&trace, "").unwrap();
        fs::write(&gate, "hold first execute before forwarding").unwrap();
        let overlap = async {
            while !gate.with_extension("entered").exists() {
                smol::Timer::after(BATCH_POLL).await;
            }
            let pending = client.pending_remote_operations();
            assert_eq!(pending.len(), 1);
            assert!(!root.join(BATCH_FIRST).exists());
            let queued = async {
                let (shell, file) = futures_lite::future::zip(
                    run("batch-second", "shell", &second),
                    run("batch-write", "file_write", &write),
                )
                .await;
                assert!(!shell.is_error, "{}", shell.output.as_text());
                assert!(shell.output.as_text().contains(BATCH_CONTENT));
                assert!(!file.is_error, "{}", file.output.as_text());
            };
            let release = async {
                let mut started = BTreeSet::new();
                while started.len() < 2 {
                    if let AgentEvent::ToolStart(start) = rx.recv_async().await.unwrap().event
                        && matches!(start.id.as_str(), "batch-second" | "batch-write")
                    {
                        started.insert(start.id);
                    }
                }
                let diagnostics = fault.with_file_name("rpc-diagnostics");
                fs::write(&diagnostics, "").unwrap();
                let (cancel, token) = CancelToken::new();
                let mut cancelled_ctx = ctx.clone();
                cancelled_ctx.cancel = token;
                let cancelled_input = json!({"filePath":BATCH_CANCELLED, "content":BATCH_CONTENT});
                let cancelled = tool_dispatch::run(
                    &registry,
                    None,
                    "batch-cancelled".into(),
                    "file_write",
                    &cancelled_input,
                    &cancelled_ctx,
                    Emit::Notify,
                );
                let cancel_when_full = async {
                    loop {
                        if let AgentEvent::ToolStart(start) = rx.recv_async().await.unwrap().event
                            && start.id == "batch-cancelled"
                        {
                            break;
                        }
                    }
                    let records = fs::read_to_string(&diagnostics).unwrap();
                    let prepared: Value = records
                        .lines()
                        .map(|line| serde_json::from_str::<Value>(line).unwrap())
                        .find(|record| {
                            record["method"] == "ai.workcell/prepare"
                                && record["tool"] == "file_write"
                        })
                        .unwrap();
                    let preparation_id = prepared["preparationId"].as_str().unwrap();
                    let request = ToolPrepareRequest {
                        name: "file_read".into(),
                        input: json!({"filePath":"fixture.txt"}),
                    };
                    let mut fillers = Vec::new();
                    for _ in HELD_PREPARATIONS..REMOTE_PREPARATION_CAPACITY {
                        fillers.push(
                            capacity_client
                                .prepare_canonical_tool(
                                    capacity_client.session_binding(),
                                    capacity_client.root_cursor(),
                                    &request,
                                )
                                .await
                                .unwrap(),
                        );
                    }
                    let full = capacity_client
                        .prepare_canonical_tool(
                            capacity_client.session_binding(),
                            capacity_client.root_cursor(),
                            &request,
                        )
                        .await;
                    let error = full.err();
                    assert!(matches!(error, Some(WorkspaceError::Conflict)), "{error:?}");
                    assert!(!root.join(BATCH_CANCELLED).exists());
                    cancel.cancel();
                    loop {
                        let released = fs::read_to_string(&diagnostics)
                            .unwrap()
                            .lines()
                            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                            .any(|record| {
                                record["method"] == "ai.workcell/release"
                                    && record["preparationId"] == preparation_id
                                    && record["result"]["released"] == true
                            });
                        if released {
                            break;
                        }
                        smol::Timer::after(BATCH_POLL).await;
                    }
                    let replacement = capacity_client
                        .prepare_canonical_tool(
                            capacity_client.session_binding(),
                            capacity_client.root_cursor(),
                            &request,
                        )
                        .await
                        .unwrap();
                    fillers.push(replacement);
                    for prepared in fillers {
                        capacity_client
                            .release_canonical_tool(
                                capacity_client.session_binding(),
                                capacity_client.root_cursor(),
                                &prepared.prepared,
                            )
                            .await
                            .unwrap();
                    }
                };
                let (cancelled, ()) = futures_lite::future::zip(cancelled, cancel_when_full).await;
                assert!(cancelled.is_error);
                assert!(
                    cancelled.output.as_text().to_lowercase().contains("cancel"),
                    "{}",
                    cancelled.output.as_text()
                );
                assert!(!root.join(BATCH_CANCELLED).exists());
                assert_eq!(client.pending_remote_operations().len(), 1);
                eprintln!(
                    "PASS queued cancellation releases unsent preparation, full 64-slot ledger admits replacement, no cancelled file or new journal row"
                );
                assert!(!root.join(BATCH_SECOND).exists());
                assert!(!root.join(BATCH_WRITE).exists());
                let retained = client.pending_remote_operations();
                assert_eq!(retained.len(), 1);
                assert_eq!(retained[0].operation_id, pending[0].operation_id);
                let records = fs::read_to_string(&trace).unwrap();
                let records = records.lines().collect::<Vec<_>>();
                assert_eq!(
                    records.len(),
                    1,
                    "queued calls dispatched or reconciled before release: {records:?}"
                );
                let record: Value = serde_json::from_str(records[0]).unwrap();
                assert_eq!(record["method"], "ai.workcell/execute");
                fs::write(gate.with_extension("release"), "release").unwrap();
            };
            futures_lite::future::zip(queued, release).await;
        };
        futures_lite::future::zip(
            async {
                let first = run("batch-first", "shell", &first).await;
                assert!(!first.is_error, "{}", first.output.as_text());
                assert!(first.output.as_text().contains(BATCH_CONTENT));
            },
            overlap,
        )
        .await;
        assert_eq!(
            fs::read_to_string(root.join(BATCH_FIRST)).unwrap(),
            BATCH_CONTENT
        );
        assert!(client.pending_remote_operations().is_empty());
        assert!(baseline.is_captured());
        for path in [BATCH_SECOND, BATCH_WRITE] {
            assert_eq!(fs::read_to_string(root.join(path)).unwrap(), BATCH_CONTENT);
        }
        let mut next_ctx = ctx.clone();
        let next_head = Some(CaudraId::generate());
        next_ctx.baseline = Some(BaselineGate::new(baseline.clone(), next_head));
        fs::write(
            fault.with_file_name("batch-prepare-barrier"),
            "three concurrent preparations",
        )
        .unwrap();
        let next_write = tool_dispatch::run(
            &registry,
            None,
            "batch-rewrite".into(),
            "file_write",
            &write,
            &next_ctx,
            Emit::Silent,
        );
        let (first_result, (second_result, write_result)) = futures_lite::future::zip(
            run("batch-first-next-head", "shell", &first),
            futures_lite::future::zip(run("batch-second-next-head", "shell", &second), next_write),
        )
        .await;
        for (name, done) in [
            ("first shell", first_result),
            ("second shell", second_result),
            ("file_write", write_result),
        ] {
            assert!(
                !done.is_error,
                "fresh snapshot concurrent {name}: {}",
                done.output.as_text()
            );
        }
        assert!(client.pending_remote_operations().is_empty());
        assert!(baseline.remote_capture(next_head).unwrap().is_some());
        assert!(
            permissions
                .structured_conversation_rules_snapshot()
                .is_empty()
        );
    };
    futures_lite::future::race(exercise, async {
        smol::Timer::after(BATCH_TIMEOUT).await;
        panic!(
            "concurrent canonical dispatch did not finish: entered={}, trace={:?}",
            gate.with_extension("entered").exists(),
            fs::read_to_string(&trace)
        );
    })
    .await;
    eprintln!(
        "PASS concurrent two-shell/file_write batch: same-client queue, no execute before release, all artifacts and shell output, no unresolved rows, unsynchronized fresh automatic snapshot"
    );
}

async fn canonical_registry_regressions(client: &RemoteWorkcellClient, root: &Path) {
    let registry = Arc::new(ToolRegistry::new());
    RemoteWorkcellHost::new(client.clone())
        .register(&registry)
        .unwrap();
    let (tx, _rx) = flume::unbounded();
    let permissions = Arc::new(PermissionManager::new_nonpersistent(
        PermissionsConfig {
            yolo: true,
            ..Default::default()
        },
        root.to_path_buf(),
        Arc::default(),
    ));
    let mut ctx = interpreter_ctx(
        &AgentMode::RemotePlan(caudra_workspace::PlanRef::new("fixture-plan").unwrap()),
        &EventSender::new(tx, 0),
        CancelToken::none(),
        permissions,
        Arc::new(FileReadTracker::new()),
        None,
        registry.clone(),
    );
    ctx.workspace_session = Some(
        caudra_workspace::WorkspaceSession::new(
            client.workspace_handle().unwrap(),
            client.session_binding().clone(),
            client.root_cursor().clone(),
        )
        .unwrap(),
    );
    for command in ["pwd", "ls", "pwd && ls"] {
        let done = tool_dispatch::run(
            &registry,
            None,
            "plan-read".into(),
            "shell",
            &json!({"command":command}),
            &ctx,
            Emit::Silent,
        )
        .await;
        assert!(!done.is_error, "{command}: {}", done.output.as_text());
    }
    for command in [
        "pwd > forbidden-plan-write",
        "ls; touch forbidden-plan-write",
        "ls $(touch forbidden-plan-write)",
        "true",
    ] {
        let done = tool_dispatch::run(
            &registry,
            None,
            "plan-write".into(),
            "shell",
            &json!({"command":command}),
            &ctx,
            Emit::Silent,
        )
        .await;
        assert!(done.is_error, "{command}");
        assert!(!root.join("forbidden-plan-write").exists());
    }
    let done = tool_dispatch::run(
        &registry,
        None,
        "isolated-python".into(),
        "python_execution",
        &json!({"code":"1 + 1"}),
        &ctx,
        Emit::Silent,
    )
    .await;
    assert!(!done.is_error, "{}", done.output.as_text());
    assert!(client.pending_remote_operations().is_empty());
    ctx.mode = AgentMode::Build;
    let progress_fault =
        PathBuf::from(env::var_os("WORKCELL_TEST_FAULT").unwrap()).with_extension("progress");
    for lose_progress in [false, true] {
        if lose_progress {
            fs::write(&progress_fault, "lose progress, retain outcome").unwrap();
        }
        let invocation = registry
            .get("shell")
            .unwrap()
            .tool
            .parse(&json!({"command":"printf completed > progress-completed; seq 1 100000"}))
            .unwrap();
        invocation.preflight(&ctx).await.unwrap();
        let done = invocation.execute(&ctx).await;
        assert!(!done.is_error, "{:?}", done.output);
        assert_eq!(
            fs::read_to_string(root.join("progress-completed")).unwrap(),
            "completed"
        );
        assert!(client.pending_remote_operations().is_empty());
        if lose_progress {
            assert!(
                done.annotation
                    .as_deref()
                    .is_some_and(|annotation| annotation.contains("progress is partial"))
            );
            fs::remove_file(&progress_fault).unwrap();
        }
    }
    let done = tool_dispatch::run(
        &registry,
        None,
        "denied-fetch".into(),
        "webfetch",
        &json!({"url":"https://example.com/"}),
        &ctx,
        Emit::Silent,
    )
    .await;
    assert!(done.is_error);
    assert!(
        !done.output.as_text().contains("indeterminate"),
        "{}",
        done.output.as_text()
    );
    eprintln!(
        "PASS actual registry Python, read/write/unknown plan shell, large progress, definitive network denial"
    );
    let trace = PathBuf::from(env::var_os("WORKCELL_TEST_RPC_TRACE").unwrap());
    fs::write(&trace, "").unwrap();
    let invocation = registry
        .get("shell")
        .unwrap()
        .tool
        .parse(&json!({"command":"printf ready > dropped-started; sleep 60"}))
        .unwrap();
    invocation.preflight(&ctx).await.unwrap();
    futures_lite::future::race(
        async {
            let result = invocation.execute(&ctx).await;
            panic!("execution completed before drop: {:?}", result.output);
        },
        async {
            loop {
                if root.join("dropped-started").exists() {
                    break;
                }
                smol::Timer::after(Duration::from_millis(20)).await;
            }
        },
    )
    .await;
    futures_lite::future::race(
        async {
            loop {
                let records = fs::read_to_string(&trace).unwrap();
                let records = records
                    .lines()
                    .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                    .collect::<Vec<_>>();
                let cancelled = records
                    .iter()
                    .any(|record| record["method"] == "ai.workcell/cancel");
                let reconciled = records.iter().any(|record| {
                    record["method"] == "ai.workcell/status"
                        && record["result"]["state"] == "indeterminate"
                });
                if cancelled && reconciled {
                    break;
                }
                smol::Timer::after(Duration::from_millis(20)).await;
            }
        },
        async {
            smol::Timer::after(Duration::from_secs(15)).await;
            panic!("dropped invocation did not independently cancel and reconcile");
        },
    )
    .await;
    assert_eq!(client.pending_remote_operations().len(), 1);
    let read = tool_dispatch::run(
        &registry,
        None,
        "read-while-locked".into(),
        "file_index",
        &json!({"path":"."}),
        &ctx,
        Emit::Silent,
    )
    .await;
    assert!(!read.is_error, "{}", read.output.as_text());
    let isolated = tool_dispatch::run(
        &registry,
        None,
        "python-while-locked".into(),
        "python_execution",
        &json!({"code":"2 + 2"}),
        &ctx,
        Emit::Silent,
    )
    .await;
    assert!(!isolated.is_error, "{}", isolated.output.as_text());
    let blocked = client
        .prepare_canonical_tool(
            client.session_binding(),
            client.root_cursor(),
            &ToolPrepareRequest {
                name: "file_write".into(),
                input: json!({"filePath":"blocked.txt", "content":"blocked"}),
            },
        )
        .await;
    let blocked = blocked.unwrap();
    let result = client
        .execute_canonical_tool(
            client.session_binding(),
            client.root_cursor(),
            &blocked.prepared,
        )
        .await;
    assert!(matches!(
        result,
        Err(WorkspaceError::PendingOperation { .. })
    ));
    client
        .release_canonical_tool(
            client.session_binding(),
            client.root_cursor(),
            &blocked.prepared,
        )
        .await
        .unwrap();
    assert!(!root.join("blocked.txt").exists());
    let pending = client.pending_remote_operations();
    let dispatch_trace = PathBuf::from(env::var_os("WORKCELL_TEST_FAULT").unwrap())
        .with_file_name("batch-rpc-trace");
    fs::write(&dispatch_trace, "").unwrap();
    let blocked = registry
        .get("file_write")
        .unwrap()
        .tool
        .parse(&json!({"filePath":"blocked.txt", "content":"blocked"}))
        .unwrap();
    blocked.preflight(&ctx).await.unwrap();
    let result = futures_lite::future::race(blocked.execute(&ctx), async {
        smol::Timer::after(BATCH_TIMEOUT).await;
        panic!("unresolved blocker must be refused, not queued indefinitely");
    })
    .await;
    assert!(result.is_error);
    let error = result.output.unwrap_err();
    assert!(error.starts_with(PENDING_MUTATION), "{error}");
    assert!(!error.contains("unknown"), "{error}");
    assert!(!error.contains("indeterminate"), "{error}");
    assert!(result.annotation.is_none());
    assert!(fs::read_to_string(&dispatch_trace).unwrap().is_empty());
    assert!(!root.join("blocked.txt").exists());
    let retained = client.pending_remote_operations();
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].operation_id, pending[0].operation_id);
    // Only this fixture's operation is acknowledged; no user state is opened.
    client
        .acknowledge_pending_operation(&pending[0].operation_id)
        .unwrap();
    eprintln!(
        "PASS dropped future sends cancel/status, retains uncertain effects lock, permits index and isolated Python, blocks writes"
    );
}

#[test]
fn unsupported_server() {
    if env::var("WORKCELL_TEST_UNSUPPORTED").as_deref() != Ok("1") {
        return;
    }
    let selection = RemoteWorkcellSelection {
        source: WorkcellSourceRef::Direct,
        endpoint: WorkcellEndpoint::parse(&env::var("WORKCELL_TEST_ENDPOINT").unwrap()).unwrap(),
        cwd: WorkspacePath::root(),
        credential_ref: Some(WorkcellCredentialRef::from_str("credential:integration").unwrap()),
        expected_server_id: None,
        expected_workspace_id: None,
    };
    let credential = NamedBearerCredential::new(
        WorkcellCredentialName::new("integration").unwrap(),
        WorkcellCredential::new(
            fs::read_to_string(env::var_os("WORKCELL_TEST_TOKEN_FILE").unwrap()).unwrap(),
        )
        .unwrap(),
    );
    let state = StateDir::from_path(PathBuf::from(env::var_os("WORKCELL_TEST_STATE").unwrap()));
    let result = smol::block_on(RemoteWorkcellClient::connect(
        &selection,
        Some(credential),
        SessionBindingId::new("unsupported-session").unwrap(),
        RemoteOperationJournal::open(&state).unwrap(),
        CancellationToken::new(),
    ));
    assert_eq!(result.unwrap_err(), RemoteWorkcellError::CapabilityMismatch);
    eprintln!("PASS unsupported server refused before catalog or transfer downgrade");
}

#[test]
fn authenticated_local() {
    let Ok(endpoint) = env::var("WORKCELL_TEST_ENDPOINT") else {
        eprintln!("skipped: run scripts/test-workcell-local.py with WORKCELL_TEST_BINARY");
        return;
    };
    smol::block_on(async {
        let probe = isahc::send_async(isahc::Request::post(&endpoint).header("Content-Type", "application/json").header("Accept", "application/json, text/event-stream").header("mcp-protocol-version", "2026-07-28").body(r#"{"jsonrpc":"2.0","id":1,"method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{},"io.modelcontextprotocol/clientInfo":{"name":"integration","version":"1"}}}}"#).unwrap()).await.expect("local TLS trust probe");
        let status = probe.status();
        let mut probe = probe;
        assert_eq!(status, 401, "{}", probe.text().await.unwrap());
        let root = PathBuf::from(env::var_os("WORKCELL_TEST_ROOT").unwrap());
        let state = StateDir::from_path(PathBuf::from(env::var_os("WORKCELL_TEST_STATE").unwrap()));
        let selection = RemoteWorkcellSelection {
            source: WorkcellSourceRef::Direct,
            endpoint: WorkcellEndpoint::parse(&endpoint).unwrap(),
            cwd: WorkspacePath::new(".").unwrap(),
            credential_ref: Some(
                WorkcellCredentialRef::from_str("credential:integration").unwrap(),
            ),
            expected_server_id: Some(ExpectedWorkcellId::new("integration-server-id").unwrap()),
            expected_workspace_id: Some(
                ExpectedWorkcellId::new("integration-workspace-id").unwrap(),
            ),
        };
        let credential = NamedBearerCredential::new(
            WorkcellCredentialName::new("integration").unwrap(),
            WorkcellCredential::new(
                fs::read_to_string(env::var_os("WORKCELL_TEST_TOKEN_FILE").unwrap()).unwrap(),
            )
            .unwrap(),
        );
        let client = RemoteWorkcellClient::connect(
            &selection,
            Some(credential.clone()),
            SessionBindingId::new("integration-session").unwrap(),
            RemoteOperationJournal::open(&state).unwrap(),
            CancellationToken::new(),
        )
        .await
        .expect("authenticated handshake");
        let binding = client.session_binding();
        let cursor = client.root_cursor();
        client.workspace_handle().unwrap();
        let resolved_root = client
            .resolve_directory_cursor(binding, cursor, &WorkspacePath::root())
            .await
            .unwrap();
        assert_eq!(resolved_root.cursor.scope(), cursor.scope());
        assert_eq!(client.canonical_catalog().len(), 17);
        assert!(
            client
                .workspace_handle()
                .unwrap()
                .capabilities()
                .supports(WorkspaceCapability::ReviewedTransfer)
        );
        for name in [
            "file_read",
            "file_glob",
            "file_grep",
            "file_write",
            "file_edit",
            "file_apply_patch",
            "shell",
            "file_index",
            "code_map",
            "code_context",
            "code_refs",
            "code_impact",
            "code_expand",
            "websearch",
            "webfetch",
            "python_execution",
            "execution_environment",
        ] {
            assert!(
                client.canonical_catalog().contains_key(name),
                "missing {name}"
            );
        }
        eprintln!("PASS authenticated handshake, full catalog, validated workspace capabilities");
        Box::pin(isolated_python_permissions(&client, &root, &state)).await;
        Box::pin(concurrent_registry_regressions(
            &client,
            &root,
            &selection,
            &credential,
            &state,
        ))
        .await;
        Box::pin(canonical_registry_regressions(&client, &root)).await;
        let assets = WorkspaceAssetService::discover(&client, binding, cursor)
            .await
            .unwrap();
        assert!(!assets.assets.is_empty());
        let session = caudra_workspace::WorkspaceSession::new(
            client.workspace_handle().unwrap(),
            binding.clone(),
            cursor.clone(),
        )
        .unwrap();
        let context = caudra_agent::remote_project_context::load_remote_project_context(&session)
            .await
            .unwrap();
        assert_eq!(context.skills().len(), 1);
        assert_eq!(context.instructions().len(), 1);
        for command in ["status", "pending", "reconnect", "reconcile"] {
            let status = caudra_workspace::execute_workspace_control(&session, command)
                .await
                .unwrap();
            assert!(status.contains("pending operations: 0"));
        }
        let nested = client
            .navigate_directory(
                binding,
                cursor,
                &DirectoryNavigation::new("nested").unwrap(),
            )
            .await
            .unwrap();
        let parent = client
            .navigate_directory(
                binding,
                &nested.cursor,
                &DirectoryNavigation::new("..").unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(parent.cursor, *cursor);
        let sibling = client
            .navigate_directory(
                binding,
                &nested.cursor,
                &DirectoryNavigation::new("../nested").unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(sibling.cursor, nested.cursor);
        assert!(
            client
                .navigate_directory(binding, cursor, &DirectoryNavigation::new("..").unwrap())
                .await
                .is_err()
        );
        tool(
            &client,
            &nested.cursor,
            "file_read",
            json!({"filePath":"fixture.txt"}),
        )
        .await;
        drop(session);
        tool(&client, cursor, "file_index", json!({"path":"."})).await;
        tool(&client, cursor, "execution_environment", json!({})).await;
        let python = tool(&client, cursor, "python_execution", json!({"code":"1 + 1"})).await;
        assert!(python.model_output.contains('2'));
        for nested in [false, true] {
            let resolved;
            let current = if nested {
                resolved = client
                    .resolve_directory_cursor(
                        binding,
                        cursor,
                        &WorkspacePath::new("nested").unwrap(),
                    )
                    .await
                    .unwrap();
                &resolved.cursor
            } else {
                cursor
            };
            tool(
                &client,
                current,
                "file_read",
                json!({"filePath":"fixture.txt"}),
            )
            .await;
            tool(
                &client,
                current,
                "file_write",
                json!({"filePath":"written.txt","content":"before\n"}),
            )
            .await;
            tool(&client, current, "file_apply_patch", json!({"patchText":"*** Begin Patch\n*** Update File: written.txt\n@@\n-before\n+after\n*** End Patch"})).await;
            let shell = tool(
                &client,
                current,
                "shell",
                json!({"command":"pwd","timeout":1000}),
            )
            .await;
            let directory = if nested {
                root.join("nested")
            } else {
                root.clone()
            };
            assert!(
                shell.model_output.contains(directory.to_str().unwrap()),
                "nested={nested}: {} {:?}",
                shell.model_output,
                shell.structured_content
            );
            assert_eq!(
                fs::read_to_string(directory.join("written.txt")).unwrap(),
                "after\n"
            );
        }
        eprintln!(
            "PASS assets, index, canonical read/write/patch/shell before and after nested cwd"
        );
        let page = client
            .list(
                binding,
                cursor,
                &ListRequest {
                    parent: ResourceSelector::Current,
                    recursive: true,
                    continuation: None,
                    limit: LIMIT,
                },
            )
            .await
            .unwrap();
        assert!(!page.resources.is_empty());
        let search = client
            .search(
                binding,
                cursor,
                &SearchRequest {
                    query: "after".into(),
                    include: None,
                    root: ResourceSelector::Current,
                    max_results: LIMIT,
                    continuation: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(search.hits.len(), 2);
        let bytes = client
            .read_bytes(
                binding,
                cursor,
                &ReadBytesRequest {
                    resource: ResourceSelector::Path(WorkspacePath::new("written.txt").unwrap()),
                    byte_offset: 1,
                    max_bytes: 3,
                    if_revision: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(bytes.bytes, b"fte");
        let watch = WorkspaceWatchService::open(
            &client,
            binding,
            cursor,
            &WatchOpenRequest {
                root: ResourceSelector::Current,
                recursive: true,
            },
        )
        .await
        .unwrap();
        tool(
            &client,
            cursor,
            "file_write",
            json!({"filePath":"watched.txt","content":"watch\n"}),
        )
        .await;
        let events = WorkspaceWatchService::poll(
            &client,
            binding,
            cursor,
            &WatchPollRequest {
                subscription_id: watch.subscription_id.clone(),
                cursor: watch.cursor,
                max_events: LIMIT,
                max_bytes: 65536,
                wait_ms: 1000,
            },
        )
        .await
        .unwrap();
        assert!(!events.events.is_empty());
        WorkspaceWatchService::close(&client, binding, cursor, &watch.subscription_id)
            .await
            .unwrap();
        let repository = WorkspaceScmReadService::discover(
            &client,
            binding,
            cursor,
            &ScmDiscoverRequest {
                path: WorkspacePath::new(".").unwrap(),
            },
        )
        .await
        .unwrap();
        let repository = repository.repository.handle;
        let scm = WorkspaceScmReadService::status(
            &client,
            binding,
            cursor,
            &ScmStatusRequest {
                repository_handle: repository.clone(),
                page_size: LIMIT,
                continuation: None,
            },
        )
        .await
        .unwrap();
        assert!(!scm.entries.is_empty());
        let log = WorkspaceScmReadService::log(
            &client,
            binding,
            cursor,
            &ScmLogRequest {
                repository_handle: repository.clone(),
                page_size: LIMIT,
                continuation: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(log.commits.len(), 1);
        let side = WorkspaceScmReadService::read_side(
            &client,
            binding,
            cursor,
            &ScmReadSideRequest {
                repository_handle: repository.clone(),
                path: WorkspacePath::new("fixture.txt").unwrap(),
                side: ScmSide::Head,
                start_line: 1,
                max_lines: LIMIT,
                max_bytes: 65536,
            },
        )
        .await
        .unwrap();
        assert_eq!(side.content, "original");
        for mutation in [
            ScmMutation::Stage {
                paths: vec![WorkspacePath::new("written.txt").unwrap()],
            },
            ScmMutation::Unstage {
                paths: vec![WorkspacePath::new("written.txt").unwrap()],
            },
        ] {
            let prepared = WorkspaceScmMutationService::prepare(
                &client,
                binding,
                cursor,
                &repository,
                &mutation,
            )
            .await
            .unwrap();
            let result = WorkspaceScmMutationService::execute(&client, binding, cursor, &prepared)
                .await
                .unwrap();
            assert!(
                matches!(result.state, OperationState::Completed { .. }),
                "{result:?}"
            );
            let diff = WorkspaceScmReadService::diff(
                &client,
                binding,
                cursor,
                &ScmDiffRequest {
                    repository_handle: repository.clone(),
                    target: ScmDiffTarget::Staged,
                    path: None,
                    max_lines: LIMIT,
                    max_bytes: 65536,
                    continuation: None,
                },
            )
            .await
            .unwrap();
            assert_eq!(
                diff.lines.is_empty(),
                matches!(mutation, ScmMutation::Unstage { .. })
            );
        }
        eprintln!(
            "PASS recursive list/search, ranged authenticated bytes, watch open/poll/close, SCM discover/status/log/read-side/stage/diff/unstage"
        );
        let snapshot = WorkspaceSnapshotReadService::capture(
            &client,
            binding,
            &resolved_root.cursor,
            &SnapshotCaptureRequest {
                checkpoint_id: CheckpointId::new("integration-checkpoint").unwrap(),
                label: None,
            },
        )
        .await
        .unwrap();
        let inspected = WorkspaceSnapshotReadService::inspect(
            &client,
            binding,
            cursor,
            &SnapshotInspectRequest {
                snapshot_id: snapshot.snapshot.snapshot_id.clone(),
                page_size: LIMIT,
                continuation: None,
            },
        )
        .await
        .unwrap();
        assert!(!inspected.files.is_empty());
        tool(
            &client,
            cursor,
            "file_write",
            json!({"filePath":"written.txt","content":"changed\n"}),
        )
        .await;
        let restore = client
            .prepare_restore(binding, cursor, &snapshot.snapshot.snapshot_id)
            .await
            .unwrap();
        let SnapshotOperationPreview::Restore(preview) = &restore.preview else {
            panic!("restore preview")
        };
        let restored =
            WorkspaceSnapshotMutationService::execute(&client, binding, cursor, &restore)
                .await
                .unwrap();
        assert!(
            matches!(restored.state, OperationState::Completed { .. }),
            "{restored:?}"
        );
        assert_eq!(
            fs::read_to_string(root.join("written.txt")).unwrap(),
            "after\n"
        );
        let unrevert = client
            .prepare_unrevert(binding, cursor, &preview.restore_id)
            .await
            .unwrap();
        let reverted =
            WorkspaceSnapshotMutationService::execute(&client, binding, cursor, &unrevert)
                .await
                .unwrap();
        assert!(
            matches!(reverted.state, OperationState::Completed { .. }),
            "{reverted:?}"
        );
        assert_eq!(
            fs::read_to_string(root.join("written.txt")).unwrap(),
            "changed\n"
        );
        client.reconnect(&CancellationToken::new()).await.unwrap();
        tool(
            &client,
            cursor,
            "file_read",
            json!({"filePath":"written.txt"}),
        )
        .await;
        eprintln!("PASS snapshot capture/inspect/restore/unrevert and reconnect");
        fs::create_dir(root.join("stale-dir")).unwrap();
        let stale = client
            .resolve_directory_cursor(binding, cursor, &WorkspacePath::new("stale-dir").unwrap())
            .await
            .unwrap();
        fs::remove_dir(root.join("stale-dir")).unwrap();
        let error = client
            .prepare_canonical_tool(
                binding,
                &stale.cursor,
                &ToolPrepareRequest {
                    name: "file_read".into(),
                    input: json!({"filePath":"missing"}),
                },
            )
            .await
            .err()
            .expect("stale cwd rejected");
        assert!(
            matches!(
                error,
                WorkspaceError::StaleCursor | WorkspaceError::StaleResource { .. }
            ),
            "{error:?}"
        );
        eprintln!("PASS structured stale cwd error over HTTP 400");
        let prepared = client
            .prepare_canonical_tool(
                binding,
                cursor,
                &ToolPrepareRequest {
                    name: "shell".into(),
                    input: json!({
                        "command": format!("printf '{EXECUTION_MARKER}' >> execution-count.txt; printf '{RECOVERED_CONTENT}' > recovered.txt"),
                        "timeout": 1000,
                    }),
                },
            )
            .await
            .unwrap();
        let fault = PathBuf::from(env::var_os("WORKCELL_TEST_FAULT").unwrap());
        fs::write(
            fault.with_extension("preparation"),
            prepared.prepared.operation.preparation_id.as_str(),
        )
        .unwrap();
        fs::write(&fault, b"drop response after real execute").unwrap();
        let uncertain = client
            .execute_canonical_tool(binding, cursor, &prepared.prepared)
            .await
            .unwrap();
        assert!(matches!(
            uncertain.state,
            OperationState::Indeterminate { .. }
        ));
        assert_eq!(client.pending_remote_operations().len(), 1);
        let recovery_session = caudra_workspace::WorkspaceSession::new(
            client.workspace_handle().unwrap(),
            binding.clone(),
            cursor.clone(),
        )
        .unwrap();
        let pending = caudra_workspace::execute_workspace_control(&recovery_session, "pending")
            .await
            .unwrap();
        assert!(pending.contains("pending operations: 1"));
        assert!(
            caudra_workspace::execute_workspace_control(&recovery_session, "acknowledge anything")
                .await
                .is_err()
        );
        assert_eq!(client.pending_remote_operations().len(), 1);
        drop(recovery_session);
        assert_eq!(
            fs::read_to_string(root.join("recovered.txt")).unwrap(),
            RECOVERED_CONTENT
        );
        drop(client);
        fs::remove_file(&fault).unwrap();
        let recovered = RemoteWorkcellClient::connect(
            &selection,
            Some(credential.clone()),
            SessionBindingId::new("integration-session").unwrap(),
            RemoteOperationJournal::open(&state).unwrap(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(recovered.pending_remote_operations().is_empty());
        assert_eq!(
            fs::read_to_string(root.join("execution-count.txt")).unwrap(),
            EXECUTION_MARKER
        );
        assert_eq!(
            fs::read_to_string(fault.with_extension("count")).unwrap(),
            "1"
        );
        tool(
            &recovered,
            recovered.root_cursor(),
            "file_read",
            json!({"filePath":"recovered.txt"}),
        )
        .await;
        eprintln!(
            "PASS lost execute response, durable pending journal reopen, server-status recovery without replay"
        );
        let prepared = recovered.prepare_canonical_tool(recovered.session_binding(), recovered.root_cursor(), &ToolPrepareRequest {
            name: "shell".into(),
            input: json!({"command":"printf acknowledged > acknowledged.txt", "timeout":1000}),
        }).await.unwrap();
        fs::write(
            fault.with_extension("preparation"),
            prepared.prepared.operation.preparation_id.as_str(),
        )
        .unwrap();
        fs::write(&fault, b"drop acknowledgement candidate response").unwrap();
        let uncertain = recovered
            .execute_canonical_tool(
                recovered.session_binding(),
                recovered.root_cursor(),
                &prepared.prepared,
            )
            .await
            .unwrap();
        assert!(matches!(
            uncertain.state,
            OperationState::Indeterminate { .. }
        ));
        let session = caudra_workspace::WorkspaceSession::new(
            recovered.workspace_handle().unwrap(),
            recovered.session_binding().clone(),
            recovered.root_cursor().clone(),
        )
        .unwrap();
        let pending = recovered.pending_remote_operations();
        assert_eq!(pending.len(), 1);
        let args = format!("acknowledge {}", pending[0].operation_id.as_str());
        assert!(
            caudra_workspace::execute_workspace_control(&session, &args)
                .await
                .is_err()
        );
        assert_eq!(recovered.pending_remote_operations().len(), 1);
        let count = fs::read_to_string(fault.with_extension("count")).unwrap();
        caudra_workspace::execute_workspace_control(
            &session,
            &format!("{args} --accept-possible-effects"),
        )
        .await
        .unwrap();
        assert!(recovered.pending_remote_operations().is_empty());
        assert_eq!(
            fs::read_to_string(fault.with_extension("count")).unwrap(),
            count
        );
        assert_eq!(
            fs::read_to_string(root.join("acknowledged.txt")).unwrap(),
            "acknowledged"
        );
        eprintln!("PASS explicit controller acknowledgement without mutation replay");
        fs::remove_file(&fault).unwrap();
        drop(session);
        drop(recovered);
        for restart in [true, false] {
            let client = RemoteWorkcellClient::connect(
                &selection,
                Some(credential.clone()),
                SessionBindingId::new("integration-session").unwrap(),
                RemoteOperationJournal::open(&state).unwrap(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            let nested = client
                .resolve_directory_cursor(
                    client.session_binding(),
                    client.root_cursor(),
                    &WorkspacePath::new("nested").unwrap(),
                )
                .await
                .unwrap();
            let marker = if restart {
                "restart-effects.txt"
            } else {
                "evicted-effects.txt"
            };
            let prepared = client.prepare_canonical_tool(client.session_binding(), &nested.cursor, &ToolPrepareRequest {
                name: "shell".into(),
                input: json!({"command":format!("printf '{EXECUTION_MARKER}' >> {marker}"), "timeout":1000}),
            }).await.unwrap();
            fs::write(
                fault.with_extension("preparation"),
                prepared.prepared.operation.preparation_id.as_str(),
            )
            .unwrap();
            fs::write(&fault, b"lose response before server forgets execution").unwrap();
            let uncertain = client
                .execute_canonical_tool(
                    client.session_binding(),
                    &nested.cursor,
                    &prepared.prepared,
                )
                .await
                .unwrap();
            assert!(matches!(
                uncertain.state,
                OperationState::Indeterminate {
                    side_effects_possible: true
                }
            ));
            assert_eq!(
                fs::read_to_string(root.join("nested").join(marker)).unwrap(),
                EXECUTION_MARKER
            );
            let count = fs::read_to_string(fault.with_extension("count")).unwrap();
            fs::remove_file(&fault).unwrap();
            let original_host = client.host_binding();
            if restart {
                fs::write(
                    fault.with_extension("restart"),
                    b"same generation, new process",
                )
                .unwrap();
            } else {
                // Evict the real server's tombstone without acknowledging the local journal.
                let mut response = isahc::send_async(isahc::Request::post(&endpoint)
                    .header("Content-Type", "application/json")
                    .header("Mcp-Method", "ai.workcell/release")
                    .header("Accept", "application/json, text/event-stream")
                    .header("mcp-protocol-version", "2026-07-28")
                    .header("Authorization", format!("Bearer {}", fs::read_to_string(env::var_os("WORKCELL_TEST_TOKEN_FILE").unwrap()).unwrap()))
                    .body(json!({"jsonrpc":"2.0", "id":1, "method":"ai.workcell/release", "params":{
                        "version":"v1", "preparationId":prepared.prepared.operation.preparation_id.as_str(),
                        "invocationId":prepared.prepared.operation.invocation_id.as_ref().unwrap().as_str(),
                        "host":original_host,
                        "_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28", "io.modelcontextprotocol/clientCapabilities":{"extensions":{"ai.workcell/remote-host":{"versions":["v1"]}}}, "io.modelcontextprotocol/clientInfo":{"name":"integration", "version":"1"}, "ai.workcell/remote-host":{"versions":["v1"]}}
                    }}).to_string()).unwrap()).await.unwrap();
                assert!(response.status().is_success());
                let body = response.text().await.unwrap();
                let response: Value = serde_json::from_str(&body).unwrap_or_else(|_| {
                    body.lines()
                        .filter_map(|line| line.strip_prefix("data:"))
                        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
                        .find(|value| value["id"] == 1)
                        .expect("release response in SSE")
                });
                assert_eq!(response["result"]["released"], true, "{response}");
                for _ in 0..TOMBSTONE_CAPACITY {
                    let read = client
                        .prepare_canonical_tool(
                            client.session_binding(),
                            client.root_cursor(),
                            &ToolPrepareRequest {
                                name: "file_read".into(),
                                input: json!({"filePath":"fixture.txt"}),
                            },
                        )
                        .await
                        .unwrap();
                    client
                        .release_canonical_tool(
                            client.session_binding(),
                            client.root_cursor(),
                            &read.prepared,
                        )
                        .await
                        .unwrap();
                }
            }
            drop(client);
            let recovered = RemoteWorkcellClient::connect(
                &selection,
                Some(credential.clone()),
                SessionBindingId::new("integration-session").unwrap(),
                RemoteOperationJournal::open(&state).unwrap(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert_eq!(
                original_host.workspace_generation,
                recovered.host_binding().workspace_generation
            );
            assert_eq!(
                original_host.instance_id == recovered.host_binding().instance_id,
                !restart
            );
            let status = recovered
                .recover_persisted_operation_status(
                    recovered.session_binding(),
                    recovered.root_cursor(),
                    &uncertain.handle,
                )
                .await
                .unwrap();
            assert!(
                matches!(status.state, OperationState::NeverSeen),
                "{status:?}"
            );
            recovered
                .reconnect(&CancellationToken::new())
                .await
                .unwrap();
            let pending = recovered.pending_remote_operations();
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].state, RemoteOperationState::Indeterminate);
            let records = RemoteOperationJournal::open(&state)
                .unwrap()
                .list_pending(recovered.stored_binding())
                .unwrap();
            assert_eq!(records.len(), 1);
            assert!(records[0].side_effects_possible);
            assert!(records[0].acknowledged_at.is_none());
            let overlap = recovered
                .prepare_canonical_tool(
                    recovered.session_binding(),
                    recovered.root_cursor(),
                    &ToolPrepareRequest {
                        name: "file_write".into(),
                        input: json!({"filePath":"after-ack.txt", "content":"acknowledged"}),
                    },
                )
                .await
                .unwrap();
            assert!(matches!(
                recovered
                    .execute_canonical_tool(
                        recovered.session_binding(),
                        recovered.root_cursor(),
                        &overlap.prepared
                    )
                    .await,
                Err(WorkspaceError::PendingOperation { .. })
            ));
            let session = caudra_workspace::WorkspaceSession::new(
                recovered.workspace_handle().unwrap(),
                recovered.session_binding().clone(),
                recovered.root_cursor().clone(),
            )
            .unwrap();
            let args = format!("acknowledge {}", pending[0].operation_id.as_str());
            assert!(
                caudra_workspace::execute_workspace_control(&session, &args)
                    .await
                    .is_err()
            );
            assert_eq!(recovered.pending_remote_operations().len(), 1);
            caudra_workspace::execute_workspace_control(
                &session,
                &format!("{args} --accept-possible-effects"),
            )
            .await
            .unwrap();
            assert!(recovered.pending_remote_operations().is_empty());
            tool(
                &recovered,
                recovered.root_cursor(),
                "file_write",
                json!({"filePath":"after-ack.txt", "content":"acknowledged"}),
            )
            .await;
            assert_eq!(
                fs::read_to_string(fault.with_extension("count")).unwrap(),
                count
            );
            assert_eq!(
                fs::read_to_string(root.join("nested").join(marker)).unwrap(),
                EXECUTION_MARKER
            );
            eprintln!(
                "PASS lost response then {}: NeverSeen retains cross-cursor locks until explicit acknowledgement",
                if restart {
                    "same-generation restart"
                } else {
                    "same-instance tombstone eviction"
                }
            );
        }
    });
}
