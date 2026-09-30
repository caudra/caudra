//! One editable field, and the keymap every text input in Caudra reads.
//!
//! [`decode`] is the only place a key becomes an editing command, so a chord
//! means the same thing in the composer, a picker's filter, a modal editor and
//! the workbench. [`TextField`] owns a [`Buffer`] and its [`History`] and runs
//! those commands against them. The composer keeps its paste chips on top of
//! the same commands, and a file tab runs them against its own buffer so its
//! highlighter hears about every edit.
//!
//! A field answers only the keys it decodes. Everything else, `Esc`, a bare
//! `Enter`, a list's navigation, a host's own chords, comes back as
//! [`TextKey::Ignored`] for the host to spend.

use std::borrow::Cow;
use std::ops::Range;

use caudra_highlight::TAB_SPACES;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Style;
use ratatui::text::Line;

use super::buffer::{Buffer, Cursor, Edit};
use super::history::History;
use super::render::{self, Row};

/// What a secret field shows in place of each character.
const MASK: &str = "*";
/// What a line break in a paste becomes in a one-line field.
const FLATTENED_BREAK: &str = " ";

/// How much of the keymap a field takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    /// One line: a filter, a name, a phrase. Pasted line breaks become spaces,
    /// and the vertical keys stay with the host, which usually has a list
    /// under the field to move.
    Line,
    /// Several lines the host submits with `Enter`: the composer, an answer, a
    /// rule. The newline chords break lines.
    Block,
    /// A document: `Enter`, `Tab` and the page keys edit and move here too.
    Document,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    WordLeft,
    WordRight,
    Up,
    Down,
    PageUp,
    PageDown,
    /// The first press lands on the text, the second on column zero.
    Home,
    End,
    TextStart,
    TextEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditCommand {
    Insert(char),
    Newline,
    /// A newline carrying the current line's indentation along.
    IndentedNewline,
    Indent,
    Dedent,
    Backspace,
    Delete,
    DeleteWordBefore,
    DeleteWordAfter,
    /// Joins the next line when the caret is already at the end of its own.
    KillToLineEnd,
    KillToLineStart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextCommand {
    Edit(EditCommand),
    Move { motion: Motion, extend: bool },
    SelectAll,
    Undo,
    Redo,
    Copy,
    Cut,
}

impl TextCommand {
    /// Whether a held key keeps acting. An edit or a motion does; selecting,
    /// copying and cutting are one gesture however long the key is held.
    pub fn repeats(self) -> bool {
        !matches!(self, Self::SelectAll | Self::Copy | Self::Cut)
    }
}

/// What a key did to a field, and what it leaves for the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextKey {
    Changed,
    /// The key was the field's but left the text alone: a motion, or an edit
    /// with nothing to act on.
    Handled,
    /// Text for the host to put on the clipboard.
    Copy(String),
    /// The selection came out of the text and goes on the clipboard.
    Cut(String),
    /// The edit would have grown the text past the field's limit, so it was
    /// undone.
    Refused,
    /// Not a key the field takes. `Ctrl+C` with nothing selected answers this
    /// way, so the host's own meaning of the chord still applies.
    Ignored,
}

impl TextKey {
    pub fn changed(&self) -> bool {
        matches!(self, Self::Changed | Self::Cut(_))
    }
}

/// What a field is painted with. The host resolves these from its theme; the
/// field only decides where each one lands.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FieldStyles {
    pub text: Style,
    pub selection: Style,
    pub caret: Style,
    pub placeholder: Style,
}

/// The editing command `key` stands for in a field of `kind`, or `None` when
/// the key is the host's.
pub fn decode(key: KeyEvent, kind: FieldKind) -> Option<TextCommand> {
    let modifiers = key.modifiers;
    // AltGr arrives as Ctrl+Alt and types text, so it reads as the key it
    // produced. Bare Alt binds nothing, and falling through would type the
    // chord's letter.
    let alt_gr = modifiers.contains(KeyModifiers::CONTROL | KeyModifiers::ALT);
    if modifiers.contains(KeyModifiers::ALT) && !alt_gr {
        return None;
    }
    let extend = modifiers.contains(KeyModifiers::SHIFT);
    if modifiers.contains(KeyModifiers::SUPER) {
        return super_chord(key.code, extend);
    }
    if modifiers.contains(KeyModifiers::CONTROL) && !alt_gr {
        return control_chord(key.code, kind, extend);
    }
    plain_key(key.code, kind, extend)
}

/// `Shift+Enter`, `Ctrl+Enter` and `Ctrl+J`, the keys that break a line in a
/// draft whose bare `Enter` submits it.
pub fn is_newline_key(key: KeyEvent) -> bool {
    decode(key, FieldKind::Block) == Some(TextCommand::Edit(EditCommand::Newline))
}

fn moving(motion: Motion, extend: bool) -> Option<TextCommand> {
    Some(TextCommand::Move { motion, extend })
}

fn editing(command: EditCommand) -> Option<TextCommand> {
    Some(TextCommand::Edit(command))
}

/// Every newline key breaks a document's line the way an editor's `Enter`
/// does, carrying its indentation onto the next.
fn newline(kind: FieldKind) -> Option<TextCommand> {
    editing(match kind {
        FieldKind::Document => EditCommand::IndentedNewline,
        FieldKind::Line | FieldKind::Block => EditCommand::Newline,
    })
}

/// The macOS line chords, which a terminal reports as Super once the kitty
/// protocol is on.
fn super_chord(code: KeyCode, extend: bool) -> Option<TextCommand> {
    match code {
        KeyCode::Backspace => editing(EditCommand::KillToLineStart),
        KeyCode::Left => moving(Motion::Home, extend),
        KeyCode::Right => moving(Motion::End, extend),
        _ => None,
    }
}

