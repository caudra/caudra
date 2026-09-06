//! The `/memory` browser: every note grouped under the tags it carries.
//!
//! A note with several tags appears once per tag, matching how the model finds
//! it. The tag is the section header, so the list reads as the same index the
//! system prompt advertises.

use caudra_agent::tools::native::memory::{self, BrowseEntry};
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::Line;

use crate::components::keybindings::key;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::components::{Overlay, hint_line};
use crate::repaint::Cadence;

const TITLE: &str = " Memory Files ";
const MAX_VISIBLE: u16 = 15;
const EMPTY_TEXT: &str = "No memories yet";

pub enum MemoryPickerAction {
    Consumed,
    Open(String),
    Delete(String),
    Closed,
}

struct MemoryItem {
    name: String,
    detail: String,
    /// Carries the count so the header reads `arch (3)`. The list matches on
    /// the label alone, so folding it in here costs nothing.
    section: String,
}

impl PickerItem for MemoryItem {
    fn label(&self) -> &str {
        &self.name
    }

    fn detail(&self) -> Option<&str> {
        Some(&self.detail)
    }

    fn section(&self) -> Option<&str> {
        Some(&self.section)
    }
}

pub struct MemoryPicker {
    picker: ListPicker<MemoryItem>,
    pending_delete: Option<String>,
    /// Set when an editor was launched from here. The editor runs
    /// synchronously in the event loop, so by the next tick it has exited and
    /// the note on disk may have changed.
    stale: bool,
}

impl MemoryPicker {
    pub fn new() -> Self {
        let mut picker = ListPicker::new()
            .with_max_visible(MAX_VISIBLE)
            .with_footer_builder(footer);
        picker.set_empty_text(EMPTY_TEXT);
        Self {
            picker,
            pending_delete: None,
            stale: false,
        }
    }

    pub fn open(&mut self, entries: Vec<BrowseEntry>) {
        let items = build_items(entries);
        self.pending_delete = None;
        self.stale = false;
        self.picker.set_info_text(None);
        // Replacing keeps the live search query, so a refresh after an edit
        // does not throw away what the user typed.
        if self.picker.is_open() {
            self.picker.replace_items(items);
        } else {
            self.picker.open(items, TITLE);
        }
    }

    pub fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub fn close(&mut self) {
        self.picker.close();
        self.pending_delete = None;
        self.stale = false;
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.picker.contains(pos)
    }

    pub fn scroll(&mut self, delta: i32) {
        self.picker.scroll(delta);
    }

