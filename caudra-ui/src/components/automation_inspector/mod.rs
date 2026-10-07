//! `/automations`: the session's automations as the runtime mirrors them, in
//! one modal built like the workflow inspector. The list on the left holds the
//! session, its catalog and other sessions' automations, the right shows the
//! selection's sections, and the footer names the keys that act on it.
//!
//! It keeps no automation state of its own. The mirror arrives whole through
//! [`AutomationInspector::sync`], and what the mirror does not hold, such as a
//! script's committed state or a firing's trace, is asked for as the selection
//! moves and lands through [`AutomationInspector::apply_response`].
//!
//! Other sessions are listed once per opening and only read: every control
//! acts on this session, so none acts on their rows. The selected one is read
//! again on the clock [`AutomationInspector::poll`] is given.

mod dry_run;
mod editor;
mod firings;
mod list;
mod overview;
mod text;
mod trace;
pub(crate) mod transcript;

use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::mem;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use caudra_automation::catalog::Trust;
use caudra_automation::request::{
    AutomationError, AutomationRequest, AutomationResponse, DropTarget,
};
use caudra_automation::snapshot::{
    ActionStatus, ArmOrigin, AutomationDetail, AutomationHistoryEntry, AutomationSnapshot,
    AutomationState, DryRunDetail, FiringDetail, FiringSummary, PauseSource,
};
use caudra_automation::untrusted::UNTRUSTED_TAG;
use caudra_grab::grab_scope;
use caudra_workbench::text_field::{FieldKind, TextField, TextKey};
use caudra_workflow::RunSnapshot;
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use dry_run::DryRun;
use editor::Editor;
use list::{Entry, Selection};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use serde_json::Value;
use text::now_ms;
use trace::{Shown, Trace};

use crate::components::json_tree;
use crate::components::keybindings::key;
use crate::components::modal::{ESC_LABEL, FooterHits, FooterLine, Modal};
use crate::components::scrollbar::{ScrollHint, Scrollbar, ScrollbarMouse};
use crate::components::section_tabs::{SectionTab, tab_strip};
use crate::components::text_editor::{EditorKey, EditorMouse};
use crate::components::{
    ModalScroll, Overlay, ascii_key, chevron_span, escape_terminal_controls, field_styles,
    hover_style, input_text_style, plain_char, visual_rows,
};
use crate::repaint::Cadence;
use crate::theme;

const TITLE: &str = " Automations ";
const WIDTH_PERCENT: u16 = 90;
const MAX_HEIGHT_PERCENT: u16 = 85;
const LIST_MAX_WIDTH: u16 = 44;
const LIST_PERCENT: u16 = 38;
const PERCENT: u16 = 100;
const PANE_GAP: u16 = 1;
/// A list narrower than this cannot show a name with its scope, and a detail
/// pane narrower than this cannot show a firing row, so a modal too narrow for
/// both shows one at a time.
const LIST_MIN_COLS: u16 = 24;
const DETAIL_MIN_COLS: u16 = 48;
const SPLIT_MIN_COLS: u16 = LIST_MIN_COLS + PANE_GAP + DETAIL_MIN_COLS;
const H_PAD: u16 = 1;
const TABS_ROWS: u16 = 1;
const INPUT_ROWS: u16 = 1;
const FOOTER_ROWS: u16 = 1;
const LOADING: &str = "Loading\u{2026}";
const EMPTY_CATALOG: &str = "No automation scripts in this session's catalog";
const NO_MATCH: &str = "No automation matches";
const GONE: &str = "This automation left the catalog";
const OTHER_GONE: &str = "Its session no longer lists this automation";
/// How often another session's automation is read again while it stays
/// selected, and the live peer directory while the inspector is open.
pub(crate) const REREAD_EVERY: Duration = Duration::from_secs(5);
pub(crate) const ARM_LABEL: &str = "Space";
pub(crate) const EDIT_LABEL: &str = "e";
pub(crate) const TRUST_LABEL: &str = "t";
pub(crate) const PAUSE_LABEL: &str = "p";
pub(crate) const CLEAR_LABEL: &str = "c";
pub(crate) const DROP_LABEL: &str = "x";
pub(crate) const DRY_RUN_LABEL: &str = "r";
pub(crate) const SCRIPT_LABEL: &str = "o";
pub(crate) const COPY_LABEL: &str = "y";
pub(crate) const FILTER_LABEL: &str = "/";
const OPEN_LABEL: &str = "Enter";
const ARM_KEY: char = ' ';
const EDIT_KEY: char = ascii_key(EDIT_LABEL);
const TRUST_KEY: char = ascii_key(TRUST_LABEL);
const PAUSE_KEY: char = ascii_key(PAUSE_LABEL);
const CLEAR_KEY: char = ascii_key(CLEAR_LABEL);
const DROP_KEY: char = ascii_key(DROP_LABEL);
const DRY_RUN_KEY: char = ascii_key(DRY_RUN_LABEL);
const SCRIPT_KEY: char = ascii_key(SCRIPT_LABEL);
const COPY_KEY: char = ascii_key(COPY_LABEL);
const FILTER_KEY: char = ascii_key(FILTER_LABEL);
pub(crate) const NOT_AN_AUTOMATION: &str = "Select an automation first";
pub(crate) const EDIT_WHERE: &str = "e edits the state in State, or the args in Args";
pub(crate) const ALREADY_TRUSTED: &str = "This script is already trusted";
pub(crate) const STATE_LOADING: &str = "The state is still loading";
pub(crate) const NO_STATE_TO_CLEAR: &str = "There is no state to clear";
pub(crate) const NOTHING_TO_DROP: &str = "x drops a waiting firing or outbox item";
pub(crate) const NOTHING_TO_COPY: &str = "y copies the firing under the cursor";
const NOTHING_TO_REPLAY: &str = "r replays a finished firing as a dry run";
const DRY_RUN_BUSY: &str = "A dry run is already running";
pub(crate) const NO_SCRIPT: &str = "No script to open here";
const READ_ONLY: &str = "Another session's automation is read-only: these keys act on this session";
const TRUST_PROMPT: &str = "Trust ";
const DIGEST_PROMPT: &str = " at digest ";
const CLEAR_PROMPT: &str = "Clear the state of ";
const REVISION_PROMPT: &str = " at revision ";
const QUESTION: &str = "?";
const OVERVIEW_TAB: &str = "Overview";
const FIRINGS_TAB: &str = "Firings";
const OUTBOX_TAB: &str = "Outbox";
const STATE_TAB: &str = "State";
const ARGS_TAB: &str = "Args";
const CURSOR_MARK: &str = "\u{203a} ";
const NO_MARK: &str = "  ";
/// What a line nested under a row starts with: the row's mark column, then a
/// step in.
const UNDER_INDENT: &str = "    ";
const TREE_INDENT: &str = "  ";
const DISARM_WORD: &str = "Disarm";
const RESUME_WORD: &str = "Resume";
const BACK_WORD: &str = "Back";
const SECTION_GAP: &str = "  ";
/// What the footer puts between its keys once it has given up their words.
const KEY_GAP: &str = " ";
/// How the footer draws itself, widest first: glossed, then keys alone, then
/// keys packed. Every key is on every rung, because a key a reader cannot see
/// is a key they cannot press.
const FOOTER_RUNGS: [(bool, &str); 3] =
    [(true, SECTION_GAP), (false, SECTION_GAP), (false, KEY_GAP)];
const FOOTER: [(&str, &str, FooterCommand); 12] = [
    (OPEN_LABEL, "Open", FooterCommand::Open),
    (ARM_LABEL, "Arm", FooterCommand::Arm),
    (EDIT_LABEL, "Edit", FooterCommand::Edit),
    (TRUST_LABEL, "Trust", FooterCommand::Trust),
    (PAUSE_LABEL, "Pause", FooterCommand::Pause),
    (CLEAR_LABEL, "Clear", FooterCommand::Clear),
    (DROP_LABEL, "Drop", FooterCommand::Drop),
    (DRY_RUN_LABEL, "Dry run", FooterCommand::DryRun),
    (SCRIPT_LABEL, "Script", FooterCommand::Script),
    (COPY_LABEL, "Copy", FooterCommand::Copy),
    (FILTER_LABEL, "Filter", FooterCommand::Filter),
    (ESC_LABEL, "Close", FooterCommand::Close),
];
const EDITOR_FOOTER: [(&str, &str, FooterCommand); 2] = [
    (key::SAVE.label, "Save", FooterCommand::Save),
    (ESC_LABEL, "Cancel", FooterCommand::Cancel),
];
const CONFIRM_FOOTER: [(&str, &str, FooterCommand); 2] = [
    (OPEN_LABEL, "Confirm", FooterCommand::Confirm),
    (ESC_LABEL, "Cancel", FooterCommand::Cancel),
];

/// What the app does after the inspector took an input.
#[must_use]
#[derive(Debug, PartialEq)]
pub enum AutomationAction {
    /// Nothing for the app to do.
    None,
    Close,
    /// A question or a control for the session's automation runtime. Its
    /// answer goes back through [`AutomationInspector::apply_response`].
    Request(AutomationRequest),
    /// The script to open in the workbench, at a line when one is known.
    OpenScript {
        path: PathBuf,
        line: Option<u32>,
    },
    /// Text for the clipboard: a firing as Markdown, or a selection.
    Copy(String),
    /// The filter's selection went to the clipboard, and the question the
    /// shorter filter raised when it moved the selection.
    Cut {
        text: String,
        request: Option<AutomationRequest>,
    },
    Flash(String),
    /// The editor left the key to the app: `Ctrl+C` with nothing selected.
    Passthrough,
    /// A run a `start_workflow` started, to open in the workflow inspector.
    OpenWorkflowRun(String),
}

/// What a session's selection shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionSection {
    Overview,
    Firings,
    Outbox,
}

impl SectionTab for SessionSection {
    const ALL: &'static [Self] = &[Self::Overview, Self::Firings, Self::Outbox];

    fn label(self) -> &'static str {
        match self {
            Self::Overview => OVERVIEW_TAB,
            Self::Firings => FIRINGS_TAB,
            Self::Outbox => OUTBOX_TAB,
        }
    }
}

/// What a script's selection shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutomationSection {
    Overview,
    Firings,
    State,
    Args,
}

impl SectionTab for AutomationSection {
    const ALL: &'static [Self] = &[Self::Overview, Self::Firings, Self::State, Self::Args];

    fn label(self) -> &'static str {
        match self {
            Self::Overview => OVERVIEW_TAB,
            Self::Firings => FIRINGS_TAB,
            Self::State => STATE_TAB,
            Self::Args => ARGS_TAB,
        }
    }
}

/// What another session's automation shows. It is only read, so it has no
/// Args to edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OtherSection {
    Overview,
    Firings,
    State,
}

impl SectionTab for OtherSection {
    const ALL: &'static [Self] = &[Self::Overview, Self::Firings, Self::State];

    fn label(self) -> &'static str {
        match self {
            Self::Overview => OVERVIEW_TAB,
            Self::Firings => FIRINGS_TAB,
            Self::State => STATE_TAB,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pane {
    List,
    Detail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FooterCommand {
    Open,
    Arm,
    Edit,
    Trust,
    Pause,
    Clear,
    Drop,
    DryRun,
    Script,
    Copy,
    Filter,
    Close,
    Save,
    Confirm,
    Cancel,
}

impl FooterCommand {
    /// Whether the command acts on this session's automations, runs this
    /// session's script, or opens it, rather than reading what is on screen.
    fn controls(self) -> bool {
        matches!(
            self,
            Self::Arm
                | Self::Edit
                | Self::Trust
                | Self::Pause
                | Self::Clear
                | Self::Drop
                | Self::DryRun
                | Self::Script
        )
    }
}

/// A key that acts only once Enter confirms it. What it acts on is fixed when
/// it asks, so a refresh landing in between cannot change what was agreed to.
enum Confirm {
    Trust {
        name: String,
        digest: String,
    },
    Clear {
        name: String,
        expected_revision: u64,
    },
}

impl Confirm {
    fn prompt(&self) -> Vec<Span<'static>> {
        let t = theme::current();
        let (verb, name, label, value) = match self {
            Self::Trust { name, digest } => (TRUST_PROMPT, name, DIGEST_PROMPT, digest.clone()),
            Self::Clear {
                name,
                expected_revision,
            } => (
                CLEAR_PROMPT,
                name,
                REVISION_PROMPT,
                expected_revision.to_string(),
            ),
        };
        vec![
            Span::styled(verb, t.tool_dim),
            Span::styled(escape_terminal_controls(name), t.bold),
            Span::styled(label, t.tool_dim),
            Span::styled(value, t.accent),
            Span::styled(QUESTION, t.tool_dim),
        ]
    }

    fn request(self) -> AutomationRequest {
        match self {
            Self::Trust { name, digest } => AutomationRequest::Trust { name, digest },
            Self::Clear {
                name,
                expected_revision,
            } => AutomationRequest::ClearState {
                name,
                expected_revision,
            },
        }
    }
}

/// What `y` copies as Markdown: a firing, with its trace once that loaded,
/// or a dry run that landed.
enum Copied<'a> {
    Firing(&'a FiringSummary, Option<&'a FiringDetail>),
    DryRun(&'a DryRun, &'a DryRunDetail),
}

/// Something asked of the runtime, from asked-for to readable.
#[derive(Debug)]
enum Loading<T> {
    Requested,
    Loaded(T),
    Failed(String),
}

/// What the cursor rests on in a section. Every one is drawn through
/// [`Body::item`] or [`Body::tree`], so the cursor, the marks and the click
/// targets all read the one walk that drew them.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Item {
    Firing(String),
    DryRun,
    Outbox { fire_id: String, seq: u64 },
    Action(u64),
    Fold(FoldScope, usize),
}

/// Which JSON a folded node belongs to. A node is named by where it sits in
/// its own value, and two values name their roots alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum FoldScope {
    Event,
    Patch,
    State,
    Request(u64),
    Result(u64),
}

/// A section as lines, and the line each of its items starts on.
struct Body {
    lines: Vec<Line<'static>>,
    items: Vec<(Item, usize)>,
    /// The item that draws the cursor mark, when the section has the focus.
    cursor: Option<usize>,
}

impl Body {
    fn new(cursor: Option<usize>) -> Self {
        Self {
            lines: Vec::new(),
            items: Vec::new(),
            cursor,
        }
    }

    /// `Label: value`, the label dim.
    fn field(&mut self, label: &'static str, value: impl Into<Cow<'static, str>>, style: Style) {
        self.lines.push(Line::from(vec![
            Span::styled(label, theme::current().tool_dim),
            Span::styled(value, style),
        ]));
    }

    fn text(&mut self, text: impl Into<Cow<'static, str>>, style: Style) {
        self.lines.push(Line::styled(text, style));
    }

    fn line(&mut self, spans: Vec<Span<'static>>) {
        self.lines.push(Line::from(spans));
    }

    /// What was asked for once it has landed; until then the line that says
    /// it is still loading, or why it failed to.
    fn landed<'a, T>(&mut self, asked: Option<&'a Loading<Box<T>>>) -> Option<&'a T> {
        let t = theme::current();
        match asked {
            Some(Loading::Loaded(detail)) => Some(detail),
            Some(Loading::Failed(error)) => {
                self.text(escape_terminal_controls(error), t.tool_error);
                None
            }
            Some(Loading::Requested) | None => {
                self.text(LOADING, t.tool_dim);
                None
            }
        }
    }

    /// A line nested under the one before it.
    fn under(&mut self, spans: Vec<Span<'static>>) {
        let mut line = Vec::with_capacity(spans.len() + 1);
        line.push(Span::raw(UNDER_INDENT));
        line.extend(spans);
        self.lines.push(Line::from(line));
    }

    /// A heading, a blank line clear of whatever came before it.
    fn heading(&mut self, text: impl Into<Cow<'static, str>>) {
        if !self.lines.is_empty() {
            self.lines.push(Line::default());
        }
        self.lines
            .push(Line::styled(text, theme::current().keybind_section));
    }

    /// A row the cursor can land on.
    fn item(&mut self, item: Item, spans: Vec<Span<'static>>) {
        let mut line = Vec::with_capacity(spans.len() + 1);
        line.push(self.mark());
        line.extend(spans);
        self.items.push((item, self.lines.len()));
        self.lines.push(Line::from(line));
    }

    /// The mark the next item draws.
    fn mark(&self) -> Span<'static> {
        match self.cursor == Some(self.items.len()) {
            true => Span::styled(CURSOR_MARK, theme::current().accent),
            false => Span::raw(NO_MARK),
        }
    }

    /// `value` as a tree whose containers fold, each container a row the
    /// cursor can land on. Its text is escaped, and the key that wraps an
    /// untrusted value is drawn as a warning.
    fn tree(&mut self, scope: FoldScope, value: &Value, folded: Option<&HashSet<usize>>) {
        let open = HashSet::new();
        let Some(rows) = json_tree::rows(value, folded.unwrap_or(&open)) else {
            self.lines.push(Line::from(vec![
                Span::raw(NO_MARK),
                Span::raw(TREE_INDENT),
                Span::styled(
                    escape_terminal_controls(&value.to_string()),
                    theme::current().tool,
                ),
            ]));
            return;
        };
        let untrusted = Value::String(UNTRUSTED_TAG.to_owned()).to_string();
        for row in rows {
            let lead = match row.fold {
                Some(_) => self.mark(),
                None => Span::raw(NO_MARK),
            };
            let mut line = vec![lead, Span::raw(TREE_INDENT)];
            line.extend(
                row.line
                    .spans
                    .into_iter()
                    .map(|span| guarded(span, &untrusted)),
            );
            if let Some(node) = row.fold {
                self.items.push((Item::Fold(scope, node), self.lines.len()));
            }
            self.lines.push(Line::from(line));
        }
    }
}

