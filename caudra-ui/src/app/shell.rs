use caudra_agent::tools::ToolEffect;
use std::collections::HashSet;
use std::process::Command as StdCommand;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use async_process::{Command, Stdio};
use caudra_agent::{
    AgentConfig, CancelToken, CancelTrigger, ToolAccounting, ToolDoneEvent, ToolInput, ToolOutput,
    ToolStartEvent, WorkspaceBaseline,
};
use caudra_providers::Message;
use caudra_workspace::{
    CommandText, DirectoryNavigation, ExecRequest, OperationProgressKind, OperationState,
    OperationStatus, WorkspaceCursor, WorkspaceSession,
};
use futures_lite::{future, io::AsyncReadExt};
use serde::Deserialize;

use super::App;
use crate::components::{DisplayMessage, DisplayRole};

const STREAM_FLUSH_INTERVAL: Duration = Duration::from_millis(100);
const SHELL_TIMEOUT: Duration = Duration::from_secs(300);
const READ_CHUNK_BYTES: usize = 8 * 1024;
const READ_CHANNEL_CAPACITY: usize = 8;
const TRUNCATED_MARKER: &str = "[truncated]";
const PREAMBLE_TRUNCATED_MARKER: &str = "[shell result truncated]";
const REMOTE_POLL_INTERVAL: Duration = Duration::from_millis(50);
const REMOTE_POLL_MAX: Duration = Duration::from_secs(2);
const REMOTE_RECONCILE_TIMEOUT: Duration = Duration::from_secs(10);
const REMOTE_RECONCILE_MAX_POLLS: usize = 4;
const REMOTE_PROGRESS_GAP: &str = "[remote progress gap]";
const REMOTE_CANCELLED: &str = "Remote command cancelled";
const REMOTE_FAILED: &str = "Remote command failed";
const REMOTE_INDETERMINATE: &str =
    "Remote command outcome is indeterminate; it will not be retried";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShellPrefix {
    pub prefix_len: usize,
    pub command: String,
    pub visible: bool,
}

pub(crate) fn parse_shell_prefix(text: &str) -> Option<ShellPrefix> {
    let (sigil_len, visible) = if text.starts_with("!!") {
        (2, false)
    } else if text.starts_with('!') {
        (1, true)
    } else {
        return None;
    };
    let rest = &text[sigil_len..];
    let prefix_len = if rest.starts_with(' ') {
        sigil_len + 1
    } else {
        sigil_len
    };
    let command = rest.trim();
    if command.is_empty() {
        return None;
    }
    Some(ShellPrefix {
        prefix_len,
        command: command.to_owned(),
        visible,
    })
}

pub(crate) enum ShellEvent {
    Start {
        id: String,
        command: String,
    },
    Output {
        id: String,
        content: String,
    },
    Done {
        id: String,
        command: String,
        output: String,
        is_error: bool,
        visible: bool,
        max_output_lines: usize,
        max_output_bytes: usize,
    },
    RemoteDirectoryResolved {
        previous: WorkspaceCursor,
        result: Box<Result<RemoteDirectoryChange, String>>,
    },
    RemoteControlFinished(Result<String, String>),
}

pub(crate) struct RemoteDirectoryChange {
    pub workspace: WorkspaceSession,
    pub binding: caudra_storage::workspace_binding::StoredWorkspaceBinding,
    pub context: Arc<caudra_agent::remote_project_context::RemoteProjectContext>,
    pub display_path: String,
}

pub(crate) struct RemoteShellTarget {
    pub workspace: WorkspaceSession,
    pub baseline: Arc<WorkspaceBaseline>,
}

#[derive(Default)]
pub(crate) struct ShellState {
    cancel_triggers: Vec<CancelTrigger>,
    pending_results: Vec<Message>,
    id_counter: u64,
    active_ids: HashSet<String>,
}

impl ShellState {
    pub fn reserve_id(&mut self) -> String {
        self.id_counter += 1;
        let id = format!("shell-{}", self.id_counter);
        self.active_ids.insert(id.clone());
        id
    }

    pub fn active_ids(&self) -> &HashSet<String> {
        &self.active_ids
    }

    pub fn release_id(&mut self, id: &str) {
        self.active_ids.remove(id);
    }

    pub fn add_trigger(&mut self, trigger: CancelTrigger) {
        self.cancel_triggers.push(trigger);
    }

    pub fn cancel_all(&mut self) {
        for trigger in self.cancel_triggers.drain(..) {
            trigger.cancel();
        }
    }

    pub fn push_result(&mut self, msg: Message) {
        self.pending_results.push(msg);
    }

    pub fn drain_results(&mut self) -> Vec<Message> {
        std::mem::take(&mut self.pending_results)
    }
}

