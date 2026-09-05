//! A lazily expanded directory tree.
//!
//! Only expanded directories are ever read, and each is read one level deep, so
//! opening the explorer in a monorepo costs one `readdir` rather than a walk of
//! the whole checkout.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

pub(crate) const GIT_DIR: &str = ".git";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Directory,
    File,
}

/// A file's standing with the repository, painted at the end of its row.
/// Source control is what produces them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitMark {
    Modified,
    Added,
    Deleted,
    Untracked,
    Conflicted,
}

impl GitMark {
    pub const fn letter(self) -> &'static str {
        match self {
            Self::Modified => "M",
            Self::Added => "A",
            Self::Deleted => "D",
            Self::Untracked => "U",
            Self::Conflicted => "!",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub path: PathBuf,
    pub name: String,
    pub depth: usize,
    pub kind: EntryKind,
    pub expanded: bool,
    pub git: Option<GitMark>,
    pub agent_touched: bool,
}

impl Row {
    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Directory
    }
}

#[derive(Debug)]
struct Node {
    path: PathBuf,
    name: String,
    kind: EntryKind,
    children: Option<Vec<Node>>,
}

#[derive(Default)]
pub struct Tree {
    root: PathBuf,
    nodes: Vec<Node>,
    expanded: HashSet<PathBuf>,
    show_hidden: bool,
    rows: Vec<Row>,
    selected: usize,
    scroll: usize,
}

