use std::cell::{Cell, OnceCell, RefCell};
use std::num::NonZeroU64;
use std::rc::Rc;
use std::time::Instant;

use caudra_script::{
    BridgeClosed, HostBridge, HostKind, InterpreterError, SandboxLimits, restricted_engine,
    run_interpreter,
};
use rhai::{Array, Dynamic, Engine, EvalAltResult, Map, Position, Scope};
use serde::Serialize;
use serde_json::Value;

use crate::host::{
    AgentRequest, DecisionRequest, HostError, UnknownCapabilityMode, UnknownModelJob, WorkflowHost,
};
use crate::journal::{
    CallKey, CallKind, Journal, agent_request_value, decision_request_value, hash_request,
    scratch_request_value,
};
use crate::run::{EngineLimits, PauseKind, WorkflowOutcome};

const INTERPRETER_THREAD_NAME: &str = "caudra-workflow";
/// Operations between deadline and cancellation polls; each poll is one host round trip.
const PROGRESS_POLL_OPS: NonZeroU64 = NonZeroU64::new(16_384).unwrap();
const UNAVAILABLE_IN: &str = "workflow scripts; end a run with complete() or pause()";
const ARGS_VARIABLE: &str = "args";

const OPT_PROMPT: &str = "prompt";
const OPT_LABEL: &str = "label";
const OPT_CAPABILITY_MODE: &str = "capability_mode";
const OPT_OUTPUT_SCHEMA: &str = "output_schema";
const OPT_PHASE: &str = "phase";
const OPT_PROFILE: &str = "profile";
const OPT_MODEL_JOB: &str = "model_job";
const OPT_MODEL: &str = "model";
const OPT_TIMEOUT_MS: &str = "timeout_ms";
const AGENT_OPTIONS: [&str; 7] = [
    OPT_PROMPT,
    OPT_LABEL,
    OPT_CAPABILITY_MODE,
    OPT_OUTPUT_SCHEMA,
    OPT_PHASE,
    OPT_PROFILE,
    OPT_MODEL_JOB,
];

const BUDGET_ISSUED: &str = "issued";
const BUDGET_LIMIT: &str = "limit";
const BUDGET_REMAINING: &str = "remaining";

const HOST_GONE: &str = "workflow host stopped servicing calls";
const FOREIGN_TERMINATION: &str = "workflow was terminated by an unknown token";
const EMPTY_PROMPT: &str = "agent prompt must be a non-empty string";
const PROMPT_CONFLICT: &str = "agent option `prompt` conflicts with the positional prompt";
const PARALLEL_ITEM_TYPE: &str = "parallel() items must be option maps with a `prompt`";
const SCHEMA_TYPE: &str = "agent option `output_schema` must be a map";
const PARALLEL_INCOMPLETE: &str = "parallel() left a request without a result";
const KEY_OVERFLOW: &str = "workflow host-call count overflowed";

pub struct RunParams<'a> {
    pub source: &'a str,
    pub args: &'a Value,
    pub journal: &'a Journal,
    pub host: &'a dyn WorkflowHost,
    pub limits: &'a EngineLimits,
    pub agent_budget: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EngineError {
    #[error("source is {bytes} bytes; the limit is {max}")]
    SourceTooLarge { bytes: usize, max: usize },
    #[error("script failed to compile: {0}")]
    Compile(String),
}

pub trait WorkflowEngine {
    fn compile(&self, source: &str) -> Result<(), EngineError>;

    /// Runs `source` to a terminal outcome. Host calls are replayed from the journal while it
    /// covers the next call key; `phase` and `log` stay silent during that catch-up so history is
    /// not emitted twice. The interpreter runs on its own thread; host methods run on the
    /// caller's thread.
    fn run(&self, params: RunParams<'_>) -> WorkflowOutcome;
}

pub struct RhaiEngine;

impl WorkflowEngine for RhaiEngine {
    fn compile(&self, source: &str) -> Result<(), EngineError> {
        let limits = EngineLimits::default();
        check_source_size(source, limits.max_source_bytes)?;
        restricted_engine(&SandboxLimits::from(&limits), UNAVAILABLE_IN)
            .compile(source)
            .map(drop)
            .map_err(|error| EngineError::Compile(error.to_string()))
    }

    fn run(&self, params: RunParams<'_>) -> WorkflowOutcome {
        let RunParams {
            source,
            args,
            journal,
            host,
            limits,
            agent_budget,
        } = params;
        run_interpreter(INTERPRETER_THREAD_NAME, host, move |bridge| {
            evaluate(source, args, journal, limits, agent_budget, bridge)
        })
        .unwrap_or_else(|error| {
            WorkflowOutcome::Failed(match error {
                InterpreterError::Spawn(error) => {
                    format!("could not start the workflow interpreter: {error}")
                }
                InterpreterError::Panicked(detail) => {
                    format!("workflow interpreter panicked: {detail}")
                }
            })
        })
    }
}

type ScriptResult<T> = Result<T, Box<EvalAltResult>>;

/// Host calls borrow the run's `dyn WorkflowHost` for as long as the run lends it.
struct WorkflowHosts;

impl HostKind for WorkflowHosts {
    type Host<'h> = dyn WorkflowHost + 'h;
}

/// Ends the run, in `EvalAltResult::ErrorTerminated`. A closure called by an array method hands
/// even that to `catch`, so the session latches the first one and every later host call and
/// operation raises it again.
#[derive(Clone)]
enum Terminal {
    Complete(Value),
    Pause { kind: PauseKind, message: String },
    Cancelled,
    BudgetLimited,
    TooManyOperations,
    Fatal(String),
}

impl Terminal {
    /// `position` is where the run stopped, if an error carried one out of the script. Only the
    /// operations limit reports it, so its failure reads as Rhai's own error does.
    fn into_outcome(self, position: Position) -> WorkflowOutcome {
        match self {
            Self::Complete(value) => WorkflowOutcome::Completed(value),
            Self::Pause { kind, message } => WorkflowOutcome::Paused { kind, message },
            Self::Cancelled => WorkflowOutcome::Cancelled,
            Self::BudgetLimited => WorkflowOutcome::BudgetLimited,
            Self::TooManyOperations => {
                WorkflowOutcome::Failed(EvalAltResult::ErrorTooManyOperations(position).to_string())
            }
            Self::Fatal(error) => WorkflowOutcome::Failed(error),
        }
    }
}

impl From<Terminal> for Box<EvalAltResult> {
    fn from(terminal: Terminal) -> Self {
        Box::new(EvalAltResult::ErrorTerminated(
            Dynamic::from(terminal),
            Position::NONE,
        ))
    }
}

impl From<BridgeClosed> for Terminal {
    fn from(_: BridgeClosed) -> Self {
        Self::Fatal(HOST_GONE.into())
    }
}

fn fatal(message: impl Into<String>) -> Box<EvalAltResult> {
    Terminal::Fatal(message.into()).into()
}

fn runtime_error(message: impl Into<String>) -> Box<EvalAltResult> {
    Box::new(EvalAltResult::ErrorRuntime(
        Dynamic::from(message.into()),
        Position::NONE,
    ))
}

fn host_result<T>(reply: Result<Result<T, HostError>, BridgeClosed>) -> ScriptResult<T> {
    match reply.map_err(Terminal::from)? {
        Ok(value) => Ok(value),
        Err(HostError::Cancelled) => Err(Terminal::Cancelled.into()),
        Err(HostError::BudgetExhausted) => Err(Terminal::BudgetLimited.into()),
        Err(error @ (HostError::Failed(_) | HostError::Scratch(_))) => {
            Err(runtime_error(error.to_string()))
        }
    }
}

struct RunState {
    next_key: CallKey,
    log_entries: u64,
    log_bytes: usize,
    agents_issued: u64,
}

struct Session {
    host: HostBridge<WorkflowHosts>,
    journal: Journal,
    limits: EngineLimits,
    agent_budget: u32,
    latched: OnceCell<Terminal>,
    operations: Cell<u64>,
    state: RefCell<RunState>,
}

impl Session {
    /// Wraps every host function that can reach the host or end the run: a latched terminal is
    /// raised again before the call does anything, and the first terminal a call raises is latched.
    fn guard<T>(&self, call: impl FnOnce() -> ScriptResult<T>) -> ScriptResult<T> {
        if let Some(terminal) = self.latched.get() {
            return Err(terminal.clone().into());
        }
        call().inspect_err(|error| {
            if let EvalAltResult::ErrorTerminated(token, _) = &**error
                && let Some(terminal) = token.read_lock::<Terminal>()
            {
                self.latch(terminal.clone());
            }
        })
    }

    /// Called for every operation, so it counts the run's total: Rhai counts a closure's
    /// operations on a copy of its state that the closure's return discards. A latched terminal
    /// is raised again at once, the operations limit is latched at its last operation, before
    /// Rhai's own error could be caught in a closure, and the deadline and host cancellation are
    /// checked every [`PROGRESS_POLL_OPS`].
    fn progress(
        &self,
        max_operations: Option<NonZeroU64>,
        deadline: Option<Instant>,
    ) -> Option<Terminal> {
        let operations = self.operations.get() + 1;
        self.operations.set(operations);
        if let Some(terminal) = self.latched.get() {
            return Some(terminal.clone());
        }
        let terminal = if max_operations.is_some_and(|max| operations >= max.get()) {
            Terminal::TooManyOperations
        } else if !operations.is_multiple_of(PROGRESS_POLL_OPS.get()) {
            return None;
        } else if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            Terminal::Fatal(format!(
                "workflow exceeded its wall-time limit of {:?}",
                self.limits.wall_time
            ))
        } else {
            match self.host.call(|host| host.is_cancelled()) {
                Ok(false) => return None,
                Ok(true) => Terminal::Cancelled,
                Err(closed) => closed.into(),
            }
        };
        Some(self.latch(terminal).clone())
    }

