//! A session's automation runtime. One actor arms scripts, routes session signals to their
//! triggers, keeps each automation's queue, starts firings on their own threads, answers their
//! host calls, and journals everything as it happens. It publishes the mirror before each event,
//! owns the pause latch and the delivery counters, and answers outbox claims in order with the
//! signals sent before them. An `http()` request, a message, a workflow start and the release of
//! a consumed message run off the actor, which serves on until they end. A firing's row holds the
//! message it consumed until it keeps it or a release succeeds, so no crash loses one. The runs
//! of the session that settle arrive from a forwarder as `workflow_finished` events.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::mem;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use caudra_automation::args;
use caudra_automation::catalog::{CatalogEntry, InvalidEntry, Trust};
use caudra_automation::engine::{
    DEFAULT_WALL_TIME, ErrorKind, FiringEnd, FiringOutcome, HOST_FUNCTIONS, StopKind,
};
use caudra_automation::event::{
    ArmedReason, Event, EventDetail, IdleDetail, InputKind, MessageDetail, SessionView,
};
use caudra_automation::host::{
    ActionReply, ActionRequest, CallSite, DeliveryMode, Failure, FailureKind, GoalSet, HostError,
    HostResult, HttpRequest, Interruption, WorkflowRequest, request_hash,
};
use caudra_automation::limits::{ActingMarks, LimitRefusal};
use caudra_automation::matcher::{TopicMatcher, first_match};
use caudra_automation::meta::{ArmMode, Trigger, TriggerKind, references};
use caudra_automation::request::{
    AutomationError, AutomationRequest, AutomationResponse, DropTarget, ProfileArming,
    SessionSignal,
};
use caudra_automation::schedule::{Decision, decide, resolve_timezone};
use caudra_automation::snapshot::{
    ActionStatus, ArmOrigin, AutomationEvent, AutomationSnapshot, AutomationStatus, Availability,
    BindingView, Capabilities, FiringStatus, FiringSummary, PauseLatch, PauseSource, SettleBlocker,
    TriggerView,
};
use caudra_automation::state::check_state;
use caudra_automation::validate::{ValidationReport, validate};
use caudra_config::{AutomationsConfig, Feature, FeatureFlags};
use caudra_storage::StateDir;
use caudra_storage::automation::{
    AutomationActionEnd, AutomationEventSeen, AutomationFiringEnd, AutomationRelease,
    FiringError as StoredFiringError, MAX_HISTORY_SESSIONS, NewAutomationAction,
    NewAutomationFiring, StateCommit,
};
use caudra_storage::id::CaudraId;
use caudra_storage::paths::config_dir;
use caudra_storage::sessions::StoredAutomationControls;
use caudra_storage::topics::pattern_matches;
use futures_lite::future;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use smol::Task;
use tracing::{debug, warn};

use super::catalog::{AutomationDirs, Catalog, CatalogError, Frontend, ResolvedAutomation};
use super::clock::{Clock, millis};
use super::dry_run::{DryRunJob, DryRuns, replayed_event, replayed_trigger};
use super::handle::{AutomationHandle, Command, Inbox, Shared};
#[cfg(test)]
use super::host::Hold;
use super::host::{FiringHost, FiringJob, Stop, not_in_this_build, shut_down, spawn_firing};
use super::http::{HttpClient, Prepared, perform, prepare};
use super::messaging::{
    Matched, Messaging, NO_MESSAGING, Observed, Outgoing, ReleaseError, Watching,
};
use super::outbox::{Outbox, Pending, Pushed};
use super::registry;
use super::restore::{self, Restored};
use super::store::{
    AutomationStore, BindingMarks, FiringScope, Mirrored, PendingFiring, QueuedDelivery,
    session_controls,
};
use super::work_finished::{self, WorkRoute, WorkWatch};
use super::workflows::{
    NO_WORKFLOWS, Settled, Workflows, forward, scratch_root, settled_again, start_failure,
};
use crate::peers::MessageKey;

/// Firings that run at once in one session.
pub const MAX_RUNNING_FIRINGS: usize = 4;
/// Events one automation holds while it waits; a new one past this drops the oldest.
pub const MAX_QUEUED_EVENTS: usize = 16;
/// Schedules read the wall clock at least this often, so a suspend or a clock change shows.
const SCHEDULE_POLL: Duration = Duration::from_secs(60);
const MILLIS_PER_SECOND: i64 = 1_000;
const DEFAULT_HISTORY_FIRINGS: usize = 20;
const MAX_HISTORY_FIRINGS: usize = 50;
const DETAIL_FIRINGS: usize = 20;
const END_FIELD: &str = "end";
pub const REPLACED: &str = "a newer event of the same kind replaced it";
pub const QUEUE_FULL: &str = "its automation's queue was full";
pub const DISARMED: &str = "its automation was disarmed";
pub const DROPPED_BY_HUMAN: &str = "dropped from the inspector";
pub const OUTBOX_FULL: &str = "the outbox was full";
pub const GOAL_ACTIVE: &str =
    "a goal is active or already queued; pass replace: true to replace it";
pub const CHANGED_SINCE_ARMED: &str =
    "the file changed after it was armed; arm it again to run the new version";
pub const PAUSED_BY_USER: &str = "paused by the user";
pub const PAUSED_BY_SDK: &str = "paused by the SDK client";
pub const PAUSED_FROM_INSPECTOR: &str = "paused from the inspector";
pub const PAUSED_BY_SCRIPT: &str = "paused by automation";
const UNJOURNALED: &str = "the request could not be journaled";
const NO_THREAD: &str = "the firing's thread could not start";
const NOT_A_SESSION: &str = "is not a session id";

pub struct RuntimeDeps {
    pub state_dir: StateDir,
    pub session_id: CaudraId,
    pub cwd: PathBuf,
    /// The user's config directory, whose `automations/` scope the catalog scans. `None` uses
    /// the real one; tests point at a tempdir.
    pub user_config_dir: Option<PathBuf>,
    /// The project lives on another machine, so only the user's own scripts apply.
    pub remote: bool,
    pub features: FeatureFlags,
    /// Refuses the scripts this frontend cannot serve.
    pub frontend: Frontend,
    pub config: AutomationsConfig,
    /// What `SessionMeta.automations` kept when the session was last saved.
    pub controls: Option<StoredAutomationControls>,
    /// The CLI's `--automation` and the profile's `automations:`.
    pub launch: Vec<LaunchArming>,
    pub facts: SessionView,
    pub clock: Arc<dyn Clock>,
    /// Performs `http()` requests. Without one, `http()` stops the firing as unavailable in this
    /// build.
    pub http: Option<Arc<dyn HttpClient>>,
    /// The session's workflow runtime, which `start_workflow()` starts runs through and whose
    /// settled runs fire `workflow_finished`. Without one, `start_workflow()` fails as
    /// unavailable and nothing fires.
    pub workflows: Option<Arc<dyn Workflows>>,
}

/// One entry of the launch arming list, with where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchArming {
    pub arming: ProfileArming,
    pub origin: ArmOrigin,
}

pub struct AutomationRuntime {
    handle: AutomationHandle,
    task: Task<()>,
}

/// Topic patterns in the messaging grammar.
pub(super) struct Topics;

/// A runtime whose store is open and whose saved work is loaded, before it arms anything.
struct Boot {
    restored: Restored,
    launch: Vec<LaunchArming>,
    inbox: Inbox,
}

/// When a schedule trigger was armed and last acted on, in unix seconds. The binding keeps
/// them, so a restart neither fires an occurrence twice nor loses one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ScheduleMark {
    anchor: i64,
    last: Option<i64>,
}

/// By trigger index.
type Schedules = BTreeMap<usize, ScheduleMark>;

struct Armed {
    entry: Arc<CatalogEntry>,
    source: Arc<str>,
    args: Arc<Map<String, Value>>,
    origin: ArmOrigin,
    limiter: ActingMarks,
    schedules: Schedules,
    timezone: TimeZone,
    queue: VecDeque<Queued>,
    timers: Vec<AfterTimer>,
}

/// A recorded event waiting for its automation.
struct Queued {
    summary: FiringSummary,
    event: Event,
    /// Across every automation, so the oldest waiting event starts first.
    order: u64,
    /// The sequence number its next attempt journals actions from.
    next_seq: u64,
}

struct Running {
    queued: Queued,
    /// The state revision the firing loaded, which its commit must still find.
    revision: u64,
    /// The script version the firing runs, whose header its requests are checked against.
    entry: Arc<CatalogEntry>,
    stop: Arc<Stop>,
    /// When the firing started on the monotonic clock, which its wall time counts from.
    started: Duration,
    /// The `http()` request the firing waits on. Dropping it cancels the request.
    flight: Option<Task<()>>,
    /// One past the highest sequence number the firing journaled.
    journaled: u64,
}

/// How an action left the actor: answered, or handed to the HTTP client, the session's peers or
/// its workflow runtime with the sequence number it journals its end under.
enum Acted {
    Replied(ActionReply),
    Sending {
        seq: u64,
        prepared: Box<Prepared>,
    },
    Messaging {
        seq: u64,
        outgoing: Box<Outgoing>,
    },
    Starting {
        seq: u64,
        request: Box<WorkflowRequest>,
    },
}

/// How a request left the actor: answered, or as a dry run that answers off it.
enum Answered {
    Now(AutomationResponse),
    Later(Box<DryRunJob>),
}

/// The end of a firing a stop released while its workflow start was in flight, journaled once
/// the start's action ends, because ending the firing first would interrupt that action.
struct Finishing {
    automation: String,
    end: AutomationFiringEnd,
}

/// The pending `after` delay of an `idle` or `needs_input` trigger. It runs on the monotonic
/// clock; `until` is its wall-clock end as it started, for the mirror.
struct AfterTimer {
    trigger_index: usize,
    deadline: Duration,
    until: i64,
    cause: After,
}

enum After {
    Idle(Box<IdleDetail>),
    Input,
}

/// The session waits on the human: a `needs_input` trigger fires once per wait, which the input
/// and its tool name.
struct InputWait {
    input: InputKind,
    tool: Option<String>,
    since: i64,
    fired: HashSet<(String, usize)>,
}

/// An arming that waits for the session's record.
struct Parked {
    name: String,
    args: Option<Value>,
    origin: ArmOrigin,
    reason: ArmedReason,
}

/// How a firing's end is journaled:
/// - completed, skipped and released firings commit their state;
/// - failed and stopped ones are `failed`, with the error's kind;
/// - a limit defers a one-shot event, which keeps its queue waiting, and records a recurring
///   one as `rate_limited`;
/// - a pause or a disarm cancels the firing, and a shutdown interrupts it.
///
/// Events that arrive while the pause latch holds never run: they are recorded as `paused`.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Disposition {
    Finish(FiringStatus),
    Defer(i64),
}

struct Manager {
    shared: Arc<Shared>,
    state_dir: StateDir,
    cwd: PathBuf,
    user_config_dir: Option<PathBuf>,
    remote: bool,
    features: FeatureFlags,
    frontend: Frontend,
    catalog: Catalog,
    /// The host functions each script version calls, by digest.
    functions: HashMap<String, Vec<String>>,
    facts: SessionView,
    armed: BTreeMap<String, Armed>,
    /// The args of every binding of the session, armed or not.
    args: HashMap<String, Value>,
    last_firings: HashMap<String, FiringSummary>,
    running: HashMap<String, Running>,
    parked: Vec<Parked>,
    save_requested: bool,
    profile: Vec<ProfileArming>,
    input: Option<InputWait>,
    order: u64,
    stopping: Option<flume::Sender<()>>,
    http: Option<Arc<dyn HttpClient>>,
    allow_private_network: bool,
    messaging: Option<Arc<dyn Messaging>>,
    /// Counts the attaches, so a release the previous session refused goes to the next one.
    attachment: u64,
    /// The firings' consumed messages to release once a session is attached.
    releases: Vec<AutomationRelease>,
    /// Releases in flight. Shutdown waits for them, so none is handed back twice.
    releasing: usize,
    work: WorkWatch,
    workflows: Option<Arc<dyn Workflows>>,
    /// Turns the runs that settle into commands. Dropping it stops it.
    forwarder: Option<Task<()>>,
    /// The firings with a workflow start in flight, by fire id, and the end of each one a stop
    /// already released. Shutdown waits for them, so each start's action ends with its answer.
    starting: HashMap<String, Option<Finishing>>,
    /// The dry runs, which shutdown never waits for.
    dry_runs: DryRuns,
    #[cfg(test)]
    hold: Option<Hold>,
}

impl AutomationRuntime {
    /// Opens the store, applies the startup rules, arms the resumed bindings, the launch list
    /// and the `arm: "always"` scripts, queues what restore takes back, and starts serving.
    /// Refuses before touching the store or the user scope while the experiment is off.
    pub async fn spawn(deps: RuntimeDeps) -> Result<Self, AutomationError> {
        let (manager, boot) = Manager::open(deps).await?;
        Ok(manager.start(boot).await)
    }

    pub fn handle(&self) -> AutomationHandle {
        self.handle.clone()
    }

    /// Interrupts the running firings, lets their threads finish, closes the store, leaves the
    /// registry, and joins the runtime task.
    pub async fn shutdown(self) {
        let (ack, acked) = flume::bounded(1);
        if self
            .handle
            .shared()
            .commands
            .send(Command::Shutdown(ack))
            .is_ok()
        {
            let _ = acked.recv_async().await;
        }
        self.task.await;
    }
}

impl LaunchArming {
    /// `--automation NAME=…`, whose args replace the stored ones.
    fn replaces_args(&self) -> bool {
        self.origin == ArmOrigin::Cli && self.arming.args.is_some()
    }
}

impl TopicMatcher for Topics {
    fn matches(&self, pattern: &str, topic: &str) -> bool {
        pattern_matches(pattern, topic)
    }
}

impl ScheduleMark {
    fn anchored(anchor: i64) -> Self {
        Self { anchor, last: None }
    }
}

impl Armed {
    /// When schedule trigger `index` is next due, in unix seconds: `now_s` while an occurrence
    /// waits to be decided.
    fn schedule_due(&self, index: usize, now_s: i64) -> Option<i64> {
        let Some(Trigger::Schedule(schedule)) = self.entry.meta.triggers.get(index) else {
            return None;
        };
        let mark = self.schedules.get(&index)?;
        Some(
            match decide(schedule, &self.timezone, mark.anchor, mark.last, now_s) {
                Decision::Wait { next } => next,
                Decision::Due { .. } | Decision::Skip { .. } => now_s,
            },
        )
    }
}

impl Manager {
    async fn open(deps: RuntimeDeps) -> Result<(Self, Boot), AutomationError> {
        if !deps.features.enabled(Feature::Automations) {
            return Err(AutomationError::Unavailable);
        }
        let store = AutomationStore::spawn(deps.state_dir.clone(), deps.session_id)?;
        let restored = restore::load(&store).await?;
        let controls = deps
            .controls
            .map(|stored| session_controls(stored).0)
            .unwrap_or_default();
        let outbox = Outbox::new(controls, &deps.config, deps.clock.now_ms());
        let (commands, inbox) = flume::unbounded();
        let forwarder = deps.workflows.as_ref().map(|workflows| {
            forward(
                workflows.observe(),
                scratch_root(&deps.state_dir, deps.session_id),
                commands.clone(),
            )
        });
        let user_config_dir = deps.user_config_dir.or_else(|| config_dir().ok());
        let directories = AutomationDirs::resolve(
            (!deps.remote).then_some(deps.cwd.as_path()),
            user_config_dir.as_deref(),
        );
        let shared = Arc::new(Shared::new(
            deps.session_id,
            commands,
            store,
            outbox,
            deps.clock,
            directories,
        ));
        let manager = Manager {
            shared,
            state_dir: deps.state_dir,
            cwd: deps.cwd,
            user_config_dir,
            remote: deps.remote,
            features: deps.features,
            frontend: deps.frontend,
            catalog: Catalog::default(),
            functions: HashMap::new(),
            facts: deps.facts,
            armed: BTreeMap::new(),
            args: HashMap::new(),
            last_firings: HashMap::new(),
            running: HashMap::new(),
            parked: Vec::new(),
            save_requested: false,
            profile: deps
                .launch
                .iter()
                .filter(|launched| launched.origin == ArmOrigin::Profile)
                .map(|launched| launched.arming.clone())
                .collect(),
            input: None,
            order: 0,
            stopping: None,
            http: deps.http,
            allow_private_network: deps.config.allow_private_network,
            messaging: None,
            attachment: 0,
            releases: Vec::new(),
            releasing: 0,
            work: WorkWatch::default(),
            workflows: deps.workflows,
            forwarder,
            starting: HashMap::new(),
            dry_runs: DryRuns::new(),
            #[cfg(test)]
            hold: None,
        };
        let boot = Boot {
            restored,
            launch: deps.launch,
            inbox: Inbox::new(Arc::clone(&manager.shared), inbox),
        };
        Ok((manager, boot))
    }

    async fn start(mut self, boot: Boot) -> AutomationRuntime {
        self.rescan();
        self.restore(boot.restored, boot.launch).await;
        let shared = Arc::clone(&self.shared);
        registry::register(&shared);
        AutomationRuntime {
            handle: AutomationHandle::new(shared),
            task: smol::spawn(self.serve(boot.inbox)),
        }
    }

    /// Serves until shutdown. Due timers, schedules and expiries are handled before each
    /// command, so a request always sees the clock's present.
    async fn serve(mut self, inbox: Inbox) {
        let mut ticked_at = self.shared.clock.now_ms();
        loop {
            let received = match self.next_wake(ticked_at).await {
                Some(deadline) => {
                    let wake = self.shared.clock.sleep_until(deadline);
                    future::or(async { Some(inbox.recv().await) }, async {
                        wake.await;
                        None
                    })
                    .await
                }
                None => Some(inbox.recv().await),
            };
            ticked_at = self.shared.clock.now_ms();
            match received {
                None => self.tick().await,
                Some(Ok(command)) => {
                    self.tick().await;
                    self.handle(command).await;
                }
                Some(Err(_)) => break,
            }
            if self.stopping.is_some()
                && self.running.is_empty()
                && self.releasing == 0
                && self.starting.is_empty()
            {
                break;
            }
        }
        drop(inbox);
        self.close().await;
    }

    async fn handle(&mut self, command: Command) {
        match command {
            Command::Request(request, reply) => {
                let answered = if self.stopping.is_some() {
                    Err(AutomationError::Unavailable)
                } else {
                    self.request(request).await
                };
                self.pump().await;
                match answered {
                    Ok(Answered::Now(response)) => {
                        let _ = reply.send(Ok(response));
                    }
                    Ok(Answered::Later(job)) => job.spawn(&self.dry_runs, reply),
                    Err(error) => {
                        let _ = reply.send(Err(error));
                    }
                }
            }
            Command::Signal(signal) => {
                if self.stopping.is_none() {
                    self.signal(signal).await;
                    self.pump().await;
                }
            }
            Command::Claim { gate, reply } => {
                let claimed = if self.stopping.is_some() {
                    Err(AutomationError::Unavailable)
                } else {
                    self.shared.claim(gate).await
                };
                let _ = reply.send(claimed);
            }
            Command::Admit { fire_id, reply } => {
                let _ = reply.send(self.admit(&fire_id).await);
            }
            Command::Act {
                fire_id,
                site,
                request,
                reply,
            } => match self.act(&fire_id, site, request).await {
                Ok(Acted::Replied(answer)) => {
                    let _ = reply.send(Ok(answer));
                }
                Ok(Acted::Sending { seq, prepared }) => {
                    self.send_http(fire_id, seq, *prepared, reply);
                }
                Ok(Acted::Messaging { seq, outgoing }) => {
                    self.send_message(fire_id, seq, *outgoing, reply);
                }
                Ok(Acted::Starting { seq, request }) => {
                    self.start_workflow(fire_id, seq, *request, reply);
                }
                Err(error) => {
                    let _ = reply.send(Err(error));
                }
            },
            Command::ActionDone {
                fire_id,
                seq,
                end,
                answer,
                reply,
            } => {
                if let Some(running) = self.running.get_mut(&fire_id) {
                    running.flight = None;
                }
                self.end_action(&fire_id, seq, end).await;
                if let Some(reply) = reply {
                    let _ = reply.send(answer);
                }
                if let Some(Some(finishing)) = self.starting.remove(&fire_id) {
                    self.finish_row(&fire_id, finishing.end).await;
                    self.publish(&finishing.automation);
                }
            }
            Command::Finished { fire_id, outcome } => {
                self.finished(&fire_id, outcome).await;
                self.pump().await;
            }
            Command::WorkflowSettled(settled) => {
                if self.stopping.is_none() {
                    self.workflow_settled(*settled).await;
                    self.pump().await;
                }
            }
            Command::Observed(observed) if self.stopping.is_some() => {
                if observed
                    .matches
                    .iter()
                    .any(|matched| matched.taken.is_some())
                {
                    self.hand_back(&observed.key);
                }
            }
            Command::Observed(observed) => {
                self.observed(*observed).await;
                self.pump().await;
            }
            Command::Messaging(messaging) => self.attach(messaging),
            Command::Released {
                release,
                attachment,
                outcome,
            } => self.released(release, attachment, outcome).await,
            Command::WorkChecked {
                attachment,
                checked,
            } => {
                let current = attachment == self.attachment && self.stopping.is_none();
                match work_finished::apply(self, current, checked).await {
                    Ok(moved) => {
                        for (name, marks) in moved {
                            self.save_marks(&name, marks).await;
                        }
                    }
                    Err(error) => {
                        warn!(session = %self.shared.session_id, %error, "published work not checked");
                    }
                }
                self.pump().await;
            }
            Command::Shutdown(ack) => {
                for running in self.running.values() {
                    running.stop.set(Interruption::Shutdown);
                }
                self.stopping = Some(ack);
                self.watch();
            }
        }
    }

    async fn close(&mut self) {
        drop(self.forwarder.take());
        registry::unregister(&self.shared);
        self.shared.store.clone().shutdown().await;
        if let Some(ack) = self.stopping.take() {
            let _ = ack.send(());
        }
    }

    /// Arms each binding that was armed when the session closed with its stored args and origin,
    /// firing `armed` with reason `resume`, unless a command-line entry gives new args, which
    /// arm it anew with reason `launch`. The other launch entries and the `arm: "always"`
    /// scripts arm what has no binding yet, a profile's args seeding it ahead of a bare
    /// command-line entry. A binding the session disarmed stays disarmed unless the command line
    /// names it. Each automation arms at most once, so one that fails to resume is not tried
    /// again. One that fails to resume is disarmed, unless only this frontend refuses it: it
    /// stays armed for the frontend that serves it.
    async fn restore(&mut self, restored: Restored, mut launch: Vec<LaunchArming>) {
        let Restored {
            releases,
            bindings,
            pending,
            outbox,
            recent,
        } = restored;
        self.releases.extend(releases);
        for firing in recent.iter().rev() {
            self.last_firings
                .insert(firing.automation.clone(), firing.clone());
        }
        self.shared.replace_recent(recent);
        launch
            .sort_by_key(|launched| (!launched.replaces_args(), launched.origin == ArmOrigin::Cli));
        let mut armings = Vec::new();
        let mut handled = HashSet::new();
        let mut disarmed = HashSet::new();
        for record in bindings {
            let binding = record.binding;
            self.args.insert(binding.name.clone(), binding.args);
            if !binding.armed {
                disarmed.insert(binding.name);
                continue;
            }
            if launch
                .iter()
                .any(|launched| launched.arming.name == binding.name && launched.replaces_args())
            {
                continue;
            }
            handled.insert(binding.name.clone());
            match self
                .install(&binding.name, None, binding.origin, ArmedReason::Resume)
                .await
            {
                Ok(()) => armings.push((binding.name, ArmedReason::Resume)),
                Err(error) => {
                    self.refused(&binding.name, &error);
                    if self.catalog.unavailable_here(&binding.name) {
                        continue;
                    }
                    if let Err(error) = self.shared.store.disarm(binding.name.clone()).await {
                        warn!(
                            session = %self.shared.session_id,
                            automation = binding.name,
                            %error,
                            "automation binding not disarmed"
                        );
                    }
                }
            }
        }
        for LaunchArming { arming, origin } in launch {
            if (origin != ArmOrigin::Cli && disarmed.contains(&arming.name))
                || !handled.insert(arming.name.clone())
            {
                continue;
            }
            match self
                .install(&arming.name, arming.args, origin, ArmedReason::Launch)
                .await
            {
                Ok(()) => armings.push((arming.name, ArmedReason::Launch)),
                Err(error) => self.refused(&arming.name, &error),
            }
        }
        let always: Vec<String> = self
            .catalog
            .entries()
            .iter()
            .map(|resolved| &resolved.entry.meta)
            .filter(|meta| {
                meta.arm == ArmMode::Always
                    && !handled.contains(&meta.name)
                    && !disarmed.contains(&meta.name)
            })
            .map(|meta| meta.name.clone())
            .collect();
        for name in always {
            match self
                .install(&name, None, ArmOrigin::Always, ArmedReason::Launch)
                .await
            {
                Ok(()) => armings.push((name, ArmedReason::Launch)),
                Err(error) => self.refused(&name, &error),
            }
        }
        self.requeue(pending).await;
        self.return_outbox(outbox).await;
        for (name, reason) in armings {
            self.fire_armed(&name, reason).await;
        }
        self.tick().await;
        self.publish_all();
        self.watch();
    }

    /// Waiting one-shot events go back to their automations' queues in their original order;
    /// the rest are dropped with the reason. A `workflow_finished` event too large to keep is
    /// rebuilt from the run as the workflow runtime publishes it now.
    async fn requeue(&mut self, pending: Vec<PendingFiring>) {
        let runs = self.workflows.as_ref().map(|workflows| workflows.state());
        let scratch = scratch_root(&self.state_dir, self.shared.session_id);
        for waiting in pending {
            let triggers = self
                .armed
                .get(&waiting.firing.automation)
                .map(|armed| armed.entry.meta.triggers.as_slice());
            let rebuilt = |key: &str| {
                let detail = settled_again(&runs.as_ref()?.runs, key, &scratch)?;
                Some(Event {
                    fire_id: waiting.firing.fire_id.clone(),
                    at: waiting.firing.queued_at.div_euclid(MILLIS_PER_SECOND),
                    session: self.facts.clone(),
                    detail: EventDetail::WorkflowFinished(detail),
                })
            };
            match restore::requeue(&waiting, triggers, rebuilt) {
                Ok(event) => {
                    let order = self.next_order();
                    let next_seq = waiting.firing.action_count;
                    self.enqueue(Queued {
                        summary: waiting.firing,
                        event,
                        order,
                        next_seq,
                    })
                    .await;
                }
                Err(reason) => {
                    self.end_waiting(
                        waiting.firing,
                        FiringStatus::Dropped,
                        Some(reason.to_owned()),
                    )
                    .await;
                }
            }
        }
    }

    async fn return_outbox(&mut self, deliveries: Vec<QueuedDelivery>) {
        for delivery in deliveries {
            match restore::returned(&delivery) {
                Ok(pending) => self.deliver(pending).await,
                Err(reason) => {
                    self.shared
                        .end_deliveries(&[delivery.item], ActionStatus::Dropped, Some(reason))
                        .await;
                }
            }
        }
    }

    async fn request(&mut self, request: AutomationRequest) -> Result<Answered, AutomationError> {
        let store = self.shared.store.clone();
        let response = match request {
            AutomationRequest::DryRun { fire_id } => {
                return self
                    .dry_run(fire_id)
                    .await
                    .map(|job| Answered::Later(Box::new(job)));
            }
            AutomationRequest::List => {
                self.rescan();
                Ok(AutomationResponse::Automations(self.publish_all()))
            }
            AutomationRequest::Validate { name } => self.validate(name).await,
            AutomationRequest::Arm { name, args, origin } => {
                self.rescan();
                self.arm(&name, args, origin, armed_reason(origin))
                    .await
                    .map(automation_response)
            }
            AutomationRequest::Disarm { name } => self.disarm(&name).await.map(automation_response),
            AutomationRequest::Trust { name, digest } => self.trust(&name, &digest),
            AutomationRequest::Inspect { name, session_id } => {
                let session_id = session_id
                    .map(|id| {
                        id.parse::<CaudraId>().map_err(|_| {
                            AutomationError::Internal(format!("{id:?} {NOT_A_SESSION}"))
                        })
                    })
                    .transpose()?;
                store
                    .load_detail(session_id, name, DETAIL_FIRINGS)
                    .await
                    .map(|detail| AutomationResponse::Detail(Box::new(detail)))
            }
            AutomationRequest::Firing { fire_id } => self.firing(fire_id, FiringScope::Any).await,
            AutomationRequest::ActionBody { fire_id, seq } => store
                .load_action(fire_id.clone(), seq)
                .await?
                .map(|body| AutomationResponse::ActionBody(Box::new(body)))
                .ok_or(AutomationError::UnknownFiring { fire_id }),
            AutomationRequest::SetArgs { name, args } => {
                self.rescan();
                let origin = self
                    .armed
                    .get(&name)
                    .map_or(ArmOrigin::Manual, |armed| armed.origin);
                self.arm(&name, Some(args), origin, ArmedReason::Manual)
                    .await
                    .map(automation_response)
            }
            AutomationRequest::SetState {
                name,
                state,
                expected_revision,
            } => {
                check_state(&state).map_err(|error| AutomationError::Invalid {
                    name: name.clone(),
                    reason: error.to_string(),
                })?;
                store
                    .commit_state(name, expected_revision, state)
                    .await
                    .map(|revision| AutomationResponse::State { revision })
            }
            AutomationRequest::ClearState {
                name,
                expected_revision,
            } => store
                .commit_state(name, expected_revision, Value::Object(Map::new()))
                .await
                .map(|revision| AutomationResponse::State { revision }),
            AutomationRequest::Drop(DropTarget::Firing { fire_id }) => {
                self.drop_firing(fire_id).await
            }
            AutomationRequest::Drop(DropTarget::OutboxItem { fire_id, seq }) => {
                self.drop_item(fire_id, seq).await
            }
            AutomationRequest::Pause { by } => {
                let reason = pause_reason(&by);
                self.pause(by, reason, None).await;
                Ok(self.controls())
            }
            AutomationRequest::Resume => {
                self.resume().await;
                Ok(self.controls())
            }
            AutomationRequest::History {
                name,
                fire_id,
                limit,
            } => match fire_id {
                Some(fire_id) => self.firing(fire_id, FiringScope::Session).await,
                None => store
                    .load_firings(
                        name,
                        limit
                            .unwrap_or(DEFAULT_HISTORY_FIRINGS)
                            .min(MAX_HISTORY_FIRINGS),
                    )
                    .await
                    .map(AutomationResponse::Firings),
            },
            AutomationRequest::Sessions { limit } => store
                .load_history(limit.unwrap_or(MAX_HISTORY_SESSIONS))
                .await
                .map(AutomationResponse::Sessions),
        };
        response.map(Answered::Now)
    }

    async fn signal(&mut self, signal: SessionSignal) {
        match signal {
            SessionSignal::Facts(facts) => self.facts = *facts,
            SessionSignal::Saved => self.saved().await,
            SessionSignal::Settled(detail) => self.settled(*detail).await,
            SessionSignal::Busy { blockers } => self.busy(blockers).await,
            SessionSignal::NeedsInput { input, tool } => self.needs_input(input, tool).await,
            SessionSignal::InputResolved => self.input_resolved(),
            SessionSignal::GoalFinished(detail) => {
                self.broadcast(EventDetail::GoalFinished(*detail)).await;
            }
            SessionSignal::HumanInput => self.human_input().await,
            SessionSignal::Pause { by } => {
                let reason = pause_reason(&by);
                self.pause(by, reason, None).await;
            }
            SessionSignal::ProfileAutomations(profile) => self.profile_changed(profile).await,
            SessionSignal::RunEnded(outcome) => {
                let now = self.shared.clock.now_ms();
                let mut outbox = self.shared.outbox.lock().await;
                outbox.run_ended(outcome, now);
                self.shared.publish_outbox(&outbox, now);
            }
        }
    }

    /// Arms `name`, or arms it again with new args, and fires `armed` with `reason`.
    async fn arm(
        &mut self,
        name: &str,
        args: Option<Value>,
        origin: ArmOrigin,
        reason: ArmedReason,
    ) -> Result<AutomationSnapshot, AutomationError> {
        self.install(name, args, origin, reason).await?;
        self.fire_armed(name, reason).await;
        self.publish(name)
            .ok_or_else(|| AutomationError::UnknownAutomation {
                name: name.to_owned(),
            })
    }

    /// Binds `name` armed, with `args` or the stored ones checked against the script, and makes
    /// it ready to fire without firing it. While the session has no record yet, the arming
    /// waits for it and the frontend is asked once to save the session.
    async fn install(
        &mut self,
        name: &str,
        args: Option<Value>,
        origin: ArmOrigin,
        reason: ArmedReason,
    ) -> Result<(), AutomationError> {
        let ResolvedAutomation { entry, source } =
            self.catalog.resolve(name).map_err(catalog_error)?;
        if !entry.trust.is_trusted() {
            return Err(AutomationError::TrustRequired {
                name: name.to_owned(),
                digest: entry.digest,
                path: entry.path.display().to_string(),
            });
        }
        let (given, resolved_args) = self.args_for(name, args, &entry)?;
        let store = self.shared.store.clone();
        let binding = BindingView {
            name: name.to_owned(),
            scope: entry.scope,
            origin,
            armed: true,
            args: given.clone(),
            args_digest: Some(entry.digest.clone()),
        };
        match store.bind(binding).await {
            Err(AutomationError::SessionNotSaved { .. }) => {
                self.park(Parked {
                    name: name.to_owned(),
                    args: Some(given),
                    origin,
                    reason,
                });
                return Ok(());
            }
            bound => bound?,
        }
        store
            .insert_source(entry.digest.clone(), source.clone())
            .await?;
        let record = store.load_binding(name.to_owned()).await?;
        self.args.insert(name.to_owned(), given);
        self.functions
            .entry(entry.digest.clone())
            .or_insert_with(|| host_functions(&source));
        let mut marks = BindingMarks::default();
        let mut limiter = record
            .as_ref()
            .map(|record| record.limiter.clone())
            .unwrap_or_default();
        if reason != ArmedReason::Resume
            && (limiter.failure_streak > 0 || limiter.backoff_until.is_some())
        {
            limiter.record_completed();
            marks.limiter = Some(limiter.clone());
        }
        let work_cursor = record
            .as_ref()
            .filter(|_| reason == ArmedReason::Resume)
            .map(|record| record.work_cursor.clone());
        let (queue, timers, schedules) = match self.armed.remove(name) {
            Some(previous) => (previous.queue, previous.timers, previous.schedules),
            None if reason == ArmedReason::Resume => (
                VecDeque::new(),
                Vec::new(),
                record
                    .and_then(|record| serde_json::from_value(record.schedule).ok())
                    .unwrap_or_default(),
            ),
            None => {
                let now_s = self.shared.clock.now_ms().div_euclid(MILLIS_PER_SECOND);
                let schedules = fresh_schedules(&entry.meta.triggers, now_s);
                marks.schedule = Some(schedule_marks(&schedules));
                (VecDeque::new(), Vec::new(), schedules)
            }
        };
        if marks != BindingMarks::default() {
            self.save_marks(name, marks).await;
        }
        let timezone =
            resolve_timezone(entry.meta.timezone.as_deref()).unwrap_or_else(|_| TimeZone::system());
        let entry = Arc::new(entry);
        self.work.arm(name, &entry, work_cursor);
        self.armed.insert(
            name.to_owned(),
            Armed {
                entry,
                source: Arc::from(source),
                args: Arc::new(resolved_args),
                origin,
                limiter,
                schedules,
                timezone,
                queue,
                timers,
            },
        );
        self.watch();
        Ok(())
    }

