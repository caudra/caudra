//! A painted document shown in a modal: scrolled, panned, swept and copied the
//! same way wherever one appears.
//!
//! The owner paints the rows and names the title and footer. This draws them
//! in the popup every document modal shares, and keeps the painted rows
//! between frames, the sweep over them, and the scroll and bars that move
//! through them.

use std::ops::Range;

use crossterm::event::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthChar;

use crate::components::input::apply_selection;
use crate::components::modal::{FooterHits, FooterLine, Modal};
use crate::components::scrollbar::{Scrollbar, ScrollbarMouse};
use crate::components::{ModalScroll, bar_area};
use crate::selection::line_text;
use crate::theme;

/// Every modal scrolls by a `u16` offset, so a document keeps no more rows than
/// one can reach.
const MAX_ROWS: usize = u16::MAX as usize;
/// The notice heading a document cut to [`MAX_ROWS`] takes one row.
const NOTICE_ROWS: usize = 1;
const TRUNCATED: &str = "Earlier rows are left out of this view; y copies everything.";
const WIDTH_PERCENT: u16 = 78;
const MAX_HEIGHT_PERCENT: u16 = 85;
/// Columns kept clear either side of the body.
const H_PAD: u16 = 2;
/// The footer keeps its own row rather than trailing the content: a document
/// runs to hundreds of rows, and a footer only reachable below them is one the
/// reader never sees.
const FOOTER_ROWS: u16 = 1;
pub(crate) const COPY_LABEL: &str = "y";
pub(crate) const COPY_HINT: &str = " copy";
pub(crate) const COPIED_SELECTION: &str = "Copied the selection";
/// Under the notice a document modal shows before there is anything to show:
/// what it shows is whatever the agent bound.
pub(crate) const UNBOUND_HINT: &str = "It appears once the agent has bound a model.";

/// Rows painted for one layout, kept until what they were painted for changes.
/// Parsing and laying out a document is the one cost here worth avoiding on a
/// keypress that only scrolls.
///
/// The gutter is drawn beside the rows rather than inside them, so it stays put
/// while the body pans and a sweep can never pick it up. Anchors are the rows
/// sections open on, ascending.
#[derive(Default)]
pub(crate) struct Painted {
    lines: Vec<Line<'static>>,
    gutter: Vec<Line<'static>>,
    anchors: Vec<usize>,
    content_width: u16,
}

impl Painted {
    /// `gutter` holds one cell per row, or none for a document without one.
    pub(crate) fn new(
        lines: Vec<Line<'static>>,
        gutter: Vec<Line<'static>>,
        anchors: Vec<usize>,
    ) -> Self {
        let content_width = widest(&lines);
        Self {
            lines,
            gutter,
            anchors,
            content_width,
        }
    }

    fn row_text(&self, row: usize) -> Option<String> {
        self.lines.get(row).map(line_text)
    }

    fn last_position(&self) -> (usize, usize) {
        let row = self.lines.len().saturating_sub(1);
        let len = self.row_text(row).map_or(0, |text| text.chars().count());
        (row, len)
    }

    /// Keeps the newest rows of a document longer than `max_rows`, behind a
    /// notice that the rest is still one `y` away.
    fn cap(&mut self, max_rows: usize, notice: Style) {
        let rows = self.lines.len();
        if rows <= max_rows {
            return;
        }
        let dropped = rows - max_rows.saturating_sub(NOTICE_ROWS);
        self.lines
            .splice(..dropped, [Line::from(Span::styled(TRUNCATED, notice))]);
        self.gutter
            .splice(..dropped.min(self.gutter.len()), [Line::default()]);
        self.anchors = self
            .anchors
            .iter()
            .filter_map(|row| row.checked_sub(dropped))
            .map(|row| row + NOTICE_ROWS)
            .collect();
        self.content_width = widest(&self.lines);
    }

    #[cfg(test)]
    pub(crate) fn lines(&self) -> &[Line<'static>] {
        &self.lines
    }

    #[cfg(test)]
    pub(crate) fn gutter(&self) -> &[Line<'static>] {
        &self.gutter
    }

    #[cfg(test)]
    pub(crate) fn anchors(&self) -> &[usize] {
        &self.anchors
    }
}

/// A sweep over the body, in rows of the painted content and characters within
/// a row. Held as the two ends the pointer gave rather than as an ordered pair,
/// so a backwards drag keeps tracking the end that is moving.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Selection {
    anchor: (usize, usize),
    cursor: (usize, usize),
}

