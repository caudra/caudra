//! The comparison as one tree both panes read, so a row on the left is always
//! the same path as the row beside it on the right.

use std::collections::{BTreeMap, BTreeSet};
use std::iter::successors;

use super::{
    TransferEntry, TransferExclusion, TransferNodeKind, TransferScan, TransferSide, TransferStatus,
};

pub(super) const MAX_PATH_LENGTH: usize = 4_096;
const MAX_PATH_DEPTH: usize = 128;
const SEPARATOR: char = '/';
/// The character after [`SEPARATOR`], so a range that stops there holds
/// everything under a folder and nothing beside it.
const AFTER_SEPARATOR: char = '0';
const CURRENT_DIRECTORY: &str = ".";
const PARENT_DIRECTORY: &str = "..";
const BACKSLASH: char = '\\';

/// What a folder's own row cannot say about what is under it: how many
/// descendants differ, and how many could not be settled either way.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Summary {
    pub(super) changed: usize,
    pub(super) unknown: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Row {
    Entry(String),
    /// Stands under an unfolded folder that lists nothing, or not everything,
    /// and says why.
    Note(String),
}

impl Row {
    pub(super) fn path(&self) -> &str {
        match self {
            Self::Entry(path) | Self::Note(path) => path,
        }
    }

    pub(super) fn depth(&self) -> usize {
        match self {
            Self::Entry(path) => depth(path),
            Self::Note(folder) => depth(folder) + 1,
        }
    }
}

/// Why a folder shows no contents on one side. The sides are asked apart
/// because only one of them may be partial.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Note {
    Ignored,
    Protected,
    Pattern,
    Dotfile,
    Symlink,
    Repository,
    Special,
    Unsupported,
    NotScanned,
    Unknown,
    Empty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NoteAction {
    IncludeIgnored,
    IncludeDotfiles,
    CompareFolder,
}

impl Note {
    pub(super) fn text(self) -> &'static str {
        match self {
            Self::Ignored => "Ignored by .gitignore",
            Self::Protected => "Protected · never transferred",
            Self::Pattern => "Excluded by a transfer pattern",
            Self::Dotfile => "Skipped as a dotfile",
            Self::Symlink => "Symbolic link · never followed",
            Self::Repository => "Nested repository · never transferred",
            Self::Special => "Special file · never transferred",
            Self::Unsupported => "Unsupported · never transferred",
            Self::NotScanned => "Not fully scanned",
            Self::Unknown => "Contents unknown",
            Self::Empty => "Empty folder",
        }
    }

    pub(super) fn action(self) -> Option<NoteAction> {
        match self {
            Self::Ignored => Some(NoteAction::IncludeIgnored),
            Self::Dotfile => Some(NoteAction::IncludeDotfiles),
            Self::NotScanned | Self::Unknown => Some(NoteAction::CompareFolder),
            _ => None,
        }
    }

    /// The word a row carries for a reason it is never transferred, so the
    /// reason shows before the row is unfolded.
    pub(super) fn badge(self) -> Option<&'static str> {
        match self {
            Self::Ignored => Some("ignored"),
            Self::Protected => Some("protected"),
            Self::Pattern => Some("excluded"),
            Self::Dotfile => Some("skipped"),
            Self::Symlink => Some("symlink"),
            Self::Repository => Some("repository"),
            Self::Special => Some("special"),
            Self::Unsupported => Some("unsupported"),
            Self::NotScanned | Self::Unknown | Self::Empty => None,
        }
    }
}

#[derive(Default)]
pub(super) struct ComparisonTree {
    entries: BTreeMap<String, TransferEntry>,
    children: BTreeMap<String, Vec<String>>,
    summaries: BTreeMap<String, Summary>,
    /// Every path that can never be transferred, or sits under a folder that
    /// cannot, worked out once so a selection costs no walk up the tree.
    blocked: BTreeSet<String>,
    dropped: usize,
}

