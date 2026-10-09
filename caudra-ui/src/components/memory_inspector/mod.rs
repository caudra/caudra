//! `/memory`: the memory's mechanism as it runs. The outline holds the view
//! in the order the model reads it, each line opening into the two it was
//! made from down to the entries; the detail pane says where the selected node
//! stands, who wrote the entry under it, and draws the few nodes around it.

mod detail;
mod outline;

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use caudra_agent::memory::search::Hit;
use caudra_agent::memory::snapshot::{EntryStatus, MemorySnapshot};
use caudra_agent::memory::store::now_ms;
use caudra_agent::memory::tree::{Block, Line as ViewLine, Part, VIEW, zoom_path};
use caudra_grab::grab_scope;
use caudra_markdown::render::{MermaidStyle, TOOL_OUTPUT_MAX_LINE_BYTES};
use caudra_storage::memory_journal::{EntryKind, EntryMeta, EntryOrigin};
use caudra_workbench::text_field::{FieldKind, TextField, TextKey};
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use self::detail::Scene;
use self::outline::{
    Columns, Look, Row, RowKind, TailMark, line_text, one_line, row_line, tree_rows,
};
use crate::animation::{animation_elapsed_ms, spinner_str};
use crate::components::document_view::COPIED_SELECTION;
use crate::components::modal::{ESC_LABEL, FooterHits, FooterLine, Modal};
use crate::components::projection_modal::UNPREPARED;
use crate::components::scrollbar::{ScrollHint, Scrollbar, ScrollbarMouse};
use crate::components::{
    ModalScroll, Overlay, ascii_key, chevron_span, field_styles, input_text_style, visual_rows,
};
use crate::markdown::text_to_painted;
use crate::repaint::Cadence;
use crate::theme;

/// How often an open inspector reads the journal again while a summarizer,
/// in this process or another, may still be writing lines.
pub(crate) const RELOAD_INTERVAL: Duration = Duration::from_secs(2);
const TITLE: &str = " Memory";
const WIDTH_PERCENT: u16 = 90;
const MAX_HEIGHT_PERCENT: u16 = 85;
const OUTLINE_PERCENT: u16 = 60;
const PANE_GAP: u16 = 1;
const OUTLINE_MIN_COLS: u16 = 44;
const DETAIL_MIN_COLS: u16 = 36;
/// Narrower than this the detail docks under the outline. The workflow
/// inspector shows one pane at a time instead and switches with ←/→, but
/// here those keys fold the tree.
const SPLIT_MIN_COLS: u16 = OUTLINE_MIN_COLS + PANE_GAP + DETAIL_MIN_COLS;
/// The share of the rows a docked detail takes.
const DOCKED_PERCENT: u16 = 45;
const H_PAD: u16 = 1;
/// The header and the blank row under it.
const HEADER_ROWS: u16 = 2;
/// The two columns a chevron and its space take.
const CHEVRON_COLS: u16 = 2;
const KIB: usize = 1024;
const GAUGE_CELLS: usize = 10;
const CELL_EIGHTHS: usize = 8;
const GAUGE_EIGHTHS: usize = GAUGE_CELLS * CELL_EIGHTHS;
/// A cell filled one eighth to seven eighths.
const PARTIAL_CELLS: [char; 7] = [
    '\u{258f}', '\u{258e}', '\u{258d}', '\u{258c}', '\u{258b}', '\u{258a}', '\u{2589}',
];
const FULL_CELL: char = '\u{2588}';
const EMPTY_CELL: char = '\u{2591}';
const GAUGE_OPEN: &str = "\u{2595}";
const GAUGE_CLOSE: &str = "\u{258f}";
const SEPARATOR: &str = " \u{b7} ";
const LIVE_LABEL: &str = "Live view";
const SESSION_LABEL: &str = "This session";
const SUMMARIZING_OFF: &str = "summarizing off";
const LOADING: &str = "Loading\u{2026}";
const EMPTY: &str = "No notes yet: the agent writes them with the memory tool";
const NO_MATCH: &str = "No note matches";
const NO_SESSION_VIEW: &str = "This session's system prompt carries no memory view";
const TAIL_DIVIDER: &str = "\u{2500}\u{2500} written since this session's view was taken";
const REMINDER_SENTENCE: &str =
    "Written after this session's view was taken: a # Memory updated reminder named it.";
const WRITTEN_HERE_SENTENCE: &str =
    "Written by this session after its view was taken, so it needed no reminder.";
const SUMMARY_CAPTION: &str = "Its line";
const BODY_GONE: &str = "The journal no longer holds this entry";
const SEARCH_PLACEHOLDER: &str = "search notes";
const COPIED_LINE: &str = "Copied the line";
const COPIED_NOTE: &str = "Copied the note";
const NO_MERGE: &str = "No two lines of the view can merge yet";
const NOT_CURRENT: &str = "Delete and forget act on the newest entry of a note";
const NOTE_GONE: &str = "That note no longer exists";
const MERMAID_OPEN: &str = "```mermaid\n";
const MERMAID_CLOSE: &str = "\n```";
const DELETE_PROMPT: &str = " again deletes ";
const FORGET_PROMPT: &str = " again forgets ";
/// What forgetting costs, said while the footer has room for it.
const FORGET_WARNING: &str = ": every version, and every line that read it, is purged";
const KEEP_HINT: &str = " keeps it";
const ENTER_LABEL: &str = "Enter";
pub(crate) const SEARCH_LABEL: &str = "/";
pub(crate) const MERGE_LABEL: &str = "m";
pub(crate) const MODE_LABEL: &str = "s";
pub(crate) const COPY_LABEL: &str = "y";
pub(crate) const DELETE_LABEL: &str = "d";
pub(crate) const FORGET_LABEL: &str = "x";
const SEARCH_KEY: char = ascii_key(SEARCH_LABEL);
const MERGE_KEY: char = ascii_key(MERGE_LABEL);
const MODE_KEY: char = ascii_key(MODE_LABEL);
const COPY_KEY: char = ascii_key(COPY_LABEL);
const DELETE_KEY: char = ascii_key(DELETE_LABEL);
const FORGET_KEY: char = ascii_key(FORGET_LABEL);
const SESSION_GLOSS: &str = "session";
const LIVE_GLOSS: &str = "live";
/// Keys the footer names but does not take clicks for: a click cannot say
/// which way to move.
const GROUP_HINTS: [(&str, &str); 2] = [("\u{2191}\u{2193}", "move"), ("\u{2190}\u{2192}", "fold")];
const FOOTER: [(&str, &str, FooterCommand); 8] = [
    (ENTER_LABEL, "open", FooterCommand::Open),
    (SEARCH_LABEL, "search", FooterCommand::Search),
    (MERGE_LABEL, "next merge", FooterCommand::Merge),
    (MODE_LABEL, SESSION_GLOSS, FooterCommand::Mode),
    (COPY_LABEL, "copy", FooterCommand::Copy),
    (DELETE_LABEL, "delete", FooterCommand::Delete),
    (FORGET_LABEL, "forget", FooterCommand::Forget),
    (ESC_LABEL, "close", FooterCommand::Close),
];
const SECTION_GAP: &str = "  ";
/// What the footer puts between its keys once it has given up their words.
const KEY_GAP: &str = " ";
/// How the footer draws itself, widest first: glossed, then keys alone, then
/// keys packed. Every key is on every rung.
const FOOTER_RUNGS: [(bool, &str); 3] =
    [(true, SECTION_GAP), (false, SECTION_GAP), (false, KEY_GAP)];

/// Which view the outline shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum MemoryMode {
    /// The fold as it stands now.
    Live,
    /// The view the system prompt of this session's last run holds.
    Session,
}

/// What this session's system prompt holds.
enum SessionView {
    /// No run has bound a prompt yet.
    Unprepared,
    /// The bound prompt carries no memory view.
    Absent,
    Bound(Block),
}

