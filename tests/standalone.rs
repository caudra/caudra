use std::time::Duration;

use reqwest::{Client, Response, StatusCode, header};
use rmcp::ServiceExt;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use workcell_mcp::{
    cli::{HttpBindMode, ToolGroup},
    server::WorkcellServer,
    transports::http::{HttpAuthentication, HttpConfiguration, HttpServer, ShutdownOutcome},
};
use workcell_mcp_shell::ShellPermissionPolicy;
use workcell_mcp_web::WebsearchExecutionConfiguration;

const ACCEPT: &str = "application/json, text/event-stream";
const PROTOCOL_VERSION: &str = "2026-07-28";
const TOKEN: &str = "workcell-integration-token-with-more-than-32-bytes";

async fn fixture_server() -> (TempDir, WorkcellServer) {
    fixture_server_with_policy(ShellPermissionPolicy::restricted()).await
}

async fn fixture_server_with_policy(
    shell_policy: ShellPermissionPolicy,
) -> (TempDir, WorkcellServer) {
    let root = tempfile::tempdir().expect("temporary root");
    tokio::fs::write(root.path().join("visible.txt"), "visible\n")
        .await
        .expect("fixture file");
    let server = WorkcellServer::configured(
        Some(root.path()),
        false,
        WebsearchExecutionConfiguration::unconfigured(),
        false,
        &[ToolGroup::Files, ToolGroup::Web, ToolGroup::Shell],
        true,
        shell_policy,
    )
    .await
    .expect("server");
    (root, server)
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

    write_json(&mut write, &discover_request(1, json!({}))).await;
    let discovered = read_json(&mut read).await;
    assert_eq!(
        discovered["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "workcell-mcp"
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
            "websearch",
            "webfetch",
            "shell",
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
            4,
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
        9
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

async fn final_sse_json(response: Response) -> Value {
    let content_type = response.headers()[header::CONTENT_TYPE].clone();
    let body = response.text().await.unwrap();
    if content_type == "application/json" {
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
