//! A fork that opens in a new tab of the terminal host, where a Caudra of
//! its own resumes it, so the session it came from stays where it is.

use std::path::Path;
use std::sync::Arc;

use caudra_storage::StateDir;
use caudra_storage::sessions::{SessionError, SessionLease};
use caudra_storage::workspace_binding::StoredWorkspaceBinding;
use caudra_terminal_host::error::HostError;
use caudra_terminal_host::resume::resume_command_line;
use caudra_terminal_host::workspaces::{PaneSite, Workspaces};
use tracing::warn;

use crate::storage_writer::StorageWriter;
use crate::{AppSession, SessionTab};

const NOTICE_SEPARATOR: &str = ". ";

pub(super) enum ForkLaunch {
    /// The fork left this process, and the notice says where it went.
    Elsewhere(String),
    /// The fork opens in this process after all, with the warning why.
    Here(Box<SessionTab>, String),
}

/// Whether a Caudra in another tab can load the fork: only a local session
/// in a store that outlives this process.
pub(super) fn opens_in_host_tab(session: &AppSession, remote: bool, ephemeral: bool) -> bool {
    !remote
        && !ephemeral
        && session
            .workspace_binding()
            .is_none_or(StoredWorkspaceBinding::is_local)
}

/// Saves the fork, opens a tab in `host` and resumes the fork there. The
/// fork comes back to this process when the host fails before a Caudra in
/// the tab takes the session.
pub(super) fn open_in_host_tab(
    host: &dyn Workspaces,
    writer: &StorageWriter,
    storage: &StateDir,
    session: AppSession,
    lease: Arc<SessionLease>,
) -> ForkLaunch {
    let name = host.host();
    let session = Arc::new(session);
    if let Err(error) = writer.save_sync(Arc::clone(&session)) {
        return here(session, lease, unsaved(name, &error));
    }
    let pane = match host.pane(PaneSite::NewTab(Path::new(&session.cwd))) {
        Ok(pane) => pane,
        Err(error) => return here(session, lease, fell_back(name, &error)),
    };
    drop(lease);
    let command_line = resume_command_line(&session.id.to_string());
    let Err(error) = host.run(&pane, &command_line, Some(&session.title)) else {
        return ForkLaunch::Elsewhere(opened(name));
    };
    match SessionLease::acquire(storage, session.id) {
        Ok(lease) => here(session, Arc::new(lease), fell_back(name, &error)),
        Err(SessionError::SessionInUse { .. }) => ForkLaunch::Elsewhere(opened(name)),
        Err(lease_error) => ForkLaunch::Elsewhere(stranded(name, &error, &lease_error)),
    }
}

/// The notices there are, as one flash.
pub(super) fn joined(notices: [Option<String>; 2]) -> Option<String> {
    let notices: Vec<String> = notices.into_iter().flatten().collect();
    (!notices.is_empty()).then(|| notices.join(NOTICE_SEPARATOR))
}

fn here(session: Arc<AppSession>, lease: Arc<SessionLease>, warning: String) -> ForkLaunch {
    warn!(session = %session.id, %warning, "fork opens in this process instead of a host tab");
    ForkLaunch::Here(
        Box::new(SessionTab {
            session: Arc::unwrap_or_clone(session),
            lease,
            cursor: None,
        }),
        warning,
    )
}

fn opened(host: &str) -> String {
    format!("Opened the fork in a new {host} tab")
}

fn unsaved(host: &str, error: &SessionError) -> String {
    format!("The fork opened here because it could not be saved for a {host} tab: {error}")
}

fn fell_back(host: &str, error: &HostError) -> String {
    format!("The fork opened here because {host} could not open it in a new tab: {error}")
}

fn stranded(host: &str, error: &HostError, lease_error: &SessionError) -> String {
    format!(
        "{host} could not open the fork in a new tab ({error}), and it could not open here ({lease_error}). Open it from /sessions"
    )
}

#[cfg(test)]
mod tests {
    use std::fmt;
    use std::sync::Mutex;

    use caudra_storage::id::CaudraId;
    use caudra_storage::sessions::PermissionMode;
    use caudra_terminal_host::workspaces::{HostLocation, HostPane, Located, Opened};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::load_app_session;

