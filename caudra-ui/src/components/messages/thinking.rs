//! Reasoning drawn with its body in a window of fixed height, the way a scroll
//! card draws its output: the window follows the newest rows, pauses when the
//! reader scrolls up, and a footer says what it hides.

use std::sync::Arc;

use ratatui::text::{Line, Span};

use super::{BuiltMessage, CHILD_SCROLL_INFIX};
use crate::components::code_view::{ScrollSpan, ScrollWindow};
use crate::components::tool_display::{ScrollTail, scroll_footer_text};
use crate::markdown::{DiagramSpan, LinkMap};
use crate::provenance::{LineProvenance, Provenance};
use crate::theme;

/// What every reasoning window is filed under, in the maps the cards use. It
/// opens with the child infix, which no tool id carries, so no card's key can
/// ever start with it.
const THINKING_KEY: &str = "#thinking";

/// Which reasoning block a window belongs to. The live block has no message
/// yet; a settled one is named by its message, the backlink its segment
/// already carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ThinkingWindow {
    Live,
    Settled(usize),
}

impl ThinkingWindow {
    pub fn key(self) -> String {
        match self {
            Self::Live => THINKING_KEY.to_owned(),
            Self::Settled(msg_index) => format!("{THINKING_KEY}{CHILD_SCROLL_INFIX}{msg_index}"),
        }
    }

    /// The block a window key names, or `None` for a card's key.
    pub fn parse(key: &str) -> Option<Self> {
        let rest = key.strip_prefix(THINKING_KEY)?;
        if rest.is_empty() {
            return Some(Self::Live);
        }
        rest.strip_prefix(CHILD_SCROLL_INFIX)?
            .parse()
            .ok()
            .map(Self::Settled)
    }
}

/// A reasoning body as painted, with each row's links, source and diagrams.
pub(super) struct Body<'a> {
    pub lines: &'a [Line<'static>],
    pub links: &'a LinkMap,
    pub provenance: Option<&'a Provenance>,
    pub diagrams: &'a [DiagramSpan],
}

