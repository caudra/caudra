//! Snapshot trees. A snapshot is a wrapper tree `{files, meta}`; `files` holds every captured path
//! as git would, so unchanged directories are shared objects and a diff skips them whole.

use std::{cmp::Ordering, collections::HashSet};

use gix::{
    bstr::BStr,
    objs::{
        Kind, Tree, TreeRefIter, WriteTo as _,
        tree::{Entry as TreeEntry, EntryKind as TreeEntryKind},
    },
};

use crate::{
    ObjectId, ObjectStore, SnapshotId, StoreError,
    meta::{Meta, valid_path, valid_scope, within},
};

const FILES: &str = "files";
const META: &str = "meta";
/// Deeper than any path a walker admits, so only a damaged tree reaches it.
const MAX_TREE_DEPTH: usize = 1_024;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum EntryKind {
    File,
    Executable,
    /// The link itself: its blob holds the raw target, which is never followed.
    Symlink,
}

/// What a snapshot holds at one path.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Content {
    pub kind: EntryKind,
    pub oid: ObjectId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    /// Workspace-relative and `/`-separated.
    pub path: String,
    pub content: Content,
}

/// One path both snapshots cover whose content differs between them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Change {
    pub path: String,
    pub source: Option<Content>,
    pub target: Option<Content>,
}

#[derive(Debug)]
pub struct Changes {
    /// The deeper of the two snapshots' scopes, where both looked.
    pub scope: String,
    /// In path byte order.
    pub changes: Vec<Change>,
}

pub(crate) struct Node {
    name: String,
    pub(crate) oid: ObjectId,
    kind: Option<EntryKind>,
}

impl Node {
    pub(crate) fn is_tree(&self) -> bool {
        self.kind.is_none()
    }

    fn content(&self) -> Option<Content> {
        self.kind.map(|kind| Content {
            kind,
            oid: self.oid,
        })
    }
}

/// Paths both snapshots cover: within the deeper scope, and neither pruned nor excluded, nor
/// beneath a path that is, in either snapshot.
struct Coverage<'a> {
    scope: &'a str,
    uncovered: HashSet<&'a str>,
}

impl Coverage<'_> {
    fn covers(&self, path: &str) -> bool {
        within(self.scope, path) && !self.blocked(path)
    }

    fn reaches(&self, directory: &str) -> bool {
        (within(self.scope, directory) || within(directory, self.scope)) && !self.blocked(directory)
    }

    fn blocked(&self, path: &str) -> bool {
        std::iter::successors(Some(path), |path| {
            path.rsplit_once('/').map(|(parent, _)| parent)
        })
        .any(|path| self.uncovered.contains(path))
    }
}

impl ObjectStore {
    /// Stores a snapshot of `entries`, which must be in strictly increasing path byte order, and
    /// returns its id. Subtrees already present are found, not rewritten.
    pub fn build(&self, entries: &[Entry], meta: &Meta) -> Result<SnapshotId, StoreError> {
        if !valid_scope(&meta.scope) {
            return Err(StoreError::InvalidInput("snapshot scope"));
        }
        if u64::try_from(entries.len()).ok() != Some(meta.file_count) {
            return Err(StoreError::InvalidInput("file count"));
        }
        for (index, entry) in entries.iter().enumerate() {
            if !valid_path(&entry.path) || !within(&meta.scope, &entry.path) {
                return Err(StoreError::InvalidInput("entry path"));
            }
            if index > 0 && entries[index - 1].path.as_bytes() >= entry.path.as_bytes() {
                return Err(StoreError::InvalidInput("entry order"));
            }
        }
        let files = self.write_tree(entries, 0, 0)?;
        let meta = self.write_object(Kind::Blob, &meta.encode()?)?.oid;
        let wrapper = self.write_tree_object(Tree {
            entries: vec![
                tree_entry(FILES, TreeEntryKind::Tree, files),
                tree_entry(META, TreeEntryKind::Blob, meta),
            ],
        })?;
        Ok(SnapshotId(wrapper))
    }

    /// Every entry of a snapshot in path byte order.
    pub fn entries(&self, id: &SnapshotId) -> Result<Vec<Entry>, StoreError> {
        let (files, meta) = self.snapshot_parts(id)?;
        let file_count = self.read_meta(&meta)?.file_count;
        let mut entries = Vec::new();
        self.collect_entries(&files, "", 0, file_count, &mut entries)?;
        if entries.len() as u64 != file_count {
            return Err(StoreError::Corrupt(id.0));
        }
        Ok(entries)
    }

