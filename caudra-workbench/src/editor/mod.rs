//! Open files, as tabs.

pub mod buffer;
pub mod find;
pub mod highlight;
pub mod history;
pub mod render;

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use buffer::Buffer;
use caudra_highlight::StyledSegment;
use find::Find;
use highlight::ViewportHighlighter;
use history::History;

use crate::fs::read::{self, LineEnding, LoadError, ReadOnly, SaveError};

/// What a line is, in a diff tab. A source tab has none of these and is
/// syntax-highlighted instead. Source control is what produces them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    Context,
    Added,
    Removed,
    Header,
}

pub struct Tab {
    pub path: PathBuf,
    pub title: String,
    pub buffer: Buffer,
    pub find: Find,
    history: History,
    highlighter: ViewportHighlighter,
    line_ending: LineEnding,
    trailing_newline: bool,
    /// Set when the file could not be opened as text. The tab shows the reason
    /// rather than a buffer of replacement characters.
    notice: Option<ReadOnly>,
    diff_kinds: Option<Vec<DiffKind>>,
    modified: Option<SystemTime>,
    /// The file changed underneath an edited buffer. Neither copy can be thrown
    /// away without being asked, so the tab says so and waits.
    pub conflict: bool,
    /// A tab one click put up, which the next one takes over. Asking for the
    /// file again or typing in it pins the tab for good.
    pub preview: bool,
    revision: u64,
    scroll: usize,
    /// Which visual row of `scroll` sits at the top of the pane. Always zero
    /// while the pane is unwrapped, where a line is exactly one row.
    scroll_row: usize,
    h_scroll: usize,
}

/// One painted row: a slice of a buffer line, in display columns. Unwrapped is
/// the one-row case, so both modes reach [`render::Row::paint`] the same way.
#[derive(Clone, Copy)]
pub struct VisualRow {
    pub line: usize,
    pub start: usize,
    pub span: usize,
    /// Where the row falls within its buffer line. Only row zero is numbered.
    pub index: usize,
}

impl Tab {
    fn from_load(path: &Path, loaded: read::Loaded, theme_generation: u64) -> Self {
        Self {
            title: title_of(path),
            path: path.to_path_buf(),
            buffer: Buffer::new(loaded.lines),
            find: Find::default(),
            history: History::default(),
            highlighter: ViewportHighlighter::new(&path.to_string_lossy(), theme_generation),
            line_ending: loaded.line_ending,
            trailing_newline: loaded.trailing_newline,
            notice: loaded.read_only,
            diff_kinds: None,
            modified: loaded.modified,
            conflict: false,
            preview: false,
            revision: 0,
            scroll: 0,
            scroll_row: 0,
            h_scroll: 0,
        }
    }

    pub fn open(path: &Path, theme_generation: u64) -> Result<Self, LoadError> {
        Ok(Self::from_load(path, read::load(path)?, theme_generation))
    }

    /// A tab that shows text the workbench produced rather than a file it read,
    /// which is how a diff gets scrolling, tabs and focus for free.
    pub fn synthetic(
        path: &Path,
        title: String,
        lines: Vec<String>,
        kinds: Vec<DiffKind>,
        theme_generation: u64,
    ) -> Self {
        Self {
            title,
            path: path.to_path_buf(),
            buffer: Buffer::new(lines),
            find: Find::default(),
            history: History::default(),
            highlighter: ViewportHighlighter::new(&path.to_string_lossy(), theme_generation),
            line_ending: LineEnding::default(),
            trailing_newline: true,
            notice: None,
            diff_kinds: Some(kinds),
            modified: None,
            conflict: false,
            preview: false,
            revision: 0,
            scroll: 0,
            scroll_row: 0,
            h_scroll: 0,
        }
    }

    /// Points the tab at where its file went. The language is read from the
    /// name, so a rename that changes the extension changes the highlighting
    /// with it.
    pub fn rename(&mut self, path: &Path, theme_generation: u64) {
        self.title = title_of(path);
        self.highlighter = ViewportHighlighter::new(&path.to_string_lossy(), theme_generation);
        self.path = path.to_path_buf();
    }

