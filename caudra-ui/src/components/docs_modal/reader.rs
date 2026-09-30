//! The page being read. It is painted once for each page, width and theme, then
//! restyled for the selected and hovered links and the search highlights, so a
//! `Tab`, an `F3` or the pointer copies rows and never renders the Markdown
//! again.

use std::borrow::Cow;
use std::mem;
use std::ops::Range;
use std::sync::Arc;

use caudra_docs::{Library, line_starts};
use caudra_markdown::render::SpanSource;
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::{PaneContext, Shown, clicked};
use crate::components::document_view::{DocumentMouse, DocumentView, Jump, Painted};
use crate::components::hover_style;
use crate::markdown::{LinkMap, text_to_rows};
use crate::provenance::{LineProvenance, Provenance};
use crate::selection::line_text;
use crate::theme::{self, Theme};

const HEADING_MARK: char = '#';

/// Where the reader lands once rows exist for the width it is drawn at: a
/// source line brought to the top, or the first highlighted row at or after
/// one.
#[derive(Clone, Copy)]
enum Landing {
    Line(usize),
    Match(usize),
}

impl Landing {
    fn line(self) -> usize {
        match self {
            Self::Line(line) | Self::Match(line) => line,
        }
    }
}

pub(super) enum ReaderMouse {
    Consumed,
    Copy(String),
    Follow(Arc<str>),
}

#[derive(Default)]
pub(super) struct Reader {
    page: Option<PageRows>,
    document: DocumentView<RowsKey>,
    link: Option<usize>,
    /// Lowercase words to highlight, from the search result that opened the
    /// page.
    terms: Vec<String>,
    /// Counts every change to `terms`, so the rows key can tell one set of
    /// highlights from another without holding a copy of it.
    highlights: u64,
    /// The highlighted row `F3` last moved to.
    found: Option<usize>,
    landing: Option<Landing>,
    pressed: Option<usize>,
}

/// What the restyled rows depend on: the page rows, and the links and the
/// highlights drawn over them.
#[derive(PartialEq)]
struct RowsKey {
    shown: Shown,
    width: u16,
    generation: u64,
    link: Option<usize>,
    hovered: Option<usize>,
    highlights: u64,
    found: Option<usize>,
}

/// One page painted to one width: its rows and the Markdown behind them, the
/// source line each row shows, the rows its headings open on, and its links.
/// The rows are kept as painted, one span to a source span, which is what the
/// provenance is zipped with.
struct PageRows {
    shown: Shown,
    width: u16,
    generation: u64,
    lines: Vec<Line<'static>>,
    provenance: Provenance,
    row_lines: Vec<usize>,
    anchors: Vec<usize>,
    links: Vec<Link>,
}

/// A link's target and the spans it was painted in, as `(row, span)` pairs in
/// reading order. A link that wraps keeps going on the next row.
struct Link {
    target: Arc<str>,
    spans: Vec<(usize, usize)>,
}

impl Link {
    fn row(&self) -> usize {
        self.spans.first().map_or(0, |&(row, _)| row)
    }

    /// Whether any of its rows is among `rows`, as a link that wraps onto the
    /// screen shows there.
    fn shown_in(&self, rows: &Range<usize>) -> bool {
        self.spans.iter().any(|(row, _)| rows.contains(row))
    }

    fn restyle_spans(&self, lines: &mut [Line<'static>], restyle: impl Fn(Style) -> Style) {
        for &(row, span) in &self.spans {
            if let Some(span) = lines.get_mut(row).and_then(|line| line.spans.get_mut(span)) {
                span.style = restyle(span.style);
            }
        }
    }
}

impl Reader {
    /// The source line at the top of the viewport, or the one a pending
    /// landing will bring there. A line survives a rewrap where a row does not.
    pub(super) fn top_line(&self) -> usize {
        if let Some(landing) = self.landing {
            return landing.line();
        }
        self.page
            .as_ref()
            .and_then(|page| page.row_lines.get(self.document.top()).copied())
            .unwrap_or(0)
    }

    /// Moves to `line` on the next frame, at the left margin. Leaving the page
    /// drops its highlights, which belonged to the search that opened it.
    pub(super) fn land(&mut self, line: usize, leaving: bool) {
        if leaving {
            self.set_terms(Vec::new());
        }
        self.found = None;
        self.link = None;
        self.pressed = None;
        self.landing = Some(Landing::Line(line));
        self.document.reset_pan();
    }

    /// Highlights `terms` and lands on the first of them at or after `line`.
    pub(super) fn highlight(&mut self, terms: Vec<String>, line: usize) {
        self.set_terms(terms);
        self.found = None;
        self.landing = Some(Landing::Match(line));
    }

