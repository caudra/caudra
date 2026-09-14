use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::mem::size_of;
use std::path::PathBuf;

use crate::text::FileVersion;

/// Serde helpers keeping the traversal disclosure fields off the wire when they
/// carry no information, so an ordinary listing looks exactly as it did before.
const fn yes() -> bool {
    true
}

fn is_yes(value: &bool) -> bool {
    *value
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

/// Resource bounds are deliberately independent so deployments can tighten one
/// attack surface without unexpectedly changing another operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FilesystemLimits {
    pub max_read_bytes: usize,
    pub max_file_bytes: usize,
    pub max_line_length: usize,
    pub max_read_lines: usize,
    pub max_search_results: usize,
    pub max_traversal_entries: usize,
    pub max_write_bytes: usize,
    pub max_patch_bytes: usize,
    pub max_patch_files: usize,
    pub max_patch_plan_bytes: usize,
    pub max_regex_length: usize,
    pub max_glob_bytes: usize,
    pub max_glob_brace_depth: usize,
    pub max_glob_alternatives: usize,
    pub max_glob_generated_bytes: usize,
    pub max_glob_match_steps: usize,
    pub max_diff_bytes: usize,
    /// How large a file may be for a write to carry the content it replaced.
    ///
    /// A caller that draws the write as a diff needs the old side, which the patch alone does not
    /// supply. Over this bound the write carries nothing and the caller falls back to the patch.
    pub max_previous_bytes: usize,
    pub max_patch_result_bytes: usize,
    /// Whether broad traversal applies `.gitignore` rules.
    ///
    /// On by default, because a search that spends its budget on build output answers a question
    /// nobody asked. An explicitly named path is never excluded, so a caller can still reach an
    /// ignored tree by naming it.
    pub honor_gitignore: bool,
    /// Whether broad traversal stops at a directory that is itself a repository.
    ///
    /// Off by default. The code graph turns it on because a vendored repository's symbols compete
    /// with the host project's in one rank vector; a text search has no such problem and keeps
    /// descending.
    pub prune_nested_repositories: bool,
    pub max_gitignore_files: usize,
    pub max_gitignore_bytes: usize,
    pub max_gitignore_patterns: usize,
    pub max_gitignore_retained_bytes: usize,
    pub max_gitignore_match_steps: usize,
}

impl Default for FilesystemLimits {
    fn default() -> Self {
        Self {
            max_read_bytes: 50 * 1024,
            max_file_bytes: 5 * 1024 * 1024,
            max_line_length: 2_000,
            max_read_lines: 2_000,
            // Search results are additionally bounded by the protocol result
            // ceiling, so this count is a coarse cap rather than the binding
            // constraint for wide rows.
            max_search_results: 500,
            // Sized from measured traversal cost of roughly 7 microseconds per
            // entry, so an exhaustive scan of a very large tree stays near one
            // second. Ordinary repositories finish well inside this budget once
            // build output is skipped.
            max_traversal_entries: 50_000,
            max_write_bytes: 5 * 1024 * 1024,
            max_patch_bytes: 1024 * 1024,
            max_patch_files: 100,
            max_patch_plan_bytes: 32 * 1024 * 1024,
            max_regex_length: 1_000,
            max_glob_bytes: 4 * 1024,
            max_glob_brace_depth: 8,
            max_glob_alternatives: 256,
            max_glob_generated_bytes: 64 * 1024,
            // A whole-operation work bound, not a per-pattern one. Sized so the
            // heaviest ordinary pattern can evaluate every candidate the
            // traversal budget admits; a pathological pattern exhausts it and
            // truncates instead of failing.
            max_glob_match_steps: 400_000_000,
            max_diff_bytes: 16 * 1024,
            // Room for any ordinary source file or document, far under `max_file_bytes`. A file
            // larger than this is one whose old side costs more to carry than the diff is worth.
            max_previous_bytes: 1024 * 1024,
            max_patch_result_bytes: 4 * 1024 * 1024,
            honor_gitignore: true,
            prune_nested_repositories: false,
            // One ignore file per directory of a large repository, with room to spare. A tree that
            // needs more than this is one where the rules themselves are the traversal cost.
            max_gitignore_files: 1_000,
            max_gitignore_bytes: 64 * 1024,
            max_gitignore_patterns: 1_000,
            max_gitignore_retained_bytes: 1024 * 1024,
            // A whole-traversal budget, like `max_glob_match_steps`. Exhausting it stops rule
            // evaluation and reports incomplete ignore data rather than failing the listing.
            max_gitignore_match_steps: 400_000_000,
        }
    }
}

