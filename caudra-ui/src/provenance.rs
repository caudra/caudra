//! Maps rendered cells back to the markdown that produced them.
//!
//! Selection works in screen coordinates, but the renderer drops syntax on
//! the way to the screen: heading hashes, emphasis delimiters, code fences,
//! list markers, table pipes. Rather than reverse-engineer the glyphs, each
//! painted line keeps the byte ranges `caudra-markdown` reported, and copy
//! slices the original text.

use std::ops::Range;
use std::sync::Arc;

use caudra_markdown::render::{self, SpanSource};
use ratatui::text::Line;
use unicode_width::UnicodeWidthChar;

use crate::selection::{ScreenSelection, col_range, line_chars, wrap_breaks};

/// Provenance for one painted line. `spans` is parallel to the line's
/// ratatui spans.
#[derive(Clone, Debug, Default)]
pub(crate) struct LineProvenance {
    /// The whole source row, used when a selection covers the line. Carries
    /// the syntax the spans dropped.
    pub line: Option<Range<u32>>,
    pub spans: Vec<SpanSource>,
}

impl LineProvenance {
    /// A line with no markdown behind it, such as an injected role prefix.
    pub fn chrome(span_count: usize) -> Self {
        Self {
            line: None,
            spans: vec![SpanSource::Chrome; span_count],
        }
    }
}

/// Provenance for a rendered message, plus the exact text its ranges index.
/// That text is what was parsed, which is not always the raw message: long
/// lines are truncated before rendering.
#[derive(Clone, Debug)]
pub(crate) struct Provenance {
    source: Arc<str>,
    lines: Vec<LineProvenance>,
}

impl Provenance {
    pub fn new(source: Arc<str>, lines: Vec<LineProvenance>) -> Self {
        Self { source, lines }
    }

    pub fn prepend_chrome_line(&mut self, span_count: usize) {
        self.lines.insert(0, LineProvenance::chrome(span_count));
    }

    pub fn push_chrome_line(&mut self, span_count: usize) {
        self.lines.push(LineProvenance::chrome(span_count));
    }

    pub fn source(&self) -> &Arc<str> {
        &self.source
    }

    /// The rows behind `range`, for a caller that is about to splice the
    /// painted lines they belong to somewhere else.
    pub fn lines_in(&self, range: Range<usize>) -> Option<Vec<LineProvenance>> {
        self.lines.get(range).map(<[LineProvenance]>::to_vec)
    }

    /// Replaces the rows of a spliced line range. A re-highlighted body changes
    /// the span count of every line it touches, so its rows have to travel with
    /// its lines: [`Self::extract`] zips the two and would otherwise read a
    /// long line against a short row and silently give up.
    pub fn splice_lines(&mut self, range: Range<usize>, lines: Vec<LineProvenance>) -> bool {
        if range.start > range.end || range.end > self.lines.len() {
            return false;
        }
        self.lines.splice(range, lines);
        true
    }

