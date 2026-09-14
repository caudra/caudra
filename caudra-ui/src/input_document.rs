use std::ops::Range;

use caudra_workbench::buffer::{Buffer, Cursor, Edit};
use caudra_workbench::history::History;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::highlight::TAB_SPACES;
use crate::text_buffer::EditResult;

pub(crate) const PASTE_TOKEN_MIN_CHARACTERS: usize = 150;
pub(crate) const PASTE_TOKEN_MIN_LINES: usize = 3;
const PALETTE_TOKEN: &str = "paste";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct PasteId(u64);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct InputDraft {
    pub text: String,
    pub paste_ranges: Vec<Range<usize>>,
}

impl InputDraft {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PasteSpan {
    id: PasteId,
    range: Range<usize>,
    text: String,
}

pub(crate) struct InputDocument {
    display: Buffer,
    /// Undo for the composer, coalesced into gestures by the same rules the
    /// workbench's editor uses.
    history: History,
    pastes: Vec<PasteSpan>,
    next_paste_id: u64,
}

impl InputDocument {
    pub fn new() -> Self {
        Self::from_plain(String::new())
    }

    pub fn from_plain(text: String) -> Self {
        Self {
            display: buffer_of(&text),
            history: History::default(),
            pastes: Vec::new(),
            next_paste_id: 1,
        }
    }

    pub fn from_draft(draft: InputDraft) -> Self {
        if !valid_ranges(&draft.text, &draft.paste_ranges) {
            return Self::from_plain(draft.text);
        }

        let mut display = String::new();
        let mut pastes = Vec::with_capacity(draft.paste_ranges.len());
        let mut source_byte = 0;
        let mut display_char = 0;

        for (index, source_range) in draft.paste_ranges.into_iter().enumerate() {
            let plain = &draft.text[source_byte..source_range.start];
            display.push_str(plain);
            display_char += plain.chars().count();

            let text = draft.text[source_range.clone()].to_string();
            let label = paste_summary_label(&text);
            let end = display_char + label.chars().count();
            display.push_str(&label);
            pastes.push(PasteSpan {
                id: PasteId(index as u64 + 1),
                range: display_char..end,
                text,
            });
            display_char = end;
            source_byte = source_range.end;
        }

        display.push_str(&draft.text[source_byte..]);
        Self {
            display: buffer_of(&display),
            history: History::default(),
            next_paste_id: pastes.len() as u64 + 1,
            pastes,
        }
    }

    pub fn draft(&self) -> InputDraft {
        let display = self.display_text();
        let mut text = String::with_capacity(display.len());
        let mut paste_ranges = Vec::with_capacity(self.pastes.len());
        let mut display_char = 0;

        for paste in &self.pastes {
            text.push_str(char_slice(&display, display_char..paste.range.start));
            let start = text.len();
            text.push_str(&paste.text);
            paste_ranges.push(start..text.len());
            display_char = paste.range.end;
        }
        text.push_str(char_slice(&display, display_char..display.chars().count()));

        InputDraft { text, paste_ranges }
    }

    pub fn display_text(&self) -> String {
        self.display.lines().join("\n")
    }

    pub fn expanded_text(&self) -> String {
        self.draft().text
    }

    pub fn value(&self) -> String {
        self.expanded_text()
    }

    pub fn palette_text(&self) -> String {
        let mut text = self.project(|_| PALETTE_TOKEN);
        let display = self.display_text();
        if self.pastes.last().is_some_and(|paste| {
            char_slice(&display, paste.range.end..display.chars().count()) == " "
        }) {
            text.pop();
        }
        text
    }

    pub fn lines(&self) -> &[String] {
        self.display.lines()
    }

    pub fn x(&self) -> usize {
        self.display.cursor().col
    }

    pub fn y(&self) -> usize {
        self.display.cursor().line
    }

    pub fn line_count(&self) -> usize {
        self.display.line_count()
    }

    pub fn cursor_offset(&self) -> usize {
        self.offset_of(self.display.cursor())
    }

    /// The selection in char offsets over [`Self::display_text`], which is the
    /// space paste spans are recorded in.
    pub fn selection(&self) -> Option<Range<usize>> {
        let (start, end) = self.display.selection()?;
        Some(self.offset_of(start)..self.offset_of(end))
    }

