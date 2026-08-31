//! Width-aware markdown renderer. Theme-free: outputs semantic style
//! tokens that consumers (`maki-ui`, `maki-lua`) map to their own colours.
//!
//! Single source of truth for layout: tables, code bars, wrapping, blank
//! lines. Everyone consumes `Line` values from here.

use std::borrow::Cow;
use std::iter;
use std::mem;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use maki_highlight::CodeHighlighter;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    Block, BlockKind, Emphasis, InlineSpan, LineBlock, Source, SpanKind, block_prefix, latex,
    mermaid, parse_at, parse_inline, parse_inline_at,
};

pub const CODE_BAR: &str = "│ ";
pub const CODE_BAR_WRAP: &str = "│";
/// Lines longer than this get truncated with `...` to protect the parser
/// and terminal from runaway output.
pub const TOOL_OUTPUT_MAX_LINE_BYTES: usize = 1_500;
const HR_CHAR: char = '─';
const MIN_COL_WIDTH: usize = 5;
const LONG_LINE_SUFFIX: &str = "...";
const MERMAID_LANG: &str = "mermaid";
/// Marks a diagram row that continues past an edge.
const DIAGRAM_MORE_RIGHT: &str = "›";
const DIAGRAM_MORE_LEFT: &str = "‹";

/// Furthest a diagram may pan before its right edge is on screen. One column
/// is owed to the `‹` marker that a panned diagram always carries.
pub fn diagram_max_pan(full_width: u16, width: u16) -> u16 {
    full_width.saturating_sub(width.saturating_sub(1).max(1))
}

/// Semantic style token. Emphasis (bold/italic/strike/underline) lives on
/// the `Span`, not here, so they compose independently.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StyleToken {
    Text,
    InlineCode,
    /// Syntax-highlighted token. Carries resolved rgb + modifiers so the
    /// consumer doesn't need to know the language.
    Highlight {
        fg: (u8, u8, u8),
        bold: bool,
        italic: bool,
        underline: bool,
    },
    CodeBar,
    Heading,
    ListMarker,
    TableBorder,
    HorizontalRule,
    Math,
    /// Box-drawing and connector cells of a rendered diagram. Node and edge
    /// labels inside one stay `Text` so they read as prose.
    Diagram,
}

/// How LaTeX is presented. Terminals cannot typeset maths, so the choice is
/// between a Unicode approximation and the source itself. Fonts vary in how
/// much of the maths block they cover, which is why this is configurable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MathStyle {
    /// `$x^2$` renders as `x²`.
    Unicode,
    /// `$x^2$` renders as `x^2`, delimiters stripped.
    Raw,
}

/// Renderers are built ad hoc all over the UI, including in draw paths that
/// have no route to the config, so the choice lives here like the theme.
static MATH_STYLE: AtomicU8 = AtomicU8::new(MathStyle::Unicode as u8);

impl Default for MathStyle {
    fn default() -> Self {
        match MATH_STYLE.load(Ordering::Relaxed) {
            x if x == Self::Raw as u8 => Self::Raw,
            _ => Self::Unicode,
        }
    }
}

impl MathStyle {
    /// Applies to renderers built after this call.
    pub fn set_global(self) {
        MATH_STYLE.store(self as u8, Ordering::Relaxed);
    }
}

impl MathStyle {
    fn apply(self, latex: &str) -> Option<String> {
        match self {
            Self::Unicode => latex::to_unicode(latex),
            Self::Raw => None,
        }
    }
}

/// How ```` ```mermaid ```` blocks are presented. Unlike maths there is a real
/// `Off`: a flowchart that cannot be laid out well is better read as source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MermaidStyle {
    /// Lay the flowchart out and draw it with box-drawing characters.
    Unicode,
    /// Leave the block as a syntax-highlighted fence.
    Off,
}

static MERMAID_STYLE: AtomicU8 = AtomicU8::new(MermaidStyle::Unicode as u8);

impl Default for MermaidStyle {
    fn default() -> Self {
        match MERMAID_STYLE.load(Ordering::Relaxed) {
            x if x == Self::Off as u8 => Self::Off,
            _ => Self::Unicode,
        }
    }
}

impl MermaidStyle {
    /// Applies to renderers built after this call.
    pub fn set_global(self) {
        MERMAID_STYLE.store(self as u8, Ordering::Relaxed);
    }
}

/// Where a rendered span's text came from. Selection reads this to rebuild
/// the original markdown instead of scraping glyphs off the screen.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SpanSource {
    /// A slice of the parsed text.
    Range(Source),
    /// Renderer chrome with no counterpart in the source: code gutters,
    /// table borders, bullets, cell padding, rule fill. Dropped on copy.
    Chrome,
    /// No provenance available; consumers fall back to the rendered text.
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Span {
    pub text: String,
    pub style: StyleToken,
    pub emphasis: Emphasis,
    pub source: SpanSource,
    pub link: Option<Arc<str>>,
}

impl Span {
    /// For spans whose text has no source counterpart.
    pub fn chrome(text: impl Into<String>, style: StyleToken) -> Self {
        Self {
            text: text.into(),
            style,
            emphasis: Emphasis::default(),
            source: SpanSource::Chrome,
            link: None,
        }
    }

    pub fn new(text: impl Into<String>, style: StyleToken) -> Self {
        Self {
            text: text.into(),
            style,
            emphasis: Emphasis::default(),
            source: SpanSource::Unknown,
            link: None,
        }
    }

    pub fn with_emphasis(text: impl Into<String>, style: StyleToken, emphasis: Emphasis) -> Self {
        Self {
            text: text.into(),
            style,
            emphasis,
            source: SpanSource::Unknown,
            link: None,
        }
    }

    pub fn sourced(
        text: impl Into<String>,
        style: StyleToken,
        emphasis: Emphasis,
        source: Source,
    ) -> Self {
        Self {
            text: text.into(),
            style,
            emphasis,
            source: SpanSource::Range(source),
            link: None,
        }
    }

    pub fn with_link(mut self, link: Option<Arc<str>>) -> Self {
        self.link = link;
        self
    }

