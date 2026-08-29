//! Ordered, bounded delivery of live shell output to host-neutral progress sinks.
//!
//! The queue bounds memory and decouples pipe reads from network latency. Saturation is a hard
//! execution failure rather than permission to drop chunks: sequence continuity means consumers can
//! trust that a successful call's progress stream is complete and ordered.

use crate::types::{ShellProgressChunk, ShellStream};
use async_trait::async_trait;
#[cfg(feature = "mcp")]
use rmcp::{
    RoleServer,
    model::{
        JsonObject, MetaObject, NotificationMetaObject, ProgressNotificationParam, ProgressToken,
    },
    service::Peer,
};
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc;

#[cfg(feature = "mcp")]
const OUTPUT_CHUNK_KEY: &str = "ai.workcell/tool-output-chunk";
const PROGRESS_QUEUE_CAPACITY: usize = 32;
const PROGRESS_DELIVERY_TIMEOUT: Duration = Duration::from_secs(2);
const PROGRESS_DRAIN_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(feature = "mcp")]
const STANDARD_MESSAGE_BYTES: usize = 16 * 1024;

#[async_trait]
pub trait ShellProgressSink: Send + Sync {
    async fn publish(&self, chunk: ShellProgressChunk) -> Result<(), String>;
}
#[cfg(feature = "mcp")]
pub(crate) struct McpProgressSink {
    pub(crate) peer: Peer<RoleServer>,
    pub(crate) token: ProgressToken,
}
#[async_trait]
#[cfg(feature = "mcp")]
impl ShellProgressSink for McpProgressSink {
    async fn publish(&self, chunk: ShellProgressChunk) -> Result<(), String> {
        self.peer
            .notify_progress(notification(&self.token, &chunk))
            .await
            .map_err(|_| "progress delivery failed".to_owned())
    }
}
pub(crate) struct ProgressPump {
    sender: mpsc::Sender<ShellProgressChunk>,
    pub(crate) failure: mpsc::Receiver<String>,
    pub(crate) task: tokio::task::JoinHandle<Result<(), String>>,
}
impl ProgressPump {
    pub(crate) fn start(sink: Arc<dyn ShellProgressSink>) -> Self {
        let (sender, mut receiver) = mpsc::channel::<ShellProgressChunk>(PROGRESS_QUEUE_CAPACITY);
        let (failure_sender, failure) = mpsc::channel(1);
        let task = tokio::spawn(async move {
            // A single worker preserves stdout/stderr event order assigned by the orchestrator.
            while let Some(message) = receiver.recv().await {
                let delivery =
                    tokio::time::timeout(PROGRESS_DELIVERY_TIMEOUT, sink.publish(message))
                        .await
                        .map_err(|_| "progress delivery timed out".to_owned())
                        .and_then(|r| r);
                if let Err(error) = delivery {
                    let _ = failure_sender.try_send(error.clone());
                    return Err(error);
                }
            }
            Ok(())
        });
        Self {
            sender,
            failure,
            task,
        }
    }
    pub(crate) fn enqueue(
        &self,
        sequence: u64,
        stream: ShellStream,
        text: &str,
    ) -> Result<(), String> {
        // Never await here: blocking the orchestrator could stop pipe draining and deadlock a child.
        // Instead, a full bounded queue aborts execution with an explicit delivery failure.
        self.sender
            .try_send(ShellProgressChunk {
                version: 1,
                sequence,
                stream,
                text: text.to_owned(),
            })
            .map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => "progress queue is saturated".to_owned(),
                mpsc::error::TrySendError::Closed(_) => "progress delivery stopped".to_owned(),
            })
    }
    pub(crate) async fn finish(mut self) -> Result<(), String> {
        // Closing the producer before joining establishes final-result ordering: every accepted
        // chunk is delivered (or the call fails) before the structured result is returned.
        drop(self.sender);
        match tokio::time::timeout(PROGRESS_DRAIN_TIMEOUT, &mut self.task).await {
            Ok(result) => result.map_err(|_| "progress worker failed".to_owned())?,
            Err(_) => {
                self.task.abort();
                let _ = self.task.await;
                Err("progress drain timed out".to_owned())
            }
        }
    }
}
#[cfg(feature = "mcp")]
fn notification(token: &ProgressToken, chunk: &ShellProgressChunk) -> ProgressNotificationParam {
    let mut fields = JsonObject::new();
    fields.insert(
        OUTPUT_CHUNK_KEY.to_owned(),
        serde_json::to_value(chunk).expect("output chunk serializes"),
    );
    let mut n = ProgressNotificationParam::new(token.clone(), chunk.sequence as f64)
        .with_message(standard_message(chunk));
    n.meta = Some(NotificationMetaObject(MetaObject(fields)));
    n
}

