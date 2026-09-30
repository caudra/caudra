use crate::components::Overlay;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::components::queue_panel::QueueEntry;
use crate::repaint::Cadence;

use caudra_agent::{PromptAdmission, QueueItemId};
use caudra_grab::grab_scope;
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};

const TITLE: &str = " Queue Actions ";
const MAX_VISIBLE: u16 = 8;
const WIDTH_PERCENT: u16 = 38;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueActionKind {
    Edit,
    MoveUp,
    MoveDown,
    Guide,
    Next,
    Replace,
    MoveMain,
    Delete,
}

impl PickerItem for QueueActionKind {
    fn label(&self) -> &str {
        match self {
            Self::Edit => "Edit",
            Self::MoveUp => "Move up",
            Self::MoveDown => "Move down",
            Self::Guide => "Move to Guide",
            Self::Next => "Move to Up next",
            Self::Replace => "Replace current run",
            Self::MoveMain => "Move to Main",
            Self::Delete => "Delete",
        }
    }
}

pub enum QueueActionsAction {
    Consumed,
    Select {
        id: QueueItemId,
        kind: QueueActionKind,
    },
    Close,
    Copy(String),
}

pub struct QueueActions {
    picker: ListPicker<QueueActionKind>,
    id: Option<QueueItemId>,
}

impl QueueActions {
    pub fn new() -> Self {
        Self {
            picker: ListPicker::new()
                .with_max_visible(MAX_VISIBLE)
                .with_width_percent(WIDTH_PERCENT),
            id: None,
        }
    }

    /// `main_queue` gates what only the main queue can do: lane changes and
    /// replacing the running turn have no meaning for subagent guidance.
    pub fn open(&mut self, entry: &QueueEntry<'_>, main_queue: bool) {
        let guided = entry.admission == Some(PromptAdmission::Steer);
        let queued = entry.admission == Some(PromptAdmission::Queue);
        let mut actions = Vec::new();
        if entry.editable {
            actions.push(QueueActionKind::Edit);
        }
        if entry.can_move_up {
            actions.push(QueueActionKind::MoveUp);
        }
        if entry.can_move_down {
            actions.push(QueueActionKind::MoveDown);
        }
        if main_queue && queued {
            actions.push(QueueActionKind::Guide);
        }
        if main_queue && guided {
            actions.push(QueueActionKind::Next);
        }
        if main_queue && (queued || guided) {
            actions.push(QueueActionKind::Replace);
        }
        if entry.movable {
            actions.push(QueueActionKind::MoveMain);
        }
        actions.push(QueueActionKind::Delete);
        self.id = Some(entry.id);
        self.picker.open(actions, TITLE);
    }

    pub fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub fn close(&mut self) {
        self.id = None;
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

    pub fn handle_key(&mut self, key: KeyEvent) -> QueueActionsAction {
        let action = self.picker.handle_key(key);
        self.map_picker_action(action)
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> QueueActionsAction {
        let action = self.picker.handle_mouse(event);
        self.map_picker_action(action)
    }

    fn map_picker_action(&mut self, action: PickerAction<QueueActionKind>) -> QueueActionsAction {
        match action {
            PickerAction::Consumed | PickerAction::Toggle(..) | PickerAction::Key(_) => {
                QueueActionsAction::Consumed
            }
            PickerAction::Select(kind) => match self.id.take() {
                Some(id) => QueueActionsAction::Select { id, kind },
                None => QueueActionsAction::Close,
            },
            PickerAction::Close => {
                self.id = None;
                QueueActionsAction::Close
            }
            PickerAction::Copy(text) => QueueActionsAction::Copy(text),
        }
    }

    #[cfg(test)]
    pub(crate) fn kinds(&self) -> Vec<QueueActionKind> {
        (0..)
            .map_while(|index| self.picker.item(index).copied())
            .collect()
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        grab_scope!("queue_actions", area);
        self.picker.view(frame, area)
    }
}

impl Overlay for QueueActions {
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
    use crossterm::event::{KeyCode, KeyModifiers};
    use std::borrow::Cow;
    use test_case::test_case;

    const PROMPT: &str = "queued prompt";

    fn entry(admission: Option<PromptAdmission>) -> QueueEntry<'static> {
        QueueEntry {
            id: QueueItemId::new(),
            text: Cow::Borrowed(PROMPT),
            color: crate::theme::current().foreground,
            editable: false,
            movable: false,
            can_move_up: false,
            can_move_down: false,
            admission,
        }
    }

    fn enter() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
    }

    #[test_case(
        entry(Some(PromptAdmission::Queue)),
        true,
        &[QueueActionKind::Guide, QueueActionKind::Replace, QueueActionKind::Delete]
        ; "queued_item_can_guide_or_replace"
    )]
    #[test_case(
        entry(Some(PromptAdmission::Steer)),
        true,
        &[QueueActionKind::Next, QueueActionKind::Replace, QueueActionKind::Delete]
        ; "guiding_item_can_defer_or_replace"
    )]
    #[test_case(
        entry(Some(PromptAdmission::Interrupt)),
        true,
        &[QueueActionKind::Delete]
        ; "pending_replacement_can_only_be_dropped"
    )]
    #[test_case(entry(None), true, &[QueueActionKind::Delete] ; "compact_can_only_be_dropped")]
    #[test_case(
        entry(Some(PromptAdmission::Steer)),
        false,
        &[QueueActionKind::Delete]
        ; "subagent_queue_has_no_lane_actions"
    )]
    #[test_case(
        QueueEntry { editable: true, ..entry(Some(PromptAdmission::Steer)) },
        false,
        &[QueueActionKind::Edit, QueueActionKind::Delete]
        ; "editable_item_can_be_edited"
    )]
    #[test_case(
        QueueEntry { movable: true, ..entry(Some(PromptAdmission::Steer)) },
        false,
        &[QueueActionKind::MoveMain, QueueActionKind::Delete]
        ; "movable_item_can_reach_main"
    )]
    #[test_case(
        QueueEntry {
            can_move_up: true,
            can_move_down: true,
            ..entry(Some(PromptAdmission::Queue))
        },
        true,
        &[
            QueueActionKind::MoveUp,
            QueueActionKind::MoveDown,
            QueueActionKind::Guide,
            QueueActionKind::Replace,
            QueueActionKind::Delete,
        ]
        ; "reorderable_item_offers_both_moves"
    )]
    fn menu_matches_item_capabilities(
        entry: QueueEntry<'static>,
        main_queue: bool,
        expected: &[QueueActionKind],
    ) {
        let mut actions = QueueActions::new();

        actions.open(&entry, main_queue);

        assert_eq!(actions.kinds(), expected);
    }

    #[test]
    fn selection_preserves_the_target_item() {
        let entry = entry(Some(PromptAdmission::Queue));
        let mut actions = QueueActions::new();
        actions.open(&entry, true);

        let QueueActionsAction::Select { id, kind } = actions.handle_key(enter()) else {
            panic!("expected a selection");
        };
        assert_eq!(id, entry.id);
        assert_eq!(kind, QueueActionKind::Guide);
    }

    #[test]
    fn closing_forgets_the_target_item() {
        let entry = entry(Some(PromptAdmission::Queue));
        let mut actions = QueueActions::new();
        actions.open(&entry, true);

        actions.close();

        assert!(!actions.is_open());
        assert!(actions.id.is_none());
    }
}
