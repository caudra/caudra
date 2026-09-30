use crate::components::Overlay;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::repaint::Cadence;

use caudra_grab::grab_scope;
use caudra_providers::{HistoryItem, HistoryItemKind, UserOrigin};
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};

const TITLE: &str = " Rewind ";
const PREVIEW_MAX_LEN: usize = 80;
pub(crate) const NO_TURNS_MSG: &str = "No user turns to rewind to";

pub enum RewindPickerAction {
    Consumed,
    Select(RewindEntry),
    Close,
    Copy(String),
}

pub struct RewindEntry {
    pub turn_index: usize,
    pub prompt_preview: String,
}

impl PickerItem for RewindEntry {
    fn label(&self) -> &str {
        &self.prompt_preview
    }
}

pub struct RewindPicker {
    picker: ListPicker<RewindEntry>,
}

impl RewindPicker {
    pub fn new() -> Self {
        Self {
            picker: ListPicker::new(),
        }
    }

    pub fn open(&mut self, items: &[HistoryItem]) -> Result<(), String> {
        let mut turn_num = 0usize;
        let mut entries: Vec<RewindEntry> = Vec::new();
        let mut start = 0;
        while start < items.len() {
            let group_id = items[start].group_id;
            let end = items[start..]
                .iter()
                .position(|item| item.group_id != group_id)
                .map_or(items.len(), |offset| start + offset);
            let Some(full_text) = user_turn_text(&items[start..end]) else {
                start = end;
                continue;
            };
            turn_num += 1;
            let first_line = full_text.lines().next().unwrap_or("");
            let preview = if first_line.len() > PREVIEW_MAX_LEN {
                format!(
                    "{turn_num}: {}...",
                    &first_line[..first_line.floor_char_boundary(PREVIEW_MAX_LEN)]
                )
            } else {
                format!("{turn_num}: {first_line}")
            };
            entries.push(RewindEntry {
                turn_index: start,
                prompt_preview: preview,
            });
            start = end;
        }
        if entries.is_empty() {
            return Err(NO_TURNS_MSG.into());
        }
        entries.reverse();
        self.picker.open(entries, TITLE);
        Ok(())
    }

    pub fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub fn close(&mut self) {
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

    pub fn handle_key(&mut self, key: KeyEvent) -> RewindPickerAction {
        let action = self.picker.handle_key(key);
        Self::map_picker_action(action)
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> RewindPickerAction {
        let action = self.picker.handle_mouse(event);
        Self::map_picker_action(action)
    }

    fn map_picker_action(action: PickerAction<RewindEntry>) -> RewindPickerAction {
        match action {
            PickerAction::Consumed => RewindPickerAction::Consumed,
            PickerAction::Select(entry) => RewindPickerAction::Select(entry),
            PickerAction::Close => RewindPickerAction::Close,
            PickerAction::Toggle(..) | PickerAction::Key(_) => RewindPickerAction::Consumed,
            PickerAction::Copy(text) => RewindPickerAction::Copy(text),
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        grab_scope!("rewind_picker", area);
        self.picker.view(frame, area)
    }
}

fn user_turn_text(items: &[HistoryItem]) -> Option<&str> {
    let first = items
        .iter()
        .find(|item| matches!(item.kind, HistoryItemKind::User { .. }))?;
    let HistoryItemKind::User {
        display_text,
        origin: UserOrigin::Turn,
        ..
    } = &first.kind
    else {
        return None;
    };
    if let Some(display_text) = display_text {
        return (!display_text.is_empty()).then_some(display_text.as_str());
    }
    items.iter().find_map(|item| match &item.kind {
        HistoryItemKind::User { text, .. } if !text.trim().is_empty() => Some(text.as_str()),
        _ => None,
    })
}

impl Overlay for RewindPicker {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close()
    }

    fn cadence(&self) -> Cadence {
        self.picker.cadence()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use caudra_providers::{ContentBlock, Message, Role};
    use test_case::test_case;

    fn user_msg(text: &str) -> Message {
        Message::user(text.into())
    }

    fn assistant_msg() -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "response".into(),
            }],
            ..Default::default()
        }
    }

    #[test_case(&[]                                          ; "empty_messages")]
    #[test_case(&[assistant_msg()]                            ; "no_user_turns")]
    #[test_case(&[Message::synthetic("continue".into())]     ; "only_synthetic")]
    fn open_without_user_turns_returns_error(msgs: &[Message]) {
        let mut picker = RewindPicker::new();
        assert_eq!(
            picker.open(&crate::history_items(msgs)),
            Err(NO_TURNS_MSG.into())
        );
    }

    #[test]
    fn entries_are_in_reverse_order() {
        let mut picker = RewindPicker::new();
        let msgs = vec![
            user_msg("first"),
            assistant_msg(),
            user_msg("second"),
            assistant_msg(),
            user_msg("third"),
        ];
        picker.open(&crate::history_items(&msgs)).unwrap();
        let item = picker.picker.selected_item().unwrap();
        assert!(item.label().contains("third"));
        assert_eq!(item.turn_index, 4);
    }

    #[test]
    fn long_prompt_is_truncated_in_preview() {
        let mut picker = RewindPicker::new();
        let long_text = "a".repeat(120);
        picker
            .open(&crate::history_items(&[user_msg(&long_text)]))
            .unwrap();
        let item = picker.picker.selected_item().unwrap();
        assert!(item.label().ends_with("..."));
        assert!(item.label().len() < 90);
    }

    #[test]
    fn multiline_prompt_uses_first_line_for_preview() {
        let mut picker = RewindPicker::new();
        picker
            .open(&crate::history_items(&[user_msg(
                "first line\nsecond line",
            )]))
            .unwrap();
        let item = picker.picker.selected_item().unwrap();
        assert!(item.label().contains("first line"));
        assert!(!item.label().contains("second"));
    }

    #[test]
    fn display_text_overrides_content() {
        let mut picker = RewindPicker::new();
        let msg = Message::user_display("ai sees this".into(), "user typed this".into());
        picker.open(&crate::history_items(&[msg])).unwrap();
        let item = picker.picker.selected_item().unwrap();
        assert!(item.label().contains("user typed this"));
    }

    #[test]
    fn synthetic_messages_and_observations_are_excluded() {
        let mut picker = RewindPicker::new();
        let msgs = vec![
            Message::observation("build failed".into()),
            user_msg("real prompt"),
            assistant_msg(),
            Message::synthetic("[Cancelled by user]".into()),
        ];
        picker.open(&crate::history_items(&msgs)).unwrap();
        let item = picker.picker.selected_item().unwrap();
        assert!(item.label().contains("real prompt"));
        assert_eq!(item.turn_index, 1);
    }

    #[test]
    fn turn_numbers_skip_synthetic() {
        let mut picker = RewindPicker::new();
        let msgs = vec![
            user_msg("first"),
            assistant_msg(),
            Message::synthetic("continue".into()),
            assistant_msg(),
            user_msg("second"),
        ];
        picker.open(&crate::history_items(&msgs)).unwrap();
        let top = picker.picker.selected_item().unwrap();
        assert!(top.label().starts_with("2: second"));
    }
}
