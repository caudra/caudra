use std::time::{SystemTime, UNIX_EPOCH};

use tracing::{info, warn};

use super::api::{AgentReport, AgentState, HerdrPane};
use super::env::HerdrEnv;
use crate::error::HostError;
use crate::host::BlockingStatusSink;
use crate::resume::{RESUME_COMMAND, command_on_path};
use crate::status::{PaneMetadata, PaneState, PaneStatus};

/// Reports to the agent Herdr tracks in this pane. Herdr shows no outcome, so
/// a finished or failed turn reads as idle there.
pub(super) struct HerdrStatusSink {
    pane: HerdrPane,
    sequence: u64,
    /// Herdr takes resume commands, and `caudra` is on `PATH` to run one.
    resume: bool,
    /// Herdr takes pane metadata.
    metadata: bool,
}

impl HerdrStatusSink {
    pub(super) fn new(env: &HerdrEnv) -> Self {
        let resume = command_on_path(RESUME_COMMAND);
        if !resume {
            info!(
                command = RESUME_COMMAND,
                "command is not on PATH, so Herdr cannot restore this pane after a restart"
            );
        }
        Self {
            pane: HerdrPane::new(env),
            sequence: sequence_seed(),
            resume,
            metadata: true,
        }
    }

    fn next_seq(&mut self) -> u64 {
        self.sequence = self.sequence.saturating_add(1);
        self.sequence
    }
}

fn agent_state(state: PaneState) -> AgentState {
    match state {
        PaneState::Idle | PaneState::Done | PaneState::Error => AgentState::Idle,
        PaneState::Working => AgentState::Working,
        PaneState::Blocked(_) => AgentState::Blocked,
    }
}

impl BlockingStatusSink for HerdrStatusSink {
    fn report(&mut self, status: &PaneStatus) {
        let state = agent_state(status.state);
        let mut report = AgentReport {
            state,
            message: status
                .label
                .as_deref()
                .filter(|_| state == AgentState::Blocked),
            resume: status.resume.as_deref().filter(|_| self.resume),
        };
        let seq = self.next_seq();
        match self.pane.report_agent(&report, seq) {
            Ok(()) => {}
            Err(HostError::Unsupported(_, detail)) if report.resume.is_some() => {
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
            Err(HostError::Unsupported(_, detail)) => {
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

/// Herdr drops a report whose sequence is not above the last one it took from
/// this source, and a later process in the same pane must win.
fn sequence_seed() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::resume::resume_argv;
    use crate::status::BlockKind;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use tempfile::TempDir;
    use test_case::test_case;

    const PANE: &str = "w1:p2";
    const SOCKET: &str = "/tmp/herdr.sock";
    const CALLS: &str = "calls";
    const SEPARATOR: &str = "--";
    const BLOCKER: &str = "Permission requested: shell";
    const SESSION: &str = "4Tx8ZrCGn1j9aE6oUvWm2k";

    /// Records every call, one line each, and answers `--` like a Herdr
    /// release that predates resume commands when `old` is set.
    fn sink(dir: &Path, old: bool) -> HerdrStatusSink {
        let binary = dir.join("herdr");
        let refuse = if old {
            format!("for arg in \"$@\"; do [ \"$arg\" = \"{SEPARATOR}\" ] && exit 2; done\n")
        } else {
            String::new()
        };
        fs::write(
            &binary,
            format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\n{refuse}exit 0\n",
                dir.join(CALLS).display()
            ),
        )
        .unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        HerdrStatusSink {
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

    fn resumable(state: PaneState) -> PaneStatus {
        PaneStatus {
            resume: Some(resume_argv(Some(SESSION))),
            ..PaneStatus::new(state)
        }
    }

    fn state_of(call: &[String]) -> &str {
        let at = call.iter().position(|arg| arg == "--state").unwrap();
        &call[at + 1]
    }

    #[test]
    fn rejected_resume_is_dropped_and_the_state_still_reaches_herdr() {
        let dir = TempDir::new().unwrap();
        let mut sink = sink(dir.path(), true);

        sink.report(&resumable(PaneState::Working));
        sink.report(&resumable(PaneState::Idle));

        let calls = calls(dir.path());
        let with_resume = |call: &Vec<String>| call.iter().any(|arg| arg == SEPARATOR);
        assert_eq!(calls.len(), 3, "{calls:?}");
        assert!(with_resume(&calls[0]));
        assert!(!with_resume(&calls[1]) && !with_resume(&calls[2]));
        assert_eq!(state_of(&calls[1]), "working");
        assert_eq!(state_of(&calls[2]), "idle");
        assert!(!sink.resume);
    }

    #[test_case(PaneState::Done, "idle" ; "done_reads_as_idle")]
    #[test_case(PaneState::Error, "idle" ; "error_reads_as_idle")]
    #[test_case(PaneState::Working, "working" ; "working")]
    #[test_case(PaneState::Blocked(None), "blocked" ; "blocked")]
    fn outcomes_read_as_idle(state: PaneState, expected: &str) {
        let dir = TempDir::new().unwrap();
        let mut sink = sink(dir.path(), false);

        sink.report(&PaneStatus::new(state));

        assert_eq!(state_of(&calls(dir.path())[0]), expected);
    }

    #[test]
    fn only_a_blocked_report_carries_its_label() {
        let dir = TempDir::new().unwrap();
        let mut sink = sink(dir.path(), false);

        sink.report(&PaneStatus {
            label: Some(BLOCKER.into()),
            ..PaneStatus::new(PaneState::Blocked(Some(BlockKind::Permission)))
        });
        sink.report(&PaneStatus {
            label: Some(BLOCKER.into()),
            ..PaneStatus::new(PaneState::Working)
        });

        let calls = fs::read_to_string(dir.path().join(CALLS)).unwrap();
        let lines: Vec<_> = calls.lines().collect();
        assert!(
            lines[0].ends_with(&format!("--message {BLOCKER}")),
            "{}",
            lines[0]
        );
        assert!(!lines[1].contains("--message"), "{}", lines[1]);
    }
}
