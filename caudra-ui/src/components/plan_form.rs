use crate::components::form::{render_form, selected_prefix};
use crate::components::hint_line;
use crate::components::keybindings::{key, leader};
use crate::theme;

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};

const FORM_LABEL: &str = " Plan complete ";

const DISMISS_KEYS: &str = if cfg!(target_os = "macos") {
    "⌃T/Esc"
} else {
    "Ctrl+T/Esc"
};
const HINT_PAIRS: &[(&str, &str)] = &[
    ("↑↓", "select"),
    ("Space", "toggle parallel"),
    ("Enter", "confirm"),
    (key::OPEN_EDITOR.label, "edit plan"),
    (DISMISS_KEYS, "dismiss"),
];

struct MenuItem {
    label: &'static str,
    desc: &'static str,
    action: fn() -> PlanFormAction,
}

const MENU: &[MenuItem] = &[
    MenuItem {
        label: "Refine plan",
        desc: "  Dismiss and keep editing the plan",
        action: || PlanFormAction::Hide,
    },
    MenuItem {
        label: "Clear context and implement",
        desc: "  Start fresh session, then implement the plan",
        action: || PlanFormAction::ClearAndImplement,
    },
    MenuItem {
        label: "Implement plan",
        desc: "  Keep current context, implement the plan",
        action: || PlanFormAction::Implement,
    },
];

// 2 borders + 1 empty line + 1 hint bar
const CHROME_LINES: u16 = 4;
const FORM_HEIGHT: u16 = MENU.len() as u16 + CHROME_LINES;

#[derive(Debug, PartialEq)]
pub enum PlanFormAction {
    Consumed,
    Passthrough,
    ClearAndImplement,
    Implement,
    OpenEditor,
    Hide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Visibility {
    Shown,
    Hidden,
    UserDismissed,
}

#[derive(Clone, Copy)]
struct PlanRowHit {
    area: Rect,
    menu_index: usize,
}

pub struct PlanForm {
    visibility: Visibility,
    selected: usize,
    parallel: bool,
    row_hits: Vec<PlanRowHit>,
    mouse_down: Option<usize>,
}

impl PlanForm {
    pub fn new() -> Self {
        Self {
            visibility: Visibility::Hidden,
            selected: 0,
            parallel: false,
            row_hits: Vec::new(),
            mouse_down: None,
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visibility == Visibility::Shown
    }

    pub fn on_plan_ready(&mut self) {
        if self.visibility != Visibility::UserDismissed {
            self.invalidate_mouse_geometry();
            self.visibility = Visibility::Shown;
            self.selected = 0;
        }
    }

    pub fn on_plan_drafting(&mut self) {
        self.invalidate_mouse_geometry();
        self.visibility = Visibility::Hidden;
    }

    pub fn toggle(&mut self) {
        self.invalidate_mouse_geometry();
        self.visibility = if self.is_visible() {
            Visibility::UserDismissed
        } else {
            self.selected = 0;
            Visibility::Shown
        };
    }

    pub fn hide(&mut self) {
        if self.is_visible() {
            self.invalidate_mouse_geometry();
            self.visibility = Visibility::UserDismissed;
        }
    }

    pub fn parallel(&self) -> bool {
        self.parallel
    }

    pub fn reset(&mut self) {
        self.invalidate_mouse_geometry();
        self.visibility = Visibility::Hidden;
        self.selected = 0;
    }

    pub fn hint_line(&self) -> Option<Line<'static>> {
        if self.visibility != Visibility::UserDismissed {
            return None;
        }
        let t = theme::current();
        Some(Line::from(vec![
            Span::styled(" Plan ", Style::new().fg(t.foreground)),
            Span::styled(leader::PLAN_TOGGLE.label, t.keybind_key),
            Span::raw(" "),
        ]))
    }

    pub fn height(&self) -> u16 {
        if self.is_visible() { FORM_HEIGHT } else { 0 }
    }

