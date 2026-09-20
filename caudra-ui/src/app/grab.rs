//! Point at a cell, get the code behind it.
//!
//! A floating button in the top-right corner arms the grab. While armed the
//! component under the pointer is outlined and named, scrolling walks up and
//! down its ancestors, and a click copies the component stack, the rendered
//! text, and the state that produced it.
//!
//! The whole module is compiled out of release builds, along with the
//! [`GrabState`] field on `App` and every `grab_scope!` that feeds it.
//!
//! The interface paints after `caudra_grab::end_frame`, so it never appears in
//! its own hit test, and the payload is assembled inside the following draw,
//! where the buffer holding the rendered text still exists. That is the same
//! deferral `SelectionState::PendingCopy` uses.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use unicode_width::UnicodeWidthStr;

use caudra_grab::Node;

use super::App;
use crate::components::{Action, escape_terminal_controls};
use crate::selection::SelectionZone;
use crate::theme;

const BADGE_IDLE: &str = "\u{25C9}";
const BADGE_HOVER: &str = "\u{25C9} grab";
const BADGE_ARMED: &str = "\u{25C9} grabbing";
const CONTENT_PREFIX: &str = "  | ";
const NO_MESSAGE: &str = "none";

#[derive(Default)]
pub(super) struct GrabState {
    armed: bool,
    /// Where the pointer last was. Doubles as the badge hover test while idle
    /// and as the hit-test point while armed.
    pointer: Option<Position>,
    /// Zero-sized until the badge has been drawn once, so a click cannot land
    /// on a button that is not on screen yet.
    badge: Rect,
    /// How far up the stack from the innermost component the grab is aimed.
    ancestor: u16,
    pending: bool,
}

impl GrabState {
    fn badge_hovered(&self) -> bool {
        self.pointer.is_some_and(|at| self.badge.contains(at))
    }

    fn label(&self) -> &'static str {
        match (self.armed, self.badge_hovered()) {
            (true, _) => BADGE_ARMED,
            (false, true) => BADGE_HOVER,
            (false, false) => BADGE_IDLE,
        }
    }
}