fn control_chord(code: KeyCode, kind: FieldKind, extend: bool) -> Option<TextCommand> {
    match code {
        KeyCode::Backspace | KeyCode::Char('w') => editing(EditCommand::DeleteWordBefore),
        KeyCode::Delete => editing(EditCommand::DeleteWordAfter),
        KeyCode::Char('k') => editing(EditCommand::KillToLineEnd),
        KeyCode::Enter | KeyCode::Char('j') if kind != FieldKind::Line => newline(kind),
        KeyCode::Char('a') => Some(TextCommand::SelectAll),
        KeyCode::Char('z') => Some(TextCommand::Undo),
        KeyCode::Char('y') => Some(TextCommand::Redo),
        KeyCode::Char('c') => Some(TextCommand::Copy),
        KeyCode::Char('e') => moving(Motion::End, extend),
        KeyCode::Left => moving(Motion::WordLeft, extend),
        KeyCode::Right => moving(Motion::WordRight, extend),
        KeyCode::Home => moving(Motion::TextStart, extend),
        KeyCode::End => moving(Motion::TextEnd, extend),
        _ => None,
    }
}

fn plain_key(code: KeyCode, kind: FieldKind, shift: bool) -> Option<TextCommand> {
    let lines = kind != FieldKind::Line;
    let document = kind == FieldKind::Document;
    match code {
        KeyCode::Char(ch) => editing(EditCommand::Insert(ch)),
        KeyCode::Backspace => editing(EditCommand::Backspace),
        KeyCode::Delete if shift => Some(TextCommand::Cut),
        KeyCode::Delete => editing(EditCommand::Delete),
        KeyCode::Left => moving(Motion::Left, shift),
        KeyCode::Right => moving(Motion::Right, shift),
        KeyCode::Home => moving(Motion::Home, shift),
        KeyCode::End => moving(Motion::End, shift),
        KeyCode::Up if lines => moving(Motion::Up, shift),
        KeyCode::Down if lines => moving(Motion::Down, shift),
        KeyCode::Enter if document || (shift && lines) => newline(kind),
        KeyCode::Tab if document && shift => editing(EditCommand::Dedent),
        KeyCode::Tab if document => editing(EditCommand::Indent),
        KeyCode::BackTab if document => editing(EditCommand::Dedent),
        KeyCode::PageUp if document => moving(Motion::PageUp, shift),
        KeyCode::PageDown if document => moving(Motion::PageDown, shift),
        _ => None,
    }
}

impl Buffer {
    /// Runs one editing command, returning the edit it made, if any.
    pub fn perform(&mut self, command: EditCommand) -> Option<Edit> {
        match command {
            EditCommand::Insert(ch) => self.insert(ch.encode_utf8(&mut [0; 4])),
            EditCommand::Newline => self.insert("\n"),
            EditCommand::IndentedNewline => self.insert_newline(),
            EditCommand::Indent => self.insert_indent(),
            EditCommand::Dedent => self.dedent(),
            EditCommand::Backspace => self.backspace(),
            EditCommand::Delete => self.delete(),
            EditCommand::DeleteWordBefore => self.delete_word_left(),
            EditCommand::DeleteWordAfter => self.delete_word_right(),
            EditCommand::KillToLineEnd => self.kill_to_end_of_line(),
            EditCommand::KillToLineStart => self.kill_to_start_of_line(),
        }
    }

    /// Moves the caret, growing the selection when `extend` is set. `page` is
    /// how many lines a page key travels.
    pub fn move_by(&mut self, motion: Motion, extend: bool, page: usize) {
        let page = isize::try_from(page.max(1)).unwrap_or(isize::MAX);
        match motion {
            Motion::Left => self.move_left(extend),
            Motion::Right => self.move_right(extend),
            Motion::WordLeft => self.move_word_left(extend),
            Motion::WordRight => self.move_word_right(extend),
            Motion::Up => self.move_vertical(-1, extend),
            Motion::Down => self.move_vertical(1, extend),
            Motion::PageUp => self.move_vertical(-page, extend),
            Motion::PageDown => self.move_vertical(page, extend),
            Motion::Home => self.move_home(extend),
            Motion::End => self.move_end(extend),
            Motion::TextStart => self.move_document_start(extend),
            Motion::TextEnd => self.move_document_end(extend),
        }
    }
}

/// A text field: a buffer, its undo history, and the rules its kind puts on
/// what may go in.
#[derive(Debug, Clone)]
pub struct TextField {
    buffer: Buffer,
    history: History,
    kind: FieldKind,
    /// Most bytes the text may grow to. An edit that would pass it is undone
    /// and reported as [`TextKey::Refused`]; one that does not grow the text
    /// always goes through, so a field loaded over its limit can be cut down.
    limit: Option<usize>,
    /// A password or a key: painted masked, and never selected whole, copied
    /// or cut.
    secret: bool,
}

impl TextField {
    pub fn new(kind: FieldKind) -> Self {
        Self::with_text(kind, "")
    }

    /// A field already holding `text`, caret at its end and nothing to undo.
    pub fn with_text(kind: FieldKind, text: &str) -> Self {
        let mut field = Self {
            buffer: Buffer::new(Vec::new()),
            history: History::default(),
            kind,
            limit: None,
            secret: false,
        };
        field.set_text(text);
        field
    }

    pub fn limited_to(mut self, bytes: usize) -> Self {
        self.limit = Some(bytes);
        self
    }

    pub fn secret(mut self) -> Self {
        self.secret = true;
        self
    }

    pub fn set_limit(&mut self, bytes: Option<usize>) {
        self.limit = bytes;
    }

