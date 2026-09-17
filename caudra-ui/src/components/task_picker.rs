//! The `/tasks` picker: the subagents of the focused session, running first.
//!
//! There is no task state here. The app owns the chats, so every open and
//! every refresh rebuilds the rows from [`crate::app::App::tasks`], and
//! previewing is a real focus with a restore on cancel.

use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};

use crate::app::tasks::{TaskInfo, TaskStatus};
use crate::components::keybindings::key;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::components::{Hint, Overlay};
use crate::repaint::Cadence;

const TITLE: &str = " Tasks ";
const MAX_VISIBLE: u16 = 15;
const EMPTY_TEXT: &str = "No tasks yet";
const RUNNING_SECTION: &str = "Running";
const FINISHED_SECTION: &str = "Finished";
const DONE_SUFFIX: &str = "done";
const ERROR_SUFFIX: &str = "error";

/// `Preview` is the reason this must not be dropped: the picker moved the
/// selection but only the app can focus the task behind it, so a discarded
/// action leaves the transcript showing something else.
#[must_use]
pub enum TaskPickerAction {
    Consumed,
    /// The selection moved. The app focuses this task so the transcript behind
    /// the float is the one being previewed.
    Preview(String),
    /// Committed: keep the previewed task and close.
    Opened,
    /// Cancelled. The app restores whichever task was focused on open.
    Closed(Option<String>),
}

pub struct TaskItem {
    id: String,
    name: String,
    suffix: Option<&'static str>,
    section: Option<&'static str>,
    running: bool,
}

impl PickerItem for TaskItem {
    fn label(&self) -> &str {
        &self.name
    }

    fn suffix(&self) -> Option<&str> {
        self.suffix
    }

    fn section(&self) -> Option<&str> {
        self.section
    }

    fn is_spinning(&self) -> bool {
        self.running
    }
}

pub struct TaskPicker {
    picker: ListPicker<TaskItem>,
    /// What was focused when the picker opened, restored unless the user
    /// commits, so a cancelled preview never sticks.
    origin: Option<String>,
    /// Suppresses the preview that would otherwise fire for the selection the
    /// picker starts on, which is already focused.
    previewed: Option<String>,
}

impl TaskPicker {
    pub fn new() -> Self {
        let mut picker = ListPicker::new()
            .with_max_visible(MAX_VISIBLE)
            .with_footer_builder(footer);
        picker.set_empty_text(EMPTY_TEXT);
        Self {
            picker,
            origin: None,
            previewed: None,
        }
    }

    pub fn open(&mut self, tasks: Vec<TaskInfo>) {
        let focused = tasks
            .iter()
            .find(|task| task.focused)
            .map(|task| task.id.to_string());
        let items = build_items(tasks);
        self.picker.open(items, TITLE);
        if let Some(id) = &focused {
            self.picker.select_item_by(|item| &item.id == id);
        }
        self.previewed = focused.clone();
        self.origin = focused;
    }

    /// Rebuilds the rows in place when a task changes status. The selection is
    /// restored by id, so a task moving from Running to Finished does not drag
    /// the cursor with it.
    pub fn refresh(&mut self, tasks: Vec<TaskInfo>) {
        if !self.picker.is_open() {
            return;
        }
        let selected = self.selected_id();
        self.picker.replace_items(build_items(tasks));
        if let Some(id) = selected {
            self.picker.select_item_by(|item| item.id == id);
        }
    }

    pub fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub fn close(&mut self) {
        self.picker.close();
        self.origin = None;
        self.previewed = None;
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.picker.contains(pos)
    }

    /// Leaving without opening a transcript, which returns to whichever one the
    /// picker was opened from.
    pub fn cancel(&mut self) -> TaskPickerAction {
        self.map_action(PickerAction::Close)
    }

