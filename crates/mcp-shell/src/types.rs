//! Protocol-neutral request, result, inspection, and progress payloads.
//!
//! These structs are wire contracts even though they are crate-private. Field names and version
//! numbers therefore change deliberately: consumers can branch on `version` rather than infer a
//! schema from optional fields or presentation text.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{mem::size_of, path::Path};

use crate::workdir::WorkdirBinding;

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

#[derive(Clone, Debug, JsonSchema, Serialize)]
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
    /// Redraw frames absorbed while rendering stdout as a terminal would show
    /// it. Non-zero means the capture held a progress bar that overwrote itself;
    /// `stdout_utf8_bytes` still reports what the command actually wrote.
    pub stdout_redraws_collapsed: u64,
    /// The same for stderr, where bars are more commonly written.
    pub stderr_redraws_collapsed: u64,
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

/// A word the shell will pass to a command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShellWord {
    /// The shell passes exactly this text, with quoting and escapes resolved.
    Literal(String),
    /// Expansion, globbing, or quoting the source does not resolve, so what the
    /// shell will pass is not knowable from the text.
    Undecodable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShellCommandScope {
    pub start_byte: usize,
    pub source: String,
    pub normalized: String,
    pub permission: String,
    /// Decoded basename of the executable.
    pub executable: String,
    /// Words after the executable, in order.
    ///
    /// `None` when the scope is not a plain command, so its words were never
    /// enumerated, which is not the same as a command with no arguments.
    pub arguments: Option<Vec<ShellWord>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShellCommandAnalysis {
    pub scopes: Vec<ShellCommandScope>,
    pub opaque: bool,
}

#[derive(Debug, Eq, PartialEq)]
pub struct ShellPolicyDecision {
    denial: Option<String>,
}

impl ShellPolicyDecision {
    pub(crate) const fn allow() -> Self {
        Self { denial: None }
    }

    pub(crate) fn deny(reason: String) -> Self {
        Self {
            denial: Some(reason),
        }
    }

    #[must_use]
    pub const fn is_allowed(&self) -> bool {
        self.denial.is_none()
    }

    #[must_use]
    pub fn denial_reason(&self) -> Option<&str> {
        self.denial.as_deref()
    }

    pub(crate) fn result(&self) -> Result<(), String> {
        match &self.denial {
            Some(reason) => Err(reason.clone()),
            None => Ok(()),
        }
    }
}

#[derive(Debug)]
pub struct PreparedShell {
    command: String,
    timeout_ms: u64,
    analysis: ShellCommandAnalysis,
    policy_decision: ShellPolicyDecision,
    workdir: WorkdirBinding,
    output_filter: bool,
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
        self.workdir.relative()
    }

    #[must_use]
    pub const fn analysis(&self) -> &ShellCommandAnalysis {
        &self.analysis
    }

    #[must_use]
    pub const fn policy_decision(&self) -> &ShellPolicyDecision {
        &self.policy_decision
    }

    #[must_use]
    pub const fn output_filter_enabled(&self) -> bool {
        self.output_filter
    }

    #[must_use]
    pub fn workdir(&self) -> &Path {
        self.workdir.canonical()
    }

    /// Conservative retained bytes, excluding immutable group policy shared by the caller.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.command.capacity())
            .saturating_add(self.analysis.retained_bytes())
            .saturating_add(
                self.policy_decision
                    .denial
                    .as_ref()
                    .map_or(0, String::capacity),
            )
            .saturating_add(self.workdir.retained_bytes())
    }

    pub(crate) fn new(
        command: String,
        timeout_ms: u64,
        analysis: ShellCommandAnalysis,
        policy_decision: ShellPolicyDecision,
        workdir: WorkdirBinding,
        output_filter: bool,
    ) -> Self {
        Self {
            command,
            timeout_ms,
            analysis,
            policy_decision,
            workdir,
            output_filter,
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
    ) -> (String, u64, WorkdirBinding, ShellCommandAnalysis, bool) {
        (
            self.command,
            self.timeout_ms,
            self.workdir,
            self.analysis,
            self.output_filter,
        )
    }
}

impl ShellCommandAnalysis {
    fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(
                self.scopes
                    .capacity()
                    .saturating_mul(size_of::<ShellCommandScope>()),
            )
            .saturating_add(
                self.scopes
                    .iter()
                    .map(ShellCommandScope::retained_bytes)
                    .fold(0, usize::saturating_add),
            )
    }
}

impl ShellCommandScope {
    fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.source.capacity())
            .saturating_add(self.normalized.capacity())
            .saturating_add(self.permission.capacity())
            .saturating_add(self.executable.capacity())
            .saturating_add(self.arguments.as_ref().map_or(0, |arguments| {
                arguments
                    .capacity()
                    .saturating_mul(size_of::<ShellWord>())
                    .saturating_add(
                        arguments
                            .iter()
                            .map(|argument| match argument {
                                ShellWord::Literal(value) => value.capacity(),
                                ShellWord::Undecodable => 0,
                            })
                            .fold(0, usize::saturating_add),
                    )
            }))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellFilterInfo {
    /// The reductions that were applied, in the order they ran. A corpus rule
    /// appears under its own name; the command-independent progress collapse
    /// appears as `progress`.
    pub stages: Vec<String>,
    pub unfiltered_utf8_bytes: usize,
    pub filtered_utf8_bytes: usize,
}

#[derive(Debug)]
pub struct ShellExecution {
    pub output: ShellOutput,
    pub model_text: String,
    pub filter: Option<ShellFilterInfo>,
}
