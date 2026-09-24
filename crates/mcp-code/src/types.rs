//! Protocol-neutral request and result payloads.
//!
//! These structs are wire contracts even though they are crate-private. Field names and version
//! numbers therefore change deliberately: consumers can branch on `version` rather than infer a
//! schema from optional fields or presentation text.

use std::mem::size_of;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const MILLIS_PER_SECOND: u64 = 1_000;
/// Small scripts are the advertised use case, so the default budget is short enough that a runaway
/// snippet fails fast instead of occupying a worker for minutes.
pub const DEFAULT_TIMEOUT_SECS: u64 = 5;
pub const MAX_TIMEOUT_SECS: u64 = 30;
pub const DEFAULT_TIMEOUT_MS: u64 = DEFAULT_TIMEOUT_SECS * MILLIS_PER_SECOND;
pub const MAX_TIMEOUT_MS: u64 = MAX_TIMEOUT_SECS * MILLIS_PER_SECOND;
/// Matches the shell tool's command bound so both executors reject oversized payloads alike.
pub const MAX_CODE_BYTES: usize = 65_536;

/// Allocator-backed ceiling handed to the worker. The worker's hard ceiling sits above this, so the
/// real process footprint is larger; `max_processes` bounds the aggregate.
pub(crate) const MAX_MEMORY_BYTES: usize = 256 * 1024 * 1024;
/// Bytes retained per stream for the returned tail. Capture is bounded independently of the value.
pub(crate) const STREAM_CAPTURE_BYTES: usize = 256 * 1024;
/// A snippet that reads undefined names in a loop would otherwise round-trip without limit.
pub(crate) const MAX_SUSPENSIONS: u32 = 256;

#[derive(Clone, Debug, Deserialize, Serialize)]
// Strict decoding mirrors the advertised schema and prevents typoed controls from being ignored.
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CodeInput {
    pub code: String,
    pub timeout_sec: Option<u64>,
}

impl CodeInput {
    /// The deadline this input runs under, or the refusal it gets, so a caller showing the
    /// deadline ahead of the run reads the same rule the executor enforces.
    pub fn timeout_ms(&self) -> Result<u64, String> {
        match self.timeout_sec {
            None => Ok(DEFAULT_TIMEOUT_MS),
            Some(requested @ 1..=MAX_TIMEOUT_SECS) => Ok(requested * MILLIS_PER_SECOND),
            Some(requested) => Err(format!(
                "Invalid arguments: timeoutSec is {requested} seconds; it must be between 1 and {MAX_TIMEOUT_SECS}. Omit it for the {DEFAULT_TIMEOUT_SECS} second default"
            )),
        }
    }
}

#[derive(Debug)]
pub struct PreparedCode {
    pub(crate) code: String,
    pub(crate) timeout_ms: u64,
    pub(crate) type_check: bool,
}

impl PreparedCode {
    /// Conservative retained bytes for the source captured by this execution plan.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        size_of::<Self>().saturating_add(self.code.capacity())
    }

    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    #[must_use]
    pub const fn timeout_ms(&self) -> u64 {
        self.timeout_ms
    }

    #[must_use]
    pub const fn type_check(&self) -> bool {
        self.type_check
    }
}

/// How a call ended. This is the field an agent should branch on before reading anything else.
#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Outcome {
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

#[derive(Debug, JsonSchema, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeException {
    /// Python exception class name, for example `ValueError`.
    pub r#type: String,
    pub message: String,
}

#[derive(Debug, JsonSchema, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeOutput {
    /// Version of the structured result shape, independent of the MCP protocol version.
    pub version: u8,
    pub kind: &'static str,
    pub outcome: Outcome,
    pub timeout_ms: u64,
    pub duration_ms: u64,
    /// Whether the snippet was type checked before running, which decides if `rejected` is possible.
    pub type_checked: bool,
    /// Value of the final expression rendered as JSON, or null when it has no JSON representation.
    pub result: serde_json::Value,
    /// Python `repr` of the same value, which stays faithful where the JSON rendering cannot.
    pub result_repr: Option<String>,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub stdout_utf8_bytes: u64,
    pub stderr_utf8_bytes: u64,
    pub exception: Option<CodeException>,
    /// Actionable guidance derived from the failure, aimed at the calling agent rather than a human.
    pub diagnostic: Option<String>,
    pub timed_out: bool,
    pub memory_exceeded: bool,
    pub suspension_limit_exceeded: bool,
}

#[derive(Debug)]
pub struct CodeExecution {
    pub output: CodeOutput,
    pub model_text: String,
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

#[cfg(test)]
mod tests {
    use super::PreparedCode;

    #[test]
    fn prepared_code_estimate_covers_the_owned_source_capacity() {
        let mut code = String::with_capacity(64 * 1_024);
        code.push_str("print('ok')");
        let prepared = PreparedCode {
            code,
            timeout_ms: 1,
            type_check: true,
        };

        assert!(prepared.retained_bytes() >= 64 * 1_024);
    }
}