    /// The first terminal wins: a later one only raises it again.
    fn latch(&self, terminal: Terminal) -> &Terminal {
        self.latched.get_or_init(|| terminal)
    }

    /// A latched terminal ends the run however evaluation ended after the script caught it.
    fn finish(&self, result: ScriptResult<()>) -> WorkflowOutcome {
        let position = result
            .as_ref()
            .err()
            .map_or(Position::NONE, |error| error.position());
        match (self.latched.get(), result) {
            (Some(terminal), _) => terminal.clone().into_outcome(position),
            (None, Ok(())) => WorkflowOutcome::Completed(Value::Null),
            (None, Err(error)) => outcome_from_error(*error),
        }
    }

    fn reserve_keys(&self, count: usize) -> ScriptResult<CallKey> {
        let count = u64::try_from(count).map_err(|_| fatal(KEY_OVERFLOW))?;
        let mut state = self.state.borrow_mut();
        let first = state.next_key;
        let end = first.offset(count).ok_or_else(|| fatal(KEY_OVERFLOW))?;
        if end.0 - 1 > self.limits.max_host_calls {
            return Err(fatal(format!(
                "workflow exceeded its limit of {} host calls",
                self.limits.max_host_calls
            )));
        }
        state.next_key = end;
        Ok(first)
    }

    fn replaying(&self) -> bool {
        self.journal.covers(self.state.borrow().next_key)
    }

    fn replayed(
        &self,
        key: CallKey,
        kind: CallKind,
        request: &Value,
    ) -> ScriptResult<Option<Value>> {
        self.journal
            .replay(key, kind, &hash_request(kind, request))
            .map(|value| value.cloned())
            .map_err(|error| fatal(error.to_string()))
    }

    fn emit(
        &self,
        bytes: usize,
        job: impl FnOnce(&dyn WorkflowHost) + Send + 'static,
    ) -> ScriptResult<()> {
        {
            let mut state = self.state.borrow_mut();
            state.log_entries += 1;
            state.log_bytes = state.log_bytes.saturating_add(bytes);
            if state.log_entries > self.limits.max_log_entries
                || state.log_bytes > self.limits.max_log_bytes
            {
                return Err(fatal(format!(
                    "workflow exceeded its log limits ({} entries, {} bytes)",
                    self.limits.max_log_entries, self.limits.max_log_bytes
                )));
            }
        }
        if self.replaying() {
            return Ok(());
        }
        Ok(self.host.call(job).map_err(Terminal::from)?)
    }

    /// What the script has spent and what it has left, as a map the script reads
    /// to decide how wide to fan out or whether it can still afford to verify.
    /// Counted from what the script asked for rather than from what the host
    /// admitted, because a replay re-issues every call without dispatching one.
    fn budget(&self) -> Map {
        let issued = self.state.borrow().agents_issued;
        let limit = u64::from(self.agent_budget);
        let mut map = Map::new();
        map.insert(BUDGET_ISSUED.into(), Dynamic::from(issued as i64));
        map.insert(BUDGET_LIMIT.into(), Dynamic::from(limit as i64));
        map.insert(
            BUDGET_REMAINING.into(),
            Dynamic::from(limit.saturating_sub(issued) as i64),
        );
        map
    }

    fn agent(&self, request: AgentRequest) -> ScriptResult<Dynamic> {
        let key = self.reserve_keys(1)?;
        self.state.borrow_mut().agents_issued += 1;
        let result = match self.replayed(key, CallKind::Agent, &agent_request_value(&request))? {
            Some(result) => result,
            None => json_value(&host_result(
                self.host.call(move |host| host.agent(key, &request)),
            )?),
        };
        json_to_dynamic(&result)
    }

    fn decide(&self, request: DecisionRequest) -> ScriptResult<Dynamic> {
        request
            .validate()
            .map_err(|error| runtime_error(error.to_string()))?;
        let key = self.reserve_keys(1)?;
        let result =
            match self.replayed(key, CallKind::Decision, &decision_request_value(&request))? {
                Some(result) => result,
                None => json_value(&host_result(
                    self.host.call(move |host| host.decide(key, &request)),
                )?),
            };
        json_to_dynamic(&result)
    }

    /// Journaled items are replayed; the rest are sent to the host in contiguous key runs, so a
    /// run that died mid-`parallel` only re-issues the items that never got committed.
    fn parallel(&self, requests: Vec<AgentRequest>) -> ScriptResult<Array> {
        let first = self.reserve_keys(requests.len())?;
        self.state.borrow_mut().agents_issued += requests.len() as u64;
        let mut results = Vec::with_capacity(requests.len());
        for (index, request) in requests.iter().enumerate() {
            let key = key_at(first, index)?;
            results.push(self.replayed(key, CallKind::Parallel, &agent_request_value(request))?);
        }
        let mut index = 0;
        while index < requests.len() {
            if results[index].is_some() {
                index += 1;
                continue;
            }
            let start = index;
            while index < requests.len() && results[index].is_none() {
                index += 1;
            }
            let batch = requests[start..index].to_vec();
            let first_key = key_at(first, start)?;
            let batch_results =
                host_result(self.host.call(move |host| host.parallel(first_key, &batch)))?;
            if batch_results.len() != index - start {
                return Err(fatal(format!(
                    "host returned {} results for {} parallel requests",
                    batch_results.len(),
                    index - start
                )));
            }
            for (offset, result) in batch_results.iter().enumerate() {
                results[start + offset] = Some(json_value(result));
            }
        }
        results
            .into_iter()
            .map(|result| {
                result
                    .ok_or_else(|| fatal(PARALLEL_INCOMPLETE))
                    .and_then(|value| json_to_dynamic(&value))
            })
            .collect()
    }

    fn write_scratch_file(&self, name: String, content: String) -> ScriptResult<String> {
        let key = self.reserve_keys(1)?;
        match self.replayed(
            key,
            CallKind::ScratchFile,
            &scratch_request_value(&name, &content),
        )? {
            Some(Value::String(path)) => Ok(path),
            Some(other) => Err(fatal(format!(
                "journal holds a non-string scratch path for call {key}: {other}"
            ))),
            None => host_result(
                self.host
                    .call(move |host| host.write_scratch_file(key, &name, &content)),
            ),
        }
    }

    fn complete(&self, value: Value) -> Box<EvalAltResult> {
        let bytes = value.to_string().len();
        if bytes > self.limits.max_output_bytes {
            return fatal(format!(
                "complete() value is {bytes} bytes; the limit is {}",
                self.limits.max_output_bytes
            ));
        }
        Terminal::Complete(value).into()
    }
}

fn key_at(first: CallKey, index: usize) -> ScriptResult<CallKey> {
    u64::try_from(index)
        .ok()
        .and_then(|offset| first.offset(offset))
        .ok_or_else(|| fatal(KEY_OVERFLOW))
}

fn json_value<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("host results are plain JSON data")
}

fn json_to_dynamic(value: &Value) -> ScriptResult<Dynamic> {
    rhai::serde::to_dynamic(value)
        .map_err(|error| fatal(format!("host result could not be converted: {error}")))
}

fn dynamic_to_json(value: &Dynamic) -> ScriptResult<Value> {
    rhai::serde::from_dynamic::<Value>(value)
        .map_err(|error| runtime_error(format!("value is not JSON-compatible: {error}")))
}

fn string_option(key: &str, value: Dynamic) -> ScriptResult<String> {
    value.into_string().map_err(|actual| {
        runtime_error(format!(
            "agent option `{key}` must be a string, got {actual}"
        ))
    })
}

fn agent_request(positional_prompt: Option<&str>, options: Map) -> ScriptResult<AgentRequest> {
    let mut request = AgentRequest::new(positional_prompt.unwrap_or_default());
    for (key, value) in options {
        match key.as_str() {
            OPT_PROMPT if positional_prompt.is_none() => {
                request.prompt = string_option(&key, value)?;
            }
            OPT_PROMPT => return Err(runtime_error(PROMPT_CONFLICT)),
            OPT_LABEL => request.label = Some(string_option(&key, value)?),
            OPT_CAPABILITY_MODE => {
                request.capability_mode = string_option(&key, value)?
                    .parse()
                    .map_err(|error: UnknownCapabilityMode| runtime_error(error.to_string()))?;
            }
            OPT_OUTPUT_SCHEMA => {
                let schema = dynamic_to_json(&value)?;
                if !schema.is_object() {
                    return Err(runtime_error(SCHEMA_TYPE));
                }
                request.output_schema = Some(schema);
            }
            OPT_PHASE => request.phase = Some(string_option(&key, value)?),
            OPT_PROFILE => request.profile = Some(string_option(&key, value)?),
            OPT_MODEL_JOB => {
                request.model_job = Some(
                    string_option(&key, value)?
                        .parse()
                        .map_err(|error: UnknownModelJob| runtime_error(error.to_string()))?,
                );
            }
            other => {
                return Err(runtime_error(format!(
                    "unknown agent option `{other}`; expected one of {}",
                    AGENT_OPTIONS.join(", ")
                )));
            }
        }
    }
    if request.prompt.trim().is_empty() {
        return Err(runtime_error(EMPTY_PROMPT));
    }
    Ok(request)
}

fn decision_request(
    state: Dynamic,
    questions: Dynamic,
    options: Map,
) -> ScriptResult<DecisionRequest> {
    let mut request = DecisionRequest {
        state: dynamic_to_json(&state)?,
        questions: dynamic_to_json(&questions)?,
        model: None,
        timeout_ms: None,
    };
    for (key, value) in options {
        match key.as_str() {
            OPT_MODEL => {
                request.model = Some(
                    value
                        .into_string()
                        .map_err(|_| runtime_error("decision model must be a string"))?,
                )
            }
            OPT_TIMEOUT_MS => {
                request.timeout_ms = Some(
                    value
                        .as_int()
                        .ok()
                        .and_then(|value| u64::try_from(value).ok())
                        .filter(|value| *value > 0)
                        .ok_or_else(|| {
                            runtime_error("decision timeout_ms must be a positive integer")
                        })?,
                );
            }
            other => {
                return Err(runtime_error(format!(
                    "unknown decision option `{other}`; expected model or timeout_ms"
                )));
            }
        }
    }
    Ok(request)
}

