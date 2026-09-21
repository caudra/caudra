//! Open files, as tabs.

pub mod buffer;
pub mod find;
pub mod highlight;
pub mod history;
pub mod render;
pub mod words;

use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tracing::info;

use buffer::Buffer;
use caudra_highlight::StyledSegment;
use find::Find;
use highlight::ViewportHighlighter;
use history::History;

use crate::fs::backend::{LoadedFile, ResourceEntry, WorkbenchPath};
use crate::fs::read::{self, LineEnding, LoadError, ReadOnly, SaveError, Source};
use crate::scm::diff::DiffRow;

/// What a line is, in a diff tab. A source tab has none of these and is
/// syntax-highlighted from its own buffer instead. Source control is what
/// produces them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DiffKind {
    #[default]
    Context,
    Added,
    Removed,
    /// Matched on both sides, but its whitespace moved. Neither added nor
    /// removed, so it is tinted like the side it now reads as and marked apart
    /// from both.
    Reindented,
    Header,
}

pub struct Tab {
    pub path: WorkbenchPath,
    pub resource: Option<ResourceEntry>,
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
    diff_rows: Option<Vec<DiffRow>>,
    modified: Option<SystemTime>,
    source: Option<Source>,
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
            path: WorkbenchPath::Local(path.to_path_buf()),
            resource: None,
            buffer: Buffer::new(loaded.lines),
            find: Find::default(),
            history: History::default(),
            highlighter: ViewportHighlighter::new(&path.to_string_lossy(), theme_generation),
            line_ending: loaded.line_ending,
            trailing_newline: loaded.trailing_newline,
            notice: loaded.read_only,
            diff_rows: None,
            modified: loaded.modified,
            source: None,
            conflict: false,
            preview: false,
            revision: 0,
            scroll: 0,
            scroll_row: 0,
            h_scroll: 0,
        }
    }

    pub(crate) fn from_local_source(
        path: &Path,
        loaded: read::Loaded,
        source: Source,
        theme_generation: u64,
    ) -> Self {
        let mut tab = Self::from_load(path, loaded, theme_generation);
        tab.source = Some(source);
        tab
    }

    /// The one place a file is read into a tab, and so the one place the cost
    /// of reading it is worth recording: opening and previewing both land here,
    /// and both skip it entirely when the path already has a tab.
    pub fn open(path: &Path, theme_generation: u64) -> Result<Self, LoadError> {
        let started = Instant::now();
        let loaded = read::load(path)?;
        info!(
            lines = loaded.lines.len(),
            read_ms = started.elapsed().as_millis() as u64,
            "workbench file read"
        );
        Ok(Self::from_load(path, loaded, theme_generation))
    }

    pub fn from_backend(loaded: LoadedFile, theme_generation: u64) -> Self {
        let path = loaded.entry.path.clone();
        let display = path.display();
        Self {
            title: path.file_name(),
            path,
            resource: Some(loaded.entry),
            buffer: Buffer::new(loaded.lines),
            find: Find::default(),
            history: History::default(),
            highlighter: ViewportHighlighter::new(&display, theme_generation),
            line_ending: loaded.line_ending,
            trailing_newline: loaded.trailing_newline,
            notice: None,
            diff_rows: None,
            modified: None,
            source: None,
            conflict: false,
            preview: false,
            revision: 0,
            scroll: 0,
            scroll_row: 0,
            h_scroll: 0,
        }
    }

    /// A tab that shows text the workbench produced rather than a file it read,
    /// which is how a diff gets scrolling, tabs and focus for free.
    pub fn synthetic(
        path: &Path,
        title: String,
        rows: Vec<DiffRow>,
        theme_generation: u64,
    ) -> Self {
        Self::synthetic_backend(
            WorkbenchPath::Local(path.to_path_buf()),
            title,
            rows,
            theme_generation,
        )
    }

    pub fn synthetic_backend(
        path: WorkbenchPath,
        title: String,
        rows: Vec<DiffRow>,
        theme_generation: u64,
    ) -> Self {
        let display = path.display();
        Self {
            title,
            path,
            resource: None,
            buffer: Buffer::new(rows.iter().map(|row| row.text.clone()).collect()),
            find: Find::default(),
            history: History::default(),
            highlighter: ViewportHighlighter::new(&display, theme_generation),
            line_ending: LineEnding::default(),
            trailing_newline: true,
            notice: None,
            diff_rows: Some(rows),
            modified: None,
            source: None,
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
    pub fn rename(
        &mut self,
        path: WorkbenchPath,
        resource: Option<ResourceEntry>,
        theme_generation: u64,
    ) {
        self.title = path.file_name();
        self.highlighter = ViewportHighlighter::new(&path.display(), theme_generation);
        self.path = path;
        if resource.is_some() {
            self.resource = resource;
        }
    }

    pub fn is_editable(&self) -> bool {
        self.notice.is_none() && self.diff_rows.is_none()
    }

    pub fn notice(&self) -> Option<ReadOnly> {
        self.notice
    }

    pub fn diff_rows(&self) -> Option<&[DiffRow]> {
        self.diff_rows.as_deref()
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
        if self.diff_rows.is_none() {
            self.highlighter.fill(self.buffer.lines(), first, last);
        }
    }

    /// The colours for one buffer line, borrowed rather than copied out so a
    /// frame costs no allocation.
    ///
    /// A diff tab carries its own, worked out per side when the tab was built,
    /// because its rows are not its file's rows and a single highlighter run
    /// over the rendered column would read a removal and its replacement as
    /// consecutive code.
    pub fn colours(&self, line: usize, first: usize, last: usize) -> Option<&[StyledSegment]> {
        match &self.diff_rows {
            Some(rows) => rows.get(line).map(|row| row.segments.as_slice()),
            None => self
                .highlighter
                .cached(first, last)
                .get(line - first)
                .map(Vec::as_slice),
        }
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
        let Some(path) = self.path.local() else {
            return Err(SaveError::ReadOnly(PathBuf::from(self.path.display())));
        };
        if !self.is_editable() {
            return Err(SaveError::ReadOnly(path.to_path_buf()));
        }
        let contents = read::encode(self.buffer.lines(), self.line_ending, self.trailing_newline);
        let saved = match &mut self.source {
            Some(source) => read::save_local_source(source, path, &contents),
            None => read::save(path, &contents, self.modified),
        };
        if matches!(
            &saved,
            Err(SaveError::Stale(_) | SaveError::Unconfirmed { .. })
        ) {
            self.conflict = true;
        }
        self.modified = saved?;
        self.history.mark_saved();
        self.conflict = false;
        Ok(())
    }

    /// Takes the file's new contents after something else wrote it, which is
    /// what the watcher asks for. A clean buffer reloads in place and keeps its
    /// cursor; a dirty one only raises the conflict, because discarding unsaved
    /// work is the user's call.
    pub fn reload_from_disk(&mut self) -> Result<bool, LoadError> {
        self.reload(false)
    }

    fn reload(&mut self, discard: bool) -> Result<bool, LoadError> {
        if self.is_dirty() && !discard {
            self.conflict = true;
            return Ok(false);
        }
        let Some(path) = self.path.local() else {
            return Ok(false);
        };
        let loaded = match &self.source {
            Some(source) => read::reload_local_source(source, path),
            None => read::load(path),
        };
        let loaded = match loaded {
            Ok(loaded) => loaded,
            Err(error) => {
                self.conflict = true;
                return Err(error);
            }
        };
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
        self.reload(true).map(|_| ())
    }

    pub fn contents(&self) -> String {
        read::encode(self.buffer.lines(), self.line_ending, self.trailing_newline)
    }

    pub fn apply_saved(&mut self, entry: ResourceEntry) {
        self.path = entry.path.clone();
        self.resource = Some(entry);
        self.history.mark_saved();
        self.conflict = false;
    }

    pub fn apply_backend_reload(&mut self, loaded: LoadedFile) -> bool {
        if self.is_dirty() {
            self.conflict = true;
            return false;
        }
        let cursor = self.buffer.cursor();
        let scroll = self.scroll;
        self.path = loaded.entry.path.clone();
        self.resource = Some(loaded.entry);
        self.line_ending = loaded.line_ending;
        self.trailing_newline = loaded.trailing_newline;
        self.buffer = Buffer::new(loaded.lines);
        self.buffer.set_cursor(cursor, false);
        self.scroll = scroll.min(self.buffer.line_count().saturating_sub(1));
        self.scroll_row = 0;
        self.history = History::default();
        self.highlighter.invalidate_from(0);
        self.conflict = false;
        true
    }

    pub fn discard_backend(&mut self, loaded: LoadedFile) {
        self.history = History::default();
        self.apply_backend_reload(loaded);
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
        let identity = WorkbenchPath::Local(path.to_path_buf());
        if let Some(index) = self.tabs.iter().position(|tab| tab.path == identity) {
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
        let identity = WorkbenchPath::Local(path.to_path_buf());
        if let Some(index) = self.tabs.iter().position(|tab| tab.path == identity) {
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

    pub fn push_backend(&mut self, mut tab: Tab, preview: bool) {
        if let Some(index) = self.tabs.iter().position(|open| open.path == tab.path) {
            self.tabs[index].preview = false;
            self.active = index;
            return;
        }
        if preview
            && let Some(index) = self
                .tabs
                .iter()
                .position(|open| open.preview && !open.is_dirty())
        {
            tab.preview = true;
            self.tabs[index] = tab;
            self.active = index;
            return;
        }
        tab.preview = preview;
        self.push(tab);
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
        self.rename_resource(
            &WorkbenchPath::Local(from.to_path_buf()),
            &WorkbenchPath::Local(to.to_path_buf()),
            None,
            theme_generation,
        );
    }

    pub fn rename_resource(
        &mut self,
        from: &WorkbenchPath,
        to: &WorkbenchPath,
        resource: Option<ResourceEntry>,
        theme_generation: u64,
    ) {
        for tab in &mut self.tabs {
            if !tab.path.starts_with(from) {
                continue;
            };
            let moved = if &tab.path == from {
                to.clone()
            } else {
                let relative = tab.path.display_relative(from);
                match to.join(&relative) {
                    Ok(path) => path,
                    Err(_) => continue,
                }
            };
            let moved_resource = (&tab.path == from).then(|| resource.clone()).flatten();
            tab.rename(moved, moved_resource, theme_generation);
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
    use super::{DiffKind, DiffRow, Editor, Tab};
    use std::fs;
    use tempfile::TempDir;

    const RAISED: &str = "opening an open file must raise its tab, not open a second one";
    const CLEAN_START: &str = "a freshly opened file must not claim to be dirty";
    const DIRTY_AFTER_EDIT: &str = "an edited buffer must claim to be dirty until it is saved";
    const NO_CLOBBER: &str = "a dirty buffer must not be replaced by what is on disk";
    const NO_ACTIVE: &str = "the tab just pushed must be the active one";
    const DIFF_READ_ONLY: &str = "a diff is not a file and must never be written back";
    const BUFFER_IS_THE_LINE: &str = "a diff row's buffer text is the line, not the patch line";

    #[cfg(unix)]
    mod local_source {
        use std::fs::{self, File};
        use std::os::unix::fs::symlink;
        use std::path::PathBuf;

        use tempfile::TempDir;
        use test_case::test_case;

        use super::Tab;
        use crate::fs::read::{LoadError, SaveError, load_local_source};

        const FILE: &str = "policy.lua";
        const ORIGINAL: &str = "allow = false\n";
        const EXTERNAL: &str = "allow = true \n";
        const EDIT: &str = "local ";
        const SENTINEL: &str = "unrelated file\n";
        const PARENT: &str = "config";
        const OUTSIDE: &str = "outside";
        const MOVED: &str = "moved";
        const DIRTY: &str =
            "a failed source save or reload must preserve the dirty draft and undo history";

        enum Swap {
            SourceSymlink,
            SourceReplacement,
            AncestorSymlink,
            AncestorAlias,
            AncestorReplacement,
        }

        fn fixture() -> (TempDir, PathBuf, Tab) {
            let dir = TempDir::new().unwrap();
            let parent = dir.path().canonicalize().unwrap().join(PARENT);
            fs::create_dir(&parent).unwrap();
            let path = parent.join(FILE);
            fs::write(&path, ORIGINAL).unwrap();
            let (loaded, source) = load_local_source(&path, |bytes| {
                assert_eq!(bytes, ORIGINAL.as_bytes());
                Ok(())
            })
            .unwrap();
            let tab = Tab::from_local_source(&path, loaded, source, 0);
            (dir, path, tab)
        }

        fn edit(tab: &mut Tab) -> String {
            let change = tab.buffer.insert(EDIT);
            assert!(tab.record(change));
            tab.contents()
        }

        #[test_case(false ; "ordinary_source")]
        #[test_case(true ; "sibling_symlink")]
        fn saves_use_exclusive_temporaries_and_advance_the_source_guard(attack: bool) {
            let (dir, path, mut tab) = fixture();
            let sentinel = dir.path().join(OUTSIDE);
            fs::write(&sentinel, SENTINEL).unwrap();
            let old_temporary = path.with_file_name(format!(".{FILE}.caudra-tmp"));
            if attack {
                symlink(&sentinel, &old_temporary).unwrap();
            }
            for _ in 0..2 {
                let edited = edit(&mut tab);
                tab.save().unwrap();
                assert!(!tab.is_dirty());
                assert_eq!(fs::read_to_string(&path).unwrap(), edited);
                assert!(
                    !fs::symlink_metadata(&path)
                        .unwrap()
                        .file_type()
                        .is_symlink()
                );
                assert_eq!(fs::read_to_string(&sentinel).unwrap(), SENTINEL);
                assert!(tab.reload_from_disk().unwrap());
                assert_eq!(tab.contents(), edited);
            }
            if attack {
                assert!(
                    fs::symlink_metadata(old_temporary)
                        .unwrap()
                        .file_type()
                        .is_symlink()
                );
            }
        }

        #[test_case(Swap::SourceSymlink ; "source_symlink")]
        #[test_case(Swap::SourceReplacement ; "source_replacement")]
        #[test_case(Swap::AncestorSymlink ; "ancestor_symlink")]
        #[test_case(Swap::AncestorAlias ; "ancestor_symlink_to_retained_directory")]
        #[test_case(Swap::AncestorReplacement ; "ancestor_replacement")]
        fn swapped_source_identity_refuses_save_and_discard_without_losing_work(swap: Swap) {
            let (dir, path, mut tab) = fixture();
            let draft = edit(&mut tab);
            let outside = dir.path().join(OUTSIDE);
            let moved = dir.path().join(MOVED);
            fs::create_dir(&outside).unwrap();
            fs::write(outside.join(FILE), ORIGINAL).unwrap();
            let original = match swap {
                Swap::SourceSymlink | Swap::SourceReplacement => {
                    fs::rename(&path, &moved).unwrap();
                    if matches!(swap, Swap::SourceSymlink) {
                        symlink(outside.join(FILE), &path).unwrap();
                    } else {
                        fs::write(&path, ORIGINAL).unwrap();
                    }
                    moved
                }
                Swap::AncestorSymlink | Swap::AncestorAlias | Swap::AncestorReplacement => {
                    let parent = path.parent().unwrap();
                    fs::rename(parent, &moved).unwrap();
                    if matches!(swap, Swap::AncestorAlias) {
                        symlink(&moved, parent).unwrap();
                    } else if matches!(swap, Swap::AncestorSymlink) {
                        symlink(&outside, parent).unwrap();
                    } else {
                        fs::create_dir(parent).unwrap();
                        fs::write(&path, ORIGINAL).unwrap();
                    }
                    moved.join(FILE)
                }
            };
            assert!(matches!(tab.save(), Err(SaveError::Stale(_))));
            assert!(tab.is_dirty(), "{DIRTY}");
            assert_eq!(tab.contents(), draft, "{DIRTY}");
            assert!(matches!(
                tab.discard_and_reload(),
                Err(LoadError::StaleSource(_))
            ));
            assert!(tab.is_dirty(), "{DIRTY}");
            assert_eq!(tab.contents(), draft, "{DIRTY}");
            assert_eq!(fs::read_to_string(original).unwrap(), ORIGINAL);
            assert_eq!(fs::read_to_string(outside.join(FILE)).unwrap(), ORIGINAL);
            assert!(tab.undo(), "{DIRTY}");
            assert_eq!(tab.contents(), ORIGINAL, "{DIRTY}");
        }

        #[test]
        fn source_save_detects_content_changes_even_with_restored_mtime() {
            let (_dir, path, mut tab) = fixture();
            let stamp = fs::metadata(&path).unwrap().modified().unwrap();
            let draft = edit(&mut tab);
            fs::write(&path, EXTERNAL).unwrap();
            File::open(&path).unwrap().set_modified(stamp).unwrap();
            assert!(matches!(tab.save(), Err(SaveError::Stale(_))));
            assert_eq!(fs::read_to_string(&path).unwrap(), EXTERNAL);
            assert_eq!(tab.contents(), draft, "{DIRTY}");
            assert!(tab.is_dirty(), "{DIRTY}");
        }

        #[test]
        fn a_clean_source_tab_does_not_adopt_unverified_disk_changes() {
            let (_dir, path, mut tab) = fixture();
            fs::write(&path, EXTERNAL).unwrap();
            assert!(matches!(
                tab.reload_from_disk(),
                Err(LoadError::StaleSource(_))
            ));
            assert_eq!(tab.contents(), ORIGINAL);
            assert!(tab.conflict);
        }
    }

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

    fn diff_row(text: &str, kind: DiffKind) -> DiffRow {
        DiffRow {
            text: text.to_owned(),
            kind,
            ..DiffRow::default()
        }
    }

    #[test]
    fn a_synthetic_tab_carries_its_diff_rows_and_refuses_to_be_edited() {
        let (_tmp, path) = fixture();
        let mut editor = Editor::default();
        let rows = vec![
            diff_row("@@ -1 +1 @@", DiffKind::Header),
            diff_row("same", DiffKind::Context),
            diff_row("old", DiffKind::Removed),
            diff_row("new", DiffKind::Added),
        ];
        editor.push(Tab::synthetic(&path, "main.rs (diff)".to_owned(), rows, 0));

        let tab = editor.active().expect(NO_ACTIVE);
        assert!(!tab.is_editable(), "{DIFF_READ_ONLY}");
        assert_eq!(tab.diff_rows().map(<[DiffRow]>::len), Some(4));
        assert_eq!(tab.buffer.line(2), "old", "{BUFFER_IS_THE_LINE}");
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
            vec![diff_row("new", DiffKind::Added)],
            0,
        ));

        assert_eq!(editor.tabs().len(), 1, "{RAISED}");
        assert_eq!(editor.active().expect(NO_ACTIVE).title, "main.rs (diff)");
    }
}
