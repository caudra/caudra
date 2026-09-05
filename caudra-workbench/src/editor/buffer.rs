//! The text of one open file, plus a cursor and a selection.
//!
//! Lines are a `Vec<String>` rather than a rope. The workbench opens files to
//! read them and to make targeted edits, both of which touch one line at a
//! time, and the read path already refuses anything over eight megabytes. A
//! rope would buy nothing here and cost a dependency.
//!
//! Columns are character offsets throughout. Display width belongs to the
//! renderer, and mixing the two is how a cursor ends up inside a glyph.

use std::ops::Range;

const INDENT_WIDTH: usize = 4;
const TAB: char = '\t';

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Cursor {
    pub line: usize,
    pub col: usize,
}

impl Cursor {
    pub const fn new(line: usize, col: usize) -> Self {
        Self { line, col }
    }
}

/// One reversible change. `removed` and `inserted` are both recorded so undo
/// and redo are the same operation with the two swapped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub at: Cursor,
    pub removed: String,
    pub inserted: String,
    pub cursor_before: Cursor,
    pub cursor_after: Cursor,
}

impl Edit {
    pub fn inverted(&self) -> Self {
        Self {
            at: self.at,
            removed: self.inserted.clone(),
            inserted: self.removed.clone(),
            cursor_before: self.cursor_after,
            cursor_after: self.cursor_before,
        }
    }

    /// What a coalescing history uses to decide whether two edits are one
    /// gesture. A newline always breaks the run: it is the natural undo
    /// boundary in every editor worth copying.
    pub fn is_plain_insert(&self) -> bool {
        self.removed.is_empty() && !self.inserted.contains('\n') && !self.inserted.is_empty()
    }

    pub fn is_plain_delete(&self) -> bool {
        self.inserted.is_empty() && !self.removed.contains('\n') && !self.removed.is_empty()
    }
}

/// Which of tab or spaces this file indents with, taken from the file itself so
/// editing a tab-indented file does not quietly convert it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Indent {
    Tab,
    Spaces(usize),
}

impl Indent {
    fn detect(lines: &[String]) -> Self {
        for line in lines {
            match line.chars().next() {
                Some(TAB) => return Self::Tab,
                Some(' ') => return Self::Spaces(INDENT_WIDTH),
                _ => {}
            }
        }
        Self::Spaces(INDENT_WIDTH)
    }

    fn unit(self) -> String {
        match self {
            Self::Tab => TAB.to_string(),
            Self::Spaces(width) => " ".repeat(width),
        }
    }
}

pub struct Buffer {
    lines: Vec<String>,
    cursor: Cursor,
    anchor: Option<Cursor>,
    indent: Indent,
    /// The column a vertical move aims for, so walking down past a short line
    /// and back up returns to where the cursor started rather than to the
    /// short line's end.
    goal_col: Option<usize>,
}

