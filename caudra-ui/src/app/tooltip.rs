use std::time::Instant;

use ratatui::Frame;

use crate::components::Overlay;
use crate::components::tooltip::{Anchor, Tip, TipKey};

use super::App;

impl App {
    /// Drawn after everything else, so the box sits over the footer, the
    /// pickers and the modals it may be explaining.
    pub(super) fn render_tooltip(&mut self, frame: &mut Frame) {
        let tip = self.tooltip_candidate();
        self.tooltip.show(frame, tip, Instant::now());
    }

    /// What the pointer rests on, asked of the topmost surface alone: a box
    /// for something an overlay covers would explain what cannot be clicked.
    fn tooltip_candidate(&self) -> Option<Tip> {
        let open = self
            .overlays()
            .into_iter()
            .filter(|overlay| overlay.is_open());
        match (open.count(), self.workbench.is_open()) {
            (0, _) => self
                .commit_popup
                .tooltip()
                .or_else(|| self.active_input_box().hover_tip())
                .or_else(|| self.status_bar_tip())
                .or_else(|| self.chats[self.active_chat].hover_tip()),
            (1, true) => self.workbench.hover_tip().map(|(area, text)| Tip {
                key: TipKey::Row(area),
                anchor: Anchor::Area(area),
                text,
            }),
            _ => self
                .overlays()
                .into_iter()
                .filter(|overlay| overlay.is_open())
                .find_map(Overlay::tooltip),
        }
    }

    /// The bar builds its tip for whatever it drew hovered, so a hover the app
    /// has since cleared must not keep the last frame's box alive.
    fn status_bar_tip(&self) -> Option<Tip> {
        self.status_hover?;
        self.status_bar.hover_tip()
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, MouseEventKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use test_case::test_case;

    use super::super::tests::{mouse_event, test_app};
    use crate::app::{App, Msg};
    use crate::components::key;
    use crate::components::status_bar::{CLICK_MODEL, StatusBarHitTarget};
    use crate::components::tooltip::Tooltip;

    const WIDE: (u16, u16) = (80, 24);
    const SHOWN: &str = "the tooltip must be on screen whole once the dwell is over";
    const EARLY: &str = "the tooltip must wait for the dwell";
    const LINGERED: &str = "the tooltip outlived what dismisses it";
    const MISPLACED: &str = "a footer tooltip must open above the footer";

    fn screen(app: &mut App, (width, height): (u16, u16)) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.view(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect()
    }

    fn row_of(rows: &[String], text: &str) -> Option<usize> {
        rows.iter().position(|row| row.contains(text))
    }

    /// Rests the pointer on the model chip of a `size` terminal and lets the
    /// dwell run out, returning where the chip is.
    fn rest_on_model(app: &mut App, size: (u16, u16)) -> Rect {
        let _ = screen(app, size);
        let hit = app
            .status_hits
            .iter()
            .find(|hit| hit.target == StatusBarHitTarget::Model)
            .copied()
            .expect("the model chip is drawn");
        app.update(mouse_event(MouseEventKind::Moved, hit.area.x, hit.area.y));
        let rows = screen(app, size);
        assert_eq!(row_of(&rows, CLICK_MODEL), None, "{EARLY}");
        app.tooltip.expire();
        hit.area
    }

    #[test_case(WIDE; "wide")]
    #[test_case((40, 12); "narrow")]
    #[test_case((30, 8); "cramped")]
    fn a_footer_chip_explains_itself_above_the_footer(size: (u16, u16)) {
        let mut app = test_app();
        let chip = rest_on_model(&mut app, size);
        let rows = screen(&mut app, size);
        let row = row_of(&rows, CLICK_MODEL).expect(SHOWN);
        assert!(row < usize::from(chip.y), "{MISPLACED}");
    }

    #[test]
    fn a_key_dismisses_the_tooltip() {
        let mut app = test_app();
        rest_on_model(&mut app, WIDE);
        app.update(Msg::Key(key(KeyCode::Right)));
        assert_eq!(
            row_of(&screen(&mut app, WIDE), CLICK_MODEL),
            None,
            "{LINGERED}"
        );
    }

    #[test]
    fn a_resize_dismisses_the_tooltip() {
        let mut app = test_app();
        rest_on_model(&mut app, WIDE);
        app.dismiss_tooltip();
        assert_eq!(
            row_of(&screen(&mut app, WIDE), CLICK_MODEL),
            None,
            "{LINGERED}"
        );
    }

    #[test]
    fn a_modal_suppresses_the_tooltip() {
        let mut app = test_app();
        rest_on_model(&mut app, WIDE);
        app.help_modal.toggle();
        assert_eq!(
            row_of(&screen(&mut app, WIDE), CLICK_MODEL),
            None,
            "{LINGERED}"
        );
    }

    #[test]
    fn tooltips_switched_off_never_show() {
        let mut app = test_app();
        app.tooltip = Tooltip::new(false);
        rest_on_model(&mut app, WIDE);
        assert_eq!(
            row_of(&screen(&mut app, WIDE), CLICK_MODEL),
            None,
            "{LINGERED}"
        );
    }
}
