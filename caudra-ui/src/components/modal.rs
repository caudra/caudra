use std::borrow::Cow;

use crate::components::hover_style;
use crate::theme;

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear};

pub const CHROME_LINES: u16 = 2;
const FOOTER_HIT_ROWS: u16 = 1;
/// The narrowest a modal may be drawn, clamped to the terminal so the popup can
/// only ever grow towards the edge and never past it. A percentage alone spends
/// a small screen's columns on margin and then cuts the content it was making
/// room for: 70 columns is the widest token table `/usage` builds plus borders.
const MIN_WIDTH: u16 = 72;

pub struct Modal<'a> {
    pub title: &'a str,
    pub width_percent: u16,
    pub max_height_percent: u16,
}

impl Modal<'_> {
    pub fn render(&self, frame: &mut Frame, area: Rect, content_height: u16) -> (Rect, Rect) {
        let max_h = (area.height as u32 * self.max_height_percent as u32 / 100) as u16;
        let total_h = (content_height + CHROME_LINES)
            .min(max_h)
            .max(CHROME_LINES + 1);

        let [popup] = Layout::vertical([Constraint::Length(total_h)])
            .flex(Flex::Center)
            .areas(area);
        let [popup] = Layout::horizontal([Constraint::Length(Self::popup_width(
            area.width,
            self.width_percent,
        ))])
        .flex(Flex::Center)
        .areas(popup);

        frame.render_widget(Clear, popup);

        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme::current().panel_border)
            .title(self.title)
            .title_style(theme::current().panel_title)
            .style(theme::current().surface_style());

        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        (popup, inner)
    }

    /// The columns inside the border. A caller that has to lay its content out
    /// before it can name the height to render at reads the width from here, so
    /// what it wrapped to and the popup it lands in cannot disagree.
    pub fn inner_width(available: u16, width_percent: u16) -> u16 {
        Self::popup_width(available, width_percent).saturating_sub(CHROME_LINES)
    }

    /// The popup's width on a terminal `available` columns wide: its share, but
    /// never so narrow that [`MIN_WIDTH`] of content would not fit, and never
    /// wider than the terminal.
    fn popup_width(available: u16, width_percent: u16) -> u16 {
        let share = (available as u32 * width_percent as u32 / 100) as u16;
        share.max(MIN_WIDTH).min(available)
    }
}

/// A modal's centred footer: the spans that draw it, and which of them answer
/// the pointer. Drawing, hit testing and hover all read this one span list, so
/// a target cannot drift off the glyphs it claims to cover.
#[derive(Default)]
pub(crate) struct FooterLine {
    spans: Vec<Span<'static>>,
    targets: Vec<usize>,
}

impl FooterLine {
    /// A clickable token. Its index among the targets is what a click reports.
    pub(crate) fn command(&mut self, text: &'static str, style: Style) {
        self.targets.push(self.spans.len());
        self.spans.push(Span::styled(text, style));
    }

    /// Inert text: a description, a separator, a key hint. Never hit, never
    /// hovered, even when it sits inside a control's phrase.
    pub(crate) fn text(&mut self, text: impl Into<Cow<'static, str>>, style: Style) {
        self.spans.push(Span::styled(text, style));
    }

