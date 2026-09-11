use std::sync::atomic::{AtomicBool, Ordering};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState};

pub const SCROLLBAR_THUMB: &str = "\u{2590}";

static ENABLED: AtomicBool = AtomicBool::new(true);

pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

/// Read by surfaces that draw their own bar rather than calling
/// [`render_vertical_scrollbar`], which is the workbench.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// `impl Into<u32>` so the transcript, whose row space outgrew `u16`, and every
/// content-bounded surface, which has not, both call it unchanged.
pub fn render_vertical_scrollbar(
    frame: &mut Frame,
    area: Rect,
    content_len: impl Into<u32>,
    position: impl Into<u32>,
) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let (content_len, position) = (content_len.into(), position.into());
    let max_scroll = content_len.saturating_sub(u32::from(area.height));
    let mut state = ScrollbarState::default()
        .content_length(max_scroll as usize + 1)
        .position(position as usize);

    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .thumb_symbol(SCROLLBAR_THUMB)
        // ListPicker renders highlighted rows over the scrollbar track; resetting
        // the thumb style keeps its color stable instead of inheriting row bg.
        .thumb_style(Style::new().fg(Color::Reset).bg(Color::Reset))
        .track_symbol(None)
        .begin_symbol(None)
        .end_symbol(None);

    frame.render_stateful_widget(scrollbar, area, &mut state);
}
