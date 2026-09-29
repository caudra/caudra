use crate::markdown::{DiagramSpan, LinkMap};
use crate::provenance::Provenance;
use crate::render_worker::RenderWorker;
use crate::theme;

use super::super::code_view::{BodySource, CodeBlock, RowTarget, ScrollSpan};
use super::super::tool_display::{HighlightRequest, ToolLines};
use super::layout::{SegmentChrome, SegmentKind};
use crate::provenance::LineProvenance;
use caudra_markdown::Source;
use caudra_markdown::render::SpanSource;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use std::cell::Cell;
use std::ops::Range;
use std::sync::Arc;

const INST_SUFFIX: &str = "__inst";

pub fn is_instruction_segment(id: &str) -> bool {
    id.ends_with(INST_SUFFIX)
}

pub fn instruction_id(parent_id: &str) -> String {
    format!("{parent_id}{INST_SUFFIX}")
}

pub fn instruction_parent(id: &str) -> Option<&str> {
    id.strip_suffix(INST_SUFFIX)
}

#[derive(Clone, Copy, Default)]
struct CachedHeight {
    at_width: u16,
    height: u16,
}

struct HighlightCache {
    request: HighlightRequest,
    theme_gen: u64,
    pending: Option<PendingHighlight>,
    paint: Option<Arc<HighlightPaint>>,
    local_source_rows: Option<Vec<LineProvenance>>,
}

struct HighlightPaint {
    request: HighlightRequest,
    lines: Vec<Line<'static>>,
    source_rows: Option<Vec<LineProvenance>>,
    syntax: Option<SourcePaint>,
}

struct PendingHighlight {
    id: u64,
    request: HighlightRequest,
    text: Vec<String>,
}

struct SourcePaint {
    source: String,
    styles: Vec<(Range<u32>, Style)>,
}

impl SourcePaint {
    fn new(
        request: &HighlightRequest,
        lines: &[Line<'_>],
        rows: &[LineProvenance],
    ) -> Option<Self> {
        let source = request.input_source()?;
        if rows.len() != lines.len() {
            return None;
        }
        let mut styles = Vec::new();
        for (line, row) in lines.iter().zip(rows) {
            if line.spans.len() != row.spans.len() {
                return None;
            }
            for (span, origin) in line.spans.iter().zip(&row.spans) {
                if let SpanSource::Range(origin) = origin {
                    let text =
                        source.get(origin.range.start as usize..origin.range.end as usize)?;
                    if !caudra_highlight::normalize_text(text).starts_with(span.content.as_ref()) {
                        return None;
                    }
                    if !origin.range.is_empty() {
                        styles.push((origin.range.clone(), span.style));
                    }
                }
            }
        }
        styles.sort_by_key(|(range, _)| (range.start, range.end));
        styles.dedup();
        Some(Self { source, styles })
    }
}

#[derive(Default)]
pub(super) struct Segment {
    lines: Vec<Line<'static>>,
    /// Set for segments whose renderer kept the source behind each painted
    /// line. Selection uses it to copy that source; without it copy falls back
    /// to scraping cells, which reads a code row's gutter back as text.
    provenance: Option<Provenance>,
    /// The painted lines holding a card's code, so a copy that ran past a
    /// block can fence it rather than drop code into prose. A batch card holds
    /// one per child. Cleared and restored alongside `provenance`, whose rows
    /// they index.
    code_blocks: Vec<CodeBlock>,
    /// Drawn diagrams in `lines`, so a hover or a pan can find one by row.
    /// Like `provenance`, cleared by `set_lines` and restored after it.
    diagrams: Vec<DiagramSpan>,
    links: LinkMap,
    pub search_text: String,
    pub tool_id: Option<String>,
    /// Backlink to `self.messages`. A click on a collapsed thinking indicator
    /// has no tool_id to route by, so this is how the click finds its message,
    /// and it is what makes `segment_source` O(1) rather than a scan per
    /// segment per frame. It looks unused; delete it and the show_thinking
    /// toggle breaks.
    ///
    /// Safe to hold because every path that renumbers `messages` (`replace`,
    /// `remove`, `load_messages`) drops the whole cache.
    pub msg_index: Option<usize>,
    kind: SegmentKind,
    /// Whether this segment was built as a one-line row rather than a card.
    /// An expanded transcript also reaches `ToolInline` for a trivial call
    /// that happens to fit on one line, and that one reads as prose and stays
    /// flat, so the kind alone cannot answer for the background.
    pub compact: bool,
    margin_top: u16,
    pub truncation: bool,
    cached_height: Cell<Option<CachedHeight>>,
    highlights: Vec<HighlightCache>,
    pub spinner_lines: Vec<(usize, usize)>,
    snapshot_base: Option<usize>,
    snapshot_skip: usize,
    pub shell_toggle_line: Option<usize>,
    pub scroll_footer_line: Option<usize>,
    pub scroll_spans: Vec<ScrollSpan>,
    /// What each line belongs to, parallel to `lines`. Spliced alongside them
    /// so a highlighted card keeps the rows a click names.
    rows: Vec<Option<RowTarget>>,
    pub content_indent: &'static str,
    /// Lines were laid out at a width or theme that is no longer current.
    /// Cleared by `set_lines` (whole vector replaced) and up front by
    /// `reflow_segment`; partial splices (`apply_highlight_result`) leave it
    /// set so the segment still reflows later.
    pub(super) stale: bool,
}

impl Segment {
    pub fn with_tool(tool_id: String, kind: SegmentKind, msg_index: Option<usize>) -> Self {
        Self {
            tool_id: Some(tool_id),
            kind,
            msg_index,
            ..Self::default()
        }
    }

    pub fn with_lines(
        lines: Vec<Line<'static>>,
        search_text: String,
        msg_index: Option<usize>,
    ) -> Self {
        Self {
            lines,
            search_text,
            msg_index,
            ..Self::default()
        }
    }

    pub fn lines(&self) -> &[Line<'static>] {
        &self.lines
    }

    pub fn kind(&self) -> SegmentKind {
        self.kind
    }

    pub fn set_kind(&mut self, kind: SegmentKind) {
        if self.kind != kind {
            self.kind = kind;
            self.invalidate_height();
        }
    }

    pub fn set_margin_top(&mut self, margin_top: u16) {
        if self.margin_top != margin_top {
            self.margin_top = margin_top;
            self.invalidate_height();
        }
    }

    pub fn chrome(&self, width: u16) -> SegmentChrome {
        SegmentChrome::for_kind(self.kind, width, self.margin_top)
    }

    pub fn content_width(&self, width: u16) -> u16 {
        self.chrome(width).content_width(width)
    }

    pub fn content_height(&self, width: u16) -> u16 {
        wrapped_line_count(&self.lines, self.content_width(width))
    }

    /// A row that reads as one entry in a list rather than as its own block,
    /// so consecutive ones sit flush instead of being parted by a blank row.
    /// Reasoning is prose the model wrote, not a call it made, so it stays a
    /// block in every mode and never joins the list.
    ///
    /// Deliberately not `content_height(width) <= 1`, which is the same
    /// answer: that clones every span into a `Paragraph` to re-wrap it, and
    /// [`SegmentCache::update_margins`] asks this of every segment several
    /// times a frame. A word wrap never breaks a line that already fits, so
    /// one line is one row exactly when it is no wider than the content box.
    pub fn is_dense_row(&self, width: u16) -> bool {
        if self.kind != SegmentKind::ToolInline || self.lines.len() > 1 {
            return false;
        }
        let Some(line) = self.lines.first() else {
            return true;
        };
        let content_width = self.content_width(width);
        // A zero-width box cannot wrap, which is what `wrapped_line_count`
        // reports by handing back the line count untouched.
        content_width == 0 || line.width() <= content_width as usize
    }

    pub fn provenance(&self) -> Option<&Provenance> {
        self.provenance.as_ref()
    }

    pub fn set_provenance(&mut self, provenance: Option<Provenance>) {
        self.provenance = provenance;
    }

    /// Adopts a card's source, refusing it when its rows do not line up with
    /// the lines they describe: [`Provenance::extract`] would give up anyway,
    /// and holding it would only hide the mismatch.
    fn set_source(&mut self, source: Option<BodySource>) {
        let Some(source) = source.filter(|source| source.rows.len() == self.lines.len()) else {
            self.provenance = None;
            self.code_blocks.clear();
            return;
        };
        self.code_blocks = source.code;
        self.provenance = Some(Provenance::new(Arc::from(source.text), source.rows));
    }

