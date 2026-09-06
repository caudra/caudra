//! Turning one buffer line into styled, tab-expanded, horizontally clipped
//! spans.
//!
//! The buffer counts characters and the terminal counts columns, and a tab or a
//! CJK glyph makes those disagree. Everything that has to cross that boundary —
//! horizontal scrolling, the cursor, mouse clicks — goes through here.

use std::mem;
use std::ops::Range;

use caudra_highlight::StyledSegment;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

pub const TAB_STOP: usize = 4;

/// Stands in for a tab's expansion, for padding past the end of a line, and for
/// a wide glyph the window cut in half.
const BLANK: char = ' ';
/// Where a wrapped row is allowed to end. A non-breaking space is deliberately
/// absent: it is written precisely to stop a break happening there.
const BREAK_AFTER: [char; 2] = [' ', '\t'];

/// One terminal column. `ch` is `None` for the trailing half of a wide glyph,
/// which the glyph itself already covers and so prints nothing of its own.
#[derive(Clone, Copy)]
struct Column {
    ch: Option<char>,
    style: Style,
}

pub struct Row<'a> {
    pub text: &'a str,
    /// Syntax segments covering `text`. Any trailing newline is ignored, so the
    /// segments the highlighter produces can be handed over as they are.
    pub segments: Option<&'a [StyledSegment]>,
    pub base: Style,
    /// Character ranges painted over the syntax colours, later entries winning.
    /// A range may reach one past the end of the line to show a cursor or a
    /// selection that swallowed the newline.
    pub overlays: &'a [(Range<usize>, Style)],
}

impl Row<'_> {
    pub fn paint(&self, h_scroll: usize, width: usize) -> Line<'static> {
        let end = h_scroll + width;
        let columns = expand(&self.cells(end));

        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut text = String::new();
        let mut style = self.base;
        for (index, column) in columns.iter().enumerate().take(end).skip(h_scroll) {
            let Some(ch) = printable(*column, index, h_scroll..end) else {
                continue;
            };
            if !text.is_empty() && column.style != style {
                spans.push(Span::styled(mem::take(&mut text), style));
            }
            style = column.style;
            text.push(ch);
        }
        if !text.is_empty() {
            spans.push(Span::styled(text, style));
        }
        Line::from(spans)
    }

    /// The line as one style per character, overlays applied, padded with
    /// blanks where an overlay reaches past the end of the text.
    fn cells(&self, limit: usize) -> Vec<(char, Style)> {
        let mut cells: Vec<(char, Style)> = self.text.chars().zip(self.char_styles()).collect();

        let reach = self
            .overlays
            .iter()
            .map(|(range, _)| range.end)
            .max()
            .unwrap_or_default()
            .min(limit);
        if reach > cells.len() {
            cells.resize(reach, (BLANK, self.base));
        }

        for (range, style) in self.overlays {
            for (_, cell) in cells.iter_mut().take(range.end).skip(range.start) {
                *cell = cell.patch(*style);
            }
        }
        cells
    }

    fn char_styles(&self) -> Vec<Style> {
        let count = self.text.chars().count();
        let mut styles = vec![self.base; count];
        let Some(segments) = self.segments else {
            return styles;
        };
        let mut index = 0;
        for segment in segments {
            let style = self.base.patch(segment_style(segment));
            for _ in segment.text.chars().filter(|ch| !matches!(ch, '\n' | '\r')) {
                let Some(slot) = styles.get_mut(index) else {
                    return styles;
                };
                *slot = style;
                index += 1;
            }
        }
        styles
    }
}

/// Character column of `col`, which is where the cursor belongs on screen.
pub fn display_column(line: &str, col: usize) -> usize {
    let mut at = 0;
    for ch in line.chars().take(col) {
        at += char_width(ch, at);
    }
    at
}

/// Inverse of [`display_column`]: the character a click at `column` landed on.
pub fn char_index(line: &str, column: usize) -> usize {
    let mut at = 0;
    for (index, ch) in line.chars().enumerate() {
        at += char_width(ch, at);
        if at > column {
            return index;
        }
    }
    line.chars().count()
}

/// The display column each visual row of `line` starts at, in a pane `width`
/// columns wide. Always at least one entry, so a wrapped caller and an
/// unwrapped one read the same shape.
///
/// A row ends after the last break character that fits, so a word is never
/// split. A word too long for any row is cut at the column that fills one,
/// because there is nowhere else for it to go. Trailing blanks overhang the
/// right margin rather than being carried down, which is what keeps a wrapped
/// word starting flush with the one above it.
///
/// This is the only description of where the editor's rows fall. The gutter,
/// the click hit-test and the cursor all read it rather than measuring again.
pub fn wrap_columns(line: &str, width: usize) -> Vec<usize> {
    let width = width.max(1);
    let mut starts = vec![0];
    let mut row_start = 0;
    let mut after_break = None;
    let mut at = 0;
    for ch in line.chars() {
        let breaking = BREAK_AFTER.contains(&ch);
        let span = char_width(ch, at);
        if !breaking && at + span > row_start + width && at > row_start {
            row_start = after_break.filter(|&start| start > row_start).unwrap_or(at);
            starts.push(row_start);
            after_break = None;
        }
        at += span;
        if breaking {
            after_break = Some(at);
        }
    }
    starts
}