impl Buffer {
    pub fn new(lines: Vec<String>) -> Self {
        let lines = if lines.is_empty() {
            vec![String::new()]
        } else {
            lines
        };
        Self {
            indent: Indent::detect(&lines),
            lines,
            cursor: Cursor::default(),
            anchor: None,
            goal_col: None,
        }
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    pub fn has_selection(&self) -> bool {
        self.anchor.is_some_and(|anchor| anchor != self.cursor)
    }

    /// The selection in document order, whichever end the cursor sits on.
    pub fn selection(&self) -> Option<(Cursor, Cursor)> {
        let anchor = self.anchor?;
        if anchor == self.cursor {
            return None;
        }
        Some(if anchor < self.cursor {
            (anchor, self.cursor)
        } else {
            (self.cursor, anchor)
        })
    }

    pub fn selected_text(&self) -> Option<String> {
        let (start, end) = self.selection()?;
        Some(self.text_between(start, end))
    }

    pub fn select_all(&mut self) {
        self.anchor = Some(Cursor::default());
        self.cursor = self.end_of_document();
        self.goal_col = None;
    }

    pub fn set_cursor(&mut self, cursor: Cursor, extend: bool) {
        self.begin_move(extend);
        self.cursor = self.clamp(cursor);
        self.goal_col = None;
    }

    /// The buffer as one string, without the file's trailing newline, which
    /// belongs to the tab rather than the text. Assertions want the whole
    /// buffer at once; the renderer only ever wants a line.
    #[cfg(test)]
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn line(&self, index: usize) -> &str {
        self.lines.get(index).map_or("", String::as_str)
    }

    // Motion.

    pub fn move_left(&mut self, extend: bool) {
        self.begin_move(extend);
        if self.cursor.col > 0 {
            self.cursor.col -= 1;
        } else if self.cursor.line > 0 {
            self.cursor.line -= 1;
            self.cursor.col = self.line_len(self.cursor.line);
        }
        self.goal_col = None;
    }

    pub fn move_right(&mut self, extend: bool) {
        self.begin_move(extend);
        if self.cursor.col < self.line_len(self.cursor.line) {
            self.cursor.col += 1;
        } else if self.cursor.line + 1 < self.lines.len() {
            self.cursor.line += 1;
            self.cursor.col = 0;
        }
        self.goal_col = None;
    }

    pub fn move_vertical(&mut self, delta: isize, extend: bool) {
        self.begin_move(extend);
        let goal = *self.goal_col.get_or_insert(self.cursor.col);
        let target = self
            .cursor
            .line
            .saturating_add_signed(delta)
            .min(self.lines.len().saturating_sub(1));
        self.cursor.line = target;
        self.cursor.col = goal.min(self.line_len(target));
    }

    pub fn move_home(&mut self, extend: bool) {
        self.begin_move(extend);
        let indent = self.indent_width(self.cursor.line);
        // First press lands on the text, second on column zero, which is what
        // every editor with a Home key does.
        self.cursor.col = if self.cursor.col == indent { 0 } else { indent };
        self.goal_col = None;
    }

    pub fn move_end(&mut self, extend: bool) {
        self.begin_move(extend);
        self.cursor.col = self.line_len(self.cursor.line);
        self.goal_col = None;
    }

    pub fn move_document_start(&mut self, extend: bool) {
        self.begin_move(extend);
        self.cursor = Cursor::default();
        self.goal_col = None;
    }

    pub fn move_document_end(&mut self, extend: bool) {
        self.begin_move(extend);
        self.cursor = self.end_of_document();
        self.goal_col = None;
    }

    pub fn move_word_left(&mut self, extend: bool) {
        self.begin_move(extend);
        self.cursor = self.word_boundary_left(self.cursor);
        self.goal_col = None;
    }

    pub fn move_word_right(&mut self, extend: bool) {
        self.begin_move(extend);
        self.cursor = self.word_boundary_right(self.cursor);
        self.goal_col = None;
    }

    pub fn goto_line(&mut self, line_number: usize) {
        let line = line_number.saturating_sub(1).min(self.lines.len() - 1);
        self.anchor = None;
        self.cursor = Cursor::new(line, 0);
        self.goal_col = None;
    }

    // Editing. Each returns the edit it made so the caller can record it.

    pub fn insert(&mut self, text: &str) -> Option<Edit> {
        let (start, end) = self.selection().unwrap_or((self.cursor, self.cursor));
        self.apply(start, end, text)
    }

    /// The new line inherits the current line's indentation, unless the cursor
    /// is still inside that indentation: splitting whitespace should divide it,
    /// not add more.
    pub fn insert_newline(&mut self) -> Option<Edit> {
        let width = self.indent_width(self.cursor.line);
        let indent: String = if self.cursor.col >= width {
            self.line(self.cursor.line).chars().take(width).collect()
        } else {
            String::new()
        };
        self.insert(&format!("\n{indent}"))
    }

    pub fn insert_indent(&mut self) -> Option<Edit> {
        if self.has_selection() {
            return self.shift_selection(true);
        }
        self.insert(&self.indent.unit())
    }

    pub fn dedent(&mut self) -> Option<Edit> {
        self.shift_selection(false)
    }

    pub fn backspace(&mut self) -> Option<Edit> {
        if let Some((start, end)) = self.selection() {
            return self.apply(start, end, "");
        }
        if self.cursor == Cursor::default() {
            return None;
        }
        let start = self.previous_position(self.cursor);
        self.apply(start, self.cursor, "")
    }

    pub fn delete(&mut self) -> Option<Edit> {
        if let Some((start, end)) = self.selection() {
            return self.apply(start, end, "");
        }
        let end = self.next_position(self.cursor);
        if end == self.cursor {
            return None;
        }
        self.apply(self.cursor, end, "")
    }

    pub fn delete_word_left(&mut self) -> Option<Edit> {
        if self.has_selection() {
            return self.backspace();
        }
        let start = self.word_boundary_left(self.cursor);
        if start == self.cursor {
            return None;
        }
        self.apply(start, self.cursor, "")
    }

    pub fn delete_word_right(&mut self) -> Option<Edit> {
        if self.has_selection() {
            return self.delete();
        }
        let end = self.word_boundary_right(self.cursor);
        if end == self.cursor {
            return None;
        }
        self.apply(self.cursor, end, "")
    }

    /// Kills to the end of the line, or joins the next line when already there,
    /// matching the readline behaviour the composer uses.
    pub fn kill_to_end_of_line(&mut self) -> Option<Edit> {
        let end_of_line = Cursor::new(self.cursor.line, self.line_len(self.cursor.line));
        let end = if self.cursor == end_of_line {
            self.next_position(self.cursor)
        } else {
            end_of_line
        };
        if end == self.cursor {
            return None;
        }
        self.apply(self.cursor, end, "")
    }

    /// Replays an edit, for undo and redo. The cursor lands where the edit
    /// recorded, not where the replay happens to leave it.
    pub fn replay(&mut self, edit: &Edit) {
        let end = self.offset_by(edit.at, &edit.removed);
        self.splice(edit.at, end, &edit.inserted);
        self.anchor = None;
        self.cursor = self.clamp(edit.cursor_after);
        self.goal_col = None;
    }

    fn apply(&mut self, start: Cursor, end: Cursor, text: &str) -> Option<Edit> {
        let removed = self.text_between(start, end);
        if removed.is_empty() && text.is_empty() {
            return None;
        }
        let cursor_before = self.cursor;
        self.splice(start, end, text);
        let cursor_after = self.offset_by(start, text);
        self.anchor = None;
        self.cursor = cursor_after;
        self.goal_col = None;
        Some(Edit {
            at: start,
            removed,
            inserted: text.to_owned(),
            cursor_before,
            cursor_after,
        })
    }

    fn shift_selection(&mut self, deeper: bool) -> Option<Edit> {
        let (start, end) = self.selection().unwrap_or((self.cursor, self.cursor));
        let first = start.line;
        let last = end.line;
        let unit = self.indent.unit();

        let block_start = Cursor::new(first, 0);
        let block_end = Cursor::new(last, self.line_len(last));
        let mut shifted: Vec<String> = Vec::with_capacity(last - first + 1);
        let mut moved = false;
        for index in first..=last {
            let line = self.line(index);
            if deeper {
                if line.is_empty() {
                    shifted.push(String::new());
                } else {
                    moved = true;
                    shifted.push(format!("{unit}{line}"));
                }
            } else if let Some(rest) = strip_one_indent(line, &unit) {
                moved = true;
                shifted.push(rest.to_owned());
            } else {
                shifted.push(line.to_owned());
            }
        }
        if !moved {
            return None;
        }
        let edit = self.apply(block_start, block_end, &shifted.join("\n"))?;
        self.anchor = Some(Cursor::new(first, 0));
        self.cursor = Cursor::new(last, self.line_len(last));
        Some(edit)
    }

    // Geometry helpers.

    fn begin_move(&mut self, extend: bool) {
        if extend {
            self.anchor.get_or_insert(self.cursor);
        } else {
            self.anchor = None;
        }
    }

    fn line_len(&self, index: usize) -> usize {
        self.lines.get(index).map_or(0, |line| line.chars().count())
    }

    fn indent_width(&self, index: usize) -> usize {
        self.line(index)
            .chars()
            .take_while(|c| *c == ' ' || *c == TAB)
            .count()
    }

    fn end_of_document(&self) -> Cursor {
        let line = self.lines.len() - 1;
        Cursor::new(line, self.line_len(line))
    }

    fn clamp(&self, cursor: Cursor) -> Cursor {
        let line = cursor.line.min(self.lines.len() - 1);
        Cursor::new(line, cursor.col.min(self.line_len(line)))
    }

    fn previous_position(&self, cursor: Cursor) -> Cursor {
        if cursor.col > 0 {
            Cursor::new(cursor.line, cursor.col - 1)
        } else if cursor.line > 0 {
            Cursor::new(cursor.line - 1, self.line_len(cursor.line - 1))
        } else {
            cursor
        }
    }

    fn next_position(&self, cursor: Cursor) -> Cursor {
        if cursor.col < self.line_len(cursor.line) {
            Cursor::new(cursor.line, cursor.col + 1)
        } else if cursor.line + 1 < self.lines.len() {
            Cursor::new(cursor.line + 1, 0)
        } else {
            cursor
        }
    }

    fn text_between(&self, start: Cursor, end: Cursor) -> String {
        if start == end {
            return String::new();
        }
        if start.line == end.line {
            return slice(self.line(start.line), start.col..end.col);
        }
        let mut out = slice(self.line(start.line), start.col..self.line_len(start.line));
        for index in start.line + 1..end.line {
            out.push('\n');
            out.push_str(self.line(index));
        }
        out.push('\n');
        out.push_str(&slice(self.line(end.line), 0..end.col));
        out
    }

    fn splice(&mut self, start: Cursor, end: Cursor, text: &str) {
        let head = slice(self.line(start.line), 0..start.col);
        let tail = slice(self.line(end.line), end.col..self.line_len(end.line));

        let mut replacement: Vec<String> = text.split('\n').map(str::to_owned).collect();
        let last = replacement.len() - 1;
        replacement[0] = format!("{head}{}", replacement[0]);
        replacement[last].push_str(&tail);

        self.lines.splice(start.line..=end.line, replacement);
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
    }

    fn offset_by(&self, start: Cursor, text: &str) -> Cursor {
        match text.rfind('\n') {
            None => Cursor::new(start.line, start.col + text.chars().count()),
            Some(index) => Cursor::new(
                start.line + text.matches('\n').count(),
                text[index + 1..].chars().count(),
            ),
        }
    }

    fn word_boundary_left(&self, cursor: Cursor) -> Cursor {
        if cursor.col == 0 {
            return self.previous_position(cursor);
        }
        let chars: Vec<char> = self.line(cursor.line).chars().collect();
        let mut col = cursor.col;
        while col > 0 && !chars[col - 1].is_alphanumeric() && chars[col - 1] != '_' {
            col -= 1;
        }
        while col > 0 && (chars[col - 1].is_alphanumeric() || chars[col - 1] == '_') {
            col -= 1;
        }
        Cursor::new(cursor.line, col)
    }

    fn word_boundary_right(&self, cursor: Cursor) -> Cursor {
        let len = self.line_len(cursor.line);
        if cursor.col >= len {
            return self.next_position(cursor);
        }
        let chars: Vec<char> = self.line(cursor.line).chars().collect();
        let mut col = cursor.col;
        while col < len && (chars[col].is_alphanumeric() || chars[col] == '_') {
            col += 1;
        }
        while col < len && !chars[col].is_alphanumeric() && chars[col] != '_' {
            col += 1;
        }
        Cursor::new(cursor.line, col)
    }
}

fn slice(line: &str, range: Range<usize>) -> String {
    line.chars()
        .skip(range.start)
        .take(range.end.saturating_sub(range.start))
        .collect()
}

fn strip_one_indent<'a>(line: &'a str, unit: &str) -> Option<&'a str> {
    line.strip_prefix(unit)
        .or_else(|| line.strip_prefix(TAB))
        .or_else(|| {
            let spaces = line.len() - line.trim_start_matches(' ').len();
            (spaces > 0).then(|| &line[spaces.min(unit.len())..])
        })
}

