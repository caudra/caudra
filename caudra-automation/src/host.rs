//! The ABI between one firing and the session it runs in. The engine checks capabilities, trust
//! and the per-firing limits before a request reaches the host, so a host performs effects and
//! reports what the outside world did: a [`Failure`] the script may catch, or a refusal or
//! [`Interruption`] that ends the firing.

use std::collections::BTreeMap;
use std::str::FromStr;
use std::time::Duration;

use caudra_script::{canonical_json, sha256_hex};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;

use crate::event::Audience;
use crate::limits::LimitRefusal;

/// The longest `expires` a `message` or `set_goal` delivery may wait.
pub const MAX_EXPIRES: Duration = Duration::from_hours(7 * 24);
/// The longest `timeout` an `http` request may ask for.
pub const MAX_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// The host cuts a response body to this many bytes before the script sees it.
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
/// The longest body an `http` request may send.
pub const MAX_REQUEST_BODY_BYTES: usize = 1024 * 1024;
const JOURNAL_JSON: &str = "a request is plain JSON data";
/// Between a [`Failure`]'s kind and its message, in its text.
const FAILURE_SEPARATOR: &str = ": ";

pub type HostResult<T> = Result<T, HostError>;

/// The session a firing runs in. Host calls arrive one at a time, in call order, over the
/// bridge from the interpreter thread.
pub trait AutomationHost {
    /// Unix milliseconds. A dry run answers the time of the firing it replays.
    fn now_ms(&self) -> i64;
    /// Polled while the script runs, so a pause, disarm or shutdown ends the firing at once.
    fn interrupted(&self) -> Option<Interruption>;
    /// Asked before the firing's first action that [`ActionKind::charges`]: cooldown,
    /// `max_per_hour` and the failure backoff. `Ok` charges the firing as acting.
    fn admit(&self) -> Result<(), LimitRefusal>;
    /// Performs one request, journaling it as it starts and as it ends.
    fn act(&self, site: CallSite, request: ActionRequest) -> HostResult<ActionReply>;
}

/// Where a request sits in its firing: its place in call order, and the position of the call
/// in the script when Rhai knows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CallSite {
    pub seq: u32,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Message,
    SetGoal,
    Notify,
    Http,
    Reply,
    Send,
    Publish,
    Broadcast,
    StartWorkflow,
    Pause,
    Log,
}

/// A host function call after the engine checked it. Serialized, it is the journal's request
/// body, tagged with its `kind`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActionRequest {
    Message(MessageRequest),
    SetGoal(GoalRequest),
    Notify {
        text: String,
    },
    Http(HttpRequest),
    /// Answers the sender of the consumed message the firing handles.
    Reply {
        text: String,
    },
    Send(SendRequest),
    Publish {
        topic: String,
        text: String,
    },
    Broadcast {
        text: String,
    },
    StartWorkflow(WorkflowRequest),
    /// Sets the session's pause latch. Other firings stop, but the calling firing runs on to its
    /// end, so it can still notify about the pause.
    Pause {
        reason: String,
    },
    Log {
        text: String,
    },
}

/// Queues text for this session's model.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MessageRequest {
    /// Trusted: it becomes this session's instructions.
    pub text: String,
    /// Shown to the model only inside a framed, untrusted JSON block.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attach: Option<Value>,
    pub delivery: DeliveryMode,
    /// At most [`MAX_EXPIRES`].
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "optional_duration"
    )]
    pub expires: Option<Duration>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryMode {
    /// Starts a turn once the session settles.
    #[default]
    Next,
    /// Joins a running turn before its next model request, or starts one when idle.
    Guide,
}

/// Sets the session goal and queues its kickoff turn like a `next` message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GoalRequest {
    /// Trusted: it becomes this session's goal.
    pub condition: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continuation_limit: Option<u32>,
    /// Replaces an active goal instead of failing with [`FailureKind::GoalActive`].
    pub replace: bool,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "optional_duration"
    )]
    pub expires: Option<Duration>,
}

