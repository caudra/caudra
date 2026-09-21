#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    process::{Command, Stdio},
    time::Duration,
};

use reqwest::{Client, Response, StatusCode, header};
use rmcp::ServiceExt;
use serde_json::{Value, json};
#[cfg(unix)]
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::{fmt::Write as _, path::Path};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use workcell_mcp::{
    cli::{HttpBindMode, ToolGroup},
    remote_host::RemoteHostConfiguration,
    server::{ServerBehavior, ToolConfiguration, WorkcellServer},
    transports::http::{HttpAuthentication, HttpConfiguration, HttpServer, ShutdownOutcome},
};
use workcell_mcp_code::{CodeConfiguration, WorkerSource};
use workcell_mcp_shell::ShellPermissionPolicy;
use workcell_mcp_web::{ProxyConfiguration, WebsearchExecutionConfiguration};

const ACCEPT: &str = "application/json, text/event-stream";
const PROTOCOL_VERSION: &str = "2026-07-28";
const LEGACY_PROTOCOL_VERSION: &str = "2025-11-25";
const TOKEN: &str = "workcell-integration-token-with-more-than-32-bytes";

async fn fixture_server() -> (TempDir, WorkcellServer) {
    fixture_server_with_policy(ShellPermissionPolicy::restricted()).await
}

async fn fixture_server_with_policy(
    shell_policy: ShellPermissionPolicy,
) -> (TempDir, WorkcellServer) {
    fixture_server_with_options(shell_policy, false).await
}

async fn fixture_server_with_options(
    shell_policy: ShellPermissionPolicy,
    modern_only: bool,
) -> (TempDir, WorkcellServer) {
    let root = tempfile::tempdir().expect("temporary root");
    tokio::fs::write(root.path().join("visible.txt"), "visible\n")
        .await
        .expect("fixture file");
    tokio::fs::write(
        root.path().join("visible.rs"),
        "pub fn visible() -> bool { true }\n",
    )
    .await
    .expect("index fixture");
    let server = WorkcellServer::configured(
        Some(root.path()),
        &[
            ToolGroup::Files,
            ToolGroup::CodeGraph,
            ToolGroup::Web,
            ToolGroup::Shell,
        ],
        ServerBehavior {
            expose_execution_environment: true,
            modern_only,
        },
        ToolConfiguration {
            allow_write: false,
            web: WebsearchExecutionConfiguration::unconfigured(),
            web_icons: false,
            proxy: ProxyConfiguration::direct(),
            shell_policy,
            shell_output_filter: true,
            honor_gitignore: true,
            code: CodeConfiguration {
                worker: WorkerSource::Discover {
                    bundled_cache_root: None,
                },
                type_check: true,
            },
            max_transfer_bytes: workcell_mcp::cli::DEFAULT_MAX_TRANSFER_BYTES,
            snapshot_root: None,
            transfer_root: None,
            snapshot_exclusions: &[],
        },
    )
    .await
    .expect("server");
    (root, server)
}

#[cfg(unix)]
#[tokio::test]
async fn stdio_supports_legacy_initialization_and_shell_progress() {
    let (_root, server) = fixture_server_with_policy(ShellPermissionPolicy::yolo()).await;
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(async move {
        server
            .serve(server_transport)
            .await
            .expect("start MCP service")
            .waiting()
            .await
            .expect("MCP service")
    });
    let (read, mut write) = tokio::io::split(client_transport);
    let mut read = BufReader::new(read);

    write_json(
        &mut write,
        &legacy_initialize_request(1, LEGACY_PROTOCOL_VERSION),
    )
    .await;
    let initialized = read_json(&mut read).await;
    assert_eq!(
        initialized["result"]["protocolVersion"],
        LEGACY_PROTOCOL_VERSION
    );
    write_json(
        &mut write,
        &json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
    )
    .await;
    write_json(
        &mut write,
        &legacy_request(20, "ai.workcell/status", json!({})),
    )
    .await;
    let remote_refusal = read_json(&mut read).await;
    assert_eq!(remote_refusal["error"]["code"], -32601);
    write_json(
        &mut write,
        &legacy_request(21, "ai.workcell/scm-status", json!({})),
    )
    .await;
    let scm_refusal = read_json(&mut read).await;
    assert_eq!(scm_refusal["error"]["code"], -32601);
    write_json(
        &mut write,
        &legacy_request(22, "ai.workcell/snapshot-capture", json!({})),
    )
    .await;
    let snapshot_refusal = read_json(&mut read).await;
    assert_eq!(snapshot_refusal["error"]["code"], -32601);
    write_json(
        &mut write,
        &legacy_request(
            2,
            "tools/call",
            json!({"name":"execution_environment","arguments":{}}),
        ),
    )
    .await;
    let environment = read_json(&mut read).await;
    assert_environment_descriptor(&environment["result"]["structuredContent"]);

    write_json(
        &mut write,
        &legacy_request(
            3,
            "tools/call",
            json!({
                "name": "shell",
                "arguments": {"command": "printf legacy-live"},
                "_meta": {"progressToken": "legacy-progress"}
            }),
        ),
    )
    .await;

    let progress = read_json(&mut read).await;
    let result = read_json(&mut read).await;

    assert_progress(
        &progress,
        json!("legacy-progress"),
        1,
        "stdout",
        "legacy-live",
    );
    assert_eq!(result["id"], 3);
    assert_eq!(result["result"]["structuredContent"]["finalSequence"], 1);

    drop(write);
    drop(read);
    tokio::time::timeout(Duration::from_secs(2), server_task)
        .await
        .expect("server stopped")
        .expect("server task");
}

#[tokio::test]
async fn stdio_modern_only_rejects_legacy_initialization() {
    let (_root, server) =
        fixture_server_with_options(ShellPermissionPolicy::restricted(), true).await;
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(async move { server.serve(server_transport).await });
    let (read, mut write) = tokio::io::split(client_transport);
    let mut read = BufReader::new(read);

    write_json(
        &mut write,
        &legacy_initialize_request(1, LEGACY_PROTOCOL_VERSION),
    )
    .await;
    let rejected = read_json(&mut read).await;

    assert_eq!(rejected["error"]["code"], -32_022);
    assert_eq!(
        rejected["error"]["data"]["supported"],
        json!([PROTOCOL_VERSION])
    );
    drop(write);
    drop(read);
    assert!(server_task.await.unwrap().is_err());
}

#[tokio::test]
async fn stdio_rejects_modern_version_on_legacy_initialize_lifecycle() {
    let (_root, server) = fixture_server().await;
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(async move { server.serve(server_transport).await });
    let (read, mut write) = tokio::io::split(client_transport);
    let mut read = BufReader::new(read);

    write_json(&mut write, &legacy_initialize_request(1, PROTOCOL_VERSION)).await;
    let rejected = read_json(&mut read).await;

    assert_eq!(rejected["error"]["code"], -32_600);
    assert!(
        rejected["error"]["message"]
            .as_str()
            .unwrap()
            .contains("server/discover")
    );
    drop(write);
    drop(read);
    assert!(server_task.await.unwrap().is_err());
}

#[tokio::test]
async fn stdio_discovery_rejects_missing_and_legacy_request_context() {
    let (_root, server) = fixture_server().await;
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(async move {
        server
            .serve(server_transport)
            .await
            .expect("start MCP service")
            .waiting()
            .await
            .expect("MCP service")
    });
    let (read, mut write) = tokio::io::split(client_transport);
    let mut read = BufReader::new(read);

    write_json(&mut write, &discover_request(1, json!({}))).await;
    assert!(read_json(&mut read).await["result"].is_object());
    write_json(
        &mut write,
        &json!({"jsonrpc":"2.0","id":2,"method":"server/discover","params":{}}),
    )
    .await;
    assert_eq!(read_json(&mut read).await["error"]["code"], -32_602);

    let mut legacy = discover_request(3, json!({}));
    legacy["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"] =
        json!(LEGACY_PROTOCOL_VERSION);
    write_json(&mut write, &legacy).await;
    assert_eq!(read_json(&mut read).await["error"]["code"], -32_022);

    drop(write);
    drop(read);
    tokio::time::timeout(Duration::from_secs(2), server_task)
        .await
        .expect("server stopped")
        .expect("server task");
}

#[cfg(unix)]
#[tokio::test]
async fn stdio_streams_standard_shell_progress_before_the_result() {
    let (_root, server) = fixture_server_with_policy(ShellPermissionPolicy::yolo()).await;
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(async move {
        server
            .serve(server_transport)
            .await
            .expect("start MCP service")
            .waiting()
            .await
            .expect("MCP service")
    });
    let (read, mut write) = tokio::io::split(client_transport);
    let mut read = BufReader::new(read);

    write_json(&mut write, &discover_request(1, json!({}))).await;
    read_json(&mut read).await;
    write_json(
        &mut write,
        &mcp_request(
            2,
            "tools/call",
            json!({
                "name": "shell",
                "arguments": {
                    "command": "printf live-output"
                },
                "_meta": {"progressToken": "shell-progress"}
            }),
        ),
    )
    .await;

    let first = read_json(&mut read).await;
    let result = read_json(&mut read).await;

    assert_progress(&first, json!("shell-progress"), 1, "stdout", "live-output");
    assert_eq!(result["id"], 2);
    assert_eq!(result["result"]["structuredContent"]["finalSequence"], 1);

    drop(write);
    drop(read);
    tokio::time::timeout(Duration::from_secs(2), server_task)
        .await
        .expect("server stopped")
        .expect("server task");
}