impl SessionView {
    fn read(system: Option<&str>) -> Self {
        match system.filter(|system| !system.is_empty()) {
            None => Self::Unprepared,
            Some(system) => Block::parse(system).map_or(Self::Absent, Self::Bound),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verb {
    Delete,
    Forget,
}

/// A delete or a forget waiting for its second press.
struct Armed {
    verb: Verb,
    name: String,
}

/// A note's text on its way from the journal.
enum Body {
    Requested,
    Loaded(String),
    Missing,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiagramKey {
    part: Part,
    mode: MemoryMode,
    width: u16,
    theme: u64,
    mermaid: MermaidStyle,
}

/// The drawing last made, kept until the selection, the mode, the width or
/// the theme moves, or the journal is read again.
struct Diagram {
    key: DiagramKey,
    lines: Vec<Line<'static>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FooterCommand {
    Open,
    Search,
    Merge,
    Mode,
    Copy,
    Delete,
    Forget,
    Close,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MemoryAction {
    Consumed,
    Close,
    Copy {
        text: String,
        label: &'static str,
    },
    /// The search field gave `text` to the clipboard and now holds `query`.
    Cut {
        text: String,
        query: Option<String>,
    },
    Flash(&'static str),
    /// The body of the entry with this number, which the outline only names.
    LoadBody(u64),
    Search(String),
    Open(String),
    Delete(String),
    Forget(String),
}

pub(crate) struct MemoryInspector {
    open: bool,
    snapshot: Option<MemorySnapshot>,
    failure: Option<String>,
    loaded_at: Option<Instant>,
    mode: MemoryMode,
    session: SessionView,
    session_id: String,
    summarizing: bool,
    expanded: HashSet<Part>,
    cursor: usize,
    /// Where the cursor was in the tree when search hits replaced it.
    tree_cursor: usize,
    search: TextField,
    search_focused: bool,
    hits: Option<Vec<Hit>>,
    /// The zoom path of the hit last revealed, which the detail spells out.
    revealed: Vec<Part>,
    armed: Option<Armed>,
    bodies: HashMap<u64, Body>,
    diagram: Option<Diagram>,
    popup: Rect,
    outline_area: Rect,
    detail_area: Rect,
    /// The row each drawn outline line shows: `None` for a divider or notice.
    outline_lines: Vec<Option<usize>>,
    outline_scroll: ModalScroll,
    detail_scroll: ModalScroll,
    scrollbar: Scrollbar,
    /// Set by whatever moves the cursor, consumed by the next draw.
    reveal_cursor: bool,
    footer: FooterLine,
    footer_hits: FooterHits,
}

impl MemoryInspector {
    pub(crate) fn new() -> Self {
        Self {
            open: false,
            snapshot: None,
            failure: None,
            loaded_at: None,
            mode: MemoryMode::Live,
            session: SessionView::Unprepared,
            session_id: String::new(),
            summarizing: true,
            expanded: HashSet::new(),
            cursor: 0,
            tree_cursor: 0,
            search: TextField::new(FieldKind::Line),
            search_focused: false,
            hits: None,
            revealed: Vec::new(),
            armed: None,
            bodies: HashMap::new(),
            diagram: None,
            popup: Rect::default(),
            outline_area: Rect::default(),
            detail_area: Rect::default(),
            outline_lines: Vec::new(),
            outline_scroll: ModalScroll::new_top(),
            detail_scroll: ModalScroll::new_top(),
            scrollbar: Scrollbar::default(),
            reveal_cursor: false,
            footer: FooterLine::default(),
            footer_hits: FooterHits::default(),
        }
    }

    /// Opens empty: the journal is read off the UI thread and lands through
    /// [`Self::fill`]. `system` is the prompt this session's last run bound.
    pub(crate) fn open(&mut self, system: Option<&str>, session_id: String, summarizing: bool) {
        *self = Self {
            open: true,
            session: SessionView::read(system),
            session_id,
            summarizing,
            ..Self::new()
        };
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    pub(crate) fn close(&mut self) {
        *self = Self::new();
    }

    pub(crate) fn contains(&self, pos: Position) -> bool {
        self.open && self.popup.contains(pos)
    }

    /// Takes a fresh read of the journal, keeping the cursor on the node it
    /// was on. The first read puts it on the newest line of the view.
    pub(crate) fn fill(
        &mut self,
        loaded: Result<MemorySnapshot, String>,
        system: Option<&str>,
    ) -> MemoryAction {
        if !self.open {
            return MemoryAction::Consumed;
        }
        self.loaded_at = Some(Instant::now());
        let snapshot = match loaded {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.failure = Some(error);
                return MemoryAction::Consumed;
            }
        };
        let anchor = self.selected().map(|row| row.part);
        self.failure = None;
        self.session = SessionView::read(system);
        self.bodies
            .retain(|seq, _| entry(&snapshot, *seq).is_some_and(|meta| !meta.forgotten));
        self.snapshot = Some(snapshot);
        self.diagram = None;
        let rows = self.rows();
        let last = rows.len().saturating_sub(1);
        self.cursor = match anchor {
            Some(part) => rows
                .iter()
                .position(|row| row.part == part)
                .unwrap_or(self.cursor),
            None => last,
        }
        .min(last);
        self.reveal_cursor = true;
        self.settle()
    }

    pub(crate) fn fill_body(&mut self, seq: u64, body: Result<Option<String>, String>) {
        if !self.bodies.contains_key(&seq) {
            return;
        }
        let body = match body {
            Ok(Some(text)) => Body::Loaded(text),
            Ok(None) => Body::Missing,
            Err(error) => Body::Failed(error),
        };
        self.bodies.insert(seq, body);
    }

    /// Hits for `query`, dropped when the field has moved on since.
    pub(crate) fn fill_hits(
        &mut self,
        query: &str,
        hits: Result<Vec<Hit>, String>,
    ) -> MemoryAction {
        if self.query().as_deref() != Some(query) {
            return MemoryAction::Consumed;
        }
        match hits {
            Ok(hits) => {
                if self.hits.is_none() {
                    self.tree_cursor = self.cursor;
                }
                self.hits = Some(hits);
                self.cursor = 0;
                self.move_to(0)
            }
            Err(error) => {
                self.failure = Some(error);
                MemoryAction::Consumed
            }
        }
    }

    /// Whether the journal is due another read: only while a line of the
    /// view waits for its summary or a summarizer holds a node, since nothing
    /// else changes without an event saying so.
    pub(crate) fn reload_due(&self) -> bool {
        self.polls()
            && self
                .loaded_at
                .is_some_and(|loaded| loaded.elapsed() >= RELOAD_INTERVAL)
    }

    fn polls(&self) -> bool {
        self.open
            && self.snapshot.as_ref().is_some_and(|snapshot| {
                snapshot
                    .view
                    .iter()
                    .any(|part| !snapshot.tree.is_built(part))
                    || snapshot.leases(now_ms()).next().is_some()
            })
    }

    /// The wheel scrolls whichever pane is under the pointer.
    pub(crate) fn scroll_at(&mut self, pos: Position, delta: i32) {
        if self.outline_area.contains(pos) {
            self.outline_scroll.scroll(delta);
        } else if self.detail_area.contains(pos) {
            self.detail_scroll.scroll(delta);
        }
    }

    pub(crate) fn text_input_active(&self) -> bool {
        self.open && self.search_focused
    }

    /// A paste lands in the search field when it has the focus, and nowhere
    /// else while the inspector is open. `None` when it is closed.
    pub(crate) fn handle_paste(&mut self, text: &str) -> Option<MemoryAction> {
        if !self.open {
            return None;
        }
        if !self.search_focused {
            return Some(MemoryAction::Consumed);
        }
        match self.search.paste(text) {
            TextKey::Changed => Some(self.search_changed()),
            _ => Some(MemoryAction::Consumed),
        }
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> MemoryAction {
        if self.search_focused {
            return self.handle_search_key(key);
        }
        let plain = key.modifiers.is_empty();
        let armed = self.armed.take();
        match key.code {
            KeyCode::Esc if armed.is_some() => MemoryAction::Consumed,
            KeyCode::Esc if self.hits.is_some() || !self.search.is_empty() => {
                self.clear_search();
                self.settle()
            }
            KeyCode::Esc => MemoryAction::Close,
            KeyCode::Up => self.step(-1),
            KeyCode::Down => self.step(1),
            KeyCode::PageUp => self.step(-self.page()),
            KeyCode::PageDown => self.step(self.page()),
            KeyCode::Home => self.move_to(0),
            KeyCode::End => self.move_to(usize::MAX),
            KeyCode::Right => self.expand(),
            KeyCode::Left => self.collapse(),
            KeyCode::Enter => self.activate(),
            KeyCode::Char(SEARCH_KEY) if plain => self.focus_search(),
            KeyCode::Char(MERGE_KEY) if plain => self.next_merge(),
            KeyCode::Char(MODE_KEY) if plain => self.switch_mode(),
            KeyCode::Char(COPY_KEY) if plain => self.copy(),
            KeyCode::Char(DELETE_KEY) if plain => self.arm(Verb::Delete, armed),
            KeyCode::Char(FORGET_KEY) if plain => self.arm(Verb::Forget, armed),
            _ => {
                self.detail_scroll.handle_key(key);
                MemoryAction::Consumed
            }
        }
    }

    /// The field takes the keys that type, and the cursor still walks the
    /// hits under it.
    fn handle_search_key(&mut self, key: KeyEvent) -> MemoryAction {
        match key.code {
            KeyCode::Esc => {
                self.clear_search();
                self.settle()
            }
            KeyCode::Enter => match self.selected() {
                Some(row) if matches!(row.kind, RowKind::Hit(_)) => self.reveal(row.part.index),
                _ => {
                    self.search_focused = false;
                    MemoryAction::Consumed
                }
            },
            KeyCode::Up => self.step(-1),
            KeyCode::Down => self.step(1),
            KeyCode::PageUp => self.step(-self.page()),
            KeyCode::PageDown => self.step(self.page()),
            _ => match self.search.handle_key(key) {
                TextKey::Changed => self.search_changed(),
                TextKey::Copy(text) => MemoryAction::Copy {
                    text,
                    label: COPIED_SELECTION,
                },
                TextKey::Cut(text) => {
                    if self.query().is_none() {
                        self.drop_hits();
                    }
                    MemoryAction::Cut {
                        text,
                        query: self.query(),
                    }
                }
                TextKey::Handled | TextKey::Refused | TextKey::Ignored => MemoryAction::Consumed,
            },
        }
    }

    pub(crate) fn handle_mouse(&mut self, event: MouseEvent) -> MemoryAction {
        match self.scrollbar.handle(&event) {
            ScrollbarMouse::Ignored => {}
            ScrollbarMouse::Consumed => return MemoryAction::Consumed,
            ScrollbarMouse::ScrollTo(top) => {
                self.detail_scroll
                    .scroll_to(u16::try_from(top).unwrap_or(u16::MAX));
                return MemoryAction::Consumed;
            }
        }
        if let Some(index) = self.footer_hits.handle_mouse(event) {
            return self.footer_command(index);
        }
        let pos = Position::new(event.column, event.row);
        if event.kind != MouseEventKind::Down(MouseButton::Left) || !self.outline_area.contains(pos)
        {
            return MemoryAction::Consumed;
        }
        let line = usize::from(pos.y - self.outline_area.y + self.outline_scroll.offset());
        let Some(&Some(index)) = self.outline_lines.get(line) else {
            return MemoryAction::Consumed;
        };
        self.armed = None;
        let Some(row) = self.rows().into_iter().nth(index) else {
            return MemoryAction::Consumed;
        };
        let chevron = self.outline_area.x.saturating_add(row.chevron_column());
        if row.has_children() && (chevron..chevron.saturating_add(CHEVRON_COLS)).contains(&pos.x) {
            self.toggle(&row.part);
        }
        self.move_to(index)
    }

    fn footer_command(&mut self, index: usize) -> MemoryAction {
        if let Some(armed) = self.armed.take() {
            return match index {
                0 => self.arm(armed.verb, Some(armed)),
                _ => MemoryAction::Consumed,
            };
        }
        match FOOTER.get(index).map(|(_, _, command)| *command) {
            Some(FooterCommand::Open) => self.activate(),
            Some(FooterCommand::Search) => self.focus_search(),
            Some(FooterCommand::Merge) => self.next_merge(),
            Some(FooterCommand::Mode) => self.switch_mode(),
            Some(FooterCommand::Copy) => self.copy(),
            Some(FooterCommand::Delete) => self.arm(Verb::Delete, None),
            Some(FooterCommand::Forget) => self.arm(Verb::Forget, None),
            Some(FooterCommand::Close) => MemoryAction::Close,
            None => MemoryAction::Consumed,
        }
    }

    /// Every row of the outline, in drawing order. Drawing, the cursor,
    /// clicks and copying all read this one enumeration.
    fn rows(&self) -> Vec<Row> {
        let Some(snapshot) = &self.snapshot else {
            return Vec::new();
        };
        if let Some(hits) = &self.hits {
            return hits
                .iter()
                .enumerate()
                .map(|(index, hit)| Row {
                    part: Part::leaf(hit.seq),
                    guides: String::new(),
                    kind: RowKind::Hit(index),
                })
                .collect();
        }
        let mut rows = tree_rows(&self.roots(), &self.expanded);
        if let Some(block) = self.frozen() {
            rows.extend((block.through()..snapshot.tree.entries()).map(|seq| Row {
                part: Part::leaf(seq),
                guides: String::new(),
                kind: RowKind::Tail(self.tail_mark(snapshot, seq)),
            }));
        }
        rows
    }

    /// The lines of the view on screen, oldest first.
    fn roots(&self) -> Vec<Part> {
        match (self.mode, &self.snapshot) {
            (MemoryMode::Live, Some(snapshot)) => snapshot.view.clone(),
            (MemoryMode::Session, Some(_)) => self.frozen().map_or_else(Vec::new, |block| {
                block.lines.iter().map(|line| line.part.clone()).collect()
            }),
            (_, None) => Vec::new(),
        }
    }

    /// The view this session's prompt holds, while the outline shows it.
    fn frozen(&self) -> Option<&Block> {
        match (self.mode, &self.session) {
            (MemoryMode::Session, SessionView::Bound(block)) => Some(block),
            _ => None,
        }
    }

    fn tail_mark(&self, snapshot: &MemorySnapshot, seq: u64) -> TailMark {
        match entry(snapshot, seq).map(|meta| &meta.origin) {
            Some(EntryOrigin::Session(id)) if *id == self.session_id => TailMark::WrittenHere,
            _ => TailMark::Reminder,
        }
    }

    fn selected(&self) -> Option<Row> {
        self.rows().into_iter().nth(self.cursor)
    }

    fn page(&self) -> isize {
        isize::try_from(self.outline_area.height.max(1)).unwrap_or(isize::MAX)
    }

    fn step(&mut self, delta: isize) -> MemoryAction {
        self.move_to(self.cursor.saturating_add_signed(delta))
    }

    /// Puts the cursor on row `index`, or the last row past the end.
    fn move_to(&mut self, index: usize) -> MemoryAction {
        let count = self.rows().len();
        if count == 0 {
            return MemoryAction::Consumed;
        }
        let index = index.min(count - 1);
        if index != self.cursor {
            self.detail_scroll.reset();
        }
        self.cursor = index;
        self.reveal_cursor = true;
        self.settle()
    }

    fn select_part(&mut self, part: &Part) -> MemoryAction {
        match self.rows().iter().position(|row| row.part == *part) {
            Some(index) => self.move_to(index),
            None => MemoryAction::Consumed,
        }
    }

    /// Asks for the body of the entry under the cursor, once.
    fn settle(&mut self) -> MemoryAction {
        let Some(seq) = self
            .selected()
            .filter(|row| row.part.level == 0)
            .map(|row| row.part.index)
        else {
            return MemoryAction::Consumed;
        };
        let wanted = self.snapshot.as_ref().is_some_and(|snapshot| {
            entry(snapshot, seq).is_some_and(|meta| meta.kind == EntryKind::Note && !meta.forgotten)
                && snapshot
                    .tree
                    .leaf(seq)
                    .is_some_and(|leaf| leaf.body.is_none())
        });
        if !wanted || self.bodies.contains_key(&seq) {
            return MemoryAction::Consumed;
        }
        self.bodies.insert(seq, Body::Requested);
        MemoryAction::LoadBody(seq)
    }

    /// → opens the line under the cursor, and on an open line steps into its
    /// first child.
    fn expand(&mut self) -> MemoryAction {
        match self.selected() {
            Some(row) if row.has_children() && !self.expanded.contains(&row.part) => {
                self.expanded.insert(row.part);
                MemoryAction::Consumed
            }
            Some(row) if row.has_children() => self.step(1),
            _ => MemoryAction::Consumed,
        }
    }

    /// ← closes the line under the cursor, and on a closed one steps to the
    /// line it was made into.
    fn collapse(&mut self) -> MemoryAction {
        let Some(row) = self.selected() else {
            return MemoryAction::Consumed;
        };
        if self.expanded.remove(&row.part) {
            return MemoryAction::Consumed;
        }
        let parent = row.part.parent();
        let rows = self.rows();
        match rows[..self.cursor]
            .iter()
            .rposition(|candidate| candidate.kind == RowKind::Node && candidate.part == parent)
        {
            Some(index) if row.kind == RowKind::Node => self.move_to(index),
            _ => MemoryAction::Consumed,
        }
    }

    fn toggle(&mut self, part: &Part) {
        if !self.expanded.remove(part) {
            self.expanded.insert(part.clone());
        }
    }

    /// Enter opens or closes a line, reveals a hit, and opens a note.
    fn activate(&mut self) -> MemoryAction {
        let Some(row) = self.selected() else {
            return MemoryAction::Consumed;
        };
        if let RowKind::Hit(_) = row.kind {
            return self.reveal(row.part.index);
        }
        if row.has_children() {
            self.toggle(&row.part);
            return MemoryAction::Consumed;
        }
        let name = self.snapshot.as_ref().and_then(|snapshot| {
            let name = &entry(snapshot, row.part.index)?.name;
            live(snapshot, name).then(|| name.clone())
        });
        name.map_or(MemoryAction::Flash(NOTE_GONE), MemoryAction::Open)
    }

    /// Opens exactly the lines between the view and the entry, the way the
    /// agent zooms down to it.
    fn reveal(&mut self, seq: u64) -> MemoryAction {
        let path = zoom_path(&self.roots(), seq);
        self.clear_search();
        self.expanded = path
            .iter()
            .take(path.len().saturating_sub(1))
            .cloned()
            .collect();
        self.revealed = path;
        self.select_part(&Part::leaf(seq))
    }

    fn focus_search(&mut self) -> MemoryAction {
        self.search_focused = true;
        MemoryAction::Consumed
    }

    fn query(&self) -> Option<String> {
        let text = self.search.text();
        let query = text.trim();
        (!query.is_empty()).then(|| query.to_owned())
    }

    fn search_changed(&mut self) -> MemoryAction {
        match self.query() {
            Some(query) => MemoryAction::Search(query),
            None => {
                self.drop_hits();
                self.settle()
            }
        }
    }

    fn clear_search(&mut self) {
        self.search.clear();
        self.search_focused = false;
        self.drop_hits();
    }

    fn drop_hits(&mut self) {
        if self.hits.take().is_some() {
            self.cursor = self.tree_cursor;
            self.reveal_cursor = true;
        }
    }

    /// The first pair the fold may merge, else the most due one, which waits
    /// for its parent.
    fn next_merge(&mut self) -> MemoryAction {
        let Some(snapshot) = &self.snapshot else {
            return MemoryAction::Consumed;
        };
        let Some(part) = snapshot
            .merges
            .iter()
            .find(|merge| merge.ready)
            .or(snapshot.merges.first())
            .and_then(|merge| snapshot.view.get(merge.at))
            .cloned()
        else {
            return MemoryAction::Flash(NO_MERGE);
        };
        self.mode = MemoryMode::Live;
        self.clear_search();
        self.select_part(&part)
    }

    fn switch_mode(&mut self) -> MemoryAction {
        let anchor = self.selected().map(|row| row.part);
        self.mode = match self.mode {
            MemoryMode::Live => MemoryMode::Session,
            MemoryMode::Session => MemoryMode::Live,
        };
        self.footer_hits.clear();
        let rows = self.rows();
        let index = anchor
            .and_then(|part| rows.iter().position(|row| row.part == part))
            .unwrap_or(usize::MAX);
        self.move_to(index)
    }

    /// `y` copies the line, or a note's text on an entry that has one.
    fn copy(&self) -> MemoryAction {
        let (Some(snapshot), Some(row)) = (&self.snapshot, self.selected()) else {
            return MemoryAction::Consumed;
        };
        if row.part.level == 0
            && entry(snapshot, row.part.index)
                .is_some_and(|meta| meta.kind == EntryKind::Note && !meta.forgotten)
        {
            return match self.body(snapshot, row.part.index) {
                Some(text) => MemoryAction::Copy {
                    text: text.to_owned(),
                    label: COPIED_NOTE,
                },
                None => MemoryAction::Flash(LOADING),
            };
        }
        MemoryAction::Copy {
            text: line_text(snapshot, self.frozen(), &row).into_owned(),
            label: COPIED_LINE,
        }
    }

    fn body<'a>(&'a self, snapshot: &'a MemorySnapshot, seq: u64) -> Option<&'a str> {
        snapshot
            .tree
            .leaf(seq)
            .and_then(|leaf| leaf.body.as_deref())
            .or_else(|| match self.bodies.get(&seq) {
                Some(Body::Loaded(text)) => Some(text.as_str()),
                _ => None,
            })
    }

    /// The first press names what a second would do; only the same key on
    /// the same note does it.
    fn arm(&mut self, verb: Verb, previous: Option<Armed>) -> MemoryAction {
        let Some(name) = self.current_note(verb) else {
            return MemoryAction::Flash(NOT_CURRENT);
        };
        if previous.is_some_and(|previous| previous.verb == verb && previous.name == name) {
            return match verb {
                Verb::Delete => MemoryAction::Delete(name),
                Verb::Forget => MemoryAction::Forget(name),
            };
        }
        self.armed = Some(Armed { verb, name });
        self.footer_hits.clear();
        MemoryAction::Consumed
    }

    /// The name of the note under the cursor, when the cursor is on its
    /// newest entry: a note to delete, or any entry to forget.
    fn current_note(&self, verb: Verb) -> Option<String> {
        let snapshot = self.snapshot.as_ref()?;
        let row = self.selected().filter(|row| row.part.level == 0)?;
        let meta = entry(snapshot, row.part.index)?;
        let current =
            snapshot.statuses.get(usize::try_from(meta.seq).ok()?)? == &EntryStatus::Current;
        let kind = verb == Verb::Forget || meta.kind == EntryKind::Note;
        (current && kind).then(|| meta.name.clone())
    }

    pub(crate) fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        if !self.open {
            return Rect::default();
        }
        grab_scope!("memory_inspector", area);
        let title = self.title();
        let modal = Modal {
            title: &title,
            width_percent: WIDTH_PERCENT,
            max_height_percent: MAX_HEIGHT_PERCENT,
        };
        let (popup, inner) = modal.render(frame, area, area.height);
        self.popup = popup;
        let padded = Rect {
            x: inner.x.saturating_add(H_PAD),
            width: inner.width.saturating_sub(H_PAD.saturating_mul(2)),
            ..inner
        };
        let input_rows = u16::from(self.search_focused || !self.search.is_empty());
        let [header, panes, input, footer] = Layout::vertical([
            Constraint::Length(HEADER_ROWS),
            Constraint::Fill(1),
            Constraint::Length(input_rows),
            Constraint::Length(1),
        ])
        .areas(padded);
        frame.render_widget(Paragraph::new(self.header_line()), header);
        let (outline, detail) = panes_of(panes);
        self.reveal_cursor |= outline.height != self.outline_area.height;
        self.outline_area = outline;
        self.detail_area = detail;
        self.render_outline(frame, outline);
        self.render_detail(frame, detail);
        if input_rows > 0 {
            self.render_search(frame, input);
        }
        self.footer = self.footer_line(footer.width);
        self.footer_hits.set(self.footer.hits(footer, 0, 1));
        frame.render_widget(
            Paragraph::new(self.footer.line(self.footer_hits.hovered())),
            footer,
        );
        popup
    }

    fn title(&self) -> String {
        match &self.snapshot {
            Some(snapshot) => {
                let entries = snapshot.tree.entries();
                format!(
                    "{TITLE}{SEPARATOR}{entries} {} ",
                    detail::entries_word(entries)
                )
            }
            None => format!("{TITLE} "),
        }
    }

    /// `Live view · 72 lines · 31.6 of 32 KiB ▕█████████▉▏ · 1 summarizing`.
    fn header_line(&self) -> Line<'static> {
        let t = theme::current();
        if let Some(failure) = &self.failure {
            return Line::styled(one_line(failure), t.error);
        }
        let Some(snapshot) = &self.snapshot else {
            return Line::styled(LOADING, t.tool_dim);
        };
        let now = now_ms();
        let mut spans = Vec::new();
        match (self.mode, &self.session) {
            (MemoryMode::Live, _) => {
                let size = snapshot.size();
                let over = size > VIEW;
                spans.push(Span::styled(LIVE_LABEL, t.accent));
                push_fact(&mut spans, lines_of(snapshot.view.len()), t.item_desc);
                push_gauge(&mut spans, size, over);
                if let Some(merge) = snapshot
                    .merges
                    .iter()
                    .find(|merge| !merge.ready)
                    .filter(|_| over)
                {
                    push_fact(
                        &mut spans,
                        format!("waits for {}", merge.parent),
                        t.tool_warning,
                    );
                }
                let leased = snapshot.leases(now).count();
                if leased > 0 {
                    push_fact(&mut spans, format!("{leased} summarizing"), t.spinner);
                }
                let pending = snapshot
                    .view
                    .iter()
                    .filter(|part| !snapshot.tree.is_built(part) && !snapshot.leased(part, now))
                    .count();
                if pending > 0 {
                    push_fact(&mut spans, format!("{pending} pending"), t.item_desc);
                }
            }
            (MemoryMode::Session, SessionView::Bound(block)) => {
                let size = block.lines.iter().map(ViewLine::size).sum();
                spans.push(Span::styled(SESSION_LABEL, t.accent));
                push_fact(&mut spans, lines_of(block.lines.len()), t.item_desc);
                push_gauge(&mut spans, size, size > VIEW);
                let since = snapshot.tree.entries().saturating_sub(block.through());
                if since > 0 {
                    push_fact(&mut spans, format!("{since} since"), t.item_desc);
                }
                if block.hidden > 0 {
                    push_fact(
                        &mut spans,
                        format!("{} oldest left out", block.hidden),
                        t.tool_warning,
                    );
                }
            }
            (MemoryMode::Session, _) => spans.push(Span::styled(SESSION_LABEL, t.accent)),
        }
        if !self.summarizing {
            push_fact(&mut spans, SUMMARIZING_OFF.to_owned(), t.tool_warning);
        }
        Line::from(spans)
    }

    fn render_outline(&mut self, frame: &mut Frame, area: Rect) {
        if area.is_empty() {
            return;
        }
        grab_scope!("memory_inspector_outline", area);
        let (lines, rows) = self.outline(area.width);
        let total = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        self.outline_scroll.update_dimensions(total, area.height);
        if self.reveal_cursor
            && let Some(line) = rows.iter().position(|row| *row == Some(self.cursor))
        {
            self.outline_scroll
                .reveal(u16::try_from(line).unwrap_or(u16::MAX), 1);
        }
        self.reveal_cursor = false;
        self.outline_lines = rows;
        frame.render_widget(
            Paragraph::new(lines).scroll((self.outline_scroll.offset(), 0)),
            area,
        );
    }

    /// The outline's lines, and the row each one draws.
    fn outline(&self, width: u16) -> (Vec<Line<'static>>, Vec<Option<usize>>) {
        let t = theme::current();
        let Some(snapshot) = &self.snapshot else {
            return (vec![Line::styled(LOADING, t.tool_dim)], vec![None]);
        };
        let rows = self.rows();
        let look = Look {
            snapshot,
            hits: self.hits.as_deref().unwrap_or_default(),
            frozen: self.frozen(),
            columns: Columns::of(&rows),
            now_ms: now_ms(),
            spinner: spinner_str(animation_elapsed_ms()),
            width: usize::from(width),
        };
        let mut lines = Vec::with_capacity(rows.len() + 1);
        let mut drawn = Vec::with_capacity(rows.len() + 1);
        for (index, row) in rows.iter().enumerate() {
            let first_tail = matches!(row.kind, RowKind::Tail(_))
                && index
                    .checked_sub(1)
                    .is_none_or(|before| !matches!(rows[before].kind, RowKind::Tail(_)));
            if first_tail {
                lines.push(Line::styled(TAIL_DIVIDER, t.tool_dim));
                drawn.push(None);
            }
            lines.push(row_line(
                row,
                &look,
                self.expanded.contains(&row.part),
                index == self.cursor,
            ));
            drawn.push(Some(index));
        }
        if lines.is_empty() {
            lines.push(Line::styled(self.empty_notice(snapshot), t.tool_dim));
            drawn.push(None);
        }
        (lines, drawn)
    }

    fn empty_notice(&self, snapshot: &MemorySnapshot) -> &'static str {
        match (&self.hits, self.mode, &self.session) {
            (Some(_), _, _) => NO_MATCH,
            (None, MemoryMode::Session, SessionView::Unprepared) => UNPREPARED,
            (None, MemoryMode::Session, SessionView::Absent) => NO_SESSION_VIEW,
            _ if snapshot.tree.entries() == 0 => EMPTY,
            _ => NO_MATCH,
        }
    }

    fn render_detail(&mut self, frame: &mut Frame, area: Rect) {
        if area.is_empty() {
            return;
        }
        grab_scope!("memory_inspector_detail", area);
        let lines = self.detail_lines(area.width, MermaidStyle::default());
        let rows = visual_rows(&lines, area.width);
        self.detail_scroll
            .update_dimensions(rows.total, area.height);
        let offset = self.detail_scroll.offset();
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((offset, 0)),
            area,
        );
        self.scrollbar.set_hint(ScrollHint::lines(
            u32::from(offset) + 1,
            u32::from(rows.total),
        ));
        self.scrollbar.draw(frame, area, rows.total, offset);
    }

    /// The selected node: its heading, where it stands, the entry under a
    /// leaf, its line, and the drawing of the nodes around it.
    fn detail_lines(&mut self, width: u16, mermaid: MermaidStyle) -> Vec<Line<'static>> {
        let Some(row) = self.selected() else {
            return Vec::new();
        };
        let diagram = self.diagram_lines(&row.part, width, mermaid);
        let Some(snapshot) = &self.snapshot else {
            return Vec::new();
        };
        let t = theme::current();
        let part = &row.part;
        let roots = self.roots();
        let scene = Scene {
            snapshot,
            roots: &roots,
            mode: self.mode,
            now_ms: now_ms(),
        };
        let mut lines = vec![detail::heading(snapshot, part), Line::default()];
        if let Some(sentence) = detail::placement(&scene, part) {
            lines.push(Line::styled(sentence, t.item));
        }
        match row.kind {
            RowKind::Tail(TailMark::Reminder) => {
                lines.push(Line::styled(REMINDER_SENTENCE, t.item))
            }
            RowKind::Tail(TailMark::WrittenHere) => {
                lines.push(Line::styled(WRITTEN_HERE_SENTENCE, t.item));
            }
            RowKind::Node | RowKind::Hit(_) => {}
        }
        if self.revealed.last() == Some(part) {
            lines.push(Line::styled(
                detail::breadcrumb(&self.revealed),
                t.item_desc,
            ));
        }
        let leaf = (part.level == 0)
            .then(|| entry(snapshot, part.index))
            .flatten();
        match leaf {
            Some(meta) => {
                lines.push(Line::default());
                lines.push(Line::styled(
                    detail::authorship(meta, &self.session_id),
                    t.item_desc,
                ));
                if let Some(status) = snapshot
                    .statuses
                    .get(usize::try_from(meta.seq).unwrap_or(usize::MAX))
                {
                    lines.push(Line::styled(detail::status_sentence(*status), t.item_desc));
                }
                lines.push(Line::default());
                lines.extend(self.body_lines(snapshot, meta, width));
                if let Some(built) = snapshot.tree.built(part).filter(|built| !built.verbatim) {
                    lines.push(Line::default());
                    lines.push(Line::styled(SUMMARY_CAPTION, t.tool_dim));
                    lines.extend(text_lines(&built.text, t.item));
                }
            }
            None => {
                lines.push(Line::default());
                lines.extend(text_lines(
                    &line_text(snapshot, self.frozen(), &row),
                    t.item,
                ));
            }
        }
        lines.push(Line::default());
        lines.extend(diagram);
        lines
    }

    /// The note's text as Markdown, or where it is on its way from the
    /// journal. Entries without text show nothing.
    fn body_lines(
        &self,
        snapshot: &MemorySnapshot,
        meta: &EntryMeta,
        width: u16,
    ) -> Vec<Line<'static>> {
        let t = theme::current();
        if meta.kind != EntryKind::Note || meta.forgotten {
            return Vec::new();
        }
        match (self.body(snapshot, meta.seq), self.bodies.get(&meta.seq)) {
            (Some(text), _) => {
                let (painted, _) = text_to_painted(
                    &escape_body(text),
                    "",
                    t.assistant,
                    t.assistant,
                    width,
                    Some(TOOL_OUTPUT_MAX_LINE_BYTES),
                    Vec::new(),
                );
                painted.lines
            }
            (None, Some(Body::Failed(error))) => vec![Line::styled(one_line(error), t.error)],
            (None, Some(Body::Missing)) => vec![Line::styled(BODY_GONE, t.tool_dim)],
            (None, _) => vec![Line::styled(LOADING, t.tool_dim)],
        }
    }

    /// The drawing of the nodes around `part`, from the cache when nothing it
    /// was drawn against has moved.
    fn diagram_lines(
        &mut self,
        part: &Part,
        width: u16,
        mermaid: MermaidStyle,
    ) -> Vec<Line<'static>> {
        let key = DiagramKey {
            part: part.clone(),
            mode: self.mode,
            width,
            theme: theme::generation(),
            mermaid,
        };
        if let Some(diagram) = self.diagram.as_ref().filter(|diagram| diagram.key == key) {
            return diagram.lines.clone();
        }
        let Some(snapshot) = &self.snapshot else {
            return Vec::new();
        };
        let roots = self.roots();
        let scene = Scene {
            snapshot,
            roots: &roots,
            mode: self.mode,
            now_ms: now_ms(),
        };
        let lines = match mermaid {
            MermaidStyle::Unicode => {
                let t = theme::current();
                let fence = format!(
                    "{MERMAID_OPEN}{}{MERMAID_CLOSE}",
                    detail::diagram_source(&scene, part)
                );
                text_to_painted(
                    &fence,
                    "",
                    t.assistant,
                    t.assistant,
                    width,
                    None,
                    Vec::new(),
                )
                .0
                .lines
            }
            MermaidStyle::Off => detail::diagram_tree(&scene, part),
        };
        self.diagram = Some(Diagram {
            key,
            lines: lines.clone(),
        });
        lines
    }

