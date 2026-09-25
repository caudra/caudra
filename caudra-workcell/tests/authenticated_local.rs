use std::{
    env, fs,
    fs::{File, FileTimes},
    path::PathBuf,
    str::FromStr,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use caudra_agent::agent::tool_dispatch::{self, Emit};
use caudra_agent::tools::{
    FileReadTracker, ToolContext, ToolEffect, ToolRegistry, interpreter_ctx, stale_read_message,
};
use caudra_agent::workspace_baseline::{BaselineGate, WorkspaceBaseline};
use caudra_agent::{
    AgentEvent, AgentMode, CancelToken, EventSender, ToolOutput,
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
use caudra_config::{Effect, PermissionRule, PermissionsConfig, SnapshotsConfig, ToolKey};
use caudra_storage::{
    StateDir,
    auth::{WorkcellCredential, WorkcellCredentialName, WorkcellCredentialRef},
    id::CaudraId,
    remote_operation_journal::{RemoteOperationJournal, RemoteOperationState},
};
use caudra_workbench::{
    BackendDriver, BackendError, BackendEvent, BackendRevision, Layout, MutationGate, SidebarView,
    Workbench, WorkbenchBackend, WorkbenchFilesystem, WorkbenchPath, WorkbenchStyles,
    WorkspaceFilesystem,
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
    CheckpointId, DirectoryNavigation, ListRequest, MutationResult, OperationState,
    OperationStatus, ReadBytesRequest, ReadTextRequest, ResourceRevision, ResourceSelector,
    ScmDiscoverRequest, ScmStatusRequest, SearchRequest, SessionBindingId, SnapshotCaptureLimits,
    SnapshotCaptureRequest, SnapshotInspectRequest, SnapshotOperationPreview, ToolPrepareRequest,
    WatchOpenRequest, WatchPollRequest, WatchPollState, WorkspaceAssetService, WorkspaceCursor,
    WorkspacePath, WorkspaceReadService, WorkspaceScmReadService, WorkspaceSearchService,
    WorkspaceSnapshotMutationService, WorkspaceSnapshotReadService, WorkspaceWatchService,
};
use caudra_workspace::{ResourceKind, WorkspaceSession};
use caudra_workspace::{
    ScmDiffRequest, ScmDiffTarget, ScmLogRequest, ScmMutation, ScmReadSideRequest, ScmSide,
    WorkspaceScmMutationService,
};
use futures_lite::io::{AsyncReadExt, repeat};
use image::{DynamicImage, ImageFormat};
use isahc::AsyncReadResponseExt;
use ratatui::{Terminal, backend::TestBackend};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};
use std::{
    fmt::{Debug, Write as _},
    sync::Arc,
};
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
const BATCH_CANCELLED: &str = "batch-cancelled.txt";
const LAPSED_FILE: &str = "lapsed-preparation.txt";
const LAPSED_TTL_MS: u64 = 1_500;
/// The lapsed preparation and the renewal that replaced it.
const LAPSED_PREPARATIONS: usize = 2;
const LAPSED_EXECUTIONS: usize = 1;
const REMOTE_PREPARATION_CAPACITY: usize = 64;
/// The shell held before dispatch and the write held at its prompt.
const HELD_PREPARATIONS: usize = 2;
const PUBLICATION_OVERWRITE: &str = "written while the publication awaited reconciliation";
const STALE_EDIT_FILE: &str = "stale-edit.txt";
const STALE_EDIT_ORIGINAL: &str = "prepared against this\n";
const STALE_EDIT_OURS: &str = "the agent's change\n";
const STALE_EDIT_THEIRS: &str = "changed before publication\n";
const SHARED_EDIT_FILE: &str = "shared-edit.txt";
const SHARED_EDIT_ORIGINAL: &str = "first line\nsecond line\n";
const SHARED_EDITS: [(&str, &str); 2] =
    [("first line", "FIRST LINE"), ("second line", "SECOND LINE")];
const SHARED_EDIT_APPLIED: &str = "FIRST LINE\nSECOND LINE\n";
const RUNNING_SHELL_STARTED: &str = "running-shell.started";
const RUNNING_SHELL_MARKER: &str = "running-shell.marker";
const RUNNING_SHELL_PEER: &str = "running-shell.peer";
const RUNNING_SHELL_OUTPUT: &str = "the marker arrived while this shell ran";
/// The main agent's `{cwd}` is a local path and a workflow agent's is the
/// workspace path, so one remote file must not be keyed by either.
const TASK_CWD_VAR: &str = "{cwd}";
const WORKFLOW_TASK_CWD: &str = ".";
const BESIDE_UNCERTAIN_SHELL: &str = "beside-uncertain-shell.txt";
const BESIDE_UNCERTAIN_WRITE: &str = "beside-uncertain-write.txt";
const BESIDE_UNCERTAIN_CONTENT: &str = "ran beside an uncertain operation";
const EDITOR_FILE: &str = "editor.txt";
const EDITOR_IMAGE: &str = "editor.png";
const EDITOR_IMAGE_EDGE: u32 = 3;
const EDITOR_ORIGINAL: &str = "original editor line\n";
const EDITOR_SAVED: &str = "saved editor line\n";
const EDITOR_EXTERNAL: &str = "other editor line\n";
const EDITOR_NAMESPACE: &str =
    "a byte read must report the revision a conditional write is checked against";
const EDITOR_SAVE_REFUSED: &str = "an edited remote buffer must save against the revision it read";
const EDITOR_STALE_ACCEPTED: &str = "a concurrent remote modification must refuse the save";
/// A full operation ledger names no limit. Only snapshot refusals do.
const LEDGER_FULL: WorkspaceError = WorkspaceError::QuotaExceeded {
    limit: None,
    maximum: None,
};
const CAPTURE_LIMITS: SnapshotCaptureLimits = SnapshotCaptureLimits {
    max_files: 10_000,
    max_file_bytes: 16 * 1024 * 1024,
    max_total_bytes: 256 * 1024 * 1024,
};
const CAPTURE_WORKLOAD_FILES: u32 = 20_000;
const CAPTURE_WORKLOAD_DIRECTORIES: u32 = 100;
const CAPTURE_WORKLOAD_FILE_BYTES: usize = 128;
const CAPTURE_WORKLOAD_CHECKPOINTS: [&str; 2] = ["workload-first", "workload-unchanged"];
const METADATA_SMALL: &str = "small.txt";
const METADATA_NESTED: &str = "nested/other.txt";
const METADATA_BINARY: &str = "huge.bin";
const METADATA_SOURCEMAP: &str = "nested/bundle.js.map";
const METADATA_CONTENT: &str = "small readable text\n";
const METADATA_REPLACEMENT: &str = "other readable text\n";
const METADATA_MOVED: &str = "moved.txt";
const FILE_TOO_LARGE: &str = "file_too_large";
const REPOSITORY_UNAVAILABLE: &str = "repository_unavailable";
const WATCH_FAULT_EXTENSION: &str = "watch-unavailable";
const WATCH_MAX_BYTES: u32 = 65_536;
const WATCH_WAIT_MS: u64 = 1_000;
const WORKBENCH_FILE: &str = "sub/a.rs";
const WORKBENCH_SHADOW: &str = "sub/sub/a.rs";
const WORKBENCH_CONTENT: &str = "const VALUE: &str = \"bytes-A\";\n";
const WORKBENCH_OTHER: &str = "const VALUE: &str = \"bytes-B\";\n";
const WORKBENCH_CHANGED: &str = "const VALUE: &str = \"bytes-C\";\n";
const WORKBENCH_SAVED: &str = "const VALUE: &str = \"saved-A\";\n";
const WORKBENCH_MOVED: &str = "sub/moved.rs";
const WORKBENCH_HIDDEN: &str = ".arbitrary-hidden";
const WORKBENCH_NO_GIT: &str = "Not a Git repository";
const WORKBENCH_FRAME_WIDTH: u16 = 120;
const WORKBENCH_FRAME_HEIGHT: u16 = 30;
const WORKBENCH_WORKLOAD_DIRECTORIES: usize = 5_000;
const WORKBENCH_FILES_PER_DIRECTORY: usize = 4;
const WORKBENCH_WORKLOAD_TIMEOUT: Duration = Duration::from_secs(420);

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
        // Larger than the workspace hashes, so only reviewed transfer can
        // answer for it; the content digest still has to be what a byte read
        // reports, and the transfer revision still has to stay out of it.
        let content_revision = ResourceRevision::new(file.content.digest.as_str()).unwrap();
        assert_ne!(content_revision, file.revision);
        let read = WorkspaceReadService::read_bytes(
            &client,
            binding,
            cursor,
            &ReadBytesRequest {
                resource: ResourceSelector::Path(prepared.request.path.clone()),
                byte_offset: range.start,
                max_bytes: 1024,
                if_revision: Some(content_revision.clone()),
            },
        )
        .await
        .unwrap();
        assert_eq!(read.bytes, vec![BINARY_BYTE; 1024]);
        assert_eq!(read.revision, content_revision);
        assert!(matches!(
            WorkspaceReadService::read_bytes(
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
            .await,
            Err(WorkspaceError::StaleResource { .. })
        ));
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
            let pending = client.pending_remote_operations();
            assert_eq!(pending.len(), 1);
            assert_eq!(
                client.execute_publication(&prepared).await.err(),
                Some(WorkspaceError::PendingOperation {
                    operation_id: pending[0].operation_id.as_str().to_owned(),
                })
            );
            let count = fs::read_to_string(fault.with_extension("count")).unwrap();
            fs::remove_file(&fault).unwrap();
            let published = root.join("nested").join(name);
            assert_binary(
                LocalTransferSource::new(smol::fs::File::open(&published).await.unwrap()),
                1024,
            )
            .await;
            tool(
                &client,
                client.root_cursor(),
                "file_write",
                json!({"filePath":format!("nested/{name}"), "content":PUBLICATION_OVERWRITE}),
            )
            .await;
            assert_eq!(
                fs::read_to_string(&published).unwrap(),
                PUBLICATION_OVERWRITE
            );
            assert_eq!(client.pending_remote_operations(), pending);
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
            assert!(status.file.is_some());
            assert_eq!(
                fs::read_to_string(fault.with_extension("count")).unwrap(),
                count
            );
            assert_eq!(
                fs::read_to_string(&published).unwrap(),
                PUBLICATION_OVERWRITE
            );
        }
        eprintln!(
            "PASS lost execute, durable restart recovery, replay refused by identity, same-file write never held back, Unknown keeps the record, no replay"
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
    let baseline = with_remote_baseline(&mut ctx, client, baseline_state.path());
    let first = json!({"command":format!("printf '{BATCH_CONTENT}' > {BATCH_FIRST}; printf '{BATCH_CONTENT}'")});
    let second = json!({"command":format!("printf '{BATCH_CONTENT}' > {BATCH_SECOND}; printf '{BATCH_CONTENT}'")});
    let write = json!({"filePath":BATCH_WRITE,"content":BATCH_CONTENT});
    let [first_edit, second_edit] = SHARED_EDITS
        .map(|(old, new)| json!({"filePath":SHARED_EDIT_FILE, "oldString":old, "newString":new}));
    let waiting = json!({
        "command":format!(
            ": > {RUNNING_SHELL_STARTED}; while [ ! -e {RUNNING_SHELL_MARKER} ] || [ ! -e {RUNNING_SHELL_PEER} ]; do sleep {}; done; cat {RUNNING_SHELL_MARKER}",
            BATCH_POLL.as_secs_f64()
        ),
        "timeoutSec":BATCH_TIMEOUT.as_secs(),
    });
    let marker = json!({"filePath":RUNNING_SHELL_MARKER, "content":RUNNING_SHELL_OUTPUT});
    let peer = json!({"command":format!(": > {RUNNING_SHELL_PEER}; printf '{BATCH_CONTENT}'")});
    let run = |id: &'static str, name, input| {
        tool_dispatch::run(&registry, None, id.into(), name, input, &ctx, Emit::Notify)
    };
    let exercise = async {
        assert!(client.pending_remote_operations().is_empty());
        fs::write(&trace, "").unwrap();
        fs::write(&gate, "hold first execute before forwarding").unwrap();
        let alongside = async {
            while !gate.with_extension("entered").exists() {
                smol::Timer::after(BATCH_POLL).await;
            }
            let held = client.pending_remote_operations();
            assert_eq!(held.len(), 1);
            let (shell, file) = futures_lite::future::zip(
                run("batch-second", "shell", &second),
                run("batch-write", "file_write", &write),
            )
            .await;
            assert!(!shell.is_error, "{}", shell.output.as_text());
            assert!(shell.output.as_text().contains(BATCH_CONTENT));
            assert!(!file.is_error, "{}", file.output.as_text());
            for path in [BATCH_SECOND, BATCH_WRITE] {
                assert_eq!(fs::read_to_string(root.join(path)).unwrap(), BATCH_CONTENT);
            }
            assert!(!root.join(BATCH_FIRST).exists());
            assert_eq!(client.pending_remote_operations(), held);
            eprintln!(
                "PASS a shell and a write complete while another shell is held in flight, whose record is kept"
            );
            let diagnostics = fault.with_file_name("rpc-diagnostics");
            fs::write(&diagnostics, "").unwrap();
            let (cancel, token) = CancelToken::new();
            let (_response_tx, response_rx) = flume::unbounded();
            let mut prompting_ctx = ctx.clone();
            prompting_ctx.cancel = token;
            prompting_ctx.permissions = Arc::new(PermissionManager::new_nonpersistent(
                PermissionsConfig::default(),
                root.to_path_buf(),
                Arc::default(),
            ));
            prompting_ctx.user_response_rx = Some(Arc::new(smol::lock::Mutex::new(response_rx)));
            let cancelled_input = json!({"filePath":BATCH_CANCELLED, "content":BATCH_CONTENT});
            let cancelled = tool_dispatch::run(
                &registry,
                None,
                "batch-cancelled".into(),
                "file_write",
                &cancelled_input,
                &prompting_ctx,
                Emit::Notify,
            );
            let cancel_when_full = async {
                loop {
                    if let AgentEvent::PermissionRequest(_) = rx.recv_async().await.unwrap().event {
                        break;
                    }
                }
                let records = fs::read_to_string(&diagnostics).unwrap();
                let prepared: Value = records
                    .lines()
                    .map(|line| serde_json::from_str::<Value>(line).unwrap())
                    .find(|record| {
                        record["method"] == "ai.workcell/prepare" && record["tool"] == "file_write"
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
                assert_eq!(full.err(), Some(LEDGER_FULL));
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
                cancelled.output.as_text().contains(PERMISSION_DENIED),
                "{}",
                cancelled.output.as_text()
            );
            assert!(!root.join(BATCH_CANCELLED).exists());
            assert_eq!(client.pending_remote_operations(), held);
            eprintln!(
                "PASS cancelling a pending prompt releases its unsent preparation, full 64-slot ledger admits replacement, no cancelled file or new journal row"
            );
            fs::write(gate.with_extension("release"), "release").unwrap();
        };
        futures_lite::future::zip(
            async {
                let first = run("batch-first", "shell", &first).await;
                assert!(!first.is_error, "{}", first.output.as_text());
                assert!(first.output.as_text().contains(BATCH_CONTENT));
            },
            alongside,
        )
        .await;
        assert_eq!(
            fs::read_to_string(root.join(BATCH_FIRST)).unwrap(),
            BATCH_CONTENT
        );
        assert!(client.pending_remote_operations().is_empty());
        assert!(baseline.is_captured());
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
        "PASS concurrent two-shell/file_write batch: nothing queues behind an in-flight shell, all artifacts and shell output, no unresolved rows, unsynchronized fresh automatic snapshot"
    );
    let beside_running = async {
        fs::write(root.join(SHARED_EDIT_FILE), SHARED_EDIT_ORIGINAL).unwrap();
        let mut workflow_ctx = ctx.clone();
        workflow_ctx.task_environment = workflow_ctx
            .task_environment
            .set(TASK_CWD_VAR, WORKFLOW_TASK_CWD);
        let (first_done, second_done) = futures_lite::future::zip(
            run("edit-first-line", "file_edit", &first_edit),
            tool_dispatch::run(
                &registry,
                None,
                "edit-second-line".into(),
                "file_edit",
                &second_edit,
                &workflow_ctx,
                Emit::Notify,
            ),
        )
        .await;
        for done in [first_done, second_done] {
            assert!(!done.is_error, "{}", done.output.as_text());
        }
        assert_eq!(
            fs::read_to_string(root.join(SHARED_EDIT_FILE)).unwrap(),
            SHARED_EDIT_APPLIED
        );
        eprintln!(
            "PASS two edits of one file started together by agents with different {{cwd}} both apply"
        );
        let beside = async {
            while !root.join(RUNNING_SHELL_STARTED).exists() {
                smol::Timer::after(BATCH_POLL).await;
            }
            let (shell, file) = futures_lite::future::zip(
                run("shell-beside-running", "shell", &peer),
                run("marker-beside-running", "file_write", &marker),
            )
            .await;
            assert!(!shell.is_error, "{}", shell.output.as_text());
            assert!(shell.output.as_text().contains(BATCH_CONTENT));
            assert!(!file.is_error, "{}", file.output.as_text());
        };
        let (running, ()) =
            futures_lite::future::zip(run("running-shell", "shell", &waiting), beside).await;
        assert!(!running.is_error, "{}", running.output.as_text());
        assert!(running.output.as_text().contains(RUNNING_SHELL_OUTPUT));
        assert!(client.pending_remote_operations().is_empty());
    };
    futures_lite::future::race(beside_running, async {
        smol::Timer::after(BATCH_TIMEOUT).await;
        panic!(
            "calls beside a running shell did not finish: started={}, marker={}, peer={}",
            root.join(RUNNING_SHELL_STARTED).exists(),
            root.join(RUNNING_SHELL_MARKER).exists(),
            root.join(RUNNING_SHELL_PEER).exists()
        );
    })
    .await;
    eprintln!(
        "PASS a running shell holds back neither a second shell nor the write it waits for, and the journal ends empty"
    );
}

/// A command held at its permission prompt outlives the preparation the
/// server made for it on an independent timer. The wait is legitimate work,
/// so the approved call has to be renewed and run, exactly once.
async fn lapsed_preparation_regression(client: &RemoteWorkcellClient, root: &Path) {
    let registry = Arc::new(ToolRegistry::new());
    RemoteWorkcellHost::new(client.clone())
        .register(&registry)
        .unwrap();
    let permissions = Arc::new(PermissionManager::new_nonpersistent(
        PermissionsConfig::default(),
        root.to_path_buf(),
        Arc::default(),
    ));
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
    let baseline_state = tempfile::tempdir().unwrap();
    with_remote_baseline(&mut ctx, client, baseline_state.path());
    let fault = PathBuf::from(env::var_os("WORKCELL_TEST_FAULT").unwrap());
    let shorten = fault.with_file_name("short-preparation-ttl");
    let expiry = fault.with_file_name("short-preparation-expiry");
    let diagnostics = fault.with_file_name("rpc-diagnostics");
    let trace = fault.with_file_name("batch-rpc-trace");
    fs::write(&diagnostics, "").unwrap();
    fs::write(&trace, "").unwrap();
    let _ = fs::remove_file(&expiry);
    fs::write(&shorten, LAPSED_TTL_MS.to_string()).unwrap();
    let reviewed = json!({"command":format!("printf '{BATCH_CONTENT}' > {LAPSED_FILE}; printf '{BATCH_CONTENT}'")});
    let exercise = async {
        let dispatch = tool_dispatch::run(
            &registry,
            None,
            "lapsed-reviewed".into(),
            "shell",
            &reviewed,
            &ctx,
            Emit::Silent,
        );
        let approve_after_lapse = async {
            let request = loop {
                if let AgentEvent::PermissionRequest(request) = rx.recv_async().await.unwrap().event
                {
                    break request;
                }
            };
            let lapses_at = fs::read_to_string(&expiry)
                .unwrap()
                .trim()
                .parse::<u128>()
                .unwrap();
            while unix_millis() <= lapses_at {
                smol::Timer::after(BATCH_POLL).await;
            }
            assert!(permissions.answer(&request.id, PermissionAnswer::AllowOnce));
        };
        let (done, ()) = futures_lite::future::zip(dispatch, approve_after_lapse).await;
        assert!(!done.is_error, "{}", done.output.as_text());
        assert!(done.output.as_text().contains(BATCH_CONTENT));
    };
    futures_lite::future::race(exercise, async {
        smol::Timer::after(BATCH_TIMEOUT).await;
        panic!(
            "a command approved after its preparation lapsed did not finish: trace={:?}",
            fs::read_to_string(&trace)
        );
    })
    .await;
    assert_eq!(
        fs::read_to_string(root.join(LAPSED_FILE)).unwrap(),
        BATCH_CONTENT
    );
    let records = fs::read_to_string(&diagnostics)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect::<Vec<_>>();
    let prepared = records
        .iter()
        .filter(|record| record["method"] == "ai.workcell/prepare" && record["tool"] == "shell")
        .map(|record| record["preparationId"].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        prepared.len(),
        LAPSED_PREPARATIONS,
        "the approved command did not renew: {records:?}"
    );
    let executed = fs::read_to_string(&trace)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|record| {
            record["method"] == "ai.workcell/execute"
                && prepared.contains(&record["params"]["preparationId"])
        })
        .map(|record| record["params"]["preparationId"].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        executed.len(),
        LAPSED_EXECUTIONS,
        "a lapsed preparation was dispatched as well"
    );
    assert_eq!(executed.last(), prepared.last());
    assert!(client.pending_remote_operations().is_empty());
    eprintln!(
        "PASS a command approved after its preparation lapsed renews it, runs exactly once, and leaves no unresolved row"
    );
}

/// Gives `ctx` the revert point a mutating remote call captures first, kept in
/// `state`.
fn with_remote_baseline(
    ctx: &mut ToolContext,
    client: &RemoteWorkcellClient,
    state: &Path,
) -> Arc<WorkspaceBaseline> {
    let baseline = WorkspaceBaseline::new_workspace_session(
        StateDir::from_path(state.into()),
        CaudraId::generate(),
        ctx.workspace_session.clone().unwrap(),
        client.stored_binding().clone(),
        SnapshotsConfig::default(),
    );
    ctx.baseline = Some(BaselineGate::new(baseline.clone(), None));
    baseline
}

fn unix_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
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
    let stale_target = root.join(STALE_EDIT_FILE);
    fs::write(&stale_target, STALE_EDIT_ORIGINAL).unwrap();
    let invocation = registry
        .get("file_edit")
        .unwrap()
        .tool
        .parse(&json!({
            "filePath": STALE_EDIT_FILE,
            "oldString": STALE_EDIT_ORIGINAL,
            "newString": STALE_EDIT_OURS,
        }))
        .unwrap();
    invocation.preflight(&ctx).await.unwrap();
    fs::write(&stale_target, STALE_EDIT_THEIRS).unwrap();
    let done = invocation.execute(&ctx).await;
    assert_eq!(done.output.err(), Some(stale_read_message(STALE_EDIT_FILE)));
    assert_eq!(
        fs::read_to_string(&stale_target).unwrap(),
        STALE_EDIT_THEIRS
    );
    assert!(client.pending_remote_operations().is_empty());
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
        "PASS actual registry Python, read/write/unknown plan shell, large progress, stale edit asks for a re-read and leaves no record, definitive network denial"
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
    let pending = client.pending_remote_operations();
    assert_eq!(pending.len(), 1);
    let baseline_state = tempfile::tempdir().unwrap();
    with_remote_baseline(&mut ctx, client, baseline_state.path());
    for (id, name, input) in [
        ("index-beside-uncertain", "file_index", json!({"path":"."})),
        (
            "python-beside-uncertain",
            "python_execution",
            json!({"code":"2 + 2"}),
        ),
        (
            "shell-beside-uncertain",
            "shell",
            json!({"command":format!("printf '{BESIDE_UNCERTAIN_CONTENT}' > {BESIDE_UNCERTAIN_SHELL}")}),
        ),
        (
            "write-beside-uncertain",
            "file_write",
            json!({"filePath":BESIDE_UNCERTAIN_WRITE, "content":BESIDE_UNCERTAIN_CONTENT}),
        ),
    ] {
        let done =
            tool_dispatch::run(&registry, None, id.into(), name, &input, &ctx, Emit::Silent).await;
        assert!(!done.is_error, "{name}: {}", done.output.as_text());
    }
    for path in [BESIDE_UNCERTAIN_SHELL, BESIDE_UNCERTAIN_WRITE] {
        assert_eq!(
            fs::read_to_string(root.join(path)).unwrap(),
            BESIDE_UNCERTAIN_CONTENT
        );
    }
    assert_eq!(client.pending_remote_operations(), pending);
    client
        .acknowledge_pending_operation(&pending[0].operation_id)
        .unwrap();
    assert!(client.pending_remote_operations().is_empty());
    eprintln!(
        "PASS dropped future sends cancel/status, keeps the uncertain record, and index, Python, shell and write all run beside it"
    );
}

