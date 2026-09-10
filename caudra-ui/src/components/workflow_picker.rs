//! The `/workflow` runs picker: every run of the session with its agent
//! roster underneath, and the pause, resume and stop controls.
//!
//! The picker holds no run state of its own beyond the snapshot it was last
//! given: the app re-supplies the runtime's read model on every change, and
//! every control names the run its row belongs to.

use caudra_workflow::{RosterState, RunSnapshot, RunStatus};
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::Line;

use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::components::{Overlay, escape_terminal_controls, hint_line};
use crate::repaint::Cadence;

const TITLE: &str = " Workflow runs ";
const MAX_VISIBLE: u16 = 20;
const EMPTY_TEXT: &str = "No workflow runs yet";
const ROSTER_INDENT: &str = "  \u{21b3} ";
const DETAIL_SEPARATOR: &str = " \u{b7} ";
const AGENTS_LABEL: &str = "agents";
const TOKENS_LABEL: &str = "tokens";
pub(crate) const PAUSE_LABEL: &str = "p";
pub(crate) const RESUME_LABEL: &str = "r";
pub(crate) const STOP_LABEL: &str = "x";
const PAUSE_KEY: char = ascii_key(PAUSE_LABEL);
const RESUME_KEY: char = ascii_key(RESUME_LABEL);
const STOP_KEY: char = ascii_key(STOP_LABEL);

/// The keybinding tables quote the label; the picker matches the key. One
/// spelling feeds both.
const fn ascii_key(label: &str) -> char {
    label.as_bytes()[0] as char
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunControl {
    Pause,
    Resume,
    Stop,
}

impl RunControl {
    pub(crate) fn verb(self) -> &'static str {
        match self {
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Stop => "stop",
        }
    }

    /// Whether the runtime would accept this control for a run in `status`.
    pub(crate) fn applies_to(self, status: RunStatus) -> bool {
        match self {
            Self::Pause => status == RunStatus::Active,
            Self::Resume => status.is_resumable() || status == RunStatus::BudgetLimited,
            Self::Stop => matches!(
                status,
                RunStatus::Active | RunStatus::Paused | RunStatus::BudgetLimited
            ),
        }
    }
}

#[must_use]
pub enum WorkflowPickerAction {
    Consumed,
    Control {
        control: RunControl,
        run_id: String,
    },
    /// Enter on an agent row that has a transcript.
    OpenTranscript(String),
    Close,
}

pub struct RunRow {
    run_id: String,
    label: String,
    suffix: &'static str,
    detail_text: String,
    status: RunStatus,
    task_id: Option<String>,
    running: bool,
}

impl PickerItem for RunRow {
    fn label(&self) -> &str {
        &self.label
    }

    fn suffix(&self) -> Option<&str> {
        Some(self.suffix)
    }

    fn detail(&self) -> Option<&str> {
        (!self.detail_text.is_empty()).then_some(&self.detail_text)
    }

    fn is_spinning(&self) -> bool {
        self.running
    }
}

pub struct WorkflowPicker {
    picker: ListPicker<RunRow>,
    runs: Vec<RunSnapshot>,
}

impl WorkflowPicker {
    pub fn new() -> Self {
        let mut picker = ListPicker::new()
            .with_max_visible(MAX_VISIBLE)
            .with_footer_builder(footer);
        picker.set_empty_text(EMPTY_TEXT);
        Self {
            picker,
            runs: Vec::new(),
        }
    }

    pub fn open(&mut self, runs: Vec<RunSnapshot>) {
        self.picker.open(build_rows(&runs), TITLE);
        self.runs = runs;
    }

    /// Rebuilds the rows in place; the selection follows its run rather than
    /// its position, so a finishing run does not drag the cursor.
    pub fn refresh(&mut self, runs: Vec<RunSnapshot>) {
        if !self.picker.is_open() {
            return;
        }
        let selected = self.selected_key();
        self.picker.replace_items(build_rows(&runs));
        self.runs = runs;
        if let Some((run_id, task_id)) = selected {
            self.picker
                .select_item_by(|row| row.run_id == run_id && row.task_id == task_id);
        }
    }

    pub fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub fn close(&mut self) {
        self.picker.close();
        self.runs.clear();
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.picker.contains(pos)
    }

    pub fn scroll(&mut self, delta: i32) {
        self.picker.scroll(delta);
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        self.picker.handle_paste(text)
    }

