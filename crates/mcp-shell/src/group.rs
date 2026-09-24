//! Shell tool orchestration.
//!
//! Execution is a small state machine with two independent completion conditions: the direct child
//! must have an exit status and both output pipes must close. Descendants can inherit pipes after the
//! child exits, so conflating these conditions can hang forever or discard trailing output.

#[cfg(feature = "mcp")]
use crate::{catalog, progress::mcp_progress_sink};
use crate::{
    output::{
        COMBINED_OUTPUT_BYTES, FALLBACK_PREVIEW_BYTES, OUTPUT_CHANNEL_CAPACITY, Tail, read_stream,
    },
    permission::{MAX_COMMAND_BYTES, ShellPermissionPolicy},
    process::{
        SharedShellLauncher, ShellLauncher, exit_signal, platform_command, terminate_and_reap,
        terminate_residual_group,
    },
    progress::{ProgressPump, ShellProgressSink, receive_failure},
    types::{
        DEFAULT_TIMEOUT_MS, MAX_TIMEOUT_MS, PreparedShell, ShellCommandAnalysis, ShellExecution,
        ShellFilterInfo, ShellInput, ShellOutput, ShellStream,
    },
    workdir,
};
#[cfg(feature = "mcp")]
use rmcp::{
    RoleServer,
    model::{CallToolResult, ContentBlock, ProgressToken, Tool},
    service::Peer,
};
#[cfg(feature = "mcp")]
use serde_json::Value;
use std::{
    fmt,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use workcell_host_contract::DirectExecOptions;
use workcell_output_filter::{Rule as FilterRule, collapse_progress_lines, strip_escape_sequences};

/// Name reported for the command-independent escape reduction.
const ESCAPE_STAGE: &str = "escapes";

/// Name reported for the command-independent progress reduction.
const PROGRESS_STAGE: &str = "progress";

const SHELL_CONCURRENCY: usize = 4;
// After child completion, allow trailing pipe data to reset this grace window before treating open
// pipes as evidence of residual descendants.
const PIPE_CLOSE_GRACE: Duration = Duration::from_millis(100);
fn concurrency() -> &'static Arc<Semaphore> {
    // The process-wide gate bounds aggregate process, pipe, and progress memory across tool groups.
    static SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();
    SEMAPHORE.get_or_init(|| Arc::new(Semaphore::new(SHELL_CONCURRENCY)))
}

#[derive(Clone, Debug)]
pub struct ShellToolGroup {
    root: PathBuf,
    policy: ShellPermissionPolicy,
    confined: bool,
    output_filter: bool,
    launcher: SharedShellLauncher,
}
#[derive(Debug)]
pub struct ShellBuildError;
impl fmt::Display for ShellBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("shell base cwd must be an existing directory")
    }
}
impl std::error::Error for ShellBuildError {}

impl ShellToolGroup {
    pub async fn new(root: impl AsRef<Path>) -> Result<Self, ShellBuildError> {
        Self::with_policy(root, ShellPermissionPolicy::restricted()).await
    }

    pub async fn with_policy(
        root: impl AsRef<Path>,
        policy: ShellPermissionPolicy,
    ) -> Result<Self, ShellBuildError> {
        Self::build(root.as_ref(), policy, true).await
    }

    /// Construct a host-managed group where `base_cwd` only anchors relative workdirs.
    ///
    /// Absolute and outside-the-base workdirs are accepted, so the host is responsible for
    /// authorizing the prepared workdir and scopes. Permission policy stays fail-closed at
    /// [`ShellPermissionPolicy::restricted`]; use [`Self::with_policy_unconfined`] to supply the
    /// host's own policy.
    pub async fn new_unconfined(base_cwd: impl AsRef<Path>) -> Result<Self, ShellBuildError> {
        Self::with_policy_unconfined(base_cwd, ShellPermissionPolicy::restricted()).await
    }

    /// Host-managed workdir resolution combined with a host-supplied permission policy.
    ///
    /// Relaxing workdir confinement and choosing a policy are separate decisions; this is the
    /// constructor for hosts that own both. Deny rules still reject a request before any command
    /// runs, exactly as in the confined server.
    pub async fn with_policy_unconfined(
        base_cwd: impl AsRef<Path>,
        policy: ShellPermissionPolicy,
    ) -> Result<Self, ShellBuildError> {
        Self::build(base_cwd.as_ref(), policy, false).await
    }

    async fn build(
        root: &Path,
        policy: ShellPermissionPolicy,
        confined: bool,
    ) -> Result<Self, ShellBuildError> {
        let root = workdir::canonicalize(root)
            .await
            .map_err(|_| ShellBuildError)?;
        if !tokio::fs::metadata(&root)
            .await
            .map_err(|_| ShellBuildError)?
            .is_dir()
        {
            return Err(ShellBuildError);
        }
        Ok(Self {
            root,
            policy,
            confined,
            output_filter: true,
            launcher: Arc::new(ShellLauncher::from_host().await),
        })
    }

    /// Enables or disables declarative filtering of the model-facing rendering.
    ///
    /// Filtering only changes the rendering. The structured result always
    /// carries the unfiltered capture, so disabling this cannot reveal output
    /// that was otherwise withheld, and enabling it cannot hide output a caller
    /// could not still read.
    #[must_use]
    pub const fn with_output_filter(mut self, enabled: bool) -> Self {
        self.output_filter = enabled;
        self
    }

    #[must_use]
    pub fn policy_summary(&self) -> crate::ShellPermissionPolicySummary {
        self.policy.summary()
    }

    pub fn authorize_prepared(&self, prepared: &PreparedShell) -> Result<(), String> {
        prepared.policy_decision().result()
    }

    #[must_use]
    #[cfg(feature = "mcp")]
    pub fn catalog(&self) -> Vec<Tool> {
        catalog::catalog()
    }
    #[cfg(feature = "mcp")]
    pub async fn dispatch(
        &self,
        name: &str,
        arguments: Value,
        cancellation: CancellationToken,
        progress: Option<(Peer<RoleServer>, ProgressToken)>,
    ) -> Option<Result<CallToolResult, rmcp::ErrorData>> {
        if name != "shell" {
            // Returning `None` lets an application compose this group with other MCP tool groups.
            return None;
        }
        let progress = progress.map(|(peer, token)| mcp_progress_sink(peer, token));
        self.dispatch_with_progress(name, arguments, cancellation, progress)
            .await
    }

