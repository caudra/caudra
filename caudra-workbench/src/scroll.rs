//! The scrollbar every scrollable surface shares: its geometry, its paint, and
//! the drag that makes an oversized document reachable by pointer. One [`Axis`]
//! covers both directions, because a bar down the right edge and a bar along
//! the bottom differ only in which coordinate they read.
//!
//! The arithmetic is authoritative here rather than inside a widget because the
//! row a press lands on and the row the thumb was painted on have to be the
//! same row. Ratatui's `Scrollbar` keeps that arithmetic private, so
//! hit-testing it would mean mirroring code that can move under a minor bump.
//!
//! `caudra-ui` draws the same bar over a `Frame`; [`Scrollbar::render`] takes a
//! `Buffer`, which `Frame::buffer_mut` hands over, so both crates share one
//! implementation instead of the copy this module replaced.

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use std::sync::atomic::{AtomicBool, Ordering};
use unicode_width::UnicodeWidthStr;

use crate::chrome;

pub const SCROLLBAR_THUMB: &str = "▐";
/// The same thumb turned on its side, for a bar that runs along a row.
pub const SCROLLBAR_THUMB_HORIZONTAL: &str = "▄";
/// The thumb while it is held. A terminal cannot change the cursor over a
/// widget, so the thumb has to say for itself that it has been grabbed.
pub const SCROLLBAR_THUMB_GRABBED: &str = "█";
/// The step arrows at the ends of a horizontal bar. Solid, to match the thumbs
/// they flank.
pub const SCROLLBAR_STEP_BACK: &str = "◀";
pub const SCROLLBAR_STEP_FORWARD: &str = "▶";
/// How wide each step zone is. A tap is the only gesture some terminals can
/// report at all, so the zone is several cells rather than the one it paints.
const STEP_CELLS: u16 = 3;
/// How thick a step zone is without touch. Unlike the seek track, which only
/// widens for a finger, an arrow is always worth a second row: a terminal
/// driven by touch over SSH cannot be detected, and two corners of an otherwise
/// inert row is a far smaller claim than a margin the width of the bar.
const STEP_HIT_ROWS: u16 = 2;
/// What is left for the track once both arrows are taken. Arrows never take
/// more than half a row: a seek track shorter than the zones flanking it is a
/// bar that has become mostly buttons, and a short bar keeps them off entirely.
const MIN_TRACK_CELLS: u16 = 2 * STEP_CELLS;
/// A tap on an arrow covers this fraction of a viewport, which is the page step
/// every other scrollbar takes.
const STEP_DIVISOR: u32 = 2;
/// A fine drag covers this fraction of the distance a plain one would. A short
/// track over a long document is hundreds of lines per row, and landing on a
/// particular one is otherwise a matter of luck.
const FINE_DIVISOR: i64 = 8;
const LINE_HINT: &str = "line";
const MESSAGE_HINT: &str = "message";
/// Columns between the position hint and the bar, so the chip does not touch
/// the thumb it describes.
const HINT_GAP: u16 = 1;
/// How thick a press on the bar may land under touch, across the bar rather than
/// along it. A fingertip covers several cells and reports their centroid, so a
/// one-cell target is unhittable while three is comfortable. Paint stays one
/// cell thick either way.
const TOUCH_HIT_CELLS: u16 = 3;

/// Whether the pointer is a finger, a property of the attached terminal and so
/// the same for every surface. Read by [`ScrollTrack::new`], which sits well
/// below anything holding the config, and by the surfaces that decide whether a
/// press can become a drag at all.
static TOUCH: AtomicBool = AtomicBool::new(false);

/// Set at startup from the resolved `ui.touch`. A later call wins, so a read
/// that happens first cannot latch the answer the way a `OnceLock` would.
pub fn set_touch(on: bool) {
    TOUCH.store(on, Ordering::Relaxed);
}

pub fn touch() -> bool {
    TOUCH.load(Ordering::Relaxed)
}

/// Which way a bar runs. Every measurement below is either along the bar, where
/// the thumb travels and a press is read, or across it, which is the one cell the
/// bar paints and the margin a fingertip is allowed either side of it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Axis {
    #[default]
    Vertical,
    Horizontal,
}

impl Axis {
    /// The extent the thumb travels in.
    const fn along(self, area: Rect) -> u16 {
        match self {
            Self::Vertical => area.height,
            Self::Horizontal => area.width,
        }
    }

    /// The extent the bar is one cell of.
    const fn across(self, area: Rect) -> u16 {
        match self {
            Self::Vertical => area.width,
            Self::Horizontal => area.height,
        }
    }

    const fn start(self, area: Rect) -> u16 {
        match self {
            Self::Vertical => area.y,
            Self::Horizontal => area.x,
        }
    }

    const fn end(self, area: Rect) -> u16 {
        match self {
            Self::Vertical => area.bottom(),
            Self::Horizontal => area.right(),
        }
    }

    /// Where a pointer sits along the bar. The other coordinate only decides
    /// whether the press is on the bar at all, which [`ScrollTrack::contains`]
    /// answers.
    const fn pointer(self, event: &MouseEvent) -> u16 {
        match self {
            Self::Vertical => event.row,
            Self::Horizontal => event.column,
        }
    }

