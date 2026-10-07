//! The cloneable face of a session's automation runtime: requests the actor answers, signals it
//! takes without answering, the mirror and its events, outbox claims, and the session's peers
//! with the observer of their messages. The actor handles a claim in order with the signals sent
//! before it, and stores it before it resolves. It never waits on a firing, so neither does a
//! claim.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use arc_swap::ArcSwap;
use async_lock::Mutex;
use caudra_automation::engine::FiringOutcome;
use caudra_automation::host::{ActionReply, ActionRequest, CallSite, DeliveryMode, HostResult};
use caudra_automation::limits::LimitRefusal;
use caudra_automation::request::{
    AutomationError, AutomationRequest, AutomationResponse, DeliveryGate, OutboxClaim,
    SessionSignal,
};
use caudra_automation::snapshot::{
    ActionStatus, AutomationEvent, AutomationSnapshot, AutomationState, FiringSummary,
    MAX_RECENT_FIRINGS, OutboxItem,
};
use caudra_storage::automation::{AutomationActionEnd, AutomationRelease};
use caudra_storage::id::CaudraId;
use flume::{RecvError, TrySendError};
use tracing::{debug, warn};

use super::catalog::AutomationDirs;
use super::clock::Clock;
use super::messaging::{Messaging, Observed, Observer, ReleaseError, Watching};
use super::outbox::Outbox;
use super::registry;
use super::store::{AutomationStore, Mirrored};
use super::work_finished::WorkChecked;
use super::workflows::Settled;
use crate::peers::{MessageObserver, PeerSession};

/// Events a frontend has not taken yet; past this the oldest news is dropped, and the mirror
/// still holds it.
const EVENT_CAPACITY: usize = 1024;

pub(super) type Reply<T> = flume::Sender<Result<T, AutomationError>>;

/// What reaches the actor: requests, signals and claims from the frontends and the agent loop,
/// host calls and endings from firing threads, the messages the observer matched and the peers
/// to answer them through, the workflow runs that settled, and the ends of what runs off the
/// actor.
pub(super) enum Command {
    Request(AutomationRequest, Reply<AutomationResponse>),
    Signal(SessionSignal),
    Claim {
        gate: DeliveryGate,
        reply: Reply<Option<OutboxClaim>>,
    },
    Admit {
        fire_id: String,
        reply: flume::Sender<Result<(), LimitRefusal>>,
    },
    Act {
        fire_id: String,
        site: CallSite,
        request: ActionRequest,
        reply: flume::Sender<HostResult<ActionReply>>,
    },
    Finished {
        fire_id: String,
        outcome: FiringOutcome,
    },
    /// An `http()` request, a message or a workflow start that ended off the actor: the actor
    /// journals `end`, then sends `answer` on `reply`, which the firing waits on. A start whose
    /// firing a stop already released has no `reply`.
    ActionDone {
        fire_id: String,
        seq: u64,
        end: AutomationActionEnd,
        answer: HostResult<ActionReply>,
        reply: Option<flume::Sender<HostResult<ActionReply>>>,
    },
    /// A workflow run of the session that settled.
    WorkflowSettled(Box<Settled>),
    /// A message the session offered the observer, which matched it.
    Observed(Box<Observed>),
    /// The session's peers to message through from now on, or none.
    Messaging(Option<Arc<dyn Messaging>>),
    /// A release that ended off the actor, sent through the session of attach `attachment`.
    Released {
        release: AutomationRelease,
        attachment: u64,
        outcome: Result<(), ReleaseError>,
    },
    /// A check of the work the session published that ended off the actor, read through the
    /// session of attach `attachment`.
    WorkChecked {
        attachment: u64,
        checked: Result<WorkChecked, String>,
    },
    Shutdown(flume::Sender<()>),
}

/// What the actor, the handles and the registry share.
pub(super) struct Shared {
    pub(super) session_id: CaudraId,
    pub(super) commands: flume::Sender<Command>,
    /// Set before the actor drops the commands still waiting when it stops. Nothing answers a
    /// command sent after that, so its sender looks here once it sent.
    closed: AtomicBool,
    pub(super) store: AutomationStore,
    /// The outbox with the session counters; the mirror's `session` and `outbox` change only
    /// under this lock.
    pub(super) outbox: Mutex<Outbox>,
    pub(super) clock: Arc<dyn Clock>,
    directories: AutomationDirs,
    state: ArcSwap<AutomationState>,
    events: flume::Sender<AutomationEvent>,
    receiver: flume::Receiver<AutomationEvent>,
    /// What the message observers match against; only the actor replaces it.
    pub(super) watching: Arc<ArcSwap<Watching>>,
}

#[derive(Clone)]
pub struct AutomationHandle(Arc<Shared>);

/// The actor's end of the command channel. However the actor stops, abandoned mid-await
/// included, dropping it answers the commands still waiting `Unavailable`, and one sent later
/// finds `closed` set.
pub(super) struct Inbox {
    shared: Arc<Shared>,
    commands: flume::Receiver<Command>,
}