impl App {
    pub(crate) fn handle_shell_event(&mut self, event: ShellEvent) {
        match event {
            ShellEvent::Start { id, command } => {
                self.main_chat().shell_tool_start(ToolStartEvent {
                    id,
                    effect: ToolEffect::Mutating,
                    tool: "bash".into(),
                    summary: command.clone(),
                    annotation: None,
                    input: Some(ToolInput::Code {
                        language: "bash".into(),
                        code: command,
                    }),
                    raw_input: None,
                    output: None,
                    render_header: None,
                });
            }
            ShellEvent::Output { id, content } => {
                self.main_chat().shell_tool_output(&id, &content);
            }
            ShellEvent::Done {
                id,
                command,
                output,
                is_error,
                visible,
                max_output_lines,
                max_output_bytes,
            } => {
                let result_msg = if visible {
                    let label = if is_error { "Error" } else { "Output" };
                    Some(Message::user(bounded_shell_result(
                        &command,
                        label,
                        &output,
                        max_output_lines,
                        max_output_bytes,
                    )))
                } else {
                    None
                };
                self.main_chat().shell_tool_done(ToolDoneEvent {
                    id: id.clone(),
                    tool: "bash".into(),
                    output: ToolOutput::Plain(output.into()),
                    is_error,
                    annotation: None,
                    written_path: None,
                    written_paths: Vec::new(),
                    remote_written_paths: false,
                    output_ref: None,
                    output_limits: None,
                    model_suffix: None,
                    model_output: None,
                    model_output_from_ref: false,
                    accounting: ToolAccounting::default(),
                });
                if let Some(msg) = result_msg {
                    self.shell.push_result(msg);
                }
                self.shell.release_id(&id);
            }
            ShellEvent::RemoteDirectoryResolved { .. } => {}
            ShellEvent::RemoteControlFinished(result) => self.main_chat().push(
                DisplayMessage::new(DisplayRole::Notice, result.unwrap_or_else(|error| error)),
            ),
        }
    }
}

pub(crate) fn spawn_remote_cd(
    workspace: WorkspaceSession,
    stored_binding: caudra_storage::workspace_binding::StoredWorkspaceBinding,
    path: DirectoryNavigation,
    tx: flume::Sender<ShellEvent>,
) {
    smol::spawn(async move {
        let previous = workspace.cursor().clone();
        let result = async {
            let service = workspace
                .workspace()
                .services()
                .read
                .as_ref()
                .ok_or_else(|| "cd: remote directory resolution is unavailable".to_owned())?;
            let resolved = service
                .navigate_directory(workspace.binding(), workspace.cursor(), &path)
                .await
                .map_err(|_| "cd: remote directory could not be resolved".to_owned())?;
            let display_path = resolved
                .resource
                .path
                .as_ref()
                .map(ToString::to_string)
                .ok_or_else(|| "cd: remote directory response is invalid".to_owned())?;
            let workspace = workspace
                .with_cursor(resolved)
                .map_err(|_| "cd: remote directory response is invalid or stale".to_owned())?;
            let binding = stored_binding
                .with_cursor(workspace.cursor().clone())
                .map_err(|_| "cd: remote workspace identity changed".to_owned())?;
            let context =
                caudra_agent::remote_project_context::load_remote_project_context(&workspace)
                    .await
                    .map_err(|_| "cd: remote project context could not be refreshed".to_owned())?;
            Ok(RemoteDirectoryChange {
                workspace,
                binding,
                context,
                display_path,
            })
        }
        .await;
        let _ = tx.send(ShellEvent::RemoteDirectoryResolved {
            previous,
            result: Box::new(result),
        });
    })
    .detach();
}

pub(crate) fn spawn_remote_control(
    workspace: Option<WorkspaceSession>,
    args: String,
    tx: flume::Sender<ShellEvent>,
) {
    smol::spawn(async move {
        let result = match workspace {
            Some(workspace) => caudra_workspace::execute_workspace_control(&workspace, &args).await,
            None => Err("Remote workspace control is unavailable".to_owned()),
        };
        let _ = tx.send(ShellEvent::RemoteControlFinished(result));
    })
    .detach();
}

pub(crate) fn spawn_shell(
    command: String,
    id: String,
    visible: bool,
    tx: flume::Sender<ShellEvent>,
    cancel: CancelToken,
    config: AgentConfig,
) {
    smol::spawn(async move {
        let _ = tx.send(ShellEvent::Start {
            id: id.clone(),
            command: command.clone(),
        });

        let result = run_command(
            &command,
            &id,
            &tx,
            &cancel,
            config.max_output_lines,
            config.max_output_bytes,
        )
        .await;

        let (output, is_error) = match result {
            Ok(out) => (out, false),
            Err(err) => (err, true),
        };

        let _ = tx.send(ShellEvent::Done {
            id,
            command,
            output,
            is_error,
            visible,
            max_output_lines: config.max_output_lines,
            max_output_bytes: config.max_output_bytes,
        });
    })
    .detach();
}

pub(crate) fn spawn_remote_shell(
    target: RemoteShellTarget,
    command: String,
    id: String,
    visible: bool,
    tx: flume::Sender<ShellEvent>,
    cancel: CancelToken,
    config: AgentConfig,
) {
    smol::spawn(async move {
        let _ = tx.send(ShellEvent::Start {
            id: id.clone(),
            command: command.clone(),
        });
        if let Err(error) = target.baseline.ensure_current().await.into_result() {
            let _ = tx.send(ShellEvent::Done {
                id,
                command,
                output: error.to_string(),
                is_error: true,
                visible,
                max_output_lines: config.max_output_lines,
                max_output_bytes: config.max_output_bytes,
            });
            return;
        }
        let result = run_remote_command(
            &target.workspace,
            &command,
            &id,
            &tx,
            &cancel,
            config.max_output_lines,
            config.max_output_bytes,
        )
        .await;
        let (output, is_error) = match result {
            Ok(output) => (output, false),
            Err(output) => (output, true),
        };
        let _ = tx.send(ShellEvent::Done {
            id,
            command,
            output,
            is_error,
            visible,
            max_output_lines: config.max_output_lines,
            max_output_bytes: config.max_output_bytes,
        });
    })
    .detach();
}

