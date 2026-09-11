//! The `/workflow` inspector: every run the session knows on the left, and
//! the selected run's overview, phases, agents, calls, logs and result on
//! the right. Runs of earlier sessions are listed to read, not to control.
//!
//! The inspector holds no run state of its own beyond what it was given: the
//! app re-supplies the runtime's read model on every change, asks the
//! runtime for a run's detail when the selection or the run moves, and
//! every control names the run it acts on.

use std::collections::HashMap;
use std::path::PathBuf;

use caudra_agent::SubagentProgress;
use caudra_agent::types::{PhaseMark, WorkflowRunCard};
use caudra_workflow::{
    AgentRosterEntry, CallKind, CallState, MAX_AGENT_BUDGET, RosterState, RunCall, RunCallBody,
    RunDetail, RunEventKind, RunHistoryEntry, RunSnapshot, RunStatus,
};
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::animation::{animation_elapsed_ms, spinner_str};
use crate::components::modal::{FooterHits, FooterLine, Modal};
use crate::components::scrollbar::{Scrollbar, ScrollbarMouse};
use crate::components::workflow_card::{phase_strip_line, status_span};
use crate::components::{
    ModalScroll, Overlay, ToolProgress, escape_terminal_controls, format_compact, format_elapsed,
    format_integer, hover_style, input_line_with_cursor, now_secs, visual_rows,
};
use crate::repaint::Cadence;
use crate::text_buffer::TextBuffer;
use crate::theme;

const TITLE: &str = " Workflows ";
const WIDTH_PERCENT: u16 = 90;
const MAX_HEIGHT_PERCENT: u16 = 85;
const LIST_MAX_WIDTH: u16 = 34;
const LIST_PERCENT: u16 = 35;
const PANE_GAP: u16 = 1;
/// Blank columns a list row keeps between its text and its clock.
const PANE_GAP_COLS: usize = 1;
const H_PAD: u16 = 1;
/// The tab strip above the body and the footer below it.
const CHROME_ROWS: u16 = 2;
const EMPTY_TEXT: &str = "No workflow runs yet";
const EMPTY_HINT: &str = "Start one with /workflow <name>, or /workflows to browse";
const NO_MATCH: &str = "No run matches";
const NO_SELECTION: &str = "Select a run";
const LOADING: &str = "Loading\u{2026}";
const NO_PHASES: &str = "No phases yet";
const NO_AGENTS: &str = "No agents yet";
const NO_AGENTS_IN_PHASE: &str = "This phase dispatched no agents";
const UNPHASED_GROUP: &str = "No phase";
const NO_CALLS: &str = "No calls yet";
const NO_LOGS: &str = "No log lines yet";
const NO_RESULT: &str = "No result yet";
const JOURNAL_TRIMMED: &str = "Journal trimmed: the calls are no longer stored";
pub(crate) const FOREIGN_RUN: &str = "Runs of earlier sessions can only be viewed";
pub(crate) const NO_TRANSCRIPT: &str = "This agent has no transcript";
const PAUSE_INERT: &str = "Only an active run can be paused";
const RESUME_INERT: &str = "Only a paused, failed, or cancelled run can be resumed";
const STOP_INERT: &str = "This run has already finished";
const BUDGET_MAXED: &str = "The maximum agent budget is spent; start a new run";
const BUDGET_INVALID: &str = "Enter an agent budget above the agents already admitted";
const BUDGET_LABEL: &str = "Agent budget: ";
/// What a budget-limited run is offered on top of what it already spent.
const BUDGET_STEP: u32 = 64;
const BUDGET_LIMITED_HINT: &str = "Budget limited: r resumes with a higher agent budget";
const FAILED_HINT: &str = "Failed: r resumes from the journal";
const COPIED: &str = "Copied section";
pub(crate) const PAUSE_LABEL: &str = "p";
pub(crate) const RESUME_LABEL: &str = "r";
pub(crate) const STOP_LABEL: &str = "s";
pub(crate) const COPY_LABEL: &str = "y";
pub(crate) const FILTER_LABEL: &str = "/";
const PAUSE_KEY: char = ascii_key(PAUSE_LABEL);
const RESUME_KEY: char = ascii_key(RESUME_LABEL);
const STOP_KEY: char = ascii_key(STOP_LABEL);
const COPY_KEY: char = ascii_key(COPY_LABEL);
const FILTER_KEY: char = ascii_key(FILTER_LABEL);
const SECTION_GAP: &str = "  ";
const GROUP_RUNNING: &str = "Running";
const GROUP_WAITING: &str = "Waiting";
const GROUP_FINISHED: &str = "Finished";
const GROUP_EARLIER: &str = "Earlier sessions";
const CURSOR_MARK: &str = "\u{203a} ";
const NO_MARK: &str = "  ";
const SEPARATOR: &str = " \u{b7} ";
const ARROW: &str = "\u{2192} ";
const EXPAND_INDENT: &str = "      ";
const STATUS_LABEL: &str = "Status: ";
const PHASE_LABEL: &str = "Phase: ";
const ELAPSED_LABEL: &str = "Elapsed: ";
const AGENTS_LABEL: &str = "Agents: ";
const TOKENS_LABEL: &str = "Tokens: ";
const OBJECTIVE_LABEL: &str = "Objective: ";
const SESSION_LABEL: &str = "Session: ";
const PAUSED_LABEL: &str = "Paused: ";
const ERROR_LABEL: &str = "Error: ";
const SCRATCH_LABEL: &str = "Scratch file: ";
const RESULT_LABEL: &str = "Result: ";
const PROMPT_HEADING: &str = "Prompt";
const RESULT_HEADING: &str = "Result";
const ERROR_HEADING: &str = "Error";
const BODY_MISSING: &str = "This call left nothing in the journal";
const REPORT_FIELD: &str = "report";
const CALL_PREFIX: &str = "#";
const TOKENS_UNIT: &str = " tokens";
const AGENTS_UNIT: &str = " agents";
const DONE_UNIT: &str = " done";
const ADMITTED_UNIT: &str = " admitted";
const AGENT_SLASH: &str = "/";
const FOOTER: [(&str, &str, FooterCommand); 7] = [
    (
        PAUSE_LABEL,
        "Pause",
        FooterCommand::Control(RunControl::Pause),
    ),
    (
        RESUME_LABEL,
        "Resume",
        FooterCommand::Control(RunControl::Resume),
    ),
    (STOP_LABEL, "Stop", FooterCommand::Control(RunControl::Stop)),
    ("Enter", "Open", FooterCommand::Activate),
    (COPY_LABEL, "Copy", FooterCommand::Copy),
    (FILTER_LABEL, "Filter", FooterCommand::Filter),
    ("Esc", "Close", FooterCommand::Close),
];
/// The prompt takes every key, so its footer names no click targets.
const BUDGET_FOOTER: [(&str, &str); 2] = [("Enter", "Resume"), ("Esc", "Cancel")];

/// The keybinding tables quote the label; the inspector matches the key.
/// One spelling feeds both.
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

    /// Why the key did nothing, for a run the control cannot act on. A
    /// dimmed control that stays silent teaches nothing.
    const fn inert_reason(self) -> &'static str {
        match self {
            Self::Pause => PAUSE_INERT,
            Self::Resume => RESUME_INERT,
            Self::Stop => STOP_INERT,
        }
    }
}

