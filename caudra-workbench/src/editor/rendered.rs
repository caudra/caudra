//! A Markdown tab read the way the transcript shows Markdown, rather than as
//! the source it is saved as.
//!
//! The workbench has no Markdown renderer of its own, so the host lends it a
//! [`PaintMarkdown`]. Painting a whole document costs far more than drawing a
//! frame, so the painted lines are kept until the text, the width or the theme
//! moves.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::text::Line;

/// Paints Markdown wrapped to a width, the way the host's transcript does.
pub type PaintMarkdown = fn(&str, u16) -> Vec<Line<'static>>;

/// What a set of painted lines was painted from. Any of them moving means the
/// lines no longer show what the tab holds.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Painting {
    pub(crate) revision: u64,
    pub(crate) width: u16,
    pub(crate) theme_generation: u64,
}

/// The rendered view of one tab. It scrolls in painted rows, which line up
/// with no source line, so it keeps its own place rather than the tab's.
#[derive(Default)]
pub struct Rendered {
    painted_from: Option<Painting>,
    lines: Vec<Line<'static>>,
    scroll: usize,
    /// The source line the view was entered on, and how many there were, kept
    /// until the first paint says how many rows that point is a share of.
    entered_at: Option<(usize, usize)>,
}

impl Rendered {
    pub(crate) fn entered_at(line: usize, lines: usize) -> Self {
        Self {
            entered_at: Some((line, lines)),
            ..Self::default()
        }
    }

    /// Paints `text` unless the lines held were painted from the same thing.
    pub(crate) fn paint(
        &mut self,
        painting: Painting,
        text: impl FnOnce() -> String,
        paint: PaintMarkdown,
    ) {
        if self.painted_from.as_ref() != Some(&painting) {
            self.lines = paint(&text(), painting.width);
            self.painted_from = Some(painting);
        }
        if let Some((line, lines)) = self.entered_at.take() {
            self.scroll = rescale(line, lines, self.lines.len());
        }
    }

    pub(crate) fn lines(&self) -> &[Line<'static>] {
        &self.lines
    }

    /// The first row a pane `rows` tall shows. Never so far down that the
    /// pane is left with rows it could have filled.
    pub(crate) fn top(&self, rows: usize) -> usize {
        self.scroll.min(self.lines.len().saturating_sub(rows))
    }

    pub(crate) fn scroll_to(&mut self, row: usize, rows: usize) {
        self.scroll = row;
        self.scroll = self.top(rows);
    }

    pub(crate) fn scroll_by(&mut self, delta: isize, rows: usize) {
        self.scroll_to(self.top(rows).saturating_add_signed(delta), rows);
    }

    /// Scrolls for a motion key and reports whether the key was one. There is
    /// no caret to carry and nothing off to the side, so every motion is a
    /// scroll and a sideways one goes nowhere.
    pub(crate) fn scroll_key(&mut self, key: KeyEvent, rows: usize) -> bool {
        let page = rows.max(1) as isize;
        match key.code {
            KeyCode::Up => self.scroll_by(-1, rows),
            KeyCode::Down => self.scroll_by(1, rows),
            KeyCode::PageUp => self.scroll_by(-page, rows),
            KeyCode::PageDown => self.scroll_by(page, rows),
            KeyCode::Home => self.scroll_to(0, rows),
            KeyCode::End => self.scroll_to(usize::MAX, rows),
            KeyCode::Left | KeyCode::Right => {}
            _ => return false,
        }
        true
    }

    /// The source line at the same point through the document as the view's
    /// place, for a reader going back to the source. A view left before it was
    /// ever painted hands back the line it was entered on.
    pub(crate) fn source_line(&self, lines: usize) -> usize {
        match self.entered_at {
            Some((line, _)) => line,
            None => rescale(self.scroll, self.lines.len(), lines),
        }
    }
}

/// `at` of `from`, taken to the nearest point the same share of `to`.
fn rescale(at: usize, from: usize, to: usize) -> usize {
    match from {
        0 => 0,
        _ => (at * to + from / 2) / from,
    }
}

#[cfg(test)]
mod tests {
    use super::{Line, Painting, Rendered};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::iter::repeat_n;
    use test_case::test_case;