impl ComparisonTree {
    /// Keeps every row it can place and counts the rest, so one malformed row
    /// costs that row rather than the whole comparison. A row is dropped when
    /// its path is not a plain relative one, repeats an earlier row, or hangs
    /// under something that is not a folder.
    pub(super) fn new(entries: Vec<TransferEntry>) -> Self {
        let offered = entries.len();
        let mut valid = BTreeMap::new();
        for entry in entries {
            if valid_relative(&entry.path, false) {
                valid.entry(entry.path.clone()).or_insert(entry);
            }
        }
        let mut tree = Self::default();
        // Sorted, so a folder is always placed before anything under it.
        for (path, entry) in valid {
            let parent = parent(&path);
            if !parent.is_empty() && !tree.entries.get(parent).is_some_and(TransferEntry::is_dir) {
                continue;
            }
            if entry.status.blocked() || tree.blocked.contains(parent) {
                tree.blocked.insert(path.clone());
            }
            tree.children
                .entry(parent.to_owned())
                .or_default()
                .push(path.clone());
            tree.entries.insert(path, entry);
        }
        tree.dropped = offered - tree.entries.len();
        for (path, entry) in &tree.entries {
            let changed = usize::from(entry.status.changed());
            let unknown = usize::from(entry.status == TransferStatus::Incomplete || entry.unlisted);
            if changed + unknown == 0 {
                continue;
            }
            let mut ancestor = path.as_str();
            loop {
                ancestor = parent(ancestor);
                if let Some(summary) = tree.summaries.get_mut(ancestor) {
                    summary.changed += changed;
                    summary.unknown += unknown;
                } else {
                    tree.summaries
                        .insert(ancestor.to_owned(), Summary { changed, unknown });
                }
                if ancestor.is_empty() {
                    break;
                }
            }
        }
        let entries = &tree.entries;
        for children in tree.children.values_mut() {
            children.sort_by(|a, b| {
                entries[b]
                    .is_dir()
                    .cmp(&entries[a].is_dir())
                    .then_with(|| a.cmp(b))
            });
        }
        tree
    }

    pub(super) fn entry(&self, path: &str) -> Option<&TransferEntry> {
        self.entries.get(path)
    }

    pub(super) fn entries(&self) -> impl Iterator<Item = &TransferEntry> {
        self.entries.values()
    }

    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(super) fn dropped(&self) -> usize {
        self.dropped
    }

    pub(super) fn summary(&self, path: &str) -> Summary {
        self.summaries.get(path).cloned().unwrap_or_default()
    }

    /// The row at `path` and every row under it, in path order.
    pub(super) fn subtree<'a>(&'a self, path: &'a str) -> impl Iterator<Item = &'a TransferEntry> {
        let below = self
            .entries
            .range(format!("{path}{SEPARATOR}")..format!("{path}{AFTER_SEPARATOR}"))
            .map(|(_, entry)| entry);
        self.entries.get(path).into_iter().chain(below)
    }

    /// Whether the path itself or a folder above it can never be transferred,
    /// which no choice made further down can override.
    pub(super) fn blocked_ancestor(&self, path: &str) -> bool {
        self.blocked.contains(path)
    }

    /// What the changes-only filter keeps: anything that differs or could not
    /// be settled, and every folder on the way to one.
    fn differs(&self, path: &str) -> bool {
        let summary = self.summary(path);
        self.entries.get(path).is_some_and(|entry| {
            entry.status.changed() || entry.status == TransferStatus::Incomplete || entry.unlisted
        }) || summary.changed + summary.unknown > 0
    }

    pub(super) fn rows(&self, expanded: &BTreeSet<String>, changes_only: bool) -> Vec<Row> {
        let mut rows = Vec::new();
        self.push_rows("", expanded, changes_only, &mut rows);
        rows
    }

    fn push_rows(
        &self,
        folder: &str,
        expanded: &BTreeSet<String>,
        changes_only: bool,
        rows: &mut Vec<Row>,
    ) {
        for path in self.children.get(folder).into_iter().flatten() {
            if changes_only && !self.differs(path) {
                continue;
            }
            rows.push(Row::Entry(path.clone()));
            let entry = &self.entries[path];
            if !expanded.contains(path) || !entry.expandable() {
                continue;
            }
            self.push_rows(path, expanded, changes_only, rows);
            if entry.unlisted || !self.children.contains_key(path) {
                rows.push(Row::Note(path.clone()));
            }
        }
    }