/// The budget a budget-limited run is being resumed with, while it is being
/// typed. Mutually exclusive with the filter: one input owns the row.
struct BudgetPrompt {
    run_id: String,
    admitted: u32,
    input: TextBuffer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FooterCommand {
    Control(RunControl),
    Activate,
    Copy,
    Filter,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Section {
    Overview,
    Phases,
    Agents,
    Calls,
    Logs,
    Result,
}

impl Section {
    const ALL: [Self; 6] = [
        Self::Overview,
        Self::Phases,
        Self::Agents,
        Self::Calls,
        Self::Logs,
        Self::Result,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Phases => "Phases",
            Self::Agents => "Agents",
            Self::Calls => "Calls",
            Self::Logs => "Logs",
            Self::Result => "Result",
        }
    }

    fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|section| *section == self)
            .unwrap_or_default()
    }

    fn from_digit(digit: char) -> Option<Self> {
        let number = digit.to_digit(10)? as usize;
        Self::ALL.get(number.checked_sub(1)?).copied()
    }

    fn step(self, delta: isize) -> Self {
        let len = Self::ALL.len() as isize;
        let index = (self.index() as isize + delta).rem_euclid(len) as usize;
        Self::ALL[index]
    }

    /// Sections whose rows a cursor walks. Enter also acts on the result,
    /// which has one thing to open and no cursor to place.
    fn has_items(self) -> bool {
        matches!(self, Self::Phases | Self::Agents | Self::Calls)
    }

    /// Logs follow their tail; everything else opens at the top.
    fn scroll(self) -> ModalScroll {
        match self {
            Self::Logs => ModalScroll::new(),
            _ => ModalScroll::new_top(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Group {
    Running,
    Waiting,
    Finished,
    Earlier,
}

impl Group {
    const ALL: [Self; 4] = [Self::Running, Self::Waiting, Self::Finished, Self::Earlier];

    fn of(status: RunStatus) -> Self {
        match status {
            RunStatus::Active => Self::Running,
            RunStatus::Paused | RunStatus::BudgetLimited => Self::Waiting,
            RunStatus::Interrupted
            | RunStatus::Completed
            | RunStatus::Cancelled
            | RunStatus::Failed => Self::Finished,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Running => GROUP_RUNNING,
            Self::Waiting => GROUP_WAITING,
            Self::Finished => GROUP_FINISHED,
            Self::Earlier => GROUP_EARLIER,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pane {
    Runs,
    Detail,
}

/// One run row of the list, in the order the list draws them.
struct Entry<'a> {
    group: Group,
    run: &'a RunSnapshot,
    session_title: Option<&'a str>,
}

#[must_use]
#[derive(Debug, PartialEq, Eq)]
pub enum InspectorAction {
    Consumed,
    Close,
    /// The selection moved, or the selected run did: ask for its detail.
    Inspect(String),
    Control {
        control: RunControl,
        run_id: String,
    },
    OpenTranscript(String),
    /// A budget-limited run, and the raised budget to resume it with.
    ResumeWithBudget {
        run_id: String,
        agent_budget: u32,
    },
    /// A scratch file, to open in the workbench.
    OpenFile(PathBuf),
    /// The reader expanded a call whose body is not loaded yet.
    LoadCallBody {
        run_id: String,
        call_key: u64,
    },
    Copy {
        text: String,
        label: &'static str,
    },
    Flash(&'static str),
}

pub struct WorkflowInspector {
    open: bool,
    runs: Vec<RunSnapshot>,
    history: Vec<RunHistoryEntry>,
    selected: Option<String>,
    section: Section,
    detail: Option<RunDetail>,
    filter: TextBuffer,
    filter_focused: bool,
    budget: Option<BudgetPrompt>,
    pane: Pane,
    cursor: usize,
    expanded_call: Option<u64>,
    scroll: ModalScroll,
    scrollbar: Scrollbar,
    list_offset: u16,
    popup: Rect,
    list_area: Rect,
    tabs_area: Rect,
    body_area: Rect,
    /// Visual row spans of the list's run rows and the body's items, as
    /// last drawn, so a click can name what it landed on.
    list_rows: Vec<(u16, String)>,
    item_rows: Vec<(u16, u16)>,
    tab_hits: Vec<(Rect, Section)>,
    /// Where the pointer last was, kept rather than resolved, so each pane
    /// answers for its own geometry on the frame it is drawn.
    pointer: Option<Position>,
    /// Whether the body may scroll to show the cursor. A cursor the pointer
    /// moved is already under the pointer, and revealing it would slide the
    /// rows out from under the hand that pointed at them.
    reveal_cursor: bool,
    footer: FooterLine,
    footer_hits: FooterHits,
    /// What each running agent is doing, by run and by the call that
    /// launched it. The roster carries no activity, so it arrives on the
    /// agent's own event stream and is dropped when the agent stops.
    live: HashMap<String, HashMap<u64, ToolProgress>>,
    /// Untruncated call text, by call key, for the selected run only. A row's
    /// preview stands in until its body lands, and the map is dropped whole
    /// when the selection moves, because the keys belong to one run.
    bodies: HashMap<u64, BodyState>,
}

/// A call body's journey from asked-for to readable.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BodyState {
    Requested,
    Loaded(RunCallBody),
    Missing,
}

impl WorkflowInspector {
    pub fn new() -> Self {
        Self {
            open: false,
            runs: Vec::new(),
            history: Vec::new(),
            selected: None,
            section: Section::Overview,
            detail: None,
            filter: TextBuffer::new(String::new()),
            filter_focused: false,
            budget: None,
            pane: Pane::Runs,
            cursor: 0,
            expanded_call: None,
            scroll: Section::Overview.scroll(),
            scrollbar: Scrollbar::default(),
            list_offset: 0,
            popup: Rect::default(),
            list_area: Rect::default(),
            tabs_area: Rect::default(),
            body_area: Rect::default(),
            list_rows: Vec::new(),
            item_rows: Vec::new(),
            tab_hits: Vec::new(),
            pointer: None,
            reveal_cursor: true,
            footer: FooterLine::default(),
            footer_hits: FooterHits::default(),
            live: HashMap::new(),
            bodies: HashMap::new(),
        }
    }

    /// Opens on `preferred` when it is listed, else on the newest run. The
    /// run to inspect first, when there is one.
    pub fn open(&mut self, runs: Vec<RunSnapshot>, preferred: Option<&str>) -> Option<String> {
        self.open = true;
        self.runs = runs;
        self.prune_live();
        self.history.clear();
        self.filter.clear();
        self.filter_focused = false;
        self.pane = Pane::Runs;
        self.section = Section::Overview;
        self.selected = None;
        self.footer_hits.reset();
        self.pointer = None;
        self.reveal_cursor = true;
        let wanted = preferred
            .filter(|id| self.session_run(id).is_some())
            .map(str::to_owned);
        match wanted {
            Some(run_id) => requested(self.select(Some(run_id))),
            None => requested(self.settle_selection()),
        }
    }

    /// Adopts the runtime's read model. The run to inspect again when the
    /// selected one moved since its detail was fetched.
    pub fn refresh(&mut self, runs: Vec<RunSnapshot>) -> Option<String> {
        if !self.open {
            return None;
        }
        self.runs = runs;
        self.prune_live();
        let Some(selected) = self.selected.clone() else {
            return requested(self.settle_selection());
        };
        let Some(run) = self.session_run(&selected) else {
            return requested(self.settle_selection());
        };
        let known = self.detail.as_ref().map(|detail| detail.run.revision);
        if known.is_some_and(|revision| revision < run.revision) {
            let run = run.clone();
            if let Some(detail) = &mut self.detail {
                detail.run = run;
            }
            return Some(selected);
        }
        None
    }

    /// What one of a run's agents is doing now. Reports arrive whether the
    /// inspector is open or not, so that a run opened mid-flight reads as
    /// busy straight away rather than after the next report.
    pub fn set_progress(&mut self, run_id: &str, call_key: u64, report: SubagentProgress) {
        self.live
            .entry(run_id.to_owned())
            .or_default()
            .insert(call_key, ToolProgress::live(report));
    }

    /// Activity outlives no agent: an entry is kept only while the roster
    /// still says the agent it belongs to is running.
    fn prune_live(&mut self) {
        let runs = &self.runs;
        self.live.retain(|run_id, agents| {
            let Some(run) = runs.iter().find(|run| run.run_id == *run_id) else {
                return false;
            };
            agents.retain(|call_key, _| {
                run.roster
                    .iter()
                    .any(|agent| agent.call_key == *call_key && agent.state == RosterState::Running)
            });
            !agents.is_empty()
        });
    }

    pub fn fill_detail(&mut self, detail: RunDetail) {
        if self.selected.as_deref() == Some(detail.run.run_id.as_str()) {
            self.detail = Some(detail);
        }
    }

    /// Bodies for the run they were asked about. A key that came back with
    /// nothing is remembered as missing, so the row stops asking.
    pub fn fill_call_bodies(&mut self, run_id: &str, asked: Option<u64>, bodies: Vec<RunCallBody>) {
        if self.selected.as_deref() != Some(run_id) {
            return;
        }
        if let Some(call_key) = asked
            && !bodies.iter().any(|body| body.call_key == call_key)
        {
            self.bodies.insert(call_key, BodyState::Missing);
        }
        for body in bodies {
            self.bodies.insert(body.call_key, BodyState::Loaded(body));
        }
    }

    /// The runs of earlier sessions. The run to inspect when nothing of this
    /// session was there to select.
    pub fn fill_history(&mut self, history: Vec<RunHistoryEntry>) -> Option<String> {
        self.history = history;
        match self.selected.is_none() {
            true => requested(self.settle_selection()),
            false => None,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn close(&mut self) {
        self.open = false;
        self.budget = None;
        self.runs.clear();
        self.history.clear();
        self.detail = None;
        self.selected = None;
        self.bodies.clear();
        self.list_rows.clear();
        self.item_rows.clear();
        self.tab_hits.clear();
        self.footer_hits.reset();
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.open && self.popup.contains(pos)
    }

    #[cfg(test)]
    pub(crate) fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn section(&self) -> Section {
        self.section
    }

    #[cfg(test)]
    pub(crate) fn run_count(&self) -> usize {
        self.runs.len()
    }

    #[cfg(test)]
    pub(crate) fn history_count(&self) -> usize {
        self.history.len()
    }

    #[cfg(test)]
    pub(crate) fn live_count(&self) -> usize {
        self.live.values().map(HashMap::len).sum()
    }

    /// The wheel over the list walks the selection; over the body it scrolls
    /// the section.
    pub fn scroll_at(&mut self, pos: Position, delta: i32) -> InspectorAction {
        if self.list_area.contains(pos) {
            return self.step_selection(-delta.signum() as isize);
        }
        self.scroll.scroll(delta);
        InspectorAction::Consumed
    }

    /// A paste lands in the filter when it has the focus. `None` when the
    /// inspector did not take it.
    pub fn handle_paste(&mut self, text: &str) -> Option<InspectorAction> {
        if !self.open {
            return None;
        }
        if let Some(prompt) = &mut self.budget {
            prompt.input.insert_text(text);
            return Some(InspectorAction::Consumed);
        }
        if !self.filter_focused {
            return None;
        }
        self.filter.insert_text(text);
        Some(self.settle_selection())
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> InspectorAction {
        self.reveal_cursor = true;
        if self.budget.is_some() {
            return self.handle_budget_key(key);
        }
        if self.filter_focused {
            return self.handle_filter_key(key);
        }
        let plain = key.modifiers.is_empty();
        match key.code {
            KeyCode::Esc => return InspectorAction::Close,
            KeyCode::Tab => self.set_section(self.section.step(1)),
            KeyCode::BackTab => self.set_section(self.section.step(-1)),
            KeyCode::Char(digit) if plain && digit.is_ascii_digit() => {
                if let Some(section) = Section::from_digit(digit) {
                    self.set_section(section);
                }
            }
            KeyCode::Char(FILTER_KEY) if plain => {
                self.budget = None;
                self.filter_focused = true;
            }
            KeyCode::Left => self.pane = Pane::Runs,
            KeyCode::Right => self.pane = Pane::Detail,
            KeyCode::Up => return self.step(-1),
            KeyCode::Down => return self.step(1),
            KeyCode::Enter => return self.activate(),
            KeyCode::Char(PAUSE_KEY) if plain => return self.control(RunControl::Pause),
            KeyCode::Char(RESUME_KEY) if plain => return self.control(RunControl::Resume),
            KeyCode::Char(STOP_KEY) if plain => return self.control(RunControl::Stop),
            KeyCode::Char(COPY_KEY) if plain => return self.copy(),
            _ => {
                self.scroll.handle_key(key);
            }
        }
        InspectorAction::Consumed
    }

    /// The prompt owns every key while it is up, so a digit cannot reach the
    /// section tabs and Esc leaves the inspector standing.
    fn handle_budget_key(&mut self, key: KeyEvent) -> InspectorAction {
        match key.code {
            KeyCode::Esc => self.budget = None,
            KeyCode::Enter => return self.resume_with_budget(),
            _ => {
                if let Some(prompt) = &mut self.budget {
                    prompt.input.handle_key(key);
                }
            }
        }
        InspectorAction::Consumed
    }

    /// A budget has to be a number the run has not already spent, else the
    /// runtime would refuse the resume and the prompt would have lied.
    fn resume_with_budget(&mut self) -> InspectorAction {
        let Some(prompt) = &self.budget else {
            return InspectorAction::Consumed;
        };
        let asked = prompt.input.value().trim().parse::<u32>().ok();
        let Some(agent_budget) =
            asked.filter(|budget| *budget > prompt.admitted && *budget <= MAX_AGENT_BUDGET)
        else {
            return InspectorAction::Flash(BUDGET_INVALID);
        };
        let run_id = prompt.run_id.clone();
        self.budget = None;
        InspectorAction::ResumeWithBudget {
            run_id,
            agent_budget,
        }
    }

    fn handle_filter_key(&mut self, key: KeyEvent) -> InspectorAction {
        match key.code {
            KeyCode::Esc => {
                self.filter_focused = false;
                self.filter.clear();
                return self.settle_selection();
            }
            KeyCode::Enter => self.filter_focused = false,
            _ => {
                let before = self.filter.value();
                self.filter.handle_key(key);
                if self.filter.value() != before {
                    return self.settle_selection();
                }
            }
        }
        InspectorAction::Consumed
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> InspectorAction {
        match self.scrollbar.handle(&event) {
            ScrollbarMouse::Ignored => {}
            ScrollbarMouse::Consumed => return InspectorAction::Consumed,
            ScrollbarMouse::ScrollTo(top) => {
                self.scroll.scroll_to(top as u16);
                return InspectorAction::Consumed;
            }
        }
        let pos = Position::new(event.column, event.row);
        self.pointer = Some(pos);
        self.reveal_cursor = true;
        if let Some(index) = self.footer_hits.handle_mouse(event) {
            return self.footer_command(index);
        }
        // A move paints the tabs and the run list from `pointer` on the next
        // frame, so it only has to reach the body, where hovering a row is
        // what puts the cursor on it.
        let hovering = event.kind == MouseEventKind::Moved;
        if !hovering && event.kind != MouseEventKind::Down(MouseButton::Left) {
            return InspectorAction::Consumed;
        }
        if let Some((_, section)) = self.tab_hits.iter().find(|(hit, _)| hit.contains(pos)) {
            if !hovering {
                self.set_section(*section);
            }
            return InspectorAction::Consumed;
        }
        if self.list_area.contains(pos) {
            if hovering {
                return InspectorAction::Consumed;
            }
            self.pane = Pane::Runs;
            let row = event.row - self.list_area.y + self.list_offset;
            let hit = self
                .list_rows
                .iter()
                .find(|(at, _)| *at == row)
                .map(|(_, run_id)| run_id.clone());
            return match hit {
                Some(run_id) => self.select(Some(run_id)),
                None => InspectorAction::Consumed,
            };
        }
        if self.body_area.contains(pos) {
            let row = event.row - self.body_area.y + self.scroll.offset();
            let hit = self
                .item_rows
                .iter()
                .position(|(start, height)| (*start..start.saturating_add(*height)).contains(&row));
            if hovering {
                // Only a row the cursor can land on takes the pane: the cursor
                // is the one thing the pane makes visible, so a graze over a
                // section without rows must not move the keys.
                if let Some(index) = hit.filter(|_| self.section.has_items()) {
                    self.pane = Pane::Detail;
                    self.cursor = index;
                    self.reveal_cursor = false;
                }
                return InspectorAction::Consumed;
            }
            self.pane = Pane::Detail;
            match (self.section, hit) {
                (Section::Result, Some(_)) => return self.activate(),
                (Section::Phases | Section::Agents | Section::Calls, Some(index))
                    if index == self.cursor =>
                {
                    return self.activate();
                }
                (_, Some(index)) => self.cursor = index,
                (_, None) => {}
            }
        }
        InspectorAction::Consumed
    }

    fn footer_command(&mut self, index: usize) -> InspectorAction {
        match FOOTER.get(index).map(|(_, _, command)| *command) {
            Some(FooterCommand::Control(control)) => self.control(control),
            Some(FooterCommand::Activate) => self.activate(),
            Some(FooterCommand::Copy) => self.copy(),
            Some(FooterCommand::Filter) => {
                self.filter_focused = true;
                InspectorAction::Consumed
            }
            Some(FooterCommand::Close) => InspectorAction::Close,
            None => InspectorAction::Consumed,
        }
    }

    fn set_section(&mut self, section: Section) {
        if self.section == section {
            return;
        }
        self.section = section;
        self.scroll = section.scroll();
        self.cursor = 0;
        self.footer_hits.clear();
    }

    /// Up and down walk whichever pane has the focus: the run list, the
    /// section's rows, or the section's text.
    fn step(&mut self, delta: isize) -> InspectorAction {
        match self.pane {
            Pane::Runs => self.step_selection(delta),
            Pane::Detail if self.section.has_items() => {
                let count = self.item_count();
                if count > 0 {
                    self.cursor =
                        (self.cursor as isize + delta).clamp(0, count as isize - 1) as usize;
                }
                InspectorAction::Consumed
            }
            Pane::Detail => {
                self.scroll.scroll(-delta as i32);
                InspectorAction::Consumed
            }
        }
    }

    fn step_selection(&mut self, delta: isize) -> InspectorAction {
        let ids: Vec<String> = self
            .entries()
            .iter()
            .map(|entry| entry.run.run_id.clone())
            .collect();
        if ids.is_empty() {
            return InspectorAction::Consumed;
        }
        let at = self
            .selected
            .as_ref()
            .and_then(|selected| ids.iter().position(|id| id == selected))
            .map_or(0, |at| {
                (at as isize + delta).clamp(0, ids.len() as isize - 1) as usize
            });
        self.select(Some(ids[at].clone()))
    }

    fn select(&mut self, run_id: Option<String>) -> InspectorAction {
        if self.selected == run_id {
            return InspectorAction::Consumed;
        }
        self.selected = run_id;
        self.detail = None;
        self.expanded_call = None;
        self.bodies.clear();
        self.cursor = 0;
        self.scroll = self.section.scroll();
        match &self.selected {
            Some(run_id) => InspectorAction::Inspect(run_id.clone()),
            None => InspectorAction::Consumed,
        }
    }

    /// Keeps the selection on a listed run: the same one when it is still
    /// listed, else the first.
    fn settle_selection(&mut self) -> InspectorAction {
        let entries = self.entries();
        let kept = self
            .selected
            .as_deref()
            .filter(|selected| entries.iter().any(|entry| entry.run.run_id == *selected));
        let next = kept
            .or_else(|| entries.first().map(|entry| entry.run.run_id.as_str()))
            .map(str::to_owned);
        self.select(next)
    }

    fn activate(&mut self) -> InspectorAction {
        match self.section {
            Section::Phases => self.open_phase(),
            Section::Agents => match self
                .selected_run()
                .and_then(|run| agent_order(run).get(self.cursor).map(|at| &run.roster[*at]))
            {
                Some(agent) => match &agent.task_id {
                    Some(task_id) => InspectorAction::OpenTranscript(task_id.clone()),
                    None => InspectorAction::Flash(NO_TRANSCRIPT),
                },
                None => InspectorAction::Consumed,
            },
            Section::Calls => {
                let Some(call) = self.calls().get(self.cursor) else {
                    return InspectorAction::Consumed;
                };
                if call.kind == CallKind::ScratchFile
                    && let Some(path) = &call.result_preview
                {
                    return InspectorAction::OpenFile(PathBuf::from(path));
                }
                let key = call.call_key;
                self.expand_call(key)
            }
            Section::Result => match self.selected_run().and_then(RunSnapshot::scratch_path) {
                Some(path) => InspectorAction::OpenFile(PathBuf::from(path)),
                None => InspectorAction::Consumed,
            },
            _ => InspectorAction::Consumed,
        }
    }

    /// Toggles a call open. Opening one whose body has never been asked for
    /// asks once: the row reads from its preview until the answer lands, and
    /// a second open costs nothing.
    fn expand_call(&mut self, call_key: u64) -> InspectorAction {
        if self.expanded_call == Some(call_key) {
            self.expanded_call = None;
            return InspectorAction::Consumed;
        }
        self.expanded_call = Some(call_key);
        let Some(run_id) = self.selected.clone() else {
            return InspectorAction::Consumed;
        };
        if self.bodies.contains_key(&call_key) {
            return InspectorAction::Consumed;
        }
        self.bodies.insert(call_key, BodyState::Requested);
        InspectorAction::LoadCallBody { run_id, call_key }
    }

    /// What an opened call shows: what it was asked, what it answered, and
    /// what went wrong. The stored body replaces the row's preview once it
    /// lands, so the reader never has to know which one they are looking at.
    fn body_lines(&self, call: &RunCall) -> Vec<Line<'static>> {
        let t = theme::current();
        let body = match self.bodies.get(&call.call_key) {
            Some(BodyState::Loaded(body)) => Some(body),
            _ => None,
        };
        let mut lines = Vec::new();
        let request = body
            .map(|body| body.request.as_str())
            .or(call.prompt.as_deref());
        if let Some(request) = request {
            lines.push(Line::styled(PROMPT_HEADING, t.tool_dim));
            lines.extend(indented(request, Style::default()));
        }
        let error = body
            .and_then(|body| body.error.as_deref())
            .or(call.error.as_deref());
        if let Some(error) = error {
            lines.push(Line::styled(ERROR_HEADING, t.tool_dim));
            lines.extend(indented(error, t.tool_error));
        }
        let result = body
            .and_then(|body| body.result.as_deref())
            .or(call.result_preview.as_deref());
        if let Some(result) = result {
            lines.push(Line::styled(RESULT_HEADING, t.tool_dim));
            lines.extend(indented(result, Style::default()));
        }
        if body.is_none() && lines.is_empty() {
            lines.push(Line::styled(
                match self.bodies.get(&call.call_key) {
                    Some(BodyState::Missing) => BODY_MISSING,
                    _ => LOADING,
                },
                t.tool_dim,
            ));
        }
        lines
    }

    /// A phase row is a link into the roster: opening it lands the cursor on
    /// the first agent that phase dispatched.
    fn open_phase(&mut self) -> InspectorAction {
        let Some(run) = self.selected_run() else {
            return InspectorAction::Consumed;
        };
        let Some(title) = phase_titles(run)
            .get(self.cursor)
            .map(|title| title.to_string())
        else {
            return InspectorAction::Consumed;
        };
        let Some(at) = agent_order(run)
            .iter()
            .position(|index| run.roster[*index].phase.as_deref() == Some(title.as_str()))
        else {
            return InspectorAction::Flash(NO_AGENTS_IN_PHASE);
        };
        self.set_section(Section::Agents);
        self.cursor = at;
        InspectorAction::Consumed
    }

    fn control(&mut self, control: RunControl) -> InspectorAction {
        let Some(run) = self.selected_run() else {
            return InspectorAction::Consumed;
        };
        if self.is_foreign(&run.run_id) {
            return InspectorAction::Flash(FOREIGN_RUN);
        }
        if !control.applies_to(run.status) {
            return InspectorAction::Flash(control.inert_reason());
        }
        // A bare resume of a budget-limited run is refused by the runtime:
        // the only way on is a higher budget, so ask for one.
        if control == RunControl::Resume && run.status == RunStatus::BudgetLimited {
            return self.ask_budget();
        }
        InspectorAction::Control {
            control,
            run_id: run.run_id.clone(),
        }
    }

    fn ask_budget(&mut self) -> InspectorAction {
        let Some(run) = self.selected_run() else {
            return InspectorAction::Consumed;
        };
        let admitted = run.usage.agents_admitted;
        let run_id = run.run_id.clone();
        if admitted >= MAX_AGENT_BUDGET {
            return InspectorAction::Flash(BUDGET_MAXED);
        }
        let suggested = admitted
            .saturating_add(BUDGET_STEP)
            .min(MAX_AGENT_BUDGET)
            .to_string();
        let mut input = TextBuffer::new(suggested.clone());
        // Behind the suggestion, so accepting it is Enter and replacing it is
        // backspace rather than a cursor trip.
        input.set_cursor(0, suggested.chars().count());
        self.filter_focused = false;
        self.budget = Some(BudgetPrompt {
            run_id,
            admitted,
            input,
        });
        InspectorAction::Consumed
    }

    fn copy(&self) -> InspectorAction {
        if self.selected_run().is_none() {
            return InspectorAction::Consumed;
        }
        let (lines, _) = self.section_lines(now_secs());
        let text = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        InspectorAction::Copy {
            text,
            label: COPIED,
        }
    }

    /// What a stalled run of this session is waiting for the reader to do.
    /// A run of an earlier session is read-only, so it is told nothing.
    fn next_move(&self, run: &RunSnapshot) -> Option<&'static str> {
        if self.is_foreign(&run.run_id) {
            return None;
        }
        match run.status {
            RunStatus::BudgetLimited => Some(BUDGET_LIMITED_HINT),
            RunStatus::Failed => Some(FAILED_HINT),
            _ => None,
        }
    }

    fn session_run(&self, run_id: &str) -> Option<&RunSnapshot> {
        self.runs.iter().find(|run| run.run_id == run_id)
    }

    fn is_foreign(&self, run_id: &str) -> bool {
        self.session_run(run_id).is_none()
    }

    /// The selected run at its freshest: the detail's copy is the runtime's
    /// answer, the list's copy is what the mirror has since heard.
    fn selected_run(&self) -> Option<&RunSnapshot> {
        let selected = self.selected.as_deref()?;
        self.session_run(selected).or_else(|| {
            self.history
                .iter()
                .map(|entry| &entry.run)
                .find(|run| run.run_id == selected)
        })
    }

    fn session_title(&self, run_id: &str) -> Option<&str> {
        self.history
            .iter()
            .find(|entry| entry.run.run_id == run_id)
            .map(|entry| entry.session_title.as_str())
    }

    fn calls(&self) -> &[RunCall] {
        self.detail.as_ref().map_or(&[], |detail| &detail.calls)
    }

    fn item_count(&self) -> usize {
        match self.section {
            Section::Phases => self.selected_run().map_or(0, |run| phase_titles(run).len()),
            Section::Agents => self.selected_run().map_or(0, |run| run.roster.len()),
            Section::Calls => self.calls().len(),
            _ => 0,
        }
    }

    fn cursor_mark(&self, position: usize) -> &'static str {
        match self.pane == Pane::Detail && position == self.cursor {
            true => CURSOR_MARK,
            false => NO_MARK,
        }
    }

    /// The run rows in list order, narrowed by the filter.
    fn entries(&self) -> Vec<Entry<'_>> {
        let needle = self.filter.value().to_lowercase();
        let matches = |run: &RunSnapshot, title: Option<&str>| {
            needle.is_empty()
                || run.display_name.to_lowercase().contains(&needle)
                || run.workflow_name.to_lowercase().contains(&needle)
                || title.is_some_and(|title| title.to_lowercase().contains(&needle))
        };
        let mut entries: Vec<Entry<'_>> = self
            .runs
            .iter()
            .filter(|run| matches(run, None))
            .map(|run| Entry {
                group: Group::of(run.status),
                run,
                session_title: None,
            })
            .collect();
        entries.extend(
            self.history
                .iter()
                .filter(|entry| matches(&entry.run, Some(&entry.session_title)))
                .map(|entry| Entry {
                    group: Group::Earlier,
                    run: &entry.run,
                    session_title: Some(entry.session_title.as_str()),
                }),
        );
        entries.sort_by_key(|entry| entry.group);
        entries
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        if !self.open {
            return Rect::default();
        }
        let modal = Modal {
            title: TITLE,
            width_percent: WIDTH_PERCENT,
            max_height_percent: MAX_HEIGHT_PERCENT,
        };
        let (popup, inner) = modal.render(frame, area, area.height);
        let padded = Rect {
            x: inner.x.saturating_add(H_PAD),
            width: inner.width.saturating_sub(H_PAD.saturating_mul(2)),
            ..inner
        };
        let list_width = (padded.width * LIST_PERCENT / 100).min(LIST_MAX_WIDTH);
        let [list, _, detail] = Layout::horizontal([
            Constraint::Length(list_width),
            Constraint::Length(PANE_GAP),
            Constraint::Fill(1),
        ])
        .areas(padded);
        let filtering = self.filter_focused || !self.filter.value().is_empty();
        let input_row = filtering || self.budget.is_some();
        let footer_rows = 1 + u16::from(input_row);
        let panes_height = padded.height.saturating_sub(footer_rows);
        let list = Rect {
            height: panes_height,
            ..list
        };
        let [tabs, body] =
            Layout::vertical([Constraint::Length(CHROME_ROWS - 1), Constraint::Fill(1)]).areas(
                Rect {
                    height: panes_height,
                    ..detail
                },
            );
        self.popup = popup;
        self.list_area = list;
        self.tabs_area = tabs;
        self.body_area = body;

        self.render_list(frame, list);
        self.render_tabs(frame, tabs);
        self.render_body(frame, body);

        let mut row = padded.y.saturating_add(panes_height);
        if input_row {
            let line = match &self.budget {
                Some(prompt) => {
                    let mut spans = vec![Span::styled(BUDGET_LABEL, theme::current().tool_dim)];
                    spans.extend(input_line_with_cursor(&prompt.input).spans);
                    Line::from(spans)
                }
                None => input_line_with_cursor(&self.filter),
            };
            frame.render_widget(
                Paragraph::new(line),
                Rect {
                    y: row,
                    height: 1,
                    ..padded
                },
            );
            row = row.saturating_add(1);
        }
        let footer = Rect {
            y: row,
            height: 1,
            ..padded
        };
        self.footer = self.footer_line();
        self.footer_hits.set(self.footer.hits(footer, 0, 1));
        frame.render_widget(
            Paragraph::new(self.footer.line(self.footer_hits.hovered())),
            footer,
        );
        popup
    }

    fn render_list(&mut self, frame: &mut Frame, area: Rect) {
        let t = theme::current();
        let entries = self.entries();
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(entries.len() + Group::ALL.len());
        let mut rows = Vec::with_capacity(entries.len());
        let mut selected_row = None;
        let mut group = None;
        let spinner = spinner_str(animation_elapsed_ms());
        let now = now_secs();
        // The offset the last frame settled on is the one the pointer was
        // reported against, and it is clamped again below.
        let hovered = self
            .pointer
            .filter(|at| area.contains(*at))
            .map(|at| at.y - area.y + self.list_offset);
        for entry in &entries {
            if group != Some(entry.group) {
                group = Some(entry.group);
                lines.push(Line::styled(entry.group.label(), t.keybind_section));
            }
            let selected = self.selected.as_deref() == Some(entry.run.run_id.as_str());
            let row = u16::try_from(lines.len()).unwrap_or(u16::MAX);
            if selected {
                selected_row = Some(row);
            }
            rows.push((row, entry.run.run_id.clone()));
            let style = hover_style(
                match selected {
                    true => t.item_selected,
                    false => t.item,
                },
                hovered == Some(row),
            );
            lines.push(list_row(entry, style, spinner, now, area.width));
        }
        if lines.is_empty() {
            match self.runs.is_empty() && self.history.is_empty() {
                true => {
                    lines.push(Line::styled(EMPTY_TEXT, t.tool_dim));
                    lines.push(Line::default());
                    lines.push(Line::styled(EMPTY_HINT, t.tool_dim));
                }
                false => lines.push(Line::styled(NO_MATCH, t.tool_dim)),
            }
        }
        self.list_rows = rows;
        let total = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        let max_offset = total.saturating_sub(area.height);
        if let Some(row) = selected_row {
            if row < self.list_offset {
                self.list_offset = row;
            } else if row >= self.list_offset.saturating_add(area.height) {
                self.list_offset = row.saturating_sub(area.height.saturating_sub(1));
            }
        }
        self.list_offset = self.list_offset.min(max_offset);
        frame.render_widget(Paragraph::new(lines).scroll((self.list_offset, 0)), area);
    }

    fn render_tabs(&mut self, frame: &mut Frame, area: Rect) {
        let t = theme::current();
        let mut spans = Vec::with_capacity(Section::ALL.len() * 2);
        let mut hits = Vec::with_capacity(Section::ALL.len());
        let mut x = area.x;
        for section in Section::ALL {
            let text = format!("{} {}", section.index() + 1, section.label());
            let width = u16::try_from(text.len()).unwrap_or(u16::MAX);
            let style = match section == self.section {
                true => t.item_selected,
                false => t.tool_dim,
            };
            let hit = Rect::new(x, area.y, width, 1);
            hits.push((hit, section));
            spans.push(Span::styled(
                text,
                hover_style(style, self.pointer.is_some_and(|at| hit.contains(at))),
            ));
            spans.push(Span::raw(SECTION_GAP));
            x = x
                .saturating_add(width)
                .saturating_add(u16::try_from(SECTION_GAP.len()).unwrap_or(u16::MAX));
        }
        self.tab_hits = hits;
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn render_body(&mut self, frame: &mut Frame, area: Rect) {
        let (lines, item_starts) = self.section_lines(now_secs());
        let rows = visual_rows(&lines, area.width);
        self.scroll.update_dimensions(rows.total, area.height);
        self.item_rows = item_starts
            .iter()
            .map(|&start| {
                let line = u16::try_from(start).unwrap_or(u16::MAX);
                (rows.row_of(line), rows.height_of(line))
            })
            .collect();
        if self.reveal_cursor
            && self.pane == Pane::Detail
            && self.section.has_items()
            && let Some(&(top, height)) = self.item_rows.get(self.cursor)
        {
            self.scroll.reveal(top, height);
        }
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((self.scroll.offset(), 0)),
            area,
        );
        self.scrollbar
            .draw(frame, area, rows.total, self.scroll.offset());
    }

    /// The selected section as lines, and the logical line each of its
    /// items starts on.
    fn section_lines(&self, now: u64) -> (Vec<Line<'static>>, Vec<usize>) {
        let t = theme::current();
        let Some(run) = self.selected_run() else {
            let text = match self.selected.is_some() {
                true => LOADING,
                false => NO_SELECTION,
            };
            return (vec![Line::styled(text, t.tool_dim)], Vec::new());
        };
        match self.section {
            Section::Overview => (self.overview_lines(run, now), Vec::new()),
            Section::Phases => self.phase_lines(run, now),
            Section::Agents => self.agent_lines(run),
            Section::Calls => self.call_lines(),
            Section::Logs => (self.log_lines(run), Vec::new()),
            Section::Result => result_lines(run),
        }
    }

    fn overview_lines(&self, run: &RunSnapshot, now: u64) -> Vec<Line<'static>> {
        let t = theme::current();
        let card = WorkflowRunCard::from(run);
        let mut lines = vec![Line::from(vec![
            Span::styled(escape_terminal_controls(&run.display_name), t.bold),
            Span::styled(
                format!(" ({}){SEPARATOR}{}", run.workflow_name, run.source_kind),
                t.tool_dim,
            ),
        ])];
        let mut status = vec![Span::raw(STATUS_LABEL), status_span(run.status)];
        if let Some(phase) = &run.phase {
            status.push(Span::raw(SEPARATOR));
            status.push(Span::raw(PHASE_LABEL));
            status.push(Span::styled(escape_terminal_controls(phase), t.accent));
            if let Some((at, of)) = run.phase_position() {
                status.push(Span::styled(format!(" {at}/{of}"), t.tool_dim));
            }
        }
        lines.push(Line::from(status));
        lines.push(Line::from(vec![
            Span::raw(ELAPSED_LABEL),
            Span::raw(format_elapsed(run.elapsed_secs(now))),
            Span::raw(SEPARATOR),
            Span::raw(AGENTS_LABEL),
            Span::raw(roster_tally(run)),
            Span::raw(SEPARATOR),
            Span::raw(format!(
                "{}{AGENT_SLASH}{}{ADMITTED_UNIT}",
                run.usage.agents_admitted, run.agent_budget
            )),
            Span::raw(SEPARATOR),
            Span::raw(TOKENS_LABEL),
            Span::raw(format_integer(run.usage.tokens_used)),
        ]));
        if let Some(objective) = &run.objective {
            lines.push(labelled(OBJECTIVE_LABEL, objective, Style::default()));
        }
        if let Some(title) = self.session_title(&run.run_id) {
            lines.push(labelled(SESSION_LABEL, title, t.tool_dim));
        }
        let strip = card.phase_strip();
        if !strip.is_empty() {
            lines.push(Line::default());
            lines.push(phase_strip_line(&strip));
        }
        if let Some(message) = &run.pause_message {
            lines.push(labelled(PAUSED_LABEL, message, t.tool_warning));
        }
        if let Some(error) = &run.error {
            lines.push(labelled(ERROR_LABEL, error, t.tool_error));
        }
        if let Some(hint) = self.next_move(run) {
            lines.push(Line::styled(hint, t.tool_warning));
        }
        if !card.logs.is_empty() {
            lines.push(Line::default());
            for log in &card.logs {
                lines.push(log_line(
                    log.at.saturating_sub(run.created_at),
                    &log.message,
                    t.tool_dim,
                ));
            }
        }
        lines
    }

    /// What the run has walked, then what the script still declares ahead of
    /// it. The entered phases are listed in the order they happened, repeats
    /// included, because the timeline is the record; the declared phases it
    /// has not reached follow as pending, because that is the part still to
    /// come.
    fn phase_lines(&self, run: &RunSnapshot, now: u64) -> (Vec<Line<'static>>, Vec<usize>) {
        let t = theme::current();
        let titles = phase_titles(run);
        if titles.is_empty() {
            return (vec![Line::styled(NO_PHASES, t.tool_dim)], Vec::new());
        }
        let end = match run.status.is_terminal() {
            true => run.updated_at,
            false => now,
        };
        let last = run.phase_history.len().saturating_sub(1);
        let mut lines: Vec<Line<'static>> = run
            .phase_history
            .iter()
            .enumerate()
            .map(|(index, record)| {
                let next_start = run
                    .phase_history
                    .get(index + 1)
                    .map_or(end, |next| next.started_at);
                let (mark, style) = match index == last && !run.status.is_terminal() {
                    true => (PhaseMark::Current, t.accent),
                    false => (PhaseMark::Done, t.tool_success),
                };
                // A phase entered twice would otherwise claim its agents twice
                // over, reading as more agents than the run ever dispatched.
                let final_visit = run
                    .phase_history
                    .iter()
                    .rposition(|other| other.title == record.title)
                    == Some(index);
                let mut spans = vec![
                    Span::styled(self.cursor_mark(index), t.accent),
                    Span::styled(format!("{} ", mark.glyph()), style),
                    Span::styled(escape_terminal_controls(&record.title), t.bold),
                    Span::styled(
                        format!(
                            "{SEPARATOR}{}{SEPARATOR}{}",
                            offset_text(record.started_at.saturating_sub(run.created_at)),
                            format_elapsed(next_start.saturating_sub(record.started_at))
                        ),
                        t.tool_dim,
                    ),
                ];
                if final_visit && let Some(tally) = agent_tally(run, &record.title) {
                    spans.push(Span::styled(tally, t.tool_dim));
                }
                Line::from(spans)
            })
            .collect();
        let walked = run.phase_history.len();
        lines.extend(
            titles
                .iter()
                .skip(walked)
                .enumerate()
                .map(|(offset, title)| {
                    Line::from(vec![
                        Span::styled(self.cursor_mark(walked + offset), t.accent),
                        Span::styled(format!("{} ", PhaseMark::Pending.glyph()), t.tool_dim),
                        Span::styled(escape_terminal_controls(title), t.tool_dim),
                    ])
                }),
        );
        let starts = (0..lines.len()).collect();
        (lines, starts)
    }

    /// The last progress report of an agent that is still running, when one
    /// has landed. A stopped agent is described by the roster instead.
    fn activity(&self, run: &RunSnapshot, agent: &AgentRosterEntry) -> Option<&ToolProgress> {
        if agent.state != RosterState::Running {
            return None;
        }
        self.live.get(&run.run_id)?.get(&agent.call_key)
    }

    /// The roster gathered under the phase that dispatched each agent, so a
    /// fan-out reads as the phase that opened it rather than as one long
    /// list. A run whose agents carry no phase keeps the plain list.
    fn agent_lines(&self, run: &RunSnapshot) -> (Vec<Line<'static>>, Vec<usize>) {
        let t = theme::current();
        if run.roster.is_empty() {
            return (vec![Line::styled(NO_AGENTS, t.tool_dim)], Vec::new());
        }
        let spinner = spinner_str(animation_elapsed_ms());
        let grouped = run.roster.iter().any(|agent| agent.phase.is_some());
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(run.roster.len());
        let mut starts = Vec::with_capacity(run.roster.len());
        let mut group: Option<&str> = None;
        for (position, index) in agent_order(run).into_iter().enumerate() {
            let agent = &run.roster[index];
            let header = agent.phase.as_deref().unwrap_or(UNPHASED_GROUP);
            if grouped && group != Some(header) {
                if group.is_some() {
                    lines.push(Line::default());
                }
                lines.push(Line::styled(escape_terminal_controls(header), t.tool_dim));
                group = Some(header);
            }
            let state = match agent.state {
                RosterState::Running => Span::styled(spinner.to_owned(), t.spinner),
                state => Span::styled(format!("{state} "), roster_style(state)),
            };
            let mut spans = vec![
                Span::styled(self.cursor_mark(position), t.accent),
                state,
                Span::styled(escape_terminal_controls(&agent.label), t.bold),
            ];
            match self.activity(run, agent) {
                Some(progress) => spans.extend(activity_spans(progress)),
                None => spans.push(Span::styled(
                    format!(
                        "{SEPARATOR}{}{TOKENS_UNIT}{SEPARATOR}{}",
                        format_compact(agent.tokens_used),
                        format_elapsed(agent.duration_ms / 1_000)
                    ),
                    t.tool_dim,
                )),
            }
            starts.push(lines.len());
            lines.push(Line::from(spans));
        }
        (lines, starts)
    }

    fn call_lines(&self) -> (Vec<Line<'static>>, Vec<usize>) {
        let t = theme::current();
        let Some(detail) = &self.detail else {
            return (vec![Line::styled(LOADING, t.tool_dim)], Vec::new());
        };
        let mut lines = Vec::with_capacity(detail.calls.len() + 1);
        let mut starts = Vec::with_capacity(detail.calls.len());
        if detail.journal_trimmed {
            lines.push(Line::styled(JOURNAL_TRIMMED, t.tool_warning));
        }
        if detail.calls.is_empty() && !detail.journal_trimmed {
            lines.push(Line::styled(NO_CALLS, t.tool_dim));
        }
        for (index, call) in detail.calls.iter().enumerate() {
            starts.push(lines.len());
            let mark = match self.pane == Pane::Detail && index == self.cursor {
                true => CURSOR_MARK,
                false => NO_MARK,
            };
            let mut spans = vec![
                Span::styled(mark, t.accent),
                Span::styled(format!("{CALL_PREFIX}{} ", call.call_key), t.tool_dim),
                Span::raw(call.kind.to_string()),
                Span::raw(SEPARATOR),
                Span::styled(call.state.to_string(), call_style(call.state)),
            ];
            if let Some(label) = &call.label {
                spans.push(Span::styled(
                    format!("{SEPARATOR}{}", escape_terminal_controls(label)),
                    t.bold,
                ));
            }
            let finished = call
                .finished_at
                .map(|at| at.saturating_sub(call.started_at));
            spans.push(Span::styled(
                format!(
                    "{SEPARATOR}{}{TOKENS_UNIT}{SEPARATOR}{}",
                    format_compact(call.tokens_used),
                    finished
                        .map_or_else(|| format_elapsed(call.duration_ms / 1_000), format_elapsed)
                ),
                t.tool_dim,
            ));
            lines.push(Line::from(spans));
            if self.expanded_call == Some(call.call_key) {
                lines.extend(self.body_lines(call));
            }
        }
        (lines, starts)
    }

    /// The stored timeline once the detail is in, and the snapshot's own
    /// log tail until then.
    fn log_lines(&self, run: &RunSnapshot) -> Vec<Line<'static>> {
        let t = theme::current();
        let lines: Vec<Line<'static>> = match &self.detail {
            Some(detail) => detail
                .events
                .iter()
                .map(|event| match event.kind {
                    RunEventKind::Phase => Line::from(vec![
                        Span::styled(
                            offset_text(event.at.saturating_sub(run.created_at)),
                            t.tool_dim,
                        ),
                        Span::styled(ARROW, t.accent),
                        Span::styled(escape_terminal_controls(&event.text), t.accent),
                    ]),
                    RunEventKind::Log => log_line(
                        event.at.saturating_sub(run.created_at),
                        &event.text,
                        Style::default(),
                    ),
                })
                .collect(),
            None => run
                .logs
                .iter()
                .map(|log| {
                    log_line(
                        log.at.saturating_sub(run.created_at),
                        &log.message,
                        Style::default(),
                    )
                })
                .collect(),
        };
        match lines.is_empty() {
            true => vec![Line::styled(NO_LOGS, t.tool_dim)],
            false => lines,
        }
    }

    fn footer_line(&self) -> FooterLine {
        let t = theme::current();
        if self.budget.is_some() {
            let mut footer = FooterLine::default();
            for (index, (key, description)) in BUDGET_FOOTER.iter().enumerate() {
                if index > 0 {
                    footer.text(SECTION_GAP, Style::default());
                }
                footer.text(*key, t.keybind_key);
                footer.text(format!(" {description}"), t.tool_dim);
            }
            return footer;
        }
        let run = self.selected_run();
        let controllable = run.is_some_and(|run| !self.is_foreign(&run.run_id));
        let mut footer = FooterLine::default();
        for (index, (key, description, command)) in FOOTER.iter().enumerate() {
            if index > 0 {
                footer.text(SECTION_GAP, Style::default());
            }
            let enabled = match command {
                FooterCommand::Control(control) => {
                    controllable && run.is_some_and(|run| control.applies_to(run.status))
                }
                FooterCommand::Activate => self.section.has_items() && self.item_count() > 0,
                FooterCommand::Copy => run.is_some(),
                FooterCommand::Filter | FooterCommand::Close => true,
            };
            let key_style = match enabled {
                true => t.keybind_key,
                false => t.tool_dim,
            };
            footer.command(key, key_style);
            footer.text(format!(" {description}"), t.tool_dim);
        }
        footer
    }
}

impl Default for WorkflowInspector {
    fn default() -> Self {
        Self::new()
    }
}

impl Overlay for WorkflowInspector {
    fn is_open(&self) -> bool {
        self.open
    }

    fn close(&mut self) {
        self.close();
    }

    fn cadence(&self) -> Cadence {
        let spinning = self.open && self.runs.iter().any(|run| run.status == RunStatus::Active);
        Cadence::when(spinning, Cadence::SPINNER)
    }
}

/// The run an action asks the app to fetch, when it asks for one.
fn requested(action: InspectorAction) -> Option<String> {
    match action {
        InspectorAction::Inspect(run_id) => Some(run_id),
        _ => None,
    }
}

/// `● deep-research · Research            2m14s`. The clock is right
/// aligned and never yields: how long a run has been going is the column a
/// reader scans, so the name and the phase give way to it instead.
fn list_row(
    entry: &Entry<'_>,
    style: Style,
    spinner: &'static str,
    now: u64,
    width: u16,
) -> Line<'static> {
    let t = theme::current();
    let mark = match entry.run.status == RunStatus::Active {
        true => spinner.to_owned(),
        false => NO_MARK.to_owned(),
    };
    let detail = match (entry.session_title, &entry.run.phase) {
        (Some(title), _) => title.to_owned(),
        (None, Some(phase)) => phase.clone(),
        (None, None) => entry.run.status.to_string(),
    };
    let name = escape_terminal_controls(&entry.run.display_name);
    let detail = escape_terminal_controls(&detail);
    let elapsed = format_elapsed(entry.run.elapsed_secs(now));
    let width = usize::from(width);
    let mut used = mark.chars().count() + name.chars().count();
    let mut spans = vec![Span::styled(mark, t.spinner), Span::styled(name, style)];
    let room = width
        .saturating_sub(used + SEPARATOR.chars().count() + elapsed.chars().count() + PANE_GAP_COLS);
    if room > 0 {
        let detail: String = detail.chars().take(room).collect();
        used += SEPARATOR.chars().count() + detail.chars().count();
        spans.push(Span::styled(format!("{SEPARATOR}{detail}"), t.item_desc));
    }
    let pad = width.saturating_sub(used + elapsed.chars().count());
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(elapsed, t.tool_dim));
    Line::from(spans)
}

