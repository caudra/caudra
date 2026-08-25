use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use http::{HeaderMap, StatusCode};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::deadline::run_until;
use crate::{NetError, TransportResponse};

pub(crate) struct ReadBody {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
    pub(crate) bytes: Bytes,
    pub(crate) truncated: bool,
}

pub(crate) async fn read_bounded_body(
    response: TransportResponse,
    limit: usize,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<ReadBody, NetError> {
    let status = response.status;
    let headers = response.headers;
    let mut stream = response.body;
    let mut output = BytesMut::with_capacity(limit.min(16 * 1024));
    let mut truncated = false;
    loop {
        // Content-Length is only a hint. The stream itself is cut off once the
        // cap is reached, so a lying or absent header cannot grow memory usage.
        let chunk = run_until(deadline, cancellation, stream.next()).await?;
        let Some(chunk) = chunk else { break };
        let chunk = chunk?;
        let remaining = limit.saturating_sub(output.len());
        if chunk.len() > remaining {
            output.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            break;
        }
        output.extend_from_slice(&chunk);
    }
    Ok(ReadBody {
        status,
        headers,
        bytes: output.freeze(),
        truncated,
    })
}
