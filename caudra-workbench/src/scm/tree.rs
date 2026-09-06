//! Turns a set of changed paths into the rows a change section lists.
//!
//! This is not [`crate::fs::tree`]. That one walks the filesystem and answers
//! for every entry it finds; this one only ever sees the handful of paths the
//! repository reported, so it builds the folders it needs out of the paths
//! themselves and never touches the disk.

use std::collections::HashSet;

use crate::scm::Row;

pub(crate) const SEPARATOR: char = '/';

/// A folder the tree invented to hold changed paths. `path` is relative to the
/// repository's workdir, which is what both the fold state and a staging sweep
/// over the subtree are keyed on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dir {
    pub label: String,
    pub path: String,
    pub depth: usize,
    pub expanded: bool,
}

#[derive(Default)]
struct Rows {
    rows: Vec<Row>,
    dirs: Vec<Dir>,
}

/// Lays `paths` out, each paired with its index into the pane's change list.
///
/// Flat mode is a plain sorted list, and tree mode nests it under folders that
/// carry their own fold state.
pub fn rows(
    paths: &[(usize, &str)],
    flat: bool,
    collapsed: &HashSet<String>,
) -> (Vec<Row>, Vec<Dir>) {
    let mut sorted = paths.to_vec();
    sorted.sort_by(|a, b| a.1.cmp(b.1));
    if flat {
        let rows = sorted
            .iter()
            .map(|(index, _)| Row::Change {
                index: *index,
                depth: 0,
            })
            .collect();
        return (rows, Vec::new());
    }
    let mut out = Rows::default();
    push_level(&sorted, "", 0, collapsed, &mut out);
    (out.rows, out.dirs)
}

/// Emits one level: folders first, then files, each already in path order
/// because the input was sorted and `/` sorts below every name character.
fn push_level(
    entries: &[(usize, &str)],
    prefix: &str,
    depth: usize,
    collapsed: &HashSet<String>,
    out: &mut Rows,
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
        push_dir(head, &group, prefix, depth, collapsed, out);
    }
    out.rows
        .extend(files.into_iter().map(|index| Row::Change { index, depth }));
}

fn push_dir(
    head: &str,
    group: &[(usize, &str)],
    prefix: &str,
    depth: usize,
    collapsed: &HashSet<String>,
    out: &mut Rows,
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

    let expanded = !collapsed.contains(&path);
    out.dirs.push(Dir {
        label,
        path,
        depth,
        expanded,
    });
    out.rows.push(Row::Directory(out.dirs.len() - 1));
    if expanded {
        push_level(group, &child, depth + 1, collapsed, out);
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
    use super::{Dir, HashSet, Row, rows};

    const WRONG_SHAPE: &str = "the change tree does not have the shape the paths describe";
    const LOST_A_CHANGE: &str = "a changed path is missing from the rows";

    fn tree(paths: &[&str], collapsed: &[&str]) -> (Vec<Row>, Vec<Dir>) {
        let numbered: Vec<(usize, &str)> = paths.iter().copied().enumerate().collect();
        let folded: HashSet<String> = collapsed.iter().map(|path| (*path).to_owned()).collect();
        rows(&numbered, false, &folded)
    }

    fn labels(dirs: &[Dir]) -> Vec<&str> {
        dirs.iter().map(|dir| dir.label.as_str()).collect()
    }

    #[test]
    fn a_flat_list_keeps_every_change_and_invents_no_folders() {
        let paths = ["b.rs", "a/deep/file.rs"];
        let numbered: Vec<(usize, &str)> = paths.iter().copied().enumerate().collect();

        let (rows, dirs) = rows(&numbered, true, &HashSet::new());

        assert!(dirs.is_empty(), "{WRONG_SHAPE}");
        assert_eq!(
            rows,
            vec![
                Row::Change { index: 1, depth: 0 },
                Row::Change { index: 0, depth: 0 },
            ],
            "{LOST_A_CHANGE}"
        );
    }

    #[test]
    fn a_chain_of_single_child_folders_is_compacted_into_one_row() {
        let (rows, dirs) = tree(&["src/scm/repo.rs"], &[]);

        assert_eq!(labels(&dirs), vec!["src/scm"], "{WRONG_SHAPE}");
        assert_eq!(
            rows,
            vec![Row::Directory(0), Row::Change { index: 0, depth: 1 }],
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
        let (rows, dirs) = tree(&["a.rs", "z/b.rs"], &[]);

        assert_eq!(labels(&dirs), vec!["z"], "{WRONG_SHAPE}");
        assert_eq!(
            rows,
            vec![
                Row::Directory(0),
                Row::Change { index: 1, depth: 1 },
                Row::Change { index: 0, depth: 0 },
            ],
            "{WRONG_SHAPE}"
        );
    }

    #[test]
    fn a_collapsed_folder_lists_itself_and_nothing_under_it() {
        let (rows, dirs) = tree(&["src/a.rs", "src/b.rs"], &["src"]);

        assert_eq!(rows, vec![Row::Directory(0)], "{WRONG_SHAPE}");
        assert!(!dirs[0].expanded, "{WRONG_SHAPE}");
    }

    #[test]
    fn siblings_that_share_a_prefix_stay_apart() {
        let (_, dirs) = tree(&["src/a.rs", "srcs/b.rs"], &[]);
        assert_eq!(labels(&dirs), vec!["src", "srcs"], "{WRONG_SHAPE}");
    }

    #[test]
    fn a_file_sorting_between_two_folder_entries_does_not_split_the_folder() {
        let (rows, dirs) = tree(&["src/b.rs", "src/b/c.rs"], &[]);

        assert_eq!(labels(&dirs), vec!["src", "b"], "{WRONG_SHAPE}");
        assert_eq!(rows.len(), 4, "{LOST_A_CHANGE}");
    }
}
