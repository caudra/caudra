use std::borrow::Cow;
use std::time::Instant;

use caudra_agent::QueueDelivery;
use ratatui::Frame;

use crate::components::Overlay;
use crate::components::input::{CLICK_OR_PRESS, ChordHint};
use crate::components::queue_panel::QueueHitTarget;
use crate::components::tooltip::{self, Tip, TipKey};
use crate::update;

use super::App;

const TIP_PLAN: &str = "A plan is ready for review";
const TIP_TASKS: &str = "Tasks and agents this session started";
const CLICK_PLAN: &str = " to reopen it";
const CLICK_TODO: &str = " to show the list";
const CLICK_TASKS: &str = " to browse them";

/// What the composer's chord hint is about. The plan and the todo list share
/// a chord, so the hint alone cannot say which one a click reaches.
#[derive(Clone, Copy)]
pub(super) enum ChordTopic {
    Plan,
    Todo,
    Tasks,
}

impl ChordTopic {
    pub(super) fn target(self) -> ChordHint {
        match self {
            Self::Plan | Self::Todo => ChordHint::PlanOrTodo,
            Self::Tasks => ChordHint::Tasks,
        }
    }
}

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
                .or_else(|| self.composer_hint_tip())
                .or_else(|| self.queue_tip())
                .or_else(|| self.todo_panel.hover_tip(self.tooltip.pointer()))
                .or_else(|| self.update_close_tip())
                .or_else(|| self.status_bar_tip())
                .or_else(|| self.chats[self.active_chat].hover_tip()),
            (1, true) => self
                .workbench
                .hover_tip()
                .map(|(area, text)| Tip::at(TipKey::Row(area), area, text)),
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

    /// The composer's top row: the ways to send while a run works, or the
    /// chord it advertises when idle. Only one of the two is ever drawn.
    fn composer_hint_tip(&self) -> Option<Tip> {
        if let Some(hovered) = self.admission_hover {
            return self
                .admission_hits
                .iter()
                .find(|hit| hit.admission == hovered)?
                .tip();
        }
        let hit = self.chord_hint_hit?;
        if self.chord_hint_hover != Some(hit.target) {
            return None;
        }
        let (topic, _) = self.chord_subject()?;
        let (meaning, click): (Cow<str>, _) = match topic {
            ChordTopic::Plan => (TIP_PLAN.into(), CLICK_PLAN),
            ChordTopic::Todo => (self.todo_panel.progress_tip().into(), CLICK_TODO),
            ChordTopic::Tasks => (TIP_TASKS.into(), CLICK_TASKS),
        };
        Some(Tip::at(
            TipKey::Chord(hit.target),
            hit.area,
            format!(
                "{meaning}\n{CLICK_OR_PRESS}{}{click}",
                hit.target.key_label()
            ),
        ))
    }

    /// A queue control, or the whole of a prompt its row had to cut. The
    /// item menu ignores clicks while a prompt is being edited, so it offers
    /// none then.
    fn queue_tip(&self) -> Option<Tip> {
        let editing = self.queue_editor_active();
        let control = self.queue_hover.and_then(|hovered| {
            if editing && matches!(hovered, QueueHitTarget::Item { .. }) {
                return None;
            }
            let together = self.active_queue_delivery() == Some(QueueDelivery::TogetherNextTurn);
            self.queue_hits
                .iter()
                .find(|hit| hit.target == hovered)?
                .tip(together)
        });
        control.or_else(|| tooltip::cut_row_tip(&self.queue_cut_rows, self.tooltip.pointer()))
    }

    fn update_close_tip(&self) -> Option<Tip> {
        let close = self.update_close?;
        close
            .contains(self.tooltip.pointer()?)
            .then(|| Tip::at(TipKey::UpdateClose, close, update::CLOSE_TIP.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, MouseEventKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use test_case::test_case;

    use caudra_agent::PromptAdmission;
    use caudra_agent::types::{TodoItem, TodoPriority, TodoStatus};
    use crossterm::event::MouseButton;

    use super::super::tests::{
        announced, app_with_finished_subagent, app_with_todos, app_without_splash, mouse_event,
        test_app, type_and_submit,
    };
    use super::{ChordTopic, TIP_PLAN, TIP_TASKS};
    use crate::app::{App, Msg};
    use crate::components::input::CLICK_OR_PRESS;
    use crate::components::keybindings::leader;
    use crate::components::queue_panel::{
        QueueAction, QueueHit, QueueHitTarget, TIP_SEPARATE, TIP_TOGETHER,
    };
    use crate::components::status_bar::{CLICK_MODEL, StatusBarHitTarget};
    use crate::components::tooltip::{TipKey, Tooltip};
    use crate::components::{Status, key};

    const WIDE: (u16, u16) = (80, 24);
    const NARROW: (u16, u16) = (40, 12);
    const SHOWN: &str = "the tooltip must be on screen whole once the dwell is over";
    const EARLY: &str = "the tooltip must wait for the dwell";
    const LINGERED: &str = "the tooltip outlived what dismisses it";
    const MISPLACED: &str = "a footer tooltip must open above the footer";
    const ABOVE: &str = "a composer or queue tooltip must open above its control";
    const CONTROL_MISSING: &str = "the control under test was not drawn";
    const WRONG_TIP: &str = "the tooltip is not the one for the control under the pointer";
    const SPURIOUS: &str = "a row drawn whole must not raise a tooltip";
    const RUNNING: &str = "keep working";
    const QUEUED: &str = "then this";
    const LONG_QUEUED: &str = "a queued prompt much too long to fit the row of a narrow terminal";
    const LONG_TODO: &str = "an item the model wrote far too long for a narrow todo panel row";
    const TODO_TITLE: &str = "Todos \u{b7} ";

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

    /// Rests the pointer on `at` and lets the dwell run out, returning the
    /// screen with whatever box that raised.
    fn rest_at(app: &mut App, size: (u16, u16), at: Rect) -> Vec<String> {
        app.update(mouse_event(MouseEventKind::Moved, at.x, at.y));
        let _ = screen(app, size);
        app.tooltip.expire();
        screen(app, size)
    }

    fn tip_text(app: &App) -> Option<String> {
        app.tooltip_candidate().map(|tip| tip.text)
    }

    /// A run under way with `queued` waiting behind it.
    fn queued_app(queued: &str, size: (u16, u16)) -> App {
        let mut app = test_app();
        type_and_submit(&mut app, RUNNING);
        type_and_submit(&mut app, queued);
        let _ = screen(&mut app, size);
        app
    }

    fn queue_hit(app: &App, matches: impl Fn(QueueHitTarget) -> bool) -> QueueHit {
        app.queue_hits
            .iter()
            .find(|hit| matches(hit.target))
            .copied()
            .expect(CONTROL_MISSING)
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

    #[test_case(PromptAdmission::Queue, "Enter", WIDE ; "next")]
    #[test_case(PromptAdmission::Queue, "Enter", NARROW ; "next_narrow")]
    #[test_case(PromptAdmission::Steer, leader::STEER_PROMPT.label, WIDE ; "guide")]
    #[test_case(PromptAdmission::Interrupt, leader::INTERRUPT_PROMPT.label, WIDE ; "replace")]
    fn a_send_choice_explains_itself_above_the_composer(
        admission: PromptAdmission,
        key_label: &str,
        size: (u16, u16),
    ) {
        let mut app = test_app();
        type_and_submit(&mut app, RUNNING);
        let _ = screen(&mut app, size);
        let hit = app
            .admission_hits
            .iter()
            .find(|hit| hit.admission == admission)
            .copied()
            .expect(CONTROL_MISSING);
        app.update(mouse_event(MouseEventKind::Moved, hit.area.x, hit.area.y));
        let click = format!("{CLICK_OR_PRESS}{key_label}");
        assert_eq!(row_of(&screen(&mut app, size), &click), None, "{EARLY}");

        let rows = rest_at(&mut app, size, hit.area);

        let row = row_of(&rows, &click).expect(SHOWN);
        assert!(row < usize::from(hit.area.y), "{ABOVE}");
    }

    #[test]
    fn the_queue_mode_tip_follows_the_mode() {
        let mut app = queued_app(QUEUED, WIDE);
        let toggle = queue_hit(&app, |target| target == QueueHitTarget::ToggleTogether);

        let rows = rest_at(&mut app, WIDE, toggle.area);
        let separate = tip_text(&app).expect(SHOWN);
        assert!(
            separate.starts_with(TIP_SEPARATE),
            "{WRONG_TIP}: {separate}"
        );
        assert!(
            row_of(&rows, "Separate:").is_some_and(|row| row < usize::from(toggle.area.y)),
            "{ABOVE}"
        );

        app.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            toggle.area.x,
            toggle.area.y,
        ));
        app.update(mouse_event(
            MouseEventKind::Up(MouseButton::Left),
            toggle.area.x,
            toggle.area.y,
        ));
        let _ = screen(&mut app, WIDE);
        let _ = rest_at(&mut app, WIDE, toggle.area);
        let together = tip_text(&app).expect(SHOWN);
        assert!(
            together.starts_with(TIP_TOGETHER),
            "{WRONG_TIP}: {together}"
        );
    }

    #[test]
    fn the_queue_menu_handle_names_its_actions() {
        let mut app = queued_app(QUEUED, WIDE);
        let menu = queue_hit(&app, |target| {
            matches!(
                target,
                QueueHitTarget::Item {
                    action: QueueAction::Menu,
                    ..
                }
            )
        });

        let _ = rest_at(&mut app, WIDE, menu.area);

        assert_eq!(
            app.tooltip_candidate().map(|tip| tip.key),
            Some(TipKey::Queue(menu.target)),
            "{WRONG_TIP}"
        );
    }

    #[test_case(LONG_QUEUED, Some(LONG_QUEUED) ; "cut_prompt_shows_whole")]
    #[test_case(QUEUED, None ; "whole_prompt_shows_nothing")]
    fn a_queued_prompt_shows_whole_only_when_cut(queued: &str, expected: Option<&str>) {
        let mut app = queued_app(queued, NARROW);
        let row = queue_hit(&app, |target| {
            matches!(
                target,
                QueueHitTarget::Item {
                    action: QueueAction::Select,
                    ..
                }
            )
        });
        let text = Rect::new(row.area.x + 3, row.area.y, 1, 1);

        let _ = rest_at(&mut app, NARROW, text);

        assert_eq!(tip_text(&app).as_deref(), expected, "{SPURIOUS}");
    }

    #[test_case(app_with_todos, ChordTopic::Todo ; "todos")]
    #[test_case(app_with_finished_subagent, ChordTopic::Tasks ; "tasks")]
    fn the_chord_hint_names_what_it_opens(build: fn() -> App, topic: ChordTopic) {
        let mut app = build();
        app.status = Status::Idle;
        let _ = screen(&mut app, WIDE);
        let hit = app.chord_hint_hit.expect(CONTROL_MISSING);
        assert_eq!(hit.target, topic.target(), "{CONTROL_MISSING}");

        let _ = rest_at(&mut app, WIDE, hit.area);

        let meaning = match topic {
            ChordTopic::Todo => app.todo_panel.progress_tip(),
            ChordTopic::Plan => TIP_PLAN.to_owned(),
            ChordTopic::Tasks => TIP_TASKS.to_owned(),
        };
        let text = tip_text(&app).expect(SHOWN);
        assert!(text.starts_with(&meaning), "{WRONG_TIP}: {text}");
        assert!(text.contains(hit.target.key_label()), "{WRONG_TIP}: {text}");
    }

    fn open_todo_panel(content: &str, size: (u16, u16)) -> (App, Vec<String>) {
        let mut app = test_app();
        app.todo_panel.set_items(vec![TodoItem {
            content: content.into(),
            status: TodoStatus::Pending,
            priority: TodoPriority::default(),
        }]);
        app.todo_panel.toggle();
        let rows = screen(&mut app, size);
        (app, rows)
    }

    #[test]
    fn the_todo_header_says_a_click_hides_it_until_hidden() {
        let (mut app, rows) = open_todo_panel(QUEUED, WIDE);
        let row = row_of(&rows, TODO_TITLE).expect(CONTROL_MISSING);
        let column = rows[row]
            .find(TODO_TITLE)
            .map_or(0, |byte| rows[row][..byte].chars().count());
        let header = Rect::new(column as u16, row as u16, 1, 1);

        let _ = rest_at(&mut app, WIDE, header);
        assert_eq!(
            app.tooltip_candidate().map(|tip| tip.key),
            Some(TipKey::TodoHeader),
            "{WRONG_TIP}"
        );

        app.todo_panel.toggle();
        let _ = screen(&mut app, WIDE);
        assert_eq!(tip_text(&app), None, "{LINGERED}");
    }

    #[test_case(LONG_TODO, Some(LONG_TODO) ; "cut_item_shows_whole")]
    #[test_case(QUEUED, None ; "whole_item_shows_nothing")]
    fn a_todo_item_shows_whole_only_when_cut(content: &str, expected: Option<&str>) {
        let (mut app, rows) = open_todo_panel(content, NARROW);
        let header = row_of(&rows, TODO_TITLE).expect(CONTROL_MISSING);
        let item = Rect::new(NARROW.0 / 2, header as u16 + 1, 1, 1);

        let _ = rest_at(&mut app, NARROW, item);

        assert_eq!(tip_text(&app).as_deref(), expected, "{SPURIOUS}");
    }

    #[test]
    fn the_update_close_explains_itself_until_dismissed() {
        let (mut app, _notice) = announced(app_without_splash);
        let _ = screen(&mut app, WIDE);
        let close = app.update_close.expect(CONTROL_MISSING);

        let _ = rest_at(&mut app, WIDE, close);
        assert_eq!(
            app.tooltip_candidate().map(|tip| tip.key),
            Some(TipKey::UpdateClose),
            "{WRONG_TIP}"
        );

        app.dismiss_update();
        let _ = screen(&mut app, WIDE);
        assert_eq!(tip_text(&app), None, "{LINGERED}");
    }

    #[test]
    fn a_key_dismisses_a_composer_tooltip() {
        let mut app = test_app();
        type_and_submit(&mut app, RUNNING);
        let _ = screen(&mut app, WIDE);
        let hit = app.admission_hits.first().copied().expect(CONTROL_MISSING);
        let _ = rest_at(&mut app, WIDE, hit.area);
        let click = format!("{CLICK_OR_PRESS}Enter");

        app.update(Msg::Key(key(KeyCode::Right)));

        assert_eq!(row_of(&screen(&mut app, WIDE), &click), None, "{LINGERED}");
    }
}
