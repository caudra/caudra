use crate::components::Overlay;
use crate::components::list_picker::{ListPicker, PickerAction};
use crate::repaint::Cadence;
use crate::theme;

use caudra_grab::grab_scope;
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};

const TITLE: &str = " Themes ";
const MAX_VISIBLE: u16 = 15;

pub enum ThemePickerAction {
    Consumed,
    Closed,
    Copy(String),
}

pub struct ThemePicker {
    picker: ListPicker<String>,
}

impl ThemePicker {
    pub fn new() -> Self {
        Self {
            picker: ListPicker::new().with_max_visible(MAX_VISIBLE),
        }
    }

    pub fn open(&mut self) {
        let current_name = theme::current_theme_name();
        let entries = theme::all_theme_names();
        let current_idx = entries
            .iter()
            .position(|name| *name == current_name)
            .unwrap_or(0);
        self.picker.open(entries, TITLE);
        self.picker.select(current_idx);
    }

    pub fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub fn close(&mut self) {
        self.picker.close();
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ThemePickerAction {
        let action = self.picker.handle_key(key);
        self.map_picker_action(action)
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> ThemePickerAction {
        let action = self.picker.handle_mouse(event);
        self.map_picker_action(action)
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.picker.contains(pos)
    }

    pub fn cancel(&mut self) -> ThemePickerAction {
        self.map_picker_action(PickerAction::Close)
    }

    /// The wheel moves the viewport, not the selection, so the previewed theme
    /// is still the selected one and needs no reapplying.
    pub fn scroll(&mut self, delta: i32) {
        self.picker.scroll(delta);
    }

    fn map_picker_action(&mut self, action: PickerAction<String>) -> ThemePickerAction {
        match action {
            PickerAction::Consumed => {
                self.apply_preview();
                ThemePickerAction::Consumed
            }
            PickerAction::Select(name) => {
                if let Ok(selected) = theme::load_by_name(&name) {
                    theme::set(selected);
                }
                theme::persist_theme(&name);
                ThemePickerAction::Closed
            }
            PickerAction::Close => {
                self.restore_committed();
                ThemePickerAction::Closed
            }
            PickerAction::Toggle(..) | PickerAction::Key(_) => ThemePickerAction::Consumed,
            PickerAction::Copy(text) => {
                self.apply_preview();
                ThemePickerAction::Copy(text)
            }
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        grab_scope!("theme_picker", area);
        self.picker.view(frame, area)
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        let consumed = self.picker.handle_paste(text);
        if consumed {
            self.apply_preview();
        }
        consumed
    }

    fn apply_preview(&self) {
        if let Some(name) = self.picker.selected_item()
            && let Ok(t) = theme::load_by_name(name)
        {
            theme::set(t);
        }
    }

    fn restore_committed(&self) {
        if let Ok(t) = theme::load_by_name(&theme::current_theme_name()) {
            theme::set(t);
        }
    }
}

impl Overlay for ThemePicker {
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
    use crate::components::key;
    use crate::components::keybindings::key as kb;
    use crossterm::event::KeyCode;
    use test_case::test_case;

    const DARK_THEME: &str = "opencode";
    const LIGHT_THEME: &str = "opencode_light";

    #[test_case(key(KeyCode::Esc); "escape")]
    #[test_case(kb::QUIT.to_key_event(); "ctrl_c")]
    fn cancel_after_appearance_change_restores_latest_palette(cancel_key: KeyEvent) {
        theme::set_current_name(DARK_THEME);
        theme::set(theme::load_by_name(DARK_THEME).unwrap());
        let mut picker = ThemePicker::new();
        picker.open();
        picker.handle_key(key(KeyCode::Down));

        theme::set_current_name(LIGHT_THEME);
        theme::set(theme::load_by_name(LIGHT_THEME).unwrap());

        assert!(matches!(
            picker.handle_key(cancel_key),
            ThemePickerAction::Closed
        ));
        assert!(!picker.is_open());
        assert_eq!(theme::current_theme_name(), LIGHT_THEME);
        let selected = theme::load_by_name(LIGHT_THEME).unwrap();
        assert_eq!(theme::current().background, selected.background);
        assert_eq!(theme::current().foreground, selected.foreground);
    }

    #[test]
    fn enter_after_appearance_change_applies_selection_and_closes() {
        let selected_name = theme::current_theme_name();
        let mut p = ThemePicker::new();
        p.open();
        let reported_name = if selected_name == LIGHT_THEME {
            DARK_THEME
        } else {
            LIGHT_THEME
        };
        theme::set_current_name(reported_name);
        theme::set(theme::load_by_name(reported_name).unwrap());
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(matches!(action, ThemePickerAction::Closed));
        assert!(!p.is_open());
        assert_eq!(theme::current_theme_name(), selected_name);
        let selected = theme::load_by_name(&selected_name).unwrap();
        assert_eq!(theme::current().background, selected.background);
        assert_eq!(theme::current().foreground, selected.foreground);
    }

    #[test_case(key(KeyCode::Esc) ; "escape_restores_and_closes")]
    #[test_case(kb::QUIT.to_key_event() ; "ctrl_c_restores_and_closes")]
    fn cancel_restores(cancel_key: crossterm::event::KeyEvent) {
        let mut p = ThemePicker::new();
        p.open();
        p.handle_key(key(KeyCode::Down));
        let action = p.handle_key(cancel_key);
        assert!(matches!(action, ThemePickerAction::Closed));
        assert!(!p.is_open());
    }
}
