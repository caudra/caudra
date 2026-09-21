use std::{io, sync::Arc, time::Duration};

use axum::{
    body::Body,
    extract::Request,
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{
            ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG, IF_MATCH, IF_RANGE,
            RANGE,
        },
    },
    response::{IntoResponse, Response},
};
use futures_util::{StreamExt, stream};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    sync::{OwnedSemaphorePermit, mpsc},
    time::Instant,
};
use tokio_util::io::ReaderStream;
use workcell_host_contract::{
    Identifier, Revision, TRANSFER_IO_TIMEOUT_MS, TRANSFER_STREAM_BUFFER_BYTES,
};

use super::{
    OCTET_STREAM,
    reviewed::{ReviewedTransfers, Slot, SlotData, TransferError, hex_digest, lock},
};
use crate::transports::http::Authenticated;

const MAX_QUERY_BYTES: usize = 2048;
const CWD_HEADER: &str = "x-workcell-cwd";
const DIGEST_HEADER: &str = "x-workcell-sha256";
const DOWNLOAD_QUEUE_CHUNKS: usize = 1;
// Charge the queue, pending send, ReaderStream buffer and Tokio file's internal read copy.
const DOWNLOAD_CHUNK_BYTES: usize = TRANSFER_STREAM_BUFFER_BYTES / (DOWNLOAD_QUEUE_CHUNKS + 3);

fn selector(request: &Request, key: &str) -> Result<Identifier, StatusCode> {
    if request.extensions().get::<Authenticated>().is_none() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let query = request.uri().query().ok_or(StatusCode::BAD_REQUEST)?;
    if query.len() > MAX_QUERY_BYTES {
        return Err(StatusCode::BAD_REQUEST);
    }
    let pairs = url::form_urlencoded::parse(query.as_bytes()).collect::<Vec<_>>();
    if pairs.len() != 2
        || pairs
            .iter()
            .filter(|(name, value)| name == "reviewed" && value == "v1")
            .count()
            != 1
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    let value = pairs
        .iter()
        .find(|(name, _)| name == key)
        .ok_or(StatusCode::BAD_REQUEST)?;
    Identifier::new(value.1.to_string()).map_err(|_| StatusCode::BAD_REQUEST)
}

fn validate_cwd(request: &Request, slot: &Slot) -> Result<(), StatusCode> {
    if request.headers().get_all(CWD_HEADER).iter().count() != 1
        || request
            .headers()
            .get(CWD_HEADER)
            .and_then(|v| v.to_str().ok())
            != Some(slot.binding.cwd_handle.as_str())
    {
        return Err(StatusCode::PRECONDITION_FAILED);
    }
    Ok(())
}

pub(super) async fn upload(manager: Option<&ReviewedTransfers>, request: Request) -> Response {
    match upload_inner(manager, request).await {
        Ok(()) => (
            StatusCode::OK,
            axum::Json(serde_json::json!({"version":"v1", "staged":true, "published":false})),
        )
            .into_response(),
        Err(status) => error(status),
    }
}

