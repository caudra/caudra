//! A task is an agent chat or a supervised shell job. The main chat
//! goes by [`MAIN_TASK_ID`] and carries no status, since its work is the
//! session's own and `caudra.session.live()` already reports that. The
//! `/tasks` picker lists agents only; shell commands have their own modal.
//!
//! Both `caudra.task.list()` and the `TaskStatusChanged` autocmd serialize the
//! types below, so the two can never spell a status differently.

use caudra_agent::background::{BackgroundTasks, SessionWork, ShellSnapshot};
use caudra_agent::types::BACKGROUND_EVENT_RUN_ID;
use caudra_agent::{AgentEvent, Envelope, SubagentInfo, TaskCard, TaskProvenance};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use caudra_config::ExecutionMode;
use caudra_providers::project_messages;
use caudra_storage::background::{JobKind, JobOwner};
use caudra_storage::id::CaudraId;
use caudra_workflow::{AgentRosterEntry, RosterState, RunSnapshot, RunStatus};
use serde::Serialize;

use crate::app::App;
use crate::app::background_delivery::{DeliveryJob, DeliveryKey};

use crate::components::shell_modal;
use crate::components::task_picker::TaskPickerAction;
use crate::components::{Action, DisplayRole};
use crate::repaint::Dirty;

pub(crate) const MAIN_TASK_ID: &str = "main";
const UNKNOWN_TASK_ERR: &str = "unknown task: ";
const TASK_NOUN: &str = "task";
const TASKS_NOUN: &str = "tasks";
const TASK_USAGE: &str = "Usage: /tasks [list | status <id> | cancel <id>]";
const AUTO_TASK_USAGE: &str = "Usage: /tasks [list | status <id> | background <id> | cancel <id>]";
const TASK_UNAVAILABLE: &str = "Background tasks are unavailable in this session";
const PROMOTION_UNAVAILABLE: &str = "Only agent tasks in auto execution mode can be promoted";

/// How a chat ended, from the vaguest to the most specific. `SubagentHistory`
/// only sees the transcript close, and the `ToolDone` carrying `is_error`
/// lands after it, so [`Self::Unknown`] holds the place until then.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TaskOutcome {
    /// Ended, and nobody said how. Reads as done unless a verdict follows.
    Unknown,
    Done,
    Killed,
    Error,
}

impl TaskOutcome {
    /// Only the placeholder gives way, so a late event cannot walk a finished
    /// task back to another ending.
    pub(crate) fn refines(self, previous: Self) -> bool {
        previous == Self::Unknown && self != Self::Unknown
    }

    /// The bubble that ends the transcript. Derived from the outcome rather
    /// than passed beside it, so a green marker cannot sit on a failed task.
    pub(crate) fn role(self) -> DisplayRole {
        match self {
            Self::Killed | Self::Error => DisplayRole::Error,
            Self::Unknown | Self::Done => DisplayRole::Done,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TaskStatus {
    Working,
    Done,
    Error,
}

impl From<Option<TaskOutcome>> for TaskStatus {
    fn from(outcome: Option<TaskOutcome>) -> Self {
        match outcome {
            None => Self::Working,
            Some(TaskOutcome::Killed | TaskOutcome::Error) => Self::Error,
            Some(TaskOutcome::Unknown | TaskOutcome::Done) => Self::Done,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct TaskState<'a> {
    pub(crate) id: &'a Arc<str>,
    pub(crate) name: &'a str,
    pub(crate) status: TaskStatus,
}

/// The wire shape of `caudra.task.list()`, and what the `/tasks` picker reads.
/// The main chat leaves `status` unset.
#[derive(Serialize)]
pub(crate) struct TaskInfo {
    pub(crate) id: Arc<str>,
    pub(crate) name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) status: Option<TaskStatus>,
    pub(crate) focused: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) runtime: Option<TaskCard>,
    /// Set only on the picker's rows for workflow agents without a chat.
    #[serde(skip)]
    pub(crate) workflow: Option<WorkflowTask>,
}

/// A workflow agent the picker lists without a chat: the run that opens in
/// the workflow inspector, and the roster state shown in place of a card's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkflowTask {
    pub(crate) run_id: String,
    pub(crate) state: &'static str,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct TaskActivity {
    pub(super) agents: usize,
    pub(super) shells: usize,
}

impl TaskActivity {
    /// Agents are counted once each, trusting the runtime over a workflow
    /// roster over a chat, the same order the `/tasks` rows use. Shells are
    /// counted as the Shell modal lists them.
    fn new<'a>(
        runtime: &[TaskCard],
        shells: Option<&ShellSnapshot>,
        workflows: &[RunSnapshot],
        chats: impl Iterator<Item = TaskState<'a>>,
    ) -> Self {
        let mut tasks: HashMap<_, _> = runtime
            .iter()
            .filter(|task| task.kind == JobKind::Agent)
            .map(|task| (task.task_id.as_str(), task.active()))
            .collect();
        for (run, agent, id) in roster_agents(workflows) {
            tasks
                .entry(id)
                .or_insert(roster_status(run, agent) == TaskStatus::Working);
        }
        for chat in chats {
            tasks
                .entry(chat.id.as_ref())
                .or_insert(chat.status == TaskStatus::Working);
        }
        Self {
            agents: tasks
                .into_iter()
                .filter(|&(id, active)| active && id != MAIN_TASK_ID)
                .count(),
            shells: shell_modal::active_count(shells, runtime),
        }
    }
}

/// Every workflow agent with a task id, newest run first.
fn roster_agents(
    workflows: &[RunSnapshot],
) -> impl Iterator<Item = (&RunSnapshot, &AgentRosterEntry, &str)> {
    workflows.iter().flat_map(|run| {
        run.roster
            .iter()
            .filter_map(move |agent| Some((run, agent, agent.task_id.as_deref()?)))
    })
}

/// Only an active run keeps its agents working: a paused or stopped one has
/// nothing running under it, whatever its roster last said.
fn roster_status(run: &RunSnapshot, agent: &AgentRosterEntry) -> TaskStatus {
    match agent.state {
        RosterState::Pending | RosterState::Running if run.status == RunStatus::Active => {
            TaskStatus::Working
        }
        RosterState::Completed => TaskStatus::Done,
        _ => TaskStatus::Error,
    }
}

/// The roster state, or the run's when the run stopped under a live agent.
fn roster_state(run: &RunSnapshot, agent: &AgentRosterEntry) -> &'static str {
    match agent.state {
        RosterState::Pending | RosterState::Running if run.status != RunStatus::Active => {
            run.status.as_str()
        }
        state => state.as_str(),
    }
}

