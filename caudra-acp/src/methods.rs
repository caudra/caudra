use std::path::Path;

use agent_client_protocol_schema::ProtocolVersion;
use agent_client_protocol_schema::v1::{
    AgentCapabilities, AuthMethod, Implementation, InitializeResponse, LoadSessionResponse,
    McpCapabilities, Meta, NewSessionResponse, PromptCapabilities, SessionCapabilities,
    SessionCloseCapabilities, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigSelectGroup, SessionConfigSelectOption, SessionDeleteCapabilities,
    SessionForkCapabilities, SessionListCapabilities, SessionMode, SessionModeId, SessionModeState,
    SessionResumeCapabilities,
};
use caudra_agent::tools::native::plan::PlanTarget;
use caudra_providers::ThinkingConfig;
use caudra_providers::manifest::ManifestRegistry;
use caudra_providers::model::Model;
use caudra_storage::sessions::PermissionMode;
use serde_json::{Value, json};

use crate::child_work::META_CHILD_WORK;
use crate::translate::META_NAMESPACE;
use crate::turn::META_STEERING;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const AGENT_NAME: &str = "caudra";
const AGENT_TITLE: &str = "Caudra";

pub const MODE_BUILD: &str = "build";
pub const MODE_PLAN: &str = "plan";

pub const MODEL_CONFIG_ID: &str = "model";
pub const EFFORT_CONFIG_ID: &str = "effort";
pub const MODE_CONFIG_ID: &str = "mode";
pub const PERMISSIONS_CONFIG_ID: &str = "permissions";
pub const FAST_CONFIG_ID: &str = "fast";

const MODEL_NAME: &str = "Model";
const EFFORT_NAME: &str = "Effort";
const MODE_NAME: &str = "Mode";
const PERMISSIONS_NAME: &str = "Permissions";
const PERMISSIONS_DESCRIPTION: &str =
    "When a tool call needs approval. Deny rules apply in every mode.";
const FAST_NAME: &str = "Fast mode";
const FAST_DESCRIPTION: &str = "Faster output at premium rates";
const PROVIDER_SEPARATOR: char = '/';
const MODES: [(&str, &str); 2] = [(MODE_BUILD, "Build"), (MODE_PLAN, "Plan")];

struct PermissionChoice {
    mode: PermissionMode,
    id: &'static str,
    name: &'static str,
    description: &'static str,
}

/// Ask first so a mode the session cannot name falls back to the safest.
const PERMISSION_MODES: [PermissionChoice; 3] = [
    PermissionChoice {
        mode: PermissionMode::Ask,
        id: "ask",
        name: "Ask",
        description: "Ask before each call that needs approval",
    },
    PermissionChoice {
        mode: PermissionMode::Auto,
        id: "auto",
        name: "Auto",
        description: "The decision engine screens calls before asking",
    },
    PermissionChoice {
        mode: PermissionMode::Yolo,
        id: "yolo",
        name: "YOLO",
        description: "Run every call without asking",
    },
];

/// `catalog` rides in `_meta.caudra`, so a client can offer models before it
/// opens a session.
pub fn initialize_response(auth_method: AuthMethod, catalog: Value) -> InitializeResponse {
    InitializeResponse::new(ProtocolVersion::V1)
        .agent_capabilities(
            AgentCapabilities::new()
                .load_session(true)
                .prompt_capabilities(PromptCapabilities::new().image(true).embedded_context(true))
                .mcp_capabilities(McpCapabilities::new().http(true))
                .session_capabilities(
                    SessionCapabilities::new()
                        .list(SessionListCapabilities::new())
                        .resume(SessionResumeCapabilities::new())
                        .close(SessionCloseCapabilities::new())
                        .delete(SessionDeleteCapabilities::new())
                        .fork(SessionForkCapabilities::new()),
                )
                .meta(Meta::from_iter([(
                    META_NAMESPACE.to_owned(),
                    json!({ META_STEERING: true, META_CHILD_WORK: true }),
                )])),
        )
        .auth_methods(vec![auth_method])
        .agent_info(Implementation::new(AGENT_NAME, VERSION).title(AGENT_TITLE.to_owned()))
        .meta(Meta::from_iter([(META_NAMESPACE.to_owned(), catalog)]))
}

