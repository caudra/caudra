//! The `/tasks` picker: the agents of the focused session, running first.
//! Shell commands have a modal of their own.
//!
//! There is no task state here. The app owns the chats, so every open and
//! every refresh rebuilds the rows from [`crate::app::App::picker_tasks`], and
//! previewing is a real focus with a restore on cancel.

use caudra_agent::TaskCard;
use caudra_grab::grab_scope;
use caudra_storage::background::JobKind;
use caudra_storage::now_epoch;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::Line;

use crate::app::tasks::{TaskInfo, TaskStatus};
use crate::components::code_view::{WrappedRows, truncation_line};
use crate::components::keybindings::{Bind, key};
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::components::modal::Modal;
use crate::components::{Hint, Overlay, escape_terminal_controls, task_card};
use crate::repaint::Cadence;
use crate::theme;

const TITLE: &str = " Tasks ";
const MAX_VISIBLE: u16 = 15;
const EMPTY_TEXT: &str = "No tasks yet";
const RUNNING_SECTION: &str = "Running";
const FINISHED_SECTION: &str = "Finished";
const DONE_SUFFIX: &str = "done";
const ERROR_SUFFIX: &str = "error";
const DETAIL_ROWS: u16 = 6;
const WIDTH_PERCENT: u16 = 85;
const LIST_ROOM: u16 = 10;
const PROMOTE: Bind = Bind {
    code: KeyCode::Char('b'),
    modifiers: KeyModifiers::CONTROL,
    label: "Ctrl+B",
};
const CANCEL: Bind = Bind {
    code: KeyCode::Char('k'),
    modifiers: KeyModifiers::CONTROL,
    label: "Ctrl+K",
};

/// `Preview` is the reason this must not be dropped: the picker moved the
/// selection but only the app can focus the task behind it, so a discarded
/// action leaves the transcript showing something else.
#[must_use]
pub enum TaskPickerAction {
    Consumed,
    History {
        older: bool,
    },
    /// The selection moved. The app focuses this task so the transcript behind
    /// the float is the one being previewed.
    Preview(String),
    /// Committed: keep the previewed task and close.
    Opened(String),
    /// Committed to a workflow agent with no chat here. The app puts the
    /// origin back and opens the run in the workflow inspector.
    Inspect {
        run_id: String,
        origin: Option<String>,
    },
    Control {
        task: Box<TaskCard>,
        promote: bool,
    },
    /// Cancelled. The app restores whichever task was focused on open.
    Closed(Option<String>),
    Copy(String),
}

#[derive(PartialEq)]
pub struct TaskItem {
    id: String,
    name: String,
    suffix: Option<&'static str>,
    section: Option<&'static str>,
    running: bool,
    search: String,
    runtime: Option<TaskCard>,
    /// The run of a workflow agent listed without a chat.
    workflow: Option<String>,
}

impl PickerItem for TaskItem {
    fn label(&self) -> &str {
        &self.name
    }

    fn search_text(&self) -> &str {
        &self.search
    }

    fn detail(&self) -> Option<&str> {
        self.runtime
            .as_ref()
            .map(|task| task.state.as_str())
            .or(self.suffix)
    }

    fn badge(&self) -> Option<&str> {
        self.runtime
            .as_ref()
            .is_some_and(|task| task.background)
            .then_some("bg")
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
    details: Option<TaskCard>,
    promotion_enabled: bool,
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
            .with_width_percent(WIDTH_PERCENT)
            .with_max_visible(MAX_VISIBLE)
            .with_footer_builder(footer);
        picker.set_empty_text(EMPTY_TEXT);
        Self {
            picker,
            details: None,
            promotion_enabled: true,
            origin: None,
            previewed: None,
        }
    }