/// An outgoing request. The engine checked a literal URL's origin, the environment variable
/// names and the header values; the host resolves `url_env` and checks that origin itself.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HttpRequest {
    pub method: HttpMethod,
    #[serde(flatten)]
    pub target: HttpTarget,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub query: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Environment variable whose value the host sends as a bearer token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bearer_env: Option<String>,
    /// Header names, each with the environment variable whose value the host sends.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub secret_headers: BTreeMap<String, String>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub payload: Option<HttpPayload>,
    /// At most [`MAX_HTTP_TIMEOUT`].
    #[serde(serialize_with = "duration")]
    pub timeout: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HttpTarget {
    Url(String),
    /// A secret URL: the journal and the trace show only its origin.
    UrlEnv(String),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HttpPayload {
    /// Serialized by the host, with `Content-Type: application/json`.
    Json(Value),
    Body(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SendRequest {
    /// An `@name` the engine matched against `meta.messaging.send`.
    pub to: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WorkflowRequest {
    /// A name the engine found in `meta.workflows`.
    pub name: String,
    /// Plain JSON: untrusted values lose their mark, as workflow args are data.
    pub args: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_budget: Option<u32>,
}

/// What the host answers. Serialized, it is the journal's result body.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ActionReply {
    Done,
    GoalSet(GoalSet),
    Http(HttpResponse),
    Sent(SendReceipt),
    Published(PublishReceipt),
    WorkflowStarted(WorkflowStarted),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalSet {
    /// The condition as the goal stored it.
    pub condition: String,
}

/// Any status. `body` is cut to [`MAX_RESPONSE_BYTES`] and reaches the script untrusted, and so
/// does `json`, the body parsed when it parses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
    pub json: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendReceipt {
    pub status: SendStatus,
    pub message_id: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendStatus {
    Queued,
    Held,
    /// The delivery may have been accepted before the connection failed: sending again could
    /// deliver it twice.
    Unknown,
}

/// A publication the history recorded, with each live recipient's outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishReceipt {
    pub message_id: String,
    pub audience: Audience,
    pub recipients: Vec<RecipientReceipt>,
    /// Matching sessions past `max_fanout`, which were not sent the message.
    pub skipped: u32,
    /// Work the publication queued for consumer groups. Queued is not done.
    pub queued: Vec<QueuedWork>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipientReceipt {
    /// The recipient's `@name`.
    pub name: String,
    /// Reaches the script untrusted.
    pub title: String,
    pub status: RecipientStatus,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecipientStatus {
    Queued,
    Held,
    Refused,
    Unavailable,
    RateLimited,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedWork {
    pub group: String,
    pub work: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowStarted {
    pub run_id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HostError {
    /// The outside world failed: the script may catch it as `#{ kind, message }`.
    #[error(transparent)]
    Failure(#[from] Failure),
    /// Policy the host enforces, such as the origin a `url_env` resolved to. Ends the firing.
    #[error("{0}")]
    Refused(String),
    #[error(transparent)]
    Interrupted(#[from] Interruption),
}

/// A failure a script may catch: `catch (err)` binds `#{ kind, message }`. Its text, which
/// storage keeps as the action's error, parses back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, thiserror::Error)]
#[error("{}{FAILURE_SEPARATOR}{message}", .kind.as_str())]
pub struct Failure {
    pub kind: FailureKind,
    pub message: String,
}

impl FromStr for Failure {
    type Err = MalformedFailure;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (kind, message) = text.split_once(FAILURE_SEPARATOR).ok_or(MalformedFailure)?;
        let kind =
            serde_json::from_value(Value::String(kind.to_owned())).map_err(|_| MalformedFailure)?;
        Ok(Self::new(kind, message))
    }
}

/// Text that is not a [`Failure`]'s.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("a failure's text is a known kind, {FAILURE_SEPARATOR:?}, then its message")]
pub struct MalformedFailure;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// An `http` request ran out of time.
    Timeout,
    /// An `http` request failed before a response arrived.
    Transport,
    Refused,
    RateLimited,
    /// Including a publication the history could not record.
    Unavailable,
    UnknownRecipient,
    /// A group's backlog, the outstanding total, or the fan-out limit.
    GroupFull,
    ReadOnly,
    /// `reply()` to a message a script sent.
    NoReplyTarget,
    /// `set_goal` without `replace: true` while another goal is active.
    GoalActive,
    InvalidArgument,
    /// An ordinary Rhai error.
    Script,
}

/// Why a firing stopped from outside. Uncatchable; the firing's state changes are discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum Interruption {
    #[error("automations are paused in this session")]
    Paused,
    #[error("the automation was disarmed")]
    Disarmed,
    #[error("the session is closing")]
    Shutdown,
}

impl ActionKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::SetGoal => "set_goal",
            Self::Notify => "notify",
            Self::Http => "http",
            Self::Reply => "reply",
            Self::Send => "send",
            Self::Publish => "publish",
            Self::Broadcast => "broadcast",
            Self::StartWorkflow => "start_workflow",
            Self::Pause => "pause",
            Self::Log => "log",
        }
    }

    /// The first such action asks [`AutomationHost::admit`]. A log line costs nothing, and
    /// pausing must work while a limit holds.
    pub const fn charges(self) -> bool {
        !matches!(self, Self::Log | Self::Pause)
    }

    /// Queues an outbox item for this session's model.
    pub const fn delivers(self) -> bool {
        matches!(self, Self::Message | Self::SetGoal)
    }
}

impl ActionRequest {
    /// The journal's request body, which [`request_hash`] keys: the request tagged with its
    /// kind.
    pub fn to_journal(&self) -> Value {
        serde_json::to_value(self).expect(JOURNAL_JSON)
    }

    pub const fn kind(&self) -> ActionKind {
        match self {
            Self::Message(_) => ActionKind::Message,
            Self::SetGoal(_) => ActionKind::SetGoal,
            Self::Notify { .. } => ActionKind::Notify,
            Self::Http(_) => ActionKind::Http,
            Self::Reply { .. } => ActionKind::Reply,
            Self::Send(_) => ActionKind::Send,
            Self::Publish { .. } => ActionKind::Publish,
            Self::Broadcast { .. } => ActionKind::Broadcast,
            Self::StartWorkflow(_) => ActionKind::StartWorkflow,
            Self::Pause { .. } => ActionKind::Pause,
            Self::Log { .. } => ActionKind::Log,
        }
    }
}

impl FailureKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Transport => "transport",
            Self::Refused => "refused",
            Self::RateLimited => "rate_limited",
            Self::Unavailable => "unavailable",
            Self::UnknownRecipient => "unknown_recipient",
            Self::GroupFull => "group_full",
            Self::ReadOnly => "read_only",
            Self::NoReplyTarget => "no_reply_target",
            Self::GoalActive => "goal_active",
            Self::InvalidArgument => "invalid_argument",
            Self::Script => "script",
        }
    }
}

impl Failure {
    pub fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

/// Keys a journaled request, so a dry run can answer from the firing it replays.
pub fn request_hash(request: &Value) -> String {
    sha256_hex(&[canonical_json(request).as_bytes()])
}

fn duration<S: Serializer>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(&humantime::format_duration(*duration))
}

fn optional_duration<S: Serializer>(
    duration: &Option<Duration>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match duration {
        Some(duration) => serializer.collect_str(&humantime::format_duration(*duration)),
        None => serializer.serialize_none(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const WEBHOOK: &str = "https://hooks.example.com/caudra";
    const TOKEN_ENV: &str = "HOOK_TOKEN";
    const URL_ENV: &str = "SLACK_STANDUP_URL";
    const NUDGE: &str = "Finish the current step.";
    const SEPARATED_MESSAGE: &str = "GitHub said: 502: bad gateway";
    const JOURNAL_SHAPE: &str = "the journal body is the request tagged with its kind";

    fn webhook() -> ActionRequest {
        ActionRequest::Http(HttpRequest {
            method: HttpMethod::Post,
            target: HttpTarget::Url(WEBHOOK.into()),
            query: BTreeMap::new(),
            headers: BTreeMap::new(),
            bearer_env: Some(TOKEN_ENV.into()),
            secret_headers: BTreeMap::new(),
            payload: Some(HttpPayload::Json(json!({ "verdict": "met" }))),
            timeout: MAX_HTTP_TIMEOUT,
        })
    }

    #[test_case(
        webhook(),
        json!({
            "kind": "http",
            "method": "POST",
            "url": WEBHOOK,
            "bearer_env": TOKEN_ENV,
            "json": { "verdict": "met" },
            "timeout": "30s",
        });
        "http_with_a_literal_url"
    )]
    #[test_case(
        ActionRequest::Http(HttpRequest {
            method: HttpMethod::Get,
            target: HttpTarget::UrlEnv(URL_ENV.into()),
            query: BTreeMap::from([("branch".into(), "main".into())]),
            headers: BTreeMap::new(),
            bearer_env: None,
            secret_headers: BTreeMap::new(),
            payload: None,
            timeout: MAX_HTTP_TIMEOUT,
        }),
        json!({
            "kind": "http",
            "method": "GET",
            "url_env": URL_ENV,
            "query": { "branch": "main" },
            "timeout": "30s",
        });
        "http_with_a_secret_url"
    )]
    #[test_case(
        ActionRequest::Message(MessageRequest {
            text: NUDGE.into(),
            attach: None,
            delivery: DeliveryMode::Guide,
            expires: Some(Duration::from_mins(10)),
        }),
        json!({ "kind": "message", "text": NUDGE, "delivery": "guide", "expires": "10m" });
        "guided_message"
    )]
    #[test_case(
        ActionRequest::Pause { reason: NUDGE.into() },
        json!({ "kind": "pause", "reason": NUDGE });
        "pause"
    )]
    fn requests_journal_as_tagged_json(request: ActionRequest, expected: Value) {
        assert_eq!(request.to_journal(), expected, "{JOURNAL_SHAPE}");
        assert_eq!(
            serde_json::to_value(request.kind()).unwrap(),
            expected["kind"],
            "{JOURNAL_SHAPE}"
        );
    }

    #[test_case(ActionKind::Message => (true, true); "message")]
    #[test_case(ActionKind::SetGoal => (true, true); "set_goal")]
    #[test_case(ActionKind::Http => (true, false); "http")]
    #[test_case(ActionKind::Publish => (true, false); "publish")]
    #[test_case(ActionKind::Pause => (false, false); "pause")]
    #[test_case(ActionKind::Log => (false, false); "log")]
    fn action_kinds_charge_and_deliver(kind: ActionKind) -> (bool, bool) {
        (kind.charges(), kind.delivers())
    }

    #[test_case(FailureKind::NoReplyTarget; "no_reply_target")]
    #[test_case(FailureKind::GroupFull; "group_full")]
    #[test_case(FailureKind::GoalActive; "goal_active")]
    fn failure_kinds_serialize_as_their_script_names(kind: FailureKind) {
        assert_eq!(serde_json::to_value(kind).unwrap(), json!(kind.as_str()));
    }

    #[test_case(FailureKind::Timeout; "timeout")]
    #[test_case(FailureKind::Transport; "transport")]
    #[test_case(FailureKind::Refused; "refused")]
    #[test_case(FailureKind::RateLimited; "rate_limited")]
    #[test_case(FailureKind::Unavailable; "unavailable")]
    #[test_case(FailureKind::UnknownRecipient; "unknown_recipient")]
    #[test_case(FailureKind::GroupFull; "group_full")]
    #[test_case(FailureKind::ReadOnly; "read_only")]
    #[test_case(FailureKind::NoReplyTarget; "no_reply_target")]
    #[test_case(FailureKind::GoalActive; "goal_active")]
    #[test_case(FailureKind::InvalidArgument; "invalid_argument")]
    #[test_case(FailureKind::Script; "script")]
    fn a_failure_parses_back_from_its_text(kind: FailureKind) {
        let failure = Failure::new(kind, SEPARATED_MESSAGE);
        assert_eq!(failure.to_string().parse::<Failure>(), Ok(failure));
    }

    #[test_case("timeout"; "no_separator")]
    #[test_case("timed_out: the request took too long"; "unknown_kind")]
    #[test_case("Timeout: the request took too long"; "kind_spelled_otherwise")]
    #[test_case(": the request took too long"; "no_kind")]
    fn other_text_is_not_a_failure(text: &str) {
        assert_eq!(text.parse::<Failure>(), Err(MalformedFailure));
    }
}
