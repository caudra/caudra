//! Cooperative, same-user live messaging. Socket credentials establish a UID, not
//! that a peer is an authentic Caudra process or has equivalent permissions.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use caudra_config::{Feature, FeatureFlags, InboundPolicy};
use caudra_providers::{HistoryItem, HistoryItemKind, Message, PeerMessageOrigin, UserOrigin};
use caudra_storage::id::CaudraId;
use caudra_storage::random_task_id;
use caudra_storage::sessions::{PermissionMode, StoredInboundPolicy, StoredPeerControls};
use event_listener::Event;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::AgentMode;

#[cfg(unix)]
mod unix;

const PROTOCOL_VERSION: u32 = 1;
const MAX_BODY_BYTES: usize = 32 * 1024;
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
const MAX_PEER_NAMES: usize = 4096;
const MAX_MESSAGE_NAMES: usize = MAX_DEDUP * 3;
const MAX_NAME_ATTEMPTS: usize = 32;
const MESSAGE_WORDS: usize = 3;
const PEER_WORDS: usize = MESSAGE_WORDS * 2;
const PEER_BUDGET: usize = 16;
const CLAIM_BATCH: usize = 4;
const RATE_WINDOW: Duration = Duration::from_secs(60);
const RECIPIENT_RATE: usize = 64;
const SENDER_RATE: usize = 16;
const RETRY_WINDOW: Duration = Duration::from_secs(300);
const CLOCK_SKEW: Duration = Duration::from_secs(30);
const CLOSED: &str = "The peer session is closed";
const REFUSED_POLICY: &str = "Inbound messaging is refused by receiver policy";
const HELD_POLICY: &str = "Receiver policy requires local approval";
const HELD_COHORT: &str = "Automatic delivery requires Ask permissions, matching Plan/Build mode, and the same canonical workspace";
const HELD_BLOCKED: &str = "Receiver is blocked; local input must resume it";
const HELD_BUDGET: &str = "Peer delivery budget exhausted; genuine local input must reset it";
const FULL: &str = "Live inbox capacity reached; no older message was removed";
const RETRY_FULL: &str = "Live retry identity capacity reached; start a new registration";
const POLICY_FLOOR: &str = "Cannot weaken the project's configured inbound policy";
const INVALID_TARGET: &str = "Invalid peer target; use an address returned by discovery";
const STALE_TARGET: &str = "Peer target is closed or belongs to an obsolete registration";
const UNKNOWN_TARGET: &str =
    "Unknown peer address; use a target from peer discovery or an incoming peer message";
