//! The fuzzy list a composer sigil drops in front of the reader.
//!
//! `@` completes paths and `#` completes commits, but neither of those facts
//! belongs here. What is shared is everything around the rows: the matcher and
//! its query, which row is selected, how far the viewport has scrolled, where
//! the rows were last drawn, and how a press and a release become a choice.
//! Each popup supplies its own source of rows and its own way of painting one.

use std::ops::Range;

use caudra_grab::grab_scope;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use nucleo::pattern::{CaseMatching, Normalization};
use nucleo::{Config, Injector, Nucleo};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::widgets::{Clear, Paragraph, Widget};
use std::sync::Arc;

use crate::repaint::Dirty;
use crate::theme;

const MAX_ROWS: usize = 10;
/// Matching a whole project on every keystroke is wasted work when the reader
/// can only see ten rows; nucleo is asked for a little more so scrolling has
/// somewhere to go.
const MAX_MATCHES: usize = 64;
pub(crate) const PAD: u16 = 1;
/// How long a settling tick waits on the matcher before looking again.
#[cfg(test)]
const POLL_MS: u64 = 10;

/// What a mouse event did to the list, for a caller that still has to decide
/// what taking a row means.
pub(crate) enum MouseOutcome {
    /// The pointer was not over the rows; the caller should pass the event on.
    Outside,
    Consumed,
    /// A press and a release landed on the same row, which is now selected.
    Chosen,
}

pub(crate) struct Completion {
    nucleo: Nucleo<()>,
    matches: Vec<String>,
    selected: usize,
    scroll_offset: usize,
    query: String,
    /// Where the rows were last drawn, so the pointer can find them. Cleared on
    /// a frame that draws nothing, because a stale rectangle would keep
    /// answering clicks for rows that are no longer on screen.
    area: Rect,
    /// The row the button went down on. A release only takes a row when it is
    /// the row the press started on.
    pressed: Option<String>,
}

impl Completion {
    pub fn new(config: Config, query: String) -> Self {
        Self {
            nucleo: Nucleo::new(config, Arc::new(|| {}), None, 1),
            matches: Vec::new(),
            selected: 0,
            scroll_offset: 0,
            query,
            area: Rect::default(),
            pressed: None,
        }
    }

    pub fn injector(&self) -> Injector<()> {
        self.nucleo.injector()
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    /// Re-runs the matcher against a new query, returning to the top of the
    /// list: the row that was selected answered a question nobody asked again.
    pub fn set_query(&mut self, query: String) {
        self.query = query;
        self.nucleo.pattern.reparse(
            0,
            &self.query,
            CaseMatching::Smart,
            Normalization::Smart,
            false,
        );
        self.selected = 0;
        self.scroll_offset = 0;
        self.nucleo.tick(0);
        self.refresh();
    }

    pub fn is_empty(&self) -> bool {
        self.matches.is_empty()
    }

    pub fn selected(&self) -> Option<&str> {
        self.matches.get(self.selected).map(String::as_str)
    }

    #[cfg(test)]
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// Stands in for the layout a frame would have done, so a test can drive
    /// the pointer without rendering.
    #[cfg(test)]
    pub fn set_area(&mut self, area: Rect) {
        self.area = area;
    }

    pub fn contains(&self, position: Position) -> bool {
        self.area.contains(position)
    }

    /// Collects whatever the source has produced so far. A popup fed by a
    /// background walk grows over the first few frames after opening.
    pub fn tick(&mut self) -> Dirty {
        let before = self.matches.len();
        self.nucleo.tick(0);
        self.refresh();
        match self.matches.len() == before {
            true => Dirty::NO,
            false => Dirty::YES,
        }
    }

    /// Waits for the matcher to drain, so a test can assert on what the popup
    /// found without racing whatever is feeding it.
    #[cfg(test)]
    pub fn settle(&mut self) {
        while self.nucleo.tick(POLL_MS).running {}
        self.refresh();
    }

    pub fn step(&mut self, delta: isize) {
        if self.matches.is_empty() {
            return;
        }
        let len = self.matches.len() as isize;
        self.selected = (self.selected as isize + delta).rem_euclid(len) as usize;
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> MouseOutcome {
        let position = Position::new(event.column, event.row);
        if !self.area.contains(position) {
            self.pressed = None;
            return MouseOutcome::Outside;
        }
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.pressed = None;
                if let Some(index) = self.row_at(position) {
                    self.selected = index;
                    self.pressed = Some(self.matches[index].clone());
                }
                MouseOutcome::Consumed
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.pressed = None;
                MouseOutcome::Consumed
            }
            MouseEventKind::Moved => {
                if let Some(index) = self.row_at(position) {
                    self.selected = index;
                }
                MouseOutcome::Consumed
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let pressed = self.pressed.take();
                let landed = self.row_at(position);
                let Some(index) =
                    landed.filter(|&index| pressed.as_deref() == Some(&self.matches[index]))
                else {
                    return MouseOutcome::Consumed;
                };
                self.selected = index;
                MouseOutcome::Chosen
            }
            _ => MouseOutcome::Consumed,
        }
    }

