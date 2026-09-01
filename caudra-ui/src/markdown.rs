use std::borrow::Cow;
use std::collections::VecDeque;
use std::mem;
use std::ops::Range;
use std::sync::Arc;

use crate::provenance::LineProvenance;
use crate::theme;
use crate::theme::Theme;
use caudra_markdown::Emphasis;
use caudra_markdown::render::{
    self, Line as RLine, LineKind, Span as RSpan, SpanSource, StyleToken,
};
use ratatui::buffer::CellWidth;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use url::Url;

pub const TRUNCATION_PREFIX: &str = "...";
const MIN_TRUNCATABLE_LINES: usize = 2;

/// Add `over`'s modifiers on top of `base`, keeping `base`'s colors.
fn apply_modifiers(base: Style, over: Style) -> Style {
    base.add_modifier(over.add_modifier)
        .remove_modifier(over.sub_modifier)
}

/// Recolor `base` with `over`'s colors and OR the modifiers.
fn overlay_style(base: Style, over: Style) -> Style {
    let mut out = base;
    if let Some(fg) = over.fg {
        out.fg = Some(fg);
    }
    if let Some(bg) = over.bg {
        out.bg = Some(bg);
    }
    apply_modifiers(out, over)
}

/// When `preserve_base_color` is set (headings), emphasis only adds modifiers
/// so the heading colour survives. Otherwise it recolors from the theme.
fn apply_emphasis(base: Style, emphasis: Emphasis, preserve_base_color: bool, t: &Theme) -> Style {
    let emph_style = match (emphasis.bold, emphasis.italic) {
        (true, true) => Some(t.bold_italic),
        (true, false) => Some(t.bold),
        (false, true) => Some(t.italic),
        (false, false) => None,
    };
    let combine = |s: Style, over: Style| {
        if preserve_base_color {
            apply_modifiers(s, over)
        } else {
            overlay_style(s, over)
        }
    };
    let mut style = base;
    if let Some(es) = emph_style {
        style = combine(style, es);
    }
    if emphasis.strike {
        style = combine(style, t.strikethrough);
    }
    style
}

fn style_for_token(
    token: &StyleToken,
    emphasis: Emphasis,
    base: Style,
    preserve_base_color: bool,
    t: &Theme,
) -> Style {
    match token {
        StyleToken::Text => apply_emphasis(base, emphasis, preserve_base_color, t),
        StyleToken::InlineCode => {
            let style = apply_emphasis(base, emphasis, preserve_base_color, t);
            overlay_style(style, t.inline_code)
        }
        StyleToken::Heading => apply_emphasis(t.heading, emphasis, true, t),
        StyleToken::Highlight {
            fg,
            bold,
            italic,
            underline,
        } => {
            let mut s = Style::default().fg(ratatui::style::Color::Rgb(fg.0, fg.1, fg.2));
            if *bold {
                s = s.add_modifier(Modifier::BOLD);
            }
            if *italic {
                s = s.add_modifier(Modifier::ITALIC);
            }
            if *underline {
                s = s.add_modifier(Modifier::UNDERLINED);
            }
            s
        }
        StyleToken::Math => apply_emphasis(t.math, emphasis, true, t),
        StyleToken::Diagram => t.diagram,
        StyleToken::CodeBar => t.code_gutter,
        StyleToken::ListMarker => t.list_marker,
        StyleToken::TableBorder => t.table_border,
        StyleToken::HorizontalRule => t.horizontal_rule,
    }
}

/// Heading lines preserve heading colour through emphasis. Code lines start
/// from `Style::default()` so highlighter colours stand alone.
fn paint_line(
    line: &RLine,
    text_style: Style,
    t: &Theme,
) -> (Line<'static>, Vec<Option<Arc<str>>>) {
    let (base, preserve_color) = match line.kind {
        LineKind::Heading => (t.heading, true),
        LineKind::Code => (Style::default(), false),
        _ => (text_style, false),
    };
    let mut spans = Vec::with_capacity(line.spans.len());
    let mut links = Vec::with_capacity(line.spans.len());
    for RSpan {
        text,
        style,
        emphasis,
        source: _,
        link,
    } in &line.spans
    {
        let link = link.as_deref().and_then(interactive_link_target);
        let mut style = style_for_token(style, *emphasis, base, preserve_color, t);
        if link.is_some() {
            style = style.add_modifier(Modifier::UNDERLINED);
        }
        spans.push(Span::styled(text.clone(), style));
        links.push(link);
    }
    (Line::from(spans), links)
}

