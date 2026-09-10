//! A task is a subagent chat, addressed by its `tool_use_id`. The main chat
//! goes by [`MAIN_TASK_ID`] and carries no status, since its work is the
//! session's own and `caudra.session.live()` already reports that.
//!
//! Both `caudra.task.list()` and the `TaskStatusChanged` autocmd serialize the
//! types below, so the two can never spell a status differently.

use std::sync::Arc;

use serde::Serialize;

use crate::app::App;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::components::hover_style;
use crate::components::input::top_right_hint_area;
use crate::components::keybindings::leader;
use crate::components::task_picker::TaskPickerAction;
use crate::components::{Action, DisplayRole};
use crate::repaint::Dirty;
use crate::theme;

pub(crate) const MAIN_TASK_ID: &str = "main";
const UNKNOWN_TASK_ERR: &str = "unknown task: ";
const TASK_HINT_PAD: &str = " ";
const TASK_HINT_KEY_GAP: &str = " ";

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
}

impl App {
    /// Subagent chats only, in chat order.
    pub(crate) fn task_states(&self) -> impl Iterator<Item = TaskState<'_>> {
        self.chats.iter().filter_map(|chat| {
            Some(TaskState {
                id: chat.task_id()?,
                name: &chat.name,
                status: chat.task_status(),
            })
        })
    }

    /// The same walk widened to every chat, so the main chat at index 0 lands
    /// first on its own and no extra code path has to place it.
    pub(crate) fn tasks(&self) -> Vec<TaskInfo> {
        self.chats
            .iter()
            .enumerate()
            .map(|(idx, chat)| {
                let task_id = chat.task_id();
                TaskInfo {
                    id: task_id.map_or_else(|| Arc::from(MAIN_TASK_ID), Arc::clone),
                    name: chat.name.clone(),
                    status: task_id.map(|_| chat.task_status()),
                    focused: idx == self.active_chat,
                }
            })
            .collect()
    }

    /// The only writer of `active_chat` outside the chat cycling keys. Tasks
    /// are looked up by id, never by position and never through `chat_index`,
    /// a routing cache wiped at the end of every turn.
    pub(crate) fn focus_task(&mut self, id: &str) -> Result<(), String> {
        self.cancel_queue_edit();
        self.unfocus_active_queue();
        self.task_queue_viewport = 0;
        self.chats[self.active_chat].clear_hover();
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
}

/// The `/tasks` picker's side of the conversation. The picker holds no task
/// state: it is opened from [`App::tasks`] and refreshed from it whenever a
/// status changes, so it can never disagree with the chats it lists.
impl App {
    pub(super) fn tasks_browse(&mut self) -> Vec<Action> {
        self.task_picker.open(self.tasks());
        Vec::new()
    }

    /// Keeps an open picker in step with the chats behind it.
    pub(crate) fn refresh_task_picker(&mut self) -> Dirty {
        if !self.task_picker.is_open() {
            return Dirty::NO;
        }
        let tasks = self.tasks();
        self.task_picker.refresh(tasks);
        Dirty::YES
    }

    pub(super) fn handle_task_picker_action(&mut self, action: TaskPickerAction) -> Vec<Action> {
        match action {
            TaskPickerAction::Consumed | TaskPickerAction::Opened => {}
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

    /// Opening a subagent puts its transcript where the main chat was, so this
    /// hint advertises the picker as the way back out to every other task.
    ///
    /// The hint renders right-aligned into [`top_right_hint_area`], so its hit
    /// region is derived the same way rather than read back off the frame. The
    /// padding on either side is separator rather than control, so it neither
    /// takes the click nor lights up under the pointer. A region too narrow to
    /// hold the hint leaves a zero-width rect, which no pointer can be inside.
    pub(crate) fn task_hint(&self, area: Rect) -> Option<(Line<'static>, Rect)> {
        let count = self.task_states().count();
        if count == 0 {
            return None;
        }
        let t = theme::current();
        let noun = if count == 1 { "task" } else { "tasks" };
        let count_text = format!("{count} {noun}");
        let control_width =
            count_text.width() + TASK_HINT_KEY_GAP.width() + leader::TASKS.label.width();
        let line = Line::from(vec![
            Span::raw(TASK_HINT_PAD),
            Span::styled(
                count_text,
                hover_style(Style::new().fg(t.foreground), self.task_hint_hover),
            ),
            Span::styled(
                format!("{TASK_HINT_KEY_GAP}{}", leader::TASKS.label),
                hover_style(t.keybind_key, self.task_hint_hover),
            ),
            Span::raw(TASK_HINT_PAD),
        ]);

        let hint_area = top_right_hint_area(area);
        let full_width = control_width + TASK_HINT_PAD.width() * 2;
        let left = if full_width <= hint_area.width as usize {
            hint_area.right().saturating_sub(full_width as u16)
        } else {
            hint_area.x
        };
        let x = left.saturating_add(TASK_HINT_PAD.width() as u16);
        let hit = Rect::new(
            x,
            hint_area.y,
            (control_width as u16).min(hint_area.right().saturating_sub(x)),
            hint_area.height.min(1),
        );
        Some((line, hit))
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