    #[cfg(test)]
    pub(crate) fn row_area(&self, index: usize) -> Option<Rect> {
        self.row_hits.get(index).map(|hit| hit.area)
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) -> PlanFormAction {
        if key::QUIT.matches(key_event) || key_event.code == KeyCode::Esc {
            return PlanFormAction::Hide;
        }
        if key::OPEN_EDITOR.matches(key_event) {
            return PlanFormAction::OpenEditor;
        }
        match key_event.code {
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                PlanFormAction::Consumed
            }
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(MENU.len() - 1);
                PlanFormAction::Consumed
            }
            KeyCode::Char(' ') => {
                self.parallel = !self.parallel;
                PlanFormAction::Consumed
            }
            KeyCode::Enter => (MENU[self.selected].action)(),
            KeyCode::Tab => PlanFormAction::Passthrough,
            // The chord prefix belongs to the host, which needs it to reach the
            // model picker and everything else from in here.
            _ if key::LEADER.matches(key_event) => PlanFormAction::Passthrough,
            _ => PlanFormAction::Consumed,
        }
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> PlanFormAction {
        let position = Position::new(event.column, event.row);
        let hit = self
            .row_hits
            .iter()
            .find(|hit| hit.area.contains(position))
            .copied();
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.mouse_down = None;
                if let Some(hit) = hit {
                    self.selected = hit.menu_index;
                    self.mouse_down = Some(hit.menu_index);
                    PlanFormAction::Consumed
                } else {
                    PlanFormAction::Passthrough
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.mouse_down = None;
                if hit.is_some() {
                    PlanFormAction::Consumed
                } else {
                    PlanFormAction::Passthrough
                }
            }
            MouseEventKind::Moved => {
                if let Some(hit) = hit {
                    self.selected = hit.menu_index;
                    PlanFormAction::Consumed
                } else {
                    PlanFormAction::Passthrough
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let pressed = self.mouse_down.take();
                match (pressed, hit) {
                    (Some(pressed), Some(hit)) if pressed == hit.menu_index => {
                        (MENU[pressed].action)()
                    }
                    (_, Some(_)) => PlanFormAction::Consumed,
                    _ => PlanFormAction::Passthrough,
                }
            }
            _ => PlanFormAction::Passthrough,
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) {
        if !self.is_visible() {
            self.invalidate_mouse_geometry();
            return;
        }

        let t = theme::current();
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(MENU.len() + 1);

        for (i, item) in MENU.iter().enumerate() {
            let (prefix, style) = selected_prefix(&t, i == self.selected);
            let mut spans = vec![
                Span::styled(prefix, t.tool_dim),
                Span::styled(item.label, style),
                Span::styled(item.desc, t.tool_dim),
            ];
            if self.parallel {
                spans.push(Span::styled(" (parallel)", t.tool_dim.bold()));
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::default());
        lines.push(hint_line(HINT_PAIRS));

        render_form(&t, FORM_LABEL, frame, area, lines, (0, 0), None);

        self.row_hits.clear();
        let content_bottom = area.bottom().saturating_sub(1);
        for menu_index in 0..MENU.len() {
            let y = area.y.saturating_add(1 + menu_index as u16);
            if y >= content_bottom || area.width <= 2 {
                break;
            }
            self.row_hits.push(PlanRowHit {
                area: Rect::new(area.x + 1, y, area.width - 2, 1),
                menu_index,
            });
        }
    }

    fn invalidate_mouse_geometry(&mut self) {
        self.row_hits.clear();
        self.mouse_down = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::key;
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use test_case::test_case;

    const LAST: usize = MENU.len() - 1;

    fn mouse(kind: MouseEventKind, area: Rect) -> MouseEvent {
        MouseEvent {
            kind,
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn render(form: &mut PlanForm) {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                form.view(frame, Rect::new(2, 2, 76, FORM_HEIGHT));
            })
            .unwrap();
    }

    #[test]
    fn on_plan_ready_shows_and_resets_selected() {
        let mut form = PlanForm::new();
        form.selected = 1;
        form.on_plan_ready();
        assert!(form.is_visible());
        assert_eq!(form.selected, 0);
    }

    #[test]
    fn on_plan_ready_respects_user_dismissed() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        form.hide();
        form.on_plan_ready();
        assert!(!form.is_visible());
    }

    #[test]
    fn on_plan_drafting_clears_user_dismissed() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        form.hide();
        form.on_plan_drafting();
        form.on_plan_ready();
        assert!(
            form.is_visible(),
            "drafting should clear dismiss so next ready shows"
        );
    }

    #[test]
    fn toggle_cycles_visibility() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        assert!(form.is_visible());
        form.toggle();
        assert!(!form.is_visible());
        form.toggle();
        assert!(form.is_visible());
    }

