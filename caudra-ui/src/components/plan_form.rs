use crate::components::form::{footer_row, render_form, selected_prefix};
use crate::components::keybindings::key;
use crate::components::{Hint, HintBar, VisualRows, hanging_lines, visual_rows};
use crate::theme;

use caudra_grab::grab_scope;
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

const FORM_LABEL: &str = " Plan complete ";
const HINT_LABEL: &str = "Plan";

/// Separates a description from the label it follows on one row, and the
/// column it hangs under once it needs rows of its own.
const DESC_GAP: &str = "  ";
const DESC_INDENT: &str = "    ";
const PARALLEL_MARK: &str = " (parallel)";

const DISMISS_KEYS: &str = if cfg!(target_os = "macos") {
    "⌃T/Esc"
} else {
    "Ctrl+T/Esc"
};
const HINTS: [Hint; 5] = [
    Hint::inert("↑↓", "select"),
    Hint::bind(key::SPACE, "toggle parallel"),
    Hint::bind(key::ENTER, "confirm"),
    Hint::bind(key::OPEN_EDITOR, "edit plan"),
    Hint::key(DISMISS_KEYS, KeyCode::Esc, "dismiss"),
];

struct MenuItem {
    label: &'static str,
    desc: &'static str,
    action: fn() -> PlanFormAction,
}

const MENU: &[MenuItem] = &[
    MenuItem {
        label: "Refine plan",
        desc: "Dismiss and keep editing the plan",
        action: || PlanFormAction::Hide,
    },
    MenuItem {
        label: "Clear context and implement",
        desc: "Start fresh session, then implement the plan",
        action: || PlanFormAction::ClearAndImplement,
    },
    MenuItem {
        label: "Implement plan",
        desc: "Keep current context, implement the plan",
        action: || PlanFormAction::Implement,
    },
];

// 2 borders + 1 empty line + 1 hint bar
const CHROME_LINES: u16 = 4;
/// The form wraps to fit, so a narrow terminal must not hand the whole screen
/// to three options. The layout clamps this again against the room it has.
const MAX_HEIGHT_PERCENT: u16 = 75;

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
    hints: HintBar,
}

