use std::fmt;
use std::future::poll_fn;
use std::io;
use std::pin::Pin;
use std::task::{Poll, ready};

use agent_client_protocol_schema::v1::RequestId;
use flume::Receiver;
use serde::Deserializer;
use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, Visitor};
use serde_json::Value;
use smol::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt};
use tracing::warn;

use crate::outbound::Frame;

pub(crate) const MAX_LINE_BYTES: usize = 64 * 1024 * 1024;
/// Enough of an oversized line to find the id of a request that leads with it.
const OVERSIZED_HEAD_BYTES: usize = 64 * 1024;
const NEWLINE: u8 = b'\n';
const ID_FIELD: &str = "id";
const METHOD_FIELD: &str = "method";

pub(crate) enum Inbound {
    Message(Value),
    /// A request too large to read, answered so its caller does not wait forever.
    Oversized(RequestId),
}

/// Delivers each line until `deliver` declines one or input ends. A line that
/// is not JSON, or too long to hold, is skipped so one bad line cannot end the
/// session for everyone.
pub(crate) async fn read_messages(
    mut reader: impl AsyncBufRead + Unpin,
    max_line: usize,
    mut deliver: impl FnMut(Inbound) -> bool,
) -> io::Result<()> {
    let mut line = Vec::new();
    while let Some(overflowed) = read_capped_line(&mut reader, max_line, &mut line).await? {
        let inbound = if overflowed {
            oversized_request(&line)
        } else {
            parse_line(&line)
        };
        if let Some(inbound) = inbound
            && !deliver(inbound)
        {
            break;
        }
        line.clear();
    }
    Ok(())
}

/// Reads through the next newline into `line`, keeping only a head once the
/// line outgrows `max_line`. `None` at end of input, else whether it overflowed.
async fn read_capped_line(
    reader: &mut (impl AsyncBufRead + Unpin),
    max_line: usize,
    line: &mut Vec<u8>,
) -> io::Result<Option<bool>> {
    let mut overflowed = false;
    let mut read_any = false;
    loop {
        // One poll per fill: `fill_buf` polls twice and panics when the second
        // poll is pending, which `Unblock` is once it has reported end of input.
        let complete = poll_fn(|cx| {
            let available = ready!(Pin::new(&mut *reader).poll_fill_buf(cx))?;
            if available.is_empty() {
                return Poll::Ready(Ok::<_, io::Error>(None));
            }
            let (chunk, complete) = match available.iter().position(|&byte| byte == NEWLINE) {
                Some(end) => (&available[..end], true),
                None => (available, false),
            };
            let consumed = chunk.len() + usize::from(complete);
            if !overflowed {
                if line.len() + chunk.len() > max_line {
                    let head = max_line.min(OVERSIZED_HEAD_BYTES);
                    let wanted = head.saturating_sub(line.len());
                    line.extend_from_slice(&chunk[..wanted]);
                    line.truncate(head);
                    overflowed = true;
                } else {
                    line.extend_from_slice(chunk);
                }
            }
            Pin::new(&mut *reader).consume(consumed);
            Poll::Ready(Ok(Some(complete)))
        })
        .await?;
        match complete {
            None => return Ok(read_any.then_some(overflowed)),
            Some(true) => return Ok(Some(overflowed)),
            Some(false) => read_any = true,
        }
    }
}

fn parse_line(line: &[u8]) -> Option<Inbound> {
    let trimmed = line.trim_ascii();
    if trimmed.is_empty() {
        return None;
    }
    match serde_json::from_slice(trimmed) {
        Ok(message) => Some(Inbound::Message(message)),
        Err(error) => {
            warn!(%error, bytes = trimmed.len(), "skipped a stdin line that is not JSON");
            None
        }
    }
}

fn oversized_request(head: &[u8]) -> Option<Inbound> {
    let mut found = RequestHead::default();
    // The head is cut mid-value, so the parse always fails once past the
    // fields it could read; those it did read are all that matter.
    let _ = (&mut found).deserialize(&mut serde_json::Deserializer::from_slice(head));
    match found {
        RequestHead {
            id: Some(id),
            method: true,
        } => {
            warn!(?id, "rejected a request over the stdin line limit");
            Some(Inbound::Oversized(id))
        }
        _ => {
            warn!("skipped a stdin line over the limit without a readable request id");
            None
        }
    }
}

#[derive(Default)]
struct RequestHead {
    id: Option<RequestId>,
    method: bool,
}

