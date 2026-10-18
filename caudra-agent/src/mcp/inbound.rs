//! Everything a server may send Caudra unasked, in one table. A request gets
//! an answer from here; a notification is acted on or logged. Deprecated
//! features Caudra never declares (Roots, Sampling, Logging) are refused like
//! anything unknown.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use event_listener::Event;
use futures_lite::future;
use serde::Deserialize;
use serde_json::{Value, json};
use smol::Timer;
use tracing::{debug, info};

use super::call::{CallScope, ElicitMode, ElicitRequest, lock};
use super::elicitation::ElicitAction;
use super::era::ProtocolEra;
use super::kind::McpServerKind;
use super::message::{Notification, Request, RequestId, reply};
use super::protocol::{
    INVALID_PARAMS, JsonRpcError, META_KEY, METHOD_NOT_FOUND, PROMPTS_LIST_CHANGED_FILTER,
    RESOURCES_LIST_CHANGED_FILTER, SUBSCRIPTION_ID_META, TOOLS_LIST_CHANGED_FILTER,
};
use crate::cancel::{CancelToken, CancelTrigger};
use crate::types::CARD_ANNOTATION_SEPARATOR;

pub const UNSUPPORTED_METHOD: &str = "Caudra does not support";
pub const NOTIFICATIONS_KEY: &str = "notifications";
const UNSUPPORTED_MODE: &str = "unsupported elicitation mode";
const MISSING_SCHEMA: &str = "a form elicitation needs a requestedSchema";
const MISSING_URL: &str = "a URL elicitation needs a url";
const FORM_MODE: &str = "form";
const URL_MODE: &str = "url";
const LIST_CHANGE_QUEUE: usize = 16;
const MAX_COMPLETED_ELICITATIONS: usize = 32;
const PERCENT: f64 = 100.0;
const LIST_FILTERS: &[(&str, ListKind)] = &[
    (TOOLS_LIST_CHANGED_FILTER, ListKind::Tools),
    (PROMPTS_LIST_CHANGED_FILTER, ListKind::Prompts),
    (RESOURCES_LIST_CHANGED_FILTER, ListKind::Resources),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ListKind {
    Tools,
    Prompts,
    Resources,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Handling {
    Ping,
    Elicit,
    Progress,
    Cancelled,
    Log,
    ListChanged(ListKind),
    Acknowledged,
    ElicitationComplete,
    Refused,
}

const METHODS: &[(&str, Handling)] = &[
    ("ping", Handling::Ping),
    ("elicitation/create", Handling::Elicit),
    ("notifications/progress", Handling::Progress),
    ("notifications/cancelled", Handling::Cancelled),
    ("notifications/message", Handling::Log),
    (
        "notifications/tools/list_changed",
        Handling::ListChanged(ListKind::Tools),
    ),
    (
        "notifications/prompts/list_changed",
        Handling::ListChanged(ListKind::Prompts),
    ),
    (
        "notifications/resources/list_changed",
        Handling::ListChanged(ListKind::Resources),
    ),
    (
        "notifications/subscriptions/acknowledged",
        Handling::Acknowledged,
    ),
    (
        "notifications/elicitation/complete",
        Handling::ElicitationComplete,
    ),
    ("roots/list", Handling::Refused),
    ("sampling/createMessage", Handling::Refused),
];

fn handling(method: &str) -> Handling {
    METHODS
        .iter()
        .find(|(name, _)| *name == method)
        .map_or(Handling::Refused, |(_, handling)| *handling)
}

/// The call a server request is served against, and a signal that fires
/// once that call has ended.
#[derive(Clone)]
pub struct Route {
    scope: CallScope,
    ended: CancelToken,
}

/// One transport's view of what the server sends unasked, and of the calls
/// that are waiting while it does.
pub struct Inbound {
    server: Arc<str>,
    kind: OnceLock<Option<McpServerKind>>,
    era: Mutex<ProtocolEra>,
    /// The list changes the server agreed to report on its subscription.
    granted: Mutex<HashSet<ListKind>>,
    calls: Mutex<Vec<(u64, Route)>>,
    /// Server requests still being answered, so the server can withdraw one.
    answering: Mutex<HashMap<RequestId, CancelTrigger>>,
    completed: Mutex<VecDeque<String>>,
    completion: Event,
    list_changes: (flume::Sender<ListKind>, flume::Receiver<ListKind>),
}

/// A call in flight. Dropping it ends the call for anything still serving
/// a request against it.
pub struct Tracked<'a> {
    inbound: &'a Inbound,
    id: u64,
    route: Route,
    _ended: CancelTrigger,
}

impl Tracked<'_> {
    pub fn route(&self) -> &Route {
        &self.route
    }
}