impl PlanForm {
    pub fn new() -> Self {
        Self {
            visibility: Visibility::Hidden,
            selected: 0,
            parallel: false,
            row_hits: Vec::new(),
            mouse_down: None,
            hints: HintBar::default(),
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

    /// What the composer's top row reads while the form is dismissed, so the
    /// plan stays reachable from the chord it names.
    pub fn hint_label(&self) -> Option<&'static str> {
        (self.visibility == Visibility::UserDismissed).then_some(HINT_LABEL)
    }

    /// The rows the form wants at this width, so the layout reserves what the
    /// form will actually draw rather than one row per option. A wrapped
    /// description costs rows, and reserving without measuring is what used to
    /// push the last option off the bottom.
    pub fn height(&self, width: u16, available: u16) -> u16 {
        if !self.is_visible() {
            return 0;
        }
        let body_width = width.saturating_sub(2);
        visual_rows(&self.body(body_width).0, body_width)
            .total
            .saturating_add(CHROME_LINES)
            .min(available.saturating_mul(MAX_HEIGHT_PERCENT) / 100)
            .max(CHROME_LINES + 1)
    }

    #[cfg(test)]
    pub(crate) fn row_area(&self, index: usize) -> Option<Rect> {
        self.row_hits
            .iter()
            .find(|hit| hit.menu_index == index)
            .map(|hit| hit.area)
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
        if let Some(key_event) = self.hints.handle_mouse(event) {
            return self.handle_key(key_event);
        }
        if self.hints.hovered().is_some()
            && matches!(
                event.kind,
                MouseEventKind::Down(MouseButton::Left)
                    | MouseEventKind::Drag(MouseButton::Left)
                    | MouseEventKind::Up(MouseButton::Left)
                    | MouseEventKind::Moved
            )
        {
            self.mouse_down = None;
            return PlanFormAction::Consumed;
        }
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

        grab_scope!("plan_form", area);
        let t = theme::current();
        let width = area.width.saturating_sub(2);
        let (lines, owners) = self.body(width);
        let rows = visual_rows(&lines, width);
        let visible = area.height.saturating_sub(CHROME_LINES);
        let scroll = self.scroll(&rows, &owners, visible);
        let footer = self.hints.line(footer_row(area), HINTS.to_vec());

        render_form(
            &t,
            FORM_LABEL,
            frame,
            area,
            lines,
            (scroll, 0),
            Some(footer),
        );

        self.record_row_hits(&rows, &owners, area, visible, scroll);
    }

    /// The menu as lines, paired with the option each line belongs to.
    /// Measuring and drawing both read this, so the rows reserved and the rows
    /// drawn can never disagree, and a click can name the option under it
    /// however the text wrapped.
    ///
    /// A description rides on its label's row while it fits and hangs under a
    /// fixed indent when it does not. Hanging under the label itself would
    /// indent by the label's own width, which leaves nothing to wrap into on a
    /// narrow terminal.
    fn body(&self, width: u16) -> (Vec<Line<'static>>, Vec<usize>) {
        let t = theme::current();
        let mut lines = Vec::with_capacity(MENU.len());
        let mut owners = Vec::with_capacity(MENU.len());
        for (index, item) in MENU.iter().enumerate() {
            let (prefix, style) = selected_prefix(&t, index == self.selected);
            let mut head = vec![
                Span::styled(prefix, t.tool_dim),
                Span::styled(item.label, style),
            ];
            if self.parallel {
                head.push(Span::styled(PARALLEL_MARK, t.tool_dim.bold()));
            }
            let head_width: usize = head.iter().map(|span| span.content.width()).sum();
            let inline = head_width + DESC_GAP.width() + item.desc.width();
            if inline <= usize::from(width) {
                head.push(Span::styled(format!("{DESC_GAP}{}", item.desc), t.tool_dim));
                lines.push(Line::from(head));
                owners.push(index);
                continue;
            }
            lines.push(Line::from(head));
            owners.push(index);
            for line in hanging_lines(
                Span::styled(DESC_INDENT, t.tool_dim),
                Span::styled(item.desc, t.tool_dim),
                width,
            ) {
                lines.push(line);
                owners.push(index);
            }
        }
        (lines, owners)
    }

    /// The first row to draw, so the selected option stays on screen however
    /// the menu wrapped and however little room the layout left. An option
    /// taller than the viewport is shown from its top rather than its end.
    fn scroll(&self, rows: &VisualRows, owners: &[usize], visible: u16) -> u16 {
        let Some(first) = owners.iter().position(|&owner| owner == self.selected) else {
            return 0;
        };
        let count = owners[first..]
            .iter()
            .take_while(|&&owner| owner == self.selected)
            .count();
        let top = rows.row_of(first as u16);
        let bottom = rows.row_of((first + count) as u16).saturating_sub(1);
        bottom.saturating_sub(visible.saturating_sub(1)).min(top)
    }

    /// Where each option landed on screen, clipped to the viewport. A row
    /// scrolled past the fold records nothing: it must not stay clickable
    /// through whatever is drawn over it.
    fn record_row_hits(
        &mut self,
        rows: &VisualRows,
        owners: &[usize],
        area: Rect,
        visible: u16,
        scroll: u16,
    ) {
        self.row_hits.clear();
        if area.width <= 2 {
            return;
        }
        for (line, &menu_index) in owners.iter().enumerate() {
            let Some(offset) = rows.row_of(line as u16).checked_sub(scroll) else {
                continue;
            };
            let height = rows
                .height_of(line as u16)
                .min(visible.saturating_sub(offset));
            if height == 0 {
                continue;
            }
            self.row_hits.push(PlanRowHit {
                area: Rect::new(area.x + 1, area.y + 1 + offset, area.width - 2, height),
                menu_index,
            });
        }
    }

    fn invalidate_mouse_geometry(&mut self) {
        self.row_hits.clear();
        self.mouse_down = None;
        self.hints.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::{buffer_text, key};
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::buffer::Buffer;
    use test_case::test_case;

    const LAST: usize = MENU.len() - 1;
    /// What an 80-column terminal leaves the form once the gutter is taken.
    /// `Clear context and implement` no longer fits on one row here, which is
    /// the width the last option used to vanish at.
    const NARROW: u16 = 76;
    const WIDE: u16 = 120;
    const ROOM: u16 = 23;
    const MISSING_OPTION: &str = "every option must be drawn";

    fn mouse(kind: MouseEventKind, area: Rect) -> MouseEvent {
        MouseEvent {
            kind,
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn area_of(form: &PlanForm, width: u16) -> Rect {
        Rect::new(2, 2, width, form.height(width, ROOM))
    }

    fn draw(form: &mut PlanForm, area: Rect) -> Buffer {
        let backend = ratatui::backend::TestBackend::new(area.right() + 2, area.bottom() + 2);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| form.view(frame, area)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn render_at(form: &mut PlanForm, width: u16) -> Buffer {
        draw(form, area_of(form, width))
    }

    fn render(form: &mut PlanForm) -> Buffer {
        render_at(form, NARROW)
    }

    fn row_text(buffer: &Buffer, y: u16) -> String {
        (buffer.area.x..buffer.area.right())
            .map(|x| buffer[(x, y)].symbol())
            .collect()
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
    fn hint_label_only_when_dismissed() {
        let mut form = PlanForm::new();
        assert!(form.hint_label().is_none());
        form.on_plan_ready();
        assert!(form.hint_label().is_none());
        form.hide();
        assert_eq!(form.hint_label(), Some(HINT_LABEL));
    }

    #[test]
    fn height_reflects_visibility() {
        let mut form = PlanForm::new();
        assert_eq!(form.height(WIDE, ROOM), 0);
        form.on_plan_ready();
        assert_eq!(form.height(WIDE, ROOM), MENU.len() as u16 + CHROME_LINES);
        form.hide();
        assert_eq!(form.height(WIDE, ROOM), 0);
    }

    /// The bug this form used to have: the layout reserved one row per option
    /// while the widget drew a wrapped description over two, so the last
    /// option was pushed off the bottom while staying selectable.
    #[test]
    fn height_grows_when_a_description_wraps() {
        let mut form = PlanForm::new();
        form.on_plan_ready();

        assert!(form.height(NARROW, ROOM) > form.height(WIDE, ROOM));
    }

    #[test_case(NARROW, false ; "narrow")]
    #[test_case(NARROW, true  ; "narrow_parallel")]
    #[test_case(WIDE, false   ; "wide")]
    #[test_case(WIDE, true    ; "wide_parallel")]
    fn every_option_is_drawn(width: u16, parallel: bool) {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        form.parallel = parallel;

        let screen = buffer_text(&render_at(&mut form, width));

        for item in MENU {
            assert!(
                screen.contains(item.label),
                "{MISSING_OPTION}: {}",
                item.label
            );
        }
    }

    /// A wrapped description takes a row of its own, so a rect placed by
    /// counting options rather than rows lands a click on the wrong action.
    #[test_case(0 ; "first")]
    #[test_case(1 ; "second")]
    #[test_case(2 ; "third")]
    fn a_row_hit_covers_the_option_it_names(index: usize) {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        let buffer = render(&mut form);
        let hit = form.row_area(index).expect(MISSING_OPTION);

        assert!(row_text(&buffer, hit.y).contains(MENU[index].label));
    }

    /// The layout clamps the form on a short terminal, so the viewport has to
    /// follow the selection rather than leaving it below the fold.
    #[test_case(0 ; "first")]
    #[test_case(1 ; "second")]
    #[test_case(2 ; "third")]
    fn the_selected_option_stays_visible_when_clamped(selected: usize) {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        form.selected = selected;

        let area = Rect::new(2, 2, NARROW, CHROME_LINES + 1);
        let screen = buffer_text(&draw(&mut form, area));

        assert!(screen.contains(MENU[selected].label), "{MISSING_OPTION}");
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
        let hit = form.row_area(LAST).expect(MISSING_OPTION);

        assert_eq!(
            form.handle_mouse(mouse(MouseEventKind::Moved, hit)),
            PlanFormAction::Consumed
        );
        assert_eq!(form.selected, LAST);
    }

    #[test]
    fn clicking_plan_row_dispatches_its_action() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        render(&mut form);
        let hit = form.row_area(1).expect(MISSING_OPTION);

        form.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), hit));
        let action = form.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), hit));

