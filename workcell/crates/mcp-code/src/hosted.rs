//! Hosted execution: a snippet that calls back into its host.
//!
//! Isolated execution refuses every suspension. A hosted run instead answers name lookups and
//! function calls through a [`CodeHost`], so the snippet can reach exactly the namespaces and
//! functions that host chooses to expose, and nothing else: OS calls are still refused the same way.
//!
//! Host functions are either immediate, answered before the snippet resumes, or deferred, which the
//! snippet sees as awaitables. A deferred call starts only once the snippet awaits it, so a coroutine
//! that is never awaited never reaches the host. Up to [`MAX_CONCURRENT_HOST_CALLS`] run at once,
//! and the sandbox clock stops while they do, so waiting on the host is never charged as compute.
//!
//! Variables survive between runs through an opaque [`HostedState`] the caller keeps and passes back.

use std::{
    collections::HashMap,
    future::Future,
    hash::{DefaultHasher, Hash, Hasher},
    pin::Pin,
    sync::{Arc, Mutex},
    time::Instant,
};

use monty_pool::{Checkout, PoolError, ReplConfig, ResumeValue, TurnEvent, on_print_sync};
use monty_types::{CallArgs, ExcType, MontyException, MontyObject, MontyUuid, NameLookupResult};
use serde_json::{Map, Value};
use tokio::task::{Id, JoinError, JoinSet};
use tokio_util::sync::CancellationToken;

use crate::{
    diagnose::{Surface, UNRESOLVED_REFERENCE_RULE},
    group::{
        Captured, CapturedOutput, CodeToolGroup, Completion, SCRIPT_NAME, captured_empty, execution,
    },
    render::{from_json, to_json},
    repair::{Repair, SETTLED_HELPER, looks_like_javascript, repair},
    suspend::{Answer, answer},
    types::{CodeExecution, MAX_CODE_BYTES, MAX_TIMEOUT_MS, Outcome, PreparedCode},
};

/// Deferred host calls in flight at once. Further awaits queue until one finishes.
pub const MAX_CONCURRENT_HOST_CALLS: usize = 8;
/// Deferred host calls one run may make. A call past it raises where it was made.
pub const MAX_HOST_CALLS_PER_RUN: usize = 100;
/// A saved session larger than this is dropped rather than kept.
pub const MAX_STATE_BYTES: usize = 16 * 1024 * 1024;
/// Every host call costs at least two round trips, so the isolated cap would stop a run long before
/// its call budget.
const MAX_HOSTED_SUSPENSIONS: usize = 1_024;
const CLASS_TAG: u128 = 1;
const INSTANCE_TAG: u128 = 2;
const CALL_BUDGET_MESSAGE: &str = "this run reached its limit of host calls; continue in another run, keeping what you need in variables";
const LOST_NAMESPACE_MESSAGE: &str = "this namespace is no longer available";
const UNKNOWN_CALL_MESSAGE: &str = "the host has no record of this call";
const HOST_FAILED_MESSAGE: &str = "the host failed while answering this call";
const ARGUMENT_TOO_LARGE_MESSAGE: &str = "an argument is too large to pass to the host";
const KEYWORD_NAME_MESSAGE: &str = "keyword argument names must be strings";
const HOSTED_SUSPENSION_LIMIT_MESSAGE: &str = "The snippet made too many host round trips and was stopped. Batch work into fewer calls, and check for an undefined name read inside a loop.";
/// The checker's builtins lack `PermissionError`, which a refused host call raises, so a snippet that
/// catches it would be rejected before it ran.
const JAVASCRIPT_GUIDANCE: &str = "This looks like JavaScript, and `mcp` runs Python. Rewrite it in Python: `await` each call, and use `asyncio.gather` for several at once.";
const HOSTED_STUBS_PRELUDE: &str = "class PermissionError(Exception): ...\n";
/// The checker reads the body as reachable and the interpreter skips it, so a snippet under it is
/// checked without running.
const CHECK_ONLY_GUARD: &str = "from typing import TYPE_CHECKING\nif TYPE_CHECKING:\n";
const CHECK_ONLY_GUARD_LINES: usize = 2;
const CHECK_ONLY_INDENT: &str = "    ";
/// Keeps the guarded block valid when the snippet is only comments.
const CHECK_ONLY_TAIL: &str = "pass";

pub type HostReply = Result<Value, HostError>;
/// A deferred call. It must have no effect until first polled: an unawaited call is dropped unpolled.
pub type HostFuture = Pin<Box<dyn Future<Output = HostReply> + Send>>;

