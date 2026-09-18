use std::env;
use std::path::{Path, PathBuf};

use caudra_agent::mcp::config::{http_endpoints, load_global_config};
use caudra_agent::tools::{RegistryError, ToolRegistry};
use caudra_config::{
    load_env_files, load_global_env_file,
    workcell::{
        RemoteWorkcellSelection, WorkcellProfileError, WorkcellSelection, WorkcellSelectionError,
        WorkcellSourceRef, load_workcell_profiles, select_workcell,
    },
};
use caudra_storage::auth::{
    WorkcellCredential, WorkcellCredentialName, WorkcellCredentialNameError,
    load_workcell_credential,
};
use caudra_storage::id::CaudraId;
use caudra_storage::local_documents::LocalDocumentStore;
use caudra_storage::remote_operation_journal::{
    RemoteOperationJournal, RemoteOperationJournalError,
};
use caudra_storage::workspace_binding::StoredWorkspaceBinding;
use caudra_storage::{StateDir, StorageError};
use caudra_workcell::{
    HostError, NamedBearerCredential, RemoteConnectionStatus, RemoteHostRegistrationError,
    RemoteWorkcellClient, RemoteWorkcellError, RemoteWorkcellHost, WorkcellHost,
};
use caudra_workspace::{
    IdentifierError, SessionBindingId, WorkspaceCapability, WorkspaceError, WorkspaceSession,
};
use std::sync::Arc;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::cli::WorkcellSelectorArgs;

const WORKCELL_CODE_WORKER_ENV: &str = "WORKCELL_MCP_CODE_WORKER";

