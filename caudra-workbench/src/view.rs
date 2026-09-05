//! Painting the workbench.
//!
//! Kept apart from the state and the keymap because it is the only part that
//! knows about terminal columns, and because the layout it records in
//! [`PaneRects`] is what paging and scrolling read back.

use std::path::Path;

use caudra_highlight::StyledSegment;
use ratatui::Frame;
use ratatui::buffer::Buffer as Surface;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::editor::{DiffKind, Editor, Tab, render};
use crate::fs::tree::{GitMark, Row as TreeRow};
use crate::scm::graph::Rail;
use crate::scm::repo::{Change, Commit};
use crate::scm::tree::{Dir, SEPARATOR};
use crate::scm::{Row as ScmRow, Scm, Section};
use crate::search::engine::Hit;
use crate::search::{Field as SearchField, Row as SearchRow, Search};
use crate::{
    Focus, SidebarView, Workbench, WorkbenchStyles, chrome, keys, layout, layout_sections,
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
const CARET: &str = "\u{2588}";
const ENTER_LABEL: &str = "Enter";
const SUMMARY_GAP: &str = " ";
/// The two-column rail down the left of the graph. A commit on the chain of
/// first parents sits on the trunk, one a merge brought in hangs beside it.
const RAIL_TRUNK: &str = "\u{25cf} ";
const RAIL_MERGE: &str = "\u{25c9} ";
const RAIL_SIDE: &str = "\u{2502}\u{25cb}";
const TREE_LABEL: &str = "TREE";
const FLAT_LABEL: &str = "FLAT";
const TREE_HINT: &str = "tree";
const FLAT_HINT: &str = "flat";
const COUNT_GAP: &str = " ";
const EMPTY_TREE: &str = "Nothing to show";
const DIRTY_MARK: &str = "\u{25cf}";
const AGENT_MARK: &str = "\u{25e6}";
const EXPANDED_MARK: &str = "\u{25be} ";
const COLLAPSED_MARK: &str = "\u{25b8} ";
const LEAF_INDENT: &str = "  ";
const DEPTH_INDENT: usize = 2;
const GUTTER_GAP: u16 = 1;
const CONFLICT_NOTICE: &str = "Changed on disk since it was opened";
const FIND_PROMPT: &str = "Find: ";
const GOTO_PROMPT: &str = "Go to line: ";
const NO_MATCHES: &str = "No results";
const TAB_GAP: &str = " ";
const CLOSE_MARK: &str = "\u{d7}";
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

/// One of the search view's three buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Toggle {
    Case,
    Word,
    Regex,
}

impl Workbench {
    pub fn view(&mut self, frame: &mut Frame, area: Rect) {
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
    }

    fn render_sidebar(&mut self, buf: &mut Surface, area: Rect) {
        let [header, body] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
        self.panes.header = header;
        let focused = self.focus == Focus::Sidebar;
        let context = match self.sidebar {
            SidebarView::SourceControl => self.scm.head().unwrap_or_default().to_owned(),
            _ => self
                .root
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
        };
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
        if self.sidebar == SidebarView::SourceControl {
            let mode = match self.scm.is_flat() {
                true => FLAT_LABEL,
                false => TREE_LABEL,
            };
            let pointed = self
                .hovering(header)
                .is_some_and(|at| mode_at(at.0, header, context.width()));
            right.push(Span::styled(mode, emphasized(self.styles.dim, pointed, &self.styles)));
            right.push(Span::styled(TAB_GAP, self.styles.background));
        }
        right.push(Span::styled(context, self.styles.dim));
        chrome::render_line(
            buf,
            header,
            chrome::status_line(switcher, right, header.width, self.styles.dim),
        );

        match self.sidebar {
            SidebarView::Explorer => self.render_tree(buf, body, focused),
            SidebarView::SourceControl => self.render_scm(buf, body, focused),
            SidebarView::Search => self.render_search(buf, body, focused),
        }
    }

