//! A pointer sweep over painted lines, held in rows of those lines and
//! characters within a row rather than in screen cells, so it outlives a
//! scroll and copies the text the lines hold rather than the cells around
//! them.
//!
//! The lines are the ones handed to a wrapping `Paragraph`: one logical line
//! may take several rows on screen, and a sweep across a wrap copies no break.

use std::iter;
use std::ops::Range;

use ratatui::layout::{Position, Rect};
use ratatui::text::Line;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::components::input::apply_selection;
use crate::selection::{line_chars, wrap_breaks};

/// The two ends the pointer gave, as `(line, char)`. Held unordered, so a
/// backwards drag keeps tracking the end that is moving.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Sweep {
    anchor: (usize, usize),
    cursor: (usize, usize),
}

impl Sweep {
    pub(crate) fn at(position: (usize, usize)) -> Self {
        Self {
            anchor: position,
            cursor: position,
        }
    }

    pub(crate) fn drag_to(&mut self, position: (usize, usize)) {
        self.cursor = position;
    }

    pub(crate) fn is_empty(self) -> bool {
        self.anchor == self.cursor
    }

    pub(crate) fn ordered(self) -> ((usize, usize), (usize, usize)) {
        if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }

    /// The characters of `row` this sweep covers, or `None` where it covers
    /// none of them. `len` closes a row the sweep runs past.
    pub(crate) fn on_row(self, row: usize, len: usize) -> Option<Range<usize>> {
        let (start, end) = self.ordered();
        if row < start.0 || row > end.0 {
            return None;
        }
        let from = if row == start.0 { start.1 } else { 0 };
        let to = if row == end.0 { end.1.min(len) } else { len };
        (from < to).then_some(from..to)
    }
}

