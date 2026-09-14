//! A lazily expanded directory tree.
//!
//! Only expanded directories are ever read, and each is read one level deep, so
//! opening the explorer in a monorepo costs one `readdir` rather than a walk of
//! the whole checkout.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use caudra_workspace::WorkspacePath;
use ignore::{DirEntry, WalkBuilder};

use super::backend::{ResourceEntry, WorkbenchPath};

pub(crate) const GIT_DIR: &str = ".git";
/// What joins the names of folders drawn on one row. A label rather than a
/// path, so it reads the same wherever it is drawn.
const CHAIN: char = '/';

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

    /// How loudly a mark speaks for the folder holding it. A closed folder
    /// wears the loudest mark under it, so one that hides a conflict never
    /// reads as merely modified.
    pub const fn rank(self) -> u8 {
        match self {
            Self::Untracked => 0,
            Self::Added => 1,
            Self::Modified => 2,
            Self::Deleted => 3,
            Self::Conflicted => 4,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub path: WorkbenchPath,
    pub resource: Option<ResourceEntry>,
    pub name: String,
    pub depth: usize,
    pub kind: EntryKind,
    pub expanded: bool,
    pub git: Option<GitMark>,
    pub agent_touched: bool,
    /// Whether the repository ignores this path or something above it. Listed
    /// all the same, and drawn back so it reads as scenery.
    pub ignored: bool,
}

impl Row {
    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Directory
    }
}

#[derive(Debug)]
struct Node {
    path: WorkbenchPath,
    resource: Option<ResourceEntry>,
    name: String,
    kind: EntryKind,
    ignored: bool,
    children: Option<Vec<Node>>,
}

pub struct Tree {
    root: WorkbenchPath,
    nodes: Vec<Node>,
    expanded: HashSet<WorkbenchPath>,
    show_hidden: bool,
    rows: Vec<Row>,
    selected: usize,
    scroll: usize,
}

impl Default for Tree {
    fn default() -> Self {
        Self {
            root: WorkbenchPath::Local(PathBuf::new()),
            nodes: Vec::new(),
            expanded: HashSet::new(),
            show_hidden: false,
            rows: Vec::new(),
            selected: 0,
            scroll: 0,
        }
    }
}