/// What a name the snippet reads resolves to.
#[derive(Debug, PartialEq, Eq)]
pub enum HostName {
    /// An object whose attributes are looked up and called through the host.
    Namespace,
    /// A callable answered by [`CodeHost::call`].
    Function,
    /// Not defined. The message, when given, replaces the interpreter's own.
    Undefined(Option<String>),
}

/// One call the snippet made. `namespace` is set for a member of a [`HostName::Namespace`].
#[derive(Debug)]
pub struct HostCall {
    pub namespace: Option<String>,
    pub name: String,
    pub args: Vec<Value>,
    pub kwargs: Map<String, Value>,
}

pub enum HostInvocation {
    /// Answered before the snippet resumes, as a plain function's return value.
    Ready(HostReply),
    /// Answered later, as an awaitable.
    Deferred(HostFuture),
}

/// The Python exception a refused or failed host call raises inside the snippet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostErrorKind {
    Runtime,
    Permission,
    Type,
    Value,
    Attribute,
    Name,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostError {
    pub kind: HostErrorKind,
    pub message: String,
}

impl HostError {
    pub fn new(kind: HostErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    fn exception(&self) -> MontyException {
        let exc_type = match self.kind {
            HostErrorKind::Runtime => ExcType::RuntimeError,
            HostErrorKind::Permission => ExcType::PermissionError,
            HostErrorKind::Type => ExcType::TypeError,
            HostErrorKind::Value => ExcType::ValueError,
            HostErrorKind::Attribute => ExcType::AttributeError,
            HostErrorKind::Name => ExcType::NameError,
        };
        MontyException::new(exc_type, Some(self.message.clone()))
    }
}

/// The host side of a hosted run. It is protocol-neutral: nothing here knows what a namespace is.
pub trait CodeHost: Send + Sync {
    /// Resolves a global name (`namespace` is `None`) or a member of a namespace.
    fn lookup(&self, namespace: Option<&str>, name: &str) -> HostName;
    fn call(&self, call: HostCall) -> HostInvocation;
    /// Type stubs declaring everything the host exposes, checked against before the snippet runs.
    fn stubs(&self) -> String;
}

/// A session saved after a run, restored before the next. It lives only in memory.
#[derive(Clone)]
pub struct HostedState {
    bytes: Vec<u8>,
    namespaces: HashMap<MontyUuid, String>,
}

impl HostedState {
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

impl std::fmt::Debug for HostedState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostedState")
            .field("bytes", &self.bytes.len())
            .finish_non_exhaustive()
    }
}

/// Why a run did not carry its session forward as expected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StateNote {
    /// The saved session could not be restored, so the run started empty.
    RestoreFailed,
    /// The run ended without a session to save. The state from before it was kept.
    RunDiscarded,
    /// The session outgrew [`MAX_STATE_BYTES`] and was dropped, so the next run starts empty.
    TooLarge { bytes: usize },
}

#[derive(Debug)]
pub struct HostedExecution {
    /// `None` when the run was cancelled.
    pub execution: Option<CodeExecution>,
    pub state: Option<HostedState>,
    pub state_note: Option<StateNote>,
    /// What was changed in the snippet before it ran.
    pub repairs: Vec<Repair>,
}

impl CodeToolGroup {
    /// Validates a hosted snippet. Hosted runs have one compute budget: a restored session keeps
    /// the limits it was created with, so a per-run budget could not be honoured.
    pub fn prepare_hosted(&self, code: String) -> Result<PreparedCode, String> {
        if code.trim().is_empty() {
            return Err("Invalid arguments: code must not be empty".into());
        }
        if code.len() > MAX_CODE_BYTES {
            return Err(format!(
                "Invalid arguments: code must not exceed {MAX_CODE_BYTES} UTF-8 bytes"
            ));
        }
        Ok(PreparedCode {
            code,
            timeout_ms: MAX_TIMEOUT_MS,
            type_check: self.type_check,
        })
    }

