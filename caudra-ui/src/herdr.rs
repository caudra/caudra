use std::borrow::Cow;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use caudra_agent::herdr::{
    AgentReport, AgentState, HerdrEnv, HerdrError, HerdrPane, PaneMetadata, RESUME_COMMAND,
    command_on_path, resume_argv,
};
use caudra_storage::id::CaudraId;
use tracing::{info, warn};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HerdrObservation {
    state: AgentState,
    message: Option<Cow<'static, str>>,
}

impl HerdrObservation {
    pub(crate) const fn idle() -> Self {
        Self {
            state: AgentState::Idle,
            message: None,
        }
    }

    pub(crate) const fn working() -> Self {
        Self {
            state: AgentState::Working,
            message: None,
        }
    }

    pub(crate) fn blocked(message: impl Into<Cow<'static, str>>) -> Self {
        Self {
            state: AgentState::Blocked,
            message: Some(message.into()),
        }
    }

    fn priority(&self) -> u8 {
        match self.state {
            AgentState::Idle => 0,
            AgentState::Working => 1,
            AgentState::Blocked => 2,
        }
    }
}

pub(crate) fn aggregate_observations(
    observations: impl IntoIterator<Item = HerdrObservation>,
) -> HerdrObservation {
    observations
        .into_iter()
        .fold(HerdrObservation::idle(), |selected, candidate| {
            if candidate.priority() > selected.priority() {
                candidate
            } else {
                selected
            }
        })
}

/// What Herdr runs in this pane once it restores it after a restart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HerdrResume {
    /// Nothing outlives the process, as in an ephemeral run.
    Never,
    /// The focused session was never saved, so there is nothing to reopen and
    /// a session handed to another pane is never resumed twice.
    Fresh,
    Session(CaudraId),
}

