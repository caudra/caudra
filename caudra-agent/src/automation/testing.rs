//! A real automation runtime in a temporary directory, for tests beyond the runtime's own: a
//! saved session, project and user script directories, and a fake clock. Its peers are a
//! [`FakeMessaging`], and a [`FakePeer`] makes the messages it offers the observer. A
//! [`FakeWorkflows`] stands in for the session's workflow runtime.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use caudra_automation::event::{SessionStatus, SessionView, WorkView};
use caudra_automation::host::{WorkflowRequest, WorkflowStarted};
use caudra_automation::request::ProfileArming;
use caudra_automation::snapshot::{ArmOrigin, AutomationState};
use caudra_automation::untrusted::Untrusted;
use caudra_config::{AutomationsConfig, FeatureFlags};
use caudra_providers::PeerAudience;
use caudra_storage::StateDir;
use caudra_storage::id::CaudraId;
use caudra_workflow::{RunSnapshot, RunStatus, RunUsage, SourceKind, WorkflowError, WorkflowState};
use futures_lite::stream::{self, StreamExt};
use serde_json::json;
use tempfile::TempDir;

use super::catalog::Frontend;
use super::clock::{Clock, FakeClock};
use super::handle::AutomationHandle;
use super::manager::{AutomationRuntime, LaunchArming, RuntimeDeps};
use super::messaging::{Messaging, MessagingFuture, ReleaseError};
use super::workflows::{SettleFeed, StartFuture, Workflows};
use crate::StoredSession;
use crate::peers::{
    HistoryVersion, MessageKey, ObservedMessage, ObservedSender, PeerSession, PublishReceipt,
    RecipientReceipt, STATUS_QUEUED, SendFailure, SendFailureKind, SendOrigin, SendReceipt,
    StoredDelivery, WorkCursor, WorkItem,
};

pub const DESCRIPTION: &str = "Exercises the runtime";
const START_MS: i64 = 1_790_000_000_000;
const MILLIS_PER_SECOND: i64 = 1_000;
const MODEL: &str = "test/model";
const STATE_DIR: &str = "state";
const CONFIG_DIR: &str = "config";
const PROJECT_DIR: &str = "project";
const CAUDRA_DIR: &str = ".caudra";
const AUTOMATIONS_DIR: &str = "automations";
const SCRIPT_EXTENSION: &str = "rhai";
const TITLE: &str = "Automation fixture";
const MODE: &str = "build";
const SPAWNS: &str = "the fixture's runtime must spawn";
const EVENTS_CLOSED: &str = "the runtime's events must stay open while a test waits on them";
const PEER_TOKEN: &str = "0123456789abcdef0123456789abcdef";
const PEER_TITLE: &str = "Lead";
const PEER_MODE: &str = "build";
const PEER_PERMISSION: &str = "ask";
const DELIVERY_SHAPE: &str = "a fake delivery must read as the peers runtime stores one";
pub const RELEASE_REFUSED: &str = "the session is closed";
const NO_PUBLISHER: &str = "the fake session published no work";
const START_DROPPED: &str = "the runtime must never drop a workflow start it sent";
const FEED_OBSERVED_ONCE: &str = "the runtime must observe the settle feed once";

/// What the runtime asked of its session's peers, in the order it asked.
#[derive(Debug, Clone, PartialEq)]
pub enum MessagingCall {
    Send {
        origin: SendOrigin,
        target: String,
        text: String,
        reply_to: Option<MessageKey>,
        request_id: String,
    },
    Publish {
        origin: SendOrigin,
        audience: PeerAudience,
        text: String,
        request_id: String,
    },
    Settle {
        key: MessageKey,
        automation: String,
    },
    GiveBack(MessageKey),
    Release(String),
    HistoryVersion,
    WorkCursor,
    PublishedWorkSince {
        after: WorkCursor,
        limit: usize,
    },
}