    /// Runs a snippet against `host`, restoring `state` first and returning the session to keep.
    /// The snippet is [repaired](Repair) first where what it meant has only one reading.
    pub async fn execute_hosted(
        &self,
        prepared: PreparedCode,
        host: Arc<dyn CodeHost>,
        state: Option<HostedState>,
        cancellation: CancellationToken,
    ) -> HostedExecution {
        let stubs = host.stubs();
        let javascript = looks_like_javascript(&prepared.code);
        let repaired = repair(prepared.code, &stubs);
        let prelude = Prelude {
            imports: repaired.imports(),
            settles: repaired.settles(),
        };
        let prepared = PreparedCode {
            code: repaired.code,
            ..prepared
        };
        let mut imported = Vec::new();
        let mut executed = tokio::select! {
            biased;
            () = cancellation.cancelled() => HostedExecution {
                execution: None,
                state,
                state_note: None,
                repairs: Vec::new(),
            },
            executed = self.run_hosted(
                prepared,
                host,
                &stubs,
                &prelude,
                &mut imported,
                state.as_ref(),
            ) => executed,
        };
        if javascript
            && let Some(execution) = &mut executed.execution
            && execution.output.outcome == Outcome::Rejected
        {
            let diagnostic = execution.output.diagnostic.take().unwrap_or_default();
            execution.output.diagnostic = Some(format!("{JAVASCRIPT_GUIDANCE}\n{diagnostic}"));
        }
        executed.repairs = repaired
            .repairs
            .into_iter()
            .filter(|repair| match repair {
                Repair::Imported(module) => imported.contains(module),
                Repair::Unfenced | Repair::Unawaited(_) | Repair::FinalReturn | Repair::Settled => {
                    true
                }
            })
            .collect();
        executed
    }

    async fn run_hosted(
        &self,
        prepared: PreparedCode,
        host: Arc<dyn CodeHost>,
        stubs: &str,
        prelude: &Prelude,
        imported: &mut Vec<&'static str>,
        prior: Option<&HostedState>,
    ) -> HostedExecution {
        let PreparedCode {
            code,
            timeout_ms,
            type_check,
        } = prepared;
        let started = Instant::now();
        let stubs = type_check.then(|| format!("{HOSTED_STUBS_PRELUDE}{stubs}"));
        let repl = hosted_repl_config(type_check, timeout_ms, stubs);
        let failed =
            |error: &PoolError, captured, state: Option<HostedState>, note| HostedExecution {
                execution: Some(execution(Self::failure(
                    type_check,
                    error,
                    timeout_ms,
                    started.elapsed(),
                    Surface::Hosted,
                    captured,
                ))),
                state,
                state_note: note,
                repairs: Vec::new(),
            };

        if type_check
            && prior.is_some()
            && let Err(error) = self.check_alone(&repl, &code).await
        {
            return failed(&error, captured_empty(), prior.cloned(), None);
        }
        let (mut session, restored, note) = match self.open_hosted(&repl, prior).await {
            Ok(opened) => opened,
            Err(error) => return failed(&error, captured_empty(), prior.cloned(), None),
        };
        let base = restored.then(|| prior.cloned()).flatten();
        // A restored session was checked alone before it was restored.
        let checked = type_check && !restored;
        let mut namespaces = base
            .as_ref()
            .map(|kept| kept.namespaces.clone())
            .unwrap_or_default();
        if !prelude.is_empty() {
            let unbound = Arc::new(Unbound::default());
            // An import that fails leaves the name undefined, and the snippet reports that itself.
            let fed = drive_hosted(
                &mut session,
                &prelude.source(restored),
                !checked,
                unbound.clone(),
                &mut namespaces,
            )
            .await;
            if fed.is_ok() {
                let unbound = unbound.names();
                imported.extend(
                    prelude
                        .imports
                        .iter()
                        .filter(|module| !restored || unbound.iter().any(|name| name == *module)),
                );
            }
        }

        let driven = drive_hosted(&mut session, &code, !checked, host, &mut namespaces)
            .await
            .map_err(|error| *error);
        let duration = started.elapsed();
        let discarded = |note: Option<StateNote>, base: Option<HostedState>| {
            let note = note.or(base.is_some().then_some(StateNote::RunDiscarded));
            (base, note)
        };
        let output = match driven {
            Ok(completion) => Self::complete(type_check, completion, timeout_ms, duration),
            // Nothing ran, so the session is exactly what was restored.
            Err(HostedDriveError::Pool(error @ PoolError::Typing(_), captured)) => {
                let _ = session.finish().await;
                return failed(&error, captured, base, note);
            }
            // The snippet raised: the session is intact, including what it assigned before raising.
            Err(HostedDriveError::Pool(error @ PoolError::Runtime(_), captured)) => Self::failure(
                type_check,
                &error,
                timeout_ms,
                duration,
                Surface::Hosted,
                captured,
            ),
            Err(HostedDriveError::Pool(error, captured)) => {
                drop(session);
                let (state, note) = discarded(note, base);
                return failed(&error, captured, state, note);
            }
            Err(HostedDriveError::SuspensionLimit(captured)) => {
                drop(session);
                let mut output =
                    Self::envelope(type_check, Outcome::Limited, timeout_ms, duration, captured);
                output.suspension_limit_exceeded = true;
                output.diagnostic = Some(HOSTED_SUSPENSION_LIMIT_MESSAGE.into());
                let (state, note) = discarded(note, base);
                return HostedExecution {
                    execution: Some(execution(output)),
                    state,
                    state_note: note,
                    repairs: Vec::new(),
                };
            }
        };

        let (state, note) = match session.dump().await {
            Ok(bytes) if bytes.len() > MAX_STATE_BYTES => {
                let _ = session.finish().await;
                (None, Some(StateNote::TooLarge { bytes: bytes.len() }))
            }
            Ok(bytes) => {
                let _ = session.finish().await;
                let state = HostedState { bytes, namespaces };
                (Some(state), note)
            }
            Err(_) => {
                drop(session);
                discarded(note, base)
            }
        };
        HostedExecution {
            execution: Some(execution(output)),
            state,
            state_note: note,
            repairs: Vec::new(),
        }
    }