impl App {
    /// Runs ahead of every other mouse handler, because a floating dev
    /// affordance sits above the whole interface. Returns `None` when the
    /// event is none of its business, which is everything but the badge while
    /// it is idle.
    pub(super) fn handle_grab_mouse(&mut self, event: MouseEvent) -> Option<Vec<Action>> {
        let at = Position::new(event.column, event.row);
        if matches!(
            event.kind,
            MouseEventKind::Moved | MouseEventKind::Drag(_) | MouseEventKind::Down(_)
        ) {
            self.grab.pointer = Some(at);
        }
        if event.kind == MouseEventKind::Down(MouseButton::Left) && self.grab.badge.contains(at) {
            self.grab.armed = !self.grab.armed;
            self.grab.ancestor = 0;
            return Some(Vec::new());
        }
        if !self.grab.armed {
            return None;
        }
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => self.grab.pending = true,
            MouseEventKind::ScrollUp => {
                self.grab.ancestor = self.grab.ancestor.saturating_add(1);
            }
            MouseEventKind::ScrollDown => {
                self.grab.ancestor = self.grab.ancestor.saturating_sub(1);
            }
            _ => {}
        }
        Some(Vec::new())
    }

    /// Disarms on `Esc`. Reports whether it consumed the key, so the same press
    /// cannot also close whatever is open behind the grab.
    pub(super) fn handle_grab_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        let armed = self.grab.armed && key.code == crossterm::event::KeyCode::Esc;
        if armed {
            self.grab.armed = false;
            self.grab.ancestor = 0;
        }
        armed
    }

    pub(super) fn render_grab(&mut self, frame: &mut Frame) {
        let style = theme::current().accent;
        if let Some((target, stack)) = self.grab_target() {
            outline(frame.buffer_mut(), target.area, self.grab.armed);
            draw_label(frame, &target, style);
            // Grabbing is the point of arming, so the mode ends with it rather
            // than leaving the pointer captured over an interface the next
            // click was meant for.
            if std::mem::take(&mut self.grab.pending) {
                let payload = self.grab_payload(frame.buffer_mut(), &target, &stack);
                self.copy_to_clipboard(&payload);
                self.grab.armed = false;
                self.grab.ancestor = 0;
            }
        }
        self.grab.pending = false;
        self.grab.badge = draw_badge(frame, self.grab.label(), style);
    }

    /// The aimed component and the chain it sits in, innermost first. `None`
    /// while idle, or when the pointer covers nothing instrumented.
    fn grab_target(&self) -> Option<(Node, Vec<Node>)> {
        if !self.grab.armed {
            return None;
        }
        let stack = caudra_grab::stack_at(self.grab.pointer?);
        let index = usize::from(self.grab.ancestor).min(stack.len().checked_sub(1)?);
        Some((stack[index], stack))
    }

    fn grab_payload(&self, buf: &Buffer, target: &Node, stack: &[Node]) -> String {
        let area = target.area;
        let screen = buf.area();
        let mut out = format!(
            "[caudra grab] {}x{} at ({},{}) of {}x{}\n\nstack:\n",
            area.width, area.height, area.x, area.y, screen.width, screen.height
        );
        let width = stack
            .iter()
            .map(|node| node.name.len())
            .max()
            .unwrap_or_default();
        for node in stack.iter().rev() {
            let aim = if node == target { '>' } else { ' ' };
            out.push_str(&format!(
                "{aim} {:width$}  {}:{}\n",
                node.name, node.file, node.line
            ));
        }
        out.push_str(&format!(
            "\nmessage:\n  {}\n\nstate:\n  mode={:?} focus={:?} theme={} chat={}\n\ncontent:\n",
            self.grab_message().as_deref().unwrap_or(NO_MESSAGE),
            self.state.mode,
            self.key_focus,
            theme::current_theme_name(),
            self.active_chat,
        ));
        for line in rect_text(buf, area).lines() {
            out.push_str(CONTENT_PREFIX);
            out.push_str(&escape_terminal_controls(line));
            out.push('\n');
        }
        out
    }

    /// What produced the transcript row under the pointer. The component stack
    /// names the code that drew a cell; this names the data it drew, which the
    /// stack alone cannot say.
    fn grab_message(&self) -> Option<String> {
        let at = self.grab.pointer?;
        self.zone_at(at.y, at.x)
            .filter(|zone| zone.zone == SelectionZone::Messages)?;
        self.chats[self.active_chat].grab_provenance_at(at.y, self.msg_area())
    }
}

/// Reverses the target's perimeter rather than dimming the rest of the screen:
/// cheap, and legible against every theme. A rect one or two rows tall is all
/// perimeter.
fn outline(buf: &mut Buffer, area: Rect, armed: bool) {
    if !armed {
        return;
    }
    let area = area.intersection(*buf.area());
    for y in area.top()..area.bottom() {
        let edge_row = y == area.top() || y + 1 == area.bottom();
        for x in area.left()..area.right() {
            if !edge_row && x != area.left() && x + 1 != area.right() {
                continue;
            }
            let cell = &mut buf[(x, y)];
            cell.set_style(cell.style().add_modifier(Modifier::REVERSED));
        }
    }
}

/// Names the target on the row above it, or below when it is already at the
/// top, so the label never falls off the screen or hides what it describes.
fn draw_label(frame: &mut Frame, target: &Node, style: Style) {
    let screen = frame.area();
    let text = format!(" {} {}:{} ", target.name, target.file, target.line);
    let width = u16::try_from(text.width())
        .unwrap_or(u16::MAX)
        .min(screen.width);
    if width == 0 {
        return;
    }
    let y = match target.area.top().checked_sub(1) {
        Some(above) => above,
        None => target.area.bottom().min(screen.bottom().saturating_sub(1)),
    };
    let x = target.area.x.min(screen.right().saturating_sub(width));
    frame.render_widget(
        Line::from(text).style(style.add_modifier(Modifier::REVERSED)),
        Rect::new(x, y, width, 1),
    );
}

fn draw_badge(frame: &mut Frame, label: &'static str, style: Style) -> Rect {
    let screen = frame.area();
    let width = u16::try_from(label.width())
        .unwrap_or(u16::MAX)
        .min(screen.width);
    if width == 0 || screen.height == 0 {
        return Rect::ZERO;
    }
    let area = Rect::new(screen.right() - width, screen.y, width, 1);
    frame.render_widget(Line::from(label).style(style), area);
    area
}

