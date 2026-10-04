//! Cooperative, same-user live messaging. Socket credentials establish a UID, not
//! that a peer is an authentic Caudra process or has equivalent permissions.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use caudra_config::{Feature, FeatureFlags, InboundPolicy, MessagingConfig};
use caudra_providers::{
    HistoryItem, HistoryItemKind, Message, PeerAssignment, PeerAudience, PeerMessageOrigin,
    UserOrigin,
};
use caudra_storage::id::CaudraId;
pub use caudra_storage::messages::{
    ChannelSummary, HistoryVersion, MessageChannel, QueuedWork, WorkGroup, WorkItem, WorkOutcome,
    WorkState,
};
use caudra_storage::messages::{
    HistoryChannel, MessageAudience, MessageRecipient, MessageSender, NewMessage, StoredMessage,
};
use caudra_storage::sessions::{PermissionMode, StoredInboundPolicy, StoredPeerControls};
use caudra_storage::{derived_phrase, random_task_id};
use event_listener::Event;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::AgentMode;
pub use history::history_retention;
use history::{HistoryWriter, MessageHistory};
use topics::{parse_pattern, parse_topic, pattern_matches, validate_patterns};
pub use work::{
    AssignedWork, COMPLETION_REQUIRED, INVALID_GROUP, MAX_MEMBERSHIPS, MAX_OUTCOME_BYTES,
    ManagedWork, NOT_MEMBER, PAUSED_BY_CANCEL, TOO_MANY_MEMBERSHIPS, WorkAction, WorkNotice,
    parse_group,
};
use work::{Assignments, check_memberships};

// Nothing opens a history where peer messaging is unavailable.
#[cfg_attr(not(unix), allow(dead_code))]
mod history;
pub mod script;
pub mod topics;
#[cfg(unix)]
mod unix;
mod work;

const PROTOCOL_VERSION: u32 = 1;
pub const MAX_BODY_BYTES: usize = 32 * 1024;
const MAX_FRAME_BYTES: usize = 64 * 1024;
const MAX_PENDING: usize = 50;
const MAX_HELD: usize = 50;
const MAX_SESSION_BYTES: usize = 1024 * 1024;
const MAX_PROCESS_BYTES: usize = 8 * 1024 * 1024;
const MAX_SESSIONS: usize = 64;
const MAX_DEDUP: usize = 1024;
const MAX_LABEL_BYTES: usize = 256;
const MAX_PATH_BYTES: usize = 4096;
const MAX_CORRELATION_BYTES: usize = 256;
const MAX_TARGET_BYTES: usize = 128;
const MAX_HANDLE_BYTES: usize = 32;
const HANDLE_PREFIX: char = '@';
const HANDLE_DOMAIN: &str = "caudra.messaging-name.v1";
const MAX_PEER_NAMES: usize = 4096;
const MAX_MESSAGE_NAMES: usize = MAX_DEDUP * 3;
const MAX_NAME_ATTEMPTS: usize = 32;
const MESSAGE_WORDS: usize = 3;
const PEER_WORDS: usize = MESSAGE_WORDS * 2;
const CLAIM_BATCH: usize = 4;
const MAX_CATCH_UP: usize = 16;
pub const MAX_HISTORY_PAGE: usize = 50;
const FANOUT_CONCURRENCY: usize = 8;
const RATE_WINDOW: Duration = Duration::from_secs(60);
const RETRY_WINDOW: Duration = Duration::from_secs(300);
const CLOCK_SKEW: Duration = Duration::from_secs(30);
const CLOSED: &str = "The peer session is closed";
const REFUSED_POLICY: &str = "Inbound messaging is refused by receiver policy";
const HELD_POLICY: &str = "Receiver policy requires local approval";
const HELD_COHORT: &str = "Automatic delivery requires Ask permissions, matching Plan/Build mode, and the same canonical workspace";
const HELD_BLOCKED: &str = "Receiver is blocked; local input must resume it";
const HELD_EXTERNAL: &str = "Automatic delivery admits only sessions; a script's message needs approval or the inbound policy accept";
const RATE_EXCEEDED: &str = "Recipient peer message rate limit reached";
const DUPLICATE: &str = "The same text from this sender arrived within the last minute";
const NOT_SUBSCRIBED: &str = "The recipient is not subscribed to this topic or to broadcasts";
const PUBLISH_RATE_EXCEEDED: &str = "Publication rate limit reached; publish again in a minute";
const DIRECT_PUBLICATION: &str =
    "Publish to a topic or to broadcasts; send direct messages with send_message";
const INVALID_TEXT: &str = "Peer text must contain between 1 byte and 32 KiB of UTF-8";
const INVALID_CORRELATION: &str = "Peer request/correlation identity exceeds its limit";
const SEND_BLOCKED: &str = "Blocked or ReadOnly sessions cannot send peer messages";
const REUSED_REQUEST: &str = "Peer request identity was reused with different content";
const RETRY_INVALIDATED: &str = "Peer retry invalidated by a session policy or workspace change";
const RETRY_EXPIRED: &str = "Peer retry identity expired; do not retry as a new message";
const FULL: &str = "Live inbox capacity reached; no older message was removed";
const RETRY_FULL: &str = "Live retry identity capacity reached; try again once older messages pass the five-minute retry window";
const POLICY_FLOOR: &str = "Cannot weaken the project's configured inbound policy";
const INVALID_TARGET: &str = "Invalid peer target; use an address returned by discovery";
const STALE_TARGET: &str = "Peer target is closed or belongs to an obsolete registration";
const UNKNOWN_TARGET: &str =
    "Unknown peer address; use a target from peer discovery or an incoming peer message";
const UNKNOWN_REPLY: &str =
    "Unknown or expired peer message name for this target; send without reply_to";
const AMBIGUOUS_REPLY: &str = "Peer reply identity is ambiguous without its original sender";
const NAME_COLLISION: &str = "Unable to allocate a unique peer name within the attempt limit";
pub const INVALID_HANDLE: &str = "Messaging names use 1 to 32 lowercase letters, digits, and hyphens, starting with a letter or digit";
const HANDLE_IN_USE: &str = "is in use by another live session";
const ANSWERS_TO: &str = "this session answers to";
const UNTIL_RESUMED: &str = "until it is resumed while that name is free";
const UNNAMED: &str = "this session has no messaging name";
const UNKNOWN_HANDLE: &str = "No live session has this messaging name";
const AMBIGUOUS_HANDLE: &str =
    "Several live sessions advertise this messaging name; use an exact target from list_sessions";
const STALE_REVIEW: &str = "Held message review is stale; inspect the current held messages again";
const NOT_HELD: &str = "Held message no longer exists";
const REJECTED: &str = "Rejected by the local receiver";
const HISTORY_HELD: &str =
    "Reading stored peer messages needs the inbound policy accept or auto in this session";
const NOT_RECORDED: &str = "Not sent, because the message history could not record it";
const STATUS_QUEUED: &str = "queued";
const STATUS_HELD: &str = "held";
const STATUS_REJECTED: &str = "rejected";
const STATUS_DELIVERED: &str = "delivered";
const STATUS_DROPPED: &str = "dropped";
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const UNAVAILABLE: &str = "Local peer messaging is unavailable on this platform";
#[cfg(all(any(test, feature = "test-support"), unix))]
const TEST_HISTORY_DIRECTORY: &str = "history";
static REGISTRATIONS: OnceLock<Mutex<HashMap<CaudraId, Weak<SessionInner>>>> = OnceLock::new();
#[cfg(any(target_os = "linux", target_os = "macos"))]
static PROCESS_BYTES: OnceLock<Arc<AtomicUsize>> = OnceLock::new();

#[derive(Debug, Clone)]
pub struct PeerDescriptor {
    pub session_id: CaudraId,
    pub name: String,
    pub cwd: PathBuf,
    pub mode: AgentMode,
    pub permission_mode: PermissionMode,
    pub inbound: InboundPolicy,
    pub blocked: bool,
    pub busy: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub target: String,
    pub session_id: CaudraId,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
    pub cwd: PathBuf,
    pub busy: bool,
    pub blocked: bool,
    pub inbound: InboundPolicy,
    /// Builds without audiences never advertise these, so publishers never
    /// select a peer that would read a publication as a direct message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub topics: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub broadcasts: bool,
    /// Consumer groups whose work the session takes. Work never travels to
    /// a session; it claims work from the history, so older peers that do
    /// not advertise groups take none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerSummary {
    /// `@name`, or a word target for a peer without a messaging name.
    pub target: String,
    #[serde(alias = "name")]
    pub title: String,
    #[serde(default)]
    pub handle: Option<String>,
    pub cwd: PathBuf,
    pub busy: bool,
    pub blocked: bool,
    pub inbound: InboundPolicy,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub topics: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub broadcasts: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
}

impl PeerSummary {
    /// The `@name` form `send_message` accepts, when the session has a name.
    pub fn handle_address(&self) -> Option<String> {
        self.handle.as_deref().map(handle_address)
    }
}

/// Spells a messaging name the way `send_message` accepts it.
pub fn handle_address(handle: &str) -> String {
    format!("{HANDLE_PREFIX}{handle}")
}

