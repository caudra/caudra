//! Caudra's side of the shared scrollbar: the `ui.scrollbar` setting, and the
//! `Frame` its surfaces draw into.
//!
//! The geometry, the paint and the drag live in [`caudra_workbench::scroll`],
//! which the workbench draws from too, so a bar behaves the same wherever it
//! is shown.

use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::event::MouseEvent;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};

use caudra_workbench::scroll;

pub use caudra_workbench::scroll::{ScrollHint, ScrollbarMouse};

/// A `ListPicker` renders highlighted rows over the bar's column, so the thumb
/// resets both channels rather than inheriting the row's background.
const THUMB: Style = Style::new().fg(Color::Reset).bg(Color::Reset);

static ENABLED: AtomicBool = AtomicBool::new(true);

pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

/// Read by surfaces that draw their own bar rather than holding a
/// [`Scrollbar`], which is the workbench.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// One surface's bar. Held as a field, placed while rendering and consulted
/// while handling the mouse, which is how every other hit region here works.
///
/// `impl Into<u32>` on the totals so the transcript, whose row space outgrew
/// `u16`, and every content-bounded surface, which has not, both call it
/// unchanged.
#[derive(Clone, Debug, Default)]
pub struct Scrollbar(scroll::Scrollbar);

impl Scrollbar {
    /// A bar along the bottom row of what it is drawn into, for a surface whose
    /// lines run wider than it does. `Default` is the vertical bar.
    pub fn horizontal() -> Self {
        Self(scroll::Scrollbar::horizontal())
    }

    /// Records the strip and paints it. Turning bars off leaves no track, so
    /// there is nothing to grab and nothing to take a press: the column goes
    /// back to whatever is drawn there.
    pub fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        total: impl Into<u32>,
        position: impl Into<u32>,
    ) {
        if !enabled() {
            self.0.place(Rect::ZERO, 0, 0);
            return;
        }
        self.0.place(area, total.into(), position.into());
        self.0.render(frame.buffer_mut(), THUMB);
    }

    /// Only drawn while a drag is live, and only worth setting on a surface
    /// wide enough to spare the columns beside its bar.
    pub fn set_hint(&mut self, hint: ScrollHint) {
        self.0.set_hint(hint);
    }

    pub fn handle(&mut self, event: &MouseEvent) -> ScrollbarMouse {
        self.0.handle(event)
    }
}