impl Drop for Tracked<'_> {
    fn drop(&mut self) {
        lock(&self.inbound.calls).retain(|(id, _)| *id != self.id);
    }
}

/// `elicitation/create` params, and an entry of a -32042 error's
/// `data.elicitations`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ElicitParams {
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub requested_schema: Option<Value>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub elicitation_id: Option<String>,
}

impl ElicitParams {
    pub fn into_request(self, kind: Option<McpServerKind>) -> Result<ElicitRequest, JsonRpcError> {
        let invalid = |message: &str| JsonRpcError::new(INVALID_PARAMS, message);
        let mode = match self.mode.as_deref() {
            None | Some(FORM_MODE) => ElicitMode::Form {
                schema: self
                    .requested_schema
                    .ok_or_else(|| invalid(MISSING_SCHEMA))?,
            },
            Some(URL_MODE) => ElicitMode::Url {
                url: self.url.ok_or_else(|| invalid(MISSING_URL))?,
                elicitation_id: self.elicitation_id,
            },
            Some(other) => return Err(invalid(&format!("{UNSUPPORTED_MODE} {other}"))),
        };
        Ok(ElicitRequest {
            mode,
            message: self.message,
            kind,
        })
    }
}

impl Inbound {
    pub fn new(server: Arc<str>) -> Self {
        Self {
            server,
            kind: OnceLock::new(),
            era: Mutex::default(),
            granted: Mutex::default(),
            calls: Mutex::default(),
            answering: Mutex::default(),
            completed: Mutex::default(),
            completion: Event::new(),
            list_changes: flume::bounded(LIST_CHANGE_QUEUE),
        }
    }

    pub fn set_kind(&self, kind: Option<McpServerKind>) {
        let _ = self.kind.set(kind);
    }

    pub fn kind(&self) -> Option<McpServerKind> {
        self.kind.get().copied().flatten()
    }

    pub fn set_era(&self, era: ProtocolEra) {
        *lock(&self.era) = era;
    }

    pub fn era(&self) -> ProtocolEra {
        lock(&self.era).clone()
    }

    pub fn granted(&self) -> HashSet<ListKind> {
        lock(&self.granted).clone()
    }

    /// The lists the server said changed, as they arrive.
    pub fn list_changes(&self) -> flume::Receiver<ListKind> {
        self.list_changes.1.clone()
    }

