use caudra_storage::prompt_stash::StashEntry;
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::Line;

use crate::components::keybindings::key;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::components::{Overlay, hint_line};
use crate::repaint::Cadence;

const TITLE: &str = " Stash ";
const MAX_VISIBLE: u16 = 15;
const EMPTY_TEXT: &str = "Nothing stashed";
const BLANK_LABEL: &str = "(blank prompt)";
const MINUTE: u64 = 60;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;

pub enum StashPickerAction {
    Consumed,
    Restore(Box<StashEntry>),
    Delete(String),
    Closed,
}

struct StashItem {
    entry: StashEntry,
    label: String,
    suffix: Option<String>,
    detail: String,
}

impl PickerItem for StashItem {
    fn label(&self) -> &str {
        &self.label
    }

    fn suffix(&self) -> Option<&str> {
        self.suffix.as_deref()
    }

    fn detail(&self) -> Option<&str> {
        Some(&self.detail)
    }
}

pub struct StashPicker {
    picker: ListPicker<StashItem>,
    pending_delete: Option<String>,
}

impl StashPicker {
    pub fn new() -> Self {
        let mut picker = ListPicker::new()
            .with_max_visible(MAX_VISIBLE)
            .with_footer_builder(footer);
        picker.set_empty_text(EMPTY_TEXT);
        Self {
            picker,
            pending_delete: None,
        }
    }

    /// Newest first, so the entry `/stash-pop` would take sits at the top.
    pub fn open(&mut self, entries: Vec<StashEntry>, now: u64) {
        let items = build_items(entries, now);
        self.pending_delete = None;
        self.picker.set_info_text(None);
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
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.picker.contains(pos)
    }

