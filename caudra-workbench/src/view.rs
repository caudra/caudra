//! Painting the workbench.
//!
//! Kept apart from the state and the keymap because it is the only part that
//! knows about terminal columns, and because the layout it records in
//! [`PaneRects`] is what paging and scrolling read back.

use std::ops::Range;

use caudra_grab::grab_scope;
use caudra_highlight::StyledSegment;
use ratatui::Frame;
use ratatui::buffer::Buffer as Surface;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::editor::{DiffKind, Editor, Tab, VisualRow, render};
use crate::fs::backend::WorkbenchPath;
use crate::fs::tree::{GitMark, Row as TreeRow};
use crate::menu::{Item, Menu};
use crate::scm::diff::DiffRow;
use crate::scm::graph::Rail;
use crate::scm::repo::{Change, Commit, CommitPath};
use crate::scm::tree::{Dir, SEPARATOR};
use crate::scm::{Row as ScmRow, Scm, Section};
use crate::scroll::ScrollHint;
use crate::search::engine::Hit;
use crate::search::{Field as SearchField, Row as SearchRow, Search};
use crate::{
    Ask, Bar, Choice, Focus, SidebarView, Workbench, WorkbenchStyles, chrome, keys, layout,
    layout_sections,
};

const HINT_GAP: &str = "  ";
const EMPTY_EDITOR_HINT: &str = "No file open";
const NO_CHANGES: &str = "No changes";
pub(crate) const NOT_A_REPOSITORY: &str = "Not a git repository";
const SEARCH_PROMPT: &str = "Search  ";
const INCLUDE_PROMPT: &str = "Files   ";
const SEARCHING: &str = "Searching\u{2026}";
const SEARCH_HINT: &str = "Type a query, then Enter";
const TRUNCATED: &str = " (truncated)";
const CASE_TOGGLE: &str = "Aa";
const WORD_TOGGLE: &str = "ab";
const REGEX_TOGGLE: &str = ".*";
pub(crate) const CARET: &str = "\u{2588}";
const ENTER_LABEL: &str = "Enter";
const SUMMARY_GAP: &str = " ";
/// The two-column rail down the left of the graph. A commit on the chain of
/// first parents sits on the trunk, one a merge brought in hangs beside it.
const RAIL_TRUNK: &str = "\u{25cf} ";
const RAIL_MERGE: &str = "\u{25c9} ";
const RAIL_SIDE: &str = "\u{2502}\u{25cb}";
/// The rail under an expanded commit, which keeps drawing the lines still live
/// beside it so its files do not leave a hole in the graph column.
const RAIL_UNDER_TRUNK: &str = "\u{2502} ";
const RAIL_UNDER_SIDE: &str = "\u{2502}\u{2502}";
const TREE_LABEL: &str = "TREE";
const FLAT_LABEL: &str = "FLAT";
const FOLD_LABEL: &str = "FOLD";
const TREE_HINT: &str = "tree";
const FLAT_HINT: &str = "flat";
const COUNT_GAP: &str = " ";
const EMPTY_TREE: &str = "Nothing to show";
const LOADING_TREE: &str = "Loading remote files…";
const DIRTY_MARK: &str = "\u{25cf}";
const AGENT_MARK: &str = "\u{25e6}";
const EXPANDED_MARK: &str = "\u{25be} ";
const COLLAPSED_MARK: &str = "\u{25b8} ";
const LEAF_INDENT: &str = "  ";
const DEPTH_INDENT: usize = 2;
/// One nesting level of the explorer, drawn as a rule rather than as air so a
/// deep row says which folder it belongs to.
const GUIDE: &str = "\u{2502} ";
const GUTTER_GAP: u16 = 1;
/// The columns a diff pane needs before a second gutter of line numbers is
/// worth what it takes away from the code.
const MIN_CODE_COLUMNS: u16 = 60;
const MARK_UNCHANGED: char = ' ';
const MARK_REMOVED: char = '-';
const MARK_ADDED: char = '+';
/// A line the alignment matched whose whitespace moved. Neither side of it is
/// new, so neither `-` nor `+` describes it.
const MARK_REINDENTED: char = '~';
const CONFLICT_NOTICE: &str = "Changed on disk since it was opened";
const FIND_PROMPT: &str = "Find: ";
const GOTO_PROMPT: &str = "Go to line: ";
const NO_MATCHES: &str = "No results";
const TAB_GAP: &str = " ";
const CLOSE_MARK: &str = "\u{d7}";
/// The handle on the context menu, kept at the left of every tree row and
/// every tab so the menu is something to reach for rather than something to
/// know about.
pub(crate) const MENU_MARK: &str = "\u{22ee}";
pub(crate) const MORE_LEFT: &str = "\u{2039}";
pub(crate) const MORE_RIGHT: &str = "\u{203a}";
pub(crate) const STAGE_MARK: &str = "+";
pub(crate) const UNSTAGE_MARK: &str = "-";
pub(crate) const OPEN_MARK: &str = "\u{2197}";
pub(crate) const REVERT_MARK: &str = "\u{21ba}";
const CONTROL_GAP: &str = " ";
/// A control and the column of air either side of it, which is the step both
/// the painted strip and [`control_at`] take. Every column of it answers to the
/// control, so a click that lands beside the glyph still presses the button.
const CONTROL_WIDTH: u16 = (CONTROL_GAP.len() * 2 + 1) as u16;
/// The git letter a change row keeps on its right, and the gap that holds the
/// name off it. The letter carries that gap itself rather than leaning on the
/// air [`chrome::status_line`] leaves, so the strip in front of it begins where
/// [`control_at`] says it does.
const CHANGE_TRAILING: u16 = CONTROL_GAP.len() as u16 + 1;
const STAGE_ONLY: [Control; 1] = [Control::Stage];
const OPEN_AND_STAGE: [Control; 2] = [Control::Open, Control::Stage];
const REVERT_AND_STAGE: [Control; 2] = [Control::Revert, Control::Stage];
const OPEN_REVERT_AND_STAGE: [Control; 3] = [Control::Open, Control::Revert, Control::Stage];
const SCROLLBAR_WIDTH: u16 = 1;
/// Narrower than this and the bar would be all there is left of the pane.
const SCROLLBAR_MIN_WIDTH: u16 = 2;
/// The search buttons in the order [`toggle_row`] paints them, which is also
/// the order [`toggle_at`] measures.
const TOGGLES: [(Toggle, &str); 3] = [
    (Toggle::Case, CASE_TOGGLE),
    (Toggle::Word, WORD_TOGGLE),
    (Toggle::Regex, REGEX_TOGGLE),
];
const PALETTE_PROMPT: &str = "> ";
const PALETTE_WIDTH: u16 = 72;
const PALETTE_ROWS: usize = 10;
/// The query row and the rule under the list.
const PALETTE_CHROME: u16 = 2;
const HORIZONTAL: &str = "\u{2500}";
const MENU_GAP: &str = " ";
/// A column of air either side of the widest label.
const MENU_PADDING: u16 = 2;
/// The rules over and under the items.
const MENU_CHROME: u16 = 2;
const UNSAVED_QUESTION: &str = " has unsaved changes";
const REVERT_QUESTION: &str = "Discard changes to ";
const ONE_FILE: &str = " file?";
const MANY_FILES: &str = " files?";
const DELETE_QUESTION: &str = "Delete ";
const ONE_PATH: &str = "?";
const WITH_MORE: &str = " and the ";
const MORE_PATHS: &str = " paths under it?";
/// A rule, the question, the answers, and a rule under them.
const CONFIRM_ROWS: u16 = 4;
/// One column of air either side of the widest row.
const CONFIRM_PADDING: u16 = 1;
const CONFIRM_HINTS: [(&str, &str); 2] = [(ENTER_LABEL, "choose"), (keys::CLOSE.label, "cancel")];
pub(crate) const MENU_HINTS: [(&str, &str); 2] =
    [(ENTER_LABEL, "take"), (keys::CLOSE.label, "close")];
pub(crate) const NAME_HINTS: [(&str, &str); 2] =
    [(ENTER_LABEL, "confirm"), (keys::CLOSE.label, "cancel")];

/// One of the search view's three buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Toggle {
    Case,
    Word,
    Regex,
}

impl Workbench {
    pub fn view(&mut self, frame: &mut Frame, area: Rect) {
        grab_scope!("workbench_view", area);
        self.panes = layout(area, self.sidebar_width, self.sidebar_collapsed);
        let panes = self.panes;
        let buf = frame.buffer_mut();
        chrome::fill(buf, area, self.styles.background);

        if let Some(sidebar) = panes.sidebar {
            self.render_sidebar(buf, sidebar);
        }
        if let Some(separator) = panes.separator {
            chrome::vertical_rule(buf, separator, self.styles.border);
        }
        self.render_editor(buf, panes.editor);
        self.render_status(buf, panes.status);
        self.render_menu(buf, area);
    }

    /// The context menu, drawn over every pane because it can be asked for in
    /// any of them. The dialog is painted inside the editor and still wins,
    /// since a menu never opens while a question is standing.
    fn render_menu(&mut self, buf: &mut Surface, area: Rect) {
        let Some(menu) = &self.menu else {
            self.panes.menu = Rect::default();
            return;
        };
        grab_scope!("workbench_menu", area);
        let panel = menu_panel(menu, area);
        chrome::fill(buf, panel, self.styles.background);
        let [top, rows, bottom] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(panel);
        for rule in [top, bottom] {
            chrome::render_line(
                buf,
                rule,
                Line::from(Span::styled(
                    HORIZONTAL.repeat(rule.width as usize),
                    self.styles.border,
                )),
            );
        }

        let pointed = self.hovered_row(rows);
        let inner = (rows.width as usize).saturating_sub(MENU_GAP.len());
        for (offset, item) in menu.items().iter().enumerate().take(rows.height as usize) {
            let line = match item {
                Item::Separator => Line::from(Span::styled(
                    HORIZONTAL.repeat(rows.width as usize),
                    self.styles.border,
                )),
                Item::Action(action) => {
                    let chosen = offset == menu.selected_index();
                    let mut style = match chosen {
                        true => self.styles.selected,
                        false => self.styles.text,
                    };
                    if pointed == Some(offset) && !chosen {
                        style = style.patch(self.styles.hover);
                    }
                    chrome::status_line(
                        vec![Span::styled(
                            format!("{MENU_GAP}{}", chrome::fit(action.label(), inner)),
                            style,
                        )],
                        Vec::new(),
                        rows.width,
                        style,
                    )
                }
            };
            chrome::render_line(buf, line_at(rows, offset), line);
        }
        self.panes.menu = rows;
    }

    fn render_sidebar(&mut self, buf: &mut Surface, area: Rect) {
        grab_scope!("workbench_sidebar", area);
        let [header, body] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
        self.panes.header = header;
        let focused = self.focus == Focus::Sidebar;
        let context = self.header_context();
        let pointed = self
            .hovering(header)
            .and_then(|at| header_at(at.0, header.x));
        let switcher = SidebarView::ALL
            .into_iter()
            .flat_map(|view| {
                let active = view == self.sidebar;
                let mut style = match (active, focused) {
                    (true, true) => self.styles.title,
                    (true, false) => self.styles.text,
                    (false, _) => self.styles.dim,
                };
                if pointed == Some(view) && !active {
                    style = style.patch(self.styles.hover);
                }
                [
                    Span::styled(TAB_GAP, self.styles.background),
                    Span::styled(view.title(), style),
                ]
            })
            .collect();
        let mut right = Vec::new();
        if let Some(label) = self.header_button() {
            let pointed = self
                .hovering(header)
                .is_some_and(|at| button_at(at.0, header, context.width(), label));
            right.push(Span::styled(
                label,
                emphasized(self.styles.dim, pointed, &self.styles),
            ));
            right.push(Span::styled(TAB_GAP, self.styles.background));
        }
        right.push(Span::styled(context, self.styles.dim));
        chrome::render_line(
            buf,
            header,
            chrome::status_line(switcher, right, header.width, self.styles.dim),
        );

        match self.sidebar {
            SidebarView::Explorer => self.render_tree(buf, body),
            SidebarView::SourceControl => self.render_scm(buf, body),
            SidebarView::Search => self.render_search(buf, body, focused),
        }
    }