#[cfg(test)]
mod tests {
    use super::{Buffer, Cursor, Indent};
    use test_case::test_case;

    const ROUND_TRIP: &str = "undoing an edit must restore the text exactly";
    const CURSOR_RESTORED: &str = "undoing an edit must put the cursor back where it was";
    const GOAL_COLUMN: &str =
        "walking past a short line and back must return to the original column";
    const SELECTION_ORDER: &str =
        "a selection must read in document order whichever way it was made";
    const NO_CONVERT: &str = "a tab-indented file must not be silently converted to spaces";

    fn buffer(text: &str) -> Buffer {
        Buffer::new(text.split('\n').map(str::to_owned).collect())
    }

    #[test]
    fn an_empty_buffer_still_has_one_line() {
        let buffer = Buffer::new(Vec::new());
        assert_eq!(buffer.line_count(), 1);
        assert_eq!(buffer.text(), "");
    }

    #[test_case("hello",           Cursor::new(0, 0), "X",    "Xhello"          ; "at_start")]
    #[test_case("hello",           Cursor::new(0, 5), "X",    "helloX"          ; "at_end")]
    #[test_case("hello",           Cursor::new(0, 2), "XY",   "heXYllo"         ; "in_middle")]
    #[test_case("a\nb",            Cursor::new(0, 1), "\nX",  "a\nX\nb"         ; "newline")]
    #[test_case("日本語",           Cursor::new(0, 1), "X",    "日X本語"          ; "wide_glyphs")]
    fn inserting_lands_where_the_cursor_is(start: &str, at: Cursor, text: &str, expected: &str) {
        let mut buffer = buffer(start);
        buffer.set_cursor(at, false);
        buffer.insert(text);
        assert_eq!(buffer.text(), expected);
    }