pub fn mode_state(current: &str) -> SessionModeState {
    SessionModeState::new(
        SessionModeId::from(current.to_string()),
        MODES
            .iter()
            .map(|(id, name)| SessionMode::new(SessionModeId::from(*id), *name))
            .collect(),
    )
}

pub fn new_session_response(session_id: &str) -> NewSessionResponse {
    NewSessionResponse::new(session_id.to_string()).modes(mode_state(MODE_BUILD))
}

pub fn load_session_response() -> LoadSessionResponse {
    LoadSessionResponse::new().modes(mode_state(MODE_BUILD))
}

/// What a session's config options are built from.
pub struct SessionSettings<'a> {
    pub model: &'a Model,
    pub model_specs: &'a [String],
    pub thinking: &'a ThinkingConfig,
    pub fast: bool,
    pub mode_id: &'a str,
    pub permission_mode: &'a PermissionMode,
    /// Whether the client renders boolean options.
    pub boolean_options: bool,
}

/// Every option the session offers, always in the same order, so a client
/// that diffs the list sees only what changed.
pub fn config_options(settings: &SessionSettings<'_>) -> Vec<SessionConfigOption> {
    let model = settings.model;
    let mut options = vec![model_config_option(&model.spec(), settings.model_specs)];
    options.extend(effort_config_option(model, settings.thinking));
    options.push(mode_config_option(settings.mode_id));
    options.push(permissions_config_option(settings.permission_mode));
    if settings.boolean_options && model.supports_fast() {
        options.push(
            SessionConfigOption::boolean(FAST_CONFIG_ID, FAST_NAME, settings.fast)
                .description(FAST_DESCRIPTION)
                .category(SessionConfigOptionCategory::ModelConfig),
        );
    }
    options
}

/// Grouped by provider, each value a `provider/model` spec. The current model
/// is listed even when discovery has not reached it yet.
pub fn model_config_option(current: &str, specs: &[String]) -> SessionConfigOption {
    let listed = specs.iter().any(|spec| spec == current);
    let current_first = (!listed).then_some(current);
    let mut groups: Vec<SessionConfigSelectGroup> = Vec::new();
    for spec in current_first
        .into_iter()
        .chain(specs.iter().map(String::as_str))
    {
        let (provider, model) = split_spec(spec);
        let option = SessionConfigSelectOption::new(spec.to_owned(), model.to_owned());
        match groups
            .iter_mut()
            .find(|group| group.group.0.as_ref() == provider)
        {
            Some(group) => group.options.push(option),
            None => groups.push(SessionConfigSelectGroup::new(
                provider.to_owned(),
                provider_name(provider),
                vec![option],
            )),
        }
    }
    SessionConfigOption::select(MODEL_CONFIG_ID, MODEL_NAME, current.to_owned(), groups)
        .category(SessionConfigOptionCategory::Model)
}

/// A `provider/model` spec's provider slug and model id.
pub fn split_spec(spec: &str) -> (&str, &str) {
    spec.split_once(PROVIDER_SEPARATOR).unwrap_or(("", spec))
}

fn provider_name(slug: &str) -> &str {
    ManifestRegistry::for_slug(slug).map_or(slug, |manifest| manifest.display_name)
}

