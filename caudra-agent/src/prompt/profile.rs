use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};

use caudra_automation::args::{ArgsError, MAX_ARGS_JSON_BYTES};
use caudra_automation::meta::{INVALID_NAME, is_valid_name};
use caudra_config::{ModelPolicy, ProfileToolPolicy};
use caudra_providers::model_registry::Binding;
use caudra_providers::{
    AgentError, Model, ModelError, ModelPurpose, ThinkingConfig, Timeouts, provider,
};
use caudra_storage::thinking::{StoredThinking, ThinkingParseError};
use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;

use crate::agent::{resolve_purpose_model, resolve_purpose_model_for_inspection};

pub const BUILTIN_PROFILE_NAME: &str = "builtin";

pub(crate) const PROFILE_DIR: &str = "system-prompts";
const MAX_PROFILE_BYTES: usize = 64 * 1024;
const MAX_PROFILE_NAME_BYTES: usize = 64;
const MAX_TASK_SUMMARY_ENTRIES: usize = 20;
const MAX_TASK_SUMMARY_DESCRIPTION_BYTES: usize = 160;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptProfileLayout {
    #[default]
    Overlay,
    Custom,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Frontmatter {
    description: Option<String>,
    layout: PromptProfileLayout,
    subagent_model: Option<String>,
    subagent_thinking: Option<FrontmatterThinking>,
    tools: ProfileToolPolicy,
    automations: Vec<FrontmatterAutomation>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum FrontmatterThinking {
    Budget(u32),
    Mode(String),
}

impl FrontmatterThinking {
    fn parse(self) -> Result<StoredThinking, ThinkingParseError> {
        match self {
            Self::Budget(tokens) => StoredThinking::parse_setting(&tokens.to_string()),
            Self::Mode(mode) => StoredThinking::parse_setting(&mode),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(
    untagged,
    expecting = "expected an automation name, or a map with a name and optional args"
)]
enum FrontmatterAutomation {
    Name(String),
    Configured(ProfileAutomation),
}

/// An automation the profile arms. `args` seed the session's binding only the first time the
/// profile arms it, so later edits survive.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileAutomation {
    pub name: String,
    pub args: Option<Value>,
}

impl From<FrontmatterAutomation> for ProfileAutomation {
    fn from(automation: FrontmatterAutomation) -> Self {
        match automation {
            FrontmatterAutomation::Name(name) => Self { name, args: None },
            FrontmatterAutomation::Configured(automation) => automation,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SystemPromptProfile {
    name: Arc<str>,
    description: Option<Arc<str>>,
    layout: PromptProfileLayout,
    subagent_model: Option<Binding>,
    subagent_thinking: Option<StoredThinking>,
    tools: ProfileToolPolicy,
    automations: Vec<ProfileAutomation>,
    body: Arc<str>,
    path: Arc<Path>,
}

impl SystemPromptProfile {
    pub fn tools(&self) -> &ProfileToolPolicy {
        &self.tools
    }

    pub fn automations(&self) -> &[ProfileAutomation] {
        &self.automations
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    pub fn layout(&self) -> PromptProfileLayout {
        self.layout
    }

    pub fn subagent_model(&self) -> Option<&Binding> {
        self.subagent_model.as_ref()
    }

    pub fn subagent_thinking(&self) -> Option<&StoredThinking> {
        self.subagent_thinking.as_ref()
    }

    pub fn body(&self) -> &str {
        &self.body
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[derive(Debug, Clone, Default)]
pub struct PromptProfileCatalog {
    profiles: BTreeMap<Arc<str>, Arc<SystemPromptProfile>>,
    invalid: BTreeMap<Arc<str>, Arc<str>>,
}

#[derive(Debug, Clone)]
pub struct TaskProfileBindings {
    available: BTreeMap<Arc<str>, Arc<SystemPromptProfile>>,
    disabled: BTreeMap<Arc<str>, Arc<str>>,
}

impl TaskProfileBindings {
    pub fn resolve(
        &self,
        name: &str,
    ) -> Result<Option<Arc<SystemPromptProfile>>, PromptProfileSelectionError> {
        if name == BUILTIN_PROFILE_NAME {
            return Ok(None);
        }
        if let Some(profile) = self.available.get(name) {
            return Ok(Some(Arc::clone(profile)));
        }
        if let Some(reason) = self.disabled.get(name) {
            return Err(PromptProfileSelectionError::Unavailable {
                name: name.to_owned(),
                reason: reason.to_string(),
            });
        }
        Err(PromptProfileSelectionError::NotFound {
            name: name.to_owned(),
            available: self
                .available
                .keys()
                .map(|name| name.as_ref())
                .collect::<Vec<_>>()
                .join(", "),
        })
    }

    pub fn task_tool_summary(&self, builtin_description: &str) -> String {
        task_tool_summary(
            self.available.values().map(AsRef::as_ref),
            self.available.len(),
            Some(builtin_description),
        )
    }

    pub fn available(&self) -> impl Iterator<Item = &SystemPromptProfile> {
        self.available.values().map(AsRef::as_ref)
    }

    pub fn disabled(&self) -> impl Iterator<Item = (&str, &str)> {
        self.disabled
            .iter()
            .map(|(name, reason)| (name.as_ref(), reason.as_ref()))
    }
}

impl PromptProfileCatalog {
    pub fn discover_user() -> Self {
        Self::discover_with(caudra_storage::paths::config_dir().ok().as_deref())
    }

    pub(crate) fn discover_with(config_dir: Option<&Path>) -> Self {
        let mut catalog = Self::default();
        if let Some(dir) = caudra_storage::paths::user_config_dir(config_dir, PROFILE_DIR) {
            catalog.load_dir(&dir);
        }
        catalog
    }

    pub fn profiles(&self) -> impl Iterator<Item = &SystemPromptProfile> {
        self.profiles.values().map(AsRef::as_ref)
    }

    pub fn get(&self, name: &str) -> Option<Arc<SystemPromptProfile>> {
        self.profiles.get(name).cloned()
    }

    pub fn resolve(
        &self,
        name: Option<&str>,
    ) -> Result<Option<Arc<SystemPromptProfile>>, PromptProfileSelectionError> {
        let Some(name) = name.filter(|name| *name != BUILTIN_PROFILE_NAME) else {
            return Ok(None);
        };
        validate_profile_name(name).map_err(|_| PromptProfileSelectionError::InvalidName {
            name: name.to_owned(),
        })?;
        if let Some(profile) = self.get(name) {
            return Ok(Some(profile));
        }
        if let Some(reason) = self.invalid.get(name) {
            return Err(PromptProfileSelectionError::InvalidProfile {
                name: name.to_owned(),
                reason: reason.to_string(),
            });
        }
        Err(PromptProfileSelectionError::NotFound {
            name: name.to_owned(),
            available: self
                .profiles
                .keys()
                .map(|name| name.as_ref())
                .collect::<Vec<_>>()
                .join(", "),
        })
    }

    /// A stable, bounded Markdown list for embedding in a task tool description.
    /// Passing a description includes the virtual `builtin` profile first.
    pub fn task_tool_summary(&self, builtin_description: Option<&str>) -> String {
        task_tool_summary(
            self.profiles.values().map(AsRef::as_ref),
            self.profiles.len(),
            builtin_description,
        )
    }

    pub fn bind_for_tasks(
        &self,
        parent_model: &Model,
        chat_model: &Model,
        parent_thinking: &ThinkingConfig,
        model_policy: &ModelPolicy,
        timeouts: Timeouts,
    ) -> TaskProfileBindings {
        self.bind_tasks_with(|profile| {
            validate_task_profile(
                profile,
                parent_thinking,
                |binding| {
                    resolve_purpose_model(
                        ModelPurpose::Subagent,
                        binding,
                        parent_model,
                        chat_model,
                        model_policy,
                    )
                },
                |model| provider::adjust_model(model, timeouts),
            )
            .inspect_err(|reason| warn_task_profile_once(profile.name(), parent_model, reason))
        })
    }

    pub fn bind_for_tasks_for_inspection(
        &self,
        parent_model: &Model,
        chat_model: &Model,
        parent_thinking: &ThinkingConfig,
        model_policy: &ModelPolicy,
    ) -> TaskProfileBindings {
        self.bind_tasks_with(|profile| {
            validate_task_profile(
                profile,
                parent_thinking,
                |binding| {
                    resolve_purpose_model_for_inspection(
                        ModelPurpose::Subagent,
                        binding,
                        parent_model,
                        chat_model,
                        model_policy,
                    )
                },
                |model| {
                    provider::adjust_model_for_inspection(model);
                    Ok(())
                },
            )
        })
    }

    fn bind_tasks_with(
        &self,
        validate: impl Fn(&SystemPromptProfile) -> Result<(), String>,
    ) -> TaskProfileBindings {
        let mut available = BTreeMap::new();
        let mut disabled = BTreeMap::new();
        for (name, profile) in &self.profiles {
            match validate(profile) {
                Ok(()) => {
                    available.insert(Arc::clone(name), Arc::clone(profile));
                }
                Err(reason) => {
                    disabled.insert(Arc::clone(name), Arc::from(reason));
                }
            }
        }
        TaskProfileBindings {
            available,
            disabled,
        }
    }

    fn load_dir(&mut self, dir: &Path) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        let mut paths = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "md"))
            .filter(|path| fs::metadata(path).is_ok_and(|metadata| metadata.is_file()))
            .collect::<Vec<_>>();
        paths.sort();

        for path in paths {
            let Some(name) = path.file_stem().and_then(|name| name.to_str()) else {
                continue;
            };
            if self.profiles.contains_key(name) || self.invalid.contains_key(name) {
                continue;
            }
            let name: Arc<str> = Arc::from(name);
            match load_profile(&path, Arc::clone(&name)) {
                Ok(profile) => {
                    self.profiles.insert(name, Arc::new(profile));
                }
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "ignoring invalid system prompt profile");
                    self.invalid.insert(name, Arc::from(error.to_string()));
                }
            }
        }
    }
}

fn task_tool_summary<'a>(
    profiles: impl Iterator<Item = &'a SystemPromptProfile>,
    profile_count: usize,
    builtin_description: Option<&str>,
) -> String {
    let builtin_entries = usize::from(builtin_description.is_some());
    let profile_limit = MAX_TASK_SUMMARY_ENTRIES.saturating_sub(builtin_entries);
    let mut lines = Vec::with_capacity(MAX_TASK_SUMMARY_ENTRIES + 1);
    if let Some(description) = builtin_description {
        lines.push(format!(
            "- `{BUILTIN_PROFILE_NAME}`: {}",
            bounded_summary_description(description)
        ));
    }
    lines.extend(profiles.take(profile_limit).map(|profile| {
        let mut line = format!("- `{}`", profile.name());
        if let Some(description) = profile.description() {
            line.push_str(": ");
            line.push_str(&bounded_summary_description(description));
        }
        line
    }));
    let omitted = profile_count.saturating_sub(profile_limit);
    if omitted > 0 {
        lines.push(format!("- ... and {omitted} more"));
    }
    lines.join("\n")
}

fn validate_task_profile(
    profile: &SystemPromptProfile,
    parent_thinking: &ThinkingConfig,
    resolve_model: impl FnOnce(Option<&Binding>) -> Result<Model, ModelError>,
    adjust_model: impl FnOnce(&mut Model) -> Result<(), AgentError>,
) -> Result<(), String> {
    if profile.subagent_model().is_none() && profile.subagent_thinking().is_none() {
        return Ok(());
    }
    let binding = profile.subagent_model();
    let mut model = resolve_model(binding).map_err(|error| match binding {
        Some(binding) => format!("subagent model {binding:?} is unavailable: {error}"),
        None => format!("global subagent model is unavailable: {error}"),
    })?;
    adjust_model(&mut model)
        .map_err(|error| format!("cannot inspect subagent model {:?}: {error}", model.spec()))?;
    let thinking = profile
        .subagent_thinking()
        .cloned()
        .map(ThinkingConfig::from)
        .unwrap_or_else(|| parent_thinking.clone());
    thinking.resolve_exact(&model).map_err(|error| {
        format!(
            "subagent thinking {thinking:?} is incompatible with model {:?}: {error}",
            model.spec()
        )
    })?;
    Ok(())
}

fn warn_task_profile_once(name: &str, model: &Model, reason: &str) {
    static WARNED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);
    let key = format!("{name}\0{}\0{reason}", model.spec());
    let mut warned = WARNED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if warned.insert(key) {
        tracing::warn!(profile = name, model = %model.spec(), reason, "subagent profile disabled");
    }
}

#[derive(Debug, Error)]
pub enum PromptProfileSelectionError {
    #[error(
        "invalid system prompt profile name {name:?}; expected 1-64 ASCII letters, digits, '-' or '_'"
    )]
    InvalidName { name: String },
    #[error("system prompt profile {name:?} is invalid: {reason}")]
    InvalidProfile { name: String, reason: String },
    #[error("system prompt profile {name:?} is unavailable for subagents: {reason}")]
    Unavailable { name: String, reason: String },
    #[error("system prompt profile {name:?} not found{suffix}", suffix = available_suffix(.available))]
    NotFound { name: String, available: String },
}