    pub fn is_editable(&self) -> bool {
        self.notice.is_none() && self.diff_kinds.is_none()
    }

    pub fn notice(&self) -> Option<ReadOnly> {
        self.notice
    }

    pub fn diff_kinds(&self) -> Option<&[DiffKind]> {
        self.diff_kinds.as_deref()
    }

    pub fn is_dirty(&self) -> bool {
        !self.history.is_saved()
    }

    pub fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    /// Only the wrap-aware walks read the pan now, so nothing outside the tests
    /// asks for it on its own.
    #[cfg(test)]
    pub fn h_scroll(&self) -> usize {
        self.h_scroll
    }

    /// Lands the pane on a buffer line. The bar counts in buffer lines even
    /// when wrapping makes a row a slice of one, so a drag arrives here in the
    /// same unit the thumb was painted from.
    pub fn set_scroll(&mut self, line: usize) {
        self.scroll = line.min(self.buffer.line_count().saturating_sub(1));
        self.scroll_row = 0;
    }

    /// Works out the syntax colours for the rows about to be drawn. A diff tab
    /// is coloured by its [`DiffKind`]s instead, so it asks for none.
    pub fn highlight(&mut self, first: usize, last: usize) {
        if self.diff_kinds.is_none() {
            self.highlighter.fill(self.buffer.lines(), first, last);
        }
    }

    /// The colours [`Self::highlight`] worked out, borrowed rather than copied
    /// out so a frame costs no allocation.
    pub fn segments(&self, first: usize, last: usize) -> &[Vec<StyledSegment>] {
        self.highlighter.cached(first, last)
    }

