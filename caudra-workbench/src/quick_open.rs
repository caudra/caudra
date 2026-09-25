//! Opening a file by typing part of its path.
//!
//! The file list is walked once when the palette opens and matched
//! synchronously on every keystroke, which is affordable because the walk is
//! capped and the match is a scan of short strings. Anything larger than the
//! cap is a repository the tree is the better way through anyway.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use ignore::WalkBuilder;
use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::{Config, Matcher, Utf32Str};

use crate::fs::backend::{ResourceEntry, WorkbenchPath};

const MAX_FILES: usize = 20_000;
const MAX_MATCHES: usize = 200;

pub struct QuickOpen {
    open: bool,
    query: String,
    /// Paths relative to the root, which is what is matched and shown.
    files: Vec<String>,
    /// Paths to list first while nothing has been typed, which is the open
    /// tabs. Held across opens because it is set on the way in.
    priority: Vec<String>,
    matches: Vec<usize>,
    selected: usize,
    scroll: usize,
    matcher: Matcher,
    scratch: Vec<char>,
    remote: HashMap<String, ResourceEntry>,
}

impl Default for QuickOpen {
    fn default() -> Self {
        Self {
            open: false,
            query: String::new(),
            files: Vec::new(),
            priority: Vec::new(),
            matches: Vec::new(),
            selected: 0,
            scroll: 0,
            matcher: Matcher::new(Config::DEFAULT.match_paths()),
            scratch: Vec::new(),
            remote: HashMap::new(),
        }
    }
}

impl QuickOpen {
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Walks the project only when it has nothing to show, so reopening the
    /// palette costs a match rather than a full crawl of the tree. What makes
    /// the list stale is [`QuickOpen::invalidate`].
    pub fn open(&mut self, root: &Path, show_hidden: bool) {
        self.remote.clear();
        let started = Instant::now();
        let walked = self.files.is_empty();
        if walked {
            self.files = walk(root, show_hidden);
        }
        let walk_ms = started.elapsed().as_millis() as u64;
        let rescan_start = Instant::now();
        self.query.clear();
        self.open = true;
        self.rescan();
        tracing::info!(
            walked,
            walk_ms,
            rescan_ms = rescan_start.elapsed().as_millis() as u64,
            files = self.files.len(),
            "workbench palette opened"
        );
    }

    pub fn open_remote(&mut self, entries: Vec<ResourceEntry>) {
        self.open = true;
        self.query.clear();
        self.files.clear();
        self.remote.clear();
        for entry in entries {
            if entry.kind != caudra_workspace::ResourceKind::File {
                continue;
            }
            let path = entry.path.display();
            self.files.push(path.clone());
            self.remote.insert(path, entry);
        }
        self.files.sort();
        self.rescan();
    }

    pub fn close(&mut self) {
        self.open = false;
        self.matches = Vec::new();
    }

    /// Throws the walk away, so the next open pays for a fresh one.
    pub fn invalidate(&mut self) {
        self.files = Vec::new();
    }