impl Selection {
    fn at(position: (usize, usize)) -> Self {
        Self {
            anchor: position,
            cursor: position,
        }
    }

    fn ordered(self) -> ((usize, usize), (usize, usize)) {
        if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }

    /// The characters of `row` this selection covers, or `None` where it covers
    /// none of them. `len` closes a row the selection runs past.
    fn on_row(self, row: usize, len: usize) -> Option<Range<usize>> {
        let (start, end) = self.ordered();
        if row < start.0 || row > end.0 {
            return None;
        }
        let from = if row == start.0 { start.1 } else { 0 };
        let to = if row == end.0 { end.1.min(len) } else { len };
        (from < to).then_some(from..to)
    }
}

pub(crate) enum Jump {
    Next,
    Previous,
}

/// What the pointer did to the document. `Passthrough` leaves the event to
/// the owner, such as its footer.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DocumentMouse {
    Consumed,
    /// A sweep was let go over this text.
    Copy(String),
    Passthrough,
}

/// One painted document and everything that moves through it. `K` is what the
/// paint depends on, such as the view, the wrap width and the theme generation:
/// the rows are repainted only when it changes.
pub(crate) struct DocumentView<K> {
    key: Option<K>,
    painted: Painted,
    gutter: u16,
    selection: Option<Selection>,
    /// Between a press on the body and its release, which is the sweep's to
    /// copy rather than the owner's to read as a click.
    dragging: bool,
    scroll: ModalScroll,
    scrollbar: Scrollbar,
    pan_bar: Scrollbar,
    /// Where the body was drawn, so a press can be read back as a position in
    /// it. Known only after a frame, which is also the only time a press can
    /// land on one.
    content: Rect,
}

impl<K> Default for DocumentView<K> {
    fn default() -> Self {
        Self {
            key: None,
            painted: Painted::default(),
            gutter: 0,
            selection: None,
            dragging: false,
            scroll: ModalScroll::new_top(),
            scrollbar: Scrollbar::default(),
            pan_bar: Scrollbar::horizontal(),
            content: Rect::default(),
        }
    }
}

impl<K: PartialEq> DocumentView<K> {
    /// Paints the document unless it was already painted for `key`. A repaint
    /// rewraps, so the rows a standing sweep named are no longer the rows it
    /// was drawn over and it goes. `gutter` is the columns the gutter cells
    /// take beside the body.
    pub(crate) fn ensure(&mut self, key: K, gutter: u16, paint: impl FnOnce() -> Painted) {
        self.gutter = gutter;
        if self.key.as_ref() == Some(&key) {
            return;
        }
        self.painted = paint();
        self.painted.cap(MAX_ROWS, theme::current().status_dim);
        self.key = Some(key);
        self.selection = None;
    }

    /// Swaps in rows restyled for `key`. Their text is where it was, so unlike
    /// [`ensure`] this leaves a standing sweep over it, and one being dragged.
    ///
    /// [`ensure`]: Self::ensure
    pub(crate) fn restyle(&mut self, key: K, paint: impl FnOnce() -> Painted) {
        let selection = self.selection.take();
        self.ensure(key, self.gutter, paint);
        self.selection = selection;
    }

    fn rows(&self) -> u16 {
        u16::try_from(self.painted.lines.len()).unwrap_or(u16::MAX)
    }

    /// Back to nothing painted, nothing swept, at the top: for a new document,
    /// or a new view whose rows share nothing with the ones before.
    pub(crate) fn reset(&mut self) {
        self.key = None;
        self.painted = Painted::default();
        self.selection = None;
        self.dragging = false;
        self.scroll.reset();
    }

    pub(crate) fn scroll(&mut self, delta: i32) {
        self.scroll.scroll(delta);
    }

    pub(crate) fn pan(&mut self, delta: i32) {
        self.scroll.pan_by(delta);
    }

    pub(crate) fn handle_scroll_key(&mut self, key_event: KeyEvent) -> bool {
        self.scroll.handle_key(key_event)
    }

    /// The first row on screen.
    pub(crate) fn top(&self) -> usize {
        usize::from(self.scroll.offset())
    }

    /// The rows on screen, as tall as the last frame drew the body.
    pub(crate) fn visible(&self) -> Range<usize> {
        let top = self.top();
        top..top + usize::from(self.content.height)
    }

