//! The `/workflow` inspector: every run the session knows on the left, and
//! the selected run's overview, phases, agents, calls, logs and result on
//! the right. Runs of earlier sessions are listed to read, not to control.
//!
//! The inspector holds no run state of its own beyond what it was given: the
//! app re-supplies the runtime's read model on every change, asks the
//! runtime for a run's detail when the selection or the run moves, and
//! every control names the run it acts on.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

mod json;
mod timeline;

use caudra_agent::SubagentProgress;
use caudra_agent::types::{PhaseMark, WorkflowRunCard};
use caudra_workflow::{
    AgentRosterEntry, CallKind, CallState, MAX_AGENT_BUDGET, RosterState, RunCall, RunCallBody,
    RunDetail, RunHistoryEntry, RunSnapshot, RunStatus,
};
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::animation::{animation_elapsed_ms, spinner_str};
use crate::components::modal::{FooterHits, FooterLine, Modal};
use crate::components::scrollbar::{ScrollHint, Scrollbar, ScrollbarMouse};
use crate::components::tool_display::{
    TREE_BRANCH, TREE_LAST, activity_child_spans, activity_detail, activity_label, activity_sigil,
};
use crate::components::workflow_card::{
    AGENTS_SUFFIX, TOKENS_SUFFIX, phase_strip_line, status_span,
};
use crate::components::workflow_inspector::json::JsonRow;
use crate::components::workflow_inspector::timeline::{TimelineRow, span_bar, timeline};
use crate::components::{
    ModalScroll, Overlay, ToolProgress, escape_terminal_controls, format_compact, format_elapsed,
    format_integer, hover_style, input_line_with_cursor, now_secs, visual_rows,
};
use crate::markdown::text_to_painted;
use crate::repaint::Cadence;
use crate::text_buffer::TextBuffer;
use crate::theme;

const TITLE: &str = " Workflows ";
const WIDTH_PERCENT: u16 = 90;
const MAX_HEIGHT_PERCENT: u16 = 85;
const LIST_MAX_WIDTH: u16 = 34;
const LIST_PERCENT: u16 = 35;
const PANE_GAP: u16 = 1;
/// A run list narrower than this shows a name with nowhere to put its clock,
/// and a detail pane narrower than this shows neither the tab strip nor the
/// timeline grid, so a modal too narrow for both panes shows one at a time.
const LIST_MIN_COLS: u16 = 24;
const DETAIL_MIN_COLS: u16 = 52;
const SPLIT_MIN_COLS: u16 = LIST_MIN_COLS + PANE_GAP + DETAIL_MIN_COLS;
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
const NO_TIMELINE: &str = "This run recorded nothing";
const NEST_INDENT: &str = "  ";
const FAILED_UNIT: &str = " failed";
/// A timeline row is a grid: the clock, the glyph, the label, the duration
/// and the bar sit in the same columns on every row, so a reader scans one
/// column down the run rather than hunting along each line in turn.
const MARK_COLS: usize = 2;
const ELAPSED_COLS: usize = 7;
const CLOCK_COLS: usize = ELAPSED_COLS + 1;
/// A glyph and the space after it, and the blank a row without one stands in
/// its place, so nothing after the glyph column moves.
const GLYPH_PAD: &str = "  ";
const DURATION_COLS: usize = 6;
const COLUMN_GAP: &str = "  ";
const LABEL_MIN_COLS: usize = 12;
const LABEL_MAX_COLS: usize = 32;
/// What a row keeps clear for the tallies that follow its bar.
const TALLY_COLS: usize = 20;
const BAR_MAX_WIDTH: usize = 24;
const ELLIPSIS: char = '\u{2026}';
const CALL_RUNNING_GLYPH: &str = "\u{25b8}";
const CALL_DONE_GLYPH: &str = "\u{2713}";
const CALL_FAILED_GLYPH: &str = "\u{2717}";
const NO_AGENTS: &str = "No agents yet";
const NO_AGENTS_IN_PHASE: &str = "This phase dispatched no agents";
const UNPHASED_GROUP: &str = "No phase";
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
const NO_SCRIPT: &str = "A builtin workflow is compiled in and has no file to open";
const NOTHING_TO_EXPORT: &str = "This run has no journal to export yet";
const EXPORTED: &str = "Copied the run as markdown";
const COPIED: &str = "Copied section";
pub(crate) const PAUSE_LABEL: &str = "p";
pub(crate) const RESUME_LABEL: &str = "r";
pub(crate) const STOP_LABEL: &str = "s";
pub(crate) const TRANSCRIPT_LABEL: &str = "t";
pub(crate) const SCRIPT_LABEL: &str = "o";
pub(crate) const EXPORT_LABEL: &str = "e";
pub(crate) const COPY_LABEL: &str = "y";
pub(crate) const FILTER_LABEL: &str = "/";
const PAUSE_KEY: char = ascii_key(PAUSE_LABEL);
const RESUME_KEY: char = ascii_key(RESUME_LABEL);
const STOP_KEY: char = ascii_key(STOP_LABEL);
const TRANSCRIPT_KEY: char = ascii_key(TRANSCRIPT_LABEL);
const SCRIPT_KEY: char = ascii_key(SCRIPT_LABEL);
const EXPORT_KEY: char = ascii_key(EXPORT_LABEL);
const COPY_KEY: char = ascii_key(COPY_LABEL);
const FILTER_KEY: char = ascii_key(FILTER_LABEL);
const SECTION_GAP: &str = "  ";
/// What the footer puts between its keys once it has given up their words.
const KEY_GAP: &str = " ";
const GROUP_RUNNING: &str = "Running";
const GROUP_WAITING: &str = "Waiting";
const GROUP_FINISHED: &str = "Finished";
const GROUP_EARLIER: &str = "Earlier sessions";
const CURSOR_MARK: &str = "\u{203a} ";
const NO_MARK: &str = "  ";
const SEPARATOR: &str = " \u{b7} ";
const EXPAND_INDENT: &str = "      ";
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
const DONE_UNIT: &str = " done";
const AGENT_SLASH: &str = "/";
const FOOTER: [(&str, &str, FooterCommand); 10] = [
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
    (TRANSCRIPT_LABEL, "Transcript", FooterCommand::Transcript),
    (SCRIPT_LABEL, "Script", FooterCommand::Script),
    (EXPORT_LABEL, "Export", FooterCommand::Export),
    (COPY_LABEL, "Copy", FooterCommand::Copy),
    (FILTER_LABEL, "Filter", FooterCommand::Filter),
    ("Esc", "Close", FooterCommand::Close),
];
/// How the footer draws itself, widest first: glossed, then keys alone, then
/// keys packed. Every key is on every rung, because a key a reader cannot see
/// is a key they cannot press.
const FOOTER_RUNGS: [(bool, &str); 3] =
    [(true, SECTION_GAP), (false, SECTION_GAP), (false, KEY_GAP)];
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
    Transcript,
    Script,
    Export,
    Copy,
    Filter,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Section {
    Overview,
    Timeline,
    Agents,
    Result,
}

impl Section {
    const ALL: [Self; 4] = [Self::Overview, Self::Timeline, Self::Agents, Self::Result];

    fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Timeline => "Timeline",
            Self::Agents => "Agents",
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

    /// The timeline follows its tail, because the newest row is the one a
    /// reader watching a live run wants. Everything else opens at the top.
    fn scroll(self) -> ModalScroll {
        match self {
            Self::Timeline => ModalScroll::new(),
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
    /// A call's body is wanted and not loaded yet, or the whole run's when
    /// `call_key` is absent.
    LoadCallBody {
        run_id: String,
        call_key: Option<u64>,
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
    /// A pending request to put the cursor in view, consumed by the next
    /// draw. Only a key that moves the cursor asks: a cursor the pointer
    /// moved is already under the pointer, and a scroll is an instruction
    /// about the view that the cursor must not undo on the next frame.
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
    /// The JSON nodes a reader has closed, by the body they belong to. Empty
    /// is every node open, which is what a body a reader has not touched
    /// shows. Dropped whole with the selection, like `bodies`.
    folded: HashMap<FoldScope, HashSet<usize>>,
    /// An export is waiting on the bodies it asked for.
    exporting: bool,
}

/// Which body a folded node belongs to. Every part of every call is its own
/// tree, and so is the run's own result, because a node is named by where it
/// sits in the value it came from and two values name their roots alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum FoldScope {
    Prompt(u64),
    Error(u64),
    CallResult(u64),
    Result,
}

/// One part of a call's body: what it is called, what it says, and the tree it
/// makes when what it says is JSON.
struct BodyPart<'a> {
    scope: FoldScope,
    heading: &'static str,
    text: &'a str,
    style: Style,
}

/// What the cursor rests on in a section: one of its own rows, or a node of a
/// JSON body opened beneath one. Both are built from one walk, so the cursor
/// cannot count rows the section did not draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Item {
    Row(usize),
    Fold(FoldScope, usize),
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
            reveal_cursor: false,
            footer: FooterLine::default(),
            footer_hits: FooterHits::default(),
            live: HashMap::new(),
            bodies: HashMap::new(),
            folded: HashMap::new(),
            exporting: false,
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
        self.reveal_cursor = false;
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

