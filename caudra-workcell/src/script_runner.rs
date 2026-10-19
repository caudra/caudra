//! Runs `code` snippets in Workcell's hosted Monty worker.
//!
//! MCP servers are always clients of this process, so snippets that call them always run here,
//! whatever Workcell the session's other tools use. An embedded host lends its own runtime and
//! worker pool; with a remote Workcell the runner builds both on first use.
//!
//! The snippet runs on the Tokio runtime that owns the worker, while every host call it makes is
//! the agent's own future. Those are spawned onto smol, which is where they expect to run, and the
//! Tokio side only awaits their handles; dropping a run drops the handles and cancels the calls.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use caudra_agent::code::runner::{
    ReplNote, ReplState, RunFuture, ScriptCall, ScriptError, ScriptErrorKind, ScriptException,
    ScriptHost, ScriptInvocation, ScriptName, ScriptOutcome, ScriptOutput, ScriptRepair, ScriptRun,
    ScriptRunner, ScriptStatus,
};
use futures_lite::future;
use tokio::runtime::{Builder, Handle, Runtime};
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;
use workcell::CodeToolGroup;
use workcell::code::{
    CodeConfiguration, CodeExecution, CodeHost, CodePools, HostCall, HostError, HostErrorKind,
    HostInvocation, HostName, HostedState, Outcome, SleepFuture, StateNote, WorkerSource,
};

use crate::HostInner;

const RUNTIME_THREAD_NAME: &str = "caudra-code";
const RUNTIME_WORKERS: usize = 2;
const NO_WORKER: &str = "The code tool needs the code worker, which is unavailable in this build or environment. Set WORKCELL_MCP_CODE_WORKER to a monty worker, or use a release build, which bundles one.";
const RUNTIME_FAILED: &str = "The code runner could not start its runtime";
const RUN_FAILED: &str = "The code runner failed while running the snippet";

pub struct WorkcellScriptRunner {
    backend: Backend,
}

#[derive(Clone)]
enum Backend {
    Embedded(Arc<HostInner>),
    Standalone(Arc<Standalone>),
}

/// The runtime and worker pool of a runner with no embedded host, built when first needed.
struct Standalone {
    worker: Option<PathBuf>,
    runtime: OnceLock<Result<Runtime, String>>,
    group: OnceCell<Result<Arc<CodeToolGroup>, String>>,
}

impl WorkcellScriptRunner {
    pub(crate) fn embedded(host: Arc<HostInner>) -> Self {
        Self {
            backend: Backend::Embedded(host),
        }
    }

    /// A runner that owns its worker pool. `worker` overrides the bundled worker, as
    /// `WORKCELL_MCP_CODE_WORKER` does for an embedded host.
    pub fn standalone(worker: Option<PathBuf>) -> Self {
        Self {
            backend: Backend::Standalone(Arc::new(Standalone {
                worker,
                runtime: OnceLock::new(),
                group: OnceCell::new(),
            })),
        }
    }
}

impl Backend {
    fn handle(&self) -> Result<Handle, String> {
        match self {
            Self::Embedded(host) => Ok(host.runtime.handle().clone()),
            Self::Standalone(standalone) => standalone
                .runtime
                .get_or_init(|| {
                    Builder::new_multi_thread()
                        .worker_threads(RUNTIME_WORKERS)
                        .enable_all()
                        .thread_name(RUNTIME_THREAD_NAME)
                        .build()
                        .map_err(|error| format!("{RUNTIME_FAILED}: {error}"))
                })
                .as_ref()
                .map(|runtime| runtime.handle().clone())
                .map_err(Clone::clone),
        }
    }

    /// Runs on the backend's runtime.
    async fn group(&self) -> Result<Arc<CodeToolGroup>, String> {
        match self {
            Self::Embedded(host) => host.code.clone().ok_or_else(|| NO_WORKER.to_owned()),
            Self::Standalone(standalone) => standalone
                .group
                .get_or_init(|| standalone.build())
                .await
                .clone(),
        }
    }
}

