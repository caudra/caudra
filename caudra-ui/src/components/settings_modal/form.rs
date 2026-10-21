//! The form `/settings` opens over its list to change one setting, or every setting of a
//! web search backend. Credentials stay masked until the form is told to reveal them.

use caudra_workbench::text_field::{FieldKind, TextField};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::components::keybindings::key;
use crate::components::modal::Modal;
use crate::components::{field_styles, input_text_style};
use crate::theme;

const WIDTH_PERCENT: u16 = 60;
const MAX_HEIGHT_PERCENT: u16 = 60;
const LABEL_GAP: usize = 2;
const READS_PREFIX: &str = "reads ";
/// The message line and the focused field's description, besides the fields and the hint.
const FIXED_ROWS: u16 = 2;

pub(super) enum FormKey {
    Submit,
    Cancel,
    Consumed,
}

enum Message {
    Warning(String),
    Error(String),
}

/// One key the form edits, with the text it held when the form opened.
pub(super) struct FormField {
    pub(super) key: &'static str,
    pub(super) env: Option<&'static str>,
    pub(super) description: &'static str,
    pub(super) secret: bool,
    initial: String,
    input: TextField,
}

impl FormField {
    pub(super) fn new(
        key: &'static str,
        env: Option<&'static str>,
        description: &'static str,
        secret: bool,
        initial: &str,
    ) -> Self {
        let mut input = TextField::with_text(FieldKind::Line, initial);
        input.set_secret(secret);
        Self {
            key,
            env,
            description,
            secret,
            initial: initial.to_owned(),
            input,
        }
    }

    pub(super) fn text(&self) -> String {
        self.input.text().trim().to_owned()
    }

    fn changed(&self) -> bool {
        self.text() != self.initial
    }
}

pub(super) struct SettingsForm {
    title: String,
    pub(super) fields: Vec<FormField>,
    focus: usize,
    revealed: bool,
    hint: &'static str,
    message: Option<Message>,
    /// A warning shown for exactly this content, so the next Enter saves anyway.
    acknowledged: bool,
    area: Rect,
}

impl SettingsForm {
    pub(super) fn new(title: String, fields: Vec<FormField>, hint: &'static str) -> Self {
        Self {
            title,
            fields,
            focus: 0,
            revealed: false,
            hint,
            message: None,
            acknowledged: false,
            area: Rect::default(),
        }
    }

    /// The fields whose text changed, each with its new value, `None` once emptied.
    pub(super) fn changes(&self) -> Vec<(&FormField, Option<String>)> {
        self.fields
            .iter()
            .filter(|field| field.changed())
            .map(|field| {
                let text = field.text();
                (field, (!text.is_empty()).then_some(text))
            })
            .collect()
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
        if key::REVEAL.matches(key) {
            self.revealed = !self.revealed;
            for field in &mut self.fields {
                field.input.set_secret(field.secret && !self.revealed);
            }
            return FormKey::Consumed;
        }
        match key.code {
            KeyCode::Enter => return FormKey::Submit,
            KeyCode::Esc => return FormKey::Cancel,
            KeyCode::Tab | KeyCode::Down => return self.step(1),
            KeyCode::BackTab | KeyCode::Up => return self.step(self.fields.len() - 1),
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

    fn step(&mut self, by: usize) -> FormKey {
        self.focus = (self.focus + by) % self.fields.len();
        FormKey::Consumed
    }

    fn focused(&mut self) -> &mut TextField {
        &mut self.fields[self.focus].input
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
        let field_rows = u16::try_from(self.fields.len()).unwrap_or(u16::MAX);
        let modal = Modal {
            title: &self.title,
            width_percent: WIDTH_PERCENT,
            max_height_percent: MAX_HEIGHT_PERCENT,
        };
        let (popup, inner) = modal.render(
            frame,
            area,
            hint_rows
                .saturating_add(field_rows)
                .saturating_add(FIXED_ROWS),
        );
        self.area = popup;
        let label_cols = self
            .fields
            .iter()
            .map(|field| field.key.width())
            .max()
            .unwrap_or_default()
            + LABEL_GAP;
        let mut lines: Vec<Line<'static>> = self
            .fields
            .iter()
            .enumerate()
            .map(|(index, field)| field_line(field, index == self.focus, label_cols, inner.width))
            .collect();
        lines.push(match &self.message {
            Some(Message::Warning(text)) => Line::from(Span::styled(text.clone(), t.tool_warning)),
            Some(Message::Error(text)) => Line::from(Span::styled(text.clone(), t.error)),
            None => Line::default(),
        });
        lines.push(Line::from(Span::styled(
            self.fields[self.focus].description,
            t.item_desc,
        )));
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

fn field_line(field: &FormField, focused: bool, label_cols: usize, width: u16) -> Line<'static> {
    let t = theme::current();
    let label = format!("{:<label_cols$}", field.key);
    let room = usize::from(width).saturating_sub(label_cols);
    let placeholder = field
        .env
        .map(|env| format!("{READS_PREFIX}{env}"))
        .unwrap_or_default();
    let mut line = field.input.paint(
        room,
        &field_styles(input_text_style()),
        focused,
        &placeholder,
    );
    line.spans.insert(0, Span::styled(label, t.tool_dim));
    line
}