    fn set_terms(&mut self, terms: Vec<String>) {
        self.terms = terms;
        self.highlights = self.highlights.wrapping_add(1);
    }

    pub(super) fn has_links(&self) -> bool {
        self.page
            .as_ref()
            .is_some_and(|page| !page.links.is_empty())
    }

    pub(super) fn has_link_selected(&self) -> bool {
        self.shown_link().is_some()
    }

    pub(super) fn has_highlights(&self) -> bool {
        !self.terms.is_empty()
    }

    pub(super) fn selected_target(&self) -> Option<Arc<str>> {
        self.target(self.shown_link()?)
    }

    /// The Markdown behind the sweep, as a sweep over the transcript copies it,
    /// or the text it covers where the rows cannot trace their source.
    pub(super) fn selected_text(&self) -> Option<String> {
        let (start, end) = self.document.sweep_ends()?;
        self.page
            .as_ref()
            .and_then(|page| page.provenance.extract_rows(&page.lines, start, end))
            .filter(|markdown| !markdown.is_empty())
            .or_else(|| self.document.selected_text())
    }

    pub(super) fn scroll(&mut self, delta: i32) {
        self.document.scroll(delta);
    }

    pub(super) fn pan(&mut self, delta: i32) {
        self.document.pan(delta);
    }

    pub(super) fn jump(&mut self, jump: Jump) {
        self.document.jump(jump);
    }

    pub(super) fn handle_scroll_key(&mut self, key: KeyEvent) {
        self.document.handle_scroll_key(key);
    }

    /// Selects the next or previous link. The one selected counts only while
    /// it is on screen; otherwise the step starts from the viewport, so `Tab`
    /// after a scroll picks a link the reader can see.
    pub(super) fn step_link(&mut self, forward: bool) -> bool {
        let Some(page) = &self.page else {
            return false;
        };
        let count = page.links.len();
        if count == 0 {
            return false;
        }
        let visible = self.document.visible();
        let next = match (self.shown_link(), forward) {
            (Some(index), true) => (index + 1) % count,
            (Some(index), false) => (index + count - 1) % count,
            (None, true) => page
                .links
                .iter()
                .position(|link| link.row() >= visible.start)
                .unwrap_or(0),
            (None, false) => page
                .links
                .iter()
                .rposition(|link| link.row() < visible.end)
                .unwrap_or(count - 1),
        };
        let row = page.links[next].row();
        self.link = Some(next);
        self.document.reveal(row);
        true
    }

    /// Moves to the next or previous highlighted row, wrapping at either end.
    /// Like a link, the row moved to last counts only while it is on screen.
    pub(super) fn step_match(&mut self, forward: bool) -> bool {
        let rows = self.match_rows();
        let (Some(&first), Some(&last)) = (rows.first(), rows.last()) else {
            return false;
        };
        let visible = self.document.visible();
        let from = self.found.filter(|row| visible.contains(row));
        let next = match (from, forward) {
            (Some(from), true) => rows.iter().find(|&&row| row > from),
            (None, true) => rows.iter().find(|&&row| row >= visible.start),
            (Some(from), false) => rows.iter().rev().find(|&&row| row < from),
            (None, false) => rows.iter().rev().find(|&&row| row < visible.end),
        };
        let next = next.copied().unwrap_or(if forward { first } else { last });
        self.found = Some(next);
        self.document.reveal(next);
        true
    }

    /// A press and release on the same link follows it, as a link in the
    /// transcript does. A drag sweeps and copies instead, wherever it started.
    pub(super) fn handle_mouse(&mut self, event: MouseEvent) -> ReaderMouse {
        let link = self.link_at(Position::new(event.column, event.row));
        let released = clicked(&mut self.pressed, event.kind, link);
        if let DocumentMouse::Copy(text) = self.document.handle_mouse(event) {
            return ReaderMouse::Copy(self.selected_text().unwrap_or(text));
        }
        let Some(index) = released else {
            return ReaderMouse::Consumed;
        };
        self.link = Some(index);
        self.selected_target()
            .map_or(ReaderMouse::Consumed, ReaderMouse::Follow)
    }