/// The levels the TUI's thinking picker offers on `model`, on the one that
/// sends what `thinking` sends. `None` for a model that cannot reason.
pub fn effort_config_option(
    model: &Model,
    thinking: &ThinkingConfig,
) -> Option<SessionConfigOption> {
    let choices = effort_levels(model);
    let current = thinking
        .matching_choice(model, &choices)
        .or_else(|| choices.first())?
        .to_string();
    let options: Vec<SessionConfigSelectOption> = choices
        .iter()
        .map(|choice| SessionConfigSelectOption::new(choice.to_string(), choice.to_string()))
        .collect();
    Some(
        SessionConfigOption::select(EFFORT_CONFIG_ID, EFFORT_NAME, current, options)
            .category(SessionConfigOptionCategory::ThoughtLevel),
    )
}

/// The levels the effort option offers on `model`, none when it cannot reason.
pub fn effort_levels(model: &Model) -> Vec<ThinkingConfig> {
    if !model.supports_thinking() {
        return Vec::new();
    }
    ThinkingConfig::choices(model)
}

/// The level `value` names on `model`, as the effort option lists it.
pub fn effort_choice(model: &Model, value: &str) -> Option<ThinkingConfig> {
    ThinkingConfig::choices(model)
        .into_iter()
        .find(|choice| choice.to_string() == value)
}

pub fn mode_config_option(current: &str) -> SessionConfigOption {
    let options: Vec<SessionConfigSelectOption> = MODES
        .iter()
        .map(|(id, name)| SessionConfigSelectOption::new(*id, *name))
        .collect();
    SessionConfigOption::select(MODE_CONFIG_ID, MODE_NAME, current.to_owned(), options)
        .category(SessionConfigOptionCategory::Mode)
}

pub fn permissions_config_option(current: &PermissionMode) -> SessionConfigOption {
    let options: Vec<SessionConfigSelectOption> = PERMISSION_MODES
        .iter()
        .map(|choice| {
            SessionConfigSelectOption::new(choice.id, choice.name).description(choice.description)
        })
        .collect();
    let current = PERMISSION_MODES
        .iter()
        .find(|choice| choice.mode == *current)
        .map_or(PERMISSION_MODES[0].id, |choice| choice.id);
    SessionConfigOption::select(PERMISSIONS_CONFIG_ID, PERMISSIONS_NAME, current, options)
        .description(PERMISSIONS_DESCRIPTION)
}