    fn render_search(&self, frame: &mut Frame, area: Rect) {
        let mut spans = vec![chevron_span()];
        let width = usize::from(area.width).saturating_sub(spans.iter().map(Span::width).sum());
        let styles = field_styles(input_text_style());
        spans.extend(
            self.search
                .paint(width, &styles, self.search_focused, SEARCH_PLACEHOLDER)
                .spans,
        );
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    /// The footer is one centred row, and a line wider than that row wraps
    /// and then answers no clicks at all, so a narrow modal gives up the
    /// words that gloss its keys, then the space between them, before a key.
    fn footer_line(&self, width: u16) -> FooterLine {
        if let Some(armed) = &self.armed {
            return armed_footer(armed, width);
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

    fn commands_footer(&self, glossed: bool, gap: &'static str) -> FooterLine {
        let t = theme::current();
        let mut footer = FooterLine::default();
        for (key, description) in GROUP_HINTS {
            footer.text(key, t.keybind_key);
            if glossed {
                footer.text(format!(" {description}"), t.tool_dim);
            }
            footer.text(gap, Style::default());
        }
        let selected = self.selected().is_some();
        for (index, (key, description, command)) in FOOTER.iter().enumerate() {
            if index > 0 {
                footer.text(gap, Style::default());
            }
            let enabled = match command {
                FooterCommand::Open | FooterCommand::Copy => selected,
                FooterCommand::Merge => self
                    .snapshot
                    .as_ref()
                    .is_some_and(|snapshot| !snapshot.merges.is_empty()),
                FooterCommand::Delete => self.current_note(Verb::Delete).is_some(),
                FooterCommand::Forget => self.current_note(Verb::Forget).is_some(),
                FooterCommand::Search | FooterCommand::Mode | FooterCommand::Close => true,
            };
            footer.command(
                key,
                match enabled {
                    true => t.keybind_key,
                    false => t.tool_dim,
                },
            );
            if glossed {
                let description = match (command, self.mode) {
                    (FooterCommand::Mode, MemoryMode::Session) => LIVE_GLOSS,
                    _ => description,
                };
                footer.describe(format!(" {description}"), t.tool_dim);
            }
        }
        footer
    }
}

impl Default for MemoryInspector {
    fn default() -> Self {
        Self::new()
    }
}

impl Overlay for MemoryInspector {
    fn is_open(&self) -> bool {
        self.open
    }

    fn close(&mut self) {
        self.close();
    }

    fn cadence(&self) -> Cadence {
        let leased = self
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.leases(now_ms()).next().is_some());
        Cadence::any([
            Cadence::when(self.open && leased, Cadence::SPINNER),
            Cadence::when(self.polls(), Cadence::polling(RELOAD_INTERVAL)),
        ])
    }
}

/// The outline over the detail when the panes are too narrow to sit side by
/// side, else side by side.
fn panes_of(area: Rect) -> (Rect, Rect) {
    if area.width >= SPLIT_MIN_COLS {
        let [outline, _, detail] = Layout::horizontal([
            Constraint::Length(area.width * OUTLINE_PERCENT / 100),
            Constraint::Length(PANE_GAP),
            Constraint::Fill(1),
        ])
        .areas(area);
        return (outline, detail);
    }
    let [outline, _, detail] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(PANE_GAP),
        Constraint::Length(area.height * DOCKED_PERCENT / 100),
    ])
    .areas(area);
    (outline, detail)
}