    /// The content rows each of a card's code blocks occupies at `width`, and
    /// what to call its language, so copy can tell a selection that stayed
    /// inside one block from one that ran past it. In row order, which is what
    /// lets a caller fence them as it walks down the selection.
    pub fn code_blocks(&self, width: u16) -> Vec<(Range<u16>, Option<&str>)> {
        let content_start = self.chrome(width).content_start();
        self.code_blocks
            .iter()
            .map(|block| {
                let (first, count) = self.rows_for_lines(block.rows.start, block.rows.len(), width);
                let start = first.saturating_sub(content_start);
                (start..start + count, block.language.as_deref())
            })
            .collect()
    }

    pub fn diagrams(&self) -> &[DiagramSpan] {
        &self.diagrams
    }

    pub fn set_diagrams(&mut self, diagrams: Vec<DiagramSpan>) {
        self.diagrams = diagrams;
    }

    pub fn set_links(&mut self, links: LinkMap) {
        debug_assert!(links.is_aligned(&self.lines));
        self.links = links;
    }

    pub fn links(&self) -> &LinkMap {
        &self.links
    }

    pub fn link_at(&self, rel_row: u16, rel_col: u16, width: u16) -> Option<std::sync::Arc<str>> {
        let chrome = self.chrome(width);
        let row = rel_row.checked_sub(chrome.content_start())?;
        if row >= self.content_height(width) {
            return None;
        }
        let col = rel_col.checked_sub(chrome.left)?;
        let content_width = chrome.content_width(width);
        self.links.target_at(&self.lines, content_width, row, col)
    }

    /// The markdown behind the cell at (`rel_row`, `rel_col`), with the byte it
    /// was painted from. What a click resolves against, since the painter drops
    /// the syntax the glyphs came from.
    pub fn source_at(&self, rel_row: u16, rel_col: u16, width: u16) -> Option<(Arc<str>, u32)> {
        let chrome = self.chrome(width);
        let row = rel_row.checked_sub(chrome.content_start())?;
        if row >= self.content_height(width) {
            return None;
        }
        let col = rel_col.checked_sub(chrome.left)?;
        let provenance = self.provenance.as_ref()?;
        let byte = provenance.byte_at(&self.lines, chrome.content_width(width), row, col)?;
        Some((Arc::clone(provenance.source()), byte))
    }

    /// The diagram drawn on `line`, if any.
    pub fn diagram_at_line(&self, line: usize) -> Option<&DiagramSpan> {
        self.diagrams.iter().find(|span| span.rows.contains(&line))
    }

    pub fn set_lines(&mut self, lines: Vec<Line<'static>>) {
        self.lines = lines;
        self.diagrams.clear();
        self.links = LinkMap::none_for(&self.lines);
        // Line indices moved, so any provenance or row recorded for the old
        // vector no longer lines up.
        self.provenance = None;
        self.code_blocks.clear();
        self.rows.clear();
        self.stale = false;
        self.invalidate_height();
    }

    /// Height in the document layout. While `stale` is set this is the height
    /// measured at an older width, which is what keeps a resize off the
    /// O(transcript) path: re-measuring means re-wrapping every line.
    ///
    /// Render, `segment_at_row` and the scrollbar all read this same number so
    /// the layout stays self consistent, and `reflow_viewport` keeps every
    /// segment the viewport can reach fresh. A caller that re-wraps the lines
    /// itself has to use `drawn_height`, or it disagrees with the layout by
    /// however much the width moved.
    pub fn height(&self, width: u16) -> u16 {
        if let Some(c) = self.cached_height.get()
            && (c.at_width == width || self.stale)
        {
            return c.height;
        }
        let h = self.drawn_height(width);
        self.cached_height.set(Some(CachedHeight {
            at_width: width,
            height: h,
        }));
        h
    }

    /// Rows the lines really take at `width`, ignoring the cache. Same as
    /// `height` for any segment that is not stale.
    pub fn drawn_height(&self, width: u16) -> u16 {
        let chrome = self.chrome(width);
        chrome
            .content_start()
            .saturating_add(self.content_height(width))
            .saturating_add(chrome.bottom)
    }

    /// Maps a display row (after wrapping) back to the source line index.
    pub fn source_line_at(&self, rel_row: u16, width: u16) -> Option<usize> {
        let chrome = self.chrome(width);
        let rel_row = rel_row.checked_sub(chrome.content_start())?;
        if rel_row >= self.content_height(width) {
            return None;
        }
        let width = chrome.content_width(width);
        let mut acc = 0u16;
        for (i, line) in self.lines.iter().enumerate() {
            acc = acc.saturating_add(wrapped_line_count(std::slice::from_ref(line), width));
            if rel_row < acc {
                return Some(i);
            }
        }
        None
    }