    pub fn selected_text(&self) -> Option<String> {
        self.display.selected_text()
    }

    pub fn select_all(&mut self) {
        self.display.select_all();
    }

    pub fn set_cursor(&mut self, y: usize, x: usize) {
        self.set_caret(Cursor::new(y, x), false);
    }

    /// Drops the caret, extending the selection when asked, and keeps it out of
    /// the middle of a paste chip either way.
    pub fn set_caret(&mut self, cursor: Cursor, extend: bool) {
        self.display.set_cursor(cursor, extend);
        self.normalize_cursor(CursorDirection::Nearest);
    }

    pub fn move_to_end(&mut self) {
        self.display.move_document_end(false);
    }

    pub fn clear(&mut self) {
        self.display = Buffer::new(Vec::new());
        self.history = History::default();
        self.pastes.clear();
    }

    /// Steps back through the undo history, reporting whether anything moved.
    pub fn undo(&mut self) -> bool {
        self.replay(History::undo)
    }

    pub fn redo(&mut self) -> bool {
        self.replay(History::redo)
    }

    /// A replayed edit moves every offset after it, and a chip the edit covered
    /// is gone for good: undo restores the text, not the summary it stood
    /// behind, which is the same trade expanding a paste already makes.
    fn replay(&mut self, step: impl Fn(&mut History) -> Option<Edit>) -> bool {
        let Some(edit) = step(&mut self.history) else {
            return false;
        };
        let at = self.offset_of(edit.at);
        let removed = edit.removed.chars().count();
        let inserted = edit.inserted.chars().count();
        self.display.replay(&edit);
        self.apply_change(at..at + removed, inserted);
        true
    }

    pub fn has_pastes(&self) -> bool {
        !self.pastes.is_empty()
    }

    pub fn starts_with_shell_prefix(&self) -> bool {
        if let Some(paste) = self.pastes.first()
            && paste.range.start == 0
        {
            return paste.text.starts_with('!');
        }
        self.lines()
            .first()
            .is_some_and(|line| line.starts_with('!'))
    }

    /// Splices `text` over a char range, which is how the mention popup
    /// completes: replacing the whole buffer would drop every paste token.
    pub fn replace_range(&mut self, range: Range<usize>, text: &str) {
        self.replace(range, text);
    }

    pub fn insert_text(&mut self, text: &str) {
        let text = sanitize_paste(text);
        self.replace(self.edit_range(), &text);
    }

    #[cfg(test)]
    pub fn push_char(&mut self, character: char) {
        self.insert_text(&character.to_string());
    }

    pub fn insert_paste(&mut self, text: &str) -> PasteId {
        let text = sanitize_paste(text);
        let label = paste_summary_label(&text);
        let start = self.cursor_offset();
        let inserted = format!("{label} ");
        self.insert_text(&inserted);

        let id = PasteId(self.next_paste_id);
        self.next_paste_id += 1;
        let end = start + label.chars().count();
        let index = self
            .pastes
            .partition_point(|paste| paste.range.start < start);
        self.pastes.insert(
            index,
            PasteSpan {
                id,
                range: start..end,
                text,
            },
        );
        id
    }

    pub fn add_line(&mut self) {
        self.replace(self.edit_range(), "\n");
    }

    pub fn remove_char(&mut self) {
        let range = self.edit_range();
        if !range.is_empty() {
            self.replace(range, "");
        } else if range.start > 0 {
            self.replace(range.start - 1..range.start, "");
        }
    }

    #[cfg(test)]
    pub fn move_left(&mut self) {
        self.display.move_left(false);
        self.normalize_cursor(CursorDirection::Left);
    }

    #[cfg(test)]
    pub fn move_right(&mut self) {
        self.display.move_right(false);
        self.normalize_cursor(CursorDirection::Right);
    }

    #[cfg(test)]
    pub fn move_up(&mut self) {
        self.display.move_vertical(-1, false);
        self.normalize_cursor(CursorDirection::Nearest);
    }