async fn run_remote_command(
    workspace: &WorkspaceSession,
    command: &str,
    id: &str,
    tx: &flume::Sender<ShellEvent>,
    cancel: &CancelToken,
    max_output_lines: usize,
    max_output_bytes: usize,
) -> Result<String, String> {
    if cancel.is_cancelled() {
        return Err(REMOTE_CANCELLED.into());
    }
    let service = workspace
        .workspace()
        .services()
        .exec
        .as_ref()
        .ok_or_else(|| "Remote command execution is unavailable".to_owned())?;
    let command = CommandText::new(command).map_err(|error| error.to_string())?;
    let request = ExecRequest {
        command,
        timeout_ms: Some(SHELL_TIMEOUT.as_millis() as u64),
    };
    let deadline = Instant::now() + SHELL_TIMEOUT;
    let mut status = cancel
        .race(future::race(
            async {
                service
                    .execute(workspace.binding(), workspace.cursor(), &request)
                    .await
                    .map_err(remote_exec_error)
            },
            async {
                smol::Timer::at(deadline).await;
                Err(REMOTE_INDETERMINATE.into())
            },
        ))
        .await
        .map_err(|_| REMOTE_INDETERMINATE.to_owned())??;
    let mut output = BoundedText::new(max_output_lines, max_output_bytes);
    let mut next_sequence = None;
    let mut reported_gap = false;
    let mut cancelling = cancel.is_cancelled();
    let mut delay = REMOTE_POLL_INTERVAL;

    loop {
        append_remote_progress(&status, &mut output, &mut next_sequence, &mut reported_gap);
        if !output.as_str().is_empty() {
            flush_output(tx, id, output.as_str());
        }
        match &status.state {
            OperationState::Completed { result, .. } => {
                let terminal = remote_terminal_output(result, &output)?;
                flush_output(tx, id, &terminal);
                return Ok(terminal);
            }
            OperationState::Failed { .. } => {
                return Err(output.with_marker(REMOTE_FAILED));
            }
            OperationState::Cancelled {
                side_effects_possible,
            } => {
                let message = if *side_effects_possible {
                    REMOTE_INDETERMINATE
                } else {
                    REMOTE_CANCELLED
                };
                return Err(output.with_marker(message));
            }
            OperationState::Indeterminate { .. }
            | OperationState::Forgotten
            | OperationState::NeverSeen => {
                return Err(output.with_marker(REMOTE_INDETERMINATE));
            }
            OperationState::Prepared => {
                return Err(output.with_marker("Remote command did not start"));
            }
            OperationState::Running => {}
        }

        if cancelling || Instant::now() >= deadline {
            let reconciled = future::race(
                async {
                    let _ = service
                        .cancel(workspace.binding(), workspace.cursor(), &status.handle)
                        .await;
                    let mut delay = REMOTE_POLL_INTERVAL;
                    for _ in 0..REMOTE_RECONCILE_MAX_POLLS {
                        smol::Timer::after(delay).await;
                        if let Ok(status) = service
                            .status(workspace.binding(), workspace.cursor(), &status.handle)
                            .await
                        {
                            append_remote_progress(
                                &status,
                                &mut output,
                                &mut next_sequence,
                                &mut reported_gap,
                            );
                            flush_output(tx, id, output.as_str());
                            if !matches!(
                                status.state,
                                OperationState::Running | OperationState::Prepared
                            ) {
                                return Some(status);
                            }
                        }
                        delay = (delay * 2).min(REMOTE_POLL_MAX);
                    }
                    None
                },
                async {
                    smol::Timer::after(REMOTE_RECONCILE_TIMEOUT).await;
                    None
                },
            )
            .await;
            let Some(reconciled) = reconciled else {
                return Err(output.with_marker(REMOTE_INDETERMINATE));
            };
            status = reconciled;
            continue;
        }
        let polled = cancel
            .race(future::race(
                async {
                    smol::Timer::after(delay).await;
                    service
                        .status(workspace.binding(), workspace.cursor(), &status.handle)
                        .await
                        .ok()
                },
                async {
                    smol::Timer::at(deadline).await;
                    None
                },
            ))
            .await;
        delay = (delay * 2).min(REMOTE_POLL_MAX);
        match polled {
            Ok(Some(result)) => status = result,
            Ok(None) | Err(_) => cancelling = true,
        }
    }
}

