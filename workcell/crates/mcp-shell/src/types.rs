//! Protocol-neutral request, result, inspection, and progress payloads.
//!
//! These structs are wire contracts even though they are crate-private. Field names and version
//! numbers therefore change deliberately: consumers can branch on `version` rather than infer a
//! schema from optional fields or presentation text.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{mem::size_of, path::Path};

use crate::{
    bash::{BashCommandContexts, BashContextError, BashParseError, BashProgram},
    process::{SharedShellLauncher, ShellLauncher, retained_launcher_bytes},
    workdir::WorkdirBinding,
};

pub(crate) const MILLIS_PER_SECOND: u64 = 1_000;
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
pub const MAX_TIMEOUT_SECS: u64 = 21_600;
/// Direct host execution counts in milliseconds, so it reads the same bounds in its own unit.
pub const DEFAULT_TIMEOUT_MS: u64 = DEFAULT_TIMEOUT_SECS * MILLIS_PER_SECOND;
pub const MAX_TIMEOUT_MS: u64 = MAX_TIMEOUT_SECS * MILLIS_PER_SECOND;

#[derive(Clone, Debug, Deserialize, Serialize)]
// Strict decoding mirrors the advertised schema and prevents typoed controls from being ignored.
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ShellInput {
    pub command: String,
    pub timeout_sec: Option<u64>,
    pub workdir: Option<String>,
}

impl ShellInput {
    /// The deadline this input runs under, or the refusal it gets, so a caller showing the
    /// deadline ahead of the run reads the same rule the executor enforces.
    ///
    /// Zero is refused rather than read as a limit: callers disagree on whether it means none or
    /// the default, and either guess runs a command for the wrong length of time.
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
    bash_program: Result<BashProgram, BashParseError>,
    policy_decision: ShellPolicyDecision,
    workdir: WorkdirBinding,
    output_filter: bool,
    launcher: SharedShellLauncher,
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

    pub fn bash_program(&self) -> Result<&BashProgram, &BashParseError> {
        self.bash_program.as_ref()
    }

    pub fn bash_command_contexts(&self) -> Result<BashCommandContexts, BashContextError> {
        let assumptions = self
            .launcher
            .as_ref()
            .as_ref()
            .ok()
            .and_then(ShellLauncher::bash_startup_assumptions)
            .ok_or(BashContextError::UnsupportedLauncher)?;
        let program = self
            .bash_program()
            .map_err(|error| BashContextError::Parse(error.clone()))?;
        Ok(program.command_contexts_with_assumptions(self.workdir(), assumptions))
    }

    #[must_use]
    pub fn bash_executable(&self) -> Option<&Path> {
        self.launcher
            .as_ref()
            .as_ref()
            .ok()
            .and_then(ShellLauncher::bash_executable)
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
                self.bash_program
                    .as_ref()
                    .map_or(0, BashProgram::retained_bytes),
            )
            .saturating_add(
                self.policy_decision
                    .denial
                    .as_ref()
                    .map_or(0, String::capacity),
            )
            .saturating_add(self.workdir.retained_bytes())
            .saturating_add(retained_launcher_bytes(&self.launcher))
    }

    pub(crate) fn new(
        command: String,
        timeout_ms: u64,
        analysis: (ShellCommandAnalysis, Result<BashProgram, BashParseError>),
        policy_decision: ShellPolicyDecision,
        workdir: WorkdirBinding,
        output_filter: bool,
        launcher: SharedShellLauncher,
    ) -> Self {
        let (analysis, bash_program) = analysis;
        Self {
            command,
            timeout_ms,
            analysis,
            bash_program,
            policy_decision,
            workdir,
            output_filter,
            launcher,
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
    ) -> (
        String,
        u64,
        WorkdirBinding,
        ShellCommandAnalysis,
        bool,
        SharedShellLauncher,
    ) {
        (
            self.command,
            self.timeout_ms,
            self.workdir,
            self.analysis,
            self.output_filter,
            self.launcher,
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

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use crate::bash::{BashCwdSet, MAX_BASH_CST_NODES};
    use crate::{ShellInput, ShellPermissionPolicy, ShellToolGroup, bash::BashContextError};
    use tempfile::TempDir;

    use super::PreparedShell;

    const WORKDIR: &str = "work";

    async fn prepare(source: &str) -> (TempDir, PreparedShell) {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join(WORKDIR)).unwrap();
        let group = ShellToolGroup::with_policy(directory.path(), ShellPermissionPolicy::yolo())
            .await
            .unwrap();
        let prepared = group
            .prepare(ShellInput {
                command: source.to_owned(),
                timeout_sec: None,
                workdir: Some(WORKDIR.into()),
            })
            .await
            .unwrap();
        (directory, prepared)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn prepared_contexts_use_launcher_guarantees_and_the_bound_workdir_not_history_assumptions()
     {
        let (_directory, prepared) = prepare("cd left || cd right; cat note.txt").await;
        let contexts = prepared.bash_command_contexts().unwrap();
        assert!(contexts.complete, "{:?}", contexts.diagnostics);
        assert_eq!(
            contexts.commands[0].incoming,
            BashCwdSet::Known(vec![prepared.workdir().to_owned()])
        );
        let mut possible = vec![
            prepared.workdir().to_owned(),
            prepared.workdir().join("left"),
            prepared.workdir().join("right"),
        ];
        possible.sort();
        assert_eq!(
            contexts.commands.last().unwrap().incoming,
            BashCwdSet::Known(possible)
        );
        assert!(
            !prepared
                .bash_program()
                .unwrap()
                .command_contexts(prepared.workdir())
                .complete
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn trusted_startup_does_not_launder_stateful_scripts_into_known_contexts() {
        for source in [
            "source setup; cat note",
            ". setup; cat note",
            "eval 'cd left'; cat note",
            "trap 'cd left' DEBUG; cat note",
            "shopt -s lastpipe; cat note",
            "set -P; cat note",
            "export CDPATH=/outside; cat note",
            "f() { cd left; }; f; cat note",
            "command cd left; cat note",
            "builtin cd left; cat note",
            "cd $target; cat note",
            "cat $(cd left); cat note",
        ] {
            let (_directory, prepared) = prepare(source).await;
            let contexts = prepared.bash_command_contexts().unwrap();
            assert!(!contexts.complete, "{source:?}");
            let last = contexts.commands.last().unwrap();
            assert!(!last.complete, "{source:?}");
            assert_eq!(last.incoming, BashCwdSet::Unknown, "{source:?}");
            assert!(!contexts.diagnostics.is_empty(), "{source:?}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_bash_analysis_cannot_produce_trusted_contexts() {
        let (_directory, prepared) = prepare(&"echo x;".repeat(MAX_BASH_CST_NODES)).await;
        let error = prepared.bash_program().unwrap_err().clone();
        assert_eq!(
            prepared.bash_command_contexts(),
            Err(BashContextError::Parse(error))
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn the_windows_cmd_launcher_never_claims_bash_context_trust() {
        let (_directory, prepared) = prepare("echo value").await;
        assert_eq!(
            prepared.bash_command_contexts(),
            Err(BashContextError::UnsupportedLauncher)
        );
    }
}
