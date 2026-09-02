use std::ffi::{OsStr, OsString};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tracing::warn;
use wait_timeout::ChildExt;

const AGENT: &str = "caudra";
const SOURCE: &str = "custom:caudra";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(1);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
const HERDR_ENV: &str = "HERDR_ENV";
const HERDR_PANE_ID: &str = "HERDR_PANE_ID";
const HERDR_BIN_PATH: &str = "HERDR_BIN_PATH";
const HERDR_SOCKET_PATH: &str = "HERDR_SOCKET_PATH";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AgentState {
    Idle,
    Working,
    Blocked,
}

impl AgentState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
        }
    }

    fn priority(self) -> u8 {
        match self {
            Self::Idle => 0,
            Self::Working => 1,
            Self::Blocked => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HerdrObservation {
    state: AgentState,
    message: Option<&'static str>,
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

    pub(crate) const fn blocked(message: &'static str) -> Self {
        Self {
            state: AgentState::Blocked,
            message: Some(message),
        }
    }
}

pub(crate) fn aggregate_observations(
    observations: impl IntoIterator<Item = HerdrObservation>,
) -> HerdrObservation {
    observations
        .into_iter()
        .fold(HerdrObservation::idle(), |selected, candidate| {
            if candidate.state.priority() > selected.state.priority() {
                candidate
            } else {
                selected
            }
        })
}

#[derive(Clone)]
struct HerdrConfig {
    binary: OsString,
    pane_id: OsString,
    socket_path: OsString,
}

impl HerdrConfig {
    fn from_env(get: impl Fn(&str) -> Option<OsString>) -> Option<Self> {
        if get(HERDR_ENV).as_deref() != Some(OsStr::new("1")) {
            return None;
        }
        Some(Self {
            binary: get(HERDR_BIN_PATH)
                .and_then(nonempty)
                .unwrap_or_else(|| OsString::from("herdr")),
            pane_id: nonempty(get(HERDR_PANE_ID)?)?,
            socket_path: nonempty(get(HERDR_SOCKET_PATH)?)?,
        })
    }
}

fn nonempty(value: OsString) -> Option<OsString> {
    (!value.is_empty()).then_some(value)
}

#[derive(Debug, PartialEq, Eq)]
struct CommandSpec {
    program: OsString,
    args: Vec<OsString>,
    socket_path: OsString,
}

impl CommandSpec {
    fn report(config: &HerdrConfig, observation: HerdrObservation, sequence: u64) -> Self {
        let mut args = vec![
            "pane".into(),
            "report-agent".into(),
            config.pane_id.clone(),
            "--source".into(),
            SOURCE.into(),
            "--agent".into(),
            AGENT.into(),
            "--state".into(),
            observation.state.as_str().into(),
            "--seq".into(),
            sequence.to_string().into(),
        ];
        if let Some(message) = observation.message {
            args.extend([OsString::from("--message"), message.into()]);
        }
        Self {
            program: config.binary.clone(),
            args,
            socket_path: config.socket_path.clone(),
        }
    }

    fn release(config: &HerdrConfig, sequence: u64) -> Self {
        Self {
            program: config.binary.clone(),
            args: vec![
                "pane".into(),
                "release-agent".into(),
                config.pane_id.clone(),
                "--source".into(),
                SOURCE.into(),
                "--agent".into(),
                AGENT.into(),
                "--seq".into(),
                sequence.to_string().into(),
            ],
            socket_path: config.socket_path.clone(),
        }
    }
}

enum WorkerAction {
    Report(HerdrObservation),
    Release,
}

#[derive(Default)]
struct Pending {
    latest: Option<HerdrObservation>,
    desired: Option<HerdrObservation>,
    closing: bool,
    released: bool,
}

struct Shared {
    pending: Mutex<Pending>,
    wake: flume::Sender<()>,
}

impl Shared {
    fn observe(&self, observation: HerdrObservation) {
        let changed = {
            let mut pending = lock(&self.pending);
            if pending.closing || pending.desired == Some(observation) {
                false
            } else {
                pending.desired = Some(observation);
                pending.latest = Some(observation);
                true
            }
        };
        if changed {
            self.wake();
        }
    }

    fn close(&self) {
        let changed = {
            let mut pending = lock(&self.pending);
            let changed = !pending.closing;
            pending.closing = true;
            changed
        };
        if changed {
            self.wake();
        }
    }

    fn next_action(&self) -> Option<WorkerAction> {
        let mut pending = lock(&self.pending);
        if let Some(observation) = pending.latest.take() {
            Some(WorkerAction::Report(observation))
        } else if pending.closing && !pending.released {
            pending.released = true;
            Some(WorkerAction::Release)
        } else {
            None
        }
    }

    fn wake(&self) {
        match self.wake.try_send(()) {
            Ok(()) | Err(flume::TrySendError::Full(())) => {}
            Err(flume::TrySendError::Disconnected(())) => {}
        }
    }
}

fn lock(pending: &Mutex<Pending>) -> std::sync::MutexGuard<'_, Pending> {
    pending.lock().unwrap_or_else(|error| error.into_inner())
}

#[derive(Clone)]
pub struct HerdrReporterHandle {
    shared: Arc<Shared>,
}

impl HerdrReporterHandle {
    pub(crate) fn observe(&self, observation: HerdrObservation) {
        self.shared.observe(observation);
    }
}

pub struct HerdrReporter {
    handle: HerdrReporterHandle,
    done_rx: flume::Receiver<()>,
    worker: Option<JoinHandle<()>>,
}

impl HerdrReporter {
    pub fn from_env() -> Option<Self> {
        let config = HerdrConfig::from_env(|name| std::env::var_os(name))?;
        Self::start(config).map_or_else(
            |error| {
                warn!(%error, "failed to start Herdr reporter");
                None
            },
            Some,
        )
    }

    fn start(config: HerdrConfig) -> std::io::Result<Self> {
        let (wake_tx, wake_rx) = flume::bounded(1);
        let (done_tx, done_rx) = flume::bounded(1);
        let shared = Arc::new(Shared {
            pending: Mutex::new(Pending::default()),
            wake: wake_tx,
        });
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::Builder::new()
            .name("herdr-reporter".into())
            .spawn(move || worker_loop(config, worker_shared, wake_rx, done_tx))?;
        Ok(Self {
            handle: HerdrReporterHandle { shared },
            done_rx,
            worker: Some(worker),
        })
    }

    pub fn handle(&self) -> HerdrReporterHandle {
        self.handle.clone()
    }

    pub fn shutdown(mut self) {
        self.finish();
    }

    fn finish(&mut self) {
        let Some(worker) = self.worker.take() else {
            return;
        };
        self.handle.shared.close();
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
        self.finish();
    }
}

fn worker_loop(
    config: HerdrConfig,
    shared: Arc<Shared>,
    wake_rx: flume::Receiver<()>,
    done_tx: flume::Sender<()>,
) {
    let mut sequence = sequence_seed();
    while wake_rx.recv().is_ok() {
        while let Some(action) = shared.next_action() {
            sequence = sequence.saturating_add(1);
            let release = matches!(action, WorkerAction::Release);
            execute(match action {
                WorkerAction::Report(observation) => {
                    CommandSpec::report(&config, observation, sequence)
                }
                WorkerAction::Release => CommandSpec::release(&config, sequence),
            });
            if release {
                let _ = done_tx.send(());
                return;
            }
        }
    }
}

fn sequence_seed() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64
}

