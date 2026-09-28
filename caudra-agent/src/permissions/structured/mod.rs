use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use caudra_config::ToolKey;
use caudra_storage::permission_patterns::PatternDefinition;
use caudra_storage::permission_state::{
    BROWSE_DIRECT, BROWSE_RECURSION_ATTRIBUTE, BROWSE_RECURSIVE, FILESYSTEM_BROWSE_CONTRACTS,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Value, json};
use thiserror::Error;

use super::CONFINED_READ_ATTRIBUTE;
use super::command_pattern::{PatternFault, grade_command_pattern};
use crate::tools::PermissionIntent;

pub use caudra_storage::permission_state::{
    PermissionArgumentConstraint, PermissionCapabilityFamily, PermissionExecutorKind,
    PermissionLifetime, PermissionResourceAccess, PermissionResourceConstraint,
    PermissionResourceKind, PermissionResourceSelector, PermissionRuleRecord, PermissionSubject,
    RemotePermissionIdentity, SelectedPermissionArgument, StructuredPermissionEffect,
    StructuredPermissionRule,
};

mod arguments;
mod composition;
mod matching;
mod options;
mod presentation;
mod resources;
#[path = "../review.rs"]
pub mod review;
pub(crate) use arguments::hex_encode;
pub use arguments::{
    SelectedInputError, argument_constraint_matches, canonical_json, canonical_json_sha256,
    escape_json_pointer_segment, json_pointer, selected_input, selected_input_digest,
    selected_input_pointer,
};
pub(in crate::permissions) use matching::trusted_command_observation;
#[cfg(test)]
use matching::{SelectorWidth, resource_decision, selector_matches, selector_width};
pub use matching::{
    evaluate_structured_permission_rules, permission_rule_covers_request,
    permission_rule_covers_resource, permission_rule_intersects_request,
    permission_rules_cover_request, permission_rules_resource_standing,
    resource_constraint_matches,
};
use matching::{
    filesystem_subtree_digest, is_filesystem_browse_subject, is_filesystem_read_access,
    is_filesystem_read_kind, is_filesystem_read_subject, normalized_filesystem_path, remote_scope,
    resource_value_digest, strict_http_url, url_origin_digest, url_subtree_digest,
    url_subtree_roots,
};
pub(super) use options::BROAD_SHELL_PHRASE;
use options::rule_options;
pub use options::{
    COMMAND_EXACT_PREFIX, COMMAND_GROUP_PREFIX, COMMAND_PATTERN_PREFIX, COMMAND_TEMPLATE_PREFIX,
    COMPOSABLE_SHELL_OPTIONS,
};
#[cfg(test)]
use options::{
    EXACT_COMMAND_CHIP, MAX_URL_LADDER_RUNGS, OUTSIDE_HOME_PHRASE, URL_ORIGIN_OPTION_ID,
    URL_SUBTREE_OPTION_ID,
};
pub use presentation::update_presentation_coverage;
use presentation::{listed_commands, presentation_for, safe_summary};
pub use resources::filesystem_permission_resource;
#[cfg(test)]
use resources::filesystem_resource_flags;
use resources::{
    GIT_METADATA_DIR, attribute_kind, exact_resource_constraints, legacy_authority_profile,
    pinned_digest, remote_resource_identity, resource_constraint, resources_for,
    reusable_remote_resource_constraint, risk_for, subject_and_executor,
};

