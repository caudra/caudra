use std::any::Any;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Instant;

use rhai::{Array, Dynamic, Engine, EvalAltResult, Map, Position, Scope};
use serde::Serialize;
use serde_json::Value;

use crate::host::{AgentRequest, HostError, UnknownCapabilityMode, WorkflowHost};
use crate::journal::{
    CallKey, CallKind, Journal, agent_request_value, hash_request, scratch_request_value,
};
use crate::run::{EngineLimits, PauseKind, WorkflowOutcome};

const INTERPRETER_THREAD_NAME: &str = "caudra-workflow";
const INTERPRETER_STACK_BYTES: usize = 32 * 1024 * 1024;
/// Operations between deadline and cancellation polls; each poll is one host round trip.
const PROGRESS_POLL_OPS: u64 = 16_384;
const DISABLED_SYMBOLS: [&str; 3] = ["eval", "print", "debug"];
const ARGS_VARIABLE: &str = "args";

const OPT_PROMPT: &str = "prompt";
const OPT_LABEL: &str = "label";
const OPT_CAPABILITY_MODE: &str = "capability_mode";
const OPT_OUTPUT_SCHEMA: &str = "output_schema";
const OPT_PHASE: &str = "phase";
const OPT_PROFILE: &str = "profile";
const AGENT_OPTIONS: [&str; 6] = [
    OPT_PROMPT,
    OPT_LABEL,
    OPT_CAPABILITY_MODE,
    OPT_OUTPUT_SCHEMA,
    OPT_PHASE,
    OPT_PROFILE,
];

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
        restricted_engine(&limits)
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
        } = params;
        let (jobs_tx, jobs_rx) = mpsc::channel();
        thread::scope(|scope| {
            let interpreter = thread::Builder::new()
                .name(INTERPRETER_THREAD_NAME.into())
                .stack_size(INTERPRETER_STACK_BYTES)
                .spawn_scoped(scope, move || {
                    evaluate(source, args, journal, limits, HostBridge(jobs_tx))
                });
            let interpreter = match interpreter {
                Ok(handle) => handle,
                Err(error) => {
                    return WorkflowOutcome::Failed(format!(
                        "could not start the workflow interpreter: {error}"
                    ));
                }
            };
            for job in jobs_rx {
                job(host);
            }
            interpreter
                .join()
                .unwrap_or_else(|panic| WorkflowOutcome::Failed(panic_message(panic.as_ref())))
        })
    }
}

type ScriptResult<T> = Result<T, Box<EvalAltResult>>;
type HostJob = Box<dyn FnOnce(&dyn WorkflowHost) + Send>;

/// Ends the run. Travels inside `EvalAltResult::ErrorTerminated`, which scripts cannot catch.
#[derive(Clone)]
enum Terminal {
    Complete(Value),
    Pause { kind: PauseKind, message: String },
    Cancelled,
    BudgetLimited,
    Fatal(String),
}