    /// Byte sub-slice of this span. Verbatim ranges narrow with the text;
    /// atomic ones keep the whole range, since any part of them still
    /// stands for the entire construct.
    fn slice(&self, start: usize, end: usize) -> Self {
        let source = match &self.source {
            SpanSource::Range(Source {
                range,
                verbatim: true,
            }) => SpanSource::Range(Source::verbatim(
                range.start + start as u32..range.start + end as u32,
            )),
            other => other.clone(),
        };
        Self {
            text: self.text[start..end].to_owned(),
            style: self.style.clone(),
            emphasis: self.emphasis,
            source,
            link: self.link.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LineKind {
    Paragraph,
    Heading,
    ListItem,
    Code,
    TableBorder,
    TableRow,
    HorizontalRule,
    Math,
    /// One row of drawn diagram. `id` counts diagrams within the message and
    /// `full_width` is the unclipped art width, which is what a horizontal
    /// pan needs to know how far it may travel.
    Diagram {
        id: u16,
        full_width: u16,
    },
    Blank,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Line {
    pub kind: LineKind,
    pub spans: Vec<Span>,
    /// Source bytes for the whole row, including syntax the spans dropped:
    /// heading hashes, list markers, emphasis delimiters, code fences, table
    /// pipes. A selection covering the row copies this instead of the spans.
    pub source: Option<Range<u32>>,
}

impl Line {
    pub fn blank() -> Self {
        Self {
            kind: LineKind::Blank,
            spans: Vec::new(),
            source: None,
        }
    }

    pub fn width(&self) -> usize {
        self.spans.iter().map(|s| s.text.width()).sum()
    }

    pub fn is_blank(&self) -> bool {
        self.spans.is_empty() || self.spans.iter().all(|s| s.text.is_empty())
    }
}

pub fn render(text: &str, width: u16) -> Vec<Line> {
    Renderer::new().render(text, width, 0)
}

/// Reuses highlighter and table-width caches across calls so streaming
/// (successive prefixes of a growing message) doesn't re-highlight completed
/// code lines. Bump `theme_gen` to flush caches after a theme change.
pub struct Renderer {
    highlighters: Vec<CodeHighlighter>,
    table_col_widths: Vec<Vec<usize>>,
    diagrams: Vec<Diagram>,
    diagram_pans: Vec<u16>,
    theme_gen: u64,
    wrap_paragraphs: bool,
    math: MathStyle,
    mermaid: MermaidStyle,
}

/// A laid-out flowchart kept beside its source so a re-render of the same
/// block during streaming does not repeat the layout.
struct Diagram {
    code: String,
    canvas: mermaid::Canvas,
}

impl Default for Renderer {
    fn default() -> Self {
        Self {
            highlighters: Vec::new(),
            table_col_widths: Vec::new(),
            diagrams: Vec::new(),
            diagram_pans: Vec::new(),
            theme_gen: 0,
            wrap_paragraphs: true,
            math: MathStyle::default(),
            mermaid: MermaidStyle::default(),
        }
    }
}

impl Renderer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Skip paragraph/heading/list wrapping (ratatui re-wraps those at
    /// paint time). Code blocks and tables still wrap.
    pub fn unwrapped() -> Self {
        Self {
            wrap_paragraphs: false,
            ..Self::default()
        }
    }

    pub fn with_math(mut self, math: MathStyle) -> Self {
        self.math = math;
        self
    }

    pub fn with_mermaid(mut self, mermaid: MermaidStyle) -> Self {
        self.mermaid = mermaid;
        self
    }

    /// Horizontal offset per diagram, indexed by the `id` on
    /// [`LineKind::Diagram`]. Missing entries pan to zero.
    pub fn with_diagram_pans(mut self, pans: Vec<u16>) -> Self {
        self.diagram_pans = pans;
        self
    }

    pub fn render(&mut self, text: &str, width: u16, theme_gen: u64) -> Vec<Line> {
        if theme_gen != self.theme_gen {
            self.highlighters.clear();
            self.theme_gen = theme_gen;
        }
        let trimmed = text.trim_start_matches('\n');
        let blocks = parse_at(trimmed, text.len() - trimmed.len());
        let mut lines: Vec<Line> = Vec::new();
        let mut state = RenderState {
            code_idx: 0,
            table_idx: 0,
            diagram_idx: 0,
            highlighters: &mut self.highlighters,
            table_col_widths: &mut self.table_col_widths,
            diagrams: &mut self.diagrams,
        };
        let ctx = RenderCtx {
            width,
            wrap_paragraphs: self.wrap_paragraphs,
            math: self.math,
            mermaid: self.mermaid,
            diagram_pans: &self.diagram_pans,
        };

        for block in &blocks {
            render_block(block, &mut lines, &mut state, &ctx);
        }

        state.highlighters.truncate(state.code_idx);
        state.table_col_widths.truncate(state.table_idx);
        state.diagrams.truncate(state.diagram_idx);
        finalize_lines(&mut lines);
        lines
    }
}

struct RenderCtx<'a> {
    width: u16,
    wrap_paragraphs: bool,
    math: MathStyle,
    mermaid: MermaidStyle,
    diagram_pans: &'a [u16],
}

struct RenderState<'a> {
    code_idx: usize,
    table_idx: usize,
    diagram_idx: usize,
    highlighters: &'a mut Vec<CodeHighlighter>,
    table_col_widths: &'a mut Vec<Vec<usize>>,
    diagrams: &'a mut Vec<Diagram>,
}

/// Draws a ```` ```mermaid ```` block, reporting whether it was understood.
/// A refusal leaves `lines` untouched so the caller can fall back to the
/// ordinary code path.
fn render_diagram(
    code: &str,
    source: &Range<u32>,
    lines: &mut Vec<Line>,
    state: &mut RenderState<'_>,
    ctx: &RenderCtx<'_>,
) -> bool {
    let index = state.diagram_idx;
    let fresh = match state.diagrams.get(index) {
        Some(cached) if cached.code == code => None,
        _ => match mermaid::render(code) {
            Ok(canvas) => Some(Diagram {
                code: code.to_owned(),
                canvas,
            }),
            Err(_) => return false,
        },
    };
    if let Some(diagram) = fresh {
        match state.diagrams.get_mut(index) {
            Some(slot) => *slot = diagram,
            None => state.diagrams.push(diagram),
        }
    }

    let canvas = &state.diagrams[index].canvas;
    let full_width = canvas.width.min(u16::MAX as usize) as u16;
    let id = index.min(u16::MAX as usize) as u16;
    let pan = ctx
        .diagram_pans
        .get(index)
        .copied()
        .unwrap_or(0)
        .min(diagram_max_pan(full_width, ctx.width));

    for row in canvas.rows() {
        lines.push(diagram_line(row, source, id, full_width, pan, ctx.width));
    }
    state.diagram_idx += 1;
    true
}

/// Slices one canvas row to the viewport at `pan`. Rows leave here already
/// fitted, so nothing downstream ever wraps a diagram.
fn diagram_line(
    row: &[mermaid::Cell],
    source: &Range<u32>,
    id: u16,
    full_width: u16,
    pan: u16,
    width: u16,
) -> Line {
    // Each marker costs a column, so the window is whatever is left after
    // accounting for the ones this row actually needs.
    let lead = pan > 0;
    let mut visible = width.saturating_sub(u16::from(lead)).max(1);
    let trail = (pan as usize + visible as usize) < full_width as usize;
    if trail {
        visible = visible.saturating_sub(1);
    }
    let window = row
        .iter()
        .skip(pan as usize)
        .take(visible as usize)
        .filter(|cell| cell.ch != mermaid::CONTINUATION);

    let mut spans: Vec<Span> = Vec::new();
    if lead {
        spans.push(Span::chrome(DIAGRAM_MORE_LEFT, StyleToken::Diagram));
    }
    for cell in window {
        let style = match cell.role {
            mermaid::Role::Label => StyleToken::Text,
            _ => StyleToken::Diagram,
        };
        // The whole fence is one atomic range, so any selection touching a
        // drawn cell copies back the mermaid source instead of the glyphs.
        match spans.last_mut() {
            Some(last) if last.style == style => last.text.push(cell.ch),
            _ => spans.push(Span::sourced(
                cell.ch.to_string(),
                style,
                Emphasis::default(),
                Source::atomic(source.clone()),
            )),
        }
    }
    if trail {
        spans.push(Span::chrome(DIAGRAM_MORE_RIGHT, StyleToken::Diagram));
    }

    Line {
        kind: LineKind::Diagram { id, full_width },
        spans,
        source: Some(source.clone()),
    }
}

/// Streaming can split tokens differently than a oneshot render because the
/// highlighter sees partial input. Merging identical neighbours keeps the
/// span shape stable.
/// Only contiguous verbatim ranges can join; anything else has to stay split
/// or the merged span would point at the wrong bytes.
fn merge_sources(a: &SpanSource, b: &SpanSource) -> Option<SpanSource> {
    match (a, b) {
        (SpanSource::Chrome, SpanSource::Chrome) => Some(SpanSource::Chrome),
        (SpanSource::Unknown, SpanSource::Unknown) => Some(SpanSource::Unknown),
        (
            SpanSource::Range(Source {
                range: x,
                verbatim: true,
            }),
            SpanSource::Range(Source {
                range: y,
                verbatim: true,
            }),
        ) if x.end == y.start => Some(SpanSource::Range(Source::verbatim(x.start..y.end))),
        _ => None,
    }
}

fn coalesce_adjacent_spans(spans: &mut Vec<Span>) {
    if spans.len() < 2 {
        return;
    }
    let mut write = 0;
    for read in 1..spans.len() {
        if spans[write].style == spans[read].style
            && spans[write].emphasis == spans[read].emphasis
            && spans[write].link == spans[read].link
            && let Some(source) = merge_sources(&spans[write].source, &spans[read].source)
        {
            let tail = mem::take(&mut spans[read].text);
            spans[write].text.push_str(&tail);
            spans[write].source = source;
        } else {
            write += 1;
            if write != read {
                spans.swap(write, read);
            }
        }
    }
    spans.truncate(write + 1);
}

fn render_block(
    block: &Block,
    lines: &mut Vec<Line>,
    state: &mut RenderState<'_>,
    ctx: &RenderCtx,
) {
    match block {
        Block::Lines(line_blocks) => {
            for lb in line_blocks {
                render_line_block(lb, lines, ctx);
            }
        }
        Block::Code {
            lang,
            code,
            source,
            code_start,
            closed,
        } => {
            ensure_blank_line(lines);
            if state.code_idx >= state.highlighters.len() {
                state.highlighters.push(CodeHighlighter::new(lang));
            }
            // A diagram still claims its highlighter slot. Streaming shows the
            // block as code until the closing fence lands, and the slot has to
            // stay put across that switch or later blocks read a stale cache.
            if *closed && lang == MERMAID_LANG && ctx.mermaid == MermaidStyle::Unicode {
                state.code_idx += 1;
                if render_diagram(code, source, lines, state, ctx) {
                    ensure_blank_line(lines);
                    return;
                }
                state.code_idx -= 1;
            }
            let segments: Vec<_> = state.highlighters[state.code_idx].update(code).to_vec();
            let start = lines.len();
            let last = segments.len().saturating_sub(1);
            // Highlighting expands tabs, so segment lengths do not track
            // source bytes. Line ranges come from `code` itself, and the
            // fences ride on the first and last rows so a full selection
            // copies back a complete fenced block.
            let mut at = *code_start;
            let mut src_lines = code.split('\n');
            for (i, segs) in segments.into_iter().enumerate() {
                let src_len = src_lines.next().map_or(0, str::len) as u32;
                let mut spans = vec![Span::chrome(CODE_BAR, StyleToken::CodeBar)];
                // Tab expansion breaks the byte correspondence, so those
                // lines fall back to one atomic range for the whole row.
                let exact = segs.iter().map(|s| s.text.len()).sum::<usize>() == src_len as usize;
                let mut col = at;
                for seg in segs {
                    let len = seg.text.len() as u32;
                    let source = match exact {
                        true => Source::verbatim(col..col + len),
                        false => Source::atomic(at..at + src_len),
                    };
                    col += len;
                    spans.push(Span::sourced(
                        seg.text,
                        StyleToken::Highlight {
                            fg: seg.fg,
                            bold: seg.bold,
                            italic: seg.italic,
                            underline: seg.underline,
                        },
                        Emphasis::default(),
                        source,
                    ));
                }
                coalesce_adjacent_spans(&mut spans);
                let from = if i == 0 { source.start } else { at };
                let to = if i == last { source.end } else { at + src_len };
                lines.push(Line {
                    kind: LineKind::Code,
                    spans,
                    source: Some(from..to),
                });
                at += src_len + 1;
            }
            wrap_code_lines(lines, start, ctx.width);
            ensure_blank_line(lines);
            state.code_idx += 1;
        }
        Block::Table {
            rows,
            header_end,
            row_sources,
            separator,
        } => {
            ensure_blank_line(lines);
            if state.table_idx >= state.table_col_widths.len() {
                state
                    .table_col_widths
                    .resize_with(state.table_idx + 1, Vec::new);
            }
            let pw = &mut state.table_col_widths[state.table_idx];
            let table = TableSource {
                rows: row_sources,
                separator,
            };
            lines.extend(render_table(
                rows,
                *header_end,
                ctx.width,
                pw,
                &table,
                ctx.math,
            ));
            ensure_blank_line(lines);
            state.table_idx += 1;
        }
        Block::Math { latex, source } => {
            ensure_blank_line(lines);
            // Every row points at the whole block: the Unicode is not a
            // slice of the source, and `\\` rows do not map to source lines.
            let rows = match ctx.math {
                MathStyle::Unicode => latex::to_unicode_rows(latex),
                MathStyle::Raw => None,
            };
            let rows = rows
                .filter(|r: &Vec<String>| !r.is_empty())
                .unwrap_or_else(|| {
                    latex
                        .lines()
                        .map(str::trim_end)
                        .map(str::to_owned)
                        .collect()
                });
            for row in rows {
                lines.push(Line {
                    kind: LineKind::Math,
                    spans: vec![Span::sourced(
                        row,
                        StyleToken::Math,
                        Emphasis::default(),
                        Source::atomic(source.clone()),
                    )],
                    source: Some(source.clone()),
                });
            }
            ensure_blank_line(lines);
        }
    }
}

fn render_line_block(lb: &LineBlock, lines: &mut Vec<Line>, ctx: &RenderCtx) {
    let source = Some(lb.source.clone());

    if matches!(lb.kind, BlockKind::HorizontalRule) {
        lines.push(Line {
            kind: LineKind::HorizontalRule,
            spans: vec![Span::chrome(hr_text(ctx.width), StyleToken::HorizontalRule)],
            source,
        });
        return;
    }

    let marker = block_prefix(&lb.kind).map(|p| Span::chrome(p, StyleToken::ListMarker));

    let is_heading = matches!(lb.kind, BlockKind::Heading(_));
    let kind = match &lb.kind {
        BlockKind::Heading(_) => LineKind::Heading,
        BlockKind::UnorderedListItem { .. } | BlockKind::OrderedListItem { .. } => {
            LineKind::ListItem
        }
        _ => LineKind::Paragraph,
    };

    let mut content_spans: Vec<Span> = Vec::new();
    for InlineSpan {
        text,
        kind: sk,
        emphasis,
        source,
        link,
    } in parse_inline_at(&lb.inline, lb.inline_start)
    {
        // Code and maths keep their own token inside headings so consumers
        // can layer colours on top. The Lua bridge collapses to one name.
        let (style, text) = match sk {
            SpanKind::Code => (StyleToken::InlineCode, text),
            SpanKind::Math => (StyleToken::Math, ctx.math.apply(&text).unwrap_or(text)),
            SpanKind::Text if is_heading => (StyleToken::Heading, text),
            SpanKind::Text => (StyleToken::Text, text),
        };
        content_spans.push(Span::sourced(text, style, emphasis, source).with_link(link));
    }

    let marker_width = marker.as_ref().map_or(0, |m| m.text.width());
    let width = ctx.width as usize;

    if width == 0 || !ctx.wrap_paragraphs {
        let mut spans = Vec::new();
        if let Some(m) = marker {
            spans.push(m);
        }
        spans.extend(content_spans);
        lines.push(Line {
            kind,
            spans,
            source,
        });
        return;
    }

    // If the marker is wider than the line, it gets its own row.
    // Otherwise it shares row 1 and continuations indent to align.
    let (first_row_marker, cont_indent, content_width) = if marker_width >= width {
        if let Some(mut mk) = marker {
            mk.text = mk.text.trim_start_matches(' ').to_owned();
            lines.push(Line {
                kind: kind.clone(),
                spans: vec![mk],
                source: source.clone(),
            });
        }
        (None, None, width)
    } else {
        let indent = marker
            .as_ref()
            .map(|_| Span::chrome(" ".repeat(marker_width), StyleToken::ListMarker));
        (marker, indent, width - marker_width)
    };

    let wrapped = wrap_spans(content_spans, content_width);

    if wrapped.is_empty() {
        if let Some(m) = first_row_marker {
            lines.push(Line {
                kind,
                spans: vec![m],
                source,
            });
        }
        return;
    }

    let mut first_row_marker = first_row_marker;
    for (i, row) in wrapped.into_iter().enumerate() {
        let mut spans = Vec::new();
        if i == 0 {
            if let Some(m) = first_row_marker.take() {
                spans.push(m);
            }
        } else if let Some(ref ind) = cont_indent {
            spans.push(ind.clone());
        }
        spans.extend(row);
        lines.push(Line {
            kind: kind.clone(),
            spans,
            source: source.clone(),
        });
    }
}

fn finalize_lines(lines: &mut Vec<Line>) {
    let mut write = 0;
    let mut prev_blank = false;
    for read in 0..lines.len() {
        let blank = lines[read].is_blank();
        if blank && prev_blank {
            continue;
        }
        if write != read {
            lines.swap(write, read);
        }
        write += 1;
        prev_blank = blank;
    }
    lines.truncate(write);
    while lines.last().is_some_and(Line::is_blank) {
        lines.pop();
    }
}

fn ensure_blank_line(lines: &mut Vec<Line>) {
    if !lines.last().is_some_and(Line::is_blank) {
        lines.push(Line::blank());
    }
}

fn fit_width(text: &str, max_width: usize) -> usize {
    let mut width = 0;
    for (i, ch) in text.char_indices() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + cw > max_width {
            return i;
        }
        width += cw;
    }
    text.len()
}

fn wrap_code_lines(lines: &mut Vec<Line>, start: usize, width: u16) {
    let width = width as usize;
    if width == 0 {
        return;
    }
    let tail = lines.split_off(start);
    for line in tail {
        if line.width() <= width {
            lines.push(line);
        } else {
            let source = line.source.clone();
            lines.extend(split_line_with_bar(line, width).into_iter().map(|mut l| {
                // Continuation rows stand for the same source line, so a
                // selection across the wrap copies it once.
                l.source = source.clone();
                l
            }));
        }
    }
}

fn split_line_with_bar(line: Line, width: usize) -> Vec<Line> {
    if line.spans.is_empty() {
        return vec![line];
    }

    let bar_span = line.spans[0].clone();
    let content_spans = &line.spans[1..];
    let first_avail = width.saturating_sub(CODE_BAR.width());
    let cont_avail = width.saturating_sub(CODE_BAR_WRAP.width());

    let mut result: Vec<Line> = Vec::new();
    let mut current_spans: Vec<Span> = vec![bar_span];
    let mut remaining = first_avail;

    for span in content_spans {
        let mut taken = 0;

        while taken < span.text.len() {
            let text = &span.text[taken..];
            let fits = fit_width(text, remaining);
            if fits == 0 {
                if current_spans.len() > 1 {
                    result.push(Line {
                        kind: LineKind::Code,
                        spans: mem::take(&mut current_spans),
                        source: None,
                    });
                    current_spans = vec![Span::chrome(CODE_BAR_WRAP, StyleToken::CodeBar)];
                    remaining = cont_avail;
                    continue;
                }
                let ch_len = text.chars().next().map_or(1, char::len_utf8);
                current_spans.push(span.slice(taken, taken + ch_len));
                taken += ch_len;
                result.push(Line {
                    kind: LineKind::Code,
                    spans: mem::take(&mut current_spans),
                    source: None,
                });
                current_spans = vec![Span::chrome(CODE_BAR_WRAP, StyleToken::CodeBar)];
                remaining = cont_avail;
                continue;
            }
            current_spans.push(span.slice(taken, taken + fits));
            remaining -= text[..fits].width();
            taken += fits;
            if taken < span.text.len() {
                result.push(Line {
                    kind: LineKind::Code,
                    spans: mem::take(&mut current_spans),
                    source: None,
                });
                current_spans = vec![Span::chrome(CODE_BAR_WRAP, StyleToken::CodeBar)];
                remaining = cont_avail;
            }
        }
    }

    if current_spans.len() > 1 || result.is_empty() {
        result.push(Line {
            kind: LineKind::Code,
            spans: current_spans,
            source: None,
        });
    }

    result
}

/// Measured from the spans that will be drawn, so a cell whose maths
/// shrinks from `\pi r^2` to `πr²` is not allotted the width of its source.
fn cell_display_width(cell: &str, math: MathStyle) -> usize {
    cell_spans(cell, false, None, math)
        .iter()
        .map(|s| s.text.width())
        .sum()
}

fn constrain_col_widths(col_widths: &mut [usize], available: usize) {
    let total: usize = col_widths.iter().sum();
    if total <= available {
        return;
    }
    for w in col_widths.iter_mut() {
        *w = (*w * available / total).max(MIN_COL_WIDTH).min(*w);
    }
    let mut excess = col_widths.iter().sum::<usize>().saturating_sub(available);
    while excess > 0 {
        let max_w = col_widths.iter().copied().max().unwrap_or(0);
        if max_w <= MIN_COL_WIDTH {
            break;
        }
        for w in col_widths.iter_mut() {
            if excess == 0 {
                break;
            }
            if *w == max_w && *w > MIN_COL_WIDTH {
                *w -= 1;
                excess -= 1;
            }
        }
    }
}

/// Soft-break on spaces, hard-break on char boundaries for long runs.
fn wrap_spans(spans: Vec<Span>, max_width: usize) -> Vec<Vec<Span>> {
    if max_width == 0 {
        return vec![spans];
    }
    let mut result: Vec<Vec<Span>> = Vec::new();
    let mut current: Vec<Span> = Vec::new();
    let mut remaining = max_width;

    for span in spans {
        let mut taken = 0;

        while taken < span.text.len() {
            let text = &span.text[taken..];
            let fits = fit_width(text, remaining);
            if fits == 0 {
                if current.is_empty() {
                    let ch_len = text.chars().next().map_or(1, char::len_utf8);
                    current.push(span.slice(taken, taken + ch_len));
                    taken += ch_len;
                }
                result.push(mem::take(&mut current));
                remaining = max_width;
                if span.text[taken..].starts_with(' ') {
                    taken += 1;
                }
                continue;
            }
            let (take, skip) = if fits < text.len() {
                match text[..fits].rfind(' ') {
                    Some(sp) if sp > 0 => (sp, sp + 1),
                    _ => (fits, fits),
                }
            } else {
                (fits, fits)
            };
            current.push(span.slice(taken, taken + take));
            remaining -= text[..take].width();
            taken += skip;
            if take < fits && taken < span.text.len() {
                result.push(mem::take(&mut current));
                remaining = max_width;
            }
        }
    }
    if !current.is_empty() || result.is_empty() {
        result.push(current);
    }
    result
}

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.text.width()).sum()
}

