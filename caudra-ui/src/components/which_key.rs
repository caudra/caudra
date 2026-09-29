use crate::components::keybindings::{LEADER_PREFIX, LeaderChord};
use crate::repaint::Cadence;
use crate::theme;

use caudra_grab::grab_scope;
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
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
/// the prefix and torn down by whatever follows, so it holds no state beyond
/// when it was armed.
pub struct WhichKey {
    armed_at: Option<Instant>,
    delay: Duration,
}

impl WhichKey {
    pub fn new(delay: Duration) -> Self {
        Self {
            armed_at: None,
            delay,
        }
    }

    pub fn arm(&mut self) {
        self.armed_at = Some(Instant::now());
    }

    pub fn disarm(&mut self) {
        self.armed_at = None;
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

    pub fn view(&self, frame: &mut Frame, area: Rect, chords: &[LeaderChord]) -> Rect {
        if !self.is_visible() || chords.is_empty() {
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

        let lines: Vec<Line> = (0..rows)
            .map(|row| {
                let mut spans = Vec::new();
                for chord in chords.iter().skip(row).step_by(rows) {
                    let pad = cell_w.saturating_sub(cell_width(chord));
                    spans.push(Span::styled(chord.key, t.keybind_key));
                    spans.push(Span::styled(KEY_GAP, t.keybind_desc));
                    spans.push(Span::styled(
                        format!("{}{:pad$}", chord.description, ""),
                        t.keybind_desc,
                    ));
                }
                Line::from(spans)
            })
            .collect();

        frame.render_widget(Paragraph::new(lines), inner);
        panel
    }
}

fn cell_width(chord: &LeaderChord) -> usize {
    UnicodeWidthStr::width(chord.key)
        + UnicodeWidthStr::width(KEY_GAP)
        + UnicodeWidthStr::width(chord.description)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::keybindings::{KeybindContext, leader_chords};
    use caudra_config::FeatureFlags;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    const IMMEDIATE: Duration = Duration::ZERO;
    const NOT_DUE: &str = "a panel still inside its delay must not paint";
    const NOT_ARMED: &str = "a disarmed panel must not paint";
    const MISSING_CHORD: &str = "an armed panel must list the chords it was handed";

    fn render(which_key: &WhichKey, chords: &[LeaderChord]) -> String {
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).expect("a test terminal");
        terminal
            .draw(|frame| {
                which_key.view(frame, frame.area(), chords);
            })
            .expect("a frame");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    #[test]
    fn a_disarmed_panel_paints_nothing() {
        let which_key = WhichKey::new(IMMEDIATE);
        let chords = leader_chords(&[KeybindContext::General], FeatureFlags::all());
        assert!(
            !render(&which_key, &chords).contains("tasks"),
            "{NOT_ARMED}"
        );
    }

    #[test]
    fn a_delay_holds_the_panel_back() {
        let mut which_key = WhichKey::new(Duration::from_secs(60));
        which_key.arm();
        assert!(!which_key.is_visible(), "{NOT_DUE}");
        assert!(which_key.is_armed(), "the chord is still pending");
    }

    #[test]
    fn an_armed_panel_lists_its_chords() {
        let mut which_key = WhichKey::new(IMMEDIATE);
        which_key.arm();
        let chords = leader_chords(&[KeybindContext::General], FeatureFlags::all());
        let painted = render(&which_key, &chords);
        assert!(painted.contains("Open tasks"), "{MISSING_CHORD}");
    }
}