    /// The cell at `along` on the bar, which is the only cell of the strip that
    /// coordinate names.
    const fn cell(self, strip: Rect, along: u16) -> (u16, u16) {
        match self {
            Self::Vertical => (strip.x, along),
            Self::Horizontal => (along, strip.y),
        }
    }

    const fn thumb(self) -> &'static str {
        match self {
            Self::Vertical => SCROLLBAR_THUMB,
            Self::Horizontal => SCROLLBAR_THUMB_HORIZONTAL,
        }
    }
}

/// The one-cell strip at the far edge of `area`: its last column for a vertical
/// bar, its last row for a horizontal one. Callers hand over either a strip they
/// already reserved or the whole surface, and both land on the same cells.
fn strip(area: Rect, axis: Axis, thickness: u16) -> Rect {
    match axis {
        Axis::Vertical => Rect {
            x: area.right() - thickness,
            width: thickness,
            ..area
        },
        Axis::Horizontal => Rect {
            y: area.bottom() - thickness,
            height: thickness,
            ..area
        },
    }
}

/// The strip a press may land on, given the surface [`ScrollTrack::new`] was
/// handed. Thicker than the paint under touch, clamped to the surface so a
/// narrow pane cannot claim presses from its neighbour, and exactly the painted
/// cells otherwise.
fn hit_area(area: Rect, touch: bool, axis: Axis) -> Rect {
    let thickness = match touch {
        true => TOUCH_HIT_CELLS.min(axis.across(area)),
        false => 1,
    };
    strip(area, axis, thickness)
}

/// Splits a horizontal bar's row into a step zone at each end and the seek
/// track between them. A vertical bar gets none: the wheel every terminal
/// reports already steps it, and the arrows are there for the one direction
/// some terminals cannot report at all. A row too short to spare six cells
/// keeps the whole row and stays a plain track.
fn split_steps(area: Rect, touch: bool, axis: Axis) -> (Rect, Option<(Rect, Rect)>) {
    if axis != Axis::Horizontal || area.width < 2 * STEP_CELLS + MIN_TRACK_CELLS {
        return (area, None);
    }
    let thickness = match touch {
        true => TOUCH_HIT_CELLS,
        false => STEP_HIT_ROWS,
    };
    let zones = strip(area, axis, thickness.min(axis.across(area)));
    let track = Rect {
        x: area.x + STEP_CELLS,
        width: area.width - 2 * STEP_CELLS,
        ..area
    };
    let back = Rect {
        width: STEP_CELLS,
        ..zones
    };
    let forward = Rect {
        x: zones.right() - STEP_CELLS,
        width: STEP_CELLS,
        ..zones
    };
    (track, Some((back, forward)))
}

/// The one cell a step zone paints: its middle, on the bar's own row, which is
/// the last row of a zone that reaches up to meet a fingertip.
fn arrow_cell(zone: Rect) -> (u16, u16) {
    (zone.x + zone.width / 2, zone.bottom() - 1)
}

/// Rounds to nearest rather than truncating, so the thumb sits where the eye
/// expects it at both ends of the track.
fn round_div(numerator: u64, denominator: u64) -> u64 {
    (numerator + denominator / 2) / denominator
}

fn round_div_signed(numerator: i64, denominator: i64) -> i64 {
    match numerator < 0 {
        true => -(((-numerator) + denominator / 2) / denominator),
        false => (numerator + denominator / 2) / denominator,
    }
}

fn is_fine(event: &MouseEvent) -> bool {
    event.modifiers.contains(KeyModifiers::ALT)
}

/// How the thumb is being drawn, which is the only affordance a terminal has
/// for saying the bar takes the pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThumbState {
    Idle,
    Hovered,
    Grabbed,
}

/// What the hint beside a dragged thumb counts. A transcript reads in messages
/// because that is what its reader is looking for; everything else reads in
/// lines because that is all it has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollUnit {
    Line,
    Message,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScrollHint {
    current: u32,
    total: u32,
    unit: ScrollUnit,
}

impl ScrollHint {
    pub fn lines(current: u32, total: u32) -> Self {
        Self {
            current,
            total,
            unit: ScrollUnit::Line,
        }
    }

    pub fn messages(current: u32, total: u32) -> Self {
        Self {
            current,
            total,
            unit: ScrollUnit::Message,
        }
    }

    fn text(&self) -> String {
        let label = match self.unit {
            ScrollUnit::Line => LINE_HINT,
            ScrollUnit::Message => MESSAGE_HINT,
        };
        format!("{label} {}/{}", self.current, self.total)
    }
}

/// Where a drag started, in both spaces at once. Anchoring the drag at the
/// grabbed point rather than recomputing the offset from the pointer keeps that
/// point under the cursor, and makes returning to it restore the offset exactly
/// instead of landing a cell's worth of lines away.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScrollGrab {
    along: u16,
    offset: u32,
}

impl ScrollGrab {
    pub fn offset(&self) -> u32 {
        self.offset
    }
}

