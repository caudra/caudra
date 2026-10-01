//! A Markdown tab read the way the transcript shows Markdown, rather than as
//! the source it is saved as.
//!
//! The workbench has no Markdown renderer of its own, so the host lends it a
//! [`PaintMarkdown`]. Painting a whole document costs far more than drawing a
//! frame, so the painted lines are kept until the text, the width or the theme
//! moves.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::text::Line;
use std::ops::Range;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{buffer::Cursor, words::is_word};

/// Paints Markdown wrapped to a width, the way the host's transcript does.
pub type PaintMarkdown = fn(&str, u16) -> PaintedMarkdown;

type ExtractMarkdown =
    dyn Fn(&[Line<'static>], (usize, usize), (usize, usize)) -> Option<String> + Send + Sync;

pub struct PaintedMarkdown {
    lines: Vec<Line<'static>>,
    extract: Box<ExtractMarkdown>,
}

impl PaintedMarkdown {
    pub fn new(
        lines: Vec<Line<'static>>,
        extract: impl Fn(&[Line<'static>], (usize, usize), (usize, usize)) -> Option<String>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            lines,
            extract: Box::new(extract),
        }
    }

    pub fn lines(&self) -> &[Line<'static>] {
        &self.lines
    }

    pub fn selected_text(&self, start: (usize, usize), end: (usize, usize)) -> Option<String> {
        if start == end {
            return None;
        }
        (self.extract)(&self.lines, start.min(end), start.max(end)).filter(|text| !text.is_empty())
    }

    fn all_text(&self) -> Option<String> {
        let row = self.lines.len().checked_sub(1)?;
        let column = self.lines[row]
            .spans
            .iter()
            .map(|span| span.content.chars().count())
            .sum();
        (self.extract)(&self.lines, (0, 0), (row, column)).filter(|text| !text.is_empty())
    }
}

enum Selection {
    Sweep(Cursor, Cursor),
    All,
}

/// What a set of painted lines was painted from. Any of them moving means the
/// lines no longer show what the tab holds.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Painting {
    pub(crate) revision: u64,
    pub(crate) width: u16,
    pub(crate) theme_generation: u64,
}

/// The rendered view of one tab. It scrolls in painted rows, which line up
/// with no source line, so it keeps its own place rather than the tab's.
#[derive(Default)]
pub struct Rendered {
    painted_from: Option<Painting>,
    painted: Option<PaintedMarkdown>,
    selection: Option<Selection>,
    has_source: bool,
    scroll: usize,
    /// The source line the view was entered on, and how many there were, kept
    /// until the first paint says how many rows that point is a share of.
    entered_at: Option<(usize, usize)>,
}

impl Rendered {
    pub(crate) fn entered_at(line: usize, lines: usize) -> Self {
        Self {
            entered_at: Some((line, lines)),
            ..Self::default()
        }
    }

    /// Paints `text` unless the lines held were painted from the same thing.
    pub(crate) fn paint(
        &mut self,
        painting: Painting,
        text: impl FnOnce() -> String,
        paint: PaintMarkdown,
    ) {
        if self.painted_from.as_ref() != Some(&painting) {
            if self.painted_from.as_ref().is_none_or(|previous| {
                previous.revision != painting.revision || previous.width != painting.width
            }) {
                self.clear_selection();
            }
            let text = text();
            self.has_source = !text.is_empty();
            self.painted = Some(paint(&text, painting.width));
            self.painted_from = Some(painting);
        }
        if let Some((line, lines)) = self.entered_at.take() {
            self.scroll = rescale(line, lines, self.lines().len());
        }
    }

    pub(crate) fn lines(&self) -> &[Line<'static>] {
        self.painted.as_ref().map_or(&[], PaintedMarkdown::lines)
    }

    pub(crate) fn position_at(&self, row: usize, column: usize) -> Option<Cursor> {
        if self.painted_from.as_ref()?.width == 0 {
            return None;
        }
        let row = row.min(self.lines().len().checked_sub(1)?);
        let character = grapheme_columns(&self.lines()[row])
            .find_map(|(characters, cells)| (cells.end > column).then_some(characters.start))
            .unwrap_or_else(|| self.line_len(row));
        Some(Cursor::new(row, character))
    }

    pub(crate) fn select_at(&mut self, cursor: Cursor, clicks: u8) {
        let Some(cursor) = self.clamp(cursor) else {
            self.clear_selection();
            return;
        };
        let (start, end) = match clicks {
            2 => {
                let chars: Vec<char> = self.lines()[cursor.line]
                    .spans
                    .iter()
                    .flat_map(|span| span.content.chars())
                    .collect();
                let mut at = cursor.col;
                while at > 0 && chars.get(at).is_some_and(|ch| ch.width() == Some(0)) {
                    at -= 1;
                }
                let Some(&under) = chars.get(at) else {
                    self.selection = Some(Selection::Sweep(cursor, cursor));
                    return;
                };
                let wanted = is_word(under);
                let boundary = |ch: &char| ch.width() != Some(0) && is_word(*ch) != wanted;
                let mut start = chars[..at]
                    .iter()
                    .rposition(boundary)
                    .map_or(0, |index| index + 1);
                while start < at && chars[start].width() == Some(0) {
                    start += 1;
                }
                let end = chars[at..]
                    .iter()
                    .position(boundary)
                    .map_or(chars.len(), |offset| at + offset);
                (
                    Cursor::new(cursor.line, start),
                    Cursor::new(cursor.line, end),
                )
            }
            3.. => (
                Cursor::new(cursor.line, 0),
                Cursor::new(cursor.line, self.line_len(cursor.line)),
            ),
            _ => (cursor, cursor),
        };
        self.selection = Some(Selection::Sweep(start, end));
    }

    pub(crate) fn extend_to(&mut self, cursor: Cursor) {
        if let Some(cursor) = self.clamp(cursor)
            && let Some(selection) = &mut self.selection
        {
            match selection {
                Selection::Sweep(_, end) => *end = cursor,
                Selection::All => *selection = Selection::Sweep(Cursor::default(), cursor),
            }
        }
    }

    pub(crate) fn select_all(&mut self) {
        self.selection = (self.has_source
            && self
                .painted_from
                .as_ref()
                .is_some_and(|painting| painting.width > 0))
        .then_some(Selection::All);
    }

    pub(crate) fn is_selecting(&self) -> bool {
        self.selection.is_some()
    }

    pub(crate) fn clear_selection(&mut self) -> bool {
        self.selection
            .take()
            .is_some_and(|selection| match selection {
                Selection::Sweep(start, end) => start != end,
                Selection::All => true,
            })
    }

    pub(crate) fn selected_text(&self, revision: u64) -> Option<String> {
        if self.painted_from.as_ref()?.revision != revision {
            return None;
        }
        let painted = self.painted.as_ref()?;
        match self.selection.as_ref()? {
            Selection::All => painted.all_text(),
            Selection::Sweep(_, _) => {
                let (start, end) = self.selection()?;
                painted.selected_text((start.line, start.col), (end.line, end.col))
            }
        }
    }

    pub(crate) fn selection_columns(&self, row: usize) -> Option<Range<usize>> {
        let (start, end) = self.selection()?;
        if row < start.line || row > end.line {
            return None;
        }
        let line = self.lines().get(row)?;
        let start = if row == start.line { start.col } else { 0 };
        let end = if row == end.line { end.col } else { usize::MAX };
        grapheme_columns(line)
            .take_while(|(characters, _)| characters.start < end)
            .filter(|(characters, cells)| characters.end > start && !cells.is_empty())
            .map(|(_, cells)| cells)
            .reduce(|selected, cells| selected.start..cells.end)
    }

    fn selection(&self) -> Option<(Cursor, Cursor)> {
        match self.selection.as_ref()? {
            Selection::Sweep(anchor, cursor) => {
                (anchor != cursor).then_some(((*anchor).min(*cursor), (*anchor).max(*cursor)))
            }
            Selection::All => self
                .clamp(Cursor::new(usize::MAX, usize::MAX))
                .map(|end| (Cursor::default(), end)),
        }
    }

    fn clamp(&self, cursor: Cursor) -> Option<Cursor> {
        if self.painted_from.as_ref()?.width == 0 {
            return None;
        }
        let line = cursor.line.min(self.lines().len().checked_sub(1)?);
        Some(Cursor::new(line, cursor.col.min(self.line_len(line))))
    }

    fn line_len(&self, row: usize) -> usize {
        self.lines()[row]
            .spans
            .iter()
            .map(|span| span.content.chars().count())
            .sum()
    }

    /// The first row a pane `rows` tall shows. Never so far down that the
    /// pane is left with rows it could have filled.
    pub(crate) fn top(&self, rows: usize) -> usize {
        self.scroll.min(self.lines().len().saturating_sub(rows))
    }

    pub(crate) fn scroll_to(&mut self, row: usize, rows: usize) {
        self.scroll = row;
        self.scroll = self.top(rows);
    }

    pub(crate) fn scroll_by(&mut self, delta: isize, rows: usize) {
        self.scroll_to(self.top(rows).saturating_add_signed(delta), rows);
    }

    /// Scrolls for a motion key and reports whether the key was one. There is
    /// no caret to carry and nothing off to the side, so every motion is a
    /// scroll and a sideways one goes nowhere.
    pub(crate) fn scroll_key(&mut self, key: KeyEvent, rows: usize) -> bool {
        let page = rows.max(1) as isize;
        match key.code {
            KeyCode::Up => self.scroll_by(-1, rows),
            KeyCode::Down => self.scroll_by(1, rows),
            KeyCode::PageUp => self.scroll_by(-page, rows),
            KeyCode::PageDown => self.scroll_by(page, rows),
            KeyCode::Home => self.scroll_to(0, rows),
            KeyCode::End => self.scroll_to(usize::MAX, rows),
            KeyCode::Left | KeyCode::Right => {}
            _ => return false,
        }
        true
    }

    /// The source line at the same point through the document as the view's
    /// place, for a reader going back to the source. A view left before it was
    /// ever painted hands back the line it was entered on.
    pub(crate) fn source_line(&self, lines: usize) -> usize {
        match self.entered_at {
            Some((line, _)) => line,
            None => rescale(self.scroll, self.lines().len(), lines),
        }
    }
}

fn grapheme_columns(line: &Line<'_>) -> impl Iterator<Item = (Range<usize>, Range<usize>)> {
    let mut offset = 0;
    line.spans
        .iter()
        .flat_map(move |span| {
            let mut character = offset;
            offset += span.content.chars().count();
            let mut remaining = span.content.as_ref();
            span.styled_graphemes(line.style)
                .filter_map(move |grapheme| {
                    let (skipped, rest) = remaining.split_once(grapheme.symbol)?;
                    character += skipped.chars().count();
                    let start = character;
                    character += grapheme.symbol.chars().count();
                    remaining = rest;
                    Some((start..character, grapheme.symbol.width()))
                })
        })
        .scan(0, |column, (characters, width)| {
            let start = *column;
            *column += width;
            Some((characters, start..*column))
        })
}

/// `at` of `from`, taken to the nearest point the same share of `to`.
fn rescale(at: usize, from: usize, to: usize) -> usize {
    match from {
        0 => 0,
        _ => (at * to + from / 2) / from,
    }
}

#[cfg(test)]
mod tests {
    use super::{Cursor, Line, PaintedMarkdown, Painting, Rendered, grapheme_columns};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        style::{Modifier, Style},
        text::Span,
        widgets::Widget,
    };
    use std::iter::repeat_n;
    use std::ops::Range;
    use test_case::test_case;

    const FIRST: &str = "first ";
    const SECOND: &str = "second ";
    const SOURCE: &str = "one\ntwo\nthree";
    const REVISION: u64 = 3;
    const WIDTH: u16 = 40;
    const THEME: u64 = 7;
    const ROWS: usize = 4;
    const SOURCE_LINES: usize = 50;
    const ENTERED_ON: usize = 20;
    const ROWS_PER_LINE: usize = 2;
    const STALE_PAINT: &str = "the view is showing lines painted from something that has moved";
    const WASTED_PAINT: &str = "the view repainted lines nothing had changed underneath";
    const OFF_THE_END: &str = "the view scrolled past the rows that fill the pane";
    const PLACE_LOST: &str = "the view did not keep the reader's place through the document";
    const UNICODE: &str = "a界e\u{301} z";
    const UNICODE_GRAPHEMES: usize = 5;
    const ZWJ_EMOJI: &str = "👩‍💻";
    const VARIATION_EMOJI: &str = "✈\u{fe0f}";
    const FLAG_EMOJI: &str = "🇬🇧";
    const CJK: &str = "界";
    const COMBINING: &str = "e\u{301}";
    const FOLLOWING: &str = "ab";
    const FOLLOWING_START: &str = "a";
    const CONTROL_PREFIX: &str = "a\t";
    const CONTROL_SUFFIX: &str = "b";
    const WORDS: &str = "cafe\u{301}! more";
    const ACCENTED_WORD: &str = "cafe\u{301}";
    const MULTILINE_SELECTION: &str = "ne\ntwo\nthr";
    const WIDE_SELECTION: &str = "界e\u{301}";
    const GAP: &str = "! ";
    const LAST_WORD: &str = "more";
    const EMPTY: &str = "";
    const RAW: &str = "# *raw source*\n";
    const WRONG_SELECTION: &str = "the rendered selection did not match the requested text";
    const WRONG_COLUMNS: &str =
        "the rendered selection used characters instead of terminal columns";
    const CACHED_ROWS: &str = "extraction did not receive the unchanged cached rows";
    const STALE_SELECTION: &str = "a changed layout retained its selection or drag anchor";

    fn first(text: &str, _width: u16) -> PaintedMarkdown {
        let lines = text
            .lines()
            .map(|line| Line::from(format!("{FIRST}{line}")))
            .collect();
        PaintedMarkdown::new(lines, |_, _, _| None)
    }

    fn second(text: &str, _width: u16) -> PaintedMarkdown {
        let lines = text
            .lines()
            .map(|line| Line::from(format!("{SECOND}{line}")))
            .collect();
        PaintedMarkdown::new(lines, |_, _, _| None)
    }

    /// Several rows for every source line, so a place in one is plainly not
    /// the same number in the other.
    fn stretched(text: &str, _width: u16) -> PaintedMarkdown {
        let lines = text
            .lines()
            .flat_map(|line| repeat_n(Line::from(line.to_owned()), ROWS_PER_LINE))
            .collect();
        PaintedMarkdown::new(lines, |_, _, _| None)
    }

    fn identity(text: &str, _width: u16) -> PaintedMarkdown {
        let lines = text
            .lines()
            .map(|line| {
                let span = Span::raw(line);
                Line::from(
                    span.styled_graphemes(Style::default())
                        .enumerate()
                        .map(|(index, grapheme)| {
                            Span::styled(
                                grapheme.symbol.to_owned(),
                                Style::default().add_modifier(if index % 2 == 0 {
                                    Modifier::BOLD
                                } else {
                                    Modifier::ITALIC
                                }),
                            )
                        })
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        PaintedMarkdown::new(lines, |lines, start, end| {
            Some(
                lines[start.0..=end.0]
                    .iter()
                    .enumerate()
                    .map(|(offset, line)| {
                        let row = start.0 + offset;
                        let first = if row == start.0 { start.1 } else { 0 };
                        let last = if row == end.0 { end.1 } else { usize::MAX };
                        line.spans
                            .iter()
                            .flat_map(|span| span.content.chars())
                            .take(last)
                            .skip(first)
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        })
    }

    fn selected_view(text: &str) -> Rendered {
        let mut view = Rendered::default();
        view.paint(
            painting(REVISION, WIDTH, THEME),
            || text.to_owned(),
            identity,
        );
        view
    }

    fn painting(revision: u64, width: u16, theme_generation: u64) -> Painting {
        Painting {
            revision,
            width,
            theme_generation,
        }
    }

    fn source(lines: usize) -> String {
        (0..lines)
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn opening(view: &Rendered) -> String {
        view.lines()[0].to_string()
    }

    #[test_case(0, 0 ; "first_character")]
    #[test_case(1, 1 ; "wide_character_start")]
    #[test_case(2, 1 ; "wide_character_second_cell")]
    #[test_case(3, 2 ; "accented_character")]
    #[test_case(4, 4 ; "after_combining_character")]
    #[test_case(usize::MAX, 6 ; "beyond_row_end")]
    fn hit_testing_uses_display_cells_and_clamps_rows(column: usize, character: usize) {
        let view = selected_view(UNICODE);

        assert_eq!(
            view.position_at(usize::MAX, column),
            Some(Cursor::new(0, character)),
            "{WRONG_COLUMNS}"
        );
    }

    #[test_case(ZWJ_EMOJI, 3, 2 ; "zwj_emoji")]
    #[test_case(VARIATION_EMOJI, 2, 2 ; "variation_selector")]
    #[test_case(FLAG_EMOJI, 2, 2 ; "regional_indicator_flag")]
    #[test_case(CJK, 1, 2 ; "wide_character")]
    #[test_case(COMBINING, 2, 1 ; "combining_character")]
    fn grapheme_cells_match_ratatui_and_keep_following_text_selectable(
        prefix: &str,
        characters: usize,
        width: u16,
    ) {
        let mut view = selected_view(&format!("{prefix}{FOLLOWING}"));
        let area = Rect::new(0, 0, WIDTH, 1);
        let mut buffer = Buffer::empty(area);
        (&view.lines()[0]).render(area, &mut buffer);
        assert_eq!(
            buffer[(width, 0)].symbol(),
            FOLLOWING_START,
            "{WRONG_COLUMNS}"
        );

        let start = view.position_at(0, usize::from(width)).unwrap();
        let end = view.position_at(0, usize::from(width) + 1).unwrap();
        assert_eq!(start, Cursor::new(0, characters), "{WRONG_COLUMNS}");
        assert_eq!(end, Cursor::new(0, characters + 1), "{WRONG_COLUMNS}");
        view.select_at(start, 1);
        view.extend_to(end);
        assert_eq!(
            view.selected_text(REVISION).as_deref(),
            Some(FOLLOWING_START),
            "{WRONG_SELECTION}"
        );
        assert_eq!(
            view.selection_columns(0),
            Some(usize::from(width)..usize::from(width) + 1),
            "{WRONG_COLUMNS}"
        );

        view.select_at(view.position_at(0, usize::from(width) - 1).unwrap(), 1);
        view.extend_to(start);
        assert_eq!(
            view.selected_text(REVISION).as_deref(),
            Some(prefix),
            "{WRONG_SELECTION}"
        );
        assert_eq!(
            view.selection_columns(0),
            Some(0..usize::from(width)),
            "{WRONG_COLUMNS}"
        );

        view.select_at(start, 1);
        view.extend_to(
            view.position_at(0, usize::from(width) + FOLLOWING.len())
                .unwrap(),
        );
        assert_eq!(
            view.selected_text(REVISION).as_deref(),
            Some(FOLLOWING),
            "{WRONG_SELECTION}"
        );
        assert_eq!(
            view.selection_columns(0),
            Some(usize::from(width)..usize::from(width) + FOLLOWING.len()),
            "{WRONG_COLUMNS}"
        );
    }

    #[test_case(false ; "single_span")]
    #[test_case(true ; "split_spans")]
    fn filtered_controls_still_count_towards_extraction_character_indices(split: bool) {
        let line = if split {
            Line::from(vec![Span::raw(CONTROL_PREFIX), Span::raw(CONTROL_SUFFIX)])
        } else {
            Line::from(format!("{CONTROL_PREFIX}{CONTROL_SUFFIX}"))
        };

        assert_eq!(
            grapheme_columns(&line).collect::<Vec<_>>(),
            vec![(0..1, 0..1), (2..3, 1..2)],
            "{WRONG_COLUMNS}"
        );
    }

    #[test_case(false ; "forward")]
    #[test_case(true ; "reverse")]
    fn multiline_sweeps_are_normalized_and_survive_scrolling(reverse: bool) {
        let mut view = selected_view(SOURCE);
        let rows = view.lines().to_vec();
        let start = Cursor::new(0, 1);
        let end = Cursor::new(2, 3);
        let (anchor, cursor) = if reverse { (end, start) } else { (start, end) };
        view.select_at(anchor, 1);
        view.extend_to(cursor);
        view.scroll_to(1, 1);

        assert_eq!(view.top(1), 1);
        assert_eq!(
            view.selected_text(REVISION).as_deref(),
            Some(MULTILINE_SELECTION),
            "{WRONG_SELECTION}"
        );
        assert_eq!(view.selection_columns(0), Some(1..3), "{WRONG_COLUMNS}");
        assert_eq!(view.selection_columns(1), Some(0..3), "{WRONG_COLUMNS}");
        assert_eq!(view.selection_columns(2), Some(0..3), "{WRONG_COLUMNS}");
        assert_eq!(view.selection_columns(3), None, "{WRONG_COLUMNS}");
        assert_eq!(view.lines(), rows, "{CACHED_ROWS}");
    }

    #[test_case(false ; "forward")]
    #[test_case(true ; "reverse")]
    fn unicode_sweeps_keep_accents_and_highlight_complete_wide_cells(reverse: bool) {
        let mut view = selected_view(UNICODE);
        let start = view.position_at(0, 2).unwrap();
        let end = view.position_at(0, 4).unwrap();
        let (anchor, cursor) = if reverse { (end, start) } else { (start, end) };
        view.select_at(anchor, 1);
        view.extend_to(cursor);

        assert_eq!(
            view.selected_text(REVISION).as_deref(),
            Some(WIDE_SELECTION),
            "{WRONG_SELECTION}"
        );
        assert_eq!(view.selection_columns(0), Some(1..4), "{WRONG_COLUMNS}");
    }

    #[test_case(2, 2, Some(ACCENTED_WORD), Some(0..4) ; "word")]
    #[test_case(4, 2, Some(ACCENTED_WORD), Some(0..4) ; "combining_character")]
    #[test_case(5, 2, Some(GAP), Some(4..6) ; "gap_does_not_take_previous_accent")]
    #[test_case(8, 2, Some(LAST_WORD), Some(6..10) ; "last_word")]
    #[test_case(2, 3, Some(WORDS), Some(0..10) ; "whole_rendered_row")]
    #[test_case(usize::MAX, 2, None, None ; "past_row_end")]
    fn multiple_clicks_select_words_or_rendered_rows(
        column: usize,
        clicks: u8,
        expected: Option<&str>,
        columns: Option<Range<usize>>,
    ) {
        let mut view = selected_view(&format!("{WORDS}\n{SOURCE}"));
        view.select_at(Cursor::new(0, column), clicks);

        assert_eq!(
            view.selected_text(REVISION).as_deref(),
            expected,
            "{WRONG_SELECTION}"
        );
        assert_eq!(view.selection_columns(0), columns, "{WRONG_COLUMNS}");
        assert_eq!(view.selection_columns(1), None, "{WRONG_COLUMNS}");
    }

    #[test_case(false ; "forward")]
    #[test_case(true ; "reverse")]
    fn the_painter_extracts_raw_source_from_unchanged_rows(reverse: bool) {
        let lines = identity(UNICODE, WIDTH).lines;
        let address = lines.as_ptr() as usize;
        let painted = PaintedMarkdown::new(lines, move |rows, start, end| {
            assert_eq!(rows.as_ptr() as usize, address, "{CACHED_ROWS}");
            assert_eq!(rows[0].spans.len(), UNICODE_GRAPHEMES, "{CACHED_ROWS}");
            assert_eq!(start, (0, 1), "{WRONG_SELECTION}");
            assert_eq!(end, (0, 4), "{WRONG_SELECTION}");
            Some(RAW.to_owned())
        });
        let (start, end) = if reverse {
            ((0, 4), (0, 1))
        } else {
            ((0, 1), (0, 4))
        };

        assert_eq!(
            painted.selected_text(start, end).as_deref(),
            Some(RAW),
            "{WRONG_SELECTION}"
        );
        assert_eq!(
            painted.selected_text(start, start),
            None,
            "{WRONG_SELECTION}"
        );
    }

    #[test_case(None ; "unmapped")]
    #[test_case(Some(EMPTY) ; "empty_extraction")]
    fn missing_source_never_falls_back_to_decorative_text(extracted: Option<&'static str>) {
        let mut view = selected_view(SOURCE);
        view.painted = Some(PaintedMarkdown::new(
            vec![Line::from(SOURCE)],
            move |_, _, _| extracted.map(str::to_owned),
        ));
        view.select_all();

        assert_eq!(view.selected_text(REVISION), None, "{WRONG_SELECTION}");
    }

    #[test_case(EMPTY, WIDTH ; "empty_source")]
    #[test_case(SOURCE, 0 ; "zero_width")]
    fn empty_views_have_no_selection_or_drag_anchor(text: &str, width: u16) {
        let mut view = Rendered::default();
        view.paint(
            painting(REVISION, width, THEME),
            || text.to_owned(),
            identity,
        );
        view.select_at(Cursor::default(), 1);
        view.extend_to(Cursor::new(usize::MAX, usize::MAX));
        view.select_all();
        view.scroll_to(usize::MAX, 0);

        assert_eq!(
            view.position_at(usize::MAX, usize::MAX),
            None,
            "{WRONG_COLUMNS}"
        );
        assert_eq!(view.selected_text(REVISION), None, "{WRONG_SELECTION}");
        assert_eq!(view.selection_columns(0), None, "{WRONG_COLUMNS}");
        assert!(!view.is_selecting(), "{STALE_SELECTION}");
        assert!(!view.clear_selection(), "{STALE_SELECTION}");
    }

    #[test_case(false ; "empty_anchor")]
    #[test_case(true ; "nonempty_sweep")]
    fn clearing_reports_only_nonempty_selection_and_cancels_extension(extend: bool) {
        let mut view = selected_view(SOURCE);
        view.select_at(Cursor::default(), 1);
        if extend {
            view.extend_to(Cursor::new(1, 1));
        }
        assert!(view.is_selecting(), "{WRONG_SELECTION}");

        assert_eq!(view.clear_selection(), extend, "{WRONG_SELECTION}");
        view.extend_to(Cursor::new(1, 1));

        assert!(!view.is_selecting(), "{STALE_SELECTION}");
        assert_eq!(view.selected_text(REVISION), None, "{STALE_SELECTION}");
        assert!(!view.clear_selection(), "{STALE_SELECTION}");
    }

    #[test_case(painting(REVISION, WIDTH, THEME), true ; "unchanged")]
    #[test_case(painting(REVISION, WIDTH, THEME + 1), true ; "theme_only")]
    #[test_case(painting(REVISION, WIDTH - 1, THEME), false ; "width_reflow")]
    #[test_case(painting(REVISION + 1, WIDTH, THEME), false ; "source_revision")]
    fn cache_changes_keep_only_compatible_selections_and_anchors(next: Painting, kept: bool) {
        let mut view = selected_view(SOURCE);
        view.select_all();
        let revision = next.revision;
        view.paint(next, || SOURCE.to_owned(), identity);

        assert_eq!(view.is_selecting(), kept, "{STALE_SELECTION}");
        assert_eq!(
            view.selected_text(revision).as_deref(),
            kept.then_some(SOURCE),
            "{STALE_SELECTION}"
        );
        view.extend_to(Cursor::new(1, 1));
        assert_eq!(view.is_selecting(), kept, "{STALE_SELECTION}");
        assert_eq!(
            view.selected_text(revision).is_some(),
            kept,
            "{STALE_SELECTION}"
        );
    }

    #[test_case(REVISION + 1 ; "newer_revision")]
    #[test_case(REVISION - 1 ; "older_revision")]
    fn stale_selections_cannot_be_copied_before_repainting(revision: u64) {
        let mut view = selected_view(SOURCE);
        view.select_all();

        assert_eq!(view.selected_text(revision), None, "{STALE_SELECTION}");
        assert_eq!(
            view.selected_text(REVISION).as_deref(),
            Some(SOURCE),
            "{WRONG_SELECTION}"
        );
    }

    #[test_case(SOURCE ; "source_only_requested_on_cache_miss")]
    fn a_cached_paint_does_not_rebuild_source(text: &str) {
        let mut view = selected_view(text);
        view.paint(
            painting(REVISION, WIDTH, THEME),
            || panic!("{WASTED_PAINT}"),
            identity,
        );

        view.select_all();
        assert_eq!(
            view.selected_text(REVISION).as_deref(),
            Some(text),
            "{WRONG_SELECTION}"
        );
    }

    #[test_case(painting(REVISION, WIDTH, THEME), false ; "nothing moved")]
    #[test_case(painting(REVISION + 1, WIDTH, THEME), true ; "the text changed")]
    #[test_case(painting(REVISION, WIDTH - 1, THEME), true ; "the pane narrowed")]
    #[test_case(painting(REVISION, WIDTH, THEME + 1), true ; "the theme changed")]
    fn a_paint_is_kept_until_what_it_was_painted_from_moves(next: Painting, repainted: bool) {
        let mut view = Rendered::default();
        view.paint(
            painting(REVISION, WIDTH, THEME),
            || SOURCE.to_owned(),
            first,
        );

        view.paint(next, || SOURCE.to_owned(), second);

        match repainted {
            true => assert!(opening(&view).starts_with(SECOND), "{STALE_PAINT}"),
            false => assert!(opening(&view).starts_with(FIRST), "{WASTED_PAINT}"),
        }
    }

    #[test_case(KeyCode::End, SOURCE_LINES - ROWS ; "end stops at the last full pane")]
    #[test_case(KeyCode::PageDown, ROWS ; "a page is a pane")]
    #[test_case(KeyCode::Down, 1 ; "an arrow is a row")]
    #[test_case(KeyCode::Right, 0 ; "sideways goes nowhere")]
    fn a_motion_scrolls_within_the_rows_there_are(code: KeyCode, expected: usize) {
        let mut view = Rendered::default();
        view.paint(
            painting(REVISION, WIDTH, THEME),
            || source(SOURCE_LINES),
            first,
        );

        assert!(view.scroll_key(KeyEvent::new(code, KeyModifiers::NONE), ROWS));

        assert_eq!(view.top(ROWS), expected, "{OFF_THE_END}");
    }

    #[test]
    fn scrolling_past_the_end_stays_on_the_last_full_pane() {
        let mut view = Rendered::default();
        view.paint(
            painting(REVISION, WIDTH, THEME),
            || source(SOURCE_LINES),
            first,
        );

        view.scroll_by(isize::MAX, ROWS);
        view.scroll_by(-1, ROWS);

        assert_eq!(view.top(ROWS), SOURCE_LINES - ROWS - 1, "{OFF_THE_END}");
    }

    #[test]
    fn going_back_to_the_source_lands_where_the_view_was_entered() {
        let mut view = Rendered::entered_at(ENTERED_ON, SOURCE_LINES);
        view.paint(
            painting(REVISION, WIDTH, THEME),
            || source(SOURCE_LINES),
            stretched,
        );

        assert_eq!(view.top(ROWS), ENTERED_ON * ROWS_PER_LINE, "{PLACE_LOST}");
        assert_eq!(view.source_line(SOURCE_LINES), ENTERED_ON, "{PLACE_LOST}");
    }

    #[test]
    fn a_view_never_painted_goes_back_to_the_line_it_was_entered_on() {
        let view = Rendered::entered_at(ENTERED_ON, SOURCE_LINES);

        assert_eq!(view.source_line(SOURCE_LINES), ENTERED_ON, "{PLACE_LOST}");
    }
}