impl App {
    #[cfg(test)]
    pub(super) async fn flush_task_controls(&mut self) {
        for job in std::mem::take(&mut self.task_controls.jobs) {
            job.await;
        }
        let _ = self.poll_task_controls();
    }

    /// What `modal` has selected. A closed modal selects nothing, so a reply
    /// to a control issued from it cannot land once it is gone.
    fn control_selection(&self, modal: ControlModal) -> Option<String> {
        match modal {
            ControlModal::Tasks => self.task_picker.selected_id(),
            ControlModal::Shells => self.shell_modal.selected_id(),
        }
    }

    pub(super) fn start_task_control(
        &mut self,
        task: TaskCard,
        promote: bool,
        modal: ControlModal,
    ) {
        let Some(runtime) = self.background.clone() else {
            self.flash(TASK_UNAVAILABLE.into());
            return;
        };
        if promote
            && (task.kind != JobKind::Agent || runtime.task_execution() != ExecutionMode::Auto)
        {
            self.flash(PROMOTION_UNAVAILABLE.into());
            return;
        }
        let session = runtime.session_id();
        let generation = task.generation;
        let epoch = self.background_delivery.fence.epoch();
        let fence = Arc::clone(&self.background_delivery.fence);
        let focus = self.chats[self.active_chat]
            .task_id()
            .map_or(MAIN_TASK_ID, |id| id.as_ref())
            .to_owned();
        let selection = self.control_selection(modal);
        let sender = self.task_controls.sender.clone();
        self.task_controls.jobs.push(smol::spawn(async move {
            if fence.epoch() != epoch {
                return;
            }
            let result = if promote {
                runtime
                    .promote_invocation(&task.task_id, &task.invocation_id, generation)
                    .await
            } else {
                runtime
                    .cancel_invocation(&task.task_id, &task.invocation_id, generation)
                    .await
            };
            let _ = sender.send(TaskControlReply {
                session,
                generation,
                epoch,
                focus,
                modal,
                selection,
                task,
                result,
            });
        }));
    }

    pub(super) fn poll_task_controls(&mut self) -> Dirty {
        self.task_controls.jobs.retain(|job| !job.is_finished());
        let mut dirty = Dirty::NO;
        while let Ok(reply) = self.task_controls.replies.try_recv() {
            if self.state.session.id != reply.session
                || self.background_delivery.fence.epoch() != reply.epoch
                || self.chats[self.active_chat]
                    .task_id()
                    .map_or(MAIN_TASK_ID, |id| id.as_ref())
                    != reply.focus
                || self.control_selection(reply.modal) != reply.selection
                || self.background.as_ref().is_none_or(|runtime| {
                    runtime.generation() != reply.generation
                        || !runtime
                            .resident_status(&reply.task.task_id)
                            .is_some_and(|task| task.invocation_id == reply.task.invocation_id)
                })
            {
                continue;
            }
            if let Err(error) = reply.result {
                self.flash(error);
                dirty = Dirty::YES;
            }
            dirty |= self.refresh_task_picker();
        }
        dirty
    }

    pub(super) fn task_response_current(&self, origin: &Arc<TaskProvenance>) -> bool {
        task_response_current(self.background.as_ref(), origin)
    }
    pub(crate) fn has_session_work(&self) -> bool {
        self.session_work().pending()
    }

    pub(crate) fn session_work(&self) -> SessionWork {
        let workflow = self.workflow.runtime_handle();
        let foreground_tasks = self
            .chats
            .iter()
            .filter(|chat| chat.task_id().is_some() && chat.task_status() == TaskStatus::Working)
            .count();
        let mut work = SessionWork::capture(
            self.background.as_ref(),
            workflow.as_ref(),
            foreground_tasks,
        );
        work.running |= self
            .workflow
            .runs()
            .iter()
            .any(|run| run.status == RunStatus::Active);
        work.settling |= self.workflow.has_delivery()
            || !self.background_claims.is_empty()
            || self.background_delivery.pending();
        work
    }

    pub(crate) fn waiting_for_background(&self) -> bool {
        if self.status != crate::components::Status::Idle || self.automatic_wakes_suppressed {
            return false;
        }
        let work = self.session_work();
        work.pending() && !work.unavailable
    }

    pub(crate) fn rearm_background(&mut self) {
        let ready = self.background_delivery.rearm();
        self.automatic_wakes_suppressed = false;
        if ready && let Some(background) = &self.background {
            background.rearm();
        }
    }

    pub(crate) fn suppress_background_wakes(&mut self) {
        self.background_delivery.invalidate();
        self.automatic_wakes_suppressed = true;
        if let Some(background) = &self.background {
            background.suppress_wakes();
        }
        self.workflow.suppress_completions();
    }

    pub(crate) fn release_background_claims(&mut self) {
        self.background_delivery.invalidate();
        if let Some(background) = &self.background {
            background.release_messages(&self.background_claims);
        }
        self.background_claims.clear();
        self.workflow.release_completions();
    }