fn available_suffix(available: &str) -> String {
    if available.is_empty() {
        "; no user profiles were found".to_owned()
    } else {
        format!("; available profiles: {available}")
    }
}

#[derive(Debug, Error)]
enum PromptProfileError {
    #[error("invalid profile name")]
    InvalidName,
    #[error("the profile name 'builtin' is reserved")]
    ReservedName,
    #[error("cannot read profile: {0}")]
    Read(#[from] std::io::Error),
    #[error("profile exceeds the {MAX_PROFILE_BYTES}-byte limit")]
    TooLarge,
    #[error("profile is not valid UTF-8")]
    InvalidUtf8,
    #[error("frontmatter is not closed with '---'")]
    UnclosedFrontmatter,
    #[error("invalid frontmatter: {0}")]
    InvalidFrontmatter(#[from] serde_yaml::Error),
    #[error(
        "invalid subagent model {model:?}; expected qualified provider/model syntax or one of: chat, plan, fast, best"
    )]
    InvalidSubagentModel { model: String },
    #[error("invalid subagent thinking: {0}")]
    InvalidSubagentThinking(#[from] ThinkingParseError),
    #[error("profile body is empty")]
    Empty,
    #[error("unknown template directive {directive:?} on line {line}")]
    UnknownDirective { directive: String, line: usize },
    #[error("template directive {directive:?} is repeated on line {line}")]
    DuplicateDirective { directive: String, line: usize },
    #[error("{{{{caudra.default}}}} cannot be combined with component directives")]
    DefaultMixedWithComponents,
    #[error("automations: {name:?} {INVALID_NAME}")]
    InvalidAutomationName { name: String },
    #[error("automations: {name:?} is listed more than once")]
    DuplicateAutomation { name: String },
    #[error("automations: {name:?}: {error}")]
    InvalidAutomationArgs { name: String, error: ArgsError },
}

fn validate_profile_name(name: &str) -> Result<(), PromptProfileError> {
    if name == BUILTIN_PROFILE_NAME {
        return Err(PromptProfileError::ReservedName);
    }
    if name.is_empty()
        || name.len() > MAX_PROFILE_NAME_BYTES
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(PromptProfileError::InvalidName);
    }
    Ok(())
}