    #[cfg(feature = "mcp")]
    pub async fn dispatch_with_progress(
        &self,
        name: &str,
        arguments: Value,
        cancellation: CancellationToken,
        progress: Option<Arc<dyn ShellProgressSink>>,
    ) -> Option<Result<CallToolResult, rmcp::ErrorData>> {
        if name != "shell" {
            return None;
        }
        let input = match serde_json::from_value::<ShellInput>(arguments) {
            Ok(input) => input,
            Err(e) => {
                return Some(Ok(tool_error(format!(
                    "Invalid arguments for tool shell: {e}"
                ))));
            }
        };
        let prepared = match self.prepare(input).await {
            Ok(prepared) => prepared,
            Err(error) => return Some(Ok(tool_error(error))),
        };
        if let Err(error) = self.authorize_prepared(&prepared) {
            return Some(Ok(tool_error(error)));
        }
        Some(Ok(
            match self
                .execute_prepared(prepared, cancellation, progress)
                .await
            {
                Ok(Some(execution)) => result_content(execution),
                Ok(None) => tool_error("Shell execution cancelled"),
                Err(e) => tool_error(e),
            },
        ))
    }

    /// Validate, inspect, and apply immutable policy without starting a process.
    pub async fn prepare(&self, input: ShellInput) -> Result<PreparedShell, String> {
        validate_command(&input.command)?;
        let timeout_ms = input.timeout_ms()?;
        self.bind(input.command, timeout_ms, input.workdir.as_deref())
            .await
    }

    /// Prepares a non-interactive host operation through the same immutable shell policy as the
    /// ordinary model-facing shell tool.
    pub async fn prepare_direct(
        &self,
        options: DirectExecOptions,
        relative_workdir: String,
    ) -> Result<PreparedShell, String> {
        let command = options.command.as_str().to_owned();
        validate_command(&command)?;
        let timeout_ms = direct_timeout_ms(options.timeout_ms)?;
        let prepared = self
            .bind(command, timeout_ms, Some(&relative_workdir))
            .await?;
        self.authorize_prepared(&prepared)?;
        Ok(prepared)
    }

    async fn bind(
        &self,
        command: String,
        timeout_ms: u64,
        requested_workdir: Option<&str>,
    ) -> Result<PreparedShell, String> {
        let requested_workdir = requested_workdir.unwrap_or(".");
        let workdir = if self.confined {
            workdir::resolve(&self.root, requested_workdir).await?
        } else {
            workdir::resolve_unconfined(&self.root, requested_workdir).await?
        };
        let (analysis, bash_program, policy_decision) = self.policy.prepare(&command);
        Ok(PreparedShell::new(
            command,
            timeout_ms,
            (analysis, bash_program),
            policy_decision,
            workdir,
            self.output_filter,
            Arc::clone(&self.launcher),
        ))
    }

