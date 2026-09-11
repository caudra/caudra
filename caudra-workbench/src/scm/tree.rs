//! Turns a set of changed paths into the rows a section lists.
//!
//! This is not [`crate::fs::tree`]. That one walks the filesystem and answers
//! for every entry it finds; this one only ever sees the handful of paths the
//! repository reported, so it builds the folders it needs out of the paths
//! themselves and never touches the disk.
//!
//! It serves both the change sections and the files under an expanded commit,
//! so it emits [`Node`]s rather than the pane's rows and leaves the caller to
//! say what a leaf is.

use std::collections::HashSet;

pub(crate) const SEPARATOR: char = '/';

/// A folder the tree invented to hold changed paths. `path` is the fold key and
/// the cursor identity, which is the repository-relative path under
/// [`Layout::scope`], so the same folder under two commits folds independently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dir {
    pub label: String,
    pub path: String,
    pub depth: usize,
    pub expanded: bool,
}

/// One laid-out entry. `index` is whatever the caller paired the path with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Node {
    Dir(usize),
    Leaf { index: usize, depth: usize },
}

/// How one call lays its paths out.
#[derive(Debug, Clone, Copy, Default)]
pub struct Layout<'a> {
    /// List the paths whole instead of nesting them under folders.
    pub flat: bool,
    /// Prefix for every folder's fold key, which keeps two callers' folders
    /// apart. The change sections share one namespace and pass nothing.
    pub scope: &'a str,
    /// The depth the top level sits at, so a commit's files indent past it.
    pub depth: usize,
}

#[derive(Default)]
struct Nodes {
    nodes: Vec<Node>,
    dirs: Vec<Dir>,
}

/// Lays `paths` out, each paired with its index into whatever list the caller
/// holds them in.
///
/// Flat mode is a plain sorted list, and tree mode nests it under folders that
/// carry their own fold state.
pub fn rows(
    paths: &[(usize, &str)],
    layout: Layout<'_>,
    collapsed: &HashSet<String>,
) -> (Vec<Node>, Vec<Dir>) {
    let mut sorted = paths.to_vec();
    sorted.sort_by(|a, b| a.1.cmp(b.1));
    if layout.flat {
        let nodes = sorted
            .iter()
            .map(|(index, _)| Node::Leaf {
                index: *index,
                depth: layout.depth,
            })
            .collect();
        return (nodes, Vec::new());
    }
    let mut out = Nodes::default();
    push_level(&sorted, "", layout, layout.depth, collapsed, &mut out);
    (out.nodes, out.dirs)
}

/// Emits one level: folders first, then files, each already in path order
/// because the input was sorted and `/` sorts below every name character.
fn push_level(
    entries: &[(usize, &str)],
    prefix: &str,
    layout: Layout<'_>,
    depth: usize,
    collapsed: &HashSet<String>,
    out: &mut Nodes,
) {
    let mut dirs: Vec<(&str, Vec<(usize, &str)>)> = Vec::new();
    let mut files: Vec<usize> = Vec::new();
    for (index, path) in entries {
        match path[prefix.len()..].split_once(SEPARATOR) {
            None => files.push(*index),
            Some((head, _)) => match dirs.last_mut() {
                Some((name, group)) if *name == head => group.push((*index, path)),
                _ => dirs.push((head, vec![(*index, path)])),
            },
        }
    }

    for (head, group) in dirs {
        push_dir(head, &group, prefix, layout, depth, collapsed, out);
    }
    out.nodes
        .extend(files.into_iter().map(|index| Node::Leaf { index, depth }));
}

fn push_dir(
    head: &str,
    group: &[(usize, &str)],
    prefix: &str,
    layout: Layout<'_>,
    depth: usize,
    collapsed: &HashSet<String>,
    out: &mut Nodes,
) {
    let mut label = head.to_owned();
    let mut path = format!("{prefix}{head}");
    let mut child = format!("{path}{SEPARATOR}");
    // A folder holding nothing but one more folder is drawn as a single
    // `a/b/c` row, so a deep source tree does not spend four rows saying
    // nothing.
    while let Some(only) = sole_child(group, &child) {
        label.push(SEPARATOR);
        label.push_str(only);
        path = format!("{path}{SEPARATOR}{only}");
        child = format!("{path}{SEPARATOR}");
    }

    // The key is scoped; `child` is not, because it slices the paths that were
    // handed in and those know nothing about the scope.
    let key = format!("{}{path}", layout.scope);
    let expanded = !collapsed.contains(&key);
    out.dirs.push(Dir {
        label,
        path: key,
        depth,
        expanded,
    });
    out.nodes.push(Node::Dir(out.dirs.len() - 1));
    if expanded {
        push_level(group, &child, layout, depth + 1, collapsed, out);
    }
}

/// The one folder every entry under `prefix` sits in, or `None` when they
/// disagree or when any of them is a file at this level.
fn sole_child<'a>(group: &[(usize, &'a str)], prefix: &str) -> Option<&'a str> {
    let mut only: Option<&str> = None;
    for (_, path) in group {
        let (head, _) = path[prefix.len()..].split_once(SEPARATOR)?;
        match only {
            Some(seen) if seen != head => return None,
            _ => only = Some(head),
        }
    }
    only
}

#[cfg(test)]
mod tests {
    use super::{Dir, HashSet, Layout, Node, rows};