    /// `args`, or else the binding's, or else none, checked against the script's header: as
    /// given, and resolved with the defaults.
    fn args_for(
        &self,
        name: &str,
        args: Option<Value>,
        entry: &CatalogEntry,
    ) -> Result<(Value, Map<String, Value>), AutomationError> {
        let given = args
            .or_else(|| self.args.get(name).cloned())
            .unwrap_or_else(|| Value::Object(Map::new()));
        let resolved =
            args::resolve(&entry.meta.args, &given).map_err(|error| AutomationError::Args {
                name: name.to_owned(),
                reason: error.to_string(),
            })?;
        Ok((given, resolved))
    }

    fn park(&mut self, parked: Parked) {
        self.parked.retain(|waiting| waiting.name != parked.name);
        self.parked.push(parked);
        if !self.save_requested {
            self.save_requested = true;
            self.shared.emit(AutomationEvent::SaveSession);
        }
    }

    /// The session's record exists now: the parked armings complete.
    async fn saved(&mut self) {
        self.save_requested = false;
        for parked in mem::take(&mut self.parked) {
            if let Err(error) = self
                .arm(&parked.name, parked.args, parked.origin, parked.reason)
                .await
            {
                self.refused(&parked.name, &error);
            }
        }
    }

    async fn disarm(&mut self, name: &str) -> Result<AutomationSnapshot, AutomationError> {
        self.parked.retain(|parked| parked.name != name);
        if let Some(armed) = self.armed.remove(name) {
            self.work.disarm(name);
            self.watch();
            for running in self
                .running
                .values()
                .filter(|running| running.queued.summary.automation == name)
            {
                running.stop.set(Interruption::Disarmed);
            }
            for queued in armed.queue {
                self.end_waiting(
                    queued.summary,
                    FiringStatus::Dropped,
                    Some(DISARMED.to_owned()),
                )
                .await;
            }
            let now = self.shared.clock.now_ms();
            let mut outbox = self.shared.outbox.lock().await;
            let items = outbox.take_automation(name);
            self.shared
                .end_deliveries(&items, ActionStatus::Dropped, Some(DISARMED))
                .await;
            self.shared.publish_outbox(&outbox, now);
            drop(outbox);
            self.shared.store.disarm(name.to_owned()).await?;
        }
        self.publish(name)
            .ok_or_else(|| AutomationError::UnknownAutomation {
                name: name.to_owned(),
            })
    }

    fn trust(&mut self, name: &str, digest: &str) -> Result<AutomationResponse, AutomationError> {
        self.rescan();
        self.catalog
            .trust(&self.state_dir, name, digest)
            .map_err(catalog_error)?;
        self.rescan();
        self.publish(name).map(automation_response).ok_or_else(|| {
            AutomationError::UnknownAutomation {
                name: name.to_owned(),
            }
        })
    }

    async fn validate(&mut self, name: String) -> Result<AutomationResponse, AutomationError> {
        self.rescan();
        let source = match self.catalog.resolve(&name) {
            Ok(resolved) => resolved.source,
            Err(CatalogError::Invalid { reason, .. }) => {
                return Ok(AutomationResponse::Validation {
                    name,
                    ok: false,
                    report: reason,
                });
            }
            Err(error) => return Err(catalog_error(error)),
        };
        let now = self.shared.clock.now_ms();
        let (ok, report) = match smol::unblock(move || validate(&source, now)).await {
            Ok(report) => (true, validation_report(&report)),
            Err(error) => (false, error.to_string()),
        };
        Ok(AutomationResponse::Validation { name, ok, report })
    }

    /// One firing's trace, its queued deliveries saying why they wait. Out of `scope`, a firing
    /// is unknown.
    async fn firing(
        &self,
        fire_id: String,
        scope: FiringScope,
    ) -> Result<AutomationResponse, AutomationError> {
        let mut detail = self
            .shared
            .store
            .load_firing(fire_id.clone(), scope)
            .await?
            .ok_or(AutomationError::UnknownFiring { fire_id })?;
        let state = self.shared.state();
        for action in &mut detail.actions {
            action.wait = state
                .outbox
                .iter()
                .find(|item| item.fire_id == detail.firing.fire_id && item.seq == action.seq)
                .and_then(|item| item.wait);
        }
        Ok(AutomationResponse::Firing(Box::new(detail)))
    }

    /// Gathers a dry run of one of this session's finished firings: the firing and its journal,
    /// the script on disk now, the args, a copy of the state, and what the automation's limits
    /// answer now. Nothing is written, and the run itself goes off the actor.
    async fn dry_run(&mut self, fire_id: String) -> Result<DryRunJob, AutomationError> {
        let store = self.shared.store.clone();
        let replayed = store
            .load_firing(fire_id.clone(), FiringScope::Session)
            .await?
            .ok_or_else(|| AutomationError::UnknownFiring {
                fire_id: fire_id.clone(),
            })?;
        let event = replayed_event(&replayed)?;
        let name = replayed.firing.automation.clone();
        self.rescan();
        let ResolvedAutomation { entry, source } =
            self.catalog.resolve(&name).map_err(catalog_error)?;
        let trigger_index = replayed_trigger(
            &fire_id,
            replayed.firing.trigger_index,
            &entry.meta.triggers,
            &event.detail,
        )?;
        let (_, args) = self.args_for(&name, None, &entry)?;
        let record = store.load_binding(name.clone()).await?;
        let admission = self
            .armed
            .get(&name)
            .map(|armed| &armed.limiter)
            .or(record.as_ref().map(|record| &record.limiter))
            .map_or(Ok(()), |marks| {
                marks.check(&entry.meta.limits, self.shared.clock.now_ms())
            });
        let (state, revision) = record.map_or_else(
            || (Value::Object(Map::new()), 0),
            |record| (record.state.value, record.state.revision),
        );
        Ok(DryRunJob {
            journal: store.load_journal(fire_id).await?,
            replayed,
            event,
            entry,
            source,
            trigger_index,
            args,
            state,
            revision,
            admission,
        })
    }

    async fn drop_firing(
        &mut self,
        fire_id: String,
    ) -> Result<AutomationResponse, AutomationError> {
        let dropped = self.armed.values_mut().find_map(|armed| {
            let position = armed
                .queue
                .iter()
                .position(|queued| queued.summary.fire_id == fire_id)?;
            armed.queue.remove(position)
        });
        let Some(queued) = dropped else {
            return Err(AutomationError::NotWaiting { fire_id, seq: None });
        };
        let name = queued.summary.automation.clone();
        self.end_waiting(
            queued.summary,
            FiringStatus::Dropped,
            Some(DROPPED_BY_HUMAN.to_owned()),
        )
        .await;
        self.publish(&name);
        Ok(AutomationResponse::Ack)
    }

    async fn drop_item(
        &mut self,
        fire_id: String,
        seq: u64,
    ) -> Result<AutomationResponse, AutomationError> {
        let now = self.shared.clock.now_ms();
        let mut outbox = self.shared.outbox.lock().await;
        let Some(item) = outbox.remove(&fire_id, seq) else {
            return Err(AutomationError::NotWaiting {
                fire_id,
                seq: Some(seq),
            });
        };
        self.shared
            .end_deliveries(&[item], ActionStatus::Dropped, Some(DROPPED_BY_HUMAN))
            .await;
        self.shared.publish_outbox(&outbox, now);
        Ok(AutomationResponse::Ack)
    }

    /// Sets the latch unless it holds already, cancels every running firing but `keep`, the
    /// one that called `pause_automations()`, and records the waiting events as `paused`.
    async fn pause(&mut self, source: PauseSource, reason: String, keep: Option<&str>) {
        let now = self.shared.clock.now_ms();
        let reason = {
            let mut outbox = self.shared.outbox.lock().await;
            let latch = outbox.controls.pause.get_or_insert(PauseLatch {
                reason,
                source,
                at: now,
            });
            let reason = latch.reason.clone();
            self.shared.publish_outbox(&outbox, now);
            reason
        };
        self.watch();
        for (fire_id, running) in &self.running {
            if keep != Some(fire_id.as_str()) {
                running.stop.set(Interruption::Paused);
            }
        }
        let waiting: Vec<Queued> = self
            .armed
            .values_mut()
            .flat_map(|armed| armed.queue.drain(..))
            .collect();
        for queued in waiting {
            self.end_waiting(queued.summary, FiringStatus::Paused, Some(reason.clone()))
                .await;
        }
        self.publish_armed();
    }

    async fn resume(&mut self) {
        let now = self.shared.clock.now_ms();
        let cleared = {
            let mut outbox = self.shared.outbox.lock().await;
            let cleared = outbox.controls.pause.take();
            self.shared.publish_outbox(&outbox, now);
            cleared
        };
        if cleared.is_some() {
            self.unpause().await;
        }
    }

    /// Clears the latch, the unattended count and the delivery backoff.
    async fn human_input(&mut self) {
        let now = self.shared.clock.now_ms();
        let cleared = {
            let mut outbox = self.shared.outbox.lock().await;
            let cleared = outbox.human_input();
            self.shared.publish_outbox(&outbox, now);
            cleared
        };
        if cleared.is_some() {
            self.unpause().await;
        }
    }

    async fn unpause(&mut self) {
        self.watch();
        let names: Vec<String> = self.armed.keys().cloned().collect();
        for name in names {
            self.fire_armed(&name, ArmedReason::Unpaused).await;
        }
        self.publish_armed();
    }

    /// Records the turns the busy period's deliveries led to, and starts the `idle` delays.
    async fn settled(&mut self, detail: IdleDetail) {
        let now = self.shared.clock.now_ms();
        let deliveries = {
            let mut outbox = self.shared.outbox.lock().await;
            let deliveries = outbox.settle(now);
            self.shared.publish_outbox(&outbox, now);
            deliveries
        };
        if !deliveries.is_empty()
            && let Err(error) = self
                .shared
                .store
                .record_turn(deliveries, detail.outcome, detail.cost)
                .await
        {
            warn!(session = %self.shared.session_id, %error, "automation turn not recorded");
        }
        let mono = self.shared.clock.monotonic();
        let mut immediate = Vec::new();
        for (name, armed) in &mut self.armed {
            for (index, trigger) in armed.entry.meta.triggers.iter().enumerate() {
                let Trigger::Idle { delay } = trigger else {
                    continue;
                };
                armed.timers.retain(|timer| timer.trigger_index != index);
                if delay.is_zero() {
                    immediate.push((name.clone(), index));
                } else {
                    armed.timers.push(AfterTimer {
                        trigger_index: index,
                        deadline: mono + *delay,
                        until: now.saturating_add(millis(*delay)),
                        cause: After::Idle(Box::new(detail.clone())),
                    });
                }
            }
        }
        for (name, index) in immediate {
            self.route(&name, index, EventDetail::Idle(detail.clone()))
                .await;
        }
        self.publish_armed();
    }

    async fn busy(&mut self, blockers: Vec<SettleBlocker>) {
        for armed in self.armed.values_mut() {
            armed
                .timers
                .retain(|timer| !matches!(timer.cause, After::Idle(_)));
        }
        let now = self.shared.clock.now_ms();
        {
            let mut outbox = self.shared.outbox.lock().await;
            outbox.busy(blockers);
            self.shared.publish_outbox(&outbox, now);
        }
        self.publish_armed();
    }

    /// A new wait starts the `needs_input` delays; the same wait again, the same input for the
    /// same tool, restarts the ones that have not fired, and never fires a trigger twice.
    async fn needs_input(&mut self, input: InputKind, tool: Option<String>) {
        let now = self.shared.clock.now_ms();
        let repeat = self
            .input
            .as_ref()
            .is_some_and(|wait| wait.input == input && wait.tool == tool);
        if !repeat {
            self.cancel_input_timers();
            self.input = Some(InputWait {
                input,
                tool,
                since: now,
                fired: HashSet::new(),
            });
        }
        let Some(wait) = &self.input else {
            return;
        };
        let mono = self.shared.clock.monotonic();
        let mut immediate = Vec::new();
        for (name, armed) in &mut self.armed {
            for (index, trigger) in armed.entry.meta.triggers.iter().enumerate() {
                let Trigger::NeedsInput { delay, inputs } = trigger else {
                    continue;
                };
                if !inputs.contains(&wait.input) || wait.fired.contains(&(name.clone(), index)) {
                    continue;
                }
                armed.timers.retain(|timer| timer.trigger_index != index);
                if delay.is_zero() {
                    immediate.push((name.clone(), index));
                } else {
                    armed.timers.push(AfterTimer {
                        trigger_index: index,
                        deadline: mono + *delay,
                        until: now.saturating_add(millis(*delay)),
                        cause: After::Input,
                    });
                }
            }
        }
        for (name, index) in immediate {
            self.fire_input(&name, index).await;
        }
        self.publish_armed();
    }

    fn input_resolved(&mut self) {
        self.input = None;
        self.cancel_input_timers();
        self.publish_armed();
    }

    fn cancel_input_timers(&mut self) {
        for armed in self.armed.values_mut() {
            armed
                .timers
                .retain(|timer| !matches!(timer.cause, After::Input));
        }
    }

    async fn fire_input(&mut self, name: &str, index: usize) {
        let now = self.shared.clock.now_ms();
        let Some(wait) = self.input.as_mut() else {
            return;
        };
        if !wait.fired.insert((name.to_owned(), index)) {
            return;
        }
        let detail = EventDetail::NeedsInput {
            input: wait.input,
            tool: wait.tool.clone(),
            waiting_s: u64::try_from(now.saturating_sub(wait.since) / MILLIS_PER_SECOND)
                .unwrap_or_default(),
        };
        self.route(name, index, detail).await;
    }

    /// Arms what the new list adds and disarms what it drops, among the profile's armings. A
    /// name both lists hold is left alone, so the same list again changes nothing, and an
    /// automation the session already has a binding for arms with its stored args.
    async fn profile_changed(&mut self, profile: Vec<ProfileArming>) {
        let previous = mem::replace(&mut self.profile, profile.clone());
        for gone in previous
            .iter()
            .filter(|old| !profile.iter().any(|new| new.name == old.name))
        {
            if self
                .armed
                .get(&gone.name)
                .is_some_and(|armed| armed.origin == ArmOrigin::Profile)
                && let Err(error) = self.disarm(&gone.name).await
            {
                warn!(session = %self.shared.session_id, automation = gone.name, %error, "profile automation not disarmed");
            }
        }
        self.rescan();
        for arming in profile {
            if previous.iter().any(|old| old.name == arming.name)
                || self.armed.contains_key(&arming.name)
            {
                continue;
            }
            let args = arming
                .args
                .filter(|_| !self.args.contains_key(&arming.name));
            if let Err(error) = self
                .arm(&arming.name, args, ArmOrigin::Profile, ArmedReason::Launch)
                .await
            {
                self.refused(&arming.name, &error);
            }
        }
    }

    async fn fire_armed(&mut self, name: &str, reason: ArmedReason) {
        let detail = EventDetail::Armed { reason };
        let Some(index) = self
            .armed
            .get(name)
            .and_then(|armed| first_match(&armed.entry.meta.triggers, &detail, &Topics))
        else {
            return;
        };
        self.route(name, index, detail).await;
    }

    /// Queues `detail` for every armed automation one of whose triggers matches it.
    async fn broadcast(&mut self, detail: EventDetail) {
        for (name, index) in self.matching(&detail) {
            self.route(&name, index, detail.clone()).await;
        }
    }

    /// The armed automations one of whose triggers matches `detail`, with the trigger's index.
    fn matching(&self, detail: &EventDetail) -> Vec<(String, usize)> {
        self.armed
            .iter()
            .filter_map(|(name, armed)| {
                first_match(&armed.entry.meta.triggers, detail, &Topics)
                    .map(|index| (name.clone(), index))
            })
            .collect()
    }

    /// Queues a run that settled for every automation it matches, once per run and execution
    /// epoch, whether the model, `/workflow` or an automation started it.
    async fn workflow_settled(&mut self, settled: Settled) {
        let Settled { key, detail } = settled;
        let detail = EventDetail::WorkflowFinished(detail);
        for (name, index) in self.matching(&detail) {
            match self
                .shared
                .store
                .event_seen(name.clone(), key.clone())
                .await
            {
                Ok(AutomationEventSeen::Unseen) => {}
                Ok(AutomationEventSeen::Holding | AutomationEventSeen::Handed) => continue,
                Err(error) => {
                    warn!(session = %self.shared.session_id, automation = name, event_key = key, %error, "automation workflow event not checked");
                    continue;
                }
            }
            if let Some(queued) = self
                .record(&name, index, detail.clone(), Some(key.clone()), None)
                .await
            {
                self.enqueue(queued).await;
            }
        }
    }

    /// Records an event for trigger `index` of `name` and queues it.
    async fn route(&mut self, name: &str, index: usize, detail: EventDetail) {
        if let Some(queued) = self.record(name, index, detail, None, None).await {
            self.enqueue(queued).await;
        }
    }

    /// Routes a message the session offered to each automation it matched, once per message and
    /// admission. A message taken again is never left taken: it is settled while a firing of its
    /// automation holds it, and handed back otherwise.
    async fn observed(&mut self, observed: Observed) {
        let Observed {
            key,
            event_key,
            detail,
            matches,
        } = observed;
        for Matched {
            automation: name,
            index,
            taken,
        } in matches
        {
            let seen = match self
                .shared
                .store
                .event_seen(name.clone(), event_key.clone())
                .await
            {
                Ok(seen) => seen,
                Err(error) => {
                    warn!(session = %self.shared.session_id, automation = name, %error, "automation message event not checked");
                    AutomationEventSeen::Handed
                }
            };
            match (seen, taken) {
                (AutomationEventSeen::Unseen, taken) => {
                    self.route_message(
                        &name,
                        index,
                        detail.clone(),
                        &key,
                        event_key.clone(),
                        taken,
                    )
                    .await;
                }
                (AutomationEventSeen::Holding, Some(_)) => {
                    if let Some(messaging) = &self.messaging {
                        messaging.settle(&key, &name);
                    }
                }
                (AutomationEventSeen::Handed, Some(_)) => self.hand_back(&key),
                (AutomationEventSeen::Holding | AutomationEventSeen::Handed, None) => {}
            }
        }
    }

    /// Records and queues a message event. Once the firing's row holds the message its
    /// automation took, the session lets go of it; when the session queued it for its model
    /// again instead, or none is attached, the firing gives it up. A taken message nothing
    /// records goes back at once.
    async fn route_message(
        &mut self,
        name: &str,
        index: usize,
        mut detail: MessageDetail,
        key: &MessageKey,
        event_key: String,
        delivery: Option<String>,
    ) {
        let consumed = delivery.is_some();
        detail.consumed = consumed;
        let recorded = self
            .record(
                name,
                index,
                EventDetail::MessageReceived(detail),
                Some(event_key),
                delivery,
            )
            .await;
        let Some(mut queued) = recorded else {
            if consumed {
                self.hand_back(key);
            }
            return;
        };
        if queued.summary.consumed
            && !self
                .messaging
                .as_ref()
                .is_some_and(|messaging| messaging.settle(key, name))
        {
            self.give_up(&mut queued).await;
        }
        self.enqueue(queued).await;
    }

    /// The session queued the message for its model again, so the firing no longer holds it.
    async fn give_up(&mut self, queued: &mut Queued) {
        if let EventDetail::MessageReceived(message) = &mut queued.event.detail {
            message.consumed = false;
        }
        queued.summary.consumed = false;
        let fire_id = queued.summary.fire_id.clone();
        let event = queued.event.to_tagged().to_string();
        if let Err(error) = self.shared.store.downgrade_firing(fire_id, event).await {
            warn!(session = %self.shared.session_id, fire_id = queued.summary.fire_id, %error, "automation firing still holds a message it gave up");
        }
    }

    /// Records an event for trigger `index` of `name`, with the key a message event is
    /// deduplicated by and the delivery of a message its automation took. `None` when it could
    /// not.
    async fn record(
        &mut self,
        name: &str,
        index: usize,
        detail: EventDetail,
        event_key: Option<String>,
        delivery: Option<String>,
    ) -> Option<Queued> {
        let armed = self.armed.get(name)?;
        let trigger = armed.entry.meta.triggers.get(index).map(Trigger::kind)?;
        let consumed = delivery.is_some();
        let now = self.shared.clock.now_ms();
        let fire_id = CaudraId::generate().to_string();
        let event = Event {
            fire_id: fire_id.clone(),
            at: now.div_euclid(MILLIS_PER_SECOND),
            session: self.facts.clone(),
            detail,
        };
        let summary = FiringSummary {
            fire_id: fire_id.clone(),
            automation: name.to_owned(),
            digest: armed.entry.digest.clone(),
            trigger,
            trigger_index: trigger_index(index),
            event_key: event_key.clone(),
            consumed,
            status: FiringStatus::Queued,
            reason: None,
            error: None,
            repeats: 1,
            attempts: 0,
            operations: 0,
            state_outcome: None,
            queued_at: now,
            deferred_until: None,
            started_at: None,
            finished_at: None,
            action_count: 0,
            first_action: None,
        };
        let firing = NewAutomationFiring {
            fire_id,
            session_id: self.shared.session_id,
            automation: name.to_owned(),
            digest: summary.digest.clone(),
            trigger: trigger.to_row(),
            trigger_index: summary.trigger_index,
            event: event.to_tagged().to_string(),
            event_key,
            consumed,
        };
        if let Err(error) = self.shared.store.insert_firing(firing, delivery).await {
            warn!(session = %self.shared.session_id, automation = name, %error, "automation event not recorded");
            return None;
        }
        Some(Queued {
            summary,
            event,
            order: self.next_order(),
            next_seq: 0,
        })
    }

    /// Puts a recorded event in its automation's queue, or records it as `paused` while the
    /// latch holds.
    async fn enqueue(&mut self, queued: Queued) {
        let name = queued.summary.automation.clone();
        if let Some(latch) = self.latch() {
            self.end_waiting(queued.summary, FiringStatus::Paused, Some(latch.reason))
                .await;
            self.publish(&name);
            return;
        }
        let Some(armed) = self.armed.get_mut(&name) else {
            return;
        };
        let summary = queued.summary.clone();
        let ousted = push_event(&mut armed.queue, queued);
        self.show_firing(summary, None);
        if let Some((ousted, reason)) = ousted {
            self.end_waiting(
                ousted.summary,
                FiringStatus::Dropped,
                Some(reason.to_owned()),
            )
            .await;
        }
        self.publish(&name);
    }

    /// Starts the oldest waiting events of idle automations while the session has room.
    async fn pump(&mut self) {
        if self.stopping.is_some() || self.latch().is_some() {
            return;
        }
        let now = self.shared.clock.now_ms();
        while self.running.len() < MAX_RUNNING_FIRINGS {
            let Some(name) = self.next_ready(now) else {
                break;
            };
            let Some(queued) = self
                .armed
                .get_mut(&name)
                .and_then(|armed| armed.queue.pop_front())
            else {
                break;
            };
            self.start_firing(queued).await;
        }
    }

    fn next_ready(&self, now: i64) -> Option<String> {
        self.armed
            .iter()
            .filter(|(name, _)| !self.is_running(name))
            .filter_map(|(name, armed)| {
                let head = armed.queue.front()?;
                head.summary
                    .deferred_until
                    .is_none_or(|until| until <= now)
                    .then_some((head.order, name))
            })
            .min_by_key(|(order, _)| *order)
            .map(|(_, name)| name.clone())
    }

    fn is_running(&self, name: &str) -> bool {
        self.running
            .values()
            .any(|running| running.queued.summary.automation == name)
    }

    async fn start_firing(&mut self, mut queued: Queued) {
        let name = queued.summary.automation.clone();
        let fire_id = queued.summary.fire_id.clone();
        let Some(armed) = self.armed.get(&name) else {
            return;
        };
        let (source, entry, args) = (
            Arc::clone(&armed.source),
            Arc::clone(&armed.entry),
            Arc::clone(&armed.args),
        );
        let store = self.shared.store.clone();
        if let Err(error) = store.start_firing(fire_id.clone()).await {
            warn!(session = %self.shared.session_id, automation = name, fire_id, %error, "automation firing not started");
            return;
        }
        let (state, revision) = match store.load_binding(name.clone()).await {
            Ok(Some(record)) => (record.state.value, record.state.revision),
            loaded => {
                if let Err(error) = loaded {
                    warn!(session = %self.shared.session_id, automation = name, %error, "automation state not loaded");
                }
                (Value::Object(Map::new()), 0)
            }
        };
        let stop = Arc::new(Stop::default());
        let host = FiringHost {
            fire_id: fire_id.clone(),
            commands: self.shared.commands.clone(),
            stop: Arc::clone(&stop),
            clock: Arc::clone(&self.shared.clock),
            #[cfg(test)]
            hold: self.hold.clone(),
        };
        let job = FiringJob {
            source,
            entry: Arc::clone(&entry),
            event: queued.event.clone(),
            state,
            args,
        };
        let started = self.shared.clock.monotonic();
        if let Err(error) = spawn_firing(job, host) {
            let end = AutomationFiringEnd {
                status: FiringStatus::Failed.to_row(),
                reason: None,
                error: Some(StoredFiringError {
                    kind: ErrorKind::Stop(StopKind::Internal).as_str().to_owned(),
                    message: format!("{NO_THREAD}: {error}"),
                    line: None,
                    column: None,
                }),
                operations: 0,
                state_patch: None,
                commit: None,
            };
            self.finish_row(&fire_id, end).await;
            return;
        }
        queued.summary.status = FiringStatus::Running;
        queued.summary.started_at = Some(self.shared.clock.now_ms());
        queued.summary.deferred_until = None;
        self.show_firing(queued.summary.clone(), None);
        let journaled = queued.next_seq;
        self.running.insert(
            fire_id,
            Running {
                queued,
                revision,
                entry,
                stop,
                started,
                flight: None,
                journaled,
            },
        );
        self.publish(&name);
    }

    /// `cooldown`, `max_per_hour` and the failure backoff, asked before a firing's first
    /// charging action; admitting it records it as acting.
    async fn admit(&mut self, fire_id: &str) -> Result<(), LimitRefusal> {
        let Some(name) = self
            .running
            .get(fire_id)
            .map(|running| running.queued.summary.automation.clone())
        else {
            return Ok(());
        };
        let now = self.shared.clock.now_ms();
        let Some(armed) = self.armed.get_mut(&name) else {
            return Ok(());
        };
        armed.limiter.check(&armed.entry.meta.limits, now)?;
        armed.limiter.record_acting(now);
        let limiter = armed.limiter.clone();
        self.save_marks(
            &name,
            BindingMarks {
                limiter: Some(limiter),
                ..BindingMarks::default()
            },
        )
        .await;
        Ok(())
    }

    /// Journals a request as it starts, performs it, and journals how it ended; deliveries stay
    /// `queued` until they are claimed, deduplicated, dropped or expire, and a checked `http()`
    /// request leaves to be sent off the actor.
    async fn act(
        &mut self,
        fire_id: &str,
        site: CallSite,
        request: ActionRequest,
    ) -> HostResult<Acted> {
        let Some(running) = self.running.get_mut(fire_id) else {
            return Err(shut_down());
        };
        if let Some(interruption) = running.stop.get() {
            return Err(HostError::Interrupted(interruption));
        }
        let name = running.queued.summary.automation.clone();
        let seq = running.queued.next_seq + u64::from(site.seq);
        running.journaled = running.journaled.max(seq + 1);
        let kind = request.kind();
        let journal = request.to_journal();
        let (delivery, expires_at) = delivery_terms(&request, self.shared.clock.now_ms());
        let status = if kind.delivers() {
            ActionStatus::Queued
        } else {
            ActionStatus::Running
        };
        let action = NewAutomationAction {
            fire_id: fire_id.to_owned(),
            seq,
            kind: kind.to_row(),
            line: site.line,
            column: site.column,
            request_hash: request_hash(&journal),
            request: journal.to_string(),
            status: status.to_row(),
            delivery: delivery.map(Mirrored::to_row),
            expires_ms: expires_at.map(|at| u64::try_from(at).unwrap_or_default()),
        };
        if let Err(error) = self.shared.store.start_action(action).await {
            warn!(session = %self.shared.session_id, automation = name, fire_id, seq, %error, "automation action not journaled");
            return Err(HostError::Refused(format!("{UNJOURNALED}: {error}")));
        }
        let performed = match &request {
            ActionRequest::Message(_) | ActionRequest::SetGoal(_) => {
                return self
                    .queue_delivery(&name, fire_id, seq, &request, &journal, expires_at)
                    .await
                    .map(Acted::Replied);
            }
            ActionRequest::Notify { text } => {
                self.shared.emit(AutomationEvent::Notice {
                    automation: name,
                    fire_id: Some(fire_id.to_owned()),
                    text: text.clone(),
                });
                Ok(ActionReply::Done)
            }
            ActionRequest::Pause { reason } => {
                self.pause(
                    PauseSource::Script { automation: name },
                    reason.clone(),
                    Some(fire_id),
                )
                .await;
                Ok(ActionReply::Done)
            }
            ActionRequest::Log { .. } => Ok(ActionReply::Done),
            ActionRequest::Http(http) if self.http.is_some() => {
                match self.check_http(fire_id, http) {
                    Ok(prepared) => {
                        return Ok(Acted::Sending {
                            seq,
                            prepared: Box::new(prepared),
                        });
                    }
                    Err(error) => Err(error),
                }
            }
            ActionRequest::Reply { .. }
            | ActionRequest::Send(_)
            | ActionRequest::Publish { .. }
            | ActionRequest::Broadcast { .. } => match self.check_message(fire_id, seq, &request) {
                Ok(outgoing) => {
                    return Ok(Acted::Messaging {
                        seq,
                        outgoing: Box::new(outgoing),
                    });
                }
                Err(error) => Err(error),
            },
            ActionRequest::StartWorkflow(workflow) if self.workflows.is_some() => {
                return Ok(Acted::Starting {
                    seq,
                    request: Box::new(workflow.clone()),
                });
            }
            ActionRequest::StartWorkflow(_) => {
                Err(Failure::new(FailureKind::Unavailable, NO_WORKFLOWS).into())
            }
            ActionRequest::Http(_) => Err(not_in_this_build(kind)),
        };
        let end = match &performed {
            Ok(_) => action_end(ActionStatus::Done, None),
            Err(error) => error_end(error),
        };
        self.end_action(fire_id, seq, end).await;
        performed.map(Acted::Replied)
    }

    /// Checks an `http()` request against the header of the script version that made it, with
    /// what its firing's wall time has left.
    fn check_http(&self, fire_id: &str, request: &HttpRequest) -> HostResult<Prepared> {
        let running = self.running.get(fire_id).ok_or_else(shut_down)?;
        let elapsed = self
            .shared
            .clock
            .monotonic()
            .saturating_sub(running.started);
        prepare(
            request,
            &running.entry.meta,
            self.allow_private_network,
            DEFAULT_WALL_TIME.saturating_sub(elapsed),
        )
    }

    /// Hands a checked request to the client and waits for it off the actor, which keeps serving.
    /// The request's end comes back as [`Command::ActionDone`] with `reply`; a pause, a disarm or a
    /// shutdown cancels it at once. Should the runtime be gone by then, dropping `reply` tells the
    /// firing it shut down.
    fn send_http(
        &mut self,
        fire_id: String,
        seq: u64,
        prepared: Prepared,
        reply: flume::Sender<HostResult<ActionReply>>,
    ) {
        let (Some(client), Some(running)) = (&self.http, self.running.get_mut(&fire_id)) else {
            return;
        };
        let Prepared {
            call,
            origin,
            variables,
            redactor,
        } = prepared;
        let (method, timeout) = (call.method, call.timeout);
        let clock = Arc::clone(&self.shared.clock);
        let sent_at = clock.monotonic();
        let deadline = clock.sleep_until(sent_at.saturating_add(timeout));
        let sending = client.send(call);
        let stop = Arc::clone(&running.stop);
        let commands = self.shared.commands.clone();
        let session_id = self.shared.session_id;
        running.flight = Some(smol::spawn(async move {
            let answer = perform(sending, &stop, deadline, timeout, &redactor).await;
            let end = match &answer {
                Ok(response) => AutomationActionEnd {
                    result: serde_json::to_string(response).ok(),
                    ..action_end(ActionStatus::Done, None)
                },
                Err(error) => error_end(error),
            };
            debug!(
                session = %session_id,
                fire_id,
                seq,
                ?method,
                origin,
                ?variables,
                status = answer.as_ref().ok().map(|response| response.status),
                error = answer.as_ref().err().map(error_label),
                elapsed_ms = millis(clock.monotonic().saturating_sub(sent_at)),
                "automation http request ended"
            );
            let _ = commands.send(Command::ActionDone {
                fire_id,
                seq,
                end: AutomationActionEnd {
                    target: Some(origin),
                    ..end
                },
                answer: answer.map(ActionReply::Http),
                reply: Some(reply),
            });
        }));
    }

    /// Admits a `reply`, `send`, `publish` or `broadcast`, whose header and `reply()` the engine
    /// already checked: the pause latch stops the firing, and without a session to send through
    /// it fails `unavailable`.
    fn check_message(
        &self,
        fire_id: &str,
        seq: u64,
        request: &ActionRequest,
    ) -> HostResult<Outgoing> {
        if self.latch().is_some() {
            return Err(HostError::Interrupted(Interruption::Paused));
        }
        if self.messaging.is_none() {
            return Err(Failure::new(FailureKind::Unavailable, NO_MESSAGING).into());
        }
        let running = self.running.get(fire_id).ok_or_else(shut_down)?;
        Outgoing::new(
            request,
            &running.queued.event,
            &running.queued.summary.automation,
            fire_id,
            seq,
        )
    }

    /// Sends an admitted message off the actor, as [`Self::send_http`] sends a request: its end
    /// comes back as [`Command::ActionDone`], and a pause, a disarm or a shutdown stops the
    /// firing's wait at once.
    fn send_message(
        &mut self,
        fire_id: String,
        seq: u64,
        outgoing: Outgoing,
        reply: flume::Sender<HostResult<ActionReply>>,
    ) {
        let (Some(messaging), Some(running)) = (&self.messaging, self.running.get_mut(&fire_id))
        else {
            return;
        };
        let target = outgoing.target();
        let sending = outgoing.send(messaging.as_ref());
        let stop = Arc::clone(&running.stop);
        let commands = self.shared.commands.clone();
        running.flight = Some(smol::spawn(async move {
            let answer = future::or(
                async { Err(HostError::Interrupted(stop.raised().await)) },
                sending,
            )
            .await;
            let end = match &answer {
                Ok(receipt) => AutomationActionEnd {
                    result: serde_json::to_string(receipt).ok(),
                    ..action_end(ActionStatus::Done, None)
                },
                Err(error) => error_end(error),
            };
            let _ = commands.send(Command::ActionDone {
                fire_id,
                seq,
                end: AutomationActionEnd { target, ..end },
                answer,
                reply: Some(reply),
            });
        }));
    }

    /// Starts a run off the actor, as [`Self::send_http`] sends a request. A pause, a disarm or a
    /// shutdown releases the firing at once but never cancels the start: its action ends once,
    /// with the runtime's answer, and the end of a firing released meanwhile waits for it.
    fn start_workflow(
        &mut self,
        fire_id: String,
        seq: u64,
        request: WorkflowRequest,
        reply: flume::Sender<HostResult<ActionReply>>,
    ) {
        let (Some(workflows), Some(running)) = (&self.workflows, self.running.get(&fire_id)) else {
            return;
        };
        let mut starting = workflows.start(request);
        let stop = Arc::clone(&running.stop);
        let commands = self.shared.commands.clone();
        self.starting.insert(fire_id.clone(), None);
        smol::spawn(async move {
            let raced = future::or(async { Err(stop.raised().await) }, async {
                Ok((&mut starting).await)
            })
            .await;
            let (started, reply) = match raced {
                Ok(started) => (started, Some(reply)),
                Err(interruption) => {
                    let _ = reply.send(Err(HostError::Interrupted(interruption)));
                    (starting.await, None)
                }
            };
            let (end, answer) = match started {
                Ok(started) => (
                    AutomationActionEnd {
                        result: serde_json::to_string(&started).ok(),
                        target: Some(started.run_id.clone()),
                        ..action_end(ActionStatus::Done, None)
                    },
                    Ok(ActionReply::WorkflowStarted(started)),
                ),
                Err(error) => {
                    let error = HostError::Failure(start_failure(&error));
                    (error_end(&error), Err(error))
                }
            };
            let _ = commands.send(Command::ActionDone {
                fire_id,
                seq,
                end,
                answer,
                reply,
            });
        })
        .detach();
    }