    #[test_case("hello world", Cursor::new(0, 5) ; "mid_line")]
    #[test_case("a\nbb\nccc", Cursor::new(1, 1) ; "across_lines")]
    fn an_edit_and_its_inverse_cancel(text: &str, at: Cursor) {
        let mut buffer = buffer(text);
        buffer.set_cursor(at, false);
        let before = buffer.text();

        let edit = buffer.insert("inserted text").unwrap();
        assert_ne!(buffer.text(), before);

        buffer.replay(&edit.inverted());
        assert_eq!(buffer.text(), before, "{ROUND_TRIP}");
        assert_eq!(buffer.cursor(), at, "{CURSOR_RESTORED}");
    }

    #[test]
    fn deleting_a_selection_and_undoing_restores_it() {
        let mut buffer = buffer("one\ntwo\nthree");
        buffer.set_cursor(Cursor::new(0, 1), false);
        buffer.set_cursor(Cursor::new(2, 2), true);
        let before = buffer.text();

        let edit = buffer.backspace().unwrap();
        assert_eq!(buffer.text(), "oree");

        buffer.replay(&edit.inverted());
        assert_eq!(buffer.text(), before, "{ROUND_TRIP}");
    }

    #[test]
    fn backspace_at_the_start_of_a_line_joins_it_to_the_one_above() {
        let mut buffer = buffer("one\ntwo");
        buffer.set_cursor(Cursor::new(1, 0), false);
        buffer.backspace();
        assert_eq!(buffer.text(), "onetwo");
        assert_eq!(buffer.cursor(), Cursor::new(0, 3));
    }