/// The armed footer says what the second press does, and the whole warning
/// for a forget while it fits.
fn armed_footer(armed: &Armed, width: u16) -> FooterLine {
    let t = theme::current();
    let name = one_line(&armed.name);
    let sayings: Vec<String> = match armed.verb {
        Verb::Delete => vec![format!("{DELETE_PROMPT}{name}")],
        Verb::Forget => vec![
            format!("{FORGET_PROMPT}{name}{FORGET_WARNING}"),
            format!("{FORGET_PROMPT}{name}"),
        ],
    };
    let key = match armed.verb {
        Verb::Delete => DELETE_LABEL,
        Verb::Forget => FORGET_LABEL,
    };
    let mut footer = FooterLine::default();
    for saying in sayings {
        footer = FooterLine::default();
        footer.command(key, t.tool_warning);
        footer.describe(saying, t.tool_warning);
        footer.text(SECTION_GAP, Style::default());
        footer.command(ESC_LABEL, t.keybind_key);
        footer.describe(KEEP_HINT, t.tool_dim);
        if footer.fits(width) {
            break;
        }
    }
    footer
}

fn push_fact(spans: &mut Vec<Span<'static>>, text: String, style: Style) {
    spans.push(Span::styled(SEPARATOR, theme::current().tool_dim));
    spans.push(Span::styled(text, style));
}