impl Shared {
    pub(super) fn new(
        session_id: CaudraId,
        commands: flume::Sender<Command>,
        store: AutomationStore,
        outbox: Outbox,
        clock: Arc<dyn Clock>,
        directories: AutomationDirs,
    ) -> Self {
        let (events, receiver) = flume::bounded(EVENT_CAPACITY);
        let state = AutomationState {
            session: outbox.view(),
            ..AutomationState::default()
        };
        Self {
            session_id,
            commands,
            closed: AtomicBool::new(false),
            store,
            outbox: Mutex::new(outbox),
            clock,
            directories,
            state: ArcSwap::from_pointee(state),
            events,
            receiver,
            watching: Arc::new(ArcSwap::from_pointee(Watching::default())),
        }
    }

    pub(super) fn state(&self) -> Arc<AutomationState> {
        self.state.load_full()
    }

    /// Announces a change the mirror already holds. A full channel drops its oldest event: the
    /// mirror stays right, and a frontend that fell behind reads it.
    pub(super) fn emit(&self, event: AutomationEvent) {
        let Err(TrySendError::Full(event)) = self.events.try_send(event) else {
            return;
        };
        debug!(session = %self.session_id, "oldest automation event dropped");
        let _ = self.receiver.try_recv();
        let _ = self.events.try_send(event);
    }

    fn update(&self, change: impl Fn(&mut AutomationState)) {
        self.state.rcu(|current| {
            let mut next = AutomationState::clone(current);
            change(&mut next);
            next
        });
    }

    /// Publishes the session row and the outbox of `outbox`, which the caller holds locked.
    pub(super) fn publish_outbox(&self, outbox: &Outbox, now: i64) {
        let session = outbox.view();
        let items = outbox.items(now);
        let current = self.state.load();
        let session_changed = current.session != session;
        let items_changed = current.outbox != items;
        if !session_changed && !items_changed {
            return;
        }
        self.update(|state| {
            state.session = session.clone();
            state.outbox = items.clone();
        });
        if session_changed {
            self.emit(AutomationEvent::Session(Box::new(session)));
        }
        if items_changed {
            self.emit(AutomationEvent::Outbox(items));
        }
    }

    pub(super) fn publish_automation(&self, snapshot: AutomationSnapshot) {
        if self.state.load().find(&snapshot.name) == Some(&snapshot) {
            return;
        }
        self.update(|state| {
            let automations = &mut state.automations;
            match automations.binary_search_by(|listed| listed.name.cmp(&snapshot.name)) {
                Ok(index) => automations[index] = snapshot.clone(),
                Err(index) => automations.insert(index, snapshot.clone()),
            }
        });
        self.emit(AutomationEvent::Automation(Box::new(snapshot)));
    }

    /// Forgets catalog names that are gone.
    pub(super) fn retain_automations(&self, keep: impl Fn(&str) -> bool) {
        if self
            .state
            .load()
            .automations
            .iter()
            .all(|listed| keep(&listed.name))
        {
            return;
        }
        self.update(|state| state.automations.retain(|listed| keep(&listed.name)));
    }

    /// Records a firing's newest summary; a row it `absorbed` leaves the list.
    pub(super) fn publish_firing(&self, firing: FiringSummary, absorbed: Option<String>) {
        self.update(|state| {
            let recent = &mut state.recent;
            if let Some(absorbed) = &absorbed {
                recent.retain(|row| row.fire_id != *absorbed);
            }
            match recent.iter_mut().find(|row| row.fire_id == firing.fire_id) {
                Some(row) => *row = firing.clone(),
                None => recent.insert(0, firing.clone()),
            }
            recent.truncate(MAX_RECENT_FIRINGS);
        });
        self.emit(AutomationEvent::Firing {
            firing: Box::new(firing),
            absorbed,
        });
    }

    pub(super) fn replace_recent(&self, recent: Vec<FiringSummary>) {
        self.update(|state| state.recent = recent.clone());
    }

    /// Journals how queued deliveries ended without reaching the model.
    pub(super) async fn end_deliveries(
        &self,
        items: &[OutboxItem],
        status: ActionStatus,
        error: Option<&str>,
    ) {
        for item in items {
            let end = AutomationActionEnd {
                status: status.to_row(),
                result: None,
                error: error.map(str::to_owned),
                target: None,
            };
            if let Err(error) = self
                .store
                .finish_action(item.fire_id.clone(), item.seq, end)
                .await
            {
                warn!(
                    session = %self.session_id,
                    automation = item.automation,
                    fire_id = item.fire_id,
                    seq = item.seq,
                    %status,
                    %error,
                    "automation delivery not journaled"
                );
            }
        }
    }

    /// Answers [`AutomationHandle::claim`] on the actor. A `guide` claim comes from a running
    /// turn, which cannot see what holds the session, so it leaves the observed gate alone.
    pub(super) async fn claim(
        &self,
        gate: DeliveryGate,
    ) -> Result<Option<OutboxClaim>, AutomationError> {
        let mut outbox = self.outbox.lock().await;
        let expired = outbox.take_expired(gate.now);
        self.end_deliveries(&expired, ActionStatus::Expired, None)
            .await;
        if gate.mode != DeliveryMode::Guide {
            outbox.observe(&gate);
        }
        let claimed = loop {
            let Some((fire_id, seq)) = outbox.claimable(&gate) else {
                break Ok(None);
            };
            match self.store.mark_delivered(fire_id.clone(), seq).await {
                Ok(true) => break Ok(outbox.claim(&fire_id, seq, &gate)),
                Ok(false) => {
                    outbox.remove(&fire_id, seq);
                }
                Err(error) => break Err(error),
            }
        };
        self.publish_outbox(&outbox, gate.now);
        claimed
    }
}

