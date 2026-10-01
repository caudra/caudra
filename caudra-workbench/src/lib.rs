//! A file explorer, tabbed editor, source control view and search view that
//! takes over the terminal beside Caudra's transcript.
//!
//! The crate owns its own rendering so `caudra-ui` stays a thin host: it
//! forwards key and mouse events, hands over a [`WorkbenchStyles`] whenever the
//! theme moves, and turns the returned [`WorkbenchAction`] into its own actions.
//! Nothing here reaches back into the agent, the session or the clipboard.

mod action;
mod chrome;
mod editor;
mod fs;
pub mod keys;
mod menu;
mod pointer;
mod quick_open;
mod scm;
pub mod scroll;
mod search;
mod style;
pub mod transfer;
mod view;

pub use action::WorkbenchAction;
pub use editor::rendered::{PaintMarkdown, PaintedMarkdown};
pub use editor::{DocumentKey, TabLabel, buffer, history, render, text_field, words};
pub use fs::backend::{
    BackendDriver, BackendError, BackendEvent, BackendRevision, ListResult, LoadedFile,
    LocalFilesystem, MutationGate, RequestId, ResourceEntry, SearchMatch, SearchResult,
    WatchHandle, WatchResult, WatchUpdate, WorkbenchBackend, WorkbenchFilesystem, WorkbenchPath,
    WorkspaceFilesystem,
};
pub use fs::read::LocalSourceError;
pub use pointer::Clicks;
pub use style::WorkbenchStyles;
use unicode_width::UnicodeWidthStr;

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::time::Instant;

use caudra_workspace::{
    ScmDiffTarget, ScmMutation, ScmRevision, ScmSide, WorkspaceError, WorkspacePath,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use serde::{Deserialize, Serialize};

use editor::Editor;
use editor::Tab;
use editor::buffer::Cursor;
use editor::text_field::{EditCommand, FieldKind, TextCommand, TextField, TextKey, decode};
use fs::ops;
use fs::tree::Tree;
use fs::watch::Watch;
use menu::{Action as MenuAction, Menu, Target};
use quick_open::QuickOpen;
use scm::backend::{
    CommitFilesResult, DiffResult as RemoteDiffResult, Driver as ScmDriver,
    Error as ScmBackendError, Event as ScmEvent,
};
use scm::repo::Commit;
use scm::{MIN_SECTION_ROWS, Scm, Section};
use scroll::{Scrollbar, ScrollbarMouse};
use search::Search;
use view::{Control, TabHit, TabPart, Toggle};

const DEFAULT_SIDEBAR_WIDTH: u16 = 30;
const MIN_SIDEBAR_WIDTH: u16 = 16;
const MAX_SIDEBAR_WIDTH: u16 = 80;
const MIN_EDITOR_WIDTH: u16 = 24;
const SEPARATOR_WIDTH: u16 = 1;
const STATUS_HEIGHT: u16 = 1;
const SCROLL_LINES: isize = 3;
/// How far one notch of a sideways wheel pans the text, in display columns.
/// Wider than a vertical notch because a column is narrower than a row.
const SCROLL_COLUMNS: isize = 4;
/// How far one tick of a drag that has run off the buffer scrolls it.
const EDGE_SCROLL_LINES: isize = 1;
const SIDEBAR_STEP: i16 = 2;
/// Every source control section keeps its title row, folded or not, so the
/// pane never loses the handle that unfolds it again.
const SECTION_HEADER_ROWS: u16 = 1;
/// How far one press of the section resize keys moves a border.
const SECTION_STEP: i16 = 1;
const STAGED: &str = "index";
const WORKING: &str = "worktree";
const SAVE_LABEL: &str = "Save";
const DISCARD_LABEL: &str = "Don't Save";
const REVERT_LABEL: &str = "Discard";
const DELETE_LABEL: &str = "Delete";
const CANCEL_LABEL: &str = "Cancel";
const RENAME_PROMPT: &str = "Rename: ";
const NEW_FILE_PROMPT: &str = "New file: ";
const NEW_FOLDER_PROMPT: &str = "New folder: ";
const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";
const WATCH_WARNING: &str = "Remote live updates are unavailable; manual refresh still works";
const LISTING_INCOMPLETE: &str =
    "Remote file listing is incomplete; keeping previously loaded entries";
const MAX_REMOTE_READS: usize = 8;
/// Where a workspace session files the tabs it synthesises from the repository,
/// which name no path the workspace itself would serve.
const SCM_SYNTHETIC_ROOT: &str = ".caudra-scm";
/// Where the host's documents are filed. Nothing is read or written under it;
/// the path only gives each document a name of its own.
const DOCUMENT_ROOT: &str = ".caudra-document";
/// Every document is Markdown, which is what earns it a rendered view.
const DOCUMENT_EXTENSION: &str = "md";
const RENDERED_READ_ONLY: &str = "The rendered view is read-only";
const NO_RENDERED_VIEW: &str = "Only a Markdown file has a rendered view";
/// Chords that land the caret somewhere in the text. The rendered view has no
/// caret to show where, so they go back to the source first.
const SOURCE_CHORDS: [keys::Bind; 4] = [
    keys::FIND,
    keys::FIND_NEXT,
    keys::FIND_PREV,
    keys::GOTO_LINE,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SidebarView {
    #[default]
    Explorer,
    SourceControl,
    Search,
    Transfer,
}

impl SidebarView {
    /// What the header switcher paints. All three together have to fit
    /// [`MIN_SIDEBAR_WIDTH`], so these are short rather than descriptive.
    const fn title(self) -> &'static str {
        match self {
            Self::Explorer => "FILES",
            Self::SourceControl => "GIT",
            Self::Search => "FIND",
            Self::Transfer => "TRANSFER",
        }
    }

    const ALL: [Self; 3] = [Self::Explorer, Self::SourceControl, Self::Search];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    #[default]
    Sidebar,
    Editor,
}

/// What the workbench looked like, in the only terms that survive a restart.
/// The host stores it per project; nothing here reaches the filesystem, so a
/// layout naming files that have since gone is still a valid layout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Layout {
    /// Open tabs in tab-bar order. Diffs are left out: they are synthesised
    /// from the repository, so restoring one would open a file instead.
    pub tabs: Vec<PathBuf>,
    pub active: usize,
    pub sidebar: SidebarView,
    pub sidebar_width: u16,
    pub sidebar_collapsed: bool,
    pub show_hidden: bool,
    /// Whether the editor breaks long lines onto more rows instead of panning.
    pub wrap: bool,
    pub scm: ScmLayout,
}

/// How the source control pane was arranged. The sections are a list rather
/// than three named fields so that reading one written by a build that knew a
/// different number of them keeps whatever it does name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScmLayout {
    /// Whether the change sections list paths whole instead of nesting them.
    pub flat: bool,
    pub sections: Vec<SectionLayout>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SectionLayout {
    pub height: u16,
    pub collapsed: bool,
}

/// Written by hand because a zero width is not a narrow sidebar, it is an
/// unset one, and the host restores a default layout when nothing was stored.
impl Default for Layout {
    fn default() -> Self {
        Self {
            tabs: Vec::new(),
            active: 0,
            sidebar: SidebarView::default(),
            sidebar_width: DEFAULT_SIDEBAR_WIDTH,
            sidebar_collapsed: false,
            show_hidden: false,
            wrap: false,
            scm: ScmLayout::default(),
        }
    }
}

/// Hand-written for the same reason [`Layout`]'s is: a derived zero height is
/// an unset section rather than a flat one.
impl Default for ScmLayout {
    fn default() -> Self {
        Self {
            flat: false,
            sections: vec![SectionLayout::default(); Section::COUNT],
        }
    }
}

impl Default for SectionLayout {
    fn default() -> Self {
        Self {
            height: scm::DEFAULT_SECTION_ROWS,
            collapsed: false,
        }
    }
}

/// Where the panes landed in the last frame. Paging and mouse routing need the
/// same geometry the renderer used, and recomputing it from the event would
/// drift the moment the layout gains a rule.
#[derive(Debug, Clone, Copy, Default)]
struct PaneRects {
    sidebar: Option<Rect>,
    separator: Option<Rect>,
    editor: Rect,
    tabs: Rect,
    /// The rows the quick open palette listed, empty when it is closed.
    palette: Rect,
    /// The unsaved-changes dialog's button row, empty when it is closed.
    confirm: Rect,
    /// The context menu's items, empty when it is closed.
    menu: Rect,
    /// The buffer's own rows and columns, with the tab bar, the gutter and any
    /// open find bar already taken out.
    text: Rect,
    status: Rect,
    /// The sidebar's one-row title, which doubles as the view switcher.
    header: Rect,
    /// Whichever list the sidebar is showing, with that view's own prompts
    /// already taken out. Search puts three rows above its results, so an
    /// offset measured from the sidebar itself would land three rows short.
    rows: Rect,
    /// The search view's case, word and regex buttons, empty in every other
    /// view.
    toggles: Rect,
    /// Where the source control sections landed, in stacking order. Empty in
    /// every other view.
    sections: [SectionRect; Section::COUNT],
}

/// One band of the source control pane. The header is always drawn; the body
/// is empty when the section is folded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SectionRect {
    header: Rect,
    body: Rect,
}

/// What a modal question is about, which is what decides the answers it offers
/// and what they do once one is picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ask {
    /// A tab was asked to close while it still had unsaved edits.
    Close,
    /// A folder or a whole section was asked to throw its working tree edits
    /// away, which no arming on the row itself is careful enough to cover.
    Revert,
    /// The explorer was asked to remove a path. Nothing here can put it back,
    /// so the count of what goes with it is part of the question. The count is
    /// taken once, when the question goes up, rather than on every frame that
    /// paints it.
    Delete(usize),
}

impl Ask {
    const CLOSE_ANSWERS: [Choice; 3] = [Choice::Save, Choice::Discard, Choice::Cancel];
    const DISCARD_ANSWERS: [Choice; 2] = [Choice::Discard, Choice::Cancel];

    /// The answers on offer, in the order they are painted and measured.
    fn answers(self) -> &'static [Choice] {
        match self {
            Ask::Close => &Self::CLOSE_ANSWERS,
            Ask::Revert | Ask::Delete(_) => &Self::DISCARD_ANSWERS,
        }
    }
}

/// One answer a dialog offers. What each one means is read from the [`Ask`] it
/// is offered against, so a discard can say what it is discarding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    Save,
    Discard,
    Cancel,
}

impl Choice {
    fn label(self, ask: Ask) -> &'static str {
        match (self, ask) {
            (Choice::Save, _) => SAVE_LABEL,
            (Choice::Discard, Ask::Close) => DISCARD_LABEL,
            (Choice::Discard, Ask::Revert) => REVERT_LABEL,
            (Choice::Discard, Ask::Delete(_)) => DELETE_LABEL,
            (Choice::Cancel, _) => CANCEL_LABEL,
        }
    }

    /// The letter that picks this answer outright, which is the first one of
    /// its label so nothing has to be memorised.
    fn accelerator(self) -> char {
        match self {
            Choice::Save => 's',
            Choice::Discard => 'd',
            Choice::Cancel => 'c',
        }
    }
}

/// A question the workbench is holding everything else up for, and the answer
/// under the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Confirm {
    ask: Ask,
    choice: Choice,
}

impl Confirm {
    /// The neighbouring answer, stopping at both ends rather than wrapping so
    /// a held arrow key cannot walk off the last one back onto the first.
    fn step(self, delta: isize) -> Self {
        let answers = self.ask.answers();
        let at = answers.iter().position(|choice| *choice == self.choice);
        let reached = at.unwrap_or_default().saturating_add_signed(delta);
        Self {
            choice: answers[reached.min(answers.len() - 1)],
            ..self
        }
    }
}

/// A name the workbench is waiting for, and what it will do with it. It is
/// typed into the status row, where a question about a path can be read beside
/// the tree that answers it.
#[derive(Debug, Clone)]
struct Input {
    kind: InputKind,
    /// What the answer is about: the path being renamed, or the folder a new
    /// path lands in.
    at: WorkbenchPath,
    value: TextField,
}

impl Input {
    fn new(kind: InputKind, at: WorkbenchPath, value: &str) -> Self {
        Self {
            kind,
            at,
            value: TextField::with_text(FieldKind::Line, value),
        }
    }
}

/// The one-line field that keys, pastes and the caret go to while it is up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusedField {
    Name,
    Palette,
    Goto,
    Find,
    Search,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputKind {
    Rename,
    NewFile,
    NewFolder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenPurpose {
    Open,
    Preview,
    Reload,
    Discard,
}

#[derive(Debug)]
struct PendingOpen {
    path: WorkbenchPath,
    purpose: OpenPurpose,
    invalidated: bool,
}

impl InputKind {
    fn label(self) -> &'static str {
        match self {
            Self::Rename => RENAME_PROMPT,
            Self::NewFile => NEW_FILE_PROMPT,
            Self::NewFolder => NEW_FOLDER_PROMPT,
        }
    }
}

/// What the held left button is doing, which the pointer cannot tell from
/// position alone once a drag wanders out of the pane it started in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Drag {
    #[default]
    None,
    Separator,
    Text,
    /// Resizing the border above this section's header, which doubles as the
    /// section's own fold handle when the pointer never moves.
    Section(usize),
    /// A press the context menu took. The release finishes nothing, so it must
    /// not be read as the end of a selection.
    Menu,
}

/// Which bar a grab is holding. The sidebar's three views share one, because
/// only one of them is ever drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Bar {
    Sidebar,
    Palette,
    Text,
    Section(Section),
}

#[derive(Default)]
struct Bars {
    sidebar: Scrollbar,
    palette: Scrollbar,
    text: Scrollbar,
    sections: [Scrollbar; Section::COUNT],
}

impl Bars {
    fn slot(&mut self, bar: Bar) -> &mut Scrollbar {
        match bar {
            Bar::Sidebar => &mut self.sidebar,
            Bar::Palette => &mut self.palette,
            Bar::Text => &mut self.text,
            Bar::Section(section) => &mut self.sections[section.index()],
        }
    }
}

/// `None` when the bar did not want the event, `Some(None)` when it took it
/// without moving, `Some(Some(offset))` when it scrolled.
fn taken(outcome: ScrollbarMouse) -> Option<Option<u32>> {
    match outcome {
        ScrollbarMouse::Ignored => None,
        ScrollbarMouse::Consumed => Some(None),
        ScrollbarMouse::ScrollTo(offset) => Some(Some(offset)),
    }
}

pub struct Workbench {
    open: bool,
    root: PathBuf,
    focus: Focus,
    sidebar: SidebarView,
    sidebar_width: u16,
    sidebar_collapsed: bool,
    show_hidden: bool,
    /// Whether the editor breaks a line too long for the pane onto further rows
    /// rather than leaving it off to the right.
    wrap: bool,
    styles: WorkbenchStyles,
    /// Whether a pane whose content overruns it gives up a column to say so.
    /// The host owns the setting, so this mirrors `ui.scrollbar` rather than
    /// reading it.
    scrollbars: bool,
    /// Bumped on every palette change so open tabs know to rehighlight.
    theme_generation: u64,
    /// The host's Markdown painter, lent for the rendered view. Without one no
    /// tab is offered that view and its chord stays the host's.
    markdown: Option<PaintMarkdown>,
    panes: PaneRects,
    /// One per bar drawn, so a grab knows which pane it is holding and a drag
    /// survives the pointer wandering out of that pane's column.
    bars: Bars,
    tree: Tree,
    editor: Editor,
    palette: QuickOpen,
    scm: Scm,
    search: Search,
    remote_backend: Option<BackendDriver>,
    remote_scm: Option<ScmDriver>,
    remote_entries: HashMap<WorkbenchPath, ResourceEntry>,
    remote_pending: HashSet<RequestId>,
    pending_open: HashMap<RequestId, PendingOpen>,
    pending_save: HashMap<RequestId, WorkbenchPath>,
    pending_create: HashMap<RequestId, InputKind>,
    pending_count: HashMap<RequestId, WorkbenchPath>,
    pending_lines: HashMap<WorkbenchPath, RangeInclusive<usize>>,
    /// The commit whose detail tab is waiting on the asynchronous walk that
    /// lists what it changed. Only a workspace session ever sets it; a local
    /// repository answers the same question in the same breath.
    pending_commit_detail: Option<String>,
    delete_target: Option<ResourceEntry>,
    /// Started when the workbench opens and dropped when it closes, so a tree
    /// nobody is looking at costs no kernel handles.
    watch: Option<Watch>,
    /// Files that changed on disk while the workbench was open, which in
    /// practice is what Caudra wrote underneath it.
    touched: HashSet<PathBuf>,
    /// What the held left button is doing, so a drag that wanders out of the
    /// pane it started in keeps doing it.
    drag: Drag,
    /// Where the last drag reached, which is what tells a text drag it has run
    /// off the top or the bottom of the buffer and should scroll.
    drag_at: (u16, u16),
    /// Where the button went down, so releasing without having moved can be
    /// told from finishing a drag. That is what lets one press on a section
    /// header both fold it and resize it.
    drag_from: (u16, u16),
    /// Consecutive presses on one cell, which is how a double click is told
    /// from two single ones.
    clicks: Clicks,
    /// Where the pointer is resting, so every pane can paint what a click
    /// would hit. `None` until the pointer first moves.
    hover: Option<(u16, u16)>,
    /// Cut and copy also leave through [`WorkbenchAction::Copy`], but the host
    /// cannot read the system clipboard back, so paste comes from here.
    clipboard: String,
    goto: Option<TextField>,
    /// The question standing over everything else, if there is one. What it is
    /// about is not kept: whatever raised it selected its target first, and the
    /// dialog is modal, so nothing can move the cursor underneath it.
    confirm: Option<Confirm>,
    /// The context menu, which does keep what it was opened on. Any other
    /// input closes it, so what it names cannot move underneath it.
    menu: Option<Menu>,
    /// The name the workbench is waiting to be given, if it asked for one.
    input: Option<Input>,
    /// What a batch close still has to get through, last one first so the next
    /// is a pop. Paths rather than indices, because every close renumbers the
    /// tabs behind it.
    closing: Vec<WorkbenchPath>,
    flash: Option<String>,
    transfer: transfer::TransferState,
    switcher: Vec<(Rect, SidebarView)>,
    /// Where the status row's hints landed in the last frame, and the key a
    /// press on each one stands in for.
    hint_hits: Vec<(Rect, keys::Bind)>,
}

impl Workbench {
    pub fn new(styles: WorkbenchStyles) -> Self {
        Self {
            open: false,
            root: PathBuf::new(),
            focus: Focus::default(),
            sidebar: SidebarView::default(),
            sidebar_width: DEFAULT_SIDEBAR_WIDTH,
            sidebar_collapsed: false,
            show_hidden: false,
            wrap: false,
            styles,
            scrollbars: true,
            theme_generation: 0,
            markdown: None,
            panes: PaneRects::default(),
            bars: Bars::default(),
            tree: Tree::default(),
            editor: Editor::default(),
            palette: QuickOpen::default(),
            scm: Scm::default(),
            search: Search::default(),
            remote_backend: None,
            remote_scm: None,
            remote_entries: HashMap::new(),
            remote_pending: HashSet::new(),
            pending_open: HashMap::new(),
            pending_save: HashMap::new(),
            pending_create: HashMap::new(),
            pending_count: HashMap::new(),
            pending_lines: HashMap::new(),
            pending_commit_detail: None,
            delete_target: None,
            watch: None,
            touched: HashSet::new(),
            drag: Drag::None,
            drag_at: (0, 0),
            drag_from: (0, 0),
            clicks: Clicks::default(),
            hover: None,
            clipboard: String::new(),
            goto: None,
            confirm: None,
            menu: None,
            input: None,
            closing: Vec::new(),
            flash: None,
            transfer: transfer::TransferState::default(),
            switcher: Vec::new(),
            hint_hits: Vec::new(),
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn open_local_source<Identity: ?Sized>(
        styles: WorkbenchStyles,
        path: &Path,
        lines: Option<RangeInclusive<usize>>,
        expected: &Identity,
        verify: impl FnOnce(&Path, &[u8], &Identity) -> Result<(), String>,
    ) -> Result<Self, LocalSourceError> {
        let root = path
            .parent()
            .ok_or_else(|| LocalSourceError::InvalidPath(path.to_path_buf()))?;
        let (loaded, source) =
            fs::read::load_local_source(path, |bytes| verify(path, bytes, expected))?;
        let mut workbench = Self::new(styles);
        workbench.editor.push(Tab::from_local_source(
            path,
            loaded,
            source,
            workbench.theme_generation,
        ));
        workbench.open_at(root, path, lines);
        Ok(workbench)
    }

    pub fn close(&mut self) {
        if self.transfer.connection_active() {
            return;
        }
        if self.sidebar == SidebarView::Transfer {
            self.transfer.close();
            self.sidebar = SidebarView::Explorer;
        }
        self.open = false;
        self.drag = Drag::None;
        self.cancel_pending_opens();
        for tab in self.editor.tabs_mut() {
            tab.remote_reload = false;
        }
        self.watch = None;
        // A clean document says nothing the host does not already hold, and
        // left open it would go stale behind the next change to the host's copy.
        self.editor
            .close_where(&|tab| tab.document.is_some() && !tab.is_dirty());
        if let Some(backend) = &mut self.remote_backend {
            backend.close_watch();
            if let Some(request) = backend.cancel_listing() {
                self.remote_pending.remove(&request);
            }
        }
        if !self.remote_pending.is_empty()
            || self.remote_scm.as_ref().is_some_and(ScmDriver::is_busy)
        {
            return;
        }
        if let Some(backend) = &mut self.remote_backend {
            backend.suspend();
        }
        if let Some(scm) = &mut self.remote_scm {
            scm.suspend();
        }
        self.remote_pending.clear();
        self.pending_open.clear();
        self.pending_save.clear();
        self.pending_create.clear();
        self.pending_count.clear();
        self.pending_lines.clear();
        self.delete_target = None;
    }

    pub fn bind_workspace(
        &mut self,
        session: caudra_workspace::WorkspaceSession,
    ) -> Result<(), BackendError> {
        self.bind_workspace_with_gate(session, MutationGate::allow())
    }

    pub fn bind_workspace_with_gate(
        &mut self,
        session: caudra_workspace::WorkspaceSession,
        gate: MutationGate,
    ) -> Result<(), BackendError> {
        let reload = self.open;
        let backend = WorkbenchBackend::workspace_with_gate(session.clone(), gate.clone())?;
        let mut remote_scm = ScmDriver::new_with_gate(session, gate).ok();
        let root = WorkbenchPath::Remote(caudra_workspace::WorkspacePath::root());
        if let Some(driver) = &mut self.remote_backend {
            driver.rebind(backend, root.clone());
        } else {
            self.remote_backend = Some(BackendDriver::new(backend, root.clone()));
        }
        self.watch = None;
        self.root.clear();
        self.tree = Tree::default();
        self.editor = Editor::default();
        self.palette = QuickOpen::default();
        self.search = Search::default();
        self.scm = Scm::default();
        self.scm.open_workspace();
        if let Some(driver) = &mut remote_scm {
            driver.refresh();
        } else {
            self.scm
                .fail_workspace_refresh("Remote source control is unavailable");
        }
        self.remote_scm = remote_scm;
        self.touched.clear();
        self.remote_entries.clear();
        self.remote_pending.clear();
        self.pending_open.clear();
        self.pending_save.clear();
        self.pending_create.clear();
        self.pending_count.clear();
        self.pending_lines.clear();
        self.delete_target = None;
        self.tree = Tree::remote(root.clone(), self.show_hidden);
        if reload && let Some(driver) = &mut self.remote_backend {
            driver.open_watch();
            let request = driver.list(root, true);
            self.remote_pending.insert(request);
        }
        Ok(())
    }

    pub fn toggle_workspace(
        &mut self,
        session: caudra_workspace::WorkspaceSession,
    ) -> Result<(), BackendError> {
        if self.open {
            self.close();
            return Ok(());
        }
        if self.remote_backend.is_none() {
            self.bind_workspace(session)?;
        }
        self.open = true;
        if let Some(driver) = &mut self.remote_backend {
            driver.open_watch();
            let request = driver.list(driver.root().clone(), true);
            self.remote_pending.insert(request);
        }
        Ok(())
    }

    pub fn bind_local(&mut self) {
        if let Some(mut backend) = self.remote_backend.take() {
            backend.suspend();
        }
        if let Some(mut scm) = self.remote_scm.take() {
            scm.suspend();
        }
        self.remote_entries.clear();
        self.remote_pending.clear();
        self.pending_open.clear();
        self.pending_save.clear();
        self.pending_create.clear();
        self.pending_count.clear();
        self.pending_lines.clear();
        self.delete_target = None;
    }

    pub fn open(&mut self, root: &Path) {
        self.bind_local();
        let started = Instant::now();
        let mut phase_start = started;
        let mut lap = || {
            let elapsed = phase_start.elapsed().as_millis() as u64;
            phase_start = Instant::now();
            elapsed
        };
        // A watch only reports what happened while it was running, so reopening
        // the same root rereads the panes rather than trusting the last visit.
        let reused = self.root == root;
        if reused {
            self.tree.reload();
        } else {
            self.root = root.to_path_buf();
            self.tree = Tree::new(root, self.show_hidden);
            self.touched.clear();
        }
        let tree_ms = lap();
        if reused {
            self.scm.refresh();
        } else {
            self.scm.open(root);
        }
        let scm_ms = lap();
        // The watch was down while the workbench was closed, so whatever was
        // written in the meantime is only on disk.
        for tab in self.editor.tabs_mut() {
            let _ = tab.refresh_if_changed();
        }
        let tabs_ms = lap();
        self.watch = Watch::start(&self.root);
        let watch_ms = lap();
        self.apply_marks();
        self.open = true;
        tracing::info!(
            reused,
            tree_ms,
            scm_ms,
            tabs_ms,
            watch_ms,
            marks_ms = lap(),
            total_ms = started.elapsed().as_millis() as u64,
            "workbench opened"
        );
    }

    pub fn toggle(&mut self, root: &Path) {
        if self.open {
            self.close();
        } else {
            self.open(root);
        }
    }

    /// Opens `path` in the editor, selecting `lines` when given. This is what a
    /// click on an `@path:L12-L20` mention lands on, so it opens the workbench
    /// if it was closed rather than requiring two gestures.
    pub fn open_at(&mut self, root: &Path, path: &Path, lines: Option<RangeInclusive<usize>>) {
        if self.remote_backend.is_some() {
            match caudra_workspace::WorkspacePath::new(path.to_string_lossy().into_owned()) {
                Ok(path) => self.open_remote_at(path, lines),
                Err(error) => self.flash = Some(error.to_string()),
            }
            return;
        }
        if !self.open || self.root != root {
            self.open(root);
        }
        // A mention names a path relative to the project, and the tree only
        // reveals what it can strip its own root from.
        self.open_path(&self.root.join(path));
        self.show_explorer();
        let Some(tab) = self.editor.active_mut() else {
            return;
        };
        match lines {
            Some(lines) => {
                tab.buffer
                    .set_cursor(Cursor::new(lines.start().saturating_sub(1), 0), false);
                tab.buffer
                    .set_cursor(Cursor::new(lines.end().saturating_sub(1), 0), true);
            }
            None => tab.buffer.goto_line(1),
        }
        self.follow_cursor();
    }

    pub fn open_remote_at(
        &mut self,
        path: caudra_workspace::WorkspacePath,
        lines: Option<RangeInclusive<usize>>,
    ) {
        if self.remote_backend.is_none() {
            self.flash = Some(BackendError::WrongBackend.to_string());
            return;
        }
        self.open = true;
        let path = WorkbenchPath::Remote(path);
        self.open_workbench_path(&path, OpenPurpose::Open);
        if let Some(lines) = lines {
            self.pending_lines.insert(path.clone(), lines);
        }
        self.show_explorer();
    }

    /// Opens a file the host keeps for itself, such as the plan, under a name
    /// that says what it is. Unlike a mention it leaves the cursor where the
    /// reader last had it and the sidebar on whatever it was showing, because
    /// coming back to the plan is picking up where the reading stopped.
    ///
    /// A workbench on a remote workspace refuses rather than flashing, because
    /// it stays closed and a closed workbench says nothing until it next opens.
    pub fn open_labelled(
        &mut self,
        root: &Path,
        path: &Path,
        label: TabLabel,
    ) -> Result<(), BackendError> {
        if self.remote_backend.is_some() {
            return Err(BackendError::WrongBackend);
        }
        if !self.open || self.root != root {
            self.open(root);
        }
        self.open_path(path);
        self.editor
            .label(&WorkbenchPath::Local(path.to_path_buf()), label);
        Ok(())
    }

    /// Rereads the tabs on files something wrote where the watch cannot see,
    /// such as the plan the agent keeps outside the project. A clean tab takes
    /// the new text and a dirty one raises its conflict, as a watched write
    /// would.
    pub fn reload_paths<'a>(&mut self, paths: impl IntoIterator<Item = &'a Path>) {
        for path in paths {
            self.reload_tab(path);
        }
    }

    /// Whether the tab on `path` holds edits that are not on disk yet.
    pub fn has_unsaved(&self, path: &Path) -> bool {
        let identity = WorkbenchPath::Local(path.to_path_buf());
        self.editor
            .tabs()
            .iter()
            .any(|tab| tab.path == identity && tab.is_dirty())
    }

    /// Opens text the host keeps for itself, such as a prompt draft, in a tab
    /// of its own, or raises the one already open. Edits there that were never
    /// handed back are the reader's and stay; a clean tab takes `text`, since
    /// the host's copy may have moved on.
    ///
    /// Saving the tab hands the text back as [`WorkbenchAction::SaveDocument`].
    /// The host opens the workbench itself, because only it knows whether
    /// that means a local root or a workspace session.
    pub fn open_document(&mut self, key: DocumentKey, label: TabLabel, text: &str) {
        self.drag = Drag::None;
        match self.editor.document(&key) {
            Some(index) => {
                self.editor.select(index);
                let tab = &mut self.editor.tabs_mut()[index];
                if !tab.is_dirty() {
                    tab.replace_text(text);
                }
                tab.label = Some(label);
            }
            None => {
                let name = format!("{}.{DOCUMENT_EXTENSION}", opaque_path_component(&key.0));
                let path = Path::new(DOCUMENT_ROOT).join(name);
                let tab = Tab::document(&path, key, label, text, self.theme_generation);
                self.editor.push(tab);
            }
        }
        self.focus = Focus::Editor;
        self.follow_cursor();
    }

    /// The host kept what a [`WorkbenchAction::SaveDocument`] handed it, so
    /// the document's tab has nothing unsaved left.
    pub fn document_saved(&mut self, key: &DocumentKey) {
        if let Some(index) = self.editor.document(key) {
            self.editor.tabs_mut()[index].mark_saved();
        }
    }

    pub fn has_document(&self, key: &DocumentKey) -> bool {
        self.editor.document(key).is_some()
    }

    /// Whether the document's tab holds edits the host has not kept yet.
    pub fn has_unsaved_document(&self, key: &DocumentKey) -> bool {
        self.editor
            .document(key)
            .is_some_and(|index| self.editor.tabs()[index].is_dirty())
    }

    /// The host's copy moved on without the reader, the way a file does when
    /// the agent writes it. A clean tab takes `text` in place, and one with
    /// unsaved edits keeps them and flies the conflict instead. Reports
    /// whether the tab took the text.
    pub fn replace_document(&mut self, key: &DocumentKey, text: &str) -> bool {
        let Some(index) = self.editor.document(key) else {
            return false;
        };
        let tab = &mut self.editor.tabs_mut()[index];
        if tab.is_dirty() {
            tab.conflict = true;
            return false;
        }
        tab.replace_text(text);
        true
    }

    /// The host's answer to [`WorkbenchAction::RevertDocument`]: the tab
    /// throws its edits away and takes `text`.
    pub fn revert_document(&mut self, key: &DocumentKey, text: &str) {
        if let Some(index) = self.editor.document(key) {
            self.editor.tabs_mut()[index].replace_text(text);
            self.follow_cursor();
        }
    }

    pub fn set_styles(&mut self, styles: WorkbenchStyles) {
        self.styles = styles;
        self.theme_generation += 1;
        self.editor.set_theme_generation(self.theme_generation);
        self.transfer.set_theme_generation(self.theme_generation);
    }

    pub fn set_scrollbars(&mut self, scrollbars: bool) {
        self.scrollbars = scrollbars;
    }

    /// Lends the workbench the host's Markdown painter, which is what offers a
    /// Markdown tab its rendered view.
    pub fn set_markdown_painter(&mut self, paint: PaintMarkdown) {
        self.markdown = Some(paint);
    }

    /// Whether a background worker owes an answer, so the host knows to look
    /// again rather than sleeping until the next key.
    pub fn is_busy(&self) -> bool {
        self.transfer.connection_active() || self.backend_busy()
    }

    pub fn backend_busy(&self) -> bool {
        !self.remote_pending.is_empty()
            || !self.pending_save.is_empty()
            || !self.pending_open.is_empty()
            || (self.open && self.editor.tabs().iter().any(|tab| tab.remote_reload))
            || self
                .remote_backend
                .as_ref()
                .is_some_and(BackendDriver::is_listing)
            || self.remote_scm.as_ref().is_some_and(ScmDriver::is_busy)
            || self.search.is_running()
            || self.edge_scroll_delta() != 0
    }

    pub fn blocks_workspace_change(&self) -> bool {
        self.editor.tabs().iter().any(Tab::is_dirty) || self.is_busy()
    }

    pub fn blocks_transfer_start(&self) -> bool {
        self.editor.tabs().iter().any(Tab::is_dirty)
            || self.backend_busy()
            || self.transfer.lease_active()
    }

    pub fn refresh_after_transfer(&mut self) {
        if self.remote_backend.is_some() {
            self.invalidate_remote(None);
            self.refresh_remote_tree();
            self.refresh_remote_scm();
            self.reload_remote_targets();
        } else {
            self.tree.reload();
            self.scm.refresh();
        }
        for tab in self.editor.tabs_mut() {
            if tab.path.local().is_some()
                && tab.is_file()
                && tab.is_editable()
                && tab.reload_from_disk().is_err()
            {
                tab.conflict = true;
            }
        }
        self.palette.invalidate();
        self.apply_marks();
    }

    /// Drains whatever the background workers have produced. Reports whether
    /// the screen changed, plus anything worth saying in the status bar.
    pub fn tick(&mut self) -> (bool, Option<String>) {
        let remote = self.drain_remote_backend();
        let remote_scm = self.drain_remote_scm();
        let watched = self.absorb_changes();
        let searched = self.search.tick();
        let scrolled = self.edge_scroll();
        (
            remote || remote_scm || watched || searched || scrolled,
            self.flash.take(),
        )
    }

    fn drain_remote_scm(&mut self) -> bool {
        let Some(driver) = &mut self.remote_scm else {
            return false;
        };
        let events = driver.drain();
        let changed = !events.is_empty();
        for event in events {
            match event {
                ScmEvent::Refreshed { result, .. } => match result {
                    Ok(snapshot) => {
                        self.scm.apply_workspace_snapshot(snapshot);
                        self.apply_marks();
                    }
                    Err(ScmBackendError::Workspace(WorkspaceError::NotRepository)) => {
                        self.scm.clear_workspace_repository();
                        self.apply_marks();
                    }
                    Err(error) => self.scm.fail_workspace_refresh(error),
                },
                ScmEvent::Diffed { result, .. } => match result {
                    Ok(result) => self.open_remote_diff(result),
                    Err(error) => self.flash = Some(error.to_string()),
                },
                ScmEvent::CommitFiles { result, .. } => match result {
                    Ok(result) => self.apply_remote_commit_files(result),
                    Err(error) => self.flash = Some(error.to_string()),
                },
                ScmEvent::Mutated {
                    mutation, result, ..
                } => {
                    if let ScmMutation::Discard { paths } = &mutation {
                        for path in paths {
                            self.invalidate_remote(Some(&WorkbenchPath::Remote(path.clone())));
                        }
                        self.refresh_remote_tree();
                    }
                    if let Err(error) = result {
                        self.flash = Some(error.to_string());
                    }
                    self.refresh_remote_scm();
                }
            }
        }
        changed
    }

    fn drain_remote_backend(&mut self) -> bool {
        let visible = self.sidebar_rows().max(1);
        let Some(backend) = &mut self.remote_backend else {
            return false;
        };
        backend.set_pinned_paths(
            self.editor
                .active()
                .map(|tab| tab.path.clone())
                .into_iter()
                .chain(self.tree.selected().map(|row| row.path.clone()))
                .chain(
                    self.editor
                        .tabs()
                        .iter()
                        .filter(|tab| tab.resource.is_some())
                        .map(|tab| tab.path.clone()),
                )
                .chain(
                    self.pending_open
                        .values()
                        .map(|pending| pending.path.clone()),
                )
                .chain(
                    self.tree
                        .rows()
                        .iter()
                        .skip(self.tree.scroll())
                        .take(visible)
                        .map(|row| row.path.clone()),
                ),
        );
        let events = backend.drain();
        self.tree.set_remote_root(backend.root());
        let changed = !events.is_empty();
        for event in events {
            let scm_invalidate = matches!(
                &event,
                BackendEvent::Saved { .. }
                    | BackendEvent::Created { .. }
                    | BackendEvent::Renamed { .. }
                    | BackendEvent::Deleted { .. }
            );
            match event {
                BackendEvent::Listed {
                    request,
                    result,
                    complete,
                    removed,
                    ..
                } => {
                    if complete {
                        self.remote_pending.remove(&request);
                    }
                    match result {
                        Ok(result) => self.apply_remote_listing(result, complete, removed),
                        Err(error) => self.flash = Some(error.to_string()),
                    }
                }
                BackendEvent::Opened { request, result } => {
                    self.remote_pending.remove(&request);
                    let pending = self.pending_open.remove(&request);
                    match (pending, result) {
                        (Some(pending), Ok(loaded)) => {
                            self.apply_remote_open(pending.purpose, loaded);
                            if pending.invalidated {
                                self.invalidate_remote(Some(&pending.path));
                            }
                        }
                        (Some(pending), Err(error)) => {
                            self.pending_lines.remove(&pending.path);
                            if let Some(tab) = self
                                .editor
                                .tabs_mut()
                                .iter_mut()
                                .find(|tab| tab.path == pending.path)
                            {
                                tab.remote_reload = false;
                            }
                            if pending.invalidated {
                                self.invalidate_remote(Some(&pending.path));
                            } else if pending.purpose == OpenPurpose::Reload
                                && matches!(error, BackendError::NotFound)
                            {
                                self.close_tabs_under(&pending.path);
                            }
                            self.flash = Some(error.to_string());
                        }
                        (None, _) => {}
                    }
                }
                BackendEvent::Saved { request, result } => {
                    self.remote_pending.remove(&request);
                    let path = self.pending_save.remove(&request);
                    match (path, result) {
                        (Some(path), Ok(entry)) => {
                            if let Some(tab) = self
                                .editor
                                .tabs_mut()
                                .iter_mut()
                                .find(|tab| tab.path == path)
                            {
                                tab.apply_saved(entry);
                            }
                            self.refresh_remote_tree();
                        }
                        (Some(path), Err(error)) => {
                            if matches!(error, BackendError::Conflict | BackendError::Indeterminate)
                                && let Some(tab) = self
                                    .editor
                                    .tabs_mut()
                                    .iter_mut()
                                    .find(|tab| tab.path == path)
                            {
                                tab.conflict = true;
                            }
                            self.flash = Some(error.to_string());
                            self.invalidate_remote(Some(&path));
                            self.refresh_remote_tree();
                        }
                        (None, _) => {}
                    }
                }
                BackendEvent::Created { request, result } => {
                    self.remote_pending.remove(&request);
                    let kind = self.pending_create.remove(&request);
                    match (kind, result) {
                        (Some(InputKind::NewFile), Ok(entry)) => {
                            self.request_remote_open(entry, OpenPurpose::Open);
                            self.refresh_remote_tree();
                        }
                        (Some(InputKind::NewFolder), Ok(entry)) => {
                            self.invalidate_remote(Some(&entry.path));
                            self.refresh_remote_tree();
                        }
                        (Some(_), Err(error)) => {
                            if matches!(error, BackendError::Indeterminate) {
                                self.refresh_remote_tree();
                            }
                            self.flash = Some(error.to_string());
                        }
                        _ => {}
                    }
                }
                BackendEvent::Renamed {
                    request,
                    source,
                    result,
                } => {
                    self.remote_pending.remove(&request);
                    match result {
                        Ok(entry) => {
                            self.apply_remote_rename(&source, &entry);
                            self.refresh_remote_tree();
                        }
                        Err(error) => {
                            if matches!(error, BackendError::Conflict) {
                                self.mark_remote_conflict(&source);
                            }
                            if matches!(error, BackendError::Indeterminate) {
                                self.refresh_remote_tree();
                            }
                            self.flash = Some(error.to_string());
                        }
                    }
                }
                BackendEvent::Deleted {
                    request,
                    path,
                    result,
                } => {
                    self.remote_pending.remove(&request);
                    match result {
                        Ok(()) => {
                            self.close_tabs_under(&path);
                            self.refresh_remote_tree();
                        }
                        Err(error) => {
                            if matches!(error, BackendError::Conflict) {
                                self.mark_remote_conflict(&path);
                            }
                            if matches!(error, BackendError::Indeterminate) {
                                self.refresh_remote_tree();
                            }
                            self.flash = Some(error.to_string());
                        }
                    }
                }
                BackendEvent::Counted { request, result } => {
                    self.remote_pending.remove(&request);
                    let path = self.pending_count.remove(&request);
                    match (path, result) {
                        (Some(path), Ok(count)) => {
                            self.delete_target = self.mutation_resource(&path);
                            self.confirm = Some(Confirm {
                                ask: Ask::Delete(count),
                                choice: Choice::Cancel,
                            });
                        }
                        (Some(_), Err(error)) => self.flash = Some(error.to_string()),
                        (None, _) => {}
                    }
                }
                BackendEvent::SearchPage { request, result } => {
                    self.remote_pending.remove(&request);
                    match result {
                        Ok(result) => self.search.apply_remote(request, result),
                        Err(error) => self.search.fail_remote(request, error.to_string()),
                    }
                }
                BackendEvent::WatchPolled {
                    result: Ok(result), ..
                } => self.apply_remote_watch(result),
                BackendEvent::WatchOpened { result: Err(_), .. }
                | BackendEvent::WatchPolled { result: Err(_), .. } => {
                    self.flash = Some(WATCH_WARNING.to_owned())
                }
                BackendEvent::WatchOpened {
                    result: Ok(Some(_)),
                    ..
                } => self.invalidate_remote(None),
                BackendEvent::WatchOpened {
                    result: Ok(None), ..
                } => {}
            }
            if scm_invalidate {
                self.refresh_remote_scm();
            }
        }
        self.reload_remote_targets();
        changed
    }

    fn apply_remote_listing(
        &mut self,
        result: ListResult,
        complete: bool,
        removed: Vec<WorkbenchPath>,
    ) {
        if complete && result.incomplete {
            self.flash = Some(LISTING_INCOMPLETE.to_owned());
        }
        if !result.entries.is_empty() || !removed.is_empty() {
            self.tree.update_remote(&result.entries, &removed);
            self.palette
                .update_remote_entries(&result.entries, &removed);
            for path in &removed {
                self.remote_entries.remove(path);
            }
            for entry in result.entries {
                self.remote_entries.insert(entry.path.clone(), entry);
            }
            self.apply_marks();
        }
    }

    fn apply_remote_open(&mut self, purpose: OpenPurpose, loaded: LoadedFile) {
        let path = loaded.entry.path.clone();
        match purpose {
            OpenPurpose::Open | OpenPurpose::Preview => {
                self.drag = Drag::None;
                let tab = Tab::from_backend(loaded, self.theme_generation);
                self.editor
                    .push_backend(tab, purpose == OpenPurpose::Preview);
                self.tree.reveal_workbench_path(&path);
                self.focus = Focus::Editor;
                if let Some(lines) = self.pending_lines.remove(&path)
                    && let Some(tab) = self.editor.active_mut()
                {
                    tab.buffer
                        .set_cursor(Cursor::new(lines.start().saturating_sub(1), 0), false);
                    tab.buffer
                        .set_cursor(Cursor::new(lines.end().saturating_sub(1), 0), true);
                    self.follow_cursor();
                }
            }
            OpenPurpose::Reload | OpenPurpose::Discard => {
                if let Some(tab) = self
                    .editor
                    .tabs_mut()
                    .iter_mut()
                    .find(|tab| tab.path == path)
                {
                    if purpose == OpenPurpose::Discard {
                        tab.discard_backend(loaded);
                    } else {
                        tab.apply_backend_reload(loaded);
                    }
                }
            }
        }
    }

    fn apply_remote_watch(&mut self, result: WatchResult) {
        let refresh_scm = match result.update {
            WatchUpdate::Events(events) => {
                let refresh = !events.is_empty();
                for event in events {
                    self.invalidate_remote(Some(&WorkbenchPath::Remote(event.path)));
                    if let Some(previous) = event.previous_path {
                        self.invalidate_remote(Some(&WorkbenchPath::Remote(previous)));
                    }
                }
                refresh
            }
            WatchUpdate::Resync => {
                self.invalidate_remote(None);
                true
            }
        };
        if refresh_scm {
            self.refresh_remote_scm();
        }
    }

    fn request_remote_open(&mut self, entry: ResourceEntry, purpose: OpenPurpose) {
        self.start_remote_read(entry.path.clone(), purpose, Some(entry));
    }

    fn apply_remote_rename(&mut self, source: &WorkbenchPath, destination: &ResourceEntry) {
        let requests = self
            .pending_open
            .iter()
            .filter(|(_, pending)| pending.path.starts_with(source))
            .map(|(request, _)| *request)
            .collect::<Vec<_>>();
        let mut retry = Vec::new();
        for request in requests {
            let Some(pending) = self.pending_open.remove(&request) else {
                continue;
            };
            if let Some(backend) = &mut self.remote_backend {
                backend.cancel_open(request);
            }
            self.remote_pending.remove(&request);
            let lines = self.pending_lines.remove(&pending.path);
            if pending.purpose == OpenPurpose::Reload {
                if let Some(tab) = self
                    .editor
                    .tabs_mut()
                    .iter_mut()
                    .find(|tab| tab.path == pending.path)
                {
                    tab.remote_reload = !tab.is_dirty();
                    tab.conflict |= tab.is_dirty();
                }
                continue;
            }
            let moved = if &pending.path == source {
                Ok(destination.path.clone())
            } else {
                destination
                    .path
                    .join(&pending.path.display_relative(source))
            };
            match moved {
                Ok(path) => retry.push((path, pending.purpose, lines)),
                Err(error) => self.flash = Some(error.to_string()),
            }
        }
        self.editor.rename_resource(
            source,
            &destination.path,
            Some(destination.clone()),
            self.theme_generation,
        );
        for tab in self.editor.tabs_mut() {
            if tab.path.starts_with(&destination.path) && tab.resource.is_some() && !tab.is_dirty()
            {
                tab.remote_reload = true;
            }
        }
        for (path, purpose, lines) in retry {
            self.request_remote_path(path.clone(), purpose);
            if let Some(lines) = lines {
                self.pending_lines.insert(path, lines);
            }
        }
    }

    fn request_remote_path(&mut self, path: WorkbenchPath, purpose: OpenPurpose) {
        let entry = matches!(purpose, OpenPurpose::Open | OpenPurpose::Preview)
            .then(|| self.remote_entries.get(&path).cloned())
            .flatten();
        self.start_remote_read(path, purpose, entry);
    }

    fn start_remote_read(
        &mut self,
        path: WorkbenchPath,
        purpose: OpenPurpose,
        entry: Option<ResourceEntry>,
    ) {
        if self
            .pending_open
            .values()
            .any(|pending| pending.path == path)
        {
            if purpose == OpenPurpose::Reload {
                return;
            }
            self.cancel_pending_opens();
        }
        if self.pending_open.len() >= MAX_REMOTE_READS {
            if purpose == OpenPurpose::Reload {
                return;
            }
            self.cancel_pending_opens();
        }
        let Some(backend) = &mut self.remote_backend else {
            return;
        };
        let request = match entry {
            Some(entry) => backend.open(entry),
            None => backend.open_path(path.clone()),
        };
        self.remote_pending.insert(request);
        self.pending_open.insert(
            request,
            PendingOpen {
                path: path.clone(),
                purpose,
                invalidated: false,
            },
        );
        if let Some(tab) = self
            .editor
            .tabs_mut()
            .iter_mut()
            .find(|tab| tab.path == path)
        {
            tab.remote_reload = false;
        }
    }

    fn invalidate_remote(&mut self, path: Option<&WorkbenchPath>) {
        for tab in self.editor.tabs_mut() {
            if tab.resource.is_some()
                && tab.path.remote().is_some()
                && path.is_none_or(|path| tab.path.starts_with(path))
            {
                if tab.is_dirty() {
                    tab.conflict = true;
                    tab.remote_reload = false;
                } else {
                    tab.remote_reload = true;
                }
            }
        }
        for pending in self.pending_open.values_mut() {
            if path.is_none_or(|path| pending.path.starts_with(path)) {
                pending.invalidated = true;
            }
        }
    }

    fn reload_remote_targets(&mut self) {
        if !self.open {
            return;
        }
        let available = MAX_REMOTE_READS.saturating_sub(self.pending_open.len());
        let paths = self
            .editor
            .tabs()
            .iter()
            .filter(|tab| {
                tab.remote_reload
                    && !self
                        .pending_open
                        .values()
                        .any(|pending| pending.path == tab.path)
            })
            .take(available)
            .map(|tab| tab.path.clone())
            .collect::<Vec<_>>();
        for path in paths {
            self.request_remote_path(path, OpenPurpose::Reload);
        }
    }

    fn cancel_pending_opens(&mut self) {
        let Some(backend) = &mut self.remote_backend else {
            return;
        };
        for request in self.pending_open.keys().copied().collect::<Vec<_>>() {
            backend.cancel_open(request);
            self.remote_pending.remove(&request);
        }
        for pending in self.pending_open.values() {
            self.pending_lines.remove(&pending.path);
            if pending.purpose == OpenPurpose::Reload
                && let Some(tab) = self
                    .editor
                    .tabs_mut()
                    .iter_mut()
                    .find(|tab| tab.path == pending.path)
            {
                tab.remote_reload = true;
            }
        }
        self.pending_open.clear();
    }

    fn refresh_remote_tree(&mut self) {
        let Some(backend) = &mut self.remote_backend else {
            return;
        };
        let request = backend.list(backend.root().clone(), true);
        self.remote_pending.insert(request);
    }

    fn refresh_remote_scm(&mut self) {
        if let Some(driver) = &mut self.remote_scm {
            driver.refresh();
        }
    }

    /// Re-reads source control on request. The host calls this for the composer's
    /// `#hash` index, which needs the same history the pane shows and has no
    /// other way to walk a remote log. Repeated calls coalesce in the driver.
    pub fn refresh_scm(&mut self) {
        self.refresh_remote_scm();
    }

    /// Whether source control still owes an answer, so a caller waiting on the
    /// log knows to look again. Narrower than [`Self::is_busy`], which also
    /// counts searches and scrolling.
    pub fn scm_refreshing(&self) -> bool {
        self.remote_scm.as_ref().is_some_and(ScmDriver::is_busy)
    }

    /// The commits source control is showing, newest first.
    pub fn scm_log(&self) -> &[scm::repo::Commit] {
        self.scm.log()
    }

    /// Folds what moved on disk into the panes: tabs catch up or raise a
    /// conflict, the tree marks what Caudra touched, and source control
    /// recounts.
    fn absorb_changes(&mut self) -> bool {
        let Some(changes) = self.watch.as_mut().map(Watch::drain) else {
            return false;
        };
        if changes.is_empty() {
            return false;
        }
        let started = Instant::now();
        let mut phase_start = started;
        let mut lap = || {
            let elapsed = phase_start.elapsed().as_millis() as u64;
            phase_start = Instant::now();
            elapsed
        };
        let files = changes.files.len();
        for path in &changes.files {
            self.reload_tab(path);
        }
        let tabs_ms = lap();
        self.touched.extend(changes.files);
        if changes.structural {
            self.tree.reload();
            self.palette.invalidate();
        }
        let tree_ms = lap();
        if changes.git {
            self.scm.refresh();
        } else {
            self.scm.refresh_worktree();
        }
        let scm_ms = lap();
        self.apply_marks();
        tracing::info!(
            files,
            structural = changes.structural,
            git = changes.git,
            tabs_ms,
            tree_ms,
            scm_ms,
            marks_ms = lap(),
            total_ms = started.elapsed().as_millis() as u64,
            touched = self.touched.len(),
            "workbench absorbed changes"
        );
        true
    }

    pub fn sidebar_view(&self) -> SidebarView {
        self.sidebar
    }

    /// What the host stores between runs.
    pub fn layout(&self) -> Layout {
        let active_tab = self.editor.active_index();
        let mut tabs = Vec::new();
        let mut active = 0;
        for (index, tab) in self.editor.tabs().iter().enumerate() {
            if !tab.is_file() {
                continue;
            }
            if index == active_tab {
                active = tabs.len();
            }
            if let Some(path) = tab.path.local() {
                tabs.push(path.to_path_buf());
            }
        }
        let (flat, sections) = self.scm.saved();
        Layout {
            tabs,
            active,
            sidebar: self.sidebar,
            sidebar_width: self.sidebar_width,
            sidebar_collapsed: self.sidebar_collapsed,
            show_hidden: self.show_hidden,
            wrap: self.wrap,
            scm: ScmLayout {
                flat,
                sections: sections
                    .into_iter()
                    .map(|(height, collapsed)| SectionLayout { height, collapsed })
                    .collect(),
            },
        }
    }

    /// Puts a stored [`Layout`] back. Files that have since gone are skipped
    /// rather than flashed: reopening is a convenience, and the reader did not
    /// ask for them now.
    pub fn restore(&mut self, layout: Layout) {
        self.sidebar = if layout.sidebar == SidebarView::Transfer {
            SidebarView::Explorer
        } else {
            layout.sidebar
        };
        self.sidebar_collapsed = layout.sidebar_collapsed;
        self.set_sidebar_width(layout.sidebar_width);
        self.show_hidden = layout.show_hidden;
        self.wrap = layout.wrap;
        self.tree.set_show_hidden(self.show_hidden);
        let sections: Vec<(u16, bool)> = layout
            .scm
            .sections
            .iter()
            .map(|section| (section.height, section.collapsed))
            .collect();
        self.scm.restore(layout.scm.flat, &sections);
        for path in &layout.tabs {
            let _ = self.editor.open(path, self.theme_generation);
        }
        self.editor.select(layout.active);
        self.reveal_active();
        self.apply_marks();
    }

    pub fn focus(&self) -> Focus {
        self.focus
    }

    pub fn text_input_active(&self) -> bool {
        if self.transfer_input_active() {
            return self.open && self.transfer.text_input_active();
        }
        if !self.open {
            return false;
        }
        if self.focused_field().is_some() {
            return true;
        }
        if self.confirm.is_some() || self.menu.is_some() {
            return false;
        }
        self.focus == Focus::Editor
            && self
                .editor
                .active()
                .is_some_and(|tab| tab.is_editable() && !tab.is_rendered())
    }

    /// Inserts text the host pulled out of a bracketed paste. Reports whether
    /// anything took it, so the host can fall back to its own composer. The
    /// rendered view takes it only to refuse it, since the composer it would
    /// otherwise reach is hidden behind the workbench.
    pub fn paste(&mut self, text: &str) -> bool {
        if self.transfer_input_active() {
            self.transfer.paste(text);
            return true;
        }
        if let Some(field) = self.focused_field() {
            let text = match field {
                FocusedField::Goto => {
                    Cow::Owned(text.chars().filter(char::is_ascii_digit).collect())
                }
                _ => Cow::Borrowed(text),
            };
            if let Some(pasted) = self.field_mut(field).map(|typing| typing.paste(&text)) {
                self.typed(field, pasted);
            }
            return true;
        }
        // A question or a menu stands over the buffer and takes no text, and
        // the composer behind the workbench must not take it either.
        if self.confirm.is_some() || self.menu.is_some() {
            return true;
        }
        if self.focus != Focus::Editor {
            return false;
        }
        let Some(tab) = self.editor.active_mut() else {
            return false;
        };
        if tab.is_rendered() {
            self.flash = Some(RENDERED_READ_ONLY.to_owned());
            return true;
        }
        if !tab.is_editable() {
            return false;
        }
        let edit = tab.buffer.insert(text);
        tab.record(edit);
        tab.break_undo_group();
        self.follow_cursor();
        true
    }

    /// Routes a mouse event by the pane it landed in, using the geometry the
    /// last frame recorded. Anything outside a pane is left alone.
    pub fn handle_mouse(&mut self, event: MouseEvent) -> WorkbenchAction {
        if event.kind == MouseEventKind::Down(MouseButton::Left) {
            self.drag = Drag::None;
        }
        if self.transfer_input_active()
            && event.kind == MouseEventKind::Down(MouseButton::Left)
            && let Some((_, view)) = self
                .switcher
                .iter()
                .find(|(rect, _)| rect.contains((event.column, event.row).into()))
        {
            return self.transfer_switch(*view);
        }
        if self.transfer_input_active() {
            return self.transfer_mouse(event);
        }
        let at = (event.column, event.row);
        // The panel is anchored to a cell, so what could move the cell out from
        // under it takes it down first. Named one by one rather than as
        // everything else: a click is a press and a release, and swallowing the
        // release would take down the menu the press had just opened.
        if self.menu.is_some()
            && matches!(
                event.kind,
                MouseEventKind::ScrollUp
                    | MouseEventKind::ScrollDown
                    | MouseEventKind::ScrollLeft
                    | MouseEventKind::ScrollRight
                    | MouseEventKind::Down(MouseButton::Middle)
                    | MouseEventKind::Drag(MouseButton::Left)
            )
        {
            self.menu = None;
            return WorkbenchAction::Consumed;
        }
        if let Some(action) = self.scrollbar_mouse(&event) {
            return action;
        }
        let delta = match event.kind {
            MouseEventKind::ScrollUp => -SCROLL_LINES,
            MouseEventKind::ScrollDown => SCROLL_LINES,
            MouseEventKind::ScrollLeft => {
                self.scroll_sideways(at, -SCROLL_COLUMNS);
                return WorkbenchAction::Consumed;
            }
            MouseEventKind::ScrollRight => {
                self.scroll_sideways(at, SCROLL_COLUMNS);
                return WorkbenchAction::Consumed;
            }
            MouseEventKind::Moved => {
                self.hover = Some(at);
                return WorkbenchAction::Consumed;
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.hover = Some(at);
                self.drag_from = at;
                self.drag_at = at;
                let clicks = self.clicks.press(at, Instant::now());
                return self.press(at, clicks);
            }
            MouseEventKind::Down(MouseButton::Right) => {
                self.hover = Some(at);
                self.open_menu_at(at);
                return WorkbenchAction::Consumed;
            }
            // The tail of the press above, which has already been acted on.
            MouseEventKind::Up(MouseButton::Right) => return WorkbenchAction::Consumed,
            MouseEventKind::Down(MouseButton::Middle) => {
                self.close_under(at);
                return WorkbenchAction::Consumed;
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.drag_to(at);
                return WorkbenchAction::Consumed;
            }
            MouseEventKind::Up(MouseButton::Left) => {
                return match self.release() {
                    Some(text) => WorkbenchAction::Copy(text),
                    None => WorkbenchAction::Consumed,
                };
            }
            _ => return WorkbenchAction::Passthrough,
        };
        self.scroll(event.column, event.row, delta);
        WorkbenchAction::Consumed
    }

    /// Offers an event to every bar that was drawn, newest pane first. Skipped
    /// while a panel is up, because the menu and the dialog are painted over
    /// the bars and a press on them belongs to the panel.
    ///
    /// A bar that returns `Ignored` has to fall through: the column of a bar
    /// that is not showing belongs to whatever is painted there.
    fn scrollbar_mouse(&mut self, event: &MouseEvent) -> Option<WorkbenchAction> {
        if self.menu.is_some() || self.confirm.is_some() {
            return None;
        }
        if let Some(offset) = taken(self.bars.palette.handle(event)) {
            if let Some(offset) = offset {
                let rows = self.panes.palette.height as usize;
                self.palette.set_scroll(offset as usize, rows);
            }
            return Some(WorkbenchAction::Consumed);
        }
        if let Some(offset) = taken(self.bars.text.handle(event)) {
            let rows = self.panes.text.height as usize;
            if let Some(offset) = offset
                && let Some(tab) = self.editor.active_mut()
            {
                match tab.rendered_mut() {
                    Some(view) => view.scroll_to(offset as usize, rows),
                    None => tab.set_scroll(offset as usize),
                }
            }
            return Some(WorkbenchAction::Consumed);
        }
        if let Some(offset) = taken(self.bars.sidebar.handle(event)) {
            if let Some(offset) = offset {
                let rows = self.panes.rows.height as usize;
                match self.sidebar {
                    SidebarView::Explorer => self.tree.set_scroll(offset as usize, rows),
                    SidebarView::Search => self.search.set_scroll(offset as usize, rows),
                    SidebarView::SourceControl | SidebarView::Transfer => {}
                }
            }
            return Some(WorkbenchAction::Consumed);
        }
        let hit = Section::ALL
            .into_iter()
            .enumerate()
            .find_map(|(index, section)| {
                taken(self.bars.sections[index].handle(event)).map(|offset| (section, offset))
            });
        let (section, offset) = hit?;
        if let Some(offset) = offset {
            let rows = self.panes.sections[section.index()].body.height as usize;
            self.scm.set_scroll(section, offset as usize, rows);
        }
        Some(WorkbenchAction::Consumed)
    }

    /// A wheel turn over `(column, row)`, worth `delta` rows and negative
    /// upwards. The host coalesces a burst of notches and scales them by the
    /// configured scroll size, so this is its own entry point rather than a
    /// [`MouseEvent`] the caller has to build.
    pub fn scroll(&mut self, column: u16, row: u16, delta: isize) {
        if self.transfer_input_active() {
            self.transfer.scroll(delta);
            return;
        }
        let at = (column, row);
        if self.sidebar == SidebarView::SourceControl
            && self
                .panes
                .sidebar
                .is_some_and(|rect| rect.contains(at.into()))
        {
            // Each section scrolls on its own, so the wheel answers to the one
            // the pointer is over rather than to whichever was last touched.
            if let Some((section, body)) = self.section_under(at) {
                self.scm.scroll_by(section, delta, body.height as usize);
            }
        } else if self
            .panes
            .sidebar
            .is_some_and(|rect| rect.contains(at.into()))
        {
            let rows = self.sidebar_rows();
            match self.sidebar {
                SidebarView::Explorer => self.tree.scroll_by(delta, rows),
                SidebarView::Search => self.search.scroll_by(delta, rows),
                SidebarView::SourceControl | SidebarView::Transfer => {}
            }
        } else if self.panes.text.contains(at.into()) {
            let text = self.panes.text;
            let wrap = self.wrap;
            if let Some(tab) = self.editor.active_mut() {
                match tab.rendered_mut() {
                    Some(view) => view.scroll_by(delta, text.height as usize),
                    None => tab.scroll_by(delta, text.height as usize, text.width as usize, wrap),
                }
            }
        }
    }

    /// A sideways wheel over `at`, worth `delta` display columns and negative
    /// leftwards. Only the text pane pans: the sidebar lists already cut
    /// themselves to their width, so there is nothing off to their side.
    fn scroll_sideways(&mut self, at: (u16, u16), delta: isize) {
        let text = self.panes.text;
        // A wrapped pane holds every column it has, so there is nothing to pan
        // towards and a sideways wheel is nothing to answer.
        if self.wrap || !text.contains(at.into()) {
            return;
        }
        if let Some(tab) = self.editor.active_mut()
            && !tab.is_rendered()
        {
            tab.h_scroll_by(delta, text.height as usize, text.width as usize);
        }
    }

    /// A left press, told how many landed on this cell in a row. The panes are
    /// tried in painting order, so the palette gets the press it is covering.
    /// The status row's hints go first, since they answer for whatever stands
    /// in front, a panel included. A menu hung over the row has already taken
    /// back the hints it covers.
    fn press(&mut self, at: (u16, u16), clicks: u8) -> WorkbenchAction {
        if let Some(bind) = self.hint_at(at) {
            return self.press_hint(bind);
        }
        let position = at.into();
        if let Some(confirm) = self.confirm {
            if self.panes.confirm.contains(position)
                && let Some(choice) = view::confirm_at(at.0, self.panes.confirm.x, confirm.ask)
            {
                return self.resolve(confirm.ask, choice);
            }
            return WorkbenchAction::Consumed;
        }
        if self.menu.is_some() {
            return self.press_menu(at);
        }
        if self.palette.is_open() {
            if self.panes.palette.contains(position) {
                self.open_palette_row((at.1 - self.panes.palette.y) as usize);
            }
            return WorkbenchAction::Consumed;
        }
        if self
            .panes
            .separator
            .is_some_and(|rect| rect.contains(position))
        {
            self.drag = Drag::Separator;
            return WorkbenchAction::Consumed;
        }
        if self.panes.tabs.contains(position) {
            self.focus = Focus::Editor;
            if let Some(hit) = view::tab_at(&self.editor, at.0, self.panes.tabs) {
                self.hit_tab(hit, at);
            }
            return WorkbenchAction::Consumed;
        }
        if self.panes.header.contains(position) {
            if let Some((_, view)) = self
                .switcher
                .iter()
                .find(|(rect, _)| rect.contains(position))
            {
                return self.transfer_switch(*view);
            } else if self.header_button().is_some_and(|label| {
                view::button_at(
                    at.0,
                    self.panes.header,
                    self.header_context().width(),
                    label,
                )
            }) {
                self.focus = Focus::Sidebar;
                self.press_header_button();
            }
            return WorkbenchAction::Consumed;
        }
        if self.panes.toggles.contains(position) {
            if let Some(toggle) = view::toggle_at(at.0, self.panes.toggles.x) {
                self.focus = Focus::Sidebar;
                match toggle {
                    Toggle::Case => self.search.toggle_case(),
                    Toggle::Word => self.search.toggle_word(),
                    Toggle::Regex => self.search.toggle_regex(),
                }
            }
            return WorkbenchAction::Consumed;
        }
        if self.sidebar == SidebarView::SourceControl && self.press_scm(at) {
            return WorkbenchAction::Consumed;
        }
        if self.panes.rows.contains(position) {
            // Only the explorer paints a handle there. Search results keep
            // their leftmost columns for the path, so a press on them is a
            // press on the result.
            if self.sidebar == SidebarView::Explorer && view::on_menu_mark(at.0, self.panes.rows.x)
            {
                self.open_menu_at(at);
            } else {
                self.press_row((at.1 - self.panes.rows.y) as usize, clicks);
            }
            return WorkbenchAction::Consumed;
        }
        if self.panes.text.contains(position) {
            self.press_text(at, clicks);
        }
        WorkbenchAction::Consumed
    }

    /// The key the status hint under `at` stands in for, as the last frame
    /// laid the hints out.
    fn hint_at(&self, at: (u16, u16)) -> Option<keys::Bind> {
        self.hint_hits
            .iter()
            .find(|(rect, _)| rect.contains(at.into()))
            .map(|(_, bind)| *bind)
    }

    /// Presses the key a status hint names, through the same door the keyboard
    /// uses, so the two cannot drift. The row is asked again first: the frame
    /// that placed the hint can predate a key that changed what the row
    /// offers, and a stale `U upload` would otherwise type into the root
    /// prompt that key opened.
    fn press_hint(&mut self, bind: keys::Bind) -> WorkbenchAction {
        if !self
            .offered_hints()
            .iter()
            .any(|(offered, _)| *offered == bind)
        {
            return WorkbenchAction::Consumed;
        }
        let key = bind.to_key_event();
        match keys::LEADER_BINDS.contains(&bind) {
            true => self.handle_leader(key),
            false => self.handle_key(key),
        }
    }

    /// Opens the menu on whatever the right button came down over. A press
    /// anywhere with nothing to offer takes down whatever was open, so the
    /// button never leaves a menu standing over nothing.
    fn open_menu_at(&mut self, at: (u16, u16)) {
        self.menu = None;
        if self.confirm.is_some() || self.palette.is_open() {
            return;
        }
        let position = at.into();
        if self.panes.tabs.contains(position) {
            let Some(hit) = view::tab_at(&self.editor, at.0, self.panes.tabs) else {
                return;
            };
            self.focus = Focus::Editor;
            self.menu = self
                .editor
                .tabs()
                .get(hit.index)
                .map(|tab| Menu::for_tab(tab, hit.index, at, self.renders(tab)));
            return;
        }
        if self.sidebar != SidebarView::Explorer || !self.panes.rows.contains(position) {
            return;
        }
        let row = self.tree.scroll() + (at.1 - self.panes.rows.y) as usize;
        self.tree.select_index(row);
        // Selecting refuses a row past the end, so the air under a short tree
        // opens nothing rather than a menu for whatever was last selected.
        if self.tree.selected_index() != row {
            return;
        }
        self.focus = Focus::Sidebar;
        self.menu = self.tree.selected().map(|row| Menu::for_row(row, at));
    }

    /// The menu for wherever the cursor already is, which is how the keyboard
    /// reaches it.
    fn open_menu(&mut self) {
        self.menu = None;
        if self.confirm.is_some() {
            return;
        }
        match self.focus {
            Focus::Sidebar if self.sidebar == SidebarView::Explorer => {
                let rows = self.panes.rows;
                let offset = self
                    .tree
                    .selected_index()
                    .saturating_sub(self.tree.scroll());
                let at = (rows.x, rows.y + offset as u16);
                self.menu = self.tree.selected().map(|row| Menu::for_row(row, at));
            }
            Focus::Editor => {
                let index = self.editor.active_index();
                let at = (self.panes.tabs.x, self.panes.tabs.y);
                self.menu = self
                    .editor
                    .tabs()
                    .get(index)
                    .map(|tab| Menu::for_tab(tab, index, at, self.renders(tab)));
            }
            _ => {}
        }
    }

    /// A press while the menu is up. Anything off the panel takes it down and
    /// is swallowed, so the press that dismisses a menu never also acts on
    /// what is underneath it.
    fn press_menu(&mut self, at: (u16, u16)) -> WorkbenchAction {
        self.drag = Drag::Menu;
        let Some(menu) = &self.menu else {
            return WorkbenchAction::Consumed;
        };
        if !self.panes.menu.contains(at.into()) {
            self.menu = None;
            return WorkbenchAction::Consumed;
        }
        let offset = (at.1 - self.panes.menu.y) as usize;
        // A rule between groups is part of the panel, so a press on one leaves
        // the menu standing rather than punishing a near miss.
        let Some(action) = menu.action_at(offset) else {
            return WorkbenchAction::Consumed;
        };
        let target = menu.target().clone();
        self.menu = None;
        self.run_menu(action, &target)
    }

    /// Takes the answer under the menu's own cursor, which is what `Enter`
    /// does.
    fn take_menu(&mut self) -> WorkbenchAction {
        let Some(menu) = self.menu.take() else {
            return WorkbenchAction::Consumed;
        };
        match menu.selected() {
            Some(action) => self.run_menu(action, menu.target()),
            None => WorkbenchAction::Consumed,
        }
    }

    fn run_menu(&mut self, action: MenuAction, target: &Target) -> WorkbenchAction {
        match target {
            Target::Row(path) => self.run_on_row(action, path.clone()),
            Target::Tab(index) => self.run_on_tab(action, *index),
        }
    }

    fn run_on_row(&mut self, action: MenuAction, path: WorkbenchPath) -> WorkbenchAction {
        match action {
            MenuAction::Open => self.open_workbench_path(&path, OpenPurpose::Open),
            MenuAction::CopyPath => return self.copy(path.display().to_string()),
            MenuAction::CopyRelative => {
                return self.copy(self.relative_path(&path));
            }
            MenuAction::SendToComposer => return self.mention(&path, None),
            MenuAction::Rename => self.ask_for_name(InputKind::Rename, path),
            MenuAction::NewFile => self.ask_for_name(InputKind::NewFile, self.holder(&path)),
            MenuAction::NewFolder => self.ask_for_name(InputKind::NewFolder, self.holder(&path)),
            MenuAction::Delete => {
                if let Some(backend) = &mut self.remote_backend {
                    let request = backend.count(path.clone());
                    self.remote_pending.insert(request);
                    self.pending_count.insert(request, path);
                } else if let Some(path) = path.local() {
                    self.confirm = Some(Confirm {
                        ask: Ask::Delete(ops::count_under(path)),
                        choice: Choice::Cancel,
                    });
                }
            }
            // Every other action belongs to the tab menu, which no row opens.
            _ => {}
        }
        WorkbenchAction::Consumed
    }

    /// Where something new made from `path` lands: inside a folder, and beside
    /// a file.
    fn holder(&self, path: &WorkbenchPath) -> WorkbenchPath {
        match self.tree.resource(path).map(|entry| entry.kind) {
            Some(caudra_workspace::ResourceKind::Directory) => path.clone(),
            Some(_) => path.parent().unwrap_or_else(|| self.backend_root()),
            None => match path.local() {
                Some(local) if local.is_dir() => path.clone(),
                Some(local) => {
                    WorkbenchPath::Local(local.parent().unwrap_or(&self.root).to_path_buf())
                }
                None => self.backend_root(),
            },
        }
    }

    /// Puts the question in the status row. A rename starts from the name it
    /// already has, since most renames change part of one.
    fn ask_for_name(&mut self, kind: InputKind, at: WorkbenchPath) {
        let value = match kind {
            InputKind::Rename => at.file_name(),
            _ => String::new(),
        };
        self.input = Some(Input::new(kind, at, &value));
    }

    /// Acts on the name that was typed. A refusal keeps the question up with
    /// what was typed still in it, so a near miss can be corrected rather than
    /// retyped.
    fn commit_input(&mut self) {
        let Some(input) = self.input.clone() else {
            return;
        };
        if self.remote_backend.is_some() {
            self.commit_remote_input(input);
            return;
        }
        let Some(at) = input.at.local() else {
            self.flash = Some(BackendError::WrongBackend.to_string());
            return;
        };
        let name = input.value.text();
        let done = match input.kind {
            InputKind::Rename => self.rename_path(at, &name),
            InputKind::NewFile => ops::create_file(at, &name).map(|path| {
                self.open_path(&path);
            }),
            InputKind::NewFolder => ops::create_dir(at, &name).map(|path| {
                self.tree.reveal(&path);
            }),
        };
        match done {
            Ok(()) => {
                self.input = None;
                self.reread();
            }
            Err(error) => self.flash = Some(error.to_string()),
        }
    }

    fn commit_remote_input(&mut self, input: Input) {
        let name = input.value.text();
        let destination = match input.kind {
            InputKind::Rename => input
                .at
                .parent()
                .unwrap_or_else(|| self.backend_root())
                .join(&name),
            InputKind::NewFile | InputKind::NewFolder => input.at.join(&name),
        };
        let destination = match destination {
            Ok(path) => path,
            Err(error) => {
                self.flash = Some(error.to_string());
                return;
            }
        };
        let entry = self.mutation_resource(&input.at);
        let Some(backend) = &mut self.remote_backend else {
            return;
        };
        let request = match input.kind {
            InputKind::Rename => {
                let Some(entry) = entry else {
                    self.flash = Some(BackendError::MissingRevision.to_string());
                    return;
                };
                backend.rename(entry, destination)
            }
            InputKind::NewFile => backend.create_file(destination),
            InputKind::NewFolder => backend.create_dir(destination),
        };
        self.remote_pending.insert(request);
        if input.kind != InputKind::Rename {
            self.pending_create.insert(request, input.kind);
        }
        self.input = None;
    }

    /// Renames a path and takes the tabs with it, including every tab under a
    /// folder that moved.
    fn rename_path(&mut self, path: &Path, name: &str) -> Result<(), ops::OpsError> {
        let moved = ops::rename(path, name)?;
        self.editor.rename(path, &moved, self.theme_generation);
        self.tree.reload();
        self.tree.reveal(&moved);
        Ok(())
    }

    /// Reads the project again after the workbench itself changed it. The
    /// watcher would catch up on its own, and waiting for it leaves the tree
    /// showing a path that is no longer there.
    fn reread(&mut self) {
        self.tree.reload();
        self.scm.refresh();
        self.palette.invalidate();
        self.apply_marks();
    }

    fn run_on_tab(&mut self, action: MenuAction, index: usize) -> WorkbenchAction {
        let Some(path) = self.editor.tabs().get(index).map(|tab| tab.path.clone()) else {
            return WorkbenchAction::Consumed;
        };
        match action {
            MenuAction::Close => self.close_at(index),
            MenuAction::KeepOpen => {
                if let Some(tab) = self.editor.tabs_mut().get_mut(index) {
                    tab.preview = false;
                }
            }
            MenuAction::Save => {
                self.cancel_pending_opens();
                self.editor.select(index);
                return self.save_active();
            }
            MenuAction::ShowRendered | MenuAction::ShowSource => {
                self.cancel_pending_opens();
                self.editor.select(index);
                self.toggle_rendered();
            }
            MenuAction::RevealInExplorer => {
                self.show_explorer();
                self.focus = Focus::Sidebar;
                self.tree.reveal_workbench_path(&path);
            }
            MenuAction::CopyPath => return self.copy(path.display().to_string()),
            MenuAction::CopyRelative => {
                return self.copy(self.relative_path(&path));
            }
            MenuAction::CloseOthers => self.close_many(self.tab_paths(|at| at != index)),
            MenuAction::CloseRight => self.close_many(self.tab_paths(|at| at > index)),
            MenuAction::CloseAll => self.close_many(self.tab_paths(|_| true)),
            // Nothing saved can raise a question, so this one needs no queue.
            MenuAction::CloseSaved => {
                self.editor.close_where(&|tab| !tab.is_dirty());
                self.reveal_active();
            }
            // Every other action belongs to the explorer's menu, which no tab
            // opens.
            _ => {}
        }
        WorkbenchAction::Consumed
    }

    /// The paths of the tabs a batch close covers, read before anything closes
    /// because closing renumbers what is left.
    fn tab_paths(&self, covered: impl Fn(usize) -> bool) -> Vec<WorkbenchPath> {
        self.editor
            .tabs()
            .iter()
            .enumerate()
            .filter(|(at, _)| covered(*at))
            .map(|(_, tab)| tab.path.clone())
            .collect()
    }

    /// Closes a list of tabs, stopping at the first that has to ask about
    /// unsaved work. The rest wait until that question is answered.
    fn close_many(&mut self, doomed: Vec<WorkbenchPath>) {
        self.closing = doomed;
        self.closing.reverse();
        self.close_next();
    }

    fn close_next(&mut self) {
        while let Some(path) = self.closing.pop() {
            let Some(index) = self.editor.tabs().iter().position(|tab| tab.path == path) else {
                continue;
            };
            self.close_at(index);
            if self.confirm.is_some() {
                return;
            }
        }
    }

    /// Puts `text` on the system clipboard and keeps a copy for the buffer's
    /// own paste, the same as letting go of a selection does.
    fn copy(&mut self, text: String) -> WorkbenchAction {
        self.clipboard = text.clone();
        WorkbenchAction::Copy(text)
    }

    /// A press somewhere in the source control pane. Reports whether it landed
    /// on a section, so the caller can go on trying the other panes.
    fn press_scm(&mut self, at: (u16, u16)) -> bool {
        if let Some(index) = self.header_under(at) {
            let section = Section::ALL[index];
            self.focus = Focus::Sidebar;
            self.scm.select(section, None);
            let control = self.header_control(at, section, index);
            self.disarm_unless_revert(control);
            if let Some(control) = control {
                self.run_scm_control(control);
                return true;
            }
            // Armed rather than acted on: the same press starts a resize, and
            // only the release can tell the two apart.
            self.drag = Drag::Section(index);
            return true;
        }
        let Some((section, body)) = self.section_under(at) else {
            return false;
        };
        self.focus = Focus::Sidebar;
        let row = self.scm.scroll(section) + (at.1 - body.y) as usize;
        let control = self.row_control(at, section, body, row);
        self.scm.select(section, Some(row));
        self.disarm_unless_revert(control);
        if let Some(control) = control {
            self.run_scm_control(control);
            return true;
        }
        let cursor = self.scm.cursor();
        if cursor.section != section || cursor.row != Some(row) {
            return true;
        }
        match self.on_closed_commit() {
            true => self.open_commit_detail(),
            false => self.activate_scm(),
        }
        true
    }

    /// Whether the cursor is on a commit that is closed, which a click opens
    /// and shows at once, the way `D` does. A click on an open commit only
    /// closes it: showing it as well would read its paths, and reading its
    /// paths is what opens it again.
    fn on_closed_commit(&self) -> bool {
        self.scm
            .selected_commit()
            .is_some_and(|commit| self.scm.commit_files(&commit.id).is_none())
    }

    /// Cancels an armed discard, which every press but the one that confirms it
    /// does, the same way every key but [`keys::DISCARD`] does.
    fn disarm_unless_revert(&mut self, control: Option<Control>) {
        if control != Some(Control::Revert) {
            self.scm.disarm();
        }
    }

    /// The control a press on a section header landed on, measured against the
    /// rect the last frame recorded so it answers to what was painted.
    fn header_control(&self, at: (u16, u16), section: Section, index: usize) -> Option<Control> {
        view::control_at(
            at.0,
            self.panes.sections[index].header,
            view::header_trailing(self.scm.count(section)),
            view::scm_controls(section, None),
        )
    }

    /// The control a press on a body row landed on. `body` is the rect
    /// [`Workbench::render_section`] reported, so a scrollbar has already been
    /// taken out of it and a press on the bar reaches no control.
    fn row_control(
        &self,
        at: (u16, u16),
        section: Section,
        body: Rect,
        row: usize,
    ) -> Option<Control> {
        let listed = *self.scm.rows(section).get(row)?;
        view::control_at(
            at.0,
            body,
            view::row_trailing(listed),
            view::scm_controls(section, Some(listed)),
        )
    }

    /// Runs a control against whatever the pane's cursor now points at, which
    /// the press has already moved onto the row that was clicked.
    fn run_scm_control(&mut self, control: Control) {
        match control {
            Control::Stage => self.stage_selected(),
            Control::Revert => self.revert_selected(),
            Control::Open => {
                let Some(change) = self.scm.selected_change().cloned() else {
                    return;
                };
                if self.remote_scm.is_some() {
                    if let Ok(path) = WorkspacePath::new(change.relative) {
                        self.open_workbench_path(&WorkbenchPath::Remote(path), OpenPurpose::Open);
                    }
                } else {
                    self.open_path(&change.path);
                }
            }
        }
    }

    /// The section header the pointer is on, by index into [`Section::ALL`].
    fn header_under(&self, at: (u16, u16)) -> Option<usize> {
        self.panes
            .sections
            .iter()
            .position(|rects| rects.header.contains(at.into()))
    }

    /// The section body the pointer is on, and the rect it was drawn in.
    fn section_under(&self, at: (u16, u16)) -> Option<(Section, Rect)> {
        self.panes
            .sections
            .iter()
            .position(|rects| rects.body.contains(at.into()))
            .map(|index| (Section::ALL[index], self.panes.sections[index].body))
    }

    /// A press on the sidebar's list, which does whatever `Enter` would have
    /// done to the row under it. One click on a file only previews it: the
    /// tab stays until the next single click takes it over, so walking a tree
    /// leaves no trail of tabs behind. Two clicks keep it.
    fn press_row(&mut self, offset: usize, clicks: u8) {
        self.focus = Focus::Sidebar;
        match self.sidebar {
            SidebarView::Explorer => {
                let row = self.tree.scroll() + offset;
                self.tree.select_index(row);
                // Selecting refuses a row past the end, so the empty space
                // under a short list opens nothing rather than whatever the
                // cursor happened to be left on.
                if self.tree.selected_index() != row {
                    return;
                }
                match self.tree.selected().is_some_and(fs::tree::Row::is_dir) {
                    true => drop(self.tree.toggle_selected()),
                    false if clicks == 1 => self.preview_selected(),
                    false => self.open_selected(),
                }
            }
            // Source control routes through `press_scm`: its rows belong to a
            // section rather than to one list filling the sidebar.
            SidebarView::SourceControl | SidebarView::Transfer => {}
            SidebarView::Search => {
                let row = self.search.scroll() + offset;
                self.search.select_index(row);
                if self.search.selected_index() == row {
                    self.open_search_selection();
                }
            }
        }
    }

    /// A press on the buffer. Every press starts a drag, so its release can
    /// copy what it took: one click drops the cursor, two take the word under
    /// it, three take the whole line. The rendered view has no caret, so there
    /// the press anchors a selection of its painted rows instead.
    fn press_text(&mut self, at: (u16, u16), clicks: u8) {
        self.focus = Focus::Editor;
        let Some(cursor) = self.cursor_at(at) else {
            return;
        };
        self.drag = Drag::Text;
        self.drag_at = at;
        let Some(tab) = self.editor.active_mut() else {
            return;
        };
        if let Some(view) = tab.rendered_mut() {
            view.select_at(cursor, clicks);
            return;
        }
        match clicks {
            1 => tab.buffer.set_cursor(cursor, false),
            2 => tab.buffer.select_word_at(cursor),
            _ => tab.buffer.select_line_at(cursor.line),
        }
    }

    /// A palette row is a menu entry rather than a tree node, so one click
    /// takes it.
    fn open_palette_row(&mut self, offset: usize) {
        self.palette.select_index(self.palette.scroll() + offset);
        let remote = self.palette.selected_remote();
        let chosen = self
            .remote_backend
            .is_none()
            .then(|| self.palette.selected(&self.root))
            .flatten();
        self.palette.close();
        if let Some(entry) = remote {
            self.request_remote_open(entry, OpenPurpose::Open);
        } else if let Some(path) = chosen {
            self.open_path(&path);
        }
    }

    /// A press on the strip, told the cell it landed on because the menu is
    /// anchored where it was asked for.
    fn hit_tab(&mut self, hit: TabHit, at: (u16, u16)) {
        match hit.part {
            TabPart::Close => self.close_at(hit.index),
            // Opened for the tab under the pointer, which stays where it is:
            // reading a menu is not a reason to leave the file on screen.
            TabPart::Menu => self.open_menu_at(at),
            TabPart::Body => {
                self.cancel_pending_opens();
                self.editor.select(hit.index);
                self.reveal_active();
            }
        }
    }

    /// Held to the same modal rule as a left press: a question about one tab
    /// cannot be answered by closing another one behind it.
    fn close_under(&mut self, at: (u16, u16)) {
        if self.confirm.is_some() || !self.panes.tabs.contains(at.into()) {
            return;
        }
        if let Some(hit) = view::tab_at(&self.editor, at.0, self.panes.tabs) {
            self.focus = Focus::Editor;
            self.close_at(hit.index);
        }
    }

    /// Closing through the same guard the keyboard uses, so an unsaved tab
    /// asks the pointer the same question it asks the keyboard.
    fn close_at(&mut self, index: usize) {
        self.cancel_pending_opens();
        self.editor.select(index);
        match self.editor.active().is_some_and(Tab::is_dirty) {
            true => {
                self.confirm = Some(Confirm {
                    ask: Ask::Close,
                    choice: Choice::Save,
                })
            }
            false => {
                self.editor.close_active();
                self.reveal_active();
            }
        }
    }

    fn drag_to(&mut self, at: (u16, u16)) {
        match self.drag {
            Drag::Separator => {
                let start = self.panes.sidebar.map_or(0, |rect| rect.x);
                self.set_sidebar_width(at.0.saturating_sub(start));
            }
            Drag::Text => {
                self.drag_at = at;
                self.extend_to(at);
            }
            Drag::Section(index) => {
                self.drag_at = at;
                self.drag_border(index, at.1);
            }
            // A drag takes the menu down before it reaches here, so a press it
            // took has nothing left to follow.
            Drag::Menu | Drag::None => {}
        }
    }

    /// Moves the border above section `index` to `row`, by resizing the nearest
    /// open section above it. Whatever flexes below takes up the difference,
    /// so one border only ever moves one section's own height.
    fn drag_border(&mut self, index: usize, row: u16) {
        let Some(above) = (0..index)
            .rev()
            .find(|above| self.panes.sections[*above].body.height > 0)
        else {
            return;
        };
        let top = self.panes.sections[above].body.y;
        self.scm
            .set_height(Section::ALL[above], row.saturating_sub(top));
    }

    /// The button came back up. A press that never moved was a click, which is
    /// how one press on a section header both folds it and resizes it.
    ///
    /// Reports whatever selection the press leaves behind in the buffer, which
    /// the caller puts on the system clipboard. The workbench holds the mouse
    /// while it is open, so the terminal underneath can no longer copy a
    /// selection the way it would from the transcript.
    fn release(&mut self) -> Option<String> {
        if let Drag::Section(index) = self.drag
            && self.drag_at == self.drag_from
        {
            self.scm.toggle_collapsed(Section::ALL[index]);
        }
        let held = self.drag;
        self.drag = Drag::None;
        if held != Drag::Text || !self.panes.text.contains(self.drag_from.into()) {
            return None;
        }
        // A plain click collapses the selection, so an idle press never
        // clobbers what was copied before it. The press was in the text, so
        // a selection standing in a field is not what it took.
        let text = self.editor.active()?.selected_text()?;
        self.clipboard = text.clone();
        Some(text)
    }

    fn extend_to(&mut self, at: (u16, u16)) {
        let Some(cursor) = self.cursor_at(at) else {
            return;
        };
        if let Some(tab) = self.editor.active_mut() {
            match tab.rendered_mut() {
                Some(view) => view.extend_to(cursor),
                None => tab.buffer.set_cursor(cursor, true),
            }
        }
    }

    /// The buffer position under `at`, which a drag may have carried outside
    /// the text pane entirely.
    fn cursor_at(&self, at: (u16, u16)) -> Option<Cursor> {
        let text = self.panes.text;
        if text.width == 0 || text.height == 0 {
            return None;
        }
        let tab = self.editor.active()?;
        let row = at.1.clamp(text.y, text.bottom() - 1);
        if let Some(view) = tab.rendered_view() {
            let column = at.0.clamp(text.x, text.right());
            return view.position_at(
                view.top(text.height as usize) + (row - text.y) as usize,
                (column - text.x) as usize,
            );
        }
        let column = at.0.clamp(text.x, text.right() - 1);
        // The same walk the frame was painted from, so a press on a wrapped
        // row cannot land on a different half of the line than it points at.
        let rows = tab.visible_rows(text.height as usize, text.width as usize, self.wrap);
        let visual = rows.get((row - text.y) as usize).or_else(|| rows.last())?;
        let reached = visual.start + (column - text.x) as usize;
        let col = editor::render::char_index(tab.buffer.line(visual.line), reached);
        Some(Cursor::new(visual.line, col))
    }

    /// How far a text drag that has run off the pane wants to scroll. Zero
    /// while the pointer is still inside it, or while nothing is dragging.
    fn edge_scroll_delta(&self) -> isize {
        if self.drag != Drag::Text {
            return 0;
        }
        let text = self.panes.text;
        match self.drag_at.1 {
            row if row < text.y => -EDGE_SCROLL_LINES,
            row if row >= text.bottom() => EDGE_SCROLL_LINES,
            _ => 0,
        }
    }

    /// Scrolls a drag that has run off the pane and carries the selection with
    /// it. Reports whether anything moved.
    fn edge_scroll(&mut self) -> bool {
        let delta = self.edge_scroll_delta();
        if delta == 0 {
            return false;
        }
        let (text, wrap) = (self.panes.text, self.wrap);
        let Some(tab) = self.editor.active_mut() else {
            return false;
        };
        let moved = match tab.rendered_mut() {
            Some(view) => {
                if !view.is_selecting() {
                    self.drag = Drag::None;
                    return false;
                }
                let rows = text.height as usize;
                let before = view.top(rows);
                view.scroll_by(delta, rows);
                view.top(rows) != before
            }
            None => {
                let before = tab.scroll();
                tab.scroll_by(delta, text.height as usize, text.width as usize, wrap);
                tab.scroll() != before
            }
        };
        if !moved {
            return false;
        }
        self.extend_to(self.drag_at);
        true
    }

    /// The pointer's position while it rests inside `rect`.
    fn hovering(&self, rect: Rect) -> Option<(u16, u16)> {
        self.hover.filter(|at| rect.contains((*at).into()))
    }

    /// Which row of `rect` the pointer is resting on.
    fn hovered_row(&self, rect: Rect) -> Option<usize> {
        self.hovering(rect).map(|at| (at.1 - rect.y) as usize)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> WorkbenchAction {
        self.drag = Drag::None;
        if self.transfer_input_active() {
            return self.transfer_key(key);
        }
        // Everything else here consumes, because a full-screen takeover that
        // leaked keys to the hidden composer would type into it. Copy is one
        // exception: with nothing selected there is nothing to take, and the
        // host spends that chord on quitting.
        if keys::COPY.matches(key) && self.selected_text().is_none() {
            return WorkbenchAction::Passthrough;
        }
        self.flash = None;
        // The name prompt and the close dialog answer even before the leader:
        // one is taking typing, the other is guarding unsaved work. They are
        // the only things here that do.
        if let Some(action) = self.input_key(key) {
            return action;
        }
        if let Some(action) = self.confirm_key(key) {
            return action;
        }
        if let Some(action) = self.menu_key(key) {
            return action;
        }
        // The leader is otherwise unconditional. Gating it on a selection left
        // every chord under it dead the moment one existed, and cut answers to
        // `Shift+Delete` now, so nothing here competes for the prefix.
        if keys::LEADER.matches(key) {
            return WorkbenchAction::Passthrough;
        }
        if let Some(action) = self.palette_key(key) {
            return action;
        }
        if let Some(action) = self.goto_key(key) {
            return action;
        }
        if let Some(action) = self.find_key(key) {
            return action;
        }
        if let Some(action) = self.global_key(key) {
            return action;
        }
        match self.focus {
            Focus::Sidebar => self.sidebar_key(key),
            Focus::Editor => self.editor_key(key),
        }
    }

    fn global_key(&mut self, key: KeyEvent) -> Option<WorkbenchAction> {
        if keys::CLOSE.matches(key) {
            if self
                .remote_scm
                .as_mut()
                .is_some_and(ScmDriver::cancel_mutation)
            {
                self.flash = Some("Cancelling remote source control operation…".to_owned());
                return Some(WorkbenchAction::Consumed);
            }
            // Everything else that owns `Esc` has already been offered it, so
            // this is the last thing between the key and leaving. A live
            // selection is the editor's own transient state and goes first.
            if self.focus == Focus::Editor
                && let Some(tab) = self.editor.active_mut()
                && tab.clear_selection()
            {
                return Some(WorkbenchAction::Consumed);
            }
            return Some(WorkbenchAction::Close);
        }
        if keys::TOGGLE_SIDEBAR.matches(key) {
            self.sidebar_collapsed = !self.sidebar_collapsed;
            if self.sidebar_collapsed {
                self.focus = Focus::Editor;
            }
            return Some(WorkbenchAction::Consumed);
        }
        if keys::QUICK_OPEN.matches(key) {
            self.quick_open();
            return Some(WorkbenchAction::Consumed);
        }
        if keys::REFRESH.matches(key) {
            if self.remote_backend.is_some() {
                // Only what the workspace serves can be asked for again. Anything
                // else, such as a host's document, would read as a path it no
                // longer lists and be closed.
                self.invalidate_remote(None);
                self.refresh_remote_tree();
            } else {
                self.tree.reload();
                self.scm.refresh();
                self.palette.invalidate();
                self.apply_marks();
            }
            return Some(WorkbenchAction::Consumed);
        }
        if keys::SAVE.matches(key) {
            return Some(self.save_active());
        }
        for (bind, delta) in [(keys::NEXT_TAB, 1), (keys::PREV_TAB, -1)] {
            if bind.matches(key) {
                self.cancel_pending_opens();
                self.editor.cycle(delta);
                self.reveal_active();
                return Some(WorkbenchAction::Consumed);
            }
        }
        self.clipboard_key(key).or_else(|| self.buffer_key(key))
    }

    fn quick_open(&mut self) {
        self.palette.set_priority(self.other_tabs());
        if self.remote_backend.is_some() {
            self.palette
                .open_remote(self.remote_entries.values().cloned().collect());
        } else {
            self.palette.open(&self.root, self.show_hidden);
        }
    }

    /// The key that followed `Ctrl+X`. The host owns the prefix and the pending
    /// state, so this only has to answer for the second half, and hands back
    /// `Passthrough` when the chord belongs to the transcript instead.
    pub fn handle_leader(&mut self, key: KeyEvent) -> WorkbenchAction {
        self.drag = Drag::None;
        self.flash = None;
        // A chord acts on the pane behind them, so they go first rather than
        // staying on screen over a workbench that has moved on.
        self.palette.close();
        self.goto = None;
        for (bind, view) in [
            (keys::VIEW_EXPLORER, SidebarView::Explorer),
            (keys::VIEW_SOURCE_CONTROL, SidebarView::SourceControl),
            (keys::VIEW_SEARCH, SidebarView::Search),
            (keys::VIEW_TRANSFER, SidebarView::Transfer),
        ] {
            if bind.matches(key) {
                if view != SidebarView::Search {
                    self.cancel_remote_search();
                }
                return self.transfer_switch(view);
            }
        }
        for (bind, step) in [
            (keys::SHRINK_SIDEBAR, -SIDEBAR_STEP),
            (keys::GROW_SIDEBAR, SIDEBAR_STEP),
        ] {
            if bind.matches(key) {
                self.set_sidebar_width(self.sidebar_width.saturating_add_signed(step));
                return WorkbenchAction::Consumed;
            }
        }
        if self.transfer_input_active() {
            return WorkbenchAction::Consumed;
        }
        if keys::SEND_TO_COMPOSER.matches(key) {
            return self.reference().unwrap_or(WorkbenchAction::Consumed);
        }
        if keys::CUT_CHORD.matches(key) {
            return self.cut();
        }
        if keys::MENU.matches(key) {
            self.open_menu();
            return WorkbenchAction::Consumed;
        }
        if keys::TOGGLE_HIDDEN.matches(key) {
            self.show_hidden = !self.show_hidden;
            self.tree.set_show_hidden(self.show_hidden);
            // The walk the palette cached was taken under the old answer, so
            // it would go on offering hidden files after they were turned off.
            self.palette.invalidate();
            return WorkbenchAction::Consumed;
        }
        if keys::CLOSE_TAB.matches(key) {
            self.close_tab();
            return WorkbenchAction::Consumed;
        }
        if keys::TOGGLE_WRAP.matches(key) {
            self.wrap = !self.wrap;
            // The caret was measured against the old shape of the pane, so it
            // is put back on screen before anything else reads the scroll.
            self.follow_cursor();
            return WorkbenchAction::Consumed;
        }
        if self.markdown.is_some() && keys::TOGGLE_RENDERED.matches(key) {
            self.toggle_rendered();
            return WorkbenchAction::Consumed;
        }
        let claimed = self.focus == Focus::Sidebar
            && match self.sidebar {
                SidebarView::SourceControl => self.scm_leader(key),
                SidebarView::Search => self.search_leader(key),
                SidebarView::Explorer | SidebarView::Transfer => false,
            };
        match claimed {
            true => WorkbenchAction::Consumed,
            false => WorkbenchAction::Passthrough,
        }
    }

    fn scm_leader(&mut self, key: KeyEvent) -> bool {
        let step = if keys::GROW_SECTION.matches(key) {
            SECTION_STEP
        } else if keys::SHRINK_SECTION.matches(key) {
            -SECTION_STEP
        } else {
            return false;
        };
        let section = self.scm.cursor().section;
        let height = self.scm.height(section).saturating_add_signed(step);
        self.scm.set_height(section, height);
        true
    }

    fn search_leader(&mut self, key: KeyEvent) -> bool {
        for (bind, toggle) in [
            (keys::NEXT_FIELD, Search::next_field as fn(&mut Search)),
            (keys::TOGGLE_CASE, Search::toggle_case),
            (keys::TOGGLE_WORD, Search::toggle_word),
            (keys::TOGGLE_REGEX, Search::toggle_regex),
        ] {
            if bind.matches(key) {
                toggle(&mut self.search);
                return true;
            }
        }
        false
    }

    /// Takes the selection out of the focused field, or else out of the
    /// buffer. With nothing selected it is inert rather than a forward delete,
    /// so a stray `Shift+Delete` never destroys a character the user could not
    /// see was at risk.
    fn cut(&mut self) -> WorkbenchAction {
        if let Some(field) = self.focused_field() {
            return self
                .field_key(field, keys::CUT.to_key_event())
                .unwrap_or(WorkbenchAction::Consumed);
        }
        if self.editor.active().is_some_and(Tab::is_rendered) {
            self.flash = Some(RENDERED_READ_ONLY.to_owned());
            return WorkbenchAction::Consumed;
        }
        let Some(text) = self.selected_text() else {
            return WorkbenchAction::Consumed;
        };
        self.clipboard = text.clone();
        if let Some(tab) = self.editor.active_mut()
            && tab.is_editable()
        {
            let edit = tab.buffer.delete();
            tab.record(edit);
            self.follow_cursor();
        }
        WorkbenchAction::Copy(text)
    }

    fn clipboard_key(&mut self, key: KeyEvent) -> Option<WorkbenchAction> {
        if keys::COPY.matches(key) {
            let text = self.selected_text()?;
            self.clipboard = text.clone();
            return Some(WorkbenchAction::Copy(text));
        }
        if keys::CUT.matches(key) {
            return Some(self.cut());
        }
        if keys::PASTE.matches(key) {
            let text = std::mem::take(&mut self.clipboard);
            let pasted = self.paste(&text);
            self.clipboard = text;
            return pasted.then_some(WorkbenchAction::Consumed);
        }
        None
    }

    fn buffer_key(&mut self, key: KeyEvent) -> Option<WorkbenchAction> {
        if self.editor.active().is_some_and(Tab::is_rendered)
            && SOURCE_CHORDS.iter().any(|bind| bind.matches(key))
        {
            self.editor.active_mut()?.show_source();
        }
        if keys::REVERT.matches(key) {
            // The only way out of a conflict that keeps the other writer's
            // work, so it throws the buffer away rather than merging.
            if let Some(document) = self.editor.active()?.document.clone() {
                return Some(WorkbenchAction::RevertDocument(document));
            }
            if self.remote_backend.is_some() {
                let path = self.editor.active()?.path.clone();
                self.request_remote_path(path, OpenPurpose::Discard);
                return Some(WorkbenchAction::Consumed);
            }
            let outcome = self.editor.active_mut()?.discard_and_reload();
            self.flash = outcome.err().map(|error| error.to_string());
            self.follow_cursor();
            return Some(WorkbenchAction::Consumed);
        }
        if keys::FIND.matches(key) {
            let tab = self.editor.active_mut()?;
            tab.find.open();
            tab.search_find();
            self.focus = Focus::Editor;
            return Some(WorkbenchAction::Consumed);
        }
        for (bind, delta) in [(keys::FIND_NEXT, 1), (keys::FIND_PREV, -1)] {
            if bind.matches(key) {
                let tab = self.editor.active_mut()?;
                // Closing the bar drops the matches, so stepping from a closed
                // one has to rescan before there is anything left to step to.
                if tab.find.current().is_none() {
                    tab.search_find();
                }
                if let Some(found) = tab.find.step(delta) {
                    tab.buffer.set_cursor(found.cursor(), false);
                    self.follow_cursor();
                }
                return Some(WorkbenchAction::Consumed);
            }
        }
        if keys::GOTO_LINE.matches(key) {
            self.editor.active()?;
            self.goto = Some(TextField::new(FieldKind::Line));
            self.focus = Focus::Editor;
            return Some(WorkbenchAction::Consumed);
        }
        None
    }

    /// The palette is modal while it is up: it is one field over a list. The
    /// list takes its own keys and the field every key it decodes. The
    /// workbench's chords still answer and act on the active tab as they
    /// would without the palette, but no other key reaches the panes behind.
    fn palette_key(&mut self, key: KeyEvent) -> Option<WorkbenchAction> {
        if !self.palette.is_open() {
            return None;
        }
        let page = self.palette_rows().max(1) as isize;
        match key.code {
            KeyCode::Esc => self.palette.close(),
            KeyCode::Enter => {
                let remote = self.palette.selected_remote();
                let chosen = self
                    .remote_backend
                    .is_none()
                    .then(|| self.palette.selected(&self.root))
                    .flatten();
                self.palette.close();
                if let Some(entry) = remote {
                    self.request_remote_open(entry, OpenPurpose::Open);
                } else if self.remote_backend.is_none()
                    && let Some(path) = chosen
                {
                    self.open_path(&path);
                }
            }
            KeyCode::Up => self.palette.move_selection(-1),
            KeyCode::Down => self.palette.move_selection(1),
            KeyCode::PageUp => self.palette.move_selection(-page),
            KeyCode::PageDown => self.palette.move_selection(page),
            _ if keys::LIST_FIRST.matches(key) => self.palette.select_first(),
            _ if keys::LIST_LAST.matches(key) => self.palette.select_last(),
            _ => {
                return self
                    .field_key(FocusedField::Palette, key)
                    .or_else(|| self.global_key(key))
                    .or(Some(WorkbenchAction::Consumed));
            }
        }
        Some(WorkbenchAction::Consumed)
    }

    /// The unsaved-changes dialog owns every key while it is up, and is asked
    /// first so `Esc` answers it rather than closing the workbench out from
    /// under the question.
    fn confirm_key(&mut self, key: KeyEvent) -> Option<WorkbenchAction> {
        let confirm = self.confirm?;
        match key.code {
            KeyCode::Esc => self.confirm = None,
            KeyCode::Left | KeyCode::BackTab => self.confirm = Some(confirm.step(-1)),
            KeyCode::Right | KeyCode::Tab => self.confirm = Some(confirm.step(1)),
            KeyCode::Enter => return Some(self.resolve(confirm.ask, confirm.choice)),
            // Modifiers are ruled out so `Ctrl+C` over a live selection cannot
            // be read as the `Cancel` accelerator.
            KeyCode::Char(typed) if key.modifiers == KeyModifiers::NONE => {
                if let Some(picked) = confirm
                    .ask
                    .answers()
                    .iter()
                    .find(|choice| choice.accelerator() == typed.to_ascii_lowercase())
                {
                    return Some(self.resolve(confirm.ask, *picked));
                }
            }
            _ => {}
        }
        Some(WorkbenchAction::Consumed)
    }

    /// The context menu holds every key while it is up, for the same reason
    /// the dialog does: it is a question, and the pane behind it is not
    /// listening.
    fn menu_key(&mut self, key: KeyEvent) -> Option<WorkbenchAction> {
        let menu = self.menu.as_mut()?;
        match key.code {
            KeyCode::Esc => self.menu = None,
            KeyCode::Up => menu.step(-1),
            KeyCode::Down => menu.step(1),
            KeyCode::Home => menu.select_first(),
            KeyCode::End => menu.select_last(),
            KeyCode::Enter => return Some(self.take_menu()),
            _ => {}
        }
        Some(WorkbenchAction::Consumed)
    }

    /// The name prompt is modal while it is up: it is one field, and the keys
    /// it does not take would otherwise reach the tree behind it.
    fn input_key(&mut self, key: KeyEvent) -> Option<WorkbenchAction> {
        self.input.as_ref()?;
        match key.code {
            KeyCode::Esc => self.input = None,
            KeyCode::Enter => self.commit_input(),
            _ => return self.modal_field_key(FocusedField::Name, key),
        }
        Some(WorkbenchAction::Consumed)
    }

    /// The go-to-line prompt is modal while it is up: it is one field, it takes
    /// digits, and every other key would otherwise edit the buffer behind it.
    fn goto_key(&mut self, key: KeyEvent) -> Option<WorkbenchAction> {
        self.goto.as_ref()?;
        match key.code {
            KeyCode::Esc => self.goto = None,
            KeyCode::Enter => {
                if let Ok(line) = self.goto.take()?.text().parse::<usize>() {
                    if let Some(tab) = self.editor.active_mut() {
                        tab.buffer.goto_line(line);
                    }
                    self.follow_cursor();
                }
            }
            _ if types_non_digit(key) => {}
            _ => return self.modal_field_key(FocusedField::Goto, key),
        }
        Some(WorkbenchAction::Consumed)
    }

    /// The find bar owns typing while it is open, and hands the buffer back on
    /// Esc. The workbench's chords still answer, so saving works, but a key
    /// its one-line field declines never reaches the document keymap behind
    /// it, where `Tab` or `Ctrl+J` would edit the file.
    fn find_key(&mut self, key: KeyEvent) -> Option<WorkbenchAction> {
        if self.focused_field() != Some(FocusedField::Find) {
            return None;
        }
        let tab = self.editor.active_mut()?;
        match key.code {
            KeyCode::Esc => tab.find.close(),
            KeyCode::Enter | KeyCode::Down | KeyCode::Up => {
                let back = key.code == KeyCode::Up || key.modifiers.contains(KeyModifiers::SHIFT);
                if let Some(found) = tab.find.step(if back { -1 } else { 1 }) {
                    tab.buffer.set_cursor(found.cursor(), false);
                    self.follow_cursor();
                }
            }
            _ => {
                return self
                    .field_key(FocusedField::Find, key)
                    .or_else(|| self.global_key(key))
                    .or(Some(WorkbenchAction::Consumed));
            }
        }
        Some(WorkbenchAction::Consumed)
    }

    /// Searches the buffer for the find bar's query as it now reads, from the
    /// caret, and lands on the first match.
    fn find_again(&mut self) {
        let Some(tab) = self.editor.active_mut() else {
            return;
        };
        tab.search_find();
        if let Some(found) = tab.find.current() {
            tab.buffer.set_cursor(found.cursor(), false);
        }
        self.follow_cursor();
    }

    /// The field keys and pastes reach, if one is up. Asked in the order
    /// [`Self::handle_key`] offers a key around, so it names the field the
    /// next key lands in.
    fn focused_field(&self) -> Option<FocusedField> {
        if self.input.is_some() {
            return Some(FocusedField::Name);
        }
        if self.confirm.is_some() || self.menu.is_some() {
            return None;
        }
        if self.palette.is_open() {
            return Some(FocusedField::Palette);
        }
        if self.goto.is_some() {
            return Some(FocusedField::Goto);
        }
        match self.focus {
            Focus::Editor => self
                .editor
                .active()
                .is_some_and(|tab| tab.find.is_open())
                .then_some(FocusedField::Find),
            Focus::Sidebar => (self.sidebar == SidebarView::Search).then_some(FocusedField::Search),
        }
    }

    fn field(&self, field: FocusedField) -> Option<&TextField> {
        match field {
            FocusedField::Name => self.input.as_ref().map(|input| &input.value),
            FocusedField::Palette => Some(self.palette.query()),
            FocusedField::Goto => self.goto.as_ref(),
            FocusedField::Find => self.editor.active().map(|tab| tab.find.query()),
            FocusedField::Search => Some(self.search.input(self.search.field())),
        }
    }

    fn field_mut(&mut self, field: FocusedField) -> Option<&mut TextField> {
        match field {
            FocusedField::Name => self.input.as_mut().map(|input| &mut input.value),
            FocusedField::Palette => Some(self.palette.query_mut()),
            FocusedField::Goto => self.goto.as_mut(),
            FocusedField::Find => self.editor.active_mut().map(|tab| tab.find.query_mut()),
            FocusedField::Search => {
                let typing = self.search.field();
                Some(self.search.input_mut(typing))
            }
        }
    }

    /// Hands `key` to `field`. `None` when the field does not decode it, so
    /// the key goes on to whatever stands behind the field.
    fn field_key(&mut self, field: FocusedField, key: KeyEvent) -> Option<WorkbenchAction> {
        let typed = self.field_mut(field)?.handle_key(key);
        self.typed(field, typed)
    }

    /// Hands `key` to a prompt that holds every key while it is up. The
    /// clipboard chords are the one way past it, since the internal paste
    /// lands in the field too.
    fn modal_field_key(&mut self, field: FocusedField, key: KeyEvent) -> Option<WorkbenchAction> {
        self.field_key(field, key)
            .or_else(|| self.clipboard_key(key))
            .or(Some(WorkbenchAction::Consumed))
    }

    /// Follows an edit to `field`: the palette refilters and the find bar
    /// searches again, and copied or cut text leaves for the clipboard.
    fn typed(&mut self, field: FocusedField, typed: TextKey) -> Option<WorkbenchAction> {
        if typed.changed() {
            match field {
                FocusedField::Palette => self.palette.rescan(),
                FocusedField::Find => self.find_again(),
                FocusedField::Name | FocusedField::Goto | FocusedField::Search => {}
            }
        }
        match typed {
            TextKey::Ignored => None,
            TextKey::Copy(text) | TextKey::Cut(text) => Some(self.copy(text)),
            TextKey::Changed | TextKey::Handled | TextKey::Refused => {
                Some(WorkbenchAction::Consumed)
            }
        }
    }

    fn sidebar_key(&mut self, key: KeyEvent) -> WorkbenchAction {
        if keys::FOCUS_NEXT.matches(key) || key.code == KeyCode::BackTab {
            self.focus = Focus::Editor;
            return WorkbenchAction::Consumed;
        }
        match self.sidebar {
            SidebarView::Explorer => self.explorer_key(key),
            SidebarView::SourceControl => self.source_control_key(key),
            SidebarView::Search => return self.search_key(key),
            SidebarView::Transfer => return self.transfer.key(key),
        }
        WorkbenchAction::Consumed
    }

    fn explorer_key(&mut self, key: KeyEvent) {
        if keys::COLLAPSE_ALL.matches(key) {
            self.tree.collapse_all();
            return;
        }
        let page = self.sidebar_rows().max(1) as isize;
        match key.code {
            KeyCode::Up => self.tree.move_selection(-1),
            KeyCode::Down => self.tree.move_selection(1),
            KeyCode::PageUp => self.tree.move_selection(-page),
            KeyCode::PageDown => self.tree.move_selection(page),
            KeyCode::Home => self.tree.select_first(),
            KeyCode::End => self.tree.select_last(),
            KeyCode::Left => self.tree.collapse_or_parent(),
            KeyCode::Right | KeyCode::Enter => self.enter_selected(),
            _ => {}
        }
    }

    fn source_control_key(&mut self, key: KeyEvent) {
        if keys::DISCARD.matches(key) {
            self.revert_selected();
            return;
        }
        self.scm.disarm();
        if keys::STAGE_TOGGLE.matches(key) {
            self.stage_selected();
            return;
        }
        if keys::TOGGLE_TREE.matches(key) {
            self.scm.toggle_flat();
            return;
        }
        if keys::OPEN_DIFF.matches(key) {
            // On a commit this means `git show` rather than a second `Enter`:
            // the row has room for a subject and the tab has room for the rest.
            match self.scm.selected_commit().is_some() {
                true => self.open_commit_detail(),
                false => self.activate_scm(),
            }
            return;
        }
        let page = self.scm_rows().max(1) as isize;
        match key.code {
            KeyCode::Up => self.scm.move_cursor(-1),
            KeyCode::Down => self.scm.move_cursor(1),
            KeyCode::PageUp => self.scm.move_cursor(-page),
            KeyCode::PageDown => self.scm.move_cursor(page),
            KeyCode::Home => self.scm.select_first(),
            KeyCode::End => self.scm.select_last(),
            KeyCode::Left => self.scm.fold(),
            KeyCode::Right => {
                if !self.scm.unfold() {
                    self.expand_remote_commit();
                }
            }
            KeyCode::Enter => self.activate_scm(),
            _ => {}
        }
    }

    /// What `Enter` means in the pane: fold a section or a folder, and open
    /// anything else.
    fn activate_scm(&mut self) {
        if self.scm.toggle_fold() {
            return;
        }
        if self.expand_remote_commit() {
            return;
        }
        self.open_scm_selection();
    }

    fn expand_remote_commit(&mut self) -> bool {
        let Some(commit) = self.scm.selected_commit().cloned() else {
            return false;
        };
        let Some(parent) = commit.parents.first() else {
            self.scm
                .open_workspace_commit(&commit.id, scm::repo::CommitFiles::default());
            return true;
        };
        let Some(repository) = self.scm.workspace_repository().cloned() else {
            return false;
        };
        let Ok(commit_id) = ScmRevision::new(commit.id) else {
            return false;
        };
        let Ok(parent_id) = ScmRevision::new(parent.clone()) else {
            return false;
        };
        let Some(driver) = &mut self.remote_scm else {
            return false;
        };
        driver.commit_files(&repository, commit_id, parent_id);
        true
    }

    /// Opens whatever the cursor is on as a read-only tab: a diff for a change,
    /// and a diff for one path of a commit in the graph.
    fn open_scm_selection(&mut self) {
        match self.scm.cursor().section {
            Section::Graph => self.open_commit_file(),
            _ => self.open_diff(),
        }
    }

    /// The search pane is a form over a list: the field holding the caret
    /// takes every key it decodes, so the list moves on the vertical keys and
    /// every command is a `Ctrl+X` chord handled in
    /// [`Workbench::handle_leader`].
    fn search_key(&mut self, key: KeyEvent) -> WorkbenchAction {
        let page = self.sidebar_rows().max(1) as isize;
        match key.code {
            KeyCode::Up => self.search.move_selection(-1),
            KeyCode::Down => self.search.move_selection(1),
            KeyCode::PageUp => self.search.move_selection(-page),
            KeyCode::PageDown => self.search.move_selection(page),
            KeyCode::Enter => self.run_or_open_search(),
            _ if keys::LIST_FIRST.matches(key) => self.search.select_first(),
            _ if keys::LIST_LAST.matches(key) => self.search.select_last(),
            _ => {
                return self
                    .field_key(FocusedField::Search, key)
                    .unwrap_or(WorkbenchAction::Consumed);
            }
        }
        WorkbenchAction::Consumed
    }

    /// Enter means "search" while the fields have moved on from the results,
    /// and "open what I am looking at" once they agree.
    fn run_or_open_search(&mut self) {
        if self.search.is_stale() {
            if self.remote_backend.is_some() {
                if let Some(previous) = self.search.cancel_remote()
                    && let Some(backend) = &mut self.remote_backend
                {
                    backend.cancel(previous);
                    self.remote_pending.remove(&previous);
                }
                if let Some((query, include)) = self.search.prepare_remote()
                    && let Some(backend) = &mut self.remote_backend
                {
                    let request = backend.search(query, include);
                    self.remote_pending.insert(request);
                    self.search.begin_remote(request);
                }
            } else {
                self.search.start(&self.root, self.show_hidden);
            }
            self.flash = self.search.error().map(str::to_owned);
            return;
        }
        self.open_search_selection();
    }

    fn open_search_selection(&mut self) {
        let Some((path, line)) = self.search.selection() else {
            return;
        };
        if self.remote_backend.is_some() {
            self.pending_lines.insert(path.clone(), line..=line);
            if let Some(entry) = self.search.selected_resource() {
                self.cancel_pending_opens();
                self.request_remote_open(entry, OpenPurpose::Open);
                self.follow_cursor();
                return;
            }
        }
        self.open_workbench_path(&path, OpenPurpose::Open);
        if self.remote_backend.is_none()
            && let Some(tab) = self.editor.active_mut()
        {
            tab.buffer.goto_line(line);
        }
        self.follow_cursor();
    }

    fn stage_selected(&mut self) {
        if self.remote_scm.is_some() {
            let paths = match self.scm.workspace_paths() {
                Ok(paths) => paths,
                Err(error) => {
                    self.flash = Some(error.to_string());
                    return;
                }
            };
            if paths.is_empty() {
                return;
            }
            let mutation = match self.scm.cursor().section {
                Section::Staged => ScmMutation::Unstage { paths },
                Section::Unstaged => ScmMutation::Stage { paths },
                Section::Graph => return,
            };
            self.run_remote_mutation(mutation);
            return;
        }
        if let Err(error) = self.scm.stage() {
            self.flash = Some(error.to_string());
            return;
        }
        self.apply_marks();
    }

    /// One file is armed on the row and confirmed there. A folder or a whole
    /// section is asked about instead, because the row says how many paths it
    /// covers but not what is in them, and git keeps no copy of what a discard
    /// throws away.
    fn revert_selected(&mut self) {
        if self.scm.selected_change().is_some() {
            self.discard_change();
            return;
        }
        self.scm.disarm();
        // Only where the row paints the control. A staged folder has an index
        // entry standing between its files and what a discard would restore,
        // so it offers unstaging instead.
        if self.scm.cursor().section != Section::Unstaged || self.scm.scope_len() == 0 {
            return;
        }
        self.confirm = Some(Confirm {
            ask: Ask::Revert,
            choice: Choice::Cancel,
        });
    }

    fn discard_change(&mut self) {
        if self.remote_scm.is_some() {
            let relative = match self.scm.discard() {
                Ok(scm::Discard::Nothing) => return,
                Ok(scm::Discard::Armed(relative)) => {
                    self.flash = Some(format!(
                        "Discard changes to {relative}? Press {} or click {} to confirm.",
                        keys::DISCARD.label,
                        view::REVERT_MARK,
                    ));
                    return;
                }
                Ok(scm::Discard::Done(relative)) => relative,
                Err(error) => {
                    self.flash = Some(error.to_string());
                    return;
                }
            };
            let Ok(path) = WorkspacePath::new(relative) else {
                return;
            };
            if self
                .scm
                .discardable_workspace_paths()
                .is_ok_and(|paths| paths.contains(&path))
            {
                self.run_remote_mutation(ScmMutation::Discard { paths: vec![path] });
            } else {
                self.flash = Some("Untracked files cannot be discarded remotely".to_owned());
            }
            return;
        }
        match self.scm.discard() {
            Ok(scm::Discard::Nothing) => {}
            Ok(scm::Discard::Armed(relative)) => {
                self.flash = Some(format!(
                    "Discard changes to {relative}? Press {} or click {} to confirm.",
                    keys::DISCARD.label,
                    view::REVERT_MARK,
                ));
            }
            Ok(scm::Discard::Done(relative)) => {
                self.reload_tabs_for(&relative);
                self.apply_marks();
            }
            Err(error) => self.flash = Some(error.to_string()),
        }
    }

    /// Opens the selected change as a read-only diff tab, which inherits the
    /// editor's scrolling, find and selection rather than reimplementing them.
    fn open_diff(&mut self) {
        if self.remote_scm.is_some() {
            self.open_remote_change_diff();
            return;
        }
        let diff = match self.scm.selected_diff() {
            Ok(Some(diff)) => diff,
            Ok(None) => return,
            Err(error) => {
                self.flash = Some(error.to_string());
                return;
            }
        };
        let (change, rendered) = diff;
        let side = if change.staged { STAGED } else { WORKING };
        let title = format!("{} \u{2194} {side}", self.relative(&change.path).display());
        self.drag = Drag::None;
        self.editor.push(Tab::synthetic(
            &change.path,
            title,
            rendered.rows,
            self.theme_generation,
        ));
        self.focus = Focus::Editor;
        // A diff names a real file, so the explorer has somewhere to go even
        // though the pane it was opened from is the one already in place.
        self.reveal_active();
    }

    /// Opens the commit under the cursor as a read-only tab: everything the
    /// graph row had no width to say, which is the whole message.
    ///
    /// The tab is filed under the hash alone, the directory the commit's file
    /// tabs already hang beneath, so it displaces none of them.
    fn open_commit_detail(&mut self) {
        let Some(commit) = self.scm.selected_commit().cloned() else {
            return;
        };
        // Listing what a commit changed is what opening it means here, so the
        // detail tab and the graph read it once between them. A local
        // repository answers in this breath; a workspace session draws the tab
        // now and again when the walk lands.
        if self.scm.commit_files(&commit.id).is_none() {
            match self.remote_scm.is_some() {
                true => {
                    self.pending_commit_detail = Some(commit.id.clone());
                    self.expand_remote_commit();
                }
                false => self.scm.expand_commit(&commit.id),
            }
        }
        self.push_commit_detail(&commit);
    }

    fn push_commit_detail(&mut self, commit: &Commit) {
        let rendered = scm::commit::detail(commit, self.scm.commit_files(&commit.id));
        let Some(path) = self.commit_detail_path(&commit.id) else {
            return;
        };
        self.drag = Drag::None;
        self.editor.push(Tab::synthetic_backend(
            path,
            format!("{}{}", scm::commit::TITLE, commit.id),
            rendered.rows,
            self.theme_generation,
        ));
        self.focus = Focus::Editor;
    }

    /// Where a commit's detail tab is filed: the hash alone, which is the
    /// directory its file tabs already hang beneath, so the two never displace
    /// each other in [`Editor::push`].
    fn commit_detail_path(&self, id: &str) -> Option<WorkbenchPath> {
        match self.scm.workdir() {
            Some(workdir) => Some(WorkbenchPath::Local(workdir.join(id))),
            None => WorkspacePath::new(format!(
                "{SCM_SYNTHETIC_ROOT}/{}",
                opaque_path_component(id)
            ))
            .ok()
            .map(WorkbenchPath::Remote),
        }
    }

    /// Opens one path of a commit as a read-only diff tab.
    ///
    /// The tab is filed under the commit's own hash rather than the worktree
    /// path, so a commit's view of a file and the working tree's are two tabs:
    /// [`Editor::push`] keeps one tab per path. The name still ends in the real
    /// one, which is what the highlighter reads the language from.
    fn open_commit_file(&mut self) {
        if self.remote_scm.is_some() {
            self.open_remote_commit_diff();
            return;
        }
        let opened = match self.scm.selected_commit_file_diff() {
            Ok(Some(opened)) => opened,
            Ok(None) => return,
            Err(error) => {
                self.flash = Some(error.to_string());
                return;
            }
        };
        let Some(workdir) = self.scm.workdir().map(Path::to_path_buf) else {
            return;
        };
        let (commit, file, rendered) = opened;
        let title = format!("{} \u{2194} {}", file.relative, commit.id);
        let path = workdir.join(&commit.id).join(&file.relative);
        self.drag = Drag::None;
        self.editor.push(Tab::synthetic(
            &path,
            title,
            rendered.rows,
            self.theme_generation,
        ));
        self.focus = Focus::Editor;
    }

    fn run_remote_mutation(&mut self, mutation: ScmMutation) {
        let Some(repository) = self.scm.workspace_repository().cloned() else {
            return;
        };
        let Some(driver) = &mut self.remote_scm else {
            return;
        };
        if let Err(error) = driver.mutate(&repository, mutation) {
            self.flash = Some(error.to_string());
        }
    }

    fn open_remote_change_diff(&mut self) {
        let Some(change) = self.scm.selected_change().cloned() else {
            return;
        };
        let Some(repository) = self.scm.workspace_repository().cloned() else {
            return;
        };
        let Ok(path) = WorkspacePath::new(change.relative) else {
            return;
        };
        let (target, old, new) = if change.staged {
            (ScmDiffTarget::Staged, ScmSide::Head, ScmSide::Index)
        } else {
            (ScmDiffTarget::Unstaged, ScmSide::Index, ScmSide::Worktree)
        };
        if let Some(driver) = &mut self.remote_scm {
            driver.diff(&repository, path, target, old, new);
        }
    }

    fn open_remote_commit_diff(&mut self) {
        let Some((commit, file)) = self
            .scm
            .selected_commit_file()
            .map(|(commit, file)| (commit.clone(), file.clone()))
        else {
            return;
        };
        let Some(parent) = commit.parents.first() else {
            return;
        };
        let Some(repository) = self.scm.workspace_repository().cloned() else {
            return;
        };
        let (Ok(path), Ok(base), Ok(target)) = (
            WorkspacePath::new(file.relative),
            ScmRevision::new(parent.clone()),
            ScmRevision::new(commit.id),
        ) else {
            return;
        };
        let diff_target = ScmDiffTarget::Tree {
            base: base.clone(),
            target: target.clone(),
        };
        if let Some(driver) = &mut self.remote_scm {
            driver.diff(
                &repository,
                path,
                diff_target,
                ScmSide::Commit { revision: base },
                ScmSide::Commit { revision: target },
            );
        }
    }

    fn open_remote_diff(&mut self, result: RemoteDiffResult) {
        let RemoteDiffResult {
            path,
            target,
            lines,
            old,
            new,
            warnings,
        } = result;
        let rendered =
            scm::diff::structured(path.as_str(), &old, &new, lines, !warnings.is_empty());
        let (title, tab_path) = match target {
            ScmDiffTarget::Staged => (
                format!("{path} ↔ {STAGED}"),
                WorkbenchPath::Remote(path.clone()),
            ),
            ScmDiffTarget::Unstaged => (
                format!("{path} ↔ {WORKING}"),
                WorkbenchPath::Remote(path.clone()),
            ),
            ScmDiffTarget::Tree { target, .. } => {
                let synthetic = WorkspacePath::new(format!(
                    "{SCM_SYNTHETIC_ROOT}/{}/{}",
                    opaque_path_component(target.as_str()),
                    path.as_str()
                ))
                .unwrap_or_else(|_| path.clone());
                (
                    format!("{path} ↔ {}", target.as_str()),
                    WorkbenchPath::Remote(synthetic),
                )
            }
        };
        if let Some(warning) = warnings.last() {
            self.flash = Some(warning.clone());
        }
        self.drag = Drag::None;
        self.editor.push(Tab::synthetic_backend(
            tab_path,
            title,
            rendered.rows,
            self.theme_generation,
        ));
        self.focus = Focus::Editor;
    }

    fn apply_remote_commit_files(&mut self, result: CommitFilesResult) {
        let mut paths = std::collections::BTreeMap::new();
        for line in result.lines {
            let mark = line
                .change
                .map(scm::workspace_change_mark)
                .unwrap_or(fs::tree::GitMark::Modified);
            paths.entry(line.path.to_string()).or_insert(mark);
        }
        let files = scm::repo::CommitFiles {
            files: paths
                .into_iter()
                .map(|(relative, mark)| scm::repo::CommitPath { relative, mark })
                .collect(),
            truncated: result.truncated || result.incomplete,
        };
        self.scm
            .open_workspace_commit(result.commit.as_str(), files);
        self.redraw_pending_commit_detail(result.commit.as_str());
        if result.truncated || result.incomplete {
            self.flash = Some("Remote commit file list is incomplete".to_owned());
        }
    }

    /// A detail tab opened before its file list arrived is drawn again now that
    /// it has, so the reader ends up with the same tab either way.
    fn redraw_pending_commit_detail(&mut self, id: &str) {
        if self.pending_commit_detail.as_deref() != Some(id) {
            return;
        }
        self.pending_commit_detail = None;
        let Some(commit) = self
            .scm
            .log()
            .iter()
            .find(|commit| commit.id == id)
            .cloned()
        else {
            return;
        };
        self.push_commit_detail(&commit);
    }

    /// A discard rewrote a file underneath whatever was showing it, so any tab
    /// on that path has to catch up or say that it cannot.
    fn reload_tabs_for(&mut self, relative: &str) {
        let Some(path) = self.scm.workdir().map(|workdir| workdir.join(relative)) else {
            return;
        };
        self.reload_tab(&path);
    }

    /// Something else wrote `path`. A clean tab takes the new contents; a dirty
    /// one keeps what was typed and flies the conflict, because throwing away
    /// unsaved work is the user's call and not the watcher's.
    fn reload_tab(&mut self, path: &Path) {
        let path = WorkbenchPath::Local(path.to_path_buf());
        for tab in self.editor.tabs_mut() {
            if tab.path == path && tab.is_editable() && tab.reload_from_disk().is_err() {
                tab.conflict = true;
            }
        }
    }

    fn apply_marks(&mut self) {
        if self.remote_scm.is_some() {
            let marks = |path: &WorkspacePath| self.scm.workspace_mark(path);
            let under = |path: &WorkspacePath| self.scm.workspace_folder_mark(path);
            self.tree.apply_remote_git(&marks, &under);
            self.tree.set_agent_touched(&self.touched);
            return;
        }
        let marks = self.scm.marks();
        let under = self.scm.folder_marks();
        self.tree.apply_git(&marks, &under);
        self.tree.set_agent_touched(&self.touched);
    }

    /// Right and Enter mean the same thing on a row: step into the directory,
    /// or open the file.
    fn enter_selected(&mut self) {
        if !self.tree.toggle_selected() {
            self.open_selected();
        }
    }

    fn editor_key(&mut self, key: KeyEvent) -> WorkbenchAction {
        let rows = self.panes.text.height as usize;
        let Some(tab) = self.editor.active_mut() else {
            return WorkbenchAction::Consumed;
        };
        if keys::SELECT_ALL.matches(key)
            && let Some(paint) = self.markdown
        {
            let (text, _) = view::scroll_column(self.scrollbars, self.panes.editor, usize::MAX);
            tab.rendered(text.width, self.theme_generation, paint);
        }
        if let Some(view) = tab.rendered_mut() {
            if keys::SELECT_ALL.matches(key) {
                view.select_all();
            } else if !view.scroll_key(key, rows) {
                self.flash = Some(RENDERED_READ_ONLY.to_owned());
            }
            return WorkbenchAction::Consumed;
        }
        let before = tab.revision();
        if tab.edit_key(key, rows) {
            if tab.revision() != before {
                tab.preview = false;
                if tab.find.is_open() {
                    tab.refresh_find();
                }
            }
            self.follow_cursor();
        }
        WorkbenchAction::Consumed
    }

    /// Whether `tab` can be shown rendered, which takes both Markdown in the
    /// tab and a painter from the host.
    fn renders(&self, tab: &Tab) -> bool {
        self.markdown.is_some() && tab.is_markdown()
    }

    fn toggle_rendered(&mut self) {
        self.drag = Drag::None;
        if !self.editor.active_mut().is_some_and(Tab::toggle_rendered) {
            self.flash = Some(NO_RENDERED_VIEW.to_owned());
        }
    }

    fn open_selected(&mut self) {
        let Some(path) = self.tree.selected().map(|row| row.path.clone()) else {
            return;
        };
        self.open_workbench_path(&path, OpenPurpose::Open);
    }

    /// Puts the selected file up without leaving the tree, which is what one
    /// click does. The cursor stays in the sidebar so the next arrow key walks
    /// on from where it was.
    fn preview_selected(&mut self) {
        let Some(path) = self.tree.selected().map(|row| row.path.clone()) else {
            return;
        };
        if self.remote_backend.is_some() {
            self.open_workbench_path(&path, OpenPurpose::Preview);
            return;
        }
        let Some(path) = path.local() else {
            self.flash = Some(BackendError::WrongBackend.to_string());
            return;
        };
        match self.editor.preview(path, self.theme_generation) {
            Ok(()) => {
                // The tree is already on this row, but source control is not.
                self.reveal_active();
                self.follow_cursor();
            }
            Err(error) => self.flash = Some(error.to_string()),
        }
    }

    fn open_path(&mut self, path: &Path) {
        self.open_workbench_path(&WorkbenchPath::Local(path.to_path_buf()), OpenPurpose::Open);
    }

    fn open_workbench_path(&mut self, path: &WorkbenchPath, purpose: OpenPurpose) {
        if self.remote_backend.is_some() {
            if matches!(purpose, OpenPurpose::Open | OpenPurpose::Preview) {
                self.cancel_pending_opens();
            }
            self.request_remote_path(path.clone(), purpose);
            return;
        }
        let Some(path) = path.local() else {
            self.flash = Some(BackendError::WrongBackend.to_string());
            return;
        };
        let started = Instant::now();
        let mut phase_start = started;
        let mut lap = || {
            let elapsed = phase_start.elapsed().as_millis() as u64;
            phase_start = Instant::now();
            elapsed
        };
        match self.editor.open(path, self.theme_generation) {
            Ok(()) => {
                let open_ms = lap();
                self.focus = Focus::Editor;
                self.reveal_active();
                let reveal_ms = lap();
                self.follow_cursor();
                tracing::info!(
                    open_ms,
                    reveal_ms,
                    follow_ms = lap(),
                    total_ms = started.elapsed().as_millis() as u64,
                    "workbench file opened"
                );
            }
            Err(error) => self.flash = Some(error.to_string()),
        }
    }

    fn close_tab(&mut self) {
        self.close_at(self.editor.active_index());
    }

    /// Takes the dialog down and acts on the answer it was given.
    fn resolve(&mut self, ask: Ask, choice: Choice) -> WorkbenchAction {
        self.confirm = None;
        match ask {
            Ask::Close => return self.resolve_close(choice),
            Ask::Revert => self.resolve_revert(choice),
            Ask::Delete(_) => self.resolve_delete(choice),
        }
        WorkbenchAction::Consumed
    }

    /// The path is the tree's own selection, which the menu landed on before
    /// it opened and the dialog has held still ever since.
    fn resolve_delete(&mut self, choice: Choice) {
        if choice != Choice::Discard {
            self.delete_target = None;
            return;
        }
        if let Some(entry) = self.delete_target.take() {
            let entry = self.mutation_resource(&entry.path).unwrap_or(entry);
            if let Some(backend) = &mut self.remote_backend {
                let request = backend.delete(entry);
                self.remote_pending.insert(request);
            }
            return;
        }
        let Some(path) = self.tree.selected().map(|row| row.path.clone()) else {
            return;
        };
        let Some(local) = path.local() else {
            self.flash = Some(BackendError::WrongBackend.to_string());
            return;
        };
        match ops::delete(local) {
            Ok(()) => {
                self.close_tabs_under(&path);
                self.reread();
            }
            Err(error) => self.flash = Some(error.to_string()),
        }
    }

    /// A path that is gone takes its tabs with it. A tab with unsaved edits
    /// stays and flies the conflict instead, because throwing that work away
    /// is not part of what was asked.
    fn close_tabs_under(&mut self, path: &WorkbenchPath) {
        for tab in self.editor.tabs_mut() {
            if tab.path.starts_with(path) && tab.is_dirty() {
                tab.conflict = true;
            }
        }
        self.editor
            .close_where(&|tab| tab.path.starts_with(path) && !tab.is_dirty());
        self.reveal_active();
    }

    fn mutation_resource(&self, path: &WorkbenchPath) -> Option<ResourceEntry> {
        self.editor
            .tabs()
            .iter()
            .find(|tab| &tab.path == path)
            .and_then(|tab| tab.resource.clone())
            .or_else(|| self.remote_entries.get(path).cloned())
    }

    fn mark_remote_conflict(&mut self, path: &WorkbenchPath) {
        for tab in self.editor.tabs_mut() {
            if tab.path.starts_with(path) {
                tab.conflict = true;
            }
        }
    }

    /// A save that fails keeps the tab open with the reason in the status row,
    /// because throwing the buffer away after failing to write it is the one
    /// outcome nobody asked for. So does one still waiting on its answer.
    fn resolve_close(&mut self, choice: Choice) -> WorkbenchAction {
        let (closing, action) = match choice {
            Choice::Cancel => (false, WorkbenchAction::Consumed),
            Choice::Save => {
                let action = self.save_active();
                (!self.editor.active().is_some_and(Tab::is_dirty), action)
            }
            Choice::Discard => (true, WorkbenchAction::Consumed),
        };
        if !closing {
            // Whatever stopped this tab stops the batch behind it: carrying on
            // would ask the same question about the next tab and read this
            // answer as covering that one too.
            self.closing.clear();
            return action;
        }
        self.editor.close_active();
        self.reveal_active();
        self.close_next();
        action
    }

    fn resolve_revert(&mut self, choice: Choice) {
        if choice != Choice::Discard {
            return;
        }
        if self.remote_scm.is_some() {
            match self.scm.discardable_workspace_paths() {
                Ok(paths) if !paths.is_empty() => {
                    self.run_remote_mutation(ScmMutation::Discard { paths });
                }
                Ok(_) => {
                    self.flash = Some("Untracked files cannot be discarded remotely".to_owned());
                }
                Err(error) => self.flash = Some(error.to_string()),
            }
            return;
        }
        match self.scm.discard_scope() {
            Ok(paths) => {
                for relative in &paths {
                    self.reload_tabs_for(relative);
                }
                self.apply_marks();
            }
            Err(error) => self.flash = Some(error.to_string()),
        }
    }

    /// Puts the tree back on screen. A revealed row is no reveal at all while
    /// the sidebar is collapsed or showing source control.
    fn show_explorer(&mut self) {
        self.sidebar = SidebarView::Explorer;
        self.sidebar_collapsed = false;
    }

    /// Opens source control on the commit `id` abbreviates, which is where a
    /// click on a `#hash` in the transcript lands. A commit the graph is not
    /// showing says so rather than opening the pane on something else.
    pub fn open_at_commit(&mut self, root: &Path, id: &str) {
        if !self.open || self.root != root {
            self.open(root);
        }
        self.sidebar = SidebarView::SourceControl;
        self.sidebar_collapsed = false;
        if !self.scm.reveal_commit(id) {
            self.flash = Some(format!("{id} is not in the recent log"));
        }
    }

    /// Points both sidebars at whatever the editor is showing: the explorer
    /// expands down to the file, and source control lands on its change when it
    /// has one.
    ///
    /// Neither pane is brought to the front. Which sidebar is up is the
    /// reader's choice, and opening a tab is not a reason to overrule it; the
    /// point is that the pane they do go back to is already in the right place.
    fn reveal_active(&mut self) {
        self.drag = Drag::None;
        let Some(path) = self.editor.active().map(|tab| tab.path.clone()) else {
            return;
        };
        self.tree.reveal_workbench_path(&path);
        // Change paths are relative to the repository, which is not the
        // workbench root when the workbench was opened below it.
        let relative = path.local().and_then(|path| {
            self.scm
                .workdir()
                .and_then(|workdir| path.strip_prefix(workdir).ok())
                .map(|relative| relative.to_string_lossy().into_owned())
        });
        if let Some(relative) = relative {
            self.scm.reveal(&relative);
        }
    }

    /// Writes the active tab, or hands a host's document back to the host to
    /// keep. Only a local write lands at once; the tab stays unsaved until the
    /// rest are answered, which is what the unsaved-changes dialog reads to
    /// know whether closing it is safe yet.
    fn save_active(&mut self) -> WorkbenchAction {
        let Some(tab) = self.editor.active_mut() else {
            return WorkbenchAction::Consumed;
        };
        if let Some(action) = hand_back(tab, false) {
            return action;
        }
        if tab.path.remote().is_some() {
            let Some(entry) = tab.resource.clone() else {
                self.flash = Some(BackendError::MissingRevision.to_string());
                return WorkbenchAction::Consumed;
            };
            let path = tab.path.clone();
            let contents = tab.contents();
            let Some(backend) = &mut self.remote_backend else {
                self.flash = Some(BackendError::WrongBackend.to_string());
                return WorkbenchAction::Consumed;
            };
            let request = backend.save(entry, contents);
            self.remote_pending.insert(request);
            self.pending_save.insert(request, path);
            return WorkbenchAction::Consumed;
        }
        if let Err(error) = tab.save() {
            self.flash = Some(error.to_string());
        }
        WorkbenchAction::Consumed
    }

    fn active_title(&self) -> String {
        self.editor
            .active()
            .map(|tab| tab.heading().to_owned())
            .unwrap_or_default()
    }

    /// The focused field's selection, or else the buffer's.
    fn selected_text(&self) -> Option<String> {
        self.focused_field()
            .and_then(|field| self.field(field)?.selected_text())
            .or_else(|| self.editor.active()?.selected_text())
    }

    /// How a path leaves for the composer, which is the same wherever the path
    /// came from.
    fn mention(
        &self,
        path: &WorkbenchPath,
        lines: Option<RangeInclusive<usize>>,
    ) -> WorkbenchAction {
        WorkbenchAction::SendToComposer {
            path: match path {
                WorkbenchPath::Local(path) => {
                    WorkbenchPath::Local(self.relative(path).to_path_buf())
                }
                WorkbenchPath::Remote(path) => WorkbenchPath::Remote(path.clone()),
            },
            lines,
        }
    }

    /// What `Ctrl+X Enter` hands the composer: the tree's selection from the
    /// sidebar, and the cursor's line span from the editor. A host's document
    /// has no lines worth naming, so it goes back whole, to be kept and left.
    fn reference(&self) -> Option<WorkbenchAction> {
        if self.focus == Focus::Sidebar {
            if self.sidebar == SidebarView::Search {
                let (path, line) = self.search.selection()?;
                return Some(self.mention(&path, Some(line..=line)));
            }
            let path = match self.sidebar {
                SidebarView::SourceControl => {
                    return Some(self.mention(
                        &WorkbenchPath::Local(self.scm.selected_change()?.path.clone()),
                        None,
                    ));
                }
                _ => &self.tree.selected()?.path,
            };
            return Some(self.mention(path, None));
        }
        let tab = self.editor.active()?;
        if let Some(action) = hand_back(tab, true) {
            return Some(action);
        }
        // The rendered view has no caret, so it points at no line in the file.
        if tab.is_rendered() {
            return Some(self.mention(&tab.path, None));
        }
        let lines = match tab.buffer.selection() {
            Some((from, to)) => from.line + 1..=to.line + 1,
            None => {
                let line = tab.buffer.cursor().line + 1;
                line..=line
            }
        };
        Some(self.mention(&tab.path, Some(lines)))
    }

    /// The open tabs the palette lists first, most recently opened before the
    /// rest. The active one is left out: `Ctrl+P` then `Enter` is worth a
    /// keystroke only if it lands somewhere other than where the cursor is.
    fn other_tabs(&self) -> Vec<String> {
        let active = self.editor.active_index();
        self.editor
            .tabs()
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != active)
            .rev()
            .map(|(_, tab)| self.relative_path(&tab.path))
            .collect()
    }

    fn relative<'a>(&'a self, path: &'a Path) -> &'a Path {
        path.strip_prefix(&self.root).unwrap_or(path)
    }

    fn relative_path(&self, path: &WorkbenchPath) -> String {
        path.display_relative(&self.backend_root())
    }

    fn backend_root(&self) -> WorkbenchPath {
        self.remote_backend
            .as_ref()
            .map(|backend| backend.root().clone())
            .unwrap_or_else(|| WorkbenchPath::Local(self.root.clone()))
    }

    fn cancel_remote_search(&mut self) {
        let Some(request) = self.search.cancel_remote() else {
            return;
        };
        if let Some(backend) = &mut self.remote_backend {
            backend.cancel(request);
        }
        self.remote_pending.remove(&request);
    }

    fn follow_cursor(&mut self) {
        let (rows, columns) = (self.panes.text.height, self.panes.text.width);
        let wrap = self.wrap;
        if let Some(tab) = self.editor.active_mut() {
            tab.follow_cursor(rows as usize, columns as usize, wrap);
        }
    }

    fn palette_rows(&self) -> usize {
        self.panes.palette.height as usize
    }

    fn sidebar_rows(&self) -> usize {
        self.panes.rows.height as usize
    }

    /// The rows a page key moves by in the source control pane, which is the
    /// body of the section the cursor is in rather than the whole sidebar.
    fn scm_rows(&self) -> usize {
        let section = self.scm.cursor().section;
        let index = Section::ALL
            .iter()
            .position(|candidate| *candidate == section)
            .unwrap_or_default();
        self.panes.sections[index].body.height as usize
    }

    /// What the sidebar header's button does for the view that painted it.
    fn press_header_button(&mut self) {
        match self.sidebar {
            SidebarView::Explorer => self.tree.collapse_all(),
            SidebarView::SourceControl => self.scm.toggle_flat(),
            SidebarView::Search | SidebarView::Transfer => {}
        }
    }

    /// The layout clamps again against the room a frame actually has, so this
    /// only has to keep the remembered width sane.
    fn set_sidebar_width(&mut self, width: u16) {
        self.sidebar_width = width.clamp(MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH);
    }
}

/// Hands the text of a host's document back to the host, which is what saving
/// one means. `close` asks to go back to the transcript as well. A tab over a
/// file has nothing to hand back.
fn hand_back(tab: &Tab, close: bool) -> Option<WorkbenchAction> {
    Some(WorkbenchAction::SaveDocument {
        key: tab.document.clone()?,
        text: tab.contents(),
        close,
    })
}

/// Whether `key` types a character go-to-line has no use for.
fn types_non_digit(key: KeyEvent) -> bool {
    matches!(
        decode(key, FieldKind::Line),
        Some(TextCommand::Edit(EditCommand::Insert(typed))) if !typed.is_ascii_digit()
    )
}

fn opaque_path_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len() * 2);
    for byte in value.bytes() {
        encoded.push(HEX_DIGITS[(byte >> 4) as usize] as char);
        encoded.push(HEX_DIGITS[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn layout(area: Rect, sidebar_width: u16, collapsed: bool) -> PaneRects {
    use ratatui::layout::{Constraint, Layout};

    let [body, status] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(STATUS_HEIGHT)]).areas(area);
    let room = MIN_SIDEBAR_WIDTH + SEPARATOR_WIDTH + MIN_EDITOR_WIDTH;
    if collapsed || body.width < room {
        return PaneRects {
            editor: body,
            status,
            ..PaneRects::default()
        };
    }
    let width = sidebar_width.clamp(
        MIN_SIDEBAR_WIDTH,
        (body.width - SEPARATOR_WIDTH - MIN_EDITOR_WIDTH).min(MAX_SIDEBAR_WIDTH),
    );
    let [sidebar, separator, editor] = Layout::horizontal([
        Constraint::Length(width),
        Constraint::Length(SEPARATOR_WIDTH),
        Constraint::Min(MIN_EDITOR_WIDTH),
    ])
    .areas(body);
    PaneRects {
        sidebar: Some(sidebar),
        separator: Some(separator),
        editor,
        status,
        ..PaneRects::default()
    }
}

/// Stacks the source control sections into `area`.
///
/// Every section keeps its header, so three rows are spoken for before any
/// body is drawn. Each expanded section then takes the height it asked for,
/// except the last one, which takes whatever is left: that leaves exactly one
/// section absorbing a terminal resize, so the others keep the size they were
/// dragged to.
fn layout_sections(
    area: Rect,
    wanted: [(u16, bool); Section::COUNT],
) -> [SectionRect; Section::COUNT] {
    let mut rects = [SectionRect::default(); Section::COUNT];
    let headers = SECTION_HEADER_ROWS * Section::COUNT as u16;
    let mut room = area.height.saturating_sub(headers);
    let last_open = wanted
        .iter()
        .rposition(|(_, collapsed)| !collapsed)
        .unwrap_or_default();

    let mut y = area.y;
    for (index, (height, collapsed)) in wanted.into_iter().enumerate() {
        // A frame too short for three headers draws the ones that fit and
        // stops, rather than wrapping a section off the bottom of the pane.
        if y >= area.bottom() {
            break;
        }
        rects[index].header = Rect {
            y,
            height: SECTION_HEADER_ROWS,
            ..area
        };
        y += SECTION_HEADER_ROWS;
        if collapsed || room == 0 {
            continue;
        }
        let below = wanted[index + 1..]
            .iter()
            .filter(|(_, collapsed)| !collapsed)
            .count() as u16;
        let body = match index == last_open {
            true => room,
            false => height.clamp(
                MIN_SECTION_ROWS,
                room.saturating_sub(MIN_SECTION_ROWS * below)
                    .max(MIN_SECTION_ROWS),
            ),
        }
        .min(room);
        rects[index].body = Rect {
            y,
            height: body,
            ..area
        };
        y += body;
        room -= body;
    }
    rects
}

#[cfg(test)]
mod tests {
    use super::{
        Ask, Choice, Confirm, Cursor, DEFAULT_SIDEBAR_WIDTH, DISCARD_LABEL, DocumentKey, Drag,
        EDGE_SCROLL_LINES, Focus, Input, InputKind, Layout, LocalSourceError, MAX_SIDEBAR_WIDTH,
        MIN_EDITOR_WIDTH, MIN_SECTION_ROWS, MIN_SIDEBAR_WIDTH, MenuAction, NEW_FILE_PROMPT,
        NO_RENDERED_VIEW, PaintedMarkdown, RENDERED_READ_ONLY, SCROLL_COLUMNS, SCROLL_LINES,
        ScmLayout, Section, SidebarView, Tab, TabLabel, Target, Toggle, Workbench, WorkbenchAction,
        WorkbenchPath, WorkbenchStyles, keys, layout, layout_sections, scm,
    };
    use crate::chrome::ELLIPSIS;
    use crate::editor::{VisualRow, buffer::Buffer, render};
    use crate::fs::backend::tests::{
        RemoteControl, SCOPED_CONTENTS, SCOPED_FILE, SCOPED_ROOT, SHADOW_CONTENTS, SHADOW_FILE,
        scoped_widget_fixture,
    };
    use crate::fs::tree::GitMark;
    use crate::menu::Item as MenuItem;
    use crate::scm::backend::DiffResult;
    use crate::scm::repo::Commit;
    use crate::scroll::SCROLLBAR_THUMB;
    use crate::search;
    use crate::view::{
        Control, FIND_HINTS, GOTO_HINTS, Hint, MENU_HINTS, MENU_MARK, MORE_LEFT, MORE_RIGHT,
        NAME_HINTS, NOT_A_REPOSITORY, OPEN_MARK, PALETTE_HINTS, RENDERED_STATUS, REVERT_MARK,
        STAGE_MARK, TabHit, TabPart, UNSTAGE_MARK, button_at, confirm_at, header_at, on_menu_mark,
        tab_at, toggle_at, visible_range,
    };
    use caudra_workspace::{ResourceKind, ScmDiffTarget, WorkspaceError, WorkspacePath};
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use gix::bstr::BString;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::{Buffer as Surface, Cell};
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Style};
    use ratatui::text::Line;
    use std::cell::Cell as Counter;
    use std::collections::BTreeMap;
    use std::fs;
    use std::ops::RangeInclusive;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant, SystemTime};
    use tempfile::TempDir;
    use test_case::test_case;
    use unicode_width::UnicodeWidthStr;

    const CLOSED_START: &str = "a fresh workbench must not be on screen";
    const WORKSPACE_CHANGE_BLOCKED: &str = "an idle clean workbench must allow workspace changes";
    const WORKSPACE_CHANGE_UNGUARDED: &str =
        "unsaved buffers and outstanding operations must block workspace changes";
    const LOCAL_SOURCE_FILE: &str = "policy @scope [1].lua";
    const LOCAL_SOURCE_CONTENT: &str = "one\r\ntwo\r\nthree\r\n";
    #[cfg(unix)]
    const LOCAL_SOURCE_CHANGED: &str = "source identity or fingerprint changed";
    #[cfg(unix)]
    const LOCAL_SOURCE_ISOLATED: &str =
        "opening a local source must not alter the previous workbench or its pending operations";
    const NARROW_DROPS_SIDEBAR: &str =
        "a terminal too narrow for both panes must keep the editor, not split into unusable strips";
    const STATUS_RESERVED: &str = "the status row must always be carved";
    const SIDEBAR_CLAMPED: &str = "the sidebar must never squeeze the editor below its minimum";
    const NO_TAB: &str = "the file under the cursor must have opened as a tab";
    const LEADER_TRAPPED: &str =
        "the workbench must hand Ctrl+X back, or every chord under it goes dead";
    const BLIND_CUT: &str = "cut with nothing selected must not eat the character at the cursor";
    const LEAKED_TO_FILE: &str = "what a field is given must never edit the file behind it";
    const PASTE_UNDER_DIALOG: &str =
        "a paste under a question must reach neither file nor composer";
    const FIELD_NOT_EDITED: &str = "the focused field did not take the key or paste it was given";
    const FIELD_INACTIVE: &str = "the host must be told a field is taking typing";
    const FIELD_NOT_COPIED: &str =
        "a selection in a field must reach the host's clipboard and the workbench's";
    const COPY_TRAPPED: &str = "Ctrl+C over a field with nothing selected must reach the host";
    const SAVE_TRAPPED: &str = "a chord the find bar does not take must still reach the workbench";
    const LIST_KEY_MISSED: &str =
        "the palette list takes the text-start chords and leaves Home and End to its field";
    const NOT_DIGITS: &str = "go-to-line must keep the digits of what it is given and nothing else";
    const GOTO_MISSED: &str = "go-to-line must land on the line its digits name";
    const NO_FIELD: &str = "a field must have the focus";
    const FIRST_WORD: &str = "one ";
    const SECOND_WORD: &str = "two";
    const FIELD_WORDS: &str = "one two";
    const SECOND_WORD_LINE: usize = 1;
    const GOTO_ENTRY: &str = "line 3";
    const GOTO_DIGITS: &str = "3";
    const GOTO_LINE_INDEX: usize = 2;
    const PALETTE_QUERY: &str = "txt";
    const PALETTE_PREFIX: &str = "b";
    const WRONG_REFERENCE: &str = "the composer reference does not point where the cursor is";
    const MENTION_NOT_OPENED: &str = "a mention must open the file it names";
    const MENTION_WRONG_LINES: &str = "a mention must land on the lines it names";
    const NOT_PAINTED: &str = "a frame is missing something it must always show";
    const WRONG_PANE: &str = "the sidebar is not showing what it was asked for";
    const STILL_UNFOLDED: &str = "folding the tree must leave nothing but its top level";
    const PREVIEW_STACKED: &str = "a preview must take over the tab the last one borrowed";
    const PREVIEW_TOOK_OVER: &str = "a tab that was kept must survive the next preview";
    const CHANGE_MISSING: &str = "the change the test made is not under the cursor";
    const MARK_MISSING: &str = "the explorer row is missing its source control mark";
    const DIFF_EDITABLE: &str = "a diff tab must be read-only";
    const REMOTE_SETTLE_TIMEOUT: Duration = Duration::from_secs(10);
    const REMOTE_TEST_FILE: &str = "same-name.txt";
    const REMOTE_TEST_NESTED: &str = "src/lib.rs";
    const REMOTE_REPLACEMENT: &str = "theirs";
    const REMOTE_TEST_DIRECTORY: &str = "src";
    const REMOTE_MOVED_DIRECTORY: &str = "moved";
    const REMOTE_MOVED_NESTED: &str = "moved/lib.rs";
    const REMOTE_MUTATION_CONFLICT: &str =
        "an opened buffer must retain its original conditional revision";
    const CAPPED_TREE_ENTRIES: usize = 2;
    const TARGETED_READ_TABS: usize = super::MAX_REMOTE_READS * 2 + 1;
    const INVALIDATION_BURST: usize = 512;
    const TARGETED_REPLACEMENT: &str = "new remote contents";
    const TARGETED_READ_INDEPENDENT: &str =
        "targeted reads must not depend on global listing completion";
    const TARGETED_READ_BOUNDED: &str =
        "invalidations must coalesce into bounded reads of open targets";
    const DIRECTORY_ERROR: &str = "directories cannot be opened in the editor";
    const SPECIAL_FILE_ERROR: &str = "pipes, devices and other special files cannot be edited";
    const BINARY_ERROR: &str = "binary files cannot be edited";
    const HUGE_FILE_ERROR: &str = "file exceeds the editable size limit";
    const OVERSIZED_BYTES: usize = 3 * 1024 * 1024;
    const REPOSITORY_UNAVAILABLE: &str = "repository_unavailable";
    const REPOSITORY_ERROR_CODE: i64 = -32000;
    const REPOSITORY_ERROR: &str =
        "workspace authority refused the request with code -32000, reason repository_unavailable";
    const COMMIT_OPENED_WHOLE: &str = "a commit must open into its paths, not into one document";
    const MESSAGE_NOT_SHOWN: &str = "the commit tab does not say what the commit says";
    const CLICK_REOPENED: &str = "a click on an open commit must close it, not show it again";
    const INITIAL_MESSAGE: &str = "initial";
    const SUBJECT: &str = "rewrite the scheduler";
    const BODY: &str = "The old one woke every tick.";
    const COMMIT_NOT_LISTED: &str = "the graph does not list what the commit changed";
    const DIFF_DISPLACED: &str = "a commit's diff replaced the working tree's diff of that path";
    const DISCARD_UNARMED: &str = "a discard must take exactly two goes at the same row";
    const NO_HITS: &str = "the search did not find what the fixture put there";
    const WRONG_LINE: &str = "the editor did not land on the line the match was on";
    const STALE_RESULTS: &str = "the pane disagrees about whether its results are current";
    const WRONG_CLICK: &str = "the pointer landed somewhere other than where it was pointing";
    const STALE_TAB: &str = "the tab is still showing what the file no longer says";
    const NO_REMOTE_LOG: &str = "a bound workspace must surface the commits it paged";
    const REFRESH_IGNORED: &str = "a requested source-control refresh never reached the workspace";
    const FALSE_CONFLICT: &str = "a tab with nothing to lose raised a conflict anyway";
    const NO_CONFLICT: &str = "unsaved work was overwritten without a word";
    const LOST_EDIT: &str = "a reload threw away work the user had not saved";
    const WRONG_WIDTH: &str = "the sidebar is not the width it was asked for";
    const WRONG_LAYOUT: &str = "the workbench did not come back the way it was left";
    const WRONG_HOVER: &str = "the row under the pointer is not marked the way it should be";
    const NOT_TRACKED: &str =
        "a sidebar without the focus must still mark the row the editor is on, and only that row";
    const WRONG_GEOMETRY: &str = "the sections did not divide the room the way they were asked to";
    const WRONG_ROW: &str = "the pointer did not act on the row it was pointing at";
    const LAYOUT_LOST: &str = "the source control layout did not come back the way it was left";
    const UNASKED_CLOSE: &str = "a tab was closed over unsaved work without asking";
    const NOT_ASKED: &str = "closing an edited tab must raise the unsaved-changes dialog";
    const POINTLESS_QUESTION: &str = "a tab with nothing to lose must close without a question";
    const ANSWER_IGNORED: &str = "the dialog did not do what it was answered";
    const ESC_ESCAPED: &str = "esc answered past the dialog instead of into it";
    const SAVE_LOST_WORK: &str = "a tab was closed after the save that should have kept it failed";
    const WRONG_ANSWER: &str = "the highlighted answer is not the one the keys walked to";
    const MODAL_LEAKED: &str = "a press reached the panes behind the dialog";
    const SELECTION_UNCOPIED: &str = "a finished selection never reached the clipboard";
    const IDLE_CLICK_COPIED: &str = "a press that selected nothing copied anyway";
    const KEPT_CLIPBOARD: &str = "what was copied before";
    const NOT_STEPPED: &str = "the find keys did not walk to the match they were pointed at";
    const ESC_LEFT: &str = "esc left the workbench instead of dropping the selection it was in";
    const TAB_OFF_STRIP: &str = "the strip is not showing the tab the editor is on";
    const NO_OVERFLOW_MARK: &str = "the strip does not say which end it cut tabs off";
    const WRONG_BAR: &str = "the pane is not saying how much of its content is off screen";
    const WRONG_ORDER: &str = "the palette is not offering the project the way it should";
    const NO_CONTROL: &str = "the hovered row is not offering the control it should";
    const STRAY_CONTROL: &str = "the row is offering a control it has no business offering";
    const CONTROL_IGNORED: &str = "clicking the control did not do what it says";
    const CONTROL_MISPLACED: &str = "the hit test is not where the row painted the control";
    const CONTROL_UNLIT: &str = "the control under the pointer looks like the ones beside it";
    /// Wide and tall enough to keep the sidebar and every section on screen.
    const TERMINAL_WIDTH: u16 = 80;
    const TERMINAL_HEIGHT: u16 = 24;
    const NOT_PANNED: &str = "the sideways wheel did not pan the text pane";
    const PANNED_OFF: &str = "the pan ran past the widest line the pane is showing";
    const PANNED_ELSEWHERE: &str = "a sideways wheel outside the text pane still panned it";
    const NOT_WRAPPED: &str = "the line is not laid out across the rows wrapping asks for";
    const STILL_WRAPPED: &str = "a line is still carried down after wrapping was turned off";
    const WRONG_GUTTER: &str = "the gutter is not numbering the rows it should";
    const PANNED_WRAPPED: &str = "a wrapped pane must sit at the left margin and stay there";
    const CARET_OFF_SCREEN: &str = "the caret is not on a row the pane is painting";
    /// Wide enough that no pane in these tests can show all of it at once.
    const WIDE_LINE_COLUMNS: usize = 400;
    /// Short enough to fit any pane here, so it never wraps itself.
    const SHORT_LINE: &str = "tail";
    /// What [`repository`] commits, and so what a discard has to bring back.
    const INDEXED_TEXT: &str = "one\ntwo\nthree\n";
    const REWRITTEN_TEXT: &str = "ruined\n";
    /// What [`nested_repository`] commits.
    const NESTED_TEXT: &str = "one\n";
    /// Longer than what was committed: a rewrite of the same length within the
    /// same second is racily clean, and status would call it unchanged.
    const NESTED_REWRITE: &str = "one\ntwo\n";
    /// The line the rewrite added, which is what a tab must stop showing once
    /// the file it is on has been discarded.
    const NESTED_ADDED_LINE: &str = "two";
    const ASKED_TOO_LATE: &str = "a bulk discard ran before the question about it was answered";
    const OVER_REACHED: &str = "the discard reached past the rows the cursor covered";
    const NO_MENU: &str = "nothing opened a menu where one was asked for";
    const MENU_STUCK: &str = "the menu is still up after a press somewhere else";
    const WRONG_TARGET: &str = "the menu is offering to act on something else";
    const PATH_UNCOPIED: &str = "the path the menu copied never reached the host";
    const NOT_RENAMED: &str = "the file is not where the rename said it would be";
    const RENAMED_OVER: &str = "a rename wrote over a path that was already there";
    const TAB_LEFT_BEHIND: &str = "the tab is still on a path that has moved or gone";
    const PROMPT_GONE: &str = "a refused name took the question down with it";
    const NO_REASON: &str = "a refusal said nothing about why";
    const NOT_MADE: &str = "the path the name asked for is not there";
    const DELETED_UNASKED: &str = "a path was removed before the question about it was answered";
    const NOT_COUNTED: &str = "the question does not say what the delete covers";
    const WRONG_TABS: &str = "the close left the strip showing something else";
    const QUEUE_RAN_ON: &str = "a batch close carried on past the question in front of it";
    const QUEUE_STALLED: &str = "the tabs behind the answered question were never closed";
    const MENU_GONE: &str = "a press inside the panel took the menu down";
    const WRONG_PROMPT: &str = "the field under the editor is not showing what is being typed";
    const WRONG_HINT: &str = "the status bar is not offering what the next key will reach";
    const NOT_SENT: &str = "the composer was handed something other than the path";
    const STILL_A_PREVIEW: &str = "the tab is still the one the next file will take over";
    const NOT_REVEALED: &str = "the sidebar is not showing the file it was pointed at";
    const STRAY_COPY: &str = "the release of a menu press was read as the end of a selection";
    const PANEL_OFF_BUFFER: &str = "the panel is not over the buffer, so the case is not covered";
    const NO_HINT: &str = "the status row is not offering the key the test presses";
    const HINT_UNLIT: &str =
        "the hint under the pointer is not lit from its key to its description";
    const GAP_LIT: &str = "the gap in front of a hint lit up with it";
    const HINT_IGNORED: &str = "a click on a hint did not do what pressing its key does";
    const HINT_REACHED_BEHIND: &str =
        "a click on a hint reached past what stands in front of the panes";
    const HIDDEN_HINT_PRESSED: &str = "a hint the row had no room to draw was pressed";
    const PANEL_OFF_HINT: &str =
        "the panel's rule is not over the hint, so the case is not covered";
    const HINT_UNDER_MENU: &str = "a press on the menu's rule pressed the hint painted under it";
    /// Too narrow for the editor's hints beside its path and cursor.
    const NARROW_TERMINAL_WIDTH: u16 = 60;
    /// The file [`project`] opens, and the folder beside it.
    const OPENED_FILE: &str = "a.txt";
    const NESTED_DIR: &str = "sub";
    const NESTED_FILE: &str = "b.txt";
    const RENAMED_FILE: &str = "renamed.txt";
    const RENAMED_DIR: &str = "moved";
    const MADE_NAME: &str = "made.txt";
    /// One keystroke of unsaved work, which is what makes a close ask.
    const EDIT: char = 'X';
    const PLAN_FILE: &str = "civil.md";
    const NEWER_PLAN_FILE: &str = "newer.md";
    const PLAN_TITLE: &str = "Plan";
    const PLAN_STATUS: &str = "Plan · civil.md";
    const NOT_LABELLED: &str = "the tab is not going by the name the host gave it";
    const LABEL_SHARED: &str = "the name the host gave one tab is still on another";
    const CURSOR_MOVED: &str = "coming back to a labelled tab moved the reader";
    const SIDEBAR_MOVED: &str = "coming back to a labelled tab switched the sidebar";
    const LOCAL_OPEN: &str = "a local workbench opens a local file";
    const MARKDOWN_FILE: &str = "notes.md";
    /// The first line of [`INDEXED_TEXT`].
    const FIRST_LINE: &str = "one";
    const PAINTED: &str = "» ";
    const TALL_LINES: usize = 60;
    const READ_TO: usize = 30;
    const WIDE_TERMINAL_WIDTH: u16 = 160;
    const NOT_RENDERED: &str = "the tab is not showing what the painter made of it";
    const EDITED_RENDERED: &str = "the rendered view changed the source buffer";
    const RENDERED_COPIED: &str = "one\ntw";
    const RENDERED_WORDS: &str = "one two\nthree\n";
    const WIDE_GLYPH: &str = "界";
    const COMBINED_GLYPH: &str = "e\u{301}";
    const SELECTION_COLOUR: Color = Color::Magenta;
    const SELECTION_UNPAINTED: &str = "the selected rendered cells were not highlighted";
    const SELECTION_STALE: &str = "a rendered selection survived a change to its source or layout";
    const NOT_REFUSED: &str = "a refused key did not say why";
    const STILL_RENDERED: &str = "the tab did not go back to its source";
    const CHORD_TAKEN: &str = "without a painter the chord and the view must stay out of sight";
    const PLACE_LOST: &str = "the toggle did not keep the reader's place through the document";
    const OFF_THE_END: &str = "the rendered view scrolled past its last full pane";
    const DRAFT_KEY: &str = "prompt:main";
    const DRAFT_TITLE: &str = "Prompt";
    const DRAFT_STATUS: &str = "Prompt · main";
    const DRAFT_TEXT: &str = "# ask\nwhy\n";
    const NEWER_DRAFT: &str = "# ask again\n";
    const NOT_HANDED_BACK: &str = "saving a document must hand its text to the host";
    const SAVED_TOO_SOON: &str = "a document must stay unsaved until the host says it kept it";
    const DOCUMENT_STORED: &str = "a document has no path to reopen, so it must not be stored";
    const FILE_ITEMS: &str = "a document has no file to copy, reveal or read back";
    const DOCUMENT_LOST: &str = "the document's tab went away with work in it";
    const STALE_DOCUMENT: &str =
        "a clean document is still showing what the host has moved on from";
    const NOT_ASKED_BACK: &str = "reverting a document must ask the host for its copy";

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// A key event for `bind`. For a leader bind that is only its second half,
    /// which is what [`Workbench::handle_leader`] expects.
    fn press(bind: keys::Bind) -> KeyEvent {
        KeyEvent::new(bind.code, bind.modifiers)
    }

    fn sent(path: &str, lines: Option<RangeInclusive<usize>>) -> WorkbenchAction {
        WorkbenchAction::SendToComposer {
            path: PathBuf::from(path).into(),
            lines,
        }
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn click(column: u16, row: u16) -> MouseEvent {
        mouse(MouseEventKind::Down(MouseButton::Left), column, row)
    }

    fn wheel(column: u16, row: u16) -> MouseEvent {
        mouse(MouseEventKind::ScrollDown, column, row)
    }

    fn wheel_right(column: u16, row: u16) -> MouseEvent {
        mouse(MouseEventKind::ScrollRight, column, row)
    }

    fn wheel_left(column: u16, row: u16) -> MouseEvent {
        mouse(MouseEventKind::ScrollLeft, column, row)
    }

    fn moved(column: u16, row: u16) -> MouseEvent {
        mouse(MouseEventKind::Moved, column, row)
    }

    fn drag(column: u16, row: u16) -> MouseEvent {
        mouse(MouseEventKind::Drag(MouseButton::Left), column, row)
    }

    fn release(column: u16, row: u16) -> MouseEvent {
        mouse(MouseEventKind::Up(MouseButton::Left), column, row)
    }

    fn middle_click(column: u16, row: u16) -> MouseEvent {
        mouse(MouseEventKind::Down(MouseButton::Middle), column, row)
    }

    fn right_click(column: u16, row: u16) -> MouseEvent {
        mouse(MouseEventKind::Down(MouseButton::Right), column, row)
    }

    fn right_release(column: u16, row: u16) -> MouseEvent {
        mouse(MouseEventKind::Up(MouseButton::Right), column, row)
    }

    /// A way the pointer reaches the panes while a menu stands over them.
    #[derive(Debug, Clone, Copy)]
    enum Reach {
        Wheel,
        Middle,
        Drag,
    }

    /// A way text reaches the focused field.
    enum Entry {
        Typed,
        Pasted,
        Clipboard,
    }

    /// Takes `action` from the menu that is up, which is what pressing its row
    /// of the panel does.
    fn menu_action(workbench: &mut Workbench, action: MenuAction) -> WorkbenchAction {
        let target = workbench.menu.as_ref().expect(NO_MENU).target().clone();
        workbench.menu = None;
        workbench.run_menu(action, &target)
    }

    /// Answers the question in the status row with `name`, over whatever it
    /// was given to start from.
    fn answer_prompt(workbench: &mut Workbench, name: &str) {
        while workbench
            .input
            .as_ref()
            .is_some_and(|input| !input.value.is_empty())
        {
            workbench.handle_key(key(KeyCode::Backspace));
        }
        for typed in name.chars() {
            workbench.handle_key(key(KeyCode::Char(typed)));
        }
        workbench.handle_key(key(KeyCode::Enter));
    }

    fn open_titles(workbench: &Workbench) -> Vec<String> {
        workbench
            .editor
            .tabs()
            .iter()
            .map(|tab| tab.title.clone())
            .collect()
    }

    /// Leaves unsaved work in the tab at `index`, which is what makes a close
    /// of it stop and ask.
    fn edit_tab(workbench: &mut Workbench, index: usize) {
        workbench.editor.select(index);
        workbench.focus = Focus::Editor;
        workbench.handle_key(key(KeyCode::Char(EDIT)));
    }

    /// Paints one frame, which is also what fills in the pane geometry the
    /// mouse tests measure themselves against.
    fn paint(workbench: &mut Workbench, width: u16, height: u16) -> Surface {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("a test terminal");
        terminal
            .draw(|frame| workbench.view(frame, frame.area()))
            .expect("a frame");
        terminal.backend().buffer().clone()
    }

    fn draw(workbench: &mut Workbench, width: u16, height: u16) -> String {
        paint(workbench, width, height)
            .content()
            .iter()
            .map(Cell::symbol)
            .collect()
    }

    /// How one cell is painted, which is how the hover tests tell a highlight
    /// from the row it sits on.
    fn cell_style(workbench: &mut Workbench, at: (u16, u16)) -> Style {
        paint(workbench, 80, 24)[at].style()
    }

    /// A column that is the row itself rather than the handle at its margin,
    /// found the same way the pointer tells the two apart.
    fn row_body(rows: Rect) -> u16 {
        (rows.x..rows.right())
            .find(|column| !on_menu_mark(*column, rows.x))
            .expect("a column past the handle")
    }

    /// The column one of a tab's marks landed on, found the same way the
    /// pointer finds it.
    fn mark_column(workbench: &Workbench, index: usize, part: TabPart) -> u16 {
        let tabs = workbench.panes.tabs;
        (tabs.x..tabs.right())
            .find(|column| tab_at(&workbench.editor, *column, tabs) == Some(TabHit { index, part }))
            .expect("a mark on the tab")
    }

    fn close_column(workbench: &Workbench, index: usize) -> u16 {
        mark_column(workbench, index, TabPart::Close)
    }

    /// The column an answer landed on, found the same way the pointer finds it.
    fn answer_column(workbench: &Workbench, answer: Choice) -> u16 {
        let ask = asked(workbench).ask;
        let answers = workbench.panes.confirm;
        (answers.x..answers.right())
            .find(|column| confirm_at(*column, answers.x, ask) == Some(answer))
            .expect("an answer in the dialog")
    }

    fn asked(workbench: &Workbench) -> Confirm {
        workbench.confirm.expect(NOT_ASKED)
    }

    /// The answer under the cursor, if a question is standing at all.
    fn answer(workbench: &Workbench) -> Option<Choice> {
        workbench.confirm.map(|confirm| confirm.choice)
    }

    fn workbench() -> Workbench {
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.open(Path::new("/tmp/project"));
        workbench
    }

    /// A root holding `a.txt` and `sub/b.txt`. Directories sort first, so the
    /// cursor starts on `sub`.
    fn project() -> (TempDir, Workbench) {
        let dir = TempDir::new().expect("a temporary directory");
        fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").expect("a file");
        fs::create_dir(dir.path().join("sub")).expect("a directory");
        fs::write(dir.path().join("sub/b.txt"), "nested\n").expect("a nested file");
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.open(dir.path());
        (dir, workbench)
    }

    #[test_case(false ; "open")]
    #[test_case(true ; "closed")]
    fn clean_idle_workbench_allows_workspace_change(closed: bool) {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        if closed {
            workbench.close();
        }

        assert!(
            !workbench.blocks_workspace_change(),
            "{WORKSPACE_CHANGE_BLOCKED}"
        );
    }

    #[test_case(false ; "active_tab")]
    #[test_case(true ; "inactive_tab")]
    fn dirty_buffers_block_workspace_change_even_when_closed(inactive: bool) {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.handle_key(key(KeyCode::Char(EDIT)));
        if inactive {
            workbench.open_path(&dir.path().join(NESTED_DIR).join(NESTED_FILE));
        }

        assert!(
            workbench.blocks_workspace_change(),
            "{WORKSPACE_CHANGE_UNGUARDED}"
        );
        workbench.close();
        assert!(!workbench.is_open(), "{CLOSED_START}");
        assert!(
            workbench.blocks_workspace_change(),
            "{WORKSPACE_CHANGE_UNGUARDED}"
        );
    }

    #[test_case(false ; "filesystem_write")]
    #[test_case(true ; "scm_mutation")]
    fn pending_operations_block_workspace_change_even_when_closed(scm: bool) {
        let (session, _control) = crate::fs::backend::tests::widget_fixture();
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.toggle_workspace(session).unwrap();
        settle_remote(&mut workbench, |workbench| !workbench.is_busy());
        assert!(
            !workbench.blocks_workspace_change(),
            "{WORKSPACE_CHANGE_BLOCKED}"
        );

        if scm {
            workbench.scm.select(Section::Unstaged, Some(0));
            workbench.stage_selected();
        } else {
            workbench.commit_remote_input(Input::new(
                InputKind::NewFile,
                workbench.backend_root(),
                MADE_NAME,
            ));
        }

        assert!(
            workbench.blocks_workspace_change(),
            "{WORKSPACE_CHANGE_UNGUARDED}"
        );
        workbench.close();
        assert!(!workbench.is_open(), "{CLOSED_START}");
        assert!(
            workbench.blocks_workspace_change(),
            "{WORKSPACE_CHANGE_UNGUARDED}"
        );
        settle_remote(&mut workbench, |workbench| !workbench.is_busy());
        assert!(
            !workbench.blocks_workspace_change(),
            "{WORKSPACE_CHANGE_BLOCKED}"
        );
    }

    #[cfg(not(unix))]
    #[test]
    fn local_source_refuses_an_unanchored_platform() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().canonicalize().unwrap().join(LOCAL_SOURCE_FILE);
        fs::write(&path, LOCAL_SOURCE_CONTENT).unwrap();
        let result = Workbench::open_local_source(
            WorkbenchStyles::default(),
            &path,
            None,
            LOCAL_SOURCE_CONTENT,
            |_, _, _| unreachable!(),
        );
        assert!(matches!(result, Err(LocalSourceError::Unsupported)));
    }

    #[cfg(unix)]
    #[test_case(None, (0, 0), None ; "whole_source")]
    #[test_case(Some(2..=2), (1, 0), None ; "source_line")]
    #[test_case(Some(1..=3), (2, 0), Some((0, 0)) ; "source_range")]
    fn local_source_keeps_the_literal_path_and_verified_bytes(
        lines: Option<RangeInclusive<usize>>,
        cursor: (usize, usize),
        anchor: Option<(usize, usize)>,
    ) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().canonicalize().unwrap().join(LOCAL_SOURCE_FILE);
        fs::write(&path, LOCAL_SOURCE_CONTENT).unwrap();
        let expected = (path.clone(), LOCAL_SOURCE_CONTENT);
        let workbench = Workbench::open_local_source(
            WorkbenchStyles::default(),
            &path,
            lines,
            &expected,
            |path, bytes, expected| {
                assert_eq!(path, expected.0);
                assert_eq!(bytes, expected.1.as_bytes());
                Ok(())
            },
        )
        .unwrap();

        let tab = workbench.editor.active().expect(NO_TAB);
        assert!(workbench.is_open());
        assert!(workbench.remote_backend.is_none());
        assert!(workbench.remote_scm.is_none());
        assert_eq!(tab.path, WorkbenchPath::Local(path));
        assert_eq!(tab.contents(), LOCAL_SOURCE_CONTENT);
        assert_eq!((tab.buffer.cursor().line, tab.buffer.cursor().col), cursor);
        assert_eq!(
            tab.buffer
                .selection()
                .map(|(from, _)| (from.line, from.col)),
            anchor
        );
    }

    #[cfg(unix)]
    #[test]
    fn changed_local_source_identity_does_not_create_a_workbench() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().canonicalize().unwrap().join(LOCAL_SOURCE_FILE);
        fs::write(&path, LOCAL_SOURCE_CHANGED).unwrap();
        let result = Workbench::open_local_source(
            WorkbenchStyles::default(),
            &path,
            None,
            LOCAL_SOURCE_CONTENT,
            |_, bytes, expected| {
                if bytes == expected.as_bytes() {
                    Ok(())
                } else {
                    Err(LOCAL_SOURCE_CHANGED.to_owned())
                }
            },
        );
        assert!(
            matches!(result, Err(LocalSourceError::Verification(message)) if message == LOCAL_SOURCE_CHANGED)
        );
    }

    #[cfg(unix)]
    #[test]
    fn local_source_changed_during_host_verification_is_refused() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().canonicalize().unwrap().join(LOCAL_SOURCE_FILE);
        fs::write(&path, LOCAL_SOURCE_CONTENT).unwrap();
        let result = Workbench::open_local_source(
            WorkbenchStyles::default(),
            &path,
            None,
            LOCAL_SOURCE_CONTENT,
            |path, bytes, expected| {
                assert_eq!(bytes, expected.as_bytes());
                fs::write(path, LOCAL_SOURCE_CHANGED).unwrap();
                Ok(())
            },
        );
        assert!(matches!(result, Err(LocalSourceError::Changed(_))));
    }

    #[cfg(unix)]
    #[test_case(false ; "ordinary_save")]
    #[test_case(true ; "sibling_symlink_attack")]
    fn separate_local_source_preserves_a_dirty_remote_tab_and_pending_operation(attack: bool) {
        const REMOTE_FILE: &str = "same-name.txt";

        let (session, control) = crate::fs::backend::tests::widget_fixture();
        let mut remote = Workbench::new(WorkbenchStyles::default());
        remote.toggle_workspace(session).unwrap();
        settle_remote(&mut remote, |workbench| !workbench.is_busy());
        remote.open_remote_at(WorkspacePath::new(REMOTE_FILE).unwrap(), None);
        settle_remote(&mut remote, |workbench| !workbench.is_busy());
        remote.editor_key(key(KeyCode::Char(EDIT)));
        let tab = remote.editor.active().expect(NO_TAB);
        let contents = tab.contents();
        let path = tab.path.clone();
        let resource = tab.resource.clone();
        let cursor = tab.buffer.cursor();
        let saved_remote = control.contents(REMOTE_FILE);
        remote.commit_remote_input(Input::new(
            InputKind::NewFile,
            remote.backend_root(),
            MADE_NAME,
        ));
        let pending = remote.remote_pending.clone();
        let pending_create = remote.pending_create.clone();
        assert!(!pending.is_empty());
        assert!(!pending_create.is_empty());

        let dir = TempDir::new().unwrap();
        let local_path = dir.path().canonicalize().unwrap().join(REMOTE_FILE);
        fs::write(&local_path, LOCAL_SOURCE_CONTENT).unwrap();
        let sentinel = dir.path().join("sentinel.txt");
        fs::write(&sentinel, LOCAL_SOURCE_CHANGED).unwrap();
        if attack {
            symlink(
                &sentinel,
                local_path.with_file_name(format!(".{REMOTE_FILE}.caudra-tmp")),
            )
            .unwrap();
        }
        let mut local = Workbench::open_local_source(
            WorkbenchStyles::default(),
            &local_path,
            None,
            LOCAL_SOURCE_CONTENT,
            |_, bytes, expected| {
                assert_eq!(bytes, expected.as_bytes());
                Ok(())
            },
        )
        .unwrap();
        local.editor_key(key(KeyCode::Char(EDIT)));
        let local_contents = local.editor.active().expect(NO_TAB).contents();
        local.save_active();
        assert!(!local.editor.active().expect(NO_TAB).is_dirty());
        local.close();

        let tab = remote.editor.active().expect(NO_TAB);
        assert!(remote.is_open(), "{LOCAL_SOURCE_ISOLATED}");
        assert!(remote.remote_backend.is_some(), "{LOCAL_SOURCE_ISOLATED}");
        assert!(tab.is_dirty(), "{LOCAL_SOURCE_ISOLATED}");
        assert_eq!(tab.contents(), contents, "{LOCAL_SOURCE_ISOLATED}");
        assert_eq!(tab.path, path, "{LOCAL_SOURCE_ISOLATED}");
        assert_eq!(tab.resource, resource, "{LOCAL_SOURCE_ISOLATED}");
        assert_eq!(tab.buffer.cursor(), cursor, "{LOCAL_SOURCE_ISOLATED}");
        assert_eq!(remote.remote_pending, pending, "{LOCAL_SOURCE_ISOLATED}");
        assert_eq!(
            remote.pending_create, pending_create,
            "{LOCAL_SOURCE_ISOLATED}"
        );
        assert_eq!(control.contents(REMOTE_FILE), saved_remote);
        assert_eq!(fs::read_to_string(local_path).unwrap(), local_contents);
        assert_eq!(fs::read_to_string(sentinel).unwrap(), LOCAL_SOURCE_CHANGED);
        settle_remote(&mut remote, |workbench| !workbench.is_busy());
        assert_eq!(control.contents(MADE_NAME), "");
        assert!(
            remote
                .editor
                .tabs()
                .iter()
                .find(|tab| tab.path == path)
                .expect(NO_TAB)
                .is_dirty()
        );
    }

    #[test_case(None, (0, 0), None ; "whole_file_lands_on_the_first_line")]
    #[test_case(Some(2..=2), (1, 0), None ; "one_line_moves_the_cursor")]
    #[test_case(Some(1..=3), (2, 0), Some((0, 0)) ; "a_range_selects_it")]
    fn open_at_opens_the_file_and_lands_on_its_lines(
        lines: Option<RangeInclusive<usize>>,
        expected: (usize, usize),
        anchor: Option<(usize, usize)>,
    ) {
        let (dir, mut workbench) = project();
        workbench.close();

        workbench.open_at(dir.path(), &dir.path().join("a.txt"), lines);

        assert!(workbench.is_open(), "{MENTION_NOT_OPENED}");
        let tab = workbench.editor.active().expect(NO_TAB);
        assert!(tab.path.ends_with("a.txt"), "{MENTION_NOT_OPENED}");
        assert_eq!(
            (tab.buffer.cursor().line, tab.buffer.cursor().col),
            expected,
            "{MENTION_WRONG_LINES}"
        );
        assert_eq!(
            tab.buffer
                .selection()
                .map(|(from, _)| (from.line, from.col)),
            anchor,
            "{MENTION_WRONG_LINES}"
        );
    }

    /// A mention names a path relative to the project, so the tree has to
    /// resolve it before it can strip its own root and expand the way there.
    #[test_case(OPENED_FILE ; "at_the_top_level")]
    #[test_case("sub/b.txt" ; "inside_a_directory")]
    fn open_at_selects_the_file_in_the_explorer(relative: &str) {
        let (dir, mut workbench) = project();
        workbench.sidebar = SidebarView::Search;
        workbench.sidebar_collapsed = true;

        workbench.open_at(dir.path(), Path::new(relative), None);

        assert_eq!(workbench.sidebar, SidebarView::Explorer, "{NOT_REVEALED}");
        assert!(!workbench.sidebar_collapsed, "{NOT_REVEALED}");
        assert_eq!(workbench.focus, Focus::Editor, "{NOT_REVEALED}");
        assert_eq!(
            workbench.tree.selected().map(|row| row.path.clone()),
            Some(dir.path().join(relative).into()),
            "{NOT_REVEALED}"
        );
    }

    fn cursor(workbench: &Workbench) -> Cursor {
        workbench.editor.active().expect(NO_TAB).buffer.cursor()
    }

    /// More tabs than a strip beside the sidebar can hold at 80 columns, each
    /// titled the same width, so what fits is arithmetic rather than luck.
    fn many_tabs(count: usize) -> (TempDir, Workbench) {
        let dir = TempDir::new().expect("a temporary directory");
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.open(dir.path());
        for index in 0..count {
            let path = dir.path().join(format!("file{index}.txt"));
            fs::write(&path, "one\n").expect("a file");
            workbench.open_path(&path);
        }
        (dir, workbench)
    }

    /// Forty files of sixty lines, which overruns both the sidebar's rows and
    /// the editor's in a 24-row terminal.
    fn tall_project() -> (TempDir, Workbench) {
        let dir = TempDir::new().expect("a temporary directory");
        for index in 0..40 {
            let path = dir.path().join(format!("file{index}.txt"));
            fs::write(&path, "line\n".repeat(60)).expect("a file");
        }
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.open(dir.path());
        (dir, workbench)
    }

    /// Puts the cursor on `a.txt`, which is what the editing tests want open.
    fn select_file(dir: &TempDir, workbench: &mut Workbench) {
        workbench.tree.reveal(&dir.path().join("a.txt"));
    }

    fn open_file(dir: &TempDir, workbench: &mut Workbench) {
        select_file(dir, workbench);
        workbench.handle_key(key(KeyCode::Enter));
    }

    #[test]
    fn a_new_workbench_is_closed() {
        let workbench = Workbench::new(WorkbenchStyles::default());
        assert!(!workbench.is_open(), "{CLOSED_START}");
    }

    #[test]
    fn toggle_alternates_open_and_closed() {
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        let root = Path::new("/tmp/project");
        workbench.toggle(root);
        assert!(workbench.is_open());
        workbench.toggle(root);
        assert!(!workbench.is_open(), "{CLOSED_START}");
    }

    #[test_case(keys::VIEW_EXPLORER.code, SidebarView::Explorer ; "explorer")]
    #[test_case(keys::VIEW_SOURCE_CONTROL.code, SidebarView::SourceControl ; "source_control")]
    #[test_case(keys::VIEW_SEARCH.code, SidebarView::Search ; "search")]
    fn a_view_key_switches_the_sidebar(code: KeyCode, expected: SidebarView) {
        let mut workbench = workbench();
        workbench.handle_leader(KeyEvent::new(code, KeyModifiers::NONE));
        assert_eq!(workbench.sidebar_view(), expected);
        assert_eq!(
            workbench.focus(),
            Focus::Sidebar,
            "switching views must put the cursor where the user just looked"
        );
    }

    #[test]
    fn esc_asks_the_host_to_close() {
        let mut workbench = workbench();
        let action = workbench.handle_key(key(KeyCode::Esc));
        assert_eq!(action, WorkbenchAction::Close);
    }

    /// The sidebar has no selection of its own to drop, so `Esc` there means
    /// what it always did on the first press.
    #[test_case(Focus::Editor, WorkbenchAction::Consumed ; "the editor drops its selection first")]
    #[test_case(Focus::Sidebar, WorkbenchAction::Close ; "the sidebar leaves anyway")]
    fn esc_drops_a_live_selection_before_it_leaves(focus: Focus, expected: WorkbenchAction) {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.handle_key(KeyEvent::new(keys::SELECT_ALL.code, KeyModifiers::CONTROL));
        workbench.focus = focus;

        assert_eq!(
            workbench.handle_key(key(KeyCode::Esc)),
            expected,
            "{ESC_LEFT}"
        );
        assert_eq!(
            workbench
                .editor
                .active()
                .expect(NO_TAB)
                .buffer
                .has_selection(),
            expected == WorkbenchAction::Close,
            "{ESC_LEFT}"
        );
    }

    #[test]
    fn ctrl_w_deletes_the_word_before_the_caret() {
        const WORD_DELETED: &str = "\ntwo\nthree\n";

        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.handle_key(key(KeyCode::End));

        workbench.handle_key(press(keys::DELETE_WORD));

        assert_eq!(
            workbench.editor.active().expect(NO_TAB).contents(),
            WORD_DELETED
        );
    }

    #[test]
    fn a_second_esc_leaves_once_the_selection_is_gone() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.handle_key(KeyEvent::new(keys::SELECT_ALL.code, KeyModifiers::CONTROL));

        workbench.handle_key(key(KeyCode::Esc));
        let action = workbench.handle_key(key(KeyCode::Esc));
        assert_eq!(action, WorkbenchAction::Close, "{ESC_LEFT}");
    }

    #[test]
    fn collapsing_the_sidebar_moves_focus_to_the_editor() {
        let mut workbench = workbench();
        workbench.handle_key(KeyEvent::new(
            keys::TOGGLE_SIDEBAR.code,
            KeyModifiers::CONTROL,
        ));
        assert_eq!(workbench.focus(), Focus::Editor);
    }

    /// Tab indents inside the buffer, so it only carries focus one way. The
    /// view keys are what come back.
    #[test]
    fn tab_leaves_the_sidebar_and_a_view_key_returns() {
        let mut workbench = workbench();
        workbench.handle_key(key(KeyCode::Tab));
        assert_eq!(workbench.focus(), Focus::Editor);
        workbench.handle_key(key(KeyCode::Tab));
        assert_eq!(workbench.focus(), Focus::Editor);
        workbench.handle_leader(press(keys::VIEW_EXPLORER));
        assert_eq!(workbench.focus(), Focus::Sidebar);
    }

    /// One click puts a file up without keeping it: the tab it borrowed is the
    /// one the next click takes over, so walking a tree leaves no trail.
    #[test]
    fn one_click_previews_a_file_and_the_next_takes_its_tab() {
        let (_dir, mut workbench) = project();
        paint(&mut workbench, 80, 24);
        let rows = workbench.panes.rows;
        let body = row_body(rows);
        workbench.handle_mouse(click(body, rows.y));
        workbench.handle_mouse(click(body, rows.y + 1));
        assert_eq!(workbench.active_title(), "b.txt", "{NO_TAB}");

        workbench.handle_mouse(click(body, rows.y + 2));

        assert_eq!(workbench.editor.tabs().len(), 1, "{PREVIEW_STACKED}");
        assert_eq!(workbench.active_title(), "a.txt", "{NO_TAB}");
    }

    #[test]
    fn a_second_click_keeps_the_tab_the_first_borrowed() {
        let (_dir, mut workbench) = project();
        paint(&mut workbench, 80, 24);
        let rows = workbench.panes.rows;
        let body = row_body(rows);
        workbench.handle_mouse(click(body, rows.y + 1));
        workbench.handle_mouse(click(body, rows.y + 1));

        workbench.handle_mouse(click(body, rows.y));
        workbench.handle_mouse(click(body, rows.y + 1));

        assert_eq!(workbench.editor.tabs().len(), 2, "{PREVIEW_TOOK_OVER}");
    }

    #[test]
    fn typing_in_a_preview_keeps_the_tab_it_was_shown_in() {
        let (_dir, mut workbench) = project();
        paint(&mut workbench, 80, 24);
        let rows = workbench.panes.rows;
        let body = row_body(rows);
        workbench.handle_mouse(click(body, rows.y + 1));
        workbench.handle_key(key(KeyCode::Tab));
        workbench.handle_key(key(KeyCode::Char('x')));

        workbench.handle_mouse(click(body, rows.y));
        workbench.handle_mouse(click(body, rows.y + 1));

        assert_eq!(workbench.editor.tabs().len(), 2, "{PREVIEW_TOOK_OVER}");
    }

    #[test]
    fn enter_on_a_directory_expands_it_and_enter_on_a_file_opens_it() {
        let (_dir, mut workbench) = project();

        workbench.handle_key(key(KeyCode::Enter));
        assert!(
            workbench.tree.rows().iter().any(|row| row.name == "b.txt"),
            "expanding a directory must reveal its children"
        );
        assert_eq!(workbench.focus(), Focus::Sidebar);

        workbench.handle_key(key(KeyCode::Down));
        workbench.handle_key(key(KeyCode::Enter));
        assert_eq!(workbench.focus(), Focus::Editor);
        assert_eq!(workbench.active_title(), "b.txt", "{NO_TAB}");
    }

    #[test]
    fn typing_in_the_editor_reaches_the_buffer_and_saving_writes_it() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        assert_eq!(workbench.active_title(), "a.txt", "{NO_TAB}");

        workbench.handle_key(key(KeyCode::Char('X')));
        assert!(
            workbench.editor.active().expect(NO_TAB).is_dirty(),
            "typing must mark the tab dirty"
        );

        workbench.handle_key(KeyEvent::new(keys::SAVE.code, KeyModifiers::CONTROL));
        let written = fs::read_to_string(dir.path().join("a.txt")).expect("the saved file");
        assert_eq!(written, "Xone\ntwo\nthree\n");
        assert!(!workbench.editor.active().expect(NO_TAB).is_dirty());
    }

    #[test]
    fn alt_w_on_a_dirty_tab_asks_before_closing() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.handle_key(key(KeyCode::Char('X')));

        workbench.handle_leader(press(keys::CLOSE_TAB));

        assert_eq!(workbench.editor.tabs().len(), 1, "{UNASKED_CLOSE}");
        assert_eq!(answer(&workbench), Some(Choice::Save), "{NOT_ASKED}");
    }

    #[test]
    fn a_paste_under_the_dialog_is_swallowed() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.handle_key(key(KeyCode::Char('X')));
        workbench.handle_leader(press(keys::CLOSE_TAB));
        let before = workbench
            .editor
            .active()
            .expect(NO_TAB)
            .buffer
            .lines()
            .to_vec();

        assert!(workbench.paste(FIRST_WORD), "{PASTE_UNDER_DIALOG}");

        let tab = workbench.editor.active().expect(NO_TAB);
        assert_eq!(tab.buffer.lines(), before, "{PASTE_UNDER_DIALOG}");
    }

    #[test]
    fn the_dialog_paints_the_file_and_every_answer() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.handle_key(key(KeyCode::Char('X')));
        workbench.handle_leader(press(keys::CLOSE_TAB));

        let frame = draw(&mut workbench, 80, 24);

        assert!(frame.contains("a.txt has unsaved changes"), "{NOT_PAINTED}");
        for answer in Ask::Close.answers() {
            assert!(frame.contains(answer.label(Ask::Close)), "{NOT_PAINTED}");
        }
    }

    #[test]
    fn a_clean_tab_closes_with_no_question() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);

        workbench.handle_leader(press(keys::CLOSE_TAB));

        assert!(workbench.editor.tabs().is_empty(), "{POINTLESS_QUESTION}");
        assert_eq!(workbench.confirm, None, "{POINTLESS_QUESTION}");
    }

    #[test]
    fn saving_from_the_dialog_writes_the_file_and_closes_the_tab() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.handle_key(key(KeyCode::Char('X')));
        workbench.handle_leader(press(keys::CLOSE_TAB));

        workbench.handle_key(key(KeyCode::Enter));

        assert!(workbench.editor.tabs().is_empty(), "{ANSWER_IGNORED}");
        assert_eq!(workbench.confirm, None, "{ANSWER_IGNORED}");
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).expect("the saved file"),
            "Xone\ntwo\nthree\n",
            "{ANSWER_IGNORED}"
        );
    }

    #[test]
    fn discarding_from_the_dialog_closes_the_tab_and_leaves_the_file_alone() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.handle_key(key(KeyCode::Char('X')));
        workbench.handle_leader(press(keys::CLOSE_TAB));

        workbench.handle_key(key(KeyCode::Char(Choice::Discard.accelerator())));

        assert!(workbench.editor.tabs().is_empty(), "{ANSWER_IGNORED}");
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).expect("the untouched file"),
            "one\ntwo\nthree\n",
            "{ANSWER_IGNORED}"
        );
    }

    #[test_case(key(KeyCode::Char(Choice::Cancel.accelerator())) ; "the cancel accelerator")]
    #[test_case(key(KeyCode::Esc) ; "esc")]
    fn cancelling_the_dialog_keeps_the_tab_and_its_edits(answer: KeyEvent) {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.handle_key(key(KeyCode::Char('X')));
        workbench.handle_leader(press(keys::CLOSE_TAB));

        let action = workbench.handle_key(answer);

        assert_eq!(action, WorkbenchAction::Consumed, "{ESC_ESCAPED}");
        assert_eq!(workbench.confirm, None, "{ANSWER_IGNORED}");
        assert_eq!(workbench.editor.tabs().len(), 1, "{ANSWER_IGNORED}");
        assert!(
            workbench.editor.active().expect(NO_TAB).is_dirty(),
            "{ANSWER_IGNORED}"
        );
    }

    #[test]
    fn a_save_that_fails_keeps_the_tab_open_and_says_why() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.handle_key(key(KeyCode::Char('X')));
        workbench.handle_leader(press(keys::CLOSE_TAB));
        // Nothing can be written over a directory, whichever way the save goes
        // about it.
        let path = dir.path().join("a.txt");
        fs::remove_file(&path).expect("the file to go");
        fs::create_dir(&path).expect("a directory in its place");

        workbench.handle_key(key(KeyCode::Enter));

        assert_eq!(workbench.editor.tabs().len(), 1, "{SAVE_LOST_WORK}");
        assert!(
            workbench.editor.active().expect(NO_TAB).is_dirty(),
            "{SAVE_LOST_WORK}"
        );
        assert!(workbench.flash.is_some(), "{SAVE_LOST_WORK}");
    }

    #[test_case(KeyCode::Left, Choice::Save ; "left stops on the first answer")]
    #[test_case(KeyCode::Right, Choice::Cancel ; "right stops on the last")]
    fn the_arrows_walk_the_answers_and_stop_at_both_ends(code: KeyCode, expected: Choice) {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.handle_key(key(KeyCode::Char('X')));
        workbench.handle_leader(press(keys::CLOSE_TAB));

        for _ in 0..Ask::Close.answers().len() + 1 {
            workbench.handle_key(key(code));
        }

        assert_eq!(answer(&workbench), Some(expected), "{WRONG_ANSWER}");
    }

    #[test]
    fn find_walks_the_matches_and_esc_gives_the_buffer_back() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.handle_key(KeyEvent::new(keys::FIND.code, KeyModifiers::CONTROL));
        workbench.handle_key(key(KeyCode::Char('t')));

        let tab = workbench.editor.active().expect(NO_TAB);
        assert_eq!(tab.find.position(), Some((1, 2)), "two lines contain a t");
        assert_eq!(tab.buffer.cursor().line, 1, "the cursor follows the match");

        workbench.handle_key(key(KeyCode::Enter));
        assert_eq!(
            workbench
                .editor
                .active()
                .expect(NO_TAB)
                .buffer
                .cursor()
                .line,
            2,
            "Enter must step to the next match"
        );

        workbench.handle_key(key(KeyCode::Esc));
        assert!(!workbench.editor.active().expect(NO_TAB).find.is_open());
        assert!(
            workbench.editor.active().expect(NO_TAB).buffer.line_count() == 3,
            "closing find must not have typed into the buffer"
        );
    }

    /// `a.txt` holds three `e`s: one in `one` and two in `three`.
    #[test]
    fn f3_walks_the_matches_with_the_find_bar_closed() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.handle_key(KeyEvent::new(keys::FIND.code, KeyModifiers::CONTROL));
        workbench.handle_key(key(KeyCode::Char('e')));
        workbench.handle_key(key(KeyCode::Esc));
        assert_eq!(cursor(&workbench), Cursor::new(0, 2), "{NOT_STEPPED}");

        workbench.handle_key(key(keys::FIND_NEXT.code));
        assert_eq!(cursor(&workbench), Cursor::new(2, 3), "{NOT_STEPPED}");
        workbench.handle_key(key(keys::FIND_NEXT.code));
        assert_eq!(cursor(&workbench), Cursor::new(2, 4), "{NOT_STEPPED}");
        workbench.handle_key(KeyEvent::new(keys::FIND_PREV.code, KeyModifiers::SHIFT));
        assert_eq!(cursor(&workbench), Cursor::new(2, 3), "{NOT_STEPPED}");
    }

    #[test]
    fn f3_without_a_query_holds_still() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);

        let action = workbench.handle_key(key(keys::FIND_NEXT.code));
        assert_eq!(action, WorkbenchAction::Consumed);
        assert_eq!(cursor(&workbench), Cursor::default(), "{NOT_STEPPED}");
    }

    #[test]
    fn goto_line_takes_digits_and_moves_the_cursor() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.handle_key(KeyEvent::new(keys::GOTO_LINE.code, KeyModifiers::CONTROL));
        workbench.handle_key(key(KeyCode::Char('3')));
        workbench.handle_key(key(KeyCode::Enter));

        assert_eq!(
            workbench
                .editor
                .active()
                .expect(NO_TAB)
                .buffer
                .cursor()
                .line,
            2
        );
        assert!(
            workbench.goto.is_none(),
            "the prompt must close after a jump"
        );
    }

    #[test]
    fn copy_passes_through_when_nothing_is_selected() {
        let mut workbench = workbench();
        let action = workbench.handle_key(KeyEvent::new(keys::COPY.code, KeyModifiers::CONTROL));
        assert_eq!(action, WorkbenchAction::Passthrough);
    }

    /// The leader has to reach the host from every workbench state, or a whole
    /// pane's chords go dead. A live selection used to eat it as a cut.
    #[test_case(false ; "with an idle buffer")]
    #[test_case(true  ; "with a live selection")]
    fn the_leader_always_reaches_the_host(select: bool) {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        if select {
            workbench.handle_key(KeyEvent::new(keys::SELECT_ALL.code, KeyModifiers::CONTROL));
        }

        let action = workbench.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));

        assert_eq!(action, WorkbenchAction::Passthrough, "{LEADER_TRAPPED}");
    }

    #[test]
    fn the_cut_chord_matches_shift_delete() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.handle_key(KeyEvent::new(keys::SELECT_ALL.code, KeyModifiers::CONTROL));

        let cut = workbench.handle_leader(press(keys::CUT_CHORD));

        assert!(matches!(cut, WorkbenchAction::Copy(text) if text.starts_with("one")));
        assert_eq!(
            workbench.editor.active().expect(NO_TAB).buffer.line_count(),
            1
        );
    }

    #[test]
    fn cut_without_a_selection_deletes_nothing() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        let before = workbench.editor.active().expect(NO_TAB).buffer.text();

        workbench.handle_key(press(keys::CUT));

        assert_eq!(
            workbench.editor.active().expect(NO_TAB).buffer.text(),
            before,
            "{BLIND_CUT}"
        );
    }

    #[test]
    fn cut_and_paste_move_the_selection() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.handle_key(KeyEvent::new(keys::SELECT_ALL.code, KeyModifiers::CONTROL));

        let cut = workbench.handle_key(press(keys::CUT));
        assert!(matches!(cut, WorkbenchAction::Copy(text) if text.starts_with("one")));
        assert_eq!(
            workbench.editor.active().expect(NO_TAB).buffer.line_count(),
            1
        );

        workbench.handle_key(KeyEvent::new(keys::PASTE.code, KeyModifiers::CONTROL));
        assert_eq!(
            workbench.editor.active().expect(NO_TAB).buffer.text(),
            "one\ntwo\nthree",
            "the trailing newline belongs to the file, not to the buffer"
        );
    }

    fn open_palette(workbench: &mut Workbench) {
        workbench.handle_key(press(keys::QUICK_OPEN));
    }

    fn open_find(workbench: &mut Workbench) {
        workbench.handle_key(press(keys::FIND));
    }

    fn open_search(workbench: &mut Workbench) {
        workbench.handle_leader(press(keys::VIEW_SEARCH));
    }

    fn open_name(workbench: &mut Workbench) {
        let at = WorkbenchPath::Local(workbench.root.clone());
        workbench.ask_for_name(InputKind::NewFile, at);
    }

    /// What the field the next key lands in holds.
    fn field_text(workbench: &Workbench) -> String {
        let field = workbench.focused_field().expect(NO_FIELD);
        workbench.field(field).expect(NO_FIELD).text()
    }

    /// The text chords used to leak through the palette, the find bar and the
    /// search pane and edit the file behind them, and a paste skipped every
    /// field for the file.
    #[test_case(open_palette ; "the palette")]
    #[test_case(open_find ; "the find bar")]
    #[test_case(open_search ; "the search pane")]
    #[test_case(open_name ; "the name prompt")]
    fn a_field_takes_its_keys_and_pastes_and_the_file_behind_it_nothing(open: fn(&mut Workbench)) {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        open(&mut workbench);
        assert!(workbench.text_input_active(), "{FIELD_INACTIVE}");

        assert!(workbench.paste(FIRST_WORD), "{FIELD_NOT_EDITED}");
        workbench.clipboard = SECOND_WORD.to_owned();
        workbench.handle_key(press(keys::PASTE));
        assert_eq!(field_text(&workbench), FIELD_WORDS, "{FIELD_NOT_EDITED}");

        let deleted = workbench.handle_key(press(keys::DELETE_WORD));
        assert_eq!(deleted, WorkbenchAction::Consumed, "{FIELD_NOT_EDITED}");
        assert_eq!(field_text(&workbench), FIRST_WORD, "{FIELD_NOT_EDITED}");

        for chord in [keys::SELECT_ALL, keys::KILL_LINE, keys::UNDO, keys::REDO] {
            workbench.handle_key(press(chord));
        }
        let tab = workbench.editor.active().expect(NO_TAB);
        assert!(!tab.is_dirty(), "{LEAKED_TO_FILE}");
        assert!(!tab.buffer.has_selection(), "{LEAKED_TO_FILE}");
    }

    /// The find field is one line, so it declines `Tab` and `Ctrl+J`, and the
    /// document keymap behind it would indent or break the file.
    #[test_case(press(keys::FOCUS_NEXT) ; "tab")]
    #[test_case(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL) ; "ctrl_j")]
    fn a_key_the_find_bar_declines_never_edits_the_file(declined: KeyEvent) {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        open_find(&mut workbench);

        let action = workbench.handle_key(declined);

        assert_eq!(action, WorkbenchAction::Consumed, "{LEAKED_TO_FILE}");
        let tab = workbench.editor.active().expect(NO_TAB);
        assert!(!tab.is_dirty(), "{LEAKED_TO_FILE}");
    }

    #[test_case(keys::COPY, FIELD_WORDS ; "copy keeps the query")]
    #[test_case(keys::CUT, "" ; "cut takes it")]
    fn a_selection_in_a_field_leaves_for_both_clipboards(chord: keys::Bind, left: &str) {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        open_palette(&mut workbench);
        workbench.paste(FIELD_WORDS);
        workbench.handle_key(press(keys::SELECT_ALL));

        let copied = workbench.handle_key(press(chord));

        assert_eq!(
            copied,
            WorkbenchAction::Copy(FIELD_WORDS.to_owned()),
            "{FIELD_NOT_COPIED}"
        );
        assert_eq!(workbench.clipboard, FIELD_WORDS, "{FIELD_NOT_COPIED}");
        assert_eq!(field_text(&workbench), left, "{FIELD_NOT_EDITED}");
        let tab = workbench.editor.active().expect(NO_TAB);
        assert!(!tab.is_dirty(), "{LEAKED_TO_FILE}");
    }

    #[test]
    fn copy_over_a_field_with_nothing_selected_passes_through() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        open_find(&mut workbench);
        workbench.paste(FIELD_WORDS);

        let copied = workbench.handle_key(press(keys::COPY));

        assert_eq!(copied, WorkbenchAction::Passthrough, "{COPY_TRAPPED}");
    }

    #[test]
    fn a_chord_the_find_bar_does_not_take_still_saves() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.handle_key(key(KeyCode::Char(EDIT)));
        open_find(&mut workbench);

        workbench.handle_key(press(keys::SAVE));

        let tab = workbench.editor.active().expect(NO_TAB);
        assert!(!tab.is_dirty(), "{SAVE_TRAPPED}");
    }

    #[test]
    fn a_paste_into_the_find_bar_searches_for_it() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        open_find(&mut workbench);

        workbench.paste(SECOND_WORD);

        let tab = workbench.editor.active().expect(NO_TAB);
        assert_eq!(tab.find.query().text(), SECOND_WORD, "{FIELD_NOT_EDITED}");
        assert_eq!(tab.buffer.cursor().line, SECOND_WORD_LINE, "{NOT_STEPPED}");
        assert!(!tab.is_dirty(), "{LEAKED_TO_FILE}");
    }

    #[test_case(Entry::Typed ; "typed")]
    #[test_case(Entry::Pasted ; "pasted")]
    #[test_case(Entry::Clipboard ; "pasted from the workbench clipboard")]
    fn go_to_line_keeps_only_the_digits(entry: Entry) {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.handle_key(press(keys::GOTO_LINE));

        match entry {
            Entry::Typed => type_query(&mut workbench, GOTO_ENTRY),
            Entry::Pasted => {
                workbench.paste(GOTO_ENTRY);
            }
            Entry::Clipboard => {
                workbench.clipboard = GOTO_ENTRY.to_owned();
                workbench.handle_key(press(keys::PASTE));
            }
        }
        assert_eq!(field_text(&workbench), GOTO_DIGITS, "{NOT_DIGITS}");
        workbench.handle_key(key(KeyCode::Enter));

        assert_eq!(cursor(&workbench).line, GOTO_LINE_INDEX, "{GOTO_MISSED}");
        let tab = workbench.editor.active().expect(NO_TAB);
        assert!(!tab.is_dirty(), "{LEAKED_TO_FILE}");
    }

    #[test]
    fn the_palette_list_takes_ctrl_end_and_leaves_home_to_the_caret() {
        let (_dir, mut workbench) = project();
        open_palette(&mut workbench);
        workbench.paste(PALETTE_QUERY);

        workbench.handle_key(press(keys::LIST_LAST));
        assert_eq!(
            workbench.palette.selected_index() + 1,
            workbench.palette.len(),
            "{LIST_KEY_MISSED}"
        );
        workbench.handle_key(key(KeyCode::Home));
        type_query(&mut workbench, PALETTE_PREFIX);

        assert_eq!(
            field_text(&workbench),
            format!("{PALETTE_PREFIX}{PALETTE_QUERY}"),
            "{LIST_KEY_MISSED}"
        );
    }

    #[test_case(Focus::Sidebar, None ; "the sidebar sends the selected path")]
    #[test_case(Focus::Editor, Some(1..=1) ; "the editor sends the cursor line")]
    fn the_send_chord_hands_a_reference_to_the_composer(
        focus: Focus,
        lines: Option<RangeInclusive<usize>>,
    ) {
        let (dir, mut workbench) = project();
        match focus {
            Focus::Sidebar => select_file(&dir, &mut workbench),
            Focus::Editor => open_file(&dir, &mut workbench),
        }

        let action = workbench.handle_leader(press(keys::SEND_TO_COMPOSER));

        assert_eq!(action, sent(OPENED_FILE, lines), "{WRONG_REFERENCE}");
    }

    /// Also proves the renderer records the text pane, which is what paging,
    /// scrolling and mouse clicks measure themselves against.
    #[test]
    fn a_frame_paints_the_tree_beside_the_open_buffer() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);

        let painted = draw(&mut workbench, 80, 24);

        for expected in ["FILES", "GIT", "FIND", "sub", "a.txt", "one", "three"] {
            assert!(painted.contains(expected), "{NOT_PAINTED}: {expected}");
        }
        assert!(workbench.panes.text.height > 0, "{NOT_PAINTED}: text pane");
    }

    #[test]
    fn a_click_in_the_buffer_moves_the_cursor_there() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        draw(&mut workbench, 80, 24);
        let text = workbench.panes.text;

        workbench.handle_mouse(click(text.x + 2, text.y + 1));

        assert_eq!(workbench.focus(), Focus::Editor);
        assert_eq!(
            workbench.editor.active().expect(NO_TAB).buffer.cursor(),
            Cursor::new(1, 2),
            "{WRONG_CLICK}"
        );
    }

    #[test]
    fn a_click_in_the_tree_selects_the_row_under_it() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        draw(&mut workbench, 80, 24);
        let sidebar = workbench
            .panes
            .sidebar
            .expect("a wide terminal keeps the sidebar");

        workbench.handle_mouse(click(sidebar.x + 1, sidebar.y + 1));

        assert_eq!(workbench.focus(), Focus::Sidebar);
        assert_eq!(workbench.tree.selected_index(), 0, "{WRONG_CLICK}");
    }

    #[test]
    fn the_wheel_scrolls_the_pane_under_the_pointer() {
        let dir = TempDir::new().expect("a temporary directory");
        let lines: String = (1..=50).map(|number| format!("line {number}\n")).collect();
        fs::write(dir.path().join("long.txt"), lines).expect("a file");
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.open(dir.path());
        workbench.handle_key(key(KeyCode::Enter));
        draw(&mut workbench, 60, 10);
        let text = workbench.panes.text;

        workbench.handle_mouse(wheel(text.x + 1, text.y + 1));

        let step = usize::try_from(SCROLL_LINES).expect("a forward scroll step");
        assert_eq!(
            workbench.editor.active().expect(NO_TAB).scroll(),
            step,
            "{WRONG_CLICK}"
        );
    }

    /// One line far wider than any pane here, so there is always something off
    /// to the right to pan towards, over a short one, so a row a wrap carried
    /// down can be told from the line below it.
    fn wide_file() -> (TempDir, Workbench) {
        let dir = TempDir::new().expect("a temporary directory");
        let line = "x".repeat(WIDE_LINE_COLUMNS);
        fs::write(
            dir.path().join("wide.txt"),
            format!("{line}\n{SHORT_LINE}\n"),
        )
        .expect("a file");
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.open(dir.path());
        workbench.handle_key(key(KeyCode::Enter));
        (dir, workbench)
    }

    fn panned(workbench: &Workbench) -> usize {
        workbench.editor.active().expect(NO_TAB).h_scroll()
    }

    fn wrap(workbench: &mut Workbench) {
        workbench.handle_leader(press(keys::TOGGLE_WRAP));
    }

    /// The line numbers beside the text pane, which is whatever the editor
    /// column has left of it.
    fn gutter_of(workbench: &Workbench) -> Rect {
        let (editor, text) = (workbench.panes.editor, workbench.panes.text);
        Rect {
            x: editor.x,
            width: text.x - editor.x,
            ..text
        }
    }

    /// One row of `rect`, as it was painted.
    fn row_of(surface: &Surface, rect: Rect, offset: u16) -> String {
        (rect.x..rect.right())
            .map(|column| surface[(column, rect.y + offset)].symbol())
            .collect()
    }

    /// The rows the pane is painting, read the same way the frame reads them.
    fn painted_rows(workbench: &Workbench) -> Vec<VisualRow> {
        let text = workbench.panes.text;
        workbench.editor.active().expect(NO_TAB).visible_rows(
            text.height as usize,
            text.width as usize,
            workbench.wrap,
        )
    }

    /// Whether the caret falls on a row the pane is painting, which is what
    /// following it is for.
    fn caret_on_screen(workbench: &Workbench) -> bool {
        let tab = workbench.editor.active().expect(NO_TAB);
        let caret = tab.buffer.cursor();
        let at = render::display_column(tab.buffer.line(caret.line), caret.col);
        painted_rows(workbench)
            .iter()
            .any(|row| row.line == caret.line && (row.start..row.start + row.span).contains(&at))
    }

    #[test]
    fn wrapping_carries_a_long_line_onto_the_next_row_and_numbers_it_once() {
        let (_dir, mut workbench) = wide_file();
        draw(&mut workbench, 60, 10);

        wrap(&mut workbench);
        let surface = paint(&mut workbench, 60, 10);
        let (text, gutter) = (workbench.panes.text, gutter_of(&workbench));

        assert_eq!(
            row_of(&surface, text, 1),
            "x".repeat(text.width as usize),
            "{NOT_WRAPPED}"
        );
        assert!(row_of(&surface, gutter, 0).contains('1'), "{WRONG_GUTTER}");
        assert!(
            row_of(&surface, gutter, 1).trim().is_empty(),
            "{WRONG_GUTTER}"
        );
    }

    #[test]
    fn turning_wrap_off_puts_the_line_back_on_one_row() {
        let (_dir, mut workbench) = wide_file();
        draw(&mut workbench, 60, 10);

        wrap(&mut workbench);
        wrap(&mut workbench);
        let surface = paint(&mut workbench, 60, 10);
        let text = workbench.panes.text;

        assert_eq!(
            row_of(&surface, text, 0),
            "x".repeat(text.width as usize),
            "{NOT_WRAPPED}"
        );
        assert_eq!(
            row_of(&surface, text, 1).trim(),
            SHORT_LINE,
            "{STILL_WRAPPED}"
        );
    }

    #[test]
    fn a_click_on_a_carried_row_lands_further_along_the_same_line() {
        let (_dir, mut workbench) = wide_file();
        draw(&mut workbench, 60, 10);
        wrap(&mut workbench);
        draw(&mut workbench, 60, 10);
        let text = workbench.panes.text;

        workbench.handle_mouse(click(text.x + 2, text.y + 1));

        assert_eq!(
            cursor(&workbench),
            Cursor::new(0, text.width as usize + 2),
            "{WRONG_CLICK}"
        );
    }

    #[test]
    fn wrapping_pans_back_to_the_left_margin_and_stays_there() {
        let (_dir, mut workbench) = wide_file();
        draw(&mut workbench, 60, 10);
        let text = workbench.panes.text;

        workbench.handle_mouse(wheel_right(text.x + 1, text.y + 1));
        assert_ne!(panned(&workbench), 0, "{NOT_PANNED}");

        wrap(&mut workbench);
        assert_eq!(panned(&workbench), 0, "{PANNED_WRAPPED}");

        workbench.handle_mouse(wheel_right(text.x + 1, text.y + 1));
        assert_eq!(panned(&workbench), 0, "{PANNED_WRAPPED}");
    }

    #[test]
    fn a_caret_below_the_fold_of_a_wrapped_line_is_scrolled_into_view() {
        let (_dir, mut workbench) = wide_file();
        draw(&mut workbench, 60, 10);
        wrap(&mut workbench);
        draw(&mut workbench, 60, 10);

        workbench.handle_key(key(KeyCode::End));

        assert!(caret_on_screen(&workbench), "{CARET_OFF_SCREEN}");
        let top = painted_rows(&workbench)
            .first()
            .copied()
            .expect("a painted row");
        assert_eq!(top.line, 0, "{CARET_OFF_SCREEN}");
        assert!(top.index > 0, "{CARET_OFF_SCREEN}");
    }

    #[test]
    fn the_sideways_wheel_pans_the_text_and_back() {
        let (_dir, mut workbench) = wide_file();
        draw(&mut workbench, 60, 10);
        let text = workbench.panes.text;
        let step = usize::try_from(SCROLL_COLUMNS).expect("a forward pan step");

        workbench.handle_mouse(wheel_right(text.x + 1, text.y + 1));
        assert_eq!(panned(&workbench), step, "{NOT_PANNED}");

        workbench.handle_mouse(wheel_left(text.x + 1, text.y + 1));
        assert_eq!(panned(&workbench), 0, "{NOT_PANNED}");
    }

    #[test]
    fn a_pan_stops_at_the_widest_line_in_view() {
        let (_dir, mut workbench) = wide_file();
        draw(&mut workbench, 60, 10);
        let text = workbench.panes.text;

        for _ in 0..WIDE_LINE_COLUMNS {
            workbench.handle_mouse(wheel_right(text.x + 1, text.y + 1));
        }

        assert_eq!(
            panned(&workbench),
            WIDE_LINE_COLUMNS - text.width as usize,
            "{PANNED_OFF}"
        );
    }

    #[test]
    fn a_sideways_wheel_outside_the_text_pans_nothing() {
        let (_dir, mut workbench) = wide_file();
        draw(&mut workbench, 80, 10);
        let sidebar = workbench
            .panes
            .sidebar
            .expect("a wide terminal keeps the sidebar");

        workbench.handle_mouse(wheel_right(sidebar.x + 1, sidebar.y + 1));

        assert_eq!(panned(&workbench), 0, "{PANNED_ELSEWHERE}");
    }

    #[test]
    fn quick_open_types_a_path_and_enter_opens_it() {
        let (dir, mut workbench) = project();
        draw(&mut workbench, 80, 24);

        workbench.handle_key(KeyEvent::new(keys::QUICK_OPEN.code, KeyModifiers::CONTROL));
        for ch in "b.txt".chars() {
            workbench.handle_key(key(KeyCode::Char(ch)));
        }
        let listed = draw(&mut workbench, 80, 24);
        assert!(listed.contains("b.txt"), "{NOT_PAINTED}: the match");

        workbench.handle_key(key(KeyCode::Enter));

        assert!(
            !workbench.palette.is_open(),
            "choosing must close the palette"
        );
        assert_eq!(workbench.active_title(), "b.txt", "{NO_TAB}");
        assert_eq!(
            workbench.editor.active().expect(NO_TAB).path,
            WorkbenchPath::Local(dir.path().join("sub/b.txt"))
        );
        assert_eq!(
            workbench.tree.selected().map(|row| row.name.clone()),
            Some("b.txt".to_owned()),
            "opening from the palette must reveal the file in the tree"
        );
    }

    /// `a.txt` is open and active and `b.txt` is not, so the palette leads
    /// with the one worth going to.
    #[test]
    fn the_palette_offers_the_other_open_tabs_first() {
        let (dir, mut workbench) = project();
        workbench.open_path(&dir.path().join("sub/b.txt"));
        workbench.open_path(&dir.path().join("a.txt"));

        workbench.handle_key(KeyEvent::new(keys::QUICK_OPEN.code, KeyModifiers::CONTROL));

        assert_eq!(
            workbench.palette.rows().next(),
            Some("sub/b.txt"),
            "{WRONG_ORDER}"
        );
    }

    #[test_case(false ; "refresh rewalks")]
    #[test_case(true  ; "so does changing what counts")]
    fn the_palette_rewalks_when_the_project_is_said_to_have_moved(toggle_hidden: bool) {
        let (dir, mut workbench) = project();
        workbench.handle_key(KeyEvent::new(keys::QUICK_OPEN.code, KeyModifiers::CONTROL));
        workbench.handle_key(key(KeyCode::Esc));
        fs::write(dir.path().join("late.txt"), "").expect("a file");

        match toggle_hidden {
            true => {
                workbench.handle_leader(press(keys::TOGGLE_HIDDEN));
            }
            false => {
                workbench.handle_key(press(keys::REFRESH));
            }
        }
        workbench.handle_key(KeyEvent::new(keys::QUICK_OPEN.code, KeyModifiers::CONTROL));

        assert!(
            workbench.palette.rows().any(|row| row == "late.txt"),
            "{WRONG_ORDER}"
        );
    }

    #[test]
    fn esc_closes_the_palette_without_opening_anything() {
        let (_dir, mut workbench) = project();
        workbench.handle_key(KeyEvent::new(keys::QUICK_OPEN.code, KeyModifiers::CONTROL));
        workbench.handle_key(key(KeyCode::Esc));

        assert!(!workbench.palette.is_open());
        assert!(workbench.editor.tabs().is_empty(), "{NO_TAB}");
    }

    #[test]
    fn hidden_files_toggle_the_tree_and_not_just_the_flag() {
        let (dir, mut workbench) = project();
        fs::write(dir.path().join(".secret"), "").expect("a hidden file");
        workbench.tree.reload();
        assert!(
            !workbench
                .tree
                .rows()
                .iter()
                .any(|row| row.name == ".secret")
        );

        workbench.handle_leader(press(keys::TOGGLE_HIDDEN));

        assert!(
            workbench
                .tree
                .rows()
                .iter()
                .any(|row| row.name == ".secret")
        );
    }

    #[test_case(30 ; "very_narrow")]
    #[test_case(MIN_SIDEBAR_WIDTH + MIN_EDITOR_WIDTH ; "one_column_short")]
    fn a_narrow_terminal_drops_the_sidebar(width: u16) {
        let panes = layout(Rect::new(0, 0, width, 20), 30, false);
        assert!(panes.sidebar.is_none(), "{NARROW_DROPS_SIDEBAR}");
        assert_eq!(panes.editor.width, width, "{NARROW_DROPS_SIDEBAR}");
        assert_eq!(panes.status.height, 1, "{STATUS_RESERVED}");
    }

    #[test_case(10 ; "below_minimum")]
    #[test_case(30 ; "comfortable")]
    #[test_case(500 ; "beyond_the_terminal")]
    fn the_sidebar_never_starves_the_editor(requested: u16) {
        let panes = layout(Rect::new(0, 0, 120, 40), requested, false);
        let sidebar = panes.sidebar.expect("a wide terminal keeps the sidebar");
        assert!(sidebar.width >= MIN_SIDEBAR_WIDTH, "{SIDEBAR_CLAMPED}");
        assert!(panes.editor.width >= MIN_EDITOR_WIDTH, "{SIDEBAR_CLAMPED}");
        assert_eq!(panes.status.height, 1, "{STATUS_RESERVED}");
    }

    #[test]
    fn a_collapsed_sidebar_gives_the_editor_the_whole_body() {
        let panes = layout(Rect::new(0, 0, 120, 40), 30, true);
        assert!(panes.sidebar.is_none());
        assert!(panes.separator.is_none());
        assert_eq!(panes.editor.width, 120);
    }

    /// A project that is also a repository, with one untracked file, showing
    /// the source control pane.
    fn repository() -> (TempDir, Workbench) {
        let dir = TempDir::new().expect("a temporary directory");
        gix::init(dir.path()).expect("a repository");
        fs::write(dir.path().join("a.txt"), INDEXED_TEXT).expect("a file");
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.open(dir.path());
        workbench.handle_leader(press(keys::VIEW_SOURCE_CONTROL));
        (dir, workbench)
    }

    fn selected_change(workbench: &Workbench) -> Option<(String, bool, GitMark)> {
        workbench
            .scm
            .selected_change()
            .map(|change| (change.relative.clone(), change.staged, change.mark))
    }

    /// One index entry, as the tree writer wants it: the path it was recorded
    /// under, the mode it had, and the blob it points at.
    type Staged = (BString, gix::objs::tree::EntryMode, gix::ObjectId);

    /// Writes `entries` out as a tree, folder objects and all. The index is a
    /// flat list of slash-separated paths and git will not read a tree that
    /// keeps them that way, so every folder is rebuilt on the way down.
    fn write_tree(repo: &gix::Repository, entries: &[Staged]) -> gix::ObjectId {
        let mut tree = gix::objs::Tree::empty();
        let mut folders: BTreeMap<BString, Vec<Staged>> = BTreeMap::new();
        for (path, mode, id) in entries {
            match path.iter().position(|byte| *byte == b'/') {
                Some(cut) => folders.entry(path[..cut].into()).or_default().push((
                    path[cut + 1..].into(),
                    *mode,
                    *id,
                )),
                None => tree.entries.push(gix::objs::tree::Entry {
                    mode: *mode,
                    filename: path.clone(),
                    oid: *id,
                }),
            }
        }
        for (name, held) in folders {
            tree.entries.push(gix::objs::tree::Entry {
                mode: gix::objs::tree::EntryKind::Tree.into(),
                filename: name,
                oid: write_tree(repo, &held),
            });
        }
        tree.entries.sort();
        repo.write_object(&tree).expect("a tree").detach()
    }

    /// Records whatever is staged, so the graph has a commit to list. The test
    /// environment has no git identity, so the signature is spelled out rather
    /// than inherited.
    fn commit_all(workbench: &mut Workbench) {
        commit_message(workbench, INITIAL_MESSAGE);
    }

    fn commit_message(workbench: &mut Workbench, message: &str) {
        let workdir = workbench.scm.workdir().expect("a repository").to_path_buf();
        let repo = gix::open(&workdir).expect("a repository");
        let index = repo.index_or_empty().expect("an index");
        let entries: Vec<Staged> = index
            .entries()
            .iter()
            .map(|entry| {
                (
                    entry.path(&index).to_owned(),
                    entry.mode.to_tree_entry_mode().expect("a tree mode"),
                    entry.id,
                )
            })
            .collect();
        let id = write_tree(&repo, &entries);
        let who = gix::actor::SignatureRef {
            name: "Workbench Test".into(),
            email: "test@example.invalid".into(),
            time: "1700000000 +0000",
        };
        repo.commit_as(who, who, "HEAD", message, id, repo.head_id().ok())
            .expect("a commit");
        workbench.handle_key(key(keys::REFRESH.code));
    }

    #[test]
    fn the_source_control_pane_lists_an_untracked_file() {
        let (_dir, workbench) = repository();
        assert_eq!(
            workbench.sidebar_view(),
            SidebarView::SourceControl,
            "{WRONG_PANE}"
        );
        assert_eq!(
            selected_change(&workbench),
            Some(("a.txt".to_owned(), false, GitMark::Untracked)),
            "{CHANGE_MISSING}"
        );
    }

    #[test]
    fn the_explorer_paints_the_mark_source_control_found() {
        let (dir, workbench) = repository();
        let row = workbench
            .tree
            .rows()
            .iter()
            .find(|row| row.path == WorkbenchPath::Local(dir.path().join("a.txt")))
            .expect("the file must be in the tree");
        assert_eq!(row.git, Some(GitMark::Untracked), "{MARK_MISSING}");
    }

    #[test]
    fn space_moves_the_change_between_the_two_sections() {
        let (_dir, mut workbench) = repository();
        workbench.handle_key(key(keys::STAGE_TOGGLE.code));

        assert_eq!(workbench.scm.count(Section::Staged), 1, "{CHANGE_MISSING}");
        assert_eq!(
            workbench.scm.count(Section::Unstaged),
            0,
            "{CHANGE_MISSING}"
        );
        // The cursor stays where it was, so a run of presses stages a run of
        // files instead of chasing each one up into the staged section.
        assert_eq!(
            workbench.scm.cursor().section,
            Section::Unstaged,
            "{WRONG_PANE}"
        );

        workbench.scm.select(Section::Staged, Some(0));
        assert_eq!(
            selected_change(&workbench),
            Some(("a.txt".to_owned(), true, GitMark::Added)),
            "{CHANGE_MISSING}"
        );

        workbench.handle_key(key(keys::STAGE_TOGGLE.code));

        assert_eq!(workbench.scm.count(Section::Staged), 0, "{CHANGE_MISSING}");
        assert_eq!(
            workbench.scm.count(Section::Unstaged),
            1,
            "{CHANGE_MISSING}"
        );
    }

    #[test]
    fn space_on_a_header_stages_every_path_the_section_lists() {
        let (dir, mut workbench) = repository();
        fs::write(dir.path().join("b.txt"), "second\n").expect("write");
        workbench.handle_key(key(keys::REFRESH.code));
        assert_eq!(
            workbench.scm.count(Section::Unstaged),
            2,
            "{CHANGE_MISSING}"
        );

        workbench.scm.select(Section::Unstaged, None);
        workbench.handle_key(key(keys::STAGE_TOGGLE.code));

        assert_eq!(workbench.scm.count(Section::Staged), 2, "{CHANGE_MISSING}");
        assert_eq!(
            workbench.scm.count(Section::Unstaged),
            0,
            "{CHANGE_MISSING}"
        );
    }

    #[test]
    fn opening_a_diff_gives_a_tab_that_cannot_be_edited() {
        let (_dir, mut workbench) = repository();
        workbench.handle_key(key(KeyCode::Char('d')));

        let tab = workbench.editor.active().expect("a diff tab");
        assert!(!tab.is_editable(), "{DIFF_EDITABLE}");
        assert!(tab.diff_rows().is_some(), "{DIFF_EDITABLE}");
        assert_eq!(workbench.focus(), Focus::Editor, "{WRONG_PANE}");
    }

    #[test]
    fn a_discard_takes_two_presses_and_restores_the_indexed_content() {
        let (dir, mut workbench) = repository();
        let path = dir.path().join("a.txt");
        workbench.handle_key(key(KeyCode::Char(' ')));
        fs::write(&path, REWRITTEN_TEXT).expect("a rewritten file");
        workbench.handle_key(key(KeyCode::F(5)));
        workbench.handle_key(key(KeyCode::Down));
        assert_eq!(
            selected_change(&workbench),
            Some(("a.txt".to_owned(), false, GitMark::Modified)),
            "{CHANGE_MISSING}"
        );

        workbench.handle_key(key(KeyCode::Char('x')));
        assert_eq!(
            fs::read_to_string(&path).expect("the file"),
            REWRITTEN_TEXT,
            "{DISCARD_UNARMED}"
        );

        workbench.handle_key(key(KeyCode::Char('x')));
        assert_eq!(
            fs::read_to_string(&path).expect("the file"),
            INDEXED_TEXT,
            "{DISCARD_UNARMED}"
        );
    }

    #[test]
    fn a_key_between_the_two_presses_cancels_the_discard() {
        let (dir, mut workbench) = repository();
        let path = dir.path().join("a.txt");
        workbench.handle_key(key(KeyCode::Char(' ')));
        fs::write(&path, REWRITTEN_TEXT).expect("a rewritten file");
        workbench.handle_key(key(KeyCode::F(5)));
        workbench.handle_key(key(KeyCode::Down));

        workbench.handle_key(key(KeyCode::Char('x')));
        workbench.handle_key(key(KeyCode::Up));
        workbench.handle_key(key(KeyCode::Down));
        workbench.handle_key(key(KeyCode::Char('x')));

        assert_eq!(
            fs::read_to_string(&path).expect("the file"),
            REWRITTEN_TEXT,
            "{DISCARD_UNARMED}"
        );
    }

    #[test]
    fn the_tree_key_switches_how_the_change_sections_list_paths() {
        let (_dir, mut workbench) = repository();
        assert!(!workbench.scm.is_flat(), "{WRONG_PANE}");

        workbench.handle_key(key(keys::TOGGLE_TREE.code));
        assert!(workbench.scm.is_flat(), "{WRONG_PANE}");

        workbench.handle_key(key(keys::TOGGLE_TREE.code));
        assert!(!workbench.scm.is_flat(), "{WRONG_PANE}");
    }

    /// A repository whose changes are spread over a folder, so the sections
    /// have enough rows for the geometry to be worth measuring.
    ///
    /// The files are committed before they are changed: git reports an
    /// untracked directory as the directory itself, so nothing under it would
    /// reach the tree.
    fn nested_repository() -> (TempDir, Workbench) {
        const PATHS: [&str; 3] = ["src/a.txt", "src/b.txt", "top.txt"];
        let dir = TempDir::new().expect("a temporary directory");
        gix::init(dir.path()).expect("a repository");
        fs::create_dir(dir.path().join("src")).expect("a directory");
        for name in PATHS {
            fs::write(dir.path().join(name), NESTED_TEXT).expect("a file");
        }
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.open(dir.path());
        workbench.handle_leader(press(keys::VIEW_SOURCE_CONTROL));
        for name in PATHS {
            workbench.scm.stage_path(name).expect("staging");
        }
        commit_all(&mut workbench);
        for name in PATHS {
            fs::write(dir.path().join(name), NESTED_REWRITE).expect("a file");
        }
        workbench.handle_key(key(keys::REFRESH.code));
        (dir, workbench)
    }

    /// The screen row a section's body starts on, read off the geometry the
    /// last frame recorded rather than worked out again here.
    fn body_of(workbench: &Workbench, section: Section) -> Rect {
        let index = Section::ALL
            .iter()
            .position(|candidate| *candidate == section)
            .expect("a section");
        workbench.panes.sections[index].body
    }

    fn header_of(workbench: &Workbench, section: Section) -> Rect {
        let index = Section::ALL
            .iter()
            .position(|candidate| *candidate == section)
            .expect("a section");
        workbench.panes.sections[index].header
    }

    fn section_index(section: Section) -> usize {
        Section::ALL
            .iter()
            .position(|candidate| *candidate == section)
            .expect("a section")
    }

    /// Where a control landed on a body row, found the way the pointer finds
    /// it rather than by counting columns here.
    fn control_on_row(
        workbench: &Workbench,
        section: Section,
        row: usize,
        wanted: Control,
    ) -> (u16, u16) {
        let body = body_of(workbench, section);
        let y = body.y + (row - workbench.scm.scroll(section)) as u16;
        let x = (body.x..body.right())
            .find(|column| workbench.row_control((*column, y), section, body, row) == Some(wanted))
            .expect("a control on the row");
        (x, y)
    }

    fn control_on_header(workbench: &Workbench, section: Section, wanted: Control) -> (u16, u16) {
        let header = header_of(workbench, section);
        let index = section_index(section);
        let x = (header.x..header.right())
            .find(|column| {
                workbench.header_control((*column, header.y), section, index) == Some(wanted)
            })
            .expect("a control on the header");
        (x, header.y)
    }

    /// The painted row, with the pointer resting on it so its controls show.
    fn hovered_row(workbench: &mut Workbench, section: Section, row: usize) -> String {
        let body = body_of(workbench, section);
        let y = body.y + (row - workbench.scm.scroll(section)) as u16;
        workbench.handle_mouse(moved(body.x, y));
        let surface = paint(workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        (body.x..body.right())
            .map(|column| surface[(column, y)].symbol())
            .collect()
    }

    #[test]
    fn an_unstaged_row_offers_to_stage_what_it_lists() {
        let (_dir, mut workbench) = repository();
        draw(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);

        let painted = hovered_row(&mut workbench, Section::Unstaged, 0);

        assert!(painted.contains(STAGE_MARK), "{NO_CONTROL}: {painted:?}");
    }

    #[test]
    fn a_staged_file_offers_to_open_it_as_well_as_unstage_it() {
        let (_dir, mut workbench) = nested_repository();
        workbench.scm.stage_path("top.txt").expect("staging");
        draw(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);

        let painted = hovered_row(&mut workbench, Section::Staged, 0);

        assert!(painted.contains(OPEN_MARK), "{NO_CONTROL}: {painted:?}");
        assert!(painted.contains(UNSTAGE_MARK), "{NO_CONTROL}: {painted:?}");
    }

    #[test]
    fn a_row_the_pointer_left_paints_no_controls() {
        let (_dir, mut workbench) = repository();
        draw(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let body = body_of(&workbench, Section::Unstaged);

        workbench.handle_mouse(moved(body.x, body.y));
        workbench.handle_mouse(moved(0, 0));
        let surface = paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let painted: String = (body.x..body.right())
            .map(|column| surface[(column, body.y)].symbol())
            .collect();

        assert!(
            !painted.contains(STAGE_MARK),
            "{STRAY_CONTROL}: {painted:?}"
        );
    }

    #[test]
    fn clicking_the_stage_control_moves_the_file_between_sections() {
        let (_dir, mut workbench) = repository();
        draw(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);

        let at = control_on_row(&workbench, Section::Unstaged, 0, Control::Stage);
        workbench.handle_mouse(click(at.0, at.1));

        assert_eq!(workbench.scm.count(Section::Staged), 1, "{CONTROL_IGNORED}");
        draw(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);

        let at = control_on_row(&workbench, Section::Staged, 0, Control::Stage);
        workbench.handle_mouse(click(at.0, at.1));

        assert_eq!(workbench.scm.count(Section::Staged), 0, "{CONTROL_IGNORED}");
    }

    #[test]
    fn clicking_a_header_control_stages_every_path_the_section_lists() {
        let (_dir, mut workbench) = nested_repository();
        draw(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let listed = workbench.scm.count(Section::Unstaged);
        assert!(listed > 1, "{CONTROL_IGNORED}: the fixture lists one path");

        let at = control_on_header(&workbench, Section::Unstaged, Control::Stage);
        workbench.handle_mouse(click(at.0, at.1));

        assert_eq!(
            workbench.scm.count(Section::Staged),
            listed,
            "{CONTROL_IGNORED}"
        );
    }

    #[test]
    fn clicking_the_open_control_opens_the_file_rather_than_its_diff() {
        let (_dir, mut workbench) = nested_repository();
        workbench.scm.stage_path("top.txt").expect("staging");
        draw(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);

        let at = control_on_row(&workbench, Section::Staged, 0, Control::Open);
        workbench.handle_mouse(click(at.0, at.1));

        assert_eq!(workbench.active_title(), "top.txt", "{CONTROL_IGNORED}");
        assert!(
            workbench
                .editor
                .active()
                .expect(NO_TAB)
                .diff_rows()
                .is_none(),
            "{CONTROL_IGNORED}: a diff opened instead of the file"
        );
    }

    /// A staged file rewritten underneath its index entry, which is the only
    /// shape of change a revert has anything to restore from.
    fn rewritten(workbench: &mut Workbench) -> PathBuf {
        let path = workbench.root.join("a.txt");
        workbench.scm.stage_path("a.txt").expect("staging");
        fs::write(&path, REWRITTEN_TEXT).expect("a rewritten file");
        workbench.scm.refresh();
        draw(workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        path
    }

    #[test]
    fn an_unstaged_row_offers_to_throw_its_edits_away() {
        let (_dir, mut workbench) = repository();
        rewritten(&mut workbench);

        let painted = hovered_row(&mut workbench, Section::Unstaged, 0);

        assert!(painted.contains(REVERT_MARK), "{NO_CONTROL}: {painted:?}");
    }

    /// The column a mark landed on with the pointer resting on its row, which
    /// is what the hit test has to agree with. Asking the hit test where its
    /// own buttons are cannot catch the two drifting apart.
    fn painted_column(workbench: &mut Workbench, rect: Rect, y: u16, mark: &str) -> u16 {
        workbench.handle_mouse(moved(rect.x, y));
        let surface = paint(workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        (rect.x..rect.right())
            .find(|column| surface[(*column, y)].symbol() == mark)
            .expect("a painted mark")
    }

    #[test_case(STAGE_MARK, Control::Stage ; "the stage mark")]
    #[test_case(REVERT_MARK, Control::Revert ; "the revert mark")]
    #[test_case(OPEN_MARK, Control::Open ; "the open mark")]
    fn a_mark_and_the_air_around_it_press_that_control(mark: &str, expected: Control) {
        let (_dir, mut workbench) = repository();
        rewritten(&mut workbench);
        let body = body_of(&workbench, Section::Unstaged);

        let column = painted_column(&mut workbench, body, body.y, mark);

        for at in [column - 1, column, column + 1] {
            assert_eq!(
                workbench.row_control((at, body.y), Section::Unstaged, body, 0),
                Some(expected),
                "{CONTROL_MISPLACED}"
            );
        }
    }

    #[test]
    fn a_click_on_a_mark_the_header_painted_presses_that_control() {
        let (_dir, mut workbench) = repository();
        rewritten(&mut workbench);
        let header = header_of(&workbench, Section::Unstaged);
        let index = section_index(Section::Unstaged);

        let column = painted_column(&mut workbench, header, header.y, STAGE_MARK);

        assert_eq!(
            workbench.header_control((column, header.y), Section::Unstaged, index),
            Some(Control::Stage),
            "{CONTROL_MISPLACED}"
        );
    }

    #[test]
    fn the_control_under_the_pointer_is_lit_apart_from_its_neighbours() {
        let (_dir, mut workbench) = repository();
        rewritten(&mut workbench);
        let body = body_of(&workbench, Section::Unstaged);
        let stage = painted_column(&mut workbench, body, body.y, STAGE_MARK);
        let revert = painted_column(&mut workbench, body, body.y, REVERT_MARK);

        workbench.handle_mouse(moved(stage, body.y));

        assert_ne!(
            cell_style(&mut workbench, (stage, body.y)),
            cell_style(&mut workbench, (revert, body.y)),
            "{CONTROL_UNLIT}"
        );
    }

    #[test]
    fn an_unstaged_file_offers_to_open_it_as_well() {
        let (_dir, mut workbench) = repository();
        rewritten(&mut workbench);

        let painted = hovered_row(&mut workbench, Section::Unstaged, 0);

        assert!(painted.contains(OPEN_MARK), "{NO_CONTROL}: {painted:?}");
    }

    #[test]
    fn clicking_the_open_control_on_an_unstaged_file_opens_the_file() {
        let (_dir, mut workbench) = repository();
        rewritten(&mut workbench);

        let at = control_on_row(&workbench, Section::Unstaged, 0, Control::Open);
        workbench.handle_mouse(click(at.0, at.1));

        assert_eq!(workbench.active_title(), "a.txt", "{CONTROL_IGNORED}");
        assert!(
            workbench
                .editor
                .active()
                .expect(NO_TAB)
                .diff_rows()
                .is_none(),
            "{CONTROL_IGNORED}: a diff opened instead of the file"
        );
    }

    #[test]
    fn a_name_too_long_for_its_row_still_shows_the_controls() {
        let (dir, mut workbench) = nested_repository();
        let long = format!("src/{}.txt", "n".repeat(MIN_SIDEBAR_WIDTH as usize));
        fs::write(dir.path().join(&long), NESTED_TEXT).expect("a file");
        workbench.scm.stage_path(&long).expect("staging");
        fs::write(dir.path().join(&long), NESTED_REWRITE).expect("a file");
        workbench.handle_key(key(keys::REFRESH.code));
        draw(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);

        let painted: Vec<String> = (0..workbench.scm.rows(Section::Unstaged).len())
            .map(|row| hovered_row(&mut workbench, Section::Unstaged, row))
            .collect();

        let cut = painted
            .iter()
            .find(|row| row.contains(ELLIPSIS))
            .expect("a row too long for the sidebar");
        assert!(cut.contains(STAGE_MARK), "{NO_CONTROL}: {cut:?}");
    }

    #[test]
    fn a_staged_row_offers_no_revert_having_nothing_unrecorded_to_lose() {
        let (_dir, mut workbench) = repository();
        rewritten(&mut workbench);

        let painted = hovered_row(&mut workbench, Section::Staged, 0);

        assert!(
            !painted.contains(REVERT_MARK),
            "{STRAY_CONTROL}: {painted:?}"
        );
    }

    #[test]
    fn the_revert_control_takes_two_clicks_and_restores_the_indexed_content() {
        let (_dir, mut workbench) = repository();
        let path = rewritten(&mut workbench);
        let at = control_on_row(&workbench, Section::Unstaged, 0, Control::Revert);

        workbench.handle_mouse(click(at.0, at.1));
        assert_eq!(
            fs::read_to_string(&path).expect("the file"),
            REWRITTEN_TEXT,
            "{DISCARD_UNARMED}"
        );

        workbench.handle_mouse(click(at.0, at.1));
        assert_eq!(
            fs::read_to_string(&path).expect("the file"),
            INDEXED_TEXT,
            "{DISCARD_UNARMED}"
        );
    }

    /// The row a section lists a folder on, since a folder and a file do not
    /// answer a revert the same way.
    fn folder_row(workbench: &Workbench, section: Section) -> usize {
        workbench
            .scm
            .rows(section)
            .iter()
            .position(|row| matches!(row, scm::Row::Directory(_)))
            .expect("a folder row")
    }

    /// Clicks the revert control of the folder the unstaged section lists,
    /// which is the press every bulk revert test starts from.
    fn revert_folder(workbench: &mut Workbench) {
        draw(workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let row = folder_row(workbench, Section::Unstaged);
        let at = control_on_row(workbench, Section::Unstaged, row, Control::Revert);
        workbench.handle_mouse(click(at.0, at.1));
    }

    fn answer_the_dialog(workbench: &mut Workbench, choice: Choice) {
        draw(workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let answers = workbench.panes.confirm;
        workbench.handle_mouse(click(answer_column(workbench, choice), answers.y));
    }

    fn nested_content(dir: &TempDir, name: &str) -> String {
        fs::read_to_string(dir.path().join(name)).expect("the file")
    }

    #[test]
    fn reverting_a_folder_asks_before_it_throws_anything_away() {
        let (dir, mut workbench) = nested_repository();

        revert_folder(&mut workbench);

        assert_eq!(asked(&workbench).ask, Ask::Revert, "{NOT_ASKED}");
        assert_eq!(
            nested_content(&dir, "src/a.txt"),
            NESTED_REWRITE,
            "{ASKED_TOO_LATE}"
        );
    }

    #[test]
    fn the_revert_dialog_says_how_many_files_it_covers() {
        let (_dir, mut workbench) = nested_repository();
        revert_folder(&mut workbench);

        let frame = draw(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);

        assert!(
            frame.contains("Discard changes to 2 files?"),
            "{NOT_PAINTED}"
        );
        for offered in Ask::Revert.answers() {
            assert!(frame.contains(offered.label(Ask::Revert)), "{NOT_PAINTED}");
        }
        assert!(!frame.contains(DISCARD_LABEL), "{NOT_PAINTED}");
    }

    #[test]
    fn answering_the_revert_dialog_throws_every_file_it_covers_away() {
        let (dir, mut workbench) = nested_repository();
        revert_folder(&mut workbench);

        answer_the_dialog(&mut workbench, Choice::Discard);

        assert_eq!(
            nested_content(&dir, "src/a.txt"),
            NESTED_TEXT,
            "{ANSWER_IGNORED}"
        );
        assert_eq!(
            nested_content(&dir, "src/b.txt"),
            NESTED_TEXT,
            "{ANSWER_IGNORED}"
        );
        assert_eq!(
            nested_content(&dir, "top.txt"),
            NESTED_REWRITE,
            "{OVER_REACHED}"
        );
        assert_eq!(workbench.confirm, None, "{ANSWER_IGNORED}");
    }

    #[test]
    fn cancelling_the_revert_dialog_leaves_every_file_alone() {
        let (dir, mut workbench) = nested_repository();
        revert_folder(&mut workbench);

        answer_the_dialog(&mut workbench, Choice::Cancel);

        assert_eq!(
            nested_content(&dir, "src/a.txt"),
            NESTED_REWRITE,
            "{ANSWER_IGNORED}"
        );
        assert_eq!(workbench.confirm, None, "{ANSWER_IGNORED}");
    }

    #[test]
    fn a_bulk_revert_brings_every_tab_it_rewrote_up_to_date() {
        let (dir, mut workbench) = nested_repository();
        workbench.open_path(&dir.path().join("src/a.txt"));
        workbench.open_path(&dir.path().join("src/b.txt"));
        revert_folder(&mut workbench);

        answer_the_dialog(&mut workbench, Choice::Discard);

        for tab in workbench.editor.tabs() {
            assert_ne!(tab.buffer.line(1), NESTED_ADDED_LINE, "{STALE_TAB}");
        }
    }

    #[test]
    fn reverting_a_section_asks_about_every_file_it_lists() {
        let (_dir, mut workbench) = repository();
        rewritten(&mut workbench);
        let at = control_on_header(&workbench, Section::Unstaged, Control::Revert);

        workbench.handle_mouse(click(at.0, at.1));
        let frame = draw(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);

        assert_eq!(asked(&workbench).ask, Ask::Revert, "{NOT_ASKED}");
        assert!(
            frame.contains("Discard changes to 1 file?"),
            "{NOT_PAINTED}"
        );
    }

    #[test]
    fn the_discard_key_on_a_staged_folder_asks_nothing() {
        let (_dir, mut workbench) = nested_repository();
        for name in ["src/a.txt", "src/b.txt"] {
            workbench.scm.stage_path(name).expect("staging");
        }
        draw(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let row = folder_row(&workbench, Section::Staged);
        workbench.scm.select(Section::Staged, Some(row));

        workbench.handle_key(press(keys::DISCARD));

        assert_eq!(workbench.confirm, None, "{STRAY_CONTROL}");
    }

    #[test]
    fn the_discard_key_on_a_folder_asks_what_a_click_on_it_asks() {
        let (_dir, mut workbench) = nested_repository();
        draw(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let row = folder_row(&workbench, Section::Unstaged);
        workbench.scm.select(Section::Unstaged, Some(row));

        workbench.handle_key(press(keys::DISCARD));

        assert_eq!(asked(&workbench).ask, Ask::Revert, "{NOT_ASKED}");
    }

    #[test]
    fn a_click_off_the_revert_control_cancels_it() {
        let (_dir, mut workbench) = repository();
        let path = rewritten(&mut workbench);
        let at = control_on_row(&workbench, Section::Unstaged, 0, Control::Revert);
        let header = header_of(&workbench, Section::Unstaged);

        workbench.handle_mouse(click(at.0, at.1));
        workbench.handle_mouse(click(header.x, header.y));
        workbench.handle_mouse(click(at.0, at.1));

        assert_eq!(
            fs::read_to_string(&path).expect("the file"),
            REWRITTEN_TEXT,
            "{DISCARD_UNARMED}"
        );
    }

    #[test]
    fn every_section_keeps_its_header_and_the_last_open_one_takes_the_rest() {
        let area = Rect::new(0, 0, 20, 20);
        let rects = layout_sections(area, [(4, false), (4, false), (4, false)]);

        assert_eq!(rects[0].header.height, 1, "{WRONG_GEOMETRY}");
        assert_eq!(rects[0].body.height, 4, "{WRONG_GEOMETRY}");
        assert_eq!(rects[1].body.height, 4, "{WRONG_GEOMETRY}");
        assert_eq!(rects[2].body.height, 9, "{WRONG_GEOMETRY}");
        assert_eq!(rects[2].body.bottom(), area.bottom(), "{WRONG_GEOMETRY}");
    }

    #[test]
    fn a_folded_section_gives_its_body_away_and_keeps_its_header() {
        let rects = layout_sections(Rect::new(0, 0, 20, 20), [(4, true), (4, false), (4, false)]);

        assert_eq!(rects[0].header.height, 1, "{WRONG_GEOMETRY}");
        assert_eq!(rects[0].body.height, 0, "{WRONG_GEOMETRY}");
        assert_eq!(rects[1].body.height, 4, "{WRONG_GEOMETRY}");
        assert_eq!(rects[2].body.height, 13, "{WRONG_GEOMETRY}");
    }

    #[test]
    fn the_sections_never_reach_past_the_room_they_were_given() {
        for height in 0..12u16 {
            let area = Rect::new(0, 0, 20, height);
            let rects = layout_sections(area, [(8, false), (8, false), (8, false)]);
            for rect in rects {
                assert!(rect.header.bottom() <= area.bottom(), "{WRONG_GEOMETRY}");
                assert!(rect.body.bottom() <= area.bottom(), "{WRONG_GEOMETRY}");
            }
        }
    }

    #[test]
    fn a_section_asking_for_more_than_there_is_leaves_the_ones_below_a_row() {
        let rects = layout_sections(
            Rect::new(0, 0, 20, 8),
            [(40, false), (4, false), (4, false)],
        );

        assert_eq!(rects[0].body.height, 3, "{WRONG_GEOMETRY}");
        assert_eq!(rects[1].body.height, MIN_SECTION_ROWS, "{WRONG_GEOMETRY}");
        assert_eq!(rects[2].body.height, MIN_SECTION_ROWS, "{WRONG_GEOMETRY}");
    }

    #[test]
    fn a_click_in_the_second_section_selects_that_section_and_not_the_first() {
        let (_dir, mut workbench) = nested_repository();
        workbench.scm.toggle_flat();
        paint(&mut workbench, 80, 24);
        let body = body_of(&workbench, Section::Unstaged);

        workbench.handle_mouse(click(body.x + 1, body.y + 1));

        assert_eq!(
            workbench.scm.cursor(),
            scm::Cursor {
                section: Section::Unstaged,
                row: Some(1)
            },
            "{WRONG_ROW}"
        );
    }

    #[test]
    fn a_press_and_release_on_a_header_folds_the_section() {
        let (_dir, mut workbench) = nested_repository();
        paint(&mut workbench, 80, 24);
        let header = header_of(&workbench, Section::Unstaged);

        workbench.handle_mouse(click(header.x + 2, header.y));
        workbench.handle_mouse(release(header.x + 2, header.y));

        assert!(workbench.scm.is_collapsed(Section::Unstaged), "{WRONG_ROW}");
    }

    #[test]
    fn a_press_that_drags_a_header_resizes_instead_of_folding() {
        let (_dir, mut workbench) = nested_repository();
        // The staged section has to have a body: an empty one is drawn folded,
        // and there is nothing above the border to resize.
        workbench.scm.stage_path("top.txt").expect("staging");
        workbench.handle_key(key(keys::REFRESH.code));
        paint(&mut workbench, 80, 24);
        let header = header_of(&workbench, Section::Unstaged);
        let before = workbench.scm.height(Section::Staged);

        workbench.handle_mouse(click(header.x + 2, header.y));
        workbench.handle_mouse(drag(header.x + 2, header.y + 3));
        workbench.handle_mouse(release(header.x + 2, header.y + 3));

        assert!(
            !workbench.scm.is_collapsed(Section::Unstaged),
            "{WRONG_ROW}"
        );
        assert_eq!(
            workbench.scm.height(Section::Staged),
            before + 3,
            "{WRONG_GEOMETRY}"
        );
    }

    #[test]
    fn the_wheel_scrolls_the_section_the_pointer_is_over() {
        let (dir, mut workbench) = nested_repository();
        for index in 0..20 {
            fs::write(dir.path().join(format!("f{index}.txt")), "one\n").expect("a file");
        }
        workbench.handle_key(key(keys::REFRESH.code));
        workbench.scm.set_height(Section::Unstaged, 4);
        paint(&mut workbench, 80, 24);
        let body = body_of(&workbench, Section::Unstaged);

        workbench.handle_mouse(wheel(body.x + 1, body.y + 1));

        assert_eq!(
            workbench.scm.scroll(Section::Unstaged),
            SCROLL_LINES as usize,
            "{WRONG_ROW}"
        );
        assert_eq!(workbench.scm.scroll(Section::Graph), 0, "{WRONG_ROW}");
    }

    #[test]
    fn a_click_on_a_folder_folds_it() {
        let (_dir, mut workbench) = nested_repository();
        paint(&mut workbench, 80, 24);
        let body = body_of(&workbench, Section::Unstaged);
        let before = workbench.scm.rows(Section::Unstaged).len();

        workbench.handle_mouse(click(body.x + 1, body.y));

        assert!(
            workbench.scm.rows(Section::Unstaged).len() < before,
            "{WRONG_ROW}"
        );
    }

    #[test]
    fn a_click_on_a_change_opens_its_diff() {
        let (_dir, mut workbench) = repository();
        paint(&mut workbench, 80, 24);
        let body = body_of(&workbench, Section::Unstaged);

        workbench.handle_mouse(click(body.x + 1, body.y));

        assert!(
            workbench
                .editor
                .active()
                .expect(NO_TAB)
                .diff_rows()
                .is_some(),
            "{WRONG_ROW}"
        );
    }

    /// The column the header's button landed on, found the same way the
    /// pointer finds it.
    fn header_button_column(workbench: &Workbench) -> u16 {
        let header = workbench.panes.header;
        let label = workbench.header_button().expect("a button in the header");
        (header.x..header.right())
            .find(|column| button_at(*column, header, workbench.header_context().width(), label))
            .expect("the button the header painted")
    }

    #[test_case(false ; "the button in the header")]
    #[test_case(true ; "and the key that does the same")]
    fn the_explorer_folds_back_to_its_top_level(by_key: bool) {
        let (_dir, mut workbench) = project();
        workbench.handle_key(key(KeyCode::Enter));
        paint(&mut workbench, 80, 24);
        assert!(workbench.tree.rows().iter().any(|row| row.depth > 0));

        match by_key {
            true => drop(workbench.handle_key(press(keys::COLLAPSE_ALL))),
            false => {
                let column = header_button_column(&workbench);
                workbench.handle_mouse(click(column, workbench.panes.header.y));
            }
        }

        assert!(
            workbench.tree.rows().iter().all(|row| row.depth == 0),
            "{STILL_UNFOLDED}"
        );
    }

    #[test]
    fn the_header_button_switches_between_tree_and_flat() {
        let (_dir, mut workbench) = nested_repository();
        paint(&mut workbench, 80, 24);
        let column = header_button_column(&workbench);

        workbench.handle_mouse(click(column, workbench.panes.header.y));

        assert!(workbench.scm.is_flat(), "{WRONG_PANE}");
    }

    #[test]
    fn a_stored_source_control_layout_survives_a_round_trip() {
        let (_dir, mut workbench) = nested_repository();
        workbench.scm.toggle_flat();
        workbench.scm.set_height(Section::Staged, 3);
        workbench.scm.toggle_collapsed(Section::Graph);

        let stored = workbench.layout();
        let mut restored = Workbench::new(WorkbenchStyles::default());
        restored.restore(stored.clone());

        assert_eq!(restored.layout().scm, stored.scm, "{LAYOUT_LOST}");
        assert!(restored.scm.is_flat(), "{LAYOUT_LOST}");
        assert_eq!(restored.scm.height(Section::Staged), 3, "{LAYOUT_LOST}");
        assert!(restored.scm.is_collapsed(Section::Graph), "{LAYOUT_LOST}");
    }

    #[test]
    fn a_stored_layout_naming_no_sections_keeps_the_defaults() {
        let stored = Layout {
            scm: ScmLayout {
                flat: true,
                sections: Vec::new(),
            },
            ..Layout::default()
        };
        let mut workbench = Workbench::new(WorkbenchStyles::default());

        workbench.restore(stored);

        assert!(workbench.scm.is_flat(), "{LAYOUT_LOST}");
        assert_eq!(
            workbench.scm.height(Section::Staged),
            scm::DEFAULT_SECTION_ROWS,
            "{LAYOUT_LOST}"
        );
    }

    #[test]
    fn enter_on_a_commit_lists_what_it_changed_and_opens_no_tab() {
        let (_dir, mut workbench) = repository();
        workbench.handle_key(key(keys::STAGE_TOGGLE.code));
        commit_all(&mut workbench);
        workbench.scm.select(Section::Graph, Some(0));

        workbench.handle_key(key(KeyCode::Enter));

        assert!(workbench.editor.active().is_none(), "{COMMIT_OPENED_WHOLE}");
        assert_eq!(
            workbench.scm.rows(Section::Graph).len(),
            2,
            "{COMMIT_NOT_LISTED}"
        );
        assert_eq!(
            workbench.scm.commit_file(0, 0).map(|file| &file.relative),
            Some(&OPENED_FILE.to_owned()),
            "{COMMIT_NOT_LISTED}"
        );
    }

    #[test]
    fn enter_on_a_path_under_a_commit_opens_that_path_alone() {
        let (_dir, mut workbench) = repository();
        workbench.handle_key(key(keys::STAGE_TOGGLE.code));
        commit_all(&mut workbench);
        workbench.scm.select(Section::Graph, Some(0));
        workbench.handle_key(key(KeyCode::Enter));

        workbench.scm.select(Section::Graph, Some(1));
        workbench.handle_key(key(KeyCode::Enter));

        let tab = workbench.editor.active().expect("a commit file tab");
        assert!(tab.diff_rows().is_some(), "{DIFF_EDITABLE}");
        assert!(!tab.is_editable(), "{DIFF_EDITABLE}");
        assert!(tab.title.starts_with(OPENED_FILE), "{COMMIT_NOT_LISTED}");
    }

    #[test]
    fn a_commit_folds_away_the_paths_it_listed() {
        let (_dir, mut workbench) = repository();
        workbench.handle_key(key(keys::STAGE_TOGGLE.code));
        commit_all(&mut workbench);
        workbench.scm.select(Section::Graph, Some(0));
        workbench.handle_key(key(KeyCode::Enter));

        workbench.handle_key(key(KeyCode::Enter));

        assert!(!workbench.scm.is_expanded(0), "{COMMIT_NOT_LISTED}");
        assert_eq!(
            workbench.scm.rows(Section::Graph).len(),
            1,
            "{COMMIT_NOT_LISTED}"
        );
    }

    /// A body of text with `D` on it, which the row never had the width for.
    #[test]
    fn the_diff_key_on_a_commit_opens_the_whole_message() {
        let (_dir, mut workbench) = repository();
        workbench.handle_key(key(keys::STAGE_TOGGLE.code));
        commit_message(&mut workbench, &format!("{SUBJECT}\n\n{BODY}"));
        workbench.scm.select(Section::Graph, Some(0));

        workbench.handle_key(press(keys::OPEN_DIFF));

        let tab = workbench.editor.active().expect("a commit detail tab");
        assert!(!tab.is_editable(), "{DIFF_EDITABLE}");
        let text = tab.buffer.lines().join("\n");
        assert!(text.contains(SUBJECT), "{MESSAGE_NOT_SHOWN}");
        assert!(text.contains(BODY), "{MESSAGE_NOT_SHOWN}");
        assert!(text.contains(OPENED_FILE), "{MESSAGE_NOT_SHOWN}");
    }

    /// `Enter` folds and `D` shows. Overloading one key with both would leave
    /// no way to read a commit without also rearranging the graph.
    #[test]
    fn enter_on_a_commit_still_folds_rather_than_opening_the_message() {
        let (_dir, mut workbench) = repository();
        workbench.handle_key(key(keys::STAGE_TOGGLE.code));
        commit_all(&mut workbench);
        workbench.scm.select(Section::Graph, Some(0));

        workbench.handle_key(key(KeyCode::Enter));

        assert!(workbench.editor.active().is_none(), "{COMMIT_OPENED_WHOLE}");
    }

    #[test]
    fn a_commit_detail_does_not_displace_the_diff_of_a_path_it_changed() {
        let (_dir, mut workbench) = repository();
        workbench.handle_key(key(keys::STAGE_TOGGLE.code));
        commit_all(&mut workbench);
        workbench.scm.select(Section::Graph, Some(0));
        workbench.handle_key(press(keys::OPEN_DIFF));

        // The detail tab took the focus with it, the way every opened tab does.
        workbench.handle_leader(press(keys::VIEW_SOURCE_CONTROL));
        workbench.scm.select(Section::Graph, Some(1));
        workbench.handle_key(key(KeyCode::Enter));

        assert_eq!(workbench.editor.tabs().len(), 2, "{DIFF_DISPLACED}");
    }

    #[test]
    fn a_commit_detail_tab_is_left_out_of_the_stored_layout() {
        let (_dir, mut workbench) = repository();
        workbench.handle_key(key(keys::STAGE_TOGGLE.code));
        commit_all(&mut workbench);
        workbench.scm.select(Section::Graph, Some(0));

        workbench.handle_key(press(keys::OPEN_DIFF));

        assert!(workbench.layout().tabs.is_empty(), "{LAYOUT_LOST}");
    }

    #[test]
    fn a_click_on_a_closed_commit_lists_its_paths_and_opens_its_message() {
        let (_dir, mut workbench) = repository();
        workbench.handle_key(key(keys::STAGE_TOGGLE.code));
        commit_message(&mut workbench, &format!("{SUBJECT}\n\n{BODY}"));
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let body = body_of(&workbench, Section::Graph);

        workbench.handle_mouse(click(body.x + 1, body.y));

        assert!(workbench.scm.is_expanded(0), "{COMMIT_NOT_LISTED}");
        let tab = workbench.editor.active().expect("a commit detail tab");
        assert!(
            tab.buffer.lines().join("\n").contains(BODY),
            "{MESSAGE_NOT_SHOWN}"
        );
    }

    /// Showing a commit reads its paths, and reading its paths is what opens
    /// it, so the second click has to close the commit rather than show it
    /// again and undo its own fold.
    #[test]
    fn a_second_click_on_a_commit_closes_it() {
        let (_dir, mut workbench) = repository();
        workbench.handle_key(key(keys::STAGE_TOGGLE.code));
        commit_all(&mut workbench);
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let body = body_of(&workbench, Section::Graph);
        workbench.handle_mouse(click(body.x + 1, body.y));
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);

        workbench.handle_mouse(click(body.x + 1, body.y));

        assert!(!workbench.scm.is_expanded(0), "{CLICK_REOPENED}");
        assert_eq!(workbench.editor.tabs().len(), 1, "{CLICK_REOPENED}");
    }

    #[test]
    fn the_diff_key_on_a_path_under_a_commit_still_opens_that_path() {
        let (_dir, mut workbench) = repository();
        workbench.handle_key(key(keys::STAGE_TOGGLE.code));
        commit_all(&mut workbench);
        workbench.scm.select(Section::Graph, Some(0));
        workbench.handle_key(key(KeyCode::Enter));
        workbench.scm.select(Section::Graph, Some(1));

        workbench.handle_key(press(keys::OPEN_DIFF));

        let tab = workbench.editor.active().expect("a commit file tab");
        assert!(tab.title.starts_with(OPENED_FILE), "{COMMIT_NOT_LISTED}");
    }

    #[test]
    fn a_commit_diff_does_not_displace_the_working_tree_diff_of_the_same_path() {
        let (_dir, mut workbench) = repository();
        workbench.handle_key(key(keys::STAGE_TOGGLE.code));
        workbench.scm.select(Section::Staged, Some(0));
        workbench.handle_key(key(KeyCode::Enter));
        commit_all(&mut workbench);

        workbench.handle_leader(press(keys::VIEW_SOURCE_CONTROL));
        workbench.scm.select(Section::Graph, Some(0));
        workbench.handle_key(key(KeyCode::Enter));
        workbench.scm.select(Section::Graph, Some(1));
        workbench.handle_key(key(KeyCode::Enter));

        assert_eq!(workbench.editor.tabs().len(), 2, "{DIFF_DISPLACED}");
    }

    #[test]
    fn the_graph_lists_the_commits_that_were_made() {
        let (_dir, mut workbench) = repository();
        workbench.handle_key(key(keys::STAGE_TOGGLE.code));
        commit_all(&mut workbench);

        assert_eq!(workbench.scm.count(Section::Graph), 1, "{CHANGE_MISSING}");
    }

    #[test]
    fn a_reference_from_source_control_names_the_changed_file() {
        let (_dir, mut workbench) = repository();
        let action = workbench.handle_leader(press(keys::SEND_TO_COMPOSER));
        assert_eq!(action, sent(OPENED_FILE, None), "{WRONG_REFERENCE}");
    }

    /// Types `text` into the focused field.
    fn type_query(workbench: &mut Workbench, text: &str) {
        for ch in text.chars() {
            workbench.handle_key(key(KeyCode::Char(ch)));
        }
    }

    /// Runs the search and drains the worker until it is done, so the assertions
    /// see the whole result set rather than whatever arrived first.
    fn search_for(workbench: &mut Workbench, text: &str) {
        workbench.handle_leader(press(keys::VIEW_SEARCH));
        type_query(workbench, text);
        workbench.handle_key(key(KeyCode::Enter));
        while workbench.is_busy() {
            workbench.tick();
        }
    }

    #[test]
    fn a_search_lists_its_hits_under_the_file_that_holds_them() {
        let (_dir, mut workbench) = project();
        search_for(&mut workbench, "two");

        assert_eq!(workbench.search.counts(), (1, 1), "{NO_HITS}");
        assert_eq!(
            workbench.search.rows(),
            &[search::Row::File(0), search::Row::Hit(0)],
            "{NO_HITS}"
        );
    }

    #[test]
    fn enter_on_a_hit_opens_the_file_at_that_line() {
        let (dir, mut workbench) = project();
        search_for(&mut workbench, "three");
        workbench.handle_key(key(KeyCode::Down));
        workbench.handle_key(key(KeyCode::Enter));

        let tab = workbench.editor.active().expect("a tab");
        assert_eq!(
            tab.path,
            WorkbenchPath::Local(dir.path().join("a.txt")),
            "{NO_TAB}"
        );
        assert_eq!(tab.buffer.cursor().line, 2, "{WRONG_LINE}");
    }

    #[test]
    fn an_include_glob_narrows_what_the_pane_lists() {
        let (_dir, mut workbench) = project();
        workbench.handle_leader(press(keys::VIEW_SEARCH));
        type_query(&mut workbench, "e");
        workbench.handle_leader(press(keys::NEXT_FIELD));
        type_query(&mut workbench, "*.md");
        workbench.handle_key(key(KeyCode::Enter));
        while workbench.is_busy() {
            workbench.tick();
        }

        assert_eq!(workbench.search.counts(), (0, 0), "{NO_HITS}");
    }

    #[test]
    fn a_toggle_makes_enter_search_again_rather_than_open() {
        let (_dir, mut workbench) = project();
        search_for(&mut workbench, "two");
        assert!(!workbench.search.is_stale(), "{STALE_RESULTS}");

        workbench.handle_leader(press(keys::TOGGLE_REGEX));
        assert!(workbench.search.is_stale(), "{STALE_RESULTS}");
    }

    #[test]
    fn a_broken_pattern_reaches_the_status_bar() {
        let (_dir, mut workbench) = project();
        workbench.handle_leader(press(keys::VIEW_SEARCH));
        workbench.handle_leader(press(keys::TOGGLE_REGEX));
        type_query(&mut workbench, "a(");
        workbench.handle_key(key(KeyCode::Enter));

        let (_, flash) = workbench.tick();
        assert!(flash.is_some(), "{NO_HITS}");
        assert!(!workbench.is_busy(), "{NO_HITS}");
    }

    #[test]
    fn a_reference_from_search_carries_the_line_it_found() {
        let (_dir, mut workbench) = project();
        search_for(&mut workbench, "three");
        workbench.handle_key(key(KeyCode::Down));

        assert_eq!(
            workbench.handle_leader(press(keys::SEND_TO_COMPOSER)),
            sent(OPENED_FILE, Some(3..=3)),
            "{WRONG_REFERENCE}"
        );
    }

    #[test]
    fn a_directory_outside_a_repository_says_so_rather_than_failing() {
        let (_dir, mut workbench) = project();
        workbench.handle_leader(press(keys::VIEW_SOURCE_CONTROL));
        assert!(!workbench.scm.is_repository(), "{WRONG_PANE}");
        assert!(
            draw(&mut workbench, 100, 20).contains(NOT_A_REPOSITORY),
            "{NOT_PAINTED}"
        );
    }

    #[test]
    fn a_write_underneath_a_clean_tab_is_taken_silently() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        let path = dir.path().join("a.txt");
        fs::write(&path, "rewritten\n").expect("a file");

        workbench.reload_tab(&path);

        let tab = workbench.editor.active().expect("a tab");
        assert_eq!(tab.buffer.line(0), "rewritten", "{STALE_TAB}");
        assert!(!tab.conflict, "{FALSE_CONFLICT}");
    }

    #[test]
    fn a_write_underneath_a_dirty_tab_raises_a_conflict_instead() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.handle_key(key(KeyCode::Char('!')));
        let path = dir.path().join("a.txt");
        fs::write(&path, "rewritten\n").expect("a file");

        workbench.reload_tab(&path);

        let tab = workbench.editor.active().expect("a tab");
        assert!(tab.conflict, "{NO_CONFLICT}");
        assert!(tab.is_dirty(), "{LOST_EDIT}");
        assert!(tab.buffer.line(0).starts_with('!'), "{LOST_EDIT}");
    }

    #[test]
    fn reverting_a_conflicted_tab_takes_what_is_on_disk() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.handle_key(key(KeyCode::Char('!')));
        let path = dir.path().join("a.txt");
        fs::write(&path, "rewritten\n").expect("a file");
        workbench.reload_tab(&path);

        workbench.handle_key(KeyEvent::new(keys::REVERT.code, keys::REVERT.modifiers));

        let tab = workbench.editor.active().expect("a tab");
        assert_eq!(tab.buffer.line(0), "rewritten", "{STALE_TAB}");
        assert!(!tab.conflict && !tab.is_dirty(), "{FALSE_CONFLICT}");
    }

    /// Writes `text` and moves the file's time well away from when it was
    /// read, so a check against that time cannot miss the write for landing in
    /// the same clock tick as the read.
    fn rewrite(path: &Path, text: &str) {
        fs::write(path, text).expect("a file");
        fs::File::options()
            .write(true)
            .open(path)
            .expect("the file")
            .set_modified(SystemTime::UNIX_EPOCH)
            .expect("a file time");
    }

    /// A file kept beside the project rather than in it, the way Caudra keeps
    /// its plans, so no watch on the project can see it change.
    fn outside_plan() -> (TempDir, PathBuf) {
        let state = TempDir::new().expect("a temporary directory");
        let plan = state.path().join(PLAN_FILE);
        fs::write(&plan, INDEXED_TEXT).expect("a plan");
        (state, plan)
    }

    /// Opens `plan` the way the host opens its own, under [`PLAN_TITLE`].
    fn open_plan(workbench: &mut Workbench, root: &Path, plan: &Path) {
        let label = TabLabel {
            title: PLAN_TITLE.to_owned(),
            status: PLAN_STATUS.to_owned(),
        };
        workbench
            .open_labelled(root, plan, label)
            .expect(LOCAL_OPEN);
    }

    /// Checks the tab took up [`REWRITTEN_TEXT`] when it was clean, and kept
    /// the edit and raised its conflict when it was not.
    fn assert_caught_up(tab: &Tab, dirty: bool) {
        match dirty {
            true => {
                assert!(tab.conflict, "{NO_CONFLICT}");
                assert!(tab.buffer.line(0).starts_with(EDIT), "{LOST_EDIT}");
            }
            false => {
                assert!(!tab.conflict, "{FALSE_CONFLICT}");
                assert_eq!(tab.buffer.line(0), REWRITTEN_TEXT.trim_end(), "{STALE_TAB}");
            }
        }
    }

    #[test_case(false ; "a clean tab rereads")]
    #[test_case(true ; "a dirty tab raises its conflict")]
    fn reopening_catches_up_with_a_write_made_while_closed(dirty: bool) {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        if dirty {
            workbench.handle_key(key(KeyCode::Char(EDIT)));
        }
        workbench.close();
        rewrite(&dir.path().join(OPENED_FILE), REWRITTEN_TEXT);

        workbench.open(dir.path());

        assert_caught_up(workbench.editor.active().expect(NO_TAB), dirty);
    }

    #[test]
    fn opening_an_open_file_again_shows_what_it_says_now() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        rewrite(&dir.path().join(OPENED_FILE), REWRITTEN_TEXT);

        workbench.open_path(&dir.path().join(OPENED_FILE));

        assert_caught_up(workbench.editor.active().expect(NO_TAB), false);
    }

    #[test_case(false ; "a clean tab rereads")]
    #[test_case(true ; "a dirty tab raises its conflict")]
    fn a_reported_write_outside_the_tree_reaches_its_tab(dirty: bool) {
        let (dir, mut workbench) = project();
        let (_state, plan) = outside_plan();
        open_plan(&mut workbench, dir.path(), &plan);
        if dirty {
            workbench.handle_key(key(KeyCode::Char(EDIT)));
        }
        fs::write(&plan, REWRITTEN_TEXT).expect("a plan");

        workbench.reload_paths([plan.as_path()]);

        assert_caught_up(workbench.editor.active().expect(NO_TAB), dirty);
        assert_eq!(workbench.has_unsaved(&plan), dirty, "{LOST_EDIT}");
    }

    #[test]
    fn a_labelled_tab_goes_by_its_label_and_no_other_tab_keeps_it() {
        let (dir, mut workbench) = project();
        let (state, plan) = outside_plan();
        open_plan(&mut workbench, dir.path(), &plan);

        let surface = paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        assert!(
            row_of(&surface, workbench.panes.tabs, 0).contains(PLAN_TITLE),
            "{NOT_LABELLED}"
        );
        assert!(
            row_of(&surface, workbench.panes.status, 0).contains(PLAN_STATUS),
            "{NOT_LABELLED}"
        );

        let newer = state.path().join(NEWER_PLAN_FILE);
        fs::write(&newer, INDEXED_TEXT).expect("a plan");
        open_plan(&mut workbench, dir.path(), &newer);
        let headings: Vec<&str> = workbench.editor.tabs().iter().map(Tab::heading).collect();
        assert_eq!(headings, [PLAN_FILE, PLAN_TITLE], "{LABEL_SHARED}");
    }

    #[test]
    fn coming_back_to_a_labelled_tab_keeps_the_reader_where_they_were() {
        let (dir, mut workbench) = project();
        let (_state, plan) = outside_plan();
        open_plan(&mut workbench, dir.path(), &plan);
        workbench.handle_key(key(KeyCode::Down));
        workbench.handle_key(key(KeyCode::Down));
        let cursor = workbench.editor.active().expect(NO_TAB).buffer.cursor();
        workbench.handle_leader(press(keys::VIEW_SOURCE_CONTROL));

        open_plan(&mut workbench, dir.path(), &plan);

        let tab = workbench.editor.active().expect(NO_TAB);
        assert_eq!(tab.buffer.cursor(), cursor, "{CURSOR_MOVED}");
        assert_eq!(
            workbench.sidebar,
            SidebarView::SourceControl,
            "{SIDEBAR_MOVED}"
        );
    }

    /// Stands in for the host's renderer, marking every row it paints so a
    /// frame shows which view drew it.
    fn painter(text: &str, _width: u16) -> PaintedMarkdown {
        let source = text.to_owned();
        let lines = text.lines().map(|line| Line::from(painted(line))).collect();
        PaintedMarkdown::new(lines, move |rows, start, end| {
            let last = rows.last()?;
            if start == (0, 0) && end == (rows.len() - 1, last.to_string().chars().count()) {
                return Some(source.clone());
            }
            let prefix = PAINTED.chars().count();
            let mut buffer = Buffer::new(source.lines().map(str::to_owned).collect());
            buffer.set_cursor(Cursor::new(start.0, start.1.saturating_sub(prefix)), false);
            buffer.set_cursor(Cursor::new(end.0, end.1.saturating_sub(prefix)), true);
            buffer.selected_text()
        })
    }

    /// A project holding one Markdown file, open in a workbench lent a painter.
    fn markdown_project(text: &str) -> (TempDir, Workbench) {
        let dir = TempDir::new().expect("a temporary directory");
        fs::write(dir.path().join(MARKDOWN_FILE), text).expect("a file");
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.set_markdown_painter(painter);
        workbench.open(dir.path());
        workbench.open_path(&dir.path().join(MARKDOWN_FILE));
        (dir, workbench)
    }

    /// Line `number` of [`tall_markdown`], counting from one.
    fn tall_line(number: usize) -> String {
        format!("line {number}")
    }

    fn tall_markdown() -> String {
        (1..=TALL_LINES)
            .map(|number| tall_line(number) + "\n")
            .collect()
    }

    fn toggle_rendered(workbench: &mut Workbench) -> WorkbenchAction {
        workbench.handle_leader(press(keys::TOGGLE_RENDERED))
    }

    fn rendered(workbench: &Workbench) -> bool {
        workbench.editor.active().expect(NO_TAB).is_rendered()
    }

    /// The text pane's first row, as a fresh frame paints it.
    fn first_row(workbench: &mut Workbench) -> String {
        let surface = paint(workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        row_of(&surface, workbench.panes.text, 0)
    }

    fn painted(line: &str) -> String {
        format!("{PAINTED}{line}")
    }

    #[test]
    fn the_chord_flips_a_markdown_tab_between_its_source_and_its_rendered_view() {
        let (_dir, mut workbench) = markdown_project(INDEXED_TEXT);

        assert_eq!(toggle_rendered(&mut workbench), WorkbenchAction::Consumed);
        let surface = paint(&mut workbench, WIDE_TERMINAL_WIDTH, TERMINAL_HEIGHT);
        assert!(
            row_of(&surface, workbench.panes.text, 0).starts_with(&painted(FIRST_LINE)),
            "{NOT_RENDERED}"
        );
        assert!(
            row_of(&surface, workbench.panes.status, 0).contains(RENDERED_STATUS),
            "{NOT_RENDERED}"
        );

        toggle_rendered(&mut workbench);
        assert!(
            first_row(&mut workbench).starts_with(FIRST_LINE),
            "{STILL_RENDERED}"
        );
    }

    #[test]
    fn the_chord_on_a_file_that_is_not_markdown_says_why() {
        let (dir, mut workbench) = markdown_project(INDEXED_TEXT);
        fs::write(dir.path().join(OPENED_FILE), INDEXED_TEXT).expect("a file");
        workbench.open_path(&dir.path().join(OPENED_FILE));

        assert_eq!(toggle_rendered(&mut workbench), WorkbenchAction::Consumed);

        assert!(!rendered(&workbench), "{NOT_REFUSED}");
        assert_eq!(
            workbench.flash.as_deref(),
            Some(NO_RENDERED_VIEW),
            "{NOT_REFUSED}"
        );
    }

    #[test_case(key(KeyCode::Char(EDIT)) ; "typing")]
    #[test_case(key(KeyCode::Enter) ; "a new line")]
    #[test_case(key(KeyCode::Backspace) ; "a deletion")]
    #[test_case(press(keys::UNDO) ; "undo")]
    #[test_case(press(keys::REDO) ; "redo")]
    #[test_case(press(keys::KILL_LINE) ; "a kill")]
    #[test_case(press(keys::CUT) ; "a cut")]
    #[test_case(press(keys::PASTE) ; "a paste")]
    fn the_rendered_view_refuses_what_would_edit_it(refused: KeyEvent) {
        let (_dir, mut workbench) = markdown_project(INDEXED_TEXT);
        workbench.clipboard = REWRITTEN_TEXT.to_owned();
        toggle_rendered(&mut workbench);
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        workbench.handle_key(press(keys::SELECT_ALL));
        let tab = workbench.editor.active().expect(NO_TAB);
        let before = (tab.contents(), tab.revision(), tab.buffer.cursor());

        assert_eq!(workbench.handle_key(refused), WorkbenchAction::Consumed);

        let tab = workbench.editor.active().expect(NO_TAB);
        assert_eq!(
            (tab.contents(), tab.revision(), tab.buffer.cursor()),
            before,
            "{EDITED_RENDERED}"
        );
        assert!(!tab.is_dirty(), "{EDITED_RENDERED}");
        assert!(!tab.buffer.has_selection(), "{EDITED_RENDERED}");
        assert_eq!(
            workbench.flash.as_deref(),
            Some(RENDERED_READ_ONLY),
            "{NOT_REFUSED}"
        );
    }

    #[test]
    fn find_goes_back_to_the_source_to_look() {
        let (_dir, mut workbench) = markdown_project(INDEXED_TEXT);
        toggle_rendered(&mut workbench);

        workbench.handle_key(press(keys::FIND));

        assert!(!rendered(&workbench), "{STILL_RENDERED}");
        assert!(workbench.editor.active().expect(NO_TAB).find.is_open());
    }

    #[test]
    fn a_write_underneath_repaints_the_rendered_view() {
        let (dir, mut workbench) = markdown_project(INDEXED_TEXT);
        toggle_rendered(&mut workbench);
        first_row(&mut workbench);
        let file = dir.path().join(MARKDOWN_FILE);
        fs::write(&file, REWRITTEN_TEXT).expect("a file");

        workbench.reload_paths([file.as_path()]);

        assert!(
            first_row(&mut workbench).starts_with(&painted(REWRITTEN_TEXT.trim_end())),
            "{STALE_TAB}"
        );
    }

    #[test]
    fn the_toggle_keeps_the_readers_place_both_ways() {
        let (_dir, mut workbench) = markdown_project(&tall_markdown());
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        workbench
            .editor
            .active_mut()
            .expect(NO_TAB)
            .set_scroll(READ_TO);

        toggle_rendered(&mut workbench);
        let line = tall_line(READ_TO + 1);
        assert!(
            first_row(&mut workbench).starts_with(&painted(&line)),
            "{PLACE_LOST}"
        );

        workbench.handle_key(key(KeyCode::Down));
        toggle_rendered(&mut workbench);
        let tab = workbench.editor.active().expect(NO_TAB);
        assert_eq!(tab.scroll(), READ_TO + 1, "{PLACE_LOST}");
    }

    #[test]
    fn the_rendered_view_scrolls_no_further_than_its_last_full_pane() {
        let (_dir, mut workbench) = markdown_project(&tall_markdown());
        toggle_rendered(&mut workbench);
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);

        workbench.handle_key(key(KeyCode::End));
        workbench.handle_key(key(KeyCode::Down));
        let text = workbench.panes.text;
        workbench.handle_mouse(wheel(text.x, text.y));

        let line = tall_line(TALL_LINES - text.height as usize + 1);
        assert!(
            first_row(&mut workbench).starts_with(&painted(&line)),
            "{OFF_THE_END}"
        );
    }

    #[test_case(false ; "forward")]
    #[test_case(true ; "backward")]
    fn a_drag_across_the_rendered_view_copies_source(reverse: bool) {
        let (_dir, mut workbench) = markdown_project(INDEXED_TEXT);
        toggle_rendered(&mut workbench);
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let text = workbench.panes.text;
        let prefix = PAINTED.width() as u16;
        let start = (text.x + prefix, text.y);
        let end = (text.x + prefix + 2, text.y + 1);
        let (start, end) = if reverse { (end, start) } else { (start, end) };
        let tab = workbench.editor.active().expect(NO_TAB);
        let before = (tab.contents(), tab.revision(), tab.buffer.cursor());

        workbench.handle_mouse(click(start.0, start.1));
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        workbench.handle_mouse(drag(end.0, end.1));
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let released = workbench.handle_mouse(release(end.0, end.1));

        assert_eq!(
            released,
            WorkbenchAction::Copy(RENDERED_COPIED.to_owned()),
            "{SELECTION_UNCOPIED}"
        );
        assert_eq!(workbench.clipboard, RENDERED_COPIED, "{SELECTION_UNCOPIED}");
        assert_eq!(
            workbench.handle_key(press(keys::COPY)),
            released,
            "{SELECTION_UNCOPIED}"
        );
        let tab = workbench.editor.active().expect(NO_TAB);
        assert!(!tab.buffer.has_selection(), "{EDITED_RENDERED}");
        assert_eq!(
            (tab.contents(), tab.revision(), tab.buffer.cursor()),
            before,
            "{EDITED_RENDERED}"
        );
    }

    #[test]
    fn select_all_in_rendered_view_copies_the_source_and_esc_clears_it() {
        let (_dir, mut workbench) = markdown_project(INDEXED_TEXT);
        toggle_rendered(&mut workbench);
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);

        workbench.handle_key(press(keys::SELECT_ALL));

        assert_eq!(
            workbench.handle_key(press(keys::COPY)),
            WorkbenchAction::Copy(INDEXED_TEXT.to_owned()),
            "{SELECTION_UNCOPIED}"
        );
        assert_eq!(
            workbench.handle_key(press(keys::CLOSE)),
            WorkbenchAction::Consumed,
            "{ESC_LEFT}"
        );
        assert_eq!(
            workbench.handle_key(press(keys::COPY)),
            WorkbenchAction::Passthrough,
            "{COPY_TRAPPED}"
        );
        assert_eq!(
            workbench.handle_key(press(keys::CLOSE)),
            WorkbenchAction::Close,
            "{ESC_LEFT}"
        );
    }

    #[test_case(2, FIRST_LINE ; "word")]
    #[test_case(3, FIELD_WORDS ; "row")]
    fn repeated_clicks_in_rendered_view_copy_the_word_or_row(clicks: usize, expected: &str) {
        let (_dir, mut workbench) = markdown_project(RENDERED_WORDS);
        toggle_rendered(&mut workbench);
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let text = workbench.panes.text;
        let column = text.x + PAINTED.width() as u16 + 1;

        for _ in 0..clicks {
            workbench.handle_mouse(click(column, text.y));
        }

        assert_eq!(
            workbench.handle_mouse(release(column, text.y)),
            WorkbenchAction::Copy(expected.to_owned()),
            "{SELECTION_UNCOPIED}"
        );
    }

    #[test]
    fn a_plain_rendered_click_clears_selection_without_copying() {
        let (_dir, mut workbench) = markdown_project(INDEXED_TEXT);
        toggle_rendered(&mut workbench);
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        workbench.handle_key(press(keys::SELECT_ALL));
        workbench.clipboard = KEPT_CLIPBOARD.to_owned();
        let text = workbench.panes.text;

        workbench.handle_mouse(click(text.x, text.y));

        assert_eq!(
            workbench.handle_mouse(release(text.x, text.y)),
            WorkbenchAction::Consumed,
            "{IDLE_CLICK_COPIED}"
        );
        assert!(workbench.selected_text().is_none(), "{IDLE_CLICK_COPIED}");
        assert_eq!(workbench.clipboard, KEPT_CLIPBOARD, "{IDLE_CLICK_COPIED}");
    }

    #[test]
    fn the_leader_cut_cannot_edit_a_rendered_selection() {
        let (_dir, mut workbench) = markdown_project(INDEXED_TEXT);
        toggle_rendered(&mut workbench);
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        workbench.handle_key(press(keys::SELECT_ALL));
        let tab = workbench.editor.active().expect(NO_TAB);
        let before = (tab.contents(), tab.revision(), tab.buffer.cursor());

        assert_eq!(
            workbench.handle_leader(press(keys::CUT_CHORD)),
            WorkbenchAction::Consumed
        );

        let tab = workbench.editor.active().expect(NO_TAB);
        assert_eq!(
            (tab.contents(), tab.revision(), tab.buffer.cursor()),
            before,
            "{EDITED_RENDERED}"
        );
        assert!(!tab.is_dirty(), "{EDITED_RENDERED}");
        assert_eq!(
            workbench.flash.as_deref(),
            Some(RENDERED_READ_ONLY),
            "{NOT_REFUSED}"
        );
    }

    #[test]
    fn a_rendered_drag_scrolls_and_extends_outside_the_pane() {
        let (_dir, mut workbench) = markdown_project(&tall_markdown());
        toggle_rendered(&mut workbench);
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let text = workbench.panes.text;
        workbench.handle_mouse(click(text.x, text.y));
        workbench.handle_mouse(drag(text.right() - 1, text.bottom()));
        let before = workbench.selected_text().expect(SELECTION_UNCOPIED);

        assert!(workbench.is_busy(), "{WRONG_CLICK}");
        let (changed, _) = workbench.tick();

        assert!(changed, "{WRONG_CLICK}");
        let after = workbench.selected_text().expect(SELECTION_UNCOPIED);
        assert!(after.len() > before.len(), "{SELECTION_UNCOPIED}");
        assert_eq!(
            workbench.handle_mouse(release(text.right() - 1, text.bottom())),
            WorkbenchAction::Copy(after),
            "{SELECTION_UNCOPIED}"
        );
    }

    #[test]
    fn a_rendered_selection_is_invalidated_before_a_reloaded_file_is_painted() {
        let (dir, mut workbench) = markdown_project(INDEXED_TEXT);
        toggle_rendered(&mut workbench);
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        workbench.handle_key(press(keys::SELECT_ALL));
        let file = dir.path().join(MARKDOWN_FILE);
        fs::write(&file, REWRITTEN_TEXT).expect(STALE_TAB);

        workbench.reload_paths([file.as_path()]);

        assert!(workbench.selected_text().is_none(), "{SELECTION_STALE}");
        assert_eq!(
            workbench.handle_key(press(keys::COPY)),
            WorkbenchAction::Passthrough,
            "{COPY_TRAPPED}"
        );
    }

    #[test]
    fn rendered_selection_survives_theme_changes_but_not_reflow() {
        let (_dir, mut workbench) = markdown_project(INDEXED_TEXT);
        toggle_rendered(&mut workbench);
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        workbench.handle_key(press(keys::SELECT_ALL));

        workbench.set_styles(WorkbenchStyles::default());
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);

        assert_eq!(
            workbench.selected_text().as_deref(),
            Some(INDEXED_TEXT),
            "{SELECTION_UNCOPIED}"
        );
        paint(&mut workbench, WIDE_TERMINAL_WIDTH, TERMINAL_HEIGHT);
        assert!(workbench.selected_text().is_none(), "{SELECTION_STALE}");
    }

    #[test]
    fn selecting_all_before_the_first_rendered_frame_uses_current_contents() {
        let (_dir, mut workbench) = markdown_project(INDEXED_TEXT);
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        toggle_rendered(&mut workbench);

        workbench.handle_key(press(keys::SELECT_ALL));
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);

        assert_eq!(
            workbench.selected_text().as_deref(),
            Some(INDEXED_TEXT),
            "{SELECTION_UNCOPIED}"
        );
    }

    #[test]
    fn replacing_a_rendered_document_cancels_its_offscreen_drag() {
        let (_dir, mut workbench) = markdown_project(INDEXED_TEXT);
        let document = DocumentKey(DRAFT_KEY.to_owned());
        workbench.open_document(
            document.clone(),
            TabLabel {
                title: DRAFT_TITLE.to_owned(),
                status: DRAFT_STATUS.to_owned(),
            },
            &tall_markdown(),
        );
        toggle_rendered(&mut workbench);
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let text = workbench.panes.text;
        workbench.handle_mouse(click(text.x, text.y));
        workbench.handle_mouse(drag(text.x, text.bottom()));

        assert!(
            workbench.replace_document(&document, NEWER_DRAFT),
            "{STALE_DOCUMENT}"
        );
        workbench.tick();

        assert_eq!(workbench.drag, Drag::None, "{SELECTION_STALE}");
        assert!(workbench.selected_text().is_none(), "{SELECTION_STALE}");
        assert_eq!(
            workbench.handle_mouse(release(text.x, text.bottom())),
            WorkbenchAction::Consumed,
            "{IDLE_CLICK_COPIED}"
        );
    }

    #[test]
    fn changing_tabs_during_a_rendered_drag_never_copies_another_tabs_selection() {
        let (dir, mut workbench) = markdown_project(INDEXED_TEXT);
        fs::write(dir.path().join(NEWER_PLAN_FILE), REWRITTEN_TEXT).expect(NO_TAB);
        workbench.open_path(&dir.path().join(NEWER_PLAN_FILE));
        toggle_rendered(&mut workbench);
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        workbench.handle_key(press(keys::SELECT_ALL));
        workbench.handle_key(press(keys::PREV_TAB));
        toggle_rendered(&mut workbench);
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let text = workbench.panes.text;
        workbench.handle_mouse(click(text.x, text.y));
        workbench.handle_mouse(drag(text.right() - 1, text.y));

        workbench.handle_key(press(keys::NEXT_TAB));
        let action = workbench.handle_mouse(release(text.right() - 1, text.y));

        assert_eq!(action, WorkbenchAction::Consumed, "{IDLE_CLICK_COPIED}");
        assert_eq!(
            workbench.selected_text().as_deref(),
            Some(REWRITTEN_TEXT),
            "{SELECTION_UNCOPIED}"
        );
    }

    #[test]
    fn a_rendered_selection_is_highlighted_after_scrolling() {
        let (_dir, mut workbench) = markdown_project(&tall_markdown());
        workbench.styles.selection = Style::new().bg(SELECTION_COLOUR);
        toggle_rendered(&mut workbench);
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        workbench.handle_key(press(keys::SELECT_ALL));
        workbench.handle_key(key(KeyCode::PageDown));

        let surface = paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);

        let text = workbench.panes.text;
        assert_eq!(
            surface[(text.x, text.y)].bg,
            SELECTION_COLOUR,
            "{SELECTION_UNPAINTED}"
        );
        assert_eq!(
            workbench.selected_text().as_deref(),
            Some(tall_markdown().as_str()),
            "{SELECTION_UNCOPIED}"
        );
    }

    #[test]
    fn an_off_pane_rendered_drag_includes_the_rightmost_character() {
        let (_dir, mut workbench) = markdown_project(INDEXED_TEXT);
        toggle_rendered(&mut workbench);
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let width = workbench.panes.text.width as usize;
        let source: String = FIRST_LINE
            .repeat(width)
            .chars()
            .take(width - PAINTED.width())
            .collect();
        workbench
            .editor
            .active_mut()
            .expect(NO_TAB)
            .replace_text(&source);
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let text = workbench.panes.text;
        workbench.handle_mouse(click(text.x, text.y));
        workbench.handle_mouse(drag(text.right(), text.y));

        assert_eq!(
            workbench.handle_mouse(release(text.right(), text.y)),
            WorkbenchAction::Copy(source),
            "{SELECTION_UNCOPIED}"
        );
    }

    #[test_case(false ; "commit details")]
    #[test_case(true ; "remote diff")]
    fn an_async_scm_tab_cancels_the_previous_rendered_drag(diff: bool) {
        let (_dir, mut workbench) = markdown_project(INDEXED_TEXT);
        toggle_rendered(&mut workbench);
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let text = workbench.panes.text;
        workbench.handle_mouse(click(text.x, text.y));
        workbench.handle_mouse(drag(text.right() - 1, text.y));

        if diff {
            workbench.open_remote_diff(DiffResult {
                path: WorkspacePath::new(MARKDOWN_FILE).expect(NO_TAB),
                target: ScmDiffTarget::Unstaged,
                lines: Vec::new(),
                old: INDEXED_TEXT.to_owned(),
                new: REWRITTEN_TEXT.to_owned(),
                warnings: Vec::new(),
            });
        } else {
            workbench.push_commit_detail(&Commit {
                id: INITIAL_MESSAGE.to_owned(),
                summary: SUBJECT.to_owned(),
                body: None,
                author: FIRST_LINE.to_owned(),
                email: String::new(),
                committed: 0,
                parents: Vec::new(),
            });
        }

        assert_eq!(workbench.drag, Drag::None, "{SELECTION_STALE}");
        assert_eq!(
            workbench.handle_mouse(release(text.right() - 1, text.y)),
            WorkbenchAction::Consumed,
            "{IDLE_CLICK_COPIED}"
        );
    }

    #[test]
    fn rendered_selection_highlights_wide_and_combining_characters() {
        let (_dir, mut workbench) =
            markdown_project(&format!("{WIDE_GLYPH}{COMBINED_GLYPH}{SHORT_LINE}\n"));
        workbench.styles.selection = Style::new().bg(SELECTION_COLOUR);
        toggle_rendered(&mut workbench);
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let text = workbench.panes.text;
        let wide = text.x + PAINTED.width() as u16;
        let combined = wide + WIDE_GLYPH.width() as u16;
        let after = combined + COMBINED_GLYPH.width() as u16;
        workbench.handle_mouse(click(wide, text.y));
        workbench.handle_mouse(drag(after, text.y));

        let surface = paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);

        for column in [wide, combined] {
            assert_eq!(
                surface[(column, text.y)].bg,
                SELECTION_COLOUR,
                "{SELECTION_UNPAINTED}"
            );
        }
        assert_ne!(
            surface[(after, text.y)].bg,
            SELECTION_COLOUR,
            "{SELECTION_UNPAINTED}"
        );
        assert_eq!(
            workbench.selected_text(),
            Some(format!("{WIDE_GLYPH}{COMBINED_GLYPH}")),
            "{SELECTION_UNCOPIED}"
        );
    }

    #[test]
    fn the_tab_menu_shows_the_view_that_is_hidden() {
        let (_dir, mut workbench) = markdown_project(INDEXED_TEXT);
        workbench.handle_leader(press(keys::MENU));

        menu_action(&mut workbench, MenuAction::ShowRendered);
        assert!(rendered(&workbench), "{NOT_RENDERED}");

        workbench.handle_leader(press(keys::MENU));
        menu_action(&mut workbench, MenuAction::ShowSource);
        assert!(!rendered(&workbench), "{STILL_RENDERED}");
    }

    #[test]
    fn without_a_painter_there_is_no_rendered_view() {
        let (dir, mut workbench) = project();
        let file = dir.path().join(MARKDOWN_FILE);
        fs::write(&file, INDEXED_TEXT).expect("a file");
        workbench.open_path(&file);

        assert_eq!(
            toggle_rendered(&mut workbench),
            WorkbenchAction::Passthrough,
            "{CHORD_TAKEN}"
        );
        workbench.handle_leader(press(keys::MENU));
        let menu = workbench.menu.as_ref().expect(NO_MENU);
        assert!(
            !menu
                .items()
                .contains(&MenuItem::Action(MenuAction::ShowRendered)),
            "{CHORD_TAKEN}"
        );
    }

    fn draft_key() -> DocumentKey {
        DocumentKey(DRAFT_KEY.to_owned())
    }

    /// Opens `text` the way the host opens a prompt draft.
    fn open_draft(workbench: &mut Workbench, text: &str) {
        let label = TabLabel {
            title: DRAFT_TITLE.to_owned(),
            status: DRAFT_STATUS.to_owned(),
        };
        workbench.open_document(draft_key(), label, text);
    }

    fn draft(workbench: &Workbench) -> Option<&Tab> {
        let index = workbench.editor.document(&draft_key())?;
        workbench.editor.tabs().get(index)
    }

    /// [`DRAFT_TEXT`] after the one keystroke of [`EDIT`].
    fn edited_draft() -> String {
        format!("{EDIT}{DRAFT_TEXT}")
    }

    fn save_chord(workbench: &mut Workbench) -> WorkbenchAction {
        workbench.handle_key(press(keys::SAVE))
    }

    fn menu_save(workbench: &mut Workbench) -> WorkbenchAction {
        workbench.handle_leader(press(keys::MENU));
        menu_action(workbench, MenuAction::Save)
    }

    fn send_chord(workbench: &mut Workbench) -> WorkbenchAction {
        workbench.handle_leader(press(keys::SEND_TO_COMPOSER))
    }

    #[test_case(save_chord, false ; "the save chord")]
    #[test_case(menu_save, false ; "the menu")]
    #[test_case(send_chord, true ; "the send chord, which also leaves")]
    fn saving_a_document_hands_its_text_back_and_waits_for_the_host(
        save: fn(&mut Workbench) -> WorkbenchAction,
        close: bool,
    ) {
        let (_dir, mut workbench) = project();
        open_draft(&mut workbench, DRAFT_TEXT);
        workbench.handle_key(key(KeyCode::Char(EDIT)));

        let action = save(&mut workbench);

        let expected = WorkbenchAction::SaveDocument {
            key: draft_key(),
            text: edited_draft(),
            close,
        };
        assert_eq!(action, expected, "{NOT_HANDED_BACK}");
        assert!(
            draft(&workbench).expect(NO_TAB).is_dirty(),
            "{SAVED_TOO_SOON}"
        );
        workbench.document_saved(&draft_key());
        assert!(
            !draft(&workbench).expect(NO_TAB).is_dirty(),
            "{SAVED_TOO_SOON}"
        );
    }

    #[test]
    fn closing_an_unsaved_document_asks_and_its_save_hands_the_text_back() {
        let (_dir, mut workbench) = project();
        open_draft(&mut workbench, DRAFT_TEXT);
        workbench.handle_key(key(KeyCode::Char(EDIT)));

        workbench.handle_leader(press(keys::CLOSE_TAB));
        assert_eq!(answer(&workbench), Some(Choice::Save), "{NOT_ASKED}");
        let action = workbench.handle_key(key(KeyCode::Enter));

        let expected = WorkbenchAction::SaveDocument {
            key: draft_key(),
            text: edited_draft(),
            close: false,
        };
        assert_eq!(action, expected, "{NOT_HANDED_BACK}");
        assert!(draft(&workbench).is_some(), "{DOCUMENT_LOST}");
    }

    #[test_case(false ; "a clean tab takes the host's text")]
    #[test_case(true ; "an unsaved tab keeps what was typed")]
    fn opening_a_document_again_raises_its_one_tab(dirty: bool) {
        let (_dir, mut workbench) = project();
        open_draft(&mut workbench, DRAFT_TEXT);
        if dirty {
            workbench.handle_key(key(KeyCode::Char(EDIT)));
        }

        open_draft(&mut workbench, NEWER_DRAFT);

        assert_eq!(workbench.editor.tabs().len(), 1, "{WRONG_TABS}");
        let tab = workbench.editor.active().expect(NO_TAB);
        assert_eq!(tab.heading(), DRAFT_TITLE, "{NOT_LABELLED}");
        let expected = match dirty {
            true => edited_draft(),
            false => NEWER_DRAFT.to_owned(),
        };
        assert_eq!(tab.contents(), expected, "{STALE_DOCUMENT}");
    }

    #[test]
    fn a_document_is_left_out_of_the_stored_layout() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        open_draft(&mut workbench, DRAFT_TEXT);

        let layout = workbench.layout();

        assert_eq!(
            layout.tabs,
            [dir.path().join(OPENED_FILE)],
            "{DOCUMENT_STORED}"
        );
    }

    #[test]
    fn a_document_offers_nothing_that_needs_a_file_behind_it() {
        let (_dir, mut workbench) = project();
        open_draft(&mut workbench, DRAFT_TEXT);
        workbench.handle_key(key(KeyCode::Char(EDIT)));

        workbench.handle_leader(press(keys::MENU));
        let items = workbench.menu.as_ref().expect(NO_MENU).items();
        assert!(
            items.contains(&MenuItem::Action(MenuAction::Save)),
            "{WRONG_TARGET}"
        );
        for named in [
            MenuAction::CopyPath,
            MenuAction::CopyRelative,
            MenuAction::RevealInExplorer,
        ] {
            assert!(
                !items.contains(&MenuItem::Action(named)),
                "{FILE_ITEMS}: {named:?}"
            );
        }
    }

    /// A document's other copy is the host's, so the key that takes the
    /// other writer's copy of a file asks the host for it instead.
    #[test]
    fn reverting_a_document_takes_the_copy_the_host_hands_over() {
        let (_dir, mut workbench) = project();
        open_draft(&mut workbench, DRAFT_TEXT);
        workbench.handle_key(key(KeyCode::Char(EDIT)));

        let action = workbench.handle_key(press(keys::REVERT));

        assert_eq!(
            action,
            WorkbenchAction::RevertDocument(draft_key()),
            "{NOT_ASKED_BACK}"
        );
        assert!(
            draft(&workbench).expect(NO_TAB).is_dirty(),
            "{SAVED_TOO_SOON}"
        );
        workbench.revert_document(&draft_key(), NEWER_DRAFT);
        let tab = draft(&workbench).expect(NO_TAB);
        assert_eq!(tab.contents(), NEWER_DRAFT, "{STALE_DOCUMENT}");
        assert!(!tab.is_dirty() && !tab.conflict, "{FALSE_CONFLICT}");
    }

    /// The host's copy moving on is a write underneath the tab, and it is
    /// treated the way a watched file treats one.
    #[test_case(false ; "a clean tab takes the new copy")]
    #[test_case(true ; "an unsaved tab keeps its edits and flies the conflict")]
    fn a_newer_host_copy_reaches_a_document_as_a_write_reaches_a_file(dirty: bool) {
        let (_dir, mut workbench) = project();
        open_draft(&mut workbench, DRAFT_TEXT);
        if dirty {
            workbench.handle_key(key(KeyCode::Char(EDIT)));
        }

        let took = workbench.replace_document(&draft_key(), NEWER_DRAFT);

        let tab = draft(&workbench).expect(NO_TAB);
        let expected = match dirty {
            true => edited_draft(),
            false => NEWER_DRAFT.to_owned(),
        };
        assert_eq!(tab.contents(), expected, "{STALE_DOCUMENT}");
        assert_eq!(took, !dirty, "{STALE_DOCUMENT}");
        assert_eq!(tab.conflict, dirty, "{NO_CONFLICT}");
        assert_eq!(
            workbench.has_unsaved_document(&draft_key()),
            dirty,
            "{DOCUMENT_LOST}"
        );
    }

    #[test]
    fn a_document_reads_as_markdown() {
        let (_dir, mut workbench) = markdown_project(INDEXED_TEXT);
        open_draft(&mut workbench, DRAFT_TEXT);

        toggle_rendered(&mut workbench);

        assert!(rendered(&workbench), "{NOT_RENDERED}");
    }

    #[test_case(false ; "a clean document goes")]
    #[test_case(true ; "an unsaved one stays")]
    fn closing_the_workbench_keeps_only_documents_with_work_in_them(dirty: bool) {
        let (_dir, mut workbench) = project();
        open_draft(&mut workbench, DRAFT_TEXT);
        if dirty {
            workbench.handle_key(key(KeyCode::Char(EDIT)));
        }

        workbench.close();

        assert_eq!(draft(&workbench).is_some(), dirty, "{DOCUMENT_LOST}");
    }

    #[test]
    fn a_refresh_in_a_workspace_keeps_the_hosts_document() {
        let (session, _control) = crate::fs::backend::tests::widget_fixture();
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.toggle_workspace(session).unwrap();
        settle_remote(&mut workbench, |workbench| !workbench.is_busy());
        open_draft(&mut workbench, DRAFT_TEXT);

        workbench.handle_key(press(keys::REFRESH));
        settle_remote(&mut workbench, |workbench| !workbench.is_busy());

        assert!(draft(&workbench).is_some(), "{DOCUMENT_LOST}");
    }

    #[test]
    fn a_watched_file_is_marked_in_the_explorer() {
        let (dir, mut workbench) = project();
        workbench.touched.insert(dir.path().join("a.txt"));
        workbench.apply_marks();

        let marked = workbench
            .tree
            .rows()
            .iter()
            .filter(|row| row.agent_touched)
            .count();
        assert_eq!(marked, 1, "{MARK_MISSING}");
    }

    #[test_case(keys::GROW_SIDEBAR.code, 1 ; "grow")]
    #[test_case(keys::SHRINK_SIDEBAR.code, -1 ; "shrink")]
    fn a_resize_key_moves_the_sidebar_edge(code: KeyCode, direction: i16) {
        let mut workbench = workbench();
        let before = workbench.sidebar_width;
        workbench.handle_leader(KeyEvent::new(code, KeyModifiers::NONE));
        let moved = i32::from(workbench.sidebar_width) - i32::from(before);
        assert_eq!(moved.signum(), i32::from(direction), "{WRONG_WIDTH}");
    }

    #[test]
    fn the_sidebar_never_resizes_past_its_bounds() {
        let mut workbench = workbench();
        for _ in 0..MAX_SIDEBAR_WIDTH {
            workbench.handle_leader(press(keys::GROW_SIDEBAR));
        }
        assert_eq!(workbench.sidebar_width, MAX_SIDEBAR_WIDTH, "{WRONG_WIDTH}");

        for _ in 0..MAX_SIDEBAR_WIDTH {
            workbench.handle_leader(press(keys::SHRINK_SIDEBAR));
        }
        assert_eq!(workbench.sidebar_width, MIN_SIDEBAR_WIDTH, "{WRONG_WIDTH}");
    }

    #[test]
    fn dragging_the_separator_sets_the_sidebar_width() {
        let (_dir, mut workbench) = project();
        draw(&mut workbench, 100, 20);
        let separator = workbench.panes.separator.expect("a separator");

        workbench.handle_mouse(click(separator.x, separator.y));
        workbench.handle_mouse(drag(separator.x + 10, separator.y));

        assert_eq!(workbench.sidebar_width, separator.x + 10, "{WRONG_WIDTH}");
    }

    #[test]
    fn a_layout_round_trips_through_a_fresh_workbench() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.handle_leader(press(keys::VIEW_SEARCH));
        workbench.handle_leader(press(keys::GROW_SIDEBAR));
        let saved = workbench.layout();

        let mut restored = Workbench::new(WorkbenchStyles::default());
        restored.open(dir.path());
        restored.restore(saved.clone());

        assert_eq!(restored.layout(), saved, "{WRONG_LAYOUT}");
        assert_eq!(
            restored.editor.active().expect("a tab").path,
            WorkbenchPath::Local(dir.path().join("a.txt")),
            "{WRONG_LAYOUT}"
        );
    }

    #[test]
    fn a_stored_tab_whose_file_is_gone_is_skipped_rather_than_flashed() {
        let (dir, mut workbench) = project();
        let layout = super::Layout {
            tabs: vec![dir.path().join("a.txt"), dir.path().join("vanished.txt")],
            ..super::Layout::default()
        };

        workbench.restore(layout);

        assert_eq!(workbench.editor.tabs().len(), 1, "{WRONG_LAYOUT}");
        assert!(workbench.flash.is_none(), "{WRONG_LAYOUT}");
    }

    #[test]
    fn restoring_nothing_leaves_the_workbench_at_its_defaults() {
        let (_dir, mut workbench) = project();
        let fresh = workbench.layout();

        workbench.restore(super::Layout::default());

        assert_eq!(workbench.layout(), fresh, "{WRONG_LAYOUT}");
    }

    #[test]
    fn a_diff_tab_is_left_out_of_the_stored_layout() {
        let (_dir, mut workbench) = repository();
        workbench.handle_key(key(keys::OPEN_DIFF.code));
        assert!(
            workbench
                .editor
                .active()
                .expect("a tab")
                .diff_rows()
                .is_some(),
            "{WRONG_LAYOUT}"
        );

        assert!(workbench.layout().tabs.is_empty(), "{WRONG_LAYOUT}");
    }

    #[test]
    fn a_press_on_the_separator_does_not_move_the_selection() {
        let (_dir, mut workbench) = project();
        draw(&mut workbench, 100, 20);
        let separator = workbench.panes.separator.expect("a separator");
        let before = workbench.tree.selected_index();

        workbench.handle_mouse(click(separator.x, separator.y + 3));

        assert_eq!(workbench.tree.selected_index(), before, "{WRONG_CLICK}");
        workbench.handle_mouse(release(separator.x, separator.y));
        assert_eq!(workbench.drag, Drag::None, "{WRONG_CLICK}");
    }

    #[test]
    fn a_file_opens_on_the_first_click() {
        let (_dir, mut workbench) = project();
        draw(&mut workbench, 80, 24);
        let rows = workbench.panes.rows;

        workbench.handle_mouse(click(row_body(rows), rows.y + 1));

        assert_eq!(
            workbench.editor.active().expect(NO_TAB).title,
            "a.txt",
            "{WRONG_CLICK}"
        );
    }

    #[test]
    fn a_press_under_the_last_row_opens_nothing() {
        let (_dir, mut workbench) = project();
        draw(&mut workbench, 80, 24);
        let rows = workbench.panes.rows;
        let below = rows.y + workbench.tree.rows().len() as u16;

        workbench.handle_mouse(click(rows.x + 1, below));

        assert!(workbench.editor.active().is_none(), "{WRONG_CLICK}");
    }

    #[test]
    fn a_directory_opens_on_the_first_click() {
        let (_dir, mut workbench) = project();
        draw(&mut workbench, 80, 24);
        let rows = workbench.panes.rows;
        let before = workbench.tree.rows().len();

        workbench.handle_mouse(click(row_body(rows), rows.y));

        assert!(workbench.tree.rows().len() > before, "{WRONG_CLICK}");
    }

    #[test]
    fn dragging_across_the_buffer_selects_what_it_crossed() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        draw(&mut workbench, 80, 24);
        let text = workbench.panes.text;

        workbench.handle_mouse(click(text.x, text.y));
        workbench.handle_mouse(drag(text.x + 2, text.y + 1));
        workbench.handle_mouse(release(text.x + 2, text.y + 1));

        assert_eq!(
            workbench.selected_text().as_deref(),
            Some("one\ntw"),
            "{WRONG_CLICK}"
        );
        assert_eq!(workbench.drag, Drag::None, "{WRONG_CLICK}");
    }

    #[test]
    fn releasing_a_drag_hands_the_selection_to_the_clipboard() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        draw(&mut workbench, 80, 24);
        let text = workbench.panes.text;

        workbench.handle_mouse(click(text.x, text.y));
        workbench.handle_mouse(drag(text.x + 2, text.y + 1));
        let action = workbench.handle_mouse(release(text.x + 2, text.y + 1));

        assert_eq!(
            action,
            WorkbenchAction::Copy("one\ntw".to_owned()),
            "{SELECTION_UNCOPIED}"
        );
        assert_eq!(workbench.clipboard, "one\ntw", "{SELECTION_UNCOPIED}");
    }

    #[test]
    fn releasing_a_double_click_hands_over_the_word_it_took() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        draw(&mut workbench, 80, 24);
        let text = workbench.panes.text;

        workbench.handle_mouse(click(text.x + 1, text.y));
        workbench.handle_mouse(click(text.x + 1, text.y));
        let action = workbench.handle_mouse(release(text.x + 1, text.y));

        assert_eq!(
            action,
            WorkbenchAction::Copy("one".to_owned()),
            "{SELECTION_UNCOPIED}"
        );
    }

    #[test_case(false ; "with no field up")]
    #[test_case(true ; "over a selection standing in the find bar")]
    fn a_press_that_selected_nothing_leaves_the_clipboard_alone(finding: bool) {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        if finding {
            open_find(&mut workbench);
            workbench.paste(FIELD_WORDS);
            workbench.handle_key(press(keys::SELECT_ALL));
        }
        draw(&mut workbench, 80, 24);
        let text = workbench.panes.text;
        workbench.clipboard = KEPT_CLIPBOARD.to_owned();

        workbench.handle_mouse(click(text.x + 1, text.y));
        let action = workbench.handle_mouse(release(text.x + 1, text.y));

        assert_eq!(action, WorkbenchAction::Consumed, "{IDLE_CLICK_COPIED}");
        assert_eq!(workbench.clipboard, KEPT_CLIPBOARD, "{IDLE_CLICK_COPIED}");
    }

    #[test]
    fn a_release_that_started_outside_the_buffer_copies_nothing() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.handle_key(KeyEvent::new(keys::SELECT_ALL.code, KeyModifiers::CONTROL));
        draw(&mut workbench, 80, 24);
        let rows = workbench.panes.rows;

        workbench.handle_mouse(click(rows.x, rows.y));
        let action = workbench.handle_mouse(release(rows.x, rows.y));

        assert_eq!(action, WorkbenchAction::Consumed, "{IDLE_CLICK_COPIED}");
    }

    #[test_case(2, "one" ; "a second click takes the word")]
    #[test_case(3, "one\n" ; "a third takes the whole line")]
    fn clicking_again_in_the_buffer_widens_what_is_selected(presses: u8, expected: &str) {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        draw(&mut workbench, 80, 24);
        let text = workbench.panes.text;

        for _ in 0..presses {
            workbench.handle_mouse(click(text.x + 1, text.y));
        }

        assert_eq!(
            workbench.selected_text().as_deref(),
            Some(expected),
            "{WRONG_CLICK}"
        );
    }

    #[test]
    fn a_drag_only_scrolls_once_it_has_left_the_buffer() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        draw(&mut workbench, 80, 24);
        let text = workbench.panes.text;
        workbench.handle_mouse(click(text.x, text.y));

        for (row, expected) in [
            (text.y, 0),
            (text.y - 1, -EDGE_SCROLL_LINES),
            (text.bottom(), EDGE_SCROLL_LINES),
        ] {
            workbench.handle_mouse(drag(text.x, row));
            assert_eq!(workbench.edge_scroll_delta(), expected, "{WRONG_CLICK}");
        }

        workbench.handle_mouse(release(text.x, text.bottom()));
        assert_eq!(workbench.edge_scroll_delta(), 0, "{WRONG_CLICK}");
    }

    #[test]
    fn a_tick_carries_a_drag_that_ran_off_the_buffer_along_with_it() {
        let dir = TempDir::new().expect("a temporary directory");
        let lines: String = (1..=50).map(|number| format!("line {number}\n")).collect();
        fs::write(dir.path().join("long.txt"), lines).expect("a file");
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.open(dir.path());
        workbench.handle_key(key(KeyCode::Enter));
        draw(&mut workbench, 60, 10);
        let text = workbench.panes.text;

        workbench.handle_mouse(click(text.x, text.y));
        workbench.handle_mouse(drag(text.x, text.bottom()));
        let reached = workbench.selected_text().expect("a selection").len();
        assert!(workbench.is_busy(), "{WRONG_CLICK}");

        let (changed, _) = workbench.tick();

        assert!(changed, "{WRONG_CLICK}");
        assert_eq!(
            workbench.editor.active().expect(NO_TAB).scroll(),
            1,
            "{WRONG_CLICK}"
        );
        assert!(
            workbench.selected_text().expect("a selection").len() > reached,
            "{WRONG_CLICK}"
        );
    }

    #[test_case(true, true ; "a tree taller than its pane draws one")]
    #[test_case(false, false ; "turning them off gives the column back")]
    fn a_scrollbar_says_how_far_down_the_tree_is(scrollbars: bool, expected: bool) {
        let (_dir, mut workbench) = tall_project();
        workbench.set_scrollbars(scrollbars);

        let painted = draw(&mut workbench, 80, 24);

        assert_eq!(painted.contains(SCROLLBAR_THUMB), expected, "{WRONG_BAR}");
        assert_eq!(
            workbench.panes.rows.right() == workbench.panes.rows.x + DEFAULT_SIDEBAR_WIDTH,
            !expected,
            "{WRONG_BAR}"
        );
    }

    #[test]
    fn a_tree_that_fits_keeps_its_whole_width() {
        let (_dir, mut workbench) = project();

        let painted = draw(&mut workbench, 80, 24);

        assert!(!painted.contains(SCROLLBAR_THUMB), "{WRONG_BAR}");
    }

    #[test]
    fn a_file_taller_than_its_pane_draws_a_scrollbar() {
        let (dir, mut workbench) = tall_project();
        workbench.open_path(&dir.path().join("file0.txt"));
        workbench.sidebar_collapsed = true;

        assert!(
            draw(&mut workbench, 80, 24).contains(SCROLLBAR_THUMB),
            "{WRONG_BAR}"
        );
    }

    /// Each section scrolls on its own, so each gives up its own column, and
    /// the body it records is the rows it drew into rather than the room it
    /// was given.
    #[test]
    fn a_crowded_section_keeps_its_scrollbar_out_of_its_body() {
        let dir = TempDir::new().expect("a temporary directory");
        gix::init(dir.path()).expect("a repository");
        for index in 0..40 {
            fs::write(dir.path().join(format!("f{index}.txt")), "").expect("a file");
        }
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.open(dir.path());
        workbench.handle_leader(press(keys::VIEW_SOURCE_CONTROL));

        assert!(
            draw(&mut workbench, 80, 24).contains(SCROLLBAR_THUMB),
            "{WRONG_BAR}"
        );

        let sidebar = workbench.panes.sidebar.expect("a sidebar");
        assert!(
            workbench
                .panes
                .sections
                .iter()
                .any(|rects| rects.body.right() < sidebar.right()),
            "{WRONG_BAR}"
        );
    }

    /// The bar is the only fast way down a long tree. Dragging it moves the
    /// window and nothing else: the selection stays where the keyboard left it,
    /// the way scrolling a buffer never moves a caret.
    #[test]
    fn dragging_the_tree_bar_scrolls_without_moving_the_selection() {
        let (_dir, mut workbench) = tall_project();
        draw(&mut workbench, 80, 24);
        let rows = workbench.panes.rows;
        let selected = workbench.tree.selected_index();

        workbench.handle_mouse(click(rows.right(), rows.y + 1));
        workbench.handle_mouse(drag(rows.right(), rows.bottom() - 1));
        draw(&mut workbench, 80, 24);

        let max = workbench.tree.rows().len() - rows.height as usize;
        assert_eq!(workbench.tree.scroll(), max, "{WRONG_BAR}");
        assert_eq!(workbench.tree.selected_index(), selected, "{WRONG_CLICK}");
    }

    /// Releasing hands the pane back, so the next keyboard move is free to pull
    /// the window to the selection again.
    #[test]
    fn releasing_the_bar_lets_the_selection_pull_the_window_back() {
        let (_dir, mut workbench) = tall_project();
        draw(&mut workbench, 80, 24);
        let rows = workbench.panes.rows;

        workbench.handle_mouse(click(rows.right(), rows.y + 1));
        workbench.handle_mouse(drag(rows.right(), rows.bottom() - 1));
        workbench.handle_mouse(release(rows.right(), rows.bottom() - 1));
        draw(&mut workbench, 80, 24);

        assert_eq!(workbench.tree.scroll(), 0, "{WRONG_BAR}");
    }

    /// The bar owns its column, so a press there acts on nothing rather than on
    /// the row painted beside it.
    #[test]
    fn the_scrollbar_column_is_not_a_row_hit() {
        let (_dir, mut workbench) = tall_project();
        draw(&mut workbench, 80, 24);
        let rows = workbench.panes.rows;
        let before = workbench.tree.selected_index();

        workbench.handle_mouse(click(rows.right(), rows.y + 3));

        assert_eq!(workbench.tree.selected_index(), before, "{WRONG_CLICK}");
    }

    /// The strip has room for three of these, so most of them are off screen
    /// whichever one is active.
    #[test]
    fn the_active_tab_is_always_on_the_strip() {
        let (_dir, mut workbench) = many_tabs(8);

        for _ in 0..workbench.editor.tabs().len() {
            let title = workbench.active_title();
            assert!(
                draw(&mut workbench, 80, 24).contains(&title),
                "{TAB_OFF_STRIP}"
            );
            workbench.handle_key(press(keys::NEXT_TAB));
        }
    }

    #[test]
    fn a_click_lands_on_the_tab_under_it_after_the_strip_scrolled() {
        let (_dir, mut workbench) = many_tabs(8);
        draw(&mut workbench, 80, 24);
        let tabs = workbench.panes.tabs;
        let shown = visible_range(&workbench.editor, tabs.width);
        assert!(shown.start > 0, "{TAB_OFF_STRIP}");
        let body = mark_column(&workbench, shown.start, TabPart::Body);

        workbench.handle_mouse(click(body, tabs.y));

        assert_eq!(
            workbench.editor.active_index(),
            shown.start,
            "{WRONG_CLICK}"
        );
    }

    #[test]
    fn a_close_mark_is_reachable_after_the_strip_scrolled() {
        let (_dir, mut workbench) = many_tabs(8);
        draw(&mut workbench, 80, 24);
        let tabs = workbench.panes.tabs;
        let last = workbench.editor.tabs().len() - 1;

        workbench.handle_mouse(click(close_column(&workbench, last), tabs.y));

        assert_eq!(workbench.editor.tabs().len(), last, "{WRONG_CLICK}");
    }

    /// The mark the strip paints and the mark the pointer finds have to be
    /// the same column, or the strip is offering a menu that is not there.
    #[test]
    fn the_tab_mark_is_painted_where_the_pointer_finds_it() {
        let (_dir, mut workbench) = many_tabs(2);
        draw(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let tabs = workbench.panes.tabs;

        let column = painted_column(&mut workbench, tabs, tabs.y, MENU_MARK);

        assert_eq!(
            tab_at(&workbench.editor, column, tabs).map(|hit| hit.part),
            Some(TabPart::Menu),
            "{CONTROL_MISPLACED}"
        );
    }

    /// The strip opens on its last tab, so what it cut off is all to the left.
    /// Cycling past the end wraps to the first, and the overflow changes ends.
    #[test]
    fn the_strip_marks_which_end_it_cut_off() {
        let (_dir, mut workbench) = many_tabs(8);

        let painted = draw(&mut workbench, 80, 24);
        assert!(painted.contains(MORE_LEFT), "{NO_OVERFLOW_MARK}");
        assert!(!painted.contains(MORE_RIGHT), "{NO_OVERFLOW_MARK}");

        workbench.handle_key(press(keys::NEXT_TAB));

        let painted = draw(&mut workbench, 80, 24);
        assert!(!painted.contains(MORE_LEFT), "{NO_OVERFLOW_MARK}");
        assert!(painted.contains(MORE_RIGHT), "{NO_OVERFLOW_MARK}");
    }

    #[test]
    fn a_strip_with_room_to_spare_marks_neither_end() {
        let (_dir, mut workbench) = many_tabs(2);

        let painted = draw(&mut workbench, 80, 24);

        assert!(!painted.contains(MORE_LEFT), "{NO_OVERFLOW_MARK}");
        assert!(!painted.contains(MORE_RIGHT), "{NO_OVERFLOW_MARK}");
    }

    #[test]
    fn a_click_on_a_tabs_close_mark_closes_it() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        draw(&mut workbench, 80, 24);
        let tabs = workbench.panes.tabs;

        workbench.handle_mouse(click(close_column(&workbench, 0), tabs.y));

        assert!(workbench.editor.tabs().is_empty(), "{WRONG_CLICK}");
    }

    #[test]
    fn the_close_mark_asks_before_dropping_unsaved_work() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.handle_key(key(KeyCode::Char('x')));
        draw(&mut workbench, 80, 24);
        let tabs = workbench.panes.tabs;

        workbench.handle_mouse(click(close_column(&workbench, 0), tabs.y));

        assert_eq!(workbench.editor.tabs().len(), 1, "{UNASKED_CLOSE}");
        assert_eq!(answer(&workbench), Some(Choice::Save), "{NOT_ASKED}");
    }

    #[test_case(Choice::Discard, 0 ; "a click on don't save closes the tab")]
    #[test_case(Choice::Cancel, 1 ; "a click on cancel keeps it")]
    fn a_click_answers_the_dialog(answer: Choice, remaining: usize) {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.handle_key(key(KeyCode::Char('x')));
        workbench.handle_leader(press(keys::CLOSE_TAB));
        draw(&mut workbench, 80, 24);
        let answers = workbench.panes.confirm;

        workbench.handle_mouse(click(answer_column(&workbench, answer), answers.y));

        assert_eq!(workbench.editor.tabs().len(), remaining, "{ANSWER_IGNORED}");
        assert_eq!(workbench.confirm, None, "{ANSWER_IGNORED}");
    }

    #[test]
    fn a_click_outside_the_dialog_leaves_the_question_standing() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.handle_key(key(KeyCode::Char('x')));
        workbench.handle_leader(press(keys::CLOSE_TAB));
        draw(&mut workbench, 80, 24);
        let rows = workbench.panes.rows;

        workbench.handle_mouse(click(rows.x, rows.y));

        assert_eq!(answer(&workbench), Some(Choice::Save), "{MODAL_LEAKED}");
        assert_eq!(workbench.focus, Focus::Editor, "{MODAL_LEAKED}");
    }

    #[test]
    fn a_middle_click_closes_the_tab_under_it() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        draw(&mut workbench, 80, 24);
        let tabs = workbench.panes.tabs;

        workbench.handle_mouse(middle_click(tabs.x + 1, tabs.y));

        assert!(workbench.editor.tabs().is_empty(), "{WRONG_CLICK}");
    }

    #[test]
    fn a_middle_click_cannot_close_a_tab_behind_the_dialog() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.handle_key(key(KeyCode::Char('x')));
        workbench.open_path(&dir.path().join("sub/b.txt"));
        draw(&mut workbench, 80, 24);
        let tabs = workbench.panes.tabs;
        workbench.handle_mouse(click(close_column(&workbench, 0), tabs.y));
        assert_eq!(answer(&workbench), Some(Choice::Save), "{NOT_ASKED}");

        workbench.handle_mouse(middle_click(close_column(&workbench, 1), tabs.y));

        assert_eq!(workbench.editor.tabs().len(), 2, "{MODAL_LEAKED}");
        assert_eq!(answer(&workbench), Some(Choice::Save), "{MODAL_LEAKED}");
    }

    #[test_case(SidebarView::SourceControl ; "the second segment switches to source control")]
    #[test_case(SidebarView::Search ; "the third segment switches to search")]
    fn a_click_on_a_header_segment_switches_the_view(expected: SidebarView) {
        let (_dir, mut workbench) = project();
        draw(&mut workbench, 80, 24);
        let header = workbench.panes.header;
        let column = (header.x..header.right())
            .find(|column| header_at(*column, header.x) == Some(expected))
            .expect("a segment for the view");

        workbench.handle_mouse(click(column, header.y));

        assert_eq!(workbench.sidebar_view(), expected, "{WRONG_PANE}");
    }

    #[test]
    fn a_click_on_a_search_toggle_turns_it_on() {
        let (_dir, mut workbench) = project();
        workbench.handle_leader(press(keys::VIEW_SEARCH));
        draw(&mut workbench, 80, 24);
        let toggles = workbench.panes.toggles;
        let column = (toggles.x..toggles.right())
            .find(|column| toggle_at(*column, toggles.x) == Some(Toggle::Regex))
            .expect("a regex button");

        workbench.handle_mouse(click(column, toggles.y));

        assert!(workbench.search.query().regex, "{WRONG_CLICK}");
    }

    /// The search view keeps three rows above its results, which an offset
    /// measured from the sidebar itself lands short of.
    #[test]
    fn a_click_on_a_search_hit_selects_the_hit_under_it() {
        let (dir, mut workbench) = project();
        search_for(&mut workbench, "three");
        draw(&mut workbench, 80, 24);
        let rows = workbench.panes.rows;

        workbench.handle_mouse(click(rows.x + 1, rows.y + 1));

        assert_eq!(workbench.search.selected_index(), 1, "{WRONG_CLICK}");
        let tab = workbench.editor.active().expect(NO_TAB);
        assert_eq!(
            tab.path,
            WorkbenchPath::Local(dir.path().join("a.txt")),
            "{NO_TAB}"
        );
        assert_eq!(tab.buffer.cursor().line, 2, "{WRONG_LINE}");
    }

    #[test]
    fn the_pointer_marks_the_row_it_is_resting_on() {
        let (_dir, mut workbench) = project();
        draw(&mut workbench, 80, 24);
        let rows = workbench.panes.rows;
        let unselected = (rows.x + 1, rows.y + 1);
        let plain = cell_style(&mut workbench, unselected);

        workbench.handle_mouse(moved(unselected.0, unselected.1));

        assert_ne!(
            cell_style(&mut workbench, unselected),
            plain,
            "{WRONG_HOVER}"
        );
    }

    /// Source control lists paths relative to the repository, and the editor
    /// holds absolute ones, so the two only meet if the translation between
    /// them is right. Nothing else exercises it: the ordinary fixture is not a
    /// repository, so `workdir` is `None` and the reveal never runs.
    #[test]
    fn opening_a_file_moves_the_source_control_cursor_to_its_change() {
        let dir = TempDir::new().expect("a temporary directory");
        gix::init(dir.path()).expect("a repository");
        fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").expect("a file");
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.open(dir.path());
        assert!(workbench.scm.is_repository(), "{NOT_TRACKED}");

        open_file(&dir, &mut workbench);

        let cursor = workbench.scm.cursor();
        assert_eq!(
            cursor
                .row
                .and_then(|row| workbench.scm.identity(cursor.section, row))
                .as_deref(),
            Some("a.txt"),
            "{NOT_TRACKED}"
        );
    }

    /// Opening a file takes the focus to the editor, and the explorer used to
    /// drop its mark the moment that happened. A reveal the reader cannot see
    /// is the same as no reveal, so the row keeps the bar.
    #[test]
    fn the_explorer_marks_the_open_file_while_the_editor_has_the_focus() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        assert_eq!(workbench.focus, Focus::Editor, "{NOT_TRACKED}");
        draw(&mut workbench, 80, 24);
        let rows = workbench.panes.rows;
        let column = row_body(rows);

        let opened = cell_style(&mut workbench, (column, rows.y + 1));
        let untouched = cell_style(&mut workbench, (column, rows.y));

        workbench.focus = Focus::Sidebar;
        draw(&mut workbench, 80, 24);

        assert_eq!(
            opened,
            cell_style(&mut workbench, (column, rows.y + 1)),
            "{NOT_TRACKED}"
        );
        assert_ne!(untouched, opened, "{NOT_TRACKED}");
    }

    /// The selected row already stands out, so a hover on top of it would be
    /// two highlights arguing over one cell.
    #[test]
    fn the_pointer_leaves_the_selected_row_alone() {
        let (_dir, mut workbench) = project();
        draw(&mut workbench, 80, 24);
        let rows = workbench.panes.rows;
        let selected = (row_body(rows), rows.y);
        let before = cell_style(&mut workbench, selected);

        workbench.handle_mouse(moved(selected.0, selected.1));

        assert_eq!(
            cell_style(&mut workbench, selected),
            before,
            "{WRONG_HOVER}"
        );
    }

    /// The handle the row paints and the handle a press reaches have to be
    /// the same column, and a left press on it has to open what the right
    /// button opens, since the handle is what says the menu is there at all.
    #[test]
    fn a_press_on_a_row_handle_opens_its_menu() {
        let (dir, mut workbench) = project();
        draw(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let rows = workbench.panes.rows;
        let column = painted_column(&mut workbench, rows, rows.y, MENU_MARK);

        workbench.handle_mouse(click(column, rows.y));

        let menu = workbench.menu.as_ref().expect(NO_MENU);
        assert_eq!(
            menu.target(),
            &Target::Row(dir.path().join(NESTED_DIR).into()),
            "{WRONG_TARGET}"
        );
    }

    /// A folder the handle belongs to must not open on the way to its menu,
    /// which is what a press one column over would have done.
    #[test]
    fn a_press_on_a_row_handle_leaves_the_row_closed() {
        let (_dir, mut workbench) = project();
        draw(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let rows = workbench.panes.rows;
        let before = workbench.tree.rows().len();

        workbench.handle_mouse(click(rows.x, rows.y));

        assert_eq!(workbench.tree.rows().len(), before, "{WRONG_CLICK}");
    }

    #[test]
    fn a_right_press_opens_the_menu_for_the_row_it_lands_on() {
        let (dir, mut workbench) = project();
        select_file(&dir, &mut workbench);
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let rows = workbench.panes.rows;

        workbench.handle_mouse(right_click(rows.x, rows.y));

        let menu = workbench.menu.as_ref().expect(NO_MENU);
        assert_eq!(
            menu.target(),
            &Target::Row(dir.path().join(NESTED_DIR).into()),
            "{WRONG_TARGET}"
        );
    }

    /// The mark is the only thing on the strip saying a menu is there, so a
    /// plain click has to reach it, and reading a menu about a tab is not a
    /// reason to leave the file that is on screen.
    #[test]
    fn a_click_on_a_tab_mark_opens_its_menu_and_leaves_the_tab_alone() {
        let (_dir, mut workbench) = many_tabs(2);
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let active = workbench.editor.active_index();
        let other = active - 1;
        let column = mark_column(&workbench, other, TabPart::Menu);

        workbench.handle_mouse(click(column, workbench.panes.tabs.y));

        let menu = workbench.menu.as_ref().expect(NO_MENU);
        assert_eq!(menu.target(), &Target::Tab(other), "{WRONG_TARGET}");
        assert_eq!(workbench.editor.active_index(), active, "{WRONG_CLICK}");
    }

    #[test]
    fn the_menu_key_opens_over_the_cursor() {
        let (dir, mut workbench) = project();
        select_file(&dir, &mut workbench);

        workbench.handle_leader(press(keys::MENU));

        let menu = workbench.menu.as_ref().expect(NO_MENU);
        assert_eq!(
            menu.target(),
            &Target::Row(dir.path().join(OPENED_FILE).into()),
            "{WRONG_TARGET}"
        );
    }

    #[test]
    fn a_press_off_the_panel_only_takes_the_menu_down() {
        let (dir, mut workbench) = project();
        select_file(&dir, &mut workbench);
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let rows = workbench.panes.rows;
        workbench.handle_mouse(right_click(rows.x, rows.y));
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let text = workbench.panes.text;

        workbench.handle_mouse(click(text.x, text.y));

        assert!(workbench.menu.is_none(), "{MENU_STUCK}");
        assert_eq!(workbench.focus, Focus::Sidebar, "{MODAL_LEAKED}");
    }

    #[test_case(MenuAction::CopyPath, false ; "the whole path")]
    #[test_case(MenuAction::CopyRelative, true ; "and the one the project sees")]
    fn a_copy_hands_the_host_the_path(action: MenuAction, relative: bool) {
        let (dir, mut workbench) = project();
        select_file(&dir, &mut workbench);
        workbench.handle_leader(press(keys::MENU));

        let copied = menu_action(&mut workbench, action);

        let path = match relative {
            true => PathBuf::from(OPENED_FILE),
            false => dir.path().join(OPENED_FILE),
        };
        assert_eq!(
            copied,
            WorkbenchAction::Copy(path.display().to_string()),
            "{PATH_UNCOPIED}"
        );
    }

    #[test]
    fn a_rename_moves_the_file_and_takes_its_tab_with_it() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.focus = Focus::Sidebar;
        workbench.handle_leader(press(keys::MENU));
        menu_action(&mut workbench, MenuAction::Rename);

        answer_prompt(&mut workbench, RENAMED_FILE);

        assert!(dir.path().join(RENAMED_FILE).is_file(), "{NOT_RENAMED}");
        assert!(!dir.path().join(OPENED_FILE).exists(), "{NOT_RENAMED}");
        let tab = workbench.editor.active().expect(NO_TAB);
        assert_eq!(
            tab.path,
            WorkbenchPath::Local(dir.path().join(RENAMED_FILE)),
            "{TAB_LEFT_BEHIND}"
        );
        assert_eq!(tab.title, RENAMED_FILE, "{TAB_LEFT_BEHIND}");
    }

    /// A folder rename moves every path under it at once, so the tabs have to
    /// be followed by prefix rather than by an equality check.
    #[test]
    fn renaming_a_folder_carries_the_tabs_under_it() {
        let (dir, mut workbench) = project();
        workbench.open_path(&dir.path().join(NESTED_DIR).join(NESTED_FILE));
        workbench.tree.reveal(&dir.path().join(NESTED_DIR));
        workbench.focus = Focus::Sidebar;
        workbench.handle_leader(press(keys::MENU));
        menu_action(&mut workbench, MenuAction::Rename);

        answer_prompt(&mut workbench, RENAMED_DIR);

        assert!(
            dir.path().join(RENAMED_DIR).join(NESTED_FILE).is_file(),
            "{NOT_RENAMED}"
        );
        assert_eq!(
            workbench.editor.active().map(|tab| tab.path.clone()),
            Some(dir.path().join(RENAMED_DIR).join(NESTED_FILE).into()),
            "{TAB_LEFT_BEHIND}"
        );
    }

    /// The name is still there to be corrected, because retyping a long one
    /// over a typo is the worst way to answer a refusal.
    #[test]
    fn a_name_that_is_taken_is_refused_and_left_in_the_box() {
        let (dir, mut workbench) = project();
        select_file(&dir, &mut workbench);
        workbench.handle_leader(press(keys::MENU));
        menu_action(&mut workbench, MenuAction::Rename);

        answer_prompt(&mut workbench, NESTED_DIR);

        assert!(dir.path().join(OPENED_FILE).is_file(), "{RENAMED_OVER}");
        let input = workbench.input.as_ref().expect(PROMPT_GONE);
        assert_eq!(input.value.text(), NESTED_DIR, "{PROMPT_GONE}");
        assert!(workbench.flash.is_some(), "{NO_REASON}");
    }

    #[test_case(MenuAction::NewFile, true ; "a new file is a file")]
    #[test_case(MenuAction::NewFolder, false ; "and a new folder is a folder")]
    fn something_new_lands_beside_the_row_it_was_asked_from(action: MenuAction, file: bool) {
        let (dir, mut workbench) = project();
        select_file(&dir, &mut workbench);
        workbench.handle_leader(press(keys::MENU));
        menu_action(&mut workbench, action);

        answer_prompt(&mut workbench, MADE_NAME);

        let made = dir.path().join(MADE_NAME);
        assert_eq!(made.is_file(), file, "{NOT_MADE}");
        assert_eq!(made.is_dir(), !file, "{NOT_MADE}");
        assert_eq!(
            workbench.editor.active().map(|tab| tab.path.clone()),
            file.then(|| made.clone().into()),
            "{NOT_MADE}"
        );
    }

    #[test]
    fn a_new_path_lands_inside_the_folder_it_was_asked_from() {
        let (dir, mut workbench) = project();
        workbench.handle_leader(press(keys::MENU));
        menu_action(&mut workbench, MenuAction::NewFile);

        answer_prompt(&mut workbench, MADE_NAME);

        assert!(
            dir.path().join(NESTED_DIR).join(MADE_NAME).is_file(),
            "{NOT_MADE}"
        );
    }

    #[test_case(OPENED_FILE, 0 ; "a file goes on its own")]
    #[test_case(NESTED_DIR, 1 ; "a folder says what goes with it")]
    fn a_delete_asks_before_it_takes_the_path(name: &str, under: usize) {
        let (dir, mut workbench) = project();
        workbench.tree.reveal(&dir.path().join(name));
        workbench.handle_leader(press(keys::MENU));

        menu_action(&mut workbench, MenuAction::Delete);

        assert!(dir.path().join(name).exists(), "{DELETED_UNASKED}");
        assert_eq!(asked(&workbench).ask, Ask::Delete(under), "{NOT_COUNTED}");

        workbench.handle_key(key(KeyCode::Char(Choice::Discard.accelerator())));

        assert!(!dir.path().join(name).exists(), "{ANSWER_IGNORED}");
    }

    #[test]
    fn a_delete_takes_the_tabs_that_were_on_it() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.focus = Focus::Sidebar;
        workbench.handle_leader(press(keys::MENU));
        menu_action(&mut workbench, MenuAction::Delete);

        workbench.handle_key(key(KeyCode::Char(Choice::Discard.accelerator())));

        assert!(workbench.editor.tabs().is_empty(), "{TAB_LEFT_BEHIND}");
    }

    /// Nothing here can put the file back, so the work in the tab is all that
    /// is left of it.
    #[test]
    fn a_delete_keeps_an_edited_tab_and_flies_the_conflict() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.handle_key(key(KeyCode::Char(EDIT)));
        workbench.focus = Focus::Sidebar;
        workbench.handle_leader(press(keys::MENU));
        menu_action(&mut workbench, MenuAction::Delete);

        workbench.handle_key(key(KeyCode::Char(Choice::Discard.accelerator())));

        let tab = workbench.editor.active().expect(NO_TAB);
        assert!(tab.conflict, "{NO_CONFLICT}");
        assert!(tab.is_dirty(), "{LOST_EDIT}");
    }

    #[test_case(MenuAction::CloseOthers, &["file1.txt"] ; "others leaves the one it was asked from")]
    #[test_case(MenuAction::CloseRight, &["file0.txt", "file1.txt"] ; "to the right keeps what is left of it")]
    #[test_case(MenuAction::CloseAll, &[] ; "all leaves nothing")]
    fn a_batch_close_covers_what_it_names(action: MenuAction, left: &[&str]) {
        let (_dir, mut workbench) = many_tabs(3);

        workbench.run_menu(action, &Target::Tab(1));

        assert_eq!(open_titles(&workbench), left, "{WRONG_TABS}");
        assert_eq!(workbench.confirm, None, "{POINTLESS_QUESTION}");
    }

    #[test]
    fn send_to_composer_names_the_row_the_way_the_composer_reads_it() {
        let (dir, mut workbench) = project();
        select_file(&dir, &mut workbench);
        workbench.handle_leader(press(keys::MENU));

        let from_menu = menu_action(&mut workbench, MenuAction::SendToComposer);

        assert_eq!(from_menu, sent(OPENED_FILE, None), "{NOT_SENT}");
    }

    /// A preview tab is the next one to be taken over, so keeping it is the
    /// only thing standing between what is in it and the next file opened.
    #[test]
    fn keep_open_takes_a_tab_out_of_the_preview_slot() {
        let (dir, mut workbench) = project();
        let path = dir.path().join(OPENED_FILE);
        workbench.editor.preview(&path, 0).expect("a preview tab");

        workbench.run_menu(MenuAction::KeepOpen, &Target::Tab(0));

        assert!(!workbench.editor.tabs()[0].preview, "{STILL_A_PREVIEW}");
    }

    #[test]
    fn save_from_the_menu_writes_the_tab_it_was_asked_from() {
        let (dir, mut workbench) = many_tabs(2);
        edit_tab(&mut workbench, 0);
        workbench.editor.select(1);

        workbench.run_menu(MenuAction::Save, &Target::Tab(0));

        assert_eq!(
            fs::read_to_string(dir.path().join("file0.txt")).expect("the saved file"),
            format!("{EDIT}one\n"),
            "{ANSWER_IGNORED}"
        );
        assert!(!workbench.editor.tabs()[0].is_dirty(), "{ANSWER_IGNORED}");
    }

    #[test]
    fn reveal_in_explorer_brings_the_sidebar_back_to_the_file() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.sidebar = SidebarView::Search;
        workbench.sidebar_collapsed = true;

        workbench.run_menu(MenuAction::RevealInExplorer, &Target::Tab(0));

        assert_eq!(workbench.sidebar, SidebarView::Explorer, "{NOT_REVEALED}");
        assert!(!workbench.sidebar_collapsed, "{NOT_REVEALED}");
        assert_eq!(workbench.focus, Focus::Sidebar, "{NOT_REVEALED}");
        assert_eq!(
            workbench.tree.selected().map(|row| row.path.clone()),
            Some(dir.path().join(OPENED_FILE).into()),
            "{NOT_REVEALED}"
        );
    }

    #[test]
    fn close_saved_leaves_the_tab_with_work_in_it() {
        let (_dir, mut workbench) = many_tabs(3);
        edit_tab(&mut workbench, 1);

        workbench.run_menu(MenuAction::CloseSaved, &Target::Tab(1));

        assert_eq!(open_titles(&workbench), ["file1.txt"], "{WRONG_TABS}");
        assert_eq!(workbench.confirm, None, "{POINTLESS_QUESTION}");
    }

    #[test]
    fn a_batch_close_stops_at_unsaved_work_and_carries_on_once_answered() {
        let (_dir, mut workbench) = many_tabs(3);
        edit_tab(&mut workbench, 1);

        workbench.run_menu(MenuAction::CloseAll, &Target::Tab(0));

        assert_eq!(
            open_titles(&workbench),
            ["file1.txt", "file2.txt"],
            "{QUEUE_RAN_ON}"
        );
        assert_eq!(asked(&workbench).ask, Ask::Close, "{NOT_ASKED}");

        workbench.handle_key(key(KeyCode::Char(Choice::Discard.accelerator())));

        assert!(open_titles(&workbench).is_empty(), "{QUEUE_STALLED}");
    }

    /// The row of the panel an action was painted on, found the way the
    /// pointer finds it.
    fn menu_row(workbench: &Workbench, action: MenuAction) -> u16 {
        let menu = workbench.menu.as_ref().expect(NO_MENU);
        let offset = menu
            .items()
            .iter()
            .position(|item| *item == MenuItem::Action(action))
            .expect("the action in the menu");
        workbench.panes.menu.y + offset as u16
    }

    /// Where the first rule between two groups was painted, counted from the
    /// top of the panel.
    fn rule_offset(workbench: &Workbench) -> u16 {
        workbench
            .menu
            .as_ref()
            .expect(NO_MENU)
            .items()
            .iter()
            .position(|item| *item == MenuItem::Separator)
            .expect("a rule between two groups") as u16
    }

    /// Opens the menu on a tree row and paints it, so the panel has geometry
    /// for a press to land on. Painted first as well, because the menu is
    /// anchored on the row the last frame put on screen.
    fn open_row_menu(dir: &TempDir, workbench: &mut Workbench) {
        select_file(dir, workbench);
        paint(workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        workbench.handle_leader(press(keys::MENU));
        paint(workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
    }

    #[test]
    fn a_press_on_a_row_of_the_panel_takes_that_action() {
        let (dir, mut workbench) = project();
        open_row_menu(&dir, &mut workbench);
        let row = menu_row(&workbench, MenuAction::CopyPath);

        let copied = workbench.handle_mouse(click(workbench.panes.menu.x, row));
        workbench.handle_mouse(release(workbench.panes.menu.x, row));

        assert_eq!(
            copied,
            WorkbenchAction::Copy(dir.path().join(OPENED_FILE).display().to_string()),
            "{PATH_UNCOPIED}"
        );
        assert!(workbench.menu.is_none(), "{MENU_STUCK}");
    }

    /// One click of a button is a press and a release. The release used to
    /// take down the menu the press had just opened, which no test that sends
    /// half a click can see.
    #[test]
    fn the_menu_outlives_the_release_of_the_press_that_opened_it() {
        let (dir, mut workbench) = project();
        select_file(&dir, &mut workbench);
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let rows = workbench.panes.rows;

        workbench.handle_mouse(right_click(rows.x, rows.y));
        let released = workbench.handle_mouse(right_release(rows.x, rows.y));

        assert!(workbench.menu.is_some(), "{MENU_GONE}");
        assert_eq!(released, WorkbenchAction::Consumed, "{MODAL_LEAKED}");
    }

    /// A rule is part of the panel, so hitting one is a near miss rather than
    /// a change of mind.
    #[test]
    fn a_press_on_a_rule_leaves_the_menu_standing() {
        let (dir, mut workbench) = project();
        open_row_menu(&dir, &mut workbench);
        let rule = rule_offset(&workbench);

        workbench.handle_mouse(click(workbench.panes.menu.x, workbench.panes.menu.y + rule));

        assert!(workbench.menu.is_some(), "{MENU_GONE}");
    }

    #[test_case(Reach::Wheel ; "a wheel would scroll what the panel is anchored to")]
    #[test_case(Reach::Middle ; "a middle press would close a tab behind it")]
    #[test_case(Reach::Drag ; "and a drag would take a selection under it")]
    fn the_menu_goes_down_before_the_pointer_reaches_the_panes(reach: Reach) {
        let (_dir, mut workbench) = many_tabs(2);
        workbench.focus = Focus::Editor;
        workbench.handle_leader(press(keys::MENU));
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let tabs = workbench.panes.tabs;

        match reach {
            Reach::Wheel => workbench.handle_mouse(wheel(tabs.x, tabs.bottom())),
            Reach::Middle => workbench.handle_mouse(middle_click(tabs.x, tabs.y)),
            Reach::Drag => workbench.handle_mouse(drag(tabs.x, tabs.bottom())),
        };

        assert!(workbench.menu.is_none(), "{MENU_STUCK}");
        assert_eq!(workbench.editor.tabs().len(), 2, "{MODAL_LEAKED}");
    }

    /// A tab's panel hangs over the buffer, and the press that opened it left
    /// a selection standing there. The release belongs to the panel, so it
    /// must not be read as the end of that selection.
    #[test_case(true ; "a press on a rule that leaves the menu up")]
    #[test_case(false ; "and a press on a row that takes it down")]
    fn a_release_after_a_menu_press_copies_nothing(rule: bool) {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        workbench.handle_leader(press(keys::MENU));
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let panel = workbench.panes.menu;
        let row = match rule {
            true => panel.y + rule_offset(&workbench),
            false => menu_row(&workbench, MenuAction::CopyRelative),
        };
        // Left of this the panel is over the gutter, where a release was never
        // going to be read as a selection anyway.
        let column = workbench.panes.text.x;
        let at = (column, row).into();
        assert!(
            panel.contains(at) && workbench.panes.text.contains(at),
            "{PANEL_OFF_BUFFER}"
        );

        workbench.handle_mouse(click(column, row));
        let released = workbench.handle_mouse(release(column, row));

        assert_eq!(released, WorkbenchAction::Consumed, "{STRAY_COPY}");
    }

    /// Whatever is standing over the panes is what the next key reaches, so
    /// the status bar has to be talking about that rather than the pane under
    /// it.
    #[test]
    fn the_status_bar_offers_the_menu_and_then_the_name_it_asks_for() {
        let (dir, mut workbench) = project();
        open_row_menu(&dir, &mut workbench);

        assert!(
            draw(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT).contains(MENU_HINTS[0].1),
            "{WRONG_HINT}"
        );

        menu_action(&mut workbench, MenuAction::Rename);

        assert!(
            draw(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT).contains(NAME_HINTS[0].1),
            "{WRONG_HINT}"
        );
    }

    /// Where the last frame laid out the hint for `bind`, found the way the
    /// pointer finds it.
    pub(crate) fn hint_rect(workbench: &Workbench, bind: keys::Bind) -> Rect {
        workbench
            .hint_hits
            .iter()
            .find(|(_, offered)| *offered == bind)
            .map(|(rect, _)| *rect)
            .expect(NO_HINT)
    }

    fn open_goto(workbench: &mut Workbench) {
        workbench.handle_key(press(keys::GOTO_LINE));
    }

    fn open_tab_menu(workbench: &mut Workbench) {
        workbench.handle_leader(press(keys::MENU));
    }

    /// Leaves unsaved work in the tab and asks to close it, which stands the
    /// dialog over the editor.
    fn ask_to_close_edited(workbench: &mut Workbench) {
        workbench.handle_key(key(KeyCode::Char(EDIT)));
        workbench.handle_leader(press(keys::CLOSE_TAB));
    }

    #[test]
    fn a_hint_under_the_pointer_is_lit_whole() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let hint = hint_rect(&workbench, keys::CLOSE);

        workbench.handle_mouse(moved(hint.right() - 1, hint.y));
        let surface = paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);

        let hover = WorkbenchStyles::default().hover.add_modifier;
        let lit = |column: u16| surface[(column, hint.y)].modifier.contains(hover);
        assert!((hint.x..hint.right()).all(lit), "{HINT_UNLIT}");
        assert!(!lit(hint.x - 1), "{GAP_LIT}");
    }

    fn leaves(_: &Workbench, action: &WorkbenchAction) -> bool {
        *action == WorkbenchAction::Close
    }

    fn sends(_: &Workbench, action: &WorkbenchAction) -> bool {
        matches!(action, WorkbenchAction::SendToComposer { .. })
    }

    fn finds(workbench: &Workbench, _: &WorkbenchAction) -> bool {
        workbench
            .editor
            .active()
            .is_some_and(|tab| tab.find.is_open())
    }

    fn saves(workbench: &Workbench, _: &WorkbenchAction) -> bool {
        let on_disk = fs::read_to_string(workbench.root.join(OPENED_FILE)).unwrap_or_default();
        on_disk.starts_with(EDIT) && workbench.editor.active().is_some_and(|tab| !tab.is_dirty())
    }

    /// One keystroke of unsaved work is left in the tab first, so saving has
    /// something to write.
    #[test_case(keys::SAVE, saves ; "save writes the file")]
    #[test_case(keys::FIND, finds ; "find opens the find bar")]
    #[test_case(keys::SEND_TO_COMPOSER, sends ; "a chord is pressed as its second half")]
    #[test_case(keys::CLOSE, leaves ; "esc leaves the workbench")]
    fn a_click_on_a_hint_presses_the_key_it_names(
        bind: keys::Bind,
        pressed: fn(&Workbench, &WorkbenchAction) -> bool,
    ) {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.handle_key(key(KeyCode::Char(EDIT)));
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let hint = hint_rect(&workbench, bind);

        let action = workbench.handle_mouse(click(hint.x, hint.y));

        assert!(pressed(&workbench, &action), "{HINT_IGNORED}: {action:?}");
    }

    /// A Markdown tab offers its other view as well, which only fits beside
    /// the rest on a wide row.
    #[test]
    fn a_click_on_the_view_hint_shows_the_other_view() {
        let (_dir, mut workbench) = markdown_project(INDEXED_TEXT);
        paint(&mut workbench, WIDE_TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let hint = hint_rect(&workbench, keys::TOGGLE_RENDERED);

        workbench.handle_mouse(click(hint.x, hint.y));

        assert!(rendered(&workbench), "{HINT_IGNORED}");
    }

    /// Each of these takes `Esc` before the workbench would, so the hint has
    /// to name what it closes, and the click has to stop there.
    #[test_case(open_palette ; "the palette")]
    #[test_case(open_goto ; "go to line")]
    #[test_case(open_find ; "the find bar")]
    #[test_case(open_tab_menu ; "the menu")]
    #[test_case(ask_to_close_edited ; "the dialog")]
    #[test_case(open_name ; "the name prompt")]
    fn a_hint_answers_whatever_stands_in_front(open: fn(&mut Workbench)) {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        open(&mut workbench);
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let hint = hint_rect(&workbench, keys::CLOSE);

        let action = workbench.handle_mouse(click(hint.x, hint.y));

        assert_eq!(action, WorkbenchAction::Consumed, "{HINT_REACHED_BEHIND}");
        assert!(
            workbench.focused_field().is_none()
                && workbench.menu.is_none()
                && workbench.confirm.is_none(),
            "{HINT_IGNORED}"
        );
        assert_eq!(workbench.editor.tabs().len(), 1, "{HINT_REACHED_BEHIND}");
    }

    #[test]
    fn a_click_on_enter_takes_what_the_menu_has_selected() {
        let (dir, mut workbench) = project();
        open_row_menu(&dir, &mut workbench);
        let hint = hint_rect(&workbench, keys::ACCEPT);

        workbench.handle_mouse(click(hint.x, hint.y));

        assert!(workbench.menu.is_none(), "{MENU_STUCK}");
        assert_eq!(workbench.active_title(), OPENED_FILE, "{HINT_IGNORED}");
    }

    #[test_case(open_palette, &PALETTE_HINTS ; "the palette")]
    #[test_case(open_goto, &GOTO_HINTS ; "go to line")]
    #[test_case(open_find, &FIND_HINTS ; "the find bar")]
    fn the_status_row_says_what_esc_does_over_a_field(open: fn(&mut Workbench), expected: &[Hint]) {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);

        open(&mut workbench);

        assert_eq!(workbench.offered_hints(), expected, "{WRONG_HINT}");
    }

    #[test]
    fn hints_with_no_room_beside_the_path_cannot_be_pressed() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        paint(&mut workbench, NARROW_TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let status = workbench.panes.status;

        let action = workbench.handle_mouse(click(status.right() - 1, status.y));

        assert!(workbench.hint_hits.is_empty(), "{HIDDEN_HINT_PRESSED}");
        assert_eq!(action, WorkbenchAction::Consumed, "{HIDDEN_HINT_PRESSED}");
    }

    /// A row's menu opened low enough in a tall tree, and far enough right,
    /// lays its bottom rule over the hints at the end of the status row.
    #[test]
    fn a_menu_over_the_status_row_keeps_its_presses() {
        let (_dir, mut workbench) = tall_project();
        workbench.sidebar_width = MAX_SIDEBAR_WIDTH;
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let rows = workbench.panes.rows;
        let column = rows.right() - 1;
        workbench.handle_mouse(right_click(column, rows.y));
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let take = hint_rect(&workbench, keys::ACCEPT);
        let low = rows.y + take.y - workbench.panes.menu.bottom();
        workbench.handle_mouse(right_click(column, low));
        paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let panel = workbench.panes.menu;
        assert!(
            panel.bottom() == take.y && (panel.x..panel.right()).contains(&take.x),
            "{PANEL_OFF_HINT}"
        );

        let action = workbench.handle_mouse(click(take.x, take.y));

        assert_eq!(action, WorkbenchAction::Consumed, "{HINT_UNDER_MENU}");
        assert!(workbench.editor.tabs().is_empty(), "{HINT_UNDER_MENU}");
        assert!(workbench.menu.is_none(), "{MENU_STUCK}");
    }

    #[test]
    fn esc_takes_the_menu_down() {
        let (dir, mut workbench) = project();
        open_row_menu(&dir, &mut workbench);

        workbench.handle_key(key(KeyCode::Esc));

        assert!(workbench.menu.is_none(), "{MENU_STUCK}");
    }

    /// A menu over a standing question would offer answers to a question
    /// nobody could see.
    #[test]
    fn no_menu_opens_while_a_dialog_is_up() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.handle_key(key(KeyCode::Char(EDIT)));
        workbench.handle_leader(press(keys::CLOSE_TAB));

        workbench.handle_leader(press(keys::MENU));

        assert!(workbench.menu.is_none(), "{MODAL_LEAKED}");
    }

    #[test]
    fn the_name_being_typed_is_painted_with_a_caret_after_it() {
        let (dir, mut workbench) = project();
        select_file(&dir, &mut workbench);
        workbench.handle_leader(press(keys::MENU));
        menu_action(&mut workbench, MenuAction::NewFile);
        for typed in MADE_NAME.chars() {
            workbench.handle_key(key(KeyCode::Char(typed)));
        }

        let surface = paint(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let typed = format!("{NEW_FILE_PROMPT}{MADE_NAME}");
        let caret = (0..TERMINAL_HEIGHT)
            .find_map(|row| {
                let line = row_of(&surface, Rect::new(0, row, TERMINAL_WIDTH, 1), 0);
                let start = line.find(&typed)?;
                let column = line[..start].width() + typed.width();
                Some(surface[(u16::try_from(column).ok()?, row)].style())
            })
            .expect(WRONG_PROMPT);

        let cursor = WorkbenchStyles::default().cursor.add_modifier;
        assert!(caret.add_modifier.contains(cursor), "{WRONG_PROMPT}");
    }

    #[test]
    fn a_batch_close_asks_about_every_tab_that_has_work_in_it() {
        let (_dir, mut workbench) = many_tabs(3);
        edit_tab(&mut workbench, 1);
        edit_tab(&mut workbench, 2);

        workbench.run_menu(MenuAction::CloseAll, &Target::Tab(0));
        workbench.handle_key(key(KeyCode::Char(Choice::Discard.accelerator())));

        assert_eq!(open_titles(&workbench), ["file2.txt"], "{QUEUE_STALLED}");
        assert_eq!(asked(&workbench).ask, Ask::Close, "{NOT_ASKED}");

        workbench.handle_key(key(KeyCode::Char(Choice::Discard.accelerator())));

        assert!(open_titles(&workbench).is_empty(), "{QUEUE_STALLED}");
    }

    /// The answer was about the tab in front, and the tabs behind it were
    /// never asked about.
    #[test]
    fn cancelling_one_close_drops_the_rest_of_the_batch() {
        let (_dir, mut workbench) = many_tabs(3);
        edit_tab(&mut workbench, 1);
        workbench.run_menu(MenuAction::CloseAll, &Target::Tab(0));

        workbench.handle_key(key(KeyCode::Char(Choice::Cancel.accelerator())));

        assert_eq!(
            open_titles(&workbench),
            ["file1.txt", "file2.txt"],
            "{QUEUE_RAN_ON}"
        );
        assert!(workbench.closing.is_empty(), "{QUEUE_RAN_ON}");
    }

    #[track_caller]
    fn settle_remote(workbench: &mut Workbench, ready: impl Fn(&Workbench) -> bool) {
        let deadline = Instant::now() + REMOTE_SETTLE_TIMEOUT;
        while Instant::now() < deadline {
            workbench.tick();
            if ready(workbench) {
                return;
            }
            smol::block_on(smol::future::yield_now());
            std::thread::yield_now();
        }
        panic!(
            "remote workbench did not settle: tabs={:?}, pending={:?}, opens={:?}, entries={:?}, flash={:?}",
            workbench
                .editor
                .tabs()
                .iter()
                .map(|tab| (&tab.path, tab.preview))
                .collect::<Vec<_>>(),
            workbench.remote_pending,
            workbench.pending_open,
            workbench
                .remote_entries
                .values()
                .map(|entry| &entry.path)
                .collect::<Vec<_>>(),
            workbench.flash,
        );
    }

    fn remote_notice(workbench: &mut Workbench) -> String {
        let deadline = Instant::now() + REMOTE_SETTLE_TIMEOUT;
        while Instant::now() < deadline {
            if let (_, Some(message)) = workbench.tick() {
                return message;
            }
            std::thread::yield_now();
        }
        panic!("remote workbench did not report the expected notice");
    }

    fn capped_remote_workbench() -> (Workbench, RemoteControl) {
        let (session, control) = crate::fs::backend::tests::widget_fixture();
        control.insert(REMOTE_TEST_NESTED, REMOTE_REPLACEMENT);
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.bind_workspace(session.clone()).unwrap();
        workbench
            .remote_backend
            .as_mut()
            .unwrap()
            .set_retained_limit(CAPPED_TREE_ENTRIES);
        workbench.toggle_workspace(session).unwrap();
        settle_remote(&mut workbench, |workbench| !workbench.is_busy());
        assert_eq!(workbench.remote_entries.len(), CAPPED_TREE_ENTRIES);
        assert!(workbench.remote_backend.as_ref().unwrap().is_stale());
        (workbench, control)
    }

    #[test]
    fn scoped_workbench_preserves_listed_id_and_reloads_the_same_canonical_path() {
        let (session, control) = scoped_widget_fixture();
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.toggle_workspace(session).unwrap();
        settle_remote(&mut workbench, |workbench| !workbench.is_busy());
        assert_eq!(workbench.backend_root().display(), SCOPED_ROOT);
        let path = WorkbenchPath::Remote(WorkspacePath::new(SCOPED_FILE).unwrap());
        let resolves = control.read_calls();
        workbench.open_workbench_path(&path, super::OpenPurpose::Open);
        settle_remote(&mut workbench, |workbench| !workbench.is_busy());
        assert!(resolves.is_empty(), "{TARGETED_READ_INDEPENDENT}");
        assert_eq!(
            workbench.editor.active().unwrap().contents(),
            SCOPED_CONTENTS
        );
        control.replace(SCOPED_FILE, TARGETED_REPLACEMENT);
        workbench.invalidate_remote(Some(&path));
        workbench.reload_remote_targets();
        let reload = resolves.recv().unwrap();
        assert_eq!(reload.path.as_str(), SCOPED_FILE);
        reload.reply.send(()).unwrap();
        settle_remote(&mut workbench, |workbench| !workbench.is_busy());
        assert_eq!(
            workbench.editor.active().unwrap().contents(),
            TARGETED_REPLACEMENT
        );
        assert_eq!(control.contents(SHADOW_FILE), SHADOW_CONTENTS);
    }

    #[test_case(false; "clean_uncached_tab_reloads")]
    #[test_case(true; "dirty_uncached_tab_keeps_its_baseline")]
    fn capped_tree_watch_changes_refresh_open_targets_without_completing_the_index(dirty: bool) {
        let (mut workbench, control) = capped_remote_workbench();
        let path = WorkbenchPath::Remote(WorkspacePath::new(REMOTE_TEST_NESTED).unwrap());
        assert!(!workbench.remote_entries.contains_key(&path));
        workbench.open_workbench_path(&path, super::OpenPurpose::Open);
        settle_remote(&mut workbench, |workbench| !workbench.is_busy());
        if dirty {
            workbench.editor_key(key(KeyCode::Char('X')));
        }
        let original = workbench.editor.active().unwrap().contents();
        let revision = workbench
            .editor
            .active()
            .unwrap()
            .resource
            .as_ref()
            .unwrap()
            .revision
            .clone();
        let blocked_listing = control.list_calls();
        control.replace(REMOTE_TEST_NESTED, TARGETED_REPLACEMENT);
        control.watch_change(REMOTE_TEST_NESTED);
        settle_remote(&mut workbench, |workbench| {
            let tab = workbench.editor.active().unwrap();
            if dirty {
                tab.conflict
            } else {
                tab.contents() == TARGETED_REPLACEMENT
            }
        });
        let _listing = blocked_listing.recv().unwrap();
        assert!(workbench.remote_backend.as_ref().unwrap().is_listing());
        let tab = workbench.editor.active().unwrap();
        if dirty {
            assert_eq!(tab.contents(), original, "{LOST_EDIT}");
            assert_eq!(
                tab.resource.as_ref().unwrap().revision,
                revision,
                "{REMOTE_MUTATION_CONFLICT}"
            );
            assert!(tab.is_dirty());
        } else {
            assert!(!tab.is_dirty() && !tab.conflict);
        }
        assert!(!tab.remote_reload, "{TARGETED_READ_INDEPENDENT}");
        assert!(
            workbench.pending_open.is_empty(),
            "{TARGETED_READ_INDEPENDENT}"
        );
        workbench.close();
    }

    #[test]
    fn uncached_renamed_descendant_revert_reads_its_new_path_without_root_rescans() {
        let (mut workbench, control) = capped_remote_workbench();
        let path = WorkbenchPath::Remote(WorkspacePath::new(REMOTE_TEST_NESTED).unwrap());
        workbench.open_workbench_path(&path, super::OpenPurpose::Open);
        settle_remote(&mut workbench, |workbench| !workbench.is_busy());
        workbench.editor_key(key(KeyCode::Char('X')));
        let revision = workbench
            .editor
            .active()
            .unwrap()
            .resource
            .as_ref()
            .unwrap()
            .revision
            .clone();
        workbench.commit_remote_input(Input::new(
            InputKind::Rename,
            WorkbenchPath::Remote(WorkspacePath::new(REMOTE_TEST_DIRECTORY).unwrap()),
            REMOTE_MOVED_DIRECTORY,
        ));
        settle_remote(&mut workbench, |workbench| !workbench.is_busy());
        let moved = WorkbenchPath::Remote(WorkspacePath::new(REMOTE_MOVED_NESTED).unwrap());
        assert_eq!(workbench.editor.active().unwrap().path, moved);
        assert!(!workbench.remote_entries.contains_key(&moved));
        control.replace(REMOTE_MOVED_NESTED, TARGETED_REPLACEMENT);
        let listings = control.list_calls();
        let reads = control.read_calls();
        workbench.buffer_key(press(keys::REVERT));
        let read = reads.recv().unwrap();
        assert_eq!(
            read.path.as_str(),
            REMOTE_MOVED_NESTED,
            "{TARGETED_READ_INDEPENDENT}"
        );
        assert_eq!(
            workbench
                .editor
                .active()
                .unwrap()
                .resource
                .as_ref()
                .unwrap()
                .revision,
            revision,
            "{REMOTE_MUTATION_CONFLICT}"
        );
        read.reply.send(()).unwrap();
        settle_remote(&mut workbench, |workbench| {
            workbench.pending_open.is_empty()
        });
        let tab = workbench.editor.active().unwrap();
        assert_eq!(tab.contents(), TARGETED_REPLACEMENT);
        assert!(!tab.is_dirty() && !tab.conflict);
        assert!(listings.is_empty(), "{TARGETED_READ_INDEPENDENT}");
    }

    #[test_case(false; "clean_reconciliation_moves_to_destination")]
    #[test_case(true; "dirty_baseline_survives_cancelled_old_read")]
    fn directory_rename_cancels_old_subtree_reads_and_reconciles_the_new_paths(dirty: bool) {
        let (mut workbench, control) = capped_remote_workbench();
        let source = WorkbenchPath::Remote(WorkspacePath::new(REMOTE_TEST_NESTED).unwrap());
        workbench.open_workbench_path(&source, super::OpenPurpose::Open);
        settle_remote(&mut workbench, |workbench| !workbench.is_busy());
        let baseline = workbench
            .editor
            .active()
            .unwrap()
            .resource
            .as_ref()
            .unwrap()
            .revision
            .clone();
        let reads = control.read_calls();
        control.replace(REMOTE_TEST_NESTED, TARGETED_REPLACEMENT);
        workbench.invalidate_remote(Some(&source));
        workbench.reload_remote_targets();
        let old = reads.recv().unwrap();
        assert_eq!(old.path.as_str(), REMOTE_TEST_NESTED);
        if dirty {
            workbench.editor_key(key(KeyCode::Char('X')));
        }
        let buffer = workbench.editor.active().unwrap().contents();
        workbench.commit_remote_input(Input::new(
            InputKind::Rename,
            WorkbenchPath::Remote(WorkspacePath::new(REMOTE_TEST_DIRECTORY).unwrap()),
            REMOTE_MOVED_DIRECTORY,
        ));
        let renamed_directory = reads.recv().unwrap();
        assert_eq!(renamed_directory.path.as_str(), REMOTE_MOVED_DIRECTORY);
        renamed_directory.reply.send(()).unwrap();
        settle_remote(&mut workbench, |workbench| {
            workbench.editor.active().unwrap().path.display() == REMOTE_MOVED_NESTED
        });
        let _ = old.reply.send(());
        if !dirty {
            let current = reads.recv().unwrap();
            assert_eq!(current.path.as_str(), REMOTE_MOVED_NESTED);
            current.reply.send(()).unwrap();
        }
        settle_remote(&mut workbench, |workbench| !workbench.is_busy());
        assert!(reads.is_empty(), "{TARGETED_READ_BOUNDED}");
        let tab = workbench.editor.active().unwrap();
        if dirty {
            assert_eq!(tab.contents(), buffer, "{LOST_EDIT}");
            assert_eq!(
                tab.resource.as_ref().unwrap().revision,
                baseline,
                "{REMOTE_MUTATION_CONFLICT}"
            );
            assert!(tab.is_dirty() && tab.conflict);
        } else {
            assert_eq!(tab.contents(), TARGETED_REPLACEMENT);
            assert!(!tab.is_dirty() && !tab.remote_reload);
        }
    }

    #[test]
    fn invalidation_bursts_coalesce_and_bound_targeted_read_concurrency() {
        let (mut workbench, control) = capped_remote_workbench();
        for index in 0..TARGETED_READ_TABS {
            let name = format!("target-{index}");
            control.insert(&name, REMOTE_REPLACEMENT);
            workbench.open_workbench_path(
                &WorkbenchPath::Remote(WorkspacePath::new(name).unwrap()),
                super::OpenPurpose::Open,
            );
            settle_remote(&mut workbench, |workbench| !workbench.is_busy());
        }
        let reads = control.read_calls();
        workbench.invalidate_remote(None);
        workbench.reload_remote_targets();
        let held = (0..super::MAX_REMOTE_READS)
            .map(|_| reads.recv().unwrap())
            .collect::<Vec<_>>();
        for index in 0..INVALIDATION_BURST {
            let unrelated =
                WorkbenchPath::Remote(WorkspacePath::new(format!("unrelated-{index}")).unwrap());
            workbench.invalidate_remote(Some(&unrelated));
            workbench.invalidate_remote(None);
            workbench.reload_remote_targets();
        }
        assert_eq!(
            workbench.pending_open.len(),
            super::MAX_REMOTE_READS,
            "{TARGETED_READ_BOUNDED}"
        );
        assert!(reads.is_empty(), "{TARGETED_READ_BOUNDED}");
        for index in 0..TARGETED_READ_TABS {
            control.replace(&format!("target-{index}"), TARGETED_REPLACEMENT);
        }
        let read_count = Counter::new(held.len());
        for read in held {
            read.reply.send(()).unwrap();
        }
        settle_remote(&mut workbench, |workbench| {
            assert!(
                workbench.pending_open.len() <= super::MAX_REMOTE_READS,
                "{TARGETED_READ_BOUNDED}"
            );
            for read in reads.try_iter() {
                read.reply.send(()).unwrap();
                read_count.set(read_count.get() + 1);
            }
            workbench.pending_open.is_empty()
                && workbench.editor.tabs().iter().all(|tab| !tab.remote_reload)
        });
        assert_eq!(
            read_count.get(),
            TARGETED_READ_TABS + super::MAX_REMOTE_READS,
            "{TARGETED_READ_BOUNDED}"
        );
        assert!(
            workbench
                .editor
                .tabs()
                .iter()
                .all(|tab| tab.contents() == TARGETED_REPLACEMENT && !tab.conflict)
        );
    }

    #[test]
    fn remote_first_page_is_visible_while_later_page_and_watch_are_blocked() {
        let (session, control) = crate::fs::backend::tests::widget_fixture();
        control.insert(REMOTE_TEST_NESTED, REMOTE_REPLACEMENT);
        let calls = control.list_calls();
        let watches = control.watch_calls();
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.toggle_workspace(session).unwrap();
        let watch = watches.recv().unwrap();
        let first = calls.recv().unwrap();
        assert!(!first.request.recursive);
        first.reply.send(Ok(())).unwrap();
        settle_remote(&mut workbench, |workbench| {
            !workbench.tree.rows().is_empty()
        });
        let later = calls.recv().unwrap();
        assert!(later.request.continuation.is_some());
        assert_eq!(workbench.tree.rows()[0].name, REMOTE_TEST_FILE);
        assert!(draw(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT).contains(REMOTE_TEST_FILE));
        watch.send(Err(WorkspaceError::WatchUnavailable)).unwrap();
        assert_eq!(remote_notice(&mut workbench), super::WATCH_WARNING);
        later.reply.send(Err(WorkspaceError::Unavailable)).unwrap();
        settle_remote(&mut workbench, |workbench| !workbench.is_busy());
        assert_eq!(workbench.tree.rows()[0].name, REMOTE_TEST_FILE);
        workbench.refresh_remote_tree();
        let retry = calls.recv().unwrap();
        assert!(retry.request.continuation.is_none());
        workbench.close();
        assert!(workbench.remote_pending.is_empty());
    }

    #[test]
    fn first_watch_install_reconciles_changes_missed_after_initial_scan() {
        let (session, control) = crate::fs::backend::tests::widget_fixture();
        let watches = control.watch_calls();
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.toggle_workspace(session).unwrap();
        let watch = watches.recv().unwrap();
        settle_remote(&mut workbench, |workbench| !workbench.is_busy());
        let path = WorkbenchPath::Remote(WorkspacePath::new(REMOTE_TEST_NESTED).unwrap());
        assert!(!workbench.remote_entries.contains_key(&path));
        control.insert(REMOTE_TEST_NESTED, REMOTE_REPLACEMENT);
        watch.send(Ok(())).unwrap();
        settle_remote(&mut workbench, |workbench| {
            workbench.remote_entries.contains_key(&path) && !workbench.is_busy()
        });
        workbench.close();
    }

    #[test]
    fn nested_opens_resolve_independently_of_shallow_listing() {
        let (session, control) = crate::fs::backend::tests::widget_fixture();
        control.insert(REMOTE_TEST_NESTED, REMOTE_REPLACEMENT);
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.toggle_workspace(session).unwrap();
        let path = WorkbenchPath::Remote(WorkspacePath::new(REMOTE_TEST_NESTED).unwrap());
        workbench.request_remote_path(path.clone(), super::OpenPurpose::Open);
        settle_remote(&mut workbench, |workbench| !workbench.is_busy());
        assert_eq!(workbench.editor.active().expect(NO_TAB).path, path);
        assert!(workbench.tree.rows().iter().any(|row| row.path == path));
        assert!(workbench.pending_open.is_empty());
    }

    #[test]
    fn listing_refresh_does_not_replace_the_revision_of_an_opened_file() {
        let (session, control) = crate::fs::backend::tests::widget_fixture();
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.toggle_workspace(session).unwrap();
        settle_remote(&mut workbench, |workbench| !workbench.is_busy());
        let path = WorkbenchPath::Remote(WorkspacePath::new(REMOTE_TEST_FILE).unwrap());
        workbench.open_workbench_path(&path, super::OpenPurpose::Open);
        settle_remote(&mut workbench, |workbench| !workbench.is_busy());
        let revision = workbench
            .editor
            .active()
            .unwrap()
            .resource
            .as_ref()
            .unwrap()
            .revision
            .clone();
        control.replace(REMOTE_TEST_FILE, REMOTE_REPLACEMENT);
        workbench.refresh_remote_tree();
        settle_remote(&mut workbench, |workbench| !workbench.is_busy());
        assert_eq!(
            workbench
                .editor
                .active()
                .unwrap()
                .resource
                .as_ref()
                .unwrap()
                .revision,
            revision
        );
        workbench.editor_key(key(KeyCode::Char('X')));
        workbench.save_active();
        settle_remote(&mut workbench, |workbench| !workbench.is_busy());
        let tab = workbench.editor.active().unwrap();
        assert!(tab.conflict && tab.is_dirty(), "{NO_CONFLICT}");
        assert_eq!(
            control.contents(REMOTE_TEST_FILE),
            REMOTE_REPLACEMENT,
            "{LOST_EDIT}"
        );
    }

    #[test_case(false; "rename")]
    #[test_case(true; "delete")]
    fn opened_file_mutations_refuse_external_changes(delete: bool) {
        let (session, control) = crate::fs::backend::tests::widget_fixture();
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.toggle_workspace(session).unwrap();
        settle_remote(&mut workbench, |workbench| !workbench.is_busy());
        let path = WorkbenchPath::Remote(WorkspacePath::new(REMOTE_TEST_FILE).unwrap());
        workbench.open_workbench_path(&path, super::OpenPurpose::Open);
        settle_remote(&mut workbench, |workbench| !workbench.is_busy());
        workbench.editor_key(key(KeyCode::Char('X')));
        let contents = workbench.editor.active().unwrap().contents();
        let revision = workbench
            .editor
            .active()
            .unwrap()
            .resource
            .as_ref()
            .unwrap()
            .revision
            .clone();
        control.replace(REMOTE_TEST_FILE, REMOTE_REPLACEMENT);
        if delete {
            workbench.run_on_row(MenuAction::Delete, path.clone());
            settle_remote(&mut workbench, |workbench| workbench.confirm.is_some());
            workbench.resolve_delete(Choice::Discard);
        } else {
            workbench.commit_remote_input(Input::new(
                InputKind::Rename,
                path.clone(),
                REMOTE_MOVED_DIRECTORY,
            ));
        }
        settle_remote(&mut workbench, |workbench| !workbench.is_busy());
        let tab = workbench.editor.active().expect(NO_TAB);
        assert!(tab.conflict && tab.is_dirty(), "{REMOTE_MUTATION_CONFLICT}");
        assert_eq!(tab.path, path);
        assert_eq!(tab.contents(), contents, "{LOST_EDIT}");
        assert_eq!(
            tab.resource.as_ref().unwrap().revision,
            revision,
            "{REMOTE_MUTATION_CONFLICT}"
        );
        workbench.save_active();
        settle_remote(&mut workbench, |workbench| !workbench.is_busy());
        assert_eq!(
            control.contents(REMOTE_TEST_FILE),
            REMOTE_REPLACEMENT,
            "{REMOTE_MUTATION_CONFLICT}"
        );
    }

    #[test_case(false; "unchanged_descendant")]
    #[test_case(true; "externally_changed_descendant")]
    fn directory_rename_remaps_dirty_descendants_without_rebasing_their_contents(changed: bool) {
        let (session, control) = crate::fs::backend::tests::widget_fixture();
        control.insert(REMOTE_TEST_NESTED, REMOTE_REPLACEMENT);
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.toggle_workspace(session).unwrap();
        settle_remote(&mut workbench, |workbench| !workbench.is_busy());
        workbench.open_workbench_path(
            &WorkbenchPath::Remote(WorkspacePath::new(REMOTE_TEST_NESTED).unwrap()),
            super::OpenPurpose::Open,
        );
        settle_remote(&mut workbench, |workbench| !workbench.is_busy());
        workbench.editor_key(key(KeyCode::Char('X')));
        let contents = workbench.editor.active().unwrap().contents();
        let revision = workbench
            .editor
            .active()
            .unwrap()
            .resource
            .as_ref()
            .unwrap()
            .revision
            .clone();
        if changed {
            control.replace(REMOTE_TEST_NESTED, REMOTE_TEST_FILE);
        }
        workbench.commit_remote_input(Input::new(
            InputKind::Rename,
            WorkbenchPath::Remote(WorkspacePath::new(REMOTE_TEST_DIRECTORY).unwrap()),
            REMOTE_MOVED_DIRECTORY,
        ));
        settle_remote(&mut workbench, |workbench| !workbench.is_busy());
        let tab = workbench.editor.active().expect(NO_TAB);
        let resource = tab.resource.as_ref().unwrap();
        assert_eq!(resource.path, tab.path);
        assert_eq!(resource.path.display(), REMOTE_MOVED_NESTED);
        assert_eq!(resource.revision, revision, "{REMOTE_MUTATION_CONFLICT}");
        assert!(resource.resource_id.is_none());
        workbench.save_active();
        settle_remote(&mut workbench, |workbench| !workbench.is_busy());
        let tab = workbench.editor.active().expect(NO_TAB);
        assert_eq!(tab.conflict, changed, "{REMOTE_MUTATION_CONFLICT}");
        assert_eq!(
            control.contents(REMOTE_MOVED_NESTED),
            if changed { REMOTE_TEST_FILE } else { &contents }
        );
        if !changed {
            assert!(tab.resource.as_ref().unwrap().resource_id.is_some());
        }
    }

    #[test_case(false; "clean_tab_closes")]
    #[test_case(true; "dirty_tab_survives")]
    fn authoritative_missing_files_preserve_dirty_buffers(dirty: bool) {
        let (session, control) = crate::fs::backend::tests::widget_fixture();
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.toggle_workspace(session).unwrap();
        settle_remote(&mut workbench, |workbench| !workbench.is_busy());
        let path = WorkbenchPath::Remote(WorkspacePath::new(REMOTE_TEST_FILE).unwrap());
        workbench.open_workbench_path(&path, super::OpenPurpose::Open);
        settle_remote(&mut workbench, |workbench| !workbench.is_busy());
        if dirty {
            workbench.editor_key(key(KeyCode::Char('X')));
        }
        let contents = workbench.editor.active().unwrap().contents();
        control.remove(REMOTE_TEST_FILE);
        workbench.invalidate_remote(Some(&path));
        workbench.refresh_remote_tree();
        settle_remote(&mut workbench, |workbench| !workbench.is_busy());
        if dirty {
            let tab = workbench.editor.active().expect(NO_TAB);
            assert!(tab.conflict && tab.is_dirty());
            assert_eq!(tab.contents(), contents, "{LOST_EDIT}");
        } else {
            assert!(workbench.editor.tabs().is_empty());
        }
    }

    #[test_case(ResourceKind::Directory, "", DIRECTORY_ERROR; "directory")]
    #[test_case(ResourceKind::Other, "", SPECIAL_FILE_ERROR; "fifo")]
    #[test_case(ResourceKind::File, "\0", BINARY_ERROR; "binary")]
    #[test_case(ResourceKind::File, "large", HUGE_FILE_ERROR; "huge")]
    fn remote_file_errors_leave_the_tree_usable(
        kind: ResourceKind,
        contents: &str,
        expected: &str,
    ) {
        let (session, control) = crate::fs::backend::tests::widget_fixture();
        let contents = if expected == HUGE_FILE_ERROR {
            "x".repeat(OVERSIZED_BYTES)
        } else {
            contents.to_owned()
        };
        control.replace(REMOTE_TEST_FILE, &contents);
        control.set_kind(REMOTE_TEST_FILE, kind);
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.toggle_workspace(session).unwrap();
        settle_remote(&mut workbench, |workbench| !workbench.is_busy());
        let entries = workbench.remote_entries.clone();
        let path = WorkbenchPath::Remote(WorkspacePath::new(REMOTE_TEST_FILE).unwrap());
        workbench.open_workbench_path(&path, super::OpenPurpose::Open);
        assert_eq!(remote_notice(&mut workbench), expected);
        assert_eq!(workbench.remote_entries, entries);
        assert!(workbench.editor.tabs().is_empty());
        workbench.refresh_remote_tree();
        settle_remote(&mut workbench, |workbench| !workbench.is_busy());
        assert_eq!(workbench.remote_entries, entries);
    }

    #[test_case(true, false; "absent_on_open")]
    #[test_case(false, false; "broken_on_open")]
    #[test_case(true, true; "repository_removed")]
    #[test_case(false, true; "repository_broken_after_refresh")]
    fn remote_scm_distinguishes_absence_from_repository_errors(
        absent: bool,
        previously_open: bool,
    ) {
        let (session, control) = crate::fs::backend::tests::widget_fixture();
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        if previously_open {
            workbench.bind_workspace(session.clone()).unwrap();
            settle_remote(&mut workbench, |workbench| !workbench.scm_refreshing());
            assert!(workbench.scm.is_repository());
        }
        control.scm_error(if absent {
            WorkspaceError::NotRepository
        } else {
            WorkspaceError::Refused {
                code: REPOSITORY_ERROR_CODE,
                symbolic: REPOSITORY_UNAVAILABLE.to_owned(),
            }
        });
        if previously_open {
            workbench.refresh_remote_scm();
        } else {
            workbench.bind_workspace(session).unwrap();
        }
        settle_remote(&mut workbench, |workbench| !workbench.scm_refreshing());
        if absent {
            assert!(!workbench.scm.is_repository());
            assert!(workbench.scm.error().is_none());
            assert!(workbench.scm.log().is_empty());
            assert!(
                Section::ALL
                    .iter()
                    .all(|section| workbench.scm.count(*section) == 0)
            );
            workbench.open = true;
            workbench.sidebar = SidebarView::SourceControl;
            assert!(
                draw(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT).contains(NOT_A_REPOSITORY)
            );
        } else if previously_open {
            assert!(workbench.scm.is_repository());
        } else {
            assert_eq!(workbench.scm.error(), Some(REPOSITORY_ERROR));
        }
    }

    #[test]
    fn remote_discard_reads_the_current_revision_with_a_stale_listing() {
        const FILE: &str = "same-name.txt";
        const REPLACEMENT: &str = "theirs";

        let (session, control) = crate::fs::backend::tests::widget_fixture();
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.toggle_workspace(session).unwrap();
        settle_remote(&mut workbench, |workbench| !workbench.is_busy());
        let path = WorkbenchPath::Remote(caudra_workspace::WorkspacePath::new(FILE).unwrap());
        workbench.open_workbench_path(&path, super::OpenPurpose::Open);
        settle_remote(&mut workbench, |workbench| {
            workbench.editor.active().is_some()
        });
        workbench.editor_key(key(KeyCode::Char('X')));
        control.replace(FILE, REPLACEMENT);
        workbench.editor.active_mut().unwrap().conflict = true;

        workbench.buffer_key(press(keys::REVERT));

        settle_remote(&mut workbench, |workbench| {
            workbench.editor.active().is_some_and(|tab| {
                tab.buffer.line(0) == REPLACEMENT && !tab.is_dirty() && !tab.conflict
            })
        });
    }

    /// The composer's `#hash` index has no way to walk a remote log of its own,
    /// so it reads the one source control already paged. A refresh on request is
    /// what lets a commit made during the session be mentioned.
    #[test]
    fn a_bound_workspace_surfaces_its_log_and_refreshes_it_on_request() {
        const REMOTE_COMMIT: &str = "remote commit";

        let (session, control) = crate::fs::backend::tests::widget_fixture();
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.bind_workspace(session).unwrap();
        settle_remote(&mut workbench, |workbench| !workbench.scm_refreshing());

        assert_eq!(
            workbench
                .scm_log()
                .iter()
                .map(|commit| commit.summary.as_str())
                .collect::<Vec<_>>(),
            [REMOTE_COMMIT],
            "{NO_REMOTE_LOG}"
        );

        let before = control.scm_calls();
        workbench.refresh_scm();
        assert!(workbench.scm_refreshing(), "{REFRESH_IGNORED}");
        settle_remote(&mut workbench, |workbench| !workbench.scm_refreshing());
        assert!(control.scm_calls() > before, "{REFRESH_IGNORED}");
    }

    /// A local session has no remote driver, so there is nothing to wait on and
    /// nothing to ask. The host reads that as "no history here".
    #[test]
    fn an_unbound_workbench_owes_no_source_control_answer() {
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.refresh_scm();
        assert!(!workbench.scm_refreshing());
        assert!(workbench.scm_log().is_empty());
    }

    #[test]
    fn remote_open_reads_the_current_revision_with_a_stale_listing() {
        const FILE: &str = "same-name.txt";
        const REPLACEMENT: &str = "theirs";

        let (session, control) = crate::fs::backend::tests::widget_fixture();
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.toggle_workspace(session).unwrap();
        settle_remote(&mut workbench, |workbench| !workbench.is_busy());
        control.replace(FILE, REPLACEMENT);
        let path = WorkbenchPath::Remote(caudra_workspace::WorkspacePath::new(FILE).unwrap());

        workbench.open_workbench_path(&path, super::OpenPurpose::Open);

        settle_remote(&mut workbench, |workbench| !workbench.is_busy());
        let tab = workbench.editor.active().expect(NO_TAB);
        assert_eq!(tab.buffer.line(0), REPLACEMENT, "{STALE_TAB}");
    }

    #[test]
    fn remote_widgets_cover_tree_editor_mutations_search_watch_resync_and_rebind() {
        const FILE: &str = "same-name.txt";
        const SECOND: &str = "second.txt";
        const MADE: &str = "made.txt";
        const RENAMED: &str = "renamed.txt";

        crate::LocalFilesystem::reset_call_count();
        let (session, control) = crate::fs::backend::tests::widget_fixture();
        control.insert(SECOND, "second\n");
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.bind_workspace(session.clone()).unwrap();
        workbench.toggle_workspace(session).unwrap();
        settle_remote(&mut workbench, |workbench| workbench.tree.rows().len() == 3);
        settle_remote(&mut workbench, |workbench| workbench.scm.is_repository());
        workbench.scm.select(Section::Unstaged, Some(0));
        workbench.stage_selected();
        settle_remote(&mut workbench, |workbench| {
            workbench
                .remote_scm
                .as_ref()
                .is_some_and(|scm| !scm.is_busy())
        });
        assert!(control.scm_calls() >= 7);
        assert!(workbench.tree.rows().iter().all(|row| {
            row.path.remote().is_some()
                && row
                    .resource
                    .as_ref()
                    .and_then(|entry| entry.resource_id.as_ref())
                    .is_some()
        }));

        let file = WorkbenchPath::Remote(caudra_workspace::WorkspacePath::new(FILE).unwrap());
        workbench.open_workbench_path(&file, super::OpenPurpose::Open);
        settle_remote(&mut workbench, |workbench| {
            workbench.editor.active().is_some()
        });
        workbench.editor_key(key(KeyCode::Char('X')));
        workbench.save_active();
        settle_remote(&mut workbench, |workbench| {
            workbench.editor.active().is_some_and(|tab| !tab.is_dirty())
        });
        assert!(control.contents(FILE).starts_with('X'));

        workbench.editor_key(key(KeyCode::Char('Y')));
        control.replace(FILE, "theirs\n");
        workbench.save_active();
        settle_remote(&mut workbench, |workbench| {
            workbench.editor.active().is_some_and(|tab| tab.conflict)
        });
        workbench.buffer_key(press(keys::REVERT));
        settle_remote(&mut workbench, |workbench| {
            workbench
                .editor
                .active()
                .is_some_and(|tab| tab.buffer.line(0) == "theirs" && !tab.is_dirty())
        });

        let scm_calls_before_watch = control.scm_calls();
        control.replace(FILE, "watched\n");
        control.watch_change(FILE);
        settle_remote(&mut workbench, |workbench| {
            workbench
                .editor
                .active()
                .is_some_and(|tab| tab.buffer.line(0) == "watched")
        });
        settle_remote(&mut workbench, |_| {
            control.scm_calls() > scm_calls_before_watch
        });
        control.replace(FILE, "resynced\n");
        control.watch_resync();
        settle_remote(&mut workbench, |workbench| {
            workbench
                .editor
                .active()
                .is_some_and(|tab| tab.buffer.line(0) == "resynced")
        });

        let root = WorkbenchPath::Remote(caudra_workspace::WorkspacePath::root());
        workbench.ask_for_name(InputKind::NewFile, root.clone());
        workbench.input.as_mut().unwrap().value.set_text(MADE);
        workbench.commit_input();
        settle_remote(&mut workbench, |workbench| {
            workbench
                .editor
                .active()
                .is_some_and(|tab| tab.path.file_name() == MADE)
        });
        workbench.ask_for_name(InputKind::NewFolder, root);
        workbench.input.as_mut().unwrap().value.set_text("folder");
        workbench.commit_input();
        settle_remote(&mut workbench, |workbench| {
            workbench.tree.rows().iter().any(|row| row.name == "folder")
        });

        let made = WorkbenchPath::Remote(caudra_workspace::WorkspacePath::new(MADE).unwrap());
        workbench.ask_for_name(InputKind::Rename, made);
        workbench.input.as_mut().unwrap().value.set_text(RENAMED);
        workbench.commit_input();
        settle_remote(&mut workbench, |workbench| {
            workbench.tree.rows().iter().any(|row| row.name == RENAMED)
        });

        workbench.sidebar = SidebarView::Search;
        workbench
            .search
            .input_mut(search::Field::Query)
            .set_text("resynced");
        workbench.run_or_open_search();
        settle_remote(&mut workbench, |workbench| workbench.search.has_results());
        assert!(workbench.search.selection().is_some());

        assert_eq!(open_titles(&workbench), [FILE, RENAMED]);
        let second = WorkbenchPath::Remote(caudra_workspace::WorkspacePath::new(SECOND).unwrap());
        workbench.open_workbench_path(&second, super::OpenPurpose::Open);
        settle_remote(&mut workbench, |workbench| {
            workbench.pending_open.is_empty()
        });
        assert_eq!(open_titles(&workbench), [FILE, RENAMED, SECOND]);
        assert_eq!(workbench.editor.active().unwrap().path, second);

        let renamed = WorkbenchPath::Remote(caudra_workspace::WorkspacePath::new(RENAMED).unwrap());
        workbench.run_on_row(MenuAction::Delete, renamed);
        settle_remote(&mut workbench, |workbench| workbench.confirm.is_some());
        workbench.resolve_delete(Choice::Discard);
        settle_remote(&mut workbench, |workbench| {
            !workbench.tree.rows().iter().any(|row| row.name == RENAMED)
        });

        let (replacement, replacement_control) = crate::fs::backend::tests::widget_fixture();
        replacement_control.replace(FILE, "replacement\n");
        workbench.open_workbench_path(&file, super::OpenPurpose::Open);
        workbench.bind_workspace(replacement.clone()).unwrap();
        settle_remote(&mut workbench, |workbench| {
            workbench.editor.tabs().is_empty() && !workbench.tree.rows().is_empty()
        });
        settle_remote(&mut workbench, |workbench| workbench.scm.is_repository());
        workbench.sidebar = SidebarView::SourceControl;
        workbench.scm.select(Section::Unstaged, Some(0));
        workbench.open_diff();
        settle_remote(&mut workbench, |workbench| {
            workbench
                .editor
                .active()
                .is_some_and(|tab| tab.diff_rows().is_some())
        });
        let frame = draw(&mut workbench, TERMINAL_WIDTH, TERMINAL_HEIGHT);
        assert!(frame.contains("scm-head"));
        assert!(!frame.contains("test-authority"));
        assert!(!frame.contains("test-anchor"));
        assert!(replacement_control.scm_calls() >= 6);

        assert_eq!(crate::LocalFilesystem::call_count(), 0);
    }
}
