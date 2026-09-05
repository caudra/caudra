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
mod quick_open;
mod scm;
mod search;
mod style;
mod view;

pub use action::WorkbenchAction;
pub use style::WorkbenchStyles;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use serde::{Deserialize, Serialize};

use editor::Editor;
use editor::buffer::Cursor;
use editor::Tab;
use fs::tree::Tree;
use fs::watch::Watch;
use quick_open::QuickOpen;
use scm::Scm;
use search::Search;

const DEFAULT_SIDEBAR_WIDTH: u16 = 30;
const MIN_SIDEBAR_WIDTH: u16 = 16;
const MAX_SIDEBAR_WIDTH: u16 = 80;
const MIN_EDITOR_WIDTH: u16 = 24;
const SEPARATOR_WIDTH: u16 = 1;
const STATUS_HEIGHT: u16 = 1;
const SCROLL_LINES: isize = 3;
const SIDEBAR_STEP: i16 = 2;
const STAGED: &str = "index";
const WORKING: &str = "worktree";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SidebarView {
    #[default]
    Explorer,
    SourceControl,
    Search,
}

impl SidebarView {
    const fn title(self) -> &'static str {
        match self {
            Self::Explorer => "Explorer",
            Self::SourceControl => "Source Control",
            Self::Search => "Search",
        }
    }
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
    /// The buffer's own rows and columns, with the tab bar, the gutter and any
    /// open find bar already taken out.
    text: Rect,
    status: Rect,
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
    /// Set while the pointer is dragging the separator, so a drag that wanders
    /// off it keeps resizing instead of selecting a row.
    resizing: bool,
    /// Cut and copy also leave through [`WorkbenchAction::Copy`], but the host
    /// cannot read the system clipboard back, so paste comes from here.
    clipboard: String,
    goto: Option<String>,
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
            resizing: false,
            clipboard: String::new(),
            goto: None,
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
        self.search.is_running()
    }

    /// Drains whatever the background workers have produced. Reports whether
    /// the screen changed, plus anything worth saying in the status bar.
    pub fn tick(&mut self) -> (bool, Option<String>) {
        let watched = self.absorb_changes();
        let searched = self.search.tick();
        (watched || searched, self.flash.take())
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
        Layout {
            tabs,
            active,
            sidebar: self.sidebar,
            sidebar_width: self.sidebar_width,
            sidebar_collapsed: self.sidebar_collapsed,
            show_hidden: self.show_hidden,
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
            MouseEventKind::Down(MouseButton::Left) => {
                self.resizing = self
                    .panes
                    .separator
                    .is_some_and(|rect| rect.contains(at.into()));
                if !self.resizing {
                    self.click(at);
                }
                return WorkbenchAction::Consumed;
            }
            MouseEventKind::Drag(MouseButton::Left) if self.resizing => {
                let start = self.panes.sidebar.map_or(0, |rect| rect.x);
                self.set_sidebar_width(at.0.saturating_sub(start));
                return WorkbenchAction::Consumed;
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.resizing = false;
                return WorkbenchAction::Consumed;
            }
            _ => return WorkbenchAction::Passthrough,
        };
        if self.panes.sidebar.is_some_and(|rect| rect.contains(at.into())) {
            let rows = self.sidebar_rows();
            match self.sidebar {
                SidebarView::Explorer => self.tree.scroll_by(delta, rows),
                SidebarView::SourceControl => self.scm.scroll_by(delta, rows),
                SidebarView::Search => self.search.scroll_by(delta, rows),
            }
        } else if self.panes.text.contains(at.into()) {
            let rows = self.panes.text.height as usize;
            if let Some(tab) = self.editor.active_mut() {
                tab.scroll_by(delta, rows);
            }
        }
        WorkbenchAction::Consumed
    }

    fn click(&mut self, at: (u16, u16)) {
        let position = at.into();
        if self.panes.tabs.contains(position) {
            self.focus = Focus::Editor;
            if let Some(index) = view::tab_at(&self.editor, at.0, self.panes.tabs.x) {
                self.editor.select(index);
                self.reveal_active();
            }
            return;
        }
        if self.panes.sidebar.is_some_and(|rect| rect.contains(position)) {
            let header = self.panes.sidebar.map_or(0, |rect| rect.y + 1);
            self.focus = Focus::Sidebar;
            if at.1 >= header {
                let offset = (at.1 - header) as usize;
                match self.sidebar {
                    SidebarView::Explorer => {
                        self.tree.select_index(self.tree.scroll() + offset);
                    }
                    SidebarView::SourceControl => {
                        self.scm.select_index(self.scm.scroll() + offset);
                    }
                    SidebarView::Search => {
                        self.search.select_index(self.search.scroll() + offset);
                    }
                }
            }
            return;
        }
        if !self.panes.text.contains(position) {
            return;
        }
        self.focus = Focus::Editor;
        let text = self.panes.text;
        let Some(tab) = self.editor.active_mut() else {
            return;
        };
        let line = (tab.scroll() + (at.1 - text.y) as usize).min(tab.buffer.line_count() - 1);
        let column = tab.h_scroll() + (at.0 - text.x) as usize;
        let col = editor::render::char_index(tab.buffer.line(line), column);
        tab.buffer.set_cursor(Cursor::new(line, col), false);
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
        if keys::TOGGLE_LOG.matches(key) {
            self.scm.toggle_listing();
            return;
        }
        if keys::OPEN_DIFF.matches(key) {
            self.open_diff();
            return;
        }
        let page = self.sidebar_rows().max(1) as isize;
        match key.code {
            KeyCode::Up => self.scm.move_selection(-1),
            KeyCode::Down => self.scm.move_selection(1),
            KeyCode::PageUp => self.scm.move_selection(-page),
            KeyCode::PageDown => self.scm.move_selection(page),
            KeyCode::Home => self.scm.select_first(),
            KeyCode::End => self.scm.select_last(),
            KeyCode::Right | KeyCode::Enter => self.open_diff(),
            _ => {}
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
        if let Err(error) = self.scm.toggle_staged() {
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
        match self.editor.active().is_some_and(editor::Tab::is_dirty) {
            true => self.flash = Some(format!("{} has unsaved changes", self.active_title())),
            false => {
                self.editor.close_active();
                self.reveal_active();
            }
        }
    }

    /// Keeps the tree on whatever the editor is showing, so the sidebar never
    /// points somewhere else after a tab switch.
    fn reveal_active(&mut self) {
        let Some(path) = self.editor.active().map(|tab| tab.path.clone()) else {
            return;
        };
        self.tree.reveal(&path);
    }

    fn save_active(&mut self) {
        let Some(tab) = self.editor.active_mut() else {
            return;
        };
        self.flash = match tab.save() {
            Ok(()) => None,
            Err(error) => Some(error.to_string()),
        };
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
        self.panes
            .sidebar
            .map_or(0, |rect| rect.height.saturating_sub(1) as usize)
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
            sidebar: None,
            separator: None,
            editor: body,
            tabs: Rect::default(),
            palette: Rect::default(),
            text: Rect::default(),
            status,
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
        tabs: Rect::default(),
        palette: Rect::default(),
        text: Rect::default(),
        status,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Cursor, Focus, MAX_SIDEBAR_WIDTH, MIN_EDITOR_WIDTH, MIN_SIDEBAR_WIDTH, SCROLL_LINES,
        SidebarView, Workbench, WorkbenchAction, WorkbenchStyles, keys, layout,
    };
    use crate::fs::tree::GitMark;
    use crate::scm::Listing;
    use crate::search;
    use crate::view::NOT_A_REPOSITORY;
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Cell;
    use ratatui::layout::Rect;
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

    /// Paints one frame and hands back what it says, which is also what fills
    /// in the pane geometry the mouse tests measure against.
    fn draw(workbench: &mut Workbench, width: u16, height: u16) -> String {
        let mut terminal =
            Terminal::new(TestBackend::new(width, height)).expect("a test terminal");
        terminal
            .draw(|frame| workbench.view(frame, frame.area()))
            .expect("a frame");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(Cell::symbol)
            .collect()
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
    fn a_dirty_tab_refuses_to_close_and_says_so() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.handle_key(key(KeyCode::Char('X')));

        workbench.handle_key(KeyEvent::new(keys::CLOSE_TAB.code, KeyModifiers::ALT));

        assert_eq!(workbench.editor.tabs().len(), 1, "a dirty tab must survive");
        assert!(workbench.flash.is_some(), "the refusal must be explained");
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
            workbench.editor.active().expect(NO_TAB).buffer.cursor().line,
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

    #[test]
    fn goto_line_takes_digits_and_moves_the_cursor() {
        let (dir, mut workbench) = project();
        open_file(&dir, &mut workbench);
        workbench.handle_key(KeyEvent::new(keys::GOTO_LINE.code, KeyModifiers::CONTROL));
        workbench.handle_key(key(KeyCode::Char('3')));
        workbench.handle_key(key(KeyCode::Enter));

        assert_eq!(workbench.editor.active().expect(NO_TAB).buffer.cursor().line, 2);
        assert!(workbench.goto.is_none(), "the prompt must close after a jump");
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
        assert_eq!(workbench.editor.active().expect(NO_TAB).buffer.line_count(), 1);

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

        for expected in ["EXPLORER", "sub", "a.txt", "one", "three"] {
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

        assert!(!workbench.palette.is_open(), "choosing must close the palette");
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
        assert!(!workbench.tree.rows().iter().any(|row| row.name == ".secret"));

        workbench.handle_key(KeyEvent::new(
            keys::TOGGLE_HIDDEN.code,
            KeyModifiers::CONTROL,
        ));

        assert!(workbench.tree.rows().iter().any(|row| row.name == ".secret"));
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
    fn space_stages_the_selection_and_stages_it_back() {
        let (_dir, mut workbench) = repository();
        workbench.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(
            selected_change(&workbench),
            Some(("a.txt".to_owned(), true, GitMark::Added)),
            "{CHANGE_MISSING}"
        );

        workbench.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(
            selected_change(&workbench),
            Some(("a.txt".to_owned(), false, GitMark::Untracked)),
            "{CHANGE_MISSING}"
        );
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
    fn the_log_key_switches_what_the_pane_lists() {
        let (_dir, mut workbench) = repository();
        workbench.handle_key(key(KeyCode::Char('l')));
        assert_eq!(workbench.scm.listing(), Listing::Log, "{WRONG_PANE}");

        workbench.handle_key(key(KeyCode::Char('l')));
        assert_eq!(workbench.scm.listing(), Listing::Changes, "{WRONG_PANE}");
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
        assert!(workbench.scm.rows().is_empty(), "{WRONG_PANE}");
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
        workbench.handle_mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            separator.x + 10,
            separator.y,
        ));

        assert_eq!(
            workbench.sidebar_width,
            separator.x + 10,
            "{WRONG_WIDTH}"
        );
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
            workbench.editor.active().expect("a tab").diff_kinds().is_some(),
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
        workbench.handle_mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            separator.x,
            separator.y,
        ));
        assert!(!workbench.resizing, "{WRONG_CLICK}");
    }
}