    /// What an unfolded folder says on `side` in place of its contents, or
    /// nothing where that side has no such folder.
    pub(super) fn note(
        &self,
        folder: &str,
        side: TransferSide,
        scan: &TransferScan,
    ) -> Option<Note> {
        let entry = self.entries.get(folder)?;
        let kind = entry.kind(side)?;
        Some(match (entry.excluded, kind) {
            (Some(TransferExclusion::Gitignore), _) => Note::Ignored,
            (Some(TransferExclusion::Protected), _) => Note::Protected,
            (Some(TransferExclusion::Pattern), _) => Note::Pattern,
            (Some(TransferExclusion::Dotfile), _) => Note::Dotfile,
            (None, TransferNodeKind::Symlink) => Note::Symlink,
            (None, TransferNodeKind::Repository) => Note::Repository,
            (None, TransferNodeKind::Special) => Note::Special,
            _ if entry.status == TransferStatus::Excluded => Note::Pattern,
            _ if entry.status == TransferStatus::Unsupported => Note::Unsupported,
            (_, TransferNodeKind::File) => return None,
            _ if entry.unlisted => Note::NotScanned,
            _ if !scan.complete() => Note::Unknown,
            _ => Note::Empty,
        })
    }
}

/// A path relative to a root: no leading or trailing separator, no empty,
/// `.` or `..` component, and nothing a terminal or another platform would
/// read differently. `empty` admits the root itself.
pub(super) fn valid_relative(path: &str, empty: bool) -> bool {
    if path.is_empty() {
        return empty;
    }
    path.len() <= MAX_PATH_LENGTH
        && !path.contains(BACKSLASH)
        && !path.chars().any(char::is_control)
        && path.split(SEPARATOR).count() <= MAX_PATH_DEPTH
        && path
            .split(SEPARATOR)
            .all(|part| !part.is_empty() && part != CURRENT_DIRECTORY && part != PARENT_DIRECTORY)
}

pub(super) fn parent(path: &str) -> &str {
    path.rsplit_once(SEPARATOR).map_or("", |(parent, _)| parent)
}

pub(super) fn name(path: &str) -> &str {
    path.rsplit_once(SEPARATOR).map_or(path, |(_, name)| name)
}

pub(super) fn depth(path: &str) -> usize {
    path.matches(SEPARATOR).count()
}

/// `path` and every folder above it, nearest first.
pub(super) fn ancestors(path: &str) -> impl Iterator<Item = &str> {
    successors(Some(path), |path| Some(parent(path))).take_while(|path| !path.is_empty())
}

/// Where `path` sits under the row `folder`, or nothing when it is not
/// strictly inside it.
pub(super) fn relative_to<'a>(path: &'a str, folder: &str) -> Option<&'a str> {
    path.strip_prefix(folder)?.strip_prefix(SEPARATOR)
}

