use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::components::{DisplaySource, Overlay};
use crate::repaint::Cadence;

use caudra_grab::grab_scope;
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};

const TITLE: &str = " Message Actions ";
const MAX_VISIBLE: u16 = 6;
const WIDTH_PERCENT: u16 = 38;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageActionKind {
    Review,
    Fork,
    RevertBoth,
    RevertConversation,
    RevertFiles,
    Unrevert,
}

impl PickerItem for MessageActionKind {
    fn label(&self) -> &str {
        match self {
            Self::Review => "Review passages",
            Self::Fork => "Fork here",
            Self::RevertBoth => "Revert both",
            Self::RevertConversation => "Revert conversation",
            Self::RevertFiles => "Revert files",
            Self::Unrevert => "Unrevert",
        }
    }
}

pub enum MessageActionsAction {
    Consumed,
    Select {
        source: DisplaySource,
        kind: MessageActionKind,
    },
    Close,
    Copy(String),
}

pub struct MessageActions {
    picker: ListPicker<MessageActionKind>,
    source: Option<DisplaySource>,
}

impl MessageActions {
    pub fn new() -> Self {
        Self {
            picker: ListPicker::new()
                .with_max_visible(MAX_VISIBLE)
                .with_width_percent(WIDTH_PERCENT),
            source: None,
        }
    }

    /// `files_withheld` says why the file reverts are left out; the title
    /// carries it so the menu explains their absence.
    pub fn open(
        &mut self,
        source: DisplaySource,
        pending_revert: bool,
        files_withheld: Option<&str>,
    ) {
        let actions = [
            MessageActionKind::Review,
            MessageActionKind::Fork,
            MessageActionKind::RevertBoth,
            MessageActionKind::RevertConversation,
            MessageActionKind::RevertFiles,
            MessageActionKind::Unrevert,
        ]
        .into_iter()
        .filter(|kind| match kind {
            MessageActionKind::RevertBoth | MessageActionKind::RevertFiles => {
                files_withheld.is_none()
            }
            MessageActionKind::Unrevert => pending_revert,
            _ => true,
        })
        .collect();
        self.source = Some(source);
        self.picker.open(
            actions,
            files_withheld.map_or_else(|| TITLE.to_owned(), |reason| format!("{TITLE}· {reason} ")),
        );
    }

    #[cfg(test)]
    pub(crate) fn offers(&self, kind: MessageActionKind) -> bool {
        (0..)
            .map_while(|index| self.picker.item(index))
            .any(|item| *item == kind)
    }

    #[cfg(test)]
    pub(crate) fn title(&self) -> &str {
        self.picker.title()
    }

    pub fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub fn close(&mut self) {
        self.source = None;
        self.picker.close();
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.picker.contains(pos)
    }

    pub fn scroll(&mut self, delta: i32) {
        self.picker.scroll(delta);
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        self.picker.handle_paste(text)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> MessageActionsAction {
        let action = self.picker.handle_key(key);
        self.map_picker_action(action)
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> MessageActionsAction {
        let action = self.picker.handle_mouse(event);
        self.map_picker_action(action)
    }

    fn map_picker_action(
        &mut self,
        action: PickerAction<MessageActionKind>,
    ) -> MessageActionsAction {
        match action {
            PickerAction::Consumed | PickerAction::Toggle(..) | PickerAction::Key(_) => {
                MessageActionsAction::Consumed
            }
            PickerAction::Select(kind) => match self.source.take() {
                Some(source) => MessageActionsAction::Select { source, kind },
                None => MessageActionsAction::Close,
            },
            PickerAction::Close => {
                self.source = None;
                MessageActionsAction::Close
            }
            PickerAction::Copy(text) => MessageActionsAction::Copy(text),
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        grab_scope!("message_actions", area);
        self.picker.view(frame, area)
    }
}

impl Overlay for MessageActions {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close();
    }

    fn cadence(&self) -> Cadence {
        self.picker.cadence()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use caudra_storage::id::CaudraId;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use test_case::test_case;

    fn enter() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
    }

    const UNREVERT_INDEX: usize = 5;
    const WITHHELD: &str = "No recorded file changes after this message";

    #[test]
    fn selection_preserves_source() {
        let source = DisplaySource::Reasoning(CaudraId::generate());
        let mut actions = MessageActions::new();
        actions.open(source, false, None);

        let MessageActionsAction::Select {
            source: selected, ..
        } = actions.handle_key(enter())
        else {
            panic!("expected a selection");
        };
        assert_eq!(selected, source);
    }

    #[test]
    fn unrevert_is_only_present_for_pending_revert() {
        let source = DisplaySource::User(CaudraId::generate());
        let mut actions = MessageActions::new();
        actions.open(source, false, None);
        assert_eq!(actions.picker.selected_index(), Some(0));
        assert!(actions.picker.item(UNREVERT_INDEX).is_none());

        actions.open(source, true, None);
        assert_eq!(
            actions.picker.item(UNREVERT_INDEX),
            Some(&MessageActionKind::Unrevert)
        );
    }

    #[test_case(None, true ; "offered_without_a_reason")]
    #[test_case(Some(WITHHELD), false ; "withheld_with_the_reason_in_the_title")]
    fn file_reverts_follow_the_reason(withheld: Option<&str>, offered: bool) {
        let mut actions = MessageActions::new();
        actions.open(DisplaySource::User(CaudraId::generate()), false, withheld);
        for kind in [
            MessageActionKind::RevertBoth,
            MessageActionKind::RevertFiles,
        ] {
            assert_eq!(actions.offers(kind), offered, "{kind:?}");
        }
        assert!(actions.offers(MessageActionKind::RevertConversation));
        assert_eq!(actions.title().contains(WITHHELD), !offered);
    }
}
