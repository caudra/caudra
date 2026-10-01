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
    arrivals: VecDeque<(Instant, String)>,
    reviews: HashMap<String, u64>,
}

struct InboxItem {
    delivery: Delivery,
    bytes: usize,
    state: ItemState,
    approved_epoch: Option<u64>,
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
}

impl Delivery {
    fn dedup_key(&self) -> String {
        format!("{}:{}", self.sender.route.host, self.message_id)
    }

    fn observation(&self) -> Message {
        Message::peer_observation(
            self.text.clone(),
            PeerMessageOrigin {
                message_id: self.message_id.clone(),
                sender_session_id: self.sender.route.session.to_string(),
                sender_name: self.sender.name.clone(),
                reply_target: self.sender.route.target(),
                reply_to: self.reply_to.clone(),
            },
        )
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    List { version: u32, host: String },
    Send { version: u32, delivery: Delivery },
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

    pub async fn send(
        &self,
        target: &str,
        text: &str,
        reply_to: Option<&str>,
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
            serde_json::to_vec(&(target, text, reply_to)).map_err(|error| error.to_string())?,
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
                    },
                    state.epoch,
                )
            } else {
                let message_id = token()?;
                if state.sends >= PEER_BUDGET {
                    return Ok(SendReceipt::new(
                        "rate_limited",
                        &message_id,
                        Some("Peer send budget exhausted; genuine local input must reset it"),
                    ));
                }
                if state.outgoing.len() >= MAX_DEDUP {
                    return Ok(SendReceipt::new(
                        "rate_limited",
                        &message_id,
                        Some(RETRY_FULL),
                    ));
                }
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
                };
                state.sends += 1;
                let epoch = state.epoch;
                state.outgoing.insert(
                    request_id.to_owned(),
                    Outgoing {
                        fingerprint,
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
                    message_id: item.delivery.message_id.clone(),
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
                item.delivery.message_id == message_id && matches!(item.state, ItemState::Held(_))
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
                item.delivery.message_id == message_id && matches!(item.state, ItemState::Held(_))
            })
            .ok_or("Held message no longer exists")?;
        if let Some(item) = state.inbox.remove(index) {
            state.bytes -= item.bytes;
            self.0.host.bytes.fetch_sub(item.bytes, Ordering::AcqRel);
            if let Some(entry) = state.dedup.get_mut(&item.delivery.dedup_key()) {
                entry.receipt = SendReceipt::new(
                    "refused",
                    message_id,
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
                messages.push(item.delivery.observation());
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
            let observation = item.delivery.observation();
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
        if !valid_token(&delivery.message_id)
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
        let bytes = encoded.len()
            + serde_json::to_vec(&delivery.observation())
                .map_err(|error| error.to_string())?
                .len();
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
        while state
            .arrivals
            .front()
            .is_some_and(|(time, _)| now.saturating_duration_since(*time) >= RATE_WINDOW)
        {
            state.arrivals.pop_front();
        }
        let sender = delivery.sender.route.target();
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
                    Some(session) => match session.receive(delivery, Instant::now(), wall_ms()) {
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
        CLAIM_BATCH, CLOSED, Delivery, FULL, HELD_BLOCKED, HELD_BUDGET, HELD_COHORT, HELD_POLICY,
        MAX_BODY_BYTES, MAX_HELD, MAX_PROCESS_BYTES, MAX_SESSION_BYTES, PEER_BUDGET, POLICY_FLOOR,
        PeerDescriptor, PeerHost, PeerSession, RATE_WINDOW, REFUSED_POLICY, RETRY_WINDOW, Route,
        Sender, WireMode, lock, token, wall_ms,
    };
    use crate::AgentMode;

    const TEXT: &str = "/compact !not-a-command @not-an-attachment";
    const QUEUED: &str = "queued";
    const HELD: &str = "held";
    const REFUSED: &str = "refused";
    const RATE_LIMITED: &str = "rate_limited";

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
        Delivery {
            message_id: token().unwrap(),
            issued_ms: wall_ms(),
            target: session.0.route.target(),
            text: TEXT.into(),
            reply_to: None,
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
                    .unwrap()
                    .status,
                RATE_LIMITED
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
                .unwrap()
                .status,
            RATE_LIMITED
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