    /// Counts changes to the text, so a caller can tell a motion from an edit
    /// without diffing the buffer.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn record(&mut self, edit: Option<buffer::Edit>) -> bool {
        let Some(edit) = edit else {
            return false;
        };
        self.highlighter.invalidate_from(edit.at.line);
        self.history.record(edit);
        self.revision += 1;
        true
    }

    pub fn undo(&mut self) -> bool {
        let Some(edit) = self.history.undo() else {
            return false;
        };
        self.highlighter.invalidate_from(edit.at.line);
        self.buffer.replay(&edit);
        self.revision += 1;
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(edit) = self.history.redo() else {
            return false;
        };
        self.highlighter.invalidate_from(edit.at.line);
        self.buffer.replay(&edit);
        self.revision += 1;
        true
    }

    pub fn set_find_query(&mut self, query: String) {
        let cursor = self.buffer.cursor();
        self.find.set_query(query, self.buffer.lines(), cursor);
    }

    pub fn refresh_find(&mut self) {
        self.find.refresh(self.buffer.lines());
    }

    /// Motions and text edits, the keys that touch nothing but this buffer.
    /// Reports whether the key was one of them.
    pub fn edit_key(&mut self, key: KeyEvent, rows: usize) -> bool {
        let extend = key.modifiers.contains(KeyModifiers::SHIFT);
        let by_word = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = rows.max(1) as isize;
        match key.code {
            KeyCode::Left if by_word => self.buffer.move_word_left(extend),
            KeyCode::Left => self.buffer.move_left(extend),
            KeyCode::Right if by_word => self.buffer.move_word_right(extend),
            KeyCode::Right => self.buffer.move_right(extend),
            KeyCode::Up => self.buffer.move_vertical(-1, extend),
            KeyCode::Down => self.buffer.move_vertical(1, extend),
            KeyCode::PageUp => self.buffer.move_vertical(-page, extend),
            KeyCode::PageDown => self.buffer.move_vertical(page, extend),
            KeyCode::Home if by_word => self.buffer.move_document_start(extend),
            KeyCode::Home => self.buffer.move_home(extend),
            KeyCode::End if by_word => self.buffer.move_document_end(extend),
            KeyCode::End => self.buffer.move_end(extend),
            _ => return self.text_key(key),
        }
        self.break_undo_group();
        true
    }

    fn text_key(&mut self, key: KeyEvent) -> bool {
        if !self.is_editable() {
            return false;
        }
        let by_word = key.modifiers.contains(KeyModifiers::CONTROL);
        let typing = (key.modifiers - KeyModifiers::SHIFT).is_empty();
        let edit = match key.code {
            KeyCode::Char(ch) if typing => self.buffer.insert(&ch.to_string()),
            KeyCode::Enter if typing => self.buffer.insert_newline(),
            KeyCode::Tab => self.buffer.insert_indent(),
            KeyCode::BackTab => self.buffer.dedent(),
            KeyCode::Backspace if by_word => self.buffer.delete_word_left(),
            KeyCode::Backspace => self.buffer.backspace(),
            KeyCode::Delete if by_word => self.buffer.delete_word_right(),
            KeyCode::Delete => self.buffer.delete(),
            _ => return false,
        };
        self.record(edit);
        true
    }

    pub fn save(&mut self) -> Result<(), SaveError> {
        if !self.is_editable() {
            return Err(SaveError::ReadOnly(self.path.clone()));
        }
        let contents = read::encode(self.buffer.lines(), self.line_ending, self.trailing_newline);
        self.modified = read::save(&self.path, &contents, self.modified)?;
        self.history.mark_saved();
        self.conflict = false;
        Ok(())
    }

    /// Takes the file's new contents after something else wrote it, which is
    /// what the watcher asks for. A clean buffer reloads in place and keeps its
    /// cursor; a dirty one only raises the conflict, because discarding unsaved
    /// work is the user's call.
    pub fn reload_from_disk(&mut self) -> Result<bool, LoadError> {
        if self.is_dirty() {
            self.conflict = true;
            return Ok(false);
        }
        let loaded = read::load(&self.path)?;
        let cursor = self.buffer.cursor();
        let scroll = self.scroll;
        self.line_ending = loaded.line_ending;
        self.trailing_newline = loaded.trailing_newline;
        self.notice = loaded.read_only;
        self.modified = loaded.modified;
        self.buffer = Buffer::new(loaded.lines);
        self.buffer.set_cursor(cursor, false);
        self.scroll = scroll.min(self.buffer.line_count().saturating_sub(1));
        self.scroll_row = 0;
        self.history = History::default();
        self.highlighter.invalidate_from(0);
        self.conflict = false;
        Ok(true)
    }

    /// Throws the buffer away and takes what is on disk, which is the only way
    /// out of a conflict that keeps the other writer's work.
    pub fn discard_and_reload(&mut self) -> Result<(), LoadError> {
        self.history = History::default();
        self.reload_from_disk().map(|_| ())
    }

    pub fn set_theme_generation(&mut self, generation: u64) {
        self.highlighter.set_theme_generation(generation);
    }

    pub fn break_undo_group(&mut self) {
        self.history.break_group();
    }

    /// The display column each visual row of `line` starts at. Unwrapped there
    /// is one row, panned to wherever the window sits, which is what lets every
    /// caller below walk both modes with the same arithmetic.
    fn row_starts(&self, line: usize, columns: usize, wrap: bool) -> Vec<usize> {
        match wrap {
            true => render::wrap_columns(self.buffer.line(line), columns),
            false => vec![self.h_scroll],
        }
    }

    /// Where the pane starts painting. `scroll_row` is clamped rather than
    /// trusted, so a row remembered under a wrap that has since been turned off,
    /// or an edit that shortened the line, cannot skip past the top line.
    fn scroll_top(&self, columns: usize, wrap: bool) -> (usize, usize) {
        let count = self.row_starts(self.scroll, columns, wrap).len();
        (self.scroll, self.scroll_row.min(count - 1))
    }

    /// The rows the pane paints, top first and at most `rows` of them.
    pub fn visible_rows(&self, rows: usize, columns: usize, wrap: bool) -> Vec<VisualRow> {
        let (mut line, mut index) = self.scroll_top(columns, wrap);
        let mut visible = Vec::with_capacity(rows);
        while visible.len() < rows && line < self.buffer.line_count() {
            let starts = self.row_starts(line, columns, wrap);
            while index < starts.len() && visible.len() < rows {
                let start = starts[index];
                let span = starts
                    .get(index + 1)
                    .map_or(columns, |next| (next - start).min(columns));
                visible.push(VisualRow {
                    line,
                    start,
                    span,
                    index,
                });
                index += 1;
            }
            line += 1;
            index = 0;
        }
        visible
    }

    /// The top of a pane `count` rows tall whose last row is `(line, row)`.
    fn top_for(
        &self,
        line: usize,
        row: usize,
        count: usize,
        columns: usize,
        wrap: bool,
    ) -> (usize, usize) {
        let (mut line, mut row) = (line, row);
        for _ in 0..count.saturating_sub(1) {
            if row > 0 {
                row -= 1;
            } else if line > 0 {
                line -= 1;
                row = self.row_starts(line, columns, wrap).len() - 1;
            } else {
                break;
            }
        }
        (line, row)
    }

    /// Keeps the cursor inside the window vertically and horizontally.
    /// Horizontal scrolling is in display columns, so a wide glyph moves the
    /// window by two. A wrapped pane has nothing off to its side, so it pans
    /// back to the left margin and stays there.
    pub fn follow_cursor(&mut self, rows: usize, columns: usize, wrap: bool) {
        let cursor = self.buffer.cursor();
        let column = render::display_column(self.buffer.line(cursor.line), cursor.col);
        let row = self
            .row_starts(cursor.line, columns, wrap)
            .iter()
            .rposition(|start| column >= *start)
            .unwrap_or_default();

        let top = self.scroll_top(columns, wrap);
        if (cursor.line, row) < top {
            (self.scroll, self.scroll_row) = (cursor.line, row);
        } else {
            let bottom = self.top_for(cursor.line, row, rows.max(1), columns, wrap);
            if top < bottom {
                (self.scroll, self.scroll_row) = bottom;
            }
        }

        if wrap {
            self.h_scroll = 0;
        } else if column < self.h_scroll {
            self.h_scroll = column;
        } else if columns > 0 && column >= self.h_scroll + columns {
            self.h_scroll = column + 1 - columns;
        }
    }

    /// Steps the window `delta` visual rows, negative upwards, stopping where
    /// the last row of the buffer reaches the bottom of the pane.
    pub fn scroll_by(&mut self, delta: isize, rows: usize, columns: usize, wrap: bool) {
        let top = self.scroll_top(columns, wrap);
        let steps = delta.unsigned_abs() + 1;
        let moved = match delta < 0 {
            true => self.top_for(top.0, top.1, steps, columns, wrap),
            false => self
                .visible_rows(steps, columns, wrap)
                .last()
                .map_or(top, |row| (row.line, row.index)),
        };
        let last_line = self.buffer.line_count().saturating_sub(1);
        let last_row = self.row_starts(last_line, columns, wrap).len() - 1;
        let bottom = self.top_for(last_line, last_row, rows.max(1), columns, wrap);
        (self.scroll, self.scroll_row) = moved.min(bottom);
    }

    /// Pans the window sideways, in display columns. Clamped to the widest
    /// line in view rather than in the file, so the pane cannot be pushed
    /// into blank space beside the longest thing it is actually showing, and
    /// nothing has to measure a file to answer a wheel.
    pub fn h_scroll_by(&mut self, delta: isize, rows: usize, columns: usize) {
        let last = (self.scroll + rows).min(self.buffer.line_count());
        let widest = (self.scroll..last)
            .map(|line| {
                let text = self.buffer.line(line);
                render::display_column(text, text.chars().count())
            })
            .max()
            .unwrap_or(0);
        let max = widest.saturating_sub(columns);
        self.h_scroll = self.h_scroll.saturating_add_signed(delta).min(max);
    }
}

