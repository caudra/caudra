//! What a ported script reaches outside itself: commands, the clock, sleeping, progress, and the
//! environment. The ports take a [`Host`] so their tests script every command and drive a manual
//! clock, and so the caller decides where commands run and how cancellation reaches a sleep.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use jiff::Timestamp;

/// What a command or a whole ported CLI ended with, as a process reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn success(stdout: impl Into<String>) -> Self {
        Self {
            code: 0,
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    pub fn failure(code: i32, stderr: impl Into<String>) -> Self {
        Self {
            code,
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }
}

/// The caller stopped the run, so a port returns at once with what it has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("cancelled")]
pub struct Cancelled;

pub trait Host: Send + Sync {
    /// The directory a relative path resolves against and commands run in by default.
    fn cwd(&self) -> &Path;
    fn env_var(&self, name: &str) -> Option<String>;
    /// Runs `argv` with stdin closed and returns its exit code and output. A program that cannot
    /// start is an `Err`; one that starts and fails is an `Ok` with its nonzero code.
    fn run(&self, argv: &[&str], cwd: Option<&Path>) -> io::Result<Output>;
    fn now(&self) -> Timestamp;
    fn sleep(&self, duration: Duration) -> Result<(), Cancelled>;
    /// One line of a long-running port's progress, such as a polling update, shown as it happens.
    fn progress(&self, line: &str);
    /// When the newest Caudra session that worked in `dir` was last active, which stands in for
    /// upstream's search of Cursor transcripts.
    fn last_session_at(&self, _dir: &Path) -> Option<Timestamp> {
        None
    }

    fn resolve(&self, path: &str) -> PathBuf {
        self.cwd().join(path)
    }
}

#[cfg(test)]
pub mod fake {
    //! A scripted [`Host`]: each command answers from a queue, the clock moves only when a port
    //! sleeps, and progress lines are kept for assertions.

    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex;

    use super::{Cancelled, Duration, Host, Output, Path, PathBuf, Timestamp, io};

    pub const UNSCRIPTED: &str = "unscripted command";

    #[derive(Default)]
    struct State {
        responses: HashMap<String, VecDeque<io::Result<Output>>>,
        ran: Vec<String>,
        progress: Vec<String>,
        slept: Vec<Duration>,
        now: Option<Timestamp>,
        cancel_after_sleeps: Option<usize>,
        sessions: HashMap<PathBuf, Timestamp>,
    }

    pub struct FakeHost {
        cwd: PathBuf,
        env: HashMap<String, String>,
        state: Mutex<State>,
    }

    impl FakeHost {
        pub fn new(cwd: impl Into<PathBuf>) -> Self {
            Self {
                cwd: cwd.into(),
                env: HashMap::new(),
                state: Mutex::new(State {
                    now: Some(Timestamp::UNIX_EPOCH),
                    ..State::default()
                }),
            }
        }

        pub fn with_env(mut self, name: &str, value: &str) -> Self {
            self.env.insert(name.to_owned(), value.to_owned());
            self
        }

        pub fn at(self, now: Timestamp) -> Self {
            self.state().now = Some(now);
            self
        }

        /// Queues what the next run of `argv`, joined by spaces, answers. A command queued
        /// several times answers in order, and its last answer repeats.
        pub fn script(&self, argv: &str, output: Output) -> &Self {
            self.state()
                .responses
                .entry(argv.to_owned())
                .or_default()
                .push_back(Ok(output));
            self
        }

        pub fn script_error(&self, argv: &str, kind: io::ErrorKind) -> &Self {
            self.state()
                .responses
                .entry(argv.to_owned())
                .or_default()
                .push_back(Err(io::Error::from(kind)));
            self
        }

        /// The sleep after `sleeps` sleeps returns [`Cancelled`].
        pub fn cancel_after(&self, sleeps: usize) -> &Self {
            self.state().cancel_after_sleeps = Some(sleeps);
            self
        }

        pub fn with_session(self, dir: impl Into<PathBuf>, at: Timestamp) -> Self {
            self.state().sessions.insert(dir.into(), at);
            self
        }

        pub fn ran(&self) -> Vec<String> {
            self.state().ran.clone()
        }

        pub fn progress_lines(&self) -> Vec<String> {
            self.state().progress.clone()
        }

        pub fn slept(&self) -> Vec<Duration> {
            self.state().slept.clone()
        }

        fn state(&self) -> std::sync::MutexGuard<'_, State> {
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }
    }

    impl Host for FakeHost {
        fn cwd(&self) -> &Path {
            &self.cwd
        }

        fn env_var(&self, name: &str) -> Option<String> {
            self.env.get(name).cloned()
        }

        fn run(&self, argv: &[&str], _cwd: Option<&Path>) -> io::Result<Output> {
            let key = argv.join(" ");
            let mut state = self.state();
            state.ran.push(key.clone());
            let Some(queue) = state.responses.get_mut(&key) else {
                return Ok(Output::failure(127, format!("{UNSCRIPTED}: {key}")));
            };
            match queue.len() {
                0 => Ok(Output::failure(127, format!("{UNSCRIPTED}: {key}"))),
                1 => match queue.front() {
                    Some(Ok(output)) => Ok(output.clone()),
                    Some(Err(error)) => Err(io::Error::from(error.kind())),
                    None => unreachable!("the queue holds one answer"),
                },
                _ => queue.pop_front().unwrap_or_else(|| unreachable!()),
            }
        }

        fn now(&self) -> Timestamp {
            self.state().now.unwrap_or(Timestamp::UNIX_EPOCH)
        }

        fn sleep(&self, duration: Duration) -> Result<(), Cancelled> {
            let mut state = self.state();
            if state.cancel_after_sleeps == Some(state.slept.len()) {
                return Err(Cancelled);
            }
            state.slept.push(duration);
            let now = state.now.unwrap_or(Timestamp::UNIX_EPOCH);
            state.now = now.checked_add(duration).ok();
            Ok(())
        }

        fn progress(&self, line: &str) {
            self.state().progress.push(line.to_owned());
        }

        fn last_session_at(&self, dir: &Path) -> Option<Timestamp> {
            self.state().sessions.get(dir).copied()
        }
    }
}
