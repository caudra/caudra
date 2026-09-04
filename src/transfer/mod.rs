//! Server-only file transfer.
//!
//! Bytes move over a dedicated HTTP route, never through a tool result: the MCP result ceiling is
//! tens of kilobytes, so a base64 payload could not carry a real file. The tools here authorize a
//! path and hand the harness a URL; [`endpoints`] moves the bytes.
//!
//! This module lives in the binary rather than a crate on purpose. `crates/workcell` re-exports only
//! from `crates/`, so a native embedder cannot link file transfer even by enabling every facade
//! feature. That is a compile-time guarantee rather than a policy choice.

pub mod catalog;
pub mod endpoints;

use std::path::Path;

use rmcp::{
    ErrorData,
    model::{CallToolResult, ContentBlock, Tool},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use workcell_mcp_files::{FileResourceAccess, FileToolGroup, FilesystemError};

/// The single transfer route. Kept distinct from the MCP endpoint so host policy can admit each with
/// its own method set and body handling.
pub const ENDPOINT_PATH: &str = "/files";

const OCTET_STREAM: &str = "application/octet-stream";

#[derive(Clone, Debug)]
pub struct TransferToolGroup {
    files: FileToolGroup,
    max_transfer_bytes: usize,
}

#[derive(Debug, Deserialize)]
struct TransferInput {
    path: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DownloadOutput {
    method: &'static str,
    url: String,
    path: String,
    name: String,
    bytes: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UploadOutput {
    method: &'static str,
    url: String,
    path: String,
    name: String,
    content_type: &'static str,
    max_bytes: usize,
}

impl TransferToolGroup {
    pub async fn new(
        root: impl AsRef<Path>,
        allow_write: bool,
        max_transfer_bytes: usize,
    ) -> Result<Self, FilesystemError> {
        Ok(Self {
            files: FileToolGroup::new(root, allow_write, None).await?,
            max_transfer_bytes,
        })
    }

    #[must_use]
    pub fn catalog(&self) -> Vec<Tool> {
        catalog::catalog(self.files.allow_write())
    }

    #[must_use]
    pub fn allow_write(&self) -> bool {
        self.files.allow_write()
    }

    #[must_use]
    pub fn max_transfer_bytes(&self) -> usize {
        self.max_transfer_bytes
    }

    /// Returns `None` only when this group does not own the tool name, so a server can compose it
    /// alongside the other groups without a second routing table.
    pub async fn dispatch(
        &self,
        name: &str,
        arguments: Value,
    ) -> Option<Result<CallToolResult, ErrorData>> {
        match name {
            "file_download" => Some(self.download(arguments).await),
            "file_upload" if self.files.allow_write() => Some(self.upload(arguments).await),
            _ => None,
        }
    }

    async fn download(&self, arguments: Value) -> Result<CallToolResult, ErrorData> {
        let input = match parse(arguments) {
            Ok(input) => input,
            Err(message) => return tool_error(message),
        };
        let resource = match self
            .files
            .inspect_path(&input.path, FileResourceAccess::Read)
            .await
        {
            Ok(resource) => resource,
            Err(error) => return tool_error(error.to_string()),
        };
        let metadata = match tokio::fs::metadata(&resource.path).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return tool_error(format!("Path does not exist: {}", input.path));
            }
            Err(_) => return tool_error(format!("Cannot read path: {}", input.path)),
        };
        if metadata.is_dir() {
            return tool_error(format!(
                "Path is a directory: {}. Transfer moves a single file; use file_read to list a directory.",
                input.path
            ));
        }
        if !metadata.is_file() {
            return tool_error(format!("Path is not a regular file: {}", input.path));
        }
        let bytes = metadata.len();
        if bytes > self.max_transfer_bytes as u64 {
            return tool_error(format!(
                "File is {bytes} bytes, larger than the {} byte transfer limit.",
                self.max_transfer_bytes
            ));
        }
        let relative = self.relative(&resource.path);
        let name = file_name(&resource.path);
        let output = DownloadOutput {
            method: "GET",
            url: transfer_url(&relative),
            path: relative,
            name,
            bytes,
        };
        let text = format!(
            "Prepared a download for `{}` ({} bytes). No bytes were transferred by this call.\n\nThe harness must now issue `GET {}` against the same origin as its MCP endpoint, reusing the credentials it already sends, and write the response body to the destination the user asked for.",
            output.name, output.bytes, output.url
        );
        success(&output, text)
    }

    async fn upload(&self, arguments: Value) -> Result<CallToolResult, ErrorData> {
        let input = match parse(arguments) {
            Ok(input) => input,
            Err(message) => return tool_error(message),
        };
        let resource = match self
            .files
            .inspect_path(&input.path, FileResourceAccess::Write)
            .await
        {
            Ok(resource) => resource,
            Err(error) => return tool_error(error.to_string()),
        };
        // Refuse a destination the endpoint would reject anyway, so the harness does not discover it
        // only after streaming a body.
        if let Ok(metadata) = tokio::fs::metadata(&resource.path).await
            && metadata.is_dir()
        {
            return tool_error(format!("Path is a directory: {}", input.path));
        }
        let relative = self.relative(&resource.path);
        let name = file_name(&resource.path);
        let output = UploadOutput {
            method: "POST",
            url: transfer_url(&relative),
            path: relative,
            name,
            content_type: OCTET_STREAM,
            max_bytes: self.max_transfer_bytes,
        };
        let text = format!(
            "Prepared an upload for `{}`. No bytes were transferred by this call.\n\nThe harness must now issue `POST {}` against the same origin as its MCP endpoint, with `Content-Type: {}`, the raw bytes as the request body, and the credentials it already sends. Bodies over {} bytes are rejected.",
            output.name, output.url, OCTET_STREAM, output.max_bytes
        );
        success(&output, text)
    }

    /// Falls back to the absolute path when the resolved path is not beneath the root. Confined
    /// resolution makes that unreachable; the fallback exists so a future unconfined caller degrades
    /// to a still-correct URL instead of a panic.
    fn relative(&self, path: &Path) -> String {
        path.strip_prefix(self.files.root())
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn transfer_url(relative: &str) -> String {
    let encoded: String = url::form_urlencoded::byte_serialize(relative.as_bytes()).collect();
    format!("{ENDPOINT_PATH}?path={encoded}")
}

fn parse(arguments: Value) -> Result<TransferInput, String> {
    let input: TransferInput = serde_json::from_value(arguments)
        .map_err(|error| format!("Invalid transfer arguments: {error}"))?;
    if input.path.trim().is_empty() {
        return Err("path is required".to_owned());
    }
    Ok(input)
}

fn success(output: &impl Serialize, text: String) -> Result<CallToolResult, ErrorData> {
    let structured = serde_json::to_value(output).map_err(|error| {
        ErrorData::internal_error(
            "Failed to serialize transfer tool result",
            Some(Value::String(error.to_string())),
        )
    })?;
    let mut result = CallToolResult::default();
    result.content = vec![ContentBlock::text(text)];
    result.structured_content = Some(structured);
    Ok(result)
}

fn tool_error(message: impl Into<String>) -> Result<CallToolResult, ErrorData> {
    Ok(CallToolResult::error(vec![ContentBlock::text(
        message.into(),
    )]))
}

#[cfg(test)]
mod tests;