    /// A control key acts on the selected row's run when the run can take
    /// it; otherwise the key edits the search line like any other.
    pub fn handle_key(&mut self, key: KeyEvent) -> WorkflowPickerAction {
        if key.modifiers.is_empty() {
            if key.code == KeyCode::Enter {
                return self.choose();
            }
            if let Some((control, run_id)) = self.control_for(key.code) {
                return WorkflowPickerAction::Control { control, run_id };
            }
        }
        let action = self.picker.handle_key(key);
        self.map_action(action)
    }

    fn control_for(&self, code: KeyCode) -> Option<(RunControl, String)> {
        let control = match code {
            KeyCode::Char(PAUSE_KEY) => RunControl::Pause,
            KeyCode::Char(RESUME_KEY) => RunControl::Resume,
            KeyCode::Char(STOP_KEY) => RunControl::Stop,
            _ => return None,
        };
        let row = self.picker.selected_item()?;
        control
            .applies_to(row.status)
            .then(|| (control, row.run_id.clone()))
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> WorkflowPickerAction {
        let action = self.picker.handle_mouse(event);
        self.map_action(action)
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        self.picker.view(frame, area)
    }

    #[cfg(test)]
    pub(crate) fn run_count(&self) -> usize {
        self.runs.len()
    }

    fn selected_key(&self) -> Option<(String, Option<String>)> {
        self.picker
            .selected_item()
            .map(|row| (row.run_id.clone(), row.task_id.clone()))
    }

    fn choose(&mut self) -> WorkflowPickerAction {
        match self
            .picker
            .selected_item()
            .and_then(|row| row.task_id.clone())
        {
            Some(task_id) => {
                self.close();
                WorkflowPickerAction::OpenTranscript(task_id)
            }
            None => WorkflowPickerAction::Consumed,
        }
    }

    fn map_action(&mut self, action: PickerAction<RunRow>) -> WorkflowPickerAction {
        match action {
            PickerAction::Consumed | PickerAction::Toggle(..) => WorkflowPickerAction::Consumed,
            // A click hands the row over and empties the list, so put the list
            // back before treating the click as Enter on that row.
            PickerAction::Select(row) => {
                self.picker.open(build_rows(&self.runs), TITLE);
                self.picker.select_item_by(|candidate| {
                    candidate.run_id == row.run_id && candidate.task_id == row.task_id
                });
                self.choose()
            }
            PickerAction::Close => {
                self.close();
                WorkflowPickerAction::Close
            }
        }
    }
}

impl Overlay for WorkflowPicker {
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

fn footer() -> Line<'static> {
    hint_line(&[
        (PAUSE_LABEL, "Pause"),
        (RESUME_LABEL, "Resume"),
        (STOP_LABEL, "Stop"),
        ("Enter", "Open agent"),
        ("Esc", "Close"),
    ])
}

/// One row per run with its roster indented beneath it, in the order the
/// runtime publishes them.
fn build_rows(runs: &[RunSnapshot]) -> Vec<RunRow> {
    let mut rows = Vec::with_capacity(runs.iter().map(|run| 1 + run.roster.len()).sum());
    for run in runs {
        let mut detail = vec![
            format!("{} {AGENTS_LABEL}", run.usage.agents_admitted),
            format!("{} {TOKENS_LABEL}", run.usage.tokens_used),
        ];
        if let Some(phase) = &run.phase {
            detail.insert(0, escape_terminal_controls(phase));
        }
        rows.push(RunRow {
            run_id: run.run_id.clone(),
            label: escape_terminal_controls(&run.display_name),
            suffix: run.status.as_str(),
            detail_text: detail.join(DETAIL_SEPARATOR),
            status: run.status,
            task_id: None,
            running: run.status == RunStatus::Active,
        });
        for agent in &run.roster {
            rows.push(RunRow {
                run_id: run.run_id.clone(),
                label: format!("{ROSTER_INDENT}{}", escape_terminal_controls(&agent.label)),
                suffix: agent.state.as_str(),
                detail_text: agent
                    .phase
                    .as_deref()
                    .map(escape_terminal_controls)
                    .unwrap_or_default(),
                status: run.status,
                task_id: agent.task_id.clone(),
                running: agent.state == RosterState::Running,
            });
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use caudra_workflow::{AgentRosterEntry, RunUsage, SourceKind};
    use test_case::test_case;

    use super::*;
    use crate::components::key as key_event;

    const RUN_ID: &str = "run-1";
    const TASK_ID: &str = "run-1:1";
    const WRONG_RUN: &str = "a control must name the run of the selected row";
    const INERT_KEY: &str = "a control the run cannot take must fall through to the search";
    const TRANSCRIPT: &str = "Enter on an agent row opens its transcript";

    pub(crate) fn run(status: RunStatus, roster: Vec<AgentRosterEntry>) -> RunSnapshot {
        RunSnapshot {
            run_id: RUN_ID.into(),
            display_name: "deep-research".into(),
            workflow_name: "deep-research".into(),
            source_kind: SourceKind::Builtin,
            objective: None,
            status,
            pause_kind: None,
            pause_message: None,
            revision: 1,
            execution_epoch: 1,
            phase: None,
            phases: Vec::new(),
            agent_budget: 8,
            usage: RunUsage::default(),
            roster,
            result: None,
            error: None,
            logs: Vec::new(),
            outbox_pending: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn agent(task_id: Option<&str>) -> AgentRosterEntry {
        AgentRosterEntry {
            call_key: 1,
            label: "researcher".into(),
            phase: None,
            task_id: task_id.map(str::to_owned),
            state: RosterState::Running,
            tokens_used: 0,
            duration_ms: 0,
        }
    }

    #[test_case(RunStatus::Active, PAUSE_KEY, Some(RunControl::Pause) ; "pause_active")]
    #[test_case(RunStatus::Active, STOP_KEY, Some(RunControl::Stop) ; "stop_active")]
    #[test_case(RunStatus::Paused, RESUME_KEY, Some(RunControl::Resume) ; "resume_paused")]
    #[test_case(RunStatus::Completed, PAUSE_KEY, None ; "pause_completed_is_inert")]
    #[test_case(RunStatus::Active, RESUME_KEY, None ; "resume_active_is_inert")]
    fn control_keys_follow_the_run_status(
        status: RunStatus,
        key: char,
        expected: Option<RunControl>,
    ) {
        let mut picker = WorkflowPicker::new();
        picker.open(vec![run(status, Vec::new())]);
        match (picker.handle_key(key_event(KeyCode::Char(key))), expected) {
            (WorkflowPickerAction::Control { control, run_id }, Some(expected)) => {
                assert_eq!(control, expected);
                assert_eq!(run_id, RUN_ID, "{WRONG_RUN}");
            }
            (WorkflowPickerAction::Consumed, None) => {
                assert_eq!(picker.picker.search_text(), key.to_string(), "{INERT_KEY}");
            }
            _ => panic!("{INERT_KEY}"),
        }
    }

    #[test]
    fn enter_on_an_agent_row_opens_its_transcript() {
        let mut picker = WorkflowPicker::new();
        picker.open(vec![run(RunStatus::Active, vec![agent(Some(TASK_ID))])]);
        let _ = picker.handle_key(key_event(KeyCode::Down));
        match picker.handle_key(key_event(KeyCode::Enter)) {
            WorkflowPickerAction::OpenTranscript(task_id) => assert_eq!(task_id, TASK_ID),
            _ => panic!("{TRANSCRIPT}"),
        }
        assert!(!picker.is_open());
    }

    #[test]
    fn enter_on_a_run_row_or_an_agent_without_a_transcript_does_nothing() {
        let mut picker = WorkflowPicker::new();
        picker.open(vec![run(RunStatus::Active, vec![agent(None)])]);
        assert!(matches!(
            picker.handle_key(key_event(KeyCode::Enter)),
            WorkflowPickerAction::Consumed
        ));
        let _ = picker.handle_key(key_event(KeyCode::Down));
        assert!(matches!(
            picker.handle_key(key_event(KeyCode::Enter)),
            WorkflowPickerAction::Consumed
        ));
        assert!(picker.is_open());
    }

    #[test]
    fn a_control_on_an_agent_row_names_its_run() {
        let mut picker = WorkflowPicker::new();
        picker.open(vec![run(RunStatus::Active, vec![agent(Some(TASK_ID))])]);
        let _ = picker.handle_key(key_event(KeyCode::Down));
        match picker.handle_key(key_event(KeyCode::Char(STOP_KEY))) {
            WorkflowPickerAction::Control { control, run_id } => {
                assert_eq!(control, RunControl::Stop);
                assert_eq!(run_id, RUN_ID, "{WRONG_RUN}");
            }
            _ => panic!("{WRONG_RUN}"),
        }
    }
}
