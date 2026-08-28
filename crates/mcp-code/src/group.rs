//! Code execution orchestration.
//!
//! One tool call is one checkout, one feed, and one finish. Nothing is retained between calls: a
//! checkout that saw a terminal resource error is discarded rather than returned, because Monty
//! makes no guarantee about heap state after one, and a reused session would leak the previous
//! caller's definitions into the next.

use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use monty_pool::{Checkout, Pool, PoolError, ReplConfig, TurnEvent, on_print_sync};
use monty_types::{PrintStream, ResourceLimits, TypeCheckingConfig, TypeCheckingFormat};
use rmcp::model::{CallToolResult, ContentBlock, Tool};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{
    catalog,
    diagnose::{diagnose, diagnose_type_errors, timeout_guidance},
    render::{Capture, render},
    suspend::{Answer, answer},
    types::{
        CodeException, CodeInput, CodeOutput, DEFAULT_TIMEOUT_MS, MAX_CODE_BYTES, MAX_MEMORY_BYTES,
        MAX_SUSPENSIONS, MAX_TIMEOUT_MS, Outcome, STREAM_CAPTURE_BYTES,
    },
    worker::{CodeBuildError, build_pool},
};

const TOOL_NAME: &str = "code_execution";
/// Name shown in tracebacks the caller sees. It is not a real path and never touches a filesystem.
const SCRIPT_NAME: &str = "snippet.py";

/// Startup configuration. Everything here is operator-chosen and unreachable from tool input.
#[derive(Clone, Copy, Debug)]
pub struct CodeConfiguration<'a> {
    /// Explicit worker path, or `None` to discover one.
    pub worker: Option<&'a Path>,
    /// Type check each snippet before running it.
    pub type_check: bool,
}

pub struct CodeToolGroup {
    pool: Pool,
    type_check: bool,
}

impl std::fmt::Debug for CodeToolGroup {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The worker path is operator configuration and is redacted like every other path.
        formatter
            .debug_struct("CodeToolGroup")
            .field("type_check", &self.type_check)
            .finish_non_exhaustive()
    }
}

impl CodeToolGroup {
    /// Builds the group and eagerly starts a worker, so a broken deployment fails at startup.
    pub async fn new(configuration: CodeConfiguration<'_>) -> Result<Self, CodeBuildError> {
        let pool = build_pool(configuration.worker, Duration::from_millis(MAX_TIMEOUT_MS)).await?;
        Ok(Self {
            pool,
            type_check: configuration.type_check,
        })
    }

    #[must_use]
    pub fn catalog(&self) -> Vec<Tool> {
        catalog::catalog()
    }

    /// Asks idle workers to exit cleanly. Dropping the group kills them instead, which is equally
    /// safe but leaves a SIGKILL in the operator's logs.
    pub async fn shutdown(&self) {
        self.pool.close().await;
    }

    pub async fn dispatch(
        &self,
        name: &str,
        arguments: Value,
        cancellation: CancellationToken,
    ) -> Option<Result<CallToolResult, rmcp::ErrorData>> {
        if name != TOOL_NAME {
            // Returning `None` lets an application compose this group with other MCP tool groups.
            return None;
        }
        let input = match serde_json::from_value::<CodeInput>(arguments) {
            Ok(input) => input,
            Err(e) => {
                return Some(Ok(tool_error(format!(
                    "Invalid arguments for tool {TOOL_NAME}: {e}"
                ))));
            }
        };
        Some(Ok(match self.execute(input, cancellation).await {
            Ok(Some(output)) => result_content(output),
            Ok(None) => tool_error("Code execution cancelled"),
            Err(e) => tool_error(e),
        }))
    }

