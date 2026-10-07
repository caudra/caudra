//! `work_finished`: what became of the group work this session published. While a binding with
//! the trigger is armed and a session is attached, the runtime checks the history off the actor,
//! at most every [`WORK_POLL`] and only once its version moved, for the items this session's
//! publications queued that changed after each binding's cursor. A binding takes its cursor when
//! it arms, so older items never fire, and keeps it in its marks across restarts. Items come
//! oldest change first, and two changes between checks show as the latest. Each matching item is
//! queued once, under a key of its group, work and change, before the cursor moves past it, and
//! the cursor stops at the first item its automation's queue has no room for, which the next
//! check reads again.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use caudra_automation::catalog::CatalogEntry;
use caudra_automation::event::{EventDetail, WorkFinishedDetail, WorkState as FinishedState};
use caudra_automation::matcher::first_match;
use caudra_automation::meta::Trigger;
use caudra_automation::untrusted::Untrusted;
use serde_json::Value;
use smol::Task;

use super::busy::pause_reason;
use super::handle::Command;
use super::manager::Topics;
use super::messaging::{Messaging, MessagingFuture};
use super::store::BindingMarks;
use crate::peers::{HistoryVersion, WorkCursor, WorkItem, WorkPause, WorkState, handle_address};

/// A check starts at most this often.
const WORK_POLL: Duration = Duration::from_secs(5);
/// Items a check reads after one cursor. After a full page the next check reads on, whether or
/// not the history moved.
const WORK_PAGE: usize = 32;
const EVENT_KEY_SEPARATOR: char = ':';

/// The armed bindings with a `work_finished` trigger, and the check of their work.
#[derive(Default)]
pub(super) struct WorkWatch {
    bindings: BTreeMap<String, Watched>,
    /// The history version the last check that left no item behind read.
    seen: Option<HistoryVersion>,
    /// When the last check started, on the monotonic clock. `None` makes the next one due at once.
    started: Option<Duration>,
    flight: Option<Flight>,
}

struct Watched {
    entry: Arc<CatalogEntry>,
    /// `None` until a check reads where it starts.
    cursor: Option<WorkCursor>,
}

/// The check running off the actor, with each binding's cursor as it started. Dropping it
/// cancels the check.
struct Flight {
    _task: Task<()>,
    cursors: BTreeMap<String, Option<WorkCursor>>,
}

/// What a check read, in order: the history version, where a binding without a cursor starts,
/// and the page of items after each cursor, oldest change first.
pub(super) struct WorkChecked {
    version: HistoryVersion,
    start: Option<WorkCursor>,
    pages: BTreeMap<WorkCursor, Vec<(WorkCursor, WorkItem)>>,
}

/// The runtime as a check's items reach it.
pub(super) trait WorkRoute {
    fn work(&mut self) -> &mut WorkWatch;

    /// Records and queues the event for trigger `index` of `name`, unless one was recorded under
    /// `key` already, which counts as queued. False when the automation's queue has no room or
    /// the event could not be checked or recorded, so the cursor waits on it.
    async fn queue_work(
        &mut self,
        name: &str,
        index: usize,
        detail: EventDetail,
        key: String,
    ) -> bool;
}

impl WorkWatch {
    /// Watches `name`, armed with `entry`. A binding armed before keeps its cursor, and a resumed
    /// one takes back the one it `stored`, which a check due at once reads on from. Any other
    /// starts where that check finds the history. A binding without a `work_finished` trigger is
    /// not watched.
    pub(super) fn arm(&mut self, name: &str, entry: &Arc<CatalogEntry>, stored: Option<Value>) {
        let previous = self.bindings.remove(name);
        if !entry
            .meta
            .triggers
            .iter()
            .any(|trigger| matches!(trigger, Trigger::WorkFinished { .. }))
        {
            return;
        }
        let cursor = match previous {
            Some(previous) => previous.cursor,
            None => {
                self.seen = None;
                self.started = None;
                stored.and_then(|stored| serde_json::from_value(stored).ok())
            }
        };
        let entry = Arc::clone(entry);
        self.bindings
            .insert(name.to_owned(), Watched { entry, cursor });
    }

