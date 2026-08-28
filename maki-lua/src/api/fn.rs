use std::collections::HashMap;
use std::env;
use std::io::{self, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Function, Lua, RegistryKey, Result as LuaResult, Table, Value};

use crate::api::fs::expand_tilde;
use crate::api::util::command::{UiAction, ui_roundtrip, ui_send};
use crate::api::util::pair::{Pair, try_pair};
use crate::plugin_permissions::PluginPermissions;
use crate::runtime::{active_task_id, job_task_id, with_jobs};

const RAW_READER_CHUNK_SIZE: usize = 16 * 1024;
const LINE_FRAGMENT_MAX_BYTES: usize = 64 * 1024;
const JOB_STREAM_ERROR_MAX_BYTES: usize = 1024;
const JOB_EVENT_CHANNEL_CAPACITY: usize = 64;
const JOB_EVENT_DRAIN_BATCH_SIZE: usize = 64;
const JOB_EVENT_DRAIN_PER_JOB: usize = 8;
const JOBWAIT_MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone)]
pub(crate) enum JobEvent {
    Stdout(String),
    Stderr(String),
    StreamError(String),
    Exit(i32),
}

#[derive(Clone, Copy)]
enum JobStream {
    Stdout,
    Stderr,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum JobOwner {
    Task(u64),
    Plugin(Arc<str>),
}

struct JobMeta {
    owner: JobOwner,
    pid: u32,
    on_stdout: Option<RegistryKey>,
    on_stderr: Option<RegistryKey>,
    on_error: Option<RegistryKey>,
    on_exit: Option<RegistryKey>,
    raw_chunks: bool,
    event_rx: Option<flume::Receiver<JobEvent>>,
}

pub(crate) struct JobStore {
    jobs: HashMap<u32, JobMeta>,
    next_id: u32,
}

struct CheckedOutReceiver {
    lua: Lua,
    job_id: u32,
    raw_chunks: bool,
    receiver: Option<flume::Receiver<JobEvent>>,
}

struct WaitState {
    child: Child,
    readers: Vec<JoinHandle<()>>,
}

type WaitTask = Box<dyn FnOnce() + Send + 'static>;

fn event_channel() -> (flume::Sender<JobEvent>, flume::Receiver<JobEvent>) {
    flume::bounded(JOB_EVENT_CHANNEL_CAPACITY)
}

fn output_event(stream: JobStream, text: String) -> JobEvent {
    match stream {
        JobStream::Stdout => JobEvent::Stdout(text),
        JobStream::Stderr => JobEvent::Stderr(text),
    }
}

fn stream_name(stream: JobStream) -> &'static str {
    match stream {
        JobStream::Stdout => "stdout",
        JobStream::Stderr => "stderr",
    }
}

fn truncate_utf8(text: &mut String, max_bytes: usize) {
    if text.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes.saturating_sub(3);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    if max_bytes >= 3 {
        text.push_str("...");
    }
}

fn stream_error(stream: JobStream, detail: impl std::fmt::Display) -> JobEvent {
    let mut message = format!("job {} stream {detail}", stream_name(stream));
    truncate_utf8(&mut message, JOB_STREAM_ERROR_MAX_BYTES);
    JobEvent::StreamError(message)
}

fn send_text(tx: &flume::Sender<JobEvent>, stream: JobStream, text: &str) -> bool {
    text.is_empty() || tx.send(output_event(stream, text.to_owned())).is_ok()
}

fn read_raw_stream(mut reader: impl Read, stream: JobStream, tx: &flume::Sender<JobEvent>) {
    let mut chunk = [0; RAW_READER_CHUNK_SIZE];
    let mut pending = Vec::with_capacity(RAW_READER_CHUNK_SIZE + 3);
    loop {
        let read = match reader.read(&mut chunk) {
            Ok(0) => {
                if !pending.is_empty() {
                    let _ = tx.send(stream_error(stream, "ended with incomplete UTF-8"));
                }
                return;
            }
            Ok(read) => read,
            Err(error) => {
                let _ = tx.send(stream_error(stream, format_args!("read failed: {error}")));
                return;
            }
        };
        pending.extend_from_slice(&chunk[..read]);

        match std::str::from_utf8(&pending) {
            Ok(text) => {
                if !send_text(tx, stream, text) {
                    return;
                }
                pending.clear();
            }
            Err(error) => {
                let valid_up_to = error.valid_up_to();
                let invalid = error.error_len().is_some();
                if valid_up_to > 0 {
                    let text = std::str::from_utf8(&pending[..valid_up_to])
                        .expect("UTF-8 validator supplied a valid prefix");
                    if !send_text(tx, stream, text) {
                        return;
                    }
                    pending.drain(..valid_up_to);
                }
                if invalid {
                    let _ = tx.send(stream_error(stream, "is not valid UTF-8"));
                    return;
                }
            }
        }
    }
}

fn send_line(tx: &flume::Sender<JobEvent>, stream: JobStream, line: &mut String) -> bool {
    tx.send(output_event(stream, std::mem::take(line))).is_ok()
}

fn append_line_segment(
    segment: &str,
    line: &mut String,
    stream: JobStream,
    tx: &flume::Sender<JobEvent>,
) -> Result<(), ()> {
    if segment.len() <= LINE_FRAGMENT_MAX_BYTES - line.len() {
        line.push_str(segment);
        return Ok(());
    }

    let mut end = LINE_FRAGMENT_MAX_BYTES - line.len();
    while !segment.is_char_boundary(end) {
        end -= 1;
    }
    line.push_str(&segment[..end]);
    if !send_line(tx, stream, line) {
        return Err(());
    }
    let _ = tx.send(stream_error(
        stream,
        format_args!("line exceeded the {LINE_FRAGMENT_MAX_BYTES}-byte limit"),
    ));
    Err(())
}

fn process_line_text(
    mut text: &str,
    line: &mut String,
    stream: JobStream,
    tx: &flume::Sender<JobEvent>,
) -> Result<(), ()> {
    while let Some(newline) = text.find('\n') {
        append_line_segment(&text[..newline], line, stream, tx)?;
        if line.ends_with('\r') {
            line.pop();
        }
        if !send_line(tx, stream, line) {
            return Err(());
        }
        text = &text[newline + 1..];
    }
    append_line_segment(text, line, stream, tx)
}

fn flush_partial_line(tx: &flume::Sender<JobEvent>, stream: JobStream, line: &mut String) -> bool {
    line.is_empty() || send_line(tx, stream, line)
}