    /// Measures `rows`, the rows about to be drawn, against a body `height`
    /// rows tall ahead of the frame that draws them, so a row asked for on
    /// rows painted this frame is reached rather than clamped to the rows drawn
    /// before them.
    pub(crate) fn fit(&mut self, rows: usize, height: u16) {
        self.scroll
            .update_dimensions(u16::try_from(rows).unwrap_or(u16::MAX), height);
    }

    /// Brings `row` to the top, as far as the scroll reaches.
    pub(crate) fn scroll_to(&mut self, row: usize) {
        self.scroll
            .scroll_to(u16::try_from(row).unwrap_or(u16::MAX));
    }

    /// Scrolls the least distance that brings `row` on screen. It stays there
    /// even after `End`, which had the view following its last row.
    pub(crate) fn reveal(&mut self, row: usize) {
        self.scroll
            .reveal_and_hold(u16::try_from(row).unwrap_or(u16::MAX), 1);
    }

    /// Back to the left margin, for a reader arriving somewhere new.
    pub(crate) fn reset_pan(&mut self) {
        self.scroll.pan_to(0);
    }

    /// Brings the next or previous section's first row to the top, as far as
    /// the scroll reaches. Nothing moves where no section lies that way.
    pub(crate) fn jump(&mut self, jump: Jump) {
        let anchors = &self.painted.anchors;
        let top = self.top();
        let target = match jump {
            Jump::Next => anchors.get(anchors.partition_point(|&row| row <= top)),
            Jump::Previous => anchors
                .partition_point(|&row| row < top)
                .checked_sub(1)
                .and_then(|index| anchors.get(index)),
        };
        if let Some(&row) = target {
            self.scroll_to(row);
        }
    }

    /// Claims a press on a bar, and a sweep over the body from its press to its
    /// release. Letting go hands over what the sweep covered, since the modal
    /// holds the pointer and the terminal can no longer copy it for the reader.
    /// Anything else goes back to the owner unclaimed and leaves a sweep
    /// standing, so a reader can mark a passage and then reach for a control
    /// without losing it.
    pub(crate) fn handle_mouse(&mut self, event: MouseEvent) -> DocumentMouse {
        // Wherever it lands, a press ends the sweep before it, whose release
        // may have gone to another overlay. A step arrow's release falls
        // through the bar and must not read as that sweep's.
        if event.kind == MouseEventKind::Down(MouseButton::Left) {
            self.dragging = false;
        }
        match self.scrollbar.handle(&event) {
            ScrollbarMouse::Ignored => {}
            ScrollbarMouse::Consumed => return DocumentMouse::Consumed,
            ScrollbarMouse::ScrollTo(top) => {
                self.scroll.scroll_to(top as u16);
                return DocumentMouse::Consumed;
            }
        }
        match self.pan_bar.handle(&event) {
            ScrollbarMouse::Ignored => {}
            ScrollbarMouse::Consumed => return DocumentMouse::Consumed,
            ScrollbarMouse::ScrollTo(column) => {
                self.scroll.pan_to(column as u16);
                return DocumentMouse::Consumed;
            }
        }
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(at) = self.position_at(&event) {
                    self.selection = Some(Selection::at(at));
                    self.dragging = true;
                    return DocumentMouse::Consumed;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging => {
                if let Some(at) = self.position_at(&event)
                    && let Some(selection) = &mut self.selection
                {
                    selection.cursor = at;
                }
                return DocumentMouse::Consumed;
            }
            MouseEventKind::Up(MouseButton::Left) if self.dragging => {
                self.dragging = false;
                return self
                    .selected_text()
                    .map_or(DocumentMouse::Consumed, DocumentMouse::Copy);
            }
            _ => {}
        }
        DocumentMouse::Passthrough
    }

    /// What the sweep covers, taken from the painted rows rather than from
    /// whatever they were painted from: a sweep copies the text the reader
    /// marked, and never the gutter beside it.
    pub(crate) fn selected_text(&self) -> Option<String> {
        let selection = self.selection?;
        let (start, end) = selection.ordered();
        let mut text = String::new();
        for row in start.0..=end.0.min(self.painted.lines.len().saturating_sub(1)) {
            let chars: Vec<char> = self.painted.row_text(row)?.chars().collect();
            let range = selection
                .on_row(row, chars.len())
                .unwrap_or(chars.len()..chars.len());
            if row > start.0 {
                text.push('\n');
            }
            text.extend(&chars[range]);
        }
        (!text.is_empty()).then_some(text)
    }