    pub(crate) fn line(&self, hovered: Option<usize>) -> Line<'static> {
        let on = hovered.and_then(|index| self.targets.get(index)).copied();
        Line::from(
            self.spans
                .iter()
                .enumerate()
                .map(|(index, span)| {
                    Span::styled(
                        span.content.clone(),
                        hover_style(span.style, on == Some(index)),
                    )
                })
                .collect::<Vec<_>>(),
        )
        .alignment(Alignment::Center)
    }

    /// Where each command landed on screen, given the paragraph's scroll
    /// `offset` and its `total` wrapped row count. The footer is the last line,
    /// so it is on the last row.
    pub(crate) fn hits(&self, area: Rect, offset: u16, total: u16) -> Vec<Rect> {
        let width = u16::try_from(self.width()).unwrap_or(u16::MAX);
        // A footer wider than its area wraps, and then a single-row hit rect
        // would claim cells the command was never drawn in.
        if area.width == 0 || width > area.width {
            return Vec::new();
        }
        let Some(row) = total.saturating_sub(1).checked_sub(offset) else {
            return Vec::new();
        };
        if row >= area.height {
            return Vec::new();
        }
        // Ratatui centres a line at `width / 2 - line / 2`, which is not
        // `(width - line) / 2`: for odd remainders the two disagree by a
        // column, and a hit rect a column off the glyphs is worse than none.
        let left = area
            .x
            .saturating_add((area.width / 2).saturating_sub(width / 2));
        let y = area.y.saturating_add(row);
        self.targets
            .iter()
            .map(|&target| {
                let start = self.width_before(target);
                Rect::new(
                    left.saturating_add(u16::try_from(start).unwrap_or(u16::MAX)),
                    y,
                    u16::try_from(self.spans[target].width()).unwrap_or(u16::MAX),
                    FOOTER_HIT_ROWS,
                )
            })
            .collect()
    }

    fn width(&self) -> usize {
        self.spans.iter().map(|span| span.width()).sum()
    }

    fn width_before(&self, index: usize) -> usize {
        self.spans[..index].iter().map(|span| span.width()).sum()
    }
}

/// Where a modal footer's controls landed, where the pointer is, and which
/// control a press is committed to.
#[derive(Default)]
pub(crate) struct FooterHits {
    hits: Vec<Rect>,
    pointer: Option<Position>,
    pressed: Option<usize>,
}

impl FooterHits {
    pub(crate) fn set(&mut self, hits: Vec<Rect>) {
        self.hits = hits;
    }

    pub(crate) fn hovered(&self) -> Option<usize> {
        self.pointer.and_then(|pointer| self.index_at(pointer))
    }