    /// Draws the list above `input_area` and reports where it landed.
    ///
    /// `row` turns one match and whether it is selected into the line drawn for
    /// it; `width` measures a match in cells, since a row may be painted wider
    /// than the string it came from.
    ///
    /// `scope` names the component for the layout grabber. Every other caller of
    /// `grab_scope!` passes a literal, which no longer exists in release; this
    /// one is a parameter, so it survives as a binding with nothing left to read
    /// it. The attribute is the same one `caudra_grab` puts on the functions the
    /// macro wraps.
    #[cfg_attr(not(debug_assertions), allow(unused_variables))]
    pub fn view(
        &mut self,
        frame: &mut Frame,
        input_area: Rect,
        scope: &'static str,
        width: impl Fn(&str) -> u16,
        row: impl Fn(&str, bool) -> Line<'static>,
    ) -> Option<Rect> {
        self.area = Rect::default();
        if self.matches.is_empty() {
            return None;
        }
        let height = (self.matches.len().min(MAX_ROWS) as u16).min(input_area.y);
        if height == 0 {
            return None;
        }
        self.scroll_offset = self
            .scroll_offset
            .min(self.selected)
            .max((self.selected + 1).saturating_sub(height as usize));

        let area = Rect {
            x: input_area.x,
            y: input_area.y.saturating_sub(height),
            width: self
                .matches
                .iter()
                .map(|item| width(item))
                .max()
                .unwrap_or(0)
                .min(input_area.width),
            height,
        };
        self.area = area;

        grab_scope!(scope, area);
        let lines: Vec<Line> = self
            .matches
            .iter()
            .enumerate()
            .skip(self.scroll_offset)
            .take(height as usize)
            .map(|(index, item)| row(item, index == self.selected))
            .collect();

        Clear.render(area, frame.buffer_mut());
        frame.render_widget(
            Paragraph::new(lines).style(theme::current().item.add_modifier(Modifier::empty())),
            area,
        );
        Some(area)
    }

    fn refresh(&mut self) {
        let snapshot = self.nucleo.snapshot();
        let count = snapshot.matched_item_count().min(MAX_MATCHES as u32);
        self.matches = snapshot
            .matched_items(0..count)
            .map(|item| item.matcher_columns[0].to_string())
            .collect();
        self.selected = self.selected.min(self.matches.len().saturating_sub(1));
    }

    /// Which match the pointer is over. Rows are one line tall and drawn from
    /// `scroll_offset`, so the row is arithmetic rather than a stored hit list.
    /// A frame that draws fewer rows than the area is tall leaves blank rows,
    /// which belong to no match.
    fn row_at(&self, position: Position) -> Option<usize> {
        let offset = position.y.checked_sub(self.area.y)? as usize;
        let index = self.scroll_offset + offset;
        (index < self.matches.len()).then_some(index)
    }
}

/// The `<sigil>query` under `cursor`, if there is one. The sigil has to start a
/// word and the query has to reach the cursor without whitespace, which is what
/// stops an earlier token on the line from reopening the popup.
pub(crate) fn trigger_at(text: &str, cursor: usize, sigil: char) -> Option<(Range<usize>, String)> {
    let chars: Vec<char> = text.chars().collect();
    if cursor > chars.len() {
        return None;
    }
    let start = chars[..cursor].iter().rposition(|&c| c == sigil)?;
    if start > 0 && !chars[start - 1].is_whitespace() {
        return None;
    }
    let query: String = chars[start + 1..cursor].iter().collect();
    match query.chars().any(char::is_whitespace) {
        true => None,
        false => Some((start..cursor, query)),
    }
}