    /// Where the sweep starts and where it stops short of, in painted rows and
    /// characters within them, first end first.
    pub(crate) fn sweep_ends(&self) -> Option<((usize, usize), (usize, usize))> {
        self.selection.map(Selection::ordered)
    }

    pub(crate) fn select_all(&mut self) {
        self.selection = Some(Selection {
            anchor: (0, 0),
            cursor: self.painted.last_position(),
        });
    }

    /// Where a press landed in the body, in the same rows and characters the
    /// selection is held in. `None` for a press outside the body, and one below
    /// the last row lands on it.
    fn position_at(&self, event: &MouseEvent) -> Option<(usize, usize)> {
        let (row, column) = self.cell_at(Position::new(event.column, event.row))?;
        let row = row.min(self.painted.lines.len().saturating_sub(1));
        let text = self.painted.row_text(row)?;
        Some((row, char_at_column(&text, column)))
    }

    /// The row and the display column of the document drawn at `position`,
    /// as the last frame placed the body. `None` off the body. A row past the
    /// last one is left there, so an owner asking what was drawn at the pointer
    /// finds nothing below a short document.
    pub(crate) fn cell_at(&self, position: Position) -> Option<(usize, usize)> {
        let column = position.x.checked_sub(self.content.x)?;
        let row = position.y.checked_sub(self.content.y)?;
        if column >= self.content.width || row >= self.content.height {
            return None;
        }
        Some((
            self.top().saturating_add(usize::from(row)),
            usize::from(self.scroll.pan()).saturating_add(usize::from(column)),
        ))
    }

    /// Draws the document in the popup every document modal shares: `title` on
    /// the border, `footer` on a row of its own at the bottom, and the rows
    /// with their gutter and bars above it. Returns the popup.
    pub(crate) fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        title: &str,
        footer: &FooterLine,
        hits: &mut FooterHits,
    ) -> Rect {
        let modal = Modal {
            title,
            width_percent: WIDTH_PERCENT,
            max_height_percent: MAX_HEIGHT_PERCENT,
        };
        let (popup, inner) = modal.render(frame, area, self.rows().saturating_add(FOOTER_ROWS));
        let body = Rect {
            x: inner.x.saturating_add(H_PAD),
            y: inner.y,
            width: inner.width.saturating_sub(H_PAD * 2),
            height: inner.height.saturating_sub(FOOTER_ROWS),
        };
        let footer_area = Rect {
            y: inner.y.saturating_add(body.height),
            height: inner.height.min(FOOTER_ROWS),
            ..body
        };
        hits.set(footer.hits(footer_area, 0, FOOTER_ROWS));
        frame.render_widget(Paragraph::new(footer.line(hits.hovered())), footer_area);
        // After the footer, so the bars paint over it.
        self.draw(frame, inner, body);
        popup
    }

    /// Draws the rows on screen and their gutter cells into `body`, and the
    /// bars along the edges of `inner`. An owner that lays out its own popup,
    /// with the document as one pane of it, calls this instead of [`render`].
    ///
    /// [`render`]: Self::render
    pub(crate) fn draw(&mut self, frame: &mut Frame, inner: Rect, body: Rect) {
        let rows = self.rows();
        let painted = &self.painted;
        let gutter_area = Rect {
            width: self.gutter.min(body.width),
            ..body
        };
        let content = Rect {
            x: body.x.saturating_add(gutter_area.width),
            width: body.width.saturating_sub(gutter_area.width),
            ..body
        };

        self.scroll.update_dimensions(rows, content.height);
        self.scroll.fit_width(painted.content_width, content.width);
        let offset = self.scroll.offset();
        let pan = self.scroll.pan();

        // Only the rows on screen are cloned. The painted body is kept whole so
        // a scroll does not repaint it, and handing the paragraph the whole of
        // it would spend on every row what the cache saved.
        let top = usize::from(offset);
        let height = usize::from(content.height);
        let visible: Vec<Line<'static>> = painted
            .lines
            .iter()
            .enumerate()
            .skip(top)
            .take(height)
            .map(|(row, line)| match self.row_selection(row, line) {
                Some(range) => Line::from(apply_selection(line.spans.clone(), &range)),
                None => line.clone(),
            })
            .collect();
        let gutter: Vec<Line<'static>> = painted
            .gutter
            .iter()
            .skip(top)
            .take(height)
            .cloned()
            .collect();

        frame.render_widget(Paragraph::new(gutter), gutter_area);
        frame.render_widget(Paragraph::new(visible).scroll((0, pan)), content);
        self.scrollbar.draw(frame, inner, rows, offset);
        self.pan_bar
            .draw(frame, bar_area(inner), painted.content_width, pan);
        self.content = content;
    }

    /// The characters of a drawn row the sweep covers, measured against the
    /// row's own text so a selection running past its end stops there.
    fn row_selection(&self, row: usize, line: &Line<'static>) -> Option<Range<usize>> {
        self.selection?.on_row(row, line_text(line).chars().count())
    }

    #[cfg(test)]
    pub(crate) fn content(&self) -> Rect {
        self.content
    }
}