        assert_eq!(action, PlanFormAction::ClearAndImplement);
    }

    #[test]
    fn releasing_on_another_plan_row_does_not_dispatch() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        render(&mut form);
        let first = form.row_area(0).expect(MISSING_OPTION);
        let second = form.row_area(1).expect(MISSING_OPTION);

        form.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), first));
        let action = form.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), second));

        assert_eq!(action, PlanFormAction::Consumed);
    }

    #[test]
    fn dragging_plan_row_cancels_click() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        render(&mut form);
        let hit = form.row_area(0).expect(MISSING_OPTION);

        form.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), hit));
        form.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), hit));
        let action = form.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), hit));

        assert_eq!(action, PlanFormAction::Consumed);
    }

    #[test]
    fn a_group_hint_is_passive_and_a_command_hint_presses_its_key() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        form.selected = 1;
        render(&mut form);
        let hits = crate::components::hint_hits(&HINTS, footer_row(area_of(&form, NARROW)));
        let (group, confirm) = (hits[0], hits[2]);

        assert_eq!(
            form.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), group)),
            PlanFormAction::Passthrough
        );
        assert_eq!(
            form.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), group)),
            PlanFormAction::Passthrough
        );
        assert_eq!(
            form.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), confirm)),
            PlanFormAction::Consumed
        );
        assert_eq!(
            form.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), confirm)),
            PlanFormAction::ClearAndImplement
        );
    }

    #[test]
    fn hiding_plan_form_invalidates_armed_row() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        render(&mut form);
        let stale = form.row_area(0).expect(MISSING_OPTION);
        form.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), stale));

        form.hide();

        assert_eq!(
            form.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), stale)),
            PlanFormAction::Passthrough
        );
        assert!(form.row_hits.is_empty());
        assert!(form.mouse_down.is_none());
    }
}