fn execute(spec: CommandSpec) {
    let child = Command::new(&spec.program)
        .args(&spec.args)
        .env(HERDR_SOCKET_PATH, &spec.socket_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            warn!(%error, "failed to invoke Herdr");
            return;
        }
    };
    match child.wait_timeout(COMMAND_TIMEOUT) {
        Ok(Some(status)) if status.success() => {}
        Ok(Some(status)) => warn!(?status, "Herdr reporter command failed"),
        Ok(None) => {
            warn!("Herdr reporter command timed out");
            terminate(&mut child);
        }
        Err(error) => {
            warn!(%error, "failed waiting for Herdr reporter command");
            terminate(&mut child);
        }
    }
}

fn terminate(child: &mut Child) {
    if let Err(error) = child.kill() {
        warn!(%error, "failed to stop Herdr reporter command");
    }
    if let Err(error) = child.wait() {
        warn!(%error, "failed to reap Herdr reporter command");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const BLOCKER: &str = "Permission requested";

    fn config() -> HerdrConfig {
        HerdrConfig {
            binary: "/path with spaces/herdr".into(),
            pane_id: "workspace:pane".into(),
            socket_path: "/tmp/herdr socket".into(),
        }
    }

    fn env(values: &[(&str, &str)]) -> Option<HerdrConfig> {
        HerdrConfig::from_env(|name| {
            values
                .iter()
                .find_map(|(key, value)| (*key == name).then(|| OsString::from(value)))
        })
    }

    #[test]
    fn complete_environment_enables_reporting() {
        let config = env(&[
            (HERDR_ENV, "1"),
            (HERDR_PANE_ID, "workspace:pane"),
            (HERDR_BIN_PATH, "/path/herdr"),
            (HERDR_SOCKET_PATH, "/tmp/herdr.sock"),
        ])
        .unwrap();

        assert_eq!(config.binary, OsString::from("/path/herdr"));
        assert_eq!(config.pane_id, OsString::from("workspace:pane"));
        assert_eq!(config.socket_path, OsString::from("/tmp/herdr.sock"));
    }

    #[test]
    fn missing_binary_path_uses_path_lookup() {
        let config = env(&[
            (HERDR_ENV, "1"),
            (HERDR_PANE_ID, "workspace:pane"),
            (HERDR_SOCKET_PATH, "/tmp/herdr.sock"),
        ])
        .unwrap();

        assert_eq!(config.binary, OsString::from("herdr"));
    }

    #[test_case(HERDR_ENV ; "environment_marker")]
    #[test_case(HERDR_PANE_ID ; "pane_id")]
    #[test_case(HERDR_SOCKET_PATH ; "socket_path")]
    fn missing_required_environment_disables_reporting(missing: &str) {
        let values = [
            (HERDR_ENV, "1"),
            (HERDR_PANE_ID, "workspace:pane"),
            (HERDR_BIN_PATH, "/path/herdr"),
            (HERDR_SOCKET_PATH, "/tmp/herdr.sock"),
        ];
        let present = values
            .into_iter()
            .filter(|(name, _)| *name != missing)
            .collect::<Vec<_>>();

        assert!(env(&present).is_none());
    }

    #[test_case("" ; "empty")]
    #[test_case("0" ; "zero")]
    #[test_case("true" ; "word")]
    fn invalid_environment_marker_disables_reporting(marker: &str) {
        assert!(
            env(&[
                (HERDR_ENV, marker),
                (HERDR_PANE_ID, "workspace:pane"),
                (HERDR_BIN_PATH, "/path/herdr"),
                (HERDR_SOCKET_PATH, "/tmp/herdr.sock"),
            ])
            .is_none()
        );
    }

    #[test]
    fn report_command_uses_exact_shell_free_arguments() {
        let spec = CommandSpec::report(&config(), HerdrObservation::blocked(BLOCKER), 42);

        assert_eq!(spec.program, OsString::from("/path with spaces/herdr"));
        assert_eq!(
            spec.args,
            [
                "pane",
                "report-agent",
                "workspace:pane",
                "--source",
                SOURCE,
                "--agent",
                AGENT,
                "--state",
                "blocked",
                "--seq",
                "42",
                "--message",
                BLOCKER,
            ]
            .map(OsString::from)
        );
        assert_eq!(spec.socket_path, OsString::from("/tmp/herdr socket"));
    }

    #[test]
    fn release_command_uses_exact_shell_free_arguments() {
        let spec = CommandSpec::release(&config(), 43);

        assert_eq!(
            spec.args,
            [
                "pane",
                "release-agent",
                "workspace:pane",
                "--source",
                SOURCE,
                "--agent",
                AGENT,
                "--seq",
                "43",
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn observations_coalesce_and_release_is_last() {
        let (wake, _wake_rx) = flume::bounded(1);
        let shared = Shared {
            pending: Mutex::new(Pending::default()),
            wake,
        };
        shared.observe(HerdrObservation::idle());
        shared.observe(HerdrObservation::working());
        shared.observe(HerdrObservation::blocked(BLOCKER));
        shared.close();

        assert!(matches!(
            shared.next_action(),
            Some(WorkerAction::Report(HerdrObservation {
                state: AgentState::Blocked,
                message: Some(BLOCKER)
            }))
        ));
        assert!(matches!(shared.next_action(), Some(WorkerAction::Release)));
        assert!(shared.next_action().is_none());
        shared.observe(HerdrObservation::idle());
        assert!(shared.next_action().is_none());
    }

    #[test]
    fn unchanged_observation_is_suppressed() {
        let (wake, _wake_rx) = flume::bounded(1);
        let shared = Shared {
            pending: Mutex::new(Pending::default()),
            wake,
        };
        shared.observe(HerdrObservation::idle());
        assert!(matches!(
            shared.next_action(),
            Some(WorkerAction::Report(_))
        ));

        shared.observe(HerdrObservation::idle());

        assert!(shared.next_action().is_none());
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
}