    /// The paths whose content differs from `source` to `target` among those both cover. Renames
    /// are not tracked: a moved file is one removal and one addition.
    pub fn changes(&self, source: &SnapshotId, target: &SnapshotId) -> Result<Changes, StoreError> {
        let (source_files, source_meta) = self.snapshot_parts(source)?;
        let (target_files, target_meta) = self.snapshot_parts(target)?;
        let (source_meta, target_meta) =
            (self.read_meta(&source_meta)?, self.read_meta(&target_meta)?);
        let scope = if within(&source_meta.scope, &target_meta.scope) {
            &target_meta.scope
        } else if within(&target_meta.scope, &source_meta.scope) {
            &source_meta.scope
        } else {
            return Err(StoreError::DisjointScopes);
        };
        let coverage = Coverage {
            scope,
            uncovered: source_meta
                .uncovered()
                .chain(target_meta.uncovered())
                .collect(),
        };
        let mut diff = Diff {
            store: self,
            coverage: &coverage,
            bound: source_meta
                .file_count
                .saturating_add(target_meta.file_count),
            corrupt: source.0,
            changes: Vec::new(),
        };
        diff.trees("", Some(source_files), Some(target_files), 0)?;
        Ok(Changes {
            scope: scope.clone(),
            changes: diff.changes,
        })
    }

    pub(crate) fn snapshot_parts(
        &self,
        id: &SnapshotId,
    ) -> Result<(ObjectId, ObjectId), StoreError> {
        let data = self.read_object(&id.0, Kind::Tree)?;
        let (mut files, mut meta) = (None, None);
        for entry in TreeRefIter::from_bytes(&data, crate::HASH_KIND) {
            let entry = entry.map_err(|_| StoreError::Corrupt(id.0))?;
            let name: &[u8] = entry.filename;
            let slot = match entry.mode.kind() {
                TreeEntryKind::Tree if name == FILES.as_bytes() => &mut files,
                TreeEntryKind::Blob if name == META.as_bytes() => &mut meta,
                _ => return Err(StoreError::Corrupt(id.0)),
            };
            if slot.replace(entry.oid.to_owned()).is_some() {
                return Err(StoreError::Corrupt(id.0));
            }
        }
        files.zip(meta).ok_or(StoreError::Corrupt(id.0))
    }

    pub(crate) fn read_tree(&self, oid: &ObjectId) -> Result<Vec<Node>, StoreError> {
        let data = self.read_object(oid, Kind::Tree)?;
        TreeRefIter::from_bytes(&data, crate::HASH_KIND)
            .map(|entry| {
                let entry = entry.ok().ok_or(StoreError::Corrupt(*oid))?;
                let name = std::str::from_utf8(entry.filename)
                    .ok()
                    .filter(|name| !name.contains('/') && valid_path(name))
                    .ok_or(StoreError::Corrupt(*oid))?;
                let kind = match entry.mode.kind() {
                    TreeEntryKind::Tree => None,
                    TreeEntryKind::Blob => Some(EntryKind::File),
                    TreeEntryKind::BlobExecutable => Some(EntryKind::Executable),
                    TreeEntryKind::Link => Some(EntryKind::Symlink),
                    TreeEntryKind::Commit => return Err(StoreError::Corrupt(*oid)),
                };
                Ok(Node {
                    name: name.to_owned(),
                    oid: entry.oid.to_owned(),
                    kind,
                })
            })
            .collect()
    }

    /// Writes the tree for `entries`, whose paths all share their first `offset` bytes, and
    /// returns its id.
    fn write_tree(
        &self,
        entries: &[Entry],
        offset: usize,
        depth: usize,
    ) -> Result<ObjectId, StoreError> {
        if depth > MAX_TREE_DEPTH {
            return Err(StoreError::InvalidInput("path depth"));
        }
        let mut tree = Tree::empty();
        let mut index = 0;
        while let Some(entry) = entries.get(index) {
            let rest = &entry.path[offset..];
            let Some((directory, _)) = rest.split_once('/') else {
                tree.entries.push(tree_entry(
                    rest,
                    tree_kind(entry.content.kind),
                    entry.content.oid,
                ));
                index += 1;
                continue;
            };
            // A file of the same name sorts before the directory's entries, possibly with names
            // that extend it by a byte below `/` in between, and every one of those extends it.
            if tree
                .entries
                .iter()
                .rev()
                .take_while(|earlier| earlier.filename.starts_with(directory.as_bytes()))
                .any(|earlier| earlier.filename == directory.as_bytes())
            {
                return Err(StoreError::InvalidInput(
                    "a path is both a file and a directory",
                ));
            }
            let prefix = &entry.path.as_bytes()[..offset + directory.len() + 1];
            let end = index
                + entries[index..]
                    .iter()
                    .take_while(|entry| entry.path.as_bytes().starts_with(prefix))
                    .count();
            let oid = self.write_tree(&entries[index..end], prefix.len(), depth + 1)?;
            tree.entries
                .push(tree_entry(directory, TreeEntryKind::Tree, oid));
            index = end;
        }
        self.write_tree_object(tree)
    }