/// How a [`FakeMessaging`] answers. A receipt's message id is the request id it answers.
#[derive(Debug, Clone)]
pub struct Answers {
    /// The status of every send's receipt.
    pub status: &'static str,
    /// Fails every send and publication instead.
    pub failure: Option<(SendFailureKind, &'static str)>,
    pub recipients: Vec<RecipientReceipt>,
    pub settles: bool,
    /// Refuses the releases it gets once it closes, as a session that closes with releases in
    /// flight does.
    pub refuses: bool,
    /// Holds each settle, and with it the runtime's actor, until the test sends on the gate.
    pub gate: Option<flume::Receiver<()>>,
}

impl Default for Answers {
    fn default() -> Self {
        Self {
            status: STATUS_QUEUED,
            failure: None,
            recipients: Vec::new(),
            settles: true,
            refuses: false,
            gate: None,
        }
    }
}

/// A session's peers that record every call the runtime makes and answer as [`Answers`] say,
/// and the work queries from the history of the session that published the work. It closes when
/// the runtime drops it.
pub struct FakeMessaging {
    calls: flume::Sender<MessagingCall>,
    answers: Answers,
    publisher: Option<PeerSession>,
    _open: flume::Sender<()>,
    closed: flume::Receiver<()>,
}

impl FakeMessaging {
    /// The fake, and the calls the runtime makes to it, in order. Its work queries fail.
    pub fn new(answers: Answers) -> (Arc<Self>, flume::Receiver<MessagingCall>) {
        Self::answering(answers, None)
    }

    /// A fake whose work queries `publisher` answers, as the session that published the work.
    pub fn publishing(publisher: PeerSession) -> (Arc<Self>, flume::Receiver<MessagingCall>) {
        Self::answering(Answers::default(), Some(publisher))
    }

    fn answering(
        answers: Answers,
        publisher: Option<PeerSession>,
    ) -> (Arc<Self>, flume::Receiver<MessagingCall>) {
        let (calls, made) = flume::unbounded();
        let (_open, closed) = flume::bounded(0);
        let messaging = Self {
            calls,
            answers,
            publisher,
            _open,
            closed,
        };
        (Arc::new(messaging), made)
    }

    fn publisher(&self) -> Result<PeerSession, String> {
        self.publisher
            .clone()
            .ok_or_else(|| NO_PUBLISHER.to_owned())
    }

    fn answer<T: Send + 'static>(&self, receipt: T) -> MessagingFuture<Result<T, SendFailure>> {
        let answer = match &self.answers.failure {
            Some((kind, reason)) => Err(SendFailure::new(kind.clone(), *reason)),
            None => Ok(receipt),
        };
        Box::pin(async move { answer })
    }
}

impl Messaging for FakeMessaging {
    fn send(
        &self,
        origin: SendOrigin,
        target: String,
        text: String,
        reply_to: Option<MessageKey>,
        request_id: String,
    ) -> MessagingFuture<Result<SendReceipt, SendFailure>> {
        let receipt = SendReceipt {
            status: self.answers.status.to_owned(),
            message_id: request_id.clone(),
            reason: None,
        };
        let _ = self.calls.send(MessagingCall::Send {
            origin,
            target,
            text,
            reply_to,
            request_id,
        });
        self.answer(receipt)
    }

    fn publish(
        &self,
        origin: SendOrigin,
        audience: PeerAudience,
        text: String,
        request_id: String,
    ) -> MessagingFuture<Result<PublishReceipt, SendFailure>> {
        let receipt = PublishReceipt {
            message_id: request_id.clone(),
            audience: audience.clone(),
            recipients: self.answers.recipients.clone(),
            skipped: 0,
            queued: Vec::new(),
        };
        let _ = self.calls.send(MessagingCall::Publish {
            origin,
            audience,
            text,
            request_id,
        });
        self.answer(receipt)
    }

    fn settle(&self, key: &MessageKey, automation: &str) -> bool {
        let _ = self.calls.send(MessagingCall::Settle {
            key: key.clone(),
            automation: automation.to_owned(),
        });
        if let Some(gate) = &self.answers.gate {
            let _ = gate.recv();
        }
        self.answers.settles
    }