/// `31.6 of 32 KiB ▕█████████▉▏`, in the warning style once over budget.
fn push_gauge(spans: &mut Vec<Span<'static>>, used: usize, over: bool) {
    let t = theme::current();
    let style = match over {
        true => t.tool_warning,
        false => t.item_desc,
    };
    let kib = used as f64 / KIB as f64;
    push_fact(spans, format!("{kib:.1} of {} KiB ", VIEW / KIB), style);
    let eighths = (used.saturating_mul(GAUGE_EIGHTHS) / VIEW).min(GAUGE_EIGHTHS);
    let (whole, partial) = (eighths / CELL_EIGHTHS, eighths % CELL_EIGHTHS);
    let mut fill = FULL_CELL.to_string().repeat(whole);
    if let Some(index) = partial.checked_sub(1) {
        fill.push(PARTIAL_CELLS[index]);
    }
    let empty = GAUGE_CELLS - whole - usize::from(partial > 0);
    spans.push(Span::styled(GAUGE_OPEN, t.tool_dim));
    spans.push(Span::styled(fill, style));
    spans.push(Span::styled(
        EMPTY_CELL.to_string().repeat(empty),
        t.tool_dim,
    ));
    spans.push(Span::styled(GAUGE_CLOSE, t.tool_dim));
}

