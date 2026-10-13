//! One MCP call as the caller sees it: who can answer the server's questions,
//! where progress goes, what cancels it, and how long it may sit idle. The
//! transport serves requests and notifications against this scope while the
//! call runs.

use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use event_listener::Event;
use futures_lite::future;
use serde_json::{Map, Value, json};
use smol::Timer;

use super::elicitation::ElicitAction;
use super::error::McpError;
use super::kind::McpServerKind;
use super::message::notification;
use super::output::McpInteraction;
use super::protocol::{
    CANCELLED_NOTIFICATION, DISCOVER_METHOD, INITIALIZE_METHOD, META_KEY, TOOLS_CALL_METHOD,
};
use super::transport::BoxFuture;
use crate::cancel::{CancelToken, CancelTrigger};
use crate::tools::ToolLive;
use crate::types::Answerer;

/// How far progress may stretch a call past its idle timeout, as a multiple
/// of that timeout.
pub const MAX_PROGRESS_EXTENSION: u32 = 10;
const PROGRESS_TOKEN_KEY: &str = "progressToken";
const REQUEST_ID_KEY: &str = "requestId";
const REASON_KEY: &str = "reason";
const REASON_TIMED_OUT: &str = "timed out";
const REASON_CANCELLED: &str = "cancelled by the user";
const REASON_ABANDONED: &str = "abandoned by the client";

/// Asks a person, or whatever stands in for one, what a server wants to know.
/// Dropping the future withdraws the question: the server cancelled it, or
/// the call it came with ended.
pub trait Elicitor: Send + Sync {
    fn elicit<'a>(
        &'a self,
        server: &'a str,
        request: ElicitRequest,
    ) -> BoxFuture<'a, ElicitResponse>;
}

