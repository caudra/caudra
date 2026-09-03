use crate::markdown::{DiagramSpan, LinkMap};
use crate::provenance::Provenance;
use crate::render_worker::RenderWorker;
use crate::theme;

use super::super::code_view::{BatchFolds, RowTarget, SectionFlags};
use super::super::tool_display::{HighlightRequest, ToolLines};
use super::layout::{SegmentChrome, SegmentKind};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use std::cell::Cell;

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

#[derive(Default, PartialEq, Eq)]
struct HighlightKey {
    has_output: bool,
    theme_gen: u64,
    /// Folding changes which lines the range holds, so reusing across a fold
    /// would splice back the body the reader just put away.
    folds: BatchFolds,
}

impl HighlightKey {
    /// The generation is read here rather than passed in: a theme only swaps
    /// from `update`, never mid-`view`, and a missed call site would silently
    /// splice old-palette lines back in.
    fn from_request(hl: Option<&HighlightRequest>) -> Self {
        Self {
            has_output: hl.is_some_and(|h| h.output.is_some()),
            theme_gen: theme::generation(),
            folds: hl.map(|h| h.limits.folds.clone()).unwrap_or_default(),
        }
    }
}

#[derive(Default)]
pub(super) struct Segment {
    lines: Vec<Line<'static>>,
    /// Set only for segments rendered from markdown. Selection uses it to
    /// copy the source; without it copy falls back to scraping cells.
    provenance: Option<Provenance>,
    /// Drawn diagrams in `lines`, so a hover or a pan can find one by row.
    /// Like `provenance`, cleared by `set_lines` and restored after it.
    diagrams: Vec<DiagramSpan>,
    links: LinkMap,
    pub search_text: String,
    pub tool_id: Option<String>,
    /// Backlink to `self.messages`, set only by `with_lines`. A click on a
    /// collapsed thinking indicator has no tool_id to route by, so this is
    /// how the click finds its message. It looks unused; delete it and the
    /// show_thinking toggle breaks.
    pub msg_index: Option<usize>,
    kind: SegmentKind,
    margin_top: u16,
    pub truncation: SectionFlags,
    cached_height: Cell<Option<CachedHeight>>,
    pending_highlight: Option<u64>,
    highlight_range: Option<(usize, usize)>,
    highlight_key: HighlightKey,
    pub spinner_lines: Vec<(usize, usize)>,
    snapshot_base: Option<usize>,
    pub shell_toggle_line: Option<usize>,
    /// What each line belongs to, parallel to `lines`. Spliced alongside them
    /// so a highlighted card keeps the rows a click names.
    rows: Vec<Option<RowTarget>>,
    pub content_indent: &'static str,
    /// Lines were laid out at a width or theme that is no longer current.
    /// Cleared by `set_lines` (whole vector replaced) and up front by
    /// `reflow_segment`; partial splices (`apply_highlight_result`) leave it
    /// set so the segment still reflows later.
    pub(super) stale: bool,
    /// Blank rows held below the card so that closing it moves nothing else.
    /// They count in the layout and draw nothing, and are handed back once
    /// they are off screen, where giving up the space costs the reader
    /// nothing.
    reserved: u16,
}