    /// Paints `shown` for `body` unless it already is, lands where the last
    /// navigation asked, and draws. `bars` is what the scrollbars run along.
    /// Returns where the link under the pointer leads, found once the rows
    /// have settled, so a scroll or a landing under a still pointer moves it.
    pub(super) fn draw(
        &mut self,
        frame: &mut Frame,
        bars: Rect,
        body: Rect,
        context: &PaneContext,
        shown: Shown,
    ) -> Option<Arc<str>> {
        let (library, theme) = (context.library, context.theme);
        let generation = theme::generation();
        let fresh = self.page.as_ref().is_some_and(|page| {
            page.shown == shown && page.width == body.width && page.generation == generation
        });
        if !fresh {
            if self.landing.is_none() && self.page.as_ref().is_some_and(|page| page.shown == shown)
            {
                self.landing = Some(Landing::Line(self.top_line()));
            }
            // A row number means nothing once the page is rewrapped.
            self.found = None;
            self.page = Some(paint(library, shown, body.width, generation, theme));
        }
        let landing = self.landing.take().map(|landing| self.resolve(landing));
        let rows = self.page.as_ref().map_or(0, |page| page.lines.len());
        self.document.fit(rows, body.height);
        if let Some((top, reveal)) = landing {
            self.document.scroll_to(top);
            if let Some(row) = reveal {
                self.document.reveal(row);
            }
        }
        let hovered = context.pointer.and_then(|at| self.link_at(at));
        if let Some(page) = &self.page {
            let (link, terms, found) = (self.link, &self.terms, self.found);
            let key = RowsKey {
                shown,
                width: body.width,
                generation,
                link,
                hovered,
                highlights: self.highlights,
                found,
            };
            let restyled = || restyle(page, link, hovered, terms, found, theme);
            // Only a repaint moves the text a standing sweep was drawn over.
            match fresh {
                true => self.document.restyle(key, restyled),
                false => self.document.ensure(key, 0, restyled),
            }
        }
        self.document.draw(frame, bars, body);
        self.target(hovered?)
    }

    /// The row a landing brings to the top, and for a highlight landing the
    /// row it then reveals.
    fn resolve(&mut self, landing: Landing) -> (usize, Option<usize>) {
        let top = self
            .page
            .as_ref()
            .map_or(0, |page| row_of_line(&page.row_lines, landing.line()));
        let Landing::Match(_) = landing else {
            return (top, None);
        };
        self.found = self.match_rows().into_iter().find(|&row| row >= top);
        (top, self.found)
    }

    fn match_rows(&self) -> Vec<usize> {
        let Some(page) = self.page.as_ref().filter(|_| !self.terms.is_empty()) else {
            return Vec::new();
        };
        page.lines
            .iter()
            .enumerate()
            .filter(|(_, line)| !term_ranges(&line_text(line), &self.terms).is_empty())
            .map(|(row, _)| row)
            .collect()
    }

    /// The selected link while any row of it is on screen. One scrolled away
    /// counts as none, so a key never acts on a link the reader cannot see.
    fn shown_link(&self) -> Option<usize> {
        let links = &self.page.as_ref()?.links;
        let visible = self.document.visible();
        self.link
            .filter(|&index| links.get(index).is_some_and(|link| link.shown_in(&visible)))
    }

    /// Whether `position` is on the page, as the last frame drew it.
    pub(super) fn contains(&self, position: Position) -> bool {
        self.document.cell_at(position).is_some()
    }

    /// The link drawn at `position`, which a press there follows and the
    /// pointer marks.
    fn link_at(&self, position: Position) -> Option<usize> {
        let (row, column) = self.document.cell_at(position)?;
        let page = self.page.as_ref()?;
        let span = span_at(page.lines.get(row)?, column)?;
        page.links
            .iter()
            .position(|link| link.spans.contains(&(row, span)))
    }

    fn target(&self, link: usize) -> Option<Arc<str>> {
        let link = self.page.as_ref()?.links.get(link)?;
        Some(Arc::clone(&link.target))
    }

    #[cfg(test)]
    pub(super) fn found(&self) -> Option<usize> {
        self.found
    }

    #[cfg(test)]
    pub(super) fn visible(&self) -> Range<usize> {
        self.document.visible()
    }

    #[cfg(test)]
    pub(super) fn row_text(&self, row: usize) -> Option<String> {
        Some(line_text(self.page.as_ref()?.lines.get(row)?))
    }

    /// The first row of the selected link, on screen or not.
    #[cfg(test)]
    pub(super) fn link_row(&self) -> Option<usize> {
        Some(self.page.as_ref()?.links.get(self.link?)?.row())
    }

