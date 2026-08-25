//! Shell tool orchestration.
//!
//! Execution is a small state machine with two independent completion conditions: the direct child
//! must have an exit status and both output pipes must close. Descendants can inherit pipes after the
//! child exits, so conflating these conditions can hang forever or discard trailing output.

use crate::{
    catalog,
    output::{
        COMBINED_OUTPUT_BYTES, FALLBACK_PREVIEW_BYTES, OUTPUT_CHANNEL_CAPACITY, Tail, read_stream,
    },
    permission::{MAX_COMMAND_BYTES, ShellPermissionPolicy},
    process::{exit_signal, platform_command, terminate_and_reap, terminate_residual_group},
    progress::{PeerProgressTransport, ProgressPump, receive_failure},
    types::{DEFAULT_TIMEOUT_MS, MAX_TIMEOUT_MS, ShellInput, ShellOutput, Stream},
    workdir,
};
use rmcp::{
    RoleServer,
    model::{CallToolResult, ContentBlock, ProgressToken, Tool},
    service::Peer,
};
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
}
#[derive(Debug)]
pub struct ShellBuildError;
impl fmt::Display for ShellBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("shell root must be an existing directory")
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
        let root = workdir::canonicalize(root.as_ref())
            .await
            .map_err(|_| ShellBuildError)?;
        if !tokio::fs::metadata(&root)
            .await
            .map_err(|_| ShellBuildError)?
            .is_dir()
        {
            return Err(ShellBuildError);
        }
        Ok(Self { root, policy })
    }
    #[must_use]
    pub fn catalog(&self) -> Vec<Tool> {
        catalog::catalog()
    }
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
        let progress = progress.map(|(peer, token)| {
            ProgressPump::start(token, Arc::new(PeerProgressTransport { peer }))
        });
        Some(Ok(
            match self.execute(input, cancellation, progress).await {
                Ok(Some(output)) => result_content(output),
                Ok(None) => tool_error("Shell execution cancelled"),
                Err(e) => tool_error(e),
            },
        ))
    }
    async fn execute(
        &self,
        input: ShellInput,
        cancellation: CancellationToken,
        mut progress: Option<ProgressPump>,
    ) -> Result<Option<ShellOutput>, String> {
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
        let (workdir, relative_workdir) =
            workdir::resolve(&self.root, input.workdir.as_deref().unwrap_or(".")).await?;
        self.policy.authorize(&input.command)?;
        // Queue admission remains cancellable; holding the permit through final progress drain keeps
        // all per-execution resources inside the global concurrency budget.
        let _permit = tokio::select! {permit=concurrency().clone().acquire_owned()=>permit.map_err(|_|"Shell concurrency gate is unavailable".to_owned())?,()=cancellation.cancelled()=>return Ok(None)};
        let started = Instant::now();
        let mut command = platform_command(&input.command);
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
        let stdout_task = tokio::spawn(read_stream(stdout, Stream::Stdout, sender.clone()));
        let stderr_task = tokio::spawn(read_stream(stderr, Stream::Stderr, sender));
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
                          Stream::Stdout=>stdout_utf8_bytes=stdout_utf8_bytes.saturating_add(utf8_bytes),
                          Stream::Stderr=>stderr_utf8_bytes=stderr_utf8_bytes.saturating_add(utf8_bytes)
                      }
                      if let Some(p)=&progress&&let Err(e)=p.enqueue(sequence,event.stream,&event.text) {
                         terminate_and_reap(&mut child,pid).await?;
                         stdout_task.abort(); stderr_task.abort(); p.task.abort(); return Err(e);
                     }
                     match event.stream {
                         Stream::Stdout=>stdout_tail.push(event.text.as_bytes()),
                         Stream::Stderr=>stderr_tail.push(event.text.as_bytes())
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
        Ok(Some(ShellOutput {
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
        }))
    }
}
async fn wait_for_pipe_deadline(deadline: Option<tokio::time::Instant>) {
    if let Some(deadline) = deadline {
        tokio::time::sleep_until(deadline).await
    } else {
        std::future::pending().await
    }
}
fn result_content(output: ShellOutput) -> CallToolResult {
    let text = if output.stdout.is_empty() && output.stderr.is_empty() {
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
    };
    let structured = serde_json::to_value(&output).expect("shell output serializes");
    let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
    result.structured_content = Some(structured);
    result
}
fn tool_error(error: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(error.into())])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
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