    fn write_tree_object(&self, mut tree: Tree) -> Result<ObjectId, StoreError> {
        tree.entries.sort();
        let mut bytes = Vec::new();
        tree.write_to(&mut bytes)
            .map_err(|_| StoreError::InvalidInput("tree entry name"))?;
        Ok(self.write_object(Kind::Tree, &bytes)?.oid)
    }

    fn collect_entries(
        &self,
        tree: &ObjectId,
        prefix: &str,
        depth: usize,
        bound: u64,
        entries: &mut Vec<Entry>,
    ) -> Result<(), StoreError> {
        if depth > MAX_TREE_DEPTH {
            return Err(StoreError::Corrupt(*tree));
        }
        for node in self.read_tree(tree)? {
            let path = join(prefix, &node.name);
            match node.content() {
                None => self.collect_entries(&node.oid, &path, depth + 1, bound, entries)?,
                Some(content) => {
                    if entries.len() as u64 >= bound {
                        return Err(StoreError::Corrupt(*tree));
                    }
                    entries.push(Entry { path, content });
                }
            }
        }
        Ok(())
    }
}

struct Diff<'a> {
    store: &'a ObjectStore,
    coverage: &'a Coverage<'a>,
    bound: u64,
    corrupt: ObjectId,
    changes: Vec<Change>,
}

impl Diff<'_> {
    fn trees(
        &mut self,
        prefix: &str,
        source: Option<ObjectId>,
        target: Option<ObjectId>,
        depth: usize,
    ) -> Result<(), StoreError> {
        if source == target {
            return Ok(());
        }
        if depth > MAX_TREE_DEPTH {
            return Err(StoreError::Corrupt(self.corrupt));
        }
        let read = |oid: Option<ObjectId>| {
            oid.map_or_else(|| Ok(Vec::new()), |oid| self.store.read_tree(&oid))
        };
        let mut sources = read(source)?.into_iter().peekable();
        let mut targets = read(target)?.into_iter().peekable();
        loop {
            let (source, target) = match (sources.peek(), targets.peek()) {
                (None, None) => return Ok(()),
                (Some(_), None) => (sources.next(), None),
                (None, Some(_)) => (None, targets.next()),
                (Some(left), Some(right)) => match tree_order(left, right) {
                    Ordering::Less => (sources.next(), None),
                    Ordering::Greater => (None, targets.next()),
                    Ordering::Equal => (sources.next(), targets.next()),
                },
            };
            let Some(node) = source.as_ref().or(target.as_ref()) else {
                return Ok(());
            };
            let path = join(prefix, &node.name);
            if node.is_tree() {
                if self.coverage.reaches(&path) {
                    self.trees(
                        &path,
                        source.map(|node| node.oid),
                        target.map(|node| node.oid),
                        depth + 1,
                    )?;
                }
                continue;
            }
            let (source, target) = (
                source.and_then(|node| node.content()),
                target.and_then(|node| node.content()),
            );
            if source == target || !self.coverage.covers(&path) {
                continue;
            }
            if self.changes.len() as u64 >= self.bound {
                return Err(StoreError::Corrupt(self.corrupt));
            }
            self.changes.push(Change {
                path,
                source,
                target,
            });
        }
    }
}

/// Git's tree order: names compare as bytes, a tree's as if it ended in `/`. A file and a
/// directory of the same name are therefore different entries, as they must be when one replaced
/// the other.
fn tree_order(left: &Node, right: &Node) -> Ordering {
    let (left_name, right_name) = (left.name.as_bytes(), right.name.as_bytes());
    let common = left_name.len().min(right_name.len());
    left_name[..common]
        .cmp(&right_name[..common])
        .then_with(|| {
            let next = |name: &[u8], node: &Node| {
                name.get(common)
                    .copied()
                    .or_else(|| node.is_tree().then_some(b'/'))
            };
            next(left_name, left).cmp(&next(right_name, right))
        })
}

