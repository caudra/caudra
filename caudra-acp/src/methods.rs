use std::path::Path;

use agent_client_protocol_schema::ProtocolVersion;
use agent_client_protocol_schema::v1::{
    AgentCapabilities, Implementation, InitializeResponse, LoadSessionResponse, McpCapabilities,
    NewSessionResponse, PromptCapabilities, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigSelectOption, SessionMode, SessionModeId, SessionModeState,
};
use caudra_agent::tools::native::plan::PlanTarget;

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub const MODE_BUILD: &str = "build";
pub const MODE_PLAN: &str = "plan";

pub const MODEL_CONFIG_ID: &str = "model";

pub fn initialize_response() -> InitializeResponse {
    InitializeResponse::new(ProtocolVersion::V1)
        .agent_capabilities(
            AgentCapabilities::new()
                .load_session(true)
                .prompt_capabilities(PromptCapabilities::new().image(true).embedded_context(true))
                .mcp_capabilities(McpCapabilities::new().http(true)),
        )
        .auth_methods(vec![])
        .agent_info(Implementation::new("caudra", VERSION))
}

pub fn mode_state(current: &str) -> SessionModeState {
    SessionModeState::new(
        SessionModeId::from(current.to_string()),
        vec![
            SessionMode::new(SessionModeId::from(MODE_BUILD.to_string()), "Build"),
            SessionMode::new(SessionModeId::from(MODE_PLAN.to_string()), "Plan"),
        ],
    )
}

pub fn new_session_response(session_id: &str) -> NewSessionResponse {
    NewSessionResponse::new(session_id.to_string()).modes(mode_state(MODE_BUILD))
}

pub fn load_session_response() -> LoadSessionResponse {
    LoadSessionResponse::new().modes(mode_state(MODE_BUILD))
}

pub fn model_config_option(current: &str, specs: &[String]) -> SessionConfigOption {
    let mut options: Vec<SessionConfigSelectOption> = specs
        .iter()
        .map(|spec| SessionConfigSelectOption::new(spec.clone(), spec.clone()))
        .collect();
    if !specs.iter().any(|spec| spec == current) {
        options.insert(
            0,
            SessionConfigSelectOption::new(current.to_string(), current.to_string()),
        );
    }
    SessionConfigOption::select(MODEL_CONFIG_ID, "Model", current.to_string(), options)
        .category(SessionConfigOptionCategory::Model)
}

pub fn mode_id_to_agent_mode(mode_id: &str, cwd: &Path) -> Option<caudra_agent::AgentMode> {
    match mode_id {
        MODE_BUILD => Some(caudra_agent::AgentMode::Build),
        MODE_PLAN => {
            let storage = caudra_storage::StateDir::resolve().ok()?;
            let plan_path = caudra_storage::plans::new_plan_path(&storage, cwd).ok()?;
            Some(caudra_agent::AgentMode::Plan(plan_path))
        }
        _ => None,
    }
}

/// Plan reuses the session's plan and allocates one only when it has none.
pub fn mode_id_to_agent_mode_for_session(
    mode_id: &str,
    cwd: &Path,
    workspace: Option<&caudra_workspace::WorkspaceSession>,
    documents: Option<&caudra_storage::local_documents::LocalDocumentStore>,
    session_id: &str,
    plan: Option<&PlanTarget>,
) -> Option<caudra_agent::AgentMode> {
    match (mode_id, plan, workspace, documents) {
        (MODE_PLAN, Some(plan), _, _) => Some(caudra_agent::AgentMode::planning(plan.clone())),
        (MODE_PLAN, None, Some(workspace), Some(documents)) => documents
            .create_plan(workspace.binding().project().key(), session_id)
            .ok()
            .map(caudra_agent::AgentMode::RemotePlan),
        _ => mode_id_to_agent_mode(mode_id, cwd),
    }
}
