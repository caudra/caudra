use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::components::{DisplaySource, Overlay};
use crate::repaint::Cadence;

use crossterm::event::KeyEvent;
use ratatui::Frame;
use ratatui::layout::{Position, Rect};

const TITLE: &str = " Message Actions ";
const MAX_VISIBLE: u16 = 5;
const WIDTH_PERCENT: u16 = 38;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageActionKind {
    Fork,
    RevertBoth,
    RevertConversation,
    RevertFiles,
    Unrevert,
}

impl PickerItem for MessageActionKind {
    fn label(&self) -> &str {
        match self {
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

    pub fn open(&mut self, source: DisplaySource, pending_revert: bool) {
        let mut actions = vec![
            MessageActionKind::Fork,
            MessageActionKind::RevertBoth,
            MessageActionKind::RevertConversation,
            MessageActionKind::RevertFiles,
        ];
        if pending_revert {
            actions.push(MessageActionKind::Unrevert);
        }
        self.source = Some(source);
        self.picker.open(actions, TITLE);
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
        match self.picker.handle_key(key) {
            PickerAction::Consumed | PickerAction::Toggle(..) => MessageActionsAction::Consumed,
            PickerAction::Select(kind) => match self.source.take() {
                Some(source) => MessageActionsAction::Select { source, kind },
                None => MessageActionsAction::Close,
            },
            PickerAction::Close => {
                self.source = None;
                MessageActionsAction::Close
            }
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
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
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use maki_storage::id::MakiId;

    fn enter() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
    }

    #[test]
    fn selection_preserves_source() {
        let source = DisplaySource::Reasoning(MakiId::generate());
        let mut actions = MessageActions::new();
        actions.open(source, false);

        assert!(matches!(
            actions.handle_key(enter()),
            MessageActionsAction::Select {
                source: selected,
                kind: MessageActionKind::Fork,
            } if selected == source
        ));
    }

    #[test]
    fn unrevert_is_only_present_for_pending_revert() {
        let source = DisplaySource::User(MakiId::generate());
        let mut actions = MessageActions::new();
        actions.open(source, false);
        assert_eq!(actions.picker.selected_index(), Some(0));
        assert!(actions.picker.item(4).is_none());

        actions.open(source, true);
        assert_eq!(actions.picker.item(4), Some(&MessageActionKind::Unrevert));
    }
}