impl Inbox {
    pub(super) fn new(shared: Arc<Shared>, commands: flume::Receiver<Command>) -> Self {
        Self { shared, commands }
    }

    pub(super) async fn recv(&self) -> Result<Command, RecvError> {
        self.commands.recv_async().await
    }
}

impl Drop for Inbox {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::SeqCst);
        drop(self.commands.drain());
    }
}

impl AutomationHandle {
    pub(super) fn new(shared: Arc<Shared>) -> Self {
        Self(shared)
    }

    pub(super) fn shared(&self) -> &Arc<Shared> {
        &self.0
    }

    /// The runtime of a session open in this process.
    pub fn lookup(session_id: CaudraId) -> Option<Self> {
        registry::lookup(session_id).map(Self)
    }

    pub fn session_id(&self) -> CaudraId {
        self.0.session_id
    }

    pub async fn request(
        &self,
        request: AutomationRequest,
    ) -> Result<AutomationResponse, AutomationError> {
        self.ask(|reply| Command::Request(request, reply)).await
    }

    /// Never waits; a runtime that stopped ignores it.
    pub fn signal(&self, signal: SessionSignal) {
        let _ = self.0.commands.send(Command::Signal(signal));
    }

    /// The mirror, already holding every change its events announce.
    pub fn state(&self) -> Arc<AutomationState> {
        self.0.state()
    }

    pub fn events(&self) -> flume::Receiver<AutomationEvent> {
        self.0.receiver.clone()
    }

    /// The runtime's clock, in unix milliseconds.
    pub fn now_ms(&self) -> i64 {
        self.0.clock.now_ms()
    }

    /// Where the session's scripts live, resolved at spawn.
    pub fn directories(&self) -> &AutomationDirs {
        &self.0.directories
    }

    /// Messages from now on through `session`, the session's peers, or through none. Attaching
    /// releases the consumed messages of ended firings that waited for a session.
    pub fn attach_messaging(&self, session: Option<PeerSession>) {
        self.attach(session.map(|session| Arc::new(session) as Arc<dyn Messaging>));
    }

    /// [`Self::attach_messaging`] through any [`Messaging`]. The observers know of the session
    /// before this returns, so one installed next may take what its replay offers.
    pub fn attach(&self, messaging: Option<Arc<dyn Messaging>>) {
        let attached = messaging.is_some();
        self.0.watching.rcu(|watching| Watching {
            attached,
            paused: watching.paused,
            armed: watching.armed.clone(),
        });
        let _ = self.0.commands.send(Command::Messaging(messaging));
    }

    /// For [`PeerSession::set_observer`]: turns the messages the session receives into
    /// `message_received` events, and lets a consuming trigger take one from the model.
    pub fn message_observer(&self) -> Arc<dyn MessageObserver> {
        Arc::new(Observer {
            watching: Arc::clone(&self.0.watching),
            commands: self.0.commands.clone(),
        })
    }

    /// Takes the next `guide` item for a running turn's next model request, never a `next` one.
    /// The gate cannot see a queued human prompt here, so the caller holds the claim back while
    /// one waits.
    pub async fn claim_guidance(&self) -> Result<Option<OutboxClaim>, AutomationError> {
        self.claim(DeliveryGate {
            mode: DeliveryMode::Guide,
            settled: false,
            prompt_queued: false,
            modal_open: false,
            peers_first: false,
            now: self.now_ms(),
        })
        .await
    }

    /// Takes the next delivery `gate` allows, after the latch, the turn rate, the unattended cap
    /// and the backoff, and records it as delivered before answering, so a crash never repeats
    /// it. Expired items leave on the way. The runtime takes it after every signal sent before
    /// it; one that is stopping or stopped answers `Unavailable` and keeps its items.
    pub async fn claim(&self, gate: DeliveryGate) -> Result<Option<OutboxClaim>, AutomationError> {
        self.ask(|reply| Command::Claim { gate, reply }).await
    }

    /// Sends the command `wrap` builds around a reply and waits for the answer: `Unavailable`
    /// once the actor stopped, or when it stops before answering.
    async fn ask<T>(&self, wrap: impl FnOnce(Reply<T>) -> Command) -> Result<T, AutomationError> {
        let (reply, answer) = flume::bounded(1);
        self.0
            .commands
            .send(wrap(reply))
            .map_err(|_| AutomationError::Unavailable)?;
        if self.0.closed.load(Ordering::SeqCst) {
            return Err(AutomationError::Unavailable);
        }
        answer
            .recv_async()
            .await
            .map_err(|_| AutomationError::Unavailable)?
    }
}
