use crate::components::Overlay;
use crate::components::keybindings::key;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::components::tooltip::Tip;
use crate::repaint::Cadence;
use crate::update::Available;

use caudra_grab::grab_scope;
use caudra_workbench::keys::list_end;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;

const MAX_VISIBLE: u16 = 2;
const WIDTH_PERCENT: u16 = 70;
const RESTART_LABEL: &str = "Update and restart";
const EXIT_LABEL: &str = "Update and exit";
const NOT_NOW_LABEL: &str = "Not now";
const RESTART_INFO: &str = "then restarts Caudra and reopens these sessions.";
const EXIT_INFO: &str = "then exits. Start Caudra again to use it.";
const WORKING_RESTART: &str =
    "stop now; [agent] resume_interrupted decides whether their work resumes after the restart.";
const WORKING_EXIT: &str = "stop now.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UpdateChoice {
    Restart,
    Exit,
    NotNow,
}

impl PickerItem for UpdateChoice {
    fn label(&self) -> &str {
        match self {
            Self::Restart => RESTART_LABEL,
            Self::Exit => EXIT_LABEL,
            Self::NotNow => NOT_NOW_LABEL,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum UpdatePromptAction {
    Consumed,
    Select(UpdateChoice),
    Copy(String),
    Close,
}

/// Asks before the banner's release is installed, which stops every session.
pub(crate) struct UpdatePrompt {
    picker: ListPicker<UpdateChoice>,
}

/// What the prompt says it will do. `working` counts the sessions that are
/// not idle, which the update stops.
fn info_text(available: &Available, restarts: bool, working: usize) -> String {
    let mut text = format!(
        "Installs v{} from the {} channel, {}",
        available.version,
        available.channel,
        if restarts { RESTART_INFO } else { EXIT_INFO }
    );
    if working > 0 {
        let sessions = if working == 1 {
            "1 working session".to_owned()
        } else {
            format!("{working} working sessions")
        };
        let tail = if restarts {
            WORKING_RESTART
        } else {
            WORKING_EXIT
        };
        text.push_str(&format!("\n{sessions} {tail}"));
    }
    text
}

impl UpdatePrompt {
    pub(crate) fn new() -> Self {
        Self {
            picker: ListPicker::new()
                .with_max_visible(MAX_VISIBLE)
                .with_width_percent(WIDTH_PERCENT),
        }
    }

    pub(crate) fn open(&mut self, available: &Available, restarts: bool, working: usize) {
        self.picker
            .set_info_text(Some(info_text(available, restarts, working)));
        let update = if restarts {
            UpdateChoice::Restart
        } else {
            UpdateChoice::Exit
        };
        self.picker.open(
            vec![update, UpdateChoice::NotNow],
            format!(" Update to v{} ", available.version),
        );
    }

    pub(crate) fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub(crate) fn close(&mut self) {
        self.picker.close();
    }

    pub(crate) fn handle_key(&mut self, event: KeyEvent) -> UpdatePromptAction {
        if !self.is_open() {
            return UpdatePromptAction::Close;
        }
        if event.kind != KeyEventKind::Press {
            return UpdatePromptAction::Consumed;
        }
        if !matches!(
            event.code,
            KeyCode::Up | KeyCode::Down | KeyCode::Enter | KeyCode::Esc
        ) && !key::QUIT.matches(event)
            && list_end(event, self.picker.filter_is_empty()).is_none()
        {
            return UpdatePromptAction::Consumed;
        }
        Self::map_picker_action(self.picker.handle_key(event))
    }

    pub(crate) fn handle_mouse(&mut self, event: MouseEvent) -> UpdatePromptAction {
        if event.kind == MouseEventKind::Moved {
            return UpdatePromptAction::Consumed;
        }
        Self::map_picker_action(self.picker.handle_mouse(event))
    }

    fn map_picker_action(action: PickerAction<UpdateChoice>) -> UpdatePromptAction {
        match action {
            PickerAction::Consumed | PickerAction::Toggle(..) | PickerAction::Key(_) => {
                UpdatePromptAction::Consumed
            }
            PickerAction::Select(choice) => UpdatePromptAction::Select(choice),
            PickerAction::Copy(text) => UpdatePromptAction::Copy(text),
            PickerAction::Close => UpdatePromptAction::Close,
        }
    }

    pub(crate) fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        grab_scope!("update_prompt", area);
        self.picker.view(frame, area)
    }
}

impl Overlay for UpdatePrompt {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close();
    }

    fn cadence(&self) -> Cadence {
        self.picker.cadence()
    }

    fn tooltip(&self) -> Option<Tip> {
        self.picker.tooltip()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        EXIT_INFO, RESTART_INFO, UpdateChoice, UpdatePrompt, UpdatePromptAction, WORKING_EXIT,
        WORKING_RESTART, info_text,
    };
    use crate::components::list_picker::PickerItem;
    use crate::update::Available;
    use caudra_storage::version::UpdateChannel;
    use crossterm::event::{
        KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::{Position, Rect};
    use test_case::test_case;

    const VERSION: &str = "99.9.9";
    const WIDTH: u16 = 100;
    const HEIGHT: u16 = 30;
    const MISSING_ROW: &str = "expected a visible choice row";

    fn available() -> Available {
        Available {
            version: VERSION.to_owned(),
            channel: UpdateChannel::Preview,
        }
    }

    fn opened(restarts: bool) -> UpdatePrompt {
        let mut prompt = UpdatePrompt::new();
        prompt.open(&available(), restarts, 0);
        prompt
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn mouse(kind: MouseEventKind, position: Position) -> MouseEvent {
        MouseEvent {
            kind,
            column: position.x,
            row: position.y,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test_case(true, &[], UpdateChoice::Restart; "enter_restarts_by_default")]
    #[test_case(false, &[], UpdateChoice::Exit; "enter_exits_where_no_restart_follows")]
    #[test_case(true, &[KeyCode::Down], UpdateChoice::NotNow; "down_declines")]
    #[test_case(true, &[KeyCode::End], UpdateChoice::NotNow; "end_declines")]
    fn keyboard_selection(restarts: bool, navigation: &[KeyCode], expected: UpdateChoice) {
        let mut prompt = opened(restarts);
        for &code in navigation {
            assert_eq!(prompt.handle_key(press(code)), UpdatePromptAction::Consumed);
        }
        assert_eq!(
            prompt.handle_key(press(KeyCode::Enter)),
            UpdatePromptAction::Select(expected)
        );
        assert!(!prompt.is_open());
    }

    #[test_case(KeyCode::Esc, KeyModifiers::NONE; "escape")]
    #[test_case(KeyCode::Char('c'), KeyModifiers::CONTROL; "control_c")]
    fn closes_without_updating(code: KeyCode, modifiers: KeyModifiers) {
        let mut prompt = opened(true);
        assert_eq!(
            prompt.handle_key(KeyEvent::new(code, modifiers)),
            UpdatePromptAction::Close
        );
        assert!(!prompt.is_open());
    }

    #[test_case(KeyEventKind::Repeat; "repeat")]
    #[test_case(KeyEventKind::Release; "release")]
    fn only_a_press_can_update(kind: KeyEventKind) {
        let mut prompt = opened(true);
        assert_eq!(
            prompt.handle_key(KeyEvent::new_with_kind(
                KeyCode::Enter,
                KeyModifiers::NONE,
                kind
            )),
            UpdatePromptAction::Consumed
        );
        assert!(prompt.is_open());
    }

    #[test]
    fn typing_cannot_filter_the_choices() {
        let mut prompt = opened(true);
        for character in "Not".chars() {
            prompt.handle_key(press(KeyCode::Char(character)));
        }
        assert_eq!(
            prompt.handle_key(press(KeyCode::Enter)),
            UpdatePromptAction::Select(UpdateChoice::Restart)
        );
    }

    #[test_case(UpdateChoice::Restart; "restart")]
    #[test_case(UpdateChoice::NotNow; "not_now")]
    fn a_click_selects_the_pressed_row(choice: UpdateChoice) {
        let mut prompt = opened(true);
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
        let mut popup = Rect::default();
        terminal
            .draw(|frame| popup = prompt.view(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let row = buffer
            .content()
            .chunks(usize::from(WIDTH))
            .position(|cells| {
                cells
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>()
                    .contains(choice.label())
            })
            .expect(MISSING_ROW);
        let position = Position::new(popup.x + 1, row as u16);
        prompt.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), position));
        assert_eq!(
            prompt.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), position)),
            UpdatePromptAction::Select(choice)
        );
    }

    #[test_case(true, 0 => format!("Installs v{VERSION} from the preview channel, {RESTART_INFO}"); "idle_restart")]
    #[test_case(false, 0 => format!("Installs v{VERSION} from the preview channel, {EXIT_INFO}"); "idle_exit")]
    #[test_case(true, 1 => format!("Installs v{VERSION} from the preview channel, {RESTART_INFO}\n1 working session {WORKING_RESTART}"); "one_working")]
    #[test_case(false, 3 => format!("Installs v{VERSION} from the preview channel, {EXIT_INFO}\n3 working sessions {WORKING_EXIT}"); "several_working")]
    fn info_names_what_the_update_stops(restarts: bool, working: usize) -> String {
        info_text(&available(), restarts, working)
    }
}