    pub fn set_secret(&mut self, secret: bool) {
        self.secret = secret;
    }

    pub fn is_secret(&self) -> bool {
        self.secret
    }

    pub fn kind(&self) -> FieldKind {
        self.kind
    }

    pub fn buffer(&self) -> &Buffer {
        &self.buffer
    }

    pub fn text(&self) -> String {
        self.buffer.lines().join("\n")
    }

    pub fn lines(&self) -> &[String] {
        self.buffer.lines()
    }

    /// Whether the field reads exactly `text`, without joining its lines.
    pub fn holds(&self, text: &str) -> bool {
        text.split('\n')
            .eq(self.buffer.lines().iter().map(String::as_str))
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.line_count() == 1 && self.buffer.line(0).is_empty()
    }

    pub fn byte_len(&self) -> usize {
        self.buffer.lines().iter().map(String::len).sum::<usize>() + self.buffer.line_count() - 1
    }

    pub fn cursor(&self) -> Cursor {
        self.buffer.cursor()
    }

    /// The caret as a character offset into [`Self::text`].
    pub fn cursor_offset(&self) -> usize {
        let cursor = self.buffer.cursor();
        self.buffer
            .lines()
            .iter()
            .take(cursor.line)
            .map(|line| line.chars().count() + 1)
            .sum::<usize>()
            + cursor.col
    }

    pub fn set_cursor_offset(&mut self, mut offset: usize) {
        let mut cursor = None;
        for (line, text) in self.buffer.lines().iter().enumerate() {
            let len = text.chars().count();
            if offset <= len {
                cursor = Some(Cursor::new(line, offset));
                break;
            }
            offset -= len + 1;
        }
        match cursor {
            Some(cursor) => self.set_cursor(cursor, false),
            None => self.move_to_end(),
        }
    }

    /// Drops the caret, extending the selection when asked. Where the caret
    /// lands is out of the run of typing before it, so undo stops there.
    pub fn set_cursor(&mut self, cursor: Cursor, extend: bool) {
        self.buffer.set_cursor(cursor, extend);
        self.history.break_group();
    }

    pub fn move_to_end(&mut self) {
        self.buffer.move_document_end(false);
        self.history.break_group();
    }

    pub fn selection(&self) -> Option<(Cursor, Cursor)> {
        self.buffer.selection()
    }

    /// The selected text, never a secret's.
    pub fn selected_text(&self) -> Option<String> {
        (!self.secret)
            .then(|| self.buffer.selected_text())
            .flatten()
    }

    pub fn select_all(&mut self) {
        if !self.secret {
            self.buffer.select_all();
        }
    }

    pub fn select_word_at(&mut self, cursor: Cursor) {
        self.buffer.select_word_at(cursor);
    }

    pub fn select_line_at(&mut self, line: usize) {
        self.buffer.select_line_at(line);
    }

    pub fn clear_selection(&mut self) {
        self.buffer.clear_selection();
    }

    /// Replaces the text outright, caret at the end and nothing to undo: a
    /// value the field was loaded with, not an edit anyone made.
    pub fn set_text(&mut self, text: &str) {
        let text = sanitize(self.kind, text);
        self.buffer = Buffer::new(text.split('\n').map(str::to_owned).collect());
        self.buffer.move_document_end(false);
        self.history = History::default();
    }

    pub fn clear(&mut self) {
        self.set_text("");
    }

    /// Inserts as typing would: over the selection, recorded for undo, and
    /// held to the field's kind and limit.
    pub fn insert_text(&mut self, text: &str) -> TextKey {
        let text = sanitize(self.kind, text);
        if text.is_empty() {
            return TextKey::Handled;
        }
        self.edit_with(|buffer| buffer.insert(&text))
    }

    /// A paste, which undo takes back in one step of its own.
    pub fn paste(&mut self, text: &str) -> TextKey {
        self.history.break_group();
        let pasted = self.insert_text(text);
        self.history.break_group();
        pasted
    }

    pub fn undo(&mut self) -> bool {
        self.replay(History::undo)
    }

    pub fn redo(&mut self) -> bool {
        self.replay(History::redo)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> TextKey {
        self.handle_key_paged(key, 1)
    }

    /// [`Self::handle_key`] for a field tall enough to page, `page` rows at a
    /// time.
    pub fn handle_key_paged(&mut self, key: KeyEvent, page: usize) -> TextKey {
        decode(key, self.kind).map_or(TextKey::Ignored, |command| self.perform(command, page))
    }

    pub fn perform(&mut self, command: TextCommand, page: usize) -> TextKey {
        match command {
            TextCommand::Edit(command) => self.edit_with(|buffer| buffer.perform(command)),
            TextCommand::Move { motion, extend } => {
                self.buffer.move_by(motion, extend, page);
                self.history.break_group();
                TextKey::Handled
            }
            TextCommand::SelectAll => {
                self.select_all();
                TextKey::Handled
            }
            TextCommand::Undo => changed_if(self.undo()),
            TextCommand::Redo => changed_if(self.redo()),
            TextCommand::Copy => self.selected_text().map_or(TextKey::Ignored, TextKey::Copy),
            TextCommand::Cut => {
                let Some(text) = self.selected_text() else {
                    return TextKey::Handled;
                };
                match self.edit_with(Buffer::delete) {
                    TextKey::Changed => TextKey::Cut(text),
                    other => other,
                }
            }
        }
    }

    /// Line `index` as it is shown: masked, for a secret.
    pub fn shown_line(&self, index: usize) -> Cow<'_, str> {
        let line = self.buffer.line(index);
        match self.secret {
            true => Cow::Owned(MASK.repeat(line.chars().count())),
            false => Cow::Borrowed(line),
        }
    }

