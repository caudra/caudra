use crate::components::Overlay;
use crate::components::keybindings::key;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::components::tooltip::Tip;
use crate::repaint::Cadence;

use caudra_grab::grab_scope;
use caudra_workbench::keys::{LIST_FIRST, LIST_LAST};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, MouseEvent, MouseEventKind};
use ratatui::Frame;
#[cfg(test)]
use ratatui::layout::Position;
use ratatui::layout::Rect;

const TITLE: &str = " Submit in Plan ";
const MAX_VISIBLE: u16 = 3;
const WIDTH_PERCENT: u16 = 80;
const KEEP_EDITING_LABEL: &str = "Keep editing";
const QUEUE_LABEL: &str = "Queue in Plan";
const STOP_LABEL: &str = "Stop work and submit in Plan";
const INFO_TEXT: &str = "Current Build work continues until you choose.\n\
Queue waits for work and result processing.\n\
Stop cancels session tasks, shells, and workflows and suppresses late automatic results. \
It does not undo edits.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModeSubmissionChoice {
    KeepEditing,
    Queue,
    Stop,
}

impl PickerItem for ModeSubmissionChoice {
    fn label(&self) -> &str {
        match self {
            Self::KeepEditing => KEEP_EDITING_LABEL,
            Self::Queue => QUEUE_LABEL,
            Self::Stop => STOP_LABEL,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ModeSubmissionAction {
    Consumed,
    Select(ModeSubmissionChoice),
    Copy(String),
    Close,
}

pub(crate) struct ModeSubmission {
    picker: ListPicker<ModeSubmissionChoice>,
}

impl ModeSubmission {
    pub(crate) fn new() -> Self {
        let mut picker = ListPicker::new()
            .with_max_visible(MAX_VISIBLE)
            .with_width_percent(WIDTH_PERCENT);
        picker.set_info_text(Some(INFO_TEXT.into()));
        Self { picker }
    }

    pub(crate) fn open(&mut self) {
        self.picker.open(
            vec![
                ModeSubmissionChoice::KeepEditing,
                ModeSubmissionChoice::Queue,
                ModeSubmissionChoice::Stop,
            ],
            TITLE,
        );
    }

    pub(crate) fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub(crate) fn close(&mut self) {
        self.picker.close();
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, position: Position) -> bool {
        self.picker.contains(position)
    }

    pub(crate) fn handle_key(&mut self, event: KeyEvent) -> ModeSubmissionAction {
        if !self.is_open() {
            return ModeSubmissionAction::Close;
        }
        if event.kind != KeyEventKind::Press {
            return ModeSubmissionAction::Consumed;
        }
        if !matches!(
            event.code,
            KeyCode::Up
                | KeyCode::Down
                | KeyCode::PageUp
                | KeyCode::PageDown
                | KeyCode::Enter
                | KeyCode::Esc
        ) && !key::QUIT.matches(event)
            && !key::SCROLL_HALF_UP.matches(event)
            && !LIST_FIRST.matches(event)
            && !LIST_LAST.matches(event)
        {
            return ModeSubmissionAction::Consumed;
        }
        Self::map_picker_action(self.picker.handle_key(event))
    }

    pub(crate) fn handle_mouse(&mut self, event: MouseEvent) -> ModeSubmissionAction {
        if event.kind == MouseEventKind::Moved {
            return ModeSubmissionAction::Consumed;
        }
        Self::map_picker_action(self.picker.handle_mouse(event))
    }

    fn map_picker_action(action: PickerAction<ModeSubmissionChoice>) -> ModeSubmissionAction {
        match action {
            PickerAction::Consumed | PickerAction::Toggle(..) | PickerAction::Key(_) => {
                ModeSubmissionAction::Consumed
            }
            PickerAction::Select(choice) => ModeSubmissionAction::Select(choice),
            PickerAction::Copy(text) => ModeSubmissionAction::Copy(text),
            PickerAction::Close => ModeSubmissionAction::Close,
        }
    }

    pub(crate) fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        grab_scope!("mode_submission", area);
        self.picker.view(frame, area)
    }
}

impl Overlay for ModeSubmission {
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
    use super::{ModeSubmission, ModeSubmissionAction, ModeSubmissionChoice};
    use crate::components::list_picker::PickerItem;
    use crossterm::event::{
        KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::{Position, Rect};
    use test_case::test_case;

    const TEST_WIDTH: u16 = 100;
    const TEST_HEIGHT: u16 = 30;
    const MISSING_ROW: &str = "expected a visible choice row";

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

    fn render(
        submission: &mut ModeSubmission,
        width: u16,
        height: u16,
    ) -> (Terminal<TestBackend>, Rect) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut popup = Rect::default();
        terminal
            .draw(|frame| popup = submission.view(frame, frame.area()))
            .unwrap();
        (terminal, popup)
    }

    fn row_position(
        terminal: &Terminal<TestBackend>,
        popup: Rect,
        choice: ModeSubmissionChoice,
    ) -> Position {
        let buffer = terminal.backend().buffer();
        let row = buffer
            .content()
            .chunks(usize::from(buffer.area.width))
            .position(|cells| {
                cells
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>()
                    .contains(choice.label())
            })
            .expect(MISSING_ROW);
        Position::new(popup.x + 1, row as u16)
    }

    #[test_case(&[], ModeSubmissionChoice::KeepEditing; "default_keeps_editing")]
    #[test_case(&[KeyCode::Down], ModeSubmissionChoice::Queue; "select_queue")]
    #[test_case(&[KeyCode::Down, KeyCode::Down], ModeSubmissionChoice::Stop; "select_stop")]
    #[test_case(&[KeyCode::Up], ModeSubmissionChoice::Stop; "navigate_backwards")]
    fn keyboard_selection(navigation: &[KeyCode], expected: ModeSubmissionChoice) {
        let mut submission = ModeSubmission::new();
        submission.open();
        for &code in navigation {
            assert_eq!(
                submission.handle_key(press(code)),
                ModeSubmissionAction::Consumed
            );
        }
        assert_eq!(
            submission.handle_key(press(KeyCode::Enter)),
            ModeSubmissionAction::Select(expected)
        );
        assert!(!submission.is_open());
    }

    #[test_case(KeyCode::Esc, KeyModifiers::NONE; "escape")]
    #[test_case(KeyCode::Char('c'), KeyModifiers::CONTROL; "control_c")]
    fn close_without_submitting(code: KeyCode, modifiers: KeyModifiers) {
        let mut submission = ModeSubmission::new();
        submission.open();
        submission.handle_key(press(KeyCode::Up));
        assert_eq!(
            submission.handle_key(KeyEvent::new(code, modifiers)),
            ModeSubmissionAction::Close
        );
        assert!(!submission.is_open());
    }

    #[test_case(KeyEventKind::Repeat, 0; "repeat_default")]
    #[test_case(KeyEventKind::Repeat, 1; "repeat_queue")]
    #[test_case(KeyEventKind::Repeat, 2; "repeat_stop")]
    #[test_case(KeyEventKind::Release, 2; "release_stop")]
    fn non_press_cannot_submit(kind: KeyEventKind, steps: usize) {
        let mut submission = ModeSubmission::new();
        submission.open();
        for _ in 0..steps {
            submission.handle_key(press(KeyCode::Down));
        }
        assert_eq!(
            submission.handle_key(KeyEvent::new_with_kind(
                KeyCode::Enter,
                KeyModifiers::NONE,
                kind,
            )),
            ModeSubmissionAction::Consumed
        );
        assert!(submission.is_open());
        assert_eq!(submission.picker.selected_index(), Some(steps));
    }

    #[test_case(KeyCode::Up; "previous_stop_selection")]
    #[test_case(KeyCode::Down; "previous_queue_selection")]
    fn reopening_resets_legacy_enter_to_keep_editing(previous: KeyCode) {
        let mut submission = ModeSubmission::new();
        submission.open();
        submission.handle_key(press(previous));
        submission.close();
        submission.open();
        assert_eq!(
            submission.handle_key(press(KeyCode::Enter)),
            ModeSubmissionAction::Select(ModeSubmissionChoice::KeepEditing)
        );
    }

    #[test_case("Stop"; "stop_filter")]
    #[test_case("Queue"; "queue_filter")]
    #[test_case("arbitrary custom text"; "custom_text")]
    fn typing_cannot_filter_out_keep_editing(text: &str) {
        let mut submission = ModeSubmission::new();
        submission.open();
        for character in text.chars() {
            assert_eq!(
                submission.handle_key(press(KeyCode::Char(character))),
                ModeSubmissionAction::Consumed
            );
        }
        assert!(submission.picker.search_text().is_empty());
        assert_eq!(
            submission.handle_key(press(KeyCode::Enter)),
            ModeSubmissionAction::Select(ModeSubmissionChoice::KeepEditing)
        );
    }

    #[test_case(1, 1; "minimal")]
    #[test_case(20, 8; "small")]
    #[test_case(40, 12; "narrow")]
    #[test_case(80, 24; "standard")]
    #[test_case(120, 40; "large")]
    fn popup_stays_inside_terminal(width: u16, height: u16) {
        let mut submission = ModeSubmission::new();
        submission.open();
        let (_, popup) = render(&mut submission, width, height);
        assert_eq!(popup.intersection(Rect::new(0, 0, width, height)), popup);
        assert!(submission.contains(Position::new(popup.x, popup.y)));
        submission.close();
        assert!(!submission.contains(Position::new(popup.x, popup.y)));
    }

    #[test_case(ModeSubmissionChoice::KeepEditing; "keep_editing")]
    #[test_case(ModeSubmissionChoice::Queue; "queue")]
    #[test_case(ModeSubmissionChoice::Stop; "stop")]
    fn pointer_click_selects_the_pressed_row(choice: ModeSubmissionChoice) {
        let mut submission = ModeSubmission::new();
        submission.open();
        let (terminal, popup) = render(&mut submission, TEST_WIDTH, TEST_HEIGHT);
        let position = row_position(&terminal, popup, choice);
        assert_eq!(
            submission.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), position)),
            ModeSubmissionAction::Consumed
        );
        assert_eq!(
            submission.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), position)),
            ModeSubmissionAction::Select(choice)
        );
        assert!(!submission.is_open());
    }

    #[test_case(false; "release_without_press")]
    #[test_case(true; "release_on_different_row")]
    fn pointer_release_cannot_retarget_stop(press_keep_editing: bool) {
        let mut submission = ModeSubmission::new();
        submission.open();
        let (terminal, popup) = render(&mut submission, TEST_WIDTH, TEST_HEIGHT);
        if press_keep_editing {
            let keep_editing = row_position(&terminal, popup, ModeSubmissionChoice::KeepEditing);
            submission.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), keep_editing));
        }
        let stop = row_position(&terminal, popup, ModeSubmissionChoice::Stop);
        assert_eq!(
            submission.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), stop)),
            ModeSubmissionAction::Consumed
        );
        assert!(submission.is_open());
    }

    #[test_case(ModeSubmissionChoice::Queue; "hover_queue")]
    #[test_case(ModeSubmissionChoice::Stop; "hover_stop")]
    fn pointer_hover_does_not_arm_a_submission(choice: ModeSubmissionChoice) {
        let mut submission = ModeSubmission::new();
        submission.open();
        let (terminal, popup) = render(&mut submission, TEST_WIDTH, TEST_HEIGHT);
        let position = row_position(&terminal, popup, choice);
        submission.handle_mouse(mouse(MouseEventKind::Moved, position));
        assert_eq!(
            submission.handle_key(press(KeyCode::Enter)),
            ModeSubmissionAction::Select(ModeSubmissionChoice::KeepEditing)
        );
    }
}