pub(super) fn join(root: &str, path: &str) -> String {
    if root.is_empty() || root == CURRENT_DIRECTORY {
        path.to_owned()
    } else {
        format!("{}{SEPARATOR}{path}", root.trim_end_matches(SEPARATOR))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use test_case::test_case;

    use super::{
        ComparisonTree, MAX_PATH_DEPTH, MAX_PATH_LENGTH, Note, Row, SEPARATOR, Summary, join,
        relative_to, valid_relative,
    };
    use crate::transfer::tests::{
        CHANGED_FILE, DOT, EMPTY_FOLDER, FILE_NAME, FOLDER, LOCAL_ROOT, NESTED, NESTED_FILE,
        REMOTE_ROOT, excluded, file, folder, partial, project, unlisted,
    };
    use crate::transfer::{
        TransferEntry, TransferExclusion, TransferNodeKind, TransferScan, TransferSide,
        TransferStatus,
    };

    const KEPT: &str = "kept.txt";
    const UNDER_FILE: &str = "kept.txt/folder";
    const UNDER_DROPPED: &str = "kept.txt/folder/child";
    const PART: &str = "a";
    const CHANGED_NAME: &str = "main.rs";
    const SIBLING_FILE: &str = "srcs/main.rs";
    const SLASHED_ROOT: &str = "/host/project/";
    const LOCAL_FOLDER_ROOT: &str = "/host/project/src";
    const REMOTE_FOLDER_ROOT: &str = "workspace/src";

    fn listed(paths: &[&str]) -> Vec<Row> {
        paths
            .iter()
            .map(|path| Row::Entry((*path).to_owned()))
            .collect()
    }

    fn deep_path(depth: usize) -> String {
        vec![PART; depth].join(&SEPARATOR.to_string())
    }

    /// Something other than a folder standing where `src` is, on both sides.
    fn standing_in(kind: TransferNodeKind) -> TransferEntry {
        TransferEntry {
            local: Some(kind),
            remote: Some(kind),
            ..folder(FOLDER, TransferStatus::Unsupported)
        }
    }

    #[test_case(DOT; "root")]
    #[test_case(""; "empty")]
    #[test_case("/etc/passwd"; "absolute")]
    #[test_case("src/"; "trailing_separator")]
    #[test_case("src//main.rs"; "empty_component")]
    #[test_case("src/./main.rs"; "current_directory")]
    #[test_case("src/../main.rs"; "parent_directory")]
    #[test_case("src\\main.rs"; "backslash")]
    #[test_case("src/\u{1b}[2J"; "control_character")]
    #[test_case(KEPT; "duplicate")]
    #[test_case("missing/main.rs"; "missing_parent")]
    #[test_case(UNDER_FILE; "file_parent")]
    fn a_row_that_cannot_be_placed_costs_only_itself(path: &str) {
        let tree = ComparisonTree::new(vec![
            file(KEPT, TransferStatus::LocalOnly),
            file(path, TransferStatus::LocalOnly),
        ]);
        assert_eq!(tree.dropped(), 1);
        assert_eq!(tree.rows(&BTreeSet::new(), false), listed(&[KEPT]));
    }

    #[test]
    fn rows_under_a_dropped_row_are_counted_one_by_one() {
        let tree = ComparisonTree::new(vec![
            file(KEPT, TransferStatus::LocalOnly),
            folder(UNDER_FILE, TransferStatus::LocalOnly),
            file(UNDER_DROPPED, TransferStatus::LocalOnly),
        ]);
        assert_eq!(tree.dropped(), 2);
        assert_eq!(tree.len(), 1);
    }

    #[test_case(&deep_path(MAX_PATH_DEPTH), true; "deepest")]
    #[test_case(&deep_path(MAX_PATH_DEPTH + 1), false; "too_deep")]
    #[test_case(&PART.repeat(MAX_PATH_LENGTH), true; "longest")]
    #[test_case(&PART.repeat(MAX_PATH_LENGTH + 1), false; "too_long")]
    fn paths_stay_within_the_depth_and_length_bounds(path: &str, valid: bool) {
        assert_eq!(valid_relative(path, false), valid);
    }

    #[test_case(file(CHANGED_FILE, TransferStatus::Different), Summary { changed: 1, unknown: 0 }; "changed_child")]
    #[test_case(file(CHANGED_FILE, TransferStatus::TypeConflict), Summary { changed: 1, unknown: 0 }; "type_conflict_child")]
    #[test_case(file(CHANGED_FILE, TransferStatus::Incomplete), Summary { changed: 0, unknown: 1 }; "incomplete_child")]
    #[test_case(unlisted(NESTED, TransferStatus::Equal), Summary { changed: 0, unknown: 1 }; "unlisted_child")]
    #[test_case(file(CHANGED_FILE, TransferStatus::Equal), Summary::default(); "equal_child")]
    fn a_matching_folder_does_not_imply_a_matching_subtree(
        child: TransferEntry,
        expected: Summary,
    ) {
        let tree = ComparisonTree::new(vec![folder(FOLDER, TransferStatus::Equal), child]);
        assert_eq!(tree.summary(FOLDER), expected);
        assert_eq!(tree.summary(""), expected);
    }

    #[test]
    fn folders_list_first_and_unfold_in_place() {
        let tree = ComparisonTree::new(project());
        assert_eq!(
            tree.rows(&BTreeSet::new(), false),
            listed(&[EMPTY_FOLDER, FOLDER, FILE_NAME])
        );
        let expanded = BTreeSet::from([FOLDER.to_owned(), NESTED.to_owned()]);
        assert_eq!(
            tree.rows(&expanded, false),
            listed(&[
                EMPTY_FOLDER,
                FOLDER,
                NESTED,
                NESTED_FILE,
                CHANGED_FILE,
                FILE_NAME
            ])
        );
    }

    #[test]
    fn an_unfolded_folder_listing_nothing_or_not_everything_gets_a_note() {
        let tree = ComparisonTree::new(vec![
            folder(EMPTY_FOLDER, TransferStatus::Equal),
            unlisted(FOLDER, TransferStatus::Equal),
            file(CHANGED_FILE, TransferStatus::Different),
        ]);
        let expanded = BTreeSet::from([EMPTY_FOLDER.to_owned(), FOLDER.to_owned()]);
        assert_eq!(
            tree.rows(&expanded, false),
            [
                Row::Entry(EMPTY_FOLDER.to_owned()),
                Row::Note(EMPTY_FOLDER.to_owned()),
                Row::Entry(FOLDER.to_owned()),
                Row::Entry(CHANGED_FILE.to_owned()),
                Row::Note(FOLDER.to_owned()),
            ]
        );
    }

    #[test_case(excluded(FOLDER, TransferExclusion::Gitignore), true, Note::Ignored; "gitignored")]
    #[test_case(excluded(FOLDER, TransferExclusion::Protected), true, Note::Protected; "protected")]
    #[test_case(excluded(FOLDER, TransferExclusion::Pattern), true, Note::Pattern; "pattern")]
    #[test_case(excluded(FOLDER, TransferExclusion::Dotfile), true, Note::Dotfile; "dotfile")]
    #[test_case(folder(FOLDER, TransferStatus::Excluded), true, Note::Pattern; "excluded_without_reason")]
    #[test_case(standing_in(TransferNodeKind::Symlink), true, Note::Symlink; "symlink")]
    #[test_case(standing_in(TransferNodeKind::Repository), true, Note::Repository; "repository")]
    #[test_case(standing_in(TransferNodeKind::Special), true, Note::Special; "special")]
    #[test_case(folder(FOLDER, TransferStatus::Unsupported), true, Note::Unsupported; "unsupported")]
    #[test_case(unlisted(FOLDER, TransferStatus::Equal), true, Note::NotScanned; "not_scanned")]
    #[test_case(folder(FOLDER, TransferStatus::Equal), false, Note::Unknown; "side_incomplete")]
    #[test_case(folder(FOLDER, TransferStatus::Equal), true, Note::Empty; "empty")]
    fn a_folder_says_why_it_lists_nothing(entry: TransferEntry, complete: bool, expected: Note) {
        let tree = ComparisonTree::new(vec![entry]);
        let scan = match complete {
            true => TransferScan::default(),
            false => partial(),
        };
        assert_eq!(
            tree.note(FOLDER, TransferSide::Local, &scan),
            Some(expected)
        );
    }

    #[test]
    fn a_side_without_the_folder_has_nothing_to_say() {
        let tree = ComparisonTree::new(vec![unlisted(FOLDER, TransferStatus::LocalOnly)]);
        let scan = TransferScan::default();
        assert_eq!(tree.note(FOLDER, TransferSide::Remote, &scan), None);
        assert_eq!(
            tree.note(FOLDER, TransferSide::Local, &scan),
            Some(Note::NotScanned)
        );
    }

    #[test_case(CHANGED_FILE, FOLDER, Some(CHANGED_NAME); "inside")]
    #[test_case(FOLDER, FOLDER, None; "itself")]
    #[test_case(SIBLING_FILE, FOLDER, None; "sibling_sharing_a_prefix")]
    fn relative_to_answers_only_strictly_inside(path: &str, within: &str, expected: Option<&str>) {
        assert_eq!(relative_to(path, within), expected);
    }

    #[test_case("", FOLDER; "workspace")]
    #[test_case(DOT, FOLDER; "workspace_as_dot")]
    #[test_case(SLASHED_ROOT, LOCAL_FOLDER_ROOT; "trailing_separator")]
    #[test_case(LOCAL_ROOT, LOCAL_FOLDER_ROOT; "absolute_root")]
    #[test_case(REMOTE_ROOT, REMOTE_FOLDER_ROOT; "relative_root")]
    fn join_puts_a_folder_under_a_root(root: &str, expected: &str) {
        assert_eq!(join(root, FOLDER), expected);
    }
}