    /// The checker holds a restored session's earlier snippets at the literal types they bound, so
    /// it would reject reassigning them. A snippet run on a restored session is checked alone
    /// instead, against the current stubs, on a fresh session it cannot change: the names it reads
    /// from earlier runs are unknown there, so what the checker says about them is dropped and the
    /// rest stands. Anything the check could not decide is left to the run itself.
    async fn check_alone(&self, repl: &ReplConfig, code: &str) -> Result<(), PoolError> {
        let mut session = self.hosted.checkout(repl).await?;
        let mut no_print = on_print_sync(|_, _| {});
        let fed = session
            .feed(
                &check_only(code),
                Vec::new(),
                Vec::new(),
                false,
                &mut no_print,
            )
            .await;
        let standing = match fed {
            Err(PoolError::Typing(diagnostics)) => standing_diagnostics(&diagnostics),
            Ok(TurnEvent::Complete(_)) | Err(PoolError::Runtime(_)) => None,
            Ok(_) | Err(_) => return Ok(()),
        };
        let _ = session.finish().await;
        standing.map_or(Ok(()), |standing| Err(PoolError::Typing(standing)))
    }

    /// Checks out a worker holding the prior session, or a fresh one when there is none or it
    /// cannot be restored. Returns whether the prior session was restored.
    async fn open_hosted(
        &self,
        repl: &ReplConfig,
        prior: Option<&HostedState>,
    ) -> Result<(Checkout, bool, Option<StateNote>), PoolError> {
        let Some(prior) = prior else {
            return Ok((self.hosted.checkout(repl).await?, false, None));
        };
        let mut session = self.hosted.checkout(repl).await?;
        let mut no_print = on_print_sync(|_, _| {});
        // A restored session is only ever saved idle, so a suspension here means it is not ours.
        if let Ok((None, _)) = session
            .restore(prior.bytes.clone(), Vec::new(), &mut no_print)
            .await
        {
            return Ok((session, true, None));
        }
        drop(session);
        Ok((
            self.hosted.checkout(repl).await?,
            false,
            Some(StateNote::RestoreFailed),
        ))
    }
}

fn hosted_repl_config(type_check: bool, timeout_ms: u64, stubs: Option<String>) -> ReplConfig {
    let mut config = CodeToolGroup::repl_config(type_check, timeout_ms);
    config.limits = config
        .limits
        .map(|limits| limits.max_suspensions(MAX_HOSTED_SUSPENSIONS));
    config.type_check_stubs = stubs;
    config
}

/// Answers the lookups of the import turn, noting each name. On a restored session the turn looks
/// up only the modules the session lacks, which are the ones it then imports.
#[derive(Default)]
struct Unbound(Mutex<Vec<String>>);

impl Unbound {
    fn names(&self) -> Vec<String> {
        self.0.lock().map(|names| names.clone()).unwrap_or_default()
    }
}

impl CodeHost for Unbound {
    fn lookup(&self, _namespace: Option<&str>, name: &str) -> HostName {
        if let Ok(mut names) = self.0.lock() {
            names.push(name.to_owned());
        }
        HostName::Undefined(None)
    }

    fn call(&self, _call: HostCall) -> HostInvocation {
        HostInvocation::Ready(Err(HostError::new(
            HostErrorKind::Name,
            UNKNOWN_CALL_MESSAGE,
        )))
    }

    fn stubs(&self) -> String {
        String::new()
    }
}