    /// What the sidebar header prints on its right: the branch for source
    /// control, the project's own name for the rest. The header's button is
    /// measured against it, so the hit test asks for it too.
    pub(crate) fn header_context(&self) -> String {
        match self.sidebar {
            SidebarView::SourceControl => self.scm.head().unwrap_or_default().to_owned(),
            _ => self
                .root
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
        }
    }

    /// The one button the header offers for the view that is up. The explorer
    /// folds its tree back, source control swaps how it lists paths, and
    /// search has nothing to put there.
    pub(crate) fn header_button(&self) -> Option<&'static str> {
        match self.sidebar {
            SidebarView::Explorer => Some(FOLD_LABEL),
            SidebarView::SourceControl => Some(match self.scm.is_flat() {
                true => FLAT_LABEL,
                false => TREE_LABEL,
            }),
            SidebarView::Search => None,
        }
    }

    /// Two fields and a toggle row over the results, so the whole question and
    /// its answer stay visible in one column.
    fn render_search(&mut self, buf: &mut Surface, area: Rect, focused: bool) {
        grab_scope!("workbench_search", area);
        let [query, include, toggles, body] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .areas(area);

        let caret = focused && self.focus == Focus::Sidebar;
        let search = self.search.query();
        for (rect, label, text, field) in [
            (query, SEARCH_PROMPT, &search.text, SearchField::Query),
            (
                include,
                INCLUDE_PROMPT,
                &search.include,
                SearchField::Include,
            ),
        ] {
            chrome::render_line(
                buf,
                rect,
                field_row(
                    label,
                    text,
                    caret && self.search.field() == field,
                    &self.styles,
                    rect.width,
                ),
            );
        }
        self.panes.toggles = toggles;
        let pointed = self
            .hovering(toggles)
            .and_then(|at| toggle_at(at.0, toggles.x));
        chrome::render_line(
            buf,
            toggles,
            toggle_row(&self.search, pointed, &self.styles, toggles.width),
        );

        let height = body.height as usize;
        // A bar drag is the user moving the window on purpose, so the cursor
        // stays where it is rather than dragging the window back to itself.
        if !self.bars.sidebar.is_dragging() {
            self.search.clamp_scroll(height);
        }
        if !self.search.has_results() {
            self.panes.rows = body;
            let notice = match (self.search.error(), self.search.is_running()) {
                (Some(error), _) => error,
                (None, true) => SEARCHING,
                (None, false) if self.search.is_stale() => SEARCH_HINT,
                (None, false) => NO_MATCHES,
            };
            placeholder(buf, body, notice, self.styles.dim);
            return;
        }

        let total = self.search.rows().len();
        let (rows, bar) = scroll_column(self.scrollbars, body, total);
        self.panes.rows = rows;
        let scroll = self.search.scroll();
        let selected = self.search.selected_index();
        let root = self.backend_root();
        let pointed = self.hovered_row(rows);
        for (offset, row) in self
            .search
            .rows()
            .iter()
            .skip(scroll)
            .take(height)
            .enumerate()
        {
            let chosen = focused && scroll + offset == selected;
            let line = search_row(&self.search, *row, &root, chosen, &self.styles, rows.width);
            let line = emphasize(line, pointed == Some(offset) && !chosen, &self.styles);
            chrome::render_line(buf, line_at(rows, offset), line);
        }
        self.scrollbar(buf, Bar::Sidebar, bar, total, scroll);
    }

    /// Three stacked sections, each with a pinned title row over a list that
    /// scrolls under it. The title stays put so the fold handle and the count
    /// never scroll out of reach.
    fn render_scm(&mut self, buf: &mut Surface, area: Rect) {
        grab_scope!("workbench_source_control", area);
        if !self.scm.is_repository() || self.scm.error().is_some() {
            let notice = self.scm.error().unwrap_or(NOT_A_REPOSITORY);
            placeholder(buf, area, notice, self.styles.dim);
            return;
        }
        let wanted = Section::ALL.map(|section| {
            (
                self.scm.height(section),
                self.scm.is_collapsed(section) || self.scm.count(section) == 0,
            )
        });
        self.panes.sections = layout_sections(area, wanted);

        let cursor = self.scm.cursor();
        for (index, section) in Section::ALL.into_iter().enumerate() {
            let rects = self.panes.sections[index];
            let chosen = cursor.section == section && cursor.row.is_none();
            let pointer = self.hovering(rects.header);
            let line = section_header(
                section,
                self.scm.count(section),
                self.scm.is_collapsed(section),
                chosen,
                self.scm_strip(pointer, rects.header, section, None),
                &self.styles,
                rects.header.width,
            );
            let line = emphasize(line, pointer.is_some() && !chosen, &self.styles);
            chrome::render_line(buf, rects.header, line);
            self.panes.sections[index].body = self.render_section(buf, section, rects.body);
        }
        if self.scm.is_empty() {
            let notice = Rect {
                y: self.panes.sections[Section::COUNT - 1].header.bottom(),
                height: area
                    .bottom()
                    .saturating_sub(self.panes.sections[Section::COUNT - 1].header.bottom()),
                ..area
            };
            placeholder(buf, notice, NO_CHANGES, self.styles.dim);
        }
    }

    /// Reports the rows it drew into, which is the section's body less
    /// whatever its scrollbar took.
    fn render_section(&mut self, buf: &mut Surface, section: Section, area: Rect) -> Rect {
        grab_scope!("workbench_source_control_section", area);
        let height = area.height as usize;
        if !self.bars.sections[section.index()].is_dragging() {
            self.scm.clamp_scroll(section, height);
        }
        if height == 0 {
            return area;
        }
        let total = self.scm.rows(section).len();
        let (rows, bar) = scroll_column(self.scrollbars, area, total);
        let scroll = self.scm.scroll(section);
        let cursor = self.scm.cursor();
        let pointer = self.hovering(rows);
        for offset in 0..height.min(total.saturating_sub(scroll)) {
            let row = self.scm.rows(section)[scroll + offset];
            let chosen = cursor.section == section && cursor.row == Some(scroll + offset);
            let on_row = pointer.filter(|at| (at.1 - rows.y) as usize == offset);
            let line = scm_row(
                &self.scm,
                section,
                row,
                chosen,
                self.scm_strip(on_row, rows, section, Some(row)),
                &self.styles,
                rows.width,
            );
            let line = emphasize(line, on_row.is_some() && !chosen, &self.styles);
            chrome::render_line(buf, line_at(rows, offset), line);
        }
        self.scrollbar(buf, Bar::Section(section), bar, total, scroll);
        rows
    }

    /// What a row shows on its right: nothing until the pointer rests on it,
    /// and then the controls it offers with whichever one the pointer is
    /// actually over. Measured with [`control_at`], so the button that lights
    /// up is the button a click presses.
    fn scm_strip(
        &self,
        at: Option<(u16, u16)>,
        rect: Rect,
        section: Section,
        row: Option<ScmRow>,
    ) -> Strip {
        let Some(at) = at else {
            return Strip::default();
        };
        let controls = scm_controls(section, row);
        let trailing = match row {
            Some(row) => row_trailing(row),
            None => header_trailing(self.scm.count(section)),
        };
        Strip {
            controls,
            pointed: control_at(at.0, rect, trailing, controls),
        }
    }

    fn render_tree(&mut self, buf: &mut Surface, area: Rect) {
        grab_scope!("workbench_explorer", area);
        let height = area.height as usize;
        if !self.bars.sidebar.is_dragging() {
            self.tree.clamp_scroll(height);
        }
        if self.tree.rows().is_empty() {
            self.panes.rows = area;
            let notice = if self.remote_backend.is_some() && !self.remote_pending.is_empty() {
                LOADING_TREE
            } else {
                EMPTY_TREE
            };
            placeholder(buf, area, notice, self.styles.dim);
            return;
        }
        let (rows, bar) = scroll_column(self.scrollbars, area, self.tree.rows().len());
        self.panes.rows = rows;
        let scroll = self.tree.scroll();
        let selected = self.tree.selected_index();
        let pointed = self.hovered_row(rows);
        let on_mark = self
            .hovering(rows)
            .is_some_and(|at| on_menu_mark(at.0, rows.x));
        for (offset, row) in self
            .tree
            .rows()
            .iter()
            .skip(scroll)
            .take(height)
            .enumerate()
        {
            // Not gated on the focus. The row is what the editor is showing as
            // much as it is where the arrow keys are, and a pane that drops the
            // mark the moment anything else is focused cannot answer the only
            // question it is ever asked from the editor: where am I? Which pane
            // has the focus is already on the header, which prints the view it
            // is showing in bold only while it holds it.
            let chosen = scroll + offset == selected;
            let marked = on_mark && pointed == Some(offset);
            let line = tree_row(row, chosen, marked, &self.styles, rows.width);
            let line = emphasize(line, pointed == Some(offset) && !chosen, &self.styles);
            chrome::render_line(buf, line_at(rows, offset), line);
        }
        self.scrollbar(buf, Bar::Sidebar, bar, self.tree.rows().len(), scroll);
    }

    fn render_editor(&mut self, buf: &mut Surface, area: Rect) {
        grab_scope!("workbench_editor", area);
        let prompt = self.prompt();
        let [tabs, body, bar] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(u16::from(prompt.is_some())),
        ])
        .areas(area);

        self.panes.tabs = tabs;
        self.render_tabs(buf, tabs);
        self.render_body(buf, body);
        if let Some((label, input, caret)) = prompt {
            self.render_prompt(buf, bar, &label, &input, caret);
        }
        self.render_palette(buf, area);
        self.render_confirm(buf, area);
    }

    /// Drawn over the editor rather than beside it, because it is a question
    /// asked of the whole project and answered by replacing what is on screen.
    fn render_palette(&mut self, buf: &mut Surface, area: Rect) {
        if !self.palette.is_open() {
            self.panes.palette = Rect::default();
            return;
        }
        grab_scope!("workbench_palette", area);
        let width = area.width.min(PALETTE_WIDTH);
        let listed = u16::try_from(self.palette.len().min(PALETTE_ROWS)).unwrap_or(u16::MAX);
        let height = (listed + PALETTE_CHROME).min(area.height);
        let panel = Rect {
            width,
            height,
            ..area
        };
        chrome::fill(buf, panel, self.styles.background);

        let [query, rows, rule] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(panel);
        if !self.bars.palette.is_dragging() {
            self.palette.clamp_scroll(rows.height as usize);
        }
        let (rows, bar) = scroll_column(self.scrollbars, rows, self.palette.len());
        self.panes.palette = rows;

        chrome::render_line(
            buf,
            query,
            Line::from(vec![
                Span::styled(PALETTE_PROMPT, self.styles.accent),
                Span::styled(self.palette.query().to_owned(), self.styles.text),
            ]),
        );
        let scroll = self.palette.scroll();
        let selected = self.palette.selected_index();
        let pointed = self.hovered_row(rows);
        for (offset, path) in self
            .palette
            .rows()
            .skip(scroll)
            .take(rows.height as usize)
            .enumerate()
        {
            let chosen = scroll + offset == selected;
            let mut style = match chosen {
                true => self.styles.selected,
                false => self.styles.dim,
            };
            if pointed == Some(offset) && !chosen {
                style = style.patch(self.styles.hover);
            }
            chrome::render_line(
                buf,
                line_at(rows, offset),
                Line::from(Span::styled(
                    chrome::fit_end(path, rows.width as usize),
                    style,
                )),
            );
        }
        self.scrollbar(buf, Bar::Palette, bar, self.palette.len(), scroll);
        chrome::render_line(
            buf,
            rule,
            Line::from(Span::styled(
                HORIZONTAL.repeat(rule.width as usize),
                self.styles.border,
            )),
        );
    }

    /// The unsaved-changes dialog, drawn over the middle of the editor because
    /// it is a question about the buffer underneath it and nothing else may be
    /// answered until it is.
    fn render_confirm(&mut self, buf: &mut Surface, area: Rect) {
        let Some(confirm) = self.confirm else {
            self.panes.confirm = Rect::default();
            return;
        };
        grab_scope!("workbench_confirm", area);
        let question = self.question(confirm.ask);
        let content = question.width().max(answers_width(confirm.ask)) as u16;
        let width = (content + CONFIRM_PADDING * 2).min(area.width);
        let height = CONFIRM_ROWS.min(area.height);
        let panel = Rect {
            x: area.x + (area.width - width) / 2,
            y: area.y + (area.height - height) / 2,
            width,
            height,
        };
        chrome::fill(buf, panel, self.styles.background);

        let [top, prompt, answers, bottom] =
            Layout::vertical([Constraint::Length(1); CONFIRM_ROWS as usize]).areas(panel);
        for rule in [top, bottom] {
            chrome::render_line(
                buf,
                rule,
                Line::from(Span::styled(
                    HORIZONTAL.repeat(rule.width as usize),
                    self.styles.border,
                )),
            );
        }

        let inner = Rect {
            x: panel.x + CONFIRM_PADDING,
            width: panel.width.saturating_sub(CONFIRM_PADDING * 2),
            ..prompt
        };
        chrome::render_line(
            buf,
            inner,
            Line::from(Span::styled(
                chrome::fit(&question, inner.width as usize),
                self.styles.text,
            )),
        );

        self.panes.confirm = Rect {
            y: answers.y,
            ..inner
        };
        let pointed = self
            .hovering(self.panes.confirm)
            .and_then(|at| confirm_at(at.0, self.panes.confirm.x, confirm.ask));
        let mut spans = Vec::new();
        for answer in confirm.ask.answers() {
            let chosen = *answer == confirm.choice;
            let mut style = match chosen {
                true => self.styles.selected,
                false => self.styles.dim,
            };
            if pointed == Some(*answer) && !chosen {
                style = style.patch(self.styles.hover);
            }
            spans.push(Span::styled(TAB_GAP, self.styles.background));
            spans.push(Span::styled(answer.label(confirm.ask), style));
        }
        chrome::render_line(buf, self.panes.confirm, Line::from(spans));
    }

    /// What the dialog is asking, which is the one place either question is
    /// spelled out.
    fn question(&self, ask: Ask) -> String {
        match ask {
            Ask::Close => format!("{}{UNSAVED_QUESTION}", self.active_title()),
            Ask::Revert => {
                let covered = self.scm.scope_len();
                let noun = match covered {
                    1 => ONE_FILE,
                    _ => MANY_FILES,
                };
                format!("{REVERT_QUESTION}{covered}{noun}")
            }
            Ask::Delete(under) => self.delete_question(under),
        }
    }

    /// Names what goes, relative to the project so a deep path is still one
    /// line, and counts what goes with it when it is a folder.
    fn delete_question(&self, under: usize) -> String {
        let path = self
            .delete_target
            .as_ref()
            .map(|entry| entry.path.clone())
            .or_else(|| self.tree.selected().map(|row| row.path.clone()));
        let Some(path) = path else {
            return String::new();
        };
        let named = path.display_relative(&self.backend_root());
        match under {
            0 => format!("{DELETE_QUESTION}{named}{ONE_PATH}"),
            _ => format!("{DELETE_QUESTION}{named}{WITH_MORE}{under}{MORE_PATHS}"),
        }
    }

    fn render_tabs(&mut self, buf: &mut Surface, area: Rect) {
        grab_scope!("workbench_tabs", area);
        let active = self.editor.active_index();
        let shown = visible_range(&self.editor, area.width);
        let pointed = self
            .hovering(area)
            .and_then(|at| tab_at(&self.editor, at.0, area));
        let mut spans = Vec::new();
        for index in shown.clone() {
            let tab = &self.editor.tabs()[index];
            let under = pointed.filter(|hit| hit.index == index);
            let mut style = if index == active {
                self.styles.tab_active
            } else {
                self.styles.tab_inactive
            };
            if tab.preview {
                style = style.add_modifier(Modifier::ITALIC);
            }
            if under.is_some() && index != active {
                style = style.patch(self.styles.hover);
            }
            let close = self.mark_style(under, TabPart::Close);
            spans.push(Span::styled(TAB_GAP, self.styles.background));
            spans.push(Span::styled(
                MENU_MARK,
                self.mark_style(under, TabPart::Menu),
            ));
            spans.push(Span::styled(TAB_GAP, self.styles.background));
            if tab.is_dirty() {
                spans.push(Span::styled(DIRTY_MARK, self.styles.accent));
            }
            spans.push(Span::styled(format!("{}{TAB_GAP}", tab.title), style));
            spans.push(Span::styled(CLOSE_MARK, close));
            spans.push(Span::styled(TAB_GAP, self.styles.background));
        }
        chrome::render_line(buf, area, Line::from(spans));

        // Painted over the gap either end of the strip, which the tabs leave
        // blank, so saying there is more costs no title.
        if shown.start > 0 {
            overwrite(buf, (area.x, area.y), MORE_LEFT, self.styles.dim);
        }
        if shown.end < self.editor.tabs().len() {
            let last = area.right().saturating_sub(1);
            overwrite(buf, (last, area.y), MORE_RIGHT, self.styles.dim);
        }
    }

    /// How one of a tab's marks is painted: lit while the pointer is on it,
    /// and drawn back the rest of the time so the title stays what the tab
    /// says.
    fn mark_style(&self, under: Option<TabHit>, part: TabPart) -> Style {
        match under.is_some_and(|hit| hit.part == part) {
            true => self.styles.accent.patch(self.styles.hover),
            false => self.styles.dim,
        }
    }

    /// Records the strip the bar owns and paints it. A pane whose content fits
    /// hands over no rect, which clears the slot and with it any drag that was
    /// in flight when the content shrank.
    fn scrollbar(
        &mut self,
        buf: &mut Surface,
        bar: Bar,
        area: Option<Rect>,
        total: usize,
        at: usize,
    ) {
        let style = self.styles.border;
        let slot = self.bars.slot(bar);
        match area {
            Some(area) => {
                slot.place(area, total as u32, at as u32);
                slot.render(buf, style);
            }
            None => slot.place(Rect::ZERO, 0, 0),
        }
    }

    fn render_body(&mut self, buf: &mut Surface, area: Rect) {
        grab_scope!("workbench_editor_body", area);
        let focused = self.focus == Focus::Editor && self.goto.is_none() && !self.palette.is_open();
        let Some(tab) = self.editor.active_mut() else {
            self.panes.text = Rect::default();
            placeholder(buf, area, EMPTY_EDITOR_HINT, self.styles.dim);
            return;
        };
        if let Some(notice) = tab.notice() {
            self.panes.text = Rect::default();
            placeholder(buf, area, notice.reason(), self.styles.dim);
            return;
        }

        let lines = tab.buffer.line_count();
        let columns = tab
            .diff_rows()
            .map(|rows| DiffColumns::for_pane(rows, area.width));
        let gutter = columns.map_or_else(|| digits(lines) + GUTTER_GAP, DiffColumns::width);
        let [numbers, text] =
            Layout::horizontal([Constraint::Length(gutter), Constraint::Min(1)]).areas(area);
        // Taken off the text rather than the gutter, and taken from the rect
        // the cursor is placed against too, so a caret at the right margin
        // cannot end up underneath the bar.
        let (text, bar) = scroll_column(self.scrollbars, text, lines);
        self.panes.text = text;

        // Wrapping makes a row a slice of a line rather than a whole one, but
        // the highlighter and the scrollbar still count in buffer lines, so
        // both ends of the window are taken back to the lines they fall on.
        let rows = tab.visible_rows(text.height as usize, text.width as usize, self.wrap);
        let first = rows.first().map_or(0, |row| row.line);
        let last = rows.last().map_or(0, |row| row.line + 1);
        tab.highlight(first, last);
        let tab = &*tab;
        for (offset, row) in rows.iter().enumerate() {
            chrome::render_line(
                buf,
                line_at(numbers, offset),
                gutter_row(tab, *row, &self.styles, focused, gutter, columns),
            );
            chrome::render_line(
                buf,
                line_at(text, offset),
                text_row(
                    tab,
                    row.line,
                    tab.colours(row.line, first, last),
                    &self.styles,
                    focused,
                    (row.start, row.span),
                ),
            );
        }
        self.bars
            .text
            .set_hint(ScrollHint::lines(first as u32 + 1, lines as u32));
        self.scrollbar(buf, Bar::Text, bar, lines, first);
    }

    fn render_prompt(&self, buf: &mut Surface, area: Rect, label: &str, input: &str, caret: bool) {
        grab_scope!("workbench_prompt", area);
        let mut left = vec![Span::styled(label.to_owned(), self.styles.accent)];
        if !input.is_empty() {
            left.push(Span::styled(input.to_owned(), self.styles.text));
        }
        if caret {
            left.push(Span::styled(CARET, self.styles.cursor));
        }
        let right = match self.editor.active().map(|tab| &tab.find) {
            Some(find) if find.is_open() && !find.query().is_empty() => match find.position() {
                Some((at, total)) => vec![Span::styled(format!("{at}/{total}"), self.styles.dim)],
                None => vec![Span::styled(NO_MATCHES, self.styles.error)],
            },
            _ => Vec::new(),
        };
        chrome::render_line(
            buf,
            area,
            chrome::status_line(left, right, area.width, self.styles.dim),
        );
    }

    fn render_status(&mut self, buf: &mut Surface, area: Rect) {
        grab_scope!("workbench_status", area);
        let half = area.width as usize / 2;
        let left = match (&self.flash, self.editor.active()) {
            (Some(message), _) => vec![Span::styled(chrome::fit(message, half), self.styles.error)],
            (None, _)
                if self.sidebar == SidebarView::SourceControl
                    && self.scm.repository_state().is_some() =>
            {
                vec![Span::styled(
                    chrome::fit(&self.scm.repository_state().unwrap_or_default(), half),
                    self.styles.dim,
                )]
            }
            (None, Some(tab)) => {
                status_left(tab, &self.relative_path(&tab.path), &self.styles, half)
            }
            (None, None) => vec![Span::styled(
                chrome::fit_end(&self.backend_root().display(), half),
                self.styles.dim,
            )],
        };

        let mut right = match self.editor.active() {
            Some(tab) => {
                let cursor = tab.buffer.cursor();
                vec![Span::styled(
                    format!(
                        "Ln {}, Col {}  {}",
                        cursor.line + 1,
                        cursor.col + 1,
                        tab.line_ending().label()
                    ),
                    self.styles.dim,
                )]
            }
            None => Vec::new(),
        };
        right.extend(hints(&self.status_hints(), &self.styles));
        chrome::render_line(
            buf,
            area,
            chrome::status_line(left, right, area.width, self.styles.dim),
        );
    }

    /// What the status bar offers, which is whatever the focused pane can do.
    /// Anything standing over the panes answers first, since that is what the
    /// next key will reach.
    fn status_hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.input.is_some() {
            return NAME_HINTS.to_vec();
        }
        if self.confirm.is_some() {
            return CONFIRM_HINTS.to_vec();
        }
        if self.menu.is_some() {
            return MENU_HINTS.to_vec();
        }
        if self.focus == Focus::Sidebar && self.sidebar == SidebarView::SourceControl {
            let other = match self.scm.is_flat() {
                true => TREE_HINT,
                false => FLAT_HINT,
            };
            return vec![
                (keys::STAGE_TOGGLE.label, "stage"),
                (keys::OPEN_DIFF.label, "diff"),
                (keys::DISCARD.label, "discard"),
                (keys::TOGGLE_TREE.label, other),
                (keys::CLOSE.label, "back"),
            ];
        }
        if self.focus == Focus::Sidebar && self.sidebar == SidebarView::Search {
            let enter = if self.search.is_stale() {
                "search"
            } else {
                "open"
            };
            return vec![
                (ENTER_LABEL, enter),
                (keys::NEXT_FIELD.label, "files"),
                (keys::TOGGLE_CASE.label, "case"),
                (keys::TOGGLE_WORD.label, "word"),
                (keys::TOGGLE_REGEX.label, "regex"),
            ];
        }
        if self.focus == Focus::Editor {
            return vec![
                (keys::SAVE.label, "save"),
                (keys::FIND.label, "find"),
                (keys::SEND_TO_COMPOSER.label, "send"),
                (keys::CLOSE.label, "back"),
            ];
        }
        vec![
            (keys::VIEW_EXPLORER.label, "explorer"),
            (keys::FIND.label, "find"),
            (keys::SEND_TO_COMPOSER.label, "send"),
            (keys::CLOSE.label, "back"),
        ]
    }

    /// The one-line field under the editor, when something is asking for
    /// input, and whether it carries a caret. A name is the only one of these
    /// with no other sign on screen that it is being typed into: find marks its
    /// matches and go-to-line takes digits alone.
    fn prompt(&self) -> Option<(String, String, bool)> {
        if let Some(input) = &self.input {
            return Some((input.kind.label().to_owned(), input.value.clone(), true));
        }
        if let Some(input) = &self.goto {
            return Some((GOTO_PROMPT.to_owned(), input.clone(), false));
        }
        let find = &self.editor.active()?.find;
        find.is_open()
            .then(|| (FIND_PROMPT.to_owned(), find.query().to_owned(), false))
    }
}