#[tokio::test]
async fn stdio_discovers_lists_and_calls_all_standalone_tools() {
    let (root, server) = fixture_server().await;
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(async move {
        server
            .serve(server_transport)
            .await
            .expect("start MCP service")
            .waiting()
            .await
            .expect("MCP service")
    });
    let (read, mut write) = tokio::io::split(client_transport);
    let mut read = BufReader::new(read);

    write_json(
        &mut write,
        &discover_request(
            1,
            json!({
                "extensions": {
                    "ai.workcell/execution-environment": {"versions": ["v1"]},
                    "ai.workcell/remote-host": {"versions": ["v1"]}
                }
            }),
        ),
    )
    .await;
    let discovered = read_json(&mut read).await;
    assert_eq!(
        discovered["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "workcell-mcp"
    );
    assert_eq!(
        discovered["result"]["supportedVersions"],
        supported_dual_versions()
    );
    assert_eq!(
        discovered["result"]["capabilities"]["extensions"]["ai.workcell/execution-environment"]["version"],
        "v1"
    );
    assert_environment_descriptor(
        &discovered["result"]["capabilities"]["extensions"]["ai.workcell/execution-environment"],
    );
    assert!(
        discovered["result"]["capabilities"]["extensions"]["ai.workcell/remote-host"].is_null()
    );

    write_json(
        &mut write,
        &remote_request(
            20,
            "ai.workcell/watch-open",
            json!({
                "version":"v1",
                "host":{
                    "serverId":"server",
                    "instanceId":"instance",
                    "workspaceId":"workspace",
                    "rootProjectId":"project",
                    "principalId":"principal",
                    "cwdHandle":"cwd",
                    "catalogRevision":"catalog",
                    "policyRevision":"policy"
                },
                "cwdHandle":"cwd",
                "path":".",
                "recursive":true
            }),
        ),
    )
    .await;
    let refused = read_json(&mut read).await;
    assert_eq!(refused["error"]["code"], -32601);

    write_json(&mut write, &mcp_request(2, "tools/list", json!({}))).await;
    let listed = read_json(&mut read).await;
    let names = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    // This fixture runs without write access, so the mutation tools are absent
    // rather than advertised as calls that could only ever fail.
    assert_eq!(
        names,
        [
            "file_read",
            "file_glob",
            "file_grep",
            "file_index",
            "code_map",
            "code_context",
            "code_refs",
            "code_impact",
            "code_expand",
            "websearch",
            "webfetch",
            "shell",
            "execution_environment",
        ]
    );

    write_json(
        &mut write,
        &mcp_request(
            21,
            "tools/call",
            json!({"name":"file_write","arguments":{"filePath":"new.txt","content":"x"}}),
        ),
    )
    .await;
    let unknown = read_json(&mut read).await;
    assert!(unknown["result"].is_null());
    assert!(unknown["error"].is_object());
    assert!(!root.path().join("new.txt").exists());

    write_json(
        &mut write,
        &mcp_request(
            3,
            "tools/call",
            json!({"name":"file_read","arguments":{"filePath":"visible.txt"}}),
        ),
    )
    .await;
    assert!(read_json(&mut read).await.to_string().contains("visible"));

    write_json(
        &mut write,
        &mcp_request(
            31,
            "tools/call",
            json!({"name":"file_index","arguments":{"path":"visible.rs"}}),
        ),
    )
    .await;
    let indexed = read_json(&mut read).await;
    assert_eq!(
        indexed["result"]["content"][0]["text"],
        "fns:\n  pub visible() -> bool [1]"
    );
    assert_eq!(indexed["result"]["structuredContent"]["language"], "rust");

    write_json(
        &mut write,
        &mcp_request(32, "tools/call", json!({"name":"code_map","arguments":{}})),
    )
    .await;
    let mapped = read_json(&mut read).await;
    assert_eq!(
        mapped["result"]["structuredContent"]["symbols"][0]["name"],
        "visible"
    );
    assert!(
        mapped["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("visible.rs")
    );

    write_json(
        &mut write,
        &mcp_request(
            4,
            "tools/call",
            json!({"name":"execution_environment","arguments":{}}),
        ),
    )
    .await;
    let environment = read_json(&mut read).await;
    assert_environment_descriptor(&environment["result"]["structuredContent"]);
    assert_eq!(
        environment["result"]["structuredContent"]["toolGroups"],
        json!({
            "files": true,
            "web": true,
            "shell": true,
            "code": false,
            "codeGraph": true,
        })
    );

    write_json(
        &mut write,
        &mcp_request(
            5,
            "tools/call",
            json!({"name":"shell","arguments":{"command":"printf hello"}}),
        ),
    )
    .await;
    let denied = read_json(&mut read).await;
    assert_eq!(denied["result"]["isError"], true);
    assert!(denied.to_string().contains("requires an allow rule"));
    assert!(denied.to_string().contains("Workcell operator"));

    drop(write);
    drop(read);
    tokio::time::timeout(Duration::from_secs(2), server_task)
        .await
        .expect("server stopped")
        .expect("server task");
}

#[tokio::test]
async fn authenticated_http_has_one_stateless_mcp_route() {
    let (_root, server) = fixture_server().await;
    let http = HttpServer::start(
        server,
        0,
        HttpConfiguration {
            bind_mode: HttpBindMode::Loopback,
            allowed_hosts: vec!["127.0.0.1".into()],
            authentication: Some(HttpAuthentication::new(TOKEN).unwrap()),
            remote_host: None,
        },
    )
    .await
    .expect("HTTP server");
    let endpoint = format!("http://{}/mcp", http.address());
    let client = Client::new();

    let unauthenticated = post_rpc(&client, &endpoint, None, discover_request(1, json!({}))).await;
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

    let discovered = post_rpc(
        &client,
        &endpoint,
        Some(TOKEN),
        discover_request(2, json!({})),
    )
    .await;
    assert_eq!(discovered.status(), StatusCode::OK);
    assert_eq!(
        final_sse_json(discovered).await["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "workcell-mcp"
    );

    let listed = post_rpc(
        &client,
        &endpoint,
        Some(TOKEN),
        mcp_request(3, "tools/list", json!({})),
    )
    .await;
    assert_eq!(listed.status(), StatusCode::OK);
    // The fixture server has no write access, so the three mutation tools are absent.
    assert_eq!(
        final_sse_json(listed).await["result"]["tools"]
            .as_array()
            .unwrap()
            .len(),
        13
    );

    let private_route = client
        .post(format!("http://{}/internal/leases", http.address()))
        .bearer_auth(TOKEN)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(private_route.status(), StatusCode::NOT_FOUND);

    let delete = client
        .delete(&endpoint)
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(delete.status(), StatusCode::METHOD_NOT_ALLOWED);

    let invalid_utf8 = client
        .post(&endpoint)
        .bearer_auth(TOKEN)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, ACCEPT)
        .header("MCP-Protocol-Version", PROTOCOL_VERSION)
        .header("Mcp-Method", "tools/call")
        .header("Mcp-Name", "shell")
        .body(vec![0xff, 0xfe])
        .send()
        .await
        .unwrap();
    assert_eq!(invalid_utf8.status(), StatusCode::BAD_REQUEST);
    let invalid_utf8 = invalid_utf8.text().await.unwrap().to_lowercase();
    assert!(
        invalid_utf8.contains("json")
            || invalid_utf8.contains("utf")
            || invalid_utf8.contains("parse"),
        "invalid payload error was not actionable: {invalid_utf8:?}"
    );
    assert_eq!(http.shutdown().await, ShutdownOutcome::Completed);
    assert_eq!(http.shutdown().await, ShutdownOutcome::AlreadyStopped);
}

#[tokio::test]
async fn authenticated_http_discovers_one_opt_in_remote_environment() {
    let (_root, server) = fixture_server().await;
    let catalog_revision = server.catalog_revision().as_str().to_owned();
    let http = HttpServer::start(
        server,
        0,
        HttpConfiguration {
            bind_mode: HttpBindMode::Loopback,
            allowed_hosts: vec!["127.0.0.1".into()],
            authentication: Some(HttpAuthentication::new(TOKEN).unwrap()),
            remote_host: Some(
                RemoteHostConfiguration::new(
                    "server-a".into(),
                    "workspace-a".into(),
                    "generation-a".into(),
                    "project-a".into(),
                    "principal-a".into(),
                )
                .unwrap(),
            ),
        },
    )
    .await
    .expect("HTTP server");
    let endpoint = format!("http://{}/mcp", http.address());
    let client = Client::new();
    let requested = json!({
        "extensions": {
            "ai.workcell/remote-host": {"versions": ["v1"]}
        }
    });

    let unauthenticated = post_rpc(
        &client,
        &endpoint,
        None,
        discover_request(1, requested.clone()),
    )
    .await;
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

    let unrequested = post_rpc(
        &client,
        &endpoint,
        Some(TOKEN),
        discover_request(2, json!({})),
    )
    .await;
    let unrequested = final_sse_json(unrequested).await;
    assert!(
        unrequested["result"]["capabilities"]["extensions"]["ai.workcell/remote-host"].is_null()
    );

    let first = post_rpc(
        &client,
        &endpoint,
        Some(TOKEN),
        discover_request(3, requested.clone()),
    )
    .await;
    let first = final_sse_json(first).await;
    let descriptor = &first["result"]["capabilities"]["extensions"]["ai.workcell/remote-host"];
    assert_eq!(descriptor["version"], "v1");
    assert_eq!(descriptor["serverId"], "server-a");
    assert_eq!(descriptor["workspaceId"], "workspace-a");
    assert_eq!(descriptor["workspaceGeneration"], "generation-a");
    assert_eq!(descriptor["rootProjectId"], "project-a");
    assert_eq!(descriptor["principalId"], "principal-a");
    assert_eq!(descriptor["resourceNamespaceVersion"], "v1");
    assert_eq!(descriptor["pathStyle"], "root-relative-posix");
    assert!(
        descriptor["cwd"]["handle"]
            .as_str()
            .unwrap()
            .starts_with("cwd_")
    );
    assert_eq!(descriptor["cwd"]["displayPath"], ".");
    assert_eq!(descriptor["revisions"]["catalog"], catalog_revision);
    for revision in [
        &descriptor["revisions"]["executionEnvironment"],
        &descriptor["revisions"]["catalog"],
        &descriptor["revisions"]["policy"],
    ] {
        assert!(revision.as_str().unwrap().starts_with("sha256:"));
    }
    assert_eq!(
        descriptor["capabilities"]["operations"]["methods"]["prepare"],
        true
    );
    assert_eq!(
        descriptor["capabilities"]["operations"]["exactPreparation"],
        true
    );
    assert_eq!(descriptor["capabilities"]["controlPlane"], false);
    assert_eq!(
        descriptor["capabilities"]["controlPlaneMissing"],
        json!(["workspaceMutation", "snapshots"])
    );
    assert!(descriptor["capabilities"]["snapshots"].is_null());
    assert_eq!(descriptor["capabilities"]["scm"]["version"], "v1");
    assert_eq!(
        descriptor["capabilities"]["scm"]["limits"]["maxConcurrentOperations"],
        workcell_workspace_scm::MAX_CONCURRENT_SCM_OPERATIONS
    );
    assert_eq!(
        descriptor["capabilities"]["scm"]["limits"]["maxConfigBytes"],
        workcell_host_contract::MAX_SCM_CONFIG_BYTES
    );
    assert_eq!(
        descriptor["capabilities"]["scm"]["limits"]["maxPaths"],
        workcell_host_contract::MAX_SCM_PATHS
    );
    assert_eq!(
        descriptor["capabilities"]["scm"]["limits"]["maxCommitBytes"],
        workcell_host_contract::MAX_SCM_COMMIT_BYTES
    );
    assert_eq!(
        descriptor["capabilities"]["scm"]["limits"]["maxLogScanBytes"],
        workcell_host_contract::MAX_SCM_LOG_SCAN_BYTES
    );
    assert_eq!(descriptor["capabilities"]["scm"]["methods"]["status"], true);
    assert_eq!(descriptor["capabilities"]["scm"]["methods"]["stage"], false);
    assert_eq!(descriptor["capabilities"]["scm"]["discardUntracked"], false);
    assert_eq!(descriptor["capabilities"]["workspace"]["version"], "v1");
    assert_eq!(
        descriptor["capabilities"]["workspace"]["methods"]["resolveDirectory"],
        true
    );
    assert!(descriptor["capabilities"]["workspaceMutation"].is_null());
    assert_eq!(descriptor["capabilities"]["watch"]["methods"]["poll"], true);
    assert_eq!(
        descriptor["capabilities"]["watch"]["exactRenamePairing"],
        false
    );
    assert_eq!(
        descriptor["capabilities"]["projectAssets"]["manifestVersion"],
        "project-assets.v1"
    );
    assert_eq!(
        descriptor["capabilities"]["projectAssets"]["limits"],
        json!({
            "maxAssets": workcell_host_contract::MAX_PROJECT_ASSETS,
            "maxReadBytes": workcell_host_contract::MAX_PROJECT_ASSET_READ_BYTES,
            "maxPathBytes": workcell_host_contract::MAX_WORKSPACE_PATH_BYTES,
            "maxDiscoveryEntries": workcell_host_contract::MAX_WORKSPACE_LIST_ENTRIES,
            "maxDiscoveryRetainedBytes": workcell_host_contract::MAX_WORKSPACE_LIST_RETAINED_BYTES,
            "maxDiscoveryHashBytes": workcell_host_contract::MAX_WORKSPACE_LIST_HASH_BYTES,
        })
    );
    assert_eq!(descriptor["capabilities"]["directExec"]["prepared"], true);
    assert_eq!(
        descriptor["capabilities"]["toolExecution"]["limits"]["maxRequestBytes"],
        workcell_mcp::http_policy::MAX_JSON_BODY_BYTES
    );
    assert!(descriptor["capabilities"].get("fileTransfer").is_none());
    assert!(descriptor["capabilities"].get("reviewedTransfer").is_none());

    let second = post_rpc(
        &client,
        &endpoint,
        Some(TOKEN),
        discover_request(4, requested),
    )
    .await;
    let second = final_sse_json(second).await;
    assert_eq!(
        descriptor["instanceId"],
        second["result"]["capabilities"]["extensions"]["ai.workcell/remote-host"]["instanceId"]
    );
    assert_eq!(http.shutdown().await, ShutdownOutcome::Completed);
}

#[tokio::test]
async fn authenticated_snapshot_restore_is_negotiated_and_uses_the_common_ledger() {
    let root = tempfile::tempdir().expect("temporary root");
    let snapshot_root = tempfile::tempdir().expect("snapshot root");
    #[cfg(unix)]
    std::fs::set_permissions(snapshot_root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    tokio::fs::write(root.path().join("state.txt"), "before")
        .await
        .unwrap();
    let server = WorkcellServer::configured(
        Some(root.path()),
        &[ToolGroup::Files, ToolGroup::Shell],
        ServerBehavior {
            expose_execution_environment: false,
            modern_only: true,
        },
        ToolConfiguration {
            allow_write: true,
            web: WebsearchExecutionConfiguration::unconfigured(),
            web_icons: false,
            proxy: ProxyConfiguration::direct(),
            shell_policy: ShellPermissionPolicy::restricted(),
            shell_output_filter: true,
            honor_gitignore: true,
            code: CodeConfiguration {
                worker: WorkerSource::Discover {
                    bundled_cache_root: None,
                },
                type_check: true,
            },
            max_transfer_bytes: workcell_mcp::cli::DEFAULT_MAX_TRANSFER_BYTES,
            snapshot_root: Some(snapshot_root.path()),
            transfer_root: None,
            snapshot_exclusions: &[],
        },
    )
    .await
    .unwrap();
    let http = HttpServer::start(
        server,
        0,
        HttpConfiguration {
            bind_mode: HttpBindMode::Loopback,
            allowed_hosts: vec!["127.0.0.1".into()],
            authentication: Some(HttpAuthentication::new(TOKEN).unwrap()),
            remote_host: Some(
                RemoteHostConfiguration::new(
                    "server-snapshot".into(),
                    "workspace-snapshot".into(),
                    "generation-snapshot".into(),
                    "project-snapshot".into(),
                    "principal-snapshot".into(),
                )
                .unwrap(),
            ),
        },
    )
    .await
    .unwrap();
    let endpoint = format!("http://{}/mcp", http.address());
    let client = Client::new();
    let discovery = final_sse_json(
        post_rpc(
            &client,
            &endpoint,
            Some(TOKEN),
            discover_request(
                1,
                json!({"extensions":{"ai.workcell/remote-host":{"versions":["v1"]}}}),
            ),
        )
        .await,
    )
    .await;
    let descriptor = &discovery["result"]["capabilities"]["extensions"]["ai.workcell/remote-host"];
    assert_eq!(descriptor["capabilities"]["controlPlane"], true);
    assert_eq!(descriptor["capabilities"]["controlPlaneMissing"], json!([]));
    assert_eq!(descriptor["capabilities"]["snapshots"]["version"], "v1");
    assert_eq!(
        descriptor["capabilities"]["snapshots"]["atomicAcrossFiles"],
        false
    );
    assert_eq!(
        descriptor["capabilities"]["snapshots"]["limits"]["maxCaptureEntries"],
        workcell_host_contract::MAX_SNAPSHOT_CAPTURE_ENTRIES
    );
    assert_eq!(
        descriptor["capabilities"]["snapshots"]["limits"]["maxCapturePathBytes"],
        workcell_host_contract::MAX_SNAPSHOT_CAPTURE_PATH_BYTES
    );
    assert_eq!(descriptor["capabilities"]["watch"]["methods"]["poll"], true);
    assert_eq!(descriptor["capabilities"]["scm"]["methods"]["status"], true);

    let capture_params = json!({
        "version":"v1",
        "host":remote_host_binding(descriptor),
        "cwdHandle":descriptor["cwd"]["handle"],
        "checkpointId":"checkpoint-http"
    });
    let unauthenticated = post_rpc(
        &client,
        &endpoint,
        None,
        remote_request(2, "ai.workcell/snapshot-capture", capture_params.clone()),
    )
    .await;
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
    let unnegotiated = final_sse_json(
        post_rpc(
            &client,
            &endpoint,
            Some(TOKEN),
            mcp_request(3, "ai.workcell/snapshot-capture", capture_params.clone()),
        )
        .await,
    )
    .await;
    assert_eq!(unnegotiated["error"]["code"], -32601);
    let capture = final_sse_json(
        post_rpc(
            &client,
            &endpoint,
            Some(TOKEN),
            remote_request(4, "ai.workcell/snapshot-capture", capture_params),
        )
        .await,
    )
    .await;
    let snapshot_id = capture["result"]["snapshot"]["snapshotId"].clone();
    assert_eq!(capture["result"]["snapshot"]["state"], "complete");

    tokio::fs::write(root.path().join("state.txt"), "after")
        .await
        .unwrap();
    let prepare = final_sse_json(
        post_rpc(
            &client,
            &endpoint,
            Some(TOKEN),
            remote_request(
                5,
                "ai.workcell/snapshot-prepare-restore",
                json!({
                    "version":"v1",
                    "host":remote_host_binding(descriptor),
                    "cwdHandle":descriptor["cwd"]["handle"],
                    "snapshotId":snapshot_id
                }),
            ),
        )
        .await,
    )
    .await;
    assert_eq!(
        prepare["result"]["preview"]["changes"][0]["kind"],
        "replace"
    );
    assert_eq!(
        prepare["result"]["preview"]["createdDirectories"],
        json!([])
    );
    let restore_id = prepare["result"]["preview"]["restoreId"].clone();
    assert!(restore_id.as_str().unwrap().starts_with("restore_"));
    assert_eq!(
        prepare["result"]["operation"]["intent"]["resources"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    assert!(
        prepare["result"]["operation"]["intent"]["resources"][1]["display"]
            .as_str()
            .unwrap()
            .starts_with("snapshot-store:pre-restore:snap_")
    );
    let execute = remote_request(
        6,
        "ai.workcell/execute",
        json!({
            "version":"v1",
            "preparationId":prepare["result"]["operation"]["preparationId"],
            "invocationId":"snapshot-restore-http",
            "host":remote_host_binding(descriptor)
        }),
    );
    let completed =
        final_sse_json(post_rpc(&client, &endpoint, Some(TOKEN), execute.clone()).await).await;
    assert_eq!(completed["result"]["state"], "completed", "{completed}");
    assert_eq!(
        completed["result"]["outcome"]["result"]["structuredContent"]["state"],
        "completed"
    );
    assert_eq!(
        completed["result"]["outcome"]["result"]["structuredContent"]["restoreId"],
        restore_id
    );
    assert_eq!(
        tokio::fs::read_to_string(root.path().join("state.txt"))
            .await
            .unwrap(),
        "before"
    );
    let duplicate = final_sse_json(post_rpc(&client, &endpoint, Some(TOKEN), execute).await).await;
    assert_eq!(duplicate["result"], completed["result"]);
    assert_eq!(http.shutdown().await, ShutdownOutcome::Completed);

    let reopened = WorkcellServer::configured(
        Some(root.path()),
        &[ToolGroup::Files, ToolGroup::Shell],
        ServerBehavior {
            expose_execution_environment: false,
            modern_only: true,
        },
        ToolConfiguration {
            allow_write: true,
            web: WebsearchExecutionConfiguration::unconfigured(),
            web_icons: false,
            proxy: ProxyConfiguration::direct(),
            shell_policy: ShellPermissionPolicy::restricted(),
            shell_output_filter: true,
            honor_gitignore: true,
            code: CodeConfiguration {
                worker: WorkerSource::Discover {
                    bundled_cache_root: None,
                },
                type_check: true,
            },
            max_transfer_bytes: workcell_mcp::cli::DEFAULT_MAX_TRANSFER_BYTES,
            snapshot_root: Some(snapshot_root.path()),
            transfer_root: None,
            snapshot_exclusions: &[],
        },
    )
    .await
    .unwrap();
    let reopened_http = HttpServer::start(
        reopened,
        0,
        HttpConfiguration {
            bind_mode: HttpBindMode::Loopback,
            allowed_hosts: vec!["127.0.0.1".into()],
            authentication: Some(HttpAuthentication::new(TOKEN).unwrap()),
            remote_host: Some(
                RemoteHostConfiguration::new(
                    "server-snapshot".into(),
                    "workspace-snapshot".into(),
                    "generation-snapshot".into(),
                    "project-snapshot".into(),
                    "principal-snapshot".into(),
                )
                .unwrap(),
            ),
        },
    )
    .await
    .unwrap();
    let reopened_endpoint = format!("http://{}/mcp", reopened_http.address());
    let reopened_discovery = final_sse_json(
        post_rpc(
            &client,
            &reopened_endpoint,
            Some(TOKEN),
            discover_request(
                7,
                json!({"extensions":{"ai.workcell/remote-host":{"versions":["v1"]}}}),
            ),
        )
        .await,
    )
    .await;
    let reopened_descriptor =
        &reopened_discovery["result"]["capabilities"]["extensions"]["ai.workcell/remote-host"];
    let restored_status = final_sse_json(
        post_rpc(
            &client,
            &reopened_endpoint,
            Some(TOKEN),
            remote_request(
                8,
                "ai.workcell/snapshot-status",
                json!({
                    "version":"v1",
                    "host":remote_host_binding(reopened_descriptor),
                    "cwdHandle":reopened_descriptor["cwd"]["handle"],
                    "restoreId":restore_id
                }),
            ),
        )
        .await,
    )
    .await;
    assert_eq!(restored_status["result"]["restore"]["state"], "completed");
    assert_eq!(
        restored_status["result"]["restore"]["restoreId"],
        restore_id
    );
    assert_eq!(reopened_http.shutdown().await, ShutdownOutcome::Completed);
}

#[tokio::test]
async fn authenticated_remote_mutation_is_prepared_once_replayed_and_released() {
    let root = tempfile::tempdir().expect("temporary root");
    let server = WorkcellServer::configured(
        Some(root.path()),
        &[ToolGroup::Files],
        ServerBehavior {
            expose_execution_environment: false,
            modern_only: true,
        },
        ToolConfiguration {
            allow_write: true,
            web: WebsearchExecutionConfiguration::unconfigured(),
            web_icons: false,
            proxy: ProxyConfiguration::direct(),
            shell_policy: ShellPermissionPolicy::restricted(),
            shell_output_filter: true,
            honor_gitignore: true,
            code: CodeConfiguration {
                worker: WorkerSource::Discover {
                    bundled_cache_root: None,
                },
                type_check: true,
            },
            max_transfer_bytes: workcell_mcp::cli::DEFAULT_MAX_TRANSFER_BYTES,
            snapshot_root: None,
            transfer_root: None,
            snapshot_exclusions: &[],
        },
    )
    .await
    .expect("server");
    let http = HttpServer::start(
        server,
        0,
        HttpConfiguration {
            bind_mode: HttpBindMode::Loopback,
            allowed_hosts: vec!["127.0.0.1".into()],
            authentication: Some(HttpAuthentication::new(TOKEN).unwrap()),
            remote_host: Some(
                RemoteHostConfiguration::new(
                    "server-a".into(),
                    "workspace-a".into(),
                    "generation-a".into(),
                    "project-a".into(),
                    "principal-a".into(),
                )
                .unwrap(),
            ),
        },
    )
    .await
    .expect("HTTP server");
    let endpoint = format!("http://{}/mcp", http.address());
    let client = Client::new();
    let discovery = final_sse_json(
        post_rpc(
            &client,
            &endpoint,
            Some(TOKEN),
            discover_request(
                1,
                json!({"extensions":{"ai.workcell/remote-host":{"versions":["v1"]}}}),
            ),
        )
        .await,
    )
    .await;
    let descriptor = &discovery["result"]["capabilities"]["extensions"]["ai.workcell/remote-host"];
    let host = remote_host_binding(descriptor);
    let target = root.path().join("once.txt");
    let prepare = remote_request(
        2,
        "ai.workcell/prepare",
        json!({
            "version":"v1",
            "host":host,
            "tool":"file_write",
            "contract":{"id":"file.write.v1","version":"v1","resultVersion":"v1"},
            "arguments":{"filePath":"once.txt","content":"first"}
        }),
    );
    let unauthenticated = post_rpc(&client, &endpoint, None, prepare.clone()).await;
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
    let mut unrequested_params = prepare["params"].clone();
    unrequested_params.as_object_mut().unwrap().remove("_meta");
    let unrequested = final_sse_json(
        post_rpc(
            &client,
            &endpoint,
            Some(TOKEN),
            mcp_request(20, "ai.workcell/prepare", unrequested_params),
        )
        .await,
    )
    .await;
    assert_eq!(unrequested["error"]["code"], -32601);
    let prepared = final_sse_json(post_rpc(&client, &endpoint, Some(TOKEN), prepare).await).await;
    assert!(prepared["error"].is_null(), "{prepared}");
    assert!(!target.exists(), "prepare must not write");
    assert_eq!(prepared["result"]["intent"]["mutating"], true);
    let preparation_id = prepared["result"]["preparationId"].clone();
    let replay_attempt = remote_request(
        3,
        "ai.workcell/execute",
        json!({
            "version":"v1",
            "preparationId":preparation_id,
            "invocationId":"invocation-a",
            "host":remote_host_binding(descriptor),
            "arguments":{"filePath":"replayed.txt","content":"replacement"}
        }),
    );
    let replay_refused =
        final_sse_json(post_rpc(&client, &endpoint, Some(TOKEN), replay_attempt).await).await;
    assert_eq!(replay_refused["error"]["data"]["code"], "invalid_request");
    let execute = remote_request(
        21,
        "ai.workcell/execute",
        json!({
            "version":"v1",
            "preparationId":preparation_id,
            "invocationId":"invocation-a",
            "host":remote_host_binding(descriptor)
        }),
    );
    let completed =
        final_sse_json(post_rpc(&client, &endpoint, Some(TOKEN), execute.clone()).await).await;
    assert_eq!(completed["result"]["state"], "completed", "{completed}");
    let tool_result = &completed["result"]["outcome"]["result"];
    assert_eq!(tool_result["version"], "v1");
    assert_eq!(tool_result["isError"], false);
    assert_eq!(tool_result["content"][0]["type"], "text");
    assert!(tool_result["structuredContent"].is_object());
    assert!(tool_result["resultType"].is_null());
    assert_eq!(tokio::fs::read_to_string(&target).await.unwrap(), "first");
    assert!(!root.path().join("replayed.txt").exists());
    tokio::fs::write(&target, "changed-after-response")
        .await
        .unwrap();
    let replayed = final_sse_json(post_rpc(&client, &endpoint, Some(TOKEN), execute).await).await;
    assert_eq!(replayed["result"], completed["result"]);
    assert_eq!(
        tokio::fs::read_to_string(&target).await.unwrap(),
        "changed-after-response"
    );

    let mut wrong_host = remote_host_binding(descriptor);
    wrong_host["instanceId"] = json!("other-instance");
    wrong_host["workspaceGeneration"] = json!("other-generation");
    let mismatch = final_sse_json(
        post_rpc(
            &client,
            &endpoint,
            Some(TOKEN),
            remote_request(
                4,
                "ai.workcell/status",
                json!({
                    "version":"v1",
                    "preparationId":preparation_id,
                    "invocationId":"invocation-a",
                    "host":wrong_host
                }),
            ),
        )
        .await,
    )
    .await;
    assert_eq!(mismatch["error"]["data"]["code"], "binding_mismatch");

    let released = final_sse_json(
        post_rpc(
            &client,
            &endpoint,
            Some(TOKEN),
            remote_request(
                5,
                "ai.workcell/release",
                json!({
                    "version":"v1",
                    "preparationId":preparation_id,
                    "invocationId":"invocation-a",
                    "host":remote_host_binding(descriptor)
                }),
            ),
        )
        .await,
    )
    .await;
    assert_eq!(released["result"]["released"], true);
    let forgotten = final_sse_json(
        post_rpc(
            &client,
            &endpoint,
            Some(TOKEN),
            remote_request(
                6,
                "ai.workcell/status",
                json!({
                    "version":"v1",
                    "preparationId":preparation_id,
                    "invocationId":"invocation-a",
                    "host":remote_host_binding(descriptor)
                }),
            ),
        )
        .await,
    )
    .await;
    assert_eq!(forgotten["result"]["state"], "forgotten");
    assert_eq!(http.shutdown().await, ShutdownOutcome::Completed);
}

#[tokio::test]
async fn authenticated_scm_is_negotiated_structured_and_uses_the_common_ledger() {
    let root = tempfile::tempdir().expect("temporary root");
    git(root.path(), &["init", "--quiet", "--initial-branch=main"]);
    git(root.path(), &["config", "user.name", "Workcell Test"]);
    git(
        root.path(),
        &["config", "user.email", "workcell@example.invalid"],
    );
    tokio::fs::write(root.path().join("tracked.txt"), "before\n")
        .await
        .unwrap();
    git(root.path(), &["add", "--", "tracked.txt"]);
    git(root.path(), &["commit", "-m", "initial"]);
    let server = WorkcellServer::configured(
        Some(root.path()),
        &[ToolGroup::Files],
        ServerBehavior {
            expose_execution_environment: false,
            modern_only: true,
        },
        ToolConfiguration {
            allow_write: true,
            web: WebsearchExecutionConfiguration::unconfigured(),
            web_icons: false,
            proxy: ProxyConfiguration::direct(),
            shell_policy: ShellPermissionPolicy::restricted(),
            shell_output_filter: true,
            honor_gitignore: true,
            code: CodeConfiguration {
                worker: WorkerSource::Discover {
                    bundled_cache_root: None,
                },
                type_check: true,
            },
            max_transfer_bytes: workcell_mcp::cli::DEFAULT_MAX_TRANSFER_BYTES,
            snapshot_root: None,
            transfer_root: None,
            snapshot_exclusions: &[],
        },
    )
    .await
    .unwrap();
    let http = HttpServer::start(
        server,
        0,
        HttpConfiguration {
            bind_mode: HttpBindMode::Loopback,
            allowed_hosts: vec!["127.0.0.1".into()],
            authentication: Some(HttpAuthentication::new(TOKEN).unwrap()),
            remote_host: Some(
                RemoteHostConfiguration::new(
                    "server-a".into(),
                    "workspace-a".into(),
                    "generation-a".into(),
                    "project-a".into(),
                    "principal-a".into(),
                )
                .unwrap(),
            ),
        },
    )
    .await
    .unwrap();
    let endpoint = format!("http://{}/mcp", http.address());
    let client = Client::new();
    let discovery = final_sse_json(
        post_rpc(
            &client,
            &endpoint,
            Some(TOKEN),
            discover_request(
                1,
                json!({"extensions":{"ai.workcell/remote-host":{"versions":["v1"]}}}),
            ),
        )
        .await,
    )
    .await;
    let descriptor = &discovery["result"]["capabilities"]["extensions"]["ai.workcell/remote-host"];
    assert_eq!(descriptor["capabilities"]["scm"]["methods"]["stage"], true);
    assert_eq!(descriptor["capabilities"]["controlPlane"], false);
    let params = json!({
        "version":"v1",
        "host":remote_host_binding(descriptor),
        "cwdHandle":descriptor["cwd"]["handle"],
        "path":"."
    });
    let unauthenticated = post_rpc(
        &client,
        &endpoint,
        None,
        remote_request(2, "ai.workcell/scm-discover", params.clone()),
    )
    .await;
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
    let unnegotiated = final_sse_json(
        post_rpc(
            &client,
            &endpoint,
            Some(TOKEN),
            mcp_request(3, "ai.workcell/scm-discover", params.clone()),
        )
        .await,
    )
    .await;
    assert_eq!(unnegotiated["error"]["code"], -32601);
    let repository = final_sse_json(
        post_rpc(
            &client,
            &endpoint,
            Some(TOKEN),
            remote_request(4, "ai.workcell/scm-discover", params),
        )
        .await,
    )
    .await;
    let repository_handle = repository["result"]["repository"]["handle"].clone();
    let repository_resource_id = repository["result"]["repository"]["resourceId"].clone();
    assert_eq!(repository["result"]["repository"]["root"], ".");

    tokio::fs::write(root.path().join("tracked.txt"), "prepared\n")
        .await
        .unwrap();
    let prepare = final_sse_json(
        post_rpc(
            &client,
            &endpoint,
            Some(TOKEN),
            remote_request(
                5,
                "ai.workcell/scm-prepare-mutation",
                json!({
                    "version":"v1",
                    "host":remote_host_binding(descriptor),
                    "cwdHandle":descriptor["cwd"]["handle"],
                    "repositoryHandle":repository_handle,
                    "mutation":{"kind":"stage","paths":["tracked.txt"]}
                }),
            ),
        )
        .await,
    )
    .await;
    assert_eq!(
        prepare["result"]["preview"]["entries"][0]["unstaged"],
        "modified"
    );
    assert_eq!(
        prepare["result"]["operation"]["intent"]["resources"][0]["resourceId"],
        repository_resource_id
    );
    assert_ne!(
        prepare["result"]["operation"]["intent"]["resources"][0]["resourceId"],
        repository_handle
    );
    let execute = remote_request(
        6,
        "ai.workcell/execute",
        json!({
            "version":"v1",
            "preparationId":prepare["result"]["operation"]["preparationId"],
            "invocationId":"scm-stage",
            "host":remote_host_binding(descriptor)
        }),
    );
    let completed =
        final_sse_json(post_rpc(&client, &endpoint, Some(TOKEN), execute.clone()).await).await;
    assert_eq!(completed["result"]["state"], "completed", "{completed}");
    assert_eq!(
        completed["result"]["outcome"]["result"]["structuredContent"]["mutation"]["kind"],
        "stage"
    );
    tokio::fs::write(root.path().join("tracked.txt"), "after duplicate\n")
        .await
        .unwrap();
    let duplicate = final_sse_json(post_rpc(&client, &endpoint, Some(TOKEN), execute).await).await;
    assert_eq!(duplicate["result"], completed["result"]);
    assert_eq!(git_text(root.path(), &["show", ":tracked.txt"]), "prepared");

    let ordinary = final_sse_json(
        post_rpc(
            &client,
            &endpoint,
            Some(TOKEN),
            mcp_request(
                7,
                "tools/call",
                json!({"name":"file_read","arguments":{"filePath":"tracked.txt"}}),
            ),
        )
        .await,
    )
    .await;
    assert_ne!(ordinary["result"]["isError"], true);
    assert_eq!(http.shutdown().await, ShutdownOutcome::Completed);
}

#[cfg(unix)]
#[tokio::test]
async fn authenticated_workspace_operations_bind_cursors_mutations_and_direct_exec() {
    let root = tempfile::tempdir().expect("temporary root");
    tokio::fs::write(root.path().join("a.txt"), "needle before\n")
        .await
        .unwrap();
    tokio::fs::write(root.path().join("b.txt"), "needle b\n")
        .await
        .unwrap();
    tokio::fs::create_dir(root.path().join("sub"))
        .await
        .unwrap();
    let policy = ShellPermissionPolicy::from_toml(
        "version = 1\ndefault = \"deny\"\nallow = [\"printf *\", \"sleep *\"]\n",
        false,
    )
    .unwrap();
    let server = WorkcellServer::configured(
        Some(root.path()),
        &[ToolGroup::Files, ToolGroup::Shell],
        ServerBehavior {
            expose_execution_environment: false,
            modern_only: true,
        },
        ToolConfiguration {
            allow_write: true,
            web: WebsearchExecutionConfiguration::unconfigured(),
            web_icons: false,
            proxy: ProxyConfiguration::direct(),
            shell_policy: policy,
            shell_output_filter: false,
            honor_gitignore: true,
            code: CodeConfiguration {
                worker: WorkerSource::Discover {
                    bundled_cache_root: None,
                },
                type_check: true,
            },
            max_transfer_bytes: workcell_mcp::cli::DEFAULT_MAX_TRANSFER_BYTES,
            snapshot_root: None,
            transfer_root: None,
            snapshot_exclusions: &[],
        },
    )
    .await
    .unwrap();
    let http = HttpServer::start(
        server,
        0,
        HttpConfiguration {
            bind_mode: HttpBindMode::Loopback,
            allowed_hosts: vec!["127.0.0.1".into()],
            authentication: Some(HttpAuthentication::new(TOKEN).unwrap()),
            remote_host: Some(
                RemoteHostConfiguration::new(
                    "server-a".into(),
                    "workspace-a".into(),
                    "generation-a".into(),
                    "project-a".into(),
                    "principal-a".into(),
                )
                .unwrap(),
            ),
        },
    )
    .await
    .unwrap();
    let endpoint = format!("http://{}/mcp", http.address());
    let client = Client::new();
    let discovery = final_sse_json(
        post_rpc(
            &client,
            &endpoint,
            Some(TOKEN),
            discover_request(
                1,
                json!({"extensions":{"ai.workcell/remote-host":{"versions":["v1"]}}}),
            ),
        )
        .await,
    )
    .await;
    let descriptor = &discovery["result"]["capabilities"]["extensions"]["ai.workcell/remote-host"];
    assert_eq!(
        descriptor["capabilities"]["workspaceMutation"]["atomicAcrossFiles"],
        false
    );
    assert_eq!(
        descriptor["capabilities"]["workspaceMutation"]["rollbackOnFailure"],
        true
    );
    let host = remote_host_binding(descriptor);
    let cwd = descriptor["cwd"]["handle"].clone();
    let list_params = json!({
        "version":"v1",
        "host":host,
        "cwdHandle":cwd,
        "path":".",
        "recursive":false,
        "pageSize":1,
        "cursor":null
    });
    let unauthenticated = post_rpc(
        &client,
        &endpoint,
        None,
        remote_request(2, "ai.workcell/list", list_params.clone()),
    )
    .await;
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
    let unrequested = final_sse_json(
        post_rpc(
            &client,
            &endpoint,
            Some(TOKEN),
            mcp_request(3, "ai.workcell/list", list_params.clone()),
        )
        .await,
    )
    .await;
    assert_eq!(unrequested["error"]["code"], -32601);
    let listed = final_sse_json(
        post_rpc(
            &client,
            &endpoint,
            Some(TOKEN),
            remote_request(4, "ai.workcell/list", list_params),
        )
        .await,
    )
    .await;
    assert_eq!(listed["result"]["entries"][0]["path"], "a.txt");
    assert!(listed["result"]["nextCursor"].is_string());

    let read = final_sse_json(
        post_rpc(
            &client,
            &endpoint,
            Some(TOKEN),
            remote_request(
                5,
                "ai.workcell/read-text",
                json!({
                    "version":"v1",
                    "host":remote_host_binding(descriptor),
                    "cwdHandle":cwd,
                    "path":"a.txt",
                    "range":null,
                    "maxBytes":1024
                }),
            ),
        )
        .await,
    )
    .await;
    assert_eq!(read["result"]["text"], "needle before\n");
    assert_eq!(read["result"]["startByte"], 0);
    assert_eq!(
        read["result"]["endByte"],
        read["result"]["text"].as_str().unwrap().len()
    );
    assert!(read["result"]["nextByteOffset"].is_null());
    assert_eq!(read["result"]["truncated"], false);
    let revision = read["result"]["revision"].clone();
    let prepared = final_sse_json(
        post_rpc(
            &client,
            &endpoint,
            Some(TOKEN),
            remote_request(
                6,
                "ai.workcell/prepare-mutation",
                json!({
                    "version":"v1",
                    "host":remote_host_binding(descriptor),
                    "cwdHandle":cwd,
                    "mutations":[
                        {"kind":"write","path":"a.txt","content":"after","expectedRevision":revision},
                        {"kind":"create","path":"created.txt","content":"created"}
                    ]
                }),
            ),
        )
        .await,
    )
    .await;
    assert_eq!(
        prepared["result"]["intent"]["resources"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        tokio::fs::read_to_string(root.path().join("a.txt"))
            .await
            .unwrap(),
        "needle before\n"
    );
    let mutation_execute = remote_request(
        7,
        "ai.workcell/execute",
        json!({
            "version":"v1",
            "preparationId":prepared["result"]["preparationId"],
            "invocationId":"workspace-mutation",
            "host":remote_host_binding(descriptor)
        }),
    );
    let completed =
        final_sse_json(post_rpc(&client, &endpoint, Some(TOKEN), mutation_execute.clone()).await)
            .await;
    assert_eq!(completed["result"]["state"], "completed", "{completed}");
    assert_eq!(
        completed["result"]["outcome"]["result"]["structuredContent"]["atomicAcrossFiles"],
        false
    );
    tokio::fs::write(root.path().join("a.txt"), "later")
        .await
        .unwrap();
    let duplicate =
        final_sse_json(post_rpc(&client, &endpoint, Some(TOKEN), mutation_execute).await).await;
    assert_eq!(duplicate["result"], completed["result"]);
    assert_eq!(
        tokio::fs::read_to_string(root.path().join("a.txt"))
            .await
            .unwrap(),
        "later"
    );

    let denied = final_sse_json(
        post_rpc(
            &client,
            &endpoint,
            Some(TOKEN),
            remote_request(
                8,
                "ai.workcell/prepare-exec",
                json!({
                    "version":"v1",
                    "host":remote_host_binding(descriptor),
                    "cwdHandle":cwd,
                    "options":{"command":"rm denied","timeoutMs":1000}
                }),
            ),
        )
        .await,
    )
    .await;
    assert_eq!(denied["error"]["data"]["code"], "invalid_request");

    let exec = final_sse_json(
        post_rpc(
            &client,
            &endpoint,
            Some(TOKEN),
            remote_request(
                9,
                "ai.workcell/prepare-exec",
                json!({
                    "version":"v1",
                    "host":remote_host_binding(descriptor),
                    "cwdHandle":cwd,
                    "options":{"command":"printf started; sleep 30","timeoutMs":60000}
                }),
            ),
        )
        .await,
    )
    .await;
    let preparation_id = exec["result"]["preparationId"].clone();
    let endpoint_for_execute = endpoint.clone();
    let client_for_execute = client.clone();
    let descriptor_for_execute = descriptor.clone();
    let preparation_for_execute = preparation_id.clone();
    let execution = tokio::spawn(async move {
        final_sse_json(
            post_rpc(
                &client_for_execute,
                &endpoint_for_execute,
                Some(TOKEN),
                remote_request(
                    10,
                    "ai.workcell/execute",
                    json!({
                        "version":"v1",
                        "preparationId":preparation_for_execute,
                        "invocationId":"direct-exec",
                        "host":remote_host_binding(&descriptor_for_execute)
                    }),
                ),
            )
            .await,
        )
        .await
    });
    let cancellation = async {
        loop {
            let status = final_sse_json(
                post_rpc(
                    &client,
                    &endpoint,
                    Some(TOKEN),
                    remote_request(
                        11,
                        "ai.workcell/status",
                        json!({
                            "version":"v1",
                            "preparationId":preparation_id,
                            "invocationId":"direct-exec",
                            "host":remote_host_binding(descriptor)
                        }),
                    ),
                )
                .await,
            )
            .await;
            if status["result"]["state"] == "running"
                && !status["result"]["progress"].as_array().unwrap().is_empty()
            {
                assert_eq!(status["result"]["progress"][0]["chunk"], "started");
                break;
            }
            tokio::task::yield_now().await;
        }
        final_sse_json(
            post_rpc(
                &client,
                &endpoint,
                Some(TOKEN),
                remote_request(
                    12,
                    "ai.workcell/cancel",
                    json!({
                        "version":"v1",
                        "preparationId":preparation_id,
                        "invocationId":"direct-exec",
                        "host":remote_host_binding(descriptor)
                    }),
                ),
            )
            .await,
        )
        .await
    };
    let cancelled = tokio::time::timeout(Duration::from_secs(5), cancellation)
        .await
        .expect("direct execution reached progress");
    assert_eq!(cancelled["result"]["cancellationRequested"], true);
    let terminal = execution.await.unwrap();
    assert_eq!(terminal["result"]["state"], "indeterminate", "{terminal}");
    let next = terminal["result"]["progressMetadata"]["nextSequence"]
        .as_u64()
        .unwrap();
    for after in [0, next - 1, next] {
        let replay = final_sse_json(
            post_rpc(
                &client,
                &endpoint,
                Some(TOKEN),
                remote_request(
                    13,
                    "ai.workcell/status",
                    json!({
                        "version": "v1",
                        "preparationId": preparation_id,
                        "invocationId": "direct-exec",
                        "host": remote_host_binding(descriptor),
                        "afterSequence": after
                    }),
                ),
            )
            .await,
        )
        .await;
        if after == next {
            assert!(replay.get("error").is_some(), "{replay}");
        } else {
            assert_eq!(
                replay["result"]["progressMetadata"],
                terminal["result"]["progressMetadata"]
            );
            let expected: Vec<_> = terminal["result"]["progress"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|event| event["sequence"].as_u64().unwrap() > after)
                .cloned()
                .collect();
            assert_eq!(replay["result"]["progress"], json!(expected));
        }
    }
    assert_eq!(
        terminal["result"]["outcome"]["sideEffectsPossible"], true,
        "{terminal}"
    );
    assert_eq!(http.shutdown().await, ShutdownOutcome::Completed);
}

#[tokio::test]
async fn authenticated_watch_replays_changes_and_assets_remain_allowlisted_bytes() {
    let root = tempfile::tempdir().expect("temporary root");
    tokio::fs::create_dir_all(root.path().join(".caudra/workflows"))
        .await
        .unwrap();
    for directory in [
        ".caudra/commands/nested",
        ".claude/commands",
        ".opencode/commands",
        ".agents/commands",
        ".config/opencode/commands",
    ] {
        tokio::fs::create_dir_all(root.path().join(directory))
            .await
            .unwrap();
    }
    tokio::fs::write(root.path().join("AGENTS.md"), "project instructions")
        .await
        .unwrap();
    tokio::fs::write(
        root.path().join(".caudra/workflows/review.rhai"),
        "untrusted workflow",
    )
    .await
    .unwrap();
    for (path, content) in [
        (".caudra/commands/review.md", "project command"),
        (".claude/commands/compat.md", "claude command"),
        (".opencode/commands/build.md", "opencode command"),
        (
            ".caudra/permissions.toml",
            "[shell]\ndeny = [\"rm *\"]\nallow = [\"cargo test\"]\n",
        ),
    ] {
        tokio::fs::write(root.path().join(path), content)
            .await
            .unwrap();
    }
    for excluded in [
        ".caudra/init.lua",
        ".caudra/mcp.toml",
        ".caudra/plugins.toml",
        ".caudra/config.toml",
        ".caudra/commands/run.sh",
        ".caudra/commands/nested/deep.md",
        ".caudra/workflows/unsafe.sh",
        ".agents/commands/not-project-compatible.md",
        ".config/opencode/commands/global-only.md",
        ".env",
    ] {
        tokio::fs::write(root.path().join(excluded), "excluded")
            .await
            .unwrap();
    }
    let server = WorkcellServer::configured(
        Some(root.path()),
        &[ToolGroup::Files],
        ServerBehavior {
            expose_execution_environment: false,
            modern_only: true,
        },
        ToolConfiguration {
            allow_write: true,
            web: WebsearchExecutionConfiguration::unconfigured(),
            web_icons: false,
            proxy: ProxyConfiguration::direct(),
            shell_policy: ShellPermissionPolicy::restricted(),
            shell_output_filter: true,
            honor_gitignore: true,
            code: CodeConfiguration {
                worker: WorkerSource::Discover {
                    bundled_cache_root: None,
                },
                type_check: true,
            },
            max_transfer_bytes: workcell_mcp::cli::DEFAULT_MAX_TRANSFER_BYTES,
            snapshot_root: None,
            transfer_root: None,
            snapshot_exclusions: &[],
        },
    )
    .await
    .unwrap();
    let http = HttpServer::start(
        server,
        0,
        HttpConfiguration {
            bind_mode: HttpBindMode::Loopback,
            allowed_hosts: vec!["127.0.0.1".into()],
            authentication: Some(HttpAuthentication::new(TOKEN).unwrap()),
            remote_host: Some(
                RemoteHostConfiguration::new(
                    "server-a".into(),
                    "workspace-a".into(),
                    "generation-a".into(),
                    "project-a".into(),
                    "principal-a".into(),
                )
                .unwrap(),
            ),
        },
    )
    .await
    .unwrap();
    let endpoint = format!("http://{}/mcp", http.address());
    let client = Client::new();
    let discovery = final_sse_json(
        post_rpc(
            &client,
            &endpoint,
            Some(TOKEN),
            discover_request(
                1,
                json!({"extensions":{"ai.workcell/remote-host":{"versions":["v1"]}}}),
            ),
        )
        .await,
    )
    .await;
    let descriptor = &discovery["result"]["capabilities"]["extensions"]["ai.workcell/remote-host"];
    let binding = json!({
        "version":"v1",
        "host":remote_host_binding(descriptor),
        "cwdHandle":descriptor["cwd"]["handle"]
    });
    let mut open_params = binding.clone();
    open_params["path"] = json!(".");
    open_params["recursive"] = json!(true);
    let unauthenticated = post_rpc(
        &client,
        &endpoint,
        None,
        remote_request(2, "ai.workcell/watch-open", open_params.clone()),
    )
    .await;
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
    let unnegotiated = final_sse_json(
        post_rpc(
            &client,
            &endpoint,
            Some(TOKEN),
            mcp_request(3, "ai.workcell/watch-open", open_params.clone()),
        )
        .await,
    )
    .await;
    assert_eq!(unnegotiated["error"]["code"], -32601);

    let unauthenticated_assets = post_rpc(
        &client,
        &endpoint,
        None,
        remote_request(4, "ai.workcell/discover-project-assets", binding.clone()),
    )
    .await;
    assert_eq!(unauthenticated_assets.status(), StatusCode::UNAUTHORIZED);
    let assets = final_sse_json(
        post_rpc(
            &client,
            &endpoint,
            Some(TOKEN),
            remote_request(5, "ai.workcell/discover-project-assets", binding.clone()),
        )
        .await,
    )
    .await;
    let asset_entries = assets["result"]["manifest"]["assets"].as_array().unwrap();
    assert_eq!(asset_entries.len(), 6);
    assert!(asset_entries.iter().all(|asset| {
        !matches!(
            asset["path"].as_str(),
            Some(
                ".env"
                    | ".caudra/init.lua"
                    | ".caudra/mcp.toml"
                    | ".caudra/plugins.toml"
                    | ".caudra/config.toml"
                    | ".caudra/commands/run.sh"
                    | ".caudra/commands/nested/deep.md"
                    | ".caudra/workflows/unsafe.sh"
                    | ".agents/commands/not-project-compatible.md"
                    | ".config/opencode/commands/global-only.md"
            )
        )
    }));
    assert_eq!(
        asset_entries
            .iter()
            .filter(|asset| asset["kind"] == "command" && asset["trust"] == "declarative")
            .count(),
        3
    );
    let workflow = asset_entries
        .iter()
        .find(|asset| asset["kind"] == "workflow")
        .unwrap();
    assert_eq!(workflow["trust"], "clientApprovalRequired");
    let permissions = asset_entries
        .iter()
        .find(|asset| asset["kind"] == "permissions")
        .unwrap();
    assert_eq!(permissions["trust"], "mixedReviewRequired");
    let mut read_params = binding.clone();
    read_params["path"] = permissions["path"].clone();
    read_params["expectedRevision"] = permissions["revision"].clone();
    read_params["maxBytes"] = json!(65_536);
    let unauthenticated_read = post_rpc(
        &client,
        &endpoint,
        None,
        remote_request(6, "ai.workcell/read-project-asset", read_params.clone()),
    )
    .await;
    assert_eq!(unauthenticated_read.status(), StatusCode::UNAUTHORIZED);
    let read = final_sse_json(
        post_rpc(
            &client,
            &endpoint,
            Some(TOKEN),
            remote_request(7, "ai.workcell/read-project-asset", read_params),
        )
        .await,
    )
    .await;
    assert_eq!(
        read["result"]["content"],
        "[shell]\ndeny = [\"rm *\"]\nallow = [\"cargo test\"]\n"
    );
    assert_eq!(read["result"]["encoding"], "utf8");

    let opened = final_sse_json(
        post_rpc(
            &client,
            &endpoint,
            Some(TOKEN),
            remote_request(8, "ai.workcell/watch-open", open_params),
        )
        .await,
    )
    .await;
    let subscription_id = opened["result"]["subscriptionId"].clone();
    let cursor = opened["result"]["cursor"].clone();
    tokio::fs::write(root.path().join("external.txt"), "one")
        .await
        .unwrap();
    tokio::fs::write(root.path().join("external.txt"), "two")
        .await
        .unwrap();
    tokio::fs::rename(
        root.path().join("external.txt"),
        root.path().join("renamed.txt"),
    )
    .await
    .unwrap();
    tokio::fs::remove_file(root.path().join("renamed.txt"))
        .await
        .unwrap();
    let ordinary = final_sse_json(
        post_rpc(
            &client,
            &endpoint,
            Some(TOKEN),
            mcp_request(
                9,
                "tools/call",
                json!({"name":"file_write","arguments":{"filePath":"mediated.txt","content":"server"}}),
            ),
        )
        .await,
    )
    .await;
    assert_ne!(ordinary["result"]["isError"], true);

    let observed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let mut poll = binding.clone();
            poll["subscriptionId"] = subscription_id.clone();
            poll["cursor"] = cursor.clone();
            poll["maxEvents"] = json!(128);
            poll["maxBytes"] = json!(262_144);
            poll["waitMs"] = json!(500);
            let response = final_sse_json(
                post_rpc(
                    &client,
                    &endpoint,
                    Some(TOKEN),
                    remote_request(10, "ai.workcell/watch-poll", poll),
                )
                .await,
            )
            .await;
            let events = response["result"]["events"].as_array().unwrap();
            let saw_external = events.iter().any(|event| {
                matches!(event["path"].as_str(), Some("external.txt" | "renamed.txt"))
            });
            let saw_remove = events.iter().any(|event| event["kind"] == "remove");
            let saw_mediated = events.iter().any(|event| event["path"] == "mediated.txt");
            if saw_external && saw_remove && saw_mediated {
                break response;
            }
        }
    })
    .await
    .expect("bounded HTTP watch wait");
    let events = observed["result"]["events"].as_array().unwrap();
    assert!(events.windows(2).all(|pair| {
        pair[0]["sequence"].as_u64().unwrap() < pair[1]["sequence"].as_u64().unwrap()
    }));
    assert!(events.iter().all(|event| {
        !event["path"]
            .as_str()
            .is_some_and(|path| path.starts_with('/'))
    }));

    let mut replay_params = binding.clone();
    replay_params["subscriptionId"] = subscription_id.clone();
    replay_params["cursor"] = cursor;
    replay_params["maxEvents"] = json!(128);
    replay_params["maxBytes"] = json!(262_144);
    replay_params["waitMs"] = json!(0);
    let replay = final_sse_json(
        post_rpc(
            &client,
            &endpoint,
            Some(TOKEN),
            remote_request(11, "ai.workcell/watch-poll", replay_params),
        )
        .await,
    )
    .await;
    let replayed = replay["result"]["events"].as_array().unwrap();
    assert!(replayed.len() >= events.len());
    assert_eq!(&replayed[..events.len()], events);

    let mut close_params = binding;
    close_params["subscriptionId"] = subscription_id;
    let closed = final_sse_json(
        post_rpc(
            &client,
            &endpoint,
            Some(TOKEN),
            remote_request(12, "ai.workcell/watch-close", close_params),
        )
        .await,
    )
    .await;
    assert_eq!(closed["result"]["closed"], true);
    assert_eq!(http.shutdown().await, ShutdownOutcome::Completed);
}

#[cfg(unix)]
#[tokio::test]
async fn http_streams_standard_shell_progress_before_the_result() {
    let (_root, server) = fixture_server_with_policy(ShellPermissionPolicy::yolo()).await;
    let http = HttpServer::start(
        server,
        0,
        HttpConfiguration {
            bind_mode: HttpBindMode::Loopback,
            allowed_hosts: vec!["127.0.0.1".into()],
            authentication: Some(HttpAuthentication::new(TOKEN).unwrap()),
            remote_host: None,
        },
    )
    .await
    .expect("HTTP server");
    let endpoint = format!("http://{}/mcp", http.address());
    let mut response = post_rpc(
        &Client::new(),
        &endpoint,
        Some(TOKEN),
        mcp_request(
            1,
            "tools/call",
            json!({
                "name": "shell",
                "arguments": {
                    "command": "printf http-live"
                },
                "_meta": {"progressToken": 17}
            }),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let mut buffer = Vec::new();
    let first = next_sse_json(&mut response, &mut buffer).await;
    let result = next_sse_json(&mut response, &mut buffer).await;

    assert_progress(&first, json!(17), 1, "stdout", "http-live");
    assert_eq!(result["id"], 1);
    assert_eq!(result["result"]["structuredContent"]["finalSequence"], 1);
    assert!(buffer.is_empty());
    assert!(
        tokio::time::timeout(Duration::from_secs(2), response.chunk())
            .await
            .expect("SSE completion timeout")
            .expect("SSE response body")
            .is_none()
    );
    assert_eq!(http.shutdown().await, ShutdownOutcome::Completed);
}

#[cfg(unix)]
#[tokio::test]
async fn http_supports_stateless_legacy_calls_and_progress() {
    let (_root, server) = fixture_server_with_policy(ShellPermissionPolicy::yolo()).await;
    let http = HttpServer::start(
        server,
        0,
        HttpConfiguration {
            bind_mode: HttpBindMode::Loopback,
            allowed_hosts: vec!["127.0.0.1".into()],
            authentication: Some(HttpAuthentication::new(TOKEN).unwrap()),
            remote_host: None,
        },
    )
    .await
    .expect("HTTP server");
    let endpoint = format!("http://{}/mcp", http.address());
    let client = Client::new();

    let initialized = post_legacy_rpc(
        &client,
        &endpoint,
        legacy_initialize_request(1, LEGACY_PROTOCOL_VERSION),
    )
    .await;
    assert_eq!(initialized.status(), StatusCode::OK);
    assert!(!initialized.headers().contains_key("mcp-session-id"));
    assert_eq!(
        final_sse_json(initialized).await["result"]["protocolVersion"],
        LEGACY_PROTOCOL_VERSION
    );

    let listed = post_legacy_rpc(
        &client,
        &endpoint,
        legacy_request(2, "tools/list", json!({})),
    )
    .await;
    assert!(!listed.headers().contains_key("mcp-session-id"));
    let listed = final_sse_json(listed).await;
    assert_eq!(listed["result"]["tools"].as_array().unwrap().len(), 13);
    assert!(listed["result"].get("ttlMs").is_none());
    assert!(listed["result"].get("cacheScope").is_none());

    let stale_session = client
        .post(&endpoint)
        .bearer_auth(TOKEN)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, ACCEPT)
        .header("MCP-Protocol-Version", LEGACY_PROTOCOL_VERSION)
        .header("Mcp-Session-Id", "stale-session")
        .json(&legacy_request(4, "tools/list", json!({})))
        .send()
        .await
        .unwrap();
    assert_eq!(stale_session.status(), StatusCode::BAD_REQUEST);

    let mut response = post_legacy_rpc(
        &client,
        &endpoint,
        legacy_request(
            3,
            "tools/call",
            json!({
                "name": "shell",
                "arguments": {"command": "printf legacy-http"},
                "_meta": {"progressToken": 23}
            }),
        ),
    )
    .await;
    let mut buffer = Vec::new();
    let progress = next_sse_json(&mut response, &mut buffer).await;
    let result = next_sse_json(&mut response, &mut buffer).await;
    assert_progress(&progress, json!(23), 1, "stdout", "legacy-http");
    assert_eq!(result["id"], 3);
    assert!(buffer.is_empty());
    assert!(
        tokio::time::timeout(Duration::from_secs(2), response.chunk())
            .await
            .expect("SSE completion timeout")
            .expect("SSE response body")
            .is_none()
    );

    assert_eq!(http.shutdown().await, ShutdownOutcome::Completed);
}

#[tokio::test]
async fn http_modern_only_rejects_legacy_and_advertises_only_modern() {
    let (_root, server) =
        fixture_server_with_options(ShellPermissionPolicy::restricted(), true).await;
    let http = HttpServer::start(
        server,
        0,
        HttpConfiguration {
            bind_mode: HttpBindMode::Loopback,
            allowed_hosts: vec!["127.0.0.1".into()],
            authentication: Some(HttpAuthentication::new(TOKEN).unwrap()),
            remote_host: None,
        },
    )
    .await
    .expect("HTTP server");
    let endpoint = format!("http://{}/mcp", http.address());
    let client = Client::new();

    let discovered = post_rpc(
        &client,
        &endpoint,
        Some(TOKEN),
        discover_request(1, json!({})),
    )
    .await;
    assert_eq!(
        final_sse_json(discovered).await["result"]["supportedVersions"],
        json!([PROTOCOL_VERSION])
    );

    let rejected = post_legacy_rpc(
        &client,
        &endpoint,
        legacy_initialize_request(2, LEGACY_PROTOCOL_VERSION),
    )
    .await;
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    let rejected = final_sse_json(rejected).await;
    assert_eq!(rejected["error"]["code"], -32_022);
    assert_eq!(
        rejected["error"]["data"]["requested"],
        LEGACY_PROTOCOL_VERSION
    );
    assert_eq!(
        rejected["error"]["data"]["supported"],
        json!([PROTOCOL_VERSION])
    );

    for (id, protocol_version) in [(3, LEGACY_PROTOCOL_VERSION), (4, "2025-06-18")] {
        let rejected = post_raw_rpc(
            &client,
            &endpoint,
            id,
            "tools/list",
            json!({}),
            Some(protocol_version),
        )
        .await;
        assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            rejected.json::<Value>().await.unwrap()["error"]["code"],
            -32_022
        );
    }

    let missing_meta = post_raw_rpc(
        &client,
        &endpoint,
        5,
        "tools/list",
        json!({}),
        Some(PROTOCOL_VERSION),
    )
    .await;
    assert_eq!(missing_meta.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        missing_meta.json::<Value>().await.unwrap()["error"]["code"],
        -32_602
    );

    assert_eq!(http.shutdown().await, ShutdownOutcome::Completed);
}

#[tokio::test]
async fn http_legacy_fallback_rejects_older_unknown_and_era_mismatched_versions() {
    let (_root, server) = fixture_server().await;
    let http = HttpServer::start(
        server,
        0,
        HttpConfiguration {
            bind_mode: HttpBindMode::Loopback,
            allowed_hosts: vec!["127.0.0.1".into()],
            authentication: Some(HttpAuthentication::new(TOKEN).unwrap()),
            remote_host: None,
        },
    )
    .await
    .expect("HTTP server");
    let endpoint = format!("http://{}/mcp", http.address());
    let client = Client::new();

    for (id, version) in [(1, "2025-06-18"), (2, "2099-01-01")] {
        let rejected =
            post_legacy_rpc(&client, &endpoint, legacy_initialize_request(id, version)).await;
        assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
        let rejected = rejected.json::<Value>().await.unwrap();
        assert_eq!(rejected["error"]["code"], -32_022);
        assert_eq!(rejected["error"]["data"]["requested"], version);
        assert_eq!(
            rejected["error"]["data"]["supported"],
            supported_dual_versions()
        );
    }

    let wrong_lifecycle = post_legacy_rpc(
        &client,
        &endpoint,
        legacy_initialize_request(3, PROTOCOL_VERSION),
    )
    .await;
    assert_eq!(wrong_lifecycle.status(), StatusCode::BAD_REQUEST);
    let wrong_lifecycle = wrong_lifecycle.json::<Value>().await.unwrap();
    assert_eq!(wrong_lifecycle["error"]["code"], -32_600);
    assert!(
        wrong_lifecycle["error"]["message"]
            .as_str()
            .unwrap()
            .contains("server/discover")
    );

    let rejected_call = post_raw_rpc(
        &client,
        &endpoint,
        4,
        "tools/list",
        json!({}),
        Some("2099-01-01"),
    )
    .await;
    assert_eq!(rejected_call.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        rejected_call.json::<Value>().await.unwrap()["error"]["code"],
        -32_022
    );

    assert_eq!(http.shutdown().await, ShutdownOutcome::Completed);
}

#[tokio::test]
async fn http_discovery_rejects_missing_and_legacy_request_context() {
    let (_root, server) = fixture_server().await;
    let http = HttpServer::start(
        server,
        0,
        HttpConfiguration {
            bind_mode: HttpBindMode::Loopback,
            allowed_hosts: vec!["127.0.0.1".into()],
            authentication: Some(HttpAuthentication::new(TOKEN).unwrap()),
            remote_host: None,
        },
    )
    .await
    .expect("HTTP server");
    let endpoint = format!("http://{}/mcp", http.address());
    let client = Client::new();

    let missing = post_raw_rpc(
        &client,
        &endpoint,
        1,
        "server/discover",
        json!({}),
        Some(PROTOCOL_VERSION),
    )
    .await;
    assert_eq!(missing.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        missing.json::<Value>().await.unwrap()["error"]["code"],
        -32_602
    );

    let mut meta = request_meta(json!({}));
    meta["io.modelcontextprotocol/protocolVersion"] = json!(LEGACY_PROTOCOL_VERSION);
    let legacy = post_raw_rpc(
        &client,
        &endpoint,
        2,
        "server/discover",
        json!({"_meta":meta}),
        Some(LEGACY_PROTOCOL_VERSION),
    )
    .await;
    assert_eq!(legacy.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        legacy.json::<Value>().await.unwrap()["error"]["code"],
        -32_022
    );

    assert_eq!(http.shutdown().await, ShutdownOutcome::Completed);
}

async fn post_rpc(client: &Client, endpoint: &str, token: Option<&str>, body: Value) -> Response {
    let method = body["method"].as_str().unwrap();
    let mut request = client
        .post(endpoint)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, ACCEPT)
        .header("MCP-Protocol-Version", PROTOCOL_VERSION)
        .header("Mcp-Method", method);
    if let Some(name) = body["params"]["name"].as_str() {
        request = request.header("Mcp-Name", name);
    }
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    request.json(&body).send().await.expect("RPC request")
}

async fn post_legacy_rpc(client: &Client, endpoint: &str, body: Value) -> Response {
    let mut request = client
        .post(endpoint)
        .bearer_auth(TOKEN)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, ACCEPT);
    if body["method"] != "initialize" {
        request = request.header("MCP-Protocol-Version", LEGACY_PROTOCOL_VERSION);
    }
    request
        .json(&body)
        .send()
        .await
        .expect("legacy RPC request")
}

async fn post_raw_rpc(
    client: &Client,
    endpoint: &str,
    id: u64,
    method: &str,
    params: Value,
    protocol_version: Option<&str>,
) -> Response {
    let mut request = client
        .post(endpoint)
        .bearer_auth(TOKEN)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, ACCEPT);
    if let Some(protocol_version) = protocol_version {
        request = request.header("MCP-Protocol-Version", protocol_version);
    }
    request
        .json(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
        .send()
        .await
        .expect("raw RPC request")
}

async fn final_sse_json(response: Response) -> Value {
    let content_type = response.headers()[header::CONTENT_TYPE].clone();
    let body = response.text().await.unwrap();
    if content_type
        .to_str()
        .is_ok_and(|value| value.split(';').next() == Some("application/json"))
    {
        return serde_json::from_str(&body).expect("JSON-RPC response");
    }
    body.split("\n\n")
        .filter_map(|event| event.lines().find_map(|line| line.strip_prefix("data: ")))
        .filter_map(|data| serde_json::from_str(data).ok())
        .last()
        .expect("SSE JSON-RPC response")
}

fn git(path: &std::path::Path, arguments: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git command failed: {arguments:?}");
}

fn git_text(path: &std::path::Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(arguments)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success(), "git command failed: {arguments:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

async fn next_sse_json(response: &mut Response, buffer: &mut Vec<u8>) -> Value {
    assert_eq!(
        response.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .split(';')
            .next(),
        Some("text/event-stream")
    );
    loop {
        let boundary = buffer
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|end| (end, 4))
            .or_else(|| {
                buffer
                    .windows(2)
                    .position(|window| window == b"\n\n")
                    .map(|end| (end, 2))
            });
        if let Some((end, separator_bytes)) = boundary {
            let event = buffer.drain(..end + separator_bytes).collect::<Vec<_>>();
            let event = std::str::from_utf8(&event).expect("UTF-8 SSE event");
            if let Some(data) = event.lines().find_map(|line| {
                line.strip_prefix("data:")
                    .map(|data| data.strip_prefix(' ').unwrap_or(data))
            }) {
                return serde_json::from_str(data).expect("SSE JSON-RPC message");
            }
        }
        let chunk = tokio::time::timeout(Duration::from_secs(2), response.chunk())
            .await
            .expect("SSE message timeout")
            .expect("SSE response body")
            .expect("SSE stream ended before the final response");
        buffer.extend_from_slice(&chunk);
    }
}

fn assert_progress(
    message: &Value,
    expected_token: Value,
    expected_sequence: u64,
    expected_stream: &str,
    expected_text: &str,
) {
    assert_eq!(message["method"], "notifications/progress");
    assert_eq!(message["params"]["progressToken"], expected_token);
    assert_eq!(message["params"]["progress"], expected_sequence as f64);
    assert_eq!(
        message["params"]["message"],
        format!("[{expected_stream}] {expected_text}")
    );
    assert_eq!(
        message["params"]["_meta"]["ai.workcell/tool-output-chunk"],
        json!({
            "version": 1,
            "sequence": expected_sequence,
            "stream": expected_stream,
            "text": expected_text
        })
    );
}

fn assert_environment_descriptor(descriptor: &Value) {
    assert_eq!(descriptor["version"], "v1");
    assert!(descriptor["os"]["systemPackageManager"]["name"].is_string());
    assert!(descriptor["os"]["systemPackageManager"]["available"].is_boolean());
    assert!(
        descriptor["execution"]["privilege"]["effectiveRoot"].is_boolean()
            || descriptor["execution"]["privilege"]["effectiveRoot"].is_null()
    );
    assert!(matches!(
        descriptor["execution"]["privilege"]["nonInteractiveSudo"].as_str(),
        Some(
            "available" | "unavailable" | "not-found" | "not-needed" | "not-applicable" | "unknown"
        )
    ));
}

fn request_meta(capabilities: Value) -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": PROTOCOL_VERSION,
        "io.modelcontextprotocol/clientCapabilities": capabilities,
        "io.modelcontextprotocol/clientInfo": {"name":"workcell-test","version":"1"}
    })
}