const NATIVE_OWNER: &str = "caudra";
/// Trust domain for the first-party Workcell tools, whose contracts are the only
/// members of `PermissionCapabilityFamily::FilesystemRead`.
const WORKCELL_OWNER: &str = "workcell";
/// The Workcell contracts that only ever read: they emit `Read` or `Search`
/// access, never `Write`, so one subtree grant may serve all of them. Adding a
/// contract here widens stored authority, so it must never gain a writing tool.
const FILESYSTEM_READ_CONTRACTS: &[&str] = &[
    "file.read.v1",
    "file.glob.v1",
    "file.grep.v1",
    "file.index.v1",
    "code.map.v1",
    "code.context.v1",
    "code.refs.v1",
    "code.impact.v1",
    "code.expand.v1",
];
const MCP_CONTRACT: &str = "mcp.tools.call/v1";
const FILE_READ_TOOLS: &[&str] = &["file_read", "file_index", "read", "view_image"];
const DIRECTORY_READ_TOOLS: &[&str] = &["list"];
const FILE_SEARCH_TOOLS: &[&str] = &["file_glob", "file_grep", "glob", "grep"];
const WORKDIR_ATTRIBUTE: &str = "workdir";
pub(super) const CONFINED_READ_AUTHORITY: &str = "reads inside the project";
/// The executable-name-resolved form of a command, set by the shell tool.
/// Restrictive policy is matched against it as well as the reviewed text, so a
/// deny cannot be dodged by spelling the executable as a path.
pub(super) const NORMALIZED_COMMAND_ATTRIBUTE: &str = "normalized_command";
pub const COMMAND_OBSERVATION_ATTRIBUTE: &str = "command_observation";
pub const COMMAND_OBSERVATION_BINDING_ATTRIBUTE: &str = "command_observation_binding";
pub(super) const POSSIBLE_WORKDIRS_ATTRIBUTE: &str = "possible_workdirs";
const PREPARED_COMMAND_BINDING_DOMAIN: &str = "caudra.prepared-command.v1";
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionRisk {
    Low,
    Medium,
    High,
    Critical,
    Unknown,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PermissionAuthorityProfile {
    #[default]
    ExactOnly,
    Filesystem {
        input_pointers: Vec<String>,
    },
    Url,
    Query,
    Shell,
    RemoteResource,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionResource {
    pub kind: PermissionResourceKind,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<PermissionResourceAccess>,
    #[serde(default)]
    pub protected: bool,
    #[serde(default)]
    pub requires_prompt: bool,
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        serialize_with = "serialize_resource_attributes",
        deserialize_with = "deserialize_resource_attributes"
    )]
    pub attributes: BTreeMap<String, String>,
}
/// One rung of a ladder of mutually exclusive authorities.
///
/// `key` names the ladder and `value` is the part that differs between its
/// rungs, so a renderer can show one row and move along it without parsing the
/// varying part back out of the description.
///
/// `resource` marks a ladder that speaks for a single resource of the request
/// rather than for all of it. Those rungs compose: one may be taken from every
/// such ladder and the chosen constraints merged into one rule. Ladders without
/// it stay mutually exclusive with everything else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionOptionGroup {
    pub key: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<usize>,
}
/// How far an authority reaches beyond what the request was about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionCaution {
    /// Outside the repository the request came from.
    Warn,
    /// Outside the user's home directory entirely.
    Danger,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionRuleOption {
    pub id: String,
    pub label: String,
    pub description: String,
    pub rule: StructuredPermissionRule,
    pub allowed_lifetimes: Vec<PermissionLifetime>,
    #[serde(default)]
    pub broad: bool,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<PermissionOptionGroup>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caution: Option<PermissionCaution>,
}
/// What one resource row contributes to a composed answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PermissionRowGrant {
    /// A rung the request offered for this resource.
    Offered(String),
    /// A pattern the user wrote for this resource.
    Written(String),
    Pattern {
        option_id: String,
        definition: Box<PatternDefinition>,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ComposedAnswerError {
    #[error("answer named {named} rows for {resources} resources")]
    RowCount { named: usize, resources: usize },
    #[error("request did not offer {0:?} for this command")]
    NotOffered(String),
    #[error("authority {0:?} does not allow the chosen lifetime")]
    LifetimeWithdrawn(String),
    #[error("pattern for `{command}` is not usable: {fault}")]
    Pattern {
        command: String,
        fault: PatternFault,
    },
    #[error("chosen authority does not cover the command it was chosen for")]
    Uncovered,
    #[error("edited command template does not preserve a currently offered structure and context")]
    TemplateNotOffered,
    #[error("invalid command template: {0}")]
    Template(String),
}
/// Where a rule came from, so a resource that is already allowed can say which
/// authority allows it. The lifetime cannot answer that on its own: the builtin
/// confined-read rule is a `Conversation` rule and configured policy compiles to
/// a `Project` one, yet neither is something the user granted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleOrigin {
    Builtin,
    Config,
    Plugin,
    Conversation,
    Project,
    Global,
}
impl RuleOrigin {
    pub fn label(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Config => "config",
            Self::Plugin => "plugin",
            Self::Conversation => "conversation",
            Self::Project => "project",
            Self::Global => "global",
        }
    }
}
/// A rule paired with where it came from. Assembly is the only place that knows
/// the origin, so it is attached there rather than rediscovered later.
#[derive(Debug, Clone, PartialEq)]
pub struct PolicyRule {
    pub origin: RuleOrigin,
    pub rule: StructuredPermissionRule,
}
/// The authority that already allows a resource, for a prompt that has to say
/// why a command needs no answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceCoverage {
    pub origin: RuleOrigin,
    /// How the covering constraint names the resource, such as `rg *`.
    pub authority: String,
}
/// What the rule set says about one resource, and which authority said it.
///
/// An ask withholds authority without erasing it, so coverage is reported
/// whatever the decision: the prompt can still say the resource is covered and a
/// later grant can still sweep it.
pub struct ResourceStanding {
    pub decision: StructuredPermissionDecision,
    pub coverage: Option<ResourceCoverage>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionResourcePresentation {
    pub kind: PermissionResourceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<PermissionResourceAccess>,
    pub summary: String,
    pub protected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<ResourceCoverage>,
}
impl PermissionResourcePresentation {
    pub fn covered(&self) -> bool {
        self.coverage.is_some()
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionAdvisory {
    pub flag: String,
    pub probability: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionPresentation {
    pub action: String,
    pub risk: PermissionRisk,
    pub risk_summary: String,
    pub resources: Vec<PermissionResourcePresentation>,
    /// Display-only, host-approved canonical project for available durable project grants.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub advisories: Vec<PermissionAdvisory>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionRequest {
    pub id: String,
    #[serde(deserialize_with = "deserialize_tool_key")]
    pub tool: ToolKey,
    pub scopes: Vec<String>,
    pub subject: PermissionSubject,
    pub executor: PermissionExecutorKind,
    pub risk: PermissionRisk,
    pub resources: Vec<PermissionResource>,
    pub input: Value,
    pub input_digest: String,
    pub lifetime: PermissionLifetime,
    pub options: Vec<PermissionRuleOption>,
    pub presentation: PermissionPresentation,
}
/// What a rule set says about a request or one of its resources.
///
/// The declaration order is the precedence: a deny outranks an ask, an ask
/// outranks an allow, and any of them outranks silence. `NoMatch` is not an
/// allow — it means no rule spoke, and the caller decides what that means.
///
/// This order combines resources and breaks ties between equally specific
/// rules. It does not rank the rules themselves; `permission_rules_resource_decision`
/// does, because a narrow allow has to survive a broad ask.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum StructuredPermissionDecision {
    #[default]
    NoMatch,
    Allow,
    Ask,
    Deny,
}
impl StructuredPermissionDecision {
    /// Folds in what another rule said. Authority only ever narrows, so this is
    /// associative and order-independent: the set decides, not the iteration.
    #[must_use]
    pub fn merge(self, other: Self) -> Self {
        self.max(other)
    }

    pub(super) fn of(effect: &StructuredPermissionEffect) -> Self {
        match effect {
            StructuredPermissionEffect::Allow => Self::Allow,
            StructuredPermissionEffect::Ask => Self::Ask,
            StructuredPermissionEffect::Deny => Self::Deny,
        }
    }
}
impl PermissionRequest {
    pub fn from_legacy(
        id: String,
        tool: ToolKey,
        scopes: Vec<String>,
        input: Value,
        cwd: &Path,
        force_prompt: bool,
    ) -> Self {
        let (subject, executor) = subject_and_executor(&tool);
        Self::from_legacy_with_identity(
            id,
            tool,
            scopes,
            input,
            cwd,
            force_prompt,
            subject,
            executor,
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub fn from_legacy_with_identity(
        id: String,
        tool: ToolKey,
        scopes: Vec<String>,
        input: Value,
        cwd: &Path,
        force_prompt: bool,
        subject: PermissionSubject,
        executor: PermissionExecutorKind,
    ) -> Self {
        let risk = risk_for(&tool, force_prompt);
        let resources = resources_for(&tool, &scopes, &input, cwd, force_prompt);
        let authority = legacy_authority_profile(&tool);
        Self::from_parts(
            id, tool, scopes, input, cwd, subject, executor, risk, resources, &authority,
        )
    }
    pub fn from_intent(
        id: String,
        tool: ToolKey,
        intent: &PermissionIntent,
        input: Value,
        cwd: &Path,
    ) -> Self {
        let (subject, executor) = subject_and_executor(&tool);
        Self::from_intent_with_identity(id, tool, intent, input, cwd, subject, executor)
    }
    #[allow(clippy::too_many_arguments)]
    pub fn from_intent_with_identity(
        id: String,
        tool: ToolKey,
        intent: &PermissionIntent,
        input: Value,
        cwd: &Path,
        subject: PermissionSubject,
        executor: PermissionExecutorKind,
    ) -> Self {
        Self::from_parts(
            id,
            tool,
            intent.scopes.scopes.clone(),
            input,
            cwd,
            subject,
            executor,
            intent.risk.clone(),
            intent.resources.clone(),
            &intent.authority,
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn from_parts(
        id: String,
        tool: ToolKey,
        scopes: Vec<String>,
        input: Value,
        cwd: &Path,
        subject: PermissionSubject,
        executor: PermissionExecutorKind,
        risk: PermissionRisk,
        resources: Vec<PermissionResource>,
        authority: &PermissionAuthorityProfile,
    ) -> Self {
        let input_digest = canonical_json_sha256(&input);
        let options = rule_options(
            &tool,
            &subject,
            &executor,
            &resources,
            &input,
            &input_digest,
            cwd,
            authority,
        );
        let presentation = presentation_for(&tool, &risk, &resources);
        Self {
            id,
            tool,
            scopes,
            subject,
            executor,
            risk,
            resources,
            input,
            input_digest,
            lifetime: PermissionLifetime::Once,
            options,
            presentation,
        }
    }
}
pub fn prepared_command_binding(resource_value: &str, input: &Value) -> String {
    canonical_json_sha256(&json!([
        PREPARED_COMMAND_BINDING_DOMAIN,
        resource_value,
        input
    ]))
}

fn serialize_resource_attributes<S>(
    attributes: &BTreeMap<String, String>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    attributes
        .iter()
        .filter(|(name, _)| {
            !matches!(
                name.as_str(),
                COMMAND_OBSERVATION_ATTRIBUTE | COMMAND_OBSERVATION_BINDING_ATTRIBUTE
            )
        })
        .collect::<BTreeMap<_, _>>()
        .serialize(serializer)
}

fn deserialize_resource_attributes<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    let mut attributes = BTreeMap::<String, String>::deserialize(deserializer)?;
    attributes.remove(COMMAND_OBSERVATION_ATTRIBUTE);
    attributes.remove(COMMAND_OBSERVATION_BINDING_ATTRIBUTE);
    Ok(attributes)
}

fn deserialize_tool_key<'de, D>(deserializer: D) -> Result<ToolKey, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    ToolKey::parse(&value).map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests;