    #[cfg(test)]
    pub fn move_home(&mut self) {
        self.display.move_home(false);
        self.normalize_cursor(CursorDirection::Nearest);
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> EditResult {
        let modifiers = key.modifiers;
        // AltGr arrives as Ctrl+Alt and is text, not a chord, so Ctrl only
        // counts on its own.
        let control =
            modifiers.contains(KeyModifiers::CONTROL) && !modifiers.contains(KeyModifiers::ALT);
        let super_key = modifiers.contains(KeyModifiers::SUPER);

        // Nothing binds bare Alt any more, and falling through would insert
        // the chord's letter as stray text.
        if modifiers.contains(KeyModifiers::ALT) && !modifiers.contains(KeyModifiers::CONTROL) {
            return EditResult::Ignored;
        }

        if control {
            return match key.code {
                KeyCode::Backspace | KeyCode::Char('w') => {
                    self.delete_word_before();
                    EditResult::Changed
                }
                KeyCode::Delete => {
                    self.delete_word_after();
                    EditResult::Changed
                }
                KeyCode::Char('k') => {
                    self.delete_to_line_end();
                    EditResult::Changed
                }
                // Select-all displaces emacs' line-start, which `Home` still
                // reaches. Nothing else in the composer can take a whole draft
                // in one keystroke.
                KeyCode::Char('a') => {
                    self.select_all();
                    EditResult::Moved
                }
                KeyCode::Char('z') => self.stepped(Self::undo),
                KeyCode::Char('y') => self.stepped(Self::redo),
                KeyCode::Left | KeyCode::Right | KeyCode::Char('e') => self.move_with_key(key),
                _ => EditResult::Ignored,
            };
        }

        if super_key {
            return match key.code {
                KeyCode::Backspace => {
                    self.delete_to_line_start();
                    EditResult::Changed
                }
                KeyCode::Left | KeyCode::Right => self.move_with_key(key),
                _ => EditResult::Ignored,
            };
        }

        match key.code {
            KeyCode::Char(character) => {
                self.insert_text(&character.to_string());
                EditResult::Changed
            }
            KeyCode::Backspace => {
                self.remove_char();
                EditResult::Changed
            }
            KeyCode::Delete => {
                let range = self.edit_range();
                if !range.is_empty() {
                    self.replace(range, "");
                } else if range.end < self.display_text().chars().count() {
                    self.replace(range.start..range.start + 1, "");
                }
                EditResult::Changed
            }
            KeyCode::Left
            | KeyCode::Right
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::Up
            | KeyCode::Down => self.move_with_key(key),
            _ => EditResult::Ignored,
        }
    }

    fn stepped(&mut self, step: impl Fn(&mut Self) -> bool) -> EditResult {
        match step(self) {
            true => EditResult::Changed,
            false => EditResult::Ignored,
        }
    }

    /// Caret motions. Shift keeps the anchor so the motion grows a selection,
    /// which is what every other editor in the app does.
    fn move_with_key(&mut self, key: KeyEvent) -> EditResult {
        let extend = key.modifiers.contains(KeyModifiers::SHIFT);
        let control = key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT);
        let super_key = key.modifiers.contains(KeyModifiers::SUPER);
        match key.code {
            KeyCode::Left if control => self.display.move_word_left(extend),
            KeyCode::Left if super_key => self.display.move_home(extend),
            KeyCode::Left => self.display.move_left(extend),
            KeyCode::Right if control => self.display.move_word_right(extend),
            KeyCode::Right if super_key => self.display.move_end(extend),
            KeyCode::Right => self.display.move_right(extend),
            KeyCode::Char('e') => self.display.move_end(extend),
            KeyCode::Up => self.display.move_vertical(-1, extend),
            KeyCode::Down => self.display.move_vertical(1, extend),
            KeyCode::Home => self.display.move_home(extend),
            KeyCode::End => self.display.move_end(extend),
            _ => return EditResult::Ignored,
        }
        self.history.break_group();
        self.normalize_cursor(CursorDirection::from_key(key));
        EditResult::Moved
    }

    pub fn focused_paste(&self) -> Option<PasteId> {
        let cursor = self.cursor_offset();
        self.pastes
            .iter()
            .find(|paste| paste.range.start == cursor)
            .map(|paste| paste.id)
    }

    pub fn paste_at(&self, y: usize, x: usize) -> Option<PasteId> {
        let offset = line_start(self.lines(), y)? + x;
        self.pastes
            .iter()
            .find(|paste| paste.range.contains(&offset))
            .map(|paste| paste.id)
    }

