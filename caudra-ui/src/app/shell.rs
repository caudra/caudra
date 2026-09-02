use caudra_agent::tools::ToolEffect;
use std::collections::HashSet;
use std::process::Command as StdCommand;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use async_process::{Command, Stdio};
use caudra_agent::{
    AgentConfig, CancelToken, CancelTrigger, ToolDoneEvent, ToolInput, ToolOutput, ToolStartEvent,
};
use caudra_providers::Message;
use futures_lite::io::AsyncReadExt;

use super::App;

const STREAM_FLUSH_INTERVAL: Duration = Duration::from_millis(100);
const SHELL_TIMEOUT: Duration = Duration::from_secs(300);
const READ_CHUNK_BYTES: usize = 8 * 1024;
const READ_CHANNEL_CAPACITY: usize = 8;
const TRUNCATED_MARKER: &str = "[truncated]";
const PREAMBLE_TRUNCATED_MARKER: &str = "[shell result truncated]";

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
                    output_ref: None,
                    output_limits: None,
                    model_suffix: None,
                    model_output: None,
                    model_output_from_ref: false,
                });
                if let Some(msg) = result_msg {
                    self.shell.push_result(msg);
                }
                self.shell.release_id(&id);
            }
        }
    }
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
