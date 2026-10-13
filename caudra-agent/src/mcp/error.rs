use serde_json::{Map, Value};
use thiserror::Error;

use super::oauth::REFUSED_ACCESS;
use super::protocol::{
    INVALID_PARAMS, JsonRpcError, LEGACY_RESOURCE_NOT_FOUND, MISSING_REQUIRED_CLIENT_CAPABILITY,
};
use crate::tools::ToolFailure;

/// Why a call whose stream broke cannot simply be sent again: the server may
/// already have acted on it.
pub const CONNECTION_LOST: &str = "connection lost; the call's outcome is unknown";
const HTTP_UNAUTHORIZED: u16 = 401;
const HTTP_FORBIDDEN: u16 = 403;
const REQUIRED_CAPABILITIES_KEY: &str = "requiredCapabilities";
const CAPABILITY_SEPARATOR: &str = ", ";

#[derive(Debug, Error)]
pub enum McpError {
    #[error("server {server} failed to start: {reason}")]
    StartFailed { server: String, reason: String },

    #[error("server {server} is not running")]
    ServerDied { server: String },

    #[error("server {server} timed out after {timeout_ms}ms")]
    Timeout { server: String, timeout_ms: u64 },

    #[error("server {server} returned error {code}: {message}")]
    RpcError {
        server: String,
        code: i64,
        message: String,
        data: Option<Box<Value>>,
    },

    #[error("call to server {server} was cancelled")]
    Cancelled { server: String },

    #[error("server {server}: {CONNECTION_LOST}")]
    ConnectionLost { server: String },

    #[error("server {server} needed input it did not get: {reason}")]
    Elicitation { server: String, reason: String },

    #[error("server {server} needs client capabilities this call does not declare: {capabilities}")]
    MissingCapability {
        server: String,
        capabilities: String,
    },

    #[error("invalid response from server {server}: {reason}")]
    InvalidResponse { server: String, reason: String },

    #[error("unknown MCP tool: {name}")]
    UnknownTool { name: String },

    #[error("unknown MCP prompt: {name}")]
    UnknownPrompt { name: String },

    #[error("server {server} does not offer {feature}")]
    Unsupported {
        server: String,
        feature: &'static str,
    },

    #[error("config error: {0}")]
    Config(String),

    #[error("write to server {server} failed: {reason}")]
    WriteFailed { server: String, reason: String },

    #[error("HTTP error from server {server}: {status} {reason}")]
    HttpError {
        server: String,
        status: u16,
        reason: String,
    },

    #[error("server {server} requires OAuth authentication")]
    OAuthRequired { server: String },

    #[error("server {server}: {REFUSED_ACCESS}")]
    Refused { server: String },

    #[error("OAuth failed for server {server}: {reason}")]
    OAuthFailed { server: String, reason: String },
}

impl McpError {
    /// The server's JSON-RPC error. A -32021 names the capabilities it wanted,
    /// so it reads as that rather than as a bare code.
    pub(super) fn rpc(server: &str, error: JsonRpcError) -> Self {
        if error.code == MISSING_REQUIRED_CLIENT_CAPABILITY
            && let Some(required) = error
                .data
                .as_ref()
                .and_then(|data| data[REQUIRED_CAPABILITIES_KEY].as_object())
        {
            return Self::MissingCapability {
                server: server.into(),
                capabilities: capability_names(required),
            };
        }
        Self::RpcError {
            server: server.into(),
            code: error.code,
            message: error.message,
            data: error.data.map(Box::new),
        }
    }

    /// `resources/read` found nothing: -32602 now, -32002 from servers that
    /// predate 2026-07-28.
    pub fn resource_not_found(&self) -> bool {
        matches!(
            self,
            Self::RpcError {
                code: INVALID_PARAMS | LEGACY_RESOURCE_NOT_FOUND,
                ..
            }
        )
    }

    /// JSON-RPC and HTTP codes are the server's own typed answer. A tool that
    /// reported `isError` explained itself only in text, so it stays opaque.
    pub(crate) fn failure(&self) -> ToolFailure {
        match self {
            Self::Timeout { .. } => ToolFailure::Timeout,
            Self::Cancelled { .. } => ToolFailure::Cancelled,
            Self::UnknownTool { .. } => ToolFailure::NotFound,
            Self::RpcError {
                code: INVALID_PARAMS,
                ..
            } => ToolFailure::InvalidInput,
            Self::OAuthRequired { .. }
            | Self::Refused { .. }
            | Self::HttpError {
                status: HTTP_UNAUTHORIZED | HTTP_FORBIDDEN,
                ..
            } => ToolFailure::Denied,
            _ => ToolFailure::Other,
        }
    }
}