impl FilesystemLimits {
    pub(crate) fn validate(self) -> Result<Self, crate::FilesystemError> {
        for (name, value) in [
            ("maxReadBytes", self.max_read_bytes),
            ("maxFileBytes", self.max_file_bytes),
            ("maxLineLength", self.max_line_length),
            ("maxReadLines", self.max_read_lines),
            ("maxSearchResults", self.max_search_results),
            ("maxTraversalEntries", self.max_traversal_entries),
            ("maxWriteBytes", self.max_write_bytes),
            ("maxPatchBytes", self.max_patch_bytes),
            ("maxPatchFiles", self.max_patch_files),
            ("maxPatchPlanBytes", self.max_patch_plan_bytes),
            ("maxRegexLength", self.max_regex_length),
            ("maxGlobBytes", self.max_glob_bytes),
            ("maxGlobBraceDepth", self.max_glob_brace_depth),
            ("maxGlobAlternatives", self.max_glob_alternatives),
            ("maxGlobGeneratedBytes", self.max_glob_generated_bytes),
            ("maxGlobMatchSteps", self.max_glob_match_steps),
            ("maxDiffBytes", self.max_diff_bytes),
            ("maxPreviousBytes", self.max_previous_bytes),
            ("maxPatchResultBytes", self.max_patch_result_bytes),
            ("maxGitignoreFiles", self.max_gitignore_files),
            ("maxGitignoreBytes", self.max_gitignore_bytes),
            ("maxGitignorePatterns", self.max_gitignore_patterns),
            (
                "maxGitignoreRetainedBytes",
                self.max_gitignore_retained_bytes,
            ),
            ("maxGitignoreMatchSteps", self.max_gitignore_match_steps),
        ] {
            if value == 0 {
                return Err(crate::FilesystemError::message(format!(
                    "{name} must be a positive integer"
                )));
            }
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileReadInput {
    pub file_path: String,
    #[serde(default)]
    pub offset: Option<usize>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileGlobInput {
    pub pattern: String,
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileGrepInput {
    pub pattern: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub include: Option<String>,
}

// Mutation inputs reject unknown fields so a misspelled or stale argument fails
// loudly instead of being dropped into a silent, unintended write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct FileWriteInput {
    pub file_path: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct FileEditInput {
    pub file_path: String,
    pub old_string: String,
    pub new_string: String,
    #[serde(default)]
    pub replace_all: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct FileApplyPatchInput {
    pub patch_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[serde(rename_all_fields = "camelCase")]
#[serde(tag = "kind")]
pub enum FileReadOutput {
    #[serde(rename = "directory")]
    Directory {
        path: String,
        relative_path: String,
        /// Rendered listing. Derived from `entry_details`, so it is the
        /// model-facing form rather than part of the structured record.
        #[serde(skip)]
        entries: Vec<String>,
        entry_details: Vec<DirectoryEntryDetail>,
        truncated: bool,
    },
    #[serde(rename = "file")]
    File {
        path: String,
        relative_path: String,
        text: String,
        /// Line-numbered rendering of `text`. Charged against the read byte
        /// budget because it is what the caller receives, and derived from
        /// `text` and `line_start`, so it is not part of the structured record.
        #[serde(skip)]
        numbered_text: String,
        line_start: usize,
        line_end: usize,
        total_lines: usize,
        truncated: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryEntryDetail {
    pub relative_path: String,
    pub kind: FileEntryKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_count: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum FileEntryKind {
    File,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileListing {
    pub path: String,
    pub relative_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_count: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileGlobOutput {
    pub cwd: String,
    pub relative_path: String,
    pub pattern: String,
    pub files: Vec<FileListing>,
    pub count: usize,
    /// Files matching the pattern. Counting continues past the returned window,
    /// so this exceeds `count` when results were withheld. Exact when
    /// `scanComplete` is true, otherwise a lower bound.
    pub total: usize,
    /// Whether every candidate the traversal produced was examined. False when
    /// the traversal or the match work budget stopped the scan early.
    pub scan_complete: bool,
    pub truncated: bool,
    /// Entries excluded by `.gitignore` rules. An empty result under ignore
    /// rules must be distinguishable from an empty tree.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub ignored: usize,
    /// Whether every applicable ignore rule was read and applied. False when a
    /// bound stopped rule collection, which means the exclusions are a subset of
    /// what the repository asked for.
    #[serde(default = "yes", skip_serializing_if = "is_yes")]
    pub ignore_complete: bool,
    /// Directories not traversed because they are themselves repositories,
    /// relative to the searched directory. Populated only when the group is
    /// configured to prune them; naming one as `path` traverses it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pruned_repositories: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileGrepRow {
    pub path: String,
    pub relative_path: String,
    pub line: usize,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileGrepOutput {
    pub cwd: String,
    pub relative_path: String,
    pub pattern: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include: Option<String>,
    pub rows: Vec<FileGrepRow>,
    pub matches: usize,
    /// Candidate files actually searched. An exact match total is not reported
    /// because producing one would require reading every remaining file.
    pub files_scanned: usize,
    /// Candidate files the traversal listed. A lower bound when `truncated`.
    pub files_listed: usize,
    pub truncated: bool,
    /// Entries excluded by `.gitignore` rules. A search that found nothing and a
    /// search that was not allowed to look are different answers.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub ignored: usize,
    /// Whether every applicable ignore rule was read and applied.
    #[serde(default = "yes", skip_serializing_if = "is_yes")]
    pub ignore_complete: bool,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) revisions: HashMap<String, FileVersion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileDiff {
    pub file: String,
    pub relative_path: String,
    pub patch: String,
    pub additions: usize,
    pub deletions: usize,
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

impl FileDiff {
    pub(crate) fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.file.capacity())
            .saturating_add(self.relative_path.capacity())
            .saturating_add(self.patch.capacity())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum FileMutationType {
    Add,
    Update,
    Delete,
    Move,
    Write,
    Edit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileMutation {
    pub file_path: String,
    pub relative_path: String,
    #[serde(rename = "type")]
    pub mutation_type: FileMutationType,
    pub patch: String,
    pub additions: usize,
    pub deletions: usize,
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub move_path: Option<String>,
}

impl FileMutation {
    pub(crate) fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.file_path.capacity())
            .saturating_add(self.relative_path.capacity())
            .saturating_add(self.patch.capacity())
            .saturating_add(self.move_path.as_ref().map_or(0, String::capacity))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileWriteOutput {
    pub kind: FileWriteKind,
    pub path: String,
    pub relative_path: String,
    pub existed: bool,
    pub applied: bool,
    pub diff: FileDiff,
    // The content the write replaced, carried when the file existed and fitted `max_previous_bytes`
    // so a caller can draw the write as a diff. `None` covers both a created file and one too large
    // to carry; `Some("")` is a file that was there and empty, which a sentinel could not tell from
    // either.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
}

impl FileWriteOutput {
    pub(crate) fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.path.capacity())
            .saturating_add(self.relative_path.capacity())
            .saturating_add(self.diff.retained_bytes())
            .saturating_add(self.previous.as_ref().map_or(0, String::capacity))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum FileWriteKind {
    Write,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileEditOutput {
    pub kind: FileEditKind,
    pub path: String,
    pub relative_path: String,
    pub applied: bool,
    pub diff: FileDiff,
}

impl FileEditOutput {
    pub(crate) fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.path.capacity())
            .saturating_add(self.relative_path.capacity())
            .saturating_add(self.diff.retained_bytes())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum FileEditKind {
    Edit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileApplyPatchOutput {
    pub kind: FilePatchKind,
    pub applied: bool,
    /// Combined preview. Concatenates `files[].patch`, so it is the
    /// model-facing form rather than part of the structured record.
    #[serde(skip)]
    pub diff: String,
    pub files: Vec<FileMutation>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

impl FileApplyPatchOutput {
    pub(crate) fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.diff.capacity())
            .saturating_add(
                self.files
                    .capacity()
                    .saturating_mul(size_of::<FileMutation>()),
            )
            .saturating_add(
                self.files
                    .iter()
                    .map(FileMutation::retained_bytes)
                    .fold(0, usize::saturating_add),
            )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum FilePatchKind {
    Patch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileResourceAccess {
    Read,
    Traverse,
    Write,
    ReadWrite,
    Delete,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileResource {
    pub requested_path: String,
    pub path: PathBuf,
    pub root_relative_path: String,
    pub access: FileResourceAccess,
}

impl FileResource {
    pub fn resource_scope(
        &self,
    ) -> Result<Vec<workcell_host_contract::ResourceId>, crate::WorkspaceError> {
        crate::root_relative_resource_scope(crate::RootResourceKind::Path, &self.root_relative_path)
    }
    pub fn resource_id(&self) -> Result<workcell_host_contract::ResourceId, crate::WorkspaceError> {
        crate::root_relative_resource_id(crate::RootResourceKind::Path, &self.root_relative_path)
    }

    /// Conservative bytes exclusively retained by this resource binding.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.requested_path.capacity())
            .saturating_add(self.path.capacity())
            .saturating_add(self.root_relative_path.capacity())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedDirectory {
    pub path: PathBuf,
    pub relative_path: String,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[cfg(test)]
mod tests {
    use super::{FileDiff, FileWriteKind, FileWriteOutput};

    const REPLACED: &str = "the content a write replaced";

    fn write_output(previous: Option<String>) -> FileWriteOutput {
        FileWriteOutput {
            kind: FileWriteKind::Write,
            path: "/root/notes.txt".into(),
            relative_path: "notes.txt".into(),
            existed: true,
            applied: true,
            diff: FileDiff {
                file: "/root/notes.txt".into(),
                relative_path: "notes.txt".into(),
                patch: String::new(),
                additions: 0,
                deletions: 0,
                truncated: false,
            },
            previous,
        }
    }

    /// The remote host charges a prepared write for what it holds, so a carried
    /// old side that went unreported would be retained for free.
    #[test]
    fn a_carried_old_side_is_charged_to_the_bytes_a_write_retains() {
        let carried = write_output(Some(REPLACED.to_owned()));
        let bare = write_output(None);

        assert_eq!(
            carried.retained_bytes() - bare.retained_bytes(),
            REPLACED.len()
        );
    }
}
