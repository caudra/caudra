use thiserror::Error;

use crate::tools::ToolFailure;

const JSONRPC_INVALID_PARAMS: i64 = -32602;
const HTTP_UNAUTHORIZED: u16 = 401;
const HTTP_FORBIDDEN: u16 = 403;

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
    },

    #[error("invalid response from server {server}: {reason}")]
    InvalidResponse { server: String, reason: String },

    #[error("unknown MCP tool: {name}")]
    UnknownTool { name: String },

    #[error("unknown MCP prompt: {name}")]
    UnknownPrompt { name: String },

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

    #[error("OAuth failed for server {server}: {reason}")]
    OAuthFailed { server: String, reason: String },
}

impl McpError {
    /// JSON-RPC and HTTP codes are the server's own typed answer. A tool that
    /// reported `isError` explained itself only in text, so it stays opaque.
    pub(crate) fn failure(&self) -> ToolFailure {
        match self {
            Self::Timeout { .. } => ToolFailure::Timeout,
            Self::UnknownTool { .. } => ToolFailure::NotFound,
            Self::RpcError {
                code: JSONRPC_INVALID_PARAMS,
                ..
            } => ToolFailure::InvalidInput,
            Self::OAuthRequired { .. }
            | Self::HttpError {
                status: HTTP_UNAUTHORIZED | HTTP_FORBIDDEN,
                ..
            } => ToolFailure::Denied,
            _ => ToolFailure::Other,
        }
    }
}

#[cfg(test)]
mod tests {
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
    #[test_case(rpc(JSONRPC_INVALID_PARAMS), ToolFailure::InvalidInput; "invalid_params")]
    #[test_case(rpc(JSONRPC_INTERNAL_ERROR), ToolFailure::Other; "internal_error")]
    #[test_case(McpError::OAuthRequired { server: SERVER.into() }, ToolFailure::Denied; "oauth_required")]
    #[test_case(http(HTTP_UNAUTHORIZED), ToolFailure::Denied; "unauthorized")]
    #[test_case(http(HTTP_FORBIDDEN), ToolFailure::Denied; "forbidden")]
    #[test_case(http(HTTP_INTERNAL_ERROR), ToolFailure::Other; "server_error")]
    #[test_case(McpError::ServerDied { server: SERVER.into() }, ToolFailure::Other; "server_died")]
    fn a_typed_server_answer_names_the_failure(error: McpError, expected: ToolFailure) {
        assert_eq!(error.failure(), expected);
    }
}
