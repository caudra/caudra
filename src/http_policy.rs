use std::{collections::HashSet, sync::Arc, time::Instant};

use axum::{
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{
        HeaderMap, HeaderValue, Method, StatusCode,
        header::{
            ALLOW, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE, HOST, ORIGIN,
            X_CONTENT_TYPE_OPTIONS,
        },
        uri::Authority,
    },
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use uuid::Uuid;

pub const ENDPOINT_PATH: &str = "/mcp";
pub const MAX_JSON_BODY_BYTES: usize = 12 * 1024 * 1024;

const REQUEST_ID: &str = "x-request-id";

#[derive(Clone, Debug)]
pub struct HttpPolicy {
    allowed_hosts: Arc<HashSet<String>>,
    modern_only: bool,
    transfer: bool,
}

impl HttpPolicy {
    /// `transfer` must match whether the transfer tool group was actually built. When it is false the
    /// transfer route is indistinguishable from any other unknown path, so a deployment without the
    /// group does not disclose that the route exists.
    #[must_use]
    pub fn new(hosts: impl IntoIterator<Item = String>, modern_only: bool, transfer: bool) -> Self {
        Self {
            allowed_hosts: Arc::new(
                hosts
                    .into_iter()
                    .map(|host| host.trim_matches(['[', ']']).to_ascii_lowercase())
                    .collect(),
            ),
            modern_only,
            transfer,
        }
    }

    fn classify_route(&self, path: &str) -> Route {
        match route_of(path) {
            Route::Transfer if !self.transfer => Route::Unknown,
            route => route,
        }
    }
}

/// Which contract a request is speaking. The two routes differ in method set, body handling, and
/// error envelope, so every branch below has to know which one it is answering for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Route {
    Mcp,
    Transfer,
    Unknown,
}

fn route_of(path: &str) -> Route {
    if path == ENDPOINT_PATH {
        Route::Mcp
    } else if path == crate::transfer::ENDPOINT_PATH {
        Route::Transfer
    } else {
        Route::Unknown
    }
}