    /// The selection on line `index`, then the caret over it, as character
    /// ranges for a [`Row`]. The caret only shows while the field is focused.
    pub fn overlays(
        &self,
        index: usize,
        styles: &FieldStyles,
        focused: bool,
    ) -> Vec<(Range<usize>, Style)> {
        let cursor = self.buffer.cursor();
        let caret = (focused && cursor.line == index).then_some(cursor.col);
        let mut overlays = Vec::with_capacity(2);
        if let Some((from, to)) = self.buffer.selection()
            && (from.line..=to.line).contains(&index)
        {
            let start = if index == from.line { from.col } else { 0 };
            let end = match index == to.line {
                true => to.col,
                false => self.buffer.line(index).chars().count() + 1,
            };
            // The caret keeps a cell of its own: painted over the selection's
            // reversal it would reverse back into plain text.
            let start = start + usize::from(caret == Some(start));
            if start < end {
                overlays.push((start..end, styles.selection));
            }
        }
        if let Some(col) = caret {
            overlays.push((col..col + 1, styles.caret));
        }
        overlays
    }

    /// The caret's line in `width` columns, panned just far enough to keep the
    /// caret on screen. An empty field shows `placeholder` instead.
    pub fn paint(
        &self,
        width: usize,
        styles: &FieldStyles,
        focused: bool,
        placeholder: &str,
    ) -> Line<'static> {
        if self.is_empty() && !placeholder.is_empty() {
            return paint_placeholder(placeholder, width, styles, focused);
        }
        let cursor = self.buffer.cursor();
        let shown = self.shown_line(cursor.line);
        let overlays = self.overlays(cursor.line, styles, focused);
        let pan = match focused {
            true => (render::display_column(&shown, cursor.col) + 1).saturating_sub(width),
            false => 0,
        };
        row(&shown, styles.text, &overlays).paint(pan, width)
    }

    /// Every line, wrapped to `width` columns the way the workbench wraps a
    /// file. A caret past a full last row gets a row of its own.
    pub fn paint_wrapped(
        &self,
        width: usize,
        styles: &FieldStyles,
        focused: bool,
        placeholder: &str,
    ) -> Vec<Line<'static>> {
        let width = width.max(1);
        if self.is_empty() && !placeholder.is_empty() {
            return vec![paint_placeholder(placeholder, width, styles, focused)];
        }
        let cursor = self.buffer.cursor();
        let mut painted = Vec::with_capacity(self.buffer.line_count());
        for index in 0..self.buffer.line_count() {
            let shown = self.shown_line(index);
            let overlays = self.overlays(index, styles, focused);
            let line = row(&shown, styles.text, &overlays);
            let mut starts = render::wrap_columns(&shown, width);
            if focused && cursor.line == index {
                let caret = render::display_column(&shown, cursor.col);
                if starts.last().is_some_and(|&last| caret >= last + width) {
                    starts.push(caret);
                }
            }
            for (at, &start) in starts.iter().enumerate() {
                let end = starts.get(at + 1).map_or(start + width, |&next| next);
                painted.push(line.paint(start, (end - start).min(width)));
            }
        }
        painted
    }

    /// Runs one edit, then holds it to the limit: an edit that grew the text
    /// past it is replayed backwards and the selection it replaced restored.
    fn edit_with(&mut self, change: impl FnOnce(&mut Buffer) -> Option<Edit>) -> TextKey {
        let selection = self.buffer.selection();
        let cursor = self.buffer.cursor();
        let Some(edit) = change(&mut self.buffer) else {
            return TextKey::Handled;
        };
        let grew = edit.inserted.len() > edit.removed.len();
        if grew && self.limit.is_some_and(|limit| self.byte_len() > limit) {
            self.buffer.replay(&edit.inverted());
            if let Some((start, end)) = selection {
                let anchor = if cursor == start { end } else { start };
                self.buffer.set_cursor(anchor, false);
                self.buffer.set_cursor(cursor, true);
            }
            return TextKey::Refused;
        }
        self.history.record(edit);
        TextKey::Changed
    }

    fn replay(&mut self, step: fn(&mut History) -> Option<Edit>) -> bool {
        let Some(edit) = step(&mut self.history) else {
            return false;
        };
        self.buffer.replay(&edit);
        true
    }
}

fn changed_if(changed: bool) -> TextKey {
    match changed {
        true => TextKey::Changed,
        false => TextKey::Handled,
    }
}

/// `text` as a field of `kind` may hold it: line breaks normalised, flattened
/// to spaces on one line, and tabs spelled as spaces outside a document, where
/// nothing expands them.
fn sanitize(kind: FieldKind, text: &str) -> String {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    match kind {
        FieldKind::Document => text,
        FieldKind::Block => text.replace('\t', TAB_SPACES),
        FieldKind::Line => text
            .lines()
            .collect::<Vec<_>>()
            .join(FLATTENED_BREAK)
            .replace('\t', TAB_SPACES),
    }
}

fn row<'a>(text: &'a str, base: Style, overlays: &'a [(Range<usize>, Style)]) -> Row<'a> {
    Row {
        text,
        segments: None,
        base,
        fill: None,
        overlays,
    }
}

