//! The contents list: every page under its group, and beneath the page being
//! read its sections. One widget whether it is docked beside the reader or
//! fills the body, and typing narrows it fuzzily.

use std::mem;

use caudra_docs::{Library, Target};
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::{Current, PaneContext, Pick, SECTION_LEVEL, clicked, list_move};
use crate::components::{ModalScroll, hover_style, input_line_with_cursor, match_spans};
use crate::text_buffer::{EditResult, TextBuffer};
use crate::theme::Theme;

const MARK: &str = "▸ ";
const NO_MARK: &str = "  ";
const SECTION_INDENT: &str = "  ";
const FILTER_ROWS: u16 = 1;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Entry {
    /// The group of the page at this index, which the rows below belong to.
    Group(usize),
    Page(usize),
    Section {
        page: usize,
        heading: usize,
    },
}

impl Entry {
    fn target(self) -> Option<Target> {
        match self {
            Self::Group(_) => None,
            Self::Page(page) => Some(Target {
                page,
                heading: None,
            }),
            Self::Section { page, heading } => Some(Target {
                page,
                heading: Some(heading),
            }),
        }
    }
}

struct Row {
    entry: Entry,
    /// Characters of the title the filter matched.
    indices: Vec<u32>,
}

pub(super) struct Contents {
    filter: TextBuffer,
    /// Among the rows that open something, which group headers do not.
    selected: usize,
    /// The selection moved since the list was last drawn. The draw brings it
    /// on screen, once it knows how tall the list is: a hidden list has no
    /// height to scroll by.
    reveal: bool,
    scroll: ModalScroll,
    matcher: Matcher,
    list: Rect,
    pressed: Option<usize>,
}

impl Default for Contents {
    fn default() -> Self {
        Self {
            filter: TextBuffer::new(String::new()),
            selected: 0,
            reveal: false,
            scroll: ModalScroll::new_top(),
            matcher: Matcher::new(Config::DEFAULT),
            list: Rect::default(),
            pressed: None,
        }
    }
}

impl Contents {
    pub(super) fn scroll(&mut self, delta: i32) {
        self.scroll.scroll(delta);
    }

    pub(super) fn clear(&mut self) {
        self.filter.clear();
        self.selected = 0;
    }

    pub(super) fn paste(&mut self, text: &str) {
        self.filter.insert_text(text);
        self.selected = 0;
    }

    /// Selects where the reader is: its section, or else its page.
    pub(super) fn focus(&mut self, library: &Library, current: Current) {
        let rows = self.rows(library, current.page);
        let here = match (current.page, current.section) {
            (Some(page), Some(heading)) => Some(Entry::Section { page, heading }),
            (Some(page), None) => Some(Entry::Page(page)),
            (None, _) => None,
        };
        if let Some(index) = selectable(&rows).position(|row| Some(row.entry) == here) {
            self.selected = index;
        }
        self.reveal = true;
    }

    pub(super) fn handle_key(
        &mut self,
        key: KeyEvent,
        library: &Library,
        current: Current,
    ) -> Pick {
        let rows = self.rows(library, current.page);
        let page = usize::from(self.list.height / 2);
        if let Some(selected) = list_move(key, self.selected, selectable(&rows).count(), page) {
            self.selected = selected;
            self.reveal = true;
            return Pick::Stay;
        }
        match key.code {
            KeyCode::Enter => selectable(&rows)
                .nth(self.selected)
                .and_then(|row| row.entry.target())
                .map_or(Pick::Stay, Pick::Open),
            KeyCode::Esc => {
                self.clear();
                Pick::Leave
            }
            KeyCode::Tab | KeyCode::BackTab => Pick::Leave,
            _ => {
                if self.filter.handle_key(key) == EditResult::Changed {
                    self.selected = 0;
                    self.scroll.scroll_to(0);
                }
                Pick::Stay
            }
        }
    }

    /// The page or section a press and release on the same row opened.
    pub(super) fn handle_mouse(
        &mut self,
        event: MouseEvent,
        library: &Library,
        current: Current,
    ) -> Option<Target> {
        let row = self.row_at(Position::new(event.column, event.row));
        let row = clicked(&mut self.pressed, event.kind, row)?;
        self.rows(library, current.page).get(row)?.entry.target()
    }

