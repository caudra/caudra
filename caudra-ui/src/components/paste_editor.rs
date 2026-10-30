use caudra_grab::grab_scope;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::keybindings::key;
use super::modal::Modal;
use super::text_editor::{EditorKey, EditorMouse, TextEditor};
use super::{Hint, HintBar, Overlay};
use crate::input_document::{PasteId, paste_summary_label};
use crate::theme;

const MODAL_TITLE: &str = " Edit pasted text ";
const MODAL_WIDTH_PERCENT: u16 = 80;
const MODAL_MAX_HEIGHT_PERCENT: u16 = 80;
const META_ROWS: u16 = 1;
const HINT_ROWS: u16 = 1;
const MIN_EDITOR_ROWS: u16 = 5;
const DISCARD_PROMPT: &str = "Discard your edits?";
const DISCARD_HINT: &str = "discard";
const KEEP_EDITING_HINT: &str = "keep editing";

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
    hints: HintBar,
    original: String,
    popup: Rect,
    confirm_discard: bool,
}

impl PasteEditor {
    pub fn new() -> Self {
        Self {
            target: None,
            id: None,
            editor: TextEditor::new(),
            hints: HintBar::default(),
            original: String::new(),
            popup: Rect::ZERO,
            confirm_discard: false,
        }
    }

    pub fn open(&mut self, target: PasteEditorTarget, id: PasteId, text: String) {
        self.target = Some(target);
        self.id = Some(id);
        self.editor.set_text(text.clone());
        self.original = text;
    }

    fn contains(&self, pos: Position) -> bool {
        self.is_open() && self.popup.contains(pos)
    }

    fn is_dirty(&self) -> bool {
        self.editor.text() != self.original
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> PasteEditorAction {
        let (Some(target), Some(id)) = (self.target.clone(), self.id) else {
            return PasteEditorAction::Consumed;
        };
        if self.confirm_discard {
            return match key.code {
                KeyCode::Enter => PasteEditorAction::Cancel,
                KeyCode::Esc => {
                    self.confirm_discard = false;
                    PasteEditorAction::Consumed
                }
                _ => PasteEditorAction::Consumed,
            };
        }
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
        self.handle_text(text, true)
    }

    pub fn handle_text(&mut self, text: &str, paste: bool) -> bool {
        if self.id.is_none() {
            return false;
        }
        if !self.confirm_discard {
            self.editor.handle_text(text, paste);
        }
        true
    }

    pub fn handle_mouse(&mut self, event: &MouseEvent) -> PasteEditorAction {
        if self.id.is_none() {
            return PasteEditorAction::Passthrough;
        }
        if self.confirm_discard {
            return match self.hints.handle_mouse(*event) {
                Some(key) => self.handle_key(key),
                None => PasteEditorAction::Consumed,
            };
        }
        if event.kind == MouseEventKind::Down(MouseButton::Left)
            && !self.contains(Position::new(event.column, event.row))
        {
            if !self.is_dirty() {
                return PasteEditorAction::Cancel;
            }
            self.confirm_discard = true;
            return PasteEditorAction::Consumed;
        }
        if let Some(key) = self.hints.handle_mouse(*event) {
            return self.handle_key(key);
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

        grab_scope!("paste_editor", area);
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
        let hints = match self.confirm_discard {
            true => vec![
                Hint::bind(key::ENTER, DISCARD_HINT),
                Hint::bind(key::ESC, KEEP_EDITING_HINT),
            ],
            false => vec![
                Hint::bind(key::SAVE, "save"),
                Hint::bind(key::SELECT_ALL, "select all"),
                Hint::bind(key::UNDO, "undo"),
                Hint::bind(key::ESC, "cancel"),
            ],
        };
        self.hints.draw(frame, hint_area, hints);
        self.popup = popup;
        popup
    }

    fn render_meta(&self, frame: &mut Frame, area: Rect) {
        grab_scope!("paste_editor_meta", area);
        if self.confirm_discard {
            let prompt = Span::styled(format!(" {DISCARD_PROMPT}"), theme::current().error);
            frame.render_widget(Paragraph::new(Line::from(prompt)), area);
            return;
        }
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
}

impl Overlay for PasteEditor {
    fn is_open(&self) -> bool {
        self.id.is_some()
    }

    fn close(&mut self) {
        self.target = None;
        self.id = None;
        self.editor = TextEditor::new();
        self.hints.reset();
        self.original.clear();
        self.popup = Rect::ZERO;
        self.confirm_discard = false;
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use test_case::test_case;

    use super::{DISCARD_PROMPT, PasteEditor, PasteEditorAction, PasteEditorTarget};
    use crate::components::{Overlay, buffer_text};
    use crate::input_document::InputDocument;

    const MULTILINE: &str = "a\nb\nc";
    const EDITED: &str = "!a\nb\nc";
    const SCREEN: (u16, u16) = (80, 24);
    const OUTSIDE: (u16, u16) = (0, 0);

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

    fn render(editor: &mut PasteEditor) -> String {
        let (width, height) = SCREEN;
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                editor.view(frame, frame.area());
            })
            .unwrap();
        buffer_text(terminal.backend().buffer())
    }

    fn press_outside(editor: &mut PasteEditor) -> PasteEditorAction {
        let (column, row) = OUTSIDE;
        editor.handle_mouse(&MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn armed() -> PasteEditor {
        let mut editor = editor(MULTILINE);
        editor.handle_key(key(KeyCode::Char('!'), KeyModifiers::NONE));
        render(&mut editor);
        assert!(matches!(
            press_outside(&mut editor),
            PasteEditorAction::Consumed
        ));
        editor
    }

    #[test]
    fn a_press_outside_unedited_text_cancels() {
        let mut editor = editor(MULTILINE);
        render(&mut editor);
        assert!(matches!(
            press_outside(&mut editor),
            PasteEditorAction::Cancel
        ));
    }

    #[test]
    fn a_press_outside_edited_text_asks_and_enter_discards() {
        let mut editor = armed();
        assert!(editor.is_open());
        assert!(render(&mut editor).contains(DISCARD_PROMPT));
        let action = editor.handle_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(action, PasteEditorAction::Cancel));
    }

    #[test]
    fn esc_keeps_editing_with_the_edit_intact() {
        let mut editor = armed();
        let action = editor.handle_key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(action, PasteEditorAction::Consumed));
        assert!(!render(&mut editor).contains(DISCARD_PROMPT));
        let action = editor.handle_key(key(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert!(matches!(
            action,
            PasteEditorAction::Save { text, .. } if text == EDITED
        ));
    }

    #[test_case(|editor| { editor.handle_key(key(KeyCode::Char('x'), KeyModifiers::NONE)); }; "typed_key")]
    #[test_case(|editor| { editor.handle_paste("x"); }; "paste")]
    fn the_prompt_holds_the_text(input: fn(&mut PasteEditor)) {
        let mut editor = armed();
        input(&mut editor);
        assert_eq!(editor.editor.text(), EDITED);
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
