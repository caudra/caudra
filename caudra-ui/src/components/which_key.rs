use crate::components::hover_style;
use crate::components::keybindings::{LEADER_PREFIX, LeaderChord};
use crate::components::modal::FooterHits;
use crate::repaint::Cadence;
use crate::theme;

use caudra_grab::grab_scope;
use crossterm::event::{KeyEvent, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

const TITLE_GAP: &str = " ";
const KEY_GAP: &str = "  ";
const COLUMN_GAP: usize = 3;
const CHROME_WIDTH: u16 = 2;
const CHROME_HEIGHT: u16 = 2;
const MAX_ROWS: usize = 8;

/// The chord list the leader puts up while it waits for a second key. Armed by
/// the prefix and torn down by whatever follows, a key or a click, so beyond
/// when it was armed it only holds where the last frame put each chord.
pub struct WhichKey {
    armed_at: Option<Instant>,
    delay: Duration,
    panel: Rect,
    hits: FooterHits,
    presses: Vec<KeyEvent>,
}

impl WhichKey {
    pub fn new(delay: Duration) -> Self {
        Self {
            armed_at: None,
            delay,
            panel: Rect::default(),
            hits: FooterHits::default(),
            presses: Vec::new(),
        }
    }

    pub fn arm(&mut self) {
        self.armed_at = Some(Instant::now());
    }

    pub fn disarm(&mut self) {
        *self = Self::new(self.delay);
    }

    pub fn is_armed(&self) -> bool {
        self.armed_at.is_some()
    }

    /// Owed a frame when the delay runs out, so the list appears on the clock
    /// rather than leaving the user staring at nothing until the next key.
    pub fn cadence(&self) -> Cadence {
        match self.armed_at {
            Some(at) if at.elapsed() < self.delay => Cadence::due(self.delay - at.elapsed()),
            _ => Cadence::IDLE,
        }
    }

    fn is_visible(&self) -> bool {
        self.armed_at.is_some_and(|at| at.elapsed() >= self.delay)
    }

    /// The pointer answers the way the next key does: a click on a chord is
    /// that chord's key, and a press anywhere off the panel backs out as `Esc`
    /// would. Either one disarms, so the caller only has to run what it gets.
    pub fn handle_mouse(&mut self, event: MouseEvent) -> Option<KeyEvent> {
        let at = Position::new(event.column, event.row);
        if matches!(event.kind, MouseEventKind::Down(_)) && !self.panel.contains(at) {
            self.disarm();
            return None;
        }
        let press = self
            .hits
            .handle_mouse(event)
            .and_then(|index| self.presses.get(index).copied());
        if press.is_some() {
            self.disarm();
        }
        press
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect, chords: &[LeaderChord]) -> Rect {
        if !self.is_visible() || chords.is_empty() {
            self.panel = Rect::default();
            self.hits.clear();
            self.presses.clear();
            return Rect::default();
        }
        let t = theme::current();

        let cell_w = chords.iter().map(cell_width).max().unwrap_or(0) + COLUMN_GAP;
        let usable = area.width.saturating_sub(CHROME_WIDTH) as usize;
        let columns = (usable / cell_w.max(1)).max(1);
        let rows = chords.len().div_ceil(columns).min(MAX_ROWS);

        let height = rows as u16 + CHROME_HEIGHT;
        let [panel] = Layout::vertical([Constraint::Length(height.min(area.height))])
            .flex(Flex::End)
            .areas(area);
        grab_scope!("which_key", panel);

        frame.render_widget(Clear, panel);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(t.panel_border)
            .title(format!("{TITLE_GAP}{LEADER_PREFIX}{TITLE_GAP}"))
            .title_style(t.panel_title)
            .style(t.surface_style());
        let inner = block.inner(panel);
        frame.render_widget(block, panel);

        self.hits.set(
            chords
                .iter()
                .enumerate()
                .map(|(index, chord)| cell_hit(inner, index, rows, cell_w, cell_width(chord)))
                .collect(),
        );
        let hovered = self.hits.hovered();

        let lines: Vec<Line> = (0..rows)
            .map(|row| {
                let mut spans = Vec::new();
                for (index, chord) in chords.iter().enumerate().skip(row).step_by(rows) {
                    let on = hovered == Some(index);
                    let pad = cell_w.saturating_sub(cell_width(chord));
                    spans.push(Span::styled(chord.key, hover_style(t.keybind_key, on)));
                    spans.push(Span::styled(KEY_GAP, hover_style(t.keybind_desc, on)));
                    spans.push(Span::styled(
                        chord.description,
                        hover_style(t.keybind_desc, on),
                    ));
                    spans.push(Span::styled(format!("{:pad$}", ""), t.keybind_desc));
                }
                Line::from(spans)
            })
            .collect();

        frame.render_widget(Paragraph::new(lines), inner);
        self.panel = panel;
        self.presses = chords.iter().map(|chord| chord.press).collect();
        panel
    }

    #[cfg(test)]
    pub(crate) fn hit(&self, press: KeyEvent) -> Rect {
        self.presses
            .iter()
            .position(|candidate| *candidate == press)
            .map_or_else(Rect::default, |index| self.hits.hit(index))
    }
}

fn cell_width(chord: &LeaderChord) -> usize {
    UnicodeWidthStr::width(chord.key)
        + UnicodeWidthStr::width(KEY_GAP)
        + UnicodeWidthStr::width(chord.description)
}

/// Where chord `index` lands, column by column. A chord the panel cut off, in a
/// column past its width or a row past its height, gets an empty rect that no
/// pointer lands on, so only what can be read can be clicked.
fn cell_hit(inner: Rect, index: usize, rows: usize, cell_w: usize, width: usize) -> Rect {
    let x = usize::from(inner.x) + index / rows * cell_w;
    let y = usize::from(inner.y) + index % rows;
    if x + width > usize::from(inner.right()) || y >= usize::from(inner.bottom()) {
        return Rect::default();
    }
    Rect::new(x as u16, y as u16, width as u16, 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::keybindings::{KeybindContext, leader_chords};
    use caudra_config::FeatureFlags;
    use crossterm::event::{KeyModifiers, MouseButton};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::{Buffer, Cell};
    use ratatui::style::Modifier;
    use test_case::test_case;

    const IMMEDIATE: Duration = Duration::ZERO;
    const LATER: Duration = Duration::from_secs(60);
    const WIDTH: u16 = 60;
    const HEIGHT: u16 = 12;
    const NOT_DUE: &str = "a panel still inside its delay must not paint";
    const NOT_ARMED: &str = "a disarmed panel must not paint";
    const MISSING_CHORD: &str = "an armed panel must list the chords it was handed";
    const HOVER_UNMARKED: &str = "the hovered chord must reverse its key and description";
    const HOVER_SPILLED: &str = "hover must mark one chord and never its padding";
    const HOVER_RESOLVED: &str = "hovering a chord must not resolve it";
    const CLICK_NOT_PRESSED: &str = "a click on a chord must press that chord's key";
    const RESOLVED_STILL_ARMED: &str = "a chord the pointer resolved must disarm the leader";
    const PRESS_ARMS_NOTHING: &str = "a press alone must wait for its release";
    const HIDDEN_CHORD_CLICKABLE: &str = "a chord the panel cut off must take no clicks";
    const OUTSIDE_KEPT: &str = "a press off the panel cancels the chord, and one on it does not";

    fn chords() -> Vec<LeaderChord> {
        leader_chords(&[KeybindContext::General], FeatureFlags::all())
    }

    fn armed(delay: Duration) -> WhichKey {
        let mut which_key = WhichKey::new(delay);
        which_key.arm();
        which_key
    }

    fn draw(which_key: &mut WhichKey, chords: &[LeaderChord]) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).expect("a test terminal");
        terminal
            .draw(|frame| {
                which_key.view(frame, frame.area(), chords);
            })
            .expect("a frame");
        terminal.backend().buffer().clone()
    }

    fn render(which_key: &mut WhichKey, chords: &[LeaderChord]) -> String {
        draw(which_key, chords)
            .content()
            .iter()
            .map(Cell::symbol)
            .collect()
    }

    fn mouse(kind: MouseEventKind, at: Position) -> MouseEvent {
        MouseEvent {
            kind,
            column: at.x,
            row: at.y,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn reversed(buffer: &Buffer, x: u16, y: u16) -> bool {
        buffer[(x, y)].modifier.contains(Modifier::REVERSED)
    }

    #[test]
    fn a_disarmed_panel_paints_nothing() {
        let mut which_key = WhichKey::new(IMMEDIATE);
        assert!(
            !render(&mut which_key, &chords()).contains("tasks"),
            "{NOT_ARMED}"
        );
    }

    #[test]
    fn a_delay_holds_the_panel_back() {
        let which_key = armed(LATER);
        assert!(!which_key.is_visible(), "{NOT_DUE}");
        assert!(which_key.is_armed(), "the chord is still pending");
    }

    #[test]
    fn an_armed_panel_lists_its_chords() {
        let mut which_key = armed(IMMEDIATE);
        let painted = render(&mut which_key, &chords());
        assert!(painted.contains("Open tasks"), "{MISSING_CHORD}");
    }

    #[test]
    fn hovering_a_chord_reverses_its_cell_alone() {
        let chords = chords();
        let mut which_key = armed(IMMEDIATE);
        draw(&mut which_key, &chords);
        let hit = which_key.hit(chords[0].press);
        let neighbour = which_key.hit(chords[1].press);

        which_key.handle_mouse(mouse(MouseEventKind::Moved, hit.as_position()));
        let buffer = draw(&mut which_key, &chords);

        assert!(
            (hit.left()..hit.right()).all(|x| reversed(&buffer, x, hit.y)),
            "{HOVER_UNMARKED}"
        );
        assert!(!reversed(&buffer, hit.right(), hit.y), "{HOVER_SPILLED}");
        assert!(
            (neighbour.left()..neighbour.right()).all(|x| !reversed(&buffer, x, neighbour.y)),
            "{HOVER_SPILLED}"
        );
        assert!(which_key.is_armed(), "{HOVER_RESOLVED}");
    }

    #[test]
    fn clicking_a_chord_presses_its_key_and_disarms() {
        let chords = chords();
        let chord = &chords[1];
        let mut which_key = armed(IMMEDIATE);
        draw(&mut which_key, &chords);
        let at = which_key.hit(chord.press).as_position();

        let pressed = which_key.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), at));
        assert_eq!(pressed, None, "{PRESS_ARMS_NOTHING}");
        assert!(which_key.is_armed(), "{PRESS_ARMS_NOTHING}");

        let released = which_key.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), at));
        assert_eq!(released, Some(chord.press), "{CLICK_NOT_PRESSED}");
        assert!(!which_key.is_armed(), "{RESOLVED_STILL_ARMED}");
    }

    #[test_case(|_| Position::ORIGIN, false ; "outside_cancels")]
    #[test_case(Rect::as_position, true ; "border_keeps_the_chord")]
    fn a_press_off_the_chords(at: fn(Rect) -> Position, stays_armed: bool) {
        let mut which_key = armed(IMMEDIATE);
        draw(&mut which_key, &chords());
        let panel = which_key.panel;
        assert!(!panel.contains(Position::ORIGIN), "{OUTSIDE_KEPT}");

        let pressed =
            which_key.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Right), at(panel)));

        assert_eq!(pressed, None, "{OUTSIDE_KEPT}");
        assert_eq!(which_key.is_armed(), stays_armed, "{OUTSIDE_KEPT}");
    }

    #[test]
    fn a_press_before_the_panel_shows_cancels_the_chord() {
        let mut which_key = armed(LATER);
        draw(&mut which_key, &chords());

        which_key.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            Position::new(WIDTH / 2, HEIGHT - 2),
        ));

        assert!(!which_key.is_armed(), "{OUTSIDE_KEPT}");
    }

    #[test]
    fn a_chord_the_panel_cuts_off_takes_no_clicks() {
        let chords = chords();
        let mut which_key = armed(IMMEDIATE);
        draw(&mut which_key, &chords);

        assert!(chords.len() > MAX_ROWS, "{HIDDEN_CHORD_CLICKABLE}");
        let hidden = chords.last().expect("a chord");
        assert_eq!(
            which_key.hit(hidden.press),
            Rect::default(),
            "{HIDDEN_CHORD_CLICKABLE}"
        );
    }
}
