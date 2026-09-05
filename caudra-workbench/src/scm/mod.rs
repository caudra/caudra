//! The Source Control pane: what the repository has to say, and the four
//! things the workbench can do about it.
//!
//! Everything here is synchronous. A `git status` over a working tree is fast
//! enough to run on a keystroke, and a worker would buy latency the pane cannot
//! spend: the list has to be correct the instant it is drawn, because the next
//! key stages whatever the cursor is on.

pub mod diff;
pub mod repo;

use std::path::{Path, PathBuf};

use crate::fs::tree::GitMark;
use repo::{Change, Commit, Repo, ScmError};

const STAGED_HEADING: &str = "Staged Changes";
const UNSTAGED_HEADING: &str = "Changes";
const LOG_HEADING: &str = "Commits";

/// What the pane is listing. The log is a second reading of the same
/// repository rather than a fourth sidebar view: it answers the same question,
/// and it shares the pane's cursor and scroll.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Listing {
    #[default]
    Changes,
    Log,
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

/// A row of the pane. Changes and commits are held by index so the pane keeps
/// one copy of each, and headings are unselectable so the cursor never lands
/// on something that cannot be staged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    Heading(&'static str),
    Change(usize),
    Commit(usize),
}

impl Row {
    const fn is_selectable(self) -> bool {
        !matches!(self, Self::Heading(_))
    }
}