fn discover_request(id: u64, capabilities: Value) -> Value {
    json!({
        "jsonrpc":"2.0",
        "id":id,
        "method":"server/discover",
        "params":{"_meta":request_meta(capabilities)}
    })
}

fn legacy_initialize_request(id: u64, protocol_version: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": protocol_version,
            "capabilities": {},
            "clientInfo": {"name": "workcell-legacy-test", "version": "1"}
        }
    })
}

fn legacy_request(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
}

fn supported_dual_versions() -> Value {
    json!(["2026-07-28", "2025-11-25"])
}

fn mcp_request(id: u64, method: &str, mut params: Value) -> Value {
    let required = request_meta(json!({}));
    if !params["_meta"].is_object() {
        params["_meta"] = json!({});
    }
    params["_meta"]
        .as_object_mut()
        .unwrap()
        .extend(required.as_object().unwrap().clone());
    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
}

fn remote_request(id: u64, method: &str, params: Value) -> Value {
    let mut request = mcp_request(id, method, params);
    request["params"]["_meta"]["ai.workcell/remote-host"] = json!({"versions":["v1"]});
    request["params"]["_meta"]["io.modelcontextprotocol/clientCapabilities"]["extensions"]["ai.workcell/remote-host"] =
        json!({"versions":["v1"]});
    request
}

