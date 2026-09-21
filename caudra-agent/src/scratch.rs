//! Where temporary work goes, and who is allowed to write there.
//!
//! Locally this is decided at startup: `TMPDIR` is pointed at a per-project
//! directory under Caudra's scratch root, and both the environment block and
//! the pre-allow rule read back from there. That answer is wrong the moment
//! tools stop running on this machine. A remote Workcell executes every file
//! and shell call in its own host, where the local path names nothing, so the
//! directory has to be made there and the two readers have to be told about it
//! instead of about `TMPDIR`.
//!
//! [`prepare_remote`] installs that answer once, from the runtime that owns the
//! remote connection and before a permission manager or an environment block
//! exists. When it cannot, nothing is installed and nothing is advertised: a
//! model told about a directory that is not there is worse off than a model
//! told about none.

use std::sync::RwLock;
use std::time::Duration;

use caudra_storage::projects::remote_scratch_id;
use caudra_workspace::{
    CommandText, ExecRequest, OperationState, OperationStatus, WorkspaceSession,
};

/// Long enough for a round trip and two `mkdir` calls on a loaded host, short
/// enough that an unresponsive remote delays startup rather than stalling it.
const REMOTE_DEADLINE: Duration = Duration::from_secs(10);
const REMOTE_POLL_INTERVAL: Duration = Duration::from_millis(50);
const REMOTE_POLL_MAX: Duration = Duration::from_millis(500);

static STATE: RwLock<Scratch> = RwLock::new(Scratch::Local);

/// Where tools run, and what was made for them there.
///
/// `Remote(None)` is not the same as `Local`: it says the tools are elsewhere
/// and that nothing was made for them, which is the one state in which Caudra
/// has to stay quiet. Collapsing it into `Local` would hand the model the path
/// this process uses for its own temporary files, on a machine none of its
/// tools can reach.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Scratch {
    Local,
    Remote(Option<RemoteScratch>),
}

/// The two levels the remote command reports. `root` is the namespaced
/// directory under the remote host's own temp root and `project` the session's
/// share of it, keyed by the remote identity.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RemoteScratch {
    root: String,
    project: String,
}

/// The directory the model is told to put temporary work in, or `None` when
/// there is none to name.
pub fn advertised() -> Option<String> {
    match state() {
        Scratch::Local => local_root().map(|_| std::env::temp_dir().to_string_lossy().into_owned()),
        Scratch::Remote(remote) => remote.map(|remote| remote.project),
    }
}

/// The root that file-write and read tools are pre-allowed inside.
///
/// Wider than [`advertised`] by exactly one level, in both modes and for the
/// same reason: the project the session is bound to can change while the
/// directory chosen at startup cannot follow it, and a grant narrowed to one
/// project's share would start prompting for a path the model was already
/// handed. That level is Caudra's own namespace, never the shared temp root
/// holding it.
pub fn permission_root() -> Option<String> {
    match state() {
        Scratch::Local => local_root(),
        Scratch::Remote(remote) => remote.map(|remote| remote.root),
    }
}

/// The scratch part of the environment block, empty when there is nothing to
/// name. The path and the claims made about it come from one read of one
/// answer, so the block cannot describe a directory the tools cannot reach.
pub fn environment_section() -> String {
    let state = state();
    let Some(path) = (match &state {
        Scratch::Local => local_root().map(|_| std::env::temp_dir().to_string_lossy().into_owned()),
        Scratch::Remote(remote) => remote.as_ref().map(|remote| remote.project.clone()),
    }) else {
        return String::new();
    };
    let prompt = match state {
        Scratch::Local => crate::prompt::SCRATCH_LOCAL_PROMPT,
        Scratch::Remote(_) => crate::prompt::SCRATCH_REMOTE_PROMPT,
    };
    prompt.replace(crate::prompt::SCRATCH_DIR_SLOT, &path)
}

fn local_root() -> Option<String> {
    caudra_storage::paths::scratch_root().ok().map(|root| {
        caudra_storage::paths::canonicalize_clean(&root)
            .display()
            .to_string()
    })
}