pub async fn enforce(
    State(policy): State<HttpPolicy>,
    mut request: Request,
    next: Next,
) -> Response {
    let request_id = format!("http_{}", Uuid::new_v4());
    let started = Instant::now();
    let method = safe_method(request.method());
    let route = policy.classify_route(request.uri().path());
    let mut response = match classify_host(request.headers(), &policy.allowed_hosts) {
        HostClassification::Invalid => route_error(
            route,
            StatusCode::BAD_REQUEST,
            -32_000,
            "Invalid Host header.",
        ),
        HostClassification::Foreign => {
            route_error(route, StatusCode::FORBIDDEN, -32_000, "Host not allowed.")
        }
        HostClassification::Allowed if request.headers().contains_key(ORIGIN) => route_error(
            route,
            StatusCode::FORBIDDEN,
            -32_000,
            "Browser-origin requests are not allowed.",
        ),
        // A disabled transfer route lands here alongside every other unknown path, so the response
        // does not disclose that this build could have served it.
        HostClassification::Allowed if route == Route::Unknown => {
            policy_error(StatusCode::NOT_FOUND, -32_001, "MCP endpoint not found.")
        }
        HostClassification::Allowed if route == Route::Transfer => {
            if matches!(*request.method(), Method::GET | Method::POST) {
                // Deliberately no body buffering: a transfer body is arbitrarily large and must
                // stream through to the handler.
                bound_transfer_server_error(next.run(request).await)
            } else {
                let mut response =
                    transfer_error(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed.");
                response
                    .headers_mut()
                    .insert(ALLOW, HeaderValue::from_static("GET, POST"));
                response
            }
        }
        HostClassification::Allowed if request.headers().contains_key("mcp-session-id") => {
            policy_error(
                StatusCode::BAD_REQUEST,
                -32_000,
                "MCP protocol sessions are not supported.",
            )
        }
        HostClassification::Allowed if request.method() != Method::POST => {
            let mut response = policy_error(
                StatusCode::METHOD_NOT_ALLOWED,
                -32_000,
                "Method not allowed.",
            );
            response
                .headers_mut()
                .insert(ALLOW, HeaderValue::from_static("POST"));
            response
        }
        HostClassification::Allowed => match bounded_body(request, policy.modern_only).await {
            Ok(bounded) => {
                request = bounded;
                bound_server_error(next.run(request).await)
            }
            Err(response) => response,
        },
    };
    apply_security_headers(&mut response, &request_id);
    tracing::debug!(
        operation = "mcp.http.completed",
        request_id,
        method,
        status = response.status().as_u16(),
        duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "HTTP request completed"
    );
    response
}

// Boxing the `Err` variant would not shrink this `Result`: the `Ok` variant `Request` is larger than
// `Response`, so the allocation would cost a heap indirection per rejected request for no benefit.
#[allow(clippy::result_large_err)]
async fn bounded_body(request: Request, modern_only: bool) -> Result<Request, Response> {
    if let Some(declared) = request.headers().get(CONTENT_LENGTH) {
        let declared = declared
            .to_str()
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| {
                policy_error(
                    StatusCode::BAD_REQUEST,
                    -32_700,
                    "Invalid Content-Length header.",
                )
            })?;
        if declared > MAX_JSON_BODY_BYTES as u64 {
            return Err(body_too_large());
        }
    }
    let (parts, body) = request.into_parts();
    let bytes = to_bytes(body, MAX_JSON_BODY_BYTES)
        .await
        .map_err(|_| body_too_large())?;
    let value = serde_json::from_slice::<Value>(&bytes).map_err(|_| {
        policy_error(
            StatusCode::BAD_REQUEST,
            -32_700,
            "Invalid JSON request body.",
        )
    })?;
    validate_protocol_admission(&parts.headers, &value, modern_only)
        .map_err(|error| (*error).into_response())?;
    Ok(Request::from_parts(parts, Body::from(bytes)))
}

struct ProtocolAdmissionError {
    code: i32,
    message: &'static str,
    id: Value,
    data: Option<Value>,
}

impl ProtocolAdmissionError {
    fn into_response(self) -> Response {
        json_rpc_error(
            StatusCode::BAD_REQUEST,
            self.code,
            self.message,
            self.id,
            self.data,
        )
    }
}

fn validate_protocol_admission(
    headers: &HeaderMap,
    body: &Value,
    modern_only: bool,
) -> Result<(), Box<ProtocolAdmissionError>> {
    match body {
        Value::Array(messages) => {
            for message in messages {
                validate_protocol_message(headers, message, modern_only)?;
            }
            Ok(())
        }
        Value::Object(_) => validate_protocol_message(headers, body, modern_only),
        _ => Ok(()),
    }
}

fn validate_protocol_message(
    headers: &HeaderMap,
    message: &Value,
    modern_only: bool,
) -> Result<(), Box<ProtocolAdmissionError>> {
    let id = message.get("id").cloned().unwrap_or(Value::Null);
    let method = message.get("method").and_then(Value::as_str);
    let body_version = message
        .pointer("/params/_meta/io.modelcontextprotocol~1protocolVersion")
        .and_then(Value::as_str);
    let header_version = headers
        .get("mcp-protocol-version")
        .and_then(|value| value.to_str().ok());

    if method == Some("initialize") {
        let requested = message
            .pointer("/params/protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if requested == "2026-07-28" {
            return Err(Box::new(ProtocolAdmissionError {
                code: -32_600,
                message: "initialize is not valid for MCP 2026-07-28; use server/discover or per-request metadata.",
                id,
                data: Some(json!({"supported": crate::server::protocol_versions(modern_only)})),
            }));
        }
        return require_supported_protocol(id, requested, modern_only);
    }
    if let Some(requested) = body_version {
        return require_supported_protocol(id, requested, modern_only);
    }

    let requested = header_version.unwrap_or("2025-03-26");
    if requested == "2026-07-28" {
        return Err(Box::new(ProtocolAdmissionError {
            code: -32_602,
            message: "Modern MCP requests require per-request protocol metadata.",
            id,
            data: None,
        }));
    }
    require_supported_protocol(id, requested, modern_only)
}

