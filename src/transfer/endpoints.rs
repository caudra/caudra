//! Byte-moving half of the transfer group.
//!
//! Every handler re-resolves and re-authorizes the requested path. A URL minted by `file_download`
//! or `file_upload` is an affordance, not a capability: it carries no signature and grants nothing
//! the caller's credentials did not already grant, so a stale or hand-written URL is treated exactly
//! like a fresh one.

use std::path::{Path, PathBuf};

use axum::{
    body::Body,
    extract::{Request, State},
    http::{
        StatusCode, Uri,
        header::{CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE},
    },
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;
use uuid::Uuid;
use workcell_mcp_files::FileResourceAccess;

use super::{OCTET_STREAM, TransferToolGroup};

/// Bounds the `Content-Disposition` file name. Long enough for any real name, short enough that a
/// pathological one cannot dominate the response headers.
const MAX_FILENAME_BYTES: usize = 128;

pub async fn download(State(group): State<TransferToolGroup>, uri: Uri) -> Response {
    let Some(requested) = query_path(&uri) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "A path query parameter is required.",
        );
    };
    let Ok(resource) = group
        .files
        .inspect_path(&requested, FileResourceAccess::Read)
        .await
    else {
        return error_response(StatusCode::BAD_REQUEST, "Path is not accessible.");
    };
    // Open first, then measure the open handle. Stat-then-open would let the file change underneath
    // the response and produce a Content-Length that disagrees with the body.
    let file = match tokio::fs::File::open(&resource.path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return error_response(StatusCode::NOT_FOUND, "Path does not exist.");
        }
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "Path cannot be opened."),
    };
    let metadata = match file.metadata().await {
        Ok(metadata) => metadata,
        Err(_) => {
            return error_response(StatusCode::BAD_REQUEST, "Path cannot be inspected.");
        }
    };
    if metadata.is_dir() {
        return error_response(StatusCode::BAD_REQUEST, "Path is a directory.");
    }
    if !metadata.is_file() {
        return error_response(StatusCode::BAD_REQUEST, "Path is not a regular file.");
    }
    let bytes = metadata.len();
    if bytes > group.max_transfer_bytes as u64 {
        return error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "File exceeds the configured transfer limit.",
        );
    }
    let mut response = Body::from_stream(ReaderStream::new(file)).into_response();
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, header_value(OCTET_STREAM));
    headers.insert(CONTENT_LENGTH, header_value(&bytes.to_string()));
    if let Some(name) = disposition_filename(&resource.path) {
        headers.insert(
            CONTENT_DISPOSITION,
            header_value(&format!("attachment; filename=\"{name}\"")),
        );
    } else {
        headers.insert(CONTENT_DISPOSITION, header_value("attachment"));
    }
    response
}

pub async fn upload(State(group): State<TransferToolGroup>, request: Request) -> Response {
    if !group.files.allow_write() {
        return error_response(
            StatusCode::FORBIDDEN,
            "This server was started without --allow-write.",
        );
    }
    let Some(requested) = query_path(request.uri()) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "A path query parameter is required.",
        );
    };
    if !is_octet_stream(&request) {
        return error_response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Content-Type must be application/octet-stream.",
        );
    }
    // Reject an oversized upload from its declared length before reading a single byte of it.
    if let Some(declared) = request
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        && declared > group.max_transfer_bytes as u64
    {
        return error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Body exceeds the configured transfer limit.",
        );
    }
    let Ok(resource) = group
        .files
        .inspect_path(&requested, FileResourceAccess::Write)
        .await
    else {
        return error_response(StatusCode::BAD_REQUEST, "Path is not accessible.");
    };
    if let Ok(metadata) = tokio::fs::metadata(&resource.path).await
        && metadata.is_dir()
    {
        return error_response(StatusCode::BAD_REQUEST, "Path is a directory.");
    }
    let Some(parent) = resource.path.parent().map(Path::to_path_buf) else {
        return error_response(StatusCode::BAD_REQUEST, "Path has no parent directory.");
    };
    if tokio::fs::create_dir_all(&parent).await.is_err() {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Destination directory cannot be created.",
        );
    }
    // Stage beside the destination so the rename is same-filesystem and therefore atomic. A partial
    // or abandoned transfer leaves only this hidden staging file, which is removed on every failure
    // path, and never a truncated file at the destination.
    let staging = parent.join(format!(".workcell-upload-{}.part", Uuid::new_v4()));
    match stream_to_file(request, &staging, group.max_transfer_bytes).await {
        Ok(written) => {
            if tokio::fs::rename(&staging, &resource.path).await.is_err() {
                remove(&staging).await;
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Uploaded file cannot be published.",
                );
            }
            let name = resource
                .path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            (
                StatusCode::OK,
                [(CONTENT_TYPE, "application/json; charset=utf-8")],
                json!({
                    "path": group.relative(&resource.path),
                    "name": name,
                    "bytes": written,
                })
                .to_string(),
            )
                .into_response()
        }
        Err(status) => {
            remove(&staging).await;
            let message = match status {
                StatusCode::PAYLOAD_TOO_LARGE => "Body exceeds the configured transfer limit.",
                StatusCode::BAD_REQUEST => "Request body could not be read.",
                _ => "Uploaded file could not be written.",
            };
            error_response(status, message)
        }
    }
}

async fn stream_to_file(
    request: Request,
    staging: &PathBuf,
    max_bytes: usize,
) -> Result<u64, StatusCode> {
    let mut file = tokio::fs::File::create(staging)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut stream = request.into_body().into_data_stream();
    let mut written: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| StatusCode::BAD_REQUEST)?;
        written = written.saturating_add(chunk.len() as u64);
        if written > max_bytes as u64 {
            return Err(StatusCode::PAYLOAD_TOO_LARGE);
        }
        file.write_all(&chunk)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }
    file.flush()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    // Close before renaming so the published file is complete on every platform.
    drop(file);
    Ok(written)
}

async fn remove(path: &PathBuf) {
    let _ = tokio::fs::remove_file(path).await;
}

fn query_path(uri: &Uri) -> Option<String> {
    let query = uri.query()?;
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == "path")
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.trim().is_empty())
}

fn is_octet_stream(request: &Request) -> bool {
    request
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|media| media.trim().eq_ignore_ascii_case(OCTET_STREAM))
        })
}

/// Restricts the name to printable ASCII minus the quoting and path characters that would let a file
/// name inject header structure or a directory traversal into a client's save path.
fn disposition_filename(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_string_lossy();
    // Every retained character is ASCII, so the character bound is also the byte bound.
    let filtered: String = name
        .chars()
        .filter(|candidate| candidate.is_ascii_graphic() || *candidate == ' ')
        .filter(|candidate| !matches!(candidate, '"' | '\\' | '/' | ';'))
        .take(MAX_FILENAME_BYTES)
        .collect();
    let trimmed = filtered.trim().to_owned();
    (!trimmed.is_empty() && trimmed != "." && trimmed != "..").then_some(trimmed)
}

fn header_value(value: &str) -> axum::http::HeaderValue {
    axum::http::HeaderValue::from_str(value)
        .unwrap_or_else(|_| axum::http::HeaderValue::from_static("attachment"))
}

/// Deliberately not the JSON-RPC envelope used by `/mcp`: this route is not a protocol endpoint, and
/// a caller that received a JSON-RPC error here would have to guess which contract it was speaking.
fn error_response(status: StatusCode, message: &'static str) -> Response {
    (
        status,
        [(CONTENT_TYPE, "application/json; charset=utf-8")],
        json!({ "message": message, "code": status.as_u16() }).to_string(),
    )
        .into_response()
}
