use std::{env, fs, path::PathBuf, str::FromStr, time::Duration};

use caudra_config::workcell::{
    ExpectedWorkcellId, RemoteWorkcellSelection, WorkcellEndpoint, WorkcellSourceRef,
};
use caudra_storage::{
    StateDir,
    auth::{WorkcellCredential, WorkcellCredentialName, WorkcellCredentialRef},
    remote_operation_journal::{RemoteOperationJournal, RemoteOperationState},
};
use caudra_workcell::{NamedBearerCredential, RemoteToolResultEnvelope, RemoteWorkcellClient};
use caudra_workspace::WorkspaceError;
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
use isahc::AsyncReadResponseExt;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const LIMIT: u32 = 100;
const EXECUTION_MARKER: &str = "executed\n";
const RECOVERED_CONTENT: &str = "exactly once\n";
const TOMBSTONE_CAPACITY: usize = 256;

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
        assert_eq!(client.canonical_catalog().len(), 19);
        for name in [
            "file_read",
            "file_glob",
            "file_grep",
            "file_write",
            "file_edit",
            "file_apply_patch",
            "shell",
            "file_download",
            "file_upload",
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