/// Wide characters (CJK, emoji) leave their continuation cells holding a space,
/// so those have to be skipped or every one of them gains a space it was never
/// drawn with. A rect starting mid-character skips that character's remainder.
fn rect_text(buf: &Buffer, area: Rect) -> String {
    let area = area.intersection(*buf.area());
    let mut out = String::new();
    for y in area.top()..area.bottom() {
        let mut row = String::new();
        let mut skip = area
            .left()
            .checked_sub(1)
            .map(|previous| buf[(previous, y)].symbol().width().saturating_sub(1))
            .unwrap_or_default();
        for x in area.left()..area.right() {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            let symbol = buf[(x, y)].symbol();
            skip = symbol.width().saturating_sub(1);
            row.push_str(symbol);
        }
        out.push_str(row.trim_end());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{mouse_event, test_app};
    use crossterm::event::KeyModifiers;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    const WIDE: &str = "\u{4e16}\u{754c}";
    const SCREEN: (u16, u16) = (80, 24);
    const ROOT_SCOPE: &str = "caudra-ui/src/app/view.rs";
    const TRANSCRIPT_MESSAGES: usize = 6;

    fn armed_app() -> (App, ratatui::Terminal<TestBackend>) {
        let mut app = test_app();
        let mut terminal =
            ratatui::Terminal::new(TestBackend::new(SCREEN.0, SCREEN.1)).expect("terminal");
        terminal.draw(|frame| app.view(frame)).expect("first draw");
        let badge = app.grab.badge;
        app.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            badge.x,
            badge.y,
        ));
        (app, terminal)
    }

    fn press(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn clicking_the_badge_arms_and_clicking_it_again_disarms() {
        let (mut app, _terminal) = armed_app();
        assert!(app.grab.armed);
        let badge = app.grab.badge;
        app.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            badge.x,
            badge.y,
        ));
        assert!(!app.grab.armed);
    }

    /// The guard that matters: an idle badge must not swallow clicks meant for
    /// the interface it floats over.
    #[test]
    fn an_idle_click_away_from_the_badge_falls_through() {
        let mut app = test_app();
        let mut terminal =
            ratatui::Terminal::new(TestBackend::new(SCREEN.0, SCREEN.1)).expect("terminal");
        terminal.draw(|frame| app.view(frame)).expect("draw");
        assert!(app.handle_grab_mouse(press(4, 4)).is_none());
    }

    #[test]
    fn an_armed_pointer_resolves_a_stack_and_builds_a_payload() {
        let (mut app, mut terminal) = armed_app();
        app.update(mouse_event(MouseEventKind::Moved, 10, 10));
        terminal.draw(|frame| app.view(frame)).expect("armed draw");

        let (target, stack) = app.grab_target().expect("a stack under the pointer");
        let payload = app.grab_payload(terminal.backend().buffer(), &target, &stack);
        assert!(payload.contains(ROOT_SCOPE), "{payload}");
        assert!(payload.contains("stack:"), "{payload}");
        assert!(payload.contains("content:"), "{payload}");
    }

    /// The rule every instrumented component has to honour: a scope opened
    /// before a "not drawing" guard would register a node covering the whole
    /// region while painting nothing, and being drawn last it would win every
    /// hit test. Closed modals must therefore be absent from the stack.
    #[test]
    fn components_that_are_not_drawing_stay_out_of_the_stack() {
        let (mut app, mut terminal) = armed_app();
        app.update(mouse_event(MouseEventKind::Moved, 40, 8));
        terminal.draw(|frame| app.view(frame)).expect("armed draw");

        let (_, stack) = app.grab_target().expect("a stack under the pointer");
        let names: Vec<&str> = stack.iter().map(|node| node.name).collect();
        assert!(names.contains(&"messages"), "{names:?}");
        assert!(
            !names.iter().any(|name| name.ends_with("_modal")),
            "a closed modal registered a node: {names:?}"
        );
    }

    /// Aiming at the transcript has to land on the message under the pointer,
    /// not on the panel that holds every message.
    #[test]
    fn a_transcript_grab_resolves_one_message_rather_than_the_whole_panel() {
        let (mut app, mut terminal) = armed_app();
        for index in 0..TRANSCRIPT_MESSAGES {
            app.main_chat().push_user_message(format!("message {index}"));
        }
        terminal.draw(|frame| app.view(frame)).expect("filled draw");
        let panel = app.msg_area();
        app.update(mouse_event(
            MouseEventKind::Moved,
            panel.x + 2,
            panel.y + panel.height / 2,
        ));
        terminal.draw(|frame| app.view(frame)).expect("armed draw");

        let (target, stack) = app.grab_target().expect("a stack under the pointer");
        let names: Vec<&str> = stack.iter().map(|node| node.name).collect();
        assert!(target.name.starts_with("transcript_"), "{names:?}");
        assert!(
            target.area.height < panel.height,
            "grabbed {}x{} of a {}x{} panel",
            target.area.width,
            target.area.height,
            panel.width,
            panel.height
        );
        assert!(names.contains(&"messages"), "{names:?}");
    }

    /// Arming exists to take one grab, so the grab ends it. Leaving it armed
    /// captures the click the user meant for the interface underneath.
    #[test]
    fn a_grab_disarms_itself() {
        let (mut app, mut terminal) = armed_app();
        app.update(mouse_event(MouseEventKind::Moved, 10, 10));
        terminal.draw(|frame| app.view(frame)).expect("armed draw");
        app.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            10,
            10,
        ));
        assert!(app.grab.armed, "the click only queues the grab");
        terminal.draw(|frame| app.view(frame)).expect("grab draw");
        assert!(!app.grab.armed, "the grab left the mode armed");
    }

    /// Scrolling walks outward, and stops at the root rather than wrapping
    /// back to the leaf.
    #[test]
    fn scrolling_clamps_the_aim_to_the_outermost_component() {
        let (mut app, mut terminal) = armed_app();
        app.update(mouse_event(MouseEventKind::Moved, 10, 10));
        terminal.draw(|frame| app.view(frame)).expect("armed draw");
        for _ in 0..8 {
            app.update(mouse_event(MouseEventKind::ScrollUp, 10, 10));
        }
        let (target, stack) = app.grab_target().expect("a stack under the pointer");
        assert_eq!(&target, stack.last().expect("a root"));
    }

    #[test]
    fn escape_disarms_without_reaching_the_interface_behind_it() {
        let (mut app, _terminal) = armed_app();
        let escape = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Esc,
            KeyModifiers::NONE,
        );
        assert!(app.handle_grab_key(escape));
        assert!(!app.grab.armed);
        assert!(!app.handle_grab_key(escape));
    }

    #[test]
    fn rect_text_reads_rows_back_without_trailing_padding() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 8, 2));
        buf.set_string(0, 0, "hi", Style::default());
        buf.set_string(0, 1, WIDE, Style::default());
        assert_eq!(rect_text(&buf, Rect::new(0, 0, 8, 2)), format!("hi\n{WIDE}\n"));
    }

    #[test]
    fn rect_text_skips_the_remainder_of_a_character_the_rect_starts_inside() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 8, 1));
        buf.set_string(0, 0, WIDE, Style::default());
        assert_eq!(rect_text(&buf, Rect::new(1, 0, 7, 1)), "\u{754c}\n");
    }

    #[test]
    fn rect_text_clamps_to_the_buffer() {
        let buf = Buffer::empty(Rect::new(0, 0, 2, 1));
        assert_eq!(rect_text(&buf, Rect::new(0, 0, 40, 40)), "\n");
    }

    #[test]
    fn outline_reverses_the_perimeter_and_spares_the_interior() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 3));
        outline(&mut buf, Rect::new(0, 0, 4, 3), true);
        assert!(buf[(0, 1)].style().add_modifier.contains(Modifier::REVERSED));
        assert!(buf[(3, 1)].style().add_modifier.contains(Modifier::REVERSED));
        assert!(!buf[(1, 1)].style().add_modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn badge_label_follows_hover_and_armed_state() {
        let mut state = GrabState {
            badge: Rect::new(10, 0, 6, 1),
            ..GrabState::default()
        };
        assert_eq!(state.label(), BADGE_IDLE);
        state.pointer = Some(Position::new(11, 0));
        assert_eq!(state.label(), BADGE_HOVER);
        state.armed = true;
        assert_eq!(state.label(), BADGE_ARMED);
    }
}