/// Where a click on the tab strip landed. A mark is its own target, so
/// reaching for one never selects the tab instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TabHit {
    pub(crate) index: usize,
    pub(crate) part: TabPart,
}

/// Which of a tab's three targets a column falls on. One answer rather than a
/// flag each, so a hit cannot claim to be two of them at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TabPart {
    Body,
    Menu,
    Close,
}

/// One tab's width, which is the only description of how
/// [`Workbench::render_tabs`] lays a tab out.
fn tab_width(tab: &Tab) -> usize {
    TAB_GAP.len() * 4
        + MENU_MARK.chars().count()
        + usize::from(tab.is_dirty())
        + tab.title.chars().count()
        + CLOSE_MARK.chars().count()
}

/// The tabs the strip has room for, as many as fit ending at the active one.
/// Derived on every paint rather than stored, so the strip cannot remember a
/// position the tabs have since moved out from under.
pub(crate) fn visible_range(editor: &Editor, width: u16) -> Range<usize> {
    let Some(tab) = editor.active() else {
        return 0..0;
    };
    let tabs = editor.tabs();
    let active = editor.active_index();
    let width = width as usize;
    let mut room = width.saturating_sub(tab_width(tab));

    let mut first = active;
    while first > 0 && tab_width(&tabs[first - 1]) <= room {
        first -= 1;
        room -= tab_width(&tabs[first]);
    }
    let mut end = active + 1;
    while end < tabs.len() && tab_width(&tabs[end]) <= room {
        room -= tab_width(&tabs[end]);
        end += 1;
    }
    first..end
}

