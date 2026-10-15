//! The boundary between the gateway and whatever executes a snippet.
//!
//! The agent defines it and an embedder implements it: Caudra's runs the snippet in Workcell's
//! hosted Monty worker. The script language is the runner's business; the gateway only answers
//! the [`ScriptHost`] questions every script language asks: what a name is, and what a call
//! returns.

use std::{
    any::Any,
    fmt,
    future::Future,
    pin::Pin,
    sync::{Arc, OnceLock},
};

use async_lock::{Mutex as AsyncMutex, MutexGuardArc};
use serde_json::{Map, Value};

use crate::cancel::CancelToken;

pub type ScriptReply = Result<Value, ScriptError>;
/// A deferred call. It must do nothing until first polled: a call the snippet never awaits is
/// dropped unpolled.
pub type ScriptFuture = Pin<Box<dyn Future<Output = ScriptReply> + Send>>;
pub type RunFuture = Pin<Box<dyn Future<Output = ScriptOutcome> + Send>>;

static INSTALLED: OnceLock<Arc<dyn ScriptRunner>> = OnceLock::new();

/// What a name the snippet reads resolves to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScriptName {
    /// An object whose members are looked up and called through the host.
    Namespace,
    Function,
    /// Not defined. The message, when given, replaces the interpreter's own.
    Undefined(Option<String>),
}

/// One call the snippet made. `namespace` is set for a member of a [`ScriptName::Namespace`].
#[derive(Clone, Debug, PartialEq)]
pub struct ScriptCall {
    pub namespace: Option<String>,
    pub name: String,
    pub args: Vec<Value>,
    pub kwargs: Map<String, Value>,
}

pub enum ScriptInvocation {
    /// Answered before the snippet resumes, as a plain function's value.
    Ready(ScriptReply),
    /// Answered later, as an awaitable.
    Deferred(ScriptFuture),
}

/// The exception a refused or failed call raises inside the snippet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScriptErrorKind {
    /// A tool reported an error, a server is unreachable, or a result is too large.
    Runtime,
    /// The call was denied, refused by policy, or blocked by the session's mode.
    Permission,
    /// Arguments do not fit the signature.
    Type,
    Value,
    Attribute,
    Name,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScriptError {
    pub kind: ScriptErrorKind,
    pub message: String,
}

