//! Private MCP request and result payloads.
//!
//! These structs are wire contracts even though they are crate-private. Field names and version
//! numbers therefore change deliberately: consumers can branch on `version` rather than infer a
//! schema from optional fields or presentation text.

use serde::{Deserialize, Serialize};

/// Small scripts are the advertised use case, so the default budget is short enough that a runaway
/// snippet fails fast instead of occupying a worker for minutes.
pub const DEFAULT_TIMEOUT_MS: u64 = 5_000;
pub const MAX_TIMEOUT_MS: u64 = 30_000;
/// Matches the shell tool's command bound so both executors reject oversized payloads alike.
pub const MAX_CODE_BYTES: usize = 65_536;

/// Allocator-backed ceiling handed to the worker. The worker's hard ceiling sits above this, so the
/// real process footprint is larger; `max_processes` bounds the aggregate.
pub(crate) const MAX_MEMORY_BYTES: usize = 256 * 1024 * 1024;
/// Bytes retained per stream for the returned tail. Capture is bounded independently of the value.
pub(crate) const STREAM_CAPTURE_BYTES: usize = 256 * 1024;
/// A snippet that reads undefined names in a loop would otherwise round-trip without limit.
pub(crate) const MAX_SUSPENSIONS: u32 = 256;

#[derive(Debug, Deserialize)]
// Strict decoding mirrors the advertised schema and prevents typoed controls from being ignored.
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct CodeInput {
    pub(crate) code: String,
    pub(crate) timeout: Option<u64>,
}

/// How a call ended. This is the field an agent should branch on before reading anything else.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Outcome {
    /// The snippet ran to completion and produced a value.
    Completed,
    /// The snippet ran and raised a Python exception.
    Exception,
    /// The snippet never ran: type checking or the parser refused it, so there are no side effects.
    Rejected,
    /// A time, memory, or round-trip budget stopped execution.
    Limited,
    /// The executor could not service the call. Nothing can be inferred about the snippet.
    Unavailable,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CodeException {
    /// Python exception class name, for example `ValueError`.
    pub(crate) r#type: String,
    pub(crate) message: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CodeOutput {
    /// Version of the structured result shape, independent of the MCP protocol version.
    pub(crate) version: u8,
    pub(crate) kind: &'static str,
    pub(crate) outcome: Outcome,
    pub(crate) timeout_ms: u64,
    pub(crate) duration_ms: u64,
    /// Whether the snippet was type checked before running, which decides if `rejected` is possible.
    pub(crate) type_checked: bool,
    /// Value of the final expression rendered as JSON, or null when it has no JSON representation.
    pub(crate) result: serde_json::Value,
    /// Python `repr` of the same value, which stays faithful where the JSON rendering cannot.
    pub(crate) result_repr: Option<String>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) stdout_truncated: bool,
    pub(crate) stderr_truncated: bool,
    pub(crate) stdout_utf8_bytes: u64,
    pub(crate) stderr_utf8_bytes: u64,
    pub(crate) exception: Option<CodeException>,
    /// Actionable guidance derived from the failure, aimed at the calling agent rather than a human.
    pub(crate) diagnostic: Option<String>,
    pub(crate) timed_out: bool,
    pub(crate) memory_exceeded: bool,
    pub(crate) suspension_limit_exceeded: bool,
}

impl CodeOutput {
    /// Builds the success-shaped envelope. Failure paths overwrite the fields they own.
    pub(crate) fn new(
        outcome: Outcome,
        timeout_ms: u64,
        duration_ms: u64,
        type_checked: bool,
    ) -> Self {
        Self {
            version: 1,
            kind: "code",
            outcome,
            timeout_ms,
            duration_ms,
            type_checked,
            result: serde_json::Value::Null,
            result_repr: None,
            stdout: String::new(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            stdout_utf8_bytes: 0,
            stderr_utf8_bytes: 0,
            exception: None,
            diagnostic: None,
            timed_out: false,
            memory_exceeded: false,
            suspension_limit_exceeded: false,
        }
    }
}