    pub fn paste_text(&self, id: PasteId) -> Option<&str> {
        self.pastes
            .iter()
            .find(|paste| paste.id == id)
            .map(|paste| paste.text.as_str())
    }

    pub fn focus_paste(&mut self, id: PasteId) -> bool {
        let Some(paste) = self.pastes.iter().find(|paste| paste.id == id) else {
            return false;
        };
        let cursor = self.cursor_of(paste.range.start);
        self.display.set_cursor(cursor, false);
        true
    }

    pub fn update_paste(&mut self, id: PasteId, text: &str) -> bool {
        let Some(index) = self.pastes.iter().position(|paste| paste.id == id) else {
            return false;
        };
        let text = sanitize_paste(text);
        let label = paste_summary_label(&text);
        let old_range = self.pastes[index].range.clone();
        let old_len = old_range.len();
        let new_len = label.chars().count();
        let cursor = self.cursor_offset();

        let start = self.cursor_of(old_range.start);
        let end = self.cursor_of(old_range.end);
        self.display.set_cursor(start, false);
        self.display.set_cursor(end, true);
        if let Some(edit) = self.display.insert(&label) {
            self.history.record(edit);
        }

        self.pastes[index].range = old_range.start..old_range.start + new_len;
        self.pastes[index].text = text;
        shift_spans(&mut self.pastes[index + 1..], old_len, new_len);

        let cursor = if cursor <= old_range.start {
            cursor
        } else if cursor >= old_range.end {
            shift_offset(cursor, old_len, new_len)
        } else {
            old_range.start
        };
        let cursor = self.cursor_of(cursor);
        self.display.set_cursor(cursor, false);
        true
    }

    /// Char offset of the first character of line `y`.
    pub fn line_offset(&self, y: usize) -> Option<usize> {
        line_start(self.lines(), y)
    }

    pub fn paste_ranges_on_line(&self, y: usize) -> Vec<(Range<usize>, PasteId)> {
        let Some(start) = line_start(self.lines(), y) else {
            return Vec::new();
        };
        let end = start + self.lines()[y].chars().count();
        self.pastes
            .iter()
            .filter(|paste| paste.range.start >= start && paste.range.end <= end)
            .map(|paste| (paste.range.start - start..paste.range.end - start, paste.id))
            .collect()
    }

    /// The selection clipped to line `y`, in line-local char offsets, so the
    /// painter can ask every line and paint the ones that come back.
    pub fn selection_on_line(&self, y: usize) -> Option<Range<usize>> {
        let selection = self.selection()?;
        let start = line_start(self.lines(), y)?;
        let end = start + self.lines()[y].chars().count();
        let lo = selection.start.max(start);
        let hi = selection.end.min(end);
        (lo < hi).then(|| lo - start..hi - start)
    }

    pub fn expand_pastes(&mut self) {
        self.display = buffer_of(&self.expanded_text());
        self.display.move_document_end(false);
        self.history = History::default();
        self.pastes.clear();
    }

    fn project<'a>(&'a self, replacement: impl Fn(&'a PasteSpan) -> &'a str) -> String {
        let display = self.display_text();
        let mut projected = String::with_capacity(display.len());
        let mut cursor = 0;
        for paste in &self.pastes {
            projected.push_str(char_slice(&display, cursor..paste.range.start));
            projected.push_str(replacement(paste));
            cursor = paste.range.end;
        }
        projected.push_str(char_slice(&display, cursor..display.chars().count()));
        projected
    }

    /// Splices `inserted` over a char range. A range that reaches into a paste
    /// chip swallows the whole chip: half a summary stands for nothing, and a
    /// selection that straddles one is the ordinary way to reach that state.
    fn replace(&mut self, mut removed: Range<usize>, inserted: &str) {
        loop {
            let mut expanded = removed.clone();
            for paste in &self.pastes {
                if ranges_intersect(&expanded, &paste.range) {
                    expanded.start = expanded.start.min(paste.range.start);
                    expanded.end = expanded.end.max(paste.range.end);
                }
            }
            if expanded == removed {
                break;
            }
            removed = expanded;
        }

        let start = self.cursor_of(removed.start);
        let end = self.cursor_of(removed.end);
        self.display.set_cursor(start, false);
        self.display.set_cursor(end, true);
        if let Some(edit) = self.display.insert(inserted) {
            self.history.record(edit);
        }
        self.apply_change(removed, inserted.chars().count());
    }

