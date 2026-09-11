//! The vertical scrollbar every scrollable surface shares: its geometry, its
//! paint, and the drag that makes a tall document reachable by pointer.
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
/// The thumb while it is held. A terminal cannot change the cursor over a
/// widget, so the thumb has to say for itself that it has been grabbed.
pub const SCROLLBAR_THUMB_GRABBED: &str = "█";
/// A fine drag covers this fraction of the distance a plain one would. A short
/// track over a long document is hundreds of lines per row, and landing on a
/// particular one is otherwise a matter of luck.
const FINE_DIVISOR: i64 = 8;
const LINE_HINT: &str = "line";
const MESSAGE_HINT: &str = "message";
/// Columns between the position hint and the bar, so the chip does not touch
/// the thumb it describes.
const HINT_GAP: u16 = 1;
/// How wide a press on the bar may land under touch. A fingertip covers several
/// cells and reports their centroid, so a one-column target is unhittable while
/// three is comfortable. Paint stays one column wide either way.
const TOUCH_HIT_COLUMNS: u16 = 3;

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

/// The strip a press may land on, given the surface [`ScrollTrack::new`] was
/// handed. Wider than the paint under touch, clamped to the surface so a narrow
/// pane cannot claim presses from its neighbour, and exactly the painted column
/// otherwise.
fn hit_area(area: Rect, touch: bool) -> Rect {
    let width = match touch {
        true => TOUCH_HIT_COLUMNS.min(area.width),
        false => 1,
    };
    Rect {
        x: area.right() - width,
        width,
        ..area
    }
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
/// grabbed point rather than recomputing the offset from the pointer row keeps
/// that point under the cursor, and makes returning to it restore the offset
/// exactly instead of landing a row's worth of lines away.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScrollGrab {
    row: u16,
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
    area: Rect,
    /// Where a press counts, which is the painted column plus a margin under
    /// touch. Kept beside the paint rather than recomputed, so hit-testing and
    /// rendering cannot drift apart.
    hit: Rect,
    total: u32,
    position: u32,
}

impl ScrollTrack {
    /// Takes the last column of whatever it is handed, so a caller that has
    /// already reserved the strip and one that passes the whole pane both get
    /// a bar on the right edge. Getting that wrong puts the thumb on the left
    /// margin and hands the entire body to the pointer, so it is settled here
    /// rather than at two dozen call sites.
    pub fn new(area: Rect, total: u32, position: u32) -> Option<Self> {
        let height = u32::from(area.height);
        if area.width == 0 || height == 0 || total <= height {
            return None;
        }
        Some(Self {
            area: Rect {
                x: area.right() - 1,
                width: 1,
                ..area
            },
            hit: hit_area(area, touch()),
            total,
            position: position.min(total - height),
        })
    }

    pub fn area(&self) -> Rect {
        self.area
    }

    pub fn position(&self) -> u32 {
        self.position
    }

    pub fn max_scroll(&self) -> u32 {
        self.total - u32::from(self.area.height)
    }

    /// The thumb's offset into the track and its length, both in rows.
    fn span(&self) -> (u32, u32) {
        let track = u64::from(self.area.height);
        let total = u64::from(self.total);
        let length = round_div(track * track, total).clamp(1, track);
        let travel = track - length;
        let start = round_div(u64::from(self.position) * track, total).min(travel);
        (start as u32, length as u32)
    }

    /// The thumb's first row in screen space, and its length.
    pub fn thumb(&self) -> (u16, u16) {
        let (start, length) = self.span();
        (self.area.y + start as u16, length as u16)
    }

    /// How far the thumb can move. Zero only when it fills the track, which
    /// [`ScrollTrack::new`] already refuses to build.
    fn travel(&self) -> u32 {
        u32::from(self.area.height) - self.span().1
    }

    pub fn contains(&self, at: Position) -> bool {
        self.hit.contains(at)
    }

    /// The ends of the document belong to the ends of the track by definition,
    /// and the rounding that places a thumb on a whole row cannot be trusted to
    /// agree. Consulted by both the grab and the drag so a press on the last
    /// row and a drag onto it mean the same thing.
    fn endpoint(&self, row: u16) -> Option<u32> {
        if row <= self.area.y {
            return Some(0);
        }
        if row + 1 >= self.area.bottom() {
            return Some(self.max_scroll());
        }
        None
    }