fn remote_host_binding(descriptor: &Value) -> Value {
    json!({
        "serverId":descriptor["serverId"],
        "instanceId":descriptor["instanceId"],
        "workspaceId":descriptor["workspaceId"],
        "workspaceGeneration":descriptor["workspaceGeneration"],
        "rootProjectId":descriptor["rootProjectId"],
        "principalId":descriptor["principalId"],
        "cwdHandle":descriptor["cwd"]["handle"],
        "catalogRevision":descriptor["revisions"]["catalog"],
        "policyRevision":descriptor["revisions"]["policy"]
    })
}

async fn write_json<W>(writer: &mut W, value: &Value)
where
    W: tokio::io::AsyncWrite + Unpin,
{
    writer
        .write_all(format!("{value}\n").as_bytes())
        .await
        .expect("write JSON-RPC frame");
    writer.flush().await.expect("flush JSON-RPC frame");
}

async fn read_json<R>(reader: &mut BufReader<R>) -> Value
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .expect("response timeout")
        .expect("read response");
    serde_json::from_str(&line).expect("JSON-RPC response")
}

/// Locates the `monty` worker the way the server does, plus the in-repo build location so a
/// developer who ran `make code-worker` needs no extra configuration.
fn code_worker() -> Option<std::path::PathBuf> {
    if let Some(configured) = std::env::var_os("WORKCELL_MCP_CODE_WORKER") {
        return Some(configured.into());
    }
    let installed = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/code-worker/bin")
        .join(workcell_mcp_code::WORKER_FILE_NAME);
    installed.is_file().then_some(installed)
}

