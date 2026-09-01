use std::time::Duration;

use reqwest::{Client, Response, StatusCode, header};
use rmcp::ServiceExt;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use workcell_mcp::{
    cli::{HttpBindMode, ToolGroup},
    server::{ServerBehavior, ToolConfiguration, WorkcellServer},
    transports::http::{HttpAuthentication, HttpConfiguration, HttpServer, ShutdownOutcome},
};
use workcell_mcp_code::{CodeConfiguration, WorkerSource};
use workcell_mcp_shell::ShellPermissionPolicy;
use workcell_mcp_web::WebsearchExecutionConfiguration;

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
        &[ToolGroup::Files, ToolGroup::Web, ToolGroup::Shell],
        ServerBehavior {
            expose_execution_environment: true,
            modern_only,
        },
        ToolConfiguration {
            allow_write: false,
            web: WebsearchExecutionConfiguration::unconfigured(),
            web_icons: false,
            shell_policy,
            shell_output_filter: true,
            code: CodeConfiguration {
                worker: WorkerSource::Discover {
                    bundled_cache_root: None,
                },
                type_check: true,
            },
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

    write_json(
        &mut write,
        &discover_request(
            1,
            json!({
                "extensions": {
                    "ai.workcell/execution-environment": {"versions": ["v1"]}
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

    write_json(&mut write, &mcp_request(2, "tools/list", json!({}))).await;
    let listed = read_json(&mut read).await;
    let names = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            "file_read",
            "file_glob",
            "file_grep",
            "file_write",
            "file_edit",
            "file_apply_patch",
            "index",
            "websearch",
            "webfetch",
            "shell",
            "execution_environment",
        ]
    );

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
            json!({"name":"index","arguments":{"path":"visible.rs"}}),
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
        json!({"files": true, "web": true, "shell": true, "code": false})
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
    assert_eq!(
        final_sse_json(listed).await["result"]["tools"]
            .as_array()
            .unwrap()
            .len(),
        11
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
    assert_eq!(listed["result"]["tools"].as_array().unwrap().len(), 11);
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
async fn stdio_serves_the_full_catalog_including_code_execution() {
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
            ToolGroup::Code,
        ],
        ServerBehavior {
            expose_execution_environment: true,
            modern_only: false,
        },
        ToolConfiguration {
            allow_write: false,
            web: WebsearchExecutionConfiguration::unconfigured(),
            web_icons: false,
            shell_policy: ShellPermissionPolicy::restricted(),
            shell_output_filter: true,
            code: CodeConfiguration {
                worker: WorkerSource::Path(&worker),
                type_check: true,
            },
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
    // Catalog order is a compatibility contract; `code_execution` sits after `shell`.
    assert_eq!(
        names,
        [
            "file_read",
            "file_glob",
            "file_grep",
            "file_write",
            "file_edit",
            "file_apply_patch",
            "index",
            "websearch",
            "webfetch",
            "shell",
            "code_execution",
            "execution_environment",
        ]
    );

    write_json(
        &mut write,
        &mcp_request(
            3,
            "tools/call",
            json!({"name": "code_execution", "arguments": {"code": "sum([1, 2, 3, 4])"}}),
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
        .find(|tool| tool["name"] == "code_execution")
        .expect("code tool listed");
    assert_eq!(code_tool["annotations"]["readOnlyHint"], json!(true));
    assert_eq!(code_tool["annotations"]["openWorldHint"], json!(false));

    drop(write);
    let _ = tokio::time::timeout(Duration::from_secs(5), server_task).await;
}