impl Tree {
    pub fn new(root: &Path, show_hidden: bool) -> Self {
        let mut tree = Self {
            root: root.to_path_buf(),
            nodes: Vec::new(),
            expanded: HashSet::new(),
            show_hidden,
            rows: Vec::new(),
            selected: 0,
            scroll: 0,
        };
        tree.reload();
        tree
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn selected(&self) -> Option<&Row> {
        self.rows.get(self.selected)
    }

    pub fn selected_index(&self) -> usize {
        self.selected
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    pub fn set_show_hidden(&mut self, show_hidden: bool) {
        if self.show_hidden != show_hidden {
            self.show_hidden = show_hidden;
            self.reload();
        }
    }

    /// Rereads every directory currently expanded, keeping the selection on the
    /// same path when it survived.
    pub fn reload(&mut self) {
        let selected = self.selected().map(|row| row.path.clone());
        self.nodes = read_dir(&self.root, self.show_hidden);
        let expanded: Vec<PathBuf> = self.expanded.iter().cloned().collect();
        for path in expanded {
            self.load_children(&path);
        }
        self.rebuild_rows();
        if let Some(path) = selected {
            self.select_path(&path);
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() - 1;
        self.selected = self.selected.saturating_add_signed(delta).min(last);
    }

    pub fn select_first(&mut self) {
        self.selected = 0;
    }

    pub fn select_last(&mut self) {
        self.selected = self.rows.len().saturating_sub(1);
    }

    pub fn select_index(&mut self, index: usize) {
        if index < self.rows.len() {
            self.selected = index;
        }
    }

    /// Expands every ancestor of `path` and lands the cursor on it, which is
    /// how a quick-open or a search hit reveals its file in the tree.
    pub fn reveal(&mut self, path: &Path) {
        let Ok(relative) = path.strip_prefix(&self.root) else {
            return;
        };
        let mut current = self.root.clone();
        for component in relative.components() {
            current.push(component);
            if current != path {
                self.expanded.insert(current.clone());
                self.load_children(&current);
            }
        }
        self.rebuild_rows();
        self.select_path(path);
    }

    /// Expands or collapses a directory. Reports whether anything moved, so a
    /// caller pressing Enter on a file can fall through to opening it.
    pub fn toggle_selected(&mut self) -> bool {
        let Some(row) = self.rows.get(self.selected) else {
            return false;
        };
        if !row.is_dir() {
            return false;
        }
        let path = row.path.clone();
        if self.expanded.remove(&path) {
            self.collapse_descendants(&path);
        } else {
            self.expanded.insert(path.clone());
            self.load_children(&path);
        }
        self.rebuild_rows();
        self.select_path(&path);
        true
    }

    /// Collapses the selected directory, or jumps to the parent of a file. The
    /// left arrow means "out of here" either way.
    pub fn collapse_or_parent(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        if row.is_dir() && self.expanded.contains(&row.path) {
            self.toggle_selected();
            return;
        }
        let Some(parent) = row.path.parent().map(Path::to_path_buf) else {
            return;
        };
        if parent != self.root {
            self.select_path(&parent);
        }
    }

    pub fn apply_git(&mut self, marks: &dyn Fn(&Path) -> Option<GitMark>) {
        for row in &mut self.rows {
            row.git = marks(&row.path);
        }
    }

    pub fn set_agent_touched(&mut self, touched: &HashSet<PathBuf>) {
        for row in &mut self.rows {
            row.agent_touched = touched.contains(&row.path);
        }
    }

    /// Keeps the cursor inside the window, and the window inside the rows.
    pub fn clamp_scroll(&mut self, viewport: usize) {
        if viewport == 0 {
            return;
        }
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + viewport {
            self.scroll = self.selected + 1 - viewport;
        }
        let max = self.rows.len().saturating_sub(viewport);
        self.scroll = self.scroll.min(max);
    }

    pub fn scroll_by(&mut self, delta: isize, viewport: usize) {
        let max = self.rows.len().saturating_sub(viewport);
        self.scroll = self.scroll.saturating_add_signed(delta).min(max);
    }

    fn select_path(&mut self, path: &Path) {
        if let Some(index) = self.rows.iter().position(|row| row.path == path) {
            self.selected = index;
        } else {
            self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        }
    }

    fn collapse_descendants(&mut self, path: &Path) {
        self.expanded.retain(|open| !open.starts_with(path));
    }

    fn load_children(&mut self, path: &Path) {
        let show_hidden = self.show_hidden;
        let Some(node) = find_node_mut(&mut self.nodes, path) else {
            return;
        };
        if node.kind != EntryKind::Directory || node.children.is_some() {
            return;
        }
        node.children = Some(read_dir(path, show_hidden));
    }

    fn rebuild_rows(&mut self) {
        let mut rows = Vec::with_capacity(self.rows.len().max(16));
        flatten(&self.nodes, 0, &self.expanded, &mut rows);
        self.rows = rows;
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
    }
}

fn flatten(nodes: &[Node], depth: usize, expanded: &HashSet<PathBuf>, out: &mut Vec<Row>) {
    for node in nodes {
        let is_expanded = expanded.contains(&node.path);
        out.push(Row {
            path: node.path.clone(),
            name: node.name.clone(),
            depth,
            kind: node.kind,
            expanded: is_expanded,
            git: None,
            agent_touched: false,
        });
        if is_expanded && let Some(children) = &node.children {
            flatten(children, depth + 1, expanded, out);
        }
    }
}

fn find_node_mut<'a>(nodes: &'a mut [Node], path: &Path) -> Option<&'a mut Node> {
    for node in nodes {
        if node.path == path {
            return Some(node);
        }
        if path.starts_with(&node.path)
            && let Some(children) = node.children.as_mut()
            && let Some(found) = find_node_mut(children, path)
        {
            return Some(found);
        }
    }
    None
}

/// One level only. `.git` is always hidden: it is not source, and walking into
/// it turns the tree into a list of loose objects.
fn read_dir(dir: &Path, show_hidden: bool) -> Vec<Node> {
    let mut nodes: Vec<Node> = WalkBuilder::new(dir)
        .max_depth(Some(1))
        .hidden(!show_hidden)
        .git_ignore(!show_hidden)
        .git_global(!show_hidden)
        .git_exclude(!show_hidden)
        .parents(!show_hidden)
        .filter_entry(|entry| entry.file_name() != GIT_DIR)
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.path() != dir)
        .map(|entry| Node {
            path: entry.path().to_path_buf(),
            name: entry.file_name().to_string_lossy().into_owned(),
            kind: if entry.file_type().is_some_and(|kind| kind.is_dir()) {
                EntryKind::Directory
            } else {
                EntryKind::File
            },
            children: None,
        })
        .collect();
    nodes.sort_by(|a, b| {
        b.kind
            .eq(&EntryKind::Directory)
            .cmp(&a.kind.eq(&EntryKind::Directory))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
    nodes
}

#[cfg(test)]
mod tests {
    use super::{EntryKind, GitMark, Tree};
    use std::collections::HashSet;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    const DIRS_FIRST: &str = "directories must sort above files so the tree reads like a tree";
    const LAZY: &str = "a collapsed directory's children must not appear in the rows";
    const IGNORED_HIDDEN: &str = "an ignored file must stay out of the tree until asked for";
    const GIT_NEVER: &str = "the .git directory is not source and must never be listed";
    const REVEALED: &str = "revealing a path must expand its ancestors and land on it";
    const MARK_MISPLACED: &str = "a mark must land on the row whose path it names, and on no other";

    fn fixture() -> TempDir {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("src/nested")).unwrap();
        fs::create_dir_all(root.join(".git/objects")).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
        fs::write(root.join(".gitignore"), "target\n").unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(root.join("src/nested/deep.rs"), "// deep\n").unwrap();
        tmp
    }

    fn names(tree: &Tree) -> Vec<String> {
        tree.rows().iter().map(|row| row.name.clone()).collect()
    }

    #[test]
    fn the_root_lists_directories_before_files() {
        let tmp = fixture();
        let tree = Tree::new(tmp.path(), false);
        let rows = tree.rows();
        let first_file = rows
            .iter()
            .position(|row| row.kind == EntryKind::File)
            .unwrap();
        assert!(
            rows[..first_file]
                .iter()
                .all(|row| row.kind == EntryKind::Directory),
            "{DIRS_FIRST}"
        );
    }

    #[test]
    fn a_collapsed_directory_hides_its_children() {
        let tmp = fixture();
        let tree = Tree::new(tmp.path(), false);
        assert!(names(&tree).contains(&"src".to_owned()));
        assert!(!names(&tree).contains(&"main.rs".to_owned()), "{LAZY}");
    }

    #[test]
    fn expanding_a_directory_reads_exactly_one_level() {
        let tmp = fixture();
        let mut tree = Tree::new(tmp.path(), false);
        let src = tree.rows().iter().position(|r| r.name == "src").unwrap();
        tree.select_index(src);
        assert!(tree.toggle_selected());

        let listed = names(&tree);
        assert!(listed.contains(&"main.rs".to_owned()));
        assert!(listed.contains(&"nested".to_owned()));
        assert!(!listed.contains(&"deep.rs".to_owned()), "{LAZY}");
    }

    #[test]
    fn collapsing_a_directory_hides_it_again() {
        let tmp = fixture();
        let mut tree = Tree::new(tmp.path(), false);
        let src = tree.rows().iter().position(|r| r.name == "src").unwrap();
        tree.select_index(src);
        tree.toggle_selected();
        tree.toggle_selected();
        assert!(!names(&tree).contains(&"main.rs".to_owned()), "{LAZY}");
    }

    #[test]
    fn an_ignored_directory_appears_only_when_hidden_files_are_shown() {
        let tmp = fixture();
        let tree = Tree::new(tmp.path(), false);
        assert!(
            !names(&tree).contains(&"target".to_owned()),
            "{IGNORED_HIDDEN}"
        );

        let tree = Tree::new(tmp.path(), true);
        assert!(names(&tree).contains(&"target".to_owned()));
    }

    #[test]
    fn the_git_directory_is_never_listed() {
        let tmp = fixture();
        for show_hidden in [false, true] {
            let tree = Tree::new(tmp.path(), show_hidden);
            assert!(!names(&tree).contains(&".git".to_owned()), "{GIT_NEVER}");
        }
    }

    #[test]
    fn revealing_a_nested_file_expands_the_path_to_it() {
        let tmp = fixture();
        let mut tree = Tree::new(tmp.path(), false);
        let deep = tmp.path().join("src/nested/deep.rs");
        tree.reveal(&deep);

        assert_eq!(
            tree.selected().map(|row| row.path.as_path()),
            Some(deep.as_path()),
            "{REVEALED}"
        );
        assert!(names(&tree).contains(&"deep.rs".to_owned()), "{REVEALED}");
    }

    #[test]
    fn revealing_a_path_outside_the_root_changes_nothing() {
        let tmp = fixture();
        let mut tree = Tree::new(tmp.path(), false);
        let before = names(&tree);
        tree.reveal(Path::new("/etc/hosts"));
        assert_eq!(names(&tree), before);
    }

    #[test]
    fn the_selection_stays_inside_the_rows() {
        let tmp = fixture();
        let mut tree = Tree::new(tmp.path(), false);
        tree.move_selection(-10);
        assert_eq!(tree.selected_index(), 0);
        tree.move_selection(1000);
        assert_eq!(tree.selected_index(), tree.rows().len() - 1);
    }

    #[test]
    fn the_window_follows_the_cursor() {
        let tmp = fixture();
        let mut tree = Tree::new(tmp.path(), true);
        tree.select_last();
        tree.clamp_scroll(1);
        assert_eq!(tree.scroll(), tree.rows().len() - 1);
        tree.select_first();
        tree.clamp_scroll(1);
        assert_eq!(tree.scroll(), 0);
    }

    #[test]
    fn the_window_scrolls_without_moving_the_cursor() {
        let tmp = fixture();
        let mut tree = Tree::new(tmp.path(), true);
        let selected = tree.selected_index();

        tree.scroll_by(1, 1);

        assert_eq!(tree.scroll(), 1);
        assert_eq!(tree.selected_index(), selected);
    }

    #[test]
    fn git_marks_follow_the_paths_they_name() {
        let tmp = fixture();
        let target = tmp.path().join("Cargo.toml");
        let mut tree = Tree::new(tmp.path(), false);

        tree.apply_git(&|path| (path == target).then_some(GitMark::Modified));

        let marked: Vec<&str> = tree
            .rows()
            .iter()
            .filter(|row| row.git.is_some())
            .map(|row| row.name.as_str())
            .collect();
        assert_eq!(marked, vec!["Cargo.toml"], "{MARK_MISPLACED}");
    }

    #[test]
    fn every_git_mark_has_its_own_letter() {
        let letters: HashSet<&str> = [
            GitMark::Modified,
            GitMark::Added,
            GitMark::Deleted,
            GitMark::Untracked,
            GitMark::Conflicted,
        ]
        .iter()
        .map(|mark| mark.letter())
        .collect();

        assert_eq!(letters.len(), 5, "two marks would be indistinguishable");
    }

    #[test]
    fn agent_marks_replace_the_previous_set() {
        let tmp = fixture();
        let mut tree = Tree::new(tmp.path(), false);
        let touched = HashSet::from([tmp.path().join("Cargo.toml")]);

        tree.set_agent_touched(&touched);
        assert_eq!(
            tree.rows().iter().filter(|row| row.agent_touched).count(),
            1,
            "{MARK_MISPLACED}"
        );

        tree.set_agent_touched(&HashSet::new());
        assert!(
            tree.rows().iter().all(|row| !row.agent_touched),
            "{MARK_MISPLACED}"
        );
    }
}