    /// Two fields and a toggle row over the results, so the whole question and
    /// its answer stay visible in one column.
    fn render_search(&mut self, buf: &mut Surface, area: Rect, focused: bool) {
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

        self.panes.rows = body;
        let height = body.height as usize;
        self.search.clamp_scroll(height);
        if !self.search.has_results() {
            let notice = match (self.search.error(), self.search.is_running()) {
                (Some(error), _) => error,
                (None, true) => SEARCHING,
                (None, false) if self.search.is_stale() => SEARCH_HINT,
                (None, false) => NO_MATCHES,
            };
            placeholder(buf, body, notice, self.styles.dim);
            return;
        }

        let scroll = self.search.scroll();
        let selected = self.search.selected_index();
        let root = self.root.clone();
        let pointed = self.hovered_row(body);
        for (offset, row) in self
            .search
            .rows()
            .iter()
            .skip(scroll)
            .take(height)
            .enumerate()
        {
            let chosen = focused && scroll + offset == selected;
            let line = search_row(&self.search, *row, &root, chosen, &self.styles, body.width);
            let line = emphasize(line, pointed == Some(offset) && !chosen, &self.styles);
            chrome::render_line(buf, line_at(body, offset), line);
        }
    }

    /// Three stacked sections, each with a pinned title row over a list that
    /// scrolls under it. The title stays put so the fold handle and the count
    /// never scroll out of reach.
    fn render_scm(&mut self, buf: &mut Surface, area: Rect, focused: bool) {
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
            let chosen = focused && cursor.section == section && cursor.row.is_none();
            let pointed = self.hovering(rects.header).is_some();
            let line = section_header(
                section,
                self.scm.count(section),
                self.scm.is_collapsed(section),
                chosen,
                &self.styles,
                rects.header.width,
            );
            let line = emphasize(line, pointed && !chosen, &self.styles);
            chrome::render_line(buf, rects.header, line);
            self.render_section(buf, section, rects.body, focused);
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

    fn render_section(&mut self, buf: &mut Surface, section: Section, area: Rect, focused: bool) {
        let height = area.height as usize;
        self.scm.clamp_scroll(section, height);
        if height == 0 {
            return;
        }
        let scroll = self.scm.scroll(section);
        let cursor = self.scm.cursor();
        let pointed = self.hovered_row(area);
        for offset in 0..height.min(self.scm.rows(section).len().saturating_sub(scroll)) {
            let row = self.scm.rows(section)[scroll + offset];
            let chosen =
                focused && cursor.section == section && cursor.row == Some(scroll + offset);
            let line = scm_row(&self.scm, section, row, chosen, &self.styles, area.width);
            let line = emphasize(line, pointed == Some(offset) && !chosen, &self.styles);
            chrome::render_line(buf, line_at(area, offset), line);
        }
    }

    fn render_tree(&mut self, buf: &mut Surface, area: Rect, focused: bool) {
        self.panes.rows = area;
        let height = area.height as usize;
        self.tree.clamp_scroll(height);
        if self.tree.rows().is_empty() {
            placeholder(buf, area, EMPTY_TREE, self.styles.dim);
            return;
        }
        let scroll = self.tree.scroll();
        let selected = self.tree.selected_index();
        let pointed = self.hovered_row(area);
        for (offset, row) in self
            .tree
            .rows()
            .iter()
            .skip(scroll)
            .take(height)
            .enumerate()
        {
            let chosen = focused && scroll + offset == selected;
            let line = tree_row(row, chosen, &self.styles, area.width);
            let line = emphasize(line, pointed == Some(offset) && !chosen, &self.styles);
            chrome::render_line(buf, line_at(area, offset), line);
        }
    }

    fn render_editor(&mut self, buf: &mut Surface, area: Rect) {
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
        if let Some((label, input)) = prompt {
            self.render_prompt(buf, bar, &label, &input);
        }
        self.render_palette(buf, area);
    }

    /// Drawn over the editor rather than beside it, because it is a question
    /// asked of the whole project and answered by replacing what is on screen.
    fn render_palette(&mut self, buf: &mut Surface, area: Rect) {
        if !self.palette.is_open() {
            self.panes.palette = Rect::default();
            return;
        }
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
        self.panes.palette = rows;
        self.palette.clamp_scroll(rows.height as usize);

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
        chrome::render_line(
            buf,
            rule,
            Line::from(Span::styled(
                HORIZONTAL.repeat(rule.width as usize),
                self.styles.border,
            )),
        );
    }

    fn render_tabs(&mut self, buf: &mut Surface, area: Rect) {
        let active = self.editor.active_index();
        let pointed = self
            .hovering(area)
            .and_then(|at| tab_at(&self.editor, at.0, area.x));
        let mut spans = Vec::new();
        for (index, tab) in self.editor.tabs().iter().enumerate() {
            let under = pointed.filter(|hit| hit.index == index);
            let mut style = if index == active {
                self.styles.tab_active
            } else {
                self.styles.tab_inactive
            };
            if under.is_some() && index != active {
                style = style.patch(self.styles.hover);
            }
            let close = match under.is_some_and(|hit| hit.close) {
                true => self.styles.accent.patch(self.styles.hover),
                false => self.styles.dim,
            };
            spans.push(Span::styled(TAB_GAP, self.styles.background));
            if tab.is_dirty() {
                spans.push(Span::styled(DIRTY_MARK, self.styles.accent));
            }
            spans.push(Span::styled(format!("{}{TAB_GAP}", tab.title), style));
            spans.push(Span::styled(CLOSE_MARK, close));
            spans.push(Span::styled(TAB_GAP, self.styles.background));
        }
        chrome::render_line(buf, area, Line::from(spans));
    }

    fn render_body(&mut self, buf: &mut Surface, area: Rect) {
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

        let gutter = digits(tab.buffer.line_count()) + GUTTER_GAP;
        let [numbers, text] =
            Layout::horizontal([Constraint::Length(gutter), Constraint::Min(1)]).areas(area);
        self.panes.text = text;

        let first = tab.scroll();
        let last = (first + text.height as usize).min(tab.buffer.line_count());
        let segments = tab.segments(first, last);
        let h_scroll = tab.h_scroll();
        for line in first..last {
            let offset = line - first;
            chrome::render_line(
                buf,
                line_at(numbers, offset),
                gutter_row(tab, line, &self.styles, focused, gutter),
            );
            chrome::render_line(
                buf,
                line_at(text, offset),
                text_row(
                    tab,
                    line,
                    segments.get(offset).map(Vec::as_slice),
                    &self.styles,
                    focused,
                    (h_scroll, text.width as usize),
                ),
            );
        }
    }

    fn render_prompt(&self, buf: &mut Surface, area: Rect, label: &str, input: &str) {
        let mut left = vec![
            Span::styled(label.to_owned(), self.styles.accent),
            Span::styled(input.to_owned(), self.styles.text),
        ];
        let right = match self.editor.active().map(|tab| &tab.find) {
            Some(find) if find.is_open() && !find.query().is_empty() => match find.position() {
                Some((at, total)) => vec![Span::styled(format!("{at}/{total}"), self.styles.dim)],
                None => vec![Span::styled(NO_MATCHES, self.styles.error)],
            },
            _ => Vec::new(),
        };
        if left[1].content.is_empty() {
            left.pop();
        }
        chrome::render_line(
            buf,
            area,
            chrome::status_line(left, right, area.width, self.styles.dim),
        );
    }

    fn render_status(&mut self, buf: &mut Surface, area: Rect) {
        let half = area.width as usize / 2;
        let left = match (&self.flash, self.editor.active()) {
            (Some(message), _) => vec![Span::styled(chrome::fit(message, half), self.styles.error)],
            (None, Some(tab)) => status_left(tab, self.relative(&tab.path), &self.styles, half),
            (None, None) => vec![Span::styled(
                chrome::fit_end(&self.root.display().to_string(), half),
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
    fn status_hints(&self) -> Vec<(&'static str, &'static str)> {
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
        vec![
            (keys::VIEW_EXPLORER.label, "explorer"),
            (keys::FIND.label, "find"),
            (keys::SEND_TO_COMPOSER.label, "send"),
            (keys::CLOSE.label, "back"),
        ]
    }

    /// The one-line field under the editor, when something is asking for input.
    fn prompt(&self) -> Option<(String, String)> {
        if let Some(input) = &self.goto {
            return Some((GOTO_PROMPT.to_owned(), input.clone()));
        }
        let find = &self.editor.active()?.find;
        find.is_open()
            .then(|| (FIND_PROMPT.to_owned(), find.query().to_owned()))
    }
}

/// Where a click on the tab strip landed. The close mark is its own target, so
/// reaching for it never selects the tab instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TabHit {
    pub(crate) index: usize,
    pub(crate) close: bool,
}

/// One tab's width, which is the only description of how
/// [`Workbench::render_tabs`] lays a tab out.
fn tab_width(tab: &Tab) -> usize {
    TAB_GAP.len() * 3
        + usize::from(tab.is_dirty())
        + tab.title.chars().count()
        + CLOSE_MARK.chars().count()
}

/// Which tab a click at `column` landed on, and whether it landed on that
/// tab's close mark.
pub(crate) fn tab_at(editor: &Editor, column: u16, origin: u16) -> Option<TabHit> {
    let mut left = column.checked_sub(origin)? as usize;
    for (index, tab) in editor.tabs().iter().enumerate() {
        let width = tab_width(tab);
        if left < width {
            let close = left == width - TAB_GAP.len() - CLOSE_MARK.chars().count();
            return Some(TabHit { index, close });
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

/// Whether `column` is on the sidebar header's tree-or-flat button, which sits
/// at the right edge ahead of `context`, the branch name. Measured from the
/// right the same way [`chrome::status_line`] lays that group out.
pub(crate) fn mode_at(column: u16, header: Rect, context: usize) -> bool {
    let trailing = (context + TAB_GAP.len() + TREE_LABEL.len()) as u16;
    let Some(start) = header.right().checked_sub(trailing) else {
        return false;
    };
    (start..start + TREE_LABEL.len() as u16).contains(&column)
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

fn tree_row(row: &TreeRow, selected: bool, styles: &WorkbenchStyles, width: u16) -> Line<'static> {
    let marker = match (row.is_dir(), row.expanded) {
        (true, true) => EXPANDED_MARK,
        (true, false) => COLLAPSED_MARK,
        (false, _) => LEAF_INDENT,
    };
    let style = match (selected, row.is_dir()) {
        (true, _) => styles.selected,
        (false, true) => styles.directory,
        (false, false) => styles.text,
    };

    let mut right = Vec::new();
    if row.agent_touched {
        right.push(Span::styled(AGENT_MARK, styles.agent_touched));
    }
    if let Some(mark) = row.git {
        right.push(Span::styled(mark.letter(), git_style(mark, styles)));
    }
    let reserved = right.len() + usize::from(!right.is_empty());
    let label = format!(
        "{:indent$}{marker}{}",
        "",
        row.name,
        indent = row.depth * DEPTH_INDENT
    );
    let left = vec![Span::styled(
        chrome::fit(&label, (width as usize).saturating_sub(reserved)),
        style,
    )];
    chrome::status_line(left, right, width, styles.background)
}

/// A section's pinned title: the fold marker, the name, and how many paths or
/// commits are behind it.
fn section_header(
    section: Section,
    count: usize,
    collapsed: bool,
    selected: bool,
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
    let count = format!("{count}{COUNT_GAP}");
    let left = vec![Span::styled(
        chrome::fit(&label, (width as usize).saturating_sub(count.width())),
        style,
    )];
    let right = vec![Span::styled(count, styles.dim)];
    chrome::status_line(left, right, width, styles.background)
}

fn scm_row(
    scm: &Scm,
    section: Section,
    row: ScmRow,
    selected: bool,
    styles: &WorkbenchStyles,
    width: u16,
) -> Line<'static> {
    match row {
        ScmRow::Directory(index) => match scm.dir(section, index) {
            Some(dir) => directory_row(dir, selected, styles, width),
            None => Line::default(),
        },
        ScmRow::Change { index, depth } => match scm.change(index) {
            Some(change) => change_row(change, depth, scm.is_flat(), selected, styles, width),
            None => Line::default(),
        },
        ScmRow::Commit(index) => match (scm.commit(index), scm.rail(index)) {
            (Some(commit), Some(rail)) => commit_row(commit, rail, selected, styles, width),
            _ => Line::default(),
        },
    }
}

fn directory_row(dir: &Dir, selected: bool, styles: &WorkbenchStyles, width: u16) -> Line<'static> {
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
    Line::from(Span::styled(chrome::fit(&label, width as usize), style))
        .style(styles.background)
}

/// The path, and its git letter on the right. Tree mode indents and shows only
/// the filename; flat mode keeps the whole path and cuts it from the left,
/// which is the end that names the file.
fn change_row(
    change: &Change,
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
    let right = vec![Span::styled(
        change.mark.letter(),
        git_style(change.mark, styles),
    )];
    let budget = (width as usize).saturating_sub(2);
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
                    &format!("{:indent$}{LEAF_INDENT}{name}", "", indent = depth * DEPTH_INDENT),
                    budget,
                ),
                style,
            )]
        }
    };
    chrome::status_line(left, right, width, styles.background)
}