async fn upload_inner(
    manager: Option<&ReviewedTransfers>,
    request: Request,
) -> Result<(), StatusCode> {
    let id = selector(&request, "stage")?;
    let manager = manager.ok_or(StatusCode::NOT_FOUND)?;
    let slot = manager.slot(&id).await.map_err(status)?;
    validate_cwd(&request, &slot)?;
    if request
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        != Some(OCTET_STREAM)
    {
        return Err(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    if let Some(length) = request.headers().get(CONTENT_LENGTH) {
        let length = length
            .to_str()
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .ok_or(StatusCode::BAD_REQUEST)?;
        if length > slot.size {
            return Err(StatusCode::PAYLOAD_TOO_LARGE);
        }
    }
    let _permit = manager.permit().map_err(status)?;
    let mut file = tokio::fs::File::from_std(manager.private_file().map_err(status)?);
    {
        let mut data = lock(&slot.data);
        if !matches!(*data, SlotData::Empty) {
            return Err(StatusCode::CONFLICT);
        }
        *data = SlotData::Uploading;
    }
    let work = async {
        let mut stream = request.into_body().into_data_stream();
        let mut size = 0u64;
        let mut digest = Sha256::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| StatusCode::BAD_REQUEST)?;
            size = size
                .checked_add(chunk.len() as u64)
                .filter(|size| *size <= slot.size)
                .ok_or(StatusCode::PAYLOAD_TOO_LARGE)?;
            digest.update(&chunk);
            file.write_all(&chunk)
                .await
                .map_err(|_| StatusCode::INSUFFICIENT_STORAGE)?;
        }
        file.sync_all()
            .await
            .map_err(|_| StatusCode::INSUFFICIENT_STORAGE)?;
        let digest = Revision::new(format!("sha256:{}", hex_digest(digest.finalize())))
            .map_err(|_| StatusCode::BAD_REQUEST)?;
        Ok((digest, size))
    };
    let result = tokio::select! {
        () = slot.cancel.cancelled() => Err(StatusCode::GONE),
        result = tokio::time::timeout(Duration::from_millis(TRANSFER_IO_TIMEOUT_MS), work) => result.unwrap_or(Err(StatusCode::REQUEST_TIMEOUT)),
    };
    match result {
        Ok((digest, size)) => {
            let file = file.into_std().await;
            if slot.cancel.is_cancelled() {
                return Err(StatusCode::GONE);
            }
            *lock(&slot.data) = SlotData::Uploaded { file, digest, size };
            Ok(())
        }
        Err(error) => {
            *lock(&slot.data) = SlotData::Claimed;
            Err(error)
        }
    }
}

pub(super) async fn download(manager: Option<&ReviewedTransfers>, request: Request) -> Response {
    match download_inner(manager, request).await {
        Ok(response) => response,
        Err(response) => *response,
    }
}

async fn download_inner(
    manager: Option<&ReviewedTransfers>,
    request: Request,
) -> Result<Response, Box<Response>> {
    let error = |status| Box::new(error(status));
    let id = selector(&request, "download").map_err(error)?;
    let manager = manager.ok_or_else(|| error(StatusCode::NOT_FOUND))?;
    let slot = manager.slot(&id).await.map_err(|e| error(status(e)))?;
    validate_cwd(&request, &slot).map_err(error)?;
    let (path, metadata) = match &*lock(&slot.data) {
        SlotData::Download { path, metadata } => (path.clone(), metadata.clone()),
        _ => return Err(error(StatusCode::CONFLICT)),
    };
    let etag = format!("\"{}\"", metadata.revision.as_str());
    if_match(request.headers(), &etag).map_err(error)?;
    let token = slot.cancel.child_token();
    let cancel = token.clone().drop_guard();
    let result = tokio::time::timeout(
        Duration::from_millis(TRANSFER_IO_TIMEOUT_MS),
        manager.stat(&slot.binding, &path, &token),
    )
    .await;
    let source = result
        .map_err(|_| error(StatusCode::REQUEST_TIMEOUT))?
        .map_err(|e| error(status(e)))?;
    drop(cancel);
    if source.metadata != metadata {
        return Err(error(StatusCode::PRECONDITION_FAILED));
    }
    let range = if request
        .headers()
        .get(IF_RANGE)
        .is_none_or(|value| value == etag.as_str())
    {
        request.headers().get(RANGE)
    } else {
        None
    };
    let selected = range.map(|range| {
        range
            .to_str()
            .ok()
            .and_then(|range| single_range(range, metadata.size_bytes))
    });
    if selected == Some(None) || request.headers().get_all(RANGE).iter().count() > 1 {
        let mut response = error(StatusCode::RANGE_NOT_SATISFIABLE);
        response.headers_mut().insert(
            CONTENT_RANGE,
            header(&format!("bytes */{}", metadata.size_bytes))?,
        );
        return Err(response);
    }
    let (start, length) = selected.flatten().unwrap_or((0, metadata.size_bytes));
    let permit = manager.permit().map_err(|e| error(status(e)))?;
    let mut file = tokio::fs::File::from_std(source.file);
    file.set_max_buf_size(DOWNLOAD_CHUNK_BYTES);
    file.seek(io::SeekFrom::Start(start))
        .await
        .map_err(|_| error(StatusCode::PRECONDITION_FAILED))?;
    let mut response = download_body(file.take(length), permit, slot).into_response();
    if selected.is_some() {
        *response.status_mut() = StatusCode::PARTIAL_CONTENT;
    }
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(OCTET_STREAM));
    headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(ETAG, header(&etag)?);
    headers.insert(DIGEST_HEADER, header(metadata.digest.as_str())?);
    headers.insert(CONTENT_LENGTH, header(&length.to_string())?);
    if selected.is_some() {
        headers.insert(
            CONTENT_RANGE,
            header(&format!(
                "bytes {start}-{}/{}",
                start + length - 1,
                metadata.size_bytes
            ))?,
        );
    }
    Ok(response)
}