fn read_line_stream(mut reader: impl Read, stream: JobStream, tx: &flume::Sender<JobEvent>) {
    let mut chunk = [0; RAW_READER_CHUNK_SIZE];
    let mut utf8_pending = Vec::with_capacity(RAW_READER_CHUNK_SIZE + 3);
    let mut line = String::with_capacity(RAW_READER_CHUNK_SIZE);
    loop {
        let read = match reader.read(&mut chunk) {
            Ok(0) => {
                if !flush_partial_line(tx, stream, &mut line) {
                    return;
                }
                if !utf8_pending.is_empty() {
                    let _ = tx.send(stream_error(stream, "ended with incomplete UTF-8"));
                }
                return;
            }
            Ok(read) => read,
            Err(error) => {
                if !flush_partial_line(tx, stream, &mut line) {
                    return;
                }
                let _ = tx.send(stream_error(stream, format_args!("read failed: {error}")));
                return;
            }
        };
        utf8_pending.extend_from_slice(&chunk[..read]);

        match std::str::from_utf8(&utf8_pending) {
            Ok(text) => {
                if process_line_text(text, &mut line, stream, tx).is_err() {
                    return;
                }
                utf8_pending.clear();
            }
            Err(error) => {
                let valid_up_to = error.valid_up_to();
                let invalid = error.error_len().is_some();
                if valid_up_to > 0 {
                    let text = std::str::from_utf8(&utf8_pending[..valid_up_to])
                        .expect("UTF-8 validator supplied a valid prefix");
                    if process_line_text(text, &mut line, stream, tx).is_err() {
                        return;
                    }
                    utf8_pending.drain(..valid_up_to);
                }
                if invalid {
                    if !flush_partial_line(tx, stream, &mut line) {
                        return;
                    }
                    let _ = tx.send(stream_error(stream, "is not valid UTF-8"));
                    return;
                }
            }
        }
    }
}

fn spawn_reader<R: Read + Send + 'static>(
    stream: Option<R>,
    name: &str,
    kind: JobStream,
    raw_chunks: bool,
    event_tx: &flume::Sender<JobEvent>,
) -> Result<Option<JoinHandle<()>>, String> {
    let Some(stream) = stream else {
        return Ok(None);
    };
    let tx = event_tx.clone();
    thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            if raw_chunks {
                read_raw_stream(stream, kind, &tx);
            } else {
                read_line_stream(stream, kind, &tx);
            }
        })
        .map(Some)
        .map_err(|error| error.to_string())
}

fn run_waiter(state: Arc<Mutex<Option<WaitState>>>, event_tx: flume::Sender<JobEvent>) {
    let Some(mut state) = state
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
    else {
        return;
    };
    let status = state.child.wait();
    for reader in state.readers {
        let _ = reader.join();
    }
    let code = match status {
        Ok(status) => status.code().unwrap_or(-1),
        Err(error) => {
            let _ = event_tx.send(JobEvent::StreamError(format!(
                "job process wait failed: {error}"
            )));
            -1
        }
    };
    let _ = event_tx.send(JobEvent::Exit(code));
}

fn spawn_waiter_with(
    state: WaitState,
    event_tx: flume::Sender<JobEvent>,
    spawn: impl FnOnce(WaitTask) -> io::Result<JoinHandle<()>>,
) -> Result<(), (String, WaitState)> {
    let state = Arc::new(Mutex::new(Some(state)));
    let worker_state = Arc::clone(&state);
    match spawn(Box::new(move || run_waiter(worker_state, event_tx))) {
        Ok(_) => Ok(()),
        Err(error) => {
            let state = state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
                .expect("failed waiter spawn retained its state");
            Err((error.to_string(), state))
        }
    }
}

fn cleanup_failed_start(pid: u32, mut state: WaitState, event_rx: flume::Receiver<JobEvent>) {
    kill_process(pid);
    let _ = state.child.kill();
    drop(event_rx);
    let _ = state.child.wait();
    for reader in state.readers {
        let _ = reader.join();
    }
}