fn parse_subagent_model(model: String) -> Result<Binding, PromptProfileError> {
    let model = model.trim();
    if let Ok(purpose) = model.parse::<ModelPurpose>() {
        return ModelPurpose::TARGETS
            .contains(&purpose)
            .then_some(Binding::Same(purpose))
            .ok_or_else(|| PromptProfileError::InvalidSubagentModel {
                model: model.to_owned(),
            });
    }
    let valid = model.split_once('/').is_some_and(|(provider, model_id)| {
        !provider.is_empty()
            && provider.as_bytes()[0].is_ascii_alphanumeric()
            && provider
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            && !model_id.is_empty()
            && model_id.split('/').all(|part| {
                !part.is_empty()
                    && part
                        .chars()
                        .all(|character| !character.is_whitespace() && !character.is_control())
            })
    });
    if !valid {
        return Err(PromptProfileError::InvalidSubagentModel {
            model: model.to_owned(),
        });
    }
    Ok(Binding::Exact(model.to_owned()))
}

fn bounded_summary_description(description: &str) -> String {
    let mut description = description.split_whitespace().collect::<Vec<_>>().join(" ");
    if description.len() <= MAX_TASK_SUMMARY_DESCRIPTION_BYTES {
        return description;
    }
    let boundary = description.floor_char_boundary(MAX_TASK_SUMMARY_DESCRIPTION_BYTES - 3);
    description.truncate(boundary);
    description.push_str("...");
    description
}