/// Paints the sweep onto the lines it covers.
pub(crate) fn highlight(lines: &mut [Line<'static>], sweep: Sweep) {
    let (start, end) = sweep.ordered();
    for (row, line) in lines.iter_mut().enumerate().take(end.0 + 1).skip(start.0) {
        let len = line
            .spans
            .iter()
            .map(|span| span.content.chars().count())
            .sum();
        if let Some(range) = sweep.on_row(row, len) {
            line.spans = apply_selection(std::mem::take(&mut line.spans), &range);
        }
    }
}

/// The text the sweep covers, one logical line to a line of text. `None`
/// where it covers nothing.
pub(crate) fn swept_text(lines: &[Line<'_>], sweep: Sweep) -> Option<String> {
    let (start, end) = sweep.ordered();
    let mut text = String::new();
    for (row, line) in lines.iter().enumerate().take(end.0 + 1).skip(start.0) {
        if row > start.0 {
            text.push('\n');
        }
        let chars = line_chars(line);
        if let Some(range) = sweep.on_row(row, chars.len()) {
            text.extend(&chars[range]);
        }
    }
    (!text.is_empty()).then_some(text)
}

/// The `(line, char)` drawn at `position` when `lines` are wrapped into `area`
/// and scrolled down `scroll` rows. A position off the area pins to its edge,
/// so a drag past it keeps going, and one below the last row lands at the end
/// of the last line.
pub(crate) fn wrapped_position(
    lines: &[Line<'_>],
    area: Rect,
    scroll: u16,
    position: Position,
) -> (usize, usize) {
    if area.is_empty() {
        return (0, 0);
    }
    let column = usize::from(position.x.clamp(area.x, area.right() - 1) - area.x);
    let mut row =
        usize::from(scroll) + usize::from(position.y.clamp(area.y, area.bottom() - 1) - area.y);
    let mut last = (0, 0);
    for (index, line) in lines.iter().enumerate() {
        let chars = line_chars(line);
        let starts: Vec<usize> = iter::once(0)
            .chain(
                wrap_breaks(&chars, area.width)
                    .into_iter()
                    .map(|wrap| wrap.start),
            )
            .collect();
        if let Some(&start) = starts.get(row) {
            let end = starts.get(row + 1).copied().unwrap_or(chars.len());
            let shown: String = chars[start..end].iter().collect();
            return (index, (start + char_at_column(&shown, column)).min(end));
        }
        row -= starts.len();
        last = (index, chars.len());
    }
    last
}

/// The character a display column falls on, so a press lands where the reader
/// sees the pointer rather than that many chars along a row of wide glyphs.
pub(crate) fn char_at_column(text: &str, column: usize) -> usize {
    let mut width = 0;
    let mut index = 0;
    for grapheme in text.graphemes(true) {
        if width >= column {
            return index;
        }
        width += grapheme.width();
        index += grapheme.chars().count();
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::SELECTION;
    use test_case::test_case;

    const AREA: Rect = Rect::new(2, 1, 10, 3);
    /// Breaks at the space after `alpha` in a [`AREA`]-wide body.
    const WORDS: &str = "alpha bravo";
    /// Too long for one row and with nowhere to break.
    const TOKEN: &str = "abcdefghijklmn";
    const WIDE: &str = "日本語の文字列";
    const JOINED: &str = "pha bra";
    const ACROSS_LINES: &str = "bravo\nabc";
    const WRAP_BROKE: &str = "a sweep across a soft wrap must copy no break";
    const POSITION_WRONG: &str = "a cell must read back as the character drawn there";
    const HIGHLIGHT_WRONG: &str = "only the swept characters carry the selection style";

    fn lines(texts: &[&'static str]) -> Vec<Line<'static>> {
        texts.iter().copied().map(Line::from).collect()
    }

    fn at(column: u16, row: u16) -> Position {
        Position::new(AREA.x + column, AREA.y + row)
    }

    #[test_case(&[WORDS], 0, at(3, 0), (0, 3)  ; "first_row")]
    #[test_case(&[WORDS], 0, at(2, 1), (0, 8)  ; "after_a_word_wrap")]
    #[test_case(&[WORDS], 0, at(9, 0), (0, 6)  ; "past_a_word_wrap_row_end_is_the_next_row")]
    #[test_case(&[TOKEN], 0, at(2, 1), (0, 12) ; "after_a_mid_word_wrap")]
    #[test_case(&[WIDE],  0, at(4, 0), (0, 2)  ; "wide_glyphs_take_two_columns")]
    #[test_case(&[WIDE],  0, at(1, 1), (0, 6)  ; "wide_glyphs_wrap_at_the_width")]
    #[test_case(&[WORDS, "x"], 1, at(0, 1), (1, 0) ; "scrolled")]
    #[test_case(&[WORDS], 0, at(0, 2), (0, 11) ; "below_the_last_row_is_the_end")]
    #[test_case(&[WORDS], 0, Position::new(0, 0), (0, 0) ; "off_the_area_pins_to_its_edge")]
    fn a_cell_reads_back_as_its_character(
        texts: &[&'static str],
        scroll: u16,
        position: Position,
        expected: (usize, usize),
    ) {
        assert_eq!(
            wrapped_position(&lines(texts), AREA, scroll, position),
            expected,
            "{POSITION_WRONG}"
        );
    }

    #[test]
    fn a_sweep_across_a_soft_wrap_copies_no_break() {
        let painted = lines(&[WORDS]);
        let mut sweep = Sweep::at(wrapped_position(&painted, AREA, 0, at(2, 0)));
        sweep.drag_to(wrapped_position(&painted, AREA, 0, at(3, 1)));

        assert_eq!(
            swept_text(&painted, sweep).as_deref(),
            Some(JOINED),
            "{WRAP_BROKE}"
        );
    }

    #[test]
    fn a_sweep_across_lines_copies_their_break() {
        let mut sweep = Sweep::at((0, 6));
        sweep.drag_to((1, 3));

        assert_eq!(
            swept_text(&lines(&[WORDS, TOKEN]), sweep).as_deref(),
            Some(ACROSS_LINES)
        );
    }

    #[test]
    fn an_empty_sweep_copies_nothing() {
        assert_eq!(swept_text(&lines(&[WORDS]), Sweep::at((0, 2))), None);
    }

    #[test]
    fn highlight_marks_only_the_swept_characters() {
        let mut painted = lines(&[WORDS]);
        let mut sweep = Sweep::at((0, 6));
        sweep.drag_to((0, 9));

        highlight(&mut painted, sweep);

        let marked: String = painted[0]
            .spans
            .iter()
            .filter(|span| span.style == span.style.patch(SELECTION))
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(marked, "bra", "{HIGHLIGHT_WRONG}");
    }

    #[test_case("abc",  0, 0 ; "column_zero_is_the_first_char")]
    #[test_case("abc",  2, 2 ; "a_narrow_column_is_its_own_char")]
    #[test_case("abc", 99, 3 ; "past_the_end_is_the_end")]
    #[test_case("日本",  2, 1 ; "a_wide_glyph_holds_two_columns")]
    fn a_column_reads_back_as_its_character(text: &str, column: usize, expected: usize) {
        assert_eq!(char_at_column(text, column), expected);
    }
}