impl<'de> DeserializeSeed<'de> for &mut RequestHead {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for &mut RequestHead {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a JSON-RPC message")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                ID_FIELD => self.id = Some(map.next_value()?),
                METHOD_FIELD => {
                    map.next_value::<IgnoredAny>()?;
                    self.method = true;
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(())
    }
}

/// Ends with the first failed write: a client that stopped reading is gone.
pub(crate) async fn write_frames(
    frames: Receiver<Frame>,
    mut out: impl AsyncWrite + Unpin,
) -> io::Result<()> {
    let mut buffer = Vec::new();
    while let Ok(frame) = frames.recv_async().await {
        buffer.clear();
        serde_json::to_writer(&mut buffer, &frame.message)?;
        buffer.push(NEWLINE);
        out.write_all(&buffer).await?;
        out.flush().await?;
        frame.mark_written();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::task::Context;

    use agent_client_protocol_schema::v1::{AgentResponse, SetSessionModeResponse};
    use serde_json::json;
    use smol::io::{AsyncRead, Cursor};
    use test_case::test_case;

    use super::*;
    use crate::outbound::Outbound;

    const SMALL_LIMIT: usize = 64;
    const OVERSIZED_ID: i64 = 7;

    fn read(input: &[u8], max_line: usize) -> Vec<Inbound> {
        let mut delivered = Vec::new();
        smol::block_on(read_messages(Cursor::new(input), max_line, |inbound| {
            delivered.push(inbound);
            true
        }))
        .unwrap();
        delivered
    }

    fn messages(delivered: &[Inbound]) -> Vec<Value> {
        delivered
            .iter()
            .filter_map(|inbound| match inbound {
                Inbound::Message(message) => Some(message.clone()),
                Inbound::Oversized(_) => None,
            })
            .collect()
    }

    fn padding() -> String {
        "x".repeat(SMALL_LIMIT * 2)
    }

    #[test]
    fn stray_lines_are_skipped_and_reading_goes_on() {
        let input = b"not json\n\n   \n{\"id\":1}\n\xff\xfe\n{\"id\":2}";
        let delivered = read(input, MAX_LINE_BYTES);
        assert_eq!(messages(&delivered), [json!({"id": 1}), json!({"id": 2})]);
    }

    #[test_case(format!(r#"{{"jsonrpc":"2.0","id":{OVERSIZED_ID},"method":"session/prompt","params":{{"pad":"{}"}}}}"#, padding()), Some(OVERSIZED_ID) ; "request_leading_with_its_id")]
    #[test_case(format!(r#"{{"jsonrpc":"2.0","method":"session/prompt","params":{{"pad":"{}"}},"id":{OVERSIZED_ID}}}"#, padding()), None ; "id_beyond_the_head")]
    #[test_case(format!(r#"{{"jsonrpc":"2.0","id":{OVERSIZED_ID},"result":{{"pad":"{}"}}}}"#, padding()), None ; "client_response")]
    fn an_oversized_line_is_answered_only_when_its_request_id_is_readable(
        message: String,
        answered: Option<i64>,
    ) {
        let mut input = message.into_bytes();
        input.extend_from_slice(b"\n{\"id\":3}\n");
        let delivered = read(&input, SMALL_LIMIT);
        let oversized: Vec<_> = delivered
            .iter()
            .filter_map(|inbound| match inbound {
                Inbound::Oversized(id) => Some(id.clone()),
                Inbound::Message(_) => None,
            })
            .collect();
        assert_eq!(oversized, Vec::from_iter(answered.map(RequestId::Number)));
        assert_eq!(messages(&delivered), [json!({"id": 3})]);
    }

    /// Reports end of input once, then goes pending on the next poll, as
    /// `Unblock` does while it starts another read.
    struct PendingAfterEnd {
        input: Cursor<Vec<u8>>,
        ended: bool,
    }

    impl AsyncRead for PendingAfterEnd {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            unreachable!("read through the buffer")
        }
    }

    impl AsyncBufRead for PendingAfterEnd {
        fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
            let this = self.get_mut();
            if this.ended {
                return Poll::Pending;
            }
            let available = ready!(Pin::new(&mut this.input).poll_fill_buf(cx))?;
            this.ended = available.is_empty();
            Poll::Ready(Ok(available))
        }

        fn consume(mut self: Pin<&mut Self>, amount: usize) {
            Pin::new(&mut self.input).consume(amount);
        }
    }

    #[test]
    fn end_of_input_ends_reading_when_polling_again_would_wait() {
        let reader = PendingAfterEnd {
            input: Cursor::new(b"{\"id\":1}\n".to_vec()),
            ended: false,
        };
        let mut delivered = Vec::new();
        smol::block_on(read_messages(reader, MAX_LINE_BYTES, |inbound| {
            delivered.push(inbound);
            true
        }))
        .unwrap();
        assert_eq!(messages(&delivered), [json!({"id": 1})]);
    }

    #[test]
    fn a_line_at_the_limit_is_read_whole() {
        let overhead = json!({"pad": ""}).to_string().len();
        let message = json!({"pad": "y".repeat(SMALL_LIMIT - overhead)}).to_string();
        assert_eq!(message.len(), SMALL_LIMIT);
        let delivered = read(format!("{message}\n").as_bytes(), SMALL_LIMIT);
        assert_eq!(messages(&delivered).len(), 1);
    }

    #[test]
    fn written_settles_once_the_frame_is_flushed() {
        let (outbound, frames) = Outbound::channel();
        let written = outbound.respond_written(
            RequestId::Number(1),
            Ok(AgentResponse::SetSessionModeResponse(
                SetSessionModeResponse::new(),
            )),
        );
        drop(outbound);
        let mut out = Vec::new();
        smol::block_on(write_frames(frames, &mut out)).unwrap();
        smol::block_on(written.wait());
        let line: Value = serde_json::from_slice(out.trim_ascii_end()).unwrap();
        assert_eq!(line["id"], 1);
    }

    struct BrokenPipe;

    impl AsyncWrite for BrokenPipe {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()))
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn a_failed_write_ends_the_writer() {
        let (outbound, frames) = Outbound::channel();
        outbound.send(json!({}));
        let error = smol::block_on(write_frames(frames, BrokenPipe)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }
}