    /// Queues a `message` or `set_goal` item. `set_goal` fails with `goal_active` while a goal
    /// is active or queued, unless it replaces it, and answers the condition the item holds.
    async fn queue_delivery(
        &mut self,
        name: &str,
        fire_id: &str,
        seq: u64,
        request: &ActionRequest,
        journal: &Value,
        expires_at: Option<i64>,
    ) -> HostResult<ActionReply> {
        let goal_pending = self.shared.outbox.lock().await.goal_pending();
        if let ActionRequest::SetGoal(goal) = request
            && !goal.replace
            && (self.facts.goal.is_some() || goal_pending)
        {
            self.end_action(
                fire_id,
                seq,
                action_end(ActionStatus::Failed, Some(GOAL_ACTIVE.to_owned())),
            )
            .await;
            return Err(HostError::Failure(Failure::new(
                FailureKind::GoalActive,
                GOAL_ACTIVE,
            )));
        }
        let now = self.shared.clock.now_ms();
        let Some(pending) = Pending::from_journal(name, fire_id, seq, journal, now, expires_at)
        else {
            let error = HostError::Refused(UNJOURNALED.to_owned());
            self.end_action(
                fire_id,
                seq,
                action_end(ActionStatus::Refused, Some(error.to_string())),
            )
            .await;
            return Err(error);
        };
        let reply = match pending.goal() {
            Some(goal) => ActionReply::GoalSet(GoalSet {
                condition: goal.condition.clone(),
            }),
            None => ActionReply::Done,
        };
        self.deliver(pending).await;
        Ok(reply)
    }

    /// Adds an item to the outbox, journaling the item it deduplicated into or the oldest one
    /// it pushed out.
    async fn deliver(&mut self, pending: Pending) {
        let (fire_id, seq) = (pending.item.fire_id.clone(), pending.item.seq);
        let now = self.shared.clock.now_ms();
        let pushed = {
            let mut outbox = self.shared.outbox.lock().await;
            let pushed = outbox.push(pending);
            self.shared.publish_outbox(&outbox, now);
            pushed
        };
        match pushed {
            Pushed::Added { dropped: None } => {}
            Pushed::Added {
                dropped: Some(oldest),
            } => {
                self.shared
                    .end_deliveries(&[oldest], ActionStatus::Dropped, Some(OUTBOX_FULL))
                    .await;
            }
            Pushed::Deduplicated { into } => {
                let end = AutomationActionEnd {
                    target: Some(into),
                    ..action_end(ActionStatus::Deduplicated, None)
                };
                self.end_action(&fire_id, seq, end).await;
            }
        }
    }

    async fn finished(&mut self, fire_id: &str, outcome: FiringOutcome) {
        let Some(Running {
            queued,
            revision,
            journaled,
            ..
        }) = self.running.remove(fire_id)
        else {
            return;
        };
        let name = queued.summary.automation.clone();
        match disposition(&outcome.end, queued.summary.trigger) {
            Disposition::Defer(until) => self.defer(queued, journaled, until).await,
            Disposition::Finish(status) => {
                let commit = outcome.state.as_ref().filter(|_| outcome.end.commits());
                let end = AutomationFiringEnd {
                    status: status.to_row(),
                    reason: end_reason(&outcome.end),
                    error: end_error(&outcome.end),
                    operations: outcome.operations,
                    state_patch: commit.map(|change| change.patch.to_string()),
                    commit: commit.map(|change| StateCommit {
                        expected_revision: revision,
                        state: change.state.to_string(),
                    }),
                };
                match self.starting.get_mut(fire_id) {
                    Some(finishing) => {
                        *finishing = Some(Finishing {
                            automation: name.clone(),
                            end,
                        });
                    }
                    None => self.finish_row(fire_id, end).await,
                }
                self.record_outcome(&name, &outcome).await;
            }
        }
        self.publish(&name);
    }

    /// Puts a one-shot event a limit refused back at the head of its queue until `until`.
    async fn defer(&mut self, mut queued: Queued, journaled: u64, until: i64) {
        match self
            .shared
            .store
            .defer_firing(queued.summary.fire_id.clone(), until)
            .await
        {
            Ok(attempts) => queued.summary.attempts = attempts,
            Err(error) => {
                warn!(session = %self.shared.session_id, fire_id = queued.summary.fire_id, %error, "automation firing not deferred");
                return;
            }
        }
        queued.summary.status = FiringStatus::Deferred;
        queued.summary.deferred_until = Some(until);
        queued.next_seq = journaled;
        self.show_firing(queued.summary.clone(), None);
        match self.armed.get_mut(&queued.summary.automation) {
            Some(armed) => armed.queue.push_front(queued),
            None => {
                self.end_waiting(
                    queued.summary,
                    FiringStatus::Dropped,
                    Some(DISARMED.to_owned()),
                )
                .await;
            }
        }
    }

    /// A failure backs the automation's next acting firing off, whether or not it acted. Only a
    /// firing that acted and committed resets the backoff, as arming again does, so firings
    /// that return early never clear the failures between them.
    async fn record_outcome(&mut self, name: &str, outcome: &FiringOutcome) {
        let now = self.shared.clock.now_ms();
        let Some(armed) = self.armed.get_mut(name) else {
            return;
        };
        let limiter = &mut armed.limiter;
        match &outcome.end {
            FiringEnd::Failed(_) | FiringEnd::Stopped(_) => limiter.record_failed(now),
            end if outcome.charged
                && end.commits()
                && (limiter.failure_streak > 0 || limiter.backoff_until.is_some()) =>
            {
                limiter.record_completed();
            }
            _ => return,
        }
        let limiter = limiter.clone();
        self.save_marks(
            name,
            BindingMarks {
                limiter: Some(limiter),
                ..BindingMarks::default()
            },
        )
        .await;
    }

    async fn end_waiting(
        &mut self,
        summary: FiringSummary,
        status: FiringStatus,
        reason: Option<String>,
    ) {
        let end = AutomationFiringEnd {
            status: status.to_row(),
            reason,
            error: None,
            operations: 0,
            state_patch: None,
            commit: None,
        };
        self.finish_row(&summary.fire_id, end).await;
    }

    /// Records a firing's final status and shows the row storage kept, which may have absorbed
    /// the automation's previous quiet skip. An ending that does not keep the firing's consumed
    /// message hands it back.
    async fn finish_row(&mut self, fire_id: &str, end: AutomationFiringEnd) {
        let store = self.shared.store.clone();
        let finished = match store.finish_firing(fire_id.to_owned(), end).await {
            Ok(finished) => finished,
            Err(error) => {
                warn!(session = %self.shared.session_id, fire_id, %error, "automation firing not finished");
                return;
            }
        };
        if let Some(delivery) = finished.release {
            self.release(AutomationRelease {
                fire_id: fire_id.to_owned(),
                delivery,
            });
        }
        match store.load_summary(fire_id.to_owned()).await {
            Ok(Some(summary)) => self.show_firing(summary, finished.absorbed),
            Ok(None) => {}
            Err(error) => {
                warn!(session = %self.shared.session_id, fire_id, %error, "automation firing not reloaded");
            }
        }
    }

    fn show_firing(&mut self, summary: FiringSummary, absorbed: Option<String>) {
        self.last_firings
            .insert(summary.automation.clone(), summary.clone());
        self.shared.publish_firing(summary, absorbed);
    }

    async fn end_action(&self, fire_id: &str, seq: u64, end: AutomationActionEnd) {
        if let Err(error) = self
            .shared
            .store
            .finish_action(fire_id.to_owned(), seq, end)
            .await
        {
            warn!(session = %self.shared.session_id, fire_id, seq, %error, "automation action end not journaled");
        }
    }

    async fn save_marks(&self, name: &str, marks: BindingMarks) {
        if let Err(error) = self.shared.store.save_marks(name.to_owned(), marks).await {
            warn!(session = %self.shared.session_id, automation = name, %error, "automation marks not saved");
        }
    }

    /// Messages through `messaging` from now on, and releases what waited for a session.
    fn attach(&mut self, messaging: Option<Arc<dyn Messaging>>) {
        self.messaging = messaging;
        self.attachment += 1;
        self.release_waiting();
    }

    fn release_waiting(&mut self) {
        if self.messaging.is_some() {
            for release in mem::take(&mut self.releases) {
                self.release(release);
            }
        }
    }

    /// Undoes the take of a message no firing holds, which its session still has. With no
    /// session attached there is nothing to undo: detaching returned every message the session
    /// had not settled.
    fn hand_back(&self, key: &MessageKey) {
        if let Some(messaging) = &self.messaging {
            messaging.give_back(key);
        }
    }

    /// Hands a firing's consumed message back to the session off the actor, or keeps it until
    /// one is attached. While the runtime stops, the firing's row keeps it for the next start.
    fn release(&mut self, release: AutomationRelease) {
        if self.stopping.is_some() {
            return;
        }
        let Some(messaging) = &self.messaging else {
            self.releases.push(release);
            return;
        };
        let releasing = messaging.release(&release.delivery);
        let (commands, attachment) = (self.shared.commands.clone(), self.attachment);
        self.releasing += 1;
        smol::spawn(async move {
            let outcome = releasing.await;
            let _ = commands.send(Command::Released {
                release,
                attachment,
                outcome,
            });
        })
        .detach();
    }

    /// A release that succeeded, or that no release can ever read, frees the firing's row of
    /// the delivery. One the session refused waits for the next session, or goes at once to a
    /// session attached since.
    async fn released(
        &mut self,
        release: AutomationRelease,
        attachment: u64,
        outcome: Result<(), ReleaseError>,
    ) {
        self.releasing = self.releasing.saturating_sub(1);
        if let Err(error) = &outcome {
            warn!(session = %self.shared.session_id, fire_id = release.fire_id, %error, "consumed message not handed back");
        }
        match outcome {
            Err(ReleaseError::Refused(_)) => {
                self.releases.push(release);
                if attachment != self.attachment {
                    self.release_waiting();
                }
            }
            Ok(()) | Err(ReleaseError::Unreadable(_)) => {
                let fire_id = release.fire_id;
                if let Err(error) = self.shared.store.clear_delivery(fire_id.clone()).await {
                    warn!(session = %self.shared.session_id, fire_id, %error, "released message not forgotten");
                }
            }
        }
    }

    /// Publishes what the message observers match against, keeping whether a session is
    /// attached: nothing once the runtime stops.
    fn watch(&self) {
        let armed: Vec<(String, Arc<CatalogEntry>)> = self
            .armed
            .iter()
            .filter(|(_, armed)| {
                self.stopping.is_none()
                    && armed
                        .entry
                        .meta
                        .triggers
                        .iter()
                        .any(|trigger| matches!(trigger, Trigger::MessageReceived(_)))
            })
            .map(|(name, armed)| (name.clone(), Arc::clone(&armed.entry)))
            .collect();
        let paused = self.latch().is_some();
        self.shared.watching.rcu(|watching| Watching {
            attached: watching.attached,
            paused,
            armed: armed.clone(),
        });
    }

    /// Fires due `after` delays, decides schedules, checks the published work when it is due,
    /// expires deliveries, and starts what waits.
    async fn tick(&mut self) {
        if self.stopping.is_some() {
            return;
        }
        let mono = self.shared.clock.monotonic();
        let mut due = Vec::new();
        for (name, armed) in &mut self.armed {
            let (ready, waiting): (Vec<AfterTimer>, Vec<AfterTimer>) = mem::take(&mut armed.timers)
                .into_iter()
                .partition(|timer| timer.deadline <= mono);
            armed.timers = waiting;
            due.extend(ready.into_iter().map(|timer| (name.clone(), timer)));
        }
        for (name, timer) in due {
            match timer.cause {
                After::Idle(detail) => {
                    self.route(&name, timer.trigger_index, EventDetail::Idle(*detail))
                        .await;
                }
                After::Input => self.fire_input(&name, timer.trigger_index).await,
            }
            self.publish(&name);
        }
        self.check_schedules().await;
        if let Some(messaging) = &self.messaging {
            self.work
                .tick(messaging, mono, &self.shared.commands, self.attachment);
        }
        self.expire_outbox().await;
        self.pump().await;
    }

    /// Fires each schedule occurrence that came due, or skips a missed one as `catch_up` says,
    /// saving the marks before the event is queued.
    async fn check_schedules(&mut self) {
        let now_s = self.shared.clock.now_ms().div_euclid(MILLIS_PER_SECOND);
        let mut due = Vec::new();
        let mut moved = Vec::new();
        for (name, armed) in &mut self.armed {
            let mut changed = false;
            for (index, trigger) in armed.entry.meta.triggers.iter().enumerate() {
                let Trigger::Schedule(schedule) = trigger else {
                    continue;
                };
                let mark = armed
                    .schedules
                    .entry(index)
                    .or_insert_with(|| ScheduleMark::anchored(now_s));
                match decide(schedule, &armed.timezone, mark.anchor, mark.last, now_s) {
                    Decision::Due {
                        scheduled_for,
                        late_by_s,
                    } => {
                        mark.last = Some(scheduled_for);
                        changed = true;
                        due.push((
                            name.clone(),
                            index,
                            EventDetail::Schedule {
                                scheduled_for,
                                late_by_s,
                            },
                        ));
                    }
                    Decision::Skip { scheduled_for } => {
                        mark.last = Some(scheduled_for);
                        changed = true;
                    }
                    Decision::Wait { .. } => {}
                }
            }
            if changed {
                moved.push((name.clone(), schedule_marks(&armed.schedules)));
            }
        }
        for (name, schedule) in moved {
            let marks = BindingMarks {
                schedule: Some(schedule),
                ..BindingMarks::default()
            };
            self.save_marks(&name, marks).await;
        }
        for (name, index, detail) in due {
            self.route(&name, index, detail).await;
        }
    }

    /// Journals the deliveries past their expiry, and publishes the outbox with its wait
    /// reasons as of now.
    async fn expire_outbox(&self) {
        let now = self.shared.clock.now_ms();
        let mut outbox = self.shared.outbox.lock().await;
        let expired = outbox.take_expired(now);
        self.shared
            .end_deliveries(&expired, ActionStatus::Expired, None)
            .await;
        self.shared.publish_outbox(&outbox, now);
    }

    /// When something next changes by itself: an `after` delay, a deferral or a limit running
    /// out, an expiry, a check of the published work while a session is attached, or a schedule,
    /// which is also polled once a minute. Nothing does while the
    /// runtime stops. The monotonic clock is read first, so a clock that moves in between wakes
    /// the runtime early rather than late. A deferral, outbox limit or expiry that runs out after
    /// `ticked_at`, the wall clock read before the last tick, still wakes it, at once when it
    /// ran out in between: no tick saw it run out, and nothing else would act on it.
    async fn next_wake(&self, ticked_at: i64) -> Option<Duration> {
        if self.stopping.is_some() {
            return None;
        }
        let mono = self.shared.clock.monotonic();
        let now = self.shared.clock.now_ms();
        let now_s = now.div_euclid(MILLIS_PER_SECOND);
        let at = |wall: i64| {
            mono + Duration::from_millis(
                u64::try_from(wall.saturating_sub(now)).unwrap_or_default(),
            )
        };
        let mut wakes: Vec<Duration> = self
            .shared
            .outbox
            .lock()
            .await
            .next_change(ticked_at)
            .map(at)
            .into_iter()
            .collect();
        for armed in self.armed.values() {
            wakes.extend(armed.timers.iter().map(|timer| timer.deadline));
            wakes.extend(
                armed
                    .queue
                    .front()
                    .and_then(|head| head.summary.deferred_until)
                    .filter(|until| *until > ticked_at)
                    .map(at),
            );
            if !armed.schedules.is_empty() {
                wakes.push(mono + SCHEDULE_POLL);
                wakes.extend(
                    armed
                        .schedules
                        .keys()
                        .filter_map(|index| armed.schedule_due(*index, now_s))
                        .map(|due| at(due.saturating_mul(MILLIS_PER_SECOND))),
                );
            }
        }
        wakes.extend(self.work.next_check().filter(|_| self.messaging.is_some()));
        wakes.into_iter().min()
    }

    fn rescan(&mut self) {
        self.catalog = if self.remote {
            Catalog::scan_user_only(
                self.user_config_dir.as_deref(),
                self.features,
                self.frontend,
            )
        } else {
            Catalog::scan_with(
                &self.state_dir,
                &self.cwd,
                self.user_config_dir.as_deref(),
                self.features,
                self.frontend,
            )
        };
        for resolved in self.catalog.entries() {
            self.functions
                .entry(resolved.entry.digest.clone())
                .or_insert_with(|| host_functions(&resolved.source));
        }
    }

    /// Publishes every catalog name and armed automation, and forgets names that are gone.
    fn publish_all(&self) -> Vec<AutomationSnapshot> {
        let mut names: Vec<&str> = self
            .catalog
            .entries()
            .iter()
            .map(|resolved| resolved.entry.meta.name.as_str())
            .chain(
                self.catalog
                    .invalid()
                    .iter()
                    .map(|invalid| invalid.name.as_str()),
            )
            .chain(self.armed.keys().map(String::as_str))
            .collect();
        names.sort_unstable();
        names.dedup();
        self.shared
            .retain_automations(|name| names.binary_search(&name).is_ok());
        names
            .into_iter()
            .filter_map(|name| self.publish(name))
            .collect()
    }

    fn publish_armed(&self) {
        for name in self.armed.keys() {
            self.publish(name);
        }
    }

    fn publish(&self, name: &str) -> Option<AutomationSnapshot> {
        let Some(snapshot) = self.snapshot(name) else {
            self.shared.retain_automations(|listed| listed != name);
            return None;
        };
        self.shared.publish_automation(snapshot.clone());
        Some(snapshot)
    }

    fn snapshot(&self, name: &str) -> Option<AutomationSnapshot> {
        let armed = self.armed.get(name);
        let listed = self.catalog.find(name).map(|resolved| &resolved.entry);
        let Some(entry) = armed.map(|armed| armed.entry.as_ref()).or(listed) else {
            return self
                .catalog
                .invalid()
                .iter()
                .find(|invalid| invalid.name == name)
                .map(|invalid| self.invalid_snapshot(invalid));
        };
        let mut warnings = entry.warnings.clone();
        if listed.is_some_and(|listed| listed.digest != entry.digest) {
            warnings.push(CHANGED_SINCE_ARMED.to_owned());
        }
        let meta = &entry.meta;
        let availability = if armed.is_some() {
            Availability::Armed
        } else if entry.trust.is_trusted() {
            Availability::Available
        } else {
            Availability::NeedsTrust
        };
        Some(AutomationSnapshot {
            name: name.to_owned(),
            description: meta.description.clone(),
            scope: entry.scope,
            path: entry.path.clone(),
            digest: entry.digest.clone(),
            trust: entry.trust,
            availability,
            armed: armed.map(|armed| armed.origin),
            args: self.args.get(name).cloned(),
            declared_args: meta.args.clone(),
            status: self.status(name, armed),
            last_firing: self.last_firings.get(name).cloned(),
            triggers: self.trigger_views(entry, armed),
            limits: Some(meta.limits.clone()),
            limiter: armed.map(|armed| armed.limiter.clone()).unwrap_or_default(),
            capabilities: Capabilities {
                network: meta.network.clone(),
                secrets: meta.secrets.clone(),
                messaging: meta.messaging.clone(),
                workflows: meta.workflows.clone(),
            },
            host_functions: self
                .functions
                .get(&entry.digest)
                .cloned()
                .unwrap_or_default(),
            warnings,
            shadowed: entry.shadowed.clone(),
        })
    }

    fn invalid_snapshot(&self, invalid: &InvalidEntry) -> AutomationSnapshot {
        AutomationSnapshot {
            name: invalid.name.clone(),
            description: String::new(),
            scope: invalid.scope,
            path: invalid.path.clone(),
            digest: String::new(),
            trust: Trust::assess(invalid.scope, false),
            availability: Availability::Invalid {
                reason: invalid.reason.clone(),
            },
            armed: None,
            args: self.args.get(&invalid.name).cloned(),
            declared_args: Vec::new(),
            status: AutomationStatus::Idle,
            last_firing: self.last_firings.get(&invalid.name).cloned(),
            triggers: Vec::new(),
            limits: None,
            limiter: ActingMarks::default(),
            capabilities: Capabilities::default(),
            host_functions: Vec::new(),
            warnings: Vec::new(),
            shadowed: Vec::new(),
        }
    }

    fn status(&self, name: &str, armed: Option<&Armed>) -> AutomationStatus {
        if self.is_running(name) {
            return AutomationStatus::Running;
        }
        let Some(armed) = armed else {
            return AutomationStatus::Idle;
        };
        if self.latch().is_some() {
            return AutomationStatus::Paused;
        }
        if let Some(until) = armed
            .queue
            .front()
            .and_then(|head| head.summary.deferred_until)
        {
            return AutomationStatus::Deferred { until };
        }
        if !armed.queue.is_empty() {
            return AutomationStatus::Queued {
                waiting: u32::try_from(armed.queue.len()).unwrap_or(u32::MAX),
            };
        }
        let now = self.shared.clock.now_ms();
        if let Some(until) = armed.limiter.backoff_until.filter(|until| *until > now) {
            return AutomationStatus::BackingOff { until };
        }
        if self
            .last_firings
            .get(name)
            .is_some_and(|firing| firing.status == FiringStatus::Failed)
        {
            return AutomationStatus::Failed;
        }
        AutomationStatus::Idle
    }

    fn trigger_views(&self, entry: &CatalogEntry, armed: Option<&Armed>) -> Vec<TriggerView> {
        let now_s = self.shared.clock.now_ms().div_euclid(MILLIS_PER_SECOND);
        entry
            .meta
            .triggers
            .iter()
            .enumerate()
            .map(|(index, trigger)| TriggerView {
                index: trigger_index(index),
                kind: trigger.kind(),
                next_due: armed
                    .and_then(|armed| armed.schedule_due(index, now_s))
                    .map(|due| due.saturating_mul(MILLIS_PER_SECOND)),
                after_until: armed
                    .and_then(|armed| {
                        armed
                            .timers
                            .iter()
                            .find(|timer| timer.trigger_index == index)
                    })
                    .map(|timer| timer.until),
                consumes: matches!(trigger, Trigger::MessageReceived(filter) if filter.consume),
            })
            .collect()
    }

    fn controls(&self) -> AutomationResponse {
        AutomationResponse::Controls(Box::new(self.shared.state().session.clone()))
    }

    fn latch(&self) -> Option<PauseLatch> {
        self.shared.state().session.controls.pause.clone()
    }

    fn next_order(&mut self) -> u64 {
        self.order += 1;
        self.order
    }

    /// A launch, profile or resumed arming that failed: the frontend shows why.
    fn refused(&self, name: &str, error: &AutomationError) {
        warn!(session = %self.shared.session_id, automation = name, %error, "automation not armed");
        self.shared.emit(AutomationEvent::Notice {
            automation: name.to_owned(),
            fire_id: None,
            text: error.to_string(),
        });
    }
}

impl WorkRoute for Manager {
    fn work(&mut self) -> &mut WorkWatch {
        &mut self.work
    }

    async fn queue_work(
        &mut self,
        name: &str,
        index: usize,
        detail: EventDetail,
        key: String,
    ) -> bool {
        match self
            .shared
            .store
            .event_seen(name.to_owned(), key.clone())
            .await
        {
            Ok(AutomationEventSeen::Unseen) => {}
            Ok(AutomationEventSeen::Holding | AutomationEventSeen::Handed) => return true,
            Err(error) => {
                warn!(session = %self.shared.session_id, automation = name, %error, "automation work event not checked");
                return false;
            }
        }
        if self
            .armed
            .get(name)
            .is_none_or(|armed| armed.queue.len() >= MAX_QUEUED_EVENTS)
        {
            return false;
        }
        let Some(queued) = self.record(name, index, detail, Some(key), None).await else {
            return false;
        };
        self.enqueue(queued).await;
        true
    }
}

/// Queues `queued` behind the automation's waiting events. A coalescing event replaces a
/// waiting one of its kind in place; past [`MAX_QUEUED_EVENTS`] the oldest leaves. Returns the
/// event that left, with why.
fn push_event(queue: &mut VecDeque<Queued>, queued: Queued) -> Option<(Queued, &'static str)> {
    if queued.summary.trigger.coalesces()
        && let Some(waiting) = queue
            .iter_mut()
            .find(|waiting| waiting.summary.trigger == queued.summary.trigger)
    {
        let order = waiting.order;
        return Some((mem::replace(waiting, Queued { order, ..queued }), REPLACED));
    }
    queue.push_back(queued);
    if queue.len() > MAX_QUEUED_EVENTS {
        queue.pop_front().map(|oldest| (oldest, QUEUE_FULL))
    } else {
        None
    }
}

pub(super) fn disposition(end: &FiringEnd, trigger: TriggerKind) -> Disposition {
    Disposition::Finish(match end {
        FiringEnd::Completed => FiringStatus::Completed,
        FiringEnd::Skipped { .. } => FiringStatus::Skipped,
        FiringEnd::Released { .. } => FiringStatus::Released,
        FiringEnd::Failed(_) | FiringEnd::Stopped(_) => FiringStatus::Failed,
        FiringEnd::Limited(refusal) if trigger.is_one_shot() => {
            return Disposition::Defer(refusal.until);
        }
        FiringEnd::Limited(_) => FiringStatus::RateLimited,
        FiringEnd::Interrupted(Interruption::Paused | Interruption::Disarmed) => {
            FiringStatus::Cancelled
        }
        FiringEnd::Interrupted(Interruption::Shutdown) => FiringStatus::Interrupted,
    })
}

pub(super) fn end_reason(end: &FiringEnd) -> Option<String> {
    match end {
        FiringEnd::Skipped { reason } | FiringEnd::Released { reason } => Some(reason.clone()),
        FiringEnd::Limited(refusal) => Some(limit_reason(refusal)),
        FiringEnd::Interrupted(interruption) => Some(interruption.to_string()),
        FiringEnd::Completed | FiringEnd::Failed(_) | FiringEnd::Stopped(_) => None,
    }
}

pub(super) fn end_error(end: &FiringEnd) -> Option<StoredFiringError> {
    let (FiringEnd::Failed(error) | FiringEnd::Stopped(error)) = end else {
        return None;
    };
    Some(StoredFiringError {
        kind: error.kind.as_str().to_owned(),
        message: error.message.clone(),
        line: error.line,
        column: error.column,
    })
}

/// Which limit refused, and when the automation may act again.
fn limit_reason(refusal: &LimitRefusal) -> String {
    let until = Timestamp::from_millisecond(refusal.until)
        .map_or_else(|_| refusal.until.to_string(), |at| at.to_string());
    format!("{} until {until}", spelled(refusal.reason))
}

