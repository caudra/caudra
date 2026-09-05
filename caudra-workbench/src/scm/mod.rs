//! The Source Control pane: what the repository has to say, and the things the
//! workbench can do about it.
//!
//! The pane is a stack of three sections — staged changes, unstaged changes,
//! and the commit graph — each with its own scroll, its own share of the
//! height, and its own fold. One cursor walks all three, so a single set of
//! arrow keys covers the pane the way it covers a single list.
//!
//! Everything here is synchronous. A `git status` over a working tree is fast
//! enough to run on a keystroke, and a worker would buy latency the pane cannot
//! spend: the list has to be correct the instant it is drawn, because the next
//! key stages whatever the cursor is on.

pub mod diff;
pub mod graph;
pub mod repo;
pub mod tree;

use std::collections::HashSet;
use std::path::Path;

use crate::fs::tree::GitMark;
use graph::Rail;
use repo::{Change, Commit, Repo, ScmError};
use tree::{Dir, SEPARATOR};

/// The body rows a section asks for before the frame has any say. Eight lists a
/// useful number of files without crowding out the two sections underneath.
pub const DEFAULT_SECTION_ROWS: u16 = 8;
/// An expanded section always keeps one row of body, so collapsing stays the
/// only way to reduce it to its header and a drag cannot squeeze it away.
pub const MIN_SECTION_ROWS: u16 = 1;

const STAGED_TITLE: &str = "STAGED CHANGES";
const UNSTAGED_TITLE: &str = "CHANGES";
const GRAPH_TITLE: &str = "GRAPH";

/// One band of the pane. The declaration order is the order they are stacked,
/// which is the order [`Section::ALL`] and every layout pass walk them in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Section {
    #[default]
    Staged,
    Unstaged,
    Graph,
}

impl Section {
    pub const ALL: [Self; 3] = [Self::Staged, Self::Unstaged, Self::Graph];
    pub const COUNT: usize = Self::ALL.len();

    pub const fn title(self) -> &'static str {
        match self {
            Self::Staged => STAGED_TITLE,
            Self::Unstaged => UNSTAGED_TITLE,
            Self::Graph => GRAPH_TITLE,
        }
    }

    const fn index(self) -> usize {
        self as usize
    }

    /// Whether the section lists paths that can be staged, which is what tells
    /// the two change sections from the graph without matching twice.
    const fn is_changes(self) -> bool {
        matches!(self, Self::Staged | Self::Unstaged)
    }
}

/// A row of a section's body. Changes, folders and commits are held by index so
/// the pane keeps one copy of each and a row stays [`Copy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    Directory(usize),
    Change { index: usize, depth: usize },
    Commit(usize),
}

/// Where the pane's one cursor is. `row` is `None` on the section's header,
/// which is how the keyboard reaches a fold without the header having to live
/// in the scrolling body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cursor {
    pub section: Section,
    pub row: Option<usize>,
}

/// What a press of the discard key did. Throwing away a worktree change cannot
/// be undone from anywhere, so the first press only arms it and the second
/// carries it out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Discard {
    Nothing,
    Armed(String),
    Done(String),
}