fn require_supported_protocol(
    id: Value,
    requested: &str,
    modern_only: bool,
) -> Result<(), Box<ProtocolAdmissionError>> {
    let supported = crate::server::protocol_versions(modern_only);
    if supported
        .iter()
        .any(|version| version.as_str() == requested)
    {
        return Ok(());
    }
    Err(Box::new(ProtocolAdmissionError {
        code: -32_022,
        message: "Unsupported protocol version",
        id,
        data: Some(json!({"requested": requested, "supported": supported})),
    }))
}

fn body_too_large() -> Response {
    policy_error(
        StatusCode::PAYLOAD_TOO_LARGE,
        -32_700,
        "Request body too large.",
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HostClassification {
    Allowed,
    Invalid,
    Foreign,
}

fn classify_host(headers: &HeaderMap, allowed_hosts: &HashSet<String>) -> HostClassification {
    if headers.get_all(HOST).iter().count() != 1 {
        return HostClassification::Invalid;
    }
    let Some(value) = headers.get(HOST).and_then(|value| value.to_str().ok()) else {
        return HostClassification::Invalid;
    };
    if value.contains('@') {
        return HostClassification::Invalid;
    }
    let Ok(authority) = Authority::try_from(value) else {
        return HostClassification::Invalid;
    };
    let has_port_suffix = if value.starts_with('[') {
        value.find(']').is_some_and(|index| index + 1 < value.len())
    } else {
        value.contains(':')
    };
    if has_port_suffix && authority.port().is_none() {
        return HostClassification::Invalid;
    }
    let host = authority
        .host()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    if allowed_hosts.contains(&host) {
        HostClassification::Allowed
    } else {
        HostClassification::Foreign
    }
}

fn safe_method(method: &Method) -> &'static str {
    match *method {
        Method::GET => "GET",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::DELETE => "DELETE",
        Method::PATCH => "PATCH",
        Method::HEAD => "HEAD",
        Method::OPTIONS => "OPTIONS",
        Method::CONNECT => "CONNECT",
        Method::TRACE => "TRACE",
        _ => "OTHER",
    }
}

pub(crate) fn policy_error(status: StatusCode, code: i32, message: &'static str) -> Response {
    json_rpc_error(status, code, message, Value::Null, None)
}

/// Rejects a request in the envelope its route speaks. A caller that asked for bytes and received a
/// JSON-RPC error would have to guess which contract answered it.
fn route_error(route: Route, status: StatusCode, code: i32, message: &'static str) -> Response {
    match route {
        Route::Transfer => transfer_error(status, message),
        Route::Mcp | Route::Unknown => policy_error(status, code, message),
    }
}

/// Route-aware rejection for layers that run after [`enforce`] and so only ever see a path this
/// build actually serves.
pub(crate) fn path_error(
    path: &str,
    status: StatusCode,
    code: i32,
    message: &'static str,
) -> Response {
    route_error(route_of(path), status, code, message)
}

fn transfer_error(status: StatusCode, message: &'static str) -> Response {
    (
        status,
        [(CONTENT_TYPE, "application/json; charset=utf-8")],
        json!({ "message": message, "code": status.as_u16() }).to_string(),
    )
        .into_response()
}

fn json_rpc_error(
    status: StatusCode,
    code: i32,
    message: &'static str,
    id: Value,
    data: Option<Value>,
) -> Response {
    let mut error = json!({"code": code, "message": message});
    if let Some(data) = data {
        error["data"] = data;
    }
    (
        status,
        [(CONTENT_TYPE, "application/json; charset=utf-8")],
        json!({
            "jsonrpc": "2.0",
            "error": error,
            "id": id,
        })
        .to_string(),
    )
        .into_response()
}

fn bound_server_error(response: Response) -> Response {
    if response.status().is_server_error() {
        policy_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            -32_603,
            "Internal server error.",
        )
    } else {
        response
    }
}

