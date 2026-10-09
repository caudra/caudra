//! Hover tooltips: a small box beside the control under a resting pointer.
//!
//! The box repeats what a control means or what was cut from it, never
//! anything only it says: motion reports are missing under some multiplexers
//! and keyboard users never see one. It holds no state of its own beyond the
//! dwell, and asks for the candidate afresh every frame, so the text tracks the
//! state it describes and a control that goes away takes its tooltip with it.

use std::time::{Duration, Instant};

use caudra_grab::grab_scope;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Widget};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::components::status_bar::StatusBarHitTarget;
use crate::input_document::PasteId;
use crate::repaint::Cadence;
use crate::theme;

/// How long the pointer rests on a control before its tooltip appears. Short
/// enough to answer a deliberate pause, long enough that sweeping across the
/// footer does not flicker a box over every chip it passes.
pub(crate) const TOOLTIP_DELAY: Duration = Duration::from_millis(500);
/// The border on each side, in both directions.
const CHROME: u16 = 2;
const MAX_TEXT_WIDTH: u16 = 48;
pub(crate) const MAX_TEXT_LINES: usize = 6;
/// Below this many columns of text a box is harder to read than no box.
const MIN_TEXT_WIDTH: u16 = 8;
const ELLIPSIS: char = '\u{2026}';
const TAB_WIDTH: usize = 4;
/// More than a full box of text, so a preview cut here still fills one.
const PREVIEW_CHARACTERS: usize = MAX_TEXT_LINES * MAX_TEXT_WIDTH as usize;

/// What the pointer rests on. Moving within the same target keeps the dwell,
/// so a box that has appeared stays put while the pointer wanders inside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TipKey {
    Status(StatusBarHitTarget),
    MessageAction(usize),
    Paste(PasteId),
    /// A list row, named by where it is drawn: rows have no identity that
    /// outlives a scroll, and a scrolled row is a different row to the reader.
    Row(Rect),
}

/// Where the box hangs from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Anchor {
    Area(Rect),
    /// The cell the pointer was on when the dwell started, for a target whose
    /// extent nobody records. Fixed for the dwell, so the box holds still.
    Pointer,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Tip {
    pub key: TipKey,
    pub anchor: Anchor,
    pub text: String,
}

struct Armed {
    key: TipKey,
    origin: Position,
    since: Instant,
}

pub(crate) struct Tooltip {
    enabled: bool,
    /// Where the pointer last moved to. `None` after a key, a press, a scroll
    /// or a resize, which dismiss the box until the pointer moves again.
    pointer: Option<Position>,
    armed: Option<Armed>,
}