/// Where the cursor was before a rebuild, in the only terms that survive one.
/// Row indices do not on their own: staging a file takes it out of one section
/// and puts it in the other, so the row is only the fallback for a path that
/// has gone from this section entirely.
struct Anchor {
    section: Section,
    row: Option<usize>,
    identity: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SectionState {
    collapsed: bool,
    /// Body rows this section asks for. The frame has the final say, and the
    /// last expanded section flexes to fill whatever is left.
    height: u16,
    scroll: usize,
    rows: Vec<Row>,
    dirs: Vec<Dir>,
    /// Paths or commits behind the rows, which is what the header counts. Rows
    /// cannot answer for it: a folded folder hides the files under it.
    count: usize,
}

impl Default for SectionState {
    fn default() -> Self {
        Self {
            collapsed: false,
            height: DEFAULT_SECTION_ROWS,
            scroll: 0,
            rows: Vec::new(),
            dirs: Vec::new(),
            count: 0,
        }
    }
}

#[derive(Default)]
pub struct Scm {
    repo: Option<Repo>,
    changes: Vec<Change>,
    log: Vec<Commit>,
    rails: Vec<Rail>,
    sections: [SectionState; Section::COUNT],
    cursor: Cursor,
    /// Folders the reader closed, keyed on the repository-relative path. Shared
    /// by both change sections, so a path that is both staged and dirty folds
    /// the same way under each.
    folded: HashSet<String>,
    /// Whether the change sections nest their paths under folders. Tree is the
    /// default; flat is the escape hatch for a wide change set.
    flat: bool,
    head: Option<String>,
    error: Option<String>,
    armed: Option<String>,
}

impl Scm {
    /// Points the pane at a repository, forgetting whatever it was showing.
    /// Discovery only fails when there is no worktree repository above `root`,
    /// which leaves nothing to act on and nothing to report beyond an empty
    /// pane, so the rest of the workbench carries on unchanged.
    pub fn open(&mut self, root: &Path) {
        let carried = (self.flat, self.sections.clone());
        *self = Self::default();
        self.flat = carried.0;
        self.sections = carried.1;
        for state in &mut self.sections {
            state.scroll = 0;
            state.rows.clear();
            state.dirs.clear();
        }
        if let Ok(repo) = Repo::discover(root) {
            self.repo = Some(repo);
            self.refresh();
            // Opening the pane lands on something actionable rather than on a
            // title, so the first key does what it looks like it will.
            self.select_first_row();
        }
    }

    /// Rereads the repository. The log costs a walk, so it is only reread when
    /// the graph is open to show it.
    pub fn refresh(&mut self) {
        // Read before the lists are replaced: the rows index into them, so a
        // cursor read afterwards would name whatever slid into its slot.
        let previous = self.anchor();
        let Some(repo) = &self.repo else {
            return;
        };
        self.head = repo.head_label();
        self.error = None;
        match repo.status() {
            Ok(changes) => self.changes = changes,
            Err(error) => {
                self.changes.clear();
                self.error = Some(error.to_string());
            }
        }
        if !self.sections[Section::Graph.index()].collapsed {
            match repo.log() {
                Ok(log) => self.log = log,
                Err(error) => {
                    self.log.clear();
                    self.error = Some(error.to_string());
                }
            }
        }
        self.rails = graph::rails(&self.log);
        self.rebuild(previous);
    }

    pub fn is_repository(&self) -> bool {
        self.repo.is_some()
    }

    /// The repository's root, which is not the workbench's root when the
    /// workbench was opened in a subdirectory. Change paths are relative to it.
    pub fn workdir(&self) -> Option<&Path> {
        self.repo.as_ref().map(Repo::workdir)
    }