pub fn deceptive(character: char) -> bool {
    matches!(character, '\u{00ad}' | '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
}

/// Peer text for display: terminal controls and invisible or bidirectional
/// characters are spelled out, and only `newlines` keeps line breaks.
pub fn literal(text: &str, newlines: bool) -> String {
    let mut output = String::with_capacity(text.len());
    for character in text.chars() {
        if character == '\n' && newlines {
            output.push(character);
        } else if character.is_control() || deceptive(character) {
            output.extend(character.escape_default());
        } else {
            output.push(character);
        }
    }
    output
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SendReceipt {
    pub status: String,
    pub message_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl SendReceipt {
    fn new(status: &str, message_id: &str, reason: Option<&str>) -> Self {
        Self {
            status: status.to_owned(),
            message_id: message_id.to_owned(),
            reason: reason.map(str::to_owned),
        }
    }
}

/// One publication's outcome across the recipients frozen when it was issued.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PublishReceipt {
    pub message_id: String,
    pub audience: PeerAudience,
    pub recipients: Vec<RecipientReceipt>,
    /// Matching sessions past `max_fanout`, which were not sent the message.
    pub skipped: usize,
    /// Work the publication queued for consumer groups, which keep it
    /// whether or not a member is live. Queued is not done.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued: Vec<QueuedWork>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecipientReceipt {
    /// The address this sender reaches the recipient at; empty for a
    /// script, which can only name it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub target: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TopicActivity {
    pub topic: String,
    pub messages: usize,
    pub last_ms: u64,
}

/// A stored topic or broadcast message. `seq` orders the history and pages
/// it; it identifies no session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredPeerMessage {
    pub seq: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub sender_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_handle: Option<String>,
    /// Sent by a script outside every session.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub external: bool,
    pub sent_ms: u64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PeerHistoryPage {
    /// The topic pattern read, or none for broadcasts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub messages: Vec<StoredPeerMessage>,
    /// Messages on the page this session would not accept automatically,
    /// counted without their text.
    pub withheld: usize,
    /// Pass back as `before` to read older messages; none once the page
    /// reaches the oldest one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<i64>,
}

/// A stored message as the local user browses it: who sent it to which
/// audience, and what became of it for each recipient.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelMessage {
    pub seq: i64,
    pub audience: PeerAudience,
    pub sender_name: String,
    pub sender_handle: Option<String>,
    /// Sent by a script outside every session.
    pub external: bool,
    /// This session sent it.
    pub own: bool,
    pub sent_ms: u64,
    pub text: String,
    pub recipients: Vec<RecipientStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipientStatus {
    pub name: Option<String>,
    /// The messaging name the recipient last reported under.
    pub handle: Option<String>,
    /// This session is the recipient.
    pub own: bool,
    pub status: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelPage {
    pub channel: MessageChannel,
    /// Newest first.
    pub messages: Vec<ChannelMessage>,
    /// Pass back as `before` to read older messages; none once the page
    /// reaches the oldest one.
    pub before: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HeldMessage {
    pub message_id: String,
    pub sender_name: String,
    pub text: String,
    pub reason: String,
    pub epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldMessageSummary {
    pub message_id: String,
    pub sender_name: String,
    /// Empty for a script, which nothing can reply to.
    pub reply_target: String,
    /// Sent by a script outside every session; its mode means nothing.
    pub external: bool,
    pub workspace: Option<PathBuf>,
    pub mode: String,
    pub audience: PeerAudience,
    pub reason: String,
    pub epoch: u64,
    pub approval_blocker: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PeerInboxSnapshot {
    pub messages: Vec<HeldMessageSummary>,
    pub inbound: InboundPolicy,
    pub inbound_override: Option<InboundPolicy>,
    pub project_floor: InboundPolicy,
}

#[derive(Debug, Clone)]
pub struct HeldReview {
    pub summary: HeldMessageSummary,
    pub text: String,
    pub token: PeerReviewToken,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerReviewToken {
    registration: Route,
    sender: Route,
    message_id: String,
    epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerDecision {
    Approve,
    Reject,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerDecisionResult {
    Queued,
    Rejected,
}

#[derive(Clone)]
pub struct PeerHost(Arc<HostInner>);

struct HostInner {
    incarnation: String,
    sessions: Mutex<HashMap<CaudraId, Weak<SessionInner>>>,
    reserved: Mutex<HashMap<CaudraId, HandleClaim>>,
    bytes: Arc<AtomicUsize>,
    changed: Event,
    #[cfg(unix)]
    endpoint: unix::Endpoint,
    #[cfg(unix)]
    listener: Mutex<Option<smol::Task<()>>>,
    history: MessageHistory,
    /// Declared after `history`, so the queue closes before the join.
    _writer: HistoryWriter,
}

#[derive(Clone)]
pub struct PeerSession(Arc<SessionInner>);

struct SessionInner {
    route: Route,
    state: Mutex<SessionState>,
    /// Declared after `state`, so its history clone closes before the last
    /// host drop joins the history writer.
    host: Arc<HostInner>,
}

struct SessionState {
    descriptor: PeerDescriptor,
    canonical_cwd: Option<PathBuf>,
    floor: InboundPolicy,
    inbound_override: Option<InboundPolicy>,
    handle: Option<String>,
    claim: Option<HandleClaim>,
    open: bool,
    wakes_suppressed: bool,
    epoch: u64,
    next_claim: u64,
    inbound_rate: usize,
    sender_rate: usize,
    publish_rate: usize,
    max_fanout: usize,
    topics: Vec<String>,
    broadcasts: bool,
    /// Consumer groups whose work this session takes.
    groups: Vec<String>,
    work: Assignments,
    bytes: usize,
    inbox: VecDeque<InboxItem>,
    dedup: HashMap<String, DedupEntry>,
    outgoing: HashMap<String, Outgoing>,
    publications: HashMap<String, Publication>,
    published: VecDeque<Instant>,
    peer_names: NameTable<String>,
    message_names: NameTable<MessageIdentity>,
    arrivals: VecDeque<Arrival>,
    reviews: HashMap<String, u64>,
    history: MessageHistory,
}

struct Arrival {
    at: Instant,
    /// The sending session, which outlives its routes, so a restart or a
    /// new script run resets neither its rate nor its duplicates.
    session: CaudraId,
    text: [u8; 32],
}

/// Holding the open lock file is what makes the name unique; dropping the
/// claim releases it, as process exit or a crash does.
struct HandleClaim {
    handle: String,
    _lock: File,
}

struct InboxItem {
    delivery: Delivery,
    origin: PeerMessageOrigin,
    bytes: usize,
    state: ItemState,
    approved_epoch: Option<u64>,
    /// Caught up from the history rather than sent to this session. It
    /// rides along with the next turn but never starts one.
    passive: bool,
}

impl InboxItem {
    fn observation(&self) -> Message {
        Message::peer_observation(self.delivery.text.clone(), self.origin.clone())
    }

    fn is_waiting(&self) -> bool {
        !self.passive && matches!(self.state, ItemState::Pending)
    }

    fn receipt(&self) -> SendReceipt {
        match &self.state {
            ItemState::Held(reason) => {
                SendReceipt::new(STATUS_HELD, &self.delivery.message_id, Some(reason))
            }
            ItemState::Pending | ItemState::Claimed(_) | ItemState::Staged => {
                SendReceipt::new(STATUS_QUEUED, &self.delivery.message_id, None)
            }
        }
    }

    fn held_summary(
        &self,
        epoch: u64,
        approval_blocker: Option<&str>,
    ) -> Option<HeldMessageSummary> {
        let ItemState::Held(reason) = &self.state else {
            return None;
        };
        Some(HeldMessageSummary {
            message_id: self.origin.message_id.clone(),
            sender_name: self.delivery.sender.name.clone(),
            reply_target: self.origin.reply_target.clone(),
            external: self.origin.external,
            workspace: self.delivery.sender.canonical_cwd.clone(),
            mode: self.delivery.sender.mode.as_str().into(),
            audience: self.delivery.audience.clone(),
            reason: reason.clone(),
            epoch,
            approval_blocker: approval_blocker.map(str::to_owned),
        })
    }
}

#[derive(Clone, PartialEq, Eq)]
struct MessageIdentity {
    sender: String,
    message_id: String,
    /// Who this session's own message went to, so replies correlate after
    /// the retry record is gone. A publication keeps its frozen recipients,
    /// at most `max_fanout` of them.
    recipients: Vec<String>,
}

impl MessageIdentity {
    fn involves(&self, peer: &str) -> bool {
        self.sender == peer || self.recipients.iter().any(|recipient| recipient == peer)
    }
}

/// Live names for one kind of peer identity. A full table evicts the least
/// recently used entry that no queued message cites. The evicted name is
/// retired for the rest of the registration, so a stale name can miss but
/// never reach a different identity. A retired name costs one hash.
struct NameTable<T> {
    live: HashMap<String, Named<T>>,
    retired: HashSet<u64>,
    capacity: usize,
    clock: u64,
}

struct Named<T> {
    value: T,
    used: u64,
}

/// A name for an identity: already bound, or fresh and not yet bound.
enum Name {
    Bound(String),
    Fresh(String),
}

/// The names a received message goes by, which bind only once it is kept.
struct Naming {
    sender: String,
    identity: MessageIdentity,
    alias: Option<Name>,
    name: Name,
}

impl Naming {
    fn origin(
        &self,
        delivery: &Delivery,
        reply_to: Option<String>,
        assignment: Option<PeerAssignment>,
    ) -> PeerMessageOrigin {
        let reply_target = match &delivery.sender.handle {
            Some(handle) => handle_address(handle),
            None => self.alias.as_ref().map_or("", Name::as_str).to_owned(),
        };
        PeerMessageOrigin {
            message_id: self.name.as_str().to_owned(),
            audience: delivery.audience.clone(),
            sender_name: delivery.sender.name.clone(),
            sender_handle: delivery.sender.handle.clone(),
            reply_target,
            reply_to,
            external: delivery.sender.external,
            assignment,
        }
    }
}

impl Name {
    fn as_str(&self) -> &str {
        match self {
            Self::Bound(name) | Self::Fresh(name) => name,
        }
    }
}

enum ItemState {
    Pending,
    Held(String),
    Claimed(u64),
    Staged,
}

struct DedupEntry {
    fingerprint: [u8; 32],
    issued_ms: u64,
    receipt: SendReceipt,
}

/// A message this session issued. A retry must repeat its content within
/// the retry window, under the session state it was issued in.
#[derive(Clone)]
struct Issued {
    fingerprint: [u8; 32],
    message_id: String,
    issued_ms: u64,
    epoch: u64,
    sender: Sender,
}

impl Issued {
    fn check_retry(&self, fingerprint: &[u8; 32], epoch: u64) -> Result<(), String> {
        if &self.fingerprint != fingerprint {
            Err(REUSED_REQUEST.into())
        } else if self.epoch != epoch {
            Err(RETRY_INVALIDATED.into())
        } else if retry_expired(self.issued_ms, wall_ms()) {
            Err(RETRY_EXPIRED.into())
        } else {
            Ok(())
        }
    }
}

struct Outgoing {
    issued: Issued,
    receipt: Option<SendReceipt>,
}

/// A retry resends only to recipients whose outcome is still unknown, and
/// never rediscovers, so the recipient set stays the one first frozen.
struct Publication {
    issued: Issued,
    /// Recipient routes, in the order of `receipt.recipients`.
    routes: Vec<String>,
    /// The consumer groups the publication may queue work for, which the
    /// fan-out limit leaves after `routes`.
    max_work: usize,
    receipt: PublishReceipt,
}

impl Publication {
    /// The delivery every recipient gets, still without a target.
    fn template(&self, text: &str) -> Delivery {
        Delivery {
            message_id: self.issued.message_id.clone(),
            issued_ms: self.issued.issued_ms,
            target: String::new(),
            sender: self.issued.sender.clone(),
            text: text.to_owned(),
            reply_to: None,
            reply_sender: None,
            audience: self.receipt.audience.clone(),
        }
    }

    fn delivery(&self, index: usize, text: &str) -> Delivery {
        Delivery {
            target: self.routes[index].clone(),
            ..self.template(text)
        }
    }

    fn unresolved(&self, text: &str) -> Vec<(usize, Delivery)> {
        self.receipt
            .recipients
            .iter()
            .enumerate()
            .filter(|(_, recipient)| recipient.status == "unknown")
            .map(|(index, _)| (index, self.delivery(index, text)))
            .collect()
    }

    fn fanout(&self, epoch: u64, text: &str) -> Fanout {
        Fanout {
            epoch,
            max_work: self.max_work,
            receipt: self.receipt.clone(),
            deliveries: self.unresolved(text),
            entry: self.template(text).history_entry(),
            recipients: self
                .routes
                .iter()
                .zip(&self.receipt.recipients)
                .filter_map(|(route, recipient)| {
                    Some(MessageRecipient {
                        session: route_session(route)?,
                        name: Some(recipient.title.clone()),
                        handle: recipient.handle.clone(),
                    })
                })
                .collect(),
        }
    }
}

/// A publication ready to send: deliveries carry their recipient's index
/// in `receipt`, and the history records `entry` for `recipients` first.
struct Fanout {
    epoch: u64,
    max_work: usize,
    receipt: PublishReceipt,
    deliveries: Vec<(usize, Delivery)>,
    entry: NewMessage,
    recipients: Vec<MessageRecipient>,
}

pub struct PeerClaim {
    session: PeerSession,
    claim_id: u64,
    messages: Vec<Message>,
    committed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Route {
    host: String,
    session: CaudraId,
    generation: String,
}

impl Route {
    fn target(&self) -> String {
        format!("p1:{}:{}:{}", self.host, self.session, self.generation)
    }

    fn parse(target: &str) -> Result<Self, String> {
        if target.len() > MAX_TARGET_BYTES {
            return Err(INVALID_TARGET.into());
        }
        let parts: Vec<_> = target.split(':').collect();
        if parts.len() != 4 || parts[0] != "p1" || !valid_token(parts[1]) || !valid_token(parts[3])
        {
            return Err(INVALID_TARGET.into());
        }
        Ok(Self {
            host: parts[1].to_owned(),
            session: parts[2].parse().map_err(|_| INVALID_TARGET.to_owned())?,
            generation: parts[3].to_owned(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireMode {
    Build,
    Plan,
    ReadOnly,
}

impl From<&AgentMode> for WireMode {
    fn from(mode: &AgentMode) -> Self {
        match mode {
            AgentMode::Build => Self::Build,
            AgentMode::Plan(_) | AgentMode::RemotePlan(_) => Self::Plan,
            AgentMode::ReadOnly => Self::ReadOnly,
        }
    }
}

impl WireMode {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Plan => "plan",
            Self::ReadOnly => "read_only",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        [Self::Build, Self::Plan, Self::ReadOnly]
            .into_iter()
            .find(|mode| mode.as_str() == value)
    }
}

fn permission_name(mode: &PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Ask => "ask",
        PermissionMode::Auto => "auto",
        PermissionMode::Yolo => "yolo",
    }
}

fn parse_permission(value: &str) -> Option<PermissionMode> {
    [
        PermissionMode::Ask,
        PermissionMode::Auto,
        PermissionMode::Yolo,
    ]
    .into_iter()
    .find(|mode| permission_name(mode) == value)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Sender {
    route: Route,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    handle: Option<String>,
    canonical_cwd: Option<PathBuf>,
    mode: WireMode,
    permission_mode: PermissionMode,
    /// A script outside every session. Its route reaches nothing, so no
    /// reply can follow; older peers refuse the unknown field.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    external: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Delivery {
    message_id: String,
    issued_ms: u64,
    target: String,
    sender: Sender,
    text: String,
    reply_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reply_sender: Option<String>,
    /// Omitted from direct messages, which every protocol 1 peer reads.
    /// Older peers refuse the unknown field rather than read a publication
    /// as a direct message.
    #[serde(default, skip_serializing_if = "PeerAudience::is_direct")]
    audience: PeerAudience,
}

impl Sender {
    fn history_entry(&self) -> MessageSender {
        MessageSender {
            route: self.route.target(),
            session: self.route.session.to_string(),
            name: self.name.clone(),
            handle: self.handle.clone(),
            cwd: self
                .canonical_cwd
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            mode: self.mode.as_str().into(),
            permission: permission_name(&self.permission_mode).into(),
            external: self.external,
        }
    }

    fn from_history(sender: &MessageSender) -> Option<Self> {
        Some(Self {
            route: Route::parse(&sender.route).ok()?,
            name: sender.name.clone(),
            handle: sender.handle.clone(),
            canonical_cwd: sender.cwd.as_ref().map(PathBuf::from),
            mode: WireMode::parse(&sender.mode)?,
            permission_mode: parse_permission(&sender.permission)?,
            external: sender.external,
        })
    }
}

impl Delivery {
    fn history_entry(&self) -> NewMessage {
        NewMessage {
            message_id: self.message_id.clone(),
            audience: match &self.audience {
                PeerAudience::Direct => MessageAudience::Direct,
                PeerAudience::Topic { topic } => MessageAudience::Topic(topic.clone()),
                PeerAudience::Broadcast => MessageAudience::Broadcast,
            },
            sender: self.sender.history_entry(),
            text: self.text.clone(),
            reply_to: self.reply_to.clone(),
            created_ms: self.issued_ms,
        }
    }

    fn dedup_key(&self) -> String {
        format!("{}:{}", self.sender.route.target(), self.message_id)
    }

    fn same_message(&self, other: &Self) -> bool {
        self.message_id == other.message_id && self.sender.route == other.sender.route
    }

    /// A stored topic message as `target` would have received it live.
    fn from_history(stored: StoredMessage, target: &Route) -> Option<Self> {
        let message = stored.message;
        let MessageAudience::Topic(topic) = message.audience else {
            return None;
        };
        let delivery = Self {
            message_id: message.message_id,
            issued_ms: message.created_ms,
            target: target.target(),
            sender: Sender::from_history(&message.sender)?,
            text: message.text,
            reply_to: None,
            reply_sender: None,
            audience: PeerAudience::Topic { topic },
        };
        delivery.has_valid_metadata().then_some(delivery)
    }

    fn has_valid_metadata(&self) -> bool {
        (valid_name(&self.message_id, MESSAGE_WORDS) || valid_token(&self.message_id))
            && valid_token(&self.sender.route.host)
            && valid_token(&self.sender.route.generation)
            && !self.text.is_empty()
            && self.text.len() <= MAX_BODY_BYTES
            && self.sender.name.len() <= MAX_LABEL_BYTES
            && self.sender.handle.as_deref().is_none_or(valid_handle)
            && self
                .sender
                .canonical_cwd
                .as_ref()
                .is_none_or(|path| path.is_absolute() && path.as_os_str().len() <= MAX_PATH_BYTES)
            && self
                .reply_to
                .as_ref()
                .is_none_or(|value| value.len() <= MAX_CORRELATION_BYTES)
            && self
                .reply_sender
                .as_deref()
                .is_none_or(|sender| self.reply_to.is_some() && Route::parse(sender).is_ok())
            && self
                .audience
                .topic()
                .is_none_or(|topic| parse_topic(topic).is_ok())
            && (self.audience.is_direct() || self.reply_to.is_none())
            && (!self.sender.external
                || (self.reply_to.is_none()
                    && self.reply_sender.is_none()
                    && self.sender.handle.is_none()))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    List {
        version: u32,
        host: String,
    },
    Send {
        version: u32,
        delivery: Box<Delivery>,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Response {
    Peers { version: u32, peers: Vec<PeerInfo> },
    Receipt { receipt: SendReceipt },
    Error { reason: String },
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn registrations() -> &'static Mutex<HashMap<CaudraId, Weak<SessionInner>>> {
    REGISTRATIONS.get_or_init(Mutex::default)
}

fn token() -> Result<String, String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| format!("Peer randomness unavailable: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn valid_token(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn valid_name(value: &str, words: usize) -> bool {
    value.len() <= MAX_TARGET_BYTES
        && value.split('-').count() == words
        && value
            .split('-')
            .all(|word| !word.is_empty() && word.bytes().all(|byte| byte.is_ascii_lowercase()))
}

fn valid_handle(value: &str) -> bool {
    value.len() <= MAX_HANDLE_BYTES
        && value
            .bytes()
            .next()
            .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// Validates a unique messaging name, as `--name` and `@name` spell it.
pub fn parse_handle(value: &str) -> Result<String, String> {
    if valid_handle(value) {
        Ok(value.to_owned())
    } else {
        Err(INVALID_HANDLE.into())
    }
}

/// Validates a messaging name written with or without its `@`.
pub fn parse_handle_address(value: &str) -> Result<String, String> {
    parse_handle(value.strip_prefix(HANDLE_PREFIX).unwrap_or(value))
}

fn handle_in_use(handle: &str) -> String {
    format!("Messaging name {HANDLE_PREFIX}{handle} {HANDLE_IN_USE}")
}

/// The names a session without a free stored name tries, in order. The
/// first is the session's own on every run, so it needs no saving; the rest
/// only matter while the same session is open twice.
fn generated_handles(session: CaudraId) -> impl Iterator<Item = String> {
    (0..MAX_NAME_ATTEMPTS as u64).map(move |attempt| {
        derived_phrase(
            HANDLE_DOMAIN,
            &[session.as_bytes().as_slice(), &attempt.to_be_bytes()].concat(),
        )
    })
}

fn message_name() -> Result<String, String> {
    random_task_id().map_err(|error| format!("Peer randomness unavailable: {error}"))
}

fn peer_name() -> Result<String, String> {
    Ok(format!("{}-{}", message_name()?, message_name()?))
}

impl<T> NameTable<T> {
    fn new(capacity: usize) -> Self {
        Self {
            live: HashMap::new(),
            retired: HashSet::new(),
            capacity,
            clock: 0,
        }
    }

    fn get(&mut self, name: &str) -> Option<&T> {
        let named = self.live.get_mut(name)?;
        self.clock += 1;
        named.used = self.clock;
        Some(&named.value)
    }

    fn find(&mut self, matches: impl Fn(&T) -> bool) -> Option<String> {
        let (name, named) = self
            .live
            .iter_mut()
            .find(|(_, named)| matches(&named.value))?;
        self.clock += 1;
        named.used = self.clock;
        Some(name.clone())
    }

    fn count(&self, matches: impl Fn(&T) -> bool) -> usize {
        self.live
            .values()
            .filter(|named| matches(&named.value))
            .count()
    }

    /// The bound name for `value`, or a fresh one the caller must bind.
    fn name<Q: ?Sized>(
        &mut self,
        value: &Q,
        preferred: Option<&str>,
        candidate: impl FnMut() -> Result<String, String>,
    ) -> Result<Name, String>
    where
        T: PartialEq<Q>,
    {
        match self.find(|known| known == value) {
            Some(name) => Ok(Name::Bound(name)),
            None => self.fresh(preferred, candidate).map(Name::Fresh),
        }
    }

    /// A name that is neither live nor retired.
    fn fresh(
        &self,
        preferred: Option<&str>,
        mut candidate: impl FnMut() -> Result<String, String>,
    ) -> Result<String, String> {
        let free = |name: &str| {
            !self.live.contains_key(name) && !self.retired.contains(&retired_key(name))
        };
        if let Some(preferred) = preferred.filter(|name| free(name)) {
            return Ok(preferred.to_owned());
        }
        for _ in 0..MAX_NAME_ATTEMPTS {
            let name = candidate()?;
            if free(&name) {
                return Ok(name);
            }
        }
        Err(NAME_COLLISION.into())
    }

    /// Binds a name from [`Self::fresh`]. Queued messages cite at most a few
    /// names per item, far below capacity, so an unpinned entry is always
    /// there to evict.
    fn bind(&mut self, name: String, value: T, pinned: impl Fn(&str) -> bool) {
        if self.live.len() >= self.capacity
            && let Some(evicted) = self
                .live
                .iter()
                .filter(|(name, _)| !pinned(name))
                .min_by_key(|(_, named)| named.used)
                .map(|(name, _)| name.clone())
        {
            self.live.remove(&evicted);
            self.retired.insert(retired_key(&evicted));
        }
        self.clock += 1;
        self.live.insert(
            name,
            Named {
                value,
                used: self.clock,
            },
        );
    }
}

fn retired_key(name: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    name.hash(&mut hasher);
    hasher.finish()
}

fn retry_expired(issued_ms: u64, now_ms: u64) -> bool {
    now_ms.saturating_sub(issued_ms) > RETRY_WINDOW.as_millis() as u64
}

/// Forgets identities no retry can still present, once the table is full.
fn has_retry_room<V>(
    table: &mut HashMap<String, V>,
    now_ms: u64,
    issued_ms: impl Fn(&V) -> u64,
) -> bool {
    if table.len() >= MAX_DEDUP {
        table.retain(|_, value| !retry_expired(issued_ms(value), now_ms));
    }
    table.len() < MAX_DEDUP
}

fn wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn check_text(text: &str) -> Result<(), String> {
    if text.is_empty() || text.len() > MAX_BODY_BYTES {
        Err(INVALID_TEXT.into())
    } else {
        Ok(())
    }
}

fn digest(content: &impl Serialize) -> Result<[u8; 32], String> {
    Ok(Sha256::digest(serde_json::to_vec(content).map_err(|error| error.to_string())?).into())
}

fn check_publication(audience: &PeerAudience, text: &str) -> Result<(), String> {
    match audience {
        PeerAudience::Direct => return Err(DIRECT_PUBLICATION.into()),
        PeerAudience::Topic { topic } => {
            parse_topic(topic)?;
        }
        PeerAudience::Broadcast => {}
    }
    check_text(text)
}

/// Whether a session with these subscriptions consents to be addressed.
fn subscribed(audience: &PeerAudience, topics: &[String], broadcasts: bool) -> bool {
    match audience {
        PeerAudience::Direct => true,
        PeerAudience::Topic { topic } => {
            topics.iter().any(|pattern| pattern_matches(pattern, topic))
        }
        PeerAudience::Broadcast => broadcasts,
    }
}

/// The sessions `audience` reaches: the first `max_fanout`, and how many
/// more matched.
fn audience_members(
    peers: Vec<PeerInfo>,
    audience: &PeerAudience,
    max_fanout: usize,
) -> (Vec<PeerInfo>, usize) {
    let mut matching = peers
        .into_iter()
        .filter(|peer| subscribed(audience, &peer.topics, peer.broadcasts));
    let selected = matching.by_ref().take(max_fanout).collect();
    (selected, matching.count())
}

/// How many live sessions a publication may reach once `groups` consumer
/// groups take their share of `max_fanout`. Groups are never cut short, so
/// more of them than that refuses the publication.
fn recipient_room(groups: usize, max_fanout: usize) -> Result<usize, String> {
    max_fanout.checked_sub(groups).ok_or_else(|| {
        format!(
            "The topic feeds {groups} consumer groups, more than the {max_fanout} destinations one publication may reach"
        )
    })
}

/// Forgets the messaging names more than one of `peers` advertises: such a
/// name reaches none of them, so they are addressed by word target instead.
fn forget_shared_handles(peers: &mut [PeerInfo]) {
    let mut seen = HashSet::new();
    let shared: HashSet<_> = peers
        .iter()
        .filter_map(|peer| peer.handle.clone())
        .filter(|handle| !seen.insert(handle.clone()))
        .collect();
    for peer in peers {
        peer.handle
            .take_if(|handle| shared.contains(handle.as_str()));
    }
}

/// The one live session holding the messaging name `handle`.
fn holder(peers: Vec<PeerInfo>, handle: &str) -> Result<PeerInfo, String> {
    let mut holders = peers
        .into_iter()
        .filter(|peer| peer.handle.as_deref() == Some(handle));
    let holder = holders.next().ok_or(UNKNOWN_HANDLE)?;
    if holders.next().is_some() {
        return Err(AMBIGUOUS_HANDLE.into());
    }
    Ok(holder)
}

/// The session `route` addresses, which keys its rows in the history.
fn route_session(route: &str) -> Option<String> {
    Route::parse(route)
        .ok()
        .map(|route| route.session.to_string())
}

fn peer_audience(audience: MessageAudience) -> PeerAudience {
    match audience {
        MessageAudience::Direct => PeerAudience::Direct,
        MessageAudience::Topic(topic) => PeerAudience::Topic { topic },
        MessageAudience::Broadcast => PeerAudience::Broadcast,
    }
}

fn validate_descriptor(descriptor: &PeerDescriptor) -> Result<(), String> {
    if descriptor.name.len() > MAX_LABEL_BYTES || descriptor.cwd.as_os_str().len() > MAX_PATH_BYTES
    {
        return Err("Peer name or workspace path exceeds the metadata limit".into());
    }
    if matches!(descriptor.mode, AgentMode::RemotePlan(_)) {
        return Err("Remote workspaces cannot register local peer messaging".into());
    }
    Ok(())
}

/// Whether `auto` admits `sender` without review: both sessions share a
/// canonical workspace and Plan or Build mode under Ask permissions. A script
/// runs outside every session, so it never qualifies.
fn same_cohort(
    mode: &AgentMode,
    canonical_cwd: Option<&Path>,
    permission_mode: &PermissionMode,
    sender: &Sender,
) -> bool {
    !sender.external
        && matches!(
            (&sender.mode, WireMode::from(mode)),
            (WireMode::Build, WireMode::Build) | (WireMode::Plan, WireMode::Plan)
        )
        && canonical_cwd.is_some()
        && canonical_cwd == sender.canonical_cwd.as_deref()
        && *permission_mode == PermissionMode::Ask
        && sender.permission_mode == PermissionMode::Ask
}

/// Why `inbound` holds what `sender` sent for review, if it does.
fn policy_hold(
    inbound: &InboundPolicy,
    sender: &Sender,
    same_cohort: bool,
) -> Option<&'static str> {
    match inbound {
        InboundPolicy::Accept => None,
        InboundPolicy::Hold | InboundPolicy::Refuse => Some(HELD_POLICY),
        InboundPolicy::Auto if sender.external => Some(HELD_EXTERNAL),
        InboundPolicy::Auto if same_cohort => None,
        InboundPolicy::Auto => Some(HELD_COHORT),
    }
}

fn stored_policy(policy: &InboundPolicy) -> StoredInboundPolicy {
    match policy {
        InboundPolicy::Accept => StoredInboundPolicy::Accept,
        InboundPolicy::Auto => StoredInboundPolicy::Auto,
        InboundPolicy::Hold => StoredInboundPolicy::Hold,
        InboundPolicy::Refuse => StoredInboundPolicy::Refuse,
    }
}

fn inbound_policy(policy: &StoredInboundPolicy) -> InboundPolicy {
    match policy {
        StoredInboundPolicy::Accept => InboundPolicy::Accept,
        StoredInboundPolicy::Auto => InboundPolicy::Auto,
        StoredInboundPolicy::Hold => InboundPolicy::Hold,
        StoredInboundPolicy::Refuse => InboundPolicy::Refuse,
    }
}

impl PeerHost {
    /// Starts messaging when the experiment is on. Fails when the message
    /// history cannot open, since nothing may be sent unrecorded.
    pub fn start(
        features: FeatureFlags,
        messaging: &MessagingConfig,
    ) -> Result<Option<Self>, String> {
        if !features.enabled(Feature::CrossSessionMessaging) {
            return Ok(None);
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            Self::bind(
                unix::runtime_directory()?,
                MessageHistory::open_shared(messaging)?,
                PROCESS_BYTES
                    .get_or_init(|| Arc::new(AtomicUsize::new(0)))
                    .clone(),
            )
            .map(Some)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = messaging;
            Err(UNAVAILABLE.into())
        }
    }

    #[cfg(unix)]
    fn bind(
        directory: PathBuf,
        (history, writer): (MessageHistory, HistoryWriter),
        bytes: Arc<AtomicUsize>,
    ) -> Result<Self, String> {
        let incarnation = token()?;
        let (endpoint, listener) =
            unix::Endpoint::bind(unix::Directory::open(directory)?, &incarnation)?;
        let host = Arc::new(HostInner {
            incarnation,
            sessions: Mutex::default(),
            reserved: Mutex::default(),
            bytes,
            changed: Event::new(),
            endpoint,
            listener: Mutex::new(None),
            history,
            _writer: writer,
        });
        *lock(&host.listener) = Some(unix::listen(Arc::downgrade(&host), listener));
        Ok(Self(host))
    }

    /// Keeps the message history under `directory` too, so hosts started in
    /// one directory share a history as every host on a machine does.
    #[cfg(all(any(test, feature = "test-support"), unix))]
    pub fn start_in(directory: PathBuf, bytes: Arc<AtomicUsize>) -> Result<Self, String> {
        let history = MessageHistory::open_in(
            &directory.join(TEST_HISTORY_DIRECTORY),
            &MessagingConfig::default(),
        )?;
        Self::bind(directory, history, bytes)
    }

    pub fn register(&self, descriptor: PeerDescriptor) -> Result<PeerSession, String> {
        self.register_with_controls(descriptor, &MessagingConfig::default(), None)
    }

    pub fn register_with_controls(
        &self,
        mut descriptor: PeerDescriptor,
        messaging: &MessagingConfig,
        controls: Option<StoredPeerControls>,
    ) -> Result<PeerSession, String> {
        validate_descriptor(&descriptor)?;
        let floor = messaging
            .project_inbound
            .clone()
            .unwrap_or(InboundPolicy::Accept);
        let controls = controls.unwrap_or_default();
        validate_patterns(&controls.topics)?;
        check_memberships(&controls.groups)?;
        let inbound_override = controls
            .inbound
            .as_ref()
            .map(|policy| inbound_policy(policy).max(floor.clone()));
        descriptor.inbound = inbound_override
            .clone()
            .unwrap_or(descriptor.inbound)
            .max(floor.clone());
        let mut registry = lock(registrations());
        registry.retain(|_, session| session.strong_count() > 0);
        if registry
            .get(&descriptor.session_id)
            .and_then(Weak::upgrade)
            .is_some_and(|session| lock(&session.state).open)
        {
            return Err("This session already has a live peer registration".into());
        }
        let mut sessions = lock(&self.0.sessions);
        sessions.retain(|_, session| {
            session
                .upgrade()
                .is_some_and(|session| lock(&session.state).open)
        });
        if sessions.len() >= MAX_SESSIONS {
            return Err("Process peer registration limit reached".into());
        }
        let id = descriptor.session_id;
        let session = Arc::new(SessionInner {
            host: self.0.clone(),
            route: Route {
                host: self.0.incarnation.clone(),
                session: id,
                generation: token()?,
            },
            state: Mutex::new(SessionState {
                canonical_cwd: descriptor.cwd.canonicalize().ok(),
                floor,
                inbound_override,
                handle: controls.handle,
                claim: None,
                descriptor,
                open: true,
                wakes_suppressed: false,
                epoch: 0,
                next_claim: 0,
                inbound_rate: messaging.inbound_per_minute,
                sender_rate: messaging.sender_per_minute,
                publish_rate: messaging.publish_per_minute,
                max_fanout: messaging.max_fanout,
                topics: controls.topics,
                broadcasts: controls.broadcasts,
                groups: controls.groups,
                work: Assignments::default(),
                bytes: 0,
                inbox: VecDeque::new(),
                dedup: HashMap::new(),
                outgoing: HashMap::new(),
                publications: HashMap::new(),
                published: VecDeque::new(),
                peer_names: NameTable::new(MAX_PEER_NAMES),
                message_names: NameTable::new(MAX_MESSAGE_NAMES),
                arrivals: VecDeque::new(),
                reviews: HashMap::new(),
                history: self.0.history.clone(),
            }),
        });
        sessions.insert(id, Arc::downgrade(&session));
        registry.insert(id, Arc::downgrade(&session));
        Ok(PeerSession(session))
    }

    pub fn notified(&self) -> impl Future<Output = ()> + Send + 'static {
        self.0.changed.listen()
    }

    /// Claims `handle` for `session` before it registers, so a conflict can stop
    /// startup. The session's [`PeerSession::claim_handle`] takes the claim over.
    pub async fn reserve_handle(&self, session: CaudraId, handle: &str) -> Result<(), String> {
        if let Some(claim) = self.0.claim(session, handle)? {
            lock(&self.0.reserved).insert(session, claim);
            return Ok(());
        }
        let holder = self.0.discover().await.ok().and_then(|peers| {
            peers
                .into_iter()
                .find(|peer| peer.handle.as_deref() == Some(handle))
        });
        Err(match holder {
            Some(peer) => format!(
                "Messaging name {HANDLE_PREFIX}{handle} is in use by {:?} in {:?}",
                peer.name, peer.cwd
            ),
            None => handle_in_use(handle),
        })
    }
}

impl PeerSession {
    pub fn lookup(id: CaudraId) -> Option<Self> {
        let session = lock(registrations()).get(&id).and_then(Weak::upgrade)?;
        let open = lock(&session.state).open;
        open.then_some(Self(session))
    }

    pub fn session_id(&self) -> CaudraId {
        self.0.route.session
    }

    pub fn descriptor(&self) -> PeerDescriptor {
        lock(&self.0.state).descriptor.clone()
    }

    pub fn controls(&self) -> StoredPeerControls {
        let state = lock(&self.0.state);
        StoredPeerControls {
            inbound: state.inbound_override.as_ref().map(stored_policy),
            handle: state.handle.clone(),
            topics: state.topics.clone(),
            broadcasts: state.broadcasts,
            groups: state.groups.clone(),
        }
    }

    /// Publishers select recipients from discovery, so one that has not
    /// rediscovered may still address this session; it then refuses what it
    /// no longer subscribes to.
    pub fn set_subscriptions(&self, topics: Vec<String>, broadcasts: bool) -> Result<(), String> {
        validate_patterns(&topics)?;
        let mut state = lock(&self.0.state);
        state.ensure_open()?;
        state.topics = topics;
        state.broadcasts = broadcasts;
        self.0.host.changed.notify(usize::MAX);
        Ok(())
    }

    /// Claims the messaging name this registration answers to: the stored
    /// name from `--name` while no other live session holds it, else the
    /// first free name generated from the session ID. A stored name passed
    /// over stays stored, so a later registration reclaims it once it is
    /// free, and the error names the one the session answers to meanwhile.
    pub fn claim_handle(&self) -> Result<(), String> {
        let stored = {
            let state = lock(&self.0.state);
            state.ensure_open()?;
            if state.claim.is_some() {
                return Ok(());
            }
            state.handle.clone()
        };
        let session = self.session_id();
        let passed_over = match stored {
            Some(stored) => match self.0.host.claim(session, &stored) {
                Ok(Some(claim)) => return self.hold(claim),
                Ok(None) => Some(handle_in_use(&stored)),
                Err(error) => Some(error),
            },
            None => None,
        };
        let claim = match generated_handles(session)
            .find_map(|handle| self.0.host.claim(session, &handle).transpose())
        {
            Some(Ok(claim)) => claim,
            Some(Err(error)) => return Err(format!("{error}; {UNNAMED}")),
            None => return Err(format!("{NAME_COLLISION}; {UNNAMED}")),
        };
        let address = handle_address(&claim.handle);
        self.hold(claim)?;
        match passed_over {
            Some(reason) => Err(format!("{reason}; {ANSWERS_TO} {address} {UNTIL_RESUMED}")),
            None => Ok(()),
        }
    }

    fn hold(&self, claim: HandleClaim) -> Result<(), String> {
        let mut state = lock(&self.0.state);
        state.ensure_open()?;
        state.claim = Some(claim);
        self.0.host.changed.notify(usize::MAX);
        Ok(())
    }

    /// The messaging name this registration answers to, which may differ
    /// from the stored one in [`Self::controls`].
    pub fn handle(&self) -> Option<String> {
        lock(&self.0.state).claimed_handle()
    }

    pub fn wakes_suppressed(&self) -> bool {
        lock(&self.0.state).wakes_suppressed
    }

    pub fn suppress_wakes(&self) {
        let mut state = lock(&self.0.state);
        if state.open && !state.wakes_suppressed {
            state.wakes_suppressed = true;
            state.epoch += 1;
            state.reevaluate();
            self.0.host.changed.notify(usize::MAX);
        }
    }

    pub fn update(&self, descriptor: PeerDescriptor) -> Result<(), String> {
        validate_descriptor(&descriptor)?;
        let canonical_cwd = descriptor.cwd.canonicalize().ok();
        let mut state = lock(&self.0.state);
        state.ensure_open()?;
        if descriptor.session_id != self.session_id() {
            return Err("A peer registration cannot change session ID".into());
        }
        if descriptor.inbound < state.floor {
            return Err(POLICY_FLOOR.into());
        }
        if descriptor.mode != state.descriptor.mode
            || descriptor.permission_mode != state.descriptor.permission_mode
            || descriptor.cwd != state.descriptor.cwd
            || canonical_cwd != state.canonical_cwd
            || descriptor.inbound != state.descriptor.inbound
            || descriptor.blocked != state.descriptor.blocked
        {
            state.epoch += 1;
        }
        state.descriptor = descriptor;
        state.canonical_cwd = canonical_cwd;
        state.reevaluate();
        self.0.host.changed.notify(usize::MAX);
        Ok(())
    }

    pub fn set_inbound(&self, policy: InboundPolicy) -> Result<(), String> {
        let mut state = lock(&self.0.state);
        state.ensure_open()?;
        if policy < state.floor {
            return Err(POLICY_FLOOR.into());
        }
        if state.inbound_override.as_ref() != Some(&policy) || policy != state.descriptor.inbound {
            state.inbound_override = Some(policy.clone());
            state.epoch += 1;
            state.descriptor.inbound = policy;
            state.reevaluate();
        }
        self.0.host.changed.notify(usize::MAX);
        Ok(())
    }

    pub fn close(&self) {
        self.0.close();
    }

    /// Whether a message or claimed work waits to wake the session.
    /// Caught-up messages only ride along with a turn something else starts.
    pub fn has_pending(&self) -> bool {
        let mut state = lock(&self.0.state);
        state.reevaluate();
        state.open && state.has_waiting()
    }

    pub async fn list(&self) -> Result<Vec<PeerInfo>, String> {
        lock(&self.0.state).ensure_open()?;
        let peers = self.0.host.discover().await?;
        lock(&self.0.state).ensure_open()?;
        Ok(peers
            .into_iter()
            .filter(|peer| peer.target != self.0.route.target())
            .collect())
    }

    pub async fn list_named(&self) -> Result<Vec<PeerSummary>, String> {
        let mut peers = self.list().await?;
        forget_shared_handles(&mut peers);
        let mut state = lock(&self.0.state);
        state.ensure_open()?;
        peers
            .into_iter()
            .map(|peer| {
                Ok(PeerSummary {
                    target: state.address(&peer)?,
                    title: peer.name,
                    handle: peer.handle,
                    cwd: peer.cwd,
                    busy: peer.busy,
                    blocked: peer.blocked,
                    inbound: peer.inbound,
                    topics: peer.topics,
                    broadcasts: peer.broadcasts,
                    groups: peer.groups,
                })
            })
            .collect()
    }

    pub async fn send_named(
        &self,
        target: &str,
        text: &str,
        reply_to: Option<&str>,
        request_id: &str,
    ) -> Result<SendReceipt, String> {
        let route = match target.strip_prefix(HANDLE_PREFIX) {
            Some(handle) => self.resolve_handle(handle).await?,
            None if valid_name(target, PEER_WORDS) => {
                let mut state = lock(&self.0.state);
                state.ensure_open()?;
                state.peer_names.get(target).ok_or(UNKNOWN_TARGET)?.clone()
            }
            None => return Err(UNKNOWN_TARGET.into()),
        };
        let reply_to = {
            let mut state = lock(&self.0.state);
            state.ensure_open()?;
            reply_to
                .map(|name| {
                    state
                        .message_names
                        .get(name)
                        .filter(|identity| identity.involves(&route))
                        .cloned()
                        .ok_or(UNKNOWN_REPLY)
                })
                .transpose()?
        };
        self.send_with_reply(
            &route,
            text,
            reply_to
                .as_ref()
                .map(|identity| identity.message_id.as_str()),
            reply_to.as_ref().map(|identity| identity.sender.as_str()),
            request_id,
        )
        .await
    }

    /// Names follow their session across restarts, so each send rediscovers
    /// the live registration that currently holds one.
    async fn resolve_handle(&self, handle: &str) -> Result<String, String> {
        let handle = parse_handle(handle)?;
        Ok(holder(self.list().await?, &handle)?.target)
    }

    pub async fn send(
        &self,
        target: &str,
        text: &str,
        reply_to: Option<&str>,
        request_id: &str,
    ) -> Result<SendReceipt, String> {
        self.send_with_reply(target, text, reply_to, None, request_id)
            .await
    }

    async fn send_with_reply(
        &self,
        target: &str,
        text: &str,
        reply_to: Option<&str>,
        reply_sender: Option<&str>,
        request_id: &str,
    ) -> Result<SendReceipt, String> {
        let recipient = Route::parse(target)?.session.to_string();
        check_text(text)?;
        if request_id.is_empty()
            || request_id.len() > MAX_CORRELATION_BYTES
            || reply_to.is_some_and(|value| value.len() > MAX_CORRELATION_BYTES)
        {
            return Err(INVALID_CORRELATION.into());
        }
        let fingerprint = digest(&(target, text, reply_to, reply_sender))?;
        let (delivery, epoch, entry) = {
            let mut state = lock(&self.0.state);
            state.ensure_can_send()?;
            let issued = if let Some(previous) = state.outgoing.get(request_id) {
                previous.issued.check_retry(&fingerprint, state.epoch)?;
                if let Some(receipt) = &previous.receipt
                    && receipt.status != "unknown"
                {
                    return Ok(receipt.clone());
                }
                previous.issued.clone()
            } else {
                let issued_ms = wall_ms();
                if !has_retry_room(&mut state.outgoing, issued_ms, |outgoing| {
                    outgoing.issued.issued_ms
                }) {
                    return Err(RETRY_FULL.into());
                }
                let message_id = state.message_names.fresh(None, message_name)?;
                state.bind_message(
                    message_id.clone(),
                    MessageIdentity {
                        sender: self.0.route.target(),
                        message_id: message_id.clone(),
                        recipients: vec![target.to_owned()],
                    },
                );
                let issued = Issued {
                    fingerprint,
                    message_id,
                    issued_ms,
                    epoch: state.epoch,
                    sender: state.sender(&self.0.route),
                };
                state.outgoing.insert(
                    request_id.to_owned(),
                    Outgoing {
                        issued: issued.clone(),
                        receipt: None,
                    },
                );
                issued
            };
            let delivery = Delivery {
                message_id: issued.message_id,
                issued_ms: issued.issued_ms,
                target: target.to_owned(),
                sender: issued.sender,
                text: text.to_owned(),
                reply_to: reply_to.map(str::to_owned),
                reply_sender: reply_sender.map(str::to_owned),
                audience: PeerAudience::Direct,
            };
            let entry = delivery.history_entry();
            (delivery, issued.epoch, entry)
        };
        let history = &self.0.host.history;
        let recipients = vec![MessageRecipient {
            session: recipient.clone(),
            name: None,
            handle: None,
        }];
        history
            .record(entry, recipients)
            .await
            .map_err(|error| format!("{NOT_RECORDED}: {error}"))?;
        let (sender, message_id) = (delivery.sender.route.target(), delivery.message_id.clone());
        let receipt = self.deliver(delivery, epoch).await;
        history.receipt(
            sender,
            message_id,
            recipient,
            receipt.status.clone(),
            receipt.reason.clone(),
        );
        if let Some(outgoing) = lock(&self.0.state).outgoing.get_mut(request_id) {
            outgoing.receipt = Some(receipt.clone());
        }
        Ok(receipt)
    }

    /// Sends one message to every live session subscribed to `audience` at
    /// discovery, up to `max_fanout`. A retry with the same `request_id`
    /// reuses the recipients first selected and resends only unknown outcomes.
    pub async fn publish(
        &self,
        audience: PeerAudience,
        text: &str,
        request_id: &str,
    ) -> Result<PublishReceipt, String> {
        check_publication(&audience, text)?;
        if request_id.is_empty() || request_id.len() > MAX_CORRELATION_BYTES {
            return Err(INVALID_CORRELATION.into());
        }
        let fingerprint = digest(&(&audience, text))?;
        let retry = {
            let mut state = lock(&self.0.state);
            state.ensure_can_send()?;
            let retry = state.publications.contains_key(request_id);
            if !retry && !state.has_publish_room(Instant::now()) {
                return Err(PUBLISH_RATE_EXCEEDED.into());
            }
            retry
        };
        let history = &self.0.host.history;
        let destinations = if retry {
            None
        } else {
            Some((
                self.list().await?,
                history
                    .group_destinations(&audience)
                    .await
                    .map_err(|error| format!("{NOT_RECORDED}: {error}"))?,
            ))
        };
        let Fanout {
            epoch,
            max_work,
            mut receipt,
            deliveries,
            entry,
            recipients,
        } = self.prepare_publication(audience, text, request_id, fingerprint, destinations)?;
        receipt.queued = history
            .record_publication(entry, recipients, max_work)
            .await
            .map_err(|error| format!("{NOT_RECORDED}: {error}"))?;
        let sender = self.0.route.target();
        for batch in deliveries.chunks(FANOUT_CONCURRENCY) {
            let sends: Vec<_> = batch
                .iter()
                .cloned()
                .map(|(index, delivery)| {
                    let session = self.clone();
                    let recipient = route_session(&delivery.target);
                    smol::spawn(async move {
                        (index, recipient, session.deliver(delivery, epoch).await)
                    })
                })
                .collect();
            for send in sends {
                let (index, recipient_session, sent) = send.await;
                if let Some(recipient_session) = recipient_session {
                    history.receipt(
                        sender.clone(),
                        receipt.message_id.clone(),
                        recipient_session,
                        sent.status.clone(),
                        sent.reason.clone(),
                    );
                }
                let recipient = &mut receipt.recipients[index];
                recipient.status = sent.status;
                recipient.reason = sent.reason;
            }
        }
        if let Some(publication) = lock(&self.0.state).publications.get_mut(request_id) {
            publication.receipt = receipt.clone();
        }
        Ok(receipt)
    }

    /// Recovers the publication `request_id` names, or records a new one
    /// for the live `peers` and the consumer groups `audience` reaches, which
    /// only a first attempt discovers.
    fn prepare_publication(
        &self,
        audience: PeerAudience,
        text: &str,
        request_id: &str,
        fingerprint: [u8; 32],
        destinations: Option<(Vec<PeerInfo>, usize)>,
    ) -> Result<Fanout, String> {
        let mut state = lock(&self.0.state);
        state.ensure_can_send()?;
        let epoch = state.epoch;
        if let Some(previous) = state.publications.get(request_id) {
            previous.issued.check_retry(&fingerprint, epoch)?;
            return Ok(previous.fanout(epoch, text));
        }
        let (mut peers, groups) = destinations.ok_or(RETRY_EXPIRED)?;
        let room = recipient_room(groups, state.max_fanout)?;
        let now = Instant::now();
        if !state.has_publish_room(now) {
            return Err(PUBLISH_RATE_EXCEEDED.into());
        }
        let issued_ms = wall_ms();
        if !has_retry_room(&mut state.publications, issued_ms, |publication| {
            publication.issued.issued_ms
        }) {
            return Err(RETRY_FULL.into());
        }
        forget_shared_handles(&mut peers);
        let (selected, skipped) = audience_members(peers, &audience, room);
        let message_id = state.message_names.fresh(None, message_name)?;
        let mut routes = Vec::with_capacity(selected.len());
        let mut recipients = Vec::with_capacity(selected.len());
        for peer in selected {
            recipients.push(RecipientReceipt {
                target: state.address(&peer)?,
                title: peer.name,
                handle: peer.handle,
                status: "unknown".into(),
                reason: None,
            });
            routes.push(peer.target);
        }
        state.bind_message(
            message_id.clone(),
            MessageIdentity {
                sender: self.0.route.target(),
                message_id: message_id.clone(),
                recipients: routes.clone(),
            },
        );
        let publication = Publication {
            issued: Issued {
                fingerprint,
                message_id: message_id.clone(),
                issued_ms,
                epoch,
                sender: state.sender(&self.0.route),
            },
            max_work: state.max_fanout - routes.len(),
            routes,
            receipt: PublishReceipt {
                message_id,
                audience,
                recipients,
                skipped,
                queued: Vec::new(),
            },
        };
        let fanout = publication.fanout(epoch, text);
        state.published.push_back(now);
        state
            .publications
            .insert(request_id.to_owned(), publication);
        Ok(fanout)
    }

    /// Offers the newest unseen message on each subscribed topic. A
    /// caught-up message rides along with the next turn and never starts one.
    pub async fn catch_up(&self) -> Result<(), String> {
        let topics = {
            let state = lock(&self.0.state);
            state.ensure_open()?;
            state.topics.clone()
        };
        if topics.is_empty() {
            return Ok(());
        }
        let unseen = self
            .0
            .host
            .history
            .unseen(self.session_id().to_string(), topics, MAX_CATCH_UP)
            .await?;
        for stored in unseen.into_iter().rev() {
            if let Some(delivery) = Delivery::from_history(stored, &self.0.route) {
                self.0.admit_passive(delivery)?;
            }
        }
        Ok(())
    }

    /// Every stored topic, most recently active first.
    pub async fn topic_directory(&self) -> Result<Vec<TopicActivity>, String> {
        lock(&self.0.state).reads_every_message()?;
        Ok(self
            .0
            .host
            .history
            .directory()
            .await?
            .into_iter()
            .map(|summary| TopicActivity {
                topic: summary.topic,
                messages: usize::try_from(summary.count).unwrap_or(usize::MAX),
                last_ms: summary.last_ms,
            })
            .collect())
    }

    /// Stored messages on every topic `topic` matches, or on broadcasts
    /// without one, newest first. Only messages this session would accept
    /// automatically now keep their text.
    pub async fn read_history(
        &self,
        topic: Option<String>,
        before: Option<i64>,
        limit: usize,
    ) -> Result<PeerHistoryPage, String> {
        let topic = topic.as_deref().map(parse_pattern).transpose()?;
        let limit = limit.clamp(1, MAX_HISTORY_PAGE);
        lock(&self.0.state).reads_every_message()?;
        let mut stored = self
            .0
            .host
            .history
            .history(topic.clone(), before, limit + 1)
            .await?;
        let older = stored.len() > limit;
        stored.truncate(limit);
        let state = lock(&self.0.state);
        let every_message = state.reads_every_message()?;
        let before = stored.last().filter(|_| older).map(|oldest| oldest.seq);
        let mut withheld = 0;
        let messages = stored
            .into_iter()
            .filter_map(|stored| {
                let sender = &stored.message.sender;
                if !every_message
                    && !Sender::from_history(sender)
                        .is_some_and(|sender| state.same_cohort(&sender))
                {
                    withheld += 1;
                    return None;
                }
                Some(StoredPeerMessage {
                    seq: stored.seq,
                    topic: match stored.message.audience {
                        MessageAudience::Topic(topic) => Some(topic),
                        MessageAudience::Direct | MessageAudience::Broadcast => None,
                    },
                    sender_name: stored.message.sender.name,
                    sender_handle: stored.message.sender.handle,
                    external: stored.message.sender.external,
                    sent_ms: stored.message.created_ms,
                    text: stored.message.text,
                })
            })
            .collect();
        Ok(PeerHistoryPage {
            topic,
            messages,
            withheld,
            before,
        })
    }

    /// Every stored topic, the broadcasts, and this session's direct
    /// conversations, most recently active first. Only the local user browses
    /// these, so the inbound policy filters nothing.
    pub async fn message_channels(&self) -> Result<Vec<ChannelSummary>, String> {
        self.0
            .host
            .history
            .channels(self.session_id().to_string())
            .await
    }

    /// Messages on `channel`, newest first, each with every recipient's
    /// outcome.
    pub async fn channel_messages(
        &self,
        channel: MessageChannel,
        before: Option<i64>,
        limit: usize,
    ) -> Result<ChannelPage, String> {
        let session = self.session_id().to_string();
        let limit = limit.clamp(1, MAX_HISTORY_PAGE);
        let read = match &channel {
            MessageChannel::Topic(topic) => HistoryChannel::Topics(vec![parse_topic(topic)?]),
            MessageChannel::Broadcast => HistoryChannel::Broadcast,
            MessageChannel::Direct(peer) => HistoryChannel::Direct {
                session: session.clone(),
                peer: peer.clone(),
            },
        };
        let (mut stored, deliveries) = self
            .0
            .host
            .history
            .channel_page(read, before, limit + 1)
            .await?;
        let older = stored.len() > limit;
        stored.truncate(limit);
        let before = stored.last().filter(|_| older).map(|oldest| oldest.seq);
        let mut recipients: HashMap<i64, Vec<RecipientStatus>> = HashMap::new();
        for delivery in deliveries {
            recipients
                .entry(delivery.seq)
                .or_default()
                .push(RecipientStatus {
                    own: delivery.recipient_session == session,
                    name: delivery.recipient_name,
                    handle: delivery.recipient_handle,
                    status: delivery.status,
                    reason: delivery.reason,
                });
        }
        let messages = stored
            .into_iter()
            .map(|stored| {
                let message = stored.message;
                ChannelMessage {
                    seq: stored.seq,
                    audience: peer_audience(message.audience),
                    own: message.sender.session == session,
                    sender_name: message.sender.name,
                    sender_handle: message.sender.handle,
                    external: message.sender.external,
                    sent_ms: message.created_ms,
                    text: message.text,
                    recipients: recipients.remove(&stored.seq).unwrap_or_default(),
                }
            })
            .collect();
        Ok(ChannelPage {
            channel,
            messages,
            before,
        })
    }

    /// Changes whenever the shared history does, so a browser reloads only
    /// then.
    pub async fn history_version(&self) -> Result<HistoryVersion, String> {
        self.0.host.history.version().await
    }

    async fn deliver(&self, delivery: Delivery, epoch: u64) -> SendReceipt {
        #[cfg(unix)]
        return unix::send(self, delivery, epoch).await;
        #[cfg(not(unix))]
        SendReceipt::new("unavailable", &delivery.message_id, Some(UNAVAILABLE))
    }

    pub fn held_count(&self) -> usize {
        lock(&self.0.state)
            .inbox
            .iter()
            .filter(|item| matches!(item.state, ItemState::Held(_)))
            .count()
    }

    pub fn held(&self) -> Vec<HeldMessage> {
        let mut state = lock(&self.0.state);
        state.reevaluate();
        let epoch = state.epoch;
        let held: Vec<_> = state
            .inbox
            .iter()
            .filter_map(|item| match &item.state {
                ItemState::Held(reason) => Some(HeldMessage {
                    message_id: item.origin.message_id.clone(),
                    sender_name: item.delivery.sender.name.clone(),
                    text: item.delivery.text.clone(),
                    reason: reason.clone(),
                    epoch,
                }),
                _ => None,
            })
            .collect();
        state.reviews = held
            .iter()
            .map(|item| (item.message_id.clone(), epoch))
            .collect();
        held
    }

    pub fn inbox_snapshot(&self) -> Result<PeerInboxSnapshot, String> {
        let mut state = lock(&self.0.state);
        state.ensure_open()?;
        state.reevaluate();
        Ok(PeerInboxSnapshot {
            messages: state
                .inbox
                .iter()
                .filter_map(|item| item.held_summary(state.epoch, state.approval_blocker()))
                .collect(),
            inbound: state.descriptor.inbound.clone(),
            inbound_override: state.inbound_override.clone(),
            project_floor: state.floor.clone(),
        })
    }

    pub fn review_held(&self, message_id: &str) -> Result<HeldReview, String> {
        let mut state = lock(&self.0.state);
        state.ensure_open()?;
        state.reevaluate();
        let item = &state.inbox[state.held_index(message_id)?];
        Ok(HeldReview {
            summary: item
                .held_summary(state.epoch, state.approval_blocker())
                .ok_or(NOT_HELD)?,
            text: item.delivery.text.clone(),
            token: PeerReviewToken {
                registration: self.0.route.clone(),
                sender: item.delivery.sender.route.clone(),
                message_id: item.delivery.message_id.clone(),
                epoch: state.epoch,
            },
        })
    }

    pub fn decide_held(
        &self,
        token: &PeerReviewToken,
        decision: PeerDecision,
    ) -> Result<PeerDecisionResult, String> {
        let mut state = lock(&self.0.state);
        state.ensure_open()?;
        if token.registration != self.0.route || token.epoch != state.epoch {
            return Err(STALE_REVIEW.into());
        }
        state.reevaluate();
        let index = state
            .inbox
            .iter()
            .position(|item| {
                item.delivery.sender.route == token.sender
                    && item.delivery.message_id == token.message_id
                    && matches!(item.state, ItemState::Held(_))
            })
            .ok_or(NOT_HELD)?;
        if decision == PeerDecision::Approve
            && let Some(blocker) = state.approval_blocker()
        {
            return Err(blocker.into());
        }
        self.decide_locked(&mut state, index, decision)
    }

    pub fn approve(&self, message_id: &str) -> Result<(), String> {
        let mut state = lock(&self.0.state);
        state.ensure_open()?;
        if let Some(blocker) = state.approval_blocker() {
            return Err(blocker.into());
        }
        if state.reviews.get(message_id) != Some(&state.epoch) {
            return Err(STALE_REVIEW.into());
        }
        state.reevaluate();
        let index = state.held_index(message_id)?;
        self.decide_locked(&mut state, index, PeerDecision::Approve)?;
        Ok(())
    }

    pub fn reject(&self, message_id: &str) -> Result<(), String> {
        let mut state = lock(&self.0.state);
        state.ensure_open()?;
        state.reevaluate();
        let index = state.held_index(message_id)?;
        self.decide_locked(&mut state, index, PeerDecision::Reject)?;
        Ok(())
    }

    fn decide_locked(
        &self,
        state: &mut SessionState,
        index: usize,
        decision: PeerDecision,
    ) -> Result<PeerDecisionResult, String> {
        let result = match decision {
            PeerDecision::Approve => {
                let item = &mut state.inbox[index];
                item.approved_epoch = Some(state.epoch);
                // Local approval may start a turn even for a caught-up message.
                item.passive = false;
                state.reevaluate();
                PeerDecisionResult::Queued
            }
            PeerDecision::Reject => {
                let item = state.inbox.remove(index).ok_or(NOT_HELD)?;
                state.bytes -= item.bytes;
                self.0.host.bytes.fetch_sub(item.bytes, Ordering::AcqRel);
                if let Some(entry) = state.dedup.get_mut(&item.delivery.dedup_key()) {
                    entry.receipt =
                        SendReceipt::new("refused", &item.delivery.message_id, Some(REJECTED));
                }
                state.reviews.remove(&item.origin.message_id);
                state.report_rejected(&item);
                PeerDecisionResult::Rejected
            }
        };
        self.0.host.changed.notify(usize::MAX);
        Ok(result)
    }

    /// Genuine local input is the only way to clear a cancellation latch.
    pub fn resume_wakes(&self) {
        let mut state = lock(&self.0.state);
        if state.open && state.wakes_suppressed {
            state.wakes_suppressed = false;
            state.epoch += 1;
            state.reevaluate();
            self.0.host.changed.notify(usize::MAX);
        }
    }

    /// Claims queued messages for a turn that happens anyway, so caught-up
    /// messages ride along.
    pub fn claim(&self) -> Option<PeerClaim> {
        let mut state = lock(&self.0.state);
        self.claim_locked(&mut state, false)
    }

    /// Claims queued messages only when one may start a turn of its own.
    pub fn claim_wake(&self) -> Option<PeerClaim> {
        let mut state = lock(&self.0.state);
        self.claim_locked(&mut state, true)
    }

    pub fn claim_or_close(&self) -> Option<PeerClaim> {
        let mut state = lock(&self.0.state);
        let claim = self.claim_locked(&mut state, true);
        if claim.is_none() {
            self.0.close_locked(&mut state);
        }
        claim
    }

    fn claim_locked(&self, state: &mut SessionState, wake: bool) -> Option<PeerClaim> {
        if !state.open {
            return None;
        }
        state.reevaluate();
        if wake && !state.has_waiting() {
            return None;
        }
        state.next_claim += 1;
        let claim_id = state.next_claim;
        // Work starts turns of its own; it never joins one mid-request.
        let mut messages: Vec<_> = wake
            .then(|| state.work.claim(claim_id))
            .flatten()
            .into_iter()
            .collect();
        for item in &mut state.inbox {
            if messages.len() == CLAIM_BATCH {
                break;
            }
            if matches!(item.state, ItemState::Pending) {
                item.state = ItemState::Claimed(claim_id);
                messages.push(item.observation());
            }
        }
        (!messages.is_empty()).then(|| PeerClaim {
            session: self.clone(),
            claim_id,
            messages,
            committed: false,
        })
    }

    pub fn checkpoint(&self, history: &[HistoryItem]) {
        let mut state = lock(&self.0.state);
        let mut released = 0;
        state.inbox.retain(|item| {
            if !matches!(item.state, ItemState::Staged) {
                return true;
            }
            let observation = item.observation();
            let saved = history.iter().any(|saved| matches!(
                &saved.kind,
                HistoryItemKind::User { text, origin: UserOrigin::Observation, peer_event: Some(origin), .. }
                    if Some(&**origin) == observation.peer_event.as_ref()
                        && Some(text.as_str()) == observation.first_text_content()
            ));
            if saved { released += item.bytes; }
            !saved
        });
        state.bytes -= released;
        self.0.host.bytes.fetch_sub(released, Ordering::AcqRel);
    }
}

impl SessionState {
    fn claimed_handle(&self) -> Option<String> {
        self.claim.as_ref().map(|claim| claim.handle.clone())
    }

    fn reply_name(
        &mut self,
        sender: &str,
        message_id: &str,
        reply_sender: Option<&str>,
    ) -> Result<String, String> {
        let replied = |identity: &MessageIdentity| {
            identity.message_id == message_id
                && reply_sender.is_none_or(|reply_sender| reply_sender == identity.sender)
                && identity.involves(sender)
        };
        if self.message_names.count(replied) > 1 {
            return Err(AMBIGUOUS_REPLY.into());
        }
        self.message_names
            .find(replied)
            .ok_or_else(|| UNKNOWN_REPLY.into())
    }

    /// How this session addresses `peer`: by its messaging name, or by a
    /// word target local to this registration when it has none.
    fn address(&mut self, peer: &PeerInfo) -> Result<String, String> {
        match &peer.handle {
            Some(handle) => Ok(handle_address(handle)),
            None => self.peer_name(&peer.target),
        }
    }

    fn peer_name(&mut self, route: &str) -> Result<String, String> {
        Ok(match self.peer_names.name(route, None, peer_name)? {
            Name::Bound(name) => name,
            Name::Fresh(name) => {
                self.bind_peer(name.clone(), route.to_owned());
                name
            }
        })
    }

    fn bind_peer(&mut self, name: String, route: String) {
        let inbox = &self.inbox;
        self.peer_names.bind(name, route, |name| {
            inbox.iter().any(|item| item.origin.reply_target == name)
        });
    }

    fn bind_message(&mut self, name: String, identity: MessageIdentity) {
        let inbox = &self.inbox;
        self.message_names.bind(name, identity, |name| {
            inbox.iter().any(|item| {
                item.origin.message_id == name || item.origin.reply_to.as_deref() == Some(name)
            })
        });
    }

    /// The names `delivery` goes by in this registration's conversation.
    fn naming(&mut self, delivery: &Delivery) -> Result<Naming, String> {
        let sender = delivery.sender.route.target();
        let identity = MessageIdentity {
            sender: sender.clone(),
            message_id: delivery.message_id.clone(),
            recipients: Vec::new(),
        };
        let preferred =
            valid_name(&delivery.message_id, MESSAGE_WORDS).then_some(delivery.message_id.as_str());
        let alias = (!delivery.sender.external && delivery.sender.handle.is_none())
            .then(|| self.peer_names.name(sender.as_str(), None, peer_name))
            .transpose()?;
        let name = self
            .message_names
            .name(&identity, preferred, message_name)?;
        Ok(Naming {
            sender,
            identity,
            alias,
            name,
        })
    }

    fn bind_names(&mut self, naming: Naming) {
        if let Some(Name::Fresh(alias)) = naming.alias {
            self.bind_peer(alias, naming.sender);
        }
        if let Name::Fresh(name) = naming.name {
            self.bind_message(name, naming.identity);
        }
    }

    fn ensure_open(&self) -> Result<(), String> {
        if self.open {
            Ok(())
        } else {
            Err(CLOSED.into())
        }
    }

    fn has_publish_room(&mut self, now: Instant) -> bool {
        while self
            .published
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) >= RATE_WINDOW)
        {
            self.published.pop_front();
        }
        self.published.len() < self.publish_rate
    }

    fn ensure_can_send(&self) -> Result<(), String> {
        self.ensure_open()?;
        if self.wakes_suppressed || self.descriptor.blocked || self.descriptor.mode.is_read_only() {
            return Err(SEND_BLOCKED.into());
        }
        Ok(())
    }

    fn sender(&self, route: &Route) -> Sender {
        Sender {
            route: route.clone(),
            name: self.descriptor.name.clone(),
            handle: self.claimed_handle(),
            canonical_cwd: self.canonical_cwd.clone(),
            mode: WireMode::from(&self.descriptor.mode),
            permission_mode: self.descriptor.permission_mode.clone(),
            external: false,
        }
    }

    fn held_index(&self, message_id: &str) -> Result<usize, String> {
        self.inbox
            .iter()
            .position(|item| {
                item.origin.message_id == message_id && matches!(item.state, ItemState::Held(_))
            })
            .ok_or_else(|| NOT_HELD.into())
    }

    fn approval_blocker(&self) -> Option<&'static str> {
        if self.descriptor.inbound == InboundPolicy::Refuse {
            Some(REFUSED_POLICY)
        } else if self.wakes_suppressed || self.descriptor.blocked {
            Some(HELD_BLOCKED)
        } else {
            None
        }
    }

    fn hold_reason(
        &self,
        delivery: &Delivery,
        approved_epoch: Option<u64>,
    ) -> Option<&'static str> {
        if let Some(blocker) = self.approval_blocker() {
            return Some(blocker);
        }
        if approved_epoch == Some(self.epoch) {
            return None;
        }
        policy_hold(
            &self.descriptor.inbound,
            &delivery.sender,
            self.same_cohort(&delivery.sender),
        )
    }

    /// Whether stored messages may be read without a cohort check: `accept`
    /// reads every message, `auto` only what it would admit without review.
    fn reads_every_message(&self) -> Result<bool, String> {
        self.ensure_open()?;
        match self.descriptor.inbound {
            InboundPolicy::Accept => Ok(true),
            InboundPolicy::Auto => Ok(false),
            InboundPolicy::Hold | InboundPolicy::Refuse => Err(HISTORY_HELD.into()),
        }
    }

    fn same_cohort(&self, sender: &Sender) -> bool {
        same_cohort(
            &self.descriptor.mode,
            self.canonical_cwd.as_deref(),
            &self.descriptor.permission_mode,
            sender,
        )
    }

    fn has_waiting(&self) -> bool {
        self.inbox.iter().any(InboxItem::is_waiting) || self.work.offered()
    }

    fn reevaluate(&mut self) {
        self.reevaluate_work();
        // Moving pending messages to held must not evict accepted messages. Admission
        // caps the combined queue at MAX_HELD so later policy tightening always fits.
        for index in 0..self.inbox.len() {
            let item = &self.inbox[index];
            if matches!(item.state, ItemState::Claimed(_) | ItemState::Staged) {
                continue;
            }
            let reason = self.hold_reason(&item.delivery, item.approved_epoch);
            let unchanged = match (&item.state, reason) {
                (ItemState::Pending, None) => true,
                (ItemState::Held(held), Some(reason)) => held == reason,
                _ => false,
            };
            if !unchanged {
                self.inbox[index].state =
                    reason.map_or(ItemState::Pending, |reason| ItemState::Held(reason.into()));
                self.report_state(&self.inbox[index]);
            }
        }
    }

    /// Reports whether a received message waits for a turn or for review.
    fn report_state(&self, item: &InboxItem) {
        match &item.state {
            ItemState::Held(reason) => self.report(item, STATUS_HELD, Some(reason)),
            ItemState::Pending | ItemState::Claimed(_) | ItemState::Staged => {
                self.report(item, STATUS_QUEUED, None);
            }
        }
    }

    /// Reports what became of a message this session received.
    fn report(&self, item: &InboxItem, status: &'static str, reason: Option<&str>) {
        self.history.transition(
            item.delivery.sender.route.target(),
            item.delivery.message_id.clone(),
            MessageRecipient {
                session: self.descriptor.session_id.to_string(),
                name: Some(self.descriptor.name.clone()),
                handle: self.claimed_handle(),
            },
            status,
            reason.map(str::to_owned),
        );
    }

    /// Reports a message the conversation took in.
    fn report_delivered(&self, item: &InboxItem) {
        self.report(item, STATUS_DELIVERED, None);
        self.report_seen(item);
    }

    /// Reports a message the user rejected, so catch-up never offers it again.
    fn report_rejected(&self, item: &InboxItem) {
        self.report(item, STATUS_REJECTED, Some(REJECTED));
        self.report_seen(item);
    }

    fn report_seen(&self, item: &InboxItem) {
        if item.delivery.audience.topic().is_some() {
            self.history.seen(
                self.descriptor.session_id.to_string(),
                item.delivery.sender.route.target(),
                item.delivery.message_id.clone(),
            );
        }
    }
}

impl SessionInner {
    fn close(&self) {
        let mut state = lock(&self.state);
        self.close_locked(&mut state);
    }

    fn close_locked(&self, state: &mut SessionState) {
        state.close_work();
        state.open = false;
        state.epoch += 1;
        let mut released = 0;
        for item in mem::take(&mut state.inbox) {
            match item.state {
                ItemState::Claimed(_) => state.inbox.push_back(item),
                ItemState::Staged => released += item.bytes,
                ItemState::Pending | ItemState::Held(_) => {
                    released += item.bytes;
                    state.report(&item, STATUS_DROPPED, Some(CLOSED));
                }
            }
        }
        self.host.bytes.fetch_sub(released, Ordering::AcqRel);
        state.bytes -= released;
        state.outgoing.clear();
        state.publications.clear();
        state.dedup.clear();
        state.reviews.clear();
        state.claim = None;
        self.host.changed.notify(usize::MAX);
    }

    /// Queues `delivery`, held when policy says so. `None` when it would
    /// exceed the session or process byte limit.
    fn enqueue(
        &self,
        state: &mut SessionState,
        delivery: Delivery,
        encoded: usize,
        reply_to: Option<String>,
        passive: bool,
    ) -> Result<Option<SendReceipt>, String> {
        let naming = state.naming(&delivery)?;
        let origin = naming.origin(&delivery, reply_to, None);
        let bytes = encoded
            + serde_json::to_vec(&Message::peer_observation(
                delivery.text.clone(),
                origin.clone(),
            ))
            .map_err(|error| error.to_string())?
            .len();
        if state.bytes + bytes > MAX_SESSION_BYTES
            || self
                .host
                .bytes
                .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                    current
                        .checked_add(bytes)
                        .filter(|next| *next <= MAX_PROCESS_BYTES)
                })
                .is_err()
        {
            return Ok(None);
        }
        state.bind_names(naming);
        let reason = state.hold_reason(&delivery, None);
        let item = InboxItem {
            delivery,
            origin,
            bytes,
            state: reason.map_or(ItemState::Pending, |reason| ItemState::Held(reason.into())),
            approved_epoch: None,
            passive,
        };
        let receipt = item.receipt();
        state.report_state(&item);
        state.bytes += bytes;
        state.inbox.push_back(item);
        self.host.changed.notify(usize::MAX);
        Ok(Some(receipt))
    }

    /// Queues a caught-up topic message as a passive item, unless this
    /// registration already has it or would not accept it live.
    fn admit_passive(&self, delivery: Delivery) -> Result<(), String> {
        let encoded = serde_json::to_vec(&delivery)
            .map_err(|error| error.to_string())?
            .len();
        let mut state = lock(&self.state);
        if !state.open
            || state.dedup.contains_key(&delivery.dedup_key())
            || state
                .inbox
                .iter()
                .any(|item| item.delivery.same_message(&delivery))
            || !subscribed(&delivery.audience, &state.topics, state.broadcasts)
            || state.descriptor.inbound == InboundPolicy::Refuse
            || delivery.sender.mode == WireMode::ReadOnly
            || state.inbox.len() >= MAX_PENDING.min(MAX_HELD)
        {
            return Ok(());
        }
        self.enqueue(&mut state, delivery, encoded, None, true)?;
        Ok(())
    }

    fn receive(
        &self,
        delivery: Delivery,
        now: Instant,
        now_ms: u64,
    ) -> Result<SendReceipt, String> {
        let route = Route::parse(&delivery.target)?;
        if route.host != self.route.host
            || route.generation != self.route.generation
            || route.session != self.route.session
        {
            return Ok(SendReceipt::new(
                "refused",
                &delivery.message_id,
                Some(STALE_TARGET),
            ));
        }
        if !delivery.has_valid_metadata() {
            return Err("Invalid peer message metadata or text bounds".into());
        }
        if retry_expired(delivery.issued_ms, now_ms)
            || delivery.issued_ms.saturating_sub(now_ms) > CLOCK_SKEW.as_millis() as u64
        {
            return Ok(SendReceipt::new(
                "refused",
                &delivery.message_id,
                Some("Peer message retry window expired or clock differs"),
            ));
        }
        let encoded = serde_json::to_vec(&delivery).map_err(|error| error.to_string())?;
        let fingerprint = Sha256::digest(&encoded).into();
        let text: [u8; 32] = Sha256::digest(delivery.text.as_bytes()).into();
        let dedup_key = delivery.dedup_key();
        let mut state = lock(&self.state);
        if !state.open {
            return Ok(SendReceipt::new(
                "refused",
                &delivery.message_id,
                Some(CLOSED),
            ));
        }
        if let Some(previous) = state.dedup.get(&dedup_key) {
            if previous.fingerprint != fingerprint {
                return Err("Peer message identity was reused with different content".into());
            }
            return Ok(previous.receipt.clone());
        }
        if !has_retry_room(&mut state.dedup, now_ms, |entry| entry.issued_ms) {
            return Ok(SendReceipt::new(
                "rate_limited",
                &delivery.message_id,
                Some(RETRY_FULL),
            ));
        }
        let session = delivery.sender.route.session;
        let reply_to = delivery
            .reply_to
            .as_deref()
            .map(|message_id| {
                state.reply_name(
                    &delivery.sender.route.target(),
                    message_id,
                    delivery.reply_sender.as_deref(),
                )
            })
            .transpose()?;
        while state
            .arrivals
            .front()
            .is_some_and(|arrival| now.saturating_duration_since(arrival.at) >= RATE_WINDOW)
        {
            state.arrivals.pop_front();
        }
        let caught_up = state
            .inbox
            .iter()
            .position(|item| item.delivery.same_message(&delivery));
        let receipt = if let Some(index) = caught_up {
            // Caught up before it arrived; sent live, it may now start a turn.
            let item = &mut state.inbox[index];
            item.passive = false;
            let receipt = item.receipt();
            self.host.changed.notify(usize::MAX);
            receipt
        } else if !subscribed(&delivery.audience, &state.topics, state.broadcasts) {
            SendReceipt::new("refused", &delivery.message_id, Some(NOT_SUBSCRIBED))
        } else if state.descriptor.inbound == InboundPolicy::Refuse
            || delivery.sender.mode == WireMode::ReadOnly
        {
            SendReceipt::new("refused", &delivery.message_id, Some(REFUSED_POLICY))
        } else if state
            .arrivals
            .iter()
            .any(|arrival| arrival.session == session && arrival.text == text)
        {
            SendReceipt::new("refused", &delivery.message_id, Some(DUPLICATE))
        } else if state.arrivals.len() >= state.inbound_rate
            || state
                .arrivals
                .iter()
                .filter(|arrival| arrival.session == session)
                .count()
                >= state.sender_rate
        {
            SendReceipt::new("rate_limited", &delivery.message_id, Some(RATE_EXCEEDED))
        } else if state.inbox.len() >= MAX_PENDING.min(MAX_HELD) {
            SendReceipt::new("rate_limited", &delivery.message_id, Some(FULL))
        } else {
            match self.enqueue(&mut state, delivery.clone(), encoded.len(), reply_to, false)? {
                Some(receipt) => {
                    state.arrivals.push_back(Arrival {
                        at: now,
                        session,
                        text,
                    });
                    receipt
                }
                None => SendReceipt::new("rate_limited", &delivery.message_id, Some(FULL)),
            }
        };
        state.dedup.insert(
            dedup_key,
            DedupEntry {
                fingerprint,
                issued_ms: delivery.issued_ms,
                receipt: receipt.clone(),
            },
        );
        Ok(receipt)
    }
}

impl Drop for SessionInner {
    fn drop(&mut self) {
        self.close();
    }
}

impl PeerClaim {
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn commit(self) {
        self.finish(false);
    }

    pub fn stage(self) {
        self.finish(true);
    }

    fn finish(mut self, stage: bool) {
        let mut state = lock(&self.session.0.state);
        let mut bytes = 0;
        let retain = stage && state.open;
        for mut item in mem::take(&mut state.inbox) {
            if matches!(item.state, ItemState::Claimed(id) if id == self.claim_id) {
                state.report_delivered(&item);
                if !retain {
                    bytes += item.bytes;
                    continue;
                }
                item.state = ItemState::Staged;
            }
            state.inbox.push_back(item);
        }
        state.bytes -= bytes;
        self.session.0.host.bytes.fetch_sub(bytes, Ordering::AcqRel);
        state.work.settle_claim(self.claim_id, true);
        state.reevaluate();
        self.committed = true;
        self.session.0.host.changed.notify(usize::MAX);
    }
}

impl Drop for PeerClaim {
    fn drop(&mut self) {
        if !self.committed {
            let mut state = lock(&self.session.0.state);
            if state.open {
                for item in &mut state.inbox {
                    if matches!(item.state, ItemState::Claimed(id) if id == self.claim_id) {
                        item.state = ItemState::Pending;
                    }
                }
            } else {
                let mut released = 0;
                for item in mem::take(&mut state.inbox) {
                    if matches!(item.state, ItemState::Claimed(id) if id == self.claim_id) {
                        released += item.bytes;
                        state.report(&item, STATUS_DROPPED, Some(CLOSED));
                    } else {
                        state.inbox.push_back(item);
                    }
                }
                state.bytes -= released;
                self.session
                    .0
                    .host
                    .bytes
                    .fetch_sub(released, Ordering::AcqRel);
            }
            state.work.settle_claim(self.claim_id, false);
            state.reevaluate();
            self.session.0.host.changed.notify(usize::MAX);
        }
    }
}

impl HostInner {
    /// `None` when another live session holds the name.
    fn claim(&self, session: CaudraId, handle: &str) -> Result<Option<HandleClaim>, String> {
        let handle = parse_handle(handle)?;
        if let Some(claim) = lock(&self.reserved).remove(&session)
            && claim.handle == handle
        {
            return Ok(Some(claim));
        }
        #[cfg(unix)]
        {
            Ok(self
                .endpoint
                .directory()
                .lock_handle(&handle)?
                .map(|lock| HandleClaim {
                    handle,
                    _lock: lock,
                }))
        }
        #[cfg(not(unix))]
        Err(UNAVAILABLE.into())
    }

    async fn discover(&self) -> Result<Vec<PeerInfo>, String> {
        #[cfg(unix)]
        {
            unix::discover(self.endpoint.directory()).await
        }
        #[cfg(not(unix))]
        Err(UNAVAILABLE.into())
    }

    fn handle(&self, request: Request) -> Response {
        match request {
            Request::List { version, host }
                if version == PROTOCOL_VERSION && host == self.incarnation =>
            {
                let sessions: Vec<_> = lock(&self.sessions)
                    .values()
                    .filter_map(Weak::upgrade)
                    .collect();
                let peers = sessions
                    .iter()
                    .filter_map(|session| {
                        let state = lock(&session.state);
                        state.open.then(|| PeerInfo {
                            target: session.route.target(),
                            session_id: session.route.session,
                            name: state.descriptor.name.clone(),
                            handle: state.claimed_handle(),
                            cwd: state.descriptor.cwd.clone(),
                            busy: state.descriptor.busy,
                            blocked: state.wakes_suppressed || state.descriptor.blocked,
                            inbound: state.descriptor.inbound.clone(),
                            topics: state.topics.clone(),
                            broadcasts: state.broadcasts,
                            groups: state.groups.clone(),
                        })
                    })
                    .collect();
                let response = Response::Peers {
                    version: PROTOCOL_VERSION,
                    peers,
                };
                if serde_json::to_vec(&response).is_ok_and(|frame| frame.len() <= MAX_FRAME_BYTES) {
                    response
                } else {
                    Response::Error {
                        reason:
                            "Peer discovery is partial: live session metadata exceeds frame limit"
                                .into(),
                    }
                }
            }
            Request::Send { version, delivery } if version == PROTOCOL_VERSION => {
                let route = match Route::parse(&delivery.target) {
                    Ok(route) => route,
                    Err(reason) => return Response::Error { reason },
                };
                let session = lock(&self.sessions)
                    .get(&route.session)
                    .and_then(Weak::upgrade);
                match session {
                    Some(session) => match session.receive(*delivery, Instant::now(), wall_ms()) {
                        Ok(receipt) => Response::Receipt { receipt },
                        Err(reason) => Response::Error { reason },
                    },
                    None => Response::Receipt {
                        receipt: SendReceipt::new(
                            "refused",
                            &delivery.message_id,
                            Some(STALE_TARGET),
                        ),
                    },
                }
            }
            _ => Response::Error {
                reason: "Unsupported peer protocol or stale host incarnation".into(),
            },
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::collections::HashSet;
    use std::fs::{self, Permissions};
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::slice::from_ref;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::Instant;

    use caudra_config::{
        DEFAULT_INBOUND_PER_MINUTE, DEFAULT_SENDER_PER_MINUTE, FeatureFlags, InboundPolicy,
        MessagingConfig,
    };
    use caudra_providers::{Message, PeerAudience, PeerMessageOrigin, expand_message};
    use caudra_storage::id::CaudraId;
    use caudra_storage::sessions::{PermissionMode, StoredInboundPolicy, StoredPeerControls};
    use futures_lite::future::poll_once;
    use tempfile::{Builder, TempDir, tempfile};
    use test_case::test_case;

    use super::history::MessageHistory;
    use super::{
        AMBIGUOUS_HANDLE, AMBIGUOUS_REPLY, ANSWERS_TO, CLAIM_BATCH, CLOSED, DIRECT_PUBLICATION,
        DUPLICATE, DedupEntry, Delivery, FULL, HANDLE_PREFIX, HELD_BLOCKED, HELD_COHORT,
        HELD_POLICY, HISTORY_HELD, HandleClaim, INVALID_HANDLE, Issued, MAX_BODY_BYTES, MAX_DEDUP,
        MAX_HELD, MAX_HISTORY_PAGE, MAX_MESSAGE_NAMES, MAX_NAME_ATTEMPTS, MAX_PEER_NAMES,
        MAX_PROCESS_BYTES, MAX_SESSION_BYTES, MESSAGE_WORDS, MessageChannel, MessageIdentity,
        NAME_COLLISION, NOT_HELD, NOT_RECORDED, NOT_SUBSCRIBED, NameTable, Outgoing, PEER_WORDS,
        POLICY_FLOOR, PUBLISH_RATE_EXCEEDED, PeerDecision, PeerDecisionResult, PeerDescriptor,
        PeerHost, PeerInfo, PeerSession, PeerSummary, RATE_EXCEEDED, RATE_WINDOW, REFUSED_POLICY,
        REJECTED, RETRY_FULL, RETRY_WINDOW, REUSED_REQUEST, RecipientStatus, Route, STALE_REVIEW,
        STALE_TARGET, SendReceipt, Sender, TEST_HISTORY_DIRECTORY, UNKNOWN_HANDLE, UNKNOWN_REPLY,
        UNKNOWN_TARGET, UNTIL_RESUMED, WireMode, generated_handles, handle_address, handle_in_use,
        literal, lock, message_name, token,
        topics::{INVALID_PATTERN, INVALID_TOPIC},
        valid_handle, valid_name, wall_ms,
    };
    use crate::AgentMode;

    const TEXT: &str = "/compact !not-a-command @not-an-attachment";
    const OTHER_TEXT: &str = "A separate request with its own text";
    const REPLY_TEXT: &str = "A reply to the original request";
    const QUEUED: &str = "queued";
    const HELD: &str = "held";
    const REFUSED: &str = "refused";
    const RATE_LIMITED: &str = "rate_limited";
    const UNKNOWN: &str = "unknown";
    const MESSAGE_NAME: &str = "brisk-calm-otter";
    const OTHER_MESSAGE_NAME: &str = "gentle-bright-falcon";
    const THIRD_MESSAGE_NAME: &str = "quiet-keen-wren";
    const FOURTH_MESSAGE_NAME: &str = "steady-warm-heron";
    const FREE_MESSAGE_NAME: &str = "amber-swift-lark";
    const PEER_NAME: &str = "brisk-calm-otter-gentle-bright-falcon";
    const OTHER_PEER_NAME: &str = "gentle-bright-falcon-brisk-calm-otter";
    const REQUEST_ID: &str = "named-request";
    const REPLY_REQUEST_ID: &str = "named-reply";
    const ORIGINAL_REQUEST_ID: &str = "original-request";
    const BUILD_MODE: &str = "build";
    const PLAN_MODE: &str = "plan";
    const HANDLE: &str = "ci-watcher";
    const SENDER_HANDLE: &str = "deployer";
    const MISSING_HANDLE: &str = "nobody-here";
    const MALFORMED_HANDLE: &str = "CI_Watcher";
    /// Pinned, not computed. A resumed session answers to the name derived
    /// from its id, so drift here renames every session out from under the
    /// agents and scripts that know it.
    const GENERATED_HANDLE: &str = "charmed-logical-mudfish";
    const PINNED_SESSION: [u8; 16] = [7; 16];
    const LOW_RATE: usize = 2;
    const LOW_FANOUT: usize = 1;
    const TOPIC: &str = "ci.failures";
    const TOPIC_PATTERN: &str = "ci.*";
    const OTHER_PATTERN: &str = "deploy.**";
    const MALFORMED_PATTERN: &str = "CI";
    /// Delivers more than the removed sixteen-message budget while staying
    /// under the default recipient rate.
    const UNBUDGETED_ROUNDS: usize = 8;

    pub(super) fn directory() -> TempDir {
        Builder::new()
            .prefix("cp-")
            .permissions(Permissions::from_mode(0o700))
            .tempdir_in(Path::new("/tmp").canonicalize().unwrap())
            .unwrap()
    }

    pub(super) fn host(directory: &Path) -> PeerHost {
        PeerHost::start_in(directory.to_owned(), Arc::new(AtomicUsize::new(0))).unwrap()
    }

    pub(super) fn descriptor(cwd: &Path, inbound: InboundPolicy) -> PeerDescriptor {
        PeerDescriptor {
            session_id: CaudraId::generate(),
            name: "peer".into(),
            cwd: cwd.to_owned(),
            mode: AgentMode::Build,
            permission_mode: PermissionMode::Ask,
            inbound,
            blocked: false,
            busy: false,
        }
    }

    fn messaging(floor: Option<InboundPolicy>) -> MessagingConfig {
        MessagingConfig {
            project_inbound: floor,
            ..MessagingConfig::default()
        }
    }

    fn fixture(inbound: InboundPolicy) -> (TempDir, PeerHost, PeerSession) {
        let directory = directory();
        let host = host(directory.path());
        let session = host
            .register(descriptor(directory.path(), inbound))
            .unwrap();
        (directory, host, session)
    }

    fn delivery(session: &PeerSession) -> Delivery {
        let message_id = lock(&session.0.state)
            .message_names
            .fresh(None, message_name)
            .unwrap();
        Delivery {
            message_id,
            issued_ms: wall_ms(),
            target: session.0.route.target(),
            text: TEXT.into(),
            reply_to: None,
            reply_sender: None,
            audience: PeerAudience::Direct,
            sender: Sender {
                route: Route {
                    host: token().unwrap(),
                    session: CaudraId::generate(),
                    generation: token().unwrap(),
                },
                name: "sender".into(),
                handle: None,
                canonical_cwd: session.descriptor().cwd.canonicalize().ok(),
                mode: WireMode::Build,
                permission_mode: PermissionMode::Ask,
                external: false,
            },
        }
    }

    fn record_outgoing(session: &PeerSession, target: &str, message_id: &str) -> Delivery {
        let mut delivery = delivery(session);
        delivery.message_id = message_id.into();
        delivery.target = target.into();
        delivery.sender.route = session.0.route.clone();
        let mut state = lock(&session.0.state);
        assert!(state.message_names.get(message_id).is_none());
        state.bind_message(
            message_id.into(),
            MessageIdentity {
                sender: session.0.route.target(),
                message_id: message_id.into(),
                recipients: vec![target.into()],
            },
        );
        let epoch = state.epoch;
        state.outgoing.insert(
            ORIGINAL_REQUEST_ID.into(),
            Outgoing {
                issued: Issued {
                    fingerprint: [0; 32],
                    message_id: message_id.into(),
                    issued_ms: delivery.issued_ms,
                    epoch,
                    sender: delivery.sender.clone(),
                },
                receipt: None,
            },
        );
        delivery
    }

    #[test_case(MESSAGE_NAME, OTHER_MESSAGE_NAME; "message_names")]
    #[test_case(PEER_NAME, OTHER_PEER_NAME; "peer_names")]
    fn name_allocation_skips_forced_collisions(used: &str, free: &str) {
        let mut names = NameTable::new(MAX_PEER_NAMES);
        names.bind(used.to_owned(), TEXT, |_| false);
        let mut candidates = [used, free].into_iter();
        assert_eq!(
            names
                .fresh(Some(used), || Ok(candidates.next().unwrap().to_owned()))
                .unwrap(),
            free
        );
        assert!(candidates.next().is_none());
        assert_eq!(names.get(used), Some(&TEXT));
    }

    #[test_case(false; "live_name")]
    #[test_case(true; "retired_name")]
    fn name_allocation_exhaustion_preserves_bindings(retired: bool) {
        let mut names = NameTable::new(MAX_PEER_NAMES);
        names.bind(PEER_NAME.to_owned(), TEXT, |_| false);
        if retired {
            names.capacity = names.live.len();
            names.bind(OTHER_PEER_NAME.to_owned(), TEXT, |_| false);
        }
        let mut attempts = 0;
        let error = names
            .fresh(Some(PEER_NAME), || {
                attempts += 1;
                Ok(PEER_NAME.to_owned())
            })
            .unwrap_err();
        assert_eq!(error, NAME_COLLISION);
        assert_eq!(attempts, MAX_NAME_ATTEMPTS);
        assert_eq!(names.get(PEER_NAME).is_none(), retired);
    }

    #[test_case(None, OTHER_MESSAGE_NAME; "least_recently_used")]
    #[test_case(Some(OTHER_MESSAGE_NAME), THIRD_MESSAGE_NAME; "queued_citation_pins")]
    fn full_name_tables_evict_and_retire_one_unpinned_name(pinned: Option<&str>, evicted: &str) {
        let bound = [MESSAGE_NAME, OTHER_MESSAGE_NAME, THIRD_MESSAGE_NAME];
        let mut names = NameTable::new(bound.len());
        for name in bound {
            names.bind(name.to_owned(), name, |_| false);
        }
        assert_eq!(names.get(MESSAGE_NAME), Some(&MESSAGE_NAME));
        names.bind(
            FOURTH_MESSAGE_NAME.to_owned(),
            FOURTH_MESSAGE_NAME,
            |name| Some(name) == pinned,
        );
        assert_eq!(names.live.len(), bound.len());
        assert!(names.get(evicted).is_none());
        for name in bound.into_iter().chain([FOURTH_MESSAGE_NAME]) {
            if name != evicted {
                assert_eq!(names.get(name), Some(&name));
            }
        }
        let mut candidates = [evicted, FREE_MESSAGE_NAME].into_iter();
        assert_eq!(
            names
                .fresh(Some(evicted), || Ok(candidates.next().unwrap().to_owned()))
                .unwrap(),
            FREE_MESSAGE_NAME
        );
    }

    #[test_case(""; "empty")]
    #[test_case("peer"; "title")]
    #[test_case(MESSAGE_NAME; "message_not_address")]
    #[test_case(PEER_NAME; "unknown_address")]
    #[test_case("Brisk-calm-otter-gentle-bright-falcon"; "case_sensitive")]
    #[test_case("brisk-calm-otter-gentle-bright-falcon!"; "punctuation")]
    fn invalid_named_targets_never_issue_a_delivery(target: &str) {
        let (_directory, _host, session) = fixture(InboundPolicy::Auto);
        assert_eq!(
            smol::block_on(session.send_named(target, TEXT, None, REQUEST_ID)).unwrap_err(),
            UNKNOWN_TARGET
        );
        assert_eq!(
            smol::block_on(session.send_named(&session.0.route.target(), TEXT, None, REQUEST_ID))
                .unwrap_err(),
            UNKNOWN_TARGET
        );
        let state = lock(&session.0.state);
        assert!(state.outgoing.is_empty());
        assert!(state.message_names.live.is_empty());
    }

    fn cap_name_tables(session: &PeerSession) {
        let mut state = lock(&session.0.state);
        state.peer_names.capacity = state.peer_names.live.len();
        state.message_names.capacity = state.message_names.live.len();
    }

    fn receive_and_commit(session: &PeerSession, delivery: Delivery) -> PeerMessageOrigin {
        let receipt = session
            .0
            .receive(delivery, Instant::now(), wall_ms())
            .unwrap();
        assert_eq!(receipt.status, QUEUED);
        let claim = session.claim().unwrap();
        let origin = claim.messages()[0].peer_event.clone().unwrap();
        claim.commit();
        origin
    }

    #[test]
    fn evicted_names_are_refused_and_never_rebound() {
        smol::block_on(async {
            let (_directory, _host, session) = fixture(InboundPolicy::Auto);
            let evicted = receive_and_commit(&session, delivery(&session));
            cap_name_tables(&session);
            let kept = receive_and_commit(&session, delivery(&session));
            assert_eq!(
                session
                    .send_named(&evicted.reply_target, OTHER_TEXT, None, REQUEST_ID)
                    .await
                    .unwrap_err(),
                UNKNOWN_TARGET
            );
            assert_eq!(
                session
                    .send_named(
                        &kept.reply_target,
                        OTHER_TEXT,
                        Some(&evicted.message_id),
                        REQUEST_ID
                    )
                    .await
                    .unwrap_err(),
                UNKNOWN_REPLY
            );
            let mut reused = delivery(&session);
            reused.message_id = evicted.message_id.clone();
            let renamed = receive_and_commit(&session, reused);
            assert_ne!(renamed.message_id, evicted.message_id);
            assert!(valid_name(&renamed.message_id, MESSAGE_WORDS));
            assert!(lock(&session.0.state).outgoing.is_empty());
        });
    }

    #[test]
    fn queued_messages_keep_the_names_they_cite() {
        let (_directory, _host, session) = fixture(InboundPolicy::Hold);
        let held = delivery(&session);
        for delivery in [held.clone(), delivery(&session)] {
            let receipt = session
                .0
                .receive(delivery, Instant::now(), wall_ms())
                .unwrap();
            assert_eq!(receipt.status, HELD);
        }
        let summaries = session.inbox_snapshot().unwrap().messages;
        session.reject(&summaries[1].message_id).unwrap();
        cap_name_tables(&session);
        let receipt = session
            .0
            .receive(delivery(&session), Instant::now(), wall_ms())
            .unwrap();
        assert_eq!(receipt.status, HELD);
        {
            let mut state = lock(&session.0.state);
            assert_eq!(
                state.peer_names.get(&summaries[0].reply_target),
                Some(&held.sender.route.target())
            );
            assert!(state.message_names.get(&summaries[0].message_id).is_some());
            assert!(state.peer_names.get(&summaries[1].reply_target).is_none());
            assert!(state.message_names.get(&summaries[1].message_id).is_none());
        }
        let review = session.review_held(&summaries[0].message_id).unwrap();
        assert_eq!(review.summary.reply_target, summaries[0].reply_target);
    }

    #[test_case(InboundPolicy::Refuse, WireMode::Build, false, REFUSED; "receiver_policy")]
    #[test_case(InboundPolicy::Auto, WireMode::ReadOnly, false, REFUSED; "read_only_sender")]
    #[test_case(InboundPolicy::Auto, WireMode::Build, true, RATE_LIMITED; "process_bytes_full")]
    fn refused_messages_bind_no_names(
        inbound: InboundPolicy,
        mode: WireMode,
        process_full: bool,
        status: &str,
    ) {
        let (_directory, host, session) = fixture(inbound);
        let mut delivery = delivery(&session);
        delivery.sender.mode = mode;
        if process_full {
            host.0.bytes.store(MAX_PROCESS_BYTES, Ordering::Release);
        }
        let receipt = session
            .0
            .receive(delivery, Instant::now(), wall_ms())
            .unwrap();
        assert_eq!(receipt.status, status);
        let state = lock(&session.0.state);
        assert!(state.peer_names.live.is_empty());
        assert!(state.message_names.live.is_empty());
    }

    #[test_case(false, None; "after_retry_record_is_forgotten")]
    #[test_case(true, Some(UNKNOWN_REPLY); "after_name_is_evicted")]
    fn replies_to_own_messages_need_only_the_message_name(evict: bool, error: Option<&str>) {
        let (_directory, _host, session) = fixture(InboundPolicy::Auto);
        let mut reply = delivery(&session);
        let recipient = reply.sender.route.target();
        record_outgoing(&session, &recipient, MESSAGE_NAME);
        {
            let mut state = lock(&session.0.state);
            state.outgoing.clear();
            if evict {
                state.message_names.capacity = state.message_names.live.len();
                let identity = MessageIdentity {
                    sender: recipient,
                    message_id: OTHER_MESSAGE_NAME.into(),
                    recipients: Vec::new(),
                };
                state.bind_message(OTHER_MESSAGE_NAME.into(), identity);
            }
        }
        reply.reply_to = Some(MESSAGE_NAME.into());
        let result = session.0.receive(reply, Instant::now(), wall_ms());
        if let Some(error) = error {
            assert_eq!(result.unwrap_err(), error);
            assert!(lock(&session.0.state).peer_names.live.is_empty());
        } else {
            assert_eq!(result.unwrap().status, QUEUED);
            let claim = session.claim().unwrap();
            let origin = claim.messages()[0].peer_event.as_ref().unwrap();
            assert_eq!(origin.reply_to.as_deref(), Some(MESSAGE_NAME));
        }
    }

    #[test]
    fn legacy_incoming_identifiers_are_named_and_framing_is_stable() {
        let (_directory, host, session) = fixture(InboundPolicy::Auto);
        let mut delivery = delivery(&session);
        delivery.message_id = token().unwrap();
        delivery.reply_to = Some(token().unwrap());
        let mut original = delivery.clone();
        original.message_id = delivery.reply_to.clone().unwrap();
        original.reply_to = None;
        original.text = OTHER_TEXT.into();
        session
            .0
            .receive(original, Instant::now(), wall_ms())
            .unwrap();
        session.claim().unwrap().commit();
        session
            .0
            .receive(delivery.clone(), Instant::now(), wall_ms())
            .unwrap();
        let claim = session.claim().unwrap();
        let observation = claim.messages()[0].clone();
        let origin = observation.peer_event.as_ref().unwrap();
        assert!(valid_name(&origin.message_id, MESSAGE_WORDS));
        assert!(valid_name(&origin.reply_target, PEER_WORDS));
        assert!(valid_name(
            origin.reply_to.as_deref().unwrap(),
            MESSAGE_WORDS
        ));
        let framed = observation.first_text_content().unwrap();
        for raw in [
            delivery.message_id.clone(),
            delivery.sender.route.target(),
            delivery.sender.route.session.to_string(),
            delivery.sender.route.host.clone(),
            delivery.sender.route.generation.clone(),
            delivery.reply_to.clone().unwrap(),
        ] {
            assert!(!framed.contains(&raw));
        }
        drop(claim);
        let claim = session.claim().unwrap();
        assert_eq!(claim.messages()[0].peer_event, observation.peer_event);
        assert_eq!(claim.messages()[0].first_text_content(), Some(framed));
        claim.stage();
        session.checkpoint(&expand_message(&observation, None));
        assert_eq!(host.0.bytes.load(Ordering::Acquire), 0);
    }

    #[test_case(true; "same_host_different_generation")]
    #[test_case(false; "different_hosts")]
    fn colliding_sender_message_names_have_unambiguous_reviews(same_host: bool) {
        let (_directory, host, session) = fixture(InboundPolicy::Hold);
        let mut first = delivery(&session);
        first.message_id = MESSAGE_NAME.into();
        let mut second = first.clone();
        second.text = OTHER_TEXT.into();
        if same_host {
            second.sender.route.generation = token().unwrap();
        } else {
            second.sender.route.host = token().unwrap();
        }
        let first_receipt = session
            .0
            .receive(first.clone(), Instant::now(), wall_ms())
            .unwrap();
        let second_receipt = session
            .0
            .receive(second.clone(), Instant::now(), wall_ms())
            .unwrap();
        assert_eq!(first_receipt.status, HELD);
        assert_eq!(second_receipt.status, HELD);
        let held = session.held();
        assert_eq!(held.len(), 2);
        assert_eq!(held[0].message_id, MESSAGE_NAME);
        assert_ne!(held[0].message_id, held[1].message_id);
        assert!(valid_name(&held[1].message_id, MESSAGE_WORDS));
        session.approve(&held[1].message_id).unwrap();
        let claim = session.claim().unwrap();
        let origin = claim.messages()[0].peer_event.as_ref().unwrap();
        assert_eq!(origin.message_id, held[1].message_id);
        assert_eq!(
            lock(&session.0.state).peer_names.get(&origin.reply_target),
            Some(&second.sender.route.target())
        );
        claim.commit();
        session.reject(&held[0].message_id).unwrap();
        assert_eq!(host.0.bytes.load(Ordering::Acquire), 0);
        assert_eq!(
            session
                .0
                .receive(second, Instant::now(), wall_ms())
                .unwrap(),
            second_receipt
        );
        let rejected = session.0.receive(first, Instant::now(), wall_ms()).unwrap();
        assert_eq!(rejected.status, REFUSED);
        assert_eq!(rejected.message_id, MESSAGE_NAME);
        assert!(session.claim().is_none());
    }

    #[test]
    fn issued_and_received_message_names_share_collision_checks() {
        let (_directory, _host, session) = fixture(InboundPolicy::Auto);
        let mut delivery = delivery(&session);
        delivery.message_id = MESSAGE_NAME.into();
        record_outgoing(&session, &delivery.sender.route.target(), MESSAGE_NAME);
        session
            .0
            .receive(delivery.clone(), Instant::now(), wall_ms())
            .unwrap();
        let claim = session.claim().unwrap();
        let received = &claim.messages()[0].peer_event.as_ref().unwrap().message_id;
        assert_ne!(received, MESSAGE_NAME);
        let mut state = lock(&session.0.state);
        let free = [OTHER_MESSAGE_NAME, THIRD_MESSAGE_NAME]
            .into_iter()
            .find(|name| !state.message_names.live.contains_key(*name))
            .unwrap();
        let mut candidates = [MESSAGE_NAME, received, free].into_iter();
        let next = state
            .message_names
            .fresh(None, || Ok(candidates.next().unwrap().to_owned()))
            .unwrap();
        assert_eq!(next, free);
        assert_eq!(
            state.message_names.get(received).unwrap().message_id,
            MESSAGE_NAME
        );
    }

    #[test]
    fn disabled_start_is_inert() {
        assert!(
            PeerHost::start(FeatureFlags::NONE, &MessagingConfig::default())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn suppression_survives_frontend_updates_until_local_input() {
        let (_directory, _host, session) = fixture(InboundPolicy::Auto);
        session
            .0
            .receive(delivery(&session), Instant::now(), wall_ms())
            .unwrap();
        session.suppress_wakes();
        let mut descriptor = session.descriptor();
        descriptor.busy = false;
        descriptor.blocked = false;
        session.update(descriptor).unwrap();
        session.set_inbound(InboundPolicy::Accept).unwrap();
        assert!(session.wakes_suppressed());
        assert!(!session.has_pending());
        assert!(session.claim().is_none());
        let held = session.held();
        assert_eq!(held[0].reason, HELD_BLOCKED);
        assert_eq!(
            session.approve(&held[0].message_id).unwrap_err(),
            HELD_BLOCKED
        );
        session.resume_wakes();
        assert!(!session.wakes_suppressed());
        assert!(session.claim().is_some());
    }

    #[test_case(InboundPolicy::Auto, PermissionMode::Ask, WireMode::Build, true, QUEUED, None; "automatic_cohort")]
    #[test_case(InboundPolicy::Auto, PermissionMode::Auto, WireMode::Build, true, HELD, Some(HELD_COHORT); "auto_permissions_held")]
    #[test_case(InboundPolicy::Auto, PermissionMode::Yolo, WireMode::Build, true, HELD, Some(HELD_COHORT); "yolo_permissions_held")]
    #[test_case(InboundPolicy::Auto, PermissionMode::Ask, WireMode::Plan, true, HELD, Some(HELD_COHORT); "plan_to_build_held")]
    #[test_case(InboundPolicy::Auto, PermissionMode::Ask, WireMode::Build, false, HELD, Some(HELD_COHORT); "different_workspace_held")]
    #[test_case(InboundPolicy::Hold, PermissionMode::Ask, WireMode::Build, true, HELD, Some(HELD_POLICY); "hold_requires_review")]
    #[test_case(InboundPolicy::Refuse, PermissionMode::Ask, WireMode::Build, true, REFUSED, Some(REFUSED_POLICY); "refuse_not_admitted")]
    #[test_case(InboundPolicy::Accept, PermissionMode::Yolo, WireMode::Plan, false, QUEUED, None; "accept_overrides_cohort")]
    #[test_case(InboundPolicy::Accept, PermissionMode::Ask, WireMode::ReadOnly, true, REFUSED, Some(REFUSED_POLICY); "readonly_cannot_send")]
    fn admission_policy(
        policy: InboundPolicy,
        permission: PermissionMode,
        mode: WireMode,
        same_cwd: bool,
        expected: &str,
        reason: Option<&str>,
    ) {
        let (_directory, _host, session) = fixture(policy);
        let mut delivery = delivery(&session);
        delivery.sender.permission_mode = permission;
        delivery.sender.mode = mode;
        if !same_cwd {
            delivery.sender.canonical_cwd = None;
        }
        let receipt = session
            .0
            .receive(delivery, Instant::now(), wall_ms())
            .unwrap();
        assert_eq!(receipt.status, expected);
        assert_eq!(receipt.reason.as_deref(), reason);
    }

    #[test_case(AgentMode::Build, AgentMode::Build, true; "build_matches")]
    #[test_case(AgentMode::Plan(PathBuf::from("plan-a")), AgentMode::Plan(PathBuf::from("plan-b")), true; "plan_matches")]
    #[test_case(AgentMode::Plan(PathBuf::from("plan")), AgentMode::Build, false; "build_to_plan_held")]
    #[test_case(AgentMode::ReadOnly, AgentMode::Build, false; "readonly_receiver_held")]
    fn matching_receiver_mode(receiver: AgentMode, sender: AgentMode, automatic: bool) {
        let (_directory, _host, session) = fixture(InboundPolicy::Auto);
        let mut descriptor = session.descriptor();
        descriptor.mode = receiver;
        session.update(descriptor).unwrap();
        let mut delivery = delivery(&session);
        delivery.sender.mode = WireMode::from(&sender);
        session
            .0
            .receive(delivery, Instant::now(), wall_ms())
            .unwrap();
        assert_eq!(session.has_pending(), automatic);
    }

    #[test]
    fn claim_release_commit_and_retry_are_lossless() {
        let (_directory, host, session) = fixture(InboundPolicy::Auto);
        let delivery = delivery(&session);
        let expected = session
            .0
            .receive(delivery.clone(), Instant::now(), wall_ms())
            .unwrap();
        let bytes = host.0.bytes.load(Ordering::Acquire);
        let claim = session.claim().unwrap();
        assert_eq!(claim.messages().len(), 1);
        assert_eq!(
            claim.messages()[0].peer_event.as_ref().unwrap().message_id,
            delivery.message_id
        );
        assert_eq!(host.0.bytes.load(Ordering::Acquire), bytes);
        assert!(session.claim().is_none());
        drop(claim);
        assert!(session.has_pending());
        session.claim().unwrap().commit();
        assert!(!session.has_pending());
        assert_eq!(host.0.bytes.load(Ordering::Acquire), 0);
        assert_eq!(
            session
                .0
                .receive(delivery.clone(), Instant::now(), wall_ms())
                .unwrap(),
            expected
        );
        assert!(!session.has_pending());
        let mut changed = delivery;
        changed.text.push('!');
        assert!(
            session
                .0
                .receive(changed, Instant::now(), wall_ms())
                .is_err()
        );
    }

    #[test]
    fn policy_rechecked_at_claim_and_reviews_fenced() {
        let directory = directory();
        let host = host(directory.path());
        let session = host
            .register_with_controls(
                descriptor(directory.path(), InboundPolicy::Auto),
                &messaging(Some(InboundPolicy::Auto)),
                None,
            )
            .unwrap();
        let delivery = delivery(&session);
        session
            .0
            .receive(delivery.clone(), Instant::now(), wall_ms())
            .unwrap();
        session.set_inbound(InboundPolicy::Hold).unwrap();
        assert!(session.claim().is_none());
        session.held();
        let mut descriptor = session.descriptor();
        descriptor.mode = AgentMode::Plan(PathBuf::from("new-plan"));
        session.update(descriptor).unwrap();
        assert!(session.approve(&delivery.message_id).is_err());
        session.held();
        session.approve(&delivery.message_id).unwrap();
        assert!(session.has_pending());
        session.set_inbound(InboundPolicy::Refuse).unwrap();
        assert!(session.claim().is_none());
        assert_eq!(
            session.approve(&delivery.message_id).unwrap_err(),
            REFUSED_POLICY
        );
        assert_eq!(
            session.set_inbound(InboundPolicy::Accept).unwrap_err(),
            POLICY_FLOOR
        );
    }

    #[test]
    fn rejection_is_terminal_and_releases_storage() {
        let (_directory, host, session) = fixture(InboundPolicy::Hold);
        let delivery = delivery(&session);
        session
            .0
            .receive(delivery.clone(), Instant::now(), wall_ms())
            .unwrap();
        session.reject(&delivery.message_id).unwrap();
        assert_eq!(host.0.bytes.load(Ordering::Acquire), 0);
        assert_eq!(
            session
                .0
                .receive(delivery, Instant::now(), wall_ms())
                .unwrap()
                .status,
            REFUSED
        );
        assert!(session.held().is_empty());
    }

    #[test]
    fn blocked_receiver_never_claims_or_approves() {
        let (_directory, _host, session) = fixture(InboundPolicy::Accept);
        let mut descriptor = session.descriptor();
        descriptor.blocked = true;
        session.update(descriptor).unwrap();
        let delivery = delivery(&session);
        assert_eq!(
            session
                .0
                .receive(delivery.clone(), Instant::now(), wall_ms())
                .unwrap()
                .reason
                .as_deref(),
            Some(HELD_BLOCKED)
        );
        session.held();
        assert_eq!(
            session.approve(&delivery.message_id).unwrap_err(),
            HELD_BLOCKED
        );
        assert!(session.claim().is_none());
    }

    #[test]
    fn full_queue_does_not_evict_and_claimed_items_count() {
        let (_directory, _host, session) = fixture(InboundPolicy::Auto);
        let now = Instant::now();
        let first = delivery(&session);
        session.0.receive(first.clone(), now, wall_ms()).unwrap();
        let claim = session.claim().unwrap();
        for index in 1..MAX_HELD {
            let receipt = session
                .0
                .receive(
                    delivery(&session),
                    now + RATE_WINDOW * index as u32,
                    wall_ms(),
                )
                .unwrap();
            assert_eq!(receipt.status, QUEUED);
        }
        let receipt = session
            .0
            .receive(
                delivery(&session),
                now + RATE_WINDOW * MAX_HELD as u32,
                wall_ms(),
            )
            .unwrap();
        assert_eq!(receipt.status, RATE_LIMITED);
        assert_eq!(receipt.reason.as_deref(), Some(FULL));
        assert_eq!(lock(&session.0.state).inbox.len(), MAX_HELD);
        assert_eq!(
            claim.messages()[0].peer_event.as_ref().unwrap().message_id,
            first.message_id
        );
        drop(claim);
        session.set_inbound(InboundPolicy::Hold).unwrap();
        assert_eq!(session.held().len(), MAX_HELD);
    }

    #[test]
    fn session_and_process_byte_budgets_include_claims() {
        let (_directory, host, session) = fixture(InboundPolicy::Auto);
        let mut count = 0;
        loop {
            let mut delivery = delivery(&session);
            delivery.text = "x".repeat(MAX_BODY_BYTES);
            let receipt = session
                .0
                .receive(delivery, Instant::now(), wall_ms())
                .unwrap();
            if receipt.status == RATE_LIMITED {
                break;
            }
            count += 1;
            assert!(count < MAX_HELD);
        }
        let bytes = host.0.bytes.load(Ordering::Acquire);
        assert!(bytes <= MAX_SESSION_BYTES);
        let claim = session.claim().unwrap();
        assert_eq!(host.0.bytes.load(Ordering::Acquire), bytes);
        drop(claim);
        session.close();
        assert_eq!(host.0.bytes.load(Ordering::Acquire), 0);

        let other = host
            .register(descriptor(
                session.descriptor().cwd.as_path(),
                InboundPolicy::Auto,
            ))
            .unwrap();
        host.0.bytes.store(MAX_PROCESS_BYTES, Ordering::Release);
        assert_eq!(
            other
                .0
                .receive(delivery(&other), Instant::now(), wall_ms())
                .unwrap()
                .reason
                .as_deref(),
            Some(FULL)
        );
        host.0.bytes.store(0, Ordering::Release);
    }

    #[test]
    fn committed_deliveries_never_exhaust_automatic_delivery() {
        let (_directory, _host, session) = fixture(InboundPolicy::Auto);
        for _ in 0..UNBUDGETED_ROUNDS {
            for _ in 0..CLAIM_BATCH {
                let receipt = session
                    .0
                    .receive(delivery(&session), Instant::now(), wall_ms())
                    .unwrap();
                assert_eq!(receipt.status, QUEUED);
            }
            let claim = session.claim().unwrap();
            assert_eq!(claim.messages().len(), CLAIM_BATCH);
            claim.commit();
            assert!(session.held().is_empty());
        }
        assert!(!session.has_pending());
    }

    fn rated(inbound_per_minute: usize, sender_per_minute: usize) -> (TempDir, PeerSession) {
        let directory = directory();
        let session = host(directory.path())
            .register_with_controls(
                descriptor(directory.path(), InboundPolicy::Auto),
                &MessagingConfig {
                    inbound_per_minute,
                    sender_per_minute,
                    ..MessagingConfig::default()
                },
                None,
            )
            .unwrap();
        (directory, session)
    }

    #[test_case(LOW_RATE, DEFAULT_SENDER_PER_MINUTE, false; "recipient_rate")]
    #[test_case(DEFAULT_INBOUND_PER_MINUTE, LOW_RATE, true; "sender_rate")]
    fn configured_rates_bound_admission_until_the_window_passes(
        inbound_per_minute: usize,
        sender_per_minute: usize,
        one_sender: bool,
    ) {
        let (_directory, session) = rated(inbound_per_minute, sender_per_minute);
        let initial = delivery(&session);
        let now = Instant::now();
        let next = |index: usize| {
            let mut next = if one_sender {
                initial.clone()
            } else {
                delivery(&session)
            };
            next.message_id = token().unwrap();
            next.text = format!("{TEXT} {index}");
            next
        };
        for index in 0..LOW_RATE {
            assert_eq!(
                session
                    .0
                    .receive(next(index), now, wall_ms())
                    .unwrap()
                    .status,
                QUEUED
            );
        }
        let limited = session.0.receive(next(LOW_RATE), now, wall_ms()).unwrap();
        assert_eq!(limited.status, RATE_LIMITED);
        assert_eq!(limited.reason.as_deref(), Some(RATE_EXCEEDED));
        if one_sender {
            let other = session.0.receive(delivery(&session), now, wall_ms());
            assert_eq!(other.unwrap().status, QUEUED);
        }
        assert_eq!(
            session
                .0
                .receive(next(LOW_RATE + 1), now + RATE_WINDOW, wall_ms())
                .unwrap()
                .status,
            QUEUED
        );
    }

    #[test]
    fn expired_retries_are_refused() {
        let (_directory, _host, session) = fixture(InboundPolicy::Auto);
        let initial = delivery(&session);
        let expired_ms = initial.issued_ms + RETRY_WINDOW.as_millis() as u64 + 1;
        assert_eq!(
            session
                .0
                .receive(initial, Instant::now(), expired_ms)
                .unwrap()
                .status,
            REFUSED
        );
    }

    #[test_case(true, QUEUED; "expired_identities_are_forgotten")]
    #[test_case(false, RATE_LIMITED; "live_identities_are_kept")]
    fn full_receipt_tables_forget_only_expired_identities(expired: bool, status: &str) {
        let (_directory, _host, session) = fixture(InboundPolicy::Auto);
        let delivery = delivery(&session);
        let now_ms = delivery.issued_ms;
        let issued_ms = if expired {
            now_ms - RETRY_WINDOW.as_millis() as u64 - 1
        } else {
            now_ms
        };
        lock(&session.0.state).dedup = (0..MAX_DEDUP)
            .map(|index| {
                let key = index.to_string();
                let receipt = SendReceipt::new(QUEUED, &key, None);
                let entry = DedupEntry {
                    fingerprint: [0; 32],
                    issued_ms,
                    receipt,
                };
                (key, entry)
            })
            .collect();
        let receipt = session.0.receive(delivery, Instant::now(), now_ms).unwrap();
        assert_eq!(receipt.status, status);
        let state = lock(&session.0.state);
        if expired {
            assert_eq!(state.dedup.len(), state.inbox.len());
        } else {
            assert_eq!(receipt.reason.as_deref(), Some(RETRY_FULL));
            assert_eq!(state.dedup.len(), MAX_DEDUP);
        }
    }

    #[test]
    fn repeated_text_from_one_sender_is_refused_within_the_window() {
        let (_directory, _host, session) = fixture(InboundPolicy::Auto);
        let first = delivery(&session);
        let now = Instant::now();
        let receipt = session.0.receive(first.clone(), now, wall_ms()).unwrap();
        assert_eq!(receipt.status, QUEUED);
        assert_eq!(
            session.0.receive(first.clone(), now, wall_ms()).unwrap(),
            receipt
        );
        let mut repeated = first.clone();
        repeated.message_id = token().unwrap();
        let duplicate = session.0.receive(repeated.clone(), now, wall_ms()).unwrap();
        assert_eq!(duplicate.status, REFUSED);
        assert_eq!(duplicate.reason.as_deref(), Some(DUPLICATE));
        let mut restarted = repeated.clone();
        restarted.message_id = token().unwrap();
        restarted.sender.route.generation = token().unwrap();
        let restarted = session.0.receive(restarted, now, wall_ms()).unwrap();
        assert_eq!(restarted.reason.as_deref(), Some(DUPLICATE));
        let mut other_sender = delivery(&session);
        other_sender.text = first.text.clone();
        let other = session.0.receive(other_sender, now, wall_ms()).unwrap();
        assert_eq!(other.status, QUEUED);
        repeated.message_id = token().unwrap();
        let later = session.0.receive(repeated, now + RATE_WINDOW, wall_ms());
        assert_eq!(later.unwrap().status, QUEUED);
    }

    #[test]
    fn independent_hosts_discover_exchange_and_refuse_stale_routes() {
        smol::block_on(async {
            let directory = directory();
            let first = host(directory.path());
            let second = host(directory.path());
            let sender = first
                .register(descriptor(directory.path(), InboundPolicy::Auto))
                .unwrap();
            let receiver = second
                .register(descriptor(directory.path(), InboundPolicy::Auto))
                .unwrap();
            let listed = sender.list().await.unwrap();
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].session_id, receiver.session_id());
            let target = &listed[0].target;
            let notified = second.notified();
            let receipt = sender.send(target, TEXT, None, "first").await.unwrap();
            notified.await;
            assert_eq!(receipt.status, QUEUED);
            assert_eq!(
                sender.send(target, TEXT, None, "first").await.unwrap(),
                receipt
            );
            assert!(sender.send(target, "changed", None, "first").await.is_err());
            receiver.claim().unwrap().commit();
            let descriptor = receiver.descriptor();
            receiver.close();
            assert!(PeerSession::lookup(descriptor.session_id).is_none());
            let replacement = second.register(descriptor).unwrap();
            assert_ne!(replacement.0.route.target(), *target);
            assert_eq!(
                sender
                    .send(target, TEXT, None, "stale")
                    .await
                    .unwrap()
                    .status,
                REFUSED
            );
            assert!(replacement.claim().is_none());
            assert_eq!(receiver.list().await.unwrap_err(), CLOSED);
            assert_eq!(
                receiver
                    .send(target, TEXT, None, "closed")
                    .await
                    .unwrap_err(),
                CLOSED
            );
        });
    }

    #[test]
    fn independent_hosts_exchange_named_messages_and_reply_before_discovery() {
        smol::block_on(async {
            let directory = directory();
            let first = host(directory.path());
            let second = host(directory.path());
            let sender = first
                .register(descriptor(directory.path(), InboundPolicy::Auto))
                .unwrap();
            let receiver = second
                .register(descriptor(directory.path(), InboundPolicy::Auto))
                .unwrap();
            let listed = sender.list_named().await.unwrap();
            assert_eq!(listed.len(), 1);
            let target = &listed[0].target;
            assert!(valid_name(target, PEER_WORDS));
            let serialized = serde_json::to_string(&listed[0]).unwrap();
            assert!(!serialized.contains("session_id"));
            assert!(!serialized.contains(&receiver.session_id().to_string()));
            assert!(!serialized.contains(&receiver.0.route.target()));
            let decoded: PeerSummary = serde_json::from_str(&serialized).unwrap();
            assert_eq!(&decoded.target, target);
            assert_eq!(sender.list_named().await.unwrap()[0].target, *target);
            assert_eq!(
                sender
                    .send_named(target, TEXT, Some(MESSAGE_NAME), REQUEST_ID)
                    .await
                    .unwrap_err(),
                UNKNOWN_REPLY
            );
            let receipt = sender
                .send_named(target, TEXT, None, REQUEST_ID)
                .await
                .unwrap();
            assert_eq!(receipt.status, QUEUED);
            assert!(valid_name(&receipt.message_id, MESSAGE_WORDS));
            assert_eq!(
                sender
                    .send_named(target, TEXT, None, REQUEST_ID)
                    .await
                    .unwrap(),
                receipt
            );
            let claim = receiver.claim().unwrap();
            let origin = claim.messages()[0].peer_event.clone().unwrap();
            assert_eq!(origin.message_id, receipt.message_id);
            assert!(valid_name(&origin.reply_target, PEER_WORDS));
            claim.commit();
            let reply = receiver
                .send_named(
                    &origin.reply_target,
                    TEXT,
                    Some(&origin.message_id),
                    REPLY_REQUEST_ID,
                )
                .await
                .unwrap();
            assert_eq!(reply.status, QUEUED);
            assert_ne!(reply.message_id, receipt.message_id);
            let claim = sender.claim().unwrap();
            let received = claim.messages()[0].peer_event.as_ref().unwrap();
            assert_eq!(received.reply_target, *target);
            assert_eq!(received.message_id, reply.message_id);
            assert_eq!(
                received.reply_to.as_deref(),
                Some(receipt.message_id.as_str())
            );
            claim.commit();
            assert_eq!(
                receiver.list_named().await.unwrap()[0].target,
                origin.reply_target
            );
            let mut changed = receiver.descriptor();
            changed.name = TEXT.into();
            receiver.update(changed).unwrap();
            let listed = sender.list_named().await.unwrap();
            assert_eq!(listed[0].target, *target);
            assert_eq!(listed[0].title, TEXT);
            let descriptor = receiver.descriptor();
            receiver.close();
            let replacement = second.register(descriptor).unwrap();
            let listed = sender.list_named().await.unwrap();
            assert_ne!(listed[0].target, *target);
            let stale = sender
                .send_named(target, TEXT, None, REPLY_REQUEST_ID)
                .await
                .unwrap();
            assert_eq!(stale.status, REFUSED);
            assert_eq!(stale.reason.as_deref(), Some(STALE_TARGET));
            assert!(replacement.claim().is_none());
            assert_eq!(receiver.list_named().await.unwrap_err(), CLOSED);
            assert_eq!(
                receiver
                    .send_named(&origin.reply_target, TEXT, None, REQUEST_ID)
                    .await
                    .unwrap_err(),
                CLOSED
            );
        });
    }

    #[test]
    fn unknown_named_retry_retains_message_identity_and_sender_snapshot() {
        smol::block_on(async {
            let (_directory, host, sender) = fixture(InboundPolicy::Auto);
            let receiver = host
                .register(descriptor(&sender.descriptor().cwd, InboundPolicy::Auto))
                .unwrap();
            let target = sender.list_named().await.unwrap().remove(0).target;
            let receipt = sender
                .send_named(&target, TEXT, None, REQUEST_ID)
                .await
                .unwrap();
            assert_eq!(receipt.status, QUEUED);
            let (issued_ms, names) = {
                let mut state = lock(&sender.0.state);
                let outgoing = state.outgoing.get_mut(REQUEST_ID).unwrap();
                outgoing.receipt = Some(SendReceipt::new(UNKNOWN, &receipt.message_id, None));
                (outgoing.issued.issued_ms, state.message_names.live.len())
            };
            let mut changed = sender.descriptor();
            let previous_name = changed.name.clone();
            changed.name = TEXT.into();
            sender.update(changed).unwrap();
            assert_eq!(
                sender
                    .send_named(&target, TEXT, None, REQUEST_ID)
                    .await
                    .unwrap(),
                receipt
            );
            assert!(
                sender
                    .send_named(&target, OTHER_MESSAGE_NAME, None, REQUEST_ID)
                    .await
                    .is_err()
            );
            let claim = receiver.claim().unwrap();
            assert_eq!(claim.messages().len(), 1);
            let origin = claim.messages()[0].peer_event.as_ref().unwrap();
            assert_eq!(origin.message_id, receipt.message_id);
            assert_eq!(origin.sender_name, previous_name);
            claim.commit();
            assert!(receiver.claim().is_none());
            let state = lock(&sender.0.state);
            let outgoing = state.outgoing.get(REQUEST_ID).unwrap();
            assert_eq!(outgoing.issued.issued_ms, issued_ms);
            assert_eq!(state.message_names.live.len(), names);
        });
    }

    #[test]
    fn replies_translate_colliding_incoming_names_to_wire_identity() {
        smol::block_on(async {
            let directory = directory();
            let first = host(directory.path());
            let second = host(directory.path());
            let sender = first
                .register(descriptor(directory.path(), InboundPolicy::Auto))
                .unwrap();
            let receiver = second
                .register(descriptor(directory.path(), InboundPolicy::Auto))
                .unwrap();
            let mut collision = delivery(&receiver);
            collision.message_id = MESSAGE_NAME.into();
            receiver
                .0
                .receive(collision.clone(), Instant::now(), wall_ms())
                .unwrap();
            receiver.claim().unwrap().commit();
            let delivery = record_outgoing(&sender, &receiver.0.route.target(), MESSAGE_NAME);
            let receipt = super::unix::send(&sender, delivery, 0).await;
            assert_eq!(receipt.status, QUEUED);
            let claim = receiver.claim().unwrap();
            let origin = claim.messages()[0].peer_event.clone().unwrap();
            assert_ne!(origin.message_id, MESSAGE_NAME);
            claim.commit();
            assert_eq!(
                receiver
                    .send_named(
                        &origin.reply_target,
                        TEXT,
                        Some(&origin.message_id),
                        REPLY_REQUEST_ID,
                    )
                    .await
                    .unwrap()
                    .status,
                QUEUED
            );
            let state = lock(&sender.0.state);
            assert_eq!(
                state.inbox[0].delivery.reply_to.as_deref(),
                Some(MESSAGE_NAME)
            );
        });
    }

    #[test_case(true, None, Some(AMBIGUOUS_REPLY); "unqualified_opposite_directions_are_ambiguous")]
    #[test_case(true, Some(true), None; "qualified_sender_followup")]
    #[test_case(true, Some(false), None; "qualified_recipient_reply")]
    #[test_case(false, None, None; "legacy_ignores_other_counterpart")]
    #[test_case(false, Some(true), None; "qualified_ignores_other_counterpart")]
    #[test_case(false, Some(false), Some(UNKNOWN_REPLY); "qualified_wrong_counterpart_refused")]
    fn colliding_reply_ids_require_exact_counterpart_and_sender(
        same_counterpart: bool,
        original_sender: Option<bool>,
        error: Option<&str>,
    ) {
        let (_directory, _host, session) = fixture(InboundPolicy::Auto);
        let mut incoming = delivery(&session);
        incoming.message_id = MESSAGE_NAME.into();
        let counterpart = if same_counterpart {
            incoming.sender.route.target()
        } else {
            delivery(&session).sender.route.target()
        };
        record_outgoing(&session, &counterpart, MESSAGE_NAME);
        session
            .0
            .receive(incoming.clone(), Instant::now(), wall_ms())
            .unwrap();
        let claim = session.claim().unwrap();
        let incoming_name = claim.messages()[0]
            .peer_event
            .as_ref()
            .unwrap()
            .message_id
            .clone();
        assert_ne!(incoming_name, MESSAGE_NAME);
        claim.commit();
        let mut followup = incoming;
        followup.message_id = OTHER_MESSAGE_NAME.into();
        followup.reply_to = Some(MESSAGE_NAME.into());
        followup.text = OTHER_TEXT.into();
        followup.reply_sender = original_sender.map(|sender| {
            if sender {
                followup.sender.route.target()
            } else {
                session.0.route.target()
            }
        });
        let result = session.0.receive(followup, Instant::now(), wall_ms());
        if let Some(error) = error {
            assert_eq!(result.unwrap_err(), error);
            assert!(session.claim().is_none());
        } else {
            assert_eq!(result.unwrap().status, QUEUED);
            let claim = session.claim().unwrap();
            let expected = if original_sender == Some(false) {
                MESSAGE_NAME
            } else {
                &incoming_name
            };
            assert_eq!(
                claim.messages()[0]
                    .peer_event
                    .as_ref()
                    .unwrap()
                    .reply_to
                    .as_deref(),
                Some(expected)
            );
        }
    }

    #[test_case(false; "unknown_legacy_reply")]
    #[test_case(true; "unknown_qualified_reply")]
    fn unknown_correlations_are_not_assigned_an_identity(qualified: bool) {
        let (_directory, _host, session) = fixture(InboundPolicy::Auto);
        let mut incoming = delivery(&session);
        incoming.reply_to = Some(MESSAGE_NAME.into());
        incoming.reply_sender = qualified.then(|| incoming.sender.route.target());
        let encoded = serde_json::to_vec(&incoming).unwrap();
        let restored: Delivery = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(restored.reply_sender, incoming.reply_sender);
        assert_eq!(
            session
                .0
                .receive(restored, Instant::now(), wall_ms())
                .unwrap_err(),
            UNKNOWN_REPLY
        );
        let state = lock(&session.0.state);
        assert!(state.peer_names.live.is_empty());
        assert!(state.message_names.live.is_empty());
        assert!(state.inbox.is_empty());
    }

    #[test]
    fn named_bidirectional_collisions_preserve_reply_qualification_and_retries() {
        smol::block_on(async {
            let (directory, _first, first) = fixture(InboundPolicy::Auto);
            let second_host = host(directory.path());
            let second = second_host
                .register(descriptor(directory.path(), InboundPolicy::Auto))
                .unwrap();
            let forward = record_outgoing(&first, &second.0.route.target(), MESSAGE_NAME);
            let backward = record_outgoing(&second, &first.0.route.target(), MESSAGE_NAME);
            assert_eq!(super::unix::send(&first, forward, 0).await.status, QUEUED);
            assert_eq!(super::unix::send(&second, backward, 0).await.status, QUEUED);
            let claim = first.claim().unwrap();
            let incoming_first = claim.messages()[0].peer_event.clone().unwrap();
            claim.commit();
            let claim = second.claim().unwrap();
            let incoming_second = claim.messages()[0].peer_event.clone().unwrap();
            claim.commit();
            let followup = second
                .send_named(
                    &incoming_second.reply_target,
                    OTHER_TEXT,
                    Some(MESSAGE_NAME),
                    REQUEST_ID,
                )
                .await
                .unwrap();
            assert_eq!(followup.status, QUEUED);
            let claim = first.claim().unwrap();
            assert_eq!(
                claim.messages()[0]
                    .peer_event
                    .as_ref()
                    .unwrap()
                    .reply_to
                    .as_deref(),
                Some(incoming_first.message_id.as_str())
            );
            let wire = lock(&first.0.state).inbox[0].delivery.clone();
            assert_eq!(
                wire.reply_sender.as_deref(),
                Some(second.0.route.target().as_str())
            );
            for route in [first.0.route.target(), second.0.route.target()] {
                assert!(
                    !claim.messages()[0]
                        .first_text_content()
                        .unwrap()
                        .contains(&route)
                );
            }
            claim.commit();
            let mut changed = wire;
            changed.reply_sender = Some(first.0.route.target());
            assert!(first.0.receive(changed, Instant::now(), wall_ms()).is_err());
            {
                let mut state = lock(&second.0.state);
                state.outgoing.get_mut(REQUEST_ID).unwrap().receipt =
                    Some(SendReceipt::new(UNKNOWN, &followup.message_id, None));
            }
            assert_eq!(
                second
                    .send_named(
                        &incoming_second.reply_target,
                        OTHER_TEXT,
                        Some(MESSAGE_NAME),
                        REQUEST_ID
                    )
                    .await
                    .unwrap(),
                followup
            );
            assert!(first.claim().is_none());
            assert!(
                second
                    .send_named(
                        &incoming_second.reply_target,
                        OTHER_TEXT,
                        Some(&incoming_second.message_id),
                        REQUEST_ID
                    )
                    .await
                    .is_err()
            );
            assert_eq!(
                second
                    .send_named(
                        &incoming_second.reply_target,
                        REPLY_TEXT,
                        Some(&incoming_second.message_id),
                        REPLY_REQUEST_ID
                    )
                    .await
                    .unwrap()
                    .status,
                QUEUED
            );
            let claim = first.claim().unwrap();
            assert_eq!(
                claim.messages()[0]
                    .peer_event
                    .as_ref()
                    .unwrap()
                    .reply_to
                    .as_deref(),
                Some(MESSAGE_NAME)
            );
        });
    }

    #[test]
    fn named_correlations_reject_both_incoming_and_outgoing_wrong_peers() {
        smol::block_on(async {
            let (_directory, _host, session) = fixture(InboundPolicy::Auto);
            let mut incoming = delivery(&session);
            incoming.message_id = MESSAGE_NAME.into();
            let other = delivery(&session).sender.route.target();
            record_outgoing(&session, &other, MESSAGE_NAME);
            session
                .0
                .receive(incoming, Instant::now(), wall_ms())
                .unwrap();
            let claim = session.claim().unwrap();
            let origin = claim.messages()[0].peer_event.clone().unwrap();
            claim.commit();
            let other = lock(&session.0.state).peer_name(&other).unwrap();
            for (target, reply_to) in [
                (&origin.reply_target, MESSAGE_NAME),
                (&other, origin.message_id.as_str()),
            ] {
                assert_eq!(
                    session
                        .send_named(target, TEXT, Some(reply_to), REQUEST_ID)
                        .await
                        .unwrap_err(),
                    UNKNOWN_REPLY
                );
            }
            assert_eq!(lock(&session.0.state).outgoing.len(), 1);
        });
    }

    fn fill_retry_records(session: &PeerSession, issued_ms: u64) {
        let sender = delivery(session).sender;
        let mut state = lock(&session.0.state);
        let epoch = state.epoch;
        state.outgoing = (0..MAX_DEDUP)
            .map(|index| {
                let outgoing = Outgoing {
                    issued: Issued {
                        fingerprint: [0; 32],
                        message_id: MESSAGE_NAME.into(),
                        issued_ms,
                        epoch,
                        sender: sender.clone(),
                    },
                    receipt: None,
                };
                (index.to_string(), outgoing)
            })
            .collect();
    }

    #[test]
    fn retry_capacity_rejections_never_consume_message_names() {
        smol::block_on(async {
            let (_directory, host, sender) = fixture(InboundPolicy::Auto);
            let _receiver = host
                .register(descriptor(&sender.descriptor().cwd, InboundPolicy::Auto))
                .unwrap();
            let target = sender.list_named().await.unwrap().remove(0).target;
            fill_retry_records(&sender, wall_ms());
            for _ in 0..=MAX_MESSAGE_NAMES {
                assert_eq!(
                    sender
                        .send_named(&target, TEXT, None, REQUEST_ID)
                        .await
                        .unwrap_err(),
                    RETRY_FULL
                );
            }
            assert!(lock(&sender.0.state).message_names.live.is_empty());
        });
    }

    #[test]
    fn expired_retry_records_free_send_capacity() {
        smol::block_on(async {
            let (_directory, host, sender) = fixture(InboundPolicy::Auto);
            let _receiver = host
                .register(descriptor(&sender.descriptor().cwd, InboundPolicy::Auto))
                .unwrap();
            let target = sender.list_named().await.unwrap().remove(0).target;
            fill_retry_records(&sender, wall_ms() - 2 * RETRY_WINDOW.as_millis() as u64);
            let receipt = sender
                .send_named(&target, TEXT, None, REQUEST_ID)
                .await
                .unwrap();
            assert_eq!(receipt.status, QUEUED);
            let state = lock(&sender.0.state);
            assert_eq!(state.outgoing.len(), 1);
            assert!(state.outgoing.contains_key(REQUEST_ID));
        });
    }

    #[test]
    fn outbound_volume_is_bounded_per_recipient_not_per_session() {
        smol::block_on(async {
            let (_directory, host, sender) = fixture(InboundPolicy::Auto);
            let cwd = sender.descriptor().cwd;
            let first = host
                .register(descriptor(&cwd, InboundPolicy::Auto))
                .unwrap();
            let second = host
                .register(descriptor(&cwd, InboundPolicy::Auto))
                .unwrap();
            let send = async |target: &PeerSession, index: usize| {
                let text = format!("{TEXT} {index}");
                let request = format!("{}-{index}", target.session_id());
                let route = target.0.route.target();
                sender.send(&route, &text, None, &request).await.unwrap()
            };
            for index in 0..DEFAULT_SENDER_PER_MINUTE {
                assert_eq!(send(&first, index).await.status, QUEUED);
            }
            let limited = send(&first, DEFAULT_SENDER_PER_MINUTE).await;
            assert_eq!(limited.status, RATE_LIMITED);
            assert_eq!(limited.reason.as_deref(), Some(RATE_EXCEEDED));
            for index in 0..DEFAULT_SENDER_PER_MINUTE {
                assert_eq!(send(&second, index).await.status, QUEUED);
            }
        });
    }

    #[test]
    fn host_cleanup_removes_only_its_own_files() {
        let directory = directory();
        let host = host(directory.path());
        let runtime_files = || {
            fs::read_dir(directory.path())
                .unwrap()
                .filter(|entry| entry.as_ref().unwrap().file_name() != TEST_HISTORY_DIRECTORY)
                .count()
        };
        assert_eq!(runtime_files(), 2);
        drop(host);
        assert_eq!(runtime_files(), 0);
        assert!(directory.path().join(TEST_HISTORY_DIRECTORY).is_dir());
    }

    #[test]
    fn closing_keeps_claim_storage_accounted_until_release() {
        let (_directory, host, session) = fixture(InboundPolicy::Auto);
        session
            .0
            .receive(delivery(&session), Instant::now(), wall_ms())
            .unwrap();
        let claim = session.claim().unwrap();
        let bytes = host.0.bytes.load(Ordering::Acquire);
        session.close();
        assert_eq!(host.0.bytes.load(Ordering::Acquire), bytes);
        assert!(PeerSession::lookup(session.session_id()).is_none());
        drop(claim);
        assert_eq!(host.0.bytes.load(Ordering::Acquire), 0);
    }

    #[test]
    fn oversized_live_roster_reports_partial_discovery() {
        smol::block_on(async {
            let (directory, host, session) = fixture(InboundPolicy::Auto);
            let mut registrations = Vec::new();
            for _ in 0..super::MAX_SESSIONS - 1 {
                let mut descriptor = descriptor(directory.path(), InboundPolicy::Auto);
                descriptor.cwd = PathBuf::from("x".repeat(super::MAX_PATH_BYTES));
                registrations.push(host.register(descriptor).unwrap());
            }
            assert_eq!(session.list().await.unwrap_err(), super::unix::PARTIAL);
        });
    }

    #[test_case(None, true; "global_auto_is_overridable")]
    #[test_case(Some(InboundPolicy::Auto), false; "project_auto_is_a_floor")]
    #[test_case(Some(InboundPolicy::Hold), false; "project_hold_is_a_floor")]
    #[test_case(Some(InboundPolicy::Refuse), false; "project_refuse_is_a_floor")]
    fn explicit_inbound_override_obeys_only_project_floor(
        floor: Option<InboundPolicy>,
        allowed: bool,
    ) {
        let directory = directory();
        let host = host(directory.path());
        let session = host
            .register_with_controls(
                descriptor(directory.path(), InboundPolicy::Auto),
                &messaging(floor),
                None,
            )
            .unwrap();
        assert_eq!(session.controls().inbound, None);
        let result = session.set_inbound(InboundPolicy::Accept);
        if allowed {
            result.unwrap();
            assert_eq!(session.descriptor().inbound, InboundPolicy::Accept);
            assert_eq!(
                session.controls().inbound,
                Some(StoredInboundPolicy::Accept)
            );
        } else {
            assert_eq!(result.unwrap_err(), POLICY_FLOOR);
            assert_eq!(session.controls().inbound, None);
        }
    }

    #[test]
    fn descriptor_updates_do_not_create_session_policy_overrides() {
        let (_directory, _host, session) = fixture(InboundPolicy::Auto);
        let mut descriptor = session.descriptor();
        descriptor.inbound = InboundPolicy::Hold;
        session.update(descriptor).unwrap();
        assert_eq!(session.controls().inbound, None);
        session.set_inbound(InboundPolicy::Hold).unwrap();
        assert_eq!(session.controls().inbound, Some(StoredInboundPolicy::Hold));
    }

    #[test_case(None, StoredInboundPolicy::Accept, InboundPolicy::Accept; "restored_accept_overrides_global_hold")]
    #[test_case(Some(InboundPolicy::Auto), StoredInboundPolicy::Accept, InboundPolicy::Auto; "restored_accept_clamped_to_project_auto")]
    #[test_case(Some(InboundPolicy::Refuse), StoredInboundPolicy::Hold, InboundPolicy::Refuse; "restored_hold_clamped_to_project_refuse")]
    fn restored_controls_preserve_the_clamped_override(
        floor: Option<InboundPolicy>,
        restored: StoredInboundPolicy,
        expected: InboundPolicy,
    ) {
        let directory = directory();
        let host = host(directory.path());
        let session = host
            .register_with_controls(
                descriptor(directory.path(), InboundPolicy::Hold),
                &messaging(floor),
                Some(StoredPeerControls {
                    inbound: Some(restored),
                    ..StoredPeerControls::default()
                }),
            )
            .unwrap();
        assert_eq!(session.descriptor().inbound, expected);
        assert_eq!(
            session.controls(),
            StoredPeerControls {
                inbound: Some(super::stored_policy(&expected)),
                ..StoredPeerControls::default()
            }
        );
    }

    fn named(host: &PeerHost, descriptor: PeerDescriptor, handle: &str) -> PeerSession {
        host.register_with_controls(
            descriptor,
            &MessagingConfig::default(),
            Some(StoredPeerControls {
                handle: Some(handle.into()),
                ..StoredPeerControls::default()
            }),
        )
        .unwrap()
    }

    fn advertised(peers: &[PeerInfo], session: &PeerSession) -> Option<String> {
        peers
            .iter()
            .find(|peer| peer.session_id == session.session_id())
            .and_then(|peer| peer.handle.clone())
    }

    #[test]
    fn generated_names_are_pinned_to_their_session() {
        let session = CaudraId::from_bytes(PINNED_SESSION);
        assert_eq!(
            generated_handles(session).next().as_deref(),
            Some(GENERATED_HANDLE)
        );
    }

    #[test]
    fn generated_names_are_distinct_three_word_messaging_names() {
        let names: Vec<_> = generated_handles(CaudraId::from_bytes(PINNED_SESSION)).collect();
        assert_eq!(names.len(), MAX_NAME_ATTEMPTS);
        assert!(
            names
                .iter()
                .all(|name| valid_handle(name) && valid_name(name, MESSAGE_WORDS))
        );
        assert_eq!(names.iter().collect::<HashSet<_>>().len(), names.len());
    }

    #[test]
    fn sessions_answer_to_their_generated_name_while_it_is_free() {
        smol::block_on(async {
            let directory = directory();
            let host = host(directory.path());
            let session = descriptor(directory.path(), InboundPolicy::Auto);
            let generated: Vec<_> = generated_handles(session.session_id).collect();
            let register = || {
                let registered = host.register(session.clone()).unwrap();
                registered.claim_handle().unwrap();
                registered
            };
            let first = register();
            assert_eq!(first.handle().as_ref(), Some(&generated[0]));
            assert_eq!(first.controls().handle, None);
            first.close();
            let resumed = register();
            assert_eq!(resumed.handle().as_ref(), Some(&generated[0]));
            resumed.close();
            let squatter = named(
                &host,
                descriptor(directory.path(), InboundPolicy::Auto),
                &generated[0],
            );
            squatter.claim_handle().unwrap();
            let displaced = register();
            assert_eq!(displaced.handle().as_ref(), Some(&generated[1]));
            let listed = squatter.list_named().await.unwrap();
            assert_eq!(listed[0].target, handle_address(&generated[1]));
        });
    }

    #[test_case(|session| { session.close(); Some(session) }; "closed")]
    #[test_case(|_| None; "dropped")]
    fn a_taken_stored_name_gives_way_to_the_generated_one_until_a_later_registration(
        release: fn(PeerSession) -> Option<PeerSession>,
    ) {
        smol::block_on(async {
            let directory = directory();
            let first = host(directory.path());
            let second = host(directory.path());
            let holder = named(
                &first,
                descriptor(directory.path(), InboundPolicy::Auto),
                HANDLE,
            );
            holder.claim_handle().unwrap();
            let session = descriptor(directory.path(), InboundPolicy::Auto);
            let generated = generated_handles(session.session_id).next().unwrap();
            let waiting = named(&second, session.clone(), HANDLE);
            assert_eq!(
                waiting.claim_handle().unwrap_err(),
                format!(
                    "{}; {ANSWERS_TO} {} {UNTIL_RESUMED}",
                    handle_in_use(HANDLE),
                    handle_address(&generated)
                )
            );
            assert_eq!(waiting.controls().handle.as_deref(), Some(HANDLE));
            assert_eq!(waiting.handle().as_ref(), Some(&generated));
            let peers = waiting.list().await.unwrap();
            assert_eq!(advertised(&peers, &holder).as_deref(), Some(HANDLE));
            let peers = holder.list().await.unwrap();
            assert_eq!(advertised(&peers, &waiting), Some(generated));
            let _released = release(holder);
            waiting.close();
            let resumed = named(&second, session, HANDLE);
            resumed.claim_handle().unwrap();
            let observer = first
                .register(descriptor(directory.path(), InboundPolicy::Auto))
                .unwrap();
            let peers = observer.list().await.unwrap();
            assert_eq!(advertised(&peers, &resumed).as_deref(), Some(HANDLE));
        });
    }

    #[test]
    fn named_sessions_address_and_reply_to_each_other_by_name() {
        smol::block_on(async {
            let directory = directory();
            let host = host(directory.path());
            let claimed = |handle| {
                let session = named(
                    &host,
                    descriptor(directory.path(), InboundPolicy::Auto),
                    handle,
                );
                session.claim_handle().unwrap();
                session
            };
            let sender = claimed(SENDER_HANDLE);
            let receiver = claimed(HANDLE);
            let listed = sender.list_named().await.unwrap();
            assert_eq!(listed[0].target, handle_address(HANDLE));
            assert!(lock(&sender.0.state).peer_names.live.is_empty());
            let sent = sender
                .send_named(&listed[0].target, TEXT, None, REQUEST_ID)
                .await
                .unwrap();
            assert_eq!(sent.status, QUEUED);
            let claim = receiver.claim().unwrap();
            let origin = claim.messages()[0].peer_event.clone().unwrap();
            claim.commit();
            assert_eq!(origin.reply_target, handle_address(SENDER_HANDLE));
            assert!(lock(&receiver.0.state).peer_names.live.is_empty());
            let reply = receiver
                .send_named(
                    &origin.reply_target,
                    REPLY_TEXT,
                    Some(&origin.message_id),
                    REPLY_REQUEST_ID,
                )
                .await
                .unwrap();
            assert_eq!(reply.status, QUEUED);
            let claim = sender.claim().unwrap();
            let replied = claim.messages()[0].peer_event.clone().unwrap();
            claim.commit();
            assert_eq!(replied.reply_to.as_deref(), Some(sent.message_id.as_str()));
        });
    }

    #[test]
    fn reserved_names_pass_to_their_session_and_conflicts_name_the_holder() {
        smol::block_on(async {
            let directory = directory();
            let first = host(directory.path());
            let second = host(directory.path());
            let descriptor = descriptor(directory.path(), InboundPolicy::Auto);
            first
                .reserve_handle(descriptor.session_id, HANDLE)
                .await
                .unwrap();
            assert_eq!(
                second
                    .reserve_handle(CaudraId::generate(), HANDLE)
                    .await
                    .unwrap_err(),
                handle_in_use(HANDLE)
            );
            let holder = named(&first, descriptor.clone(), HANDLE);
            holder.claim_handle().unwrap();
            assert!(lock(&first.0.reserved).is_empty());
            let conflict = second
                .reserve_handle(CaudraId::generate(), HANDLE)
                .await
                .unwrap_err();
            assert!(conflict.contains(&descriptor.name), "{conflict}");
            assert!(
                conflict.contains(descriptor.cwd.to_str().unwrap()),
                "{conflict}"
            );
        });
    }

    #[test_case(HANDLE, None; "live_name")]
    #[test_case(MISSING_HANDLE, Some(UNKNOWN_HANDLE); "unknown_name")]
    #[test_case(MALFORMED_HANDLE, Some(INVALID_HANDLE); "malformed_name")]
    #[test_case("", Some(INVALID_HANDLE); "empty_name")]
    fn messaging_names_address_live_sessions(handle: &str, refusal: Option<&str>) {
        smol::block_on(async {
            let directory = directory();
            let first = host(directory.path());
            let second = host(directory.path());
            let sender = named(
                &first,
                descriptor(directory.path(), InboundPolicy::Auto),
                SENDER_HANDLE,
            );
            sender.claim_handle().unwrap();
            let receiver = named(
                &second,
                descriptor(directory.path(), InboundPolicy::Auto),
                HANDLE,
            );
            receiver.claim_handle().unwrap();
            let sent = sender
                .send_named(&format!("{HANDLE_PREFIX}{handle}"), TEXT, None, REQUEST_ID)
                .await;
            let Some(refusal) = refusal else {
                assert_eq!(sent.unwrap().status, QUEUED);
                let claim = receiver.claim().unwrap();
                let origin = claim.messages()[0].peer_event.clone().unwrap();
                assert_eq!(origin.sender_handle.as_deref(), Some(SENDER_HANDLE));
                claim.commit();
                return;
            };
            assert_eq!(sent.unwrap_err(), refusal);
            assert!(receiver.claim().is_none());
        });
    }

    #[test]
    fn names_advertised_twice_are_ambiguous() {
        smol::block_on(async {
            let (directory, host, sender) = fixture(InboundPolicy::Auto);
            let receivers: Vec<_> = (0..2)
                .map(|_| {
                    let receiver = host
                        .register(descriptor(directory.path(), InboundPolicy::Auto))
                        .unwrap();
                    lock(&receiver.0.state).claim = Some(HandleClaim {
                        handle: HANDLE.into(),
                        _lock: tempfile().unwrap(),
                    });
                    receiver
                })
                .collect();
            assert_eq!(
                sender
                    .send_named(&format!("{HANDLE_PREFIX}{HANDLE}"), TEXT, None, REQUEST_ID)
                    .await
                    .unwrap_err(),
                AMBIGUOUS_HANDLE
            );
            assert!(receivers.iter().all(|receiver| receiver.claim().is_none()));
            let listed = sender.list_named().await.unwrap();
            assert_eq!(listed.len(), receivers.len());
            assert!(
                listed
                    .iter()
                    .all(|peer| valid_name(&peer.target, PEER_WORDS) && peer.handle.is_none())
            );
        });
    }

    #[test]
    fn held_count_does_not_grant_or_refresh_review() {
        let (_directory, _host, session) = fixture(InboundPolicy::Hold);
        let delivery = delivery(&session);
        session
            .0
            .receive(delivery.clone(), Instant::now(), wall_ms())
            .unwrap();
        assert_eq!(session.held_count(), 1);
        assert!(session.approve(&delivery.message_id).is_err());
        session.held();
        let mut descriptor = session.descriptor();
        descriptor.mode = AgentMode::Plan(PathBuf::from("another-plan"));
        session.update(descriptor).unwrap();
        assert_eq!(session.held_count(), 1);
        assert!(session.approve(&delivery.message_id).is_err());
        session.held();
        session.approve(&delivery.message_id).unwrap();
    }

    #[test_case(WireMode::Build, true, BUILD_MODE; "build_workspace")]
    #[test_case(WireMode::Plan, false, PLAN_MODE; "plan_workspace_unavailable")]
    fn inbox_snapshot_retains_metadata_without_authorizing(
        mode: WireMode,
        workspace: bool,
        expected_mode: &str,
    ) {
        let (_directory, _host, session) = fixture(InboundPolicy::Hold);
        let mut delivery = delivery(&session);
        delivery.message_id = token().unwrap();
        delivery.sender.mode = mode;
        if !workspace {
            delivery.sender.canonical_cwd = None;
        }
        session
            .0
            .receive(delivery.clone(), Instant::now(), wall_ms())
            .unwrap();
        let snapshot = session.inbox_snapshot().unwrap();
        assert_eq!(snapshot.messages.len(), 1);
        let summary = &snapshot.messages[0];
        assert!(valid_name(&summary.message_id, MESSAGE_WORDS));
        assert_ne!(summary.message_id, delivery.message_id);
        assert_eq!(summary.sender_name, delivery.sender.name);
        assert_eq!(summary.workspace, delivery.sender.canonical_cwd);
        assert_eq!(summary.mode, expected_mode);
        assert_eq!(summary.reason, HELD_POLICY);
        assert_eq!(summary.approval_blocker, None);
        assert_eq!(snapshot.inbound, InboundPolicy::Hold);
        assert_eq!(snapshot.inbound_override, None);
        assert_eq!(snapshot.project_floor, InboundPolicy::Accept);
        assert_eq!(
            lock(&session.0.state).peer_names.get(&summary.reply_target),
            Some(&delivery.sender.route.target())
        );
        assert_eq!(session.inbox_snapshot().unwrap(), snapshot);
        assert_eq!(
            session.approve(&summary.message_id).unwrap_err(),
            STALE_REVIEW
        );
        let review = session.review_held(&summary.message_id).unwrap();
        assert_eq!(review.summary, *summary);
        assert_eq!(review.text, delivery.text);
        assert_eq!(session.inbox_snapshot().unwrap(), snapshot);
        assert!(lock(&session.0.state).reviews.is_empty());
        assert!(!session.has_pending());
    }

    #[test_case(PeerDecision::Approve, true; "approve_colliding_names")]
    #[test_case(PeerDecision::Reject, true; "reject_colliding_names")]
    #[test_case(PeerDecision::Approve, false; "approve_same_sender")]
    #[test_case(PeerDecision::Reject, false; "reject_same_sender")]
    fn single_review_does_not_authorize_other_messages(decision: PeerDecision, colliding: bool) {
        let (_directory, host, session) = fixture(InboundPolicy::Hold);
        let mut first = delivery(&session);
        first.message_id = MESSAGE_NAME.into();
        let mut second = first.clone();
        second.text = OTHER_TEXT.into();
        if colliding {
            second.sender.route.generation = token().unwrap();
        } else {
            second.message_id = OTHER_MESSAGE_NAME.into();
        }
        session.0.receive(first, Instant::now(), wall_ms()).unwrap();
        let receipt = session
            .0
            .receive(second.clone(), Instant::now(), wall_ms())
            .unwrap();
        let snapshot = session.inbox_snapshot().unwrap();
        let first_summary = &snapshot.messages[0];
        let second_summary = &snapshot.messages[1];
        assert_ne!(first_summary.message_id, second_summary.message_id);
        let review = session.review_held(&second_summary.message_id).unwrap();
        assert_eq!(review.summary, *second_summary);
        assert_eq!(
            session.approve(&first_summary.message_id).unwrap_err(),
            STALE_REVIEW
        );
        let notified = host.notified();
        assert_eq!(
            session
                .decide_held(&review.token, decision.clone())
                .unwrap(),
            if decision == PeerDecision::Approve {
                PeerDecisionResult::Queued
            } else {
                PeerDecisionResult::Rejected
            }
        );
        assert!(smol::block_on(poll_once(notified)).is_some());
        assert_eq!(
            session.inbox_snapshot().unwrap().messages,
            from_ref(first_summary)
        );
        for decision in [PeerDecision::Approve, PeerDecision::Reject] {
            assert_eq!(
                session.decide_held(&review.token, decision).unwrap_err(),
                NOT_HELD
            );
        }
        let retry = session
            .0
            .receive(second.clone(), Instant::now(), wall_ms())
            .unwrap();
        if decision == PeerDecision::Approve {
            assert_eq!(retry, receipt);
            let claim = session.claim().unwrap();
            assert_eq!(claim.messages().len(), 1);
            assert_eq!(
                claim.messages()[0].peer_event.as_ref().unwrap().message_id,
                second_summary.message_id
            );
            claim.commit();
        } else {
            assert_eq!(
                retry,
                SendReceipt::new(REFUSED, &second.message_id, Some(REJECTED))
            );
            assert!(session.claim().is_none());
        }
        {
            let state = lock(&session.0.state);
            assert_eq!(state.inbox.len(), 1);
            assert_eq!(state.bytes, state.inbox[0].bytes);
            assert_eq!(host.0.bytes.load(Ordering::Acquire), state.bytes);
            assert!(state.inbox[0].approved_epoch.is_none());
        }
        let review = session.review_held(&first_summary.message_id).unwrap();
        assert_eq!(
            session
                .decide_held(&review.token, PeerDecision::Reject)
                .unwrap(),
            PeerDecisionResult::Rejected
        );
        assert_eq!(host.0.bytes.load(Ordering::Acquire), 0);
        assert_eq!(lock(&session.0.state).bytes, 0);
        assert_eq!(
            session.review_held(&first_summary.message_id).unwrap_err(),
            NOT_HELD
        );
    }

    #[test_case(|session| { let mut descriptor = session.descriptor(); descriptor.mode = AgentMode::ReadOnly; session.update(descriptor).unwrap(); }; "mode")]
    #[test_case(|session| { let mut descriptor = session.descriptor(); descriptor.permission_mode = PermissionMode::Yolo; session.update(descriptor).unwrap(); }; "permission_mode")]
    #[test_case(|session| { let mut descriptor = session.descriptor(); descriptor.cwd = descriptor.cwd.join(MESSAGE_NAME); session.update(descriptor).unwrap(); }; "workspace")]
    #[test_case(|session| { session.set_inbound(InboundPolicy::Refuse).unwrap(); session.set_inbound(InboundPolicy::Hold).unwrap(); }; "policy_round_trip")]
    #[test_case(|session| session.set_inbound(InboundPolicy::Hold).unwrap(); "explicit_override")]
    #[test_case(|session| { let mut descriptor = session.descriptor(); descriptor.blocked = true; session.update(descriptor).unwrap(); }; "blocked")]
    #[test_case(PeerSession::suppress_wakes; "cancellation")]
    fn snapshot_refresh_does_not_refresh_stale_review(change: fn(&PeerSession)) {
        let (_directory, _host, session) = fixture(InboundPolicy::Hold);
        let delivery = delivery(&session);
        session
            .0
            .receive(delivery.clone(), Instant::now(), wall_ms())
            .unwrap();
        let review = session.review_held(&delivery.message_id).unwrap();
        change(&session);
        let snapshot = session.inbox_snapshot().unwrap();
        assert!(snapshot.messages[0].epoch > review.summary.epoch);
        for decision in [PeerDecision::Approve, PeerDecision::Reject] {
            assert_eq!(
                session.decide_held(&review.token, decision).unwrap_err(),
                STALE_REVIEW
            );
        }
        assert_eq!(session.inbox_snapshot().unwrap(), snapshot);
        let current = session.review_held(&delivery.message_id).unwrap();
        assert_ne!(review.token, current.token);
        assert_eq!(
            session
                .decide_held(&current.token, PeerDecision::Reject)
                .unwrap(),
            PeerDecisionResult::Rejected
        );
    }

    #[test_case(PeerDecision::Approve; "approve")]
    #[test_case(PeerDecision::Reject; "reject")]
    fn review_token_cannot_cross_registration(decision: PeerDecision) {
        let (_directory, host, session) = fixture(InboundPolicy::Hold);
        let mut delivery = delivery(&session);
        session
            .0
            .receive(delivery.clone(), Instant::now(), wall_ms())
            .unwrap();
        let review = session.review_held(&delivery.message_id).unwrap();
        session.close();
        assert_eq!(session.inbox_snapshot().unwrap_err(), CLOSED);
        assert_eq!(
            session.review_held(&delivery.message_id).unwrap_err(),
            CLOSED
        );
        assert_eq!(
            session
                .decide_held(&review.token, decision.clone())
                .unwrap_err(),
            CLOSED
        );
        let replacement = host.register(session.descriptor()).unwrap();
        delivery.target = replacement.0.route.target();
        replacement
            .0
            .receive(delivery.clone(), Instant::now(), wall_ms())
            .unwrap();
        let current = replacement.review_held(&delivery.message_id).unwrap();
        assert_eq!(review.summary.message_id, current.summary.message_id);
        assert_eq!(review.summary.epoch, current.summary.epoch);
        assert_ne!(review.token, current.token);
        assert_eq!(
            replacement
                .decide_held(&review.token, decision.clone())
                .unwrap_err(),
            STALE_REVIEW
        );
        assert!(replacement.decide_held(&current.token, decision).is_ok());
    }

    #[test_case(|session| session.set_inbound(InboundPolicy::Refuse).unwrap(), REFUSED_POLICY; "refused")]
    #[test_case(|session| { let mut descriptor = session.descriptor(); descriptor.blocked = true; session.update(descriptor).unwrap(); }, HELD_BLOCKED; "blocked")]
    #[test_case(PeerSession::suppress_wakes, HELD_BLOCKED; "cancelled")]
    fn review_reports_hard_approval_blockers_without_resuming(
        block: fn(&PeerSession),
        reason: &str,
    ) {
        let (_directory, host, session) = fixture(InboundPolicy::Hold);
        let delivery = delivery(&session);
        session
            .0
            .receive(delivery.clone(), Instant::now(), wall_ms())
            .unwrap();
        block(&session);
        let controls = session.controls();
        let suppressed = session.wakes_suppressed();
        let snapshot = session.inbox_snapshot().unwrap();
        let review = session.review_held(&delivery.message_id).unwrap();
        assert_eq!(review.summary, snapshot.messages[0]);
        assert_eq!(review.summary.reason, reason);
        assert_eq!(review.summary.approval_blocker.as_deref(), Some(reason));
        assert_eq!(
            session
                .decide_held(&review.token, PeerDecision::Approve)
                .unwrap_err(),
            reason
        );
        assert_eq!(session.inbox_snapshot().unwrap(), snapshot);
        assert!(session.claim().is_none());
        assert_eq!(
            session
                .decide_held(&review.token, PeerDecision::Reject)
                .unwrap(),
            PeerDecisionResult::Rejected
        );
        assert_eq!(session.controls(), controls);
        assert_eq!(session.wakes_suppressed(), suppressed);
        assert_eq!(host.0.bytes.load(Ordering::Acquire), 0);
    }

    #[test_case(PeerDecision::Approve; "approve_after_resume")]
    #[test_case(PeerDecision::Reject; "reject_after_resume")]
    fn resuming_cancellation_invalidates_blocked_review(decision: PeerDecision) {
        let (_directory, _host, session) = fixture(InboundPolicy::Hold);
        let delivery = delivery(&session);
        session
            .0
            .receive(delivery.clone(), Instant::now(), wall_ms())
            .unwrap();
        session.suppress_wakes();
        let review = session.review_held(&delivery.message_id).unwrap();
        session.resume_wakes();
        assert_eq!(
            session.decide_held(&review.token, decision).unwrap_err(),
            STALE_REVIEW
        );
        assert_eq!(
            session.inbox_snapshot().unwrap().messages[0].approval_blocker,
            None
        );
    }

    #[test_case(InboundPolicy::Hold; "hold_floor")]
    #[test_case(InboundPolicy::Auto; "auto_floor")]
    fn approval_under_a_restored_hold_queues_exactly_the_reviewed_message(floor: InboundPolicy) {
        let directory = directory();
        let host = host(directory.path());
        let session = host
            .register_with_controls(
                descriptor(directory.path(), InboundPolicy::Auto),
                &messaging(Some(floor.clone())),
                Some(StoredPeerControls {
                    inbound: Some(StoredInboundPolicy::Hold),
                    ..StoredPeerControls::default()
                }),
            )
            .unwrap();
        for _ in 0..2 {
            session
                .0
                .receive(delivery(&session), Instant::now(), wall_ms())
                .unwrap();
        }
        let controls = session.controls();
        let snapshot = session.inbox_snapshot().unwrap();
        assert_eq!(snapshot.project_floor, floor);
        assert_eq!(snapshot.inbound, InboundPolicy::Hold);
        assert_eq!(snapshot.inbound_override, Some(InboundPolicy::Hold));
        assert_eq!(
            session.set_inbound(InboundPolicy::Accept).unwrap_err(),
            POLICY_FLOOR
        );
        let review = session
            .review_held(&snapshot.messages[0].message_id)
            .unwrap();
        assert_eq!(review.summary.reason, HELD_POLICY);
        assert_eq!(review.summary.approval_blocker, None);
        assert_eq!(
            session
                .decide_held(&review.token, PeerDecision::Approve)
                .unwrap(),
            PeerDecisionResult::Queued
        );
        assert_eq!(session.controls(), controls);
        assert_eq!(session.inbox_snapshot().unwrap().messages.len(), 1);
        let claim = session.claim().unwrap();
        assert_eq!(claim.messages().len(), 1);
        assert_eq!(
            claim.messages()[0].peer_event.as_ref().unwrap().message_id,
            review.summary.message_id
        );
        claim.commit();
        assert!(session.claim().is_none());
    }

    #[test_case(PeerDecision::Approve, true; "controls_before_approve")]
    #[test_case(PeerDecision::Approve, false; "approve_before_controls")]
    #[test_case(PeerDecision::Reject, true; "controls_before_reject")]
    #[test_case(PeerDecision::Reject, false; "reject_before_controls")]
    fn review_decisions_and_control_changes_are_serialized(
        decision: PeerDecision,
        controls_first: bool,
    ) {
        let (_directory, _host, session) = fixture(InboundPolicy::Hold);
        let delivery = delivery(&session);
        session
            .0
            .receive(delivery.clone(), Instant::now(), wall_ms())
            .unwrap();
        let review = session.review_held(&delivery.message_id).unwrap();
        let (proceed_tx, proceed_rx) = flume::bounded(1);
        let (changed_tx, changed_rx) = flume::bounded(1);
        let other = session.clone();
        let change = thread::spawn(move || {
            proceed_rx.recv().unwrap();
            other.set_inbound(InboundPolicy::Refuse).unwrap();
            changed_tx.send(()).unwrap();
        });
        if controls_first {
            proceed_tx.send(()).unwrap();
            changed_rx.recv().unwrap();
            assert_eq!(
                session.decide_held(&review.token, decision).unwrap_err(),
                STALE_REVIEW
            );
            assert_eq!(session.held_count(), 1);
        } else {
            let result = session
                .decide_held(&review.token, decision.clone())
                .unwrap();
            assert_eq!(
                result,
                if decision == PeerDecision::Approve {
                    PeerDecisionResult::Queued
                } else {
                    PeerDecisionResult::Rejected
                }
            );
            proceed_tx.send(()).unwrap();
            changed_rx.recv().unwrap();
            assert_eq!(
                session.held_count(),
                usize::from(decision == PeerDecision::Approve)
            );
        }
        change.join().unwrap();
        assert!(session.claim().is_none());
    }

    #[test]
    fn staging_waits_for_exact_checkpoint_without_reinjection() {
        let (_directory, host, session) = fixture(InboundPolicy::Auto);
        let delivery = delivery(&session);
        session
            .0
            .receive(delivery.clone(), Instant::now(), wall_ms())
            .unwrap();
        let claim = session.claim().unwrap();
        let saved = expand_message(&claim.messages()[0], None);
        let bytes = host.0.bytes.load(Ordering::Acquire);
        session.checkpoint(&saved);
        assert_eq!(host.0.bytes.load(Ordering::Acquire), bytes);
        claim.stage();
        assert_eq!(host.0.bytes.load(Ordering::Acquire), bytes);
        assert!(session.claim().is_none());
        assert_eq!(session.held_count(), 0);
        session.checkpoint(&[]);
        assert_eq!(host.0.bytes.load(Ordering::Acquire), bytes);
        session.set_inbound(InboundPolicy::Hold).unwrap();
        assert_eq!(session.held_count(), 0);
        assert!(session.claim().is_none());
        session.checkpoint(&saved);
        assert_eq!(host.0.bytes.load(Ordering::Acquire), 0);
        session.checkpoint(&saved);
        assert_eq!(host.0.bytes.load(Ordering::Acquire), 0);
        assert_eq!(
            session
                .0
                .receive(delivery, Instant::now(), wall_ms())
                .unwrap()
                .status,
            QUEUED
        );
        assert!(session.claim().is_none());
    }

    #[test_case(|origin| origin.message_id.push('x'); "different_message")]
    #[test_case(|origin| origin.sender_handle = Some(TEXT.into()); "different_sender")]
    #[test_case(|origin| origin.sender_name.push('x'); "different_name")]
    #[test_case(|origin| origin.reply_target.push('x'); "different_incarnation")]
    #[test_case(|origin| origin.reply_to = Some(TEXT.into()); "different_correlation")]
    fn stale_provenance_cannot_acknowledge_staged_items(mutate: fn(&mut PeerMessageOrigin)) {
        let (_directory, host, session) = fixture(InboundPolicy::Auto);
        let delivery = delivery(&session);
        session
            .0
            .receive(delivery, Instant::now(), wall_ms())
            .unwrap();
        let claim = session.claim().unwrap();
        let mut stale = claim.messages()[0].clone();
        mutate(stale.peer_event.as_mut().unwrap());
        let bytes = host.0.bytes.load(Ordering::Acquire);
        claim.stage();
        session.checkpoint(&expand_message(&stale, None));
        assert_eq!(host.0.bytes.load(Ordering::Acquire), bytes);
        assert!(session.claim().is_none());
        session.close();
        assert_eq!(host.0.bytes.load(Ordering::Acquire), 0);
    }

    #[test]
    fn mismatched_body_or_human_history_cannot_acknowledge_staged_items() {
        let (_directory, host, session) = fixture(InboundPolicy::Auto);
        let delivery = delivery(&session);
        session
            .0
            .receive(delivery, Instant::now(), wall_ms())
            .unwrap();
        let claim = session.claim().unwrap();
        let observation = claim.messages()[0].clone();
        let origin = observation.peer_event.clone().unwrap();
        let changed = Message::peer_observation(String::new(), origin.clone());
        let mut human = Message::user(observation.first_text_content().unwrap().into());
        human.peer_event = Some(origin);
        let bytes = host.0.bytes.load(Ordering::Acquire);
        claim.stage();
        session.checkpoint(&expand_message(&changed, None));
        assert_eq!(host.0.bytes.load(Ordering::Acquire), bytes);
        session.checkpoint(&expand_message(&human, None));
        assert_eq!(host.0.bytes.load(Ordering::Acquire), bytes);
    }

    #[test_case(true; "enqueue_wins_final_boundary")]
    #[test_case(false; "final_boundary_wins_enqueue")]
    fn final_claim_and_enqueue_are_serialized(enqueue_first: bool) {
        let (_directory, _host, session) = fixture(InboundPolicy::Auto);
        let delivery = delivery(&session);
        let (proceed_tx, proceed_rx) = flume::bounded(1);
        let (arrived_tx, arrived_rx) = flume::bounded(1);
        let sender = session.clone();
        let receiver = thread::spawn(move || {
            proceed_rx.recv().unwrap();
            let receipt = sender
                .0
                .receive(delivery, Instant::now(), wall_ms())
                .unwrap();
            arrived_tx.send(receipt).unwrap();
        });
        if enqueue_first {
            proceed_tx.send(()).unwrap();
            assert_eq!(arrived_rx.recv().unwrap().status, QUEUED);
            session.claim_or_close().unwrap().commit();
            assert!(session.claim_or_close().is_none());
        } else {
            assert!(session.claim_or_close().is_none());
            proceed_tx.send(()).unwrap();
            assert_eq!(arrived_rx.recv().unwrap().status, REFUSED);
        }
        receiver.join().unwrap();
        assert!(PeerSession::lookup(session.session_id()).is_none());
        assert!(session.claim().is_none());
    }

    fn topic_audience() -> PeerAudience {
        PeerAudience::Topic {
            topic: TOPIC.into(),
        }
    }

    fn patterns(values: &[&str]) -> Vec<String> {
        values.iter().copied().map(str::to_owned).collect()
    }

    fn publisher_fixture(messaging: MessagingConfig) -> (TempDir, PeerHost, PeerSession) {
        let directory = directory();
        let host = host(directory.path());
        let session = host
            .register_with_controls(
                descriptor(directory.path(), InboundPolicy::Auto),
                &messaging,
                None,
            )
            .unwrap();
        (directory, host, session)
    }

    fn subscriber(host: &PeerHost, cwd: &Path, topics: &[&str], broadcasts: bool) -> PeerSession {
        host.register_with_controls(
            descriptor(cwd, InboundPolicy::Auto),
            &MessagingConfig::default(),
            Some(StoredPeerControls {
                topics: patterns(topics),
                broadcasts,
                ..StoredPeerControls::default()
            }),
        )
        .unwrap()
    }

    #[test_case(topic_audience(), 0; "topic_subscribers")]
    #[test_case(PeerAudience::Broadcast, 1; "broadcast_opt_ins")]
    fn publications_reach_only_subscribed_sessions(audience: PeerAudience, reached: usize) {
        smol::block_on(async {
            let (directory, host, publisher) = publisher_fixture(MessagingConfig::default());
            publisher
                .set_subscriptions(patterns(&[TOPIC_PATTERN]), true)
                .unwrap();
            let receivers = [
                subscriber(&host, directory.path(), &[TOPIC_PATTERN], false),
                subscriber(&host, directory.path(), &[OTHER_PATTERN], true),
                subscriber(&host, directory.path(), &[], false),
            ];
            let receipt = publisher
                .publish(audience.clone(), TEXT, REQUEST_ID)
                .await
                .unwrap();
            assert_eq!(receipt.audience, audience);
            assert_eq!(receipt.skipped, 0);
            assert_eq!(receipt.recipients.len(), 1);
            assert_eq!(receipt.recipients[0].status, QUEUED);
            assert!(publisher.claim().is_none());
            for (index, receiver) in receivers.iter().enumerate() {
                let claim = receiver.claim();
                assert_eq!(claim.is_some(), index == reached);
                if let Some(claim) = claim {
                    let origin = claim.messages()[0].peer_event.clone().unwrap();
                    assert_eq!(origin.message_id, receipt.message_id);
                    assert_eq!(origin.audience, audience);
                    claim.commit();
                }
            }
        });
    }

    #[test_case(topic_audience(), &[TOPIC_PATTERN], false; "topic")]
    #[test_case(PeerAudience::Broadcast, &[], true; "broadcast")]
    fn recipients_recheck_their_subscriptions_on_arrival(
        audience: PeerAudience,
        topics: &[&str],
        broadcasts: bool,
    ) {
        let (_directory, _host, session) = fixture(InboundPolicy::Auto);
        session
            .set_subscriptions(patterns(&[OTHER_PATTERN]), false)
            .unwrap();
        let mut publication = delivery(&session);
        publication.audience = audience;
        let now = Instant::now();
        let mut arrive = |text: &str| {
            publication.message_id = token().unwrap();
            publication.text = text.into();
            session
                .0
                .receive(publication.clone(), now, wall_ms())
                .unwrap()
        };
        let refused = arrive(TEXT);
        assert_eq!(refused.status, REFUSED);
        assert_eq!(refused.reason.as_deref(), Some(NOT_SUBSCRIBED));
        session
            .set_subscriptions(patterns(topics), broadcasts)
            .unwrap();
        assert_eq!(arrive(TEXT).status, QUEUED);
        session.set_subscriptions(Vec::new(), false).unwrap();
        assert_eq!(arrive(OTHER_TEXT).reason.as_deref(), Some(NOT_SUBSCRIBED));
    }

    #[test]
    fn publications_beyond_the_fanout_limit_are_counted_as_skipped() {
        smol::block_on(async {
            let (directory, host, publisher) = publisher_fixture(MessagingConfig {
                max_fanout: LOW_FANOUT,
                ..MessagingConfig::default()
            });
            let receivers: Vec<_> = (0..=LOW_FANOUT)
                .map(|_| subscriber(&host, directory.path(), &[TOPIC_PATTERN], false))
                .collect();
            let receipt = publisher
                .publish(topic_audience(), TEXT, REQUEST_ID)
                .await
                .unwrap();
            assert_eq!(receipt.recipients.len(), LOW_FANOUT);
            assert_eq!(receipt.skipped, receivers.len() - LOW_FANOUT);
            let reached = receivers.iter().filter_map(PeerSession::claim).count();
            assert_eq!(reached, LOW_FANOUT);
        });
    }

    #[test]
    fn publication_rate_bounds_new_publications_but_not_retries() {
        smol::block_on(async {
            let (directory, host, publisher) = publisher_fixture(MessagingConfig {
                publish_per_minute: LOW_RATE,
                ..MessagingConfig::default()
            });
            let _receiver = subscriber(&host, directory.path(), &[TOPIC_PATTERN], false);
            let attempt =
                |index: usize| (format!("{TEXT} {index}"), format!("{REQUEST_ID}-{index}"));
            let mut receipts = Vec::new();
            for index in 0..LOW_RATE {
                let (text, request_id) = attempt(index);
                receipts.push(
                    publisher
                        .publish(topic_audience(), &text, &request_id)
                        .await
                        .unwrap(),
                );
            }
            let (text, request_id) = attempt(LOW_RATE);
            assert_eq!(
                publisher
                    .publish(topic_audience(), &text, &request_id)
                    .await
                    .unwrap_err(),
                PUBLISH_RATE_EXCEEDED
            );
            let (text, request_id) = attempt(0);
            assert_eq!(
                publisher
                    .publish(topic_audience(), &text, &request_id)
                    .await
                    .unwrap(),
                receipts[0]
            );
            let mut state = lock(&publisher.0.state);
            assert_eq!(state.publications.len(), LOW_RATE);
            assert!(!state.has_publish_room(Instant::now()));
            assert!(state.has_publish_room(Instant::now() + RATE_WINDOW));
        });
    }

    #[test]
    fn publication_retries_resend_only_unknown_outcomes() {
        smol::block_on(async {
            let (directory, host, publisher) = publisher_fixture(MessagingConfig::default());
            let resolved = subscriber(&host, directory.path(), &[TOPIC_PATTERN], false);
            let unresolved = subscriber(&host, directory.path(), &[TOPIC_PATTERN], false);
            let receipt = publisher
                .publish(topic_audience(), TEXT, REQUEST_ID)
                .await
                .unwrap();
            assert!(
                receipt
                    .recipients
                    .iter()
                    .all(|recipient| recipient.status == QUEUED)
            );
            {
                let mut state = lock(&publisher.0.state);
                let publication = state.publications.get_mut(REQUEST_ID).unwrap();
                let index = publication
                    .routes
                    .iter()
                    .position(|route| *route == unresolved.0.route.target())
                    .unwrap();
                publication.receipt.recipients[index].status = UNKNOWN.into();
            }
            resolved.close();
            assert_eq!(
                publisher
                    .publish(topic_audience(), OTHER_TEXT, REQUEST_ID)
                    .await
                    .unwrap_err(),
                REUSED_REQUEST
            );
            assert_eq!(
                publisher
                    .publish(topic_audience(), TEXT, REQUEST_ID)
                    .await
                    .unwrap(),
                receipt
            );
            assert_eq!(unresolved.claim().unwrap().messages().len(), 1);
        });
    }

    #[test]
    fn publication_recipients_reply_directly_to_the_publisher() {
        smol::block_on(async {
            let (directory, host, publisher) = publisher_fixture(MessagingConfig::default());
            let receiver = subscriber(&host, directory.path(), &[TOPIC_PATTERN], false);
            let receipt = publisher
                .publish(topic_audience(), TEXT, REQUEST_ID)
                .await
                .unwrap();
            let claim = receiver.claim().unwrap();
            let origin = claim.messages()[0].peer_event.clone().unwrap();
            claim.commit();
            let reply = receiver
                .send_named(
                    &origin.reply_target,
                    REPLY_TEXT,
                    Some(&origin.message_id),
                    REPLY_REQUEST_ID,
                )
                .await
                .unwrap();
            assert_eq!(reply.status, QUEUED);
            let claim = publisher.claim().unwrap();
            let received = claim.messages()[0].peer_event.clone().unwrap();
            assert_eq!(received.audience, PeerAudience::Direct);
            assert_eq!(
                received.reply_to.as_deref(),
                Some(receipt.message_id.as_str())
            );
            assert_eq!(received.reply_target, receipt.recipients[0].target);
            claim.commit();
        });
    }

    #[test_case(&[TOPIC_PATTERN, OTHER_PATTERN], true, None; "patterns_and_broadcasts")]
    #[test_case(&[TOPIC], false, None; "concrete_topic")]
    #[test_case(&[MALFORMED_PATTERN], true, Some(INVALID_PATTERN); "malformed_pattern")]
    fn subscriptions_are_validated_and_survive_reregistration(
        topics: &[&str],
        broadcasts: bool,
        error: Option<&str>,
    ) {
        let (_directory, host, session) = fixture(InboundPolicy::Auto);
        assert_eq!(
            session
                .set_subscriptions(patterns(topics), broadcasts)
                .err()
                .as_deref(),
            error
        );
        let controls = session.controls();
        if error.is_none() {
            assert_eq!(controls.topics, patterns(topics));
            assert_eq!(controls.broadcasts, broadcasts);
        } else {
            assert_eq!(controls, StoredPeerControls::default());
        }
        let descriptor = session.descriptor();
        session.close();
        let messaging = MessagingConfig::default();
        let restored = host
            .register_with_controls(descriptor.clone(), &messaging, Some(controls.clone()))
            .unwrap();
        assert_eq!(restored.controls(), controls);
        restored.close();
        let corrupted = StoredPeerControls {
            topics: patterns(&[MALFORMED_PATTERN]),
            ..controls
        };
        assert_eq!(
            host.register_with_controls(descriptor, &messaging, Some(corrupted))
                .err()
                .as_deref(),
            Some(INVALID_PATTERN)
        );
    }

    #[test_case(PeerAudience::Direct, DIRECT_PUBLICATION; "direct")]
    #[test_case(PeerAudience::Topic { topic: TOPIC_PATTERN.into() }, INVALID_TOPIC; "wildcard_topic")]
    fn publications_need_a_concrete_topic_or_broadcasts(audience: PeerAudience, error: &str) {
        let (_directory, _host, session) = fixture(InboundPolicy::Auto);
        assert_eq!(
            smol::block_on(session.publish(audience, TEXT, REQUEST_ID)).unwrap_err(),
            error
        );
        assert!(lock(&session.0.state).publications.is_empty());
    }

    fn planner(host: &PeerHost, cwd: &Path) -> PeerSession {
        host.register(PeerDescriptor {
            mode: AgentMode::Plan(cwd.join(PLAN_MODE)),
            ..descriptor(cwd, InboundPolicy::Auto)
        })
        .unwrap()
    }

    async fn publish_each(publisher: &PeerSession, texts: &[&str]) {
        for (index, text) in texts.iter().enumerate() {
            publisher
                .publish(topic_audience(), text, &format!("{REQUEST_ID}-{index}"))
                .await
                .unwrap();
        }
    }

    #[test]
    fn catch_up_offers_the_newest_unseen_message_per_topic_without_waking() {
        smol::block_on(async {
            let (directory, host, publisher) = publisher_fixture(MessagingConfig::default());
            publish_each(&publisher, &[TEXT, OTHER_TEXT]).await;
            publisher
                .set_subscriptions(patterns(&[TOPIC_PATTERN]), false)
                .unwrap();
            publisher.catch_up().await.unwrap();
            assert!(publisher.claim().is_none());
            let late = subscriber(&host, directory.path(), &[TOPIC_PATTERN], false);
            late.catch_up().await.unwrap();
            assert!(!late.has_pending());
            assert!(late.claim_wake().is_none());
            let claim = late.claim().unwrap();
            assert_eq!(claim.messages().len(), 1);
            let framed = claim.messages()[0].first_text_content().unwrap();
            assert!(
                framed.contains(OTHER_TEXT) && !framed.contains(TEXT),
                "{framed}"
            );
            claim.commit();
            late.catch_up().await.unwrap();
            assert!(late.claim().is_none());
        });
    }

    #[test]
    fn a_caught_up_message_sent_live_may_wake_the_session() {
        smol::block_on(async {
            let (directory, host, publisher) = publisher_fixture(MessagingConfig::default());
            publish_each(&publisher, &[TEXT]).await;
            let late = subscriber(&host, directory.path(), &[TOPIC_PATTERN], false);
            late.catch_up().await.unwrap();
            let delivery = lock(&late.0.state).inbox[0].delivery.clone();
            let receipt = late.0.receive(delivery, Instant::now(), wall_ms()).unwrap();
            assert_eq!(receipt.status, QUEUED);
            assert!(late.has_pending());
            assert_eq!(late.claim_wake().unwrap().messages().len(), 1);
        });
    }

    #[test_case(PeerDecision::Approve, true; "approval_may_wake")]
    #[test_case(PeerDecision::Reject, false; "rejection_counts_as_seen")]
    fn a_held_caught_up_message_follows_its_review(decision: PeerDecision, wakes: bool) {
        smol::block_on(async {
            let (directory, host, session) = fixture(InboundPolicy::Auto);
            session
                .set_subscriptions(patterns(&[TOPIC_PATTERN]), false)
                .unwrap();
            publish_each(&planner(&host, directory.path()), &[TEXT]).await;
            session.catch_up().await.unwrap();
            let held = session.held();
            assert_eq!(held[0].reason, HELD_COHORT);
            let review = session.review_held(&held[0].message_id).unwrap();
            session.decide_held(&review.token, decision).unwrap();
            assert_eq!(session.has_pending(), wakes);
            session.catch_up().await.unwrap();
            assert_eq!(session.held_count(), 0);
            assert_eq!(session.claim().is_some(), wakes);
        });
    }

    #[test]
    fn messages_the_history_cannot_record_are_never_sent() {
        smol::block_on(async {
            let directory = directory();
            let recorded = host(directory.path());
            let receiver = subscriber(&recorded, directory.path(), &[TOPIC_PATTERN], false);
            let unrecorded = PeerHost::bind(
                directory.path().to_owned(),
                MessageHistory::stopped(),
                Arc::new(AtomicUsize::new(0)),
            )
            .unwrap();
            let sender = unrecorded
                .register(descriptor(directory.path(), InboundPolicy::Auto))
                .unwrap();
            let sent = sender
                .send(&receiver.0.route.target(), TEXT, None, REQUEST_ID)
                .await;
            let published = sender
                .publish(topic_audience(), OTHER_TEXT, ORIGINAL_REQUEST_ID)
                .await;
            for error in [sent.unwrap_err(), published.unwrap_err()] {
                assert!(error.starts_with(NOT_RECORDED), "{error}");
            }
            assert!(receiver.claim().is_none());
        });
    }

    #[test_case(InboundPolicy::Accept, Ok((2, 0)); "accept_reads_every_message")]
    #[test_case(InboundPolicy::Auto, Ok((1, 1)); "auto_withholds_other_cohorts")]
    #[test_case(InboundPolicy::Hold, Err(HISTORY_HELD); "hold_reads_nothing")]
    #[test_case(InboundPolicy::Refuse, Err(HISTORY_HELD); "refuse_reads_nothing")]
    fn stored_messages_are_read_under_the_inbound_policy(
        inbound: InboundPolicy,
        expected: Result<(usize, usize), &str>,
    ) {
        smol::block_on(async {
            let (directory, host, publisher) = publisher_fixture(MessagingConfig::default());
            publish_each(&publisher, &[TEXT]).await;
            publish_each(&planner(&host, directory.path()), &[OTHER_TEXT]).await;
            let reader = host
                .register(descriptor(directory.path(), inbound))
                .unwrap();
            let read = reader
                .read_history(Some(TOPIC_PATTERN.into()), None, MAX_HISTORY_PAGE)
                .await
                .map(|page| (page.messages.len(), page.withheld));
            assert_eq!(read, expected.map_err(str::to_owned));
            let topics = reader.topic_directory().await;
            assert_eq!(
                topics
                    .ok()
                    .map(|topics| (topics[0].topic.clone(), topics[0].messages)),
                expected.ok().map(|_| (TOPIC.to_owned(), 2))
            );
        });
    }

    #[test]
    fn history_pages_from_the_newest_message_to_the_oldest() {
        smol::block_on(async {
            let (_directory, _host, publisher) = publisher_fixture(MessagingConfig::default());
            publish_each(&publisher, &[TEXT, OTHER_TEXT]).await;
            let read = async |before| {
                publisher
                    .read_history(Some(TOPIC.into()), before, 1)
                    .await
                    .unwrap()
            };
            let newest = read(None).await;
            assert_eq!(newest.messages[0].text, OTHER_TEXT);
            assert!(newest.before.is_some());
            let oldest = read(newest.before).await;
            assert_eq!(oldest.messages[0].text, TEXT);
            assert_eq!(oldest.before, None);
        });
    }

    #[test]
    fn the_manager_lists_topics_and_only_this_sessions_conversations() {
        smol::block_on(async {
            let (directory, host, publisher) = publisher_fixture(MessagingConfig::default());
            let watcher = subscriber(&host, directory.path(), &[TOPIC_PATTERN], false);
            let bystander = subscriber(&host, directory.path(), &[], false);
            publish_each(&publisher, &[TEXT]).await;
            for (target, text, request) in [
                (&publisher, OTHER_TEXT, REQUEST_ID),
                (&bystander, REPLY_TEXT, REPLY_REQUEST_ID),
            ] {
                watcher
                    .send(&target.0.route.target(), text, None, request)
                    .await
                    .unwrap();
            }
            let channels = publisher.message_channels().await.unwrap();
            assert_eq!(
                channels
                    .iter()
                    .map(|summary| (&summary.channel, summary.count))
                    .collect::<Vec<_>>(),
                [
                    (&MessageChannel::Direct(watcher.session_id().to_string()), 1),
                    (&MessageChannel::Topic(TOPIC.into()), 1),
                ]
            );
            assert_eq!(channels[0].name, Some(watcher.descriptor().name));
        });
    }

    #[test]
    fn channel_pages_show_who_sent_each_message_and_every_recipient_outcome() {
        smol::block_on(async {
            let (directory, host, publisher) = publisher_fixture(MessagingConfig::default());
            let watcher = subscriber(&host, directory.path(), &[TOPIC_PATTERN], false);
            publish_each(&publisher, &[TEXT, OTHER_TEXT]).await;
            watcher
                .send(&publisher.0.route.target(), REPLY_TEXT, None, REQUEST_ID)
                .await
                .unwrap();
            let topic = MessageChannel::Topic(TOPIC.into());
            let newest = publisher
                .channel_messages(topic.clone(), None, 1)
                .await
                .unwrap();
            let published = &newest.messages[0];
            assert_eq!((published.text.as_str(), published.own), (OTHER_TEXT, true));
            assert_eq!(published.audience, topic_audience());
            assert_eq!(
                published.recipients,
                [RecipientStatus {
                    name: Some(watcher.descriptor().name),
                    handle: None,
                    own: false,
                    status: QUEUED.into(),
                    reason: None,
                }]
            );
            let oldest = publisher
                .channel_messages(topic, newest.before, 1)
                .await
                .unwrap();
            assert_eq!(oldest.messages[0].text, TEXT);
            assert_eq!(oldest.before, None);
            let conversation = publisher
                .channel_messages(
                    MessageChannel::Direct(watcher.session_id().to_string()),
                    None,
                    MAX_HISTORY_PAGE,
                )
                .await
                .unwrap();
            let reply = &conversation.messages[0];
            assert_eq!((reply.text.as_str(), reply.own), (REPLY_TEXT, false));
            assert_eq!(reply.audience, PeerAudience::Direct);
            assert!(reply.recipients.iter().all(|recipient| recipient.own));
        });
    }

    #[test]
    fn the_history_version_moves_when_this_process_records_a_message() {
        smol::block_on(async {
            let (_directory, _host, publisher) = publisher_fixture(MessagingConfig::default());
            let initial = publisher.history_version().await.unwrap();
            assert_eq!(publisher.history_version().await.unwrap(), initial);
            publish_each(&publisher, &[TEXT]).await;
            assert_ne!(publisher.history_version().await.unwrap(), initial);
        });
    }

    #[test_case(false, "line\\n\\r\\t\\u{1b}\\u{85}\\u{202e}\\u{2066}\\u{200f}終" ; "metadata")]
    #[test_case(true, "line\n\\r\\t\\u{1b}\\u{85}\\u{202e}\\u{2066}\\u{200f}終" ; "body")]
    fn terminal_controls_and_bidi_are_literal_but_body_newlines_survive(
        newlines: bool,
        expected: &str,
    ) {
        assert_eq!(
            literal("line\n\r\t\u{1b}\u{85}\u{202e}\u{2066}\u{200f}終", newlines),
            expected
        );
    }
}