    /// Execute after a native host has authorized the prepared scopes.
    ///
    /// ```compile_fail
    /// use tokio_util::sync::CancellationToken;
    /// use workcell_mcp_shell::{PreparedShell, ShellToolGroup};
    ///
    /// async fn execute_twice(group: &ShellToolGroup, prepared: PreparedShell) {
    ///     let _ = group.execute_prepared(prepared, CancellationToken::new(), None).await;
    ///     let _ = group.execute_prepared(prepared, CancellationToken::new(), None).await;
    /// }
    /// ```
    pub async fn execute_prepared(
        &self,
        prepared: PreparedShell,
        cancellation: CancellationToken,
        progress: Option<Arc<dyn ShellProgressSink>>,
    ) -> Result<Option<ShellExecution>, String> {
        let (command_text, timeout_ms, workdir, analysis, output_filter, launcher) =
            prepared.into_execution_parts();
        let launcher = launcher.as_ref().as_ref().map_err(ToString::to_string)?;
        let relative_workdir = workdir.relative().to_owned();
        let mut progress = progress.map(ProgressPump::start);
        // Queue admission remains cancellable; holding the permit through final progress drain keeps
        // all per-execution resources inside the global concurrency budget.
        let _permit = tokio::select! {permit=concurrency().clone().acquire_owned()=>permit.map_err(|_|"Shell concurrency gate is unavailable".to_owned())?,()=cancellation.cancelled()=>return Ok(None)};
        let started = Instant::now();
        let mut command = platform_command(launcher, &command_text);
        command
            .current_dir(workdir.canonical())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        tokio::select! {
            biased;
            () = cancellation.cancelled() => return Ok(None),
            result = async {
                workdir::revalidate(&workdir).await?;
                launcher.revalidate().await.map_err(|error| error.to_string())
            } => result?,
        }
        if cancellation.is_cancelled() {
            return Ok(None);
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("Failed to start shell: {e}"))?;
        let pid = child.id();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "Failed to capture stdout".to_owned())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "Failed to capture stderr".to_owned())?;
        let (sender, mut receiver) = mpsc::channel(OUTPUT_CHANNEL_CAPACITY);
        let stdout_task = tokio::spawn(read_stream(stdout, ShellStream::Stdout, sender.clone()));
        let stderr_task = tokio::spawn(read_stream(stderr, ShellStream::Stderr, sender));
        let mut stdout_tail = Tail::default();
        let mut stderr_tail = Tail::default();
        let mut total_bytes = 0_u64;
        let mut sequence = 0_u64;
        let mut stdout_utf8_bytes = 0_u64;
        let mut stderr_utf8_bytes = 0_u64;
        let mut timed_out = false;
        let mut output_limit_exceeded = false;
        let mut status = None;
        let mut pipes_closed = false;
        let mut pipe_close_deadline = None;
        let timeout = tokio::time::sleep(Duration::from_millis(timeout_ms));
        tokio::pin!(timeout);
        loop {
            // Biased ordering makes cancellation/progress failure/timeout win over fresh output when
            // several branches become ready together. Output cannot postpone a lifecycle decision.
            tokio::select! {biased;
             ()=cancellation.cancelled()=>{terminate_and_reap(&mut child,pid).await?;stdout_task.abort();stderr_task.abort();if let Some(progress)=progress{progress.task.abort();}return Ok(None);}
             error=receive_failure(&mut progress),if progress.is_some()=>{terminate_and_reap(&mut child,pid).await?;stdout_task.abort();stderr_task.abort();if let Some(progress)=progress{progress.task.abort();}return Err(error);}
             ()=&mut timeout,if !timed_out&&(status.is_none()||!pipes_closed)=>{timed_out=true;status=terminate_and_reap(&mut child,pid).await?.or(status);pipe_close_deadline=Some(tokio::time::Instant::now()+PIPE_CLOSE_GRACE);}
             result=child.wait(),if status.is_none()=>{status=Some(result.map_err(|e|format!("Failed to wait for shell: {e}"))?);pipe_close_deadline=Some(tokio::time::Instant::now()+PIPE_CLOSE_GRACE);}
              // An exited child does not imply EOF: descendants may still own pipe handles.
              ()=wait_for_pipe_deadline(pipe_close_deadline),if status.is_some()&&!pipes_closed=>{terminate_residual_group(pid).await;stdout_task.abort();stderr_task.abort();pipes_closed=true;}
             event=receiver.recv(),if !pipes_closed=>match event {
                 Some(event) => {
                     if status.is_some() {
                         pipe_close_deadline=Some(tokio::time::Instant::now()+PIPE_CLOSE_GRACE);
                     }
                      total_bytes=total_bytes.saturating_add(event.raw_bytes as u64);
                      // Enforce the budget on pre-decoding bytes across both streams. Tail capture is
                      // separately bounded, but without this limit a command could stream forever.
                      if total_bytes>COMBINED_OUTPUT_BYTES&&!output_limit_exceeded {
                         output_limit_exceeded=true;
                         status=terminate_and_reap(&mut child,pid).await?.or(status);
                         pipe_close_deadline=Some(tokio::time::Instant::now()+PIPE_CLOSE_GRACE);
                     }
                      if event.text.is_empty() { continue; }
                      sequence=sequence.saturating_add(1);
                      let utf8_bytes=u64::try_from(event.text.len()).unwrap_or(u64::MAX);
                      match event.stream {
                          ShellStream::Stdout=>stdout_utf8_bytes=stdout_utf8_bytes.saturating_add(utf8_bytes),
                          ShellStream::Stderr=>stderr_utf8_bytes=stderr_utf8_bytes.saturating_add(utf8_bytes)
                      }
                      if let Some(p)=&progress&&let Err(e)=p.enqueue(sequence,event.stream,&event.text) {
                         terminate_and_reap(&mut child,pid).await?;
                         stdout_task.abort(); stderr_task.abort(); p.task.abort(); return Err(e);
                     }
                     match event.stream {
                         ShellStream::Stdout=>stdout_tail.push(&event.text),
                         ShellStream::Stderr=>stderr_tail.push(&event.text)
                     }
                 },
                 None=>pipes_closed=true
             }
            }
            if status.is_some() && pipes_closed {
                // Both state-machine terminal conditions are required before producing a result.
                break;
            }
        }
        let _ = stdout_task.await;
        let _ = stderr_task.await;
        if let Some(progress) = progress {
            // Drain accepted progress before exposing the final result to preserve observable order.
            progress.finish().await?;
        }
        // A stream can end mid-row, either without a trailing newline or with a
        // bar still redrawing when the command exited.
        stdout_tail.finish();
        stderr_tail.finish();
        let (stdout, stdout_preview_truncated) = stdout_tail.preview(FALLBACK_PREVIEW_BYTES / 2);
        let (stderr, stderr_preview_truncated) = stderr_tail.preview(FALLBACK_PREVIEW_BYTES / 2);
        let output = ShellOutput {
            version: 1,
            kind: "shell",
            relative_workdir,
            timeout_ms,
            duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            exit_code: status.as_ref().and_then(ExitStatus::code),
            signal: status.as_ref().and_then(exit_signal),
            timed_out,
            output_limit_exceeded,
            final_sequence: sequence,
            stdout_utf8_bytes,
            stderr_utf8_bytes,
            stdout,
            stderr,
            stdout_capture_truncated: stdout_tail.truncated,
            stderr_capture_truncated: stderr_tail.truncated,
            stdout_preview_truncated,
            stderr_preview_truncated,
            stdout_redraws_collapsed: stdout_tail.redraws(),
            stderr_redraws_collapsed: stderr_tail.redraws(),
        };
        let (model_text, filter) = self.render_with_filter(
            &output,
            &analysis,
            &stdout_tail,
            &stderr_tail,
            output_filter,
        );
        Ok(Some(ShellExecution {
            output,
            model_text,
            filter,
        }))
    }

    /// Builds the model-facing rendering for a completed execution.
    ///
    /// Filtering is applied to the full retained tails rather than to the
    /// previews already placed in the structured result. Reducing first means
    /// the same preview budget carries what survived filtering instead of the
    /// raw end of the stream.
    #[cfg(all(test, feature = "mcp"))]
    fn render(
        &self,
        output: &ShellOutput,
        analysis: &ShellCommandAnalysis,
        stdout_tail: &Tail,
        stderr_tail: &Tail,
    ) -> String {
        self.render_with_filter(
            output,
            analysis,
            stdout_tail,
            stderr_tail,
            self.output_filter,
        )
        .0
    }

    /// Builds the model-facing rendering from the retained tails.
    ///
    /// Three reductions can apply. A corpus rule is selected by command and
    /// knows the format it is reading. The escape strip and the progress
    /// collapse are command-independent, because the commands that emit
    /// decoration and bars are overwhelmingly ones no rule names — a deploy
    /// script or an ad-hoc program — and a rule cannot be written for a program
    /// that does not exist yet.
    fn render_with_filter(
        &self,
        output: &ShellOutput,
        analysis: &ShellCommandAnalysis,
        stdout_tail: &Tail,
        stderr_tail: &Tail,
        output_filter: bool,
    ) -> (String, Option<ShellFilterInfo>) {
        let unfiltered = model_text(output);
        if !output_filter {
            return (unfiltered, None);
        }
        let mut stages: Vec<String> = Vec::new();

        // Both reductions read the full retained window rather than the preview
        // already in the structured result, so the preview budget is spent on
        // what survived instead of on the raw end of the stream.
        let stdout = stdout_tail.text();
        let stderr = stderr_tail.text();

        // Runs before rule selection for two reasons. Rule patterns are authored
        // against clean text, so a coloured line silently evades a
        // `strip_lines_matching` that was written to catch it; and the progress
        // collapse compares line shapes, which decoration makes incomparable.
        let (stdout, stdout_escapes) = strip_escape_sequences(&stdout);
        let (stderr, stderr_escapes) = strip_escape_sequences(&stderr);
        if stdout_escapes > 0 || stderr_escapes > 0 {
            stages.push(ESCAPE_STAGE.to_owned());
        }

        let mut consumed_stderr = false;
        let mut body = stdout;
        if let Some(rule) = Self::matching_rule(analysis, output_filter) {
            let filtered = rule.apply(&body, &stderr, output.exit_code);
            // A rule that matched but removed nothing is not announced.
            // Announcing a filter that did not filter would tell a reader to go
            // looking for output that was never dropped.
            if filtered.lossy {
                body = filtered.text;
                consumed_stderr = filtered.consumed_stderr;
                stages.push(rule.name().to_owned());
            }
        }

        let mut rendered = body;
        if !consumed_stderr && !stderr.is_empty() {
            // A rule that describes stdout must not make a diagnostic written to
            // stderr disappear from the rendering.
            if !rendered.is_empty() {
                rendered.push('\n');
            }
            rendered.push_str("stderr tail:\n");
            rendered.push_str(&stderr);
        }

        // Runs last so a rule that already knows the format gets first refusal,
        // and so the collapse sees whatever that rule left behind.
        let (collapsed, removed) = collapse_progress_lines(&rendered);
        if removed > 0 {
            rendered = collapsed;
            stages.push(PROGRESS_STAGE.to_owned());
        }

        if stages.is_empty() || rendered.trim().is_empty() {
            return (unfiltered, None);
        }
        // The notice is deliberately terse. It is paid on every filtered result
        // and is compared against the complete capture below, so a verbose
        // notice would stop small reductions from ever being worth taking.
        let candidate = format!(
            "{}\n[filtered: {}]",
            bound_rendering(&rendered),
            stages.join(", ")
        );
        // Filtering exists to reduce what a model reads, and the notice is not
        // free. A rendering that is not smaller than the complete capture is
        // strictly worse than it: it costs more and says less.
        if candidate.len() >= unfiltered.len() {
            return (unfiltered, None);
        }
        let filter = ShellFilterInfo {
            stages,
            unfiltered_utf8_bytes: unfiltered.len(),
            filtered_utf8_bytes: candidate.len(),
        };
        (candidate, Some(filter))
    }

    /// Selects the rule for a command, or `None` when none should apply.
    ///
    /// A rule is only used for a request that resolves to exactly one
    /// classified command scope. In a pipeline the captured output belongs to
    /// the last stage rather than the program a rule names, and in a chain it
    /// belongs to several programs at once, so applying a single rule would
    /// describe output it did not produce.
    fn matching_rule(
        analysis: &ShellCommandAnalysis,
        output_filter: bool,
    ) -> Option<&'static FilterRule> {
        if !output_filter || analysis.opaque {
            return None;
        }
        let [scope] = analysis.scopes.as_slice() else {
            return None;
        };
        workcell_output_filter::builtin().find(&scope.normalized)
    }

    pub async fn execute(
        &self,
        input: ShellInput,
        cancellation: CancellationToken,
        progress: Option<Arc<dyn ShellProgressSink>>,
    ) -> Result<Option<ShellExecution>, String> {
        let prepared = self.prepare(input).await?;
        self.authorize_prepared(&prepared)?;
        self.execute_prepared(prepared, cancellation, progress)
            .await
    }
}