/// The strip a bar was painted into and what it was painted from. Built only
/// when the content overflows: a surface that fits has no track at all, and a
/// press in its column belongs to whatever is drawn there instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScrollTrack {
    axis: Axis,
    area: Rect,
    /// Where a press counts, which is the painted cells plus a margin under
    /// touch. Kept beside the paint rather than recomputed, so hit-testing and
    /// rendering cannot drift apart.
    hit: Rect,
    /// The step zones flanking the track, when the bar is wide enough to wear
    /// them. Outside `area`, so the thumb never shares a cell with an arrow.
    steps: Option<(Rect, Rect)>,
    total: u32,
    /// How much content is on screen, which the arrows make shorter than the
    /// track: they take cells from the bar without hiding anything.
    viewport: u32,
    position: u32,
}

impl ScrollTrack {
    /// Takes the last column of whatever it is handed, so a caller that has
    /// already reserved the strip and one that passes the whole pane both get
    /// a bar on the right edge. Getting that wrong puts the thumb on the left
    /// margin and hands the entire body to the pointer, so it is settled here
    /// rather than at two dozen call sites.
    pub fn new(area: Rect, total: u32, position: u32) -> Option<Self> {
        Self::place(Axis::Vertical, area, total, position)
    }

    /// The same, along the last row of what it is handed. A caller that wants a
    /// touch margin above the paint hands over more than one row.
    fn place(axis: Axis, area: Rect, total: u32, position: u32) -> Option<Self> {
        let viewport = u32::from(axis.along(area));
        if axis.across(area) == 0 || viewport == 0 || total <= viewport {
            return None;
        }
        let (track, steps) = split_steps(area, touch(), axis);
        Some(Self {
            axis,
            area: strip(track, axis, 1),
            hit: hit_area(track, touch(), axis),
            steps,
            total,
            viewport,
            position: position.min(total - viewport),
        })
    }

    pub fn area(&self) -> Rect {
        self.area
    }

    pub fn position(&self) -> u32 {
        self.position
    }

    pub fn max_scroll(&self) -> u32 {
        self.total - self.viewport
    }

    /// The thumb's offset into the track and its length, both in cells. Length
    /// is the share of the document on screen; offset maps the position onto
    /// the cells the thumb has left. With no arrows the track is the viewport
    /// and both reduce to the ratio against `total`.
    fn span(&self) -> (u32, u32) {
        let track = u64::from(self.axis.along(self.area));
        let length =
            round_div(track * u64::from(self.viewport), u64::from(self.total)).clamp(1, track);
        let travel = track - length;
        let start = round_div(
            u64::from(self.position) * travel,
            u64::from(self.max_scroll()),
        )
        .min(travel);
        (start as u32, length as u32)
    }

    /// The thumb's first cell in screen space, along the axis, and its length.
    pub fn thumb(&self) -> (u16, u16) {
        let (start, length) = self.span();
        (self.axis.start(self.area) + start as u16, length as u16)
    }

    /// How far the thumb can move. Zero only when it fills the track, which
    /// [`ScrollTrack::new`] already refuses to build.
    fn travel(&self) -> u32 {
        u32::from(self.axis.along(self.area)) - self.span().1
    }

    pub fn contains(&self, at: Position) -> bool {
        self.hit.contains(at)
    }

    /// Where a tap on one of the arrows lands, or `None` when the press is on
    /// neither. A page rather than a line: a bar exists only once the content
    /// overflows, and walking a wide table back a column at a time is no
    /// better than not reaching it.
    pub fn step_at(&self, at: Position) -> Option<u32> {
        let (back, forward) = self.steps?;
        let page = (self.viewport / STEP_DIVISOR).max(1);
        if back.contains(at) {
            return Some(self.position.saturating_sub(page));
        }
        forward
            .contains(at)
            .then(|| (self.position + page).min(self.max_scroll()))
    }

    /// The ends of the document belong to the ends of the track by definition,
    /// and the rounding that places a thumb on a whole cell cannot be trusted to
    /// agree. Consulted by both the grab and the drag so a press on the last
    /// cell and a drag onto it mean the same thing.
    fn endpoint(&self, along: u16) -> Option<u32> {
        if along <= self.axis.start(self.area) {
            return Some(0);
        }
        if along + 1 >= self.axis.end(self.area) {
            return Some(self.max_scroll());
        }
        None
    }

    /// A press on the thumb keeps the current offset; a press anywhere else on
    /// the track centres the thumb on that cell first. Either way the result is
    /// an anchor, so one press seeks coarsely and then adjusts finely without
    /// being released.
    pub fn grab(&self, along: u16) -> ScrollGrab {
        if let Some(offset) = self.endpoint(along) {
            return ScrollGrab { along, offset };
        }
        let (start, length) = self.span();
        let local = u32::from(along.saturating_sub(self.axis.start(self.area)));
        if local >= start && local < start + length {
            return ScrollGrab {
                along,
                offset: self.position,
            };
        }
        let travel = self.travel();
        let wanted = local.saturating_sub(length / 2).min(travel);
        let offset = match travel {
            0 => 0,
            travel => round_div(
                u64::from(wanted) * u64::from(self.max_scroll()),
                u64::from(travel),
            ) as u32,
        };
        ScrollGrab { along, offset }
    }