fn validation_report(report: &ValidationReport) -> String {
    report
        .smoke
        .iter()
        .enumerate()
        .map(|(index, smoke)| {
            let end = &smoke.run.outcome.end;
            let label = serde_json::to_value(end)
                .ok()
                .and_then(|value| value.get(END_FIELD)?.as_str().map(str::to_owned))
                .unwrap_or_default();
            let reason = end_reason(end)
                .map(|reason| format!(": {reason}"))
                .unwrap_or_default();
            format!(
                "meta.triggers[{index}] {}: {label}{reason}",
                spelled(smoke.trigger)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A unit enum as serde spells it.
pub(crate) fn spelled(value: impl Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn host_functions(source: &str) -> Vec<String> {
    let Ok(references) = references(source) else {
        return Vec::new();
    };
    HOST_FUNCTIONS
        .iter()
        .filter(|function| references.calls.contains(**function))
        .map(|function| (*function).to_owned())
        .collect()
}

fn fresh_schedules(triggers: &[Trigger], now_s: i64) -> Schedules {
    triggers
        .iter()
        .enumerate()
        .filter(|(_, trigger)| matches!(trigger, Trigger::Schedule(_)))
        .map(|(index, _)| (index, ScheduleMark::anchored(now_s)))
        .collect()
}

fn schedule_marks(schedules: &Schedules) -> Value {
    serde_json::to_value(schedules).unwrap_or_else(|_| Value::Object(Map::new()))
}

fn catalog_error(error: CatalogError) -> AutomationError {
    let message = error.to_string();
    match error {
        CatalogError::Unknown { name } => AutomationError::UnknownAutomation { name },
        CatalogError::Invalid { name, reason, .. } => AutomationError::Invalid { name, reason },
        CatalogError::Storage { .. } => AutomationError::Storage(message),
        CatalogError::Changed { name, .. }
        | CatalogError::Reread { name, .. }
        | CatalogError::OutsideProject { name, .. } => AutomationError::Invalid {
            name,
            reason: message,
        },
    }
}

fn pause_reason(source: &PauseSource) -> String {
    match source {
        PauseSource::User => PAUSED_BY_USER.to_owned(),
        PauseSource::Sdk => PAUSED_BY_SDK.to_owned(),
        PauseSource::Inspector => PAUSED_FROM_INSPECTOR.to_owned(),
        PauseSource::Script { automation } => format!("{PAUSED_BY_SCRIPT} {automation}"),
    }
}

/// A human's arming is `manual`; the launch line, a profile and `arm: "always"` arm at launch.
fn armed_reason(origin: ArmOrigin) -> ArmedReason {
    match origin {
        ArmOrigin::Manual | ArmOrigin::Sdk => ArmedReason::Manual,
        ArmOrigin::Cli | ArmOrigin::Profile | ArmOrigin::Always => ArmedReason::Launch,
    }
}

fn action_end(status: ActionStatus, error: Option<String>) -> AutomationActionEnd {
    AutomationActionEnd {
        status: status.to_row(),
        result: None,
        error,
        target: None,
    }
}

/// A request that got no answer: a failure the firing may catch, a refusal that stops it, or an
/// interruption.
fn error_end(error: &HostError) -> AutomationActionEnd {
    let status = match error {
        HostError::Failure(_) => ActionStatus::Failed,
        HostError::Refused(_) => ActionStatus::Refused,
        HostError::Interrupted(_) => ActionStatus::Interrupted,
    };
    action_end(status, Some(error.to_string()))
}

/// What a log may say of a request's error: its kind, never its message.
fn error_label(error: &HostError) -> &'static str {
    match error {
        HostError::Failure(failure) => failure.kind.as_str(),
        HostError::Refused(_) => ActionStatus::Refused.as_str(),
        HostError::Interrupted(_) => ActionStatus::Interrupted.as_str(),
    }
}

fn automation_response(snapshot: AutomationSnapshot) -> AutomationResponse {
    AutomationResponse::Automation(Box::new(snapshot))
}

pub(super) fn trigger_index(index: usize) -> u32 {
    u32::try_from(index).unwrap_or(u32::MAX)
}

/// How a `message` or `set_goal` request is delivered, and when it expires once queued at
/// `now`.
pub(super) fn delivery_terms(
    request: &ActionRequest,
    now: i64,
) -> (Option<DeliveryMode>, Option<i64>) {
    let (delivery, expires) = match request {
        ActionRequest::Message(message) => (Some(message.delivery), message.expires),
        ActionRequest::SetGoal(goal) => (Some(DeliveryMode::Next), goal.expires),
        _ => (None, None),
    };
    (
        delivery,
        expires.map(|expires| now.saturating_add(millis(expires))),
    )
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::path::Path;
    use std::pin::pin;
    use std::thread;

    use async_lock::Semaphore;
    use caudra_automation::catalog::Scope;
    use caudra_automation::engine::{DEFAULT_MAX_DELIVERIES, FiringError};
    use caudra_automation::event::{
        GoalFinishedDetail, GoalVerdict, SessionStatus, StartedBy, TurnOutcome, WorkView,
    };
    use caudra_automation::host::{
        ActionKind, HttpMethod, MAX_REQUEST_BODY_BYTES, MAX_RESPONSE_BYTES, MessageRequest,
        RecipientStatus, SendStatus,
    };
    use caudra_automation::limits::{
        BACKOFF_BASE_MS, DeliveryBackoff, LimitReason, ROLLING_WINDOW_MS, TurnWindow,
    };
    use caudra_automation::request::{DeliveryGate, OutboxClaim};
    use caudra_automation::snapshot::{
        ActionBody, ActionRow, FiringDetail, SessionControls, StateView, WaitReason,
    };
    use caudra_automation::state::MAX_STATE_BYTES;
    use caudra_automation::untrusted::{UNTRUSTED_TAG, Untrusted};
    use caudra_providers::{PeerAudience, user_agent};
    use serde_json::json;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::StoredSession;
    use crate::automation::catalog::UNAVAILABLE_IN_SDK;
    use crate::automation::clock::{FakeClock, Wake};
    use crate::automation::host::{HOST_PANICKED, NOT_IN_THIS_BUILD, THREAD_NAME as FIRING_THREAD};
    use crate::automation::http::{
        BEARER_CONFLICT, BODY_TOO_LARGE, HttpAnswer, HttpCall, HttpError, HttpErrorKind,
        HttpFuture, INVALID_HEADER_NAME, PRIVATE_TARGET, TIMED_OUT, UNDECLARED_ORIGIN,
        UNSET_SECRET,
    };
    use crate::automation::messaging::request_id;
    use crate::automation::store::stored_controls;
    use crate::automation::testing::{Answers, FakeMessaging, FakePeer, MessagingCall, delivery};
    use crate::peers::{
        ObservedMessage, ObservedSender, RecipientReceipt, STATUS_CONSUMED, STATUS_HELD,
        STATUS_QUEUED, STATUS_RATE_LIMITED, STATUS_REFUSED, STATUS_UNAVAILABLE, STATUS_UNKNOWN,
        SendFailureKind, SendOrigin,
    };

    const START_MS: i64 = 1_790_000_000_000;
    const MODEL: &str = "test/model";
    const STATE_DIR: &str = "state";
    const CONFIG_DIR: &str = "config";
    const PROJECT_DIR: &str = "project";
    /// Where a project keeps its `automations`.
    const CAUDRA_DIR: &str = ".caudra";
    const AUTOMATIONS_DIR: &str = "automations";
    const SCRIPT_EXTENSION: &str = "rhai";
    const DESCRIPTION: &str = "Exercises the runtime";
    const SESSION_LABEL: &str = "session-1";
    const TITLE: &str = "Ship the release";
    const MODE: &str = "build";
    const RESPONSE: &str = "Done.";
    const GOAL: &str = "the tests pass";
    const MESSAGE: &str = "check CI";
    const PAUSE_REASON: &str = "enough";
    const FAILURE: &str = "boom";
    const SEEN_KEY: &str = "seen";
    const TOOL: &str = "shell";
    const OTHER_TOOL: &str = "edit";
    const HOST_BUG: &str = "the clock broke";
    const FIRST: &str = "fire-1";
    const SECOND: &str = "fire-2";
    const THIRD: &str = "fire-3";

    const HELD: &str = "held";
    const COURIER: &str = "courier";
    const WORKER: &str = "worker";
    const GUARD: &str = "guard";
    const LATCH: &str = "latch";
    const CHAIN: &str = "chain";
    const FLAKY: &str = "flaky";
    const PACED: &str = "paced";
    const NUDGE: &str = "nudge";
    const WAITER: &str = "waiter";
    const HOURLY: &str = "hourly";
    const GREETER: &str = "greeter";
    const LEDGER: &str = "ledger";
    const GOALS: &str = "goals";
    const CLOCKED: &str = "clocked";
    const BRIEFED: &str = "briefed";
    const ROUTINE: &str = "routine";

    const ARMED: &str = r#"#{ kind: "armed" }"#;
    const IDLE: &str = r#"#{ kind: "idle" }"#;
    const IDLE_AFTER: &str = r#"#{ kind: "idle", after: "2m" }"#;
    const IDLE_DELAY: Duration = Duration::from_secs(2 * 60);
    const GOAL_FINISHED: &str = r#"#{ kind: "goal_finished" }"#;
    const NEEDS_PERMISSION: &str =
        r#"#{ kind: "needs_input", inputs: ["permission"], after: "1m" }"#;
    const INPUT_DELAY: Duration = Duration::from_secs(60);
    const EVERY_HOUR: &str = r#"#{ kind: "schedule", every: "1h" }"#;
    const SCHEDULE_PERIOD: Duration = Duration::from_secs(60 * 60);
    const IN_UTC: &str = r#"timezone: "UTC""#;
    const COOLDOWN: &str = r#"limits: #{ cooldown: "10m" }"#;
    const COOLDOWN_DELAY: Duration = Duration::from_secs(10 * 60);
    const EXPIRES: &str = "1h";
    const EXPIRY: Duration = Duration::from_secs(60 * 60);
    const BACKOFF: Duration = Duration::from_millis(BACKOFF_BASE_MS.unsigned_abs());
    const UNATTENDED_CAP: u32 = 1;
    const TURN_WINDOW: Duration = Duration::from_millis(ROLLING_WINDOW_MS.unsigned_abs());
    const TURNS_PER_HOUR: u32 = 1;
    /// USD, what the busy period a settle closes cost.
    const CLOSING_COST: f64 = 0.5;
    /// USD, what the turn a claimed delivery started cost.
    const CLAIMED_COST: f64 = 0.25;
    /// How far the clock moves while the session is closed: less than a schedule period.
    const LATER: Duration = Duration::from_secs(10 * 60);
    const ALWAYS: &str = r#"arm: "always""#;
    /// Declares the string arg [`TOPIC_ARG`], with the default `arm: "always"` requires.
    const TOPIC_ARGS: &str = r#"args: #{ topic: #{ type: "string", default_value: "unset" } }"#;
    const TOPIC_ARG: &str = "topic";
    const SEED: &str = "seeded by the profile";
    const EDITED: &str = "edited by the user";
    const GIVEN: &str = "given on the command line";

    const HOLD_BODY: &str = r#"log("hold");"#;
    const NOTIFY_BODY: &str = r#"notify("ping");"#;
    const NOW_BODY: &str = "now();";
    /// Appends the tool of each `needs_input` event to `state.seen`.
    const TOOL_BODY: &str = r#"let seen = state.seen ?? [];
seen.push(event.tool);
state.seen = seen;"#;
    const STEP_BODY: &str = r#"message("step " + event.trigger);"#;
    const HOLD_AT_LAUNCH: &str =
        r#"if event.trigger == "armed" && event.reason == "launch" { log("hold"); }"#;
    /// Appends the armed reason, or the trigger of any other event, to `state.seen`.
    const RECORD_BODY: &str = r#"let seen = state.seen ?? [];
let entry = if event.trigger == "armed" { event.reason } else { event.trigger };
seen.push(entry);
state.seen = seen;"#;

    const EVENTS_CLOSED: &str = "the runtime's events must stay open while a test reads them";
    const HOLD_CLOSED: &str = "a held firing must report and wait for its release";
    const UNLISTED: &str = "the mirror must list every script of the catalog";
    const UNBOUND: &str = "an armed automation must have a binding with state";
    const NOT_A_DETAIL: &str = "inspect must answer with the automation's detail";
    const NOT_A_TRACE: &str = "a firing request must answer with its trace";
    const SHUTDOWN_WAITS: &str = "shutdown must wait for the held firing";
    const NO_STORAGE: &str = "a refused runtime must not create its state directory";
    const KEEPS_ITS_PLACE: &str = "a replacement must keep the place of the event it replaced";
    const SESSION_CAP: &str = "a session must run at most four firings at once";
    const ONE_PER_AUTOMATION: &str = "an automation must run one firing at a time";
    const OLDEST_FIRST: &str = "the oldest waiting event must start first";
    const HUMAN_FIRST: &str = "a queued human prompt must go before a delivery";
    const DELIVERED_ONCE: &str = "a delivered item must never be delivered again";
    const STATE_DISCARDED: &str = "a cancelled firing must not commit its state";
    const HUMAN_INPUT_UNPAUSES: &str = "human input must clear the latch";
    const SHUTDOWN_INTERRUPTS: &str = "a firing cut off by shutdown must end interrupted";
    const EXPIRED_STAYS_OUT: &str = "an expired delivery must not return to the outbox";
    const BUSY_CANCELS: &str = "a busy session must cancel the idle delay";
    const REPEAT_RESTARTS: &str = "a repeated wait must restart the delay";
    const ONCE_PER_WAIT: &str = "a trigger must fire once per wait";
    const NO_REFIRE: &str = "a restart must not fire an occurrence again";
    const ASKS_ONCE: &str = "the runtime must ask once to save the session";
    const NO_WRITES: &str = "nothing may be written before the session is saved";
    const PARKED: &str = "an arming must wait for the session record";
    const STATE_CAPPED: &str = "state over the size limit must be refused";
    const UNREGISTERED: &str = "a stopped runtime must leave the registry";
    const OWN_HISTORY_ONLY: &str = "history must not read another session's firing";
    const TRACE_READS_ANY: &str = "a trace must read a firing of any session";
    const SWARM_CAPPED_BY_STORAGE: &str =
        "the swarm view must list as many other sessions as storage keeps unless asked for fewer";
    const TOOL_CARRIED: &str =
        "a needs_input event must carry its tool, and another tool must fire again";
    const SLOT_FREED: &str = "a firing whose host panicked must end and free its slot";
    const NO_ERROR: &str = "a failed firing must record its error";
    const EARLY_RETURN_KEEPS_BACKOFF: &str =
        "a firing that returns before acting must not reset the failure backoff";
    const ACTING_COMPLETION_RESETS: &str =
        "a firing that acts and completes must reset the failure backoff";
    const STORED_ARGS_WIN: &str = "only command-line args may replace the stored ones on a restart";
    const ARMED_ONCE: &str = "a restart must fire armed once, with reason resume for a binding \
         that stays as it was";
    const MARKS_KEPT: &str = "a resume must keep the schedule anchor and the failure backoff";
    const DISARM_HOLDS: &str =
        "a disarmed binding must stay disarmed unless the command line names it";
    const KEPT_FOR_TUI: &str =
        "a binding only the SDK refuses must not arm there and must stay armed for the TUI";
    const PROFILE_SEEDS: &str =
        "a profile's args must seed a new binding that a bare command-line entry also names";
    const SAME_PROFILE_IDLE: &str = "the same profile list again must change nothing";
    const SWITCH_BACK_REARMS: &str = "switching back to a profile must re-arm with the stored args";
    const WAKES_AT_ONCE: &str =
        "a wait that ran out since the last tick must wake the runtime at once";
    const CLOSING_PERIOD_STAYS: &str =
        "a claim sent after a settle must not take the outcome of the period it closed";
    const OWN_TURN_RECORDED: &str =
        "a claimed delivery must carry the outcome and cost of the turn it started";
    const UNCLAIMED_ERROR: &str = "an error ending a run no claim started must not back off";
    const CLAIMED_ERROR: &str = "an error ending the run a claim started must back off";
    const GUIDE_LEAVES_GATE: &str = "a guide claim must leave the wait a busy signal set";
    const STOPPED_REFUSES: &str = "a stopping or stopped runtime must refuse a claim";
    const ITEM_KEPT: &str = "a refused claim must leave its item queued";
    const CLAIM_WAITS: &str = "a claim must wait for the actor to answer it";
    const ABANDONED_REFUSES: &str = "an abandoned runtime must answer its waiting claims";

    const FETCHER: &str = "fetcher";
    const API_ORIGIN: &str = "https://api.example.com";
    const API_URL: &str = "https://api.example.com/v1/items";
    const HOOK_ORIGIN: &str = "https://hooks.example.com";
    const HOOK_URL: &str = "https://hooks.example.com/services/T0001/B0001";
    const ELSEWHERE_ORIGIN: &str = "https://elsewhere.example.com";
    const ELSEWHERE_URL: &str = "https://elsewhere.example.com/hook";
    const LOOPBACK_ORIGIN: &str = "http://127.0.0.1:8080";
    const LOOPBACK_URL: &str = "http://127.0.0.1:8080/health";
    /// A script's `query`, and the URL it encodes into.
    const QUERY: &str = r#"#{ q: "a b&c", page: "2" }"#;
    const ENCODED_URL: &str = "https://api.example.com/v1/items?page=2&q=a+b%26c";
    const NOTE_HEADER: &str = "X-Note";
    const NOTE: &str = "plain";
    const BAD_HEADER_NAME: &str = "X Note";
    const KEY_HEADER: &str = "X-Key";
    const AUTHORIZATION: &str = "Authorization";
    const BEARER: &str = "Bearer";
    const CONTENT_TYPE: &str = "Content-Type";
    const USER_AGENT: &str = "User-Agent";
    const JSON_TYPE: &str = "application/json";
    const JSON_PAYLOAD: &str = "json: #{ ok: true }";
    const JSON_SENT: &str = r#"{"ok":true}"#;
    const TEXT_PAYLOAD: &str = r#"body: "ping""#;
    const TEXT_SENT: &str = "ping";
    /// Doubles `big` to 256 KiB of a control character, which JSON escapes to six bytes: a body
    /// past the limit from text within the script's own 1 MiB limit on the text one value holds.
    const BIG_TEXT: &str = r#"let big = "\x01"; for i in 0..18 { big += big; }"#;
    const BIG_PAYLOAD: &str = "json: big";
    const HTTP_TIMEOUT: Duration = Duration::from_secs(5);
    const HTTP_TIMEOUT_TEXT: &str = "5s";
    /// The wall time a firing spends before its request.
    const SPENT: Duration = Duration::from_secs(100);
    const OK_STATUS: u16 = 200;
    const NOT_FOUND: u16 = 404;
    const MISSING_BODY: &str = r#"{"error":"missing"}"#;
    const CLIENT_ERROR: &str = "connection reset by peer";
    const SHAPE_TOKEN_VAR: &str = "CAUDRA_TEST_HTTP_SHAPE_TOKEN";
    const SHAPE_TOKEN: &str = "shape-token-3fa1";
    const SHAPE_KEY_VAR: &str = "CAUDRA_TEST_HTTP_SHAPE_KEY";
    const SHAPE_KEY: &str = "shape-key-77c2";
    const DECLARED_URL_VAR: &str = "CAUDRA_TEST_HTTP_DECLARED_URL";
    const UNDECLARED_URL_VAR: &str = "CAUDRA_TEST_HTTP_UNDECLARED_URL";
    const UNSET_VAR: &str = "CAUDRA_TEST_HTTP_NEVER_SET";
    const CONFLICT_VAR: &str = "CAUDRA_TEST_HTTP_CONFLICT_TOKEN";
    const LEAK_URL_VAR: &str = "CAUDRA_TEST_HTTP_LEAK_URL";
    const LEAK_URL: &str = "https://hooks.example.com/services/T0001/B0001/Zpath4711secret";
    const LEAK_PATH: &str = "Zpath4711secret";
    const LEAK_TOKEN_VAR: &str = "CAUDRA_TEST_HTTP_LEAK_TOKEN";
    const LEAK_TOKEN: &str = "leak-token-9b0e";
    const LEAK_KEY_VAR: &str = "CAUDRA_TEST_HTTP_LEAK_KEY";
    const LEAK_KEY: &str = "leak-key-c41d";
    /// What the host shows in place of the values of [`LEAK_TOKEN_VAR`] and [`LEAK_KEY_VAR`].
    const LEAK_TOKEN_SHOWN: &str = "${CAUDRA_TEST_HTTP_LEAK_TOKEN}";
    const LEAK_KEY_SHOWN: &str = "${CAUDRA_TEST_HTTP_LEAK_KEY}";
    /// The fields of the document [`echo`] answers.
    const ECHO_URL: &str = "url";
    const ECHO_LINES: &str = "lines";
    const ECHO_VALUES: &str = "values";
    /// The `state` keys the leak test's script and [`catching`] fill.
    const ECHO_KEY: &str = "echo";
    const PARSED_KEY: &str = "parsed";
    const KIND_KEY: &str = "kind";
    const MESSAGE_KEY: &str = "message";

    const NOT_SENT: &str = "the fake client must receive the request";
    const UNANSWERED: &str = "the test dropped the request unanswered";
    const ANSWERED_LATE: &str = "the runtime must still wait for the request it sent";
    const NO_IO: &str = "a request refused before any I/O must not reach the client";
    const NO_ACTION: &str = "the firing must journal its request";
    const NOT_JOURNALED: &str = "the journal must keep the firing";
    const NOT_AN_ACTION: &str = "an action body request must answer with the body";
    const CANCELLED: &str = "a stop must drop the request's future, which then never answers";
    const SERVED: &str = "the actor must serve on while a request is in flight";
    const LEAKED: &str = "a secret must never reach the firing, its state, the journal, the trace, \
         the mirror or an error";
    const REDACTED: &str =
        "the host must show the origin and the variable names in place of secrets";
    const SAME_RESPONSE: &str = "the journal must keep the response the firing received";

    const TAKER: &str = "taker";
    const RIVAL: &str = "understudy";
    const ONLOOKER: &str = "onlooker";
    const CONSUMING: &str = r#"#{ kind: "message_received", consume: true }"#;
    const OBSERVING: &str = r#"#{ kind: "message_received" }"#;
    const ANY_ADMISSION: &str = r#"#{ kind: "message_received", admissions: ["held", "queued"] }"#;
    const TOPICS_ONLY: &str =
        r#"#{ kind: "message_received", audiences: ["topic"], consume: true }"#;
    const FROM_SCRIPTS: &str =
        r#"#{ kind: "message_received", scripts: ["nightly-ci"], consume: true }"#;
    const MESSAGING: &str =
        r#"messaging: #{ reply: true, send: ["@lead"], publish: ["swarm.status", "broadcast"] }"#;
    const NO_BROADCAST: &str =
        r#"messaging: #{ reply: true, send: ["@lead"], publish: ["swarm.status"] }"#;
    const SCRIPT_LABEL: &str = "nightly-ci";
    const OTHER_LABEL: &str = "deploy";
    const LEAD: &str = "@lead";
    const STATUS_TOPIC: &str = "swarm.status";
    const QUESTION: &str = "status?";
    const ANSWER: &str = "all green";
    const SLOW_DOWN: &str = "slow down";
    const PEER_REASON: &str = "the recipient's own reason";
    const SKIP_BODY: &str = r#"skip("nothing to answer");"#;
    const RELEASE_BODY: &str = r#"release("not mine");"#;
    const UNDECLARED_SEND: &str = r#"send("@other", "hi");"#;
    const UNDECLARED_PUBLISH: &str = r#"publish("ci.failures", "hi");"#;
    const UNDECLARED_BROADCAST: &str = r#"broadcast("hi");"#;
    const BARE_REPLY: &str = r#"reply("hi");"#;
    const PAUSED_REPLY: &str = r#"pause_automations("enough"); reply("hi");"#;
    const CONSUMED_BODY: &str = "state.consumed = event.consumed;";
    const COUNT_BODY: &str = "state.count = (state.count ?? 0) + 1;";
    const CONSUMED_KEY: &str = "consumed";
    const COUNT_KEY: &str = "count";
    const NO_SENDER_KEY: &str = "no_sender";
    const REPLIED_KEY: &str = "replied";
    const SENT_KEY: &str = "sent";
    const PUBLISHED_KEY: &str = "published";
    const BROADCAST_KEY: &str = "broadcast";
    const RECEIPT_ID_KEY: &str = "message_id";
    const RECEIPT_STATUS_KEY: &str = "status";
    const RECIPIENTS_KEY: &str = "recipients";
    const RECIPIENT_NAME_KEY: &str = "name";
    const CALLS_OPEN: &str = "the fake session must outlive the test's wait";
    const KEPT_OR_HANDED_BACK: &str =
        "only a completed or skipped firing may keep its message; every other ending hands it back";
    const NOTHING_TAKEN: &str = "a paused or disarmed automation must take nothing";
    const FIRST_NAME_TAKES: &str = "the first consuming automation by name must take the message";
    const ONCE_PER_ADMISSION: &str = "a message must fire a trigger once per admission";
    const TAKEN_AGAIN: &str = "a message taken again must be settled while a firing holds it, and \
                               handed back once its firing released it";
    const GIVEN_UP: &str = "a message the session queued again must leave its firing unconsumed";
    const WAITS_FOR_SESSION: &str = "a release must reach a session attached after it was refused";
    const UNRECORDED_GOES_BACK: &str = "a message taken that no firing can record must go back";
    const STALLED: &str = "a runtime held in a settle must answer nothing yet";
    const GATE_OPEN: &str = "the stalled session must still wait on its gate";
    const NEXT_START: &str = "a message an ended firing still holds must go back at the next start";
    const RELEASED_ONCE: &str = "a release that succeeded must not repeat after a restart";
    const DEFERRAL_KEEPS: &str = "a deferred event must keep its message";
    const SENT_AS_AUTOMATION: &str = "a message must go out as its automation with its request id";
    const NO_SEND: &str = "a stopped call must never reach the session";
    const SCRIPT_SENDER: &str = "a script's message must match by label and name no sender";

    struct Fixture {
        temp: TempDir,
        state_dir: StateDir,
        session: StoredSession,
        clock: Arc<FakeClock>,
    }

    /// A spawned runtime whose firings hold at every `log` until the test releases them, and
    /// whose dry runs wait while the test holds their turn.
    struct Live {
        runtime: AutomationRuntime,
        handle: AutomationHandle,
        events: flume::Receiver<AutomationEvent>,
        entered: flume::Receiver<String>,
        release: flume::Sender<()>,
        dry_runs: Arc<Semaphore>,
    }

    /// Holds every request until the test answers it.
    struct FakeHttp(flume::Sender<HeldRequest>);

    /// A request the fake client holds. Once the runtime drops the request's future, answering
    /// it fails and [`Self::cancelled`] completes.
    struct HeldRequest {
        call: HttpCall,
        reply: flume::Sender<Result<HttpAnswer, HttpError>>,
        alive: flume::Receiver<()>,
    }

    impl HttpClient for FakeHttp {
        fn send(&self, call: HttpCall) -> HttpFuture {
            let (reply, answered) = flume::bounded(1);
            let (guard, alive) = flume::bounded(1);
            let _ = self.0.send(HeldRequest { call, reply, alive });
            Box::pin(async move {
                // A tuple drops in field order: `answered` disconnects before the guard wakes
                // `cancelled`.
                let held: (_, flume::Sender<()>) = (answered, guard);
                held.0
                    .recv_async()
                    .await
                    .unwrap_or_else(|_| Err(HttpError::new(HttpErrorKind::Transport, UNANSWERED)))
            })
        }
    }

    impl HeldRequest {
        fn answer(&self, answer: Result<HttpAnswer, HttpError>) {
            self.reply.send(answer).expect(ANSWERED_LATE);
        }

        fn respond(&self, status: u16, body: &str) {
            self.answer(Ok(HttpAnswer {
                status,
                body: body.as_bytes().to_vec(),
                truncated: false,
            }));
        }

        async fn cancelled(&self) {
            let _ = self.alive.recv_async().await;
        }
    }

    impl Fixture {
        fn new() -> Self {
            let mut fixture = Self::unsaved();
            fixture.save();
            fixture
        }

        /// A session whose record is not written yet.
        fn unsaved() -> Self {
            let temp = TempDir::new().unwrap();
            let project = temp.path().join(PROJECT_DIR);
            fs::create_dir(&project).unwrap();
            fs::create_dir_all(temp.path().join(CONFIG_DIR).join(AUTOMATIONS_DIR)).unwrap();
            Self {
                state_dir: StateDir::from_path(temp.path().join(STATE_DIR)),
                session: StoredSession::new(MODEL, &project.to_string_lossy()),
                clock: FakeClock::new(START_MS),
                temp,
            }
        }

        fn save(&mut self) {
            self.session.save(&self.state_dir).unwrap();
        }

        /// Another saved session in the same state directory.
        fn other_session(&self) -> StoredSession {
            let project = self.temp.path().join(PROJECT_DIR);
            let mut other = StoredSession::new(MODEL, &project.to_string_lossy());
            other.save(&self.state_dir).unwrap();
            other
        }

        /// Writes a user-scope script, which needs no trust.
        fn script(&self, name: &str, triggers: &[&str], fields: &[&str], body: &str) {
            let root = self.temp.path().join(CONFIG_DIR);
            write_script(&root, name, triggers, fields, body);
        }

        /// Writes a project-scope script, which runs only once its digest is trusted.
        fn project_script(&self, name: &str, triggers: &[&str], fields: &[&str], body: &str) {
            let root = self.temp.path().join(PROJECT_DIR).join(CAUDRA_DIR);
            write_script(&root, name, triggers, fields, body);
        }

        fn deps(&self, launch: &[&str]) -> RuntimeDeps {
            RuntimeDeps {
                state_dir: self.state_dir.clone(),
                session_id: self.session.id,
                cwd: self.temp.path().join(PROJECT_DIR),
                user_config_dir: Some(self.temp.path().join(CONFIG_DIR)),
                remote: false,
                features: FeatureFlags::all(),
                frontend: Frontend::Tui,
                config: AutomationsConfig::default(),
                controls: None,
                launch: launch
                    .iter()
                    .map(|name| LaunchArming {
                        arming: ProfileArming {
                            name: (*name).to_owned(),
                            args: None,
                        },
                        origin: ArmOrigin::Cli,
                    })
                    .collect(),
                facts: facts(),
                clock: Arc::clone(&self.clock) as Arc<dyn Clock>,
                http: None,
                workflows: None,
            }
        }

        async fn spawn(&self, launch: &[&str]) -> Live {
            boot(self.deps(launch)).await
        }

        async fn launch(&self, launch: Vec<LaunchArming>) -> Live {
            boot(RuntimeDeps {
                launch,
                ..self.deps(&[])
            })
            .await
        }

        /// Writes [`FETCHER`], which fires once armed and may reach `origins` with `variables`.
        fn fetcher(&self, origins: &[&str], variables: &[&str], body: &str) {
            let list = |items: &[&str]| {
                items
                    .iter()
                    .map(|item| format!("{item:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let network = format!("network: [{}]", list(origins));
            let secrets = format!("secrets: [{}]", list(variables));
            self.script(FETCHER, &[ARMED], &[&network, &secrets], body);
        }

        /// Deps that launch [`FETCHER`], whose requests the returned receiver holds.
        fn fetching(&self) -> (RuntimeDeps, flume::Receiver<HeldRequest>) {
            let (sent, requests) = flume::unbounded();
            let deps = RuntimeDeps {
                http: Some(Arc::new(FakeHttp(sent))),
                ..self.deps(&[FETCHER])
            };
            (deps, requests)
        }

        /// A firing as the journal kept it, read once the runtime is gone.
        async fn journaled(&self, fire_id: &str) -> FiringDetail {
            let store = AutomationStore::spawn(self.state_dir.clone(), self.session.id).unwrap();
            let detail = store
                .load_firing(fire_id.to_owned(), FiringScope::Any)
                .await
                .unwrap()
                .expect(NOT_JOURNALED);
            store.shutdown().await;
            detail
        }

        fn gate(&self) -> DeliveryGate {
            DeliveryGate {
                mode: DeliveryMode::Next,
                settled: true,
                prompt_queued: false,
                modal_open: false,
                peers_first: false,
                now: self.clock.now_ms(),
            }
        }
    }

    impl Live {
        /// Answers once the runtime handled every signal sent before it and ran what came due.
        async fn barrier(&self) {
            self.handle.request(AutomationRequest::List).await.unwrap();
        }

        fn signal(&self, signal: SessionSignal) {
            self.handle.signal(signal);
        }

        /// The next event that shows a firing of `name` in `status`.
        async fn firing(&self, name: &str, status: FiringStatus) -> FiringSummary {
            loop {
                if let AutomationEvent::Firing { firing, .. } =
                    self.events.recv_async().await.expect(EVENTS_CLOSED)
                    && firing.automation == name
                    && firing.status == status
                {
                    return *firing;
                }
            }
        }

        /// The fire id of the next firing that reached a held `log`.
        async fn held(&self) -> String {
            self.entered.recv_async().await.expect(HOLD_CLOSED)
        }

        fn release(&self) {
            self.release.send(()).expect(HOLD_CLOSED);
        }

        /// Attaches a fake session that answers as `answers`, and returns the calls it gets.
        fn attach(&self, answers: Answers) -> flume::Receiver<MessagingCall> {
            let (messaging, calls) = FakeMessaging::new(answers);
            self.handle.attach(Some(messaging));
            calls
        }

        /// Offers `message` as the session does, answering the automation that took it.
        fn offer(&self, message: &ObservedMessage) -> Option<String> {
            self.handle.message_observer().observe(message)
        }

        /// Lets [`TAKER`] take `message` through a session whose settle holds the actor until
        /// the test sends on the returned gate. Returns once the actor waits there, with the
        /// calls the session gets after the settle.
        async fn stall(
            &self,
            message: &ObservedMessage,
        ) -> (flume::Sender<()>, flume::Receiver<MessagingCall>) {
            let (unstall, gate) = flume::unbounded();
            let calls = self.attach(Answers {
                gate: Some(gate),
                ..Answers::default()
            });
            self.offer(message);
            assert_eq!(
                calls.recv_async().await.expect(CALLS_OPEN),
                settle_call(message, TAKER)
            );
            (unstall, calls)
        }

        /// Waits until each of `names` shows a firing in `status`, in any order.
        async fn all_in(&self, names: &[&str], status: FiringStatus) {
            let mut waiting: HashSet<&str> = names.iter().copied().collect();
            while !waiting.is_empty() {
                if let AutomationEvent::Firing { firing, .. } =
                    self.events.recv_async().await.expect(EVENTS_CLOSED)
                    && firing.status == status
                {
                    waiting.remove(firing.automation.as_str());
                }
            }
        }

        fn snapshot(&self, name: &str) -> AutomationSnapshot {
            self.handle.state().find(name).cloned().expect(UNLISTED)
        }

        fn firings(&self, name: &str) -> Vec<FiringSummary> {
            self.handle
                .state()
                .recent
                .iter()
                .filter(|firing| firing.automation == name)
                .cloned()
                .collect()
        }

        /// The automations with a running firing, sorted.
        fn running(&self) -> Vec<String> {
            let mut running: Vec<String> = self
                .handle
                .state()
                .recent
                .iter()
                .filter(|firing| firing.status == FiringStatus::Running)
                .map(|firing| firing.automation.clone())
                .collect();
            running.sort_unstable();
            running
        }

        async fn state_of(&self, name: &str) -> StateView {
            let inspect = AutomationRequest::Inspect {
                name: name.to_owned(),
                session_id: None,
            };
            match self.handle.request(inspect).await {
                Ok(AutomationResponse::Detail(detail)) => detail.state.expect(UNBOUND),
                other => panic!("{NOT_A_DETAIL}: {other:?}"),
            }
        }

        async fn actions(&self, fire_id: &str) -> Vec<ActionStatus> {
            self.trace(fire_id)
                .await
                .iter()
                .map(|action| action.status)
                .collect()
        }

        /// The outcome and cost each action of `fire_id` carries from the turn it started.
        async fn turns(&self, fire_id: &str) -> Vec<(Option<TurnOutcome>, Option<f64>)> {
            self.trace(fire_id)
                .await
                .iter()
                .map(|action| (action.turn_outcome, action.turn_cost))
                .collect()
        }

        async fn trace(&self, fire_id: &str) -> Vec<ActionRow> {
            self.detail(fire_id).await.actions
        }

        async fn detail(&self, fire_id: &str) -> FiringDetail {
            let trace = AutomationRequest::Firing {
                fire_id: fire_id.to_owned(),
            };
            match self.handle.request(trace).await {
                Ok(AutomationResponse::Firing(detail)) => *detail,
                other => panic!("{NOT_A_TRACE}: {other:?}"),
            }
        }

        async fn action_body(&self, fire_id: &str, seq: u64) -> ActionBody {
            let request = AutomationRequest::ActionBody {
                fire_id: fire_id.to_owned(),
                seq,
            };
            match self.handle.request(request).await {
                Ok(AutomationResponse::ActionBody(body)) => *body,
                other => panic!("{NOT_AN_ACTION}: {other:?}"),
            }
        }

        async fn claim(&self, gate: DeliveryGate) -> Option<OutboxClaim> {
            self.handle.claim(gate).await.unwrap()
        }

        fn save_requests(&self) -> usize {
            self.events
                .try_iter()
                .filter(|event| matches!(event, AutomationEvent::SaveSession))
                .count()
        }

        /// Shuts down once every held firing may go on.
        async fn stop(self) {
            let Self {
                runtime, release, ..
            } = self;
            drop(release);
            runtime.shutdown().await;
        }

        /// Shuts down while a firing is held, so it ends interrupted.
        async fn interrupt(self) {
            let Self {
                runtime, release, ..
            } = self;
            let mut stopping = pin!(runtime.shutdown());
            assert!(
                future::poll_once(&mut stopping).await.is_none(),
                "{SHUTDOWN_WAITS}"
            );
            drop(release);
            stopping.await;
        }
    }

    /// Writes `<name>.rhai` into the `automations` directory under `root`.
    fn write_script(root: &Path, name: &str, triggers: &[&str], fields: &[&str], body: &str) {
        let fields: String = fields.iter().map(|field| format!(", {field}")).collect();
        let source = format!(
            "let meta = #{{ name: \"{name}\", description: \"{DESCRIPTION}\", triggers: [{}]{fields} }};\n{body}",
            triggers.join(", ")
        );
        let dir = root.join(AUTOMATIONS_DIR);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{name}.{SCRIPT_EXTENSION}")), source).unwrap();
    }

    async fn boot(deps: RuntimeDeps) -> Live {
        let (entered_sender, entered) = flume::unbounded();
        let (release, released) = flume::unbounded();
        let (mut manager, boot) = Manager::open(deps).await.unwrap();
        manager.hold = Some(Hold {
            entered: entered_sender,
            release: released,
        });
        let dry_runs = Arc::clone(&manager.dry_runs.turns);
        let runtime = manager.start(boot).await;
        let handle = runtime.handle();
        Live {
            events: handle.events(),
            handle,
            runtime,
            entered,
            release,
            dry_runs,
        }
    }

    /// Passes the fake clock through, but panics when a firing's host asks it the time, as a
    /// bug on the host side of the bridge would.
    struct HostPanickingClock(Arc<FakeClock>);

    impl Clock for HostPanickingClock {
        fn now_ms(&self) -> i64 {
            if thread::current().name() == Some(FIRING_THREAD) {
                panic!("{HOST_BUG}");
            }
            self.0.now_ms()
        }

        fn monotonic(&self) -> Duration {
            self.0.monotonic()
        }

        fn sleep_until(&self, deadline: Duration) -> Wake {
            self.0.sleep_until(deadline)
        }
    }

    fn facts() -> SessionView {
        SessionView {
            id: SESSION_LABEL.into(),
            title: Untrusted::text(TITLE),
            name: None,
            mode: MODE.into(),
            status: SessionStatus::Idle,
            status_since: START_MS / MILLIS_PER_SECOND,
            goal: None,
            cost: None,
            groups: Vec::new(),
            work: WorkView::default(),
        }
    }

    fn settled() -> SessionSignal {
        settled_after(TurnOutcome::Completed)
    }

    fn settled_after(outcome: TurnOutcome) -> SessionSignal {
        settled_costing(outcome, None)
    }

    fn settled_costing(outcome: TurnOutcome, cost: Option<f64>) -> SessionSignal {
        SessionSignal::Settled(Box::new(IdleDetail {
            outcome,
            error_kind: None,
            error: None,
            started_by: StartedBy::User,
            automations: Vec::new(),
            runs: 1,
            busy_s: 1,
            cost,
            work: Vec::new(),
            last_response: Untrusted::text(RESPONSE),
        }))
    }

    fn goal_finished() -> SessionSignal {
        SessionSignal::GoalFinished(Box::new(GoalFinishedDetail {
            verdict: GoalVerdict::Met,
            condition: GOAL.into(),
            reason: Untrusted::text(GOAL),
            evaluations: 1,
            duration_s: 1,
            cost: None,
        }))
    }

    fn permission(tool: &str) -> SessionSignal {
        SessionSignal::NeedsInput {
            input: InputKind::Permission,
            tool: Some(tool.to_owned()),
        }
    }

    /// The state [`RECORD_BODY`] leaves after recording `entries`.
    fn seen(entries: &[String]) -> Value {
        json!({ SEEN_KEY: entries })
    }

    /// Args that set [`TOPIC_ARG`] to `value`.
    fn topic(value: &str) -> Value {
        json!({ TOPIC_ARG: value })
    }

    fn profile_arming(name: &str, topic_value: Option<&str>) -> ProfileArming {
        ProfileArming {
            name: name.to_owned(),
            args: topic_value.map(topic),
        }
    }

    fn launching(name: &str, topic_value: Option<&str>, origin: ArmOrigin) -> LaunchArming {
        LaunchArming {
            arming: profile_arming(name, topic_value),
            origin,
        }
    }

    fn disarm(name: &str) -> AutomationRequest {
        AutomationRequest::Disarm { name: name.into() }
    }

    /// Records as [`RECORD_BODY`] does, but fails on `idle`, which discards the record.
    fn failing_on_idle_body() -> String {
        format!(
            r#"{RECORD_BODY}
if event.trigger == "idle" {{ throw "{FAILURE}"; }}"#
        )
    }

    fn message_body() -> String {
        format!(r#"message("{MESSAGE}");"#)
    }

    fn message_at_launch_body() -> String {
        format!(
            r#"if event.reason == "launch" {{ message("{MESSAGE}", #{{ expires: "{EXPIRES}" }}); }}"#
        )
    }

    fn failing_body() -> String {
        format!(r#"message("{MESSAGE}"); throw "{FAILURE}";"#)
    }

    /// Returns before acting on `goal_finished`. On `idle` it acts, then fails after a turn
    /// that ended in error.
    fn acting_body() -> String {
        format!(
            r#"if event.trigger == "{goal_finished}" {{ return; }}
{NOTIFY_BODY}
if event.outcome == "{error}" {{ throw "{FAILURE}"; }}"#,
            goal_finished = spelled(TriggerKind::GoalFinished),
            error = spelled(TurnOutcome::Error),
        )
    }

    /// Records, and pauses every automation on `idle`.
    fn pausing_body() -> String {
        format!(
            r#"{RECORD_BODY}
if event.trigger == "idle" {{ pause_automations("{PAUSE_REASON}"); }}"#
        )
    }

    /// Records the condition `set_goal` answers, or the kind of failure it raises.
    fn goal_body(replace: bool) -> String {
        format!(
            r#"let seen = state.seen ?? [];
try {{ seen.push(set_goal("{GOAL}", #{{ replace: {replace} }}).condition); }} catch (failure) {{ seen.push(failure.kind); }}
state.seen = seen;"#
        )
    }

    fn queued(fire_id: &str, trigger: TriggerKind, order: u64) -> Queued {
        Queued {
            summary: FiringSummary {
                fire_id: fire_id.into(),
                automation: HELD.into(),
                digest: String::new(),
                trigger,
                trigger_index: 0,
                event_key: None,
                consumed: false,
                status: FiringStatus::Queued,
                reason: None,
                error: None,
                repeats: 1,
                attempts: 0,
                operations: 0,
                state_outcome: None,
                queued_at: START_MS,
                deferred_until: None,
                started_at: None,
                finished_at: None,
                action_count: 0,
                first_action: None,
            },
            event: Event {
                fire_id: fire_id.into(),
                at: START_MS / MILLIS_PER_SECOND,
                session: facts(),
                detail: EventDetail::Armed {
                    reason: ArmedReason::Launch,
                },
            },
            order,
            next_seq: 0,
        }
    }

    fn fire_ids(queue: &VecDeque<Queued>) -> Vec<&str> {
        queue
            .iter()
            .map(|queued| queued.summary.fire_id.as_str())
            .collect()
    }

    fn failure() -> FiringError {
        FiringError {
            kind: ErrorKind::Stop(StopKind::Internal),
            message: FAILURE.into(),
            line: None,
            column: None,
        }
    }

    fn refusal() -> LimitRefusal {
        LimitRefusal {
            reason: LimitReason::Cooldown,
            until: START_MS,
        }
    }

    #[test_case(TriggerKind::Armed; "armed")]
    #[test_case(TriggerKind::Idle; "idle")]
    #[test_case(TriggerKind::NeedsInput; "needs_input")]
    #[test_case(TriggerKind::Schedule; "schedule")]
    fn a_newer_event_replaces_a_waiting_one_of_its_kind_in_place(trigger: TriggerKind) {
        let mut queue = VecDeque::new();
        assert!(push_event(&mut queue, queued(FIRST, trigger, 1)).is_none());
        assert!(push_event(&mut queue, queued(SECOND, TriggerKind::GoalFinished, 2)).is_none());

        let (replaced, reason) = push_event(&mut queue, queued(THIRD, trigger, 3)).unwrap();

        assert_eq!(
            (replaced.summary.fire_id.as_str(), reason),
            (FIRST, REPLACED)
        );
        assert_eq!(fire_ids(&queue), [THIRD, SECOND]);
        assert_eq!(queue[0].order, 1, "{KEEPS_ITS_PLACE}");
    }

    #[test]
    fn a_full_queue_drops_its_oldest_event() {
        let ids: Vec<String> = (0..=MAX_QUEUED_EVENTS)
            .map(|index| format!("fire-{index}"))
            .collect();
        let mut queue = VecDeque::new();
        for (order, fire_id) in (0..).zip(&ids[..MAX_QUEUED_EVENTS]) {
            let pushed = push_event(
                &mut queue,
                queued(fire_id, TriggerKind::GoalFinished, order),
            );
            assert!(pushed.is_none());
        }
        let newest = &ids[MAX_QUEUED_EVENTS];

        let (dropped, reason) = push_event(
            &mut queue,
            queued(newest, TriggerKind::GoalFinished, u64::MAX),
        )
        .unwrap();

        assert_eq!(
            (dropped.summary.fire_id.as_str(), reason),
            (ids[0].as_str(), QUEUE_FULL)
        );
        assert_eq!(queue.len(), MAX_QUEUED_EVENTS);
        assert_eq!(
            queue.back().map(|queued| queued.summary.fire_id.as_str()),
            Some(newest.as_str())
        );
    }

    #[test_case(FiringEnd::Completed, TriggerKind::Idle => Disposition::Finish(FiringStatus::Completed); "completed")]
    #[test_case(FiringEnd::Skipped { reason: FAILURE.into() }, TriggerKind::Idle => Disposition::Finish(FiringStatus::Skipped); "skipped")]
    #[test_case(FiringEnd::Released { reason: FAILURE.into() }, TriggerKind::MessageReceived => Disposition::Finish(FiringStatus::Released); "released")]
    #[test_case(FiringEnd::Failed(failure()), TriggerKind::Idle => Disposition::Finish(FiringStatus::Failed); "failed")]
    #[test_case(FiringEnd::Stopped(failure()), TriggerKind::Idle => Disposition::Finish(FiringStatus::Failed); "stopped")]
    #[test_case(FiringEnd::Limited(refusal()), TriggerKind::GoalFinished => Disposition::Defer(START_MS); "limited_one_shot")]
    #[test_case(FiringEnd::Limited(refusal()), TriggerKind::Schedule => Disposition::Finish(FiringStatus::RateLimited); "limited_recurring")]
    #[test_case(FiringEnd::Interrupted(Interruption::Paused), TriggerKind::Idle => Disposition::Finish(FiringStatus::Cancelled); "paused")]
    #[test_case(FiringEnd::Interrupted(Interruption::Disarmed), TriggerKind::Idle => Disposition::Finish(FiringStatus::Cancelled); "disarmed")]
    #[test_case(FiringEnd::Interrupted(Interruption::Shutdown), TriggerKind::Idle => Disposition::Finish(FiringStatus::Interrupted); "shutdown")]
    fn a_firing_end_maps_to_its_journal_status(
        end: FiringEnd,
        trigger: TriggerKind,
    ) -> Disposition {
        disposition(&end, trigger)
    }

    #[test]
    fn the_runtime_refuses_while_the_flag_is_off_without_touching_storage() {
        smol::block_on(async {
            let fixture = Fixture::unsaved();
            let deps = RuntimeDeps {
                features: FeatureFlags::all().without(Feature::Automations),
                ..fixture.deps(&[])
            };

            let refused = AutomationRuntime::spawn(deps).await.err();

            assert_eq!(refused, Some(AutomationError::Unavailable));
            assert!(
                !fixture.temp.path().join(STATE_DIR).exists(),
                "{NO_STORAGE}"
            );
        });
    }

    #[test]
    fn a_runtime_is_found_by_its_session_only_while_it_runs() {
        smol::block_on(async {
            let fixture = Fixture::new();
            let session_id = fixture.session.id;
            assert!(AutomationHandle::lookup(session_id).is_none());

            let live = fixture.spawn(&[]).await;
            let found = AutomationHandle::lookup(session_id).map(|handle| handle.session_id());
            live.stop().await;

            assert_eq!(found, Some(session_id));
            assert!(
                AutomationHandle::lookup(session_id).is_none(),
                "{UNREGISTERED}"
            );
        });
    }

    #[test]
    fn history_reads_this_sessions_firings_while_a_trace_reads_any() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.script(GREETER, &[ARMED], &[], RECORD_BODY);
            let elsewhere = boot(RuntimeDeps {
                session_id: fixture.other_session().id,
                ..fixture.deps(&[GREETER])
            })
            .await;
            let foreign = elsewhere.firing(GREETER, FiringStatus::Completed).await;
            elsewhere.stop().await;
            let live = fixture.spawn(&[GREETER]).await;
            let own = live.firing(GREETER, FiringStatus::Completed).await;
            let history = |fire_id: &str| AutomationRequest::History {
                name: None,
                fire_id: Some(fire_id.to_owned()),
                limit: None,
            };
            let trace = AutomationRequest::Firing {
                fire_id: foreign.fire_id.clone(),
            };

            let foreign_history = live.handle.request(history(&foreign.fire_id)).await;
            let own_history = live.handle.request(history(&own.fire_id)).await;
            let foreign_trace = live.handle.request(trace).await;

            assert_eq!(
                foreign_history,
                Err(AutomationError::UnknownFiring {
                    fire_id: foreign.fire_id.clone()
                }),
                "{OWN_HISTORY_ONLY}"
            );
            assert!(
                matches!(own_history, Ok(AutomationResponse::Firing(detail)) if detail.firing.fire_id == own.fire_id)
            );
            assert!(
                matches!(foreign_trace, Ok(AutomationResponse::Firing(detail)) if detail.firing.fire_id == foreign.fire_id),
                "{TRACE_READS_ANY}"
            );
            live.stop().await;
        });
    }

    #[test]
    fn the_swarm_view_lists_as_many_sessions_as_storage_keeps_by_default() {
        smol::block_on(async {
            let fixture = Fixture::new();
            for _ in 0..=MAX_HISTORY_SESSIONS {
                let store =
                    AutomationStore::spawn(fixture.state_dir.clone(), fixture.other_session().id)
                        .unwrap();
                store
                    .bind(BindingView {
                        name: GREETER.into(),
                        scope: Scope::User,
                        origin: ArmOrigin::Manual,
                        armed: true,
                        args: json!({}),
                        args_digest: None,
                    })
                    .await
                    .unwrap();
                store.shutdown().await;
            }
            let live = fixture.spawn(&[]).await;

            let listed = live
                .handle
                .request(AutomationRequest::Sessions { limit: None })
                .await;

            assert!(
                matches!(listed, Ok(AutomationResponse::Sessions(sessions)) if sessions.len() == MAX_HISTORY_SESSIONS),
                "{SWARM_CAPPED_BY_STORAGE}"
            );
            live.stop().await;
        });
    }

    #[test]
    fn firings_run_one_per_automation_and_at_most_four_per_session() {
        smol::block_on(async {
            let fixture = Fixture::new();
            let names: Vec<String> = (0..=MAX_RUNNING_FIRINGS)
                .map(|index| format!("{HELD}-{index}"))
                .collect();
            for name in &names {
                fixture.script(name, &[ARMED, IDLE], &[], HOLD_BODY);
            }
            let launch: Vec<&str> = names.iter().map(String::as_str).collect();
            let live = fixture.spawn(&launch).await;
            for _ in 0..MAX_RUNNING_FIRINGS {
                live.held().await;
            }
            let running = live.running();
            let waiting = names.iter().find(|name| !running.contains(name)).unwrap();
            assert_eq!(running.len(), MAX_RUNNING_FIRINGS, "{SESSION_CAP}");
            assert_eq!(
                live.snapshot(waiting).status,
                AutomationStatus::Queued { waiting: 1 }
            );

            live.signal(settled());
            live.barrier().await;
            assert_eq!(live.running(), running, "{ONE_PER_AUTOMATION}");
            live.release();
            let started = live.held().await;

            let state = live.handle.state();
            let automation = state
                .recent
                .iter()
                .find(|firing| firing.fire_id == started)
                .map(|firing| firing.automation.as_str());
            assert_eq!(automation, Some(waiting.as_str()), "{OLDEST_FIRST}");
            live.stop().await;
        });
    }

    #[test]
    fn a_panic_on_the_host_side_fails_the_firing_and_frees_its_slot() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.script(CLOCKED, &[ARMED, IDLE], &[], NOW_BODY);
            let live = boot(RuntimeDeps {
                clock: Arc::new(HostPanickingClock(Arc::clone(&fixture.clock))),
                ..fixture.deps(&[CLOCKED])
            })
            .await;

            let launched = live.firing(CLOCKED, FiringStatus::Failed).await;
            live.signal(settled());
            live.firing(CLOCKED, FiringStatus::Failed).await;

            let error = launched.error.expect(NO_ERROR);
            assert_eq!(
                (error.kind.as_str(), error.message),
                (
                    ErrorKind::Stop(StopKind::Internal).as_str(),
                    format!("{HOST_PANICKED}: {HOST_BUG}")
                )
            );
            assert!(live.running().is_empty(), "{SLOT_FREED}");
            live.stop().await;
        });
    }

    #[test]
    fn a_queued_message_waits_for_the_gate_and_is_delivered_once() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.script(COURIER, &[ARMED], &[], &message_body());
            let live = fixture.spawn(&[COURIER]).await;
            let firing = live.firing(COURIER, FiringStatus::Completed).await;
            let prompt_queued = DeliveryGate {
                prompt_queued: true,
                ..fixture.gate()
            };

            assert!(live.claim(prompt_queued).await.is_none(), "{HUMAN_FIRST}");
            assert_eq!(
                live.handle.state().outbox[0].wait,
                Some(WaitReason::HumanPromptQueued)
            );
            let claim = live.claim(fixture.gate()).await.unwrap();

            assert_eq!(
                (claim.automation.as_str(), claim.text.as_str()),
                (COURIER, MESSAGE)
            );
            assert!(
                live.claim(fixture.gate()).await.is_none(),
                "{DELIVERED_ONCE}"
            );
            assert_eq!(
                live.actions(&firing.fire_id).await,
                [ActionStatus::Delivered]
            );
            live.stop().await;
        });
    }

    #[test]
    fn human_input_lifts_the_unattended_cap() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.script(COURIER, &[ARMED, IDLE], &[], STEP_BODY);
            let mut deps = fixture.deps(&[COURIER]);
            deps.config.max_unattended_turns = Some(UNATTENDED_CAP);
            let live = boot(deps).await;
            live.firing(COURIER, FiringStatus::Completed).await;
            assert!(live.claim(fixture.gate()).await.is_some());
            live.signal(settled());
            live.firing(COURIER, FiringStatus::Completed).await;

            assert!(live.claim(fixture.gate()).await.is_none());
            assert_eq!(
                live.handle.state().outbox[0].wait,
                Some(WaitReason::UnattendedCap {
                    cap: UNATTENDED_CAP
                })
            );
            live.signal(SessionSignal::HumanInput);
            live.barrier().await;

            assert!(live.claim(fixture.gate()).await.is_some());
            live.stop().await;
        });
    }

    #[test]
    fn a_claim_sent_right_after_a_settle_carries_the_outcome_of_its_own_turn() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.script(COURIER, &[ARMED], &[], &message_body());
            let live = fixture.spawn(&[COURIER]).await;
            let firing = live.firing(COURIER, FiringStatus::Completed).await;

            live.signal(settled_costing(TurnOutcome::MaxTurns, Some(CLOSING_COST)));
            assert!(live.claim(fixture.gate()).await.is_some());
            assert_eq!(
                live.turns(&firing.fire_id).await,
                [(None, None)],
                "{CLOSING_PERIOD_STAYS}"
            );
            live.signal(SessionSignal::RunEnded(TurnOutcome::Completed));
            live.signal(settled_costing(TurnOutcome::Completed, Some(CLAIMED_COST)));

            assert_eq!(
                live.turns(&firing.fire_id).await,
                [(Some(TurnOutcome::Completed), Some(CLAIMED_COST))],
                "{OWN_TURN_RECORDED}"
            );
            live.stop().await;
        });
    }

    #[test]
    fn only_an_error_ending_the_run_a_claim_started_backs_deliveries_off() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.script(COURIER, &[ARMED], &[], &message_body());
            let live = fixture.spawn(&[COURIER]).await;
            live.firing(COURIER, FiringStatus::Completed).await;
            let errors = || live.handle.state().session.controls.delivery_backoff.errors;

            live.signal(SessionSignal::RunEnded(TurnOutcome::Error));
            assert!(live.claim(fixture.gate()).await.is_some());
            live.barrier().await;
            assert_eq!(errors(), 0, "{UNCLAIMED_ERROR}");
            live.signal(SessionSignal::RunEnded(TurnOutcome::Error));
            live.barrier().await;

            assert_eq!(errors(), 1, "{CLAIMED_ERROR}");
            live.stop().await;
        });
    }

    #[test]
    fn a_guide_claim_leaves_the_wait_a_busy_signal_set() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.script(COURIER, &[ARMED], &[], &message_body());
            let live = fixture.spawn(&[COURIER]).await;
            live.firing(COURIER, FiringStatus::Completed).await;
            live.signal(SessionSignal::Busy {
                blockers: vec![SettleBlocker::Busy, SettleBlocker::PromptQueued],
            });

            assert_eq!(live.handle.claim_guidance().await, Ok(None));

            assert_eq!(
                live.handle.state().outbox[0].wait,
                Some(WaitReason::HumanPromptQueued),
                "{GUIDE_LEAVES_GATE}"
            );
            live.stop().await;
        });
    }

    #[test]
    fn a_stopping_or_stopped_runtime_refuses_a_claim_and_keeps_the_item() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.script(COURIER, &[ARMED], &[], &message_body());
            fixture.script(HELD, &[ARMED], &[], HOLD_BODY);
            let live = fixture.spawn(&[COURIER, HELD]).await;
            let firing = live.firing(COURIER, FiringStatus::Completed).await;
            live.held().await;
            let Live {
                runtime,
                handle,
                release,
                ..
            } = live;
            let mut stopping = pin!(runtime.shutdown());
            assert!(
                future::poll_once(&mut stopping).await.is_none(),
                "{SHUTDOWN_WAITS}"
            );

            let while_stopping = handle.claim(fixture.gate()).await;
            drop(release);
            stopping.await;
            let once_stopped = handle.claim(fixture.gate()).await;

            let refused = Err(AutomationError::Unavailable);
            assert_eq!(while_stopping, refused, "{STOPPED_REFUSES}");
            assert_eq!(once_stopped, refused, "{STOPPED_REFUSES}");
            assert_eq!(handle.state().outbox.len(), 1, "{ITEM_KEPT}");
            let (manager, boot) = Manager::open(fixture.deps(&[])).await.unwrap();
            let stored: Vec<String> = boot
                .restored
                .outbox
                .into_iter()
                .map(|queued| queued.item.fire_id)
                .collect();
            assert_eq!(stored, [firing.fire_id], "{ITEM_KEPT}");
            manager.shared.store.clone().shutdown().await;
        });
    }

    #[test]
    fn an_abandoned_runtime_answers_its_waiting_claims_unavailable() {
        smol::block_on(async {
            let fixture = Fixture::new();
            let (manager, boot) = Manager::open(fixture.deps(&[])).await.unwrap();
            let handle = AutomationHandle::new(Arc::clone(&manager.shared));
            let mut waiting = pin!(handle.claim(fixture.gate()));
            assert!(
                future::poll_once(&mut waiting).await.is_none(),
                "{CLAIM_WAITS}"
            );

            drop(boot);

            let refused = Err(AutomationError::Unavailable);
            assert_eq!(
                future::poll_once(&mut waiting).await,
                Some(refused.clone()),
                "{ABANDONED_REFUSES}"
            );
            assert_eq!(
                handle.claim(fixture.gate()).await,
                refused,
                "{ABANDONED_REFUSES}"
            );
            manager.shared.store.clone().shutdown().await;
        });
    }

    #[test_case(false, FailureKind::GoalActive.as_str(); "fails_while_a_goal_waits")]
    #[test_case(true, GOAL; "replaces_it_when_asked")]
    fn set_goal_answers_its_condition_or_goal_active(replace: bool, second: &str) {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.script(GOALS, &[ARMED, IDLE], &[], &goal_body(replace));
            let live = fixture.spawn(&[GOALS]).await;
            live.firing(GOALS, FiringStatus::Completed).await;

            live.signal(settled());
            live.firing(GOALS, FiringStatus::Completed).await;
            let claim = live.claim(fixture.gate()).await;

            assert_eq!(
                live.state_of(GOALS).await.value,
                seen(&[GOAL.to_owned(), second.to_owned()])
            );
            assert_eq!(
                claim
                    .and_then(|claim| claim.goal)
                    .map(|goal| goal.condition),
                Some(GOAL.to_owned())
            );
            live.stop().await;
        });
    }

    #[test]
    fn a_pause_cancels_running_firings_but_lets_the_pausing_one_finish() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.script(
                WORKER,
                &[ARMED],
                &[],
                &format!("{RECORD_BODY}\n{HOLD_BODY}"),
            );
            fixture.script(GUARD, &[IDLE], &[], &pausing_body());
            let live = fixture.spawn(&[WORKER, GUARD]).await;
            live.held().await;

            live.signal(settled());
            live.firing(GUARD, FiringStatus::Completed).await;
            live.release();
            live.firing(WORKER, FiringStatus::Cancelled).await;

            assert_eq!(
                live.state_of(WORKER).await.value,
                json!({}),
                "{STATE_DISCARDED}"
            );
            assert_eq!(
                live.state_of(GUARD).await.value,
                seen(&[spelled(TriggerKind::Idle)])
            );
            let latch = live.handle.state().session.controls.pause.clone();
            assert_eq!(
                latch.map(|latch| (latch.source, latch.reason)),
                Some((
                    PauseSource::Script {
                        automation: GUARD.into()
                    },
                    PAUSE_REASON.to_owned()
                ))
            );
            live.stop().await;
        });
    }

    #[test]
    fn events_during_a_pause_are_paused_and_human_input_rearms_with_unpaused() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.script(LATCH, &[ARMED, IDLE], &[], &pausing_body());
            let live = fixture.spawn(&[LATCH]).await;
            live.firing(LATCH, FiringStatus::Completed).await;
            live.signal(settled());
            live.firing(LATCH, FiringStatus::Completed).await;

            live.signal(settled());
            let paused = live.firing(LATCH, FiringStatus::Paused).await;
            live.barrier().await;
            assert_eq!(paused.reason.as_deref(), Some(PAUSE_REASON));
            assert_eq!(live.snapshot(LATCH).status, AutomationStatus::Paused);
            live.signal(SessionSignal::HumanInput);
            live.firing(LATCH, FiringStatus::Completed).await;

            let recorded = seen(&[
                spelled(ArmedReason::Launch),
                spelled(TriggerKind::Idle),
                spelled(ArmedReason::Unpaused),
            ]);
            assert_eq!(live.state_of(LATCH).await.value, recorded);
            assert!(
                live.handle.state().session.controls.pause.is_none(),
                "{HUMAN_INPUT_UNPAUSES}"
            );
            live.stop().await;
        });
    }

    #[test]
    fn a_restored_latch_holds_events_until_human_input() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.script(GREETER, &[ARMED], &[], RECORD_BODY);
            let controls = SessionControls {
                pause: Some(PauseLatch {
                    reason: PAUSE_REASON.into(),
                    source: PauseSource::User,
                    at: START_MS,
                }),
                ..SessionControls::default()
            };
            let live = boot(RuntimeDeps {
                controls: Some(stored_controls(&controls, Vec::new())),
                ..fixture.deps(&[GREETER])
            })
            .await;

            let paused = live.firing(GREETER, FiringStatus::Paused).await;
            live.signal(SessionSignal::HumanInput);
            live.firing(GREETER, FiringStatus::Completed).await;

            assert_eq!(paused.reason.as_deref(), Some(PAUSE_REASON));
            assert_eq!(
                live.state_of(GREETER).await.value,
                seen(&[spelled(ArmedReason::Unpaused)])
            );
            live.stop().await;
        });
    }

    #[test]
    fn a_restart_runs_waiting_one_shot_events_ahead_of_armed_resume() {
        smol::block_on(async {
            let fixture = Fixture::new();
            let body = format!("{HOLD_AT_LAUNCH}\n{RECORD_BODY}");
            fixture.script(CHAIN, &[ARMED, GOAL_FINISHED], &[], &body);
            let first = fixture.spawn(&[CHAIN]).await;
            first.held().await;
            first.signal(goal_finished());
            first.barrier().await;
            first.interrupt().await;

            let second = fixture.spawn(&[]).await;
            second.firing(CHAIN, FiringStatus::Completed).await;
            second.firing(CHAIN, FiringStatus::Completed).await;

            let recorded = seen(&[
                spelled(TriggerKind::GoalFinished),
                spelled(ArmedReason::Resume),
            ]);
            assert_eq!(second.state_of(CHAIN).await.value, recorded);
            assert!(
                second
                    .firings(CHAIN)
                    .iter()
                    .any(|firing| firing.status == FiringStatus::Interrupted),
                "{SHUTDOWN_INTERRUPTS}"
            );
            second.stop().await;
        });
    }

    #[test]
    fn a_restart_returns_a_waiting_delivery_and_never_repeats_a_delivered_one() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.script(COURIER, &[ARMED], &[], &message_at_launch_body());
            let first = fixture.spawn(&[COURIER]).await;
            first.firing(COURIER, FiringStatus::Completed).await;
            first.stop().await;

            let second = fixture.spawn(&[]).await;
            let returned = second.handle.state().outbox.len();
            let claim = second.claim(fixture.gate()).await;
            second.stop().await;
            let third = fixture.spawn(&[]).await;

            assert_eq!(returned, 1);
            assert_eq!(claim.map(|claim| claim.text), Some(MESSAGE.to_owned()));
            assert!(third.handle.state().outbox.is_empty(), "{DELIVERED_ONCE}");
            third.stop().await;
        });
    }

    #[test]
    fn a_delivery_that_expired_while_closed_expires_on_restart() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.script(COURIER, &[ARMED], &[], &message_at_launch_body());
            let first = fixture.spawn(&[COURIER]).await;
            let firing = first.firing(COURIER, FiringStatus::Completed).await;
            first.stop().await;
            fixture.clock.advance(EXPIRY);

            let second = fixture.spawn(&[]).await;

            assert!(
                second.handle.state().outbox.is_empty(),
                "{EXPIRED_STAYS_OUT}"
            );
            assert_eq!(
                second.actions(&firing.fire_id).await,
                [ActionStatus::Expired]
            );
            second.stop().await;
        });
    }

    #[test]
    fn failures_back_an_automation_off_with_a_doubling_delay() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.script(FLAKY, &[IDLE], &[], &failing_body());
            let live = fixture.spawn(&[FLAKY]).await;
            let first_until = START_MS + BACKOFF_BASE_MS;

            live.signal(settled());
            live.firing(FLAKY, FiringStatus::Failed).await;
            live.barrier().await;
            assert_eq!(
                live.snapshot(FLAKY).status,
                AutomationStatus::BackingOff { until: first_until }
            );
            live.signal(settled());
            live.firing(FLAKY, FiringStatus::RateLimited).await;
            fixture.clock.advance(BACKOFF);
            live.signal(settled());
            live.firing(FLAKY, FiringStatus::Failed).await;
            live.barrier().await;

            let limiter = live.snapshot(FLAKY).limiter;
            assert_eq!(limiter.failure_streak, 2);
            assert_eq!(
                limiter.backoff_until,
                Some(first_until + 2 * BACKOFF_BASE_MS)
            );
            live.stop().await;
        });
    }

    #[test]
    fn only_a_firing_that_acts_and_completes_resets_the_failure_backoff() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.script(FLAKY, &[IDLE, GOAL_FINISHED], &[], &acting_body());
            let live = fixture.spawn(&[FLAKY]).await;
            let backing_off = AutomationStatus::BackingOff {
                until: START_MS + BACKOFF_BASE_MS,
            };

            live.signal(settled_after(TurnOutcome::Error));
            live.firing(FLAKY, FiringStatus::Failed).await;
            live.signal(goal_finished());
            live.firing(FLAKY, FiringStatus::Completed).await;
            live.barrier().await;
            assert_eq!(
                live.snapshot(FLAKY).status,
                backing_off,
                "{EARLY_RETURN_KEEPS_BACKOFF}"
            );
            fixture.clock.advance(BACKOFF);
            live.signal(settled());
            live.firing(FLAKY, FiringStatus::Completed).await;
            live.barrier().await;

            let limiter = live.snapshot(FLAKY).limiter;
            assert_eq!(
                (limiter.failure_streak, limiter.backoff_until),
                (0, None),
                "{ACTING_COMPLETION_RESETS}"
            );
            live.stop().await;
        });
    }

    #[test]
    fn a_limited_one_shot_event_is_deferred_and_retried() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.script(PACED, &[ARMED, GOAL_FINISHED], &[COOLDOWN], STEP_BODY);
            let live = fixture.spawn(&[PACED]).await;
            live.firing(PACED, FiringStatus::Completed).await;
            let until = START_MS + millis(COOLDOWN_DELAY);

            live.signal(goal_finished());
            let deferred = live.firing(PACED, FiringStatus::Deferred).await;
            live.barrier().await;
            assert_eq!(deferred.deferred_until, Some(until));
            assert_eq!(
                live.snapshot(PACED).status,
                AutomationStatus::Deferred { until }
            );
            fixture.clock.advance(COOLDOWN_DELAY);
            let retried = live.firing(PACED, FiringStatus::Completed).await;

            assert_eq!(retried.fire_id, deferred.fire_id);
            live.stop().await;
        });
    }

    #[test]
    fn a_limited_recurring_event_is_rate_limited() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.script(PACED, &[ARMED, IDLE], &[COOLDOWN], STEP_BODY);
            let live = fixture.spawn(&[PACED]).await;
            live.firing(PACED, FiringStatus::Completed).await;

            live.signal(settled());
            let limited = live.firing(PACED, FiringStatus::RateLimited).await;

            let refusal = LimitRefusal {
                reason: LimitReason::Cooldown,
                until: START_MS + millis(COOLDOWN_DELAY),
            };
            assert_eq!(limited.reason, Some(limit_reason(&refusal)));
            live.stop().await;
        });
    }

    async fn idle_delay_started(fixture: &Fixture) -> Live {
        fixture.script(NUDGE, &[IDLE_AFTER], &[], NOTIFY_BODY);
        let live = fixture.spawn(&[NUDGE]).await;
        live.signal(settled());
        live.barrier().await;
        assert_eq!(
            live.snapshot(NUDGE).triggers[0].after_until,
            Some(START_MS + millis(IDLE_DELAY))
        );
        live
    }

    #[test]
    fn an_idle_delay_fires_once_it_runs_out() {
        smol::block_on(async {
            let fixture = Fixture::new();
            let live = idle_delay_started(&fixture).await;

            fixture.clock.advance(IDLE_DELAY);
            let firing = live.firing(NUDGE, FiringStatus::Completed).await;

            assert_eq!(firing.trigger, TriggerKind::Idle);
            live.stop().await;
        });
    }

    #[test]
    fn a_busy_session_cancels_the_idle_delay() {
        smol::block_on(async {
            let fixture = Fixture::new();
            let live = idle_delay_started(&fixture).await;

            live.signal(SessionSignal::Busy {
                blockers: vec![SettleBlocker::Busy],
            });
            live.barrier().await;
            fixture.clock.advance(IDLE_DELAY);
            live.barrier().await;

            assert_eq!(live.snapshot(NUDGE).triggers[0].after_until, None);
            assert!(live.firings(NUDGE).is_empty(), "{BUSY_CANCELS}");
            assert_eq!(live.handle.state().session.blockers, [SettleBlocker::Busy]);
            live.stop().await;
        });
    }

    #[test]
    fn needs_input_fires_once_per_wait_and_a_repeat_restarts_its_delay() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.script(WAITER, &[NEEDS_PERMISSION], &[], NOTIFY_BODY);
            let live = fixture.spawn(&[WAITER]).await;
            let ask = || live.signal(permission(TOOL));
            let half = INPUT_DELAY / 2;

            ask();
            live.barrier().await;
            fixture.clock.advance(half);
            ask();
            live.barrier().await;
            assert_eq!(
                live.snapshot(WAITER).triggers[0].after_until,
                Some(START_MS + millis(half + INPUT_DELAY))
            );
            fixture.clock.advance(half);
            live.barrier().await;
            assert!(live.firings(WAITER).is_empty(), "{REPEAT_RESTARTS}");
            fixture.clock.advance(half);
            live.firing(WAITER, FiringStatus::Completed).await;
            ask();
            live.barrier().await;
            fixture.clock.advance(INPUT_DELAY);
            live.barrier().await;
            assert_eq!(live.firings(WAITER).len(), 1, "{ONCE_PER_WAIT}");

            live.signal(SessionSignal::InputResolved);
            ask();
            live.barrier().await;
            fixture.clock.advance(INPUT_DELAY);
            live.firing(WAITER, FiringStatus::Completed).await;

            assert_eq!(live.firings(WAITER).len(), 2);
            live.stop().await;
        });
    }

    #[test]
    fn a_needs_input_event_carries_its_tool_and_another_tool_is_a_new_wait() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.script(WAITER, &[NEEDS_PERMISSION], &[], TOOL_BODY);
            let live = fixture.spawn(&[WAITER]).await;

            live.signal(permission(TOOL));
            live.barrier().await;
            fixture.clock.advance(INPUT_DELAY);
            live.firing(WAITER, FiringStatus::Completed).await;
            live.signal(permission(OTHER_TOOL));
            live.barrier().await;
            fixture.clock.advance(INPUT_DELAY);
            live.firing(WAITER, FiringStatus::Completed).await;

            assert_eq!(
                live.state_of(WAITER).await.value,
                seen(&[TOOL.to_owned(), OTHER_TOOL.to_owned()]),
                "{TOOL_CARRIED}"
            );
            live.stop().await;
        });
    }

    #[test]
    fn a_schedule_fires_on_time_and_not_again_after_a_restart() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.script(HOURLY, &[EVERY_HOUR], &[IN_UTC], RECORD_BODY);
            let first = fixture.spawn(&[HOURLY]).await;
            assert_eq!(
                first.snapshot(HOURLY).triggers[0].next_due,
                Some(START_MS + millis(SCHEDULE_PERIOD))
            );
            fixture.clock.advance(SCHEDULE_PERIOD);
            let fired = first.firing(HOURLY, FiringStatus::Completed).await;
            first.stop().await;

            let second = fixture.spawn(&[]).await;
            second.barrier().await;

            assert_eq!(fired.trigger, TriggerKind::Schedule);
            assert_eq!(second.firings(HOURLY).len(), 1, "{NO_REFIRE}");
            assert_eq!(
                second.snapshot(HOURLY).triggers[0].next_due,
                Some(START_MS + 2 * millis(SCHEDULE_PERIOD))
            );
            assert_eq!(
                second.state_of(HOURLY).await.value,
                seen(&[spelled(TriggerKind::Schedule)])
            );
            second.stop().await;
        });
    }

    #[test]
    fn an_arming_waits_for_the_session_record_and_fires_once_it_is_saved() {
        smol::block_on(async {
            let mut fixture = Fixture::unsaved();
            fixture.script(GREETER, &[ARMED], &[], RECORD_BODY);
            let live = fixture.spawn(&[]).await;
            let arm = AutomationRequest::Arm {
                name: GREETER.into(),
                args: None,
                origin: ArmOrigin::Manual,
            };

            let parked = live.handle.request(arm.clone()).await.unwrap();
            live.handle.request(arm).await.unwrap();

            assert!(
                matches!(parked, AutomationResponse::Automation(snapshot) if snapshot.armed.is_none()),
                "{PARKED}"
            );
            assert_eq!(live.save_requests(), 1, "{ASKS_ONCE}");
            assert!(live.handle.state().recent.is_empty(), "{NO_WRITES}");
            fixture.save();
            live.signal(SessionSignal::Saved);
            live.firing(GREETER, FiringStatus::Completed).await;

            assert_eq!(live.snapshot(GREETER).armed, Some(ArmOrigin::Manual));
            assert_eq!(
                live.state_of(GREETER).await.value,
                seen(&[spelled(ArmedReason::Manual)])
            );
            live.stop().await;
        });
    }

    #[test]
    fn state_edits_conflict_on_a_stale_revision_and_a_clear_empties_the_state() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.script(LEDGER, &[GOAL_FINISHED], &[], NOTIFY_BODY);
            let live = fixture.spawn(&[LEDGER]).await;
            let set = |state: Value, expected_revision: u64| AutomationRequest::SetState {
                name: LEDGER.into(),
                state,
                expected_revision,
            };

            let written = live.handle.request(set(seen(&[GOAL.to_owned()]), 0)).await;
            let stale = live.handle.request(set(seen(&[]), 0)).await;
            let oversized = live
                .handle
                .request(set(seen(&["x".repeat(MAX_STATE_BYTES)]), 1))
                .await;
            let cleared = live
                .handle
                .request(AutomationRequest::ClearState {
                    name: LEDGER.into(),
                    expected_revision: 1,
                })
                .await;

            assert_eq!(written, Ok(AutomationResponse::State { revision: 1 }));
            assert_eq!(
                stale,
                Err(AutomationError::StateConflict {
                    name: LEDGER.into(),
                    current: 1
                })
            );
            assert!(
                matches!(oversized, Err(AutomationError::Invalid { .. })),
                "{STATE_CAPPED}"
            );
            assert_eq!(cleared, Ok(AutomationResponse::State { revision: 2 }));
            let state = live.state_of(LEDGER).await;
            assert_eq!((state.value, state.revision), (json!({}), 2));
            live.stop().await;
        });
    }

    /// Boots with the profile arming [`BRIEFED`] with [`SEED`], then edits its args to
    /// [`EDITED`], which arms it again with reason `manual` and keeps the profile's origin.
    async fn edited_after_profile_arming(fixture: &Fixture) -> Live {
        fixture.script(BRIEFED, &[ARMED], &[TOPIC_ARGS], RECORD_BODY);
        let live = fixture
            .launch(vec![launching(BRIEFED, Some(SEED), ArmOrigin::Profile)])
            .await;
        live.firing(BRIEFED, FiringStatus::Completed).await;
        let edit = AutomationRequest::SetArgs {
            name: BRIEFED.into(),
            args: topic(EDITED),
        };
        live.handle.request(edit).await.unwrap();
        live.firing(BRIEFED, FiringStatus::Completed).await;
        live
    }

    #[test_case(ArmOrigin::Profile, Some(SEED), ArmedReason::Resume, EDITED, ArmOrigin::Profile; "profile_args_leave_the_stored_ones")]
    #[test_case(ArmOrigin::Cli, None, ArmedReason::Resume, EDITED, ArmOrigin::Profile; "a_bare_command_line_entry_resumes")]
    #[test_case(ArmOrigin::Cli, Some(GIVEN), ArmedReason::Launch, GIVEN, ArmOrigin::Cli; "command_line_args_arm_anew")]
    fn a_restart_keeps_the_stored_args_unless_the_command_line_gives_new_ones(
        origin: ArmOrigin,
        given: Option<&str>,
        reason: ArmedReason,
        args: &str,
        armed: ArmOrigin,
    ) {
        smol::block_on(async {
            let fixture = Fixture::new();
            edited_after_profile_arming(&fixture).await.stop().await;

            let live = fixture
                .launch(vec![launching(BRIEFED, given, origin)])
                .await;
            live.firing(BRIEFED, FiringStatus::Completed).await;

            let snapshot = live.snapshot(BRIEFED);
            assert_eq!(
                (snapshot.armed, snapshot.args),
                (Some(armed), Some(topic(args))),
                "{STORED_ARGS_WIN}"
            );
            let reasons = [ArmedReason::Launch, ArmedReason::Manual, reason].map(spelled);
            assert_eq!(
                live.state_of(BRIEFED).await.value,
                seen(&reasons),
                "{ARMED_ONCE}"
            );
            live.stop().await;
        });
    }

    #[test]
    fn a_profile_seeds_a_new_binding_ahead_of_a_bare_command_line_entry() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.script(BRIEFED, &[ARMED], &[TOPIC_ARGS], RECORD_BODY);

            let live = fixture
                .launch(vec![
                    launching(BRIEFED, None, ArmOrigin::Cli),
                    launching(BRIEFED, Some(SEED), ArmOrigin::Profile),
                ])
                .await;
            live.firing(BRIEFED, FiringStatus::Completed).await;

            let snapshot = live.snapshot(BRIEFED);
            assert_eq!(
                (snapshot.armed, snapshot.args),
                (Some(ArmOrigin::Profile), Some(topic(SEED))),
                "{PROFILE_SEEDS}"
            );
            live.stop().await;
        });
    }

    #[test]
    fn a_resumed_profile_arming_keeps_its_schedule_marks_and_failure_backoff() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.script(
                ROUTINE,
                &[ARMED, IDLE, EVERY_HOUR],
                &[IN_UTC],
                &failing_on_idle_body(),
            );
            let profile = || vec![launching(ROUTINE, None, ArmOrigin::Profile)];
            let first = fixture.launch(profile()).await;
            first.firing(ROUTINE, FiringStatus::Completed).await;
            first.signal(settled());
            first.firing(ROUTINE, FiringStatus::Failed).await;
            first.barrier().await;
            fixture.clock.advance(SCHEDULE_PERIOD);
            first.firing(ROUTINE, FiringStatus::Completed).await;
            first.stop().await;
            fixture.clock.advance(LATER);

            let second = fixture.launch(profile()).await;
            second.firing(ROUTINE, FiringStatus::Completed).await;
            second.barrier().await;

            let reasons = [
                spelled(ArmedReason::Launch),
                spelled(TriggerKind::Schedule),
                spelled(ArmedReason::Resume),
            ];
            assert_eq!(
                second.state_of(ROUTINE).await.value,
                seen(&reasons),
                "{ARMED_ONCE}"
            );
            let snapshot = second.snapshot(ROUTINE);
            let backoff = ActingMarks {
                acting: Vec::new(),
                failure_streak: 1,
                backoff_until: Some(START_MS + BACKOFF_BASE_MS),
            };
            assert_eq!(snapshot.limiter, backoff, "{MARKS_KEPT}");
            assert_eq!(
                snapshot.triggers[2].next_due,
                Some(START_MS + 2 * millis(SCHEDULE_PERIOD)),
                "{MARKS_KEPT}"
            );
            assert_eq!(
                second
                    .firings(ROUTINE)
                    .iter()
                    .filter(|firing| firing.trigger == TriggerKind::Schedule)
                    .count(),
                1,
                "{NO_REFIRE}"
            );
            second.stop().await;
        });
    }

    #[test_case(&[TOPIC_ARGS], Some(ArmOrigin::Profile), None; "for_a_profile_entry")]
    #[test_case(&[TOPIC_ARGS, ALWAYS], None, None; "for_an_always_script")]
    #[test_case(&[TOPIC_ARGS], Some(ArmOrigin::Cli), Some(ArmOrigin::Cli); "but_a_command_line_entry_arms_it")]
    fn a_disarmed_binding_stays_disarmed_unless_the_command_line_names_it(
        fields: &[&str],
        origin: Option<ArmOrigin>,
        armed: Option<ArmOrigin>,
    ) {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.script(BRIEFED, &[ARMED], fields, RECORD_BODY);
            let first = fixture
                .launch(vec![launching(BRIEFED, Some(SEED), ArmOrigin::Cli)])
                .await;
            first.firing(BRIEFED, FiringStatus::Completed).await;
            first.handle.request(disarm(BRIEFED)).await.unwrap();
            first.stop().await;

            let launch = origin.map(|origin| launching(BRIEFED, None, origin));
            let live = fixture.launch(launch.into_iter().collect()).await;
            let mut reasons = vec![spelled(ArmedReason::Launch)];
            if armed.is_some() {
                live.firing(BRIEFED, FiringStatus::Completed).await;
                reasons.push(spelled(ArmedReason::Launch));
            }

            let snapshot = live.snapshot(BRIEFED);
            assert_eq!(
                (snapshot.armed, snapshot.args),
                (armed, Some(topic(SEED))),
                "{DISARM_HOLDS}"
            );
            assert_eq!(
                live.state_of(BRIEFED).await.value,
                seen(&reasons),
                "{DISARM_HOLDS}"
            );
            live.stop().await;
        });
    }

    #[test]
    fn a_binding_only_the_sdk_refuses_stays_armed_for_the_tui() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.script(BRIEFED, &[ARMED, NEEDS_PERMISSION], &[], RECORD_BODY);
            let tui = fixture.spawn(&[BRIEFED]).await;
            tui.firing(BRIEFED, FiringStatus::Completed).await;
            tui.stop().await;

            let sdk = boot(RuntimeDeps {
                frontend: Frontend::Sdk,
                ..fixture.deps(&[])
            })
            .await;
            sdk.barrier().await;
            let refused = sdk.snapshot(BRIEFED);
            assert_eq!(refused.armed, None, "{KEPT_FOR_TUI}");
            assert!(
                matches!(&refused.availability, Availability::Invalid { reason } if reason.ends_with(UNAVAILABLE_IN_SDK)),
                "{KEPT_FOR_TUI}: {:?}",
                refused.availability
            );
            sdk.stop().await;

            let resumed = fixture.spawn(&[]).await;
            resumed.firing(BRIEFED, FiringStatus::Completed).await;
            assert_eq!(
                resumed.state_of(BRIEFED).await.value,
                seen(&[spelled(ArmedReason::Launch), spelled(ArmedReason::Resume)]),
                "{KEPT_FOR_TUI}"
            );
            resumed.stop().await;
        });
    }

    #[test]
    fn the_same_profile_list_again_changes_nothing() {
        smol::block_on(async {
            let fixture = Fixture::new();
            let live = edited_after_profile_arming(&fixture).await;
            let profile =
                || SessionSignal::ProfileAutomations(vec![profile_arming(BRIEFED, Some(SEED))]);

            live.signal(profile());
            live.barrier().await;
            let snapshot = live.snapshot(BRIEFED);
            assert_eq!(
                (snapshot.armed, snapshot.args),
                (Some(ArmOrigin::Profile), Some(topic(EDITED))),
                "{SAME_PROFILE_IDLE}"
            );
            assert_eq!(live.firings(BRIEFED).len(), 2, "{SAME_PROFILE_IDLE}");
            live.handle.request(disarm(BRIEFED)).await.unwrap();
            live.signal(profile());
            live.barrier().await;

            assert_eq!(live.snapshot(BRIEFED).armed, None, "{SAME_PROFILE_IDLE}");
            live.stop().await;
        });
    }

    #[test]
    fn switching_back_to_a_profile_rearms_with_the_stored_args() {
        smol::block_on(async {
            let fixture = Fixture::new();
            let live = edited_after_profile_arming(&fixture).await;

            live.signal(SessionSignal::ProfileAutomations(Vec::new()));
            live.barrier().await;
            assert_eq!(live.snapshot(BRIEFED).armed, None);
            live.signal(SessionSignal::ProfileAutomations(vec![profile_arming(
                BRIEFED,
                Some(SEED),
            )]));
            live.firing(BRIEFED, FiringStatus::Completed).await;

            let snapshot = live.snapshot(BRIEFED);
            assert_eq!(
                (snapshot.armed, snapshot.args),
                (Some(ArmOrigin::Profile), Some(topic(EDITED))),
                "{SWITCH_BACK_REARMS}"
            );
            let reasons = [
                ArmedReason::Launch,
                ArmedReason::Manual,
                ArmedReason::Launch,
            ]
            .map(spelled);
            assert_eq!(live.state_of(BRIEFED).await.value, seen(&reasons));
            live.stop().await;
        });
    }

    #[test_case(SessionControls { turn_window: TurnWindow { turns: vec![START_MS] }, ..SessionControls::default() }, None, TURN_WINDOW; "a_full_turn_window")]
    #[test_case(SessionControls { delivery_backoff: DeliveryBackoff { errors: 1, until: Some(START_MS + BACKOFF_BASE_MS) }, ..SessionControls::default() }, None, BACKOFF; "the_delivery_backoff")]
    #[test_case(SessionControls::default(), Some(EXPIRY), EXPIRY; "an_expiry")]
    fn a_wait_that_ran_out_since_the_last_tick_wakes_the_runtime_at_once(
        controls: SessionControls,
        expires: Option<Duration>,
        lapse: Duration,
    ) {
        smol::block_on(async {
            let fixture = Fixture::new();
            let mut deps = fixture.deps(&[]);
            deps.config.turns_per_hour = TURNS_PER_HOUR;
            let (manager, _boot) = Manager::open(deps).await.unwrap();
            let ticked_at = fixture.clock.now_ms();
            let journal = ActionRequest::Message(MessageRequest {
                text: MESSAGE.into(),
                attach: None,
                delivery: DeliveryMode::Next,
                expires,
            })
            .to_journal();
            let expires_at = expires.map(|expires| ticked_at + millis(expires));
            let pending =
                Pending::from_journal(COURIER, FIRST, 0, &journal, ticked_at, expires_at).unwrap();
            {
                let mut outbox = manager.shared.outbox.lock().await;
                outbox.controls = controls;
                outbox.push(pending);
            }
            fixture.clock.advance(lapse);

            let wake = manager.next_wake(ticked_at).await;

            assert_eq!(wake, Some(fixture.clock.monotonic()), "{WAKES_AT_ONCE}");
            manager.shared.store.clone().shutdown().await;
        });
    }

    fn set_env(name: &str, value: &str) {
        // SAFETY: test-only variables with names no other test reads, each always set to one value.
        unsafe { env::set_var(name, value) };
    }

    /// Sends one GET to `url`.
    fn get_body(url: &str) -> String {
        format!(r#"http(#{{ method: "GET", url: "{url}" }});"#)
    }

    /// Runs `request` and records the failure it throws in `state`.
    fn catching(request: &str) -> String {
        format!(
            "try {{ {request}; }} catch (err) {{ state.kind = err.kind; state.message = err.message; }}"
        )
    }

    /// The state [`catching`] leaves after a failure of `kind`.
    fn caught(kind: FailureKind, message: &str) -> Value {
        json!({ "kind": kind.as_str(), "message": { UNTRUSTED_TAG: message } })
    }

    /// A stop for policy the host enforces, as a firing's error shows it.
    fn refused(message: String) -> (String, String) {
        (
            ErrorKind::Stop(StopKind::Refused).as_str().to_owned(),
            message,
        )
    }

    /// The error [`FETCHER`]'s firing stopped with, before any request reached the client.
    async fn stopped_before_sending(
        live: &Live,
        requests: &flume::Receiver<HeldRequest>,
    ) -> (String, String) {
        let error = live
            .firing(FETCHER, FiringStatus::Failed)
            .await
            .error
            .expect(NO_ERROR);
        assert!(requests.is_empty(), "{NO_IO}");
        (error.kind, error.message)
    }

    /// Every secret a request carries, as a careless server or client echoes them: the URL and
    /// each header line as string values, and each header value as a key.
    fn echo(call: &HttpCall) -> String {
        let lines: Vec<String> = call
            .headers
            .iter()
            .map(|(name, value)| format!("{name}: {value}"))
            .collect();
        let values: Map<String, Value> = call
            .headers
            .iter()
            .map(|(name, value)| (value.clone(), Value::String(name.clone())))
            .collect();
        json!({ ECHO_URL: call.url.as_str(), ECHO_LINES: lines, ECHO_VALUES: values }).to_string()
    }

    #[test_case(JSON_PAYLOAD, JSON_SENT, Some(JSON_TYPE); "with_json")]
    #[test_case(TEXT_PAYLOAD, TEXT_SENT, None; "with_a_text_body")]
    fn a_request_reaches_the_client_resolved_and_encoded(
        payload: &str,
        sent: &str,
        content_type: Option<&str>,
    ) {
        smol::block_on(async {
            set_env(SHAPE_TOKEN_VAR, SHAPE_TOKEN);
            set_env(SHAPE_KEY_VAR, SHAPE_KEY);
            let fixture = Fixture::new();
            let body = format!(
                r#"http(#{{ method: "post", url: "{API_URL}", query: {QUERY}, headers: #{{ "{NOTE_HEADER}": "{NOTE}" }}, bearer_env: "{SHAPE_TOKEN_VAR}", secret_headers: #{{ "{KEY_HEADER}": "{SHAPE_KEY_VAR}" }}, {payload}, timeout: "{HTTP_TIMEOUT_TEXT}" }});"#
            );
            fixture.fetcher(&[API_ORIGIN], &[SHAPE_TOKEN_VAR, SHAPE_KEY_VAR], &body);
            let (deps, requests) = fixture.fetching();
            let live = boot(deps).await;

            let held = requests.recv_async().await.expect(NOT_SENT);
            held.respond(OK_STATUS, "");
            live.firing(FETCHER, FiringStatus::Completed).await;

            let mut headers = vec![
                (NOTE_HEADER.to_owned(), NOTE.to_owned()),
                (KEY_HEADER.to_owned(), SHAPE_KEY.to_owned()),
                (AUTHORIZATION.to_owned(), format!("{BEARER} {SHAPE_TOKEN}")),
            ];
            headers.extend(content_type.map(|media| (CONTENT_TYPE.to_owned(), media.to_owned())));
            headers.push((USER_AGENT.to_owned(), user_agent().to_owned()));
            let call = held.call;
            assert_eq!(
                (call.method, call.url.as_str()),
                (HttpMethod::Post, ENCODED_URL)
            );
            assert_eq!(call.headers, headers);
            assert_eq!(call.body.as_deref(), Some(sent.as_bytes()));
            assert_eq!(
                (
                    call.timeout,
                    call.allow_private_network,
                    call.max_response_bytes
                ),
                (HTTP_TIMEOUT, false, MAX_RESPONSE_BYTES)
            );
            live.stop().await;
        });
    }

    #[test]
    fn a_url_env_target_is_resolved_on_the_host_and_journaled_as_its_origin() {
        smol::block_on(async {
            set_env(DECLARED_URL_VAR, HOOK_URL);
            let fixture = Fixture::new();
            let body = format!(r#"http(#{{ method: "GET", url_env: "{DECLARED_URL_VAR}" }});"#);
            fixture.fetcher(&[HOOK_ORIGIN], &[DECLARED_URL_VAR], &body);
            let (deps, requests) = fixture.fetching();
            let live = boot(deps).await;

            let held = requests.recv_async().await.expect(NOT_SENT);
            held.respond(OK_STATUS, "");
            let completed = live.firing(FETCHER, FiringStatus::Completed).await;

            assert_eq!(held.call.url.as_str(), HOOK_URL);
            let action = live.trace(&completed.fire_id).await.remove(0);
            assert_eq!(action.target.as_deref(), Some(HOOK_ORIGIN));
            live.stop().await;
        });
    }

    #[test]
    fn a_url_env_outside_meta_network_stops_the_firing_before_any_io() {
        smol::block_on(async {
            set_env(UNDECLARED_URL_VAR, ELSEWHERE_URL);
            let fixture = Fixture::new();
            let body = format!(r#"http(#{{ method: "GET", url_env: "{UNDECLARED_URL_VAR}" }});"#);
            fixture.fetcher(&[HOOK_ORIGIN], &[UNDECLARED_URL_VAR], &body);
            let (deps, requests) = fixture.fetching();
            let live = boot(deps).await;

            let error = stopped_before_sending(&live, &requests).await;

            assert_eq!(
                error,
                refused(format!("{UNDECLARED_ORIGIN} {ELSEWHERE_ORIGIN}"))
            );
            live.stop().await;
        });
    }

    #[test]
    fn a_private_target_stops_the_firing_without_allow_private_network() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.fetcher(&[LOOPBACK_ORIGIN], &[], &get_body(LOOPBACK_URL));
            let (deps, requests) = fixture.fetching();
            let live = boot(deps).await;

            let error = stopped_before_sending(&live, &requests).await;

            assert_eq!(
                error,
                refused(format!("{PRIVATE_TARGET} {LOOPBACK_ORIGIN}"))
            );
            live.stop().await;
        });
    }

    #[test]
    fn allow_private_network_lets_a_private_target_through_and_reaches_the_client() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.fetcher(&[LOOPBACK_ORIGIN], &[], &get_body(LOOPBACK_URL));
            let (mut deps, requests) = fixture.fetching();
            deps.config.allow_private_network = true;
            let live = boot(deps).await;

            let held = requests.recv_async().await.expect(NOT_SENT);
            held.respond(OK_STATUS, "");
            live.firing(FETCHER, FiringStatus::Completed).await;

            assert_eq!(
                (held.call.url.as_str(), held.call.allow_private_network),
                (LOOPBACK_URL, true)
            );
            live.stop().await;
        });
    }

    #[test_case("", &format!(r#"bearer_env: "{UNSET_VAR}""#), FailureKind::Refused, &format!("{UNSET_VAR} {UNSET_SECRET}"); "an_unset_secret")]
    #[test_case("", &format!(r#"headers: #{{ "{BAD_HEADER_NAME}": "{NOTE}" }}"#), FailureKind::InvalidArgument, &format!("{BAD_HEADER_NAME:?} {INVALID_HEADER_NAME}"); "an_invalid_header_name")]
    #[test_case("", &format!(r#"headers: #{{ {AUTHORIZATION}: "{NOTE}" }}, bearer_env: "{CONFLICT_VAR}""#), FailureKind::InvalidArgument, BEARER_CONFLICT; "authorization_beside_bearer_env")]
    #[test_case(BIG_TEXT, BIG_PAYLOAD, FailureKind::InvalidArgument, &format!("{BODY_TOO_LARGE} {MAX_REQUEST_BODY_BYTES} bytes"); "a_body_past_its_limit")]
    fn a_request_the_host_cannot_send_fails_catchably_before_any_io(
        prelude: &str,
        options: &str,
        kind: FailureKind,
        message: &str,
    ) {
        smol::block_on(async {
            let fixture = Fixture::new();
            let request = format!(r#"http(#{{ method: "POST", url: "{API_URL}", {options} }})"#);
            let body = format!("{prelude}\n{}", catching(&request));
            fixture.fetcher(&[API_ORIGIN], &[UNSET_VAR, CONFLICT_VAR], &body);
            let (deps, requests) = fixture.fetching();
            let live = boot(deps).await;

            live.firing(FETCHER, FiringStatus::Completed).await;

            assert!(requests.is_empty(), "{NO_IO}");
            assert_eq!(live.state_of(FETCHER).await.value, caught(kind, message));
            live.stop().await;
        });
    }

    #[test]
    fn any_status_reaches_the_script_and_the_journal_with_its_body_and_json() {
        smol::block_on(async {
            let fixture = Fixture::new();
            let body = format!(r#"state.response = http(#{{ method: "GET", url: "{API_URL}" }});"#);
            fixture.fetcher(&[API_ORIGIN], &[], &body);
            let (deps, requests) = fixture.fetching();
            let live = boot(deps).await;

            let held = requests.recv_async().await.expect(NOT_SENT);
            held.respond(NOT_FOUND, MISSING_BODY);
            let completed = live.firing(FETCHER, FiringStatus::Completed).await;

            let parsed: Value = serde_json::from_str(MISSING_BODY).unwrap();
            assert_eq!(
                live.state_of(FETCHER).await.value,
                json!({ "response": {
                    "status": NOT_FOUND,
                    "body": { UNTRUSTED_TAG: MISSING_BODY },
                    "json": { UNTRUSTED_TAG: parsed },
                } })
            );
            let action = live.trace(&completed.fire_id).await.remove(0);
            assert_eq!(
                (action.status, action.target.as_deref()),
                (ActionStatus::Done, Some(API_ORIGIN))
            );
            let journaled = live.action_body(&completed.fire_id, action.seq).await;
            assert_eq!(
                journaled.result,
                Some(json!({ "status": NOT_FOUND, "body": MISSING_BODY, "json": parsed }))
            );
            live.stop().await;
        });
    }

    #[test_case(Some(HttpErrorKind::Transport), FailureKind::Transport; "a_transport_error")]
    #[test_case(Some(HttpErrorKind::Timeout), FailureKind::Timeout; "the_clients_timeout")]
    #[test_case(Some(HttpErrorKind::Refused), FailureKind::Refused; "a_private_address_refused_at_connect")]
    #[test_case(Some(HttpErrorKind::InvalidArgument), FailureKind::InvalidArgument; "a_request_the_client_will_not_send")]
    #[test_case(None, FailureKind::Timeout; "the_hosts_deadline")]
    fn a_request_without_a_response_fails_catchably(
        error: Option<HttpErrorKind>,
        kind: FailureKind,
    ) {
        smol::block_on(async {
            let fixture = Fixture::new();
            let request = format!(
                r#"http(#{{ method: "GET", url: "{API_URL}", timeout: "{HTTP_TIMEOUT_TEXT}" }})"#
            );
            fixture.fetcher(&[API_ORIGIN], &[], &catching(&request));
            let (deps, requests) = fixture.fetching();
            let live = boot(deps).await;
            let held = requests.recv_async().await.expect(NOT_SENT);

            match error {
                Some(error) => held.answer(Err(HttpError::new(error, CLIENT_ERROR))),
                None => fixture.clock.advance(HTTP_TIMEOUT),
            }
            live.firing(FETCHER, FiringStatus::Completed).await;

            let message = error.map_or_else(
                || format!("{TIMED_OUT} {HTTP_TIMEOUT:?}"),
                |_| CLIENT_ERROR.to_owned(),
            );
            assert_eq!(live.state_of(FETCHER).await.value, caught(kind, &message));
            live.stop().await;
        });
    }

    #[test]
    fn a_request_timeout_is_cut_to_what_the_firings_wall_time_has_left() {
        smol::block_on(async {
            let fixture = Fixture::new();
            let body = format!("{HOLD_BODY}\n{}", get_body(API_URL));
            fixture.fetcher(&[API_ORIGIN], &[], &body);
            let (deps, requests) = fixture.fetching();
            let live = boot(deps).await;
            live.held().await;

            fixture.clock.advance(SPENT);
            live.release();
            let held = requests.recv_async().await.expect(NOT_SENT);
            held.respond(OK_STATUS, "");
            live.firing(FETCHER, FiringStatus::Completed).await;

            assert_eq!(held.call.timeout, DEFAULT_WALL_TIME - SPENT);
            live.stop().await;
        });
    }

    #[test]
    fn the_actor_serves_requests_and_claims_while_a_request_is_in_flight() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.fetcher(&[API_ORIGIN], &[], &get_body(API_URL));
            let (deps, requests) = fixture.fetching();
            let live = boot(deps).await;
            let held = requests.recv_async().await.expect(NOT_SENT);

            live.barrier().await;
            let claim = live.claim(fixture.gate()).await;
            let running = live.running();
            held.respond(OK_STATUS, "");
            live.firing(FETCHER, FiringStatus::Completed).await;

            assert!(claim.is_none(), "{SERVED}");
            assert_eq!(running, [FETCHER], "{SERVED}");
            live.stop().await;
        });
    }

    #[test_case(Some(AutomationRequest::Pause { by: PauseSource::Inspector }), Interruption::Paused, FiringStatus::Cancelled; "a_pause")]
    #[test_case(Some(disarm(FETCHER)), Interruption::Disarmed, FiringStatus::Cancelled; "a_disarm")]
    #[test_case(None, Interruption::Shutdown, FiringStatus::Interrupted; "a_shutdown")]
    fn a_stop_cancels_the_request_in_flight_and_ends_its_firing(
        stop: Option<AutomationRequest>,
        interruption: Interruption,
        status: FiringStatus,
    ) {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.fetcher(&[API_ORIGIN], &[], &get_body(API_URL));
            let (deps, requests) = fixture.fetching();
            let live = boot(deps).await;
            let held = requests.recv_async().await.expect(NOT_SENT);
            let fire_id = live.firings(FETCHER).remove(0).fire_id;

            if let Some(request) = stop {
                live.handle.request(request).await.unwrap();
            }
            live.stop().await;

            assert!(held.reply.is_disconnected(), "{CANCELLED}");
            let journaled = fixture.journaled(&fire_id).await;
            assert_eq!(journaled.firing.status, status);
            let action = journaled.actions.first().expect(NO_ACTION);
            assert_eq!(
                (
                    action.status,
                    action.error.clone(),
                    action.target.as_deref()
                ),
                (
                    ActionStatus::Interrupted,
                    Some(interruption.to_string()),
                    Some(API_ORIGIN)
                )
            );
        });
    }

    #[test]
    fn a_runtime_dropped_mid_request_cancels_the_request() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.fetcher(&[API_ORIGIN], &[], &get_body(API_URL));
            let (deps, requests) = fixture.fetching();
            let live = boot(deps).await;
            let held = requests.recv_async().await.expect(NOT_SENT);

            drop(live);

            held.cancelled().await;
            assert!(held.reply.is_disconnected(), "{CANCELLED}");
        });
    }

    #[test]
    fn no_secret_reaches_the_firing_its_state_the_journal_the_trace_the_mirror_or_an_error() {
        smol::block_on(async {
            set_env(LEAK_URL_VAR, LEAK_URL);
            set_env(LEAK_TOKEN_VAR, LEAK_TOKEN);
            set_env(LEAK_KEY_VAR, LEAK_KEY);
            let fixture = Fixture::new();
            let request = format!(
                r#"http(#{{ method: "POST", url_env: "{LEAK_URL_VAR}", query: {QUERY}, bearer_env: "{LEAK_TOKEN_VAR}", secret_headers: #{{ "{KEY_HEADER}": "{LEAK_KEY_VAR}" }}, {JSON_PAYLOAD} }})"#
            );
            let body = format!(
                "let response = {request};\nstate.{ECHO_KEY} = response.body;\nstate.{PARSED_KEY} = response.json;\n{}",
                catching(&request)
            );
            let variables = [LEAK_URL_VAR, LEAK_TOKEN_VAR, LEAK_KEY_VAR];
            fixture.fetcher(&[HOOK_ORIGIN], &variables, &body);
            let (deps, requests) = fixture.fetching();
            let live = boot(deps).await;

            let echoed = requests.recv_async().await.expect(NOT_SENT);
            echoed.respond(OK_STATUS, &echo(&echoed.call));
            let failing = requests.recv_async().await.expect(NOT_SENT);
            failing.answer(Err(HttpError::new(
                HttpErrorKind::Transport,
                echo(&failing.call),
            )));
            let completed = live.firing(FETCHER, FiringStatus::Completed).await;

            let state = live.state_of(FETCHER).await.value;
            let detail = live.detail(&completed.fire_id).await;
            let mut shown = vec![
                state.to_string(),
                format!("{completed:?}"),
                format!("{detail:?}"),
                format!("{:?}", live.handle.state()),
            ];
            let mut results = Vec::new();
            for action in &detail.actions {
                let body = live.action_body(&completed.fire_id, action.seq).await;
                shown.push(format!("{body:?}"));
                results.push(body.result);
            }
            shown.extend(live.events.try_iter().map(|event| format!("{event:?}")));
            for text in &shown {
                for secret in [LEAK_PATH, LEAK_TOKEN, LEAK_KEY] {
                    assert!(!text.contains(secret), "{LEAKED}: {text}");
                }
            }
            let bearer = format!("{BEARER} {LEAK_TOKEN_SHOWN}");
            let agent = user_agent();
            let parsed = json!({
                ECHO_URL: HOOK_ORIGIN,
                ECHO_LINES: [
                    format!("{KEY_HEADER}: {LEAK_KEY_SHOWN}"),
                    format!("{AUTHORIZATION}: {bearer}"),
                    format!("{CONTENT_TYPE}: {JSON_TYPE}"),
                    format!("{USER_AGENT}: {agent}"),
                ],
                ECHO_VALUES: {
                    LEAK_KEY_SHOWN: KEY_HEADER,
                    bearer: AUTHORIZATION,
                    JSON_TYPE: CONTENT_TYPE,
                    agent: USER_AGENT,
                },
            });
            assert_eq!(state[PARSED_KEY][UNTRUSTED_TAG], parsed, "{REDACTED}");
            assert_eq!(
                results.first().cloned().flatten(),
                Some(json!({
                    "status": OK_STATUS,
                    "body": state[ECHO_KEY][UNTRUSTED_TAG],
                    "json": parsed,
                })),
                "{SAME_RESPONSE}"
            );
            let targets: Vec<Option<&str>> = detail
                .actions
                .iter()
                .map(|action| action.target.as_deref())
                .collect();
            assert_eq!(targets, [Some(HOOK_ORIGIN); 2]);
            assert_eq!(state[KIND_KEY], FailureKind::Transport.as_str());
            let caught = state[MESSAGE_KEY][UNTRUSTED_TAG]
                .as_str()
                .unwrap_or_default();
            let error = detail.actions[1].error.clone().unwrap_or_default();
            for message in [caught, error.as_str()] {
                assert!(
                    [HOOK_ORIGIN, LEAK_TOKEN_SHOWN, LEAK_KEY_SHOWN]
                        .iter()
                        .all(|shown| message.contains(shown)),
                    "{REDACTED}: {message}"
                );
            }
            live.stop().await;
        });
    }

    #[test]
    fn http_without_a_client_stops_the_firing_as_unavailable() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.fetcher(&[API_ORIGIN], &[], &get_body(API_URL));
            let live = fixture.spawn(&[FETCHER]).await;

            let stopped = live.firing(FETCHER, FiringStatus::Failed).await;

            let error = stopped.error.expect(NO_ERROR);
            assert_eq!(
                (error.kind, error.message),
                refused(format!(
                    "{}() {NOT_IN_THIS_BUILD}",
                    ActionKind::Http.as_str()
                ))
            );
            assert_eq!(
                live.actions(&stopped.fire_id).await,
                [ActionStatus::Refused]
            );
            live.stop().await;
        });
    }

    /// [`FakePeer`]'s queued message `number` from the session `@lead`.
    fn from_lead(peer: &FakePeer, number: usize) -> ObservedMessage {
        let sender = ObservedSender::Session {
            handle: LEAD.into(),
        };
        peer.message(number, sender, QUESTION, false)
    }

    fn settle_call(message: &ObservedMessage, automation: &str) -> MessagingCall {
        MessagingCall::Settle {
            key: message.key.clone(),
            automation: automation.to_owned(),
        }
    }

    /// The deliveries released among the calls made so far.
    fn releases(calls: &flume::Receiver<MessagingCall>) -> Vec<String> {
        calls
            .try_iter()
            .filter_map(|call| match call {
                MessagingCall::Release(delivery) => Some(delivery),
                _ => None,
            })
            .collect()
    }

    async fn next_release(calls: &flume::Receiver<MessagingCall>) -> String {
        loop {
            if let MessagingCall::Release(delivery) = calls.recv_async().await.expect(CALLS_OPEN) {
                return delivery;
            }
        }
    }

    #[test_case("", FiringStatus::Completed, false; "completed")]
    #[test_case(SKIP_BODY, FiringStatus::Skipped, false; "skipped")]
    #[test_case(RELEASE_BODY, FiringStatus::Released, true; "released")]
    #[test_case(&failing_body(), FiringStatus::Failed, true; "failed")]
    #[test_case(UNDECLARED_SEND, FiringStatus::Failed, true; "stopped")]
    #[test_case(PAUSED_REPLY, FiringStatus::Cancelled, true; "paused_before_its_reply")]
    fn a_consumed_message_is_kept_or_handed_back_by_how_its_firing_ends(
        body: &str,
        status: FiringStatus,
        hands_back: bool,
    ) {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.script(TAKER, &[CONSUMING], &[MESSAGING], body);
            let live = fixture.spawn(&[TAKER]).await;
            let calls = live.attach(Answers::default());
            let message = from_lead(&FakePeer::default(), 1);

            let taker = live.offer(&message);
            let ended = live.firing(TAKER, status).await;

            let mut expected = vec![settle_call(&message, TAKER)];
            expected.extend(hands_back.then(|| MessagingCall::Release(delivery(&message))));
            assert_eq!((taker.as_deref(), ended.consumed), (Some(TAKER), true));
            assert_eq!(
                calls.try_iter().collect::<Vec<_>>(),
                expected,
                "{KEPT_OR_HANDED_BACK}"
            );
            live.stop().await;
        });
    }

    #[test_case(AutomationRequest::Pause { by: PauseSource::Inspector }, FiringStatus::Paused; "a_pause")]
    #[test_case(disarm(TAKER), FiringStatus::Dropped; "a_disarm")]
    fn a_pause_or_a_disarm_hands_back_the_running_and_the_waiting_messages(
        stop: AutomationRequest,
        waiting: FiringStatus,
    ) {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.script(TAKER, &[CONSUMING], &[], HOLD_BODY);
            let live = fixture.spawn(&[TAKER]).await;
            let calls = live.attach(Answers::default());
            let peer = FakePeer::default();
            let messages = [from_lead(&peer, 1), from_lead(&peer, 2)];
            live.offer(&messages[0]);
            live.held().await;
            live.offer(&messages[1]);

            live.handle.request(stop).await.unwrap();
            live.firing(TAKER, waiting).await;
            live.release();
            live.firing(TAKER, FiringStatus::Cancelled).await;

            let mut released = releases(&calls);
            released.sort_unstable();
            let mut expected = messages.map(|message| delivery(&message));
            expected.sort_unstable();
            assert_eq!(released, expected, "{KEPT_OR_HANDED_BACK}");
            assert_eq!(live.offer(&from_lead(&peer, 3)), None, "{NOTHING_TAKEN}");
            live.stop().await;
        });
    }

    #[test]
    fn an_overflowing_queue_hands_back_the_message_it_drops() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.script(TAKER, &[CONSUMING], &[], HOLD_BODY);
            let live = fixture.spawn(&[TAKER]).await;
            let calls = live.attach(Answers::default());
            let peer = FakePeer::default();
            live.offer(&from_lead(&peer, 0));
            live.held().await;
            let waiting: Vec<ObservedMessage> = (1..=MAX_QUEUED_EVENTS + 1)
                .map(|number| from_lead(&peer, number))
                .collect();

            for message in &waiting {
                live.offer(message);
            }
            let dropped = live.firing(TAKER, FiringStatus::Dropped).await;

            assert_eq!(dropped.reason.as_deref(), Some(QUEUE_FULL));
            assert_eq!(
                releases(&calls),
                [delivery(&waiting[0])],
                "{KEPT_OR_HANDED_BACK}"
            );
            live.stop().await;
        });
    }

    #[test]
    fn a_deferred_event_keeps_its_message_through_its_retry() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.script(TAKER, &[ARMED, CONSUMING], &[COOLDOWN], STEP_BODY);
            let live = fixture.spawn(&[TAKER]).await;
            live.firing(TAKER, FiringStatus::Completed).await;
            let calls = live.attach(Answers::default());
            let message = from_lead(&FakePeer::default(), 1);

            live.offer(&message);
            let deferred = live.firing(TAKER, FiringStatus::Deferred).await;
            fixture.clock.advance(COOLDOWN_DELAY);
            let retried = live.firing(TAKER, FiringStatus::Completed).await;

            assert_eq!(retried.fire_id, deferred.fire_id);
            assert_eq!(
                calls.try_iter().collect::<Vec<_>>(),
                [settle_call(&message, TAKER)],
                "{DEFERRAL_KEEPS}"
            );
            live.stop().await;
        });
    }

    #[test]
    fn the_first_consuming_automation_by_name_takes_the_message() {
        smol::block_on(async {
            let fixture = Fixture::new();
            for name in [RIVAL, TAKER] {
                fixture.script(name, &[CONSUMING], &[], CONSUMED_BODY);
            }
            let live = fixture.spawn(&[RIVAL, TAKER]).await;
            let calls = live.attach(Answers::default());
            let message = from_lead(&FakePeer::default(), 1);

            let taker = live.offer(&message);
            live.all_in(&[RIVAL, TAKER], FiringStatus::Completed).await;

            assert_eq!(taker.as_deref(), Some(TAKER), "{FIRST_NAME_TAKES}");
            assert_eq!(
                (
                    live.state_of(TAKER).await.value,
                    live.state_of(RIVAL).await.value
                ),
                (
                    json!({ CONSUMED_KEY: true }),
                    json!({ CONSUMED_KEY: false })
                ),
                "{FIRST_NAME_TAKES}"
            );
            assert_eq!(
                calls.try_iter().collect::<Vec<_>>(),
                [settle_call(&message, TAKER)]
            );
            live.stop().await;
        });
    }

    #[test]
    fn a_message_fires_once_per_admission_whatever_offers_it_again() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.script(ONLOOKER, &[ANY_ADMISSION], &[], COUNT_BODY);
            let live = fixture.spawn(&[ONLOOKER]).await;
            let held = FakePeer::default().message(
                1,
                ObservedSender::Session {
                    handle: LEAD.into(),
                },
                QUESTION,
                true,
            );
            let queued = ObservedMessage {
                held: false,
                ..held.clone()
            };
            let caught_up = ObservedMessage {
                catch_up: true,
                ..queued.clone()
            };

            for message in [&held, &held, &queued, &caught_up, &held] {
                live.offer(message);
            }
            for _ in 0..2 {
                live.firing(ONLOOKER, FiringStatus::Completed).await;
            }
            live.barrier().await;

            assert_eq!(live.firings(ONLOOKER).len(), 2, "{ONCE_PER_ADMISSION}");
            assert_eq!(
                live.state_of(ONLOOKER).await.value,
                json!({ COUNT_KEY: 2 }),
                "{ONCE_PER_ADMISSION}"
            );
            live.stop().await;
        });
    }

    #[test_case("", FiringStatus::Completed; "while_its_completed_firing_keeps_it")]
    #[test_case(RELEASE_BODY, FiringStatus::Released; "after_its_firing_released_it")]
    fn a_message_taken_again_is_settled_or_handed_back(body: &str, ended: FiringStatus) {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.script(TAKER, &[CONSUMING], &[], body);
            let message = from_lead(&FakePeer::default(), 1);
            let first = fixture.spawn(&[TAKER]).await;
            let _first_calls = first.attach(Answers::default());
            first.offer(&message);
            first.firing(TAKER, ended).await;
            first.stop().await;

            let second = fixture.spawn(&[TAKER]).await;
            let calls = second.attach(Answers::default());
            let taker = second.offer(&message);
            second.barrier().await;

            let again = if ended == FiringStatus::Released {
                MessagingCall::GiveBack(message.key.clone())
            } else {
                settle_call(&message, TAKER)
            };
            assert_eq!(
                (taker.as_deref(), calls.try_iter().collect::<Vec<_>>()),
                (Some(TAKER), vec![again]),
                "{TAKEN_AGAIN}"
            );
            assert_eq!(second.firings(TAKER).len(), 1, "{TAKEN_AGAIN}");
            second.stop().await;
        });
    }

    #[test]
    fn a_message_the_session_queued_again_leaves_its_firing_unconsumed() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.script(TAKER, &[CONSUMING], &[], CONSUMED_BODY);
            let live = fixture.spawn(&[TAKER]).await;
            let calls = live.attach(Answers {
                settles: false,
                ..Answers::default()
            });
            let message = from_lead(&FakePeer::default(), 1);

            live.offer(&message);
            let completed = live.firing(TAKER, FiringStatus::Completed).await;

            assert_eq!(
                (completed.consumed, live.state_of(TAKER).await.value),
                (false, json!({ CONSUMED_KEY: false })),
                "{GIVEN_UP}"
            );
            assert_eq!(
                calls.try_iter().collect::<Vec<_>>(),
                [settle_call(&message, TAKER)],
                "{GIVEN_UP}"
            );
            live.stop().await;
        });
    }

    #[test_case(true; "refused_by_a_closing_session")]
    #[test_case(false; "due_while_no_session_was_attached")]
    fn a_release_reaches_the_next_session_attached(refused: bool) {
        smol::block_on(async {
            let fixture = Fixture::new();
            let body = format!("{HOLD_BODY}\n{}", failing_body());
            fixture.script(TAKER, &[CONSUMING], &[], &body);
            let live = fixture.spawn(&[TAKER]).await;
            let first = live.attach(Answers {
                refuses: refused,
                ..Answers::default()
            });
            let message = from_lead(&FakePeer::default(), 1);
            live.offer(&message);
            live.held().await;
            if !refused {
                live.handle.attach(None);
            }

            live.release();
            live.firing(TAKER, FiringStatus::Failed).await;
            let second = live.attach(Answers::default());

            assert_eq!(
                next_release(&second).await,
                delivery(&message),
                "{WAITS_FOR_SESSION}"
            );
            assert_eq!(releases(&first).len(), usize::from(refused));
            live.stop().await;
        });
    }

    #[test]
    fn a_message_taken_as_the_runtime_stops_goes_back() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.script(TAKER, &[CONSUMING], &[], "");
            let live = fixture.spawn(&[TAKER]).await;
            let peer = FakePeer::default();
            let (stalling, late) = (from_lead(&peer, 1), from_lead(&peer, 2));
            let (unstall, calls) = live.stall(&stalling).await;
            let Live {
                runtime,
                handle,
                release,
                ..
            } = live;
            let mut stopping = pin!(runtime.shutdown());
            assert!(
                future::poll_once(&mut stopping).await.is_none(),
                "{STALLED}"
            );

            let taker = handle.message_observer().observe(&late);
            unstall.send(()).expect(GATE_OPEN);
            drop(release);
            stopping.await;

            assert_eq!(taker.as_deref(), Some(TAKER));
            assert_eq!(
                calls.try_iter().collect::<Vec<_>>(),
                [MessagingCall::GiveBack(late.key.clone())],
                "{UNRECORDED_GOES_BACK}"
            );
        });
    }

    #[test]
    fn a_message_taken_as_its_automation_is_disarmed_goes_back() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.script(TAKER, &[CONSUMING], &[], "");
            let live = fixture.spawn(&[TAKER]).await;
            let peer = FakePeer::default();
            let (stalling, late) = (from_lead(&peer, 1), from_lead(&peer, 2));
            let (unstall, calls) = live.stall(&stalling).await;

            let taker = {
                let mut disarming = pin!(live.handle.request(disarm(TAKER)));
                assert!(
                    future::poll_once(&mut disarming).await.is_none(),
                    "{STALLED}"
                );
                let taker = live.offer(&late);
                unstall.send(()).expect(GATE_OPEN);
                disarming.await.unwrap();
                taker
            };
            live.barrier().await;

            assert_eq!(taker.as_deref(), Some(TAKER));
            assert!(
                calls
                    .try_iter()
                    .any(|call| call == MessagingCall::GiveBack(late.key.clone())),
                "{UNRECORDED_GOES_BACK}"
            );
            live.stop().await;
        });
    }

    #[test]
    fn shutdown_and_a_restore_that_no_longer_matches_hand_back_at_the_next_start_once() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.script(TAKER, &[CONSUMING], &[], HOLD_BODY);
            let peer = FakePeer::default();
            let (interrupted, waiting) = (from_lead(&peer, 1), from_lead(&peer, 2));
            let first = fixture.spawn(&[TAKER]).await;
            let before = first.attach(Answers::default());
            first.offer(&interrupted);
            first.held().await;
            first.offer(&waiting);
            first.barrier().await;
            first.interrupt().await;

            fixture.script(TAKER, &[TOPICS_ONLY], &[], HOLD_BODY);
            let second = fixture.spawn(&[TAKER]).await;
            let calls = second.attach(Answers::default());
            let released = [next_release(&calls).await, next_release(&calls).await];
            second.stop().await;
            let third = fixture.spawn(&[TAKER]).await;
            let after = third.attach(Answers::default());
            third.barrier().await;

            assert!(releases(&before).is_empty(), "{NEXT_START}");
            assert_eq!(
                released,
                [delivery(&interrupted), delivery(&waiting)],
                "{NEXT_START}"
            );
            assert!(releases(&after).is_empty(), "{RELEASED_ONCE}");
            third.stop().await;
        });
    }

    #[test]
    fn messages_go_out_as_the_automation_with_their_request_ids_and_receipts() {
        smol::block_on(async {
            let fixture = Fixture::new();
            let body = format!(
                r#"state.{REPLIED_KEY} = reply("{ANSWER}");
state.{SENT_KEY} = send(event.sender, "{ANSWER}", #{{ reply_to: event.message_id }});
state.{PUBLISHED_KEY} = publish("{STATUS_TOPIC}", "{ANSWER}");
state.{BROADCAST_KEY} = broadcast("{ANSWER}");"#
            );
            fixture.script(TAKER, &[CONSUMING], &[MESSAGING], &body);
            let live = fixture.spawn(&[TAKER]).await;
            let recipient = RecipientReceipt {
                target: LEAD.into(),
                title: QUESTION.into(),
                handle: None,
                status: STATUS_RATE_LIMITED.into(),
                reason: Some(SLOW_DOWN.into()),
            };
            let calls = live.attach(Answers {
                recipients: vec![recipient],
                ..Answers::default()
            });
            let message = from_lead(&FakePeer::default(), 1);

            live.offer(&message);
            let completed = live.firing(TAKER, FiringStatus::Completed).await;

            let origin = SendOrigin::Automation(TAKER.into());
            let ids: Vec<String> = (0..4)
                .map(|seq| request_id(&completed.fire_id, seq))
                .collect();
            let reply = |request_id: &str| MessagingCall::Send {
                origin: origin.clone(),
                target: LEAD.into(),
                text: ANSWER.into(),
                reply_to: Some(message.key.clone()),
                request_id: request_id.into(),
            };
            let publish = |audience, request_id: &str| MessagingCall::Publish {
                origin: origin.clone(),
                audience,
                text: ANSWER.into(),
                request_id: request_id.into(),
            };
            let topic = PeerAudience::Topic {
                topic: STATUS_TOPIC.into(),
            };
            assert_eq!(
                calls.try_iter().collect::<Vec<_>>(),
                [
                    settle_call(&message, TAKER),
                    reply(&ids[0]),
                    reply(&ids[1]),
                    publish(topic, &ids[2]),
                    publish(PeerAudience::Broadcast, &ids[3]),
                ],
                "{SENT_AS_AUTOMATION}"
            );
            let state = live.state_of(TAKER).await.value;
            let receipts: Vec<Value> = [REPLIED_KEY, SENT_KEY, PUBLISHED_KEY, BROADCAST_KEY]
                .iter()
                .map(|key| state[*key][RECEIPT_ID_KEY].clone())
                .collect();
            assert_eq!(receipts, ids.iter().map(|id| json!(id)).collect::<Vec<_>>());
            let published = &state[PUBLISHED_KEY][RECIPIENTS_KEY][0];
            assert_eq!(
                (
                    &published[RECIPIENT_NAME_KEY],
                    &published[RECEIPT_STATUS_KEY]
                ),
                (&json!(LEAD), &json!(STATUS_RATE_LIMITED))
            );
            live.stop().await;
        });
    }

    /// The state a firing that sends with `send_status` and publishes to one recipient in
    /// `recipient_status` leaves: the two receipts as the script reads them.
    async fn receipts(send_status: &'static str, recipient_status: &str) -> Value {
        let fixture = Fixture::new();
        let body = format!(
            r#"state.{SENT_KEY} = send("{LEAD}", "{ANSWER}");
state.{PUBLISHED_KEY} = publish("{STATUS_TOPIC}", "{ANSWER}");"#
        );
        fixture.script(TAKER, &[OBSERVING], &[MESSAGING], &body);
        let live = fixture.spawn(&[TAKER]).await;
        let recipient = RecipientReceipt {
            target: LEAD.into(),
            title: QUESTION.into(),
            handle: None,
            status: recipient_status.into(),
            reason: None,
        };
        let _calls = live.attach(Answers {
            status: send_status,
            recipients: vec![recipient],
            ..Answers::default()
        });
        live.offer(&from_lead(&FakePeer::default(), 1));
        live.firing(TAKER, FiringStatus::Completed).await;
        let state = live.state_of(TAKER).await.value;
        live.stop().await;
        state
    }

    #[test_case(STATUS_QUEUED, SendStatus::Queued; "queued")]
    #[test_case(STATUS_CONSUMED, SendStatus::Queued; "consumed")]
    #[test_case(STATUS_HELD, SendStatus::Held; "held")]
    #[test_case(STATUS_UNKNOWN, SendStatus::Unknown; "unknown")]
    fn a_send_returns_its_receipt_with_the_status_the_guide_promises(
        status: &'static str,
        shown: SendStatus,
    ) {
        let state = smol::block_on(receipts(status, STATUS_QUEUED));

        assert_eq!(state[SENT_KEY][RECEIPT_STATUS_KEY], json!(spelled(shown)));
    }

    #[test_case(STATUS_QUEUED, RecipientStatus::Queued; "queued")]
    #[test_case(STATUS_CONSUMED, RecipientStatus::Queued; "consumed")]
    #[test_case(STATUS_HELD, RecipientStatus::Held; "held")]
    #[test_case(STATUS_REFUSED, RecipientStatus::Refused; "refused")]
    #[test_case(STATUS_UNAVAILABLE, RecipientStatus::Unavailable; "unavailable")]
    #[test_case(STATUS_RATE_LIMITED, RecipientStatus::RateLimited; "rate_limited")]
    #[test_case(STATUS_UNKNOWN, RecipientStatus::Unknown; "unknown")]
    fn a_publication_shows_each_recipient_with_the_status_the_guide_promises(
        status: &str,
        shown: RecipientStatus,
    ) {
        let state = smol::block_on(receipts(STATUS_QUEUED, status));

        assert_eq!(
            state[PUBLISHED_KEY][RECIPIENTS_KEY][0][RECEIPT_STATUS_KEY],
            json!(spelled(shown))
        );
    }

    #[test_case(Some(SendFailureKind::Refused), FailureKind::Refused, PEER_REASON; "refused")]
    #[test_case(Some(SendFailureKind::RateLimited), FailureKind::RateLimited, PEER_REASON; "rate_limited")]
    #[test_case(Some(SendFailureKind::Unavailable), FailureKind::Unavailable, PEER_REASON; "unavailable")]
    #[test_case(Some(SendFailureKind::UnknownRecipient), FailureKind::UnknownRecipient, PEER_REASON; "unknown_recipient")]
    #[test_case(Some(SendFailureKind::GroupFull), FailureKind::GroupFull, PEER_REASON; "group_full")]
    #[test_case(Some(SendFailureKind::ReadOnly), FailureKind::ReadOnly, PEER_REASON; "read_only")]
    #[test_case(Some(SendFailureKind::Invalid), FailureKind::InvalidArgument, PEER_REASON; "invalid")]
    #[test_case(None, FailureKind::Unavailable, NO_MESSAGING; "no_session")]
    fn a_send_that_fails_throws_its_kind_with_the_reason(
        failure: Option<SendFailureKind>,
        kind: FailureKind,
        message: &str,
    ) {
        smol::block_on(async {
            let fixture = Fixture::new();
            let body = catching(&format!(r#"send("{LEAD}", "{ANSWER}")"#));
            fixture.script(TAKER, &[OBSERVING], &[MESSAGING], &body);
            let live = fixture.spawn(&[TAKER]).await;
            let _calls = failure.map(|failure| {
                live.attach(Answers {
                    failure: Some((failure, PEER_REASON)),
                    ..Answers::default()
                })
            });

            live.offer(&from_lead(&FakePeer::default(), 1));
            live.firing(TAKER, FiringStatus::Completed).await;

            assert_eq!(live.state_of(TAKER).await.value, caught(kind, message));
            live.stop().await;
        });
    }

    #[test_case(UNDECLARED_SEND, StopKind::Capability; "an_undeclared_recipient")]
    #[test_case(UNDECLARED_PUBLISH, StopKind::Capability; "an_undeclared_topic")]
    #[test_case(UNDECLARED_BROADCAST, StopKind::Capability; "an_undeclared_broadcast")]
    #[test_case(BARE_REPLY, StopKind::NoConsumedMessage; "a_reply_without_a_consumed_message")]
    #[test_case(RELEASE_BODY, StopKind::NoConsumedMessage; "a_release_without_a_consumed_message")]
    fn messaging_the_header_or_the_event_does_not_allow_stops_before_the_session(
        body: &str,
        stop: StopKind,
    ) {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.script(TAKER, &[OBSERVING], &[NO_BROADCAST], body);
            let live = fixture.spawn(&[TAKER]).await;
            let calls = live.attach(Answers::default());

            live.offer(&from_lead(&FakePeer::default(), 1));
            let failed = live.firing(TAKER, FiringStatus::Failed).await;

            assert_eq!(
                failed.error.expect(NO_ERROR).kind,
                ErrorKind::Stop(stop).as_str()
            );
            assert!(calls.is_empty(), "{NO_SEND}");
            live.stop().await;
        });
    }

    #[test]
    fn a_script_message_matches_by_its_label_and_has_no_reply_target() {
        smol::block_on(async {
            let fixture = Fixture::new();
            let body = format!(
                r#"state.{NO_SENDER_KEY} = event.sender == ();
try {{ reply("{ANSWER}"); }} catch (err) {{ state.{KIND_KEY} = err.kind; }}
{RELEASE_BODY}"#
            );
            fixture.script(TAKER, &[FROM_SCRIPTS], &[MESSAGING], &body);
            let live = fixture.spawn(&[TAKER]).await;
            let calls = live.attach(Answers::default());
            let peer = FakePeer::default();
            let script = |label: &str, number| {
                let sender = ObservedSender::Script {
                    label: label.into(),
                };
                peer.message(number, sender, QUESTION, false)
            };
            let labelled = script(SCRIPT_LABEL, 2);

            let taken = [
                live.offer(&from_lead(&peer, 0)),
                live.offer(&script(OTHER_LABEL, 1)),
                live.offer(&labelled),
            ];
            live.firing(TAKER, FiringStatus::Released).await;

            assert_eq!(
                taken,
                [None, None, Some(TAKER.to_owned())],
                "{SCRIPT_SENDER}"
            );
            assert_eq!(
                live.state_of(TAKER).await.value,
                json!({ NO_SENDER_KEY: true, KIND_KEY: FailureKind::NoReplyTarget.as_str() }),
                "{SCRIPT_SENDER}"
            );
            assert_eq!(
                calls.try_iter().collect::<Vec<_>>(),
                [
                    settle_call(&labelled, TAKER),
                    MessagingCall::Release(delivery(&labelled))
                ]
            );
            live.stop().await;
        });
    }

    #[test]
    fn sends_do_not_count_against_the_queued_messages_a_firing_may_leave() {
        smol::block_on(async {
            let fixture = Fixture::new();
            let over = DEFAULT_MAX_DELIVERIES + 1;
            let body = format!(
                r#"for i in 0..{over} {{ send("{LEAD}", "{ANSWER}"); }}
for i in 0..{over} {{ message("{MESSAGE}"); }}"#
            );
            fixture.script(TAKER, &[OBSERVING], &[MESSAGING], &body);
            let live = fixture.spawn(&[TAKER]).await;
            let calls = live.attach(Answers::default());

            live.offer(&from_lead(&FakePeer::default(), 1));
            let failed = live.firing(TAKER, FiringStatus::Failed).await;

            assert_eq!(
                failed.error.expect(NO_ERROR).kind,
                ErrorKind::Stop(StopKind::FiringLimit).as_str()
            );
            let sends = calls
                .try_iter()
                .filter(|call| matches!(call, MessagingCall::Send { .. }))
                .count();
            assert_eq!(u32::try_from(sends), Ok(over));
            live.stop().await;
        });
    }

    mod with_workflows {
        use caudra_automation::event::MAX_EVENT_BYTES;
        use caudra_automation::host::WorkflowStarted;
        use caudra_automation::snapshot::AutomationState;
        use caudra_storage::workflow_scratch::WORKFLOW_SCRATCH_DIR;
        use caudra_workflow::{RunSnapshot, RunStatus, RunUsage, SourceKind, WorkflowError};
        use test_case::test_case;

        use super::*;
        use crate::automation::restore::RUN_MOVED_ON;
        use crate::automation::testing::{
            FakeWorkflows, HeldStart, SettleSource, settled_run, until,
        };

        const STARTER: &str = "starter";
        const WATCHER: &str = "watcher";
        const BYSTANDER: &str = "bystander";
        const WORKFLOW: &str = "review-changes";
        const OTHER_WORKFLOW: &str = "deep-research";
        const DISPLAY_NAME: &str = "review-changes-2";
        const RUN_ID: &str = "run-1";
        const OTHER_RUN_ID: &str = "run-2";
        const SENTINEL_RUN_ID: &str = "run-9";
        const WORKFLOWS: &str = r#"workflows: ["review-changes"]"#;
        const START: &str =
            r#"start_workflow("review-changes", #{ scope: "main" }, #{ agent_budget: 24 })"#;
        const SCOPE: &str = "main";
        const AGENT_BUDGET: u32 = 24;
        const FINISHED: &str = r#"#{ kind: "workflow_finished", workflows: ["review-changes"] }"#;
        const FAILED_ONLY: &str = r#"#{ kind: "workflow_finished", workflows: ["review-changes"], statuses: ["failed"] }"#;
        const ONE_PER_HOUR: &str = "limits: #{ max_per_hour: 1 }";
        /// Appends what each `workflow_finished` event shows to `state.seen`.
        const SEEN_BODY: &str = r#"let seen = state.seen ?? [];
seen.push(#{ run_id: event.run_id, name: event.name, workflow: event.workflow, status: event.status, report: event.report, result: event.result, error: event.error, scratch_dir: event.scratch_dir, agents: event.agents, tokens: event.tokens });
state.seen = seen;"#;
        /// Appends the run id of each event to `state.runs`.
        const RUNS_BODY: &str = r#"let runs = state.runs ?? [];
runs.push(event.run_id);
state.runs = runs;"#;
        const REPORT_LENGTH_BODY: &str = r#"log("hold");
state.report_bytes = event.report.len();"#;
        const REPORT: &str = "All green.";
        const VERDICT: &str = "verdict";
        const PASSED: &str = "passed";
        const ERROR: &str = "one reviewer gave up";
        const AGENTS: u32 = 3;
        const TOKENS: u64 = 4_096;
        const EPOCH: u64 = 1;
        const LATE_REVISION: u64 = 7;
        /// Larger than a stored event may be, within what a report may carry.
        const STORED_REPORT_BYTES: usize = MAX_EVENT_BYTES * 2;
        const SEEN_KEY: &str = "seen";
        const RUNS_KEY: &str = "runs";
        const STARTED_KEY: &str = "started";
        const REPORT_BYTES_KEY: &str = "report_bytes";
        const NOT_STARTED: &str = "the runtime must hand the start to the workflow runtime";
        const ENDED_ONCE: &str = "the start's action must end once, with the runtime's answer";
        const SHUTDOWN_WAITS_FOR_START: &str = "shutdown must wait for the start in flight";
        const FIRES_ONCE_PER_EPOCH: &str = "a run must fire once per execution epoch";
        const FILTERED: &str = "only a run of a listed workflow in a listed status may fire";
        const NOT_A_CUT: &str = "the stored report must be cut by the size of the stored event";

        struct Workflowed {
            deps: RuntimeDeps,
            workflows: Arc<FakeWorkflows>,
            starts: flume::Receiver<HeldStart>,
            settles: SettleSource,
        }

        fn workflowed(fixture: &Fixture, launch: &[&str]) -> Workflowed {
            let (workflows, starts, settles) = FakeWorkflows::new();
            Workflowed {
                deps: RuntimeDeps {
                    workflows: Some(Arc::clone(&workflows) as Arc<dyn Workflows>),
                    ..fixture.deps(launch)
                },
                workflows,
                starts,
                settles,
            }
        }

        fn started() -> WorkflowStarted {
            WorkflowStarted {
                run_id: RUN_ID.to_owned(),
                name: DISPLAY_NAME.to_owned(),
            }
        }

        fn run(run_id: &str, workflow: &str, status: RunStatus) -> RunSnapshot {
            settled_run(run_id, workflow, status, EPOCH)
        }

        fn released(state: &AutomationState) -> bool {
            state
                .find(STARTER)
                .is_some_and(|snapshot| snapshot.status != AutomationStatus::Running)
        }

        #[test]
        fn a_started_run_answers_the_script_and_targets_its_action() {
            smol::block_on(async {
                let fixture = Fixture::new();
                let body = format!("state.{STARTED_KEY} = {START};");
                fixture.script(STARTER, &[ARMED], &[WORKFLOWS], &body);
                let workflowed = workflowed(&fixture, &[STARTER]);
                let live = boot(workflowed.deps).await;

                let held = workflowed.starts.recv_async().await.expect(NOT_STARTED);
                held.answer(Ok(started()));
                let completed = live.firing(STARTER, FiringStatus::Completed).await;

                assert_eq!(
                    held.request,
                    WorkflowRequest {
                        name: WORKFLOW.to_owned(),
                        args: json!({ "scope": SCOPE }),
                        agent_budget: Some(AGENT_BUDGET),
                    }
                );
                assert_eq!(
                    live.state_of(STARTER).await.value,
                    json!({ STARTED_KEY: { "run_id": RUN_ID, "name": DISPLAY_NAME } })
                );
                let action = live.trace(&completed.fire_id).await.remove(0);
                assert_eq!(
                    (action.kind, action.status, action.target.as_deref()),
                    (ActionKind::StartWorkflow, ActionStatus::Done, Some(RUN_ID))
                );
                live.stop().await;
            });
        }

        #[test_case(WorkflowError::Budget { requested: 99, max: 64 }, FailureKind::InvalidArgument; "budget")]
        #[test_case(WorkflowError::UnknownWorkflow { name: WORKFLOW.into() }, FailureKind::Refused; "unknown_workflow")]
        #[test_case(WorkflowError::Ambiguous { name: WORKFLOW.into(), scopes: vec![SourceKind::User] }, FailureKind::Refused; "ambiguous")]
        #[test_case(WorkflowError::TrustRequired { name: WORKFLOW.into(), digest: "d1".into(), path: "p".into() }, FailureKind::Refused; "trust_required")]
        #[test_case(WorkflowError::Invalid { name: WORKFLOW.into(), error: "bad".into() }, FailureKind::Refused; "invalid")]
        #[test_case(WorkflowError::TooManyRuns { max: 4 }, FailureKind::Refused; "too_many_runs")]
        #[test_case(WorkflowError::NotAdmitted("background admission is closed".into()), FailureKind::Refused; "not_admitted")]
        #[test_case(WorkflowError::Unavailable, FailureKind::Unavailable; "unavailable")]
        #[test_case(WorkflowError::Storage("disk full".into()), FailureKind::Unavailable; "storage")]
        #[test_case(WorkflowError::Internal("bug".into()), FailureKind::Unavailable; "internal")]
        #[test_case(WorkflowError::UnknownRun { run_id: RUN_ID.into() }, FailureKind::Unavailable; "unknown_run")]
        #[test_case(WorkflowError::InvalidTransition { run_id: RUN_ID.into(), status: RunStatus::Active }, FailureKind::Unavailable; "invalid_transition")]
        fn a_refused_start_fails_catchably_with_its_kind(error: WorkflowError, kind: FailureKind) {
            smol::block_on(async {
                let fixture = Fixture::new();
                fixture.script(STARTER, &[ARMED], &[WORKFLOWS], &catching(START));
                let workflowed = workflowed(&fixture, &[STARTER]);
                let live = boot(workflowed.deps).await;

                let held = workflowed.starts.recv_async().await.expect(NOT_STARTED);
                held.answer(Err(error.clone()));
                let completed = live.firing(STARTER, FiringStatus::Completed).await;

                let message = error.to_string();
                assert_eq!(live.state_of(STARTER).await.value, caught(kind, &message));
                let action = live.trace(&completed.fire_id).await.remove(0);
                assert_eq!(
                    (action.status, action.error, action.target),
                    (
                        ActionStatus::Failed,
                        Some(Failure::new(kind, message).to_string()),
                        None
                    )
                );
                live.stop().await;
            });
        }

        #[test]
        fn without_workflows_a_start_fails_as_unavailable() {
            smol::block_on(async {
                let fixture = Fixture::new();
                fixture.script(STARTER, &[ARMED], &[WORKFLOWS], &catching(START));
                let live = fixture.spawn(&[STARTER]).await;

                live.firing(STARTER, FiringStatus::Completed).await;

                assert_eq!(
                    live.state_of(STARTER).await.value,
                    caught(FailureKind::Unavailable, NO_WORKFLOWS)
                );
                live.stop().await;
            });
        }

        #[test]
        fn the_actor_serves_claims_and_other_firings_while_a_start_is_in_flight() {
            smol::block_on(async {
                let fixture = Fixture::new();
                fixture.script(STARTER, &[ARMED], &[WORKFLOWS], &format!("{START};"));
                fixture.script(BYSTANDER, &[ARMED], &[], HOLD_BODY);
                let workflowed = workflowed(&fixture, &[STARTER, BYSTANDER]);
                let live = boot(workflowed.deps).await;
                let held = workflowed.starts.recv_async().await.expect(NOT_STARTED);

                let claim = live.claim(fixture.gate()).await;
                live.held().await;
                live.release();
                live.firing(BYSTANDER, FiringStatus::Completed).await;
                let running = live.running();
                held.answer(Ok(started()));
                live.firing(STARTER, FiringStatus::Completed).await;

                assert!(claim.is_none(), "{SERVED}");
                assert_eq!(running, [STARTER], "{SERVED}");
                live.stop().await;
            });
        }

        #[test_case(AutomationRequest::Pause { by: PauseSource::Inspector }; "a_pause")]
        #[test_case(disarm(STARTER); "a_disarm")]
        fn a_stop_releases_the_firing_at_once_and_the_start_still_ends_its_action(
            stop: AutomationRequest,
        ) {
            smol::block_on(async {
                let fixture = Fixture::new();
                fixture.script(STARTER, &[ARMED], &[WORKFLOWS], &format!("{START};"));
                let workflowed = workflowed(&fixture, &[STARTER]);
                let live = boot(workflowed.deps).await;
                let held = workflowed.starts.recv_async().await.expect(NOT_STARTED);
                let fire_id = live.firings(STARTER).remove(0).fire_id;

                live.handle.request(stop).await.unwrap();
                until(&live.handle, released).await;
                held.answer(Ok(started()));
                live.firing(STARTER, FiringStatus::Cancelled).await;
                live.stop().await;

                let journaled = fixture.journaled(&fire_id).await;
                let action = journaled.actions.first().expect(NO_ACTION);
                assert_eq!(
                    (action.status, action.target.as_deref()),
                    (ActionStatus::Done, Some(RUN_ID)),
                    "{ENDED_ONCE}"
                );
            });
        }

        #[test]
        fn a_shutdown_releases_the_firing_at_once_and_waits_for_the_start() {
            smol::block_on(async {
                let fixture = Fixture::new();
                fixture.script(STARTER, &[ARMED], &[WORKFLOWS], &format!("{START};"));
                let workflowed = workflowed(&fixture, &[STARTER]);
                let Live {
                    runtime, handle, ..
                } = boot(workflowed.deps).await;
                let held = workflowed.starts.recv_async().await.expect(NOT_STARTED);
                let fire_id = handle.state().recent[0].fire_id.clone();

                let mut stopping = pin!(runtime.shutdown());
                assert!(future::poll_once(&mut stopping).await.is_none());
                until(&handle, released).await;
                held.answer(Ok(started()));
                stopping.await;

                let journaled = fixture.journaled(&fire_id).await;
                assert_eq!(journaled.firing.status, FiringStatus::Interrupted);
                let action = journaled.actions.first().expect(NO_ACTION);
                assert_eq!(
                    (action.status, action.target.as_deref()),
                    (ActionStatus::Done, Some(RUN_ID)),
                    "{SHUTDOWN_WAITS_FOR_START}"
                );
            });
        }

        #[test]
        fn a_settled_run_fires_workflow_finished_with_every_field() {
            smol::block_on(async {
                let fixture = Fixture::new();
                fixture.script(WATCHER, &[FINISHED], &[], SEEN_BODY);
                let workflowed = workflowed(&fixture, &[WATCHER]);
                let live = boot(workflowed.deps).await;

                workflowed.settles.settle(vec![RunSnapshot {
                    display_name: DISPLAY_NAME.to_owned(),
                    result: Some(json!({ "report": REPORT, VERDICT: PASSED })),
                    error: Some(ERROR.to_owned()),
                    usage: RunUsage {
                        agents_admitted: AGENTS,
                        tokens_used: TOKENS,
                    },
                    ..run(RUN_ID, WORKFLOW, RunStatus::Failed)
                }]);
                live.firing(WATCHER, FiringStatus::Completed).await;

                let scratch_dir = fixture
                    .state_dir
                    .path()
                    .join(WORKFLOW_SCRATCH_DIR)
                    .join(fixture.session.id.to_string())
                    .join(RUN_ID);
                assert_eq!(
                    live.state_of(WATCHER).await.value,
                    json!({ SEEN_KEY: [{
                        "run_id": RUN_ID,
                        "name": DISPLAY_NAME,
                        "workflow": WORKFLOW,
                        "status": "failed",
                        "report": { UNTRUSTED_TAG: REPORT },
                        "result": { UNTRUSTED_TAG: { VERDICT: PASSED } },
                        "error": { UNTRUSTED_TAG: ERROR },
                        "scratch_dir": scratch_dir.to_string_lossy(),
                        "agents": AGENTS,
                        "tokens": TOKENS,
                    }] })
                );
                live.stop().await;
            });
        }

        #[test]
        fn the_workflows_and_statuses_filters_skip_other_runs() {
            smol::block_on(async {
                let fixture = Fixture::new();
                fixture.script(WATCHER, &[FAILED_ONLY], &[], RUNS_BODY);
                let workflowed = workflowed(&fixture, &[WATCHER]);
                let live = boot(workflowed.deps).await;

                workflowed.settles.settle(vec![
                    run(OTHER_RUN_ID, OTHER_WORKFLOW, RunStatus::Failed),
                    run(RUN_ID, WORKFLOW, RunStatus::Completed),
                    run(SENTINEL_RUN_ID, WORKFLOW, RunStatus::Failed),
                ]);
                live.firing(WATCHER, FiringStatus::Completed).await;

                assert_eq!(live.firings(WATCHER).len(), 1, "{FILTERED}");
                assert_eq!(
                    live.state_of(WATCHER).await.value,
                    json!({ RUNS_KEY: [SENTINEL_RUN_ID] }),
                    "{FILTERED}"
                );
                live.stop().await;
            });
        }

        #[test]
        fn a_run_fires_once_per_execution_epoch() {
            smol::block_on(async {
                let fixture = Fixture::new();
                fixture.script(WATCHER, &[FINISHED], &[], RUNS_BODY);
                let workflowed = workflowed(&fixture, &[WATCHER]);
                let live = boot(workflowed.deps).await;
                let settled = run(RUN_ID, WORKFLOW, RunStatus::Completed);

                for runs in [
                    settled.clone(),
                    RunSnapshot {
                        revision: LATE_REVISION,
                        ..settled.clone()
                    },
                    settled_run(RUN_ID, WORKFLOW, RunStatus::Failed, EPOCH + 1),
                    run(SENTINEL_RUN_ID, WORKFLOW, RunStatus::Completed),
                ] {
                    workflowed.settles.settle(vec![runs]);
                }
                let sentinel = json!(SENTINEL_RUN_ID);
                let runs = loop {
                    live.firing(WATCHER, FiringStatus::Completed).await;
                    let state = live.state_of(WATCHER).await.value;
                    if state[RUNS_KEY]
                        .as_array()
                        .is_some_and(|runs| runs.contains(&sentinel))
                    {
                        break state;
                    }
                };

                assert_eq!(
                    runs,
                    json!({ RUNS_KEY: [RUN_ID, RUN_ID, SENTINEL_RUN_ID] }),
                    "{FIRES_ONCE_PER_EPOCH}"
                );
                live.stop().await;
            });
        }

        #[test]
        fn a_settle_past_max_per_hour_is_deferred() {
            smol::block_on(async {
                let fixture = Fixture::new();
                fixture.script(WATCHER, &[FINISHED], &[ONE_PER_HOUR], NOTIFY_BODY);
                let workflowed = workflowed(&fixture, &[WATCHER]);
                let live = boot(workflowed.deps).await;

                workflowed
                    .settles
                    .settle(vec![run(RUN_ID, WORKFLOW, RunStatus::Completed)]);
                live.firing(WATCHER, FiringStatus::Completed).await;
                workflowed
                    .settles
                    .settle(vec![run(OTHER_RUN_ID, WORKFLOW, RunStatus::Completed)]);
                let deferred = live.firing(WATCHER, FiringStatus::Deferred).await;

                assert!(deferred.deferred_until.is_some());
                live.stop().await;
            });
        }

        #[test]
        fn a_settle_while_paused_is_recorded_paused() {
            smol::block_on(async {
                let fixture = Fixture::new();
                fixture.script(WATCHER, &[FINISHED], &[], RUNS_BODY);
                let workflowed = workflowed(&fixture, &[WATCHER]);
                let live = boot(workflowed.deps).await;
                let pause = AutomationRequest::Pause {
                    by: PauseSource::Inspector,
                };
                live.handle.request(pause).await.unwrap();

                workflowed
                    .settles
                    .settle(vec![run(RUN_ID, WORKFLOW, RunStatus::Completed)]);
                let paused = live.firing(WATCHER, FiringStatus::Paused).await;

                assert_eq!(paused.reason.as_deref(), Some(PAUSED_FROM_INSPECTOR));
                live.stop().await;
            });
        }

        #[test_case(true; "the_feed_ends_first")]
        #[test_case(false; "the_runtime_stops_first")]
        fn the_forwarder_ends_with_the_feed_or_the_runtime(feed_ends_first: bool) {
            smol::block_on(async {
                let fixture = Fixture::new();
                fixture.script(WATCHER, &[FINISHED], &[], RUNS_BODY);
                let workflowed = workflowed(&fixture, &[WATCHER]);
                let live = boot(workflowed.deps).await;

                if feed_ends_first {
                    workflowed.settles.close().await;
                    live.stop().await;
                } else {
                    live.stop().await;
                    workflowed.settles.dropped().await;
                }
            });
        }

        #[test_case(EPOCH, None; "rebuilt_at_the_same_epoch")]
        #[test_case(EPOCH + 1, Some(RUN_MOVED_ON); "dropped_once_the_run_moved_on")]
        fn a_cut_workflow_event_comes_back_only_while_its_run_is_as_it_settled(
            epoch_now: u64,
            dropped: Option<&str>,
        ) {
            smol::block_on(async {
                let fixture = Fixture::new();
                fixture.script(WATCHER, &[FINISHED], &[], REPORT_LENGTH_BODY);
                let report = json!({ "report": "r".repeat(STORED_REPORT_BYTES) });
                let large = |run_id: &str, epoch: u64| RunSnapshot {
                    result: Some(report.clone()),
                    ..settled_run(run_id, WORKFLOW, RunStatus::Completed, epoch)
                };
                let first = workflowed(&fixture, &[WATCHER]);
                let live = boot(first.deps).await;
                first
                    .settles
                    .settle(vec![large(RUN_ID, EPOCH), large(OTHER_RUN_ID, EPOCH)]);
                live.held().await;
                until(&live.handle, |state| {
                    state
                        .recent
                        .iter()
                        .filter(|firing| firing.automation == WATCHER)
                        .count()
                        == 2
                })
                .await;
                live.interrupt().await;

                let second = workflowed(&fixture, &[WATCHER]);
                second
                    .workflows
                    .publish(vec![large(OTHER_RUN_ID, epoch_now)]);
                let live = boot(second.deps).await;
                let requeued = match dropped {
                    Some(reason) => {
                        let firing = live.firing(WATCHER, FiringStatus::Dropped).await;
                        assert_eq!(firing.reason.as_deref(), Some(reason));
                        firing
                    }
                    None => {
                        live.held().await;
                        live.release();
                        let firing = live.firing(WATCHER, FiringStatus::Completed).await;
                        assert_eq!(
                            live.state_of(WATCHER).await.value,
                            json!({ REPORT_BYTES_KEY: STORED_REPORT_BYTES })
                        );
                        firing
                    }
                };

                assert!(
                    live.detail(&requeued.fire_id).await.event_cut,
                    "{NOT_A_CUT}"
                );
                live.stop().await;
            });
        }
    }

    #[cfg(unix)]
    mod with_peers {
        use std::fs::Permissions;
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::AtomicUsize;

        use caudra_config::InboundPolicy;
        use caudra_storage::sessions::PermissionMode;
        use tempfile::Builder;

        use super::*;
        use crate::AgentMode;
        use crate::peers::{MessageChannel, PeerDescriptor, PeerHost, PeerSession};

        const DIRECTORY_MODE: u32 = 0o700;
        const ASKER: &str = "asker";
        const ANSWERER: &str = "answerer";
        const QUESTION_REQUEST: &str = "question-1";
        const HISTORY_PAGE: usize = 8;
        const LISTED: &str = "the host must list the answering session";
        const ASKED: &str = "the answerer's history must hold the question";
        const ANSWERED: &str = "the reply must reach the asking session as its automation's";
        const KEPT_FROM_MODEL: &str = "a consumed message must never reach the model";
        const CONSUMED_BY: &str = "the history must record the automation that consumed it";
        const BACK_TO_MODEL: &str = "a message released after a restart must reach the model";

        /// Two sessions of one host: the asker messages the answerer, whose automations run.
        struct Peers {
            asker: PeerSession,
            answerer: PeerSession,
            _host: PeerHost,
            _directory: TempDir,
        }

        impl Peers {
            fn new() -> Self {
                let directory = Builder::new()
                    .permissions(Permissions::from_mode(DIRECTORY_MODE))
                    .tempdir()
                    .unwrap();
                let cwd = directory.path().canonicalize().unwrap();
                let host = PeerHost::start_in(cwd.clone(), Arc::new(AtomicUsize::new(0))).unwrap();
                let register = |name: &str| {
                    host.register(PeerDescriptor {
                        session_id: CaudraId::generate(),
                        name: name.into(),
                        cwd: cwd.clone(),
                        mode: AgentMode::Build,
                        permission_mode: PermissionMode::Ask,
                        inbound: InboundPolicy::Accept,
                        blocked: false,
                        busy: false,
                    })
                    .unwrap()
                };
                Self {
                    asker: register(ASKER),
                    answerer: register(ANSWERER),
                    _host: host,
                    _directory: directory,
                }
            }

            /// Attaches the answerer to `live` as the TUI's `install_peer` does.
            fn attach(&self, live: &Live) {
                live.handle.attach_messaging(Some(self.answerer.clone()));
                self.answerer
                    .set_observer(Some(live.handle.message_observer()));
            }

            async fn ask(&self) {
                let target = self
                    .asker
                    .list_named()
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|peer| peer.title == ANSWERER)
                    .expect(LISTED)
                    .target;
                self.asker
                    .send_named(&target, QUESTION, None, QUESTION_REQUEST)
                    .await
                    .unwrap();
            }

            /// What the answerer's history records of the question.
            async fn recorded(&self) -> (String, Option<String>) {
                let channel = MessageChannel::Direct(self.asker.session_id().to_string());
                let page = self
                    .answerer
                    .channel_messages(channel, None, HISTORY_PAGE)
                    .await
                    .unwrap();
                let question = page
                    .messages
                    .into_iter()
                    .find(|message| !message.own)
                    .expect(ASKED);
                let recipient = &question.recipients[0];
                (recipient.status.clone(), recipient.reason.clone())
            }
        }

        #[test]
        fn a_consumed_message_is_answered_as_the_automation_and_kept_from_the_model() {
            smol::block_on(async {
                let fixture = Fixture::new();
                let body = format!(r#"reply("{ANSWER}");"#);
                fixture.script(TAKER, &[CONSUMING], &[MESSAGING], &body);
                let peers = Peers::new();
                let live = fixture.spawn(&[TAKER]).await;
                peers.attach(&live);

                peers.ask().await;
                live.firing(TAKER, FiringStatus::Completed).await;

                assert!(peers.answerer.claim().is_none(), "{KEPT_FROM_MODEL}");
                assert_eq!(
                    peers.recorded().await,
                    (STATUS_CONSUMED.to_owned(), Some(TAKER.to_owned())),
                    "{CONSUMED_BY}"
                );
                let claim = peers.asker.claim().expect(ANSWERED);
                let answer = claim.messages()[0].peer_event.clone().expect(ANSWERED);
                assert_eq!(
                    (answer.automation.as_deref(), answer.reply_to.is_some()),
                    (Some(TAKER), true),
                    "{ANSWERED}"
                );
                live.stop().await;
            });
        }

        #[test]
        fn a_message_consumed_before_a_restart_reaches_the_model_after_it() {
            smol::block_on(async {
                let fixture = Fixture::new();
                fixture.script(TAKER, &[CONSUMING], &[], HOLD_BODY);
                let peers = Peers::new();
                let first = fixture.spawn(&[TAKER]).await;
                peers.attach(&first);
                peers.ask().await;
                first.held().await;
                let consumed = peers.recorded().await;
                first.interrupt().await;
                peers.answerer.set_observer(None);

                let second = fixture.spawn(&[TAKER]).await;
                peers.attach(&second);
                second.stop().await;

                assert_eq!(
                    consumed,
                    (STATUS_CONSUMED.to_owned(), Some(TAKER.to_owned())),
                    "{CONSUMED_BY}"
                );
                let claim = peers.answerer.claim().expect(BACK_TO_MODEL);
                assert_eq!(claim.messages().len(), 1, "{BACK_TO_MODEL}");
            });
        }
    }

    mod dry_runs {
        use caudra_automation::replay::{Answer, DRY_RUN_ID, STUB_STATUS};
        use caudra_automation::request::{
            REPLAY_EVENT_CUT, REPLAY_EVENT_UNREADABLE, REPLAY_NO_TRIGGER, REPLAY_NOT_FINISHED,
        };
        use caudra_automation::snapshot::DryRunDetail;
        use caudra_storage::automation::{MAX_EVENT_BYTES, MAX_RESULT_BYTES};
        use caudra_storage::sessions::SessionDatabase;
        use test_case::test_case;

        use super::*;
        use crate::automation::testing::FakeWorkflows;

        const SEEDED: &str = "fire-seeded";
        const MISSING: &str = "fire-missing";
        const SEEDED_DIGEST: &str = "seeded-digest";
        const WORKFLOW: &str = "review-changes";
        const WORKFLOWS: &str = r#"workflows: ["review-changes"]"#;
        /// The default [`TOPIC_ARGS`] declares.
        const DEFAULT_TOPIC: &str = "unset";
        const PAGE_PARAM: &str = "page";
        const PAGE: &str = "1";
        const OTHER_PAGE: &str = "2";
        const FILLER: &str = "x";
        const STATUS_KEY: &str = "status";
        const VERSION_KEY: &str = "version";
        const FIRST_VERSION: u32 = 1;
        const EDITED_VERSION: u32 = 2;
        const UNIX_KEY: &str = "unix";
        const UNIX_BODY: &str = "state.unix = now().unix;";
        /// The revision of a state one firing wrote.
        const WRITTEN_REVISION: u64 = 1;
        /// What [`COUNT_BODY`] counts when it runs again after that firing.
        const COUNTED_AGAIN: u64 = 2;
        /// The revision of that state once a clear emptied it.
        const CLEARED_REVISION: u64 = 2;

        const NOT_A_DRY_RUN: &str = "a dry run must answer with its detail";
        const TRACED: &str = "a dry run must trace the replayed event under the placeholder id";
        const ANSWERED: &str = "each request must be recorded, or answered from the journal or \
                                the stub, and show its placeholder id or its target";
        const NOTHING_HAPPENS: &str = "a dry run must leave the mirror, the outbox, the latch, \
                                       the marks and the state as they were";
        const NOTHING_STORED: &str = "a dry run must store nothing";
        const NO_EVENTS: &str = "a dry run must announce nothing";
        const NO_CALLS: &str = "no fake host may see a call from a dry run";
        const JOURNALED: &str = "an http request must be answered as the journal kept its answer";
        const RUNS_THE_EDIT: &str = "a dry run must run the script on disk now, trusted or not";
        const PINNED: &str = "a dry run must run at the time the replayed firing started";
        const NOT_CHARGED: &str =
            "a dry run must report a limit, but neither enforce nor charge it";
        const WAITS_ITS_TURN: &str =
            "a dry run must wait for its turn off the actor, which serves on and shuts down";
        const LET_GO_ON_STOP: &str =
            "a dry run still waiting when its runtime stops must answer unavailable at once";
        const POINTS_AT_THE_EDIT: &str = "a failing dry run must point at the line it failed on";

        /// How a test comes by a firing a dry run refuses.
        enum Refused {
            OtherSession,
            Unknown,
            Queued,
            Running,
            CutEvent,
            UnreadableEvent,
            NoTrigger,
        }

        fn dry_run(fire_id: &str) -> AutomationRequest {
            AutomationRequest::DryRun {
                fire_id: fire_id.to_owned(),
            }
        }

        async fn replayed(live: &Live, fire_id: &str) -> DryRunDetail {
            match live.handle.request(dry_run(fire_id)).await {
                Ok(AutomationResponse::DryRun(detail)) => *detail,
                other => panic!("{NOT_A_DRY_RUN}: {other:?}"),
            }
        }

        /// The firings and actions storage keeps, and the bytes this session's automations
        /// take.
        fn stored(fixture: &Fixture) -> (u64, u64, u64) {
            let database = SessionDatabase::open(&fixture.state_dir).unwrap();
            let stats = database.stats().unwrap();
            (
                stats.automation_firing_count,
                stats.automation_action_count,
                database.automation_bytes(fixture.session.id).unwrap(),
            )
        }

        /// The next firing of `name` to leave the queue, for good or until a limit lets it.
        async fn left_the_queue(live: &Live, name: &str) -> FiringSummary {
            loop {
                if let AutomationEvent::Firing { firing, .. } =
                    live.events.recv_async().await.expect(EVENTS_CLOSED)
                    && firing.automation == name
                    && !matches!(firing.status, FiringStatus::Queued | FiringStatus::Running)
                {
                    return *firing;
                }
            }
        }

        fn version_body(version: u32) -> String {
            format!("state.{VERSION_KEY} = {version};")
        }

        /// Every request a firing of a non-message event may make.
        fn everything_body() -> String {
            format!(
                r#"{COUNT_BODY}
message("{MESSAGE}");
set_goal("{GOAL}");
{NOTIFY_BODY}
{get}
send("{LEAD}", "{QUESTION}");
publish("{STATUS_TOPIC}", "{ANSWER}");
broadcast("{ANSWER}");
start_workflow("{WORKFLOW}", #{{}});
pause_automations("{PAUSE_REASON}");
{HOLD_BODY}"#,
                get = get_body(API_URL),
            )
        }

        /// Fetches `page` of [`API_URL`], and records the status it answers or the failure it
        /// throws.
        fn fetch_body(page: &str) -> String {
            catching(&format!(
                r#"state.{STATUS_KEY} = http(#{{ method: "GET", url: "{API_URL}", query: #{{ {PAGE_PARAM}: "{page}" }} }}).status"#
            ))
        }

        /// Boots once storage holds a completed firing of [`GREETER`] whose event reads as
        /// `event`.
        async fn seeded(fixture: &Fixture, event: String) -> (Live, String) {
            let store =
                AutomationStore::spawn(fixture.state_dir.clone(), fixture.session.id).unwrap();
            let firing = NewAutomationFiring {
                fire_id: SEEDED.to_owned(),
                session_id: fixture.session.id,
                automation: GREETER.to_owned(),
                digest: SEEDED_DIGEST.to_owned(),
                trigger: TriggerKind::Armed.to_row(),
                trigger_index: 0,
                event,
                event_key: None,
                consumed: false,
            };
            store.insert_firing(firing, None).await.unwrap();
            store.start_firing(SEEDED.to_owned()).await.unwrap();
            let end = AutomationFiringEnd {
                status: FiringStatus::Completed.to_row(),
                reason: None,
                error: None,
                operations: 0,
                state_patch: None,
                commit: None,
            };
            store.finish_firing(SEEDED.to_owned(), end).await.unwrap();
            store.shutdown().await;
            (fixture.spawn(&[]).await, SEEDED.to_owned())
        }

        /// A runtime, and the id of a firing of which it refuses a dry run as `refused` says.
        async fn refusing(fixture: &Fixture, refused: Refused) -> (Live, String) {
            match refused {
                Refused::OtherSession => {
                    fixture.script(GREETER, &[ARMED], &[], RECORD_BODY);
                    let elsewhere = boot(RuntimeDeps {
                        session_id: fixture.other_session().id,
                        ..fixture.deps(&[GREETER])
                    })
                    .await;
                    let foreign = elsewhere.firing(GREETER, FiringStatus::Completed).await;
                    elsewhere.stop().await;
                    (fixture.spawn(&[]).await, foreign.fire_id)
                }
                Refused::Unknown => (fixture.spawn(&[]).await, MISSING.to_owned()),
                Refused::Queued => {
                    fixture.script(HELD, &[ARMED, IDLE], &[], HOLD_BODY);
                    let live = fixture.spawn(&[HELD]).await;
                    live.held().await;
                    live.signal(settled());
                    live.barrier().await;
                    let queued = live
                        .firings(HELD)
                        .into_iter()
                        .find(|firing| firing.status == FiringStatus::Queued)
                        .expect(NOT_JOURNALED);
                    (live, queued.fire_id)
                }
                Refused::Running => {
                    fixture.script(HELD, &[ARMED], &[], HOLD_BODY);
                    let live = fixture.spawn(&[HELD]).await;
                    let running = live.held().await;
                    (live, running)
                }
                Refused::CutEvent => {
                    let event = Value::String(FILLER.repeat(MAX_EVENT_BYTES)).to_string();
                    seeded(fixture, event).await
                }
                Refused::UnreadableEvent => seeded(fixture, json!({}).to_string()).await,
                Refused::NoTrigger => {
                    fixture.script(GREETER, &[ARMED], &[], RECORD_BODY);
                    let live = fixture.spawn(&[GREETER]).await;
                    let completed = live.firing(GREETER, FiringStatus::Completed).await;
                    fixture.script(GREETER, &[IDLE], &[], RECORD_BODY);
                    (live, completed.fire_id)
                }
            }
        }

        #[test_case(Refused::OtherSession, None; "another_sessions_firing")]
        #[test_case(Refused::Unknown, None; "an_unknown_firing")]
        #[test_case(Refused::Queued, Some(REPLAY_NOT_FINISHED); "a_queued_firing")]
        #[test_case(Refused::Running, Some(REPLAY_NOT_FINISHED); "a_running_firing")]
        #[test_case(Refused::CutEvent, Some(REPLAY_EVENT_CUT); "an_event_too_large_to_keep")]
        #[test_case(Refused::UnreadableEvent, Some(REPLAY_EVENT_UNREADABLE); "an_event_that_no_longer_reads")]
        #[test_case(Refused::NoTrigger, Some(REPLAY_NO_TRIGGER); "an_event_the_current_script_has_no_trigger_for")]
        fn a_dry_run_refuses_a_firing_it_cannot_replay(refused: Refused, reason: Option<&str>) {
            smol::block_on(async {
                let fixture = Fixture::new();
                let (live, fire_id) = refusing(&fixture, refused).await;

                let answer = live.handle.request(dry_run(&fire_id)).await;

                let expected = match reason {
                    Some(reason) => AutomationError::NotReplayable {
                        fire_id,
                        reason: reason.to_owned(),
                    },
                    None => AutomationError::UnknownFiring { fire_id },
                };
                assert_eq!(answer, Err(expected));
                live.stop().await;
            });
        }

        #[test]
        fn a_dry_run_traces_every_request_and_performs_and_stores_nothing() {
            smol::block_on(async {
                let fixture = Fixture::new();
                fixture.script(COURIER, &[ARMED], &[], COUNT_BODY);
                let (sent, requests) = flume::unbounded();
                let (workflows, starts, _settles) = FakeWorkflows::new();
                let live = boot(RuntimeDeps {
                    http: Some(Arc::new(FakeHttp(sent))),
                    workflows: Some(workflows as Arc<dyn Workflows>),
                    ..fixture.deps(&[COURIER])
                })
                .await;
                let calls = live.attach(Answers::default());
                let original = live.firing(COURIER, FiringStatus::Completed).await;
                let network = format!("network: [{API_ORIGIN:?}]");
                fixture.script(
                    COURIER,
                    &[ARMED],
                    &[&network, MESSAGING, WORKFLOWS],
                    &everything_body(),
                );
                live.barrier().await;
                let event = live.detail(&original.fire_id).await.event;
                let mirror = live.handle.state();
                let state = live.state_of(COURIER).await;
                let kept = stored(&fixture);
                live.events.try_iter().for_each(drop);
                calls.try_iter().for_each(drop);

                let detail = replayed(&live, &original.fire_id).await;
                live.barrier().await;

                let trace = &detail.trace;
                assert_eq!(
                    (
                        detail.fire_id.as_str(),
                        trace.firing.fire_id.as_str(),
                        trace.firing.status,
                        &trace.event,
                        trace.event_cut,
                    ),
                    (
                        original.fire_id.as_str(),
                        DRY_RUN_ID,
                        FiringStatus::Completed,
                        &event,
                        false
                    ),
                    "{TRACED}"
                );
                assert_eq!(
                    (
                        trace.firing.repeats,
                        trace.firing.attempts,
                        trace.firing.state_outcome,
                        trace.firing.action_count,
                    ),
                    (1, 0, None, u64::try_from(trace.actions.len()).unwrap()),
                    "{TRACED}"
                );
                assert_eq!(
                    (&trace.state_patch, trace.patch_cut, detail.state_revision),
                    (
                        &Some(json!({ COUNT_KEY: COUNTED_AGAIN })),
                        false,
                        WRITTEN_REVISION
                    ),
                    "{TRACED}"
                );
                let answered: Vec<(ActionKind, Answer, Option<&str>)> = trace
                    .actions
                    .iter()
                    .zip(&detail.answers)
                    .map(|(action, answer)| (action.kind, *answer, action.target.as_deref()))
                    .collect();
                assert_eq!(
                    answered,
                    [
                        (ActionKind::Message, Answer::Recorded, None),
                        (ActionKind::SetGoal, Answer::Recorded, None),
                        (ActionKind::Notify, Answer::Recorded, None),
                        (ActionKind::Http, Answer::Stubbed, Some(API_ORIGIN)),
                        (ActionKind::Send, Answer::Recorded, Some(LEAD)),
                        (ActionKind::Publish, Answer::Recorded, Some(STATUS_TOPIC)),
                        (ActionKind::Broadcast, Answer::Recorded, None),
                        (
                            ActionKind::StartWorkflow,
                            Answer::Recorded,
                            Some(DRY_RUN_ID)
                        ),
                        (ActionKind::Pause, Answer::Recorded, None),
                        (ActionKind::Log, Answer::Recorded, None),
                    ],
                    "{ANSWERED}"
                );
                assert_eq!(detail.answers.len(), trace.actions.len(), "{ANSWERED}");
                assert_eq!(*live.handle.state(), *mirror, "{NOTHING_HAPPENS}");
                assert_eq!(live.state_of(COURIER).await, state, "{NOTHING_HAPPENS}");
                assert_eq!(stored(&fixture), kept, "{NOTHING_STORED}");
                assert!(live.events.is_empty(), "{NO_EVENTS}");
                assert!(
                    requests.is_empty() && calls.is_empty() && starts.is_empty(),
                    "{NO_CALLS}"
                );
                live.stop().await;
            });
        }

        #[test_case(PAGE, Ok(0), Answer::Journal, Ok(NOT_FOUND); "a_matching_request_answers_from_the_journal")]
        #[test_case(OTHER_PAGE, Ok(0), Answer::Stubbed, Ok(STUB_STATUS); "a_changed_request_answers_with_the_stub")]
        #[test_case(PAGE, Ok(MAX_RESULT_BYTES), Answer::Cut, Ok(STUB_STATUS); "a_result_too_large_to_keep_answers_with_the_stub")]
        #[test_case(PAGE, Err(HttpErrorKind::Transport), Answer::Journal, Err(FailureKind::Transport); "a_journaled_failure_fails_again")]
        fn http_answers_as_the_replayed_firings_journal_kept_its_answer(
            page: &str,
            original: Result<usize, HttpErrorKind>,
            answer: Answer,
            seen: Result<u16, FailureKind>,
        ) {
            smol::block_on(async {
                let fixture = Fixture::new();
                fixture.fetcher(&[API_ORIGIN], &[], &fetch_body(PAGE));
                let (deps, requests) = fixture.fetching();
                let live = boot(deps).await;
                let held = requests.recv_async().await.expect(NOT_SENT);
                match original {
                    Ok(bytes) => held.respond(NOT_FOUND, &FILLER.repeat(bytes)),
                    Err(kind) => held.answer(Err(HttpError::new(kind, CLIENT_ERROR))),
                }
                let completed = live.firing(FETCHER, FiringStatus::Completed).await;
                let clear = AutomationRequest::ClearState {
                    name: FETCHER.into(),
                    expected_revision: WRITTEN_REVISION,
                };
                live.handle.request(clear).await.unwrap();
                fixture.fetcher(&[API_ORIGIN], &[], &fetch_body(page));

                let detail = replayed(&live, &completed.fire_id).await;

                let (patch, status, error) = match seen {
                    Ok(code) => (json!({ STATUS_KEY: code }), ActionStatus::Done, None),
                    Err(kind) => (
                        caught(kind, CLIENT_ERROR),
                        ActionStatus::Failed,
                        Some(Failure::new(kind, CLIENT_ERROR).to_string()),
                    ),
                };
                let action = &detail.trace.actions[0];
                assert_eq!(detail.answers, [answer], "{JOURNALED}");
                assert_eq!(
                    (&detail.trace.state_patch, detail.state_revision),
                    (&Some(patch), CLEARED_REVISION),
                    "{JOURNALED}"
                );
                assert_eq!(
                    (
                        action.status,
                        action.error.clone(),
                        action.target.as_deref()
                    ),
                    (status, error, Some(API_ORIGIN)),
                    "{JOURNALED}"
                );
                live.stop().await;
            });
        }

        #[test_case(false; "a_user_script")]
        #[test_case(true; "an_untrusted_edit_of_a_project_script")]
        fn a_dry_run_runs_the_script_on_disk_now(project: bool) {
            smol::block_on(async {
                let fixture = Fixture::new();
                let write = |version: u32| {
                    let body = version_body(version);
                    if project {
                        fixture.project_script(GREETER, &[ARMED], &[], &body);
                    } else {
                        fixture.script(GREETER, &[ARMED], &[], &body);
                    }
                };
                write(FIRST_VERSION);
                let live = fixture.spawn(&[]).await;
                live.barrier().await;
                if project {
                    let trust = AutomationRequest::Trust {
                        name: GREETER.into(),
                        digest: live.snapshot(GREETER).digest,
                    };
                    live.handle.request(trust).await.unwrap();
                }
                let arm = AutomationRequest::Arm {
                    name: GREETER.into(),
                    args: None,
                    origin: ArmOrigin::Manual,
                };
                live.handle.request(arm).await.unwrap();
                let original = live.firing(GREETER, FiringStatus::Completed).await;
                write(EDITED_VERSION);

                let detail = replayed(&live, &original.fire_id).await;

                // The mirror shows the armed script while armed, and the one on disk once disarmed.
                live.handle.request(disarm(GREETER)).await.unwrap();
                let edited = live.snapshot(GREETER);
                assert_eq!(
                    detail.trace.state_patch,
                    Some(json!({ VERSION_KEY: EDITED_VERSION })),
                    "{RUNS_THE_EDIT}"
                );
                assert_eq!(detail.trace.firing.digest, edited.digest, "{RUNS_THE_EDIT}");
                assert_ne!(edited.digest, original.digest, "{RUNS_THE_EDIT}");
                assert_eq!(edited.trust.is_trusted(), !project, "{RUNS_THE_EDIT}");
                live.stop().await;
            });
        }

        #[test]
        fn a_failing_dry_run_points_at_the_line_of_the_script_on_disk() {
            smol::block_on(async {
                let fixture = Fixture::new();
                fixture.script(GREETER, &[ARMED], &[], RECORD_BODY);
                let live = fixture.spawn(&[GREETER]).await;
                let original = live.firing(GREETER, FiringStatus::Completed).await;
                fixture.script(GREETER, &[ARMED], &[], &failing_body());

                let detail = replayed(&live, &original.fire_id).await;

                let firing = &detail.trace.firing;
                assert_eq!(firing.status, FiringStatus::Failed, "{POINTS_AT_THE_EDIT}");
                assert!(
                    firing
                        .error
                        .as_ref()
                        .is_some_and(|error| error.message.contains(FAILURE)),
                    "{POINTS_AT_THE_EDIT}"
                );
                assert_eq!(
                    detail.trace.error_source,
                    Some(failing_body()),
                    "{POINTS_AT_THE_EDIT}"
                );
                live.stop().await;
            });
        }

        #[test_case(Some(GIVEN), false => Some(topic(GIVEN)); "the_armed_args")]
        #[test_case(Some(GIVEN), true => Some(topic(GIVEN)); "the_stored_args_of_a_disarmed_binding")]
        #[test_case(None, false => Some(topic(DEFAULT_TOPIC)); "the_defaults")]
        fn a_dry_run_runs_with_the_args_resolved_against_the_script_on_disk(
            given: Option<&str>,
            restarted: bool,
        ) -> Option<Value> {
            smol::block_on(async {
                let fixture = Fixture::new();
                fixture.script(BRIEFED, &[ARMED], &[TOPIC_ARGS], "");
                let mut live = fixture
                    .launch(vec![launching(BRIEFED, given, ArmOrigin::Cli)])
                    .await;
                let original = live.firing(BRIEFED, FiringStatus::Completed).await;
                if restarted {
                    live.handle.request(disarm(BRIEFED)).await.unwrap();
                    live.stop().await;
                    live = fixture.spawn(&[]).await;
                }
                let body = format!("state.{TOPIC_ARG} = args.{TOPIC_ARG};");
                fixture.script(BRIEFED, &[ARMED], &[TOPIC_ARGS], &body);

                let detail = replayed(&live, &original.fire_id).await;

                live.stop().await;
                detail.trace.state_patch
            })
        }

        #[test]
        fn now_answers_the_time_the_replayed_firing_started() {
            smol::block_on(async {
                let fixture = Fixture::new();
                fixture.script(CLOCKED, &[ARMED], &[], "");
                let live = fixture.spawn(&[CLOCKED]).await;
                let original = live.firing(CLOCKED, FiringStatus::Completed).await;
                let started_at = live
                    .detail(&original.fire_id)
                    .await
                    .firing
                    .started_at
                    .expect(PINNED);
                let body = format!("{UNIX_BODY}\n{NOTIFY_BODY}");
                fixture.script(CLOCKED, &[ARMED], &[], &body);
                fixture.clock.advance(LATER);

                let detail = replayed(&live, &original.fire_id).await;

                let unix = started_at.div_euclid(MILLIS_PER_SECOND);
                assert_eq!(
                    detail.trace.state_patch,
                    Some(json!({ UNIX_KEY: unix })),
                    "{PINNED}"
                );
                let firing = &detail.trace.firing;
                let action = &detail.trace.actions[0];
                assert_eq!(
                    (
                        firing.queued_at,
                        firing.started_at,
                        firing.finished_at,
                        action.started_at,
                        action.finished_at
                    ),
                    (
                        started_at,
                        Some(started_at),
                        Some(started_at),
                        started_at,
                        Some(started_at)
                    ),
                    "{PINNED}"
                );
                live.stop().await;
            });
        }

        #[test]
        fn a_limit_is_reported_but_neither_enforced_nor_charged() {
            smol::block_on(async {
                let fixture = Fixture::new();
                fixture.script(PACED, &[ARMED, GOAL_FINISHED], &[COOLDOWN], STEP_BODY);
                let live = fixture.spawn(&[PACED]).await;
                let original = live.firing(PACED, FiringStatus::Completed).await;
                live.barrier().await;
                let marks = live.snapshot(PACED).limiter;

                let limited = replayed(&live, &original.fire_id).await;
                fixture.clock.advance(COOLDOWN_DELAY);
                let admitted = replayed(&live, &original.fire_id).await;
                live.barrier().await;
                let marks_after = live.snapshot(PACED).limiter;
                live.signal(goal_finished());
                let next = left_the_queue(&live, PACED).await;

                let refusal = LimitRefusal {
                    reason: LimitReason::Cooldown,
                    until: START_MS + millis(COOLDOWN_DELAY),
                };
                assert_eq!(
                    (limited.limited, limited.trace.firing.status),
                    (Some(refusal), FiringStatus::Completed),
                    "{NOT_CHARGED}"
                );
                assert_eq!(admitted.limited, None, "{NOT_CHARGED}");
                assert_eq!(marks_after, marks, "{NOT_CHARGED}");
                assert_eq!(next.status, FiringStatus::Completed, "{NOT_CHARGED}");
                live.stop().await;
            });
        }

        #[test]
        fn a_dry_run_waits_its_turn_off_the_actor_and_answers_unavailable_once_it_stops() {
            smol::block_on(async {
                let fixture = Fixture::new();
                fixture.script(GREETER, &[ARMED], &[], RECORD_BODY);
                let live = fixture.spawn(&[GREETER]).await;
                let original = live.firing(GREETER, FiringStatus::Completed).await;
                let handle = live.handle.clone();
                let _turn = live.dry_runs.acquire_arc().await;
                let mut answer = pin!(handle.request(dry_run(&original.fire_id)));

                let sent = future::poll_once(&mut answer).await;
                live.barrier().await;
                let waiting = future::poll_once(&mut answer).await;
                live.stop().await;
                let answered = answer.await;

                assert!(sent.is_none() && waiting.is_none(), "{WAITS_ITS_TURN}");
                assert!(
                    matches!(answered, Err(AutomationError::Unavailable)),
                    "{LET_GO_ON_STOP}"
                );
            });
        }
    }
}