impl JobStore {
    pub fn new() -> Self {
        Self {
            jobs: HashMap::new(),
            next_id: 1,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn start(
        &mut self,
        owner: JobOwner,
        cmd: &str,
        cwd: Option<String>,
        env: Option<HashMap<String, String>>,
        on_stdout: Option<RegistryKey>,
        on_stderr: Option<RegistryKey>,
        on_error: Option<RegistryKey>,
        on_exit: Option<RegistryKey>,
        raw_chunks: bool,
    ) -> Result<u32, String> {
        let mut command = shell_command(cmd);
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());

        configure_process_group(&mut command);

        if let Some(dir) = cwd.as_deref().map(expand_tilde) {
            if !dir.is_dir() {
                return Err(format!("cwd is not a directory: {}", dir.display()));
            }
            command.current_dir(dir);
        }
        if let Some(ref env_map) = env {
            for (k, v) in env_map {
                command.env(k, v);
            }
        }

        let mut child = command.spawn().map_err(|e| e.to_string())?;
        let pid = child.id();
        let id = self.next_id;
        self.next_id += 1;

        let (event_tx, event_rx) = event_channel();
        let mut readers = Vec::new();
        let stdout_handle = match spawn_reader(
            child.stdout.take(),
            "job-stdout",
            JobStream::Stdout,
            raw_chunks,
            &event_tx,
        ) {
            Ok(handle) => handle,
            Err(error) => {
                cleanup_failed_start(pid, WaitState { child, readers }, event_rx);
                return Err(error);
            }
        };
        readers.extend(stdout_handle);
        let stderr_handle = match spawn_reader(
            child.stderr.take(),
            "job-stderr",
            JobStream::Stderr,
            raw_chunks,
            &event_tx,
        ) {
            Ok(handle) => handle,
            Err(error) => {
                cleanup_failed_start(pid, WaitState { child, readers }, event_rx);
                return Err(error);
            }
        };
        readers.extend(stderr_handle);

        if let Err((error, state)) =
            spawn_waiter_with(WaitState { child, readers }, event_tx, |task| {
                thread::Builder::new().name("job-wait".into()).spawn(task)
            })
        {
            cleanup_failed_start(pid, state, event_rx);
            return Err(error);
        }

        self.jobs.insert(
            id,
            JobMeta {
                owner,
                pid,
                on_stdout,
                on_stderr,
                on_error,
                on_exit,
                raw_chunks,
                event_rx: Some(event_rx),
            },
        );

        Ok(id)
    }

    pub fn is_empty(&self, owner: &JobOwner) -> bool {
        !self.jobs.values().any(|job| job.owner == *owner)
    }

    pub fn callback_key(&self, job_id: u32, event: &JobEvent) -> Option<&RegistryKey> {
        let meta = self.jobs.get(&job_id)?;
        match event {
            JobEvent::Stdout(_) => meta.on_stdout.as_ref(),
            JobEvent::Stderr(_) => meta.on_stderr.as_ref(),
            JobEvent::StreamError(_) => meta.on_error.as_ref(),
            JobEvent::Exit(_) => meta.on_exit.as_ref(),
        }
    }

    pub fn take_receiver(
        &mut self,
        job_id: u32,
        task_id: Option<u64>,
        plugin: &str,
    ) -> Option<(flume::Receiver<JobEvent>, bool)> {
        let job = self.jobs.get_mut(&job_id)?;
        job.can_access(task_id, plugin).then(|| {
            job.event_rx
                .take()
                .map(|receiver| (receiver, job.raw_chunks))
        })?
    }

    pub fn restore_receiver(&mut self, job_id: u32, receiver: flume::Receiver<JobEvent>) {
        if let Some(job) = self.jobs.get_mut(&job_id)
            && job.event_rx.is_none()
        {
            job.event_rx = Some(receiver);
        }
    }

    pub fn drain_events(&self, owner: &JobOwner, buf: &mut Vec<(u32, JobEvent)>) {
        buf.clear();
        for (&id, job) in self.jobs.iter().filter(|(_, job)| job.owner == *owner) {
            if let Some(ref rx) = job.event_rx {
                for _ in 0..JOB_EVENT_DRAIN_PER_JOB {
                    let Ok(event) = rx.try_recv() else {
                        break;
                    };
                    buf.push((id, event));
                    if buf.len() == JOB_EVENT_DRAIN_BATCH_SIZE {
                        return;
                    }
                }
            }
        }
    }

    pub fn drain_plugin_events(&self, buf: &mut Vec<(u32, JobEvent)>) {
        buf.clear();
        for (&id, job) in self
            .jobs
            .iter()
            .filter(|(_, job)| matches!(job.owner, JobOwner::Plugin(_)))
        {
            if let Some(ref rx) = job.event_rx {
                for _ in 0..JOB_EVENT_DRAIN_PER_JOB {
                    let Ok(event) = rx.try_recv() else {
                        break;
                    };
                    buf.push((id, event));
                    if buf.len() == JOB_EVENT_DRAIN_BATCH_SIZE {
                        return;
                    }
                }
            }
        }
    }

    pub fn stop_owner_receivers(
        &mut self,
        owner: &JobOwner,
    ) -> Vec<(u32, flume::Receiver<JobEvent>)> {
        let ids = self
            .jobs
            .iter()
            .filter_map(|(&id, job)| (job.owner == *owner).then_some(id))
            .collect::<Vec<_>>();
        for id in &ids {
            if let Some(job) = self.jobs.get_mut(id) {
                kill_job(job);
            }
        }
        ids.into_iter()
            .filter_map(|id| {
                self.jobs
                    .get_mut(&id)?
                    .event_rx
                    .take()
                    .map(|receiver| (id, receiver))
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn queued_event_count(&self, owner: &JobOwner) -> usize {
        self.jobs
            .values()
            .filter(|job| job.owner == *owner)
            .filter_map(|job| job.event_rx.as_ref())
            .map(flume::Receiver::len)
            .sum()
    }

    pub fn kill(&mut self, job_id: u32, task_id: Option<u64>, plugin: &str) {
        if let Some(job) = self.jobs.get_mut(&job_id)
            && job.can_access(task_id, plugin)
        {
            kill_job(job);
        }
    }

    pub fn kill_owner(&mut self, lua: &Lua, owner: &JobOwner) {
        let ids = self
            .jobs
            .iter()
            .filter_map(|(&id, job)| (job.owner == *owner).then_some(id))
            .collect::<Vec<_>>();
        for id in ids {
            self.remove(lua, id, true);
        }
    }

    pub fn finish(&mut self, lua: &Lua, job_id: u32) {
        self.remove(lua, job_id, false);
    }

    fn remove(&mut self, lua: &Lua, job_id: u32, kill: bool) {
        if let Some(mut job) = self.jobs.remove(&job_id) {
            if kill {
                kill_job(&mut job);
            }
            for key in [job.on_stdout, job.on_stderr, job.on_error, job.on_exit]
                .into_iter()
                .flatten()
            {
                lua.remove_registry_value(key).ok();
            }
        }
    }

    fn kill_all(&mut self) {
        for job in self.jobs.values_mut() {
            kill_job(job);
        }
    }
}

impl Drop for JobStore {
    fn drop(&mut self) {
        self.kill_all();
    }
}

impl CheckedOutReceiver {
    fn new(lua: &Lua, job_id: u32, receiver: flume::Receiver<JobEvent>, raw_chunks: bool) -> Self {
        Self {
            lua: lua.clone(),
            job_id,
            raw_chunks,
            receiver: Some(receiver),
        }
    }

    fn get(&self) -> &flume::Receiver<JobEvent> {
        self.receiver.as_ref().expect("receiver is checked out")
    }
}

impl Drop for CheckedOutReceiver {
    fn drop(&mut self) {
        if let Some(receiver) = self.receiver.take() {
            with_jobs(&self.lua, |store| {
                store.restore_receiver(self.job_id, receiver);
            });
        }
    }
}

impl JobMeta {
    fn can_access(&self, task_id: Option<u64>, plugin: &str) -> bool {
        match &self.owner {
            JobOwner::Task(owner_id) => task_id == Some(*owner_id),
            JobOwner::Plugin(owner_plugin) => owner_plugin.as_ref() == plugin,
        }
    }
}

fn shell_command(cmd: &str) -> Command {
    #[cfg(unix)]
    {
        let mut c = Command::new("bash");
        c.arg("-c").arg(cmd);
        c
    }
    #[cfg(windows)]
    {
        let mut c = Command::new("cmd.exe");
        c.arg("/C").arg(cmd);
        c
    }
}

fn configure_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe, so it is sound to call in pre_exec.
        unsafe {
            command.pre_exec(|| {
                rustix::process::setsid()?;
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    let _ = command;
}

fn kill_process(pid: u32) {
    #[cfg(unix)]
    {
        use rustix::process::{Pid, Signal, kill_process_group};
        let raw = match i32::try_from(pid) {
            Ok(raw) => raw,
            Err(_) => return,
        };
        if let Some(pid) = Pid::from_raw(raw) {
            let _ = kill_process_group(pid, Signal::KILL);
        }
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

fn kill_job(meta: &mut JobMeta) {
    kill_process(meta.pid);
}

/// Run a shell command in the background. The command runs through
/// `bash -c` on Unix or `cmd /C` on Windows. You get back a job id
/// that you can pass to `jobstop` or `jobwait` to control the process.
///
/// @param cmd string Shell command to run.
/// @param opts table? Optional settings:
///   `cwd` (string?) working directory (tilde is expanded).
///   `env` (table?) extra environment variables, `{ VAR = "value" }`.
///   `on_stdout` (function?) called with `(job_id, line)` for each stdout line.
///   `on_stderr` (function?) called with `(job_id, line)` for each stderr line.
///   `on_error` (function?) called with `(job_id, message)` on stream failures.
///   `on_exit` (function?) called with `(job_id, code)` when the process finishes.
///   `raw_chunks` (boolean?) deliver bounded chunks with exact newlines instead
///     of line callbacks. Defaults to false.
///   `owner` (string?) job lifetime. `"task"` (default) ends the job with
///     the current call. `"plugin"` keeps it alive until the plugin unloads
///     or reloads.
/// @return (integer) Job id.
/// @example
/// local id = maki.fn.jobstart("ls -la", {
///   cwd = "~/projects",
///   on_stdout = function(_, line) print(line) end,
///   on_exit = function(_, code) print("exit: " .. code) end,
/// })
#[lua_fn(guard = Run)]
fn jobstart(
    lua: &Lua,
    #[ctx] plugin: Arc<str>,
    cmd: String,
    opts: Option<Table>,
) -> LuaResult<u32> {
    let owner_name: Option<String> = opts
        .as_ref()
        .map(|opts| opts.get("owner"))
        .transpose()?
        .flatten();
    let owner = match owner_name.as_deref() {
        None | Some("task") => job_task_id(lua).map(JobOwner::Task).ok_or_else(|| {
            mlua::Error::runtime("jobstart: no active task; use owner = \"plugin\"")
        })?,
        Some("plugin") => JobOwner::Plugin(Arc::clone(&plugin)),
        Some(other) => {
            return Err(mlua::Error::runtime(format!(
                "jobstart: unknown owner {other:?}; expected \"task\" or \"plugin\""
            )));
        }
    };

    let (cwd, env, on_stdout, on_stderr, on_error, on_exit, raw_chunks) = match opts {
        Some(ref opts) => {
            let cwd: Option<String> = opts.get("cwd").ok();
            let env: Option<HashMap<String, String>> = opts
                .get::<Table>("env")
                .ok()
                .map(|t| t.pairs::<String, String>().filter_map(Result::ok).collect());
            let on_stdout = opts
                .get::<Function>("on_stdout")
                .ok()
                .map(|f| lua.create_registry_value(f))
                .transpose()?;
            let on_stderr = opts
                .get::<Function>("on_stderr")
                .ok()
                .map(|f| lua.create_registry_value(f))
                .transpose()?;
            let on_error = opts
                .get::<Function>("on_error")
                .ok()
                .map(|f| lua.create_registry_value(f))
                .transpose()?;
            let on_exit = opts
                .get::<Function>("on_exit")
                .ok()
                .map(|f| lua.create_registry_value(f))
                .transpose()?;
            let raw_chunks = opts.get::<Option<bool>>("raw_chunks")?.unwrap_or(false);
            (
                cwd, env, on_stdout, on_stderr, on_error, on_exit, raw_chunks,
            )
        }
        None => (None, None, None, None, None, None, false),
    };

    with_jobs(lua, |store| {
        store.start(
            owner, &cmd, cwd, env, on_stdout, on_stderr, on_error, on_exit, raw_chunks,
        )
    })
    .map_err(mlua::Error::runtime)
}

/// Kill a running job immediately (SIGKILL on Unix). Safe to call on
/// jobs that already exited or on unknown ids.
///
/// @param job_id integer Job id returned by `jobstart`.
/// @return
/// @example
/// maki.fn.jobstop(id)
#[lua_fn(guard = Run)]
fn jobstop(lua: &Lua, #[ctx] plugin: Arc<str>, job_id: u32) -> LuaResult<()> {
    let task_id = active_task_id(lua);
    with_jobs(lua, |store| store.kill(job_id, task_id, &plugin));
    Ok(())
}

/// Wait for a job to finish and collect its output. Returns a result
/// table with `stdout`, `stderr`, and `exit_code`. Returns `nil` if the
/// job does not finish before the timeout. Collection is limited to 16 MiB
/// across stdout and stderr; larger output raises an explicit error.
///
/// While waiting, the job's `on_stdout`, `on_stderr`, and `on_exit`
/// callbacks fire as events arrive (like Neovim), so you can stream
/// output into a buffer while parked here.
///
/// @param job_id integer Job id returned by `jobstart`.
/// @param timeout_ms integer? Maximum wait in milliseconds (default 30000).
/// @return (table?) `{ stdout, stderr, exit_code }`, or nil on timeout.
/// @example
/// local id = maki.fn.jobstart("echo hello")
/// local result = maki.fn.jobwait(id, 5000)
/// if result then
///   print(result.stdout)
/// end
#[lua_fn(guard = Run)]
async fn jobwait(
    lua: Lua,
    #[ctx] plugin: Arc<str>,
    job_id: u32,
    timeout_ms: Option<u64>,
) -> LuaResult<Value> {
    let task_id = active_task_id(&lua);
    let (receiver, raw_chunks) =
        with_jobs(&lua, |store| store.take_receiver(job_id, task_id, &plugin))
            .ok_or_else(|| mlua::Error::runtime("unknown job id or already waited"))?;
    let receiver = CheckedOutReceiver::new(&lua, job_id, receiver, raw_chunks);

    let timeout = Duration::from_millis(timeout_ms.unwrap_or(30_000));
    let deadline = smol::Timer::after(timeout);
    futures_lite::pin!(deadline);

    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut stdout_has_output = false;
    let mut stderr_has_output = false;
    let mut collected_bytes = 0;

    let exit_code = loop {
        let event =
            futures_lite::future::or(async { receiver.get().recv_async().await.ok() }, async {
                (&mut deadline).await;
                None
            })
            .await;

        let Some(event) = event else {
            return Ok(mlua::Value::Nil);
        };
        deliver_job_event(&lua, job_id, &event)?;
        match event {
            JobEvent::Stdout(text) => append_jobwait_output(
                &mut stdout,
                &mut stdout_has_output,
                &mut collected_bytes,
                &text,
                receiver.raw_chunks,
            )?,
            JobEvent::Stderr(text) => append_jobwait_output(
                &mut stderr,
                &mut stderr_has_output,
                &mut collected_bytes,
                &text,
                receiver.raw_chunks,
            )?,
            JobEvent::StreamError(error) => {
                return Err(mlua::Error::runtime(error));
            }
            JobEvent::Exit(code) => break code,
        }
    };

    let result = lua.create_table()?;
    result.set("stdout", stdout)?;
    result.set("stderr", stderr)?;
    result.set("exit_code", exit_code)?;
    Ok(mlua::Value::Table(result))
}

fn append_jobwait_output(
    output: &mut String,
    has_output: &mut bool,
    collected_bytes: &mut usize,
    text: &str,
    raw_chunks: bool,
) -> LuaResult<()> {
    let separator_bytes = usize::from(!raw_chunks && *has_output);
    let next_bytes = collected_bytes
        .saturating_add(separator_bytes)
        .saturating_add(text.len());
    if next_bytes > JOBWAIT_MAX_OUTPUT_BYTES {
        return Err(mlua::Error::runtime(format!(
            "jobwait output exceeded the {JOBWAIT_MAX_OUTPUT_BYTES}-byte collection limit"
        )));
    }
    if separator_bytes > 0 {
        output.push('\n');
    }
    output.push_str(text);
    *has_output = true;
    *collected_bytes = next_bytes;
    Ok(())
}

/// Fire the job's Lua callback for {event} (if any) and mark the job
/// dead on exit. Shared by `jobwait` and the async dispatch loop so
/// both deliver events identically.
pub(crate) fn deliver_job_event(lua: &Lua, job_id: u32, event: &JobEvent) -> LuaResult<()> {
    let callback = with_jobs(lua, |store| {
        store
            .callback_key(job_id, event)
            .and_then(|key| lua.registry_value::<Function>(key).ok())
    });
    if let JobEvent::Exit(_) = event {
        with_jobs(lua, |store| store.finish(lua, job_id));
    }
    if let Some(callback) = callback {
        let arg = match event {
            JobEvent::Stdout(line) | JobEvent::Stderr(line) | JobEvent::StreamError(line) => {
                Value::String(lua.create_string(line)?)
            }
            JobEvent::Exit(code) => Value::Integer(*code as i64),
        };
        callback.call::<()>((job_id, arg))?;
    } else if let JobEvent::StreamError(error) = event {
        return Err(mlua::Error::runtime(error));
    }
    Ok(())
}

/// Check whether {name} can be found on `$PATH` or is an absolute path
/// to a file. Returns 1 when found, 0 otherwise (matches Neovim's
/// `vim.fn.executable`).
///
/// @param name string Program name (e.g. `"git"`) or absolute path.
/// @return (integer) `1` if found, `0` otherwise.
/// @example
/// if maki.fn.executable("rg") == 1 then
///   -- use ripgrep
/// end
#[lua_fn(guard = Env)]
fn executable(_lua: &Lua, name: String) -> LuaResult<i32> {
    let found = env::var_os("PATH")
        .map(|paths| env::split_paths(&paths).any(|dir| dir.join(&name).is_file()))
        .unwrap_or(false)
        || Path::new(&name).is_file();
    Ok(if found { 1 } else { 0 })
}

/// Read the viewport of the focused chat transcript, like Neovim's
/// `vim.fn.winsaveview()`. The transcript is the only scrollable window
/// maki has, so there is no window argument.
///
/// `topline` is the 1-based transcript line at the top of the viewport, so
/// the last visible one is `math.min(topline + height - 1, line_count)`.
/// `auto_scroll` has no Vim counterpart: it is true while the transcript
/// follows streaming output.
///
/// @return (table|nil, string|nil) `{topline, line_count, height, auto_scroll}`, or nil and an error.
/// @example
/// local view = maki.fn.winsaveview()
/// maki.fn.winrestview({ topline = view.topline + 1 })
#[lua_fn]
async fn winsaveview(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
) -> LuaResult<Pair<Table>> {
    let view =
        try_pair!(ui_roundtrip(tx.as_ref(), |reply_tx| UiAction::WinSaveView { reply_tx }).await);
    let t = lua.create_table()?;
    t.set("topline", i64::from(view.scroll_top) + 1)?;
    t.set("line_count", view.line_count)?;
    t.set("height", view.height)?;
    t.set("auto_scroll", view.auto_scroll)?;
    Ok((Some(t), None))
}

/// Scroll the focused chat transcript so that the `topline` field of
/// {view} becomes the top visible line, like Neovim's
/// `vim.fn.winrestview()`. Out of range values are clamped. Other keys are
/// ignored, so a table straight from `winsaveview()` round-trips.
///
/// Scrolling away from the bottom unpins the transcript; landing back at
/// the bottom re-pins it so streaming output keeps following.
///
/// @param view table View to restore. Only `topline` (1-based) is read.
/// @return (boolean|nil, string|nil) true on success, or nil and an error.
/// @example
/// maki.fn.winrestview({ topline = 1 })
#[lua_fn]
fn winrestview(
    _lua: &Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    view: Table,
) -> LuaResult<Pair<bool>> {
    let topline = view.get::<Option<i64>>("topline")?.unwrap_or(1);
    let scroll_top = topline.saturating_sub(1).clamp(0, u16::MAX as i64) as u16;
    try_pair!(ui_send(tx.as_ref(), UiAction::WinRestView { scroll_top }));
    Ok((Some(true), None))
}

lua_table! {
    /// Process and environment helpers, modeled after Neovim's `vim.fn` job
    /// control. Use these to run shell commands, wait for output, and check
    /// whether programs are installed.
    ///
    /// ```lua
    /// local id = maki.fn.jobstart("git status", {
    ///   on_exit = function(code) print("done: " .. code) end,
    /// })
    /// ```
    "maki.fn" => pub(crate) fn create_fn_table(
        plugin: Arc<str>,
        perms: &PluginPermissions,
        tx: Option<flume::Sender<UiAction>>,
    ), DOCS [
        jobstart(perms, plugin), jobstop(perms, plugin), jobwait(perms, plugin), executable(perms),
        winsaveview(tx), winrestview(tx),
    ]
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io;

    use super::*;
    use crate::api::util::command::{NO_UI_ERR, WinView};

    const TEST_PLUGIN: &str = "test-plugin";

    struct ScriptedReader {
        reads: VecDeque<io::Result<Vec<u8>>>,
    }

    impl ScriptedReader {
        fn new(reads: impl IntoIterator<Item = io::Result<Vec<u8>>>) -> Self {
            Self {
                reads: reads.into_iter().collect(),
            }
        }
    }

    impl Read for ScriptedReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let Some(read) = self.reads.pop_front() else {
                return Ok(0);
            };
            let bytes = read?;
            assert!(bytes.len() <= buf.len());
            buf[..bytes.len()].copy_from_slice(&bytes);
            Ok(bytes.len())
        }
    }

    fn raw_events(reader: impl Read) -> Vec<JobEvent> {
        let (tx, rx) = event_channel();
        read_raw_stream(reader, JobStream::Stdout, &tx);
        drop(tx);
        rx.into_iter().collect()
    }

    fn line_events(reader: impl Read) -> Vec<JobEvent> {
        let (tx, rx) = event_channel();
        read_line_stream(reader, JobStream::Stdout, &tx);
        drop(tx);
        rx.into_iter().collect()
    }

    #[test]
    fn raw_reader_reconstructs_exact_chunks_and_split_utf8() {
        let events = raw_events(ScriptedReader::new([
            Ok(b"alpha\n\xe2".to_vec()),
            Ok(b"\x82\xac\nomega\n".to_vec()),
        ]));
        let output = events
            .iter()
            .filter_map(|event| match event {
                JobEvent::Stdout(chunk) => Some(chunk.as_str()),
                _ => None,
            })
            .collect::<String>();

        assert_eq!(output, "alpha\n€\nomega\n");
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, JobEvent::StreamError(_)))
        );
    }

    #[test]
    fn raw_reader_reports_invalid_utf8() {
        let events = raw_events(ScriptedReader::new([Ok(vec![b'a', 0xff])]));

        assert!(matches!(&events[0], JobEvent::Stdout(chunk) if chunk == "a"));
        assert!(matches!(
            &events[1],
            JobEvent::StreamError(error) if error.contains("not valid UTF-8")
        ));
    }

    #[test]
    fn raw_reader_reports_read_errors_after_accepted_output() {
        let events = raw_events(ScriptedReader::new([
            Ok(b"accepted".to_vec()),
            Err(io::Error::other("reader exploded")),
        ]));

        assert!(matches!(&events[0], JobEvent::Stdout(chunk) if chunk == "accepted"));
        assert!(matches!(
            &events[1],
            JobEvent::StreamError(error) if error.contains("reader exploded")
        ));
    }

    #[test]
    fn raw_reader_bounds_an_unterminated_line_to_transport_chunks() {
        let bytes = vec![b'x'; RAW_READER_CHUNK_SIZE * 3 + 17];
        let events = raw_events(io::Cursor::new(bytes.clone()));
        let chunks = events
            .iter()
            .filter_map(|event| match event {
                JobEvent::Stdout(chunk) => Some(chunk),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.len() <= RAW_READER_CHUNK_SIZE)
        );
        let reconstructed = chunks
            .into_iter()
            .map(|chunk| chunk.as_str())
            .collect::<String>();
        assert_eq!(reconstructed.as_bytes(), bytes);
    }

    #[test]
    fn line_reader_preserves_normal_lines_and_split_utf8() {
        let events = line_events(ScriptedReader::new([
            Ok(b"alpha\r\n\xe2".to_vec()),
            Ok(b"\x82\xac\nomega".to_vec()),
        ]));
        let lines = events
            .iter()
            .map(|event| match event {
                JobEvent::Stdout(line) => line.as_str(),
                _ => panic!("unexpected line event"),
            })
            .collect::<Vec<_>>();

        assert_eq!(lines, ["alpha", "€", "omega"]);
    }

    #[test]
    fn line_reader_bounds_a_huge_unterminated_line_and_reports_it() {
        let events = line_events(io::Cursor::new(vec![b'x'; LINE_FRAGMENT_MAX_BYTES + 1]));

        assert!(
            matches!(&events[0], JobEvent::Stdout(fragment) if fragment.len() == LINE_FRAGMENT_MAX_BYTES)
        );
        assert!(matches!(
            &events[1],
            JobEvent::StreamError(error) if error.contains("line exceeded")
        ));
    }

    #[test_case::test_case(
        ScriptedReader::new([Ok(b"accepted".to_vec()), Err(io::Error::other("reader exploded"))]),
        "reader exploded"
        ; "read error"
    )]
    #[test_case::test_case(
        ScriptedReader::new([Ok(vec![b'a', b'c', b'c', b'e', b'p', b't', b'e', b'd', 0xff])]),
        "not valid UTF-8"
        ; "invalid UTF-8"
    )]
    fn line_reader_delivers_partial_data_before_an_explicit_error(
        reader: ScriptedReader,
        expected_error: &str,
    ) {
        let events = line_events(reader);

        assert!(matches!(&events[0], JobEvent::Stdout(line) if line == "accepted"));
        assert!(matches!(
            &events[1],
            JobEvent::StreamError(error) if error.contains(expected_error)
        ));
    }

    #[test]
    fn job_event_transport_is_bounded() {
        let (tx, _rx) = event_channel();
        assert_eq!(tx.capacity(), Some(JOB_EVENT_CHANNEL_CAPACITY));
    }

    #[test]
    fn event_drains_are_bounded_and_fair_per_job() {
        const JOB_COUNT: u32 = 10;
        const EVENTS_PER_JOB: usize = 16;

        let owner = task_owner(1);
        let mut store = make_store();
        let mut senders = Vec::new();
        for id in 1..=JOB_COUNT {
            let (tx, rx) = event_channel();
            for event in 0..EVENTS_PER_JOB {
                tx.try_send(JobEvent::Stdout(event.to_string())).unwrap();
            }
            senders.push(tx);
            store.jobs.insert(
                id,
                JobMeta {
                    owner: owner.clone(),
                    pid: 0,
                    on_stdout: None,
                    on_stderr: None,
                    on_error: None,
                    on_exit: None,
                    raw_chunks: true,
                    event_rx: Some(rx),
                },
            );
        }

        let mut events = Vec::new();
        store.drain_events(&owner, &mut events);

        assert_eq!(events.len(), JOB_EVENT_DRAIN_BATCH_SIZE);
        for id in 1..=JOB_COUNT {
            assert!(
                events.iter().filter(|(job_id, _)| *job_id == id).count()
                    <= JOB_EVENT_DRAIN_PER_JOB
            );
        }
        drop(senders);
    }

    #[test]
    fn jobwait_collection_cap_is_explicit_and_includes_separators() {
        let mut output = String::new();
        let mut has_output = true;
        let mut collected_bytes = JOBWAIT_MAX_OUTPUT_BYTES - 1;

        append_jobwait_output(
            &mut output,
            &mut has_output,
            &mut collected_bytes,
            "x",
            true,
        )
        .unwrap();
        let error = append_jobwait_output(
            &mut output,
            &mut has_output,
            &mut collected_bytes,
            "",
            false,
        )
        .unwrap_err();

        assert!(error.to_string().contains("collection limit"));
        assert_eq!(collected_bytes, JOBWAIT_MAX_OUTPUT_BYTES);
    }

    #[test]
    fn unhandled_stream_errors_are_delivered_as_lua_errors() {
        let lua = Lua::new();
        lua.set_app_data(JobStore::new());
        with_jobs(&lua, |store| {
            store.jobs.insert(
                1,
                JobMeta {
                    owner: task_owner(1),
                    pid: 0,
                    on_stdout: None,
                    on_stderr: None,
                    on_error: None,
                    on_exit: None,
                    raw_chunks: true,
                    event_rx: None,
                },
            );
        });

        let error = deliver_job_event(&lua, 1, &JobEvent::StreamError("reader exploded".into()))
            .unwrap_err();

        assert!(error.to_string().contains("reader exploded"));
        with_jobs(&lua, |store| store.finish(&lua, 1));
    }

    fn make_store() -> JobStore {
        JobStore::new()
    }

    fn task_owner(id: u64) -> JobOwner {
        JobOwner::Task(id)
    }

    fn plugin_owner() -> JobOwner {
        JobOwner::Plugin(Arc::from(TEST_PLUGIN))
    }

    fn start_echo(store: &mut JobStore) -> u32 {
        store
            .start(
                task_owner(1),
                "echo hello",
                None,
                None,
                None,
                None,
                None,
                None,
                false,
            )
            .unwrap()
    }

    #[cfg(unix)]
    fn group_alive(pid: u32) -> bool {
        use rustix::process::{Pid, test_kill_process_group};
        i32::try_from(pid)
            .ok()
            .and_then(Pid::from_raw)
            .is_some_and(|pid| test_kill_process_group(pid).is_ok())
    }

    #[cfg(unix)]
    fn wait_for_group_exit(pid: u32) -> bool {
        (0..500).any(|_| {
            thread::sleep(Duration::from_millis(10));
            !group_alive(pid)
        })
    }

    #[cfg(unix)]
    #[test]
    fn failed_waiter_spawn_cleans_a_child_with_a_blocked_reader() {
        let mut command = shell_command("printf '%02000000d' 0; sleep 30");
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());
        configure_process_group(&mut command);
        let mut child = command.spawn().unwrap();
        let pid = child.id();
        let (event_tx, event_rx) = event_channel();
        let stdout = spawn_reader(
            child.stdout.take(),
            "failed-start-reader",
            JobStream::Stdout,
            true,
            &event_tx,
        )
        .unwrap();
        let readers = stdout.into_iter().collect();
        let (_, state) = spawn_waiter_with(WaitState { child, readers }, event_tx, |_| {
            Err(io::Error::other("injected waiter spawn failure"))
        })
        .unwrap_err();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while event_rx.len() < JOB_EVENT_CHANNEL_CAPACITY {
            assert!(
                std::time::Instant::now() < deadline,
                "reader never blocked on the full event channel"
            );
            thread::yield_now();
        }

        cleanup_failed_start(pid, state, event_rx);

        assert!(wait_for_group_exit(pid));
    }

    #[cfg(unix)]
    #[test]
    fn dropping_the_store_kills_its_jobs() {
        let mut store = make_store();
        let id = store
            .start(
                task_owner(1),
                "sleep 30",
                None,
                None,
                None,
                None,
                None,
                None,
                false,
            )
            .expect("job started");
        let pid = store.jobs[&id].pid;
        assert!(group_alive(pid), "job should be running before the drop");

        drop(store);

        assert!(
            wait_for_group_exit(pid),
            "dropping the store must not orphan the process group"
        );
    }

    #[test]
    fn start_invalid_cwd_returns_error() {
        let mut store = make_store();
        let result = store.start(
            task_owner(1),
            "echo hello",
            Some("/nonexistent_dir_abc_xyz_123".into()),
            None,
            None,
            None,
            None,
            None,
            false,
        );
        assert!(result.is_err());
    }

    #[test]
    fn finishing_a_job_removes_it() {
        let lua = Lua::new();
        let mut store = make_store();
        let owner = task_owner(1);
        assert!(store.is_empty(&owner));

        let id = start_echo(&mut store);
        assert!(!store.is_empty(&owner));
        let (receiver, _) = store.take_receiver(id, Some(1), TEST_PLUGIN).unwrap();
        while !matches!(
            receiver.recv_timeout(Duration::from_secs(5)).unwrap(),
            JobEvent::Exit(_)
        ) {}

        store.finish(&lua, id);
        assert!(store.is_empty(&owner));
    }

    #[test]
    fn unknown_job_operations_are_noops() {
        let mut store = make_store();
        store.kill(999, Some(1), TEST_PLUGIN);
        assert!(store.take_receiver(999, Some(1), TEST_PLUGIN).is_none());
        assert!(store.callback_key(999, &JobEvent::Exit(0)).is_none());
    }

    #[test]
    fn take_receiver_lifecycle() {
        let mut store = make_store();
        assert!(store.take_receiver(999, Some(1), TEST_PLUGIN).is_none());

        let id = start_echo(&mut store);
        assert!(
            store.take_receiver(id, Some(2), TEST_PLUGIN).is_none(),
            "another task must not access the job"
        );
        assert!(store.take_receiver(id, Some(1), TEST_PLUGIN).is_some());
        assert!(
            store.take_receiver(id, Some(1), TEST_PLUGIN).is_none(),
            "second take should fail (receiver already moved)"
        );
    }

    #[test]
    fn plugin_owner_can_be_accessed_only_by_its_plugin() {
        let mut store = make_store();
        let id = store
            .start(
                plugin_owner(),
                "echo hello",
                None,
                None,
                None,
                None,
                None,
                None,
                false,
            )
            .unwrap();

        assert!(store.take_receiver(id, Some(1), "other-plugin").is_none());
        assert!(store.take_receiver(id, None, TEST_PLUGIN).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn kill_requires_owner_access() {
        let lua = Lua::new();
        let mut store = make_store();
        let id = store
            .start(
                task_owner(1),
                "sleep 30",
                None,
                None,
                None,
                None,
                None,
                None,
                false,
            )
            .unwrap();
        let pid = store.jobs[&id].pid;

        store.kill(id, Some(2), TEST_PLUGIN);
        assert!(group_alive(pid));

        store.kill(id, Some(1), TEST_PLUGIN);
        assert!(wait_for_group_exit(pid));
        store.finish(&lua, id);
    }

    #[cfg(unix)]
    #[test]
    fn owner_cleanup_is_isolated() {
        let lua = Lua::new();
        let mut store = make_store();
        let task = task_owner(1);
        let plugin = plugin_owner();
        let task_id = store
            .start(
                task.clone(),
                "sleep 30",
                None,
                None,
                None,
                None,
                None,
                None,
                false,
            )
            .unwrap();
        let plugin_id = store
            .start(
                plugin.clone(),
                "sleep 30",
                None,
                None,
                None,
                None,
                None,
                None,
                false,
            )
            .unwrap();
        let task_pid = store.jobs[&task_id].pid;
        let plugin_pid = store.jobs[&plugin_id].pid;

        store.kill_owner(&lua, &task);

        assert!(store.is_empty(&task));
        assert!(!store.is_empty(&plugin));
        assert!(wait_for_group_exit(task_pid));
        assert!(group_alive(plugin_pid));
        store.kill_owner(&lua, &plugin);
        assert!(wait_for_group_exit(plugin_pid));
    }

    #[test]
    fn callback_key_returns_none_without_callbacks() {
        let mut store = make_store();
        let id = start_echo(&mut store);
        assert!(
            store
                .callback_key(id, &JobEvent::Stdout("x".into()))
                .is_none()
        );
        assert!(
            store
                .callback_key(id, &JobEvent::Stderr("x".into()))
                .is_none()
        );
        assert!(store.callback_key(id, &JobEvent::Exit(0)).is_none());
    }

    #[test]
    fn take_receiver_delivers_events() {
        let mut store = make_store();
        let id = start_echo(&mut store);
        let (rx, _) = store.take_receiver(id, Some(1), TEST_PLUGIN).unwrap();

        let mut got_exit = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(JobEvent::Exit(_)) => {
                    got_exit = true;
                    break;
                }
                Ok(_) => continue,
                Err(flume::RecvTimeoutError::Timeout) => continue,
                Err(flume::RecvTimeoutError::Disconnected) => break,
            }
        }
        assert!(got_exit, "should receive exit event for completed job");
    }

    #[test]
    fn drain_events_filters_by_owner() {
        let mut store = make_store();
        let id = start_echo(&mut store);
        let plugin_id = store
            .start(
                plugin_owner(),
                "echo plugin",
                None,
                None,
                None,
                None,
                None,
                None,
                false,
            )
            .unwrap();

        let mut buf = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            store.drain_events(&task_owner(1), &mut buf);
            if buf
                .iter()
                .any(|(jid, e)| *jid == id && matches!(e, JobEvent::Exit(_)))
            {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("should receive exit event for completed job");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(buf.iter().all(|(job_id, _)| *job_id != plugin_id));

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            store.drain_plugin_events(&mut buf);
            if buf
                .iter()
                .any(|(job_id, event)| *job_id == plugin_id && matches!(event, JobEvent::Exit(_)))
            {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("should receive plugin job exit event");
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(buf.iter().all(|(job_id, _)| *job_id != id));
    }

    #[test]
    fn drain_events_empty_after_take() {
        let mut store = make_store();
        let id = start_echo(&mut store);
        let _rx = store.take_receiver(id, Some(1), TEST_PLUGIN).unwrap();

        let mut buf = Vec::new();
        store.drain_events(&task_owner(1), &mut buf);
        assert!(
            buf.is_empty(),
            "drained receiver yields no events via drain_events"
        );
    }

    fn lua_with_view(tx: Option<flume::Sender<UiAction>>) -> Lua {
        let lua = Lua::new();
        let t = lua.create_table().unwrap();
        winsaveview__register(&t, &lua, tx.clone()).unwrap();
        winrestview__register(&t, &lua, tx).unwrap();
        lua.globals().set("f", t).unwrap();
        lua
    }

    #[test_case::test_case("return f.winsaveview()" ; "winsaveview")]
    #[test_case::test_case("return f.winrestview({ topline = 3 })" ; "winrestview")]
    fn view_without_ui_returns_error_pair(code: &str) {
        let lua = lua_with_view(None);
        let (val, err): (Value, Option<String>) =
            smol::block_on(lua.load(code).eval_async()).unwrap();
        assert!(val.is_nil());
        assert_eq!(err.as_deref(), Some(NO_UI_ERR));
    }

    #[test]
    fn winsaveview_reports_the_viewport_one_based() {
        const SCROLL_TOP: u16 = 6;
        const LINE_COUNT: u16 = 100;
        const HEIGHT: u16 = 24;

        let (tx, rx) = flume::unbounded::<UiAction>();
        let lua = lua_with_view(Some(tx));
        std::thread::spawn(move || {
            let Ok(UiAction::WinSaveView { reply_tx }) = rx.recv() else {
                panic!("expected winsaveview request");
            };
            reply_tx
                .send(WinView {
                    scroll_top: SCROLL_TOP,
                    line_count: LINE_COUNT,
                    height: HEIGHT,
                    auto_scroll: false,
                })
                .unwrap();
        });
        let (view, err): (Table, Option<String>) =
            smol::block_on(lua.load("return f.winsaveview()").eval_async()).unwrap();
        assert_eq!(err, None);
        assert_eq!(view.get::<u16>("topline").unwrap(), SCROLL_TOP + 1);
        assert_eq!(view.get::<u16>("line_count").unwrap(), LINE_COUNT);
        assert_eq!(view.get::<u16>("height").unwrap(), HEIGHT);
        assert!(!view.get::<bool>("auto_scroll").unwrap());
    }

    #[test_case::test_case("{ topline = 12 }", 11 ; "explicit_topline")]
    #[test_case::test_case("{}", 0 ; "missing_topline_defaults_to_first_line")]
    #[test_case::test_case("{ topline = -5 }", 0 ; "below_range_clamps_to_first_line")]
    fn winrestview_forwards_zero_based_scroll_top(arg: &str, expected: u16) {
        let (tx, rx) = flume::unbounded::<UiAction>();
        let lua = lua_with_view(Some(tx));
        let (ok, err): (bool, Option<String>) = smol::block_on(
            lua.load(format!("return f.winrestview({arg})"))
                .eval_async(),
        )
        .unwrap();
        assert!(ok);
        assert_eq!(err, None);
        let Ok(UiAction::WinRestView { scroll_top }) = rx.recv() else {
            panic!("expected winrestview request");
        };
        assert_eq!(scroll_top, expected);
    }

    #[test]
    fn exit_cleanup_runs_before_a_failing_callback() {
        let lua = Lua::new();
        lua.set_app_data(JobStore::new());
        let callback = lua
            .create_function(|_, ()| Err::<(), _>(mlua::Error::runtime("callback failed")))
            .unwrap();
        let callback_key = lua.create_registry_value(callback).unwrap();
        with_jobs(&lua, |store| {
            store.jobs.insert(
                1,
                JobMeta {
                    owner: task_owner(1),
                    pid: 0,
                    on_stdout: None,
                    on_stderr: None,
                    on_error: None,
                    on_exit: Some(callback_key),
                    raw_chunks: false,
                    event_rx: None,
                },
            );
        });

        assert!(deliver_job_event(&lua, 1, &JobEvent::Exit(0)).is_err());
        assert!(with_jobs(&lua, |store| store.is_empty(&task_owner(1))));
    }

    #[test]
    fn finish_releases_callback_registry_values() {
        let lua = Lua::new();
        let capture = Arc::new(());
        let callback_capture = Arc::clone(&capture);
        let callback = lua
            .create_function(move |_, ()| {
                let _ = &callback_capture;
                Ok(())
            })
            .unwrap();
        let callback_key = lua.create_registry_value(callback).unwrap();
        let mut store = make_store();
        store.jobs.insert(
            1,
            JobMeta {
                owner: task_owner(1),
                pid: 0,
                on_stdout: Some(callback_key),
                on_stderr: None,
                on_error: None,
                on_exit: None,
                raw_chunks: false,
                event_rx: None,
            },
        );
        assert_eq!(Arc::strong_count(&capture), 2);

        store.finish(&lua, 1);
        lua.gc_collect().unwrap();

        assert_eq!(Arc::strong_count(&capture), 1);
    }
}
