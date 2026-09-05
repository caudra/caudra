//! Opening a file by typing part of its path.
//!
//! The file list is walked once when the palette opens and matched
//! synchronously on every keystroke, which is affordable because the walk is
//! capped and the match is a scan of short strings. Anything larger than the
//! cap is a repository the tree is the better way through anyway.

use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::{Config, Matcher, Utf32Str};

const MAX_FILES: usize = 20_000;
const MAX_MATCHES: usize = 200;

pub struct QuickOpen {
    open: bool,
    query: String,
    /// Paths relative to the root, which is what is matched and shown.
    files: Vec<String>,
    matches: Vec<usize>,
    selected: usize,
    scroll: usize,
    matcher: Matcher,
    scratch: Vec<char>,
}

impl Default for QuickOpen {
    fn default() -> Self {
        Self {
            open: false,
            query: String::new(),
            files: Vec::new(),
            matches: Vec::new(),
            selected: 0,
            scroll: 0,
            matcher: Matcher::new(Config::DEFAULT.match_paths()),
            scratch: Vec::new(),
        }
    }
}

impl QuickOpen {
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn open(&mut self, root: &Path, show_hidden: bool) {
        self.files = walk(root, show_hidden);
        self.query.clear();
        self.open = true;
        self.rescan();
    }

    pub fn close(&mut self) {
        self.open = false;
        self.files = Vec::new();
        self.matches = Vec::new();
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

    /// An empty query lists the files as walked, so the palette is useful
    /// before anything is typed.
    fn rescan(&mut self) {
        self.selected = 0;
        self.scroll = 0;
        self.matches.clear();
        if self.query.is_empty() {
            self.matches.extend(0..self.files.len().min(MAX_MATCHES));
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

    #[test]
    fn directories_are_not_offered() {
        let (_tmp, palette) = opened();
        assert!(!rows(&palette).iter().any(|row| row == "src"), "{NOT_FOUND}");
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
            rows(&palette).first().is_some_and(|row| row.ends_with("main.rs")),
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
