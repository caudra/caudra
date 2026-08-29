use std::sync::Arc;

use crossterm::event::{KeyEvent, MouseEvent};
use maki_agent::prompt::profile::{BUILTIN_PROFILE_NAME, PromptProfileCatalog};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};

use crate::components::Overlay;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::repaint::Cadence;

const TITLE: &str = " System prompts ";
const MAX_VISIBLE: u16 = 15;

pub enum PromptProfilePickerAction {
    Consumed,
    Select(String),
    Closed,
}

struct ProfileItem {
    name: String,
    description: Option<String>,
}

impl PickerItem for ProfileItem {
    fn label(&self) -> &str {
        &self.name
    }

    fn detail(&self) -> Option<&str> {
        self.description.as_deref()
    }
}

pub struct PromptProfilePicker {
    picker: ListPicker<ProfileItem>,
    catalog: Arc<PromptProfileCatalog>,
}

impl PromptProfilePicker {
    pub fn new(catalog: Arc<PromptProfileCatalog>) -> Self {
        Self {
            picker: ListPicker::new().with_max_visible(MAX_VISIBLE),
            catalog,
        }
    }

    pub fn open(&mut self, current: &str) {
        let mut entries = vec![ProfileItem {
            name: BUILTIN_PROFILE_NAME.to_owned(),
            description: Some("Maki's built-in system prompt".to_owned()),
        }];
        entries.extend(self.catalog.profiles().map(|profile| ProfileItem {
            name: profile.name().to_owned(),
            description: profile.description().map(str::to_owned),
        }));
        let current_index = entries
            .iter()
            .position(|entry| entry.name == current)
            .unwrap_or(0);
        self.picker.open(entries, TITLE);
        self.picker.select(current_index);
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

    pub fn handle_key(&mut self, key: KeyEvent) -> PromptProfilePickerAction {
        Self::map_action(self.picker.handle_key(key))
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> PromptProfilePickerAction {
        Self::map_action(self.picker.handle_mouse(event))
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        self.picker.handle_paste(text)
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        self.picker.view(frame, area)
    }

    fn map_action(action: PickerAction<ProfileItem>) -> PromptProfilePickerAction {
        match action {
            PickerAction::Consumed | PickerAction::Toggle(..) => {
                PromptProfilePickerAction::Consumed
            }
            PickerAction::Select(item) => PromptProfilePickerAction::Select(item.name),
            PickerAction::Close => PromptProfilePickerAction::Closed,
        }
    }
}

impl Overlay for PromptProfilePicker {
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
    use crossterm::event::KeyCode;

    use super::*;
    use crate::components::key;

    #[test]
    fn builtin_is_always_selectable() {
        let mut picker = PromptProfilePicker::new(Arc::new(PromptProfileCatalog::default()));
        picker.open(BUILTIN_PROFILE_NAME);
        let action = picker.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            action,
            PromptProfilePickerAction::Select(ref name) if name == BUILTIN_PROFILE_NAME
        ));
        assert!(!picker.is_open());
    }
}