impl Standalone {
    async fn build(&self) -> Result<Arc<CodeToolGroup>, String> {
        let cache_root;
        let worker = match &self.worker {
            Some(path) => WorkerSource::Path(path),
            None => {
                cache_root =
                    caudra_storage::paths::cache_dir().map_err(|_| NO_WORKER.to_owned())?;
                WorkerSource::Bundled {
                    cache_root: &cache_root,
                }
            }
        };
        CodeToolGroup::new(CodeConfiguration {
            worker,
            type_check: true,
            pools: CodePools::HostedOnly,
        })
        .await
        .map(Arc::new)
        .map_err(|error| format!("{NO_WORKER} ({error})"))
    }
}

impl ScriptRunner for WorkcellScriptRunner {
    fn run(&self, run: ScriptRun) -> RunFuture {
        let backend = self.backend.clone();
        Box::pin(async move {
            let ScriptRun {
                code,
                host,
                mut state,
                cancel,
            } = run;
            let handle = match backend.handle() {
                Ok(handle) => handle,
                Err(message) => return unavailable(message),
            };
            let token = CancellationToken::new();
            // Dropping the run stops the snippet and its host calls, which then hand the session
            // back to the lease the next run is waiting on.
            let _stop_when_dropped = token.clone().drop_guard();
            let task_token = token.clone();
            let task = handle.spawn(async move {
                let saved = state.take().and_then(ReplState::into_inner::<HostedState>);
                let (outcome, kept) = run_hosted(&backend, code, host, saved, task_token).await;
                state.keep(kept.map(|kept| ReplState::new(kept.len(), kept)));
                outcome
            });
            let mut task = Box::pin(task);
            let joined = future::or(task.as_mut(), async {
                cancel.cancelled().await;
                token.cancel();
                std::future::pending().await
            })
            .await;
            joined.unwrap_or_else(|_| ScriptOutcome {
                output: Some(ScriptOutput::refused(ScriptStatus::Unavailable, RUN_FAILED)),
                ..ScriptOutcome::default()
            })
        })
    }

    /// A standalone runner builds its pool on first use, so until then it knows of no reason.
    fn unavailable(&self) -> Option<String> {
        match &self.backend {
            Backend::Embedded(host) => host.code.is_none().then(|| NO_WORKER.to_owned()),
            Backend::Standalone(standalone) => standalone
                .group
                .get()
                .and_then(|built| built.as_ref().err().cloned()),
        }
    }
}

/// Runs on the backend's runtime, answering with the outcome and the session to keep.
async fn run_hosted(
    backend: &Backend,
    code: String,
    host: Arc<dyn ScriptHost>,
    saved: Option<HostedState>,
    token: CancellationToken,
) -> (ScriptOutcome, Option<HostedState>) {
    let group = match backend.group().await {
        Ok(group) => group,
        Err(message) => return (unavailable(message), saved),
    };
    let prepared = match group.prepare_hosted(code) {
        Ok(prepared) => prepared,
        Err(message) => {
            let refused = ScriptOutcome {
                output: Some(ScriptOutput::refused(ScriptStatus::Rejected, message)),
                ..ScriptOutcome::default()
            };
            return (refused, saved);
        }
    };
    let host: Arc<dyn CodeHost> = Arc::new(Bridge(host));
    let executed = group.execute_hosted(prepared, host, saved, token).await;
    let outcome = ScriptOutcome {
        output: executed.execution.map(output),
        note: executed.state_note.map(|note| match note {
            StateNote::RestoreFailed => ReplNote::RestoreFailed,
            StateNote::RunDiscarded => ReplNote::RunDiscarded,
            StateNote::TooLarge { bytes } => ReplNote::TooLarge { bytes },
        }),
        repairs: executed
            .repairs
            .iter()
            .map(|repair| ScriptRepair {
                done: repair.to_string(),
                lesson: repair.lesson(),
            })
            .collect(),
        taught_code: executed.taught_code,
    };
    (outcome, executed.state)
}