    pub fn head(&self) -> Option<&str> {
        self.head.as_deref()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn is_flat(&self) -> bool {
        self.flat
    }

    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    pub fn rows(&self, section: Section) -> &[Row] {
        &self.sections[section.index()].rows
    }

    pub fn dir(&self, section: Section, index: usize) -> Option<&Dir> {
        self.sections[section.index()].dirs.get(index)
    }

    pub fn scroll(&self, section: Section) -> usize {
        self.sections[section.index()].scroll
    }

    pub fn count(&self, section: Section) -> usize {
        self.sections[section.index()].count
    }

    pub fn is_collapsed(&self, section: Section) -> bool {
        self.sections[section.index()].collapsed
    }

    pub fn height(&self, section: Section) -> u16 {
        self.sections[section.index()].height
    }

    pub fn change(&self, index: usize) -> Option<&Change> {
        self.changes.get(index)
    }

    pub fn commit(&self, index: usize) -> Option<&Commit> {
        self.log.get(index)
    }

    pub fn rail(&self, index: usize) -> Option<Rail> {
        self.rails.get(index).copied()
    }

    /// Whether every section has nothing to list, which is when the pane says
    /// so in one line instead of drawing three empty bands.
    pub fn is_empty(&self) -> bool {
        self.sections.iter().all(|state| state.count == 0)
    }

    /// Swaps the change sections between nesting paths under folders and
    /// listing them whole.
    pub fn toggle_flat(&mut self) {
        self.flat = !self.flat;
        let previous = self.anchor();
        self.rebuild(previous);
    }

    pub fn set_collapsed(&mut self, section: Section, collapsed: bool) {
        self.sections[section.index()].collapsed = collapsed;
        if collapsed && self.cursor.section == section {
            self.cursor.row = None;
        }
        // The log is only walked while the graph is open, so opening it is the
        // moment that walk has to happen.
        if !collapsed && section == Section::Graph && self.log.is_empty() {
            self.refresh();
        }
    }

    pub fn toggle_collapsed(&mut self, section: Section) {
        self.set_collapsed(section, !self.is_collapsed(section));
    }

    pub fn set_height(&mut self, section: Section, height: u16) {
        self.sections[section.index()].height = height.max(MIN_SECTION_ROWS);
    }

    /// Moves by `delta` cursor stops, counting a section's header as one and
    /// stepping over the body of a folded section.
    pub fn move_cursor(&mut self, delta: isize) {
        let stops = self.stops();
        let Some(last) = stops.len().checked_sub(1) else {
            return;
        };
        let current = stops
            .iter()
            .position(|stop| *stop == self.cursor)
            .unwrap_or_default();
        self.cursor = stops[current.saturating_add_signed(delta).min(last)];
    }

    pub fn select_first(&mut self) {
        self.cursor = self.stops().first().copied().unwrap_or_default();
    }

    pub fn select_last(&mut self) {
        self.cursor = self.stops().last().copied().unwrap_or_default();
    }

    /// Puts the cursor on the first row of the first section that has one,
    /// leaving it where it was when every section is empty.
    fn select_first_row(&mut self) {
        if let Some(stop) = self.stops().into_iter().find(|stop| stop.row.is_some()) {
            self.cursor = stop;
        }
    }

    /// Lands the cursor on a stop the pointer named. A row past the end of a
    /// section is ignored rather than clamped: it is empty space under the
    /// list, and clicking nothing should select nothing.
    pub fn select(&mut self, section: Section, row: Option<usize>) {
        let cursor = Cursor { section, row };
        if self.stops().contains(&cursor) {
            self.cursor = cursor;
        }
    }

    /// Keeps a section's scroll inside its list, and pulls it far enough to
    /// show the cursor when the cursor is in this section.
    pub fn clamp_scroll(&mut self, section: Section, viewport: usize) {
        let state = &mut self.sections[section.index()];
        if viewport == 0 {
            state.scroll = 0;
            return;
        }
        if let Some(row) = self.cursor.row.filter(|_| self.cursor.section == section) {
            if row < state.scroll {
                state.scroll = row;
            } else if row >= state.scroll + viewport {
                state.scroll = row + 1 - viewport;
            }
        }
        state.scroll = state.scroll.min(state.rows.len().saturating_sub(viewport));
    }

    pub fn scroll_by(&mut self, section: Section, delta: isize, viewport: usize) {
        let state = &mut self.sections[section.index()];
        let max = state.rows.len().saturating_sub(viewport);
        state.scroll = state.scroll.saturating_add_signed(delta).min(max);
    }

    /// Folds what the cursor is on, or steps out to whatever holds it. This is
    /// what `Left` means in a tree, and it is the only way back to a header
    /// without walking the whole section.
    pub fn fold(&mut self) {
        match self.cursor.row {
            None => self.set_collapsed(self.cursor.section, true),
            Some(row) => match self.dir_at(self.cursor.section, row) {
                Some(index) if self.dir_expanded(self.cursor.section, index) => {
                    self.fold_dir(self.cursor.section, index, true);
                }
                _ => self.cursor.row = self.parent_of(self.cursor.section, row),
            },
        }
    }

    /// Unfolds what the cursor is on. Reports whether it had anything to open,
    /// so the caller can fall through to opening a file instead.
    pub fn unfold(&mut self) -> bool {
        match self.cursor.row {
            None if self.is_collapsed(self.cursor.section) => {
                self.set_collapsed(self.cursor.section, false);
                true
            }
            None => false,
            Some(row) => match self.dir_at(self.cursor.section, row) {
                Some(index) if !self.dir_expanded(self.cursor.section, index) => {
                    self.fold_dir(self.cursor.section, index, false);
                    true
                }
                Some(_) => true,
                None => false,
            },
        }
    }

    /// Folds or unfolds whatever the cursor is on. Reports whether it was
    /// something foldable, so `Enter` can go on to open a file or a commit.
    pub fn toggle_fold(&mut self) -> bool {
        match self.cursor.row {
            None => {
                self.toggle_collapsed(self.cursor.section);
                true
            }
            Some(row) => match self.dir_at(self.cursor.section, row) {
                Some(index) => {
                    let expanded = self.dir_expanded(self.cursor.section, index);
                    self.fold_dir(self.cursor.section, index, expanded);
                    true
                }
                None => false,
            },
        }
    }

    pub fn selected_change(&self) -> Option<&Change> {
        match self.row_at(self.cursor.section, self.cursor.row?)? {
            Row::Change { index, .. } => self.changes.get(index),
            _ => None,
        }
    }

    pub fn selected_commit(&self) -> Option<&Commit> {
        match self.row_at(self.cursor.section, self.cursor.row?)? {
            Row::Commit(index) => self.log.get(index),
            _ => None,
        }
    }

    /// Stages what the cursor covers, or unstages it when the cursor is in the
    /// staged section: one file, everything under one folder, or the whole
    /// section from its header.
    pub fn stage(&mut self) -> Result<(), ScmError> {
        let section = self.cursor.section;
        if !section.is_changes() {
            return Ok(());
        }
        let paths = self.scope();
        let Some(repo) = &self.repo else {
            return Ok(());
        };
        for relative in &paths {
            match section {
                Section::Staged => repo.unstage(relative)?,
                _ => repo.stage(relative)?,
            }
        }
        if !paths.is_empty() {
            self.refresh();
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn stage_path(&mut self, relative: &str) -> Result<(), ScmError> {
        if let Some(repo) = &self.repo {
            repo.stage(relative)?;
            self.refresh();
        }
        Ok(())
    }

    /// Throws away the worktree's copy of the selected change, restoring it
    /// from the index. A staged change stays staged: this only undoes what has
    /// not been recorded anywhere yet.
    ///
    /// One file at a time, deliberately. A folder-wide discard is the most
    /// destructive thing the pane could offer and the cursor is not a careful
    /// enough way to ask for it.
    pub fn discard(&mut self) -> Result<Discard, ScmError> {
        let Some(change) = self.selected_change() else {
            return Ok(Discard::Nothing);
        };
        let relative = change.relative.clone();
        if self.armed.as_deref() != Some(relative.as_str()) {
            self.armed = Some(relative.clone());
            return Ok(Discard::Armed(relative));
        }
        self.armed = None;
        let Some(repo) = &self.repo else {
            return Ok(Discard::Nothing);
        };
        repo.discard(&relative)?;
        self.refresh();
        Ok(Discard::Done(relative))
    }

    /// Cancels an armed discard, which every other key in the pane does.
    pub fn disarm(&mut self) {
        self.armed = None;
    }

    /// The rendered diff for the selected change, and the path it belongs to.
    pub fn selected_diff(&self) -> Result<Option<(Change, diff::Diff)>, ScmError> {
        let Some((repo, change)) = self.repo.as_ref().zip(self.selected_change()) else {
            return Ok(None);
        };
        let (old, new) = repo.sides(&change.relative, change.staged)?;
        Ok(Some((change.clone(), diff::unified(&old, &new))))
    }

    /// The rendered contents of the selected commit, and the commit itself.
    pub fn selected_commit_diff(&self) -> Result<Option<(Commit, diff::Diff)>, ScmError> {
        let Some((repo, commit)) = self.repo.as_ref().zip(self.selected_commit()) else {
            return Ok(None);
        };
        let detail = repo.commit_detail(&commit.id)?;
        Ok(Some((commit.clone(), diff::commit(&detail))))
    }

    /// The marks the explorer paints at the end of its rows. A path changed in
    /// both the index and the worktree keeps the worktree's mark, because that
    /// is the one describing the file the tree is showing.
    pub fn marks(&self) -> impl Fn(&Path) -> Option<GitMark> + '_ {
        move |path: &Path| {
            self.changes
                .iter()
                .filter(|change| change.path == path)
                .min_by_key(|change| u8::from(change.staged))
                .map(|change| change.mark)
        }
    }

    /// What the host stores between runs, per section and in stacking order.
    pub fn saved(&self) -> (bool, Vec<(u16, bool)>) {
        let sections = self
            .sections
            .iter()
            .map(|state| (state.height, state.collapsed))
            .collect();
        (self.flat, sections)
    }

    /// Puts a stored layout back. The sections are zipped rather than indexed,
    /// so a list written by a build that knew a different number of them keeps
    /// the defaults for whatever it does not name.
    pub fn restore(&mut self, flat: bool, sections: &[(u16, bool)]) {
        self.flat = flat;
        for (state, (height, collapsed)) in self.sections.iter_mut().zip(sections) {
            state.height = (*height).max(MIN_SECTION_ROWS);
            state.collapsed = *collapsed;
        }
    }

    /// Every place the cursor can rest, in the order the pane draws them.
    fn stops(&self) -> Vec<Cursor> {
        let mut stops = Vec::new();
        for section in Section::ALL {
            stops.push(Cursor { section, row: None });
            let state = &self.sections[section.index()];
            if state.collapsed {
                continue;
            }
            stops.extend((0..state.rows.len()).map(|row| Cursor {
                section,
                row: Some(row),
            }));
        }
        stops
    }

    /// The repository-relative paths the cursor covers, which is what a staging
    /// key acts on.
    fn scope(&self) -> Vec<String> {
        let section = self.cursor.section;
        if !section.is_changes() {
            return Vec::new();
        }
        let staged = section == Section::Staged;
        let side = self
            .changes
            .iter()
            .filter(move |change| change.staged == staged);
        let Some(row) = self.cursor.row else {
            return side.map(|change| change.relative.clone()).collect();
        };
        match self.row_at(section, row) {
            Some(Row::Change { index, .. }) => self
                .changes
                .get(index)
                .map(|change| vec![change.relative.clone()])
                .unwrap_or_default(),
            Some(Row::Directory(index)) => {
                let Some(dir) = self.dir(section, index) else {
                    return Vec::new();
                };
                let prefix = format!("{}{SEPARATOR}", dir.path);
                side.filter(|change| change.relative.starts_with(&prefix))
                    .map(|change| change.relative.clone())
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    fn row_at(&self, section: Section, row: usize) -> Option<Row> {
        self.sections[section.index()].rows.get(row).copied()
    }

    fn dir_at(&self, section: Section, row: usize) -> Option<usize> {
        match self.row_at(section, row)? {
            Row::Directory(index) => Some(index),
            _ => None,
        }
    }

    fn dir_expanded(&self, section: Section, index: usize) -> bool {
        self.dir(section, index).is_some_and(|dir| dir.expanded)
    }

    fn fold_dir(&mut self, section: Section, index: usize, fold: bool) {
        let Some(dir) = self.dir(section, index) else {
            return;
        };
        let path = dir.path.clone();
        match fold {
            true => drop(self.folded.insert(path)),
            false => drop(self.folded.remove(&path)),
        }
        let previous = self.anchor();
        self.rebuild(previous);
    }

    /// The folder holding the row, which `Left` steps out to. A row at the top
    /// level has none, and the header stands in for it.
    fn parent_of(&self, section: Section, row: usize) -> Option<usize> {
        let depth = match self.row_at(section, row)? {
            Row::Change { depth, .. } => depth,
            Row::Directory(index) => self.dir(section, index)?.depth,
            Row::Commit(_) => return None,
        };
        (0..row).rev().find(|candidate| {
            self.dir_at(section, *candidate)
                .and_then(|index| self.dir(section, index))
                .is_some_and(|dir| dir.depth < depth)
        })
    }

    fn rebuild(&mut self, previous: Option<Anchor>) {
        for section in [Section::Staged, Section::Unstaged] {
            let staged = section == Section::Staged;
            let paths: Vec<(usize, &str)> = self
                .changes
                .iter()
                .enumerate()
                .filter(|(_, change)| change.staged == staged)
                .map(|(index, change)| (index, change.relative.as_str()))
                .collect();
            let (rows, dirs) = tree::rows(&paths, self.flat, &self.folded);
            let state = &mut self.sections[section.index()];
            state.count = paths.len();
            state.rows = rows;
            state.dirs = dirs;
        }
        let graph = &mut self.sections[Section::Graph.index()];
        graph.count = self.log.len();
        graph.rows = (0..self.log.len()).map(Row::Commit).collect();
        graph.dirs = Vec::new();

        if let Some(anchor) = previous {
            let found = anchor
                .identity
                .as_deref()
                .and_then(|identity| self.row_of(anchor.section, identity));
            self.cursor = Cursor {
                section: anchor.section,
                // A path that left this section leaves the cursor at the row it
                // held, so staging a run of files walks down the list instead
                // of chasing each one into the other section.
                row: found.or(anchor.row),
            };
        }
        self.clamp_cursor();
    }

    fn anchor(&self) -> Option<Anchor> {
        let section = self.cursor.section;
        Some(Anchor {
            section,
            row: self.cursor.row,
            identity: self.cursor.row.and_then(|row| self.identity(section, row)),
        })
    }

    fn identity(&self, section: Section, row: usize) -> Option<String> {
        match self.row_at(section, row)? {
            Row::Change { index, .. } => {
                Some(self.changes.get(index)?.relative.clone())
            }
            Row::Directory(index) => Some(self.dir(section, index)?.path.clone()),
            Row::Commit(index) => Some(self.log.get(index)?.id.clone()),
        }
    }

    fn row_of(&self, section: Section, identity: &str) -> Option<usize> {
        (0..self.sections[section.index()].rows.len())
            .find(|row| self.identity(section, *row).as_deref() == Some(identity))
    }

    /// Snaps the cursor back onto a row that exists, so a refresh that emptied
    /// a section leaves it on that section's header rather than past the end.
    fn clamp_cursor(&mut self) {
        let state = &self.sections[self.cursor.section.index()];
        let last = state.rows.len().checked_sub(1);
        self.cursor.row = match (self.cursor.row, state.collapsed) {
            (Some(row), false) => last.map(|last| row.min(last)),
            _ => None,
        };
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use test_case::test_case;

    use super::{Change, Cursor, GitMark, Row, Scm, Section};

    const WRONG_STOP: &str = "the cursor is not where walking the pane should have put it";
    const SELECTION_LOST: &str = "the cursor must stay on the change it was on";
    const MARK_WRONG: &str = "the explorer must show the worktree's mark";
    const WRONG_SCOPE: &str = "the staging scope does not cover the paths the cursor names";
    const WRONG_COUNT: &str = "the section counts the wrong number of paths";

    fn change(relative: &str, staged: bool, mark: GitMark) -> Change {
        Change {
            path: PathBuf::from("/repo").join(relative),
            relative: relative.to_owned(),
            staged,
            mark,
        }
    }

    fn pane(changes: Vec<Change>) -> Scm {
        let mut scm = Scm {
            changes,
            flat: true,
            ..Scm::default()
        };
        scm.rebuild(None);
        scm
    }

    fn nested() -> Scm {
        let mut scm = pane(vec![
            change("src/a.rs", false, GitMark::Modified),
            change("src/b.rs", false, GitMark::Modified),
            change("top.rs", false, GitMark::Modified),
        ]);
        scm.flat = false;
        scm.rebuild(None);
        scm
    }

    #[test]
    fn each_section_counts_only_its_own_side() {
        let scm = pane(vec![
            change("staged.rs", true, GitMark::Added),
            change("dirty.rs", false, GitMark::Modified),
            change("other.rs", false, GitMark::Modified),
        ]);

        assert_eq!(scm.count(Section::Staged), 1, "{WRONG_COUNT}");
        assert_eq!(scm.count(Section::Unstaged), 2, "{WRONG_COUNT}");
        assert_eq!(scm.count(Section::Graph), 0, "{WRONG_COUNT}");
    }

    #[test]
    fn the_cursor_starts_on_the_first_header() {
        let scm = pane(vec![change("dirty.rs", false, GitMark::Modified)]);
        assert_eq!(scm.cursor(), Cursor::default(), "{WRONG_STOP}");
    }

    #[test]
    fn walking_down_crosses_from_one_section_into_the_next() {
        let mut scm = pane(vec![change("staged.rs", true, GitMark::Added)]);

        scm.move_cursor(1);
        assert_eq!(
            scm.cursor(),
            Cursor {
                section: Section::Staged,
                row: Some(0)
            },
            "{WRONG_STOP}"
        );

        scm.move_cursor(1);
        assert_eq!(
            scm.cursor(),
            Cursor {
                section: Section::Unstaged,
                row: None
            },
            "{WRONG_STOP}"
        );
    }

    #[test]
    fn a_folded_section_offers_its_header_and_nothing_else() {
        let mut scm = pane(vec![change("staged.rs", true, GitMark::Added)]);
        scm.set_collapsed(Section::Staged, true);

        scm.move_cursor(1);

        assert_eq!(
            scm.cursor(),
            Cursor {
                section: Section::Unstaged,
                row: None
            },
            "{WRONG_STOP}"
        );
    }

    #[test]
    fn walking_past_the_end_stops_on_the_last_stop() {
        let mut scm = pane(vec![change("dirty.rs", false, GitMark::Modified)]);
        scm.move_cursor(50);
        assert_eq!(
            scm.cursor(),
            Cursor {
                section: Section::Graph,
                row: None
            },
            "{WRONG_STOP}"
        );
    }

    #[test]
    fn a_click_past_the_end_of_a_section_selects_nothing() {
        let mut scm = pane(vec![change("dirty.rs", false, GitMark::Modified)]);
        let before = scm.cursor();

        scm.select(Section::Unstaged, Some(9));

        assert_eq!(scm.cursor(), before, "{WRONG_STOP}");
    }

    #[test]
    fn a_rebuild_keeps_the_cursor_on_the_change_it_was_on() {
        let mut scm = pane(vec![
            change("a.rs", false, GitMark::Modified),
            change("b.rs", false, GitMark::Modified),
        ]);
        scm.select(Section::Unstaged, Some(1));

        let previous = scm.anchor();
        scm.changes.insert(0, change("new.rs", false, GitMark::Added));
        scm.rebuild(previous);

        assert_eq!(
            scm.selected_change().map(|change| change.relative.as_str()),
            Some("b.rs"),
            "{SELECTION_LOST}"
        );
    }

    #[test]
    fn staging_a_change_leaves_the_cursor_in_the_section_it_was_in() {
        let mut scm = pane(vec![
            change("a.rs", false, GitMark::Modified),
            change("b.rs", false, GitMark::Modified),
        ]);
        scm.select(Section::Unstaged, Some(1));

        // What staging does to the lists, without a repository to do it with.
        let previous = scm.anchor();
        scm.changes[1].staged = true;
        scm.rebuild(previous);

        // Staying put is what lets a run of presses stage a run of files,
        // rather than chasing each one up into the staged section.
        assert_eq!(scm.cursor().section, Section::Unstaged, "{SELECTION_LOST}");
        assert_eq!(
            scm.selected_change().map(|change| change.relative.as_str()),
            Some("a.rs"),
            "{SELECTION_LOST}"
        );
    }

    #[test]
    fn a_refresh_that_emptied_the_section_leaves_the_cursor_on_its_header() {
        let mut scm = pane(vec![change("a.rs", false, GitMark::Modified)]);
        scm.select(Section::Unstaged, Some(0));

        scm.changes.clear();
        scm.rebuild(None);

        assert_eq!(scm.cursor().row, None, "{WRONG_STOP}");
    }

    #[test_case(Section::Staged ; "staged")]
    #[test_case(Section::Unstaged ; "unstaged")]
    fn a_header_covers_every_path_on_its_own_side(section: Section) {
        let mut scm = pane(vec![
            change("staged.rs", true, GitMark::Added),
            change("dirty.rs", false, GitMark::Modified),
        ]);
        scm.select(section, None);

        let expected = match section {
            Section::Staged => vec!["staged.rs"],
            _ => vec!["dirty.rs"],
        };
        assert_eq!(scm.scope(), expected, "{WRONG_SCOPE}");
    }

    #[test]
    fn a_folder_covers_the_paths_under_it_and_no_others() {
        let mut scm = nested();
        scm.select(Section::Unstaged, Some(0));

        assert!(
            matches!(scm.rows(Section::Unstaged)[0], Row::Directory(_)),
            "{WRONG_SCOPE}"
        );
        assert_eq!(scm.scope(), vec!["src/a.rs", "src/b.rs"], "{WRONG_SCOPE}");
    }

    #[test]
    fn a_file_covers_only_itself() {
        let mut scm = nested();
        scm.select(Section::Unstaged, Some(1));
        assert_eq!(scm.scope(), vec!["src/a.rs"], "{WRONG_SCOPE}");
    }

    #[test]
    fn the_graph_has_nothing_to_stage() {
        let mut scm = pane(vec![change("dirty.rs", false, GitMark::Modified)]);
        scm.select(Section::Graph, None);
        assert!(scm.scope().is_empty(), "{WRONG_SCOPE}");
    }

    #[test]
    fn folding_a_folder_hides_its_files_and_keeps_the_cursor_on_it() {
        let mut scm = nested();
        scm.select(Section::Unstaged, Some(0));

        scm.fold();

        assert_eq!(scm.rows(Section::Unstaged).len(), 2, "{WRONG_STOP}");
        assert_eq!(scm.cursor().row, Some(0), "{WRONG_STOP}");
    }

    #[test]
    fn folding_a_file_steps_out_to_the_folder_holding_it() {
        let mut scm = nested();
        scm.select(Section::Unstaged, Some(1));

        scm.fold();

        assert_eq!(scm.cursor().row, Some(0), "{WRONG_STOP}");
    }

    #[test]
    fn folding_a_top_level_file_steps_out_to_the_header() {
        let mut scm = nested();
        scm.select(Section::Unstaged, Some(3));

        scm.fold();

        assert_eq!(scm.cursor().row, None, "{WRONG_STOP}");
    }

    #[test]
    fn folding_a_header_collapses_the_section() {
        let mut scm = pane(vec![change("a.rs", false, GitMark::Modified)]);
        scm.select(Section::Unstaged, None);

        scm.fold();

        assert!(scm.is_collapsed(Section::Unstaged), "{WRONG_STOP}");
    }

    #[test]
    fn switching_to_flat_drops_the_folders() {
        let mut scm = nested();
        assert!(
            matches!(scm.rows(Section::Unstaged)[0], Row::Directory(_)),
            "{WRONG_SHAPE_TREE}"
        );

        scm.toggle_flat();

        assert!(
            scm.rows(Section::Unstaged)
                .iter()
                .all(|row| matches!(row, Row::Change { .. })),
            "{WRONG_SHAPE_TREE}"
        );
    }

    const WRONG_SHAPE_TREE: &str = "the change section is not in the mode it was switched to";

    #[test]
    fn a_stored_layout_shorter_than_the_pane_keeps_the_rest_of_the_defaults() {
        let mut scm = Scm::default();
        let before = scm.height(Section::Graph);

        scm.restore(true, &[(3, true)]);

        assert_eq!(scm.height(Section::Staged), 3, "{WRONG_COUNT}");
        assert!(scm.is_collapsed(Section::Staged), "{WRONG_COUNT}");
        assert_eq!(scm.height(Section::Graph), before, "{WRONG_COUNT}");
        assert!(scm.is_flat(), "{WRONG_COUNT}");
    }

    #[test]
    fn a_stored_height_below_the_floor_is_lifted_to_it() {
        let mut scm = Scm::default();
        scm.restore(false, &[(0, false)]);
        assert_eq!(scm.height(Section::Staged), super::MIN_SECTION_ROWS);
    }

    #[test]
    fn the_scroll_follows_the_cursor_into_the_section_it_is_in() {
        let mut scm = pane((0..10).map(|n| change(&format!("f{n}.rs"), false, GitMark::Modified)).collect());
        scm.select(Section::Unstaged, Some(9));

        scm.clamp_scroll(Section::Unstaged, 4);

        assert_eq!(scm.scroll(Section::Unstaged), 6, "{WRONG_STOP}");
    }

    #[test]
    fn the_scroll_of_another_section_is_left_alone_by_the_cursor() {
        let mut scm = pane((0..10).map(|n| change(&format!("f{n}.rs"), true, GitMark::Added)).collect());
        scm.select(Section::Staged, Some(9));

        scm.clamp_scroll(Section::Unstaged, 4);

        assert_eq!(scm.scroll(Section::Unstaged), 0, "{WRONG_STOP}");
    }

    #[test]
    fn the_worktree_mark_wins_when_a_path_changed_on_both_sides() {
        let scm = pane(vec![
            change("both.rs", true, GitMark::Added),
            change("both.rs", false, GitMark::Modified),
        ]);
        let marks = scm.marks();
        assert_eq!(
            marks(&PathBuf::from("/repo/both.rs")),
            Some(GitMark::Modified),
            "{MARK_WRONG}"
        );
    }
}