/// Which tab a click at `column` landed on, and whether it landed on that
/// tab's close mark. Measured over the tabs the strip is showing, so a click
/// answers to what is under it rather than to the tab that would have been
/// there had the strip never scrolled.
pub(crate) fn tab_at(editor: &Editor, column: u16, strip: Rect) -> Option<TabHit> {
    let shown = visible_range(editor, strip.width);
    let mut left = column.checked_sub(strip.x)? as usize;
    for index in shown {
        let width = tab_width(&editor.tabs()[index]);
        if left < width {
            // The leading gap goes to the mark. It would otherwise select the
            // tab, which the title already does, so it widens the smaller
            // target for nothing.
            let menu = TAB_GAP.len() + MENU_MARK.chars().count();
            let part = match left {
                _ if left < menu => TabPart::Menu,
                _ if left == width - TAB_GAP.len() - CLOSE_MARK.chars().count() => TabPart::Close,
                _ => TabPart::Body,
            };
            return Some(TabHit { index, part });
        }
        left -= width;
    }
    None
}

/// Which view the switcher segment at `column` selects, measured the same way
/// [`Workbench::render_sidebar`] lays them out. The gaps between them are not
/// buttons.
pub(crate) fn header_at(column: u16, origin: u16) -> Option<SidebarView> {
    let mut left = column.checked_sub(origin)? as usize;
    for view in SidebarView::ALL {
        let width = TAB_GAP.len() + view.title().len();
        if left < width {
            return (left >= TAB_GAP.len()).then_some(view);
        }
        left -= width;
    }
    None
}

/// One answer's width, which is the only description of how
/// [`Workbench::render_confirm`] lays the row out.
fn answer_width(answer: Choice, ask: Ask) -> usize {
    TAB_GAP.len() + answer.label(ask).width()
}

fn answers_width(ask: Ask) -> usize {
    ask.answers()
        .iter()
        .map(|answer| answer_width(*answer, ask))
        .sum()
}

/// Which answer a click at `column` landed on, measured the same way
/// [`Workbench::render_confirm`] lays them out. The gaps between them are not
/// buttons.
pub(crate) fn confirm_at(column: u16, origin: u16, ask: Ask) -> Option<Choice> {
    let mut left = column.checked_sub(origin)? as usize;
    for answer in ask.answers() {
        let width = answer_width(*answer, ask);
        if left < width {
            return (left >= TAB_GAP.len()).then_some(*answer);
        }
        left -= width;
    }
    None
}

/// A control a source control row offers while the pointer is resting on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Control {
    /// Opens the working tree file rather than the diff.
    Open,
    /// Stages or unstages, which is one action read from where the row sits.
    Stage,
    /// Throws the working tree's edits away, which only an unstaged change has
    /// to throw.
    Revert,
}

/// The controls a row offers, left to right, and the only description of them.
/// `None` asks about the section header. A section that cannot be staged and a
/// commit offer nothing, so nothing can be clicked on them either. Only a file
/// has a working tree copy to open, and only an unstaged row has working tree
/// edits to throw away, at whatever width the row covers.
pub(crate) fn scm_controls(section: Section, row: Option<ScmRow>) -> &'static [Control] {
    if !section.is_changes() {
        return &[];
    }
    match row {
        Some(ScmRow::Commit(_)) => &[],
        Some(ScmRow::Change { .. }) if section == Section::Staged => &OPEN_AND_STAGE,
        Some(ScmRow::Change { .. }) => &OPEN_REVERT_AND_STAGE,
        _ if section == Section::Unstaged => &REVERT_AND_STAGE,
        _ => &STAGE_ONLY,
    }
}

/// Which control a click at `column` landed on, measured over the same strip
/// [`scm_controls`] describes: right aligned in `rect`, left of the `trailing`
/// columns the row already keeps for its mark or its count.
pub(crate) fn control_at(
    column: u16,
    rect: Rect,
    trailing: u16,
    controls: &[Control],
) -> Option<Control> {
    let width = CONTROL_WIDTH * controls.len() as u16;
    let origin = rect.right().checked_sub(trailing + width)?;
    let reached = column.checked_sub(origin)? / CONTROL_WIDTH;
    controls.get(reached as usize).copied()
}

/// What a row is showing on its right and which of it the pointer is on. The
/// two travel together because the strip is only painted while the row is
/// hovered, so the answer to one is always wanted with the other.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Strip {
    controls: &'static [Control],
    pointed: Option<Control>,
}

/// The count a section header keeps on its right, which the controls sit left
/// of. Both the painter and [`control_at`]'s caller read it here, so the two
/// cannot disagree about where the strip ends.
fn header_count(count: usize) -> String {
    format!("{count}{COUNT_GAP}")
}

pub(crate) fn header_trailing(count: usize) -> u16 {
    header_count(count).width() as u16
}

/// The columns a body row keeps on its right before the strip begins. Only a
/// change carries a git letter, so a folder's controls reach the margin.
pub(crate) fn row_trailing(row: ScmRow) -> u16 {
    match row {
        ScmRow::Change { .. } | ScmRow::CommitFile { .. } => CHANGE_TRAILING,
        _ => 0,
    }
}

/// The painted strip, in the order [`control_at`] measures it. The one under
/// the pointer is painted in the accent so the row says which button a click
/// would press, not merely that it has buttons.
fn control_spans(staged: bool, strip: Strip, styles: &WorkbenchStyles) -> Vec<Span<'static>> {
    strip
        .controls
        .iter()
        .map(|control| {
            let mark = match control {
                Control::Open => OPEN_MARK,
                Control::Revert => REVERT_MARK,
                Control::Stage if staged => UNSTAGE_MARK,
                Control::Stage => STAGE_MARK,
            };
            let style = match strip.pointed == Some(*control) {
                true => styles.accent,
                false => styles.dim,
            };
            Span::styled(format!("{CONTROL_GAP}{mark}{CONTROL_GAP}"), style)
        })
        .collect()
}

/// The columns a strip takes, which is what the row has to keep clear of its
/// own text.
fn control_reserve(strip: Strip) -> usize {
    CONTROL_WIDTH as usize * strip.controls.len()
}

/// Which search button a click at `column` landed on, measured the same way
/// [`toggle_row`] lays them out.
pub(crate) fn toggle_at(column: u16, origin: u16) -> Option<Toggle> {
    let mut left = column.checked_sub(origin)? as usize;
    for (toggle, label) in TOGGLES {
        let width = TAB_GAP.len() + label.len();
        if left < width {
            return (left >= TAB_GAP.len()).then_some(toggle);
        }
        left -= width;
    }
    None
}

/// Where the context menu lands: under and to the right of the cell it was
/// asked for, pulled back inside the frame when it would run off the side, and
/// flipped over that cell when there is no room for it underneath.
fn menu_panel(menu: &Menu, area: Rect) -> Rect {
    let width = (menu.width() as u16 + MENU_PADDING).min(area.width);
    let height = (menu.items().len() as u16 + MENU_CHROME).min(area.height);
    let (column, row) = menu.at();
    let below = row.saturating_add(1);
    Rect {
        x: column.clamp(area.x, area.right().saturating_sub(width)),
        y: match below.saturating_add(height) <= area.bottom() {
            true => below,
            false => row.saturating_sub(height).max(area.y),
        },
        width,
        height,
    }
}