fn unavailable(message: String) -> ScriptOutcome {
    ScriptOutcome {
        output: Some(ScriptOutput::refused(ScriptStatus::Unavailable, message)),
        ..ScriptOutcome::default()
    }
}

fn output(execution: CodeExecution) -> ScriptOutput {
    let output = execution.output;
    ScriptOutput {
        status: match output.outcome {
            Outcome::Completed => ScriptStatus::Completed,
            Outcome::Exception => ScriptStatus::Exception,
            Outcome::Rejected => ScriptStatus::Rejected,
            Outcome::Limited => ScriptStatus::Limited,
            Outcome::Unavailable => ScriptStatus::Unavailable,
        },
        result: output.result,
        result_repr: output.result_repr,
        stdout: output.stdout,
        stderr: output.stderr,
        stdout_truncated: output.stdout_truncated,
        stderr_truncated: output.stderr_truncated,
        exception: output.exception.map(|exception| ScriptException {
            kind: exception.r#type,
            message: exception.message,
        }),
        diagnostic: output.diagnostic,
        duration_ms: output.duration_ms,
        timed_out: output.timed_out,
    }
}

/// The agent's gateway seen as a Workcell host.
struct Bridge(Arc<dyn ScriptHost>);

impl CodeHost for Bridge {
    fn lookup(&self, namespace: Option<&str>, name: &str) -> HostName {
        match self.0.lookup(namespace, name) {
            ScriptName::Namespace => HostName::Namespace,
            ScriptName::Function => HostName::Function,
            ScriptName::Undefined(message) => HostName::Undefined(message),
        }
    }

    fn call(&self, call: HostCall) -> HostInvocation {
        let HostCall {
            namespace,
            name,
            args,
            kwargs,
        } = call;
        match self.0.call(ScriptCall {
            namespace,
            name,
            args,
            kwargs,
        }) {
            ScriptInvocation::Ready(reply) => HostInvocation::Ready(reply.map_err(host_error)),
            ScriptInvocation::Deferred(reply) => HostInvocation::Deferred(Box::pin(async move {
                smol::spawn(reply).await.map_err(host_error)
            })),
        }
    }

    fn stubs(&self) -> String {
        self.0.stubs()
    }

    fn sleep(&self, delay: Duration) -> SleepFuture {
        let wait = self.0.sleep(delay);
        Box::pin(async move { smol::spawn(wait).await })
    }
}