    const WRONG_SHAPE: &str = "the change tree does not have the shape the paths describe";
    const LOST_A_CHANGE: &str = "a changed path is missing from the rows";
    const SCOPES_SHARE_A_FOLD: &str = "the same folder under two scopes folds as one";

    fn tree(paths: &[&str], collapsed: &[&str]) -> (Vec<Node>, Vec<Dir>) {
        scoped(paths, collapsed, Layout::default())
    }

    fn scoped(paths: &[&str], collapsed: &[&str], layout: Layout<'_>) -> (Vec<Node>, Vec<Dir>) {
        let numbered: Vec<(usize, &str)> = paths.iter().copied().enumerate().collect();
        let folded: HashSet<String> = collapsed.iter().map(|path| (*path).to_owned()).collect();
        rows(&numbered, layout, &folded)
    }

    fn labels(dirs: &[Dir]) -> Vec<&str> {
        dirs.iter().map(|dir| dir.label.as_str()).collect()
    }

    #[test]
    fn a_flat_list_keeps_every_change_and_invents_no_folders() {
        let (nodes, dirs) = scoped(
            &["b.rs", "a/deep/file.rs"],
            &[],
            Layout {
                flat: true,
                ..Layout::default()
            },
        );

        assert!(dirs.is_empty(), "{WRONG_SHAPE}");
        assert_eq!(
            nodes,
            vec![
                Node::Leaf { index: 1, depth: 0 },
                Node::Leaf { index: 0, depth: 0 },
            ],
            "{LOST_A_CHANGE}"
        );
    }

    #[test]
    fn a_chain_of_single_child_folders_is_compacted_into_one_row() {
        let (nodes, dirs) = tree(&["src/scm/repo.rs"], &[]);

        assert_eq!(labels(&dirs), vec!["src/scm"], "{WRONG_SHAPE}");
        assert_eq!(
            nodes,
            vec![Node::Dir(0), Node::Leaf { index: 0, depth: 1 }],
            "{WRONG_SHAPE}"
        );
    }

    #[test]
    fn a_folder_holding_a_file_is_not_compacted_past_it() {
        let (_, dirs) = tree(&["src/lib.rs", "src/scm/repo.rs"], &[]);
        assert_eq!(labels(&dirs), vec!["src", "scm"], "{WRONG_SHAPE}");
    }

    #[test]
    fn folders_come_before_files_at_the_same_depth() {
        let (nodes, dirs) = tree(&["a.rs", "z/b.rs"], &[]);

        assert_eq!(labels(&dirs), vec!["z"], "{WRONG_SHAPE}");
        assert_eq!(
            nodes,
            vec![
                Node::Dir(0),
                Node::Leaf { index: 1, depth: 1 },
                Node::Leaf { index: 0, depth: 0 },
            ],
            "{WRONG_SHAPE}"
        );
    }

    #[test]
    fn a_collapsed_folder_lists_itself_and_nothing_under_it() {
        let (nodes, dirs) = tree(&["src/a.rs", "src/b.rs"], &["src"]);

        assert_eq!(nodes, vec![Node::Dir(0)], "{WRONG_SHAPE}");
        assert!(!dirs[0].expanded, "{WRONG_SHAPE}");
    }

    #[test]
    fn siblings_that_share_a_prefix_stay_apart() {
        let (_, dirs) = tree(&["src/a.rs", "srcs/b.rs"], &[]);
        assert_eq!(labels(&dirs), vec!["src", "srcs"], "{WRONG_SHAPE}");
    }

    #[test]
    fn a_file_sorting_between_two_folder_entries_does_not_split_the_folder() {
        let (nodes, dirs) = tree(&["src/b.rs", "src/b/c.rs"], &[]);

        assert_eq!(labels(&dirs), vec!["src", "b"], "{WRONG_SHAPE}");
        assert_eq!(nodes.len(), 4, "{LOST_A_CHANGE}");
    }

    #[test]
    fn a_folder_folded_under_one_scope_stays_open_under_another() {
        let layout = Layout {
            scope: "abc1234/",
            ..Layout::default()
        };

        let (unscoped, _) = tree(&["src/a.rs"], &["src"]);
        let (nodes, dirs) = scoped(&["src/a.rs"], &["src"], layout);

        assert_eq!(unscoped, vec![Node::Dir(0)], "{WRONG_SHAPE}");
        assert_eq!(dirs[0].path, "abc1234/src", "{SCOPES_SHARE_A_FOLD}");
        assert_eq!(
            nodes,
            vec![Node::Dir(0), Node::Leaf { index: 0, depth: 1 }],
            "{SCOPES_SHARE_A_FOLD}"
        );
    }

    #[test]
    fn a_starting_depth_indents_the_whole_level() {
        let layout = Layout {
            depth: 1,
            ..Layout::default()
        };

        let (nodes, dirs) = scoped(&["a.rs", "z/b.rs"], &[], layout);

        assert_eq!(dirs[0].depth, 1, "{WRONG_SHAPE}");
        assert_eq!(
            nodes,
            vec![
                Node::Dir(0),
                Node::Leaf { index: 1, depth: 2 },
                Node::Leaf { index: 0, depth: 1 },
            ],
            "{WRONG_SHAPE}"
        );
    }
}