    /// The range an edit acts on: the selection when there is one, otherwise
    /// the caret.
    fn edit_range(&self) -> Range<usize> {
        self.selection().unwrap_or_else(|| {
            let cursor = self.cursor_offset();
            cursor..cursor
        })
    }

    /// Char offset of `cursor` in [`Self::display_text`].
    fn offset_of(&self, cursor: Cursor) -> usize {
        self.display
            .lines()
            .iter()
            .take(cursor.line)
            .map(|line| line.chars().count() + 1)
            .sum::<usize>()
            + cursor.col
    }

    /// Inverse of [`Self::offset_of`], clamped to the end of the text.
    fn cursor_of(&self, mut offset: usize) -> Cursor {
        for (line, text) in self.display.lines().iter().enumerate() {
            let len = text.chars().count();
            if offset <= len {
                return Cursor::new(line, offset);
            }
            offset -= len + 1;
        }
        let last = self.display.line_count().saturating_sub(1);
        Cursor::new(last, self.display.line(last).chars().count())
    }

    fn apply_change(&mut self, removed: Range<usize>, inserted_len: usize) {
        let removed_len = removed.len();
        self.pastes
            .retain(|paste| !ranges_intersect(&removed, &paste.range));
        for paste in &mut self.pastes {
            if paste.range.start >= removed.end {
                paste.range.start = shift_offset(paste.range.start, removed_len, inserted_len);
                paste.range.end = shift_offset(paste.range.end, removed_len, inserted_len);
            }
        }
    }

    fn normalize_cursor(&mut self, direction: CursorDirection) {
        let cursor = self.cursor_offset();
        let Some(paste) = self
            .pastes
            .iter()
            .find(|paste| paste.range.contains(&cursor))
        else {
            return;
        };
        let offset = match direction {
            CursorDirection::Left => paste.range.start,
            CursorDirection::Right => paste.range.end,
            CursorDirection::Nearest => {
                if cursor - paste.range.start <= paste.range.end - cursor {
                    paste.range.start
                } else {
                    paste.range.end
                }
            }
        };
        let cursor = self.cursor_of(offset);
        self.display.set_cursor(cursor, false);
    }

    fn delete_word_before(&mut self) {
        let cursor = self.cursor_offset();
        let x = self.x();
        if x == 0 {
            if cursor > 0 {
                self.replace(cursor - 1..cursor, "");
            }
            return;
        }
        let chars: Vec<char> = self.lines()[self.y()].chars().collect();
        let mut start = x;
        while start > 0 && chars[start - 1].is_ascii_whitespace() {
            start -= 1;
        }
        while start > 0 && !chars[start - 1].is_ascii_whitespace() {
            start -= 1;
        }
        self.replace(cursor - (x - start)..cursor, "");
    }

    fn delete_word_after(&mut self) {
        let cursor = self.cursor_offset();
        let x = self.x();
        let chars: Vec<char> = self.lines()[self.y()].chars().collect();
        if x == chars.len() {
            if self.y() + 1 < self.line_count() {
                self.replace(cursor..cursor + 1, "");
            }
            return;
        }
        let mut end = x;
        while end < chars.len() && chars[end].is_ascii_whitespace() {
            end += 1;
        }
        while end < chars.len() && !chars[end].is_ascii_whitespace() {
            end += 1;
        }
        self.replace(cursor..cursor + end - x, "");
    }

    fn delete_to_line_end(&mut self) {
        let cursor = self.cursor_offset();
        let remaining = self.lines()[self.y()].chars().count() - self.x();
        self.replace(cursor..cursor + remaining, "");
    }