fn load_profile(path: &Path, name: Arc<str>) -> Result<SystemPromptProfile, PromptProfileError> {
    validate_profile_name(&name)?;
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take((MAX_PROFILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_PROFILE_BYTES {
        return Err(PromptProfileError::TooLarge);
    }
    let text = String::from_utf8(bytes).map_err(|_| PromptProfileError::InvalidUtf8)?;
    let (frontmatter, body, mut body_line) =
        parse_frontmatter(text.strip_prefix('\u{feff}').unwrap_or(&text))?;
    let trimmed = body.trim_start_matches(['\r', '\n']);
    body_line += body[..body.len() - trimmed.len()]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count();
    let body = trimmed.trim_end_matches(['\r', '\n']);
    if body.trim().is_empty() {
        return Err(PromptProfileError::Empty);
    }
    if frontmatter.layout == PromptProfileLayout::Custom {
        validate_custom_template(body, body_line)?;
    }
    let subagent_model = frontmatter
        .subagent_model
        .map(parse_subagent_model)
        .transpose()?;
    let subagent_thinking = frontmatter
        .subagent_thinking
        .map(FrontmatterThinking::parse)
        .transpose()?;
    let description = frontmatter
        .description
        .map(|description| Arc::from(description.trim()))
        .filter(|description: &Arc<str>| !description.is_empty());
    Ok(SystemPromptProfile {
        name,
        description,
        layout: frontmatter.layout,
        subagent_model,
        subagent_thinking,
        tools: frontmatter.tools,
        automations: profile_automations(frontmatter.automations)?,
        body: Arc::from(body),
        path: Arc::from(path),
    })
}

/// Names follow the automation name rules, and args must fit what an arming accepts.
fn profile_automations(
    listed: Vec<FrontmatterAutomation>,
) -> Result<Vec<ProfileAutomation>, PromptProfileError> {
    let mut automations: Vec<ProfileAutomation> = Vec::with_capacity(listed.len());
    for automation in listed.into_iter().map(ProfileAutomation::from) {
        if !is_valid_name(&automation.name) {
            return Err(PromptProfileError::InvalidAutomationName {
                name: automation.name,
            });
        }
        if automations
            .iter()
            .any(|earlier| earlier.name == automation.name)
        {
            return Err(PromptProfileError::DuplicateAutomation {
                name: automation.name,
            });
        }
        if let Some(args) = &automation.args
            && let Err(error) = check_profile_args(args)
        {
            return Err(PromptProfileError::InvalidAutomationArgs {
                name: automation.name,
                error,
            });
        }
        automations.push(automation);
    }
    Ok(automations)
}

fn check_profile_args(args: &Value) -> Result<(), ArgsError> {
    if !args.is_object() {
        return Err(ArgsError::NotAnObject);
    }
    let bytes = serde_json::to_vec(args).map_or(usize::MAX, |json| json.len());
    if bytes > MAX_ARGS_JSON_BYTES {
        return Err(ArgsError::TooLarge(bytes));
    }
    Ok(())
}

fn parse_frontmatter(text: &str) -> Result<(Frontmatter, &str, usize), PromptProfileError> {
    let mut offset = 0;
    let mut lines = text.split_inclusive('\n');
    let Some(first) = lines.next() else {
        return Ok((Frontmatter::default(), text, 1));
    };
    if first.trim_end_matches(['\r', '\n']) != "---" {
        return Ok((Frontmatter::default(), text, 1));
    }
    offset += first.len();
    let yaml_start = offset;
    let mut line_number = 1;
    for line in lines {
        line_number += 1;
        let line_start = offset;
        offset += line.len();
        if line.trim_end_matches(['\r', '\n']) == "---" {
            let yaml = &text[yaml_start..line_start];
            let frontmatter = if yaml.trim().is_empty() {
                Frontmatter::default()
            } else {
                serde_yaml::from_str(yaml)?
            };
            return Ok((frontmatter, &text[offset..], line_number + 1));
        }
    }
    Err(PromptProfileError::UnclosedFrontmatter)
}

fn validate_custom_template(
    template: &str,
    starting_line: usize,
) -> Result<(), PromptProfileError> {
    let mut directives = HashSet::new();
    for (index, line) in template.lines().enumerate() {
        let line_number = starting_line + index;
        if line.starts_with("\\{{caudra.") && line.ends_with("}}") {
            continue;
        }
        let Some(name) = line
            .strip_prefix("{{caudra.")
            .and_then(|line| line.strip_suffix("}}"))
        else {
            continue;
        };
        if !super::is_system_component(name) {
            return Err(PromptProfileError::UnknownDirective {
                directive: line.to_owned(),
                line: line_number,
            });
        }
        if !directives.insert(name) {
            return Err(PromptProfileError::DuplicateDirective {
                directive: line.to_owned(),
                line: line_number,
            });
        }
    }
    if directives.contains("default") && directives.len() > 1 {
        return Err(PromptProfileError::DefaultMixedWithComponents);
    }
    for required in ["tools", "context", "plan"] {
        if !directives.contains("default") && !directives.contains(required) {
            tracing::warn!(
                component = required,
                "custom system prompt profile omits Caudra component"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::process::Command;

    use caudra_automation::meta::MAX_NAME_BYTES;
    use caudra_config::ModelPolicy;
    use caudra_providers::{Model, ThinkingConfig, catalog_providers_if_available, model_registry};
    use caudra_storage::StateDir;
    use caudra_storage::thinking::StoredThinking;
    use serde_json::json;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    /// The reminder contract is appended once, after the profile, so a profile
    /// cannot leave a subagent unable to read its own announcements.
    const EXPECTED_REMINDER_CONTRACTS: usize = 1;
    const INSPECTION_PROFILE: &str = "inspect";
    const INSPECTION_PARENT: &str = "mistral/ministral-14b-latest";
    const INSPECTION_CHAT: &str = "anthropic/claude-sonnet-4-6";
    const INSPECTION_OPUS: &str = "anthropic/claude-opus-5-5";
    const INSPECTION_ROUTED: &str = "aperture/mistral/mistral-medium-latest";
    const INSPECTION_UNKNOWN: &str = "caudra-inspection-unknown/model";
    const INCOMPATIBLE_THINKING: &str = "subagent thinking Adaptive is incompatible with model \"mistral/ministral-14b-latest\": model does not support thinking";
    const OPUS_INCOMPATIBLE_OFF: &str = "subagent thinking Off is incompatible with model \"anthropic/claude-opus-5-5\": model would resolve thinking as low";
    const OPUS_INCOMPATIBLE_BUDGET: &str = "subagent thinking Budget(8192) is incompatible with model \"anthropic/claude-opus-5-5\": model would resolve thinking as adaptive";
    const OPUS_INCOMPATIBLE_MINIMAL: &str = "subagent thinking Effort(\"minimal\") is incompatible with model \"anthropic/claude-opus-5-5\": model would resolve thinking as low";
    const INSPECTION_OFFLINE_ENV: &str = "CAUDRA_TEST_PROFILE_INSPECTION_OFFLINE";
    const INSPECTION_OFFLINE_TEST: &str =
        "prompt::profile::tests::inspection_task_bindings_need_no_auth_or_catalog";
    const AUTOMATION_PROFILE: &str = "worker";
    const JOIN_SWARM: &str = "join-swarm";
    const GOAL_CHAIN: &str = "goal-chain";
    const NOT_KEBAB_CASE: &str = "Join_Swarm";
    /// The compact JSON around the string in `{k: …}` args.
    const ARGS_FRAME: &str = r#"{"k":""}"#;

    fn inspection_catalog(model: Option<&str>, thinking: Option<&str>) -> PromptProfileCatalog {
        let name: Arc<str> = Arc::from(INSPECTION_PROFILE);
        let profile = SystemPromptProfile {
            name: Arc::clone(&name),
            description: None,
            layout: PromptProfileLayout::Overlay,
            subagent_model: model.map(|model| parse_subagent_model(model.to_owned()).unwrap()),
            subagent_thinking: thinking
                .map(|thinking| StoredThinking::parse_setting(thinking).unwrap()),
            tools: ProfileToolPolicy::default(),
            automations: Vec::new(),
            body: Arc::from(""),
            path: Arc::from(Path::new(INSPECTION_PROFILE)),
        };
        PromptProfileCatalog {
            profiles: BTreeMap::from([(name, Arc::new(profile))]),
            invalid: BTreeMap::new(),
        }
    }

    fn discover(dir: &Path) -> PromptProfileCatalog {
        PromptProfileCatalog::discover_with(Some(dir))
    }

    fn profile_dir(dir: &TempDir) -> std::path::PathBuf {
        let profiles = dir.path().join(PROFILE_DIR);
        fs::create_dir(&profiles).unwrap();
        profiles
    }

    #[test_case("", true; "omitted")]
    #[test_case("tools: {}", true; "empty")]
    #[test_case("layout: custom\ntools:\n  default: lazy", true; "custom")]
    #[test_case("tools:\n  default: inherit\n  groups:\n    messaging: lazy\n    support: eager", true; "groups")]
    #[test_case("tools:\n  default: disabled\n  overrides:\n    github.*: lazy", true; "mcp_server")]
    #[test_case("tools:\n  groups:\n    bogus: eager", false; "unknown_group")]
    #[test_case("tools:\n  overrides:\n    file_read: lazy\n    file_read: eager", false; "duplicate_selector")]
    #[test_case("tools:\n  groups:\n    web: lazy\n    web: eager", false; "duplicate_group")]
    #[test_case("tools:\n  overrides:\n    tool_search: eager", false; "reserved_loader")]
    #[test_case("tools:\n  overrides:\n    structured_output: disabled", false; "required_output")]
    fn profile_tool_frontmatter_is_strict(metadata: &str, valid: bool) {
        let dir = TempDir::new().unwrap();
        let profiles = profile_dir(&dir);
        fs::write(
            profiles.join("policy.md"),
            format!("---\n{metadata}\n---\nProfile body."),
        )
        .unwrap();
        assert_eq!(discover(dir.path()).resolve(Some("policy")).is_ok(), valid);
    }

    #[test]
    fn discovers_overlay_and_custom_profiles() {
        let dir = TempDir::new().unwrap();
        let profiles = dir.path().join(PROFILE_DIR);
        fs::create_dir(&profiles).unwrap();
        fs::write(profiles.join("review.md"), "Review carefully.").unwrap();
        fs::write(
            profiles.join("custom.md"),
            "---\ndescription: Custom layout\nlayout: custom\n---\n{{caudra.default}}",
        )
        .unwrap();

        let catalog = discover(dir.path());
        let review = catalog.get("review").unwrap();
        assert_eq!(review.layout(), PromptProfileLayout::Overlay);
        assert_eq!(review.body(), "Review carefully.");
        let custom = catalog.get("custom").unwrap();
        assert_eq!(custom.layout(), PromptProfileLayout::Custom);
        assert_eq!(custom.description(), Some("Custom layout"));
    }

    #[test]
    fn parses_subagent_model_and_semantic_thinking_overrides() {
        let dir = TempDir::new().unwrap();
        let profiles = profile_dir(&dir);
        fs::write(
            profiles.join("review.md"),
            "---\nsubagent_model: custom-provider/org/model\nsubagent_thinking: XHigh\n---\nReview carefully.",
        )
        .unwrap();
        fs::write(
            profiles.join("budget.md"),
            "---\nsubagent_thinking: 8192\n---\nUse a budget.",
        )
        .unwrap();

        let catalog = discover(dir.path());
        let review = catalog.get("review").unwrap();
        assert_eq!(
            review.subagent_model(),
            Some(&Binding::Exact("custom-provider/org/model".into()))
        );
        assert_eq!(
            review.subagent_thinking(),
            Some(&StoredThinking::Effort {
                level: "xhigh".into()
            })
        );
        assert_eq!(
            catalog.get("budget").unwrap().subagent_thinking(),
            Some(&StoredThinking::Budget { tokens: 8192 })
        );
    }

    #[test]
    fn omitted_subagent_overrides_have_no_effect() {
        let dir = TempDir::new().unwrap();
        let profiles = profile_dir(&dir);
        fs::write(profiles.join("plain.md"), "Plain profile.").unwrap();

        let profile = discover(dir.path()).get("plain").unwrap();
        assert_eq!(profile.subagent_model(), None);
        assert_eq!(profile.subagent_thinking(), None);
    }

    #[test]
    fn task_bindings_disable_incompatible_profiles_without_invalidating_main_use() {
        let dir = TempDir::new().unwrap();
        let profiles = profile_dir(&dir);
        fs::write(profiles.join("plain.md"), "Plain profile.").unwrap();
        fs::write(
            profiles.join("blocked.md"),
            "---\nsubagent_model: openai/gpt-5.4\n---\nBlocked child model.",
        )
        .unwrap();
        let catalog = discover(dir.path());
        let parent = Model::from_spec("anthropic/claude-sonnet-4-6").unwrap();
        let policy = ModelPolicy::new(&[], &["openai/gpt-5.4".into()]).unwrap();

        let bindings = catalog.bind_for_tasks(
            &parent,
            &parent,
            &ThinkingConfig::Off,
            &policy,
            Timeouts::default(),
        );

        assert!(bindings.resolve("plain").unwrap().is_some());
        assert!(matches!(
            bindings.resolve("blocked"),
            Err(PromptProfileSelectionError::Unavailable { .. })
        ));
        assert!(catalog.get("blocked").is_some());
        assert_eq!(
            bindings
                .available()
                .map(SystemPromptProfile::name)
                .collect::<Vec<_>>(),
            vec!["plain"]
        );
        let summary = bindings.task_tool_summary("Built in");
        assert!(summary.contains("`plain`"));
        assert!(!summary.contains("`blocked`"));
    }

    #[test_case(Some("chat"), None, None; "selected_chat_not_parent")]
    #[test_case(Some(INSPECTION_CHAT), None, None; "exact_model_not_parent")]
    #[test_case(Some(INSPECTION_ROUTED), None, None; "routed_metadata_adjusted")]
    #[test_case(None, Some("adaptive"), Some(INCOMPATIBLE_THINKING); "unbound_uses_effective_parent")]
    #[test_case(Some(INSPECTION_PARENT), None, Some(INCOMPATIBLE_THINKING); "exact_model_inherits_thinking")]
    #[test_case(Some(INSPECTION_PARENT), Some("off"), None; "profile_thinking_overrides_parent")]
    #[test_case(None, None, None; "no_overrides_skip_validation")]
    fn inspection_task_bindings_validate_selected_model_and_thinking(
        model: Option<&str>,
        thinking: Option<&str>,
        expected_error: Option<&str>,
    ) {
        let catalog = inspection_catalog(model, thinking);
        let parent = Model::from_spec(INSPECTION_PARENT).unwrap();
        let chat = Model::from_spec(INSPECTION_CHAT).unwrap();
        let bindings = catalog.bind_for_tasks_for_inspection(
            &parent,
            &chat,
            &ThinkingConfig::Adaptive,
            &ModelPolicy::default(),
        );

        assert_eq!(
            bindings.disabled().next(),
            expected_error.map(|reason| (INSPECTION_PROFILE, reason))
        );
        assert_eq!(
            bindings.resolve(INSPECTION_PROFILE).is_ok(),
            expected_error.is_none()
        );
        assert!(catalog.resolve(Some(INSPECTION_PROFILE)).unwrap().is_some());
    }

    #[test_case(Some("adaptive"), "off", None; "adaptive_overrides_inherited_off")]
    #[test_case(Some("low"), "off", None; "low_overrides_inherited_off")]
    #[test_case(Some("medium"), "off", None; "medium_overrides_inherited_off")]
    #[test_case(Some("high"), "off", None; "high_overrides_inherited_off")]
    #[test_case(Some("xhigh"), "off", None; "xhigh_overrides_inherited_off")]
    #[test_case(Some("max"), "off", None; "max_overrides_inherited_off")]
    #[test_case(Some("off"), "adaptive", Some(OPUS_INCOMPATIBLE_OFF); "explicit_off_is_not_promoted")]
    #[test_case(Some("8192"), "adaptive", Some(OPUS_INCOMPATIBLE_BUDGET); "explicit_budget_is_not_translated")]
    #[test_case(Some("minimal"), "adaptive", Some(OPUS_INCOMPATIBLE_MINIMAL); "explicit_minimal_is_not_snapped")]
    #[test_case(None, "off", Some(OPUS_INCOMPATIBLE_OFF); "model_override_inherits_off")]
    #[test_case(None, "8192", Some(OPUS_INCOMPATIBLE_BUDGET); "model_override_inherits_budget")]
    #[test_case(None, "minimal", Some(OPUS_INCOMPATIBLE_MINIMAL); "model_override_inherits_minimal")]
    fn inspection_opus_task_bindings_require_exact_thinking(
        thinking: Option<&str>,
        parent_thinking: &str,
        expected_error: Option<&str>,
    ) {
        let catalog = inspection_catalog(Some(INSPECTION_OPUS), thinking);
        let parent = Model::from_spec(INSPECTION_CHAT).unwrap();
        let parent_thinking = StoredThinking::parse_setting(parent_thinking)
            .unwrap()
            .into();
        let bindings = catalog.bind_for_tasks_for_inspection(
            &parent,
            &parent,
            &parent_thinking,
            &ModelPolicy::default(),
        );

        assert_eq!(
            bindings.disabled().next(),
            expected_error.map(|reason| (INSPECTION_PROFILE, reason))
        );
        match expected_error {
            Some(expected_reason) => {
                assert!(matches!(
                    bindings.resolve(INSPECTION_PROFILE),
                    Err(PromptProfileSelectionError::Unavailable { name, reason })
                        if name == INSPECTION_PROFILE && reason == expected_reason
                ));
                assert!(bindings.available().next().is_none());
            }
            None => assert!(bindings.resolve(INSPECTION_PROFILE).unwrap().is_some()),
        }
        assert!(catalog.get(INSPECTION_PROFILE).is_some());
        assert!(catalog.resolve(Some(INSPECTION_PROFILE)).unwrap().is_some());
    }

    #[test]
    fn inspection_opus_task_bindings_without_overrides_skip_exact_validation() {
        let catalog = inspection_catalog(None, None);
        let parent = Model::from_spec(INSPECTION_OPUS).unwrap();
        let bindings = catalog.bind_for_tasks_for_inspection(
            &parent,
            &parent,
            &ThinkingConfig::Off,
            &ModelPolicy::default(),
        );

        assert!(bindings.disabled().next().is_none());
        assert!(bindings.resolve(INSPECTION_PROFILE).unwrap().is_some());
    }

    #[test]
    fn inspection_task_bindings_need_no_auth_or_catalog() {
        if env::var_os(INSPECTION_OFFLINE_ENV).is_none() {
            let home = TempDir::new().unwrap();
            let status = Command::new(env::current_exe().unwrap())
                .args(["--exact", INSPECTION_OFFLINE_TEST, "--nocapture"])
                .env_clear()
                .env(INSPECTION_OFFLINE_ENV, INSPECTION_OFFLINE_TEST)
                .env("HOME", home.path())
                .env("XDG_CONFIG_HOME", home.path())
                .env("XDG_DATA_HOME", home.path())
                .env("XDG_STATE_HOME", home.path())
                .env("XDG_CACHE_HOME", home.path())
                .current_dir(home.path())
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }

        let parent = Model::from_spec(INSPECTION_PARENT).unwrap();
        let chat = Model::from_spec(INSPECTION_CHAT).unwrap();
        let policy = ModelPolicy::default();
        assert!(catalog_providers_if_available().is_none());
        for model in [INSPECTION_CHAT, INSPECTION_ROUTED] {
            let bindings = inspection_catalog(Some(model), None).bind_for_tasks_for_inspection(
                &parent,
                &chat,
                &ThinkingConfig::Adaptive,
                &policy,
            );
            assert!(bindings.resolve(INSPECTION_PROFILE).unwrap().is_some());
        }

        let bindings = inspection_catalog(Some(INSPECTION_UNKNOWN), None)
            .bind_for_tasks_for_inspection(&parent, &chat, &ThinkingConfig::Off, &policy);
        assert!(matches!(
            bindings.resolve(INSPECTION_PROFILE),
            Err(PromptProfileSelectionError::Unavailable { .. })
        ));

        let dir = TempDir::new().unwrap();
        let state = StateDir::from_path(dir.path().to_path_buf());
        model_registry::set_binding_and_persist(
            ModelPurpose::Subagent,
            Binding::Same(ModelPurpose::Chat),
            &state,
        )
        .unwrap();
        let bindings = inspection_catalog(None, Some("adaptive")).bind_for_tasks_for_inspection(
            &parent,
            &chat,
            &ThinkingConfig::Off,
            &policy,
        );
        assert!(bindings.resolve(INSPECTION_PROFILE).unwrap().is_some());

        let bindings = inspection_catalog(Some(INSPECTION_PARENT), None)
            .bind_for_tasks_for_inspection(&parent, &chat, &ThinkingConfig::Adaptive, &policy);
        assert_eq!(
            bindings.disabled().next(),
            Some((INSPECTION_PROFILE, INCOMPATIBLE_THINKING))
        );

        let blocked = ModelPolicy::new(&[], &[INSPECTION_CHAT.into()]).unwrap();
        let bindings = inspection_catalog(Some(INSPECTION_CHAT), None)
            .bind_for_tasks_for_inspection(&parent, &chat, &ThinkingConfig::Off, &blocked);
        assert!(matches!(
            bindings.resolve(INSPECTION_PROFILE),
            Err(PromptProfileSelectionError::Unavailable { .. })
        ));
        assert!(catalog_providers_if_available().is_none());
    }

    #[test]
    fn rejects_invalid_subagent_model_selectors_without_requiring_known_provider() {
        let dir = TempDir::new().unwrap();
        let profiles = profile_dir(&dir);
        for (name, model) in [
            ("unqualified", "model"),
            ("provider", "/model"),
            ("model", "provider/"),
            ("empty-segment", "provider/org//model"),
            ("bad-provider", "provider.name/model"),
            ("whitespace", "provider/model name"),
            ("self", "subagent"),
            ("compact", "compact"),
            ("title", "title"),
            ("goal", "goal"),
        ] {
            fs::write(
                profiles.join(format!("{name}.md")),
                format!("---\nsubagent_model: {model}\n---\nbody"),
            )
            .unwrap();
        }
        fs::write(
            profiles.join("unknown.md"),
            "---\nsubagent_model: not-installed/model\n---\nbody",
        )
        .unwrap();

        let catalog = discover(dir.path());
        assert_eq!(
            catalog.get("unknown").unwrap().subagent_model(),
            Some(&Binding::Exact("not-installed/model".into()))
        );
        for name in [
            "unqualified",
            "provider",
            "model",
            "empty-segment",
            "bad-provider",
            "whitespace",
            "self",
            "compact",
            "title",
            "goal",
        ] {
            let error = catalog.resolve(Some(name)).unwrap_err();
            assert!(error.to_string().contains("qualified provider/model"));
        }
    }

    #[test_case("chat", ModelPurpose::Chat ; "chat")]
    #[test_case("plan", ModelPurpose::Plan ; "plan")]
    #[test_case("fast", ModelPurpose::Fast ; "fast")]
    #[test_case("best", ModelPurpose::Best ; "best")]
    fn parses_named_subagent_model_targets(selector: &str, purpose: ModelPurpose) {
        assert_eq!(
            parse_subagent_model(selector.to_owned()).unwrap(),
            Binding::Same(purpose)
        );
    }

    #[test]
    fn rejects_thinking_outside_shared_vocabulary() {
        let dir = TempDir::new().unwrap();
        let profiles = profile_dir(&dir);
        fs::write(
            profiles.join("bad.md"),
            "---\nsubagent_thinking: turbo\n---\nbody",
        )
        .unwrap();
        fs::write(
            profiles.join("zero.md"),
            "---\nsubagent_thinking: 0\n---\nbody",
        )
        .unwrap();

        let catalog = discover(dir.path());
        assert!(
            catalog
                .resolve(Some("bad"))
                .unwrap_err()
                .to_string()
                .contains("unknown thinking level")
        );
        assert!(
            catalog
                .resolve(Some("zero"))
                .unwrap_err()
                .to_string()
                .contains("greater than zero")
        );
    }

    #[test]
    fn task_tool_summary_is_sorted_normalized_and_bounded() {
        let dir = TempDir::new().unwrap();
        let profiles = profile_dir(&dir);
        let long_description = format!("line one\n{}end", "x".repeat(200));
        for index in (0..25).rev() {
            let description = if index == 0 {
                long_description.as_str()
            } else {
                "short description"
            };
            let description = description.replace('\n', "\n  ");
            fs::write(
                profiles.join(format!("p{index:02}.md")),
                format!("---\ndescription: |\n  {description}\n---\nbody"),
            )
            .unwrap();
        }
        let catalog = discover(dir.path());

        let summary = catalog.task_tool_summary(Some("Built in\n task prompt"));
        assert_eq!(
            summary,
            catalog.task_tool_summary(Some("Built in\n task prompt"))
        );
        let lines = summary.lines().collect::<Vec<_>>();
        assert_eq!(lines[0], "- `builtin`: Built in task prompt");
        assert!(lines[1].starts_with("- `p00`: line one "));
        assert!(lines[1].ends_with("..."));
        assert!(lines[1].len() <= "- `p00`: ".len() + MAX_TASK_SUMMARY_DESCRIPTION_BYTES);
        assert!(lines[2].starts_with("- `p01`:"));
        assert_eq!(lines[MAX_TASK_SUMMARY_ENTRIES], "- ... and 6 more");
        assert_eq!(lines.len(), MAX_TASK_SUMMARY_ENTRIES + 1);

        let without_builtin = catalog.task_tool_summary(None);
        assert!(without_builtin.starts_with("- `p00`"));
        assert!(without_builtin.ends_with("- ... and 5 more"));
    }

    #[test]
    fn invalid_profile_is_reported_when_selected() {
        let dir = TempDir::new().unwrap();
        let profiles = dir.path().join(PROFILE_DIR);
        fs::create_dir(&profiles).unwrap();
        fs::write(profiles.join("bad.md"), "---\nlayout: nope\n---\nbody").unwrap();

        let error = discover(dir.path()).resolve(Some("bad")).unwrap_err();
        assert!(error.to_string().contains("is invalid"));
    }

    #[test]
    fn oversized_profile_is_rejected_without_truncation() {
        let dir = TempDir::new().unwrap();
        let profiles = dir.path().join(PROFILE_DIR);
        fs::create_dir(&profiles).unwrap();
        fs::write(profiles.join("large.md"), vec![b'x'; MAX_PROFILE_BYTES + 1]).unwrap();

        let error = discover(dir.path()).resolve(Some("large")).unwrap_err();
        assert!(error.to_string().contains("exceeds"));
    }

    #[test]
    fn preserves_indented_markdown_and_skips_non_files() {
        let dir = TempDir::new().unwrap();
        let profiles = dir.path().join(PROFILE_DIR);
        fs::create_dir(&profiles).unwrap();
        fs::write(profiles.join("code.md"), "    indented code\n").unwrap();
        fs::create_dir(profiles.join("directory.md")).unwrap();

        let catalog = discover(dir.path());
        assert_eq!(catalog.get("code").unwrap().body(), "    indented code");
        assert!(catalog.get("directory").is_none());
    }

    #[test]
    fn custom_template_rejects_unknown_duplicate_and_mixed_directives() {
        let unknown = validate_custom_template("{{caudra.unknown}}", 1).unwrap_err();
        assert!(matches!(
            unknown,
            PromptProfileError::UnknownDirective { .. }
        ));
        let duplicate =
            validate_custom_template("{{caudra.tools}}\n{{caudra.tools}}", 1).unwrap_err();
        assert!(matches!(
            duplicate,
            PromptProfileError::DuplicateDirective { .. }
        ));
        let mixed = validate_custom_template("{{caudra.default}}\n{{caudra.plan}}", 1).unwrap_err();
        assert!(matches!(
            mixed,
            PromptProfileError::DefaultMixedWithComponents
        ));
    }

    #[test]
    fn custom_template_error_uses_file_line_number() {
        let dir = TempDir::new().unwrap();
        let profiles = dir.path().join(PROFILE_DIR);
        fs::create_dir(&profiles).unwrap();
        fs::write(
            profiles.join("bad.md"),
            "---\nlayout: custom\n---\n\n{{caudra.unknown}}",
        )
        .unwrap();

        let error = discover(dir.path()).resolve(Some("bad")).unwrap_err();
        assert!(error.to_string().contains("line 5"));
    }

    #[test]
    fn builtin_and_missing_profiles_resolve_predictably() {
        let catalog = PromptProfileCatalog::default();
        assert!(catalog.resolve(None).unwrap().is_none());
        assert!(
            catalog
                .resolve(Some(BUILTIN_PROFILE_NAME))
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            catalog.resolve(Some("missing")),
            Err(PromptProfileSelectionError::NotFound { .. })
        ));
    }

    #[test]
    fn overlay_keeps_dynamic_components_and_lands_before_plan() {
        const PROFILE_TEXT: &str = "PROFILE {{tone}} {{instructions}}";
        let dir = TempDir::new().unwrap();
        let profiles = dir.path().join(PROFILE_DIR);
        fs::create_dir(&profiles).unwrap();
        fs::write(profiles.join("review.md"), PROFILE_TEXT).unwrap();
        let profile = discover(dir.path()).get("review").unwrap();

        let output = crate::prompt::assemble_system(
            &crate::prompt::ResolvedSlots::default(),
            "RUNTIME_CONTEXT",
            "PLAN_REMINDER",
            Some(&profile),
        );
        assert!(output.contains("# Tool usage"));
        assert!(output.contains("RUNTIME_CONTEXT"));
        assert!(output.contains(PROFILE_TEXT));
        assert!(output.find(PROFILE_TEXT) < output.find("PLAN_REMINDER"));
    }

    #[test]
    fn custom_layout_reorders_components_and_can_omit_sections() {
        let dir = TempDir::new().unwrap();
        let profiles = dir.path().join(PROFILE_DIR);
        fs::create_dir(&profiles).unwrap();
        fs::write(
            profiles.join("custom.md"),
            "---\nlayout: custom\n---\nCUSTOM\n{{caudra.context}}\n{{caudra.identity}}\n{{caudra.plan}}",
        )
        .unwrap();
        let profile = discover(dir.path()).get("custom").unwrap();

        let output = crate::prompt::assemble_system(
            &crate::prompt::ResolvedSlots::default(),
            "RUNTIME_CONTEXT",
            "PLAN_REMINDER",
            Some(&profile),
        );
        assert!(output.starts_with("CUSTOM"));
        assert!(output.find("RUNTIME_CONTEXT") < output.find("You are Caudra"));
        assert!(!output.contains("# Tool usage"));
        assert!(output.ends_with("PLAN_REMINDER"));
    }

    #[test]
    fn custom_layout_can_escape_a_directive() {
        let dir = TempDir::new().unwrap();
        let profiles = dir.path().join(PROFILE_DIR);
        fs::create_dir(&profiles).unwrap();
        fs::write(
            profiles.join("escaped.md"),
            "---\nlayout: custom\n---\n\\{{caudra.tools}}",
        )
        .unwrap();
        let profile = discover(dir.path()).get("escaped").unwrap();

        let output = crate::prompt::assemble_system(
            &crate::prompt::ResolvedSlots::default(),
            "",
            "",
            Some(&profile),
        );
        assert_eq!(output, "{{caudra.tools}}");
    }

    #[test]
    fn task_overlay_lands_after_default_and_before_reminder_contract() {
        let dir = TempDir::new().unwrap();
        let profiles = profile_dir(&dir);
        fs::write(profiles.join("review.md"), "PROFILE_OVERLAY").unwrap();
        let profile = discover(dir.path()).get("review").unwrap();

        let output = crate::prompt::assemble_task(
            crate::prompt::PromptId::Research,
            &crate::prompt::ResolvedSlots::default(),
            "TASK_CONTEXT",
            Some(&profile),
        );
        assert!(output.starts_with("You are a research agent"));
        assert!(output.find("TASK_CONTEXT") < output.find("PROFILE_OVERLAY"));
        assert!(output.ends_with(crate::prompt::REMINDERS_PROMPT));
        assert_eq!(
            output.matches(crate::prompt::REMINDERS_PROMPT).count(),
            EXPECTED_REMINDER_CONTRACTS
        );
    }

    #[test]
    fn task_custom_default_is_the_full_task_default() {
        let dir = TempDir::new().unwrap();
        let profiles = profile_dir(&dir);
        fs::write(
            profiles.join("custom.md"),
            "---\nlayout: custom\n---\n{{caudra.default}}",
        )
        .unwrap();
        let profile = discover(dir.path()).get("custom").unwrap();
        let slots = crate::prompt::ResolvedSlots::default();
        let default =
            crate::prompt::assemble(crate::prompt::PromptId::General, &slots, "TASK_CONTEXT");

        let output = crate::prompt::assemble_task(
            crate::prompt::PromptId::General,
            &slots,
            "TASK_CONTEXT",
            Some(&profile),
        );
        assert_eq!(
            output,
            format!("{default}{}", crate::prompt::REMINDERS_PROMPT)
        );
    }

    #[test]
    fn task_custom_directives_use_mode_specific_components() {
        let dir = TempDir::new().unwrap();
        let profiles = profile_dir(&dir);
        fs::write(
            profiles.join("custom.md"),
            concat!(
                "---\nlayout: custom\n---\n",
                "{{caudra.identity}}\n",
                "{{caudra.style}}\n",
                "{{caudra.tools}}\n",
                "{{caudra.conventions}}\n",
                "{{caudra.completion}}\n",
                "{{caudra.context}}\n",
                "PLAN_START\n{{caudra.plan}}\nPLAN_END",
            ),
        )
        .unwrap();
        let profile = discover(dir.path()).get("custom").unwrap();
        let slots = crate::prompt::ResolvedSlots::default();

        let research = crate::prompt::assemble_task(
            crate::prompt::PromptId::Research,
            &slots,
            "TASK_CONTEXT",
            Some(&profile),
        );
        assert!(research.contains("You are a research agent"));
        assert!(research.contains("Do NOT modify files"));
        assert!(research.contains("# Output discipline"));
        assert!(research.contains("# Tool usage"));
        assert!(research.contains("# Guidelines"));
        assert!(research.contains("TASK_CONTEXT"));
        assert!(!research.contains("# When done"));
        assert!(research.contains("PLAN_START\nPLAN_END"));
        assert!(research.ends_with(crate::prompt::REMINDERS_PROMPT));

        let general = crate::prompt::assemble_task(
            crate::prompt::PromptId::General,
            &slots,
            "TASK_CONTEXT",
            Some(&profile),
        );
        assert!(general.contains("You are a general-purpose coding agent"));
        assert!(general.contains("# Conventions"));
        assert!(general.contains("# When done"));
        assert!(!general.contains("# Guidelines"));
        assert!(general.ends_with(crate::prompt::REMINDERS_PROMPT));
    }

    fn load_frontmatter(frontmatter: &str) -> Result<SystemPromptProfile, PromptProfileError> {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(format!("{AUTOMATION_PROFILE}.md"));
        fs::write(&path, format!("---\n{frontmatter}\n---\nWork the queue.")).unwrap();
        load_profile(&path, Arc::from(AUTOMATION_PROFILE))
    }

    fn automations_with_args(args: &str) -> String {
        format!("automations: [{{name: {GOAL_CHAIN}, args: {args}}}]")
    }

    #[test_case("description: Worker", Vec::new(); "omitted")]
    #[test_case(&format!("automations: [{JOIN_SWARM}]"), vec![ProfileAutomation { name: JOIN_SWARM.into(), args: None }]; "bare_name")]
    #[test_case(
        &format!(r#"automations: [{JOIN_SWARM}, {{name: {GOAL_CHAIN}, args: {{goals: ["A"]}}}}]"#),
        vec![
            ProfileAutomation { name: JOIN_SWARM.into(), args: None },
            ProfileAutomation { name: GOAL_CHAIN.into(), args: Some(json!({"goals": ["A"]})) },
        ];
        "map_with_args"
    )]
    fn profile_automations_are_listed_in_order(
        frontmatter: &str,
        expected: Vec<ProfileAutomation>,
    ) {
        let profile = load_frontmatter(frontmatter).unwrap();

        assert_eq!(profile.automations(), expected.as_slice());
    }

    #[test_case(NOT_KEBAB_CASE; "not_kebab_case")]
    #[test_case(&"a".repeat(MAX_NAME_BYTES + 1); "too_long")]
    fn an_invalid_automation_name_is_refused(name: &str) {
        let error = load_frontmatter(&format!("automations: [{name}]")).unwrap_err();

        assert!(
            matches!(&error, PromptProfileError::InvalidAutomationName { name: refused } if refused == name),
            "{error}"
        );
    }

    #[test]
    fn a_repeated_automation_is_refused() {
        let error = load_frontmatter(&format!(
            "automations: [{JOIN_SWARM}, {{name: {JOIN_SWARM}}}]"
        ))
        .unwrap_err();

        assert!(
            matches!(&error, PromptProfileError::DuplicateAutomation { name } if name == JOIN_SWARM),
            "{error}"
        );
    }

    #[test_case("[A]"; "list")]
    #[test_case("text"; "string")]
    fn automation_args_must_be_an_object(args: &str) {
        let error = load_frontmatter(&automations_with_args(args)).unwrap_err();

        assert!(
            matches!(
                &error,
                PromptProfileError::InvalidAutomationArgs { name, error: ArgsError::NotAnObject }
                    if name == GOAL_CHAIN
            ),
            "{error}"
        );
    }

    #[test_case(0 => None; "at_the_limit")]
    #[test_case(1 => Some(ArgsError::TooLarge(MAX_ARGS_JSON_BYTES + 1)); "one_byte_over")]
    fn automation_args_are_bounded(excess: usize) -> Option<ArgsError> {
        let text = "x".repeat(MAX_ARGS_JSON_BYTES - ARGS_FRAME.len() + excess);
        match load_frontmatter(&automations_with_args(&format!("{{k: {text}}}"))) {
            Ok(_) => None,
            Err(PromptProfileError::InvalidAutomationArgs { error, .. }) => Some(error),
            Err(other) => panic!("{other}"),
        }
    }

    #[test_case(&format!("[{{name: {GOAL_CHAIN}, arg: {{}}}}]"); "unknown_key")]
    #[test_case("[{args: {}}]"; "missing_name")]
    fn a_malformed_automation_entry_is_invalid_frontmatter(list: &str) {
        let error = load_frontmatter(&format!("automations: {list}")).unwrap_err();

        assert!(
            matches!(error, PromptProfileError::InvalidFrontmatter(_)),
            "{error}"
        );
    }
}