/// Cells arrive as owned strings with their pipe offsets already lost, so
/// every cell span points atomically at the whole source row. Selecting into
/// a table therefore copies parseable rows rather than bare cell text.
fn cell_spans(cell: &str, header: bool, row: Option<&Range<u32>>, math: MathStyle) -> Vec<Span> {
    parse_inline(cell)
        .into_iter()
        .map(
            |InlineSpan {
                 text,
                 kind,
                 emphasis,
                 source: _,
                 link,
             }| {
                let mut emphasis = emphasis;
                if header {
                    emphasis.bold = true;
                }
                let (style, text) = match kind {
                    SpanKind::Code => (StyleToken::InlineCode, text),
                    SpanKind::Math => (StyleToken::Math, math.apply(&text).unwrap_or(text)),
                    SpanKind::Text => (StyleToken::Text, text),
                };
                match row {
                    Some(range) => {
                        Span::sourced(text, style, emphasis, Source::atomic(range.clone()))
                    }
                    None => Span::with_emphasis(text, style, emphasis),
                }
                .with_link(link)
            },
        )
        .collect()
}

/// Source lines for a table. Cells are detached from the text by the time
/// they reach the renderer, so provenance stays at row granularity.
struct TableSource<'a> {
    rows: &'a [Range<u32>],
    separator: &'a Range<u32>,
}

