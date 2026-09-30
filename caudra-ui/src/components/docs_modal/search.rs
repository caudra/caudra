//! The search view: a query that every edit runs against every page, and the
//! sections holding all of its terms, best first.

use std::ops::Range;

use caudra_docs::{Hit, Library, Search, Target};
use caudra_workbench::text_field::{FieldKind, TextField};
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::{PaneContext, Pick, clicked, crumb, field_pick, list_move, prompt_line};
use crate::components::{ModalScroll, hover_style, match_spans};
use crate::theme::Theme;

const SEARCH_LIMIT: usize = 100;
/// Each result takes a row for where it is and a row for its snippet.
pub(super) const HIT_ROWS: u16 = 2;
pub(super) const SNIPPET_INDENT: &str = "  ";
const QUERY_ROWS: u16 = 1;
const STATUS_ROWS: u16 = 1;
const STATUS_GAP: &str = " · ";
const CORRECTED: &str = " → ";
const PROMPT: &str = "Type to search every page";
const NO_HITS: &str = "No section contains every term";
const ONE_HIT: &str = "1 section";
const SECTIONS: &str = " sections";
const SHOWN_OF: &str = " of ";

pub(super) struct SearchView {
    query: TextField,
    found: Search,
    selected: usize,
    scroll: ModalScroll,
    results: Rect,
    pressed: Option<usize>,
}

impl Default for SearchView {
    fn default() -> Self {
        Self {
            query: TextField::new(FieldKind::Line),
            found: Search::default(),
            selected: 0,
            scroll: ModalScroll::new_top(),
            results: Rect::default(),
            pressed: None,
        }
    }
}

impl SearchView {
    pub(super) fn set_query(&mut self, query: &str, library: &Library) {
        self.query.set_text(query);
        self.run(library);
    }

    pub(super) fn paste(&mut self, text: &str, library: &Library) {
        if self.query.paste(text).changed() {
            self.run(library);
        }
    }

    pub(super) fn selected_text(&self) -> Option<String> {
        self.query.selected_text()
    }

    pub(super) fn scroll(&mut self, delta: i32) {
        self.scroll.scroll(delta);
    }

    /// What to highlight on the page a result opens: every term as typed, and
    /// every word a misspelt one was taken for.
    pub(super) fn terms(&self) -> Vec<String> {
        let mut terms: Vec<String> = self
            .query
            .text()
            .split_whitespace()
            .map(str::to_ascii_lowercase)
            .chain(
                self.found
                    .corrections
                    .iter()
                    .map(|correction| correction.used.clone()),
            )
            .collect();
        terms.sort_unstable();
        terms.dedup();
        terms
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent, library: &Library) -> Pick {
        let page = usize::from(self.results.height / HIT_ROWS / 2);
        if let Some(selected) = list_move(key, self.selected, self.found.hits.len(), page) {
            self.selected = selected;
            self.scroll.reveal_and_hold(first_row(selected), HIT_ROWS);
            return Pick::Stay;
        }
        match key.code {
            KeyCode::Enter => self
                .found
                .hits
                .get(self.selected)
                .map_or(Pick::Stay, |hit| Pick::Open(target(hit))),
            KeyCode::Esc => Pick::Leave,
            _ => {
                let edit = self.query.handle_key(key);
                if edit.changed() {
                    self.run(library);
                }
                field_pick(edit)
            }
        }
    }

    /// The result a press and release on the same result opened.
    pub(super) fn handle_mouse(&mut self, event: MouseEvent) -> Option<Target> {
        let hit = self.hit_at(Position::new(event.column, event.row));
        let hit = clicked(&mut self.pressed, event.kind, hit)?;
        self.found.hits.get(hit).map(target)
    }

    pub(super) fn draw(&mut self, frame: &mut Frame, area: Rect, context: &PaneContext) {
        let theme = context.theme;
        let [query, status, results] = Layout::vertical([
            Constraint::Length(QUERY_ROWS),
            Constraint::Length(STATUS_ROWS),
            Constraint::Fill(1),
        ])
        .areas(area);
        frame.render_widget(Paragraph::new(prompt_line(&self.query, query.width)), query);
        frame.render_widget(Paragraph::new(self.status(theme)), status);
        let rows = self.found.hits.len().saturating_mul(usize::from(HIT_ROWS));
        self.scroll
            .update_dimensions(u16::try_from(rows).unwrap_or(u16::MAX), results.height);
        self.results = results;
        let hovered = context.pointer.and_then(|at| self.hit_at(at));
        let offset = usize::from(self.scroll.offset());
        let per_hit = usize::from(HIT_ROWS);
        let lines: Vec<Line<'static>> = self
            .found
            .hits
            .iter()
            .enumerate()
            .skip(offset / per_hit)
            .flat_map(|(index, hit)| {
                let (selected, hovered) = (index == self.selected, hovered == Some(index));
                hit_lines(context.library, hit, selected, hovered, theme)
            })
            .skip(offset % per_hit)
            .take(usize::from(results.height))
            .collect();
        frame.render_widget(Paragraph::new(lines), results);
    }