    /// The rows a run of source lines occupies after wrapping, relative to the
    /// segment's own first row. The inverse of [`Self::source_line_at`], and
    /// the only place a window's geometry is derived, so a bar drawn beside a
    /// body cannot disagree with the body by a row.
    pub fn rows_for_lines(&self, first: usize, count: usize, width: u16) -> (u16, u16) {
        let chrome = self.chrome(width);
        let inner = chrome.content_width(width);
        let first = first.min(self.lines.len());
        let end = (first + count).min(self.lines.len());
        let rows = |range: &[Line<'static>]| wrapped_line_count(range, inner);
        (
            chrome.content_start() + rows(&self.lines[..first]),
            rows(&self.lines[first..end]),
        )
    }

    /// What the row at `rel_row` belongs to, for a click or a hover to name.
    pub fn row_target_at(&self, rel_row: u16, width: u16) -> Option<RowTarget> {
        let line = self.source_line_at(rel_row, width)?;
        self.rows.get(line).copied().flatten()
    }

    /// The line that names the control at `rel_row`: the first of the run
    /// carrying the same target. A child is folded or whole, so its summary
    /// row and its body are one control, and the row the pointer marks has to
    /// be that same one from anywhere inside it.
    ///
    /// Scans back from the hovered line rather than from the start of the
    /// segment. A control's lines are contiguous, so this costs the control's
    /// own height rather than the whole card's, on every pointer move.
    pub fn control_line_at(&self, rel_row: u16, width: u16) -> Option<usize> {
        let line = self.source_line_at(rel_row, width)?;
        let target = self.rows.get(line).copied().flatten()?;
        Some(
            (0..line)
                .rev()
                .take_while(|earlier| self.rows[*earlier] == Some(target))
                .last()
                .unwrap_or(line),
        )
    }

    /// Maps a source line to a 1-based row in the tool's live buffer, or 0
    /// for lines outside it (header etc.). The Lua click-row contract is
    /// computed here and nowhere else, from the base recorded when the
    /// buffer snapshot was laid out.
    pub fn buf_row(&self, source_line: usize) -> usize {
        match self.snapshot_base {
            Some(base) if source_line >= base => source_line - base + self.snapshot_skip + 1,
            _ => 0,
        }
    }

    fn invalidate_height(&self) {
        self.cached_height.set(None);
    }

    pub fn update_spinners(&mut self, span: &Span<'static>) {
        for &(line_idx, span_idx) in &self.spinner_lines {
            if let Some(line) = self.lines.get_mut(line_idx)
                && line.spans.len() > span_idx
            {
                line.spans[span_idx] = span.clone();
            }
        }
    }

    pub fn apply_highlight(&mut self, tl: ToolLines, worker: &RenderWorker, compact: bool) {
        self.highlights.clear();
        self.update_with_reuse(tl, worker, compact);
    }

    pub fn update_with_reuse(&mut self, mut tl: ToolLines, worker: &RenderWorker, compact: bool) {
        self.compact = compact;
        self.set_kind(tool_kind(self.kind, &tl, compact));
        let mut previous = std::mem::take(&mut self.highlights);
        let generation = theme::generation();
        for request in tl.highlight.drain(..) {
            let cached = previous.iter().position(|cached| {
                cached.theme_gen == generation
                    && (cached.request.matches(&request)
                        || cached.request.append_compatible(&request))
            });
            let mut cached = match cached {
                Some(index) => previous.swap_remove(index),
                None => HighlightCache {
                    pending: None,
                    request: request.clone(),
                    theme_gen: generation,
                    paint: None,
                    local_source_rows: None,
                },
            };
            cached.request = request;
            cached.local_source_rows = None;
            self.highlights.push(cached);
        }
        self.spinner_lines = tl.spinner_lines;
        self.snapshot_base = tl.snapshot_base;
        self.snapshot_skip = tl.snapshot_skip;
        self.shell_toggle_line = tl.shell_toggle_line;
        self.scroll_footer_line = tl.scroll_footer_line;
        self.scroll_spans = tl.scroll_spans;
        self.content_indent = tl.content_indent;
        self.truncation = tl.truncation;
        self.set_lines(tl.lines);
        self.set_links(tl.links);
        self.rows = tl.rows;
        self.set_source(tl.source);
        for index in 0..self.highlights.len() {
            if let Some(paint) = self.highlights[index].paint.clone() {
                self.paint_cached(index, &paint);
            }
            self.schedule_highlight(index, worker);
        }
    }

    fn schedule_highlight(&mut self, index: usize, worker: &RenderWorker) {
        let cached = &mut self.highlights[index];
        if cached.pending.is_none()
            && cached
                .paint
                .as_ref()
                .is_none_or(|paint| !paint.request.matches(&cached.request))
        {
            cached.pending = Some(PendingHighlight {
                id: worker.send(cached.request.clone()),
                request: cached.request.clone(),
                text: self.lines[cached.request.region.range.clone()]
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
            });
        }
    }

    fn paint_cached(&mut self, index: usize, paint: &HighlightPaint) -> bool {
        let request = &self.highlights[index].request;
        let range = request.region.range.clone();
        if paint.request.matches(request) {
            self.paint_highlight(range, paint)
        } else if paint.request.append_compatible(request) {
            let source = request.input_source();
            match (&paint.syntax, source) {
                (Some(syntax), Some(source)) => self.paint_source(index, &source, syntax),
                _ => false,
            }
        } else {
            false
        }
    }

    pub fn matches_pending_highlight(&self, id: u64) -> bool {
        self.highlights.iter().any(|cached| {
            cached
                .pending
                .as_ref()
                .is_some_and(|pending| pending.id == id)
        })
    }

    pub fn apply_highlight_result(
        &mut self,
        id: u64,
        lines: Vec<Line<'static>>,
        rows: Vec<Option<RowTarget>>,
        source_rows: Option<Vec<LineProvenance>>,
        worker: &RenderWorker,
    ) {
        let Some(index) = self.highlights.iter().position(|cached| {
            cached
                .pending
                .as_ref()
                .is_some_and(|pending| pending.id == id)
        }) else {
            return;
        };
        let Some(pending) = self.highlights[index].pending.take() else {
            return;
        };
        if self.stale || self.highlights[index].theme_gen != theme::generation() {
            return;
        }
        if rows.len() == lines.len()
            && lines.len() == pending.text.len()
            && lines.iter().zip(&pending.text).all(|(line, text)| {
                line.spans
                    .iter()
                    .flat_map(|span| span.content.bytes())
                    .eq(text.bytes())
            })
        {
            let syntax = source_rows
                .as_ref()
                .and_then(|rows| SourcePaint::new(&pending.request, &lines, rows));
            let paint = HighlightPaint {
                request: pending.request.clone(),
                lines,
                source_rows,
                syntax,
            };
            if self.paint_cached(index, &paint) {
                self.highlights[index].paint = Some(Arc::new(paint));
            }
        }
        if !pending.request.matches(&self.highlights[index].request) {
            self.schedule_highlight(index, worker);
        }
    }

    fn paint_source(&mut self, index: usize, source: &str, paint: &SourcePaint) -> bool {
        if !source.starts_with(&paint.source) || !self.links.is_aligned(&self.lines) {
            return false;
        }
        let cached = &mut self.highlights[index];
        let range = cached.request.region.range.clone();
        let (mut rows, base) = if let Some(provenance) = &self.provenance {
            let Some(rows) = provenance.lines_in(range.clone()) else {
                return false;
            };
            let Some(base) = self.code_blocks.iter().find_map(|block| {
                (block.rows.start <= range.start
                    && block.rows.end >= range.end
                    && provenance
                        .source()
                        .get(block.source.start as usize..block.source.end as usize)
                        == Some(source))
                .then_some(block.source.start)
            }) else {
                return false;
            };
            (rows, base)
        } else if let Some(rows) = cached.local_source_rows.take() {
            (rows, 0)
        } else {
            let Some(input) = cached.request.sources().0 else {
                return false;
            };
            let fallback = cached.request.region.render_fallback(input);
            let Some(local) = fallback.source else {
                return false;
            };
            let current = &self.lines[range.clone()];
            if local.text != source
                || fallback.lines.len() != current.len()
                || !fallback.lines.iter().zip(current).all(|(left, right)| {
                    left.spans.len() == right.spans.len()
                        && left
                            .spans
                            .iter()
                            .zip(&right.spans)
                            .all(|(left, right)| left.content == right.content)
                })
            {
                return false;
            }
            (local.rows, 0)
        };
        if rows.len() != range.len()
            || rows
                .iter()
                .zip(&self.lines[range.clone()])
                .any(|(row, line)| row.spans.len() != line.spans.len())
        {
            return false;
        }
        for (index, row) in range.clone().zip(&mut rows) {
            let mut spans = Vec::new();
            let mut origins = Vec::new();
            let mut links = Vec::new();
            for ((span, origin), link) in self.lines[index]
                .spans
                .iter()
                .zip(&row.spans)
                .zip(&self.links.rows[index])
            {
                let mut push = |text: &str, style, origin| {
                    spans.push(Span::styled(text.to_owned(), style));
                    origins.push(origin);
                    links.push(link.clone());
                };
                let SpanSource::Range(mapped) = origin else {
                    push(&span.content, span.style, origin.clone());
                    continue;
                };
                let Some(start) = mapped.range.start.checked_sub(base) else {
                    push(&span.content, span.style, origin.clone());
                    continue;
                };
                let first = paint
                    .styles
                    .partition_point(|(range, _)| range.end <= start);
                if !mapped.verbatim {
                    let style = paint
                        .styles
                        .get(first)
                        .filter(|(range, _)| {
                            range.start <= start && range.end >= mapped.range.end - base
                        })
                        .map_or(span.style, |(_, style)| *style);
                    push(&span.content, style, origin.clone());
                    continue;
                }
                let mut taken = 0;
                for (range, style) in &paint.styles[first..] {
                    let from = range.start.saturating_sub(start) as usize;
                    if from >= span.content.len() {
                        break;
                    }
                    let to = ((range.end - start) as usize).min(span.content.len());
                    let from = from.max(taken);
                    if from > taken {
                        push(
                            &span.content[taken..from],
                            span.style,
                            SpanSource::Range(Source::verbatim(
                                mapped.range.start + taken as u32..mapped.range.start + from as u32,
                            )),
                        );
                    }
                    if to > from {
                        let end = if to == span.content.len() {
                            mapped.range.end
                        } else {
                            mapped.range.start + to as u32
                        };
                        push(
                            &span.content[from..to],
                            *style,
                            SpanSource::Range(Source::verbatim(
                                mapped.range.start + from as u32..end,
                            )),
                        );
                    }
                    taken = to.max(taken);
                }
                if taken < span.content.len() || span.content.is_empty() {
                    push(
                        &span.content[taken..],
                        span.style,
                        SpanSource::Range(Source::verbatim(
                            mapped.range.start + taken as u32..mapped.range.end,
                        )),
                    );
                }
            }
            self.lines[index].spans = spans;
            self.links.rows[index] = links;
            row.spans = origins;
        }
        if let Some(provenance) = &mut self.provenance {
            provenance.splice_lines(range, rows)
        } else {
            self.highlights[index].local_source_rows = Some(rows);
            true
        }
    }

    fn paint_highlight(&mut self, range: Range<usize>, paint: &HighlightPaint) -> bool {
        let Some(current) = self.lines.get(range.clone()) else {
            return false;
        };
        if current.len() != paint.lines.len()
            || !current.iter().zip(&paint.lines).all(|(left, right)| {
                left.spans
                    .iter()
                    .flat_map(|span| span.content.bytes())
                    .eq(right.spans.iter().flat_map(|span| span.content.bytes()))
            })
            || !self.links.is_aligned(&self.lines)
            || self.links.rows[range.clone()]
                .iter()
                .flatten()
                .any(Option::is_some)
        {
            return false;
        }
        if let Some(provenance) = self.provenance.as_mut() {
            let Some(current_rows) = provenance.lines_in(range.clone()) else {
                return false;
            };
            let Some(mut source_rows) = paint.source_rows.clone().filter(|rows| {
                rows.len() == current_rows.len()
                    && rows
                        .iter()
                        .zip(&paint.lines)
                        .all(|(row, line)| row.spans.len() == line.spans.len())
            }) else {
                return false;
            };
            for (row, current) in source_rows.iter_mut().zip(current_rows) {
                match (&row.line, &current.line) {
                    (Some(old), Some(new)) if old.end - old.start == new.end - new.start => {
                        let offset = i64::from(new.start) - i64::from(old.start);
                        for span in &mut row.spans {
                            if let SpanSource::Range(source) = span {
                                let Ok(start) =
                                    u32::try_from(i64::from(source.range.start) + offset)
                                else {
                                    return false;
                                };
                                let Ok(end) = u32::try_from(i64::from(source.range.end) + offset)
                                else {
                                    return false;
                                };
                                source.range = start..end;
                            }
                        }
                        row.line = current.line;
                    }
                    (None, None) => {}
                    _ => return false,
                }
            }
            if !provenance.splice_lines(range.clone(), source_rows) {
                return false;
            }
        }
        self.links
            .rows
            .splice(range.clone(), LinkMap::none_for(&paint.lines).rows);
        self.lines.splice(range, paint.lines.clone());
        true
    }

    #[cfg(test)]
    pub fn settle_highlight(&mut self) {
        for cached in &mut self.highlights {
            cached.pending = None;
        }
    }

    #[cfg(test)]
    pub fn has_pending_highlight(&self) -> bool {
        self.highlights
            .iter()
            .any(|cached| cached.pending.is_some())
    }
}

/// Compact rows are always inline: the kind is what strips the card rail and
/// the padding that would otherwise separate every call by a blank line.
fn tool_kind(current: SegmentKind, lines: &ToolLines, compact: bool) -> SegmentKind {
    if compact {
        return SegmentKind::ToolInline;
    }
    if current == SegmentKind::Instruction {
        return current;
    }
    if lines.lines.len() == 1
        && lines.highlight.is_empty()
        && lines.snapshot_base.is_none()
        && !lines.truncation
    {
        SegmentKind::ToolInline
    } else {
        SegmentKind::ToolBlock
    }
}

pub(super) struct SegmentCache {
    segments: Vec<Segment>,
    msg_count: usize,
}

impl SegmentCache {
    pub fn new() -> Self {
        Self {
            segments: Vec::new(),
            msg_count: 0,
        }
    }