fn char_width(ch: char, at: usize) -> usize {
    if ch == '\t' {
        TAB_STOP - at % TAB_STOP
    } else {
        ch.width().unwrap_or_default()
    }
}

fn expand(cells: &[(char, Style)]) -> Vec<Column> {
    let mut columns = Vec::with_capacity(cells.len());
    for (ch, style) in cells {
        let width = char_width(*ch, columns.len());
        if width == 0 {
            continue;
        }
        let style = *style;
        if *ch == '\t' {
            columns.resize(
                columns.len() + width,
                Column {
                    ch: Some(BLANK),
                    style,
                },
            );
            continue;
        }
        columns.push(Column {
            ch: Some(*ch),
            style,
        });
        columns.resize(columns.len() + width - 1, Column { ch: None, style });
    }
    columns
}

/// Nothing is printed for a column the preceding wide glyph already covers. A
/// wide glyph with only one of its two columns inside the window would overflow
/// it, so a blank stands in at whichever end survived.
fn printable(column: Column, index: usize, window: Range<usize>) -> Option<char> {
    match column.ch {
        None => (index == window.start).then_some(BLANK),
        Some(ch) if ch.width().unwrap_or_default() > 1 && index + 1 >= window.end => Some(BLANK),
        Some(ch) => Some(ch),
    }
}