fn lines_of(count: usize) -> String {
    match count {
        1 => "1 line".to_owned(),
        count => format!("{count} lines"),
    }
}

fn entry(snapshot: &MemorySnapshot, seq: u64) -> Option<&EntryMeta> {
    snapshot.entries.get(usize::try_from(seq).ok()?)
}

/// Whether the newest entry of `name` is a note nobody forgot.
fn live(snapshot: &MemorySnapshot, name: &str) -> bool {
    snapshot
        .entries
        .iter()
        .rev()
        .find(|meta| meta.name == name)
        .is_some_and(|meta| meta.kind == EntryKind::Note && !meta.forgotten)
}

/// A line's text, one terminal row per line it holds, controls escaped.
fn text_lines(text: &str, style: Style) -> impl Iterator<Item = Line<'static>> {
    text.lines()
        .map(move |line| Line::styled(one_line(line), style))
        .collect::<Vec<_>>()
        .into_iter()
}

/// A note as Markdown: line breaks and tabs stay, every other control
/// character is spelled out rather than sent to the terminal.
fn escape_body(text: &str) -> Cow<'_, str> {
    if !text
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
    {
        return Cow::Borrowed(text);
    }
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\n' | '\t' => escaped.push(character),
            _ if character.is_control() => escaped.extend(character.escape_default()),
            _ => escaped.push(character),
        }
    }
    Cow::Owned(escaped)
}

#[cfg(test)]
pub(super) mod tests {
    use caudra_agent::memory::store::{MemoryState, leaf_kind};
    use caudra_agent::memory::tree::{Leaf, Tree};
    use caudra_storage::memory_journal::StoredNode;
    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use test_case::test_case;

    use super::outline::LEVEL_CELL;
    use super::*;
    use crate::components::tool_display::TREE_LAST;

    pub(crate) const SESSION: &str = "session";
    const OTHER: &str = "other";
    const OWNER: &str = "owner";
    const MODEL: &str = "fast";
    const HEADING: &str = "Note";
    const SUMMARY_PREFIX: &str = "line ";
    /// Room for about seven short lines, so a dozen entries fold.
    const BUDGET: usize = 100;
    pub(crate) const ENTRIES: u64 = 13;
    /// Entries the session's prompt was built with, two fewer than now.
    const BOUND: u64 = 11;
    const LONG_BODY: u64 = 4_096;
    const START_MS: i64 = 1_790_000_000_000;
    const HOUR_MS: i64 = 3_600_000;
    const NARROW: u16 = 80;
    const WIDE: u16 = 120;
    const TALL: u16 = 48;
    /// Rows that leave a docked outline shorter than a fully expanded line.
    const SHORT: u16 = 24;
    /// Expands the oldest line down to its first entry.
    const EXPANDING_PRESSES: usize = 8;
    const QUERY: &str = "note";
    const HOSTILE: &str = "clear\u{1b}[2J screen";
    const ESCAPED: &str = "\\u{1b}[2J";
    const PROMPT_HEAD: &str = "You are Caudra.\n";
    const CURSOR_AND_ROWS_AGREE: &str = "the cursor walks exactly the rows the outline draws";
    const NOT_DOCKED: &str = "the detail docks under the outline only when narrow";
    const CURSOR_HIDDEN: &str = "a resize must keep the cursor's row in the outline";
    const FOOTER_WRAPS: &str = "the footer must fit one row at 80 columns";
    const CONTROL_LEAKED: &str = "a control character reached the terminal";
    const BARS_MISALIGNED: &str =
        "guides and addresses pad to the widest, so the level bars line up";

    /// A memory of long notes, so no leaf is kept word for word and every
    /// line is a summary or waits for one.
    pub(crate) struct Fixture {
        entries: Vec<EntryMeta>,
        nodes: Vec<StoredNode>,
    }