    /// The source byte the cell at (`row`, `column`) was painted from, for a
    /// click to resolve against the text rather than the glyphs.
    ///
    /// `None` where no byte lines up: chrome the markdown never produced, and
    /// any span the renderer rewrote on the way to the screen.
    pub fn byte_at(&self, lines: &[Line<'_>], width: u16, row: u16, column: u16) -> Option<u32> {
        if width == 0 || column >= width || self.lines.len() != lines.len() {
            return None;
        }

        let mut at = 0u16;
        for (line, provenance) in lines.iter().zip(&self.lines) {
            let chars = line_chars(line);
            let starts = row_starts(&chars, width);
            for (index, &start) in starts.iter().enumerate() {
                if at != row {
                    at = at.saturating_add(1);
                    continue;
                }
                let end = starts.get(index + 1).copied().unwrap_or(chars.len());
                let offset = column_char(&chars[start..end], column)?;
                return span_byte(line, provenance, start + offset);
            }
        }
        None
    }

    /// Source text for the rows `from..to` of `lines`, clipped horizontally
    /// by `sel`. Returns `None` when any covered span lacks provenance, so
    /// the caller can fall back to scraping cells.
    pub fn extract(
        &self,
        lines: &[Line<'_>],
        width: u16,
        sel: &ScreenSelection,
        from: u16,
        to: u16,
    ) -> Option<String> {
        if width == 0 || self.lines.len() != lines.len() {
            return None;
        }

        let mut ranges: Vec<Range<u32>> = Vec::new();
        let mut row: u16 = 0;

        for (index, line) in lines.iter().enumerate() {
            let chars = line_chars(line);
            let starts = row_starts(&chars, width);
            let covered = covered_chars(&chars, &starts, sel, width, from, to, &mut row);
            let Some(covered) = covered else { continue };

            let provenance = &self.lines[index];
            if covered.start == 0 && covered.end == chars.len() {
                match &provenance.line {
                    Some(range) => ranges.push(range.clone()),
                    // A fully covered row with no line range still has spans
                    // worth reading, so fall through rather than drop it.
                    None => span_ranges(line, provenance, &covered, &mut ranges)?,
                }
            } else {
                span_ranges(line, provenance, &covered, &mut ranges)?;
            }
        }

        Some(render::source_text(&self.source, ranges))
    }
}

/// Char index at which each display row of a wrapped line starts.
fn row_starts(chars: &[char], width: u16) -> Vec<usize> {
    let mut starts = vec![0usize];
    starts.extend(wrap_breaks(chars, width).into_iter().map(|b| b.start));
    starts
}

/// Chars of a line that the selection touches, advancing `row` past every
/// display row the line occupies.
fn covered_chars(
    chars: &[char],
    starts: &[usize],
    sel: &ScreenSelection,
    width: u16,
    from: u16,
    to: u16,
    row: &mut u16,
) -> Option<Range<usize>> {
    let mut first: Option<usize> = None;
    let mut last: Option<usize> = None;

    for (i, &start) in starts.iter().enumerate() {
        let end = starts.get(i + 1).copied().unwrap_or(chars.len());
        let current = *row;
        *row = row.saturating_add(1);

        if current < from || current >= to {
            continue;
        }
        let (col_start, col_end) = col_range(sel, 0, width.saturating_sub(1), current);
        let mut col = 0usize;
        for (idx, ch) in chars.iter().enumerate().take(end).skip(start) {
            let cw = ch.width().unwrap_or(0).max(1);
            if col + cw > col_start as usize && col <= col_end as usize {
                first.get_or_insert(idx);
                last = Some(idx);
            }
            col += cw;
        }
        // An empty row inside the selection still belongs to the line, and
        // dropping it would swallow blank lines between blocks.
        if start == end && col_start == 0 {
            first.get_or_insert(start);
            last = Some(start);
        }
    }

    Some(first?..last? + 1)
}

/// Which char of a display row the cell at `column` shows.
fn column_char(chars: &[char], column: u16) -> Option<usize> {
    let mut at = 0usize;
    for (index, ch) in chars.iter().enumerate() {
        at += ch.width().unwrap_or(0).max(1);
        if (column as usize) < at {
            return Some(index);
        }
    }
    None
}

/// The source byte behind char `index` of a painted line. Only a verbatim span
/// can answer: anywhere else the rendered chars are not the source slice, so
/// there is no byte to name.
fn span_byte(line: &Line<'_>, provenance: &LineProvenance, index: usize) -> Option<u32> {
    let mut at = 0usize;
    for (span, source) in line.spans.iter().zip(&provenance.spans) {
        let len = span.content.chars().count();
        if index >= at + len {
            at += len;
            continue;
        }
        let SpanSource::Range(source) = source else {
            return None;
        };
        if !source.verbatim {
            return None;
        }
        let byte = span
            .content
            .char_indices()
            .nth(index - at)
            .map_or(span.content.len(), |(byte, _)| byte) as u32;
        return Some(source.range.start + byte);
    }
    None
}

/// Ranges for the spans overlapping `covered`. Fails on spans with no
/// provenance so the caller can fall back wholesale.
fn span_ranges(
    line: &Line<'_>,
    provenance: &LineProvenance,
    covered: &Range<usize>,
    out: &mut Vec<Range<u32>>,
) -> Option<()> {
    let mut at = 0usize;
    for (span, source) in line.spans.iter().zip(&provenance.spans) {
        let len = span.content.chars().count();
        let (start, end) = (at, at + len);
        at = end;

        if end <= covered.start || start >= covered.end {
            continue;
        }
        match source {
            SpanSource::Chrome => {}
            SpanSource::Unknown => return None,
            SpanSource::Range(source) if !source.verbatim => out.push(source.range.clone()),
            SpanSource::Range(source) => {
                let lo = covered.start.saturating_sub(start);
                let hi = (covered.end - start).min(len);
                let bytes = |n: usize| {
                    span.content
                        .char_indices()
                        .nth(n)
                        .map_or(span.content.len(), |(b, _)| b) as u32
                };
                out.push(source.range.start + bytes(lo)..source.range.start + bytes(hi));
            }
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::text_to_painted;
    use ratatui::style::Style;

    const WIDTH: u16 = 40;
    const WRONG_BYTE: &str = "the cell does not name the source byte behind it";
    const NOT_INERT: &str = "a rewritten span named a source byte it cannot have";

    fn painted(text: &str, width: u16) -> (Vec<Line<'static>>, Provenance) {
        let (painted, parsed) = text_to_painted(
            text,
            "",
            Style::default(),
            Style::default(),
            width,
            None,
            Vec::new(),
        );
        (painted.lines, Provenance::new(parsed, painted.provenance))
    }

    #[test]
    fn a_cell_names_the_source_byte_behind_it() {
        const TEXT: &str = "see @src/lib.rs now";
        let (lines, provenance) = painted(TEXT, WIDTH);
        let column = TEXT.find('@').expect("a sigil");

        let byte = provenance.byte_at(&lines, WIDTH, 0, column as u16);

        assert_eq!(byte, Some(column as u32), "{WRONG_BYTE}");
    }

    /// The heading marker never reaches the screen, so the first cell of the
    /// row is two bytes into the source rather than at its start.
    #[test]
    fn dropped_syntax_does_not_shift_the_byte() {
        const TEXT: &str = "# Title";
        let (lines, provenance) = painted(TEXT, WIDTH);

        let byte = provenance.byte_at(&lines, WIDTH, 0, 0);

        assert_eq!(
            byte,
            Some(TEXT.find('T').expect("a title") as u32),
            "{WRONG_BYTE}"
        );
    }

    /// A word too long for the width is broken mid-word, so the second row
    /// starts where the first ran out rather than at a space.
    #[test]
    fn a_wrapped_row_counts_from_where_the_line_broke() {
        const NARROW: u16 = 10;
        const TEXT: &str = "abcdefghijklmnopqrst";
        let (lines, provenance) = painted(TEXT, NARROW);

        let byte = provenance.byte_at(&lines, NARROW, 1, 0);

        assert_eq!(byte, Some(u32::from(NARROW)), "{WRONG_BYTE}");
    }

    /// Inline maths is copied whole because its glyphs are not its source, so
    /// no cell inside it can name a byte.
    #[test]
    fn a_rewritten_span_names_no_byte() {
        const TEXT: &str = "a $x^2$ b";
        let (lines, provenance) = painted(TEXT, WIDTH);

        assert_eq!(provenance.byte_at(&lines, WIDTH, 0, 2), None, "{NOT_INERT}");
    }
}