    /// Pointer motion mapped through the thumb's own travel, not the whole
    /// track: the thumb has `travel` cells in which to cover `max_scroll` lines.
    ///
    /// The ends of the track are taken as the ends of the document rather than
    /// computed. A thumb is painted on whole cells, so the offset its grabbed
    /// cell stands for is only accurate to the rounding that put it there, and
    /// an interior drag that is reversible cannot also land exactly on the last
    /// line. Reaching the end is the more important of the two, and the first
    /// and last cells of a track mean nothing else.
    pub fn offset_at(&self, grab: &ScrollGrab, along: u16, fine: bool) -> u32 {
        let travel = self.travel();
        if travel == 0 {
            return grab.offset;
        }
        let max_scroll = self.max_scroll();
        if let Some(offset) = self.endpoint(along).filter(|_| !fine) {
            return offset;
        }
        let cells = i64::from(along) - i64::from(grab.along);
        let lines = round_div_signed(cells * i64::from(max_scroll), i64::from(travel));
        let lines = match fine {
            true => lines / FINE_DIVISOR,
            false => lines,
        };
        (i64::from(grab.offset) + lines).clamp(0, i64::from(max_scroll)) as u32
    }

    pub fn render(&self, buf: &mut Buffer, style: Style, state: ThumbState) {
        let (start, length) = self.thumb();
        let (symbol, style) = match state {
            ThumbState::Idle => (self.axis.thumb(), style),
            ThumbState::Hovered => (self.axis.thumb(), style.add_modifier(Modifier::BOLD)),
            ThumbState::Grabbed => (SCROLLBAR_THUMB_GRABBED, style.add_modifier(Modifier::BOLD)),
        };
        let end = start
            .saturating_add(length)
            .min(self.axis.end(self.area));
        for along in start..end {
            if let Some(cell) = buf.cell_mut(self.axis.cell(self.area, along)) {
                cell.set_symbol(symbol);
                cell.set_style(style);
            }
        }
        self.render_steps(buf, style);
    }

    /// An arrow dims once its direction is spent, which is the only way a bar
    /// that takes taps can say a tap would do nothing.
    fn render_steps(&self, buf: &mut Buffer, style: Style) {
        let Some((back, forward)) = self.steps else {
            return;
        };
        for (zone, symbol, spent) in [
            (back, SCROLLBAR_STEP_BACK, self.position == 0),
            (
                forward,
                SCROLLBAR_STEP_FORWARD,
                self.position >= self.max_scroll(),
            ),
        ] {
            if let Some(cell) = buf.cell_mut(arrow_cell(zone)) {
                cell.set_symbol(symbol);
                cell.set_style(match spent {
                    true => style.add_modifier(Modifier::DIM),
                    false => style,
                });
            }
        }
    }
}

/// What a surface should do with a mouse event it offered to its bar.
/// `Ignored` has to fall through: a press in the column of a bar that is not
/// showing belongs to whatever is painted there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollbarMouse {
    Ignored,
    Consumed,
    ScrollTo(u32),
}

/// A bar and the drag that owns it, held by the surface that draws it. Placed
/// during rendering and consulted during input, which is how every other hit
/// region in this codebase works.
#[derive(Clone, Debug, Default)]
pub struct Scrollbar {
    axis: Axis,
    track: Option<ScrollTrack>,
    grab: Option<ScrollGrab>,
    fine: bool,
    hovered: bool,
    hint: Option<ScrollHint>,
}

impl Scrollbar {
    /// A bar along the bottom of what it is placed on, for a surface whose lines
    /// run wider than it does. `Default` is the vertical bar.
    pub fn horizontal() -> Self {
        Self {
            axis: Axis::Horizontal,
            ..Self::default()
        }
    }

    /// Records the strip the bar owns. A surface whose content fits leaves no
    /// track behind, which drops any drag that was in flight when it shrank.
    pub fn place(&mut self, area: Rect, total: u32, position: u32) {
        self.track = ScrollTrack::place(self.axis, area, total, position);
        if self.track.is_none() {
            self.grab = None;
            self.hovered = false;
        }
    }

    /// Only drawn while a drag is live. Set by surfaces wide enough to spare
    /// the columns beside their bar.
    pub fn set_hint(&mut self, hint: ScrollHint) {
        self.hint = Some(hint);
    }

    pub fn is_dragging(&self) -> bool {
        self.grab.is_some()
    }

    pub fn track(&self) -> Option<&ScrollTrack> {
        self.track.as_ref()
    }

    pub fn handle(&mut self, event: &MouseEvent) -> ScrollbarMouse {
        let Some(track) = self.track.clone() else {
            return ScrollbarMouse::Ignored;
        };
        let at = Position::new(event.column, event.row);
        let along = self.axis.pointer(event);
        // A step is a discrete action, so it leaves no grab behind: the release
        // that follows falls through, which every surface drawing a bar already
        // ignores.
        if matches!(event.kind, MouseEventKind::Down(MouseButton::Left))
            && let Some(offset) = track.step_at(at)
        {
            return ScrollbarMouse::ScrollTo(offset);
        }
        match event.kind {
            MouseEventKind::Moved => {
                self.hovered = track.contains(at);
                ScrollbarMouse::Ignored
            }
            MouseEventKind::Down(MouseButton::Left) if track.contains(at) => {
                let grab = track.grab(along);
                let offset = grab.offset;
                self.fine = is_fine(event);
                self.hovered = true;
                self.grab = Some(grab);
                ScrollbarMouse::ScrollTo(offset)
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some(grab) = self.grab.as_ref() else {
                    return ScrollbarMouse::Ignored;
                };
                // Re-anchoring on the modifier flip is what keeps the two
                // regimes continuous: fine motion departs from wherever the
                // view stood when Alt moved, rather than from the grab.
                if is_fine(event) != self.fine {
                    self.fine = !self.fine;
                    self.grab = Some(ScrollGrab {
                        along,
                        offset: track.position(),
                    });
                    return ScrollbarMouse::ScrollTo(track.position());
                }
                ScrollbarMouse::ScrollTo(track.offset_at(grab, along, self.fine))
            }
            MouseEventKind::Up(MouseButton::Left) if self.grab.is_some() => {
                self.grab = None;
                self.fine = false;
                ScrollbarMouse::Consumed
            }
            _ => ScrollbarMouse::Ignored,
        }
    }