/// What a repaired snippet needs defined before it runs, fed as a turn of its own so the snippet
/// keeps its lines.
struct Prelude {
    imports: Vec<&'static str>,
    // MONTY-GATHER-SHIM: remove with the rest of the shim; see `repair::SETTLED_HELPER`.
    settles: bool,
}

impl Prelude {
    fn is_empty(&self) -> bool {
        self.imports.is_empty() && !self.settles
    }

    /// The checker reads earlier turns as a stub file, which exports an import only under its own
    /// name written out, hence `import json as json`. A restored session may hold the name already,
    /// bound to something of its own, so there each module is imported only while its name is
    /// undefined.
    fn source(&self, restored: bool) -> String {
        let mut source: String = self
            .imports
            .iter()
            .map(|module| {
                if restored {
                    format!("try:\n    {module}\nexcept NameError:\n    import {module}\n")
                } else {
                    format!("import {module} as {module}\n")
                }
            })
            .collect();
        if self.settles {
            source.push_str(SETTLED_HELPER);
        }
        source
    }
}

fn check_only(code: &str) -> String {
    let mut guarded = String::from(CHECK_ONLY_GUARD);
    for line in code.lines().chain([CHECK_ONLY_TAIL]) {
        guarded.push_str(CHECK_ONLY_INDENT);
        guarded.push_str(line);
        guarded.push('\n');
    }
    guarded
}

/// What the checker said about a snippet checked alone, at the snippet's own lines and columns,
/// leaving out names it could not resolve. `None` when nothing else is left.
fn standing_diagnostics(diagnostics: &str) -> Option<String> {
    let standing: Vec<String> = diagnostics
        .lines()
        .filter(|line| !line.contains(UNRESOLVED_REFERENCE_RULE))
        .filter_map(at_snippet_position)
        .collect();
    (!standing.is_empty()).then(|| standing.join("\n"))
}

/// One `snippet.py:LINE:COLUMN: message` line moved back from the guarded source to the snippet.
fn at_snippet_position(line: &str) -> Option<String> {
    let rest = line.strip_prefix(SCRIPT_NAME)?.strip_prefix(':')?;
    let (row, rest) = rest.split_once(':')?;
    let (column, message) = rest.split_once(':')?;
    let row = row
        .parse::<usize>()
        .ok()?
        .saturating_sub(CHECK_ONLY_GUARD_LINES)
        .max(1);
    let column = column
        .parse::<usize>()
        .ok()?
        .saturating_sub(CHECK_ONLY_INDENT.len())
        .max(1);
    Some(format!("{SCRIPT_NAME}:{row}:{column}:{message}"))
}

fn fingerprint(stubs: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    stubs.hash(&mut hasher);
    hasher.finish()
}

/// Namespace identities derive from the name, so a reference saved in one run resolves in the next.
fn namespace_ids(name: &str) -> (MontyUuid, MontyUuid) {
    let high = u128::from(fingerprint(name)) << 64;
    (
        MontyUuid::from_u128(high | CLASS_TAG),
        MontyUuid::from_u128(high | INSTANCE_TAG),
    )
}

enum HostedDriveError {
    Pool(PoolError, CapturedOutput),
    SuspensionLimit(CapturedOutput),
}

async fn drive_hosted(
    session: &mut Checkout,
    code: &str,
    skip_type_check: bool,
    host: Arc<dyn CodeHost>,
    namespaces: &mut HashMap<MontyUuid, String>,
) -> Result<Completion, Box<HostedDriveError>> {
    let captured = Captured::new();
    let mut on_print = captured.sink();
    let mut calls = Calls::new(Arc::clone(&host));
    let mut event = session
        .feed(code, Vec::new(), Vec::new(), skip_type_check, &mut on_print)
        .await;
    for _ in 0..MAX_HOSTED_SUSPENSIONS {
        let current = match event {
            Ok(current) => current,
            Err(error) => {
                drop(on_print);
                return Err(Box::new(HostedDriveError::Pool(error, captured.take())));
            }
        };
        event = match current {
            TurnEvent::Complete(value) => {
                drop(on_print);
                return Ok(Completion {
                    value,
                    captured: captured.take(),
                });
            }
            TurnEvent::NameLookup {
                name, object_id, ..
            } => {
                let resolved = resolve(host.as_ref(), namespaces, &name, object_id);
                session.resume_name_lookup(resolved, &mut on_print).await
            }
            TurnEvent::FunctionCall {
                function_name,
                args,
                call_id,
                object_id,
                ..
            } => {
                let resume = match host_call(&function_name, object_id, &args, namespaces) {
                    Ok(call) => calls.register(call_id, call),
                    Err(error) => ResumeValue::Error(error.exception()),
                };
                session.resume(resume, &mut on_print).await
            }
            TurnEvent::ResolveFutures {
                pending_call_ids, ..
            } => {
                let results = calls.settle(&pending_call_ids).await;
                session.resume_futures(results, &mut on_print).await
            }
            ref os_call @ TurnEvent::OsCall { .. } => {
                let Answer::Resume(value) = answer(os_call) else {
                    unreachable!("OS calls are always answered with a value")
                };
                session.resume(value, &mut on_print).await
            }
        };
    }
    drop(on_print);
    Err(Box::new(HostedDriveError::SuspensionLimit(captured.take())))
}