fn validate_command(command: &str) -> Result<(), String> {
    if command.trim().is_empty() {
        return Err("Invalid arguments: command must not be empty".into());
    }
    if command.len() > MAX_COMMAND_BYTES {
        return Err(format!(
            "Invalid arguments for tool shell: command is {} UTF-8 bytes; maximum is {MAX_COMMAND_BYTES}. Split the operation into smaller shell calls",
            command.len()
        ));
    }
    Ok(())
}

/// The rule `ShellInput::timeout_ms` applies, zero included, read in the milliseconds the host
/// contract counts in.
fn direct_timeout_ms(requested: Option<u64>) -> Result<u64, String> {
    match requested {
        None => Ok(DEFAULT_TIMEOUT_MS),
        Some(requested @ 1..=MAX_TIMEOUT_MS) => Ok(requested),
        Some(requested) => Err(format!(
            "Invalid arguments: timeout is {requested} milliseconds; it must be between 1 and {MAX_TIMEOUT_MS}. Omit it for the {DEFAULT_TIMEOUT_MS} millisecond default"
        )),
    }
}

async fn wait_for_pipe_deadline(deadline: Option<tokio::time::Instant>) {
    if let Some(deadline) = deadline {
        tokio::time::sleep_until(deadline).await
    } else {
        std::future::pending().await
    }
}
/// Caps a filtered rendering at the same budget the unfiltered one uses.
///
/// A rule reduces output but does not guarantee a bound, so the rendering is
/// capped independently. The tail is kept for the same reason the capture ring
/// keeps it: failures and summaries appear last.
fn bound_rendering(rendered: &str) -> &str {
    if rendered.len() <= FALLBACK_PREVIEW_BYTES {
        return rendered;
    }
    let start = rendered.len() - FALLBACK_PREVIEW_BYTES;
    let start = (start..rendered.len())
        .find(|index| rendered.is_char_boundary(*index))
        .unwrap_or(rendered.len());
    &rendered[start..]
}

fn model_text(output: &ShellOutput) -> String {
    let redraws = output
        .stdout_redraws_collapsed
        .saturating_add(output.stderr_redraws_collapsed);
    // Rendering a redraw stream is faithful but not lossless: the intermediate
    // frames are gone. Saying so costs one line and stops a reader from assuming
    // the command only ever printed the frame it can see.
    let note = if redraws == 0 {
        String::new()
    } else {
        format!("\n[{redraws} progress redraws collapsed]")
    };
    if output.stdout.is_empty() && output.stderr.is_empty() {
        format!(
            "Command exited with code {} and produced no output.{note}",
            output
                .exit_code
                .map_or_else(|| "unknown".into(), |v| v.to_string())
        )
    } else {
        format!(
            "stdout tail:\n{}\nstderr tail:\n{}{note}",
            output.stdout, output.stderr
        )
    }
}

#[cfg(feature = "mcp")]
fn result_content(execution: ShellExecution) -> CallToolResult {
    let structured = serde_json::to_value(&execution.output).expect("shell output serializes");
    let mut result = CallToolResult::success(vec![ContentBlock::text(execution.model_text)]);
    result.structured_content = Some(structured);
    result
}
#[cfg(feature = "mcp")]
fn tool_error(error: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(error.into())])
}

#[cfg(all(test, feature = "mcp"))]
mod tests {
    use super::*;
    use crate::types::{DEFAULT_TIMEOUT_SECS, MAX_TIMEOUT_SECS, MILLIS_PER_SECOND};
    use serde_json::json;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingProgress {
        chunks: Mutex<Vec<crate::ShellProgressChunk>>,
    }

    #[async_trait::async_trait]
    impl ShellProgressSink for RecordingProgress {
        async fn publish(&self, chunk: crate::ShellProgressChunk) -> Result<(), String> {
            self.chunks.lock().unwrap().push(chunk);
            Ok(())
        }
    }