    #[test]
    fn reset_clears_state() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        form.selected = 1;
        form.reset();
        assert!(!form.is_visible());
        assert_eq!(form.selected, 0);
    }

    #[test]
    fn hint_line_only_when_dismissed() {
        let mut form = PlanForm::new();
        assert!(form.hint_line().is_none());
        form.on_plan_ready();
        assert!(form.hint_line().is_none());
        form.hide();
        assert!(form.hint_line().is_some());
    }

    #[test]
    fn height_reflects_visibility() {
        let mut form = PlanForm::new();
        assert_eq!(form.height(), 0);
        form.on_plan_ready();
        assert_eq!(form.height(), FORM_HEIGHT);
        form.hide();
        assert_eq!(form.height(), 0);
    }

    #[test_case(0, KeyCode::Up,   0    ; "up_at_zero_stays")]
    #[test_case(0, KeyCode::Down, 1    ; "down_from_zero")]
    #[test_case(LAST, KeyCode::Down, LAST ; "down_at_max_stays")]
    #[test_case(LAST, KeyCode::Up, LAST - 1 ; "up_from_max")]
    fn navigation(start: usize, code: KeyCode, expected: usize) {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        form.selected = start;
        assert_eq!(form.handle_key(key(code)), PlanFormAction::Consumed);
        assert_eq!(form.selected, expected);
    }

    #[test_case(0, PlanFormAction::Hide              ; "enter_at_0_refine")]
    #[test_case(1, PlanFormAction::ClearAndImplement ; "enter_at_1")]
    #[test_case(2, PlanFormAction::Implement          ; "enter_at_2")]
    fn enter_dispatches(selected: usize, expected: PlanFormAction) {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        form.selected = selected;
        assert_eq!(form.handle_key(key(KeyCode::Enter)), expected);
    }

    #[test]
    fn space_toggles_parallel() {
        let mut form = PlanForm::new();
        let initial = form.parallel();
        form.on_plan_ready();
        assert_eq!(form.parallel(), initial);
        assert_eq!(
            form.handle_key(key(KeyCode::Char(' '))),
            PlanFormAction::Consumed
        );
        assert_eq!(form.parallel(), !initial);
        assert_eq!(
            form.handle_key(key(KeyCode::Char(' '))),
            PlanFormAction::Consumed
        );
        assert_eq!(form.parallel(), initial);
    }

    #[test_case(key(KeyCode::Esc)              ; "esc")]
    #[test_case(key::QUIT.to_key_event()      ; "ctrl_c")]
    fn dismiss(k: KeyEvent) {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        assert_eq!(form.handle_key(k), PlanFormAction::Hide);
    }

    #[test]
    fn ctrl_o_opens_editor() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        assert_eq!(
            form.handle_key(key::OPEN_EDITOR.to_key_event()),
            PlanFormAction::OpenEditor
        );
    }

    #[test]
    fn unknown_key_consumed() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        assert_eq!(
            form.handle_key(key(KeyCode::Char('x'))),
            PlanFormAction::Consumed
        );
    }

    #[test]
    fn tab_passes_through() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        assert_eq!(
            form.handle_key(key(KeyCode::Tab)),
            PlanFormAction::Passthrough
        );
    }

    #[test]
    fn the_leader_passes_through_so_its_chords_stay_reachable() {
        let mut form = PlanForm::new();
        form.toggle();

        assert_eq!(
            form.handle_key(key::LEADER.to_key_event()),
            PlanFormAction::Passthrough
        );
    }

    #[test]
    fn hovering_plan_row_moves_selection() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        render(&mut form);
        let hit = form.row_hits[2];

        assert_eq!(
            form.handle_mouse(mouse(MouseEventKind::Moved, hit.area)),
            PlanFormAction::Consumed
        );
        assert_eq!(form.selected, 2);
    }

    #[test]
    fn clicking_plan_row_dispatches_its_action() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        render(&mut form);
        let hit = form.row_hits[1];

        form.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), hit.area));
        let action = form.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), hit.area));

        assert_eq!(action, PlanFormAction::ClearAndImplement);
    }

    #[test]
    fn releasing_on_another_plan_row_does_not_dispatch() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        render(&mut form);
        let first = form.row_hits[0];
        let second = form.row_hits[1];

        form.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), first.area));
        let action = form.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), second.area));

        assert_eq!(action, PlanFormAction::Consumed);
    }

    #[test]
    fn dragging_plan_row_cancels_click() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        render(&mut form);
        let hit = form.row_hits[0];

        form.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), hit.area));
        form.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), hit.area));
        let action = form.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), hit.area));

        assert_eq!(action, PlanFormAction::Consumed);
    }

    #[test]
    fn plan_hint_line_is_passive() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        render(&mut form);
        let last_row = form.row_hits[LAST].area;
        let hint = Rect::new(last_row.x, last_row.y + 2, last_row.width, 1);

        assert_eq!(
            form.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), hint)),
            PlanFormAction::Passthrough
        );
        assert_eq!(
            form.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), hint)),
            PlanFormAction::Passthrough
        );
    }

    #[test]
    fn hiding_plan_form_invalidates_armed_row() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        render(&mut form);
        let stale = form.row_hits[0];
        form.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), stale.area));

        form.hide();

        assert_eq!(
            form.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), stale.area)),
            PlanFormAction::Passthrough
        );
        assert!(form.row_hits.is_empty());
        assert!(form.mouse_down.is_none());
    }
}