/// Exercises the full catalog, including the code group, over a real stdio session.
///
/// The code group needs the separately built worker binary, so this skips with an explicit message
/// rather than silently passing when it is absent. CI builds the worker and sets the environment
/// variable, so the skip only applies to a local checkout that has not run `make code-worker`.
#[cfg(unix)]
#[tokio::test]
async fn stdio_serves_the_full_catalog_including_python_execution() {
    let Some(worker) = code_worker() else {
        eprintln!(
            "skipping: no `monty` worker found. Run `make code-worker` or set WORKCELL_MCP_CODE_WORKER."
        );
        return;
    };
    let root = tempfile::tempdir().expect("temporary root");
    let server = WorkcellServer::configured(
        Some(root.path()),
        &[
            ToolGroup::Files,
            ToolGroup::Web,
            ToolGroup::Shell,
            ToolGroup::PythonExecution,
        ],
        ServerBehavior {
            expose_execution_environment: true,
            modern_only: false,
        },
        ToolConfiguration {
            // The full catalog includes the mutation tools, which are exposed
            // only when the process actually holds write authority.
            allow_write: true,
            web: WebsearchExecutionConfiguration::unconfigured(),
            web_icons: false,
            proxy: ProxyConfiguration::direct(),
            shell_policy: ShellPermissionPolicy::restricted(),
            shell_output_filter: true,
            honor_gitignore: true,
            code: CodeConfiguration {
                worker: WorkerSource::Path(&worker),
                type_check: true,
            },
            max_transfer_bytes: workcell_mcp::cli::DEFAULT_MAX_TRANSFER_BYTES,
            snapshot_root: None,
            transfer_root: None,
            snapshot_exclusions: &[],
        },
    )
    .await
    .expect("server with a code worker");

    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(async move {
        server
            .serve(server_transport)
            .await
            .expect("start MCP service")
            .waiting()
            .await
            .expect("MCP service")
    });
    let (read, mut write) = tokio::io::split(client_transport);
    let mut read = BufReader::new(read);

    write_json(&mut write, &discover_request(1, json!({}))).await;
    let _ = read_json(&mut read).await;

    write_json(&mut write, &mcp_request(2, "tools/list", json!({}))).await;
    let listed = read_json(&mut read).await;
    let names = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    // Catalog order is a compatibility contract; `python_execution` sits after `shell`.
    assert_eq!(
        names,
        [
            "file_read",
            "file_glob",
            "file_grep",
            "file_write",
            "file_edit",
            "file_apply_patch",
            "file_index",
            "websearch",
            "webfetch",
            "shell",
            "python_execution",
            "execution_environment",
        ]
    );

    write_json(
        &mut write,
        &mcp_request(
            3,
            "tools/call",
            json!({"name": "python_execution", "arguments": {"code": "sum([1, 2, 3, 4])"}}),
        ),
    )
    .await;
    let called = read_json(&mut read).await;
    let structured = &called["result"]["structuredContent"];
    assert_eq!(structured["outcome"], "completed");
    assert_eq!(structured["result"], json!(10));
    assert_eq!(structured["version"], json!(1));

    // The tool must also be discoverable as isolated, which is what lets a client skip a prompt.
    let code_tool = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "python_execution")
        .expect("code tool listed");
    assert_eq!(code_tool["annotations"]["readOnlyHint"], json!(true));
    assert_eq!(code_tool["annotations"]["openWorldHint"], json!(false));

    drop(write);
    let _ = tokio::time::timeout(Duration::from_secs(5), server_task).await;
}