pub(super) fn download_body(
    reader: impl AsyncRead + Unpin + Send + 'static,
    permit: OwnedSemaphorePermit,
    slot: Arc<Slot>,
) -> Body {
    let deadline = Instant::now() + Duration::from_millis(TRANSFER_IO_TIMEOUT_MS);
    let (sender, receiver) = mpsc::channel(DOWNLOAD_QUEUE_CHUNKS);
    let producer = tokio::spawn(async move {
        let _permit = permit;
        let mut reader = ReaderStream::with_capacity(reader, DOWNLOAD_CHUNK_BYTES);
        tokio::select! {
            biased;
            () = slot.cancel.cancelled() => Err(io::Error::from(io::ErrorKind::Interrupted)),
            () = tokio::time::sleep_until(deadline) => Err(io::Error::from(io::ErrorKind::TimedOut)),
            () = sender.closed() => Ok(()),
            result = async {
                while let Some(chunk) = reader.next().await {
                    sender.send(chunk?).await.map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
                }
                Ok(())
            } => result,
        }
    });
    Body::from_stream(stream::try_unfold(
        (receiver, producer),
        |(mut receiver, producer)| async move {
            if let Some(chunk) = receiver.recv().await {
                return Ok(Some((chunk, (receiver, producer))));
            }
            producer
                .await
                .map_err(|_| io::Error::other("reviewed download task failed"))??;
            Ok::<_, io::Error>(None)
        },
    ))
}

fn if_match(headers: &HeaderMap, etag: &str) -> Result<(), StatusCode> {
    let value = headers
        .get(IF_MATCH)
        .ok_or(StatusCode::PRECONDITION_REQUIRED)?;
    if headers.get_all(IF_MATCH).iter().count() != 1 {
        return Err(StatusCode::BAD_REQUEST);
    }
    let value = value.to_str().map_err(|_| StatusCode::BAD_REQUEST)?;
    if value.len() > MAX_QUERY_BYTES {
        return Err(StatusCode::BAD_REQUEST);
    }
    if value.trim() == "*" || value.split(',').any(|item| item.trim() == etag) {
        Ok(())
    } else {
        Err(StatusCode::PRECONDITION_FAILED)
    }
}

fn single_range(value: &str, size: u64) -> Option<(u64, u64)> {
    if size == 0 {
        return None;
    }
    let value = value.strip_prefix("bytes=")?;
    let (start, end) = value.split_once('-')?;
    if !start
        .bytes()
        .chain(end.bytes())
        .all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    if start.is_empty() {
        let suffix = end.parse::<u64>().ok()?.min(size);
        return (suffix > 0).then_some((size - suffix, suffix));
    }
    let start = start.parse::<u64>().ok()?;
    let end = if end.is_empty() {
        size - 1
    } else {
        end.parse::<u64>().ok()?.min(size - 1)
    };
    (start <= end && start < size).then(|| (start, end - start + 1))
}

fn header(value: &str) -> Result<HeaderValue, Box<Response>> {
    HeaderValue::from_str(value).map_err(|_| Box::new(error(StatusCode::INTERNAL_SERVER_ERROR)))
}

fn status(error: TransferError) -> StatusCode {
    match error {
        TransferError::Binding => StatusCode::FORBIDDEN,
        TransferError::Missing => StatusCode::GONE,
        TransferError::Limit => StatusCode::TOO_MANY_REQUESTS,
        TransferError::Storage => StatusCode::INSUFFICIENT_STORAGE,
        TransferError::Cancelled => StatusCode::REQUEST_TIMEOUT,
        TransferError::Binary(_) => StatusCode::PRECONDITION_FAILED,
        _ => StatusCode::CONFLICT,
    }
}

fn error(status: StatusCode) -> Response {
    (status, axum::Json(serde_json::json!({"version":"v1", "code":status.as_u16(), "message":"Reviewed transfer request refused."}))).into_response()
}