fn interactive_link_target(target: &str) -> Option<Arc<str>> {
    if target.chars().any(char::is_control) {
        return None;
    }
    let url = Url::parse(target).ok()?;
    (matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
        .then(|| Arc::from(target))
}

fn line_provenance(line: &RLine) -> LineProvenance {
    LineProvenance {
        line: line.source.clone(),
        spans: line.spans.iter().map(|s| s.source.clone()).collect(),
    }
}

pub fn should_truncate(hidden: usize) -> bool {
    hidden >= MIN_TRUNCATABLE_LINES
}

pub fn truncation_notice(count: usize) -> String {
    debug_assert!(
        should_truncate(count),
        "truncation_notice called with count={count} below threshold"
    );
    format!("{TRUNCATION_PREFIX} ({count} lines) click to expand")
}

pub struct Truncated<'a> {
    pub kept: &'a str,
    pub skipped: usize,
}

pub(crate) fn hr_line(width: u16, style: Style) -> Line<'static> {
    Line::from(Span::styled(render::hr_text(width), style))
}

fn prefix_span(prefix: &str, style: Style) -> Span<'static> {
    Span::styled(prefix.to_owned(), style.add_modifier(Modifier::BOLD))
}

/// Returns `Line::default()` when the prefix is empty so callers can use it
/// as a blank first line without an empty styled span sneaking in.
fn prefix_line(prefix: &str, style: Style) -> Line<'static> {
    if prefix.is_empty() {
        Line::default()
    } else {
        Line::from(prefix_span(prefix, style))
    }
}

/// Inline block kinds (paragraph, heading, list) share line 1 with their
/// prefix. Standalone kinds (code, table, hr) need a separate leader line.
fn shares_line_with_prefix(kind: &LineKind) -> bool {
    matches!(
        kind,
        LineKind::Paragraph | LineKind::Heading | LineKind::ListItem | LineKind::Blank
    )
}

pub fn plain_lines(
    text: &str,
    prefix: &str,
    text_style: Style,
    prefix_style: Style,
) -> Vec<Line<'static>> {
    let text = text.trim_start_matches('\n');
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut first_line = true;

    for line in text.split('\n') {
        let mut spans: Vec<Span<'static>> = Vec::new();
        if first_line {
            if !prefix.is_empty() {
                spans.push(prefix_span(prefix, prefix_style));
            }
            first_line = false;
        }
        spans.push(Span::styled(line.to_owned(), text_style));
        lines.push(Line::from(spans));
    }

    if lines.is_empty() {
        lines.push(prefix_line(prefix, prefix_style));
    }

    lines
}