    pub(super) fn disarm(&mut self, name: &str) {
        self.bindings.remove(name);
    }

    /// When the next check is due on the monotonic clock: never while no binding is watched or a
    /// check runs.
    pub(super) fn next_check(&self) -> Option<Duration> {
        if self.bindings.is_empty() || self.flight.is_some() {
            return None;
        }
        Some(
            self.started
                .map_or(Duration::ZERO, |started| started + WORK_POLL),
        )
    }

    /// Starts the check through `messaging` once it is due at `now`. It reads the history version
    /// before anything else, and its end comes back as [`Command::WorkChecked`].
    pub(super) fn tick(
        &mut self,
        messaging: &Arc<dyn Messaging>,
        now: Duration,
        commands: &flume::Sender<Command>,
        attachment: u64,
    ) {
        if self.next_check().is_none_or(|due| due > now) {
            return;
        }
        let cursors: BTreeMap<String, Option<WorkCursor>> = self
            .bindings
            .iter()
            .map(|(name, watched)| (name.clone(), watched.cursor))
            .collect();
        let starting = cursors.values().any(Option::is_none);
        let after = cursors.values().copied().flatten().collect();
        let checking = check(
            messaging.history_version(),
            Arc::clone(messaging),
            self.seen.clone(),
            starting,
            after,
        );
        let commands = commands.clone();
        let task = smol::spawn(async move {
            let checked = checking.await;
            let _ = commands.send(Command::WorkChecked {
                attachment,
                checked,
            });
        });
        self.started = Some(now);
        self.flight = Some(Flight {
            _task: task,
            cursors,
        });
    }
}

/// Takes the end of the check in flight, unless `current` says it read through a session
/// attached since or the runtime stops. Each binding it checked that still has the cursor it had
/// queues the events of its page, and its cursor moves past them, or to the check's start when
/// it had none. The version it read counts as seen only when it left no binding behind and
/// checked every binding with a cursor. Answers the marks of the cursors that moved.
pub(super) async fn apply(
    runtime: &mut impl WorkRoute,
    current: bool,
    checked: Result<WorkChecked, String>,
) -> Result<Vec<(String, BindingMarks)>, String> {
    let Some(flight) = runtime.work().flight.take() else {
        return Ok(Vec::new());
    };
    if !current {
        return Ok(Vec::new());
    }
    let checked = checked?;
    let mut behind = runtime.work().bindings.iter().any(|(name, watched)| {
        watched.cursor.is_some() && flight.cursors.get(name) != Some(&watched.cursor)
    });
    let mut moved = Vec::new();
    for (name, from) in flight.cursors {
        let Some(entry) = runtime
            .work()
            .bindings
            .get(&name)
            .filter(|watched| watched.cursor == from)
            .map(|watched| Arc::clone(&watched.entry))
        else {
            continue;
        };
        let to = match from {
            None => checked.start,
            Some(from) => {
                let page = checked.pages.get(&from).map_or(&[][..], Vec::as_slice);
                let to = follow(runtime, &name, &entry.meta.triggers, from, page).await;
                behind |=
                    page.len() == WORK_PAGE || page.last().is_some_and(|(last, _)| *last != to);
                Some(to)
            }
        };
        if let Some(cursor) = to.filter(|_| to != from)
            && let Some(watched) = runtime.work().bindings.get_mut(&name)
        {
            watched.cursor = to;
            moved.push((name, marks(cursor)));
        }
    }
    runtime.work().seen = (!behind).then_some(checked.version);
    Ok(moved)
}

/// Reads the history version, where a new binding starts when one is `starting`, and, once the
/// version moved since `seen`, the page after each of `cursors`.
async fn check(
    version: MessagingFuture<Result<HistoryVersion, String>>,
    messaging: Arc<dyn Messaging>,
    seen: Option<HistoryVersion>,
    starting: bool,
    cursors: BTreeSet<WorkCursor>,
) -> Result<WorkChecked, String> {
    let version = version.await?;
    let start = if starting {
        Some(messaging.work_cursor().await?)
    } else {
        None
    };
    let mut pages = BTreeMap::new();
    if seen.as_ref() != Some(&version) {
        for cursor in cursors {
            let page = messaging.published_work_since(cursor, WORK_PAGE).await?;
            pages.insert(cursor, page);
        }
    }
    Ok(WorkChecked {
        version,
        start,
        pages,
    })
}

