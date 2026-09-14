use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::Overlay;
use super::modal::Modal;
use super::text_editor::{EditorKey, EditorMouse, TextEditor};
use crate::input_document::{PasteId, paste_summary_label};
use crate::theme;

const MODAL_TITLE: &str = " Edit pasted text ";
const MODAL_WIDTH_PERCENT: u16 = 80;
const MODAL_MAX_HEIGHT_PERCENT: u16 = 80;
const META_ROWS: u16 = 1;
const HINT_ROWS: u16 = 1;
const MIN_EDITOR_ROWS: u16 = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PasteEditorTarget {
    Main,
    Subagent(String),
}

pub(crate) enum PasteEditorAction {
    Consumed,
    Save {
        target: PasteEditorTarget,
        id: PasteId,
        text: String,
    },
    /// Text the host should put on the system clipboard.
    Copy(String),
    Cancel,
    /// Nothing here wanted the key, so the app keeps whatever it means
    /// globally. `Ctrl+C` with nothing selected is the case that matters.
    Passthrough,
}

pub(crate) struct PasteEditor {
    target: Option<PasteEditorTarget>,
    id: Option<PasteId>,
    editor: TextEditor,
}

impl PasteEditor {
    pub fn new() -> Self {
        Self {
            target: None,
            id: None,
            editor: TextEditor::new(),
        }
    }

    pub fn open(&mut self, target: PasteEditorTarget, id: PasteId, text: String) {
        self.target = Some(target);
        self.id = Some(id);
        self.editor.set_text(text);
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> PasteEditorAction {
        let (Some(target), Some(id)) = (self.target.clone(), self.id) else {
            return PasteEditorAction::Consumed;
        };
        if key.code == KeyCode::Esc {
            return PasteEditorAction::Cancel;
        }
        if key.code == KeyCode::Char('s') && key.modifiers == KeyModifiers::CONTROL {
            let text = self.editor.text();
            return if text.trim().is_empty() {
                PasteEditorAction::Consumed
            } else {
                PasteEditorAction::Save { target, id, text }
            };
        }
        match self.editor.handle_key(key) {
            EditorKey::Consumed => PasteEditorAction::Consumed,
            EditorKey::Copy(text) => PasteEditorAction::Copy(text),
            EditorKey::Passthrough => PasteEditorAction::Passthrough,
        }
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        if self.id.is_none() {
            return false;
        }
        self.editor.handle_paste(text);
        true
    }

    pub fn handle_mouse(&mut self, event: &MouseEvent) -> PasteEditorAction {
        if self.id.is_none() {
            return PasteEditorAction::Passthrough;
        }
        match self.editor.handle_mouse(event) {
            EditorMouse::Consumed => PasteEditorAction::Consumed,
            EditorMouse::Copy(text) => PasteEditorAction::Copy(text),
            EditorMouse::Passthrough => PasteEditorAction::Passthrough,
        }
    }

    pub fn scroll(&mut self, delta: i32) {
        self.editor.scroll(delta);
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        if self.id.is_none() {
            return Rect::default();
        }

        let modal = Modal {
            title: MODAL_TITLE,
            width_percent: MODAL_WIDTH_PERCENT,
            max_height_percent: MODAL_MAX_HEIGHT_PERCENT,
        };
        let desired = area
            .height
            .saturating_mul(MODAL_MAX_HEIGHT_PERCENT)
            .div_ceil(100)
            .saturating_sub(2)
            .max(MIN_EDITOR_ROWS + META_ROWS + HINT_ROWS);
        let (popup, inner) = modal.render(frame, area, desired);
        let [meta_area, editor_area, hint_area] = Layout::vertical([
            Constraint::Length(META_ROWS),
            Constraint::Min(MIN_EDITOR_ROWS),
            Constraint::Length(HINT_ROWS),
        ])
        .areas(inner);

        self.render_meta(frame, meta_area);
        self.editor.view(frame, editor_area);
        self.render_hint(frame, hint_area);
        popup
    }

    fn render_meta(&self, frame: &mut Frame, area: Rect) {
        let text = self.editor.text();
        let line = Line::from(vec![
            Span::styled(
                format!(" {} ", paste_summary_label(&text)),
                theme::current().active,
            ),
            Span::styled(
                format!("{} chars", text.chars().count()),
                theme::current().item_desc,
            ),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }

    fn render_hint(&self, frame: &mut Frame, area: Rect) {
        frame.render_widget(
            Paragraph::new(super::hint_line(&[
                ("Ctrl+S", "save"),
                ("Ctrl+A", "select all"),
                ("Ctrl+Z", "undo"),
                ("Esc", "cancel"),
            ])),
            area,
        );
    }
}

impl Overlay for PasteEditor {
    fn is_open(&self) -> bool {
        self.id.is_some()
    }

    fn close(&mut self) {
        self.target = None;
        self.id = None;
        self.editor = TextEditor::new();
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::{PasteEditor, PasteEditorAction, PasteEditorTarget};
    use crate::input_document::InputDocument;

    const MULTILINE: &str = "a\nb\nc";

    fn paste_id() -> crate::input_document::PasteId {
        let mut document = InputDocument::new();
        document.insert_paste(MULTILINE)
    }

    fn editor(text: &str) -> PasteEditor {
        let mut editor = PasteEditor::new();
        editor.open(PasteEditorTarget::Main, paste_id(), text.to_owned());
        editor
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn saves_edited_multiline_text() {
        let mut editor = editor(MULTILINE);
        editor.handle_key(key(KeyCode::End, KeyModifiers::NONE));
        editor.handle_key(key(KeyCode::Char('!'), KeyModifiers::NONE));
        let action = editor.handle_key(key(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert!(matches!(
            action,
            PasteEditorAction::Save { text, .. } if text == "a!\nb\nc"
        ));
    }

    #[test]
    fn blank_text_cannot_be_saved() {
        let mut editor = editor(" ");
        let action = editor.handle_key(key(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert!(matches!(action, PasteEditorAction::Consumed));
    }

    #[test]
    fn selecting_all_and_copying_hands_the_text_to_the_host() {
        let mut editor = editor(MULTILINE);
        editor.handle_key(key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        let action = editor.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(matches!(action, PasteEditorAction::Copy(text) if text == MULTILINE));
    }

    #[test]
    fn copy_without_a_selection_is_left_to_the_app() {
        let mut editor = editor(MULTILINE);
        let action = editor.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(matches!(action, PasteEditorAction::Passthrough));
    }

    #[test]
    fn undo_takes_back_an_edit() {
        let mut editor = editor(MULTILINE);
        editor.handle_key(key(KeyCode::Char('!'), KeyModifiers::NONE));
        editor.handle_key(key(KeyCode::Char('z'), KeyModifiers::CONTROL));
        let action = editor.handle_key(key(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert!(matches!(
            action,
            PasteEditorAction::Save { text, .. } if text == MULTILINE
        ));
    }
}
