//! Shell tool orchestration.
//!
//! Execution is a small state machine with two independent completion conditions: the direct child
//! must have an exit status and both output pipes must close. Descendants can inherit pipes after the
//! child exits, so conflating these conditions can hang forever or discard trailing output.

#[cfg(feature = "mcp")]
use crate::{catalog, progress::McpProgressSink};
use crate::{
    output::{
        COMBINED_OUTPUT_BYTES, FALLBACK_PREVIEW_BYTES, OUTPUT_CHANNEL_CAPACITY, Tail, read_stream,
    },
    permission::{MAX_COMMAND_BYTES, ShellPermissionPolicy},
    process::{exit_signal, platform_command, terminate_and_reap, terminate_residual_group},
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
use workcell_output_filter::Rule as FilterRule;

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
        if let Err(error) = self.policy.authorize(prepared.command()) {
            return Some(Ok(tool_error(error)));
        }
        let progress = progress.map(|(peer, token)| {
            Arc::new(McpProgressSink { peer, token }) as Arc<dyn ShellProgressSink>
        });
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

    /// Validate and inspect a command without applying policy or starting a process.
    pub async fn prepare(&self, input: ShellInput) -> Result<PreparedShell, String> {
        if input.command.trim().is_empty() {
            return Err("Invalid arguments: command must not be empty".into());
        }
        if input.command.len() > MAX_COMMAND_BYTES {
            return Err(format!(
                "Invalid arguments for tool shell: command is {} UTF-8 bytes; maximum is {MAX_COMMAND_BYTES}. Split the operation into smaller shell calls",
                input.command.len()
            ));
        }
        let timeout_ms = input.timeout.unwrap_or(DEFAULT_TIMEOUT_MS);
        if timeout_ms == 0 || timeout_ms > MAX_TIMEOUT_MS {
            return Err(format!(
                "Invalid arguments: timeout must be between 1 and {MAX_TIMEOUT_MS}"
            ));
        }
        let requested_workdir = input.workdir.as_deref().unwrap_or(".");
        let (workdir, relative_workdir) = if self.confined {
            workdir::resolve(&self.root, requested_workdir).await?
        } else {
            workdir::resolve_unconfined(&self.root, requested_workdir).await?
        };
        let analysis = crate::permission::inspect(&input.command);
        Ok(PreparedShell::new(
            input.command,
            timeout_ms,
            workdir,
            relative_workdir,
            analysis,
        ))
    }

    /// Execute after a native host has authorized the prepared scopes.
    pub async fn execute_prepared(
        &self,
        prepared: PreparedShell,
        cancellation: CancellationToken,
        progress: Option<Arc<dyn ShellProgressSink>>,
    ) -> Result<Option<ShellExecution>, String> {
        let (command_text, timeout_ms, workdir, relative_workdir, analysis) =
            prepared.into_execution_parts();
        let mut progress = progress.map(ProgressPump::start);
        // Queue admission remains cancellable; holding the permit through final progress drain keeps
        // all per-execution resources inside the global concurrency budget.
        let _permit = tokio::select! {permit=concurrency().clone().acquire_owned()=>permit.map_err(|_|"Shell concurrency gate is unavailable".to_owned())?,()=cancellation.cancelled()=>return Ok(None)};
        let started = Instant::now();
        let mut command = platform_command(&command_text);
        command
            .current_dir(workdir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
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
                         ShellStream::Stdout=>stdout_tail.push(event.text.as_bytes()),
                         ShellStream::Stderr=>stderr_tail.push(event.text.as_bytes())
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
        };
        let (model_text, filter) =
            self.render_with_filter(&output, &analysis, &stdout_tail, &stderr_tail);
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
    #[cfg(test)]
    fn render(
        &self,
        output: &ShellOutput,
        analysis: &ShellCommandAnalysis,
        stdout_tail: &Tail,
        stderr_tail: &Tail,
    ) -> String {
        self.render_with_filter(output, analysis, stdout_tail, stderr_tail)
            .0
    }

    fn render_with_filter(
        &self,
        output: &ShellOutput,
        analysis: &ShellCommandAnalysis,
        stdout_tail: &Tail,
        stderr_tail: &Tail,
    ) -> (String, Option<ShellFilterInfo>) {
        let unfiltered = model_text(output);
        let Some(rule) = self.matching_rule(analysis) else {
            return (unfiltered, None);
        };
        let filtered = rule.apply(&stdout_tail.text(), &stderr_tail.text(), output.exit_code);
        if !filtered.lossy {
            // The rule matched but removed nothing. Announcing a filter that did
            // not filter would tell a reader to go looking for output that was
            // never dropped.
            return (unfiltered, None);
        }
        let mut rendered = filtered.text;
        if !filtered.consumed_stderr && !output.stderr.is_empty() {
            // A rule that describes stdout must not make a diagnostic written to
            // stderr disappear from the rendering.
            if !rendered.is_empty() {
                rendered.push('\n');
            }
            rendered.push_str("stderr tail:\n");
            rendered.push_str(&output.stderr);
        }
        if rendered.trim().is_empty() {
            return (unfiltered, None);
        }
        // The notice is deliberately terse. It is paid on every filtered result
        // and is compared against the complete capture below, so a verbose
        // notice would stop small reductions from ever being worth taking.
        let candidate = format!(
            "{}\n[filtered: {}]",
            bound_rendering(&rendered),
            rule.name()
        );
        // Filtering exists to reduce what a model reads, and the notice is not
        // free. A rendering that is not smaller than the complete capture is
        // strictly worse than it: it costs more and says less.
        if candidate.len() >= unfiltered.len() {
            return (unfiltered, None);
        }
        let filter = ShellFilterInfo {
            rule: rule.name().to_owned(),
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
    fn matching_rule(&self, analysis: &ShellCommandAnalysis) -> Option<&'static FilterRule> {
        if !self.output_filter || analysis.opaque {
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
        self.execute_prepared(self.prepare(input).await?, cancellation, progress)
            .await
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
    if output.stdout.is_empty() && output.stderr.is_empty() {
        format!(
            "Command exited with code {} and produced no output.",
            output
                .exit_code
                .map_or_else(|| "unknown".into(), |v| v.to_string())
        )
    } else {
        format!(
            "stdout tail:\n{}\nstderr tail:\n{}",
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
    use serde_json::json;

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
        }
    }

    fn tail(bytes: &str) -> Tail {
        let mut tail = Tail::default();
        tail.push(bytes.as_bytes());
        tail
    }

    const MAKE_OUTPUT: &str =
        "make[1]: Entering directory '/x'\ngcc -O2 foo.c\nmake[1]: Leaving directory '/x'\n";
    const CARGO_TEST_STDOUT: &str = "running 2 tests\ntest sdk_mode::tests::wire_init ... ok\ntest sdk_mode::tests::wire_result ... ok\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n";
    const CARGO_TEST_STDERR: &str = "warning: future incompatibility\n";

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
            &Tail::default(),
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
                timeout: None,
                workdir: None,
            })
            .await
            .unwrap();

        assert_eq!(prepared.workdir(), root.path().canonicalize().unwrap());
        assert_eq!(prepared.relative_workdir(), ".");
        assert_eq!(prepared.analysis().scopes.len(), 1);
        assert_eq!(prepared.analysis().scopes[0].permission, "printf *");
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
                timeout: None,
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
                timeout: None,
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
                timeout: None,
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
        assert_eq!(
            call(&group, json!({"command":"sleep 2","timeout":10}))
                .await
                .structured_content
                .unwrap()["timedOut"],
            true
        );
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
        assert_eq!(
            call(&group, json!({"command":command,"timeout":20}))
                .await
                .structured_content
                .unwrap()["timedOut"],
            true
        );
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
        assert_eq!(
            call(&group, json!({"command":command,"timeout":20}))
                .await
                .structured_content
                .unwrap()["timedOut"],
            true
        );
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(!sentinel.exists());
    }
}