    pub(crate) fn persist_background_delivery(&mut self, final_save: bool) -> Result<(), String> {
        self.poll_background_delivery()?;
        let workflow = self.workflow.delivery_snapshot();
        if self
            .background
            .as_ref()
            .is_none_or(|background| background.list().is_empty())
            && !self.workflow.has_claims()
            && workflow.pending.is_empty()
            && self.background_claims.is_empty()
        {
            return Ok(());
        }
        self.checkpoint_now();
        let revision = self.state.session.content_revision();
        let key = DeliveryKey {
            session: self.state.session.id,
            run: self.run_id,
            revision,
            epoch: self.background_delivery.fence.epoch(),
            generation: self.background.as_ref().map(BackgroundTasks::generation),
            final_save,
            claims: workflow.claims.clone(),
            pending: workflow
                .pending
                .iter()
                .map(|(origin, _)| origin.clone())
                .collect(),
        };
        if !self.background_delivery.needs(&key) {
            return Ok(());
        }
        let history = crate::active_session_history(&self.state.session)
            .map_err(|error| error.to_string())?;
        let messages = project_messages(&history).map_err(|error| error.to_string())?;
        if self.background_saved_revision != Some(revision) {
            if let Err(error) = self
                .storage_writer
                .save_sync(Arc::clone(&self.state.session))
            {
                if final_save {
                    self.release_background_claims();
                }
                return Err(error.to_string());
            }
            self.background_saved_revision = Some(revision);
        }
        self.background_delivery.schedule(DeliveryJob {
            key,
            messages,
            background: self.background.clone(),
            workflow,
        });
        Ok(())
    }

    pub(super) fn execute_task_control(&mut self, args: &str) -> Vec<Action> {
        let mut words = args.split_whitespace();
        let operation = words.next().unwrap_or("list");
        let target = words.next();
        match (operation, target, words.next()) {
            ("list", None, None) => return self.tasks_browse(),
            ("status", Some(id), None) => {
                if !self.show_shell(id) {
                    self.tasks_browse();
                    if !self.task_picker.select(id) {
                        if self.background.is_some() {
                            self.load_task_status(id);
                        } else {
                            self.flash(format!("{UNKNOWN_TASK_ERR}{id}"));
                        }
                    }
                    let _ = self.refresh_task_picker();
                }
            }
            ("background" | "cancel", Some(id), None) => {
                match self
                    .background
                    .as_ref()
                    .ok_or_else(|| TASK_UNAVAILABLE.to_owned())
                    .and_then(|runtime| {
                        runtime
                            .resident_status(id)
                            .ok_or_else(|| format!("{UNKNOWN_TASK_ERR}{id}"))
                    }) {
                    Ok(task) => self.start_task_control(
                        task,
                        operation == "background",
                        ControlModal::Tasks,
                    ),
                    Err(error) => self.flash(error),
                }
            }
            _ => self.flash(
                if self
                    .background
                    .as_ref()
                    .is_some_and(|runtime| runtime.task_execution() == ExecutionMode::Auto)
                {
                    AUTO_TASK_USAGE.into()
                } else {
                    TASK_USAGE.into()
                },
            ),
        }
        Vec::new()
    }

    pub(super) fn session_owns_task(&self, task_id: &str) -> bool {
        self.background
            .as_ref()
            .is_some_and(|background| background.resident_status(task_id).is_some())
    }

