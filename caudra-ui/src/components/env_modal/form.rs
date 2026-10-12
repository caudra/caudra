//! The small form `/env` opens over its list to set a value or add a pass pattern.

use caudra_workbench::text_field::{FieldKind, TextField};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::components::modal::Modal;
use crate::components::{field_styles, input_text_style};
use crate::theme;

const VALUE_TITLE: &str = " Set a variable ";
const PATTERN_TITLE: &str = " Forward a pattern ";
const NAME_LABEL: &str = "Name    ";
const VALUE_LABEL: &str = "Value   ";
const PATTERN_LABEL: &str = "Pattern ";
const WIDTH_PERCENT: u16 = 60;
const MAX_HEIGHT_PERCENT: u16 = 60;
/// The fields and the message line under them, besides the wrapped hint.
const FIXED_ROWS: u16 = 3;

pub(super) enum FormKind {
    Value,
    Pattern,
}

pub(super) enum FormKey {
    Submit,
    Cancel,
    Consumed,
}

enum Message {
    Warning(String),
    Error(String),
}

pub(super) struct EnvForm {
    pub(super) kind: FormKind,
    name: TextField,
    value: TextField,
    name_locked: bool,
    on_value: bool,
    hint: &'static str,
    message: Option<Message>,
    /// A warning shown for exactly this content, so the next Enter saves anyway.
    acknowledged: bool,
    area: Rect,
}

impl EnvForm {
    /// Adds a value, or edits the value of `existing` without renaming it.
    pub(super) fn value(existing: Option<(&str, &str)>, secret: bool, hint: &'static str) -> Self {
        let mut value = TextField::new(FieldKind::Line);
        value.set_secret(secret);
        let mut name = TextField::new(FieldKind::Line);
        if let Some((existing_name, existing_value)) = existing {
            name.set_text(existing_name);
            value.set_text(existing_value);
        }
        Self {
            kind: FormKind::Value,
            name,
            value,
            name_locked: existing.is_some(),
            on_value: existing.is_some(),
            hint,
            message: None,
            acknowledged: false,
            area: Rect::default(),
        }
    }

    pub(super) fn pattern(hint: &'static str) -> Self {
        Self {
            kind: FormKind::Pattern,
            on_value: false,
            name_locked: false,
            ..Self::value(None, false, hint)
        }
    }

    pub(super) fn name(&self) -> String {
        self.name.text().trim().to_owned()
    }

    pub(super) fn value_text(&self) -> String {
        self.value.text()
    }

    /// Shows `warning` and returns true, unless this content was already warned about.
    pub(super) fn warn(&mut self, warning: String) -> bool {
        if self.acknowledged {
            return false;
        }
        self.message = Some(Message::Warning(warning));
        self.acknowledged = true;
        true
    }

    pub(super) fn fail(&mut self, error: String) {
        self.message = Some(Message::Error(error));
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent) -> FormKey {
        match key.code {
            KeyCode::Enter => return FormKey::Submit,
            KeyCode::Esc => return FormKey::Cancel,
            KeyCode::Tab | KeyCode::BackTab => {
                if matches!(self.kind, FormKind::Value) && !self.name_locked {
                    self.on_value = !self.on_value;
                }
                return FormKey::Consumed;
            }
            _ => {}
        }
        if self.focused().handle_key(key).changed() {
            self.edited();
        }
        FormKey::Consumed
    }

    pub(super) fn handle_text(&mut self, text: &str, paste: bool) {
        if self.focused().handle_text(text, paste).changed() {
            self.edited();
        }
    }

    pub(super) fn contains(&self, position: Position) -> bool {
        self.area.contains(position)
    }

    fn focused(&mut self) -> &mut TextField {
        match self.on_value {
            true => &mut self.value,
            false => &mut self.name,
        }
    }

    fn edited(&mut self) {
        self.message = None;
        self.acknowledged = false;
    }

    pub(super) fn view(&mut self, frame: &mut Frame, area: Rect) {
        let t = theme::current();
        let width = Modal::inner_width(area.width, WIDTH_PERCENT);
        let hint = Paragraph::new(Line::from(Span::styled(self.hint, t.input_placeholder)))
            .wrap(Wrap { trim: false });
        let hint_rows = u16::try_from(hint.line_count(width.max(1))).unwrap_or(u16::MAX);
        let (title, first_label) = match self.kind {
            FormKind::Value => (VALUE_TITLE, NAME_LABEL),
            FormKind::Pattern => (PATTERN_TITLE, PATTERN_LABEL),
        };
        let modal = Modal {
            title,
            width_percent: WIDTH_PERCENT,
            max_height_percent: MAX_HEIGHT_PERCENT,
        };
        let (popup, inner) = modal.render(frame, area, hint_rows.saturating_add(FIXED_ROWS));
        self.area = popup;
        let mut lines = vec![field_line(
            first_label,
            &self.name,
            !self.on_value,
            inner.width,
        )];
        if matches!(self.kind, FormKind::Value) {
            lines.push(field_line(
                VALUE_LABEL,
                &self.value,
                self.on_value,
                inner.width,
            ));
        }
        lines.push(match &self.message {
            Some(Message::Warning(text)) => Line::from(Span::styled(text.clone(), t.tool_warning)),
            Some(Message::Error(text)) => Line::from(Span::styled(text.clone(), t.error)),
            None => Line::default(),
        });
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
}

fn field_line(label: &'static str, field: &TextField, focused: bool, width: u16) -> Line<'static> {
    let t = theme::current();
    let room = usize::from(width).saturating_sub(label.width());
    let mut line = field.paint(room, &field_styles(input_text_style()), focused, "");
    line.spans.insert(0, Span::styled(label, t.tool_dim));
    line
}