    fn give_back(&self, key: &MessageKey) -> bool {
        let _ = self.calls.send(MessagingCall::GiveBack(key.clone()));
        true
    }

    fn release(&self, delivery: &str) -> MessagingFuture<Result<(), ReleaseError>> {
        let _ = self.calls.send(MessagingCall::Release(delivery.to_owned()));
        let (refuses, closed) = (self.answers.refuses, self.closed.clone());
        Box::pin(async move {
            if !refuses {
                return Ok(());
            }
            let _ = closed.recv_async().await;
            Err(ReleaseError::Refused(RELEASE_REFUSED.to_owned()))
        })
    }

    fn history_version(&self) -> MessagingFuture<Result<HistoryVersion, String>> {
        let _ = self.calls.send(MessagingCall::HistoryVersion);
        let publisher = self.publisher();
        Box::pin(async move { publisher?.history_version().await })
    }

    fn work_cursor(&self) -> MessagingFuture<Result<WorkCursor, String>> {
        let _ = self.calls.send(MessagingCall::WorkCursor);
        let publisher = self.publisher();
        Box::pin(async move { publisher?.work_cursor().await })
    }

    fn published_work_since(
        &self,
        after: WorkCursor,
        limit: usize,
    ) -> MessagingFuture<Result<Vec<(WorkCursor, WorkItem)>, String>> {
        let _ = self
            .calls
            .send(MessagingCall::PublishedWorkSince { after, limit });
        let publisher = self.publisher();
        Box::pin(async move { publisher?.published_work_since(after, limit).await })
    }
}

/// Another session messaging the fixture's: its messages as the fixture's session offers them
/// to an observer.
pub struct FakePeer {
    session: CaudraId,
}

impl Default for FakePeer {
    fn default() -> Self {
        Self {
            session: CaudraId::generate(),
        }
    }
}

impl FakePeer {
    /// Its direct message number `number` from `sender`, queued for the model unless `held`.
    pub fn message(
        &self,
        number: usize,
        sender: ObservedSender,
        text: &str,
        held: bool,
    ) -> ObservedMessage {
        let message_id = format!("{number:032x}");
        let delivery = json!({ "delivery": {
            "message_id": message_id,
            "issued_ms": 0,
            "target": self.route(),
            "sender": {
                "route": { "host": PEER_TOKEN, "session": self.session, "generation": PEER_TOKEN },
                "name": PEER_TITLE,
                "canonical_cwd": null,
                "mode": PEER_MODE,
                "permission_mode": PEER_PERMISSION,
            },
            "text": text,
            "reply_to": null,
        }});
        ObservedMessage {
            key: MessageKey {
                sender_route: self.route(),
                message_id,
            },
            audience: PeerAudience::Direct,
            sender,
            title: PEER_TITLE.to_owned(),
            cwd: None,
            text: text.to_owned(),
            reply_to: None,
            held,
            catch_up: false,
            delivery: serde_json::from_value::<StoredDelivery>(delivery).expect(DELIVERY_SHAPE),
        }
    }

    fn route(&self) -> String {
        format!("p1:{PEER_TOKEN}:{}:{PEER_TOKEN}", self.session)
    }
}

/// The delivery a release of `message` hands back, as the runtime stores it.
pub fn delivery(message: &ObservedMessage) -> String {
    serde_json::to_string(&message.delivery).expect(DELIVERY_SHAPE)
}

/// A workflow start the runtime made, which waits until the test answers it.
pub struct HeldStart {
    pub request: WorkflowRequest,
    reply: flume::Sender<Result<WorkflowStarted, WorkflowError>>,
}

impl HeldStart {
    /// Panics when the runtime dropped the start, which it must never do.
    pub fn answer(&self, answer: Result<WorkflowStarted, WorkflowError>) {
        self.reply.send(answer).expect(START_DROPPED);
    }
}

