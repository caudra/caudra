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

    pub fn keep_rows(lines: &[Self], kept: Range<usize>) -> Option<Vec<Self>> {
        let mut rows = lines.get(kept.clone())?.to_vec();
        let mut start = kept.start;
        for group in rows.chunk_by_mut(|a, b| a.line.is_some() && a.line == b.line) {
            let end = start + group.len();
            let cut = group[0].line.as_ref().is_some_and(|range| {
                (start > 0 && lines[start - 1].line.as_ref() == Some(range))
                    || lines
                        .get(end)
                        .is_some_and(|row| row.line.as_ref() == Some(range))
            });
            if cut {
                let range =
                    bridged(group.iter().flat_map(|row| &row.spans).filter_map(
                        |span| match span {
                            SpanSource::Range(source) => Some(source.range.clone()),
                            SpanSource::Chrome | SpanSource::Unknown => None,
                        },
                    ));
                for row in group {
                    row.line = range.clone();
                }
            }
            start = end;
        }
        Some(rows)
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

    pub fn kept_lines(&self, range: Range<usize>) -> Option<Vec<LineProvenance>> {
        LineProvenance::keep_rows(&self.lines, range)
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
        let mut row: u16 = 0;
        let covered: Vec<_> = lines
            .iter()
            .map(|line| {
                let chars = line_chars(line);
                let starts = row_starts(&chars, width);
                covered_chars(&chars, &starts, sel, width, from, to, &mut row)
            })
            .collect();
        self.swept_source(lines, &covered)
    }

    /// Source text for a sweep over `rows`, painted one to a provenance line,
    /// from `start` up to `end`, both `(row, char)` and `end` exclusive.
    /// Returns `None` where the rows are not the ones these lines describe, or
    /// a swept span lacks provenance.
    pub fn extract_rows(
        &self,
        rows: &[Line<'_>],
        start: (usize, usize),
        end: (usize, usize),
    ) -> Option<String> {
        if self.lines.len() != rows.len() {
            return None;
        }
        let covered: Vec<_> = rows
            .iter()
            .enumerate()
            .map(|(row, painted)| {
                (start.0..=end.0).contains(&row).then(|| {
                    let len = char_count(painted);
                    let from = if row == start.0 { start.1.min(len) } else { 0 };
                    let to = if row == end.0 { end.1.min(len) } else { len };
                    from..to
                })
            })
            .collect();
        self.swept_source(rows, &covered)
    }

    /// The source behind the chars `covered` names on each of `lines`, `None`
    /// for a line the sweep missed.
    ///
    /// Every line a block was broken onto repeats the block's range, so the
    /// block is copied whole, syntax and all, only where the sweep takes in
    /// every line of it. A sweep that starts or stops inside it copies the
    /// source from the first span it covers to the last: between two spans of
    /// one block lies only that block's syntax, such as the asterisks around
    /// bold text, which `source_text` would break onto a line of its own.
    fn swept_source(&self, lines: &[Line<'_>], covered: &[Option<Range<usize>>]) -> Option<String> {
        let whole = |index: usize| covered[index] == Some(0..char_count(&lines[index]));
        let mut ranges: Vec<Range<u32>> = Vec::new();
        let mut index = 0;
        while let Some(first) = (index..lines.len()).find(|&at| covered[at].is_some()) {
            let run = self.block_lines(first);
            index = run.end;
            let block = self.lines[first].line.as_ref();
            if let Some(range) = block
                && run.clone().all(whole)
            {
                ranges.push(range.clone());
                continue;
            }
            let mut spans = Vec::new();
            for at in first..run.end {
                if let Some(chars) = covered[at].as_ref().filter(|chars| !chars.is_empty()) {
                    span_ranges(&lines[at], &self.lines[at], chars, &mut spans)?;
                }
            }
            match block {
                Some(_) => ranges.extend(bridged(spans.into_iter())),
                None => ranges.extend(spans),
            }
        }
        Some(render::source_text(&self.source, ranges))
    }

    /// The lines the block painted on line `index` was broken onto: its
    /// neighbours that repeat its range, or the line alone when it has none.
    fn block_lines(&self, index: usize) -> Range<usize> {
        let Some(range) = self.lines[index].line.as_ref() else {
            return index..index + 1;
        };
        let same = |other: &&LineProvenance| other.line.as_ref() == Some(range);
        let before = self.lines[..index].iter().rev().take_while(same).count();
        let after = self.lines[index..].iter().take_while(same).count();
        index - before..index + after
    }
}

fn char_count(line: &Line<'_>) -> usize {
    line.spans
        .iter()
        .map(|span| span.content.chars().count())
        .sum()
}

/// One slice from the first byte `spans` cover to the last.
fn bridged(spans: impl Iterator<Item = Range<u32>>) -> Option<Range<u32>> {
    spans.reduce(|a, b| a.start.min(b.start)..a.end.max(b.end))
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

        if len == 0
            && covered.start == 0
            && covered.end == char_count(line)
            && let SpanSource::Range(source) = source
        {
            out.push(source.range.clone());
            continue;
        }
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
    use crate::markdown::{text_to_painted, text_to_rows};
    use crate::selection::line_text;
    use caudra_markdown::render::CODE_BAR;
    use ratatui::style::Style;
    use test_case::test_case;

    const WIDTH: u16 = 40;
    const WRONG_BYTE: &str = "the cell does not name the source byte behind it";
    const NOT_INERT: &str = "a rewritten span named a source byte it cannot have";
    /// Narrow enough to wrap [`PARAGRAPH`] over three rows.
    const ROWS_WIDTH: u16 = 16;
    const HEADING: &str = "## Heading";
    const PARAGRAPH: &str = "Some **bold** words and a [link](https://example.com/x) that wrap.";
    const FENCE: &str = "```rust\nlet x = 1;\n```";
    const FIRST_WORD: &str = "Some";
    /// The first word of the paragraph's second row.
    const SECOND_ROW: &str = "and";
    const LAST_WORD: &str = "wrap.";
    /// The start of the paragraph's first row as painted, and the source behind it.
    const CUT_PAINTED: &str = "Some bold wo";
    const CUT_SOURCE: &str = "Some **bold** wo";
    const SOURCE_LOST: &str = "the copy is not the Markdown behind the rows";
    const OVER_COPIED: &str = "the copy reaches past the rows swept";
    const UNWRAPPED: &str = "the paragraph has to wrap for this to test anything";
    const NO_ROW: &str = "no row starts with";
    const NO_COPY: &str = "the sweep copied nothing";
    const CLIPPED_CODE: &str = "```text\nalpha\nbeta\ngamma\ndelta\n```";
    const BLANK_CODE: &str = "```text\nalpha\n\nbeta\n\ngamma\n```";
    const CODE_FIRST: &str = "alpha";
    const INVALID_ROWS: &str = "the retained rows are outside the source";

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

    fn document() -> String {
        format!("{HEADING}\n\n{PARAGRAPH}\n\n{FENCE}")
    }

    /// The document painted a row to a line, as the docs reader paints a page.
    fn rows() -> (Vec<Line<'static>>, Provenance) {
        let (painted, source) = text_to_rows(&document(), Style::default(), ROWS_WIDTH, Vec::new());
        (painted.lines, Provenance::new(source, painted.provenance))
    }

    fn row_with(lines: &[Line<'_>], text: &str) -> usize {
        lines
            .iter()
            .position(|line| line_text(line).starts_with(text))
            .unwrap_or_else(|| panic!("{NO_ROW} {text}"))
    }

    fn end_of(lines: &[Line<'_>], row: usize) -> (usize, usize) {
        (row, line_text(&lines[row]).chars().count())
    }

    fn copy_all(lines: &[Line<'_>], provenance: &Provenance, width: u16) -> String {
        let copied = provenance
            .extract_rows(lines, (0, 0), end_of(lines, lines.len() - 1))
            .expect(NO_COPY);
        let selection = ScreenSelection {
            start_row: 0,
            start_col: 0,
            end_row: lines.len() as u16 - 1,
            end_col: width - 1,
        };
        assert_eq!(
            provenance.extract(lines, width, &selection, 0, lines.len() as u16),
            Some(copied.clone()),
            "{SOURCE_LOST}"
        );
        copied
    }

    #[test_case(CLIPPED_CODE, 0..2, 0, "alpha\nbeta"; "prefix")]
    #[test_case(CLIPPED_CODE, 2..4, 0, "gamma\ndelta"; "suffix")]
    #[test_case(CLIPPED_CODE, 1..3, 0, "beta\ngamma"; "middle")]
    #[test_case(CLIPPED_CODE, 0..4, CODE_BAR.chars().count(), "alpha\nbeta\ngamma\ndelta"; "body_only")]
    #[test_case(CLIPPED_CODE, 0..4, CODE_BAR.chars().count() + 2, "pha\nbeta\ngamma\ndelta"; "partial_word")]
    #[test_case(BLANK_CODE, 1..3, 0, "\nbeta"; "leading_blank_row")]
    #[test_case(BLANK_CODE, 2..4, 0, "beta\n"; "trailing_blank_row")]
    #[test_case(BLANK_CODE, 1..4, 0, "\nbeta\n"; "both_blank_rows")]
    #[test_case(BLANK_CODE, 1..3, CODE_BAR.chars().count() - 1, "beta"; "partial_blank_gutter")]
    fn partial_code_selections_copy_no_fences(
        text: &str,
        selected: Range<usize>,
        start_col: usize,
        expected: &str,
    ) {
        let (lines, provenance) = painted(text, WIDTH);
        let first = row_with(&lines, &format!("{CODE_BAR}{CODE_FIRST}"));
        let start = first + selected.start;
        let last = first + selected.end - 1;
        let selection = ScreenSelection {
            start_row: start as u16,
            start_col: start_col as u16,
            end_row: last as u16,
            end_col: WIDTH - 1,
        };

        assert_eq!(
            provenance
                .extract_rows(&lines, (start, start_col), end_of(&lines, last))
                .as_deref(),
            Some(expected),
            "{OVER_COPIED}"
        );
        assert_eq!(
            provenance
                .extract(&lines, WIDTH, &selection, start as u16, last as u16 + 1)
                .as_deref(),
            Some(expected),
            "{OVER_COPIED}"
        );
    }

    #[test_case("alpha\n\nbeta", true; "blank_lines")]
    #[test_case("\talpha\n\t\tbeta", true; "tabs")]
    #[test_case("é界\n    beta", true; "unicode_and_indentation")]
    #[test_case("alpha\n```\nbeta", true; "literal_backticks")]
    #[test_case("alpha\nbeta", false; "streaming")]
    fn selecting_code_text_preserves_its_body_bytes(body: &str, closed: bool) {
        let text = format!("````text\n{body}{}", if closed { "\n````" } else { "" });
        let (lines, provenance) = painted(&text, WIDTH);
        let first = row_with(&lines, CODE_BAR);

        assert_eq!(
            provenance
                .extract_rows(
                    &lines,
                    (first, CODE_BAR.chars().count()),
                    end_of(&lines, lines.len() - 1),
                )
                .as_deref(),
            Some(body),
            "{SOURCE_LOST}"
        );
    }

    #[test_case(0..2, "alpha\nbeta"; "prefix")]
    #[test_case(2..4, "gamma\ndelta"; "suffix")]
    #[test_case(1..3, "beta\ngamma"; "middle")]
    #[test_case(0..4, CLIPPED_CODE; "whole_block")]
    fn retained_code_rows_copy_only_their_source(kept: Range<usize>, expected: &str) {
        let (lines, provenance) = painted(CLIPPED_CODE, WIDTH);
        let first = row_with(&lines, &format!("{CODE_BAR}{CODE_FIRST}"));
        let kept = first + kept.start..first + kept.end;
        let retained = Provenance::new(
            Arc::clone(provenance.source()),
            provenance.kept_lines(kept.clone()).expect(INVALID_ROWS),
        );

        assert_eq!(
            copy_all(&lines[kept], &retained, WIDTH),
            expected,
            "{OVER_COPIED}"
        );
    }

    #[test_case(0..1, "alpha"; "prefix")]
    #[test_case(1..2, "beta"; "middle")]
    #[test_case(2..3, "gamma"; "suffix")]
    fn repeated_clipping_narrows_the_code_again(kept: Range<usize>, expected: &str) {
        let (lines, provenance) = painted(CLIPPED_CODE, WIDTH);
        let first = row_with(&lines, &format!("{CODE_BAR}{CODE_FIRST}"));
        let outer = first..first + 3;
        let retained = Provenance::new(
            Arc::clone(provenance.source()),
            provenance.kept_lines(outer.clone()).expect(INVALID_ROWS),
        );
        let narrowed = Provenance::new(
            Arc::clone(provenance.source()),
            retained.kept_lines(kept.clone()).expect(INVALID_ROWS),
        );

        assert_eq!(
            copy_all(&lines[outer][kept], &narrowed, WIDTH),
            expected,
            "{OVER_COPIED}"
        );
    }

    #[test_case(0..1; "prefix")]
    #[test_case(1..2; "middle")]
    #[test_case(2..3; "suffix")]
    fn clipping_a_wrapped_code_line_does_not_restore_its_other_rows(kept: Range<usize>) {
        const CODE: &str = "```text\nabcdefghijklmnopqrstuvwxyz0123456789\n```";
        let (lines, provenance) = painted(CODE, ROWS_WIDTH);
        let first = row_with(&lines, &format!("{CODE_BAR}abc"));
        let kept = first + kept.start..first + kept.end;
        let expected: String = lines[kept.start]
            .spans
            .iter()
            .zip(&provenance.lines[kept.start].spans)
            .filter(|(_, source)| matches!(source, SpanSource::Range(_)))
            .map(|(span, _)| span.content.as_ref())
            .collect();
        let retained = Provenance::new(
            Arc::clone(provenance.source()),
            provenance.kept_lines(kept.clone()).expect(INVALID_ROWS),
        );

        assert_eq!(
            copy_all(&lines[kept], &retained, ROWS_WIDTH),
            expected,
            "{OVER_COPIED}"
        );
    }

    #[test_case(0..0; "empty")]
    #[test_case(0..usize::MAX; "past_end")]
    fn retaining_invalid_or_empty_rows_is_bounded(kept: Range<usize>) {
        let (_, provenance) = painted(CLIPPED_CODE, WIDTH);
        let rows = provenance.kept_lines(kept.clone());
        assert_eq!(rows.as_ref().map(Vec::len), kept.is_empty().then_some(0));
    }

    #[test_case(false; "blank_body")]
    #[test_case(true; "atomic_tab_body")]
    fn clipped_rows_preserve_blank_and_atomic_sources(tab: bool) {
        let middle = if tab { "\tbeta" } else { "" };
        let text = format!("```text\nalpha\n{middle}\ngamma\n```");
        let (lines, provenance) = painted(&text, WIDTH);
        let first = row_with(&lines, &format!("{CODE_BAR}{CODE_FIRST}"));
        let kept = first + 1..first + 2;
        let retained = Provenance::new(
            Arc::clone(provenance.source()),
            provenance.kept_lines(kept.clone()).expect(INVALID_ROWS),
        );

        assert_eq!(
            copy_all(&lines[kept], &retained, WIDTH),
            middle,
            "{OVER_COPIED}"
        );
    }

    #[test_case(ROWS_WIDTH; "wrapped")]
    #[test_case(WIDTH; "wide")]
    fn clipping_prose_keeps_a_complete_inner_fence(width: u16) {
        let text = format!("{PARAGRAPH}\n\n{FENCE}\n\n{PARAGRAPH}");
        let (painted, source) = text_to_rows(&text, Style::default(), width, Vec::new());
        let provenance = Provenance::new(source, painted.provenance);
        let lines = painted.lines;
        let kept = 1..lines.len() - 1;
        let retained = Provenance::new(
            Arc::clone(provenance.source()),
            provenance.kept_lines(kept.clone()).expect(INVALID_ROWS),
        );

        let copied = copy_all(&lines[kept], &retained, width);
        assert!(copied.contains(FENCE), "{SOURCE_LOST}: {copied}");
        assert!(!copied.contains(PARAGRAPH), "{OVER_COPIED}: {copied}");
    }

    #[test]
    fn a_sweep_over_every_row_copies_the_whole_source() {
        let (lines, provenance) = rows();
        let end = end_of(&lines, lines.len() - 1);

        let copied = provenance.extract_rows(&lines, (0, 0), end);

        assert_eq!(copied, Some(document()), "{SOURCE_LOST}");
    }

    #[test]
    fn a_sweep_stopping_inside_a_wrapped_paragraph_copies_no_further() {
        let (lines, provenance) = rows();
        let first = row_with(&lines, FIRST_WORD);
        let end = (first, FIRST_WORD.len());

        let copied = provenance.extract_rows(&lines, (0, 0), end);

        assert_eq!(
            copied,
            Some(format!("{HEADING}\n\n{FIRST_WORD}")),
            "{OVER_COPIED}"
        );
    }

    #[test]
    fn a_sweep_starting_on_a_later_wrapped_row_leaves_the_rows_above_out() {
        let (lines, provenance) = rows();
        let second = row_with(&lines, SECOND_ROW);
        let end = end_of(&lines, lines.len() - 1);

        let copied = provenance
            .extract_rows(&lines, (second, 0), end)
            .expect(NO_COPY);

        assert!(copied.starts_with(SECOND_ROW), "{OVER_COPIED}: {copied}");
        assert!(!copied.contains(FIRST_WORD), "{OVER_COPIED}: {copied}");
        assert!(copied.ends_with(FENCE), "{SOURCE_LOST}: {copied}");
    }

    #[test]
    fn a_sweep_cutting_a_row_keeps_the_syntax_between_its_spans() {
        let (lines, provenance) = rows();
        let row = row_with(&lines, CUT_PAINTED);

        let copied = provenance.extract_rows(&lines, (row, 0), (row, CUT_PAINTED.chars().count()));

        assert_eq!(copied.as_deref(), Some(CUT_SOURCE), "{SOURCE_LOST}");
    }

    /// A card's body is broken to the card, and every row of a paragraph names
    /// the whole paragraph, so a sweep over one row must not take the rest.
    #[test]
    fn a_selection_of_one_wrapped_row_copies_that_row_alone() {
        let (lines, provenance) = rows();
        let second = row_with(&lines, SECOND_ROW) as u16;
        let row = ScreenSelection {
            start_row: second,
            start_col: 0,
            end_row: second,
            end_col: ROWS_WIDTH - 1,
        };

        let copied = provenance
            .extract(&lines, ROWS_WIDTH, &row, second, second + 1)
            .expect(NO_COPY);

        assert!(copied.starts_with(SECOND_ROW), "{SOURCE_LOST}: {copied}");
        assert!(
            !copied.contains(FIRST_WORD) && !copied.contains(LAST_WORD),
            "{OVER_COPIED}: {copied}"
        );
    }

    #[test]
    fn a_sweep_over_one_whole_wrapped_paragraph_copies_its_source_once() {
        let (lines, provenance) = rows();
        let first = row_with(&lines, FIRST_WORD);
        let last = row_with(&lines, LAST_WORD);
        assert!(last > first + 1, "{UNWRAPPED}");

        let copied = provenance.extract_rows(&lines, (first, 0), end_of(&lines, last));

        assert_eq!(copied.as_deref(), Some(PARAGRAPH), "{SOURCE_LOST}");
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