fn paint_placeholder(
    placeholder: &str,
    width: usize,
    styles: &FieldStyles,
    focused: bool,
) -> Line<'static> {
    let caret = [(0..1, styles.caret)];
    let overlays: &[(Range<usize>, Style)] = if focused { &caret } else { &[] };
    row(placeholder, styles.placeholder, overlays).paint(0, width)
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::Line;
    use test_case::test_case;

    use super::{
        Cursor, EditCommand, FieldKind, FieldStyles, Motion, TextCommand, TextField, TextKey,
        decode, is_newline_key,
    };
    use crate::keys::{self, Bind};

    const BLOCK: FieldKind = FieldKind::Block;
    const DOCUMENT: FieldKind = FieldKind::Document;
    const LINE: FieldKind = FieldKind::Line;

    const WRONG_TEXT: &str = "the field holds the wrong text";
    const WRONG_CARET: &str = "the caret landed somewhere else";
    const WRONG_RESULT: &str = "the key reported the wrong outcome";
    const BIND_DRIFT: &str = "a documented chord no longer decodes to the command it names";
    const LEAKED_SECRET: &str = "a secret field handed its text out";
    const CARET_HIDDEN: &str = "the caret must stay on screen";

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn plain(code: KeyCode) -> KeyEvent {
        press(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        press(code, KeyModifiers::CONTROL)
    }

    fn shift(code: KeyCode) -> KeyEvent {
        press(code, KeyModifiers::SHIFT)
    }

    fn edit(command: EditCommand) -> Option<TextCommand> {
        Some(TextCommand::Edit(command))
    }

    fn to(motion: Motion) -> Option<TextCommand> {
        Some(TextCommand::Move {
            motion,
            extend: false,
        })
    }

    fn extending(motion: Motion) -> Option<TextCommand> {
        Some(TextCommand::Move {
            motion,
            extend: true,
        })
    }

    fn styles() -> FieldStyles {
        FieldStyles {
            text: Style::new().fg(Color::White),
            selection: Style::new().add_modifier(Modifier::REVERSED),
            caret: Style::new().fg(Color::Black).bg(Color::White),
            placeholder: Style::new().fg(Color::DarkGray),
        }
    }

    fn field(kind: FieldKind, text: &str) -> TextField {
        TextField::with_text(kind, text)
    }

    fn type_keys(field: &mut TextField, keys: &[KeyEvent]) {
        for key in keys {
            field.handle_key(*key);
        }
    }

    fn cells(line: &Line) -> Vec<(char, Style)> {
        line.spans
            .iter()
            .flat_map(|span| span.content.chars().map(|ch| (ch, span.style)))
            .collect()
    }

    #[test_case(plain(KeyCode::Char('x')), LINE => edit(EditCommand::Insert('x')); "a character is typed")]
    #[test_case(shift(KeyCode::Char('X')), LINE => edit(EditCommand::Insert('X')); "a shifted character is typed")]
    #[test_case(press(KeyCode::Char('@'), KeyModifiers::CONTROL | KeyModifiers::ALT), LINE => edit(EditCommand::Insert('@')); "alt_gr types its character")]
    #[test_case(press(KeyCode::Char('x'), KeyModifiers::ALT), LINE => None; "bare alt types nothing")]
    #[test_case(press(KeyCode::Char('x'), KeyModifiers::SUPER), LINE => None; "super with a character types nothing")]
    #[test_case(plain(KeyCode::Backspace), LINE => edit(EditCommand::Backspace); "backspace")]
    #[test_case(plain(KeyCode::Delete), LINE => edit(EditCommand::Delete); "delete")]
    #[test_case(shift(KeyCode::Delete), LINE => Some(TextCommand::Cut); "shift delete cuts")]
    #[test_case(ctrl(KeyCode::Char('w')), LINE => edit(EditCommand::DeleteWordBefore); "ctrl w")]
    #[test_case(ctrl(KeyCode::Backspace), LINE => edit(EditCommand::DeleteWordBefore); "ctrl backspace")]
    #[test_case(ctrl(KeyCode::Delete), LINE => edit(EditCommand::DeleteWordAfter); "ctrl delete")]
    #[test_case(ctrl(KeyCode::Char('k')), LINE => edit(EditCommand::KillToLineEnd); "ctrl k")]
    #[test_case(press(KeyCode::Backspace, KeyModifiers::SUPER), LINE => edit(EditCommand::KillToLineStart); "super backspace")]
    #[test_case(ctrl(KeyCode::Char('a')), LINE => Some(TextCommand::SelectAll); "ctrl a selects all")]
    #[test_case(ctrl(KeyCode::Char('z')), LINE => Some(TextCommand::Undo); "ctrl z undoes")]
    #[test_case(ctrl(KeyCode::Char('y')), LINE => Some(TextCommand::Redo); "ctrl y redoes")]
    #[test_case(ctrl(KeyCode::Char('c')), LINE => Some(TextCommand::Copy); "ctrl c copies")]
    #[test_case(plain(KeyCode::Left), LINE => to(Motion::Left); "left")]
    #[test_case(shift(KeyCode::Right), LINE => extending(Motion::Right); "shift right extends")]
    #[test_case(ctrl(KeyCode::Left), LINE => to(Motion::WordLeft); "ctrl left")]
    #[test_case(press(KeyCode::Right, KeyModifiers::CONTROL | KeyModifiers::SHIFT), LINE => extending(Motion::WordRight); "ctrl shift right extends by word")]
    #[test_case(plain(KeyCode::Home), LINE => to(Motion::Home); "home")]
    #[test_case(plain(KeyCode::End), LINE => to(Motion::End); "end")]
    #[test_case(ctrl(KeyCode::Char('e')), LINE => to(Motion::End); "ctrl e")]
    #[test_case(press(KeyCode::Left, KeyModifiers::SUPER), LINE => to(Motion::Home); "super left")]
    #[test_case(press(KeyCode::Right, KeyModifiers::SUPER), LINE => to(Motion::End); "super right")]
    #[test_case(ctrl(KeyCode::Home), LINE => to(Motion::TextStart); "ctrl home")]
    #[test_case(press(KeyCode::End, KeyModifiers::CONTROL | KeyModifiers::SHIFT), BLOCK => extending(Motion::TextEnd); "ctrl shift end extends")]
    #[test_case(plain(KeyCode::Up), LINE => None; "up belongs to a one line field's host")]
    #[test_case(plain(KeyCode::Up), BLOCK => to(Motion::Up); "up moves in a block")]
    #[test_case(shift(KeyCode::Down), DOCUMENT => extending(Motion::Down); "shift down extends in a document")]
    #[test_case(shift(KeyCode::Enter), LINE => None; "shift enter is the host's on one line")]
    #[test_case(shift(KeyCode::Enter), BLOCK => edit(EditCommand::Newline); "shift enter breaks a block's line")]
    #[test_case(ctrl(KeyCode::Enter), BLOCK => edit(EditCommand::Newline); "ctrl enter breaks a block's line")]
    #[test_case(ctrl(KeyCode::Char('j')), DOCUMENT => edit(EditCommand::IndentedNewline); "ctrl j keeps a document's indent")]
    #[test_case(shift(KeyCode::Enter), DOCUMENT => edit(EditCommand::IndentedNewline); "shift enter keeps a document's indent")]
    #[test_case(ctrl(KeyCode::Char('j')), LINE => None; "ctrl j is the host's on one line")]
    #[test_case(plain(KeyCode::Enter), BLOCK => None; "enter submits a block")]
    #[test_case(plain(KeyCode::Enter), DOCUMENT => edit(EditCommand::IndentedNewline); "enter breaks a document's line")]
    #[test_case(plain(KeyCode::Tab), BLOCK => None; "tab is the host's in a block")]
    #[test_case(plain(KeyCode::Tab), DOCUMENT => edit(EditCommand::Indent); "tab indents a document")]
    #[test_case(shift(KeyCode::BackTab), DOCUMENT => edit(EditCommand::Dedent); "shift tab dedents a document")]
    #[test_case(plain(KeyCode::PageUp), BLOCK => None; "page up is the host's in a block")]
    #[test_case(plain(KeyCode::PageDown), DOCUMENT => to(Motion::PageDown); "page down pages a document")]
    #[test_case(plain(KeyCode::Esc), DOCUMENT => None; "esc is always the host's")]
    #[test_case(ctrl(KeyCode::Char('u')), BLOCK => None; "an unbound chord is the host's")]
    #[test_case(ctrl(KeyCode::PageDown), DOCUMENT => None; "ctrl page down is the host's")]
    fn a_key_decodes_to_its_command(key: KeyEvent, kind: FieldKind) -> Option<TextCommand> {
        decode(key, kind)
    }

    #[test_case(keys::DELETE_WORD, TextCommand::Edit(EditCommand::DeleteWordBefore); "delete word")]
    #[test_case(keys::DELETE_WORD_BACK, TextCommand::Edit(EditCommand::DeleteWordBefore); "delete word back")]
    #[test_case(keys::DELETE_WORD_AFTER, TextCommand::Edit(EditCommand::DeleteWordAfter); "delete word after")]
    #[test_case(keys::KILL_LINE, TextCommand::Edit(EditCommand::KillToLineEnd); "kill line")]
    #[test_case(keys::KILL_TO_LINE_START, TextCommand::Edit(EditCommand::KillToLineStart); "kill to line start")]
    #[test_case(keys::SELECT_ALL, TextCommand::SelectAll; "select all")]
    #[test_case(keys::UNDO, TextCommand::Undo; "undo")]
    #[test_case(keys::REDO, TextCommand::Redo; "redo")]
    #[test_case(keys::COPY, TextCommand::Copy; "copy")]
    #[test_case(keys::CUT, TextCommand::Cut; "cut")]
    #[test_case(keys::LINE_END, TextCommand::Move { motion: Motion::End, extend: false }; "line end")]
    #[test_case(keys::WORD_LEFT, TextCommand::Move { motion: Motion::WordLeft, extend: false }; "word left")]
    #[test_case(keys::WORD_RIGHT, TextCommand::Move { motion: Motion::WordRight, extend: false }; "word right")]
    #[test_case(keys::SUPER_HOME, TextCommand::Move { motion: Motion::Home, extend: false }; "super home")]
    #[test_case(keys::SUPER_END, TextCommand::Move { motion: Motion::End, extend: false }; "super end")]
    #[test_case(keys::TEXT_START, TextCommand::Move { motion: Motion::TextStart, extend: false }; "text start")]
    #[test_case(keys::TEXT_END, TextCommand::Move { motion: Motion::TextEnd, extend: false }; "text end")]
    fn a_documented_chord_decodes_to_what_it_names(bind: Bind, expected: TextCommand) {
        assert_eq!(
            decode(bind.to_key_event(), BLOCK),
            Some(expected),
            "{BIND_DRIFT}: {}",
            bind.label
        );
    }

    #[test_case(shift(KeyCode::Enter) => true; "shift enter")]
    #[test_case(ctrl(KeyCode::Enter) => true; "ctrl enter")]
    #[test_case(ctrl(KeyCode::Char('j')) => true; "ctrl j")]
    #[test_case(plain(KeyCode::Enter) => false; "a bare enter submits")]
    #[test_case(press(KeyCode::Enter, KeyModifiers::ALT) => false; "alt enter")]
    fn newline_keys(key: KeyEvent) -> bool {
        is_newline_key(key)
    }

    #[test]
    fn select_copy_and_cut_never_repeat() {
        for command in [TextCommand::SelectAll, TextCommand::Copy, TextCommand::Cut] {
            assert!(!command.repeats(), "{command:?}");
        }
        assert!(TextCommand::Edit(EditCommand::Backspace).repeats());
        assert!(TextCommand::Undo.repeats());
    }

    #[test]
    fn a_word_delete_takes_the_selection_when_there_is_one() {
        let mut field = field(LINE, "alpha beta gamma");
        field.set_cursor(Cursor::new(0, 6), false);
        field.set_cursor(Cursor::new(0, 10), true);

        assert_eq!(field.handle_key(ctrl(KeyCode::Char('w'))), TextKey::Changed);

        assert_eq!(field.text(), "alpha  gamma", "{WRONG_TEXT}");
    }

    #[test]
    fn kill_line_at_the_end_of_a_line_joins_the_next() {
        let mut field = field(BLOCK, "one\ntwo");
        field.set_cursor(Cursor::new(0, 3), false);

        field.handle_key(ctrl(KeyCode::Char('k')));

        assert_eq!(field.text(), "onetwo", "{WRONG_TEXT}");
    }

    #[test]
    fn super_backspace_kills_back_to_the_head_of_the_line() {
        let mut field = field(BLOCK, "one\ntwo three");
        field.set_cursor(Cursor::new(1, 3), false);

        field.handle_key(press(KeyCode::Backspace, KeyModifiers::SUPER));

        assert_eq!(field.text(), "one\n three", "{WRONG_TEXT}");
    }

    #[test]
    fn cut_with_nothing_selected_changes_nothing() {
        let mut field = field(LINE, "keep");
        field.set_cursor(Cursor::new(0, 1), false);

        assert_eq!(field.handle_key(shift(KeyCode::Delete)), TextKey::Handled);
        assert_eq!(field.text(), "keep", "{WRONG_TEXT}");
    }

    #[test]
    fn cut_hands_the_selection_over_and_removes_it() {
        let mut field = field(LINE, "cut here");
        field.set_cursor(Cursor::new(0, 3), false);
        field.set_cursor(Cursor::new(0, 8), true);

        let cut = field.handle_key(shift(KeyCode::Delete));

        assert_eq!(cut, TextKey::Cut(" here".to_owned()), "{WRONG_RESULT}");
        assert!(cut.changed());
        assert_eq!(field.text(), "cut", "{WRONG_TEXT}");
    }

    #[test]
    fn copy_with_nothing_selected_is_left_to_the_host() {
        let mut field = field(LINE, "text");
        assert_eq!(field.handle_key(ctrl(KeyCode::Char('c'))), TextKey::Ignored);
    }

    #[test]
    fn select_all_then_copy_hands_over_every_line() {
        let mut field = field(BLOCK, "one\ntwo");
        field.handle_key(ctrl(KeyCode::Char('a')));

        assert_eq!(
            field.handle_key(ctrl(KeyCode::Char('c'))),
            TextKey::Copy("one\ntwo".to_owned()),
            "{WRONG_RESULT}"
        );
    }

    #[test]
    fn ctrl_home_and_end_reach_the_ends_of_the_text() {
        let mut field = field(BLOCK, "one\ntwo\nthree");
        field.handle_key(ctrl(KeyCode::Home));
        assert_eq!(field.cursor(), Cursor::new(0, 0), "{WRONG_CARET}");
        field.handle_key(ctrl(KeyCode::End));
        assert_eq!(field.cursor(), Cursor::new(2, 5), "{WRONG_CARET}");
    }

    #[test]
    fn a_word_motion_lands_on_the_start_of_the_next_word() {
        let mut field = field(LINE, "foo bar");
        field.set_cursor(Cursor::new(0, 0), false);

        field.handle_key(ctrl(KeyCode::Right));

        assert_eq!(field.cursor(), Cursor::new(0, 4), "{WRONG_CARET}");
    }

    #[test]
    fn undo_and_redo_walk_the_typing_back_and_forth() {
        let mut field = field(LINE, "");
        type_keys(
            &mut field,
            &[plain(KeyCode::Char('h')), plain(KeyCode::Char('i'))],
        );

        assert_eq!(field.handle_key(ctrl(KeyCode::Char('z'))), TextKey::Changed);
        assert_eq!(field.text(), "", "{WRONG_TEXT}");
        assert_eq!(field.handle_key(ctrl(KeyCode::Char('y'))), TextKey::Changed);
        assert_eq!(field.text(), "hi", "{WRONG_TEXT}");
        assert_eq!(field.handle_key(ctrl(KeyCode::Char('y'))), TextKey::Handled);
    }

    #[test]
    fn a_paste_is_an_undo_step_of_its_own() {
        let mut field = field(LINE, "");
        field.handle_key(plain(KeyCode::Char('a')));
        field.paste("bc");

        field.undo();

        assert_eq!(field.text(), "a", "{WRONG_TEXT}");
    }

    #[test]
    fn set_text_leaves_the_caret_at_the_end_and_nothing_to_undo() {
        let mut field = field(BLOCK, "one\ntwo");
        assert_eq!(field.cursor(), Cursor::new(1, 3), "{WRONG_CARET}");
        assert!(!field.undo());
    }

    #[test_case(LINE, "a\r\nb\n" => "a b"; "one line flattens its breaks")]
    #[test_case(BLOCK, "a\r\nb\rc" => "a\nb\nc"; "a block normalises its breaks")]
    #[test_case(LINE, "a\tb" => "a  b"; "one line spells a tab as spaces")]
    #[test_case(BLOCK, "a\tb" => "a  b"; "a block spells a tab as spaces")]
    #[test_case(DOCUMENT, "a\tb" => "a\tb"; "a document keeps its tabs")]
    fn a_paste_is_held_to_the_kind_of_field(kind: FieldKind, pasted: &str) -> String {
        let mut field = field(kind, "");
        field.paste(pasted);
        field.text()
    }

    #[test]
    fn cursor_offsets_count_characters_across_lines() {
        let mut field = field(BLOCK, "é\nab");
        field.set_cursor_offset(3);
        assert_eq!(field.cursor(), Cursor::new(1, 1), "{WRONG_CARET}");
        assert_eq!(field.cursor_offset(), 3);
        field.set_cursor_offset(99);
        assert_eq!(field.cursor(), Cursor::new(1, 2), "{WRONG_CARET}");
    }

    #[test]
    fn an_edit_past_the_limit_is_refused_and_undone() {
        let mut field = field(LINE, "abc").limited_to(3);

        assert_eq!(
            field.handle_key(plain(KeyCode::Char('d'))),
            TextKey::Refused
        );

        assert_eq!(field.text(), "abc", "{WRONG_TEXT}");
        assert_eq!(field.cursor(), Cursor::new(0, 3), "{WRONG_CARET}");
        assert!(!field.undo(), "a refused edit must leave nothing to undo");
    }

    #[test]
    fn a_refused_paste_keeps_the_selection_it_would_have_replaced() {
        let mut field = field(LINE, "abc").limited_to(3);
        field.set_cursor(Cursor::new(0, 1), false);
        field.set_cursor(Cursor::new(0, 2), true);

        assert_eq!(field.paste("xyz"), TextKey::Refused);

        assert_eq!(field.text(), "abc", "{WRONG_TEXT}");
        assert_eq!(
            field.selection(),
            Some((Cursor::new(0, 1), Cursor::new(0, 2)))
        );
    }

    #[test]
    fn a_field_at_its_limit_still_deletes() {
        let mut field = field(LINE, "abcd").limited_to(3);
        assert_eq!(
            field.handle_key(plain(KeyCode::Backspace)),
            TextKey::Changed
        );
        assert_eq!(field.text(), "abc", "{WRONG_TEXT}");
    }

    #[test]
    fn a_secret_field_never_hands_its_text_out() {
        let mut field = field(LINE, "hunter2").secret();

        field.handle_key(ctrl(KeyCode::Char('a')));
        assert_eq!(field.selection(), None, "{LEAKED_SECRET}");

        field.handle_key(shift(KeyCode::Home));
        assert_eq!(field.handle_key(ctrl(KeyCode::Char('c'))), TextKey::Ignored);
        assert_eq!(field.handle_key(shift(KeyCode::Delete)), TextKey::Handled);
        assert_eq!(field.text(), "hunter2", "{LEAKED_SECRET}");
        assert_eq!(field.shown_line(0), "*******", "{LEAKED_SECRET}");
    }

    #[test]
    fn the_caret_stays_on_screen_past_the_right_edge() {
        let field = field(LINE, "abcdefghij");

        let painted = cells(&field.paint(4, &styles(), true, ""));

        let shown: String = painted.iter().map(|(ch, _)| ch).collect();
        assert_eq!(shown, "hij ", "{CARET_HIDDEN}");
        assert_eq!(
            painted.last().map(|(_, style)| *style),
            Some(styles().text.patch(styles().caret))
        );
    }

    #[test]
    fn an_unfocused_field_shows_its_start_and_no_caret() {
        let field = field(LINE, "abcdefghij");

        let painted = cells(&field.paint(4, &styles(), false, ""));

        let shown: String = painted.iter().map(|(ch, _)| ch).collect();
        assert_eq!(shown, "abcd");
        assert!(painted.iter().all(|(_, style)| *style == styles().text));
    }

    #[test]
    fn the_caret_keeps_its_own_cell_inside_a_selection() {
        let mut field = field(LINE, "abcd");
        field.set_cursor(Cursor::new(0, 3), false);
        field.set_cursor(Cursor::new(0, 1), true);

        let painted = cells(&field.paint(10, &styles(), true, ""));

        let text = styles().text;
        assert_eq!(painted[0], ('a', text));
        assert_eq!(painted[1], ('b', text.patch(styles().caret)));
        assert_eq!(painted[2], ('c', text.patch(styles().selection)));
        assert_eq!(painted[3], ('d', text));
    }

    #[test]
    fn a_secret_field_is_painted_masked() {
        let field = field(LINE, "key").secret();
        let shown: String = cells(&field.paint(10, &styles(), false, ""))
            .iter()
            .map(|(ch, _)| ch)
            .collect();
        assert_eq!(shown, "***", "{LEAKED_SECRET}");
    }

    #[test]
    fn an_empty_field_shows_its_placeholder_under_the_caret() {
        let field = field(LINE, "");

        let painted = cells(&field.paint(10, &styles(), true, "type"));

        assert_eq!(
            painted[0],
            ('t', styles().placeholder.patch(styles().caret))
        );
        assert_eq!(painted[1], ('y', styles().placeholder));
    }

    #[test]
    fn a_caret_past_a_full_row_gets_a_row_of_its_own() {
        let field = field(LINE, "abcd");

        let rows = field.paint_wrapped(4, &styles(), true, "");

        assert_eq!(rows.len(), 2, "{CARET_HIDDEN}");
        assert_eq!(
            cells(&rows[1]),
            vec![(' ', styles().text.patch(styles().caret))]
        );
    }

    #[test]
    fn wrapped_rows_break_between_words() {
        let field = field(BLOCK, "one two\nthree");

        let rows: Vec<String> = field
            .paint_wrapped(4, &styles(), false, "")
            .iter()
            .map(|row| cells(row).iter().map(|(ch, _)| ch).collect())
            .collect();

        assert_eq!(rows, ["one ", "two", "thre", "e"]);
    }
}