    pub fn track(&self, id: u64, scope: &CallScope) -> Tracked<'_> {
        let (ended_trigger, ended) = CancelToken::new();
        let route = Route {
            scope: scope.clone(),
            ended,
        };
        lock(&self.calls).push((id, route.clone()));
        Tracked {
            inbound: self,
            id,
            route,
            _ended: ended_trigger,
        }
    }

    /// Where a request nothing ties to a call goes: the newest call in
    /// flight that can ask someone.
    pub fn routed(&self) -> Option<Route> {
        lock(&self.calls)
            .iter()
            .filter(|(_, route)| route.scope.elicitor.is_some())
            .max_by_key(|(id, _)| *id)
            .map(|(_, route)| route.clone())
    }

    /// The reply to send, or `None` when the server withdrew its request.
    pub async fn answer(&self, request: Request, route: Option<Route>) -> Option<Value> {
        let outcome = match handling(&request.method) {
            Handling::Ping => Ok(json!({})),
            Handling::Elicit => self.elicit(&request, route).await?,
            _ => {
                info!(server = %self.server, method = %request.method, "refused MCP server request");
                Err(JsonRpcError::new(
                    METHOD_NOT_FOUND,
                    format!("{UNSUPPORTED_METHOD} {}", request.method),
                ))
            }
        };
        Some(reply(&request.id, outcome))
    }

    pub fn notify(&self, notification: Notification) {
        let Notification { method, params } = notification;
        let handling = handling(&method);
        if matches!(handling, Handling::ListChanged(_) | Handling::Acknowledged)
            && !self.subscribed(&params)
        {
            debug!(server = %self.server, method, "MCP notification for a subscription not open");
            return;
        }
        match handling {
            Handling::Progress => self.progress(&params),
            Handling::Cancelled => self.withdraw(&params),
            Handling::Log => debug!(
                server = %self.server,
                level = %params["level"],
                logger = %params["logger"],
                data = %params["data"],
                "MCP server log message"
            ),
            Handling::ListChanged(kind) => {
                let _ = self.list_changes.0.try_send(kind);
            }
            Handling::Acknowledged => self.acknowledge(&params),
            Handling::ElicitationComplete => self.complete(&params),
            Handling::Ping | Handling::Elicit | Handling::Refused => {
                debug!(server = %self.server, method, "ignoring MCP server notification");
            }
        }
    }

    /// Whether the server reported the URL elicitation `elicitation_id`
    /// complete, now or within `wait`.
    pub async fn completion(&self, elicitation_id: &str, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        loop {
            let listener = self.completion.listen();
            if lock(&self.completed).iter().any(|id| id == elicitation_id) {
                return true;
            }
            let expired = future::or(
                async {
                    listener.await;
                    false
                },
                async {
                    Timer::at(deadline).await;
                    true
                },
            )
            .await;
            if expired {
                return false;
            }
        }
    }

    async fn elicit(
        &self,
        request: &Request,
        route: Option<Route>,
    ) -> Option<Result<Value, JsonRpcError>> {
        let elicitation = match ElicitParams::deserialize(&request.params) {
            Ok(params) => params.into_request(self.kind()),
            Err(error) => Err(JsonRpcError::new(INVALID_PARAMS, error.to_string())),
        };
        let elicitation = match elicitation {
            Ok(elicitation) => elicitation,
            Err(error) => return Some(Err(error)),
        };
        let Some(route) = route.filter(|route| route.scope.elicitor.is_some()) else {
            return Some(Ok(json!({"action": ElicitAction::Decline})));
        };
        let (withdraw, withdrawn) = CancelToken::new();
        lock(&self.answering).insert(request.id.clone(), withdraw);
        let response = route
            .scope
            .elicit(&self.server, elicitation, async {
                future::or(withdrawn.cancelled(), route.ended.cancelled()).await;
            })
            .await;
        lock(&self.answering).remove(&request.id)?;
        Some(Ok(response.map_or_else(
            || json!({"action": ElicitAction::Cancel}),
            |response| response.to_result(),
        )))
    }

    fn progress(&self, params: &Value) {
        let Some(id) = RequestId::deserialize(&params["progressToken"])
            .ok()
            .and_then(|token| token.as_u64())
        else {
            return;
        };
        let Some(text) = progress_text(params) else {
            return;
        };
        let scope = lock(&self.calls)
            .iter()
            .find(|(call, _)| *call == id)
            .map(|(_, route)| route.scope.clone());
        if let Some(scope) = scope {
            scope.report_progress(text);
        }
    }

    /// The server withdrew a request of its own, or, on stdio, tore down one
    /// of Caudra's subscriptions.
    fn withdraw(&self, params: &Value) {
        let Ok(id) = RequestId::deserialize(&params["requestId"]) else {
            return;
        };
        if lock(&self.answering).remove(&id).is_some() {
            debug!(server = %self.server, ?id, "MCP server withdrew its request");
        } else if let Some(subscription) = id.as_u64().and_then(|id| self.subscription(id)) {
            info!(server = %self.server, ?id, "MCP server ended a subscription");
            subscription.end();
        }
    }

    /// The open subscription `id` names.
    fn subscription(&self, id: u64) -> Option<CallScope> {
        lock(&self.calls)
            .iter()
            .find(|(call, route)| *call == id && route.scope.is_standing())
            .map(|(_, route)| route.scope.clone())
    }

    /// Whether a notification belongs to an open subscription, or names
    /// none: on stdio every subscription shares the one channel.
    fn subscribed(&self, params: &Value) -> bool {
        let id = &params[META_KEY][SUBSCRIPTION_ID_META];
        id.is_null()
            || RequestId::deserialize(id)
                .ok()
                .and_then(|id| id.as_u64())
                .and_then(|id| self.subscription(id))
                .is_some()
    }

    fn acknowledge(&self, params: &Value) {
        let granted: HashSet<ListKind> = LIST_FILTERS
            .iter()
            .filter(|(filter, _)| params[NOTIFICATIONS_KEY][*filter] == true)
            .map(|(_, kind)| *kind)
            .collect();
        info!(server = %self.server, ?granted, "MCP subscription acknowledged");
        *lock(&self.granted) = granted;
    }

    fn complete(&self, params: &Value) {
        let Some(id) = params["elicitationId"].as_str() else {
            return;
        };
        let mut completed = lock(&self.completed);
        if completed.len() == MAX_COMPLETED_ELICITATIONS {
            completed.pop_front();
        }
        completed.push_back(id.to_owned());
        drop(completed);
        self.completion.notify(usize::MAX);
        let scopes: Vec<CallScope> = lock(&self.calls)
            .iter()
            .map(|(_, route)| route.scope.clone())
            .collect();
        for scope in scopes {
            scope.url_completed(&self.server, id);
        }
    }
}