    async fn execute(
        &self,
        input: CodeInput,
        cancellation: CancellationToken,
    ) -> Result<Option<CodeOutput>, String> {
        if input.code.trim().is_empty() {
            return Err("Invalid arguments: code must not be empty".into());
        }
        if input.code.len() > MAX_CODE_BYTES {
            return Err(format!(
                "Invalid arguments: code must not exceed {MAX_CODE_BYTES} UTF-8 bytes"
            ));
        }
        let timeout_ms = match input.timeout {
            Some(value) if value == 0 || value > MAX_TIMEOUT_MS => {
                return Err(format!(
                    "Invalid arguments: timeout must be between 1 and {MAX_TIMEOUT_MS} milliseconds"
                ));
            }
            Some(value) => value,
            None => DEFAULT_TIMEOUT_MS,
        };

        let started = Instant::now();
        let repl = self.repl_config(timeout_ms);
        let session = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Ok(None),
            session = self.pool.checkout(&repl) => session,
        };
        let mut session = match session {
            Ok(session) => session,
            Err(e) => {
                return Ok(Some(Self::failure(
                    self.type_check,
                    &e,
                    timeout_ms,
                    started.elapsed(),
                )));
            }
        };

        let outcome = tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                // Dropping the checkout kills the worker: mid-execution state cannot be trusted
                // back into the pool, and the pool spawns a replacement on demand.
                drop(session);
                return Ok(None);
            }
            outcome = self.drive(&mut session, &input.code) => outcome,
        };
        let duration = started.elapsed();

        match outcome {
            Ok(completion) => {
                // Only a clean completion returns the worker; anything else discards it.
                let _ = session.finish().await;
                Ok(Some(self.complete(completion, timeout_ms, duration)))
            }
            Err(DriveError::Pool(e)) => {
                drop(session);
                Ok(Some(Self::failure(
                    self.type_check,
                    &e,
                    timeout_ms,
                    duration,
                )))
            }
            Err(DriveError::SuspensionLimit(captured)) => {
                drop(session);
                let mut output = Self::envelope(
                    self.type_check,
                    Outcome::Limited,
                    timeout_ms,
                    duration,
                    captured,
                );
                output.suspension_limit_exceeded = true;
                output.diagnostic = Some(
                    "The snippet made too many unresolved external lookups and was stopped. This usually means a typo or an undefined name inside a loop.".into(),
                );
                Ok(Some(output))
            }
        }
    }

    fn repl_config(&self, timeout_ms: u64) -> ReplConfig {
        let limits = ResourceLimits::default()
            .max_duration(Duration::from_millis(timeout_ms))
            .max_memory(MAX_MEMORY_BYTES);
        ReplConfig {
            script_name: SCRIPT_NAME.to_owned(),
            limits: Some(limits),
            type_check: self.type_check,
            type_check_stubs: None,
            // Concise diagnostics stay readable in a tool result; the full form renders a source
            // snippet with carets for a terminal, and colour would be ANSI noise in JSON.
            type_check_config: TypeCheckingConfig {
                format: TypeCheckingFormat::Concise,
                color: false,
            },
            ..ReplConfig::default()
        }
    }

    /// Feeds the snippet and answers suspensions until it completes or the round-trip cap is hit.
    async fn drive(&self, session: &mut Checkout, code: &str) -> Result<Completion, DriveError> {
        let captured = Captured::new();
        let mut event = {
            let mut on_print = captured.sink();
            session
                .feed(code, Vec::new(), Vec::new(), false, &mut on_print)
                .await
                .map_err(DriveError::Pool)?
        };

        for _ in 0..MAX_SUSPENSIONS {
            match event {
                TurnEvent::Complete(value) => {
                    return Ok(Completion {
                        value,
                        captured: captured.take(),
                    });
                }
                ref suspension => {
                    let decision = answer(suspension);
                    let mut on_print = captured.sink();
                    event = match decision {
                        Answer::Resume(value) => session.resume(value, &mut on_print).await,
                        Answer::NameError => session.resume_name_lookup(None, &mut on_print).await,
                    }
                    .map_err(DriveError::Pool)?;
                }
            }
        }
        Err(DriveError::SuspensionLimit(captured.take()))
    }

    fn complete(&self, completion: Completion, timeout_ms: u64, duration: Duration) -> CodeOutput {
        let mut output = Self::envelope(
            self.type_check,
            Outcome::Completed,
            timeout_ms,
            duration,
            completion.captured,
        );
        let (json, repr) = render(&completion.value);
        output.result = json;
        output.result_repr = repr;
        output
    }

    /// Converts a pool failure into the envelope, keeping worker internals out of the result.
    fn failure(
        type_check: bool,
        error: &PoolError,
        timeout_ms: u64,
        duration: Duration,
    ) -> CodeOutput {
        let mut output = Self::envelope(
            type_check,
            Outcome::Unavailable,
            timeout_ms,
            duration,
            captured_empty(),
        );
        match error {
            PoolError::Runtime(exception) => {
                let diagnosis = diagnose(exception);
                output.outcome = diagnosis.outcome;
                output.diagnostic = diagnosis.diagnostic;
                output.memory_exceeded =
                    matches!(exception.exc_type(), monty_types::ExcType::MemoryError);
                output.timed_out =
                    matches!(exception.exc_type(), monty_types::ExcType::TimeoutError);
                output.exception = Some(CodeException {
                    r#type: exception.exc_type().to_string(),
                    message: exception.message().unwrap_or_default().to_owned(),
                });
            }
            // Type checking ran before execution, so nothing was executed and nothing printed.
            PoolError::Typing(diagnostics) => {
                output.outcome = Outcome::Rejected;
                output.diagnostic = Some(diagnose_type_errors(diagnostics));
            }
            PoolError::Timeout { .. } => {
                output.outcome = Outcome::Limited;
                output.timed_out = true;
                output.diagnostic = Some(timeout_guidance(""));
            }
            PoolError::Exhausted => {
                output.diagnostic = Some(
                    "All code execution workers are busy. Retry shortly, or use a different tool."
                        .into(),
                );
            }
            // A crash is the isolation doing its job. Exit statuses and paths stay out of the result.
            PoolError::Crashed { .. } => {
                output.diagnostic = Some(
                    "The code execution worker terminated unexpectedly and has been replaced. Retry once; if it repeats, simplify the snippet.".into(),
                );
            }
            PoolError::Spawn(_)
            | PoolError::Protocol(_)
            | PoolError::Finished
            | PoolError::Disconnected { .. }
            | PoolError::Shutdown { .. } => {
                output.diagnostic = Some(
                    "The code execution worker is unavailable. Retry shortly, or use a different tool."
                        .into(),
                );
            }
        }
        output
    }

    fn envelope(
        type_check: bool,
        outcome: Outcome,
        timeout_ms: u64,
        duration: Duration,
        captured: CapturedOutput,
    ) -> CodeOutput {
        let mut output = CodeOutput::new(outcome, timeout_ms, duration_ms(duration), type_check);
        let (stdout, stdout_truncated, stdout_bytes) = captured.stdout;
        let (stderr, stderr_truncated, stderr_bytes) = captured.stderr;
        output.stdout = stdout;
        output.stderr = stderr;
        output.stdout_truncated = stdout_truncated;
        output.stderr_truncated = stderr_truncated;
        output.stdout_utf8_bytes = stdout_bytes;
        output.stderr_utf8_bytes = stderr_bytes;
        output
    }
}