    #[test]
    fn backspace_at_the_very_start_does_nothing() {
        let mut buffer = buffer("one");
        assert!(buffer.backspace().is_none());
        assert_eq!(buffer.text(), "one");
    }

    #[test]
    fn delete_at_the_very_end_does_nothing() {
        let mut buffer = buffer("one");
        buffer.move_document_end(false);
        assert!(buffer.delete().is_none());
    }

    #[test]
    fn a_new_line_inherits_the_indentation_above_it() {
        let mut buffer = buffer("    indented");
        buffer.move_end(false);
        buffer.insert_newline();
        assert_eq!(buffer.text(), "    indented\n    ");
        assert_eq!(buffer.cursor(), Cursor::new(1, 4));
    }

    #[test]
    fn splitting_a_line_does_not_indent_past_the_split() {
        let mut buffer = buffer("        deep");
        buffer.set_cursor(Cursor::new(0, 2), false);
        buffer.insert_newline();
        assert_eq!(buffer.text(), "  \n      deep");
    }

    #[test]
    fn the_goal_column_survives_a_short_line() {
        let mut buffer = buffer("longest line here\nx\nanother long line");
        buffer.set_cursor(Cursor::new(0, 15), false);
        buffer.move_vertical(1, false);
        assert_eq!(buffer.cursor(), Cursor::new(1, 1));
        buffer.move_vertical(1, false);
        assert_eq!(buffer.cursor(), Cursor::new(2, 15), "{GOAL_COLUMN}");
    }

    #[test]
    fn a_horizontal_move_forgets_the_goal_column() {
        let mut buffer = buffer("longest line here\nx\nanother long line");
        buffer.set_cursor(Cursor::new(0, 15), false);
        buffer.move_vertical(1, false);
        buffer.move_left(false);
        buffer.move_vertical(1, false);
        assert_eq!(buffer.cursor(), Cursor::new(2, 0));
    }

    #[test]
    fn home_alternates_between_the_text_and_the_margin() {
        let mut buffer = buffer("    indented");
        buffer.move_end(false);
        buffer.move_home(false);
        assert_eq!(buffer.cursor().col, 4);
        buffer.move_home(false);
        assert_eq!(buffer.cursor().col, 0);
    }

