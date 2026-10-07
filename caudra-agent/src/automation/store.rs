//! One session's automation persistence, behind a dedicated thread. The thread owns its own
//! `SessionDatabase` connection to `caudra.db`, as the workflow store does, and drains jobs in
//! order, so every awaiting caller sees writes land in the sequence they were issued and the
//! async executor never blocks on SQLite. Reads come back as the `caudra_automation` read model.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use caudra_automation::catalog::Scope;
use caudra_automation::event::TurnOutcome;
use caudra_automation::host::{ActionKind, DeliveryMode};
use caudra_automation::limits::{ActingMarks, DeliveryBackoff, TurnWindow, UnattendedTurns};
use caudra_automation::meta::TriggerKind;
use caudra_automation::replay::{JournalEntry, StoredAction, journal_entry};
use caudra_automation::request::AutomationError;
use caudra_automation::snapshot::{
    ActionBody, ActionRow, ActionStatus, ArmOrigin, AutomationDetail, AutomationHistoryEntry,
    BindingView, ErrorView, FiringDetail, FiringStatus, FiringSummary, MAX_SWARM_FIRINGS,
    OutboxItem, PauseLatch, PauseSource, SessionControls, StateOutcome, StateView, request_summary,
};
use caudra_storage::automation::{
    AUTOMATION_ACTION_FINISHED, AUTOMATION_FIRING_FINISHED, AUTOMATION_FIRING_NOT_WAITING,
    AutomationActionEnd, AutomationActionKind, AutomationActionStatus, AutomationActionSummary,
    AutomationArming, AutomationBindingRow, AutomationDelivery, AutomationEventSeen,
    AutomationFiringEnd, AutomationFiringStatus, AutomationFiringSummary, AutomationMarks,
    AutomationOrigin, AutomationScope, AutomationStartup, AutomationStateOutcome,
    AutomationTrigger, FiringError, FiringFinished, NewAutomationAction, NewAutomationFiring,
    StateWrite,
};
use caudra_storage::id::CaudraId;
use caudra_storage::sessions::{
    SessionDatabase, SessionError, StoredAutomationControls, StoredDeliveryBackoff,
    StoredPauseLatch, StoredPauseSource, StoredTurnWindow, StoredUnattendedTurns,
};
use caudra_storage::{StateDir, StorageError};
use serde_json::Value;
use tracing::warn;

use crate::peers::default_handle;

const THREAD_NAME: &str = "automation-store";
/// The refusals storage gives a firing or action that already moved past waiting.
const NOT_WAITING_REFUSALS: [&str; 3] = [
    AUTOMATION_FIRING_NOT_WAITING,
    AUTOMATION_FIRING_FINISHED,
    AUTOMATION_ACTION_FINISHED,
];

type Job = Box<dyn FnOnce(&Worker) + Send>;

enum Command {
    Run(Job),
    Shutdown,
}

struct Worker {
    database: SessionDatabase,
    session_id: CaudraId,
}

/// Cloneable handle to the storage thread. Dropping every handle without
/// [`Self::shutdown`] still ends the thread once its queue drains.
#[derive(Clone)]
pub struct AutomationStore {
    commands: flume::Sender<Command>,
    thread: Arc<Mutex<Option<JoinHandle<()>>>>,
}

/// A binding as the runtime restores it: what the inspector shows, plus the marks that carry
/// limits, schedules and the `work_finished` cursor across restarts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingRecord {
    pub binding: BindingView,
    pub state: StateView,
    pub limiter: ActingMarks,
    pub schedule: Value,
    pub work_cursor: Value,
}

/// Marks to save; `None` keeps the stored one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BindingMarks {
    pub limiter: Option<ActingMarks>,
    pub schedule: Option<Value>,
    pub work_cursor: Option<Value>,
}

/// A firing still waiting when its session stopped, with the event to queue again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingFiring {
    pub firing: FiringSummary,
    /// Tagged JSON; a cut event is only a preview of it.
    pub event: Value,
    pub event_cut: bool,
}

/// A delivery waiting in the outbox, with the request it delivers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedDelivery {
    pub item: OutboxItem,
    pub request: Value,
    pub request_cut: bool,
}

/// Whose firings a trace read may return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FiringScope {
    /// This session's only, as the model's `history` reads them.
    Session,
    /// Any session's, as the inspector's swarm view reads them.
    Any,
}

/// A read-model enum and the storage enum spelled the same, converted by exhaustive matches, so
/// a variant added on either side fails to compile until it is mapped.
pub(crate) trait Mirrored: Copy + 'static {
    type Row: Copy;
    #[cfg(test)]
    const VARIANTS: &'static [Self];

    fn to_row(self) -> Self::Row;
    fn from_row(row: Self::Row) -> Self;
}

macro_rules! mirrored {
    ($($model:ident = $row:ident { $($variant:ident),+ $(,)? })+) => {$(
        impl Mirrored for $model {
            type Row = $row;
            #[cfg(test)]
            const VARIANTS: &'static [Self] = &[$(Self::$variant),+];

            fn to_row(self) -> $row {
                match self {
                    $(Self::$variant => $row::$variant),+
                }
            }

            fn from_row(row: $row) -> Self {
                match row {
                    $($row::$variant => Self::$variant),+
                }
            }
        }
    )+};
}

mirrored! {
    Scope = AutomationScope { User, Project }
    ArmOrigin = AutomationOrigin { Cli, Profile, Manual, Always, Sdk }
    TriggerKind = AutomationTrigger {
        Armed, Idle, NeedsInput, GoalFinished, MessageReceived, WorkFinished, WorkflowFinished,
        Schedule,
    }
    FiringStatus = AutomationFiringStatus {
        Queued, Deferred, Running, Completed, Skipped, Released, Failed, RateLimited, Cancelled,
        Paused, Dropped, Interrupted,
    }
    StateOutcome = AutomationStateOutcome { Committed, Conflict }
    ActionKind = AutomationActionKind {
        Message, SetGoal, Notify, Http, Reply, Send, Publish, Broadcast, StartWorkflow, Pause, Log,
    }
    ActionStatus = AutomationActionStatus {
        Running, Done, Failed, Refused, Queued, Delivered, Deduplicated, Dropped, Expired,
        Interrupted,
    }
    DeliveryMode = AutomationDelivery { Next, Guide }
}

impl AutomationStore {
    pub fn spawn(state_dir: StateDir, session_id: CaudraId) -> Result<Self, AutomationError> {
        let database = SessionDatabase::open(&state_dir).map_err(storage)?;
        let worker = Worker {
            database,
            session_id,
        };
        let (commands, queue) = flume::unbounded();
        let thread = thread::Builder::new()
            .name(THREAD_NAME.to_owned())
            .spawn(move || {
                for command in queue.iter() {
                    match command {
                        Command::Run(job) => job(&worker),
                        Command::Shutdown => break,
                    }
                }
            })
            .map_err(storage)?;
        Ok(Self {
            commands,
            thread: Arc::new(Mutex::new(Some(thread))),
        })
    }

    /// Arms, re-arms or disarms as `binding` says. A binding that already exists keeps its state
    /// and marks.
    pub async fn bind(&self, binding: BindingView) -> Result<(), AutomationError> {
        self.call(move |worker| {
            let arming = AutomationArming {
                session_id: worker.session_id,
                automation: binding.name,
                scope: binding.scope.to_row(),
                origin: binding.origin.to_row(),
                armed: binding.armed,
                args: binding.args.to_string(),
                args_digest: binding.args_digest,
            };
            worker
                .database
                .upsert_automation_binding(&arming)
                .map_err(|error| worker.owned_write_failed(worker.session_id, error))
        })
        .await
    }

