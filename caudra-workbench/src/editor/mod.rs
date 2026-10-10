//! Open files, as tabs.

pub mod buffer;
pub mod find;
pub mod highlight;
pub mod history;
pub mod render;
pub mod rendered;
pub mod text_field;
mod words;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use crossterm::event::KeyEvent;
use tracing::info;

use buffer::Buffer;
use caudra_highlight::StyledSegment;
use find::Find;
use highlight::ViewportHighlighter;
use history::History;
use rendered::{PaintMarkdown, Painting, Rendered};
use text_field::{FieldKind, TextCommand, decode};

use crate::fs::backend::{LoadedFile, ResourceEntry, WorkbenchPath};
use crate::fs::read::{self, LineEnding, LoadError, ReadOnly, SaveError, Source};
use crate::scm::diff::DiffRow;

/// The extensions a tab is read as Markdown by, matched without regard to case.
const MARKDOWN_EXTENSIONS: [&str; 2] = ["md", "markdown"];

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

/// What the host calls a file it opened for a reason of its own, such as the
/// plan, in place of a file name and a path that say nothing about why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabLabel {
    pub title: String,
    pub status: String,
}

/// Names text the host keeps for itself, such as a prompt draft, so the host
/// can find its tab again and knows what a save hands back.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DocumentKey(pub String);