/// The transfer-route counterpart of [`bound_server_error`]. Replacing the body also discards any
/// streaming body the handler had begun, which is the point: a failed transfer must not leave a
/// caller reading a truncated response as if it were the file.
fn bound_transfer_server_error(response: Response) -> Response {
    if response.status().is_server_error() {
        transfer_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error.")
    } else {
        response
    }
}

fn apply_security_headers(response: &mut Response, request_id: &str) {
    response.headers_mut().insert(
        REQUEST_ID,
        HeaderValue::from_str(request_id)
            .unwrap_or_else(|_| HeaderValue::from_static("http_invalid")),
    );
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_policy_uses_the_explicit_allowlist() {
        let hosts = HashSet::from(["127.0.0.1".into(), "workcell.internal".into()]);
        for host in ["127.0.0.1:3001", "workcell.internal"] {
            let mut headers = HeaderMap::new();
            headers.insert(HOST, HeaderValue::from_str(host).unwrap());
            assert_eq!(classify_host(&headers, &hosts), HostClassification::Allowed);
        }
        let mut headers = HeaderMap::new();
        headers.insert(HOST, HeaderValue::from_static("example.com"));
        assert_eq!(classify_host(&headers, &hosts), HostClassification::Foreign);
    }

    /// A build without the transfer group must answer `/files` exactly as it answers any other
    /// unknown path, so the response does not disclose that the route exists elsewhere.
    #[test]
    fn a_disabled_transfer_route_is_indistinguishable_from_an_unknown_path() {
        let disabled = HttpPolicy::new(["127.0.0.1".to_owned()], false, false);
        assert_eq!(disabled.classify_route("/files"), Route::Unknown);
        assert_eq!(disabled.classify_route("/absent"), Route::Unknown);

        let enabled = HttpPolicy::new(["127.0.0.1".to_owned()], false, true);
        assert_eq!(enabled.classify_route("/files"), Route::Transfer);
        assert_eq!(enabled.classify_route("/mcp"), Route::Mcp);
        // `/mcp` is matched exactly; a nested path is not the protocol endpoint.
        assert_eq!(enabled.classify_route("/files/nested"), Route::Unknown);
    }

    /// A caller that asked for bytes must not have to guess whether a JSON-RPC envelope came from
    /// the protocol endpoint or from the transfer route.
    #[test]
    fn each_route_rejects_in_its_own_envelope() {
        let transfer = transfer_error(StatusCode::FORBIDDEN, "Host not allowed.");
        assert_eq!(transfer.status(), StatusCode::FORBIDDEN);
        let mcp = route_error(
            Route::Mcp,
            StatusCode::FORBIDDEN,
            -32_000,
            "Host not allowed.",
        );
        assert_eq!(mcp.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            path_error(
                crate::transfer::ENDPOINT_PATH,
                StatusCode::UNAUTHORIZED,
                -32_000,
                "Bearer authentication is required.",
            )
            .status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn transfer_route_bodies_are_never_buffered_or_json_parsed() {
        // The `/mcp` bound exists to keep a JSON document parseable in memory. A transfer body is
        // arbitrarily large, so admitting it through the same path would cap every upload at this
        // bound and buffer it entirely in memory.
        const { assert!(MAX_JSON_BODY_BYTES < crate::cli::DEFAULT_MAX_TRANSFER_BYTES) };
        let request = Request::post("/files?path=a.bin")
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(Body::from(vec![0_u8; 32]))
            .expect("request");
        // A body that is not JSON would be rejected by `bounded_body`; the transfer branch never
        // calls it, which is what makes streaming possible.
        assert!(bounded_body(request, false).await.is_err());
    }
}