    /// Reports the command a release completed. Press and release must land on
    /// the same target, and a drag cancels.
    pub(crate) fn handle_mouse(&mut self, event: MouseEvent) -> Option<usize> {
        let position = Position::new(event.column, event.row);
        self.pointer = Some(position);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.pressed = self.index_at(position);
                None
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.pressed = None;
                None
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let pressed = self.pressed.take();
                pressed.filter(|&index| self.index_at(position) == Some(index))
            }
            _ => None,
        }
    }

    /// Drops the geometry of a footer that is about to be redrawn differently,
    /// keeping the pointer: a view switched by its own footer click leaves the
    /// pointer sitting on the control, and the hover has to survive it.
    pub(crate) fn clear(&mut self) {
        self.hits.clear();
        self.pressed = None;
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    fn index_at(&self, position: Position) -> Option<usize> {
        self.hits.iter().position(|hit| hit.contains(position))
    }

    #[cfg(test)]
    pub(crate) fn hit(&self, index: usize) -> Rect {
        self.hits.get(index).copied().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyModifiers;
    use test_case::test_case;
    use unicode_width::UnicodeWidthStr;

    use super::*;

    const FIRST: &str = "/context";
    const SECOND: &str = "/context all";
    const GAP: &str = " · ";
    const AREA: Rect = Rect::new(10, 4, 40, 6);
    const TOTAL: u16 = 6;
    const HALF: u16 = 50;
    const WRONG_WIDTH: &str = "the popup is not the width the terminal allows it";

    /// A percentage alone leaves a small terminal drawing a sliver and then
    /// cutting the content it made room for, and a large one is still handed its
    /// share rather than the whole screen.
    #[test_case(40, 40 ; "a terminal narrower than the floor is filled")]
    #[test_case(MIN_WIDTH, MIN_WIDTH ; "a terminal exactly at the floor is filled")]
    #[test_case(100, MIN_WIDTH ; "a share below the floor is raised to it")]
    #[test_case(200, 100 ; "a share above the floor is left alone")]
    fn a_modal_is_never_narrower_than_its_content_floor(available: u16, expected: u16) {
        assert_eq!(
            Modal::popup_width(available, HALF),
            expected,
            "{WRONG_WIDTH}"
        );
    }

    /// Callers that wrap their content before they can name its height read the
    /// width from here, so the two cannot disagree about where the border is.
    #[test]
    fn the_inner_width_is_the_popup_less_its_border() {
        assert_eq!(
            Modal::inner_width(200, HALF),
            Modal::popup_width(200, HALF) - CHROME_LINES,
            "{WRONG_WIDTH}"
        );
    }

    fn footer() -> FooterLine {
        let mut footer = FooterLine::default();
        footer.command(FIRST, Style::new());
        footer.text(GAP, Style::new());
        footer.command(SECOND, Style::new());
        footer
    }

    fn mouse(kind: MouseEventKind, at: Rect) -> MouseEvent {
        MouseEvent {
            kind,
            column: at.x,
            row: at.y,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn a_centred_footer_puts_its_hits_on_its_commands() {
        let footer = footer();
        let hits = footer.hits(AREA, 0, TOTAL);
        let width = (FIRST.width() + GAP.width() + SECOND.width()) as u16;
        let left = AREA.x + AREA.width / 2 - width / 2;

        assert_eq!(hits.len(), 2);
        assert_eq!(
            hits[0],
            Rect::new(left, AREA.y + TOTAL - 1, FIRST.width() as u16, 1)
        );
        assert_eq!(
            hits[1],
            Rect::new(
                left + (FIRST.width() + GAP.width()) as u16,
                AREA.y + TOTAL - 1,
                SECOND.width() as u16,
                1,
            )
        );
    }

    #[test]
    fn a_footer_wider_than_its_area_has_no_hits() {
        assert!(footer().hits(Rect::new(0, 0, 4, 6), 0, TOTAL).is_empty());
        assert!(footer().hits(Rect::new(0, 0, 0, 6), 0, TOTAL).is_empty());
    }

    #[test_case(TOTAL, TOTAL ; "scrolled_past")]
    #[test_case(0, u16::MAX ; "below_the_viewport")]
    fn a_footer_off_the_viewport_has_no_hits(offset: u16, total: u16) {
        assert!(footer().hits(AREA, offset, total).is_empty());
    }

    #[test]
    fn a_release_completes_only_the_command_it_was_pressed_on() {
        let mut state = FooterHits::default();
        state.set(footer().hits(AREA, 0, TOTAL));
        let first = state.hit(0);
        let second = state.hit(1);

        assert_eq!(
            state.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), first)),
            None
        );
        assert_eq!(
            state.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), second)),
            None
        );

        state.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), second));
        assert_eq!(
            state.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), second)),
            Some(1)
        );
    }

    #[test]
    fn a_drag_cancels_a_press() {
        let mut state = FooterHits::default();
        state.set(footer().hits(AREA, 0, TOTAL));
        let first = state.hit(0);

        state.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), first));
        state.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), first));
        assert_eq!(
            state.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), first)),
            None
        );
    }

    #[test]
    fn the_pointer_names_the_command_under_it() {
        let mut state = FooterHits::default();
        state.set(footer().hits(AREA, 0, TOTAL));
        let second = state.hit(1);

        assert_eq!(state.hovered(), None);
        state.handle_mouse(mouse(MouseEventKind::Moved, second));
        assert_eq!(state.hovered(), Some(1));

        state.clear();
        assert_eq!(state.hovered(), None, "cleared geometry cannot be hovered");
        state.set(footer().hits(AREA, 0, TOTAL));
        assert_eq!(
            state.hovered(),
            Some(1),
            "the pointer outlives a view switch"
        );

        state.reset();
        assert_eq!(state.hovered(), None);
    }
}
