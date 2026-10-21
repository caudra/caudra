//! The form `/settings` opens over its list to change a key that holds a list of strings: a
//! checklist when the key names its choices, else the items in order with a line to add one.

use caudra_workbench::text_field::{FieldKind, TextField};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::components::modal::Modal;
use crate::components::{field_styles, input_text_style, selectable_row};
use crate::theme;

const WIDTH_PERCENT: u16 = 60;
const MAX_HEIGHT_PERCENT: u16 = 60;
const CHECKED: &str = "[x] ";
const UNCHECKED: &str = "[ ] ";
const CURSOR_MARK: &str = "\u{203a} ";
const NO_MARK: &str = "  ";
const ADD_LABEL: &str = "Add: ";
const NO_ITEMS: &str = "(empty)";
const CHECKLIST_HINT: &str = "Space checks or unchecks, Enter saves, Esc cancels.";
const ITEMS_HINT: &str = "Type and press Enter to add. ↑/↓ select, Shift+↑/↓ move, Delete removes the selected item while the line is empty. Enter on an empty line saves, Esc cancels.";
pub(super) const DUPLICATE: &str = "The list already holds that.";
/// The line under the rows that says why an item was refused.
const MESSAGE_ROWS: u16 = 1;

pub(super) enum ListKey {
    Save(Vec<String>),
    Cancel,
    Consumed,
}

pub(super) struct ListForm {
    title: String,
    description: &'static str,
    items: Vec<String>,
    /// What a checklist offers, in the order it lists them; empty for free text.
    choices: &'static [&'static str],
    selected: usize,
    input: TextField,
    message: Option<&'static str>,
    area: Rect,
}

