//! What a snapshot saw besides its files: the scope it covered and every path it left out.
//! Restore leaves a path alone unless both snapshots it compares cover it, so a file that was too
//! large to capture is never mistaken for one that did not exist.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{ObjectStore, StoreError};

/// The scope of a snapshot of the whole workspace.
pub const ROOT_SCOPE: &str = ".";
const META_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    NestedRepository,
    Mount,
    Special,
    Oversized,
    Unreadable,
    Unstable,
    Unrepresentable,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkippedPath {
    pub path: String,
    pub reason: SkipReason,
}

/// Counts of everything a capture skipped and a bounded sample of it, for display.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Skipped {
    pub counts: BTreeMap<SkipReason, u32>,
    pub samples: Vec<SkippedPath>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Meta {
    /// [`ROOT_SCOPE`] or the workspace-relative directory the capture was limited to.
    pub scope: String,
    /// Paths seen but not recorded. Nothing at or beneath one is ever changed by a restore.
    pub pruned: Vec<SkippedPath>,
    /// Paths the capture was told to leave out, with the same effect as `pruned`.
    pub exclusions: Vec<String>,
    pub skipped: Skipped,
    pub file_count: u64,
    pub total_bytes: u64,
}

#[derive(Serialize)]
struct StoredMetaRef<'a> {
    version: u32,
    meta: &'a Meta,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredMeta {
    version: u32,
    meta: Meta,
}

impl Meta {
    /// Canonical: field order is fixed and maps are ordered, so equal metadata encodes equally
    /// and a snapshot id depends only on content.
    pub(crate) fn encode(&self) -> Result<Vec<u8>, StoreError> {
        serde_json::to_vec(&StoredMetaRef {
            version: META_VERSION,
            meta: self,
        })
        .map_err(|_| StoreError::InvalidInput("metadata"))
    }

    pub(crate) fn uncovered(&self) -> impl Iterator<Item = &str> {
        self.pruned
            .iter()
            .map(|pruned| pruned.path.as_str())
            .chain(self.exclusions.iter().map(String::as_str))
    }
}

impl ObjectStore {
    pub fn meta(&self, id: &crate::SnapshotId) -> Result<Meta, StoreError> {
        let (_, meta) = self.snapshot_parts(id)?;
        self.read_meta(&meta)
    }

    pub(crate) fn read_meta(&self, oid: &crate::ObjectId) -> Result<Meta, StoreError> {
        let bytes = self.read_blob(oid)?;
        match serde_json::from_slice::<StoredMeta>(&bytes) {
            Ok(stored) if stored.version == META_VERSION => Ok(stored.meta),
            _ => Err(StoreError::Corrupt(*oid)),
        }
    }
}

/// Whether `path` is `scope` or beneath it.
#[must_use]
pub fn within(scope: &str, path: &str) -> bool {
    scope == ROOT_SCOPE
        || path
            .strip_prefix(scope)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// A workspace-relative, `/`-separated path with no empty, `.` or `..` component.
pub(crate) fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path
            .split('/')
            .all(|component| !matches!(component, "" | "." | "..") && !component.contains('\0'))
}

pub(crate) fn valid_scope(scope: &str) -> bool {
    scope == ROOT_SCOPE || valid_path(scope)
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test_case(ROOT_SCOPE, "a/b", true ; "the root covers everything")]
    #[test_case("a", "a", true ; "a scope covers itself")]
    #[test_case("a", "a/b", true ; "a scope covers its descendants")]
    #[test_case("a", "ab", false ; "a sibling sharing a prefix is outside")]
    #[test_case("a/b", "a", false ; "an ancestor is outside")]
    fn within_matches_whole_components(scope: &str, path: &str, expected: bool) {
        assert_eq!(within(scope, path), expected);
    }

    #[test_case("a/b.txt", true ; "a nested file")]
    #[test_case("", false ; "empty")]
    #[test_case("/a", false ; "absolute")]
    #[test_case("a//b", false ; "an empty component")]
    #[test_case("a/./b", false ; "a dot component")]
    #[test_case("a/../b", false ; "a parent component")]
    #[test_case("a/", false ; "a trailing separator")]
    #[test_case("a\0b", false ; "a nul byte")]
    fn only_normal_relative_paths_are_valid(path: &str, expected: bool) {
        assert_eq!(valid_path(path), expected);
    }
}