    pub fn clear(&mut self) {
        self.segments.clear();
        self.msg_count = 0;
    }

    pub fn push(&mut self, seg: Segment) {
        self.segments.push(seg);
    }

    pub fn insert(&mut self, pos: usize, seg: Segment) {
        self.segments.insert(pos, seg);
    }

    pub fn needs_rebuild(&self, msg_len: usize) -> bool {
        self.msg_count != msg_len
    }

    pub fn mark_built(&mut self, count: usize) {
        self.msg_count = count;
    }

    pub fn msg_count(&self) -> usize {
        self.msg_count
    }

    pub fn total_height(&self, width: u16) -> u32 {
        self.segments.iter().map(|s| s.height(width) as u32).sum()
    }

    pub fn segment_at_row(&self, doc_row: u32, width: u16) -> Option<(usize, &Segment, u32)> {
        let mut cumulative: u32 = 0;
        for (i, seg) in self.segments.iter().enumerate() {
            let seg_start = cumulative;
            cumulative += seg.height(width) as u32;
            if doc_row < cumulative {
                return Some((i, seg, seg_start));
            }
        }
        None
    }

    /// The segment holding `doc_row` and the row's offset inside it. Survives
    /// a reflow, which a bare document-line offset does not.
    pub fn anchor_at(&self, doc_row: u32, width: u16) -> Option<(usize, u16)> {
        let (i, _, start) = self.segment_at_row(doc_row, width)?;
        Some((i, (doc_row - start).min(u16::MAX as u32) as u16))
    }

    /// Where `anchor` sits now, clamped in case the reflow shrank the segment
    /// it points into.
    pub fn anchor_offset(&self, (idx, rel): (usize, u16), width: u16) -> u32 {
        let before: u32 = self
            .segments
            .iter()
            .take(idx)
            .map(|s| s.height(width) as u32)
            .sum();
        let h = self.segments.get(idx).map_or(0, |s| s.height(width));
        before + rel.min(h.saturating_sub(1)) as u32
    }

    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    pub fn segments_mut(&mut self) -> &mut [Segment] {
        &mut self.segments
    }

    pub fn get(&self, idx: usize) -> Option<&Segment> {
        self.segments.get(idx)
    }

    pub fn get_mut(&mut self, idx: usize) -> Option<&mut Segment> {
        self.segments.get_mut(idx)
    }

    pub fn find_by_tool_id(&self, id: &str) -> Option<usize> {
        self.segments
            .iter()
            .rposition(|s| s.tool_id.as_deref() == Some(id))
    }

    pub fn len(&self) -> usize {
        self.segments.len()
    }

    /// Single-line tool rows read as a list, so they sit flush against each
    /// other. A row that wraps or carries a body has stopped being a list
    /// entry and is given air on both sides, or it runs into its neighbours
    /// and the eye cannot tell where one call ends and the next begins.
    ///
    /// Runs several times a frame, so it must stay proportional to the
    /// transcript with a trivial constant: see [`Segment::is_dense_row`].
    pub fn update_margins(&mut self, width: u16) {
        let mut previous: Option<bool> = None;
        for segment in &mut self.segments {
            let dense = segment.is_dense_row(width);
            let margin = previous.map_or(0, |was_dense| u16::from(!(was_dense && dense)));
            segment.set_margin_top(margin);
            previous = Some(dense);
        }
    }

    pub fn search_texts(&self) -> Vec<&str> {
        self.segments
            .iter()
            .map(|s| s.search_text.as_str())
            .collect()
    }

