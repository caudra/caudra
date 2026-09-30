use std::ops::Range;

use caudra_workbench::buffer::{Buffer, Cursor, Edit};
use caudra_workbench::history::History;
use caudra_workbench::text_field::{self, EditCommand, FieldKind, Motion, TextCommand, TextKey};
use caudra_workbench::words::{component_boundary_left, component_boundary_right};
use crossterm::event::KeyEvent;

use crate::highlight::TAB_SPACES;

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

    /// The selection with every paste chip it covers restored to the content the
    /// chip stands for. The display buffer holds labels, so reading the
    /// selection straight off it hands out `[Pasted 12 lines]` rather than the
    /// twelve lines, which is not what submitting the same draft would send.
    pub fn selected_text(&self) -> Option<String> {
        Some(self.project_range(self.selection()?, |paste| &paste.text))
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

    /// A key of the shared field keymap, run through the chip-aware edits so a
    /// paste chip moves and deletes as the one glyph it is drawn as.
    pub fn handle_key(&mut self, key: KeyEvent) -> TextKey {
        text_field::decode(key, FieldKind::Block)
            .map_or(TextKey::Ignored, |command| self.perform(command))
    }

    fn perform(&mut self, command: TextCommand) -> TextKey {
        match command {
            TextCommand::Edit(edit) => self.edit(edit),
            TextCommand::Move { motion, extend } => {
                self.display.move_by(motion, extend, 1);
                self.history.break_group();
                self.normalize_cursor(CursorDirection::of(motion));
                TextKey::Handled
            }
            // Select-all displaces emacs' line-start, which `Home` still
            // reaches. Nothing else in the composer can take a whole draft in
            // one keystroke.
            TextCommand::SelectAll => {
                self.select_all();
                TextKey::Handled
            }
            TextCommand::Undo => changed_if(self.undo()),
            TextCommand::Redo => changed_if(self.redo()),
            TextCommand::Copy => self.selected_text().map_or(TextKey::Ignored, TextKey::Copy),
            TextCommand::Cut => match self.selected_text() {
                Some(text) => {
                    self.replace(self.edit_range(), "");
                    TextKey::Cut(text)
                }
                None => TextKey::Handled,
            },
        }
    }

    fn edit(&mut self, command: EditCommand) -> TextKey {
        match command {
            EditCommand::Insert(character) => self.insert_text(character.encode_utf8(&mut [0; 4])),
            EditCommand::Newline | EditCommand::IndentedNewline => self.add_line(),
            EditCommand::Indent | EditCommand::Dedent => return TextKey::Ignored,
            deletion => {
                let range = self.deletion_range(deletion);
                if range.is_empty() {
                    return TextKey::Handled;
                }
                self.replace(range, "");
            }
        }
        TextKey::Changed
    }

    /// The chars a delete takes: the selection when there is one, except for a
    /// kill, which runs from the caret as it does in every other field. Words
    /// go a path component at a time, and a delete forward from the end of a
    /// line joins the next one.
    fn deletion_range(&self, command: EditCommand) -> Range<usize> {
        let kill = matches!(
            command,
            EditCommand::KillToLineEnd | EditCommand::KillToLineStart
        );
        if !kill && let Some(selection) = self.selection() {
            return selection;
        }
        let cursor = self.cursor_offset();
        let x = self.x();
        let chars: Vec<char> = self.lines()[self.y()].chars().collect();
        let forward = matches!(
            command,
            EditCommand::Delete | EditCommand::DeleteWordAfter | EditCommand::KillToLineEnd
        );
        if forward && x == chars.len() {
            let joins = self.y() + 1 < self.line_count();
            return cursor..cursor + usize::from(joins);
        }
        match command {
            EditCommand::Backspace => cursor.saturating_sub(1)..cursor,
            EditCommand::DeleteWordBefore if x == 0 => cursor.saturating_sub(1)..cursor,
            EditCommand::DeleteWordBefore => {
                cursor - (x - component_boundary_left(&chars, x))..cursor
            }
            EditCommand::KillToLineStart => cursor - x..cursor,
            EditCommand::Delete => cursor..cursor + 1,
            EditCommand::DeleteWordAfter => {
                cursor..cursor + component_boundary_right(&chars, x) - x
            }
            EditCommand::KillToLineEnd => cursor..cursor + chars.len() - x,
            _ => cursor..cursor,
        }
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
        self.project_range(0..self.display_text().chars().count(), replacement)
    }

    /// `range` of the display text with every chip it touches swapped for
    /// `replacement`. A chip the range only partly covers comes back whole, the
    /// rule [`Self::replace`] already applies to an edit that straddles one.
    fn project_range<'a>(
        &'a self,
        range: Range<usize>,
        replacement: impl Fn(&'a PasteSpan) -> &'a str,
    ) -> String {
        let display = self.display_text();
        let mut projected = String::with_capacity(range.len());
        let mut cursor = range.start;
        for paste in &self.pastes {
            if !ranges_intersect(&range, &paste.range) {
                continue;
            }
            projected.push_str(char_slice(&display, cursor..paste.range.start.max(cursor)));
            projected.push_str(replacement(paste));
            cursor = paste.range.end;
        }
        projected.push_str(char_slice(&display, cursor.min(range.end)..range.end));
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
        // Nudging the caret off a chip must not disturb the anchor: dropping it
        // here left a sweep that crossed a chip with nothing before the chip
        // selected, starting again at the edge it came out of.
        let extend = self.display.has_selection();
        let cursor = self.cursor_of(offset);
        self.display.set_cursor(cursor, extend);
    }
}

