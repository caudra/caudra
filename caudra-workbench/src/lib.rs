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
mod pointer;
mod quick_open;
mod scm;
mod search;
mod style;
mod view;

pub use action::WorkbenchAction;
pub use style::WorkbenchStyles;
use unicode_width::UnicodeWidthStr;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use serde::{Deserialize, Serialize};

use editor::Editor;
use editor::Tab;
use editor::buffer::Cursor;
use fs::tree::Tree;
use fs::watch::Watch;
use pointer::Clicks;
use quick_open::QuickOpen;
use scm::{MIN_SECTION_ROWS, Scm, Section};
use search::Search;
use view::{TabHit, Toggle};

const DEFAULT_SIDEBAR_WIDTH: u16 = 30;
const MIN_SIDEBAR_WIDTH: u16 = 16;
const MAX_SIDEBAR_WIDTH: u16 = 80;
const MIN_EDITOR_WIDTH: u16 = 24;
const SEPARATOR_WIDTH: u16 = 1;
const STATUS_HEIGHT: u16 = 1;
const SCROLL_LINES: isize = 3;
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
const CANCEL_LABEL: &str = "Cancel";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SidebarView {
    #[default]
    Explorer,
    SourceControl,
    Search,
}

impl SidebarView {
    /// What the header switcher paints. All three together have to fit
    /// [`MIN_SIDEBAR_WIDTH`], so these are short rather than descriptive.
    const fn title(self) -> &'static str {
        match self {
            Self::Explorer => "FILES",
            Self::SourceControl => "GIT",
            Self::Search => "FIND",
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

/// One of the three answers to a tab that was asked to close while it still
/// had unsaved edits, in the order they are painted and measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    Save,
    Discard,
    Cancel,
}

impl Choice {
    const ALL: [Choice; 3] = [Choice::Save, Choice::Discard, Choice::Cancel];