/// The columns a document modal's rows get beside a `gutter` on a terminal
/// `available` columns wide, which is what its owner wraps them to.
pub(crate) fn body_width(available: u16, gutter: u16) -> u16 {
    Modal::inner_width(available, WIDTH_PERCENT)
        .saturating_sub(H_PAD * 2)
        .saturating_sub(gutter)
}

/// How far the widest row runs, which is as far as the body can pan.
fn widest(lines: &[Line<'static>]) -> u16 {
    lines
        .iter()
        .map(Line::width)
        .max()
        .and_then(|width| u16::try_from(width).ok())
        .unwrap_or(u16::MAX)
}

/// The character a display column falls on, so a press lands where the reader
/// sees the pointer rather than that many chars along a row of wide glyphs.
fn char_at_column(text: &str, column: usize) -> usize {
    let mut width = 0;
    for (index, character) in text.chars().enumerate() {
        if width >= column {
            return index;
        }
        width += UnicodeWidthChar::width(character).unwrap_or(0);
    }
    text.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::buffer_text;
    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use test_case::test_case;

    const KEY: u8 = 0;
    const OTHER_KEY: u8 = 1;
    const GUTTER: u16 = 3;
    const WIDTH: u16 = 40;
    const HEIGHT: u16 = 8;
    const ROWS: [&str; 3] = ["alpha", "", "bravo charlie"];
    const NUMBERS: [&str; 3] = [" 1 ", " 2 ", " 3 "];
    /// Wider than the body beside a [`GUTTER`] in a [`WIDTH`] terminal.
    const WIDE: &str = "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    const PAN: u16 = 8;
    /// Enough rows to scroll every anchor in [`SECTIONS`] to the top.
    const LONG: usize = 30;
    const SECTIONS: [usize; 3] = [3, 10, 20];
    const CAP: usize = 5;
    const CAPPED_FROM: usize = 10;
    const PRESS: MouseEventKind = MouseEventKind::Down(MouseButton::Left);
    const DRAG: MouseEventKind = MouseEventKind::Drag(MouseButton::Left);
    const RELEASE: MouseEventKind = MouseEventKind::Up(MouseButton::Left);
    /// What a sweep over [`ROWS`] from `(0, 0)` to `(4, 2)` covers.
    const SWEPT: &str = "alpha\n\nbrav";
    /// Past the body's last column and row, counted from its first cell.
    const OFF_THE_BODY: (u16, u16) = (WIDTH, HEIGHT);
    const SELECTION_WRONG: &str = "a sweep must copy the rows it was drawn over";
    const SWEEP_DROPPED: &str = "letting go must leave the sweep standing";
    const CLICK_COPIED: &str = "a click that swept nothing must copy nothing";
    const GUTTER_TAKEN: &str = "a click on the gutter must be left to the owner";
    const GUTTER_MOVED: &str = "the gutter must stay put while the body pans";
    const REPAINT_WRONG: &str = "rows must be repainted only for a new key";
    const RESTYLE_DROPPED: &str = "restyled rows keep their text, so the sweep over it must stay";
    const JUMP_WRONG: &str = "a jump must bring the section's first row to the top";
    const CAP_WRONG: &str = "a capped document must keep its newest rows behind the notice";

    fn lines(texts: &[&'static str]) -> Vec<Line<'static>> {
        texts.iter().copied().map(Line::from).collect()
    }

    fn texts(lines: &[Line<'static>]) -> Vec<String> {
        lines.iter().map(ToString::to_string).collect()
    }

    fn sample() -> Painted {
        Painted::new(lines(&ROWS), lines(&NUMBERS), Vec::new())
    }

    fn opened(painted: Painted) -> DocumentView<u8> {
        let mut view = DocumentView::default();
        view.ensure(KEY, GUTTER, || painted);
        view
    }

    /// Leaves the terminal's last row below the body, where a modal's border
    /// carries the pan bar.
    fn draw(view: &mut DocumentView<u8>) -> String {
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
        terminal
            .draw(|frame| {
                let inner = Rect {
                    height: HEIGHT - 1,
                    ..frame.area()
                };
                view.draw(frame, inner, inner);
            })
            .unwrap();
        buffer_text(terminal.backend().buffer())
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// Sends `kind` at a position counted from the body's first cell.
    fn pointer(
        view: &mut DocumentView<u8>,
        kind: MouseEventKind,
        (column, row): (u16, u16),
    ) -> DocumentMouse {
        let content = view.content;
        view.handle_mouse(mouse(kind, content.x + column, content.y + row))
    }

    /// Presses at one body position and drags to another, still holding on.
    fn sweep(view: &mut DocumentView<u8>, from: (u16, u16), to: (u16, u16)) {
        pointer(view, PRESS, from);
        pointer(view, DRAG, to);
    }

    #[test_case((0, 0), (4, 2), (4, 2)       ; "forwards")]
    #[test_case((4, 2), (0, 0), (0, 0)       ; "backwards")]
    #[test_case((0, 0), (4, 2), OFF_THE_BODY ; "let_go_off_the_body")]
    fn a_sweep_copies_the_rows_it_covers(from: (u16, u16), to: (u16, u16), released: (u16, u16)) {
        let mut view = opened(sample());
        draw(&mut view);
        sweep(&mut view, from, to);

        assert_eq!(
            pointer(&mut view, RELEASE, released),
            DocumentMouse::Copy(SWEPT.to_owned()),
            "{SELECTION_WRONG}"
        );
        assert_eq!(
            view.selected_text().as_deref(),
            Some(SWEPT),
            "{SWEEP_DROPPED}"
        );
    }

    #[test]
    fn a_bare_click_copies_nothing() {
        let mut view = opened(sample());
        draw(&mut view);
        pointer(&mut view, PRESS, (1, 0));

        assert_eq!(
            pointer(&mut view, RELEASE, (1, 0)),
            DocumentMouse::Consumed,
            "{CLICK_COPIED}"
        );
    }

    #[test]
    fn select_all_copies_every_row_and_none_of_the_gutter() {
        let mut view = opened(sample());
        view.select_all();

        assert_eq!(
            view.selected_text(),
            Some(ROWS.join("\n")),
            "{SELECTION_WRONG}"
        );
    }

    /// The sweep's own release never arrived, as when another overlay took the
    /// pointer mid-drag. The click after it is still the owner's, and neither
    /// half of it touches the sweep.
    #[test]
    fn a_click_on_the_gutter_is_left_to_the_owner() {
        let mut view = opened(sample());
        draw(&mut view);
        sweep(&mut view, (0, 0), (4, 2));
        let content = view.content;

        for kind in [PRESS, RELEASE] {
            assert_eq!(
                view.handle_mouse(mouse(kind, content.x - 1, content.y)),
                DocumentMouse::Passthrough,
                "{GUTTER_TAKEN}"
            );
        }
        assert_eq!(
            view.selected_text().as_deref(),
            Some(SWEPT),
            "{SWEEP_DROPPED}"
        );
    }

    #[test]
    fn the_gutter_stays_put_while_the_body_pans() {
        let mut view = opened(Painted::new(
            lines(&[WIDE]),
            lines(&NUMBERS[..1]),
            Vec::new(),
        ));
        draw(&mut view);
        view.pan(i32::from(PAN));

        let first_row: String = draw(&mut view).chars().take(usize::from(WIDTH)).collect();
        let shown = usize::from(PAN)..usize::from(PAN + WIDTH - GUTTER);
        assert_eq!(
            first_row,
            format!("{}{}", NUMBERS[0], &WIDE[shown]),
            "{GUTTER_MOVED}"
        );
    }

    #[test_case(KEY,       1, true  ; "an_unchanged_key_keeps_the_rows_and_the_sweep")]
    #[test_case(OTHER_KEY, 2, false ; "a_new_key_repaints_and_drops_the_sweep")]
    fn rows_are_repainted_only_for_a_new_key(key: u8, expected_paints: usize, sweep_kept: bool) {
        let mut paints = 0;
        let mut view = DocumentView::default();
        view.ensure(KEY, GUTTER, || {
            paints += 1;
            sample()
        });
        view.select_all();
        view.ensure(key, GUTTER, || {
            paints += 1;
            sample()
        });

        assert_eq!(paints, expected_paints, "{REPAINT_WRONG}");
        assert_eq!(
            view.selected_text().is_some(),
            sweep_kept,
            "{REPAINT_WRONG}"
        );
    }

    #[test]
    fn a_restyle_keeps_the_sweep() {
        let mut view = opened(sample());
        view.select_all();

        view.restyle(OTHER_KEY, sample);

        assert_eq!(
            view.selected_text(),
            Some(ROWS.join("\n")),
            "{RESTYLE_DROPPED}"
        );
    }

    #[test_case(0,  Jump::Next,     3  ; "next_from_the_top")]
    #[test_case(3,  Jump::Next,     10 ; "next_from_a_section_start")]
    #[test_case(5,  Jump::Previous, 3  ; "previous_from_inside_a_section")]
    #[test_case(10, Jump::Previous, 3  ; "previous_from_a_section_start")]
    #[test_case(3,  Jump::Previous, 3  ; "previous_from_the_first_section_stays")]
    #[test_case(20, Jump::Next,     20 ; "next_from_the_last_section_stays")]
    fn a_jump_brings_a_section_to_the_top(from: u16, jump: Jump, expected: u16) {
        let rows = (0..LONG).map(|row| Line::from(row.to_string())).collect();
        let mut view = opened(Painted::new(rows, Vec::new(), SECTIONS.to_vec()));
        draw(&mut view);
        view.scroll.scroll_to(from);

        view.jump(jump);

        assert_eq!(view.scroll.offset(), expected, "{JUMP_WRONG}");
    }

    /// Row 0 is wider than any row kept, so the pan range has to shrink with it.
    #[test]
    fn past_the_cap_the_newest_rows_stay_behind_a_notice() {
        let rows = (0..CAPPED_FROM)
            .map(|row| match row {
                0 => Line::from(WIDE),
                _ => Line::from(format!("row {row}")),
            })
            .collect();
        let gutter = (0..CAPPED_FROM)
            .map(|row| Line::from(row.to_string()))
            .collect();
        let mut painted = Painted::new(rows, gutter, vec![0, 4, 7, 9]);

        painted.cap(CAP, Style::default());

        assert_eq!(
            texts(&painted.lines),
            [TRUNCATED, "row 6", "row 7", "row 8", "row 9"],
            "{CAP_WRONG}"
        );
        assert_eq!(
            texts(&painted.gutter),
            ["", "6", "7", "8", "9"],
            "{CAP_WRONG}"
        );
        assert_eq!(painted.anchors, [2, 4], "{CAP_WRONG}");
        assert_eq!(
            usize::from(painted.content_width),
            TRUNCATED.len(),
            "{CAP_WRONG}"
        );
    }

    #[test]
    fn a_document_at_the_cap_keeps_every_row() {
        let rows: Vec<Line<'static>> = (0..CAP).map(|row| Line::from(row.to_string())).collect();
        let mut painted = Painted::new(rows.clone(), Vec::new(), Vec::new());

        painted.cap(CAP, Style::default());

        assert_eq!(painted.lines, rows, "{CAP_WRONG}");
    }

    #[test]
    fn a_document_past_the_scroll_range_is_cut_to_it() {
        let mut view = DocumentView::default();
        view.ensure(KEY, GUTTER, || {
            Painted::new(
                vec![Line::from("row"); MAX_ROWS + 1],
                Vec::new(),
                Vec::new(),
            )
        });

        assert_eq!(view.painted.lines.len(), MAX_ROWS, "{CAP_WRONG}");
        assert_eq!(view.painted.lines[0].to_string(), TRUNCATED, "{CAP_WRONG}");
    }

    #[test_case("abc",  0, 0 ; "column_zero_is_the_first_char")]
    #[test_case("abc",  2, 2 ; "a_narrow_column_is_its_own_char")]
    #[test_case("abc", 99, 3 ; "past_the_end_is_the_end")]
    #[test_case("日本",  2, 1 ; "a_wide_glyph_holds_two_columns")]
    fn a_column_reads_back_as_its_character(text: &str, column: usize, expected: usize) {
        assert_eq!(char_at_column(text, column), expected);
    }
}