    pub(super) fn retain_session_task_routes(&mut self) {
        let owned = self
            .background
            .as_ref()
            .map(|background| {
                background
                    .list()
                    .into_iter()
                    .map(|task| task.task_id)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let retired: Vec<_> = self
            .pending_subagent_steers
            .keys()
            .filter(|id| !owned.contains(id))
            .cloned()
            .collect();
        for task_id in retired {
            self.preserve_unconsumed_steers(&task_id);
        }
        self.chat_index.retain(|id, _| owned.contains(id));
        self.subagent_answers.retain(|id, _| owned.contains(id));
        self.subagent_steers.retain(|id, _| owned.contains(id));
    }

    /// Subagent chats only, in chat order.
    pub(crate) fn task_states(&self) -> impl Iterator<Item = TaskState<'_>> {
        let runtime = self
            .background
            .as_ref()
            .map(BackgroundTasks::list)
            .unwrap_or_default();
        self.chats.iter().filter_map(move |chat| {
            Some(TaskState {
                id: chat.task_id()?,
                name: &chat.name,
                status: runtime
                    .iter()
                    .find(|task| chat.task_id().is_some_and(|id| id.as_ref() == task.task_id))
                    .map_or_else(|| chat.task_status(), runtime_status),
            })
        })
    }

    pub(super) fn task_activity(&self) -> TaskActivity {
        let runtime = self
            .background
            .as_ref()
            .map(BackgroundTasks::list)
            .unwrap_or_default();
        TaskActivity::new(
            &runtime,
            self.shell_snapshot.get(),
            self.workflow.runs(),
            self.chats.iter().filter_map(|chat| {
                Some(TaskState {
                    id: chat.task_id()?,
                    name: &chat.name,
                    status: chat.task_status(),
                })
            }),
        )
    }

    /// The runtime's cards, the picker's selected one in full detail.
    fn runtime_tasks(&self) -> Vec<TaskCard> {
        self.task_history_cards()
    }

    /// Every chat, the main one first, with its runtime card if it has one.
    fn chat_tasks(&self, runtime: &[TaskCard]) -> Vec<TaskInfo> {
        self.chats
            .iter()
            .enumerate()
            .map(|(idx, chat)| {
                let task_id = chat.task_id();
                let card =
                    task_id.and_then(|id| runtime.iter().find(|task| task.task_id == id.as_ref()));
                TaskInfo {
                    id: task_id.map_or_else(|| Arc::from(MAIN_TASK_ID), Arc::clone),
                    name: chat.name.clone(),
                    status: task_id
                        .map(|_| card.map_or_else(|| chat.task_status(), runtime_status)),
                    focused: idx == self.active_chat,
                    runtime: card.cloned(),
                    workflow: None,
                }
            })
            .collect()
    }

    /// What `caudra.task.list()` reports: every chat, then every background
    /// shell job.
    pub(crate) fn tasks(&self) -> Vec<TaskInfo> {
        let runtime = self.runtime_tasks();
        let mut tasks = self.chat_tasks(&runtime);
        tasks.extend(
            runtime
                .into_iter()
                .filter(|task| task.kind == JobKind::Shell)
                .map(|task| TaskInfo {
                    id: Arc::from(task.task_id.as_str()),
                    name: task.label.clone(),
                    status: Some(runtime_status(&task)),
                    focused: false,
                    runtime: Some(task),
                    workflow: None,
                }),
        );
        tasks
    }

    /// The `/tasks` rows: every chat, then every workflow agent no chat here
    /// stands for. A chat without a runtime card takes its status from the
    /// roster first, as [`TaskActivity`] counts it.
    pub(crate) fn picker_tasks(&self) -> Vec<TaskInfo> {
        let runtime = self.runtime_tasks();
        let mut tasks = self.chat_tasks(&runtime);
        let mut seen = HashSet::new();
        for card in runtime.iter().filter(|card| card.kind == JobKind::Agent) {
            if !tasks.iter().any(|task| task.id.as_ref() == card.task_id) {
                tasks.push(TaskInfo {
                    id: Arc::from(card.task_id.as_str()),
                    name: card.label.clone(),
                    status: Some(runtime_status(card)),
                    focused: false,
                    runtime: Some(card.clone()),
                    workflow: None,
                });
            }
        }
        for (run, agent, id) in roster_agents(self.workflow.runs()) {
            if !seen.insert(id) {
                continue;
            }
            let status = roster_status(run, agent);
            match tasks.iter_mut().find(|task| &*task.id == id) {
                Some(task) if task.runtime.is_none() => task.status = Some(status),
                Some(_) => {}
                None => tasks.push(TaskInfo {
                    id: Arc::from(id),
                    name: agent.label.clone(),
                    status: Some(status),
                    focused: false,
                    runtime: None,
                    workflow: Some(WorkflowTask {
                        run_id: run.run_id.clone(),
                        state: roster_state(run, agent),
                    }),
                }),
            }
        }
        tasks
    }

    /// The only writer of `active_chat` outside the chat cycling keys. Tasks
    /// are looked up by id, never by position and never through `chat_index`,
    /// a routing cache wiped at the end of every turn. A shell command opens
    /// in the Shell modal instead.
    pub(crate) fn focus_task(&mut self, id: &str) -> Result<(), String> {
        if self.show_shell(id) {
            return Ok(());
        }
        self.leave_active_chat();
        self.task_queue_viewport = 0;
        self.active_chat = if id == MAIN_TASK_ID {
            0
        } else {
            self.chats
                .iter()
                .position(|chat| chat.task_id().is_some_and(|task_id| &**task_id == id))
                .ok_or_else(|| format!("{UNKNOWN_TASK_ERR}{id}"))?
        };
        Ok(())
    }

    /// Bookkeeping every chat switch shares: an edit in flight, a queue
    /// selection and a hover all belong to the chat being left, and the
    /// navigation keys go back to the composer with it.
    pub(super) fn leave_active_chat(&mut self) {
        self.cancel_queue_edit();
        self.unfocus_active_queue();
        self.chats[self.active_chat].clear_hover();
        self.key_focus = super::KeyFocus::Composer;
    }
}

#[derive(Default)]
pub(super) struct TaskInteractions {
    pub(super) question: Option<Arc<TaskProvenance>>,
    pub(super) auth: HashMap<String, Arc<TaskProvenance>>,
    pub(super) permissions: HashMap<String, Arc<TaskProvenance>>,
}

/// The modal a control was issued from. Its reply is fenced on that modal's
/// own selection, so a stop from one never lands on the other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ControlModal {
    Tasks,
    Shells,
}

struct TaskControlReply {
    session: CaudraId,
    generation: u64,
    epoch: u64,
    focus: String,
    modal: ControlModal,
    selection: Option<String>,
    task: TaskCard,
    result: Result<TaskCard, String>,
}

pub(super) struct TaskControls {
    jobs: Vec<smol::Task<()>>,
    sender: flume::Sender<TaskControlReply>,
    replies: flume::Receiver<TaskControlReply>,
}

impl Default for TaskControls {
    fn default() -> Self {
        let (sender, replies) = flume::unbounded();
        Self {
            jobs: Vec::new(),
            sender,
            replies,
        }
    }
}

pub(crate) fn runtime_status(task: &TaskCard) -> TaskStatus {
    if task.active() {
        TaskStatus::Working
    } else if task.state == "succeeded" {
        TaskStatus::Done
    } else {
        TaskStatus::Error
    }
}

pub(super) fn task_response_current(
    background: Option<&BackgroundTasks>,
    origin: &Arc<TaskProvenance>,
) -> bool {
    background.is_some_and(|background| {
        background.event_is_current(&Envelope {
            event: AgentEvent::AuthRequired,
            subagent: None,
            run_id: BACKGROUND_EVENT_RUN_ID,
            workflow: None,
            task: Some(Arc::clone(origin)),
        })
    })
}