fn resolve(
    host: &dyn CodeHost,
    namespaces: &mut HashMap<MontyUuid, String>,
    name: &str,
    object_id: Option<MontyUuid>,
) -> NameLookupResult {
    let namespace = match object_id {
        None => None,
        Some(id) => match namespaces.get(&id) {
            Some(namespace) => Some(namespace.clone()),
            None => return NameLookupResult::Undefined,
        },
    };
    match (host.lookup(namespace.as_deref(), name), namespace) {
        (HostName::Namespace, None) => {
            let (class_id, instance_id) = namespace_ids(name);
            namespaces.insert(instance_id, name.to_owned());
            let class = MontyObject::class_type(name, class_id, true, false, []);
            MontyObject::class_instance(class, instance_id, []).into()
        }
        (HostName::Namespace, Some(_)) | (HostName::Undefined(None), _) => {
            NameLookupResult::Undefined
        }
        (HostName::Function, None) => MontyObject::function(name, None).into(),
        (HostName::Function, Some(namespace)) => {
            MontyObject::function(format!("{namespace}.{name}"), None).into()
        }
        (HostName::Undefined(Some(message)), namespace) => {
            let kind = if namespace.is_some() {
                HostErrorKind::Attribute
            } else {
                HostErrorKind::Name
            };
            NameLookupResult::Error(HostError::new(kind, message).exception())
        }
    }
}

/// A method call names its receiver by identity; a function value carries a dotted name.
fn host_call(
    function_name: &str,
    object_id: Option<MontyUuid>,
    args: &CallArgs,
    namespaces: &HashMap<MontyUuid, String>,
) -> Result<HostCall, HostError> {
    let (namespace, name) = match object_id {
        Some(id) => {
            let namespace = namespaces
                .get(&id)
                .ok_or_else(|| HostError::new(HostErrorKind::Attribute, LOST_NAMESPACE_MESSAGE))?;
            (Some(namespace.clone()), function_name.to_owned())
        }
        None => match function_name.split_once('.') {
            Some((namespace, name)) => (Some(namespace.to_owned()), name.to_owned()),
            None => (None, function_name.to_owned()),
        },
    };
    let too_large = || HostError::new(HostErrorKind::Value, ARGUMENT_TOO_LARGE_MESSAGE);
    let positional = args
        .args()
        .map(|arg| to_json(&arg.to_owned()).ok_or_else(too_large))
        .collect::<Result<_, _>>()?;
    let mut kwargs = Map::new();
    for (key, value) in args.kwargs() {
        let key = key
            .as_str()
            .ok_or_else(|| HostError::new(HostErrorKind::Type, KEYWORD_NAME_MESSAGE))?;
        kwargs.insert(
            key.to_owned(),
            to_json(&value.to_owned()).ok_or_else(too_large)?,
        );
    }
    Ok(HostCall {
        namespace,
        name,
        args: positional,
        kwargs,
    })
}

fn resume_reply(reply: HostReply) -> ResumeValue {
    match reply {
        Ok(value) => ResumeValue::Return(from_json(&value)),
        Err(error) => ResumeValue::Error(error.exception()),
    }
}

/// Deferred calls of one run: registered when made, started when awaited.
struct Calls {
    host: Arc<dyn CodeHost>,
    waiting: HashMap<u32, HostFuture>,
    running: JoinSet<(u32, HostReply)>,
    running_ids: HashMap<Id, u32>,
    made: usize,
}

impl Calls {
    fn new(host: Arc<dyn CodeHost>) -> Self {
        Self {
            host,
            waiting: HashMap::new(),
            running: JoinSet::new(),
            running_ids: HashMap::new(),
            made: 0,
        }
    }

