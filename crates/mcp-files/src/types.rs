use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
    pub max_patch_result_bytes: usize,
}

impl Default for FilesystemLimits {
    fn default() -> Self {
        Self {
            max_read_bytes: 50 * 1024,
            max_file_bytes: 5 * 1024 * 1024,
            max_line_length: 2_000,
            max_read_lines: 2_000,
            max_search_results: 100,
            max_traversal_entries: 10_000,
            max_write_bytes: 5 * 1024 * 1024,
            max_patch_bytes: 1024 * 1024,
            max_patch_files: 100,
            max_patch_plan_bytes: 32 * 1024 * 1024,
            max_regex_length: 1_000,
            max_glob_bytes: 4 * 1024,
            max_glob_brace_depth: 8,
            max_glob_alternatives: 256,
            max_glob_generated_bytes: 64 * 1024,
            max_glob_match_steps: 1_000_000,
            max_diff_bytes: 16 * 1024,
            max_patch_result_bytes: 4 * 1024 * 1024,
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
            ("maxPatchResultBytes", self.max_patch_result_bytes),
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileWriteInput {
    pub file_path: String,
    pub content: String,
    #[serde(default)]
    pub dry_run: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileEditInput {
    pub file_path: String,
    pub old_string: String,
    pub new_string: String,
    #[serde(default)]
    pub replace_all: Option<bool>,
    #[serde(default)]
    pub dry_run: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileApplyPatchInput {
    pub patch_text: String,
    #[serde(default)]
    pub dry_run: Option<bool>,
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
    pub truncated: bool,
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
    pub truncated: bool,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileWriteOutput {
    pub kind: FileWriteKind,
    pub path: String,
    pub relative_path: String,
    pub existed: bool,
    pub applied: bool,
    pub diff: FileDiff,
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
    pub access: FileResourceAccess,
}

fn is_false(value: &bool) -> bool {
    !*value
}