struct Completion {
    value: monty_types::MontyObject,
    captured: CapturedOutput,
}

enum DriveError {
    Pool(PoolError),
    SuspensionLimit(CapturedOutput),
}

/// Shared print capture. The pool hands the sink a `&mut` closure per turn, so the buffers live
/// outside the turn and are locked only for the duration of one chunk.
struct Captured {
    stdout: Arc<Mutex<Capture>>,
    stderr: Arc<Mutex<Capture>>,
}

struct CapturedOutput {
    stdout: (String, bool, u64),
    stderr: (String, bool, u64),
}

fn captured_empty() -> CapturedOutput {
    CapturedOutput {
        stdout: (String::new(), false, 0),
        stderr: (String::new(), false, 0),
    }
}

impl Captured {
    fn new() -> Self {
        Self {
            stdout: Arc::new(Mutex::new(Capture::new(STREAM_CAPTURE_BYTES))),
            stderr: Arc::new(Mutex::new(Capture::new(STREAM_CAPTURE_BYTES))),
        }
    }

    fn sink(&self) -> impl FnMut(PrintStream, &str) -> monty_pool::PrintFuture + Send + use<> {
        let stdout = Arc::clone(&self.stdout);
        let stderr = Arc::clone(&self.stderr);
        on_print_sync(move |stream, text| {
            let target = match stream {
                PrintStream::Stdout => &stdout,
                PrintStream::Stderr => &stderr,
            };
            if let Ok(mut capture) = target.lock() {
                capture.push(text);
            }
        })
    }

