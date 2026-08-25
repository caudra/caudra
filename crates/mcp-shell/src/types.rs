//! Private MCP request, result, and progress payloads.
//!
//! These structs are wire contracts even though they are crate-private. Field names and version
//! numbers therefore change deliberately: consumers can branch on `version` rather than infer a
//! schema from optional fields or presentation text.

use serde::{Deserialize, Serialize};

pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;
pub const MAX_TIMEOUT_MS: u64 = 600_000;

#[derive(Debug, Deserialize)]
// Strict decoding mirrors the advertised schema and prevents typoed controls from being ignored.
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct ShellInput {
    pub(crate) command: String,
    pub(crate) timeout: Option<u64>,
    pub(crate) workdir: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ShellOutput {
    /// Version of the structured result shape, independent of the MCP protocol version.
    pub(crate) version: u8,
    pub(crate) kind: &'static str,
    pub(crate) relative_workdir: String,
    pub(crate) timeout_ms: u64,
    pub(crate) duration_ms: u64,
    pub(crate) exit_code: Option<i32>,
    pub(crate) signal: Option<i32>,
    pub(crate) timed_out: bool,
    pub(crate) output_limit_exceeded: bool,
    /// Last emitted progress sequence, or zero when the command produced no decoded output.
    pub(crate) final_sequence: u64,
    /// UTF-8 bytes emitted in stdout progress chunks, independent of raw process byte counts.
    pub(crate) stdout_utf8_bytes: u64,
    /// UTF-8 bytes emitted in stderr progress chunks, independent of raw process byte counts.
    pub(crate) stderr_utf8_bytes: u64,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) stdout_capture_truncated: bool,
    pub(crate) stderr_capture_truncated: bool,
    pub(crate) stdout_preview_truncated: bool,
    pub(crate) stderr_preview_truncated: bool,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Stream {
    Stdout,
    Stderr,
}

pub(crate) struct OutputEvent {
    pub(crate) stream: Stream,
    pub(crate) text: String,
    /// Number of source bytes represented by this event, which may differ from UTF-8 text length.
    pub(crate) raw_bytes: usize,
}

#[derive(Serialize)]
pub(crate) struct OutputChunk<'a> {
    /// Version of the progress extension payload; it evolves separately from the final result.
    pub(crate) version: u8,
    pub(crate) sequence: u64,
    pub(crate) stream: Stream,
    pub(crate) text: &'a str,
}