enum WorkcellBackend {
    Embedded { _host: WorkcellHost },
    Remote(RemoteWorkcellHost),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkcellOrigin {
    Embedded,
    Loopback,
    SecureRemote,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkcellDisplay {
    pub label: String,
    pub origin: WorkcellOrigin,
    pub cwd: String,
    pub platform: String,
}

#[derive(Debug, Error)]
pub enum WorkcellRuntimeError {
    #[error("remote control requires a remote Workcell selection")]
    RemoteRequired,
    #[error("failed to load local Workcell profiles")]
    Profiles(#[source] WorkcellProfileError),
    #[error("failed to select a Workcell backend")]
    Selection(#[source] WorkcellSelectionError),
    #[error("failed to load Workcell credential '{name}'")]
    Credential {
        name: String,
        #[source]
        source: StorageError,
    },
    #[error("Workcell credential '{name}' is not stored")]
    MissingCredential { name: String },
    #[error("the reserved ephemeral Workcell credential name is not usable")]
    EphemeralCredentialName {
        #[source]
        source: WorkcellCredentialNameError,
    },
    #[error("failed to mint a Workcell session binding")]
    SessionBinding(#[source] IdentifierError),
    #[error("failed to connect to the selected remote Workcell")]
    Connection(#[source] RemoteWorkcellError),
    #[error("failed to open the durable remote Workcell operation journal")]
    RemoteJournal(#[source] RemoteOperationJournalError),
    #[error("remote Workcell workspace services are incompatible")]
    Workspace(#[source] WorkspaceError),
    #[error("failed to load remote project context")]
    ProjectContext(#[source] caudra_agent::remote_project_context::RemoteProjectContextError),
    #[error("global MCP configuration is invalid: {0}")]
    GlobalMcp(String),
    #[error("failed to register the selected remote Workcell tools")]
    RemoteRegistration(#[source] RemoteHostRegistrationError),
    #[error("failed to initialize embedded Workcell tools")]
    Embedded(#[source] HostError),
    #[error("failed to register runtime tools")]
    Registry(#[source] RegistryError),
}

pub struct WorkcellRuntime {
    backend: WorkcellBackend,
    workspace_session: Option<WorkspaceSession>,
    stored_binding: Option<StoredWorkspaceBinding>,
    local_documents: Option<Arc<LocalDocumentStore>>,
    remote_project_context: Option<Arc<caudra_agent::remote_project_context::RemoteProjectContext>>,
    display: WorkcellDisplay,
}

impl WorkcellRuntime {
    pub fn initialize(
        args: &WorkcellSelectorArgs,
        client_cwd: &Path,
        storage: &StateDir,
        registry: &ToolRegistry,
    ) -> Result<Self, WorkcellRuntimeError> {
        let selection = resolve_selection(args)?;

        match &selection {
            WorkcellSelection::Embedded => load_env_files(client_cwd),
            WorkcellSelection::Remote(_) => load_global_env_file(),
        }

        Self::initialize_selection(selection, client_cwd, storage, registry)
    }

    fn initialize_selection(
        selection: WorkcellSelection,
        client_cwd: &Path,
        storage: &StateDir,
        registry: &ToolRegistry,
    ) -> Result<Self, WorkcellRuntimeError> {
        match selection {
            WorkcellSelection::Embedded => Self::embedded(client_cwd, registry),
            WorkcellSelection::Remote(selection) => {
                let client = connect_remote(&selection, storage)?;
                let workspace = client
                    .workspace_handle()
                    .map_err(WorkcellRuntimeError::Workspace)?;
                let session_binding = client.session_binding().clone();
                let root_cursor = client.root_cursor().clone();
                let stored_binding = client.stored_binding().clone();
                let workspace_session =
                    WorkspaceSession::new(workspace, session_binding, root_cursor)
                        .map_err(WorkcellRuntimeError::Workspace)?;
                require_remote_snapshot_lifecycle(&workspace_session)
                    .map_err(WorkcellRuntimeError::Workspace)?;
                let local_documents = Arc::new(LocalDocumentStore::remote(
                    storage.clone(),
                    workspace_session.binding(),
                ));
                let remote_project_context = smol::block_on(
                    caudra_agent::remote_project_context::load_remote_project_context(
                        &workspace_session,
                    ),
                )
                .map_err(WorkcellRuntimeError::ProjectContext)?;
                for pending in client.pending_remote_operations() {
                    eprintln!(
                        "warning: remote mutations overlapping pending operation {} are blocked until it is reconciled or explicitly acknowledged",
                        pending.operation_id.as_str()
                    );
                }
                let label = match &selection.source {
                    WorkcellSourceRef::Direct => "direct remote Workcell".to_owned(),
                    WorkcellSourceRef::Profile(profile) => {
                        format!("Workcell profile '{profile}'")
                    }
                };
                let display = WorkcellDisplay {
                    label,
                    origin: if selection.endpoint.is_loopback() {
                        WorkcellOrigin::Loopback
                    } else {
                        WorkcellOrigin::SecureRemote
                    },
                    cwd: client.descriptor().cwd.display_path.as_str().to_owned(),
                    platform: format!(
                        "remote Workcell ({})",
                        client.descriptor().path_style.as_str()
                    ),
                };
                let host = RemoteWorkcellHost::new(client);
                let (global_mcp, errors) = load_global_config(client_cwd);
                if !errors.is_empty() {
                    return Err(WorkcellRuntimeError::GlobalMcp(errors.to_string()));
                }
                host.register_with_generic_mcp_endpoints(registry, http_endpoints(&global_mcp))
                    .map_err(WorkcellRuntimeError::RemoteRegistration)?;
                caudra_agent::tools::native::register_remote(
                    registry,
                    remote_project_context.skills(),
                )
                .map_err(WorkcellRuntimeError::Registry)?;
                Ok(Self {
                    backend: WorkcellBackend::Remote(host),
                    workspace_session: Some(workspace_session),
                    stored_binding: Some(stored_binding),
                    local_documents: Some(local_documents),
                    remote_project_context: Some(remote_project_context),
                    display,
                }
                .ready())
            }
        }
    }

    fn embedded(client_cwd: &Path, registry: &ToolRegistry) -> Result<Self, WorkcellRuntimeError> {
        let worker = std::env::var_os(WORKCELL_CODE_WORKER_ENV).map(PathBuf::from);
        let host = WorkcellHost::new_production(client_cwd, worker.as_deref())
            .map_err(WorkcellRuntimeError::Embedded)?;
        host.register(registry)
            .map_err(WorkcellRuntimeError::Registry)?;
        caudra_agent::tools::native::register(registry).map_err(WorkcellRuntimeError::Registry)?;
        for warning in host.warnings() {
            eprintln!("warning: {warning}");
        }
        Ok(Self {
            backend: WorkcellBackend::Embedded { _host: host },
            workspace_session: None,
            stored_binding: None,
            local_documents: None,
            remote_project_context: None,
            display: WorkcellDisplay {
                label: "embedded Workcell".to_owned(),
                origin: WorkcellOrigin::Embedded,
                cwd: client_cwd.to_string_lossy().into_owned(),
                platform: std::env::consts::OS.to_owned(),
            },
        }
        .ready())
    }

    fn ready(self) -> Self {
        tracing::debug!(
            remote = self.is_remote(),
            source = %self.display().label,
            origin = ?self.display().origin,
            workspace_session = self.workspace_session().is_some(),
            local_documents = self.local_documents().is_some(),
            connection = ?self.connection_status(),
            "Workcell runtime ready"
        );
        self
    }

    pub fn is_remote(&self) -> bool {
        matches!(self.backend, WorkcellBackend::Remote(_))
    }

    pub fn local_host(&self) -> Option<&WorkcellHost> {
        match &self.backend {
            WorkcellBackend::Embedded { _host } => Some(_host),
            WorkcellBackend::Remote(_) => None,
        }
    }

    pub fn stored_binding(&self) -> Option<&StoredWorkspaceBinding> {
        self.stored_binding.as_ref()
    }

    pub fn workspace_session(&self) -> Option<&WorkspaceSession> {
        self.workspace_session.as_ref()
    }

    pub fn local_documents(&self) -> Option<&Arc<LocalDocumentStore>> {
        self.local_documents.as_ref()
    }

    pub fn remote_project_context(
        &self,
    ) -> Option<&Arc<caudra_agent::remote_project_context::RemoteProjectContext>> {
        self.remote_project_context.as_ref()
    }

    pub fn display(&self) -> &WorkcellDisplay {
        &self.display
    }

    pub fn connection_status(&self) -> Option<RemoteConnectionStatus> {
        match &self.backend {
            WorkcellBackend::Embedded { .. } => None,
            WorkcellBackend::Remote(host) => Some(host.client().connection_status()),
        }
    }
}

fn resolve_selection(
    args: &WorkcellSelectorArgs,
) -> Result<WorkcellSelection, WorkcellRuntimeError> {
    let profiles = load_workcell_profiles().map_err(WorkcellRuntimeError::Profiles)?;
    select_workcell(
        &profiles,
        args.profile.as_deref(),
        args.endpoint.as_deref(),
        args.cwd.as_deref(),
        args.credential_ref.as_deref(),
    )
    .map_err(WorkcellRuntimeError::Selection)
}

/// A bearer supplied by the process that launched us, for endpoints whose token
/// is minted per sandbox and dies with it. Saving such a token would leave the
/// auth store full of credentials for machines that no longer exist.
const EPHEMERAL_TOKEN_ENV: &str = "CAUDRA_WORKCELL_TOKEN";
/// Names the ephemeral credential in diagnostics. It is never written to the
/// auth store, so it cannot collide with a saved credential.
const EPHEMERAL_CREDENTIAL_NAME: &str = "ephemeral";

/// The saved reference wins when both are present: an explicit selector should
/// never be silently overridden by an inherited environment variable.
fn resolve_credential(
    selection: &RemoteWorkcellSelection,
    storage: &StateDir,
) -> Result<Option<NamedBearerCredential>, WorkcellRuntimeError> {
    if let Some(reference) = selection.credential_ref.as_ref() {
        let name = reference.name();
        let credential = load_workcell_credential(storage, name)
            .map_err(|source| WorkcellRuntimeError::Credential {
                name: name.to_string(),
                source,
            })?
            .ok_or_else(|| WorkcellRuntimeError::MissingCredential {
                name: name.to_string(),
            })?;
        return Ok(Some(NamedBearerCredential::new(name.clone(), credential)));
    }

    // Selection already refused a non-loopback endpoint without a saved
    // reference, so an inherited variable can only ever reach a local sandbox.
    env::var(EPHEMERAL_TOKEN_ENV)
        .ok()
        .filter(|token| !token.is_empty())
        .map(ephemeral_credential)
        .transpose()
}

fn ephemeral_credential(token: String) -> Result<NamedBearerCredential, WorkcellRuntimeError> {
    let name = WorkcellCredentialName::new(EPHEMERAL_CREDENTIAL_NAME)
        .map_err(|source| WorkcellRuntimeError::EphemeralCredentialName { source })?;
    let credential =
        WorkcellCredential::new(token).map_err(|source| WorkcellRuntimeError::Credential {
            name: EPHEMERAL_TOKEN_ENV.to_string(),
            source,
        })?;
    Ok(NamedBearerCredential::new(name, credential))
}

fn connect_remote(
    selection: &RemoteWorkcellSelection,
    storage: &StateDir,
) -> Result<RemoteWorkcellClient, WorkcellRuntimeError> {
    let journal =
        RemoteOperationJournal::open(storage).map_err(WorkcellRuntimeError::RemoteJournal)?;
    let credential = resolve_credential(selection, storage)?;
    let binding_id = SessionBindingId::new(format!("caudra-{}", CaudraId::generate()))
        .map_err(WorkcellRuntimeError::SessionBinding)?;
    smol::block_on(RemoteWorkcellClient::connect(
        selection,
        credential,
        binding_id,
        journal,
        CancellationToken::new(),
    ))
    .map_err(WorkcellRuntimeError::Connection)
}

pub fn connect_control(
    args: &WorkcellSelectorArgs,
    storage: &StateDir,
) -> Result<WorkspaceSession, WorkcellRuntimeError> {
    let WorkcellSelection::Remote(selection) = resolve_selection(args)? else {
        return Err(WorkcellRuntimeError::RemoteRequired);
    };
    load_global_env_file();
    let client = connect_remote(&selection, storage)?;
    WorkspaceSession::new(
        client
            .workspace_handle()
            .map_err(WorkcellRuntimeError::Workspace)?,
        client.session_binding().clone(),
        client.root_cursor().clone(),
    )
    .map_err(WorkcellRuntimeError::Workspace)
}

fn require_remote_snapshot_lifecycle(workspace: &WorkspaceSession) -> Result<(), WorkspaceError> {
    for capability in [
        WorkspaceCapability::SnapshotCapture,
        WorkspaceCapability::SnapshotStatus,
        WorkspaceCapability::SnapshotPrepareRestore,
        WorkspaceCapability::SnapshotPrepareUnrevert,
        WorkspaceCapability::SnapshotAcknowledge,
        WorkspaceCapability::SnapshotPrepareCleanup,
        WorkspaceCapability::SnapshotExecute,
        WorkspaceCapability::SnapshotOperationStatus,
        WorkspaceCapability::SnapshotRelease,
        WorkspaceCapability::SnapshotDurablePerFileJournal,
    ] {
        workspace.workspace().capabilities().require(capability)?;
    }
    let services = workspace.workspace().services();
    if services.snapshot_read.is_none() || services.snapshot_mutation.is_none() {
        return Err(WorkspaceError::Unavailable);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    use caudra_agent::tools::{ToolRegistry, ToolSource};
    use caudra_config::workcell::{WorkcellProfiles, select_workcell};
    use caudra_storage::StateDir;

    use super::{
        EPHEMERAL_CREDENTIAL_NAME, WorkcellOrigin, WorkcellRuntime, WorkcellRuntimeError,
        ephemeral_credential,
    };

    const EMBEDDED_SOURCE: &str = "default Workcell runtime must register embedded canonical tools";
    const CREDENTIAL_NAME: &str = "missing-test-credential";
    const REJECTS_UNUSABLE_TOKEN: &str = "a token the transport cannot send must fail loudly";
    const SECRET_ENDPOINT: &str = "https://workcell.example/private/tenant";
    const INVALID_DISCOVER_RESPONSE: &str = "{}";
    const MAX_TEST_REQUEST_BYTES: usize = 4096;

    #[test]
    fn ephemeral_credential_uses_the_reserved_name() {
        let credential = ephemeral_credential("sandbox-token".to_owned()).unwrap();

        assert_eq!(credential.name().as_str(), EPHEMERAL_CREDENTIAL_NAME);
    }

    #[test_case::test_case("" ; "empty")]
    #[test_case::test_case("has space" ; "embedded_whitespace")]
    #[test_case::test_case("trailing\n" ; "trailing_newline")]
    fn ephemeral_credential_rejects_an_unusable_token(token: &str) {
        assert!(
            ephemeral_credential(token.to_owned()).is_err(),
            "{REJECTS_UNUSABLE_TOKEN}"
        );
    }

    #[test]
    fn embedded_default_preserves_registration_and_context() {
        let root = tempfile::tempdir().expect("tempdir");
        let registry = ToolRegistry::new();
        let runtime = WorkcellRuntime::embedded(root.path(), &registry).expect("embedded runtime");

        assert!(!runtime.is_remote());
        assert_eq!(runtime.display().origin, WorkcellOrigin::Embedded);
        assert!(runtime.workspace_session().is_none());
        assert!(runtime.local_documents().is_none());
        assert!(runtime.connection_status().is_none());
        for name in caudra_workcell::NATIVE_TOOL_NAMES {
            let registered = registry
                .get(name)
                .unwrap_or_else(|| panic!("{EMBEDDED_SOURCE}: missing {name}"));
            assert!(
                matches!(registered.source, ToolSource::Native { ref owner, trusted: true, .. } if owner.as_ref() == caudra_workcell::OWNER),
                "{EMBEDDED_SOURCE}: {name}"
            );
        }
    }

    #[test]
    fn missing_remote_credential_fails_without_registering_tools_or_leaking_endpoint() {
        let root = tempfile::tempdir().expect("tempdir");
        let storage = StateDir::from_path(root.path().join("state"));
        let registry = ToolRegistry::new();
        let selection = select_workcell(
            &WorkcellProfiles::default(),
            None,
            Some(SECRET_ENDPOINT),
            Some("project"),
            Some(&format!("credential:{CREDENTIAL_NAME}")),
        )
        .expect("valid remote selection");

        let error =
            WorkcellRuntime::initialize_selection(selection, root.path(), &storage, &registry)
                .err()
                .expect("missing credential must fail");

        assert!(matches!(
            &error,
            WorkcellRuntimeError::MissingCredential { .. }
        ));
        assert!(error.to_string().contains(CREDENTIAL_NAME));
        assert!(!format!("{error:?}").contains(SECRET_ENDPOINT));
        for name in caudra_workcell::NATIVE_TOOL_NAMES {
            assert!(registry.get(name).is_none(), "unexpected tool {name}");
        }
    }

    #[test]
    fn unavailable_remote_journal_fails_before_connecting_or_registering_tools() {
        let root = tempfile::tempdir().expect("tempdir");
        let persistent_file = root.path().join("not-a-state-directory");
        std::fs::write(&persistent_file, "occupied").expect("write persistent blocker");
        let storage = StateDir::split(root.path().join("volatile"), persistent_file);
        let registry = ToolRegistry::new();
        let selection = select_workcell(
            &WorkcellProfiles::default(),
            None,
            Some("http://127.0.0.1:9/mcp"),
            Some("project"),
            None,
        )
        .expect("valid loopback selection");

        let error =
            WorkcellRuntime::initialize_selection(selection, root.path(), &storage, &registry)
                .err()
                .expect("unavailable journal must fail");

        assert!(matches!(error, WorkcellRuntimeError::RemoteJournal(_)));
        for name in caudra_workcell::NATIVE_TOOL_NAMES {
            assert!(registry.get(name).is_none(), "unexpected tool {name}");
        }
    }

    #[test]
    fn handshake_contract_failure_registers_no_canonical_tools() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut request = [0; MAX_TEST_REQUEST_BYTES];
            let _ = stream.read(&mut request).expect("read request");
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{INVALID_DISCOVER_RESPONSE}",
                INVALID_DISCOVER_RESPONSE.len()
            )
            .expect("write response");
        });
        let root = tempfile::tempdir().expect("tempdir");
        let storage = StateDir::from_path(root.path().join("state"));
        let registry = ToolRegistry::new();
        let endpoint = format!("http://{address}");
        let selection = select_workcell(
            &WorkcellProfiles::default(),
            None,
            Some(&endpoint),
            Some("project"),
            None,
        )
        .expect("valid loopback selection");

        let error =
            WorkcellRuntime::initialize_selection(selection, root.path(), &storage, &registry)
                .err()
                .expect("invalid handshake must fail");
        server.join().expect("test server");

        assert!(matches!(error, WorkcellRuntimeError::Connection(_)));
        for name in caudra_workcell::NATIVE_TOOL_NAMES {
            assert!(registry.get(name).is_none(), "unexpected tool {name}");
        }
    }
}