    pub async fn load_binding(
        &self,
        name: String,
    ) -> Result<Option<BindingRecord>, AutomationError> {
        self.call(move |worker| {
            worker
                .database
                .load_automation_binding(worker.session_id, &name)
                .map_err(storage)?
                .map(binding_record)
                .transpose()
        })
        .await
    }

    /// Every binding of the session, by name.
    pub async fn load_bindings(&self) -> Result<Vec<BindingRecord>, AutomationError> {
        self.call(|worker| {
            worker
                .database
                .load_automation_bindings(worker.session_id)
                .map_err(storage)?
                .into_iter()
                .map(binding_record)
                .collect()
        })
        .await
    }

    pub async fn arm(&self, name: String, origin: ArmOrigin) -> Result<(), AutomationError> {
        self.call(move |worker| {
            worker
                .database
                .arm_automation(worker.session_id, &name, origin.to_row())
                .map_err(storage)
        })
        .await
    }

    pub async fn disarm(&self, name: String) -> Result<(), AutomationError> {
        self.call(move |worker| {
            worker
                .database
                .disarm_automation(worker.session_id, &name)
                .map_err(storage)
        })
        .await
    }

    /// Replaces the args and the script digest they were validated against.
    pub async fn set_args(
        &self,
        name: String,
        args: Value,
        args_digest: Option<String>,
    ) -> Result<(), AutomationError> {
        self.call(move |worker| {
            worker
                .database
                .set_automation_args(
                    worker.session_id,
                    &name,
                    &args.to_string(),
                    args_digest.as_deref(),
                )
                .map_err(storage)
        })
        .await
    }

    /// A human edit, applied only at the revision the editor loaded. Returns the new revision.
    pub async fn commit_state(
        &self,
        name: String,
        expected_revision: u64,
        state: Value,
    ) -> Result<u64, AutomationError> {
        self.call(move |worker| {
            let written = worker
                .database
                .commit_automation_state(
                    worker.session_id,
                    &name,
                    expected_revision,
                    &state.to_string(),
                )
                .map_err(storage)?;
            match written {
                StateWrite::Committed { revision } => Ok(revision),
                StateWrite::Conflict { revision } => Err(AutomationError::StateConflict {
                    name,
                    current: revision,
                }),
            }
        })
        .await
    }

    /// Empties the state at whatever revision it is, so a firing running across the clear
    /// commits nothing. Returns the new revision.
    pub async fn clear_state(&self, name: String) -> Result<u64, AutomationError> {
        self.call(move |worker| {
            worker
                .database
                .clear_automation_state(worker.session_id, &name)
                .map_err(storage)
        })
        .await
    }

    pub async fn save_marks(
        &self,
        name: String,
        marks: BindingMarks,
    ) -> Result<(), AutomationError> {
        self.call(move |worker| {
            let marks = AutomationMarks {
                limiter: marks
                    .limiter
                    .map(|limiter| serde_json::to_string(&limiter))
                    .transpose()
                    .map_err(storage)?,
                schedule: marks.schedule.map(|schedule| schedule.to_string()),
                work_cursor: marks.work_cursor.map(|cursor| cursor.to_string()),
            };
            worker
                .database
                .save_automation_marks(worker.session_id, &name, &marks)
                .map_err(storage)
        })
        .await
    }

    /// Keeps the text of a script version a firing ran. Returns whether the version was new.
    pub async fn insert_source(
        &self,
        digest: String,
        source: String,
    ) -> Result<bool, AutomationError> {
        self.call(move |worker| {
            worker
                .database
                .insert_automation_source(worker.session_id, &digest, &source)
                .map_err(|error| worker.owned_write_failed(worker.session_id, error))
        })
        .await
    }

    pub async fn load_source(&self, digest: String) -> Result<Option<String>, AutomationError> {
        self.call(move |worker| {
            worker
                .database
                .load_automation_source(worker.session_id, &digest)
                .map_err(storage)
        })
        .await
    }

    /// Records an event waiting for its automation, with the `delivery` of the message it
    /// consumed, and prunes the automation's oldest finished firings.
    pub async fn insert_firing(
        &self,
        firing: NewAutomationFiring,
        delivery: Option<String>,
    ) -> Result<(), AutomationError> {
        self.call(move |worker| {
            match &delivery {
                Some(delivery) => worker
                    .database
                    .insert_consuming_automation_firing(&firing, delivery),
                None => worker.database.insert_automation_firing(&firing),
            }
            .map_err(|error| worker.owned_write_failed(firing.session_id, error))
        })
        .await
    }

    /// Gives up the message a pending firing consumed, which its session queued for the model
    /// again; `event` says so.
    pub async fn downgrade_firing(
        &self,
        fire_id: String,
        event: String,
    ) -> Result<(), AutomationError> {
        self.call(move |worker| {
            worker
                .database
                .downgrade_automation_firing(&fire_id, &event)
                .map_err(firing_error(&fire_id, None))
        })
        .await
    }

    /// Forgets the delivery a firing held, once its release succeeded or no release can read it.
    pub async fn clear_delivery(&self, fire_id: String) -> Result<(), AutomationError> {
        self.call(move |worker| {
            worker
                .database
                .clear_automation_delivery(&fire_id)
                .map_err(storage)
        })
        .await
    }

    /// Puts a firing back to wait until `until`, as a limit does to a one-shot event. Returns
    /// the attempts so far.
    pub async fn defer_firing(&self, fire_id: String, until: i64) -> Result<u64, AutomationError> {
        self.call(move |worker| {
            worker
                .database
                .defer_automation_firing(&fire_id, stored_millis(until))
                .map_err(firing_error(&fire_id, None))
        })
        .await
    }

    pub async fn start_firing(&self, fire_id: String) -> Result<(), AutomationError> {
        self.call(move |worker| {
            worker
                .database
                .start_automation_firing(&fire_id)
                .map_err(firing_error(&fire_id, None))
        })
        .await
    }

    /// Records a firing's final status, and commits its state in the same transaction.
    pub async fn finish_firing(
        &self,
        fire_id: String,
        end: AutomationFiringEnd,
    ) -> Result<FiringFinished, AutomationError> {
        self.call(move |worker| {
            worker
                .database
                .finish_automation_firing(&fire_id, &end)
                .map_err(firing_error(&fire_id, None))
        })
        .await
    }

    /// Journals an action as it starts, or as it is queued for delivery.
    pub async fn start_action(&self, action: NewAutomationAction) -> Result<(), AutomationError> {
        self.call(move |worker| {
            worker
                .database
                .start_automation_action(&action)
                .map_err(firing_error(&action.fire_id, None))
        })
        .await
    }

    pub async fn finish_action(
        &self,
        fire_id: String,
        seq: u64,
        end: AutomationActionEnd,
    ) -> Result<(), AutomationError> {
        self.call(move |worker| {
            worker
                .database
                .finish_automation_action(&fire_id, seq, &end)
                .map_err(firing_error(&fire_id, Some(seq)))
        })
        .await
    }

    /// Records that the frontend handed a queued item to the agent loop. `false` when the item
    /// is no longer queued.
    pub async fn mark_delivered(&self, fire_id: String, seq: u64) -> Result<bool, AutomationError> {
        self.call(move |worker| {
            worker
                .database
                .mark_automation_delivered(&fire_id, seq)
                .map_err(storage)
        })
        .await
    }