    pub fn open(&mut self, tasks: Vec<TaskInfo>) {
        self.details = tasks
            .iter()
            .find(|task| task.focused)
            .or(tasks.first())
            .and_then(|task| task.runtime.clone());
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

    pub fn set_promotion_enabled(&mut self, enabled: bool) {
        self.promotion_enabled = enabled;
    }

    fn can_promote(&self, task: &TaskCard) -> bool {
        self.promotion_enabled && task.kind == JobKind::Agent && !task.background
    }

    /// Rebuilds the rows in place when a task changes status. The selection is
    /// restored by id, so a task moving from Running to Finished does not drag
    /// the cursor with it.
    pub fn refresh(&mut self, tasks: Vec<TaskInfo>) -> bool {
        if !self.picker.is_open() {
            return false;
        }
        let selected = self.selected_id();
        let details = tasks
            .iter()
            .find(|task| Some(task.id.as_ref()) == selected.as_deref())
            .and_then(|task| task.runtime.clone());
        let detail_changed = self.details != details;
        self.details = details;
        let items = build_items(tasks);
        if items
            .iter()
            .enumerate()
            .all(|(index, item)| self.picker.item(index) == Some(item))
            && self.picker.item(items.len()).is_none()
        {
            return detail_changed;
        }
        self.picker.replace_items(items);
        if let Some(id) = selected {
            self.picker.select_item_by(|item| item.id == id);
        }
        true
    }

    pub fn select(&mut self, id: &str) -> bool {
        self.picker.clear_search();
        self.picker.select_item_by(|item| item.id == id)
    }

    pub fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub fn close(&mut self) {
        self.picker.close();
        self.details = None;
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
        if key.modifiers == KeyModifiers::ALT && matches!(key.code, KeyCode::Left | KeyCode::Right)
        {
            return TaskPickerAction::History {
                older: key.code == KeyCode::Right,
            };
        }
        if PROMOTE.matches(key) || CANCEL.matches(key) {
            let promote = PROMOTE.matches(key);
            return self
                .picker
                .selected_item()
                .and_then(|item| {
                    let task = item.runtime.as_ref()?;
                    (task.active()
                        && task.state != "cancelling"
                        && (!promote || self.can_promote(task)))
                    .then(|| TaskPickerAction::Control {
                        task: Box::new(task.clone()),
                        promote,
                    })
                })
                .unwrap_or(TaskPickerAction::Consumed);
        }
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
        grab_scope!("task_picker", area);
        let info = self.picker.selected_item().map(|item| {
            let runtime = self
                .details
                .as_ref()
                .filter(|task| task.task_id == item.id)
                .or(item.runtime.as_ref());
            let text = runtime.map_or_else(
                || format!("{}\n{}", item.id, item.name),
                |task| {
                    let end = if task.active() {
                        now_epoch()
                    } else {
                        task.updated_at
                    };
                    format!(
                        "{} · {} · {}\n{}\n{} · {}s elapsed",
                        task.task_id,
                        task.state,
                        task.mode,
                        task.label,
                        if task.background {
                            "background"
                        } else {
                            "foreground"
                        },
                        end.saturating_sub(task.created_at),
                    )
                },
            );
            let rows = DETAIL_ROWS.min(area.height.saturating_sub(LIST_ROOM) / 2) as usize;
            let width = Modal::inner_width(area.width, WIDTH_PERCENT);
            let facts = text
                .lines()
                .map(|line| {
                    Line::styled(escape_terminal_controls(line), theme::current().item_desc)
                })
                .collect::<Vec<_>>();
            let mut lines = WrappedRows::new(facts, 0, width).lines();
            if let Some(task) = runtime {
                lines.extend(task_card::details(task, width).0);
            }
            if rows > 0 && lines.len() > rows {
                let hidden = lines.len() - rows + 1;
                lines.truncate(rows - 1);
                lines.push(truncation_line(hidden));
            }
            lines.resize(rows, Line::default());
            lines
        });
        self.picker
            .set_info_lines(info.filter(|lines| !lines.is_empty()));
        let task = self
            .picker
            .selected_item()
            .and_then(|item| item.runtime.as_ref());
        self.picker.set_footer_builder(match task {
            Some(task) if task.active() && task.state != "cancelling" && self.can_promote(task) => {
                foreground_footer
            }
            Some(task) if task.active() && task.state != "cancelling" => background_footer,
            _ => footer,
        });
        self.picker.view(frame, area)
    }

    pub(crate) fn selected_id(&self) -> Option<String> {
        self.picker.selected_item().map(|item| item.id.clone())
    }

    /// Emitted whenever the cursor lands somewhere new, which is what makes
    /// arrowing through the list preview each transcript. A workflow row has
    /// no transcript here, so the one behind it stays.
    fn preview(&mut self) -> TaskPickerAction {
        match self.picker.selected_item() {
            Some(item)
                if item.workflow.is_none()
                    && self.previewed.as_deref() != Some(item.id.as_str()) =>
            {
                let id = item.id.clone();
                self.previewed = Some(id.clone());
                TaskPickerAction::Preview(id)
            }
            _ => TaskPickerAction::Consumed,
        }
    }

    fn map_action(&mut self, action: PickerAction<TaskItem>) -> TaskPickerAction {
        match action {
            PickerAction::Consumed | PickerAction::Toggle(..) => self.preview(),
            PickerAction::Select(item) => {
                let origin = self.origin.take();
                self.close();
                match item.workflow {
                    Some(run_id) => TaskPickerAction::Inspect { run_id, origin },
                    None => TaskPickerAction::Opened(item.id),
                }
            }
            PickerAction::Close => {
                let origin = self.origin.take();
                self.close();
                TaskPickerAction::Closed(origin)
            }
            PickerAction::Key(key) => self.handle_key(key),
            PickerAction::Copy(text) => TaskPickerAction::Copy(text),
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
        Hint::inert("Alt+←/→", "recent/older"),
    ]
}

fn foreground_footer() -> Vec<Hint> {
    let mut hints = background_footer();
    hints.push(Hint::bind(PROMOTE, "background"));
    hints
}

fn background_footer() -> Vec<Hint> {
    let mut hints = footer();
    hints.push(Hint::bind(CANCEL, "stop task"));
    hints
}

/// The main chat comes first and has no status. The subagents follow, running
/// ones above finished ones so a long job never gets buried under the ones
/// that already returned. Within a section, chat order. A workflow agent
/// without a chat shows its roster state.
fn build_items(tasks: Vec<TaskInfo>) -> Vec<TaskItem> {
    let (mut main, mut running, mut finished) = (Vec::new(), Vec::new(), Vec::new());
    for mut task in tasks {
        if let Some(runtime) = &mut task.runtime {
            runtime.result = None;
            runtime.result_preview = None;
            runtime.result_truncated = false;
            runtime.reports.clear();
            runtime.reports_truncated = false;
        }
        let id = task.id.to_string();
        let status = task
            .runtime
            .as_ref()
            .map(crate::app::tasks::runtime_status)
            .or(task.status);
        let (suffix, bucket) = match status {
            None => (None, &mut main),
            Some(TaskStatus::Working) => (None, &mut running),
            Some(TaskStatus::Done) => (Some(DONE_SUFFIX), &mut finished),
            Some(TaskStatus::Error) => (Some(ERROR_SUFFIX), &mut finished),
        };
        bucket.push(TaskItem {
            search: format!("{} {}", task.name, task.id),
            id,
            name: task.name,
            suffix: task
                .workflow
                .as_ref()
                .map_or(suffix, |workflow| Some(workflow.state)),
            section: None,
            running: status == Some(TaskStatus::Working),
            runtime: task.runtime,
            workflow: task.workflow.map(|workflow| workflow.run_id),
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
    use crate::animation::test_clock::FrozenSpinner;
    use crate::app::tasks::WorkflowTask;
    use crate::components::key as key_event;
    use caudra_storage::tool_outputs::ToolOutputRef;
    use crossterm::event::{KeyCode, MouseButton, MouseEventKind};
    use ratatui::{Terminal, backend::TestBackend, style::Modifier};
    use std::sync::Arc;
    use test_case::test_case;
    use unicode_width::UnicodeWidthStr;

    const MAIN_ID: &str = "main";
    const RUNNING_ID: &str = "toolu_running";
    const DONE_ID: &str = "toolu_done";
    const LABEL: &str = "等待 sixty seconds 界";
    const INVOCATION: &str = "private-invocation";
    const CLICK_LABEL: &str = "Completed investigation";
    const CLICK_RESULT: &str = "Hydrated task result";
    const OTHER_LABEL: &str = "Other investigation";
    const CLICK_ROW_MISSING: &str = "the completed task has a visible list row";
    const OUTPUT_ID: &str = "calm-blue-wren";
    const RUN_ID: &str = "run-build";

    fn click_tasks(hydrated: bool, background: bool, state: &str) -> Vec<TaskInfo> {
        let mut target = runtime_task(state, background);
        target.name = CLICK_LABEL.into();
        let runtime = target.runtime.as_mut().unwrap();
        runtime.label = CLICK_LABEL.into();
        if hydrated {
            runtime.result = Some(serde_json::json!({ "output": CLICK_RESULT }));
        }
        vec![
            task(MAIN_ID, "Main", None, true),
            target,
            task(DONE_ID, OTHER_LABEL, Some(TaskStatus::Done), false),
        ]
    }

    fn paint_picker(picker: &mut TaskPicker, terminal: &mut Terminal<TestBackend>) -> Rect {
        let mut popup = Rect::default();
        terminal
            .draw(|frame| popup = picker.view(frame, frame.area()))
            .unwrap();
        popup
    }

    fn task_row(terminal: &Terminal<TestBackend>, popup: Rect) -> Position {
        let buffer = terminal.backend().buffer();
        let y = (popup.y..popup.bottom())
            .rfind(|&y| {
                let row: String = (popup.x..popup.right())
                    .map(|x| buffer[(x, y)].symbol())
                    .collect();
                row.contains(CLICK_LABEL)
            })
            .expect(CLICK_ROW_MISSING);
        Position::new(popup.x + 2, y)
    }

    fn mouse_at(kind: MouseEventKind, position: Position) -> MouseEvent {
        MouseEvent {
            kind,
            column: position.x,
            row: position.y,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test_case(false; "foreground_result")]
    #[test_case(true; "background_result")]
    fn pressed_task_opens_after_selected_detail_hydration_and_render(background: bool) {
        let mut picker = TaskPicker::new();
        picker.open(click_tasks(false, background, "succeeded"));
        assert_eq!(picker.selected_id().as_deref(), Some(MAIN_ID));
        let mut terminal = Terminal::new(TestBackend::new(127, 30)).unwrap();
        let popup = paint_picker(&mut picker, &mut terminal);
        let position = task_row(&terminal, popup);
        assert!(
            matches!(picker.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), position)), TaskPickerAction::Preview(id) if id == RUNNING_ID)
        );
        assert!(picker.refresh(click_tasks(true, background, "succeeded")));
        assert_eq!(paint_picker(&mut picker, &mut terminal), popup);
        assert_eq!(task_row(&terminal, popup), position);
        let painted: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(painted.contains(CLICK_RESULT));
        assert!(!picker.refresh(click_tasks(true, background, "succeeded")));
        assert!(
            matches!(picker.handle_mouse(mouse_at(MouseEventKind::Up(MouseButton::Left), position)), TaskPickerAction::Opened(id) if id == RUNNING_ID)
        );
        assert!(!picker.is_open());
    }

    #[test_case(""; "empty_success")]
    #[test_case(CLICK_RESULT; "small_success")]
    fn complete_picker_result_hides_retrieval_without_dropping_reference(result: &str) {
        let mut task = runtime_task("succeeded", true);
        let runtime = task.runtime.as_mut().unwrap();
        runtime.result = Some(serde_json::json!({ "output": result, "error": null }));
        runtime.output_ref = Some(ToolOutputRef {
            id: OUTPUT_ID.parse().unwrap(),
            byte_count: result.len(),
            line_count: 1,
        });
        let mut picker = TaskPicker::new();
        picker.open(vec![task]);
        let mut terminal = Terminal::new(TestBackend::new(127, 30)).unwrap();
        paint_picker(&mut picker, &mut terminal);
        let shown: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(!shown.contains(OUTPUT_ID), "{shown}");
        assert!(!shown.contains("tool_output"), "{shown}");
        assert!(!shown.contains("Full outcome"), "{shown}");
        if !result.is_empty() {
            assert!(shown.contains(result), "{shown}");
        }
        assert_eq!(
            picker
                .picker
                .selected_item()
                .unwrap()
                .runtime
                .as_ref()
                .unwrap()
                .output_ref
                .as_ref()
                .unwrap()
                .id
                .to_string(),
            OUTPUT_ID
        );
    }

    #[test_case("reorder"; "reordered_rows")]
    #[test_case("remove"; "removed_row")]
    #[test_case("resize"; "resized_popup")]
    fn changed_task_geometry_invalidates_pending_press(change: &str) {
        let mut picker = TaskPicker::new();
        picker.open(click_tasks(false, true, "succeeded"));
        let mut terminal = Terminal::new(TestBackend::new(127, 30)).unwrap();
        let popup = paint_picker(&mut picker, &mut terminal);
        let position = task_row(&terminal, popup);
        let _ = picker.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), position));
        let mut tasks = click_tasks(true, true, "succeeded");
        match change {
            "reorder" => tasks.swap(1, 2),
            "remove" => {
                tasks.remove(1);
            }
            _ => terminal = Terminal::new(TestBackend::new(117, 30)).unwrap(),
        }
        picker.refresh(tasks);
        paint_picker(&mut picker, &mut terminal);
        assert!(!matches!(
            picker.handle_mouse(mouse_at(MouseEventKind::Up(MouseButton::Left), position)),
            TaskPickerAction::Opened(_)
        ));
        assert!(picker.is_open());
    }