fn state() -> Scratch {
    STATE
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

/// Create the session's scratch directory on the remote host and adopt it for
/// the rest of the process.
///
/// The path is not guessed. The remote shell expands its own `TMPDIR`, so the
/// temp root is whatever that host says it is, and only the two names Caudra
/// adds are supplied from here. The command prints the directory it ended up
/// with, and that printed value rather than the request is what is adopted.
///
/// Returns the directory, or `None` when the host refused, answered with
/// something unusable, or took too long. Either way the process is now in
/// remote mode, so a failure silences the local answer instead of falling back
/// to it: the caller keeps its connection, and the model is told nothing rather
/// than told about a path on a machine its tools never see.
pub async fn prepare_remote(session: &WorkspaceSession) -> Option<String> {
    let scratch = create_remote(session).await;
    if let Some(scratch) = &scratch {
        tracing::debug!(root = %scratch.root, project = %scratch.project, "remote scratch directory ready");
    }
    let project = scratch.as_ref().map(|scratch| scratch.project.clone());
    *STATE.write().unwrap_or_else(|error| error.into_inner()) = Scratch::Remote(scratch);
    project
}

async fn create_remote(session: &WorkspaceSession) -> Option<RemoteScratch> {
    let id = remote_scratch_id(session.binding());
    let namespace = caudra_storage::paths::active_app_dir_name()
        .inspect_err(|error| tracing::warn!(%error, "no scratch namespace for the remote host"))
        .ok()?;
    let project = run_remote_setup(session, &setup_command(namespace, &id)).await?;
    let Some(root) = project.strip_suffix(&format!("/{id}")) else {
        tracing::warn!(
            directory = %project,
            "remote host reported a scratch directory other than the one requested"
        );
        return None;
    };
    Some(RemoteScratch {
        root: root.to_owned(),
        project: project.clone(),
    })
}

/// A POSIX shell script, which a remote Workcell always has: the handshake
/// rejects any host that does not report a root-relative POSIX path style.
///
/// Both levels are created one at a time rather than with `mkdir -p`, and both
/// are then rejected if they turn out to be symbolic links. The parent is the
/// shared temp root, world-writable and holding a name another local user can
/// predict, so an entry planted there ahead of Caudra would otherwise redirect
/// every pre-allowed write at once. `umask` makes what is created owner-only.
///
/// `namespace` and `id` are Caudra's own alphanumeric directory names, so the
/// only value that needs quoting is the host's `TMPDIR`.
fn setup_command(namespace: &str, id: &str) -> String {
    format!(
        "umask 077; \
         t=\"${{TMPDIR:-/tmp}}\"; t=\"${{t%/}}\"; \
         r=\"$t/{namespace}\"; mkdir \"$r\" 2>/dev/null; \
         p=\"$r/{id}\"; mkdir \"$p\" 2>/dev/null; \
         [ ! -L \"$r\" ] && [ ! -L \"$p\" ] && [ -d \"$p\" ] && printf '%s' \"$p\""
    )
}

async fn run_remote_setup(session: &WorkspaceSession, command: &str) -> Option<String> {
    let service = session.workspace().services().exec.as_ref()?;
    let request = ExecRequest {
        command: CommandText::new(command).ok()?,
        timeout_ms: Some(REMOTE_DEADLINE.as_millis() as u64),
    };
    let expire = async {
        smol::Timer::after(REMOTE_DEADLINE).await;
        None
    };
    let settle = async {
        let mut status = service
            .execute(session.binding(), session.cursor(), &request)
            .await
            .inspect_err(|error| tracing::warn!(%error, "remote scratch directory was refused"))
            .ok()?;
        let mut delay = REMOTE_POLL_INTERVAL;
        loop {
            match &status.state {
                OperationState::Running | OperationState::Prepared => {}
                _ => return Some(completed_path(&status)),
            }
            smol::Timer::after(delay).await;
            delay = (delay * 2).min(REMOTE_POLL_MAX);
            status = service
                .status(session.binding(), session.cursor(), &status.handle)
                .await
                .ok()?;
        }
    };
    futures_lite::future::or(settle, expire)
        .await
        .flatten()
        .or_else(|| {
            tracing::warn!("remote host did not create a scratch directory in time");
            None
        })
}

/// A path only when the command actually succeeded. A non-zero exit, a timeout
/// or a truncated stream means the directory was not proven to exist, and an
/// unproven directory must not reach the model.
fn completed_path(status: &OperationStatus<serde_json::Value>) -> Option<String> {
    let OperationState::Completed { result, .. } = &status.state else {
        return None;
    };
    let output: RemoteExecOutput = serde_json::from_value(result.clone()).ok()?;
    if output.exit_code != Some(0) || output.timed_out || output.output_limit_exceeded {
        return None;
    }
    let path = output.stdout.trim();
    path.starts_with('/').then(|| path.to_owned())
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteExecOutput {
    exit_code: Option<i32>,
    timed_out: bool,
    output_limit_exceeded: bool,
    stdout: String,
}

/// Serializes the tests that read or write the process-wide answer and puts it
/// back afterwards, so a remote case cannot leak into a local one.
#[cfg(test)]
pub(crate) struct ScratchGuard(
    #[expect(dead_code, reason = "held for the lifetime of the guard, never read")]
    std::sync::MutexGuard<'static, ()>,
);

#[cfg(test)]
static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
impl ScratchGuard {
    pub(crate) fn local() -> Self {
        let guard = Self(TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner()));
        *STATE.write().unwrap_or_else(|error| error.into_inner()) = Scratch::Local;
        guard
    }

    /// `None` is the remote host that would not make one, which has to stay
    /// distinguishable from never having connected to a remote host at all.
    pub(crate) fn remote(created: Option<(&str, &str)>) -> Self {
        let guard = Self::local();
        *STATE.write().unwrap_or_else(|error| error.into_inner()) =
            Scratch::Remote(created.map(|(root, project)| RemoteScratch {
                root: root.to_owned(),
                project: project.to_owned(),
            }));
        guard
    }
}

#[cfg(test)]
impl Drop for ScratchGuard {
    fn drop(&mut self) {
        *STATE.write().unwrap_or_else(|error| error.into_inner()) = Scratch::Local;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use caudra_workspace::{
        AuthenticatedPrincipalId, AuthorityIdentity, CancellationResult, CwdHandle, OperationError,
        OperationHandle, OperationId, OperationPhase, ProjectIdentity, ProjectKey, ResourceId,
        ResourceScope, SequenceMetadata, SessionBindingId, SessionWorkspaceBinding,
        SourceTrustAnchor, WorkspaceCapabilities, WorkspaceCapability, WorkspaceCursor,
        WorkspaceError, WorkspaceExecService, WorkspaceHandle, WorkspaceServices,
    };
    use serde_json::json;
    use test_case::test_case;

    use super::{
        ExecRequest, OperationState, OperationStatus, ScratchGuard, WorkspaceSession, advertised,
        environment_section, permission_root, prepare_remote, remote_scratch_id, setup_command,
    };

    const NAMESPACE: &str = "caudra";
    const CREATED_ROOT: &str = "/var/folders/xy/caudra";
    const CREATED: &str = "/var/folders/xy/caudra/remote-abc";
    const FAILURE: &str = "boom";
    const TMPDIR_CLAIM: &str = "is where `TMPDIR` points";
    const ADVERTISED_IS_CREATED: &str = "the advertised path must be the one the host printed";
    const ALLOWED_COVERS_ADVERTISED: &str =
        "the pre-allowed root must contain the advertised directory";
    const NOTHING_ADVERTISED: &str = "a remote path that was not created must not be advertised";
    const NOTHING_ALLOWED: &str = "a directory that was not created must not be pre-allowed";
    const NOTHING_IN_THE_BLOCK: &str = "with no usable directory the block must name none";
    const NO_STATUS_POLL: &str = "a terminal execute must not be polled again";
    const LOCAL_CLAIM: &str = "the local block must say temporary files already land there";
    const REMOTE_CLAIM: &str = "the remote block must not claim TMPDIR points at the directory";

    struct Exec {
        result: Mutex<Result<OperationStatus<serde_json::Value>, WorkspaceError>>,
        commands: Mutex<Vec<String>>,
        status_calls: AtomicUsize,
    }

    #[async_trait]
    impl WorkspaceExecService for Exec {
        async fn execute(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            request: &ExecRequest,
        ) -> Result<OperationStatus<serde_json::Value>, WorkspaceError> {
            self.commands
                .lock()
                .unwrap()
                .push(request.command.as_str().to_owned());
            self.result.lock().unwrap().clone()
        }

        async fn status(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            _operation: &OperationHandle,
        ) -> Result<OperationStatus<serde_json::Value>, WorkspaceError> {
            self.status_calls.fetch_add(1, Ordering::SeqCst);
            Err(WorkspaceError::Unavailable)
        }

        async fn cancel(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            _operation: &OperationHandle,
        ) -> Result<CancellationResult, WorkspaceError> {
            Ok(CancellationResult {
                state: OperationPhase::Running,
                cancellation_requested: true,
            })
        }
    }

    fn exec(result: Result<OperationStatus<serde_json::Value>, WorkspaceError>) -> Arc<Exec> {
        Arc::new(Exec {
            result: Mutex::new(result),
            commands: Mutex::new(Vec::new()),
            status_calls: AtomicUsize::new(0),
        })
    }

    fn status(state: OperationState<serde_json::Value>) -> OperationStatus<serde_json::Value> {
        OperationStatus {
            handle: OperationHandle {
                preparation_id: OperationId::new("preparation").unwrap(),
                invocation_id: Some(OperationId::new("invocation").unwrap()),
                execution_id: Some(OperationId::new("execution").unwrap()),
                expires_at_unix_ms: None,
            },
            state,
            progress: Vec::new(),
            progress_metadata: SequenceMetadata {
                first_retained_sequence: None,
                next_sequence: 0,
                gap_before_first: false,
            },
        }
    }

    fn completed(stdout: &str, exit_code: i32) -> OperationState<serde_json::Value> {
        OperationState::Completed {
            result: json!({
                "exitCode": exit_code,
                "signal": null,
                "timedOut": false,
                "outputLimitExceeded": false,
                "stdout": stdout,
                "stderr": "",
            }),
            side_effects_possible: true,
        }
    }

    fn session(service: Arc<Exec>) -> WorkspaceSession {
        let authority = AuthorityIdentity::new(
            SourceTrustAnchor::new("test-source").unwrap(),
            "authority",
            "workspace",
            "generation",
            "namespace",
        )
        .unwrap();
        let binding = SessionWorkspaceBinding::new(
            SessionBindingId::new("tab").unwrap(),
            authority.clone(),
            AuthenticatedPrincipalId::new(authority.clone(), "principal").unwrap(),
            ProjectIdentity::new(authority.clone(), ProjectKey::new("project").unwrap()),
        )
        .unwrap();
        let cursor = WorkspaceCursor::new(
            &binding,
            ResourceScope::root(ResourceId::new("root").unwrap()),
            1,
            CwdHandle::new("remote-cwd").unwrap(),
        );
        let handle = WorkspaceHandle::new(
            authority,
            WorkspaceCapabilities::from([
                WorkspaceCapability::ExecExecute,
                WorkspaceCapability::ExecStatus,
                WorkspaceCapability::ExecCancel,
            ]),
            WorkspaceServices {
                exec: Some(service),
                ..Default::default()
            },
        )
        .unwrap();
        WorkspaceSession::new(handle, binding, cursor).unwrap()
    }

    /// Both levels are made one at a time and both are rejected if they turn
    /// out to be links, because the parent is a world-writable temp root where
    /// another local user can plant the name first. The temp root itself is the
    /// remote shell's, never one this process resolved.
    #[test_case("mkdir -p" => false ; "levels_are_not_created_through_a_link")]
    #[test_case("[ ! -L \"$r\" ]" => true ; "namespace_level_refuses_a_link")]
    #[test_case("[ ! -L \"$p\" ]" => true ; "project_level_refuses_a_link")]
    #[test_case("${TMPDIR:-/tmp}" => true ; "temp_root_is_the_remote_hosts")]
    #[test_case("umask 077" => true ; "directories_are_owner_only")]
    fn the_setup_command(fragment: &str) -> bool {
        setup_command(NAMESPACE, "remote-abc").contains(fragment)
    }

    /// The advertised directory is the one the remote host printed, and the
    /// pre-allowed root is the level above it rather than the shared temp root
    /// holding that. The printed path carries the session's own directory name,
    /// which is what lets the two levels be told apart without asking again.
    #[test]
    fn a_created_remote_directory_is_what_gets_advertised_and_allowed() {
        let _guard = ScratchGuard::local();
        let service = exec(Err(WorkspaceError::Unavailable));
        let session = session(Arc::clone(&service));
        let created = format!("{CREATED_ROOT}/{}", remote_scratch_id(session.binding()));
        *service.result.lock().unwrap() = Ok(status(completed(&created, 0)));

        let prepared = smol::block_on(prepare_remote(&session));

        assert_eq!(prepared.as_deref(), Some(created.as_str()));
        assert_eq!(
            advertised().as_deref(),
            Some(created.as_str()),
            "{ADVERTISED_IS_CREATED}"
        );
        assert_eq!(
            permission_root().as_deref(),
            Some(CREATED_ROOT),
            "{ALLOWED_COVERS_ADVERTISED}"
        );
        assert!(environment_section().contains(&created));
        assert_eq!(
            service.status_calls.load(Ordering::SeqCst),
            0,
            "{NO_STATUS_POLL}"
        );
        assert_eq!(service.commands.lock().unwrap().len(), 1);
    }

    /// Every way the creation can fail ends the same way. Nothing is
    /// advertised and nothing is pre-allowed, and in particular the local
    /// answer does not come back: the tools are on the remote host either way,
    /// and this machine's temp directory is not a place any of them can write.
    #[test_case(Err(WorkspaceError::PolicyDenied) ; "host_refused_the_command")]
    #[test_case(Ok(status(completed(CREATED, 1))) ; "command_exited_non_zero")]
    #[test_case(Ok(status(completed("", 0))) ; "command_printed_nothing")]
    #[test_case(Ok(status(completed("relative/path", 0))) ; "path_is_not_absolute")]
    #[test_case(Ok(status(completed("/var/tmp/caudra/other", 0))) ; "path_is_not_the_requested_one")]
    #[test_case(Ok(status(OperationState::Failed {
        error: OperationError {
            code: OperationId::new("failed").unwrap(),
            message: FAILURE.into(),
        },
        side_effects_possible: true,
    })) ; "operation_failed")]
    #[test_case(Ok(status(OperationState::Cancelled { side_effects_possible: false })) ; "operation_cancelled")]
    fn a_creation_that_did_not_happen_is_not_advertised(
        result: Result<OperationStatus<serde_json::Value>, WorkspaceError>,
    ) {
        let _guard = ScratchGuard::local();

        let prepared = smol::block_on(prepare_remote(&session(exec(result))));

        assert_eq!(prepared, None);
        assert_eq!(advertised(), None, "{NOTHING_ADVERTISED}");
        assert_eq!(permission_root(), None, "{NOTHING_ALLOWED}");
        assert_eq!(environment_section(), "", "{NOTHING_IN_THE_BLOCK}");
    }

    /// Without a remote host both readers stay on the local answers, which are
    /// what they were before remote mode had any say in it.
    #[test]
    fn local_mode_names_the_temp_directory_and_allows_the_scratch_root() {
        let _guard = ScratchGuard::local();

        assert_eq!(
            advertised(),
            Some(std::env::temp_dir().to_string_lossy().into_owned())
        );
        assert_eq!(
            permission_root(),
            Some(
                caudra_storage::paths::canonicalize_clean(
                    &caudra_storage::paths::scratch_root().unwrap()
                )
                .display()
                .to_string()
            )
        );
        assert!(
            environment_section().contains(TMPDIR_CLAIM),
            "{LOCAL_CLAIM}"
        );
    }

    /// The two fragments differ in the one claim that is only true locally, so
    /// carrying it onto a remote host would be the lie worth catching.
    #[test]
    fn the_remote_block_does_not_claim_the_temp_variable_points_at_the_directory() {
        let _guard = ScratchGuard::remote(Some((CREATED_ROOT, CREATED)));

        let section = environment_section();

        assert!(section.contains(CREATED));
        assert!(!section.contains(TMPDIR_CLAIM), "{REMOTE_CLAIM}");
    }
}
