use std::sync::Arc;

use caudra_agent::prompt::profile::{BUILTIN_PROFILE_NAME, PromptProfileCatalog};
use caudra_grab::grab_scope;
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};

use crate::components::Overlay;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::components::tooltip::Tip;
use crate::repaint::Cadence;

const TITLE: &str = " System prompts ";
const MAX_VISIBLE: u16 = 15;

pub enum PromptProfilePickerAction {
    Consumed,
    Select(String),
    Closed,
    Copy(String),
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

    /// Lists `catalog` from the next time the picker opens.
    pub fn set_catalog(&mut self, catalog: Arc<PromptProfileCatalog>) {
        self.catalog = catalog;
    }

    pub fn open(&mut self, current: &str) {
        let mut entries = vec![ProfileItem {
            name: BUILTIN_PROFILE_NAME.to_owned(),
            description: Some("Caudra's built-in system prompt".to_owned()),
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
        grab_scope!("prompt_profile_picker", area);
        self.picker.view(frame, area)
    }

    fn map_action(action: PickerAction<ProfileItem>) -> PromptProfilePickerAction {
        match action {
            PickerAction::Consumed | PickerAction::Toggle(..) | PickerAction::Key(_) => {
                PromptProfilePickerAction::Consumed
            }
            PickerAction::Select(item) => PromptProfilePickerAction::Select(item.name),
            PickerAction::Close => PromptProfilePickerAction::Closed,
            PickerAction::Copy(text) => PromptProfilePickerAction::Copy(text),
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

    fn tooltip(&self) -> Option<Tip> {
        self.picker.tooltip()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use caudra_agent::prompt::profile::{PROFILE_DIR, PROFILE_EXTENSION};
    use crossterm::event::KeyCode;
    use tempfile::TempDir;

    use super::*;
    use crate::components::key;

    const SAVED_LATER: &str = "review";
    const SAVED_BODY: &str = "Review carefully.";

    #[test]
    fn a_profile_saved_after_the_picker_was_made_can_be_picked() {
        let dir = TempDir::new().unwrap();
        let profiles = dir.path().join(PROFILE_DIR);
        fs::create_dir(&profiles).unwrap();
        let mut picker = PromptProfilePicker::new(Arc::new(PromptProfileCatalog::default()));
        fs::write(
            profiles.join(format!("{SAVED_LATER}.{PROFILE_EXTENSION}")),
            SAVED_BODY,
        )
        .unwrap();

        picker.set_catalog(Arc::new(PromptProfileCatalog::discover_with(Some(
            dir.path(),
        ))));
        picker.open(SAVED_LATER);

        assert!(matches!(
            picker.handle_key(key(KeyCode::Enter)),
            PromptProfilePickerAction::Select(ref name) if name == SAVED_LATER
        ));
    }

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