    /// Writes how a settled busy period ended onto every delivery it made. Returns how many
    /// deliveries took it.
    pub async fn record_turn(
        &self,
        deliveries: Vec<(String, u64)>,
        outcome: TurnOutcome,
        cost: Option<f64>,
    ) -> Result<usize, AutomationError> {
        self.call(move |worker| {
            worker
                .database
                .record_automation_turn(&deliveries, outcome_text(outcome), cost)
                .map_err(storage)
        })
        .await
    }

    /// The deliveries waiting in the outbox, oldest first.
    pub async fn load_outbox(&self) -> Result<Vec<QueuedDelivery>, AutomationError> {
        self.call(|worker| {
            worker
                .database
                .load_automation_outbox(worker.session_id)
                .map_err(storage)?
                .into_iter()
                .map(|row| {
                    let request = parse_json(&row.action.request)?;
                    let action = row.action.summary;
                    Ok(QueuedDelivery {
                        item: OutboxItem {
                            automation: row.automation,
                            fire_id: action.fire_id,
                            seq: action.seq,
                            kind: ActionKind::from_row(action.kind),
                            summary: request_summary(&request),
                            delivery: action
                                .delivery
                                .map(DeliveryMode::from_row)
                                .unwrap_or_default(),
                            queued_at: millis(action.started_ms),
                            expires_at: action.expires_ms.map(millis),
                            wait: None,
                        },
                        request,
                        request_cut: action.request_cut,
                    })
                })
                .collect()
        })
        .await
    }

    /// This session's newest firings, of one automation or of all.
    pub async fn load_firings(
        &self,
        name: Option<String>,
        limit: usize,
    ) -> Result<Vec<FiringSummary>, AutomationError> {
        self.call(move |worker| {
            let firings = worker
                .database
                .load_automation_firings(worker.session_id, name.as_deref(), limit)
                .map_err(storage)?;
            Ok(firings.into_iter().map(firing_summary).collect())
        })
        .await
    }

    /// One firing's trace, each action summarized from its request. Out of `scope`, a firing
    /// reads as missing.
    pub async fn load_firing(
        &self,
        fire_id: String,
        scope: FiringScope,
    ) -> Result<Option<FiringDetail>, AutomationError> {
        self.call(move |worker| {
            let Some(detail) = worker
                .database
                .load_automation_firing(&fire_id)
                .map_err(storage)?
                .filter(|detail| {
                    scope == FiringScope::Any
                        || detail.firing.summary.session_id == worker.session_id
                })
            else {
                return Ok(None);
            };
            let actions = detail
                .actions
                .into_iter()
                .map(|action| {
                    let request = worker
                        .database
                        .load_automation_action(&fire_id, action.seq)
                        .map_err(storage)?
                        .map(|row| parse_json(&row.request))
                        .transpose()?;
                    action_row(action, request.as_ref())
                })
                .collect::<Result<_, AutomationError>>()?;
            let firing = detail.firing;
            let error_source = match firing.summary.error.as_ref().and_then(|error| error.line) {
                Some(line) => worker
                    .database
                    .load_automation_source(firing.summary.session_id, &firing.summary.digest)
                    .map_err(storage)?
                    .and_then(|source| source_line(&source, line)),
                None => None,
            };
            Ok(Some(FiringDetail {
                event: parse_json(&firing.event)?,
                event_cut: firing.summary.event_cut,
                state_patch: firing.state_patch.as_deref().map(parse_json).transpose()?,
                patch_cut: firing.summary.patch_cut,
                firing: firing_summary(firing.summary),
                actions,
                error_source,
            }))
        })
        .await
    }

    /// One firing as listings show it, without loading its action bodies.
    pub async fn load_summary(
        &self,
        fire_id: String,
    ) -> Result<Option<FiringSummary>, AutomationError> {
        self.call(move |worker| {
            Ok(worker
                .database
                .load_automation_firing(&fire_id)
                .map_err(storage)?
                .map(|detail| firing_summary(detail.firing.summary)))
        })
        .await
    }

    pub async fn load_action(
        &self,
        fire_id: String,
        seq: u64,
    ) -> Result<Option<ActionBody>, AutomationError> {
        self.call(move |worker| {
            let Some(row) = worker
                .database
                .load_automation_action(&fire_id, seq)
                .map_err(storage)?
            else {
                return Ok(None);
            };
            Ok(Some(ActionBody {
                request: parse_json(&row.request)?,
                result: row.result.as_deref().map(parse_json).transpose()?,
                fire_id: row.summary.fire_id,
                seq: row.summary.seq,
                request_cut: row.summary.request_cut,
                result_cut: row.summary.result_cut,
            }))
        })
        .await
    }

    /// What a firing's `http()` requests got, in call order, as a dry run of it answers them.
    pub async fn load_journal(
        &self,
        fire_id: String,
    ) -> Result<Vec<JournalEntry>, AutomationError> {
        self.call(move |worker| {
            let Some(detail) = worker
                .database
                .load_automation_firing(&fire_id)
                .map_err(storage)?
            else {
                return Ok(Vec::new());
            };
            detail
                .actions
                .into_iter()
                .filter(|action| action.kind == AutomationActionKind::Http)
                .map(|action| {
                    let result = worker
                        .database
                        .load_automation_action(&fire_id, action.seq)
                        .map_err(storage)?
                        .and_then(|row| row.result);
                    Ok(journal_entry(StoredAction {
                        kind: ActionKind::from_row(action.kind),
                        status: ActionStatus::from_row(action.status),
                        request_hash: &action.request_hash,
                        result: result.as_deref(),
                        result_cut: action.result_cut,
                        error: action.error.as_deref(),
                    }))
                })
                .filter_map(Result::transpose)
                .collect()
        })
        .await
    }

    /// The session's pending firings in the order they were queued. After [`Self::interrupt`]
    /// these are the waiting events a resumed session queues again.
    pub async fn load_pending(&self) -> Result<Vec<PendingFiring>, AutomationError> {
        self.call(|worker| {
            worker
                .database
                .load_pending_automation_firings(worker.session_id)
                .map_err(storage)?
                .into_iter()
                .map(|firing| {
                    Ok(PendingFiring {
                        event: parse_json(&firing.event)?,
                        event_cut: firing.summary.event_cut,
                        firing: firing_summary(firing.summary),
                    })
                })
                .collect()
        })
        .await
    }

    /// Whether the automation already has a firing for this message event, and whether that
    /// firing holds the message.
    pub async fn event_seen(
        &self,
        name: String,
        event_key: String,
    ) -> Result<AutomationEventSeen, AutomationError> {
        self.call(move |worker| {
            worker
                .database
                .automation_event_seen(worker.session_id, &name, &event_key)
                .map_err(storage)
        })
        .await
    }

    /// Applies the startup rules before the session arms anything: running firings become
    /// `interrupted`, and waiting events that resume fires again are dropped. Answers with the
    /// consumed messages finished firings still have to release.
    pub async fn interrupt(&self) -> Result<AutomationStartup, AutomationError> {
        self.call(|worker| {
            worker
                .database
                .interrupt_automation_firings(worker.session_id)
                .map_err(storage)
        })
        .await
    }

    /// One automation's binding, state and newest firings, in this session or in the one
    /// `session_id` names.
    pub async fn load_detail(
        &self,
        session_id: Option<CaudraId>,
        name: String,
        limit: usize,
    ) -> Result<AutomationDetail, AutomationError> {
        self.call(move |worker| {
            let session_id = session_id.unwrap_or(worker.session_id);
            let binding = worker
                .database
                .load_automation_binding(session_id, &name)
                .map_err(storage)?;
            let firings = worker
                .database
                .load_automation_firings(session_id, Some(&name), limit)
                .map_err(storage)?;
            Ok(AutomationDetail {
                session_id: session_id.to_string(),
                state: binding.as_ref().map(state_view).transpose()?,
                binding: binding.map(binding_view).transpose()?,
                firings: firings.into_iter().map(firing_summary).collect(),
                name,
            })
        })
        .await
    }

