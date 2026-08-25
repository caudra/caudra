//! Ordered, bounded delivery of live shell output through MCP progress notifications.
//!
//! The queue bounds memory and decouples pipe reads from network latency. Saturation is a hard
//! execution failure rather than permission to drop chunks: sequence continuity means consumers can
//! trust that a successful call's progress stream is complete and ordered.

use crate::types::{OutputChunk, Stream};
use async_trait::async_trait;
use rmcp::{
    RoleServer,
    model::{
        JsonObject, MetaObject, NotificationMetaObject, ProgressNotificationParam, ProgressToken,
    },
    service::Peer,
};
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc;

const OUTPUT_CHUNK_KEY: &str = "ai.workcell/tool-output-chunk";
const PROGRESS_QUEUE_CAPACITY: usize = 32;
const PROGRESS_DELIVERY_TIMEOUT: Duration = Duration::from_secs(2);
const PROGRESS_DRAIN_TIMEOUT: Duration = Duration::from_secs(3);
const STANDARD_MESSAGE_BYTES: usize = 16 * 1024;

#[async_trait]
pub(crate) trait ProgressTransport: Send + Sync {
    async fn publish(&self, notification: ProgressNotificationParam) -> Result<(), String>;
}
pub(crate) struct PeerProgressTransport {
    pub(crate) peer: Peer<RoleServer>,
}
#[async_trait]
impl ProgressTransport for PeerProgressTransport {
    async fn publish(&self, notification: ProgressNotificationParam) -> Result<(), String> {
        self.peer
            .notify_progress(notification)
            .await
            .map_err(|_| "progress delivery failed".to_owned())
    }
}
struct ProgressMessage {
    sequence: u64,
    stream: Stream,
    text: String,
}
pub(crate) struct ProgressPump {
    sender: mpsc::Sender<ProgressMessage>,
    pub(crate) failure: mpsc::Receiver<String>,
    pub(crate) task: tokio::task::JoinHandle<Result<(), String>>,
}
impl ProgressPump {
    pub(crate) fn start(token: ProgressToken, transport: Arc<dyn ProgressTransport>) -> Self {
        let (sender, mut receiver) = mpsc::channel::<ProgressMessage>(PROGRESS_QUEUE_CAPACITY);
        let (failure_sender, failure) = mpsc::channel(1);
        let task = tokio::spawn(async move {
            // A single worker preserves stdout/stderr event order assigned by the orchestrator.
            while let Some(message) = receiver.recv().await {
                let delivery = tokio::time::timeout(
                    PROGRESS_DELIVERY_TIMEOUT,
                    transport.publish(notification(&token, &message)),
                )
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
    pub(crate) fn enqueue(&self, sequence: u64, stream: Stream, text: &str) -> Result<(), String> {
        // Never await here: blocking the orchestrator could stop pipe draining and deadlock a child.
        // Instead, a full bounded queue aborts execution with an explicit delivery failure.
        self.sender
            .try_send(ProgressMessage {
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
fn notification(token: &ProgressToken, message: &ProgressMessage) -> ProgressNotificationParam {
    let mut fields = JsonObject::new();
    fields.insert(
        OUTPUT_CHUNK_KEY.to_owned(),
        serde_json::to_value(OutputChunk {
            version: 1,
            sequence: message.sequence,
            stream: message.stream,
            text: &message.text,
        })
        .expect("output chunk serializes"),
    );
    let mut n = ProgressNotificationParam::new(token.clone(), message.sequence as f64)
        .with_message(standard_message(message));
    n.meta = Some(NotificationMetaObject(MetaObject(fields)));
    n
}

fn standard_message(message: &ProgressMessage) -> String {
    let label = match message.stream {
        Stream::Stdout => "stdout",
        Stream::Stderr => "stderr",
    };
    let mut rendered = format!("[{label}] ");
    for character in message.text.chars() {
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
        notifications: Arc<Mutex<Vec<ProgressNotificationParam>>>,
    }

    #[async_trait]
    impl ProgressTransport for Recording {
        async fn publish(&self, notification: ProgressNotificationParam) -> Result<(), String> {
            self.notifications.lock().unwrap().push(notification);
            Ok(())
        }
    }

    #[test]
    fn notification_has_standard_message_and_structured_chunk() {
        let notification = notification(
            &ProgressToken(rmcp::model::NumberOrString::String("request-1".into())),
            &ProgressMessage {
                sequence: 4,
                stream: Stream::Stderr,
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
    fn standard_message_escapes_terminal_controls_but_metadata_remains_exact() {
        let text = "safe\x1b[31m\r\0\u{009b}\u{200f}\u{2028}\u{202e}\n\t";
        let notification = notification(
            &ProgressToken(rmcp::model::NumberOrString::Number(1)),
            &ProgressMessage {
                sequence: 1,
                stream: Stream::Stdout,
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
    fn standard_message_is_bounded_after_control_escaping() {
        let message = ProgressMessage {
            sequence: 1,
            stream: Stream::Stdout,
            text: "\x1b".repeat(STANDARD_MESSAGE_BYTES),
        };

        let rendered = standard_message(&message);

        assert!(rendered.len() <= STANDARD_MESSAGE_BYTES);
        assert!(rendered.ends_with("..."));
    }

    #[tokio::test]
    async fn pump_delivers_monotonic_messages_in_order_before_finishing() {
        let notifications = Arc::new(Mutex::new(Vec::new()));
        let pump = ProgressPump::start(
            ProgressToken(rmcp::model::NumberOrString::String("ordered".into())),
            Arc::new(Recording {
                notifications: notifications.clone(),
            }),
        );
        pump.enqueue(1, Stream::Stdout, "first").unwrap();
        pump.enqueue(2, Stream::Stderr, "second").unwrap();

        pump.finish().await.unwrap();

        let notifications = notifications.lock().unwrap();
        assert_eq!(notifications.len(), 2);
        assert_eq!(notifications[0].progress, 1.0);
        assert_eq!(notifications[0].message.as_deref(), Some("[stdout] first"));
        assert_eq!(notifications[1].progress, 2.0);
        assert_eq!(notifications[1].message.as_deref(), Some("[stderr] second"));
    }

    struct Stalled;
    #[async_trait]
    impl ProgressTransport for Stalled {
        async fn publish(&self, _: ProgressNotificationParam) -> Result<(), String> {
            std::future::pending().await
        }
    }
    #[tokio::test]
    async fn saturated_queue_fails() {
        let token = ProgressToken(rmcp::model::NumberOrString::Number(7));
        let pump = ProgressPump::start(token, Arc::new(Stalled));
        pump.enqueue(1, Stream::Stdout, "first").unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let mut saturated = false;
        for sequence in 2..=(PROGRESS_QUEUE_CAPACITY as u64 + 2) {
            if pump.enqueue(sequence, Stream::Stdout, "queued").is_err() {
                saturated = true;
                break;
            }
        }
        assert!(saturated);
        pump.task.abort();
    }
}