/// Author on the right, rail, hash and summary on the left, so a narrow sidebar
/// drops the author rather than the line that identifies the commit.
fn commit_row(
    commit: &Commit,
    rail: Rail,
    selected: bool,
    styles: &WorkbenchStyles,
    width: u16,
) -> Line<'static> {
    let style = match selected {
        true => styles.selected,
        false => styles.text,
    };
    let mark = match rail {
        Rail::Trunk => RAIL_TRUNK,
        Rail::Merge => RAIL_MERGE,
        Rail::Side => RAIL_SIDE,
    };
    let budget = (width as usize).saturating_sub(commit.id.len() + mark.width() + 1);
    let left = vec![
        Span::styled(mark, styles.dim),
        Span::styled(commit.id.clone(), styles.accent),
        Span::styled(
            format!("{SUMMARY_GAP}{}", chrome::fit(&commit.summary, budget)),
            style,
        ),
    ];
    let right = vec![Span::styled(commit.author.clone(), styles.dim)];
    chrome::status_line(left, right, width, styles.background)
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
    root: &Path,
    selected: bool,
    styles: &WorkbenchStyles,
    width: u16,
) -> Line<'static> {
    match row {
        SearchRow::File(index) => match search.file(index) {
            Some(path) => {
                let relative = path.strip_prefix(root).unwrap_or(path);
                Line::from(Span::styled(
                    chrome::fit_end(&relative.display().to_string(), width as usize),
                    if selected {
                        styles.selected
                    } else {
                        styles.directory
                    },
                ))
            }
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
    let style = if selected {
        styles.selected
    } else {
        styles.text
    };
    let number = format!("{LEAF_INDENT}{}{SUMMARY_GAP}", hit.line);
    let budget = (width as usize).saturating_sub(number.chars().count());
    let text: Vec<char> = hit.text.chars().collect();
    let (start, end) = (hit.range.0.min(text.len()), hit.range.1.min(text.len()));

    let mut spans = vec![Span::styled(number, styles.gutter)];
    for (slice, painted) in [
        (&text[..start], style),
        (&text[start..end], styles.match_highlight),
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

fn gutter_row(
    tab: &Tab,
    line: usize,
    styles: &WorkbenchStyles,
    focused: bool,
    width: u16,
) -> Line<'static> {
    let style = match focused && tab.buffer.cursor().line == line {
        true => styles.text,
        false => match tab.diff_kinds().is_some() {
            true => styles.diff_line_nr,
            false => styles.gutter,
        },
    };
    let gap = GUTTER_GAP as usize;
    Line::from(Span::styled(
        format!(
            "{:>width$}{:gap$}",
            line + 1,
            "",
            width = width as usize - gap
        ),
        style,
    ))
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
    let base = match tab.diff_kinds().and_then(|kinds| kinds.get(line)) {
        Some(DiffKind::Added) => styles.diff_new,
        Some(DiffKind::Removed) => styles.diff_old,
        Some(DiffKind::Header) => styles.title,
        _ => styles.text,
    };

    let mut overlays = Vec::new();
    let current = tab.find.current();
    for found in tab.find.on_line(line) {
        let style = match Some(*found) == current {
            true => styles.diff_new_emphasis,
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
        overlays: &overlays,
    }
    .paint(window.0, window.1)
}

fn status_left(
    tab: &Tab,
    path: &Path,
    styles: &WorkbenchStyles,
    width: usize,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if tab.is_dirty() {
        spans.push(Span::styled(DIRTY_MARK, styles.accent));
    }
    spans.push(Span::styled(
        chrome::fit_end(&path.display().to_string(), width),
        styles.dim,
    ));
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

    use super::{Editor, SidebarView, Tab, TabHit, Toggle, header_at, tab_at, toggle_at};

    const WRONG_TAB: &str = "the column does not fall on the tab the strip painted there";
    const WRONG_VIEW: &str = "the column does not fall on the view the header painted there";
    const WRONG_TOGGLE: &str = "the column does not fall on the button the row painted there";

    /// Two two-column titles, so every tab spans ` ab \u{d7} ` and the second
    /// starts where the first ended.
    fn editor(dirty: bool) -> Editor {
        let mut editor = Editor::default();
        for title in ["ab", "cd"] {
            let mut tab = Tab::synthetic(
                Path::new(title),
                title.to_owned(),
                vec![String::new()],
                Vec::new(),
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

    #[test_case(0, Some(TabHit { index: 0, close: false }) ; "the gap in front of a tab still selects it")]
    #[test_case(1, Some(TabHit { index: 0, close: false }) ; "the title selects its tab")]
    #[test_case(4, Some(TabHit { index: 0, close: true }) ; "the close mark is its own target")]
    #[test_case(5, Some(TabHit { index: 0, close: false }) ; "the gap after the close mark is not it")]
    #[test_case(6, Some(TabHit { index: 1, close: false }) ; "the next tab starts where the last ended")]
    #[test_case(10, Some(TabHit { index: 1, close: true }) ; "every tab has its own close mark")]
    #[test_case(12, None ; "past the last tab is nothing")]
    fn a_column_falls_on_the_tab_the_strip_painted(column: u16, expected: Option<TabHit>) {
        assert_eq!(tab_at(&editor(false), column, 0), expected, "{WRONG_TAB}");
    }

    #[test]
    fn a_dirty_mark_shifts_the_close_mark_along_with_the_title() {
        assert_eq!(
            tab_at(&editor(true), 5, 0),
            Some(TabHit {
                index: 0,
                close: true
            }),
            "{WRONG_TAB}"
        );
    }

    #[test]
    fn the_origin_is_taken_off_before_the_strip_is_measured() {
        let editor = editor(false);

        assert_eq!(tab_at(&editor, 9, 0), tab_at(&editor, 12, 3), "{WRONG_TAB}");
        assert_eq!(tab_at(&editor, 2, 3), None, "{WRONG_TAB}");
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

    #[test_case(0, None ; "the gap in front of a button is not it")]
    #[test_case(1, Some(Toggle::Case) ; "the first label toggles case")]
    #[test_case(4, Some(Toggle::Word) ; "the second label toggles whole word")]
    #[test_case(7, Some(Toggle::Regex) ; "the third label toggles regex")]
    #[test_case(9, None ; "past the last label is nothing")]
    fn a_column_falls_on_the_button_the_row_painted(column: u16, expected: Option<Toggle>) {
        assert_eq!(toggle_at(column, 0), expected, "{WRONG_TOGGLE}");
    }
}