    #[test]
    fn a_selection_reads_in_document_order_either_way() {
        let mut forward = buffer("one\ntwo");
        forward.set_cursor(Cursor::new(0, 1), false);
        forward.set_cursor(Cursor::new(1, 2), true);

        let mut backward = buffer("one\ntwo");
        backward.set_cursor(Cursor::new(1, 2), false);
        backward.set_cursor(Cursor::new(0, 1), true);

        assert_eq!(
            forward.selected_text(),
            backward.selected_text(),
            "{SELECTION_ORDER}"
        );
        assert_eq!(forward.selected_text().unwrap(), "ne\ntw");
    }

    #[test]
    fn selecting_everything_covers_the_whole_buffer() {
        let mut buffer = buffer("one\ntwo\nthree");
        buffer.select_all();
        assert_eq!(buffer.selected_text().unwrap(), "one\ntwo\nthree");
    }

    #[test]
    fn a_cursor_with_no_selection_selects_nothing() {
        let buffer = buffer("one");
        assert!(buffer.selection().is_none());
        assert!(!buffer.has_selection());
    }

    #[test_case("word another", Cursor::new(0, 12), Cursor::new(0, 5) ; "from_end")]
    #[test_case("word another", Cursor::new(0, 4),  Cursor::new(0, 0) ; "from_word_end")]
    fn word_motion_stops_at_word_starts(text: &str, from: Cursor, expected: Cursor) {
        let mut buffer = buffer(text);
        buffer.set_cursor(from, false);
        buffer.move_word_left(false);
        assert_eq!(buffer.cursor(), expected);
    }

    #[test]
    fn deleting_a_word_removes_exactly_that_word() {
        let mut buffer = buffer("alpha beta gamma");
        buffer.move_end(false);
        buffer.delete_word_left();
        assert_eq!(buffer.text(), "alpha beta ");
    }

    #[test]
    fn killing_to_the_end_of_a_line_joins_when_already_there() {
        let mut buffer = buffer("one\ntwo");
        buffer.set_cursor(Cursor::new(0, 1), false);
        buffer.kill_to_end_of_line();
        assert_eq!(buffer.text(), "o\ntwo");
        buffer.kill_to_end_of_line();
        assert_eq!(buffer.text(), "otwo");
    }

    #[test]
    fn indenting_a_selection_shifts_every_line_it_covers() {
        let mut buffer = buffer("one\ntwo\nthree");
        buffer.set_cursor(Cursor::new(0, 0), false);
        buffer.set_cursor(Cursor::new(1, 1), true);
        buffer.insert_indent();
        assert_eq!(buffer.text(), "    one\n    two\nthree");
    }

    #[test]
    fn dedenting_undoes_indenting() {
        let mut buffer = buffer("one\ntwo");
        buffer.select_all();
        buffer.insert_indent();
        buffer.select_all();
        buffer.dedent();
        assert_eq!(buffer.text(), "one\ntwo");
    }

    #[test]
    fn dedenting_an_unindented_block_changes_nothing() {
        let mut buffer = buffer("one\ntwo");
        buffer.select_all();
        assert!(buffer.dedent().is_none());
        assert_eq!(buffer.text(), "one\ntwo");
    }

    #[test]
    fn indenting_leaves_blank_lines_blank() {
        let mut buffer = buffer("one\n\ntwo");
        buffer.select_all();
        buffer.insert_indent();
        assert_eq!(buffer.text(), "    one\n\n    two");
    }

    #[test]
    fn a_tab_indented_file_keeps_indenting_with_tabs() {
        let buffer = buffer("\tindented");
        assert_eq!(buffer.indent, Indent::Tab, "{NO_CONVERT}");

        let mut buffer = buffer;
        buffer.move_home(false);
        buffer.insert_indent();
        assert!(buffer.text().starts_with("\t\t"), "{NO_CONVERT}");
    }

    #[test]
    fn goto_line_clamps_to_the_last_line() {
        let mut buffer = buffer("one\ntwo");
        buffer.goto_line(999);
        assert_eq!(buffer.cursor(), Cursor::new(1, 0));
        buffer.goto_line(0);
        assert_eq!(buffer.cursor(), Cursor::new(0, 0));
    }

    #[test]
    fn typing_over_a_selection_replaces_it() {
        let mut buffer = buffer("hello world");
        buffer.set_cursor(Cursor::new(0, 0), false);
        buffer.set_cursor(Cursor::new(0, 5), true);
        buffer.insert("goodbye");
        assert_eq!(buffer.text(), "goodbye world");
        assert!(!buffer.has_selection());
    }
}