    pub fn render(&self, buf: &mut Buffer, style: Style) {
        let Some(track) = self.track.as_ref() else {
            return;
        };
        track.render(buf, style, self.state());
        // A touch grab lives only between the press and the release of one tap,
        // which is long enough to catch a frame and flash a chip nobody can act
        // on.
        if let Some(hint) = self
            .hint
            .as_ref()
            .filter(|_| self.grab.is_some() && !touch())
        {
            render_hint(buf, track, hint, style);
        }
    }

    fn state(&self) -> ThumbState {
        match (self.grab.is_some(), self.hovered) {
            (true, _) => ThumbState::Grabbed,
            (false, true) => ThumbState::Hovered,
            (false, false) => ThumbState::Idle,
        }
    }
}

fn render_hint(buf: &mut Buffer, track: &ScrollTrack, hint: &ScrollHint, style: Style) {
    // The chip needs the columns beside the thumb, which only a vertical bar has.
    if track.axis == Axis::Horizontal {
        return;
    }
    let text = hint.text();
    let width = UnicodeWidthStr::width(text.as_str()) as u16;
    let area = track.area();
    let Some(x) = area.x.checked_sub(width + HINT_GAP) else {
        return;
    };
    let (row, _) = track.thumb();
    chrome::render_line(
        buf,
        Rect::new(x, row, width, 1),
        Line::styled(text, style.add_modifier(Modifier::REVERSED)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const BAR: Rect = Rect {
        x: 10,
        y: 0,
        width: 1,
        height: 10,
    };
    /// The same track turned on its side: ten cells long, one thick.
    const H_BAR: Rect = Rect {
        x: 0,
        y: 4,
        width: 10,
        height: 1,
    };
    /// A surface wide enough for the touch margin to have somewhere to go.
    const PANE: Rect = Rect {
        x: 4,
        y: 0,
        width: 20,
        height: 10,
    };
    const WRONG_THUMB: &str = "the thumb is not where the content says it is";
    const WRONG_OFFSET: &str = "the drag landed on the wrong offset";
    const WRONG_TRACK: &str = "the track was built for content that does not overflow";
    const WRONG_HIT: &str = "the press did not land where the bar accepts one";
    const WRONG_PAINT: &str = "the bar painted a different strip than it was placed on";
    const AXES_DISAGREE: &str = "the two axes measured the same content differently";
    const WRONG_HINT: &str = "a bar with no room beside it painted a hint anyway";
    const WRONG_STEP: &str = "the arrow moved the document somewhere else";
    /// Five times [`PANE`]'s width, so a page is a round number of columns.
    const PANNED_TOTAL: u32 = 100;
    const PAGE: u32 = PANE.width as u32 / STEP_DIVISOR;
    const PANNED_MAX: u32 = PANNED_TOTAL - PANE.width as u32;

    fn track(total: u32, position: u32) -> ScrollTrack {
        placed(Axis::Vertical, total, position)
    }

    fn placed(axis: Axis, total: u32, position: u32) -> ScrollTrack {
        let area = match axis {
            Axis::Vertical => BAR,
            Axis::Horizontal => H_BAR,
        };
        ScrollTrack::place(axis, area, total, position).expect("overflowing content")
    }

    /// The thumb measured from the start of its own track, which is the only way
    /// two axes sitting at different screen coordinates can be compared.
    fn relative_thumb(track: &ScrollTrack) -> (u16, u16) {
        let (start, length) = track.thumb();
        (start - track.axis.start(track.area), length)
    }

    /// Mirrors what [`ScrollTrack::new`] does under touch; the flag itself is a
    /// process-wide `OnceLock` that a test cannot set without racing its peers.
    fn touch_track(area: Rect, total: u32) -> ScrollTrack {
        let mut track = ScrollTrack::new(area, total, 0).expect("overflowing content");
        track.hit = hit_area(area, true, Axis::Vertical);
        track
    }

    #[test_case(Axis::Vertical, 10 ; "vertical content exactly fills the pane")]
    #[test_case(Axis::Vertical, 3 ; "vertical content is shorter than the pane")]
    #[test_case(Axis::Vertical, 0 ; "there is no vertical content at all")]
    #[test_case(Axis::Horizontal, 10 ; "horizontal content exactly fills the pane")]
    #[test_case(Axis::Horizontal, 3 ; "horizontal content is shorter than the pane")]
    #[test_case(Axis::Horizontal, 0 ; "there is no horizontal content at all")]
    fn content_that_fits_gets_no_track(axis: Axis, total: u32) {
        let area = match axis {
            Axis::Vertical => BAR,
            Axis::Horizontal => H_BAR,
        };
        assert!(
            ScrollTrack::place(axis, area, total, 0).is_none(),
            "{WRONG_TRACK}"
        );
    }

    #[test_case(Axis::Vertical, Rect { height: 0, ..BAR } ; "a vertical strip with no length")]
    #[test_case(Axis::Vertical, Rect { width: 0, ..BAR } ; "a vertical strip with no column to paint")]
    #[test_case(Axis::Horizontal, Rect { width: 0, ..H_BAR } ; "a horizontal strip with no length")]
    #[test_case(Axis::Horizontal, Rect { height: 0, ..H_BAR } ; "a horizontal strip with no row to paint")]
    fn a_strip_with_no_room_gets_no_track(axis: Axis, area: Rect) {
        assert!(
            ScrollTrack::place(axis, area, 500, 0).is_none(),
            "{WRONG_TRACK}"
        );
    }

    /// The generalisation is only worth having if it did not skew one direction:
    /// the same content over the same track length has to place the same thumb
    /// and allow the same travel whichever way the bar runs.
    #[test_case(20, 0 ; "a half length thumb at the start")]
    #[test_case(20, 10 ; "the same thumb at the end")]
    #[test_case(1000, 400 ; "a one cell thumb midway")]
    fn the_two_axes_measure_the_same_content(total: u32, position: u32) {
        let vertical = placed(Axis::Vertical, total, position);
        let horizontal = placed(Axis::Horizontal, total, position);

        assert_eq!(
            relative_thumb(&vertical),
            relative_thumb(&horizontal),
            "{AXES_DISAGREE}"
        );
        assert_eq!(
            vertical.max_scroll(),
            horizontal.max_scroll(),
            "{AXES_DISAGREE}"
        );
    }

    /// A horizontal drag reads the column, a vertical one the row, and both ends
    /// of either track are the ends of the document.
    #[test_case(Axis::Vertical ; "vertical")]
    #[test_case(Axis::Horizontal ; "horizontal")]
    fn a_drag_along_either_axis_reaches_both_ends(axis: Axis) {
        let track = placed(axis, 200, 95);
        let start = axis.start(track.area);
        let grab = track.grab(track.thumb().0);

        assert_eq!(track.offset_at(&grab, start, false), 0, "{WRONG_OFFSET}");
        assert_eq!(
            track.offset_at(&grab, axis.end(track.area) - 1, false),
            track.max_scroll(),
            "{WRONG_OFFSET}"
        );
    }

    #[test_case(20, 0, 0, 5 ; "twice the pane parks a half length thumb at the top")]
    #[test_case(20, 10, 5, 5 ; "the same thumb at the bottom sits flush")]
    #[test_case(100, 0, 0, 1 ; "ten times the pane shrinks the thumb to one row")]
    #[test_case(20000, 0, 0, 1 ; "a huge document cannot shrink it below one row")]
    #[test_case(11, 0, 0, 9 ; "one row of overflow leaves the thumb nearly full")]
    fn the_thumb_is_proportional(total: u32, position: u32, start: u16, length: u16) {
        let (row, len) = track(total, position).thumb();

        assert_eq!((row - BAR.y, len), (start, length), "{WRONG_THUMB}");
    }

    #[test_case(20 ; "a half length thumb")]
    #[test_case(100 ; "a one row thumb")]
    #[test_case(20000 ; "a one row thumb on a huge document")]
    fn a_thumb_at_the_end_never_overruns_the_track(total: u32) {
        let track = track(total, u32::MAX);
        let (row, length) = track.thumb();

        assert_eq!(row + length, BAR.bottom(), "{WRONG_THUMB}");
    }

    #[test_case(20, 7 ; "a half length thumb")]
    #[test_case(20000, 4000 ; "a one row thumb on a huge document")]
    fn grabbing_without_moving_does_not_scroll(total: u32, position: u32) {
        let track = track(total, position);
        let (row, _) = track.thumb();
        let grab = track.grab(row);

        assert_eq!(grab.offset(), position, "{WRONG_OFFSET}");
        assert_eq!(
            track.offset_at(&grab, row, false),
            position,
            "{WRONG_OFFSET}"
        );
    }

    /// A press off the thumb seeks, and the anchor it leaves behind has to
    /// agree with where it seeked to or the first drag row would jump.
    #[test]
    fn a_press_off_the_thumb_seeks_and_anchors_there() {
        let track = track(110, 0);
        let grab = track.grab(BAR.y + 5);

        assert!(grab.offset() > 0, "{WRONG_OFFSET}");
        assert_eq!(
            track.offset_at(&grab, BAR.y + 5, false),
            grab.offset(),
            "{WRONG_OFFSET}"
        );
    }

    #[test]
    fn dragging_down_never_scrolls_back_up() {
        let track = track(500, 200);
        let grab = track.grab(track.thumb().0);

        let offsets: Vec<u32> = (BAR.y..BAR.bottom())
            .map(|row| track.offset_at(&grab, row, false))
            .collect();

        assert!(offsets.windows(2).all(|w| w[0] <= w[1]), "{WRONG_OFFSET}");
    }

    #[test]
    fn a_fine_drag_covers_an_eighth_of_the_distance() {
        let track = track(1000, 0);
        let grab = track.grab(BAR.y);
        let row = BAR.y + 4;

        let coarse = track.offset_at(&grab, row, false);
        let fine = track.offset_at(&grab, row, true);

        assert_eq!(fine, coarse / FINE_DIVISOR as u32, "{WRONG_OFFSET}");
    }

    #[test]
    fn a_drag_up_and_back_returns_to_the_grab() {
        let track = track(4000, 1800);
        let (thumb, _) = track.thumb();
        let grab = track.grab(thumb);

        assert!(
            track.offset_at(&grab, thumb - 2, false) < 1800,
            "{WRONG_OFFSET}"
        );
        assert_eq!(track.offset_at(&grab, thumb, false), 1800, "{WRONG_OFFSET}");
    }

    #[test_case(PANE, false, 1 ; "a pointer gets the painted column alone")]
    #[test_case(PANE, true, TOUCH_HIT_CELLS ; "a finger gets a margin beside it")]
    #[test_case(BAR, true, 1 ; "a one column surface has no margin to give")]
    fn the_hit_strip_widens_only_for_touch(area: Rect, touch: bool, width: u16) {
        let hit = hit_area(area, touch, Axis::Vertical);

        assert_eq!(
            (hit.right(), hit.width),
            (area.right(), width),
            "{WRONG_HIT}"
        );
    }

    /// The touch margin is measured across the bar, so on a horizontal one it
    /// reaches up into the rows above the paint rather than out to the side.
    #[test_case(PANE, false, 1 ; "a pointer gets the painted row alone")]
    #[test_case(PANE, true, TOUCH_HIT_CELLS ; "a finger gets a margin above it")]
    #[test_case(H_BAR, true, 1 ; "a one row surface has no margin to give")]
    fn a_horizontal_hit_strip_thickens_upwards(area: Rect, touch: bool, height: u16) {
        let hit = hit_area(area, touch, Axis::Horizontal);

        assert_eq!(
            (hit.bottom(), hit.height),
            (area.bottom(), height),
            "{WRONG_HIT}"
        );
    }

    #[test]
    fn a_finger_may_press_beside_the_bar() {
        let track = touch_track(PANE, 100);
        let thumb = track.thumb().0;

        for column in track.area().x + 1 - TOUCH_HIT_CELLS..=track.area().x {
            assert!(track.contains(Position::new(column, thumb)), "{WRONG_HIT}");
        }
        assert!(
            !track.contains(Position::new(track.area().x - TOUCH_HIT_CELLS, thumb)),
            "{WRONG_HIT}"
        );
    }

    #[test]
    fn the_widened_strip_does_not_widen_the_paint() {
        let track = touch_track(PANE, 100);
        let mut buf = Buffer::empty(PANE);

        track.render(&mut buf, Style::default(), ThumbState::Idle);

        let painted = (PANE.x..PANE.right())
            .filter(|&x| buf[(x, track.thumb().0)].symbol() == SCROLLBAR_THUMB)
            .collect::<Vec<_>>();
        assert_eq!(painted, vec![track.area().x], "{WRONG_PAINT}");
    }

    /// A horizontal bar paints along the last row of what it was handed, with the
    /// sideways thumb, and leaves the rest of the surface alone.
    #[test]
    fn a_horizontal_bar_paints_along_the_bottom_row() {
        let track = ScrollTrack::place(Axis::Horizontal, PANE, 100, 0).expect("overflow");
        let mut buf = Buffer::empty(PANE);

        track.render(&mut buf, Style::default(), ThumbState::Idle);

        let (start, length) = track.thumb();
        let painted = (PANE.x..PANE.right())
            .filter(|&x| buf[(x, PANE.bottom() - 1)].symbol() == SCROLLBAR_THUMB_HORIZONTAL)
            .collect::<Vec<_>>();
        assert_eq!(
            painted,
            (start..start + length).collect::<Vec<_>>(),
            "{WRONG_PAINT}"
        );
        for y in PANE.y..PANE.bottom() - 1 {
            assert_eq!(buf[(start, y)].symbol(), " ", "{WRONG_PAINT}");
        }
    }

    /// The position chip is painted in the columns left of the thumb. A
    /// horizontal thumb has content there, and its `thumb()` reports a column, so
    /// drawing the chip anyway would put it at an arbitrary row.
    #[test]
    fn a_horizontal_bar_never_paints_a_position_hint() {
        let mut bar = Scrollbar::horizontal();
        bar.place(PANE, 100, 40);
        bar.set_hint(ScrollHint::lines(40, 100));
        bar.grab = Some(bar.track().expect("overflow").grab(PANE.x));
        let mut buf = Buffer::empty(PANE);

        bar.render(&mut buf, Style::default());

        let text: String = (PANE.x..PANE.right())
            .map(|x| buf[(x, PANE.bottom() - 1)].symbol())
            .collect();
        assert!(!text.contains(LINE_HINT), "{WRONG_HINT}");
    }

    fn panned(position: u32) -> ScrollTrack {
        ScrollTrack::place(Axis::Horizontal, PANE, PANNED_TOTAL, position).expect("overflow")
    }

    /// The middle of a step zone, which is also the cell its arrow is painted on.
    fn tap(zone: Rect) -> Position {
        let (x, y) = arrow_cell(zone);
        Position::new(x, y)
    }

    fn zones(track: &ScrollTrack) -> (Rect, Rect) {
        track.steps.expect("a bar wide enough to wear arrows")
    }

    /// Half a viewport, not half a track: the arrows take cells from the bar
    /// without hiding any content, so a page is still what is on screen.
    #[test_case(0, PAGE ; "forward from the start")]
    #[test_case(PAGE, 2 * PAGE ; "forward again from there")]
    fn a_tap_on_an_arrow_moves_by_half_a_viewport(from: u32, expected: u32) {
        let track = panned(from);
        let (back, forward) = zones(&track);

        assert_eq!(track.step_at(tap(forward)), Some(expected), "{WRONG_STEP}");
        assert_eq!(
            track.step_at(tap(back)),
            Some(from.saturating_sub(PAGE)),
            "{WRONG_STEP}"
        );
    }

    #[test_case(0, true ; "back from the start stays there")]
    #[test_case(PANNED_MAX, false ; "forward from the end stays there")]
    fn a_step_never_leaves_the_document(from: u32, backwards: bool) {
        let track = panned(from);
        let (back, forward) = zones(&track);
        let zone = match backwards {
            true => back,
            false => forward,
        };

        assert_eq!(track.step_at(tap(zone)), Some(from), "{WRONG_STEP}");
    }

    /// The arrows sit outside the track, so the thumb never shares a cell with
    /// one and a tap on an arrow is a step rather than a seek.
    #[test]
    fn the_seek_track_stops_short_of_the_arrows() {
        let track = panned(0);
        let (back, forward) = zones(&track);

        assert_eq!(
            (track.area.x, track.area.right()),
            (back.right(), forward.x),
            "{WRONG_PAINT}"
        );
        assert!(!track.contains(tap(back)), "{WRONG_HIT}");
        assert!(!track.contains(tap(forward)), "{WRONG_HIT}");
    }

    /// The thumb's length is the share of the document on screen, which the
    /// arrows leave alone: measuring it against the shortened track instead
    /// would report less content than the reader can see.
    #[test]
    fn a_thumb_measures_the_viewport_not_the_shortened_track() {
        let track = panned(0);
        let along = u32::from(track.axis.along(track.area));

        assert_eq!(along, PANE.width as u32 - 2 * STEP_CELLS as u32, "{WRONG_PAINT}");
        assert_eq!(track.max_scroll(), PANNED_MAX, "{WRONG_THUMB}");
        assert_eq!(
            u32::from(track.thumb().1),
            round_div(u64::from(along) * u64::from(PANE.width), PANNED_TOTAL as u64) as u32,
            "{WRONG_THUMB}"
        );
    }

    #[test]
    fn a_thumb_still_reaches_the_end_of_a_shortened_track() {
        let track = panned(PANNED_MAX);
        let (start, length) = track.thumb();

        assert_eq!(start + length, track.axis.end(track.area), "{WRONG_THUMB}");
    }

    /// A finger's margin is measured across the bar either way. Unlike the seek
    /// track an arrow keeps a second row without touch, because a terminal
    /// driven by touch over SSH reports nothing that would reveal it.
    #[test_case(false, STEP_HIT_ROWS ; "a pointer still gets two rows")]
    #[test_case(true, TOUCH_HIT_CELLS ; "a finger gets the full margin")]
    fn an_arrow_reaches_up_off_its_row(touch: bool, height: u16) {
        let (_, steps) = split_steps(PANE, touch, Axis::Horizontal);
        let (back, forward) = steps.expect("a bar wide enough to wear arrows");

        for zone in [back, forward] {
            assert_eq!(
                (zone.bottom(), zone.height, zone.width),
                (PANE.bottom(), height, STEP_CELLS),
                "{WRONG_HIT}"
            );
        }
    }

    /// Arrows never take more than half a row, and a vertical bar gets none at
    /// all: the wheel every terminal reports already steps it.
    #[test_case(Axis::Horizontal, Rect { width: 2 * STEP_CELLS + MIN_TRACK_CELLS - 1, ..H_BAR } ; "a row too short to spare them")]
    #[test_case(Axis::Vertical, PANE ; "a vertical bar")]
    fn a_bar_without_room_keeps_its_whole_track(axis: Axis, area: Rect) {
        let (track, steps) = split_steps(area, false, axis);

        assert_eq!(track, area, "{WRONG_PAINT}");
        assert!(steps.is_none(), "{WRONG_PAINT}");
    }

    /// The arrows are the affordance, so they have to be visible, and a spent
    /// one has to say so: a tap that would do nothing is otherwise silent.
    #[test_case(0, SCROLLBAR_STEP_BACK ; "back is spent at the start")]
    #[test_case(PANNED_MAX, SCROLLBAR_STEP_FORWARD ; "forward is spent at the end")]
    fn a_spent_arrow_is_painted_dim(position: u32, spent: &str) {
        let track = panned(position);
        let mut buf = Buffer::empty(PANE);

        track.render(&mut buf, Style::default(), ThumbState::Idle);

        let (back, forward) = zones(&track);
        for zone in [back, forward] {
            let (x, y) = arrow_cell(zone);
            let dim = buf[(x, y)].style().add_modifier.contains(Modifier::DIM);
            assert_eq!(dim, buf[(x, y)].symbol() == spent, "{WRONG_PAINT}");
        }
    }
}