async fn conditional_write(
    client: &RemoteWorkcellClient,
    path: &WorkspacePath,
    contents: &str,
    revision: ResourceRevision,
) -> Result<OperationStatus<MutationResult>, WorkspaceError> {
    WorkspaceMutationService::execute(
        client,
        client.session_binding(),
        client.root_cursor(),
        &MutationRequest {
            mutations: vec![Mutation::Write {
                path: path.clone(),
                content: WriteContent::Text(contents.to_owned()),
                condition: MutationCondition::Matches(revision),
            }],
        },
    )
    .await
}

/// A remote buffer is opened with a byte read and saved with a conditional
/// write, so the revision the read reports and the revision the write is
/// checked against have to be the same kind of value. Reviewed transfer stat
/// answers in its own revision namespace, which no workspace precondition can
/// ever match, so an editor that trusted it could never save.
async fn remote_editor_revision_namespace(client: &RemoteWorkcellClient, root: &Path) {
    let binding = client.session_binding();
    let cursor = client.root_cursor();
    fs::write(root.join(EDITOR_FILE), EDITOR_ORIGINAL).unwrap();
    let path = WorkspacePath::new(EDITOR_FILE).unwrap();
    let opened = WorkspaceReadService::read_bytes(
        client,
        binding,
        cursor,
        &ReadBytesRequest {
            resource: ResourceSelector::Path(path.clone()),
            byte_offset: 0,
            max_bytes: LIMIT as u64,
            if_revision: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(opened.bytes, EDITOR_ORIGINAL.as_bytes());
    let stat = WorkspaceReadService::stat(
        client,
        binding,
        cursor,
        &ResourceSelector::Path(path.clone()),
    )
    .await
    .unwrap();
    assert_eq!(stat.scope.resource_id(), &opened.resource_id);
    assert_eq!(
        stat.revision.as_ref(),
        Some(&opened.revision),
        "{EDITOR_NAMESPACE}"
    );
    let reread = WorkspaceReadService::read_bytes(
        client,
        binding,
        cursor,
        &ReadBytesRequest {
            resource: ResourceSelector::Id(opened.resource_id.clone()),
            byte_offset: 0,
            max_bytes: LIMIT as u64,
            if_revision: Some(opened.revision.clone()),
        },
    )
    .await
    .unwrap();
    assert_eq!(reread.revision, opened.revision, "{EDITOR_NAMESPACE}");
    let saved = conditional_write(client, &path, EDITOR_SAVED, opened.revision.clone()).await;
    assert!(
        matches!(
            saved,
            Ok(OperationStatus {
                state: OperationState::Completed { .. },
                ..
            })
        ),
        "{EDITOR_SAVE_REFUSED}: {saved:?}"
    );
    assert_eq!(
        fs::read_to_string(root.join(EDITOR_FILE)).unwrap(),
        EDITOR_SAVED
    );
    let reopened = WorkspaceReadService::read_bytes(
        client,
        binding,
        cursor,
        &ReadBytesRequest {
            resource: ResourceSelector::Path(path.clone()),
            byte_offset: 0,
            max_bytes: LIMIT as u64,
            if_revision: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(reopened.bytes, EDITOR_SAVED.as_bytes());
    overwrite_preserving_metadata(&root.join(EDITOR_FILE), EDITOR_EXTERNAL);
    let refused =
        conditional_write(client, &path, EDITOR_ORIGINAL, reopened.revision.clone()).await;
    assert!(
        matches!(
            refused,
            Err(WorkspaceError::StaleCursor
                | WorkspaceError::StaleResource { .. }
                | WorkspaceError::Conflict)
        ),
        "{EDITOR_STALE_ACCEPTED}: {refused:?}"
    );
    assert_eq!(
        fs::read_to_string(root.join(EDITOR_FILE)).unwrap(),
        EDITOR_EXTERNAL
    );
    assert!(matches!(
        WorkspaceReadService::read_bytes(
            client,
            binding,
            cursor,
            &ReadBytesRequest {
                resource: ResourceSelector::Id(reopened.resource_id.clone()),
                byte_offset: 0,
                max_bytes: LIMIT as u64,
                if_revision: Some(reopened.revision),
            },
        )
        .await,
        Err(WorkspaceError::StaleResource { .. })
    ));
    eprintln!(
        "PASS remote byte reads report workspace revisions, conditional saves commit, same-size/mtime content overwrite refuses stale save"
    );
    remote_image_bytes(client, root).await;
}

/// `view_image` resolves and stats a remote image, then reads its bytes under
/// that revision, so it only loads when reads answer in the namespace stat
/// speaks.
async fn remote_image_bytes(client: &RemoteWorkcellClient, root: &Path) {
    DynamicImage::new_rgb8(EDITOR_IMAGE_EDGE, EDITOR_IMAGE_EDGE)
        .save_with_format(root.join(EDITOR_IMAGE), ImageFormat::Png)
        .unwrap();
    let registry = Arc::new(ToolRegistry::new());
    caudra_agent::tools::native::register(&registry).unwrap();
    let permissions = Arc::new(PermissionManager::new_nonpersistent(
        PermissionsConfig {
            yolo: true,
            ..Default::default()
        },
        root.to_path_buf(),
        Arc::default(),
    ));
    let (tx, _events) = flume::unbounded();
    let mut ctx = interpreter_ctx(
        &AgentMode::Build,
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
    let done = tool_dispatch::run(
        &registry,
        None,
        "remote-image".into(),
        "view_image",
        &json!({ "path": EDITOR_IMAGE }),
        &ctx,
        Emit::Silent,
    )
    .await;
    assert!(!done.is_error, "{}", done.output.as_text());
    match &done.output {
        ToolOutput::Image { text, source } => {
            assert!(
                text.contains(&format!("{EDITOR_IMAGE_EDGE}x{EDITOR_IMAGE_EDGE}")),
                "{text}"
            );
            assert!(!source.data.is_empty());
        }
        other => panic!("remote view_image must return pixels: {other:?}"),
    }
    eprintln!("PASS remote view_image resolves, stats, and reads image bytes");
}

fn overwrite_preserving_metadata(path: &Path, content: &str) {
    let before = fs::metadata(path).unwrap();
    assert_eq!(before.len(), content.len() as u64);
    fs::write(path, content).unwrap();
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(FileTimes::new().set_modified(before.modified().unwrap()))
        .unwrap();
    let after = fs::metadata(path).unwrap();
    assert_eq!(after.len(), before.len());
    assert_eq!(after.modified().unwrap(), before.modified().unwrap());
}

fn workbench_path(path: &str) -> WorkbenchPath {
    WorkbenchPath::Remote(WorkspacePath::new(path).unwrap())
}

async fn workbench_session(client: &RemoteWorkcellClient, path: &str) -> WorkspaceSession {
    let directory = client
        .resolve_directory_cursor(
            client.session_binding(),
            client.root_cursor(),
            &WorkspacePath::new(path).unwrap(),
        )
        .await
        .unwrap();
    WorkspaceSession::new(
        client.workspace_handle().unwrap(),
        client.session_binding().clone(),
        directory.cursor,
    )
    .unwrap()
}

fn assert_workbench_conflict<T: Debug>(result: Result<T, BackendError>) {
    assert!(
        matches!(
            result,
            Err(BackendError::Conflict | BackendError::Workspace(WorkspaceError::StaleCursor))
        ),
        "{result:?}"
    );
}

async fn settle_workbench(workbench: &mut Workbench) -> Vec<String> {
    let deadline = Instant::now() + BATCH_TIMEOUT;
    let mut warnings = Vec::new();
    loop {
        let (_, warning) = workbench.tick();
        warnings.extend(warning);
        if !workbench.is_busy() && !workbench.scm_refreshing() {
            break;
        }
        assert!(Instant::now() < deadline, "Workbench did not settle");
        smol::Timer::after(BATCH_POLL).await;
    }
    warnings
}

fn workbench_frame(workbench: &mut Workbench) -> String {
    let mut terminal = Terminal::new(TestBackend::new(
        WORKBENCH_FRAME_WIDTH,
        WORKBENCH_FRAME_HEIGHT,
    ))
    .unwrap();
    terminal
        .draw(|frame| workbench.view(frame, frame.area()))
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

fn workbench_fixture(root: &Path) {
    fs::create_dir_all(root.join("sub/sub")).unwrap();
    fs::create_dir_all(root.join("sub").join(WORKBENCH_HIDDEN)).unwrap();
    fs::write(root.join(WORKBENCH_FILE), WORKBENCH_CONTENT).unwrap();
    fs::write(root.join(WORKBENCH_SHADOW), WORKBENCH_OTHER).unwrap();
}

async fn authenticated_workbench_nonroot(client: &RemoteWorkcellClient, root: &Path) {
    workbench_fixture(root);
    let session = workbench_session(client, "sub").await;
    let filesystem = WorkspaceFilesystem::new(session.clone()).unwrap();
    let page = filesystem
        .list(&workbench_path("sub"), true, None)
        .await
        .unwrap();
    assert!(!page.incomplete);
    assert!(page.continuation.is_none());
    let entry = page
        .entries
        .iter()
        .find(|entry| entry.path == workbench_path(WORKBENCH_FILE))
        .unwrap();
    assert_eq!(entry.revision, None);
    assert!(entry.resource_id.is_some());
    let loaded = filesystem.read(entry).await.unwrap();
    assert_eq!(loaded.lines, [WORKBENCH_CONTENT.trim_end()]);
    assert_eq!(loaded.entry.path, entry.path);
    assert!(matches!(
        loaded.entry.revision,
        Some(BackendRevision::Remote(_))
    ));
    let direct = filesystem.read_path(&entry.path).await.unwrap();
    assert_eq!(direct.lines, loaded.lines);
    assert_eq!(direct.entry.resource_id, entry.resource_id);
    eprintln!(
        "PASS real Workbench non-root metadata selection and direct path read bytes-A, not sub/sub bytes-B"
    );
    assert!(matches!(
        filesystem.write(entry, WORKBENCH_SAVED.to_owned()).await,
        Err(BackendError::MissingRevision)
    ));
    overwrite_preserving_metadata(&root.join(WORKBENCH_FILE), WORKBENCH_CHANGED);
    assert_workbench_conflict(
        filesystem
            .write(&loaded.entry, WORKBENCH_CONTENT.to_owned())
            .await,
    );
    assert_workbench_conflict(
        filesystem
            .rename(&loaded.entry, &workbench_path(WORKBENCH_MOVED))
            .await,
    );
    assert_workbench_conflict(filesystem.delete(&loaded.entry).await);
    for rename in [true, false] {
        fs::write(root.join(WORKBENCH_FILE), WORKBENCH_CONTENT).unwrap();
        let file = root.join(WORKBENCH_FILE);
        let gate = MutationGate::new(move || {
            let file = file.clone();
            Box::pin(async move {
                overwrite_preserving_metadata(&file, WORKBENCH_CHANGED);
                Ok(())
            })
        });
        let raced = WorkspaceFilesystem::new_with_gate(session.clone(), gate).unwrap();
        let result = if rename {
            raced
                .rename(entry, &workbench_path(WORKBENCH_MOVED))
                .await
                .map(|_| ())
        } else {
            raced.delete(entry).await
        };
        assert_workbench_conflict(result);
        assert_eq!(
            fs::read_to_string(root.join(WORKBENCH_FILE)).unwrap(),
            WORKBENCH_CHANGED
        );
        assert!(!root.join(WORKBENCH_MOVED).exists());
    }
    assert_eq!(
        fs::read_to_string(root.join(WORKBENCH_SHADOW)).unwrap(),
        WORKBENCH_OTHER
    );
    eprintln!(
        "PASS real Workbench same-size/mtime stale save/rename/delete and unrevisioned stat-to-prepare races refused; shadow bytes-B untouched"
    );
    let fresh = filesystem.read(entry).await.unwrap();
    let saved = filesystem
        .write(&fresh.entry, WORKBENCH_SAVED.to_owned())
        .await;
    assert_eq!(
        fs::read_to_string(root.join(WORKBENCH_FILE)).unwrap(),
        WORKBENCH_SAVED
    );
    saved.unwrap();
    assert_workbench_conflict(
        filesystem
            .rename(entry, &workbench_path(WORKBENCH_MOVED))
            .await,
    );
    let refreshed = filesystem
        .list(&workbench_path("sub"), true, None)
        .await
        .unwrap();
    assert!(!refreshed.incomplete);
    assert!(refreshed.continuation.is_none());
    let entry = refreshed
        .entries
        .iter()
        .find(|entry| entry.path == workbench_path(WORKBENCH_FILE))
        .unwrap();
    assert_eq!(entry.revision, None);
    let mut renamed = filesystem
        .rename(entry, &workbench_path(WORKBENCH_MOVED))
        .await
        .unwrap();
    assert_eq!(renamed.path, workbench_path(WORKBENCH_MOVED));
    assert!(renamed.revision.is_some());
    assert_eq!(
        fs::read_to_string(root.join(WORKBENCH_MOVED)).unwrap(),
        WORKBENCH_SAVED
    );
    renamed.revision = None;
    filesystem.delete(&renamed).await.unwrap();
    assert!(!root.join(WORKBENCH_FILE).exists());
    assert!(!root.join(WORKBENCH_MOVED).exists());
    assert_eq!(
        fs::read_to_string(root.join(WORKBENCH_SHADOW)).unwrap(),
        WORKBENCH_OTHER
    );
    eprintln!(
        "PASS real Workbench non-root conditional save/rename/delete, same-size/mtime stale revisions and stat-to-prepare races, fresh unrevisioned mutations, shadow bytes-B untouched"
    );
}

async fn authenticated_workbench_view(session: WorkspaceSession) {
    let filesystem = WorkspaceFilesystem::new(session.clone()).unwrap();
    let watch = filesystem
        .watch_open()
        .await
        .unwrap()
        .expect("real Workbench watch");
    let polled = filesystem.watch_poll(watch.clone()).await;
    let watch = polled
        .as_ref()
        .map_or(watch, |result| result.handle.clone());
    filesystem.watch_close(watch).await.unwrap();
    let mut workbench = Workbench::new(WorkbenchStyles::default());
    workbench.restore(Layout {
        sidebar: SidebarView::SourceControl,
        show_hidden: true,
        ..Layout::default()
    });
    workbench.toggle_workspace(session).unwrap();
    let mut warnings = settle_workbench(&mut workbench).await;
    let source_control = workbench_frame(&mut workbench);
    assert!(
        source_control.contains(WORKBENCH_NO_GIT),
        "{source_control}"
    );
    workbench.open_remote_at(WorkspacePath::new(WORKBENCH_FILE).unwrap(), None);
    warnings.extend(settle_workbench(&mut workbench).await);
    let frame = workbench_frame(&mut workbench);
    assert!(frame.contains(WORKBENCH_CONTENT.trim_end()), "{frame}");
    assert!(!frame.contains(WORKBENCH_OTHER.trim_end()), "{frame}");
    assert!(frame.contains(WORKBENCH_HIDDEN), "{frame}");
    workbench.close();
    eprintln!("PASS real Workbench rendered bytes-A, NoGit UI, show_hidden folder");
    polled.unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    eprintln!("PASS real Workbench initial watch setup/poll/close without degraded live updates");
}

async fn authenticated_workbench_workload(client: &RemoteWorkcellClient, root: &Path) {
    let fixture = tempfile::Builder::new()
        .prefix("workbench-workload-")
        .tempdir_in(root)
        .unwrap();
    let relative = fixture.path().strip_prefix(root).unwrap().to_str().unwrap();
    let mut expected = Vec::new();
    for directory in 0..WORKBENCH_WORKLOAD_DIRECTORIES {
        let name = format!("directory-{directory:04}");
        fs::create_dir(fixture.path().join(&name)).unwrap();
        expected.push((
            workbench_path(&format!("{relative}/{name}")),
            ResourceKind::Directory,
        ));
        for file in 0..WORKBENCH_FILES_PER_DIRECTORY {
            let name = format!("{name}/file-{file}.rs");
            fs::write(fixture.path().join(&name), WORKBENCH_CONTENT).unwrap();
            expected.push((
                workbench_path(&format!("{relative}/{name}")),
                ResourceKind::File,
            ));
        }
    }
    fs::create_dir(fixture.path().join(WORKBENCH_HIDDEN)).unwrap();
    fs::write(
        fixture.path().join(WORKBENCH_HIDDEN).join("a.rs"),
        WORKBENCH_CONTENT,
    )
    .unwrap();
    expected.push((
        workbench_path(&format!("{relative}/{WORKBENCH_HIDDEN}")),
        ResourceKind::Directory,
    ));
    expected.push((
        workbench_path(&format!("{relative}/{WORKBENCH_HIDDEN}/a.rs")),
        ResourceKind::File,
    ));
    let session = workbench_session(client, relative).await;
    let mut driver = BackendDriver::new(
        WorkbenchBackend::workspace(session).unwrap(),
        workbench_path(relative),
    );
    let started = Instant::now();
    let request = driver.list(driver.root().clone(), true);
    let mut first_visible = None;
    let mut seen = BTreeSet::new();
    let mut pages = 0;
    let mut drain_time = Duration::ZERO;
    loop {
        let mut done = false;
        let drain_started = Instant::now();
        let events = driver.drain();
        drain_time += drain_started.elapsed();
        for event in events {
            let BackendEvent::Listed {
                request: returned,
                complete,
                authoritative,
                removed,
                result,
                ..
            } = event
            else {
                panic!("unexpected event: {event:?}")
            };
            assert_eq!(returned, request);
            assert!(removed.is_empty());
            let page = result.unwrap_or_else(|error| {
                panic!(
                    "Workbench list failed: {error:?}; batches={pages} retained={} elapsed_ms={} drain_ms={}",
                    seen.len(), started.elapsed().as_millis(), drain_time.as_millis()
                )
            });
            assert!(!page.incomplete);
            pages += 1;
            if !page.entries.is_empty() && first_visible.is_none() {
                first_visible = Some(started.elapsed());
                eprintln!(
                    "Workbench first_visible_ms={}",
                    started.elapsed().as_millis()
                );
                assert!(
                    !complete,
                    "first visible batch waited for complete indexing"
                );
            }
            for entry in page.entries {
                assert_eq!(entry.revision, None);
                seen.insert(entry.path.display());
            }
            if complete {
                assert!(authoritative);
                done = true;
            }
        }
        if done {
            break;
        }
        assert!(
            started.elapsed() < WORKBENCH_WORKLOAD_TIMEOUT,
            "Workbench workload exceeded bounded wait: pages={pages}, retained={}",
            seen.len()
        );
        smol::Timer::after(BATCH_POLL).await;
    }
    let complete = started.elapsed();
    assert_eq!(seen.len(), expected.len());
    assert!(!driver.is_listing());
    assert!(!driver.is_stale());
    for (path, kind) in &expected {
        let entry = driver
            .resource(path)
            .unwrap_or_else(|| panic!("not retained: {path:?}"));
        assert_eq!(&entry.kind, kind);
        assert_eq!(entry.revision, None);
        assert_eq!(
            entry.size_bytes,
            (*kind == ResourceKind::File).then_some(WORKBENCH_CONTENT.len() as u64)
        );
    }
    let entry = driver.resource(&expected[1].0).unwrap().clone();
    let open = driver.open(entry);
    let deadline = Instant::now() + BATCH_TIMEOUT;
    loop {
        let events = driver.drain();
        if !events.is_empty() {
            assert_eq!(events.len(), 1);
            let BackendEvent::Opened { request, result } = events.into_iter().next().unwrap()
            else {
                panic!("expected driver open")
            };
            assert_eq!(request, open);
            assert_eq!(result.unwrap().lines, [WORKBENCH_CONTENT.trim_end()]);
            break;
        }
        assert!(Instant::now() < deadline, "retained file did not open");
        smol::Timer::after(BATCH_POLL).await;
    }
    eprintln!(
        "PASS real Workbench driver normal_files={} normal_directories={} hidden_entries=2 retained={} pages={pages} first_visible_ms={} complete_ms={} drain_ms={} retained metadata selection opens",
        WORKBENCH_WORKLOAD_DIRECTORIES * WORKBENCH_FILES_PER_DIRECTORY,
        WORKBENCH_WORKLOAD_DIRECTORIES,
        expected.len(),
        first_visible.unwrap().as_millis(),
        complete.as_millis(),
        drain_time.as_millis()
    );
}

async fn metadata_client(state: &Path, session: &str) -> RemoteWorkcellClient {
    let credential = NamedBearerCredential::new(
        WorkcellCredentialName::new("integration").unwrap(),
        WorkcellCredential::new(
            fs::read_to_string(env::var_os("WORKCELL_TEST_TOKEN_FILE").unwrap()).unwrap(),
        )
        .unwrap(),
    );
    let selection = RemoteWorkcellSelection {
        source: WorkcellSourceRef::Direct,
        endpoint: WorkcellEndpoint::parse(&env::var("WORKCELL_TEST_ENDPOINT").unwrap()).unwrap(),
        cwd: WorkspacePath::root(),
        credential_ref: Some(WorkcellCredentialRef::from_str("credential:integration").unwrap()),
        expected_server_id: Some(ExpectedWorkcellId::new("integration-server-id").unwrap()),
        expected_workspace_id: Some(ExpectedWorkcellId::new("integration-workspace-id").unwrap()),
    };
    RemoteWorkcellClient::connect(
        &selection,
        Some(credential),
        SessionBindingId::new(session).unwrap(),
        RemoteOperationJournal::open(&StateDir::from_path(state.to_owned())).unwrap(),
        CancellationToken::new(),
    )
    .await
    .unwrap()
}

#[test]
fn metadata_workbench_nonroot() {
    let Some(root) = env::var_os("WORKCELL_TEST_METADATA_ROOT") else {
        return;
    };
    smol::block_on(async {
        let state = tempfile::tempdir().unwrap();
        let client = metadata_client(state.path(), "workbench-nonroot").await;
        authenticated_workbench_nonroot(&client, &PathBuf::from(root)).await;
    });
}

#[test]
fn metadata_workbench_workload() {
    let Some(root) = env::var_os("WORKCELL_TEST_METADATA_ROOT") else {
        return;
    };
    smol::block_on(async {
        let state = tempfile::tempdir().unwrap();
        let client = metadata_client(state.path(), "workbench-workload").await;
        authenticated_workbench_workload(&client, &PathBuf::from(root)).await;
    });
}

#[test]
fn metadata_workbench_view() {
    let Some(root) = env::var_os("WORKCELL_TEST_METADATA_ROOT") else {
        return;
    };
    smol::block_on(async {
        let root = PathBuf::from(root);
        workbench_fixture(&root);
        let state = tempfile::tempdir().unwrap();
        let client = metadata_client(state.path(), "workbench-view").await;
        authenticated_workbench_view(workbench_session(&client, "sub").await).await;
    });
}

#[test]
fn metadata_only_workspace() {
    let Some(root) = env::var_os("WORKCELL_TEST_METADATA_ROOT") else {
        return;
    };
    smol::block_on(async {
        let root = PathBuf::from(root);
        let state = tempfile::tempdir().unwrap();
        let client = metadata_client(state.path(), "metadata-session").await;
        let binding = client.session_binding();
        let cursor = client.root_cursor();
        assert!(!root.join(".git").exists());
        assert_eq!(
            WorkspaceScmReadService::discover(
                &client,
                binding,
                cursor,
                &ScmDiscoverRequest {
                    path: WorkspacePath::root()
                },
            )
            .await
            .unwrap_err(),
            WorkspaceError::NotRepository,
        );
        let fixture = tempfile::Builder::new()
            .prefix("metadata-")
            .tempdir_in(&root)
            .unwrap();
        let relative = fixture
            .path()
            .strip_prefix(&root)
            .unwrap()
            .to_str()
            .unwrap();
        let path = |name: &str| WorkspacePath::new(format!("{relative}/{name}")).unwrap();
        fs::create_dir(fixture.path().join("nested")).unwrap();
        for name in [METADATA_SMALL, METADATA_NESTED] {
            fs::write(fixture.path().join(name), METADATA_CONTENT).unwrap();
        }
        fs::write(
            fixture.path().join(METADATA_BINARY),
            vec![BINARY_BYTE; BINARY_BYTES as usize],
        )
        .unwrap();
        fs::write(
            fixture.path().join(METADATA_SOURCEMAP),
            vec![b'A'; BINARY_BYTES as usize],
        )
        .unwrap();
        let expected = [
            (METADATA_SMALL, Some(METADATA_CONTENT.len() as u64)),
            (METADATA_NESTED, Some(METADATA_CONTENT.len() as u64)),
            (METADATA_BINARY, Some(BINARY_BYTES)),
            (METADATA_SOURCEMAP, Some(BINARY_BYTES)),
            ("nested", None),
        ]
        .map(|(name, size)| (path(name), size))
        .into_iter()
        .collect::<BTreeSet<_>>();
        for limit in [2, LIMIT] {
            let mut continuation = None;
            let mut entries = BTreeSet::new();
            loop {
                let page = client
                    .list(
                        binding,
                        cursor,
                        &ListRequest {
                            parent: ResourceSelector::Path(WorkspacePath::new(relative).unwrap()),
                            recursive: true,
                            continuation,
                            limit,
                        },
                    )
                    .await
                    .unwrap();
                assert!(!page.incomplete, "{page:?}");
                assert_eq!(page.truncated, page.continuation.is_some());
                assert!(!page.resources.is_empty());
                for resource in page.resources {
                    assert_eq!(resource.revision, None, "{resource:?}");
                    assert!(entries.insert((resource.path.unwrap(), resource.size_bytes)));
                }
                continuation = page.continuation;
                if continuation.is_none() {
                    break;
                }
            }
            assert_eq!(entries, expected);
        }
        let small = ResourceSelector::Path(path(METADATA_SMALL));
        let read = client
            .read_text(
                binding,
                cursor,
                &ReadTextRequest {
                    resource: small.clone(),
                    range: None,
                    byte_offset: 0,
                    max_bytes: LIMIT,
                },
            )
            .await
            .unwrap();
        assert_eq!(read.text, METADATA_CONTENT);
        let stat = WorkspaceReadService::stat(&client, binding, cursor, &small)
            .await
            .unwrap();
        assert_eq!(stat.revision.as_ref(), Some(&read.revision));
        for name in [METADATA_BINARY, METADATA_SOURCEMAP] {
            let error = WorkspaceReadService::stat(
                &client,
                binding,
                cursor,
                &ResourceSelector::Path(path(name)),
            )
            .await
            .unwrap_err();
            assert!(
                matches!(&error, WorkspaceError::Refused { symbolic, .. } if symbolic == FILE_TOO_LARGE),
                "{error:?}"
            );
        }
        eprintln!(
            "PASS complete recursive metadata-only listing, pagination, names/sizes, absent revisions, small read/stat, explicit 6MiB binary/sourcemap stat refuses file_too_large"
        );
        overwrite_preserving_metadata(&fixture.path().join(METADATA_SMALL), METADATA_REPLACEMENT);
        for mutation in [
            Mutation::Move {
                source: path(METADATA_SMALL),
                destination: path(METADATA_MOVED),
                expected_revision: read.revision.clone(),
            },
            Mutation::Remove {
                path: path(METADATA_SMALL),
                expected_revision: read.revision,
            },
        ] {
            let refused = WorkspaceMutationService::execute(
                &client,
                binding,
                cursor,
                &MutationRequest {
                    mutations: vec![mutation],
                },
            )
            .await;
            assert!(
                matches!(
                    refused,
                    Err(WorkspaceError::StaleCursor
                        | WorkspaceError::StaleResource { .. }
                        | WorkspaceError::Conflict)
                ),
                "{refused:?}"
            );
            assert_eq!(
                fs::read_to_string(fixture.path().join(METADATA_SMALL)).unwrap(),
                METADATA_REPLACEMENT
            );
            assert!(!fixture.path().join(METADATA_MOVED).exists());
        }
        let fresh = WorkspaceReadService::stat(&client, binding, cursor, &small)
            .await
            .unwrap();
        for mutation in [
            Mutation::Move {
                source: path(METADATA_SMALL),
                destination: path(METADATA_MOVED),
                expected_revision: fresh.revision.clone().unwrap(),
            },
            Mutation::Remove {
                path: path(METADATA_MOVED),
                expected_revision: fresh.revision.unwrap(),
            },
        ] {
            let result = WorkspaceMutationService::execute(
                &client,
                binding,
                cursor,
                &MutationRequest {
                    mutations: vec![mutation],
                },
            )
            .await
            .unwrap();
            assert!(
                matches!(result.state, OperationState::Completed { .. }),
                "{result:?}"
            );
        }
        assert!(!fixture.path().join(METADATA_SMALL).exists());
        assert!(!fixture.path().join(METADATA_MOVED).exists());
        eprintln!(
            "PASS metadata-only entries require verified mutation revisions; stale same-size/mtime rename/delete refused, fresh revisions succeed"
        );
        fs::create_dir(fixture.path().join(".git")).unwrap();
        let error = WorkspaceScmReadService::discover(
            &client,
            binding,
            cursor,
            &ScmDiscoverRequest {
                path: WorkspacePath::new(relative).unwrap(),
            },
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&error, WorkspaceError::Refused { symbolic, .. } if symbolic == REPOSITORY_UNAVAILABLE),
            "{error:?}"
        );
        eprintln!(
            "PASS no .git root is typed NotRepository; corrupt .git remains repository_unavailable"
        );
        let watch_request = WatchOpenRequest {
            root: ResourceSelector::Path(path("nested")),
            recursive: true,
        };
        let watch = WorkspaceWatchService::open(&client, binding, cursor, &watch_request)
            .await
            .unwrap();
        fs::write(fixture.path().join(METADATA_BINARY), METADATA_CONTENT).unwrap();
        fs::write(fixture.path().join(METADATA_NESTED), METADATA_REPLACEMENT).unwrap();
        let deadline = Instant::now() + BATCH_TIMEOUT;
        let mut watch_cursor = watch.cursor;
        loop {
            let page = WorkspaceWatchService::poll(
                &client,
                binding,
                cursor,
                &WatchPollRequest {
                    subscription_id: watch.subscription_id.clone(),
                    cursor: watch_cursor,
                    max_events: LIMIT,
                    max_bytes: WATCH_MAX_BYTES,
                    wait_ms: WATCH_WAIT_MS,
                },
            )
            .await
            .unwrap();
            assert!(
                page.events.iter().all(|event| event
                    .path
                    .as_str()
                    .starts_with(&format!("{relative}/nested/"))),
                "{page:?}"
            );
            if page
                .events
                .iter()
                .any(|event| event.path == path(METADATA_NESTED))
            {
                break;
            }
            let WatchPollState::Current { next_cursor, .. } = page.state else {
                panic!("watch requires resync: {page:?}")
            };
            watch_cursor = next_cursor;
            assert!(
                Instant::now() < deadline,
                "scoped watch event never arrived"
            );
        }
        assert!(
            WorkspaceWatchService::close(&client, binding, cursor, &watch.subscription_id)
                .await
                .unwrap()
                .closed
        );
        let fault = PathBuf::from(env::var_os("WORKCELL_TEST_FAULT").unwrap())
            .with_extension(WATCH_FAULT_EXTENSION);
        fs::write(&fault, b"inject watch setup refusal").unwrap();
        assert_eq!(
            WorkspaceWatchService::open(&client, binding, cursor, &watch_request)
                .await
                .unwrap_err(),
            WorkspaceError::WatchUnavailable
        );
        fs::remove_file(fault).unwrap();
        eprintln!(
            "PASS real scoped recursive watch setup/poll/close, injected typed watch_unavailable"
        );
    });
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

async fn capture_workload(client: &RemoteWorkcellClient, root: &Path) {
    if env::var("WORKCELL_TEST_CAPTURE_WORKLOAD").as_deref() != Ok("1") {
        return;
    }
    let workspace = tempfile::Builder::new()
        .prefix("capture-workload-")
        .tempdir_in(root)
        .unwrap();
    for directory in 0..CAPTURE_WORKLOAD_DIRECTORIES {
        fs::create_dir(workspace.path().join(directory.to_string())).unwrap();
    }
    for index in 0..CAPTURE_WORKLOAD_FILES {
        let mut content = [b'x'; CAPTURE_WORKLOAD_FILE_BYTES];
        let prefix = format!("fixture {index}\n");
        content[..prefix.len()].copy_from_slice(prefix.as_bytes());
        fs::write(
            workspace.path().join(format!(
                "{}/{index}.txt",
                index % CAPTURE_WORKLOAD_DIRECTORIES
            )),
            content,
        )
        .unwrap();
    }
    let scope =
        WorkspacePath::new(workspace.path().file_name().unwrap().to_str().unwrap()).unwrap();
    let resolved = client
        .resolve_directory_cursor(client.session_binding(), client.root_cursor(), &scope)
        .await
        .unwrap();
    for checkpoint in CAPTURE_WORKLOAD_CHECKPOINTS {
        let started = Instant::now();
        let result = WorkspaceSnapshotReadService::capture(
            client,
            client.session_binding(),
            &resolved.cursor,
            &SnapshotCaptureRequest {
                checkpoint_id: CheckpointId::new(checkpoint).unwrap(),
                label: None,
                limits: SnapshotCaptureLimits {
                    max_files: u64::from(CAPTURE_WORKLOAD_FILES),
                    ..CAPTURE_LIMITS
                },
            },
        )
        .await
        .unwrap();
        assert!(!result.reused_checkpoint);
        assert_eq!(result.snapshot.scope, scope);
        assert_eq!(result.snapshot.file_count, CAPTURE_WORKLOAD_FILES);
        assert_eq!(
            result.snapshot.total_bytes,
            u64::from(CAPTURE_WORKLOAD_FILES) * CAPTURE_WORKLOAD_FILE_BYTES as u64
        );
        assert!(client.pending_remote_operations().is_empty());
        eprintln!(
            "PASS capture workload {checkpoint}: {} files, {} bytes, {:.3}s",
            result.snapshot.file_count,
            result.snapshot.total_bytes,
            started.elapsed().as_secs_f64()
        );
    }
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
        Box::pin(lapsed_preparation_regression(&client, &root)).await;
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
        assert!(context.skipped().is_empty(), "{:?}", context.skipped());
        // The overlay layers on top of the ranked file rather than displacing
        // it, and neither one costs the manifest the rest of its assets.
        let applicable = context.applicable_instructions(&WorkspacePath::root());
        assert_eq!(applicable.len(), 2);
        assert_eq!(applicable[0].source.path.as_str(), "AGENTS.md");
        assert!(applicable[1].is_personal_overlay());
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
                json!({"command":"pwd","timeoutSec":1}),
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
        Box::pin(remote_editor_revision_namespace(&client, &root)).await;
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
        capture_workload(&client, &root).await;
        let capture_request = SnapshotCaptureRequest {
            checkpoint_id: CheckpointId::new("integration-checkpoint").unwrap(),
            label: None,
            limits: CAPTURE_LIMITS,
        };
        let snapshot = WorkspaceSnapshotReadService::capture(
            &client,
            binding,
            &resolved_root.cursor,
            &capture_request,
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
        let changed = WorkspaceSnapshotReadService::capture(
            &client,
            binding,
            &resolved_root.cursor,
            &SnapshotCaptureRequest {
                checkpoint_id: CheckpointId::new("integration-changed").unwrap(),
                label: None,
                limits: CAPTURE_LIMITS,
            },
        )
        .await
        .unwrap();
        let restore = client
            .prepare_restore(
                binding,
                cursor,
                &snapshot.snapshot.snapshot_id,
                &changed.snapshot.snapshot_id,
            )
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
        let recovered = WorkspaceSnapshotReadService::capture(
            &client,
            client.session_binding(),
            client.root_cursor(),
            &capture_request,
        )
        .await
        .unwrap();
        assert!(recovered.reused_checkpoint);
        assert_eq!(recovered.snapshot, snapshot.snapshot);
        assert!(client.pending_remote_operations().is_empty());
        tool(
            &client,
            cursor,
            "file_read",
            json!({"filePath":"written.txt"}),
        )
        .await;
        eprintln!(
            "PASS snapshot capture/inspect/restore/unrevert and checkpoint recovery after reconnect"
        );
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
                        "timeoutSec": 1,
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
            input: json!({"command":"printf acknowledged > acknowledged.txt", "timeoutSec":1}),
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
                input: json!({"command":format!("printf '{EXECUTION_MARKER}' >> {marker}"), "timeoutSec":1}),
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
            for path in [BESIDE_UNCERTAIN_SHELL, BESIDE_UNCERTAIN_WRITE] {
                let beside = root.join("nested").join(path);
                if beside.exists() {
                    fs::remove_file(beside).unwrap();
                }
            }
            tool(
                &recovered,
                recovered.root_cursor(),
                "shell",
                json!({"command":format!("printf '{BESIDE_UNCERTAIN_CONTENT}' > nested/{BESIDE_UNCERTAIN_SHELL}")}),
            )
            .await;
            tool(
                &recovered,
                recovered.root_cursor(),
                "file_write",
                json!({"filePath":format!("nested/{BESIDE_UNCERTAIN_WRITE}"), "content":BESIDE_UNCERTAIN_CONTENT}),
            )
            .await;
            for path in [BESIDE_UNCERTAIN_SHELL, BESIDE_UNCERTAIN_WRITE] {
                assert_eq!(
                    fs::read_to_string(root.join("nested").join(path)).unwrap(),
                    BESIDE_UNCERTAIN_CONTENT
                );
            }
            assert_eq!(recovered.pending_remote_operations(), pending);
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
            assert_eq!(
                fs::read_to_string(fault.with_extension("count")).unwrap(),
                count
            );
            assert_eq!(
                fs::read_to_string(root.join("nested").join(marker)).unwrap(),
                EXECUTION_MARKER
            );
            eprintln!(
                "PASS lost response then {}: NeverSeen keeps the record until explicit acknowledgement while shells and writes run beside it",
                if restart {
                    "same-generation restart"
                } else {
                    "same-instance tombstone eviction"
                }
            );
        }
    });
}