    const FIRST: &str = "first ";
    const SECOND: &str = "second ";
    const SOURCE: &str = "one\ntwo\nthree";
    const REVISION: u64 = 3;
    const WIDTH: u16 = 40;
    const THEME: u64 = 7;
    const ROWS: usize = 4;
    const SOURCE_LINES: usize = 50;
    const ENTERED_ON: usize = 20;
    const ROWS_PER_LINE: usize = 2;
    const STALE_PAINT: &str = "the view is showing lines painted from something that has moved";
    const WASTED_PAINT: &str = "the view repainted lines nothing had changed underneath";
    const OFF_THE_END: &str = "the view scrolled past the rows that fill the pane";
    const PLACE_LOST: &str = "the view did not keep the reader's place through the document";

    fn first(text: &str, _width: u16) -> Vec<Line<'static>> {
        text.lines()
            .map(|line| Line::from(format!("{FIRST}{line}")))
            .collect()
    }

    fn second(text: &str, _width: u16) -> Vec<Line<'static>> {
        text.lines()
            .map(|line| Line::from(format!("{SECOND}{line}")))
            .collect()
    }

    /// Several rows for every source line, so a place in one is plainly not
    /// the same number in the other.
    fn stretched(text: &str, _width: u16) -> Vec<Line<'static>> {
        text.lines()
            .flat_map(|line| repeat_n(Line::from(line.to_owned()), ROWS_PER_LINE))
            .collect()
    }

    fn painting(revision: u64, width: u16, theme_generation: u64) -> Painting {
        Painting {
            revision,
            width,
            theme_generation,
        }
    }

    fn source(lines: usize) -> String {
        (0..lines)
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn opening(view: &Rendered) -> String {
        view.lines()[0].to_string()
    }

    #[test_case(painting(REVISION, WIDTH, THEME), false ; "nothing moved")]
    #[test_case(painting(REVISION + 1, WIDTH, THEME), true ; "the text changed")]
    #[test_case(painting(REVISION, WIDTH - 1, THEME), true ; "the pane narrowed")]
    #[test_case(painting(REVISION, WIDTH, THEME + 1), true ; "the theme changed")]
    fn a_paint_is_kept_until_what_it_was_painted_from_moves(next: Painting, repainted: bool) {
        let mut view = Rendered::default();
        view.paint(
            painting(REVISION, WIDTH, THEME),
            || SOURCE.to_owned(),
            first,
        );

        view.paint(next, || SOURCE.to_owned(), second);

        match repainted {
            true => assert!(opening(&view).starts_with(SECOND), "{STALE_PAINT}"),
            false => assert!(opening(&view).starts_with(FIRST), "{WASTED_PAINT}"),
        }
    }

    #[test_case(KeyCode::End, SOURCE_LINES - ROWS ; "end stops at the last full pane")]
    #[test_case(KeyCode::PageDown, ROWS ; "a page is a pane")]
    #[test_case(KeyCode::Down, 1 ; "an arrow is a row")]
    #[test_case(KeyCode::Right, 0 ; "sideways goes nowhere")]
    fn a_motion_scrolls_within_the_rows_there_are(code: KeyCode, expected: usize) {
        let mut view = Rendered::default();
        view.paint(
            painting(REVISION, WIDTH, THEME),
            || source(SOURCE_LINES),
            first,
        );

        assert!(view.scroll_key(KeyEvent::new(code, KeyModifiers::NONE), ROWS));

        assert_eq!(view.top(ROWS), expected, "{OFF_THE_END}");
    }

    #[test]
    fn scrolling_past_the_end_stays_on_the_last_full_pane() {
        let mut view = Rendered::default();
        view.paint(
            painting(REVISION, WIDTH, THEME),
            || source(SOURCE_LINES),
            first,
        );

        view.scroll_by(isize::MAX, ROWS);
        view.scroll_by(-1, ROWS);

        assert_eq!(view.top(ROWS), SOURCE_LINES - ROWS - 1, "{OFF_THE_END}");
    }

    #[test]
    fn going_back_to_the_source_lands_where_the_view_was_entered() {
        let mut view = Rendered::entered_at(ENTERED_ON, SOURCE_LINES);
        view.paint(
            painting(REVISION, WIDTH, THEME),
            || source(SOURCE_LINES),
            stretched,
        );

        assert_eq!(view.top(ROWS), ENTERED_ON * ROWS_PER_LINE, "{PLACE_LOST}");
        assert_eq!(view.source_line(SOURCE_LINES), ENTERED_ON, "{PLACE_LOST}");
    }

    #[test]
    fn a_view_never_painted_goes_back_to_the_line_it_was_entered_on() {
        let view = Rendered::entered_at(ENTERED_ON, SOURCE_LINES);

        assert_eq!(view.source_line(SOURCE_LINES), ENTERED_ON, "{PLACE_LOST}");
    }
}