fn render_table(
    rows: &[Vec<String>],
    header_end: usize,
    width: u16,
    persistent_widths: &mut Vec<usize>,
    table: &TableSource<'_>,
    math: MathStyle,
) -> Vec<Line> {
    let col_count = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if col_count == 0 {
        return Vec::new();
    }

    let overhead = col_count * 3 + 1;
    let min_box_width = overhead + col_count * MIN_COL_WIDTH;
    if (width as usize) < min_box_width {
        return render_table_compact(rows, header_end, width, table, math);
    }

    let mut col_widths = vec![0usize; col_count];
    for row in rows {
        for (c, cell) in row.iter().enumerate() {
            col_widths[c] = col_widths[c].max(cell_display_width(cell, math));
        }
    }

    let available = (width as usize) - overhead;

    persistent_widths.resize(persistent_widths.len().max(col_count), 0);
    for (i, w) in col_widths.iter_mut().enumerate() {
        persistent_widths[i] = persistent_widths[i].max(*w);
        *w = persistent_widths[i];
    }

    constrain_col_widths(&mut col_widths, available);

    let mut lines = Vec::new();

    let border = |left: &str, mid: &str, right: &str, source: Option<Range<u32>>| -> Line {
        let mut spans = vec![Span::chrome(left, StyleToken::TableBorder)];
        for (i, &w) in col_widths.iter().enumerate() {
            spans.push(Span::chrome("─".repeat(w + 2), StyleToken::TableBorder));
            if i < col_count - 1 {
                spans.push(Span::chrome(mid, StyleToken::TableBorder));
            }
        }
        spans.push(Span::chrome(right, StyleToken::TableBorder));
        Line {
            kind: LineKind::TableBorder,
            spans,
            source,
        }
    };

    lines.push(border("╭", "┬", "╮", None));

    for (ri, row) in rows.iter().enumerate() {
        let header = ri < header_end;

        let wrapped_cells: Vec<Vec<Vec<Span>>> = (0..col_count)
            .map(|c| {
                let cell = row.get(c).map(String::as_str).unwrap_or("");
                wrap_spans(
                    cell_spans(cell, header, table.rows.get(ri), math),
                    col_widths[c],
                )
            })
            .collect();

        let row_height = wrapped_cells.iter().map(|c| c.len()).max().unwrap_or(1);
        let row_emphasis = if header {
            Emphasis::BOLD
        } else {
            Emphasis::default()
        };

        for line_idx in 0..row_height {
            let mut spans = vec![Span::chrome("│ ", StyleToken::TableBorder)];
            for (c, &w) in col_widths.iter().enumerate() {
                let sub_line = wrapped_cells[c].get(line_idx);
                let content_width = sub_line.map_or(0, |sl| spans_width(sl));

                let pad = w.saturating_sub(content_width);

                if let Some(sl) = sub_line {
                    spans.extend(sl.iter().cloned());
                }
                let mut padding = Span::chrome(" ".repeat(pad + 1), StyleToken::Text);
                padding.emphasis = row_emphasis;
                spans.push(padding);
                if c < col_count - 1 {
                    spans.push(Span::chrome("│ ", StyleToken::TableBorder));
                } else {
                    spans.push(Span::chrome("│", StyleToken::TableBorder));
                }
            }
            lines.push(Line {
                kind: LineKind::TableRow,
                spans,
                source: table.rows.get(ri).cloned(),
            });
        }

        if ri + 1 < rows.len() {
            // The divider stands in for the `| --- |` line that `rows` drops,
            // so copying a whole table yields parseable markdown.
            let divider = (ri + 1 == header_end).then(|| table.separator.clone());
            lines.push(border("├", "┼", "┤", divider));
        }
    }

    lines.push(border("╰", "┴", "╯", None));

    lines
}