impl Tree {
    pub fn new(root: &Path, show_hidden: bool) -> Self {
        let mut tree = Self {
            root: WorkbenchPath::Local(root.to_path_buf()),
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

    pub fn remote(root: WorkbenchPath, show_hidden: bool) -> Self {
        Self {
            root,
            show_hidden,
            ..Self::default()
        }
    }

    pub fn replace_remote(&mut self, entries: Vec<ResourceEntry>) {
        let selected = self.selected().map(|row| row.path.clone());
        self.nodes = remote_children(&self.root, &entries);
        self.expanded.retain(|path| {
            entries.iter().any(|entry| {
                entry.path == *path && entry.kind == caudra_workspace::ResourceKind::Directory
            })
        });
        self.rebuild_rows();
        if let Some(path) = selected {
            self.select_workbench_path(&path);
        }
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
            if self.root.local().is_some() {
                self.reload();
            }
        }
    }

    /// Rereads every directory currently expanded, keeping the selection on the
    /// same path when it survived.
    pub fn reload(&mut self) {
        let Some(root) = self.root.local().map(Path::to_path_buf) else {
            return;
        };
        let selected = self.selected().map(|row| row.path.clone());
        self.nodes = read_dir(&root, self.show_hidden);
        let expanded: Vec<WorkbenchPath> = self.expanded.iter().cloned().collect();
        for path in expanded {
            self.load_children(&path);
        }
        self.rebuild_rows();
        if let Some(path) = selected {
            self.select_workbench_path(&path);
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
        self.reveal_workbench_path(&WorkbenchPath::Local(path.to_path_buf()));
    }

    pub fn reveal_workbench_path(&mut self, path: &WorkbenchPath) {
        let components = match (path, &self.root) {
            (WorkbenchPath::Local(path), WorkbenchPath::Local(root)) => {
                let Ok(relative) = path.strip_prefix(root) else {
                    return;
                };
                relative
                    .components()
                    .map(|component| component.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
            }
            (WorkbenchPath::Remote(path), WorkbenchPath::Remote(root)) => {
                let relative = if root.is_root() {
                    path.as_str()
                } else {
                    let Some(relative) = path
                        .as_str()
                        .strip_prefix(root.as_str())
                        .and_then(|path| path.strip_prefix('/'))
                    else {
                        return;
                    };
                    relative
                };
                relative.split('/').map(str::to_owned).collect()
            }
            _ => return,
        };
        let mut current = self.root.clone();
        for component in components {
            let Ok(joined) = current.join(&component) else {
                return;
            };
            current = joined;
            if &current != path {
                self.expanded.insert(current.clone());
                self.load_children(&current);
            }
        }
        self.rebuild_rows();
        self.select_workbench_path(path);
    }

    pub fn resource(&self, path: &WorkbenchPath) -> Option<&ResourceEntry> {
        find_node(&self.nodes, path)?.resource.as_ref()
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
        self.select_workbench_path(&path);
        true
    }

    /// Folds the tree back to its top level. The cursor rides up to the
    /// nearest ancestor still listed, so it stays on the branch it was in
    /// rather than landing wherever the shorter list happens to reach.
    pub fn collapse_all(&mut self) {
        let selected = self.selected().map(|row| row.path.clone());
        self.expanded.clear();
        self.rebuild_rows();
        let Some(path) = selected else {
            return;
        };
        let mut showing = Some(path);
        while let Some(candidate) = showing {
            if self.rows.iter().any(|row| row.path == candidate) {
                self.select_workbench_path(&candidate);
                break;
            }
            showing = candidate.parent();
        }
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
        let Some(parent) = row.path.parent() else {
            return;
        };
        if parent != self.root {
            self.select_workbench_path(&parent);
        }
    }

    /// Marks every row: a file with its own standing, a closed folder with
    /// whatever `under` finds below it, and an open folder with nothing at
    /// all, since the rows it just revealed already say it.
    pub fn apply_git(
        &mut self,
        marks: &dyn Fn(&Path) -> Option<GitMark>,
        under: &dyn Fn(&Path) -> Option<GitMark>,
    ) {
        for row in &mut self.rows {
            let Some(path) = row.path.local() else {
                row.git = None;
                continue;
            };
            row.git = match (row.is_dir(), row.expanded) {
                (true, true) => None,
                (true, false) => under(path),
                (false, _) => marks(path),
            };
        }
    }

    pub fn apply_remote_git(
        &mut self,
        marks: &dyn Fn(&WorkspacePath) -> Option<GitMark>,
        under: &dyn Fn(&WorkspacePath) -> Option<GitMark>,
    ) {
        for row in &mut self.rows {
            let Some(path) = row.path.remote() else {
                row.git = None;
                continue;
            };
            row.git = match (row.is_dir(), row.expanded) {
                (true, true) => None,
                (true, false) => under(path),
                (false, _) => marks(path),
            };
        }
    }

    pub fn set_agent_touched(&mut self, touched: &HashSet<PathBuf>) {
        for row in &mut self.rows {
            row.agent_touched = row.path.local().is_some_and(|path| touched.contains(path));
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

    pub fn set_scroll(&mut self, top: usize, viewport: usize) {
        self.scroll = top.min(self.rows.len().saturating_sub(viewport));
    }

    fn select_workbench_path(&mut self, path: &WorkbenchPath) {
        if let Some(index) = self.rows.iter().position(|row| &row.path == path) {
            self.selected = index;
        } else {
            self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        }
    }

    fn collapse_descendants(&mut self, path: &WorkbenchPath) {
        self.expanded.retain(|open| !open.starts_with(path));
    }

    fn load_children(&mut self, path: &WorkbenchPath) {
        let Some(local_path) = path.local() else {
            return;
        };
        let show_hidden = self.show_hidden;
        let Some(node) = find_node_mut(&mut self.nodes, path) else {
            return;
        };
        if node.kind != EntryKind::Directory || node.children.is_some() {
            return;
        }
        node.children = Some(read_dir(local_path, show_hidden));
    }

    fn rebuild_rows(&mut self) {
        let mut rows = Vec::with_capacity(self.rows.len().max(16));
        flatten(&self.nodes, 0, false, &self.expanded, &mut rows);
        self.rows = rows;
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
    }
}

/// `ignored` says whether an ancestor was ignored, which everything below it
/// inherits: git stops at the folder it was told to ignore and says nothing
/// about the contents, so the tree has to carry that down itself.
fn flatten(
    nodes: &[Node],
    depth: usize,
    ignored: bool,
    expanded: &HashSet<WorkbenchPath>,
    out: &mut Vec<Row>,
) {
    for node in nodes {
        let is_expanded = expanded.contains(&node.path);
        let ignored = ignored || node.ignored;
        out.push(Row {
            path: node.path.clone(),
            resource: node.resource.clone(),
            name: node.name.clone(),
            depth,
            kind: node.kind,
            expanded: is_expanded,
            git: None,
            agent_touched: false,
            ignored,
        });
        if is_expanded && let Some(children) = &node.children {
            flatten(children, depth + 1, ignored, expanded, out);
        }
    }
}

fn find_node_mut<'a>(nodes: &'a mut [Node], path: &WorkbenchPath) -> Option<&'a mut Node> {
    for node in nodes {
        if &node.path == path {
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

fn find_node<'a>(nodes: &'a [Node], path: &WorkbenchPath) -> Option<&'a Node> {
    for node in nodes {
        if &node.path == path {
            return Some(node);
        }
        if path.starts_with(&node.path)
            && let Some(children) = node.children.as_ref()
            && let Some(found) = find_node(children, path)
        {
            return Some(found);
        }
    }
    None
}

/// The entries of `dir` alone, with `.git` always left out: it is not source,
/// and walking into it turns the tree into a list of loose objects. `git` asks
/// for the repository's ignore rules to be applied.
fn walk(dir: &Path, show_hidden: bool, git: bool) -> impl Iterator<Item = DirEntry> {
    let dir = dir.to_path_buf();
    WalkBuilder::new(&dir)
        .max_depth(Some(1))
        .hidden(!show_hidden)
        .git_ignore(git)
        .git_global(git)
        .git_exclude(git)
        .parents(git)
        .filter_entry(|entry| entry.file_name() != GIT_DIR)
        .build()
        .filter_map(Result::ok)
        .filter(move |entry| entry.path() != dir)
}

fn node_of(entry: &DirEntry, ignored: bool) -> Node {
    Node {
        ignored,
        path: WorkbenchPath::Local(entry.path().to_path_buf()),
        resource: None,
        name: entry.file_name().to_string_lossy().into_owned(),
        kind: match entry.file_type().is_some_and(|kind| kind.is_dir()) {
            true => EntryKind::Directory,
            false => EntryKind::File,
        },
        children: None,
    }
}

/// A folder holding nothing but one folder is drawn as a single row carrying
/// both names, the way VS Code lists `src/main/java`. The row stands for the
/// end of the chain, since that is the folder it opens, and it is ignored if
/// any step of the chain was.
fn compact(node: Node, show_hidden: bool) -> Node {
    if node.kind != EntryKind::Directory {
        return node;
    }
    let Some(path) = node.path.local() else {
        return node;
    };
    let Some(only) = only_child(path, show_hidden) else {
        return node;
    };
    if only.kind != EntryKind::Directory {
        return node;
    }
    let deeper = compact(only, show_hidden);
    Node {
        name: format!("{}{CHAIN}{}", node.name, deeper.name),
        ignored: node.ignored || deeper.ignored,
        ..deeper
    }
}

/// The one entry `dir` holds, if it holds exactly one. Two entries are all it
/// takes to answer that, so the walk stops there rather than reading a folder
/// out just to count it.
fn only_child(dir: &Path, show_hidden: bool) -> Option<Node> {
    let mut entries = walk(dir, show_hidden, false);
    let entry = entries.next()?;
    if entries.next().is_some() {
        return None;
    }
    let ignored = walk(dir, show_hidden, true).next().is_none();
    Some(node_of(&entry, ignored))
}

/// One level only. An ignored path is listed like any other and marked rather
/// than left out: a build directory nobody tracks is still somewhere you look,
/// and a tree that silently drops it cannot say why it is not there.
fn read_dir(dir: &Path, show_hidden: bool) -> Vec<Node> {
    let tracked: HashSet<PathBuf> = walk(dir, show_hidden, true)
        .map(|entry| entry.path().to_path_buf())
        .collect();
    let mut nodes: Vec<Node> = walk(dir, show_hidden, false)
        .map(|entry| {
            compact(
                node_of(&entry, !tracked.contains(entry.path())),
                show_hidden,
            )
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

fn remote_children(parent: &WorkbenchPath, entries: &[ResourceEntry]) -> Vec<Node> {
    let mut nodes = entries
        .iter()
        .filter(|entry| entry.path.parent().as_ref() == Some(parent))
        .map(|entry| {
            let kind = match entry.kind {
                caudra_workspace::ResourceKind::Directory => EntryKind::Directory,
                _ => EntryKind::File,
            };
            Node {
                path: entry.path.clone(),
                resource: Some(entry.clone()),
                name: entry.path.file_name(),
                kind,
                ignored: false,
                children: (kind == EntryKind::Directory)
                    .then(|| remote_children(&entry.path, entries)),
            }
        })
        .collect::<Vec<_>>();
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
    use super::{EntryKind, GitMark, Row, Tree};
    use std::collections::HashSet;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;
    use test_case::test_case;

    const DIRS_FIRST: &str = "directories must sort above files so the tree reads like a tree";
    const LAZY: &str = "a collapsed directory's children must not appear in the rows";
    const IGNORED_UNMARKED: &str = "the tree disagrees with git about what is ignored";
    const DOTFILE_SHOWN: &str = "a dotfile must wait for hidden files to be asked for";
    const GIT_NEVER: &str = "the .git directory is not source and must never be listed";
    const REVEALED: &str = "revealing a path must expand its ancestors and land on it";
    const MARK_MISPLACED: &str = "a mark must land on the row whose path it names, and on no other";
    const FOLDER_MUTE: &str = "a closed folder must say that something under it changed";
    const FOLDER_SHOUTS: &str = "an open folder must leave the marks to the rows it revealed";
    const RANK_TIED: &str = "two marks would fight over the same folder";
    const STILL_UNFOLDED: &str = "folding the tree must leave nothing but its top level";
    const CHAIN_SPLIT: &str = "folders holding nothing but one another must share a row";
    const CHAIN_SHUT: &str = "opening a shared row must open the last folder on it";
    const CHAIN_GREEDY: &str = "a folder with more than one way on must keep its own row";
    const CURSOR_ADRIFT: &str = "the cursor must ride up to the folder that held its row";
    const LETTER_TIED: &str = "two marks would be indistinguishable";
    const MARKS: [GitMark; 5] = [
        GitMark::Modified,
        GitMark::Added,
        GitMark::Deleted,
        GitMark::Untracked,
        GitMark::Conflicted,
    ];

    fn fixture() -> TempDir {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("src/nested")).unwrap();
        fs::create_dir_all(root.join(".git/objects")).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
        fs::write(root.join(".gitignore"), "target\n").unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("target/build.log"), "log\n").unwrap();
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

    fn row<'a>(tree: &'a Tree, name: &str) -> &'a Row {
        tree.rows()
            .iter()
            .find(|row| row.name == name)
            .expect("a row the fixture created")
    }

    #[test_case("target", true ; "the directory the gitignore names")]
    #[test_case("src", false ; "a directory it does not")]
    #[test_case("Cargo.toml", false ; "a tracked file")]
    fn a_row_says_whether_git_ignores_it(name: &str, expected: bool) {
        let tmp = fixture();
        let tree = Tree::new(tmp.path(), false);

        assert_eq!(row(&tree, name).ignored, expected, "{IGNORED_UNMARKED}");
    }

    /// Git names the folder and says nothing about its contents, so the tree
    /// carries the answer down itself.
    #[test]
    fn everything_under_an_ignored_directory_is_ignored_too() {
        let tmp = fixture();
        let mut tree = Tree::new(tmp.path(), false);
        let target = tree.rows().iter().position(|r| r.name == "target").unwrap();
        tree.select_index(target);
        tree.toggle_selected();

        assert!(row(&tree, "build.log").ignored, "{IGNORED_UNMARKED}");
    }

    #[test]
    fn a_dotfile_still_waits_to_be_asked_for() {
        let tmp = fixture();
        assert!(
            !names(&Tree::new(tmp.path(), false)).contains(&".gitignore".to_owned()),
            "{DOTFILE_SHOWN}"
        );
        assert!(names(&Tree::new(tmp.path(), true)).contains(&".gitignore".to_owned()));
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
            tree.selected().and_then(|row| row.path.local()),
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

        tree.apply_git(
            &|path| (path == target).then_some(GitMark::Modified),
            &|_| None,
        );

        let marked: Vec<&str> = tree
            .rows()
            .iter()
            .filter(|row| row.git.is_some())
            .map(|row| row.name.as_str())
            .collect();
        assert_eq!(marked, vec!["Cargo.toml"], "{MARK_MISPLACED}");
    }

    #[test]
    fn folders_holding_nothing_but_one_another_share_a_row() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("src/main/java")).unwrap();
        fs::write(tmp.path().join("src/main/java/App.java"), "class App {}\n").unwrap();
        let mut tree = Tree::new(tmp.path(), false);

        assert_eq!(names(&tree), vec!["src/main/java"], "{CHAIN_SPLIT}");

        tree.toggle_selected();

        assert_eq!(
            names(&tree),
            vec!["src/main/java", "App.java"],
            "{CHAIN_SHUT}"
        );
    }

    #[test]
    fn a_folder_with_more_than_one_way_on_keeps_its_own_row() {
        let tmp = fixture();
        let tree = Tree::new(tmp.path(), false);

        assert!(names(&tree).contains(&"src".to_owned()), "{CHAIN_GREEDY}");
    }

    #[test]
    fn folding_the_tree_leaves_the_cursor_on_what_held_the_row() {
        let tmp = fixture();
        let mut tree = Tree::new(tmp.path(), false);
        tree.reveal(&tmp.path().join("src/nested/deep.rs"));

        tree.collapse_all();

        assert!(
            tree.rows().iter().all(|row| row.depth == 0),
            "{STILL_UNFOLDED}"
        );
        assert_eq!(
            tree.selected().map(|row| row.name.as_str()),
            Some("src"),
            "{CURSOR_ADRIFT}"
        );
    }

    /// A closed folder is the only place a change under it can be shown, and
    /// opening it hands that job to the rows it reveals.
    #[test]
    fn a_closed_folder_wears_what_is_hidden_under_it() {
        let tmp = fixture();
        let mut tree = Tree::new(tmp.path(), false);
        let mark = |_: &Path| None;
        let under = |_: &Path| Some(GitMark::Modified);

        tree.apply_git(&mark, &under);
        assert_eq!(
            row(&tree, "src").git,
            Some(GitMark::Modified),
            "{FOLDER_MUTE}"
        );

        let src = tree
            .rows()
            .iter()
            .position(|row| row.name == "src")
            .unwrap();
        tree.select_index(src);
        tree.toggle_selected();
        tree.apply_git(&mark, &under);

        assert_eq!(row(&tree, "src").git, None, "{FOLDER_SHOUTS}");
    }

    #[test]
    fn every_git_mark_speaks_at_its_own_volume() {
        let ranks: HashSet<u8> = MARKS.iter().map(|mark| mark.rank()).collect();

        assert_eq!(ranks.len(), MARKS.len(), "{RANK_TIED}");
    }

    #[test]
    fn every_git_mark_has_its_own_letter() {
        let letters: HashSet<&str> = MARKS.iter().map(|mark| mark.letter()).collect();

        assert_eq!(letters.len(), MARKS.len(), "{LETTER_TIED}");
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