fn title_of(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

#[derive(Default)]
pub struct Editor {
    tabs: Vec<Tab>,
    active: usize,
}

impl Editor {
    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    pub fn tabs_mut(&mut self) -> &mut [Tab] {
        &mut self.tabs
    }

    pub fn active_index(&self) -> usize {
        self.active
    }

    pub fn active(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }

    pub fn active_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active)
    }

    /// Opening a file already open raises its tab instead of duplicating it,
    /// which also protects the one buffer per path invariant the watcher and
    /// the save path both rely on.
    pub fn open(&mut self, path: &Path, theme_generation: u64) -> Result<(), LoadError> {
        if let Some(index) = self.tabs.iter().position(|tab| tab.path == path) {
            self.active = index;
            self.tabs[index].preview = false;
            return Ok(());
        }
        self.tabs.push(Tab::open(path, theme_generation)?);
        self.active = self.tabs.len() - 1;
        Ok(())
    }

    /// Opens `path` in the preview slot: the tab one click puts up, which the
    /// next one takes over rather than stacking beside it. A file already open
    /// is raised as it stands, so a tab that was pinned stays pinned.
    pub fn preview(&mut self, path: &Path, theme_generation: u64) -> Result<(), LoadError> {
        if let Some(index) = self.tabs.iter().position(|tab| tab.path == path) {
            self.active = index;
            return Ok(());
        }
        let mut tab = Tab::open(path, theme_generation)?;
        tab.preview = true;
        self.active = match self.tabs.iter().position(|tab| tab.preview) {
            Some(index) => {
                self.tabs[index] = tab;
                index
            }
            None => {
                self.tabs.push(tab);
                self.tabs.len() - 1
            }
        };
        Ok(())
    }

    /// Adds a tab the workbench built rather than read, replacing any tab
    /// already showing that path.
    pub fn push(&mut self, tab: Tab) {
        if let Some(index) = self.tabs.iter().position(|open| open.path == tab.path) {
            self.tabs[index] = tab;
            self.active = index;
            return;
        }
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
    }

    pub fn close_active(&mut self) -> Option<Tab> {
        if self.tabs.is_empty() {
            return None;
        }
        let tab = self.tabs.remove(self.active);
        self.active = self.active.min(self.tabs.len().saturating_sub(1));
        Some(tab)
    }

    /// Follows a path that moved, taking the tabs under a folder that moved
    /// along with it.
    pub fn rename(&mut self, from: &Path, to: &Path, theme_generation: u64) {
        for tab in &mut self.tabs {
            let Ok(rest) = tab.path.clone().strip_prefix(from).map(Path::to_path_buf) else {
                continue;
            };
            let moved = match rest.as_os_str().is_empty() {
                true => to.to_path_buf(),
                false => to.join(rest),
            };
            tab.rename(&moved, theme_generation);
        }
    }

    /// Closes every tab the test picks out. What was active stays active when
    /// it survived, so closing tabs elsewhere does not move the reader.
    pub fn close_where(&mut self, doomed: &dyn Fn(&Tab) -> bool) {
        let active = self.tabs.get(self.active).map(|tab| tab.path.clone());
        self.tabs.retain(|tab| !doomed(tab));
        self.active = active
            .and_then(|path| self.tabs.iter().position(|tab| tab.path == path))
            .unwrap_or_else(|| self.active.min(self.tabs.len().saturating_sub(1)));
    }

    pub fn select(&mut self, index: usize) {
        if index < self.tabs.len() {
            self.active = index;
        }
    }

    pub fn cycle(&mut self, delta: isize) {
        if self.tabs.is_empty() {
            return;
        }
        let count = self.tabs.len() as isize;
        self.active = (self.active as isize + delta).rem_euclid(count) as usize;
    }

    pub fn set_theme_generation(&mut self, generation: u64) {
        for tab in &mut self.tabs {
            tab.set_theme_generation(generation);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::buffer::Cursor;
    use super::{DiffKind, Editor, Tab};
    use std::fs;
    use tempfile::TempDir;

    const RAISED: &str = "opening an open file must raise its tab, not open a second one";
    const CLEAN_START: &str = "a freshly opened file must not claim to be dirty";
    const DIRTY_AFTER_EDIT: &str = "an edited buffer must claim to be dirty until it is saved";
    const NO_CLOBBER: &str = "a dirty buffer must not be replaced by what is on disk";
    const NO_ACTIVE: &str = "the tab just pushed must be the active one";
    const DIFF_READ_ONLY: &str = "a diff is not a file and must never be written back";

    fn fixture() -> (TempDir, std::path::PathBuf) {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("main.rs");
        fs::write(&path, "fn main() {}\n").unwrap();
        (tmp, path)
    }

    #[test]
    fn a_freshly_opened_file_is_clean() {
        let (_tmp, path) = fixture();
        let tab = Tab::open(&path, 0).unwrap();
        assert!(!tab.is_dirty(), "{CLEAN_START}");
        assert_eq!(tab.title, "main.rs");
        assert!(tab.is_editable());
    }

    #[test]
    fn editing_marks_the_tab_dirty_and_saving_cleans_it() {
        let (_tmp, path) = fixture();
        let mut tab = Tab::open(&path, 0).unwrap();

        let edit = tab.buffer.insert("// note\n");
        tab.record(edit);
        assert!(tab.is_dirty(), "{DIRTY_AFTER_EDIT}");

        tab.save().unwrap();
        assert!(!tab.is_dirty(), "{CLEAN_START}");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "// note\nfn main() {}\n"
        );
    }

    #[test]
    fn undoing_back_to_the_saved_text_cleans_the_tab() {
        let (_tmp, path) = fixture();
        let mut tab = Tab::open(&path, 0).unwrap();
        let edit = tab.buffer.insert("x");
        tab.record(edit);
        assert!(tab.is_dirty());

        tab.undo();
        assert!(!tab.is_dirty(), "{CLEAN_START}");
    }

    #[test]
    fn undo_and_redo_walk_the_buffer_back_and_forth() {
        let (_tmp, path) = fixture();
        let mut tab = Tab::open(&path, 0).unwrap();
        let before = tab.buffer.text();

        let edit = tab.buffer.insert("added");
        tab.record(edit);
        let after = tab.buffer.text();

        assert!(tab.undo());
        assert_eq!(tab.buffer.text(), before);
        assert!(tab.redo());
        assert_eq!(tab.buffer.text(), after);
    }

    #[test]
    fn a_clean_tab_reloads_when_the_file_changes_underneath_it() {
        let (_tmp, path) = fixture();
        let mut tab = Tab::open(&path, 0).unwrap();
        tab.buffer.set_cursor(Cursor::new(0, 3), false);

        fs::write(&path, "fn main() { rewritten(); }\n").unwrap();
        assert!(tab.reload_from_disk().unwrap());

        assert!(tab.buffer.text().contains("rewritten"));
        assert_eq!(tab.buffer.cursor(), Cursor::new(0, 3));
        assert!(!tab.conflict);
    }

    #[test]
    fn a_dirty_tab_raises_a_conflict_instead_of_reloading() {
        let (_tmp, path) = fixture();
        let mut tab = Tab::open(&path, 0).unwrap();
        let edit = tab.buffer.insert("mine");
        tab.record(edit);
        let mine = tab.buffer.text();

        fs::write(&path, "theirs\n").unwrap();
        assert!(!tab.reload_from_disk().unwrap());

        assert_eq!(tab.buffer.text(), mine, "{NO_CLOBBER}");
        assert!(tab.conflict);
    }

    #[test]
    fn discarding_a_conflict_takes_what_is_on_disk() {
        let (_tmp, path) = fixture();
        let mut tab = Tab::open(&path, 0).unwrap();
        let edit = tab.buffer.insert("mine");
        tab.record(edit);

        fs::write(&path, "theirs\n").unwrap();
        tab.discard_and_reload().unwrap();

        assert_eq!(tab.buffer.text(), "theirs");
        assert!(!tab.conflict);
        assert!(!tab.is_dirty());
    }

    #[test]
    fn saving_a_read_only_tab_is_refused() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("binary.bin");
        fs::write(&path, b"\0\0\0").unwrap();
        let mut tab = Tab::open(&path, 0).unwrap();

        assert!(!tab.is_editable());
        assert!(tab.save().is_err());
    }

    #[test]
    fn opening_the_same_file_twice_raises_the_first_tab() {
        let (_tmp, path) = fixture();
        let mut editor = Editor::default();
        editor.open(&path, 0).unwrap();
        editor.open(&path, 0).unwrap();
        assert_eq!(editor.tabs().len(), 1, "{RAISED}");
    }

    #[test]
    fn cycling_wraps_at_both_ends() {
        let tmp = TempDir::new().unwrap();
        let mut editor = Editor::default();
        for name in ["a.rs", "b.rs", "c.rs"] {
            let path = tmp.path().join(name);
            fs::write(&path, "\n").unwrap();
            editor.open(&path, 0).unwrap();
        }
        assert_eq!(editor.active_index(), 2);
        editor.cycle(1);
        assert_eq!(editor.active_index(), 0);
        editor.cycle(-1);
        assert_eq!(editor.active_index(), 2);
    }

    #[test]
    fn closing_the_last_tab_leaves_the_editor_empty() {
        let (_tmp, path) = fixture();
        let mut editor = Editor::default();
        editor.open(&path, 0).unwrap();
        assert!(editor.close_active().is_some());
        assert!(editor.tabs().is_empty());
        assert!(editor.close_active().is_none());
    }

    #[test]
    fn closing_a_middle_tab_keeps_the_selection_in_range() {
        let tmp = TempDir::new().unwrap();
        let mut editor = Editor::default();
        for name in ["a.rs", "b.rs"] {
            let path = tmp.path().join(name);
            fs::write(&path, "\n").unwrap();
            editor.open(&path, 0).unwrap();
        }
        editor.select(0);
        editor.close_active();
        assert_eq!(editor.active_index(), 0);
        assert_eq!(editor.tabs().len(), 1);
    }

    #[test]
    fn a_synthetic_tab_carries_its_diff_kinds_and_refuses_to_be_edited() {
        let (_tmp, path) = fixture();
        let mut editor = Editor::default();
        let tab = Tab::synthetic(
            &path,
            "main.rs (diff)".to_owned(),
            vec![
                "@@ -1 +1 @@".to_owned(),
                " same".to_owned(),
                "-old".to_owned(),
                "+new".to_owned(),
            ],
            vec![
                DiffKind::Header,
                DiffKind::Context,
                DiffKind::Removed,
                DiffKind::Added,
            ],
            0,
        );
        editor.push(tab);

        let tab = editor.active().expect(NO_ACTIVE);
        assert!(!tab.is_editable(), "{DIFF_READ_ONLY}");
        assert_eq!(tab.diff_kinds().map(<[DiffKind]>::len), Some(4));
        assert!(tab.notice().is_none(), "a diff is shown, not refused");

        assert!(
            editor.active_mut().expect(NO_ACTIVE).save().is_err(),
            "{DIFF_READ_ONLY}"
        );
    }

    /// A second diff of the same file replaces the first, so repeatedly asking
    /// for one does not pile up tabs that all say the same thing.
    #[test]
    fn pushing_a_tab_for_an_open_path_replaces_it() {
        let (_tmp, path) = fixture();
        let mut editor = Editor::default();
        editor.open(&path, 0).unwrap();
        editor.push(Tab::synthetic(
            &path,
            "main.rs (diff)".to_owned(),
            vec!["+new".to_owned()],
            vec![DiffKind::Added],
            0,
        ));

        assert_eq!(editor.tabs().len(), 1, "{RAISED}");
        assert_eq!(editor.active().expect(NO_ACTIVE).title, "main.rs (diff)");
    }
}