    /// Other sessions with an armed automation or a recent firing, newest activity first, each
    /// with its bindings and newest firings. A session that never stored a messaging name
    /// answers to its [`default_handle`].
    pub async fn load_history(
        &self,
        limit: usize,
    ) -> Result<Vec<AutomationHistoryEntry>, AutomationError> {
        self.call(move |worker| {
            let sessions = worker
                .database
                .load_automation_history(worker.session_id, limit)
                .map_err(storage)?;
            sessions
                .into_iter()
                .map(|session| {
                    let bindings = worker
                        .database
                        .load_automation_bindings(session.session_id)
                        .map_err(storage)?;
                    let firings = worker
                        .database
                        .load_automation_firings(session.session_id, None, MAX_SWARM_FIRINGS)
                        .map_err(storage)?;
                    Ok(AutomationHistoryEntry {
                        session_id: session.session_id.to_string(),
                        title: session.title,
                        handle: session
                            .handle
                            .unwrap_or_else(|| default_handle(session.session_id)),
                        last_activity_at: millis(session.last_activity_ms),
                        bindings: bindings
                            .into_iter()
                            .map(binding_view)
                            .collect::<Result<_, _>>()?,
                        firings: firings.into_iter().map(firing_summary).collect(),
                    })
                })
                .collect()
        })
        .await
    }

    /// Lets queued commands finish, then stops the thread and waits for it.
    pub async fn shutdown(self) {
        let _ = self.commands.send(Command::Shutdown);
        let thread = self
            .thread
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(thread) = thread
            && let Err(error) = smol::unblock(move || thread.join()).await
        {
            warn!(?error, "automation store thread panicked");
        }
    }

    async fn call<T, F>(&self, job: F) -> Result<T, AutomationError>
    where
        T: Send + 'static,
        F: FnOnce(&Worker) -> Result<T, AutomationError> + Send + 'static,
    {
        let (reply, response) = flume::bounded(1);
        let job: Job = Box::new(move |worker| {
            let _ = reply.send(job(worker));
        });
        self.commands
            .send(Command::Run(job))
            .map_err(|_| AutomationError::Unavailable)?;
        response
            .recv_async()
            .await
            .map_err(|_| AutomationError::Unavailable)?
    }
}

impl Worker {
    /// A row that cascades from the session's record failed to insert, perhaps for want of that
    /// record.
    fn owned_write_failed(&self, session_id: CaudraId, error: SessionError) -> AutomationError {
        match self.database.write_version(session_id) {
            Ok(None) => AutomationError::SessionNotSaved {
                session_id: session_id.to_string(),
            },
            _ => storage(error),
        }
    }
}

/// What `SessionMeta.automations` keeps: the runtime's session counters, and the armed names
/// sorted.
pub fn stored_controls(
    controls: &SessionControls,
    mut armed: Vec<String>,
) -> StoredAutomationControls {
    armed.sort_unstable();
    armed.dedup();
    StoredAutomationControls {
        pause: controls.pause.as_ref().map(|latch| StoredPauseLatch {
            reason: latch.reason.clone(),
            source: match &latch.source {
                PauseSource::User => StoredPauseSource::User,
                PauseSource::Sdk => StoredPauseSource::Sdk,
                PauseSource::Script { automation } => StoredPauseSource::Script {
                    automation: automation.clone(),
                },
                PauseSource::Inspector => StoredPauseSource::Inspector,
            },
            at: latch.at,
        }),
        turn_window: StoredTurnWindow {
            turns: controls.turn_window.turns.clone(),
        },
        unattended: StoredUnattendedTurns {
            count: controls.unattended.count,
        },
        delivery_backoff: StoredDeliveryBackoff {
            errors: controls.delivery_backoff.errors,
            until: controls.delivery_backoff.until,
        },
        armed,
    }
}

/// The runtime's session counters and the armed names a saved session kept.
pub fn session_controls(stored: StoredAutomationControls) -> (SessionControls, Vec<String>) {
    let controls = SessionControls {
        pause: stored.pause.map(|latch| PauseLatch {
            reason: latch.reason,
            source: match latch.source {
                StoredPauseSource::User => PauseSource::User,
                StoredPauseSource::Sdk => PauseSource::Sdk,
                StoredPauseSource::Script { automation } => PauseSource::Script { automation },
                StoredPauseSource::Inspector => PauseSource::Inspector,
            },
            at: latch.at,
        }),
        turn_window: TurnWindow {
            turns: stored.turn_window.turns,
        },
        unattended: UnattendedTurns {
            count: stored.unattended.count,
        },
        delivery_backoff: DeliveryBackoff {
            errors: stored.delivery_backoff.errors,
            until: stored.delivery_backoff.until,
        },
    };
    (controls, stored.armed)
}

fn binding_record(row: AutomationBindingRow) -> Result<BindingRecord, AutomationError> {
    Ok(BindingRecord {
        state: state_view(&row)?,
        limiter: acting_marks(&row.limiter)?,
        schedule: parse_json(&row.schedule)?,
        work_cursor: parse_json(&row.work_cursor)?,
        binding: binding_view(row)?,
    })
}

fn binding_view(row: AutomationBindingRow) -> Result<BindingView, AutomationError> {
    Ok(BindingView {
        args: parse_json(&row.args)?,
        name: row.automation,
        scope: Scope::from_row(row.scope),
        origin: ArmOrigin::from_row(row.origin),
        armed: row.armed,
        args_digest: row.args_digest,
    })
}

fn state_view(row: &AutomationBindingRow) -> Result<StateView, AutomationError> {
    Ok(StateView {
        value: parse_json(&row.state)?,
        revision: row.state_revision,
        writer: row.state_writer.clone(),
        digest: row.state_digest.clone(),
        written_at: row.state_written_ms.map(millis),
    })
}

/// A new binding stores `{}` until the runtime first saves its limiter marks.
fn acting_marks(text: &str) -> Result<ActingMarks, AutomationError> {
    match parse_json(text)? {
        Value::Object(fields) if fields.is_empty() => Ok(ActingMarks::default()),
        marks => serde_json::from_value(marks).map_err(storage),
    }
}

fn firing_summary(summary: AutomationFiringSummary) -> FiringSummary {
    FiringSummary {
        fire_id: summary.fire_id,
        automation: summary.automation,
        digest: summary.digest,
        trigger: TriggerKind::from_row(summary.trigger),
        trigger_index: summary.trigger_index,
        event_key: summary.event_key,
        consumed: summary.consumed,
        status: FiringStatus::from_row(summary.status),
        reason: summary.reason,
        error: summary.error.map(error_view),
        repeats: summary.repeats,
        attempts: summary.attempts,
        operations: summary.operations,
        state_outcome: summary.state_outcome.map(StateOutcome::from_row),
        queued_at: millis(summary.queued_ms),
        deferred_until: summary.deferred_until_ms.map(millis),
        started_at: summary.started_ms.map(millis),
        finished_at: summary.finished_ms.map(millis),
        action_count: summary.action_count,
        first_action: summary.first_action.map(ActionKind::from_row),
    }
}