pub(crate) fn check_source_size(source: &str, max: usize) -> Result<(), EngineError> {
    if source.len() > max {
        return Err(EngineError::SourceTooLarge {
            bytes: source.len(),
            max,
        });
    }
    Ok(())
}

fn register_host_api(engine: &mut Engine, session: &Rc<Session>) {
    let s = Rc::clone(session);
    engine.register_fn(
        "decide",
        move |state: Dynamic, questions: Dynamic| -> ScriptResult<Dynamic> {
            s.guard(|| s.decide(decision_request(state, questions, Map::new())?))
        },
    );
    let s = Rc::clone(session);
    engine.register_fn(
        "decide",
        move |state: Dynamic, questions: Dynamic, options: Map| -> ScriptResult<Dynamic> {
            s.guard(|| s.decide(decision_request(state, questions, options)?))
        },
    );
    let s = Rc::clone(session);
    engine.register_fn("agent", move |prompt: &str| -> ScriptResult<Dynamic> {
        s.guard(|| s.agent(agent_request(Some(prompt), Map::new())?))
    });
    let s = Rc::clone(session);
    engine.register_fn(
        "agent",
        move |prompt: &str, options: Map| -> ScriptResult<Dynamic> {
            s.guard(|| s.agent(agent_request(Some(prompt), options)?))
        },
    );
    let s = Rc::clone(session);
    engine.register_fn("parallel", move |items: Array| -> ScriptResult<Array> {
        s.guard(|| {
            let requests = items
                .into_iter()
                .map(|item| {
                    item.try_cast::<Map>()
                        .ok_or_else(|| runtime_error(PARALLEL_ITEM_TYPE))
                        .and_then(|options| agent_request(None, options))
                })
                .collect::<ScriptResult<Vec<AgentRequest>>>()?;
            s.parallel(requests)
        })
    });
    let s = Rc::clone(session);
    engine.register_fn("phase", move |title: &str| -> ScriptResult<()> {
        let title = title.to_owned();
        s.guard(|| s.emit(title.len(), move |host| host.phase(&title)))
    });
    let s = Rc::clone(session);
    engine.register_fn("log", move |message: &str| -> ScriptResult<()> {
        let message = message.to_owned();
        s.guard(|| s.emit(message.len(), move |host| host.log(&message)))
    });
    let s = Rc::clone(session);
    engine.register_fn(
        "write_scratch_file",
        move |name: &str, content: &str| -> ScriptResult<String> {
            s.guard(|| s.write_scratch_file(name.to_owned(), content.to_owned()))
        },
    );
    let s = Rc::clone(session);
    engine.register_fn("complete", move |value: Dynamic| -> ScriptResult<()> {
        s.guard(|| Err(s.complete(dynamic_to_json(&value)?)))
    });
    let s = Rc::clone(session);
    engine.register_fn("complete", move || -> ScriptResult<()> {
        s.guard(|| Err(s.complete(Value::Null)))
    });
    let s = Rc::clone(session);
    engine.register_fn(
        "pause",
        move |kind: &str, message: &str| -> ScriptResult<()> {
            s.guard(|| {
                let kind =
                    PauseKind::new(kind).map_err(|error| runtime_error(error.to_string()))?;
                Err(Terminal::Pause {
                    kind,
                    message: message.to_owned(),
                }
                .into())
            })
        },
    );
    let s = Rc::clone(session);
    engine.register_fn("budget", move || -> Map { s.budget() });
    engine.register_fn("json_encode", |value: Dynamic| -> ScriptResult<String> {
        serde_json::to_string(&dynamic_to_json(&value)?)
            .map_err(|error| runtime_error(format!("json_encode failed: {error}")))
    });
}

fn evaluate(
    source: &str,
    args: &Value,
    journal: &Journal,
    limits: &EngineLimits,
    agent_budget: u32,
    host: HostBridge<WorkflowHosts>,
) -> WorkflowOutcome {
    if let Err(error) = check_source_size(source, limits.max_source_bytes) {
        return WorkflowOutcome::Failed(error.to_string());
    }
    let args_bytes = args.to_string().len();
    if args_bytes > limits.max_args_bytes {
        return WorkflowOutcome::Failed(format!(
            "args are {args_bytes} bytes; the limit is {}",
            limits.max_args_bytes
        ));
    }
    let args = match rhai::serde::to_dynamic(args) {
        Ok(args) => args,
        Err(error) => return WorkflowOutcome::Failed(format!("invalid workflow args: {error}")),
    };
    let session = Rc::new(Session {
        host,
        journal: journal.clone(),
        limits: limits.clone(),
        agent_budget,
        latched: OnceCell::new(),
        operations: Cell::default(),
        state: RefCell::new(RunState {
            next_key: CallKey::FIRST,
            log_entries: 0,
            log_bytes: 0,
            agents_issued: 0,
        }),
    });
    let mut engine = restricted_engine(&SandboxLimits::from(limits), UNAVAILABLE_IN);
    register_host_api(&mut engine, &session);
    let max_operations = NonZeroU64::new(engine.max_operations());
    let deadline = Instant::now().checked_add(limits.wall_time);
    let progress_session = Rc::clone(&session);
    engine.on_progress(move |_| {
        progress_session
            .progress(max_operations, deadline)
            .map(Dynamic::from)
    });
    let ast = match engine.compile(source) {
        Ok(ast) => ast,
        Err(error) => {
            return WorkflowOutcome::Failed(EngineError::Compile(error.to_string()).to_string());
        }
    };
    let mut scope = Scope::new();
    scope.push_dynamic(ARGS_VARIABLE, args);
    session.finish(engine.run_ast_with_scope(&mut scope, &ast))
}