    fn delete_to_line_start(&mut self) {
        let cursor = self.cursor_offset();
        self.replace(cursor - self.x()..cursor, "");
    }
}

pub(crate) fn sanitize_paste(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\t', TAB_SPACES)
}

pub(crate) fn should_summarize_paste(text: &str) -> bool {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    !text.trim().is_empty()
        && (text.encode_utf16().count() > PASTE_TOKEN_MIN_CHARACTERS
            || paste_line_count(&text) >= PASTE_TOKEN_MIN_LINES)
}

pub(crate) fn paste_summary_label(text: &str) -> String {
    let lines = paste_line_count(&sanitize_paste(text)).max(1);
    let suffix = if lines == 1 { "" } else { "s" };
    format!("[Pasted {lines} line{suffix}]")
}

fn paste_line_count(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.split('\n').count()
    }
}

#[derive(Clone, Copy)]
enum CursorDirection {
    Left,
    Right,
    Nearest,
}

impl CursorDirection {
    fn from_key(key: KeyEvent) -> Self {
        match key.code {
            KeyCode::Left => Self::Left,
            KeyCode::Right => Self::Right,
            KeyCode::Home if key.modifiers.contains(KeyModifiers::SUPER) => Self::Left,
            KeyCode::End if key.modifiers.contains(KeyModifiers::SUPER) => Self::Right,
            _ => Self::Nearest,
        }
    }
}

fn valid_ranges(text: &str, ranges: &[Range<usize>]) -> bool {
    let mut previous_end = 0;
    ranges.iter().all(|range| {
        let valid = range.start < range.end
            && range.start >= previous_end
            && range.end <= text.len()
            && text.is_char_boundary(range.start)
            && text.is_char_boundary(range.end);
        previous_end = range.end;
        valid
    })
}

fn buffer_of(text: &str) -> Buffer {
    Buffer::new(text.split('\n').map(str::to_owned).collect())
}

fn line_start(lines: &[String], y: usize) -> Option<usize> {
    (y < lines.len()).then(|| {
        lines
            .iter()
            .take(y)
            .map(|line| line.chars().count() + 1)
            .sum()
    })
}

fn char_to_byte(text: &str, chars: usize) -> usize {
    text.char_indices()
        .nth(chars)
        .map_or(text.len(), |(offset, _)| offset)
}

fn char_slice(text: &str, range: Range<usize>) -> &str {
    let start = char_to_byte(text, range.start);
    let end = char_to_byte(text, range.end);
    &text[start..end]
}

fn ranges_intersect(left: &Range<usize>, right: &Range<usize>) -> bool {
    left.start < right.end && right.start < left.end
}

fn shift_offset(offset: usize, removed_len: usize, inserted_len: usize) -> usize {
    if inserted_len >= removed_len {
        offset + inserted_len - removed_len
    } else {
        offset - (removed_len - inserted_len)
    }
}