fn host_error(error: ScriptError) -> HostError {
    let kind = match error.kind {
        ScriptErrorKind::Runtime => HostErrorKind::Runtime,
        ScriptErrorKind::Permission => HostErrorKind::Permission,
        ScriptErrorKind::Type => HostErrorKind::Type,
        ScriptErrorKind::Value => HostErrorKind::Value,
        ScriptErrorKind::Attribute => HostErrorKind::Attribute,
        ScriptErrorKind::Name => HostErrorKind::Name,
    };
    HostError::new(kind, error.message)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use std::time::Duration;

    use caudra_agent::CancelToken;
    use caudra_agent::code::runner::{ReplLease, ReplSlot, ScriptFuture, SharedRepl};
    use caudra_agent::tools::native::code::DESCRIPTION;
    use serde_json::{Value, json};
    use workcell::code::{SUBSET_MODULES, UNTYPED_BUILTINS, WITHHELD_BUILTINS};

    use super::*;

    const NAMESPACE: &str = "srv";
    const ECHO: &str = "echo";
    const HANG: &str = "hang";
    const STUBS: &str = "from typing import Any\nclass _S:\n    async def echo(self, *, text: str) -> Any: ...\n    async def hang(self) -> Any: ...\nsrv: _S\n";
    /// Only a deadline for a step that takes milliseconds, so a hang fails the test.
    const STEP_DEADLINE: Duration = Duration::from_secs(60);
    const PURE_COMPUTATION: &str = "print('summing')\nsum([1, 2, 3, 4])";
    const PURE_RESULT: i64 = 10;
    const PURE_STDOUT: &str = "summing\n";

    /// The `code` tool restates the worker's subset in its own words, so it is held to Workcell's
    /// list rather than to a copy of it.
    #[test]
    fn the_code_description_states_the_subset_the_worker_provides() {
        let named = |name: &str| DESCRIPTION.contains(&format!("`{name}`"));
        for name in SUBSET_MODULES.iter().chain(&UNTYPED_BUILTINS) {
            assert!(named(name), "`{name}` is missing from the description");
        }
        for name in WITHHELD_BUILTINS {
            assert!(!named(name), "`{name}` is not defined in the worker");
        }
    }

    #[derive(Default)]
    struct EchoHost {
        calls: Mutex<Vec<ScriptCall>>,
        /// Told when `hang` is called; the call it starts closes the second when dropped.
        hanging: Mutex<Option<(flume::Sender<()>, flume::Sender<()>)>>,
    }

    impl ScriptHost for EchoHost {
        fn lookup(&self, namespace: Option<&str>, name: &str) -> ScriptName {
            match (namespace, name) {
                (None, NAMESPACE) => ScriptName::Namespace,
                (Some(NAMESPACE), ECHO | HANG) => ScriptName::Function,
                _ => ScriptName::Undefined(None),
            }
        }

        fn call(&self, call: ScriptCall) -> ScriptInvocation {
            self.calls.lock().expect("calls").push(call.clone());
            if call.name == HANG {
                let (started, held) = self
                    .hanging
                    .lock()
                    .expect("hanging")
                    .take()
                    .expect("a host that hangs once");
                let reply: ScriptFuture = Box::pin(async move {
                    let _held_until_dropped = held;
                    let _ = started.send(());
                    std::future::pending().await
                });
                return ScriptInvocation::Deferred(reply);
            }
            let text = call.kwargs.get("text").cloned().unwrap_or(Value::Null);
            let reply: ScriptFuture = Box::pin(async move {
                smol::Timer::after(std::time::Duration::from_millis(1)).await;
                Ok(json!({ "echo": text }))
            });
            ScriptInvocation::Deferred(reply)
        }

        fn stubs(&self) -> String {
            STUBS.to_owned()
        }
    }

    fn runner() -> Option<WorkcellScriptRunner> {
        let worker = std::env::var_os("WORKCELL_MCP_CODE_WORKER").map(PathBuf::from)?;
        Some(WorkcellScriptRunner::standalone(Some(worker)))
    }

    async fn start(
        runner: &WorkcellScriptRunner,
        host: &Arc<EchoHost>,
        code: &str,
        repl: &SharedRepl,
    ) -> RunFuture {
        let host: Arc<dyn ScriptHost> = host.clone();
        runner.run(ScriptRun {
            code: code.to_owned(),
            host,
            state: ReplLease::acquire(repl).await,
            cancel: CancelToken::none(),
        })
    }

    fn run(
        runner: &WorkcellScriptRunner,
        host: &Arc<EchoHost>,
        code: &str,
        repl: &SharedRepl,
    ) -> ScriptOutcome {
        smol::block_on(async { start(runner, host, code, repl).await.await })
    }

    fn holds_state(repl: &SharedRepl) -> bool {
        smol::block_on(ReplLease::acquire(repl)).take().is_some()
    }

    async fn within_deadline<T>(step: impl Future<Output = T>) -> T {
        future::or(step, async {
            smol::Timer::after(STEP_DEADLINE).await;
            panic!("a step that takes milliseconds hung");
        })
        .await
    }

    #[test]
    fn smol_side_calls_answer_the_snippet_and_state_carries_over() {
        let Some(runner) = runner() else { return };
        let host = Arc::new(EchoHost::default());
        let repl = ReplSlot::shared(false);
        let first = run(
            &runner,
            &host,
            "kept = await srv.echo(text='hi')\nkept['echo']",
            &repl,
        );
        let output = first.output.expect("ran");
        assert_eq!(output.status, ScriptStatus::Completed, "{output:?}");
        assert_eq!(output.result, json!("hi"));
        let second = run(&runner, &host, "kept", &repl);
        assert_eq!(second.output.expect("ran").result, json!({ "echo": "hi" }));
        assert_eq!(host.calls.lock().expect("calls").len(), 1);
    }

    /// Plain computation is the hosted pool's job now: a snippet that calls no host prints and
    /// returns like any other, with no isolated pool behind it.
    #[test]
    fn pure_computation_runs_on_the_hosted_only_pool() {
        let Some(runner) = runner() else { return };
        let host = Arc::new(EchoHost::default());
        let output = run(&runner, &host, PURE_COMPUTATION, &ReplSlot::shared(false))
            .output
            .expect("ran");
        assert_eq!(output.status, ScriptStatus::Completed, "{output:?}");
        assert_eq!(output.result, json!(PURE_RESULT));
        assert_eq!(output.stdout, PURE_STDOUT);
        assert!(host.calls.lock().expect("calls").is_empty());
    }

    #[test]
    fn invalid_input_keeps_the_state_it_was_given() {
        let Some(runner) = runner() else { return };
        let host = Arc::new(EchoHost::default());
        let repl = ReplSlot::shared(false);
        run(&runner, &host, "kept = 1", &repl);
        let refused = run(&runner, &host, "   ", &repl);
        assert_eq!(
            refused.output.expect("refused").status,
            ScriptStatus::Rejected
        );
        assert!(holds_state(&repl));
    }

    /// A run dropped mid-call, as a turn torn down under it drops it, stops the snippet and the
    /// call it is waiting on, and the next run still sees the variables from before it.
    #[test]
    fn a_dropped_run_stops_its_calls_and_hands_on_its_variables() {
        let Some(runner) = runner() else { return };
        let (started, started_rx) = flume::bounded(1);
        let (held, held_rx) = flume::bounded::<()>(1);
        let host = Arc::new(EchoHost {
            hanging: Mutex::new(Some((started, held))),
            ..EchoHost::default()
        });
        let repl = ReplSlot::shared(false);
        run(&runner, &host, "kept = 41", &repl);

        let hang = format!("await {NAMESPACE}.{HANG}()");
        smol::block_on(within_deadline(async {
            let hanging = start(&runner, &host, &hang, &repl);
            let dropped = future::or(async { Some(hanging.await.await) }, async {
                started_rx.recv_async().await.expect("the call started");
                None
            })
            .await;
            assert!(dropped.is_none(), "the hanging run finished: {dropped:?}");
            assert!(held_rx.recv_async().await.is_err(), "the call was dropped");
        }));

        let next = run(&runner, &host, "kept + 1", &repl);
        assert_eq!(next.output.expect("ran").result, json!(42));
    }

    /// The gateway reads the stub class and the missing name out of this rejection to say which
    /// tool was meant, so its wording is held here against the real checker.
    #[test]
    fn a_misspelt_tool_is_rejected_naming_its_class_and_the_missing_name() {
        let Some(runner) = runner() else { return };
        let host = Arc::new(EchoHost::default());
        let code = format!("await {NAMESPACE}.ech(text='hi')");
        let output = run(&runner, &host, &code, &ReplSlot::shared(false))
            .output
            .expect("refused");
        assert_eq!(output.status, ScriptStatus::Rejected, "{output:?}");
        let diagnostic = output.diagnostic.expect("diagnostic");
        assert!(
            diagnostic.contains("`_S` has no attribute `ech`"),
            "{diagnostic}"
        );
        assert!(host.calls.lock().expect("calls").is_empty());
    }

    #[test]
    fn a_missing_worker_is_reported_with_the_remedy() {
        let runner = WorkcellScriptRunner::standalone(Some(PathBuf::from("/nonexistent/monty")));
        assert_eq!(runner.unavailable(), None);
        let host = Arc::new(EchoHost::default());
        let outcome = run(&runner, &host, "1", &ReplSlot::shared(false));
        let output = outcome.output.expect("refused");
        assert_eq!(output.status, ScriptStatus::Unavailable);
        let message = output.diagnostic.expect("message");
        assert!(message.starts_with(NO_WORKER));
        assert_eq!(runner.unavailable(), Some(message));
    }
}