    #[test_case(false; "summary")]
    #[test_case(true; "hydrated_detail")]
    fn promotion_refreshes_badge_once_without_selection_or_preview_loop(hydrated: bool) {
        let mut picker = TaskPicker::new();
        picker.open(click_tasks(hydrated, false, "running"));
        assert!(picker.select(RUNNING_ID));
        let _ = picker.preview();
        assert!(picker.refresh(click_tasks(hydrated, true, "running")));
        assert_eq!(picker.selected_id().as_deref(), Some(RUNNING_ID));
        assert_eq!(picker.picker.selected_item().unwrap().badge(), Some("bg"));
        assert!(!picker.refresh(click_tasks(hydrated, true, "running")));
        assert!(matches!(picker.preview(), TaskPickerAction::Consumed));
        assert!(matches!(picker.cancel(), TaskPickerAction::Closed(Some(id)) if id == MAIN_ID));
    }

    fn runtime_task(state: &str, background: bool) -> TaskInfo {
        let mut item = task(RUNNING_ID, LABEL, Some(TaskStatus::Working), false);
        item.runtime = Some(
            serde_json::from_value(serde_json::json!({
                "task_id": RUNNING_ID, "invocation_id": INVOCATION,
                "call_id": "launch", "root_call_id": "launch", "label": LABEL,
                "state": state, "background": background, "mode": "build",
                "generation": 1, "created_at": 1, "updated_at": 2
            }))
            .unwrap(),
        );
        item
    }