impl Segment {
    pub fn with_tool(tool_id: String, kind: SegmentKind) -> Self {
        Self {
            tool_id: Some(tool_id),
            kind,
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

    pub fn reserved(&self) -> u16 {
        self.reserved
    }

    /// Adds to what is already held: a card can lose rows more than once
    /// before the reader scrolls past what it gave up the first time.
    pub fn reserve(&mut self, rows: u16) {
        if rows == 0 {
            return;
        }
        self.reserved = self.reserved.saturating_add(rows);
        self.invalidate_height();
    }

    pub fn release(&mut self) {
        if self.reserved != 0 {
            self.reserved = 0;
            self.invalidate_height();
        }
    }

    pub fn content_width(&self, width: u16) -> u16 {
        self.chrome(width).content_width(width)
    }

    pub fn content_height(&self, width: u16) -> u16 {
        wrapped_line_count(&self.lines, self.content_width(width))
    }

    pub fn provenance(&self) -> Option<&Provenance> {
        self.provenance.as_ref()
    }

    pub fn set_provenance(&mut self, provenance: Option<Provenance>) {
        self.provenance = provenance;
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
        self.card_height(width).saturating_add(self.reserved)
    }

    /// The card without the blank rows held below it, which is what the
    /// renderer draws into and what every row inside the card is measured
    /// against.
    pub fn card_height(&self, width: u16) -> u16 {
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

    /// What the row at `rel_row` belongs to, for a click or a hover to name.
    pub fn row_target_at(&self, rel_row: u16, width: u16) -> Option<RowTarget> {
        let line = self.source_line_at(rel_row, width)?;
        self.rows.get(line).copied().flatten()
    }

    /// Maps a source line to a 1-based row in the tool's live buffer, or 0
    /// for lines outside it (header etc.). The Lua click-row contract is
    /// computed here and nowhere else, from the base recorded when the
    /// buffer snapshot was laid out.
    pub fn buf_row(&self, source_line: usize) -> usize {
        match self.snapshot_base {
            Some(base) if source_line >= base => source_line - base + 1,
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

    fn reuse_highlight(
        &self,
        key: &HighlightKey,
        new_range: (usize, usize),
    ) -> Option<Vec<Line<'static>>> {
        if self.pending_highlight.is_some() || self.highlight_key != *key {
            return None;
        }
        let (s, e) = self.highlight_range?;
        if s > e || e > self.lines.len() {
            return None;
        }
        if (e - s) != (new_range.1 - new_range.0) {
            return None;
        }
        Some(self.lines[s..e].to_vec())
    }

    pub fn apply_highlight(&mut self, mut tl: ToolLines, worker: &RenderWorker, compact: bool) {
        self.set_kind(tool_kind(self.kind, &tl, compact));
        self.pending_highlight = tl.send_highlight(worker);
        self.highlight_range = tl.highlight.as_ref().map(|h| h.range);
        self.highlight_key = HighlightKey::from_request(tl.highlight.as_ref());
        self.spinner_lines = tl.spinner_lines;
        self.snapshot_base = tl.snapshot_base;
        self.shell_toggle_line = tl.shell_toggle_line;
        self.content_indent = tl.content_indent;
        self.truncation = tl.truncation;
        let links = std::mem::take(&mut tl.links);
        let rows = std::mem::take(&mut tl.rows);
        self.set_lines(tl.lines);
        self.set_links(links);
        self.rows = rows;
    }

    pub fn update_with_reuse(&mut self, mut tl: ToolLines, worker: &RenderWorker, compact: bool) {
        self.set_kind(tool_kind(self.kind, &tl, compact));
        let key = HighlightKey::from_request(tl.highlight.as_ref());
        let reused = tl.highlight.as_ref().and_then(|req| {
            let hl_lines = self.reuse_highlight(&key, req.range)?;
            let (s, _) = req.range;
            let new_end = s + hl_lines.len();
            let link_rows = hl_lines
                .iter()
                .map(|line| vec![None; line.spans.len()])
                .collect::<Vec<_>>();
            tl.lines.splice(s..req.range.1, hl_lines);
            tl.links.rows.splice(s..req.range.1, link_rows);
            Some((s, new_end))
        });
        self.truncation = tl.truncation;
        if let Some((s, e)) = reused {
            let links = std::mem::take(&mut tl.links);
            let rows = std::mem::take(&mut tl.rows);
            self.set_lines(tl.lines);
            self.set_links(links);
            self.rows = rows;
            self.highlight_range = Some((s, e));
            self.pending_highlight = None;
            self.spinner_lines = tl.spinner_lines;
            self.snapshot_base = tl.snapshot_base;
            self.shell_toggle_line = tl.shell_toggle_line;
            self.content_indent = tl.content_indent;
        } else {
            self.apply_highlight(tl, worker, compact);
        }
    }

    pub fn matches_pending_highlight(&self, id: u64) -> bool {
        self.pending_highlight == Some(id)
    }

    pub fn apply_highlight_result(
        &mut self,
        lines: Vec<Line<'static>>,
        rows: Vec<Option<RowTarget>>,
    ) {
        if !self.links.is_aligned(&self.lines) {
            self.links = LinkMap::none_for(&self.lines);
        }
        if self.rows.len() != self.lines.len() {
            self.rows = vec![None; self.lines.len()];
        }
        if let Some((start, end)) = self.highlight_range {
            let indent = self.content_indent;
            let indented: Vec<Line<'static>> = lines
                .into_iter()
                .map(|mut line| {
                    if !indent.is_empty() {
                        line.spans.insert(0, Span::raw(indent));
                    }
                    line
                })
                .collect();
            let new_end = start + indented.len();
            let link_rows = indented
                .iter()
                .map(|line| vec![None; line.spans.len()])
                .collect::<Vec<_>>();
            let mut rows = rows;
            rows.resize(new_end - start, None);
            self.lines.splice(start..end, indented);
            self.links.rows.splice(start..end, link_rows);
            self.rows.splice(start..end, rows);
            self.highlight_range = Some((start, new_end));
            self.shift_after(end, new_end as isize - end as isize);
            self.invalidate_height();
        }
        self.pending_highlight = None;
    }

    /// Keeps recorded line positions (spinners, buffer base) in step when
    /// a splice changes the number of lines before them.
    fn shift_after(&mut self, from: usize, delta: isize) {
        if delta == 0 {
            return;
        }
        let shift = |v: &mut usize| {
            if *v >= from {
                *v = v.saturating_add_signed(delta);
            }
        };
        for (line, _) in &mut self.spinner_lines {
            shift(line);
        }
        if let Some(base) = &mut self.snapshot_base {
            shift(base);
        }
        if let Some(line) = &mut self.shell_toggle_line {
            shift(line);
        }
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
        && lines.highlight.is_none()
        && lines.snapshot_base.is_none()
        && !lines.truncation.any()
    {
        SegmentKind::ToolInline
    } else {
        SegmentKind::ToolBlock
    }
}

/// A row that reads as one entry in a list rather than as its own block.
/// Compact folds reasoning in, which is what lets a thought sit against the
/// tool calls it interleaves with.
pub(super) fn dense_kind(kind: SegmentKind, compact: bool) -> bool {
    kind == SegmentKind::ToolInline || (compact && kind == SegmentKind::Thinking)
}

/// Single-line tool rows read as a list, so they sit flush against each other.
/// Compact drops the height condition: every row there is one entry, and a
/// wrapped one is still one entry.
fn stacks_flush(previous: (SegmentKind, u16), current: (SegmentKind, u16), compact: bool) -> bool {
    dense_kind(previous.0, compact)
        && dense_kind(current.0, compact)
        && (compact || (previous.1 <= 1 && current.1 <= 1))
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

    /// Where each held-open run of blank rows starts in the document, with
    /// the segment holding it. Only walks far enough to answer, since the
    /// caller reclaims from the top down.
    pub fn spacers(&self, width: u16) -> impl Iterator<Item = (usize, u32, u16)> + '_ {
        let mut offset: u32 = 0;
        self.segments
            .iter()
            .enumerate()
            .filter_map(move |(i, seg)| {
                offset += seg.height(width) as u32;
                let reserved = seg.reserved();
                (reserved > 0).then(|| (i, offset - reserved as u32, reserved))
            })
    }

    pub fn release(&mut self, idx: usize) {
        if let Some(seg) = self.segments.get_mut(idx) {
            seg.release();
        }
    }

    pub fn find_by_tool_id(&self, id: &str) -> Option<usize> {
        self.segments
            .iter()
            .rposition(|s| s.tool_id.as_deref() == Some(id))
    }

    pub fn len(&self) -> usize {
        self.segments.len()
    }

    /// What the streaming block below the cache would be following.
    pub fn last_kind(&self) -> Option<SegmentKind> {
        self.segments.last().map(Segment::kind)
    }

    pub fn update_margins(&mut self, width: u16, compact: bool) {
        let mut previous = None;
        for segment in &mut self.segments {
            let current = (segment.kind(), segment.content_height(width));
            let margin = previous.map_or(0, |previous| {
                u16::from(!stacks_flush(previous, current, compact))
            });
            segment.set_margin_top(margin);
            previous = Some(current);
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
    Paragraph::new(lines.to_vec())
        .wrap(Wrap { trim: false })
        .line_count(width) as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const OTHER_THEME: &str = "dracula";

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

        cache.update_margins(80, false);

        assert_eq!(cache.segments[0].margin_top, 0);
        assert_eq!(cache.segments[1].margin_top, 0);
    }

    #[test]
    fn wrapped_tool_separates_the_next_inline_tool() {
        let mut cache = SegmentCache::new();
        cache.push(inline_tool("x".repeat(80)));
        cache.push(inline_tool("next".into()));

        cache.update_margins(40, false);

        assert_eq!(cache.segments[1].margin_top, 1);
    }

    #[test]
    fn wrapped_inline_tool_is_separated_from_the_previous_tool() {
        let mut cache = SegmentCache::new();
        cache.push(inline_tool("first".into()));
        cache.push(inline_tool("x".repeat(80)));

        cache.update_margins(40, false);

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

    #[test]
    fn reuse_highlight_keys_on_theme_not_width() {
        use crate::components::code_view::RenderLimits;
        use caudra_agent::ToolOutput;
        use std::sync::Arc;

        let output = Arc::new(ToolOutput::ReadCode {
            path: "f.rs".into(),
            start_line: 1,
            lines: vec!["fn main() {}".into()],
            total_lines: 1,
            instructions: None,
        });
        let key = || {
            HighlightKey::from_request(Some(&HighlightRequest {
                range: (1, 3),
                input: None,
                output: Some(Arc::clone(&output)),
                limits: RenderLimits::default(),
            }))
        };
        let seg = Segment {
            highlight_key: key(),
            highlight_range: Some((1, 3)),
            lines: vec![
                Line::raw("h"),
                Line::raw("a"),
                Line::raw("b"),
                Line::raw("t"),
            ],
            ..Segment::default()
        };

        // Highlighted lines are source lines, not wrapped rows (the worker
        // job carries no width), so the key deliberately omits width.
        assert!(
            seg.reuse_highlight(&key(), (1, 3)).is_some(),
            "reuse must fire across a width change; highlight lines are width-independent"
        );

        theme::set(theme::load_by_name(OTHER_THEME).unwrap());
        assert!(
            seg.reuse_highlight(&key(), (1, 3)).is_none(),
            "theme mismatch must force a fresh highlight, not splice old-palette lines"
        );
    }

    #[test_case(4, 6 ; "splice_grows")]
    #[test_case(1, 3 ; "splice_shrinks")]
    fn highlight_splice_shifts_spinners_and_base(replacement_lines: usize, expected_base: usize) {
        let mut seg = seg_with_base(8, Some(4));
        seg.highlight_range = Some((1, 3));
        seg.spinner_lines = vec![(0, 0), (5, 1)];
        seg.apply_highlight_result(
            (0..replacement_lines).map(|_| Line::raw("hl")).collect(),
            vec![None; replacement_lines],
        );
        let delta = expected_base as isize - 4;
        assert_eq!(seg.snapshot_base, Some(expected_base));
        assert_eq!(
            seg.spinner_lines,
            vec![(0, 0), (5usize.saturating_add_signed(delta), 1)],
            "positions before the splice stay, after it shift by the delta"
        );
    }
    const EXPECT_ROWS_ALIGNED: &str = "the rows have to stay parallel to the lines";

    /// The splice replaces the content range, so the worker's rows have to
    /// come in with its lines or a click loses the target under it.
    #[test_case(4 ; "splice_grows")]
    #[test_case(1 ; "splice_shrinks")]
    #[test_case(2 ; "same_length")]
    fn highlight_splice_carries_the_rows_with_the_lines(replacement_lines: usize) {
        let mut seg = seg_with_base(8, None);
        seg.highlight_range = Some((1, 3));
        seg.rows = vec![None; 8];
        seg.rows[7] = Some(RowTarget::BatchChild(9));
        let hl_rows: Vec<Option<RowTarget>> = (0..replacement_lines)
            .map(|i| Some(RowTarget::BatchChild(i)))
            .collect();

        seg.apply_highlight_result(
            (0..replacement_lines).map(|_| Line::raw("hl")).collect(),
            hl_rows.clone(),
        );

        assert_eq!(seg.rows.len(), seg.lines.len(), "{EXPECT_ROWS_ALIGNED}");
        assert_eq!(&seg.rows[1..1 + replacement_lines], hl_rows.as_slice());
        assert_eq!(
            seg.rows.last().copied().flatten(),
            Some(RowTarget::BatchChild(9)),
            "a row after the splice moves with its line"
        );
    }

    /// A worker that answers with fewer rows than lines must not leave the
    /// two out of step, or every row below reads as the wrong child.
    #[test]
    fn a_short_row_answer_is_padded_rather_than_left_ragged() {
        let mut seg = seg_with_base(6, None);
        seg.highlight_range = Some((1, 4));
        seg.rows = vec![None; 6];

        seg.apply_highlight_result(
            (0..3).map(|_| Line::raw("hl")).collect(),
            vec![Some(RowTarget::BatchChild(0))],
        );

        assert_eq!(seg.rows.len(), seg.lines.len(), "{EXPECT_ROWS_ALIGNED}");
    }

    /// Folding changes which lines the range holds, so a reused highlight
    /// would splice back the body the reader just put away.
    #[test]
    fn a_fold_forces_a_fresh_highlight() {
        use crate::components::code_view::RenderLimits;

        let request = |folds: BatchFolds| HighlightRequest {
            range: (1, 3),
            input: None,
            output: None,
            limits: RenderLimits {
                script: 0,
                output: 0,
                folds,
            },
        };
        let seg = Segment {
            highlight_key: HighlightKey::from_request(Some(&request(BatchFolds::default()))),
            highlight_range: Some((1, 3)),
            lines: vec![Line::raw("h"), Line::raw("a"), Line::raw("b")],
            ..Segment::default()
        };

        let folded = HighlightKey::from_request(Some(&request(BatchFolds::new([0]))));
        assert!(seg.reuse_highlight(&folded, (1, 3)).is_none());
        let same = HighlightKey::from_request(Some(&request(BatchFolds::default())));
        assert!(seg.reuse_highlight(&same, (1, 3)).is_some());
    }
}