/// Queues the events of `page` for `name` in order, and answers where its cursor moves: past
/// each item that matched none of `triggers` or whose event was queued, up to the first whose
/// event was not.
async fn follow(
    runtime: &mut impl WorkRoute,
    name: &str,
    triggers: &[Trigger],
    from: WorkCursor,
    page: &[(WorkCursor, WorkItem)],
) -> WorkCursor {
    let mut cursor = from;
    for (stamp, item) in page {
        if let Some(event) = detail(item).map(EventDetail::WorkFinished)
            && let Some(index) = first_match(triggers, &event, &Topics)
            && !runtime
                .queue_work(name, index, event, event_key(item))
                .await
        {
            break;
        }
        cursor = *stamp;
    }
    cursor
}

/// `item` as its publisher's event carries it, once it reached a state the trigger knows. The
/// detail is the result of completed work and the reason otherwise.
fn detail(item: &WorkItem) -> Option<WorkFinishedDetail> {
    let state = match item.state {
        WorkState::Completed => FinishedState::Completed,
        WorkState::Failed => FinishedState::Failed,
        WorkState::Cancelled => FinishedState::Cancelled,
        WorkState::Paused => FinishedState::Paused,
        WorkState::Pending | WorkState::Leased | WorkState::Pausing => return None,
    };
    let detail = match state {
        FinishedState::Completed => &item.result,
        _ => &item.reason,
    };
    let message = &item.message.message;
    Some(WorkFinishedDetail {
        group: item.group.clone(),
        work: item.name.clone(),
        message_id: message.message_id.clone(),
        topic: message.audience.topic().map(str::to_owned),
        state,
        attempts: item.attempts,
        max_attempts: item.max_attempts,
        member: item
            .owner
            .as_ref()
            .and_then(|owner| owner.handle.as_deref())
            .map(handle_address),
        pause_reason: item
            .reason
            .as_deref()
            .filter(|_| state == FinishedState::Paused)
            .and_then(WorkPause::from_text)
            .map(pause_reason),
        detail: detail.as_deref().map(Untrusted::text),
    })
}

/// One per transition: its group, its work and the stamp of the change.
fn event_key(item: &WorkItem) -> String {
    format!(
        "{}{EVENT_KEY_SEPARATOR}{}{EVENT_KEY_SEPARATOR}{}",
        item.group, item.name, item.changed
    )
}