const UNKNOWN_REPLY: &str = "Unknown peer message name for this target";
const AMBIGUOUS_REPLY: &str = "Peer reply identity is ambiguous without its original sender";
const SEND_BUDGET: &str = "Peer send budget exhausted; genuine local input must reset it";
const NAME_FULL: &str = "Live peer name capacity reached; start a new registration";
const NAME_COLLISION: &str = "Unable to allocate a unique peer name within the attempt limit";
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const UNAVAILABLE: &str = "Local peer messaging is unavailable on this platform";
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
    pub cwd: PathBuf,
    pub busy: bool,
    pub blocked: bool,
    pub inbound: InboundPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerSummary {
    pub target: String,
    pub name: String,
    pub cwd: PathBuf,
    pub busy: bool,
    pub blocked: bool,
    pub inbound: InboundPolicy,
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

#[derive(Debug, Clone, Serialize)]
pub struct HeldMessage {
    pub message_id: String,
    pub sender_name: String,
    pub text: String,
    pub reason: String,
    pub epoch: u64,
}

#[derive(Clone)]
pub struct PeerHost(Arc<HostInner>);

struct HostInner {
    incarnation: String,
    sessions: Mutex<HashMap<CaudraId, Weak<SessionInner>>>,
    bytes: Arc<AtomicUsize>,
    changed: Event,
    #[cfg(unix)]
    endpoint: unix::Endpoint,
    #[cfg(unix)]
    listener: Mutex<Option<smol::Task<()>>>,
}

#[derive(Clone)]
pub struct PeerSession(Arc<SessionInner>);

struct SessionInner {
    host: Arc<HostInner>,
    route: Route,
    state: Mutex<SessionState>,
}

struct SessionState {
    descriptor: PeerDescriptor,
    canonical_cwd: Option<PathBuf>,
    floor: InboundPolicy,
    inbound_override: Option<InboundPolicy>,
    open: bool,
    wakes_suppressed: bool,
    epoch: u64,
    next_claim: u64,
    delivered: usize,
    sends: usize,
    bytes: usize,
    inbox: VecDeque<InboxItem>,
    dedup: HashMap<String, DedupEntry>,
    outgoing: HashMap<String, Outgoing>,
    peer_names: HashMap<String, String>,
    message_names: HashMap<String, MessageIdentity>,
    arrivals: VecDeque<(Instant, String)>,
    reviews: HashMap<String, u64>,
}

struct InboxItem {
    delivery: Delivery,
    origin: PeerMessageOrigin,
    bytes: usize,
    state: ItemState,
    approved_epoch: Option<u64>,
}

impl InboxItem {
    fn observation(&self) -> Message {
        Message::peer_observation(self.delivery.text.clone(), self.origin.clone())
    }
}

#[derive(Clone, PartialEq, Eq)]
struct MessageIdentity {
    sender: String,
    message_id: String,
}

enum ItemState {
    Pending,
    Held(String),
    Claimed(u64),
    Staged,
}

struct DedupEntry {
    fingerprint: [u8; 32],
    receipt: SendReceipt,
}

struct Outgoing {
    fingerprint: [u8; 32],
    target: String,
    message_id: String,
    issued_ms: u64,
    epoch: u64,
    sender: Sender,
    receipt: Option<SendReceipt>,
}

pub struct PeerClaim {
    session: PeerSession,
    claim_id: u64,
    messages: Vec<Message>,
    committed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Sender {
    route: Route,
    name: String,
    canonical_cwd: Option<PathBuf>,
    mode: WireMode,
    permission_mode: PermissionMode,
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
}

impl Delivery {
    fn dedup_key(&self) -> String {
        format!("{}:{}", self.sender.route.target(), self.message_id)
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

fn message_name() -> Result<String, String> {
    random_task_id().map_err(|error| format!("Peer randomness unavailable: {error}"))
}

fn peer_name() -> Result<String, String> {
    Ok(format!("{}-{}", message_name()?, message_name()?))
}

fn allocate_name<T>(
    names: &HashMap<String, T>,
    capacity: usize,
    preferred: Option<&str>,
    mut candidate: impl FnMut() -> Result<String, String>,
) -> Result<String, String> {
    if names.len() >= capacity {
        return Err(NAME_FULL.into());
    }
    if let Some(preferred) = preferred
        && !names.contains_key(preferred)
    {
        return Ok(preferred.to_owned());
    }
    for _ in 0..MAX_NAME_ATTEMPTS {
        let name = candidate()?;
        if !names.contains_key(&name) {
            return Ok(name);
        }
    }
    Err(NAME_COLLISION.into())
}

fn wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
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
    pub fn start(features: FeatureFlags) -> Result<Option<Self>, String> {
        if !features.enabled(Feature::CrossSessionMessaging) {
            return Ok(None);
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            Self::bind(
                unix::runtime_directory()?,
                PROCESS_BYTES
                    .get_or_init(|| Arc::new(AtomicUsize::new(0)))
                    .clone(),
            )
            .map(Some)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        Err(UNAVAILABLE.into())
    }

    #[cfg(unix)]
    fn bind(directory: PathBuf, bytes: Arc<AtomicUsize>) -> Result<Self, String> {
        let incarnation = token()?;
        let (endpoint, listener) = unix::Endpoint::bind(directory, &incarnation)?;
        let host = Arc::new(HostInner {
            incarnation,
            sessions: Mutex::default(),
            bytes,
            changed: Event::new(),
            endpoint,
            listener: Mutex::new(None),
        });
        *lock(&host.listener) = Some(unix::listen(Arc::downgrade(&host), listener));
        Ok(Self(host))
    }

    #[cfg(all(test, unix))]
    pub(crate) fn start_in(directory: PathBuf, bytes: Arc<AtomicUsize>) -> Result<Self, String> {
        Self::bind(directory, bytes)
    }

    pub fn register(&self, descriptor: PeerDescriptor) -> Result<PeerSession, String> {
        self.register_with_controls(descriptor, None, None)
    }

    pub fn register_with_controls(
        &self,
        mut descriptor: PeerDescriptor,
        floor: Option<InboundPolicy>,
        controls: Option<StoredPeerControls>,
    ) -> Result<PeerSession, String> {
        validate_descriptor(&descriptor)?;
        let floor = floor.unwrap_or(InboundPolicy::Accept);
        let controls = controls.unwrap_or_default();
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
                descriptor,
                open: true,
                wakes_suppressed: false,
                epoch: 0,
                next_claim: 0,
                delivered: controls.delivered.min(PEER_BUDGET),
                sends: controls.sends.min(PEER_BUDGET),
                bytes: 0,
                inbox: VecDeque::new(),
                dedup: HashMap::new(),
                outgoing: HashMap::new(),
                peer_names: HashMap::new(),
                message_names: HashMap::new(),
                arrivals: VecDeque::new(),
                reviews: HashMap::new(),
            }),
        });
        sessions.insert(id, Arc::downgrade(&session));
        registry.insert(id, Arc::downgrade(&session));
        Ok(PeerSession(session))
    }

    pub fn notified(&self) -> impl Future<Output = ()> + Send + 'static {
        self.0.changed.listen()
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
            delivered: state.delivered,
            sends: state.sends,
        }
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
        state.inbound_override = Some(policy.clone());
        if policy != state.descriptor.inbound {
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

    pub fn has_pending(&self) -> bool {
        let mut state = lock(&self.0.state);
        state.reevaluate();
        state.open
            && state
                .inbox
                .iter()
                .any(|item| matches!(item.state, ItemState::Pending))
    }

    pub async fn list(&self) -> Result<Vec<PeerInfo>, String> {
        lock(&self.0.state).ensure_open()?;
        #[cfg(unix)]
        {
            let peers = unix::discover(&self.0.host.endpoint).await?;
            lock(&self.0.state).ensure_open()?;
            Ok(peers
                .into_iter()
                .filter(|peer| peer.target != self.0.route.target())
                .collect())
        }
        #[cfg(not(unix))]
        Err(UNAVAILABLE.into())
    }

    pub async fn list_named(&self) -> Result<Vec<PeerSummary>, String> {
        let peers = self.list().await?;
        let mut state = lock(&self.0.state);
        state.ensure_open()?;
        peers
            .into_iter()
            .map(|peer| {
                Ok(PeerSummary {
                    target: state.peer_name(&peer.target)?,
                    name: peer.name,
                    cwd: peer.cwd,
                    busy: peer.busy,
                    blocked: peer.blocked,
                    inbound: peer.inbound,
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
        if !valid_name(target, PEER_WORDS) {
            return Err(UNKNOWN_TARGET.into());
        }
        let (target, reply_to) = {
            let state = lock(&self.0.state);
            state.ensure_open()?;
            let route = state.peer_names.get(target).ok_or(UNKNOWN_TARGET)?;
            let reply_to = reply_to
                .map(|name| {
                    state
                        .message_names
                        .get(name)
                        .filter(|identity| {
                            state.matches_counterpart(identity, route, &self.0.route.target())
                        })
                        .cloned()
                        .ok_or(UNKNOWN_REPLY)
                })
                .transpose()?;
            (route.clone(), reply_to)
        };
        self.send_with_reply(
            &target,
            text,
            reply_to
                .as_ref()
                .map(|identity| identity.message_id.as_str()),
            reply_to.as_ref().map(|identity| identity.sender.as_str()),
            request_id,
        )
        .await
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
        Route::parse(target)?;
        if text.is_empty() || text.len() > MAX_BODY_BYTES {
            return Err("Peer text must contain between 1 byte and 32 KiB of UTF-8".into());
        }
        if request_id.is_empty()
            || request_id.len() > MAX_CORRELATION_BYTES
            || reply_to.is_some_and(|value| value.len() > MAX_CORRELATION_BYTES)
        {
            return Err("Peer request/correlation identity exceeds its limit".into());
        }
        let fingerprint: [u8; 32] = Sha256::digest(
            serde_json::to_vec(&(target, text, reply_to, reply_sender))
                .map_err(|error| error.to_string())?,
        )
        .into();
        let (delivery, epoch) = {
            let mut state = lock(&self.0.state);
            state.ensure_open()?;
            if state.wakes_suppressed
                || state.descriptor.blocked
                || state.descriptor.mode.is_read_only()
            {
                return Err("Blocked or ReadOnly sessions cannot send peer messages".into());
            }
            if let Some(previous) = state.outgoing.get(request_id) {
                if previous.fingerprint != fingerprint {
                    return Err("Peer request identity was reused with different content".into());
                }
                if previous.epoch != state.epoch {
                    return Err(
                        "Peer retry invalidated by a session policy or workspace change".into(),
                    );
                }
                if wall_ms().saturating_sub(previous.issued_ms) > RETRY_WINDOW.as_millis() as u64 {
                    return Err("Peer retry identity expired; do not retry as a new message".into());
                }
                if let Some(receipt) = &previous.receipt
                    && receipt.status != "unknown"
                {
                    return Ok(receipt.clone());
                }
                (
                    Delivery {
                        message_id: previous.message_id.clone(),
                        issued_ms: previous.issued_ms,
                        target: target.to_owned(),
                        sender: previous.sender.clone(),
                        text: text.to_owned(),
                        reply_to: reply_to.map(str::to_owned),
                        reply_sender: reply_sender.map(str::to_owned),
                    },
                    state.epoch,
                )
            } else {
                if state.sends >= PEER_BUDGET {
                    return Err(SEND_BUDGET.into());
                }
                if state.outgoing.len() >= MAX_DEDUP {
                    return Err(RETRY_FULL.into());
                }
                let message_id =
                    allocate_name(&state.message_names, MAX_MESSAGE_NAMES, None, message_name)?;
                state.message_names.insert(
                    message_id.clone(),
                    MessageIdentity {
                        sender: self.0.route.target(),
                        message_id: message_id.clone(),
                    },
                );
                let delivery = Delivery {
                    message_id,
                    issued_ms: wall_ms(),
                    target: target.to_owned(),
                    sender: Sender {
                        route: self.0.route.clone(),
                        name: state.descriptor.name.clone(),
                        canonical_cwd: state.canonical_cwd.clone(),
                        mode: WireMode::from(&state.descriptor.mode),
                        permission_mode: state.descriptor.permission_mode.clone(),
                    },
                    text: text.to_owned(),
                    reply_to: reply_to.map(str::to_owned),
                    reply_sender: reply_sender.map(str::to_owned),
                };
                state.sends += 1;
                let epoch = state.epoch;
                state.outgoing.insert(
                    request_id.to_owned(),
                    Outgoing {
                        fingerprint,
                        target: delivery.target.clone(),
                        message_id: delivery.message_id.clone(),
                        issued_ms: delivery.issued_ms,
                        epoch,
                        sender: delivery.sender.clone(),
                        receipt: None,
                    },
                );
                (delivery, epoch)
            }
        };
        #[cfg(unix)]
        let receipt = unix::send(self, delivery, epoch).await;
        #[cfg(not(unix))]
        let receipt = SendReceipt::new("unavailable", &delivery.message_id, Some(UNAVAILABLE));
        if let Some(outgoing) = lock(&self.0.state).outgoing.get_mut(request_id) {
            outgoing.receipt = Some(receipt.clone());
        }
        Ok(receipt)
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

    pub fn approve(&self, message_id: &str) -> Result<(), String> {
        let mut state = lock(&self.0.state);
        state.ensure_open()?;
        if state.descriptor.inbound == InboundPolicy::Refuse {
            return Err(REFUSED_POLICY.into());
        }
        if state.wakes_suppressed || state.descriptor.blocked {
            return Err(HELD_BLOCKED.into());
        }
        let epoch = state.epoch;
        if state.reviews.get(message_id) != Some(&epoch) {
            return Err(
                "Held message review is stale; inspect the current held messages again".into(),
            );
        }
        let item = state
            .inbox
            .iter_mut()
            .find(|item| {
                item.origin.message_id == message_id && matches!(item.state, ItemState::Held(_))
            })
            .ok_or("Held message no longer exists")?;
        item.approved_epoch = Some(epoch);
        state.reevaluate();
        self.0.host.changed.notify(usize::MAX);
        Ok(())
    }

    pub fn reject(&self, message_id: &str) -> Result<(), String> {
        let mut state = lock(&self.0.state);
        state.ensure_open()?;
        let index = state
            .inbox
            .iter()
            .position(|item| {
                item.origin.message_id == message_id && matches!(item.state, ItemState::Held(_))
            })
            .ok_or("Held message no longer exists")?;
        if let Some(item) = state.inbox.remove(index) {
            state.bytes -= item.bytes;
            self.0.host.bytes.fetch_sub(item.bytes, Ordering::AcqRel);
            if let Some(entry) = state.dedup.get_mut(&item.delivery.dedup_key()) {
                entry.receipt = SendReceipt::new(
                    "refused",
                    &item.delivery.message_id,
                    Some("Rejected by the local receiver"),
                );
            }
        }
        state.reviews.remove(message_id);
        Ok(())
    }

    pub fn reset_budget(&self) {
        let mut state = lock(&self.0.state);
        if state.open {
            state.delivered = 0;
            state.sends = 0;
            state.wakes_suppressed = false;
            state.reevaluate();
            self.0.host.changed.notify(usize::MAX);
        }
    }

    pub fn claim(&self) -> Option<PeerClaim> {
        let mut state = lock(&self.0.state);
        self.claim_locked(&mut state)
    }

    pub fn claim_or_close(&self) -> Option<PeerClaim> {
        let mut state = lock(&self.0.state);
        let claim = self.claim_locked(&mut state);
        if claim.is_none() {
            self.0.close_locked(&mut state);
        }
        claim
    }

    fn claim_locked(&self, state: &mut SessionState) -> Option<PeerClaim> {
        if !state.open {
            return None;
        }
        state.reevaluate();
        let reserved = state
            .inbox
            .iter()
            .filter(|item| matches!(item.state, ItemState::Claimed(_)))
            .count();
        let limit = PEER_BUDGET
            .saturating_sub(state.delivered.saturating_add(reserved))
            .min(CLAIM_BATCH);
        state.next_claim += 1;
        let claim_id = state.next_claim;
        let mut messages = Vec::new();
        for item in &mut state.inbox {
            if messages.len() == limit {
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
                    if Some(origin) == observation.peer_event.as_ref()
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
    fn matches_counterpart(&self, identity: &MessageIdentity, peer: &str, own: &str) -> bool {
        identity.sender == peer
            || (identity.sender == own
                && self.outgoing.values().any(|outgoing| {
                    outgoing.message_id == identity.message_id && outgoing.target == peer
                }))
    }

    fn reply_name(
        &self,
        sender: &str,
        own: &str,
        message_id: &str,
        reply_sender: Option<&str>,
    ) -> Result<String, String> {
        let mut matches = self.message_names.iter().filter(|(_, identity)| {
            identity.message_id == message_id
                && reply_sender.is_none_or(|sender| sender == identity.sender)
                && self.matches_counterpart(identity, sender, own)
        });
        let (name, _) = matches.next().ok_or(UNKNOWN_REPLY)?;
        if matches.next().is_some() {
            return Err(AMBIGUOUS_REPLY.into());
        }
        Ok(name.clone())
    }

    fn peer_name(&mut self, route: &str) -> Result<String, String> {
        if let Some((name, _)) = self.peer_names.iter().find(|(_, known)| *known == route) {
            return Ok(name.clone());
        }
        let name = allocate_name(&self.peer_names, MAX_PEER_NAMES, None, peer_name)?;
        self.peer_names.insert(name.clone(), route.to_owned());
        Ok(name)
    }

    fn message_name(&mut self, sender: &str, message_id: &str) -> Result<String, String> {
        let identity = MessageIdentity {
            sender: sender.to_owned(),
            message_id: message_id.to_owned(),
        };
        if let Some((name, _)) = self
            .message_names
            .iter()
            .find(|(_, known)| **known == identity)
        {
            return Ok(name.clone());
        }
        let preferred = valid_name(message_id, MESSAGE_WORDS).then_some(message_id);
        let name = allocate_name(
            &self.message_names,
            MAX_MESSAGE_NAMES,
            preferred,
            message_name,
        )?;
        self.message_names.insert(name.clone(), identity);
        Ok(name)
    }

    fn ensure_open(&self) -> Result<(), String> {
        if self.open {
            Ok(())
        } else {
            Err(CLOSED.into())
        }
    }

    fn hold_reason(
        &self,
        delivery: &Delivery,
        approved_epoch: Option<u64>,
    ) -> Option<&'static str> {
        if self.descriptor.inbound == InboundPolicy::Refuse {
            return Some(REFUSED_POLICY);
        }
        if self.wakes_suppressed || self.descriptor.blocked {
            return Some(HELD_BLOCKED);
        }
        if self.delivered >= PEER_BUDGET {
            return Some(HELD_BUDGET);
        }
        if approved_epoch == Some(self.epoch) {
            return None;
        }
        match self.descriptor.inbound {
            InboundPolicy::Accept => None,
            InboundPolicy::Hold | InboundPolicy::Refuse => Some(HELD_POLICY),
            InboundPolicy::Auto => {
                let receiver_mode = WireMode::from(&self.descriptor.mode);
                let compatible = matches!(
                    (&delivery.sender.mode, &receiver_mode),
                    (WireMode::Build, WireMode::Build) | (WireMode::Plan, WireMode::Plan)
                );
                let same_cwd = self.canonical_cwd.is_some()
                    && self.canonical_cwd == delivery.sender.canonical_cwd;
                if compatible
                    && same_cwd
                    && self.descriptor.permission_mode == PermissionMode::Ask
                    && delivery.sender.permission_mode == PermissionMode::Ask
                {
                    None
                } else {
                    Some(HELD_COHORT)
                }
            }
        }
    }

    fn reevaluate(&mut self) {
        // Moving pending messages to held must not evict accepted messages. Admission
        // caps the combined queue at MAX_HELD so later policy tightening always fits.
        for index in 0..self.inbox.len() {
            let item = &self.inbox[index];
            if matches!(item.state, ItemState::Claimed(_) | ItemState::Staged) {
                continue;
            }
            let reason = self.hold_reason(&item.delivery, item.approved_epoch);
            self.inbox[index].state =
                reason.map_or(ItemState::Pending, |reason| ItemState::Held(reason.into()));
        }
    }
}

impl SessionInner {
    fn close(&self) {
        let mut state = lock(&self.state);
        self.close_locked(&mut state);
    }

    fn close_locked(&self, state: &mut SessionState) {
        state.open = false;
        state.epoch += 1;
        let mut released = 0;
        state.inbox.retain(|item| {
            if matches!(item.state, ItemState::Claimed(_)) {
                true
            } else {
                released += item.bytes;
                false
            }
        });
        self.host.bytes.fetch_sub(released, Ordering::AcqRel);
        state.bytes -= released;
        state.outgoing.clear();
        state.dedup.clear();
        state.reviews.clear();
        self.host.changed.notify(usize::MAX);
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
        if !(valid_name(&delivery.message_id, MESSAGE_WORDS) || valid_token(&delivery.message_id))
            || !valid_token(&delivery.sender.route.host)
            || !valid_token(&delivery.sender.route.generation)
            || delivery.text.is_empty()
            || delivery.text.len() > MAX_BODY_BYTES
            || delivery.sender.name.len() > MAX_LABEL_BYTES
            || delivery
                .sender
                .canonical_cwd
                .as_ref()
                .is_some_and(|path| !path.is_absolute() || path.as_os_str().len() > MAX_PATH_BYTES)
            || delivery
                .reply_to
                .as_ref()
                .is_some_and(|value| value.len() > MAX_CORRELATION_BYTES)
            || delivery
                .reply_sender
                .as_deref()
                .is_some_and(|sender| delivery.reply_to.is_none() || Route::parse(sender).is_err())
        {
            return Err("Invalid peer message metadata or text bounds".into());
        }
        if now_ms.saturating_sub(delivery.issued_ms) > RETRY_WINDOW.as_millis() as u64
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
        if state.dedup.len() >= MAX_DEDUP {
            return Ok(SendReceipt::new(
                "rate_limited",
                &delivery.message_id,
                Some(RETRY_FULL),
            ));
        }
        let sender = delivery.sender.route.target();
        let reply_to = delivery
            .reply_to
            .as_deref()
            .map(|message_id| {
                state.reply_name(
                    &sender,
                    &self.route.target(),
                    message_id,
                    delivery.reply_sender.as_deref(),
                )
            })
            .transpose()?;
        let sender_name = state.peer_name(&sender)?;
        let origin = PeerMessageOrigin {
            message_id: state.message_name(&sender, &delivery.message_id)?,
            sender_session_id: sender_name.clone(),
            sender_name: delivery.sender.name.clone(),
            reply_target: sender_name,
            reply_to,
        };
        let bytes = encoded.len()
            + serde_json::to_vec(&Message::peer_observation(
                delivery.text.clone(),
                origin.clone(),
            ))
            .map_err(|error| error.to_string())?
            .len();
        while state
            .arrivals
            .front()
            .is_some_and(|(time, _)| now.saturating_duration_since(*time) >= RATE_WINDOW)
        {
            state.arrivals.pop_front();
        }
        let receipt = if state.descriptor.inbound == InboundPolicy::Refuse
            || delivery.sender.mode == WireMode::ReadOnly
        {
            SendReceipt::new("refused", &delivery.message_id, Some(REFUSED_POLICY))
        } else if state.arrivals.len() >= RECIPIENT_RATE
            || state
                .arrivals
                .iter()
                .filter(|(_, peer)| peer == &sender)
                .count()
                >= SENDER_RATE
        {
            SendReceipt::new(
                "rate_limited",
                &delivery.message_id,
                Some("Recipient peer message rate limit reached"),
            )
        } else if state.inbox.len() >= MAX_PENDING.min(MAX_HELD)
            || state.bytes + bytes > MAX_SESSION_BYTES
            || self
                .host
                .bytes
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                    current
                        .checked_add(bytes)
                        .filter(|next| *next <= MAX_PROCESS_BYTES)
                })
                .is_err()
        {
            SendReceipt::new("rate_limited", &delivery.message_id, Some(FULL))
        } else {
            let reason = state.hold_reason(&delivery, None);
            let receipt = SendReceipt::new(
                if reason.is_some() { "held" } else { "queued" },
                &delivery.message_id,
                reason,
            );
            let item_state =
                reason.map_or(ItemState::Pending, |reason| ItemState::Held(reason.into()));
            state.bytes += bytes;
            state.arrivals.push_back((now, sender));
            state.inbox.push_back(InboxItem {
                delivery: delivery.clone(),
                origin,
                bytes,
                state: item_state,
                approved_epoch: None,
            });
            self.host.changed.notify(usize::MAX);
            receipt
        };
        state.dedup.insert(
            dedup_key,
            DedupEntry {
                fingerprint,
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
        let mut count = 0;
        let retain = stage && state.open;
        state.inbox.retain_mut(|item| {
            if matches!(item.state, ItemState::Claimed(id) if id == self.claim_id) {
                count += 1;
                if retain {
                    item.state = ItemState::Staged;
                    true
                } else {
                    bytes += item.bytes;
                    false
                }
            } else {
                true
            }
        });
        state.bytes -= bytes;
        state.delivered = state.delivered.saturating_add(count).min(PEER_BUDGET);
        self.session.0.host.bytes.fetch_sub(bytes, Ordering::AcqRel);
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
                state.inbox.retain(|item| {
                    if matches!(item.state, ItemState::Claimed(id) if id == self.claim_id) {
                        released += item.bytes;
                        false
                    } else {
                        true
                    }
                });
                state.bytes -= released;
                self.session
                    .0
                    .host
                    .bytes
                    .fetch_sub(released, Ordering::AcqRel);
            }
            state.reevaluate();
            self.session.0.host.changed.notify(usize::MAX);
        }
    }
}

impl HostInner {
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
                            cwd: state.descriptor.cwd.clone(),
                            busy: state.descriptor.busy,
                            blocked: state.wakes_suppressed || state.descriptor.blocked,
                            inbound: state.descriptor.inbound.clone(),
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
    use std::collections::HashMap;
    use std::fs::{self, Permissions};
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::Instant;

    use caudra_config::{FeatureFlags, InboundPolicy};
    use caudra_providers::{Message, PeerMessageOrigin, expand_message};
    use caudra_storage::id::CaudraId;
    use caudra_storage::sessions::{PermissionMode, StoredInboundPolicy, StoredPeerControls};
    use tempfile::{Builder, TempDir};
    use test_case::test_case;

    use super::{
        AMBIGUOUS_REPLY, CLAIM_BATCH, CLOSED, Delivery, FULL, HELD_BLOCKED, HELD_BUDGET,
        HELD_COHORT, HELD_POLICY, MAX_BODY_BYTES, MAX_DEDUP, MAX_HELD, MAX_MESSAGE_NAMES,
        MAX_NAME_ATTEMPTS, MAX_PEER_NAMES, MAX_PROCESS_BYTES, MAX_SESSION_BYTES, MESSAGE_WORDS,
        MessageIdentity, NAME_COLLISION, NAME_FULL, Outgoing, PEER_BUDGET, PEER_WORDS,
        POLICY_FLOOR, PeerDescriptor, PeerHost, PeerSession, PeerSummary, RATE_WINDOW,
        REFUSED_POLICY, RETRY_FULL, RETRY_WINDOW, Route, SEND_BUDGET, STALE_TARGET, SendReceipt,
        Sender, UNKNOWN_REPLY, UNKNOWN_TARGET, WireMode, allocate_name, lock, message_name, token,
        valid_name, wall_ms,
    };
    use crate::AgentMode;

    const TEXT: &str = "/compact !not-a-command @not-an-attachment";
    const QUEUED: &str = "queued";
    const HELD: &str = "held";
    const REFUSED: &str = "refused";
    const RATE_LIMITED: &str = "rate_limited";
    const UNKNOWN: &str = "unknown";
    const MESSAGE_NAME: &str = "brisk-calm-otter";
    const OTHER_MESSAGE_NAME: &str = "gentle-bright-falcon";
    const THIRD_MESSAGE_NAME: &str = "quiet-keen-wren";
    const PEER_NAME: &str = "brisk-calm-otter-gentle-bright-falcon";
    const OTHER_PEER_NAME: &str = "gentle-bright-falcon-brisk-calm-otter";
    const REQUEST_ID: &str = "named-request";
    const REPLY_REQUEST_ID: &str = "named-reply";
    const ORIGINAL_REQUEST_ID: &str = "original-request";

    pub(super) fn directory() -> TempDir {
        Builder::new()
            .prefix("cp-")
            .permissions(Permissions::from_mode(0o700))
            .tempdir_in(Path::new("/tmp").canonicalize().unwrap())
            .unwrap()
    }

    fn host(directory: &Path) -> PeerHost {
        PeerHost::start_in(directory.to_owned(), Arc::new(AtomicUsize::new(0))).unwrap()
    }

    fn descriptor(cwd: &Path, inbound: InboundPolicy) -> PeerDescriptor {
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

    fn fixture(inbound: InboundPolicy) -> (TempDir, PeerHost, PeerSession) {
        let directory = directory();
        let host = host(directory.path());
        let session = host
            .register(descriptor(directory.path(), inbound))
            .unwrap();
        (directory, host, session)
    }

    fn delivery(session: &PeerSession) -> Delivery {
        let message_id = allocate_name(
            &lock(&session.0.state).message_names,
            MAX_MESSAGE_NAMES,
            None,
            message_name,
        )
        .unwrap();
        Delivery {
            message_id,
            issued_ms: wall_ms(),
            target: session.0.route.target(),
            text: TEXT.into(),
            reply_to: None,
            reply_sender: None,
            sender: Sender {
                route: Route {
                    host: token().unwrap(),
                    session: CaudraId::generate(),
                    generation: token().unwrap(),
                },
                name: "sender".into(),
                canonical_cwd: session.descriptor().cwd.canonicalize().ok(),
                mode: WireMode::Build,
                permission_mode: PermissionMode::Ask,
            },
        }
    }

    fn record_outgoing(session: &PeerSession, target: &str, message_id: &str) -> Delivery {
        let mut delivery = delivery(session);
        delivery.message_id = message_id.into();
        delivery.target = target.into();
        delivery.sender.route = session.0.route.clone();
        let mut state = lock(&session.0.state);
        assert!(
            state
                .message_names
                .insert(
                    message_id.into(),
                    MessageIdentity {
                        sender: session.0.route.target(),
                        message_id: message_id.into(),
                    },
                )
                .is_none()
        );
        let epoch = state.epoch;
        state.outgoing.insert(
            ORIGINAL_REQUEST_ID.into(),
            Outgoing {
                fingerprint: [0; 32],
                target: target.into(),
                message_id: message_id.into(),
                issued_ms: delivery.issued_ms,
                epoch,
                sender: delivery.sender.clone(),
                receipt: None,
            },
        );
        delivery
    }

    #[test_case(MESSAGE_NAME, OTHER_MESSAGE_NAME; "message_names")]
    #[test_case(PEER_NAME, OTHER_PEER_NAME; "peer_names")]
    fn name_allocation_skips_forced_collisions(used: &str, free: &str) {
        let names = HashMap::from([(used.to_owned(), TEXT)]);
        let mut candidates = [used, free].into_iter();
        assert_eq!(
            allocate_name(&names, MAX_PEER_NAMES, Some(used), || {
                Ok(candidates.next().unwrap().to_owned())
            })
            .unwrap(),
            free
        );
        assert!(candidates.next().is_none());
        assert_eq!(names.get(used), Some(&TEXT));
    }

    #[test_case(true; "capacity_exhaustion")]
    #[test_case(false; "candidate_exhaustion")]
    fn name_allocation_exhaustion_preserves_bindings(full: bool) {
        let names = HashMap::from([(PEER_NAME.to_owned(), TEXT)]);
        let mut attempts = 0;
        let capacity = if full { names.len() } else { MAX_PEER_NAMES };
        let error = allocate_name(&names, capacity, None, || {
            attempts += 1;
            Ok(PEER_NAME.to_owned())
        })
        .unwrap_err();
        assert_eq!(error, if full { NAME_FULL } else { NAME_COLLISION });
        assert_eq!(attempts, if full { 0 } else { MAX_NAME_ATTEMPTS });
        assert_eq!(names.get(PEER_NAME), Some(&TEXT));
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
        assert!(state.message_names.is_empty());
        assert_eq!(state.sends, 0);
    }

    #[test_case(true; "peer_names")]
    #[test_case(false; "message_names")]
    fn incoming_alias_capacity_is_checked_before_accepting(peer_names: bool) {
        let (_directory, host, session) = fixture(InboundPolicy::Auto);
        let delivery = delivery(&session);
        let known_route = session.0.route.target();
        {
            let mut state = lock(&session.0.state);
            if peer_names {
                state.peer_names = (1..MAX_PEER_NAMES)
                    .map(|index| (index.to_string(), index.to_string()))
                    .collect();
            } else {
                state.message_names = (1..MAX_MESSAGE_NAMES)
                    .map(|index| {
                        (
                            index.to_string(),
                            MessageIdentity {
                                sender: known_route.clone(),
                                message_id: index.to_string(),
                            },
                        )
                    })
                    .collect();
                state.message_name(&known_route, MESSAGE_NAME).unwrap();
                assert_eq!(
                    state.message_name(&known_route, MESSAGE_NAME).unwrap(),
                    MESSAGE_NAME
                );
            }
            state
                .peer_names
                .insert(PEER_NAME.into(), known_route.clone());
            assert_eq!(state.peer_name(&known_route).unwrap(), PEER_NAME);
        }
        assert_eq!(
            session
                .0
                .receive(delivery, Instant::now(), wall_ms())
                .unwrap_err(),
            NAME_FULL
        );
        let state = lock(&session.0.state);
        assert!(state.inbox.is_empty());
        assert!(state.dedup.is_empty());
        assert_eq!(state.peer_names.get(PEER_NAME), Some(&known_route));
        assert_eq!(host.0.bytes.load(Ordering::Acquire), 0);
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
        assert!(valid_name(&origin.sender_session_id, PEER_WORDS));
        assert_eq!(origin.sender_session_id, origin.reply_target);
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
        let mut state = lock(&session.0.state);
        state.message_names.insert(
            MESSAGE_NAME.into(),
            MessageIdentity {
                sender: session.0.route.target(),
                message_id: MESSAGE_NAME.into(),
            },
        );
        drop(state);
        session
            .0
            .receive(delivery.clone(), Instant::now(), wall_ms())
            .unwrap();
        let claim = session.claim().unwrap();
        let received = &claim.messages()[0].peer_event.as_ref().unwrap().message_id;
        assert_ne!(received, MESSAGE_NAME);
        let state = lock(&session.0.state);
        let free = [OTHER_MESSAGE_NAME, THIRD_MESSAGE_NAME]
            .into_iter()
            .find(|name| !state.message_names.contains_key(*name))
            .unwrap();
        let mut candidates = [MESSAGE_NAME, received, free].into_iter();
        let next = allocate_name(&state.message_names, MAX_MESSAGE_NAMES, None, || {
            Ok(candidates.next().unwrap().to_owned())
        })
        .unwrap();
        assert_eq!(next, free);
        assert_eq!(
            state.message_names.get(received).unwrap().message_id,
            MESSAGE_NAME
        );
    }

    #[test]
    fn disabled_start_is_inert() {
        assert!(PeerHost::start(FeatureFlags::NONE).unwrap().is_none());
    }

    #[test]
    fn suppression_survives_frontend_updates_until_local_budget_reset() {
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
        session.reset_budget();
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
                Some(InboundPolicy::Auto),
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
    fn delivery_budget_reserves_claims_and_requires_human_reset() {
        let (_directory, _host, session) = fixture(InboundPolicy::Auto);
        for _ in 0..PEER_BUDGET + 1 {
            session
                .0
                .receive(delivery(&session), Instant::now(), wall_ms())
                .unwrap();
        }
        let mut claims = Vec::new();
        for _ in 0..PEER_BUDGET / CLAIM_BATCH {
            claims.push(session.claim().unwrap());
        }
        assert!(session.claim().is_none());
        for claim in claims {
            claim.commit();
        }
        assert_eq!(session.held()[0].reason, HELD_BUDGET);
        session.approve(&session.held()[0].message_id).unwrap();
        assert!(session.claim().is_none());
        session.reset_budget();
        assert!(session.claim().is_some());
    }

    #[test]
    fn recipient_rate_and_expired_retries_are_bounded() {
        let (_directory, _host, session) = fixture(InboundPolicy::Auto);
        let initial = delivery(&session);
        let now = Instant::now();
        for _ in 0..super::SENDER_RATE {
            let mut next = initial.clone();
            next.message_id = token().unwrap();
            assert_eq!(
                session.0.receive(next, now, wall_ms()).unwrap().status,
                QUEUED
            );
        }
        assert_eq!(
            session
                .0
                .receive(initial.clone(), now, wall_ms())
                .unwrap()
                .status,
            RATE_LIMITED
        );
        let mut later = initial.clone();
        later.message_id = token().unwrap();
        assert_eq!(
            session
                .0
                .receive(later, now + RATE_WINDOW, wall_ms())
                .unwrap()
                .status,
            QUEUED
        );
        let expired_ms = initial.issued_ms + RETRY_WINDOW.as_millis() as u64 + 1;
        assert_eq!(
            session.0.receive(initial, now, expired_ms).unwrap().status,
            REFUSED
        );
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
            assert_eq!(sender.controls().sends, 0);
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
            assert_eq!(origin.sender_session_id, origin.reply_target);
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
            assert_eq!(listed[0].name, TEXT);
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
                (outgoing.issued_ms, state.message_names.len())
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
            assert_eq!(outgoing.issued_ms, issued_ms);
            assert_eq!(state.message_names.len(), names);
            assert_eq!(state.sends, 1);
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
        assert!(state.peer_names.is_empty());
        assert!(state.message_names.is_empty());
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
                    TEXT,
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
                        TEXT,
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
                        TEXT,
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
                        TEXT,
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

    #[test_case(false; "send_budget")]
    #[test_case(true; "retry_capacity")]
    fn admission_rejections_never_consume_message_names(retry_capacity: bool) {
        smol::block_on(async {
            let (_directory, host, sender) = fixture(InboundPolicy::Auto);
            let receiver = host
                .register(descriptor(&sender.descriptor().cwd, InboundPolicy::Auto))
                .unwrap();
            let target = sender.list_named().await.unwrap().remove(0).target;
            let original = delivery(&sender);
            {
                let mut state = lock(&sender.0.state);
                if retry_capacity {
                    state.outgoing = (0..MAX_DEDUP)
                        .map(|index| {
                            (
                                index.to_string(),
                                Outgoing {
                                    fingerprint: [0; 32],
                                    target: receiver.0.route.target(),
                                    message_id: MESSAGE_NAME.into(),
                                    issued_ms: original.issued_ms,
                                    epoch: state.epoch,
                                    sender: original.sender.clone(),
                                    receipt: None,
                                },
                            )
                        })
                        .collect();
                } else {
                    state.sends = PEER_BUDGET;
                }
            }
            let error = if retry_capacity {
                RETRY_FULL
            } else {
                SEND_BUDGET
            };
            for _ in 0..=MAX_MESSAGE_NAMES {
                assert_eq!(
                    sender
                        .send_named(&target, TEXT, None, REQUEST_ID)
                        .await
                        .unwrap_err(),
                    error
                );
            }
            assert!(lock(&sender.0.state).message_names.is_empty());
            sender.reset_budget();
            if retry_capacity {
                assert_eq!(
                    sender
                        .send_named(&target, TEXT, None, REQUEST_ID)
                        .await
                        .unwrap_err(),
                    RETRY_FULL
                );
            } else {
                let receipt = sender
                    .send_named(&target, TEXT, None, REQUEST_ID)
                    .await
                    .unwrap();
                assert_eq!(receipt.status, QUEUED);
                assert_eq!(
                    sender
                        .send_named(&target, TEXT, None, REQUEST_ID)
                        .await
                        .unwrap(),
                    receipt
                );
                assert_eq!(lock(&sender.0.state).message_names.len(), 1);
            }
        });
    }

    #[test]
    fn outbound_budget_does_not_reset_on_delivery_or_retry() {
        smol::block_on(async {
            let (_directory, host, sender) = fixture(InboundPolicy::Auto);
            let receiver = host
                .register(descriptor(&sender.descriptor().cwd, InboundPolicy::Auto))
                .unwrap();
            let target = receiver.0.route.target();
            for index in 0..PEER_BUDGET {
                assert_eq!(
                    sender
                        .send(&target, TEXT, None, &index.to_string())
                        .await
                        .unwrap()
                        .status,
                    QUEUED
                );
            }
            assert_eq!(
                sender.send(&target, TEXT, None, "0").await.unwrap().status,
                QUEUED
            );
            assert_eq!(
                sender
                    .send(&target, TEXT, None, "over-budget")
                    .await
                    .unwrap_err(),
                SEND_BUDGET
            );
            sender.reset_budget();
            assert_eq!(lock(&sender.0.state).sends, 0);
        });
    }

    #[test]
    fn host_cleanup_removes_only_its_own_files() {
        let directory = directory();
        let host = host(directory.path());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
        drop(host);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
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
                floor,
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
    fn restored_controls_preserve_override_and_clamp_counters(
        floor: Option<InboundPolicy>,
        restored: StoredInboundPolicy,
        expected: InboundPolicy,
    ) {
        let directory = directory();
        let host = host(directory.path());
        let session = host
            .register_with_controls(
                descriptor(directory.path(), InboundPolicy::Hold),
                floor,
                Some(StoredPeerControls {
                    inbound: Some(restored),
                    delivered: usize::MAX,
                    sends: usize::MAX,
                }),
            )
            .unwrap();
        assert_eq!(session.descriptor().inbound, expected);
        assert_eq!(
            session.controls(),
            StoredPeerControls {
                inbound: Some(super::stored_policy(&expected)),
                delivered: PEER_BUDGET,
                sends: PEER_BUDGET
            }
        );
        assert!(session.claim().is_none());
        assert_eq!(
            smol::block_on(session.send(&session.0.route.target(), TEXT, None, "restored-budget"))
                .unwrap_err(),
            SEND_BUDGET
        );
        session.reset_budget();
        assert_eq!(session.controls().delivered, 0);
        assert_eq!(session.controls().sends, 0);
    }

    #[test]
    fn exhausted_restored_deliveries_hold_new_messages() {
        let directory = directory();
        let host = host(directory.path());
        let session = host
            .register_with_controls(
                descriptor(directory.path(), InboundPolicy::Auto),
                None,
                Some(StoredPeerControls {
                    inbound: None,
                    delivered: usize::MAX,
                    sends: 0,
                }),
            )
            .unwrap();
        let receipt = session
            .0
            .receive(delivery(&session), Instant::now(), wall_ms())
            .unwrap();
        assert_eq!(receipt.reason.as_deref(), Some(HELD_BUDGET));
        assert!(session.claim().is_none());
        session.reset_budget();
        assert!(session.claim().is_some());
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
        assert_eq!(session.controls().delivered, 0);
        session.checkpoint(&saved);
        assert_eq!(host.0.bytes.load(Ordering::Acquire), bytes);
        claim.stage();
        assert_eq!(session.controls().delivered, 1);
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
        assert_eq!(session.controls().delivered, 1);
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
    #[test_case(|origin| origin.sender_session_id.push('x'); "different_sender")]
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
}