fn changed_if(changed: bool) -> TextKey {
    match changed {
        true => TextKey::Changed,
        false => TextKey::Handled,
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
    fn of(motion: Motion) -> Self {
        match motion {
            Motion::Left | Motion::WordLeft => Self::Left,
            Motion::Right | Motion::WordRight => Self::Right,
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

pub(crate) fn char_to_byte(text: &str, chars: usize) -> usize {
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
    use std::ops::Range;
    use test_case::test_case;

    use super::{InputDocument, InputDraft, paste_summary_label, should_summarize_paste};

    const PASTE_BODY: &str = "a\nb\nc";
    const PASTE_LABEL: &str = "[Pasted 3 lines]";

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

    /// A path goes one component per press, the separator leaving with the
    /// component it trails rather than costing a press of its own.
    #[test]
    fn word_deletion_takes_one_path_component() {
        let mut document = InputDocument::from_plain("read @src/main.rs".into());
        document.move_to_end();
        for expected in ["read @src/", "read @", "read "] {
            document.handle_key(key(KeyCode::Char('w'), KeyModifiers::CONTROL));
            assert_eq!(document.display_text(), expected);
        }
    }

    const FIRST_LINE: &str = "one";
    const LAST_LINE: &str = "two three";
    const DRAFT: &str = "one\ntwo three";
    const SELECTED_TAIL: &str = "ree";
    const SELECTED_HEAD: &str = "two";

    fn draft_at(y: usize, x: usize) -> InputDocument {
        let mut document = InputDocument::from_plain(DRAFT.into());
        document.set_cursor(y, x);
        document
    }

    #[test_case(KeyCode::Char('w'); "ctrl_w")]
    #[test_case(KeyCode::Backspace; "ctrl_backspace")]
    #[test_case(KeyCode::Delete; "ctrl_delete")]
    fn a_word_delete_takes_a_selection_whole(code: KeyCode) {
        let mut document = draft_at(1, LAST_LINE.len());
        for _ in SELECTED_TAIL.chars() {
            document.handle_key(key(KeyCode::Left, KeyModifiers::SHIFT));
        }
        document.handle_key(key(code, KeyModifiers::CONTROL));
        assert_eq!(
            document.display_text(),
            DRAFT.strip_suffix(SELECTED_TAIL).unwrap()
        );
    }

    /// A kill runs from the caret whatever is selected, as it does in every
    /// other field, rather than taking the selection the way a delete does.
    #[test]
    fn a_kill_runs_from_the_caret_past_a_selection() {
        let mut document = draft_at(1, 0);
        for _ in SELECTED_HEAD.chars() {
            document.handle_key(key(KeyCode::Right, KeyModifiers::SHIFT));
        }
        document.handle_key(key(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert_eq!(
            document.display_text(),
            format!("{FIRST_LINE}\n{SELECTED_HEAD}")
        );
    }

    #[test_case(0, FIRST_LINE.len(), KeyCode::Char('k'), KeyModifiers::CONTROL, "onetwo three", FIRST_LINE.len(); "ctrl_k_at_a_line_end_joins_the_next_line")]
    #[test_case(0, 0, KeyCode::Delete, KeyModifiers::SHIFT, DRAFT, 0; "shift_delete_with_nothing_selected_does_nothing")]
    #[test_case(1, LAST_LINE.len(), KeyCode::Home, KeyModifiers::CONTROL, DRAFT, 0; "ctrl_home_reaches_the_start_of_the_draft")]
    #[test_case(0, 0, KeyCode::End, KeyModifiers::CONTROL, DRAFT, DRAFT.len(); "ctrl_end_reaches_the_end_of_the_draft")]
    fn keys_follow_the_shared_edge_rules(
        y: usize,
        x: usize,
        code: KeyCode,
        modifiers: KeyModifiers,
        text: &str,
        caret: usize,
    ) {
        let mut document = draft_at(y, x);
        document.handle_key(key(code, modifiers));
        assert_eq!(document.display_text(), text);
        assert_eq!(document.cursor_offset(), caret);
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

    fn select(document: &mut InputDocument, range: Range<usize>) {
        let start = document.cursor_of(range.start);
        let end = document.cursor_of(range.end);
        document.set_caret(start, false);
        document.set_caret(end, true);
    }

    fn with_one_paste() -> InputDocument {
        let mut document = InputDocument::from_plain("Review ".into());
        document.move_to_end();
        document.insert_paste(PASTE_BODY);
        document
    }

    #[test]
    fn selection_across_a_chip_copies_the_pasted_text() {
        let mut document = with_one_paste();
        let whole = document.display_text().chars().count();
        select(&mut document, 0..whole);

        assert_eq!(document.selected_text().as_deref(), Some("Review a\nb\nc "));
    }

    #[test]
    fn selection_of_only_the_chip_copies_its_text() {
        let mut document = with_one_paste();
        let start = "Review ".chars().count();
        select(&mut document, start..start + PASTE_LABEL.chars().count());

        assert_eq!(document.selected_text().as_deref(), Some(PASTE_BODY));
    }

    #[test]
    fn selection_clear_of_every_chip_is_left_alone() {
        let mut document = InputDocument::from_plain("plain words".into());
        select(&mut document, 0..5);

        assert_eq!(document.selected_text().as_deref(), Some("plain"));
    }

    #[test]
    fn selection_expands_each_chip_in_order() {
        let mut document = InputDocument::from_plain("Review ".into());
        document.move_to_end();
        document.insert_paste(PASTE_BODY);
        document.insert_text("then ");
        document.insert_paste("x\ny\nz");
        let whole = document.display_text().chars().count();
        select(&mut document, 0..whole);

        assert_eq!(
            document.selected_text().as_deref(),
            Some("Review a\nb\nc then x\ny\nz ")
        );
    }

    #[test]
    fn selecting_everything_copies_what_submitting_would_send() {
        let mut document = with_one_paste();
        document.select_all();

        assert_eq!(document.selected_text(), Some(document.value()));
    }

    /// Nudging the caret off a chip must not disturb the anchor. It used to,
    /// so a sweep that crossed a chip lost everything selected before it and
    /// started again at the chip's far edge.
    #[test]
    fn extending_across_a_chip_keeps_what_came_before_it() {
        let mut document = with_one_paste();
        let before_chip = "Rev".chars().count();
        let after_chip = document.display_text().chars().count();
        document.set_caret(document.cursor_of(before_chip), false);

        for offset in before_chip + 1..=after_chip {
            let step = document.cursor_of(offset);
            document.set_caret(step, true);
        }

        assert_eq!(document.selected_text().as_deref(), Some("iew a\nb\nc "));
    }

    /// The same crossing by keyboard, which reaches `normalize_cursor` through
    /// `handle_key` rather than through a drag.
    #[test]
    fn shift_right_across_a_chip_keeps_what_came_before_it() {
        let mut document = with_one_paste();
        document.set_caret(document.cursor_of(0), false);
        let steps = document.display_text().chars().count();

        for _ in 0..steps {
            document.handle_key(key(KeyCode::Right, KeyModifiers::SHIFT));
        }

        assert_eq!(document.selected_text().as_deref(), Some("Review a\nb\nc "));
    }

    /// The caret is kept off the middle of a chip, so this range is not one the
    /// composer can produce. The clamp still has to hold: half a summary stands
    /// for nothing, which is the rule `replace` applies to the same overlap.
    #[test]
    fn partly_covered_chip_comes_back_whole() {
        let document = with_one_paste();
        let inside = "Review ".chars().count() + 2;

        assert_eq!(
            document.project_range(0..inside, |paste| &paste.text),
            "Review a\nb\nc"
        );
    }
}