pub struct Tab {
    pub path: WorkbenchPath,
    pub resource: Option<ResourceEntry>,
    pub title: String,
    pub label: Option<TabLabel>,
    /// Set on a tab over text the host keeps rather than a file. Saving hands
    /// the text back to the host, and nothing on disk is read into it.
    pub document: Option<DocumentKey>,
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
    snapshot: Option<String>,
    source: Option<Source>,
    /// The file changed underneath an edited buffer. Neither copy can be thrown
    /// away without being asked, so the tab says so and waits.
    pub conflict: bool,
    pub(crate) remote_reload: bool,
    /// A tab one click put up, which the next one takes over. Asking for the
    /// file again or typing in it pins the tab for good.
    pub preview: bool,
    /// Set while a Markdown tab shows its rendered view instead of its source.
    rendered: Option<Rendered>,
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
            label: None,
            document: None,
            buffer: Buffer::new(loaded.lines),
            find: Find::default(),
            history: History::default(),
            highlighter: ViewportHighlighter::new(&path.to_string_lossy(), theme_generation),
            line_ending: loaded.line_ending,
            trailing_newline: loaded.trailing_newline,
            notice: loaded.read_only,
            diff_rows: None,
            modified: loaded.modified,
            snapshot: None,
            source: None,
            conflict: false,
            remote_reload: false,
            preview: false,
            rendered: None,
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
            label: None,
            document: None,
            buffer: Buffer::new(loaded.lines),
            find: Find::default(),
            history: History::default(),
            highlighter: ViewportHighlighter::new(&display, theme_generation),
            line_ending: loaded.line_ending,
            trailing_newline: loaded.trailing_newline,
            notice: None,
            diff_rows: None,
            modified: None,
            snapshot: None,
            source: None,
            conflict: false,
            remote_reload: false,
            preview: false,
            rendered: None,
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
            label: None,
            document: None,
            buffer: Buffer::new(rows.iter().map(|row| row.text.clone()).collect()),
            find: Find::default(),
            history: History::default(),
            highlighter: ViewportHighlighter::new(&display, theme_generation),
            line_ending: LineEnding::default(),
            trailing_newline: true,
            notice: None,
            diff_rows: Some(rows),
            modified: None,
            snapshot: None,
            source: None,
            conflict: false,
            remote_reload: false,
            preview: false,
            rendered: None,
            revision: 0,
            scroll: 0,
            scroll_row: 0,
            h_scroll: 0,
        }
    }

    /// A tab over text the host keeps, filed under `path`: a name no file has
    /// that still says Markdown, so the text highlights and renders the way a
    /// note on disk would.
    pub fn document(
        path: &Path,
        key: DocumentKey,
        label: TabLabel,
        text: &str,
        theme_generation: u64,
    ) -> Self {
        let mut tab = Self::from_load(path, read::decode(text, None), theme_generation);
        tab.title = label.title.clone();
        tab.label = Some(label);
        tab.document = Some(key);
        tab
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
        if let Some(opened) = &mut self.resource {
            opened.path = path.clone();
            opened.resource_id = resource.and_then(|resource| resource.resource_id);
        }
        self.path = path;
        if !self.is_markdown() {
            self.rendered = None;
        }
    }

    pub fn is_editable(&self) -> bool {
        self.notice.is_none() && self.diff_rows.is_none()
    }

    /// Whether the tab stands for a file, which a diff and a host's document
    /// do not: neither has a path worth copying, revealing or reopening.
    pub fn is_file(&self) -> bool {
        self.diff_rows.is_none() && self.document.is_none()
    }

    /// Whether the tab holds Markdown text, which is what a rendered view can
    /// be offered for. A diff or a file that is not text has none to render.
    pub fn is_markdown(&self) -> bool {
        self.is_editable()
            && Path::new(&self.path.file_name())
                .extension()
                .and_then(OsStr::to_str)
                .is_some_and(|extension| {
                    MARKDOWN_EXTENSIONS
                        .iter()
                        .any(|markdown| extension.eq_ignore_ascii_case(markdown))
                })
    }

    pub fn is_rendered(&self) -> bool {
        self.rendered.is_some()
    }

    pub(crate) fn rendered_view(&self) -> Option<&Rendered> {
        self.rendered.as_ref()
    }

    pub(crate) fn rendered_mut(&mut self) -> Option<&mut Rendered> {
        self.rendered.as_mut()
    }

    pub fn selected_text(&self) -> Option<String> {
        match &self.rendered {
            Some(view) => view.selected_text(self.revision),
            None => self.buffer.selected_text(),
        }
    }

    pub fn clear_selection(&mut self) -> bool {
        match &mut self.rendered {
            Some(view) => view.clear_selection(),
            None => {
                let selected = self.buffer.has_selection();
                self.buffer.clear_selection();
                selected
            }
        }
    }

    /// Flips between the source and the rendered view, keeping the reader at
    /// about the same point through the document. Reports whether it could:
    /// a tab with no Markdown in it has nothing to render.
    ///
    /// A find bar left open over the source would act on text the reader
    /// cannot see. The source selection stays, since the rendered view keeps
    /// its own.
    pub fn toggle_rendered(&mut self) -> bool {
        if self.is_rendered() {
            self.show_source();
            return true;
        }
        if !self.is_markdown() {
            return false;
        }
        self.find.close();
        self.rendered = Some(Rendered::entered_at(self.scroll, self.buffer.line_count()));
        true
    }

    /// Leaves the rendered view for the source, scrolled to the same point
    /// through the document. The caret stays where it was, as it does under a
    /// wheel, so the next motion picks up from it.
    pub fn show_source(&mut self) {
        if let Some(view) = self.rendered.take() {
            self.set_scroll(view.source_line(self.buffer.line_count()));
        }
    }

    /// The rendered view brought up to date with the text, `width` and the
    /// theme, painting only when one of them moved. A tab showing its source
    /// has none.
    pub(crate) fn rendered(
        &mut self,
        width: u16,
        theme_generation: u64,
        paint: PaintMarkdown,
    ) -> Option<&Rendered> {
        let painting = Painting {
            revision: self.revision,
            width,
            theme_generation,
        };
        let view = self.rendered.as_mut()?;
        view.paint(
            painting,
            || read::encode(self.buffer.lines(), self.line_ending, self.trailing_newline),
            paint,
        );
        Some(view)
    }

    /// What the strip and the unsaved-changes question call the tab: the
    /// host's name for it when it gave one, the file name otherwise.
    pub fn heading(&self) -> &str {
        self.label
            .as_ref()
            .map_or(&self.title, |label| &label.title)
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
        self.changed();
        true
    }

    pub fn undo(&mut self) -> bool {
        let Some(edit) = self.history.undo() else {
            return false;
        };
        self.highlighter.invalidate_from(edit.at.line);
        self.buffer.replay(&edit);
        self.changed();
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(edit) = self.history.redo() else {
            return false;
        };
        self.highlighter.invalidate_from(edit.at.line);
        self.buffer.replay(&edit);
        self.changed();
        true
    }

    fn changed(&mut self) {
        self.revision += 1;
        if let Some(view) = &mut self.rendered {
            view.clear_selection();
        }
    }

    /// Runs the find query from the caret, after the query changed.
    pub fn search_find(&mut self) {
        let cursor = self.buffer.cursor();
        self.find.search(self.buffer.lines(), cursor);
    }

    pub fn refresh_find(&mut self) {
        self.find.refresh(self.buffer.lines());
    }

    /// Runs the document keymap against this buffer: motions, edits, select
    /// all, undo and redo. Reports whether the caret may have moved, which is
    /// what the pane follows. Selecting all leaves the reader where they are,
    /// and copy and cut belong to the workbench's clipboard.
    pub fn edit_key(&mut self, key: KeyEvent, rows: usize) -> bool {
        let Some(command) = decode(key, FieldKind::Document) else {
            return false;
        };
        match command {
            TextCommand::Move { motion, extend } => {
                self.buffer.move_by(motion, extend, rows);
                self.break_undo_group();
                true
            }
            TextCommand::Edit(command) if self.is_editable() => {
                let edit = self.buffer.perform(command);
                self.record(edit);
                true
            }
            TextCommand::SelectAll => {
                self.buffer.select_all();
                false
            }
            TextCommand::Undo => self.undo(),
            TextCommand::Redo => self.redo(),
            TextCommand::Edit(_) | TextCommand::Copy | TextCommand::Cut => false,
        }
    }

    pub fn save(&mut self) -> Result<(), SaveError> {
        let Some(path) = self.path.local() else {
            return Err(SaveError::ReadOnly(PathBuf::from(self.path.display())));
        };
        if !self.is_editable() {
            return Err(SaveError::ReadOnly(path.to_path_buf()));
        }
        let contents = read::encode(self.buffer.lines(), self.line_ending, self.trailing_newline);
        let saved = match (&self.snapshot, &mut self.source) {
            (Some(snapshot), source) => {
                read::save_snapshot(path, &contents, snapshot, source.as_mut())
            }
            (None, Some(source)) => read::save_local_source(source, path, &contents),
            (None, None) => read::save(path, &contents, self.modified),
        };
        match &saved {
            Err(SaveError::Stale(_)) => self.conflict = true,
            #[cfg(unix)]
            Err(SaveError::Unconfirmed { .. }) => self.conflict = true,
            _ => {}
        }
        self.modified = saved?;
        self.snapshot = None;
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

    /// Catches a write no watch was there to see, because the workbench was
    /// closed or the file lives outside the tree it watches. A file whose time
    /// has not moved costs one stat and keeps its undo history.
    pub fn refresh_if_changed(&mut self) -> Result<bool, LoadError> {
        let Some(path) = self.path.local() else {
            return Ok(false);
        };
        if !self.is_editable() || read::modified(path) == self.modified {
            return Ok(false);
        }
        self.reload(false)
    }

    fn reload(&mut self, discard: bool) -> Result<bool, LoadError> {
        // A document's text lives with the host, so nothing on disk is newer.
        if self.document.is_some() {
            return Ok(false);
        }
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
        match loaded {
            Ok(loaded) => {
                self.take(loaded);
                Ok(true)
            }
            Err(error) => {
                self.conflict = true;
                Err(error)
            }
        }
    }

    /// Hands a document the host's newer copy of its text, the way a reload
    /// hands a file tab what is on disk: the reader keeps their place.
    pub fn replace_text(&mut self, text: &str) {
        self.take(read::decode(text, None));
    }

    pub fn replace_file(&mut self, text: &str) -> bool {
        if self.is_dirty() {
            self.conflict = true;
            return false;
        }
        self.take(read::decode(text, self.modified));
        self.snapshot = Some(text.to_owned());
        self.refresh_find();
        let cursor = self.buffer.cursor();
        self.h_scroll = self.h_scroll.min(render::display_column(
            self.buffer.line(cursor.line),
            cursor.col,
        ));
        true
    }

    /// The host kept what a save of this document handed it, so nothing in
    /// the tab is unsaved any more.
    pub fn mark_saved(&mut self) {
        self.history.mark_saved();
        self.conflict = false;
    }

    /// Swaps in text from outside the buffer, keeping the caret and the
    /// scroll where they still fit. The undo history described the old text,
    /// so it goes with it.
    fn take(&mut self, loaded: read::Loaded) {
        let cursor = self.buffer.cursor();
        let scroll = self.scroll;
        self.line_ending = loaded.line_ending;
        self.trailing_newline = loaded.trailing_newline;
        self.notice = loaded.read_only;
        self.modified = loaded.modified;
        self.snapshot = None;
        self.buffer = Buffer::new(loaded.lines);
        self.changed();
        self.buffer.set_cursor(cursor, false);
        self.scroll = scroll.min(self.buffer.line_count().saturating_sub(1));
        self.scroll_row = 0;
        self.history = History::default();
        self.highlighter.invalidate_from(0);
        self.conflict = false;
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
        self.changed();
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
    /// the save path both rely on. The raised tab catches up with its file
    /// first, so asking for a file never shows what it used to say.
    pub fn open(&mut self, path: &Path, theme_generation: u64) -> Result<(), LoadError> {
        if let Some(tab) = self.raise(path) {
            tab.preview = false;
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
        if self.raise(path).is_some() {
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

    /// Makes the tab on `path` the active one and brings it up to date with
    /// its file. A read that fails has already flown the tab's conflict, which
    /// is what the reader needs to see, so there is nothing to pass back.
    fn raise(&mut self, path: &Path) -> Option<&mut Tab> {
        let identity = WorkbenchPath::Local(path.to_path_buf());
        let index = self.tabs.iter().position(|tab| tab.path == identity)?;
        self.active = index;
        let tab = &mut self.tabs[index];
        let _ = tab.refresh_if_changed();
        Some(tab)
    }

    /// Where the host's document `key` is open, if it is.
    pub fn document(&self, key: &DocumentKey) -> Option<usize> {
        self.tabs
            .iter()
            .position(|tab| tab.document.as_ref() == Some(key))
    }

    /// Names the tab on `path` the way the host asked, and takes that name off
    /// any other tab, so two tabs never both claim to be the plan.
    pub fn label(&mut self, path: &WorkbenchPath, label: TabLabel) {
        for tab in &mut self.tabs {
            if tab
                .label
                .as_ref()
                .is_some_and(|held| held.title == label.title)
            {
                tab.label = None;
            }
        }
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.path == *path) {
            tab.label = Some(label);
        }
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
    use super::rendered::PaintedMarkdown;
    use super::{DiffKind, DiffRow, DocumentKey, Editor, Tab, TabLabel, WorkbenchPath};
    use crate::fs::backend::{LoadedFile, ResourceEntry};
    use crate::fs::read;
    use crate::keys;
    use caudra_workspace::{ResourceKind, WorkspacePath};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::text::Line;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;
    use test_case::test_case;

    const MARKDOWN_FILE: &str = "notes.md";
    const MARKDOWN: &str = "# Title\n\nSome *prose*.\n";
    const NOT_TEXT: &[u8] = b"\0\0\0";
    const RENAMED_TEXT_FILE: &str = "notes.txt";
    const WRONG_ELIGIBILITY: &str = "the rendered view is offered to the wrong tabs";
    const STILL_RENDERED: &str = "a tab that stopped being Markdown is still rendered";
    const RAISED: &str = "opening an open file must raise its tab, not open a second one";
    const CLEAN_START: &str = "a freshly opened file must not claim to be dirty";
    const DIRTY_AFTER_EDIT: &str = "an edited buffer must claim to be dirty until it is saved";
    const NO_CLOBBER: &str = "a dirty buffer must not be replaced by what is on disk";
    const NO_ACTIVE: &str = "the tab just pushed must be the active one";
    const DIFF_READ_ONLY: &str = "a diff is not a file and must never be written back";
    const BUFFER_IS_THE_LINE: &str = "a diff row's buffer text is the line, not the patch line";
    const FIRST_LINE: &str = "fn main() {}";
    const PAGE_ROWS: usize = 10;
    const ALT_GR_TEXT: char = '@';
    const MOVED_WRONG: &str = "the chord did not take the caret where its motion points";
    const MOTION_UNREPORTED: &str = "a motion must report that the caret may have moved";
    const ALT_TYPED: &str = "AltGr types its character, and bare Alt, dead on macOS, types nothing";
    const PAINT_WIDTH: u16 = 40;
    const PAINT_THEME: u64 = 7;
    const ADDITION: &str = "draft ";
    const REPLACEMENT: &str = "# Replacement\n\nNew *contents*.\n";
    const CRLF_MARKDOWN: &str = "\r\n# Title\r\n\r\nSome *prose*.\r\n\r\n";
    const NO_FINAL_NEWLINE: &str = "\n# Title\nSome *prose*.  ";
    const BLANK_MARKDOWN: &str = "\n\n";
    const SPACE_MARKDOWN: &str = " \t\n  ";
    const EMPTY_MARKDOWN: &str = "";
    const SOURCE_DISTURBED: &str =
        "rendered selection disturbed the source cursor, selection or history";
    const STALE_SELECTION: &str =
        "source changes retained a rendered selection or anchor before repaint";
    const WRONG_SOURCE: &str = "rendered copy did not preserve the complete current document";
    const NO_RENDERED: &str = "the Markdown tab has no rendered view";
    const DOCUMENT_STATUS: &str = "draft";
    const SNAPSHOT_QUERY: &str = "Title";

    enum SourceEdit {
        Insert,
        Undo,
        Redo,
    }

    fn markdown_tab(text: &str) -> Tab {
        Tab::from_load(
            Path::new(MARKDOWN_FILE),
            read::decode(text, None),
            PAINT_THEME,
        )
    }

    fn paint_source(text: &str, _width: u16) -> PaintedMarkdown {
        let lines = text
            .lines()
            .map(|line| Line::from(line.to_owned()))
            .collect();
        let source = text.to_owned();
        PaintedMarkdown::new(lines, move |_, _, _| Some(source.clone()))
    }

    fn paint_placeholder(text: &str, _width: u16) -> PaintedMarkdown {
        let source = text.to_owned();
        PaintedMarkdown::new(vec![Line::default()], move |rows, start, end| {
            assert_eq!(rows.len(), 1, "{WRONG_SOURCE}");
            assert!(rows[0].spans.is_empty(), "{WRONG_SOURCE}");
            assert_eq!(start, (0, 0), "{WRONG_SOURCE}");
            assert_eq!(end, (0, 0), "{WRONG_SOURCE}");
            Some(source.clone())
        })
    }

    fn select_rendered(tab: &mut Tab) {
        if !tab.is_rendered() {
            assert!(tab.toggle_rendered(), "{NO_RENDERED}");
        }
        tab.rendered(PAINT_WIDTH, PAINT_THEME, paint_source)
            .expect(NO_RENDERED);
        tab.rendered_mut().expect(NO_RENDERED).select_all();
        assert_eq!(tab.selected_text(), Some(tab.contents()), "{WRONG_SOURCE}");
    }

    fn assert_rendered_selection_invalidated(tab: &mut Tab) {
        assert_eq!(tab.selected_text(), None, "{STALE_SELECTION}");
        let view = tab.rendered_mut().expect(NO_RENDERED);
        assert!(!view.is_selecting(), "{STALE_SELECTION}");
        view.extend_to(Cursor::new(usize::MAX, usize::MAX));
        assert!(!view.is_selecting(), "{STALE_SELECTION}");
        assert_eq!(view.selection_columns(0), None, "{STALE_SELECTION}");
        assert!(!tab.clear_selection(), "{STALE_SELECTION}");
    }

    fn remote_markdown(text: &str) -> LoadedFile {
        let loaded = read::decode(text, None);
        LoadedFile {
            entry: ResourceEntry {
                path: WorkbenchPath::Remote(WorkspacePath::new(MARKDOWN_FILE).unwrap()),
                resource_id: None,
                revision: None,
                kind: ResourceKind::File,
                size_bytes: None,
            },
            lines: loaded.lines,
            line_ending: loaded.line_ending,
            trailing_newline: loaded.trailing_newline,
        }
    }

    #[test_case(MARKDOWN ; "lf_and_final_newline")]
    #[test_case(CRLF_MARKDOWN ; "crlf_and_blank_boundaries")]
    #[test_case(NO_FINAL_NEWLINE ; "unterminated_and_trailing_spaces")]
    fn rendered_copy_uses_unsaved_source_with_original_line_endings(source: &str) {
        let mut tab = markdown_tab(source);
        let edit = tab.buffer.insert(ADDITION);
        assert!(tab.record(edit));
        select_rendered(&mut tab);

        assert!(tab.is_dirty(), "{DIRTY_AFTER_EDIT}");
        assert_eq!(
            tab.selected_text(),
            Some(format!("{ADDITION}{source}")),
            "{WRONG_SOURCE}"
        );
    }

    #[test_case(BLANK_MARKDOWN, true ; "blank_lines")]
    #[test_case(SPACE_MARKDOWN, true ; "spaces_and_tabs")]
    #[test_case(EMPTY_MARKDOWN, false ; "empty_source")]
    fn select_all_can_copy_invisible_source_but_plain_clicks_cannot(source: &str, nonempty: bool) {
        let mut tab = markdown_tab(source);
        assert!(tab.toggle_rendered(), "{NO_RENDERED}");
        tab.rendered(PAINT_WIDTH, PAINT_THEME, paint_placeholder)
            .expect(NO_RENDERED);
        tab.rendered_mut().expect(NO_RENDERED).select_all();

        assert_eq!(
            tab.selected_text().as_deref(),
            nonempty.then_some(source),
            "{WRONG_SOURCE}"
        );
        assert_eq!(
            tab.rendered_view().expect(NO_RENDERED).is_selecting(),
            nonempty,
            "{WRONG_SOURCE}"
        );
        assert_eq!(
            tab.rendered_view().expect(NO_RENDERED).selection_columns(0),
            None,
            "{WRONG_SOURCE}"
        );
        tab.rendered(PAINT_WIDTH, PAINT_THEME + 1, paint_placeholder)
            .expect(NO_RENDERED);
        assert_eq!(
            tab.selected_text().as_deref(),
            nonempty.then_some(source),
            "{WRONG_SOURCE}"
        );
        assert_eq!(tab.clear_selection(), nonempty, "{WRONG_SOURCE}");

        let view = tab.rendered_mut().expect(NO_RENDERED);
        view.select_all();
        view.select_at(Cursor::default(), 1);
        view.extend_to(Cursor::default());
        assert_eq!(tab.selected_text(), None, "{WRONG_SOURCE}");
        assert!(!tab.clear_selection(), "{WRONG_SOURCE}");

        tab.rendered_mut().expect(NO_RENDERED).select_all();
        tab.replace_text(EMPTY_MARKDOWN);
        assert_rendered_selection_invalidated(&mut tab);
        tab.rendered(PAINT_WIDTH, PAINT_THEME, paint_placeholder)
            .expect(NO_RENDERED);
        tab.rendered_mut().expect(NO_RENDERED).select_all();
        assert_eq!(tab.selected_text(), None, "{WRONG_SOURCE}");
        assert!(!tab.clear_selection(), "{WRONG_SOURCE}");
    }

    #[test_case(false ; "clear_rendered_selection")]
    #[test_case(true ; "leave_rendered_selection")]
    fn toggling_rendered_preserves_source_selection_cursor_and_history(leave_selected: bool) {
        let mut tab = markdown_tab(MARKDOWN);
        let edit = tab.buffer.insert(ADDITION);
        assert!(tab.record(edit));
        tab.buffer.select_word_at(Cursor::default());
        let selection = tab.buffer.selection();
        let source_selection = tab.selected_text();
        let cursor = tab.buffer.cursor();
        let revision = tab.revision();
        let contents = tab.contents();

        assert!(tab.toggle_rendered(), "{NO_RENDERED}");
        assert_eq!(tab.selected_text(), None, "{SOURCE_DISTURBED}");
        select_rendered(&mut tab);
        if !leave_selected {
            assert!(tab.clear_selection(), "{SOURCE_DISTURBED}");
            assert!(!tab.clear_selection(), "{SOURCE_DISTURBED}");
        }
        assert!(tab.toggle_rendered(), "{NO_RENDERED}");

        assert_eq!(tab.buffer.cursor(), cursor, "{SOURCE_DISTURBED}");
        assert_eq!(tab.buffer.selection(), selection, "{SOURCE_DISTURBED}");
        assert_eq!(tab.selected_text(), source_selection, "{SOURCE_DISTURBED}");
        assert_eq!(tab.revision(), revision, "{SOURCE_DISTURBED}");
        assert_eq!(tab.contents(), contents, "{SOURCE_DISTURBED}");
        assert!(tab.is_dirty(), "{SOURCE_DISTURBED}");
        assert!(tab.clear_selection(), "{SOURCE_DISTURBED}");
        assert!(!tab.clear_selection(), "{SOURCE_DISTURBED}");
        assert_eq!(tab.buffer.cursor(), cursor, "{SOURCE_DISTURBED}");

        assert!(tab.toggle_rendered(), "{NO_RENDERED}");
        assert!(
            !tab.rendered_view().expect(NO_RENDERED).is_selecting(),
            "{STALE_SELECTION}"
        );
        tab.show_source();
        assert!(tab.undo(), "{SOURCE_DISTURBED}");
        assert_eq!(tab.contents(), MARKDOWN, "{SOURCE_DISTURBED}");
        assert!(tab.redo(), "{SOURCE_DISTURBED}");
        assert_eq!(tab.contents(), contents, "{SOURCE_DISTURBED}");
    }

    #[test_case(SourceEdit::Insert ; "record")]
    #[test_case(SourceEdit::Undo ; "undo")]
    #[test_case(SourceEdit::Redo ; "redo")]
    fn source_edits_invalidate_rendered_selection_before_repaint(change: SourceEdit) {
        let mut tab = markdown_tab(MARKDOWN);
        let edit = tab.buffer.insert(ADDITION);
        assert!(tab.record(edit));
        if matches!(change, SourceEdit::Redo) {
            assert!(tab.undo());
        }
        select_rendered(&mut tab);
        let revision = tab.revision();

        assert!(match change {
            SourceEdit::Insert => {
                let edit = tab.buffer.insert(ADDITION);
                tab.record(edit)
            }
            SourceEdit::Undo => tab.undo(),
            SourceEdit::Redo => tab.redo(),
        });

        assert_eq!(tab.revision(), revision + 1);
        assert_rendered_selection_invalidated(&mut tab);
    }

    #[test_case(REPLACEMENT ; "host_document_replacement")]
    fn host_replacement_invalidates_rendered_selection_before_repaint(replacement: &str) {
        let mut tab = Tab::document(
            Path::new(MARKDOWN_FILE),
            DocumentKey(MARKDOWN_FILE.to_owned()),
            TabLabel {
                title: MARKDOWN_FILE.to_owned(),
                status: DOCUMENT_STATUS.to_owned(),
            },
            MARKDOWN,
            PAINT_THEME,
        );
        let cursor = Cursor::new(0, 3);
        tab.buffer.set_cursor(cursor, false);
        select_rendered(&mut tab);

        tab.replace_text(replacement);

        assert_rendered_selection_invalidated(&mut tab);
        assert_eq!(tab.contents(), replacement, "{WRONG_SOURCE}");
        assert_eq!(tab.buffer.cursor(), cursor, "{SOURCE_DISTURBED}");
        select_rendered(&mut tab);
        assert_eq!(
            tab.selected_text().as_deref(),
            Some(replacement),
            "{WRONG_SOURCE}"
        );
    }

    #[test_case(false ; "clean_remote_reload")]
    #[test_case(true ; "discard_dirty_remote_reload")]
    fn remote_replacement_invalidates_rendered_selection_before_repaint(discard: bool) {
        let mut tab = Tab::from_backend(remote_markdown(MARKDOWN), PAINT_THEME);
        if discard {
            let edit = tab.buffer.insert(ADDITION);
            assert!(tab.record(edit));
        }
        select_rendered(&mut tab);
        let cursor = tab.buffer.cursor();

        if discard {
            tab.discard_backend(remote_markdown(REPLACEMENT));
        } else {
            assert!(tab.apply_backend_reload(remote_markdown(REPLACEMENT)));
        }

        assert_rendered_selection_invalidated(&mut tab);
        assert_eq!(tab.contents(), REPLACEMENT, "{WRONG_SOURCE}");
        assert_eq!(tab.buffer.cursor(), cursor, "{SOURCE_DISTURBED}");
        assert!(!tab.is_dirty(), "{CLEAN_START}");
    }

    #[test_case(false ; "clean_local_reload")]
    #[test_case(true ; "discard_dirty_local_reload")]
    fn disk_reload_invalidates_rendered_selection_before_repaint(discard: bool) {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(MARKDOWN_FILE);
        fs::write(&path, MARKDOWN).unwrap();
        let mut tab = Tab::open(&path, PAINT_THEME).unwrap();
        if discard {
            let edit = tab.buffer.insert(ADDITION);
            assert!(tab.record(edit));
        }
        select_rendered(&mut tab);
        fs::write(&path, REPLACEMENT).unwrap();

        if discard {
            tab.discard_and_reload().unwrap();
        } else {
            assert!(tab.reload_from_disk().unwrap());
        }

        assert_rendered_selection_invalidated(&mut tab);
        assert_eq!(tab.contents(), REPLACEMENT, "{WRONG_SOURCE}");
    }

    #[test_case(REPLACEMENT, Cursor::new(2, 3), 2, false ; "preserves_fitting_source")]
    #[test_case(REPLACEMENT, Cursor::new(2, 3), 2, true ; "preserves_fitting_rendered")]
    #[test_case(EMPTY_MARKDOWN, Cursor::new(0, 0), 0, false ; "clamps_shorter_source")]
    #[test_case(EMPTY_MARKDOWN, Cursor::new(0, 0), 0, true ; "clamps_shorter_rendered")]
    fn file_snapshot_refreshes_view_search_and_history(
        text: &str,
        cursor: Cursor,
        scroll: usize,
        rendered: bool,
    ) {
        let mut tab = markdown_tab(MARKDOWN);
        let edit = tab.buffer.insert(ADDITION);
        assert!(tab.record(edit));
        assert!(tab.undo());
        tab.buffer.set_cursor(Cursor::new(2, 3), false);
        tab.set_scroll(2);
        tab.h_scroll = PAINT_WIDTH as usize;
        tab.find.open();
        tab.find.query_mut().set_text(SNAPSHOT_QUERY);
        tab.search_find();
        assert!(tab.find.current().is_some());
        if rendered {
            select_rendered(&mut tab);
        }
        let modified = tab.modified;

        assert!(tab.replace_file(text));

        assert_eq!(tab.contents(), text);
        assert_eq!(tab.buffer.cursor(), cursor);
        assert_eq!(tab.scroll(), scroll);
        assert_eq!(tab.scroll_row, 0);
        assert!(tab.h_scroll <= cursor.col);
        assert_eq!(tab.modified, modified);
        assert_eq!(tab.snapshot.as_deref(), Some(text));
        assert!(tab.find.current().is_none());
        assert_eq!(tab.find.is_open(), !rendered);
        if rendered {
            assert_rendered_selection_invalidated(&mut tab);
        }
        assert!(!tab.is_dirty());
        assert!(!tab.undo() && !tab.redo());
    }

    #[test_case(false ; "reload_clean_snapshot")]
    #[test_case(true ; "discard_edited_snapshot")]
    fn disk_reload_replaces_the_snapshot_save_baseline(dirty: bool) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(MARKDOWN_FILE);
        fs::write(&path, MARKDOWN).unwrap();
        let mut tab = Tab::open(&path, PAINT_THEME).unwrap();
        assert!(tab.replace_file(REPLACEMENT));
        if dirty {
            let edit = tab.buffer.insert(ADDITION);
            assert!(tab.record(edit));
            tab.discard_and_reload().unwrap();
        } else {
            assert!(tab.reload_from_disk().unwrap());
        }
        assert_eq!(tab.contents(), MARKDOWN);
        assert!(tab.snapshot.is_none());
        let edit = tab.buffer.insert(ADDITION);
        assert!(tab.record(edit));
        tab.save().unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), tab.contents());
    }

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

        #[test_case(false, ORIGINAL, true ; "unchanged_verified_source")]
        #[test_case(false, EXTERNAL, false ; "snapshot_cannot_reauthorize_source")]
        #[test_case(true, ORIGINAL, false ; "same_bytes_replacement_keeps_identity_guard")]
        fn snapshot_preserves_verified_source_authority(
            replaced: bool,
            text: &str,
            can_save: bool,
        ) {
            let (_dir, path, mut tab) = fixture();
            if replaced {
                fs::remove_file(&path).unwrap();
                fs::write(&path, ORIGINAL).unwrap();
            }
            assert!(tab.replace_file(text));
            let contents = edit(&mut tab);
            let saved = tab.save();
            if can_save {
                saved.unwrap();
                assert_eq!(fs::read_to_string(&path).unwrap(), contents);
                assert!(tab.reload_from_disk().unwrap());
            } else {
                assert!(matches!(saved, Err(SaveError::Stale(_))));
                assert!(tab.conflict && tab.is_dirty());
                assert_eq!(fs::read_to_string(&path).unwrap(), ORIGINAL);
            }
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
        fs::write(&path, format!("{FIRST_LINE}\n")).unwrap();
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

    #[test_case(keys::LINE_END, 0, FIRST_LINE.len() ; "ctrl e goes to the line end")]
    #[test_case(keys::SUPER_END, 0, FIRST_LINE.len() ; "super right goes to the line end")]
    #[test_case(keys::SUPER_HOME, FIRST_LINE.len(), 0 ; "super left goes to the line start")]
    fn a_line_chord_moves_the_caret_along_its_line(chord: keys::Bind, from: usize, to: usize) {
        let (_tmp, path) = fixture();
        let mut tab = Tab::open(&path, 0).unwrap();
        tab.buffer.set_cursor(Cursor::new(0, from), false);

        assert!(
            tab.edit_key(chord.to_key_event(), PAGE_ROWS),
            "{MOTION_UNREPORTED}"
        );
        assert_eq!(tab.buffer.cursor(), Cursor::new(0, to), "{MOVED_WRONG}");
    }

    #[test_case(KeyModifiers::CONTROL | KeyModifiers::ALT, true ; "altgr types")]
    #[test_case(KeyModifiers::ALT, false ; "bare alt types nothing")]
    fn only_altgr_types_through_alt(modifiers: KeyModifiers, types: bool) {
        let (_tmp, path) = fixture();
        let mut tab = Tab::open(&path, 0).unwrap();

        let typed = KeyEvent::new(KeyCode::Char(ALT_GR_TEXT), modifiers);

        assert_eq!(tab.edit_key(typed, PAGE_ROWS), types, "{ALT_TYPED}");
        assert_eq!(
            tab.buffer.text().starts_with(ALT_GR_TEXT),
            types,
            "{ALT_TYPED}"
        );
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

    #[test_case("notes.md", true ; "markdown")]
    #[test_case("README.MD", true ; "markdown in capitals")]
    #[test_case("guide.markdown", true ; "the long extension")]
    #[test_case("main.rs", false ; "source code")]
    #[test_case("md", false ; "a name that is only the extension")]
    fn only_markdown_has_a_rendered_view(name: &str, expected: bool) {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(name);
        fs::write(&path, MARKDOWN).unwrap();
        let mut tab = Tab::open(&path, 0).unwrap();

        assert_eq!(tab.toggle_rendered(), expected, "{WRONG_ELIGIBILITY}");
        assert_eq!(tab.is_rendered(), expected, "{WRONG_ELIGIBILITY}");
    }

    fn not_text(path: &Path) -> Tab {
        fs::write(path, NOT_TEXT).unwrap();
        Tab::open(path, 0).unwrap()
    }

    fn diff(path: &Path) -> Tab {
        let rows = vec![diff_row(MARKDOWN, DiffKind::Added)];
        Tab::synthetic(path, MARKDOWN_FILE.to_owned(), rows, 0)
    }

    #[test_case(not_text ; "a file that is not text")]
    #[test_case(diff ; "a diff")]
    fn a_markdown_name_is_not_enough_to_render(build: fn(&Path) -> Tab) {
        let tmp = TempDir::new().unwrap();
        let mut tab = build(&tmp.path().join(MARKDOWN_FILE));

        assert!(!tab.toggle_rendered(), "{WRONG_ELIGIBILITY}");
    }

    #[test]
    fn renaming_away_from_markdown_goes_back_to_the_source() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(MARKDOWN_FILE);
        fs::write(&path, MARKDOWN).unwrap();
        let mut tab = Tab::open(&path, 0).unwrap();
        tab.toggle_rendered();

        let renamed = WorkbenchPath::Local(tmp.path().join(RENAMED_TEXT_FILE));
        tab.rename(renamed, None, 0);

        assert!(!tab.is_rendered(), "{STILL_RENDERED}");
    }
}
