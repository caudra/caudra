//! Bounded process-output ingestion and tail capture.
//!
//! Readers incrementally decode UTF-8 so code points split across OS reads are not corrupted, then
//! coalesce tiny reads to avoid flooding progress delivery. Resource limits use raw source bytes;
//! decoded replacement characters must not let malformed output evade the combined byte budget.

use crate::types::{OutputEvent, Stream};
use std::collections::VecDeque;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    sync::mpsc,
};

pub(crate) const STREAM_CAPTURE_BYTES: usize = 1_048_576;
pub(crate) const FALLBACK_PREVIEW_BYTES: usize = 24 * 1024;
const CHUNK_BYTES: usize = 16 * 1024;
const CHUNK_IDLE_FLUSH: std::time::Duration = std::time::Duration::from_millis(10);
#[cfg(not(test))]
pub(crate) const COMBINED_OUTPUT_BYTES: u64 = 100 * 1024 * 1024;
// Keep lifecycle coverage fast while exercising the same production limit path.
#[cfg(test)]
pub(crate) const COMBINED_OUTPUT_BYTES: u64 = 2 * 1024 * 1024;
pub(crate) const OUTPUT_CHANNEL_CAPACITY: usize = 32;

#[derive(Default)]
pub(crate) struct Tail {
    bytes: VecDeque<u8>,
    pub(crate) truncated: bool,
}
impl Tail {
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        // Retain the newest bytes because command failures and summaries conventionally appear last.
        if bytes.len() >= STREAM_CAPTURE_BYTES {
            self.bytes = bytes[bytes.len() - STREAM_CAPTURE_BYTES..]
                .iter()
                .copied()
                .collect();
            self.truncated = true;
            return;
        }
        while self.bytes.len() + bytes.len() > STREAM_CAPTURE_BYTES {
            self.bytes.pop_front();
            self.truncated = true;
        }
        self.bytes.extend(bytes);
    }
    pub(crate) fn preview(&self, limit: usize) -> (String, bool) {
        let all = self.bytes.iter().copied().collect::<Vec<_>>();
        let truncated = all.len() > limit;
        let slice = if truncated {
            &all[all.len() - limit..]
        } else {
            &all
        };
        // A byte-boundary cut may split a code point; lossy conversion keeps the preview valid UTF-8.
        (String::from_utf8_lossy(slice).into_owned(), truncated)
    }
}

pub(crate) async fn read_stream(
    mut reader: impl AsyncRead + Unpin,
    stream: Stream,
    sender: mpsc::Sender<OutputEvent>,
) {
    let mut buffer = [0_u8; CHUNK_BYTES];
    let mut pending_bytes = Vec::new();
    let mut pending_text = String::new();
    let mut pending_raw_bytes = 0;
    loop {
        let read = reader.read(&mut buffer);
        tokio::pin!(read);
        let count = if pending_text.is_empty() {
            read.await
        } else {
            tokio::select! {
                result = &mut read => result,
                // Flush interactive output promptly without emitting one progress event per tiny read.
                () = tokio::time::sleep(CHUNK_IDLE_FLUSH) => {
                    if !emit_remainder(
                        &mut pending_text,
                        &mut pending_raw_bytes,
                        stream,
                        &sender,
                    ).await {
                        return;
                    }
                    continue;
                }
            }
        };
        let Ok(count) = count else { break };
        if count == 0 {
            break;
        }
        pending_raw_bytes += count;
        pending_bytes.extend_from_slice(&buffer[..count]);
        let consumed = decode_available(&pending_bytes, &mut pending_text);
        pending_bytes.drain(..consumed);
        if !emit_full_chunks(&mut pending_text, &mut pending_raw_bytes, stream, &sender).await {
            return;
        }
    }
    if !pending_bytes.is_empty() {
        pending_text.push_str(&String::from_utf8_lossy(&pending_bytes));
    }
    let _ = emit_remainder(&mut pending_text, &mut pending_raw_bytes, stream, &sender).await;
}

fn decode_available(bytes: &[u8], text: &mut String) -> usize {
    // An incomplete suffix stays buffered for the next read; a proven-invalid sequence is replaced
    // now so later valid bytes continue to make progress.
    match std::str::from_utf8(bytes) {
        Ok(decoded) => {
            text.push_str(decoded);
            bytes.len()
        }
        Err(error) if error.error_len().is_none() => {
            let valid = error.valid_up_to();
            text.push_str(std::str::from_utf8(&bytes[..valid]).expect("validated prefix"));
            valid
        }
        Err(error) => {
            let end = error.valid_up_to() + error.error_len().unwrap_or(1);
            text.push_str(&String::from_utf8_lossy(&bytes[..end]));
            end
        }
    }
}

async fn emit_full_chunks(
    text: &mut String,
    raw_bytes: &mut usize,
    stream: Stream,
    sender: &mpsc::Sender<OutputEvent>,
) -> bool {
    while text.len() >= CHUNK_BYTES {
        // String lengths are bytes, so retreat to a character boundary before splitting.
        let mut end = CHUNK_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let remainder = text.split_off(end);
        let chunk = std::mem::replace(text, remainder);
        if sender
            .send(OutputEvent {
                stream,
                text: chunk,
                // Raw bytes are charged once, even when decoding changed their rendered length.
                raw_bytes: std::mem::take(raw_bytes),
            })
            .await
            .is_err()
        {
            return false;
        }
    }
    true
}

async fn emit_remainder(
    text: &mut String,
    raw_bytes: &mut usize,
    stream: Stream,
    sender: &mpsc::Sender<OutputEvent>,
) -> bool {
    if text.is_empty() {
        return true;
    }
    sender
        .send(OutputEvent {
            stream,
            text: std::mem::take(text),
            raw_bytes: std::mem::take(raw_bytes),
        })
        .await
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        pin::Pin,
        task::{Context, Poll},
    };
    use tokio::io::{AsyncRead, ReadBuf};

    struct TinyReads {
        bytes: Vec<u8>,
        offset: usize,
    }

    impl AsyncRead for TinyReads {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buffer: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            if let Some(byte) = self.bytes.get(self.offset).copied() {
                buffer.put_slice(&[byte]);
                self.offset += 1;
            }
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn tiny_reads_coalesce_into_bounded_exact_events() {
        let expected = "aé🙂".repeat(10_000);
        let raw = expected.as_bytes().to_vec();
        let raw_len = raw.len();
        let (sender, mut receiver) = mpsc::channel(OUTPUT_CHANNEL_CAPACITY);
        let reader = tokio::spawn(read_stream(
            TinyReads {
                bytes: raw,
                offset: 0,
            },
            Stream::Stdout,
            sender,
        ));

        let mut events = Vec::new();
        while let Some(event) = receiver.recv().await {
            events.push(event);
        }
        reader.await.unwrap();

        assert!(events.len() <= raw_len.div_ceil(CHUNK_BYTES));
        assert!(events.iter().all(|event| event.text.len() <= CHUNK_BYTES));
        assert_eq!(
            events.iter().map(|event| event.raw_bytes).sum::<usize>(),
            raw_len
        );
        assert_eq!(
            events
                .into_iter()
                .map(|event| event.text)
                .collect::<String>(),
            expected
        );
    }
}