#[derive(Debug, Clone, PartialEq)]
pub enum ElicitMode {
    Form { schema: Value },
    Url { url: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ElicitRequest {
    pub mode: ElicitMode,
    pub message: String,
    pub kind: Option<McpServerKind>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ElicitResponse {
    pub action: ElicitAction,
    /// What a form was answered with; only an accepted form carries it.
    pub content: Option<Map<String, Value>>,
    pub by: Answerer,
    /// `(label, value)` for display, with credential-looking values masked.
    pub answers: Vec<(String, String)>,
}

impl ElicitResponse {
    /// The user's `action` with nothing filled in: a decline, a cancel, or
    /// consent that asks for no content.
    pub fn unanswered(action: ElicitAction) -> Self {
        Self {
            action,
            content: None,
            by: Answerer::User,
            answers: Vec::new(),
        }
    }

    pub(super) fn to_result(&self) -> Value {
        match (self.action, &self.content) {
            (ElicitAction::Accept, Some(content)) => {
                json!({"action": self.action, "content": content})
            }
            _ => json!({"action": self.action}),
        }
    }
}

/// What a call carries while it runs. Cloning shares it: a server request
/// answered on another task records into, and pauses, the same call.
#[derive(Clone)]
pub struct CallScope {
    pub elicitor: Option<Arc<dyn Elicitor>>,
    pub progress: Option<flume::Sender<ToolLive>>,
    pub cancel: CancelToken,
    state: Arc<ScopeState>,
}

#[derive(Default)]
struct ScopeState {
    interactions: Mutex<Vec<McpInteraction>>,
    clock: IdleClock,
    standing: Option<Standing>,
}

/// A subscription: answered only when it ends, so never idle too long, and
/// one the server itself may end.
struct Standing {
    end: Mutex<Option<CancelTrigger>>,
    ended: CancelToken,
}

impl CallScope {
    pub fn new(
        elicitor: Option<Arc<dyn Elicitor>>,
        progress: Option<flume::Sender<ToolLive>>,
        cancel: CancelToken,
    ) -> Self {
        Self {
            elicitor,
            progress,
            cancel,
            state: Arc::default(),
        }
    }

    /// For calls nobody watches: no one to ask, nowhere to report, never
    /// cancelled.
    pub fn none() -> Self {
        Self::new(None, None, CancelToken::none())
    }

    /// For `subscriptions/listen`, which stays open until it ends.
    pub fn standing() -> Self {
        let (end, ended) = CancelToken::new();
        Self {
            state: Arc::new(ScopeState {
                standing: Some(Standing {
                    end: Mutex::new(Some(end)),
                    ended,
                }),
                ..ScopeState::default()
            }),
            ..Self::none()
        }
    }

    pub(super) fn is_standing(&self) -> bool {
        self.state.standing.is_some()
    }

    /// Settles a standing call as ended, for a server that tore it down
    /// without answering it.
    pub(super) fn end(&self) {
        if let Some(end) = self
            .state
            .standing
            .as_ref()
            .and_then(|standing| lock(&standing.end).take())
        {
            end.cancel();
        }
    }

    pub fn take_interactions(&self) -> Vec<McpInteraction> {
        std::mem::take(&mut *lock(&self.state.interactions))
    }

    pub(super) fn report_progress(&self, text: String) {
        if let Some(progress) = &self.progress {
            let _ = progress.try_send(ToolLive::Annotation(text));
        }
        self.state.clock.update(ClockState::touch);
    }

    /// Waits for `response`, unless the caller cancels first or the server
    /// goes quiet for `timeout`. Progress restarts the quiet period, up to
    /// [`MAX_PROGRESS_EXTENSION`] times `timeout` in all, and nothing counts
    /// while someone is answering the server. A standing call has no quiet
    /// period, and ends with `null` once the server tore it down.
    pub(super) async fn wait(
        &self,
        server: &str,
        timeout: Duration,
        response: impl Future<Output = Result<Value, McpError>>,
    ) -> Result<Value, McpError> {
        self.state.clock.update(ClockState::restart);
        let cancelled = async {
            self.cancel.cancelled().await;
            Err(McpError::Cancelled {
                server: server.into(),
            })
        };
        let idle = async {
            if let Some(standing) = &self.state.standing {
                standing.ended.cancelled().await;
                return Ok(Value::Null);
            }
            self.state.clock.expired(timeout).await;
            Err(McpError::Timeout {
                server: server.into(),
                timeout_ms: timeout.as_millis() as u64,
            })
        };
        future::or(response, future::or(cancelled, idle)).await
    }

    /// The elicitor's answer, recorded on the call. `None` when there is no
    /// elicitor, or the question was withdrawn or the call cancelled first.
    pub(super) async fn elicit(
        &self,
        server: &str,
        request: ElicitRequest,
        withdrawn: impl Future<Output = ()>,
    ) -> Option<ElicitResponse> {
        let elicitor = self.elicitor.as_ref()?;
        let message = request.message.clone();
        let url = match &request.mode {
            ElicitMode::Url { url } => Some(url.clone()),
            ElicitMode::Form { .. } => None,
        };
        let mut response = {
            let _paused = self.state.clock.pause();
            future::or(
                async { Some(elicitor.elicit(server, request).await) },
                async {
                    future::or(withdrawn, self.cancel.cancelled()).await;
                    None
                },
            )
            .await?
        };
        lock(&self.state.interactions).push(McpInteraction {
            message,
            url,
            action: response.action,
            by: response.by,
            answers: std::mem::take(&mut response.answers),
        });
        Some(response)
    }
}

pub(super) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Asks the server to report progress on a tool call, under the call's own
/// id so the report finds its way back.
pub(super) fn with_progress_token(method: &str, params: Option<Value>, id: u64) -> Option<Value> {
    if method != TOOLS_CALL_METHOD {
        return params;
    }
    with_meta(params, [(PROGRESS_TOKEN_KEY, id.into())])
}

/// `params` with `entries` added to its `_meta`, keeping what is there.
pub(super) fn with_meta<'a>(
    params: Option<Value>,
    entries: impl IntoIterator<Item = (&'a str, Value)>,
) -> Option<Value> {
    let mut params = params.unwrap_or_else(|| json!({}));
    if let Some(meta) = params.as_object_mut().and_then(|params| {
        params
            .entry(META_KEY)
            .or_insert_with(|| json!({}))
            .as_object_mut()
    }) {
        meta.extend(entries.into_iter().map(|(key, value)| (key.into(), value)));
    }
    Some(params)
}

/// Tells the server a request ended without its response, so it can stop
/// working on it. Never armed for `initialize`, which may not be cancelled,
/// nor for the `server/discover` probe, which a server that leaves it
/// unanswered is taken never to have known. Disarmed by anything that shows
/// the server already finished.
pub(super) struct CancelOnDrop<F: FnOnce(Value)> {
    id: u64,
    reason: &'static str,
    send: Option<F>,
}

impl<F: FnOnce(Value)> CancelOnDrop<F> {
    pub(super) fn new(method: &str, id: u64, send: F) -> Self {
        Self {
            id,
            reason: REASON_ABANDONED,
            send: (method != INITIALIZE_METHOD && method != DISCOVER_METHOD).then_some(send),
        }
    }

    pub(super) fn settle<T>(&mut self, result: &Result<T, McpError>) {
        match result {
            Err(McpError::Timeout { .. }) => self.reason = REASON_TIMED_OUT,
            Err(McpError::Cancelled { .. }) => self.reason = REASON_CANCELLED,
            Err(McpError::ConnectionLost { .. }) => {}
            _ => self.send = None,
        }
    }
}

impl<F: FnOnce(Value)> Drop for CancelOnDrop<F> {
    fn drop(&mut self) {
        if let Some(send) = self.send.take() {
            send(notification(
                CANCELLED_NOTIFICATION,
                json!({REQUEST_ID_KEY: self.id, REASON_KEY: self.reason}),
            ));
        }
    }
}

#[derive(Default)]
struct IdleClock {
    state: Mutex<ClockState>,
    changed: Event,
}

impl IdleClock {
    fn update(&self, change: impl FnOnce(&mut ClockState, Instant)) {
        change(&mut lock(&self.state), Instant::now());
        self.changed.notify(usize::MAX);
    }

    fn pause(&self) -> Paused<'_> {
        self.update(ClockState::pause);
        Paused(self)
    }

    async fn expired(&self, timeout: Duration) {
        loop {
            let listener = self.changed.listen();
            let deadline = lock(&self.state).deadline(timeout);
            match deadline {
                Some(deadline) if Instant::now() >= deadline => return,
                Some(deadline) => {
                    future::or(
                        async {
                            Timer::at(deadline).await;
                        },
                        listener,
                    )
                    .await
                }
                None => listener.await,
            }
        }
    }
}

struct Paused<'a>(&'a IdleClock);

impl Drop for Paused<'_> {
    fn drop(&mut self) {
        self.0.update(ClockState::resume);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ClockState {
    started: Instant,
    last_activity: Instant,
    pauses: usize,
    paused_since: Instant,
    paused_for: Duration,
}

impl Default for ClockState {
    fn default() -> Self {
        Self::at(Instant::now())
    }
}

impl ClockState {
    fn at(now: Instant) -> Self {
        Self {
            started: now,
            last_activity: now,
            pauses: 0,
            paused_since: now,
            paused_for: Duration::ZERO,
        }
    }

    /// A pause in progress outlives the request it began under.
    fn restart(&mut self, now: Instant) {
        *self = Self {
            pauses: self.pauses,
            ..Self::at(now)
        };
    }

    fn touch(&mut self, now: Instant) {
        self.last_activity = self.last_activity.max(now);
    }

    fn pause(&mut self, now: Instant) {
        if self.pauses == 0 {
            self.paused_since = now;
        }
        self.pauses += 1;
    }

    fn resume(&mut self, now: Instant) {
        self.pauses = self.pauses.saturating_sub(1);
        if self.pauses == 0 {
            self.paused_for += now.saturating_duration_since(self.paused_since);
            self.last_activity = now;
        }
    }

    /// `None` while paused.
    fn deadline(&self, timeout: Duration) -> Option<Instant> {
        let cap = self.started + self.paused_for + timeout * MAX_PROGRESS_EXTENSION;
        (self.pauses == 0).then(|| (self.last_activity + timeout).min(cap))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const TIMEOUT: Duration = Duration::from_secs(10);
    const REQUEST_ID: u64 = 7;
    const ANSWER_KEY: &str = "name";
    const ANSWER: &str = "caudra";

    fn after(start: Instant, timeouts: f64) -> Instant {
        start + TIMEOUT.mul_f64(timeouts)
    }

    #[test_case(&[], 1.0 ; "idle_from_the_start")]
    #[test_case(&[0.5], 1.5 ; "progress_resets_the_idle_window")]
    #[test_case(&[0.9, 1.8, 2.7], 3.7 ; "repeated_progress_keeps_extending")]
    #[test_case(&[9.5], 10.0 ; "capped_at_the_maximum_extension")]
    fn progress_resets_the_idle_timeout_up_to_the_cap(progress: &[f64], deadline: f64) {
        let start = Instant::now();
        let mut clock = ClockState::at(start);
        for at in progress {
            clock.touch(after(start, *at));
        }
        assert_eq!(clock.deadline(TIMEOUT), Some(after(start, deadline)));
    }

    #[test]
    fn a_pause_stops_the_clock_and_moves_the_cap() {
        let start = Instant::now();
        let mut clock = ClockState::at(start);
        clock.pause(after(start, 0.5));
        clock.pause(after(start, 0.6));
        assert_eq!(clock.deadline(TIMEOUT), None);
        clock.resume(after(start, 20.0));
        assert_eq!(clock.deadline(TIMEOUT), None);
        clock.resume(after(start, 30.0));
        assert_eq!(clock.deadline(TIMEOUT), Some(after(start, 31.0)));
        clock.touch(after(start, 39.5));
        assert_eq!(clock.deadline(TIMEOUT), Some(after(start, 39.5)));
    }

    #[test]
    fn a_restart_keeps_a_pause_in_progress() {
        let start = Instant::now();
        let mut clock = ClockState::at(start);
        clock.pause(start);
        clock.restart(after(start, 1.0));
        assert_eq!(clock.deadline(TIMEOUT), None);
    }

    #[test_case(TOOLS_CALL_METHOD, None, Some(json!({META_KEY: {PROGRESS_TOKEN_KEY: REQUEST_ID}})) ; "call_without_params")]
    #[test_case(TOOLS_CALL_METHOD, Some(json!({"name": "t", META_KEY: {"other": 1}})), Some(json!({"name": "t", META_KEY: {"other": 1, PROGRESS_TOKEN_KEY: REQUEST_ID}})) ; "call_keeps_meta")]
    #[test_case("tools/list", None, None ; "list_untouched")]
    fn tool_calls_ask_for_progress(method: &str, params: Option<Value>, expected: Option<Value>) {
        assert_eq!(with_progress_token(method, params, REQUEST_ID), expected);
    }

    #[test_case(ElicitAction::Accept, Some(json!({ANSWER_KEY: ANSWER})), json!({"action": "accept", "content": {ANSWER_KEY: ANSWER}}) ; "accepted_form")]
    #[test_case(ElicitAction::Accept, None, json!({"action": "accept"}) ; "accepted_url")]
    #[test_case(ElicitAction::Decline, Some(json!({ANSWER_KEY: ANSWER})), json!({"action": "decline"}) ; "declined_drops_content")]
    #[test_case(ElicitAction::Cancel, None, json!({"action": "cancel"}) ; "cancelled")]
    fn an_elicitation_result_carries_content_only_when_accepted(
        action: ElicitAction,
        content: Option<Value>,
        expected: Value,
    ) {
        let response = ElicitResponse {
            action,
            content: content.and_then(|content| content.as_object().cloned()),
            by: Answerer::User,
            answers: Vec::new(),
        };
        assert_eq!(response.to_result(), expected);
    }

    #[test_case("tools/call", Ok(()), false ; "answered")]
    #[test_case("tools/call", Err(McpError::Timeout { server: String::new(), timeout_ms: 1 }), true ; "timed_out")]
    #[test_case("tools/call", Err(McpError::Cancelled { server: String::new() }), true ; "cancelled")]
    #[test_case("tools/call", Err(McpError::ServerDied { server: String::new() }), false ; "server_died")]
    #[test_case(INITIALIZE_METHOD, Err(McpError::Cancelled { server: String::new() }), false ; "initialize_never")]
    fn a_call_ending_without_its_response_is_cancelled(
        method: &str,
        result: Result<(), McpError>,
        cancels: bool,
    ) {
        let sent = Mutex::new(None);
        {
            let mut guard = CancelOnDrop::new(method, REQUEST_ID, |message| {
                *lock(&sent) = Some(message);
            });
            guard.settle(&result);
        }
        let sent = lock(&sent).take();
        assert_eq!(sent.is_some(), cancels);
        if let Some(message) = sent {
            assert_eq!(message["method"], CANCELLED_NOTIFICATION);
            assert_eq!(message["params"][REQUEST_ID_KEY], REQUEST_ID);
        }
    }
}