    /// Builds a completed result carrying the given previews. Callers pass the
    /// same text they push into the corresponding tail, because in execution the
    /// previews are derived from the tails and the two cannot disagree.
    fn rendered_output(stdout: &str, stderr: &str, exit_code: Option<i32>) -> ShellOutput {
        ShellOutput {
            version: 1,
            kind: "shell",
            relative_workdir: ".".into(),
            timeout_ms: DEFAULT_TIMEOUT_MS,
            duration_ms: 0,
            exit_code,
            signal: None,
            timed_out: false,
            output_limit_exceeded: false,
            final_sequence: 0,
            stdout_utf8_bytes: 0,
            stderr_utf8_bytes: 0,
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
            stdout_capture_truncated: false,
            stderr_capture_truncated: false,
            stdout_preview_truncated: false,
            stderr_preview_truncated: false,
            stdout_redraws_collapsed: 0,
            stderr_redraws_collapsed: 0,
        }
    }

    fn tail(text: &str) -> Tail {
        let mut tail = Tail::default();
        tail.push(text);
        tail.finish();
        tail
    }

    const MAKE_OUTPUT: &str =
        "make[1]: Entering directory '/x'\ngcc -O2 foo.c\nmake[1]: Leaving directory '/x'\n";
    const CARGO_TEST_STDOUT: &str = "running 2 tests\ntest sdk_mode::tests::wire_init ... ok\ntest sdk_mode::tests::wire_result ... ok\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n";
    const CARGO_TEST_STDERR: &str = "warning: future incompatibility\n";
    const PREPARED_TIMEOUT_SECS: u64 = 321;
    const HOURS_LONG_TIMEOUT_SECS: u64 = 10_800;

    async fn group_for_render(output_filter: bool) -> (tempfile::TempDir, ShellToolGroup) {
        let root = tempfile::tempdir().unwrap();
        let group = ShellToolGroup::new(root.path())
            .await
            .unwrap()
            .with_output_filter(output_filter);
        (root, group)
    }

    #[tokio::test]
    async fn a_matched_single_scope_is_filtered_and_labelled() {
        let (_root, group) = group_for_render(true).await;
        let analysis = crate::permission::inspect("make build");
        let rendered = group.render(
            &rendered_output(MAKE_OUTPUT, "", Some(0)),
            &analysis,
            &tail(MAKE_OUTPUT),
            &Tail::default(),
        );
        assert!(rendered.starts_with("gcc -O2 foo.c"), "{rendered}");
        assert!(!rendered.contains("Entering directory"), "{rendered}");
        assert!(rendered.contains("[filtered: make]"), "{rendered}");
    }

    #[tokio::test]
    async fn cargo_test_hides_passing_rows_and_keeps_summary_and_warnings() {
        let (_root, group) = group_for_render(true).await;
        let analysis = crate::permission::inspect("cargo test");
        let rendered = group.render(
            &rendered_output(CARGO_TEST_STDOUT, CARGO_TEST_STDERR, Some(0)),
            &analysis,
            &tail(CARGO_TEST_STDOUT),
            &tail(CARGO_TEST_STDERR),
        );

        assert!(!rendered.contains("wire_init"), "{rendered}");
        assert!(!rendered.contains("wire_result"), "{rendered}");
        assert!(rendered.contains("test result: ok. 2 passed"), "{rendered}");
        assert!(
            rendered.contains("warning: future incompatibility"),
            "{rendered}"
        );
        assert!(rendered.contains("[filtered: cargo-test]"), "{rendered}");
    }

    #[tokio::test]
    async fn filtering_never_returns_a_larger_rendering() {
        let (_root, group) = group_for_render(true).await;
        // A matched rule that strips nothing, or strips less than the notice
        // costs, must fall back to the complete capture. Otherwise announcing
        // the filter makes the rendering larger than the output it reduced.
        let clean = "gcc -O2 foo.c\n";
        let analysis = crate::permission::inspect("make build");
        let rendered = group.render(
            &rendered_output(clean, "", Some(0)),
            &analysis,
            &tail(clean),
            &Tail::default(),
        );
        assert_eq!(rendered, format!("stdout tail:\n{clean}\nstderr tail:\n"));
        assert!(!rendered.contains("[filtered:"), "{rendered}");
    }

    #[tokio::test]
    async fn multi_scope_requests_are_never_filtered() {
        let (_root, group) = group_for_render(true).await;
        // The capture belongs to the last stage of a pipeline and to several
        // programs in a chain, so no single rule describes it.
        for command in ["make build | cat", "make build && echo done"] {
            let analysis = crate::permission::inspect(command);
            let rendered = group.render(
                &rendered_output(MAKE_OUTPUT, "", Some(0)),
                &analysis,
                &tail(MAKE_OUTPUT),
                &Tail::default(),
            );
            assert!(rendered.contains("Entering directory"), "{command}");
            assert!(!rendered.contains("[filtered:"), "{command}");
        }
    }

    #[tokio::test]
    async fn opaque_commands_are_never_filtered() {
        let (_root, group) = group_for_render(true).await;
        let analysis = crate::permission::inspect("eval \"$CMD\"");
        assert!(analysis.opaque);
        let rendered = group.render(
            &rendered_output(MAKE_OUTPUT, "", Some(0)),
            &analysis,
            &tail(MAKE_OUTPUT),
            &Tail::default(),
        );
        assert!(rendered.contains("Entering directory"));
    }

    #[tokio::test]
    async fn disabling_the_filter_restores_the_unfiltered_rendering() {
        let (_root, group) = group_for_render(false).await;
        let analysis = crate::permission::inspect("make build");
        let rendered = group.render(
            &rendered_output(MAKE_OUTPUT, "", Some(0)),
            &analysis,
            &tail(MAKE_OUTPUT),
            &Tail::default(),
        );
        assert_eq!(
            rendered,
            format!("stdout tail:\n{MAKE_OUTPUT}\nstderr tail:\n")
        );
    }

    #[tokio::test]
    async fn a_failing_command_is_never_rendered_as_success() {
        let (_root, group) = group_for_render(true).await;
        let analysis = crate::permission::inspect("make build");
        // Every line of this capture is stripped by the rule, which would
        // otherwise emit the rule's `on_empty` success message.
        let stripped = "make[1]: Entering directory '/x'\n";
        let rendered = group.render(
            &rendered_output(stripped, "ld: undefined reference", Some(2)),
            &analysis,
            &tail(stripped),
            &Tail::default(),
        );
        assert!(!rendered.contains("make: ok"), "{rendered}");
        assert!(rendered.contains("ld: undefined reference"), "{rendered}");
    }