pub fn hr_text(width: u16) -> String {
    iter::repeat_n(HR_CHAR, width as usize).collect()
}

/// Collapses source ranges into the fewest slices that reproduce the
/// original text. Ranges separated only by whitespace merge, so the blank
/// lines between blocks survive. Any other gap stays split: it holds content
/// that was deliberately not rendered, such as collapsed tool output, and
/// copying across it must not resurrect it.
pub fn merge_source_ranges(
    text: &str,
    ranges: impl IntoIterator<Item = Range<u32>>,
) -> Vec<Range<u32>> {
    let mut merged: Vec<Range<u32>> = Vec::new();
    for range in ranges {
        match merged.last_mut() {
            Some(prev)
                if range.start <= prev.end
                    || text
                        .get(prev.end as usize..range.start as usize)
                        .is_some_and(|gap| gap.chars().all(char::is_whitespace)) =>
            {
                prev.end = prev.end.max(range.end);
            }
            _ => merged.push(range),
        }
    }
    merged
}

/// The source behind a set of ranges. Non-contiguous runs join with a
/// newline so unrendered content between them stays out.
pub fn source_text(text: &str, ranges: impl IntoIterator<Item = Range<u32>>) -> String {
    merge_source_ranges(text, ranges)
        .into_iter()
        .filter_map(|r| text.get(r.start as usize..r.end as usize))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn truncate_long_lines(text: &str) -> Cow<'_, str> {
    truncate_long_lines_at(text, TOOL_OUTPUT_MAX_LINE_BYTES)
}

pub fn truncate_long_lines_at(text: &str, max_bytes: usize) -> Cow<'_, str> {
    if !text.lines().any(|l| l.len() > max_bytes) {
        return Cow::Borrowed(text);
    }
    let mut result = String::with_capacity(text.len());
    for (i, line) in text.lines().enumerate() {
        if i > 0 {
            result.push('\n');
        }
        if line.len() > max_bytes {
            let mut boundary = max_bytes;
            while !line.is_char_boundary(boundary) {
                boundary -= 1;
            }
            result.push_str(&line[..boundary]);
            result.push_str(LONG_LINE_SUFFIX);
        } else {
            result.push_str(line);
        }
    }
    if text.ends_with('\n') {
        result.push('\n');
    }
    Cow::Owned(result)
}

