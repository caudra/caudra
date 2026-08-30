use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::FilesystemError;

pub const INDEX_PARSER_CONCURRENCY: usize = 2;
pub const INDEX_MAX_PATH_BYTES: usize = 4_096;
pub const INDEX_MAX_DEADLINE_MS: u64 = 60_000;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IndexExecutionConfiguration {
    pub limits: IndexLimits,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IndexLimits {
    pub max_source_bytes: usize,
    pub max_model_output_bytes: usize,
    pub max_output_line_bytes: usize,
    pub max_directory_entries: usize,
    pub max_directory_scan_entries: usize,
    /// Maximum nodes accepted by post-parse tree inspection before extraction.
    pub max_nodes: usize,
    /// Maximum depth accepted by post-parse tree inspection before extraction.
    pub max_depth: usize,
    /// Absolute blocking-queue, parse, inspection, and extraction deadline.
    pub parser_deadline_ms: u64,
    pub admission_deadline_ms: u64,
}

impl Default for IndexLimits {
    fn default() -> Self {
        Self {
            max_source_bytes: 2 * 1024 * 1024,
            max_model_output_bytes: 50 * 1024,
            max_output_line_bytes: 2_000,
            max_directory_entries: 1_000,
            max_directory_scan_entries: 10_000,
            max_nodes: 200_000,
            max_depth: 512,
            parser_deadline_ms: 2_000,
            admission_deadline_ms: 2_000,
        }
    }
}

impl IndexLimits {
    pub(crate) fn validate(self) -> Result<Self, FilesystemError> {
        for (name, value) in [
            ("maxSourceBytes", self.max_source_bytes),
            ("maxModelOutputBytes", self.max_model_output_bytes),
            ("maxOutputLineBytes", self.max_output_line_bytes),
            ("maxDirectoryEntries", self.max_directory_entries),
            ("maxDirectoryScanEntries", self.max_directory_scan_entries),
            ("maxNodes", self.max_nodes),
            ("maxDepth", self.max_depth),
        ] {
            if value == 0 {
                return Err(FilesystemError::message(format!(
                    "Index limit {name} must be a positive integer"
                )));
            }
        }
        for (name, value) in [
            ("parserDeadlineMs", self.parser_deadline_ms),
            ("admissionDeadlineMs", self.admission_deadline_ms),
        ] {
            if value == 0 {
                return Err(FilesystemError::message(format!(
                    "Index limit {name} must be a positive integer"
                )));
            }
            if value > INDEX_MAX_DEADLINE_MS {
                return Err(FilesystemError::message(format!(
                    "Index limit {name} must not exceed {INDEX_MAX_DEADLINE_MS}"
                )));
            }
        }
        if self.max_output_line_bytes > self.max_model_output_bytes {
            return Err(FilesystemError::message(
                "Index limit maxOutputLineBytes must not exceed maxModelOutputBytes",
            ));
        }
        if self.max_output_line_bytes < "[truncated]".len()
            || self.max_model_output_bytes < "[truncated]".len()
        {
            return Err(FilesystemError::message(
                "Index output byte limits must fit the truncation marker",
            ));
        }
        if self.max_directory_entries > self.max_directory_scan_entries {
            return Err(FilesystemError::message(
                "Index limit maxDirectoryEntries must not exceed maxDirectoryScanEntries",
            ));
        }
        Ok(self)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IndexInput {
    pub path: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[serde(rename_all_fields = "camelCase")]
#[serde(tag = "kind")]
pub enum IndexOutput {
    #[serde(rename = "file")]
    File {
        path: String,
        relative_path: String,
        language: String,
        skeleton: String,
        lines: Vec<IndexOutputLine>,
        source_line_count: usize,
        parse_error: bool,
        truncated: bool,
    },
    #[serde(rename = "directory")]
    Directory {
        path: String,
        relative_path: String,
        entries: Vec<IndexDirectoryEntry>,
        total_count: usize,
        truncated: bool,
        listing: String,
    },
}

impl IndexOutput {
    #[must_use]
    pub fn model_text(&self) -> &str {
        match self {
            Self::File { skeleton, .. } => skeleton,
            Self::Directory { listing, .. } => listing,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct IndexOutputLine {
    pub output_line: usize,
    pub text: String,
    pub semantic: IndexLineSemantic,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_range: Option<IndexSourceRange>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IndexLineSemantic {
    Section,
    Item,
    Dimmed,
    Plain,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct IndexSourceRange {
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct IndexDirectoryEntry {
    pub name: String,
    pub kind: IndexDirectoryEntryKind,
}

#[derive(
    Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum IndexDirectoryEntryKind {
    Directory,
    File,
}