impl ListForm {
    pub(super) fn new(
        title: String,
        description: &'static str,
        items: Vec<String>,
        choices: &'static [&'static str],
    ) -> Self {
        Self {
            title,
            description,
            items,
            choices,
            selected: 0,
            input: TextField::new(FieldKind::Line),
            message: None,
            area: Rect::default(),
        }
    }

    /// Whether typed text goes to the line that adds an item.
    pub(super) fn takes_text(&self) -> bool {
        self.choices.is_empty()
    }

    pub(super) fn contains(&self, position: Position) -> bool {
        self.area.contains(position)
    }

    fn rows(&self) -> usize {
        match self.takes_text() {
            true => self.items.len(),
            false => self.choices.len(),
        }
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent) -> ListKey {
        let moving = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Esc => return ListKey::Cancel,
            KeyCode::Up if moving => self.move_selected(false),
            KeyCode::Down if moving => self.move_selected(true),
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(self.rows().saturating_sub(1));
            }
            KeyCode::Char(' ') if !self.takes_text() => self.toggle(),
            KeyCode::Enter => return self.enter(),
            KeyCode::Delete if self.takes_text() && self.input.text().is_empty() => {
                self.remove_selected();
            }
            _ if self.takes_text() && self.input.handle_key(key).changed() => self.message = None,
            _ => {}
        }
        ListKey::Consumed
    }

    pub(super) fn handle_text(&mut self, text: &str, paste: bool) {
        if self.takes_text() && self.input.handle_text(text, paste).changed() {
            self.message = None;
        }
    }

    fn enter(&mut self) -> ListKey {
        let text = self.input.text().trim().to_owned();
        if !self.takes_text() || text.is_empty() {
            return ListKey::Save(self.items.clone());
        }
        if self.items.contains(&text) {
            self.message = Some(DUPLICATE);
            return ListKey::Consumed;
        }
        self.items.push(text);
        self.selected = self.items.len() - 1;
        self.input.clear();
        ListKey::Consumed
    }

    fn toggle(&mut self) {
        let Some(choice) = self.choices.get(self.selected) else {
            return;
        };
        match self.items.iter().position(|item| item == choice) {
            Some(index) => {
                self.items.remove(index);
            }
            None => {
                self.items.push((*choice).to_owned());
                let order = |item: &String| self.choices.iter().position(|choice| choice == item);
                self.items.sort_by_key(order);
            }
        }
    }

    fn move_selected(&mut self, down: bool) {
        if !self.takes_text() {
            return;
        }
        let target = match down {
            true => self.selected + 1,
            false => self.selected.wrapping_sub(1),
        };
        if target < self.items.len() {
            self.items.swap(self.selected, target);
            self.selected = target;
        }
    }

    fn remove_selected(&mut self) {
        if self.selected < self.items.len() {
            self.items.remove(self.selected);
            self.selected = self.selected.min(self.items.len().saturating_sub(1));
        }
    }

    pub(super) fn view(&mut self, frame: &mut Frame, area: Rect) {
        let t = theme::current();
        let width = Modal::inner_width(area.width, WIDTH_PERCENT);
        let hint_text = match self.takes_text() {
            true => ITEMS_HINT,
            false => CHECKLIST_HINT,
        };
        let hint = Paragraph::new(Line::from(Span::styled(hint_text, t.input_placeholder)))
            .wrap(Wrap { trim: false });
        let hint_rows = u16::try_from(hint.line_count(width.max(1))).unwrap_or(u16::MAX);
        let description = Line::styled(self.description, t.item_desc);
        let description_rows = Paragraph::new(description.clone())
            .wrap(Wrap { trim: false })
            .line_count(width.max(1));
        let description_rows = u16::try_from(description_rows).unwrap_or(u16::MAX);
        let input_rows = u16::from(self.takes_text());
        let item_rows = u16::try_from(self.rows().max(1)).unwrap_or(u16::MAX);
        let modal = Modal {
            title: &self.title,
            width_percent: WIDTH_PERCENT,
            max_height_percent: MAX_HEIGHT_PERCENT,
        };
        let (popup, inner) = modal.render(
            frame,
            area,
            hint_rows
                .saturating_add(item_rows)
                .saturating_add(input_rows)
                .saturating_add(MESSAGE_ROWS)
                .saturating_add(description_rows),
        );
        self.area = popup;
        let mut lines: Vec<Line<'static>> = match self.takes_text() {
            true if self.items.is_empty() => vec![Line::styled(NO_ITEMS, t.tool_dim)],
            true => self
                .items
                .iter()
                .enumerate()
                .map(|(index, item)| self.row(index, vec![Span::raw(item.clone())]))
                .collect(),
            false => self
                .choices
                .iter()
                .enumerate()
                .map(|(index, choice)| {
                    let check = match self.items.iter().any(|item| item == choice) {
                        true => CHECKED,
                        false => UNCHECKED,
                    };
                    self.row(
                        index,
                        vec![Span::styled(check, t.accent), Span::raw(*choice)],
                    )
                })
                .collect(),
        };
        if self.takes_text() {
            let room = usize::from(inner.width).saturating_sub(ADD_LABEL.width());
            let mut line = self
                .input
                .paint(room, &field_styles(input_text_style()), true, "");
            line.spans.insert(0, Span::styled(ADD_LABEL, t.tool_dim));
            lines.push(line);
        }
        lines.push(match self.message {
            Some(text) => Line::styled(text, t.error),
            None => Line::default(),
        });
        lines.push(description);
        let [hint_area, body] =
            Layout::vertical([Constraint::Length(hint_rows), Constraint::Min(1)]).areas(inner);
        frame.render_widget(hint, hint_area);
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .style(t.surface_style()),
            body,
        );
    }

    fn row(&self, index: usize, spans: Vec<Span<'static>>) -> Line<'static> {
        let t = theme::current();
        let selected = index == self.selected;
        let mark = match selected {
            true => CURSOR_MARK,
            false => NO_MARK,
        };
        let mut row = vec![Span::styled(mark, t.accent)];
        row.extend(spans);
        selectable_row(row, selected.then_some(t.item_selected), false)
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const TITLE: &str = " list ";
    const FIRST: &str = "first";
    const SECOND: &str = "second";
    const CHOICES: &[&str] = &["project", "local", "memory"];

    fn press(form: &mut ListForm, code: KeyCode, modifiers: KeyModifiers) -> ListKey {
        form.handle_key(KeyEvent::new(code, modifiers))
    }

    fn saved(key: ListKey) -> Vec<String> {
        match key {
            ListKey::Save(items) => items,
            _ => panic!("expected a save"),
        }
    }

    fn items(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn enter_adds_the_typed_item_then_saves_on_an_empty_line() {
        let mut form = ListForm::new(TITLE.to_owned(), "", items(&[FIRST]), &[]);
        form.handle_text(SECOND, false);
        assert!(matches!(
            press(&mut form, KeyCode::Enter, KeyModifiers::NONE),
            ListKey::Consumed
        ));
        let list = saved(press(&mut form, KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(list, items(&[FIRST, SECOND]));
    }

    #[test]
    fn a_duplicate_is_refused() {
        let mut form = ListForm::new(TITLE.to_owned(), "", items(&[FIRST]), &[]);
        form.handle_text(FIRST, false);
        press(&mut form, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(form.message, Some(DUPLICATE));
        assert_eq!(form.items, items(&[FIRST]));
    }

    #[test_case(KeyCode::Down, KeyModifiers::SHIFT, &[SECOND, FIRST]; "shift_down_moves")]
    #[test_case(KeyCode::Delete, KeyModifiers::NONE, &[SECOND]; "delete_removes")]
    fn the_selected_item_changes(code: KeyCode, modifiers: KeyModifiers, expected: &[&str]) {
        let mut form = ListForm::new(TITLE.to_owned(), "", items(&[FIRST, SECOND]), &[]);
        press(&mut form, code, modifiers);
        let list = saved(press(&mut form, KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(list, items(expected));
    }

    #[test]
    fn a_checklist_keeps_the_choice_order() {
        let mut form = ListForm::new(TITLE.to_owned(), "", items(&["memory"]), CHOICES);
        assert!(!form.takes_text());
        press(&mut form, KeyCode::Char(' '), KeyModifiers::NONE);
        let list = saved(press(&mut form, KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(list, items(&["project", "memory"]));
    }
}
