use std::any::Any;
use std::io;
use std::sync::mpsc::{self, Sender};
use std::thread;

const INTERPRETER_STACK_BYTES: usize = 32 * 1024 * 1024;
const UNKNOWN_PANIC: &str = "unknown panic";

/// Names the host an interpreter calls into for every lifetime it can be borrowed for, as in
/// `type Host<'h> = dyn WorkflowHost + 'h`. Engine callbacks must be `'static`, so the bridge
/// they hold cannot name the borrow itself.
pub trait HostKind {
    type Host<'h>: ?Sized + 'h;
}

type HostJob<K> = Box<dyn for<'h> FnOnce(&'h <K as HostKind>::Host<'h>) + Send>;

/// The interpreter's end of the channel to the thread that owns the host. Rhai values are not
/// `Send`, so the interpreter runs on its own thread and ships each host call back here.
pub struct HostBridge<K: HostKind>(Sender<HostJob<K>>);

impl<K: HostKind> HostBridge<K> {
    /// Runs `job` against the host on the caller's thread and waits for its answer.
    pub fn call<T: Send + 'static>(
        &self,
        job: impl for<'h> FnOnce(&'h K::Host<'h>) -> T + Send + 'static,
    ) -> Result<T, BridgeClosed> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.0
            .send(Box::new(move |host| {
                let _ = reply_tx.send(job(host));
            }))
            .map_err(|_| BridgeClosed)?;
        reply_rx.recv().map_err(|_| BridgeClosed)
    }
}

/// The caller's thread stopped serving host calls.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("the host stopped servicing calls")]
pub struct BridgeClosed;

#[derive(Debug, thiserror::Error)]
pub enum InterpreterError {
    #[error("could not start the interpreter: {0}")]
    Spawn(io::Error),
    #[error("interpreter panicked: {0}")]
    Panicked(String),
}

/// Runs `interpret` on its own named thread, with a stack deep enough for the engine's limits,
/// and serves its host calls on this thread until it returns.
pub fn run_interpreter<'h, K: HostKind, R: Send>(
    thread_name: &str,
    host: &'h K::Host<'h>,
    interpret: impl FnOnce(HostBridge<K>) -> R + Send,
) -> Result<R, InterpreterError> {
    let (jobs_tx, jobs_rx) = mpsc::channel();
    thread::scope(|scope| {
        let interpreter = thread::Builder::new()
            .name(thread_name.to_owned())
            .stack_size(INTERPRETER_STACK_BYTES)
            .spawn_scoped(scope, move || interpret(HostBridge(jobs_tx)))
            .map_err(InterpreterError::Spawn)?;
        for job in jobs_rx {
            job(host);
        }
        interpreter
            .join()
            .map_err(|panic| InterpreterError::Panicked(panic_detail(panic.as_ref())))
    })
}

fn panic_detail(panic: &(dyn Any + Send)) -> String {
    panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or(UNKNOWN_PANIC)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use std::panic;
    use std::thread::ThreadId;

    use test_case::test_case;

    use super::*;

    const THREAD_NAME: &str = "caudra-script-test";
    const GREETING: &str = "borrowed for the run";
    const PANIC_DETAIL: &str = "the interpreter fell over";
    const PANIC_CODE: i32 = 3;
    const MUST_RUN: &str = "the interpreter thread must start and finish";
    const MUST_PANIC: &str = "the interpreter panic must surface as an error";

    trait Greeter {
        fn greet(&self) -> (String, ThreadId);
    }

    struct Borrowing<'a>(&'a str);

    impl Greeter for Borrowing<'_> {
        fn greet(&self) -> (String, ThreadId) {
            (self.0.to_owned(), thread::current().id())
        }
    }

    struct Greeters;

    impl HostKind for Greeters {
        type Host<'h> = dyn Greeter + 'h;
    }

    #[test]
    fn a_borrowed_host_answers_on_the_callers_thread() {
        let greeting = GREETING.to_owned();
        let host = Borrowing(&greeting);
        let (interpreter, answer) = run_interpreter::<Greeters, _>(THREAD_NAME, &host, |bridge| {
            (
                thread::current().name().map(str::to_owned),
                bridge.call(|host| host.greet()),
            )
        })
        .expect(MUST_RUN);
        assert_eq!(interpreter.as_deref(), Some(THREAD_NAME));
        assert_eq!(answer, Ok((greeting, thread::current().id())));
    }

    #[test]
    fn a_closed_bridge_fails_the_call() {
        let (jobs_tx, jobs_rx) = mpsc::channel();
        drop(jobs_rx);
        let bridge = HostBridge::<Greeters>(jobs_tx);
        assert_eq!(bridge.call(|host| host.greet()), Err(BridgeClosed));
    }

    #[test_case(|| panic::panic_any(PANIC_DETAIL) => PANIC_DETAIL; "str_payload")]
    #[test_case(|| panic::panic_any(PANIC_DETAIL.to_owned()) => PANIC_DETAIL; "string_payload")]
    #[test_case(|| panic::panic_any(PANIC_CODE) => UNKNOWN_PANIC; "other_payload")]
    fn an_interpreter_panic_becomes_its_message(raise: fn()) -> String {
        let host = Borrowing(GREETING);
        match run_interpreter::<Greeters, ()>(THREAD_NAME, &host, |_| raise()) {
            Err(InterpreterError::Panicked(detail)) => detail,
            other => panic!("{MUST_PANIC}: {other:?}"),
        }
    }
}