/// A tree row's span with its text escaped, and drawn as a warning when it is
/// the key that marks an untrusted value.
fn guarded(span: Span<'static>, untrusted_key: &str) -> Span<'static> {
    let style = match span.content == untrusted_key {
        true => theme::current().tool_warning,
        false => span.style,
    };
    Span::styled(escape_terminal_controls(&span.content), style)
}

pub struct AutomationInspector {
    open: bool,
    state: Arc<AutomationState>,
    selection: Selection,
    session_section: SessionSection,
    automation_section: AutomationSection,
    other_section: OtherSection,
    /// The selected script's binding, state and older firings.
    detail: Option<Loading<Box<AutomationDetail>>>,
    /// Other sessions with automations, newest activity first, as this
    /// opening's list had them.
    sessions: Vec<AutomationHistoryEntry>,
    /// The sessions the live peer directory lists, which their rows mark
    /// online. Empty while messaging is off.
    online: HashSet<String>,
    /// When the selected other session's automation was last read, by the
    /// clock [`Self::poll`] is given: unset until the first poll after the
    /// selection, which follows it within a frame.
    read_at: Option<Instant>,
    trace: Option<Trace>,
    /// The dry run `r` asked for, held only while the selection stays.
    dry_run: Option<DryRun>,
    editor: Option<Editor>,
    confirm: Option<Confirm>,
    filter: TextField,
    filter_focused: bool,
    pane: Pane,
    cursor: usize,
    /// The JSON nodes a reader closed, by the value they belong to. Dropped
    /// with the selection, and the trace's with the trace.
    folded: HashMap<FoldScope, HashSet<usize>>,
    scroll: ModalScroll,
    scrollbar: Scrollbar,
    list_offset: u16,
    popup: Rect,
    list_area: Rect,
    body_area: Rect,
    /// Where the list's rows and the body's items landed, as last drawn, so
    /// a click can name what it hit.
    list_rows: Vec<(u16, Selection)>,
    item_rows: Vec<(u16, u16)>,
    tab_hits: Vec<(Rect, usize)>,
    pointer: Option<Position>,
    /// A key moved the cursor, so the next draw scrolls it into view.
    reveal_cursor: bool,
    footer: FooterLine,
    footer_hits: FooterHits,
}

impl AutomationInspector {
    pub fn new() -> Self {
        Self {
            open: false,
            state: Arc::default(),
            selection: Selection::Session,
            session_section: SessionSection::Overview,
            automation_section: AutomationSection::Overview,
            other_section: OtherSection::Overview,
            detail: None,
            sessions: Vec::new(),
            online: HashSet::new(),
            read_at: None,
            trace: None,
            dry_run: None,
            editor: None,
            confirm: None,
            filter: TextField::new(FieldKind::Line),
            filter_focused: false,
            pane: Pane::List,
            cursor: 0,
            folded: HashMap::new(),
            scroll: ModalScroll::new_top(),
            scrollbar: Scrollbar::default(),
            list_offset: 0,
            popup: Rect::default(),
            list_area: Rect::default(),
            body_area: Rect::default(),
            list_rows: Vec::new(),
            item_rows: Vec::new(),
            tab_hits: Vec::new(),
            pointer: None,
            reveal_cursor: false,
            footer: FooterLine::default(),
            footer_hits: FooterHits::default(),
        }
    }

    /// Opens on `focus` when it names a script of the catalog, or on that
    /// firing's trace when it names a firing; else on the session. What to ask
    /// the runtime for straight away: other sessions' automations, and what
    /// `focus` needs.
    pub fn open(
        &mut self,
        state: Arc<AutomationState>,
        focus: Option<&str>,
    ) -> Vec<AutomationRequest> {
        self.open = true;
        self.state = state;
        self.selection = Selection::Session;
        self.session_section = SessionSection::Overview;
        self.automation_section = AutomationSection::Overview;
        self.other_section = OtherSection::Overview;
        self.detail = None;
        self.sessions.clear();
        self.online.clear();
        self.trace = None;
        self.dry_run = None;
        self.editor = None;
        self.confirm = None;
        self.filter.clear();
        self.filter_focused = false;
        self.pane = Pane::List;
        self.cursor = 0;
        self.folded.clear();
        self.scroll = ModalScroll::new_top();
        self.list_offset = 0;
        self.pointer = None;
        self.reveal_cursor = false;
        self.footer_hits.reset();
        let mut requests = vec![AutomationRequest::Sessions { limit: None }];
        let Some(focus) = focus else {
            return requests;
        };
        if self.state.find(focus).is_some() {
            requests.extend(requested(
                self.select(Selection::Automation(focus.to_owned())),
            ));
            return requests;
        }
        let owner = self
            .state
            .recent
            .iter()
            .find(|firing| firing.fire_id == focus)
            .map(|firing| firing.automation.clone())
            .filter(|name| self.state.find(name).is_some());
        if let Some(name) = owner {
            requests.extend(requested(self.select(Selection::Automation(name))));
        }
        self.pane = Pane::Detail;
        requests.push(self.open_trace(focus.to_owned()));
        requests
    }

    /// Adopts the runtime's mirror. What to ask again: the selected script's
    /// detail when the script moved, and the open trace when its firing or the
    /// deliveries it waits on did.
    pub fn sync(&mut self, state: Arc<AutomationState>) -> Vec<AutomationRequest> {
        let previous = mem::replace(&mut self.state, state);
        if !self.open {
            return Vec::new();
        }
        let mut requests = requested(self.settle_selection());
        if let Selection::Automation(name) = &self.selection
            && requests.is_empty()
            && !matches!(self.detail, Some(Loading::Requested))
            && previous.find(name) != self.state.find(name)
        {
            requests.push(AutomationRequest::Inspect {
                name: name.clone(),
                session_id: None,
            });
        }
        if let Some(trace) = &self.trace
            && !matches!(trace.detail, Loading::Requested)
            && (known_firing(&previous, &trace.fire_id)
                != known_firing(&self.state, &trace.fire_id)
                || (previous.outbox != self.state.outbox && waits_for_delivery(trace)))
        {
            requests.push(AutomationRequest::Firing {
                fire_id: trace.fire_id.clone(),
            });
        }
        self.clamp_cursor();
        requests
    }

    /// What to read again at `now`: the selected automation of another
    /// session, once [`REREAD_EVERY`] has passed since it was last read.
    /// Selecting it read it, so the first call after that only starts the
    /// clock.
    pub fn poll(&mut self, now: Instant) -> Option<AutomationRequest> {
        if !self.open || !self.read_only() {
            return None;
        }
        let read_at = *self.read_at.get_or_insert(now);
        if now.saturating_duration_since(read_at) < REREAD_EVERY {
            return None;
        }
        self.read_at = Some(now);
        self.selection.inspect()
    }

    /// The sessions the live peer directory lists, which their rows mark
    /// online. None while messaging is off.
    pub fn set_online(&mut self, sessions: HashSet<String>) {
        self.online = sessions;
    }

    /// The runtime's answer to `request`. What to do about it: ask for the
    /// state again after a write, or flash a refusal that has nowhere else
    /// to show.
    pub fn apply_response(
        &mut self,
        request: &AutomationRequest,
        response: Result<AutomationResponse, AutomationError>,
    ) -> AutomationAction {
        if !self.open {
            return AutomationAction::None;
        }
        let action = match (request, response) {
            (
                AutomationRequest::Inspect { name, session_id },
                Ok(AutomationResponse::Detail(detail)),
            ) => {
                self.fill_detail(name, session_id.as_deref(), Loading::Loaded(detail));
                AutomationAction::None
            }
            (AutomationRequest::Inspect { name, session_id }, Err(error)) => {
                self.fill_detail(
                    name,
                    session_id.as_deref(),
                    Loading::Failed(error.to_string()),
                );
                AutomationAction::None
            }
            (AutomationRequest::Sessions { .. }, Ok(AutomationResponse::Sessions(sessions))) => {
                self.sessions = sessions;
                self.settle_selection()
            }
            (AutomationRequest::Firing { fire_id }, Ok(AutomationResponse::Firing(detail))) => {
                if let Some(trace) = self.trace_of(fire_id) {
                    trace.detail = Loading::Loaded(detail);
                }
                AutomationAction::None
            }
            (AutomationRequest::Firing { fire_id }, Err(error)) => {
                if let Some(trace) = self.trace_of(fire_id) {
                    trace.detail = Loading::Failed(error.to_string());
                }
                AutomationAction::None
            }
            (
                AutomationRequest::ActionBody { fire_id, seq },
                Ok(AutomationResponse::ActionBody(body)),
            ) => {
                if let Some(trace) = self.trace_of(fire_id) {
                    trace.bodies.insert(*seq, Loading::Loaded(body));
                }
                AutomationAction::None
            }
            (AutomationRequest::ActionBody { fire_id, seq }, Err(error)) => {
                if let Some(trace) = self.trace_of(fire_id) {
                    trace
                        .bodies
                        .insert(*seq, Loading::Failed(error.to_string()));
                }
                AutomationAction::None
            }
            (AutomationRequest::DryRun { fire_id }, Ok(AutomationResponse::DryRun(replay))) => {
                if let Some(dry_run) = self.dry_run_of(fire_id) {
                    dry_run.result = Loading::Loaded(replay);
                }
                AutomationAction::None
            }
            (AutomationRequest::DryRun { fire_id }, Err(error)) => {
                if let Some(dry_run) = self.dry_run_of(fire_id) {
                    dry_run.result = Loading::Failed(dry_run::refusal(error));
                }
                AutomationAction::None
            }
            (AutomationRequest::SetState { name, .. }, Ok(_)) => {
                if self.editor_of(name, true).is_some() {
                    self.editor = None;
                }
                self.reinspect(name)
            }
            (
                AutomationRequest::SetState { name, .. },
                Err(AutomationError::StateConflict { current, .. }),
            ) => {
                if let Some(editor) = self.editor_of(name, true) {
                    editor.conflicted(current);
                }
                self.reinspect(name)
            }
            (AutomationRequest::SetArgs { name, .. }, Ok(_)) => {
                if self.editor_of(name, false).is_some() {
                    self.editor = None;
                }
                AutomationAction::None
            }
            (AutomationRequest::SetState { name, .. }, Err(error)) => {
                self.refuse_edit(name, true, error)
            }
            (AutomationRequest::SetArgs { name, .. }, Err(error)) => {
                self.refuse_edit(name, false, error)
            }
            (AutomationRequest::ClearState { name, .. }, Ok(_)) => self.reinspect(name),
            (_, Err(error)) => AutomationAction::Flash(error.to_string()),
            (_, Ok(_)) => AutomationAction::None,
        };
        self.clamp_cursor();
        action
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn close(&mut self) {
        self.open = false;
        self.detail = None;
        self.trace = None;
        self.dry_run = None;
        self.editor = None;
        self.confirm = None;
        self.folded.clear();
        self.list_rows.clear();
        self.item_rows.clear();
        self.tab_hits.clear();
        self.footer_hits.reset();
    }

    /// Whether a press at `pos` is the inspector's: inside it, or anywhere
    /// while an edit is open, so a stray press cannot throw the edit away.
    pub fn contains(&self, pos: Position) -> bool {
        self.open && (self.editor.is_some() || self.popup.contains(pos))
    }

    /// Whether keys are typing into the filter or an editor.
    pub fn text_input_active(&self) -> bool {
        self.open && (self.filter_focused || self.editor.is_some())
    }

    /// A paste lands in the editor, or in the filter when it has the focus.
    /// `None` when the inspector did not take it.
    pub fn handle_paste(&mut self, text: &str) -> Option<AutomationAction> {
        if !self.open {
            return None;
        }
        if let Some(editor) = &mut self.editor {
            editor.handle_paste(text);
            return Some(AutomationAction::None);
        }
        if !self.filter_focused {
            return None;
        }
        self.filter.paste(text);
        Some(self.settle_selection())
    }

    /// The wheel over the list walks the selection; anywhere else it scrolls
    /// what is under it.
    pub fn scroll_at(&mut self, pos: Position, delta: i32) -> AutomationAction {
        if let Some(editor) = &mut self.editor {
            editor.scroll(delta);
            return AutomationAction::None;
        }
        if self.list_area.contains(pos) {
            return self.step_selection(-delta.signum() as isize);
        }
        self.scroll.scroll(delta);
        self.reveal_cursor = false;
        AutomationAction::None
    }

    /// `runs` is the session's workflow mirror, which Enter opens a started run from.
    pub fn handle_key(&mut self, key: KeyEvent, runs: &[RunSnapshot]) -> AutomationAction {
        if !self.open {
            return AutomationAction::None;
        }
        if self.editor.is_some() {
            return self.handle_editor_key(key);
        }
        if self.confirm.is_some() {
            return self.handle_confirm_key(key);
        }
        if self.filter_focused {
            return self.handle_filter_key(key);
        }
        match (key.code, plain_char(&key)) {
            (KeyCode::Esc, _) => return self.back(),
            (KeyCode::Tab, _) => self.step_tab(1),
            (KeyCode::BackTab, _) => self.step_tab(-1),
            (KeyCode::Left, _) => self.pane = Pane::List,
            (KeyCode::Right, _) => self.pane = Pane::Detail,
            (KeyCode::Up, _) => return self.step(-1),
            (KeyCode::Down, _) => return self.step(1),
            (KeyCode::Enter, _) => return self.activate(runs),
            (_, Some(digit)) if digit.is_ascii_digit() => {
                if let Some(index) = digit
                    .to_digit(10)
                    .and_then(|number| (number as usize).checked_sub(1))
                {
                    self.set_tab(index);
                }
            }
            (_, Some(ARM_KEY)) => return self.command(FooterCommand::Arm, runs),
            (_, Some(EDIT_KEY)) => return self.command(FooterCommand::Edit, runs),
            (_, Some(TRUST_KEY)) => return self.command(FooterCommand::Trust, runs),
            (_, Some(PAUSE_KEY)) => return self.command(FooterCommand::Pause, runs),
            (_, Some(CLEAR_KEY)) => return self.command(FooterCommand::Clear, runs),
            (_, Some(DROP_KEY)) => return self.command(FooterCommand::Drop, runs),
            (_, Some(DRY_RUN_KEY)) => return self.command(FooterCommand::DryRun, runs),
            (_, Some(SCRIPT_KEY)) => return self.command(FooterCommand::Script, runs),
            (_, Some(COPY_KEY)) => return self.command(FooterCommand::Copy, runs),
            (_, Some(FILTER_KEY)) => return self.command(FooterCommand::Filter, runs),
            _ => {
                if self.scroll.handle_key(key) {
                    self.reveal_cursor = false;
                }
            }
        }
        AutomationAction::None
    }

    /// The editor owns every key but the two that leave it: save, and Esc,
    /// which drops the edit.
    fn handle_editor_key(&mut self, key: KeyEvent) -> AutomationAction {
        if key.code == KeyCode::Esc {
            self.editor = None;
            return AutomationAction::None;
        }
        if key::SAVE.matches(key) {
            return self.save();
        }
        let Some(editor) = &mut self.editor else {
            return AutomationAction::None;
        };
        match editor.handle_key(key) {
            EditorKey::Consumed => AutomationAction::None,
            EditorKey::Copy(text) => AutomationAction::Copy(text),
            EditorKey::Passthrough => AutomationAction::Passthrough,
        }
    }

    fn handle_confirm_key(&mut self, key: KeyEvent) -> AutomationAction {
        match key.code {
            KeyCode::Esc => {
                self.confirm = None;
                AutomationAction::None
            }
            KeyCode::Enter => self.confirm(),
            _ => AutomationAction::None,
        }
    }

    fn handle_filter_key(&mut self, key: KeyEvent) -> AutomationAction {
        match key.code {
            KeyCode::Esc => {
                self.filter_focused = false;
                self.filter.clear();
                self.settle_selection()
            }
            KeyCode::Enter => {
                self.filter_focused = false;
                AutomationAction::None
            }
            _ => match self.filter.handle_key(key) {
                TextKey::Changed => self.settle_selection(),
                TextKey::Copy(text) => AutomationAction::Copy(text),
                TextKey::Cut(text) => AutomationAction::Cut {
                    text,
                    request: requested(self.settle_selection()).pop(),
                },
                TextKey::Handled | TextKey::Refused | TextKey::Ignored => AutomationAction::None,
            },
        }
    }

    pub fn handle_mouse(&mut self, event: MouseEvent, runs: &[RunSnapshot]) -> AutomationAction {
        if !self.open {
            return AutomationAction::None;
        }
        if let Some(index) = self.footer_hits.handle_mouse(event) {
            return self.footer_command(index, runs);
        }
        if let Some(editor) = &mut self.editor {
            return match editor.handle_mouse(&event) {
                EditorMouse::Copy(text) => AutomationAction::Copy(text),
                EditorMouse::Consumed | EditorMouse::Passthrough => AutomationAction::None,
            };
        }
        match self.scrollbar.handle(&event) {
            ScrollbarMouse::Ignored => {}
            ScrollbarMouse::Consumed => return AutomationAction::None,
            ScrollbarMouse::ScrollTo(top) => {
                self.scroll
                    .scroll_to(u16::try_from(top).unwrap_or(u16::MAX));
                self.reveal_cursor = false;
                return AutomationAction::None;
            }
        }
        let pos = Position::new(event.column, event.row);
        self.pointer = Some(pos);
        let hovering = event.kind == MouseEventKind::Moved;
        if !hovering && event.kind != MouseEventKind::Down(MouseButton::Left) {
            return AutomationAction::None;
        }
        if let Some(&(_, index)) = self.tab_hits.iter().find(|(hit, _)| hit.contains(pos)) {
            if !hovering {
                self.set_tab(index);
            }
            return AutomationAction::None;
        }
        if self.list_area.contains(pos) {
            if hovering {
                return AutomationAction::None;
            }
            self.pane = Pane::List;
            let row = event.row - self.list_area.y + self.list_offset;
            let hit = self
                .list_rows
                .iter()
                .find(|(at, _)| *at == row)
                .map(|(_, selection)| selection.clone());
            return match hit {
                Some(selection) => self.select(selection),
                None => AutomationAction::None,
            };
        }
        if self.body_area.contains(pos) {
            let row = event.row - self.body_area.y + self.scroll.offset();
            let hit = self
                .item_rows
                .iter()
                .position(|(start, height)| (*start..start.saturating_add(*height)).contains(&row));
            // Hovering puts the cursor on the row under the pointer, so the
            // click that follows lands on the cursor and opens it.
            let Some(index) = hit else {
                return AutomationAction::None;
            };
            self.pane = Pane::Detail;
            if !hovering && index == self.cursor {
                return self.activate(runs);
            }
            self.cursor = index;
        }
        AutomationAction::None
    }

    fn footer_command(&mut self, index: usize, runs: &[RunSnapshot]) -> AutomationAction {
        match self.footer_entries().get(index) {
            Some(&(_, _, command)) => self.command(command, runs),
            None => AutomationAction::None,
        }
    }

    /// Runs `command` from its key or its footer entry. A control refuses
    /// while another session's automation is selected, since it would act on
    /// this session's.
    fn command(&mut self, command: FooterCommand, runs: &[RunSnapshot]) -> AutomationAction {
        if command.controls() && self.read_only() {
            return AutomationAction::Flash(READ_ONLY.to_owned());
        }
        match command {
            FooterCommand::Open => self.activate(runs),
            FooterCommand::Arm => self.arm(),
            FooterCommand::Edit => self.edit(),
            FooterCommand::Trust => self.ask_trust(),
            FooterCommand::Pause => self.pause(),
            FooterCommand::Clear => self.ask_clear(),
            FooterCommand::Drop => self.drop_item(),
            FooterCommand::DryRun => self.dry_run(),
            FooterCommand::Script => self.open_script(),
            FooterCommand::Copy => self.copy(runs),
            FooterCommand::Filter => {
                self.filter_focused = true;
                AutomationAction::None
            }
            FooterCommand::Close => self.back(),
            FooterCommand::Save => self.save(),
            FooterCommand::Confirm => self.confirm(),
            FooterCommand::Cancel => {
                self.editor = None;
                self.confirm = None;
                AutomationAction::None
            }
        }
    }

    /// Whether the selection is another session's automation, which nothing
    /// here may act on.
    fn read_only(&self) -> bool {
        matches!(self.selection, Selection::Other { .. })
    }

    fn footer_entries(&self) -> &'static [(&'static str, &'static str, FooterCommand)] {
        if self.editor.is_some() {
            &EDITOR_FOOTER
        } else if self.confirm.is_some() {
            &CONFIRM_FOOTER
        } else {
            &FOOTER
        }
    }

    /// Esc steps out of an open trace or dry run, and only then closes the
    /// inspector.
    fn back(&mut self) -> AutomationAction {
        let opened_from = match (self.trace.take(), &mut self.dry_run) {
            (Some(trace), _) => Item::Firing(trace.fire_id),
            (None, Some(dry_run)) if dry_run.open => {
                dry_run.open = false;
                Item::DryRun
            }
            (None, _) => return AutomationAction::Close,
        };
        self.folded.retain(|scope, _| *scope == FoldScope::State);
        self.cursor = self
            .items()
            .iter()
            .position(|item| *item == opened_from)
            .unwrap_or_default();
        self.scroll = ModalScroll::new_top();
        self.reveal_cursor = true;
        AutomationAction::None
    }

    fn tab_index(&self) -> usize {
        match self.selection {
            Selection::Session => self.session_section.index(),
            Selection::Automation(_) => self.automation_section.index(),
            Selection::Other { .. } => self.other_section.index(),
        }
    }

    fn tab_count(&self) -> usize {
        match self.selection {
            Selection::Session => SessionSection::ALL.len(),
            Selection::Automation(_) => AutomationSection::ALL.len(),
            Selection::Other { .. } => OtherSection::ALL.len(),
        }
    }

    fn step_tab(&mut self, delta: isize) {
        let count = self.tab_count() as isize;
        self.set_tab((self.tab_index() as isize + delta).rem_euclid(count) as usize);
    }

    /// Switches the selection's section, leaving any trace behind and the dry
    /// run closed.
    fn set_tab(&mut self, index: usize) {
        if index == self.tab_index() {
            return;
        }
        match self.selection {
            Selection::Session => match SessionSection::ALL.get(index) {
                Some(&section) => self.session_section = section,
                None => return,
            },
            Selection::Automation(_) => match AutomationSection::ALL.get(index) {
                Some(&section) => self.automation_section = section,
                None => return,
            },
            Selection::Other { .. } => match OtherSection::ALL.get(index) {
                Some(&section) => self.other_section = section,
                None => return,
            },
        }
        self.trace = None;
        if let Some(dry_run) = &mut self.dry_run {
            dry_run.open = false;
        }
        self.restart_view();
    }

    /// Puts what the section shows now at its top, the cursor on its first
    /// item, with only the state's folds kept.
    fn restart_view(&mut self) {
        self.folded.retain(|scope, _| *scope == FoldScope::State);
        self.cursor = 0;
        self.scroll = ModalScroll::new_top();
        self.footer_hits.clear();
    }

    /// Up and down walk whichever pane has the focus: the list, the section's
    /// items, or the section's text when it has none.
    fn step(&mut self, delta: isize) -> AutomationAction {
        if self.pane == Pane::List {
            return self.step_selection(delta);
        }
        let count = self.items().len();
        if count == 0 {
            self.scroll.scroll(-delta as i32);
            return AutomationAction::None;
        }
        self.cursor = (self.cursor as isize + delta).clamp(0, count as isize - 1) as usize;
        self.reveal_cursor = true;
        AutomationAction::None
    }

    fn step_selection(&mut self, delta: isize) -> AutomationAction {
        let selections: Vec<Selection> =
            list::entries(&self.state, &self.sessions, &self.filter.text())
                .iter()
                .map(Entry::selection)
                .collect();
        let at = selections
            .iter()
            .position(|selection| *selection == self.selection)
            .map_or(0, |at| {
                (at as isize + delta).clamp(0, selections.len() as isize - 1) as usize
            });
        match selections.into_iter().nth(at) {
            Some(selection) => self.select(selection),
            None => AutomationAction::None,
        }
    }

    /// Moves the selection, asking for an automation's detail as it lands on
    /// one.
    fn select(&mut self, selection: Selection) -> AutomationAction {
        if self.selection == selection {
            return AutomationAction::None;
        }
        self.selection = selection;
        self.detail = None;
        self.read_at = None;
        self.trace = None;
        self.dry_run = None;
        self.confirm = None;
        self.folded.clear();
        self.cursor = 0;
        self.scroll = ModalScroll::new_top();
        self.footer_hits.clear();
        let Some(request) = self.selection.inspect() else {
            return AutomationAction::None;
        };
        self.detail = Some(Loading::Requested);
        AutomationAction::Request(request)
    }

    /// Keeps the selection on a listed row: the same one when the filter, the
    /// catalog and the other sessions still list it, else the first script,
    /// else the session.
    fn settle_selection(&mut self) -> AutomationAction {
        let entries = list::entries(&self.state, &self.sessions, &self.filter.text());
        if entries
            .iter()
            .any(|entry| entry.selection() == self.selection)
        {
            return AutomationAction::None;
        }
        let next = entries
            .iter()
            .find(|entry| matches!(entry, Entry::Automation(_)))
            .map_or(Selection::Session, Entry::selection);
        self.select(next)
    }

    /// A started run that `runs` holds opens in the workflow inspector; any
    /// other action of a firing opens its request and result. A dry run's
    /// actions open nothing, since nothing stored what they asked or got.
    fn activate(&mut self, runs: &[RunSnapshot]) -> AutomationAction {
        if self.pane == Pane::List {
            self.pane = Pane::Detail;
            return AutomationAction::None;
        }
        match self.cursor_item() {
            Some(Item::Fold(scope, node)) => {
                let folded = self.folded.entry(scope).or_default();
                if !folded.remove(&node) {
                    folded.insert(node);
                }
                AutomationAction::None
            }
            Some(Item::Firing(fire_id) | Item::Outbox { fire_id, .. }) => {
                AutomationAction::Request(self.open_trace(fire_id))
            }
            Some(Item::DryRun) => {
                if let Some(dry_run) = &mut self.dry_run {
                    dry_run.open = true;
                }
                self.restart_view();
                AutomationAction::None
            }
            Some(Item::Action(seq)) => {
                let opened = self
                    .trace
                    .as_ref()
                    .and_then(|trace| trace.action(seq))
                    .and_then(trace::started_run)
                    .filter(|run_id| trace::mirrored(runs, run_id).is_some())
                    .map(str::to_owned);
                match opened {
                    Some(run_id) => AutomationAction::OpenWorkflowRun(run_id),
                    None => self.toggle_action(seq),
                }
            }
            None => AutomationAction::None,
        }
    }

    /// Opens a firing's trace in the selection's Firings section.
    fn open_trace(&mut self, fire_id: String) -> AutomationRequest {
        match &self.selection {
            Selection::Session => self.session_section = SessionSection::Firings,
            Selection::Automation(_) => self.automation_section = AutomationSection::Firings,
            Selection::Other { .. } => self.other_section = OtherSection::Firings,
        }
        self.trace = Some(Trace::new(fire_id.clone()));
        self.restart_view();
        AutomationRequest::Firing { fire_id }
    }

    /// Opens or closes an action's request and result, asking for them the
    /// first time, and again after a failed load.
    fn toggle_action(&mut self, seq: u64) -> AutomationAction {
        let Some(trace) = &mut self.trace else {
            return AutomationAction::None;
        };
        if trace.open_action == Some(seq) {
            trace.open_action = None;
            return AutomationAction::None;
        }
        trace.open_action = Some(seq);
        if matches!(
            trace.bodies.get(&seq),
            Some(Loading::Requested | Loading::Loaded(_))
        ) {
            return AutomationAction::None;
        }
        trace.bodies.insert(seq, Loading::Requested);
        AutomationAction::Request(AutomationRequest::ActionBody {
            fire_id: trace.fire_id.clone(),
            seq,
        })
    }

    fn arm(&self) -> AutomationAction {
        let Some(automation) = self.selected_automation() else {
            return AutomationAction::Flash(NOT_AN_AUTOMATION.to_owned());
        };
        let name = automation.name.clone();
        AutomationAction::Request(match automation.armed {
            Some(_) => AutomationRequest::Disarm { name },
            None => AutomationRequest::Arm {
                name,
                args: None,
                origin: ArmOrigin::Manual,
            },
        })
    }

    fn edit(&mut self) -> AutomationAction {
        let Some(automation) = self.selected_automation() else {
            return AutomationAction::Flash(NOT_AN_AUTOMATION.to_owned());
        };
        let editor = match (self.automation_section, &self.detail) {
            (AutomationSection::Args, _) => Editor::args(automation),
            (AutomationSection::State, Some(Loading::Loaded(detail))) => {
                if detail.binding.is_none() {
                    return AutomationAction::Flash(editor::NO_BINDING.to_owned());
                }
                Editor::state(automation.name.clone(), detail.state.as_ref())
            }
            (AutomationSection::State, Some(Loading::Failed(error))) => {
                return AutomationAction::Flash(error.clone());
            }
            (AutomationSection::State, _) => {
                return AutomationAction::Flash(STATE_LOADING.to_owned());
            }
            (AutomationSection::Overview | AutomationSection::Firings, _) => {
                return AutomationAction::Flash(EDIT_WHERE.to_owned());
            }
        };
        self.editor = Some(editor);
        self.pane = Pane::Detail;
        AutomationAction::None
    }

    fn save(&mut self) -> AutomationAction {
        match self.editor.as_mut().and_then(Editor::save) {
            Some(request) => AutomationAction::Request(request),
            None => AutomationAction::None,
        }
    }

    fn ask_trust(&mut self) -> AutomationAction {
        let Some(automation) = self.selected_automation() else {
            return AutomationAction::Flash(NOT_AN_AUTOMATION.to_owned());
        };
        if automation.trust != Trust::Required {
            return AutomationAction::Flash(ALREADY_TRUSTED.to_owned());
        }
        self.confirm = Some(Confirm::Trust {
            name: automation.name.clone(),
            digest: automation.digest.clone(),
        });
        AutomationAction::None
    }

    fn pause(&self) -> AutomationAction {
        AutomationAction::Request(match self.state.session.controls.pause {
            Some(_) => AutomationRequest::Resume,
            None => AutomationRequest::Pause {
                by: PauseSource::Inspector,
            },
        })
    }

    fn ask_clear(&mut self) -> AutomationAction {
        let Some(automation) = self.selected_automation() else {
            return AutomationAction::Flash(NOT_AN_AUTOMATION.to_owned());
        };
        let confirm = match &self.detail {
            Some(Loading::Loaded(detail)) => match &detail.state {
                Some(state) => Confirm::Clear {
                    name: automation.name.clone(),
                    expected_revision: state.revision,
                },
                None => return AutomationAction::Flash(NO_STATE_TO_CLEAR.to_owned()),
            },
            Some(Loading::Failed(error)) => return AutomationAction::Flash(error.clone()),
            Some(Loading::Requested) | None => {
                return AutomationAction::Flash(STATE_LOADING.to_owned());
            }
        };
        self.confirm = Some(confirm);
        AutomationAction::None
    }

    fn confirm(&mut self) -> AutomationAction {
        match self.confirm.take() {
            Some(confirm) => AutomationAction::Request(confirm.request()),
            None => AutomationAction::None,
        }
    }

    fn drop_item(&self) -> AutomationAction {
        match self.drop_target(self.cursor_item().as_ref()) {
            Some(target) => AutomationAction::Request(AutomationRequest::Drop(target)),
            None => AutomationAction::Flash(NOTHING_TO_DROP.to_owned()),
        }
    }

    /// Replays a finished firing, its dry run atop the Firings list with the
    /// cursor on it.
    fn dry_run(&mut self) -> AutomationAction {
        if self.dry_run.as_ref().is_some_and(DryRun::loading) {
            return AutomationAction::Flash(DRY_RUN_BUSY.to_owned());
        }
        let Some(replayed) = self.replayable(self.cursor_item().as_ref()) else {
            return AutomationAction::Flash(NOTHING_TO_REPLAY.to_owned());
        };
        let dry_run = DryRun::new(replayed);
        let fire_id = dry_run.fire_id.clone();
        self.dry_run = Some(dry_run);
        self.trace = None;
        self.restart_view();
        self.reveal_cursor = true;
        AutomationAction::Request(AutomationRequest::DryRun { fire_id })
    }

    fn open_script(&self) -> AutomationAction {
        match self.script(self.cursor_item().as_ref()) {
            Some((path, line)) => AutomationAction::OpenScript { path, line },
            None => AutomationAction::Flash(NO_SCRIPT.to_owned()),
        }
    }

    fn copy(&self, runs: &[RunSnapshot]) -> AutomationAction {
        let now = now_ms();
        match self.copy_source(self.cursor_item().as_ref()) {
            Some(Copied::Firing(firing, detail)) => {
                AutomationAction::Copy(trace::markdown(firing, detail, now, runs))
            }
            Some(Copied::DryRun(dry_run, replay)) => {
                AutomationAction::Copy(trace::dry_run_markdown(dry_run, replay, now, runs))
            }
            None => AutomationAction::Flash(NOTHING_TO_COPY.to_owned()),
        }
    }

    /// A queued or deferred firing, a waiting outbox item, or a trace's
    /// delivery that is still queued.
    fn drop_target(&self, item: Option<&Item>) -> Option<DropTarget> {
        match item? {
            Item::Firing(fire_id) => self
                .firing(fire_id)
                .filter(|firing| firings::is_waiting(firing.status))
                .map(|_| DropTarget::Firing {
                    fire_id: fire_id.clone(),
                }),
            Item::Outbox { fire_id, seq } => Some(DropTarget::OutboxItem {
                fire_id: fire_id.clone(),
                seq: *seq,
            }),
            Item::Action(seq) => {
                let trace = self.trace.as_ref()?;
                trace
                    .action(*seq)
                    .filter(|action| action.status == ActionStatus::Queued)
                    .map(|_| DropTarget::OutboxItem {
                        fire_id: trace.fire_id.clone(),
                        seq: *seq,
                    })
            }
            Item::DryRun | Item::Fold(..) => None,
        }
    }

    /// The finished firing `r` replays: the open trace's, else the firing row
    /// under the cursor.
    fn replayable(&self, item: Option<&Item>) -> Option<&FiringSummary> {
        let fire_id = match (&self.trace, item) {
            (Some(trace), _) => &trace.fire_id,
            (None, Some(Item::Firing(fire_id))) => fire_id,
            (None, _) => return None,
        };
        self.firing(fire_id)
            .filter(|firing| !firing.status.is_pending())
    }

    /// The script behind what the cursor is on, at the line that matters: an
    /// action's call, a failed firing's error, or none for the script itself.
    fn script(&self, item: Option<&Item>) -> Option<(PathBuf, Option<u32>)> {
        let shown = self.shown().and_then(Shown::loaded);
        let traced = shown.map(|detail| &detail.firing);
        let (owner, line) = match item {
            Some(Item::Action(seq)) => (
                traced,
                shown
                    .and_then(|detail| trace::action_of(detail, *seq))
                    .and_then(|action| action.line),
            ),
            Some(Item::Firing(fire_id) | Item::Outbox { fire_id, .. }) => {
                let firing = self.firing(fire_id);
                (firing, firing.and_then(error_line))
            }
            Some(Item::DryRun) => {
                let ran = self
                    .dry_run
                    .as_ref()
                    .and_then(DryRun::loaded)
                    .map(|replay| &replay.trace.firing);
                (ran, ran.and_then(error_line))
            }
            Some(Item::Fold(..)) | None => (traced, traced.and_then(error_line)),
        };
        let automation = match owner {
            Some(firing) => self.state.find(&firing.automation)?,
            None => self.selected_automation()?,
        };
        Some((automation.path.clone(), line))
    }

    /// The open trace's firing, with everything it loaded; the dry run, open
    /// or under the cursor, once it landed; else the firing row under the
    /// cursor.
    fn copy_source(&self, item: Option<&Item>) -> Option<Copied<'_>> {
        if let Some(trace) = &self.trace {
            return match trace.loaded() {
                Some(detail) => Some(Copied::Firing(&detail.firing, Some(detail))),
                None => self
                    .firing(&trace.fire_id)
                    .map(|firing| Copied::Firing(firing, None)),
            };
        }
        if let Some(dry_run) = self
            .dry_run
            .as_ref()
            .filter(|dry_run| dry_run.open || item == Some(&Item::DryRun))
        {
            return dry_run
                .loaded()
                .map(|replay| Copied::DryRun(dry_run, replay));
        }
        match item? {
            Item::Firing(fire_id) => self
                .firing(fire_id)
                .map(|firing| Copied::Firing(firing, None)),
            Item::DryRun | Item::Outbox { .. } | Item::Action(_) | Item::Fold(..) => None,
        }
    }

    /// The selected script of this session's catalog.
    fn selected_automation(&self) -> Option<&AutomationSnapshot> {
        match &self.selection {
            Selection::Automation(name) => self.state.find(name),
            Selection::Session | Selection::Other { .. } => None,
        }
    }

    fn loaded_detail(&self) -> Option<&AutomationDetail> {
        match &self.detail {
            Some(Loading::Loaded(detail)) => Some(detail),
            Some(Loading::Requested | Loading::Failed(_)) | None => None,
        }
    }

    /// The firings a Firings section lists, newest first: every script's for
    /// the session; the selected script's from the mirror, its last firing,
    /// and the older ones its detail brought; else another session's
    /// automation's, as its detail brought them.
    fn firings(&self) -> Vec<&FiringSummary> {
        let older = self
            .loaded_detail()
            .into_iter()
            .flat_map(|detail| detail.firings.iter());
        let mut firings: Vec<&FiringSummary> = match self.selected_automation() {
            None if self.read_only() => older.collect(),
            None => return self.state.recent.iter().collect(),
            Some(automation) => {
                let mut firings: Vec<&FiringSummary> = self
                    .state
                    .recent
                    .iter()
                    .filter(|firing| firing.automation == automation.name)
                    .collect();
                for firing in automation.last_firing.iter().chain(older) {
                    if !firings.iter().any(|known| known.fire_id == firing.fire_id) {
                        firings.push(firing);
                    }
                }
                firings
            }
        };
        firings.sort_by_key(|firing| Reverse(firing.queued_at));
        firings
    }

    fn firing(&self, fire_id: &str) -> Option<&FiringSummary> {
        self.firings()
            .into_iter()
            .find(|firing| firing.fire_id == fire_id)
            .or_else(|| {
                self.trace
                    .as_ref()
                    .and_then(Trace::loaded)
                    .map(|detail| &detail.firing)
                    .filter(|firing| firing.fire_id == fire_id)
            })
    }

    /// Lands the detail an `Inspect` of `name` in `session_id`, or in this
    /// session when that is `None`, asked for, while it is still the
    /// selection's.
    fn fill_detail(
        &mut self,
        name: &str,
        session_id: Option<&str>,
        detail: Loading<Box<AutomationDetail>>,
    ) {
        if !self.selection.inspected_by(name, session_id) {
            return;
        }
        if let Loading::Loaded(loaded) = &detail
            && let Some(editor) = self.editor_of(name, true)
        {
            editor.reload(loaded.state.as_ref());
        }
        self.detail = Some(detail);
    }

    fn trace_of(&mut self, fire_id: &str) -> Option<&mut Trace> {
        self.trace.as_mut().filter(|trace| trace.fire_id == fire_id)
    }

    /// The dry run of `fire_id`, while it is the one held: an answer to one
    /// the selection dropped since lands nowhere.
    fn dry_run_of(&mut self, fire_id: &str) -> Option<&mut DryRun> {
        self.dry_run
            .as_mut()
            .filter(|dry_run| dry_run.fire_id == fire_id)
    }

    /// The trace open in place of the Firings list: a firing's, else the dry
    /// run's.
    fn shown(&self) -> Option<Shown<'_>> {
        match (&self.trace, &self.dry_run) {
            (Some(trace), _) => Some(Shown::Firing(trace)),
            (None, Some(dry_run)) if dry_run.open => Some(Shown::DryRun(dry_run)),
            (None, _) => None,
        }
    }

    fn editor_of(&mut self, name: &str, state: bool) -> Option<&mut Editor> {
        self.editor
            .as_mut()
            .filter(|editor| editor.name == name && editor.is_state() == state)
    }

    /// A refused save shows under the editor that made it, or as a flash once
    /// that editor is gone.
    fn refuse_edit(&mut self, name: &str, state: bool, error: AutomationError) -> AutomationAction {
        match self.editor_of(name, state) {
            Some(editor) => {
                editor.refused(error.to_string());
                AutomationAction::None
            }
            None => AutomationAction::Flash(error.to_string()),
        }
    }

    /// Asks again for the detail of this session's script `name`, after a
    /// write to it, when it is selected. The one on screen stays, so a
    /// refresh does not flash back to loading.
    fn reinspect(&self, name: &str) -> AutomationAction {
        match self
            .selection
            .inspect()
            .filter(|_| self.selection.inspected_by(name, None))
        {
            Some(request) => AutomationAction::Request(request),
            None => AutomationAction::None,
        }
    }

    /// The section as lines, with the cursor mark on item `cursor`. `runs`
    /// changes the words of a started run's row, never which items there are.
    fn body(&self, now: i64, cursor: Option<usize>, runs: &[RunSnapshot]) -> Body {
        let mut body = Body::new(cursor);
        match &self.selection {
            Selection::Session => match self.session_section {
                SessionSection::Overview => overview::session(&mut body, &self.state.session, now),
                SessionSection::Firings => self.firings_body(&mut body, true, now, runs),
                SessionSection::Outbox => firings::outbox(
                    &mut body,
                    &self.state.outbox,
                    self.state.session.blockers.is_empty(),
                    now,
                ),
            },
            Selection::Automation(name) => match (self.state.find(name), self.automation_section) {
                (None, _) => body.text(GONE, theme::current().tool_dim),
                (Some(automation), AutomationSection::Overview) => {
                    overview::automation(&mut body, automation, now);
                }
                (Some(_), AutomationSection::Firings) => {
                    self.firings_body(&mut body, false, now, runs);
                }
                (Some(_), AutomationSection::State) => {
                    editor::state_section(&mut body, self.detail.as_ref(), &self.folded, now, true);
                }
                (Some(automation), AutomationSection::Args) => {
                    editor::args_section(&mut body, automation);
                }
            },
            Selection::Other { session_id, .. } => {
                self.other_body(&mut body, session_id, now, runs);
            }
        }
        body
    }

    /// Another session's automation. Each section reads its detail, so each
    /// waits for it, but for a trace opened from Firings.
    fn other_body(&self, body: &mut Body, session_id: &str, now: i64, runs: &[RunSnapshot]) {
        let Some(session) = self
            .sessions
            .iter()
            .find(|session| session.session_id == session_id)
        else {
            body.text(OTHER_GONE, theme::current().tool_dim);
            return;
        };
        match self.other_section {
            OtherSection::Overview => {
                if let Some(detail) = body.landed(self.detail.as_ref()) {
                    let online = self.online.contains(session_id);
                    overview::other(body, detail, session, online, now);
                }
            }
            OtherSection::Firings => {
                if self.trace.is_some() || body.landed(self.detail.as_ref()).is_some() {
                    self.firings_body(body, false, now, runs);
                }
            }
            OtherSection::State => {
                editor::state_section(body, self.detail.as_ref(), &self.folded, now, false);
            }
        }
    }

    /// The open trace, else the Firings list under the dry run's row.
    fn firings_body(&self, body: &mut Body, merged: bool, now: i64, runs: &[RunSnapshot]) {
        if let Some(shown) = self.shown() {
            trace::lines(body, shown, &self.folded, now, runs);
            return;
        }
        if let Some(dry_run) = &self.dry_run {
            dry_run::row(body, dry_run, merged);
        }
        firings::firings(body, &self.firings(), merged, now);
    }

    fn items(&self) -> Vec<Item> {
        self.body(now_ms(), None, &[])
            .items
            .into_iter()
            .map(|(item, _)| item)
            .collect()
    }

    /// The item the cursor is on, while the section has the focus.
    fn cursor_item(&self) -> Option<Item> {
        match self.pane {
            Pane::Detail => self.items().into_iter().nth(self.cursor),
            Pane::List => None,
        }
    }

    fn clamp_cursor(&mut self) {
        self.cursor = self.cursor.min(self.items().len().saturating_sub(1));
    }

    /// `runs` is the session's workflow mirror, which a trace's started runs
    /// are drawn from.
    pub fn view(&mut self, frame: &mut Frame, area: Rect, runs: &[RunSnapshot]) -> Rect {
        if !self.open {
            return Rect::default();
        }
        grab_scope!("automation_inspector", area);
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
        let input_row = self.confirm.is_some() || self.filter_focused || !self.filter.is_empty();
        let chrome = FOOTER_ROWS + u16::from(input_row) * INPUT_ROWS;
        let panes_height = padded.height.saturating_sub(chrome);
        let (list, detail) = self.panes(Rect {
            height: panes_height,
            ..padded
        });
        self.popup = popup;
        self.list_area = list;
        if list.width > 0 {
            self.render_list(frame, list);
        }
        match &mut self.editor {
            Some(editor) => {
                editor.render(frame, detail);
                self.body_area = Rect::default();
                self.tab_hits.clear();
                self.item_rows.clear();
            }
            None => {
                let [tabs, body] =
                    Layout::vertical([Constraint::Length(TABS_ROWS), Constraint::Fill(1)])
                        .areas(detail);
                self.body_area = body;
                if body.width > 0 {
                    self.render_tabs(frame, tabs);
                    self.render_body(frame, body, runs);
                }
            }
        }
        let mut row = padded.y.saturating_add(panes_height);
        if input_row {
            self.render_input(
                frame,
                Rect {
                    y: row,
                    height: INPUT_ROWS,
                    ..padded
                },
            );
            row = row.saturating_add(INPUT_ROWS);
        }
        let footer = Rect {
            y: row,
            height: FOOTER_ROWS,
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

    /// The list and the detail pane, or the one with the focus when the modal
    /// is too narrow for both. The hidden pane keeps a zero area, so nothing
    /// draws into it and the pointer cannot land on it.
    fn panes(&self, area: Rect) -> (Rect, Rect) {
        if area.width < SPLIT_MIN_COLS {
            return match self.pane {
                Pane::List if self.editor.is_none() => (area, Rect::default()),
                Pane::List | Pane::Detail => (Rect::default(), area),
            };
        }
        let list_width = (area.width * LIST_PERCENT / PERCENT).min(LIST_MAX_WIDTH);
        let [list, _, detail] = Layout::horizontal([
            Constraint::Length(list_width),
            Constraint::Length(PANE_GAP),
            Constraint::Fill(1),
        ])
        .areas(area);
        (list, detail)
    }

    fn render_list(&mut self, frame: &mut Frame, area: Rect) {
        grab_scope!("automation_inspector_list", area);
        let t = theme::current();
        let now = now_ms();
        let filter = self.filter.text();
        let entries = list::entries(&self.state, &self.sessions, &filter);
        let hint = if self.state.automations.is_empty() {
            Some(EMPTY_CATALOG)
        } else if entries.iter().all(|entry| matches!(entry, Entry::Session)) {
            Some(NO_MATCH)
        } else {
            None
        };
        let hovered = self
            .pointer
            .filter(|at| area.contains(*at))
            .map(|at| at.y - area.y + self.list_offset);
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(entries.len() * 2);
        let mut rows = Vec::with_capacity(entries.len());
        let mut selected_row = None;
        let mut group = None;
        let mut owner = None;
        for entry in &entries {
            if group != Some(entry.group()) {
                group = Some(entry.group());
                lines.push(Line::styled(entry.group().label(), t.keybind_section));
            }
            if let Entry::Other { session, .. } = entry
                && owner != Some(session.session_id.as_str())
            {
                owner = Some(session.session_id.as_str());
                let online = self.online.contains(&session.session_id);
                lines.push(list::session_row(session, online, now, area.width));
            }
            let selection = entry.selection();
            let selected = selection == self.selection;
            let row = u16::try_from(lines.len()).unwrap_or(u16::MAX);
            if selected {
                selected_row = Some(row);
            }
            let style = hover_style(
                match selected {
                    true => t.item_selected,
                    false => t.item,
                },
                hovered == Some(row),
            );
            lines.push(list::row(entry, &self.state, style, now));
            rows.push((row, selection));
            if let (Entry::Session, Some(hint)) = (entry, hint) {
                lines.push(Line::default());
                lines.push(Line::styled(hint, t.tool_dim));
            }
        }
        self.list_rows = rows;
        let total = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        if let Some(row) = selected_row {
            if row < self.list_offset {
                self.list_offset = row;
            } else if row >= self.list_offset.saturating_add(area.height) {
                self.list_offset = row.saturating_sub(area.height.saturating_sub(1));
            }
        }
        self.list_offset = self.list_offset.min(total.saturating_sub(area.height));
        frame.render_widget(Paragraph::new(lines).scroll((self.list_offset, 0)), area);
    }

    fn render_tabs(&mut self, frame: &mut Frame, area: Rect) {
        grab_scope!("automation_inspector_tabs", area);
        let (line, hits) = match self.selection {
            Selection::Session => indexed_strip(self.session_section, self.pointer, area),
            Selection::Automation(_) => indexed_strip(self.automation_section, self.pointer, area),
            Selection::Other { .. } => indexed_strip(self.other_section, self.pointer, area),
        };
        self.tab_hits = hits;
        frame.render_widget(Paragraph::new(line), area);
    }

    fn render_body(&mut self, frame: &mut Frame, area: Rect, runs: &[RunSnapshot]) {
        grab_scope!("automation_inspector_body", area);
        let cursor = (self.pane == Pane::Detail).then_some(self.cursor);
        let body = self.body(now_ms(), cursor, runs);
        let rows = visual_rows(&body.lines, area.width);
        self.scroll.update_dimensions(rows.total, area.height);
        self.item_rows = body
            .items
            .iter()
            .map(|(_, start)| {
                let line = u16::try_from(*start).unwrap_or(u16::MAX);
                (rows.row_of(line), rows.height_of(line))
            })
            .collect();
        if self.reveal_cursor
            && self.pane == Pane::Detail
            && let Some(&(top, height)) = self.item_rows.get(self.cursor)
        {
            self.scroll.reveal(top, height);
        }
        self.reveal_cursor = false;
        frame.render_widget(
            Paragraph::new(body.lines)
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

    /// The confirmation that waits for Enter, else the filter.
    fn render_input(&self, frame: &mut Frame, area: Rect) {
        let line = match &self.confirm {
            Some(confirm) => Line::from(confirm.prompt()),
            None => {
                let mut spans = vec![chevron_span()];
                let width =
                    usize::from(area.width).saturating_sub(spans.iter().map(Span::width).sum());
                let styles = field_styles(input_text_style());
                spans.extend(
                    self.filter
                        .paint(width, &styles, self.filter_focused, "")
                        .spans,
                );
                Line::from(spans)
            }
        };
        frame.render_widget(Paragraph::new(line), area);
    }

    /// The footer is one centred row, and a line wider than that row wraps and
    /// then answers no clicks at all, so a narrow modal gives up the words
    /// that gloss its keys, and then the space between them, before a key.
    fn footer_line(&self, width: u16) -> FooterLine {
        let item = self.cursor_item();
        let enabled: Vec<bool> = self
            .footer_entries()
            .iter()
            .map(|(_, _, command)| self.enabled(*command, item.as_ref()))
            .collect();
        let mut footer = FooterLine::default();
        for (glossed, gap) in FOOTER_RUNGS {
            footer = self.commands_footer(&enabled, glossed, gap);
            if footer.fits(width) {
                break;
            }
        }
        footer
    }

    fn commands_footer(&self, enabled: &[bool], glossed: bool, gap: &'static str) -> FooterLine {
        let t = theme::current();
        let mut footer = FooterLine::default();
        for (index, ((label, description, command), enabled)) in
            self.footer_entries().iter().zip(enabled).enumerate()
        {
            if index > 0 {
                footer.text(gap, Style::default());
            }
            let key_style = match enabled {
                true => t.keybind_key,
                false => t.tool_dim,
            };
            footer.command(label, key_style);
            if glossed {
                footer.describe(
                    format!(" {}", self.describe(*command, description)),
                    t.tool_dim,
                );
            }
        }
        footer
    }

    /// The word a command goes by now: a toggle names what it would do.
    fn describe(&self, command: FooterCommand, description: &'static str) -> &'static str {
        match command {
            FooterCommand::Arm
                if self
                    .selected_automation()
                    .is_some_and(|automation| automation.armed.is_some()) =>
            {
                DISARM_WORD
            }
            FooterCommand::Pause if self.state.session.controls.pause.is_some() => RESUME_WORD,
            FooterCommand::Close if self.shown().is_some() => BACK_WORD,
            _ => description,
        }
    }

    fn enabled(&self, command: FooterCommand, item: Option<&Item>) -> bool {
        if command.controls() && self.read_only() {
            return false;
        }
        let automation = self.selected_automation();
        match command {
            FooterCommand::Open => self.pane == Pane::List || item.is_some(),
            FooterCommand::Arm => automation.is_some(),
            FooterCommand::Edit => {
                automation.is_some()
                    && matches!(
                        self.automation_section,
                        AutomationSection::State | AutomationSection::Args
                    )
            }
            FooterCommand::Trust => {
                automation.is_some_and(|automation| automation.trust == Trust::Required)
            }
            FooterCommand::Clear => {
                automation.is_some()
                    && self
                        .loaded_detail()
                        .is_some_and(|detail| detail.state.is_some())
            }
            FooterCommand::Drop => self.drop_target(item).is_some(),
            FooterCommand::DryRun => {
                !self.dry_run.as_ref().is_some_and(DryRun::loading)
                    && self.replayable(item).is_some()
            }
            FooterCommand::Script => self.script(item).is_some(),
            FooterCommand::Copy => self.copy_source(item).is_some(),
            FooterCommand::Pause
            | FooterCommand::Filter
            | FooterCommand::Close
            | FooterCommand::Save
            | FooterCommand::Confirm
            | FooterCommand::Cancel => true,
        }
    }
}

impl Default for AutomationInspector {
    fn default() -> Self {
        Self::new()
    }
}

impl Overlay for AutomationInspector {
    fn is_open(&self) -> bool {
        self.open
    }

    fn close(&mut self) {
        self.close();
    }

    /// Ages, countdowns and waits are told against the clock.
    fn cadence(&self) -> Cadence {
        Cadence::when(self.open, Cadence::CLOCK)
    }
}

/// The request an action carries, when it carries one.
fn requested(action: AutomationAction) -> Vec<AutomationRequest> {
    match action {
        AutomationAction::Request(request) => vec![request],
        _ => Vec::new(),
    }
}

/// [`tab_strip`] with each hit named by its index, so both selections' tabs
/// share one hit list.
fn indexed_strip<T: SectionTab>(
    current: T,
    pointer: Option<Position>,
    area: Rect,
) -> (Line<'static>, Vec<(Rect, usize)>) {
    let (line, hits) = tab_strip(current, pointer, area);
    (
        line,
        hits.into_iter()
            .map(|(hit, tab)| (hit, tab.index()))
            .collect(),
    )
}

fn error_line(firing: &FiringSummary) -> Option<u32> {
    firing.error.as_ref().and_then(|error| error.line)
}

/// A firing as the mirror holds it, in the newest firings or as a script's
/// last one.
fn known_firing<'a>(state: &'a AutomationState, fire_id: &str) -> Option<&'a FiringSummary> {
    state
        .recent
        .iter()
        .chain(
            state
                .automations
                .iter()
                .filter_map(|automation| automation.last_firing.as_ref()),
        )
        .find(|firing| firing.fire_id == fire_id)
}

/// Whether a trace holds a delivery that is still queued, whose state the
/// outbox moves.
fn waits_for_delivery(trace: &Trace) -> bool {
    trace.loaded().is_some_and(|detail| {
        detail
            .actions
            .iter()
            .any(|action| action.status == ActionStatus::Queued)
    })
}

#[cfg(test)]
mod tests {
    use caudra_agent::automation::testing::settled_run;
    use caudra_agent::peers::handle_address;
    use caudra_automation::args::{ArgDecl, ArgSpec, ArgType, ArgsError, ValueError};
    use caudra_automation::catalog::Scope;
    use caudra_automation::event::TurnOutcome;
    use caudra_automation::host::{ActionKind, DeliveryMode};
    use caudra_automation::limits::{ActingMarks, LimitReason, LimitRefusal};
    use caudra_automation::meta::TriggerKind;
    use caudra_automation::replay::{Answer, DRY_RUN_ID};
    use caudra_automation::request::REPLAY_EVENT_CUT;
    use caudra_automation::snapshot::{
        ActionBody, ActionRow, AutomationStatus, Availability, BindingView, Capabilities,
        ErrorView, FiringStatus, OutboxItem, PauseLatch, StateView, WaitReason,
    };
    use caudra_automation::state::StateError;
    use caudra_workflow::RunStatus;
    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use serde_json::json;
    use test_case::test_case;

    use super::dry_run::{
        BADGE, CANNOT_REPLAY, CUT, JOURNAL, LIMITED, RAN_AGAINST, RECORDED, SCRIPT_CHANGED,
        STUBBED, UNTIL,
    };
    use super::editor::{ARGS_NOT_JSON, CONFLICT_PREFIX};
    use super::firings::{
        CONSUMED_BADGE, GROUP_FINISHED, GROUP_RUNNING, GROUP_WAITING, JOINS_RUNNING_TURN, READY,
        REPEATS_PREFIX, STARTS_ONCE_CLOSED, TRIGGER_INDEX,
    };
    use super::list::{
        BACKING_OFF_GLYPH, DEFERRED_GLYPH, FAILED_GLYPH, Group, IDLE_GLYPH, ONLINE, PAUSED_GLYPH,
        QUEUED_GLYPH, RUNNING_GLYPH,
    };
    use super::text::{
        clock, limit_text, moment, outcome_text, relative, scope_text, span, trigger_text,
        wait_text,
    };
    use super::trace::{
        AT_LINE, COLUMN_SEPARATOR, DEDUPLICATED, DELIVERED, DONE, DROPPED, DRY_RUN_TITLE, EXPIRED,
        FAILED, INTERRUPTED, MARKDOWN_DRY_RUN_TITLE, MARKDOWN_TITLE, QUEUED, REFUSED, RUNNING,
        SEPARATOR, SOURCE_RULE, STARTED, UNTRUSTED_LEGEND,
    };
    use super::*;
    use crate::components::key as key_event;
    use crate::selection::line_text;

    const FRAME_WIDTH: u16 = 160;
    const FRAME_HEIGHT: u16 = 40;
    const WHEEL_DOWN: i32 = -1;
    const NOW: i64 = 1_800_000_000_000;
    const MINUTE_MS: i64 = 60_000;
    const FIRING_TOOK_MS: i64 = 450;
    const OVERVIEW_TAB_KEY: char = '1';
    const FIRINGS_TAB_KEY: char = '2';
    const OUTBOX_TAB_KEY: char = '3';
    const STATE_TAB_KEY: char = '3';
    const ARGS_TAB_KEY: char = '4';
    const SCRIPT: &str = "triage";
    const SCRIPT_DESCRIPTION: &str = "Sorts incoming bug reports";
    const OTHER_SCRIPT: &str = "nightly";
    const OTHER_DESCRIPTION: &str = "Summarises the day's commits";
    const UNTRUSTED_SCRIPT: &str = "vendored";
    const BROKEN_SCRIPT: &str = "broken";
    const NAME_FILTER: &str = "TRI";
    const DESCRIPTION_FILTER: &str = "day's commits";
    const SCRIPT_PATH: &str = ".caudra/automations/triage.rhai";
    const DIGEST: &str = "sha256:5f1d0c";
    const SESSION_ID: &str = "session-1";
    const FIRE_ID: &str = "fire-done";
    const QUEUED_FIRE: &str = "fire-queued";
    const DEFERRED_FIRE: &str = "fire-deferred";
    const RUNNING_FIRE: &str = "fire-running";
    const FAILED_FIRE: &str = "fire-failed";
    const TRIGGER_POSITION: u32 = 1;
    const SEQ: u64 = 2;
    const ACTION_LINE: u32 = 9;
    const ACTION_SUMMARY: &str = "Triage the new report";
    const ERROR_KIND: &str = "runtime";
    const ERROR_MESSAGE: &str = "index out of bounds";
    const ERROR_LINE: u32 = 12;
    const ERROR_COLUMN: u32 = 5;
    const ERROR_SOURCE: &str = "let first = reports[0];";
    const REPEATS: u64 = 3;
    const WAITING: u32 = 2;
    const UNATTENDED_CAP: u32 = 6;
    const TURN_COST: f64 = 0.0125;
    const JOINED_DELIVERY: &str = "fire-earlier";
    const STATE_REVISION: u64 = 4;
    const CONFLICT_REVISION: u64 = 7;
    const NOTE_KEY: &str = "note";
    const NOTE: &str = "the reviewer asked twice";
    const COUNT_KEY: &str = "count";
    const RESERVED_KEY: &str = "$cost";
    const ARG_TOPIC: &str = "topic";
    const ARG_INTERVAL: &str = "interval";
    const UNDECLARED_ARG: &str = "colour";
    const TOPIC: &str = "bugs";
    const INTERVAL: u64 = 5;
    const NOT_JSON: &str = "topic = bugs";
    const EVENT_KEY: &str = "message";
    const HOSTILE_TEXT: &str = "hi\u{1b}[31m";
    const ESCAPE: char = '\u{1b}';
    const REQUEST_TEXT: &str = "request-marker";
    const RESULT_TEXT: &str = "result-marker";
    const GROUP_DRAWN: &str = "every group heads its rows in the list";
    const GROUPS_IN_ORDER: &str = "the groups draw in their enum's order";
    const HEADING_DRAWN: &str = "every group with firings has its heading";
    const SEVERAL_ITEMS: &str = "the section offers the cursor more than one row";
    const TARGETS_MATCH_ITEMS: &str = "every item has a click target";
    const MARK_ON_TARGET: &str = "the one marked row is the row a click on the cursor's item hits";
    const CURSOR_CLAMPS: &str = "the cursor stops on the last item";
    const ITEM_LISTED: &str = "the section lists the item";
    const ITEM_UNDER_CURSOR: &str = "the arrows put the cursor on the item";
    const ACTION_ASKS: &str = "Enter on an action asks for its body";
    const SAVE_SENT: &str = "a valid edit is sent";
    const CONFIRM_FIRST: &str = "the key asks for confirmation before it acts";
    const ESC_CANCELS: &str = "Esc drops the confirmation and keeps the inspector open";
    const EDITOR_OPENS: &str = "e opens the editor";
    const NOTHING_SENT: &str = "an edit the runtime would refuse sends nothing";
    const EDITOR_STAYS_OPEN: &str = "a conflicting save keeps the editor open";
    const RELOADED: &str = "the reloaded state replaces the text";
    const CONFLICT_SHOWN: &str = "the editor names the revision it lost to";
    const OPENS_FROM_CACHE: &str = "reopening a loaded action asks for nothing";
    const BACK_TO_ROW: &str = "Esc steps out of the trace onto the firing it opened";
    const LEGEND_SHOWN: &str = "an event holding untrusted text says so";
    const ESCAPED: &str = "no raw control byte reaches a line";
    const TAG_DRAWN: &str = "the untrusted key is drawn";
    const UNCHANGED_ASKS_NOTHING: &str = "an unchanged mirror asks for nothing";
    const COPIES_MARKDOWN: &str = "y copies the firing";
    const PROMPT_DRAWN: &str = "the confirmation names the digest it trusts";
    const REQUIRED_ARGS_TO_FILL: &str = "a required arg starts as null for the reader to fill in";
    const ROW_DRAWN: &str = "the list drew the row";
    const COMMAND_DRAWN: &str = "the footer drew the command";
    const HOVER_PLACES_THE_CURSOR: &str = "the pointer puts the cursor on the row under it";
    const UNFOCUSED_PASTE_PASSES: &str = "a paste with nothing focused is the app's";
    const PRESS_OUTSIDE_IS_THE_APPS: &str =
        "a press outside the modal is the app's to dismiss with";
    const EDIT_KEEPS_PRESSES: &str = "a stray press cannot throw an open edit away";
    const STARTED_RUN: &str = "run-7";
    const OTHER_RUN: &str = "run-8";
    const STARTED_WORKFLOW: &str = "review-changes";
    const RUN_NAME: &str = "review-changes-3";
    const FOLLOWS_THE_MIRROR: &str = "the started run reads as the mirror holds it now";
    const PEER_SESSION: &str = "session-2";
    const PEER_TITLE: &str = "Release";
    const PEER_HANDLE: &str = "otter";
    const PEER_FIRE: &str = "fire-peer-queued";
    const PEER_DONE_FIRE: &str = "fire-peer-done";
    const QUIET_SESSION: &str = "session-3";
    const QUIET_TITLE: &str = "Docs";
    const QUIET_HANDLE: &str = "heron";
    const QUIET_AUTOMATION: &str = "standup";
    const TITLE_FILTER: &str = "RELEASE";
    const HANDLE_FILTER: &str = "@ott";
    const AUTOMATION_FILTER: &str = "stand";
    const HEADING_WIDTH: u16 = 60;
    const JUST_BEFORE: Duration = Duration::from_millis(1);
    const DETAIL_LANDED: &str = "the detail asked for lands on the selection";
    const STRAY_DETAIL_IGNORED: &str =
        "a detail asked of another session leaves the selection's detail alone";
    const READ_AGAIN: &str = "the selected automation is read again once the interval is up";
    const NOT_BEFORE: &str = "nothing is read again before the interval is up";
    const NOTHING_TO_READ: &str = "only another session's selected automation is read again";
    const STOPS_ONCE_MOVED: &str = "a selection that moved away is no longer read";
    const MARKED_ONLINE: &str = "a session the live directory lists is marked online";
    const NOT_MARKED: &str = "a session the live directory does not list is not marked";
    const HEADING_SELECTS_NOTHING: &str = "a session's heading row selects nothing";
    const HEADING_ABOVE_ROWS: &str = "a session's heading sits above its automations";
    const NOTHING_ACTS: &str = "no key on another session's automation sends what acts";
    const DRY_DIGEST: &str = "sha256:8c3b2a";
    const DRY_RUN_ASKED: &str = "r asks for a dry run of the firing";
    const ROW_ON_TOP: &str = "r puts the dry run atop Firings, under the cursor";
    const FOOTER_AGREES: &str = "the footer greys r exactly when the key sends nothing";
    const DRY_RUN_HELD: &str = "a section switch closes the dry run and keeps its row atop Firings";
    const DRY_RUN_DROPPED: &str = "the dry run goes with the selection";
    const STALE_DROPPED: &str = "an answer to a dry run no longer held lands nowhere";
    const NO_BODY: &str = "a dry run's action asks for no body";
    const BACK_TO_DRY_RUN: &str = "Esc steps out of the dry run onto its row";
    const PATCH_DRAWN: &str = "the dry run shows the state change it would commit";

    fn script(name: &str, availability: Availability) -> AutomationSnapshot {
        AutomationSnapshot {
            name: name.to_owned(),
            description: String::new(),
            scope: Scope::Project,
            path: PathBuf::from(SCRIPT_PATH),
            digest: DIGEST.to_owned(),
            trust: Trust::Approved,
            availability,
            armed: None,
            args: None,
            declared_args: Vec::new(),
            status: AutomationStatus::Idle,
            last_firing: None,
            triggers: Vec::new(),
            limits: None,
            limiter: ActingMarks::default(),
            capabilities: Capabilities::default(),
            host_functions: Vec::new(),
            warnings: Vec::new(),
            shadowed: Vec::new(),
        }
    }

    fn firing(fire_id: &str, status: FiringStatus, queued_at: i64) -> FiringSummary {
        FiringSummary {
            fire_id: fire_id.to_owned(),
            automation: SCRIPT.to_owned(),
            digest: DIGEST.to_owned(),
            trigger: TriggerKind::MessageReceived,
            trigger_index: TRIGGER_POSITION,
            event_key: None,
            consumed: false,
            status,
            reason: None,
            error: None,
            repeats: 1,
            attempts: 0,
            operations: 0,
            state_outcome: None,
            queued_at,
            deferred_until: None,
            started_at: None,
            finished_at: None,
            action_count: 1,
            first_action: Some(ActionKind::Message),
        }
    }

    fn finished_firing() -> FiringSummary {
        let started = NOW - 3 * MINUTE_MS;
        FiringSummary {
            consumed: true,
            repeats: REPEATS,
            started_at: Some(started),
            finished_at: Some(started + FIRING_TOOK_MS),
            ..firing(FIRE_ID, FiringStatus::Completed, started)
        }
    }

    fn failed_firing() -> FiringSummary {
        let started = NOW - 4 * MINUTE_MS;
        FiringSummary {
            error: Some(ErrorView {
                kind: ERROR_KIND.to_owned(),
                message: ERROR_MESSAGE.to_owned(),
                line: Some(ERROR_LINE),
                column: Some(ERROR_COLUMN),
            }),
            started_at: Some(started),
            finished_at: Some(started + FIRING_TOOK_MS),
            ..firing(FAILED_FIRE, FiringStatus::Failed, started)
        }
    }

    /// A firing in each place a firing can be, newest first.
    fn recent() -> Vec<FiringSummary> {
        vec![
            firing(QUEUED_FIRE, FiringStatus::Queued, NOW - MINUTE_MS),
            FiringSummary {
                started_at: Some(NOW - 2 * MINUTE_MS),
                ..firing(RUNNING_FIRE, FiringStatus::Running, NOW - 2 * MINUTE_MS)
            },
            finished_firing(),
            failed_firing(),
            FiringSummary {
                deferred_until: Some(NOW + 5 * MINUTE_MS),
                ..firing(DEFERRED_FIRE, FiringStatus::Deferred, NOW - 5 * MINUTE_MS)
            },
        ]
    }

    fn outbox_item(seq: u64, wait: Option<WaitReason>) -> OutboxItem {
        OutboxItem {
            automation: SCRIPT.to_owned(),
            fire_id: FIRE_ID.to_owned(),
            seq,
            kind: ActionKind::Message,
            summary: ACTION_SUMMARY.to_owned(),
            delivery: DeliveryMode::Next,
            queued_at: NOW - MINUTE_MS,
            expires_at: None,
            wait,
        }
    }

    fn catalog() -> AutomationState {
        AutomationState {
            automations: vec![
                AutomationSnapshot {
                    description: SCRIPT_DESCRIPTION.to_owned(),
                    armed: Some(ArmOrigin::Manual),
                    ..script(SCRIPT, Availability::Armed)
                },
                AutomationSnapshot {
                    description: OTHER_DESCRIPTION.to_owned(),
                    ..script(OTHER_SCRIPT, Availability::Available)
                },
            ],
            outbox: vec![
                outbox_item(SEQ, Some(WaitReason::Busy)),
                outbox_item(SEQ + 1, None),
            ],
            recent: recent(),
            ..AutomationState::default()
        }
    }

    fn action(status: ActionStatus) -> ActionRow {
        ActionRow {
            seq: SEQ,
            kind: ActionKind::Message,
            line: Some(ACTION_LINE),
            column: Some(1),
            status,
            summary: ACTION_SUMMARY.to_owned(),
            error: None,
            target: None,
            delivery: Some(DeliveryMode::Next),
            expires_at: None,
            wait: None,
            turn_outcome: None,
            turn_cost: None,
            started_at: NOW - MINUTE_MS,
            finished_at: None,
            delivered_at: None,
            request_cut: false,
            result_cut: false,
        }
    }

    fn action_body() -> ActionBody {
        ActionBody {
            fire_id: FIRE_ID.to_owned(),
            seq: SEQ,
            request: json!({ EVENT_KEY: { NOTE_KEY: REQUEST_TEXT } }),
            request_cut: false,
            result: Some(json!({ NOTE_KEY: RESULT_TEXT })),
            result_cut: false,
        }
    }

    fn trace_of(firing: FiringSummary, actions: Vec<ActionRow>) -> FiringDetail {
        FiringDetail {
            firing,
            event: json!({ EVENT_KEY: { UNTRUSTED_TAG: HOSTILE_TEXT } }),
            event_cut: false,
            state_patch: Some(json!({ NOTE_KEY: { COUNT_KEY: REPEATS } })),
            patch_cut: false,
            actions,
            error_source: None,
        }
    }

    fn tagged_state() -> Value {
        json!({ NOTE_KEY: { UNTRUSTED_TAG: NOTE }, COUNT_KEY: [REPEATS, REPEATS] })
    }

    fn state_view(revision: u64, value: Value) -> StateView {
        StateView {
            value,
            revision,
            writer: Some(FIRE_ID.to_owned()),
            digest: Some(DIGEST.to_owned()),
            written_at: Some(NOW - MINUTE_MS),
        }
    }

    fn binding(name: &str) -> BindingView {
        BindingView {
            name: name.to_owned(),
            scope: Scope::Project,
            origin: ArmOrigin::Manual,
            armed: true,
            args: json!({}),
            args_digest: None,
        }
    }

    fn detail(state: Option<StateView>) -> AutomationDetail {
        AutomationDetail {
            session_id: SESSION_ID.to_owned(),
            name: SCRIPT.to_owned(),
            binding: Some(binding(SCRIPT)),
            state,
            firings: Vec::new(),
        }
    }

    /// Another session's firings of its script named like this session's
    /// [`SCRIPT`]: one that waits and one that finished.
    fn peer_firings() -> Vec<FiringSummary> {
        vec![
            firing(PEER_FIRE, FiringStatus::Queued, NOW - MINUTE_MS),
            FiringSummary {
                finished_at: Some(NOW - 2 * MINUTE_MS),
                ..firing(PEER_DONE_FIRE, FiringStatus::Completed, NOW - 3 * MINUTE_MS)
            },
        ]
    }

    /// Another session, with an `@name`, that armed a script named like this
    /// session's [`SCRIPT`].
    fn peer() -> AutomationHistoryEntry {
        AutomationHistoryEntry {
            session_id: PEER_SESSION.to_owned(),
            title: PEER_TITLE.to_owned(),
            handle: PEER_HANDLE.to_owned(),
            last_activity_at: NOW - MINUTE_MS,
            bindings: vec![binding(SCRIPT)],
            firings: peer_firings(),
        }
    }

    /// A session whose automation only its firings name.
    fn quiet() -> AutomationHistoryEntry {
        AutomationHistoryEntry {
            session_id: QUIET_SESSION.to_owned(),
            title: QUIET_TITLE.to_owned(),
            handle: QUIET_HANDLE.to_owned(),
            last_activity_at: NOW - 3 * MINUTE_MS,
            bindings: Vec::new(),
            firings: vec![FiringSummary {
                automation: QUIET_AUTOMATION.to_owned(),
                ..finished_firing()
            }],
        }
    }

    /// The detail of [`peer`]'s script, at `state`.
    fn peer_detail(state: Option<StateView>) -> AutomationDetail {
        AutomationDetail {
            session_id: PEER_SESSION.to_owned(),
            firings: peer_firings(),
            ..detail(state)
        }
    }

    fn arg_spec(kind: ArgType, default: Option<Value>) -> ArgSpec {
        ArgSpec {
            kind,
            default,
            min: None,
            max: None,
            choices: Vec::new(),
            description: None,
            example: None,
        }
    }

    fn inspect(name: &str) -> AutomationRequest {
        AutomationRequest::Inspect {
            name: name.to_owned(),
            session_id: None,
        }
    }

    fn firing_request(fire_id: &str) -> AutomationRequest {
        AutomationRequest::Firing {
            fire_id: fire_id.to_owned(),
        }
    }

    fn sessions_request() -> AutomationRequest {
        AutomationRequest::Sessions { limit: None }
    }

    fn peer_selection() -> Selection {
        Selection::Other {
            session_id: PEER_SESSION.to_owned(),
            name: SCRIPT.to_owned(),
        }
    }

    fn inspect_in(session_id: &str, name: &str) -> AutomationRequest {
        AutomationRequest::Inspect {
            name: name.to_owned(),
            session_id: Some(session_id.to_owned()),
        }
    }

    fn inspect_peer() -> AutomationRequest {
        inspect_in(PEER_SESSION, SCRIPT)
    }

    /// Whether `request` does more than read. Each such request acts on this
    /// session, whichever row sent it.
    fn acts(request: &AutomationRequest) -> bool {
        !matches!(
            request,
            AutomationRequest::List
                | AutomationRequest::Validate { .. }
                | AutomationRequest::Inspect { .. }
                | AutomationRequest::Firing { .. }
                | AutomationRequest::ActionBody { .. }
                | AutomationRequest::History { .. }
                | AutomationRequest::Sessions { .. }
        )
    }

    fn opened(state: AutomationState, focus: Option<&str>) -> AutomationInspector {
        let mut inspector = AutomationInspector::new();
        inspector.open(Arc::new(state), focus);
        inspector
    }

    fn press(inspector: &mut AutomationInspector, code: KeyCode) -> AutomationAction {
        inspector.handle_key(key_event(code), &[])
    }

    fn press_char(inspector: &mut AutomationInspector, character: char) -> AutomationAction {
        press(inspector, KeyCode::Char(character))
    }

    /// The session's `section`, with the focus on it.
    fn session_section(section: char) -> AutomationInspector {
        let mut inspector = opened(catalog(), None);
        let _ = press_char(&mut inspector, section);
        let _ = press(&mut inspector, KeyCode::Right);
        inspector
    }

    /// The script selected with its detail landed, and `section` open with
    /// the focus on it.
    fn script_section(
        state: AutomationState,
        section: char,
        detail: AutomationDetail,
    ) -> AutomationInspector {
        let mut inspector = opened(state, Some(SCRIPT));
        let landed = inspector.apply_response(
            &inspect(SCRIPT),
            Ok(AutomationResponse::Detail(Box::new(detail))),
        );
        assert_eq!(landed, AutomationAction::None);
        let _ = press_char(&mut inspector, section);
        let _ = press(&mut inspector, KeyCode::Right);
        inspector
    }

    /// Opened on a firing whose trace has landed.
    fn traced(detail: FiringDetail) -> AutomationInspector {
        let fire_id = detail.firing.fire_id.clone();
        let mut inspector = opened(catalog(), Some(&fire_id));
        let landed = inspector.apply_response(
            &firing_request(&fire_id),
            Ok(AutomationResponse::Firing(Box::new(detail))),
        );
        assert_eq!(landed, AutomationAction::None);
        inspector
    }

    fn step_to(inspector: &mut AutomationInspector, item: &Item) {
        let index = inspector
            .items()
            .iter()
            .position(|candidate| candidate == item)
            .expect(ITEM_LISTED);
        let steps = index as isize - inspector.cursor as isize;
        let code = match steps < 0 {
            true => KeyCode::Up,
            false => KeyCode::Down,
        };
        for _ in 0..steps.unsigned_abs() {
            let _ = press(inspector, code);
        }
        assert_eq!(
            inspector.cursor_item().as_ref(),
            Some(item),
            "{ITEM_UNDER_CURSOR}"
        );
    }

    fn on_session_firings() -> AutomationInspector {
        session_section(FIRINGS_TAB_KEY)
    }

    fn on_session_outbox() -> AutomationInspector {
        session_section(OUTBOX_TAB_KEY)
    }

    fn on_script_firings() -> AutomationInspector {
        script_section(catalog(), FIRINGS_TAB_KEY, detail(None))
    }

    fn on_script_state() -> AutomationInspector {
        script_section(
            catalog(),
            STATE_TAB_KEY,
            detail(Some(state_view(STATE_REVISION, tagged_state()))),
        )
    }

    fn on_script_list() -> AutomationInspector {
        opened(catalog(), Some(SCRIPT))
    }

    /// Opened on the session, with [`peer`] and [`quiet`] listed.
    fn with_peers() -> AutomationInspector {
        let mut inspector = opened(catalog(), None);
        let listed = inspector.apply_response(
            &sessions_request(),
            Ok(AutomationResponse::Sessions(vec![peer(), quiet()])),
        );
        assert_eq!(listed, AutomationAction::None);
        inspector
    }

    /// [`peer`]'s script selected with its detail landed, and `section` open
    /// with the focus on it.
    fn peer_section(section: char) -> AutomationInspector {
        let mut inspector = with_peers();
        assert_eq!(
            inspector.select(peer_selection()),
            AutomationAction::Request(inspect_peer())
        );
        let state = state_view(STATE_REVISION, tagged_state());
        let landed = inspector.apply_response(
            &inspect_peer(),
            Ok(AutomationResponse::Detail(Box::new(peer_detail(Some(
                state,
            ))))),
        );
        assert_eq!(landed, AutomationAction::None);
        let _ = press_char(&mut inspector, section);
        let _ = press(&mut inspector, KeyCode::Right);
        inspector
    }

    /// [`peer`]'s firings, the cursor on the one that waits.
    fn on_peer_firings() -> AutomationInspector {
        peer_section(FIRINGS_TAB_KEY)
    }

    fn on_peer_state() -> AutomationInspector {
        peer_section(STATE_TAB_KEY)
    }

    fn on_finished_firing() -> AutomationInspector {
        let mut inspector = on_session_firings();
        step_to(&mut inspector, &Item::Firing(FIRE_ID.to_owned()));
        inspector
    }

    fn on_failed_firing() -> AutomationInspector {
        let mut inspector = on_script_firings();
        step_to(&mut inspector, &Item::Firing(FAILED_FIRE.to_owned()));
        inspector
    }

    fn on_traced_action(status: ActionStatus) -> AutomationInspector {
        let delivery = ActionRow {
            wait: Some(WaitReason::Busy),
            ..action(status)
        };
        let mut inspector = traced(trace_of(finished_firing(), vec![delivery]));
        step_to(&mut inspector, &Item::Action(SEQ));
        inspector
    }

    fn on_done_action() -> AutomationInspector {
        on_traced_action(ActionStatus::Done)
    }

    fn on_queued_delivery() -> AutomationInspector {
        on_traced_action(ActionStatus::Queued)
    }

    /// A trace with an action's request and result open, the cursor back on
    /// its first row.
    fn on_opened_action() -> AutomationInspector {
        let mut inspector = on_done_action();
        let AutomationAction::Request(request) = press(&mut inspector, KeyCode::Enter) else {
            panic!("{ACTION_ASKS}");
        };
        let _ = inspector.apply_response(
            &request,
            Ok(AutomationResponse::ActionBody(Box::new(action_body()))),
        );
        inspector.cursor = 0;
        inspector
    }

    fn start_action(run_id: &str) -> ActionRow {
        ActionRow {
            kind: ActionKind::StartWorkflow,
            target: Some(run_id.to_owned()),
            ..action(ActionStatus::Done)
        }
    }

    /// A trace whose action started `run_id`, the cursor on that action.
    fn on_start_of(run_id: &str) -> AutomationInspector {
        let mut inspector = traced(trace_of(finished_firing(), vec![start_action(run_id)]));
        step_to(&mut inspector, &Item::Action(SEQ));
        inspector
    }

    /// A trace whose action started [`STARTED_RUN`], the cursor on its first row.
    fn on_started_run() -> AutomationInspector {
        traced(trace_of(finished_firing(), vec![start_action(STARTED_RUN)]))
    }

    /// The session's workflow mirror, holding the started run in `status`.
    fn mirror(status: RunStatus) -> Vec<RunSnapshot> {
        vec![RunSnapshot {
            display_name: RUN_NAME.to_owned(),
            ..settled_run(STARTED_RUN, STARTED_WORKFLOW, status, 0)
        }]
    }

    fn mirrored_text(status: RunStatus) -> String {
        format!("{RUN_NAME}{SEPARATOR}{status}")
    }

    fn terminal() -> Terminal<TestBackend> {
        Terminal::new(TestBackend::new(FRAME_WIDTH, FRAME_HEIGHT)).unwrap()
    }

    fn draw(
        inspector: &mut AutomationInspector,
        terminal: &mut Terminal<TestBackend>,
    ) -> Vec<String> {
        draw_with(inspector, terminal, &[])
    }

    /// A frame drawn against `runs` as the session's workflow mirror.
    fn draw_with(
        inspector: &mut AutomationInspector,
        terminal: &mut Terminal<TestBackend>,
        runs: &[RunSnapshot],
    ) -> Vec<String> {
        terminal
            .draw(|frame| {
                inspector.view(frame, frame.area(), runs);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect()
            })
            .collect()
    }

    fn mouse(kind: MouseEventKind, at: Position) -> MouseEvent {
        MouseEvent {
            kind,
            column: at.x,
            row: at.y,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn click(inspector: &mut AutomationInspector, at: Position) -> AutomationAction {
        inspector.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), at), &[])
    }

    /// Where the last frame drew the list row of `selection`.
    fn list_row(inspector: &AutomationInspector, selection: &Selection) -> Position {
        let (row, _) = inspector
            .list_rows
            .iter()
            .find(|(_, listed)| listed == selection)
            .expect(ROW_DRAWN);
        Position::new(
            inspector.list_area.x,
            inspector.list_area.y + row - inspector.list_offset,
        )
    }

    /// Where the last frame drew the first row of body item `index`.
    fn item_row(inspector: &AutomationInspector, index: usize) -> Position {
        let (top, _) = inspector.item_rows[index];
        Position::new(
            inspector.body_area.x,
            inspector.body_area.y + top - inspector.scroll.offset(),
        )
    }

    /// The rows of `area` whose first cell holds the cursor mark.
    fn marked_rows(terminal: &Terminal<TestBackend>, area: Rect) -> Vec<u16> {
        let buffer = terminal.backend().buffer();
        (area.top()..area.bottom())
            .filter(|row| buffer[(area.x, *row)].symbol() == CURSOR_MARK.trim_end())
            .collect()
    }

    fn lines_of(body: &Body) -> Vec<String> {
        body.lines.iter().map(line_text).collect()
    }

    fn body_shows(inspector: &AutomationInspector, text: &str) -> bool {
        lines_of(&inspector.body(NOW, None, &[]))
            .iter()
            .any(|line| line.contains(text))
    }

    fn editing_state(state: Option<StateView>) -> AutomationInspector {
        let mut inspector = script_section(catalog(), STATE_TAB_KEY, detail(state));
        assert_eq!(press_char(&mut inspector, EDIT_KEY), AutomationAction::None);
        assert!(inspector.text_input_active(), "{EDITOR_OPENS}");
        inspector
    }

    fn editing_args() -> AutomationInspector {
        let mut state = catalog();
        state.automations[0].declared_args = vec![
            ArgDecl {
                name: ARG_TOPIC.to_owned(),
                spec: arg_spec(ArgType::String, None),
            },
            ArgDecl {
                name: ARG_INTERVAL.to_owned(),
                spec: arg_spec(ArgType::Int, Some(json!(INTERVAL))),
            },
        ];
        let mut inspector = script_section(state, ARGS_TAB_KEY, detail(None));
        assert_eq!(press_char(&mut inspector, EDIT_KEY), AutomationAction::None);
        assert!(inspector.text_input_active(), "{EDITOR_OPENS}");
        inspector
    }

    fn save_edit(inspector: &mut AutomationInspector, text: &str) -> AutomationAction {
        inspector
            .editor
            .as_mut()
            .expect(EDITOR_OPENS)
            .set_text(text);
        inspector.handle_key(key::SAVE.to_key_event(), &[])
    }

    fn editor_error(inspector: &AutomationInspector) -> Option<&str> {
        inspector.editor.as_ref().and_then(Editor::error)
    }

    #[test]
    fn opening_on_a_firing_selects_its_script_and_asks_for_the_trace() {
        let mut inspector = AutomationInspector::new();

        let requests = inspector.open(Arc::new(catalog()), Some(FIRE_ID));

        assert_eq!(
            requests,
            [sessions_request(), inspect(SCRIPT), firing_request(FIRE_ID)]
        );
        assert_eq!(
            inspector.selection,
            Selection::Automation(SCRIPT.to_owned())
        );
        assert_eq!(inspector.automation_section, AutomationSection::Firings);
    }

    #[test]
    fn the_list_puts_the_session_first_then_the_catalog_by_availability() {
        let state = AutomationState {
            automations: vec![
                script(
                    BROKEN_SCRIPT,
                    Availability::Invalid {
                        reason: ERROR_MESSAGE.to_owned(),
                    },
                ),
                script(UNTRUSTED_SCRIPT, Availability::NeedsTrust),
                script(OTHER_SCRIPT, Availability::Available),
                script(SCRIPT, Availability::Armed),
            ],
            ..AutomationState::default()
        };
        let groups: Vec<Group> = list::entries(&state, &[], "")
            .iter()
            .map(Entry::group)
            .collect();
        assert_eq!(
            groups,
            [
                Group::Session,
                Group::Armed,
                Group::Available,
                Group::NeedsTrust,
                Group::Invalid
            ]
        );

        let rows = draw(&mut opened(state, None), &mut terminal());

        let drawn: Vec<usize> = groups
            .iter()
            .map(|group| {
                rows.iter()
                    .position(|row| row.contains(group.label()))
                    .expect(GROUP_DRAWN)
            })
            .collect();
        assert!(
            drawn.windows(2).all(|pair| pair[0] < pair[1]),
            "{GROUPS_IN_ORDER}: {rows:#?}"
        );
    }

    #[test_case(AutomationStatus::Idle, IDLE_GLYPH; "idle")]
    #[test_case(AutomationStatus::Running, RUNNING_GLYPH; "running")]
    #[test_case(AutomationStatus::Queued { waiting: WAITING }, QUEUED_GLYPH; "queued")]
    #[test_case(AutomationStatus::Deferred { until: NOW + MINUTE_MS }, DEFERRED_GLYPH; "deferred")]
    #[test_case(AutomationStatus::BackingOff { until: NOW + MINUTE_MS }, BACKING_OFF_GLYPH; "backing_off")]
    #[test_case(AutomationStatus::Paused, PAUSED_GLYPH; "paused")]
    #[test_case(AutomationStatus::Failed, FAILED_GLYPH; "failed")]
    fn a_script_row_leads_with_its_status_glyph(status: AutomationStatus, glyph: &str) {
        let state = AutomationState {
            automations: vec![AutomationSnapshot {
                status,
                ..script(SCRIPT, Availability::Armed)
            }],
            ..AutomationState::default()
        };

        let row = line_text(&list::row(
            &list::entries(&state, &[], "")[1],
            &state,
            Style::default(),
            NOW,
        ));

        let lead = match status {
            AutomationStatus::Queued { waiting } => format!("{glyph}{waiting}"),
            _ => glyph.to_owned(),
        };
        assert!(row.starts_with(&lead) && row.contains(SCRIPT), "{row}");
    }

    #[test]
    fn a_script_row_names_its_scope_and_when_its_last_firing_ended_and_how() {
        let finished_at = NOW - 5 * MINUTE_MS;
        let state = AutomationState {
            automations: vec![AutomationSnapshot {
                last_firing: Some(FiringSummary {
                    finished_at: Some(finished_at),
                    ..firing(FIRE_ID, FiringStatus::Completed, finished_at - MINUTE_MS)
                }),
                ..script(SCRIPT, Availability::Armed)
            }],
            ..AutomationState::default()
        };

        let row = line_text(&list::row(
            &list::entries(&state, &[], "")[1],
            &state,
            Style::default(),
            NOW,
        ));

        assert!(row.contains(scope_text(Scope::Project)), "{row}");
        assert!(row.contains(&relative(finished_at, NOW)), "{row}");
        assert!(row.ends_with(FiringStatus::Completed.as_str()), "{row}");
    }

    #[test_case(NAME_FILTER, SCRIPT; "a_name_in_any_case")]
    #[test_case(DESCRIPTION_FILTER, OTHER_SCRIPT; "a_description")]
    fn the_filter_matches_a_name_or_a_description(filter: &str, expected: &str) {
        let mut inspector = opened(catalog(), None);
        let _ = press_char(&mut inspector, FILTER_KEY);
        for character in filter.chars() {
            let _ = press_char(&mut inspector, character);
        }

        let listed: Vec<Selection> = list::entries(&inspector.state, &[], &inspector.filter.text())
            .iter()
            .filter(|entry| matches!(entry, Entry::Automation(_)))
            .map(Entry::selection)
            .collect();

        assert_eq!(listed, [Selection::Automation(expected.to_owned())]);
    }

    #[test]
    fn firings_group_by_where_they_are_and_count_what_they_stand_for() {
        let firings = recent();
        let mut body = Body::new(None);

        firings::firings(&mut body, &firings.iter().collect::<Vec<_>>(), false, NOW);

        let text = lines_of(&body);
        let headings: Vec<usize> = [(GROUP_WAITING, 2), (GROUP_RUNNING, 1), (GROUP_FINISHED, 2)]
            .iter()
            .map(|(group, count)| {
                text.iter()
                    .position(|line| *line == format!("{group} ({count})"))
                    .expect(HEADING_DRAWN)
            })
            .collect();
        assert!(
            headings.windows(2).all(|pair| pair[0] < pair[1]),
            "{text:#?}"
        );
        let order: Vec<Item> = body.items.iter().map(|(item, _)| item.clone()).collect();
        assert_eq!(
            order,
            [
                QUEUED_FIRE,
                DEFERRED_FIRE,
                RUNNING_FIRE,
                FIRE_ID,
                FAILED_FIRE
            ]
            .map(|fire_id| Item::Firing(fire_id.to_owned()))
        );
        let row_of = |fire_id: &str| {
            body.items
                .iter()
                .find(|(item, _)| *item == Item::Firing(fire_id.to_owned()))
                .map(|(_, line)| text[*line].as_str())
                .unwrap_or_default()
        };
        let finished = row_of(FIRE_ID);
        assert!(
            finished.contains(&format!(
                "{}{TRIGGER_INDEX}{TRIGGER_POSITION}",
                trigger_text(TriggerKind::MessageReceived)
            )),
            "{finished}"
        );
        assert!(finished.contains(&span(FIRING_TOOK_MS)), "{finished}");
        assert!(finished.contains(CONSUMED_BADGE), "{finished}");
        assert!(
            finished.contains(&format!("{REPEATS_PREFIX}{REPEATS}")),
            "{finished}"
        );
        assert!(!row_of(QUEUED_FIRE).contains(REPEATS_PREFIX), "{text:#?}");
        assert!(
            row_of(DEFERRED_FIRE).contains(&clock(NOW + 5 * MINUTE_MS)),
            "{text:#?}"
        );
        assert!(row_of(FAILED_FIRE).contains(ERROR_MESSAGE), "{text:#?}");
    }

    #[test]
    fn the_outbox_says_what_holds_each_delivery_and_when_it_expires() {
        let expiry = NOW + MINUTE_MS;
        let capped = WaitReason::UnattendedCap {
            cap: UNATTENDED_CAP,
        };
        let expiring = WaitReason::ExpiresAt { at: expiry };
        let items = [
            OutboxItem {
                expires_at: Some(expiry),
                ..outbox_item(SEQ, Some(capped))
            },
            OutboxItem {
                expires_at: Some(expiry),
                ..outbox_item(SEQ + 1, Some(expiring))
            },
        ];
        let mut body = Body::new(None);

        firings::outbox(&mut body, &items, true, NOW);

        let text = lines_of(&body);
        let under = |index: usize| text[body.items[index].1 + 1].as_str();
        assert!(under(0).contains(&wait_text(capped, NOW)), "{text:#?}");
        assert!(under(0).contains(&clock(expiry)), "{text:#?}");
        assert!(under(1).contains(&wait_text(expiring, NOW)), "{text:#?}");
        assert_eq!(under(1).matches(&clock(expiry)).count(), 1, "{text:#?}");
    }

    /// This window holds a settled session's turn, so an item nothing else
    /// holds goes once it closes; a `guide` item joins a busy session's turn.
    #[test_case(true, DeliveryMode::Next, STARTS_ONCE_CLOSED; "settled_next")]
    #[test_case(true, DeliveryMode::Guide, STARTS_ONCE_CLOSED; "settled_guide")]
    #[test_case(false, DeliveryMode::Guide, JOINS_RUNNING_TURN; "busy_guide")]
    #[test_case(false, DeliveryMode::Next, READY; "busy_next")]
    fn an_unheld_delivery_says_when_it_goes(settled: bool, delivery: DeliveryMode, expected: &str) {
        let items = [OutboxItem {
            delivery,
            ..outbox_item(SEQ, None)
        }];
        let mut body = Body::new(None);

        firings::outbox(&mut body, &items, settled, NOW);

        let text = lines_of(&body);
        assert!(text[body.items[0].1 + 1].contains(expected), "{text:#?}");
    }

    #[test_case(ActionStatus::Running, RUNNING; "running")]
    #[test_case(ActionStatus::Done, DONE; "done")]
    #[test_case(ActionStatus::Failed, FAILED; "failed")]
    #[test_case(ActionStatus::Refused, REFUSED; "refused")]
    #[test_case(ActionStatus::Queued, QUEUED; "queued")]
    #[test_case(ActionStatus::Delivered, DELIVERED; "delivered")]
    #[test_case(ActionStatus::Deduplicated, DEDUPLICATED; "deduplicated")]
    #[test_case(ActionStatus::Dropped, DROPPED; "dropped")]
    #[test_case(ActionStatus::Expired, EXPIRED; "expired")]
    #[test_case(ActionStatus::Interrupted, INTERRUPTED; "interrupted")]
    fn every_action_status_reads_as_its_word(status: ActionStatus, word: &str) {
        let (text, _) = trace::action_state(&action(status), NOW, &[]);

        assert!(text.starts_with(word), "{text}");
    }

    #[test_case(
        ActionRow { wait: Some(WaitReason::ModalOpen), expires_at: Some(NOW + MINUTE_MS), ..action(ActionStatus::Queued) },
        vec![wait_text(WaitReason::ModalOpen, NOW), clock(NOW + MINUTE_MS)];
        "a_queued_delivery_names_its_wait_and_expiry"
    )]
    #[test_case(
        ActionRow {
            delivered_at: Some(NOW),
            turn_outcome: Some(TurnOutcome::Completed),
            turn_cost: Some(TURN_COST),
            ..action(ActionStatus::Delivered)
        },
        vec![clock(NOW), outcome_text(TurnOutcome::Completed).to_owned(), format!("{TURN_COST:.4}")];
        "a_delivery_names_its_turn_outcome_and_cost"
    )]
    #[test_case(
        ActionRow { target: Some(JOINED_DELIVERY.to_owned()), ..action(ActionStatus::Deduplicated) },
        vec![JOINED_DELIVERY.to_owned()];
        "a_duplicate_names_the_delivery_it_joined"
    )]
    #[test_case(
        ActionRow { error: Some(ERROR_MESSAGE.to_owned()), ..action(ActionStatus::Failed) },
        vec![ERROR_MESSAGE.to_owned()];
        "a_failure_says_why"
    )]
    fn an_action_state_carries_its_details(row: ActionRow, details: Vec<String>) {
        let (text, _) = trace::action_state(&row, NOW, &[]);

        for detail in details {
            assert!(text.contains(&detail), "{detail:?} in {text:?}");
        }
    }

    #[test]
    fn a_failed_trace_points_at_its_error_and_quotes_that_line() {
        let inspector = traced(FiringDetail {
            error_source: Some(ERROR_SOURCE.to_owned()),
            ..trace_of(failed_firing(), Vec::new())
        });

        let text = lines_of(&inspector.body(NOW, None, &[]));

        let at = format!("{AT_LINE}{ERROR_LINE}{COLUMN_SEPARATOR}{ERROR_COLUMN}");
        assert!(
            text.iter().any(|line| line.starts_with(ERROR_KIND)
                && line.contains(ERROR_MESSAGE)
                && line.ends_with(&at)),
            "{text:#?}"
        );
        assert!(
            text.contains(&format!("{ERROR_LINE}{SOURCE_RULE}{ERROR_SOURCE}")),
            "{text:#?}"
        );
    }

    #[test]
    fn untrusted_event_text_is_marked_and_reaches_the_lines_escaped() {
        let inspector = traced(trace_of(finished_firing(), Vec::new()));

        let body = inspector.body(NOW, None, &[]);

        let text = lines_of(&body);
        assert!(
            text.iter().any(|line| line == UNTRUSTED_LEGEND),
            "{LEGEND_SHOWN}"
        );
        assert!(
            text.iter().all(|line| !line.contains(ESCAPE)),
            "{ESCAPED}: {text:#?}"
        );
        let tag = Value::String(UNTRUSTED_TAG.to_owned()).to_string();
        let drawn = body
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content == tag)
            .expect(TAG_DRAWN);
        assert_eq!(drawn.style, theme::current().tool_warning);
    }

    /// The cursor, its mark and the click targets all read the one walk that
    /// drew the section. Were they to part, an arrow key would mark one row
    /// while Enter and a click acted on another.
    #[test_case(on_session_firings; "session_firings")]
    #[test_case(on_session_outbox; "session_outbox")]
    #[test_case(on_script_firings; "script_firings")]
    #[test_case(on_script_state; "script_state")]
    #[test_case(on_opened_action; "trace_with_an_open_action")]
    #[test_case(on_started_run; "trace_with_a_mirrored_run")]
    #[test_case(on_peer_firings; "another_sessions_firings")]
    #[test_case(on_peer_state; "another_sessions_state")]
    #[test_case(on_dry_run_row; "firings_under_a_dry_run")]
    #[test_case(on_open_dry_run; "an_open_dry_run")]
    fn every_marked_row_is_a_row_the_cursor_can_reach(setup: fn() -> AutomationInspector) {
        let mut inspector = setup();
        let mut terminal = terminal();
        let runs = mirror(RunStatus::Active);
        let count = inspector.items().len();
        assert!(count > 1, "{SEVERAL_ITEMS}");

        for index in 0..count {
            draw_with(&mut inspector, &mut terminal, &runs);
            assert_eq!(inspector.cursor, index);
            assert_eq!(inspector.item_rows.len(), count, "{TARGETS_MATCH_ITEMS}");
            assert_eq!(
                marked_rows(&terminal, inspector.body_area),
                [item_row(&inspector, index).y],
                "{MARK_ON_TARGET}"
            );
            let _ = press(&mut inspector, KeyCode::Down);
        }

        assert_eq!(inspector.cursor, count - 1, "{CURSOR_CLAMPS}");
    }

    #[test_case(RunStatus::Active, STARTED_RUN => mirrored_text(RunStatus::Active); "an_active_run")]
    #[test_case(RunStatus::Completed, STARTED_RUN => mirrored_text(RunStatus::Completed); "a_completed_run")]
    #[test_case(RunStatus::Active, OTHER_RUN => format!("{STARTED}{SEPARATOR}{OTHER_RUN}"); "a_run_the_mirror_lacks")]
    #[test_case(RunStatus::Active, DRY_RUN_ID => format!("{STARTED}{SEPARATOR}{DRY_RUN_ID}"); "a_dry_run")]
    fn a_started_run_reads_from_the_mirror(status: RunStatus, run_id: &str) -> String {
        trace::action_state(&start_action(run_id), NOW, &mirror(status)).0
    }

    #[test]
    fn the_trace_follows_the_mirror_as_the_run_moves() {
        let mut inspector = on_started_run();
        let mut terminal = terminal();
        let shows =
            |frame: &[String], status| frame.iter().any(|row| row.contains(&mirrored_text(status)));

        let active = draw_with(&mut inspector, &mut terminal, &mirror(RunStatus::Active));
        let completed = draw_with(&mut inspector, &mut terminal, &mirror(RunStatus::Completed));

        assert!(shows(&active, RunStatus::Active), "{FOLLOWS_THE_MIRROR}");
        assert!(
            shows(&completed, RunStatus::Completed) && !shows(&completed, RunStatus::Active),
            "{FOLLOWS_THE_MIRROR}"
        );
    }

    #[test_case(STARTED_RUN, true; "a_mirrored_run_opens")]
    #[test_case(OTHER_RUN, false; "a_run_the_mirror_lacks_toggles_its_body")]
    #[test_case(DRY_RUN_ID, false; "a_dry_run_toggles_its_body")]
    fn enter_on_a_started_run_opens_it_only_from_the_mirror(run_id: &str, opens: bool) {
        let mut inspector = on_start_of(run_id);

        let action = inspector.handle_key(key_event(KeyCode::Enter), &mirror(RunStatus::Active));

        let expected = match opens {
            true => AutomationAction::OpenWorkflowRun(run_id.to_owned()),
            false => AutomationAction::Request(AutomationRequest::ActionBody {
                fire_id: FIRE_ID.to_owned(),
                seq: SEQ,
            }),
        };
        assert_eq!(action, expected);
    }

    #[test]
    fn a_click_on_a_list_row_selects_its_script_and_asks_for_it() {
        let mut inspector = opened(catalog(), None);
        draw(&mut inspector, &mut terminal());
        let other = Selection::Automation(OTHER_SCRIPT.to_owned());
        let at = list_row(&inspector, &other);

        let action = click(&mut inspector, at);

        assert_eq!(action, AutomationAction::Request(inspect(OTHER_SCRIPT)));
        assert_eq!(inspector.selection, other);
    }

    #[test]
    fn the_wheel_over_the_list_walks_the_selection() {
        let mut inspector = opened(catalog(), None);
        draw(&mut inspector, &mut terminal());

        let action = inspector.scroll_at(list_row(&inspector, &Selection::Session), WHEEL_DOWN);

        assert_eq!(action, AutomationAction::Request(inspect(SCRIPT)));
    }

    #[test]
    fn a_hovered_firing_opens_on_one_click() {
        let mut inspector = on_session_firings();
        draw(&mut inspector, &mut terminal());
        let deferred = item_row(&inspector, 1);

        let hovered = inspector.handle_mouse(mouse(MouseEventKind::Moved, deferred), &[]);

        assert_eq!(hovered, AutomationAction::None);
        assert_eq!(
            inspector.cursor_item(),
            Some(Item::Firing(DEFERRED_FIRE.to_owned())),
            "{HOVER_PLACES_THE_CURSOR}"
        );
        assert_eq!(
            click(&mut inspector, deferred),
            AutomationAction::Request(firing_request(DEFERRED_FIRE))
        );
    }

    #[test]
    fn a_footer_click_runs_the_command_it_names() {
        let mut inspector = opened(catalog(), None);
        draw(&mut inspector, &mut terminal());
        let pause = FOOTER
            .iter()
            .position(|(_, _, command)| *command == FooterCommand::Pause)
            .map(|index| inspector.footer_hits.hit(index))
            .filter(|hit| !hit.is_empty())
            .expect(COMMAND_DRAWN);
        let at = Position::new(pause.x, pause.y);

        assert_eq!(click(&mut inspector, at), AutomationAction::None);
        assert_eq!(
            inspector.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), at), &[]),
            AutomationAction::Request(AutomationRequest::Pause {
                by: PauseSource::Inspector,
            })
        );
    }

    #[test]
    fn a_paste_lands_in_the_filter_only_once_it_has_the_focus() {
        let mut inspector = opened(catalog(), None);

        assert_eq!(
            inspector.handle_paste(OTHER_SCRIPT),
            None,
            "{UNFOCUSED_PASTE_PASSES}"
        );
        let _ = press_char(&mut inspector, FILTER_KEY);

        assert_eq!(
            inspector.handle_paste(OTHER_SCRIPT),
            Some(AutomationAction::None)
        );
        assert_eq!(inspector.filter.text(), OTHER_SCRIPT);
    }

    #[test]
    fn an_open_edit_keeps_the_pointer_and_the_paste() {
        let mut inspector = on_script_state();
        let mut terminal = terminal();
        draw(&mut inspector, &mut terminal);
        let outside = Position::new(inspector.popup.x - 1, inspector.popup.y);
        assert!(!inspector.contains(outside), "{PRESS_OUTSIDE_IS_THE_APPS}");
        assert_eq!(press_char(&mut inspector, EDIT_KEY), AutomationAction::None);
        draw(&mut inspector, &mut terminal);

        assert!(inspector.contains(outside), "{EDIT_KEEPS_PRESSES}");
        assert_eq!(click(&mut inspector, outside), AutomationAction::None);
        assert_eq!(
            inspector.scroll_at(outside, WHEEL_DOWN),
            AutomationAction::None
        );
        assert_eq!(inspector.handle_paste(NOTE), Some(AutomationAction::None));
        let text = inspector.editor.as_ref().expect(EDITOR_OPENS).text();
        assert!(text.contains(NOTE), "{text}");
    }

    #[test]
    fn enter_opens_a_firing_then_loads_an_actions_request_and_result_once() {
        let mut inspector = on_script_firings();
        step_to(&mut inspector, &Item::Firing(FIRE_ID.to_owned()));

        assert_eq!(
            press(&mut inspector, KeyCode::Enter),
            AutomationAction::Request(firing_request(FIRE_ID))
        );
        let landed = inspector.apply_response(
            &firing_request(FIRE_ID),
            Ok(AutomationResponse::Firing(Box::new(trace_of(
                finished_firing(),
                vec![action(ActionStatus::Done)],
            )))),
        );
        assert_eq!(landed, AutomationAction::None);
        step_to(&mut inspector, &Item::Action(SEQ));
        let body_request = AutomationRequest::ActionBody {
            fire_id: FIRE_ID.to_owned(),
            seq: SEQ,
        };
        assert_eq!(
            press(&mut inspector, KeyCode::Enter),
            AutomationAction::Request(body_request.clone())
        );
        let _ = inspector.apply_response(
            &body_request,
            Ok(AutomationResponse::ActionBody(Box::new(action_body()))),
        );
        assert!(body_shows(&inspector, REQUEST_TEXT) && body_shows(&inspector, RESULT_TEXT));

        assert_eq!(
            press(&mut inspector, KeyCode::Enter),
            AutomationAction::None
        );
        assert!(!body_shows(&inspector, REQUEST_TEXT));
        assert_eq!(
            press(&mut inspector, KeyCode::Enter),
            AutomationAction::None,
            "{OPENS_FROM_CACHE}"
        );
        assert!(body_shows(&inspector, RESULT_TEXT));

        assert_eq!(press(&mut inspector, KeyCode::Esc), AutomationAction::None);
        assert_eq!(
            inspector.cursor_item(),
            Some(Item::Firing(FIRE_ID.to_owned())),
            "{BACK_TO_ROW}"
        );
        assert_eq!(press(&mut inspector, KeyCode::Esc), AutomationAction::Close);
    }

    #[test]
    fn a_refresh_that_changes_the_selected_script_asks_for_its_detail_again() {
        let mut inspector = opened(catalog(), Some(SCRIPT));
        let _ = inspector.apply_response(
            &inspect(SCRIPT),
            Ok(AutomationResponse::Detail(Box::new(detail(None)))),
        );
        assert!(
            inspector.sync(Arc::new(catalog())).is_empty(),
            "{UNCHANGED_ASKS_NOTHING}"
        );
        let mut changed = catalog();
        changed.automations[0].status = AutomationStatus::Running;

        assert_eq!(inspector.sync(Arc::new(changed)), [inspect(SCRIPT)]);
    }

    #[test]
    fn a_refresh_that_moves_the_traced_firing_asks_for_its_trace_again() {
        let mut inspector = traced(trace_of(finished_firing(), Vec::new()));
        let mut moved = catalog();
        moved.recent = vec![FiringSummary {
            status: FiringStatus::Failed,
            ..finished_firing()
        }];

        assert_eq!(inspector.sync(Arc::new(moved)), [firing_request(FIRE_ID)]);
    }

    #[test_case(None, AutomationRequest::Arm { name: SCRIPT.to_owned(), args: None, origin: ArmOrigin::Manual }; "arms_it_by_hand")]
    #[test_case(Some(ArmOrigin::Profile), AutomationRequest::Disarm { name: SCRIPT.to_owned() }; "disarms_it")]
    fn space_toggles_whether_the_script_is_armed(
        armed: Option<ArmOrigin>,
        expected: AutomationRequest,
    ) {
        let mut state = catalog();
        state.automations[0].armed = armed;
        let mut inspector = opened(state, Some(SCRIPT));

        assert_eq!(
            press_char(&mut inspector, ARM_KEY),
            AutomationAction::Request(expected)
        );
    }

    #[test_case(ARM_KEY; "arm")]
    #[test_case(EDIT_KEY; "edit")]
    #[test_case(TRUST_KEY; "trust")]
    #[test_case(CLEAR_KEY; "clear")]
    fn a_script_key_on_the_session_asks_for_a_script(key: char) {
        let mut inspector = opened(catalog(), None);

        assert_eq!(
            press_char(&mut inspector, key),
            AutomationAction::Flash(NOT_AN_AUTOMATION.to_owned())
        );
    }

    #[test]
    fn trust_shows_the_digest_and_asks_only_once_confirmed() {
        let mut state = catalog();
        state.automations[0].trust = Trust::Required;
        let mut inspector = opened(state, Some(SCRIPT));

        assert_eq!(
            press_char(&mut inspector, TRUST_KEY),
            AutomationAction::None,
            "{CONFIRM_FIRST}"
        );
        let rows = draw(&mut inspector, &mut terminal());
        assert!(
            rows.iter()
                .any(|row| row.contains(&format!("{DIGEST_PROMPT}{DIGEST}"))),
            "{PROMPT_DRAWN}: {rows:#?}"
        );
        assert_eq!(
            press(&mut inspector, KeyCode::Enter),
            AutomationAction::Request(AutomationRequest::Trust {
                name: SCRIPT.to_owned(),
                digest: DIGEST.to_owned(),
            })
        );
    }

    #[test]
    fn a_trusted_script_has_nothing_to_trust() {
        let mut inspector = opened(catalog(), Some(SCRIPT));

        assert_eq!(
            press_char(&mut inspector, TRUST_KEY),
            AutomationAction::Flash(ALREADY_TRUSTED.to_owned())
        );
    }

    #[test_case(None, AutomationRequest::Pause { by: PauseSource::Inspector }; "pauses_a_running_session")]
    #[test_case(
        Some(PauseLatch { reason: String::new(), source: PauseSource::User, at: NOW }),
        AutomationRequest::Resume;
        "resumes_a_paused_one"
    )]
    fn p_toggles_the_session_pause(pause: Option<PauseLatch>, expected: AutomationRequest) {
        let mut state = catalog();
        state.session.controls.pause = pause;
        let mut inspector = opened(state, None);

        assert_eq!(
            press_char(&mut inspector, PAUSE_KEY),
            AutomationAction::Request(expected)
        );
    }

    #[test_case(on_session_firings, Some(DropTarget::Firing { fire_id: QUEUED_FIRE.to_owned() }); "a_queued_firing")]
    #[test_case(on_session_outbox, Some(DropTarget::OutboxItem { fire_id: FIRE_ID.to_owned(), seq: SEQ }); "an_outbox_item")]
    #[test_case(on_queued_delivery, Some(DropTarget::OutboxItem { fire_id: FIRE_ID.to_owned(), seq: SEQ }); "a_queued_delivery_in_its_trace")]
    #[test_case(on_finished_firing, None; "nothing_for_a_finished_firing")]
    fn x_drops_what_waits_under_the_cursor(
        setup: fn() -> AutomationInspector,
        target: Option<DropTarget>,
    ) {
        let mut inspector = setup();

        let expected = match target {
            Some(target) => AutomationAction::Request(AutomationRequest::Drop(target)),
            None => AutomationAction::Flash(NOTHING_TO_DROP.to_owned()),
        };
        assert_eq!(press_char(&mut inspector, DROP_KEY), expected);
    }

    #[test]
    fn clear_asks_at_the_loaded_revision_once_confirmed() {
        let mut inspector = on_script_state();

        assert_eq!(
            press_char(&mut inspector, CLEAR_KEY),
            AutomationAction::None,
            "{CONFIRM_FIRST}"
        );
        assert_eq!(press(&mut inspector, KeyCode::Esc), AutomationAction::None);
        assert!(inspector.is_open(), "{ESC_CANCELS}");
        assert_eq!(
            press_char(&mut inspector, CLEAR_KEY),
            AutomationAction::None
        );
        assert_eq!(
            press(&mut inspector, KeyCode::Enter),
            AutomationAction::Request(AutomationRequest::ClearState {
                name: SCRIPT.to_owned(),
                expected_revision: STATE_REVISION,
            })
        );
    }

    #[test]
    fn clear_waits_for_a_loaded_state() {
        let mut inspector = opened(catalog(), Some(SCRIPT));
        assert_eq!(
            press_char(&mut inspector, CLEAR_KEY),
            AutomationAction::Flash(STATE_LOADING.to_owned())
        );
        let _ = inspector.apply_response(
            &inspect(SCRIPT),
            Ok(AutomationResponse::Detail(Box::new(detail(None)))),
        );

        assert_eq!(
            press_char(&mut inspector, CLEAR_KEY),
            AutomationAction::Flash(NO_STATE_TO_CLEAR.to_owned())
        );
    }

    #[test_case(on_failed_firing, Some(ERROR_LINE); "a_failed_firing_at_its_error")]
    #[test_case(on_done_action, Some(ACTION_LINE); "an_action_at_its_call")]
    #[test_case(on_dry_run_action, Some(ACTION_LINE); "a_dry_run_action_at_its_call")]
    #[test_case(on_script_list, None; "the_script_itself")]
    fn o_opens_the_script_at_the_line_that_matters(
        setup: fn() -> AutomationInspector,
        line: Option<u32>,
    ) {
        let mut inspector = setup();

        assert_eq!(
            press_char(&mut inspector, SCRIPT_KEY),
            AutomationAction::OpenScript {
                path: PathBuf::from(SCRIPT_PATH),
                line,
            }
        );
    }

    #[test]
    fn copy_hands_over_the_firing_under_the_cursor_as_markdown() {
        let mut inspector = on_script_firings();
        step_to(&mut inspector, &Item::Firing(FIRE_ID.to_owned()));

        let AutomationAction::Copy(markdown) = press_char(&mut inspector, COPY_KEY) else {
            panic!("{COPIES_MARKDOWN}");
        };

        assert!(
            markdown.starts_with(&format!("{MARKDOWN_TITLE}{FIRE_ID}")),
            "{markdown}"
        );
    }

    #[test_case(json!([REPEATS]), StateError::NotObject; "a_list")]
    #[test_case(json!({ UNTRUSTED_TAG: NOTE }), StateError::NotObject; "a_bare_untrusted_wrapper")]
    #[test_case(json!({ RESERVED_KEY: REPEATS }), StateError::ReservedKey(RESERVED_KEY.to_owned()); "a_reserved_key")]
    fn the_state_editor_refuses_what_the_runtime_would(edited: Value, refusal: StateError) {
        let mut inspector = editing_state(Some(state_view(STATE_REVISION, tagged_state())));

        let action = save_edit(&mut inspector, &edited.to_string());

        assert_eq!(action, AutomationAction::None, "{NOTHING_SENT}");
        assert_eq!(editor_error(&inspector), Some(refusal.to_string().as_str()));
    }

    #[test_case(tagged_state(); "keeping_the_untrusted_wrapper")]
    #[test_case(json!({ NOTE_KEY: NOTE, COUNT_KEY: [REPEATS, REPEATS] }); "removing_the_untrusted_wrapper")]
    fn the_state_editor_saves_at_the_revision_it_loaded(edited: Value) {
        let mut inspector = editing_state(Some(state_view(STATE_REVISION, tagged_state())));

        let action = save_edit(&mut inspector, &edited.to_string());

        assert_eq!(
            action,
            AutomationAction::Request(AutomationRequest::SetState {
                name: SCRIPT.to_owned(),
                state: edited,
                expected_revision: STATE_REVISION,
            })
        );
    }

    #[test]
    fn a_state_conflict_keeps_the_editor_open_on_the_current_revision() {
        let mut inspector = editing_state(Some(state_view(STATE_REVISION, tagged_state())));
        let edited = json!({ COUNT_KEY: REPEATS });
        let AutomationAction::Request(request) = save_edit(&mut inspector, &edited.to_string())
        else {
            panic!("{SAVE_SENT}");
        };

        let action = inspector.apply_response(
            &request,
            Err(AutomationError::StateConflict {
                name: SCRIPT.to_owned(),
                current: CONFLICT_REVISION,
            }),
        );

        assert_eq!(action, AutomationAction::Request(inspect(SCRIPT)));
        let editor = inspector.editor.as_ref().expect(EDITOR_STAYS_OPEN);
        assert_eq!(editor.expected_revision(), Some(CONFLICT_REVISION));
        let current = json!({ NOTE_KEY: NOTE });
        let _ = inspector.apply_response(
            &inspect(SCRIPT),
            Ok(AutomationResponse::Detail(Box::new(detail(Some(
                state_view(CONFLICT_REVISION, current.clone()),
            ))))),
        );
        let editor = inspector.editor.as_ref().expect(EDITOR_STAYS_OPEN);
        assert_eq!(
            serde_json::from_str::<Value>(&editor.text()).ok(),
            Some(current.clone()),
            "{RELOADED}"
        );
        let rows = draw(&mut inspector, &mut terminal());
        assert!(
            rows.iter()
                .any(|row| row.contains(&format!("{CONFLICT_PREFIX}{CONFLICT_REVISION}"))),
            "{CONFLICT_SHOWN}: {rows:#?}"
        );
        assert_eq!(
            save_edit(&mut inspector, &current.to_string()),
            AutomationAction::Request(AutomationRequest::SetState {
                name: SCRIPT.to_owned(),
                state: current,
                expected_revision: CONFLICT_REVISION,
            })
        );
    }

    #[test]
    fn the_args_editor_starts_with_every_required_arg_to_fill_in() {
        let inspector = editing_args();

        let text = inspector.editor.as_ref().expect(EDITOR_OPENS).text();

        assert_eq!(
            serde_json::from_str::<Value>(&text).ok(),
            Some(json!({ ARG_TOPIC: null })),
            "{REQUIRED_ARGS_TO_FILL}"
        );
    }

    #[test_case(json!({ ARG_TOPIC: null }), ArgsError::Missing(ARG_TOPIC.to_owned()); "a_required_arg_left_null")]
    #[test_case(json!({ ARG_TOPIC: TOPIC, UNDECLARED_ARG: TOPIC }), ArgsError::Undeclared(UNDECLARED_ARG.to_owned()); "an_undeclared_arg")]
    #[test_case(
        json!({ ARG_TOPIC: TOPIC, ARG_INTERVAL: TOPIC }),
        ArgsError::Invalid { name: ARG_INTERVAL.to_owned(), error: ValueError::WrongType(ArgType::Int) };
        "a_value_of_the_wrong_type"
    )]
    fn the_args_editor_refuses_what_arming_would(edited: Value, refusal: ArgsError) {
        let mut inspector = editing_args();

        let action = save_edit(&mut inspector, &edited.to_string());

        assert_eq!(action, AutomationAction::None, "{NOTHING_SENT}");
        assert_eq!(editor_error(&inspector), Some(refusal.to_string().as_str()));
    }

    #[test]
    fn the_args_editor_refuses_text_that_is_not_json() {
        let mut inspector = editing_args();

        let action = save_edit(&mut inspector, NOT_JSON);

        assert_eq!(action, AutomationAction::None, "{NOTHING_SENT}");
        assert!(
            editor_error(&inspector).is_some_and(|error| error.starts_with(ARGS_NOT_JSON)),
            "{:?}",
            editor_error(&inspector)
        );
    }

    #[test]
    fn the_args_editor_saves_args_that_resolve() {
        let mut inspector = editing_args();
        let args = json!({ ARG_TOPIC: TOPIC });

        assert_eq!(
            save_edit(&mut inspector, &args.to_string()),
            AutomationAction::Request(AutomationRequest::SetArgs {
                name: SCRIPT.to_owned(),
                args,
            })
        );
    }

    #[test]
    fn opening_asks_for_other_sessions_automations() {
        let mut inspector = AutomationInspector::new();

        assert_eq!(
            inspector.open(Arc::new(catalog()), None),
            [sessions_request()]
        );
    }

    #[test]
    fn each_other_session_heads_its_automations_and_its_heading_selects_nothing() {
        let mut inspector = with_peers();
        let rows = draw(&mut inspector, &mut terminal());
        let quiet_selection = Selection::Other {
            session_id: QUIET_SESSION.to_owned(),
            name: QUIET_AUTOMATION.to_owned(),
        };
        let peer_row = list_row(&inspector, &peer_selection());
        assert!(
            rows[usize::from(peer_row.y - 2)].contains(Group::OtherSessions.label()),
            "{GROUP_DRAWN}"
        );

        for (selection, title) in [
            (peer_selection(), PEER_TITLE),
            (quiet_selection, QUIET_TITLE),
        ] {
            let row = list_row(&inspector, &selection);
            let heading = Position::new(row.x, row.y - 1);
            assert!(
                rows[usize::from(heading.y)].contains(title),
                "{HEADING_ABOVE_ROWS}: {rows:#?}"
            );

            assert_eq!(
                click(&mut inspector, heading),
                AutomationAction::None,
                "{HEADING_SELECTS_NOTHING}"
            );
            assert_eq!(inspector.selection, Selection::Session);
        }
    }

    #[test]
    fn the_arrows_walk_every_listed_row_through_other_sessions() {
        let mut inspector = with_peers();
        let listed: Vec<Selection> = list::entries(&inspector.state, &inspector.sessions, "")
            .iter()
            .map(Entry::selection)
            .collect();
        let mut walked = vec![inspector.selection.clone()];

        for _ in 1..listed.len() {
            let _ = press(&mut inspector, KeyCode::Down);
            walked.push(inspector.selection.clone());
        }

        assert_eq!(walked, listed);
        assert!(walked.contains(&peer_selection()), "{ITEM_LISTED}");
    }

    #[test]
    fn a_sessions_heading_shows_its_title_name_online_mark_and_last_activity() {
        let session = peer();

        let heading = line_text(&list::session_row(&session, true, NOW, HEADING_WIDTH));

        for part in [
            PEER_TITLE.to_owned(),
            handle_address(PEER_HANDLE),
            ONLINE.to_owned(),
            relative(session.last_activity_at, NOW),
        ] {
            assert!(heading.contains(&part), "{part:?} in {heading:?}");
        }
    }

    #[test]
    fn only_sessions_the_live_directory_lists_are_marked_online() {
        let mut inspector = with_peers();
        let mut terminal = terminal();
        let peer_name = handle_address(PEER_HANDLE);
        let marked = |rows: &[String], heading: &str| {
            rows.iter()
                .find(|row| row.contains(heading))
                .expect(ROW_DRAWN)
                .contains(ONLINE)
        };

        inspector.set_online(HashSet::from([PEER_SESSION.to_owned()]));
        let rows = draw(&mut inspector, &mut terminal);
        assert!(marked(&rows, &peer_name), "{MARKED_ONLINE}");
        assert!(!marked(&rows, QUIET_TITLE), "{NOT_MARKED}");

        inspector.set_online(HashSet::new());
        let rows = draw(&mut inspector, &mut terminal);
        assert!(!marked(&rows, &peer_name), "{NOT_MARKED}");
    }

    #[test_case(TITLE_FILTER, PEER_SESSION, SCRIPT; "a_sessions_title_in_any_case")]
    #[test_case(HANDLE_FILTER, PEER_SESSION, SCRIPT; "a_sessions_name")]
    #[test_case(AUTOMATION_FILTER, QUIET_SESSION, QUIET_AUTOMATION; "an_automations_name")]
    fn the_filter_matches_another_sessions_title_or_name(
        filter: &str,
        session_id: &str,
        name: &str,
    ) {
        let mut inspector = with_peers();
        let _ = press_char(&mut inspector, FILTER_KEY);
        for character in filter.chars() {
            let _ = press_char(&mut inspector, character);
        }

        let listed: Vec<Selection> = list::entries(
            &inspector.state,
            &inspector.sessions,
            &inspector.filter.text(),
        )
        .iter()
        .filter(|entry| !matches!(entry, Entry::Session))
        .map(Entry::selection)
        .collect();

        assert_eq!(
            listed,
            [Selection::Other {
                session_id: session_id.to_owned(),
                name: name.to_owned(),
            }]
        );
    }

    #[test]
    fn a_click_on_a_row_named_like_another_selects_it_in_its_own_session() {
        let mut inspector = with_peers();
        let mut terminal = terminal();
        let this = Selection::Automation(SCRIPT.to_owned());

        for (selection, request) in [(peer_selection(), inspect_peer()), (this, inspect(SCRIPT))] {
            draw(&mut inspector, &mut terminal);
            let at = list_row(&inspector, &selection);

            assert_eq!(
                click(&mut inspector, at),
                AutomationAction::Request(request)
            );
            assert_eq!(inspector.selection, selection);
        }
    }

    #[test_case(Selection::Automation(SCRIPT.to_owned()), Some(PEER_SESSION); "another_sessions_detail_leaves_this_sessions_script")]
    #[test_case(peer_selection(), None; "this_sessions_detail_leaves_another_sessions_script")]
    #[test_case(peer_selection(), Some(QUIET_SESSION); "a_third_sessions_detail_leaves_another_sessions_script")]
    fn a_detail_lands_only_on_the_session_it_was_asked_of(
        selection: Selection,
        stray: Option<&str>,
    ) {
        let mut inspector = with_peers();
        let AutomationAction::Request(asked) = inspector.select(selection) else {
            panic!("{DETAIL_LANDED}");
        };
        let stray = AutomationRequest::Inspect {
            name: SCRIPT.to_owned(),
            session_id: stray.map(str::to_owned),
        };

        let _ = inspector.apply_response(
            &stray,
            Ok(AutomationResponse::Detail(Box::new(detail(None)))),
        );
        assert!(
            matches!(inspector.detail, Some(Loading::Requested)),
            "{STRAY_DETAIL_IGNORED}"
        );

        let _ = inspector.apply_response(
            &asked,
            Ok(AutomationResponse::Detail(Box::new(peer_detail(None)))),
        );
        assert!(
            matches!(inspector.detail, Some(Loading::Loaded(_))),
            "{DETAIL_LANDED}"
        );
    }

    #[test_case(Selection::Automation(SCRIPT.to_owned()), Some(inspect(SCRIPT)); "this_sessions_script_is_asked_again")]
    #[test_case(peer_selection(), None; "another_sessions_script_is_left_alone")]
    fn a_write_to_this_sessions_script_asks_again_only_for_it(
        selection: Selection,
        expected: Option<AutomationRequest>,
    ) {
        let mut inspector = with_peers();
        let _ = inspector.select(selection);
        let cleared = AutomationRequest::ClearState {
            name: SCRIPT.to_owned(),
            expected_revision: STATE_REVISION,
        };

        let action = inspector.apply_response(
            &cleared,
            Ok(AutomationResponse::State {
                revision: CONFLICT_REVISION,
            }),
        );

        assert_eq!(requested(action), Vec::from_iter(expected));
    }

    #[test]
    fn another_sessions_automation_is_read_again_on_the_interval_while_selected() {
        let mut inspector = with_peers();
        let start = Instant::now();
        assert_eq!(
            inspector.poll(start + REREAD_EVERY),
            None,
            "{NOTHING_TO_READ}"
        );
        assert_eq!(
            inspector.select(peer_selection()),
            AutomationAction::Request(inspect_peer())
        );

        assert_eq!(inspector.poll(start), None, "{NOT_BEFORE}");
        assert_eq!(
            inspector.poll(start + REREAD_EVERY - JUST_BEFORE),
            None,
            "{NOT_BEFORE}"
        );
        assert_eq!(
            inspector.poll(start + REREAD_EVERY),
            Some(inspect_peer()),
            "{READ_AGAIN}"
        );
        assert_eq!(inspector.poll(start + REREAD_EVERY), None, "{NOT_BEFORE}");
        assert_eq!(
            inspector.poll(start + REREAD_EVERY * 2),
            Some(inspect_peer()),
            "{READ_AGAIN}"
        );

        let _ = press(&mut inspector, KeyCode::Up);
        assert_eq!(
            inspector.selection,
            Selection::Automation(OTHER_SCRIPT.to_owned())
        );
        assert_eq!(
            inspector.poll(start + REREAD_EVERY * 4),
            None,
            "{STOPS_ONCE_MOVED}"
        );
    }

    #[test_case(KeyCode::Enter, false; "open")]
    #[test_case(KeyCode::Char(ARM_KEY), true; "arm")]
    #[test_case(KeyCode::Char(EDIT_KEY), true; "edit")]
    #[test_case(KeyCode::Char(TRUST_KEY), true; "trust")]
    #[test_case(KeyCode::Char(PAUSE_KEY), true; "pause")]
    #[test_case(KeyCode::Char(CLEAR_KEY), true; "clear")]
    #[test_case(KeyCode::Char(DROP_KEY), true; "drop")]
    #[test_case(KeyCode::Char(DRY_RUN_KEY), true; "dry_run")]
    #[test_case(KeyCode::Char(SCRIPT_KEY), true; "script")]
    #[test_case(KeyCode::Char(COPY_KEY), false; "copy")]
    #[test_case(KeyCode::Char(FILTER_KEY), false; "filter")]
    #[test_case(KeyCode::Esc, false; "close")]
    fn a_footer_key_on_another_sessions_automation_sends_nothing_that_acts(
        code: KeyCode,
        refused: bool,
    ) {
        let mut inspector = on_peer_firings();

        let action = press(&mut inspector, code);

        assert_eq!(
            action == AutomationAction::Flash(READ_ONLY.to_owned()),
            refused,
            "{action:?}"
        );
        assert!(!requested(action).iter().any(acts), "{NOTHING_ACTS}");
    }

    #[test]
    fn a_footer_click_on_a_control_of_another_sessions_automation_sends_nothing() {
        let mut inspector = on_peer_firings();
        let mut terminal = terminal();

        for (index, (_, _, command)) in FOOTER.iter().enumerate() {
            if !command.controls() {
                continue;
            }
            draw(&mut inspector, &mut terminal);
            let hit = Some(inspector.footer_hits.hit(index))
                .filter(|hit| !hit.is_empty())
                .expect(COMMAND_DRAWN);
            let at = Position::new(hit.x, hit.y);

            assert_eq!(click(&mut inspector, at), AutomationAction::None);
            assert_eq!(
                inspector.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), at), &[]),
                AutomationAction::Flash(READ_ONLY.to_owned()),
                "{command:?}"
            );
        }
    }

    #[test_case(FooterCommand::Open, true; "open")]
    #[test_case(FooterCommand::Arm, false; "arm")]
    #[test_case(FooterCommand::Edit, false; "edit")]
    #[test_case(FooterCommand::Trust, false; "trust")]
    #[test_case(FooterCommand::Pause, false; "pause")]
    #[test_case(FooterCommand::Clear, false; "clear")]
    #[test_case(FooterCommand::Drop, false; "drop")]
    #[test_case(FooterCommand::DryRun, false; "dry_run")]
    #[test_case(FooterCommand::Script, false; "script")]
    #[test_case(FooterCommand::Copy, true; "copy")]
    fn the_footer_greys_every_control_on_another_sessions_automation(
        command: FooterCommand,
        enabled: bool,
    ) {
        let inspector = on_peer_firings();

        assert_eq!(
            inspector.enabled(command, inspector.cursor_item().as_ref()),
            enabled
        );
    }

    #[test]
    fn enter_and_copy_still_read_another_sessions_firing() {
        let mut inspector = on_peer_firings();

        let AutomationAction::Copy(markdown) = press_char(&mut inspector, COPY_KEY) else {
            panic!("{COPIES_MARKDOWN}");
        };
        assert!(
            markdown.starts_with(&format!("{MARKDOWN_TITLE}{PEER_FIRE}")),
            "{markdown}"
        );
        assert_eq!(
            press(&mut inspector, KeyCode::Enter),
            AutomationAction::Request(firing_request(PEER_FIRE))
        );
    }

    #[test]
    fn another_sessions_overview_names_its_session_and_says_it_is_read_only() {
        let mut inspector = peer_section(OVERVIEW_TAB_KEY);
        inspector.set_online(HashSet::from([PEER_SESSION.to_owned()]));

        let text = lines_of(&inspector.body(NOW, None, &[]));

        let owner = text
            .iter()
            .find(|line| line.contains(PEER_TITLE))
            .expect(ROW_DRAWN);
        assert!(
            owner.contains(&handle_address(PEER_HANDLE)) && owner.contains(ONLINE),
            "{owner}"
        );
        assert!(
            text.iter().any(|line| line == overview::READ_ONLY_NOTE),
            "{text:#?}"
        );
    }

    #[test]
    fn another_sessions_state_shows_without_saying_how_to_edit_it() {
        let inspector = on_peer_state();

        assert!(body_shows(&inspector, NOTE), "{ITEM_LISTED}");
        assert!(!body_shows(&inspector, editor::STATE_HINT));
    }

    fn dry_run_request(fire_id: &str) -> AutomationRequest {
        AutomationRequest::DryRun {
            fire_id: fire_id.to_owned(),
        }
    }

    /// A dry run of [`FIRE_ID`] by the script at `digest`: its one action
    /// answered as `answer`, and `limited` the limit a real firing would have
    /// met.
    fn replay(digest: &str, answer: Answer, limited: Option<LimitRefusal>) -> DryRunDetail {
        let ran = FiringSummary {
            fire_id: DRY_RUN_ID.to_owned(),
            digest: digest.to_owned(),
            ..finished_firing()
        };
        DryRunDetail {
            fire_id: FIRE_ID.to_owned(),
            trace: trace_of(ran, vec![action(ActionStatus::Done)]),
            answers: vec![answer],
            limited,
            state_revision: STATE_REVISION,
        }
    }

    fn replayed(replay: DryRunDetail) -> Result<AutomationResponse, AutomationError> {
        Ok(AutomationResponse::DryRun(Box::new(replay)))
    }

    fn cut_event() -> AutomationError {
        AutomationError::NotReplayable {
            fire_id: FIRE_ID.to_owned(),
            reason: REPLAY_EVENT_CUT.to_owned(),
        }
    }

    fn unknown_firing() -> AutomationError {
        AutomationError::UnknownFiring {
            fire_id: FIRE_ID.to_owned(),
        }
    }

    fn cooldown() -> LimitRefusal {
        LimitRefusal {
            reason: LimitReason::Cooldown,
            until: NOW + MINUTE_MS,
        }
    }

    /// `r` on [`FIRE_ID`] in the session's Firings, before its answer.
    fn dry_running() -> AutomationInspector {
        let mut inspector = on_finished_firing();
        assert_eq!(
            press_char(&mut inspector, DRY_RUN_KEY),
            AutomationAction::Request(dry_run_request(FIRE_ID)),
            "{DRY_RUN_ASKED}"
        );
        inspector
    }

    fn dry_run_answered(
        answer: Result<AutomationResponse, AutomationError>,
    ) -> AutomationInspector {
        let mut inspector = dry_running();
        let landed = inspector.apply_response(&dry_run_request(FIRE_ID), answer);
        assert_eq!(landed, AutomationAction::None);
        inspector
    }

    /// A dry run whose one action the journal answered, the cursor on its
    /// row.
    fn on_dry_run_row() -> AutomationInspector {
        dry_run_answered(replayed(replay(DIGEST, Answer::Journal, None)))
    }

    fn on_open_dry_run() -> AutomationInspector {
        let mut inspector = on_dry_run_row();
        assert_eq!(
            press(&mut inspector, KeyCode::Enter),
            AutomationAction::None
        );
        inspector
    }

    fn on_dry_run_action() -> AutomationInspector {
        let mut inspector = on_open_dry_run();
        step_to(&mut inspector, &Item::Action(SEQ));
        inspector
    }

    fn select_script(inspector: &mut AutomationInspector) {
        let _ = inspector.select(Selection::Automation(SCRIPT.to_owned()));
    }

    fn select_other_script(inspector: &mut AutomationInspector) {
        let _ = inspector.select(Selection::Automation(OTHER_SCRIPT.to_owned()));
    }

    fn select_session(inspector: &mut AutomationInspector) {
        let _ = inspector.select(Selection::Session);
    }

    #[test_case(on_session_firings, Some(FIRE_ID), AutomationAction::Request(dry_run_request(FIRE_ID)); "a_finished_firing_of_the_session")]
    #[test_case(on_script_firings, Some(FAILED_FIRE), AutomationAction::Request(dry_run_request(FAILED_FIRE)); "a_failed_firing_of_a_script")]
    #[test_case(on_done_action, None, AutomationAction::Request(dry_run_request(FIRE_ID)); "the_open_trace_of_a_finished_firing")]
    #[test_case(on_session_firings, Some(QUEUED_FIRE), AutomationAction::Flash(NOTHING_TO_REPLAY.to_owned()); "a_queued_firing")]
    #[test_case(on_session_firings, Some(DEFERRED_FIRE), AutomationAction::Flash(NOTHING_TO_REPLAY.to_owned()); "a_deferred_firing")]
    #[test_case(on_session_firings, Some(RUNNING_FIRE), AutomationAction::Flash(NOTHING_TO_REPLAY.to_owned()); "a_running_firing")]
    #[test_case(on_peer_firings, Some(PEER_DONE_FIRE), AutomationAction::Flash(READ_ONLY.to_owned()); "another_sessions_finished_firing")]
    #[test_case(dry_running, Some(FIRE_ID), AutomationAction::Flash(DRY_RUN_BUSY.to_owned()); "a_finished_firing_while_a_dry_run_runs")]
    #[test_case(on_dry_run_row, None, AutomationAction::Flash(NOTHING_TO_REPLAY.to_owned()); "a_dry_runs_row")]
    #[test_case(on_open_dry_run, None, AutomationAction::Flash(NOTHING_TO_REPLAY.to_owned()); "an_open_dry_run")]
    fn r_replays_only_a_finished_firing_of_this_session(
        setup: fn() -> AutomationInspector,
        firing: Option<&str>,
        expected: AutomationAction,
    ) {
        let mut inspector = setup();
        if let Some(fire_id) = firing {
            step_to(&mut inspector, &Item::Firing(fire_id.to_owned()));
        }

        assert_eq!(
            inspector.enabled(FooterCommand::DryRun, inspector.cursor_item().as_ref()),
            matches!(expected, AutomationAction::Request(_)),
            "{FOOTER_AGREES}"
        );
        assert_eq!(press_char(&mut inspector, DRY_RUN_KEY), expected);
    }

    #[test_case(on_finished_firing, select_script; "the_session_moving_to_a_script")]
    #[test_case(on_failed_firing, select_other_script; "a_script_moving_to_another")]
    #[test_case(on_failed_firing, select_session; "a_script_moving_to_the_session")]
    #[test_case(on_finished_firing, AutomationInspector::close; "closing")]
    fn a_dry_run_stays_until_the_selection_moves_or_the_inspector_closes(
        setup: fn() -> AutomationInspector,
        leave: fn(&mut AutomationInspector),
    ) {
        let mut inspector = setup();
        let AutomationAction::Request(asked) = press_char(&mut inspector, DRY_RUN_KEY) else {
            panic!("{DRY_RUN_ASKED}");
        };
        let _ = inspector.apply_response(&asked, replayed(replay(DIGEST, Answer::Recorded, None)));
        let _ = press(&mut inspector, KeyCode::Enter);
        let _ = press_char(&mut inspector, OVERVIEW_TAB_KEY);
        let _ = press_char(&mut inspector, FIRINGS_TAB_KEY);
        assert_eq!(
            inspector.items().first(),
            Some(&Item::DryRun),
            "{DRY_RUN_HELD}"
        );

        leave(&mut inspector);

        assert!(inspector.dry_run.is_none(), "{DRY_RUN_DROPPED}");
    }

    #[test]
    fn an_answer_to_a_dry_run_the_selection_dropped_lands_nowhere() {
        let mut inspector = dry_running();
        select_script(&mut inspector);
        select_session(&mut inspector);
        step_to(&mut inspector, &Item::Firing(FAILED_FIRE.to_owned()));
        assert_eq!(
            press_char(&mut inspector, DRY_RUN_KEY),
            AutomationAction::Request(dry_run_request(FAILED_FIRE))
        );

        for answer in [
            replayed(replay(DIGEST, Answer::Recorded, None)),
            Err(unknown_firing()),
        ] {
            let _ = inspector.apply_response(&dry_run_request(FIRE_ID), answer);
            assert!(
                inspector.dry_run.as_ref().is_some_and(DryRun::loading),
                "{STALE_DROPPED}"
            );
        }
    }

    #[test_case(None, format!("{SEPARATOR}{LOADING}"); "while_it_runs")]
    #[test_case(Some(replayed(replay(DIGEST, Answer::Recorded, None))), format!("{SEPARATOR}{}", firings::summary(&finished_firing())); "what_it_did")]
    #[test_case(Some(Err(cut_event())), format!("{SEPARATOR}{CANNOT_REPLAY}{REPLAY_EVENT_CUT}"); "why_it_could_not_run")]
    #[test_case(Some(Err(unknown_firing())), format!("{SEPARATOR}{}", unknown_firing()); "any_other_refusal")]
    fn the_dry_run_leads_firings_under_its_badge(
        answer: Option<Result<AutomationResponse, AutomationError>>,
        tail: String,
    ) {
        let mut inspector = dry_running();
        if let Some(answer) = answer {
            let _ = inspector.apply_response(&dry_run_request(FIRE_ID), answer);
        }

        let row = lines_of(&inspector.body(NOW, None, &[])).remove(0);

        assert_eq!(inspector.cursor_item(), Some(Item::DryRun), "{ROW_ON_TOP}");
        assert!(row.contains(BADGE) && row.ends_with(&tail), "{row}");
    }

    #[test]
    fn an_open_dry_run_says_why_it_could_not_run() {
        let mut inspector = dry_run_answered(Err(cut_event()));
        assert_eq!(
            press(&mut inspector, KeyCode::Enter),
            AutomationAction::None
        );

        assert_eq!(
            lines_of(&inspector.body(NOW, None, &[])),
            [
                format!("{DRY_RUN_TITLE}{FIRE_ID}"),
                format!("{CANNOT_REPLAY}{REPLAY_EVENT_CUT}")
            ]
        );
    }

    #[test_case(Answer::Recorded, RECORDED; "recorded")]
    #[test_case(Answer::Journal, JOURNAL; "journal")]
    #[test_case(Answer::Stubbed, STUBBED; "stubbed")]
    #[test_case(Answer::Cut, CUT; "cut")]
    fn every_dry_run_answer_reads_as_its_badge(answer: Answer, badge: &str) {
        let mut inspector = dry_run_answered(replayed(replay(DIGEST, answer, None)));
        let _ = press(&mut inspector, KeyCode::Enter);

        let text = lines_of(&inspector.body(NOW, None, &[]));

        let row = text
            .iter()
            .find(|line| line.contains(ACTION_SUMMARY))
            .expect(ROW_DRAWN);
        assert!(
            row.ends_with(&format!("{ACTION_SUMMARY}{SEPARATOR}{badge}")),
            "{row}"
        );
    }

    #[test_case(DIGEST, None; "the_same_script_and_no_limit")]
    #[test_case(DRY_DIGEST, None; "a_script_changed_since")]
    #[test_case(DIGEST, Some(cooldown()); "a_limit_a_real_firing_would_meet")]
    fn an_open_dry_run_notes_its_script_its_state_and_its_limit(
        digest: &str,
        limited: Option<LimitRefusal>,
    ) {
        let limit_note = limited.as_ref().map(|limit| {
            format!(
                "{LIMITED}{}{UNTIL}{}",
                limit_text(limit.reason),
                moment(limit.until, NOW)
            )
        });
        let mut inspector = dry_run_answered(replayed(replay(digest, Answer::Recorded, limited)));
        let _ = press(&mut inspector, KeyCode::Enter);

        let text = lines_of(&inspector.body(NOW, None, &[]));

        let shows = |note: &str| text.iter().any(|line| line == note);
        assert_eq!(shows(SCRIPT_CHANGED), digest != DIGEST, "{text:#?}");
        assert!(
            shows(&format!("{RAN_AGAINST}{STATE_REVISION}")),
            "{text:#?}"
        );
        let limit_notes: Vec<&String> = text
            .iter()
            .filter(|line| line.starts_with(LIMITED))
            .collect();
        assert_eq!(limit_notes, Vec::from_iter(limit_note.as_ref()));
    }

    #[test]
    fn an_open_dry_run_shows_the_state_change_it_would_commit() {
        let inspector = on_open_dry_run();

        assert!(
            inspector
                .items()
                .iter()
                .any(|item| matches!(item, Item::Fold(FoldScope::Patch, _))),
            "{PATCH_DRAWN}"
        );
    }

    #[test]
    fn enter_on_a_dry_run_action_asks_for_no_body() {
        let mut inspector = on_dry_run_action();

        assert_eq!(
            press(&mut inspector, KeyCode::Enter),
            AutomationAction::None,
            "{NO_BODY}"
        );
        assert!(!body_shows(&inspector, LOADING), "{NO_BODY}");
    }

    #[test]
    fn esc_steps_out_of_the_dry_run_onto_its_row() {
        let mut inspector = on_dry_run_action();

        assert_eq!(press(&mut inspector, KeyCode::Esc), AutomationAction::None);
        assert_eq!(
            inspector.cursor_item(),
            Some(Item::DryRun),
            "{BACK_TO_DRY_RUN}"
        );
        assert_eq!(press(&mut inspector, KeyCode::Esc), AutomationAction::Close);
    }

    #[test_case(on_dry_run_row; "its_row")]
    #[test_case(on_open_dry_run; "it_open")]
    fn y_copies_the_dry_run_as_markdown_with_its_badges_and_notes(
        setup: fn() -> AutomationInspector,
    ) {
        let mut inspector = setup();

        let AutomationAction::Copy(markdown) = press_char(&mut inspector, COPY_KEY) else {
            panic!("{COPIES_MARKDOWN}");
        };

        assert!(
            markdown.starts_with(&format!("{MARKDOWN_DRY_RUN_TITLE}{FIRE_ID}")),
            "{markdown}"
        );
        assert!(
            markdown
                .lines()
                .any(|line| line.contains(ACTION_SUMMARY) && line.contains(JOURNAL)),
            "{markdown}"
        );
        assert!(
            markdown.contains(&format!("{RAN_AGAINST}{STATE_REVISION}")),
            "{markdown}"
        );
    }
}
