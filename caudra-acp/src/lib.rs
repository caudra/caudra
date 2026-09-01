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
use caudra_agent::{AgentConfig, PermissionsConfig};
use caudra_config::ModelPolicy;
use caudra_providers::model::Model;
use caudra_providers::{ThinkingConfig, Timeouts};

pub struct AcpParams {
    pub model: Model,
    pub config: AgentConfig,
    pub permissions_config: PermissionsConfig,
    pub timeouts: Timeouts,
    pub initial_wd: PathBuf,
    pub prompt_slots: Arc<ResolvedSlots>,
    pub thinking: ThinkingConfig,
    pub prompt_profiles: Arc<PromptProfileCatalog>,
    pub system_prompt_profile_override: Option<String>,
    pub yolo: bool,
    pub model_policy: Arc<ModelPolicy>,
    pub plugin_rules: Arc<PluginRuleStore>,
}

pub fn run(params: AcpParams) -> color_eyre::Result<()> {
    smol::block_on(server::serve(params))
}