/// A list seeded with `rows` and settled, for tests that care about selection
/// and geometry rather than about whatever normally feeds the matcher.
#[cfg(test)]
pub(crate) fn seeded(rows: &[&str], area: Rect) -> Completion {
    let mut completion = Completion::new(Config::DEFAULT, String::new());
    let injector = completion.injector();
    for row in rows {
        let row = (*row).to_owned();
        injector.push((), |_, columns| {
            columns[0] = nucleo::Utf32String::from(row.as_str());
        });
    }
    completion.settle();
    completion.set_area(area);
    completion
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use test_case::test_case;

    const ROWS: [&str; 3] = ["one", "two", "three"];
    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 10,
        height: 3,
    };
    const EXPECT_WRAPPED: &str = "stepping past an end wraps to the other";

    fn mouse(kind: MouseEventKind, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column: 0,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test_case(1, "two" ; "down_one")]
    #[test_case(-1, "three" ; "up_from_the_top_wraps")]
    #[test_case(3, "one" ; "a_full_turn_returns")]
    fn stepping_walks_the_list_and_wraps(delta: isize, expected: &str) {
        let mut completion = seeded(&ROWS, AREA);
        completion.step(delta);
        assert_eq!(completion.selected(), Some(expected), "{EXPECT_WRAPPED}");
    }

    #[test]
    fn stepping_an_empty_list_selects_nothing() {
        let mut completion = seeded(&[], AREA);
        completion.step(1);
        assert_eq!(completion.selected(), None);
    }

    #[test]
    fn a_press_and_release_on_one_row_takes_it() {
        let mut completion = seeded(&ROWS, AREA);
        completion.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1));
        let outcome = completion.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 1));
        assert!(matches!(outcome, MouseOutcome::Chosen));
        assert_eq!(completion.selected(), Some("two"));
    }

    /// A release somewhere other than the press is a slip, not a choice.
    #[test]
    fn a_release_on_a_different_row_takes_nothing() {
        let mut completion = seeded(&ROWS, AREA);
        completion.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 0));
        let outcome = completion.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 2));
        assert!(matches!(outcome, MouseOutcome::Consumed));
    }

    #[test]
    fn a_pointer_outside_the_rows_is_left_for_the_composer() {
        let mut completion = seeded(&ROWS, AREA);
        let outcome = completion.handle_mouse(mouse(MouseEventKind::Moved, AREA.height + 1));
        assert!(matches!(outcome, MouseOutcome::Outside));
    }

    #[test]
    fn moving_over_a_row_marks_it() {
        let mut completion = seeded(&ROWS, AREA);
        completion.handle_mouse(mouse(MouseEventKind::Moved, 2));
        assert_eq!(completion.selected(), Some("three"));
    }

    #[test_case("see @src", 8, '@', Some("src") ; "at_the_cursor")]
    #[test_case("see @src", 5, '@', Some("") ; "just_after_the_sigil")]
    #[test_case("mail@host", 9, '@', None ; "mid_word_sigil")]
    #[test_case("see @src more", 13, '@', None ; "query_crossed_whitespace")]
    #[test_case("see #a1b2", 9, '#', Some("a1b2") ; "hash_sigil")]
    #[test_case("nothing", 7, '@', None ; "no_sigil")]
    fn a_trigger_is_the_run_between_a_leading_sigil_and_the_cursor(
        text: &str,
        cursor: usize,
        sigil: char,
        expected: Option<&str>,
    ) {
        let found = trigger_at(text, cursor, sigil);
        assert_eq!(found.as_ref().map(|(_, query)| query.as_str()), expected);
    }

    #[test]
    fn a_new_query_returns_to_the_top_of_the_list() {
        let mut completion = seeded(&ROWS, AREA);
        completion.step(2);
        completion.set_query(String::new());
        assert_eq!(completion.selected(), Some("one"));
    }
}