/// Whether `column` is on the sidebar header's button, which sits at the right
/// edge ahead of `context`. Measured from the right the same way
/// [`chrome::status_line`] lays that group out.
pub(crate) fn button_at(column: u16, header: Rect, context: usize, label: &str) -> bool {
    let width = label.width() as u16;
    let trailing = context as u16 + TAB_GAP.len() as u16 + width;
    let Some(start) = header.right().checked_sub(trailing) else {
        return false;
    };
    (start..start + width).contains(&column)
}

/// The hover highlight as a style rather than a whole line, for the header
/// segments that share a row with things that are not buttons.
fn emphasized(base: Style, hovered: bool, styles: &WorkbenchStyles) -> Style {
    match hovered {
        true => base.patch(styles.hover),
        false => base,
    }
}

/// Paints the pointer's own highlight over a row it is resting on, keeping the
/// colours the row already earned rather than replacing them.
fn emphasize(line: Line<'static>, hovered: bool, styles: &WorkbenchStyles) -> Line<'static> {
    if !hovered {
        return line;
    }
    let base = line.style.patch(styles.hover);
    let spans: Vec<Span<'static>> = line
        .spans
        .into_iter()
        .map(|span| {
            let style = span.style.patch(styles.hover);
            span.style(style)
        })
        .collect();
    Line::from(spans).style(base)
}

/// Splits a pane into the rows the content gets and the pane the bar is placed
/// against. A pane whose content already fits keeps its full width, so the bar
/// shows up only where it has something to say.
///
/// The second rect is the whole pane rather than the reserved column:
/// `ScrollTrack` paints itself into the last column of whatever it is handed,
/// and it needs the pane to know how far a touch press may stray from the bar
/// before it would land in the pane next door.
fn scroll_column(enabled: bool, area: Rect, total: usize) -> (Rect, Option<Rect>) {
    if !enabled || area.width < SCROLLBAR_MIN_WIDTH || total <= area.height as usize {
        return (area, None);
    }
    let [rows, _] =
        Layout::horizontal([Constraint::Min(1), Constraint::Length(SCROLLBAR_WIDTH)]).areas(area);
    (rows, Some(area))
}

fn overwrite(buf: &mut Surface, at: (u16, u16), symbol: &str, style: Style) {
    if let Some(cell) = buf.cell_mut(at) {
        cell.set_symbol(symbol);
        cell.set_style(style);
    }
}

fn placeholder(buf: &mut Surface, area: Rect, text: &str, style: Style) {
    chrome::render_line(
        buf,
        area,
        Line::from(Span::styled(chrome::fit(text, area.width as usize), style)),
    );
}

fn line_at(area: Rect, offset: usize) -> Rect {
    Rect {
        y: area.y + offset as u16,
        height: 1,
        ..area
    }
}

fn digits(count: usize) -> u16 {
    count.max(1).ilog10() as u16 + 1
}

fn tree_row(
    row: &TreeRow,
    selected: bool,
    marked: bool,
    styles: &WorkbenchStyles,
    width: u16,
) -> Line<'static> {
    let marker = match (row.is_dir(), row.expanded) {
        (true, true) => EXPANDED_MARK,
        (true, false) => COLLAPSED_MARK,
        (false, _) => LEAF_INDENT,
    };
    let style = tree_style(row, selected, styles);

    let mut right = Vec::new();
    if row.agent_touched {
        right.push(Span::styled(
            format!("{TAB_GAP}{AGENT_MARK}"),
            styles.agent_touched,
        ));
    }
    if let Some(mark) = row.git {
        right.push(Span::styled(
            format!("{TAB_GAP}{}", mark.letter()),
            git_style(mark, styles),
        ));
    }
    let reserved: usize = right.iter().map(|span| span.content.width()).sum();
    let guides = indent_guides(row.depth);
    let label = format!("{marker}{}", row.name);
    let budget = (width as usize).saturating_sub(reserved + guides.width() + menu_reserve());
    let left = vec![
        Span::styled(
            format!("{MENU_MARK}{TAB_GAP}"),
            match marked {
                true => styles.accent,
                false => styles.dim,
            },
        ),
        Span::styled(guides, styles.border),
        Span::styled(chrome::fit(&label, budget), style),
    ];
    chrome::status_line(left, right, width, styles.background)
}

/// The columns every row keeps at its left for the menu handle, which is the
/// only description of where [`tree_row`] puts it. Ahead of the indent guides
/// rather than after them, so the handles line up as one column however deep
/// the rows around them are nested.
fn menu_reserve() -> usize {
    MENU_MARK.chars().count() + TAB_GAP.len()
}

/// Whether a click at `column` landed on that handle. The gap after the mark
/// goes with it, because what the gap would otherwise do is select the row,
/// which the name already does.
pub(crate) fn on_menu_mark(column: u16, origin: u16) -> bool {
    column
        .checked_sub(origin)
        .is_some_and(|left| (left as usize) < menu_reserve())
}

/// A faint rule down every level the row sits under, so a name three folders
/// deep says which one it belongs to without counting spaces.
fn indent_guides(depth: usize) -> String {
    GUIDE.repeat(depth)
}

/// What a row's name is painted in. An ignored path is drawn back before
/// anything else is said about it, because git says nothing about a path it
/// was told to skip, and the cursor still wins so the row it is on stays
/// legible.
fn tree_style(row: &TreeRow, selected: bool, styles: &WorkbenchStyles) -> Style {
    if selected {
        return styles.selected;
    }
    if row.ignored {
        return styles.dim;
    }
    match (row.git, row.is_dir()) {
        (Some(mark), _) => git_style(mark, styles),
        (None, true) => styles.directory,
        (None, false) => styles.text,
    }
}

/// A section's pinned title: the fold marker, the name, and how many paths or
/// commits are behind it.
fn section_header(
    section: Section,
    count: usize,
    collapsed: bool,
    selected: bool,
    strip: Strip,
    styles: &WorkbenchStyles,
    width: u16,
) -> Line<'static> {
    let marker = match collapsed {
        true => COLLAPSED_MARK,
        false => EXPANDED_MARK,
    };
    let style = match selected {
        true => styles.selected,
        false => styles.title,
    };
    let label = format!("{marker}{}", section.title());
    let count = header_count(count);
    let reserved = count.width() + control_reserve(strip);
    let left = vec![Span::styled(
        chrome::fit(&label, (width as usize).saturating_sub(reserved)),
        style,
    )];
    let mut right = control_spans(section == Section::Staged, strip, styles);
    right.push(Span::styled(count, styles.dim));
    chrome::status_line(left, right, width, styles.background)
}

fn scm_row(
    scm: &Scm,
    section: Section,
    row: ScmRow,
    selected: bool,
    strip: Strip,
    styles: &WorkbenchStyles,
    width: u16,
) -> Line<'static> {
    match row {
        ScmRow::Directory(index) => match scm.dir(section, index) {
            Some(dir) => directory_row(
                dir,
                selected,
                section == Section::Staged,
                strip,
                styles,
                width,
            ),
            None => Line::default(),
        },
        ScmRow::Change { index, depth } => match scm.change(index) {
            Some(change) => {
                change_row(change, depth, scm.is_flat(), selected, strip, styles, width)
            }
            None => Line::default(),
        },
        ScmRow::Commit(index) => match (scm.commit(index), scm.rail(index)) {
            (Some(commit), Some(rail)) => commit_row(
                commit,
                rail,
                scm.is_expanded(index),
                selected,
                styles,
                width,
            ),
            _ => Line::default(),
        },
        ScmRow::CommitFile {
            commit,
            index,
            depth,
        } => match (scm.commit_file(commit, index), scm.rail(commit)) {
            (Some(file), Some(rail)) => {
                commit_file_row(file, rail, depth, scm.is_flat(), selected, styles, width)
            }
            _ => Line::default(),
        },
        ScmRow::Note(text) => note_row(text, styles, width),
    }
}

fn directory_row(
    dir: &Dir,
    selected: bool,
    staged: bool,
    strip: Strip,
    styles: &WorkbenchStyles,
    width: u16,
) -> Line<'static> {
    let marker = match dir.expanded {
        true => EXPANDED_MARK,
        false => COLLAPSED_MARK,
    };
    let style = match selected {
        true => styles.selected,
        false => styles.directory,
    };
    let label = format!(
        "{:indent$}{marker}{}",
        "",
        dir.label,
        indent = dir.depth * DEPTH_INDENT
    );
    if strip.controls.is_empty() {
        return Line::from(Span::styled(chrome::fit(&label, width as usize), style))
            .style(styles.background);
    }
    let budget = (width as usize).saturating_sub(control_reserve(strip));
    let left = vec![Span::styled(chrome::fit(&label, budget), style)];
    let right = control_spans(staged, strip, styles);
    chrome::status_line(left, right, width, styles.background)
}

/// The path, and its git letter on the right. Tree mode indents and shows only
/// the filename; flat mode keeps the whole path and cuts it from the left,
/// which is the end that names the file.
fn change_row(
    change: &Change,
    depth: usize,
    flat: bool,
    selected: bool,
    strip: Strip,
    styles: &WorkbenchStyles,
    width: u16,
) -> Line<'static> {
    let style = match selected {
        true => styles.selected,
        false => styles.text,
    };
    let mut right = control_spans(change.staged, strip, styles);
    right.push(Span::styled(
        format!("{CONTROL_GAP}{}", change.mark.letter()),
        git_style(change.mark, styles),
    ));
    let budget = (width as usize).saturating_sub(CHANGE_TRAILING as usize + control_reserve(strip));
    let left = match flat {
        true => vec![Span::styled(
            chrome::fit_end(&format!("{LEAF_INDENT}{}", change.relative), budget),
            style,
        )],
        false => {
            let name = change
                .relative
                .rsplit(SEPARATOR)
                .next()
                .unwrap_or(&change.relative);
            vec![Span::styled(
                chrome::fit(
                    &format!(
                        "{:indent$}{LEAF_INDENT}{name}",
                        "",
                        indent = depth * DEPTH_INDENT
                    ),
                    budget,
                ),
                style,
            )]
        }
    };
    chrome::status_line(left, right, width, styles.background)
}

/// Author on the right, rail, fold marker, hash and summary on the left, so a
/// narrow sidebar drops the author rather than the line that identifies the
/// commit. The marker says the row opens into what the commit changed.
fn commit_row(
    commit: &Commit,
    rail: Rail,
    expanded: bool,
    selected: bool,
    styles: &WorkbenchStyles,
    width: u16,
) -> Line<'static> {
    let style = match selected {
        true => styles.selected,
        false => styles.text,
    };
    let fold = match expanded {
        true => EXPANDED_MARK,
        false => COLLAPSED_MARK,
    };
    let mark = rail_mark(rail);
    let budget = (width as usize)
        .saturating_sub(commit.id.len() + mark.width() + fold.width() + SUMMARY_GAP.len());
    let left = vec![
        Span::styled(mark, styles.dim),
        Span::styled(fold, styles.dim),
        Span::styled(commit.id.clone(), styles.accent),
        Span::styled(
            format!("{SUMMARY_GAP}{}", chrome::fit(&commit.summary, budget)),
            style,
        ),
    ];
    let right = vec![Span::styled(commit.author.clone(), styles.dim)];
    chrome::status_line(left, right, width, styles.background)
}