    #[tokio::test]
    async fn stderr_survives_a_rule_that_only_describes_stdout() {
        let (_root, group) = group_for_render(true).await;
        let analysis = crate::permission::inspect("make build");
        let rendered = group.render(
            &rendered_output(MAKE_OUTPUT, "warning: deprecated", Some(0)),
            &analysis,
            &tail(MAKE_OUTPUT),
            &tail("warning: deprecated"),
        );
        assert!(rendered.contains("gcc -O2 foo.c"), "{rendered}");
        assert!(rendered.contains("warning: deprecated"), "{rendered}");
    }

    #[tokio::test]
    async fn filtering_leaves_the_structured_capture_unfiltered() {
        let root = tempfile::tempdir().unwrap();
        let group = ShellToolGroup::with_policy(root.path(), ShellPermissionPolicy::yolo())
            .await
            .unwrap();
        let result = call(
            &group,
            json!({"command":"printf 'make[1]: Entering directory\\ngcc -O2 foo.c\\n'"}),
        )
        .await;
        let structured = result.structured_content.unwrap();
        // `printf` matches no rule, so this also pins that an unmatched command
        // keeps the unfiltered rendering.
        assert!(
            structured["stdout"]
                .as_str()
                .unwrap()
                .contains("Entering directory")
        );
    }

    async fn call(group: &ShellToolGroup, args: Value) -> CallToolResult {
        group
            .dispatch("shell", args, CancellationToken::new(), None)
            .await
            .unwrap()
            .unwrap()
    }
    #[tokio::test]
    async fn strict_result() {
        let root = tempfile::tempdir().unwrap();
        let group = ShellToolGroup::with_policy(root.path(), ShellPermissionPolicy::yolo())
            .await
            .unwrap();
        let result = call(&group, json!({"command":"printf hello"})).await;
        let output = result.structured_content.unwrap();
        assert_eq!(output["version"], 1);
        assert_eq!(output["kind"], "shell");
        assert_eq!(output["finalSequence"], 1);
        assert_eq!(output["stdoutUtf8Bytes"], 5);
        assert_eq!(output["stderrUtf8Bytes"], 0);
        assert_eq!(output["relativeWorkdir"], ".");
        assert_eq!(output["timeoutMs"], DEFAULT_TIMEOUT_MS);
        assert_eq!(
            call(&group, json!({"command":"pwd","workdir":""}))
                .await
                .structured_content
                .unwrap()["relativeWorkdir"],
            "."
        );
        assert_eq!(
            call(&group, json!({"command":"true","extra":1}))
                .await
                .is_error,
            Some(true)
        );
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn denied_scope_prevents_every_command_in_the_request() {
        let root = tempfile::tempdir().unwrap();
        let policy = ShellPermissionPolicy::from_toml(
            "version = 1\nallow = ['printf *']\ndeny = ['rm *']\n",
            false,
        )
        .unwrap();
        let group = ShellToolGroup::with_policy(root.path(), policy)
            .await
            .unwrap();
        let sentinel = root.path().join("must-not-exist");
        let command = format!(
            "printf started > '{}' && rm -rf ./anything",
            sentinel.display()
        );

        let result = call(&group, json!({"command":command})).await;

        assert_eq!(result.is_error, Some(true));
        let error = serde_json::to_string(&result).unwrap();
        assert!(error.contains("Workcell operator"));
        assert!(error.contains("tool arguments cannot override"));
        assert!(!sentinel.exists());
    }
    #[tokio::test]
    async fn admission_errors_are_actionable_tool_results() {
        let root = tempfile::tempdir().unwrap();
        let restricted = ShellToolGroup::new(root.path()).await.unwrap();
        let required = call(&restricted, json!({"command":"printf hello"})).await;
        assert_eq!(required.is_error, Some(true));
        let required = serde_json::to_string(&required).unwrap();
        assert!(required.contains("requires an allow rule"));
        assert!(required.contains("tool arguments cannot approve"));

        let yolo = ShellToolGroup::with_policy(root.path(), ShellPermissionPolicy::yolo())
            .await
            .unwrap();
        let too_long = call(&yolo, json!({"command":"x".repeat(MAX_COMMAND_BYTES + 1)})).await;
        assert_eq!(too_long.is_error, Some(true));
        let too_long = serde_json::to_string(&too_long).unwrap();
        assert!(too_long.contains("65537 UTF-8 bytes"));
        assert!(too_long.contains("maximum is 65536"));
        assert!(too_long.contains("Split the operation"));
    }
    #[tokio::test]
    async fn native_prepare_exposes_scopes_and_does_not_execute() {
        let root = tempfile::tempdir().unwrap();
        let group = ShellToolGroup::new(root.path()).await.unwrap();
        let marker = root.path().join("not-created");
        let prepared = group
            .prepare(ShellInput {
                command: format!("printf prepared > '{}'", marker.display()),
                timeout_sec: None,
                workdir: None,
            })
            .await
            .unwrap();

        assert_eq!(prepared.workdir(), root.path().canonicalize().unwrap());
        assert_eq!(prepared.relative_workdir(), ".");
        assert_eq!(prepared.timeout_ms(), DEFAULT_TIMEOUT_MS);
        assert!(prepared.output_filter_enabled());
        assert_eq!(prepared.analysis().scopes.len(), 1);
        assert_eq!(prepared.analysis().scopes[0].permission, "printf *");
        assert!(!prepared.policy_decision().is_allowed());
        assert!(prepared.policy_decision().denial_reason().is_some());
        assert!(prepared.retained_bytes() >= prepared.command().len());
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn prepared_execution_binds_timeout_workdir_policy_and_progress() {
        let root = tempfile::tempdir().unwrap();
        let workdir = root.path().join("bound");
        std::fs::create_dir(&workdir).unwrap();
        let group = ShellToolGroup::with_policy(root.path(), ShellPermissionPolicy::yolo())
            .await
            .unwrap();
        let prepared = group
            .prepare(ShellInput {
                command: "printf bound".into(),
                timeout_sec: Some(PREPARED_TIMEOUT_SECS),
                workdir: Some("bound".into()),
            })
            .await
            .unwrap();
        assert_eq!(
            prepared.timeout_ms(),
            PREPARED_TIMEOUT_SECS * MILLIS_PER_SECOND
        );
        assert_eq!(prepared.workdir(), workdir.canonicalize().unwrap());
        assert_eq!(prepared.relative_workdir(), "bound");
        assert!(prepared.policy_decision().is_allowed());
        let progress = Arc::new(RecordingProgress::default());

        let execution = group
            .execute_prepared(prepared, CancellationToken::new(), Some(progress.clone()))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            execution.output.timeout_ms,
            PREPARED_TIMEOUT_SECS * MILLIS_PER_SECOND
        );
        assert_eq!(execution.output.relative_workdir, "bound");
        assert_eq!(execution.output.stdout, "bound");
        let chunks = progress.chunks.lock().unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "bound");
    }