    /// A press on the thumb keeps the current offset; a press anywhere else on
    /// the track centres the thumb on that row first. Either way the result is
    /// an anchor, so one press seeks coarsely and then adjusts finely without
    /// being released.
    pub fn grab(&self, row: u16) -> ScrollGrab {
        if let Some(offset) = self.endpoint(row) {
            return ScrollGrab { row, offset };
        }
        let (start, length) = self.span();
        let local = u32::from(row.saturating_sub(self.area.y));
        if local >= start && local < start + length {
            return ScrollGrab {
                row,
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
        ScrollGrab { row, offset }
    }

    /// Pointer motion mapped through the thumb's own travel, not the whole
    /// track: the thumb has `travel` rows in which to cover `max_scroll` lines.
    ///
    /// The ends of the track are taken as the ends of the document rather than
    /// computed. A thumb is painted on whole rows, so the offset its grabbed
    /// row stands for is only accurate to the rounding that put it there, and
    /// an interior drag that is reversible cannot also land exactly on the last
    /// line. Reaching the end is the more important of the two, and the first
    /// and last rows of a track mean nothing else.
    pub fn offset_at(&self, grab: &ScrollGrab, row: u16, fine: bool) -> u32 {
        let travel = self.travel();
        if travel == 0 {
            return grab.offset;
        }
        let max_scroll = self.max_scroll();
        if let Some(offset) = self.endpoint(row).filter(|_| !fine) {
            return offset;
        }
        let rows = i64::from(row) - i64::from(grab.row);
        let lines = round_div_signed(rows * i64::from(max_scroll), i64::from(travel));
        let lines = match fine {
            true => lines / FINE_DIVISOR,
            false => lines,
        };
        (i64::from(grab.offset) + lines).clamp(0, i64::from(max_scroll)) as u32
    }

    pub fn render(&self, buf: &mut Buffer, style: Style, state: ThumbState) {
        let (start, length) = self.thumb();
        let (symbol, style) = match state {
            ThumbState::Idle => (SCROLLBAR_THUMB, style),
            ThumbState::Hovered => (SCROLLBAR_THUMB, style.add_modifier(Modifier::BOLD)),
            ThumbState::Grabbed => (SCROLLBAR_THUMB_GRABBED, style.add_modifier(Modifier::BOLD)),
        };
        let end = start.saturating_add(length).min(self.area.bottom());
        for y in start..end {
            if let Some(cell) = buf.cell_mut((self.area.x, y)) {
                cell.set_symbol(symbol);
                cell.set_style(style);
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
    track: Option<ScrollTrack>,
    grab: Option<ScrollGrab>,
    fine: bool,
    hovered: bool,
    hint: Option<ScrollHint>,
}

impl Scrollbar {
    /// Records the strip the bar owns. A surface whose content fits leaves no
    /// track behind, which drops any drag that was in flight when it shrank.
    pub fn place(&mut self, area: Rect, total: u32, position: u32) {
        self.track = ScrollTrack::new(area, total, position);
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
        match event.kind {
            MouseEventKind::Moved => {
                self.hovered = track.contains(at);
                ScrollbarMouse::Ignored
            }
            MouseEventKind::Down(MouseButton::Left) if track.contains(at) => {
                let grab = track.grab(event.row);
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
                        row: event.row,
                        offset: track.position(),
                    });
                    return ScrollbarMouse::ScrollTo(track.position());
                }
                ScrollbarMouse::ScrollTo(track.offset_at(grab, event.row, self.fine))
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

    fn track(total: u32, position: u32) -> ScrollTrack {
        ScrollTrack::new(BAR, total, position).expect("overflowing content")
    }

    /// Mirrors what [`ScrollTrack::new`] does under touch; the flag itself is a
    /// process-wide `OnceLock` that a test cannot set without racing its peers.
    fn touch_track(area: Rect, total: u32) -> ScrollTrack {
        let mut track = ScrollTrack::new(area, total, 0).expect("overflowing content");
        track.hit = hit_area(area, true);
        track
    }

    #[test_case(10 ; "content exactly fills the pane")]
    #[test_case(3 ; "content is shorter than the pane")]
    #[test_case(0 ; "there is no content at all")]
    fn content_that_fits_gets_no_track(total: u32) {
        assert!(ScrollTrack::new(BAR, total, 0).is_none(), "{WRONG_TRACK}");
    }

    #[test]
    fn a_zero_height_strip_gets_no_track() {
        let flat = Rect { height: 0, ..BAR };
        assert!(ScrollTrack::new(flat, 500, 0).is_none(), "{WRONG_TRACK}");
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
    fn a_drag_to_the_track_ends_lands_on_the_extremes() {
        let track = track(200, 95);
        let grab = track.grab(track.thumb().0);

        assert_eq!(track.offset_at(&grab, BAR.y, false), 0, "{WRONG_OFFSET}");
        assert_eq!(
            track.offset_at(&grab, BAR.bottom() - 1, false),
            track.max_scroll(),
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
    #[test_case(PANE, true, TOUCH_HIT_COLUMNS ; "a finger gets a margin beside it")]
    #[test_case(BAR, true, 1 ; "a one column surface has no margin to give")]
    fn the_hit_strip_widens_only_for_touch(area: Rect, touch: bool, width: u16) {
        let hit = hit_area(area, touch);

        assert_eq!(
            (hit.right(), hit.width),
            (area.right(), width),
            "{WRONG_HIT}"
        );
    }

    #[test]
    fn a_finger_may_press_beside_the_bar() {
        let track = touch_track(PANE, 100);
        let thumb = track.thumb().0;

        for column in track.area().x + 1 - TOUCH_HIT_COLUMNS..=track.area().x {
            assert!(track.contains(Position::new(column, thumb)), "{WRONG_HIT}");
        }
        assert!(
            !track.contains(Position::new(track.area().x - TOUCH_HIT_COLUMNS, thumb)),
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
}