    pub fn scroll(&mut self, delta: i32) -> TaskPickerAction {
        self.picker.scroll(delta);
        self.preview()
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> TaskPickerAction {
        let action = self.picker.handle_key(key);
        self.map_action(action)
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> TaskPickerAction {
        let action = self.picker.handle_mouse(event);
        self.map_action(action)
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        self.picker.handle_paste(text)
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        self.picker.view(frame, area)
    }

    fn selected_id(&self) -> Option<String> {
        self.picker.selected_item().map(|item| item.id.clone())
    }

    /// Emitted whenever the cursor lands somewhere new, which is what makes
    /// arrowing through the list preview each transcript.
    fn preview(&mut self) -> TaskPickerAction {
        match self.selected_id() {
            Some(id) if self.previewed.as_deref() != Some(id.as_str()) => {
                self.previewed = Some(id.clone());
                TaskPickerAction::Preview(id)
            }
            _ => TaskPickerAction::Consumed,
        }
    }

    fn map_action(&mut self, action: PickerAction<TaskItem>) -> TaskPickerAction {
        match action {
            PickerAction::Consumed | PickerAction::Toggle(..) => self.preview(),
            PickerAction::Select(_) => {
                self.close();
                TaskPickerAction::Opened
            }
            PickerAction::Close => {
                let origin = self.origin.take();
                self.close();
                TaskPickerAction::Closed(origin)
            }
            PickerAction::Key(key) => self.handle_key(key),
        }
    }
}

impl Overlay for TaskPicker {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close();
    }

    fn cadence(&self) -> Cadence {
        self.picker.cadence()
    }
}

fn footer() -> Vec<Hint> {
    vec![
        Hint::bind(key::ENTER, "open"),
        Hint::bind(key::ESC, "cancel"),
    ]
}

/// The main chat comes first and has no status. The subagents follow, running
/// ones above finished ones so a long job never gets buried under the ones
/// that already returned. Within a section, chat order.
fn build_items(tasks: Vec<TaskInfo>) -> Vec<TaskItem> {
    let (mut main, mut running, mut finished) = (Vec::new(), Vec::new(), Vec::new());
    for task in tasks {
        let id = task.id.to_string();
        let (suffix, bucket) = match task.status {
            None => (None, &mut main),
            Some(TaskStatus::Working) => (None, &mut running),
            Some(TaskStatus::Done) => (Some(DONE_SUFFIX), &mut finished),
            Some(TaskStatus::Error) => (Some(ERROR_SUFFIX), &mut finished),
        };
        bucket.push(TaskItem {
            id,
            name: task.name,
            suffix,
            section: None,
            running: task.status == Some(TaskStatus::Working),
        });
    }
    for (heading, bucket) in [
        (RUNNING_SECTION, &mut running),
        (FINISHED_SECTION, &mut finished),
    ] {
        if let Some(first) = bucket.first_mut() {
            first.section = Some(heading);
        }
    }
    main.into_iter().chain(running).chain(finished).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::key as key_event;
    use crossterm::event::KeyCode;
    use std::sync::Arc;

    const MAIN_ID: &str = "main";
    const RUNNING_ID: &str = "toolu_running";
    const DONE_ID: &str = "toolu_done";

    fn task(id: &str, name: &str, status: Option<TaskStatus>, focused: bool) -> TaskInfo {
        TaskInfo {
            id: Arc::from(id),
            name: name.into(),
            status,
            focused,
        }
    }

    fn mixed() -> Vec<TaskInfo> {
        vec![
            task(MAIN_ID, "chat", None, true),
            task(DONE_ID, "finished work", Some(TaskStatus::Done), false),
            task(RUNNING_ID, "live work", Some(TaskStatus::Working), false),
        ]
    }

    fn opened() -> TaskPicker {
        let mut picker = TaskPicker::new();
        picker.open(mixed());
        picker
    }

    #[test]
    fn running_tasks_sort_above_finished_ones_under_the_main_chat() {
        let items = build_items(mixed());
        let ids: Vec<&str> = items.iter().map(|item| item.id.as_str()).collect();
        assert_eq!(ids, [MAIN_ID, RUNNING_ID, DONE_ID]);
        assert_eq!(items[0].section(), None, "the main chat opens no section");
        assert_eq!(items[1].section(), Some(RUNNING_SECTION));
        assert_eq!(items[2].section(), Some(FINISHED_SECTION));
    }

    #[test]
    fn only_a_running_task_spins() {
        let items = build_items(mixed());
        let spinning: Vec<bool> = items.iter().map(PickerItem::is_spinning).collect();
        assert_eq!(spinning, [false, true, false]);
    }

    #[test]
    fn opening_selects_the_focused_task_without_previewing_it() {
        let mut picker = TaskPicker::new();
        picker.open(vec![
            task(MAIN_ID, "chat", None, false),
            task(RUNNING_ID, "live work", Some(TaskStatus::Working), true),
        ]);
        assert_eq!(picker.selected_id().as_deref(), Some(RUNNING_ID));
        assert!(
            matches!(picker.preview(), TaskPickerAction::Consumed),
            "the task already on screen is not re-focused"
        );
    }

    #[test]
    fn moving_the_cursor_previews_the_task_it_lands_on() {
        let mut picker = opened();
        let action = picker.handle_key(key_event(KeyCode::Down));
        assert!(
            matches!(action, TaskPickerAction::Preview(id) if id == RUNNING_ID),
            "moving down previews the next task"
        );
    }

    #[test]
    fn cancelling_hands_back_the_task_that_was_focused_on_open() {
        let mut picker = opened();
        let _ = picker.handle_key(key_event(KeyCode::Down));
        let action = picker.handle_key(key_event(KeyCode::Esc));
        assert!(
            matches!(action, TaskPickerAction::Closed(Some(id)) if id == MAIN_ID),
            "cancelling restores the origin"
        );
        assert!(!picker.is_open());
    }

    #[test]
    fn committing_keeps_the_previewed_task() {
        let mut picker = opened();
        let _ = picker.handle_key(key_event(KeyCode::Down));
        let action = picker.handle_key(key_event(KeyCode::Enter));
        assert!(matches!(action, TaskPickerAction::Opened));
        assert!(!picker.is_open());
    }

    #[test]
    fn a_status_change_keeps_the_cursor_on_its_task() {
        let mut picker = opened();
        let _ = picker.handle_key(key_event(KeyCode::Down));
        assert_eq!(picker.selected_id().as_deref(), Some(RUNNING_ID));
        picker.refresh(vec![
            task(MAIN_ID, "chat", None, false),
            task(DONE_ID, "finished work", Some(TaskStatus::Done), false),
            task(RUNNING_ID, "live work", Some(TaskStatus::Done), true),
        ]);
        assert_eq!(
            picker.selected_id().as_deref(),
            Some(RUNNING_ID),
            "the task kept the cursor as it moved into Finished"
        );
    }
}