impl HerdrResume {
    fn argv(self) -> Option<Vec<String>> {
        match self {
            Self::Never => None,
            Self::Fresh => Some(resume_argv(None)),
            Self::Session(id) => Some(resume_argv(Some(&id.to_string()))),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HerdrStatus {
    pub(crate) observation: HerdrObservation,
    pub(crate) resume: HerdrResume,
}

/// Only the newest value is ever sent, and an unchanged one is never sent twice.
struct Latest<T> {
    value: Option<T>,
    unsent: bool,
}

impl<T> Default for Latest<T> {
    fn default() -> Self {
        Self {
            value: None,
            unsent: false,
        }
    }
}

impl<T: Clone> Latest<T> {
    fn set(&mut self, value: T) {
        self.value = Some(value);
        self.unsent = true;
    }

    fn take(&mut self) -> Option<T> {
        std::mem::take(&mut self.unsent)
            .then(|| self.value.clone())
            .flatten()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Farewell {
    /// The user exited: Herdr forgets the agent and its resume command.
    Release,
    /// The process is going away for any other reason. Herdr keeps the resume
    /// command, and notices on its own when the shell gets the pane back.
    Detach,
}

#[derive(Default)]
struct Pending {
    status: Latest<HerdrStatus>,
    metadata: Latest<PaneMetadata<'static>>,
    farewell: Option<Farewell>,
}

enum WorkerAction {
    Report(HerdrStatus),
    Describe(PaneMetadata<'static>),
    Release,
    Stop,
}

struct Shared {
    pending: Mutex<Pending>,
    wake: flume::Sender<()>,
}

impl Shared {
    fn update(&self, change: impl FnOnce(&mut Pending) -> bool) {
        let changed = {
            let mut pending = lock(&self.pending);
            pending.farewell.is_none() && change(&mut pending)
        };
        if changed {
            self.wake();
        }
    }

    fn observe(&self, status: HerdrStatus) {
        self.update(|pending| {
            let changed = pending.status.value.as_ref() != Some(&status);
            if changed {
                pending.status.set(status);
            }
            changed
        });
    }

    fn describe(&self, metadata: PaneMetadata<'_>) {
        self.update(|pending| {
            let changed = pending.metadata.value.as_ref() != Some(&metadata);
            if changed {
                pending.metadata.set(metadata.into_owned());
            }
            changed
        });
    }

    fn close(&self, farewell: Farewell) {
        self.update(|pending| {
            pending.farewell = Some(farewell);
            true
        });
    }

    fn next_action(&self) -> Option<WorkerAction> {
        let mut pending = lock(&self.pending);
        match pending.farewell {
            Some(Farewell::Release) => Some(WorkerAction::Release),
            Some(Farewell::Detach) => Some(WorkerAction::Stop),
            None => pending
                .status
                .take()
                .map(WorkerAction::Report)
                .or_else(|| pending.metadata.take().map(WorkerAction::Describe)),
        }
    }

    fn wake(&self) {
        match self.wake.try_send(()) {
            Ok(()) | Err(flume::TrySendError::Full(())) => {}
            Err(flume::TrySendError::Disconnected(())) => {}
        }
    }
}

fn lock(pending: &Mutex<Pending>) -> MutexGuard<'_, Pending> {
    pending.lock().unwrap_or_else(|error| error.into_inner())
}

#[derive(Clone)]
pub struct HerdrReporterHandle {
    shared: Arc<Shared>,
}

impl HerdrReporterHandle {
    pub(crate) fn observe(&self, status: HerdrStatus) {
        self.shared.observe(status);
    }

    pub(crate) fn describe(&self, metadata: PaneMetadata<'_>) {
        self.shared.describe(metadata);
    }
}

pub struct HerdrReporter {
    handle: HerdrReporterHandle,
    done_rx: flume::Receiver<()>,
    worker: Option<JoinHandle<()>>,
}

impl HerdrReporter {
    pub fn from_env() -> Option<Self> {
        Self::start(HerdrEnv::detect()?).map_or_else(
            |error| {
                warn!(%error, "failed to start Herdr reporter");
                None
            },
            Some,
        )
    }

    fn start(env: HerdrEnv) -> io::Result<Self> {
        let (wake_tx, wake_rx) = flume::bounded(1);
        let (done_tx, done_rx) = flume::bounded(1);
        let shared = Arc::new(Shared {
            pending: Mutex::new(Pending::default()),
            wake: wake_tx,
        });
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::Builder::new()
            .name("herdr-reporter".into())
            .spawn(move || worker_loop(&env, &worker_shared, &wake_rx, &done_tx))?;
        Ok(Self {
            handle: HerdrReporterHandle { shared },
            done_rx,
            worker: Some(worker),
        })
    }

    pub fn handle(&self) -> HerdrReporterHandle {
        self.handle.clone()
    }

    /// Tells Herdr the agent left the pane. Only a deliberate exit may: the
    /// release also forgets the resume command, which is what brings the
    /// session back after a hangup or a Herdr restart. Dropping the reporter
    /// without this keeps it.
    pub fn release(mut self) {
        let Some(worker) = self.worker.take() else {
            return;
        };
        self.handle.shared.close(Farewell::Release);
        if self.done_rx.recv_timeout(SHUTDOWN_TIMEOUT).is_err() {
            warn!("Herdr reporter did not stop within {SHUTDOWN_TIMEOUT:?}");
            return;
        }
        if worker.join().is_err() {
            warn!("Herdr reporter thread panicked");
        }
    }
}

impl Drop for HerdrReporter {
    fn drop(&mut self) {
        self.handle.shared.close(Farewell::Detach);
    }
}

struct Worker {
    pane: HerdrPane,
    sequence: u64,
    resume: bool,
    metadata: bool,
}

impl Worker {
    fn next_seq(&mut self) -> u64 {
        self.sequence = self.sequence.saturating_add(1);
        self.sequence
    }

    fn report(&mut self, status: &HerdrStatus) {
        let resume = self.resume.then(|| status.resume.argv()).flatten();
        let mut report = AgentReport {
            state: status.observation.state,
            message: status.observation.message.as_deref(),
            resume: resume.as_deref(),
        };
        let seq = self.next_seq();
        match self.pane.report_agent(&report, seq) {
            Ok(()) => {}
            Err(HerdrError::Unsupported(detail)) if report.resume.is_some() => {
                warn!(%detail, "Herdr rejected the resume command, so it cannot restore this pane");
                self.resume = false;
                report.resume = None;
                let seq = self.next_seq();
                if let Err(error) = self.pane.report_agent(&report, seq) {
                    warn!(%error, "Herdr state report failed");
                }
            }
            Err(error) => warn!(%error, "Herdr state report failed"),
        }
    }

    fn describe(&mut self, metadata: &PaneMetadata<'_>) {
        if !self.metadata {
            return;
        }
        let seq = self.next_seq();
        match self.pane.report_metadata(metadata, seq) {
            Ok(()) => {}
            Err(HerdrError::Unsupported(detail)) => {
                warn!(%detail, "Herdr rejected pane metadata; the sidebar keeps its defaults");
                self.metadata = false;
            }
            Err(error) => warn!(%error, "Herdr metadata report failed"),
        }
    }

    fn release(&mut self) {
        let seq = self.next_seq();
        if let Err(error) = self.pane.release_agent(seq) {
            warn!(%error, "Herdr release failed");
        }
    }
}

fn worker_loop(
    env: &HerdrEnv,
    shared: &Shared,
    wake_rx: &flume::Receiver<()>,
    done_tx: &flume::Sender<()>,
) {
    let resume = command_on_path(RESUME_COMMAND);
    if !resume {
        info!(
            command = RESUME_COMMAND,
            "command is not on PATH, so Herdr cannot restore this pane after a restart"
        );
    }
    let mut worker = Worker {
        pane: HerdrPane::new(env),
        sequence: sequence_seed(),
        resume,
        metadata: true,
    };
    while wake_rx.recv().is_ok() {
        while let Some(action) = shared.next_action() {
            match action {
                WorkerAction::Report(status) => worker.report(&status),
                WorkerAction::Describe(metadata) => worker.describe(&metadata),
                WorkerAction::Release => {
                    worker.release();
                    let _ = done_tx.send(());
                    return;
                }
                WorkerAction::Stop => return,
            }
        }
    }
}

/// Herdr drops a report whose sequence is not above the last one it took from
/// this source, and a later process in the same pane must win.
fn sequence_seed() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const BLOCKER: &str = "Permission requested: shell";
    const TITLE: &str = "Refactor auth";
    const MODEL: &str = "claude-opus-4-5";

    fn shared() -> Shared {
        let (wake, _wake_rx) = flume::bounded(1);
        Shared {
            pending: Mutex::new(Pending::default()),
            wake,
        }
    }

    fn status(observation: HerdrObservation) -> HerdrStatus {
        HerdrStatus {
            observation,
            resume: HerdrResume::Fresh,
        }
    }

    fn metadata(context_percent: Option<u32>) -> PaneMetadata<'static> {
        PaneMetadata {
            title: TITLE.into(),
            model: MODEL.into(),
            context_percent,
        }
    }

    #[test]
    fn status_coalesces_to_the_latest_before_metadata() {
        let shared = shared();
        shared.describe(metadata(Some(1)));
        shared.observe(status(HerdrObservation::idle()));
        shared.observe(status(HerdrObservation::working()));
        shared.observe(status(HerdrObservation::blocked(BLOCKER)));

        assert!(matches!(
            shared.next_action(),
            Some(WorkerAction::Report(reported)) if reported == status(HerdrObservation::blocked(BLOCKER))
        ));
        assert!(matches!(
            shared.next_action(),
            Some(WorkerAction::Describe(described)) if described == metadata(Some(1))
        ));
        assert!(shared.next_action().is_none());
    }

    #[test]
    fn unchanged_values_are_not_sent_twice() {
        let shared = shared();
        shared.observe(status(HerdrObservation::idle()));
        shared.describe(metadata(None));
        while shared.next_action().is_some() {}

        shared.observe(status(HerdrObservation::idle()));
        shared.describe(metadata(None));

        assert!(shared.next_action().is_none());
    }

    #[test]
    fn a_new_resume_target_is_a_change() {
        let shared = shared();
        shared.observe(status(HerdrObservation::idle()));
        while shared.next_action().is_some() {}

        shared.observe(HerdrStatus {
            observation: HerdrObservation::idle(),
            resume: HerdrResume::Session(CaudraId::generate()),
        });

        assert!(matches!(
            shared.next_action(),
            Some(WorkerAction::Report(_))
        ));
    }

    #[test_case(Farewell::Release ; "release")]
    #[test_case(Farewell::Detach ; "detach")]
    fn the_first_farewell_wins_and_drops_pending_reports(first: Farewell) {
        let shared = shared();
        shared.observe(status(HerdrObservation::working()));
        shared.close(first);
        shared.close(Farewell::Release);
        shared.close(Farewell::Detach);
        shared.observe(status(HerdrObservation::idle()));

        let action = shared.next_action();

        match first {
            Farewell::Release => assert!(matches!(action, Some(WorkerAction::Release))),
            Farewell::Detach => assert!(matches!(action, Some(WorkerAction::Stop))),
        }
    }

    #[test_case(HerdrResume::Never, None ; "never")]
    #[test_case(HerdrResume::Fresh, Some(vec![RESUME_COMMAND.to_owned()]) ; "fresh")]
    fn resume_without_a_session(resume: HerdrResume, expected: Option<Vec<String>>) {
        assert_eq!(resume.argv(), expected);
    }

    #[test]
    fn resume_names_the_saved_session() {
        let id = CaudraId::generate();

        assert_eq!(
            HerdrResume::Session(id).argv(),
            Some(resume_argv(Some(&id.to_string())))
        );
    }

    #[test]
    fn aggregate_prefers_blocked_then_working() {
        assert_eq!(
            aggregate_observations([
                HerdrObservation::working(),
                HerdrObservation::blocked(BLOCKER),
                HerdrObservation::idle(),
            ]),
            HerdrObservation::blocked(BLOCKER)
        );
        assert_eq!(
            aggregate_observations([HerdrObservation::idle(), HerdrObservation::working()]),
            HerdrObservation::working()
        );
    }

    #[cfg(unix)]
    mod fake_herdr {
        use super::*;
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::path::Path;
        use tempfile::TempDir;

        const PANE: &str = "w1:p2";
        const SOCKET: &str = "/tmp/herdr.sock";
        const CALLS: &str = "calls";
        const SEPARATOR: &str = "--";

        /// Records every call, one line each, and answers `--` like a Herdr
        /// release that predates resume commands.
        fn worker(dir: &Path) -> Worker {
            let binary = dir.join("herdr");
            fs::write(
                &binary,
                format!(
                    "#!/bin/sh\necho \"$*\" >> '{}'\nfor arg in \"$@\"; do [ \"$arg\" = \"{SEPARATOR}\" ] && exit 2; done\nexit 0\n",
                    dir.join(CALLS).display()
                ),
            )
            .unwrap();
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
            Worker {
                pane: HerdrPane::new(&HerdrEnv {
                    binary: binary.into(),
                    socket_path: SOCKET.into(),
                    pane_id: PANE.into(),
                    workspace_id: None,
                }),
                sequence: 0,
                resume: true,
                metadata: true,
            }
        }

        fn calls(dir: &Path) -> Vec<Vec<String>> {
            fs::read_to_string(dir.join(CALLS))
                .unwrap()
                .lines()
                .map(|line| line.split(' ').map(str::to_owned).collect())
                .collect()
        }

        #[test]
        fn rejected_resume_is_dropped_and_the_state_still_reaches_herdr() {
            let dir = TempDir::new().unwrap();
            let mut worker = worker(dir.path());

            worker.report(&status(HerdrObservation::working()));
            worker.report(&status(HerdrObservation::idle()));

            let calls = calls(dir.path());
            let with_resume = |call: &Vec<String>| call.iter().any(|arg| arg == SEPARATOR);
            assert_eq!(calls.len(), 3, "{calls:?}");
            assert!(with_resume(&calls[0]));
            assert!(!with_resume(&calls[1]) && !with_resume(&calls[2]));
            assert!(calls[1].contains(&"working".to_owned()));
            assert!(calls[2].contains(&"idle".to_owned()));
            assert!(!worker.resume);
        }
    }
}