/// Fallback when the terminal is too narrow for box-drawing borders.
fn render_table_compact(
    rows: &[Vec<String>],
    header_end: usize,
    width: u16,
    table: &TableSource<'_>,
    math: MathStyle,
) -> Vec<Line> {
    const CELL_SEP: &str = " | ";
    // No divider row exists here, so the `| --- |` line rides on the row it
    // follows. Without it a copied table would not parse back.
    let row_source = |ri: usize| -> Option<Range<u32>> {
        let row = table.rows.get(ri)?.clone();
        Some(match ri + 1 == header_end {
            true => row.start..table.separator.end.max(row.end),
            false => row,
        })
    };
    let mut lines = Vec::new();
    for (ri, row) in rows.iter().enumerate() {
        let header = ri < header_end;
        let mut spans: Vec<Span> = Vec::new();
        for (c, cell) in row.iter().enumerate() {
            if c > 0 {
                spans.push(Span::chrome(CELL_SEP, StyleToken::TableBorder));
            }
            spans.extend(cell_spans(cell, header, row_source(ri).as_ref(), math));
        }
        for row_spans in wrap_spans(spans, width as usize) {
            lines.push(Line {
                kind: LineKind::TableRow,
                spans: row_spans,
                source: row_source(ri),
            });
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const TEST_WIDTH: u16 = 80;

    fn lines_text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.text.as_str()).collect())
            .collect()
    }

    fn find_span<'a>(lines: &'a [Line], text: &str) -> Option<&'a Span> {
        lines
            .iter()
            .flat_map(|l| &l.spans)
            .find(|s| s.text.trim() == text)
    }

    #[test]
    fn render_empty_input_yields_no_lines() {
        assert!(render("", TEST_WIDTH).is_empty());
    }

    /// Merge the source ranges of every rendered row and slice them back out
    /// of the input. Runs that are not contiguous join with a newline, which
    /// is what the copy path does.
    fn rebuild_from_line_sources(text: &str, width: u16) -> String {
        let lines = render(text, width);
        source_text(text, lines.iter().filter_map(|l| l.source.clone()))
    }

    const ROUND_TRIP_DOC: &str = "# Title with **bold**\n\nA paragraph with `code` and *italic*.\n\n- first item\n- second item\n\n1. ordered\n2. also ordered\n\n```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n\n| Name | Value |\n| --- | --- |\n| foo | 42 |\n\n---\n\nTrailing paragraph.";

    #[test_case(ROUND_TRIP_DOC; "mixed_document")]
    #[test_case("# Heading"; "heading_hashes")]
    #[test_case("**bold** and _italic_"; "emphasis_delimiters")]
    #[test_case("- a\n- b"; "bullets")]
    #[test_case("```\ncode\n```"; "fenced_block")]
    #[test_case("| a | b |\n| --- | --- |\n| 1 | 2 |"; "table_pipes")]
    #[test_case("---"; "horizontal_rule")]
    #[test_case("a `x|y` b"; "inline_code")]
    fn line_sources_reconstruct_the_source(input: &str) {
        assert_eq!(rebuild_from_line_sources(input, TEST_WIDTH), input);
    }

    #[test]
    fn line_sources_survive_narrow_widths() {
        // Wrapping splits rows but must not duplicate or drop source bytes.
        for width in [10, 20, 40] {
            assert_eq!(
                rebuild_from_line_sources(ROUND_TRIP_DOC, width),
                ROUND_TRIP_DOC,
                "width {width}"
            );
        }
    }

    #[test]
    fn leading_newlines_do_not_shift_source_ranges() {
        let input = "\n\n# Title";
        assert_eq!(rebuild_from_line_sources(input, TEST_WIDTH), "# Title");
    }

    #[test]
    fn span_sources_are_verbatim_slices_of_the_input() {
        let lines = render(ROUND_TRIP_DOC, TEST_WIDTH);
        let mut checked = 0;
        for span in lines.iter().flat_map(|l| &l.spans) {
            if let SpanSource::Range(Source {
                range,
                verbatim: true,
            }) = &span.source
            {
                let slice = &ROUND_TRIP_DOC[range.start as usize..range.end as usize];
                assert_eq!(
                    slice, span.text,
                    "span {:?} misreports its range",
                    span.text
                );
                checked += 1;
            }
        }
        assert!(checked > 0, "expected verbatim spans to check");
    }

    fn math_spans(lines: &[Line]) -> Vec<&str> {
        lines
            .iter()
            .flat_map(|l| &l.spans)
            .filter(|s| s.style == StyleToken::Math)
            .map(|s| s.text.as_str())
            .collect()
    }

    #[test_case("Value $x^2$ here", &["x²"]; "inline_dollar")]
    #[test_case(r"Value \(x^2\) here", &["x²"]; "inline_paren")]
    #[test_case("Both $$a+b$$ inline", &["a+b"]; "same_line_double_dollar")]
    #[test_case(r"Sum $\sum_{i=1}^{n} i$", &["∑ᵢ₌₁ⁿ i"]; "inline_sum")]
    #[test_case("$$\nE = mc^2\n$$", &["E = mc²"]; "display_block")]
    #[test_case("\\[\nx^2\n\\]", &["x²"]; "display_bracket")]
    #[test_case("$$a = b \\\\ c = d$$", &["a = b", "c = d"]; "display_rows")]
    fn math_renders_as_unicode(input: &str, expected: &[&str]) {
        assert_eq!(math_spans(&render(input, TEST_WIDTH)), expected);
    }

    #[test_case("It costs $5 and $10 total"; "currency_pair")]
    #[test_case("Cost is $5 today"; "single_price")]
    #[test_case("Empty $$ delimiters"; "empty_content")]
    #[test_case("Unclosed $x^2 stays plain"; "unterminated")]
    fn non_math_dollars_stay_plain(input: &str) {
        let lines = render(input, TEST_WIDTH);
        assert!(math_spans(&lines).is_empty(), "{input:?} became maths");
        let visible: String = lines[0].spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(visible, input);
    }

    /// Table cells are parsed separately from prose, so they need their own
    /// maths handling and their own width measurement to match.
    #[test]
    fn table_cells_render_maths_and_size_to_it() {
        let lines = render("| a | b |\n| --- | --- |\n| $\\pi r^2$ | x |", TEST_WIDTH);
        assert_eq!(math_spans(&lines), ["π r²"]);
        let body = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.text.contains("π r²")))
            .expect("body row");
        let header = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.text.contains('a')))
            .expect("header row");
        let width = |l: &Line| l.spans.iter().map(|s| s.text.width()).sum::<usize>();
        assert_eq!(width(body), width(header), "columns must line up");
    }

    #[test]
    fn latex_signal_beats_the_currency_heuristic() {
        assert_eq!(math_spans(&render("Try $2^n$ here", TEST_WIDTH)), ["2ⁿ"]);
    }

    /// Before display maths was a block, the line classifier saw its rows:
    /// `- x` became a bullet and `---` a horizontal rule.
    #[test]
    fn display_math_rows_are_not_classified_as_markdown() {
        let lines = render("$$\n- x\n---\n# y\n$$", TEST_WIDTH);
        assert!(
            lines
                .iter()
                .all(|l| matches!(l.kind, LineKind::Math | LineKind::Blank)),
            "maths rows leaked into the line classifier: {lines:#?}"
        );
    }

    #[test]
    fn math_inside_code_is_not_math() {
        let lines = render("```\n$$\nx^2\n$$\n```", TEST_WIDTH);
        assert!(math_spans(&lines).is_empty());
        assert!(lines.iter().any(|l| l.kind == LineKind::Code));
    }

    #[test]
    fn inline_math_is_atomic_and_reports_its_delimiters() {
        let input = "a $x^2$ b";
        let lines = render(input, TEST_WIDTH);
        let span = lines[0]
            .spans
            .iter()
            .find(|s| s.style == StyleToken::Math)
            .expect("maths span");
        let SpanSource::Range(source) = &span.source else {
            panic!("maths span must carry a source")
        };
        assert!(!source.verbatim, "maths must copy atomically");
        assert_eq!(
            &input[source.range.start as usize..source.range.end as usize],
            "$x^2$"
        );
    }

    #[test_case("Value $x^2$ here"; "inline")]
    #[test_case("$$\nE = mc^2\n$$"; "display")]
    #[test_case("Text\n\n$$\n\\frac{a}{b}\n$$\n\nMore"; "display_between_paragraphs")]
    fn math_round_trips_to_source(input: &str) {
        assert_eq!(rebuild_from_line_sources(input, TEST_WIDTH), input);
    }

    #[test]
    fn raw_style_shows_latex_source() {
        let lines = Renderer::new().with_math(MathStyle::Raw).render(
            "$x^2$ and\n\n$$\n\\alpha\n$$",
            TEST_WIDTH,
            0,
        );
        assert_eq!(math_spans(&lines), ["x^2", "\\alpha"]);
    }

    #[test]
    fn chrome_spans_carry_no_source() {
        let lines = render("- item\n\n```\nx\n```", TEST_WIDTH);
        for span in lines.iter().flat_map(|l| &l.spans) {
            if matches!(span.style, StyleToken::ListMarker | StyleToken::CodeBar) {
                assert_eq!(span.source, SpanSource::Chrome, "{:?}", span.text);
            }
        }
    }

    #[test]
    fn render_bold_emits_text_with_bold_emphasis() {
        let lines = render("**bold**", TEST_WIDTH);
        let span = find_span(&lines, "bold").expect("bold span");
        assert_eq!(span.style, StyleToken::Text);
        assert_eq!(span.emphasis, Emphasis::BOLD);
    }

    #[test]
    fn render_inline_code_emits_inline_code_token() {
        let lines = render("a `b` c", TEST_WIDTH);
        let code = find_span(&lines, "b").expect("code span");
        assert_eq!(code.style, StyleToken::InlineCode);
    }

    #[test]
    fn render_link_preserves_target_across_nested_styles() {
        let lines = render("[**bold** and `code`](https://example.com)", TEST_WIDTH);
        let spans = &lines[0].spans;
        assert_eq!(spans.len(), 3);
        assert!(
            spans
                .iter()
                .all(|span| span.link.as_deref() == Some("https://example.com"))
        );
        assert_eq!(spans[0].emphasis, Emphasis::BOLD);
        assert_eq!(spans[2].style, StyleToken::InlineCode);
    }

    #[test]
    fn render_wrapped_link_slices_keep_target() {
        let lines = render("[abcdefghij](https://example.com)", 4);
        assert!(lines.len() > 1);
        assert!(
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .all(|span| span.link.as_deref() == Some("https://example.com"))
        );
    }

    #[test]
    fn render_table_link_keeps_target() {
        let lines = render(
            "| Link |\n| --- |\n| [site](https://example.com) |",
            TEST_WIDTH,
        );
        let link = find_span(&lines, "site").expect("table link span");
        assert_eq!(link.link.as_deref(), Some("https://example.com"));
    }

    #[test_case(1; "h1")]
    #[test_case(3; "h3")]
    #[test_case(6; "h6")]
    fn render_heading_emits_heading_kind_and_token(level: u8) {
        let input = format!("{} hello", "#".repeat(level as usize));
        let lines = render(&input, TEST_WIDTH);
        assert_eq!(lines[0].kind, LineKind::Heading);
        let hello = lines[0]
            .spans
            .iter()
            .find(|s| s.text == "hello")
            .expect("hello span");
        assert_eq!(hello.style, StyleToken::Heading);
    }

    #[test]
    fn render_heading_preserves_inline_styles() {
        let lines = render("## **bold** and `code`", TEST_WIDTH);
        assert_eq!(
            find_span(&lines, "code").unwrap().style,
            StyleToken::InlineCode
        );
        assert_eq!(
            find_span(&lines, "bold").unwrap().style,
            StyleToken::Heading
        );
    }

    #[test]
    fn render_horizontal_rule_emits_hr_token() {
        let lines = render("---", TEST_WIDTH);
        assert_eq!(lines[0].kind, LineKind::HorizontalRule);
        assert_eq!(lines[0].spans[0].style, StyleToken::HorizontalRule);
    }

    #[test]
    fn render_unordered_list_marker_then_content() {
        let lines = render("- item", TEST_WIDTH);
        assert_eq!(lines[0].kind, LineKind::ListItem);
        assert_eq!(lines[0].spans[0].text, "• ");
        assert_eq!(lines[0].spans[0].style, StyleToken::ListMarker);
        assert_eq!(lines[0].spans[1].text, "item");
    }

    #[test]
    fn render_code_block_emits_code_bar_then_highlight_tokens() {
        let lines = render("```rust\nfn x() {}\n```", TEST_WIDTH);
        let code_lines: Vec<_> = lines.iter().filter(|l| l.kind == LineKind::Code).collect();
        assert!(!code_lines.is_empty());
        assert_eq!(code_lines[0].spans[0].style, StyleToken::CodeBar);
        assert!(
            code_lines[0]
                .spans
                .iter()
                .skip(1)
                .all(|s| matches!(s.style, StyleToken::Highlight { .. })),
            "code line content spans must be highlight tokens"
        );
    }

    #[test]
    fn render_code_block_wraps_long_lines_with_continuation_bar() {
        let code = "a".repeat(40);
        let input = format!("```\n{code}\n```");
        let lines = render(&input, 15);
        let code_lines: Vec<_> = lines.iter().filter(|l| l.kind == LineKind::Code).collect();
        assert!(code_lines.len() > 1, "long code line should wrap");
        for line in &code_lines {
            assert!(line.width() <= 15);
            assert_eq!(line.spans[0].style, StyleToken::CodeBar);
        }
        let bar_text: Vec<_> = code_lines
            .iter()
            .map(|l| l.spans[0].text.as_str())
            .collect();
        assert_eq!(bar_text[0], CODE_BAR);
        assert_eq!(bar_text[1], CODE_BAR_WRAP);
    }

    #[test]
    fn render_code_block_narrow_width_does_not_loop() {
        let input = "```\n\u{4e16}\u{754c}\n```";
        for w in 1..=3 {
            let lines = render(input, w);
            let code_lines: Vec<_> = lines.iter().filter(|l| l.kind == LineKind::Code).collect();
            assert!(
                !code_lines.is_empty(),
                "width={w} should produce code lines"
            );
        }
    }

    #[test]
    fn render_table_emits_borders_and_rows() {
        let lines = render("| H |\n| --- |\n| d |", TEST_WIDTH);
        assert!(
            lines
                .iter()
                .filter(|l| l.kind == LineKind::TableBorder)
                .count()
                >= 2
        );
        assert!(lines.iter().any(|l| l.kind == LineKind::TableRow));
    }

    #[test]
    fn render_table_wraps_overflowing_cells_within_width() {
        let long = "x".repeat(60);
        let input = format!("| Col1 | Col2 |\n| --- | --- |\n| short | {long} |");
        let width: u16 = 40;
        let lines = render(&input, width);
        assert!(
            lines.iter().all(|l| l.width() <= width as usize),
            "line overflow"
        );
        let x_count: usize = lines_text(&lines).join("").matches('x').count();
        assert_eq!(x_count, 60, "wrap must preserve content");
    }

    #[test]
    fn render_never_emits_consecutive_blanks() {
        let lines = render("before\n```\ncode\n```\nafter", TEST_WIDTH);
        let consecutive = lines.windows(2).any(|w| w[0].is_blank() && w[1].is_blank());
        assert!(!consecutive, "should never have two consecutive blanks");
    }

    #[test]
    fn renderer_caches_table_column_widths_across_calls() {
        let mut r = Renderer::new();
        r.render("| A | B |\n| --- | --- |\n| hi | there |", 120, 0);
        let widths_before = r.table_col_widths[0].clone();
        r.render(
            "| A | B |\n| --- | --- |\n| hi | there |\n| longer | x |",
            120,
            0,
        );
        for (i, (&old, &new)) in widths_before.iter().zip(&r.table_col_widths[0]).enumerate() {
            assert!(new >= old, "table width shrank at col {i}: {old} -> {new}");
        }
    }

    #[test]
    fn render_width_zero_does_not_panic() {
        let _ = render("```\nhello\n```", 0);
    }

    const FLOWCHART: &str = "```mermaid\nflowchart TD\n  A[Start] --> B[Stop]\n```";
    const UNSUPPORTED: &str = "```mermaid\nsequenceDiagram\n  A ->> B: hi\n```";

    fn diagram_rows(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .filter(|line| matches!(line.kind, LineKind::Diagram { .. }))
            .map(|line| line.spans.iter().map(|s| s.text.as_str()).collect())
            .collect()
    }

    fn mermaid_render(text: &str, width: u16) -> Vec<Line> {
        Renderer::new()
            .with_mermaid(MermaidStyle::Unicode)
            .render(text, width, 0)
    }

    #[test]
    fn a_flowchart_fence_becomes_diagram_lines() {
        let lines = mermaid_render(FLOWCHART, TEST_WIDTH);
        let rows = diagram_rows(&lines);
        assert!(!rows.is_empty(), "expected drawn rows");
        assert!(rows.iter().any(|row| row.contains("Start")), "{rows:?}");
        assert!(rows.iter().any(|row| row.contains('▼')), "{rows:?}");
        assert!(
            !lines_text(&lines).iter().any(|row| row.contains("-->")),
            "the source must not survive alongside the drawing"
        );
    }

    #[test_case(UNSUPPORTED                                     ; "other_diagram_family")]
    #[test_case("```mermaid\nflowchart TD\n  A ~~~ B\n```"      ; "unknown_operator")]
    #[test_case("```mermaid\nflowchart TD\n  A --> A\n```"      ; "self_loop")]
    fn unsupported_mermaid_falls_back_to_a_code_block(source: &str) {
        let lines = mermaid_render(source, TEST_WIDTH);
        assert!(diagram_rows(&lines).is_empty());
        assert!(lines.iter().any(|line| line.kind == LineKind::Code));
    }

    #[test]
    fn mermaid_off_leaves_the_fence_as_code() {
        let lines = Renderer::new()
            .with_mermaid(MermaidStyle::Off)
            .render(FLOWCHART, TEST_WIDTH, 0);
        assert!(diagram_rows(&lines).is_empty());
        assert!(lines.iter().any(|line| line.kind == LineKind::Code));
    }

    #[test]
    fn an_unterminated_fence_stays_code_while_it_streams() {
        let partial = "```mermaid\nflowchart TD\n  A[Start] --> B[Stop]";
        assert!(diagram_rows(&mermaid_render(partial, TEST_WIDTH)).is_empty());
        assert!(!diagram_rows(&mermaid_render(FLOWCHART, TEST_WIDTH)).is_empty());
    }

    #[test_case(80 ; "wide")]
    #[test_case(20 ; "narrow")]
    #[test_case(4  ; "tiny")]
    fn diagram_rows_never_exceed_the_width(width: u16) {
        let lines = mermaid_render(FLOWCHART, width);
        for row in diagram_rows(&lines) {
            assert!(row.width() <= width as usize, "{:?} at {width}", row);
        }
    }

    #[test]
    fn a_clipped_diagram_is_marked_and_a_fitting_one_is_not() {
        let narrow = diagram_rows(&mermaid_render(FLOWCHART, 8));
        assert!(narrow.iter().all(|row| row.ends_with(DIAGRAM_MORE_RIGHT)));
        let wide = diagram_rows(&mermaid_render(FLOWCHART, TEST_WIDTH));
        assert!(wide.iter().all(|row| !row.ends_with(DIAGRAM_MORE_RIGHT)));
    }

    fn panned_rows(pan: u16, width: u16) -> Vec<String> {
        diagram_rows(
            &Renderer::new()
                .with_mermaid(MermaidStyle::Unicode)
                .with_diagram_pans(vec![pan])
                .render(FLOWCHART, width, 0),
        )
    }

    #[test]
    fn panning_shifts_the_window_without_changing_the_row_count() {
        let (start, panned) = (panned_rows(0, 8), panned_rows(3, 8));
        assert_eq!(start.len(), panned.len(), "pan must not change height");
        assert_ne!(start, panned, "pan must move the window");
    }

    #[test]
    fn a_panned_diagram_is_marked_on_the_left() {
        assert!(
            panned_rows(0, 8)
                .iter()
                .all(|row| !row.starts_with(DIAGRAM_MORE_LEFT))
        );
        assert!(
            panned_rows(3, 8)
                .iter()
                .all(|row| row.starts_with(DIAGRAM_MORE_LEFT))
        );
    }

    #[test]
    fn panning_to_the_limit_reveals_the_last_column() {
        const WIDTH: u16 = 12;
        let full = match mermaid_render(FLOWCHART, WIDTH)
            .iter()
            .find_map(|line| match line.kind {
                LineKind::Diagram { full_width, .. } => Some(full_width),
                _ => None,
            }) {
            Some(width) => width,
            None => panic!("expected a diagram"),
        };
        let rows = panned_rows(diagram_max_pan(full, WIDTH), WIDTH);
        assert!(
            rows.iter().all(|row| !row.ends_with(DIAGRAM_MORE_RIGHT)),
            "the far edge must be reachable: {rows:?}"
        );
        let unpanned = panned_rows(0, WIDTH);
        let tail: String = unpanned.concat();
        assert!(!tail.is_empty());
    }

    #[test]
    fn a_pan_past_the_limit_clamps_instead_of_emptying_the_window() {
        let rows = panned_rows(u16::MAX, 12);
        assert!(rows.iter().any(|row| row.chars().count() > 1), "{rows:?}");
    }

    #[test]
    fn copying_a_diagram_returns_the_mermaid_source() {
        let lines = mermaid_render(FLOWCHART, TEST_WIDTH);
        let ranges = lines
            .iter()
            .filter(|line| matches!(line.kind, LineKind::Diagram { .. }))
            .filter_map(|line| line.source.clone());
        let merged = merge_source_ranges(FLOWCHART, ranges);
        assert_eq!(source_text(FLOWCHART, merged), FLOWCHART);
    }

    #[test]
    fn every_drawn_span_is_atomic_so_partial_selections_stay_whole() {
        let lines = mermaid_render(FLOWCHART, TEST_WIDTH);
        for line in lines
            .iter()
            .filter(|line| matches!(line.kind, LineKind::Diagram { .. }))
        {
            for span in &line.spans {
                match &span.source {
                    SpanSource::Range(source) => assert!(!source.verbatim, "{span:?}"),
                    SpanSource::Chrome => {}
                    other => panic!("unexpected provenance {other:?}"),
                }
            }
        }
    }

    #[test]
    fn a_diagram_keeps_its_neighbours_highlighters_aligned() {
        let text = format!("```rust\nfn a() {{}}\n```\n\n{FLOWCHART}\n\n```rust\nfn b() {{}}\n```");
        let mut renderer = Renderer::new().with_mermaid(MermaidStyle::Unicode);
        let first = renderer.render(&text, TEST_WIDTH, 0);
        let second = renderer.render(&text, TEST_WIDTH, 0);
        assert_eq!(lines_text(&first), lines_text(&second));
        assert!(lines_text(&second).iter().any(|row| row.contains("fn b()")));
    }

    #[test]
    fn render_table_header_row_cells_are_bold() {
        let lines = render("| Header |\n| --- |\n| Data |", TEST_WIDTH);
        let header = find_span(&lines, "Header").expect("header span");
        assert!(header.emphasis.bold, "header cells must be bold");
    }

    #[test]
    fn truncate_long_lines_behavior() {
        let max = TOOL_OUTPUT_MAX_LINE_BYTES;

        assert_eq!(&*truncate_long_lines("short\nlines\n"), "short\nlines\n");
        assert_eq!(&*truncate_long_lines(&"a".repeat(max)), "a".repeat(max));
        assert_eq!(
            &*truncate_long_lines(&"a".repeat(max + 1)),
            format!("{}{LONG_LINE_SUFFIX}", "a".repeat(max))
        );

        let mut multibyte = "a".repeat(max - 1);
        multibyte.push('\u{00e9}');
        let result = truncate_long_lines(&multibyte);
        assert!(result.ends_with(LONG_LINE_SUFFIX));
        assert!(!result.contains('\u{00e9}'));

        let with_nl = format!("{}\n", "z".repeat(max + 10));
        assert!(truncate_long_lines(&with_nl).ends_with('\n'));
        let without_nl = "z".repeat(max + 10);
        assert!(!truncate_long_lines(&without_nl).ends_with('\n'));
    }

    #[test]
    fn streaming_matches_oneshot() {
        const CORPUS: &[&str] = &[
            "hello world\n# heading\n\npara",
            "```rust\nfn main() {}\n```",
            "| H1 | H2 |\n| --- | --- |\n| a | b |\n| c | d |",
            "## title with `code`\n\n- one\n- two\n- three\n\n```py\nx=1\ny=2\n```\nend",
        ];
        const WIDTHS: &[u16] = &[20, 40, 80];
        for text in CORPUS {
            for &w in WIDTHS {
                let oneshot = Renderer::new().render(text, w, 0);
                let mut streamer = Renderer::new();
                for end in 1..text.len() {
                    if !text.is_char_boundary(end) {
                        continue;
                    }
                    let _ = streamer.render(&text[..end], w, 0);
                }
                let final_streamed = streamer.render(text, w, 0);
                assert_eq!(final_streamed, oneshot, "mismatch text={text:?} width={w}");
            }
        }
    }

    #[test]
    fn finalize_lines_collapses_consecutive_blanks() {
        let para = |t| Line {
            kind: LineKind::Paragraph,
            spans: vec![Span::new(t, StyleToken::Text)],
            source: None,
        };
        let mut lines = vec![
            para("a"),
            Line::blank(),
            Line::blank(),
            Line::blank(),
            para("b"),
            Line::blank(),
            para("c"),
            Line::blank(),
            Line::blank(),
        ];
        finalize_lines(&mut lines);
        assert!(!lines.last().is_some_and(Line::is_blank));
        assert!(!lines.windows(2).any(|w| w[0].is_blank() && w[1].is_blank()));
        assert_eq!(lines_text(&lines), vec!["a", "", "b", "", "c"]);
    }

    #[test]
    fn theme_gen_change_clears_highlighter_cache() {
        let mut r = Renderer::new();
        let code = "```rust\nlet x = 42;\n```";
        r.render(code, TEST_WIDTH, 0);
        assert_eq!(r.highlighters.len(), 1);
        r.render(code, TEST_WIDTH, 1);
        assert_eq!(r.theme_gen, 1);
        assert_eq!(r.highlighters.len(), 1);
    }

    #[test]
    fn unwrapped_mode_skips_paragraph_wrap_but_wraps_code() {
        let long_para = "word ".repeat(50);
        let mut r = Renderer::unwrapped();
        let para_lines = r.render(long_para.trim(), 30, 0);
        assert_eq!(
            para_lines
                .iter()
                .filter(|l| l.kind == LineKind::Paragraph)
                .count(),
            1
        );

        let long_code = "a".repeat(60);
        let input = format!("```\n{long_code}\n```");
        let code_lines = r.render(&input, 20, 0);
        assert!(
            code_lines
                .iter()
                .filter(|l| l.kind == LineKind::Code)
                .count()
                > 1
        );
    }

    #[test]
    fn table_compact_fallback_at_small_width() {
        let lines = render("| aa | bb |\n| --- | --- |\n| cc | dd |", 10);
        assert!(
            !lines.iter().any(|l| l.kind == LineKind::TableBorder),
            "compact: no borders"
        );
        assert!(
            lines.iter().any(|l| l.kind == LineKind::TableRow),
            "compact: has rows"
        );
    }

    #[test]
    fn multiple_code_blocks_get_separate_highlighters() {
        let mut r = Renderer::new();
        r.render(
            "```rust\nfn a() {}\n```\n\n```python\nx = 1\n```",
            TEST_WIDTH,
            0,
        );
        assert_eq!(r.highlighters.len(), 2);
        r.render("```rust\nfn a() {}\n```", TEST_WIDTH, 0);
        assert_eq!(r.highlighters.len(), 1);
    }

    #[test]
    fn paragraph_wrapping_preserves_all_content() {
        const INPUT: &str = "The **quick** brown _fox_ jumps over the `lazy` dog repeatedly";
        let lines = render(INPUT, 20);
        let rendered: String = lines
            .iter()
            .flat_map(|l| &l.spans)
            .map(|s| s.text.as_str())
            .collect();
        let expected = INPUT.replace("**", "").replace(['_', '`'], "");
        assert_eq!(
            rendered, expected,
            "wrapped output must preserve all visible text"
        );
    }

    #[test]
    fn coalesce_merges_same_style_splits_different() {
        let mut spans = vec![
            Span::new("aa", StyleToken::Text),
            Span::new("bb", StyleToken::Text),
            Span::new("cc", StyleToken::InlineCode),
            Span::new("dd", StyleToken::InlineCode),
            Span::new("plain", StyleToken::Text),
            Span::with_emphasis("bold", StyleToken::Text, Emphasis::BOLD),
        ];
        coalesce_adjacent_spans(&mut spans);
        assert_eq!(spans.len(), 4);
        assert_eq!(spans[0].text, "aabb");
        assert_eq!(spans[1].text, "ccdd");
        assert_eq!(spans[2].text, "plain");
        assert_eq!(spans[3].text, "bold");
    }

    #[test_case(10 ; "narrow")]
    #[test_case(40 ; "medium")]
    #[test_case(120 ; "wide")]
    fn table_with_empty_cell_does_not_panic(width: u16) {
        let lines = render("| a | | c |\n| --- | --- | --- |\n| d | | f |", width);
        assert!(
            lines
                .iter()
                .filter(|l| l.kind == LineKind::TableRow)
                .count()
                >= 2
        );
    }

    #[test]
    fn ordered_list_emits_correct_marker() {
        let lines = render("1. first", TEST_WIDTH);
        assert_eq!(lines[0].kind, LineKind::ListItem);
        assert_eq!(lines[0].spans[0].style, StyleToken::ListMarker);
        assert!(find_span(&lines, "first").is_some());
    }

    #[test]
    fn wrap_spans_hard_breaks_unbreakable_run() {
        let long_word = "x".repeat(30);
        let wrapped = wrap_spans(vec![Span::new(long_word.clone(), StyleToken::Text)], 10);
        assert!(wrapped.len() >= 3);
        let reassembled: String = wrapped
            .iter()
            .flat_map(|row| row.iter())
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(reassembled, long_word);
        assert!(wrapped.iter().all(|row| spans_width(row) <= 10));
    }

    #[test]
    fn render_leading_newlines_are_stripped() {
        let lines = render("\n\n\nhello", TEST_WIDTH);
        assert!(!lines.is_empty());
        assert_eq!(find_span(&lines, "hello").unwrap().text, "hello");
    }

    #[test]
    fn truncate_long_line_preserves_table_detection() {
        let long_cell = "A".repeat(500);
        let text = format!("| What | Why |\n| --- | --- |\n| Short cell | {long_cell} |");

        let full = Renderer::unwrapped().render(&text, 120, 0);
        assert!(
            full.iter().any(|l| l.kind == LineKind::TableBorder),
            "full text must parse as a table"
        );

        let truncated = truncate_long_lines_at(&text, 500);
        assert_ne!(truncated.as_ref(), text, "truncation must modify the text");

        let trunc = Renderer::unwrapped().render(&truncated, 120, 0);
        assert!(
            trunc.iter().any(|l| l.kind == LineKind::TableBorder),
            "table detection survives truncation (parser is lenient about missing closing |)"
        );
    }
}