#[cfg(feature = "mcp")]
fn standard_message(chunk: &ShellProgressChunk) -> String {
    let label = match chunk.stream {
        ShellStream::Stdout => "stdout",
        ShellStream::Stderr => "stderr",
    };
    let mut rendered = format!("[{label}] ");
    for character in chunk.text.chars() {
        let escaped = match character {
            '\0' => Some("\\0".to_owned()),
            '\n' => Some("\\n".to_owned()),
            '\r' => Some("\\r".to_owned()),
            '\t' => Some("\\t".to_owned()),
            character if character.is_ascii_control() => {
                Some(format!("\\x{:02x}", character as u32))
            }
            character if character.is_control() => Some(format!("\\u{{{:x}}}", character as u32)),
            '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{2028}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}' => Some(format!("\\u{{{:x}}}", character as u32)),
            _ => None,
        };
        let mut encoded = [0; 4];
        let piece = escaped
            .as_deref()
            .unwrap_or_else(|| character.encode_utf8(&mut encoded));
        if rendered.len() + piece.len() > STANDARD_MESSAGE_BYTES - 3 {
            rendered.push_str("...");
            break;
        }
        rendered.push_str(piece);
    }
    rendered
}
pub(crate) async fn receive_failure(progress: &mut Option<ProgressPump>) -> String {
    progress
        .as_mut()
        .expect("guarded by progress presence")
        .failure
        .recv()
        .await
        .unwrap_or_else(|| "progress delivery stopped".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Recording {
        chunks: Arc<Mutex<Vec<ShellProgressChunk>>>,
    }

    #[async_trait]
    impl ShellProgressSink for Recording {
        async fn publish(&self, chunk: ShellProgressChunk) -> Result<(), String> {
            self.chunks.lock().unwrap().push(chunk);
            Ok(())
        }
    }

    #[test]
    #[cfg(feature = "mcp")]
    fn notification_has_standard_message_and_structured_chunk() {
        let notification = notification(
            &ProgressToken(rmcp::model::NumberOrString::String("request-1".into())),
            &ShellProgressChunk {
                version: 1,
                sequence: 4,
                stream: ShellStream::Stderr,
                text: "compiling\n".into(),
            },
        );
        let value = serde_json::to_value(notification).unwrap();

        assert_eq!(value["progressToken"], "request-1");
        assert_eq!(value["progress"], 4.0);
        assert_eq!(value["message"], "[stderr] compiling\\n");
        assert_eq!(
            value["_meta"][OUTPUT_CHUNK_KEY],
            serde_json::json!({
                "version": 1,
                "sequence": 4,
                "stream": "stderr",
                "text": "compiling\n"
            })
        );
    }

    #[test]
    #[cfg(feature = "mcp")]
    fn standard_message_escapes_terminal_controls_but_metadata_remains_exact() {
        let text = "safe\x1b[31m\r\0\u{009b}\u{200f}\u{2028}\u{202e}\n\t";
        let notification = notification(
            &ProgressToken(rmcp::model::NumberOrString::Number(1)),
            &ShellProgressChunk {
                version: 1,
                sequence: 1,
                stream: ShellStream::Stdout,
                text: text.into(),
            },
        );
        let value = serde_json::to_value(notification).unwrap();

        assert_eq!(
            value["message"],
            "[stdout] safe\\x1b[31m\\r\\0\\u{9b}\\u{200f}\\u{2028}\\u{202e}\\n\\t"
        );
        assert_eq!(value["_meta"][OUTPUT_CHUNK_KEY]["text"], text);
    }

    #[test]
    #[cfg(feature = "mcp")]
    fn standard_message_is_bounded_after_control_escaping() {
        let message = ShellProgressChunk {
            version: 1,
            sequence: 1,
            stream: ShellStream::Stdout,
            text: "\x1b".repeat(STANDARD_MESSAGE_BYTES),
        };

        let rendered = standard_message(&message);

        assert!(rendered.len() <= STANDARD_MESSAGE_BYTES);
        assert!(rendered.ends_with("..."));
    }

    #[tokio::test]
    async fn pump_delivers_monotonic_messages_in_order_before_finishing() {
        let chunks = Arc::new(Mutex::new(Vec::new()));
        let pump = ProgressPump::start(Arc::new(Recording {
            chunks: chunks.clone(),
        }));
        pump.enqueue(1, ShellStream::Stdout, "first").unwrap();
        pump.enqueue(2, ShellStream::Stderr, "second").unwrap();

        pump.finish().await.unwrap();

        let chunks = chunks.lock().unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].sequence, 1);
        assert_eq!(chunks[0].stream, ShellStream::Stdout);
        assert_eq!(chunks[0].text, "first");
        assert_eq!(chunks[1].sequence, 2);
        assert_eq!(chunks[1].stream, ShellStream::Stderr);
        assert_eq!(chunks[1].text, "second");
    }

    struct Stalled;
    #[async_trait]
    impl ShellProgressSink for Stalled {
        async fn publish(&self, _: ShellProgressChunk) -> Result<(), String> {
            std::future::pending().await
        }
    }
    #[tokio::test]
    async fn saturated_queue_fails() {
        let pump = ProgressPump::start(Arc::new(Stalled));
        pump.enqueue(1, ShellStream::Stdout, "first").unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let mut saturated = false;
        for sequence in 2..=(PROGRESS_QUEUE_CAPACITY as u64 + 2) {
            if pump
                .enqueue(sequence, ShellStream::Stdout, "queued")
                .is_err()
            {
                saturated = true;
                break;
            }
        }
        assert!(saturated);
        pump.task.abort();
    }
}