    #[tokio::test]
    async fn an_omitted_timeout_defaults_and_an_asked_one_is_kept() {
        let root = tempfile::tempdir().unwrap();
        let group = ShellToolGroup::with_policy(root.path(), ShellPermissionPolicy::yolo())
            .await
            .unwrap();
        for (requested, expected) in [
            (None, DEFAULT_TIMEOUT_MS),
            (Some(1), MILLIS_PER_SECOND),
            (
                Some(HOURS_LONG_TIMEOUT_SECS),
                HOURS_LONG_TIMEOUT_SECS * MILLIS_PER_SECOND,
            ),
            (Some(MAX_TIMEOUT_SECS), MAX_TIMEOUT_MS),
        ] {
            let prepared = group
                .prepare(ShellInput {
                    command: "printf bounded".into(),
                    timeout_sec: requested,
                    workdir: None,
                })
                .await
                .unwrap();
            assert_eq!(prepared.timeout_ms(), expected, "{requested:?}");
        }
    }

    /// Zero is refused with anything above the maximum, and the refusal names the range and the
    /// default so the caller can correct the call without guessing.
    #[tokio::test]
    async fn a_timeout_outside_the_range_is_refused_with_its_value_and_the_bounds() {
        let root = tempfile::tempdir().unwrap();
        let group = ShellToolGroup::with_policy(root.path(), ShellPermissionPolicy::yolo())
            .await
            .unwrap();
        for requested in [0, MAX_TIMEOUT_SECS + 1] {
            let error = group
                .prepare(ShellInput {
                    command: "printf refused".into(),
                    timeout_sec: Some(requested),
                    workdir: None,
                })
                .await
                .unwrap_err();
            assert!(
                error.contains(&format!("timeoutSec is {requested} seconds")),
                "{error}"
            );
            assert!(
                error.contains(&format!("between 1 and {MAX_TIMEOUT_SECS}")),
                "{error}"
            );
            assert!(
                error.contains(&format!("{DEFAULT_TIMEOUT_SECS} second default")),
                "{error}"
            );
        }
    }

    #[tokio::test]
    async fn a_zero_timeout_is_refused_before_the_command_runs() {
        let root = tempfile::tempdir().unwrap();
        let group = ShellToolGroup::with_policy(root.path(), ShellPermissionPolicy::yolo())
            .await
            .unwrap();
        let marker = root.path().join("must-not-run");
        let command = format!("printf ran > '{}'", marker.display());

        let result = call(&group, json!({"command":command,"timeoutSec":0})).await;

        assert_eq!(result.is_error, Some(true));
        assert!(!marker.exists());
    }