#[derive(Default)]
pub struct Scm {
    repo: Option<Repo>,
    listing: Listing,
    changes: Vec<Change>,
    log: Vec<Commit>,
    rows: Vec<Row>,
    selected: usize,
    scroll: usize,
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
        *self = Self::default();
        if let Ok(repo) = Repo::discover(root) {
            self.repo = Some(repo);
            self.refresh();
        }
    }

    /// Rereads the repository. The log costs a walk, so it is only reread when
    /// the pane is actually showing it.
    pub fn refresh(&mut self) {
        // Read before the lists are replaced: the rows index into them, so a
        // cursor read afterwards would name whatever slid into its slot.
        let previous = self.selected_path();
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
        if self.listing == Listing::Log {
            match repo.log() {
                Ok(log) => self.log = log,
                Err(error) => {
                    self.log.clear();
                    self.error = Some(error.to_string());
                }
            }
        }
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

    pub fn listing(&self) -> Listing {
        self.listing
    }

    pub fn head(&self) -> Option<&str> {
        self.head.as_deref()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn change(&self, index: usize) -> Option<&Change> {
        self.changes.get(index)
    }

    pub fn commit(&self, index: usize) -> Option<&Commit> {
        self.log.get(index)
    }

    pub fn selected_index(&self) -> usize {
        self.selected
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    pub fn toggle_listing(&mut self) {
        self.listing = match self.listing {
            Listing::Changes => Listing::Log,
            Listing::Log => Listing::Changes,
        };
        self.selected = 0;
        self.scroll = 0;
        self.refresh();
    }

    /// Moves by `delta` selectable rows, stepping over the headings between
    /// them so a heading never becomes the cursor's resting place.
    pub fn move_selection(&mut self, delta: isize) {
        let selectable = self.selectable();
        let Some(last) = selectable.len().checked_sub(1) else {
            return;
        };
        let current = selectable
            .iter()
            .position(|index| *index >= self.selected)
            .unwrap_or(last);
        let target = current.saturating_add_signed(delta).min(last);
        self.selected = selectable[target];
    }

    pub fn select_first(&mut self) {
        self.selected = self.selectable().first().copied().unwrap_or_default();
    }

    pub fn select_last(&mut self) {
        self.selected = self.selectable().last().copied().unwrap_or_default();
    }

    /// Lands the cursor on `index` when it is a selectable row, which is what a
    /// click on the pane means.
    pub fn select_index(&mut self, index: usize) {
        if self.rows.get(index).is_some_and(|row| row.is_selectable()) {
            self.selected = index;
        }
    }

    pub fn clamp_scroll(&mut self, viewport: usize) {
        if viewport == 0 {
            return;
        }
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + viewport {
            self.scroll = self.selected + 1 - viewport;
        }
        self.scroll = self.scroll.min(self.rows.len().saturating_sub(viewport));
    }

    pub fn scroll_by(&mut self, delta: isize, viewport: usize) {
        let max = self.rows.len().saturating_sub(viewport);
        self.scroll = self.scroll.saturating_add_signed(delta).min(max);
    }

    pub fn selected_change(&self) -> Option<&Change> {
        match self.rows.get(self.selected)? {
            Row::Change(index) => self.changes.get(*index),
            _ => None,
        }
    }

    /// Stages an unstaged change and unstages a staged one, so one key covers
    /// both directions the way clicking the same gutter icon does.
    pub fn toggle_staged(&mut self) -> Result<(), ScmError> {
        let Some(change) = self.selected_change() else {
            return Ok(());
        };
        let (relative, staged) = (change.relative.clone(), change.staged);
        let Some(repo) = &self.repo else {
            return Ok(());
        };
        match staged {
            true => repo.unstage(&relative)?,
            false => repo.stage(&relative)?,
        }
        self.refresh();
        Ok(())
    }

    /// Throws away the worktree's copy of the selected change, restoring it
    /// from the index. A staged change stays staged: this only undoes what has
    /// not been recorded anywhere yet.
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

    fn selectable(&self) -> Vec<usize> {
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.is_selectable())
            .map(|(index, _)| index)
            .collect()
    }

    fn rebuild(&mut self, previous: Option<PathBuf>) {
        self.rows = match self.listing {
            Listing::Changes => change_rows(&self.changes),
            Listing::Log => log_rows(self.log.len()),
        };
        if let Some(index) = previous.and_then(|path| self.row_of(&path)) {
            self.selected = index;
        }
        self.clamp_selection();
    }

    fn selected_path(&self) -> Option<PathBuf> {
        self.selected_change().map(|change| change.path.clone())
    }

    fn row_of(&self, path: &Path) -> Option<usize> {
        self.rows.iter().position(|row| match row {
            Row::Change(index) => self.changes.get(*index).is_some_and(|c| c.path == path),
            _ => false,
        })
    }

    /// Snaps the cursor onto the nearest selectable row at or after it, so a
    /// refresh that removed rows never leaves it on a heading or past the end.
    fn clamp_selection(&mut self) {
        let selectable = self.selectable();
        self.selected = selectable
            .iter()
            .find(|index| **index >= self.selected)
            .or_else(|| selectable.last())
            .copied()
            .unwrap_or_default();
    }
}

fn change_rows(changes: &[Change]) -> Vec<Row> {
    let mut rows = Vec::with_capacity(changes.len() + 2);
    for (staged, heading) in [(true, STAGED_HEADING), (false, UNSTAGED_HEADING)] {
        let matching = changes
            .iter()
            .enumerate()
            .filter(|(_, change)| change.staged == staged)
            .map(|(index, _)| Row::Change(index));
        let before = rows.len();
        rows.extend(matching);
        if rows.len() > before {
            rows.insert(before, Row::Heading(heading));
        }
    }
    rows
}

fn log_rows(commits: usize) -> Vec<Row> {
    if commits == 0 {
        return Vec::new();
    }
    let mut rows = Vec::with_capacity(commits + 1);
    rows.push(Row::Heading(LOG_HEADING));
    rows.extend((0..commits).map(Row::Commit));
    rows
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use test_case::test_case;

    use super::{
        Change, GitMark, LOG_HEADING, Listing, Row, STAGED_HEADING, Scm, UNSTAGED_HEADING,
        change_rows, log_rows,
    };

    const HEADING_SELECTED: &str = "the cursor must never rest on a heading";
    const HEADING_MISSING: &str = "a populated section must carry its heading";
    const HEADING_SPURIOUS: &str = "an empty section must not carry a heading";
    const SELECTION_LOST: &str = "the cursor must stay on the change it was on";
    const MARK_WRONG: &str = "the explorer must show the worktree's mark";

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
            ..Scm::default()
        };
        scm.rebuild(None);
        scm
    }

    #[test]
    fn both_sections_carry_a_heading_when_both_are_populated() {
        let rows = change_rows(&[
            change("staged.rs", true, GitMark::Added),
            change("dirty.rs", false, GitMark::Modified),
        ]);
        assert_eq!(rows[0], Row::Heading(STAGED_HEADING), "{HEADING_MISSING}");
        assert_eq!(rows[2], Row::Heading(UNSTAGED_HEADING), "{HEADING_MISSING}");
        assert_eq!(rows.len(), 4, "{HEADING_SPURIOUS}");
    }

    #[test]
    fn an_empty_section_contributes_no_heading() {
        let rows = change_rows(&[change("dirty.rs", false, GitMark::Modified)]);
        assert_eq!(rows[0], Row::Heading(UNSTAGED_HEADING), "{HEADING_MISSING}");
        assert_eq!(rows.len(), 2, "{HEADING_SPURIOUS}");
    }

    #[test]
    fn no_changes_render_no_rows() {
        assert!(change_rows(&[]).is_empty(), "{HEADING_SPURIOUS}");
        assert!(log_rows(0).is_empty(), "{HEADING_SPURIOUS}");
    }

    #[test]
    fn the_log_carries_one_heading() {
        let rows = log_rows(3);
        assert_eq!(rows[0], Row::Heading(LOG_HEADING), "{HEADING_MISSING}");
        assert_eq!(rows.len(), 4, "{HEADING_SPURIOUS}");
    }

    #[test]
    fn the_cursor_starts_below_the_first_heading() {
        let scm = pane(vec![change("dirty.rs", false, GitMark::Modified)]);
        assert_eq!(scm.selected_index(), 1, "{HEADING_SELECTED}");
    }

    #[test_case(-5; "far up")]
    #[test_case(-1; "one up")]
    #[test_case(1; "one down")]
    #[test_case(5; "far down")]
    fn moving_the_cursor_never_lands_on_a_heading(delta: isize) {
        let mut scm = pane(vec![
            change("staged.rs", true, GitMark::Added),
            change("dirty.rs", false, GitMark::Modified),
        ]);
        scm.move_selection(delta);
        let row = scm.rows()[scm.selected_index()];
        assert!(matches!(row, Row::Change(_)), "{HEADING_SELECTED}");
    }

    #[test]
    fn stepping_down_skips_the_heading_between_sections() {
        let mut scm = pane(vec![
            change("staged.rs", true, GitMark::Added),
            change("dirty.rs", false, GitMark::Modified),
        ]);
        scm.move_selection(1);
        assert_eq!(
            scm.selected_change().map(|c| c.relative.as_str()),
            Some("dirty.rs"),
            "{HEADING_SELECTED}"
        );
    }

    #[test]
    fn first_and_last_land_on_changes() {
        let mut scm = pane(vec![
            change("staged.rs", true, GitMark::Added),
            change("dirty.rs", false, GitMark::Modified),
        ]);
        scm.select_last();
        assert_eq!(
            scm.selected_change().map(|c| c.relative.as_str()),
            Some("dirty.rs"),
            "{HEADING_SELECTED}"
        );
        scm.select_first();
        assert_eq!(
            scm.selected_change().map(|c| c.relative.as_str()),
            Some("staged.rs"),
            "{HEADING_SELECTED}"
        );
    }

    #[test]
    fn a_rebuild_keeps_the_cursor_on_its_change() {
        let mut scm = pane(vec![
            change("a.rs", false, GitMark::Modified),
            change("b.rs", false, GitMark::Modified),
        ]);
        scm.move_selection(1);
        let previous = scm.selected_path();
        scm.changes
            .insert(0, change("staged.rs", true, GitMark::Added));
        scm.rebuild(previous);
        assert_eq!(
            scm.selected_change().map(|c| c.relative.as_str()),
            Some("b.rs"),
            "{SELECTION_LOST}"
        );
    }

    #[test]
    fn a_rebuild_that_empties_the_pane_resets_the_cursor() {
        let mut scm = pane(vec![change("a.rs", false, GitMark::Modified)]);
        scm.changes.clear();
        scm.rebuild(None);
        assert_eq!(scm.selected_index(), 0, "{HEADING_SELECTED}");
        assert!(scm.selected_change().is_none(), "{HEADING_SELECTED}");
    }

    #[test]
    fn a_heading_cannot_be_clicked_onto() {
        let mut scm = pane(vec![change("a.rs", false, GitMark::Modified)]);
        scm.select_index(0);
        assert_eq!(scm.selected_index(), 1, "{HEADING_SELECTED}");
    }

    #[test]
    fn a_path_changed_on_both_sides_shows_the_worktree_mark() {
        let scm = pane(vec![
            change("a.rs", true, GitMark::Added),
            change("a.rs", false, GitMark::Modified),
        ]);
        let marks = scm.marks();
        assert_eq!(
            marks(&PathBuf::from("/repo/a.rs")),
            Some(GitMark::Modified),
            "{MARK_WRONG}"
        );
        assert_eq!(
            marks(&PathBuf::from("/repo/other.rs")),
            None,
            "{MARK_WRONG}"
        );
    }

    #[test]
    fn the_listing_toggles_without_a_repository() {
        let mut scm = Scm::default();
        scm.toggle_listing();
        assert_eq!(scm.listing(), Listing::Log, "{HEADING_SPURIOUS}");
        scm.toggle_listing();
        assert_eq!(scm.listing(), Listing::Changes, "{HEADING_SPURIOUS}");
    }

    #[test]
    fn scrolling_keeps_the_cursor_in_view() {
        let changes = (0..20)
            .map(|index| change(&format!("file{index}.rs"), false, GitMark::Modified))
            .collect();
        let mut scm = pane(changes);
        scm.select_last();
        scm.clamp_scroll(5);
        let selected = scm.selected_index();
        assert!(
            (scm.scroll()..scm.scroll() + 5).contains(&selected),
            "{SELECTION_LOST}"
        );
    }
}