/// The runs that settle in a [`FakeWorkflows`], as the test pushes them.
pub struct SettleSource {
    runs: flume::Sender<Vec<RunSnapshot>>,
    dropped: flume::Receiver<()>,
}

impl SettleSource {
    pub fn settle(&self, runs: Vec<RunSnapshot>) {
        let _ = self.runs.send(runs);
    }

    /// Ends the feed, and returns once the runtime dropped it.
    pub async fn close(self) {
        let Self { runs, dropped } = self;
        drop(runs);
        let _ = dropped.recv_async().await;
    }

    /// Returns once the runtime dropped the feed.
    pub async fn dropped(&self) {
        let _ = self.dropped.recv_async().await;
    }
}

/// A session's workflow runtime: each start waits for the test's answer, the runs that settle
/// are those the test pushes, and the published state is the one it sets.
pub struct FakeWorkflows {
    starts: flume::Sender<HeldStart>,
    settles: flume::Receiver<Vec<RunSnapshot>>,
    /// Moves into the first feed, so dropping that feed disconnects [`SettleSource::dropped`].
    observed: Mutex<Option<flume::Sender<()>>>,
    state: ArcSwap<WorkflowState>,
}

impl FakeWorkflows {
    /// The fake, the starts it holds in the order the runtime made them, and its settle feed.
    pub fn new() -> (Arc<Self>, flume::Receiver<HeldStart>, SettleSource) {
        let (starts, held) = flume::unbounded();
        let (runs, settles) = flume::unbounded();
        let (observed, dropped) = flume::bounded(0);
        let workflows = Self {
            starts,
            settles,
            observed: Mutex::new(Some(observed)),
            state: ArcSwap::from_pointee(WorkflowState::default()),
        };
        (Arc::new(workflows), held, SettleSource { runs, dropped })
    }

    /// Publishes `runs` as the workflow runtime's state.
    pub fn publish(&self, runs: Vec<RunSnapshot>) {
        self.state.store(Arc::new(WorkflowState { runs }));
    }
}

impl Workflows for FakeWorkflows {
    fn start(&self, request: WorkflowRequest) -> StartFuture {
        let (reply, answered) = flume::bounded(1);
        let _ = self.starts.send(HeldStart { request, reply });
        Box::pin(async move {
            answered
                .recv_async()
                .await
                .unwrap_or(Err(WorkflowError::Unavailable))
        })
    }

    fn observe(&self) -> SettleFeed {
        let observed = self
            .observed
            .lock()
            .ok()
            .and_then(|mut observed| observed.take())
            .expect(FEED_OBSERVED_ONCE);
        stream::unfold(
            (self.settles.clone(), observed),
            |(settles, observed)| async move {
                let runs = settles.recv_async().await.ok()?;
                Some((runs, (settles, observed)))
            },
        )
        .boxed()
    }

    fn state(&self) -> Arc<WorkflowState> {
        self.state.load_full()
    }
}

/// Run `run_id` of `workflow`, named after it, in `status` at `execution_epoch`.
pub fn settled_run(
    run_id: &str,
    workflow: &str,
    status: RunStatus,
    execution_epoch: u64,
) -> RunSnapshot {
    RunSnapshot {
        run_id: run_id.to_owned(),
        display_name: workflow.to_owned(),
        workflow_name: workflow.to_owned(),
        source_kind: SourceKind::User,
        source_path: None,
        objective: None,
        status,
        pause_kind: None,
        pause_message: None,
        revision: 0,
        execution_epoch,
        phase: None,
        phases: Vec::new(),
        phase_history: Vec::new(),
        agent_budget: 1,
        usage: RunUsage::default(),
        roster: Vec::new(),
        result: None,
        error: None,
        logs: Vec::new(),
        outbox_pending: false,
        created_at: 0,
        updated_at: 0,
    }
}

pub struct AutomationFixture {
    temp: TempDir,
    state_dir: StateDir,
    session: StoredSession,
    clock: Arc<FakeClock>,
}