    impl Fixture {
        pub(crate) fn notes(count: u64) -> Self {
            let entries = (0..count)
                .map(|seq| EntryMeta {
                    seq,
                    kind: EntryKind::Note,
                    name: note_name(seq),
                    heading: format!("{HEADING} {seq}"),
                    body_bytes: LONG_BODY,
                    origin: EntryOrigin::Session(SESSION.to_owned()),
                    created_ms: START_MS + HOUR_MS * i64::try_from(seq).unwrap(),
                    forgotten: false,
                })
                .collect();
            Self {
                entries,
                nodes: Vec::new(),
            }
        }

        /// Every node whose entries all exist has its summary.
        pub(crate) fn summarized(mut self) -> Self {
            let entries = self.entries.len() as u64;
            let mut level = 0;
            while let Some(count) = entries.checked_shr(level).filter(|count| *count > 0) {
                for index in 0..count {
                    let part = Part { level, index };
                    let text = format!("{SUMMARY_PREFIX}{part}");
                    self = self.line(&part, &text);
                }
                level += 1;
            }
            self
        }

        pub(crate) fn line(mut self, part: &Part, text: &str) -> Self {
            self.forget_node(part);
            self.nodes.push(StoredNode {
                level: part.level,
                index: part.index,
                text: Some(text.to_owned()),
                model: Some(MODEL.to_owned()),
                created_ms: START_MS,
                lease_owner: None,
                lease_expires_ms: None,
            });
            self
        }

        /// The newest entry waits for its summary.
        pub(crate) fn pending_tail(mut self) -> Self {
            let newest = Part::leaf(self.entries.len() as u64 - 1);
            self.forget_node(&newest);
            self
        }

        pub(crate) fn leased(mut self, part: &Part) -> Self {
            self.forget_node(part);
            self.nodes.push(StoredNode {
                level: part.level,
                index: part.index,
                text: None,
                model: None,
                created_ms: START_MS,
                lease_owner: Some(OWNER.to_owned()),
                lease_expires_ms: Some(i64::MAX),
            });
            self
        }

        fn written_by(mut self, seq: u64, origin: EntryOrigin) -> Self {
            self.entries[usize::try_from(seq).unwrap()].origin = origin;
            self
        }

        fn forget_node(&mut self, part: &Part) {
            self.nodes
                .retain(|node| (node.level, node.index) != (part.level, part.index));
        }

        pub(crate) fn snapshot(self) -> MemorySnapshot {
            let leaves = self
                .entries
                .iter()
                .map(|meta| Leaf {
                    kind: leaf_kind(meta),
                    name: meta.name.clone(),
                    heading: meta.heading.clone(),
                    body: None,
                })
                .collect();
            let stored: Vec<(Part, String)> = self
                .nodes
                .iter()
                .filter_map(|node| {
                    let part = Part {
                        level: node.level,
                        index: node.index,
                    };
                    Some((part, node.text.clone()?))
                })
                .collect();
            let tree = Tree::new(leaves, stored);
            MemorySnapshot::fold(
                MemoryState {
                    entries: self.entries,
                    nodes: self.nodes,
                    tree,
                },
                BUDGET,
            )
        }
    }

    pub(crate) fn note_name(seq: u64) -> String {
        format!("note-{seq}.md")
    }

    pub(crate) fn opened(snapshot: MemorySnapshot) -> MemoryInspector {
        let mut inspector = MemoryInspector::new();
        inspector.open(None, SESSION.to_owned(), true);
        let _ = inspector.fill(Ok(snapshot), None);
        inspector
    }