    fn label(self) -> &'static str {
        match self {
            Choice::Save => SAVE_LABEL,
            Choice::Discard => DISCARD_LABEL,
            Choice::Cancel => CANCEL_LABEL,
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

    /// The neighbouring answer, stopping at both ends rather than wrapping so
    /// a held arrow key cannot walk past `Cancel` back onto `Save`.
    fn step(self, delta: isize) -> Self {
        let at = Self::ALL.iter().position(|choice| *choice == self);
        let reached = at.unwrap_or_default().saturating_add_signed(delta);
        Self::ALL[reached.min(Self::ALL.len() - 1)]
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
}

pub struct Workbench {
    open: bool,
    root: PathBuf,
    focus: Focus,
    sidebar: SidebarView,
    sidebar_width: u16,
    sidebar_collapsed: bool,
    show_hidden: bool,
    styles: WorkbenchStyles,
    /// Bumped on every palette change so open tabs know to rehighlight.
    theme_generation: u64,
    panes: PaneRects,
    tree: Tree,
    editor: Editor,
    palette: QuickOpen,
    scm: Scm,
    search: Search,
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
    goto: Option<String>,
    /// The answer under the cursor while a tab is being asked whether to close
    /// with unsaved edits. Which tab is not kept: [`Workbench::close_at`]
    /// selects it before asking, and the dialog is modal, so nothing can move
    /// the active one underneath it.
    confirm: Option<Choice>,
    flash: Option<String>,
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
            styles,
            theme_generation: 0,
            panes: PaneRects::default(),
            tree: Tree::default(),
            editor: Editor::default(),
            palette: QuickOpen::default(),
            scm: Scm::default(),
            search: Search::default(),
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
            flash: None,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn close(&mut self) {
        self.open = false;
        self.watch = None;
    }

    pub fn open(&mut self, root: &Path) {
        if self.root != root {
            self.root = root.to_path_buf();
            self.tree = Tree::new(root, self.show_hidden);
            self.scm.open(root);
            self.touched.clear();
        } else {
            // A watch only reports what happened while it was running, so the
            // panes are reread here rather than trusting the last visit.
            self.tree.reload();
            self.scm.refresh();
        }
        self.watch = Watch::start(&self.root);
        self.apply_marks();
        self.open = true;
    }

    pub fn toggle(&mut self, root: &Path) {
        if self.open {
            self.close();
        } else {
            self.open(root);
        }
    }

    pub fn set_styles(&mut self, styles: WorkbenchStyles) {
        self.styles = styles;
        self.theme_generation += 1;
        self.editor.set_theme_generation(self.theme_generation);
    }

    /// Whether a background worker owes an answer, so the host knows to look
    /// again rather than sleeping until the next key.
    pub fn is_busy(&self) -> bool {
        self.search.is_running() || self.edge_scroll_delta() != 0
    }

    /// Drains whatever the background workers have produced. Reports whether
    /// the screen changed, plus anything worth saying in the status bar.
    pub fn tick(&mut self) -> (bool, Option<String>) {
        let watched = self.absorb_changes();
        let searched = self.search.tick();
        let scrolled = self.edge_scroll();
        (watched || searched || scrolled, self.flash.take())
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
        for path in &changes.files {
            self.reload_tab(path);
        }
        self.touched.extend(changes.files);
        if changes.structural {
            self.tree.reload();
        }
        self.scm.refresh();
        self.apply_marks();
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
            if tab.diff_kinds().is_some() {
                continue;
            }
            if index == active_tab {
                active = tabs.len();
            }
            tabs.push(tab.path.clone());
        }
        let (flat, sections) = self.scm.saved();
        Layout {
            tabs,
            active,
            sidebar: self.sidebar,
            sidebar_width: self.sidebar_width,
            sidebar_collapsed: self.sidebar_collapsed,
            show_hidden: self.show_hidden,
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
        self.sidebar = layout.sidebar;
        self.sidebar_collapsed = layout.sidebar_collapsed;
        self.set_sidebar_width(layout.sidebar_width);
        self.show_hidden = layout.show_hidden;
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

    /// Inserts text the host pulled out of a bracketed paste. Reports whether
    /// anything took it, so the host can fall back to its own composer.
    pub fn paste(&mut self, text: &str) -> bool {
        if self.focus != Focus::Editor {
            return false;
        }
        let Some(tab) = self.editor.active_mut() else {
            return false;
        };
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
        let at = (event.column, event.row);
        let delta = match event.kind {
            MouseEventKind::ScrollUp => -SCROLL_LINES,
            MouseEventKind::ScrollDown => SCROLL_LINES,
            MouseEventKind::Moved => {
                self.hover = Some(at);
                return WorkbenchAction::Consumed;
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.hover = Some(at);
                self.drag_from = at;
                self.drag_at = at;
                let clicks = self.clicks.press(at, Instant::now());
                self.press(at, clicks);
                return WorkbenchAction::Consumed;
            }
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

    /// A wheel turn over `(column, row)`, worth `delta` rows and negative
    /// upwards. The host coalesces a burst of notches and scales them by the
    /// configured scroll size, so this is its own entry point rather than a
    /// [`MouseEvent`] the caller has to build.
    pub fn scroll(&mut self, column: u16, row: u16, delta: isize) {
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
                SidebarView::SourceControl => {}
            }
        } else if self.panes.text.contains(at.into()) {
            let rows = self.panes.text.height as usize;
            if let Some(tab) = self.editor.active_mut() {
                tab.scroll_by(delta, rows);
            }
        }
    }

    /// A left press, told how many landed on this cell in a row. The panes are
    /// tried in painting order, so the palette gets the press it is covering.
    fn press(&mut self, at: (u16, u16), clicks: u8) {
        let position = at.into();
        if self.confirm.is_some() {
            if self.panes.confirm.contains(position)
                && let Some(choice) = view::confirm_at(at.0, self.panes.confirm.x)
            {
                self.resolve_close(choice);
            }
            return;
        }
        if self.palette.is_open() {
            if self.panes.palette.contains(position) {
                self.open_palette_row((at.1 - self.panes.palette.y) as usize);
            }
            return;
        }
        if self
            .panes
            .separator
            .is_some_and(|rect| rect.contains(position))
        {
            self.drag = Drag::Separator;
            return;
        }
        if self.panes.tabs.contains(position) {
            self.focus = Focus::Editor;
            if let Some(hit) = view::tab_at(&self.editor, at.0, self.panes.tabs.x) {
                self.hit_tab(hit);
            }
            return;
        }
        if self.panes.header.contains(position) {
            if let Some(view) = view::header_at(at.0, self.panes.header.x) {
                self.focus = Focus::Sidebar;
                self.sidebar = view;
            } else if self.sidebar == SidebarView::SourceControl
                && view::mode_at(at.0, self.panes.header, self.head_width())
            {
                self.focus = Focus::Sidebar;
                self.scm.toggle_flat();
            }
            return;
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
            return;
        }
        if self.sidebar == SidebarView::SourceControl && self.press_scm(at) {
            return;
        }
        if self.panes.rows.contains(position) {
            self.press_row((at.1 - self.panes.rows.y) as usize);
            return;
        }
        if self.panes.text.contains(position) {
            self.press_text(at, clicks);
        }
    }

    /// A press somewhere in the source control pane. Reports whether it landed
    /// on a section, so the caller can go on trying the other panes.
    fn press_scm(&mut self, at: (u16, u16)) -> bool {
        if let Some(index) = self.header_under(at) {
            self.focus = Focus::Sidebar;
            self.scm.select(Section::ALL[index], None);
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
        self.scm.select(section, Some(row));
        let cursor = self.scm.cursor();
        if cursor.section != section || cursor.row != Some(row) {
            return true;
        }
        self.activate_scm();
        true
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
    /// done to the row under it.
    fn press_row(&mut self, offset: usize) {
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
                    false => self.open_selected(),
                }
            }
            // Source control routes through `press_scm`: its rows belong to a
            // section rather than to one list filling the sidebar.
            SidebarView::SourceControl => {}
            SidebarView::Search => {
                let row = self.search.scroll() + offset;
                self.search.select_index(row);
                if self.search.selected_index() == row {
                    self.open_search_selection();
                }
            }
        }
    }

    /// A press on the buffer. One click drops the cursor and starts a drag,
    /// two take the word under it, three take the whole line.
    fn press_text(&mut self, at: (u16, u16), clicks: u8) {
        self.focus = Focus::Editor;
        let Some(cursor) = self.cursor_at(at) else {
            return;
        };
        if clicks == 1 {
            self.drag = Drag::Text;
            self.drag_at = at;
        }
        let Some(tab) = self.editor.active_mut() else {
            return;
        };
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
        let chosen = self.palette.selected(&self.root);
        self.palette.close();
        if let Some(path) = chosen {
            self.open_path(&path);
        }
    }

    fn hit_tab(&mut self, hit: TabHit) {
        if hit.close {
            self.close_at(hit.index);
            return;
        }
        self.editor.select(hit.index);
        self.reveal_active();
    }

    fn close_under(&mut self, at: (u16, u16)) {
        if !self.panes.tabs.contains(at.into()) {
            return;
        }
        if let Some(hit) = view::tab_at(&self.editor, at.0, self.panes.tabs.x) {
            self.focus = Focus::Editor;
            self.close_at(hit.index);
        }
    }

    /// Closing through the same guard the keyboard uses, so an unsaved tab
    /// asks the pointer the same question it asks the keyboard.
    fn close_at(&mut self, index: usize) {
        self.editor.select(index);
        match self.editor.active().is_some_and(Tab::is_dirty) {
            true => self.confirm = Some(Choice::Save),
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
            Drag::None => {}
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
        self.drag = Drag::None;
        if !self.panes.text.contains(self.drag_from.into()) {
            return None;
        }
        // A plain click collapses the selection, so an idle press never
        // clobbers what was copied before it.
        let text = self.selected_text()?;
        self.clipboard = text.clone();
        Some(text)
    }

    fn extend_to(&mut self, at: (u16, u16)) {
        let Some(cursor) = self.cursor_at(at) else {
            return;
        };
        if let Some(tab) = self.editor.active_mut() {
            tab.buffer.set_cursor(cursor, true);
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
        let column = at.0.clamp(text.x, text.right() - 1);
        let line = (tab.scroll() + (row - text.y) as usize).min(tab.buffer.line_count() - 1);
        let reached = tab.h_scroll() + (column - text.x) as usize;
        let col = editor::render::char_index(tab.buffer.line(line), reached);
        Some(Cursor::new(line, col))
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
        let rows = self.panes.text.height as usize;
        let Some(tab) = self.editor.active_mut() else {
            return false;
        };
        let before = tab.scroll();
        tab.scroll_by(delta, rows);
        if tab.scroll() == before {
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
        // Everything else here consumes, because a full-screen takeover that
        // leaked keys to the hidden composer would type into it. Copy is the
        // exception: with nothing selected there is nothing to copy, and the
        // host spends the same chord on quitting.
        if keys::COPY.matches(key) && self.selected_text().is_none() {
            return WorkbenchAction::Passthrough;
        }
        self.flash = None;
        if let Some(action) = self.confirm_key(key) {
            return action;
        }
        if let Some(action) = self.palette_key(key) {
            return action;
        }
        if let Some(action) = self.goto_key(key) {
            return action;
        }
        if self.find_key(key) {
            return WorkbenchAction::Consumed;
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
            // Everything else that owns `Esc` has already been offered it, so
            // this is the last thing between the key and leaving. A live
            // selection is the editor's own transient state and goes first.
            if self.focus == Focus::Editor
                && let Some(tab) = self.editor.active_mut()
                && tab.buffer.has_selection()
            {
                tab.buffer.clear_selection();
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
        if keys::TOGGLE_HIDDEN.matches(key) {
            self.show_hidden = !self.show_hidden;
            self.tree.set_show_hidden(self.show_hidden);
            return Some(WorkbenchAction::Consumed);
        }
        for (bind, view) in [
            (keys::VIEW_EXPLORER, SidebarView::Explorer),
            (keys::VIEW_SOURCE_CONTROL, SidebarView::SourceControl),
            (keys::VIEW_SEARCH, SidebarView::Search),
        ] {
            if bind.matches(key) {
                self.sidebar = view;
                self.sidebar_collapsed = false;
                self.focus = Focus::Sidebar;
                return Some(WorkbenchAction::Consumed);
            }
        }
        if keys::QUICK_OPEN.matches(key) {
            self.palette.open(&self.root, self.show_hidden);
            return Some(WorkbenchAction::Consumed);
        }
        if keys::REFRESH.matches(key) {
            self.tree.reload();
            self.scm.refresh();
            self.apply_marks();
            return Some(WorkbenchAction::Consumed);
        }
        if keys::SEND_TO_COMPOSER.matches(key) {
            return Some(match self.reference() {
                Some(text) => WorkbenchAction::SendToComposer(text),
                None => WorkbenchAction::Consumed,
            });
        }
        if keys::SAVE.matches(key) {
            self.save_active();
            return Some(WorkbenchAction::Consumed);
        }
        for (bind, delta) in [(keys::NEXT_TAB, 1), (keys::PREV_TAB, -1)] {
            if bind.matches(key) {
                self.editor.cycle(delta);
                self.reveal_active();
                return Some(WorkbenchAction::Consumed);
            }
        }
        if keys::CLOSE_TAB.matches(key) {
            self.close_tab();
            return Some(WorkbenchAction::Consumed);
        }
        for (bind, step) in [
            (keys::SHRINK_SIDEBAR, -SIDEBAR_STEP),
            (keys::GROW_SIDEBAR, SIDEBAR_STEP),
        ] {
            if bind.matches(key) {
                self.set_sidebar_width(self.sidebar_width.saturating_add_signed(step));
                return Some(WorkbenchAction::Consumed);
            }
        }
        self.clipboard_key(key).or_else(|| self.buffer_key(key))
    }

    fn clipboard_key(&mut self, key: KeyEvent) -> Option<WorkbenchAction> {
        if keys::COPY.matches(key) {
            let text = self.selected_text()?;
            self.clipboard = text.clone();
            return Some(WorkbenchAction::Copy(text));
        }
        if keys::CUT.matches(key) {
            let text = self.selected_text()?;
            self.clipboard = text.clone();
            let tab = self.editor.active_mut()?;
            if tab.is_editable() {
                let edit = tab.buffer.delete();
                tab.record(edit);
                self.follow_cursor();
            }
            return Some(WorkbenchAction::Copy(text));
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
        if keys::UNDO.matches(key) || keys::REDO.matches(key) {
            let undo = keys::UNDO.matches(key);
            let tab = self.editor.active_mut()?;
            let moved = if undo { tab.undo() } else { tab.redo() };
            if moved {
                self.follow_cursor();
            }
            return Some(WorkbenchAction::Consumed);
        }
        if keys::SELECT_ALL.matches(key) {
            self.editor.active_mut()?.buffer.select_all();
            return Some(WorkbenchAction::Consumed);
        }
        if keys::REVERT.matches(key) {
            // The only way out of a conflict that keeps the other writer's
            // work, so it throws the buffer away rather than merging.
            let outcome = self.editor.active_mut()?.discard_and_reload();
            self.flash = outcome.err().map(|error| error.to_string());
            self.follow_cursor();
            return Some(WorkbenchAction::Consumed);
        }
        if keys::KILL_LINE.matches(key) {
            let tab = self.editor.active_mut()?;
            if tab.is_editable() {
                let edit = tab.buffer.kill_to_end_of_line();
                tab.record(edit);
                self.follow_cursor();
            }
            return Some(WorkbenchAction::Consumed);
        }
        if keys::FIND.matches(key) {
            let tab = self.editor.active_mut()?;
            tab.find.open();
            let query = tab.find.query().to_owned();
            tab.set_find_query(query);
            self.focus = Focus::Editor;
            return Some(WorkbenchAction::Consumed);
        }
        for (bind, delta) in [(keys::FIND_NEXT, 1), (keys::FIND_PREV, -1)] {
            if bind.matches(key) {
                let tab = self.editor.active_mut()?;
                // Closing the bar drops the matches, so stepping from a closed
                // one has to rescan before there is anything left to step to.
                if tab.find.current().is_none() {
                    let query = tab.find.query().to_owned();
                    tab.set_find_query(query);
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
            self.goto = Some(String::new());
            self.focus = Focus::Editor;
            return Some(WorkbenchAction::Consumed);
        }
        None
    }

    /// The palette is modal while it is up: it is one field over a list, and
    /// every key that is not navigation is part of the query.
    fn palette_key(&mut self, key: KeyEvent) -> Option<WorkbenchAction> {
        if !self.palette.is_open() {
            return None;
        }
        let typing = (key.modifiers - KeyModifiers::SHIFT).is_empty();
        let page = self.palette_rows().max(1) as isize;
        match key.code {
            KeyCode::Esc => self.palette.close(),
            KeyCode::Enter => {
                let chosen = self.palette.selected(&self.root);
                self.palette.close();
                if let Some(path) = chosen {
                    self.open_path(&path);
                }
            }
            KeyCode::Up => self.palette.move_selection(-1),
            KeyCode::Down => self.palette.move_selection(1),
            KeyCode::PageUp => self.palette.move_selection(-page),
            KeyCode::PageDown => self.palette.move_selection(page),
            KeyCode::Home => self.palette.select_first(),
            KeyCode::End => self.palette.select_last(),
            KeyCode::Backspace if typing => {
                let mut query = self.palette.query().to_owned();
                query.pop();
                self.palette.set_query(query);
            }
            KeyCode::Char(ch) if typing => {
                let mut query = self.palette.query().to_owned();
                query.push(ch);
                self.palette.set_query(query);
            }
            _ => return None,
        }
        Some(WorkbenchAction::Consumed)
    }

    /// The unsaved-changes dialog owns every key while it is up, and is asked
    /// first so `Esc` answers it rather than closing the workbench out from
    /// under the question.
    fn confirm_key(&mut self, key: KeyEvent) -> Option<WorkbenchAction> {
        let choice = self.confirm?;
        match key.code {
            KeyCode::Esc => self.confirm = None,
            KeyCode::Left | KeyCode::BackTab => self.confirm = Some(choice.step(-1)),
            KeyCode::Right | KeyCode::Tab => self.confirm = Some(choice.step(1)),
            KeyCode::Enter => self.resolve_close(choice),
            // Modifiers are ruled out so `Ctrl+C` over a live selection cannot
            // be read as the `Cancel` accelerator.
            KeyCode::Char(typed) if key.modifiers == KeyModifiers::NONE => {
                if let Some(picked) = Choice::ALL
                    .into_iter()
                    .find(|choice| choice.accelerator() == typed.to_ascii_lowercase())
                {
                    self.resolve_close(picked);
                }
            }
            _ => {}
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
                if let Ok(line) = self.goto.take()?.parse::<usize>() {
                    if let Some(tab) = self.editor.active_mut() {
                        tab.buffer.goto_line(line);
                    }
                    self.follow_cursor();
                }
            }
            KeyCode::Backspace => {
                self.goto.as_mut()?.pop();
            }
            KeyCode::Char(digit) if digit.is_ascii_digit() => self.goto.as_mut()?.push(digit),
            _ => {}
        }
        Some(WorkbenchAction::Consumed)
    }

    /// The find bar owns typing while it is open, and hands the buffer back on
    /// Esc. Chords it does not know fall through, so saving still works.
    fn find_key(&mut self, key: KeyEvent) -> bool {
        if self.focus != Focus::Editor || self.goto.is_some() {
            return false;
        }
        let Some(tab) = self.editor.active_mut() else {
            return false;
        };
        if !tab.find.is_open() {
            return false;
        }
        let typing = (key.modifiers - KeyModifiers::SHIFT).is_empty();
        let mut query = tab.find.query().to_owned();
        match key.code {
            KeyCode::Esc => {
                tab.find.close();
                return true;
            }
            KeyCode::Enter | KeyCode::Down | KeyCode::Up => {
                let back = key.code == KeyCode::Up || key.modifiers.contains(KeyModifiers::SHIFT);
                if let Some(found) = tab.find.step(if back { -1 } else { 1 }) {
                    tab.buffer.set_cursor(found.cursor(), false);
                    self.follow_cursor();
                }
                return true;
            }
            KeyCode::Backspace if typing => {
                query.pop();
            }
            KeyCode::Char(ch) if typing => query.push(ch),
            _ => return false,
        }
        tab.set_find_query(query);
        if let Some(found) = tab.find.current() {
            tab.buffer.set_cursor(found.cursor(), false);
        }
        self.follow_cursor();
        true
    }

    fn sidebar_key(&mut self, key: KeyEvent) -> WorkbenchAction {
        if keys::FOCUS_NEXT.matches(key) || key.code == KeyCode::BackTab {
            self.focus = Focus::Editor;
            return WorkbenchAction::Consumed;
        }
        match self.sidebar {
            SidebarView::Explorer => self.explorer_key(key),
            SidebarView::SourceControl => self.source_control_key(key),
            SidebarView::Search => self.search_key(key),
        }
        WorkbenchAction::Consumed
    }

    fn explorer_key(&mut self, key: KeyEvent) {
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
            self.discard_change();
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
        if keys::SHRINK_SECTION.matches(key) || keys::GROW_SECTION.matches(key) {
            let step = match keys::GROW_SECTION.matches(key) {
                true => SECTION_STEP,
                false => -SECTION_STEP,
            };
            let section = self.scm.cursor().section;
            let height = self.scm.height(section).saturating_add_signed(step);
            self.scm.set_height(section, height);
            return;
        }
        if keys::OPEN_DIFF.matches(key) {
            self.activate_scm();
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
            KeyCode::Right => drop(self.scm.unfold()),
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
        self.open_scm_selection();
    }

    /// Opens whatever the cursor is on as a read-only tab, which is a diff for
    /// a change and the whole commit for a row of the graph.
    fn open_scm_selection(&mut self) {
        match self.scm.cursor().section {
            Section::Graph => self.open_commit(),
            _ => self.open_diff(),
        }
    }

    /// The search pane is a form over a list: bare characters belong to
    /// whichever field holds the caret, so navigation is arrows and every
    /// command carries `Alt`.
    fn search_key(&mut self, key: KeyEvent) {
        for (bind, toggle) in [
            (keys::NEXT_FIELD, Search::next_field as fn(&mut Search)),
            (keys::TOGGLE_CASE, Search::toggle_case),
            (keys::TOGGLE_WORD, Search::toggle_word),
            (keys::TOGGLE_REGEX, Search::toggle_regex),
        ] {
            if bind.matches(key) {
                toggle(&mut self.search);
                return;
            }
        }
        let typing = (key.modifiers - KeyModifiers::SHIFT).is_empty();
        let page = self.sidebar_rows().max(1) as isize;
        match key.code {
            KeyCode::Up => self.search.move_selection(-1),
            KeyCode::Down => self.search.move_selection(1),
            KeyCode::PageUp => self.search.move_selection(-page),
            KeyCode::PageDown => self.search.move_selection(page),
            KeyCode::Home => self.search.select_first(),
            KeyCode::End => self.search.select_last(),
            KeyCode::Enter => self.run_or_open_search(),
            KeyCode::Backspace if typing => self.search.pop_char(),
            KeyCode::Char(ch) if typing => self.search.push_char(ch),
            _ => {}
        }
    }

    /// Enter means "search" while the fields have moved on from the results,
    /// and "open what I am looking at" once they agree.
    fn run_or_open_search(&mut self) {
        if self.search.is_stale() {
            self.search.start(&self.root, self.show_hidden);
            self.flash = self.search.error().map(str::to_owned);
            return;
        }
        self.open_search_selection();
    }

    fn open_search_selection(&mut self) {
        let Some((path, line)) = self.search.selection() else {
            return;
        };
        self.open_path(&path);
        if let Some(tab) = self.editor.active_mut() {
            tab.buffer.goto_line(line);
        }
        self.follow_cursor();
    }

    fn stage_selected(&mut self) {
        if let Err(error) = self.scm.stage() {
            self.flash = Some(error.to_string());
            return;
        }
        self.apply_marks();
    }

    fn discard_change(&mut self) {
        match self.scm.discard() {
            Ok(scm::Discard::Nothing) => {}
            Ok(scm::Discard::Armed(relative)) => {
                self.flash = Some(format!(
                    "Press {} again to discard changes to {relative}",
                    keys::DISCARD.label
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
        self.editor.push(Tab::synthetic(
            &change.path,
            title,
            rendered.lines,
            rendered.kinds,
            self.theme_generation,
        ));
        self.focus = Focus::Editor;
    }

    /// Opens the selected commit as a read-only tab.
    ///
    /// Every commit tab is filed under the repository's own directory, so a
    /// second commit replaces the first rather than stacking up, and the path
    /// can never collide with a file: [`Editor::push`] keeps one tab per path,
    /// and no file tab is ever opened on a directory.
    fn open_commit(&mut self) {
        let opened = match self.scm.selected_commit_diff() {
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
        let (commit, rendered) = opened;
        let title = format!("{} {}", commit.id, commit.summary);
        self.editor.push(Tab::synthetic(
            &workdir,
            title,
            rendered.lines,
            rendered.kinds,
            self.theme_generation,
        ));
        self.focus = Focus::Editor;
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
        for tab in self.editor.tabs_mut() {
            if tab.path == path && tab.is_editable() && tab.reload_from_disk().is_err() {
                tab.conflict = true;
            }
        }
    }

    fn apply_marks(&mut self) {
        let marks = self.scm.marks();
        self.tree.apply_git(&marks);
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
        let before = tab.revision();
        if tab.edit_key(key, rows) {
            if tab.revision() != before && tab.find.is_open() {
                tab.refresh_find();
            }
            self.follow_cursor();
        }
        WorkbenchAction::Consumed
    }

    fn open_selected(&mut self) {
        let Some(path) = self.tree.selected().map(|row| row.path.clone()) else {
            return;
        };
        self.open_path(&path);
    }

    fn open_path(&mut self, path: &Path) {
        match self.editor.open(path, self.theme_generation) {
            Ok(()) => {
                self.focus = Focus::Editor;
                self.tree.reveal(path);
                self.follow_cursor();
            }
            Err(error) => self.flash = Some(error.to_string()),
        }
    }

    fn close_tab(&mut self) {
        self.close_at(self.editor.active_index());
    }

    /// Acts on the answer the dialog was given. A save that fails keeps the tab
    /// open with the reason in the status row, because throwing the buffer away
    /// after failing to write it is the one outcome nobody asked for.
    fn resolve_close(&mut self, choice: Choice) {
        self.confirm = None;
        match choice {
            Choice::Cancel => return,
            Choice::Save if !self.save_active() => return,
            _ => {}
        }
        self.editor.close_active();
        self.reveal_active();
    }

    /// Keeps the tree on whatever the editor is showing, so the sidebar never
    /// points somewhere else after a tab switch.
    fn reveal_active(&mut self) {
        let Some(path) = self.editor.active().map(|tab| tab.path.clone()) else {
            return;
        };
        self.tree.reveal(&path);
    }

    /// Reports whether the write landed, which is what tells the unsaved-changes
    /// dialog that closing the tab is now safe.
    fn save_active(&mut self) -> bool {
        let Some(tab) = self.editor.active_mut() else {
            return false;
        };
        self.flash = match tab.save() {
            Ok(()) => return true,
            Err(error) => Some(error.to_string()),
        };
        false
    }

    fn active_title(&self) -> String {
        self.editor
            .active()
            .map(|tab| tab.title.clone())
            .unwrap_or_default()
    }

    fn selected_text(&self) -> Option<String> {
        self.editor.active()?.buffer.selected_text()
    }

    /// What `Alt+Enter` hands the composer: the tree's selection from the
    /// sidebar, and the cursor's line span from the editor.
    fn reference(&self) -> Option<String> {
        if self.focus == Focus::Sidebar {
            if self.sidebar == SidebarView::Search {
                let (path, line) = self.search.selection()?;
                return Some(format!("@{}:L{line}", self.relative(&path).display()));
            }
            let path = match self.sidebar {
                SidebarView::SourceControl => &self.scm.selected_change()?.path,
                _ => &self.tree.selected()?.path,
            };
            return Some(format!("@{}", self.relative(path).display()));
        }
        let tab = self.editor.active()?;
        let path = self.relative(&tab.path).display().to_string();
        let (first, last) = match tab.buffer.selection() {
            Some((from, to)) => (from.line + 1, to.line + 1),
            None => {
                let line = tab.buffer.cursor().line + 1;
                (line, line)
            }
        };
        Some(match first == last {
            true => format!("@{path}:L{first}"),
            false => format!("@{path}:L{first}-L{last}"),
        })
    }

    fn relative<'a>(&'a self, path: &'a Path) -> &'a Path {
        path.strip_prefix(&self.root).unwrap_or(path)
    }

    fn follow_cursor(&mut self) {
        let (rows, columns) = (self.panes.text.height, self.panes.text.width);
        if let Some(tab) = self.editor.active_mut() {
            tab.follow_cursor(rows as usize, columns as usize);
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

    /// The width of what the sidebar header prints on its right, which is where
    /// the tree-or-flat button has to be measured from.
    fn head_width(&self) -> usize {
        self.scm.head().map(UnicodeWidthStr::width).unwrap_or_default()
    }

    /// The layout clamps again against the room a frame actually has, so this
    /// only has to keep the remembered width sane.
    fn set_sidebar_width(&mut self, width: u16) {
        self.sidebar_width = width.clamp(MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH);
    }
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
fn layout_sections(area: Rect, wanted: [(u16, bool); Section::COUNT]) -> [SectionRect; Section::COUNT] {
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
        rects[index].body = Rect { y, height: body, ..area };
        y += body;
        room -= body;
    }
    rects
}

#[cfg(test)]
mod tests {
    use super::{
        Choice, Cursor, Drag, EDGE_SCROLL_LINES, Focus, Layout, MAX_SIDEBAR_WIDTH,
        MIN_EDITOR_WIDTH, MIN_SECTION_ROWS, MIN_SIDEBAR_WIDTH, SCROLL_LINES, ScmLayout, Section,
        SidebarView, Toggle, Workbench, WorkbenchAction, WorkbenchStyles, keys, layout,
        layout_sections, scm,
    };
    use crate::fs::tree::GitMark;
    use crate::search;
    use crate::view::{
        NOT_A_REPOSITORY, TabHit, confirm_at, header_at, mode_at, tab_at, toggle_at,
    };
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use gix::bstr::BString;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::{Buffer as Surface, Cell};
    use ratatui::layout::Rect;
    use ratatui::style::Style;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;
    use test_case::test_case;

    const CLOSED_START: &str = "a fresh workbench must not be on screen";
    const NARROW_DROPS_SIDEBAR: &str =
        "a terminal too narrow for both panes must keep the editor, not split into unusable strips";
    const STATUS_RESERVED: &str = "the status row must always be carved";
    const SIDEBAR_CLAMPED: &str = "the sidebar must never squeeze the editor below its minimum";
    const NO_TAB: &str = "the file under the cursor must have opened as a tab";
    const WRONG_REFERENCE: &str = "the composer reference does not point where the cursor is";
    const NOT_PAINTED: &str = "a frame is missing something it must always show";
    const WRONG_PANE: &str = "the sidebar is not showing what it was asked for";
    const CHANGE_MISSING: &str = "the change the test made is not under the cursor";
    const MARK_MISSING: &str = "the explorer row is missing its source control mark";
    const DIFF_EDITABLE: &str = "a diff tab must be read-only";
    const DISCARD_UNARMED: &str = "a discard must take exactly two presses of the same key";
    const NO_HITS: &str = "the search did not find what the fixture put there";
    const WRONG_LINE: &str = "the editor did not land on the line the match was on";
    const STALE_RESULTS: &str = "the pane disagrees about whether its results are current";
    const WRONG_CLICK: &str = "the pointer landed somewhere other than where it was pointing";
    const STALE_TAB: &str = "the tab is still showing what the file no longer says";
    const FALSE_CONFLICT: &str = "a tab with nothing to lose raised a conflict anyway";
    const NO_CONFLICT: &str = "unsaved work was overwritten without a word";
    const LOST_EDIT: &str = "a reload threw away work the user had not saved";
    const WRONG_WIDTH: &str = "the sidebar is not the width it was asked for";
    const WRONG_LAYOUT: &str = "the workbench did not come back the way it was left";
    const WRONG_HOVER: &str = "the row under the pointer is not marked the way it should be";
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

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn alt(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::ALT)
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

    /// The column a tab's close mark landed on, found the same way the pointer
    /// finds it.
    fn close_column(workbench: &Workbench, index: usize) -> u16 {
        let tabs = workbench.panes.tabs;
        (tabs.x..tabs.right())
            .find(|column| {
                tab_at(&workbench.editor, *column, tabs.x) == Some(TabHit { index, close: true })
            })
            .expect("a close mark on the tab")
    }

    /// The column an answer landed on, found the same way the pointer finds it.
    fn answer_column(workbench: &Workbench, answer: Choice) -> u16 {
        let answers = workbench.panes.confirm;
        (answers.x..answers.right())
            .find(|column| confirm_at(*column, answers.x) == Some(answer))
            .expect("an answer in the dialog")
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

    fn cursor(workbench: &Workbench) -> Cursor {
        workbench.editor.active().expect(NO_TAB).buffer.cursor()
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
        workbench.handle_key(KeyEvent::new(code, KeyModifiers::ALT));
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

        assert_eq!(workbench.handle_key(key(KeyCode::Esc)), expected, "{ESC_LEFT}");
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
        workbench.handle_key(KeyEvent::new(keys::VIEW_EXPLORER.code, KeyModifiers::ALT));
        assert_eq!(workbench.focus(), Focus::Sidebar);
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

        workbench.handle_key(KeyEvent::new(keys::CLOSE_TAB.code, KeyModifiers::ALT));

        assert_eq!(workbench.editor.tabs().len(), 1, "{UNASKED_CLOSE}");
        assert_eq!(workbench.confirm, Some(Choice::Save), "{NOT_ASKED}");
    }

    #[test]
    fn the_dialog_paints_the_file_and_every_answer() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.handle_key(key(KeyCode::Char('X')));
        workbench.handle_key(KeyEvent::new(keys::CLOSE_TAB.code, KeyModifiers::ALT));

        let frame = draw(&mut workbench, 80, 24);

        assert!(frame.contains("a.txt has unsaved changes"), "{NOT_PAINTED}");
        for answer in Choice::ALL {
            assert!(frame.contains(answer.label()), "{NOT_PAINTED}");
        }
    }

    #[test]
    fn a_clean_tab_closes_with_no_question() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);

        workbench.handle_key(KeyEvent::new(keys::CLOSE_TAB.code, KeyModifiers::ALT));

        assert!(workbench.editor.tabs().is_empty(), "{POINTLESS_QUESTION}");
        assert_eq!(workbench.confirm, None, "{POINTLESS_QUESTION}");
    }

    #[test]
    fn saving_from_the_dialog_writes_the_file_and_closes_the_tab() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.handle_key(key(KeyCode::Char('X')));
        workbench.handle_key(KeyEvent::new(keys::CLOSE_TAB.code, KeyModifiers::ALT));

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
        workbench.handle_key(KeyEvent::new(keys::CLOSE_TAB.code, KeyModifiers::ALT));

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
        workbench.handle_key(KeyEvent::new(keys::CLOSE_TAB.code, KeyModifiers::ALT));

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
        workbench.handle_key(KeyEvent::new(keys::CLOSE_TAB.code, KeyModifiers::ALT));
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
        workbench.handle_key(KeyEvent::new(keys::CLOSE_TAB.code, KeyModifiers::ALT));

        for _ in 0..Choice::ALL.len() + 1 {
            workbench.handle_key(key(code));
        }

        assert_eq!(workbench.confirm, Some(expected), "{WRONG_ANSWER}");
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

    #[test]
    fn cut_and_paste_move_the_selection() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.handle_key(KeyEvent::new(keys::SELECT_ALL.code, KeyModifiers::CONTROL));

        let cut = workbench.handle_key(KeyEvent::new(keys::CUT.code, KeyModifiers::CONTROL));
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

    #[test_case(Focus::Sidebar, "@a.txt" ; "the sidebar sends the selected path")]
    #[test_case(Focus::Editor, "@a.txt:L1" ; "the editor sends the cursor line")]
    fn alt_enter_hands_a_reference_to_the_composer(focus: Focus, expected: &str) {
        let (dir, mut workbench) = project();
        match focus {
            Focus::Sidebar => select_file(&dir, &mut workbench),
            Focus::Editor => open_file(&dir, &mut workbench),
        }

        let action = workbench.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));

        assert_eq!(
            action,
            WorkbenchAction::SendToComposer(expected.to_owned()),
            "{WRONG_REFERENCE}"
        );
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
            dir.path().join("sub/b.txt")
        );
        assert_eq!(
            workbench.tree.selected().map(|row| row.name.clone()),
            Some("b.txt".to_owned()),
            "opening from the palette must reveal the file in the tree"
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

        workbench.handle_key(KeyEvent::new(
            keys::TOGGLE_HIDDEN.code,
            KeyModifiers::CONTROL,
        ));

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
        fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").expect("a file");
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.open(dir.path());
        workbench.handle_key(alt(KeyCode::Char('2')));
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
                Some(cut) => folders
                    .entry(path[..cut].into())
                    .or_default()
                    .push((path[cut + 1..].into(), *mode, *id)),
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
        repo.commit_as(who, who, "HEAD", "initial", id, repo.head_id().ok())
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
            .find(|row| row.path == dir.path().join("a.txt"))
            .expect("the file must be in the tree");
        assert_eq!(row.git, Some(GitMark::Untracked), "{MARK_MISSING}");
    }

    #[test]
    fn space_moves_the_change_between_the_two_sections() {
        let (_dir, mut workbench) = repository();
        workbench.handle_key(key(keys::STAGE_TOGGLE.code));

        assert_eq!(workbench.scm.count(Section::Staged), 1, "{CHANGE_MISSING}");
        assert_eq!(workbench.scm.count(Section::Unstaged), 0, "{CHANGE_MISSING}");
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
        assert_eq!(workbench.scm.count(Section::Unstaged), 1, "{CHANGE_MISSING}");
    }

    #[test]
    fn space_on_a_header_stages_every_path_the_section_lists() {
        let (dir, mut workbench) = repository();
        fs::write(dir.path().join("b.txt"), "second\n").expect("write");
        workbench.handle_key(key(keys::REFRESH.code));
        assert_eq!(workbench.scm.count(Section::Unstaged), 2, "{CHANGE_MISSING}");

        workbench.scm.select(Section::Unstaged, None);
        workbench.handle_key(key(keys::STAGE_TOGGLE.code));

        assert_eq!(workbench.scm.count(Section::Staged), 2, "{CHANGE_MISSING}");
        assert_eq!(workbench.scm.count(Section::Unstaged), 0, "{CHANGE_MISSING}");
    }

    #[test]
    fn opening_a_diff_gives_a_tab_that_cannot_be_edited() {
        let (_dir, mut workbench) = repository();
        workbench.handle_key(key(KeyCode::Char('d')));

        let tab = workbench.editor.active().expect("a diff tab");
        assert!(!tab.is_editable(), "{DIFF_EDITABLE}");
        assert!(tab.diff_kinds().is_some(), "{DIFF_EDITABLE}");
        assert_eq!(workbench.focus(), Focus::Editor, "{WRONG_PANE}");
    }

    #[test]
    fn a_discard_takes_two_presses_and_restores_the_indexed_content() {
        let (dir, mut workbench) = repository();
        let path = dir.path().join("a.txt");
        workbench.handle_key(key(KeyCode::Char(' ')));
        fs::write(&path, "ruined\n").expect("a rewritten file");
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
            "ruined\n",
            "{DISCARD_UNARMED}"
        );

        workbench.handle_key(key(KeyCode::Char('x')));
        assert_eq!(
            fs::read_to_string(&path).expect("the file"),
            "one\ntwo\nthree\n",
            "{DISCARD_UNARMED}"
        );
    }

    #[test]
    fn a_key_between_the_two_presses_cancels_the_discard() {
        let (dir, mut workbench) = repository();
        let path = dir.path().join("a.txt");
        workbench.handle_key(key(KeyCode::Char(' ')));
        fs::write(&path, "ruined\n").expect("a rewritten file");
        workbench.handle_key(key(KeyCode::F(5)));
        workbench.handle_key(key(KeyCode::Down));

        workbench.handle_key(key(KeyCode::Char('x')));
        workbench.handle_key(key(KeyCode::Up));
        workbench.handle_key(key(KeyCode::Down));
        workbench.handle_key(key(KeyCode::Char('x')));

        assert_eq!(
            fs::read_to_string(&path).expect("the file"),
            "ruined\n",
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
            fs::write(dir.path().join(name), "one\n").expect("a file");
        }
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.open(dir.path());
        workbench.handle_key(alt(KeyCode::Char('2')));
        for name in PATHS {
            workbench.scm.stage_path(name).expect("staging");
        }
        commit_all(&mut workbench);
        // Longer than what was committed: a rewrite of the same length within
        // the same second is racily clean, and status would call it unchanged.
        for name in PATHS {
            fs::write(dir.path().join(name), "one\ntwo\n").expect("a file");
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
        let rects = layout_sections(Rect::new(0, 0, 20, 8), [(40, false), (4, false), (4, false)]);

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

        assert!(!workbench.scm.is_collapsed(Section::Unstaged), "{WRONG_ROW}");
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
                .diff_kinds()
                .is_some(),
            "{WRONG_ROW}"
        );
    }

    #[test]
    fn the_header_button_switches_between_tree_and_flat() {
        let (_dir, mut workbench) = nested_repository();
        paint(&mut workbench, 80, 24);
        let header = workbench.panes.header;
        let column = (header.x..header.right())
            .find(|column| mode_at(*column, header, workbench.head_width()))
            .expect("a tree or flat button");

        workbench.handle_mouse(click(column, header.y));

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
    fn enter_on_a_commit_opens_it_as_a_tab_that_cannot_be_edited() {
        let (_dir, mut workbench) = repository();
        workbench.handle_key(key(keys::STAGE_TOGGLE.code));
        commit_all(&mut workbench);
        workbench.scm.select(Section::Graph, Some(0));

        workbench.handle_key(key(KeyCode::Enter));

        let tab = workbench.editor.active().expect("a commit tab");
        assert!(tab.diff_kinds().is_some(), "{DIFF_EDITABLE}");
        assert!(!tab.is_editable(), "{DIFF_EDITABLE}");
        assert!(tab.title.contains("initial"), "{DIFF_EDITABLE}");
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
        let action = workbench.handle_key(alt(KeyCode::Enter));
        assert_eq!(
            action,
            WorkbenchAction::SendToComposer("@a.txt".to_owned()),
            "{WRONG_REFERENCE}"
        );
    }

    /// Types `text` into whichever field the search pane has the caret in.
    fn type_query(workbench: &mut Workbench, text: &str) {
        for ch in text.chars() {
            workbench.handle_key(key(KeyCode::Char(ch)));
        }
    }

    /// Runs the search and drains the worker until it is done, so the assertions
    /// see the whole result set rather than whatever arrived first.
    fn search_for(workbench: &mut Workbench, text: &str) {
        workbench.handle_key(alt(KeyCode::Char('3')));
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
        assert_eq!(tab.path, dir.path().join("a.txt"), "{NO_TAB}");
        assert_eq!(tab.buffer.cursor().line, 2, "{WRONG_LINE}");
    }

    #[test]
    fn an_include_glob_narrows_what_the_pane_lists() {
        let (_dir, mut workbench) = project();
        workbench.handle_key(alt(KeyCode::Char('3')));
        type_query(&mut workbench, "e");
        workbench.handle_key(alt(KeyCode::Char('i')));
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

        workbench.handle_key(alt(KeyCode::Char('r')));
        assert!(workbench.search.is_stale(), "{STALE_RESULTS}");
    }

    #[test]
    fn a_broken_pattern_reaches_the_status_bar() {
        let (_dir, mut workbench) = project();
        workbench.handle_key(alt(KeyCode::Char('3')));
        workbench.handle_key(alt(KeyCode::Char('r')));
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
            workbench.handle_key(alt(KeyCode::Enter)),
            WorkbenchAction::SendToComposer("@a.txt:L3".to_owned()),
            "{WRONG_REFERENCE}"
        );
    }

    #[test]
    fn a_directory_outside_a_repository_says_so_rather_than_failing() {
        let (_dir, mut workbench) = project();
        workbench.handle_key(alt(KeyCode::Char('2')));
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
        workbench.handle_key(alt(code));
        let moved = i32::from(workbench.sidebar_width) - i32::from(before);
        assert_eq!(moved.signum(), i32::from(direction), "{WRONG_WIDTH}");
    }

    #[test]
    fn the_sidebar_never_resizes_past_its_bounds() {
        let mut workbench = workbench();
        for _ in 0..MAX_SIDEBAR_WIDTH {
            workbench.handle_key(alt(keys::GROW_SIDEBAR.code));
        }
        assert_eq!(workbench.sidebar_width, MAX_SIDEBAR_WIDTH, "{WRONG_WIDTH}");

        for _ in 0..MAX_SIDEBAR_WIDTH {
            workbench.handle_key(alt(keys::SHRINK_SIDEBAR.code));
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
        workbench.handle_key(alt(keys::VIEW_SEARCH.code));
        workbench.handle_key(alt(keys::GROW_SIDEBAR.code));
        let saved = workbench.layout();

        let mut restored = Workbench::new(WorkbenchStyles::default());
        restored.open(dir.path());
        restored.restore(saved.clone());

        assert_eq!(restored.layout(), saved, "{WRONG_LAYOUT}");
        assert_eq!(
            restored.editor.active().expect("a tab").path,
            dir.path().join("a.txt"),
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
                .diff_kinds()
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

        workbench.handle_mouse(click(rows.x + 1, rows.y + 1));

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

        workbench.handle_mouse(click(rows.x + 1, rows.y));

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

    #[test]
    fn a_press_that_selected_nothing_leaves_the_clipboard_alone() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
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
        assert_eq!(workbench.confirm, Some(Choice::Save), "{NOT_ASKED}");
    }

    #[test_case(Choice::Discard, 0 ; "a click on don't save closes the tab")]
    #[test_case(Choice::Cancel, 1 ; "a click on cancel keeps it")]
    fn a_click_answers_the_dialog(answer: Choice, remaining: usize) {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.handle_key(key(KeyCode::Char('x')));
        workbench.handle_key(KeyEvent::new(keys::CLOSE_TAB.code, KeyModifiers::ALT));
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
        workbench.handle_key(KeyEvent::new(keys::CLOSE_TAB.code, KeyModifiers::ALT));
        draw(&mut workbench, 80, 24);
        let rows = workbench.panes.rows;

        workbench.handle_mouse(click(rows.x, rows.y));

        assert_eq!(workbench.confirm, Some(Choice::Save), "{MODAL_LEAKED}");
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
        workbench.handle_key(alt(keys::VIEW_SEARCH.code));
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
        assert_eq!(tab.path, dir.path().join("a.txt"), "{NO_TAB}");
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

    /// The selected row already stands out, so a hover on top of it would be
    /// two highlights arguing over one cell.
    #[test]
    fn the_pointer_leaves_the_selected_row_alone() {
        let (_dir, mut workbench) = project();
        draw(&mut workbench, 80, 24);
        let rows = workbench.panes.rows;
        let selected = (rows.x + 1, rows.y);
        let before = cell_style(&mut workbench, selected);

        workbench.handle_mouse(moved(selected.0, selected.1));

        assert_eq!(
            cell_style(&mut workbench, selected),
            before,
            "{WRONG_HOVER}"
        );
    }
}