/// Write authority is immutable process configuration, so it decides the shape
/// of the catalog rather than being negotiated per call.
#[tokio::test]
async fn write_authority_decides_whether_mutation_tools_exist_at_all() {
    async fn files_catalog(allow_write: bool) -> (TempDir, Vec<String>) {
        let root = tempfile::tempdir().expect("temporary root");
        let server = WorkcellServer::configured(
            Some(root.path()),
            &[ToolGroup::Files],
            ServerBehavior {
                expose_execution_environment: false,
                modern_only: false,
            },
            ToolConfiguration {
                allow_write,
                web: WebsearchExecutionConfiguration::unconfigured(),
                web_icons: false,
                proxy: ProxyConfiguration::direct(),
                shell_policy: ShellPermissionPolicy::restricted(),
                shell_output_filter: true,
                honor_gitignore: true,
                code: CodeConfiguration {
                    worker: WorkerSource::Discover {
                        bundled_cache_root: None,
                    },
                    type_check: true,
                },
                max_transfer_bytes: workcell_mcp::cli::DEFAULT_MAX_TRANSFER_BYTES,
                snapshot_root: None,
                transfer_root: None,
                snapshot_exclusions: &[],
            },
        )
        .await
        .expect("files server");
        let names = server
            .catalog()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        (root, names)
    }

    let (_read_only_root, read_only) = files_catalog(false).await;
    assert_eq!(
        read_only,
        ["file_read", "file_glob", "file_grep", "file_index"]
    );

    let (_writable_root, writable) = files_catalog(true).await;
    assert_eq!(
        writable,
        [
            "file_read",
            "file_glob",
            "file_grep",
            "file_write",
            "file_edit",
            "file_apply_patch",
            "file_index",
        ]
    );
}