/// One path an expanded commit touched, drawn like a change row but over the
/// rail instead of a control strip: nothing in the graph can be staged.
fn commit_file_row(
    file: &CommitPath,
    rail: Rail,
    depth: usize,
    flat: bool,
    selected: bool,
    styles: &WorkbenchStyles,
    width: u16,
) -> Line<'static> {
    let style = match selected {
        true => styles.selected,
        false => styles.text,
    };
    let mark = match rail {
        Rail::Side => RAIL_UNDER_SIDE,
        _ => RAIL_UNDER_TRUNK,
    };
    let right = vec![Span::styled(
        format!("{CONTROL_GAP}{}", file.mark.letter()),
        git_style(file.mark, styles),
    )];
    let budget = (width as usize).saturating_sub(CHANGE_TRAILING as usize + mark.width());
    let label = match flat {
        true => chrome::fit_end(&file.relative, budget),
        false => {
            let name = file
                .relative
                .rsplit(SEPARATOR)
                .next()
                .unwrap_or(&file.relative);
            chrome::fit(
                &format!(
                    "{:indent$}{LEAF_INDENT}{name}",
                    "",
                    indent = depth * DEPTH_INDENT
                ),
                budget,
            )
        }
    };
    let left = vec![Span::styled(mark, styles.dim), Span::styled(label, style)];
    chrome::status_line(left, right, width, styles.background)
}

/// What an expanded commit says instead of a path, when it has none to list.
fn note_row(text: &'static str, styles: &WorkbenchStyles, width: u16) -> Line<'static> {
    let label = format!("{RAIL_UNDER_TRUNK}{LEAF_INDENT}{text}");
    Line::from(Span::styled(
        chrome::fit(&label, width as usize),
        styles.dim,
    ))
    .style(styles.background)
}

const fn rail_mark(rail: Rail) -> &'static str {
    match rail {
        Rail::Trunk => RAIL_TRUNK,
        Rail::Merge => RAIL_MERGE,
        Rail::Side => RAIL_SIDE,
    }
}

fn field_row(
    label: &'static str,
    text: &str,
    caret: bool,
    styles: &WorkbenchStyles,
    width: u16,
) -> Line<'static> {
    let budget = (width as usize).saturating_sub(label.len() + usize::from(caret));
    let mut spans = vec![
        Span::styled(label, if caret { styles.accent } else { styles.dim }),
        Span::styled(chrome::fit_end(text, budget), styles.text),
    ];
    if caret {
        spans.push(Span::styled(CARET, styles.cursor));
    }
    Line::from(spans)
}

fn toggle_row(
    search: &Search,
    pointed: Option<Toggle>,
    styles: &WorkbenchStyles,
    width: u16,
) -> Line<'static> {
    let query = search.query();
    let left = TOGGLES
        .into_iter()
        .flat_map(|(toggle, label)| {
            let on = match toggle {
                Toggle::Case => query.case_sensitive,
                Toggle::Word => query.whole_word,
                Toggle::Regex => query.regex,
            };
            let mut style = if on { styles.selected } else { styles.dim };
            if pointed == Some(toggle) && !on {
                style = style.patch(styles.hover);
            }
            [
                Span::styled(TAB_GAP, styles.background),
                Span::styled(label, style),
            ]
        })
        .collect();

    let (hits, files) = search.counts();
    let summary = match (search.is_running(), search.has_results()) {
        (true, _) => SEARCHING.to_owned(),
        (false, true) => {
            let truncated = if search.truncated() { TRUNCATED } else { "" };
            format!("{hits} in {files}{truncated}")
        }
        (false, false) => String::new(),
    };
    chrome::status_line(
        left,
        vec![Span::styled(summary, styles.dim)],
        width,
        styles.background,
    )
}

fn search_row(
    search: &Search,
    row: SearchRow,
    root: &WorkbenchPath,
    selected: bool,
    styles: &WorkbenchStyles,
    width: u16,
) -> Line<'static> {
    match row {
        SearchRow::File(index) => match search.file(index) {
            Some(path) => Line::from(Span::styled(
                chrome::fit_end(&path.display_relative(root), width as usize),
                if selected {
                    styles.selected
                } else {
                    styles.directory
                },
            )),
            None => Line::default(),
        },
        SearchRow::Hit(index) => match search.hit(index) {
            Some(hit) => hit_row(hit, selected, styles, width),
            None => Line::default(),
        },
    }
}

/// The line number, then the matching text with the match itself picked out, so
/// the eye lands on the same thing the cursor would.
fn hit_row(hit: &Hit, selected: bool, styles: &WorkbenchStyles, width: u16) -> Line<'static> {
    let (style, matched) = match selected {
        true => (styles.selected, styles.match_highlight_selected),
        false => (styles.text, styles.match_highlight),
    };
    let number = format!("{LEAF_INDENT}{}{SUMMARY_GAP}", hit.line);
    let budget = (width as usize).saturating_sub(number.chars().count());
    let text: Vec<char> = hit.text.chars().collect();
    let (start, end) = (hit.range.0.min(text.len()), hit.range.1.min(text.len()));

    let mut spans = vec![Span::styled(number, styles.gutter)];
    for (slice, painted) in [
        (&text[..start], style),
        (&text[start..end], matched),
        (&text[end..], style),
    ] {
        if !slice.is_empty() {
            spans.push(Span::styled(slice.iter().collect::<String>(), painted));
        }
    }
    truncate(spans, budget)
}

/// Cuts a painted row to the pane's width without losing the styling of the
/// spans that survive.
fn truncate(spans: Vec<Span<'static>>, budget: usize) -> Line<'static> {
    let mut used = 0;
    let mut kept = Vec::with_capacity(spans.len());
    for span in spans {
        if used >= budget {
            break;
        }
        let fitted = chrome::fit(&span.content, budget - used);
        used += UnicodeWidthStr::width(fitted.as_str());
        kept.push(Span::styled(fitted, span.style));
    }
    Line::from(kept)
}

fn git_style(mark: GitMark, styles: &WorkbenchStyles) -> Style {
    match mark {
        GitMark::Modified => styles.git_modified,
        GitMark::Added => styles.git_added,
        GitMark::Deleted => styles.git_deleted,
        GitMark::Untracked => styles.git_untracked,
        GitMark::Conflicted => styles.git_conflicted,
    }
}

/// The line number, or the blank that stands in its place where a wrapped line
/// carries on. Both are the same width in the same style, so a continuation
/// reads as part of the line above rather than as a line of its own.
///
/// A diff tab numbers the file rather than the rendered column, and says with a
/// marker which side each number belongs to. Given the room it shows both
/// sides, the way a side-by-side diff does.
fn gutter_row(
    tab: &Tab,
    row: VisualRow,
    styles: &WorkbenchStyles,
    focused: bool,
    width: u16,
    columns: Option<DiffColumns>,
) -> Line<'static> {
    let on_cursor = focused && tab.buffer.cursor().line == row.line;
    let diff = tab.diff_rows().and_then(|rows| rows.get(row.line));
    let style = match (on_cursor, diff.is_some()) {
        (true, _) => styles.text,
        (false, true) => styles.diff_line_nr,
        (false, false) => styles.gutter,
    };
    let text = match (diff, columns) {
        (Some(diff), Some(columns)) if row.index == 0 => diff_gutter(diff, columns),
        (Some(_), _) => " ".repeat(width as usize),
        _ => {
            let number = match row.index {
                0 => (row.line + 1).to_string(),
                _ => String::new(),
            };
            let gap = GUTTER_GAP as usize;
            format!("{number:>inner$}{:gap$}", "", inner = width as usize - gap)
        }
    };
    Line::from(Span::styled(text, style))
}

/// How a diff tab spends its gutter. Two columns of numbers say which line each
/// side of a change sits on, the way a side-by-side diff does, but they cost
/// the code the room to read; a narrow pane gets one column and leans on the
/// marker to say which side the number belongs to.
#[derive(Clone, Copy)]
enum DiffColumns {
    Both(u16),
    One(u16),
}

impl DiffColumns {
    fn for_pane(rows: &[DiffRow], pane: u16) -> Self {
        let widest = |pick: fn(&DiffRow) -> Option<usize>| {
            digits(rows.iter().filter_map(pick).max().unwrap_or(1))
        };
        let inner = widest(|row| row.before).max(widest(|row| row.after));
        let both = Self::Both(inner);
        match pane.saturating_sub(both.width()) >= MIN_CODE_COLUMNS {
            true => both,
            false => Self::One(inner),
        }
    }

    fn width(self) -> u16 {
        match self {
            Self::Both(inner) => inner * 2 + 3 + GUTTER_GAP,
            Self::One(inner) => inner + 2 + GUTTER_GAP,
        }
    }
}

fn diff_gutter(row: &DiffRow, columns: DiffColumns) -> String {
    let mark = match row.kind {
        DiffKind::Added => MARK_ADDED,
        DiffKind::Removed => MARK_REMOVED,
        DiffKind::Reindented => MARK_REINDENTED,
        DiffKind::Context | DiffKind::Header => MARK_UNCHANGED,
    };
    let number = |line: Option<usize>, width: u16| match line {
        Some(line) => format!("{line:>width$}", width = width as usize),
        None => " ".repeat(width as usize),
    };
    let body = match columns {
        DiffColumns::Both(inner) => format!(
            "{} {} {mark}",
            number(row.before, inner),
            number(row.after, inner)
        ),
        DiffColumns::One(inner) => format!("{} {mark}", number(row.before.or(row.after), inner)),
    };
    let gap = GUTTER_GAP as usize;
    format!("{body}{:gap$}", "")
}

/// The base colour of a row, its syntax segments and everything painted over
/// them: find matches first, then the selection, then the cursor on top.
fn text_row(
    tab: &Tab,
    line: usize,
    segments: Option<&[StyledSegment]>,
    styles: &WorkbenchStyles,
    focused: bool,
    window: (usize, usize),
) -> Line<'static> {
    let diff = tab.diff_rows().and_then(|rows| rows.get(line));
    let (base, fill) = match diff.map(|row| row.kind) {
        Some(DiffKind::Added | DiffKind::Reindented) => (styles.diff_new, Some(styles.diff_new)),
        Some(DiffKind::Removed) => (styles.diff_old, Some(styles.diff_old)),
        Some(DiffKind::Header) => (styles.title, None),
        _ => (styles.text, None),
    };

    // Emphasis goes down first so a find match and the selection still paint
    // over it: what the reader asked to see outranks what the diff points at.
    let mut overlays: Vec<(Range<usize>, Style)> = diff
        .into_iter()
        .flat_map(|row| row.emphasis.iter().cloned())
        .map(|range| {
            (
                range,
                match diff.map(|row| row.kind) {
                    Some(DiffKind::Removed) => styles.diff_old_emphasis,
                    _ => styles.diff_new_emphasis,
                },
            )
        })
        .collect();
    let current = tab.find.current();
    for found in tab.find.on_line(line) {
        let style = match Some(*found) == current {
            true => styles.current_match,
            false => styles.match_highlight,
        };
        overlays.push((found.start..found.end, style));
    }
    if let Some((from, to)) = tab.buffer.selection()
        && (from.line..=to.line).contains(&line)
    {
        let start = if line == from.line { from.col } else { 0 };
        let end = match line == to.line {
            true => to.col,
            false => tab.buffer.line(line).chars().count() + 1,
        };
        overlays.push((start..end, styles.selection));
    }
    let cursor = tab.buffer.cursor();
    if focused && cursor.line == line {
        overlays.push((cursor.col..cursor.col + 1, styles.cursor));
    }

    render::Row {
        text: tab.buffer.line(line),
        segments,
        base,
        fill,
        overlays: &overlays,
    }
    .paint(window.0, window.1)
}