    const HOST: &str = "Fake";
    const MODEL: &str = "test/model";
    const CWD: &str = "/work/app";
    const TITLE: &str = "Fix the parser (fork #1)";
    const DRAFT: &str = "edit me";
    const PANE: &str = "w1:p3";
    const REFUSAL: &str = "the host said no";
    const REMOTE_SOURCE: &str = "https://remote.example";
    const LOCAL_SOURCE: &str = "caudra:local:v1";
    const NOT_HERE: &str = "the fork should have opened in this process";
    const NOT_ELSEWHERE: &str = "the fork should have left this process";

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Outcome {
        Starts,
        PaneFails,
        RunFails,
        /// The command starts a Caudra, which takes the session, and the
        /// host still reports a failure.
        StartsButFails,
    }

    struct FakeHost {
        outcome: Outcome,
        storage: StateDir,
        calls: Mutex<Vec<String>>,
        started: Mutex<Option<SessionLease>>,
    }

    impl fmt::Debug for FakeHost {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(HOST)
        }
    }

    impl FakeHost {
        fn new(outcome: Outcome, storage: &StateDir) -> Self {
            Self {
                outcome,
                storage: storage.clone(),
                calls: Mutex::default(),
                started: Mutex::default(),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn refusal() -> HostError {
            HostError::Refused {
                host: HOST,
                code: HOST.into(),
                message: REFUSAL.into(),
            }
        }
    }

    impl Workspaces for FakeHost {
        fn host(&self) -> &'static str {
            HOST
        }

        fn names_branches(&self) -> bool {
            false
        }

        fn create(&self, _: &Path, _: Option<&str>, _: &str) -> Result<Opened, HostError> {
            unreachable!("a fork creates no worktree")
        }

        fn show(&self, _: &Path, _: &Path) -> Result<Opened, HostError> {
            unreachable!("a fork shows no worktree")
        }

        fn locate(&self, _: &Path, _: &Path) -> Result<Located, HostError> {
            unreachable!("a fork locates no worktree")
        }

        fn pane(&self, site: PaneSite<'_>) -> Result<HostPane, HostError> {
            self.calls.lock().unwrap().push(format!("{site:?}"));
            if self.outcome == Outcome::PaneFails {
                return Err(Self::refusal());
            }
            Ok(HostPane::Open {
                pane: PANE.into(),
                tab: None,
            })
        }

        fn run(
            &self,
            _: &HostPane,
            command_line: &str,
            label: Option<&str>,
        ) -> Result<(), HostError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{command_line} {label:?}"));
            match self.outcome {
                Outcome::Starts => Ok(()),
                Outcome::PaneFails => unreachable!("no pane, no command"),
                Outcome::RunFails => Err(Self::refusal()),
                Outcome::StartsButFails => {
                    let id = shell_words::split(command_line).unwrap().pop().unwrap();
                    let lease =
                        SessionLease::acquire(&self.storage, id.parse::<CaudraId>().unwrap())
                            .unwrap();
                    *self.started.lock().unwrap() = Some(lease);
                    Err(Self::refusal())
                }
            }
        }

        fn remove(&self, _: &HostLocation, _: bool) -> Result<bool, HostError> {
            unreachable!("a fork removes no worktree")
        }

        fn remove_command(&self, _: &HostLocation, _: bool) -> std::process::Command {
            unreachable!("a fork removes no worktree")
        }

        fn holds_this_pane(&self, _: &HostLocation) -> bool {
            false
        }
    }

    struct Fixture {
        _temp: TempDir,
        storage: StateDir,
        writer: StorageWriter,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = TempDir::new().unwrap();
            let storage = StateDir::from_path(temp.path().to_path_buf());
            let writer = StorageWriter::new(storage.clone(), flume::unbounded().0);
            Self {
                _temp: temp,
                storage,
                writer,
            }
        }

        fn launch(&self, host: &FakeHost) -> (CaudraId, ForkLaunch) {
            let mut session = AppSession::new(MODEL, CWD);
            session.set_title(TITLE.into());
            session.meta.input_draft = Some(DRAFT.into());
            session.meta.permission_mode = Some(PermissionMode::Yolo);
            let id = session.id;
            let lease = Arc::new(SessionLease::acquire(&self.storage, id).unwrap());
            let launch = open_in_host_tab(host, &self.writer, &self.storage, session, lease);
            (id, launch)
        }

        fn in_use(&self, id: CaudraId) -> bool {
            matches!(
                SessionLease::acquire(&self.storage, id),
                Err(SessionError::SessionInUse { .. })
            )
        }
    }

    fn expected_calls(id: CaudraId) -> Vec<String> {
        vec![
            format!("{:?}", PaneSite::NewTab(Path::new(CWD))),
            format!("{} {:?}", resume_command_line(&id.to_string()), Some(TITLE)),
        ]
    }

    #[test]
    fn a_fork_is_saved_and_resumed_in_a_new_tab() {
        let fixture = Fixture::new();
        let host = FakeHost::new(Outcome::Starts, &fixture.storage);

        let (id, launch) = fixture.launch(&host);

        let ForkLaunch::Elsewhere(notice) = launch else {
            panic!("{NOT_ELSEWHERE}");
        };
        assert_eq!(notice, opened(HOST));
        assert_eq!(host.calls(), expected_calls(id));
        let saved = load_app_session(id, &fixture.storage).unwrap();
        assert_eq!(saved.title, TITLE);
        assert_eq!(saved.meta.input_draft.as_deref(), Some(DRAFT));
        assert_eq!(saved.meta.permission_mode, Some(PermissionMode::Yolo));
        assert!(!fixture.in_use(id), "the new tab's Caudra needs the lease");
    }

    #[test_case(Outcome::PaneFails ; "no_tab")]
    #[test_case(Outcome::RunFails ; "no_command")]
    fn a_host_failure_opens_the_fork_here(outcome: Outcome) {
        let fixture = Fixture::new();
        let host = FakeHost::new(outcome, &fixture.storage);

        let (id, launch) = fixture.launch(&host);

        let ForkLaunch::Here(tab, warning) = launch else {
            panic!("{NOT_HERE}");
        };
        assert_eq!(tab.session.id, id);
        assert_eq!(tab.session.meta.input_draft.as_deref(), Some(DRAFT));
        assert_eq!(tab.session.meta.permission_mode, Some(PermissionMode::Yolo));
        assert!(warning.contains(REFUSAL), "{warning}");
        assert!(fixture.in_use(id), "this process keeps the fork's lease");
        assert!(tab.lease.validate(&fixture.storage, id).is_ok());
    }

    #[test]
    fn a_caudra_that_started_despite_the_failure_keeps_the_fork() {
        let fixture = Fixture::new();
        let host = FakeHost::new(Outcome::StartsButFails, &fixture.storage);

        let (id, launch) = fixture.launch(&host);

        let ForkLaunch::Elsewhere(notice) = launch else {
            panic!("{NOT_ELSEWHERE}");
        };
        assert_eq!(notice, opened(HOST));
        assert_eq!(host.calls(), expected_calls(id));
    }

    #[test_case(Some(REFUSAL), Some(DRAFT), Some(format!("{REFUSAL}{NOTICE_SEPARATOR}{DRAFT}")) ; "both")]
    #[test_case(None, Some(DRAFT), Some(DRAFT.to_owned()) ; "second_only")]
    #[test_case(Some(REFUSAL), None, Some(REFUSAL.to_owned()) ; "first_only")]
    #[test_case(None, None, None ; "none")]
    fn notices_share_one_flash(
        first: Option<&str>,
        second: Option<&str>,
        expected: Option<String>,
    ) {
        assert_eq!(
            joined([first.map(str::to_owned), second.map(str::to_owned)]),
            expected
        );
    }

    fn remote_session() -> AppSession {
        let local = StoredWorkspaceBinding::local_from_cwd(CWD);
        let remote: StoredWorkspaceBinding = serde_json::from_str(
            &serde_json::to_string(&local)
                .unwrap()
                .replace(LOCAL_SOURCE, REMOTE_SOURCE),
        )
        .unwrap();
        AppSession::new_with_workspace(MODEL, CWD, remote)
    }

    #[test_case(AppSession::new(MODEL, CWD), false, false, true ; "local")]
    #[test_case(remote_session(), false, false, false ; "remote_binding")]
    #[test_case(AppSession::new(MODEL, CWD), true, false, false ; "remote_app")]
    #[test_case(AppSession::new(MODEL, CWD), false, true, false ; "ephemeral_store")]
    fn only_a_local_lasting_fork_opens_in_a_host_tab(
        session: AppSession,
        remote: bool,
        ephemeral: bool,
        expected: bool,
    ) {
        assert_eq!(opens_in_host_tab(&session, remote, ephemeral), expected);
    }
}