fn action_row(
    action: AutomationActionSummary,
    request: Option<&Value>,
) -> Result<ActionRow, AutomationError> {
    Ok(ActionRow {
        seq: action.seq,
        kind: ActionKind::from_row(action.kind),
        line: action.line,
        column: action.column,
        status: ActionStatus::from_row(action.status),
        summary: request.map(request_summary).unwrap_or_default(),
        error: action.error,
        target: action.target,
        delivery: action.delivery.map(DeliveryMode::from_row),
        expires_at: action.expires_ms.map(millis),
        wait: None,
        turn_outcome: action.turn_outcome.map(parse_outcome).transpose()?,
        turn_cost: action.turn_cost,
        started_at: millis(action.started_ms),
        finished_at: action.finished_ms.map(millis),
        delivered_at: action.delivered_ms.map(millis),
        request_cut: action.request_cut,
        result_cut: action.result_cut,
    })
}

/// How a delivery's turn ended, spelled as the read model's serde spells it.
const fn outcome_text(outcome: TurnOutcome) -> &'static str {
    match outcome {
        TurnOutcome::Completed => "completed",
        TurnOutcome::Error => "error",
        TurnOutcome::Cancelled => "cancelled",
        TurnOutcome::MaxTurns => "max_turns",
    }
}

fn parse_outcome(text: String) -> Result<TurnOutcome, AutomationError> {
    serde_json::from_value(Value::String(text)).map_err(storage)
}

pub(super) fn error_view(error: FiringError) -> ErrorView {
    ErrorView {
        kind: error.kind,
        message: error.message,
        line: error.line,
        column: error.column,
    }
}

/// Line `line` of `source`, counted from 1 as Rhai counts it.
pub(super) fn source_line(source: &str, line: u32) -> Option<String> {
    let index = usize::try_from(line).ok()?.checked_sub(1)?;
    source.lines().nth(index).map(str::to_owned)
}

fn parse_json(text: &str) -> Result<Value, AutomationError> {
    serde_json::from_str(text).map_err(storage)
}

/// Storage keeps unsigned milliseconds that it read from SQLite's signed integers.
fn millis(stored: u64) -> i64 {
    i64::try_from(stored).unwrap_or(i64::MAX)
}

/// A time before the epoch has already passed.
fn stored_millis(at: i64) -> u64 {
    u64::try_from(at).unwrap_or_default()
}

/// A refused firing or action write as the request contract names it. A missing action, unlike
/// a missing firing, keeps storage's message, which names the action.
fn firing_error(
    fire_id: &str,
    seq: Option<u64>,
) -> impl FnOnce(SessionError) -> AutomationError + '_ {
    move |error| match error {
        SessionError::Storage(StorageError::NotFound(_)) if seq.is_none() => {
            AutomationError::UnknownFiring {
                fire_id: fire_id.to_owned(),
            }
        }
        SessionError::Storage(StorageError::Io(ref refusal))
            if NOT_WAITING_REFUSALS.contains(&refusal.to_string().as_str()) =>
        {
            AutomationError::NotWaiting {
                fire_id: fire_id.to_owned(),
                seq,
            }
        }
        error => storage(error),
    }
}