fn segment_style(segment: &StyledSegment) -> Style {
    let (red, green, blue) = segment.fg;
    let mut style = Style::default().fg(Color::Rgb(red, green, blue));
    if segment.bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if segment.italic {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if segment.underline {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    style
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Style};
    use test_case::test_case;

    use super::{Row, StyledSegment, char_index, display_column, wrap_columns};

    const WRONG_TEXT: &str = "the painted row does not read as expected";
    const WRONG_COLUMN: &str = "character and display columns do not line up";
    const WRONG_STYLE: &str = "the painted row is not styled as expected";
    const WRONG_ROWS: &str = "the line does not wrap onto the rows it should";

    fn plain(text: &str) -> Row<'_> {
        Row {
            text,
            segments: None,
            base: Style::default(),
            overlays: &[],
        }
    }

    fn painted(row: &Row<'_>, h_scroll: usize, width: usize) -> String {
        row.paint(h_scroll, width).to_string()
    }

    #[test_case("abc", 0, 3, "abc" ; "a short line is printed whole")]
    #[test_case("abcdef", 0, 3, "abc" ; "the window cuts the tail")]
    #[test_case("abcdef", 2, 3, "cde" ; "scrolling cuts the head")]
    #[test_case("abc", 0, 9, "abc" ; "a window wider than the line is not padded")]
    #[test_case("abc", 9, 3, "" ; "scrolling past the line leaves nothing")]
    #[test_case("abc", 0, 0, "" ; "a zero width window prints nothing")]
    fn painting_clips_to_the_window(text: &str, h_scroll: usize, width: usize, expected: &str) {
        assert_eq!(
            painted(&plain(text), h_scroll, width),
            expected,
            "{WRONG_TEXT}"
        );
    }

    #[test_case("\tx", 0, 8, "    x" ; "a tab reaches the next stop")]
    #[test_case("ab\tx", 0, 8, "ab  x" ; "a tab fills only what is left of the stop")]
    #[test_case("abcd\tx", 0, 9, "abcd    x" ; "a tab on a stop spans a whole one")]
    fn tabs_expand_to_the_next_stop(text: &str, h_scroll: usize, width: usize, expected: &str) {
        assert_eq!(
            painted(&plain(text), h_scroll, width),
            expected,
            "{WRONG_TEXT}"
        );
    }

    #[test_case(0, 4, "中文" ; "a wide glyph prints whole when it fits")]
    #[test_case(0, 3, "中 " ; "a wide glyph cut by the window becomes a blank")]
    #[test_case(1, 3, " 文" ; "scrolling into a wide glyph leaves its second column")]
    fn wide_glyphs_never_overflow_the_window(h_scroll: usize, width: usize, expected: &str) {
        assert_eq!(
            painted(&plain("中文"), h_scroll, width),
            expected,
            "{WRONG_TEXT}"
        );
    }

    #[test_case("abc", 0, 0 ; "the start of a line is column zero")]
    #[test_case("abc", 2, 2 ; "narrow characters are one column each")]
    #[test_case("\tabc", 1, 4 ; "a tab counts as its expansion")]
    #[test_case("中文", 1, 2 ; "a wide glyph counts as two columns")]
    #[test_case("中文", 2, 4 ; "wide glyphs accumulate")]
    fn display_columns_account_for_width(line: &str, col: usize, expected: usize) {
        assert_eq!(display_column(line, col), expected, "{WRONG_COLUMN}");
    }

    #[test_case("abc", 1, 1 ; "a click lands on the character under it")]
    #[test_case("abc", 9, 3 ; "a click past the end lands after the last character")]
    #[test_case("\tabc", 2, 0 ; "a click inside a tab lands on the tab")]
    #[test_case("\tabc", 4, 1 ; "a click after a tab lands on what follows")]
    #[test_case("中文", 1, 0 ; "a click on the second half of a glyph lands on the glyph")]
    fn clicks_map_back_to_characters(line: &str, column: usize, expected: usize) {
        assert_eq!(char_index(line, column), expected, "{WRONG_COLUMN}");
    }

    #[test_case("abc", 3 ; "a plain line round trips")]
    #[test_case("\tabc", 4 ; "a tabbed line round trips")]
    #[test_case("中文", 2 ; "a wide line round trips")]
    fn display_column_and_char_index_are_inverses(line: &str, count: usize) {
        for col in 0..=count {
            assert_eq!(
                char_index(line, display_column(line, col)),
                col,
                "{WRONG_COLUMN}"
            );
        }
    }

    #[test]
    fn overlays_paint_over_the_base_style() {
        let selection = Style::default().bg(Color::Blue);
        let row = Row {
            text: "abc",
            segments: None,
            base: Style::default().fg(Color::White),
            overlays: &[(1..2, selection)],
        };

        let line = row.paint(0, 3);
        let styled: Vec<(String, Option<Color>)> = line
            .spans
            .iter()
            .map(|span| (span.content.to_string(), span.style.bg))
            .collect();

        assert_eq!(
            styled,
            vec![
                ("a".to_owned(), None),
                ("b".to_owned(), Some(Color::Blue)),
                ("c".to_owned(), None),
            ],
            "{WRONG_STYLE}"
        );
    }

    #[test]
    fn an_overlay_past_the_end_of_the_line_paints_a_blank() {
        let row = Row {
            text: "ab",
            segments: None,
            base: Style::default(),
            overlays: &[(2..3, Style::default().bg(Color::Blue))],
        };

        assert_eq!(painted(&row, 0, 8), "ab ", "{WRONG_TEXT}");
        assert_eq!(row.paint(0, 8).spans.len(), 2, "{WRONG_STYLE}");
    }

    #[test]
    fn syntax_segments_colour_the_characters_they_cover() {
        let segments = vec![
            StyledSegment {
                text: "let".to_owned(),
                fg: (255, 0, 0),
                bold: false,
                italic: false,
                underline: false,
            },
            StyledSegment {
                text: " x\n".to_owned(),
                fg: (0, 255, 0),
                bold: false,
                italic: false,
                underline: false,
            },
        ];
        let row = Row {
            text: "let x",
            segments: Some(&segments),
            base: Style::default(),
            overlays: &[],
        };

        let line = row.paint(0, 8);
        let colours: Vec<(String, Option<Color>)> = line
            .spans
            .iter()
            .map(|span| (span.content.to_string(), span.style.fg))
            .collect();

        assert_eq!(
            colours,
            vec![
                ("let".to_owned(), Some(Color::Rgb(255, 0, 0))),
                (" x".to_owned(), Some(Color::Rgb(0, 255, 0))),
            ],
            "{WRONG_STYLE}"
        );
    }

    #[test]
    fn segments_longer_than_the_line_are_ignored_past_its_end() {
        let segments = vec![StyledSegment {
            text: "abcdef".to_owned(),
            fg: (1, 2, 3),
            bold: false,
            italic: false,
            underline: false,
        }];
        let row = Row {
            text: "abc",
            segments: Some(&segments),
            base: Style::default(),
            overlays: &[],
        };

        assert_eq!(painted(&row, 0, 8), "abc", "{WRONG_TEXT}");
    }

    #[test_case("", 8 => vec![0]; "an empty line still has one row")]
    #[test_case("hello", 8 => vec![0]; "a line that fits keeps one row")]
    #[test_case("hello world", 8 => vec![0, 6]; "a row ends after the space before the word")]
    #[test_case("abcdefghij", 8 => vec![0, 8]; "a word too long for a row is cut")]
    #[test_case("ab cdefghijkl", 8 => vec![0, 3, 11]; "an over-long word breaks after the space first")]
    #[test_case("\tword here", 8 => vec![0, 9]; "a tab counts its expansion rather than one column")]
    fn wrap_columns_breaks_on_words(text: &str, width: usize) -> Vec<usize> {
        wrap_columns(text, width)
    }

    /// The rows the render loop would paint from those columns, so the wrap and
    /// the painting are checked against each other rather than separately.
    #[test]
    fn a_wrapped_row_paints_only_its_own_words() {
        let (text, width) = ("hello world", 8);
        let starts = wrap_columns(text, width);
        let row = plain(text);
        let rows: Vec<String> = starts
            .iter()
            .enumerate()
            .map(|(index, &start)| {
                let span = starts
                    .get(index + 1)
                    .map_or(width, |&next| (next - start).min(width));
                painted(&row, start, span)
            })
            .collect();

        assert_eq!(rows, ["hello ", "world"], "{WRONG_ROWS}");
    }
}