/// Every phase row in display order: the ones the run walked, repeats
/// included, then the declared ones it has not reached.
fn phase_titles(run: &RunSnapshot) -> Vec<&str> {
    let walked = run.phase_history.iter().map(|record| record.title.as_str());
    let pending = run.phases.iter().map(String::as_str).filter(|title| {
        !run.phase_history
            .iter()
            .any(|record| record.title == *title)
    });
    walked.chain(pending).collect()
}

/// Roster indices in display order: the agents of a phase together, phases
/// in the order the run walked or declared them, unphased agents last. The
/// sort is stable, so a phase keeps its dispatch order.
fn agent_order(run: &RunSnapshot) -> Vec<usize> {
    let titles = phase_titles(run);
    let mut order: Vec<usize> = (0..run.roster.len()).collect();
    order.sort_by_key(|index| match &run.roster[*index].phase {
        Some(phase) => titles
            .iter()
            .position(|title| *title == phase)
            .unwrap_or(titles.len()),
        None => titles.len() + 1,
    });
    order
}

/// `3/8 done` across the whole roster, so the overview says how much of the
/// fleet has landed and not only how much of the budget is spent.
fn roster_tally(run: &RunSnapshot) -> String {
    let done = run
        .roster
        .iter()
        .filter(|agent| !matches!(agent.state, RosterState::Running | RosterState::Pending))
        .count();
    format!("{done}{AGENT_SLASH}{}{DONE_UNIT}", run.roster.len())
}