/// A server built without the transfer group must answer `/files` exactly as it answers any other
/// unknown path, so the response does not disclose that the route exists elsewhere.
#[tokio::test]
async fn the_files_route_is_absent_when_the_transfer_group_is_not_enabled() {
    let (_root, server) = fixture_server().await;
    let http = HttpServer::start(
        server,
        0,
        HttpConfiguration {
            bind_mode: HttpBindMode::Loopback,
            allowed_hosts: vec!["127.0.0.1".into()],
            authentication: Some(HttpAuthentication::new(TOKEN).unwrap()),
            remote_host: None,
        },
    )
    .await
    .expect("HTTP server");
    let client = Client::new();
    let absent = client
        .get(format!("http://{}/files?path=visible.txt", http.address()))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    let unknown = client
        .get(format!("http://{}/absent", http.address()))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(absent.status(), StatusCode::NOT_FOUND);
    assert_eq!(absent.status(), unknown.status());
    assert_eq!(absent.text().await.unwrap(), unknown.text().await.unwrap());
    assert_eq!(http.shutdown().await, ShutdownOutcome::Completed);
}

#[cfg(unix)]
async fn transfer_server(root: &Path, private: Option<&Path>, allow_write: bool) -> WorkcellServer {
    WorkcellServer::configured(
        Some(root),
        &[ToolGroup::Files, ToolGroup::Transfer],
        ServerBehavior {
            expose_execution_environment: false,
            modern_only: true,
        },
        ToolConfiguration {
            allow_write,
            web: WebsearchExecutionConfiguration::unconfigured(),
            web_icons: false,
            proxy: ProxyConfiguration::direct(),
            shell_policy: ShellPermissionPolicy::restricted(),
            shell_output_filter: true,
            honor_gitignore: true,
            code: CodeConfiguration {
                worker: WorkerSource::Discover {
                    bundled_cache_root: None,
                },
                type_check: true,
            },
            max_transfer_bytes: workcell_mcp::cli::DEFAULT_MAX_TRANSFER_BYTES,
            snapshot_root: None,
            transfer_root: private,
            snapshot_exclusions: &[],
        },
    )
    .await
    .unwrap()
}

#[cfg(unix)]
fn transfer_http_configuration() -> HttpConfiguration {
    HttpConfiguration {
        bind_mode: HttpBindMode::Loopback,
        allowed_hosts: vec!["127.0.0.1".into()],
        authentication: Some(HttpAuthentication::new(TOKEN).unwrap()),
        remote_host: Some(
            RemoteHostConfiguration::new(
                "transfer-server".into(),
                "transfer-workspace".into(),
                "transfer-generation".into(),
                "transfer-project".into(),
                "transfer-principal".into(),
            )
            .unwrap(),
        ),
    }
}

#[cfg(unix)]
async fn reviewed_transfer_server(root: &Path, private: &Path) -> HttpServer {
    HttpServer::start(
        transfer_server(root, Some(private), true).await,
        0,
        transfer_http_configuration(),
    )
    .await
    .unwrap()
}

#[cfg(unix)]
async fn reviewed_rpc(client: &Client, endpoint: &str, method: &str, params: Value) -> Value {
    final_sse_json(
        post_rpc(
            client,
            endpoint,
            Some(TOKEN),
            remote_request(1, method, params),
        )
        .await,
    )
    .await
}

#[cfg(unix)]
async fn reviewed_discovery(client: &Client, endpoint: &str) -> Value {
    let result = final_sse_json(
        post_rpc(
            client,
            endpoint,
            Some(TOKEN),
            discover_request(
                1,
                json!({"extensions":{"ai.workcell/remote-host":{"versions":["v1"]}}}),
            ),
        )
        .await,
    )
    .await;
    result["result"]["capabilities"]["extensions"]["ai.workcell/remote-host"].clone()
}