impl ScriptError {
    pub fn new(kind: ScriptErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

/// The host side of one run.
pub trait ScriptHost: Send + Sync {
    /// Resolves a global name (`namespace` is `None`) or a member of a namespace.
    fn lookup(&self, namespace: Option<&str>, name: &str) -> ScriptName;
    fn call(&self, call: ScriptCall) -> ScriptInvocation;
    /// Type stubs for everything the host exposes, checked before the snippet runs.
    fn stubs(&self) -> String;
}

/// A saved interpreter session. Only the runner that produced it can read it.
pub struct ReplState {
    bytes: usize,
    inner: Box<dyn Any + Send + Sync>,
}

impl ReplState {
    pub fn new(bytes: usize, inner: impl Any + Send + Sync) -> Self {
        Self {
            bytes,
            inner: Box::new(inner),
        }
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn into_inner<T: Any>(self) -> Option<T> {
        self.inner.downcast::<T>().ok().map(|inner| *inner)
    }
}

/// The variables one agent's `mcp` runs keep, held only in memory.
pub struct ReplSlot {
    state: Option<ReplState>,
    /// Earlier runs kept variables in a process that has since gone, which the next run says.
    restarted: bool,
}

pub type SharedRepl = Arc<AsyncMutex<ReplSlot>>;

impl ReplSlot {
    pub fn shared(ran_before: bool) -> SharedRepl {
        Arc::new(AsyncMutex::new(Self {
            state: None,
            restarted: ran_before,
        }))
    }
}

/// One run's hold on its agent's variables. The run puts its session back before letting go, so
/// one cancelled or dropped part-way still hands its variables on, and the next run waits until
/// the last one has really stopped.
pub struct ReplLease(MutexGuardArc<ReplSlot>);

impl ReplLease {
    pub async fn acquire(slot: &SharedRepl) -> Self {
        Self(slot.lock_arc().await)
    }

    pub fn take(&mut self) -> Option<ReplState> {
        self.0.state.take()
    }

    pub fn keep(&mut self, state: Option<ReplState>) {
        self.0.state = state;
    }

    /// Whether earlier runs' variables died with an earlier process, said once.
    pub(super) fn take_restarted(&mut self) -> bool {
        std::mem::take(&mut self.0.restarted)
    }
}

impl fmt::Debug for ReplState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReplState")
            .field("bytes", &self.bytes)
            .finish_non_exhaustive()
    }
}

/// Why a run did not carry its session forward as expected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplNote {
    /// The saved session could not be restored, so the run started empty.
    RestoreFailed,
    /// The run ended without a session to save; the variables from before it were kept.
    RunDiscarded,
    /// The session outgrew the cap and was dropped, so the next run starts empty.
    TooLarge { bytes: usize },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScriptStatus {
    Completed,
    /// The snippet raised.
    Exception,
    /// The snippet never ran: type checking or the parser refused it.
    Rejected,
    /// A time, memory, or round-trip budget stopped it.
    Limited,
    /// The runner could not service the run.
    Unavailable,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScriptException {
    pub kind: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScriptOutput {
    pub status: ScriptStatus,
    pub result: Value,
    /// Python's own rendering, present when `result` loses type or precision.
    pub result_repr: Option<String>,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub exception: Option<ScriptException>,
    /// Guidance for the next attempt, when the failure has a known cause.
    pub diagnostic: Option<String>,
    pub duration_ms: u64,
    pub timed_out: bool,
}

#[derive(Debug, Default)]
pub struct ScriptOutcome {
    /// `None` when the run was cancelled.
    pub output: Option<ScriptOutput>,
    pub note: Option<ReplNote>,
    /// What the runner changed in the snippet before running it.
    pub repairs: Vec<ScriptRepair>,
}

/// One change made to a snippet before it ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptRepair {
    /// What was changed, in a few words.
    pub done: String,
    /// What the model should write next time, when the change undid a slip of its own.
    pub lesson: Option<String>,
}

pub struct ScriptRun {
    pub code: String,
    pub host: Arc<dyn ScriptHost>,
    pub state: ReplLease,
    pub cancel: CancelToken,
}

pub trait ScriptRunner: Send + Sync {
    /// Runs one snippet. However the run ends, even dropped, the lease gets the session to keep
    /// before it is released, so a run that never started (invalid input, no worker) leaves the
    /// state it was given.
    fn run(&self, run: ScriptRun) -> RunFuture;

    /// Why no snippet can run, when that is already known before one is tried.
    fn unavailable(&self) -> Option<String> {
        None
    }
}

impl ScriptOutput {
    /// The output of a run that never started.
    pub fn refused(status: ScriptStatus, diagnostic: impl Into<String>) -> Self {
        Self {
            status,
            result: Value::Null,
            result_repr: None,
            stdout: String::new(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            exception: None,
            diagnostic: Some(diagnostic.into()),
            duration_ms: 0,
            timed_out: false,
        }
    }
}

/// Makes `runner` the process's runner. The first installation wins; Caudra installs one at
/// startup, whether its Workcell is embedded or remote, because MCP servers always run locally.
pub fn install(runner: Arc<dyn ScriptRunner>) {
    let _ = INSTALLED.set(runner);
}

pub fn installed() -> Option<Arc<dyn ScriptRunner>> {
    INSTALLED.get().cloned()
}

/// Why the installed runner cannot run snippets, as far as it knows without trying one.
pub fn unavailable() -> Option<String> {
    installed().and_then(|runner| runner.unavailable())
}