/// `· shell cargo test · 3 tools · 1m2s`, the shape a subagent's task header
/// already uses, so a workflow agent reads like any other agent.
fn activity_spans(progress: &ToolProgress) -> Vec<Span<'static>> {
    let t = theme::current();
    let mut spans = vec![
        Span::raw(SEPARATOR),
        Span::styled(progress.report.activity.label().to_owned(), t.tool_prefix),
    ];
    if let Some(detail) = progress.report.activity.detail() {
        spans.push(Span::styled(
            format!(" {}", escape_terminal_controls(detail)),
            t.tool_dim,
        ));
    }
    spans.push(Span::styled(
        format!(
            "{SEPARATOR}{}",
            SubagentProgress::tally(progress.report.tools, progress.elapsed())
        ),
        t.tool_dim,
    ));
    spans
}

/// `· 2/5 agents` for a phase that dispatched any, counting the ones that
/// have stopped against the ones it opened.
fn agent_tally(run: &RunSnapshot, phase: &str) -> Option<String> {
    let dispatched = run
        .roster
        .iter()
        .filter(|agent| agent.phase.as_deref() == Some(phase));
    let total = dispatched.clone().count();
    if total == 0 {
        return None;
    }
    let done = dispatched
        .filter(|agent| !matches!(agent.state, RosterState::Running | RosterState::Pending))
        .count();
    Some(format!("{SEPARATOR}{done}/{total}{AGENTS_UNIT}"))
}