impl Terminal {
    fn into_outcome(self) -> WorkflowOutcome {
        match self {
            Self::Complete(value) => WorkflowOutcome::Completed(value),
            Self::Pause { kind, message } => WorkflowOutcome::Paused { kind, message },
            Self::Cancelled => WorkflowOutcome::Cancelled,
            Self::BudgetLimited => WorkflowOutcome::BudgetLimited,
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

fn fatal(message: impl Into<String>) -> Box<EvalAltResult> {
    Terminal::Fatal(message.into()).into()
}

fn runtime_error(message: impl Into<String>) -> Box<EvalAltResult> {
    Box::new(EvalAltResult::ErrorRuntime(
        Dynamic::from(message.into()),
        Position::NONE,
    ))
}

/// Ships host work to the thread that owns the `&dyn WorkflowHost` and waits for the answer.
#[derive(Clone)]
struct HostBridge(Sender<HostJob>);

impl HostBridge {
    fn call<T: Send + 'static>(
        &self,
        job: impl FnOnce(&dyn WorkflowHost) -> T + Send + 'static,
    ) -> Result<T, Terminal> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.0
            .send(Box::new(move |host| {
                let _ = reply_tx.send(job(host));
            }))
            .map_err(|_| Terminal::Fatal(HOST_GONE.into()))?;
        reply_rx
            .recv()
            .map_err(|_| Terminal::Fatal(HOST_GONE.into()))
    }
}

fn host_result<T>(reply: Result<Result<T, HostError>, Terminal>) -> ScriptResult<T> {
    match reply? {
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
}

struct Session {
    host: HostBridge,
    journal: Journal,
    limits: EngineLimits,
    state: RefCell<RunState>,
}

impl Session {
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
        Ok(self.host.call(job)?)
    }

    fn agent(&self, request: AgentRequest) -> ScriptResult<Dynamic> {
        let key = self.reserve_keys(1)?;
        let result = match self.replayed(key, CallKind::Agent, &agent_request_value(&request))? {
            Some(result) => result,
            None => json_value(&host_result(
                self.host.call(move |host| host.agent(key, &request)),
            )?),
        };
        json_to_dynamic(&result)
    }

    /// Journaled items are replayed; the rest are sent to the host in contiguous key runs, so a
    /// run that died mid-`parallel` only re-issues the items that never got committed.
    fn parallel(&self, requests: Vec<AgentRequest>) -> ScriptResult<Array> {
        let first = self.reserve_keys(requests.len())?;
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

fn unavailable(name: &str) -> ScriptResult<()> {
    Err(runtime_error(format!(
        "{name}() is unavailable in workflow scripts; end a run with complete() or pause()"
    )))
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

/// A full engine with `eval`/`print`/`debug` disabled, `sleep`/`exit` stubbed to errors, and every
/// resource limit applied. `import` and `timestamp` are compiled out by the `no_module` and
/// `no_time` features.
pub(crate) fn restricted_engine(limits: &EngineLimits) -> Engine {
    let mut engine = Engine::new();
    engine
        .set_max_operations(limits.max_operations)
        .set_max_call_levels(limits.max_call_levels)
        .set_max_expr_depths(limits.max_expr_depth, limits.max_expr_depth)
        .set_max_string_size(limits.max_string_size)
        .set_max_array_size(limits.max_array_size)
        .set_max_map_size(limits.max_map_size);
    for symbol in DISABLED_SYMBOLS {
        engine.disable_symbol(symbol);
    }
    engine.register_fn("sleep", |_seconds: i64| unavailable("sleep"));
    engine.register_fn("sleep", |_seconds: f64| unavailable("sleep"));
    engine.register_fn("exit", || unavailable("exit"));
    engine.register_fn("exit", |_value: Dynamic| unavailable("exit"));
    engine
}

fn register_host_api(engine: &mut Engine, session: &Rc<Session>) {
    let s = Rc::clone(session);
    engine.register_fn("agent", move |prompt: &str| -> ScriptResult<Dynamic> {
        s.agent(agent_request(Some(prompt), Map::new())?)
    });
    let s = Rc::clone(session);
    engine.register_fn(
        "agent",
        move |prompt: &str, options: Map| -> ScriptResult<Dynamic> {
            s.agent(agent_request(Some(prompt), options)?)
        },
    );
    let s = Rc::clone(session);
    engine.register_fn("parallel", move |items: Array| -> ScriptResult<Array> {
        let requests = items
            .into_iter()
            .map(|item| {
                item.try_cast::<Map>()
                    .ok_or_else(|| runtime_error(PARALLEL_ITEM_TYPE))
                    .and_then(|options| agent_request(None, options))
            })
            .collect::<ScriptResult<Vec<AgentRequest>>>()?;
        s.parallel(requests)
    });
    let s = Rc::clone(session);
    engine.register_fn("phase", move |title: &str| -> ScriptResult<()> {
        let title = title.to_owned();
        s.emit(title.len(), move |host| host.phase(&title))
    });
    let s = Rc::clone(session);
    engine.register_fn("log", move |message: &str| -> ScriptResult<()> {
        let message = message.to_owned();
        s.emit(message.len(), move |host| host.log(&message))
    });
    let s = Rc::clone(session);
    engine.register_fn(
        "write_scratch_file",
        move |name: &str, content: &str| -> ScriptResult<String> {
            s.write_scratch_file(name.to_owned(), content.to_owned())
        },
    );
    let s = Rc::clone(session);
    engine.register_fn("complete", move |value: Dynamic| -> ScriptResult<()> {
        Err(s.complete(dynamic_to_json(&value)?))
    });
    let s = Rc::clone(session);
    engine.register_fn("complete", move || -> ScriptResult<()> {
        Err(s.complete(Value::Null))
    });
    engine.register_fn("pause", |kind: &str, message: &str| -> ScriptResult<()> {
        let kind = PauseKind::new(kind).map_err(|error| runtime_error(error.to_string()))?;
        Err(Terminal::Pause {
            kind,
            message: message.to_owned(),
        }
        .into())
    });
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
    host: HostBridge,
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
        state: RefCell::new(RunState {
            next_key: CallKey::FIRST,
            log_entries: 0,
            log_bytes: 0,
        }),
    });
    let mut engine = restricted_engine(limits);
    register_host_api(&mut engine, &session);
    let deadline = Instant::now().checked_add(limits.wall_time);
    let progress_session = Rc::clone(&session);
    engine.on_progress(move |operations| {
        if !operations.is_multiple_of(PROGRESS_POLL_OPS) {
            return None;
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Some(Dynamic::from(Terminal::Fatal(format!(
                "workflow exceeded its wall-time limit of {:?}",
                progress_session.limits.wall_time
            ))));
        }
        match progress_session.host.call(|host| host.is_cancelled()) {
            Ok(false) => None,
            Ok(true) => Some(Dynamic::from(Terminal::Cancelled)),
            Err(terminal) => Some(Dynamic::from(terminal)),
        }
    });
    let ast = match engine.compile(source) {
        Ok(ast) => ast,
        Err(error) => {
            return WorkflowOutcome::Failed(EngineError::Compile(error.to_string()).to_string());
        }
    };
    let mut scope = Scope::new();
    scope.push_dynamic(ARGS_VARIABLE, args);
    match engine.run_ast_with_scope(&mut scope, &ast) {
        Ok(()) => WorkflowOutcome::Completed(Value::Null),
        Err(error) => outcome_from_error(*error),
    }
}

fn outcome_from_error(error: EvalAltResult) -> WorkflowOutcome {
    match error {
        EvalAltResult::ErrorTerminated(token, _) => token.try_cast::<Terminal>().map_or_else(
            || WorkflowOutcome::Failed(FOREIGN_TERMINATION.into()),
            Terminal::into_outcome,
        ),
        other => WorkflowOutcome::Failed(other.to_string()),
    }
}

fn panic_message(panic: &(dyn Any + Send)) -> String {
    let detail = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or("unknown panic");
    format!("workflow interpreter panicked: {detail}")
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, PoisonError};
    use std::time::Duration;

    use serde_json::json;
    use test_case::test_case;

    use super::*;
    use crate::DEEP_RESEARCH_SOURCE;
    use crate::host::{AgentResult, CapabilityMode};
    use crate::journal::JournalEntry;

    const META: &str = r#"let meta = #{ name: "t", description: "d" };"#;
    const SCRATCH_ROOT: &str = "/scratch";
    const PHASE_PREFIX: &str = "phase:";
    const LOG_PREFIX: &str = "log:";
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
                output_schema: #{ "type": "object" },
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

        let replay = FakeHost::failing(HostError::Failed("must not be called".into()));
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
