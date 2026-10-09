//! A lazily expanded directory tree.
//!
//! Only expanded directories are ever read, and each is read one level deep, so
//! opening the explorer in a monorepo costs one `readdir` rather than a walk of
//! the whole checkout.

#[cfg(test)]
use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use caudra_workspace::{ResourceKind, WorkspacePath};
use ignore::{DirEntry, WalkBuilder};

use super::backend::{ResourceEntry, WorkbenchPath};

pub(crate) const GIT_DIR: &str = ".git";
/// What joins the names of folders drawn on one row. A label rather than a
/// path, so it reads the same wherever it is drawn.
const CHAIN: char = '/';
const LOCK_SUFFIX: &str = ".lock";

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

/// A directory on this machine that is not under the project, such as where
/// Caudra keeps its config or its plans, listed as a top-level row of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostMount {
    /// What the row reads. The folder's own name rarely says what it holds,
    /// and two mounts may well share one.
    pub label: String,
    /// Read lazily, like a project root. A mount whose directory does not
    /// exist yet is left off until a reload finds it.
    pub root: PathBuf,
    /// Whether a store owns the names of everything inside, as Caudra's
    /// document store does. Creating, renaming or deleting behind its back
    /// would leave it pointing at files that are not there, so nothing in here
    /// offers to.
    pub managed: bool,
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

/// Where the top-level rows come from.
enum Source {
    /// The entries of one directory, read from disk as folders open.
    Local(PathBuf),
    /// A workspace that streams its entries in through `update_remote`, so
    /// nothing is ever read.
    Remote {
        root: WorkbenchPath,
        index: RemoteTree,
    },
    /// One labelled row per directory, each read from disk like a local root.
    /// There is no common root, so nothing above the mounts is ever read.
    Mounts(Vec<HostMount>),
}

impl Source {
    fn mounts(&self) -> &[HostMount] {
        match self {
            Self::Mounts(mounts) => mounts,
            Self::Local(_) | Self::Remote { .. } => &[],
        }
    }
}

pub struct Tree {
    source: Source,
    nodes: Vec<Node>,
    expanded: HashSet<WorkbenchPath>,
    show_hidden: bool,
    rows: Vec<Row>,
    selected: usize,
    scroll: usize,
    follow_selection: bool,
}

impl Default for Tree {
    fn default() -> Self {
        Self {
            source: Source::Local(PathBuf::new()),
            nodes: Vec::new(),
            expanded: HashSet::new(),
            show_hidden: false,
            rows: Vec::new(),
            selected: 0,
            scroll: 0,
            follow_selection: false,
        }
    }
}

impl Tree {
    pub fn new(root: &Path, show_hidden: bool) -> Self {
        let mut tree = Self {
            source: Source::Local(root.to_path_buf()),
            show_hidden,
            ..Self::default()
        };
        tree.reload();
        tree
    }

    pub fn remote(root: WorkbenchPath, show_hidden: bool) -> Self {
        Self {
            source: Source::Remote {
                root,
                index: RemoteTree::default(),
            },
            show_hidden,
            ..Self::default()
        }
    }

    /// The directories outside the project that the explorer shows beside
    /// it, in the order given.
    pub fn mounts(mounts: Vec<HostMount>, show_hidden: bool) -> Self {
        let mut tree = Self {
            source: Source::Mounts(mounts),
            show_hidden,
            ..Self::default()
        };
        tree.reload();
        tree
    }

    pub fn update_remote(&mut self, entries: &[ResourceEntry], removed: &[WorkbenchPath]) {
        let selected = self.selected().map(|row| row.path.clone());
        let Source::Remote { root, index } = &mut self.source else {
            return;
        };
        let visible_change = entries
            .iter()
            .map(|entry| &entry.path)
            .chain(removed)
            .any(|path| {
                path.parent()
                    .is_some_and(|parent| parent == *root || self.expanded.contains(&parent))
            });
        for path in removed {
            index.remove(path);
            self.expanded.remove(path);
        }
        for entry in entries {
            index.insert(entry.clone());
            if entry.kind != ResourceKind::Directory {
                self.expanded.remove(&entry.path);
            }
        }
        if !visible_change {
            return;
        }
        self.rebuild_rows();
        if let Some(path) = selected {
            self.restore_selection(&path);
        }
    }

    pub fn set_remote_root(&mut self, root: &WorkbenchPath) {
        if let Source::Remote { root: current, .. } = &mut self.source
            && current != root
        {
            *current = root.clone();
            self.rebuild_rows();
        }
    }

    /// Swaps in another set of mounts, keeping open folders open and the
    /// cursor on its path. This is a refresh rather than navigation, so the
    /// view stays where it was scrolled, and the same set again reads nothing.
    pub fn set_mounts(&mut self, mounts: Vec<HostMount>) {
        let Source::Mounts(current) = &mut self.source else {
            return;
        };
        if *current != mounts {
            *current = mounts;
            self.reload();
        }
    }

    pub fn mount_list(&self) -> &[HostMount] {
        self.source.mounts()
    }

    /// The folders of this machine whose entries are listed right now, which
    /// are the ones a watch has to cover for the rows to stay true.
    pub fn open_dirs(&self) -> impl Iterator<Item = &Path> {
        self.expanded.iter().filter_map(WorkbenchPath::local)
    }

    /// The mount `path` is listed under. Mounts may nest, and a path belongs
    /// to the one whose directory holds it most closely.
    pub fn mount_of(&self, path: &Path) -> Option<&HostMount> {
        self.mount_list()
            .iter()
            .filter(|mount| path.starts_with(&mount.root))
            .max_by_key(|mount| mount.root.components().count())
    }