    /// Activity outlives the agent that reported it but not the run that
    /// dispatched it: an entry survives as long as the roster still lists its
    /// agent, so a settled agent keeps its last report and a vanished run
    /// takes every one of them with it.
    fn prune_live(&mut self) {
        let runs = &self.runs;
        self.live.retain(|run_id, agents| {
            let Some(run) = runs.iter().find(|run| run.run_id == *run_id) else {
                return false;
            };
            agents.retain(|call_key, _| run.roster.iter().any(|agent| agent.call_key == *call_key));
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
    pub fn fill_call_bodies(
        &mut self,
        run_id: &str,
        asked: Option<u64>,
        bodies: Vec<RunCallBody>,
    ) -> Option<InspectorAction> {
        if self.selected.as_deref() != Some(run_id) {
            return None;
        }
        if let Some(call_key) = asked
            && !bodies.iter().any(|body| body.call_key == call_key)
        {
            self.bodies.insert(call_key, BodyState::Missing);
        }
        for body in bodies {
            self.bodies.insert(body.call_key, BodyState::Loaded(body));
        }
        (asked.is_none() && self.exporting).then(|| self.export_now())
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
        self.reveal_cursor = false;
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
            KeyCode::Char(TRANSCRIPT_KEY) if plain => return self.open_transcript(),
            KeyCode::Char(SCRIPT_KEY) if plain => return self.open_script(),
            KeyCode::Char(EXPORT_KEY) if plain => return self.export(),
            KeyCode::Char(COPY_KEY) if plain => return self.copy(),
            _ => {
                if self.scroll.handle_key(key) {
                    self.reveal_cursor = false;
                }
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
                self.reveal_cursor = false;
                return InspectorAction::Consumed;
            }
        }
        let pos = Position::new(event.column, event.row);
        self.pointer = Some(pos);
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
                if let Some(index) = hit.filter(|_| self.has_items()) {
                    self.pane = Pane::Detail;
                    self.cursor = index;
                }
                return InspectorAction::Consumed;
            }
            self.pane = Pane::Detail;
            match (self.section, hit) {
                (Section::Result, Some(_)) => return self.activate(),
                (Section::Timeline | Section::Agents, Some(index)) if index == self.cursor => {
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
            Some(FooterCommand::Transcript) => self.open_transcript(),
            Some(FooterCommand::Script) => self.open_script(),
            Some(FooterCommand::Export) => self.export(),
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
            Pane::Detail if self.has_items() => {
                let count = self.item_count();
                if count > 0 {
                    self.cursor =
                        (self.cursor as isize + delta).clamp(0, count as isize - 1) as usize;
                    self.reveal_cursor = true;
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
        self.folded.clear();
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
        if let Some(Item::Fold(scope, node)) = self.items().get(self.cursor).copied() {
            return self.toggle_fold(scope, node);
        }
        match self.section {
            Section::Timeline => self.open_timeline_row(),
            Section::Agents => match self.cursor_agent().map(|agent| agent.call_key) {
                Some(call_key) => self.expand_call(call_key),
                None => InspectorAction::Consumed,
            },
            Section::Result => match self.selected_run().and_then(RunSnapshot::scratch_path) {
                Some(path) => InspectorAction::OpenFile(PathBuf::from(path)),
                None => InspectorAction::Consumed,
            },
            _ => InspectorAction::Consumed,
        }
    }

    /// The agent the cursor is on: named directly in the roster, or reached
    /// through the call a timeline row stands for.
    fn cursor_agent(&self) -> Option<&AgentRosterEntry> {
        let run = self.selected_run()?;
        match self.section {
            Section::Agents => {
                let Item::Row(index) = self.items().get(self.cursor).copied()? else {
                    return None;
                };
                run.roster.get(index)
            }
            Section::Timeline => {
                let Item::Row(index) = self.items().get(self.cursor).copied()? else {
                    return None;
                };
                let rows = self.timeline_rows(now_secs());
                let TimelineRow::Call { call, .. } = rows.get(index)? else {
                    return None;
                };
                let call_key = self.detail.as_ref()?.calls.get(*call)?.call_key;
                run.roster.iter().find(|agent| agent.call_key == call_key)
            }
            _ => None,
        }
    }

    fn cursor_task_id(&self) -> Option<&str> {
        self.cursor_agent()?.task_id.as_deref()
    }

    /// The transcript of whatever agent the cursor is on, from either the
    /// timeline or the roster, so a reader never has to change section to
    /// read what an agent actually said.
    fn open_transcript(&mut self) -> InspectorAction {
        match self.cursor_task_id() {
            Some(task_id) => InspectorAction::OpenTranscript(task_id.to_owned()),
            None => InspectorAction::Flash(NO_TRANSCRIPT),
        }
    }

    /// The script the run executed. A builtin is compiled in and has no file
    /// to open, which is a reason rather than a silent refusal.
    fn open_script(&mut self) -> InspectorAction {
        match self
            .selected_run()
            .and_then(|run| run.source_path.as_deref())
        {
            Some(path) => InspectorAction::OpenFile(PathBuf::from(path)),
            None => InspectorAction::Flash(NO_SCRIPT),
        }
    }

    /// The whole run as one markdown artifact. Every call's body is needed,
    /// so the first press asks for them and the copy follows when they land.
    fn export(&mut self) -> InspectorAction {
        let (Some(run_id), Some(detail)) = (self.selected.clone(), self.detail.as_ref()) else {
            return InspectorAction::Flash(NOTHING_TO_EXPORT);
        };
        let loaded = detail
            .calls
            .iter()
            .all(|call| matches!(self.bodies.get(&call.call_key), Some(BodyState::Loaded(_))));
        if loaded {
            return self.export_now();
        }
        self.exporting = true;
        InspectorAction::LoadCallBody {
            run_id,
            call_key: None,
        }
    }

    fn export_now(&mut self) -> InspectorAction {
        self.exporting = false;
        let (Some(run), Some(detail)) = (self.selected_run(), self.detail.as_ref()) else {
            return InspectorAction::Flash(NOTHING_TO_EXPORT);
        };
        InspectorAction::Copy {
            text: self.markdown(run, detail),
            label: EXPORTED,
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
        InspectorAction::LoadCallBody {
            run_id,
            call_key: Some(call_key),
        }
    }

    /// The run as markdown: the overview, the timeline in order, and every
    /// call's full prompt and result. The point is an artifact a reader can
    /// take somewhere else, so nothing here is abbreviated for width.
    fn markdown(&self, run: &RunSnapshot, detail: &RunDetail) -> String {
        let now = now_secs();
        let mut out = format!("# {}\n\n", run.display_name);
        out.push_str(&format!(
            "- workflow: {} ({})\n- status: {}\n- elapsed: {}\n- agents: {} of {} admitted\n- tokens: {}\n",
            run.workflow_name,
            run.source_kind,
            run.status,
            format_elapsed(run.elapsed_secs(now)),
            run.usage.agents_admitted,
            run.agent_budget,
            format_integer(run.usage.tokens_used),
        ));
        if let Some(objective) = &run.objective {
            out.push_str(&format!("- objective: {objective}\n"));
        }
        if let Some(error) = &run.error {
            out.push_str(&format!("- error: {error}\n"));
        }
        out.push_str("\n## Timeline\n\n");
        for row in timeline(run, detail, now) {
            match row {
                TimelineRow::Phase {
                    title,
                    at,
                    end,
                    agents,
                    failed,
                } => out.push_str(&format!(
                    "\n### {title}\n\n- at: {}\n- took: {}\n- agents: {agents} ({failed} failed)\n",
                    offset_text(at.saturating_sub(run.created_at)),
                    format_elapsed(end.saturating_sub(at)),
                )),
                TimelineRow::Pending { title } => {
                    out.push_str(&format!("\n### {title} (not reached)\n"));
                }
                TimelineRow::Call { at, end, call } => {
                    let call = &detail.calls[call];
                    out.push_str(&format!(
                        "\n#### {} ({} {})\n\n- at: {}\n- took: {}\n- tokens: {}\n",
                        call_name(call),
                        call.kind,
                        call.state,
                        offset_text(at.saturating_sub(run.created_at)),
                        format_elapsed(end.saturating_sub(at)),
                        format_integer(call.tokens_used),
                    ));
                    for part in self.body_parts(call) {
                        out.push_str(part.heading);
                        out.push('\n');
                        for line in part.text.lines() {
                            out.push_str(EXPAND_INDENT);
                            out.push_str(line);
                            out.push('\n');
                        }
                    }
                }
                TimelineRow::Log { at, message } => out.push_str(&format!(
                    "- {} {message}\n",
                    offset_text(at.saturating_sub(run.created_at))
                )),
                TimelineRow::Settled { at, status } => out.push_str(&format!(
                    "\n## {status} at {}\n",
                    offset_text(at.saturating_sub(run.created_at))
                )),
            }
        }
        out
    }

    /// What an opened call shows: what it was asked, what it answered, and
    /// what went wrong. The stored body replaces the row's preview once it
    /// lands, so the reader never has to know which one they are looking at.
    /// What a call has to show, in the order it shows it: the prompt it was
    /// given, the error it raised, and the result it answered with, each from
    /// the journal when the body has landed and from the row's own preview
    /// until it does.
    fn body_parts<'a>(&'a self, call: &'a RunCall) -> Vec<BodyPart<'a>> {
        let t = theme::current();
        let key = call.call_key;
        let body = match self.bodies.get(&key) {
            Some(BodyState::Loaded(body)) => Some(body),
            _ => None,
        };
        let mut parts = Vec::with_capacity(3);
        if let Some(text) = body
            .map(|body| body.request.as_str())
            .or(call.prompt.as_deref())
        {
            parts.push(BodyPart {
                scope: FoldScope::Prompt(key),
                heading: PROMPT_HEADING,
                text,
                style: Style::default(),
            });
        }
        if let Some(text) = body
            .and_then(|body| body.error.as_deref())
            .or(call.error.as_deref())
        {
            parts.push(BodyPart {
                scope: FoldScope::Error(key),
                heading: ERROR_HEADING,
                text,
                style: t.tool_error,
            });
        }
        if let Some(text) = body
            .and_then(|body| body.result.as_deref())
            .or(call.result_preview.as_deref())
        {
            parts.push(BodyPart {
                scope: FoldScope::CallResult(key),
                heading: RESULT_HEADING,
                text,
                style: Style::default(),
            });
        }
        parts
    }

    /// A call's body on screen, with every part that is JSON drawn as a tree:
    /// a prompt built from a workflow's args is as much a value as the result
    /// answering it. Rows that open a node are the ones a cursor can rest on.
    fn body_rows(&self, call: &RunCall) -> Vec<JsonRow> {
        let t = theme::current();
        let parts = self.body_parts(call);
        if parts.is_empty() {
            return vec![plain_row(Line::styled(
                match self.bodies.get(&call.call_key) {
                    Some(BodyState::Missing) => BODY_MISSING,
                    _ => LOADING,
                },
                t.tool_dim,
            ))];
        }
        let mut rows = Vec::new();
        for part in parts {
            rows.push(plain_row(Line::styled(part.heading, t.tool_dim)));
            rows.extend(self.json_rows(&part));
        }
        rows
    }

    /// The nodes a call's body offers the cursor, part by part, in the order
    /// it draws them.
    fn body_folds(&self, call: &RunCall) -> Vec<(FoldScope, usize)> {
        self.body_parts(call)
            .into_iter()
            .flat_map(|part| {
                self.json_folds(&part)
                    .into_iter()
                    .map(move |node| (part.scope, node))
            })
            .collect()
    }

    /// A part as a folded tree when it parses as one, and as the lines it was
    /// written in when it does not.
    fn json_rows(&self, part: &BodyPart<'_>) -> Vec<JsonRow> {
        match self.json_body(part.text) {
            Some(value) => json::rows(&value, self.fold_set(part.scope)).unwrap_or_default(),
            None => indented(part.text, part.style)
                .into_iter()
                .map(plain_row)
                .collect(),
        }
    }

    /// The nodes a part offers the cursor, without paying to paint them.
    fn json_folds(&self, part: &BodyPart<'_>) -> Vec<usize> {
        match self.json_body(part.text) {
            Some(value) => json::folds(&value, self.fold_set(part.scope)),
            None => Vec::new(),
        }
    }

    fn json_body(&self, text: &str) -> Option<Value> {
        serde_json::from_str::<Value>(text)
            .ok()
            .filter(|value| value.is_object() || value.is_array())
    }

    fn fold_set(&self, scope: FoldScope) -> &HashSet<usize> {
        static NONE: std::sync::OnceLock<HashSet<usize>> = std::sync::OnceLock::new();
        self.folded
            .get(&scope)
            .unwrap_or_else(|| NONE.get_or_init(HashSet::new))
    }

    fn toggle_fold(&mut self, scope: FoldScope, node: usize) -> InspectorAction {
        let folded = self.folded.entry(scope).or_default();
        if !folded.insert(node) {
            folded.remove(&node);
        }
        InspectorAction::Consumed
    }

    /// What the row under the cursor opens: a scratch call opens its file, an
    /// agent call opens its body, and a phase is a link into the roster that
    /// lands the cursor on the first agent it dispatched. The rest are the
    /// record speaking for itself and have nothing behind them.
    fn open_timeline_row(&mut self) -> InspectorAction {
        let Some(Item::Row(index)) = self.items().get(self.cursor).copied() else {
            return InspectorAction::Consumed;
        };
        let Some(row) = self.timeline_rows(now_secs()).get(index).cloned() else {
            return InspectorAction::Consumed;
        };
        match row {
            TimelineRow::Phase { title, .. } => self.open_phase(&title),
            TimelineRow::Call { call, .. } => {
                let Some(call) = self.detail.as_ref().and_then(|d| d.calls.get(call)) else {
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
            _ => InspectorAction::Consumed,
        }
    }

    fn open_phase(&mut self, title: &str) -> InspectorAction {
        let Some(run) = self.selected_run() else {
            return InspectorAction::Consumed;
        };
        let Some(at) = agent_order(run)
            .iter()
            .position(|index| run.roster[*index].phase.as_deref() == Some(title))
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
        let (lines, _) = self.section_lines(now_secs(), self.body_area.width);
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

    /// The run's record as rows, or nothing until its journal has landed.
    fn timeline_rows(&self, now: u64) -> Vec<TimelineRow> {
        let (Some(run), Some(detail)) = (self.selected_run(), self.detail.as_ref()) else {
            return Vec::new();
        };
        timeline(run, detail, now)
    }

    /// Everything the cursor can rest on in the open section, in the order it
    /// is drawn: the section's own rows, and the JSON nodes of a body opened
    /// beneath one of them. One enumeration feeds the cursor, the marks and
    /// the click targets, so none of the three can count a row the others did
    /// not draw.
    fn items(&self) -> Vec<Item> {
        let Some(run) = self.selected_run() else {
            return Vec::new();
        };
        match self.section {
            Section::Timeline => {
                let mut items = Vec::new();
                for (index, row) in self.timeline_rows(now_secs()).iter().enumerate() {
                    items.push(Item::Row(index));
                    if let TimelineRow::Call { call, .. } = row
                        && let Some(call) = self.expanded_body(*call)
                    {
                        items.extend(self.call_folds(call));
                    }
                }
                items
            }
            Section::Agents => {
                let mut items = Vec::new();
                for index in agent_order(run) {
                    items.push(Item::Row(index));
                    let call_key = run.roster[index].call_key;
                    if let Some(call) = self
                        .agent_call(call_key)
                        .filter(|_| self.expanded_call == Some(call_key))
                    {
                        items.extend(self.call_folds(call));
                    }
                }
                items
            }
            Section::Result => {
                let mut items = self.result_folds(run);
                if run.scratch_path().is_some() {
                    items.push(Item::Row(0));
                }
                items
            }
            Section::Overview => Vec::new(),
        }
    }

    /// The call a timeline row stands for, when its body is the open one.
    fn expanded_body(&self, index: usize) -> Option<&RunCall> {
        let call = self.detail.as_ref()?.calls.get(index)?;
        (self.expanded_call == Some(call.call_key)).then_some(call)
    }

    fn call_folds(&self, call: &RunCall) -> Vec<Item> {
        self.body_folds(call)
            .into_iter()
            .map(|(scope, node)| Item::Fold(scope, node))
            .collect()
    }

    /// The nodes of a run's own result, which is a tree only when it is JSON
    /// the run did not write a report into.
    fn result_folds(&self, run: &RunSnapshot) -> Vec<Item> {
        let Some(result) = run.result.as_ref().filter(|result| !has_report(result)) else {
            return Vec::new();
        };
        json::folds(result, self.fold_set(FoldScope::Result))
            .into_iter()
            .map(|node| Item::Fold(FoldScope::Result, node))
            .collect()
    }

    fn item_count(&self) -> usize {
        self.items().len()
    }

    /// Sections the arrows walk with a cursor rather than scroll. The result
    /// earns one only when it has nodes to fold: a report is prose, and prose
    /// is read by scrolling.
    fn has_items(&self) -> bool {
        match self.section {
            Section::Timeline | Section::Agents => true,
            Section::Result => self.item_count() > 0,
            Section::Overview => false,
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
        let (list, detail) = self.panes(padded);
        let filtering = self.filter_focused || !self.filter.value().is_empty();
        let input_row = filtering || self.budget.is_some();
        let footer_rows = 1 + u16::from(input_row);
        let panes_height = padded.height.saturating_sub(footer_rows);
        let list = pane_rows(list, panes_height);
        let [tabs, body] =
            Layout::vertical([Constraint::Length(CHROME_ROWS - 1), Constraint::Fill(1)])
                .areas(pane_rows(detail, panes_height));
        self.popup = popup;
        self.list_area = list;
        self.tabs_area = tabs;
        self.body_area = body;

        if list.width > 0 {
            self.render_list(frame, list);
        }
        if body.width > 0 {
            self.render_tabs(frame, tabs);
            self.render_body(frame, body);
        }

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
        self.footer = self.footer_line(footer.width);
        self.footer_hits.set(self.footer.hits(footer, 0, 1));
        frame.render_widget(
            Paragraph::new(self.footer.line(self.footer_hits.hovered())),
            footer,
        );
        popup
    }

    /// The run list and the detail pane, or the one the cursor is on when the
    /// modal is too narrow to hold both. The hidden pane keeps a zero area, so
    /// nothing draws into it and the pointer cannot land on it.
    fn panes(&self, padded: Rect) -> (Rect, Rect) {
        if padded.width < SPLIT_MIN_COLS {
            return match self.pane {
                Pane::Runs => (padded, Rect::default()),
                Pane::Detail => (Rect::default(), padded),
            };
        }
        let list_width = (padded.width * LIST_PERCENT / 100).min(LIST_MAX_WIDTH);
        let [list, _, detail] = Layout::horizontal([
            Constraint::Length(list_width),
            Constraint::Length(PANE_GAP),
            Constraint::Fill(1),
        ])
        .areas(padded);
        (list, detail)
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
        let named = tab_strip_cols() <= area.width;
        for section in Section::ALL {
            let digit = section.index() + 1;
            let text = match named {
                true => format!("{digit} {}", section.label()),
                false => digit.to_string(),
            };
            let width = u16::try_from(text.len()).unwrap_or(u16::MAX);
            let style = match section == self.section {
                true => t.item_selected,
                false => t.tool_dim,
            };
            let hit = Rect::new(x, area.y, width, 1);
            if hit.right() <= area.right() {
                hits.push((hit, section));
            }
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
        let (lines, item_starts) = self.section_lines(now_secs(), area.width);
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
            && self.has_items()
            && let Some(&(top, height)) = self.item_rows.get(self.cursor)
        {
            self.scroll.reveal(top, height);
        }
        self.reveal_cursor = false;
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((self.scroll.offset(), 0)),
            area,
        );
        self.scrollbar.set_hint(ScrollHint::lines(
            u32::from(self.scroll.offset()) + 1,
            u32::from(rows.total),
        ));
        self.scrollbar
            .draw(frame, area, rows.total, self.scroll.offset());
    }

    /// The selected section as lines, and the logical line each of its
    /// items starts on. `width` is the columns the lines will be drawn in,
    /// which the sections that lay themselves out need and zero disables.
    fn section_lines(&self, now: u64, width: u16) -> (Vec<Line<'static>>, Vec<usize>) {
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
            Section::Timeline => self.timeline_lines(run, now, width),
            Section::Agents => self.agent_lines(run),
            Section::Result => result_lines(self, run, width),
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
        // The card's shape, carrying what the card has no room for: where the
        // phase sits among the declared ones, how much of the roster landed,
        // and a clock that runs while the run does.
        let mut stats = vec![status_span(run.status)];
        if let Some(phase) = &run.phase {
            stats.push(Span::raw(SEPARATOR));
            stats.push(Span::styled(escape_terminal_controls(phase), t.accent));
            if let Some((at, of)) = run.phase_position() {
                stats.push(Span::styled(format!(" {at}/{of}"), t.tool_dim));
            }
        }
        stats.push(Span::raw(format!(
            "{SEPARATOR}{}{SEPARATOR}{}{AGENT_SLASH}{}{AGENTS_SUFFIX}{SEPARATOR}{}{TOKENS_SUFFIX}{SEPARATOR}{}",
            roster_tally(run),
            run.usage.agents_admitted,
            run.agent_budget,
            format_compact(run.usage.tokens_used),
            format_elapsed(run.elapsed_secs(now)),
        )));
        lines.push(Line::from(stats));
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

    /// The run's record as one ordered list. Phases are the spine, and the
    /// calls and log lines that happened inside one are indented under it, so
    /// the join a reader used to perform across four sections is already done.
    fn timeline_lines(
        &self,
        run: &RunSnapshot,
        now: u64,
        width: u16,
    ) -> (Vec<Line<'static>>, Vec<usize>) {
        let t = theme::current();
        let Some(detail) = &self.detail else {
            return (vec![Line::styled(LOADING, t.tool_dim)], Vec::new());
        };
        let rows = timeline(run, detail, now);
        let window = (
            run.created_at,
            match run.status.is_terminal() {
                true => run.updated_at,
                false => now,
            },
        );
        if rows.is_empty() {
            return (vec![Line::styled(NO_TIMELINE, t.tool_dim)], Vec::new());
        }
        let (label_cols, bar_width) = timeline_columns(width);
        let spinner = spinner_str(animation_elapsed_ms());
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(rows.len());
        let mut starts = Vec::with_capacity(rows.len());
        if detail.journal_trimmed {
            lines.push(Line::styled(JOURNAL_TRIMMED, t.tool_warning));
        }
        let mut position = 0;
        for row in rows.iter() {
            starts.push(lines.len());
            let mut spans = vec![
                Span::styled(self.cursor_mark(position), t.accent),
                Span::styled(clock_column(row, run.created_at), t.tool_dim),
            ];
            position += 1;
            let label_cols = match row.is_nested() {
                true => {
                    spans.push(Span::raw(NEST_INDENT));
                    label_cols.saturating_sub(NEST_INDENT.len())
                }
                false => label_cols,
            };
            let mut tally: Vec<Span<'static>> = Vec::new();
            match row {
                TimelineRow::Phase {
                    title,
                    at,
                    end,
                    agents,
                    failed,
                } => {
                    let running = *end >= now && !run.status.is_terminal();
                    let (mark, style) = match running {
                        true => (PhaseMark::Current, t.accent),
                        false => (PhaseMark::Done, t.tool_success),
                    };
                    spans.push(Span::styled(format!("{} ", mark.glyph()), style));
                    spans.push(Span::styled(label_column(title, label_cols), t.bold));
                    spans.push(Span::styled(
                        duration_column(end.saturating_sub(*at)),
                        t.tool_dim,
                    ));
                    if *agents > 0 {
                        tally.push(Span::styled(
                            format!("{COLUMN_GAP}{agents}{AGENTS_SUFFIX}"),
                            t.tool_dim,
                        ));
                    }
                    if *failed > 0 {
                        tally.push(Span::styled(
                            format!("{COLUMN_GAP}{failed}{FAILED_UNIT}"),
                            t.tool_error,
                        ));
                    }
                }
                TimelineRow::Pending { title } => {
                    spans.push(Span::styled(
                        format!("{} ", PhaseMark::Pending.glyph()),
                        t.tool_dim,
                    ));
                    spans.push(Span::styled(escape_terminal_controls(title), t.tool_dim));
                }
                TimelineRow::Call { at, end, call } => {
                    let call = &detail.calls[*call];
                    match call.state {
                        CallState::Started => {
                            spans.push(Span::styled(spinner.to_owned(), t.spinner));
                        }
                        state => spans.push(Span::styled(
                            format!("{} ", call_glyph(state)),
                            call_style(state),
                        )),
                    }
                    spans.push(Span::styled(
                        label_column(&call_name(call), label_cols),
                        t.bold,
                    ));
                    spans.push(Span::styled(
                        duration_column(end.saturating_sub(*at)),
                        t.tool_dim,
                    ));
                    if call.tokens_used > 0 {
                        tally.push(Span::styled(
                            format!("{COLUMN_GAP}{}", format_compact(call.tokens_used)),
                            t.tool_dim,
                        ));
                    }
                }
                TimelineRow::Log { message, .. } => {
                    spans.push(Span::raw(GLYPH_PAD));
                    spans.push(Span::raw(escape_terminal_controls(message)));
                }
                TimelineRow::Settled { status, .. } => {
                    spans.push(Span::raw(GLYPH_PAD));
                    spans.push(status_span(*status));
                }
            }
            if let Some(span) = row.span() {
                let bar = span_bar(span, window, bar_width);
                if !bar.is_empty() {
                    spans.push(Span::raw(COLUMN_GAP));
                    spans.push(Span::styled(bar, t.tool_dim));
                }
            }
            if !grid_is_tight(width) {
                spans.extend(tally);
            }
            lines.push(Line::from(spans));
            if let TimelineRow::Call { call, .. } = row {
                let call = &detail.calls[*call];
                if self.expanded_call == Some(call.call_key) {
                    self.push_body(call, &mut lines, &mut starts, &mut position);
                }
            }
        }
        (lines, starts)
    }

    /// The last progress an agent reported, whether or not it is still going.
    /// A settled agent keeps its final activity, because what it was doing
    /// when it stopped is the part a reader is looking for.
    fn activity(&self, run: &RunSnapshot, agent: &AgentRosterEntry) -> Option<&ToolProgress> {
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
        let mut position = 0;
        for index in agent_order(run) {
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
            match agent.state {
                RosterState::Running => {}
                _ => spans.push(Span::styled(
                    format!(
                        "{SEPARATOR}{}{TOKENS_SUFFIX}{SEPARATOR}{}",
                        format_compact(agent.tokens_used),
                        format_elapsed(agent.duration_ms / 1_000)
                    ),
                    t.tool_dim,
                )),
            }
            let progress = self.activity(run, agent);
            if let Some(progress) = progress {
                spans.extend(activity_spans(progress, agent.state));
            }
            starts.push(lines.len());
            position += 1;
            lines.push(Line::from(spans));
            // The batch the agent is working through, hung under the row that
            // names it and indented past the mark and glyph columns so the
            // connectors sit under the agent's label.
            if let Some(progress) = progress.filter(|_| agent.state == RosterState::Running) {
                let children = progress.report.activity.children();
                for (index, child) in children.iter().enumerate() {
                    let connector = match index + 1 == children.len() {
                        true => TREE_LAST,
                        false => TREE_BRANCH,
                    };
                    lines.push(Line::from(activity_child_spans(
                        child,
                        format!("{}{GLYPH_PAD}{connector}", " ".repeat(MARK_COLS)),
                    )));
                }
            }
            if let Some(call) = self.agent_call(agent.call_key)
                && self.expanded_call == Some(agent.call_key)
            {
                self.push_body(call, &mut lines, &mut starts, &mut position);
            }
        }
        (lines, starts)
    }

    /// A call's body under the row that opened it. A row that opens a JSON
    /// node takes a cursor position of its own, in the same order `items`
    /// counts them, so a mark cannot land on a row that opens nothing.
    fn push_body(
        &self,
        call: &RunCall,
        lines: &mut Vec<Line<'static>>,
        starts: &mut Vec<usize>,
        position: &mut usize,
    ) {
        let t = theme::current();
        for row in self.body_rows(call) {
            let mark = match row.fold {
                Some(_) => {
                    starts.push(lines.len());
                    let mark = self.cursor_mark(*position);
                    *position += 1;
                    mark
                }
                None => NO_MARK,
            };
            let mut line = row.line;
            line.spans.insert(0, Span::styled(mark, t.accent));
            lines.push(line);
        }
    }

    /// The journal row an agent came from, once the detail has landed. The
    /// roster and the journal are two halves of one row: the roster knows the
    /// phase and the state, the journal knows what was asked and answered.
    fn agent_call(&self, call_key: u64) -> Option<&RunCall> {
        self.detail
            .as_ref()?
            .calls
            .iter()
            .find(|call| call.call_key == call_key)
    }

    /// The footer is one centred row, and a line wider than that row wraps and
    /// then answers no clicks at all, so a narrow modal gives up the words that
    /// gloss its keys, and then the space between them, before it gives up a
    /// key.
    fn footer_line(&self, width: u16) -> FooterLine {
        if self.budget.is_some() {
            return self.budget_footer();
        }
        let mut footer = self.commands_footer(FOOTER_RUNGS[0].0, FOOTER_RUNGS[0].1);
        for (glossed, gap) in FOOTER_RUNGS.into_iter().skip(1) {
            if footer.fits(width) {
                break;
            }
            footer = self.commands_footer(glossed, gap);
        }
        footer
    }

    fn budget_footer(&self) -> FooterLine {
        let t = theme::current();
        let mut footer = FooterLine::default();
        for (index, (key, description)) in BUDGET_FOOTER.iter().enumerate() {
            if index > 0 {
                footer.text(SECTION_GAP, Style::default());
            }
            footer.text(*key, t.keybind_key);
            footer.text(format!(" {description}"), t.tool_dim);
        }
        footer
    }

    fn commands_footer(&self, glossed: bool, gap: &'static str) -> FooterLine {
        let t = theme::current();
        let run = self.selected_run();
        let controllable = run.is_some_and(|run| !self.is_foreign(&run.run_id));
        let mut footer = FooterLine::default();
        for (index, (key, description, command)) in FOOTER.iter().enumerate() {
            if index > 0 {
                footer.text(gap, Style::default());
            }
            let enabled = match command {
                FooterCommand::Control(control) => {
                    controllable && run.is_some_and(|run| control.applies_to(run.status))
                }
                FooterCommand::Activate => self.has_items() && self.item_count() > 0,
                FooterCommand::Transcript => self.cursor_task_id().is_some(),
                FooterCommand::Script => run.is_some_and(|run| run.source_path.is_some()),
                FooterCommand::Export => self.detail.is_some(),
                FooterCommand::Copy => run.is_some(),
                FooterCommand::Filter | FooterCommand::Close => true,
            };
            let key_style = match enabled {
                true => t.keybind_key,
                false => t.tool_dim,
            };
            footer.command(key, key_style);
            if glossed {
                footer.describe(format!(" {description}"), t.tool_dim);
            }
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

/// `· Running cargo test · 3 tools · 1m2s`, the shape a subagent's task header
/// already uses, so a workflow agent reads like any other agent.
///
/// What an agent is doing, or was doing when it stopped. A settled agent's
/// last report is dimmed throughout and drops the running tally, which its
/// roster row already states in final form.
fn activity_spans(progress: &ToolProgress, state: RosterState) -> Vec<Span<'static>> {
    let t = theme::current();
    let running = state == RosterState::Running;
    let label = match running {
        true => t.tool_prefix,
        false => t.tool_dim,
    };
    let mut spans = vec![Span::raw(SEPARATOR)];
    if let Some(sigil) = activity_sigil(&progress.report.activity) {
        spans.push(Span::styled(format!("{sigil} "), label));
    }
    spans.push(Span::styled(
        activity_label(&progress.report.activity),
        label,
    ));
    if let Some(detail) = activity_detail(&progress.report.activity) {
        spans.push(Span::styled(format!(" {detail}"), t.tool_dim));
    }
    if running {
        spans.push(Span::styled(
            format!(
                "{SEPARATOR}{}",
                SubagentProgress::tally(progress.report.tools, progress.elapsed())
            ),
            t.tool_dim,
        ));
    }
    spans
}

fn result_lines(
    inspector: &WorkflowInspector,
    run: &RunSnapshot,
    width: u16,
) -> (Vec<Line<'static>>, Vec<usize>) {
    let t = theme::current();
    let mut lines = Vec::new();
    let mut starts = Vec::new();
    let mut position = 0;
    if let Some(result) = &run.result {
        match result.get(REPORT_FIELD).and_then(Value::as_str) {
            Some(report) => lines.extend(report_lines(report, width)),
            None => {
                lines.push(Line::styled(RESULT_LABEL, t.tool_dim));
                let rows = json::rows(result, inspector.fold_set(FoldScope::Result))
                    .unwrap_or_else(|| vec![plain_row(Line::raw(result.to_string()))]);
                for row in rows {
                    let mark = match row.fold {
                        Some(_) => {
                            starts.push(lines.len());
                            let mark = inspector.cursor_mark(position);
                            position += 1;
                            mark
                        }
                        None => NO_MARK,
                    };
                    let mut line = row.line;
                    line.spans.insert(0, Span::styled(mark, t.accent));
                    lines.push(line);
                }
            }
        }
    }
    if let Some(path) = run.scratch_path() {
        lines.push(Line::default());
        starts.push(lines.len());
        let mut line = labelled(SCRATCH_LABEL, path, t.tool_path);
        if position > 0 {
            line.spans
                .insert(0, Span::styled(inspector.cursor_mark(position), t.accent));
        }
        lines.push(line);
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

/// A workflow's report is markdown a model wrote, so it is painted rather
/// than shown as source. Painting needs the columns it will wrap into, and a
/// caller that has none asks for the source instead.
fn report_lines(report: &str, width: u16) -> Vec<Line<'static>> {
    if width == 0 {
        return report
            .lines()
            .map(|line| Line::raw(line.to_owned()))
            .collect();
    }
    let style = theme::current().assistant;
    let (painted, _) = text_to_painted(
        report,
        "",
        style,
        style,
        width,
        Some(caudra_markdown::render::TOOL_OUTPUT_MAX_LINE_BYTES),
        Vec::new(),
    );
    painted.lines
}

fn log_line(offset: u64, message: &str, style: Style) -> Line<'static> {
    let t = theme::current();
    Line::from(vec![
        Span::styled(offset_text(offset), t.tool_dim),
        Span::styled(escape_terminal_controls(message), style),
    ])
}

fn offset_text(seconds: u64) -> String {
    format!("+{:<ELAPSED_COLS$}", format_elapsed(seconds))
}

/// What a timeline row leads with: when it happened on the run's own clock,
/// blank for a phase the run has not reached.
fn clock_column(row: &TimelineRow, created_at: u64) -> String {
    match row.at() {
        Some(at) => offset_text(at.saturating_sub(created_at)),
        None => " ".repeat(CLOCK_COLS),
    }
}

/// How long a row lasted, right aligned so the readings stack by magnitude
/// rather than by however wide the label before them happened to be.
fn duration_column(seconds: u64) -> String {
    format!("{:>DURATION_COLS$}", format_elapsed(seconds))
}

/// The label and bar widths at this pane width. Both give way to the fixed
/// columns around them and share what is left, so a wider pane spends it on
/// longer names and longer bars rather than on moving the columns.
/// A pane below the width the grid was drawn for keeps the clock, the label
/// and the duration, and leaves the bar and the tallies out: a bar of two
/// columns says nothing, and a tally that wraps costs the row below it. No
/// width at all is a caller with no pane to fit, such as a copy.
fn grid_is_tight(width: u16) -> bool {
    width > 0 && width < DETAIL_MIN_COLS
}

fn timeline_columns(width: u16) -> (usize, usize) {
    let mut fixed = MARK_COLS + CLOCK_COLS + GLYPH_PAD.len() + DURATION_COLS + COLUMN_GAP.len();
    if grid_is_tight(width) {
        let label = usize::from(width).saturating_sub(fixed);
        return (label.clamp(LABEL_MIN_COLS, LABEL_MAX_COLS), 0);
    }
    fixed += TALLY_COLS;
    let free = usize::from(width).saturating_sub(fixed);
    let label = (free / 2).clamp(LABEL_MIN_COLS, LABEL_MAX_COLS);
    (label, free.saturating_sub(label).min(BAR_MAX_WIDTH))
}

/// A pane's rows, or nothing at all when the pane is the hidden one.
fn pane_rows(pane: Rect, height: u16) -> Rect {
    match pane.width {
        0 => Rect::default(),
        _ => Rect { height, ..pane },
    }
}

/// What the tab strip needs to name every section, including the gap the last
/// one carries.
fn tab_strip_cols() -> u16 {
    let named: usize = Section::ALL
        .iter()
        .map(|section| section.label().len() + 1 + SECTION_GAP.len() + 1)
        .sum();
    u16::try_from(named).unwrap_or(u16::MAX)
}

/// A label at exactly `cols` columns, cut with an ellipsis when it is longer,
/// so the column after it starts in the same place on every row.
fn label_column(text: &str, cols: usize) -> String {
    let mut text = escape_terminal_controls(text);
    let width = UnicodeWidthStr::width(text.as_str());
    if width <= cols {
        text.push_str(&" ".repeat(cols - width));
        return text;
    }
    let mut cut = String::with_capacity(cols);
    let mut used = 0;
    for character in text.chars() {
        let next = used + UnicodeWidthChar::width(character).unwrap_or(0);
        if next > cols.saturating_sub(1) {
            break;
        }
        cut.push(character);
        used = next;
    }
    cut.push(ELLIPSIS);
    cut.push_str(&" ".repeat(cols.saturating_sub(used + 1)));
    cut
}

fn labelled(label: &'static str, text: &str, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(label, theme::current().tool_dim),
        Span::styled(escape_terminal_controls(text), style),
    ])
}

/// Whether a result carries the report a workflow wrote, which is prose and
/// not a tree.
fn has_report(result: &Value) -> bool {
    result.get(REPORT_FIELD).and_then(Value::as_str).is_some()
}

/// A body line that opens nothing, which is every line but the head of a
/// JSON node.
fn plain_row(line: Line<'static>) -> JsonRow {
    JsonRow { fold: None, line }
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

/// What a call is called on a timeline row: the label the script gave it,
/// else the first line of its prompt, else its position in the journal.
fn call_name(call: &RunCall) -> String {
    call.label
        .as_deref()
        .or_else(|| call.prompt.as_deref().and_then(|text| text.lines().next()))
        .map_or_else(|| format!("{CALL_PREFIX}{}", call.call_key), str::to_owned)
}

fn call_glyph(state: CallState) -> &'static str {
    match state {
        CallState::Started => CALL_RUNNING_GLYPH,
        CallState::Completed => CALL_DONE_GLYPH,
        CallState::Failed => CALL_FAILED_GLYPH,
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
    use caudra_workflow::{CallKind, PhaseRecord, RunEvent, RunEventKind, RunUsage, SourceKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Modifier;
    use test_case::test_case;

    use super::*;
    use crate::components::buffer_text;
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
    const HOVER_MARKS_THE_PHRASE: &str = "a footer control reverses whole under the pointer";
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
    const BUSY_ROSTER: u64 = 8;
    const LANDED_AGENTS: usize = 3;
    const BIG_BUDGET: u32 = 128;
    const ADMITTED_AGENTS: u32 = 11;
    const MANY_TOKENS: u64 = 1_068_245;
    /// Wide enough to be usable and too narrow to split into two panes.
    const NARROW_TERMINAL: u16 = 80;
    /// Too narrow for the whole tab strip, wide enough for one section name,
    /// so a strip that gave up on names is told apart from one that was cut.
    const CRAMPED_TERMINAL: u16 = 40;
    /// Narrower than one tab.
    const TINY_TERMINAL: u16 = 8;
    /// The keys with one space between them and none of their words.
    const PACKED_FOOTER_COLS: u16 = 25;
    const GLOSSED_KEY: &str = "Transcript";
    const TIGHT_PANE_COLS: u16 = 38;
    const BOTH_PANES_DRAW: &str = "a modal wide enough for two panes draws both";
    const ONE_PANE_DRAWS: &str = "a modal too narrow for two panes draws one";
    const RIGHT_SHOWS_DETAIL: &str = "right moves the cursor to the detail pane";
    const LEFT_SHOWS_RUNS: &str = "left moves the cursor back to the run list";
    const ONE_PANE_TAKES_THE_WIDTH: &str = "the pane on show takes every column";
    const FOOTER_FITS: &str = "the footer draws on one row of the width it was given";
    const FOOTER_KEEPS_KEYS: &str = "every footer key answers the pointer at every width";
    const FOOTER_GLOSSES: &str = "a footer with room for its words keeps them";
    const NAMES_WHEN_THEY_FIT: &str = "a tab strip with room names its sections";
    const DIGITS_WHEN_NAMES_DO_NOT_FIT: &str = "a tab strip without room shows digits alone";
    const TINY_STRIP_DROPS_TABS: &str = "a strip too narrow for every tab draws fewer";
    const HITS_STAY_IN_THE_PANE: &str = "a tab claims no cells outside the pane it drew in";
    const TIGHT_ROW_FITS: &str = "a timeline row fits the pane it was laid out for";
    const WIDE_ROW_TALLIES: &str = "a pane with room for the tallies shows them";
    const JSON_RESULT: &str = "{\"findings\":{\"claims\":[1,2],\"score\":3}}";
    const FOLDED_KEY: &str = "\"claims\"";
    const CURSOR_AND_ROWS_AGREE: &str =
        "the cursor counts exactly the rows the section marked as its items";
    const NODES_FOLLOW_THEIR_ROW: &str = "a body's nodes come after the row that opened it";
    const FOLD_HIDES_THE_SUBTREE: &str = "closing a node takes its children off the section";
    const JSON_IS_A_TREE: &str = "a result that is JSON draws as a tree";
    const CALL_STAYS_OPEN: &str = "folding a node inside a body leaves the body open";
    const OUTER_KEY: &str = "\"findings\"";
    const OUTER_STAYS_OPEN: &str = "folding a node leaves the nodes above it open";
    const MARK_FOLLOWS_THE_CURSOR: &str = "the marked row is the row the cursor names";
    const JSON_PROMPT: &str = "{\"question\":\"why\",\"breadth\":2}";
    const PROMPT_KEY: &str = "\"question\"";
    const PROMPT_IS_A_TREE: &str = "a prompt that is JSON folds like any other value";
    const PARTS_FOLD_APART: &str = "closing a node in one part leaves the others open";
    const MARKDOWN_HEADING: &str = "## Findings";
    const MARKDOWN_REPORT: &str = "## Findings\n\nA **bold** claim.\n\n- one\n";
    const PAINTED_HEADING: &str = "Findings";
    const PAINTED_BULLET: &str = "\u{2022} one";
    const PAINTS_MARKDOWN: &str = "a report is painted as markdown, not shown as source";
    const KEEPS_SOURCE: &str = "a report asked for with no width stays source";
    const SCRATCH_IS_A_TARGET: &str = "the scratch row is the row the result section targets";
    const ABBREVIATED_STATS: &str = "active \u{b7} Report 2/2 \u{b7} 3/8 done \u{b7} 11/128 agents \u{b7} 1.1M tokens \u{b7} 2m14s";
    const ONE_AGENT: &str = "1 agents";
    const CALL_LABEL: &str = "researcher";
    const LONG_CALL: &str = "researcher-with-a-name-too-long-for-one-column";
    const PHASE_SECS: u64 = 90;
    const PHASE_ELAPSED: &str = "1m30s";
    const CALL_AT: u64 = 3;
    const CALL_SECS: u64 = 12;
    const CALL_ELAPSED: &str = "12s";
    const COLUMNS_LINE_UP: &str = "every timeline row must end its duration in the same column";
    const WHEEL_ROWS: i32 = 3;
    const SCROLL_STICKS: &str = "a scrolled section must stay where the scroll put it";
    const TAIL_IS_FOLLOWED: &str = "a live timeline must keep showing its newest row";
    const POSITION_HINT: &str = "line 1/";
    const SCROLL_OUTRANKS: &str =
        "a scroll and a cursor move in one frame must leave the view where the scroll put it";
    const HINT_IS_SHOWN: &str = "a held bar must say where in the section the view is";
    const LOG_LINE: &str = "dispatching the workers";
    const TIMELINE_IS_ONE_ORDER: &str =
        "a phase, the calls it opened, and the lines logged beside them must read in that order";
    const ROSTER_TALLY: &str = "0/1 done";
    const CLOCK_IS_LAST: &str = "the clock holds the right edge of a run row";
    const PHASE_COUNTS_ITS_OWN: &str = "a phase row counts the agents it dispatched";
    const PENDING_IS_LISTED: &str = "a declared phase the run has not reached is listed";
    const EARLY_AGENT: &str = "scout";
    const LATE_AGENT: &str = "writer";
    const EARLY_KEY: u64 = 1;
    const LATE_KEY: u64 = 2;
    const RUNNING_TOOL: &str = "shell";
    /// The verb the row shows for `RUNNING_TOOL`, which names itself nowhere.
    const RUNNING_LABEL: &str = "Running";
    const TOOL_SUMMARY: &str = "cargo nextest run";
    const TOOLS_RUN: u32 = 3;
    const TOOLS_TALLY: &str = "3 tools";
    const ACTIVITY_IS_TALLIED: &str = "a running agent counts the tools it has called";
    const SCRIPT_PATH: &str = "/project/.caudra/workflows/review.rhai";
    const EXPORT_ASKS_FIRST: &str = "an export must fetch every body before it copies";
    const EXPORT_IS_THE_WHOLE_RUN: &str = "an export must carry every call name, prompt and result";
    const TIMELINE_REACHES_THE_TRANSCRIPT: &str =
        "the transcript key must reach an agent from its timeline row";
    const ROSTER_JOINS_THE_JOURNAL: &str =
        "an agent row must open the request and result its call journaled";
    const ACTIVITY_OUTLIVES_ITS_AGENT: &str = "a settled agent must keep what it was last doing";
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
            source_path: None,
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
                label: Some(CALL_LABEL.into()),
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
        terminal_at(FRAME_WIDTH)
    }

    fn terminal_at(width: u16) -> Terminal<TestBackend> {
        Terminal::new(TestBackend::new(width, FRAME_HEIGHT)).unwrap()
    }

    /// One pane drew, and the other kept nothing for the pointer to land on.
    fn only_pane_drawn(inspector: &WorkflowInspector) -> Option<Pane> {
        match (inspector.list_area.width, inspector.body_area.width) {
            (0, 0) => None,
            (0, _) => Some(Pane::Detail),
            (_, 0) => Some(Pane::Runs),
            _ => None,
        }
    }

    /// Two panes at a width that holds both, so the narrow case is measured
    /// against a frame that is known to split.
    #[test]
    fn a_wide_modal_shows_both_panes() {
        let mut terminal = terminal();
        let mut inspector = open_with(vec![run(RUN_ID, RunStatus::Active, Vec::new())]);

        draw(&mut inspector, &mut terminal);

        assert!(inspector.list_area.width > 0, "{BOTH_PANES_DRAW}");
        assert!(inspector.body_area.width > 0, "{BOTH_PANES_DRAW}");
    }

    /// Two panes split out of too few columns are two panes too narrow to
    /// read, so below the width they need the modal shows the one the cursor
    /// is on and the arrows move between them.
    #[test]
    fn a_modal_too_narrow_for_two_panes_shows_one_at_a_time() {
        let mut terminal = terminal_at(NARROW_TERMINAL);
        let mut inspector = open_with(vec![run(RUN_ID, RunStatus::Active, Vec::new())]);

        draw(&mut inspector, &mut terminal);
        assert_eq!(
            only_pane_drawn(&inspector),
            Some(Pane::Runs),
            "{ONE_PANE_DRAWS}"
        );
        let runs_width = inspector.list_area.width;

        let _ = inspector.handle_key(key_event(KeyCode::Right));
        draw(&mut inspector, &mut terminal);
        assert_eq!(
            only_pane_drawn(&inspector),
            Some(Pane::Detail),
            "{RIGHT_SHOWS_DETAIL}"
        );
        assert_eq!(
            inspector.body_area.width, runs_width,
            "{ONE_PANE_TAKES_THE_WIDTH}"
        );

        let _ = inspector.handle_key(key_event(KeyCode::Left));
        draw(&mut inspector, &mut terminal);
        assert_eq!(
            only_pane_drawn(&inspector),
            Some(Pane::Runs),
            "{LEFT_SHOWS_RUNS}"
        );
    }

    /// A footer wider than its row wraps, and a wrapped footer answers no
    /// clicks at all, so every width that can hold the keys holds all of them.
    #[test]
    fn the_footer_keeps_every_key_clickable_as_it_narrows() {
        let inspector = open_with(vec![run(RUN_ID, RunStatus::Active, Vec::new())]);

        for width in PACKED_FOOTER_COLS..=FRAME_WIDTH {
            let footer = inspector.footer_line(width);
            let row = Rect::new(0, 0, width, 1);
            assert!(footer.fits(width), "{FOOTER_FITS}: {width}");
            assert_eq!(
                footer.hits(row, 0, 1).len(),
                FOOTER.len(),
                "{FOOTER_KEEPS_KEYS}: {width}"
            );
        }
    }

    /// The words are what the footer gives up first, so a row wide enough to
    /// gloss the keys still does.
    #[test]
    fn a_wide_footer_still_glosses_its_keys() {
        let inspector = open_with(vec![run(RUN_ID, RunStatus::Active, Vec::new())]);

        let footer = inspector.footer_line(FRAME_WIDTH);

        assert!(
            line_text(&footer.line(None)).contains(GLOSSED_KEY),
            "{FOOTER_GLOSSES}"
        );
    }

    /// A name cut in half selects nothing a reader can read, so a strip with
    /// no room for names shows the digits that select the sections instead.
    #[test]
    fn a_tab_strip_too_narrow_for_names_shows_the_digits() {
        let mut terminal = terminal_at(NARROW_TERMINAL);
        let mut inspector = open_with(vec![run(RUN_ID, RunStatus::Active, Vec::new())]);
        inspector.pane = Pane::Detail;

        draw(&mut inspector, &mut terminal);
        let named = buffer_text(terminal.backend().buffer()).contains(Section::Overview.label());

        let mut narrow = terminal_at(CRAMPED_TERMINAL);
        draw(&mut inspector, &mut narrow);

        assert!(named, "{NAMES_WHEN_THEY_FIT}");
        assert!(
            !buffer_text(narrow.backend().buffer()).contains(Section::Overview.label()),
            "{DIGITS_WHEN_NAMES_DO_NOT_FIT}"
        );
    }

    /// A tab the pane had no room to draw claims no cells, because a click
    /// there landed on whatever the terminal actually shows.
    #[test]
    fn a_tab_drawn_past_its_pane_claims_no_cells() {
        let mut terminal = terminal_at(TINY_TERMINAL);
        let mut inspector = open_with(vec![run(RUN_ID, RunStatus::Active, Vec::new())]);
        inspector.pane = Pane::Detail;

        draw(&mut inspector, &mut terminal);

        assert!(
            inspector.tab_hits.len() < Section::ALL.len(),
            "{TINY_STRIP_DROPS_TABS}: {:?}",
            inspector.tabs_area
        );
        let edge = inspector.tabs_area.right();
        assert!(
            inspector
                .tab_hits
                .iter()
                .all(|(hit, _)| hit.right() <= edge),
            "{HITS_STAY_IN_THE_PANE}"
        );
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
    fn a_footer_control_hovers_as_one_phrase_and_answers_a_click() {
        const CLOSE: usize = FOOTER.len() - 1;
        // Wide enough for every control: a footer that wraps offers no hits.
        const WIDE: u16 = 160;
        let mut terminal = Terminal::new(TestBackend::new(WIDE, FRAME_HEIGHT)).unwrap();
        let mut inspector = open_with(vec![run(RUN_ID, RunStatus::Active, Vec::new())]);
        draw(&mut inspector, &mut terminal);
        let hit = inspector.footer_hits.hit(CLOSE);
        let (label, description, _) = FOOTER[CLOSE];
        assert_eq!(
            usize::from(hit.width),
            label.len() + " ".len() + description.len(),
            "the hit spans the key and its gloss"
        );

        let _ = inspector.handle_mouse(mouse_at(MouseEventKind::Moved, hit.x, hit.y));
        draw(&mut inspector, &mut terminal);
        let buffer = terminal.backend().buffer();
        let reversed = buffer
            .area
            .positions()
            .filter(|position| {
                buffer[(position.x, position.y)]
                    .style()
                    .add_modifier
                    .contains(Modifier::REVERSED)
            })
            .collect::<Vec<_>>();
        assert!(
            reversed.len() == usize::from(hit.width)
                && reversed.iter().all(|position| hit.contains(*position)),
            "{HOVER_MARKS_THE_PHRASE}: hit={hit:?} reversed={reversed:?}"
        );

        let _ = inspector.handle_mouse(mouse_at(
            MouseEventKind::Down(MouseButton::Left),
            hit.x,
            hit.y,
        ));
        assert_eq!(
            inspector.handle_mouse(mouse_at(
                MouseEventKind::Up(MouseButton::Left),
                hit.x,
                hit.y
            )),
            InspectorAction::Close
        );
    }

    #[test]
    fn a_hovered_section_tab_marks_itself_without_switching() {
        let mut terminal = terminal();
        let mut inspector = open_with(vec![run(RUN_ID, RunStatus::Active, Vec::new())]);
        draw(&mut inspector, &mut terminal);
        let (hit, section) = *inspector
            .tab_hits
            .iter()
            .find(|(_, section)| *section == Section::Agents)
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
    fn a_hovered_agent_row_expands_on_one_click() {
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

        assert_eq!(
            action,
            InspectorAction::LoadCallBody {
                run_id: RUN_ID.into(),
                call_key: Some(1),
            },
            "{ONE_CLICK_OPENS}"
        );
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
    fn the_timeline_joins_the_phases_the_calls_and_the_log_into_one_order() {
        let mut walking = run(RUN_ID, RunStatus::Active, vec![agent(None)]);
        walking.phases = vec![PHASE_ONE.into(), PHASE_TWO.into()];
        walking.phase = Some(PHASE_ONE.into());
        walking.phase_history = vec![PhaseRecord {
            title: PHASE_ONE.into(),
            started_at: 0,
        }];
        walking.roster[0].phase = Some(PHASE_ONE.into());
        walking.roster[0].state = RosterState::Completed;
        let mut inspector = open_with(vec![walking.clone()]);
        let mut journal = detail(walking);
        journal.events.push(RunEvent {
            seq: 0,
            at: 1,
            kind: RunEventKind::Log,
            text: LOG_LINE.into(),
        });
        inspector.fill_detail(journal);

        let text = section_text(&mut inspector, '2');

        let phase = text.find(PHASE_ONE).expect(&text);
        let call = text.find(CALL_LABEL).expect(&text);
        let logged = text.find(LOG_LINE).expect(&text);
        assert!(
            phase < call && call < logged,
            "{TIMELINE_IS_ONE_ORDER}: {text}"
        );
        assert!(text.contains(ONE_AGENT), "{PHASE_COUNTS_ITS_OWN}: {text}");
        assert!(
            text.contains(&format!("{} {PHASE_TWO}", PhaseMark::Pending.glyph())),
            "{PENDING_IS_LISTED}: {text}"
        );
    }

    /// A completed run whose phase and whose call ran for different lengths
    /// of time under labels of different lengths, which is what the columns
    /// have to survive.
    fn timed_run() -> (RunSnapshot, RunDetail) {
        let mut walked = run(RUN_ID, RunStatus::Completed, Vec::new());
        walked.updated_at = PHASE_SECS;
        walked.phase_history = vec![PhaseRecord {
            title: PHASE_ONE.into(),
            started_at: 0,
        }];
        let mut journal = detail(walked.clone());
        journal.calls[0].label = Some(LONG_CALL.into());
        journal.calls[0].started_at = CALL_AT;
        journal.calls[0].finished_at = Some(CALL_AT + CALL_SECS);
        (walked, journal)
    }

    fn line_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// A run of any size reads at a glance, which a grouped integer does not:
    /// `1,068,245` is four columns of digits nobody counts. The line carries
    /// what the transcript card has no room for as well, so opening the
    /// inspector tells the reader more than the card it was opened from.
    #[test]
    fn the_overview_reads_its_stats_on_one_abbreviated_line() {
        let mut settled = roster(BUSY_ROSTER);
        for agent in settled.iter_mut().take(LANDED_AGENTS) {
            agent.state = RosterState::Completed;
        }
        let mut run = run(RUN_ID, RunStatus::Active, settled);
        run.phase = Some(PHASE_TWO.into());
        run.phases = vec![PHASE_ONE.into(), PHASE_TWO.into()];
        run.agent_budget = BIG_BUDGET;
        run.usage = RunUsage {
            agents_admitted: ADMITTED_AGENTS,
            tokens_used: MANY_TOKENS,
        };
        let inspector = open_with(vec![run.clone()]);

        let lines = inspector.overview_lines(&run, ELAPSED_SECS);

        assert_eq!(line_text(&lines[1]), ABBREVIATED_STATS);
    }

    fn json_call() -> (RunSnapshot, RunDetail) {
        let settled = run(RUN_ID, RunStatus::Completed, vec![agent(Some(TASK_ID))]);
        let mut journal = detail(settled.clone());
        journal.calls[0].result_preview = Some(JSON_RESULT.into());
        (settled, journal)
    }

    /// An agent is dispatched with a prompt a workflow built out of its args,
    /// which is as much a value as the result answering it. A node closed in
    /// one of them says nothing about the other, because the two are separate
    /// trees whose roots would otherwise share a name.
    #[test]
    fn a_json_prompt_folds_as_its_own_tree() {
        let (settled, mut journal) = json_call();
        journal.calls[0].prompt = Some(JSON_PROMPT.into());
        let mut inspector = open_with(vec![settled]);
        inspector.fill_detail(journal);
        let _ = inspector.handle_key(key_event(KeyCode::Char('3')));
        let _ = inspector.handle_key(key_event(KeyCode::Right));
        let _ = inspector.handle_key(key_event(KeyCode::Enter));
        assert!(
            section_of(&inspector)
                .0
                .iter()
                .any(|line| line.contains(PROMPT_KEY)),
            "{PROMPT_IS_A_TREE}"
        );

        let _ = inspector.handle_key(key_event(KeyCode::Down));
        let _ = inspector.handle_key(key_event(KeyCode::Enter));

        let (text, starts, items) = section_of(&inspector);
        assert!(
            text.iter().all(|line| !line.contains(PROMPT_KEY)),
            "{PROMPT_IS_A_TREE}: {text:?}"
        );
        assert!(
            text.iter().any(|line| line.contains(FOLDED_KEY)),
            "{PARTS_FOLD_APART}: {text:?}"
        );
        assert_eq!(starts, items, "{CURSOR_AND_ROWS_AGREE}: {text:?}");
    }

    fn section_of(inspector: &WorkflowInspector) -> (Vec<String>, usize, usize) {
        let (lines, starts) = inspector.section_lines(now_secs(), FRAME_WIDTH);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        (text, starts.len(), inspector.item_count())
    }

    /// The cursor, the marks and the click targets all read one enumeration.
    /// If they ever disagree, an arrow key lands the cursor on a row nothing
    /// marked and Enter acts on something the reader is not looking at.
    #[test_case('2' ; "timeline")]
    #[test_case('3' ; "agents")]
    fn every_marked_row_is_a_row_the_cursor_can_reach(section: char) {
        let (settled, journal) = json_call();
        let mut inspector = open_with(vec![settled]);
        inspector.fill_detail(journal);
        let _ = inspector.handle_key(key_event(KeyCode::Char(section)));
        let _ = inspector.handle_key(key_event(KeyCode::Enter));

        let (text, starts, items) = section_of(&inspector);

        assert!(
            text.iter().any(|line| line.contains(FOLDED_KEY)),
            "{JSON_IS_A_TREE}: {text:?}"
        );
        assert_eq!(starts, items, "{CURSOR_AND_ROWS_AGREE}: {text:?}");
    }

    /// A node opens under the row that owns it, so walking down from a call
    /// walks into its result rather than past it to the next call.
    #[test]
    fn an_opened_body_puts_its_nodes_after_the_row_that_opened_it() {
        let (settled, journal) = json_call();
        let mut inspector = open_with(vec![settled]);
        inspector.fill_detail(journal);
        let _ = inspector.handle_key(key_event(KeyCode::Char('3')));

        let before = inspector.item_count();
        let _ = inspector.handle_key(key_event(KeyCode::Enter));

        assert!(inspector.item_count() > before, "{NODES_FOLLOW_THEIR_ROW}");
        assert!(
            matches!(inspector.items().get(1), Some(Item::Fold(..))),
            "{NODES_FOLLOW_THEIR_ROW}: {:?}",
            inspector.items()
        );
    }

    /// Enter on a node closes it, and what it held leaves the section with it.
    #[test]
    fn closing_a_node_takes_its_children_with_it() {
        let (settled, journal) = json_call();
        let mut inspector = open_with(vec![settled]);
        inspector.fill_detail(journal);
        let _ = inspector.handle_key(key_event(KeyCode::Char('3')));
        let _ = inspector.handle_key(key_event(KeyCode::Right));
        let _ = inspector.handle_key(key_event(KeyCode::Enter));
        let _ = inspector.handle_key(key_event(KeyCode::Down));

        let action = inspector.handle_key(key_event(KeyCode::Enter));

        let (text, starts, items) = section_of(&inspector);
        assert_eq!(
            action,
            InspectorAction::Consumed,
            "{FOLD_HIDES_THE_SUBTREE}"
        );
        assert!(
            text.iter().any(|line| line.contains(RESULT_HEADING)),
            "{CALL_STAYS_OPEN}: {text:?}"
        );
        assert!(
            text.iter().all(|line| !line.contains(FOLDED_KEY)),
            "{FOLD_HIDES_THE_SUBTREE}: {text:?}"
        );
        assert_eq!(starts, items, "{CURSOR_AND_ROWS_AGREE}: {text:?}");
    }

    /// A result the run wrote no report into is JSON, and a reader folds it
    /// rather than scrolling past it. The arrows walk its nodes, so the second
    /// node closes while the one holding it stays open.
    #[test]
    fn a_result_without_a_report_folds_as_a_tree() {
        let mut settled = run(RUN_ID, RunStatus::Completed, Vec::new());
        settled.result = Some(serde_json::from_str(JSON_RESULT).unwrap());
        let mut inspector = open_with(vec![settled]);
        let _ = inspector.handle_key(key_event(KeyCode::Char('4')));
        let _ = inspector.handle_key(key_event(KeyCode::Right));
        assert!(
            section_of(&inspector)
                .0
                .iter()
                .any(|line| line.contains(FOLDED_KEY)),
            "{JSON_IS_A_TREE}"
        );

        let _ = inspector.handle_key(key_event(KeyCode::Down));
        let _ = inspector.handle_key(key_event(KeyCode::Enter));

        let (text, starts, items) = section_of(&inspector);
        assert!(
            text.iter().any(|line| line.contains(OUTER_KEY)),
            "{OUTER_STAYS_OPEN}: {text:?}"
        );
        assert!(
            text.iter().all(|line| !line.contains(FOLDED_KEY)),
            "{FOLD_HIDES_THE_SUBTREE}: {text:?}"
        );
        assert_eq!(starts, items, "{CURSOR_AND_ROWS_AGREE}: {text:?}");
    }

    /// Enter acts on the item the cursor names, so the mark has to be drawn on
    /// the row that item starts. A body's nodes take positions of their own,
    /// which is exactly where the two can come apart.
    #[test]
    fn the_marked_row_is_the_row_the_cursor_names() {
        let (settled, journal) = json_call();
        let mut inspector = open_with(vec![settled]);
        inspector.fill_detail(journal);
        let _ = inspector.handle_key(key_event(KeyCode::Char('3')));
        let _ = inspector.handle_key(key_event(KeyCode::Right));
        let _ = inspector.handle_key(key_event(KeyCode::Enter));
        let _ = inspector.handle_key(key_event(KeyCode::Down));
        let _ = inspector.handle_key(key_event(KeyCode::Down));

        let (lines, starts) = inspector.section_lines(now_secs(), FRAME_WIDTH);

        let marked: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line_text(line).starts_with(CURSOR_MARK))
            .map(|(index, _)| index)
            .collect();
        assert_eq!(
            marked,
            vec![starts[inspector.cursor]],
            "{MARK_FOLLOWS_THE_CURSOR}"
        );
    }

    /// Where `needle` ends in `line`, in the columns a terminal draws it in.
    fn end_column(line: &str, needle: &str) -> Option<usize> {
        let byte = line.find(needle)? + needle.len();
        Some(UnicodeWidthStr::width(&line[..byte]))
    }

    /// A grid laid out for a wide pane wraps in a narrow one, and a wrapped
    /// row costs the row below it, so a tight pane keeps the clock, the label
    /// and the duration and leaves the bar and the tallies out.
    #[test]
    fn a_tight_timeline_row_fits_the_pane_it_was_laid_out_for() {
        let (walked, journal) = timed_run();
        let mut inspector = open_with(vec![walked.clone()]);
        inspector.fill_detail(journal);

        let (tight, _) = inspector.timeline_lines(&walked, PHASE_SECS, TIGHT_PANE_COLS);
        let (wide, _) = inspector.timeline_lines(&walked, PHASE_SECS, FRAME_WIDTH);

        for line in &tight {
            let text = line_text(line);
            assert!(
                UnicodeWidthStr::width(text.as_str()) <= usize::from(TIGHT_PANE_COLS),
                "{TIGHT_ROW_FITS}: {text:?}"
            );
        }
        let wide: String = wide.iter().map(line_text).collect();
        assert!(wide.contains(ONE_AGENT), "{WIDE_ROW_TALLIES}: {wide}");
    }

    #[test]
    fn timeline_rows_end_their_duration_in_the_same_column() {
        let (walked, journal) = timed_run();
        let mut inspector = open_with(vec![walked.clone()]);
        inspector.fill_detail(journal);

        let (lines, _) = inspector.timeline_lines(&walked, PHASE_SECS, FRAME_WIDTH);

        let rows: Vec<String> = lines.iter().map(line_text).collect();
        let phase = end_column(&rows[0], PHASE_ELAPSED).expect(&rows[0]);
        let call = end_column(&rows[1], CALL_ELAPSED).expect(&rows[1]);
        assert_eq!(phase, call, "{COLUMNS_LINE_UP}: {rows:?}");
    }

    /// A journal with more rows than the body can show, drawn once so the
    /// section has its geometry and the cursor is on a row of it.
    fn tall_timeline(terminal: &mut Terminal<TestBackend>) -> WorkflowInspector {
        let mut inspector = open_with(vec![run(RUN_ID, RunStatus::Active, vec![agent(None)])]);
        let walking = inspector.selected_run().cloned().expect(NO_SUCH_ROW);
        let events = (0..TALL_ROSTER)
            .map(|index| RunEvent {
                seq: index,
                at: index,
                kind: RunEventKind::Log,
                text: format!("{LOG_LINE} {index}"),
            })
            .collect();
        inspector.fill_detail(RunDetail {
            events,
            ..detail(walking)
        });
        let _ = inspector.handle_key(key_event(KeyCode::Char('2')));
        let _ = inspector.handle_key(key_event(KeyCode::Right));
        let _ = inspector.handle_key(key_event(KeyCode::Down));
        draw(&mut inspector, terminal);
        inspector
    }

    #[test]
    fn a_scroll_key_leaves_the_timeline_where_it_put_it() {
        let mut terminal = terminal();
        let mut inspector = tall_timeline(&mut terminal);
        let before = inspector.scroll.offset();

        let _ = inspector.handle_key(key_event(KeyCode::PageDown));
        draw(&mut inspector, &mut terminal);

        assert!(inspector.scroll.offset() > before, "{SCROLL_STICKS}");
    }

    #[test]
    fn the_wheel_leaves_the_timeline_where_it_put_it() {
        let mut terminal = terminal();
        let mut inspector = tall_timeline(&mut terminal);
        let before = inspector.scroll.offset();
        let over_body = Position::new(inspector.body_area.x, inspector.body_area.y);

        let _ = inspector.scroll_at(over_body, -WHEEL_ROWS);
        draw(&mut inspector, &mut terminal);

        assert!(inspector.scroll.offset() > before, "{SCROLL_STICKS}");
    }

    /// The section follows its tail, so the cursor is a place in the list
    /// and not a leash on the view: one move of it must not pin every frame
    /// after to the row it landed on.
    #[test]
    fn a_live_timeline_follows_its_tail_after_the_cursor_moves() {
        let mut terminal = terminal();
        let mut inspector = tall_timeline(&mut terminal);

        draw(&mut inspector, &mut terminal);

        let screen = buffer_text(terminal.backend().buffer());
        let last = format!("{LOG_LINE} {}", TALL_ROSTER - 1);
        assert!(screen.contains(&last), "{TAIL_IS_FOLLOWED}: {screen}");
    }

    fn press_bar(inspector: &mut WorkflowInspector, row: u16) {
        let column = inspector.body_area.x + inspector.body_area.width - 1;
        let _ = inspector.handle_mouse(mouse_at(
            MouseEventKind::Down(MouseButton::Left),
            column,
            row,
        ));
    }

    /// Pressing the head of the track lands the view on the first row, so the
    /// chip carries a reading nothing else on the frame can be showing.
    #[test]
    fn a_held_bar_says_where_in_the_section_the_view_is() {
        let mut terminal = terminal();
        let mut inspector = tall_timeline(&mut terminal);

        let head = inspector.body_area.y;
        press_bar(&mut inspector, head);
        draw(&mut inspector, &mut terminal);

        let screen = buffer_text(terminal.backend().buffer());
        assert!(screen.contains(POSITION_HINT), "{HINT_IS_SHOWN}: {screen}");
    }

    fn scroll_by_key(inspector: &mut WorkflowInspector) {
        let _ = inspector.handle_key(key_event(KeyCode::PageUp));
    }

    fn scroll_by_wheel(inspector: &mut WorkflowInspector) {
        let over_body = Position::new(inspector.body_area.x, inspector.body_area.y);
        let _ = inspector.scroll_at(over_body, WHEEL_ROWS);
    }

    fn scroll_by_bar(inspector: &mut WorkflowInspector) {
        press_bar(
            inspector,
            inspector.body_area.y + inspector.body_area.height - 1,
        );
    }

    /// Input arrives in batches and the frame is drawn once for all of it, so
    /// a scroll and a cursor move can land on the same frame. The scroll is
    /// the instruction about the view, and it is the later one.
    #[test_case(scroll_by_key ; "a scroll key")]
    #[test_case(scroll_by_wheel ; "the wheel")]
    #[test_case(scroll_by_bar ; "the bar")]
    fn a_scroll_outranks_a_cursor_move_in_the_frame_they_share(scroll: fn(&mut WorkflowInspector)) {
        let mut terminal = terminal();
        let mut inspector = tall_timeline(&mut terminal);
        draw(&mut inspector, &mut terminal);
        let _ = inspector.handle_key(key_event(KeyCode::Down));

        scroll(&mut inspector);
        let scrolled = inspector.scroll.offset();
        draw(&mut inspector, &mut terminal);

        assert_eq!(inspector.scroll.offset(), scrolled, "{SCROLL_OUTRANKS}");
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
        walking.phase_history = vec![
            PhaseRecord {
                title: PHASE_ONE.into(),
                started_at: 0,
            },
            PhaseRecord {
                title: PHASE_TWO.into(),
                started_at: 1,
            },
        ];
        walking
    }

    /// The timeline is the journal joined to the snapshot, so a run with no
    /// calls still needs one before it has any rows to walk.
    fn walked(run: RunSnapshot) -> RunDetail {
        RunDetail {
            run,
            calls: Vec::new(),
            events: Vec::new(),
            journal_trimmed: false,
        }
    }

    /// Puts the cursor on the second phase row of the timeline.
    fn walk_to_second_phase(inspector: &mut WorkflowInspector) {
        let run = inspector.selected_run().cloned().expect(NO_SUCH_ROW);
        inspector.fill_detail(walked(run));
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
        assert_eq!(inspector.section, Section::Timeline);
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

        assert!(text.contains(RUNNING_LABEL), "{text}");
        assert!(text.contains(TOOL_SUMMARY), "{text}");
        assert!(text.contains(TOOLS_TALLY), "{ACTIVITY_IS_TALLIED}: {text}");
    }

    #[test]
    fn an_agent_that_stopped_keeps_its_last_activity() {
        let mut inspector = open_with(vec![fanned_out()]);
        inspector.set_progress(RUN_ID, EARLY_KEY, tool_progress());
        let mut settled = fanned_out();
        for agent in &mut settled.roster {
            agent.state = RosterState::Completed;
        }

        let _ = inspector.refresh(vec![settled]);
        let text = section_text(&mut inspector, '3');

        assert!(
            text.contains(RUNNING_LABEL),
            "{ACTIVITY_OUTLIVES_ITS_AGENT}: {text}"
        );
    }

    #[test_case(Some(SCRIPT_PATH) => InspectorAction::OpenFile(PathBuf::from(SCRIPT_PATH)); "a_script_from_a_file_opens")]
    #[test_case(None => InspectorAction::Flash(NO_SCRIPT); "a_builtin_says_it_has_no_file")]
    fn the_script_key_opens_what_the_run_executed(path: Option<&str>) -> InspectorAction {
        let mut settled = run(RUN_ID, RunStatus::Completed, Vec::new());
        settled.source_path = path.map(str::to_owned);
        let mut inspector = open_with(vec![settled]);

        inspector.handle_key(key_event(KeyCode::Char(SCRIPT_KEY)))
    }

    #[test]
    fn exporting_asks_for_every_body_then_copies_the_run() {
        let mut inspector = open_with(vec![run(RUN_ID, RunStatus::Completed, Vec::new())]);
        inspector.fill_detail(detail(run(RUN_ID, RunStatus::Completed, Vec::new())));

        let asked = inspector.handle_key(key_event(KeyCode::Char(EXPORT_KEY)));
        let copied = inspector.fill_call_bodies(RUN_ID, None, vec![body(1)]);

        assert_eq!(
            asked,
            InspectorAction::LoadCallBody {
                run_id: RUN_ID.into(),
                call_key: None,
            },
            "{EXPORT_ASKS_FIRST}"
        );
        match copied {
            Some(InspectorAction::Copy { text, label }) => {
                assert_eq!(label, EXPORTED);
                assert!(
                    text.contains(CALL_LABEL),
                    "{EXPORT_IS_THE_WHOLE_RUN}: {text}"
                );
                assert!(
                    text.contains(FULL_PROMPT),
                    "{EXPORT_IS_THE_WHOLE_RUN}: {text}"
                );
                assert!(
                    text.contains(FULL_RESULT),
                    "{EXPORT_IS_THE_WHOLE_RUN}: {text}"
                );
            }
            other => panic!("{EXPORT_IS_THE_WHOLE_RUN}: {other:?}"),
        }
    }

    #[test]
    fn a_run_with_no_journal_has_nothing_to_export() {
        let mut inspector = open_with(vec![run(RUN_ID, RunStatus::Completed, Vec::new())]);

        let action = inspector.handle_key(key_event(KeyCode::Char(EXPORT_KEY)));

        assert_eq!(action, InspectorAction::Flash(NOTHING_TO_EXPORT));
    }

    #[test]
    fn the_transcript_key_reaches_an_agent_from_the_timeline_too() {
        let mut walking = run(RUN_ID, RunStatus::Active, vec![agent(Some(TASK_ID))]);
        walking.roster[0].call_key = 1;
        let mut inspector = open_with(vec![walking.clone()]);
        inspector.fill_detail(detail(walking));
        let _ = inspector.handle_key(key_event(KeyCode::Char('2')));

        let action = inspector.handle_key(key_event(KeyCode::Char(TRANSCRIPT_KEY)));

        assert_eq!(
            action,
            InspectorAction::OpenTranscript(TASK_ID.into()),
            "{TIMELINE_REACHES_THE_TRANSCRIPT}"
        );
    }

    #[test]
    fn an_agent_row_opens_the_journal_row_it_came_from() {
        let mut walking = run(RUN_ID, RunStatus::Active, vec![agent(Some(TASK_ID))]);
        walking.roster[0].call_key = 1;
        let mut inspector = open_with(vec![walking.clone()]);
        inspector.fill_detail(detail(walking));
        let _ = inspector.handle_key(key_event(KeyCode::Char('3')));

        let asked = inspector.handle_key(key_event(KeyCode::Enter));
        inspector.fill_call_bodies(RUN_ID, Some(1), vec![body(1)]);

        assert_eq!(
            asked,
            InspectorAction::LoadCallBody {
                run_id: RUN_ID.into(),
                call_key: Some(1),
            },
            "{ROSTER_JOINS_THE_JOURNAL}"
        );
        let text = section_text(&mut inspector, '3');
        assert!(
            text.contains(FULL_PROMPT),
            "{ROSTER_JOINS_THE_JOURNAL}: {text}"
        );
        assert!(
            text.contains(FULL_RESULT),
            "{ROSTER_JOINS_THE_JOURNAL}: {text}"
        );
    }

    #[test]
    fn a_run_that_left_the_list_takes_its_activity_with_it() {
        let mut inspector = open_with(vec![fanned_out()]);
        inspector.set_progress(RUN_ID, EARLY_KEY, tool_progress());

        let _ = inspector.refresh(vec![run(OTHER_RUN_ID, RunStatus::Active, Vec::new())]);

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
    fn the_transcript_key_opens_the_agent_under_the_cursor() {
        let mut inspector = open_with(vec![run(
            RUN_ID,
            RunStatus::Active,
            vec![agent(Some(TASK_ID))],
        )]);
        let _ = inspector.handle_key(key_event(KeyCode::Char('3')));
        assert_eq!(inspector.section(), Section::Agents);

        match inspector.handle_key(key_event(KeyCode::Char(TRANSCRIPT_KEY))) {
            InspectorAction::OpenTranscript(task_id) => assert_eq!(task_id, TASK_ID),
            action => panic!("{TRANSCRIPT}: {action:?}"),
        }
    }

    #[test]
    fn the_transcript_key_on_an_agent_without_one_says_so() {
        let mut inspector = open_with(vec![run(RUN_ID, RunStatus::Active, vec![agent(None)])]);
        let _ = inspector.handle_key(key_event(KeyCode::Char('3')));

        assert_eq!(
            inspector.handle_key(key_event(KeyCode::Char(TRANSCRIPT_KEY))),
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

    /// A report is markdown a model wrote, and the section that shows it is
    /// the one place a reader reads it in full, so it is painted rather than
    /// shown as source. The scratch row is targeted by the line it landed on,
    /// so painting must not leave the target pointing at prose.
    #[test]
    fn the_result_paints_the_report_and_keeps_the_scratch_row() {
        let mut settled = run(RUN_ID, RunStatus::Completed, Vec::new());
        settled.result = Some(serde_json::json!({
            REPORT_FIELD: MARKDOWN_REPORT,
            "path": SCRATCH_PATH,
        }));

        let inspector = open_with(vec![settled.clone()]);

        let (lines, starts) = result_lines(&inspector, &settled, FRAME_WIDTH);

        let painted: Vec<String> = lines.iter().map(line_text).collect();
        assert!(
            painted.contains(&PAINTED_HEADING.to_owned()),
            "{PAINTS_MARKDOWN}: {painted:?}"
        );
        assert!(
            painted.contains(&PAINTED_BULLET.to_owned()),
            "{PAINTS_MARKDOWN}: {painted:?}"
        );
        let scratch = starts.first().copied().expect(SCRATCH_IS_A_TARGET);
        assert!(
            painted[scratch].contains(SCRATCH_PATH),
            "{SCRATCH_IS_A_TARGET}: {painted:?}"
        );
    }

    /// Copying a section asks for no width, and a report with no columns to
    /// wrap into is the source the workflow wrote, not a painted rendering.
    #[test]
    fn a_report_with_no_width_stays_source() {
        let lines = report_lines(MARKDOWN_REPORT, 0);

        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert!(
            text.contains(&MARKDOWN_HEADING.to_owned()),
            "{KEEPS_SOURCE}: {text:?}"
        );
    }

    #[test]
    fn enter_on_the_result_opens_the_scratch_file() {
        let mut settled = run(RUN_ID, RunStatus::Completed, Vec::new());
        settled.result = Some(serde_json::json!({ "report": "done", "path": SCRATCH_PATH }));
        let mut inspector = open_with(vec![settled]);
        let _ = inspector.handle_key(key_event(KeyCode::Char('4')));

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
        let _ = inspector.handle_key(key_event(KeyCode::Char('4')));

        let action = inspector.handle_key(key_event(KeyCode::Enter));

        assert_eq!(action, InspectorAction::Consumed, "{NO_SCRATCH}");
    }

    #[test]
    fn enter_on_a_scratch_call_opens_its_file() {
        let mut inspector = open_with(vec![run(RUN_ID, RunStatus::Completed, Vec::new())]);
        let mut with_scratch = detail(run(RUN_ID, RunStatus::Completed, Vec::new()));
        with_scratch.calls.push(scratch_call());
        inspector.fill_detail(with_scratch);
        let _ = inspector.handle_key(key_event(KeyCode::Char('2')));
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
        let _ = inspector.handle_key(key_event(KeyCode::Char('2')));

        let _ = inspector.handle_key(key_event(KeyCode::Enter));
        assert_eq!(inspector.expanded_call, Some(1));
        let _ = inspector.handle_key(key_event(KeyCode::Enter));
        assert_eq!(inspector.expanded_call, None);
    }

    #[test]
    fn an_opened_call_asks_for_its_body_once() {
        let mut inspector = open_with(vec![run(RUN_ID, RunStatus::Active, Vec::new())]);
        inspector.fill_detail(detail(run(RUN_ID, RunStatus::Active, Vec::new())));
        let _ = inspector.handle_key(key_event(KeyCode::Char('2')));

        let asked = inspector.handle_key(key_event(KeyCode::Enter));

        assert_eq!(
            asked,
            InspectorAction::LoadCallBody {
                run_id: RUN_ID.into(),
                call_key: Some(1),
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
        let _ = inspector.handle_key(key_event(KeyCode::Char('2')));
        let _ = inspector.handle_key(key_event(KeyCode::Enter));

        inspector.fill_call_bodies(RUN_ID, Some(1), vec![body(1)]);

        let text = section_text(&mut inspector, '2');
        assert!(text.contains(FULL_PROMPT), "{BODY_WINS}");
        assert!(text.contains(FULL_RESULT), "{BODY_WINS}");
        assert!(!text.contains(PROMPT_PREVIEW), "{BODY_WINS}");
    }

    #[test]
    fn a_body_for_another_run_is_dropped() {
        let mut inspector = open_with(vec![run(RUN_ID, RunStatus::Active, Vec::new())]);
        inspector.fill_detail(detail(run(RUN_ID, RunStatus::Active, Vec::new())));
        let _ = inspector.handle_key(key_event(KeyCode::Char('2')));
        let _ = inspector.handle_key(key_event(KeyCode::Enter));

        inspector.fill_call_bodies(OTHER_RUN_ID, Some(1), vec![body(1)]);

        assert!(
            !section_text(&mut inspector, '2').contains(FULL_RESULT),
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
        let _ = inspector.handle_key(key_event(KeyCode::Char('2')));
        let _ = inspector.handle_key(key_event(KeyCode::Enter));

        inspector.fill_call_bodies(RUN_ID, Some(1), Vec::new());

        assert!(section_text(&mut inspector, '2').contains(BODY_MISSING));
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
        assert_eq!(inspector.section(), Section::Timeline);
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