#[cfg(unix)]
#[tokio::test]
async fn transfer_capabilities_and_routes_require_authenticated_rooted_private_storage() {
    for (authenticated, remote, storage, writable) in [
        (false, false, false, true),
        (true, false, false, true),
        (true, true, false, true),
        (true, false, true, true),
        (false, true, true, true),
        (true, true, true, false),
    ] {
        let root = tempfile::tempdir().unwrap();
        let private = tempfile::tempdir().unwrap();
        std::fs::set_permissions(private.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let server =
            transfer_server(root.path(), storage.then_some(private.path()), writable).await;
        let mut configuration = transfer_http_configuration();
        if !authenticated {
            configuration.authentication = None;
        }
        if !remote {
            configuration.remote_host = None;
        }
        let started = HttpServer::start(server, 0, configuration).await;
        if remote && (!authenticated || !writable) {
            assert!(started.is_err());
        } else {
            let http = started.unwrap();
            let client = Client::new();
            let origin = format!("http://{}", http.address());
            let endpoint = format!("{origin}/mcp");
            if remote {
                let descriptor = reviewed_discovery(&client, &endpoint).await;
                assert!(descriptor["capabilities"].is_object(), "{descriptor}");
                assert!(descriptor["capabilities"].get("reviewedTransfer").is_none());
                assert!(descriptor["capabilities"].get("fileTransfer").is_none());
            }
            for method in [
                "stage",
                "seal",
                "release",
                "stat",
                "download",
                "preparePublication",
                "publicationStatus",
                "inventory",
            ] {
                let refused = final_sse_json(
                    post_rpc(
                        &client,
                        &endpoint,
                        authenticated.then_some(TOKEN),
                        remote_request(1, &format!("ai.workcell/transfer/{method}"), json!({})),
                    )
                    .await,
                )
                .await;
                assert_eq!(refused["error"]["code"], -32601, "{refused}");
            }
            for query in ["?path=missing/file.bin", "?reviewed=v1&stage=unknown"] {
                let refused = client
                    .post(format!("{origin}/files{query}"))
                    .bearer_auth(TOKEN)
                    .body("no effect")
                    .send()
                    .await
                    .unwrap();
                assert_eq!(refused.status(), StatusCode::NOT_FOUND);
            }
            let tools = final_sse_json(
                post_rpc(
                    &client,
                    &endpoint,
                    authenticated.then_some(TOKEN),
                    mcp_request(1, "tools/list", json!({})),
                )
                .await,
            )
            .await;
            assert!(
                tools["result"]["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|tool| tool["name"] == "file_read")
            );
            assert_eq!(http.shutdown().await, ShutdownOutcome::Completed);
        }
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(private.path()).unwrap().count(), 0);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn raw_transfer_routes_are_refused_without_workspace_or_private_store_effects() {
    const ORIGINAL: &[u8] = b"unchanged binary\0\xff";
    let root = tempfile::tempdir().unwrap();
    let private = tempfile::tempdir().unwrap();
    std::fs::set_permissions(private.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(root.path().join("existing.bin"), ORIGINAL).unwrap();
    let http = reviewed_transfer_server(root.path(), private.path()).await;
    let origin = format!("http://{}", http.address());
    let client = Client::new();
    let store = std::fs::read_dir(private.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let before = std::fs::read_dir(&store)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    for query in [
        "?path=existing.bin",
        "?path=missing/received.bin",
        "?path=existing.bin&unknown=value",
        "",
        "?unknown=value",
        "?reviewed=v0&stage=unknown",
        "?reviewed=v1&stage=unknown&path=missing/received.bin",
        "?reviewed=v1&download=unknown&path=existing.bin",
        "?reviewed=v1&stage=unknown&stage=duplicate",
        "?stage=unknown",
        "?download=unknown",
    ] {
        for method in [reqwest::Method::GET, reqwest::Method::POST] {
            let response = client
                .request(method.clone(), format!("{origin}/files{query}"))
                .bearer_auth(TOKEN)
                .header(header::CONTENT_TYPE, "application/octet-stream")
                .body("must not publish")
                .send()
                .await
                .unwrap();
            assert_eq!(
                std::fs::read(root.path().join("existing.bin")).unwrap(),
                ORIGINAL
            );
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
            assert_eq!(
                std::fs::read_dir(&store)
                    .unwrap()
                    .map(|entry| entry.unwrap().file_name())
                    .collect::<Vec<_>>(),
                before
            );
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "{method} {query}"
            );
        }
    }
    assert_eq!(http.shutdown().await, ShutdownOutcome::Completed);
}

#[cfg(unix)]
#[tokio::test]
async fn raw_transfer_tool_names_are_absent_and_undispatchable_while_plain_mcp_files_work() {
    let root = tempfile::tempdir().unwrap();
    let private = tempfile::tempdir().unwrap();
    std::fs::set_permissions(private.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(root.path().join("existing.txt"), "ordinary text").unwrap();
    let http = reviewed_transfer_server(root.path(), private.path()).await;
    let endpoint = format!("http://{}/mcp", http.address());
    let client = Client::new();
    let descriptor = reviewed_discovery(&client, &endpoint).await;
    let listed = final_sse_json(
        post_rpc(
            &client,
            &endpoint,
            Some(TOKEN),
            mcp_request(1, "tools/list", json!({})),
        )
        .await,
    )
    .await;
    for (name, contract) in [
        ("file_upload", "transfer.upload.v1"),
        ("file_download", "transfer.download.v1"),
    ] {
        let result = final_sse_json(
            post_rpc(
                &client,
                &endpoint,
                Some(TOKEN),
                mcp_request(
                    1,
                    "tools/call",
                    json!({"name":name,"arguments":{"path":"missing/received.bin"}}),
                ),
            )
            .await,
        )
        .await;
        assert!(result["error"].is_object(), "{result}");
        let prepared = reviewed_rpc(&client, &endpoint, "ai.workcell/prepare", json!({"version":"v1","host":remote_host_binding(&descriptor),"tool":name,"contract":{"id":contract,"version":"v1","resultVersion":"v1"},"arguments":{"path":"missing/received.bin"}})).await;
        assert!(prepared["error"].is_object(), "{prepared}");
        assert!(
            !listed["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == name)
        );
    }
    assert!(descriptor["capabilities"].get("fileTransfer").is_none());
    assert!(!root.path().join("missing").exists());
    let result = final_sse_json(
        post_rpc(
            &client,
            &endpoint,
            Some(TOKEN),
            mcp_request(
                1,
                "tools/call",
                json!({"name":"file_read","arguments":{"filePath":"existing.txt"}}),
            ),
        )
        .await,
    )
    .await;
    assert!(result.get("error").is_none(), "{result}");
    assert_ne!(result["result"]["isError"], true, "{result}");
    assert!(result["result"].to_string().contains("ordinary text"));
    let result = final_sse_json(post_rpc(&client, &endpoint, Some(TOKEN), mcp_request(1, "tools/call", json!({"name":"file_write","arguments":{"filePath":"ordinary.txt","content":"ordinary write"}}))).await).await;
    assert!(result.get("error").is_none(), "{result}");
    assert_eq!(
        std::fs::read_to_string(root.path().join("ordinary.txt")).unwrap(),
        "ordinary write"
    );
    assert_eq!(http.shutdown().await, ShutdownOutcome::Completed);
}

#[cfg(unix)]
#[tokio::test]
async fn reviewed_binary_transfer_uses_authenticated_bytes_exact_ledger_execution_and_durable_recovery()
 {
    const LARGE_BYTES: usize = 6 * 1024 * 1024;
    const CWD: &str = "x-workcell-cwd";
    let root = tempfile::tempdir().unwrap();
    let private = tempfile::tempdir().unwrap();
    std::fs::set_permissions(private.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let http = reviewed_transfer_server(root.path(), private.path()).await;
    let origin = format!("http://{}", http.address());
    let endpoint = format!("{origin}/mcp");
    let client = Client::new();
    let descriptor = reviewed_discovery(&client, &endpoint).await;
    assert_eq!(
        descriptor["capabilities"]["reviewedTransfer"]["version"],
        "v1"
    );
    assert_eq!(
        descriptor["capabilities"]["reviewedTransfer"]["atomicReplaceAgainstExternalWriters"],
        false
    );
    let binding = json!({"version":"v1", "host":remote_host_binding(&descriptor), "cwdHandle":descriptor["cwd"]["handle"]});
    let scoped = |extra: Value| {
        let mut params = binding.clone();
        params
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        params
    };
    let bytes = vec![0xff; LARGE_BYTES];
    let mut digest = String::from("sha256:");
    for byte in Sha256::digest(&bytes) {
        write!(digest, "{byte:02x}").unwrap();
    }
    let staged = reviewed_rpc(
        &client,
        &endpoint,
        "ai.workcell/transfer/stage",
        scoped(json!({"sizeBytes":LARGE_BYTES,"digest":digest})),
    )
    .await;
    let staged: workcell_host_contract::TransferStageResponse =
        serde_json::from_value(staged["result"].clone()).unwrap();
    let prepare_params = scoped(
        json!({"stageId":staged.stage_id,"digest":digest,"sizeBytes":LARGE_BYTES,"publicationId":"binary-publication","path":"binary.bin","createDirectories":[],"precondition":{"kind":"mustNotExist"},"mode":"regular"}),
    );
    let premature = reviewed_rpc(
        &client,
        &endpoint,
        "ai.workcell/transfer/preparePublication",
        prepare_params.clone(),
    )
    .await;
    assert_eq!(premature["error"]["data"]["code"], "transferInvalidState");
    let upload_url = format!("{origin}{}", staged.upload_path);
    assert_eq!(
        client
            .post(&upload_url)
            .body(bytes.clone())
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .post(&upload_url)
            .bearer_auth(TOKEN)
            .header(header::ORIGIN, &origin)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let upload = client
        .post(&upload_url)
        .bearer_auth(TOKEN)
        .header(CWD, binding["cwdHandle"].as_str().unwrap())
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(bytes.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(upload.status(), StatusCode::OK);
    assert_eq!(upload.json::<Value>().await.unwrap()["published"], false);
    assert!(!root.path().join("binary.bin").exists());
    let selector = scoped(json!({"stageId":staged.stage_id}));
    let mut other = selector.clone();
    other["host"]["principalId"] = json!("other-principal");
    assert_eq!(
        reviewed_rpc(&client, &endpoint, "ai.workcell/transfer/seal", other).await["error"]["data"]
            ["code"],
        "transferBindingMismatch"
    );
    let sealed = reviewed_rpc(&client, &endpoint, "ai.workcell/transfer/seal", selector).await;
    assert_eq!(sealed["result"]["digest"], digest);
    let prepared = reviewed_rpc(
        &client,
        &endpoint,
        "ai.workcell/transfer/preparePublication",
        prepare_params,
    )
    .await;
    let prepared: workcell_host_contract::TransferPrepareResponse =
        serde_json::from_value(prepared["result"].clone()).unwrap();
    prepared.operation.intent.validate().unwrap();
    assert_eq!(prepared.operation.intent.resources.len(), 2);
    assert!(!root.path().join("binary.bin").exists());
    let execute = json!({"version":"v1", "host":remote_host_binding(&descriptor), "preparationId":prepared.operation.preparation_id,"invocationId":"binary-invocation"});
    let completed = reviewed_rpc(&client, &endpoint, "ai.workcell/execute", execute.clone()).await;
    assert_eq!(completed["result"]["state"], "completed", "{completed}");
    assert_eq!(
        tokio::fs::read(root.path().join("binary.bin"))
            .await
            .unwrap(),
        bytes
    );
    let stat = reviewed_rpc(
        &client,
        &endpoint,
        "ai.workcell/transfer/stat",
        scoped(json!({"path":"binary.bin"})),
    )
    .await;
    let file = &stat["result"]["file"];
    assert_eq!(file["sizeBytes"], LARGE_BYTES);
    assert_eq!(file["digest"], digest);
    let selected = reviewed_rpc(
        &client,
        &endpoint,
        "ai.workcell/transfer/download",
        scoped(json!({"path":"binary.bin","revision":file["revision"],"digest":file["digest"]})),
    )
    .await;
    let download = format!(
        "{origin}{}",
        selected["result"]["downloadPath"].as_str().unwrap()
    );
    let response = client
        .get(&download)
        .bearer_auth(TOKEN)
        .header(CWD, binding["cwdHandle"].as_str().unwrap())
        .header(
            header::IF_MATCH,
            format!("\"{}\"", file["revision"].as_str().unwrap()),
        )
        .header(header::RANGE, "bytes=1048576-1048583")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.bytes().await.unwrap().as_ref(), &[0xff; 8]);
    tokio::fs::write(root.path().join("binary.bin"), b"later external edit")
        .await
        .unwrap();
    assert_eq!(
        reviewed_rpc(&client, &endpoint, "ai.workcell/execute", execute).await["result"],
        completed["result"]
    );
    assert_eq!(
        tokio::fs::read(root.path().join("binary.bin"))
            .await
            .unwrap(),
        b"later external edit"
    );
    let malformed = client
        .post(format!("{origin}/files?reviewed=v1&path=must-not-publish"))
        .bearer_auth(TOKEN)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body("no fallback")
        .send()
        .await
        .unwrap();
    assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
    assert!(!root.path().join("must-not-publish").exists());
    let status_params = scoped(json!({"publicationId":"binary-publication"}));
    let durable = reviewed_rpc(
        &client,
        &endpoint,
        "ai.workcell/transfer/publicationStatus",
        status_params,
    )
    .await["result"]
        .clone();
    assert_eq!(durable["state"], "completed");
    assert_eq!(http.shutdown().await, ShutdownOutcome::Completed);
    let restarted = reviewed_transfer_server(root.path(), private.path()).await;
    let endpoint = format!("http://{}/mcp", restarted.address());
    let descriptor = reviewed_discovery(&client, &endpoint).await;
    let status = reviewed_rpc(&client, &endpoint, "ai.workcell/transfer/publicationStatus", json!({"version":"v1","host":remote_host_binding(&descriptor),"cwdHandle":descriptor["cwd"]["handle"],"publicationId":"binary-publication"})).await;
    assert_eq!(status["result"], durable);
    assert_eq!(restarted.shutdown().await, ShutdownOutcome::Completed);
}
