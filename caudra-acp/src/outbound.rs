use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol_schema::v1::{
    AgentNotification, AgentRequest, AgentResponse, CancelRequestNotification, Error as AcpError,
    JsonRpcMessage, Meta, Notification, Request, RequestId, Response, SessionId,
    SessionNotification, SessionUpdate,
};
use flume::{Receiver, Sender};
use serde::Serialize;
use serde_json::Value;
use tracing::warn;

use crate::locked;
use crate::turn::{CancelSignal, OwedPrompt};

const FIRST_OUTGOING_REQUEST_ID: i64 = 1000;
const SESSION_UPDATE_METHOD: &str = "session/update";
pub(crate) const CANCEL_REQUEST_METHOD: &str = "$/cancel_request";

/// Ids come from here and are never reused, so a late answer for a closed
/// session cannot match a request of another session.
static NEXT_OUTGOING_REQUEST_ID: AtomicI64 = AtomicI64::new(FIRST_OUTGOING_REQUEST_ID);

/// What one session and its client still owe each other. Permission requests
/// have independent broker waiters; elicitation uses the interactive answer
/// channel.
#[derive(Default)]
pub(crate) struct Pending {
    pub(crate) prompt: Option<OwedPrompt>,
    pub(crate) asks: HashMap<i64, AskKind>,
    pub(crate) cancel_signal: CancelSignal,
    /// The agent's events stopped, so no turn will answer another prompt.
    pub(crate) agent_gone: bool,
}

impl Pending {
    /// The `_meta` every update of the owed prompt's turn carries.
    pub(crate) fn update_meta(&self) -> Option<Meta> {
        self.prompt.as_ref().map(|owed| owed.identity.update_meta())
    }
}

pub(crate) enum AskKind {
    Permission {
        request_id: String,
        exact_project_deny: bool,
    },
    ResolvedPermission,
    Elicitation,
    /// An MCP server's elicitation, answered on its own channel so a late
    /// result for one the server withdrew reaches nobody.
    McpElicitation(Sender<Value>),
}

pub(crate) type PendingState = Arc<Mutex<Pending>>;

pub(crate) struct Frame {
    pub(crate) message: Value,
    written: Option<Sender<()>>,
}

impl Frame {
    pub(crate) fn mark_written(self) {
        if let Some(written) = self.written {
            let _ = written.send(());
        }
    }
}

/// Settles once its frame reached stdout, or once it never can.
pub(crate) struct Written(Receiver<()>);

impl Written {
    #[cfg(test)]
    pub(crate) fn settled() -> Self {
        Self(flume::bounded(1).1)
    }

    #[cfg(test)]
    pub(crate) fn gate() -> (Sender<()>, Self) {
        let (open, gate) = flume::bounded(1);
        (open, Self(gate))
    }

    pub(crate) async fn wait(self) {
        let _ = self.0.recv_async().await;
    }
}

/// Every frame leaves through one queue, so the order they are sent in is the
/// order the client reads them in.
#[derive(Clone)]
pub(crate) struct Outbound {
    frames: Sender<Frame>,
}

impl Outbound {
    pub(crate) fn channel() -> (Self, Receiver<Frame>) {
        let (frames, queue) = flume::unbounded();
        (Self { frames }, queue)
    }

    pub(crate) fn send(&self, message: impl Serialize) {
        self.enqueue(message, None);
    }

    pub(crate) fn respond(&self, id: RequestId, result: Result<AgentResponse, AcpError>) {
        self.send(Response::new(id, result));
    }

    /// Lets the caller hold back what must reach the client after this response.
    pub(crate) fn respond_written(
        &self,
        id: RequestId,
        result: Result<AgentResponse, AcpError>,
    ) -> Written {
        let (written_tx, written) = flume::bounded(1);
        self.enqueue(Response::new(id, result), Some(written_tx));
        Written(written)
    }

    pub(crate) fn session_update(&self, sid: &SessionId, update: SessionUpdate) {
        self.session_update_meta(sid, update, None);
    }

    pub(crate) fn session_update_meta(
        &self,
        sid: &SessionId,
        update: SessionUpdate,
        meta: Option<Meta>,
    ) {
        let notification = AgentNotification::SessionNotification(
            SessionNotification::new(sid.clone(), update).meta(meta),
        );
        self.send(Notification {
            method: Arc::from(SESSION_UPDATE_METHOD),
            params: Some(notification),
        });
    }

    /// Withdraws a request the client was asked, which it may still be showing.
    pub(crate) fn cancel_request(&self, id: i64) {
        self.send(Notification {
            method: Arc::from(CANCEL_REQUEST_METHOD),
            params: Some(CancelRequestNotification::new(RequestId::Number(id))),
        });
    }

    /// An extension notification, whose method starts with `_`.
    pub(crate) fn notify_ext(&self, method: &str, params: Value) {
        self.send(Notification {
            method: Arc::from(method),
            params: Some(params),
        });
    }

    pub(crate) fn notify(&self, notification: AgentNotification) {
        self.send(Notification {
            method: Arc::from(notification.method()),
            params: Some(notification),
        });
    }

    /// Sends a request the client must answer, registering it first so the
    /// response can never race past us.
    pub(crate) fn ask(&self, pending: &PendingState, kind: AskKind, request: AgentRequest) -> i64 {
        let id = next_request_id();
        locked(pending).asks.insert(id, kind);
        self.send(Request {
            id: RequestId::Number(id),
            method: Arc::from(request.method()),
            params: Some(request),
        });
        id
    }

    fn enqueue(&self, message: impl Serialize, written: Option<Sender<()>>) {
        match serde_json::to_value(JsonRpcMessage::wrap(message)) {
            Ok(message) => {
                let _ = self.frames.send(Frame { message, written });
            }
            Err(error) => warn!(%error, "dropped an outbound message that does not serialize"),
        }
    }
}

pub(crate) fn next_request_id() -> i64 {
    NEXT_OUTGOING_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
}