/// The `/tasks` picker's side of the conversation. The picker holds no task
/// state: it is opened from [`App::picker_tasks`] and refreshed from it
/// whenever a status changes, so it can never disagree with the chats it lists.
impl App {
    pub(super) fn tasks_browse(&mut self) -> Vec<Action> {
        self.shell_modal.close();
        self.load_task_history(false);
        self.task_picker.set_promotion_enabled(
            self.background
                .as_ref()
                .is_some_and(|runtime| runtime.task_execution() == ExecutionMode::Auto),
        );
        let _ = self.reconcile_tasks();
        if self.task_picker.is_open() {
            let _ = self.refresh_task_picker();
        } else {
            self.task_picker.open(self.picker_tasks());
        }
        Vec::new()
    }

    /// Keeps an open picker in step with the chats behind it.
    pub(crate) fn refresh_task_picker(&mut self) -> Dirty {
        if !self.task_picker.is_open() {
            return Dirty::NO;
        }
        self.task_picker.set_promotion_enabled(
            self.background
                .as_ref()
                .is_some_and(|runtime| runtime.task_execution() == ExecutionMode::Auto),
        );
        let tasks = self.picker_tasks();
        Dirty::from(self.task_picker.refresh(tasks))
    }

    pub(crate) fn reconcile_tasks(&mut self) -> Dirty {
        let Some(runtime) = self.background.clone() else {
            return Dirty::NO;
        };
        let mut changed = false;
        for task in runtime.list() {
            if task.kind == JobKind::Shell {
                if let Some(index) = self.job_owner_chat(&task) {
                    changed |= self.chats[index].task_card_update(task);
                }
                continue;
            }
            let index = self
                .chats
                .iter()
                .position(|chat| chat.task_id().is_some_and(|id| id.as_ref() == task.task_id));
            if index.is_none() && !task.active() {
                changed |= self.chats[0].task_card_update(task);
                continue;
            }
            let index = index.unwrap_or_else(|| {
                self.resolve_or_create_chat(&SubagentInfo {
                    parent_tool_use_id: task.call_id.clone(),
                    task_id: task.task_id.clone(),
                    name: task.label.clone(),
                    prompt: None,
                    model: None,
                    thinking: None,
                    fast: false,
                    answer_tx: None,
                    steer_tx: None,
                })
            });
            if !task.active() {
                let outcome = match task.state.as_str() {
                    "succeeded" => TaskOutcome::Done,
                    "cancelled" => TaskOutcome::Killed,
                    _ => TaskOutcome::Error,
                };
                let chat = &mut self.chats[index];
                if chat.task_outcome() != Some(outcome) {
                    chat.resume();
                    chat.mark_finished(outcome, &task.state);
                    changed = true;
                }
            }
            changed |= self.chats[0].task_card_update(task);
        }
        for chat in &mut self.chats {
            changed |= chat.reconcile_task_cards(&runtime);
        }
        if changed {
            self.sync_subagents();
        }
        Dirty::from(changed)
    }

    pub(super) fn job_owner_chat(&self, task: &TaskCard) -> Option<usize> {
        match &task.owner {
            JobOwner::Main => Some(0),
            JobOwner::Child { invocation_id } => {
                let owner = self.background.as_ref()?.list().into_iter().find(|owner| {
                    owner.invocation_id == *invocation_id && owner.kind == JobKind::Agent
                })?;
                self.chats.iter().position(|chat| {
                    chat.task_id()
                        .is_some_and(|id| id.as_ref() == owner.task_id)
                })
            }
        }
    }

    pub(super) fn handle_task_picker_action(&mut self, action: TaskPickerAction) -> Vec<Action> {
        match action {
            TaskPickerAction::History { older } => self.load_task_history(older),
            TaskPickerAction::Consumed => {}
            TaskPickerAction::Opened(id) => self.preview_task(&id),
            TaskPickerAction::Control { task, promote } => {
                self.start_task_control(*task, promote, ControlModal::Tasks);
            }
            // Previewing is a real focus, so the transcript behind the float is
            // the one the app already draws.
            TaskPickerAction::Preview(id) => self.preview_task(&id),
            TaskPickerAction::Closed(origin) => {
                if let Some(id) = origin {
                    self.preview_task(&id);
                }
            }
            TaskPickerAction::Inspect { run_id, origin } => {
                if let Some(id) = origin {
                    self.preview_task(&id);
                }
                self.open_workflow_inspector(Some(&run_id));
            }
        }
        Vec::new()
    }

    /// Opening a subagent puts its transcript where the main chat was, so the
    /// composer's top row advertises the picker as the way back out to every
    /// other task. Shell commands are not tasks and stay out of the count.
    pub(crate) fn task_hint_text(&self) -> Option<String> {
        let mut roster_only: HashSet<_> = roster_agents(self.workflow.runs())
            .map(|(_, _, id)| id)
            .collect();
        for chat in &self.chats {
            if let Some(id) = chat.task_id() {
                roster_only.remove(id.as_ref());
            }
        }
        let count = self.chats.len().saturating_sub(1) + roster_only.len();
        if count == 0 {
            return None;
        }
        let noun = if count == 1 { TASK_NOUN } else { TASKS_NOUN };
        Some(format!("{count} {noun}"))
    }

    pub(super) fn preview_task(&mut self, id: &str) {
        self.task_history.cancel_transcript();
        if !self
            .chats
            .iter()
            .any(|chat| chat.task_id().is_some_and(|task| task.as_ref() == id))
            && self.load_archived_chat(id)
        {
            return;
        }
        if let Err(error) = self.focus_task(id) {
            self.flash(error);
        }
    }
}