fn shift_spans(spans: &mut [PasteSpan], removed_len: usize, inserted_len: usize) {
    for span in spans {
        span.range.start = shift_offset(span.range.start, removed_len, inserted_len);
        span.range.end = shift_offset(span.range.end, removed_len, inserted_len);
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use test_case::test_case;

    use super::{InputDocument, InputDraft, paste_summary_label, should_summarize_paste};

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test_case("x", false ; "short")]
    #[test_case("a\nb", false ; "two_lines")]
    #[test_case("a\nb\nc", true ; "three_lines")]
    #[test_case(&"x".repeat(150), false ; "character_boundary")]
    #[test_case(&"x".repeat(151), true ; "over_character_boundary")]
    #[test_case(" \n \n ", false ; "whitespace_only")]
    fn summary_threshold(text: &str, expected: bool) {
        assert_eq!(should_summarize_paste(text), expected);
    }

    #[test]
    fn normalizes_summary_label() {
        assert_eq!(paste_summary_label("a\r\nb\rc"), "[Pasted 3 lines]");
        assert_eq!(paste_summary_label("a\n"), "[Pasted 2 lines]");
    }

    #[test]
    fn inserts_and_expands_multiple_pastes() {
        let mut document = InputDocument::from_plain("Review ".into());
        document.move_to_end();
        document.insert_paste("a\nb\nc");
        document.insert_text("then ");
        document.insert_paste("x".repeat(151).as_str());

        assert_eq!(
            document.display_text(),
            "Review [Pasted 3 lines] then [Pasted 1 line] "
        );
        assert_eq!(
            document.expanded_text(),
            format!("Review a\nb\nc then {} ", "x".repeat(151))
        );
    }

    #[test]
    fn movement_and_deletion_treat_token_as_one_item() {
        let mut document = InputDocument::new();
        let id = document.insert_paste("a\nb\nc");
        document.handle_key(key(KeyCode::Left, KeyModifiers::NONE));
        document.handle_key(key(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(document.focused_paste(), Some(id));

        document.handle_key(key(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(document.cursor_offset(), "[Pasted 3 lines]".chars().count());
        document.handle_key(key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(document.display_text(), " ");
        assert_eq!(document.expanded_text(), " ");
    }

    #[test]
    fn word_deletion_removes_whole_intersected_token() {
        let mut document = InputDocument::from_plain("before ".into());
        document.move_to_end();
        document.insert_paste("a\nb\nc");
        document.handle_key(key(KeyCode::Left, KeyModifiers::NONE));
        document.handle_key(key(KeyCode::Backspace, KeyModifiers::CONTROL));
        assert_eq!(document.display_text(), "before  ");
        assert_eq!(document.expanded_text(), "before  ");
    }

    #[test]
    fn repeated_boundary_character_does_not_confuse_edits() {
        let mut document = InputDocument::from_plain("[".into());
        document.move_to_end();
        document.insert_paste("a\nb\nc");
        document.handle_key(key(KeyCode::Left, KeyModifiers::NONE));
        document.handle_key(key(KeyCode::Left, KeyModifiers::NONE));

        document.handle_key(key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(document.display_text(), "[Pasted 3 lines] ");
        assert_eq!(document.expanded_text(), "a\nb\nc ");

        document.handle_key(key(KeyCode::Char('['), KeyModifiers::NONE));
        assert_eq!(document.display_text(), "[[Pasted 3 lines] ");
        assert_eq!(document.expanded_text(), "[a\nb\nc ");
    }

    #[test]
    fn word_movement_skips_token() {
        let mut document = InputDocument::new();
        let id = document.insert_paste("a\nb\nc");
        document.handle_key(key(KeyCode::Left, KeyModifiers::NONE));
        document.handle_key(key(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(document.focused_paste(), Some(id));

        document.handle_key(key(KeyCode::Right, KeyModifiers::CONTROL));
        assert_eq!(document.cursor_offset(), "[Pasted 3 lines]".chars().count());
        document.handle_key(key(KeyCode::Left, KeyModifiers::CONTROL));
        assert_eq!(document.focused_paste(), Some(id));
    }

    #[test]
    fn character_threshold_matches_utf16_length() {
        assert!(should_summarize_paste(&"😀".repeat(76)));
    }

    #[test]
    fn palette_counts_terminal_paste_as_one_argument() {
        let mut document = InputDocument::from_plain("/cd ".into());
        document.move_to_end();
        document.insert_paste("a\nb\nc");
        assert_eq!(document.palette_text(), "/cd paste");

        document.insert_text("next");
        assert_eq!(document.palette_text(), "/cd paste next");
    }

    #[test]
    fn updating_paste_preserves_identity_and_shifts_following_token() {
        let mut document = InputDocument::new();
        let first = document.insert_paste("a\nb\nc");
        let second = document.insert_paste("d\ne\nf");

        assert!(document.update_paste(first, "one line"));
        assert_eq!(document.paste_text(first), Some("one line"));
        assert_eq!(document.paste_text(second), Some("d\ne\nf"));
        assert_eq!(document.display_text(), "[Pasted 1 line] [Pasted 3 lines] ");
        assert_eq!(document.expanded_text(), "one line d\ne\nf ");
    }

    #[test]
    fn draft_roundtrip_restores_tokens() {
        let draft = InputDraft {
            text: "Review a\nb\nc next".into(),
            paste_ranges: std::iter::once(7..12).collect(),
        };
        let document = InputDocument::from_draft(draft.clone());
        assert_eq!(document.display_text(), "Review [Pasted 3 lines] next");
        assert_eq!(document.draft(), draft);
    }

    #[test]
    fn invalid_draft_range_falls_back_to_plain_text() {
        let document = InputDocument::from_draft(InputDraft {
            text: "é".into(),
            paste_ranges: std::iter::once(1..2).collect(),
        });
        assert_eq!(document.display_text(), "é");
        assert!(!document.has_pastes());
    }
}