/// `3/10 · message` for counted steps, `40%` for a fraction, the bare
/// number when the server gave no total.
fn progress_text(params: &Value) -> Option<String> {
    let progress = params["progress"].as_f64()?;
    let amount = match params["total"].as_f64() {
        Some(total) if progress.fract() == 0.0 && total.fract() == 0.0 => {
            format!("{progress}/{total}")
        }
        Some(total) if total > 0.0 => format!("{:.0}%", progress / total * PERCENT),
        _ => format!("{progress}"),
    };
    Some(
        match params["message"].as_str().filter(|text| !text.is_empty()) {
            Some(message) => format!("{amount}{CARD_ANNOTATION_SEPARATOR}{message}"),
            None => amount,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::call::{ElicitResponse, Elicitor};
    use crate::mcp::transport::BoxFuture;
    use crate::tools::ToolLive;
    use test_case::test_case;

    const SERVER: &str = "srv";
    const CALL_ID: u64 = 4;

    fn inbound() -> Inbound {
        Inbound::new(Arc::from(SERVER))
    }

    fn request(method: &str, params: Value) -> Request {
        Request {
            id: RequestId::String("server-1".into()),
            method: method.into(),
            params,
        }
    }

    #[test_case(json!({"progress": 3, "total": 10, "message": "copying"}), Some("3/10 · copying") ; "counted_with_message")]
    #[test_case(json!({"progress": 0.4, "total": 1.0}), Some("40%") ; "fraction")]
    #[test_case(json!({"progress": 2}), Some("2") ; "no_total")]
    #[test_case(json!({"progress": 1, "message": ""}), Some("1") ; "empty_message")]
    #[test_case(json!({"total": 10}), None ; "no_progress")]
    fn progress_reads_as_an_annotation(params: Value, expected: Option<&str>) {
        assert_eq!(progress_text(&params).as_deref(), expected);
    }

    #[test]
    fn progress_reaches_the_call_its_token_names() {
        let inbound = inbound();
        let (tx, rx) = flume::unbounded();
        let scope = CallScope::new(None, Some(tx), CancelToken::none());
        let _tracked = inbound.track(CALL_ID, &scope);
        inbound.notify(Notification {
            method: "notifications/progress".into(),
            params: json!({"progressToken": CALL_ID, "progress": 1, "total": 2}),
        });
        inbound.notify(Notification {
            method: "notifications/progress".into(),
            params: json!({"progressToken": CALL_ID + 1, "progress": 1}),
        });
        let annotations: Vec<String> = rx
            .try_iter()
            .filter_map(|live| match live {
                ToolLive::Annotation(text) => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(annotations, ["1/2"]);
    }

    #[test_case("roots/list" ; "roots")]
    #[test_case("sampling/createMessage" ; "sampling")]
    #[test_case("vendor/unknown" ; "unknown")]
    #[test_case("notifications/progress" ; "notification_sent_as_request")]
    fn deprecated_and_unknown_requests_are_refused(method: &str) {
        let reply = smol::block_on(inbound().answer(request(method, Value::Null), None)).unwrap();
        assert_eq!(reply["error"]["code"], METHOD_NOT_FOUND);
        assert_eq!(
            reply["error"]["message"],
            format!("{UNSUPPORTED_METHOD} {method}")
        );
        assert_eq!(reply["id"], "server-1");
    }

    #[test_case(json!({"mode": "telepathy", "message": "m"}), INVALID_PARAMS ; "unknown_mode")]
    #[test_case(json!({"message": "m"}), INVALID_PARAMS ; "form_without_schema")]
    #[test_case(json!({"mode": "url", "message": "m"}), INVALID_PARAMS ; "url_without_url")]
    #[test_case(Value::Null, INVALID_PARAMS ; "no_params")]
    fn a_malformed_elicitation_is_invalid_params(params: Value, code: i64) {
        let reply =
            smol::block_on(inbound().answer(request("elicitation/create", params), None)).unwrap();
        assert_eq!(reply["error"]["code"], code);
    }

    #[test]
    fn an_elicitation_nobody_can_answer_is_declined() {
        let params = json!({"message": "m", "requestedSchema": {"type": "object"}});
        let reply =
            smol::block_on(inbound().answer(request("elicitation/create", params), None)).unwrap();
        assert_eq!(reply["result"], json!({"action": "decline"}));
    }

    #[test]
    fn a_list_change_is_queued_for_the_relist() {
        let inbound = inbound();
        let changes = inbound.list_changes();
        inbound.notify(Notification {
            method: "notifications/tools/list_changed".into(),
            params: Value::Null,
        });
        assert_eq!(changes.try_recv(), Ok(ListKind::Tools));
    }

    const SUBSCRIPTION_ID: u64 = 5;
    const ACKNOWLEDGED: &str = "notifications/subscriptions/acknowledged";
    const TOOLS_CHANGED: &str = "notifications/tools/list_changed";
    const PROMPTS_CHANGED: &str = "notifications/prompts/list_changed";

    fn on_subscription(method: &str, id: u64, mut params: Value) -> Notification {
        params[META_KEY] = json!({SUBSCRIPTION_ID_META: id});
        Notification {
            method: method.into(),
            params,
        }
    }

    #[test]
    fn subscription_notifications_reach_only_the_subscription_they_name() {
        let inbound = inbound();
        let changes = inbound.list_changes();
        let scope = CallScope::standing();
        let _subscription = inbound.track(SUBSCRIPTION_ID, &scope);
        let _plain = inbound.track(SUBSCRIPTION_ID + 1, &CallScope::none());
        inbound.notify(on_subscription(
            ACKNOWLEDGED,
            SUBSCRIPTION_ID,
            json!({NOTIFICATIONS_KEY: {TOOLS_LIST_CHANGED_FILTER: true, PROMPTS_LIST_CHANGED_FILTER: false}}),
        ));
        inbound.notify(on_subscription(
            PROMPTS_CHANGED,
            SUBSCRIPTION_ID + 1,
            json!({}),
        ));
        inbound.notify(on_subscription(
            PROMPTS_CHANGED,
            SUBSCRIPTION_ID + 2,
            json!({}),
        ));
        inbound.notify(on_subscription(TOOLS_CHANGED, SUBSCRIPTION_ID, json!({})));
        assert_eq!(inbound.granted(), HashSet::from([ListKind::Tools]));
        assert_eq!(changes.try_iter().collect::<Vec<_>>(), [ListKind::Tools]);
    }

    #[test]
    fn a_server_cancelling_a_subscription_ends_it() {
        let inbound = inbound();
        let scope = CallScope::standing();
        let _subscription = inbound.track(SUBSCRIPTION_ID, &scope);
        inbound.notify(Notification {
            method: "notifications/cancelled".into(),
            params: json!({"requestId": SUBSCRIPTION_ID}),
        });
        let ended = smol::block_on(scope.wait(SERVER, Duration::ZERO, future::pending()));
        assert_eq!(ended.unwrap(), Value::Null);
    }

    #[test]
    fn a_completion_heard_before_the_wait_still_counts() {
        let inbound = inbound();
        inbound.notify(Notification {
            method: "notifications/elicitation/complete".into(),
            params: json!({"elicitationId": "e1"}),
        });
        assert!(smol::block_on(inbound.completion("e1", Duration::ZERO)));
        assert!(!smol::block_on(inbound.completion("e2", Duration::ZERO)));
    }

    #[test]
    fn routing_prefers_the_newest_call_that_can_ask() {
        struct Never;
        impl Elicitor for Never {
            fn elicit<'a>(
                &'a self,
                _server: &'a str,
                _request: ElicitRequest,
            ) -> BoxFuture<'a, ElicitResponse> {
                Box::pin(future::pending())
            }
        }
        let inbound = inbound();
        let scope =
            |progress| CallScope::new(Some(Arc::new(Never)), Some(progress), CancelToken::none());
        let (older_tx, older) = flume::unbounded();
        let (newer_tx, newer) = flume::unbounded();
        let (silent_tx, silent) = flume::unbounded();
        let _older = inbound.track(1, &scope(older_tx));
        let _newer = inbound.track(2, &scope(newer_tx));
        let _silent = inbound.track(
            3,
            &CallScope::new(None, Some(silent_tx), CancelToken::none()),
        );
        inbound
            .routed()
            .unwrap()
            .scope
            .report_progress(SERVER.into());
        assert_eq!((older.len(), newer.len(), silent.len()), (0, 1, 0));
    }
}
