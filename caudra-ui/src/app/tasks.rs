//! A task is a subagent chat, addressed by its `tool_use_id`. The main chat
//! goes by [`MAIN_TASK_ID`] and carries no status, since its work is the
//! session's own and `caudra.session.live()` already reports that.
//!
//! Both `caudra.task.list()` and the `TaskStatusChanged` autocmd serialize the
//! types below, so the two can never spell a status differently.

use caudra_agent::background::BackgroundTasks;
use caudra_agent::types::BACKGROUND_EVENT_RUN_ID;
use caudra_agent::{AgentEvent, Envelope, SubagentInfo, TaskCard, TaskProvenance};
use std::collections::HashMap;
use std::sync::Arc;

use caudra_providers::project_messages;
use caudra_storage::id::CaudraId;
use serde::Serialize;

use crate::app::App;
use crate::app::background_delivery::{DeliveryJob, DeliveryKey};

use crate::components::task_picker::TaskPickerAction;
use crate::components::{Action, DisplayRole};
use crate::repaint::Dirty;

pub(crate) const MAIN_TASK_ID: &str = "main";
const UNKNOWN_TASK_ERR: &str = "unknown task: ";
const TASK_NOUN: &str = "task";
const TASKS_NOUN: &str = "tasks";
const TASK_USAGE: &str = "Usage: /tasks [list | status <id> | background <id> | cancel <id>]";
const TASK_UNAVAILABLE: &str = "Background tasks are unavailable in this session";

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
}

impl App {
    #[cfg(test)]
    pub(super) async fn flush_task_controls(&mut self) {
        for job in std::mem::take(&mut self.task_controls.jobs) {
            job.await;
        }
        let _ = self.poll_task_controls();
    }

