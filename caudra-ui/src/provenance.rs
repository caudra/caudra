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