fn marks(cursor: WorkCursor) -> BindingMarks {
    BindingMarks {
        work_cursor: serde_json::to_value(cursor).ok(),
        ..BindingMarks::default()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    use caudra_automation::catalog::{CatalogEntry, Scope, Trust};
    use caudra_automation::event::{
        Event, EventDetail, PauseReason, WorkFinishedDetail, WorkState as FinishedState,
    };
    use caudra_automation::meta::parse_meta;
    use caudra_automation::request::{AutomationRequest, AutomationResponse};
    use caudra_automation::snapshot::{AutomationState, FiringStatus, FiringSummary};
    use caudra_automation::untrusted::Untrusted;
    use caudra_config::{InboundPolicy, MessagingConfig};
    use caudra_providers::PeerAudience;
    use caudra_storage::StateDir;
    use caudra_storage::id::CaudraId;
    use caudra_storage::messages::{GroupPolicy, MessageLog, WorkOutcome};
    use caudra_storage::sessions::{PermissionMode, StoredPeerControls};
    use jiff::Timestamp;
    use tempfile::{Builder, TempDir};
    use test_case::test_case;

    use super::{WORK_PAGE, WORK_POLL, WorkRoute, WorkWatch, apply};
    use crate::automation::handle::{AutomationHandle, Command};
    use crate::automation::manager::{AutomationRuntime, MAX_QUEUED_EVENTS};
    use crate::automation::messaging::Messaging;
    use crate::automation::store::{AutomationStore, BindingMarks};
    use crate::automation::testing::{
        AutomationFixture, DESCRIPTION, FakeMessaging, MessagingCall, until,
    };
    use crate::peers::{
        AssignedWork, PeerDescriptor, PeerHost, PeerSession, PublishReceipt, WorkAction,
        WorkCursor, handle_address, history_retention,
    };
    use crate::{AgentMode, DoneReason};

    const WATCHER: &str = "watcher";
    const PUBLISHER: &str = "publisher";
    const WORKER: &str = "worker";
    const WORKER_HANDLE: &str = "fixer";
    const GROUP: &str = "ci-triage";
    const PATTERN: &str = "ci.*";
    const TOPIC: &str = "ci.failures";
    const TEXT: &str = "The nightly build failed";
    const REQUEST_ID: &str = "nightly-failure";
    const SUMMARY: &str = "Fixed the flaky linker step";
    const FAILURE: &str = "The runner ran out of disk";
    /// Where [`PeerHost::start_in`] keeps the history its sessions share.
    const PEER_HISTORY: &str = "history";
    const DIRECTORY_MODE: u32 = 0o700;
    const EVERY_STATE: &str =
        r#"#{ kind: "work_finished", states: ["completed", "failed", "cancelled", "paused"] }"#;
    const DEFAULT_STATES: &str = r#"#{ kind: "work_finished" }"#;
    const COOLDOWN: &str = r#"limits: #{ cooldown: "10m" }"#;
    const COOLDOWN_DELAY: Duration = Duration::from_secs(10 * 60);
    /// Journals an action, so storage never folds a firing into the next as a quiet repeat and
    /// each one the runtime queued shows.
    const LOG_BODY: &str = r#"log("work finished");"#;
    /// Acts under the automation's limits.
    const NOTIFY_BODY: &str = r#"notify("work finished");"#;
    const EARLY: Duration = Duration::from_secs(1);
    /// Publications past the room of [`WATCHER`]'s queue.
    const OVERFLOW: usize = 2;
    const ATTACHMENT: u64 = 1;

    const CALLS_OPEN: &str = "the fake session must outlive the test's wait";
    const RUNNING: &str = "the runtime must answer while it runs";
    const NO_WORK: &str = "the worker must take the published item";
    const NOT_OWNED: &str = "the worker must still own the work it paused";
    const NOT_A_FIRING: &str = "a firing request must answer with the firing";
    const NOT_WORK: &str = "a firing of the watcher must carry a work_finished event";
    const EVENT_SHAPE: &str = "a journaled event must read back as an event";
    const NO_START: &str = "the first check must give a new binding its cursor";
    const THROTTLED: &str = "no check may start before the poll interval passed";
    const ONCE: &str = "a transition must fire once, whatever later checks or a restart read";
    const NEVER_DROPPED: &str = "a full queue must hold the cursor back rather than drop an event";
    const DEFERRED: &str = "a limit must defer the event and retry it, never drop it";
    const STALE: &str = "a check read through an earlier session must queue nothing";

    #[derive(Debug, Clone, Copy)]
    enum Ending {
        Completed,
        Failed,
        Paused,
        Cancelled,
    }

    /// A peer host whose group takes what [`PUBLISHER`] publishes on [`TOPIC`], and a member
    /// with a messaging name that works it.
    struct Group {
        publisher: PeerSession,
        worker: PeerSession,
        _host: PeerHost,
        _directory: TempDir,
    }

    impl Group {
        fn new() -> Self {
            let directory = Builder::new()
                .permissions(Permissions::from_mode(DIRECTORY_MODE))
                .tempdir()
                .unwrap();
            let cwd = directory.path().canonicalize().unwrap();
            let host = PeerHost::start_in(cwd.clone(), Arc::new(AtomicUsize::new(0))).unwrap();
            let messaging = MessagingConfig::default();
            let now = Timestamp::now().as_millisecond().unsigned_abs();
            MessageLog::open(
                &StateDir::from_path(cwd.join(PEER_HISTORY)),
                &history_retention(&messaging),
                now,
            )
            .unwrap()
            .create_group(GROUP, &[PATTERN.to_owned()], &GroupPolicy::default(), now)
            .unwrap();
            let register = |name: &str, messaging: &MessagingConfig, controls| {
                let descriptor = PeerDescriptor {
                    session_id: CaudraId::generate(),
                    name: name.into(),
                    cwd: cwd.clone(),
                    mode: AgentMode::Build,
                    permission_mode: PermissionMode::Ask,
                    inbound: InboundPolicy::Accept,
                    blocked: false,
                    busy: false,
                };
                host.register_with_controls(descriptor, messaging, Some(controls))
                    .unwrap()
            };
            let publishing = MessagingConfig {
                publish_per_minute: MAX_QUEUED_EVENTS + OVERFLOW,
                ..MessagingConfig::default()
            };
            let membership = StoredPeerControls {
                handle: Some(WORKER_HANDLE.into()),
                groups: vec![GROUP.into()],
                ..StoredPeerControls::default()
            };
            let worker = register(WORKER, &messaging, membership);
            worker.claim_handle().unwrap();
            Self {
                publisher: register(PUBLISHER, &publishing, StoredPeerControls::default()),
                worker,
                _host: host,
                _directory: directory,
            }
        }

        /// Publication `number` on [`TOPIC`], which queues one item for [`GROUP`].
        async fn publish(&self, number: usize) -> PublishReceipt {
            let audience = PeerAudience::Topic {
                topic: TOPIC.into(),
            };
            self.publisher
                .publish(audience, TEXT, &format!("{REQUEST_ID}-{number}"))
                .await
                .unwrap()
        }

        /// What the publisher's person made of `work`, as storage holds it then.
        async fn manage(&self, work: &str, action: WorkAction) -> AssignedWork {
            self.publisher.manage_work(work, action).await.unwrap().item
        }

        /// Brings `work`, the only item queued, to `ending`, and answers it as storage holds it
        /// then.
        async fn end(&self, work: &str, ending: Ending) -> AssignedWork {
            let outcome = match ending {
                Ending::Cancelled => return self.manage(work, WorkAction::Cancel).await,
                Ending::Completed => Some(WorkOutcome::Completed(Some(SUMMARY.into()))),
                Ending::Failed => Some(WorkOutcome::Failed(FAILURE.into())),
                Ending::Paused => None,
            };
            assert!(self.worker.acquire_work().await.unwrap(), "{NO_WORK}");
            self.worker.claim_wake().expect(NO_WORK).commit();
            match outcome {
                Some(outcome) => self.worker.report_work(work, outcome).await.unwrap(),
                None => {
                    self.worker.settle_work(Some(DoneReason::EndTurn)).await;
                    self.worker
                        .owned_work()
                        .await
                        .unwrap()
                        .into_iter()
                        .find(|owned| owned.work == work)
                        .expect(NOT_OWNED)
                }
            }
        }
    }

    /// A runtime whose [`WATCHER`] watches the publisher's work through a fake session that
    /// answers from the publisher's history, and the calls that session gets.
    struct Watch {
        fixture: AutomationFixture,
        runtime: AutomationRuntime,
        messaging: Arc<FakeMessaging>,
        calls: flume::Receiver<MessagingCall>,
    }

    impl Watch {
        /// Arms [`WATCHER`] with the meta `fields` and `body`, and returns once a check gave it
        /// its cursor.
        async fn new(group: &Group, fields: &str, body: &str) -> Self {
            let fixture = AutomationFixture::default();
            fixture.script(&fixture.user_scripts(), WATCHER, fields, body);
            Self::start(fixture, group).await
        }

        /// Starts a runtime for `fixture`'s session, which resumes the binding it kept, and
        /// returns once the check the attach starts ended.
        async fn start(fixture: AutomationFixture, group: &Group) -> Self {
            let runtime = fixture.spawn(&[WATCHER]).await;
            let (messaging, calls) = FakeMessaging::publishing(group.publisher.clone());
            let watch = Self {
                fixture,
                runtime,
                messaging,
                calls,
            };
            watch.attach();
            watch.started().await;
            watch.next_check().await;
            watch
        }

        fn handle(&self) -> AutomationHandle {
            self.runtime.handle()
        }

        fn attach(&self) {
            self.handle().attach(Some(self.messaging.clone()));
        }

        /// Detaches the session and returns once the runtime did, so the check in flight queues
        /// nothing, whatever it reads.
        async fn detach(&self) {
            let handle = self.handle();
            handle.attach(None);
            handle
                .request(AutomationRequest::List)
                .await
                .expect(RUNNING);
        }

        /// Waits until the next check starts.
        async fn started(&self) {
            while self.calls.recv_async().await.expect(CALLS_OPEN) != MessagingCall::HistoryVersion
            {
            }
        }

        /// Moves the clock to the next check and returns as it starts, which it does only once
        /// the check before it ended.
        async fn next_check(&self) {
            self.fixture.clock().advance(WORK_POLL);
            self.started().await;
        }

        /// Returns once a check that started after the call ended: what the history held at
        /// the call is queued, or waits for room.
        async fn checked(&self) {
            self.calls.drain().for_each(drop);
            self.next_check().await;
            self.next_check().await;
        }

        /// The event of each firing of [`WATCHER`], oldest first.
        async fn events(&self) -> Vec<WorkFinishedDetail> {
            let handle = self.handle();
            let mut events = Vec::new();
            for firing in watched(&handle.state()).into_iter().rev() {
                let request = AutomationRequest::Firing {
                    fire_id: firing.fire_id,
                };
                let detail = match handle.request(request).await {
                    Ok(AutomationResponse::Firing(detail)) => detail,
                    other => panic!("{NOT_A_FIRING}: {other:?}"),
                };
                let event: Event = serde_json::from_value(detail.event).expect(EVENT_SHAPE);
                match event.detail {
                    EventDetail::WorkFinished(work) => events.push(work),
                    other => panic!("{NOT_WORK}: {other:?}"),
                }
            }
            events
        }

        /// The work of each firing of [`WATCHER`], oldest first.
        async fn works(&self) -> Vec<String> {
            self.events()
                .await
                .into_iter()
                .map(|event| event.work)
                .collect()
        }

        async fn stop(self) -> AutomationFixture {
            self.runtime.shutdown().await;
            self.fixture
        }
    }

    /// The runtime as a check reaches it, with room for every event.
    #[derive(Default)]
    struct Queue {
        work: WorkWatch,
        events: Vec<EventDetail>,
    }

    impl WorkRoute for Queue {
        fn work(&mut self) -> &mut WorkWatch {
            &mut self.work
        }

        async fn queue_work(
            &mut self,
            _name: &str,
            _index: usize,
            detail: EventDetail,
            _key: String,
        ) -> bool {
            self.events.push(detail);
            true
        }
    }

    impl Queue {
        /// Ticks at `now` and takes the end of the check that started, read through the
        /// session attached now unless `current` says otherwise. Answers the calls the check
        /// made, none when none was due.
        async fn tick(
            &mut self,
            messaging: &Arc<dyn Messaging>,
            calls: &flume::Receiver<MessagingCall>,
            now: Duration,
            current: bool,
        ) -> Vec<MessagingCall> {
            let (commands, ended) = flume::unbounded();
            self.work.tick(messaging, now, &commands, ATTACHMENT);
            drop(commands);
            if let Ok(Command::WorkChecked { checked, .. }) = ended.recv_async().await {
                apply(self, current, checked).await.unwrap();
            }
            calls.drain().collect()
        }

        fn cursor(&self) -> Option<WorkCursor> {
            self.work
                .bindings
                .get(WATCHER)
                .and_then(|watched| watched.cursor)
        }
    }

    /// [`WATCHER`] armed on every state, watched without a runtime.
    fn watching(group: &Group) -> (Queue, Arc<dyn Messaging>, flume::Receiver<MessagingCall>) {
        let source = format!(
            "let meta = #{{ name: \"{WATCHER}\", description: \"{DESCRIPTION}\", triggers: [{EVERY_STATE}] }};"
        );
        let entry = Arc::new(CatalogEntry {
            meta: parse_meta(&source).unwrap(),
            scope: Scope::User,
            path: PathBuf::new(),
            digest: String::new(),
            trust: Trust::Location,
            warnings: Vec::new(),
            shadowed: Vec::new(),
        });
        let mut queue = Queue::default();
        queue.work.arm(WATCHER, &entry, None);
        let (messaging, calls) = FakeMessaging::publishing(group.publisher.clone());
        (queue, messaging, calls)
    }

    fn triggers(trigger: &str) -> String {
        format!("triggers: [{trigger}]")
    }

    /// The firings of [`WATCHER`], newest first.
    fn watched(state: &AutomationState) -> Vec<FiringSummary> {
        state
            .recent
            .iter()
            .filter(|firing| firing.automation == WATCHER)
            .cloned()
            .collect()
    }

    fn count(state: &AutomationState, status: FiringStatus) -> usize {
        watched(state)
            .iter()
            .filter(|firing| firing.status == status)
            .count()
    }

    /// Stores `cursor` as [`WATCHER`]'s, as a crash before the runtime saved the moves past it
    /// leaves it.
    async fn rewind(fixture: &AutomationFixture, cursor: WorkCursor) {
        let store =
            AutomationStore::spawn(fixture.state_dir().clone(), fixture.session_id()).unwrap();
        let marks = BindingMarks {
            work_cursor: Some(serde_json::to_value(cursor).unwrap()),
            ..BindingMarks::default()
        };
        store.save_marks(WATCHER.to_owned(), marks).await.unwrap();
        store.shutdown().await;
    }

    #[test]
    fn a_check_starts_at_most_every_poll_and_reads_work_only_once_the_history_moved() {
        smol::block_on(async {
            let group = Group::new();
            let (mut queue, messaging, calls) = watching(&group);

            let first = queue.tick(&messaging, &calls, Duration::ZERO, true).await;
            let start = queue.cursor().expect(NO_START);
            let early = queue
                .tick(&messaging, &calls, WORK_POLL - EARLY, true)
                .await;
            let mut now = WORK_POLL;
            let unmoved = queue.tick(&messaging, &calls, now, true).await;
            let receipt = group.publish(0).await;
            group
                .manage(&receipt.queued[0].work, WorkAction::Cancel)
                .await;
            now += WORK_POLL;
            let moved = queue.tick(&messaging, &calls, now, true).await;
            now += WORK_POLL;
            let seen = queue.tick(&messaging, &calls, now, true).await;

            assert_eq!(
                first,
                [MessagingCall::HistoryVersion, MessagingCall::WorkCursor]
            );
            assert!(early.is_empty(), "{THROTTLED}: {early:?}");
            assert_eq!(unmoved, [MessagingCall::HistoryVersion]);
            assert_eq!(
                moved,
                [
                    MessagingCall::HistoryVersion,
                    MessagingCall::PublishedWorkSince {
                        after: start,
                        limit: WORK_PAGE,
                    },
                ]
            );
            assert_eq!(seen, [MessagingCall::HistoryVersion]);
            assert_eq!(queue.events.len(), 1);
        });
    }

    #[test]
    fn a_check_read_through_an_earlier_session_queues_nothing_and_moves_no_cursor() {
        smol::block_on(async {
            let group = Group::new();
            let (mut queue, messaging, calls) = watching(&group);
            queue.tick(&messaging, &calls, Duration::ZERO, true).await;
            let start = queue.cursor();
            let receipt = group.publish(0).await;
            group
                .manage(&receipt.queued[0].work, WorkAction::Cancel)
                .await;

            queue.tick(&messaging, &calls, WORK_POLL, false).await;
            let stale = (queue.cursor(), queue.events.len());
            queue
                .tick(&messaging, &calls, WORK_POLL + WORK_POLL, true)
                .await;

            assert_eq!(stale, (start, 0), "{STALE}");
            assert_eq!(queue.events.len(), 1);
        });
    }

    #[test_case(EVERY_STATE, Ending::Completed, Some(FinishedState::Completed), true, None; "completed")]
    #[test_case(EVERY_STATE, Ending::Failed, Some(FinishedState::Failed), true, None; "failed")]
    #[test_case(EVERY_STATE, Ending::Cancelled, Some(FinishedState::Cancelled), false, None; "cancelled")]
    #[test_case(EVERY_STATE, Ending::Paused, Some(FinishedState::Paused), true, Some(PauseReason::CompletionRequired); "paused")]
    #[test_case(DEFAULT_STATES, Ending::Paused, None, true, None; "paused_outside_the_default_states")]
    fn the_publishing_session_fires_once_when_its_work_reaches_a_watched_state(
        trigger: &str,
        ending: Ending,
        state: Option<FinishedState>,
        owned: bool,
        pause_reason: Option<PauseReason>,
    ) {
        smol::block_on(async {
            let group = Group::new();
            let watch = Watch::new(&group, &triggers(trigger), LOG_BODY).await;
            let receipt = group.publish(0).await;
            let work = receipt.queued[0].work.clone();
            let finished = group.end(&work, ending).await;

            watch.checked().await;
            let fired = watch.events().await;
            watch.checked().await;
            let again = watch.events().await;
            watch.stop().await;

            let expected: Vec<WorkFinishedDetail> = state
                .into_iter()
                .map(|state| WorkFinishedDetail {
                    group: GROUP.to_owned(),
                    work: work.clone(),
                    message_id: receipt.message_id.clone(),
                    topic: Some(TOPIC.to_owned()),
                    state,
                    attempts: finished.attempt,
                    max_attempts: finished.max_attempts,
                    member: owned.then(|| handle_address(WORKER_HANDLE)),
                    pause_reason,
                    detail: match state {
                        FinishedState::Completed => &finished.result,
                        _ => &finished.reason,
                    }
                    .as_deref()
                    .map(Untrusted::text),
                })
                .collect();
            assert_eq!(fired, expected);
            assert_eq!(again, fired, "{ONCE}");
        });
    }

    #[test]
    fn a_restart_resumes_the_cursor_and_fires_no_transition_twice() {
        smol::block_on(async {
            let group = Group::new();
            let watch = Watch::new(&group, &triggers(EVERY_STATE), LOG_BODY).await;
            let rewound = group.publisher.work_cursor().await.unwrap();
            let before = group.publish(0).await.queued[0].work.clone();
            group.manage(&before, WorkAction::Cancel).await;
            watch.checked().await;
            let fixture = watch.stop().await;

            let during = group.publish(1).await.queued[0].work.clone();
            for action in [WorkAction::Pause, WorkAction::Retry, WorkAction::Cancel] {
                group.manage(&during, action).await;
            }
            rewind(&fixture, rewound).await;
            let watch = Watch::start(fixture, &group).await;
            watch.checked().await;
            let fired: Vec<(String, FinishedState)> = watch
                .events()
                .await
                .into_iter()
                .map(|event| (event.work, event.state))
                .collect();
            watch.stop().await;

            assert_eq!(
                fired,
                [
                    (before, FinishedState::Cancelled),
                    (during, FinishedState::Cancelled),
                ],
                "{ONCE}"
            );
        });
    }

    #[test]
    fn a_full_queue_holds_the_cursor_back_and_a_limit_defers_rather_than_drops() {
        smol::block_on(async {
            let group = Group::new();
            let fields = format!("{}, {COOLDOWN}", triggers(DEFAULT_STATES));
            let watch = Watch::new(&group, &fields, NOTIFY_BODY).await;
            watch.detach().await;
            let mut works = Vec::new();
            for number in 0..MAX_QUEUED_EVENTS + OVERFLOW {
                let work = group.publish(number).await.queued[0].work.clone();
                group.manage(&work, WorkAction::Cancel).await;
                works.push(work);
            }
            watch.attach();
            watch.next_check().await;
            until(&watch.handle(), |state| {
                count(state, FiringStatus::Deferred) == 1
            })
            .await;
            watch.checked().await;
            let full = watch.works().await;

            watch.fixture.clock().advance(COOLDOWN_DELAY);
            until(&watch.handle(), |state| {
                count(state, FiringStatus::Completed) == 2
                    && count(state, FiringStatus::Deferred) == 1
            })
            .await;
            watch.checked().await;
            let all = watch.works().await;
            let dropped = count(&watch.handle().state(), FiringStatus::Dropped);
            watch.stop().await;

            assert_eq!(full, works[..=MAX_QUEUED_EVENTS], "{NEVER_DROPPED}");
            assert_eq!(all, works, "{DEFERRED}");
            assert_eq!(dropped, 0, "{NEVER_DROPPED}");
        });
    }
}