    fn take(self) -> CapturedOutput {
        CapturedOutput {
            stdout: into_parts(self.stdout),
            stderr: into_parts(self.stderr),
        }
    }
}

fn into_parts(capture: Arc<Mutex<Capture>>) -> (String, bool, u64) {
    Mutex::into_inner(Arc::into_inner(capture).expect("sole owner after the turn ends"))
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .into_parts()
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn result_content(output: CodeOutput) -> CallToolResult {
    let text = summary(&output);
    let structured = serde_json::to_value(&output).expect("code output serializes");
    let is_error = output.outcome != Outcome::Completed;
    let mut result = if is_error {
        CallToolResult::error(vec![ContentBlock::text(text)])
    } else {
        CallToolResult::success(vec![ContentBlock::text(text)])
    };
    result.structured_content = Some(structured);
    result
}

/// The text block restates the machine-readable envelope for clients that only render text.
fn summary(output: &CodeOutput) -> String {
    let mut lines = Vec::new();
    if !output.stdout.is_empty() {
        lines.push(format!("stdout:\n{}", output.stdout));
    }
    if !output.stderr.is_empty() {
        lines.push(format!("stderr:\n{}", output.stderr));
    }
    match &output.exception {
        Some(exception) if exception.message.is_empty() => {
            lines.push(format!("raised {}", exception.r#type));
        }
        Some(exception) => {
            lines.push(format!(
                "raised {}: {}",
                exception.r#type, exception.message
            ));
        }
        None if output.outcome == Outcome::Completed => {
            let rendered = output
                .result_repr
                .clone()
                .unwrap_or_else(|| output.result.to_string());
            lines.push(format!("result: {rendered}"));
        }
        None => {}
    }
    if let Some(diagnostic) = &output.diagnostic {
        lines.push(diagnostic.clone());
    }
    if lines.is_empty() {
        return "Code executed and produced no output.".to_owned();
    }
    lines.join("\n")
}

fn tool_error(error: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(error.into())])
}

#[cfg(test)]
mod tests {
    use monty_pool::CrashCause;

    use super::*;

    /// A pool failure never becomes an MCP fault, and never leaks worker internals. The hard-kill
    /// paths cannot be provoked from Python on purpose — Monty preflights the allocations that would
    /// reach them — so the classification we own is asserted directly.
    fn failure_envelope(error: &PoolError) -> CodeOutput {
        CodeToolGroup::failure(true, error, DEFAULT_TIMEOUT_MS, Duration::from_millis(1))
    }

    fn vanished() -> PoolError {
        PoolError::Crashed {
            status: None,
            cause: CrashCause::Vanished {
                context: "reading turn events".into(),
            },
        }
    }

    #[test]
    fn a_crashed_worker_is_a_bounded_result_rather_than_a_fault() {
        let output = failure_envelope(&vanished());
        assert_eq!(output.outcome, Outcome::Unavailable);
        let diagnostic = output.diagnostic.as_deref().expect("guidance");
        assert!(diagnostic.contains("replaced"));
        assert!(diagnostic.contains("Retry"));
        assert_eq!(output.result, serde_json::Value::Null);
    }

    #[test]
    fn failure_diagnostics_never_disclose_worker_internals() {
        // `Spawn` is the variant that carries a resolved path in its message; it must not survive
        // into anything the caller sees, in line with the redaction rule for operator configuration.
        let output = failure_envelope(&PoolError::Spawn("/opt/secret/path/monty: denied".into()));
        assert_eq!(output.outcome, Outcome::Unavailable);
        let diagnostic = output.diagnostic.as_deref().expect("guidance");
        assert!(!diagnostic.contains("/opt/secret/path"));
        assert!(!diagnostic.contains("denied"));
    }

    #[test]
    fn exhaustion_is_retryable_and_distinct_from_a_crash() {
        let exhausted = failure_envelope(&PoolError::Exhausted);
        let crashed = failure_envelope(&vanished());
        assert_eq!(exhausted.outcome, Outcome::Unavailable);
        assert_ne!(exhausted.diagnostic, crashed.diagnostic);
        assert!(
            exhausted
                .diagnostic
                .as_deref()
                .expect("guidance")
                .contains("busy")
        );
    }
}