    #[test_case(false; "result")]
    #[test_case(true; "report")]
    fn details_render_markdown_before_row_budget(report: bool) {
        const BODY: &str = "**Finding** with `code`\n\n- checked\n- another\n- final";
        let mut task = runtime_task("succeeded", false);
        let card = task.runtime.as_mut().unwrap();
        if report {
            card.reports = vec![BODY.into()];
        } else {
            card.result = Some(serde_json::json!({"output": BODY}));
        }
        let mut picker = TaskPicker::new();
        picker.open(vec![task]);
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| {
                picker.view(frame, frame.area());
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let painted = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(painted.contains("Finding"));
        assert!(!painted.contains("**Finding**"));
        assert!(!painted.contains("`code`"));
        assert!(
            buffer
                .content
                .iter()
                .any(|cell| cell.symbol() == "F" && cell.modifier.contains(Modifier::BOLD))
        );
        assert!(!painted.contains("final"));
    }

    #[test_case("running", 127; "running_wide")]
    #[test_case("succeeded", 127; "finished_wide")]
    #[test_case("running", 28; "running_narrow_unicode")]
    #[test_case("cancelled", 28; "cancelled_narrow_unicode")]
    fn background_badge_composes_with_right_edge_status(state: &str, width: u16) {
        let _clock = FrozenSpinner::at(0);
        let mut picker = TaskPicker::new();
        picker.open(vec![
            task(MAIN_ID, "Main", None, true),
            runtime_task(state, true),
        ]);
        let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
        let mut popup = Rect::default();
        terminal
            .draw(|frame| popup = picker.view(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let expected = if state == "running" {
            "bg  ⠋"
        } else if state == "succeeded" {
            "bg  succeeded"
        } else {
            "bg  cancelled"
        };
        let mut badge = None;
        for y in popup.y..popup.bottom() {
            let row: String = (popup.x..popup.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect();
            if row.contains(expected) {
                badge = Some((y, row));
            }
        }
        let (y, row) = badge.expect("background row retains badge and execution status");
        assert!(
            row.trim_end_matches('│').trim_end().ends_with(expected),
            "{row}"
        );
        let detail = if state == "running" {
            format!("bg  {}", crate::animation::spinner_str(0))
        } else {
            expected.to_owned()
        };
        assert_eq!(
            buffer[(popup.right() - 2 - detail.width() as u16, y)].symbol(),
            "b",
            "{row}"
        );
        assert!(!row.contains(INVOCATION));
    }

    #[test_case(false; "foreground")]
    #[test_case(true; "main")]
    fn only_background_tasks_have_badges(main: bool) {
        let items = build_items(vec![if main {
            task(MAIN_ID, "Main", None, true)
        } else {
            runtime_task("running", false)
        }]);
        assert_eq!(items[0].badge(), None);
    }

    #[test_case("running", true; "running")]
    #[test_case("failed", false; "failed")]
    #[test_case("blocked", false; "blocked")]
    #[test_case("interrupted", false; "interrupted")]
    fn runtime_state_overrides_unfinished_chat(state: &str, spinning: bool) {
        let items = build_items(vec![runtime_task(state, true)]);
        assert_eq!(items[0].is_spinning(), spinning);
        assert_eq!(items[0].detail(), Some(state));
        assert_eq!(items[0].label(), LABEL);
    }

    #[test_case(KeyCode::Enter; "enter_without_preview")]
    fn preselection_opens_explicit_identity(code: KeyCode) {
        let mut picker = opened();
        assert!(picker.select(DONE_ID));
        assert!(
            matches!(picker.handle_key(key_event(code)), TaskPickerAction::Opened(id) if id == DONE_ID)
        );
    }

    #[test_case('b'; "b_filters")]
    #[test_case('k'; "k_filters")]
    fn plain_letters_remain_filter_input(letter: char) {
        let mut picker = opened();
        let action = picker.handle_key(key_event(KeyCode::Char(letter)));
        assert!(!matches!(action, TaskPickerAction::Control { .. }));
        assert_eq!(picker.picker.search_text(), letter.to_string());
    }

    #[test_case(true; "promote")]
    #[test_case(false; "cancel")]
    fn controls_capture_displayed_invocation(promote: bool) {
        let mut picker = TaskPicker::new();
        picker.open(vec![runtime_task("running", false)]);
        let action = picker.handle_key(if promote { PROMOTE } else { CANCEL }.to_key_event());
        assert!(
            matches!(action, TaskPickerAction::Control { task, promote: actual } if task.invocation_id == INVOCATION && actual == promote)
        );
    }

    #[test_case(true, JobKind::Agent, true; "auto_agent")]
    #[test_case(false, JobKind::Agent, false; "fixed_agent")]
    #[test_case(true, JobKind::Shell, false; "auto_shell")]
    #[test_case(false, JobKind::Shell, false; "fixed_shell")]
    fn promotion_requires_policy_and_agent_kind(enabled: bool, kind: JobKind, allowed: bool) {
        let mut item = runtime_task("running", false);
        item.runtime.as_mut().unwrap().kind = kind;
        let mut picker = TaskPicker::new();
        picker.set_promotion_enabled(enabled);
        picker.open(vec![item]);
        assert_eq!(
            matches!(
                picker.handle_key(PROMOTE.to_key_event()),
                TaskPickerAction::Control { promote: true, .. }
            ),
            allowed
        );
        assert!(matches!(
            picker.handle_key(CANCEL.to_key_event()),
            TaskPickerAction::Control { promote: false, .. }
        ));
    }

    #[test_case(TaskStatus::Working, "running"; "running_agent")]
    #[test_case(TaskStatus::Error, "failed"; "failed_agent")]
    fn workflow_rows_inspect_their_run_without_previewing(status: TaskStatus, state: &'static str) {
        let mut agent = task(RUNNING_ID, LABEL, Some(status), false);
        agent.workflow = Some(WorkflowTask {
            run_id: RUN_ID.into(),
            state,
        });
        let mut picker = TaskPicker::new();
        picker.open(vec![task(MAIN_ID, "Main", None, true), agent]);
        assert!(matches!(
            picker.handle_key(key_event(KeyCode::Down)),
            TaskPickerAction::Consumed
        ));
        let row = picker.picker.selected_item().unwrap();
        assert_eq!(row.detail(), Some(state));
        assert_eq!(row.is_spinning(), status == TaskStatus::Working);
        assert!(matches!(
            picker.handle_key(CANCEL.to_key_event()),
            TaskPickerAction::Consumed
        ));
        assert!(matches!(
            picker.handle_key(key_event(KeyCode::Enter)),
            TaskPickerAction::Inspect { run_id, origin: Some(origin) } if run_id == RUN_ID && origin == MAIN_ID
        ));
        assert!(!picker.is_open());
    }

    #[test_case("succeeded"; "terminal")]
    #[test_case("cancelling"; "cancelling")]
    fn inactive_controls_are_disabled(state: &str) {
        let mut picker = TaskPicker::new();
        picker.open(vec![runtime_task(state, false)]);
        assert!(matches!(
            picker.handle_key(CANCEL.to_key_event()),
            TaskPickerAction::Consumed
        ));
        assert!(matches!(
            picker.handle_key(PROMOTE.to_key_event()),
            TaskPickerAction::Consumed
        ));
    }

    #[test_case("running"; "promotion")]
    #[test_case("failed"; "settlement")]
    fn refresh_preserves_filter_selection_and_escape_origin(state: &str) {
        let mut picker = TaskPicker::new();
        picker.open(vec![
            task(MAIN_ID, "Main", None, true),
            runtime_task("running", false),
        ]);
        picker.picker.set_search_text(RUNNING_ID);
        let _ = picker.preview();
        assert!(picker.refresh(vec![
            task(MAIN_ID, "Main", None, false),
            runtime_task(state, true)
        ]));
        assert_eq!(picker.selected_id().as_deref(), Some(RUNNING_ID));
        assert_eq!(picker.picker.search_text(), RUNNING_ID);
        assert!(matches!(picker.cancel(), TaskPickerAction::Closed(Some(id)) if id == MAIN_ID));
    }

    fn task(id: &str, name: &str, status: Option<TaskStatus>, focused: bool) -> TaskInfo {
        TaskInfo {
            id: Arc::from(id),
            name: name.into(),
            status,
            focused,
            runtime: None,
            workflow: None,
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
        assert!(matches!(action, TaskPickerAction::Opened(id) if id == RUNNING_ID));
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