/// Lays out a reasoning block: the header, a blank row, then the body cut to
/// `window` when there is one.
///
/// A body that overflows its window gains a footer saying what it hides, and
/// the span its bar is placed from. One that fits draws whole with neither,
/// because there is nothing to scroll. The search text is left empty for the
/// caller, which alone knows what a search should reach.
pub(super) fn assemble(
    header: Vec<Line<'static>>,
    body: Body<'_>,
    window: Option<ScrollWindow>,
    tail: ScrollTail,
) -> BuiltMessage {
    let total = body.lines.len();
    let (start, end) = window.map_or((0, total), |window| window.range(total));
    let first = header.len() + 1;
    let shown = &body.lines[start..end];

    let mut links = LinkMap::none_for(&header);
    links.rows.push(Vec::new());
    match body.links.rows.get(start..end) {
        Some(rows) => links.rows.extend_from_slice(rows),
        None => links.rows.extend(LinkMap::none_for(shown).rows),
    }
    let mut chrome: Vec<LineProvenance> = header
        .iter()
        .map(|line| LineProvenance::chrome(line.spans.len()))
        .collect();
    chrome.push(LineProvenance::chrome(0));
    let mut provenance = body.provenance.and_then(|source| {
        chrome.extend(source.kept_lines(start..end)?);
        Some(Provenance::new(Arc::clone(source.source()), chrome))
    });
    let diagrams = body
        .diagrams
        .iter()
        .filter_map(|diagram| {
            let rows = diagram.rows.start.max(start)..diagram.rows.end.min(end);
            (!rows.is_empty()).then(|| DiagramSpan {
                id: diagram.id,
                rows: rows.start - start + first..rows.end - start + first,
                full_width: diagram.full_width,
            })
        })
        .collect();

    let mut lines = header;
    lines.push(Line::default());
    lines.extend_from_slice(shown);
    let mut built = BuiltMessage {
        lines,
        search_text: String::new(),
        provenance: None,
        diagrams,
        links,
        scroll_span: None,
        scroll_footer_line: None,
    };
    if let Some(text) = scroll_footer_text(start, total - end, tail) {
        let footer = Line::from(Span::styled(text, theme::current().tool_dim));
        built.links.rows.push(vec![None; footer.spans.len()]);
        if let Some(provenance) = provenance.as_mut() {
            provenance.push_chrome_line(footer.spans.len());
        }
        built.scroll_footer_line = Some(built.lines.len());
        built.lines.push(footer);
        built.scroll_span = Some(ScrollSpan {
            child: None,
            first,
            lines: shown.len(),
            extent_lines: shown.len(),
            total,
            offset: start,
            history_start: None,
        });
    }
    built.provenance = provenance;
    built
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::text_to_rows;
    use ratatui::style::Style;
    use std::ops::Range;
    use test_case::test_case;

    const TOOL_KEY: &str = "call_1";
    const CHILD_KEY: &str = "call_1#2";
    const LONGER_PREFIX_KEY: &str = "#thinkingx";
    const NO_INDEX_KEY: &str = "#thinking#x";
    const HEADER: &str = "Thought";
    const BODY_ROWS: usize = 10;
    const WINDOW_ROWS: usize = 4;
    const PAUSED_OFFSET: usize = 2;
    const HEADER_ROWS: usize = 2;
    const SOURCE_TEXT: &str = "reasoning";
    const DIAGRAM_ID: u16 = 7;
    const DIAGRAM_ROWS: Range<usize> = 2..5;
    const DIAGRAM_WIDTH: u16 = 120;
    const COPY_FENCE: &str = "```text\nalpha\nbeta\ngamma\ndelta\n```";
    const COPY_WIDTH: u16 = 40;
    const COPY_WINDOW_ROWS: usize = 2;
    const COPY_SOURCE_MISSING: &str = "the reasoning window lost its source";
    const COPY_HIDDEN_ROWS: &str = "the copy included code outside the reasoning window";

    #[test_case(None, COPY_FENCE; "complete_block")]
    #[test_case(Some((0, false)), "alpha"; "prefix")]
    #[test_case(Some((2, false)), "beta\ngamma"; "middle")]
    #[test_case(Some((0, true)), "gamma\ndelta"; "suffix")]
    fn copying_a_reasoning_window_keeps_only_visible_code(
        window: Option<(usize, bool)>,
        expected: &str,
    ) {
        let (painted, source) = text_to_rows(COPY_FENCE, Style::default(), COPY_WIDTH, Vec::new());
        let provenance = Provenance::new(source, painted.provenance);
        let built = assemble(
            vec![Line::from(HEADER)],
            Body {
                lines: &painted.lines,
                links: &painted.links,
                provenance: Some(&provenance),
                diagrams: &painted.diagrams,
            },
            window.map(|(offset, follow)| ScrollWindow {
                height: COPY_WINDOW_ROWS,
                offset,
                follow,
            }),
            ScrollTail::Settled,
        );
        let provenance = built.provenance.expect(COPY_SOURCE_MISSING);
        let last = built.lines.len() - 1;
        let end = (last, built.lines[last].to_string().chars().count());

        assert_eq!(
            provenance
                .extract_rows(&built.lines, (0, 0), end)
                .as_deref(),
            Some(expected),
            "{COPY_HIDDEN_ROWS}"
        );
    }

    fn body_lines() -> Vec<Line<'static>> {
        (0..BODY_ROWS)
            .map(|row| Line::from(format!("row {row}")))
            .collect()
    }

    fn texts(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(Line::to_string).collect()
    }

    fn window(offset: usize, follow: bool) -> ScrollWindow {
        ScrollWindow {
            height: WINDOW_ROWS,
            offset,
            follow,
        }
    }

    fn built(window: Option<ScrollWindow>, tail: ScrollTail) -> BuiltMessage {
        let lines = body_lines();
        let links = LinkMap::none_for(&lines);
        let provenance = Provenance::new(
            SOURCE_TEXT.into(),
            lines
                .iter()
                .map(|line| LineProvenance::chrome(line.spans.len()))
                .collect(),
        );
        let diagrams = [DiagramSpan {
            id: DIAGRAM_ID,
            rows: DIAGRAM_ROWS,
            full_width: DIAGRAM_WIDTH,
        }];
        assemble(
            vec![Line::from(HEADER)],
            Body {
                lines: &lines,
                links: &links,
                provenance: Some(&provenance),
                diagrams: &diagrams,
            },
            window,
            tail,
        )
    }

    #[test_case(ThinkingWindow::Live ; "live")]
    #[test_case(ThinkingWindow::Settled(0) ; "first_message")]
    #[test_case(ThinkingWindow::Settled(42) ; "later_message")]
    fn a_key_names_the_block_it_was_made_for(block: ThinkingWindow) {
        assert_eq!(ThinkingWindow::parse(&block.key()), Some(block));
    }

    #[test_case(TOOL_KEY ; "card")]
    #[test_case(CHILD_KEY ; "batch_child")]
    #[test_case(LONGER_PREFIX_KEY ; "longer_prefix")]
    #[test_case(NO_INDEX_KEY ; "no_index")]
    fn a_card_key_names_no_block(key: &str) {
        assert_eq!(ThinkingWindow::parse(key), None);
    }

    #[test_case(Some(window(0, true)), ScrollTail::Resumable, BODY_ROWS - WINDOW_ROWS ; "following_shows_the_tail")]
    #[test_case(Some(window(PAUSED_OFFSET, false)), ScrollTail::Resumable, PAUSED_OFFSET ; "paused_stays_put")]
    #[test_case(Some(window(PAUSED_OFFSET, false)), ScrollTail::Settled, PAUSED_OFFSET ; "settled_stays_put")]
    fn an_overflowing_body_shows_its_window_and_a_footer(
        window: Option<ScrollWindow>,
        tail: ScrollTail,
        above: usize,
    ) {
        let built = built(window, tail);
        let below = BODY_ROWS - WINDOW_ROWS - above;
        let footer_line = HEADER_ROWS + WINDOW_ROWS;

        let rows = texts(&built.lines);
        assert_eq!(rows[..HEADER_ROWS], [HEADER.to_owned(), String::new()]);
        assert_eq!(
            rows[HEADER_ROWS..footer_line],
            texts(&body_lines()[above..above + WINDOW_ROWS])
        );
        assert_eq!(
            rows[footer_line],
            scroll_footer_text(above, below, tail).expect("an overflowing window names its edges")
        );
        assert_eq!(built.scroll_footer_line, Some(footer_line));
        let span = built.scroll_span.expect("an overflowing window has a bar");
        assert_eq!(
            (span.first, span.lines, span.total, span.offset),
            (HEADER_ROWS, WINDOW_ROWS, BODY_ROWS, above)
        );
        assert!(built.links.is_aligned(&built.lines));
    }

    #[test_case(None ; "no_window")]
    #[test_case(Some(ScrollWindow { height: BODY_ROWS, offset: 0, follow: true }) ; "window_that_fits")]
    fn a_body_that_fits_draws_whole_with_nothing_to_scroll(window: Option<ScrollWindow>) {
        let built = built(window, ScrollTail::Resumable);

        assert_eq!(texts(&built.lines[HEADER_ROWS..]), texts(&body_lines()));
        assert!(built.scroll_span.is_none());
        assert!(built.scroll_footer_line.is_none());
    }

    #[test_case(0, Some(HEADER_ROWS + 2..HEADER_ROWS + 4) ; "window_holds_the_diagram_head")]
    #[test_case(3, Some(HEADER_ROWS..HEADER_ROWS + 2) ; "window_holds_the_diagram_tail")]
    #[test_case(6, None ; "diagram_above_the_window")]
    fn a_diagram_keeps_only_the_rows_the_window_shows(offset: usize, rows: Option<Range<usize>>) {
        let built = built(Some(window(offset, false)), ScrollTail::Settled);

        assert_eq!(
            built.diagrams.first().map(|diagram| diagram.rows.clone()),
            rows
        );
    }

    #[test]
    fn every_row_keeps_a_source_row() {
        let built = built(Some(window(PAUSED_OFFSET, false)), ScrollTail::Settled);

        let provenance = built.provenance.expect("a painted body keeps its source");
        assert!(provenance.lines_in(0..built.lines.len()).is_some());
        assert!(provenance.lines_in(0..built.lines.len() + 1).is_none());
    }
}