fn outcome_from_error(error: EvalAltResult) -> WorkflowOutcome {
    match error {
        EvalAltResult::ErrorTerminated(token, position) => {
            token.try_cast::<Terminal>().map_or_else(
                || WorkflowOutcome::Failed(FOREIGN_TERMINATION.into()),
                |terminal| terminal.into_outcome(position),
            )
        }
        other => WorkflowOutcome::Failed(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, PoisonError};
    use std::time::Duration;

    use serde_json::json;
    use test_case::test_case;

    use super::*;
    use crate::host::{AgentResult, CapabilityMode, DecisionResult, ModelJob};
    use crate::journal::JournalEntry;
    use crate::{DEEP_RESEARCH_SOURCE, REVIEW_CHANGES_SOURCE, ROOT_CAUSE_SOURCE};

    const META: &str = r#"let meta = #{ name: "t", description: "d" };"#;
    const SCRATCH_ROOT: &str = "/scratch";
    const PHASE_PREFIX: &str = "phase:";
    const LOG_PREFIX: &str = "log:";
    const TEST_AGENT_BUDGET: u32 = 16;
    const MUST_NOT_CALL: &str = "must not be called";
    const DECISION_MODEL: &str = "test-decision-model";
    const DECISION_BODY: &str = r#"
        let questions = #{ ready: #{ type: "noul", instructions: "Ready?" } };
        complete(decide(args, questions));"#;
    const DECISION_WITH_OPTIONS: &str = r#"
        let questions = #{ ready: #{ type: "noul", instructions: "Ready?" } };
        complete(decide(args, questions, #{ model: "test-decision-model", timeout_ms: 400 }));"#;
    const HOST_CALL_LIMIT: &str = "host calls";
    const CAUGHT: &str = "caught";
    const AFTER_CATCH: &str = "after the catch";
    const HOST_AFTER_TERMINAL: &str = "a caught terminal must stop every later host call";
    const TEST_MAX_OPERATIONS: u64 = 10_000;
    const TOO_MANY_OPERATIONS: &str = "Too many operations";
    const REPLAY_DIVERGENCE: &str = "diverged";
    const BUDGET_COUNTS_ISSUED: &str =
        "budget() counts every agent the script asked for, one per parallel item";
    const BUDGET_SURVIVES_REPLAY: &str =
        "a branch on budget() must read the same on a replay, which re-issues without dispatching";
    const BUDGET_BODY: &str = r#"
        agent("one");
        parallel([#{ prompt: "a" }, #{ prompt: "b" }]);
        let b = budget();
        complete([b.issued, b.limit, b.remaining]);"#;
    const BUDGET_BRANCH_BODY: &str = r#"
        parallel([#{ prompt: "a" }, #{ prompt: "b" }]);
        let wide = budget().remaining > 15;
        agent(if wide { "wide" } else { "narrow" });
        complete(wide);"#;
    const SURVEYOR: &str = "change-surveyor";
    const REVIEWER_PREFIX: &str = "reviewer-";
    const REFUTER_PREFIX: &str = "refuter-";
    const REVIEW_WRITER: &str = "review-writer";
    const SECURITY_DIMENSION: &str = "security";
    const BLOCKER_SEVERITY: &str = "blocker";
    const DIMENSION_COUNT: usize = 5;
    const FINDINGS_OPEN: &str = "<findings-json>\n";
    const FINDINGS_CLOSE: &str = "\n</findings-json>";
    const CHANGE_SUMMARY: &str = "It rewrote the parser.";
    const REVIEW_BODY: &str = "### blocker: src/security.rs:1\n\nThe parser trusts its input.";
    const UNCHALLENGED_NOTE: &str = "unchallenged claim";
    const RANKED_BY_SEVERITY: &str = "surviving findings are ordered blocker first";
    const REFUTED_ARE_DROPPED: &str = "a finding a refuter ruled refuted never reaches the report";
    const UNCHALLENGED_ARE_KEPT: &str =
        "a budget too small to refute keeps every finding and says they went unchallenged";
    const UNREADABLE_SUMMARY: &str = "git reported no change to review.";
    const UNREVIEWED_STATUS: &str = "unreviewed";
    const NOTHING_READ_DISPATCHES_NOBODY: &str =
        "a surveyor that could read nothing must stop the run before any reviewer is paid for";
    const NOTHING_READ_IS_NOT_A_PASS: &str =
        "a run that reviewed nothing must not read as a change that passed review";
    const CLEAN_VERDICT: &str = "**Verdict: no findings**";
    const GAPS_VERDICT: &str = "**Verdict: reviewed with gaps**";
    const PARTIAL_VERDICT_MARKER: &str = "(partial review)";
    const PARTIAL_EVIDENCE_MARKER: &str = "(partial evidence)";
    const PARTIAL_IS_SAID_UP_FRONT: &str =
        "a review with a hole in its coverage must say so in the verdict, not only in a footnote";
    const EVIDENCE_PREFIX: &str = "evidence-";
    const HYPOTHESIS_PREFIX: &str = "hypothesis-";
    const DIAGNOSIS_WRITER: &str = "diagnosis-writer";
    const REFUTED_STANCE: &str = "the-boundary";
    const STRAND_COUNT: usize = 4;
    const STANCE_COUNT: usize = 3;
    const CAUSES_OPEN: &str = "<causes-json>\n";
    const CAUSES_CLOSE: &str = "\n</causes-json>";
    const DIAGNOSIS_BODY: &str = "The config loader reads the key before the file is parsed.";
    const UNREFUTED_NOTE: &str = "not whether it survives scrutiny";
    const RANKED_BY_CONVERGENCE: &str = "the cause the most refuters upheld is ranked first";
    const REFUTED_STANCE_IS_DROPPED: &str =
        "a cause every refuter ruled out never reaches the report";
    const UNREFUTED_ARE_KEPT: &str =
        "a budget too small to refute keeps every cause and says none was challenged";
    const PLANNER: &str = "research-planner";
    const RESEARCHER_PREFIX: &str = "researcher-";
    const VERIFIER_PREFIX: &str = "evidence-verifier-";
    const SYNTHESIZER: &str = "report-synthesizer";
    const PACKET_OPEN: &str = "<candidate-claims-json>\n";
    const PACKET_CLOSE: &str = "\n</candidate-claims-json>";
    const REPORT_BODY: &str =
        "<report-body>Answer [S1] [S2].\n\n### Detail\nMore [S3] [S4].</report-body>";
    const SOURCES_HEADING: &str = "## Sources";
    const REPORT_FILE: &str = "report.md";

    type Responder =
        Box<dyn Fn(CallKey, &AgentRequest) -> Result<AgentResult, HostError> + Send + Sync>;

    struct FakeHost {
        respond: Responder,
        committed: Mutex<Vec<(CallKey, JournalEntry)>>,
        requests: Mutex<Vec<(CallKey, AgentRequest)>>,
        decisions: Mutex<Vec<(CallKey, DecisionRequest)>>,
        decision_error: Option<HostError>,
        emissions: Mutex<Vec<String>>,
        scratch: Mutex<Vec<(String, String)>>,
        cancelled: bool,
    }

    fn agent_result(agent_id: String, success: bool, output: Value) -> AgentResult {
        AgentResult {
            agent_id,
            success,
            output,
            cancelled: false,
            tokens_used: 10,
            duration_ms: 5,
        }
    }

    impl FakeHost {
        fn new(respond: Responder) -> Self {
            Self {
                respond,
                committed: Mutex::default(),
                requests: Mutex::default(),
                decisions: Mutex::default(),
                decision_error: None,
                emissions: Mutex::default(),
                scratch: Mutex::default(),
                cancelled: false,
            }
        }

        fn echo() -> Self {
            Self::new(Box::new(|key, request| {
                Ok(agent_result(
                    format!("agent-{key}"),
                    true,
                    Value::String(request.prompt.clone()),
                ))
            }))
        }

        fn failing(error: HostError) -> Self {
            Self::new(Box::new(move |_, _| Err(error.clone())))
        }

        fn record(
            &self,
            key: CallKey,
            kind: CallKind,
            request: &AgentRequest,
        ) -> Result<AgentResult, HostError> {
            self.requests.lock().unwrap().push((key, request.clone()));
            let result = (self.respond)(key, request)?;
            self.committed
                .lock()
                .unwrap()
                .push((key, JournalEntry::agent(kind, request, &result)));
            Ok(result)
        }

        fn journal(&self) -> Journal {
            let mut journal = Journal::new();
            for (key, entry) in self.committed.lock().unwrap().iter() {
                journal.insert(*key, entry.clone()).expect("unique keys");
            }
            journal
        }

        fn requests(&self) -> Vec<(CallKey, AgentRequest)> {
            self.requests.lock().unwrap().clone()
        }

        fn emissions(&self) -> Vec<String> {
            self.emissions.lock().unwrap().clone()
        }

        fn phases(&self) -> Vec<String> {
            self.emissions()
                .into_iter()
                .filter_map(|emission| emission.strip_prefix(PHASE_PREFIX).map(str::to_owned))
                .collect()
        }

        fn scratch(&self) -> Vec<(String, String)> {
            self.scratch.lock().unwrap().clone()
        }
    }

    impl WorkflowHost for FakeHost {
        fn decide(
            &self,
            key: CallKey,
            request: &DecisionRequest,
        ) -> Result<DecisionResult, HostError> {
            self.decisions.lock().unwrap().push((key, request.clone()));
            if let Some(error) = &self.decision_error {
                return Err(error.clone());
            }
            let result = DecisionResult {
                answers: json!({ "ready": { "type": "noul", "noul": 0.75 } }),
                model: request
                    .model
                    .clone()
                    .unwrap_or_else(|| DECISION_MODEL.into()),
            };
            self.committed
                .lock()
                .unwrap()
                .push((key, JournalEntry::decision(request, &result)));
            Ok(result)
        }

        fn agent(&self, key: CallKey, request: &AgentRequest) -> Result<AgentResult, HostError> {
            self.record(key, CallKind::Agent, request)
        }

        fn parallel(
            &self,
            first_key: CallKey,
            requests: &[AgentRequest],
        ) -> Result<Vec<AgentResult>, HostError> {
            let mut results = vec![None; requests.len()];
            for (index, request) in requests.iter().enumerate().rev() {
                let key = first_key.offset(index as u64).expect("key fits");
                results[index] = Some(self.record(key, CallKind::Parallel, request)?);
            }
            Ok(results.into_iter().flatten().collect())
        }

        fn phase(&self, title: &str) {
            self.emissions
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(format!("{PHASE_PREFIX}{title}"));
        }

        fn log(&self, message: &str) {
            self.emissions
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(format!("{LOG_PREFIX}{message}"));
        }

        fn write_scratch_file(
            &self,
            key: CallKey,
            name: &str,
            content: &str,
        ) -> Result<String, HostError> {
            let path = format!("{SCRATCH_ROOT}/{name}");
            self.scratch
                .lock()
                .unwrap()
                .push((name.to_owned(), content.to_owned()));
            self.committed
                .lock()
                .unwrap()
                .push((key, JournalEntry::scratch_file(name, content, &path)));
            Ok(path)
        }

        fn is_cancelled(&self) -> bool {
            self.cancelled
        }
    }

    fn script(body: &str) -> String {
        format!("{META}\n{body}")
    }

    fn run_with(
        body: &str,
        args: &Value,
        journal: &Journal,
        host: &FakeHost,
        limits: &EngineLimits,
    ) -> WorkflowOutcome {
        RhaiEngine.run(RunParams {
            source: &script(body),
            args,
            journal,
            host,
            limits,
            agent_budget: TEST_AGENT_BUDGET,
        })
    }

    fn run(body: &str, host: &FakeHost) -> WorkflowOutcome {
        run_with(
            body,
            &json!({ "objective": "test" }),
            &Journal::new(),
            host,
            &EngineLimits::default(),
        )
    }

    fn failure_message(outcome: WorkflowOutcome) -> String {
        match outcome {
            WorkflowOutcome::Failed(message) => message,
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test_case(DECISION_BODY, None, None; "defaults")]
    #[test_case(DECISION_WITH_OPTIONS, Some(DECISION_MODEL), Some(400); "options")]
    fn decision_options_reach_host(body: &str, model: Option<&str>, timeout_ms: Option<u64>) {
        let host = FakeHost::echo();
        assert_eq!(
            run(body, &host),
            WorkflowOutcome::Completed(json!({
                "answers": { "ready": { "type": "noul", "noul": 0.75 } }, "model": DECISION_MODEL,
            }))
        );
        let decisions = host.decisions.lock().unwrap();
        assert_eq!(decisions.len(), 1);
        let (key, request) = &decisions[0];
        assert_eq!(*key, CallKey::FIRST);
        assert_eq!(request.state, json!({ "objective": "test" }));
        assert_eq!(request.model.as_deref(), model);
        assert_eq!(request.timeout_ms, timeout_ms);
        assert!(host.requests().is_empty());
    }

    #[test_case("#{ unexpected: true }"; "unknown")]
    #[test_case("#{ model: 1 }"; "model_type")]
    #[test_case("#{ model: \" \" }"; "model_empty")]
    #[test_case("#{ timeout_ms: 0 }"; "timeout_zero")]
    #[test_case("#{ timeout_ms: -1 }"; "timeout_negative")]
    #[test_case("#{ timeout_ms: 1.5 }"; "timeout_float")]
    #[test_case("#{ timeout_ms: \"400\" }"; "timeout_string")]
    fn invalid_decision_options_are_catchable_without_host_call(options: &str) {
        let host = FakeHost::echo();
        let body = format!(
            r#"
            let questions = #{{ ready: #{{ type: "noul", instructions: "Ready?" }} }};
            try {{ decide(args, questions, {options}); }} catch (error) {{ complete(true); }}
            complete(false);"#
        );
        assert_eq!(run(&body, &host), WorkflowOutcome::Completed(json!(true)));
        assert!(host.decisions.lock().unwrap().is_empty());
    }

    #[test_case(json!({ "state": null, "questions": { "q": { "type": "noul", "instructions": "Ready?" } } }); "missing_state")]
    #[test_case(json!({ "state": "state", "questions": [] }); "wrong_questions_type")]
    #[test_case(json!({ "state": "state", "questions": { "q": { "type": "choice", "instructions": "Route?", "criteria": [] } } }); "invalid_criteria")]
    fn invalid_decision_requests_do_not_reach_host(args: Value) {
        let host = FakeHost::echo();
        let body = r#"try { decide(args.state, args.questions); } catch (error) { complete(true); } complete(false);"#;
        assert_eq!(
            run_with(
                body,
                &args,
                &Journal::new(),
                &host,
                &EngineLimits::default()
            ),
            WorkflowOutcome::Completed(json!(true))
        );
        assert!(host.decisions.lock().unwrap().is_empty());
    }

    #[test]
    fn decision_replay_uses_committed_result_without_host_dispatch() {
        let first_host = FakeHost::echo();
        let first = run(DECISION_WITH_OPTIONS, &first_host);
        let replay_host = FakeHost {
            decision_error: Some(HostError::Failed(MUST_NOT_CALL.into())),
            ..FakeHost::echo()
        };
        let replay = run_with(
            DECISION_WITH_OPTIONS,
            &json!({ "objective": "test" }),
            &first_host.journal(),
            &replay_host,
            &EngineLimits::default(),
        );
        assert_eq!(replay, first);
        assert!(replay_host.decisions.lock().unwrap().is_empty());
    }

    #[test_case("args", "questions", "#{ model: \"changed\", timeout_ms: 400 }"; "model")]
    #[test_case("args", "questions", "#{ model: \"test-decision-model\", timeout_ms: 401 }"; "timeout")]
    #[test_case("\"changed\"", "questions", "#{ model: \"test-decision-model\", timeout_ms: 400 }"; "state")]
    #[test_case("args", "#{ changed: #{ type: \"noul\", instructions: \"Different?\" } }", "#{ model: \"test-decision-model\", timeout_ms: 400 }"; "questions")]
    fn changed_decision_request_refuses_replay(state: &str, questions: &str, options: &str) {
        let host = FakeHost::echo();
        run(DECISION_WITH_OPTIONS, &host);
        let body = format!(
            r#"let questions = #{{ ready: #{{ type: "noul", instructions: "Ready?" }} }}; complete(decide({state}, {questions}, {options}));"#
        );
        let replay_host = FakeHost::echo();
        let message = failure_message(run_with(
            &body,
            &json!({ "objective": "test" }),
            &host.journal(),
            &replay_host,
            &EngineLimits::default(),
        ));
        assert!(message.contains(REPLAY_DIVERGENCE), "{message}");
        assert!(replay_host.decisions.lock().unwrap().is_empty());
    }

    #[test]
    fn decisions_share_keys_and_host_budget_but_not_agent_budget() {
        let host = FakeHost::echo();
        let body = r#"
            let questions = #{ ready: #{ type: "noul", instructions: "Ready?" } };
            decide(args, questions);
            agent("one");
            decide(args, questions);
            complete(budget().issued);"#;
        let limits = EngineLimits {
            max_host_calls: 3,
            ..EngineLimits::default()
        };
        assert_eq!(
            run_with(body, &json!("state"), &Journal::new(), &host, &limits),
            WorkflowOutcome::Completed(json!(1))
        );
        assert_eq!(
            host.journal()
                .iter()
                .map(|(key, entry)| (key, entry.kind))
                .collect::<Vec<_>>(),
            vec![
                (CallKey(1), CallKind::Decision),
                (CallKey(2), CallKind::Agent),
                (CallKey(3), CallKind::Decision),
            ]
        );
        let limited_host = FakeHost::echo();
        let limits = EngineLimits {
            max_host_calls: 2,
            ..limits
        };
        let message = failure_message(run_with(
            body,
            &json!("state"),
            &Journal::new(),
            &limited_host,
            &limits,
        ));
        assert!(message.contains(HOST_CALL_LIMIT), "{message}");
        assert_eq!(limited_host.decisions.lock().unwrap().len(), 1);
        let resumed_host = FakeHost::echo();
        assert_eq!(
            run_with(
                body,
                &json!("state"),
                &limited_host.journal(),
                &resumed_host,
                &EngineLimits::default()
            ),
            WorkflowOutcome::Completed(json!(1))
        );
        assert_eq!(resumed_host.decisions.lock().unwrap()[0].0, CallKey(3));
        assert!(resumed_host.requests().is_empty());
    }

    #[test]
    fn decision_host_failures_are_catchable() {
        let host = FakeHost {
            decision_error: Some(HostError::Failed(MUST_NOT_CALL.into())),
            ..FakeHost::echo()
        };
        let body = r#"try { decide(args, #{ q: #{ type: "noul", instructions: "Ready?" } }); } catch (error) { complete(error); }"#;
        let WorkflowOutcome::Completed(Value::String(message)) = run(body, &host) else {
            panic!("expected caught host error");
        };
        assert!(message.contains(MUST_NOT_CALL), "{message}");
    }

    #[test_case(r#"import "x";"#; "import")]
    #[test_case(r#"eval("1");"#; "eval")]
    #[test_case("print(1);"; "print")]
    #[test_case("debug(1);"; "debug")]
    fn forbidden_symbols_fail_to_compile(body: &str) {
        assert!(matches!(
            RhaiEngine.compile(&script(body)),
            Err(EngineError::Compile(_))
        ));
    }

    #[test_case("timestamp();", "timestamp"; "timestamp")]
    #[test_case("sleep(1);", "sleep"; "sleep_int")]
    #[test_case("sleep(0.5);", "sleep"; "sleep_float")]
    #[test_case("exit();", "exit"; "exit")]
    #[test_case("exit(1);", "exit"; "exit_value")]
    fn unavailable_functions_fail_at_runtime(body: &str, name: &str) {
        let message = failure_message(run(body, &FakeHost::echo()));
        assert!(message.contains(name), "{message}");
    }

    #[test]
    fn max_operations_fails_an_infinite_loop() {
        let limits = EngineLimits {
            max_operations: 10_000,
            ..EngineLimits::default()
        };
        let outcome = run_with(
            "while true {}",
            &Value::Null,
            &Journal::new(),
            &FakeHost::echo(),
            &limits,
        );
        assert!(failure_message(outcome).contains("operations"));
    }

    #[test_case("while true {}"; "at_top_level")]
    #[test_case("fn spin() { loop {} } spin();"; "in_a_script_function")]
    fn an_uncaught_operations_overrun_reads_as_rhais_own_error(body: &str) {
        let limits = EngineLimits {
            max_operations: TEST_MAX_OPERATIONS,
            ..EngineLimits::default()
        };
        let rhai_error = restricted_engine(&SandboxLimits::from(&limits), UNAVAILABLE_IN)
            .run(&script(body))
            .expect_err("Rhai stops a runaway loop at its operations limit");
        assert_eq!(
            run_with(
                body,
                &Value::Null,
                &Journal::new(),
                &FakeHost::echo(),
                &limits
            ),
            WorkflowOutcome::Failed(rhai_error.to_string())
        );
    }

    #[test]
    fn wall_time_deadline_fails_deterministically() {
        let limits = EngineLimits {
            wall_time: Duration::ZERO,
            ..EngineLimits::default()
        };
        let outcome = run_with(
            "while true {}",
            &Value::Null,
            &Journal::new(),
            &FakeHost::echo(),
            &limits,
        );
        assert!(failure_message(outcome).contains("wall-time"));
    }

    #[test]
    fn cancelled_host_stops_a_compute_loop() {
        let host = FakeHost {
            cancelled: true,
            ..FakeHost::echo()
        };
        assert_eq!(run("while true {}", &host), WorkflowOutcome::Cancelled);
    }

    #[test_case(r#"try { complete(#{ ok: true }); } catch (e) { complete("caught"); }"#; "direct")]
    #[test_case(r#"fn finish() { complete(#{ ok: true }); } try { finish(); } catch (e) { complete("caught"); }"#; "through_function")]
    fn complete_inside_try_catch_still_completes(body: &str) {
        assert_eq!(
            run(body, &FakeHost::echo()),
            WorkflowOutcome::Completed(json!({ "ok": true }))
        );
    }

    #[test]
    fn pause_inside_try_catch_still_pauses() {
        let outcome = run(
            r#"try { pause("user", "wait"); } catch (e) { complete("caught"); }"#,
            &FakeHost::echo(),
        );
        assert_eq!(
            outcome,
            WorkflowOutcome::Paused {
                kind: PauseKind::new("user").expect("valid kind"),
                message: "wait".into(),
            }
        );
    }

    #[test]
    fn falling_off_the_end_completes_with_null() {
        assert_eq!(
            run("let x = 1; x", &FakeHost::echo()),
            WorkflowOutcome::Completed(Value::Null)
        );
    }

    #[test]
    fn null_args_are_unit() {
        let outcome = run_with(
            r#"if args == () { complete("unit"); } complete("map");"#,
            &Value::Null,
            &Journal::new(),
            &FakeHost::echo(),
            &EngineLimits::default(),
        );
        assert_eq!(outcome, WorkflowOutcome::Completed(json!("unit")));
    }

    #[test_case(r#"agent("p", #{ bogus: 1 })"#, "bogus"; "unknown_option")]
    #[test_case(r#"agent("p", #{ label: 1 })"#, "label"; "non_string_option")]
    #[test_case(r#"agent("p", #{ capability_mode: "nonsense" })"#, "nonsense"; "unknown_mode")]
    #[test_case(r#"agent("p", #{ output_schema: "x" })"#, "output_schema"; "schema_not_map")]
    #[test_case(r#"agent("p", #{ prompt: "q" })"#, "prompt"; "prompt_conflict")]
    #[test_case(r#"agent("   ")"#, "prompt"; "blank_prompt")]
    #[test_case(r#"parallel([#{ label: "x" }])"#, "prompt"; "parallel_item_without_prompt")]
    #[test_case(r#"parallel(["x"])"#, "option maps"; "parallel_item_not_map")]
    fn option_errors_are_script_visible(call: &str, needle: &str) {
        let host = FakeHost::echo();
        let outcome = run(
            &format!(r#"try {{ {call}; }} catch (e) {{ complete(e); }} complete("unreached");"#),
            &host,
        );
        match outcome {
            WorkflowOutcome::Completed(Value::String(message)) => {
                assert!(message.contains(needle), "{message}");
            }
            other => panic!("expected caught error, got {other:?}"),
        }
        assert!(host.requests().is_empty());
    }

    #[test]
    fn agent_options_reach_the_host() {
        let host = FakeHost::echo();
        let outcome = run(
            r#"let r = agent("do it", #{
                label: "worker", capability_mode: "execute", phase: "P", profile: "fast",
                model_job: "best", output_schema: #{ "type": "object" },
            });
            complete(r.output);"#,
            &host,
        );
        assert_eq!(outcome, WorkflowOutcome::Completed(json!("do it")));
        let requests = host.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].0, CallKey::FIRST);
        assert_eq!(
            requests[0].1,
            AgentRequest {
                prompt: "do it".into(),
                label: Some("worker".into()),
                capability_mode: CapabilityMode::Build,
                output_schema: Some(json!({ "type": "object" })),
                phase: Some("P".into()),
                profile: Some("fast".into()),
                model_job: Some(ModelJob::Best),
            }
        );
    }

    #[test]
    fn host_failure_is_catchable() {
        let host = FakeHost::failing(HostError::Failed("boom".into()));
        let outcome = run(
            r#"try { agent("p"); } catch (e) { complete("caught: " + e); }"#,
            &host,
        );
        assert_eq!(
            outcome,
            WorkflowOutcome::Completed(json!("caught: host failure: boom"))
        );
    }

    #[test_case(HostError::Cancelled => WorkflowOutcome::Cancelled; "cancelled")]
    #[test_case(HostError::BudgetExhausted => WorkflowOutcome::BudgetLimited; "budget")]
    fn terminal_host_errors_are_uncatchable(error: HostError) -> WorkflowOutcome {
        run(
            r#"try { agent("p"); } catch (e) { complete("caught"); }"#,
            &FakeHost::failing(error),
        )
    }

    /// Rhai wraps whatever a closure called by an array method raises in `ErrorInFunctionCall`,
    /// which `catch` intercepts even when it carries a terminal.
    #[test_case("[1].map(|x| complete(true))", None => WorkflowOutcome::Completed(json!(true)); "complete_in_map")]
    #[test_case("[1].find(|x| complete(true))", None => WorkflowOutcome::Completed(json!(true)); "complete_in_find")]
    #[test_case(r#"[1].map(|x| pause("user", "wait"))"#, None => matches WorkflowOutcome::Paused { .. }; "pause_in_map")]
    #[test_case(r#"[1].filter(|x| pause("user", "wait"))"#, None => matches WorkflowOutcome::Paused { .. }; "pause_in_filter")]
    #[test_case(r#"[1].map(|x| agent("p"))"#, Some(HostError::Cancelled) => WorkflowOutcome::Cancelled; "cancelled_in_map")]
    #[test_case(r#"[1].filter(|x| agent("p"))"#, Some(HostError::Cancelled) => WorkflowOutcome::Cancelled; "cancelled_in_filter")]
    #[test_case(r#"[1].find(|x| agent("p"))"#, Some(HostError::BudgetExhausted) => WorkflowOutcome::BudgetLimited; "budget_in_find")]
    fn a_terminal_caught_from_a_closure_still_ends_the_run(
        closure: &str,
        failure: Option<HostError>,
    ) -> WorkflowOutcome {
        let host = failure.map_or_else(FakeHost::echo, FakeHost::failing);
        let outcome = run(
            &format!(
                r#"try {{ {closure}; }} catch (e) {{ agent("{AFTER_CATCH}"); complete("{CAUGHT}"); }}"#
            ),
            &host,
        );
        assert!(
            host.requests()
                .iter()
                .all(|(_, request)| request.prompt != AFTER_CATCH),
            "{HOST_AFTER_TERMINAL}"
        );
        outcome
    }

    #[test_case("[1].filter(|x| complete(true))", None => WorkflowOutcome::Completed(json!(true)); "complete_in_filter")]
    #[test_case(r#"[1].find(|x| pause("user", "wait"))"#, None => matches WorkflowOutcome::Paused { .. }; "pause_in_find")]
    #[test_case(r#"[1].find(|x| agent("p"))"#, Some(HostError::Cancelled) => WorkflowOutcome::Cancelled; "cancelled_in_find")]
    #[test_case(r#"[1].map(|x| agent("p"))"#, Some(HostError::BudgetExhausted) => WorkflowOutcome::BudgetLimited; "budget_in_map")]
    fn a_terminal_swallowed_from_a_closure_still_ends_the_run(
        closure: &str,
        failure: Option<HostError>,
    ) -> WorkflowOutcome {
        run(
            &format!("try {{ {closure}; }} catch {{}}"),
            &failure.map_or_else(FakeHost::echo, FakeHost::failing),
        )
    }

    #[test]
    fn a_cancellation_polled_inside_a_closure_still_cancels() {
        let host = FakeHost {
            cancelled: true,
            ..FakeHost::echo()
        };
        let body = format!(
            r#"try {{ [1].map(|x| {{ loop {{}} }}); }} catch (e) {{ agent("{AFTER_CATCH}"); }}"#
        );
        assert_eq!(run(&body, &host), WorkflowOutcome::Cancelled);
        assert!(host.requests().is_empty(), "{HOST_AFTER_TERMINAL}");
    }

    /// A closure runs on a copy of Rhai's operation count, so after a caught overrun the script
    /// carries on with the count it had before the call.
    #[test_case("catch {}"; "then_ends")]
    #[test_case(r#"catch (e) { agent("p"); }"#; "then_calls_the_host")]
    fn an_operations_overrun_caught_from_a_closure_still_fails_the_run(handler: &str) {
        let host = FakeHost::echo();
        let limits = EngineLimits {
            max_operations: TEST_MAX_OPERATIONS,
            ..EngineLimits::default()
        };
        let outcome = run_with(
            &format!("try {{ [1].map(|x| {{ loop {{}} }}); }} {handler}"),
            &Value::Null,
            &Journal::new(),
            &host,
            &limits,
        );
        let message = failure_message(outcome);
        assert!(message.starts_with(TOO_MANY_OPERATIONS), "{message}");
        assert!(host.requests().is_empty(), "{HOST_AFTER_TERMINAL}");
    }

    /// Each closure call runs under a third of the limit, on a copy of Rhai's count that its
    /// return discards, so no copy reaches the limit, while the ten rounds run three times it.
    #[test]
    fn operations_in_closure_calls_add_up_to_the_limit() {
        let limits = EngineLimits {
            max_operations: TEST_MAX_OPERATIONS,
            ..EngineLimits::default()
        };
        let outcome = run_with(
            "for round in 0..10 { [1].map(|x| { let i = 0; while i < 500 { i += 1; } }); }",
            &Value::Null,
            &Journal::new(),
            &FakeHost::echo(),
            &limits,
        );
        let message = failure_message(outcome);
        assert!(message.starts_with(TOO_MANY_OPERATIONS), "{message}");
    }

    #[test]
    fn host_call_limit_fails_the_run() {
        let limits = EngineLimits {
            max_host_calls: 2,
            ..EngineLimits::default()
        };
        let outcome = run_with(
            r#"agent("a"); agent("b"); agent("c");"#,
            &Value::Null,
            &Journal::new(),
            &FakeHost::echo(),
            &limits,
        );
        assert!(failure_message(outcome).contains("host calls"));
    }

    #[test]
    fn output_limit_fails_complete() {
        let limits = EngineLimits {
            max_output_bytes: 8,
            ..EngineLimits::default()
        };
        let outcome = run_with(
            r#"complete("this is far too long");"#,
            &Value::Null,
            &Journal::new(),
            &FakeHost::echo(),
            &limits,
        );
        assert!(failure_message(outcome).contains("complete()"));
    }

    const JOURNALED_BODY: &str = r#"
        phase("A");
        let one = agent("one");
        log("x");
        let pair = parallel([#{ prompt: "two" }, #{ prompt: "three" }]);
        let path = write_scratch_file("r.md", "content");
        complete([one.output, pair[0].output, pair[1].output, path]);
    "#;

    #[test]
    fn sequential_run_records_dense_keys_and_replays_without_host_calls() {
        let first = FakeHost::echo();
        let expected = WorkflowOutcome::Completed(json!(["one", "two", "three", "/scratch/r.md"]));
        assert_eq!(run(JOURNALED_BODY, &first), expected);
        let journal = first.journal();
        assert_eq!(
            journal.iter().map(|(key, _)| key).collect::<Vec<_>>(),
            [CallKey(1), CallKey(2), CallKey(3), CallKey(4)]
        );
        assert_eq!(first.emissions(), ["phase:A", "log:x"]);

        let replay = FakeHost::failing(HostError::Failed(MUST_NOT_CALL.into()));
        let outcome = run_with(
            JOURNALED_BODY,
            &json!({ "objective": "test" }),
            &journal,
            &replay,
            &EngineLimits::default(),
        );
        assert_eq!(outcome, expected);
        assert!(replay.requests().is_empty());
        assert!(replay.scratch().is_empty());
        assert!(replay.emissions().is_empty());
    }

    #[test]
    fn budget_counts_every_agent_the_script_asked_for() {
        let host = FakeHost::echo();

        let outcome = run(BUDGET_BODY, &host);

        assert_eq!(
            outcome,
            WorkflowOutcome::Completed(json!([3, TEST_AGENT_BUDGET, TEST_AGENT_BUDGET - 3])),
            "{BUDGET_COUNTS_ISSUED}"
        );
    }

    /// The threshold sits between what the script has really spent and what a
    /// replay that counted only freshly dispatched agents would report, so the
    /// second pass would take the other branch and diverge at the call after it.
    #[test]
    fn a_budget_branch_takes_the_same_path_on_a_replay() {
        let first = FakeHost::echo();
        let expected = WorkflowOutcome::Completed(json!(false));
        assert_eq!(run(BUDGET_BRANCH_BODY, &first), expected);

        let replay = FakeHost::failing(HostError::Failed(MUST_NOT_CALL.into()));
        let outcome = run_with(
            BUDGET_BRANCH_BODY,
            &json!({ "objective": "test" }),
            &first.journal(),
            &replay,
            &EngineLimits::default(),
        );

        assert_eq!(outcome, expected, "{BUDGET_SURVIVES_REPLAY}");
        assert!(replay.requests().is_empty(), "{BUDGET_SURVIVES_REPLAY}");
    }

    #[test]
    fn changed_prompt_diverges_from_the_journal() {
        let first = FakeHost::echo();
        run(r#"agent("one"); complete();"#, &first);
        let outcome = run_with(
            r#"agent("two"); complete();"#,
            &Value::Null,
            &first.journal(),
            &FakeHost::echo(),
            &EngineLimits::default(),
        );
        assert!(failure_message(outcome).contains("diverged"));
    }

    #[test]
    fn parallel_keeps_input_order_and_reserves_contiguous_keys() {
        let host = FakeHost::echo();
        let outcome = run(
            r#"agent("first");
            let r = parallel([#{ prompt: "a" }, #{ prompt: "b" }, #{ prompt: "c" }]);
            complete([r[0].output, r[1].output, r[2].output, r[0].agent_id]);"#,
            &host,
        );
        assert_eq!(
            outcome,
            WorkflowOutcome::Completed(json!(["a", "b", "c", "agent-#2"]))
        );
        let keyed: Vec<(CallKey, String)> = host
            .requests()
            .into_iter()
            .map(|(key, request)| (key, request.prompt))
            .collect();
        assert_eq!(
            keyed,
            [
                (CallKey(1), "first".to_owned()),
                (CallKey(4), "c".to_owned()),
                (CallKey(3), "b".to_owned()),
                (CallKey(2), "a".to_owned()),
            ]
        );
    }

    #[test]
    fn partial_parallel_resume_calls_the_host_only_for_unjournaled_items() {
        let body = r#"let r = parallel([#{ prompt: "a" }, #{ prompt: "b" }, #{ prompt: "c" }]);
            complete([r[0].output, r[1].output, r[2].output]);"#;
        let first = FakeHost::echo();
        let expected = WorkflowOutcome::Completed(json!(["a", "b", "c"]));
        assert_eq!(run(body, &first), expected);
        let mut partial = Journal::new();
        for (key, entry) in first.committed.lock().unwrap().iter() {
            if *key != CallKey(3) {
                partial.insert(*key, entry.clone()).expect("unique keys");
            }
        }
        let resume = FakeHost::echo();
        let outcome = run_with(
            body,
            &Value::Null,
            &partial,
            &resume,
            &EngineLimits::default(),
        );
        assert_eq!(outcome, expected);
        let requests = resume.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].0, CallKey(3));
        assert_eq!(requests[0].1.prompt, "c");
    }

    #[test]
    fn emissions_are_suppressed_while_replaying() {
        let body = r#"phase("A"); let x = agent("one"); log("x"); let y = agent("two");
            log("after"); complete(x.output + y.output);"#;
        let first = FakeHost::echo();
        assert_eq!(
            run(body, &first),
            WorkflowOutcome::Completed(json!("onetwo"))
        );
        assert_eq!(first.emissions(), ["phase:A", "log:x", "log:after"]);
        let replay = FakeHost::echo();
        let outcome = run_with(
            body,
            &json!({ "objective": "test" }),
            &first.journal(),
            &replay,
            &EngineLimits::default(),
        );
        assert_eq!(outcome, WorkflowOutcome::Completed(json!("onetwo")));
        assert_eq!(replay.emissions(), ["log:after"]);
        assert!(replay.requests().is_empty());
    }

    fn finding(label: &str, index: usize, severity: &str) -> Value {
        json!({
            "path": format!("src/{label}.rs"),
            "line": index.to_string(),
            "claim": format!("{label} defect {index}"),
            "evidence": format!("{label} evidence {index}"),
            "consequence": format!("{label} breaks {index}"),
            "severity": severity,
        })
    }

    /// Refuters answer from the packet they were handed, so the bijection check
    /// passes and the script reaches the ranking and report it guards.
    fn review_changes_host(refute_everything: bool) -> FakeHost {
        FakeHost::new(Box::new(move |key, request| {
            let label = request.label.clone().unwrap_or_default();
            let output = if label == SURVEYOR {
                json!({
                    "readable": true,
                    "summary": CHANGE_SUMMARY,
                    "files": [{ "path": "src/a.rs", "change": "rewrote it", "risk": "high" }],
                })
            } else if label.starts_with(REVIEWER_PREFIX) {
                let dimension = label.trim_start_matches(REVIEWER_PREFIX);
                let severity = if dimension == SECURITY_DIMENSION {
                    BLOCKER_SEVERITY
                } else {
                    "minor"
                };
                json!({ "findings": [finding(dimension, 1, severity)] })
            } else if label.starts_with(REFUTER_PREFIX) {
                let start =
                    request.prompt.find(FINDINGS_OPEN).expect("packet open") + FINDINGS_OPEN.len();
                let end = request.prompt.find(FINDINGS_CLOSE).expect("packet close");
                let shard: Vec<Value> =
                    serde_json::from_str(&request.prompt[start..end]).expect("packet json");
                let verdicts: Vec<Value> = shard
                    .iter()
                    .map(|found| {
                        let refuted = refute_everything || found["dimension"] == "performance";
                        json!({
                            "finding_id": found["id"],
                            "verdict": if refuted { "refuted" } else { "upheld" },
                            "reason": "read the code",
                        })
                    })
                    .collect();
                json!({ "verdicts": verdicts })
            } else if label == REVIEW_WRITER {
                Value::String(REVIEW_BODY.into())
            } else {
                return Err(HostError::Failed(format!("unexpected label {label}")));
            };
            Ok(agent_result(format!("agent-{key}"), true, output))
        }))
    }

    fn run_review_changes(host: &FakeHost, agent_budget: u32) -> Value {
        let outcome = RhaiEngine.run(RunParams {
            source: REVIEW_CHANGES_SOURCE,
            args: &json!({ "scope": "the working tree" }),
            journal: &Journal::new(),
            host,
            limits: &EngineLimits::default(),
            agent_budget,
        });
        match outcome {
            WorkflowOutcome::Completed(value) => value,
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn review_changes_drops_refuted_findings_and_ranks_the_rest() {
        let host = review_changes_host(false);

        let result = run_review_changes(&host, TEST_AGENT_BUDGET);

        assert_eq!(result["status"], "reviewed", "{result}");
        let findings = result["findings"].as_array().expect("findings");
        assert_eq!(findings.len(), DIMENSION_COUNT - 1, "{result}");
        assert_eq!(
            findings[0]["severity"], BLOCKER_SEVERITY,
            "{RANKED_BY_SEVERITY}"
        );
        assert!(
            findings
                .iter()
                .all(|found| found["dimension"] != "performance"),
            "{REFUTED_ARE_DROPPED}"
        );
        assert_eq!(
            host.phases(),
            ["Survey", "Review", "Refute", "Report"],
            "{result}"
        );
        assert!(
            result["report"]
                .as_str()
                .expect("report")
                .contains(REVIEW_BODY)
        );
    }

    #[test]
    fn review_changes_reports_nothing_when_every_finding_is_refuted() {
        let host = review_changes_host(true);

        let result = run_review_changes(&host, TEST_AGENT_BUDGET);

        assert_eq!(result["findings"], json!([]), "{result}");
        assert_eq!(host.phases(), ["Survey", "Review", "Refute", "Report"]);
        assert_eq!(host.scratch().len(), 1);
    }

    /// Every reviewer is an agent the user pays for, and every one of them would
    /// have reviewed the same nothing. The report has to say so too: a run that
    /// read no change is the one most likely to be mistaken for a clean one.
    #[test]
    fn review_changes_stops_when_the_survey_could_read_nothing() {
        let host = FakeHost::new(Box::new(|key, request| {
            let label = request.label.clone().unwrap_or_default();
            if label != SURVEYOR {
                return Err(HostError::Failed(format!("unexpected label {label}")));
            }
            let output = json!({
                "readable": false,
                "summary": UNREADABLE_SUMMARY,
                "files": [],
            });
            Ok(agent_result(format!("agent-{key}"), true, output))
        }));

        let result = run_review_changes(&host, TEST_AGENT_BUDGET);

        assert_eq!(result["status"], UNREVIEWED_STATUS, "{result}");
        assert_eq!(
            host.phases(),
            ["Survey"],
            "{NOTHING_READ_DISPATCHES_NOBODY}"
        );
        let report = result["report"].as_str().expect("report");
        assert!(
            report.contains(UNREADABLE_SUMMARY) && !report.contains(CLEAN_VERDICT),
            "{NOTHING_READ_IS_NOT_A_PASS}"
        );
    }

    /// A run with no room left for refuters must still deliver a review, and
    /// must say the findings in it went unchallenged.
    #[test]
    fn review_changes_skips_refutation_when_the_budget_is_nearly_spent() {
        let host = review_changes_host(false);

        let result = run_review_changes(&host, DIMENSION_COUNT as u32 + 2);

        assert_eq!(result["status"], "partial", "{result}");
        assert_eq!(
            result["findings"].as_array().map(Vec::len),
            Some(DIMENSION_COUNT),
            "{UNCHALLENGED_ARE_KEPT}"
        );
        assert_eq!(host.phases(), ["Survey", "Review", "Refute", "Report"]);
        let report = result["report"].as_str().expect("report");
        assert!(
            report.contains(UNCHALLENGED_NOTE),
            "{UNCHALLENGED_ARE_KEPT}"
        );
        assert!(
            report.contains(PARTIAL_VERDICT_MARKER),
            "{PARTIAL_IS_SAID_UP_FRONT}"
        );
    }

    /// Five reviewers that all failed found nothing because none of them ever
    /// looked. Calling that "no findings" reports a clean bill of health for a
    /// change nobody read.
    #[test]
    fn review_changes_does_not_call_a_degraded_run_clean() {
        let host = FakeHost::new(Box::new(|key, request| {
            let label = request.label.clone().unwrap_or_default();
            if label != SURVEYOR {
                return Ok(agent_result(format!("agent-{key}"), false, Value::Null));
            }
            let output = json!({
                "readable": true,
                "summary": CHANGE_SUMMARY,
                "files": [{ "path": "src/a.rs", "change": "rewrote it", "risk": "high" }],
            });
            Ok(agent_result(format!("agent-{key}"), true, output))
        }));

        let result = run_review_changes(&host, TEST_AGENT_BUDGET);

        assert_eq!(result["status"], "partial", "{result}");
        let report = result["report"].as_str().expect("report");
        assert!(
            !report.contains(CLEAN_VERDICT),
            "{NOTHING_READ_IS_NOT_A_PASS}"
        );
        assert!(report.contains(GAPS_VERDICT), "{report}");
    }

    /// Every refuter rules the `the-boundary` cause out and every other cause
    /// in, so the survivors separate on how many refuters upheld them.
    fn root_cause_host() -> FakeHost {
        FakeHost::new(Box::new(move |key, request| {
            let label = request.label.clone().unwrap_or_default();
            let output = if label.starts_with(EVIDENCE_PREFIX) {
                let strand = label.trim_start_matches(EVIDENCE_PREFIX);
                json!({
                    "observations": [{
                        "fact": format!("{strand} fact"),
                        "locator": format!("src/{strand}.rs:1"),
                        "relevance": "direct",
                    }],
                    "gaps": [],
                })
            } else if label.starts_with(HYPOTHESIS_PREFIX) {
                let stance = label.trim_start_matches(HYPOTHESIS_PREFIX);
                json!({
                    "hypotheses": [{
                        "cause": format!("{stance} cause"),
                        "mechanism": format!("{stance} mechanism"),
                        "supporting_ids": ["observation-0"],
                        "disconfirming": format!("check {stance}"),
                    }],
                })
            } else if label.starts_with(REFUTER_PREFIX) {
                let start =
                    request.prompt.find(CAUSES_OPEN).expect("packet open") + CAUSES_OPEN.len();
                let end = request.prompt.find(CAUSES_CLOSE).expect("packet close");
                let causes: Vec<Value> =
                    serde_json::from_str(&request.prompt[start..end]).expect("packet json");
                let verdicts: Vec<Value> = causes
                    .iter()
                    .map(|cause| {
                        let refuted = cause["stance"] == REFUTED_STANCE;
                        json!({
                            "cause_id": cause["id"],
                            "verdict": if refuted { "refuted" } else { "survives" },
                            "reason": "read the code",
                        })
                    })
                    .collect();
                json!({ "verdicts": verdicts })
            } else if label == DIAGNOSIS_WRITER {
                Value::String(DIAGNOSIS_BODY.into())
            } else {
                return Err(HostError::Failed(format!("unexpected label {label}")));
            };
            Ok(agent_result(format!("agent-{key}"), true, output))
        }))
    }

    fn run_root_cause(host: &FakeHost, agent_budget: u32) -> Value {
        let outcome = RhaiEngine.run(RunParams {
            source: ROOT_CAUSE_SOURCE,
            args: &json!({ "failure": "it panics on startup" }),
            journal: &Journal::new(),
            host,
            limits: &EngineLimits::default(),
            agent_budget,
        });
        match outcome {
            WorkflowOutcome::Completed(value) => value,
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn root_cause_rules_out_the_refuted_stance_and_ranks_by_convergence() {
        let host = root_cause_host();

        let result = run_root_cause(&host, TEST_AGENT_BUDGET);

        assert_eq!(result["status"], "diagnosed", "{result}");
        let causes = result["causes"].as_array().expect("causes");
        assert_eq!(causes.len(), STANCE_COUNT - 1, "{result}");
        assert!(
            causes.iter().all(|cause| cause["stance"] != REFUTED_STANCE),
            "{REFUTED_STANCE_IS_DROPPED}"
        );
        assert_eq!(
            causes[0]["upheld_by"], STANCE_COUNT,
            "{RANKED_BY_CONVERGENCE}"
        );
        assert_eq!(
            host.phases(),
            ["Evidence", "Hypothesize", "Refute", "Report"]
        );
        assert!(
            result["report"]
                .as_str()
                .expect("report")
                .contains(DIAGNOSIS_BODY)
        );
    }

    /// With room for the writer but not for a refuter, the causes still reach a
    /// report and the report says nothing challenged them.
    #[test]
    fn root_cause_reports_unchallenged_causes_when_the_budget_runs_out() {
        let host = root_cause_host();

        let result = run_root_cause(&host, (STRAND_COUNT + STANCE_COUNT) as u32 + 1);

        assert_eq!(result["status"], "partial", "{result}");
        assert_eq!(
            result["causes"].as_array().map(Vec::len),
            Some(STANCE_COUNT),
            "{UNREFUTED_ARE_KEPT}"
        );
        let report = result["report"].as_str().expect("report");
        assert!(report.contains(UNREFUTED_NOTE), "{UNREFUTED_ARE_KEPT}");
        assert!(
            report.contains(PARTIAL_EVIDENCE_MARKER),
            "{PARTIAL_IS_SAID_UP_FRONT}"
        );
    }

    fn claim(label: &str, index: usize) -> Value {
        json!({
            "claim": format!("{label} claim {index}"),
            "evidence": format!("{label} evidence {index}"),
            "source_title": format!("{label} source {index}"),
            "source_locator": format!("https://example.org/{label}/{index}"),
            "source_type": "primary",
            "confidence": "high",
        })
    }

    fn deep_research_host(researchers_fail: bool) -> FakeHost {
        FakeHost::new(Box::new(move |key, request| {
            let label = request.label.clone().unwrap_or_default();
            let output = if label == PLANNER {
                json!({ "questions": ["What is A?", "What is B?"] })
            } else if label.starts_with(RESEARCHER_PREFIX) {
                if researchers_fail {
                    return Ok(agent_result(format!("agent-{key}"), false, Value::Null));
                }
                json!({ "claims": [claim(&label, 1), claim(&label, 2)], "uncertainties": [] })
            } else if label.starts_with(VERIFIER_PREFIX) {
                let start =
                    request.prompt.find(PACKET_OPEN).expect("packet open") + PACKET_OPEN.len();
                let end = request.prompt.find(PACKET_CLOSE).expect("packet close");
                let claims: Vec<Value> =
                    serde_json::from_str(&request.prompt[start..end]).expect("packet json");
                let verdicts: Vec<Value> = claims
                    .iter()
                    .map(|claim| {
                        json!({
                            "claim_id": claim["id"],
                            "supported": true,
                            "reason": "checked",
                            "evidence": "independent evidence",
                            "source_title": "Independent source",
                            "source_locator": "https://example.org/check",
                        })
                    })
                    .collect();
                json!({ "verdicts": verdicts })
            } else if label == SYNTHESIZER {
                Value::String(REPORT_BODY.into())
            } else {
                return Err(HostError::Failed(format!("unexpected label {label}")));
            };
            Ok(agent_result(format!("agent-{key}"), true, output))
        }))
    }

    fn run_deep_research(host: &FakeHost) -> Value {
        let outcome = RhaiEngine.run(RunParams {
            source: DEEP_RESEARCH_SOURCE,
            args: &json!({ "query": "test" }),
            journal: &Journal::new(),
            host,
            limits: &EngineLimits::default(),
            agent_budget: TEST_AGENT_BUDGET,
        });
        match outcome {
            WorkflowOutcome::Completed(value) => value,
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn deep_research_verifies_claims_end_to_end() {
        let host = deep_research_host(false);
        let result = run_deep_research(&host);
        assert_eq!(result["status"], "verified", "{result}");
        assert_eq!(
            result["verified_claim_ids"].as_array().map(Vec::len),
            Some(4)
        );
        assert_eq!(host.phases(), ["Plan", "Research", "Verify", "Report"]);
        let scratch = host.scratch();
        assert_eq!(scratch.len(), 1);
        assert_eq!(scratch[0].0, REPORT_FILE);
        assert!(scratch[0].1.contains(SOURCES_HEADING));
        assert_eq!(result["path"], format!("{SCRATCH_ROOT}/{REPORT_FILE}"));
    }

    #[test]
    fn deep_research_reports_partial_when_researchers_fail() {
        let host = deep_research_host(true);
        let result = run_deep_research(&host);
        assert_eq!(result["status"], "partial", "{result}");
        assert_eq!(result["verified_claim_ids"], json!([]));
        assert_eq!(host.phases(), ["Plan", "Research"]);
        assert_eq!(host.scratch().len(), 1);
    }
}