fn storage(error: impl fmt::Display) -> AutomationError {
    AutomationError::Storage(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::fmt::Debug;

    use caudra_storage::sessions::StoredPeerControls;
    use serde::Serialize;
    use serde_json::json;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::StoredSession;

    const CWD: &str = "/project";
    const MODEL: &str = "test/model";
    const NAME: &str = "ci-watch";
    const OTHER_NAME: &str = "keep-going";
    const PAUSING_NAME: &str = "goal-chain";
    const FIRE_ID: &str = "fire-1";
    const RUNNING_FIRE_ID: &str = "fire-running";
    const SUPERSEDED_FIRE_ID: &str = "fire-superseded";
    const MISSING_FIRE_ID: &str = "fire-missing";
    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const SOURCE: &str = "let meta = #{ triggers: [#{ on: \"idle\" }] };";
    const REQUEST_HASH: &str = "request-hash";
    const EVENT_KEY: &str = "delivery-1";
    const OTHER_EVENT_KEY: &str = "delivery-2";
    const MESSAGE_TEXT: &str = "look at CI";
    const PAUSE_REASON: &str = "Esc Esc";
    const SOURCE_LINE: u32 = 3;
    const FAILING_SOURCE: &str = "let meta = #{};\nlet count = 1;\nlet ratio = count / 0;\n";
    const FAILING_LINE: &str = "let ratio = count / 0;";
    const ERROR_KIND: &str = "script";
    const ERROR_MESSAGE: &str = "Division by zero";
    const ERROR_SHOWS_ITS_LINE: &str =
        "a failed firing's trace must quote the line its error points at";
    const SOURCE_COLUMN: u32 = 5;
    const OPERATIONS: u64 = 2;
    const LIMIT: usize = 10;
    const TURN_COST: f64 = 0.25;
    const ACTED_AT: i64 = 1_700_000_000_000;
    const PAUSED_AT: i64 = 1_700_000_100_000;
    const BACKOFF_UNTIL: i64 = 1_700_000_200_000;
    const DEFER_UNTIL: i64 = 4_102_444_800_000;
    const UNATTENDED: u32 = 3;
    const BACKOFF_ERRORS: u32 = 2;
    const MAPPING_ROUND_TRIPS: &str = "every variant must map to storage and back to itself";
    const SPELLINGS_AGREE: &str = "the read model must spell a variant as storage's CHECK does";
    const MISSING_SESSION_IS_CLEAR: &str = "a write for an unsaved session must say so";
    const NEW_BINDING_IS_BLANK: &str = "a new binding must read with empty state and no marks";
    const STALE_EDIT_LOSES: &str = "an edit at an old revision must conflict";
    const MARKS_SURVIVE: &str = "saved marks must read back, and unsaved ones stay";
    const OUTBOX_SUMMARIZES: &str = "an outbox item must summarize the request it delivers";
    const DELIVERED_ONCE: &str = "an item must be delivered once";
    const TRACE_READS_BACK: &str = "a firing's trace must read back as the read model";
    const REFUSALS_ARE_TYPED: &str = "a firing past waiting must refuse with a request error";
    const RESTORE_KEEPS_ONE_SHOTS: &str =
        "restore must keep a waiting one-shot event with its payload";
    const RESTORE_INTERRUPTS: &str = "restore must interrupt a running firing";
    const HISTORY_LISTS_OTHERS: &str =
        "history must list other sessions with their bindings and firings";
    const ANY_SCOPE_READS_OTHERS: &str = "a trace of any session must read another's firing";
    const SESSION_SCOPE_STAYS_HOME: &str = "a trace of this session must not read another's firing";
    const CONTROLS_SURVIVE: &str = "the session controls must survive the session meta";
    const SHAPES_AGREE: &str = "a stored part must serialize like its runtime counterpart";
    const CLOSED_IS_UNAVAILABLE: &str = "a stopped store must refuse rather than hang";
    const ANSWERS_TO_ITS_NAME: &str =
        "a session must be listed by its stored messaging name, or else by its default one";
    const STORED_HANDLE: &str = "night-shift";

    fn open() -> (TempDir, StateDir, CaudraId) {
        let temp = TempDir::new().unwrap();
        let state_dir = StateDir::from_path(temp.path().to_path_buf());
        let mut session = StoredSession::new(MODEL, CWD);
        session.save(&state_dir).unwrap();
        (temp, state_dir, session.id)
    }

    fn binding() -> BindingView {
        BindingView {
            name: NAME.into(),
            scope: Scope::User,
            origin: ArmOrigin::Manual,
            armed: true,
            args: json!({"branch": "main"}),
            args_digest: Some(DIGEST.into()),
        }
    }

    fn event() -> Value {
        json!({"kind": "idle", "outcome": "completed"})
    }

    fn request() -> Value {
        json!({"kind": "message", "text": MESSAGE_TEXT})
    }

    fn firing(session_id: CaudraId, fire_id: &str, trigger: TriggerKind) -> NewAutomationFiring {
        NewAutomationFiring {
            fire_id: fire_id.into(),
            session_id,
            automation: NAME.into(),
            digest: DIGEST.into(),
            trigger: trigger.to_row(),
            trigger_index: 0,
            event: event().to_string(),
            event_key: None,
            consumed: false,
        }
    }

    fn action(fire_id: &str, kind: ActionKind, status: ActionStatus) -> NewAutomationAction {
        NewAutomationAction {
            fire_id: fire_id.into(),
            seq: 0,
            kind: kind.to_row(),
            line: Some(SOURCE_LINE),
            column: Some(SOURCE_COLUMN),
            request_hash: REQUEST_HASH.into(),
            request: request().to_string(),
            status: status.to_row(),
            delivery: (status == ActionStatus::Queued).then_some(DeliveryMode::Next.to_row()),
            expires_ms: None,
        }
    }

    fn ending(status: FiringStatus) -> AutomationFiringEnd {
        AutomationFiringEnd {
            status: status.to_row(),
            reason: None,
            error: None,
            operations: OPERATIONS,
            state_patch: None,
            commit: None,
        }
    }

    fn done() -> AutomationActionEnd {
        AutomationActionEnd {
            status: ActionStatus::Done.to_row(),
            result: Some(json!({"status": 200}).to_string()),
            error: None,
            target: None,
        }
    }

    fn mirrors<M>()
    where
        M: Mirrored + Serialize + PartialEq + Debug,
        M::Row: fmt::Display,
    {
        for &variant in M::VARIANTS {
            let row = variant.to_row();
            assert_eq!(M::from_row(row), variant, "{MAPPING_ROUND_TRIPS}");
            assert_eq!(
                serde_json::to_value(variant).unwrap(),
                json!(row.to_string()),
                "{SPELLINGS_AGREE}"
            );
        }
    }

    #[test_case(mirrors::<Scope>; "scope")]
    #[test_case(mirrors::<ArmOrigin>; "arm_origin")]
    #[test_case(mirrors::<TriggerKind>; "trigger_kind")]
    #[test_case(mirrors::<FiringStatus>; "firing_status")]
    #[test_case(mirrors::<StateOutcome>; "state_outcome")]
    #[test_case(mirrors::<ActionKind>; "action_kind")]
    #[test_case(mirrors::<ActionStatus>; "action_status")]
    #[test_case(mirrors::<DeliveryMode>; "delivery_mode")]
    fn every_variant_maps_both_ways(check: fn()) {
        check();
    }

    #[test_case(TurnOutcome::Completed; "completed")]
    #[test_case(TurnOutcome::Error; "error")]
    #[test_case(TurnOutcome::Cancelled; "cancelled")]
    #[test_case(TurnOutcome::MaxTurns; "max_turns")]
    fn turn_outcomes_keep_their_spelling(outcome: TurnOutcome) {
        let text = outcome_text(outcome);

        assert_eq!(
            serde_json::to_value(outcome).unwrap(),
            json!(text),
            "{SPELLINGS_AGREE}"
        );
        assert_eq!(
            parse_outcome(text.to_owned()),
            Ok(outcome),
            "{MAPPING_ROUND_TRIPS}"
        );
    }

    #[test_case(PauseSource::User; "user")]
    #[test_case(PauseSource::Sdk; "sdk")]
    #[test_case(PauseSource::Script { automation: PAUSING_NAME.into() }; "script")]
    #[test_case(PauseSource::Inspector; "inspector")]
    fn session_controls_survive_the_session_meta(source: PauseSource) {
        let controls = SessionControls {
            pause: Some(PauseLatch {
                reason: PAUSE_REASON.into(),
                source,
                at: PAUSED_AT,
            }),
            turn_window: TurnWindow {
                turns: vec![ACTED_AT, PAUSED_AT],
            },
            unattended: UnattendedTurns { count: UNATTENDED },
            delivery_backoff: DeliveryBackoff {
                errors: BACKOFF_ERRORS,
                until: Some(BACKOFF_UNTIL),
            },
        };
        let armed = vec![
            OTHER_NAME.to_owned(),
            NAME.to_owned(),
            OTHER_NAME.to_owned(),
        ];

        let stored = stored_controls(&controls, armed);

        assert_eq!(stored.armed, [NAME, OTHER_NAME]);
        assert_eq!(
            serde_json::to_value(&stored.pause).unwrap(),
            serde_json::to_value(&controls.pause).unwrap(),
            "{SHAPES_AGREE}"
        );
        assert_eq!(
            serde_json::to_value(&stored.turn_window).unwrap(),
            serde_json::to_value(&controls.turn_window).unwrap(),
            "{SHAPES_AGREE}"
        );
        assert_eq!(
            serde_json::to_value(&stored.unattended).unwrap(),
            serde_json::to_value(&controls.unattended).unwrap(),
            "{SHAPES_AGREE}"
        );
        assert_eq!(
            serde_json::to_value(&stored.delivery_backoff).unwrap(),
            serde_json::to_value(&controls.delivery_backoff).unwrap(),
            "{SHAPES_AGREE}"
        );
        assert_eq!(
            session_controls(stored),
            (controls, vec![NAME.to_owned(), OTHER_NAME.to_owned()]),
            "{CONTROLS_SURVIVE}"
        );
    }

    #[test]
    fn writes_for_an_unsaved_session_say_so() {
        smol::block_on(async {
            let temp = TempDir::new().unwrap();
            let state_dir = StateDir::from_path(temp.path().to_path_buf());
            let session_id = CaudraId::generate();
            let store = AutomationStore::spawn(state_dir, session_id).unwrap();
            let not_saved = AutomationError::SessionNotSaved {
                session_id: session_id.to_string(),
            };

            assert_eq!(
                store.bind(binding()).await,
                Err(not_saved.clone()),
                "{MISSING_SESSION_IS_CLEAR}"
            );
            assert_eq!(
                store.insert_source(DIGEST.into(), SOURCE.into()).await,
                Err(not_saved.clone()),
                "{MISSING_SESSION_IS_CLEAR}"
            );
            assert_eq!(
                store
                    .insert_firing(firing(session_id, FIRE_ID, TriggerKind::Armed), None)
                    .await,
                Err(not_saved),
                "{MISSING_SESSION_IS_CLEAR}"
            );
            store.shutdown().await;
        });
    }

    #[test]
    fn bindings_read_back_as_the_read_model() {
        smol::block_on(async {
            let (_temp, state_dir, session_id) = open();
            let store = AutomationStore::spawn(state_dir, session_id).unwrap();
            let marks = ActingMarks {
                acting: vec![ACTED_AT],
                failure_streak: 1,
                backoff_until: Some(BACKOFF_UNTIL),
            };
            let schedule = json!({"last": ACTED_AT});
            let state = json!({"seen": 1});
            let args = json!({"branch": "next"});

            store.bind(binding()).await.unwrap();
            let fresh = store.load_binding(NAME.into()).await.unwrap().unwrap();
            let committed = store.commit_state(NAME.into(), 0, state.clone()).await;
            let stale = store.commit_state(NAME.into(), 0, state.clone()).await;
            store
                .save_marks(
                    NAME.into(),
                    BindingMarks {
                        limiter: Some(marks.clone()),
                        schedule: Some(schedule.clone()),
                        ..BindingMarks::default()
                    },
                )
                .await
                .unwrap();
            store.disarm(NAME.into()).await.unwrap();
            store
                .set_args(NAME.into(), args.clone(), None)
                .await
                .unwrap();
            let edited = store.load_binding(NAME.into()).await.unwrap().unwrap();

            assert_eq!(fresh.binding, binding());
            assert_eq!(
                (&fresh.state.value, fresh.state.revision, &fresh.limiter),
                (&json!({}), 0, &ActingMarks::default()),
                "{NEW_BINDING_IS_BLANK}"
            );
            assert_eq!(committed, Ok(1));
            assert_eq!(
                stale,
                Err(AutomationError::StateConflict {
                    name: NAME.into(),
                    current: 1,
                }),
                "{STALE_EDIT_LOSES}"
            );
            assert_eq!(
                (&edited.limiter, &edited.schedule, &edited.work_cursor),
                (&marks, &schedule, &json!({})),
                "{MARKS_SURVIVE}"
            );
            assert_eq!((&edited.state.value, edited.state.revision), (&state, 1));
            assert!(edited.state.written_at.is_some());
            assert_eq!(
                edited.binding,
                BindingView {
                    armed: false,
                    args,
                    args_digest: None,
                    ..binding()
                }
            );

            store.arm(NAME.into(), ArmOrigin::Cli).await.unwrap();
            assert_eq!(store.clear_state(NAME.into()).await, Ok(2));
            let records = store.load_bindings().await.unwrap();
            assert_eq!(records.len(), 1);
            assert_eq!(
                (records[0].binding.origin, records[0].binding.armed),
                (ArmOrigin::Cli, true)
            );
            assert_eq!(records[0].state.value, json!({}));
            store.shutdown().await;
        });
    }

    #[test_case(1 => Some("first".to_owned()); "first")]
    #[test_case(3 => Some("third".to_owned()); "last")]
    #[test_case(0 => None; "zero")]
    #[test_case(4 => None; "past_the_end")]
    fn source_lines_count_from_one(line: u32) -> Option<String> {
        source_line("first\nsecond\nthird\n", line)
    }

    #[test]
    fn a_failed_firing_quotes_the_line_its_error_points_at() {
        smol::block_on(async {
            let (_temp, state_dir, session_id) = open();
            let store = AutomationStore::spawn(state_dir, session_id).unwrap();
            store.bind(binding()).await.unwrap();
            store
                .insert_source(DIGEST.into(), FAILING_SOURCE.into())
                .await
                .unwrap();
            store
                .insert_firing(firing(session_id, FIRE_ID, TriggerKind::Idle), None)
                .await
                .unwrap();
            store.start_firing(FIRE_ID.into()).await.unwrap();
            let failed = AutomationFiringEnd {
                error: Some(FiringError {
                    kind: ERROR_KIND.into(),
                    message: ERROR_MESSAGE.into(),
                    line: Some(SOURCE_LINE),
                    column: Some(SOURCE_COLUMN),
                }),
                ..ending(FiringStatus::Failed)
            };
            store.finish_firing(FIRE_ID.into(), failed).await.unwrap();

            let detail = store
                .load_firing(FIRE_ID.into(), FiringScope::Session)
                .await
                .unwrap()
                .unwrap();

            assert_eq!(
                detail.error_source.as_deref(),
                Some(FAILING_LINE),
                "{ERROR_SHOWS_ITS_LINE}"
            );
            store.shutdown().await;
        });
    }

    #[test]
    fn a_firing_trace_reads_back_as_the_read_model() {
        smol::block_on(async {
            let (_temp, state_dir, session_id) = open();
            let store = AutomationStore::spawn(state_dir, session_id).unwrap();
            store.bind(binding()).await.unwrap();
            assert!(
                store
                    .insert_source(DIGEST.into(), SOURCE.into())
                    .await
                    .unwrap()
            );
            assert!(
                !store
                    .insert_source(DIGEST.into(), SOURCE.into())
                    .await
                    .unwrap()
            );
            let keyed = NewAutomationFiring {
                event_key: Some(EVENT_KEY.into()),
                ..firing(session_id, FIRE_ID, TriggerKind::Idle)
            };
            store.insert_firing(keyed, None).await.unwrap();
            store.start_firing(FIRE_ID.into()).await.unwrap();
            store
                .start_action(action(FIRE_ID, ActionKind::Message, ActionStatus::Queued))
                .await
                .unwrap();

            let outbox = store.load_outbox().await.unwrap();
            let delivered = store.mark_delivered(FIRE_ID.into(), 0).await.unwrap();
            let redelivered = store.mark_delivered(FIRE_ID.into(), 0).await.unwrap();
            let recorded = store
                .record_turn(
                    vec![(FIRE_ID.into(), 0)],
                    TurnOutcome::Completed,
                    Some(TURN_COST),
                )
                .await;
            store
                .finish_firing(FIRE_ID.into(), ending(FiringStatus::Completed))
                .await
                .unwrap();
            let detail = store
                .load_firing(FIRE_ID.into(), FiringScope::Session)
                .await
                .unwrap()
                .unwrap();
            let body = store.load_action(FIRE_ID.into(), 0).await.unwrap().unwrap();

            assert_eq!(
                store.load_source(DIGEST.into()).await.unwrap().as_deref(),
                Some(SOURCE)
            );
            assert_eq!(outbox.len(), 1);
            assert_eq!(outbox[0].item.summary, MESSAGE_TEXT, "{OUTBOX_SUMMARIZES}");
            assert_eq!(
                (outbox[0].item.kind, outbox[0].item.delivery),
                (ActionKind::Message, DeliveryMode::Next)
            );
            assert_eq!(
                (&outbox[0].request, outbox[0].request_cut),
                (&request(), false)
            );
            assert!(delivered && !redelivered, "{DELIVERED_ONCE}");
            assert_eq!(recorded, Ok(1));
            assert_eq!(
                (detail.firing.status, detail.firing.trigger),
                (FiringStatus::Completed, TriggerKind::Idle),
                "{TRACE_READS_BACK}"
            );
            assert_eq!(
                (detail.firing.operations, detail.firing.first_action),
                (OPERATIONS, Some(ActionKind::Message))
            );
            assert_eq!((&detail.event, detail.event_cut), (&event(), false));
            let row = &detail.actions[0];
            assert_eq!(
                (row.status, row.summary.as_str()),
                (ActionStatus::Delivered, MESSAGE_TEXT),
                "{TRACE_READS_BACK}"
            );
            assert_eq!(
                (row.turn_outcome, row.turn_cost),
                (Some(TurnOutcome::Completed), Some(TURN_COST))
            );
            assert!(row.delivered_at.is_some());
            assert_eq!((&body.request, body.result.as_ref()), (&request(), None));
            assert_eq!(
                store.load_firings(None, LIMIT).await.unwrap(),
                [detail.firing]
            );
            assert!(
                store
                    .load_firing(MISSING_FIRE_ID.into(), FiringScope::Any)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                store.event_seen(NAME.into(), EVENT_KEY.into()).await,
                Ok(AutomationEventSeen::Handed)
            );
            assert_eq!(
                store.event_seen(NAME.into(), OTHER_EVENT_KEY.into()).await,
                Ok(AutomationEventSeen::Unseen)
            );
            store.shutdown().await;
        });
    }

    #[test]
    fn firings_past_waiting_refuse_with_request_errors() {
        smol::block_on(async {
            let (_temp, state_dir, session_id) = open();
            let store = AutomationStore::spawn(state_dir, session_id).unwrap();
            store
                .insert_firing(firing(session_id, FIRE_ID, TriggerKind::Armed), None)
                .await
                .unwrap();
            store.start_firing(FIRE_ID.into()).await.unwrap();
            store
                .start_action(action(FIRE_ID, ActionKind::Http, ActionStatus::Running))
                .await
                .unwrap();
            store
                .finish_action(FIRE_ID.into(), 0, done())
                .await
                .unwrap();
            store
                .finish_firing(FIRE_ID.into(), ending(FiringStatus::Completed))
                .await
                .unwrap();
            let firing_not_waiting = AutomationError::NotWaiting {
                fire_id: FIRE_ID.into(),
                seq: None,
            };

            assert_eq!(
                store.start_firing(FIRE_ID.into()).await,
                Err(firing_not_waiting.clone()),
                "{REFUSALS_ARE_TYPED}"
            );
            assert_eq!(
                store.defer_firing(FIRE_ID.into(), DEFER_UNTIL).await,
                Err(firing_not_waiting.clone()),
                "{REFUSALS_ARE_TYPED}"
            );
            assert_eq!(
                store
                    .finish_firing(FIRE_ID.into(), ending(FiringStatus::Dropped))
                    .await
                    .unwrap_err(),
                firing_not_waiting,
                "{REFUSALS_ARE_TYPED}"
            );
            assert_eq!(
                store.finish_action(FIRE_ID.into(), 0, done()).await,
                Err(AutomationError::NotWaiting {
                    fire_id: FIRE_ID.into(),
                    seq: Some(0),
                }),
                "{REFUSALS_ARE_TYPED}"
            );
            assert_eq!(
                store.start_firing(MISSING_FIRE_ID.into()).await,
                Err(AutomationError::UnknownFiring {
                    fire_id: MISSING_FIRE_ID.into(),
                }),
                "{REFUSALS_ARE_TYPED}"
            );
            store.shutdown().await;
        });
    }

    #[test]
    fn restore_interrupts_running_firings_and_keeps_waiting_one_shots() {
        smol::block_on(async {
            let (_temp, state_dir, session_id) = open();
            let store = AutomationStore::spawn(state_dir, session_id).unwrap();
            store
                .insert_firing(firing(session_id, RUNNING_FIRE_ID, TriggerKind::Idle), None)
                .await
                .unwrap();
            store.start_firing(RUNNING_FIRE_ID.into()).await.unwrap();
            store
                .insert_firing(
                    firing(session_id, SUPERSEDED_FIRE_ID, TriggerKind::Armed),
                    None,
                )
                .await
                .unwrap();
            store
                .insert_firing(firing(session_id, FIRE_ID, TriggerKind::GoalFinished), None)
                .await
                .unwrap();
            let attempts = store.defer_firing(FIRE_ID.into(), DEFER_UNTIL).await;

            let startup = store.interrupt().await.unwrap();
            let pending = store.load_pending().await.unwrap();
            let running = store
                .load_firing(RUNNING_FIRE_ID.into(), FiringScope::Session)
                .await
                .unwrap()
                .unwrap();

            assert_eq!(attempts, Ok(1));
            assert_eq!(
                startup,
                AutomationStartup {
                    interrupted: 1,
                    dropped: 1,
                    releases: Vec::new(),
                }
            );
            assert_eq!(
                running.firing.status,
                FiringStatus::Interrupted,
                "{RESTORE_INTERRUPTS}"
            );
            assert_eq!(pending.len(), 1, "{RESTORE_KEEPS_ONE_SHOTS}");
            let waiting = &pending[0];
            assert_eq!(
                (waiting.firing.fire_id.as_str(), waiting.firing.status),
                (FIRE_ID, FiringStatus::Deferred),
                "{RESTORE_KEEPS_ONE_SHOTS}"
            );
            assert_eq!(waiting.firing.deferred_until, Some(DEFER_UNTIL));
            assert_eq!(
                (&waiting.event, waiting.event_cut),
                (&event(), false),
                "{RESTORE_KEEPS_ONE_SHOTS}"
            );
            store.shutdown().await;
        });
    }

    #[test]
    fn history_detail_and_any_scope_traces_read_other_sessions() {
        smol::block_on(async {
            let (_temp, state_dir, session_id) = open();
            let mut other = StoredSession::new(MODEL, CWD);
            other.save(&state_dir).unwrap();
            let store = AutomationStore::spawn(state_dir.clone(), session_id).unwrap();
            let other_store = AutomationStore::spawn(state_dir, other.id).unwrap();
            other_store.bind(binding()).await.unwrap();
            other_store
                .insert_firing(firing(other.id, FIRE_ID, TriggerKind::Armed), None)
                .await
                .unwrap();

            let history = store.load_history(LIMIT).await.unwrap();
            let detail = store
                .load_detail(Some(other.id), NAME.into(), LIMIT)
                .await
                .unwrap();
            let own = store.load_detail(None, NAME.into(), LIMIT).await.unwrap();
            let any_trace = store
                .load_firing(FIRE_ID.into(), FiringScope::Any)
                .await
                .unwrap();
            let session_trace = store
                .load_firing(FIRE_ID.into(), FiringScope::Session)
                .await
                .unwrap();

            assert_eq!(
                any_trace.map(|trace| trace.firing.fire_id),
                Some(FIRE_ID.to_owned()),
                "{ANY_SCOPE_READS_OTHERS}"
            );
            assert!(session_trace.is_none(), "{SESSION_SCOPE_STAYS_HOME}");

            assert_eq!(history.len(), 1, "{HISTORY_LISTS_OTHERS}");
            assert_eq!(history[0].session_id, other.id.to_string());
            assert_eq!(history[0].bindings, [binding()], "{HISTORY_LISTS_OTHERS}");
            assert_eq!(history[0].firings.len(), 1, "{HISTORY_LISTS_OTHERS}");
            assert_eq!(detail.session_id, other.id.to_string());
            assert_eq!(detail.binding, Some(binding()));
            assert_eq!(detail.state.map(|state| state.revision), Some(0));
            assert_eq!(detail.firings, history[0].firings);
            assert_eq!(
                (own.session_id, own.binding, own.state, own.firings.len()),
                (session_id.to_string(), None, None, 0)
            );
            store.shutdown().await;
            other_store.shutdown().await;
        });
    }

    #[test_case(Some(STORED_HANDLE); "a_stored_name")]
    #[test_case(None; "no_stored_name")]
    fn history_names_a_session_by_its_stored_or_default_handle(stored: Option<&str>) {
        smol::block_on(async {
            let (_temp, state_dir, session_id) = open();
            let mut other = StoredSession::new(MODEL, CWD);
            other.meta.peer_controls = stored.map(|handle| StoredPeerControls {
                handle: Some(handle.to_owned()),
                ..StoredPeerControls::default()
            });
            other.save(&state_dir).unwrap();
            let other_store = AutomationStore::spawn(state_dir.clone(), other.id).unwrap();
            other_store.bind(binding()).await.unwrap();
            other_store.shutdown().await;
            let store = AutomationStore::spawn(state_dir, session_id).unwrap();

            let history = store.load_history(LIMIT).await.unwrap();

            let handle = stored.map_or_else(|| default_handle(other.id), str::to_owned);
            assert_eq!(history[0].handle, handle, "{ANSWERS_TO_ITS_NAME}");
            store.shutdown().await;
        });
    }

    #[test]
    fn shutdown_joins_and_later_calls_are_unavailable() {
        smol::block_on(async {
            let (_temp, state_dir, session_id) = open();
            let store = AutomationStore::spawn(state_dir, session_id).unwrap();
            let handle = store.clone();

            store.shutdown().await;

            assert!(
                handle.thread.lock().unwrap().is_none(),
                "{CLOSED_IS_UNAVAILABLE}"
            );
            assert_eq!(
                handle.load_bindings().await,
                Err(AutomationError::Unavailable),
                "{CLOSED_IS_UNAVAILABLE}"
            );
            handle.shutdown().await;
        });
    }
}