fn append_remote_progress(
    status: &OperationStatus<serde_json::Value>,
    output: &mut BoundedText,
    next_sequence: &mut Option<u64>,
    reported_gap: &mut bool,
) {
    let first = status.progress_metadata.first_retained_sequence;
    let gap = status.progress_metadata.gap_before_first
        || next_sequence
            .zip(first)
            .is_some_and(|(expected, actual)| actual > expected);
    if gap && !*reported_gap {
        output.push(REMOTE_PROGRESS_GAP);
        output.push("\n");
        *reported_gap = true;
    }
    if next_sequence.is_none() {
        *next_sequence = first;
    }
    for progress in &status.progress {
        if next_sequence.is_some_and(|expected| progress.sequence < expected) {
            continue;
        }
        if next_sequence.is_some_and(|expected| progress.sequence > expected) && !*reported_gap {
            output.push(REMOTE_PROGRESS_GAP);
            output.push("\n");
            *reported_gap = true;
        }
        *next_sequence = Some(progress.sequence.saturating_add(1));
        match &progress.kind {
            OperationProgressKind::Stdout | OperationProgressKind::Stderr => {
                output.push(&progress.chunk)
            }
            OperationProgressKind::Started
            | OperationProgressKind::Exited
            | OperationProgressKind::Unknown(_)
                if !progress.chunk.is_empty() =>
            {
                output.push("[");
                output.push(&progress.chunk);
                output.push("]\n");
            }
            OperationProgressKind::Started
            | OperationProgressKind::Exited
            | OperationProgressKind::Unknown(_) => {}
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteExecOutput {
    exit_code: Option<i32>,
    signal: Option<i32>,
    timed_out: bool,
    output_limit_exceeded: bool,
    stdout: String,
    stderr: String,
}

fn remote_terminal_output(
    value: &serde_json::Value,
    progress: &BoundedText,
) -> Result<String, String> {
    let result: RemoteExecOutput = serde_json::from_value(value.clone())
        .map_err(|_| "Remote command returned an invalid result".to_owned())?;
    let mut output = BoundedText::new(progress.max_lines, progress.max_bytes);
    if progress.as_str().is_empty() {
        output.push(&result.stdout);
        output.push(&result.stderr);
    } else {
        output.push(progress.as_str());
    }
    let mut statuses = Vec::new();
    if result.timed_out {
        statuses.push("timed out".to_owned());
    }
    if result.output_limit_exceeded {
        statuses.push("output limit exceeded".to_owned());
    }
    if let Some(code) = result.exit_code {
        statuses.push(format!("exit code {code}"));
    } else if let Some(signal) = result.signal {
        statuses.push(format!("signal {signal}"));
    } else if statuses.is_empty() {
        statuses.push("exit unknown".to_owned());
    }
    output.push("\n[shell status: ");
    output.push(&statuses.join("; "));
    output.push("]");
    let output = output.finish();
    if result.exit_code == Some(0) && !result.timed_out && !result.output_limit_exceeded {
        Ok(output)
    } else {
        Err(output)
    }
}

fn remote_exec_error(error: caudra_workspace::WorkspaceError) -> String {
    match error {
        caudra_workspace::WorkspaceError::PolicyDenied
        | caudra_workspace::WorkspaceError::PermissionDenied => {
            "Remote command was denied by policy".into()
        }
        caudra_workspace::WorkspaceError::Cancelled => REMOTE_CANCELLED.into(),
        caudra_workspace::WorkspaceError::IndeterminateOutcome => REMOTE_INDETERMINATE.into(),
        _ => "Remote command execution failed".into(),
    }
}

async fn run_command(
    command: &str,
    id: &str,
    tx: &flume::Sender<ShellEvent>,
    cancel: &CancelToken,
    max_output_lines: usize,
    max_output_bytes: usize,
) -> Result<String, String> {
    let mut std_cmd = StdCommand::new("bash");
    std_cmd
        .arg("-c")
        .arg(command)
        .env("GIT_TERMINAL_PROMPT", "0");

    #[cfg(unix)]
    unsafe {
        std_cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    let mut cmd: Command = std_cmd.into();
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| format!("failed to spawn: {e}"))?;

    let (read_tx, read_rx) = flume::bounded::<ReadEvent>(READ_CHANNEL_CAPACITY);
    if let Some(stdout) = child.stdout.take() {
        spawn_reader(stdout, ShellStream::Stdout, read_tx.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_reader(stderr, ShellStream::Stderr, read_tx.clone());
    }
    let mut guard = caudra_agent::ChildGuard::new(child);
    drop(read_tx);

    let mut output = BoundedText::new(max_output_lines, max_output_bytes);
    let mut stdout_decoder = Utf8Decoder::default();
    let mut stderr_decoder = Utf8Decoder::default();
    let mut last_flush = Instant::now();
    let deadline = Instant::now() + SHELL_TIMEOUT;

    macro_rules! race_deadline {
        ($future:expr) => {
            futures_lite::future::race(
                $future,
                futures_lite::future::race(
                    async {
                        smol::Timer::at(deadline).await;
                        Err(format!("timed out after {}s", SHELL_TIMEOUT.as_secs()))
                    },
                    async {
                        cancel.cancelled().await;
                        Err("cancelled".to_string())
                    },
                ),
            )
            .await
        };
    }

    loop {
        let event = race_deadline!(async { Ok(read_rx.recv_async().await.ok()) });
        match event {
            Ok(Some(ReadEvent::Bytes { stream, bytes })) => {
                let decoder = match stream {
                    ShellStream::Stdout => &mut stdout_decoder,
                    ShellStream::Stderr => &mut stderr_decoder,
                };
                if let Err(error) = decoder.push(&bytes, &mut output) {
                    guard.kill_and_reap().await;
                    return Err(output
                        .with_marker(&format!("[{stream} output is not valid UTF-8: {error}]")));
                }
            }
            Ok(Some(ReadEvent::End(stream))) => {
                let decoder = match stream {
                    ShellStream::Stdout => &mut stdout_decoder,
                    ShellStream::Stderr => &mut stderr_decoder,
                };
                if let Err(error) = decoder.finish() {
                    guard.kill_and_reap().await;
                    return Err(output
                        .with_marker(&format!("[{stream} output is not valid UTF-8: {error}]")));
                }
            }
            Ok(Some(ReadEvent::Error { stream, error })) => {
                guard.kill_and_reap().await;
                return Err(output.with_marker(&format!("[{stream} read error: {error}]")));
            }
            Ok(None) => break,
            Err(e) => {
                guard.kill_and_reap().await;
                return Err(e);
            }
        }

        if last_flush.elapsed() >= STREAM_FLUSH_INTERVAL && !output.as_str().is_empty() {
            flush_output(tx, id, output.as_str());
            last_flush = Instant::now();
        }
    }

    let status =
        race_deadline!(async { guard.status().await.map_err(|e| format!("wait error: {e}")) });
    match status {
        Ok(status) => {
            let output = output.finish();
            flush_output(tx, id, &output);
            if !status.success() {
                if output.is_empty() {
                    return Err(bound_text(
                        &format!("exited with code {}", status.code().unwrap_or(-1)),
                        max_output_lines,
                        max_output_bytes,
                    ));
                }
                return Err(output);
            }
            Ok(output)
        }
        Err(e) => {
            guard.kill_and_reap().await;
            Err(e)
        }
    }
}

fn flush_output(tx: &flume::Sender<ShellEvent>, id: &str, output: &str) {
    let _ = tx.send(ShellEvent::Output {
        id: id.to_string(),
        content: output.to_string(),
    });
}

#[derive(Clone, Copy)]
enum ShellStream {
    Stdout,
    Stderr,
}

impl std::fmt::Display for ShellStream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stdout => formatter.write_str("stdout"),
            Self::Stderr => formatter.write_str("stderr"),
        }
    }
}

enum ReadEvent {
    Bytes {
        stream: ShellStream,
        bytes: Vec<u8>,
    },
    End(ShellStream),
    Error {
        stream: ShellStream,
        error: std::io::Error,
    },
}

fn spawn_reader<R: futures_lite::io::AsyncRead + Unpin + Send + 'static>(
    mut reader: R,
    stream: ShellStream,
    tx: flume::Sender<ReadEvent>,
) {
    smol::spawn(async move {
        loop {
            let mut buffer = [0; READ_CHUNK_BYTES];
            match reader.read(&mut buffer).await {
                Ok(0) => {
                    let _ = tx.send_async(ReadEvent::End(stream)).await;
                    break;
                }
                Ok(read) => {
                    if tx
                        .send_async(ReadEvent::Bytes {
                            stream,
                            bytes: buffer[..read].to_vec(),
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(error) => {
                    let _ = tx.send_async(ReadEvent::Error { stream, error }).await;
                    break;
                }
            }
        }
    })
    .detach();
}

#[derive(Default)]
struct Utf8Decoder {
    pending: Vec<u8>,
}

impl Utf8Decoder {
    fn push(&mut self, bytes: &[u8], output: &mut BoundedText) -> Result<(), std::str::Utf8Error> {
        if self.pending.is_empty() {
            return self.decode(bytes, output);
        }

        self.pending.extend_from_slice(bytes);
        let pending = std::mem::take(&mut self.pending);
        self.decode(&pending, output)
    }

    fn decode(
        &mut self,
        bytes: &[u8],
        output: &mut BoundedText,
    ) -> Result<(), std::str::Utf8Error> {
        match std::str::from_utf8(bytes) {
            Ok(text) => {
                output.push(text);
                Ok(())
            }
            Err(error) => {
                let valid_up_to = error.valid_up_to();
                if valid_up_to > 0 {
                    let valid = std::str::from_utf8(&bytes[..valid_up_to])
                        .expect("valid_up_to must identify valid UTF-8");
                    output.push(valid);
                }
                if error.error_len().is_some() {
                    return Err(error);
                }
                self.pending.extend_from_slice(&bytes[valid_up_to..]);
                Ok(())
            }
        }
    }

    fn finish(&self) -> Result<(), std::str::Utf8Error> {
        std::str::from_utf8(&self.pending).map(|_| ())
    }
}

struct BoundedText {
    text: String,
    max_lines: usize,
    max_bytes: usize,
    lines: usize,
    ends_with_newline: bool,
    truncated: bool,
}

impl BoundedText {
    fn new(max_lines: usize, max_bytes: usize) -> Self {
        Self {
            text: String::new(),
            max_lines,
            max_bytes,
            lines: 0,
            ends_with_newline: false,
            truncated: false,
        }
    }

    fn push(&mut self, text: &str) {
        if self.truncated {
            return;
        }

        for character in text.chars() {
            let next_bytes = self.text.len().saturating_add(character.len_utf8());
            let starts_line = self.text.is_empty() || self.ends_with_newline;
            let next_lines = self.lines + usize::from(starts_line);
            if next_bytes > self.max_bytes || next_lines > self.max_lines {
                self.truncated = true;
                return;
            }
            self.text.push(character);
            if starts_line {
                self.lines = next_lines;
            }
            self.ends_with_newline = character == '\n';
        }
    }

    fn as_str(&self) -> &str {
        &self.text
    }

    fn finish(self) -> String {
        if self.truncated {
            self.with_marker(TRUNCATED_MARKER)
        } else {
            self.text
        }
    }

    fn with_marker(mut self, marker: &str) -> String {
        append_bounded_marker(&mut self.text, marker, self.max_lines, self.max_bytes);
        self.text
    }
}

fn bounded_shell_result(
    command: &str,
    label: &str,
    output: &str,
    max_lines: usize,
    max_bytes: usize,
) -> String {
    let result = format!("I ran: $ {command}\n\n{label}:\n{output}");
    if fits(&result, max_lines, max_bytes) {
        result
    } else {
        let mut result = bound_text(&result, max_lines, max_bytes);
        append_bounded_marker(&mut result, PREAMBLE_TRUNCATED_MARKER, max_lines, max_bytes);
        result
    }
}

fn append_bounded_marker(text: &mut String, marker: &str, max_lines: usize, max_bytes: usize) {
    let marker = bound_text(marker, max_lines, max_bytes);
    if marker.is_empty() {
        text.clear();
        return;
    }

    let marker_lines = logical_line_count(&marker);
    let payload_lines = max_lines.saturating_sub(marker_lines);
    let payload_bytes = max_bytes.saturating_sub(marker.len().saturating_add(1));
    *text = bound_text(text, payload_lines, payload_bytes);
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&marker);
}

fn bound_text(text: &str, max_lines: usize, max_bytes: usize) -> String {
    let mut bounded = BoundedText::new(max_lines, max_bytes);
    bounded.push(text);
    bounded.text
}

fn fits(text: &str, max_lines: usize, max_bytes: usize) -> bool {
    text.len() <= max_bytes && logical_line_count(text) <= max_lines
}

fn logical_line_count(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    text.bytes().filter(|byte| *byte == b'\n').count() + usize::from(!text.ends_with('\n'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use caudra_workspace::{
        AuthenticatedPrincipalId, AuthorityIdentity, CancellationResult, CwdHandle,
        OperationHandle, OperationId, OperationPhase, OperationProgress, ProjectIdentity,
        ProjectKey, ResourceId, ResourceScope, SequenceMetadata, SessionBindingId,
        SessionWorkspaceBinding, SourceTrustAnchor, WorkspaceCapabilities, WorkspaceCapability,
        WorkspaceError, WorkspaceHandle, WorkspaceServices,
    };
    use serde_json::json;
    use test_case::test_case;

    #[test_case("! ls",                     Some(ShellPrefix { prefix_len: 2, command: "ls".into(), visible: true })           ; "simple_visible")]
    #[test_case("!! ls",                    Some(ShellPrefix { prefix_len: 3, command: "ls".into(), visible: false })          ; "simple_anonymous")]
    #[test_case("! cargo test --release",   Some(ShellPrefix { prefix_len: 2, command: "cargo test --release".into(), visible: true })  ; "multi_word_command")]
    #[test_case("!! cargo build",           Some(ShellPrefix { prefix_len: 3, command: "cargo build".into(), visible: false }) ; "multi_word_anonymous")]
    #[test_case("! ",                       None                        ; "bang_space_only")]
    #[test_case("!",                        None                        ; "bang_alone")]
    #[test_case("!!",                       None                        ; "double_bang_alone")]
    #[test_case("!! ",                      None                        ; "double_bang_space_only")]
    #[test_case("hello ! world",            None                        ; "bang_mid_string")]
    #[test_case(" ! ls",                    None                        ; "leading_space")]
    #[test_case("!echo hi",                 Some(ShellPrefix { prefix_len: 1, command: "echo hi".into(), visible: true })      ; "no_space_after_bang")]
    #[test_case("!!echo hi",                Some(ShellPrefix { prefix_len: 2, command: "echo hi".into(), visible: false })     ; "no_space_after_double_bang")]
    #[test_case("!  ls",                    Some(ShellPrefix { prefix_len: 2, command: "ls".into(), visible: true })           ; "extra_spaces_trimmed")]
    fn parse_shell_prefix_cases(input: &str, expected: Option<ShellPrefix>) {
        assert_eq!(parse_shell_prefix(input), expected);
    }

    fn remote_status(
        progress: Vec<OperationProgress>,
        gap: bool,
    ) -> OperationStatus<serde_json::Value> {
        OperationStatus {
            handle: OperationHandle {
                preparation_id: OperationId::new("prepared").unwrap(),
                invocation_id: Some(OperationId::new("invocation").unwrap()),
                execution_id: Some(OperationId::new("execution").unwrap()),
                expires_at_unix_ms: None,
            },
            state: OperationState::Running,
            progress_metadata: SequenceMetadata {
                first_retained_sequence: progress.first().map(|item| item.sequence),
                next_sequence: progress.last().map_or(0, |item| item.sequence + 1),
                gap_before_first: gap,
            },
            progress,
        }
    }

    fn progress(sequence: u64, kind: OperationProgressKind, chunk: &str) -> OperationProgress {
        OperationProgress {
            execution_id: OperationId::new("execution").unwrap(),
            sequence,
            kind,
            chunk: chunk.into(),
        }
    }

    struct ExecService {
        execute: Mutex<Result<OperationStatus<serde_json::Value>, WorkspaceError>>,
        statuses: Mutex<VecDeque<OperationStatus<serde_json::Value>>>,
        commands: Mutex<Vec<String>>,
        cwd_handles: Mutex<Vec<String>>,
        execute_calls: AtomicUsize,
        cancel_calls: AtomicUsize,
        cancel_on_status: Mutex<Option<CancelTrigger>>,
        status_calls: AtomicUsize,
    }

    #[async_trait]
    impl caudra_workspace::WorkspaceExecService for ExecService {
        async fn execute(
            &self,
            _binding: &SessionWorkspaceBinding,
            cursor: &WorkspaceCursor,
            request: &ExecRequest,
        ) -> Result<OperationStatus<serde_json::Value>, WorkspaceError> {
            self.execute_calls.fetch_add(1, Ordering::SeqCst);
            self.commands
                .lock()
                .unwrap()
                .push(request.command.as_str().to_owned());
            self.cwd_handles
                .lock()
                .unwrap()
                .push(cursor.cwd_handle().as_str().to_owned());
            self.execute.lock().unwrap().clone()
        }

        async fn status(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            _operation: &OperationHandle,
        ) -> Result<OperationStatus<serde_json::Value>, WorkspaceError> {
            self.status_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(trigger) = self.cancel_on_status.lock().unwrap().take() {
                trigger.cancel();
            }
            self.statuses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or(WorkspaceError::Unavailable)
        }

        async fn cancel(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            _operation: &OperationHandle,
        ) -> Result<CancellationResult, WorkspaceError> {
            self.cancel_calls.fetch_add(1, Ordering::SeqCst);
            Ok(CancellationResult {
                state: OperationPhase::Running,
                cancellation_requested: true,
            })
        }
    }

    fn operation_status(
        state: OperationState<serde_json::Value>,
    ) -> OperationStatus<serde_json::Value> {
        OperationStatus {
            state,
            ..remote_status(Vec::new(), false)
        }
    }

    fn workspace(service: Arc<ExecService>) -> WorkspaceSession {
        let authority = AuthorityIdentity::new(
            SourceTrustAnchor::new("test-source").unwrap(),
            "authority",
            "workspace",
            "generation",
            "namespace",
        )
        .unwrap();
        let binding = SessionWorkspaceBinding::new(
            SessionBindingId::new("tab").unwrap(),
            authority.clone(),
            AuthenticatedPrincipalId::new(authority.clone(), "principal").unwrap(),
            ProjectIdentity::new(authority.clone(), ProjectKey::new("project").unwrap()),
        )
        .unwrap();
        let cursor = WorkspaceCursor::new(
            &binding,
            ResourceScope::root(ResourceId::new("root").unwrap()),
            1,
            CwdHandle::new("remote-cwd").unwrap(),
        );
        let services = WorkspaceServices {
            exec: Some(service),
            ..Default::default()
        };
        let handle = WorkspaceHandle::new(
            authority,
            WorkspaceCapabilities::from([
                WorkspaceCapability::ExecExecute,
                WorkspaceCapability::ExecStatus,
                WorkspaceCapability::ExecCancel,
            ]),
            services,
        )
        .unwrap();
        WorkspaceSession::new(handle, binding, cursor).unwrap()
    }

    fn exec_service(
        execute: Result<OperationStatus<serde_json::Value>, WorkspaceError>,
        statuses: Vec<OperationStatus<serde_json::Value>>,
    ) -> Arc<ExecService> {
        Arc::new(ExecService {
            execute: Mutex::new(execute),
            statuses: Mutex::new(statuses.into()),
            commands: Mutex::new(Vec::new()),
            cwd_handles: Mutex::new(Vec::new()),
            execute_calls: AtomicUsize::new(0),
            cancel_calls: AtomicUsize::new(0),
            cancel_on_status: Mutex::new(None),
            status_calls: AtomicUsize::new(0),
        })
    }

    #[test]
    fn remote_command_cancellation_before_dispatch_does_not_execute() {
        let service = exec_service(Ok(operation_status(OperationState::Running)), Vec::new());
        let workspace = workspace(Arc::clone(&service));
        let (trigger, cancel) = CancelToken::new();
        trigger.cancel();
        let (tx, _rx) = flume::unbounded();

        let result = smol::block_on(run_remote_command(
            &workspace, "canary", "shell", &tx, &cancel, 20, 1024,
        ));

        assert_eq!(result, Err(REMOTE_CANCELLED.into()));
        assert_eq!(service.execute_calls.load(Ordering::SeqCst), 0);
    }

    #[test_case(true; "confirmed")]
    #[test_case(false; "running_is_bounded")]
    fn remote_command_cancellation_after_dispatch_uses_remote_cancel(confirmed: bool) {
        let mut statuses = vec![operation_status(OperationState::Running)];
        if confirmed {
            statuses.push(operation_status(OperationState::Cancelled {
                side_effects_possible: false,
            }));
        } else {
            statuses.extend(
                (0..REMOTE_RECONCILE_MAX_POLLS).map(|_| operation_status(OperationState::Running)),
            );
        }
        let service = exec_service(Ok(operation_status(OperationState::Running)), statuses);
        let workspace = workspace(Arc::clone(&service));
        let (trigger, cancel) = CancelToken::new();
        let (tx, _rx) = flume::unbounded();
        *service.cancel_on_status.lock().unwrap() = Some(trigger);

        let result = smol::block_on(run_remote_command(
            &workspace, "canary", "shell", &tx, &cancel, 20, 1024,
        ));

        assert!(result.unwrap_err().contains(if confirmed {
            REMOTE_CANCELLED
        } else {
            REMOTE_INDETERMINATE
        }));
        assert_eq!(service.execute_calls.load(Ordering::SeqCst), 1);
        assert_eq!(service.cancel_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            service.status_calls.load(Ordering::SeqCst),
            1 + if confirmed {
                1
            } else {
                REMOTE_RECONCILE_MAX_POLLS
            }
        );
    }

    #[test]
    fn remote_policy_denial_and_indeterminate_are_final() {
        let denied = exec_service(Err(WorkspaceError::PolicyDenied), Vec::new());
        let indeterminate = exec_service(
            Ok(operation_status(OperationState::Indeterminate {
                side_effects_possible: true,
            })),
            Vec::new(),
        );
        let (_trigger, cancel) = CancelToken::new();
        let (tx, _rx) = flume::unbounded();

        let denial = smol::block_on(run_remote_command(
            &workspace(denied),
            "canary",
            "shell",
            &tx,
            &cancel,
            20,
            1024,
        ));
        let unknown = smol::block_on(run_remote_command(
            &workspace(indeterminate),
            "canary",
            "shell",
            &tx,
            &cancel,
            20,
            1024,
        ));

        assert_eq!(denial.unwrap_err(), "Remote command was denied by policy");
        assert!(unknown.unwrap_err().contains(REMOTE_INDETERMINATE));
    }

    #[test]
    fn remote_command_uses_the_cursor_and_never_runs_the_local_canary() {
        const CANARY: &str = "local canary must stay untouched";
        let directory = tempfile::tempdir().unwrap();
        let canary = directory.path().join("same-name-command");
        std::fs::write(&canary, CANARY).unwrap();
        let service = exec_service(
            Ok(operation_status(OperationState::Completed {
                result: json!({
                    "exitCode": 0,
                    "signal": null,
                    "timedOut": false,
                    "outputLimitExceeded": false,
                    "stdout": "remote",
                    "stderr": ""
                }),
                side_effects_possible: false,
            })),
            Vec::new(),
        );
        let (_trigger, cancel) = CancelToken::new();
        let (tx, _rx) = flume::unbounded();

        let result = smol::block_on(run_remote_command(
            &workspace(Arc::clone(&service)),
            "same-name-command",
            "shell",
            &tx,
            &cancel,
            20,
            1024,
        ));

        assert!(result.unwrap().contains("remote"));
        assert_eq!(&*service.commands.lock().unwrap(), &["same-name-command"]);
        assert_eq!(&*service.cwd_handles.lock().unwrap(), &["remote-cwd"]);
        assert_eq!(std::fs::read_to_string(canary).unwrap(), CANARY);
    }

    #[test]
    fn remote_progress_preserves_stream_order_and_reports_one_gap() {
        let mut output = BoundedText::new(20, 1024);
        let mut next = None;
        let mut gap = false;
        append_remote_progress(
            &remote_status(
                vec![
                    progress(4, OperationProgressKind::Stdout, "out"),
                    progress(5, OperationProgressKind::Stderr, "err"),
                ],
                true,
            ),
            &mut output,
            &mut next,
            &mut gap,
        );
        append_remote_progress(
            &remote_status(
                vec![
                    progress(5, OperationProgressKind::Stderr, "duplicate"),
                    progress(6, OperationProgressKind::Exited, "exit"),
                ],
                false,
            ),
            &mut output,
            &mut next,
            &mut gap,
        );

        assert_eq!(output.finish(), "[remote progress gap]\nouterr[exit]\n");
    }

    #[test]
    fn remote_terminal_status_distinguishes_success_and_failure() {
        let empty = BoundedText::new(20, 1024);
        let success = remote_terminal_output(
            &json!({
                "exitCode": 0,
                "signal": null,
                "timedOut": false,
                "outputLimitExceeded": false,
                "stdout": "ok",
                "stderr": ""
            }),
            &empty,
        )
        .unwrap();
        assert_eq!(success, "ok\n[shell status: exit code 0]");

        let failed = remote_terminal_output(
            &json!({
                "exitCode": 2,
                "signal": null,
                "timedOut": false,
                "outputLimitExceeded": false,
                "stdout": "",
                "stderr": "bad"
            }),
            &empty,
        )
        .unwrap_err();
        assert_eq!(failed, "bad\n[shell status: exit code 2]");
    }

    #[test]
    fn utf8_decoder_handles_code_point_split_across_chunks() {
        let mut decoder = Utf8Decoder::default();
        let mut output = BoundedText::new(10, 100);
        let crab = "蟹".as_bytes();

        decoder.push(&crab[..1], &mut output).unwrap();
        decoder.push(&crab[1..], &mut output).unwrap();
        decoder.finish().unwrap();

        assert_eq!(output.finish(), "蟹");
    }

    #[test]
    fn utf8_decoder_reports_invalid_and_incomplete_sequences() {
        let mut invalid = Utf8Decoder::default();
        let mut output = BoundedText::new(10, 100);
        assert!(invalid.push(b"valid\xff", &mut output).is_err());
        assert_eq!(output.finish(), "valid");

        let mut incomplete = Utf8Decoder::default();
        let mut output = BoundedText::new(10, 100);
        incomplete.push(&[0xe8, 0x9f], &mut output).unwrap();
        assert!(incomplete.finish().is_err());
    }

    #[test]
    fn complete_shell_preamble_is_bounded() {
        let result = bounded_shell_result(
            &"command\n".repeat(100),
            "Output",
            &"output\n".repeat(100),
            6,
            120,
        );

        assert!(fits(&result, 6, 120));
        assert!(result.ends_with(PREAMBLE_TRUNCATED_MARKER));
    }

    #[cfg(unix)]
    #[test]
    fn huge_unterminated_line_is_bounded_before_append() {
        const MAX_LINES: usize = 4;
        const MAX_BYTES: usize = 128;

        let (_trigger, cancel) = CancelToken::new();
        let (tx, rx) = flume::unbounded();
        let output = smol::block_on(run_command(
            "printf '%2000000s' x",
            "shell-test",
            &tx,
            &cancel,
            MAX_LINES,
            MAX_BYTES,
        ))
        .unwrap();

        assert!(fits(&output, MAX_LINES, MAX_BYTES));
        assert!(output.ends_with(TRUNCATED_MARKER));
        for event in rx.try_iter() {
            let ShellEvent::Output { content, .. } = event else {
                unreachable!();
            };
            assert!(fits(&content, MAX_LINES, MAX_BYTES));
        }
    }
}