    /// Draws the list into `area`, under the filter while it has focus or
    /// holds text.
    pub(super) fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        context: &PaneContext,
        current: Current,
        focused: bool,
    ) {
        let (library, theme) = (context.library, context.theme);
        self.list = area;
        if area.is_empty() {
            return;
        }
        let filter = self.filter.value();
        if focused || !filter.is_empty() {
            let line = match focused {
                true => input_line_with_cursor(&self.filter),
                false => Line::from(Span::styled(filter, theme.tool_dim)),
            };
            let filter_area = Rect {
                height: FILTER_ROWS,
                ..area
            };
            frame.render_widget(Paragraph::new(line), filter_area);
            self.list = Rect {
                y: area.y.saturating_add(FILTER_ROWS),
                height: area.height.saturating_sub(FILTER_ROWS),
                ..area
            };
        }
        let rows = self.rows(library, current.page);
        self.scroll.update_dimensions(
            u16::try_from(rows.len()).unwrap_or(u16::MAX),
            self.list.height,
        );
        if mem::take(&mut self.reveal)
            && let Some(row) = selected_row(&rows, self.selected)
        {
            self.scroll
                .reveal_and_hold(u16::try_from(row).unwrap_or(u16::MAX), 1);
        }
        let selected = focused
            .then(|| selected_row(&rows, self.selected))
            .flatten();
        let hovered = context.pointer.and_then(|at| self.row_at(at));
        let lines: Vec<Line<'static>> = rows
            .iter()
            .enumerate()
            .skip(usize::from(self.scroll.offset()))
            .take(usize::from(self.list.height))
            .map(|(index, row)| {
                let (selected, hovered) = (selected == Some(index), hovered == Some(index));
                row_line(library, row, &current, selected, hovered, theme)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), self.list);
    }

    /// The row of the list drawn at `position`, whether or not it holds one.
    fn row_at(&self, position: Position) -> Option<usize> {
        self.list
            .contains(position)
            .then(|| usize::from(self.scroll.offset()) + usize::from(position.y - self.list.y))
    }

    /// Every page whose title the filter matches, or that has a section it
    /// matches, each under its group. Sections are listed for the page being
    /// read alone.
    fn rows(&mut self, library: &Library, current: Option<usize>) -> Vec<Row> {
        let pattern = Pattern::parse(
            &self.filter.value(),
            CaseMatching::Ignore,
            Normalization::Smart,
        );
        let mut buffer = Vec::new();
        let mut matched = |text: &str| {
            let mut indices = Vec::new();
            pattern.indices(
                Utf32Str::new(text, &mut buffer),
                &mut self.matcher,
                &mut indices,
            )?;
            indices.sort_unstable();
            indices.dedup();
            Some(indices)
        };
        let mut rows = Vec::new();
        let mut group = None;
        for (index, page) in library.pages().iter().enumerate() {
            let sections: Vec<Row> = page
                .headings
                .iter()
                .enumerate()
                .filter(|(_, heading)| current == Some(index) && heading.level == SECTION_LEVEL)
                .filter_map(|(position, heading)| {
                    Some(Row {
                        entry: Entry::Section {
                            page: index,
                            heading: position,
                        },
                        indices: matched(&heading.title)?,
                    })
                })
                .collect();
            let title = matched(&page.title);
            if title.is_none() && sections.is_empty() {
                continue;
            }
            if group != Some(page.group.as_str()) {
                group = Some(page.group.as_str());
                rows.push(Row {
                    entry: Entry::Group(index),
                    indices: Vec::new(),
                });
            }
            rows.push(Row {
                entry: Entry::Page(index),
                indices: title.unwrap_or_default(),
            });
            rows.extend(sections);
        }
        rows
    }
}

fn selectable(rows: &[Row]) -> impl Iterator<Item = &Row> {
    rows.iter().filter(|row| row.entry.target().is_some())
}

fn selected_row(rows: &[Row], selected: usize) -> Option<usize> {
    rows.iter()
        .enumerate()
        .filter(|(_, row)| row.entry.target().is_some())
        .nth(selected)
        .map(|(index, _)| index)
}

/// A group header, or an entry whose title the pointer marks: a press anywhere
/// on the row opens it, but the indent and the mark are not what it names.
fn row_line(
    library: &Library,
    row: &Row,
    current: &Current,
    selected: bool,
    hovered: bool,
    theme: &Theme,
) -> Line<'static> {
    let pages = library.pages();
    let (indent, title, here) = match row.entry {
        Entry::Group(page) => {
            return Line::from(Span::styled(
                pages[page].group.clone(),
                theme.keybind_section,
            ));
        }
        Entry::Page(page) => ("", &pages[page].title, current.page == Some(page)),
        Entry::Section { page, heading } => (
            SECTION_INDENT,
            &pages[page].headings[heading].title,
            current.section == Some(heading),
        ),
    };
    let (base, matched) = match selected {
        true => (theme.item_selected, theme.item_match_selected),
        false => (theme.item, theme.item_match),
    };
    let (mark, mark_style) = match here {
        true => (MARK, theme.accent),
        false => (NO_MARK, base),
    };
    let mut spans = vec![Span::styled(indent, base), Span::styled(mark, mark_style)];
    spans.extend(match_spans(
        title,
        &row.indices,
        hover_style(base, hovered),
        hover_style(matched, hovered),
        None,
    ));
    Line::from(spans)
}