    #[cfg(test)]
    pub(super) fn content(&self) -> Rect {
        self.document.content()
    }
}

fn paint(library: &Library, shown: Shown, width: u16, generation: u64, theme: &Theme) -> PageRows {
    let (text, headings): (Cow<'_, str>, Vec<usize>) = match shown {
        Shown::Contents => {
            let text = library.contents_markdown();
            let headings = text
                .lines()
                .enumerate()
                .filter(|(_, line)| line.starts_with(HEADING_MARK))
                .map(|(line, _)| line)
                .collect();
            (Cow::Owned(text), headings)
        }
        Shown::Page(index) => {
            let page = &library.pages()[index];
            (
                Cow::Borrowed(page.display.as_str()),
                page.headings.iter().map(|heading| heading.line).collect(),
            )
        }
    };
    let (painted, source) = text_to_rows(&text, theme.assistant, width, Vec::new());
    let row_lines = source_lines(&painted.provenance, &line_starts(&text));
    let mut anchors: Vec<usize> = headings
        .into_iter()
        .map(|line| row_of_line(&row_lines, line))
        .collect();
    anchors.sort_unstable();
    anchors.dedup();
    let links = links(&painted.lines, &painted.links);
    PageRows {
        shown,
        width,
        generation,
        lines: painted.lines,
        provenance: Provenance::new(source, painted.provenance),
        row_lines,
        anchors,
        links,
    }
}

/// The source line each row shows. A row with no source of its own, such as
/// the blank between two blocks, shows the line of the row above it.
fn source_lines(provenance: &[LineProvenance], starts: &[usize]) -> Vec<usize> {
    let mut line = 0;
    provenance
        .iter()
        .map(|row| {
            if let Some(byte) = source_byte(row) {
                line = starts
                    .partition_point(|&start| start <= byte)
                    .saturating_sub(1);
            }
            line
        })
        .collect()
}

fn source_byte(row: &LineProvenance) -> Option<usize> {
    let byte = row.line.as_ref().map(|range| range.start).or_else(|| {
        row.spans.iter().find_map(|span| match span {
            SpanSource::Range(source) => Some(source.range.start),
            SpanSource::Chrome | SpanSource::Unknown => None,
        })
    })?;
    usize::try_from(byte).ok()
}

/// The first row showing `line` or anything after it.
fn row_of_line(row_lines: &[usize], line: usize) -> usize {
    row_lines
        .iter()
        .position(|&row_line| row_line >= line)
        .unwrap_or_else(|| row_lines.len().saturating_sub(1))
}

/// Runs of spans that point at the same target. Blank spans between them, such
/// as the indent a wrap puts before the rest of a link, do not break a run.
fn links(lines: &[Line<'static>], map: &LinkMap) -> Vec<Link> {
    let mut links: Vec<Link> = Vec::new();
    let mut open = false;
    for (row, (line, targets)) in lines.iter().zip(&map.rows).enumerate() {
        for (index, (span, target)) in line.spans.iter().zip(targets).enumerate() {
            let Some(target) = target else {
                open &= span.content.trim().is_empty();
                continue;
            };
            match links.last_mut() {
                Some(link) if open && link.target == *target => link.spans.push((row, index)),
                _ => links.push(Link {
                    target: Arc::clone(target),
                    spans: vec![(row, index)],
                }),
            }
            open = true;
        }
    }
    links
}

fn restyle(
    page: &PageRows,
    link: Option<usize>,
    hovered: Option<usize>,
    terms: &[String],
    found: Option<usize>,
    theme: &Theme,
) -> Painted {
    let mut lines = page.lines.clone();
    if let Some(link) = link.and_then(|index| page.links.get(index)) {
        link.restyle_spans(&mut lines, |style| style.patch(theme.item_selected));
    }
    if let Some(link) = hovered.and_then(|index| page.links.get(index)) {
        link.restyle_spans(&mut lines, |style| hover_style(style, true));
    }
    if !terms.is_empty() {
        for (row, line) in lines.iter_mut().enumerate() {
            let ranges = term_ranges(&line_text(line), terms);
            if ranges.is_empty() {
                continue;
            }
            let style = match found == Some(row) {
                true => theme.item_match.add_modifier(Modifier::REVERSED),
                false => theme.item_match,
            };
            line.spans = highlight(mem::take(&mut line.spans), &ranges, style);
        }
    }
    Painted::new(lines, Vec::new(), page.anchors.clone())
}

/// Where `terms` occur in `text`, ignoring ASCII case, merged where they
/// overlap. Folding ASCII case keeps every byte where it was, so the ranges
/// hold for `text` itself.
fn term_ranges(text: &str, terms: &[String]) -> Vec<Range<usize>> {
    let folded = text.to_ascii_lowercase();
    let mut ranges: Vec<Range<usize>> = terms
        .iter()
        .filter(|term| !term.is_empty())
        .flat_map(|term| {
            folded
                .match_indices(term.as_str())
                .map(|(at, found)| at..at + found.len())
        })
        .collect();
    ranges.sort_unstable_by_key(|range| (range.start, range.end));
    let mut merged: Vec<Range<usize>> = Vec::new();
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    merged
}

/// Splits `spans` where `ranges` start and end and patches `style` onto what
/// they cover. `ranges` are ascending byte ranges into the spans' joined text.
fn highlight(
    spans: Vec<Span<'static>>,
    ranges: &[Range<usize>],
    style: Style,
) -> Vec<Span<'static>> {
    let mut out = Vec::with_capacity(spans.len() + ranges.len() * 2);
    let mut start = 0;
    for span in spans {
        let end = start + span.content.len();
        let mut cut = start;
        for range in ranges
            .iter()
            .filter(|range| range.start < end && range.end > start)
        {
            let from = range.start.max(start);
            let to = range.end.min(end);
            if from > cut {
                out.push(Span::styled(
                    span.content[cut - start..from - start].to_owned(),
                    span.style,
                ));
            }
            out.push(Span::styled(
                span.content[from - start..to - start].to_owned(),
                span.style.patch(style),
            ));
            cut = to;
        }
        if cut == start {
            out.push(span);
        } else if cut < end {
            out.push(Span::styled(
                span.content[cut - start..].to_owned(),
                span.style,
            ));
        }
        start = end;
    }
    out
}

/// The span drawn over display `column` of `line`.
fn span_at(line: &Line<'_>, column: usize) -> Option<usize> {
    let mut end = 0;
    line.spans.iter().position(|span| {
        end += span.width();
        column < end
    })
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const TERM: &str = "timeout";
    const MIXED_CASE: &str = "A Timeout, then a TIMEOUT.";
    const SPAN_HEAD: &str = "A Time";
    const SPAN_TAIL: &str = "out, then";
    /// Where "Timeout" sits in the two spans joined, straddling their seam.
    const ACROSS_SPANS: Range<usize> = 2..9;
    /// The source line each of five rows shows, with no row for lines 1 and 3.
    const ROW_LINES: [usize; 5] = [0, 0, 2, 2, 4];

    #[test]
    fn terms_match_whatever_their_case() {
        assert_eq!(
            term_ranges(MIXED_CASE, &[TERM.to_owned()]),
            vec![2..9, 18..25]
        );
    }

    #[test_case(&["time", "timeout"], vec![2..9, 18..25] ; "nested")]
    #[test_case(&["a t", "timeout"], vec![0..9, 16..25] ; "overlapping")]
    fn overlapping_terms_merge(terms: &[&str], expected: Vec<Range<usize>>) {
        let terms: Vec<String> = terms.iter().map(|term| term.to_string()).collect();
        assert_eq!(term_ranges(MIXED_CASE, &terms), expected);
    }

    #[test]
    fn highlight_splits_spans_and_keeps_their_text() {
        let marked = Style::new().add_modifier(Modifier::BOLD);
        let spans = vec![Span::raw(SPAN_HEAD), Span::raw(SPAN_TAIL)];
        let split = highlight(spans, &[ACROSS_SPANS], marked);
        let text: String = split.iter().map(|span| span.content.as_ref()).collect();
        let highlighted: String = split
            .iter()
            .filter(|span| span.style == marked)
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(text, format!("{SPAN_HEAD}{SPAN_TAIL}"));
        assert_eq!(highlighted, text[ACROSS_SPANS]);
    }

    #[test]
    fn a_wrapped_link_is_one_link() {
        let target: Arc<str> = Arc::from("https://caudra.ai/docs/guide/");
        let lines = vec![
            Line::from(vec![Span::raw("see "), Span::raw("the long")]),
            Line::from(vec![Span::raw("  "), Span::raw("guide"), Span::raw(" now")]),
        ];
        let map = LinkMap {
            rows: vec![
                vec![None, Some(Arc::clone(&target))],
                vec![None, Some(Arc::clone(&target)), None],
            ],
        };
        let found = links(&lines, &map);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].spans, vec![(0, 1), (1, 1)]);
    }

    #[test_case(4, 4 ; "a_line_a_row_shows")]
    #[test_case(3, 4 ; "a_line_no_row_shows")]
    #[test_case(9, 4 ; "a_line_past_the_last_row")]
    fn a_line_lands_on_the_first_row_at_or_after_it(line: usize, expected: usize) {
        assert_eq!(row_of_line(&ROW_LINES, line), expected);
    }
}