/// Fires when a known task changes status, or when a new one shows up working.
/// A task first seen already finished is recorded quietly, so a restored
/// session does not replay yesterday's tasks.
///
/// `previous` is keyed by id, because a session reset reuses positions.
pub(crate) fn diff_task_states<'a>(
    previous: &mut Vec<(Arc<str>, TaskStatus)>,
    current: impl Iterator<Item = TaskState<'a>>,
    mut emit: impl FnMut(TaskState<'a>),
) {
    let mut next = Vec::with_capacity(previous.len());
    for task in current {
        let announce = match previous.iter().find(|(id, _)| id == task.id) {
            Some((_, status)) => *status != task.status,
            None => task.status == TaskStatus::Working,
        };
        if announce {
            emit(task);
        }
        next.push((Arc::clone(task.id), task.status));
    }
    *previous = next;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{
        RESEARCH_NAME, app_with_subagent_id, cancel_app, close_subagent_transcript, end_turn,
        error_app, finish_subagent, start_subagent,
    };
    use crate::chat::{CANCELLED_TEXT, DONE_TEXT, ERROR_TEXT};
    use caudra_storage::sessions::StoredSubagentOutcome;
    use test_case::test_case;

    const TASK_ID: &str = "toolu_01";
    const OTHER_ID: &str = "toolu_02";
    const ERROR_ID: &str = "toolu_03";
    const MISSING_ID: &str = "toolu_nope";
    const BUILD_NAME: &str = "build";
    const UNCHANGED_CHAT: usize = 2;

    fn activity_card(id: &str, status: &str, kind: JobKind) -> TaskCard {
        serde_json::from_value(serde_json::json!({
            "task_id": id, "invocation_id": id, "call_id": id, "root_call_id": id,
            "label": BUILD_NAME, "state": status, "kind": kind, "background": true,
            "mode": "build", "generation": 1, "created_at": 0, "updated_at": 0
        }))
        .unwrap()
    }

    #[test_case("queued", true; "queued")]
    #[test_case("running", true; "running")]
    #[test_case("cancelling", true; "cancelling")]
    #[test_case("succeeded", false; "succeeded")]
    #[test_case("failed", false; "failed")]
    #[test_case("cancelled", false; "cancelled")]
    fn activity_counts_runtime_states_without_transcripts(status: &str, active: bool) {
        let cards = [
            activity_card(TASK_ID, status, JobKind::Agent),
            activity_card(OTHER_ID, status, JobKind::Shell),
        ];
        assert_eq!(
            TaskActivity::new(&cards, None, &[], std::iter::empty()),
            TaskActivity {
                agents: usize::from(active),
                shells: usize::from(active),
            }
        );
    }

    #[test_case("running", TaskStatus::Working, 1; "deduplicate")]
    #[test_case("cancelled", TaskStatus::Working, 0; "runtime_overrides_stale_chat")]
    #[test_case("running", TaskStatus::Done, 1; "resumed_runtime_overrides_finished_chat")]
    fn activity_counts_prefer_runtime(status: &str, chat_status: TaskStatus, agents: usize) {
        let id = Arc::from(TASK_ID);
        let cards = [activity_card(TASK_ID, status, JobKind::Agent)];
        let chats = [state(&id, chat_status), state(&id, chat_status)];
        assert_eq!(
            TaskActivity::new(&cards, None, &[], chats.into_iter()),
            TaskActivity { agents, shells: 0 }
        );
    }

    #[test]
    fn activity_counts_merge_foreground_chats_and_mixed_shell_owners() {
        let main = Arc::from(MAIN_TASK_ID);
        let task = Arc::from(TASK_ID);
        let finished = Arc::from(ERROR_ID);
        let mut child_shell = activity_card(OTHER_ID, "cancelling", JobKind::Shell);
        child_shell.owner = JobOwner::Child {
            invocation_id: TASK_ID.into(),
        };
        let cards = [
            child_shell,
            activity_card(MISSING_ID, "queued", JobKind::Shell),
        ];
        let chats = [
            state(&main, TaskStatus::Working),
            state(&task, TaskStatus::Working),
            state(&finished, TaskStatus::Error),
        ];
        assert_eq!(
            TaskActivity::new(&cards, None, &[], chats.into_iter()),
            TaskActivity {
                agents: 1,
                shells: 2
            }
        );
        assert_eq!(
            TaskActivity::new(&[], None, &[], std::iter::empty()),
            TaskActivity::default()
        );
    }

    #[test_case(RosterState::Pending, true; "queued_agent")]
    #[test_case(RosterState::Running, true; "running_agent")]
    #[test_case(RosterState::Completed, false; "completed_agent")]
    #[test_case(RosterState::Failed, false; "failed_agent")]
    #[test_case(RosterState::Cancelled, false; "cancelled_agent")]
    fn activity_counts_workflow_agents_not_runs(status: RosterState, active: bool) {
        let mut run: RunSnapshot = serde_json::from_value(serde_json::json!({
            "run_id": OTHER_ID, "display_name": BUILD_NAME, "workflow_name": BUILD_NAME,
            "source_kind": "builtin", "status": "active", "revision": 1,
            "execution_epoch": 1, "agent_budget": 1, "created_at": 0, "updated_at": 0,
            "roster": [{
                "call_key": 1, "label": BUILD_NAME, "task_id": TASK_ID,
                "state": status, "tokens_used": 0, "duration_ms": 0
            }]
        }))
        .unwrap();
        let id = Arc::from(TASK_ID);
        let count = |run: &RunSnapshot, runtime: &[TaskCard]| {
            TaskActivity::new(
                runtime,
                None,
                std::slice::from_ref(run),
                [state(&id, TaskStatus::Working)].into_iter(),
            )
        };
        assert_eq!(count(&run, &[]).agents, usize::from(active));
        let terminal = [activity_card(TASK_ID, "cancelled", JobKind::Agent)];
        assert_eq!(count(&run, &terminal), TaskActivity::default());
        for status in [
            RunStatus::Paused,
            RunStatus::BudgetLimited,
            RunStatus::Interrupted,
            RunStatus::Completed,
            RunStatus::Cancelled,
            RunStatus::Failed,
        ] {
            run.status = status;
            assert_eq!(count(&run, &[]), TaskActivity::default());
        }
        run.status = RunStatus::Active;
        run.roster.clear();
        assert_eq!(
            TaskActivity::new(&[], None, &[run], std::iter::empty()),
            TaskActivity::default()
        );
    }

    fn app_with_two_subagents() -> App {
        let mut app = app_with_subagent_id(TASK_ID);
        start_subagent(&mut app, OTHER_ID, BUILD_NAME);
        app
    }

    /// Restores the two subagents in the reverse of the order their ids
    /// suggest, so a lookup guessing a position from an id lands on the wrong
    /// transcript instead of being right by accident.
    fn restored_app_with_two_subagents() -> App {
        let mut app = app_with_subagent_id(OTHER_ID);
        start_subagent(&mut app, TASK_ID, BUILD_NAME);
        // Only a subagent that handed over its transcript survives a reload,
        // so both have to close before the session is written.
        close_subagent_transcript(&mut app, OTHER_ID);
        close_subagent_transcript(&mut app, TASK_ID);
        app.reset_ui_chrome();
        app.restore_display();
        app
    }

    fn state(id: &Arc<str>, status: TaskStatus) -> TaskState<'_> {
        TaskState {
            id,
            name: "task",
            status,
        }
    }

    fn collect(
        previous: &mut Vec<(Arc<str>, TaskStatus)>,
        current: &[(Arc<str>, TaskStatus)],
    ) -> Vec<(String, TaskStatus)> {
        let mut fired = Vec::new();
        diff_task_states(
            previous,
            current.iter().map(|(id, status)| state(id, *status)),
            |task| fired.push((task.id.to_string(), task.status)),
        );
        fired
    }

    /// Identity lives on the chat, not in `chat_index`: that one is wiped at
    /// the end of every turn, while the picker keeps addressing tasks long
    /// after.
    #[test_case(MAIN_TASK_ID, Some(0) ; "main")]
    #[test_case(TASK_ID, Some(1)      ; "subagent")]
    #[test_case(MISSING_ID, None      ; "unknown_id")]
    fn focus_addresses_a_task_by_id_alone(id: &str, expected: Option<usize>) {
        let mut app = app_with_two_subagents();
        app.focus_task(OTHER_ID).unwrap();
        assert_eq!(app.active_chat, UNCHANGED_CHAT);
        app.chat_index.clear();

        let err = app.focus_task(id).err();

        assert_eq!(
            err,
            expected
                .is_none()
                .then(|| format!("{UNKNOWN_TASK_ERR}{id}"))
        );
        assert_eq!(app.active_chat, expected.unwrap_or(UNCHANGED_CHAT));
    }

    /// A reload rebuilds the chats from scratch, so the id is all the picker
    /// has left to aim with.
    #[test_case(OTHER_ID, RESEARCH_NAME, 1 ; "first_restored")]
    #[test_case(TASK_ID, BUILD_NAME, 2     ; "second_restored")]
    fn focus_addresses_a_restored_task_by_id(id: &str, name: &str, expected: usize) {
        let mut app = restored_app_with_two_subagents();
        app.focus_task(id).unwrap();

        assert_eq!(app.active_chat, expected);
        let chat = &app.chats[app.active_chat];
        assert_eq!(chat.task_id().map(|task_id| &**task_id), Some(id));
        assert_eq!(chat.name, name);
    }

    /// A subagent reaches disk when it spawns but its transcript only when it
    /// ends, so quitting mid-turn strands one with no messages. Restoring it
    /// would pin a task no agent backs at the top of the picker, running
    /// forever. The reload drops it instead.
    #[test]
    fn a_subagent_stranded_without_a_transcript_is_not_restored() {
        let mut app = app_with_subagent_id(TASK_ID);
        start_subagent(&mut app, OTHER_ID, BUILD_NAME);
        close_subagent_transcript(&mut app, OTHER_ID);

        app.reset_ui_chrome();
        app.restore_display();

        let restored: Vec<_> = app
            .task_states()
            .map(|task| (task.id.to_string(), task.status))
            .collect();
        assert_eq!(restored, vec![(OTHER_ID.to_owned(), TaskStatus::Done)]);
        let recorded: Vec<_> = app
            .state
            .session
            .subagents()
            .iter()
            .map(|sa| sa.tool_use_id.clone())
            .collect();
        assert_eq!(recorded, vec![OTHER_ID.to_owned()], "and stays dropped");
    }

    /// The `task` tool closes the subagent session before it reports a failure,
    /// so the `SubagentHistory` that only knows "it ended" always arrives before
    /// the `ToolDone` holding the verdict. Let the first one decide and every
    /// failure reads as done.
    #[test_case(
        |app: &mut App| close_subagent_transcript(app, TASK_ID),
        TaskStatus::Done, DONE_TEXT
        ; "a_close_nothing_follows_reads_as_done"
    )]
    #[test_case(
        |app| {
            close_subagent_transcript(app, TASK_ID);
            finish_subagent(app, TASK_ID, true);
        },
        TaskStatus::Error, ERROR_TEXT
        ; "a_late_verdict_corrects_the_close"
    )]
    #[test_case(
        |app| {
            finish_subagent(app, TASK_ID, true);
            close_subagent_transcript(app, TASK_ID);
        },
        TaskStatus::Error, ERROR_TEXT
        ; "a_close_never_clears_a_verdict"
    )]
    fn the_most_specific_ending_decides(end: fn(&mut App), status: TaskStatus, text: &str) {
        let mut app = app_with_subagent_id(TASK_ID);
        end(&mut app);
        assert_eq!(app.chats[1].task_status(), status);
        assert_eq!(app.chats[1].last_message_text(), text);
    }

    #[test]
    fn completion_kill_and_error_outcomes_restore_exactly() {
        let mut app = app_with_subagent_id(TASK_ID);
        start_subagent(&mut app, OTHER_ID, BUILD_NAME);
        start_subagent(&mut app, ERROR_ID, RESEARCH_NAME);

        close_subagent_transcript(&mut app, TASK_ID);
        finish_subagent(&mut app, TASK_ID, false);
        app.focus_task(OTHER_ID).unwrap();
        let actions = app.handle_subagent_cancel();
        assert!(
            matches!(actions.as_slice(), [Action::CancelSubagent { tool_use_id }] if tool_use_id == OTHER_ID)
        );
        close_subagent_transcript(&mut app, OTHER_ID);
        close_subagent_transcript(&mut app, ERROR_ID);
        finish_subagent(&mut app, ERROR_ID, true);

        let stored = app
            .state
            .session
            .subagents()
            .iter()
            .map(|subagent| (subagent.tool_use_id.as_str(), subagent.outcome))
            .collect::<Vec<_>>();
        assert_eq!(
            stored,
            [
                (TASK_ID, StoredSubagentOutcome::Done),
                (OTHER_ID, StoredSubagentOutcome::Killed),
                (ERROR_ID, StoredSubagentOutcome::Error),
            ]
        );

        app.reset_ui_chrome();
        app.restore_display();

        for (id, outcome, text, status) in [
            (TASK_ID, TaskOutcome::Done, DONE_TEXT, TaskStatus::Done),
            (
                OTHER_ID,
                TaskOutcome::Killed,
                CANCELLED_TEXT,
                TaskStatus::Error,
            ),
            (ERROR_ID, TaskOutcome::Error, ERROR_TEXT, TaskStatus::Error),
        ] {
            let chat = app
                .chats
                .iter()
                .find(|chat| chat.task_id().is_some_and(|task_id| &**task_id == id))
                .unwrap();
            assert_eq!(chat.task_outcome(), Some(outcome));
            assert_eq!(chat.last_message_text(), text);
            assert_eq!(chat.task_status(), status);
        }
    }

    /// No way of ending a turn may leave a task `working`: nothing runs after
    /// to correct it, and the picker would spin on it forever. The finished
    /// task comes along to show the sweep leaves it alone.
    #[test_case(end_turn as fn(&mut App) ; "turn_end")]
    #[test_case(error_app                ; "parent_error")]
    #[test_case(cancel_app               ; "user_cancel")]
    fn a_turn_ending_terminalizes_every_unfinished_task(terminate: fn(&mut App)) {
        let mut app = app_with_two_subagents();
        finish_subagent(&mut app, TASK_ID, false);
        assert_eq!(app.chats[2].task_status(), TaskStatus::Working);

        terminate(&mut app);

        assert_eq!(
            app.task_states().map(|t| t.status).collect::<Vec<_>>(),
            vec![TaskStatus::Done, TaskStatus::Error]
        );
    }

    /// The picker remembers the focused id before it previews anything and
    /// goes back there on cancel. With two entries claiming focus, or none,
    /// the user is stranded on a task they were only peeking at.
    #[test_case(MAIN_TASK_ID, 0 ; "main")]
    #[test_case(TASK_ID, 1      ; "first_task")]
    #[test_case(OTHER_ID, 2     ; "second_task")]
    fn exactly_one_task_reports_focused(id: &str, expected: usize) {
        let mut app = app_with_two_subagents();
        app.focus_task(OTHER_ID).unwrap();
        app.focus_task(id).unwrap();

        let tasks = app.tasks();
        let focused: Vec<_> = tasks
            .iter()
            .enumerate()
            .filter(|(_, task)| task.focused)
            .map(|(idx, task)| (idx, &*task.id))
            .collect();
        assert_eq!(focused, vec![(expected, id)]);
    }

    /// A task announces itself the moment it shows up working and on every
    /// change after, never twice for the same state. A session reset forgets
    /// it, so running the same task again reads as new.
    #[test]
    fn diff_announces_first_sight_and_every_change() {
        let id: Arc<str> = Arc::from(TASK_ID);
        let working = [(Arc::clone(&id), TaskStatus::Working)];
        let mut previous = Vec::new();

        assert_eq!(
            collect(&mut previous, &working),
            vec![(TASK_ID.to_owned(), TaskStatus::Working)]
        );
        assert!(collect(&mut previous, &working).is_empty());
        assert_eq!(
            collect(&mut previous, &[(Arc::clone(&id), TaskStatus::Done)]),
            vec![(TASK_ID.to_owned(), TaskStatus::Done)]
        );

        assert!(collect(&mut previous, &[]).is_empty());
        assert!(previous.is_empty());
        assert_eq!(
            collect(&mut previous, &working),
            vec![(TASK_ID.to_owned(), TaskStatus::Working)]
        );
    }

    /// A task first seen already finished stays quiet, so a reload does not
    /// replay old news. It is still recorded, or its next change would look
    /// like another first sight and stay quiet too.
    #[test]
    fn a_task_first_seen_finished_is_recorded_silently() {
        let one: Arc<str> = Arc::from(TASK_ID);
        let two: Arc<str> = Arc::from(OTHER_ID);
        let mut previous = Vec::new();

        assert!(collect(&mut previous, &[(Arc::clone(&one), TaskStatus::Done)]).is_empty());
        assert_eq!(
            collect(&mut previous, &[(one, TaskStatus::Error)]),
            vec![(TASK_ID.to_owned(), TaskStatus::Error)]
        );

        assert_eq!(
            collect(&mut previous, &[(Arc::clone(&two), TaskStatus::Working)]),
            vec![(OTHER_ID.to_owned(), TaskStatus::Working)]
        );
        assert_eq!(previous, vec![(two, TaskStatus::Working)]);
    }
}