impl Tooltip {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            pointer: None,
            armed: None,
        }
    }

    pub(crate) fn pointer_moved(&mut self, at: Position) {
        self.pointer = Some(at);
    }

    pub(crate) fn dismiss(&mut self) {
        self.pointer = None;
        self.armed = None;
    }

    /// Owed one frame when the dwell runs out, so the box appears on the clock
    /// rather than on the next unrelated event.
    pub(crate) fn cadence(&self, now: Instant) -> Cadence {
        self.armed
            .as_ref()
            .and_then(|armed| TOOLTIP_DELAY.checked_sub(now.duration_since(armed.since)))
            .filter(|left| !left.is_zero())
            .map_or(Cadence::IDLE, Cadence::due)
    }

    /// Takes this frame's candidate, and draws the box once the pointer has
    /// rested on it long enough. Returns where it drew.
    pub(crate) fn show(&mut self, frame: &mut Frame, tip: Option<Tip>, now: Instant) -> Rect {
        let Some((anchor, text)) = self.settle(tip, now) else {
            return Rect::default();
        };
        let Some(placed) = place(anchor, frame.area(), &text) else {
            return Rect::default();
        };
        grab_scope!("tooltip", placed.area);
        draw(frame.buffer_mut(), &placed);
        placed.area
    }

    fn settle(&mut self, tip: Option<Tip>, now: Instant) -> Option<(Rect, String)> {
        let (true, Some(pointer), Some(tip)) = (self.enabled, self.pointer, tip) else {
            self.armed = None;
            return None;
        };
        let armed = match self.armed.take() {
            Some(armed) if armed.key == tip.key => armed,
            _ => Armed {
                key: tip.key,
                origin: pointer,
                since: now,
            },
        };
        let due = now.duration_since(armed.since) >= TOOLTIP_DELAY;
        let anchor = match tip.anchor {
            Anchor::Area(area) => area,
            Anchor::Pointer => Rect::new(armed.origin.x, armed.origin.y, 1, 1),
        };
        self.armed = Some(armed);
        due.then_some((anchor, tip.text))
    }

    /// Ends the dwell at once, for a test that cannot wait it out.
    #[cfg(test)]
    pub(crate) fn expire(&mut self) {
        if let Some(armed) = &mut self.armed {
            armed.since = armed
                .since
                .checked_sub(TOOLTIP_DELAY)
                .unwrap_or(armed.since);
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Placed {
    pub area: Rect,
    pub lines: Vec<String>,
}

/// Where a box for `text` goes beside `anchor`, kept whole inside `frame`.
///
/// It opens below an anchor in the top half and above one in the bottom half,
/// so footer chips and the composer look up into the transcript. When that
/// side is short it takes the other, and when both are it takes the roomier
/// one and cuts the text to fit. It never covers the anchor's own rows, so the
/// control and the pointer on it stay in sight. It starts under the anchor's
/// left edge and slides left as far as the right edge demands.
pub(crate) fn place(anchor: Rect, frame: Rect, text: &str) -> Option<Placed> {
    let anchor = anchor.intersection(frame);
    if anchor.is_empty() {
        return None;
    }
    let room = frame.width.saturating_sub(CHROME).min(MAX_TEXT_WIDTH);
    if room < MIN_TEXT_WIDTH {
        return None;
    }
    let mut lines = wrap(text, usize::from(room), MAX_TEXT_LINES);
    if lines.is_empty() {
        return None;
    }
    let above = anchor.y - frame.y;
    let below = frame.bottom() - anchor.bottom();
    let wanted = lines.len().min(MAX_TEXT_LINES) as u16 + CHROME;
    let prefer_above = above >= frame.height / 2;
    let (near, far) = match prefer_above {
        true => (above, below),
        false => (below, above),
    };
    let go_above = match (near >= wanted, far >= wanted) {
        (true, _) => prefer_above,
        (false, true) => !prefer_above,
        (false, false) if near >= far => prefer_above,
        (false, false) => !prefer_above,
    };
    let space = if go_above { above } else { below };
    let rows = usize::from(space.saturating_sub(CHROME)).min(MAX_TEXT_LINES);
    if rows == 0 {
        return None;
    }
    if lines.len() > rows {
        lines.truncate(rows);
        if let Some(last) = lines.last_mut() {
            ellipsize(last, usize::from(room));
        }
    }
    let text_width = lines.iter().map(|line| line.width()).max().unwrap_or(0);
    let width = u16::try_from(text_width).ok()?.max(1) + CHROME;
    let height = lines.len() as u16 + CHROME;
    let y = match go_above {
        true => anchor.y - height,
        false => anchor.bottom(),
    };
    let x = anchor.x.min(frame.right() - width);
    Some(Placed {
        area: Rect::new(x, y, width, height),
        lines,
    })
}

fn draw(buf: &mut Buffer, placed: &Placed) {
    blank_split_glyphs(buf, placed.area);
    Clear.render(placed.area, buf);
    let t = theme::current();
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(t.panel_border)
        .style(t.surface_style());
    let lines: Vec<Line> = placed
        .lines
        .iter()
        .map(|line| Line::raw(line.as_str()))
        .collect();
    Paragraph::new(lines).block(block).render(placed.area, buf);
}

/// A double-width glyph just left of the box would have its right half under
/// the border. The terminal then erases the whole glyph while the buffer still
/// believes it is drawn, and the two disagree until something repaints it. A
/// blank keeps them in step.
fn blank_split_glyphs(buf: &mut Buffer, area: Rect) {
    let Some(left) = area.x.checked_sub(1) else {
        return;
    };
    for y in area.top()..area.bottom() {
        if let Some(cell) = buf.cell_mut((left, y))
            && cell.symbol().width() > 1
        {
            cell.set_symbol(" ");
        }
    }
}

/// Word-wraps `text` to `width` columns, breaking words longer than a line.
/// Stops once it has more than `limit` lines, which is enough to know it was
/// cut, so a pasted megabyte costs no more than a sentence.
fn wrap(text: &str, width: usize, limit: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph in text.lines() {
        if lines.len() > limit {
            break;
        }
        let mut line = String::new();
        let mut used = 0;
        // Just past the last space with words before it, and the width there.
        let mut soft: Option<(usize, usize)> = None;
        for character in paragraph.chars() {
            let (character, repeat) = match character {
                '\t' => (' ', TAB_WIDTH),
                other if other.is_control() => continue,
                other => (other, 1),
            };
            let columns = character.width().unwrap_or(0);
            for _ in 0..repeat {
                if used + columns > width {
                    if lines.len() > limit {
                        return lines;
                    }
                    match soft.take() {
                        Some((at, at_used)) if character != ' ' => {
                            let tail = line.split_off(at);
                            lines.push(line.trim_end().to_owned());
                            line = tail;
                            used -= at_used;
                        }
                        _ => {
                            lines.push(line.trim_end().to_owned());
                            line.clear();
                            used = 0;
                        }
                    }
                    if character == ' ' {
                        continue;
                    }
                }
                if character == ' ' && !line.trim().is_empty() {
                    soft = Some((line.len() + 1, used + columns));
                }
                line.push(character);
                used += columns;
            }
        }
        lines.push(line.trim_end().to_owned());
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

/// The head of a long text, for a preview. Cut by characters before anything
/// else touches it, so hovering a pasted megabyte copies a paragraph per frame.
pub(crate) fn preview(text: &str) -> String {
    match text.char_indices().nth(PREVIEW_CHARACTERS) {
        Some((end, _)) => format!("{}{ELLIPSIS}", text[..end].trim_end()),
        None => text.to_owned(),
    }
}

/// Ends `line` with an ellipsis inside `width` columns, for the last line kept
/// of a text that had more.
fn ellipsize(line: &mut String, width: usize) {
    while line.width() + 1 > width {
        line.pop();
    }
    line.truncate(line.trim_end().len());
    line.push(ELLIPSIS);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use test_case::test_case;

    const FRAME: Rect = Rect::new(0, 0, 80, 24);
    const SHORT: &str = "Click to switch model";
    const LONG: &str = "one two three four five six seven eight nine ten eleven twelve thirteen \
        fourteen fifteen sixteen seventeen eighteen nineteen twenty twenty-one twenty-two \
        twenty-three twenty-four twenty-five twenty-six twenty-seven twenty-eight twenty-nine \
        thirty thirty-one thirty-two thirty-three thirty-four thirty-five thirty-six";
    const OUTSIDE: &str = "the box left the frame";
    const OVER_ANCHOR: &str = "the box covers the control it explains";
    const WRONG_SIDE: &str = "the box opened on the wrong side";
    const NOT_CUT: &str = "a cut text must end with an ellipsis";
    const SPLIT_GLYPH: &str = "a wide glyph cut by the border must be blanked";
    const DWELL: &str = "the dwell is wrong";

    fn placed(anchor: Rect, frame: Rect, text: &str) -> Placed {
        place(anchor, frame, text).expect("a box fits")
    }

    fn assert_sound(anchor: Rect, frame: Rect, placed: &Placed) {
        assert_eq!(placed.area.intersection(frame), placed.area, "{OUTSIDE}");
        let rows = anchor.intersection(frame);
        assert!(
            placed.area.bottom() <= rows.top() || placed.area.top() >= rows.bottom(),
            "{OVER_ANCHOR}"
        );
    }

    #[test_case(Rect::new(0, 0, 4, 1), false; "top_left")]
    #[test_case(Rect::new(76, 0, 4, 1), false; "top_right")]
    #[test_case(Rect::new(0, 23, 4, 1), true; "bottom_left")]
    #[test_case(Rect::new(76, 23, 4, 1), true; "bottom_right")]
    #[test_case(Rect::new(40, 11, 4, 1), false; "middle_upper")]
    #[test_case(Rect::new(40, 12, 4, 1), true; "middle_lower")]
    #[test_case(Rect::new(79, 10, 1, 1), false; "last_column")]
    #[test_case(Rect::new(10, 22, 30, 2), true; "two_row_anchor_at_the_foot")]
    fn the_box_stays_inside_the_frame_and_off_the_anchor(anchor: Rect, above: bool) {
        for text in [SHORT, LONG] {
            let placed = placed(anchor, FRAME, text);
            assert_sound(anchor, FRAME, &placed);
            assert_eq!(placed.area.bottom() <= anchor.top(), above, "{WRONG_SIDE}");
        }
    }

    #[test]
    fn a_right_edge_anchor_slides_the_box_flush_with_the_border() {
        let anchor = Rect::new(75, 23, 5, 1);
        let placed = placed(anchor, FRAME, SHORT);
        assert_eq!(placed.area.right(), FRAME.right());
    }

    #[test]
    fn a_left_edge_anchor_starts_the_box_under_it() {
        let anchor = Rect::new(3, 0, 5, 1);
        assert_eq!(placed(anchor, FRAME, SHORT).area.x, anchor.x);
    }

    #[test]
    fn an_offset_frame_is_respected_on_every_side() {
        let frame = Rect::new(10, 5, 30, 10);
        for anchor in [
            Rect::new(10, 5, 2, 1),
            Rect::new(38, 5, 2, 1),
            Rect::new(10, 14, 2, 1),
            Rect::new(38, 14, 2, 1),
        ] {
            let placed = placed(anchor, frame, LONG);
            assert_sound(anchor, frame, &placed);
        }
    }

    #[test]
    fn a_frame_narrower_than_the_text_wraps_within_it() {
        let frame = Rect::new(0, 0, 20, 24);
        let placed = placed(Rect::new(18, 0, 2, 1), frame, LONG);
        assert_sound(Rect::new(18, 0, 2, 1), frame, &placed);
        assert!(placed.lines.iter().all(|line| line.width() <= 18));
    }

    #[test_case(Rect::new(0, 0, 9, 24); "too_narrow")]
    #[test_case(Rect::new(0, 0, 80, 3); "too_short_either_side")]
    fn a_frame_with_no_room_draws_nothing(frame: Rect) {
        assert_eq!(place(Rect::new(0, 1, 2, 1), frame, SHORT), None);
    }

    #[test_case(Rect::new(100, 3, 4, 1); "beyond_the_right")]
    #[test_case(Rect::new(3, 30, 4, 1); "below_the_foot")]
    #[test_case(Rect::new(3, 3, 0, 0); "empty")]
    fn an_anchor_off_screen_draws_nothing(anchor: Rect) {
        assert_eq!(place(anchor, FRAME, SHORT), None);
    }

    #[test]
    fn an_anchor_partly_off_screen_is_clipped_first() {
        let anchor = Rect::new(70, 23, 20, 3);
        let placed = placed(anchor, FRAME, SHORT);
        assert_sound(anchor, FRAME, &placed);
    }

    #[test]
    fn when_neither_side_fits_the_roomier_one_takes_a_cut_text() {
        let frame = Rect::new(0, 0, 80, 7);
        let anchor = Rect::new(0, 4, 4, 1);
        let placed = placed(anchor, frame, LONG);
        assert_sound(anchor, frame, &placed);
        assert_eq!(placed.area, Rect::new(0, 0, placed.area.width, 4));
        assert!(
            placed.lines.last().unwrap().ends_with(ELLIPSIS),
            "{NOT_CUT}"
        );
    }

    #[test]
    fn a_text_past_the_line_limit_ends_with_an_ellipsis() {
        let placed = placed(Rect::new(0, 0, 4, 1), FRAME, &LONG.repeat(4));
        assert_eq!(placed.lines.len(), MAX_TEXT_LINES);
        assert!(
            placed.lines.last().unwrap().ends_with(ELLIPSIS),
            "{NOT_CUT}"
        );
        assert!(placed.lines.iter().all(|line| line.width() <= 48));
    }

    #[test_case("a b c", 10, &["a b c"]; "fits")]
    #[test_case("alpha beta gamma", 10, &["alpha beta", "gamma"]; "breaks_at_a_space")]
    #[test_case("abcdefghijkl", 5, &["abcde", "fghij", "kl"]; "breaks_a_long_word")]
    #[test_case("  indented line", 20, &["  indented line"]; "keeps_indentation")]
    #[test_case("a\tb", 10, &["a    b"]; "expands_tabs")]
    #[test_case("a\u{1b}[31mb", 10, &["a[31mb"]; "drops_controls")]
    #[test_case("one\n\ntwo\n\n", 10, &["one", "", "two"]; "keeps_inner_blank_lines")]
    #[test_case("中文中文中文", 5, &["中文", "中文", "中文"]; "wide_glyphs_never_straddle")]
    fn wrapping(text: &str, width: usize, expected: &[&str]) {
        assert_eq!(wrap(text, width, MAX_TEXT_LINES), expected);
    }

    #[test]
    fn wrapping_stops_once_it_knows_the_text_was_cut() {
        let huge = "word ".repeat(1_000_000);
        assert_eq!(wrap(&huge, 10, 2).len(), 3);
    }

    #[test]
    fn a_wide_glyph_under_the_left_border_is_blanked() {
        let mut terminal = Terminal::new(TestBackend::new(20, 6)).unwrap();
        terminal
            .draw(|frame| {
                frame
                    .buffer_mut()
                    .set_string(4, 0, "中中中中", ratatui::style::Style::new());
                let placed = Placed {
                    area: Rect::new(5, 0, 10, 3),
                    lines: vec!["hi".into()],
                };
                draw(frame.buffer_mut(), &placed);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(4, 0)].symbol(), " ", "{SPLIT_GLYPH}");
    }

    fn tip(key: TipKey) -> Option<Tip> {
        Some(Tip {
            key,
            anchor: Anchor::Pointer,
            text: SHORT.into(),
        })
    }

    #[test]
    fn the_box_waits_for_the_dwell_and_keeps_it_within_a_target() {
        let start = Instant::now();
        let mut tooltip = Tooltip::new(true);
        tooltip.pointer_moved(Position::new(3, 4));
        let key = TipKey::MessageAction(0);
        assert_eq!(tooltip.settle(tip(key), start), None, "{DWELL}");
        assert_eq!(
            tooltip.cadence(start + Duration::from_millis(200)),
            Cadence::due(TOOLTIP_DELAY - Duration::from_millis(200))
        );
        tooltip.pointer_moved(Position::new(9, 9));
        let shown = tooltip.settle(tip(key), start + TOOLTIP_DELAY);
        assert_eq!(
            shown,
            Some((Rect::new(3, 4, 1, 1), SHORT.into())),
            "{DWELL}"
        );
        assert_eq!(tooltip.cadence(start + TOOLTIP_DELAY), Cadence::IDLE);
    }

    #[test]
    fn a_new_target_restarts_the_dwell() {
        let start = Instant::now();
        let mut tooltip = Tooltip::new(true);
        tooltip.pointer_moved(Position::new(0, 0));
        tooltip.settle(tip(TipKey::MessageAction(0)), start);
        let later = start + TOOLTIP_DELAY;
        assert_eq!(
            tooltip.settle(tip(TipKey::MessageAction(1)), later),
            None,
            "{DWELL}"
        );
    }

    #[test_case(true, false; "dismissed")]
    #[test_case(false, true; "disabled")]
    fn nothing_shows_while_dismissed_or_disabled(dismiss: bool, enabled_off: bool) {
        let start = Instant::now();
        let mut tooltip = Tooltip::new(!enabled_off);
        tooltip.pointer_moved(Position::new(0, 0));
        tooltip.settle(tip(TipKey::MessageAction(0)), start);
        if dismiss {
            tooltip.dismiss();
        }
        let later = start + TOOLTIP_DELAY;
        assert_eq!(tooltip.settle(tip(TipKey::MessageAction(0)), later), None);
        assert_eq!(tooltip.cadence(later), Cadence::IDLE);
    }
}