/// The report or the raw result, then the scratch file line, which is the
/// section's one item so a click can land on it.
fn result_lines(run: &RunSnapshot) -> (Vec<Line<'static>>, Vec<usize>) {
    let t = theme::current();
    let mut lines = Vec::new();
    let mut starts = Vec::new();
    if let Some(result) = &run.result {
        match result.get(REPORT_FIELD).and_then(serde_json::Value::as_str) {
            Some(report) => lines.extend(report.lines().map(|line| Line::raw(line.to_owned()))),
            None => {
                let pretty =
                    serde_json::to_string_pretty(result).unwrap_or_else(|_| result.to_string());
                lines.push(Line::styled(RESULT_LABEL, t.tool_dim));
                lines.extend(pretty.lines().map(|line| Line::raw(line.to_owned())));
            }
        }
    }
    if let Some(path) = run.scratch_path() {
        lines.push(Line::default());
        starts.push(lines.len());
        lines.push(labelled(SCRATCH_LABEL, path, t.tool_path));
    }
    if let Some(message) = &run.pause_message {
        lines.push(labelled(PAUSED_LABEL, message, t.tool_warning));
    }
    if let Some(error) = &run.error {
        lines.push(labelled(ERROR_LABEL, error, t.tool_error));
    }
    match lines.is_empty() {
        true => (vec![Line::styled(NO_RESULT, t.tool_dim)], starts),
        false => (lines, starts),
    }
}

fn log_line(offset: u64, message: &str, style: Style) -> Line<'static> {
    let t = theme::current();
    Line::from(vec![
        Span::styled(offset_text(offset), t.tool_dim),
        Span::styled(escape_terminal_controls(message), style),
    ])
}

fn offset_text(seconds: u64) -> String {
    format!("+{:<7}", format_elapsed(seconds))
}