    /// The result drawn at `position`, whether or not there is one. Both of a
    /// result's rows are it.
    fn hit_at(&self, position: Position) -> Option<usize> {
        self.results.contains(position).then(|| {
            let row = self
                .scroll
                .offset()
                .saturating_add(position.y - self.results.y);
            usize::from(row / HIT_ROWS)
        })
    }

    fn run(&mut self, library: &Library) {
        let query = self.query.text();
        self.found = match query.trim().is_empty() {
            true => Search::default(),
            false => library.search(&query, SEARCH_LIMIT),
        };
        self.selected = 0;
        self.scroll.scroll_to(0);
    }

    fn status(&self, theme: &Theme) -> Line<'static> {
        if self.query.text().trim().is_empty() {
            return Line::from(Span::styled(PROMPT, theme.tool_dim));
        }
        let mut parts: Vec<String> = self
            .found
            .corrections
            .iter()
            .map(|correction| format!("{}{CORRECTED}{}", correction.typed, correction.used))
            .collect();
        parts.push(match (self.found.hits.len(), self.found.matched) {
            (0, _) => NO_HITS.to_owned(),
            (1, 1) => ONE_HIT.to_owned(),
            (shown, matched) if shown < matched => format!("{shown}{SHOWN_OF}{matched}{SECTIONS}"),
            (shown, _) => format!("{shown}{SECTIONS}"),
        });
        Line::from(Span::styled(parts.join(STATUS_GAP), theme.tool_dim))
    }

    #[cfg(test)]
    pub(super) fn query(&self) -> String {
        self.query.text()
    }

    #[cfg(test)]
    pub(super) fn results(&self) -> Rect {
        self.results
    }
}

fn first_row(hit: usize) -> u16 {
    u16::try_from(hit.saturating_mul(usize::from(HIT_ROWS))).unwrap_or(u16::MAX)
}

fn target(hit: &Hit) -> Target {
    Target {
        page: hit.page,
        heading: Some(hit.heading),
    }
}

/// Where a result is, `Page › Section`, over its snippet with the terms marked.
/// The pointer marks both rows, though not the snippet's indent.
fn hit_lines(
    library: &Library,
    hit: &Hit,
    selected: bool,
    hovered: bool,
    theme: &Theme,
) -> [Line<'static>; 2] {
    let (base, dim, matched) = match selected {
        true => (
            theme.item_selected,
            theme.item_selected,
            theme.item_match_selected,
        ),
        false => (theme.item, theme.tool_dim, theme.item_match),
    };
    let indices = char_indices(&hit.snippet, &hit.highlights);
    let mut snippet = vec![Span::styled(SNIPPET_INDENT, dim)];
    snippet.extend(match_spans(
        &hit.snippet,
        &indices,
        hover_style(dim, hovered),
        hover_style(matched, hovered),
        None,
    ));
    let place = Span::styled(crumb(library, target(hit)), hover_style(base, hovered));
    [Line::from(place), Line::from(snippet)]
}

/// The characters of `text` that `ranges`, byte ranges into it, cover.
fn char_indices(text: &str, ranges: &[Range<usize>]) -> Vec<u32> {
    text.char_indices()
        .enumerate()
        .filter(|(_, (byte, _))| ranges.iter().any(|range| range.contains(byte)))
        .filter_map(|(index, _)| u32::try_from(index).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `é` is two bytes and one character, so everything after it sits a byte
    /// further on than its character position.
    const ACCENTED: &str = "é timeout";
    const TERM_BYTES: Range<usize> = 3..10;
    const TERM_CHARS: [u32; 7] = [2, 3, 4, 5, 6, 7, 8];

    #[test]
    fn highlights_become_character_positions() {
        assert_eq!(char_indices(ACCENTED, &[TERM_BYTES]), TERM_CHARS);
    }
}