    pub fn is_mount_root(&self, path: &WorkbenchPath) -> bool {
        path.local()
            .is_some_and(|path| self.mount_list().iter().any(|mount| mount.root == path))
    }

    /// Whether a store owns `path`. Any managed mount holding it counts, not
    /// only the nearest: a store owns everything under its directory, however
    /// the explorer happens to list it.
    pub fn is_managed(&self, path: &WorkbenchPath) -> bool {
        path.local().is_some_and(|path| {
            self.mount_list()
                .iter()
                .any(|mount| mount.managed && path.starts_with(&mount.root))
        })
    }

    /// Whether `path` lies where `reveal_workbench_path` can reach: under the
    /// root, or under one of the mounts. Decided from the path alone, so it
    /// costs no disk access.
    pub fn contains(&self, path: &WorkbenchPath) -> bool {
        match (&self.source, path) {
            (Source::Local(root), WorkbenchPath::Local(path)) => {
                !root.as_os_str().is_empty() && path.starts_with(root)
            }
            (Source::Remote { root, .. }, path) => path.starts_with(root),
            (Source::Mounts(_), WorkbenchPath::Local(path)) => self.mount_of(path).is_some(),
            (Source::Local(_) | Source::Mounts(_), WorkbenchPath::Remote(_)) => false,
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
            self.reload();
        }
    }

