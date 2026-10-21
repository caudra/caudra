mod asks;
mod auth;
mod catalog;
mod child_work;
mod commands;
#[cfg(test)]
mod conformance;
mod dispatch;
#[cfg(all(test, unix))]
mod e2e;
pub mod elicitation;
mod errors;
mod lifecycle;
mod listing;
mod mcp_servers;
pub mod methods;
mod outbound;
pub mod permissions;
mod restore;
mod server;
mod session;
#[cfg(test)]
mod test_support;
pub mod translate;
mod transport;
mod turn;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use caudra_agent::permissions::PluginRuleStore;
use caudra_agent::prompt::ResolvedSlots;
use caudra_agent::prompt::profile::PromptProfileCatalog;
use caudra_agent::tools::ToolRegistry;
use caudra_agent::{AgentConfig, PermissionsConfig};
use caudra_config::decisions::DecisionsConfig;
use caudra_config::{ModelPolicy, SnapshotsConfig};
use caudra_providers::model::Model;
#[cfg(test)]
use caudra_providers::provider::Provider;
use caudra_providers::{ThinkingConfig, Timeouts};
use caudra_storage::local_documents::LocalDocumentStore;
use caudra_storage::sessions::PermissionMode;
use caudra_storage::workspace_binding::StoredWorkspaceBinding;
use caudra_workspace::WorkspaceSession;

pub use auth::{AcpAccounts, check_credentials};

pub struct AcpParams {
    /// The model a session opens on until a sign-in resolves it again.
    pub model: Model,
    pub timeouts: Timeouts,
    pub thinking: ThinkingConfig,
    pub system_prompt_profile_override: Option<String>,
    pub permission_mode: Option<PermissionMode>,
    pub model_policy: Arc<ModelPolicy>,
    pub runtime_resolver: AcpRuntimeResolver,
    pub accounts: Arc<dyn AcpAccounts>,
}

pub type AcpRuntimeResolver = Arc<
    dyn Fn(PathBuf, Option<StoredWorkspaceBinding>) -> Result<AcpRuntime, String> + Send + Sync,
>;

pub trait AcpRuntimeGuard: Send {
    fn shutdown(&mut self) -> Result<(), String>;
}

#[derive(Default)]
pub struct AcpRuntime {
    pub config: AgentConfig,
    pub permissions_config: PermissionsConfig,
    pub decisions_config: DecisionsConfig,
    pub seed_permission_mode: Option<PermissionMode>,
    pub snapshots: SnapshotsConfig,
    pub prompt_slots: Arc<ResolvedSlots>,
    /// Built from this session's own config, so one project's `builtin_skills.pstack` never
    /// reaches another session's profiles.
    pub prompt_profiles: Arc<PromptProfileCatalog>,
    pub plugin_rules: Arc<PluginRuleStore>,
    pub registry: Arc<ToolRegistry>,
    pub guard: Option<Box<dyn AcpRuntimeGuard>>,
    pub workspace_binding: Option<StoredWorkspaceBinding>,
    pub remote_environment: Option<caudra_agent::headless::RemoteEnvironment>,
    pub workspace_session: Option<WorkspaceSession>,
    pub remote_project_context:
        Option<Arc<caudra_agent::remote_project_context::RemoteProjectContext>>,
    pub local_documents: Option<Arc<LocalDocumentStore>>,
    /// Answers in place of the provider the session's model names.
    #[cfg(test)]
    pub(crate) provider: Option<Arc<dyn Provider>>,
}

pub fn run(params: AcpParams) -> color_eyre::Result<()> {
    smol::block_on(server::serve(params))
}

/// A panic elsewhere must not wedge the protocol: the guarded data is plain
/// bookkeeping that stays consistent between statements.
fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