    fn press(inspector: &mut MemoryInspector, code: KeyCode) -> MemoryAction {
        inspector.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn press_label(inspector: &mut MemoryInspector, label: &str) -> MemoryAction {
        press(inspector, KeyCode::Char(ascii_key(label)))
    }

    fn selected_part(inspector: &MemoryInspector) -> Part {
        inspector.selected().expect("a row is selected").part
    }

    fn terminal(width: u16) -> Terminal<TestBackend> {
        Terminal::new(TestBackend::new(width, TALL)).unwrap()
    }

    fn draw(inspector: &mut MemoryInspector, terminal: &mut Terminal<TestBackend>) {
        terminal
            .draw(|frame| {
                inspector.view(frame, frame.area());
            })
            .unwrap();
    }

    fn screen(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let width = usize::from(buffer.area.width);
        buffer
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The text of each drawn outline line.
    fn outline_text(inspector: &MemoryInspector, terminal: &Terminal<TestBackend>) -> Vec<String> {
        let buffer = terminal.backend().buffer();
        let area = inspector.outline_area;
        let drawn = u16::try_from(inspector.outline_lines.len())
            .unwrap()
            .min(area.height);
        (area.y..area.y + drawn)
            .map(|y| {
                (area.x..area.x + area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    fn address(line: &str) -> Option<&str> {
        line.split_whitespace().find(|token| token.contains('+'))
    }

    fn texts(lines: &[Line<'static>]) -> Vec<String> {
        lines.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn the_cursor_starts_on_the_newest_line_of_the_view() {
        let snapshot = Fixture::notes(ENTRIES).summarized().snapshot();
        let newest = snapshot.view.last().cloned().unwrap();

        let inspector = opened(snapshot);

        assert_eq!(selected_part(&inspector), newest);
    }

    #[test]
    fn every_drawn_row_is_a_row_the_cursor_reaches() {
        let mut inspector = opened(Fixture::notes(ENTRIES).summarized().snapshot());
        for code in [
            KeyCode::Home,
            KeyCode::Right,
            KeyCode::Right,
            KeyCode::Right,
        ] {
            let _ = press(&mut inspector, code);
        }
        let mut terminal = terminal(WIDE);
        draw(&mut inspector, &mut terminal);
        let drawn = outline_text(&inspector, &terminal);

        let _ = press(&mut inspector, KeyCode::Home);
        let reached: Vec<String> = drawn
            .iter()
            .map(|_| {
                let part = selected_part(&inspector).to_string();
                let _ = press(&mut inspector, KeyCode::Down);
                part
            })
            .collect();

        assert_eq!(
            drawn.len(),
            inspector.rows().len(),
            "{CURSOR_AND_ROWS_AGREE}"
        );
        for (line, part) in drawn.iter().zip(&reached) {
            assert_eq!(
                address(line),
                Some(part.as_str()),
                "{CURSOR_AND_ROWS_AGREE}: {drawn:?}"
            );
        }
    }

    #[test]
    fn the_level_bars_line_up_at_every_depth() {
        let mut inspector = opened(Fixture::notes(ENTRIES).summarized().snapshot());
        for code in [
            KeyCode::Home,
            KeyCode::Right,
            KeyCode::Right,
            KeyCode::Right,
        ] {
            let _ = press(&mut inspector, code);
        }
        let mut terminal = terminal(WIDE);

        draw(&mut inspector, &mut terminal);

        let lines = outline_text(&inspector, &terminal);
        let columns: HashSet<Option<usize>> = lines
            .iter()
            .map(|line| line.chars().position(|cell| LEVEL_CELL.starts_with(cell)))
            .collect();
        assert_eq!(columns.len(), 1, "{BARS_MISALIGNED}: {lines:?}");
    }

    #[test]
    fn right_opens_a_line_then_steps_into_its_first_child() {
        let mut inspector = opened(Fixture::notes(ENTRIES).summarized().snapshot());
        let _ = press(&mut inspector, KeyCode::Home);
        let line = selected_part(&inspector);

        let _ = press(&mut inspector, KeyCode::Right);
        assert!(inspector.expanded.contains(&line));
        assert_eq!(selected_part(&inspector), line);

        let _ = press(&mut inspector, KeyCode::Right);
        assert_eq!(selected_part(&inspector), line.children().unwrap()[0]);
    }

    #[test]
    fn left_steps_to_the_parent_then_closes_it() {
        let mut inspector = opened(Fixture::notes(ENTRIES).summarized().snapshot());
        for code in [KeyCode::Home, KeyCode::Right, KeyCode::Right] {
            let _ = press(&mut inspector, code);
        }
        let child = selected_part(&inspector);

        let _ = press(&mut inspector, KeyCode::Left);
        assert_eq!(selected_part(&inspector), child.parent());

        let _ = press(&mut inspector, KeyCode::Left);
        assert!(inspector.expanded.is_empty());
        assert_eq!(selected_part(&inspector), child.parent());
    }

    #[test]
    fn enter_on_a_leaf_opens_its_note() {
        let mut inspector = opened(Fixture::notes(ENTRIES).summarized().snapshot());
        let leaf = selected_part(&inspector);
        assert_eq!(leaf.level, 0);

        assert_eq!(
            press(&mut inspector, KeyCode::Enter),
            MemoryAction::Open(note_name(leaf.index))
        );
    }

    #[test]
    fn a_revealed_hit_opens_exactly_its_zoom_path() {
        let snapshot = Fixture::notes(ENTRIES).summarized().snapshot();
        let seq = snapshot.view[0].start() + 1;
        let path = zoom_path(&snapshot.view, seq);
        let mut inspector = opened(snapshot);
        for code in [KeyCode::End, KeyCode::Up, KeyCode::Up, KeyCode::Right] {
            let _ = press(&mut inspector, code);
        }
        let _ = press_label(&mut inspector, SEARCH_LABEL);
        assert_eq!(
            inspector.handle_paste(QUERY),
            Some(MemoryAction::Search(QUERY.to_owned()))
        );
        let hit = Hit {
            seq,
            name: note_name(seq),
            heading: HEADING.to_owned(),
            line: None,
        };
        let _ = inspector.fill_hits(QUERY, Ok(vec![hit]));

        let _ = press(&mut inspector, KeyCode::Enter);

        let opened: HashSet<Part> = path[..path.len() - 1].iter().cloned().collect();
        assert_eq!(inspector.expanded, opened);
        assert_eq!(selected_part(&inspector), Part::leaf(seq));
        let crumb = detail::breadcrumb(&path);
        assert!(
            crumb.starts_with(&format!("{} zooms", path.len())),
            "{crumb}"
        );
        let detail = texts(&inspector.detail_lines(WIDE, MermaidStyle::Off));
        assert!(detail.contains(&crumb), "{detail:?}");
    }

    #[test]
    fn m_selects_the_head_of_the_merge_queue() {
        let snapshot = Fixture::notes(ENTRIES).summarized().snapshot();
        let head = &snapshot.merges[0];
        assert!(head.ready);
        let expected = snapshot.view[head.at].clone();
        let mut inspector = opened(snapshot);

        let _ = press_label(&mut inspector, MERGE_LABEL);

        assert_eq!(selected_part(&inspector), expected);
    }

    #[test]
    fn session_mode_reads_its_lines_from_the_bound_prompt() {
        let bound = Fixture::notes(BOUND).summarized().snapshot();
        let block = bound.tree.block(&bound.view, VIEW);
        let system = format!("{PROMPT_HEAD}{}", block.render());
        let snapshot = Fixture::notes(ENTRIES)
            .summarized()
            .written_by(ENTRIES - 1, EntryOrigin::Session(OTHER.to_owned()))
            .snapshot();
        let mut inspector = MemoryInspector::new();
        inspector.open(Some(&system), SESSION.to_owned(), true);
        let _ = inspector.fill(Ok(snapshot), Some(&system));

        let _ = press_label(&mut inspector, MODE_LABEL);

        let rows = inspector.rows();
        let lines: Vec<Part> = rows
            .iter()
            .filter(|row| row.kind == RowKind::Node)
            .map(|row| row.part.clone())
            .collect();
        let expected: Vec<Part> = block.lines.iter().map(|line| line.part.clone()).collect();
        assert_eq!(lines, expected);
        let tails: Vec<(Part, RowKind)> = rows
            .iter()
            .filter(|row| matches!(row.kind, RowKind::Tail(_)))
            .map(|row| (row.part.clone(), row.kind))
            .collect();
        assert_eq!(
            tails,
            [
                (Part::leaf(BOUND), RowKind::Tail(TailMark::WrittenHere)),
                (Part::leaf(BOUND + 1), RowKind::Tail(TailMark::Reminder)),
            ]
        );
    }

    #[test]
    fn session_mode_before_the_first_turn_says_nothing_was_prepared() {
        let mut inspector = opened(Fixture::notes(ENTRIES).summarized().snapshot());
        let _ = press_label(&mut inspector, MODE_LABEL);
        let mut terminal = terminal(WIDE);

        draw(&mut inspector, &mut terminal);

        assert!(screen(&terminal).contains(UNPREPARED));
    }

    #[test_case(MermaidStyle::Off, true ; "tree_glyphs_without_mermaid")]
    #[test_case(MermaidStyle::Unicode, false ; "a_drawing_with_mermaid")]
    fn the_drawing_follows_the_mermaid_setting(mermaid: MermaidStyle, glyphs: bool) {
        let mut inspector = opened(Fixture::notes(ENTRIES).summarized().snapshot());

        let detail = texts(&inspector.detail_lines(WIDE, mermaid));

        assert_eq!(
            detail.iter().any(|line| line.contains(TREE_LAST)),
            glyphs,
            "{detail:?}"
        );
        assert!(
            !detail.iter().any(|line| line.contains("flowchart")),
            "{detail:?}"
        );
    }

    #[test_case(NARROW, true ; "docked_when_narrow")]
    #[test_case(WIDE, false ; "beside_when_wide")]
    fn the_detail_docks_below_under_split_min_cols(width: u16, docked: bool) {
        let mut inspector = opened(Fixture::notes(ENTRIES).summarized().snapshot());
        let mut terminal = terminal(width);

        draw(&mut inspector, &mut terminal);

        let (outline, detail) = (inspector.outline_area, inspector.detail_area);
        assert_eq!(detail.y > outline.y, docked, "{NOT_DOCKED}");
        assert_eq!(detail.x == outline.x, docked, "{NOT_DOCKED}");
    }

    #[test]
    fn a_resize_keeps_the_cursor_drawn() {
        let mut inspector = opened(Fixture::notes(ENTRIES).summarized().snapshot());
        let _ = press(&mut inspector, KeyCode::Home);
        for _ in 0..EXPANDING_PRESSES {
            let _ = press(&mut inspector, KeyCode::Right);
        }
        let _ = press(&mut inspector, KeyCode::End);
        draw(&mut inspector, &mut terminal(WIDE));
        let mut short = Terminal::new(TestBackend::new(NARROW, SHORT)).unwrap();

        draw(&mut inspector, &mut short);

        let cursor = selected_part(&inspector).to_string();
        let drawn = outline_text(&inspector, &short);
        assert!(
            drawn
                .iter()
                .any(|line| address(line) == Some(cursor.as_str())),
            "{CURSOR_HIDDEN}: {cursor} not in {drawn:?}"
        );
    }

    #[test]
    fn the_footer_fits_80_columns() {
        let mut inspector = opened(Fixture::notes(ENTRIES).summarized().snapshot());
        let mut terminal = terminal(NARROW);
        draw(&mut inspector, &mut terminal);
        let width = Modal::inner_width(NARROW, WIDTH_PERCENT) - 2 * H_PAD;

        assert!(inspector.footer_line(width).fits(width), "{FOOTER_WRAPS}");
    }

    #[test]
    fn control_characters_never_reach_the_terminal() {
        let newest = Part::leaf(ENTRIES - 1);
        let snapshot = Fixture::notes(ENTRIES)
            .summarized()
            .line(&newest, HOSTILE)
            .snapshot();
        let mut inspector = opened(snapshot);
        let _ = press(&mut inspector, KeyCode::End);
        assert_eq!(selected_part(&inspector), newest);
        inspector.fill_body(newest.index, Ok(Some(HOSTILE.to_owned())));
        let mut terminal = terminal(WIDE);

        draw(&mut inspector, &mut terminal);

        let drawn = screen(&terminal);
        assert!(!drawn.contains('\u{1b}'), "{CONTROL_LEAKED}");
        assert!(drawn.contains(ESCAPED), "{drawn}");
    }

    #[test_case(DELETE_LABEL ; "delete")]
    #[test_case(FORGET_LABEL ; "forget")]
    fn delete_and_forget_take_a_second_press(label: &str) {
        let mut inspector = opened(Fixture::notes(ENTRIES).summarized().snapshot());
        let name = note_name(selected_part(&inspector).index);
        let expected = match label {
            DELETE_LABEL => MemoryAction::Delete(name),
            _ => MemoryAction::Forget(name),
        };

        assert_eq!(press_label(&mut inspector, label), MemoryAction::Consumed);
        let _ = press(&mut inspector, KeyCode::Up);
        let _ = press(&mut inspector, KeyCode::Down);
        assert_eq!(press_label(&mut inspector, label), MemoryAction::Consumed);
        assert_eq!(press_label(&mut inspector, label), expected);
    }

    #[test]
    fn a_line_being_summarized_spins_and_keeps_the_inspector_polling() {
        let newest = Part::leaf(ENTRIES - 1);
        let snapshot = Fixture::notes(ENTRIES)
            .summarized()
            .leased(&newest)
            .snapshot();

        let inspector = opened(snapshot);

        assert!(inspector.polls());
        assert_eq!(
            Overlay::cadence(&inspector),
            Cadence::any([Cadence::SPINNER, Cadence::polling(RELOAD_INTERVAL)])
        );
    }

    #[test_case(NARROW ; "narrow")]
    #[test_case(WIDE ; "wide")]
    fn renders_at_common_widths(width: u16) {
        let snapshot = Fixture::notes(ENTRIES)
            .summarized()
            .pending_tail()
            .snapshot();
        let newest = snapshot.view.last().cloned().unwrap();
        let mut inspector = opened(snapshot);
        let mut terminal = terminal(width);

        draw(&mut inspector, &mut terminal);

        let drawn = screen(&terminal);
        for expected in [TITLE.trim(), LIVE_LABEL, ESC_LABEL, &newest.to_string()] {
            assert!(
                drawn.contains(expected),
                "{expected} missing from:\n{drawn}"
            );
        }
    }
}