pub fn permission_mode(value: &str) -> Option<PermissionMode> {
    PERMISSION_MODES
        .iter()
        .find(|choice| choice.id == value)
        .map(|choice| choice.mode.clone())
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

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use test_case::test_case;

    use super::*;
    use crate::test_support::{FAST_SPEC, LEVELS, OFFLINE_SPEC, SELECTED_SPEC, effort_model};

    const OFF: &str = "off";
    const UNLISTED: &str = "openai/unlisted-model";

    fn settings<'a>(
        model: &'a Model,
        thinking: &'a ThinkingConfig,
        permission_mode: &'a PermissionMode,
        boolean_options: bool,
    ) -> SessionSettings<'a> {
        SessionSettings {
            model,
            model_specs: &[],
            thinking,
            fast: false,
            mode_id: MODE_BUILD,
            permission_mode,
            boolean_options,
        }
    }

    fn ids(options: &[SessionConfigOption]) -> Vec<String> {
        options.iter().map(|option| option.id.to_string()).collect()
    }

    fn select(option: &SessionConfigOption) -> (String, Vec<String>) {
        let json = serde_json::to_value(option).unwrap();
        let values = json["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|choice| choice["value"].as_str().unwrap().to_owned())
            .collect();
        (json["currentValue"].as_str().unwrap().to_owned(), values)
    }

    #[test_case(effort_model(&LEVELS, true), false, &[MODEL_CONFIG_ID, EFFORT_CONFIG_ID, MODE_CONFIG_ID, PERMISSIONS_CONFIG_ID] ; "reasoning_model")]
    #[test_case(Model { thinking_override: Some(caudra_providers::ThinkingSupport::No), ..effort_model(&LEVELS, true) }, false, &[MODEL_CONFIG_ID, MODE_CONFIG_ID, PERMISSIONS_CONFIG_ID] ; "model_without_reasoning")]
    #[test_case(Model::from_spec(FAST_SPEC).unwrap(), true, &[MODEL_CONFIG_ID, EFFORT_CONFIG_ID, MODE_CONFIG_ID, PERMISSIONS_CONFIG_ID, FAST_CONFIG_ID] ; "fast_model_for_a_boolean_client")]
    #[test_case(Model::from_spec(FAST_SPEC).unwrap(), false, &[MODEL_CONFIG_ID, EFFORT_CONFIG_ID, MODE_CONFIG_ID, PERMISSIONS_CONFIG_ID] ; "fast_model_for_a_select_only_client")]
    fn options_offer_what_the_model_and_client_support(
        model: Model,
        boolean_options: bool,
        expected: &[&str],
    ) {
        let options = config_options(&settings(
            &model,
            &ThinkingConfig::Off,
            &PermissionMode::Ask,
            boolean_options,
        ));
        assert_eq!(ids(&options), expected);
    }

    #[test_case(true, &[OFF, "low", "medium", "high"] ; "a_model_that_can_stop_offers_off")]
    #[test_case(false, &LEVELS ; "a_model_that_cannot_stop_offers_only_levels")]
    fn effort_lists_the_thinking_pickers_rows(toggle: bool, expected: &[&str]) {
        let model = effort_model(&LEVELS, toggle);
        let option = effort_config_option(&model, &ThinkingConfig::Off).unwrap();
        assert_eq!(select(&option).1, expected);
    }

    /// The level stays as asked, so it snaps per model and comes back when the
    /// model that declares it does.
    #[test_case(&LEVELS, "high" ; "snaps_to_the_top_of_a_shorter_ladder")]
    #[test_case(&["low", "medium", "high", "xhigh"], "xhigh" ; "kept_where_declared")]
    #[test_case(&["medium"], "medium" ; "lands_on_the_only_level")]
    fn effort_shows_the_level_the_model_sends(levels: &[&str], expected: &str) {
        let asked = ThinkingConfig::Effort("xhigh".into());
        let option = effort_config_option(&effort_model(levels, false), &asked).unwrap();
        assert_eq!(select(&option).0, expected);
    }

    #[test_case(PermissionMode::Ask, "ask" ; "ask")]
    #[test_case(PermissionMode::Auto, "auto" ; "auto")]
    #[test_case(PermissionMode::Yolo, "yolo" ; "yolo")]
    fn permissions_offer_every_mode_on_the_current_one(mode: PermissionMode, expected: &str) {
        let (current, values) = select(&permissions_config_option(&mode));
        assert_eq!(current, expected);
        assert_eq!(values, ["ask", "auto", "yolo"]);
        assert_eq!(permission_mode(expected), Some(mode));
    }

    fn groups(option: &SessionConfigOption) -> Value {
        let json = serde_json::to_value(option).unwrap();
        json["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|group| {
                let values: Vec<&Value> = group["options"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|choice| &choice["value"])
                    .collect();
                json!({"group": group["group"], "values": values})
            })
            .collect()
    }

    #[test_case(SELECTED_SPEC, json!([
        {"group": "openai", "values": [OFFLINE_SPEC, SELECTED_SPEC]},
        {"group": "anthropic", "values": [FAST_SPEC]},
    ]) ; "listed_model")]
    #[test_case(UNLISTED, json!([
        {"group": "openai", "values": [UNLISTED, OFFLINE_SPEC, SELECTED_SPEC]},
        {"group": "anthropic", "values": [FAST_SPEC]},
    ]) ; "unlisted_model_leads_its_provider")]
    fn models_are_grouped_by_provider(current: &str, expected: Value) {
        let specs = [OFFLINE_SPEC, FAST_SPEC, SELECTED_SPEC].map(str::to_owned);
        let option = model_config_option(current, &specs);
        assert_eq!(groups(&option), expected);
    }
}