    pub fn scroll(&mut self, delta: i32) {
        self.picker.scroll(delta);
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> StashPickerAction {
        // `ListPicker` swallows every unbound control key, so the delete
        // binding has to be claimed before the list sees it.
        if key::DELETE.matches(key) {
            return self.press_delete();
        }
        self.clear_pending();
        let action = self.picker.handle_key(key);
        self.map_action(action)
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> StashPickerAction {
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

    fn press_delete(&mut self) -> StashPickerAction {
        let Some(id) = self
            .picker
            .selected_item()
            .map(|item| item.entry.id.clone())
        else {
            return StashPickerAction::Consumed;
        };
        if self.pending_delete.as_deref() == Some(id.as_str()) {
            self.clear_pending();
            return StashPickerAction::Delete(id);
        }
        self.pending_delete = Some(id);
        self.picker.set_info_text(Some(confirm_hint()));
        StashPickerAction::Consumed
    }

    /// Moving off the armed row cancels the delete, so a stray `Ctrl+D` later
    /// cannot drop an entry the user is no longer looking at.
    fn clear_pending(&mut self) {
        if self.pending_delete.take().is_some() {
            self.picker.set_info_text(None);
        }
    }

    fn map_action(&mut self, action: PickerAction<StashItem>) -> StashPickerAction {
        match action {
            PickerAction::Consumed | PickerAction::Toggle(..) => StashPickerAction::Consumed,
            PickerAction::Select(item) => StashPickerAction::Restore(Box::new(item.entry)),
            PickerAction::Close => {
                self.pending_delete = None;
                StashPickerAction::Closed
            }
        }
    }
}

impl Overlay for StashPicker {
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
        ("Enter", "restore"),
        (key::DELETE.label, "delete"),
        ("Esc", "close"),
    ])
}

fn confirm_hint() -> String {
    format!("Press {} again to delete this entry.", key::DELETE.label)
}

fn build_items(entries: Vec<StashEntry>, now: u64) -> Vec<StashItem> {
    entries
        .into_iter()
        .rev()
        .map(|entry| StashItem {
            label: preview(&entry.text),
            suffix: suffix(&entry),
            detail: detail(&entry, now),
            entry,
        })
        .collect()
}

fn preview(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(BLANK_LABEL)
        .to_string()
}

/// What the one-line preview leaves out: how much more text there is and
/// whether images came along.
fn suffix(entry: &StashEntry) -> Option<String> {
    let lines = entry
        .text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    let mut parts = Vec::new();
    if lines > 1 {
        parts.push(format!("+{} lines", lines - 1));
    }
    match entry.images.len() {
        0 => {}
        1 => parts.push("1 image".into()),
        count => parts.push(format!("{count} images")),
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

fn detail(entry: &StashEntry, now: u64) -> String {
    let age = age(now.saturating_sub(entry.created_at));
    match origin(&entry.cwd) {
        Some(origin) => format!("{origin} · {age}"),
        None => age,
    }
}

fn origin(cwd: &str) -> Option<&str> {
    cwd.trim_end_matches('/')
        .rsplit('/')
        .find(|part| !part.is_empty())
}

fn age(seconds: u64) -> String {
    for (unit, label) in [(DAY, "d"), (HOUR, "h"), (MINUTE, "m")] {
        if seconds >= unit {
            return format!("{}{label} ago", seconds / unit);
        }
    }
    "just now".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::key as key_event;
    use caudra_storage::sessions::StoredImage;
    use crossterm::event::KeyCode;
    use test_case::test_case;

    const NOW: u64 = 1_000_000;

    fn entry(id: &str, text: &str) -> StashEntry {
        StashEntry {
            id: id.into(),
            text: text.into(),
            paste_ranges: Vec::new(),
            images: Vec::new(),
            cwd: "/home/dev/project".into(),
            created_at: NOW,
        }
    }

    fn opened(entries: Vec<StashEntry>) -> StashPicker {
        let mut picker = StashPicker::new();
        picker.open(entries, NOW);
        picker
    }

    #[test]
    fn newest_entry_is_listed_first() {
        let picker = opened(vec![entry("a", "older"), entry("b", "newer")]);
        assert_eq!(picker.picker.selected_item().unwrap().entry.id, "b");
    }

    #[test]
    fn enter_restores_the_selected_entry() {
        let mut picker = opened(vec![entry("a", "draft")]);
        let action = picker.handle_key(key_event(KeyCode::Enter));
        assert!(matches!(action, StashPickerAction::Restore(e) if e.id == "a"));
        assert!(!picker.is_open());
    }

    #[test]
    fn delete_needs_two_presses() {
        let mut picker = opened(vec![entry("a", "draft")]);
        assert!(matches!(
            picker.handle_key(bind_delete()),
            StashPickerAction::Consumed
        ));
        assert!(picker.is_open());
        assert!(matches!(
            picker.handle_key(bind_delete()),
            StashPickerAction::Delete(id) if id == "a"
        ));
    }

    #[test]
    fn moving_the_selection_cancels_a_pending_delete() {
        let mut picker = opened(vec![entry("a", "older"), entry("b", "newer")]);
        picker.handle_key(bind_delete());
        picker.handle_key(key_event(KeyCode::Down));
        assert!(matches!(
            picker.handle_key(bind_delete()),
            StashPickerAction::Consumed
        ));
    }

    #[test]
    fn esc_closes_without_restoring() {
        let mut picker = opened(vec![entry("a", "draft")]);
        assert!(matches!(
            picker.handle_key(key_event(KeyCode::Esc)),
            StashPickerAction::Closed
        ));
        assert!(!picker.is_open());
    }

    #[test]
    fn reopening_keeps_the_active_search() {
        let mut picker = opened(vec![entry("a", "alpha"), entry("b", "beta")]);
        picker.handle_key(key_event(KeyCode::Char('b')));
        picker.open(vec![entry("b", "beta")], NOW);
        assert_eq!(picker.picker.search_text(), "b");
    }

    #[test]
    fn preview_skips_leading_blank_lines() {
        assert_eq!(preview("\n\n  hello  \nworld"), "hello");
        assert_eq!(preview("   "), BLANK_LABEL);
    }

    #[test]
    fn suffix_counts_extra_lines_and_images() {
        let mut multi = entry("a", "one\ntwo\nthree");
        assert_eq!(suffix(&multi).as_deref(), Some("+2 lines"));

        multi.images.push(StoredImage {
            media_type: "image/png".into(),
            data: "AAAA".into(),
        });
        assert_eq!(suffix(&multi).as_deref(), Some("+2 lines, 1 image"));
        assert_eq!(suffix(&entry("a", "single")), None);
    }

    #[test]
    fn detail_shows_the_origin_directory() {
        assert_eq!(detail(&entry("a", "draft"), NOW + 90), "project · 1m ago");
    }

    #[test_case(0, "just now")]
    #[test_case(59, "just now")]
    #[test_case(60, "1m ago")]
    #[test_case(3_600, "1h ago")]
    #[test_case(90_000, "1d ago")]
    fn age_picks_the_largest_whole_unit(seconds: u64, expected: &str) {
        assert_eq!(age(seconds), expected);
    }

    fn bind_delete() -> KeyEvent {
        KeyEvent::new(key::DELETE.code, key::DELETE.modifiers)
    }
}