/// Where a drawn diagram sits in the painted lines, and how far it may pan.
/// Rows are indices into [`Painted::lines`], which a diagram never wraps
/// because the renderer slices it to the viewport first.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DiagramSpan {
    pub id: u16,
    pub rows: Range<usize>,
    pub full_width: u16,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct LinkMap {
    pub rows: Vec<Vec<Option<Arc<str>>>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TerminalLink {
    pub position: Position,
    pub symbol: String,
    pub width: u16,
    pub target: Arc<str>,
}

impl LinkMap {
    pub fn none_for(lines: &[Line<'_>]) -> Self {
        Self {
            rows: lines
                .iter()
                .map(|line| vec![None; line.spans.len()])
                .collect(),
        }
    }

    pub fn target_at(
        &self,
        lines: &[Line<'_>],
        width: u16,
        wrapped_row: u16,
        column: u16,
    ) -> Option<Arc<str>> {
        if width == 0 || column >= width || self.rows.len() != lines.len() {
            return None;
        }

        let mut row = 0u16;
        for (line, links) in lines.iter().zip(&self.rows) {
            if links.len() != line.spans.len() {
                return None;
            }
            let graphemes = linked_graphemes(line, links);
            for wrapped in link_wrapped_rows(&graphemes, width) {
                if row == wrapped_row {
                    return link_in_wrapped_row(&graphemes, &wrapped, column);
                }
                row = row.saturating_add(1);
            }
        }
        None
    }

    pub fn is_aligned(&self, lines: &[Line<'_>]) -> bool {
        self.rows.len() == lines.len()
            && self
                .rows
                .iter()
                .zip(lines)
                .all(|(links, line)| links.len() == line.spans.len())
    }

    pub fn append_terminal_links(
        &self,
        lines: &[Line<'_>],
        width: u16,
        scroll: u16,
        area: Rect,
        output: &mut Vec<TerminalLink>,
    ) {
        if width == 0 || area.is_empty() || !self.is_aligned(lines) {
            return;
        }

        let visible_end = scroll.saturating_add(area.height);
        let mut wrapped_row = 0u16;
        for (line, links) in lines.iter().zip(&self.rows) {
            let graphemes = linked_graphemes(line, links);
            for wrapped in link_wrapped_rows(&graphemes, width) {
                if wrapped_row >= visible_end {
                    return;
                }
                if wrapped_row >= scroll {
                    let mut column = 0u16;
                    for index in wrapped {
                        let grapheme = &graphemes[index];
                        if grapheme.width > 0
                            && column < area.width
                            && let Some(target) = grapheme.link
                        {
                            output.push(TerminalLink {
                                position: Position::new(
                                    area.x.saturating_add(column),
                                    area.y.saturating_add(wrapped_row - scroll),
                                ),
                                symbol: grapheme.symbol.to_owned(),
                                width: grapheme.width,
                                target: Arc::clone(target),
                            });
                        }
                        column = column.saturating_add(grapheme.width);
                    }
                }
                wrapped_row = wrapped_row.saturating_add(1);
            }
        }
    }
}

struct LinkedGrapheme<'a> {
    symbol: &'a str,
    width: u16,
    whitespace: bool,
    link: &'a Option<Arc<str>>,
}

fn linked_graphemes<'a>(
    line: &'a Line<'a>,
    links: &'a [Option<Arc<str>>],
) -> Vec<LinkedGrapheme<'a>> {
    line.spans
        .iter()
        .zip(links)
        .flat_map(|(span, link)| {
            span.styled_graphemes(Style::default())
                .map(move |grapheme| LinkedGrapheme {
                    symbol: grapheme.symbol,
                    width: grapheme.symbol.cell_width(),
                    whitespace: grapheme.is_whitespace(),
                    link,
                })
        })
        .collect()
}

fn link_wrapped_rows(graphemes: &[LinkedGrapheme<'_>], max_width: u16) -> Vec<Vec<usize>> {
    let mut rows = Vec::new();
    let mut pending_line = Vec::new();
    let mut pending_word = Vec::new();
    let mut pending_whitespace = VecDeque::<usize>::new();
    let mut line_width = 0;
    let mut word_width = 0;
    let mut whitespace_width = 0;
    let mut previous_was_text = false;

    for (index, grapheme) in graphemes.iter().enumerate() {
        if grapheme.width > max_width {
            continue;
        }
        let word_found = previous_was_text && grapheme.whitespace;
        let segment_overflow =
            pending_line.is_empty() && word_width + whitespace_width + grapheme.width > max_width;
        if word_found || segment_overflow {
            pending_line.extend(pending_whitespace.drain(..));
            line_width += whitespace_width;
            pending_line.append(&mut pending_word);
            line_width += word_width;
            whitespace_width = 0;
            word_width = 0;
        }

        let line_full = line_width >= max_width;
        let word_overflow =
            grapheme.width > 0 && line_width + whitespace_width + word_width >= max_width;
        if line_full || word_overflow {
            let mut remaining = max_width.saturating_sub(line_width);
            rows.push(mem::take(&mut pending_line));
            line_width = 0;
            while let Some(index) = pending_whitespace.front() {
                let width = graphemes[*index].width;
                if width > remaining {
                    break;
                }
                whitespace_width -= width;
                remaining -= width;
                pending_whitespace.pop_front();
            }
            if grapheme.whitespace && pending_whitespace.is_empty() {
                continue;
            }
        }

        if grapheme.whitespace {
            whitespace_width += grapheme.width;
            pending_whitespace.push_back(index);
        } else {
            word_width += grapheme.width;
            pending_word.push(index);
        }
        previous_was_text = !grapheme.whitespace;
    }

    pending_line.extend(pending_whitespace);
    pending_line.extend(pending_word);
    if !pending_line.is_empty() {
        rows.push(pending_line);
    }
    if rows.is_empty() {
        rows.push(Vec::new());
    }
    rows
}

fn link_in_wrapped_row(
    graphemes: &[LinkedGrapheme<'_>],
    wrapped: &[usize],
    target_column: u16,
) -> Option<Arc<str>> {
    let mut column = 0u16;
    for &index in wrapped {
        let grapheme = &graphemes[index];
        let width = grapheme.width;
        if width > 0 && column <= target_column && target_column < column + width {
            return grapheme.link.clone();
        }
        column += width;
    }
    None
}

/// Painted markdown together with the provenance that lets a selection copy
/// the source instead of the glyphs.
pub(crate) struct Painted {
    pub lines: Vec<Line<'static>>,
    pub provenance: Vec<LineProvenance>,
    pub diagrams: Vec<DiagramSpan>,
    pub links: LinkMap,
}

/// Consecutive rows carrying the same diagram id collapse into one span.
fn diagram_spans(semantic: &[RLine]) -> Vec<DiagramSpan> {
    let mut spans: Vec<DiagramSpan> = Vec::new();
    for (row, line) in semantic.iter().enumerate() {
        let LineKind::Diagram { id, full_width } = line.kind else {
            continue;
        };
        match spans.last_mut() {
            Some(last) if last.id == id => last.rows.end = row + 1,
            _ => spans.push(DiagramSpan {
                id,
                rows: row..row + 1,
                full_width,
            }),
        }
    }
    spans
}

/// Paint semantic lines into ratatui lines, splicing the prefix onto
/// the first line (or as a standalone leader for non-inline blocks).
pub(crate) fn paint_semantic(
    semantic: &[RLine],
    prefix: &str,
    text_style: Style,
    prefix_style: Style,
) -> Painted {
    let t = theme::current();
    let mut lines = Vec::with_capacity(semantic.len());
    let mut link_rows = Vec::with_capacity(semantic.len());
    for line in semantic {
        let (painted, links) = paint_line(line, text_style, &t);
        lines.push(painted);
        link_rows.push(links);
    }
    let mut links = LinkMap { rows: link_rows };
    let mut provenance: Vec<LineProvenance> = semantic.iter().map(line_provenance).collect();
    let mut diagrams = diagram_spans(semantic);

    if lines.is_empty() {
        lines.push(prefix_line(prefix, prefix_style));
        provenance.push(LineProvenance::chrome(lines[0].spans.len()));
        links.rows.push(vec![None; lines[0].spans.len()]);
        return Painted {
            lines,
            provenance,
            diagrams,
            links,
        };
    }

    // The prefix is UI chrome with no markdown behind it, so it is recorded
    // as such and drops out of anything copied.
    if shares_line_with_prefix(&semantic[0].kind) {
        if !prefix.is_empty() {
            lines[0].spans.insert(0, prefix_span(prefix, prefix_style));
            provenance[0].spans.insert(0, SpanSource::Chrome);
            links.rows[0].insert(0, None);
        }
    } else if !prefix.is_empty() {
        let leader = prefix_line(prefix, prefix_style);
        provenance.insert(0, LineProvenance::chrome(leader.spans.len()));
        links.rows.insert(0, vec![None; leader.spans.len()]);
        lines.insert(0, leader);
        for span in &mut diagrams {
            span.rows.start += 1;
            span.rows.end += 1;
        }
    }

    Painted {
        lines,
        provenance,
        diagrams,
        links,
    }
}

/// Renders markdown and keeps the text the provenance ranges index. That is
/// not always the caller's string: long lines are truncated before parsing.
pub(crate) fn text_to_painted(
    text: &str,
    prefix: &str,
    text_style: Style,
    prefix_style: Style,
    width: u16,
    max_line_bytes: Option<usize>,
    diagram_pans: Vec<u16>,
) -> (Painted, Arc<str>) {
    let parsed: Arc<str> = match max_line_bytes {
        Some(limit) => render::truncate_long_lines_at(text, limit).as_ref().into(),
        None => text.into(),
    };
    let semantic = render::Renderer::unwrapped()
        .with_diagram_pans(diagram_pans)
        .render(&parsed, width, 0);
    (
        paint_semantic(&semantic, prefix, text_style, prefix_style),
        parsed,
    )
}

/// Paints a fragment whose provenance has to point into a larger document.
/// Every recorded range is shifted by `base`, the offset of `text` inside that
/// document, so a copy reaches the original bytes.
pub(crate) fn text_to_painted_at(text: &str, style: Style, width: u16, base: u32) -> Painted {
    let semantic = render::Renderer::unwrapped().render(text, width, 0);
    let mut painted = paint_semantic(&semantic, "", style, style);
    for line in &mut painted.provenance {
        line.line = line
            .line
            .as_ref()
            .map(|range| range.start + base..range.end + base);
        for span in &mut line.spans {
            if let SpanSource::Range(source) = span {
                source.range = source.range.start + base..source.range.end + base;
            }
        }
    }
    painted
}

#[cfg(test)]
pub fn text_to_lines(
    text: &str,
    prefix: &str,
    text_style: Style,
    prefix_style: Style,
    width: u16,
    max_line_bytes: Option<usize>,
) -> Vec<Line<'static>> {
    text_to_painted(
        text,
        prefix,
        text_style,
        prefix_style,
        width,
        max_line_bytes,
        Vec::new(),
    )
    .0
    .lines
}

pub struct TruncatedOutput<'a> {
    pub kept: Cow<'a, str>,
    pub skipped: usize,
}

pub fn truncate_output(text: &str, max: usize) -> TruncatedOutput<'_> {
    let tr = truncate_lines(text, max);
    TruncatedOutput {
        kept: render::truncate_long_lines(tr.kept),
        skipped: tr.skipped,
    }
}

/// Keeps the head. Tools that want tail truncation do it in Lua instead
/// (ToolView `keep = "tail"`).
pub fn truncate_lines(s: &str, max: usize) -> Truncated<'_> {
    let Some((i, _)) = s.match_indices('\n').nth(max.saturating_sub(1)) else {
        return Truncated {
            kept: s,
            skipped: 0,
        };
    };
    let tail = &s[i..];
    let newlines = tail.matches('\n').count();
    let has_content = tail.bytes().any(|b| b != b'\n');
    let result = Truncated {
        kept: &s[..i],
        skipped: if has_content { newlines } else { 0 },
    };
    if result.skipped > 0 && !should_truncate(result.skipped) {
        return Truncated {
            kept: s,
            skipped: 0,
        };
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use caudra_markdown::render::CODE_BAR;
    use test_case::test_case;

    const TEST_WIDTH: u16 = 80;

    fn text_to_lines(
        text: &str,
        prefix: &str,
        text_style: Style,
        prefix_style: Style,
        width: u16,
    ) -> Vec<Line<'static>> {
        super::text_to_lines(text, prefix, text_style, prefix_style, width, None)
    }

    fn lines_text(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    fn find_span<'a>(lines: &'a [Line<'_>], needle: &str) -> &'a Span<'a> {
        lines
            .iter()
            .flat_map(|l| &l.spans)
            .find(|s| s.content == needle)
            .unwrap_or_else(|| panic!("span {needle:?} not found in {:?}", lines_text(lines)))
    }

    #[test]
    fn heading_uses_theme_heading_style() {
        let style = Style::default();
        let lines = text_to_lines("# hello", "", style, style, TEST_WIDTH);
        assert_eq!(lines.len(), 1);
        let heading_fg = theme::current().heading.fg;
        assert_eq!(lines[0].spans[0].style.fg, heading_fg);
    }

    #[test]
    fn code_block_emits_code_bar_with_theme_color() {
        let style = Style::default();
        let lines = text_to_lines("```\nhello\n```", "", style, style, TEST_WIDTH);
        let bar = lines
            .iter()
            .flat_map(|l| &l.spans)
            .find(|s| s.content.as_ref() == CODE_BAR)
            .expect("code bar span");
        assert_eq!(bar.style, theme::current().code_gutter);
    }

    #[test]
    fn bold_uses_theme_bold_fg_and_modifier() {
        let style = Style::default();
        let lines = text_to_lines("**bold**", "", style, style, TEST_WIDTH);
        let bold = find_span(&lines, "bold");
        assert_eq!(bold.style.fg, theme::current().bold.fg);
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn heading_emphasis_preserves_heading_color() {
        let style = Style::default();
        let lines = text_to_lines("## ***hi***", "", style, style, TEST_WIDTH);
        let hi = find_span(&lines, "hi");
        assert_eq!(hi.style.fg, theme::current().heading.fg);
        assert!(
            hi.style
                .add_modifier
                .contains(Modifier::BOLD | Modifier::ITALIC)
        );
    }

    #[test]
    fn heading_inline_code_recolors_to_code_fg() {
        let style = Style::default();
        let lines = text_to_lines("## foo `bar`", "", style, style, TEST_WIDTH);
        let bar = find_span(&lines, "bar");
        assert_eq!(bar.style.fg, theme::current().inline_code.fg);
    }

    #[test]
    fn list_marker_uses_list_marker_style() {
        let style = Style::default();
        let lines = text_to_lines("- item", "", style, style, TEST_WIDTH);
        let marker = lines[0]
            .spans
            .iter()
            .find(|s| s.style == theme::current().list_marker)
            .expect("list marker span");
        assert_eq!(marker.content, "• ");
    }

    #[test_case("hello", "p> hello"           ; "paragraph_inline")]
    #[test_case("# title", "p> title"         ; "heading_inline")]
    #[test_case("- item", "p> • item"         ; "list_inline")]
    fn prefix_inlined_on_first_line_blocks(input: &str, expected: &str) {
        let style = Style::default();
        let lines = text_to_lines(input, "p> ", style, style, TEST_WIDTH);
        assert_eq!(lines_text(&lines)[0], expected);
    }

    #[test]
    fn prefix_emits_standalone_line_for_code_block() {
        let style = Style::default();
        let lines = text_to_lines("```\ncode\n```", "p> ", style, style, TEST_WIDTH);
        assert_eq!(lines[0].spans[0].content, "p> ");
    }

    #[test]
    fn prefix_emits_standalone_line_for_table() {
        let style = Style::default();
        let input = "| a | b |\n| --- | --- |\n| 1 | 2 |";
        let lines = text_to_lines(input, "p> ", style, style, TEST_WIDTH);
        assert_eq!(lines[0].spans[0].content, "p> ");
    }

    #[test]
    fn empty_input_yields_single_prefix_line() {
        let style = Style::default();
        let lines = text_to_lines("", "p> ", style, style, TEST_WIDTH);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans[0].content, "p> ");
    }

    #[test]
    fn no_phantom_empty_bold_span_when_prefix_empty() {
        let style = Style::default();
        let lines = text_to_lines("hello", "", style, style, TEST_WIDTH);
        for span in &lines[0].spans {
            assert!(
                !(span.content.is_empty() && span.style.add_modifier.contains(Modifier::BOLD)),
                "phantom empty bold span: {:?}",
                lines[0].spans
            );
        }
    }

    #[test_case("a\nb\nc", 5, "a\nb\nc", 0     ; "under_limit")]
    #[test_case("a\nb\nc\nd", 2, "a\nb", 2     ; "over_limit_keeps_head")]
    fn truncate_lines_cases(input: &str, max: usize, expected_kept: &str, expected_skipped: usize) {
        let tr = truncate_lines(input, max);
        assert_eq!(tr.kept, expected_kept);
        assert_eq!(tr.skipped, expected_skipped);
    }

    #[test_case(
        "**bold** `code` ```fences```",
        &["p> **bold** `code` ```fences```"]
        ; "plain_ignores_all_markdown"
    )]
    #[test_case(
        "before\n```rust\nfn main() {}\n```\nafter",
        &["p> before", "```rust", "fn main() {}", "```", "after"]
        ; "plain_preserves_code_fences_literally"
    )]
    fn plain_content(input: &str, expected: &[&str]) {
        let base = Style::new().fg(ratatui::style::Color::Cyan);
        let lines = plain_lines(input, "p> ", base, base);
        assert_eq!(lines_text(&lines), expected);
    }

    #[test_case(5,  "click to expand"   ; "collapsed_shows_expand")]
    #[test_case(2,  "(2 lines)"         ; "collapsed_plural")]
    fn truncation_notice_text(count: usize, expected_substr: &str) {
        let notice = truncation_notice(count);
        assert!(
            notice.contains(expected_substr),
            "expected {expected_substr:?} in {notice:?}"
        );
    }

    #[test]
    fn strikethrough_uses_theme_strikethrough_style() {
        let style = Style::default();
        let lines = text_to_lines("~~struck~~", "", style, style, TEST_WIDTH);
        let struck = find_span(&lines, "struck");
        assert!(struck.style.add_modifier.contains(Modifier::CROSSED_OUT));
        assert_eq!(struck.style.fg, theme::current().strikethrough.fg);
    }

    #[test]
    fn italic_uses_italic_modifier() {
        let style = Style::default();
        let lines = text_to_lines("*italic*", "", style, style, TEST_WIDTH);
        let it = find_span(&lines, "italic");
        assert!(it.style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn table_border_uses_theme_table_border_style() {
        let style = Style::default();
        let input = "| a | b |\n| --- | --- |\n| 1 | 2 |";
        let lines = text_to_lines(input, "", style, style, TEST_WIDTH);
        let border_span = lines
            .iter()
            .flat_map(|l| &l.spans)
            .find(|s| {
                let c = s.content.as_ref();
                c.contains('╭') || c.contains('│')
            })
            .expect("box-drawing border span");
        assert_eq!(border_span.style, theme::current().table_border);
    }

    #[test]
    fn horizontal_rule_uses_theme_style_and_fill_char() {
        let style = Style::default();
        let lines = text_to_lines("---", "", style, style, TEST_WIDTH);
        assert_eq!(lines.len(), 1);
        let hr = &lines[0].spans[0];
        assert_eq!(hr.style, theme::current().horizontal_rule);
        assert!(
            hr.content.chars().all(|c| c == '─'),
            "HR should be filled with ─ chars, got {:?}",
            hr.content
        );
    }

    #[test]
    fn prefix_on_hr_gets_standalone_leader_line() {
        let style = Style::default();
        let lines = text_to_lines("---", "p> ", style, style, TEST_WIDTH);
        assert_eq!(lines[0].spans[0].content, "p> ");
        assert!(
            lines[1].spans[0].content.chars().all(|c| c == '─'),
            "second line should be the HR"
        );
    }

    #[test]
    fn code_block_highlight_spans_have_rgb_color() {
        let style = Style::default();
        let lines = text_to_lines("```rust\nfn x() {}\n```", "", style, style, TEST_WIDTH);
        let code_spans: Vec<_> = lines
            .iter()
            .flat_map(|l| &l.spans)
            .filter(|s| {
                s.content.as_ref() != CODE_BAR && !s.content.is_empty() && s.content.as_ref() != "│"
            })
            .collect();
        let has_rgb = code_spans
            .iter()
            .any(|s| matches!(s.style.fg, Some(ratatui::style::Color::Rgb(_, _, _))));
        assert!(
            has_rgb,
            "expected at least one Rgb-colored span in code block, got: {:?}",
            code_spans
                .iter()
                .map(|s| (&s.content, s.style.fg))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn inline_code_inside_bold_gets_overlay() {
        let style = Style::default();
        let lines = text_to_lines("**a `code` b**", "", style, style, TEST_WIDTH);
        let code = find_span(&lines, "code");
        assert_eq!(code.style.fg, theme::current().inline_code.fg);
        assert!(
            code.style.add_modifier.contains(Modifier::BOLD),
            "inline code inside bold should inherit BOLD modifier"
        );
    }

    #[test]
    fn links_are_underlined_without_losing_nested_style() {
        let style = Style::default();
        let lines = text_to_lines(
            "[**bold** and `code`](https://example.com)",
            "",
            style,
            style,
            TEST_WIDTH,
        );
        let bold = find_span(&lines, "bold");
        let code = find_span(&lines, "code");
        assert!(bold.style.add_modifier.contains(Modifier::UNDERLINED));
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        assert!(code.style.add_modifier.contains(Modifier::UNDERLINED));
        assert_eq!(code.style.fg, theme::current().inline_code.fg);
    }

    #[test]
    fn link_map_tracks_wrapped_and_wide_link_cells() {
        let target: Arc<str> = "https://example.com".into();
        let lines = vec![Line::from(vec![
            Span::raw("aa "),
            Span::raw("link"),
            Span::raw(" z"),
        ])];
        let links = LinkMap {
            rows: vec![vec![None, Some(Arc::clone(&target)), None]],
        };

        assert_eq!(links.target_at(&lines, 4, 1, 0), Some(Arc::clone(&target)));
        assert_eq!(links.target_at(&lines, 4, 1, 3), Some(Arc::clone(&target)));
        assert_eq!(links.target_at(&lines, 4, 0, 0), None);
        assert_eq!(links.target_at(&lines, 4, 2, 0), None);

        let mut terminal_links = Vec::new();
        links.append_terminal_links(&lines, 4, 1, Rect::new(10, 20, 4, 1), &mut terminal_links);
        assert_eq!(terminal_links.len(), 4);
        assert_eq!(terminal_links[0].position, Position::new(10, 20));
        assert_eq!(terminal_links[3].position, Position::new(13, 20));
        assert_eq!(terminal_links[0].symbol, "l");
        assert!(
            terminal_links
                .iter()
                .all(|link| link.target.as_ref() == "https://example.com")
        );

        let wide_lines = vec![Line::from(Span::raw("界"))];
        let wide_links = LinkMap {
            rows: vec![vec![Some(Arc::clone(&target))]],
        };
        assert_eq!(
            wide_links.target_at(&wide_lines, 4, 0, 1),
            Some(Arc::clone(&target))
        );

        let emoji_lines = vec![Line::from(vec![Span::raw("👩‍💻"), Span::raw("docs")])];
        let emoji_links = LinkMap {
            rows: vec![vec![None, Some(target)]],
        };
        assert_eq!(
            emoji_links.target_at(&emoji_lines, 8, 0, 2).as_deref(),
            Some("https://example.com")
        );

        let whitespace_lines = vec![Line::raw("aaa  "), Line::raw("x")];
        let whitespace_links = LinkMap {
            rows: vec![vec![None], vec![Some("https://example.com".into())]],
        };
        assert_eq!(
            whitespace_links
                .target_at(&whitespace_lines, 4, 1, 0)
                .as_deref(),
            Some("https://example.com")
        );
    }

    #[test]
    fn malformed_web_target_is_not_interactive() {
        let style = Style::default();
        let (painted, _) = text_to_painted(
            "[bad](https://)",
            "",
            style,
            style,
            TEST_WIDTH,
            None,
            Vec::new(),
        );
        let bad = find_span(&painted.lines, "bad");

        assert!(!bad.style.add_modifier.contains(Modifier::UNDERLINED));
        assert!(painted.links.rows.iter().flatten().all(Option::is_none));
    }

    #[test]
    fn interactive_link_targets_preserve_source_and_reject_controls() {
        assert_eq!(
            interactive_link_target("HTTPS://EXAMPLE.COM/path").as_deref(),
            Some("HTTPS://EXAMPLE.COM/path")
        );
        assert_eq!(
            interactive_link_target("https://example.com/\ntrimmed"),
            None
        );
    }
}