fn labelled(label: &'static str, text: &str, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(label, theme::current().tool_dim),
        Span::styled(escape_terminal_controls(text), style),
    ])
}

fn indented(text: &str, style: Style) -> Vec<Line<'static>> {
    text.lines()
        .map(|line| {
            Line::from(vec![
                Span::raw(EXPAND_INDENT),
                Span::styled(escape_terminal_controls(line), style),
            ])
        })
        .collect()
}

fn roster_style(state: RosterState) -> Style {
    let t = theme::current();
    match state {
        RosterState::Pending => t.tool_dim,
        RosterState::Running => t.accent,
        RosterState::Completed => t.tool_success,
        RosterState::Failed | RosterState::Cancelled => t.tool_error,
    }
}

fn call_style(state: CallState) -> Style {
    let t = theme::current();
    match state {
        CallState::Started => t.accent,
        CallState::Completed => t.tool_success,
        CallState::Failed => t.tool_error,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use caudra_agent::SubagentActivity;
    use caudra_workflow::{CallKind, PhaseRecord, RunUsage, SourceKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Modifier;
    use test_case::test_case;

    use super::*;
    use crate::components::key as key_event;

    const FRAME_WIDTH: u16 = 100;
    const FRAME_HEIGHT: u16 = 24;
    /// More agents than the body can show, so the cursor has to scroll to it.
    const TALL_ROSTER: u64 = 40;
    const NO_SUCH_ROW: &str = "the row was drawn on the frame under test";
    const ROW_STARTS_PLAIN: &str = "an unpointed row carries no hover mark";
    const ROW_MARKS_THE_POINTER: &str = "a run row marks itself while the pointer is on it";
    const ROW_RELEASES_THE_POINTER: &str = "a run row drops its mark when the pointer leaves";
    const HOVER_IS_NOT_A_CLICK: &str = "hovering must not act, only mark";
    const TAB_MARKS_THE_POINTER: &str = "a section tab marks itself while the pointer is on it";
    const CLICK_STILL_SWITCHES: &str = "pressing a tab still switches to it";
    const ONE_CLICK_OPENS: &str = "a click opens the row the pointer already put the cursor on";
    const HOVER_PLACES_THE_CURSOR: &str = "the pointer puts the cursor on the row under it";
    const POINTER_DOES_NOT_SCROLL: &str = "a cursor the pointer moved is already in view";
    const KEYS_STILL_SCROLL: &str = "the keys still scroll the cursor into view";
    const RUN_ID: &str = "run-1";
    const OTHER_RUN_ID: &str = "run-2";
    const OLD_RUN_ID: &str = "run-old";
    const TASK_ID: &str = "run-1:1";
    const PROMPT_PREVIEW: &str = "research the thing";
    const FULL_PROMPT: &str = "every last detail of what was asked";
    const FULL_RESULT: &str = "here is everything that was found";
    const ASKS_ONCE: &str = "an open call asks for its body exactly once";
    const BODY_WINS: &str = "a landed body replaces the row's preview";
    const STALE_BODY: &str = "a body for a run that is no longer selected must be dropped";
    const SESSION_TITLE: &str = "yesterday's research";
    const WRONG_RUN: &str = "a control must name the selected run";
    const INERT_KEY: &str = "a control the run cannot take must do nothing";
    const TRANSCRIPT: &str = "Enter on an agent row opens its transcript";
    const STALE_DETAIL: &str = "a detail for another run must be dropped";
    const REINSPECT: &str = "a moved run must be inspected again";
    const FOLLOW_RUN: &str = "the selection follows its run, not its position";
    const SCRATCH_PATH: &str = "/state/workflow_scratch/session/run-1/report.md";
    const OPENS_SCRATCH: &str = "Enter opens the scratch file the run wrote";
    const NO_SCRATCH: &str = "a result without a scratch file has nothing to open";
    const ADMITTED: u32 = 8;
    const SUGGESTED_BUDGET: &str = "72";
    const SUGGESTS_A_STEP_UP: &str = "the prompt offers a budget above what the run spent";
    const PROMPT_STANDS: &str = "a refused budget leaves the prompt up to correct";
    const PROMPT_IS_NOT_THE_MODAL: &str = "escaping the prompt must not close the inspector";
    const SHORT_NAME: &str = "deep-research";
    const PHASE_ONE: &str = "Research";
    const PHASE_TWO: &str = "Report";
    const ELAPSED_SECS: u64 = 134;
    const ELAPSED_TEXT: &str = "2m14s";
    const ONE_OF_ONE_AGENT: &str = "1/1 agents";
    const ROSTER_TALLY: &str = "0/1 done";
    const CLOCK_IS_LAST: &str = "the clock holds the right edge of a run row";
    const PHASE_COUNTS_ITS_OWN: &str = "a phase row counts the agents it dispatched";
    const PENDING_IS_LISTED: &str = "a declared phase the run has not reached is listed";
    const EARLY_AGENT: &str = "scout";
    const LATE_AGENT: &str = "writer";
    const EARLY_KEY: u64 = 1;
    const LATE_KEY: u64 = 2;
    const RUNNING_TOOL: &str = "shell";
    const TOOL_SUMMARY: &str = "cargo nextest run";
    const TOOLS_RUN: u32 = 3;
    const TOOLS_TALLY: &str = "3 tools";
    const ACTIVITY_IS_TALLIED: &str = "a running agent counts the tools it has called";
    const ACTIVITY_IS_DROPPED: &str = "activity does not outlive the agent it described";
    const GROUPS_ARE_HEADED: &str = "a group of agents is headed by its phase";
    const GROUPS_FOLLOW_THE_PLAN: &str = "phase groups follow the order the run declares";
    const PHASE_OPENS_ITS_GROUP: &str = "opening a phase moves the cursor to the agents it opened";

    pub(crate) fn run(
        run_id: &str,
        status: RunStatus,
        roster: Vec<AgentRosterEntry>,
    ) -> RunSnapshot {
        RunSnapshot {
            run_id: run_id.into(),
            display_name: format!("deep-research-{run_id}"),
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
            phase_history: Vec::new(),
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

    fn detail(run: RunSnapshot) -> RunDetail {
        RunDetail {
            run,
            calls: vec![RunCall {
                call_key: 1,
                kind: CallKind::Agent,
                state: CallState::Completed,
                label: Some("researcher".into()),
                prompt: Some(PROMPT_PREVIEW.into()),
                task_id: Some(TASK_ID.into()),
                tokens_used: 10,
                duration_ms: 1_000,
                started_at: 0,
                finished_at: Some(1),
                result_preview: Some("found it".into()),
                error: None,
            }],
            events: Vec::new(),
            journal_trimmed: false,
        }
    }

    fn history() -> Vec<RunHistoryEntry> {
        vec![RunHistoryEntry {
            run: run(OLD_RUN_ID, RunStatus::Completed, Vec::new()),
            session_id: "session-old".into(),
            session_title: SESSION_TITLE.into(),
        }]
    }

    fn open_with(runs: Vec<RunSnapshot>) -> WorkflowInspector {
        let mut inspector = WorkflowInspector::new();
        let _ = inspector.open(runs, None);
        inspector
    }

    fn task_id_of(index: u64) -> String {
        format!("{RUN_ID}:{index}")
    }

    fn roster(count: u64) -> Vec<AgentRosterEntry> {
        (0..count)
            .map(|index| AgentRosterEntry {
                call_key: index,
                task_id: Some(task_id_of(index)),
                ..agent(None)
            })
            .collect()
    }

    fn mouse_at(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }
    }

    fn draw(inspector: &mut WorkflowInspector, terminal: &mut Terminal<TestBackend>) {
        terminal
            .draw(|frame| {
                inspector.view(frame, frame.area());
            })
            .unwrap();
    }

    fn terminal() -> Terminal<TestBackend> {
        Terminal::new(TestBackend::new(FRAME_WIDTH, FRAME_HEIGHT)).unwrap()
    }

    fn reversed_at(terminal: &Terminal<TestBackend>, row: u16) -> bool {
        let buffer = terminal.backend().buffer();
        (0..FRAME_WIDTH).any(|column| {
            buffer[(column, row)]
                .style()
                .add_modifier
                .contains(Modifier::REVERSED)
        })
    }

    /// The screen row a run's list row was last drawn on.
    fn screen_row_of(inspector: &WorkflowInspector, run_id: &str) -> u16 {
        let row = inspector
            .list_rows
            .iter()
            .find(|(_, id)| id == run_id)
            .map(|(row, _)| *row)
            .expect(NO_SUCH_ROW);
        inspector.list_area.y + row - inspector.list_offset
    }

    #[test]
    fn a_hovered_run_row_marks_itself_without_selecting() {
        let mut terminal = terminal();
        let mut inspector = open_with(vec![
            run(RUN_ID, RunStatus::Active, Vec::new()),
            run(OTHER_RUN_ID, RunStatus::Completed, Vec::new()),
        ]);
        draw(&mut inspector, &mut terminal);
        let row = screen_row_of(&inspector, OTHER_RUN_ID);
        assert!(!reversed_at(&terminal, row), "{ROW_STARTS_PLAIN}");

        let action =
            inspector.handle_mouse(mouse_at(MouseEventKind::Moved, inspector.list_area.x, row));
        draw(&mut inspector, &mut terminal);

        assert_eq!(action, InspectorAction::Consumed, "{HOVER_IS_NOT_A_CLICK}");
        assert_eq!(inspector.selected(), Some(RUN_ID), "{HOVER_IS_NOT_A_CLICK}");
        assert!(reversed_at(&terminal, row), "{ROW_MARKS_THE_POINTER}");

        let _ = inspector.handle_mouse(mouse_at(
            MouseEventKind::Moved,
            inspector.body_area.x,
            inspector.body_area.y,
        ));
        draw(&mut inspector, &mut terminal);

        assert!(!reversed_at(&terminal, row), "{ROW_RELEASES_THE_POINTER}");
    }

    #[test]
    fn a_hovered_section_tab_marks_itself_without_switching() {
        let mut terminal = terminal();
        let mut inspector = open_with(vec![run(RUN_ID, RunStatus::Active, Vec::new())]);
        draw(&mut inspector, &mut terminal);
        let (hit, section) = *inspector
            .tab_hits
            .iter()
            .find(|(_, section)| *section == Section::Calls)
            .expect(NO_SUCH_ROW);

        let _ = inspector.handle_mouse(mouse_at(MouseEventKind::Moved, hit.x, hit.y));
        draw(&mut inspector, &mut terminal);

        assert_eq!(
            inspector.section(),
            Section::Overview,
            "{HOVER_IS_NOT_A_CLICK}"
        );
        assert!(reversed_at(&terminal, hit.y), "{TAB_MARKS_THE_POINTER}");

        let _ = inspector.handle_mouse(mouse_at(
            MouseEventKind::Down(MouseButton::Left),
            hit.x,
            hit.y,
        ));

        assert_eq!(inspector.section(), section, "{CLICK_STILL_SWITCHES}");
    }

    /// The second row, so a cursor that did not follow the pointer would sit
    /// on the first one and the press would only move it.
    #[test]
    fn a_hovered_agent_row_opens_on_one_click() {
        let mut terminal = terminal();
        let mut inspector = open_with(vec![run(RUN_ID, RunStatus::Active, roster(2))]);
        let _ = inspector.handle_key(key_event(KeyCode::Char('3')));
        draw(&mut inspector, &mut terminal);
        let (start, _) = inspector.item_rows[1];
        let column = inspector.body_area.x;
        let row = inspector.body_area.y + start - inspector.scroll.offset();

        let _ = inspector.handle_mouse(mouse_at(MouseEventKind::Moved, column, row));
        let action = inspector.handle_mouse(mouse_at(
            MouseEventKind::Down(MouseButton::Left),
            column,
            row,
        ));

        match action {
            InspectorAction::OpenTranscript(task_id) => assert_eq!(task_id, task_id_of(1)),
            action => panic!("{ONE_CLICK_OPENS}: {action:?}"),
        }
    }

    #[test]
    fn a_pointer_driven_cursor_does_not_scroll_the_body() {
        let mut terminal = terminal();
        let mut inspector = open_with(vec![run(RUN_ID, RunStatus::Active, roster(TALL_ROSTER))]);
        let _ = inspector.handle_key(key_event(KeyCode::Char('3')));
        draw(&mut inspector, &mut terminal);

        let _ = inspector.handle_mouse(mouse_at(
            MouseEventKind::Moved,
            inspector.body_area.x,
            inspector.body_area.y,
        ));
        draw(&mut inspector, &mut terminal);

        assert_eq!(inspector.cursor, 0, "{HOVER_PLACES_THE_CURSOR}");

        let before = inspector.scroll.offset();
        inspector.cursor = TALL_ROSTER as usize - 1;
        draw(&mut inspector, &mut terminal);

        assert_eq!(
            inspector.scroll.offset(),
            before,
            "{POINTER_DOES_NOT_SCROLL}"
        );

        let _ = inspector.handle_key(key_event(KeyCode::Down));
        draw(&mut inspector, &mut terminal);

        assert!(inspector.scroll.offset() > before, "{KEYS_STILL_SCROLL}");
    }

    #[test_case(None, RUN_ID ; "the_newest_by_default")]
    #[test_case(Some(OTHER_RUN_ID), OTHER_RUN_ID ; "the_preferred_run_when_listed")]
    #[test_case(Some(OLD_RUN_ID), RUN_ID ; "the_newest_when_the_preferred_is_not_listed")]
    fn opening_selects_a_run_and_asks_for_it(preferred: Option<&str>, expected: &str) {
        let mut inspector = WorkflowInspector::new();

        let first = inspector.open(
            vec![
                run(RUN_ID, RunStatus::Active, Vec::new()),
                run(OTHER_RUN_ID, RunStatus::Completed, Vec::new()),
            ],
            preferred,
        );

        assert_eq!(first.as_deref(), Some(expected));
        assert_eq!(inspector.selected(), Some(expected));
    }

    #[test_case(RunStatus::Active, PAUSE_KEY, Ok(RunControl::Pause) ; "pause_active")]
    #[test_case(RunStatus::Active, STOP_KEY, Ok(RunControl::Stop) ; "stop_active")]
    #[test_case(RunStatus::Paused, RESUME_KEY, Ok(RunControl::Resume) ; "resume_paused")]
    #[test_case(RunStatus::Completed, PAUSE_KEY, Err(PAUSE_INERT) ; "pause_completed_says_why")]
    #[test_case(RunStatus::Active, RESUME_KEY, Err(RESUME_INERT) ; "resume_active_says_why")]
    #[test_case(RunStatus::Completed, STOP_KEY, Err(STOP_INERT) ; "stop_completed_says_why")]
    fn control_keys_follow_the_run_status(
        status: RunStatus,
        key: char,
        expected: Result<RunControl, &str>,
    ) {
        let mut inspector = open_with(vec![run(RUN_ID, status, Vec::new())]);
        match (
            inspector.handle_key(key_event(KeyCode::Char(key))),
            expected,
        ) {
            (InspectorAction::Control { control, run_id }, Ok(expected)) => {
                assert_eq!(control, expected);
                assert_eq!(run_id, RUN_ID, "{WRONG_RUN}");
            }
            (InspectorAction::Flash(reason), Err(expected)) => assert_eq!(reason, expected),
            (action, _) => panic!("{INERT_KEY}: {action:?}"),
        }
    }

    fn budget_limited(admitted: u32) -> RunSnapshot {
        let mut limited = run(RUN_ID, RunStatus::BudgetLimited, Vec::new());
        limited.usage = RunUsage {
            agents_admitted: admitted,
            tokens_used: 0,
        };
        limited
    }

    #[test]
    fn resuming_a_budget_limited_run_asks_for_a_higher_budget() {
        let mut inspector = open_with(vec![budget_limited(ADMITTED)]);

        let asked = inspector.handle_key(key_event(KeyCode::Char(RESUME_KEY)));
        let prompt = inspector.budget.as_ref().map(|prompt| prompt.input.value());

        assert_eq!(asked, InspectorAction::Consumed);
        assert_eq!(
            prompt.as_deref(),
            Some(SUGGESTED_BUDGET),
            "{SUGGESTS_A_STEP_UP}"
        );
        assert_eq!(
            inspector.handle_key(key_event(KeyCode::Enter)),
            InspectorAction::ResumeWithBudget {
                run_id: RUN_ID.into(),
                agent_budget: ADMITTED + BUDGET_STEP,
            }
        );
        assert!(inspector.budget.is_none());
    }

    #[test]
    fn a_budget_the_run_already_spent_is_refused() {
        let mut inspector = open_with(vec![budget_limited(ADMITTED)]);
        let _ = inspector.handle_key(key_event(KeyCode::Char(RESUME_KEY)));
        for _ in 0..SUGGESTED_BUDGET.len() {
            let _ = inspector.handle_key(key_event(KeyCode::Backspace));
        }
        let _ = inspector.handle_key(key_event(KeyCode::Char('1')));

        let action = inspector.handle_key(key_event(KeyCode::Enter));

        assert_eq!(action, InspectorAction::Flash(BUDGET_INVALID));
        assert!(inspector.budget.is_some(), "{PROMPT_STANDS}");
    }

    #[test]
    fn escape_closes_the_budget_prompt_and_leaves_the_inspector_open() {
        let mut inspector = open_with(vec![budget_limited(ADMITTED)]);
        let _ = inspector.handle_key(key_event(KeyCode::Char(RESUME_KEY)));

        let action = inspector.handle_key(key_event(KeyCode::Esc));

        assert_eq!(action, InspectorAction::Consumed);
        assert!(inspector.budget.is_none());
        assert!(inspector.is_open(), "{PROMPT_IS_NOT_THE_MODAL}");
    }

    #[test]
    fn a_run_that_spent_the_maximum_budget_has_nothing_left_to_raise() {
        let mut inspector = open_with(vec![budget_limited(MAX_AGENT_BUDGET)]);

        let action = inspector.handle_key(key_event(KeyCode::Char(RESUME_KEY)));

        assert_eq!(action, InspectorAction::Flash(BUDGET_MAXED));
        assert!(inspector.budget.is_none());
    }

    /// The section as a reader sees it, which is what `y` hands over.
    fn section_text(inspector: &mut WorkflowInspector, section: char) -> String {
        let _ = inspector.handle_key(key_event(KeyCode::Char(section)));
        match inspector.handle_key(key_event(KeyCode::Char(COPY_KEY))) {
            InspectorAction::Copy { text, .. } => text,
            action => panic!("{action:?}"),
        }
    }

    #[test]
    fn a_run_row_carries_its_clock() {
        let mut walking = run(RUN_ID, RunStatus::Active, Vec::new());
        walking.display_name = SHORT_NAME.into();
        walking.phase = Some(PHASE_ONE.into());
        let entry = Entry {
            group: Group::Running,
            run: &walking,
            session_title: None,
        };

        let row = list_row(&entry, Style::default(), "", ELAPSED_SECS, LIST_MAX_WIDTH);

        let text: String = row.spans.iter().map(|span| span.content.as_ref()).collect();
        assert!(text.contains(PHASE_ONE), "{text}");
        assert!(text.ends_with(ELAPSED_TEXT), "{CLOCK_IS_LAST}: {text}");
    }

    #[test]
    fn the_phases_section_lists_what_is_left_and_what_each_phase_dispatched() {
        let mut walking = run(RUN_ID, RunStatus::Active, vec![agent(None)]);
        walking.phases = vec![PHASE_ONE.into(), PHASE_TWO.into()];
        walking.phase = Some(PHASE_ONE.into());
        walking.phase_history = vec![PhaseRecord {
            title: PHASE_ONE.into(),
            started_at: 0,
        }];
        walking.roster[0].phase = Some(PHASE_ONE.into());
        walking.roster[0].state = RosterState::Completed;
        let mut inspector = open_with(vec![walking]);

        let text = section_text(&mut inspector, '2');

        assert!(text.contains(PHASE_ONE), "{text}");
        assert!(
            text.contains(ONE_OF_ONE_AGENT),
            "{PHASE_COUNTS_ITS_OWN}: {text}"
        );
        assert!(
            text.contains(&format!("{} {PHASE_TWO}", PhaseMark::Pending.glyph())),
            "{PENDING_IS_LISTED}: {text}"
        );
    }

    fn phased(call_key: u64, label: &str, phase: &str) -> AgentRosterEntry {
        AgentRosterEntry {
            call_key,
            label: label.into(),
            phase: Some(phase.into()),
            ..agent(None)
        }
    }

    /// A run whose roster is stored out of phase order, so display order has
    /// to be the grouping and not the storage.
    fn fanned_out() -> RunSnapshot {
        let mut walking = run(
            RUN_ID,
            RunStatus::Active,
            vec![
                phased(LATE_KEY, LATE_AGENT, PHASE_TWO),
                phased(EARLY_KEY, EARLY_AGENT, PHASE_ONE),
            ],
        );
        walking.phases = vec![PHASE_ONE.into(), PHASE_TWO.into()];
        walking
    }

    /// Puts the cursor on the second phase row.
    fn walk_to_second_phase(inspector: &mut WorkflowInspector) {
        let _ = inspector.handle_key(key_event(KeyCode::Char('2')));
        let _ = inspector.handle_key(key_event(KeyCode::Right));
        let _ = inspector.handle_key(key_event(KeyCode::Down));
    }

    #[test]
    fn agents_gather_under_the_phase_that_dispatched_them() {
        let mut inspector = open_with(vec![fanned_out()]);

        let text = section_text(&mut inspector, '3');

        let early = text.find(EARLY_AGENT).expect(&text);
        let late = text.find(LATE_AGENT).expect(&text);
        let header = text.find(PHASE_ONE).expect(&text);
        assert!(header < early, "{GROUPS_ARE_HEADED}: {text}");
        assert!(early < late, "{GROUPS_FOLLOW_THE_PLAN}: {text}");
    }

    #[test]
    fn opening_a_phase_row_lands_on_its_first_agent() {
        let mut inspector = open_with(vec![fanned_out()]);
        walk_to_second_phase(&mut inspector);

        let action = inspector.handle_key(key_event(KeyCode::Enter));

        assert_eq!(action, InspectorAction::Consumed);
        assert_eq!(inspector.section, Section::Agents);
        assert_eq!(inspector.cursor, 1, "{PHASE_OPENS_ITS_GROUP}");
    }

    #[test]
    fn a_phase_that_dispatched_nothing_says_so() {
        let mut walking = fanned_out();
        walking.roster.remove(0);
        let mut inspector = open_with(vec![walking]);
        walk_to_second_phase(&mut inspector);

        let action = inspector.handle_key(key_event(KeyCode::Enter));

        assert_eq!(action, InspectorAction::Flash(NO_AGENTS_IN_PHASE));
        assert_eq!(inspector.section, Section::Phases);
    }

    fn tool_progress() -> SubagentProgress {
        SubagentProgress {
            activity: SubagentActivity::tool(Arc::from(RUNNING_TOOL), TOOL_SUMMARY),
            tools: TOOLS_RUN,
            elapsed: Duration::ZERO,
        }
    }

    #[test]
    fn a_running_agent_says_what_it_is_doing() {
        let mut inspector = open_with(vec![fanned_out()]);
        inspector.set_progress(RUN_ID, EARLY_KEY, tool_progress());

        let text = section_text(&mut inspector, '3');

        assert!(text.contains(RUNNING_TOOL), "{text}");
        assert!(text.contains(TOOL_SUMMARY), "{text}");
        assert!(text.contains(TOOLS_TALLY), "{ACTIVITY_IS_TALLIED}: {text}");
    }

    #[test]
    fn an_agent_that_stopped_drops_its_activity() {
        let mut inspector = open_with(vec![fanned_out()]);
        inspector.set_progress(RUN_ID, EARLY_KEY, tool_progress());
        let mut settled = fanned_out();
        for agent in &mut settled.roster {
            agent.state = RosterState::Completed;
        }

        let _ = inspector.refresh(vec![settled]);
        let text = section_text(&mut inspector, '3');

        assert!(
            !text.contains(RUNNING_TOOL),
            "{ACTIVITY_IS_DROPPED}: {text}"
        );
        assert!(inspector.live.is_empty(), "{ACTIVITY_IS_DROPPED}");
    }

    #[test]
    fn the_overview_counts_the_roster_beside_the_budget() {
        let mut inspector = open_with(vec![run(RUN_ID, RunStatus::Active, vec![agent(None)])]);

        let text = section_text(&mut inspector, '1');

        assert!(text.contains(ROSTER_TALLY), "{text}");
    }

    #[test]
    fn the_overview_tells_a_stalled_run_what_to_do() {
        let mut inspector = open_with(vec![budget_limited(ADMITTED)]);

        match inspector.handle_key(key_event(KeyCode::Char(COPY_KEY))) {
            InspectorAction::Copy { text, .. } => {
                assert!(text.contains(BUDGET_LIMITED_HINT), "{text}");
            }
            action => panic!("{action:?}"),
        }
    }

    #[test]
    fn a_control_on_an_earlier_sessions_run_is_refused() {
        let mut inspector = open_with(Vec::new());
        assert_eq!(
            inspector.fill_history(history()).as_deref(),
            Some(OLD_RUN_ID)
        );

        let action = inspector.handle_key(key_event(KeyCode::Char(RESUME_KEY)));

        assert_eq!(action, InspectorAction::Flash(FOREIGN_RUN));
    }

    #[test]
    fn enter_on_an_agent_row_opens_its_transcript() {
        let mut inspector = open_with(vec![run(
            RUN_ID,
            RunStatus::Active,
            vec![agent(Some(TASK_ID))],
        )]);
        let _ = inspector.handle_key(key_event(KeyCode::Char('3')));
        assert_eq!(inspector.section(), Section::Agents);

        match inspector.handle_key(key_event(KeyCode::Enter)) {
            InspectorAction::OpenTranscript(task_id) => assert_eq!(task_id, TASK_ID),
            action => panic!("{TRANSCRIPT}: {action:?}"),
        }
    }

    #[test]
    fn enter_on_an_agent_without_a_transcript_says_so() {
        let mut inspector = open_with(vec![run(RUN_ID, RunStatus::Active, vec![agent(None)])]);
        let _ = inspector.handle_key(key_event(KeyCode::Char('3')));

        assert_eq!(
            inspector.handle_key(key_event(KeyCode::Enter)),
            InspectorAction::Flash(NO_TRANSCRIPT)
        );
    }

    fn scratch_call() -> RunCall {
        RunCall {
            call_key: 2,
            kind: CallKind::ScratchFile,
            state: CallState::Completed,
            label: Some("report.md".into()),
            prompt: None,
            task_id: None,
            tokens_used: 0,
            duration_ms: 0,
            started_at: 1,
            finished_at: Some(1),
            result_preview: Some(SCRATCH_PATH.into()),
            error: None,
        }
    }

    #[test]
    fn enter_on_the_result_opens_the_scratch_file() {
        let mut settled = run(RUN_ID, RunStatus::Completed, Vec::new());
        settled.result = Some(serde_json::json!({ "report": "done", "path": SCRATCH_PATH }));
        let mut inspector = open_with(vec![settled]);
        let _ = inspector.handle_key(key_event(KeyCode::Char('6')));

        let action = inspector.handle_key(key_event(KeyCode::Enter));

        assert_eq!(
            action,
            InspectorAction::OpenFile(PathBuf::from(SCRATCH_PATH)),
            "{OPENS_SCRATCH}"
        );
    }

    #[test]
    fn enter_on_a_result_without_a_scratch_file_does_nothing() {
        let mut inspector = open_with(vec![run(RUN_ID, RunStatus::Completed, Vec::new())]);
        let _ = inspector.handle_key(key_event(KeyCode::Char('6')));

        let action = inspector.handle_key(key_event(KeyCode::Enter));

        assert_eq!(action, InspectorAction::Consumed, "{NO_SCRATCH}");
    }

    #[test]
    fn enter_on_a_scratch_call_opens_its_file() {
        let mut inspector = open_with(vec![run(RUN_ID, RunStatus::Completed, Vec::new())]);
        let mut with_scratch = detail(run(RUN_ID, RunStatus::Completed, Vec::new()));
        with_scratch.calls.push(scratch_call());
        inspector.fill_detail(with_scratch);
        let _ = inspector.handle_key(key_event(KeyCode::Char('4')));
        let _ = inspector.handle_key(key_event(KeyCode::Right));
        let _ = inspector.handle_key(key_event(KeyCode::Down));

        let action = inspector.handle_key(key_event(KeyCode::Enter));

        assert_eq!(
            action,
            InspectorAction::OpenFile(PathBuf::from(SCRATCH_PATH)),
            "{OPENS_SCRATCH}"
        );
        assert_eq!(inspector.expanded_call, None, "{OPENS_SCRATCH}");
    }

    #[test]
    fn enter_on_a_call_toggles_its_preview() {
        let mut inspector = open_with(vec![run(RUN_ID, RunStatus::Active, Vec::new())]);
        inspector.fill_detail(detail(run(RUN_ID, RunStatus::Active, Vec::new())));
        let _ = inspector.handle_key(key_event(KeyCode::Char('4')));

        let _ = inspector.handle_key(key_event(KeyCode::Enter));
        assert_eq!(inspector.expanded_call, Some(1));
        let _ = inspector.handle_key(key_event(KeyCode::Enter));
        assert_eq!(inspector.expanded_call, None);
    }

    #[test]
    fn an_opened_call_asks_for_its_body_once() {
        let mut inspector = open_with(vec![run(RUN_ID, RunStatus::Active, Vec::new())]);
        inspector.fill_detail(detail(run(RUN_ID, RunStatus::Active, Vec::new())));
        let _ = inspector.handle_key(key_event(KeyCode::Char('4')));

        let asked = inspector.handle_key(key_event(KeyCode::Enter));

        assert_eq!(
            asked,
            InspectorAction::LoadCallBody {
                run_id: RUN_ID.into(),
                call_key: 1,
            },
            "{ASKS_ONCE}"
        );
        let _ = inspector.handle_key(key_event(KeyCode::Enter));
        assert_eq!(
            inspector.handle_key(key_event(KeyCode::Enter)),
            InspectorAction::Consumed,
            "{ASKS_ONCE}"
        );
    }

    #[test]
    fn a_landed_body_replaces_the_preview() {
        let mut inspector = open_with(vec![run(RUN_ID, RunStatus::Active, Vec::new())]);
        inspector.fill_detail(detail(run(RUN_ID, RunStatus::Active, Vec::new())));
        let _ = inspector.handle_key(key_event(KeyCode::Char('4')));
        let _ = inspector.handle_key(key_event(KeyCode::Enter));

        inspector.fill_call_bodies(RUN_ID, Some(1), vec![body(1)]);

        let text = section_text(&mut inspector, '4');
        assert!(text.contains(FULL_PROMPT), "{BODY_WINS}");
        assert!(text.contains(FULL_RESULT), "{BODY_WINS}");
        assert!(!text.contains(PROMPT_PREVIEW), "{BODY_WINS}");
    }

    #[test]
    fn a_body_for_another_run_is_dropped() {
        let mut inspector = open_with(vec![run(RUN_ID, RunStatus::Active, Vec::new())]);
        inspector.fill_detail(detail(run(RUN_ID, RunStatus::Active, Vec::new())));
        let _ = inspector.handle_key(key_event(KeyCode::Char('4')));
        let _ = inspector.handle_key(key_event(KeyCode::Enter));

        inspector.fill_call_bodies(OTHER_RUN_ID, Some(1), vec![body(1)]);

        assert!(
            !section_text(&mut inspector, '4').contains(FULL_RESULT),
            "{STALE_BODY}"
        );
    }

    #[test]
    fn a_call_the_journal_lost_says_so_instead_of_loading() {
        let mut inspector = open_with(vec![run(RUN_ID, RunStatus::Active, Vec::new())]);
        let mut bare = detail(run(RUN_ID, RunStatus::Active, Vec::new()));
        bare.calls[0].prompt = None;
        bare.calls[0].result_preview = None;
        inspector.fill_detail(bare);
        let _ = inspector.handle_key(key_event(KeyCode::Char('4')));
        let _ = inspector.handle_key(key_event(KeyCode::Enter));

        inspector.fill_call_bodies(RUN_ID, Some(1), Vec::new());

        assert!(section_text(&mut inspector, '4').contains(BODY_MISSING));
    }

    fn body(call_key: u64) -> RunCallBody {
        RunCallBody {
            call_key,
            request: FULL_PROMPT.into(),
            result: Some(FULL_RESULT.into()),
            error: None,
        }
    }

    #[test]
    fn tab_cycles_the_sections_both_ways() {
        let mut inspector = open_with(vec![run(RUN_ID, RunStatus::Active, Vec::new())]);

        let _ = inspector.handle_key(key_event(KeyCode::Tab));
        assert_eq!(inspector.section(), Section::Phases);
        let _ = inspector.handle_key(key_event(KeyCode::BackTab));
        let _ = inspector.handle_key(key_event(KeyCode::BackTab));
        assert_eq!(inspector.section(), Section::Result);
    }

    #[test]
    fn moving_the_selection_asks_for_the_new_run() {
        let mut inspector = open_with(vec![
            run(RUN_ID, RunStatus::Active, Vec::new()),
            run(OTHER_RUN_ID, RunStatus::Completed, Vec::new()),
        ]);

        let action = inspector.handle_key(key_event(KeyCode::Down));

        assert_eq!(action, InspectorAction::Inspect(OTHER_RUN_ID.into()));
        assert_eq!(inspector.selected(), Some(OTHER_RUN_ID));
    }

    #[test]
    fn a_detail_for_another_run_is_dropped() {
        let mut inspector = open_with(vec![run(RUN_ID, RunStatus::Active, Vec::new())]);

        inspector.fill_detail(detail(run(OTHER_RUN_ID, RunStatus::Active, Vec::new())));

        assert!(inspector.detail.is_none(), "{STALE_DETAIL}");
    }

    #[test]
    fn a_refresh_that_moves_the_selected_run_asks_again() {
        let mut inspector = open_with(vec![run(RUN_ID, RunStatus::Active, Vec::new())]);
        inspector.fill_detail(detail(run(RUN_ID, RunStatus::Active, Vec::new())));
        let mut moved = run(RUN_ID, RunStatus::Paused, Vec::new());
        moved.revision = 2;

        let again = inspector.refresh(vec![moved]);

        assert_eq!(again.as_deref(), Some(RUN_ID), "{REINSPECT}");
        assert_eq!(
            inspector.detail.as_ref().map(|detail| detail.run.status),
            Some(RunStatus::Paused)
        );
    }

    #[test]
    fn a_refresh_keeps_the_selection_on_its_run() {
        let mut inspector = open_with(vec![
            run(RUN_ID, RunStatus::Active, Vec::new()),
            run(OTHER_RUN_ID, RunStatus::Completed, Vec::new()),
        ]);
        let _ = inspector.handle_key(key_event(KeyCode::Down));

        let _ = inspector.refresh(vec![
            run("run-3", RunStatus::Active, Vec::new()),
            run(RUN_ID, RunStatus::Active, Vec::new()),
            run(OTHER_RUN_ID, RunStatus::Completed, Vec::new()),
        ]);

        assert_eq!(inspector.selected(), Some(OTHER_RUN_ID), "{FOLLOW_RUN}");
    }

    #[test]
    fn the_filter_narrows_the_list_by_session_title() {
        let mut inspector = open_with(vec![run(RUN_ID, RunStatus::Active, Vec::new())]);
        assert!(inspector.fill_history(history()).is_none());
        let _ = inspector.handle_key(key_event(KeyCode::Char(FILTER_KEY)));

        let action = inspector.handle_key(key_event(KeyCode::Char('y')));

        assert_eq!(action, InspectorAction::Inspect(OLD_RUN_ID.into()));
        assert_eq!(inspector.entries().len(), 1);
    }

    #[test]
    fn copy_hands_over_the_visible_section() {
        let mut inspector = open_with(vec![run(RUN_ID, RunStatus::Active, Vec::new())]);

        match inspector.handle_key(key_event(KeyCode::Char(COPY_KEY))) {
            InspectorAction::Copy { text, label } => {
                assert!(text.contains(&format!("deep-research-{RUN_ID}")), "{text}");
                assert_eq!(label, COPIED);
            }
            action => panic!("{action:?}"),
        }
    }

    #[test_case(0 => "0s" ; "zero")]
    #[test_case(134 => "2m14s" ; "minutes")]
    #[test_case(3_780 => "1h03m" ; "hours")]
    fn elapsed_reads_at_card_width(seconds: u64) -> String {
        format_elapsed(seconds)
    }

    #[test_case(999 => "999" ; "units")]
    #[test_case(41_250 => "41k" ; "thousands")]
    #[test_case(1_250_000 => "1.2M" ; "millions")]
    fn counts_compact(value: u64) -> String {
        format_compact(value)
    }
}