    /// Rereads every directory currently expanded, keeping the selection on the
    /// same path when it survived. Outermost folders go first, since a folder
    /// can only be found once the one holding it has been read again.
    pub fn reload(&mut self) {
        let nodes = match &self.source {
            Source::Local(root) => read_dir(root, self.show_hidden, &[]),
            Source::Remote { .. } => return,
            Source::Mounts(mounts) => mounts
                .iter()
                .filter(|mount| mount.root.is_dir())
                .map(|mount| Node {
                    path: WorkbenchPath::Local(mount.root.clone()),
                    resource: None,
                    name: mount.label.clone(),
                    kind: EntryKind::Directory,
                    ignored: false,
                    children: None,
                })
                .collect(),
        };
        let selected = self.selected().map(|row| row.path.clone());
        self.nodes = nodes;
        let mut expanded: Vec<WorkbenchPath> = self.expanded.iter().cloned().collect();
        expanded.sort_by_key(|path| path.local().map(|path| path.components().count()));
        for path in expanded {
            self.load_children(&path);
        }
        self.rebuild_rows();
        if let Some(path) = selected {
            self.restore_selection(&path);
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        self.follow_selection = true;
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() - 1;
        self.selected = self.selected.saturating_add_signed(delta).min(last);
    }

    pub fn select_first(&mut self) {
        self.follow_selection = true;
        self.selected = 0;
    }

    pub fn select_last(&mut self) {
        self.follow_selection = true;
        self.selected = self.rows.len().saturating_sub(1);
    }

    pub fn select_index(&mut self, index: usize) {
        if index < self.rows.len() {
            self.selected = index;
            self.follow_selection = true;
        }
    }

    /// Expands every ancestor of `path` and lands the cursor on it.
    #[cfg(test)]
    pub fn reveal(&mut self, path: &Path) {
        self.reveal_workbench_path(&WorkbenchPath::Local(path.to_path_buf()));
    }

    /// A mount tree reveals through the mount nearest to `path`, opening its
    /// row along with the folders under it.
    pub fn reveal_workbench_path(&mut self, path: &WorkbenchPath) {
        let Some(folders) = self.folders_above(path) else {
            return;
        };
        for folder in folders {
            self.load_children(&folder);
            self.expanded.insert(folder);
        }
        self.rebuild_rows();
        self.restore_selection(path);
        self.follow_selection = true;
    }

    /// The folders that must be open for `path` to be listed, outermost
    /// first, or `None` when it is out of reach. A mount's own row is one of
    /// them, where a root is not: a root is never listed, only what it holds.
    fn folders_above(&self, path: &WorkbenchPath) -> Option<Vec<WorkbenchPath>> {
        match (path, &self.source) {
            (WorkbenchPath::Local(target), Source::Local(root)) => target
                .starts_with(root)
                .then(|| local_folders_above(target, root, false)),
            (WorkbenchPath::Local(target), Source::Mounts(_)) => self
                .mount_of(target)
                .map(|mount| local_folders_above(target, &mount.root, true)),
            (
                WorkbenchPath::Remote(target),
                Source::Remote {
                    root: WorkbenchPath::Remote(root),
                    ..
                },
            ) => {
                let relative = if root.is_root() {
                    target.as_str()
                } else {
                    target
                        .as_str()
                        .strip_prefix(root.as_str())?
                        .strip_prefix('/')?
                };
                let mut current = WorkbenchPath::Remote(root.clone());
                let mut folders = Vec::new();
                for name in relative.split('/') {
                    current = current.join(name).ok()?;
                    if &current != path {
                        folders.push(current.clone());
                    }
                }
                Some(folders)
            }
            _ => None,
        }
    }

    pub fn resource(&self, path: &WorkbenchPath) -> Option<&ResourceEntry> {
        if let Source::Remote { index, .. } = &self.source {
            return index.entries.get(path);
        }
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
        self.restore_selection(&path);
        self.follow_selection = true;
        true
    }

    /// Folds the tree back to its top level. The cursor rides up to the
    /// nearest ancestor still listed, so it stays on the branch it was in
    /// rather than landing wherever the shorter list happens to reach.
    pub fn collapse_all(&mut self) {
        self.follow_selection = true;
        let selected = self.selected().map(|row| row.path.clone());
        self.expanded.clear();
        self.rebuild_rows();
        self.select_listed(selected);
    }

    /// Collapses the selected directory, or jumps to the folder listing it.
    /// The left arrow means "out of here" either way, short of a mount's own
    /// row: whatever holds its directory is not part of the tree.
    pub fn collapse_or_parent(&mut self) {
        self.follow_selection = true;
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        if row.is_dir() && self.expanded.contains(&row.path) {
            self.toggle_selected();
            return;
        }
        if self.is_mount_root(&row.path) {
            return;
        }
        let parent = row.path.parent();
        self.select_listed(parent);
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

    pub fn clamp_scroll(&mut self, viewport: usize) {
        let max = self.rows.len().saturating_sub(viewport);
        self.scroll = self.scroll.min(max);
        if viewport == 0 {
            return;
        }
        if self.follow_selection {
            if self.selected < self.scroll {
                self.scroll = self.selected;
            } else if self.selected.saturating_sub(self.scroll) >= viewport {
                self.scroll = self.selected + 1 - viewport;
            }
            self.scroll = self.scroll.min(max);
            self.follow_selection = false;
        }
    }

    pub fn scroll_by(&mut self, delta: isize, viewport: usize) {
        self.follow_selection = false;
        let max = self.rows.len().saturating_sub(viewport);
        self.scroll = self.scroll.saturating_add_signed(delta).min(max);
    }

    pub fn set_scroll(&mut self, top: usize, viewport: usize) {
        self.follow_selection = false;
        self.scroll = top.min(self.rows.len().saturating_sub(viewport));
    }

    fn restore_selection(&mut self, path: &WorkbenchPath) {
        if let Some(index) = self.rows.iter().position(|row| &row.path == path) {
            self.selected = index;
        } else {
            self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        }
    }

    /// Lands the cursor on the nearest of `path` and its ancestors that has a
    /// row, so it stays on the branch it was in. A folder sharing a row with
    /// the one inside it has no row of its own, so the plain parent may not do.
    fn select_listed(&mut self, path: Option<WorkbenchPath>) {
        let mut candidate = path;
        while let Some(path) = candidate {
            if let Some(index) = self.rows.iter().position(|row| row.path == path) {
                self.selected = index;
                return;
            }
            candidate = path.parent();
        }
    }

    fn collapse_descendants(&mut self, path: &WorkbenchPath) {
        self.expanded.retain(|open| !open.starts_with(path));
    }

    fn load_children(&mut self, path: &WorkbenchPath) {
        let Some(local_path) = path.local() else {
            return;
        };
        let Some(node) = find_node_mut(&mut self.nodes, path) else {
            return;
        };
        if node.kind != EntryKind::Directory || node.children.is_some() {
            return;
        }
        node.children = Some(read_dir(local_path, self.show_hidden, self.source.mounts()));
    }

    fn rebuild_rows(&mut self) {
        let mut rows = Vec::with_capacity(self.rows.len().max(16));
        match &self.source {
            Source::Remote { root, index } => index.flatten(root, 0, &self.expanded, &mut rows),
            Source::Local(_) | Source::Mounts(_) => {
                flatten(&self.nodes, 0, false, &self.expanded, &mut rows);
            }
        }
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

/// Every folder from `base` down to the one holding `target`, outermost first.
/// `base` itself is left out unless it is `listed`, as a mount's row is.
fn local_folders_above(target: &Path, base: &Path, listed: bool) -> Vec<WorkbenchPath> {
    let mut folders: Vec<WorkbenchPath> = target
        .ancestors()
        .skip(1)
        .take_while(|folder| folder.starts_with(base) && (listed || *folder != base))
        .map(|folder| WorkbenchPath::Local(folder.to_path_buf()))
        .collect();
    folders.reverse();
    folders
}

/// The entries of `dir` alone, with `.git` always left out: it is not source,
/// and walking into it turns the tree into a list of loose objects. A folder
/// one of `mounts` lists under its own label is left out too, so that every
/// path has exactly one row to open, select and reveal. `git` asks for the
/// repository's ignore rules to be applied.
fn walk(
    dir: &Path,
    show_hidden: bool,
    git: bool,
    mounts: &[HostMount],
) -> impl Iterator<Item = DirEntry> {
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
        .filter(move |entry| {
            entry.path() != dir && !mounts.iter().any(|mount| mount.root == entry.path())
        })
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
fn compact(node: Node, show_hidden: bool, mounts: &[HostMount]) -> Node {
    if node.kind != EntryKind::Directory {
        return node;
    }
    let Some(path) = node.path.local() else {
        return node;
    };
    let Some(only) = only_child(path, show_hidden, mounts) else {
        return node;
    };
    if only.kind != EntryKind::Directory {
        return node;
    }
    let deeper = compact(only, show_hidden, mounts);
    Node {
        name: format!("{}{CHAIN}{}", node.name, deeper.name),
        ignored: node.ignored || deeper.ignored,
        ..deeper
    }
}

/// The one entry `dir` holds, if it holds exactly one. Two entries are all it
/// takes to answer that, so the walk stops there rather than reading a folder
/// out just to count it.
fn only_child(dir: &Path, show_hidden: bool, mounts: &[HostMount]) -> Option<Node> {
    let mut entries = walk(dir, show_hidden, false, mounts);
    let entry = entries.next()?;
    if entries.next().is_some() {
        return None;
    }
    let ignored = walk(dir, show_hidden, true, mounts).next().is_none();
    Some(node_of(&entry, ignored))
}

/// One level only. An ignored path is listed like any other and marked rather
/// than left out: a build directory nobody tracks is still somewhere you look,
/// and a tree that silently drops it cannot say why it is not there.
fn read_dir(dir: &Path, show_hidden: bool, mounts: &[HostMount]) -> Vec<Node> {
    let tracked: HashSet<PathBuf> = walk(dir, show_hidden, true, mounts)
        .map(|entry| entry.path().to_path_buf())
        .collect();
    let mut nodes: Vec<Node> = walk(dir, show_hidden, false, mounts)
        .map(|entry| {
            compact(
                node_of(&entry, !tracked.contains(entry.path())),
                show_hidden,
                mounts,
            )
        })
        .collect();
    if !show_hidden && !mounts.is_empty() {
        drop_guard_locks(&mut nodes);
    }
    nodes.sort_by(|a, b| {
        b.kind
            .eq(&EntryKind::Directory)
            .cmp(&a.kind.eq(&EntryKind::Directory))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
    nodes
}

/// Caudra guards each file it keeps with a `<name>.lock` beside it, which
/// says nothing to a reader, so a mount lists one only with the dotfiles.
fn drop_guard_locks(nodes: &mut Vec<Node>) {
    let keep: Vec<bool> = {
        let files: HashSet<&str> = nodes
            .iter()
            .filter(|node| node.kind == EntryKind::File)
            .map(|node| node.name.as_str())
            .collect();
        nodes
            .iter()
            .map(|node| {
                node.kind != EntryKind::File
                    || !node
                        .name
                        .strip_suffix(LOCK_SUFFIX)
                        .is_some_and(|guarded| files.contains(guarded))
            })
            .collect()
    };
    let mut keep = keep.into_iter();
    nodes.retain(|_| keep.next().unwrap_or(true));
}

type RemoteKey = (bool, String, String);

#[derive(Default)]
struct RemoteTree {
    entries: HashMap<WorkbenchPath, ResourceEntry>,
    children: HashMap<WorkbenchPath, BTreeMap<RemoteKey, WorkbenchPath>>,
    #[cfg(test)]
    updates: usize,
    #[cfg(test)]
    visits: Cell<usize>,
}

impl RemoteTree {
    fn key(entry: &ResourceEntry) -> RemoteKey {
        let name = entry.path.file_name();
        (
            entry.kind != ResourceKind::Directory,
            name.to_lowercase(),
            name,
        )
    }

    fn remove(&mut self, path: &WorkbenchPath) {
        let Some(entry) = self.entries.remove(path) else {
            return;
        };
        if let Some(parent) = path.parent()
            && let Some(children) = self.children.get_mut(&parent)
        {
            children.remove(&Self::key(&entry));
            if children.is_empty() {
                self.children.remove(&parent);
            }
        }
    }

    fn insert(&mut self, entry: ResourceEntry) {
        #[cfg(test)]
        {
            self.updates += 1;
        }
        self.remove(&entry.path);
        if let Some(parent) = entry.path.parent() {
            self.children
                .entry(parent)
                .or_default()
                .insert(Self::key(&entry), entry.path.clone());
        }
        self.entries.insert(entry.path.clone(), entry);
    }

    fn flatten(
        &self,
        parent: &WorkbenchPath,
        depth: usize,
        expanded: &HashSet<WorkbenchPath>,
        rows: &mut Vec<Row>,
    ) {
        let Some(children) = self.children.get(parent) else {
            return;
        };
        for ((file, _, name), path) in children {
            #[cfg(test)]
            self.visits.set(self.visits.get() + 1);
            let Some(entry) = self.entries.get(path) else {
                continue;
            };
            let is_expanded = !file && expanded.contains(path);
            rows.push(Row {
                path: path.clone(),
                resource: Some(entry.clone()),
                name: name.clone(),
                depth,
                kind: if *file {
                    EntryKind::File
                } else {
                    EntryKind::Directory
                },
                expanded: is_expanded,
                git: None,
                agent_touched: false,
                ignored: false,
            });
            if is_expanded {
                self.flatten(path, depth + 1, expanded, rows);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{EntryKind, GitMark, HostMount, RemoteTree, Row, Source, Tree};
    use super::{ResourceEntry, ResourceKind, WorkbenchPath, WorkspacePath};
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
    const REMOTE_DIRECTORIES: usize = 128;
    const REMOTE_CHILDREN: usize = 16;
    const REMOTE_PAGE: usize = 32;
    const REMOTE_LINEAR: &str = "remote indexing must touch only changed entries and visible rows";
    const SCROLL_ROWS: usize = 12;
    const SCROLL_VIEWPORT: usize = 3;
    const MANUAL_SCROLL: usize = 5;
    const SCROLL_WRONG: &str = "manual scrolling must persist until explicit selection navigation";
    const FOLLOW_WRONG: &str = "navigation must reveal selection once a viewport is available";
    const MOUNTS_MISLISTED: &str =
        "mounts must be listed by label, in order, once their directory exists";
    const MOUNT_SWALLOWED: &str =
        "a mount's row must keep its label rather than absorb its only folder";
    const LISTED_TWICE: &str = "a folder with a mount of its own must be listed once, under it";
    const MOUNT_MISSED: &str = "a path belongs to the mount whose directory holds it most closely";
    const MANAGED_EXPOSED: &str = "everything under a managed mount must say a store owns it";
    const REVEAL_STRAYED: &str = "revealing a path no mount holds must leave the tree alone";
    const MOUNT_ESCAPED: &str = "leaving a row must climb to what lists it and stop at its mount";
    const REFRESH_REREAD: &str = "the same mounts again must not reread anything";
    const NOT_A_MOUNT_TREE: &str = "a project tree has no mounts, only a root";
    const LOCK_SHOWN: &str = "a lock beside the file it guards must wait for hidden files";
    const SETTINGS_LOCK: &str = "config/caudra.toml.lock";
    const LONE_LOCK: &str = "config/orphan.lock";
    const CONFIG: &str = "Config";
    const MEMORY: &str = "Memory";
    const SCRATCH: &str = "Scratch";
    const PLANS: &str = "Plans";
    const CONFIG_DIR: &str = "config";
    const MEMORY_DIR: &str = ".memory";
    const SCRATCH_DIR: &str = "scratch";
    const PLANS_DIR: &str = "config/plans";
    const THEMES_DIR: &str = "config/themes";
    const NOTES_DIR: &str = ".memory/notes/2026";
    const NOTES_ROW: &str = "notes/2026";
    const SETTINGS: &str = "config/caudra.toml";
    const SECRET: &str = "config/.secret.toml";
    const DARK_THEME: &str = "config/themes/dark.toml";
    const LIGHT_THEME: &str = "config/themes/light.toml";
    const ADDED_THEME: &str = "config/themes/added.toml";
    const PLAN: &str = "config/plans/archive/old.md";
    const FACT: &str = ".memory/notes/2026/fact.md";
    const OUTSIDE: &str = "elsewhere.txt";
    const NAME_PREFIX: &str = "config-old/caudra.toml";
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

    #[test_case(false; "directories_first")]
    #[test_case(true; "children_first")]
    fn remote_parent_index_installs_many_pages_without_recursive_rebuilds(children_first: bool) {
        let root = WorkbenchPath::Remote(WorkspacePath::root());
        let mut tree = Tree::remote(root, false);
        let entry = |path: String, kind| ResourceEntry {
            path: WorkbenchPath::Remote(WorkspacePath::new(path).unwrap()),
            resource_id: None,
            revision: None,
            kind,
            size_bytes: None,
        };
        let directories = (0..REMOTE_DIRECTORIES)
            .map(|index| entry(format!("dir-{index}"), ResourceKind::Directory))
            .collect::<Vec<_>>();
        let children = (0..REMOTE_DIRECTORIES)
            .flat_map(|dir| {
                (0..REMOTE_CHILDREN)
                    .map(move |child| entry(format!("dir-{dir}/file-{child}"), ResourceKind::File))
            })
            .collect::<Vec<_>>();
        if !children_first {
            tree.update_remote(&directories, &[]);
        }
        for page in children.chunks(REMOTE_PAGE) {
            tree.update_remote(page, &[]);
        }
        if children_first {
            tree.update_remote(&directories, &[]);
        }
        let remote = remote_index(&tree).unwrap();
        assert_eq!(
            remote.updates,
            directories.len() + children.len(),
            "{REMOTE_LINEAR}"
        );
        assert_eq!(remote.visits.get(), directories.len(), "{REMOTE_LINEAR}");
        assert_eq!(remote.children.len(), REMOTE_DIRECTORIES + 1);
        assert_eq!(tree.rows().len(), REMOTE_DIRECTORIES);
        let selected = children.last().unwrap();
        tree.reveal_workbench_path(&selected.path);
        assert_eq!(tree.selected().unwrap().path, selected.path);
        assert_eq!(tree.rows().len(), REMOTE_DIRECTORIES + REMOTE_CHILDREN);
        let before = remote_index(&tree).unwrap().updates;
        tree.update_remote(&[], std::slice::from_ref(&selected.path));
        assert!(tree.resource(&selected.path).is_none());
        assert_eq!(
            remote_index(&tree).unwrap().updates,
            before,
            "{REMOTE_LINEAR}"
        );
    }

    fn remote_index(tree: &Tree) -> Option<&RemoteTree> {
        match &tree.source {
            Source::Remote { index, .. } => Some(index),
            Source::Local(_) | Source::Mounts(_) => None,
        }
    }

    fn names(tree: &Tree) -> Vec<String> {
        tree.rows().iter().map(|row| row.name.clone()).collect()
    }

    fn scrolling_tree() -> Tree {
        let mut tree = Tree::remote(WorkbenchPath::Remote(WorkspacePath::root()), false);
        let entries = (0..SCROLL_ROWS)
            .map(|index| ResourceEntry {
                path: WorkbenchPath::Remote(
                    WorkspacePath::new(format!("file-{index:02}")).unwrap(),
                ),
                resource_id: None,
                revision: None,
                kind: ResourceKind::File,
                size_bytes: None,
            })
            .collect::<Vec<_>>();
        tree.update_remote(&entries, &[]);
        tree
    }

    #[test_case(false, SCROLL_VIEWPORT; "wheel")]
    #[test_case(true, SCROLL_VIEWPORT; "bar")]
    #[test_case(false, 0; "wheel_without_viewport")]
    #[test_case(true, 0; "bar_without_viewport")]
    fn manual_scroll_survives_redraws(bar: bool, viewport: usize) {
        let mut tree = scrolling_tree();
        tree.select_first();
        if bar {
            tree.set_scroll(MANUAL_SCROLL, viewport);
        } else {
            tree.scroll_by(MANUAL_SCROLL as isize, viewport);
        }
        tree.clamp_scroll(viewport);
        tree.clamp_scroll(SCROLL_VIEWPORT);
        tree.clamp_scroll(SCROLL_VIEWPORT);
        assert_eq!(tree.scroll(), MANUAL_SCROLL, "{SCROLL_WRONG}");
        assert_eq!(tree.selected_index(), 0, "{CURSOR_ADRIFT}");
    }

    #[test_case(Tree::select_first, false; "home")]
    #[test_case(Tree::select_last, true; "end")]
    #[test_case(|tree: &mut Tree| tree.move_selection(-1), false; "up_at_start")]
    #[test_case(|tree: &mut Tree| tree.move_selection(1), true; "down_at_end")]
    #[test_case(|tree: &mut Tree| tree.select_index(tree.selected_index()), false; "same_index")]
    #[test_case(Tree::collapse_or_parent, false; "parent_at_root")]
    #[test_case(Tree::collapse_all, false; "already_folded")]
    fn boundary_navigation_follows_once(action: fn(&mut Tree), last: bool) {
        let mut tree = scrolling_tree();
        if last {
            tree.select_last();
        }
        tree.set_scroll(MANUAL_SCROLL, SCROLL_VIEWPORT);
        action(&mut tree);
        tree.clamp_scroll(SCROLL_VIEWPORT);
        let expected = if last {
            SCROLL_ROWS - SCROLL_VIEWPORT
        } else {
            0
        };
        assert_eq!(tree.scroll(), expected, "{FOLLOW_WRONG}");
        tree.clamp_scroll(SCROLL_ROWS);
        tree.clamp_scroll(SCROLL_VIEWPORT);
        assert_eq!(tree.scroll(), 0, "{SCROLL_WRONG}");
    }

    #[test_case(1; "single_row")]
    #[test_case(SCROLL_VIEWPORT; "several_rows")]
    fn zero_viewport_retains_navigation(viewport: usize) {
        let mut tree = scrolling_tree();
        tree.select_last();
        tree.clamp_scroll(0);
        tree.clamp_scroll(0);
        assert_eq!(tree.scroll(), 0, "{FOLLOW_WRONG}");
        tree.clamp_scroll(viewport);
        assert_eq!(tree.scroll(), SCROLL_ROWS - viewport, "{FOLLOW_WRONG}");
    }

    #[test_case(0; "hidden")]
    #[test_case(SCROLL_VIEWPORT - 1; "partly_visible")]
    #[test_case(SCROLL_VIEWPORT; "visible")]
    fn remote_removal_bounds_manual_scroll_without_following(viewport: usize) {
        let mut tree = scrolling_tree();
        tree.set_scroll(MANUAL_SCROLL, SCROLL_VIEWPORT);
        let removed = tree.rows()[SCROLL_VIEWPORT..]
            .iter()
            .map(|row| row.path.clone())
            .collect::<Vec<_>>();
        tree.update_remote(&[], &removed);
        tree.clamp_scroll(viewport);
        assert_eq!(tree.scroll(), SCROLL_VIEWPORT - viewport, "{SCROLL_WRONG}");
        assert_eq!(tree.selected_index(), 0, "{CURSOR_ADRIFT}");
    }

    #[test_case(false; "metadata_update")]
    #[test_case(true; "insertion_before_selection")]
    fn remote_updates_preserve_manual_scroll(insert: bool) {
        let mut tree = scrolling_tree();
        let selected = tree.selected().unwrap().path.clone();
        let mut entry = tree.selected().unwrap().resource.clone().unwrap();
        if insert {
            entry.path = WorkbenchPath::Remote(WorkspacePath::new("before-files").unwrap());
        }
        tree.set_scroll(MANUAL_SCROLL, SCROLL_VIEWPORT);
        tree.update_remote(&[entry], &[]);
        tree.clamp_scroll(SCROLL_VIEWPORT);
        assert_eq!(tree.scroll(), MANUAL_SCROLL, "{SCROLL_WRONG}");
        assert_eq!(tree.selected().unwrap().path, selected, "{CURSOR_ADRIFT}");
        tree.clamp_scroll(tree.rows().len() - 1);
        assert_eq!(tree.scroll(), 1, "{SCROLL_WRONG}");
    }

    #[test_case(false; "unchanged")]
    #[test_case(true; "selected_file_removed")]
    fn reload_restores_selection_without_following(remove: bool) {
        let tmp = fixture();
        let mut tree = Tree::new(tmp.path(), true);
        tree.reveal(&tmp.path().join("Cargo.toml"));
        tree.set_scroll(0, 1);
        if remove {
            fs::remove_file(tmp.path().join("Cargo.toml")).unwrap();
        }
        tree.reload();
        tree.clamp_scroll(1);
        tree.clamp_scroll(1);
        assert_eq!(tree.scroll(), 0, "{SCROLL_WRONG}");
        assert!(tree.selected_index() > 0, "{CURSOR_ADRIFT}");
    }

    #[test_case(false; "reveal")]
    #[test_case(true; "toggle")]
    fn explicit_tree_actions_reveal_selection(toggle: bool) {
        let tmp = fixture();
        let mut tree = Tree::new(tmp.path(), true);
        let path = tmp.path().join("src");
        tree.reveal(&path);
        tree.set_scroll(tree.rows().len() - 1, 1);
        if toggle {
            assert!(tree.toggle_selected(), "{FOLLOW_WRONG}");
        } else {
            tree.reveal(&path);
        }
        tree.clamp_scroll(1);
        assert_eq!(tree.scroll(), tree.selected_index(), "{FOLLOW_WRONG}");
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

    /// Plans sits inside Config's directory, Memory's directory is hidden and
    /// holds a single folder, and Scratch has not been created yet.
    fn host_fixture() -> TempDir {
        let tmp = TempDir::new().unwrap();
        for file in [SETTINGS, SECRET, DARK_THEME, LIGHT_THEME, PLAN, FACT] {
            let path = tmp.path().join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "").unwrap();
        }
        tmp
    }

    fn host_mounts(base: &Path) -> Vec<HostMount> {
        [
            (CONFIG, CONFIG_DIR, false),
            (MEMORY, MEMORY_DIR, true),
            (SCRATCH, SCRATCH_DIR, false),
            (PLANS, PLANS_DIR, true),
        ]
        .into_iter()
        .map(|(label, dir, managed)| HostMount {
            label: label.to_owned(),
            root: base.join(dir),
            managed,
        })
        .collect()
    }

    fn leaf(relative: &str) -> String {
        Path::new(relative)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    fn names_at(tree: &Tree, depth: usize) -> Vec<String> {
        tree.rows()
            .iter()
            .filter(|row| row.depth == depth)
            .map(|row| row.name.clone())
            .collect()
    }

    fn selected_path(tree: &Tree) -> Option<&Path> {
        tree.selected().and_then(|row| row.path.local())
    }

    #[test]
    fn mounts_are_listed_by_label_once_their_directory_exists() {
        let tmp = host_fixture();
        let mut tree = Tree::mounts(host_mounts(tmp.path()), false);

        assert_eq!(names(&tree), [CONFIG, MEMORY, PLANS], "{MOUNTS_MISLISTED}");
        assert!(
            tree.rows()
                .iter()
                .all(|row| row.depth == 0 && row.is_dir() && !row.expanded),
            "{MOUNTS_MISLISTED}"
        );

        fs::create_dir(tmp.path().join(SCRATCH_DIR)).unwrap();
        tree.reload();

        assert_eq!(
            names(&tree),
            [CONFIG, MEMORY, SCRATCH, PLANS],
            "{MOUNTS_MISLISTED}"
        );
    }

    #[test]
    fn a_mount_opens_one_level_at_a_time() {
        let tmp = host_fixture();
        let mut tree = Tree::mounts(host_mounts(tmp.path()), false);

        assert!(tree.toggle_selected());
        assert_eq!(
            names_at(&tree, 1),
            [leaf(THEMES_DIR), leaf(SETTINGS)],
            "{LAZY}"
        );
        assert!(names_at(&tree, 2).is_empty(), "{LAZY}");
        assert!(!names(&tree).contains(&leaf(PLANS_DIR)), "{LISTED_TWICE}");

        tree.reveal(&tmp.path().join(THEMES_DIR));
        assert!(tree.toggle_selected());
        assert_eq!(
            names_at(&tree, 2),
            [leaf(DARK_THEME), leaf(LIGHT_THEME)],
            "{LAZY}"
        );
    }

    #[test]
    fn a_mount_holding_one_folder_keeps_its_label() {
        let tmp = host_fixture();
        let mut tree = Tree::mounts(host_mounts(tmp.path()), false);

        tree.reveal(&tmp.path().join(MEMORY_DIR));
        assert_eq!(
            tree.selected().map(|row| row.name.as_str()),
            Some(MEMORY),
            "{MOUNT_SWALLOWED}"
        );

        assert!(tree.toggle_selected());
        assert_eq!(names_at(&tree, 1), [NOTES_ROW], "{CHAIN_SPLIT}");
    }

    #[test_case(false; "hidden")]
    #[test_case(true; "shown")]
    fn a_mount_lists_a_guard_lock_only_when_asked(show_hidden: bool) {
        let tmp = host_fixture();
        fs::write(tmp.path().join(SETTINGS_LOCK), "").unwrap();
        fs::write(tmp.path().join(LONE_LOCK), "").unwrap();
        let mut tree = Tree::mounts(host_mounts(tmp.path()), show_hidden);

        tree.toggle_selected();

        let listed = names(&tree);
        assert_eq!(
            listed.contains(&leaf(SETTINGS_LOCK)),
            show_hidden,
            "{LOCK_SHOWN}"
        );
        assert!(
            listed.contains(&leaf(LONE_LOCK)) && listed.contains(&leaf(SETTINGS)),
            "{MOUNTS_MISLISTED}"
        );
    }

    #[test_case(false; "hidden")]
    #[test_case(true; "shown")]
    fn a_mount_lists_dotfiles_only_when_asked(show_hidden: bool) {
        let tmp = host_fixture();
        let mut tree = Tree::mounts(host_mounts(tmp.path()), !show_hidden);
        tree.toggle_selected();

        tree.set_show_hidden(show_hidden);

        assert_eq!(
            names(&tree).contains(&leaf(SECRET)),
            show_hidden,
            "{DOTFILE_SHOWN}"
        );
        assert!(
            names(&tree).contains(&MEMORY.to_owned()),
            "{MOUNTS_MISLISTED}"
        );
    }

    #[test_case(PLAN, PLANS, 2; "nested_mount_wins")]
    #[test_case(LIGHT_THEME, CONFIG, 2; "outer_mount")]
    #[test_case(FACT, MEMORY, 2; "through_a_shared_row")]
    #[test_case(CONFIG_DIR, CONFIG, 0; "mount_row")]
    fn revealing_goes_through_the_nearest_mount(relative: &str, label: &str, depth: usize) {
        let tmp = host_fixture();
        let mut tree = Tree::mounts(host_mounts(tmp.path()), false);
        let target = tmp.path().join(relative);
        tree.reveal(&tmp.path().join(SETTINGS));
        tree.set_scroll(tree.rows().len() - 1, 1);

        tree.reveal(&target);

        let selected = tree.selected_index();
        assert_eq!(selected_path(&tree), Some(target.as_path()), "{REVEALED}");
        assert_eq!(tree.rows()[selected].depth, depth, "{REVEALED}");
        let mount = tree.rows()[..=selected]
            .iter()
            .rev()
            .find(|row| row.depth == 0)
            .map(|row| row.name.as_str());
        assert_eq!(mount, Some(label), "{MOUNT_MISSED}");
        assert_eq!(
            tree.rows()
                .iter()
                .filter(|row| row.path.local() == Some(target.as_path()))
                .count(),
            1,
            "{LISTED_TWICE}"
        );
        tree.clamp_scroll(1);
        assert_eq!(tree.scroll(), selected, "{FOLLOW_WRONG}");
    }

    #[test_case(OUTSIDE; "beside_the_mounts")]
    #[test_case(NAME_PREFIX; "sharing_a_name_prefix")]
    fn revealing_outside_every_mount_changes_nothing(relative: &str) {
        let tmp = host_fixture();
        let mut tree = Tree::mounts(host_mounts(tmp.path()), false);
        let last = tree.rows().len() - 1;
        tree.set_scroll(last, 1);

        tree.reveal(&tmp.path().join(relative));
        tree.clamp_scroll(1);

        assert_eq!(names(&tree), [CONFIG, MEMORY, PLANS], "{REVEAL_STRAYED}");
        assert_eq!(tree.selected_index(), 0, "{REVEAL_STRAYED}");
        assert_eq!(tree.scroll(), last, "{REVEAL_STRAYED}");
    }

    #[test_case(CONFIG_DIR, Some(CONFIG), true, false; "outer_mount_row")]
    #[test_case(SETTINGS, Some(CONFIG), false, false; "inside_the_outer_mount")]
    #[test_case(PLANS_DIR, Some(PLANS), true, true; "nested_mount_row")]
    #[test_case(PLAN, Some(PLANS), false, true; "inside_the_nested_mount")]
    #[test_case(FACT, Some(MEMORY), false, true; "inside_a_managed_mount")]
    #[test_case(SCRATCH_DIR, Some(SCRATCH), true, false; "mount_not_created_yet")]
    #[test_case(OUTSIDE, None, false, false; "beside_the_mounts")]
    #[test_case(NAME_PREFIX, None, false, false; "sharing_a_name_prefix")]
    fn a_path_answers_to_its_nearest_mount(
        relative: &str,
        label: Option<&str>,
        mount_root: bool,
        managed: bool,
    ) {
        let tmp = host_fixture();
        let tree = Tree::mounts(host_mounts(tmp.path()), false);
        let path = tmp.path().join(relative);
        let workbench_path = WorkbenchPath::Local(path.clone());

        assert_eq!(
            tree.mount_of(&path).map(|mount| mount.label.as_str()),
            label,
            "{MOUNT_MISSED}"
        );
        assert_eq!(
            tree.contains(&workbench_path),
            label.is_some(),
            "{MOUNT_MISSED}"
        );
        assert_eq!(
            tree.is_mount_root(&workbench_path),
            mount_root,
            "{MOUNT_MISSED}"
        );
        assert_eq!(
            tree.is_managed(&workbench_path),
            managed,
            "{MANAGED_EXPOSED}"
        );
    }

    #[test]
    fn a_project_tree_has_no_mounts() {
        let tmp = fixture();
        let tree = Tree::new(tmp.path(), false);
        let inside = tmp.path().join(SETTINGS);
        let beside = tmp.path().with_file_name(OUTSIDE);

        assert!(tree.mount_list().is_empty(), "{NOT_A_MOUNT_TREE}");
        assert!(tree.mount_of(&inside).is_none(), "{NOT_A_MOUNT_TREE}");
        assert!(
            tree.contains(&WorkbenchPath::Local(inside)),
            "{NOT_A_MOUNT_TREE}"
        );
        assert!(
            !tree.contains(&WorkbenchPath::Local(beside)),
            "{NOT_A_MOUNT_TREE}"
        );
    }

    #[test_case(false; "same_mounts")]
    #[test_case(true; "one_mount_fewer")]
    fn refreshing_the_mounts_keeps_the_view(changed: bool) {
        let tmp = host_fixture();
        let mut tree = Tree::mounts(host_mounts(tmp.path()), false);
        let theme = tmp.path().join(LIGHT_THEME);
        tree.reveal(&theme);
        tree.set_scroll(0, 1);
        fs::write(tmp.path().join(ADDED_THEME), "").unwrap();
        let mut mounts = host_mounts(tmp.path());
        if changed {
            mounts.retain(|mount| mount.label != MEMORY);
        }

        tree.set_mounts(mounts);
        tree.clamp_scroll(1);

        assert_eq!(
            names(&tree).contains(&leaf(ADDED_THEME)),
            changed,
            "{REFRESH_REREAD}"
        );
        assert_eq!(
            names(&tree).contains(&MEMORY.to_owned()),
            !changed,
            "{MOUNTS_MISLISTED}"
        );
        assert_eq!(
            selected_path(&tree),
            Some(theme.as_path()),
            "{CURSOR_ADRIFT}"
        );
        assert_eq!(tree.scroll(), 0, "{SCROLL_WRONG}");
    }

    #[test_case(MEMORY_DIR, MEMORY_DIR; "closed_mount_row_stays")]
    #[test_case(PLANS_DIR, PLANS_DIR; "nested_mount_row_stays")]
    #[test_case(SETTINGS, CONFIG_DIR; "child_climbs_to_its_mount")]
    #[test_case(LIGHT_THEME, THEMES_DIR; "file_climbs_to_its_folder")]
    #[test_case(NOTES_DIR, MEMORY_DIR; "shared_row_climbs_to_its_mount")]
    fn leaving_a_row_stops_at_its_mount(relative: &str, expected: &str) {
        let tmp = host_fixture();
        let mut tree = Tree::mounts(host_mounts(tmp.path()), false);
        tree.reveal(&tmp.path().join(LIGHT_THEME));
        tree.reveal(&tmp.path().join(relative));

        tree.collapse_or_parent();

        assert_eq!(
            selected_path(&tree),
            Some(tmp.path().join(expected).as_path()),
            "{MOUNT_ESCAPED}"
        );
    }

    #[test]
    fn folding_mounts_leaves_their_rows() {
        let tmp = host_fixture();
        let mut tree = Tree::mounts(host_mounts(tmp.path()), false);
        tree.reveal(&tmp.path().join(PLAN));

        tree.collapse_all();

        assert_eq!(names(&tree), [CONFIG, MEMORY, PLANS], "{STILL_UNFOLDED}");
        assert_eq!(
            tree.selected().map(|row| row.name.as_str()),
            Some(PLANS),
            "{CURSOR_ADRIFT}"
        );
    }
}