    fn start_task_control(&mut self, task: TaskCard, promote: bool) {
        let Some(runtime) = self.background.clone() else {
            self.flash(TASK_UNAVAILABLE.into());
            return;
        };
        let session = runtime.session_id();
        let generation = task.generation;
        let epoch = self.background_delivery.fence.epoch();
        let fence = Arc::clone(&self.background_delivery.fence);
        let focus = self.chats[self.active_chat]
            .task_id()
            .map_or(MAIN_TASK_ID, |id| id.as_ref())
            .to_owned();
        let selection = self.task_picker.selected_id();
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
                || self.task_picker.selected_id() != reply.selection
                || self.background.as_ref().is_none_or(|runtime| {
                    runtime.generation() != reply.generation
                        || !runtime
                            .status(&reply.task.task_id)
                            .is_ok_and(|task| task.invocation_id == reply.task.invocation_id)
                })
            {
                continue;
            }
            if let Err(error) = reply.result {
                self.flash(error);
            }
            dirty |= self.refresh_task_picker();
        }
        dirty
    }

    pub(super) fn task_response_current(&self, origin: &Arc<TaskProvenance>) -> bool {
        task_response_current(self.background.as_ref(), origin)
    }
    pub(crate) fn has_session_work(&self) -> bool {
        self.background
            .as_ref()
            .is_some_and(|background| background.active_count() > 0 || background.has_pending())
            || self
                .workflow
                .runs()
                .iter()
                .any(|run| run.status == caudra_workflow::RunStatus::Active)
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
                self.tasks_browse();
                if !self.task_picker.select(id) {
                    self.flash(format!("{UNKNOWN_TASK_ERR}{id}"));
                }
                let _ = self.refresh_task_picker();
            }
            ("background" | "cancel", Some(id), None) => {
                match self
                    .background
                    .as_ref()
                    .ok_or_else(|| TASK_UNAVAILABLE.to_owned())
                    .and_then(|runtime| runtime.status(id))
                {
                    Ok(task) => self.start_task_control(task, operation == "background"),
                    Err(error) => self.flash(error),
                }
            }
            _ => self.flash(TASK_USAGE.into()),
        }
        Vec::new()
    }

    pub(super) fn session_owns_task(&self, task_id: &str) -> bool {
        self.background
            .as_ref()
            .is_some_and(|background| background.status(task_id).is_ok())
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

    /// The same walk widened to every chat, so the main chat at index 0 lands
    /// first on its own and no extra code path has to place it.
    pub(crate) fn tasks(&self) -> Vec<TaskInfo> {
        let mut runtime = self
            .background
            .as_ref()
            .map(BackgroundTasks::list)
            .unwrap_or_default();
        if let Some(id) = self.task_picker.selected_id()
            && let Some(background) = &self.background
            && let Ok(detail) = background.status(&id)
            && let Some(task) = runtime.iter_mut().find(|task| task.task_id == id)
        {
            *task = detail;
        }
        self.chats
            .iter()
            .enumerate()
            .map(|(idx, chat)| {
                let task_id = chat.task_id();
                TaskInfo {
                    id: task_id.map_or_else(|| Arc::from(MAIN_TASK_ID), Arc::clone),
                    name: chat.name.clone(),
                    status: task_id.map(|id| {
                        runtime
                            .iter()
                            .find(|task| task.task_id == id.as_ref())
                            .map_or_else(|| chat.task_status(), runtime_status)
                    }),
                    focused: idx == self.active_chat,
                    runtime: task_id.and_then(|id| {
                        runtime
                            .iter()
                            .find(|task| task.task_id == id.as_ref())
                            .cloned()
                    }),
                }
            })
            .collect()
    }

    /// The only writer of `active_chat` outside the chat cycling keys. Tasks
    /// are looked up by id, never by position and never through `chat_index`,
    /// a routing cache wiped at the end of every turn.
    pub(crate) fn focus_task(&mut self, id: &str) -> Result<(), String> {
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

struct TaskControlReply {
    session: CaudraId,
    generation: u64,
    epoch: u64,
    focus: String,
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
/// state: it is opened from [`App::tasks`] and refreshed from it whenever a
/// status changes, so it can never disagree with the chats it lists.
impl App {
    pub(super) fn tasks_browse(&mut self) -> Vec<Action> {
        let _ = self.reconcile_tasks();
        if self.task_picker.is_open() {
            let _ = self.refresh_task_picker();
        } else {
            self.task_picker.open(self.tasks());
        }
        Vec::new()
    }

    /// Keeps an open picker in step with the chats behind it.
    pub(crate) fn refresh_task_picker(&mut self) -> Dirty {
        if !self.task_picker.is_open() {
            return Dirty::NO;
        }
        let tasks = self.tasks();
        Dirty::from(self.task_picker.refresh(tasks))
    }

    pub(crate) fn reconcile_tasks(&mut self) -> Dirty {
        let Some(runtime) = self.background.clone() else {
            return Dirty::NO;
        };
        let mut changed = false;
        for task in runtime.list() {
            let index = self
                .chats
                .iter()
                .position(|chat| chat.task_id().is_some_and(|id| id.as_ref() == task.task_id));
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
        changed |= self.chats[0].reconcile_task_cards(&runtime);
        if changed {
            self.sync_subagents();
        }
        Dirty::from(changed)
    }

    pub(super) fn handle_task_picker_action(&mut self, action: TaskPickerAction) -> Vec<Action> {
        match action {
            TaskPickerAction::Consumed => {}
            TaskPickerAction::Opened(id) => self.preview_task(&id),
            TaskPickerAction::Control { task, promote } => self.start_task_control(*task, promote),
            // Previewing is a real focus, so the transcript behind the float is
            // the one the app already draws.
            TaskPickerAction::Preview(id) => self.preview_task(&id),
            TaskPickerAction::Closed(origin) => {
                if let Some(id) = origin {
                    self.preview_task(&id);
                }
            }
        }
        Vec::new()
    }

    /// Opening a subagent puts its transcript where the main chat was, so the
    /// composer's top row advertises the picker as the way back out to every
    /// other task.
    pub(crate) fn task_hint_text(&self) -> Option<String> {
        let count = self.task_states().count();
        if count == 0 {
            return None;
        }
        let noun = if count == 1 { TASK_NOUN } else { TASKS_NOUN };
        Some(format!("{count} {noun}"))
    }

    pub(super) fn preview_task(&mut self, id: &str) {
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