    /// The key names its unit, so a millisecond count sent as `timeout` is refused rather than
    /// read as seconds. The count is in range for `timeoutSec`, so only the key can refuse it.
    #[tokio::test]
    async fn a_timeout_under_the_millisecond_key_is_refused_before_the_command_runs() {
        let root = tempfile::tempdir().unwrap();
        let group = ShellToolGroup::with_policy(root.path(), ShellPermissionPolicy::yolo())
            .await
            .unwrap();
        let marker = root.path().join("must-not-run");
        let command = format!("printf ran > '{}'", marker.display());

        let result = call(
            &group,
            json!({"command":command,"timeout":MAX_TIMEOUT_SECS}),
        )
        .await;

        assert_eq!(result.is_error, Some(true));
        let error = serde_json::to_string(&result).unwrap();
        assert!(error.contains("unknown field `timeout`"), "{error}");
        assert!(error.contains("timeoutSec"), "{error}");
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn a_direct_host_operation_refuses_zero_too() {
        let root = tempfile::tempdir().unwrap();
        let group = ShellToolGroup::with_policy(root.path(), ShellPermissionPolicy::yolo())
            .await
            .unwrap();
        let error = group
            .prepare_direct(
                DirectExecOptions {
                    command: workcell_host_contract::CommandText::new("printf direct").unwrap(),
                    timeout_ms: Some(0),
                },
                ".".to_owned(),
            )
            .await
            .unwrap_err();
        assert!(error.contains("timeout is 0 milliseconds"), "{error}");
    }

    #[tokio::test]
    async fn cancellation_before_prepared_spawn_has_no_side_effect() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("cancelled");
        let group = ShellToolGroup::with_policy(root.path(), ShellPermissionPolicy::yolo())
            .await
            .unwrap();
        let prepared = group
            .prepare(ShellInput {
                command: format!("printf ran > '{}'", marker.display()),
                timeout_sec: None,
                workdir: None,
            })
            .await
            .unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        assert!(
            group
                .execute_prepared(prepared, cancellation, None)
                .await
                .unwrap()
                .is_none()
        );
        assert!(!marker.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn prepared_workdir_rejects_symlink_retarget_before_spawn() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        let link = root.path().join("work");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        symlink(&first, &link).unwrap();
        let marker = root.path().join("must-not-run");
        let group = ShellToolGroup::with_policy(root.path(), ShellPermissionPolicy::yolo())
            .await
            .unwrap();
        let prepared = group
            .prepare(ShellInput {
                command: format!("printf ran > '{}'", marker.display()),
                timeout_sec: None,
                workdir: Some("work".into()),
            })
            .await
            .unwrap();
        std::fs::remove_file(&link).unwrap();
        symlink(&second, &link).unwrap();

        let error = group
            .execute_prepared(prepared, CancellationToken::new(), None)
            .await
            .unwrap_err();

        assert_eq!(error, workdir::STALE_WORKDIR_ERROR);
        assert!(!marker.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn prepared_workdir_rejects_same_path_directory_replacement() {
        let root = tempfile::tempdir().unwrap();
        let selected = root.path().join("selected");
        let original = root.path().join("original");
        std::fs::create_dir(&selected).unwrap();
        let marker = root.path().join("must-not-run");
        let group = ShellToolGroup::with_policy(root.path(), ShellPermissionPolicy::yolo())
            .await
            .unwrap();
        let prepared = group
            .prepare(ShellInput {
                command: format!("printf ran > '{}'", marker.display()),
                timeout_sec: None,
                workdir: Some("selected".into()),
            })
            .await
            .unwrap();
        std::fs::rename(&selected, &original).unwrap();
        std::fs::create_dir(&selected).unwrap();

        let error = group
            .execute_prepared(prepared, CancellationToken::new(), None)
            .await
            .unwrap_err();

        assert_eq!(error, workdir::STALE_WORKDIR_ERROR);
        assert!(!marker.exists());
    }
    #[tokio::test]
    async fn unconfined_native_prepare_accepts_absolute_and_outside_workdirs() {
        let temporary = tempfile::tempdir().unwrap();
        let base = temporary.path().join("base");
        let outside = temporary.path().join("outside");
        std::fs::create_dir(&base).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let group = ShellToolGroup::new_unconfined(&base).await.unwrap();

        let absolute = group
            .prepare(ShellInput {
                command: "pwd".into(),
                timeout_sec: None,
                workdir: Some(outside.to_string_lossy().into_owned()),
            })
            .await
            .unwrap();
        assert_eq!(absolute.workdir(), outside.canonicalize().unwrap());
        assert_eq!(
            absolute.relative_workdir(),
            outside.canonicalize().unwrap().to_string_lossy()
        );

        let relative = group
            .prepare(ShellInput {
                command: "pwd".into(),
                timeout_sec: None,
                workdir: Some("../outside".into()),
            })
            .await
            .unwrap();
        assert_eq!(relative.workdir(), outside.canonicalize().unwrap());
    }
    #[tokio::test]
    async fn unconfined_workdir_and_permission_policy_are_independent_choices() {
        let temporary = tempfile::tempdir().unwrap();
        let base = temporary.path().join("base");
        let outside = temporary.path().join("outside");
        std::fs::create_dir(&base).unwrap();
        std::fs::create_dir(&outside).unwrap();

        // Relaxing workdir confinement must not silently relax policy.
        let default = ShellToolGroup::new_unconfined(&base).await.unwrap();
        assert_eq!(default.policy_summary().default_decision, "deny");
        assert!(!default.policy_summary().yolo);

        // ...and a host that owns policy must be able to supply it alongside an outside workdir.
        let hosted = ShellToolGroup::with_policy_unconfined(&base, ShellPermissionPolicy::yolo())
            .await
            .unwrap();
        assert!(hosted.policy_summary().yolo);

        let result = call(
            &hosted,
            json!({"command":"printf hosted","workdir":outside.to_string_lossy()}),
        )
        .await;
        let output = result.structured_content.unwrap();
        assert_eq!(output["stdout"], "hosted");

        let denied = call(
            &default,
            json!({"command":"printf denied","workdir":outside.to_string_lossy()}),
        )
        .await;
        assert_eq!(denied.is_error, Some(true));
    }
    #[tokio::test]
    async fn trusted_native_execution_uses_host_authorization_instead_of_workcell_policy() {
        let root = tempfile::tempdir().unwrap();
        let group = ShellToolGroup::new(root.path()).await.unwrap();
        let prepared = group
            .prepare(ShellInput {
                command: "printf native".into(),
                timeout_sec: None,
                workdir: None,
            })
            .await
            .unwrap();

        let execution = group
            .execute_prepared(prepared, CancellationToken::new(), None)
            .await
            .unwrap()
            .expect("not cancelled");

        assert_eq!(execution.output.stdout, "native");
        assert_eq!(execution.model_text, "stdout tail:\nnative\nstderr tail:\n");
    }
    #[tokio::test]
    async fn bounded_tails() {
        let root = tempfile::tempdir().unwrap();
        let group = ShellToolGroup::with_policy(root.path(), ShellPermissionPolicy::yolo())
            .await
            .unwrap();
        let result = call(
            &group,
            json!({"command":"printf START; yes x | head -c 1100000; printf END"}),
        )
        .await;
        let serialized = serde_json::to_vec(&result).unwrap();
        let output = result.structured_content.unwrap();
        assert!(output["stdoutCaptureTruncated"].as_bool().unwrap());
        assert!(output["stdout"].as_str().unwrap().ends_with("END"));
        assert!(!output["stdout"].as_str().unwrap().contains("START"));
        assert!(serialized.len() < 64_000);
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn output_limit_terminates_command_and_reports_cause() {
        let root = tempfile::tempdir().unwrap();
        let group = ShellToolGroup::with_policy(root.path(), ShellPermissionPolicy::yolo())
            .await
            .unwrap();
        let command = format!("yes x | head -c {}", COMBINED_OUTPUT_BYTES + 1_048_576);
        let output = call(&group, json!({"command":command}))
            .await
            .structured_content
            .unwrap();

        assert_eq!(output["outputLimitExceeded"], true);
        assert_eq!(output["timedOut"], false);
        assert!(output["finalSequence"].as_u64().unwrap() > 0);
    }
    #[tokio::test]
    async fn timeout_and_escape() {
        let root = tempfile::tempdir().unwrap();
        let group = ShellToolGroup::with_policy(root.path(), ShellPermissionPolicy::yolo())
            .await
            .unwrap();
        assert_eq!(
            call(&group, json!({"command":"true","workdir":".."}))
                .await
                .is_error,
            Some(true)
        );
        assert!(run_direct(&group, "sleep 2", 10).await.timed_out);
    }

    /// Runs under the host's millisecond deadline, the one unit fine enough to time a command
    /// out without slowing the suite by a second per case.
    async fn run_direct(group: &ShellToolGroup, command: &str, timeout_ms: u64) -> ShellOutput {
        let options = DirectExecOptions {
            command: workcell_host_contract::CommandText::new(command).unwrap(),
            timeout_ms: Some(timeout_ms),
        };
        let prepared = group.prepare_direct(options, ".".to_owned()).await.unwrap();
        group
            .execute_prepared(prepared, CancellationToken::new(), None)
            .await
            .unwrap()
            .unwrap()
            .output
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_descendant() {
        let root = tempfile::tempdir().unwrap();
        let group = ShellToolGroup::with_policy(root.path(), ShellPermissionPolicy::yolo())
            .await
            .unwrap();
        let sentinel = root.path().join("descendant-survived");
        let command = format!(
            "(trap '' TERM; sleep 10; printf survived > '{}') & wait",
            sentinel.display()
        );
        assert!(run_direct(&group, &command, 20).await.timed_out);
        assert!(!sentinel.exists());
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!sentinel.exists());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn closed_pipes_do_not_bypass_timeout() {
        let root = tempfile::tempdir().unwrap();
        let group = ShellToolGroup::with_policy(root.path(), ShellPermissionPolicy::yolo())
            .await
            .unwrap();
        let sentinel = root.path().join("late-sentinel");
        let command = format!(
            "exec 1>&- 2>&-; sleep 1; printf late > '{}'",
            sentinel.display()
        );
        assert!(run_direct(&group, &command, 20).await.timed_out);
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(!sentinel.exists());
    }
}