    fn register(&mut self, call_id: u32, call: HostCall) -> ResumeValue {
        match self.host.call(call) {
            HostInvocation::Ready(reply) => resume_reply(reply),
            HostInvocation::Deferred(_) if self.made >= MAX_HOST_CALLS_PER_RUN => {
                ResumeValue::Error(
                    HostError::new(HostErrorKind::Runtime, CALL_BUDGET_MESSAGE).exception(),
                )
            }
            HostInvocation::Deferred(future) => {
                self.made += 1;
                self.waiting.insert(call_id, future);
                ResumeValue::Future
            }
        }
    }

    /// Starts every awaited call there is room for, then returns whatever has finished, waiting
    /// for at least one result when none is ready.
    async fn settle(&mut self, pending: &[u32]) -> Vec<(u32, ResumeValue)> {
        let mut results = Vec::new();
        for &call_id in pending {
            if self.running_ids.values().any(|running| *running == call_id) {
                continue;
            }
            match self.waiting.remove(&call_id) {
                Some(future) if self.running.len() < MAX_CONCURRENT_HOST_CALLS => {
                    let handle = self.running.spawn(async move { (call_id, future.await) });
                    self.running_ids.insert(handle.id(), call_id);
                }
                Some(future) => {
                    self.waiting.insert(call_id, future);
                }
                None => results.push((
                    call_id,
                    ResumeValue::Error(
                        HostError::new(HostErrorKind::Runtime, UNKNOWN_CALL_MESSAGE).exception(),
                    ),
                )),
            }
        }
        if results.is_empty()
            && let Some(joined) = self.running.join_next_with_id().await
        {
            results.push(self.finished(joined));
        }
        while let Some(joined) = self.running.try_join_next_with_id() {
            results.push(self.finished(joined));
        }
        results
    }