/// `elicitation`, or `elicitation.url` for a capability narrowed to a mode.
fn capability_names(required: &Map<String, Value>) -> String {
    required
        .iter()
        .flat_map(|(name, detail)| match detail.as_object() {
            Some(modes) if !modes.is_empty() => {
                modes.keys().map(|mode| format!("{name}.{mode}")).collect()
            }
            _ => vec![name.clone()],
        })
        .collect::<Vec<_>>()
        .join(CAPABILITY_SEPARATOR)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const SERVER: &str = "srv";
    const JSONRPC_INTERNAL_ERROR: i64 = -32603;
    const HTTP_INTERNAL_ERROR: u16 = 500;

    fn rpc(code: i64) -> McpError {
        McpError::RpcError {
            server: SERVER.into(),
            code,
            message: SERVER.into(),
            data: None,
        }
    }

    fn http(status: u16) -> McpError {
        McpError::HttpError {
            server: SERVER.into(),
            status,
            reason: SERVER.into(),
        }
    }

    #[test_case(McpError::Timeout { server: SERVER.into(), timeout_ms: 1 }, ToolFailure::Timeout; "timeout")]
    #[test_case(McpError::UnknownTool { name: SERVER.into() }, ToolFailure::NotFound; "unknown_tool")]
    #[test_case(McpError::Cancelled { server: SERVER.into() }, ToolFailure::Cancelled; "cancelled")]
    #[test_case(rpc(INVALID_PARAMS), ToolFailure::InvalidInput; "invalid_params")]
    #[test_case(rpc(JSONRPC_INTERNAL_ERROR), ToolFailure::Other; "internal_error")]
    #[test_case(McpError::OAuthRequired { server: SERVER.into() }, ToolFailure::Denied; "oauth_required")]
    #[test_case(McpError::Refused { server: SERVER.into() }, ToolFailure::Denied; "refused")]
    #[test_case(http(HTTP_UNAUTHORIZED), ToolFailure::Denied; "unauthorized")]
    #[test_case(http(HTTP_FORBIDDEN), ToolFailure::Denied; "forbidden")]
    #[test_case(http(HTTP_INTERNAL_ERROR), ToolFailure::Other; "server_error")]
    #[test_case(McpError::ServerDied { server: SERVER.into() }, ToolFailure::Other; "server_died")]
    fn a_typed_server_answer_names_the_failure(error: McpError, expected: ToolFailure) {
        assert_eq!(error.failure(), expected);
    }

    #[test_case(json!({REQUIRED_CAPABILITIES_KEY: {"elicitation": {}}}), "elicitation" ; "bare")]
    #[test_case(json!({REQUIRED_CAPABILITIES_KEY: {"elicitation": {"url": {}}, "sampling": {}}}), "elicitation.url, sampling" ; "narrowed_to_a_mode")]
    fn a_missing_client_capability_names_what_the_server_wanted(data: Value, expected: &str) {
        let error = McpError::rpc(
            SERVER,
            JsonRpcError {
                code: MISSING_REQUIRED_CLIENT_CAPABILITY,
                message: SERVER.into(),
                data: Some(data),
            },
        );
        assert!(
            matches!(&error, McpError::MissingCapability { capabilities, .. } if capabilities == expected),
            "{error:?}"
        );
        assert!(error.to_string().contains(expected));
    }

    #[test]
    fn a_missing_client_capability_without_data_stays_the_servers_error() {
        let error = McpError::rpc(
            SERVER,
            JsonRpcError::new(MISSING_REQUIRED_CLIENT_CAPABILITY, SERVER),
        );
        assert!(matches!(
            error,
            McpError::RpcError {
                code: MISSING_REQUIRED_CLIENT_CAPABILITY,
                ..
            }
        ));
    }

    #[test_case(INVALID_PARAMS, true ; "current")]
    #[test_case(LEGACY_RESOURCE_NOT_FOUND, true ; "legacy")]
    #[test_case(JSONRPC_INTERNAL_ERROR, false ; "other")]
    fn both_resource_not_found_codes_are_recognised(code: i64, expected: bool) {
        assert_eq!(rpc(code).resource_not_found(), expected);
    }
}