    /// True once, after the editor this picker launched has exited.
    pub fn take_stale(&mut self) -> bool {
        std::mem::take(&mut self.stale)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> MemoryPickerAction {
        // `ListPicker` swallows every unbound control key, so both bindings
        // have to be claimed before the list sees them.
        if key::DELETE.matches(key) {
            return self.press_delete();
        }
        self.clear_pending();
        if key::OPEN_EDITOR.matches(key) {
            return self.open_selected();
        }
        let action = self.picker.handle_key(key);
        self.map_action(action)
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> MemoryPickerAction {
        let action = self.picker.handle_mouse(event);
        if !matches!(action, PickerAction::Consumed) {
            self.clear_pending();
        }
        self.map_action(action)
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        self.picker.handle_paste(text)
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        self.picker.view(frame, area)
    }

    fn open_selected(&mut self) -> MemoryPickerAction {
        match self.picker.selected_item().map(|item| item.name.clone()) {
            Some(name) => {
                self.stale = true;
                MemoryPickerAction::Open(name)
            }
            None => MemoryPickerAction::Consumed,
        }
    }

    fn press_delete(&mut self) -> MemoryPickerAction {
        let Some(name) = self.picker.selected_item().map(|item| item.name.clone()) else {
            return MemoryPickerAction::Consumed;
        };
        if self.pending_delete.as_deref() == Some(name.as_str()) {
            self.clear_pending();
            return MemoryPickerAction::Delete(name);
        }
        self.pending_delete = Some(name);
        self.picker.set_info_text(Some(confirm_hint()));
        MemoryPickerAction::Consumed
    }

    /// Moving off the armed row cancels the delete, so a stray `Ctrl+D` later
    /// cannot drop a note the user is no longer looking at.
    fn clear_pending(&mut self) {
        if self.pending_delete.take().is_some() {
            self.picker.set_info_text(None);
        }
    }

    fn map_action(&mut self, action: PickerAction<MemoryItem>) -> MemoryPickerAction {
        match action {
            PickerAction::Consumed | PickerAction::Toggle(..) => MemoryPickerAction::Consumed,
            PickerAction::Select(item) => {
                self.stale = true;
                MemoryPickerAction::Open(item.name)
            }
            PickerAction::Close => {
                self.pending_delete = None;
                MemoryPickerAction::Closed
            }
        }
    }
}

impl Overlay for MemoryPicker {
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

fn footer() -> Line<'static> {
    hint_line(&[
        ("Enter", "open"),
        (key::DELETE.label, "delete"),
        ("Esc", "close"),
    ])
}

fn confirm_hint() -> String {
    format!("Press {} again to delete this note.", key::DELETE.label)
}

fn build_items(entries: Vec<BrowseEntry>) -> Vec<MemoryItem> {
    entries
        .into_iter()
        .map(|entry| MemoryItem {
            name: entry.name,
            detail: format!("({})", memory::token_label(entry.tokens)),
            section: format!("{} ({})", entry.tag, entry.tag_count),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::key as key_event;
    use crossterm::event::KeyCode;

    fn entry(name: &str, tag: &str, tag_count: usize) -> BrowseEntry {
        BrowseEntry {
            name: name.into(),
            tokens: 10,
            tag: tag.into(),
            tag_count,
        }
    }

    fn opened(entries: Vec<BrowseEntry>) -> MemoryPicker {
        let mut picker = MemoryPicker::new();
        picker.open(entries);
        picker
    }

    #[test]
    fn a_tag_becomes_a_section_header_with_its_count() {
        let items = build_items(vec![entry("a.md", "arch", 3)]);
        assert_eq!(items[0].section(), Some("arch (3)"));
        assert_eq!(items[0].label(), "a.md");
        assert_eq!(items[0].detail(), Some("(~10 tokens)"));
    }

    #[test]
    fn enter_opens_the_selected_note() {
        let mut picker = opened(vec![entry("a.md", "arch", 1)]);
        let action = picker.handle_key(key_event(KeyCode::Enter));
        let MemoryPickerAction::Open(name) = action else {
            panic!("enter opens the selection");
        };
        assert_eq!(name, "a.md");
    }

    #[test]
    fn the_editor_binding_opens_the_selected_note() {
        let mut picker = opened(vec![entry("a.md", "arch", 1)]);
        let MemoryPickerAction::Open(name) = picker.handle_key(key::OPEN_EDITOR.to_key_event())
        else {
            panic!("the editor binding opens the selection");
        };
        assert_eq!(name, "a.md");
    }

    /// The editor runs outside the TUI, so the list it came from is suspect
    /// the moment it returns.
    #[test]
    fn opening_a_note_marks_the_list_stale_exactly_once() {
        let mut picker = opened(vec![entry("a.md", "arch", 1)]);
        picker.handle_key(key_event(KeyCode::Enter));
        assert!(picker.take_stale());
        assert!(!picker.take_stale(), "staleness is consumed");
    }

    #[test]
    fn merely_moving_the_cursor_does_not_mark_the_list_stale() {
        let mut picker = opened(vec![entry("a.md", "arch", 2), entry("b.md", "arch", 2)]);
        picker.handle_key(key_event(KeyCode::Down));
        assert!(!picker.take_stale());
    }

    #[test]
    fn deleting_takes_two_presses() {
        let mut picker = opened(vec![entry("a.md", "arch", 1)]);
        assert!(matches!(
            picker.handle_key(key::DELETE.to_key_event()),
            MemoryPickerAction::Consumed
        ));
        let MemoryPickerAction::Delete(name) = picker.handle_key(key::DELETE.to_key_event()) else {
            panic!("the second press deletes");
        };
        assert_eq!(name, "a.md");
    }

    /// Otherwise a stray second press drops whatever row the cursor landed on.
    #[test]
    fn moving_the_cursor_disarms_a_pending_delete() {
        let mut picker = opened(vec![entry("a.md", "arch", 2), entry("b.md", "arch", 2)]);
        picker.handle_key(key::DELETE.to_key_event());
        picker.handle_key(key_event(KeyCode::Down));
        assert!(matches!(
            picker.handle_key(key::DELETE.to_key_event()),
            MemoryPickerAction::Consumed
        ));
    }

    #[test]
    fn an_empty_list_has_nothing_to_open_or_delete() {
        let mut picker = opened(Vec::new());
        assert!(matches!(
            picker.handle_key(key_event(KeyCode::Enter)),
            MemoryPickerAction::Consumed
        ));
        assert!(matches!(
            picker.handle_key(key::DELETE.to_key_event()),
            MemoryPickerAction::Consumed
        ));
    }

    #[test]
    fn escape_closes_the_picker() {
        let mut picker = opened(vec![entry("a.md", "arch", 1)]);
        assert!(matches!(
            picker.handle_key(key_event(KeyCode::Esc)),
            MemoryPickerAction::Closed
        ));
    }

    #[test]
    fn reopening_replaces_the_items_without_closing() {
        let mut picker = opened(vec![entry("a.md", "arch", 1)]);
        picker.open(vec![entry("b.md", "arch", 1)]);
        assert!(picker.is_open());
        let MemoryPickerAction::Open(name) = picker.handle_key(key_event(KeyCode::Enter)) else {
            panic!("the refreshed list is selectable");
        };
        assert_eq!(name, "b.md");
    }

    #[test]
    fn a_refresh_disarms_a_pending_delete() {
        let mut picker = opened(vec![entry("a.md", "arch", 1)]);
        picker.handle_key(key::DELETE.to_key_event());
        picker.open(vec![entry("a.md", "arch", 1)]);
        assert!(matches!(
            picker.handle_key(key::DELETE.to_key_event()),
            MemoryPickerAction::Consumed
        ));
    }
}