    /// The paths to offer ahead of the rest before anything is typed.
    pub fn set_priority(&mut self, priority: Vec<String>) {
        self.priority = priority;
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn rows(&self) -> impl Iterator<Item = &str> {
        self.matches.iter().map(|index| self.files[*index].as_str())
    }

    pub fn len(&self) -> usize {
        self.matches.len()
    }

    pub fn selected_index(&self) -> usize {
        self.selected
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    pub fn selected(&self, root: &Path) -> Option<PathBuf> {
        let index = *self.matches.get(self.selected)?;
        Some(root.join(&self.files[index]))
    }

    pub fn selected_remote(&self) -> Option<ResourceEntry> {
        let index = *self.matches.get(self.selected)?;
        self.remote.get(&self.files[index]).cloned()
    }

    pub fn update_remote_entries(&mut self, entries: &[ResourceEntry], removed: &[WorkbenchPath]) {
        if !self.open {
            return;
        }
        for path in removed {
            self.remote.remove(&path.display());
        }
        for entry in entries {
            let path = entry.path.display();
            if entry.kind == caudra_workspace::ResourceKind::File {
                self.remote.insert(path, entry.clone());
            } else {
                self.remote.remove(&path);
            }
        }
        self.files = self.remote.keys().cloned().collect();
        self.files.sort();
        self.rescan();
    }

    pub fn set_query(&mut self, query: String) {
        self.query = query;
        self.rescan();
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.matches.is_empty() {
            return;
        }
        let last = self.matches.len() - 1;
        self.selected = self.selected.saturating_add_signed(delta).min(last);
    }

    pub fn select_first(&mut self) {
        self.selected = 0;
    }

    pub fn select_last(&mut self) {
        self.selected = self.matches.len().saturating_sub(1);
    }

    pub fn select_index(&mut self, index: usize) {
        if self.matches.is_empty() {
            return;
        }
        self.selected = index.min(self.matches.len() - 1);
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
        self.scroll = self.scroll.min(self.matches.len().saturating_sub(viewport));
    }

    pub fn set_scroll(&mut self, top: usize, viewport: usize) {
        self.scroll = top.min(self.matches.len().saturating_sub(viewport));
    }

    /// An empty query lists the priority paths and then the rest as walked, so
    /// the palette is useful before anything is typed.
    fn rescan(&mut self) {
        self.selected = 0;
        self.scroll = 0;
        self.matches.clear();
        if self.query.is_empty() {
            let promoted: Vec<usize> = self
                .priority
                .iter()
                .filter_map(|wanted| self.files.iter().position(|file| file == wanted))
                .collect();
            self.matches.extend(promoted.iter().copied());
            self.matches
                .extend((0..self.files.len()).filter(|index| !promoted.contains(index)));
            self.matches.truncate(MAX_MATCHES);
            return;
        }

        let pattern = Pattern::parse(&self.query, CaseMatching::Smart, Normalization::Smart);
        let mut scored: Vec<(u32, usize)> = Vec::new();
        for (index, path) in self.files.iter().enumerate() {
            let haystack = Utf32Str::new(path, &mut self.scratch);
            if let Some(score) = pattern.score(haystack, &mut self.matcher) {
                scored.push((score, index));
            }
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        self.matches
            .extend(scored.iter().take(MAX_MATCHES).map(|(_, index)| *index));
    }
}

fn walk(root: &Path, show_hidden: bool) -> Vec<String> {
    WalkBuilder::new(root)
        .hidden(!show_hidden)
        .git_ignore(!show_hidden)
        .git_global(!show_hidden)
        .git_exclude(!show_hidden)
        .parents(!show_hidden)
        .filter_entry(|entry| entry.file_name() != crate::fs::tree::GIT_DIR)
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        .filter_map(|entry| {
            let path = entry.path().strip_prefix(root).ok()?;
            Some(path.to_string_lossy().into_owned())
        })
        .take(MAX_FILES)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{MAX_MATCHES, QuickOpen};
    use std::fs;
    use tempfile::TempDir;

    const NOT_FOUND: &str = "the file typed for is not among the matches";
    const WRONG_ORDER: &str = "the closest match must come first";
    const WRONG_END: &str = "the selection did not land on the end of the list it was sent to";
    const STALE: &str = "the palette is not offering what the project holds";

    fn fixture() -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("src/nested")).unwrap();
        fs::write(tmp.path().join("Cargo.toml"), "").unwrap();
        fs::write(tmp.path().join("src/main.rs"), "").unwrap();
        fs::write(tmp.path().join("src/nested/deep.rs"), "").unwrap();
        tmp
    }

    fn opened() -> (TempDir, QuickOpen) {
        let tmp = fixture();
        let mut palette = QuickOpen::default();
        palette.open(tmp.path(), false);
        (tmp, palette)
    }

    fn rows(palette: &QuickOpen) -> Vec<String> {
        palette.rows().map(str::to_owned).collect()
    }

    #[test]
    fn an_empty_query_lists_every_file() {
        let (_tmp, palette) = opened();
        assert_eq!(palette.len(), 3, "{NOT_FOUND}");
        assert!(palette.len() <= MAX_MATCHES);
    }

    /// The walk is what makes opening the palette expensive, so it survives a
    /// close and only a deliberate invalidation pays for it again.
    #[test]
    fn reopening_does_not_rewalk_until_the_project_is_said_to_have_moved() {
        let (tmp, mut palette) = opened();
        palette.close();
        fs::write(tmp.path().join("late.rs"), "").unwrap();

        palette.open(tmp.path(), false);
        assert!(
            !rows(&palette).iter().any(|row| row == "late.rs"),
            "{STALE}"
        );

        palette.close();
        palette.invalidate();
        palette.open(tmp.path(), false);
        assert!(rows(&palette).iter().any(|row| row == "late.rs"), "{STALE}");
    }

    #[test]
    fn an_empty_query_lists_the_priority_paths_first() {
        let (tmp, mut palette) = opened();
        palette.close();
        palette.set_priority(vec!["src/nested/deep.rs".to_owned()]);
        palette.open(tmp.path(), false);

        let found = rows(&palette);
        assert_eq!(found[0], "src/nested/deep.rs", "{WRONG_ORDER}");
        assert_eq!(found.len(), 3, "{WRONG_ORDER}");
    }

    /// A path the walk never found cannot be promoted, and must not push the
    /// rest of the list down a row either.
    #[test]
    fn a_priority_path_that_is_gone_is_left_out() {
        let (tmp, mut palette) = opened();
        palette.close();
        palette.set_priority(vec!["deleted.rs".to_owned()]);
        palette.open(tmp.path(), false);

        assert_eq!(rows(&palette).len(), 3, "{WRONG_ORDER}");
    }

    #[test]
    fn home_and_end_reach_both_ends_of_the_matches() {
        let (_tmp, mut palette) = opened();

        palette.select_last();
        assert_eq!(palette.selected_index(), palette.len() - 1, "{WRONG_END}");
        palette.select_first();
        assert_eq!(palette.selected_index(), 0, "{WRONG_END}");
    }

    /// Both ends of nothing are the same place, and neither may reach for a
    /// row that is not there.
    #[test]
    fn home_and_end_hold_still_with_nothing_to_select() {
        let (_tmp, mut palette) = opened();
        palette.set_query("nosuchfile".to_owned());

        palette.select_last();
        assert_eq!(palette.selected_index(), 0, "{WRONG_END}");
        palette.select_first();
        assert_eq!(palette.selected_index(), 0, "{WRONG_END}");
    }

    #[test]
    fn directories_are_not_offered() {
        let (_tmp, palette) = opened();
        assert!(
            !rows(&palette).iter().any(|row| row == "src"),
            "{NOT_FOUND}"
        );
    }

    #[test]
    fn a_query_narrows_and_ranks_the_matches() {
        let (_tmp, mut palette) = opened();
        palette.set_query("deep".to_owned());

        let found = rows(&palette);
        assert_eq!(found.len(), 1, "{NOT_FOUND}");
        assert!(found[0].ends_with("deep.rs"), "{NOT_FOUND}");
    }

    #[test]
    fn a_path_fragment_matches_across_separators() {
        let (_tmp, mut palette) = opened();
        palette.set_query("srmain".to_owned());

        assert!(
            rows(&palette)
                .first()
                .is_some_and(|row| row.ends_with("main.rs")),
            "{WRONG_ORDER}"
        );
    }

    #[test]
    fn the_selection_resolves_to_a_path_under_the_root() {
        let (tmp, mut palette) = opened();
        palette.set_query("main".to_owned());

        assert_eq!(
            palette.selected(tmp.path()),
            Some(tmp.path().join("src/main.rs")),
            "{NOT_FOUND}"
        );
    }

    #[test]
    fn a_query_that_matches_nothing_leaves_no_selection() {
        let (tmp, mut palette) = opened();
        palette.set_query("zzzz".to_owned());

        assert_eq!(palette.len(), 0);
        assert_eq!(palette.selected(tmp.path()), None);
    }

    #[test]
    fn the_selection_and_window_stay_inside_the_matches() {
        let (_tmp, mut palette) = opened();
        palette.move_selection(10);
        assert_eq!(palette.selected_index(), 2);

        palette.clamp_scroll(1);
        assert_eq!(palette.scroll(), 2);

        palette.move_selection(-10);
        palette.clamp_scroll(1);
        assert_eq!(palette.scroll(), 0);
    }

    #[test]
    fn closing_drops_the_walk_so_it_is_fresh_next_time() {
        let (_tmp, mut palette) = opened();
        palette.close();

        assert!(!palette.is_open());
        assert_eq!(palette.len(), 0);
    }
}