/// A saved session whose project and user script directories are empty.
impl Default for AutomationFixture {
    fn default() -> Self {
        let temp = TempDir::new().unwrap();
        let project = temp.path().join(PROJECT_DIR);
        fs::create_dir(&project).unwrap();
        fs::create_dir_all(temp.path().join(CONFIG_DIR).join(AUTOMATIONS_DIR)).unwrap();
        let state_dir = StateDir::from_path(temp.path().join(STATE_DIR));
        let mut session = StoredSession::new(MODEL, &project.to_string_lossy());
        session.save(&state_dir).unwrap();
        Self {
            temp,
            state_dir,
            session,
            clock: FakeClock::new(START_MS),
        }
    }
}

impl AutomationFixture {
    pub fn session_id(&self) -> CaudraId {
        self.session.id
    }

    pub fn state_dir(&self) -> &StateDir {
        &self.state_dir
    }

    /// The clock of every runtime the fixture spawns.
    pub fn clock(&self) -> &FakeClock {
        &self.clock
    }

    /// The session's working directory.
    pub fn project(&self) -> PathBuf {
        self.temp.path().join(PROJECT_DIR)
    }

    pub fn project_scripts(&self) -> PathBuf {
        self.project().join(CAUDRA_DIR).join(AUTOMATIONS_DIR)
    }

    /// User scripts need no trust.
    pub fn user_scripts(&self) -> PathBuf {
        self.config_dir().join(AUTOMATIONS_DIR)
    }

    /// `<name>.rhai` in `dir`: a header naming `name` and [`DESCRIPTION`] with `fields`, then
    /// `body` from line 2.
    pub fn script(&self, dir: &Path, name: &str, fields: &str, body: &str) -> PathBuf {
        self.write(
            dir,
            name,
            &format!(
                "let meta = #{{ name: \"{name}\", description: \"{DESCRIPTION}\", {fields} }};\n{body}"
            ),
        )
    }

    pub fn write(&self, dir: &Path, stem: &str, source: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(stem).with_extension(SCRIPT_EXTENSION);
        fs::write(&path, source).unwrap();
        path
    }

    /// Another saved session in the same state directory.
    pub fn other_session(&self) -> CaudraId {
        let mut other = StoredSession::new(MODEL, &self.project().to_string_lossy());
        other.save(&self.state_dir).unwrap();
        other.id
    }

    /// `launch` arms from the command line.
    pub fn deps(&self, session_id: CaudraId, launch: &[&str]) -> RuntimeDeps {
        RuntimeDeps {
            state_dir: self.state_dir.clone(),
            session_id,
            cwd: self.project(),
            user_config_dir: Some(self.config_dir()),
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
            facts: SessionView {
                id: session_id.to_string(),
                title: Untrusted::text(TITLE),
                name: None,
                mode: MODE.into(),
                status: SessionStatus::Idle,
                status_since: START_MS / MILLIS_PER_SECOND,
                goal: None,
                cost: None,
                groups: Vec::new(),
                work: WorkView::default(),
            },
            clock: Arc::clone(&self.clock) as Arc<dyn Clock>,
            http: None,
            workflows: None,
        }
    }

    pub async fn spawn(&self, launch: &[&str]) -> AutomationRuntime {
        self.spawn_session(self.session.id, launch).await
    }

    pub async fn spawn_session(&self, session_id: CaudraId, launch: &[&str]) -> AutomationRuntime {
        AutomationRuntime::spawn(self.deps(session_id, launch))
            .await
            .expect(SPAWNS)
    }

    fn config_dir(&self) -> PathBuf {
        self.temp.path().join(CONFIG_DIR)
    }
}

/// Waits, without sleeping, until the mirror satisfies `ready`: the mirror changes before the
/// event that announces it.
pub async fn until(handle: &AutomationHandle, ready: impl Fn(&AutomationState) -> bool) {
    let events = handle.events();
    while !ready(&handle.state()) {
        events.recv_async().await.expect(EVENTS_CLOSED);
    }
}