    fn finished(
        &mut self,
        joined: Result<(Id, (u32, HostReply)), JoinError>,
    ) -> (u32, ResumeValue) {
        match joined {
            Ok((id, (call_id, reply))) => {
                self.running_ids.remove(&id);
                (call_id, resume_reply(reply))
            }
            Err(error) => {
                let call_id = self.running_ids.remove(&error.id()).unwrap_or_default();
                (
                    call_id,
                    ResumeValue::Error(
                        HostError::new(HostErrorKind::Runtime, HOST_FAILED_MESSAGE).exception(),
                    ),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const MISSING_TOOL: &str =
        "error[unresolved-attribute] Object of type `_Gh` has no attribute `nope`";
    const EARLIER_NAME: &str = "error[unresolved-reference] Name `kept` used when not defined";

    /// The guard adds two lines above the snippet and indents it, so a position the checker gives is
    /// moved back by both before the caller reads it.
    #[test_case(&[(3, 11, MISSING_TOOL)], Some(&[(1, 7, MISSING_TOOL)]) ; "moved_back_to_the_snippet")]
    #[test_case(&[(4, 5, EARLIER_NAME)], None ; "a_name_from_an_earlier_run_is_dropped")]
    #[test_case(&[(3, 5, EARLIER_NAME), (4, 11, MISSING_TOOL)], Some(&[(2, 7, MISSING_TOOL)]) ; "the_rest_stands")]
    fn a_snippet_checked_alone_reports_at_its_own_positions(
        reported: &[(usize, usize, &str)],
        expected: Option<&[(usize, usize, &str)]>,
    ) {
        let render = |lines: &[(usize, usize, &str)]| {
            lines
                .iter()
                .map(|(row, column, message)| format!("{SCRIPT_NAME}:{row}:{column}: {message}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert_eq!(
            standing_diagnostics(&render(reported)),
            expected.map(render)
        );
    }

    struct NoHost;

    impl CodeHost for NoHost {
        fn lookup(&self, namespace: Option<&str>, name: &str) -> HostName {
            match (namespace, name) {
                (None, "github") => HostName::Namespace,
                (Some("github"), "list") | (None, "search") => HostName::Function,
                (Some(_), _) => HostName::Undefined(Some(format!("no tool `{name}`"))),
                (None, _) => HostName::Undefined(None),
            }
        }

        fn call(&self, _: HostCall) -> HostInvocation {
            HostInvocation::Ready(Ok(Value::Null))
        }

        fn stubs(&self) -> String {
            String::new()
        }
    }

    #[test]
    fn namespace_identity_is_stable_and_distinct_per_name() {
        assert_eq!(namespace_ids("github"), namespace_ids("github"));
        assert_ne!(namespace_ids("github").1, namespace_ids("gitlab").1);
        let (class_id, instance_id) = namespace_ids("github");
        assert_ne!(class_id, instance_id);
    }

    #[test]
    fn a_namespace_is_remembered_so_its_members_resolve() {
        let mut namespaces = HashMap::new();
        let resolved = resolve(&NoHost, &mut namespaces, "github", None);
        assert!(matches!(resolved, NameLookupResult::Value(_)));
        let (_, instance_id) = namespace_ids("github");
        assert_eq!(
            namespaces.get(&instance_id).map(String::as_str),
            Some("github")
        );

        let member = resolve(&NoHost, &mut namespaces, "list", Some(instance_id));
        assert!(matches!(member, NameLookupResult::Value(_)));
        let missing = resolve(&NoHost, &mut namespaces, "nope", Some(instance_id));
        let NameLookupResult::Error(error) = missing else {
            panic!("an unknown member raises the host's message");
        };
        assert_eq!(error.exc_type(), ExcType::AttributeError);
    }

    #[test]
    fn an_unknown_global_stays_undefined() {
        let mut namespaces = HashMap::new();
        assert!(matches!(
            resolve(&NoHost, &mut namespaces, "requests", None),
            NameLookupResult::Undefined
        ));
        assert!(namespaces.is_empty());
    }

    #[test]
    fn calls_route_by_receiver_or_dotted_name() {
        let mut namespaces = HashMap::new();
        let (_, instance_id) = namespace_ids("github");
        namespaces.insert(instance_id, "github".to_owned());
        let mut args = CallArgs::new();
        args.push_kwarg("owner", MontyObject::string("o"));

        let method = host_call("list", Some(instance_id), &args, &namespaces).expect("call");
        let value = host_call("github.list", None, &args, &namespaces).expect("call");
        let global = host_call("search", None, &args, &namespaces).expect("call");
        for call in [&method, &value] {
            assert_eq!(call.namespace.as_deref(), Some("github"));
            assert_eq!(call.name, "list");
            assert_eq!(call.kwargs.get("owner"), Some(&Value::String("o".into())));
        }
        assert_eq!(global.namespace, None);
        assert_eq!(global.name, "search");

        let lost = host_call("list", Some(namespace_ids("gone").1), &args, &namespaces);
        assert_eq!(lost.map(|_| ()).unwrap_err().kind, HostErrorKind::Attribute);
    }

    #[tokio::test]
    async fn calls_past_the_budget_raise_where_they_are_made() {
        struct Deferring;
        impl CodeHost for Deferring {
            fn lookup(&self, _: Option<&str>, _: &str) -> HostName {
                HostName::Function
            }
            fn call(&self, _: HostCall) -> HostInvocation {
                HostInvocation::Deferred(Box::pin(async { Ok(Value::Null) }))
            }
            fn stubs(&self) -> String {
                String::new()
            }
        }
        let mut calls = Calls::new(Arc::new(Deferring));
        let call = || HostCall {
            namespace: None,
            name: "f".into(),
            args: Vec::new(),
            kwargs: Map::new(),
        };
        for call_id in 0..MAX_HOST_CALLS_PER_RUN {
            let id = u32::try_from(call_id).expect("small");
            assert!(matches!(calls.register(id, call()), ResumeValue::Future));
        }
        assert!(matches!(
            calls.register(u32::MAX, call()),
            ResumeValue::Error(_)
        ));
    }

    #[tokio::test]
    async fn settling_caps_concurrency_and_reports_unknown_calls() {
        struct Pending;
        impl CodeHost for Pending {
            fn lookup(&self, _: Option<&str>, _: &str) -> HostName {
                HostName::Function
            }
            fn call(&self, _: HostCall) -> HostInvocation {
                HostInvocation::Deferred(Box::pin(async { Ok(Value::Bool(true)) }))
            }
            fn stubs(&self) -> String {
                String::new()
            }
        }
        let mut calls = Calls::new(Arc::new(Pending));
        let ids: Vec<u32> = (0..12).collect();
        for &id in &ids {
            calls.register(
                id,
                HostCall {
                    namespace: None,
                    name: "f".into(),
                    args: Vec::new(),
                    kwargs: Map::new(),
                },
            );
        }
        let mut remaining = ids.clone();
        while !remaining.is_empty() {
            let round = calls.settle(&remaining).await;
            assert!(!round.is_empty());
            assert!(calls.running.len() <= MAX_CONCURRENT_HOST_CALLS);
            remaining.retain(|id| !round.iter().any(|(settled, _)| settled == id));
        }

        let unknown = calls.settle(&[99]).await;
        assert!(matches!(unknown.as_slice(), [(99, ResumeValue::Error(_))]));
    }
}