fn tree_kind(kind: EntryKind) -> TreeEntryKind {
    match kind {
        EntryKind::File => TreeEntryKind::Blob,
        EntryKind::Executable => TreeEntryKind::BlobExecutable,
        EntryKind::Symlink => TreeEntryKind::Link,
    }
}

fn tree_entry(name: &str, kind: TreeEntryKind, oid: ObjectId) -> TreeEntry {
    TreeEntry {
        mode: kind.into(),
        filename: BStr::new(name).to_owned(),
        oid,
    }
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}/{name}")
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use test_case::test_case;

    use super::*;
    use crate::{
        Skipped,
        meta::{ROOT_SCOPE, SkipReason, SkippedPath},
        tests::store,
    };

    const NOT_COVERED: &str = "a path either snapshot left out must never change";

    pub(crate) fn meta(scope: &str, entries: &[Entry]) -> Meta {
        Meta {
            scope: scope.to_owned(),
            pruned: Vec::new(),
            exclusions: Vec::new(),
            skipped: Skipped::default(),
            file_count: entries.len() as u64,
            total_bytes: 0,
        }
    }

    pub(crate) fn entry(store: &ObjectStore, path: &str, kind: EntryKind, data: &str) -> Entry {
        Entry {
            path: path.to_owned(),
            content: Content {
                kind,
                oid: store.write_blob(data.as_bytes()).unwrap().oid,
            },
        }
    }

    fn files(store: &ObjectStore, files: &[(&str, &str)]) -> Vec<Entry> {
        files
            .iter()
            .map(|(path, data)| entry(store, path, EntryKind::File, data))
            .collect()
    }

    fn paths(changes: &Changes) -> Vec<&str> {
        changes
            .changes
            .iter()
            .map(|change| change.path.as_str())
            .collect()
    }

    #[test]
    fn a_snapshot_round_trips_its_entries_and_metadata_under_a_content_derived_id() {
        let (_dir, store) = store(false);
        let entries = vec![
            entry(&store, "a.txt", EntryKind::File, "a"),
            entry(&store, "bin/run", EntryKind::Executable, "#!/bin/sh"),
            entry(&store, "bin/tool", EntryKind::Symlink, "run"),
            entry(&store, "deep/er/still/file", EntryKind::File, "d"),
        ];
        let mut meta = meta(ROOT_SCOPE, &entries);
        meta.pruned.push(SkippedPath {
            path: "big.bin".to_owned(),
            reason: SkipReason::Oversized,
        });
        meta.skipped.counts.insert(SkipReason::Oversized, 1);
        let id = store.build(&entries, &meta).unwrap();
        assert_eq!(store.build(&entries, &meta).unwrap(), id);
        store.sync().unwrap();
        assert_eq!(store.entries(&id).unwrap(), entries);
        assert_eq!(store.meta(&id).unwrap(), meta);
        assert_eq!(store.read_blob(&entries[2].content.oid).unwrap(), b"run");
        let mut other = meta.clone();
        other.total_bytes = 1;
        assert_ne!(store.build(&entries, &other).unwrap(), id);
    }

    #[test]
    fn an_empty_snapshot_is_valid() {
        let (_dir, store) = store(false);
        let id = store.build(&[], &meta(ROOT_SCOPE, &[])).unwrap();
        store.sync().unwrap();
        assert!(store.entries(&id).unwrap().is_empty());
    }

    #[test_case(&["b", "a"] ; "out of order")]
    #[test_case(&["a", "a"] ; "duplicated")]
    #[test_case(&["a", "a/b"] ; "a file that is also a directory")]
    #[test_case(&["a", "a.c", "a/b"] ; "a file that is also a directory with a name sorting between them")]
    #[test_case(&["a/../b"] ; "a parent component")]
    #[test_case(&["/a"] ; "absolute")]
    fn build_refuses_paths_git_could_not_represent(paths: &[&str]) {
        let (_dir, store) = store(false);
        let entries: Vec<Entry> = paths
            .iter()
            .map(|path| entry(&store, path, EntryKind::File, path))
            .collect();
        assert!(matches!(
            store.build(&entries, &meta(ROOT_SCOPE, &entries)),
            Err(StoreError::InvalidInput(_))
        ));
    }

    #[test]
    fn build_refuses_an_entry_outside_its_scope() {
        let (_dir, store) = store(false);
        let entries = files(&store, &[("b/file", "x")]);
        assert!(store.build(&entries, &meta("a", &entries)).is_err());
    }

    #[test]
    fn changes_are_every_differing_path_in_byte_order() {
        let (_dir, store) = store(false);
        let source = files(
            &store,
            &[
                ("a", "file"),
                ("a.b", "same"),
                ("dir/x", "old"),
                ("gone", "g"),
                ("same/y", "y"),
            ],
        );
        let target = vec![
            entry(&store, "a.b", EntryKind::File, "same"),
            entry(&store, "a/child", EntryKind::File, "new"),
            entry(&store, "dir/x", EntryKind::Executable, "old"),
            entry(&store, "same/y", EntryKind::File, "y"),
        ];
        let source = store.build(&source, &meta(ROOT_SCOPE, &source)).unwrap();
        let target = store.build(&target, &meta(ROOT_SCOPE, &target)).unwrap();
        store.sync().unwrap();
        let changes = store.changes(&source, &target).unwrap();
        assert_eq!(paths(&changes), ["a", "a/child", "dir/x", "gone"]);
        assert!(changes.changes[0].target.is_none());
        assert!(changes.changes[1].source.is_none());
        assert_eq!(
            changes.changes[2].target.map(|content| content.kind),
            Some(EntryKind::Executable)
        );
        assert!(store.changes(&source, &source).unwrap().changes.is_empty());
    }

    #[test]
    fn a_path_pruned_in_either_snapshot_is_left_alone() {
        let (_dir, store) = store(false);
        let captured = files(&store, &[("big.bin", "small"), ("kept", "1")]);
        let pruned = files(&store, &[("kept", "2")]);
        let mut pruned_meta = meta(ROOT_SCOPE, &pruned);
        pruned_meta.pruned.push(SkippedPath {
            path: "big.bin".to_owned(),
            reason: SkipReason::Oversized,
        });
        let captured = store
            .build(&captured, &meta(ROOT_SCOPE, &captured))
            .unwrap();
        let pruned = store.build(&pruned, &pruned_meta).unwrap();
        store.sync().unwrap();
        assert_eq!(
            paths(&store.changes(&captured, &pruned).unwrap()),
            ["kept"],
            "{NOT_COVERED}"
        );
        assert_eq!(
            paths(&store.changes(&pruned, &captured).unwrap()),
            ["kept"],
            "{NOT_COVERED}"
        );
    }

    #[test_case("vendor" ; "the directory itself")]
    #[test_case("vendor/nested" ; "a directory beneath it")]
    fn nothing_beneath_an_excluded_directory_changes(excluded: &str) {
        let (_dir, store) = store(false);
        let source = files(&store, &[("src", "1"), ("vendor/nested/lib", "old")]);
        let target = files(&store, &[("src", "2"), ("vendor/nested/new", "new")]);
        let mut target_meta = meta(ROOT_SCOPE, &target);
        target_meta.exclusions.push(excluded.to_owned());
        let source = store.build(&source, &meta(ROOT_SCOPE, &source)).unwrap();
        let target = store.build(&target, &target_meta).unwrap();
        store.sync().unwrap();
        assert_eq!(
            paths(&store.changes(&source, &target).unwrap()),
            ["src"],
            "{NOT_COVERED}"
        );
        assert_eq!(
            paths(&store.changes(&target, &source).unwrap()),
            ["src"],
            "{NOT_COVERED}"
        );
    }

    #[test]
    fn nested_scopes_compare_within_the_deeper_one_and_disjoint_scopes_are_refused() {
        let (_dir, store) = store(false);
        let whole = files(&store, &[("a/in", "1"), ("b/out", "1")]);
        let part = files(&store, &[("a/in", "2")]);
        let other = files(&store, &[("b/out", "2")]);
        let whole = store.build(&whole, &meta(ROOT_SCOPE, &whole)).unwrap();
        let part = store.build(&part, &meta("a", &part)).unwrap();
        let other = store.build(&other, &meta("b", &other)).unwrap();
        store.sync().unwrap();
        let changes = store.changes(&whole, &part).unwrap();
        assert_eq!(changes.scope, "a");
        assert_eq!(paths(&changes), ["a/in"]);
        assert!(matches!(
            store.changes(&part, &other),
            Err(StoreError::DisjointScopes)
        ));
    }

    #[test]
    fn a_missing_snapshot_is_reported_as_missing() {
        let (_dir, store) = store(false);
        let id = SnapshotId(crate::blob_id(b"never stored").unwrap());
        assert!(matches!(store.entries(&id), Err(StoreError::Missing(_))));
    }
}
