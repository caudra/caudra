//! Protocol-neutral request, result, inspection, and progress payloads.
//!
//! These structs are wire contracts even though they are crate-private. Field names and version
//! numbers therefore change deliberately: consumers can branch on `version` rather than infer a
//! schema from optional fields or presentation text.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;
pub const MAX_TIMEOUT_MS: u64 = 600_000;

#[derive(Clone, Debug, Deserialize, Serialize)]
// Strict decoding mirrors the advertised schema and prevents typoed controls from being ignored.
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ShellInput {
    pub command: String,
    pub timeout: Option<u64>,
    pub workdir: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellOutput {
    /// Version of the structured result shape, independent of the MCP protocol version.
    pub version: u8,
    pub kind: &'static str,
    pub relative_workdir: String,
    pub timeout_ms: u64,
    pub duration_ms: u64,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub output_limit_exceeded: bool,
    /// Last emitted progress sequence, or zero when the command produced no decoded output.
    pub final_sequence: u64,
    /// UTF-8 bytes emitted in stdout progress chunks, independent of raw process byte counts.
    pub stdout_utf8_bytes: u64,
    /// UTF-8 bytes emitted in stderr progress chunks, independent of raw process byte counts.
    pub stderr_utf8_bytes: u64,
    pub stdout: String,
    pub stderr: String,
    pub stdout_capture_truncated: bool,
    pub stderr_capture_truncated: bool,
    pub stdout_preview_truncated: bool,
    pub stderr_preview_truncated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ShellStream {
    Stdout,
    Stderr,
}

pub(crate) struct OutputEvent {
    pub(crate) stream: ShellStream,
    pub(crate) text: String,
    /// Number of source bytes represented by this event, which may differ from UTF-8 text length.
    pub(crate) raw_bytes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ShellProgressChunk {
    /// Version of the progress extension payload; it evolves separately from the final result.
    pub version: u8,
    pub sequence: u64,
    pub stream: ShellStream,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShellCommandScope {
    pub start_byte: usize,
    pub source: String,
    pub normalized: String,
    pub permission: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShellCommandAnalysis {
    pub scopes: Vec<ShellCommandScope>,
    pub opaque: bool,
}

#[derive(Debug)]
pub struct PreparedShell {
    command: String,
    timeout_ms: u64,
    relative_workdir: String,
    analysis: ShellCommandAnalysis,
    workdir: PathBuf,
}

impl PreparedShell {
    #[must_use]
    pub fn command(&self) -> &str {
        &self.command
    }

    #[must_use]
    pub const fn timeout_ms(&self) -> u64 {
        self.timeout_ms
    }

    #[must_use]
    pub fn relative_workdir(&self) -> &str {
        &self.relative_workdir
    }

    #[must_use]
    pub const fn analysis(&self) -> &ShellCommandAnalysis {
        &self.analysis
    }

    #[must_use]
    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    pub(crate) fn new(
        command: String,
        timeout_ms: u64,
        workdir: PathBuf,
        relative_workdir: String,
        analysis: ShellCommandAnalysis,
    ) -> Self {
        Self {
            command,
            timeout_ms,
            relative_workdir,
            analysis,
            workdir,
        }
    }

    /// Consumes the prepared command into the parts execution needs.
    ///
    /// The analysis is carried through rather than dropped so result rendering
    /// can reuse the parsed command scopes. Re-deriving them from the raw
    /// command string would reintroduce the shell-lexical evasions the parser
    /// already resolves.
    pub(crate) fn into_execution_parts(
        self,
    ) -> (String, u64, PathBuf, String, ShellCommandAnalysis) {
        (
            self.command,
            self.timeout_ms,
            self.workdir,
            self.relative_workdir,
            self.analysis,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellFilterInfo {
    pub rule: String,
    pub unfiltered_utf8_bytes: usize,
    pub filtered_utf8_bytes: usize,
}

#[derive(Debug)]
pub struct ShellExecution {
    pub output: ShellOutput,
    pub model_text: String,
    pub filter: Option<ShellFilterInfo>,
}