    pub fn mark_all_width_stale(&mut self) {
        for seg in &mut self.segments {
            seg.stale = true;
        }
    }
}

pub(super) fn wrapped_line_count(lines: &[Line<'_>], width: u16) -> u16 {
    if width == 0 {
        return lines.len() as u16;
    }
    lines.iter().fold(0, |rows, line| {
        rows.saturating_add(wrapped_rows(line, width))
    })
}

/// A line no wider than the box takes exactly one row, and once the renderer
/// has broken code blocks and tables to width that is most of them. Asking
/// ratatui costs a clone and a full re-wrap, so only an overflowing row pays
/// for one: measuring a screenful used to cost more than rendering it.
///
/// `Line::width` and the wrapper both sum `unicode_width` per character, so a
/// line this accepts cannot be one the wrapper would split. A grapheme whose
/// parts measure wider than the cluster only overstates the width, which falls
/// through to the wrapper and is merely slower.
fn wrapped_rows(line: &Line<'_>, width: u16) -> u16 {
    if line.width() <= width as usize {
        return 1;
    }
    Paragraph::new(line.clone())
        .wrap(Wrap { trim: false })
        .line_count(width) as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::code_view::{self, RenderLimits, SourceTrace};
    use caudra_agent::ToolInput;
    use test_case::test_case;
    use unicode_width::UnicodeWidthStr;

    const WIDTH: u16 = 40;
    const NARROW_WIDTH: u16 = 24;
    /// What [`SegmentChrome`] leaves an inline row at [`WIDTH`], pinned by
    /// `inline_content_width_tracks_the_chrome` because the boundary cases
    /// below stop testing a boundary the moment it drifts.
    const INLINE_CONTENT_WIDTH: usize = 36;
    /// One call's worth of streaming: a summary line, an annotation, a live
    /// body, the authoritative summary `tool_start` restores, then output.
    const STREAMED_LINE_COUNTS: [usize; 5] = [1, 2, 9, 1, 12];
    const EXPECT_DENSE_AGREES: &str =
        "the wrap-free dense test must answer exactly what re-wrapping would";
    const EXPECT_ONE_WRAP_WIDTH: &str =
        "a card must wrap to one width while its line count crosses one";
    const EXPECT_KIND_STILL_FLIPS: &str =
        "the inline/block flip is the movement this test exists to survive";
    const EXPECT_ORACLE_AGREES: &str =
        "skipping the wrapper must answer exactly what the wrapper would";
    const MEASURE_WIDTHS: [u16; 6] = [1, 2, 7, 20, 40, 100];

    /// What [`wrapped_line_count`] replaced: every line handed to ratatui,
    /// whatever its width. Kept here so the fast path is checked against the
    /// wrapper itself rather than against hand-counted numbers.
    fn reference_rows(lines: &[Line<'_>], width: u16) -> u16 {
        Paragraph::new(lines.to_vec())
            .wrap(Wrap { trim: false })
            .line_count(width) as u16
    }

    /// Shapes that between them cover every branch the wrapper takes: nothing
    /// to wrap, the exact boundary either side, whitespace the `trim: false`
    /// wrapper has to keep, a word longer than the box, and characters whose
    /// display width is not their byte count.
    fn measure_corpus(width: u16) -> Vec<Vec<Line<'static>>> {
        let w = width as usize;
        let shapes: Vec<Line<'static>> = vec![
            Line::from(""),
            Line::from(" "),
            Line::from("short"),
            Line::from("x".repeat(w)),
            Line::from("x".repeat(w + 1)),
            Line::from(format!("{} ", "x".repeat(w.saturating_sub(1)))),
            Line::from(format!("{}  ", "x".repeat(w))),
            Line::from("word ".repeat(w)),
            Line::from("   leading and then a long run of ordinary words ".repeat(3)),
            Line::from("日本語のテキスト".repeat(4)),
            Line::from("👨‍👩‍👦 family and more text after it".to_owned()),
            Line::from(vec![
                Span::raw("a ".repeat(w / 2)),
                Span::raw("b ".repeat(w / 2)),
            ]),
        ];
        let mut cases: Vec<Vec<Line<'static>>> = shapes.iter().cloned().map(|l| vec![l]).collect();
        cases.push(shapes);
        cases
    }

    #[test_case(MEASURE_WIDTHS[0] ; "width_1")]
    #[test_case(MEASURE_WIDTHS[1] ; "width_2")]
    #[test_case(MEASURE_WIDTHS[2] ; "width_7")]
    #[test_case(MEASURE_WIDTHS[3] ; "width_20")]
    #[test_case(MEASURE_WIDTHS[4] ; "width_40")]
    #[test_case(MEASURE_WIDTHS[5] ; "width_100")]
    fn wrapped_line_count_matches_the_wrapper(width: u16) {
        for case in measure_corpus(width) {
            assert_eq!(
                wrapped_line_count(&case, width),
                reference_rows(&case, width),
                "{EXPECT_ORACLE_AGREES}: {case:?} at width {width}"
            );
        }
    }

    fn seg_with_base(line_count: usize, base: Option<usize>) -> Segment {
        Segment {
            lines: (0..line_count)
                .map(|i| Line::raw(format!("l{i}")))
                .collect(),
            snapshot_base: base,
            ..Segment::default()
        }
    }

    fn inline_tool(text: String) -> Segment {
        let mut segment = Segment::with_lines(vec![Line::raw(text)], String::new(), None);
        segment.set_kind(SegmentKind::ToolInline);
        segment
    }

    #[test]
    fn consecutive_single_line_tools_stay_dense() {
        let mut cache = SegmentCache::new();
        cache.push(inline_tool("first".into()));
        cache.push(inline_tool("second".into()));

        cache.update_margins(80);

        assert_eq!(cache.segments[0].margin_top, 0);
        assert_eq!(cache.segments[1].margin_top, 0);
    }

    /// `is_dense_row` is the allocation-free spelling of "this inline row
    /// draws as one row". Let the two drift and margins stop agreeing with
    /// the heights the layout is measured from.
    #[test_case(SegmentKind::ToolInline, 1 ; "inline_fits")]
    #[test_case(SegmentKind::ToolInline, INLINE_CONTENT_WIDTH ; "inline_exactly_fills")]
    #[test_case(SegmentKind::ToolInline, INLINE_CONTENT_WIDTH + 1 ; "inline_overflows_by_one")]
    #[test_case(SegmentKind::ToolInline, 0 ; "inline_empty")]
    #[test_case(SegmentKind::ToolBlock, 1 ; "block_is_never_dense")]
    #[test_case(SegmentKind::Assistant, 1 ; "prose_is_never_dense")]
    fn is_dense_row_agrees_with_the_wrapped_height(kind: SegmentKind, text_len: usize) {
        let mut segment =
            Segment::with_lines(vec![Line::raw("x".repeat(text_len))], String::new(), None);
        segment.set_kind(kind);

        let by_wrap = kind == SegmentKind::ToolInline && segment.content_height(WIDTH) <= 1;
        assert_eq!(
            segment.is_dense_row(WIDTH),
            by_wrap,
            "{EXPECT_DENSE_AGREES}"
        );
    }

    #[test]
    fn a_multi_line_inline_row_is_not_dense() {
        let mut segment =
            Segment::with_lines(vec![Line::raw("a"), Line::raw("b")], String::new(), None);
        segment.set_kind(SegmentKind::ToolInline);

        assert!(segment.content_height(WIDTH) > 1);
        assert!(!segment.is_dense_row(WIDTH), "{EXPECT_DENSE_AGREES}");
    }

    fn streamed_tool_lines(count: usize) -> ToolLines {
        let lines: Vec<Line<'static>> = (0..count).map(|i| Line::raw(format!("l{i}"))).collect();
        ToolLines {
            links: LinkMap::none_for(&lines),
            lines,
            search_text: String::new(),
            highlight: Vec::new(),
            spinner_lines: Vec::new(),
            snapshot_base: None,
            snapshot_skip: 0,
            shell_toggle_line: None,
            scroll_footer_line: None,
            scroll_spans: Vec::new(),
            content_indent: "",
            rows: Vec::new(),
            truncation: false,
            source: None,
        }
    }

    #[test]
    fn inline_content_width_tracks_the_chrome() {
        assert_eq!(
            SegmentChrome::for_kind(SegmentKind::ToolInline, WIDTH, 0).content_width(WIDTH),
            INLINE_CONTENT_WIDTH as u16
        );
    }

    /// The jitter this guards against: a card crosses one logical line several
    /// times per call, and it used to rewrap everything already on screen each
    /// time it did. The kind is still free to flip; the width is not.
    #[test_case(WIDTH ; "width_40")]
    #[test_case(NARROW_WIDTH ; "narrow")]
    fn a_streaming_card_keeps_one_wrap_width(width: u16) {
        let mut kind = SegmentKind::ToolBlock;
        let mut seen = Vec::new();

        for count in STREAMED_LINE_COUNTS {
            kind = tool_kind(kind, &streamed_tool_lines(count), false);
            seen.push((
                kind,
                SegmentChrome::for_kind(kind, width, 0).content_width(width),
            ));
        }

        assert!(
            seen.iter().any(|(k, _)| *k == SegmentKind::ToolInline)
                && seen.iter().any(|(k, _)| *k == SegmentKind::ToolBlock),
            "{EXPECT_KIND_STILL_FLIPS}: {seen:?}"
        );
        assert!(
            seen.iter().all(|(_, w)| *w == seen[0].1),
            "{EXPECT_ONE_WRAP_WIDTH}: {seen:?}"
        );
    }

    #[test]
    fn wrapped_tool_separates_the_next_inline_tool() {
        let mut cache = SegmentCache::new();
        cache.push(inline_tool("x".repeat(80)));
        cache.push(inline_tool("next".into()));

        cache.update_margins(40);

        assert_eq!(cache.segments[1].margin_top, 1);
    }

    #[test]
    fn wrapped_inline_tool_is_separated_from_the_previous_tool() {
        let mut cache = SegmentCache::new();
        cache.push(inline_tool("first".into()));
        cache.push(inline_tool("x".repeat(80)));

        cache.update_margins(40);

        assert_eq!(cache.segments[1].margin_top, 1);
    }

    #[test_case(0, 0 ; "header_maps_to_zero")]
    #[test_case(1, 1 ; "first_snapshot_line_is_row_one")]
    #[test_case(4, 4 ; "later_line_offsets_from_base")]
    fn buf_row_maps_source_lines_through_snapshot_base(source_line: usize, expected: usize) {
        let seg = seg_with_base(5, Some(1));
        assert_eq!(seg.buf_row(source_line), expected);
    }

    #[test]
    fn buf_row_is_zero_without_snapshot() {
        let seg = seg_with_base(3, None);
        assert_eq!(seg.buf_row(2), 0);
    }

    #[test]
    fn buf_row_tracks_base_when_lines_precede_snapshot() {
        let seg = seg_with_base(6, Some(3));
        assert_eq!(seg.buf_row(2), 0, "pre-snapshot lines map outside the buf");
        assert_eq!(seg.buf_row(3), 1);
        assert_eq!(seg.buf_row(5), 3);
    }

    fn code_tool_lines(code: &str, prefix: &str, tail: &str, width: u16) -> ToolLines {
        let input = Arc::new(ToolInput::Code {
            language: "rust".into(),
            code: code.into(),
        });
        let content = code_view::render_tool_content(
            Some(&input),
            None,
            false,
            RenderLimits::default().with_width(width),
        );
        let (mut lines, source) = code_view::plain_body(prefix, width);
        let mut trace = SourceTrace::default();
        trace.record(0, source);
        let start = lines.len();
        trace.record(start, content.source.unwrap());
        lines.extend(content.lines);
        let (tail_lines, source) = code_view::plain_body(tail, width);
        trace.record(lines.len(), source);
        lines.extend(tail_lines);
        let mut tl = streamed_tool_lines(0);
        tl.source = trace.finish(&lines);
        tl.links = LinkMap::none_for(&lines);
        tl.rows = vec![Some(RowTarget::Item(0)); lines.len()];
        tl.lines = lines;
        tl.highlight = content
            .highlights
            .into_iter()
            .map(|mut region| {
                region.shift(start);
                HighlightRequest {
                    region,
                    input: Some(input.clone()),
                    output: None,
                }
            })
            .collect();
        tl
    }

    const CODE: &str = "fn main() { println!(\"hello\"); }";
    const CHANGED_CODE: &str = "fn changed() {}";
    const PREFIX: &str = "header";
    const MOVED_PREFIX: &str = "a longer header\nsecond row";
    const LIVE_BEFORE: &str = "before";
    const LIVE_AFTER: &str = "after";
    const EXPECT_PENDING: &str = "unchanged syntax must retain its pending request";
    const EXPECT_FRESH: &str = "highlighting must not replace current live rows";
    const EXPECT_SHAPE: &str = "highlighting cannot change text, height, or targets";
    const EXPECT_PAINT: &str = "syntax colors must be applied";
    const EXPECT_COALESCED: &str = "only the in-flight request and newest desired source survive";
    const STREAM_CODE: &str = "fn main() {\n\tlet café = \"日本語  ";
    const APPENDS: &[&str] = &["hello", " world\";", "\n\tprintln!(\"{café}\");\n}"];
    const CURRENT_LINK: &str = "current-source-link";

    fn deliver_next(seg: &mut Segment, worker: &RenderWorker) {
        let result = worker.render_next();
        seg.apply_highlight_result(
            result.id,
            result.lines,
            result.rows,
            result.source_rows,
            worker,
        );
    }

    fn input_lines(code: &str, script: bool, prefix: &str, width: u16) -> ToolLines {
        let mut lines = code_tool_lines(code, prefix, LIVE_AFTER, width);
        if script {
            for request in &mut lines.highlight {
                request.input = Some(Arc::new(ToolInput::Script {
                    language: "rust".into(),
                    code: code.into(),
                }));
            }
        }
        lines
    }

    fn assert_source_frame(seg: &Segment, fallback: &ToolLines, paint: &SourcePaint, width: u16) {
        assert_eq!(
            seg.lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            fallback
                .lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            "{EXPECT_SHAPE}"
        );
        assert_eq!(seg.rows, fallback.rows, "{EXPECT_SHAPE}");
        assert_eq!(
            seg.code_blocks
                .iter()
                .map(|block| (&block.rows, &block.source, &block.language))
                .collect::<Vec<_>>(),
            fallback
                .source
                .as_ref()
                .unwrap()
                .code
                .iter()
                .map(|block| (&block.rows, &block.source, &block.language))
                .collect::<Vec<_>>()
        );
        let provenance = seg.provenance.as_ref().unwrap();
        let source = fallback.source.as_ref().unwrap();
        assert_eq!(provenance.source().as_ref(), source.text);
        assert!(seg.links.is_aligned(&seg.lines));
        let region = &seg.highlights[0].request.region;
        let base = source
            .code
            .iter()
            .find(|block| {
                block.rows.start <= region.range.start && block.rows.end >= region.range.end
            })
            .unwrap()
            .source
            .start;
        let rows = provenance.lines_in(region.range.clone()).unwrap();
        let mut retained = 0;
        let mut neutral = 0;
        for (index, row) in region.range.clone().zip(rows) {
            assert_eq!(row.line, source.rows[index].line);
            let mut column = 0;
            for (span_index, (span, origin)) in
                seg.lines[index].spans.iter().zip(row.spans).enumerate()
            {
                if let SpanSource::Range(origin) = origin {
                    assert_eq!(
                        seg.links.rows[index][span_index].as_deref(),
                        Some(CURRENT_LINK)
                    );
                    for (offset, ch) in span.content.char_indices() {
                        let byte =
                            origin.range.start + if origin.verbatim { offset as u32 } else { 0 };
                        assert_eq!(
                            provenance.byte_at(&seg.lines, width, index as u16, column as u16),
                            origin.verbatim.then_some(byte)
                        );
                        if let Some((_, style)) = paint
                            .styles
                            .iter()
                            .find(|(range, _)| range.contains(&(byte - base)))
                        {
                            assert_eq!(span.style, *style, "{EXPECT_PAINT}");
                            retained += 1;
                        } else if byte - base >= paint.source.len() as u32 {
                            assert_eq!(span.style, theme::current().code_block, "{EXPECT_PAINT}");
                            neutral += 1;
                        }
                        column += UnicodeWidthStr::width(ch.to_string().as_str());
                    }
                } else {
                    column += UnicodeWidthStr::width(span.content.as_ref());
                }
            }
        }
        assert!(retained > 0, "{EXPECT_PAINT}");
        assert!(neutral > 0, "{EXPECT_PAINT}");
    }

    fn linked_input_lines(code: &str, script: bool, prefix: &str, width: u16) -> ToolLines {
        let mut lines = input_lines(code, script, prefix, width);
        let source = lines.source.as_ref().unwrap();
        for (row, links) in source.rows.iter().zip(&mut lines.links.rows) {
            for (origin, link) in row.spans.iter().zip(links) {
                if matches!(origin, SpanSource::Range(_)) {
                    *link = Some(Arc::from(CURRENT_LINK));
                }
            }
        }
        lines
    }

    #[test_case(false; "code")]
    #[test_case(true; "script")]
    fn append_frames_preserve_source_styles_through_unicode_tabs_and_wrapping(script: bool) {
        let worker = RenderWorker::manual();
        let mut seg = Segment::default();
        seg.apply_highlight(
            input_lines(STREAM_CODE, script, PREFIX, NARROW_WIDTH),
            &worker,
            false,
        );
        let before = seg.lines.clone();
        deliver_next(&mut seg, &worker);
        assert_ne!(seg.lines, before, "{EXPECT_PAINT}");
        let paint = seg.highlights[0].paint.clone().unwrap();
        let syntax = paint.syntax.as_ref().unwrap();
        let mut code = STREAM_CODE.to_owned();
        let mut pending = None;
        for append in APPENDS {
            code.push_str(append);
            seg.update_with_reuse(
                linked_input_lines(&code, script, MOVED_PREFIX, NARROW_WIDTH),
                &worker,
                false,
            );
            let fallback = linked_input_lines(&code, script, MOVED_PREFIX, NARROW_WIDTH);
            assert_source_frame(&seg, &fallback, syntax, NARROW_WIDTH);
            assert_eq!(worker.queued(), 1, "{EXPECT_COALESCED}");
            let id = seg.highlights[0].pending.as_ref().unwrap().id;
            assert_eq!(*pending.get_or_insert(id), id, "{EXPECT_COALESCED}");
        }
    }

    #[test_case(false, false; "code")]
    #[test_case(true, false; "script")]
    #[test_case(false, true; "clipped_code")]
    fn append_paint_without_card_provenance_matches_traced_frames(script: bool, clipped: bool) {
        let build = |code: &str, linked: bool| {
            if clipped {
                clipped_input_lines(code, linked)
            } else if linked {
                linked_input_lines(code, script, MOVED_PREFIX, NARROW_WIDTH)
            } else {
                input_lines(code, script, MOVED_PREFIX, NARROW_WIDTH)
            }
        };
        let mut traced = Segment::default();
        let mut untraced = Segment::default();
        let traced_worker = RenderWorker::manual();
        let untraced_worker = RenderWorker::manual();
        let mut initial = build(STREAM_CODE, false);
        let plain = initial.lines.clone();
        initial.source = None;
        untraced.apply_highlight(initial, &untraced_worker, false);
        traced.apply_highlight(build(STREAM_CODE, false), &traced_worker, false);
        deliver_next(&mut traced, &traced_worker);
        deliver_next(&mut untraced, &untraced_worker);
        assert_ne!(untraced.lines, plain, "{EXPECT_PAINT}");
        let mut code = STREAM_CODE.to_owned();
        for append in APPENDS {
            code.push_str(append);
            let mut current = build(&code, true);
            let rows = current.rows.clone();
            current.source = None;
            untraced.update_with_reuse(current, &untraced_worker, false);
            traced.update_with_reuse(build(&code, true), &traced_worker, false);
            assert_eq!(untraced.lines, traced.lines, "{EXPECT_PAINT}");
            assert_eq!(untraced.links.rows, traced.links.rows);
            assert_eq!(untraced.rows, rows, "{EXPECT_SHAPE}");
            assert!(untraced.provenance.is_none());
            assert!(untraced.code_blocks.is_empty());
            assert!(untraced.highlights[0].local_source_rows.is_some());
            assert_eq!(untraced_worker.queued(), 1, "{EXPECT_COALESCED}");
        }
        deliver_next(&mut traced, &traced_worker);
        deliver_next(&mut untraced, &untraced_worker);
        assert_eq!(untraced.lines, traced.lines, "{EXPECT_PAINT}");
        assert_eq!(untraced.links.rows, traced.links.rows);
        assert!(untraced.provenance.is_none());
        assert_eq!(untraced_worker.queued(), 1, "{EXPECT_COALESCED}");
        let mut current = build(&code, false);
        current.source = None;
        untraced.update_with_reuse(current, &untraced_worker, false);
        traced.update_with_reuse(build(&code, false), &traced_worker, false);
        deliver_next(&mut traced, &traced_worker);
        deliver_next(&mut untraced, &untraced_worker);
        assert_eq!(untraced.lines, traced.lines, "{EXPECT_PAINT}");
        assert!(!untraced.has_pending_highlight());
        assert!(untraced.provenance.is_none());
        assert!(untraced.code_blocks.is_empty());
        let request = &untraced.highlights[0].request;
        assert_eq!(
            untraced.lines[request.region.range.clone()],
            request.region.render(request.sources().0, None).lines,
            "{EXPECT_PAINT}"
        );
    }

    #[test]
    fn local_source_reconstruction_rejects_mismatched_current_text() {
        let worker = RenderWorker::manual();
        let mut seg = Segment::default();
        let mut initial = input_lines(STREAM_CODE, false, PREFIX, WIDTH);
        initial.source = None;
        seg.apply_highlight(initial, &worker, false);
        deliver_next(&mut seg, &worker);
        let mut current = input_lines(
            &format!("{STREAM_CODE}{}", APPENDS[0]),
            false,
            PREFIX,
            WIDTH,
        );
        current.source = None;
        let first = current.highlight[0].region.range.start;
        current.lines[first].spans.last_mut().unwrap().content = CHANGED_CODE.into();
        let expected = current.lines.clone();
        seg.update_with_reuse(current, &worker, false);
        assert_eq!(seg.lines, expected, "{EXPECT_SHAPE}");
        assert!(seg.highlights[0].local_source_rows.is_none());
        deliver_next(&mut seg, &worker);
        assert_eq!(seg.lines, expected, "{EXPECT_SHAPE}");
        assert!(seg.provenance.is_none());
        assert!(seg.links.is_aligned(&seg.lines));
    }

    #[test_case(false; "code")]
    #[test_case(true; "script")]
    fn older_result_paints_prefix_and_schedules_latest_without_an_input_event(script: bool) {
        let worker = RenderWorker::manual();
        let mut seg = Segment::default();
        seg.apply_highlight(
            input_lines(STREAM_CODE, script, PREFIX, NARROW_WIDTH),
            &worker,
            false,
        );
        let id = seg.highlights[0].pending.as_ref().unwrap().id;
        let mut code = STREAM_CODE.to_owned();
        for append in APPENDS {
            code.push_str(append);
            seg.update_with_reuse(
                input_lines(&code, script, MOVED_PREFIX, NARROW_WIDTH),
                &worker,
                false,
            );
            assert!(seg.matches_pending_highlight(id), "{EXPECT_COALESCED}");
            assert_eq!(worker.queued(), 1, "{EXPECT_COALESCED}");
        }
        let text = seg
            .lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let before = seg.lines.clone();
        deliver_next(&mut seg, &worker);
        assert_ne!(seg.lines, before, "{EXPECT_PAINT}");
        assert_eq!(worker.queued(), 1, "{EXPECT_COALESCED}");
        let pending = seg.highlights[0].pending.as_ref().unwrap();
        assert_ne!(pending.id, id);
        assert_eq!(pending.request.input_source().unwrap(), code);
        assert_eq!(
            seg.lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            text,
            "{EXPECT_SHAPE}"
        );
        deliver_next(&mut seg, &worker);
        assert!(!seg.has_pending_highlight());
        assert_eq!(worker.queued(), 0, "{EXPECT_COALESCED}");
        let request = &seg.highlights[0].request;
        let expected = request.region.render(request.sources().0, None);
        assert_eq!(
            seg.lines[request.region.range.clone()],
            expected.lines,
            "{EXPECT_PAINT}"
        );
        assert_eq!(
            seg.lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            text,
            "{EXPECT_SHAPE}"
        );
    }

    #[test]
    fn append_paint_uses_current_gutters_after_line_number_growth() {
        let worker = RenderWorker::manual();
        let mut seg = Segment::default();
        let code = format!("{}{}", "\n".repeat(98), CODE);
        seg.apply_highlight(
            input_lines(&code, false, PREFIX, NARROW_WIDTH),
            &worker,
            false,
        );
        deliver_next(&mut seg, &worker);
        let paint = seg.highlights[0].paint.clone().unwrap();
        let appended = format!("{code}\n{CHANGED_CODE}");
        seg.update_with_reuse(
            linked_input_lines(&appended, false, PREFIX, NARROW_WIDTH),
            &worker,
            false,
        );
        assert_source_frame(
            &seg,
            &linked_input_lines(&appended, false, PREFIX, NARROW_WIDTH),
            paint.syntax.as_ref().unwrap(),
            NARROW_WIDTH,
        );
    }

    fn clipped_input_lines(code: &str, linked: bool) -> ToolLines {
        let mut lines = if linked {
            linked_input_lines(code, false, code, WIDTH)
        } else {
            input_lines(code, false, code, WIDTH)
        };
        let start = lines.highlight[0].region.range.start + 1;
        let kept = start..start + 1;
        assert!(lines.highlight[0].region.keep(&kept));
        lines.source = lines
            .source
            .and_then(|source| source.keep_rows(kept.clone()));
        lines.lines = lines.lines[kept.clone()].to_vec();
        lines.rows = lines.rows[kept.clone()].to_vec();
        lines.links.rows = lines.links.rows[kept].to_vec();
        lines
    }

    #[test]
    fn clipped_prefix_paint_uses_its_own_block_source_not_identical_header_text() {
        const CODE: &str = "fn first() {}\nfn second() {}";
        let worker = RenderWorker::manual();
        let mut seg = Segment::default();
        seg.apply_highlight(clipped_input_lines(CODE, false), &worker, false);
        deliver_next(&mut seg, &worker);
        let paint = seg.highlights[0].paint.clone().unwrap();
        let append = format!("{CODE} // appended");
        seg.update_with_reuse(clipped_input_lines(&append, true), &worker, false);
        assert_source_frame(
            &seg,
            &clipped_input_lines(&append, true),
            paint.syntax.as_ref().unwrap(),
            WIDTH,
        );
    }

    #[test_case(0; "replacement")]
    #[test_case(1; "shrink")]
    #[test_case(2; "language")]
    #[test_case(3; "resize")]
    #[test_case(4; "theme")]
    #[test_case(5; "closure")]
    #[test_case(6; "input_kind")]
    #[test_case(7; "viewport")]
    fn incompatible_append_retires_paint_and_in_flight_ownership(change: usize) {
        let worker = RenderWorker::manual();
        let mut seg = Segment::default();
        seg.apply_highlight(
            input_lines(STREAM_CODE, false, PREFIX, WIDTH),
            &worker,
            false,
        );
        deliver_next(&mut seg, &worker);
        let append = format!("{STREAM_CODE}{}", APPENDS[0]);
        seg.update_with_reuse(input_lines(&append, false, PREFIX, WIDTH), &worker, false);
        let old = worker.render_next();
        let mut current = match change {
            0 => input_lines(CHANGED_CODE, false, PREFIX, WIDTH),
            1 => input_lines(STREAM_CODE, false, PREFIX, WIDTH),
            3 => input_lines(&append, false, PREFIX, NARROW_WIDTH),
            5 => streamed_tool_lines(1),
            6 => input_lines(&append, true, PREFIX, WIDTH),
            7 => clipped_input_lines(&append, false),
            _ => input_lines(&append, false, PREFIX, WIDTH),
        };
        if change == 2 {
            current.highlight[0].input = Some(Arc::new(ToolInput::Code {
                language: "python".into(),
                code: append,
            }));
        } else if change == 4 {
            seg.highlights[0].theme_gen = theme::generation().wrapping_sub(1);
        }
        let plain = current.lines.clone();
        seg.update_with_reuse(current, &worker, false);
        assert_eq!(seg.lines, plain, "{EXPECT_FRESH}");
        assert!(!seg.matches_pending_highlight(old.id));
        seg.apply_highlight_result(old.id, old.lines, old.rows, old.source_rows, &worker);
        assert_eq!(seg.lines, plain, "{EXPECT_FRESH}");
        assert_eq!(
            worker.queued(),
            usize::from(change != 5),
            "{EXPECT_COALESCED}"
        );
    }

    #[test]
    fn appended_input_retains_completed_colors_before_result() {
        let worker = RenderWorker::new();
        let mut seg = Segment::default();
        seg.apply_highlight(
            code_tool_lines(CODE, PREFIX, LIVE_BEFORE, WIDTH),
            &worker,
            false,
        );
        let request = seg.highlights[0].request.clone();
        let id = seg.highlights[0].pending.as_ref().unwrap().id;
        let result = request.region.render(request.sources().0, None);
        let fallback = seg.lines.clone();
        seg.apply_highlight_result(
            id,
            result.lines,
            result.rows,
            result.source.map(|s| s.rows),
            &worker,
        );
        assert_ne!(seg.lines, fallback, "{EXPECT_PAINT}");
        let painted = seg.lines[request.region.range.start].clone();
        seg.update_with_reuse(
            code_tool_lines(
                &format!("{CODE}\n{CHANGED_CODE}"),
                PREFIX,
                LIVE_AFTER,
                WIDTH,
            ),
            &worker,
            false,
        );
        assert_eq!(
            seg.lines[request.region.range.start], painted,
            "{EXPECT_PAINT}"
        );
    }

    #[test]
    fn pending_regions_relocate_and_completed_paints_rebase_source() {
        let worker = RenderWorker::new();
        let mut seg = Segment::default();
        seg.apply_highlight(
            code_tool_lines(CODE, PREFIX, LIVE_BEFORE, WIDTH),
            &worker,
            false,
        );
        let request = seg.highlights[0].request.clone();
        let id = seg.highlights[0].pending.as_ref().unwrap().id;
        seg.update_with_reuse(
            code_tool_lines(CODE, MOVED_PREFIX, LIVE_AFTER, WIDTH),
            &worker,
            false,
        );
        assert!(seg.matches_pending_highlight(id), "{EXPECT_PENDING}");
        let before = seg.lines.clone();
        let (input, output) = request.sources();
        let result = request.region.render(input, output);
        seg.apply_highlight_result(
            id,
            result.lines,
            result.rows,
            result.source.map(|source| source.rows),
            &worker,
        );
        let range = seg.highlights[0].request.region.range.clone();
        let painted = seg.lines[range.clone()].to_vec();
        assert_ne!(painted, before[range], "{EXPECT_PAINT}");
        assert_eq!(
            seg.lines.last().unwrap().to_string(),
            LIVE_AFTER,
            "{EXPECT_FRESH}"
        );
        assert!(!seg.has_pending_highlight());
        seg.update_with_reuse(
            code_tool_lines(CODE, PREFIX, LIVE_BEFORE, WIDTH),
            &worker,
            false,
        );
        let range = seg.highlights[0].request.region.range.clone();
        assert_eq!(seg.lines[range.clone()], painted);
        assert!(!seg.has_pending_highlight());
        let column = seg.lines[range.start].to_string().find("fn").unwrap() as u16;
        let provenance = seg.provenance.as_ref().unwrap();
        let offset = provenance
            .byte_at(&seg.lines, WIDTH, range.start as u16, column)
            .unwrap();
        assert!(provenance.source()[offset as usize..].starts_with(CODE));
        assert!(seg.links.is_aligned(&seg.lines));
    }

    #[test_case(false; "source_changes")]
    #[test_case(true; "width_changes")]
    fn changed_regions_reject_late_results(resized: bool) {
        let worker = RenderWorker::new();
        let mut seg = Segment::default();
        seg.apply_highlight(
            code_tool_lines(CODE, PREFIX, LIVE_BEFORE, WIDTH),
            &worker,
            false,
        );
        let id = seg.highlights[0].pending.as_ref().unwrap().id;
        let request = seg.highlights[0].request.clone();
        let (code, width) = if resized {
            (CODE, NARROW_WIDTH)
        } else {
            (CHANGED_CODE, WIDTH)
        };
        seg.update_with_reuse(
            code_tool_lines(code, PREFIX, LIVE_AFTER, width),
            &worker,
            false,
        );
        assert!(!seg.matches_pending_highlight(id));
        let before = seg.lines.clone();
        let (input, output) = request.sources();
        let result = request.region.render(input, output);
        seg.apply_highlight_result(
            id,
            result.lines,
            result.rows,
            result.source.map(|source| source.rows),
            &worker,
        );
        assert_eq!(seg.lines, before, "{EXPECT_FRESH}");
    }

    #[test]
    fn removed_and_old_theme_regions_retire_requests() {
        let worker = RenderWorker::new();
        let mut seg = Segment::default();
        seg.apply_highlight(
            code_tool_lines(CODE, PREFIX, LIVE_BEFORE, WIDTH),
            &worker,
            false,
        );
        let id = seg.highlights[0].pending.as_ref().unwrap().id;
        seg.highlights[0].theme_gen = theme::generation().wrapping_sub(1);
        seg.update_with_reuse(
            code_tool_lines(CODE, PREFIX, LIVE_BEFORE, WIDTH),
            &worker,
            false,
        );
        assert!(!seg.matches_pending_highlight(id));
        assert!(seg.has_pending_highlight());
        seg.update_with_reuse(streamed_tool_lines(1), &worker, false);
        assert!(seg.highlights.is_empty());
    }

    #[test_case(0; "short_result")]
    #[test_case(1; "changed_text")]
    #[test_case(2; "missing_targets")]
    fn malformed_results_cannot_splice_card_rows(failure: usize) {
        let worker = RenderWorker::new();
        let mut seg = Segment::default();
        seg.apply_highlight(
            code_tool_lines(CODE, PREFIX, LIVE_BEFORE, WIDTH),
            &worker,
            false,
        );
        let id = seg.highlights[0].pending.as_ref().unwrap().id;
        let range = seg.highlights[0].request.region.range.clone();
        let mut lines = seg.lines[range.clone()].to_vec();
        let mut rows = seg.rows[range].to_vec();
        match failure {
            0 => {
                lines.pop();
                rows.pop();
            }
            1 => lines[0] = Line::raw(LIVE_AFTER),
            _ => rows.clear(),
        }
        let before = seg.lines.clone();
        let targets = seg.rows.clone();
        seg.apply_highlight_result(id, lines, rows, None, &worker);
        assert_eq!(seg.lines, before, "{EXPECT_SHAPE}");
        assert_eq!(seg.rows, targets, "{EXPECT_SHAPE}");
        assert!(seg.links.is_aligned(&seg.lines));
    }
}
