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

use std::{mem::size_of, path::Path, sync::Arc, time::SystemTime};

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
    authority: Arc<()>,
}

#[derive(Debug, Deserialize)]
struct TransferInput {
    path: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferDirection {
    Download,
    Upload,
}

#[derive(Debug, Eq, PartialEq)]
pub struct TransferRevision {
    bytes: u64,
    modified: Option<SystemTime>,
    regular_file: bool,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    change_seconds: i64,
    #[cfg(unix)]
    change_nanoseconds: i64,
}

impl TransferRevision {
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }
}

#[derive(Debug, Eq, PartialEq)]
enum TransferPrecondition {
    Missing,
    Existing(TransferRevision),
}

#[derive(Debug)]
pub struct PreparedTransfer {
    authority: Arc<()>,
    resource: workcell_mcp_files::FileResource,
    direction: TransferDirection,
    relative_path: String,
    name: String,
    max_bytes: usize,
    precondition: TransferPrecondition,
}

impl PreparedTransfer {
    /// Conservative retained bytes; the group's shared authority allocation is excluded.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.resource.retained_bytes())
            .saturating_add(self.relative_path.capacity())
            .saturating_add(self.name.capacity())
    }

    #[must_use]
    pub const fn direction(&self) -> TransferDirection {
        self.direction
    }

    #[must_use]
    pub const fn resource(&self) -> &workcell_mcp_files::FileResource {
        &self.resource
    }

    #[must_use]
    pub fn path(&self) -> &str {
        &self.relative_path
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    #[must_use]
    pub const fn revision(&self) -> Option<&TransferRevision> {
        match &self.precondition {
            TransferPrecondition::Missing => None,
            TransferPrecondition::Existing(revision) => Some(revision),
        }
    }

    #[must_use]
    pub const fn requires_absent_destination(&self) -> bool {
        matches!(self.precondition, TransferPrecondition::Missing)
    }
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
            authority: Arc::new(()),
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

    pub async fn inspect(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<workcell_mcp_files::FileResource, String> {
        let input = parse(arguments)?;
        let access = match name {
            "file_download" => FileResourceAccess::Read,
            "file_upload" if self.files.allow_write() => FileResourceAccess::Write,
            _ => return Err("unknown transfer tool".to_owned()),
        };
        self.files
            .inspect_path(&input.path, access)
            .await
            .map_err(|error| error.to_string())
    }

    /// Returns `None` only when this group does not own the tool name, so a server can compose it
    /// alongside the other groups without a second routing table.
    pub async fn dispatch(
        &self,
        name: &str,
        arguments: Value,
    ) -> Option<Result<CallToolResult, ErrorData>> {
        match name {
            "file_download" | "file_upload" if name == "file_download" || self.allow_write() => {
                let prepared = match self.prepare(name, arguments).await {
                    Ok(prepared) => prepared,
                    Err(message) => return Some(tool_error(message)),
                };
                Some(self.execute_prepared(prepared).await)
            }
            _ => None,
        }
    }

    /// Parses, resolves, and snapshots one transfer without minting a URL or moving bytes.
    pub async fn prepare(&self, name: &str, arguments: Value) -> Result<PreparedTransfer, String> {
        let input = parse(arguments)?;
        let (direction, access) = match name {
            "file_download" => (TransferDirection::Download, FileResourceAccess::Read),
            "file_upload" if self.files.allow_write() => {
                (TransferDirection::Upload, FileResourceAccess::Write)
            }
            _ => return Err("unknown transfer tool".to_owned()),
        };
        let resource = self
            .files
            .inspect_path(&input.path, access)
            .await
            .map_err(|error| error.to_string())?;
        let precondition = match tokio::fs::metadata(&resource.path).await {
            Ok(metadata) => {
                if metadata.is_dir() {
                    let message = if direction == TransferDirection::Download {
                        format!(
                            "Path is a directory: {}. Transfer moves a single file; use file_read to list a directory.",
                            input.path
                        )
                    } else {
                        format!("Path is a directory: {}", input.path)
                    };
                    return Err(message);
                }
                if direction == TransferDirection::Download && !metadata.is_file() {
                    return Err(format!("Path is not a regular file: {}", input.path));
                }
                let revision = transfer_revision(&metadata);
                if direction == TransferDirection::Download
                    && revision.bytes > self.max_transfer_bytes as u64
                {
                    return Err(format!(
                        "File is {} bytes, larger than the {} byte transfer limit.",
                        revision.bytes, self.max_transfer_bytes
                    ));
                }
                TransferPrecondition::Existing(revision)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if direction == TransferDirection::Download {
                    return Err(format!("Path does not exist: {}", input.path));
                }
                TransferPrecondition::Missing
            }
            Err(_) => return Err(format!("Cannot inspect path: {}", input.path)),
        };
        let relative = self.relative(&resource.path);
        let name = file_name(&resource.path);
        Ok(PreparedTransfer {
            authority: self.authority.clone(),
            resource,
            direction,
            relative_path: relative,
            name,
            max_bytes: self.max_transfer_bytes,
            precondition,
        })
    }

    /// Reauthorizes and consumes a prepared transfer before minting its bearer-authenticated route.
    pub async fn execute_prepared(
        &self,
        prepared: PreparedTransfer,
    ) -> Result<CallToolResult, ErrorData> {
        if !Arc::ptr_eq(&self.authority, &prepared.authority) {
            return tool_error("Prepared transfer belongs to a different transfer tool group");
        }
        if prepared.direction == TransferDirection::Upload && !self.files.allow_write() {
            return tool_error("This server was started without --allow-write");
        }
        let access = match prepared.direction {
            TransferDirection::Download => FileResourceAccess::Read,
            TransferDirection::Upload => FileResourceAccess::Write,
        };
        let current = match self
            .files
            .inspect_path(&prepared.resource.requested_path, access)
            .await
        {
            Ok(resource) => resource,
            Err(error) => return tool_error(error.to_string()),
        };
        if current.path != prepared.resource.path {
            return tool_error("Transfer resource changed after preparation");
        }
        if let Err(message) = verify_precondition(&prepared).await {
            return tool_error(message);
        }

        match prepared.direction {
            TransferDirection::Download => complete_download(prepared),
            TransferDirection::Upload => complete_upload(prepared),
        }
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

fn transfer_revision(metadata: &std::fs::Metadata) -> TransferRevision {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;

    TransferRevision {
        bytes: metadata.len(),
        modified: metadata.modified().ok(),
        regular_file: metadata.is_file(),
        #[cfg(unix)]
        device: metadata.dev(),
        #[cfg(unix)]
        inode: metadata.ino(),
        #[cfg(unix)]
        change_seconds: metadata.ctime(),
        #[cfg(unix)]
        change_nanoseconds: metadata.ctime_nsec(),
    }
}

async fn verify_precondition(prepared: &PreparedTransfer) -> Result<(), &'static str> {
    let current = tokio::fs::metadata(&prepared.resource.path).await;
    match (&prepared.precondition, current) {
        (TransferPrecondition::Missing, Err(error))
            if error.kind() == std::io::ErrorKind::NotFound =>
        {
            Ok(())
        }
        (TransferPrecondition::Existing(expected), Ok(metadata))
            if !metadata.is_dir() && transfer_revision(&metadata) == *expected =>
        {
            Ok(())
        }
        _ => Err("Transfer resource changed after preparation"),
    }
}

fn complete_download(prepared: PreparedTransfer) -> Result<CallToolResult, ErrorData> {
    let TransferPrecondition::Existing(revision) = prepared.precondition else {
        return tool_error("Transfer resource changed after preparation");
    };
    let output = DownloadOutput {
        method: "GET",
        url: transfer_url(&prepared.relative_path),
        path: prepared.relative_path,
        name: prepared.name,
        bytes: revision.bytes,
    };
    let text = format!(
        "Prepared a download for `{}` ({} bytes). No bytes were transferred by this call.\n\nThe harness must now issue `GET {}` against the same origin as its MCP endpoint, reusing the credentials it already sends, and write the response body to the destination the user asked for.",
        output.name, output.bytes, output.url
    );
    success(&output, text)
}

fn complete_upload(prepared: PreparedTransfer) -> Result<CallToolResult, ErrorData> {
    let output = UploadOutput {
        method: "POST",
        url: transfer_url(&prepared.relative_path),
        path: prepared.relative_path,
        name: prepared.name,
        content_type: OCTET_STREAM,
        max_bytes: prepared.max_bytes,
    };
    let text = format!(
        "Prepared an upload for `{}`. No bytes were transferred by this call.\n\nThe harness must now issue `POST {}` against the same origin as its MCP endpoint, with `Content-Type: {}`, the raw bytes as the request body, and the credentials it already sends. Bodies over {} bytes are rejected.",
        output.name, output.url, OCTET_STREAM, output.max_bytes
    );
    success(&output, text)
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