fn status_left(
    tab: &Tab,
    path: &str,
    styles: &WorkbenchStyles,
    width: usize,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if tab.is_dirty() {
        spans.push(Span::styled(DIRTY_MARK, styles.accent));
    }
    spans.push(Span::styled(chrome::fit_end(path, width), styles.dim));
    if tab.conflict {
        spans.push(Span::styled(HINT_GAP, styles.dim));
        spans.push(Span::styled(CONFLICT_NOTICE, styles.error));
    }
    spans
}

fn hints(pairs: &[(&'static str, &'static str)], styles: &WorkbenchStyles) -> Vec<Span<'static>> {
    let mut spans = Vec::with_capacity(pairs.len() * 3);
    for (key, label) in pairs {
        spans.push(Span::styled(HINT_GAP, styles.dim));
        spans.push(Span::styled(*key, styles.accent));
        spans.push(Span::styled(format!(" {label}"), styles.dim));
    }
    spans
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use test_case::test_case;

    use ratatui::layout::Rect;

    use super::menu_panel;
    use super::{
        CHANGE_TRAILING, COLLAPSED_MARK, Commit, CommitPath, Control, DiffColumns, DiffKind,
        DiffRow, EXPANDED_MARK, Editor, Focus, GitMark, Line, MENU_MARK, MIN_CODE_COLUMNS,
        RAIL_TRUNK, RAIL_UNDER_SIDE, RAIL_UNDER_TRUNK, Rail, ScmRow, Section, SidebarView, Style,
        Tab, TabHit, TabPart, Toggle, TreeRow, Workbench, WorkbenchStyles, commit_file_row,
        commit_row, control_at, diff_gutter, header_at, keys, on_menu_mark, scm_controls,
        scroll_column, tab_at, toggle_at, tree_row, tree_style, visible_range,
    };
    use crate::fs::backend::WorkbenchPath;
    use crate::fs::tree::EntryKind;
    use crate::menu::Menu;

    const WRONG_TAB: &str = "the column does not fall on the tab the strip painted there";
    const WRONG_CONTROL: &str = "the column does not fall on the control the row painted there";
    const WRONG_STRIP: &str = "the strip is not showing the tabs it has room for";
    const WRONG_BAR: &str = "the pane gave up the wrong amount of room to its scrollbar";
    const WRONG_VIEW: &str = "the column does not fall on the view the header painted there";
    const WRONG_TOGGLE: &str = "the column does not fall on the button the row painted there";
    const WRONG_HINT: &str = "the status bar is not offering what the focused pane needs most";
    const WRONG_PAINT: &str = "the row is not painted the way its standing asks for";
    const WRONG_GUIDES: &str = "the rules down the indent are not drawn as chrome";
    const WRONG_HANDLE: &str = "the handle on the menu is not drawn back the way chrome is";
    const HANDLE_MISPLACED: &str =
        "the column the row paints the handle on is not the one a press reaches";
    const PANEL_MISPLACED: &str = "the panel is not where the cell it was asked for puts it";
    const PANEL_OFF_FRAME: &str = "the panel ran off the frame it was given";
    const TREE_NAME: &str = "a.rs";
    const TREE_WIDTH: u16 = 40;
    const COMMIT_ID: &str = "abc1234";
    const GRAPH_WIDTH: u16 = 48;
    /// Room for the longest menu either target builds, with edges close enough
    /// to reach. Away from the origin, so a clamp that forgets where the frame
    /// starts is caught.
    const FRAME: Rect = Rect {
        x: 3,
        y: 2,
        width: 40,
        height: 20,
    };

    /// Wide enough for every tab the cases open, so only the ones that ask for
    /// a narrow strip have to say so.
    fn strip(x: u16) -> Rect {
        Rect::new(x, 0, 80, 1)
    }

    const GUTTER_SIDES: &str = "a diff gutter names the file's lines, not the column's";
    const GUTTER_ROOM: &str = "a gutter must not grow past the room the pane has for code";

    fn numbered(before: Option<usize>, after: Option<usize>, kind: DiffKind) -> DiffRow {
        DiffRow {
            before,
            after,
            kind,
            ..DiffRow::default()
        }
    }

    #[test_case(Some(9), Some(12), DiffKind::Context, "  9  12   " ; "context numbers both")]
    #[test_case(Some(9), None, DiffKind::Removed, "  9     - " ; "a removal numbers the before side")]
    #[test_case(None, Some(12), DiffKind::Added, "     12 + " ; "an addition numbers the after side")]
    #[test_case(Some(9), Some(12), DiffKind::Reindented, "  9  12 ~ " ; "a reindent is neither")]
    fn a_wide_diff_gutter_numbers_both_sides(
        before: Option<usize>,
        after: Option<usize>,
        kind: DiffKind,
        expected: &str,
    ) {
        let rows = [numbered(Some(999), Some(12), DiffKind::Context)];
        let columns = DiffColumns::for_pane(&rows, 120);

        assert_eq!(
            diff_gutter(&numbered(before, after, kind), columns),
            expected,
            "{GUTTER_SIDES}"
        );
    }

    #[test_case(120, "  9 - " ; "a wide pane still fits one column when both would not")]
    #[test_case(40, "  9 - " ; "a narrow pane spends its columns on the code")]
    fn a_narrow_diff_gutter_numbers_the_side_the_marker_names(pane: u16, expected: &str) {
        let rows = [numbered(Some(999), Some(999), DiffKind::Context)];
        let columns = DiffColumns::for_pane(&rows, pane);
        let painted = diff_gutter(&numbered(Some(9), None, DiffKind::Removed), columns);

        assert!(
            columns.width() + MIN_CODE_COLUMNS <= pane || matches!(columns, DiffColumns::One(_)),
            "{GUTTER_ROOM}"
        );
        assert_eq!(painted.len(), columns.width() as usize, "{GUTTER_ROOM}");
        if pane < 60 {
            assert_eq!(painted, expected, "{GUTTER_SIDES}");
        }
    }

    /// Two two-column titles, so every tab spans ` ab \u{d7} ` and the second
    /// starts where the first ended.
    fn editor(dirty: bool) -> Editor {
        let mut editor = Editor::default();
        for title in ["ab", "cd"] {
            let mut tab = Tab::synthetic(
                Path::new(title),
                title.to_owned(),
                vec![DiffRow::default()],
                0,
            );
            if dirty {
                let edit = tab.buffer.insert("x");
                tab.record(edit);
            }
            editor.push(tab);
        }
        editor
    }

    fn hit(index: usize, part: TabPart) -> Option<TabHit> {
        Some(TabHit { index, part })
    }

    #[test_case(0, hit(0, TabPart::Menu) ; "the gap in front of a tab reaches its mark")]
    #[test_case(1, hit(0, TabPart::Menu) ; "and so does the mark itself")]
    #[test_case(2, hit(0, TabPart::Body) ; "the gap after the mark selects the tab")]
    #[test_case(3, hit(0, TabPart::Body) ; "and so does the title")]
    #[test_case(6, hit(0, TabPart::Close) ; "the close mark is its own target")]
    #[test_case(7, hit(0, TabPart::Body) ; "the gap after the close mark is not it")]
    #[test_case(8, hit(1, TabPart::Menu) ; "the next tab starts where the last ended")]
    #[test_case(14, hit(1, TabPart::Close) ; "every tab has its own close mark")]
    #[test_case(16, None ; "past the last tab is nothing")]
    fn a_column_falls_on_the_tab_the_strip_painted(column: u16, expected: Option<TabHit>) {
        assert_eq!(
            tab_at(&editor(false), column, strip(0)),
            expected,
            "{WRONG_TAB}"
        );
    }

    #[test]
    fn a_dirty_mark_shifts_the_close_mark_along_with_the_title() {
        assert_eq!(
            tab_at(&editor(true), 7, strip(0)),
            hit(0, TabPart::Close),
            "{WRONG_TAB}"
        );
    }

    #[test]
    fn the_origin_is_taken_off_before_the_strip_is_measured() {
        let editor = editor(false);

        assert_eq!(
            tab_at(&editor, 11, strip(0)),
            tab_at(&editor, 14, strip(3)),
            "{WRONG_TAB}"
        );
        assert_eq!(tab_at(&editor, 2, strip(3)), None, "{WRONG_TAB}");
    }

    /// Both tabs span eight columns, so a strip fifteen wide has room for one
    /// and the second is the one the cursor is on.
    #[test]
    fn a_strip_too_narrow_for_both_shows_the_active_one() {
        let editor = editor(false);

        assert_eq!(visible_range(&editor, 16), 0..2, "{WRONG_STRIP}");
        assert_eq!(visible_range(&editor, 15), 1..2, "{WRONG_STRIP}");
    }

    /// The strip is measured from the tab it is showing, so the first column
    /// of a scrolled strip is the second tab rather than the first.
    #[test]
    fn a_scrolled_strip_measures_from_what_it_shows() {
        let editor = editor(false);

        assert_eq!(
            tab_at(&editor, 3, Rect::new(0, 0, 15, 1)),
            hit(1, TabPart::Body),
            "{WRONG_TAB}"
        );
    }

    #[test_case(true, 5, true ; "content past the pane takes a column")]
    #[test_case(true, 4, false ; "content that fits keeps the whole width")]
    #[test_case(false, 40, false ; "a bar turned off takes nothing")]
    fn a_pane_gives_up_a_column_only_when_it_has_something_to_say(
        enabled: bool,
        total: usize,
        expected: bool,
    ) {
        let area = Rect::new(0, 0, 20, 4);

        let (rows, bar) = scroll_column(enabled, area, total);

        assert_eq!(bar.is_some(), expected, "{WRONG_BAR}");
        assert_eq!(rows.width, area.width - u16::from(expected), "{WRONG_BAR}");
        // The pane, not the reserved column: the bar measures its own touch
        // margin against what the pane can spare.
        assert_eq!(bar, expected.then_some(area), "{WRONG_BAR}");
    }

    /// A bar in a one-column pane would be the whole pane.
    #[test]
    fn a_pane_too_narrow_for_a_bar_keeps_what_it_has() {
        let (rows, bar) = scroll_column(true, Rect::new(0, 0, 1, 4), 40);

        assert!(bar.is_none(), "{WRONG_BAR}");
        assert_eq!(rows.width, 1, "{WRONG_BAR}");
    }

    #[test]
    fn an_empty_strip_shows_nothing() {
        assert_eq!(visible_range(&Editor::default(), 80), 0..0, "{WRONG_STRIP}");
    }

    #[test_case(0, None ; "the gap in front of a segment is not a button")]
    #[test_case(1, Some(SidebarView::Explorer) ; "the first label switches to the explorer")]
    #[test_case(5, Some(SidebarView::Explorer) ; "the whole label is the button")]
    #[test_case(7, Some(SidebarView::SourceControl) ; "the second label switches to source control")]
    #[test_case(11, Some(SidebarView::Search) ; "the third label switches to search")]
    #[test_case(15, None ; "past the last label is nothing")]
    fn a_column_falls_on_the_view_the_header_painted(column: u16, expected: Option<SidebarView>) {
        assert_eq!(header_at(column, 0), expected, "{WRONG_VIEW}");
    }

    #[test_case(Focus::Editor, keys::SAVE.label ; "the editor is offered save")]
    #[test_case(Focus::Sidebar, keys::VIEW_EXPLORER.label ; "the sidebar is offered its views")]
    fn the_focused_pane_leads_the_status_hints(focus: Focus, expected: &str) {
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.focus = focus;
        assert_eq!(
            workbench.status_hints().first().map(|(bind, _)| *bind),
            Some(expected),
            "{WRONG_HINT}"
        );
    }

    #[test_case(0, None ; "the gap in front of a button is not it")]
    #[test_case(1, Some(Toggle::Case) ; "the first label toggles case")]
    #[test_case(4, Some(Toggle::Word) ; "the second label toggles whole word")]
    #[test_case(7, Some(Toggle::Regex) ; "the third label toggles regex")]
    #[test_case(9, None ; "past the last label is nothing")]
    fn a_column_falls_on_the_button_the_row_painted(column: u16, expected: Option<Toggle>) {
        assert_eq!(toggle_at(column, 0), expected, "{WRONG_TOGGLE}");
    }

    const CHANGE: ScmRow = ScmRow::Change { index: 0, depth: 0 };
    const FOLDER: ScmRow = ScmRow::Directory(0);

    #[test_case(Section::Unstaged, None => vec![Control::Revert, Control::Stage] ; "an unstaged header stages or reverts everything")]
    #[test_case(Section::Staged, None => vec![Control::Stage] ; "a staged header unstages everything")]
    #[test_case(Section::Unstaged, Some(CHANGE) => vec![Control::Open, Control::Revert, Control::Stage] ; "an unstaged file opens, reverts and stages")]
    #[test_case(Section::Unstaged, Some(FOLDER) => vec![Control::Revert, Control::Stage] ; "an unstaged folder reverts everything under it")]
    #[test_case(Section::Staged, Some(CHANGE) => vec![Control::Open, Control::Stage] ; "a staged file also opens")]
    #[test_case(Section::Staged, Some(FOLDER) => vec![Control::Stage] ; "a folder never opens, having no file to open")]
    #[test_case(Section::Graph, None => Vec::<Control>::new() ; "the graph stages nothing")]
    #[test_case(Section::Graph, Some(ScmRow::Commit(0)) => Vec::<Control>::new() ; "a commit stages nothing")]
    fn a_row_offers_the_controls_it_can_act_on(
        section: Section,
        row: Option<ScmRow>,
    ) -> Vec<Control> {
        scm_controls(section, row).to_vec()
    }

    /// A ten column row keeping two columns for its git letter, so a strip of
    /// two controls runs from column two to column seven.
    fn change_strip(column: u16) -> Option<Control> {
        let controls = scm_controls(Section::Staged, Some(CHANGE));
        control_at(column, Rect::new(0, 0, 10, 1), CHANGE_TRAILING, controls)
    }

    #[test_case(1 => None ; "the name reaches up to the strip")]
    #[test_case(2 => Some(Control::Open) ; "the air in front of a control belongs to it")]
    #[test_case(3 => Some(Control::Open) ; "and so does the mark itself")]
    #[test_case(4 => Some(Control::Open) ; "and the air after it")]
    #[test_case(5 => Some(Control::Stage) ; "the next column is the next control")]
    #[test_case(7 => Some(Control::Stage) ; "up to its last column")]
    #[test_case(8 => None ; "the git letter is not a control")]
    fn a_column_falls_on_the_control_the_row_painted(column: u16) -> Option<Control> {
        change_strip(column)
    }

    #[test]
    fn a_row_too_narrow_for_a_strip_answers_nothing() {
        let controls = scm_controls(Section::Staged, Some(CHANGE));
        let narrow = Rect::new(0, 0, 3, 1);
        for column in 0..narrow.width {
            assert_eq!(
                control_at(column, narrow, CHANGE_TRAILING, controls),
                None,
                "{WRONG_CONTROL}"
            );
        }
    }

    /// One entry of the explorer, so a case only has to say what it changes.
    fn entry(kind: EntryKind, depth: usize) -> TreeRow {
        TreeRow {
            path: WorkbenchPath::Local(Path::new(TREE_NAME).to_path_buf()),
            resource: None,
            name: TREE_NAME.to_owned(),
            depth,
            kind,
            expanded: false,
            git: None,
            agent_touched: false,
            ignored: false,
        }
    }

    #[test_case(None, false, false => WorkbenchStyles::default().text ; "a file nobody has changed is plain")]
    #[test_case(Some(GitMark::Modified), false, false => WorkbenchStyles::default().git_modified ; "a changed file wears its mark's colour")]
    #[test_case(None, true, false => WorkbenchStyles::default().dim ; "an ignored file is drawn back")]
    #[test_case(Some(GitMark::Untracked), true, false => WorkbenchStyles::default().dim ; "ignored beats whatever git says about it")]
    #[test_case(Some(GitMark::Modified), true, true => WorkbenchStyles::default().selected ; "the cursor beats both")]
    fn a_file_is_painted_by_what_it_is(
        git: Option<GitMark>,
        ignored: bool,
        selected: bool,
    ) -> Style {
        let row = TreeRow {
            git,
            ignored,
            ..entry(EntryKind::File, 0)
        };
        tree_style(&row, selected, &WorkbenchStyles::default())
    }

    #[test_case(false => WorkbenchStyles::default().directory ; "a directory has a colour of its own")]
    #[test_case(true => WorkbenchStyles::default().dim ; "one git skips is drawn back like everything under it")]
    fn a_directory_is_painted_by_whether_git_looks_at_it(ignored: bool) -> Style {
        let row = TreeRow {
            ignored,
            ..entry(EntryKind::Directory, 0)
        };
        tree_style(&row, false, &WorkbenchStyles::default())
    }

    /// What a row reads as once painted, without the filler that pads it out.
    fn painted(row: &TreeRow) -> String {
        tree_row(row, false, false, &WorkbenchStyles::default(), TREE_WIDTH)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
            .trim_end()
            .to_owned()
    }

    #[test_case(0 => format!("{MENU_MARK}   {TREE_NAME}") ; "a row at the root has no folder to point at")]
    #[test_case(1 => format!("{MENU_MARK} \u{2502}   {TREE_NAME}") ; "one level in stands under one rule")]
    #[test_case(3 => format!("{MENU_MARK} \u{2502} \u{2502} \u{2502}   {TREE_NAME}") ; "and every level after it adds another")]
    fn a_nested_row_stands_under_a_rule_for_each_level(depth: usize) -> String {
        painted(&entry(EntryKind::File, depth))
    }

    /// What one graph row reads as once painted, without the filler that pads
    /// it out to the sidebar's width.
    fn graph_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
            .trim_end()
            .to_owned()
    }

    fn a_commit(id: &str) -> Commit {
        Commit {
            id: id.to_owned(),
            summary: "did a thing".to_owned(),
            author: "Tester".to_owned(),
            parents: Vec::new(),
        }
    }

    #[test_case(false => format!("{RAIL_TRUNK}{COLLAPSED_MARK}{COMMIT_ID}") ; "a closed commit says it opens")]
    #[test_case(true => format!("{RAIL_TRUNK}{EXPANDED_MARK}{COMMIT_ID}") ; "an open one says it closes")]
    fn a_commit_row_says_whether_it_is_showing_what_it_changed(expanded: bool) -> String {
        let line = commit_row(
            &a_commit(COMMIT_ID),
            Rail::Trunk,
            expanded,
            false,
            &WorkbenchStyles::default(),
            GRAPH_WIDTH,
        );

        assert!(graph_text(&line).ends_with("Tester"), "{WRONG_PAINT}");
        line.spans[..3]
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test_case(Rail::Trunk => RAIL_UNDER_TRUNK.to_owned() ; "a file under the trunk keeps the trunk beside it")]
    #[test_case(Rail::Merge => RAIL_UNDER_TRUNK.to_owned() ; "and so does one under a merge")]
    #[test_case(Rail::Side => RAIL_UNDER_SIDE.to_owned() ; "one under a side branch keeps both lines")]
    fn a_path_under_a_commit_keeps_the_rail_unbroken(rail: Rail) -> String {
        let file = CommitPath {
            relative: TREE_NAME.to_owned(),
            mark: GitMark::Modified,
        };

        let line = commit_file_row(
            &file,
            rail,
            1,
            false,
            false,
            &WorkbenchStyles::default(),
            GRAPH_WIDTH,
        );

        assert!(
            graph_text(&line).ends_with(GitMark::Modified.letter()),
            "{WRONG_PAINT}"
        );
        line.spans[0].content.as_ref().to_owned()
    }

    #[test]
    fn the_rules_are_faint_so_the_name_is_still_what_the_row_says() {
        let styles = WorkbenchStyles::default();
        let line = tree_row(
            &entry(EntryKind::File, 2),
            false,
            false,
            &styles,
            TREE_WIDTH,
        );

        assert_eq!(line.spans[0].style, styles.dim, "{WRONG_HANDLE}");
        assert_eq!(line.spans[1].style, styles.border, "{WRONG_GUIDES}");
        assert_eq!(line.spans[2].style, styles.text, "{WRONG_PAINT}");
    }

    /// The handle sits at the left margin whatever the row is, so a press
    /// reaches it without the depth of the row entering into it.
    #[test_case(FRAME.x, 0 => true ; "the mark itself is the handle")]
    #[test_case(FRAME.x + 1, 0 => true ; "and so is the gap that follows it")]
    #[test_case(FRAME.x + 2, 0 => false ; "the rules past it belong to the row")]
    #[test_case(FRAME.x + 2, 3 => false ; "however deep the row is nested")]
    #[test_case(FRAME.x - 1, 0 => false ; "and a column left of the pane reaches nothing")]
    fn a_press_at_the_left_margin_reaches_the_handle(column: u16, depth: usize) -> bool {
        assert!(
            painted(&entry(EntryKind::File, depth)).starts_with(MENU_MARK),
            "{HANDLE_MISPLACED}"
        );

        on_menu_mark(column, FRAME.x)
    }

    fn menu(at: (u16, u16)) -> Menu {
        Menu::for_row(&entry(EntryKind::File, 0), at)
    }

    #[test]
    fn the_panel_hangs_under_the_cell_it_was_asked_for() {
        let panel = menu_panel(&menu((4, 2)), FRAME);

        assert_eq!((panel.x, panel.y), (4, 3), "{PANEL_MISPLACED}");
    }

    /// A row near the bottom is where a menu is asked for most, since that is
    /// where a long tree ends.
    #[test]
    fn a_panel_with_no_room_under_it_stands_over_the_cell_instead() {
        let anchor = FRAME.bottom() - 1;

        let panel = menu_panel(&menu((4, anchor)), FRAME);

        assert_eq!(panel.bottom(), anchor, "{PANEL_MISPLACED}");
        assert!(panel.y >= FRAME.y, "{PANEL_OFF_FRAME}");
    }

    #[test]
    fn a_panel_asked_for_at_the_right_edge_is_pulled_back_inside() {
        let panel = menu_panel(&menu((FRAME.right() - 2, 0)), FRAME);

        assert_eq!(panel.right(), FRAME.right(), "{PANEL_OFF_FRAME}");
        assert!(panel.x >= FRAME.x, "{PANEL_OFF_FRAME}");
    }

    /// Every label has to fit, or the menu says something other than what it
    /// does.
    #[test]
    fn the_panel_is_wide_enough_for_the_longest_label() {
        let menu = menu((0, 0));

        let panel = menu_panel(&menu, FRAME);

        assert!(panel.width as usize > menu.width(), "{PANEL_MISPLACED}");
    }
}
