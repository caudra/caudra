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
}

impl HttpPolicy {
    #[must_use]
    pub fn new(hosts: impl IntoIterator<Item = String>) -> Self {
        Self {
            allowed_hosts: Arc::new(
                hosts
                    .into_iter()
                    .map(|host| host.trim_matches(['[', ']']).to_ascii_lowercase())
                    .collect(),
            ),
        }
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
    let mut response = match classify_host(request.headers(), &policy.allowed_hosts) {
        HostClassification::Invalid => {
            policy_error(StatusCode::BAD_REQUEST, -32_000, "Invalid Host header.")
        }
        HostClassification::Foreign => {
            policy_error(StatusCode::FORBIDDEN, -32_000, "Host not allowed.")
        }
        HostClassification::Allowed if request.headers().contains_key(ORIGIN) => policy_error(
            StatusCode::FORBIDDEN,
            -32_000,
            "Browser-origin requests are not allowed.",
        ),
        HostClassification::Allowed if request.uri().path() != ENDPOINT_PATH => {
            policy_error(StatusCode::NOT_FOUND, -32_001, "MCP endpoint not found.")
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
        HostClassification::Allowed => match bounded_body(request).await {
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

async fn bounded_body(request: Request) -> Result<Request, Response> {
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
    serde_json::from_slice::<Value>(&bytes).map_err(|_| {
        policy_error(
            StatusCode::BAD_REQUEST,
            -32_700,
            "Invalid JSON request body.",
        )
    })?;
    Ok(Request::from_parts(parts, Body::from(bytes)))
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
    (
        status,
        [(CONTENT_TYPE, "application/json; charset=utf-8")],
        json!({
            "jsonrpc": "2.0",
            "error": { "code": code, "message": message },
            "id": null,
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
}
