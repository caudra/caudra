#[cfg(test)]
mod conformance;
pub mod elicitation;
pub mod methods;
pub mod permissions;
pub mod server;
pub mod translate;

use std::path::PathBuf;
use std::sync::Arc;

use caudra_agent::permissions::PluginRuleStore;
use caudra_agent::prompt::ResolvedSlots;
use caudra_agent::prompt::profile::PromptProfileCatalog;
use caudra_agent::tools::ToolRegistry;
use caudra_agent::{AgentConfig, PermissionsConfig};
use caudra_config::{ModelPolicy, SnapshotsConfig};
use caudra_providers::model::Model;
use caudra_providers::{ThinkingConfig, Timeouts};
use caudra_storage::local_documents::LocalDocumentStore;
use caudra_storage::workspace_binding::StoredWorkspaceBinding;
use caudra_workspace::WorkspaceSession;

pub struct AcpParams {
    pub model: Model,
    pub timeouts: Timeouts,
    pub initial_wd: PathBuf,
    pub thinking: ThinkingConfig,
    pub prompt_profiles: Arc<PromptProfileCatalog>,
    pub system_prompt_profile_override: Option<String>,
    pub yolo: bool,
    pub model_policy: Arc<ModelPolicy>,
    pub runtime_resolver: AcpRuntimeResolver,
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
    pub snapshots: SnapshotsConfig,
    pub prompt_slots: Arc<ResolvedSlots>,
    pub plugin_rules: Arc<PluginRuleStore>,
    pub registry: Arc<ToolRegistry>,
    pub guard: Option<Box<dyn AcpRuntimeGuard>>,
    pub workspace_binding: Option<StoredWorkspaceBinding>,
    pub remote_environment: Option<caudra_agent::headless::RemoteEnvironment>,
    pub workspace_session: Option<WorkspaceSession>,
    pub remote_project_context:
        Option<Arc<caudra_agent::remote_project_context::RemoteProjectContext>>,
    pub local_documents: Option<Arc<LocalDocumentStore>>,
}

pub fn run(params: AcpParams) -> color_eyre::Result<()> {
    smol::block_on(server::serve(params))
}
